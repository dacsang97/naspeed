mod app_state;
mod clipboard;
mod input;
mod navigation;
mod render;
mod render_status;
mod search;
mod search_state;
mod selection;
mod selection_state;
mod terminal_guard;
pub(super) mod tui_sink;
mod view;
mod wrap;

#[cfg(test)]
mod render_tests;
#[cfg(test)]
mod tests;

use std::io;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyEventKind};
use ratatui::{Terminal, backend::CrosstermBackend};

use self::app_state::TuiState;
use self::input::{handle_key, handle_mouse};
use self::render::{ScreenMap, render};
use self::terminal_guard::TerminalGuard;
use super::dev_loop::UiCommand;
use super::log_bus::DevEvent;
use super::process_supervisor::{request_shutdown, shutdown_requested};

const EVENT_BATCH: usize = 2048;
const EVENT_POLL: Duration = Duration::from_millis(50);

#[cfg(test)]
static SHUTDOWN_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub(super) fn run_tui(
    events: Receiver<DevEvent>,
    bins: &[String],
    commands: Sender<UiCommand>,
) -> Result<()> {
    let guard = match TerminalGuard::enter() {
        Ok(guard) => guard,
        Err(error) => {
            stop_loop(&commands);
            return Err(error.into());
        }
    };
    let result = run_event_loop(events, bins, &commands);
    drop(guard);
    if result.is_err() {
        stop_loop(&commands);
    }
    result
}

fn run_event_loop(
    events: Receiver<DevEvent>,
    bins: &[String],
    commands: &Sender<UiCommand>,
) -> Result<()> {
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    let mut state = TuiState::new(bins);
    let mut screen = ScreenMap::default();
    let mut dirty = true;

    loop {
        if shutdown_requested() {
            break;
        }
        let mut disconnected = false;
        for _ in 0..EVENT_BATCH {
            match events.try_recv() {
                Ok(event) => {
                    state.apply_event(event);
                    dirty = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        if disconnected {
            stop_loop(commands);
            break;
        }
        if state.shutdown || shutdown_requested() {
            break;
        }
        if state.expire_toast(std::time::Instant::now()) {
            dirty = true;
        }
        if dirty {
            terminal.draw(|frame| render(frame, &mut state, &mut screen))?;
            dirty = false;
        }
        if event::poll(EVENT_POLL)? {
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    dirty = true;
                    if handle_key(&mut state, key, &screen, commands) {
                        break;
                    }
                }
                Event::Mouse(mouse) => {
                    handle_mouse(&mut state, mouse, &screen);
                    dirty = true;
                }
                Event::Resize(_, _) => dirty = true,
                _ => {}
            }
        }
    }
    Ok(())
}

fn stop_loop(commands: &Sender<UiCommand>) {
    request_shutdown();
    let _ = commands.send(UiCommand::Quit);
}
