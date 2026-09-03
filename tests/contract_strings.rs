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

/// `src/main.rs` with CRLF folded to LF.
///
/// Windows checks this repo out with `core.autocrlf=true`, so `include_str!` hands back `\r\n`
/// and any needle containing a bare `\n` — `pinned("TOF_C0_CURIE,\n    \"tof_c0\",")` — silently
/// never matches. That made this suite RED on the Windows box and green on macOS, which is the
/// platform where the `#[cfg(windows)]` code these pins guard is not even compiled. Normalize once
/// so a pin means the same thing on both.
fn src() -> &'static str {
    static NORM: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    NORM.get_or_init(|| SRC.replace("\r\n", "\n"))
}

fn pinned(needle: &str) {
    assert!(
        src().contains(needle),
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

/// Every `codec: "tof-grid"` block must name its integer axis and say whether m/z survives the
/// round trip — with the SAME keys across all three models, so a reader needs one code path.
///
/// History this pins, and it is worth reading before touching these strings. `lossless` was read as
/// a claim that m/z reconstruction is exact, judged self-contradictory beside a 4.99 ppm bound in
/// the same block, and renamed to `integer_column`. That was a MISREADING: the spec defines
/// `lossless` as "name of the exactly-preserved stored column", and `tof_index` is exactly that —
/// the integer we store and read back bit-for-bit. The rename broke no runtime consumer, but it
/// diverged from the spec's schema and from the 11 published archives carrying the key, to fix a
/// contradiction that was never there. It is restored; the genuinely new information — whether the
/// m/z you REBUILD from that column is exact — lives in `mz_reconstruction` beside it.
///
/// The count is FOUR, not three. A fourth emission site (the Shimadzu profile lane) shipped with
/// NEITHER key while sharing its `model` string with the per-spectrum SCIEX lane, so a reader
/// keying off the model got one answer there and null here. It stayed invisible because this pin
/// asserted three. Hence: assert the number of sites, not merely the presence of a string.
#[test]
fn tof_grid_reconstruction_keys_pinned() {
    // One per `codec: "tof-grid"` emission site. Counted against the sites themselves so that
    // adding a fifth lane without its keys fails here rather than in someone's reader.
    let sites = src().matches("\"codec\": \"tof-grid\"").count();
    assert_eq!(sites, 4, "expected 4 `codec: \"tof-grid\"` emission sites, found {sites}");
    assert_eq!(
        src().matches("\"lossless\": \"tof_index\"").count(),
        sites,
        "every `codec: \"tof-grid\"` block must name its exactly-stored column with the spec's \
         `lossless` key; found {} of {sites} emission sites",
        src().matches("\"lossless\": \"tof_index\"").count()
    );
    assert!(
        !src().contains("integer_column"),
        "`integer_column` is a synonym for the spec's `lossless` and was reverted; two keys naming \
         the same column is how they drift apart"
    );
    // The two honest values of `mz_reconstruction`, and the bound that must accompany the lossy one.
    pinned("\"mz_reconstruction\": \"exact\"");
    assert_eq!(
        src().matches("\"mz_reconstruction\": \"bounded-lossy\"").count(),
        2,
        "the run-wide and per-spectrum SCIEX grid lanes are bounded-lossy and must say so \
         (the Agilent and Shimadzu lanes are exact)"
    );
    assert_eq!(
        src().matches("\"roundtrip_tolerance_ppm\": tof_grid::ppm_tol()").count(),
        2,
        "a bounded-lossy block must state its bound"
    );
    // `lossless` means the same thing in every block that carries it — the name of the exactly
    // stored column — so the `mz-grid` lattice (src/mz_lattice.rs) and `ims-compact` blocks spell
    // it identically. One archive can carry it twice, once per facet, without ambiguity.
    assert!(
        src().contains("\"lossless\": \"tof\""),
        "the ims-compact block names its exactly-stored integer column the same way"
    );
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
