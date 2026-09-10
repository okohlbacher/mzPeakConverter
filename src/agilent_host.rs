//! The Agilent MHDAC host process: where it writes, how long it may run, and the wait that holds it
//! to that.
//!
//! Host-independent ON PURPOSE (the `sciex_run` pattern): `agilent.rs` is `#[cfg(windows)]`, so
//! nothing in it compiles or runs here. It used to spawn the host with a bare `Command::output()`:
//! no deadline, nothing to stop the host when the converter was killed, and a `MZPC_AGILENT_TMPDIR`
//! that named no directory ignored in silence. The decisions and the wait live here with tests that
//! run on every host; only the Job Object at the bottom is Windows code. Standard library only.
#![cfg_attr(not(windows), allow(dead_code))]

use std::ffi::OsString;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// The host's deadline when `MZPC_AGILENT_HOST_TIMEOUT` is unset: two hours. The host needs about
/// 20 s for a 242 MB Q-TOF profile run (181 M points), so this stops only a host that is stuck — a
/// wedged MHDAC call on a locked `.d` or a dead share would otherwise hold a corpus unit forever.
pub const DEFAULT_HOST_TIMEOUT: Duration = Duration::from_secs(2 * 60 * 60);

/// `MZPC_AGILENT_HOST_TIMEOUT` in whole seconds: `0` = no deadline (`None`); unset or empty = the
/// default. Anything else is an error, raised before the host runs rather than hours into it.
pub fn host_timeout(raw: Option<&str>) -> Result<Option<Duration>, String> {
    let Some(s) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(Some(DEFAULT_HOST_TIMEOUT));
    };
    match s.parse::<u64>() {
        Ok(0) => Ok(None),
        Ok(n) => Ok(Some(Duration::from_secs(n))),
        Err(_) => Err(format!(
            "MZPC_AGILENT_HOST_TIMEOUT={s:?} is not a number of seconds; set a count (default {}), \
             0 for no deadline, or unset it",
            DEFAULT_HOST_TIMEOUT.as_secs()
        )),
    }
}

/// Where the host materialises the run (16 B/point): `MZPC_AGILENT_TMPDIR` when it names a
/// directory, else `default`. A value that is set but names no directory used to fall back in
/// silence, putting gigabytes on the drive the variable was set to avoid; the fallback stays, and
/// the second element is the warning that says so. Empty counts as unset.
pub fn tmp_dir(raw: Option<OsString>, default: PathBuf) -> (PathBuf, Option<String>) {
    match raw.filter(|v| !v.is_empty()).map(PathBuf::from) {
        None => (default, None),
        Some(dir) if dir.is_dir() => (dir, None),
        Some(dir) => {
            let warning = format!(
                "MZPC_AGILENT_TMPDIR={} is not a directory; the Agilent host writes its temp file \
                 (16 B/point) to {} instead",
                dir.display(),
                default.display()
            );
            (default, Some(warning))
        }
    }
}

/// How the host process ended.
#[derive(Debug)]
pub enum HostExit {
    /// On its own: its exit status and everything it wrote to stderr.
    Exited { status: ExitStatus, stderr: Vec<u8> },
    /// Past its deadline: killed and reaped, so it no longer holds its files open.
    TimedOut,
}

/// Spawn `cmd` and wait for it, for at most `timeout` (`None` = no deadline). Past the deadline the
/// child is killed and reaped before this returns, so the caller can remove the files it was
/// writing. `on_spawn` runs right after the spawn: the Windows lane puts the child into its
/// kill-on-close Job Object there. stdin and stdout are null (the host reads nothing and prints its
/// notes to stderr); stderr is drained on a thread, so a host that writes a lot there cannot fill
/// the pipe and stall until the deadline.
pub fn run_with_deadline(
    cmd: &mut Command,
    timeout: Option<Duration>,
    on_spawn: impl FnOnce(&Child),
) -> std::io::Result<HostExit> {
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn()?;
    on_spawn(&child);
    let mut pipe = child.stderr.take().expect("stderr was piped");
    let drain = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        buf
    });
    let deadline = timeout.map(|t| Instant::now() + t);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return Ok(HostExit::Exited { status, stderr: drain.join().unwrap_or_default() });
            }
            Ok(None) if deadline.is_none_or(|d| Instant::now() < d) => {
                std::thread::sleep(Duration::from_millis(100));
            }
            // Past the deadline, or the wait itself failed: never leave the host running behind us.
            // The drain thread ends when the pipe closes; it is not joined, so a process that
            // inherited the pipe cannot hang the caller.
            other => {
                let _ = child.kill();
                let _ = child.wait();
                return other.map(|_| HostExit::TimedOut);
            }
        }
    }
}

/// A Job Object that ends the processes inside it when its last handle closes — which Windows does
/// for a process that is killed (Task Manager, `Stop-Process`, a harness timeout). Windows does not
/// end children with their parent, so without it a killed converter left the host running and
/// writing its multi-GB `.part`.
///
/// Four kernel32 calls, declared here: windows-sys is in the tree, but without its
/// `Win32_System_JobObjects` feature, and these do not justify a dependency change. The layouts
/// follow windows-sys 0.61's `JOBOBJECT_EXTENDED_LIMIT_INFORMATION` (144 bytes on 64-bit Windows).
#[cfg(windows)]
pub mod job {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;

    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;
    /// `JOBOBJECTINFOCLASS::JobObjectExtendedLimitInformation`.
    const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: i32 = 9;

