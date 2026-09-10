//! The Agilent MHDAC host process: where it writes, how long it may run, and the wait that holds it
//! to that.
//!
//! Host-independent ON PURPOSE (the `sciex_run` pattern): `agilent.rs` is `#[cfg(windows)]`, so
//! nothing in it compiles or runs here. It used to spawn the host with a bare `Command::output()`:
//! no deadline, and a `MZPC_AGILENT_TMPDIR` that named no directory ignored in silence. The
//! decisions and the wait live here with tests that run on every host. Standard library only.
//!
//! Not covered: a converter that is itself killed leaves the host running, writing its temp file
//! (Windows does not end children with their parent). A kill-on-close Job Object would end it, but
//! windows-sys is in the tree without its `Win32_System_JobObjects` feature, so that waits for a
//! dependency change (BACKLOG).
#![cfg_attr(not(windows), allow(dead_code))]

use std::ffi::OsString;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
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
/// writing. stdin and stdout are null (the host reads nothing and prints its notes to stderr);
/// stderr is drained on a thread, as `output()` did, so a host that writes a lot there cannot fill
/// the pipe and stall this wait until the deadline.
pub fn run_with_deadline(cmd: &mut Command, timeout: Option<Duration>) -> std::io::Result<HostExit> {
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn()?;
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
        let exit = run_with_deadline(Command::new("sleep").arg("30"), Some(Duration::from_millis(300))).unwrap();
        assert!(matches!(exit, HostExit::TimedOut), "{exit:?}");
        assert!(started.elapsed() < Duration::from_secs(10), "returned after {:?}", started.elapsed());
    }

    #[cfg(unix)]
    #[test]
    fn a_host_that_exits_reports_its_status_and_all_of_stderr() {
        // 1 MiB on stderr, more than a pipe buffer holds: a wait that does not drain it stalls.
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "head -c 1048576 /dev/zero >&2; exit 3"]);
        match run_with_deadline(&mut cmd, Some(Duration::from_secs(60))).unwrap() {
            HostExit::Exited { status, stderr } => {
                assert_eq!(status.code(), Some(3));
                assert_eq!(stderr.len(), 1 << 20);
            }
            HostExit::TimedOut => panic!("a 1 MiB stderr stalled the wait until the deadline"),
        }
    }
}
