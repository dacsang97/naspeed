//! Unix process groups: `process_group(0)` at spawn, SIGTERM to the group,
//! grace period, SIGKILL; SIGINT/SIGTERM/SIGHUP set the shutdown flag.

use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use super::{POLL, SHUTDOWN, STOP_GRACE};

extern "C" fn on_shutdown_signal(_sig: libc::c_int) {
    // Only an atomic store: async-signal-safe.
    SHUTDOWN.store(true, Ordering::SeqCst);
}

/// Routes SIGINT, SIGTERM and SIGHUP to a flag the dev loop polls, so the
/// loop (not the default handler) ends the process after stopping children.
pub fn install_shutdown_handler() -> Result<()> {
    for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        // SAFETY: the handler only performs an atomic store.
        let prev =
            unsafe { libc::signal(sig, on_shutdown_signal as *const () as libc::sighandler_t) };
        if prev == libc::SIG_ERR {
            return Err(std::io::Error::last_os_error()).context("install signal handler");
        }
    }
    Ok(())
}

/// A child that leads its own process group. Dropping it stops the group, so
/// an early return or a panic in `ns dev` never leaves the app running.
pub struct GroupChild {
    child: Child,
    pgid: libc::pid_t,
    status: Option<ExitStatus>,
    /// The whole group is known to be gone; never signal the pgid again
    /// (it could be reused by an unrelated group).
    group_gone: bool,
}

impl GroupChild {
    pub fn spawn(mut cmd: Command) -> Result<GroupChild> {
        cmd.process_group(0);
        let child = cmd
            .spawn()
            .with_context(|| format!("spawn {:?}", cmd.get_program()))?;
        let pgid = child.id() as libc::pid_t;
        Ok(GroupChild {
            child,
            pgid,
            status: None,
            group_gone: false,
        })
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    pub fn child_mut(&mut self) -> &mut Child {
        &mut self.child
    }

    /// Exit status of the group leader once it has exited (reaps it).
    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
        if self.status.is_none() {
            self.status = self.child.try_wait().context("wait for child")?;
        }
        Ok(self.status)
    }

    fn signal_group(&mut self, sig: libc::c_int) {
        if self.group_gone {
            return;
        }
        // SAFETY: plain kill(2) on our own child's process group.
        if unsafe { libc::kill(-self.pgid, sig) } != 0
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        {
            self.group_gone = true;
        }
    }

    /// True while any process of the group (including an unreaped leader) exists.
    fn group_alive(&mut self) -> bool {
        self.signal_group(0);
        !self.group_gone
    }

    /// SIGTERM to the whole group, wait up to `grace` for every member to
    /// exit, then SIGKILL the rest. Returns the leader's exit status.
    pub fn stop(&mut self, grace: Duration) -> Option<ExitStatus> {
        self.signal_group(libc::SIGTERM);
        let deadline = Instant::now() + grace;
        loop {
            let _ = self.try_wait();
            if !self.group_alive() {
                break;
            }
            if Instant::now() >= deadline {
                self.signal_group(libc::SIGKILL);
                break;
            }
            std::thread::sleep(POLL);
        }
        if self.status.is_none() {
            self.status = self.child.wait().ok();
        }
        // Killed members are reaped by init; wait briefly so ports are released.
        let deadline = Instant::now() + Duration::from_secs(1);
        while self.group_alive() && Instant::now() < deadline {
            std::thread::sleep(POLL);
        }
        self.status
    }
}

impl Drop for GroupChild {
    fn drop(&mut self) {
        if !self.group_gone {
            self.stop(STOP_GRACE);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_kills_the_whole_group() {
        // Spawning at the same instant as the flock tests in this binary
        // (which relock right after releasing) makes them flaky on macOS;
        // start after they are done instead of racing them at t=0.
        std::thread::sleep(Duration::from_millis(300));
        // The shell ignores SIGTERM and has a grandchild: both must still go.
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "trap '' TERM; sleep 30 & wait"]);
        let mut child = GroupChild::spawn(cmd).unwrap();
        let pgid = child.pid() as libc::pid_t;
        std::thread::sleep(Duration::from_millis(200));
        let started = Instant::now();
        child.stop(Duration::from_millis(300));
        assert!(started.elapsed() < Duration::from_secs(3));
        // SAFETY: probing a process group we created.
        let alive = unsafe { libc::kill(-pgid, 0) } == 0;
        assert!(!alive, "process group {pgid} survived stop");
    }
}
