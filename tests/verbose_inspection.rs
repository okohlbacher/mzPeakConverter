//! `-v` prints the inspection report beside a conversion; the report must never be what fails it,
//! and it must not open a vendor library that the conversion opens again or does not need.
//!
//! The report ran before the lane was chosen, with a bare `?`: `-v --via-msconvert` on a `.wiff`
//! without the native SciEX stack exited with the native reader's error and wrote nothing. With an
//! output given, a report error is now a `note:` line and the chosen lane runs; without one the
//! report is the whole job and its error stays the run's error.
//!
//! The Agilent, SciEX, Waters and Shimadzu branches exist only on Windows. The host-runnable
//! trigger for a failing report is a TSF `.d` whose `analysis.tsf` is not SQLite: the report fails
//! to open it, and so does the conversion, with its own `converting …` context — which can only
//! appear if the report did not end the run first. The vendor library the report could open on
//! every platform is Thermo's RawFileReader (`small.RAW`); on Linux and Windows, Bruker's baf2sql
//! as well.

use std::path::Path;
use std::process::Command;

/// What the report prints under `-o` in place of opening a vendor reader.
const NOT_OPENED: &str = "note:          native reader not opened for this report: the conversion opens it";

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

/// mzdata reads a Thermo `.raw` through RawFileReader, an in-process .NET runtime. Beside a
/// conversion the report leaves it closed (it printed `spectra: 48` from an open of its own), and
/// the conversion still opens the file and writes the archive.
#[test]
fn verbose_leaves_the_thermo_reader_to_the_conversion() {
    let dir = std::env::temp_dir().join(format!("mzpc-verbose-thermo-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("small.mzpeak");
    let run = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"))
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/small.RAW"))
        .arg("-o")
        .arg(&out)
        .arg("-v")
        .env_remove("DOTNET_ROLL_FORWARD") // the binary's own Thermo default decides (tests/thermo_raw.rs)
        .output()
        .expect("failed to run mzpeak-convert");
    let converted = out.is_file();
    let _ = std::fs::remove_dir_all(&dir);

    let stdout = String::from_utf8_lossy(&run.stdout);
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(stdout.contains("format:        Thermo .raw"), "stdout:\n{stdout}");
    assert!(stdout.contains(NOT_OPENED), "the report must leave RawFileReader closed; stdout:\n{stdout}");
    assert!(!stdout.contains("spectra:"), "a spectrum count means the report opened the file; stdout:\n{stdout}");
    assert!(run.status.success() && converted, "the conversion still runs; stderr:\n{stderr}");
}

/// Bruker's baf2sql is a vendor library on Linux and Windows. Beside a conversion the report leaves
/// it closed; before, a missing baf2sql library was its error. Any non-empty `analysis.baf` makes
/// the `.d` a BAF run, and nothing here reads it. (The BAF lane, like this test, exists on Linux
/// and Windows only, so macOS never runs it.)
#[cfg(any(windows, target_os = "linux"))]
#[test]
fn verbose_leaves_the_baf_reader_to_the_conversion() {
    let dir = std::env::temp_dir().join(format!("mzpc-verbose-baf-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let run = dir.join("run.d");
    std::fs::create_dir_all(&run).unwrap();
    std::fs::write(run.join("analysis.baf"), b"not a BAF file").unwrap();
    let converting = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"))
        .arg(&run)
        .arg("-o")
        .arg(dir.join("run.mzpeak"))
        .arg("-v")
        .env_remove("TIMSDATA_LIB_DIR")
        .output()
        .expect("failed to run mzpeak-convert");
    let _ = std::fs::remove_dir_all(&dir);

    let stdout = String::from_utf8_lossy(&converting.stdout);
    assert!(stdout.contains("format:        Bruker BAF (.d)"), "stdout:\n{stdout}");
    assert!(stdout.contains(NOT_OPENED), "the report must leave baf2sql closed; stdout:\n{stdout}");
    assert!(!stdout.contains("inspection failed"), "the report opened baf2sql; stdout:\n{stdout}");
}
