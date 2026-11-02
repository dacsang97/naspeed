use std::time::{Duration, SystemTime};

use ratatui::{
    Terminal,
    backend::TestBackend,
    buffer::Buffer,
    style::{Color, Modifier},
};

use super::super::log_bus::{DevEvent, LogLine, LogSource, Stream};
use super::app_state::{SOURCE_CAPACITY, ScrollPosition, TuiState};
use super::input::handle_mouse;
use super::render::{self, ScreenMap};
use super::wrap;

fn log(source: LogSource, raw: &str) -> DevEvent {
    DevEvent::Log(LogLine {
        ts: SystemTime::now(),
        source,
        stream: Stream::Stderr,
        raw: raw.to_string(),
    })
}

#[test]
fn scroll_deltas_preserve_position_from_both_edges() {
    let mut state = TuiState::new(&[]);
    state.scroll_home();
    state.scroll_by(3);
    assert_eq!(state.scroll, ScrollPosition::RowsFromTop(3));
    state.scroll_by(-5);
    assert_eq!(state.scroll, ScrollPosition::Top);

    state.scroll_end();
    state.scroll_by(-6);
    assert_eq!(state.scroll, ScrollPosition::RowsFromBottom(6));
    state.scroll_by(2);
    assert_eq!(state.scroll, ScrollPosition::RowsFromBottom(4));
    state.scroll_by(9);
    assert_eq!(state.scroll, ScrollPosition::Bottom);
}

#[test]
fn paused_render_stays_on_existing_rows_after_new_logs_arrive() {
    let mut terminal = Terminal::new(TestBackend::new(40, 9)).unwrap();
    let mut state = TuiState::new(&[]);
    for line in ["one", "two", "three", "four"] {
        state.apply_event(log(LogSource::Build, line));
    }
    let mut screen = ScreenMap::default();
    terminal
        .draw(|frame| render::render(frame, &mut state, &mut screen))
        .unwrap();
    state.autoscroll = false;
    terminal
        .draw(|frame| render::render(frame, &mut state, &mut screen))
        .unwrap();
    state.apply_event(log(LogSource::Build, "five"));
    terminal
        .draw(|frame| render::render(frame, &mut state, &mut screen))
        .unwrap();

    let buffer = terminal.backend().buffer();
    assert!(row_text(buffer, 1).contains("one"));
    assert!(row_text(buffer, 4).contains("four"));
}

#[test]
fn paused_viewport_preserves_anchor_when_source_buffer_evicts() {
    let mut terminal = Terminal::new(TestBackend::new(80, 9)).unwrap();
    let mut state = TuiState::new(&["api".to_string()]);
    let source = LogSource::App("api".to_string());
    let mut screen = ScreenMap::default();
    for index in 0..SOURCE_CAPACITY {
        state.apply_event(log(source.clone(), &format!("line-{index:05}")));
    }
    terminal
        .draw(|frame| render::render(frame, &mut state, &mut screen))
        .unwrap();
    state.scroll_by(-6);
    terminal
        .draw(|frame| render::render(frame, &mut state, &mut screen))
        .unwrap();
    let before = row_text(terminal.backend().buffer(), 1)
        .trim_end()
        .to_string();
    assert!(before.contains("line-09990"), "{before:?}");

    state.apply_event(log(source, &format!("line-{SOURCE_CAPACITY:05}")));
    terminal
        .draw(|frame| render::render(frame, &mut state, &mut screen))
        .unwrap();
    let after = row_text(terminal.backend().buffer(), 1)
        .trim_end()
        .to_string();

    assert_eq!(
        after, before,
        "paused viewport moved when oldest source row was evicted"
    );
}

