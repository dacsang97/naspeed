use std::io::{self, Write};
use std::panic::{self, PanicHookInfo};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use crossterm::{
    event::EnableMouseCapture,
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};

static ACTIVE: AtomicBool = AtomicBool::new(false);
type Hook = dyn Fn(&PanicHookInfo<'_>) + Send + Sync + 'static;

pub(super) struct TerminalGuard {
    previous_hook: Option<Arc<Hook>>,
}

impl TerminalGuard {
    pub(super) fn enter() -> io::Result<Self> {
        let previous_hook = install_panic_hook();
        if let Err(error) = terminal::enable_raw_mode() {
            restore_panic_hook(previous_hook);
            return Err(error);
        }
        ACTIVE.store(true, Ordering::SeqCst);
        let result = execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableMouseCapture,
            crossterm::cursor::Hide
        );
        if let Err(error) = result {
            restore_terminal();
            restore_panic_hook(previous_hook);
            return Err(error);
        }
        Ok(Self {
            previous_hook: Some(previous_hook),
        })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
        if !std::thread::panicking()
            && let Some(previous_hook) = self.previous_hook.take()
        {
            restore_panic_hook(previous_hook);
        }
    }
}

fn install_panic_hook() -> Arc<Hook> {
    let previous: Arc<Hook> = Arc::from(panic::take_hook());
    let hook = Arc::clone(&previous);
    panic::set_hook(Box::new(move |info| {
        super::super::process_supervisor::request_shutdown();
        restore_terminal();
        hook(info);
    }));
    previous
}

fn restore_panic_hook(previous: Arc<Hook>) {
    panic::set_hook(Box::new(move |info| previous(info)));
}

fn restore_terminal() {
    if !ACTIVE.swap(false, Ordering::SeqCst) {
        return;
    }
    let _ = terminal::disable_raw_mode();
    let mut stdout = io::stdout();
    let _ = execute!(
        stdout,
        crossterm::event::DisableMouseCapture,
        LeaveAlternateScreen,
        crossterm::cursor::Show
    );
    let _ = stdout.flush();
}

#[cfg(test)]
mod tests {
    use std::panic::{self, catch_unwind};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use super::{install_panic_hook, restore_panic_hook};

    struct ShutdownRestore(bool);
    impl Drop for ShutdownRestore {
        fn drop(&mut self) {
            super::super::super::process_supervisor::set_shutdown_for_test(self.0);
        }
    }

    #[test]
    fn panic_hook_requests_shutdown_and_chains_to_previous_hook() {
        let _shutdown_test_lock = super::super::SHUTDOWN_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let shutdown_before = super::super::super::process_supervisor::shutdown_requested();
        super::super::super::process_supervisor::set_shutdown_for_test(false);
        let shutdown_restore = ShutdownRestore(shutdown_before);
        let original = panic::take_hook();
        let previous_called = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&previous_called);
        panic::set_hook(Box::new(move |_| {
            observed.store(true, Ordering::SeqCst);
        }));
        let previous = install_panic_hook();
        let result = catch_unwind(|| panic!("exercise the TUI panic hook"));
        restore_panic_hook(previous);
        drop(panic::take_hook());
        panic::set_hook(original);

        let shutdown_requested = super::super::super::process_supervisor::shutdown_requested();
        drop(shutdown_restore);
        assert!(result.is_err());
        assert!(previous_called.load(Ordering::SeqCst));
        assert!(shutdown_requested);
    }
}
