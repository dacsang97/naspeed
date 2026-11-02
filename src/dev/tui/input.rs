use std::io;
use std::sync::mpsc::Sender;
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Position;

use super::super::dev_loop::UiCommand;
use super::super::process_supervisor::request_shutdown;
use super::app_state::TuiState;
use super::clipboard;
use super::render::ScreenMap;

/// Rows one wheel notch scrolls.
const WHEEL_ROWS: isize = 3;

/// Keys go to the composer: printable characters filter as they are typed,
/// so commands sit on control keys. Returns true to quit.
pub(super) fn handle_key(
    state: &mut TuiState,
    key: KeyEvent,
    screen: &mut ScreenMap,
    commands: &Sender<UiCommand>,
) -> bool {
    let had_selection = screen.selection.take().is_some();
    let had_shortcuts = std::mem::take(&mut state.show_shortcuts);
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    if control {
        match key.code {
            KeyCode::Char('c' | 'd') => {
                request_shutdown();
                let _ = commands.send(UiCommand::Quit);
                return true;
            }
            KeyCode::Char('r') => {
                let _ = commands.send(UiCommand::Restart(state.restart_bin()));
            }
            KeyCode::Char('l') => state.clear_active(),
            KeyCode::Char('e') => state.cycle_level_filter(),
            KeyCode::Char('u') => state.clear_query(),
            _ => {}
        }
        return false;
    }
    match key.code {
        KeyCode::Esc if !had_selection && !had_shortcuts => state.clear_query(),
        // As in Codex: `?` on an empty filter toggles the shortcut list;
        // inside a filter it is just a character.
        KeyCode::Char('?') if state.query_text.is_empty() => state.show_shortcuts = !had_shortcuts,
        KeyCode::Backspace => state.backspace(),
        KeyCode::Enter | KeyCode::End => state.scroll_end(),
        KeyCode::Home => state.scroll_home(),
        KeyCode::Tab => state.cycle_tab(1),
        KeyCode::BackTab => state.cycle_tab(-1),
        KeyCode::Up => state.scroll_by(-1),
        KeyCode::Down => state.scroll_by(1),
        KeyCode::PageUp => state.scroll_by(-page(screen)),
        KeyCode::PageDown => state.scroll_by(page(screen)),
        KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::ALT) => state.type_char(ch),
        _ => {}
    }
    false
}

/// The wheel scrolls the logs; a press and drag on them selects text and
/// the release copies it; a click on a tab opens it.
pub(super) fn handle_mouse(state: &mut TuiState, mouse: MouseEvent, screen: &mut ScreenMap) {
    let at = Position::new(mouse.column, mouse.row);
    match mouse.kind {
        MouseEventKind::ScrollUp => {
            screen.selection = None;
            state.scroll_by(-WHEEL_ROWS);
        }
        MouseEventKind::ScrollDown => {
            screen.selection = None;
            state.scroll_by(WHEEL_ROWS);
        }
        MouseEventKind::Down(MouseButton::Left) => {
            screen.selection = None;
            if let Some(tab) = screen.tab_at(at) {
                state.choose_tab(tab);
            } else if screen.in_log_area(at) {
                screen.start_selection(at);
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if let Some(selection) = screen.selection.as_mut().filter(|s| s.dragging()) {
                selection.drag_to(at);
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            let Some(selection) = screen.selection.as_mut().filter(|s| s.dragging()) else {
                return;
            };
            selection.drag_to(at);
            match selection.release() {
                Some(text) => copy(state, &text),
                None => screen.selection = None,
            }
        }
        _ => {}
    }
}

fn page(screen: &ScreenMap) -> isize {
    screen.log_area.height.max(1) as isize
}

fn copy(state: &mut TuiState, text: &str) {
    let lines = text.lines().count().max(1);
    let mut stdout = io::stdout();
    let result = clipboard::copy(text, std::env::var_os("TMUX").is_some(), &mut stdout);
    let message = match (result.osc52, result.native) {
        (true, Some(native)) => format!("Copied {lines} lines (OSC 52 + {native})"),
        (true, None) => format!("Copied {lines} lines (OSC 52 only)"),
        (false, Some(native)) => format!("Copied {lines} lines ({native})"),
        (false, None) => format!(
            "Copy failed: {}",
            result
                .error
                .unwrap_or_else(|| "clipboard unavailable".into())
        ),
    };
    state.show_toast(message, Instant::now());
}
