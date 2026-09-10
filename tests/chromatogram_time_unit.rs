//! Every chromatogram time is stored in minutes, the unit `chromatograms_data` declares on every lane.
//!
//! The spec leaves a chromatogram's time unit to the writer and recommends minutes
//! (`docs/schemas/chromatograms.md`); spectrum and wavelength times are minutes by rule, and
//! mzPeakViewer reads every stored chromatogram time as minutes without looking at the declared
//! unit. The mzML lane used to sample the column from the source's chromatograms, which ProteoWizard
//! writes in seconds (`UO:0000010`): 0.11.5 then stored the synthesized TIC and BPC in minutes under
//! that label, and storing them in seconds instead made the viewer draw a 5.9-minute run as 5.9 hours.
//! A source chromatogram in seconds or milliseconds is now converted to minutes before the column is
//! sampled, and the archive declares `chromatogram-time-to-minutes`. `--rt`, a window in minutes,
//! still reads each column's declared unit, so an archive with a seconds column (an mzML-lane
//! archive built by 0.11.5 or earlier) filters correctly: `tests::an_rt_window_reads_a_seconds_column_in_its_unit`
//! in `src/main.rs` builds one with the writer.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;

use arrow::array::{Array, Float64Array, StructArray, UInt64Array};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

const TINY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tiny.pwiz.1.1.mzML");
/// Non-indexed, so its chromatograms are not read: only the synthesized TIC and BPC, from minutes.
const CENTROID_ONLY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tiny_centroid_only.mzML");
const TO_MINUTES: &str = "chromatogram-time-to-minutes";

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

/// The archive's `transformations` entries.
fn transformations(archive: &Path) -> Vec<String> {
    let mut zip = zip::ZipArchive::new(File::open(archive).unwrap()).unwrap();
    let index: serde_json::Value = serde_json::from_reader(zip.by_name("mzpeak_index.json").unwrap()).unwrap();
    index["metadata"]["transformations"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect()
}

fn close(a: &[f64], b: &[f64]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| (x - y).abs() <= 1e-9 * y.abs().max(1.0))
}

/// The mzML lane: `tiny.pwiz.1.1.mzML`'s `sic` (chromatogram 2) is in seconds, so it is stored
/// divided by 60, the column declares minutes, the synthesized TIC and BPC (0 and 1) keep the
/// spectrum start times they are built from, and the conversion is declared.
#[test]
fn the_mzml_lane_stores_chromatogram_times_in_minutes() {
    let dir = scratch("mzml-lane");
    let archive = dir.join("tiny.mzpeak");
    mzpc(Path::new(TINY), &archive, &[]);
    let (unit, times) = chromatogram_times(&archive, &dir);
    assert_eq!(unit, "UO:0000031", "the column declares minutes, whatever unit the source's chromatograms state");
    let ms1_minutes = [0.0, 0.7008333333333333, 5.8905];
    for c in [0, 1] {
        assert!(close(&times[&c], &ms1_minutes), "chromatogram {c} is not in minutes: {:?}", times[&c]);
    }
    let sic_minutes: Vec<f64> = (0..10).map(|s| f64::from(s) / 60.0).collect();
    assert!(close(&times[&2], &sic_minutes), "the source's sic, 0-9 s, in minutes: {:?}", times[&2]);
    assert!(transformations(&archive).iter().any(|t| t == TO_MINUTES), "{:?}", transformations(&archive));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Where no source chromatogram is read, nothing is converted and nothing is declared.
#[test]
fn minutes_stay_minutes() {
    let dir = scratch("minutes");
    let archive = dir.join("centroid_only.mzpeak");
    mzpc(Path::new(CENTROID_ONLY), &archive, &[]);
    let (unit, times) = chromatogram_times(&archive, &dir);
    assert_eq!(unit, "UO:0000031");
    let tic = &times[&0];
    assert!(close(&tic[tic.len() - 1..], &[5.8905]), "TIC is not in minutes: {tic:?}");
    assert!(!transformations(&archive).iter().any(|t| t == TO_MINUTES), "{:?}", transformations(&archive));
    let _ = std::fs::remove_dir_all(&dir);
}

/// `--rt` is in minutes, and so is every column the converter writes now: `--rt 0-0.05` keeps the
/// source's `sic` up to 3 s.
#[test]
fn rt_window_is_minutes() {
    let dir = scratch("rt");
    let archive = dir.join("tiny.mzpeak");
    mzpc(Path::new(TINY), &archive, &[]);
    let filtered = dir.join("rt.mzpeak");
    mzpc(&archive, &filtered, &["--rt", "0-0.05"]);
    let (unit, times) = chromatogram_times(&filtered, &dir);
    assert_eq!(unit, "UO:0000031");
    let kept: BTreeMap<u64, usize> = times.iter().map(|(c, t)| (*c, t.len())).collect();
    assert_eq!(kept, BTreeMap::from([(0, 1), (1, 1), (2, 4)]), "sic must keep 0, 1, 2 and 3 s: {times:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
