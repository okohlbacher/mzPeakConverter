//! A timsTOF `.d` whose `MzCalibration` row is ModelType 2, exported with `--to mzml`, has its m/z
//! on timsrust's two-point chord — inside the run's acquisition range — as the archive lanes read
//! it since 0.14.0 (`mzdata_tdf_needs_chord`).
//!
//! mzdata 0.67.1 reads every row as ModelType 1; a ModelType-2 row's `C3`/`C4` (copies of `C0`/`C2`)
//! then become a cubic term and an m/z shift. Through 0.16.0 the `--to mzml` lane kept that reading:
//! on the corpus's SBA415 run (timsTOF Pro, the one ModelType-2 file of the corpus) the first frame's
//! m/z 270.18 … 1055.84 came out as 21.03 … 35.95.

use std::path::Path;
use std::process::Command;

#[path = "common/corpus.rs"]
mod corpus;

const SBA415: &str = "general-ms/bruker-timstof-pro/SBA415_Try.d/SBA415(1) Try_Slot1-2_1_8271.d";

/// `GlobalMetadata` value `key` of the run's `analysis.tdf`, read without writing beside it.
fn global(dot_d: &Path, key: &str) -> f64 {
    let uri = format!("file:{}?immutable=1", dot_d.join("analysis.tdf").display());
    let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI;
    let conn = rusqlite::Connection::open_with_flags(uri, flags).unwrap();
    let v: String = conn.query_row("SELECT Value FROM GlobalMetadata WHERE Key = ?1", [key], |r| r.get(0)).unwrap();
    v.parse().unwrap()
}

#[test]
#[ignore = "needs the 2 GB SBA415 timsTOF corpus fixture (MZPEAK_CORPUS); run with --include-ignored"]
fn a_modeltype2_tdf_exports_its_mz_on_the_chord() {
    use mzdata::prelude::*;
    let Some(dot_d) = corpus::corpus_path(SBA415) else { return };
    let dir = std::env::temp_dir().join(format!("mzpc-tdf-mt2-mzml-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mzml = dir.join("sba415.mzML");
    let out = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"))
        .arg(&dot_d)
        .args(["--to", "mzml", "--force", "-o"])
        .arg(&mzml)
        .env("MZPC_MAX_SPECTRA", "3")
        .output()
        .unwrap();
    let log = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{log}");

    let (lo, hi) = (global(&dot_d, "MzAcqRangeLower"), global(&dot_d, "MzAcqRangeUpper"));
    let spectra: Vec<_> = mzdata::io::mzml::MzMLReader::open_path(&mzml).unwrap().iter().collect();
    assert_eq!(spectra.len(), 3);
    for s in &spectra {
        let mzs = s.arrays.as_ref().unwrap().mzs().unwrap();
        assert!(!mzs.is_empty(), "{}: no peaks", s.id());
        let (min, max) = mzs.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), &v| (a.min(v), b.max(v)));
        // The chord meets the vendor model at the range's ends and is −5 … −11 ppm off inside it.
        assert!(min >= lo * (1.0 - 20e-6) && max <= hi * (1.0 + 20e-6), "{}: m/z {min} ..= {max} outside the acquisition range {lo} ..= {hi}", s.id());
    }
    // And the export says so, as mzML cannot declare it.
    assert!(log.contains("two-point chord") && log.contains("no transformations list"), "no chord warning in {log}");
    let _ = std::fs::remove_dir_all(&dir);
}
