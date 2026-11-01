//! Windows process groups: each child starts in a new console process group
//! (so a terminal Ctrl-C reaches only `ns dev`) and inside a job object that
//! catches every descendant. Stopping sends Ctrl-Break to the group, waits
//! the grace period, then terminates the job. Console control events
//! (Ctrl-C, Ctrl-Break, closing the window) set the shutdown flag.

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::process::{Child, Command, ExitStatus};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use windows_sys::Win32::Foundation::TRUE;
use windows_sys::Win32::System::Console::{
    CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent, SetConsoleCtrlHandler,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
    QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP;
use windows_sys::core::BOOL;

use super::{POLL, SHUTDOWN, STOP_GRACE};

unsafe extern "system" fn on_console_event(_ctrl_type: u32) -> BOOL {
    SHUTDOWN.store(true, Ordering::SeqCst);
    TRUE
}

/// Routes console control events to a flag the dev loop polls, so the loop
/// (not the default handler) ends the process after stopping children.
pub fn install_shutdown_handler() -> Result<()> {
    // SAFETY: the handler only performs an atomic store.
    if unsafe { SetConsoleCtrlHandler(Some(on_console_event), TRUE) } == 0 {
        return Err(std::io::Error::last_os_error()).context("install console control handler");
    }
    Ok(())
}

/// A job that kills its processes when the last handle closes.
fn new_job() -> Result<OwnedHandle> {
    // SAFETY: anonymous job with default security.
    let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if raw.is_null() {
        return Err(std::io::Error::last_os_error()).context("create job object");
    }
    // SAFETY: `raw` is a fresh handle we own.
    let job = unsafe { OwnedHandle::from_raw_handle(raw) };
    // SAFETY: plain data; only the limit flags are set.
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: valid job handle and a struct of the declared size.
    let ok = unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            &limits as *const _ as *const _,
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error()).context("configure job object");
    }
    Ok(job)
}

/// A child that leads its own process group inside its own job. Dropping it
/// stops the group, so an early return or a panic in `ns dev` never leaves
/// the app running.
pub struct GroupChild {
    child: Child,
    job: OwnedHandle,
    status: Option<ExitStatus>,
    group_gone: bool,
}

impl GroupChild {
    pub fn spawn(mut cmd: Command) -> Result<GroupChild> {
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
        let job = new_job()?;
        let child = cmd
            .spawn()
            .with_context(|| format!("spawn {:?}", cmd.get_program()))?;
        // A process the child starts before this call escapes the job; the
        // Ctrl-Break to its process group still reaches it.
        // SAFETY: both handles are valid and owned.
        if unsafe { AssignProcessToJobObject(job.as_raw_handle(), child.as_raw_handle()) } == 0 {
            tracing::debug!(
                "child not assigned to a job: {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(GroupChild {
            child,
            job,
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

    /// Exit status of the group leader once it has exited.
    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
        if self.status.is_none() {
            self.status = self.child.try_wait().context("wait for child")?;
        }
        Ok(self.status)
    }

    /// True while any process of the job exists (or, if the job cannot be
    /// queried, while the leader runs).
    fn group_alive(&mut self) -> bool {
        if self.group_gone {
            return false;
        }
        // SAFETY: plain data, filled by the query.
        let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: valid job handle and a struct of the declared size.
        let ok = unsafe {
            QueryInformationJobObject(
                self.job.as_raw_handle(),
                JobObjectBasicAccountingInformation,
                &mut info as *mut _ as *mut _,
                size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                std::ptr::null_mut(),
            )
        };
        let alive = if ok != 0 {
            info.ActiveProcesses > 0
        } else {
            matches!(self.try_wait(), Ok(None))
        };
        if !alive {
            self.group_gone = true;
        }
        alive
    }

    /// Ctrl-Break to the group, wait up to `grace` for every member to exit,
    /// then terminate the job. Returns the leader's exit status.
    pub fn stop(&mut self, grace: Duration) -> Option<ExitStatus> {
        // SAFETY: the group id is the leader's pid (CREATE_NEW_PROCESS_GROUP).
        // Fails harmlessly when there is no shared console.
        unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, self.child.id()) };
        let deadline = Instant::now() + grace;
        loop {
            let _ = self.try_wait();
            if !self.group_alive() {
                break;
            }
            if Instant::now() >= deadline {
                // SAFETY: valid job handle.
                unsafe { TerminateJobObject(self.job.as_raw_handle(), 1) };
                let _ = self.child.kill();
                break;
            }
            std::thread::sleep(POLL);
        }
        if self.status.is_none() {
            self.status = self.child.wait().ok();
        }
        // Wait briefly so ports are released.
        let deadline = Instant::now() + Duration::from_secs(1);
        while self.group_alive() && Instant::now() < deadline {
            std::thread::sleep(POLL);
        }
        self.group_gone = true;
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
        // `cmd` waits on a grandchild `ping`, which survives Ctrl-Break: the
        // job must still take both down.
        let mut cmd = Command::new("cmd");
        cmd.args(["/C", "ping -n 30 127.0.0.1 >NUL"]);
        let mut child = GroupChild::spawn(cmd).unwrap();
        std::thread::sleep(Duration::from_millis(500));
        assert!(child.group_alive());
        let started = Instant::now();
        child.stop(Duration::from_millis(300));
        assert!(started.elapsed() < Duration::from_secs(3));
        child.group_gone = false;
        assert!(!child.group_alive(), "job still has processes after stop");
    }
}
