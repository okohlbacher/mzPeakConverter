//! `-v` prints the inspection report beside a conversion; the report must never be what fails it.
//!
//! The report ran before the lane was chosen, with a bare `?`: `-v --via-msconvert` on a `.wiff`
//! without the native SciEX stack exited with the native reader's error and wrote nothing. With an
//! output given, a report error is now a `note:` line and the chosen lane runs; without one the
//! report is the whole job and its error stays the run's error.
//!
//! The native vendor branches exist only on Windows. The host-runnable trigger is a TSF `.d` whose
//! `analysis.tsf` is not SQLite: the report fails to open it, and so does the conversion, with its
//! own `converting …` context — which can only appear if the report did not end the run first.

use std::process::Command;

#[test]
fn a_failing_report_under_verbose_is_a_note_not_the_error() {
    let dir = std::env::temp_dir().join(format!("mzpc-verbose-inspect-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let run = dir.join("broken.d");
    std::fs::create_dir_all(&run).unwrap();
    std::fs::write(run.join("analysis.tsf"), b"this is not an SQLite database").unwrap();
    let exe = env!("CARGO_BIN_EXE_mzpeak-convert");

    let converting = Command::new(exe)
        .arg(&run)
        .arg("-o")
        .arg(dir.join("out.mzpeak"))
        .arg("-v")
        .output()
        .expect("failed to run mzpeak-convert");
    let inspecting = Command::new(exe).arg(&run).output().expect("failed to run mzpeak-convert");
    let _ = std::fs::remove_dir_all(&dir);

    let stdout = String::from_utf8_lossy(&converting.stdout);
    let stderr = String::from_utf8_lossy(&converting.stderr);
    assert!(
        stdout.contains("note:          inspection failed: reading TSF"),
        "the report's error must be a note; stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(!converting.status.success(), "the broken run still fails to convert");
    assert!(stderr.contains("error: converting "), "the lane must run after the report; stderr:\n{stderr}");

    let stdout = String::from_utf8_lossy(&inspecting.stdout);
    let stderr = String::from_utf8_lossy(&inspecting.stderr);
    assert!(!inspecting.status.success(), "without an output the report is the job, and it failed");
    assert!(!stdout.contains("inspection failed"), "no note when the report is the job; stdout:\n{stdout}");
    assert!(stderr.contains("error: reading TSF"), "stderr:\n{stderr}");
}
