use std::time::{Duration, SystemTime};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::super::log_bus::{AppState, DevEvent, LogLine, LogSource, Stream};
use super::app_state::{SOURCE_CAPACITY, TuiState};
use super::input::handle_key;
use super::render::ScreenMap;
use super::search::{LevelFilter, Query};

fn visible(state: &TuiState) -> Vec<&str> {
    state
        .visible_lines()
        .map(|line| line.plain.as_str())
        .collect()
}

fn log(source: LogSource, raw: &str) -> DevEvent {
    DevEvent::Log(LogLine {
        ts: SystemTime::now(),
        source,
        stream: Stream::Stderr,
        raw: raw.to_string(),
    })
}

#[test]
fn each_source_is_bounded_and_all_merges_sources_in_order() {
    let mut state = TuiState::new(&["api".to_string()]);
    let source = LogSource::App("api".to_string());
    for index in 0..=SOURCE_CAPACITY {
        state.apply_event(log(source.clone(), &index.to_string()));
    }
    state.apply_event(log(LogSource::Build, "build complete"));
    assert_eq!(state.source_count(&source), SOURCE_CAPACITY);
    assert_eq!(state.merged.len(), SOURCE_CAPACITY + 1);
    assert_eq!(state.visible_lines().next().unwrap().plain, "1");
    assert_eq!(
        state.visible_lines().last().unwrap().plain,
        "build complete"
    );
}

#[test]
fn paused_scroll_stays_pinned_when_new_lines_arrive() {
    let mut state = TuiState::new(&["api".to_string()]);
    let source = LogSource::App("api".to_string());
    state.apply_event(log(source.clone(), "first"));
    state.scroll_by(-2);
    let pinned_scroll = state.scroll;
    state.apply_event(log(source, "second"));
    assert!(!state.autoscroll);
    assert_eq!(state.scroll, pinned_scroll);
}

#[test]
fn tabs_filter_sources_and_clear_only_the_active_view() {
    let mut state = TuiState::new(&["api".to_string()]);
    state.apply_event(log(LogSource::App("api".to_string()), "api line"));
    state.apply_event(log(LogSource::Build, "build line"));
    state.choose_tab(1);
    assert_eq!(
        state
            .visible_lines()
            .map(|line| line.plain.as_str())
            .collect::<Vec<_>>(),
        ["api line"]
    );
    state.clear_active();
    state.choose_tab(0);
    assert_eq!(
        state
            .visible_lines()
            .map(|line| line.plain.as_str())
            .collect::<Vec<_>>(),
        ["build line"]
    );
}
#[test]
fn an_incomplete_regex_keeps_the_last_valid_filter_until_it_parses() {
    let mut state = TuiState::new(&[]);
    state.apply_event(log(LogSource::Build, "error one"));
    state.apply_event(log(LogSource::Build, "other"));
    for ch in "re:err(".chars() {
        state.type_char(ch);
    }
    assert!(state.search_error.is_some());
    assert_eq!(state.query_text, "re:err(");
    assert_eq!(visible(&state), ["error one"]);
    state.type_char(')');
    assert!(state.search_error.is_none());
    assert_eq!(visible(&state), ["error one"]);
    state.clear_query();
    assert_eq!(visible(&state), ["error one", "other"]);
}

#[test]
fn query_uses_smart_case_literals_and_explicit_regex() {
    assert!(Query::parse("error").unwrap().matches("ERROR: failed"));
    let smart = Query::parse("Error").unwrap();
    assert!(smart.matches("Error"));
    assert!(!smart.matches("ERROR"));

    let literal = Query::parse("a.b").unwrap();
    assert!(literal.matches("a.b"));
    assert!(!literal.matches("axb"));
    let regex = Query::parse("re:err(or|and)").unwrap();
    assert!(regex.matches("ERROR"));
    assert!(regex.matches("errand"));
    assert!(!regex.matches("erxxor"));
}
#[test]
fn typing_filters_at_once_and_the_level_filter_cycles() {
    let mut state = TuiState::new(&[]);
    state.apply_event(log(LogSource::Build, "needle found"));
    state.apply_event(log(LogSource::Build, "WARN: other"));
    state.apply_event(log(LogSource::Build, "ERROR: needle"));
    state.scroll_by(-1);
    for ch in "needle".chars() {
        state.type_char(ch);
    }
    assert_eq!(visible(&state), ["needle found", "ERROR: needle"]);
    assert!(state.autoscroll, "a new filter follows the newest match");
    state.backspace();
    assert_eq!(state.query_text, "needl");
    state.cycle_level_filter();
    assert_eq!(state.level_filter(), LevelFilter::WarnUp);
    assert_eq!(visible(&state), ["ERROR: needle"]);
}

