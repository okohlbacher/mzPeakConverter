//! Fixture-free pin of the calibration-block contract strings mzPeakViewer matches reconstruction on.
//!
//! mzPeakViewer keys TOF→m/z reconstruction off the `model` field for all encodings, AND — for
//! SciEX `sciex_sqrt_per_spectrum` specifically — ALSO matches the exact `tof_to_mz` formula string
//! (whitespace-tolerant). A silent reformat of any of these would make the viewer fail-loud and
//! render empty spectra. These tests assert the literals are still emitted VERBATIM in the converter
//! source, with no corpus fixture needed (we read `src/main.rs` at compile time and search it — the
//! needle appearing in this test file is irrelevant, since we search main.rs's content, not ours).
//!
//! Changing any pinned string is a BREAKING contract change: bump the version and notify the viewer
//! team. See the calibration emission sites in `src/main.rs` (ims_calibration / tof_calibration).

const SRC: &str = include_str!("../src/main.rs");

fn pinned(needle: &str) {
    assert!(
        SRC.contains(needle),
        "calibration contract drift: `{needle}` is no longer emitted verbatim in src/main.rs \
         — this is a BREAKING change for mzPeakViewer (fail-loud -> empty spectra). \
         If intentional, update this pin + bump the version + tell the viewer team."
    );
}

#[test]
fn ims_compact_calibration_pinned() {
    pinned("\"codec\": \"ims-compact\"");
    pinned("\"mz_from_tof\": \"(a + b*tof)^2\"");
    // `tof_encoding` is emitted from a variable with two TRUTHFUL values the viewer must accept
    // verbatim: "absolute" (archive layout + SDK path) and "m/z-chunked" (--ims-chunked). The third,
    // "per-scan-delta", was REMOVED in v0.7.3 — no reader ever cumsummed it, so it produced wrong
    // m/z. Do not re-add the pin: the label must not reappear in emitted output.
    pinned("\"absolute\"");
    pinned("\"m/z-chunked\"");
}

#[test]
fn sciex_per_spectrum_tof_grid_pinned() {
    // The SciEX encoding actually present across the corpus. The viewer matches BOTH the model
    // string AND this exact tof_to_mz formula, so both are load-bearing.
    pinned("\"model\": \"sciex_sqrt_per_spectrum\"");
    pinned("\"tof_to_mz\": \"mz = (tof_c0 + tof_c1*tof_index)^2\"");
    pinned("\"per_spectrum_columns\": [\"tof_c0\", \"tof_c1\"]");
}

#[test]
fn agilent_and_sciex_global_models_pinned() {
    pinned("\"model\": \"agilent_sqrt_poly\"");
    // The global-coefficient mzML `--tof-grid` path (distinct from the per-spectrum SciEX encoding).
    pinned("\"model\": \"sciex_sqrt\"");
}

#[test]
fn ims_compact_per_spectrum_exact_pinned() {
    // The timsTOF exact lane (MzCalibration ModelType 1, C2 = 0): the viewer keys the per-spectrum
    // pair off `ims_calibration.per_spectrum == "tof_c0,tof_c1"` (resolveImsCalibration) and reads
    // the cells by the `_tof_c0` / `_tof_c1` column-name SUFFIX — the accession prefix is allowed to
    // drift, the suffix is not. Both halves of that contract live in main.rs: the index-block keys and
    // the spectra_metadata column specs (`from_spec(TOF_C0_CURIE, "tof_c0", …)` → `opt_MS_4000900_tof_c0`).
    pinned("cal[\"per_spectrum\"] = serde_json::json!(\"tof_c0,tof_c1\")");
    pinned("cal[\"exact_per_spectrum\"] = serde_json::json!(true)");
    pinned("cal[\"per_spectrum_chord_frames\"]");
    pinned("\"mzpeak:transform_params_per_spectrum\".to_string(), \"tof_c0,tof_c1\".to_string()");
    pinned("TOF_C0_CURIE,\n                \"tof_c0\",");
    pinned("TOF_C1_CURIE,\n                \"tof_c1\",");
    // The accessions behind the `opt_MS_4000900_tof_c0` / `opt_MS_4000901_tof_c1` column names.
    pinned("ControlledVocabulary::MS, 4_000_900)");
    pinned("ControlledVocabulary::MS, 4_000_901)");
}