#[test]
fn paused_all_view_preserves_wrapped_mixed_source_anchor_through_eviction() {
    let mut terminal = Terminal::new(TestBackend::new(80, 9)).unwrap();
    let mut state = TuiState::new(&["api".to_string(), "worker".to_string()]);
    let api = LogSource::App("api".to_string());
    let worker = LogSource::App("worker".to_string());
    for index in 0..SOURCE_CAPACITY {
        let raw = if index == SOURCE_CAPACITY - 10 {
            format!("anchor-{index:05} {}", "x".repeat(130))
        } else {
            format!("api-{index:05}")
        };
        state.apply_event(log(api.clone(), &raw));
        state.apply_event(log(worker.clone(), &format!("worker-{index:05}")));
    }

    let anchor_id = state
        .visible_lines()
        .find(|line| line.plain.starts_with("anchor-09990"))
        .unwrap()
        .id;
    let mut anchor_start = 0usize;
    for line in state.visible_lines() {
        if line.id == anchor_id {
            break;
        }
        let prefix = state.line_prefix(&line.source);
        anchor_start += usize::from(wrap::height(&line.plain, &prefix, 80));
    }
    let anchor = state
        .visible_lines()
        .find(|line| line.id == anchor_id)
        .unwrap();
    let prefix = state.line_prefix(&anchor.source);
    assert_eq!(wrap::height(&anchor.plain, &prefix, 80), 2);
    state.autoscroll = false;
    state.scroll = ScrollPosition::RowsFromTop(anchor_start + 1);

    let mut screen = ScreenMap::default();
    terminal
        .draw(|frame| render::render(frame, &mut state, &mut screen))
        .unwrap();
    let before = row_text(terminal.backend().buffer(), 1)
        .trim_end()
        .to_string();
    assert!(before.starts_with("      "), "{before:?}");

    for index in 0..5 {
        state.apply_event(log(api.clone(), &format!("steady-{index:02}")));
        terminal
            .draw(|frame| render::render(frame, &mut state, &mut screen))
            .unwrap();
        let after = row_text(terminal.backend().buffer(), 1)
            .trim_end()
            .to_string();
        assert_eq!(
            after,
            before,
            "wrapped All-view anchor moved after {} API evictions",
            index + 1
        );
    }
    assert_eq!(state.source_count(&api), SOURCE_CAPACITY);
    assert_eq!(state.source_count(&worker), SOURCE_CAPACITY);
}

#[test]
fn paused_viewport_clamps_to_next_line_when_anchor_is_evicted() {
    let mut terminal = Terminal::new(TestBackend::new(80, 9)).unwrap();
    let mut state = TuiState::new(&["api".to_string()]);
    let source = LogSource::App("api".to_string());
    for index in 0..SOURCE_CAPACITY {
        state.apply_event(log(source.clone(), &format!("line-{index:05}")));
    }
    state.autoscroll = false;
    state.scroll = ScrollPosition::RowsFromTop(0);
    let mut screen = ScreenMap::default();
    terminal
        .draw(|frame| render::render(frame, &mut state, &mut screen))
        .unwrap();
    assert!(row_text(terminal.backend().buffer(), 1).contains("line-00000"));

    state.apply_event(log(source, "line-10000"));
    terminal
        .draw(|frame| render::render(frame, &mut state, &mut screen))
        .unwrap();

    let first = row_text(terminal.backend().buffer(), 1);
    assert!(first.contains("line-00001"), "{first:?}");
    assert!(!first.contains("line-00000"), "{first:?}");
}

#[test]
fn autoscroll_follows_newest_line_after_ring_eviction() {
    let mut terminal = Terminal::new(TestBackend::new(80, 9)).unwrap();
    let mut state = TuiState::new(&["api".to_string()]);
    let source = LogSource::App("api".to_string());
    for index in 0..SOURCE_CAPACITY {
        state.apply_event(log(source.clone(), &format!("line-{index:05}")));
    }
    let mut screen = ScreenMap::default();
    terminal
        .draw(|frame| render::render(frame, &mut state, &mut screen))
        .unwrap();

    state.apply_event(log(source, "line-10000"));
    terminal
        .draw(|frame| render::render(frame, &mut state, &mut screen))
        .unwrap();

    let buffer = terminal.backend().buffer();
    assert!(row_text(buffer, 4).contains("line-10000"));
    assert!(state.autoscroll);
    assert_eq!(state.scroll, ScrollPosition::Bottom);
}

