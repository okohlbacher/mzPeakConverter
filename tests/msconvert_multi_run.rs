//! A multi-sample WIFF through `--via-msconvert` without `--sample` is refused, not truncated.
//!
//! With one `--outfile`, msconvert writes every run of a multi-run source onto that same path in
//! turn and the last one wins (En_PPY: 117 samples, one survived) — exit 0, no warning. It prints
//! `writing output file: <path>` once per run before writing it (pwiz `msconvert.cpp`,
//! `processFile`), so both msconvert lanes count those lines and refuse more than one. The
//! stand-in below mimics that: two runs, or one when `--runIndexSet` picks it.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tiny.pwiz.1.1.mzML");

#[test]
fn via_msconvert_refuses_a_multi_run_wiff_without_sample() {
    let dir = std::env::temp_dir().join(format!("mzpc-msconvert-multirun-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
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
             runs='1 2'\n\
             while [ $# -gt 0 ]; do\n\
             case $1 in --outdir) outdir=$2 ;; --outfile) outfile=$2 ;; --runIndexSet) runs=1 ;; esac\n\
             shift\n\
             done\n\
             for r in $runs; do\n\
             echo \"writing output file: $outdir/$outfile\"\n\
             cp \"$in\" \"$outdir/$outfile\"\n\
             done\n",
            argv = argv.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let run = |output: &Path, extra: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"))
            .arg(&input)
            .arg("-o")
            .arg(output)
            .arg("--via-msconvert")
            .arg("--msconvert-path")
            .arg(&script)
            .args(extra)
            .output()
            .expect("failed to run mzpeak-convert")
    };

    for name in ["out.mzpeak", "out.mzML"] {
        let output = dir.join(name);
        let r = run(&output, &[]);
        let stderr = String::from_utf8_lossy(&r.stderr);
        assert!(
            !r.status.success() && stderr.contains("--sample"),
            "{name}: a two-run WIFF without --sample must be refused; stderr:\n{stderr}"
        );
        assert!(!output.exists(), "{name}: the refusal left an output holding one arbitrary run");

        // The other direction: one run written is not refused, so a blanket refusal cannot pass.
        let r = run(&output, &["--sample", "2"]);
        assert!(r.status.success(), "{name}: --sample 2 failed: {}", String::from_utf8_lossy(&r.stderr));
        assert!(output.exists(), "{name}: --sample 2 wrote nothing");
        let args: Vec<String> = std::fs::read_to_string(&argv).unwrap().lines().map(String::from).collect();
        assert!(
            args.windows(2).any(|w| w == ["--runIndexSet", "1"]),
            "{name}: --sample 2 must reach msconvert as `--runIndexSet 1`; it got {args:?}"
        );
    }

    // `--sample 0` used to become run index 0 (sample 1) on these lanes.
    let output = dir.join("zero.mzpeak");
    let r = run(&output, &["--sample", "0"]);
    assert!(
        !r.status.success() && !output.exists(),
        "--sample 0 must be refused; stderr:\n{}",
        String::from_utf8_lossy(&r.stderr)
    );
    let _ = std::fs::remove_dir_all(&dir);
}
