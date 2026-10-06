//! A plugin's life is tied to the engine's at the operating system (#39).
//!
//! The supervisor stops its children on the engine's way out. That covers
//! an engine ending on its own terms. One that is killed, crashes, or is
//! taken down with its parent runs no destructors, and the plugins it
//! started kept running until someone found them: holding the port they
//! registered on, and counted by the next engine's probe as a plugin that
//! is up. So the kernel is asked to end them with the engine, however the
//! engine ends, and the engine stops any it finds left over from before.

use std::path::Path;

/// Ties `child` to this process: when this process ends, by any route, the
/// kernel ends the child.
///
/// On Windows, a Job Object with "kill on close", created once and never
/// closed; the kernel closes it when the process goes, and everything in it
/// with it. On Linux, the parent-death signal, set in the child before it
/// executes. Elsewhere there is nothing to tie with, and `kill_on_drop`
/// remains what there is.
///
/// # Errors
///
/// The tie could not be made. The child runs on untied, as before; the
/// caller says so and goes on.
#[cfg(windows)]
pub fn tie(child: &tokio::process::Child) -> Result<(), String> {
    use std::sync::OnceLock;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    /// The one job every plugin goes into. A handle is a pointer, which is
    /// not `Send`; it is only ever passed to the kernel.
    struct Job(HANDLE);
    // SAFETY: a job handle may be used from any thread.
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}
    static JOB: OnceLock<Option<Job>> = OnceLock::new();

    let job = JOB
        .get_or_init(|| {
            // SAFETY: plain Win32 calls with valid arguments; the handle is
            // closed on the one failure path and otherwise kept for the
            // process's life, which is the point.
            unsafe {
                let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if job.is_null() {
                    return None;
                }
                let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let set = SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    std::ptr::addr_of!(limits).cast(),
                    u32::try_from(std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()).unwrap_or(0),
                );
                if set == 0 {
                    CloseHandle(job);
                    return None;
                }
                Some(Job(job))
            }
        })
        .as_ref()
        .map(|job| job.0)
        .ok_or_else(|| format!("no job object: {}", std::io::Error::last_os_error()))?;
    let handle = child.raw_handle().ok_or("the child has already ended")?;
    // SAFETY: both handles are live; the call only reads them.
    if unsafe { AssignProcessToJobObject(job, handle.cast()) } == 0 {
        return Err(format!("could not join the job: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}

/// See the Windows [`tie`]. Linux: the child asks for SIGTERM when its
/// parent thread ends, before it executes. The thread is the runtime worker
/// that spawned it, which lives as long as the runtime; the check after
/// covers the parent having gone between the fork and the request.
#[cfg(target_os = "linux")]
pub fn tie(command: &mut tokio::process::Command) {
    // SAFETY: getpid is async-signal-safe and so is everything in the
    // pre-exec closure; nothing allocates.
    let parent = unsafe { libc::getpid() };
    unsafe {
        command.pre_exec(move || {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            if libc::getppid() != parent {
                libc::raise(libc::SIGTERM);
            }
            Ok(())
        });
    }
}

/// Stops every process running `program` that is not a child of this one:
/// a plugin an earlier engine left behind. Returns the ids stopped.
///
/// Only an engine serving this root starts a plugin from this root's
/// extension cache, and the engine has already made sure it is the only one
/// serving the root, so whatever runs that binary now has no engine behind
/// it. For orphans left by builds before [`tie`]; with it there are none.
#[must_use]
pub fn reap_orphans(program: &Path) -> Vec<u32> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

    let Ok(program) = std::fs::canonicalize(program) else {
        return Vec::new();
    };
    let me = sysinfo::get_current_pid().ok();
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_exe(UpdateKind::Always),
    );
    let mut stopped = Vec::new();
    for (pid, process) in system.processes() {
        if process.parent() == me {
            continue;
        }
        let same = process.exe().and_then(|exe| std::fs::canonicalize(exe).ok()).is_some_and(|exe| exe == program);
        if same && process.kill() {
            stopped.push(pid.as_u32());
        }
    }
    stopped
}

#[cfg(test)]
mod tests {
    /// The tie is real: a child spawned through it is in the engine's job.
    /// (What the job does when the engine dies cannot be watched from
    /// inside the engine; the kernel's word on membership is what there is.)
    #[cfg(windows)]
    #[tokio::test]
    async fn a_child_is_tied_to_this_process() {
        use windows_sys::Win32::System::JobObjects::IsProcessInJob;

        let mut child = tokio::process::Command::new("cmd")
            .args(["/C", "ping -n 30 127.0.0.1 > NUL"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("cmd exists");
        let handle = child.raw_handle().expect("alive");
        let mut before = 0i32;
        // SAFETY: a live process handle and a null job: "is it in any job?"
        unsafe { IsProcessInJob(handle.cast(), std::ptr::null_mut(), &raw mut before) };

        super::tie(&child).expect("tied");

        let mut after = 0i32;
        // SAFETY: as above.
        unsafe { IsProcessInJob(handle.cast(), std::ptr::null_mut(), &raw mut after) };
        assert_eq!(after, 1, "in a job now");
        let _ = child.kill().await;
    }

    #[test]
    fn nothing_to_reap_is_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(super::reap_orphans(&dir.path().join("arvo-plugin-nobody.exe")).is_empty());
    }
}
