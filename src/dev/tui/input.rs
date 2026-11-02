use std::io;
use std::sync::mpsc::Sender;
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use super::super::dev_loop::UiCommand;
use super::super::process_supervisor::request_shutdown;
use super::app_state::TuiState;
use super::clipboard;
use super::render::ScreenMap;

pub(super) fn handle_key(
    state: &mut TuiState,
    key: KeyEvent,
    screen: &ScreenMap,
    commands: &Sender<UiCommand>,
) -> bool {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        request_shutdown();
        let _ = commands.send(UiCommand::Quit);
        return true;
    }

    if state.search_editing {
        match key.code {
            KeyCode::Esc => state.cancel_search(),
            KeyCode::Enter => {
                let _ = state.submit_search();
            }
            KeyCode::Backspace => state.search_backspace(),
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                state.search_char(ch);
            }
            _ => {}
        }
        return false;
    }

    if key.code == KeyCode::Esc {
        if state.selection.active() {
            state.clear_selection();
        } else if state.search_is_active() {
            state.clear_search();
        }
        return false;
    }
    if key.modifiers.contains(KeyModifiers::SHIFT) {
        match key.code {
            KeyCode::Up => {
                state.extend_selection(-1);
                return false;
            }
            KeyCode::Down => {
                state.extend_selection(1);
                return false;
            }
            _ => {}
        }
    }

    match key.code {
        KeyCode::Char('q') => {
            request_shutdown();
            let _ = commands.send(UiCommand::Quit);
            true
        }
        KeyCode::Char('/') => {
            state.begin_search();
            false
        }
        KeyCode::Char('a') => {
            state.toggle_autoscroll();
            false
        }
        KeyCode::Char('C') => {
            state.clear_active();
            false
        }
        KeyCode::Char('r') => {
            let _ = commands.send(UiCommand::Restart(state.restart_bin()));
            false
        }
        KeyCode::Char('f') => {
            state.toggle_filter();
            false
        }
        KeyCode::Char('e') => {
            state.cycle_level_filter();
            false
        }
        KeyCode::Char('n') => {
            state.move_to_match(false);
            false
        }
        KeyCode::Char('N') => {
            state.move_to_match(true);
            false
        }
        KeyCode::Char('v') => {
            if state.selection.active() {
                state.clear_selection();
            } else {
                state.start_selection();
            }
            false
        }
        KeyCode::Char('c') => {
            copy_selection(state);
            false
        }
        KeyCode::Char(ch @ '1'..='9') => {
            state.choose_tab(usize::from(ch as u8 - b'1'));
            false
        }
        KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => {
            state.cycle_tab(-1);
            false
        }
        KeyCode::Tab | KeyCode::Right => {
            state.cycle_tab(1);
            false
        }
        KeyCode::BackTab | KeyCode::Left => {
            state.cycle_tab(-1);
            false
        }
        KeyCode::Up => {
            state.scroll_by(-1);
            false
        }
        KeyCode::Down => {
            state.scroll_by(1);
            false
        }
        KeyCode::PageUp => {
            state.scroll_by(-(screen.log_area.height.max(1) as isize));
            false
        }
        KeyCode::PageDown => {
            state.scroll_by(screen.log_area.height.max(1) as isize);
            false
        }
        KeyCode::Home => {
            state.scroll_home();
            false
        }
        KeyCode::End => {
            state.scroll_end();
            false
        }
        _ => false,
    }
}

pub(super) fn handle_mouse(state: &mut TuiState, mouse: MouseEvent, screen: &ScreenMap) {
    if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
        return;
    }
    if let Some(id) = screen.line_at(mouse.row) {
        state.select_clicked_line(id);
        copy_selection(state);
    }
}

fn copy_selection(state: &mut TuiState) {
    let (lines, payload) = state.copy_payload();
    if lines == 0 {
        state.show_toast("Copy failed: no lines selected".to_string(), Instant::now());
        return;
    }
    let mut stdout = io::stdout();
    let result = clipboard::copy(&payload, std::env::var_os("TMUX").is_some(), &mut stdout);
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