#[test]
fn a_single_app_drops_its_tag_but_keeps_the_other_sources_tags() {
    let single = TuiState::new(&["api".to_string()]);
    let api = LogSource::App("api".to_string());
    assert_eq!(single.line_prefix(&api), "");
    assert_eq!(single.line_prefix(&LogSource::Build), "[build] ");
    assert_eq!(
        single.line_prefix(&LogSource::Ns),
        "",
        "status lines carry their own [ns]"
    );
    let several = TuiState::new(&["api".to_string(), "worker".to_string()]);
    assert_eq!(several.line_prefix(&api), "[api] ");
}

#[test]
fn scrolling_back_to_the_bottom_resumes_following() {
    let mut state = TuiState::new(&[]);
    state.scroll_by(-3);
    assert!(!state.autoscroll);
    state.scroll_by(3);
    assert!(state.autoscroll);
}

#[test]
fn toast_expires_after_two_seconds() {
    let mut state = TuiState::new(&[]);
    let now = std::time::Instant::now();
    state.show_toast("Copied 1 lines (OSC 52 only)".to_string(), now);
    assert!(!state.expire_toast(now + Duration::from_millis(1999)));
    assert_eq!(state.toast(), Some("Copied 1 lines (OSC 52 only)"));
    assert!(state.expire_toast(now + Duration::from_secs(2)));
    assert_eq!(state.toast(), None);
}

#[test]
fn status_events_update_bin_state() {
    let mut state = TuiState::new(&["api".to_string()]);
    state.apply_event(DevEvent::Status {
        bin: "api".to_string(),
        state: AppState::Running { pid: 42 },
    });
    assert!(matches!(
        state.statuses.get("api"),
        Some(AppState::Running { pid: 42 })
    ));
}

#[test]
fn letters_go_to_the_composer_and_control_keys_run_commands() {
    let _shutdown_test_lock = super::SHUTDOWN_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let shutdown_before = super::super::process_supervisor::shutdown_requested();
    super::super::process_supervisor::set_shutdown_for_test(false);
    let _shutdown_restore = ShutdownRestore(shutdown_before);

    let mut state = TuiState::new(&["api".to_string()]);
    let (commands, received) = std::sync::mpsc::channel();
    let mut screen = ScreenMap::default();
    let mut press = |state: &mut TuiState, code, modifiers| {
        handle_key(
            state,
            KeyEvent::new(code, modifiers),
            &mut screen,
            &commands,
        )
    };
    for ch in ['q', 'r', 'C'] {
        assert!(!press(&mut state, KeyCode::Char(ch), KeyModifiers::NONE));
    }
    assert_eq!(state.query_text, "qrC");
    assert!(received.try_recv().is_err());
    assert!(!press(&mut state, KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(state.query_text, "");
    assert!(!press(
        &mut state,
        KeyCode::Char('e'),
        KeyModifiers::CONTROL
    ));
    assert_eq!(state.level_filter(), LevelFilter::WarnUp);
    assert!(!press(&mut state, KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(state.active_tab, 1);
    assert!(!press(
        &mut state,
        KeyCode::Char('r'),
        KeyModifiers::CONTROL
    ));
    assert!(matches!(
        received.try_recv(),
        Ok(super::super::dev_loop::UiCommand::Restart(Some(bin))) if bin == "api"
    ));
    state.type_char('x');
    assert_ctrl_c_quits(&mut state);
    assert_eq!(state.query_text, "x");
}

struct ShutdownRestore(bool);
impl Drop for ShutdownRestore {
    fn drop(&mut self) {
        super::super::process_supervisor::set_shutdown_for_test(self.0);
    }
}

fn assert_ctrl_c_quits(state: &mut TuiState) {
    let (commands, quit) = std::sync::mpsc::channel();
    assert!(handle_key(
        state,
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL,),
        &mut ScreenMap::default(),
        &commands,
    ));
    assert!(matches!(
        quit.try_recv(),
        Ok(super::super::dev_loop::UiCommand::Quit)
    ));
    assert!(super::super::process_supervisor::shutdown_requested());
}

#[test]
fn question_mark_toggles_shortcuts_on_an_empty_filter_and_types_inside_one() {
    let mut state = TuiState::new(&[]);
    let (commands, _received) = std::sync::mpsc::channel();
    let mut screen = ScreenMap::default();
    let mut press = |state: &mut TuiState, code| {
        handle_key(
            state,
            KeyEvent::new(code, KeyModifiers::NONE),
            &mut screen,
            &commands,
        )
    };
    press(&mut state, KeyCode::Char('?'));
    assert!(state.show_shortcuts);
    press(&mut state, KeyCode::Char('?'));
    assert!(!state.show_shortcuts);
    press(&mut state, KeyCode::Char('?'));
    press(&mut state, KeyCode::Esc);
    assert!(!state.show_shortcuts, "Esc closes the list");
    press(&mut state, KeyCode::Char('a'));
    press(&mut state, KeyCode::Char('?'));
    assert_eq!(state.query_text, "a?");
    assert!(!state.show_shortcuts);
}
