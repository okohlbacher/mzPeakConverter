//! No partial mzML (or filtered mzPeak) may survive a failed export, and no `.tmp` beside it.
//!
//! The four mzML export sites and the mzPeak→mzPeak filter used to `File::create` the FINAL output
//! path directly: a failure after that point left a partial file under the output name and, under
//! `--force`, had already destroyed the previous output. The mzPeak convert lanes had `TmpGuard`
//! (write to `<out>.tmp`, remove on any exit, rename on success — `tests/tmp_cleanup.rs`); since
//! 0.9.13 these lanes are on it too. Same host-runnable trigger as that suite: the output path is
//! an existing, non-empty DIRECTORY named like the file, passed with `--force`, so the whole export
//! runs and only the final `rename(tmp, output)` fails — the tmp exists, complete, right before.

use std::path::{Path, PathBuf};
use std::process::Command;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tiny.pwiz.1.1.mzML");

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mzpc-mzml-atomic-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn run(args: &[&Path]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"))
        .args(args)
        .arg("--force")
        .output()
        .expect("failed to run mzpeak-convert")
}

/// An occupied directory at `output`, so the final rename fails; returns it.
fn occupied(output: &Path) {
    std::fs::create_dir_all(output).unwrap();
    std::fs::write(output.join("occupant"), b"x").unwrap();
}

/// Assert the export onto the occupied directory failed AT the rename, left `tmp` gone, the
/// occupant untouched and no `.tmp` of any name in `dir`.
fn assert_clean_failure(result: &std::process::Output, dir: &Path, output: &Path, tmp: &Path) {
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(!result.status.success(), "export onto a directory must fail; stderr:\n{stderr}");
    assert!(stderr.contains("finalizing"), "expected the rename ('finalizing …') to be the failure; stderr:\n{stderr}");
    assert!(!tmp.exists(), "{} was left behind; stderr:\n{stderr}", tmp.display());
    assert!(output.join("occupant").is_file(), "the occupied output path must be untouched");
    let leftovers: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .filter(|n| n.contains(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "stray tmp files: {leftovers:?}");
}

#[test]
fn failed_mzml_export_leaves_no_tmp_behind() {
    let dir = scratch("mzml");
    let output = dir.join("out.mzML");
    occupied(&output);
    let r = run(&[Path::new(FIXTURE), Path::new("-o"), &output]);
    assert_clean_failure(&r, &dir, &output, &dir.join("out.mzML.tmp"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// The gzip sink is chosen from the name it is handed, so the tmp keeps `.gz` last:
/// `out.mzML.gz` → `out.mzML.tmp.gz`.
#[test]
fn failed_gzipped_mzml_export_leaves_no_tmp_behind() {
    let dir = scratch("mzmlgz");
    let output = dir.join("out.mzML.gz");
    occupied(&output);
    let r = run(&[Path::new(FIXTURE), Path::new("-o"), &output]);
    assert!(
        String::from_utf8_lossy(&r.stderr).contains("gzip-compressing"),
        "the .gz request must still reach the gzip encoder through the tmp name"
    );
    assert_clean_failure(&r, &dir, &output, &dir.join("out.mzML.tmp.gz"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// The mzPeak-input lanes: the filter (`.mzpeak` → `.mzpeak`) and the mzPeak → mzML export.
#[test]
fn failed_mzpeak_filter_and_mzpeak_to_mzml_leave_no_tmp_behind() {
    let dir = scratch("filter");
    let archive = dir.join("src.mzpeak");
    let r = run(&[Path::new(FIXTURE), Path::new("-o"), &archive]);
    assert!(r.status.success(), "fixture conversion failed: {}", String::from_utf8_lossy(&r.stderr));

    let output = dir.join("out.mzpeak");
    occupied(&output);
    let r = run(&[&archive, Path::new("-o"), &output]);
    assert_clean_failure(&r, &dir, &output, &dir.join("out.mzpeak.tmp"));

    let output = dir.join("out.mzML");
    occupied(&output);
    let r = run(&[&archive, Path::new("-o"), &output]);
    assert_clean_failure(&r, &dir, &output, &dir.join("out.mzML.tmp"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// The success path: the export lands under the requested name, complete, with no tmp beside it.
#[test]
fn successful_mzml_export_is_renamed_into_place() {
    let dir = scratch("ok");
    let output = dir.join("out.mzML");
    let r = run(&[Path::new(FIXTURE), Path::new("-o"), &output]);
    assert!(r.status.success(), "export failed: {}", String::from_utf8_lossy(&r.stderr));
    assert!(output.is_file());
    assert!(!dir.join("out.mzML.tmp").exists());
    let inspect = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert")).arg(&output).output().unwrap();
    let text = String::from_utf8_lossy(&inspect.stdout);
    assert!(inspect.status.success() && text.contains("spectra:       4"), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// `--via-msconvert --to mzml --sample N` must hand msconvert `--runIndexSet <N-1>`: with one
/// `--outfile` it writes every run of a multi-sample WIFF in turn and the LAST wins, so without
/// the argument the export was the last sample under exit 0. The `.wiff` is never opened on the
/// way to msconvert, so the stand-in only records its arguments and copies its input into place.
#[cfg(unix)]
#[test]
fn via_msconvert_mzml_export_passes_the_sample_as_run_index_set() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch("sample");
    let input = dir.join("multi.wiff");
    std::fs::copy(FIXTURE, &input).unwrap();
    let argv = dir.join("argv");
    let script = dir.join("msconvert");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$@\" > '{argv}'\n\
             in=$1\n\
             while [ $# -gt 0 ]; do\n\
             case $1 in --outdir) outdir=$2 ;; --outfile) outfile=$2 ;; esac\n\
             shift\n\
             done\n\
             cp \"$in\" \"$outdir/$outfile\"\n",
            argv = argv.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let output = dir.join("out.mzML");
    let r = run(&[
        &input, Path::new("-o"), &output, Path::new("--to"), Path::new("mzml"),
        Path::new("--via-msconvert"), Path::new("--msconvert-path"), &script,
        Path::new("--sample"), Path::new("3"),
    ]);
    assert!(r.status.success(), "export failed: {}", String::from_utf8_lossy(&r.stderr));
    let args: Vec<String> = std::fs::read_to_string(&argv).unwrap().lines().map(String::from).collect();
    assert!(
        args.windows(2).any(|w| w == ["--runIndexSet", "2"]),
        "--sample 3 must reach msconvert as `--runIndexSet 2`; it got {args:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
