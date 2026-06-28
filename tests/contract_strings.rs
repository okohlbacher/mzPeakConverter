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
    pinned("\"tof_encoding\": \"absolute\"");
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
