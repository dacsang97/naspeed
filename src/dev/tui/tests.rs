use std::time::{Duration, SystemTime};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::super::log_bus::{AppState, DevEvent, LogLine, LogSource, Stream};
use super::app_state::{SOURCE_CAPACITY, TuiState};
use super::input::handle_key;
use super::render::ScreenMap;
use super::search::{LevelFilter, Query};

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
    let pinned_cursor = state.cursor;
    let pinned_scroll = state.scroll;
    state.apply_event(log(source, "second"));
    assert!(!state.autoscroll);
    assert_eq!(state.cursor, pinned_cursor);
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
fn clearing_a_source_resets_its_continuation_group() {
    let mut state = TuiState::new(&["api".to_string()]);
    state.apply_event(log(LogSource::App("api".to_string()), "root"));
    state.choose_tab(1);
    state.clear_active();
    state.apply_event(log(LogSource::App("api".to_string()), "    orphan"));
    let line = state.visible_lines().next().unwrap();
    assert_eq!(line.group, line.id);
}

#[test]
fn invalid_regex_keeps_the_previous_query() {
    let mut state = TuiState::new(&[]);
    state.query_text = "old".to_string();
    state.query = Some(Query::parse("old").unwrap());
    state.begin_search();
    state.search_draft = "re:[".to_string();
    assert!(state.submit_search().is_err());
    assert_eq!(state.query_text, "old");
    assert!(state.query.as_ref().unwrap().matches("old value"));
    assert!(state.search_display().1.is_some());
    state.cancel_search();
    assert_eq!(state.search_display().0, "old");
    assert!(state.search_display().1.is_none());
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
fn next_and_previous_match_navigation_wraps() {
    let mut state = TuiState::new(&[]);
    state.apply_event(log(LogSource::Build, "needle one"));
    state.apply_event(log(LogSource::Build, "other"));
    state.apply_event(log(LogSource::Build, "needle two"));
    state.query = Some(Query::parse("needle").unwrap());
    let first = state.visible_lines().next().unwrap().id;
    let last = state.visible_lines().last().unwrap().id;

    state.move_to_match(false);
    assert_eq!(state.cursor, Some(first));
    state.move_to_match(true);
    assert_eq!(state.cursor, Some(last));
}

#[test]
fn filter_mode_hides_nonmatches_and_level_filter_cycles() {
    let mut state = TuiState::new(&[]);
    state.apply_event(log(LogSource::Build, "needle found"));
    state.apply_event(log(LogSource::Build, "WARN: other"));
    state.apply_event(log(LogSource::Build, "ERROR: needle"));
    state.query = Some(Query::parse("needle").unwrap());
    assert_eq!(state.visible_lines().count(), 3);
    state.toggle_filter();
    assert_eq!(state.visible_lines().count(), 2);
    state.cycle_level_filter();
    assert_eq!(state.level_filter(), LevelFilter::WarnUp);
    assert_eq!(state.visible_lines().count(), 1);
    state.cycle_level_filter();
    assert_eq!(state.visible_lines().count(), 1);
}

#[test]
fn selection_copies_source_aware_continuations_without_ansi() {
    let mut state = TuiState::new(&["api".to_string(), "worker".to_string()]);
    let api = LogSource::App("api".to_string());
    state.apply_event(log(api.clone(), "thread panicked"));
    state.apply_event(log(api.clone(), "   at api::run"));
    state.apply_event(log(
        LogSource::App("worker".to_string()),
        "   at worker::run",
    ));
    state.apply_event(log(api.clone(), "\x1b[31mready\x1b[0m"));
    let selected = state.visible_lines().next().unwrap().id;
    state.select_clicked_line(selected);
    let (count, payload) = state.copy_payload();
    assert_eq!(count, 2);
    assert_eq!(payload, "[api] thread panicked\n[api]    at api::run");
    assert!(!payload.contains('\x1b'));
}
#[test]
fn extended_selection_includes_intervening_sources_and_full_groups() {
    let mut state = TuiState::new(&["api".to_string(), "worker".to_string()]);
    state.apply_event(log(LogSource::App("api".to_string()), "root A"));
    state.apply_event(log(LogSource::App("api".to_string()), "    child A"));
    state.apply_event(log(LogSource::App("worker".to_string()), "worker root"));
    state.apply_event(log(LogSource::App("api".to_string()), "root B"));
    state.apply_event(log(LogSource::App("api".to_string()), "    child B"));
    let first = state.visible_lines().next().unwrap().id;
    state.select_clicked_line(first);
    state.extend_selection(3);

    let (count, payload) = state.copy_payload();
    assert_eq!(count, 5);
    assert_eq!(
        payload,
        "[api] root A\n[api]     child A\n[worker] worker root\n[api] root B\n[api]     child B"
    );
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
fn ctrl_c_quits_from_normal_and_search_editing_modes() {
    let _shutdown_test_lock = super::SHUTDOWN_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let shutdown_before = super::super::process_supervisor::shutdown_requested();
    super::super::process_supervisor::set_shutdown_for_test(false);
    let _shutdown_restore = ShutdownRestore(shutdown_before);

    let mut normal = TuiState::new(&[]);
    assert_ctrl_c_quits(&mut normal);
    super::super::process_supervisor::set_shutdown_for_test(false);

    let mut search = TuiState::new(&[]);
    let (commands, quit) = std::sync::mpsc::channel();
    let screen = ScreenMap::default();
    assert!(!handle_key(
        &mut search,
        KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
        &screen,
        &commands,
    ));
    assert!(!handle_key(
        &mut search,
        KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
        &screen,
        &commands,
    ));
    assert_eq!(search.search_draft, "q");
    assert!(quit.try_recv().is_err());
    assert_ctrl_c_quits(&mut search);
    assert!(search.search_editing);
    assert_eq!(search.search_draft, "q");
    super::super::process_supervisor::set_shutdown_for_test(false);

    let mut invalid_regex = TuiState::new(&[]);
    invalid_regex.query_text = "old".to_string();
    invalid_regex.query = Some(Query::parse("old").unwrap());
    invalid_regex.begin_search();
    let (commands, quit) = std::sync::mpsc::channel();
    for _ in 0..3 {
        assert!(!handle_key(
            &mut invalid_regex,
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
            &screen,
            &commands,
        ));
    }
    for ch in ['r', 'e', ':', '['] {
        assert!(!handle_key(
            &mut invalid_regex,
            KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
            &screen,
            &commands,
        ));
    }
    assert!(!handle_key(
        &mut invalid_regex,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        &screen,
        &commands,
    ));
    let draft = invalid_regex.search_draft.clone();
    let error = invalid_regex.search_error.clone().unwrap();
    assert_eq!(draft, "re:[");
    assert_eq!(invalid_regex.query_text, "old");
    assert!(invalid_regex.query.as_ref().unwrap().matches("old"));
    assert!(invalid_regex.search_editing);

    assert_ctrl_c_quits(&mut invalid_regex);
    assert_eq!(invalid_regex.search_draft, draft);
    assert_eq!(invalid_regex.search_error.as_ref(), Some(&error));
    assert_eq!(invalid_regex.query_text, "old");
    assert!(invalid_regex.query.as_ref().unwrap().matches("old"));
    assert!(invalid_regex.search_editing);
    assert!(quit.try_recv().is_err());
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
        &ScreenMap::default(),
        &commands,
    ));
    assert!(matches!(
        quit.try_recv(),
        Ok(super::super::dev_loop::UiCommand::Quit)
    ));
    assert!(super::super::process_supervisor::shutdown_requested());
}