#[test]
fn clamped_scroll_edges_remain_pinned_when_content_grows() {
    let mut top = Terminal::new(TestBackend::new(40, 9)).unwrap();
    let mut top_state = TuiState::new(&[]);
    for line in ["one", "two", "three"] {
        top_state.apply_event(log(LogSource::Build, line));
    }
    top_state.scroll_home();
    top_state.scroll_by(99);
    let mut top_screen = ScreenMap::default();
    top.draw(|frame| render::render(frame, &mut top_state, &mut top_screen))
        .unwrap();
    assert_eq!(top_state.scroll, ScrollPosition::RowsFromTop(0));
    top_state.apply_event(log(LogSource::Build, "four"));
    top_state.apply_event(log(LogSource::Build, "five"));
    top.draw(|frame| render::render(frame, &mut top_state, &mut top_screen))
        .unwrap();
    assert!(row_text(top.backend().buffer(), 1).contains("one"));
    assert!(row_text(top.backend().buffer(), 4).contains("four"));

    let mut bottom = Terminal::new(TestBackend::new(40, 9)).unwrap();
    let mut bottom_state = TuiState::new(&[]);
    for line in [
        "one", "two", "three", "four", "five", "six", "seven", "eight",
    ] {
        bottom_state.apply_event(log(LogSource::Build, line));
    }
    bottom_state.scroll_home();
    bottom_state.scroll_by(99);
    let mut bottom_screen = ScreenMap::default();
    bottom
        .draw(|frame| render::render(frame, &mut bottom_state, &mut bottom_screen))
        .unwrap();
    assert_eq!(bottom_state.scroll, ScrollPosition::RowsFromBottom(0));
    bottom_state.apply_event(log(LogSource::Build, "nine"));
    bottom
        .draw(|frame| render::render(frame, &mut bottom_state, &mut bottom_screen))
        .unwrap();
    assert!(row_text(bottom.backend().buffer(), 1).contains("five"));
    assert!(row_text(bottom.backend().buffer(), 4).contains("eight"));
}

#[test]
fn test_backend_renders_ansi_color_filtered_lines_tabs_and_cycle_status() {
    let backend = TestBackend::new(64, 9);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut state = TuiState::new(&[]);
    state.apply_event(log(LogSource::Build, "\x1b[31mred\x1b[0m needle"));
    state.apply_event(log(LogSource::Build, "haystack"));
    for ch in "needle".chars() {
        state.type_char(ch);
    }
    state.apply_event(DevEvent::Cycle {
        ok: false,
        elapsed: Duration::from_millis(12),
        errors: 2,
    });
    let mut screen = ScreenMap::default();
    terminal
        .draw(|frame| render::render(frame, &mut state, &mut screen))
        .unwrap();

    let buffer = terminal.backend().buffer();
    assert!(row_text(buffer, 0).contains("All"));
    assert!(row_text(buffer, 0).contains("build"));
    assert!(row_text(buffer, 0).contains("build ✗ 2 errors"));
    let red = buffer.cell((8, 1)).unwrap();
    assert_eq!(red.symbol(), "r");
    assert_eq!(red.fg, Color::Red, "ANSI colours survive");
    let hit = buffer.cell((12, 1)).unwrap();
    assert_eq!(hit.symbol(), "n");
    assert_eq!(hit.bg, Color::Yellow, "the filter's hit is highlighted");
    assert!(!(1..5).any(|row| row_text(buffer, row).contains("haystack")));
    assert!(row_text(buffer, 6).contains("› needle"));
}

fn row_text(buffer: &Buffer, row: u16) -> String {
    let mut text = String::new();
    for column in 0..buffer.area.width {
        if let Some(cell) = buffer.cell((column, row)) {
            text.push_str(cell.symbol());
        }
    }
    text
}

