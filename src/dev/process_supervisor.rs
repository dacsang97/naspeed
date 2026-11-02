//! Child processes in their own process group, stopped as a whole group
//! (SIGTERM, grace period, SIGKILL; on Windows Ctrl-Break, grace period,
//! job termination), plus port-release waiting and the shutdown flag
//! (SIGINT/SIGTERM/SIGHUP, or a console control event) that makes `ns dev`
//! clean up before exiting.
//!
//! Every child (the app, cargo, the test run) gets its own process group: a
//! terminal Ctrl-C then reaches only `ns dev`, which decides what to stop,
//! and grandchildren (a server's workers, test binaries) die with the group.

use std::io::ErrorKind;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[cfg(unix)]
#[path = "process_group_unix.rs"]
mod group;
#[cfg(windows)]
#[path = "process_group_windows.rs"]
mod group;

pub use group::{GroupChild, install_shutdown_handler};

/// Grace period between the polite stop (SIGTERM, Ctrl-Break) and the kill.
pub const STOP_GRACE: Duration = Duration::from_secs(5);
/// How long to wait for `[dev].ports` to become bindable after a stop.
pub const PORT_WAIT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(20);

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

pub fn shutdown_requested() -> bool {
    SHUTDOWN.load(Ordering::SeqCst)
}

/// Requests the same orderly shutdown used by the installed signal handler.
pub fn request_shutdown() {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

#[cfg(test)]
pub(super) fn set_shutdown_for_test(requested: bool) {
    SHUTDOWN.store(requested, Ordering::SeqCst);
}

/// A port is free when nothing accepts connections on it and it can be bound
/// on all interfaces (std sets `SO_REUSEADDR`, like the apps we restart, so
/// TIME_WAIT leftovers do not count as busy).
pub fn port_is_free(port: u16) -> bool {
    let local = SocketAddr::from(([127, 0, 0, 1], port));
    if TcpStream::connect_timeout(&local, Duration::from_millis(200)).is_ok() {
        return false;
    }
    match TcpListener::bind(("0.0.0.0", port)) {
        Ok(_) => true,
        Err(err) => err.kind() != ErrorKind::AddrInUse,
    }
}

/// Waits until every port is free; returns the ports still busy at `timeout`.
pub fn wait_ports_free(ports: &[u16], timeout: Duration) -> Vec<u16> {
    let deadline = Instant::now() + timeout;
    loop {
        let busy: Vec<u16> = ports
            .iter()
            .copied()
            .filter(|p| !port_is_free(*p))
            .collect();
        if busy.is_empty() || Instant::now() >= deadline {
            return busy;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// `lsof` lines describing who listens on `port` (empty if unknown).
#[cfg(unix)]
pub fn port_owner(port: u16) -> String {
    Command::new("lsof")
        .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// `netstat` lines (with the owning PID) for listeners on `port`.
#[cfg(windows)]
pub fn port_owner(port: u16) -> String {
    let suffix = format!(":{port}");
    Command::new("netstat")
        .args(["-ano", "-p", "TCP"])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter(|l| {
                    let cols: Vec<&str> = l.split_whitespace().collect();
                    cols.get(1).is_some_and(|a| a.ends_with(&suffix))
                        && cols.get(3) == Some(&"LISTENING")
                })
                .map(str::trim)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bound_port_is_busy_until_released() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(!port_is_free(port));
        assert_eq!(
            wait_ports_free(&[port], Duration::from_millis(100)),
            vec![port]
        );
        drop(listener);
        assert!(wait_ports_free(&[port], Duration::from_secs(2)).is_empty());
    }
}
