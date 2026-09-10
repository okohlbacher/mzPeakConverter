//! Pins for what the native lanes state about a run (the `run_metadata` seam) and for the
//! precursor rows the Bruker TSF lane now writes.
//!
//! * `target_only_window.mzML` — tiny.pwiz with every isolation window reduced to its target: the
//!   archive must keep the target and NULL offsets. Before the fix the writer turned "width unknown"
//!   into offsets of ±target (measured on RS080806, Minimal_DDA, En_PPY in the corpus). mzdata's mzML
//!   writer did the same on `--to mzml`, so the export must be target-only too.
//! * A Bruker TSF `.d` (set `MZPC_TSF_FIXTURE=/path/to/x.d` and run with `--include-ignored`; the corpus holds no TSF
//!   acquisition, and `bruker_tsf`'s unit tests pin the FrameMsMsInfo mapping without one): every MS2 frame
//!   gets its `FrameMsMsInfo` precursor with a resolved parent, the stated charges are carried, and
//!   the run block carries the zoned start time, serial, model, acquisition software, sample name
//!   and the two digested members — what the mzML lane inherits from ProteoWizard, read natively.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

#[path = "common/corpus.rs"]
mod corpus;

fn convert(input: &Path, tag: &str, extra: &[&str]) -> PathBuf {
    let out = std::env::temp_dir().join(format!("mzpc-runmeta-{}-{tag}.mzpeak", std::process::id()));
    let _ = std::fs::remove_file(&out);
    let status = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"))
        .arg(input)
        .args(extra)
        .arg("-o")
        .arg(&out)
        .arg("--force")
        .status()
        .expect("failed to run mzpeak-convert");
    assert!(status.success(), "conversion of {} failed: {status}", input.display());
    out
}

fn member(archive: &Path, name: &str) -> Vec<u8> {
    let mut zip = zip::ZipArchive::new(File::open(archive).unwrap()).unwrap();
    let mut f = zip.by_name(name).unwrap_or_else(|_| panic!("{name} missing"));
    let mut v = Vec::new();
    f.read_to_end(&mut v).unwrap();
    v
}

fn table(archive: &Path, name: &str) -> arrow::array::RecordBatch {
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    let bytes = bytes::Bytes::from(member(archive, name));
    let reader = ParquetRecordBatchReaderBuilder::try_new(bytes).unwrap().with_batch_size(1 << 20).build().unwrap();
    let batches: Vec<_> = reader.map(|b| b.unwrap()).collect();
    arrow::compute::concat_batches(&batches[0].schema(), &batches).unwrap()
}

fn index(archive: &Path) -> serde_json::Value {
    serde_json::from_slice(&member(archive, "mzpeak_index.json")).unwrap()
}

fn struct_field<'a>(b: &'a arrow::array::RecordBatch, col: &str, field: &str) -> arrow::array::ArrayRef {
    use arrow::array::Array;
    let s = b.column_by_name(col).unwrap_or_else(|| panic!("no column {col}"));
    let s = s.as_any().downcast_ref::<arrow::array::StructArray>().unwrap_or_else(|| panic!("{col} is not a struct"));
    s.column_by_name(field).unwrap_or_else(|| panic!("no {col}.{field}")).clone()
}

#[test]
fn a_target_only_isolation_window_keeps_null_offsets() {
    use arrow::array::Array;
    let fixture = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/target_only_window.mzML"));
    let archive = convert(&fixture, "tow", &[]);
    let prec = table(&archive, "spectra_metadata_precursors.parquet");
    assert_eq!(prec.num_rows(), 1);
    let target = struct_field(&prec, "isolation_window", "isolation_window_target");
    let lower = struct_field(&prec, "isolation_window", "isolation_window_lower_offset");
    let upper = struct_field(&prec, "isolation_window", "isolation_window_upper_offset");
    assert!(!target.is_null(0), "the stated target is kept");
    assert!(lower.is_null(0) && upper.is_null(0), "an unstated width stays unknown — not ±target");
    let _ = std::fs::remove_file(&archive);
}

/// The same window through `--to mzml`. mzdata's writer printed it as lower offset 445.3 and upper
/// offset −445.3, a window from 0 to twice the target, until the export sink blanked that pair in
/// place (`src/mzml_isolation.rs`); in place means every `<indexList>` offset still finds its element.
#[test]
fn a_target_only_isolation_window_exports_to_mzml_target_only() {
    let fixture = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/target_only_window.mzML"));
    let mzml = std::env::temp_dir().join(format!("mzpc-runmeta-{}-tow.mzML", std::process::id()));
    let _ = std::fs::remove_file(&mzml);
    let status = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"))
        .arg(&fixture)
        .arg("-o")
        .arg(&mzml)
        .arg("--force")
        .status()
        .expect("failed to run mzpeak-convert");
    assert!(status.success(), "mzML export of {} failed: {status}", fixture.display());
    let xml = std::fs::read_to_string(&mzml).unwrap();
    let window = &xml[xml.find("<isolationWindow>").unwrap()..xml.find("</isolationWindow>").unwrap()];
    assert!(window.contains("\"MS:1000827\""), "the stated target is kept: {window}");
    assert!(
        !window.contains("\"MS:1000828\"") && !window.contains("\"MS:1000829\""),
        "an unstated width is not exported as offsets of ±target: {window}"
    );
    let index = &xml[xml.find("<indexList").unwrap()..];
    let offsets: Vec<usize> = index
        .split("<offset ")
        .skip(1)
        .map(|o| o[o.find('>').unwrap() + 1..o.find("</offset>").unwrap()].parse().unwrap())
        .collect();
    assert!(!offsets.is_empty(), "an indexed export");
    for at in offsets {
        let element = xml[at..].trim_start();
        assert!(
            element.starts_with("<spectrum ") || element.starts_with("<chromatogram "),
            "offset {at} no longer points at its element"
        );
    }
    let _ = std::fs::remove_file(&mzml);
}