    #[repr(C)]
    #[derive(Default)]
    #[allow(dead_code)] // filled in for the kernel, never read back
    struct BasicLimitInformation {
        per_process_user_time_limit: i64,
        per_job_user_time_limit: i64,
        limit_flags: u32,
        minimum_working_set_size: usize,
        maximum_working_set_size: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    #[allow(dead_code)]
    struct IoCounters {
        read_operation_count: u64,
        write_operation_count: u64,
        other_operation_count: u64,
        read_transfer_count: u64,
        write_transfer_count: u64,
        other_transfer_count: u64,
    }

    #[repr(C)]
    #[derive(Default)]
    #[allow(dead_code)]
    struct ExtendedLimitInformation {
        basic_limit_information: BasicLimitInformation,
        io_info: IoCounters,
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory_used: usize,
        peak_job_memory_used: usize,
    }

    #[cfg(target_pointer_width = "64")]
    const _: () = assert!(std::mem::size_of::<ExtendedLimitInformation>() == 144);
    #[cfg(target_pointer_width = "64")]
    const _: () = assert!(std::mem::offset_of!(BasicLimitInformation, limit_flags) == 16);

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateJobObjectW(attributes: *const c_void, name: *const u16) -> *mut c_void;
        fn SetInformationJobObject(job: *mut c_void, class: i32, info: *const c_void, len: u32) -> i32;
        fn AssignProcessToJobObject(job: *mut c_void, process: *mut c_void) -> i32;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }

    /// The job's handle; dropping it closes the handle, which ends whatever is still inside.
    pub struct KillOnClose(*mut c_void);

    impl Drop for KillOnClose {
        fn drop(&mut self) {
            // SAFETY: a handle CreateJobObjectW returned, closed exactly once.
            unsafe { CloseHandle(self.0) };
        }
    }

    /// Put `child` into a new kill-on-close job.
    pub fn kill_on_close(child: &Child) -> std::io::Result<KillOnClose> {
        // SAFETY: kernel32 calls on the job handle created here (owned by `job`, so every early
        // return closes it) and on the live child's process handle; `info` outlives the call that
        // reads it, and `len` is its size.
        unsafe {
            let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if handle.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            let job = KillOnClose(handle);
            let mut info = ExtendedLimitInformation::default();
            info.basic_limit_information.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let len = std::mem::size_of::<ExtendedLimitInformation>() as u32;
            let info_ptr = std::ptr::from_ref(&info).cast::<c_void>();
            if SetInformationJobObject(job.0, JOB_OBJECT_EXTENDED_LIMIT_INFORMATION, info_ptr, len) == 0 {
                return Err(std::io::Error::last_os_error());
            }
            if AssignProcessToJobObject(job.0, child.as_raw_handle().cast()) == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(job)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_timeout_takes_seconds_zero_or_nothing() {
        assert_eq!(host_timeout(None), Ok(Some(DEFAULT_HOST_TIMEOUT)));
        assert_eq!(host_timeout(Some("  ")), Ok(Some(DEFAULT_HOST_TIMEOUT)), "empty is unset");
        assert_eq!(host_timeout(Some("90")), Ok(Some(Duration::from_secs(90))));
        assert_eq!(host_timeout(Some("0")), Ok(None), "0 = no deadline");
        let err = host_timeout(Some("2h")).unwrap_err();
        assert!(err.starts_with("MZPC_AGILENT_HOST_TIMEOUT=\"2h\" is not a number of seconds"), "{err}");
    }

    #[test]
    fn a_tmpdir_that_is_not_a_directory_is_named_not_ignored() {
        let default = std::env::temp_dir();
        let file = default.join(format!("mzpc-agilent-tmpdir-{}", std::process::id()));
        std::fs::write(&file, b"x").unwrap();
        let (dir, warning) = tmp_dir(Some(file.clone().into_os_string()), default.clone());
        std::fs::remove_file(&file).ok();
        assert_eq!(dir, default, "the fallback stays");
        let warning = warning.expect("a non-directory is warned about");
        assert!(warning.starts_with(&format!("MZPC_AGILENT_TMPDIR={} is not a directory", file.display())), "{warning}");
        let elsewhere = PathBuf::from("/nonexistent-default");
        assert_eq!(tmp_dir(Some(default.clone().into_os_string()), elsewhere.clone()), (default.clone(), None));
        assert_eq!(tmp_dir(Some(OsString::new()), elsewhere.clone()), (elsewhere.clone(), None), "empty is unset");
        assert_eq!(tmp_dir(None, elsewhere.clone()), (elsewhere, None));
    }

    #[cfg(unix)]
    #[test]
    fn a_host_past_its_deadline_is_killed_and_reaped() {
        let started = Instant::now();
        let exit = run_with_deadline(Command::new("sleep").arg("30"), Some(Duration::from_millis(300)), |_| {})
            .unwrap();
        assert!(matches!(exit, HostExit::TimedOut), "{exit:?}");
        assert!(started.elapsed() < Duration::from_secs(10), "returned after {:?}", started.elapsed());
    }

    #[cfg(unix)]
    #[test]
    fn a_host_that_exits_reports_its_status_and_all_of_stderr() {
        // 1 MiB on stderr, more than a pipe buffer holds: a wait that does not drain it stalls.
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "head -c 1048576 /dev/zero >&2; exit 3"]);
        let mut spawned = false;
        match run_with_deadline(&mut cmd, Some(Duration::from_secs(60)), |_| spawned = true).unwrap() {
            HostExit::Exited { status, stderr } => {
                assert_eq!(status.code(), Some(3));
                assert_eq!(stderr.len(), 1 << 20);
            }
            HostExit::TimedOut => panic!("a 1 MiB stderr stalled the wait until the deadline"),
        }
        assert!(spawned, "on_spawn runs once the child exists");
    }
}
