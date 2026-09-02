//! No `<out>.mzpeak.tmp` may survive a failed conversion.
//!
//! Every lane writes the archive to `<out>.mzpeak.tmp` and renames it into place at the end. A
//! failure after that file exists — a writer error, a failed peak-writer open (a panic since
//! 0.9.5), or the rename itself — used to leave the partial `.tmp` beside the missing output.
//! `TmpGuard` in `main.rs` removes it on the error path (`Drop`) and on a panic (the panic hook,
//! because the release profile is `panic = "abort"` and runs no destructors).
//!
//! The host-runnable trigger here is the LAST failure point: the output path is an existing,
//! non-empty directory named like the archive, passed with `--force` so the pre-flight "output
//! exists" refusal lets it through, and the whole conversion runs before `rename(tmp, output)`
//! fails on the directory. The tmp therefore exists, complete, right before the failure.

use std::path::PathBuf;
use std::process::Command;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mzpc-tmp-cleanup-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn failed_rename_leaves_no_tmp_behind() {
    let dir = scratch("rename");
    // A non-empty directory where the archive should go: `rename(file, dir)` fails on every OS.
    let output = dir.join("out.mzpeak");
    std::fs::create_dir_all(&output).unwrap();
    std::fs::write(output.join("occupant"), b"x").unwrap();
    let tmp = dir.join("out.mzpeak.tmp");

    let result = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"))
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tiny.pwiz.1.1.mzML"))
        .arg("-o")
        .arg(&output)
        .arg("--force")
        .output()
        .expect("failed to run mzpeak-convert");
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(!result.status.success(), "conversion onto a directory must fail; stderr:\n{stderr}");
    // The failure is the rename — i.e. the tmp had been written in full before it.
    assert!(
        stderr.contains("finalizing"),
        "expected the rename ('finalizing …') to be the failure; stderr:\n{stderr}"
    );
    assert!(!tmp.exists(), "{} was left behind; stderr:\n{stderr}", tmp.display());
    assert!(output.join("occupant").is_file(), "the occupied output path must be untouched");
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .filter(|n| n.ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "stray tmp files: {leftovers:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The other temp file a conversion can create: an mzML with an empty self-closing
/// `<referenceableParamGroup/>` is converted from a sanitized copy (`mzpc-san-<pid>-<stem>.mzML`
/// in the temp dir; mzdata panics on the original). It used to be removed only after a successful
/// run — every failure, and every `-o x.mzML` export, left it behind. Same failure trigger as
/// above (rename onto an occupied directory), so the copy exists right up to the failure.
#[test]
fn failed_conversion_leaves_no_sanitized_copy_behind() {
    let dir = scratch("sanitized");
    // A unique stem: the sanitized copy is named after it, so we can find it in the temp dir.
    let stem = format!("mzpc-san-probe-{}", std::process::id());
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/tiny.pwiz.1.1.mzML"
    ))
    .unwrap();
    let marker = "<referenceableParamGroupList count=\"2\">";
    assert!(src.contains(marker), "fixture layout changed");
    let with_empty_group = src.replacen(
        marker,
        "<referenceableParamGroupList count=\"3\">\n      <referenceableParamGroup id=\"empty_probe\"/>",
        1,
    );
    let input = dir.join(format!("{stem}.mzML"));
    std::fs::write(&input, with_empty_group).unwrap();
    let output = dir.join("out.mzpeak");
    std::fs::create_dir_all(&output).unwrap();
    std::fs::write(output.join("occupant"), b"x").unwrap();

    let result = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"))
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--force")
        .env("RUST_LOG", "debug")
        .output()
        .expect("failed to run mzpeak-convert");
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(!result.status.success(), "conversion onto a directory must fail; stderr:\n{stderr}");
    // The sanitized copy really was made (else this test proves nothing) …
    assert!(
        stderr.contains("sanitized empty referenceableParamGroup"),
        "expected the sanitize path to run; stderr:\n{stderr}"
    );
    // … and the failure came after it, at the rename.
    assert!(stderr.contains("finalizing"), "expected the rename to be the failure; stderr:\n{stderr}");
    assert!(!dir.join("out.mzpeak.tmp").exists(), "tmp left behind; stderr:\n{stderr}");
    let suffix = format!("-{stem}.mzML");
    let stray: Vec<_> = std::fs::read_dir(std::env::temp_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with("mzpc-san-") && n.ends_with(&suffix))
        .collect();
    assert!(stray.is_empty(), "sanitized copy left in the temp dir: {stray:?}; stderr:\n{stderr}");
    let _ = std::fs::remove_dir_all(&dir);
}