#[test]
#[ignore = "needs a Bruker TSF .d via MZPC_TSF_FIXTURE; the corpus holds no TSF acquisition"]
fn tsf_frames_carry_their_frame_msms_info_precursors_and_the_run_block() {
    use arrow::array::{Array, AsArray};
    let Some(dot_d) = corpus::env_path("MZPC_TSF_FIXTURE") else { return };
    let dot_d = dot_d.as_path();
    let nonempty = |name: &str| std::fs::metadata(dot_d.join(name)).is_ok_and(|m| m.len() > 0);
    // A TDF run with an empty analysis.tsf beside it once passed for a TSF fixture: refuse it.
    assert!(nonempty("analysis.tsf") && !nonempty("analysis.tdf"),
        "MZPC_TSF_FIXTURE={} is not a TSF acquisition (needs a non-empty analysis.tsf and no analysis.tdf)", dot_d.display());
    let archive = convert(dot_d, "tsf", &["--no-vendor"]);

    // Every MS2 frame has exactly one precursor whose parent resolved to a spectrum index.
    let meta = table(&archive, "spectra_metadata.parquet");
    let ms_level = meta.column_by_name("ms_level").unwrap();
    let ms2 = (0..meta.num_rows()).filter(|&i| !ms_level.is_null(i) && ms_level.as_primitive::<arrow::datatypes::UInt8Type>().value(i) == 2).count();
    let prec = table(&archive, "spectra_metadata_precursors.parquet");
    assert_eq!(prec.num_rows(), ms2, "one precursor row per MS2 frame");
    assert!(ms2 > 0, "the fixture has MS2 frames");
    let parent = prec.column_by_name("precursor_index").unwrap();
    assert_eq!(parent.null_count(), 0, "every FrameMsMsInfo.Parent resolved through precursor_id");
    let lower = struct_field(&prec, "isolation_window", "isolation_window_lower_offset");
    assert_eq!(lower.null_count(), 0, "TSF states an isolation width on every row");

    // Stated charges are carried verbatim; unstated ones stay null (never invented).
    let ions = table(&archive, "spectra_metadata_selected_ions.parquet");
    let charge = ions.column_by_name("charge_state").unwrap();
    assert!(charge.null_count() < ions.num_rows(), "some charges are stated");
    assert!(charge.null_count() > 0, "and the unstated ones are not invented");

    // The run block, read natively from GlobalMetadata.
    let idx = index(&archive);
    let m = &idx["metadata"];
    assert!(m["run"]["start_time"].as_str().is_some_and(|t| t.contains('+') || t.ends_with('Z')), "zoned start time: {}", m["run"]);
    let inst = &m["instrument_configuration_list"][0]["parameters"];
    let accs: Vec<&str> = inst.as_array().unwrap().iter().filter_map(|p| p["accession"].as_str()).collect();
    assert!(accs.contains(&"MS:1000529"), "serial: {accs:?}");
    assert!(accs.contains(&"MS:1000031"), "model: {accs:?}");
    let sw: Vec<&str> = m["software_list"].as_array().unwrap().iter().filter_map(|s| s["id"].as_str()).collect();
    assert!(sw.iter().any(|s| *s != "mzpeak-convert"), "acquisition software recorded: {sw:?}");
    assert_eq!(m["sample_list"].as_array().map(|a| a.len()), Some(1), "sample name from SampleName");
    let sources = m["file_description"]["source_files"].as_array().unwrap();
    let names: Vec<&str> = sources.iter().filter_map(|s| s["name"].as_str()).collect();
    assert!(names.iter().any(|n| n.eq_ignore_ascii_case("analysis.tsf")) && names.iter().any(|n| n.eq_ignore_ascii_case("analysis.tsf_bin")), "{names:?}");
    assert!(names.iter().all(|n| !n.ends_with(".d")), "no synthesised directory entry beside the members: {names:?}");
    assert!(sources.iter().all(|s| s["parameters"].as_array().unwrap().iter().any(|p| p["accession"] == "MS:1000569")), "every member digested");
    let contents: Vec<&str> = m["file_description"]["contents"].as_array().unwrap().iter().filter_map(|p| p["accession"].as_str()).collect();
    assert!(contents.contains(&"MS:1000579") && contents.contains(&"MS:1000580") && !contents.contains(&"MS:1000294"), "{contents:?}");
    let _ = std::fs::remove_file(&archive);
}
