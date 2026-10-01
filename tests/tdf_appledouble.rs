//! A timsTOF `.d` copied from a Mac to NTFS, exFAT or SMB carries AppleDouble companions
//! (`._analysis.tdf`, 163 bytes of Finder metadata, beside every file). timsrust 0.4.1 opens the
//! first entry whose name ENDS WITH `analysis.tdf` / `analysis.tdf_bin`, and NTFS and APFS list the
//! companion first, so every lane that reads a `.d` through timsrust — the default ims-compact lane,
//! `--no-ims-compact`, `--to mzml` and inspection, the last three through mzdata — failed with
//! "file is not a database" and no word about the cause. Each now refuses such a `.d` before opening
//! it, naming the file and the fix. A volume that lists the companion AFTER the run's file (a fresh
//! copy onto exFAT) never let timsrust reach it; that `.d` converts as before, with a warning.
//! `--bruker-sdk` opens the exact names and is not refused.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A readable synthetic TDF of three empty frames.
fn synthetic_dot_d(dir: &Path) -> PathBuf {
    let dot_d = dir.join("run.d");
    std::fs::create_dir_all(&dot_d).unwrap();
    std::fs::write(dot_d.join("analysis.tdf_bin"), [0u8; 8]).unwrap();
    let conn = rusqlite::Connection::open(dot_d.join("analysis.tdf")).unwrap();
    conn.execute_batch(
        "CREATE TABLE GlobalMetadata (Key TEXT, Value TEXT);
         INSERT INTO GlobalMetadata VALUES ('TimsCompressionType', '2'), ('AcquisitionSoftware', 'timsTOF'),
             ('MzAcqRangeLower', '100'), ('MzAcqRangeUpper', '2000'), ('DigitizerNumSamples', '439442'),
             ('OneOverK0AcqRangeLower', '0.78'), ('OneOverK0AcqRangeUpper', '1.6');
         CREATE TABLE Frames (Id INTEGER PRIMARY KEY, Time REAL, Polarity TEXT, ScanMode INTEGER, MsMsType INTEGER,
             TimsId INTEGER, NumScans INTEGER, NumPeaks INTEGER, AccumulationTime REAL);
         INSERT INTO Frames VALUES (1, 0.5, '+', 20, 0, 0, 900, 0, 100.0), (2, 0.6, '+', 20, 0, 0, 900, 0, 100.0),
                                   (3, 0.9, '+', 20, 0, 0, 900, 0, 100.0);",
    )
    .unwrap();
    dot_d
}

/// A 163-byte AppleDouble header (magic 0x00051607, version 2, "Mac OS X" filler).
fn appledouble() -> Vec<u8> {
    let mut b = vec![0x00, 0x05, 0x16, 0x07, 0x00, 0x02, 0x00, 0x00];
    b.extend_from_slice(b"Mac OS X        ");
    b.resize(163, 0);
    b
}

fn convert(input: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_mzpeak-convert")).arg(input).args(args).output().expect("run mzpeak-convert")
}

/// Every timsrust lane agrees with timsrust's own lookup on whatever order this volume lists the
/// `.d` in. When timsrust would read the stub, the lane refuses the `.d`, names the file and the fix,
/// and writes nothing. When the stub lists after the run's file, the lane ends exactly as it does
/// on the same `.d` without the stub (the synthetic run's mzML export panics in mzdata's writer for
/// want of an instrument either way), with a warning added. APFS lists `._analysis.tdf` before
/// `analysis.tdf` and `old_analysis.tdf` after it, so on a Mac both branches run.
#[test]
fn every_timsrust_lane_refuses_a_lookalike_exactly_when_timsrust_would_read_it() {
    for (i, lookalike) in ["._analysis.tdf", "old_analysis.tdf"].into_iter().enumerate() {
        let dir = std::env::temp_dir().join(format!("mzpc-tdf-appledouble-{}-{i}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dot_d = synthetic_dot_d(&dir);
        let (archive, mzml) = (dir.join("out.mzpeak"), dir.join("out.mzML"));
        let (a, m) = (archive.to_str().unwrap(), mzml.to_str().unwrap());
        let lanes = [
            ("default (ims-compact)", vec!["-o", a]),
            ("--no-ims-compact", vec!["-o", a, "--no-ims-compact"]),
            ("--to mzml", vec!["-o", m, "--to", "mzml"]),
            ("inspection", vec![]),
        ];
        let run = |args: &[&str]| {
            let _ = (std::fs::remove_file(&archive), std::fs::remove_file(&mzml));
            let out = convert(&dot_d, args);
            (out, archive.is_file(), mzml.is_file())
        };
        let without: Vec<_> = lanes.iter().map(|(_, args)| run(args)).collect();
        std::fs::write(dot_d.join(lookalike), appledouble()).unwrap();
        // timsrust's own lookup, unguarded: the 163-byte stub is no database.
        let timsrust_reads_the_run = timsrust::readers::FrameReader::new(&dot_d).is_ok();
        eprintln!("{lookalike}: timsrust reads the run: {timsrust_reads_the_run}");
        for ((lane, args), (before, had_archive, had_mzml)) in lanes.iter().zip(&without) {
            let (out, has_archive, has_mzml) = run(args);
            let stderr = String::from_utf8_lossy(&out.stderr);
            if timsrust_reads_the_run {
                assert_eq!(
                    (out.status.code(), has_archive, has_mzml),
                    (before.status.code(), *had_archive, *had_mzml),
                    "{lane}: {lookalike} listed after the run changed the outcome; stderr:\n{stderr}"
                );
                assert!(
                    stderr.contains(&format!("holds {lookalike} beside analysis.tdf")) && stderr.contains("listed after them"),
                    "{lane}: no warning names {lookalike}; stderr:\n{stderr}"
                );
            } else {
                assert!(!out.status.success(), "{lane}: converted a .d whose {lookalike} timsrust reads; stderr:\n{stderr}");
                assert!(
                    stderr.contains(&format!("lists {lookalike} before analysis.tdf")) && stderr.contains("--bruker-sdk"),
                    "{lane}: {lookalike} and the fix are not named; stderr:\n{stderr}"
                );
                assert!(!has_archive && !has_mzml, "{lane}: an output was written");
            }
        }
        // The SDK lane opens `analysis.tdf` by name. Without the vendor library (macOS, CI) it fails
        // for that reason, never for the lookalike.
        let out = convert(&dot_d, &["-o", a, "--bruker-sdk"]);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!stderr.contains(lookalike), "--bruker-sdk was refused for {lookalike}; stderr:\n{stderr}");
        assert!(dot_d.join(lookalike).is_file(), "the converter never removes {lookalike}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
