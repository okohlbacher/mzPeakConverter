//! A timsTOF `.d` copied from a Mac to NTFS, exFAT or SMB carries AppleDouble companions
//! (`._analysis.tdf`, 163 bytes of Finder metadata, beside every file). timsrust 0.4.1 opens the
//! first entry whose name ENDS WITH `analysis.tdf` / `analysis.tdf_bin`, and NTFS and APFS list the
//! companion first, so every lane that reads a `.d` through timsrust — the default ims-compact lane,
//! `--no-ims-compact`, `--to mzml` and inspection, the last three through mzdata — failed with
//! "file is not a database" and no word about the cause. Each now refuses the `.d` before opening
//! it, naming the files and the fix; `--bruker-sdk` opens the exact names and is not refused.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A readable synthetic TDF of three empty frames plus the two AppleDouble companions.
fn dot_d_with_companions(dir: &Path) -> PathBuf {
    let dot_d = dir.join("run.d");
    std::fs::create_dir_all(&dot_d).unwrap();
    let mut appledouble = vec![0x00, 0x05, 0x16, 0x07, 0x00, 0x02, 0x00, 0x00];
    appledouble.extend_from_slice(b"Mac OS X        ");
    appledouble.resize(163, 0);
    std::fs::write(dot_d.join("._analysis.tdf"), &appledouble).unwrap();
    std::fs::write(dot_d.join("._analysis.tdf_bin"), &appledouble).unwrap();
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

fn convert(input: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_mzpeak-convert")).arg(input).args(args).output().expect("run mzpeak-convert")
}

#[test]
fn every_timsrust_lane_names_the_appledouble_companions_before_opening() {
    let dir = std::env::temp_dir().join(format!("mzpc-tdf-appledouble-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let dot_d = dot_d_with_companions(&dir);
    let (archive, mzml) = (dir.join("out.mzpeak"), dir.join("out.mzML"));
    let (a, m) = (archive.to_str().unwrap(), mzml.to_str().unwrap());
    for (lane, args) in [
        ("default (ims-compact)", vec!["-o", a]),
        ("--no-ims-compact", vec!["-o", a, "--no-ims-compact"]),
        ("--to mzml", vec!["-o", m, "--to", "mzml"]),
        ("inspection", vec![]),
    ] {
        let out = convert(&dot_d, &args);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{lane}: converted a .d with AppleDouble companions; stderr:\n{stderr}");
        assert!(
            stderr.contains("holds ._analysis.tdf, ._analysis.tdf_bin beside") && stderr.contains("--bruker-sdk"),
            "{lane}: the companions and the fix are not named; stderr:\n{stderr}"
        );
        assert!(!archive.exists() && !mzml.exists(), "{lane}: an output was written");
    }
    // The SDK lane opens `analysis.tdf` by name. Without the vendor library (macOS, CI) it fails for
    // that reason, never for the companions.
    let out = convert(&dot_d, &["-o", a, "--bruker-sdk"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("._analysis.tdf"), "--bruker-sdk was refused for the companions; stderr:\n{stderr}");
    assert!(dot_d.join("._analysis.tdf").is_file(), "the converter never removes the companions");
    let _ = std::fs::remove_dir_all(&dir);
}