#[test]
fn a_drag_selects_on_a_held_frame_and_the_wheel_or_a_tab_click_drops_it() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::layout::Position;

    let mouse = |kind, column, row| MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    };
    let mut terminal = Terminal::new(TestBackend::new(40, 9)).unwrap();
    let mut state = TuiState::new(&["api".to_string()]);
    let api = LogSource::App("api".to_string());
    state.apply_event(log(api.clone(), "listening on :8080"));
    state.apply_event(log(api.clone(), "ready"));
    let mut screen = ScreenMap::default();
    terminal
        .draw(|frame| render::render(frame, &mut state, &mut screen))
        .unwrap();
    // Rows 1..5 hold the logs (two lines here); one app carries no tag.
    assert!(row_text(terminal.backend().buffer(), 1).starts_with("listening"));

    handle_mouse(
        &mut state,
        mouse(MouseEventKind::Down(MouseButton::Left), 13, 1),
        &mut screen,
    );
    handle_mouse(
        &mut state,
        mouse(MouseEventKind::Drag(MouseButton::Left), 2, 2),
        &mut screen,
    );
    state.apply_event(log(api, "arrived meanwhile"));
    terminal
        .draw(|frame| render::render(frame, &mut state, &mut screen))
        .unwrap();
    let buffer = terminal.backend().buffer();
    assert!(
        row_text(buffer, 2).starts_with("ready"),
        "the frame is held"
    );
    assert!(buffer[(13, 1)].modifier.contains(Modifier::REVERSED));
    let selection = screen.selection.as_ref().unwrap();
    assert_eq!(selection.text(), ":8080\nrea");

    handle_mouse(
        &mut state,
        mouse(MouseEventKind::ScrollUp, 5, 4),
        &mut screen,
    );
    assert!(screen.selection.is_none());
    assert!(!state.autoscroll);
    terminal
        .draw(|frame| render::render(frame, &mut state, &mut screen))
        .unwrap();

    screen.start_selection(Position::new(0, 2));
    let build_tab = screen.tab_at(Position::new(9, 0)).unwrap();
    assert_eq!(build_tab, 1);
    handle_mouse(
        &mut state,
        mouse(MouseEventKind::Down(MouseButton::Left), 9, 0),
        &mut screen,
    );
    assert!(screen.selection.is_none());
    assert_eq!(state.active_tab, 1);
}

#[test]
fn the_composer_sits_on_its_surface_with_the_cursor_and_match_count() {
    let mut terminal = Terminal::new(TestBackend::new(60, 9)).unwrap();
    let mut state = TuiState::new(&["api".to_string()]);
    state.surface = Some(Color::Rgb(40, 40, 40));
    let api = LogSource::App("api".to_string());
    state.apply_event(log(api.clone(), "GET /health 200"));
    state.apply_event(log(api, "GET /users 500"));
    state.apply_event(DevEvent::Status {
        bin: "api".to_string(),
        state: super::super::log_bus::AppState::Running { pid: 7 },
    });
    for ch in "500".chars() {
        state.type_char(ch);
    }
    let mut screen = ScreenMap::default();
    terminal
        .draw(|frame| render::render(frame, &mut state, &mut screen))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let header = row_text(buffer, 0);
    assert!(header.starts_with(" All "), "{header:?}");
    assert!(header.trim_end().ends_with("api ● 7"), "{header:?}");
    let input = row_text(buffer, 6);
    assert!(input.starts_with(" › 500"), "{input:?}");
    assert!(input.trim_end().ends_with("1 match"), "{input:?}");
    for row in 5..8 {
        assert_eq!(buffer[(30, row)].bg, Color::Rgb(40, 40, 40), "row {row}");
    }
    assert_eq!(
        buffer[(30, 4)].bg,
        Color::Reset,
        "logs keep the terminal background"
    );
    terminal.backend_mut().assert_cursor_position((6, 6));
}

#[test]
fn untagged_wrapped_rows_hang_and_an_empty_filter_says_so() {
    let mut terminal = Terminal::new(TestBackend::new(20, 9)).unwrap();
    let mut state = TuiState::new(&["api".to_string()]);
    state.apply_event(log(
        LogSource::App("api".to_string()),
        "abcdefghijklmnopqrstuvwxyz",
    ));
    let mut screen = ScreenMap::default();
    terminal
        .draw(|frame| render::render(frame, &mut state, &mut screen))
        .unwrap();
    assert_eq!(
        row_text(terminal.backend().buffer(), 1),
        "abcdefghijklmnopqrst"
    );
    assert_eq!(
        row_text(terminal.backend().buffer(), 2).trim_end(),
        "  uvwxyz"
    );

    for ch in "zzz".chars() {
        state.type_char(ch);
    }
    terminal
        .draw(|frame| render::render(frame, &mut state, &mut screen))
        .unwrap();
    let rows: Vec<String> = (1..5)
        .map(|row| row_text(terminal.backend().buffer(), row))
        .collect();
    assert!(
        rows.iter().any(|row| row.contains("No lines match")),
        "{rows:?}"
    );
}
