//! Cross-references an mzML source states between its own lists (`src/mzml_refs.rs`): one that names
//! no entry of the source is dropped and declared, one that resolves is carried as stated.
//!
//! Through 0.16.0 every reference passed into the archive unchanged, resolving or not: a pyimzML
//! export's scans (GBM `Test_P15_r2`) were stored naming configuration 1 of a list holding only 0, a
//! MALDIquantForeign export's processing (LA-ESI `Thaliana`) named software the archive did not list
//! — mzdata skips the self-closing `<software/>` the source does state — and the synthetic imzML
//! fixture's spectrum list names processing `dp1`, which it does not have.

use arrow::array::{Array, RecordBatch, UInt32Array};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

const DANGLING: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/dangling_refs.mzML");
const TINY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tiny.pwiz.1.1.mzML");
const SYNTHETIC_IMZML: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/imaging/Synthetic_DeclaredGrid.imzML");
const DROPPED: &str = "mzml:dangling-reference-dropped";

fn scratch(test: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mzpc-dangling-{}-{test}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Convert `input` into `dir/out.mzpeak`; the archive and the run's log.
fn convert(input: &str, dir: &Path) -> (PathBuf, String) {
    let out = dir.join("out.mzpeak");
    let r = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"))
        .arg(input)
        .arg("-o")
        .arg(&out)
        .arg("--force")
        .env_remove("RUST_LOG")
        .output()
        .expect("failed to run mzpeak-convert");
    let log = String::from_utf8_lossy(&r.stderr).into_owned();
    assert!(r.status.success(), "exit {:?}; stderr:\n{log}", r.status.code());
    (out, log)
}

fn member(archive: &Path, name: &str) -> Vec<u8> {
    let mut zip = zip::ZipArchive::new(File::open(archive).unwrap()).unwrap();
    let mut v = Vec::new();
    zip.by_name(name).unwrap_or_else(|_| panic!("{name} missing")).read_to_end(&mut v).unwrap();
    v
}

fn metadata(archive: &Path) -> serde_json::Value {
    serde_json::from_slice::<serde_json::Value>(&member(archive, "mzpeak_index.json")).unwrap()["metadata"].clone()
}

/// The scans' `instrument_configuration_id`, nulls as `None`.
fn scan_configurations(archive: &Path) -> Vec<Option<u32>> {
    let b = ParquetRecordBatchReaderBuilder::try_new(bytes::Bytes::from(member(archive, "spectra_metadata_scans.parquet"))).unwrap();
    let batches: Vec<RecordBatch> = b.build().unwrap().map(Result::unwrap).collect();
    batches
        .iter()
        .flat_map(|t| {
            let c = t.column_by_name("instrument_configuration_id").expect("instrument_configuration_id");
            let c = c.as_any().downcast_ref::<UInt32Array>().expect("a UInt32 column");
            (0..c.len()).map(|i| c.is_valid(i).then(|| c.value(i))).collect::<Vec<_>>()
        })
        .collect()
}

fn strings<'a>(v: &'a serde_json::Value, key: &str) -> Vec<&'a str> {
    v.as_array().unwrap().iter().map(|e| e[key].as_str().unwrap()).collect()
}

/// Each kind is dropped where it names nothing and kept where it resolves; the run's defaults then
/// name the first entry of their lists, as for a source that states none; one warning counts them.
#[test]
fn dangling_references_are_dropped_declared_and_warned_once() {
    let dir = scratch("fixture");
    let (archive, log) = convert(DANGLING, &dir);
    let m = metadata(&archive);

    // scan=1 names IC1 (configuration 0); scan=2's IC9 names nothing and is null, not configuration 2.
    assert_eq!(scan_configurations(&archive), [Some(0), None]);
    let ics = m["instrument_configuration_list"].as_array().unwrap();
    assert_eq!(ics.len(), 1, "{ics:?}");
    assert_eq!(ics[0]["software_reference"], "", "IC1's softwareRef `acquisition` names nothing");

    // The self-closing `exporter` is back in the list, so `export`'s method still names it.
    let software = strings(&m["software_list"], "id");
    assert_eq!(software, ["pwiz", "exporter", "mzpeak-convert"], "{:#}", m["software_list"]);
    assert_eq!(m["software_list"][1]["version"], "0.12");
    let dps = m["data_processing_method_list"].as_array().unwrap();
    let method_software: Vec<(&str, &str)> = dps
        .iter()
        .map(|dp| (dp["id"].as_str().unwrap(), dp["methods"][0]["software_reference"].as_str().unwrap()))
        .collect();
    assert_eq!(
        method_software,
        [("pwiz_conversion", "pwiz"), ("export", "exporter"), ("ghost_processing", ""), ("mzpeak_convert_conversion", "mzpeak-convert")]
    );

    // IC7, dp1 and sf9 name nothing: each default is the first entry of its list instead.
    assert_eq!(m["run"]["default_instrument_id"], 0);
    assert_eq!(m["run"]["default_data_processing_id"], "pwiz_conversion");
    assert_eq!(m["run"]["default_source_file_id"], "sf1");

    let applied = m["transformations"].as_array().unwrap();
    assert!(applied.iter().any(|t| t == DROPPED), "{applied:?}");
    let warnings: Vec<&str> = log.lines().filter(|l| l.contains("dropped references")).collect();
    assert_eq!(warnings.len(), 1, "{log}");
    assert!(
        warnings[0].contains(
            "1 defaultDataProcessingRef (dp1), 1 defaultInstrumentConfigurationRef (configuration 1), \
             1 defaultSourceFileRef (sf9), 1 instrumentConfigurationRef (configuration 2), 2 softwareRef (acquisition, ghost)"
        ),
        "{}",
        warnings[0]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A whole ProteoWizard mzML keeps every reference as stated and declares nothing.
#[test]
fn a_whole_source_keeps_every_reference() {
    let dir = scratch("tiny");
    let (archive, log) = convert(TINY, &dir);
    let m = metadata(&archive);
    assert!(scan_configurations(&archive).iter().all(|c| *c == Some(0)), "{:?}", scan_configurations(&archive));
    assert_eq!(m["run"]["default_data_processing_id"], "pwiz_processing");
    assert_eq!(m["instrument_configuration_list"][0]["software_reference"], "CompassXtract");
    let method_software: Vec<&str> =
        m["data_processing_method_list"].as_array().unwrap().iter().map(|dp| dp["methods"][0]["software_reference"].as_str().unwrap()).collect();
    assert_eq!(method_software, ["CompassXtract", "pwiz", "mzpeak-convert"]);
    assert!(!m["transformations"].as_array().unwrap().iter().any(|t| t == DROPPED));
    assert!(!log.contains("dropped references"), "{log}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The imzML lane too: the synthetic fixture's spectrum list names processing `dp1` and holds no
/// processing list, so the run's default is this conversion's own.
#[test]
fn an_imzml_default_processing_that_names_nothing_is_dropped() {
    let dir = scratch("imzml");
    let (archive, _) = convert(SYNTHETIC_IMZML, &dir);
    let m = metadata(&archive);
    assert_eq!(m["run"]["default_data_processing_id"], "mzpeak_convert_conversion");
    assert!(m["transformations"].as_array().unwrap().iter().any(|t| t == DROPPED), "{:#}", m["transformations"]);
    let _ = std::fs::remove_dir_all(&dir);
}
