//! Every chromatogram time is stored in the unit `chromatograms_data` declares for it.
//!
//! The spec leaves that unit to the writer (it recommends minutes; `docs/schemas/chromatograms.md`)
//! and the vendored reader labels each time array with it, but the converter wrote one column in two
//! units. The mzML lane samples the column from the source's chromatograms, which ProteoWizard writes
//! in seconds (`UO:0000010`), then stored its synthesized TIC and BPC in minutes, the unit of the
//! spectrum start times they are built from: `tiny.pwiz.1.1.mzML`'s spectra at 0.7008 and 5.8905 min
//! read back as 0.7 and 5.9 seconds. And `--rt`, a window in minutes like `spectrum.time`, was
//! compared with the column as stored, so it cut the source's own `sic` trace at 0.05 s where
//! 0.05 min was asked for.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;

use arrow::array::{Array, Float64Array, StructArray, UInt64Array};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

const TINY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tiny.pwiz.1.1.mzML");
/// Non-indexed, so its chromatograms are not read: the column keeps the writer's default, minutes.
const CENTROID_ONLY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tiny_centroid_only.mzML");

/// A per-test scratch dir: cargo runs these tests in parallel inside one process.
fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("mzpc-chrom-unit-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn mzpc(input: &Path, output: &Path, extra: &[&str]) {
    let r = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"))
        .arg(input)
        .arg("-o")
        .arg(output)
        .arg("--force")
        .args(extra)
        .output()
        .expect("failed to run mzpeak-convert");
    assert!(r.status.success(), "exit {:?}; stderr:\n{}", r.status.code(), String::from_utf8_lossy(&r.stderr));
}

/// The declared time unit of `chromatograms_data`, and each chromatogram's stored times.
fn chromatogram_times(archive: &Path, dir: &Path) -> (String, BTreeMap<u64, Vec<f64>>) {
    let mut zip = zip::ZipArchive::new(File::open(archive).unwrap()).unwrap();
    let member = dir.join(format!("{}-chromatograms_data.parquet", archive.file_stem().unwrap().to_string_lossy()));
    std::io::copy(&mut zip.by_name("chromatograms_data.parquet").unwrap(), &mut File::create(&member).unwrap()).unwrap();
    let batches = ParquetRecordBatchReaderBuilder::try_new(File::open(&member).unwrap()).unwrap().build().unwrap();
    let (mut unit, mut times) = (String::new(), BTreeMap::<u64, Vec<f64>>::new());
    for batch in batches {
        let batch = batch.unwrap();
        let point = batch.column_by_name("point").unwrap().as_any().downcast_ref::<StructArray>().unwrap();
        let arrow::datatypes::DataType::Struct(fields) = point.data_type() else { unreachable!() };
        unit = fields.iter().find(|f| f.name() == "time").unwrap().metadata()["unit"].clone();
        let index = point.column_by_name("chromatogram_index").unwrap().as_any().downcast_ref::<UInt64Array>().unwrap();
        let time = point.column_by_name("time").unwrap().as_any().downcast_ref::<Float64Array>().unwrap();
        for r in 0..index.len() {
            times.entry(index.value(r)).or_default().push(time.value(r));
        }
    }
    (unit, times)
}

fn close(a: &[f64], b: &[f64]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| (x - y).abs() <= 1e-9 * y.abs().max(1.0))
}

/// The mzML lane: the column is declared in seconds, from `sic`, so the synthesized TIC and BPC
/// (chromatograms 0 and 1) are stored in seconds too, and `sic` (2) is stored as the source wrote it.
#[test]
fn synthesized_chromatograms_follow_the_declared_unit() {
    let dir = scratch("seconds");
    let archive = dir.join("tiny.mzpeak");
    mzpc(Path::new(TINY), &archive, &[]);
    let (unit, times) = chromatogram_times(&archive, &dir);
    assert_eq!(unit, "UO:0000010", "tiny's chromatograms declare seconds");
    let ms1_seconds = [0.0, 0.7008333333333333 * 60.0, 5.8905 * 60.0];
    for c in [0, 1] {
        assert!(close(&times[&c], &ms1_seconds), "chromatogram {c} is not in seconds: {:?}", times[&c]);
    }
    assert_eq!(times[&2], (0..10).map(f64::from).collect::<Vec<_>>(), "the source's sic, verbatim");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Where no source chromatogram sets it, the column keeps minutes and so does the TIC.
#[test]
fn minutes_stay_minutes() {
    let dir = scratch("minutes");
    let archive = dir.join("centroid_only.mzpeak");
    mzpc(Path::new(CENTROID_ONLY), &archive, &[]);
    let (unit, times) = chromatogram_times(&archive, &dir);
    assert_eq!(unit, "UO:0000031");
    let tic = &times[&0];
    assert!(close(&tic[tic.len() - 1..], &[5.8905]), "TIC is not in minutes: {tic:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// `--rt` is in minutes; on a column declared in seconds it must select the same time span.
#[test]
fn rt_window_is_minutes_on_a_seconds_column() {
    let dir = scratch("rt");
    let archive = dir.join("tiny.mzpeak");
    mzpc(Path::new(TINY), &archive, &[]);
    let filtered = dir.join("rt.mzpeak");
    mzpc(&archive, &filtered, &["--rt", "0-0.05"]); // 0-3 s
    let (_, times) = chromatogram_times(&filtered, &dir);
    let kept: BTreeMap<u64, usize> = times.iter().map(|(c, t)| (*c, t.len())).collect();
    assert_eq!(kept, BTreeMap::from([(0, 1), (1, 1), (2, 4)]), "sic must keep 0, 1, 2 and 3 s: {times:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
