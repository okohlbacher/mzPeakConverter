//! Regression pin for the shape of `scan_settings_list[*].targets` in the archive metadata.
//!
//! Before 0.9.2 each target was written as a bare JSON list of params, which the mzPeak 0.9
//! schema (`scan_settings_list.json`, definition `target`) rejects: a target must be an object
//! `{"parameters": [...]}`. The tiny pwiz fixture declares a `<scanSettings>` with two targets.

use std::fs::File;
use std::path::PathBuf;
use std::process::Command;

fn convert_fixture() -> PathBuf {
    let out = std::env::temp_dir().join(format!("mzpc-targets-{}.mzpeak", std::process::id()));
    let _ = std::fs::remove_file(&out);
    let status = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"))
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tiny.pwiz.1.1.mzML"))
        .arg("-o")
        .arg(&out)
        .arg("--force")
        .status()
        .expect("failed to run mzpeak-convert");
    assert!(status.success(), "conversion failed: {status}");
    out
}

#[test]
fn scan_settings_targets_are_objects() {
    let archive = convert_fixture();
    let mut zip = zip::ZipArchive::new(File::open(&archive).unwrap()).unwrap();
    let index: serde_json::Value =
        serde_json::from_reader(zip.by_name("mzpeak_index.json").unwrap()).unwrap();
    let targets = &index["metadata"]["scan_settings_list"][0]["targets"];
    let targets = targets.as_array().expect("scan_settings_list[0].targets is not an array");
    assert_eq!(targets.len(), 2, "fixture no longer declares two <target>s");
    for (i, t) in targets.iter().enumerate() {
        assert!(t.is_object(), "targets[{i}] is not an object: {t}");
        let params = t["parameters"].as_array().unwrap_or_else(|| panic!("targets[{i}] lacks a 'parameters' list: {t}"));
        assert!(!params.is_empty(), "targets[{i}].parameters is empty");
    }
    // The schema's key (review 2026-09-30): the fixture's `<sourceFileRef ref="sf_parameters"/>`
    // lands under `source_file_references`, and the field name the struct used is gone.
    let settings = &index["metadata"]["scan_settings_list"][0];
    assert_eq!(settings["source_file_references"], serde_json::json!(["sf_parameters"]), "{settings}");
    assert!(settings.get("source_file_refs").is_none(), "{settings}");
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn scan_settings_read_both_source_file_keys_and_neither() {
    let read = |v: serde_json::Value| -> Vec<mzpeak_prototyping::param::ScanSettings> { serde_json::from_value(v).unwrap() };
    // The schema's key, the one archives through 0.16.0 carry, and a spec archive that states only
    // the required `id` and `parameters`.
    let spec = read(serde_json::json!([{"id": "s1", "source_file_references": ["sf1"], "parameters": []}]));
    let old = read(serde_json::json!([{"id": "s1", "source_file_refs": ["sf1"], "targets": [], "parameters": []}]));
    let bare = read(serde_json::json!([{"id": "s1", "parameters": []}]));
    assert_eq!(spec[0].source_file_refs, ["sf1"]);
    assert_eq!(old[0].source_file_refs, ["sf1"]);
    assert!(bare[0].source_file_refs.is_empty() && bare[0].targets.is_empty());
}

#[test]
fn legacy_bare_list_targets_still_deserialize() {
    // Archives written by 0.9.0/0.9.1 stored each target as a bare param list.
    let legacy = serde_json::json!([{
        "id": "s1",
        "source_file_refs": [],
        "targets": [[{"name": "selected ion m/z", "accession": "MS:1000744", "value": 1000.0, "unit": "MS:1000040"}]],
        "parameters": []
    }]);
    let parsed: Vec<mzpeak_prototyping::param::ScanSettings> = serde_json::from_value(legacy).unwrap();
    assert_eq!(parsed[0].targets.len(), 1);
    assert_eq!(parsed[0].targets[0].parameters.len(), 1);
    // And the fixed form round-trips through the same type.
    let fixed = serde_json::to_value(&parsed).unwrap();
    assert!(fixed[0]["targets"][0].is_object(), "{fixed}");
    let again: Vec<mzpeak_prototyping::param::ScanSettings> = serde_json::from_value(fixed).unwrap();
    assert_eq!(again[0].targets[0].parameters.len(), 1);
}
