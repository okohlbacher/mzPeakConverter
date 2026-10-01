//! Cross-references an mzML source states between its own lists (`src/mzml_refs.rs`): one that names
//! no entry of the source is dropped and declared, one that resolves is carried as stated.
//!
//! Through 0.16.0 every reference passed into the archive unchanged, resolving or not: a pyimzML
//! export's scans (GBM `Test_P15_r2`) were stored naming configuration 1 of a list holding only 0, a
//! MALDIquantForeign export's processing (LA-ESI `Thaliana`) named software the archive did not list
//! — mzdata skips the self-closing `<software/>` the source does state — and the synthetic imzML
//! fixture's spectrum list names processing `dp1`, which it does not have. mzdata skips a
//! self-closing `<sourceFile/>` and `<instrumentConfiguration/>` the same way; a reference to one is
//! whole and is kept.

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
fn convert(input: impl AsRef<Path>, dir: &Path) -> (PathBuf, String) {
    convert_with(input, dir, &[])
}

fn convert_with(input: impl AsRef<Path>, dir: &Path, args: &[&str]) -> (PathBuf, String) {
    let out = dir.join("out.mzpeak");
    let r = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"))
        .arg(input.as_ref())
        .arg("-o")
        .arg(&out)
        .arg("--force")
        .args(args)
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
/// name the first entry of their lists, as for a source that states none; one warning counts them,
/// naming each id as the source states it.
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
            "1 defaultDataProcessingRef (dp1), 1 defaultInstrumentConfigurationRef (IC7), \
             1 defaultSourceFileRef (sf9), 1 instrumentConfigurationRef (IC9), 2 softwareRef (acquisition, ghost)"
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

/// The fixture made whole: every reference resolves, and two of them name entries mzdata skips for
/// being self-closing — the run's default source file `sf9` and scan=2's configuration `IC2`. Both
/// are put back and kept; nothing is dropped. Through the first cut of the check, both were dropped
/// and declared, and the default re-pointed at `sf1`, a different file.
#[test]
fn references_to_self_closing_entries_are_kept() {
    let dir = scratch("selfclosing");
    let fixture = std::fs::read_to_string(DANGLING).unwrap();
    let mut whole = fixture.clone();
    for (from, to) in [
        (r#"<softwareRef ref="acquisition"/>"#, r#"<softwareRef ref="pwiz"/>"#),
        (r#"softwareRef="ghost""#, r#"softwareRef="pwiz""#),
        (r#"defaultInstrumentConfigurationRef="IC7""#, r#"defaultInstrumentConfigurationRef="IC1""#),
        (r#"defaultDataProcessingRef="dp1""#, r#"defaultDataProcessingRef="pwiz_conversion""#),
        (r#"instrumentConfigurationRef="IC9""#, r#"instrumentConfigurationRef="IC2""#),
        (
            "    </instrumentConfiguration>\n  </instrumentConfigurationList>",
            "    </instrumentConfiguration>\n    <instrumentConfiguration id=\"IC2\"/>\n  </instrumentConfigurationList>",
        ),
        (
            "      </sourceFile>\n    </sourceFileList>",
            "      </sourceFile>\n      <sourceFile id=\"sf9\" name=\"other.raw\" location=\"file:///data\"/>\n    </sourceFileList>",
        ),
    ] {
        assert!(whole.contains(from), "{from}");
        whole = whole.replacen(from, to, 1);
    }
    let src = dir.join("self_closing.mzML");
    std::fs::write(&src, whole).unwrap();
    let (archive, log) = convert(&src, &dir);
    let m = metadata(&archive);

    assert_eq!(scan_configurations(&archive), [Some(0), Some(1)], "scan=2 names IC2, put back as configuration 1");
    let mut ics: Vec<u64> = m["instrument_configuration_list"].as_array().unwrap().iter().map(|ic| ic["id"].as_u64().unwrap()).collect();
    ics.sort();
    assert_eq!(ics, [0, 1], "{:#}", m["instrument_configuration_list"]);
    let files = m["file_description"]["source_files"].as_array().unwrap();
    let files: Vec<(&str, &str)> = files.iter().map(|f| (f["id"].as_str().unwrap(), f["name"].as_str().unwrap())).collect();
    assert_eq!(files, [("sf1", "dangling_refs.raw"), ("sf9", "other.raw")]);
    assert_eq!(m["run"]["default_source_file_id"], "sf9", "the default names the file the source names");
    assert_eq!(m["run"]["default_instrument_id"], 0);
    assert!(!m["transformations"].as_array().unwrap().iter().any(|t| t == DROPPED), "{:#}", m["transformations"]);
    assert!(!log.contains("dropped references"), "{log}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The `--tof-grid` lane runs the same check on its own copy of the metadata and the scans: the
/// SWATH fixture (whose fit is accepted) with spectrum 0's scan naming `IC_gone`, which nothing
/// states, and spectrum 1's naming `IC2`, a self-closing entry of its list. The first is null and
/// declared next to the lane's own entry, the second kept and put back. Through the first cut of the
/// check, no test ran this lane.
#[test]
fn the_tof_grid_lane_checks_the_scans_too() {
    use std::io::Read as _;
    let dir = scratch("tofgrid");
    let mut text = String::new();
    let gz = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/swath.api-sample-centroid.mzML.gz");
    flate2::read::GzDecoder::new(File::open(gz).unwrap()).read_to_string(&mut text).unwrap();
    assert!(!text.contains("<indexList"), "the edit shifts offsets: the fixture must be unindexed");
    let list_end = "    </instrumentConfiguration>\n  </instrumentConfigurationList>";
    assert!(text.contains(list_end));
    let text = text
        .replacen(list_end, "    </instrumentConfiguration>\n    <instrumentConfiguration id=\"IC2\"/>\n  </instrumentConfigurationList>", 1)
        .replacen("<scan>", r#"<scan instrumentConfigurationRef="IC_gone">"#, 1)
        .replacen("<scan>", r#"<scan instrumentConfigurationRef="IC2">"#, 1);
    let src = dir.join("swath_refs.mzML");
    std::fs::write(&src, text).unwrap();
    let (archive, log) = convert_with(&src, &dir, &["--tof-grid", "on"]);
    let m = metadata(&archive);

    let applied: Vec<&str> = m["transformations"].as_array().unwrap().iter().map(|t| t.as_str().unwrap()).collect();
    assert!(applied.iter().any(|t| t.starts_with("tof-grid:")), "the TOF-grid lane wrote this archive: {applied:?}");
    assert!(applied.contains(&DROPPED), "{applied:?}");
    let scans = scan_configurations(&archive);
    assert_eq!(scans.len(), 201);
    assert_eq!(scans[0], None, "IC_gone names nothing");
    assert!(scans[2..].iter().all(|c| *c == Some(0)), "a scan naming none is the run's IC1");
    // IC2's number is the one mzdata gave it on first sight, which depends on the order it read the
    // spectra in (the lane samples some first): not 0, and listed.
    let ics: Vec<u64> = m["instrument_configuration_list"].as_array().unwrap().iter().map(|ic| ic["id"].as_u64().unwrap()).collect();
    assert_eq!(ics.len(), 2, "{:#}", m["instrument_configuration_list"]);
    let ic2 = scans[1].expect("IC2 is kept") as u64;
    assert!(ic2 != 0 && ics.contains(&ic2), "{ic2} in {ics:?}");
    let warnings: Vec<&str> = log.lines().filter(|l| l.contains("dropped references")).collect();
    assert_eq!(warnings.len(), 1, "{log}");
    assert!(warnings[0].contains("1 instrumentConfigurationRef (IC_gone)"), "{}", warnings[0]);
    let _ = std::fs::remove_dir_all(&dir);
}
