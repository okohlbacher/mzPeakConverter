//! Fixture-free pin of the calibration-block contract strings mzPeakViewer matches reconstruction on.
//!
//! mzPeakViewer keys TOF→m/z reconstruction off the `model` field for all encodings, AND — for
//! SciEX `sciex_sqrt_per_spectrum` specifically — ALSO matches the exact `tof_to_mz` formula string
//! (whitespace-tolerant). A silent reformat of any of these would make the viewer fail-loud and
//! render empty spectra. These tests assert the literals are still emitted VERBATIM in the converter
//! source, with no corpus fixture needed (we read `src/main.rs` at compile time and search its CODE:
//! comments and the `#[cfg(test)]` module are cut away first, so neither a comment quoting a literal
//! nor a unit test asserting on it can stand in for the emission site — see [`code`]).
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

/// The converter's CODE: `src/main.rs` up to its `#[cfg(test)] mod tests`, every `//` comment cut.
/// A pin exists to go red when the emission site goes away, and both stand-ins kept pins green: the
/// `tof_encoding` values are also quoted in a comment beside the call, and `"global_metadata"` in
/// the test module.
fn code() -> &'static str {
    static CODE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    CODE.get_or_init(|| {
        let lanes = src().split("\n#[cfg(test)]\nmod tests {").next().unwrap();
        lanes.lines().map(strip_comment).collect::<Vec<_>>().join("\n")
    })
}

/// `line` without its `//` comment. A `//` inside a string literal (`"file://"`) is not one; quote
/// state is per line (a `'"'` char literal does not open a string).
fn strip_comment(line: &str) -> &str {
    let b = line.as_bytes();
    let (mut in_str, mut i) = (false, 0);
    while i < b.len() {
        match b[i] {
            b'\\' if in_str => i += 1,
            b'\'' if !in_str && b.get(i + 1) == Some(&b'"') && b.get(i + 2) == Some(&b'\'') => i += 2,
            b'"' => in_str = !in_str,
            b'/' if !in_str && b.get(i + 1) == Some(&b'/') => return line[..i].trim_end(),
            _ => {}
        }
        i += 1;
    }
    line
}

#[test]
fn code_view_drops_comments_and_the_test_module() {
    assert_eq!(strip_comment(r#"    x("file://"); // "absolute""#), r#"    x("file://");"#);
    assert_eq!(strip_comment(r#"    // `tof_encoding` is "absolute""#), "");
    assert_eq!(strip_comment(r#"    s.split('"').next() // "m/z-chunked""#), r#"    s.split('"').next()"#);
    assert!(code().contains("fn main()"), "the lanes survive");
    // `mod tests {` occurs in main.rs only where the test module opens (fn corpus_root(), the old
    // needle, moved to tests/common/corpus.rs, so it could no longer fail).
    assert!(!code().contains("mod tests {"), "the test module is cut");
}

fn pinned(needle: &str) {
    assert!(
        code().contains(needle),
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
    pinned("\"tof_encoding\": tof_encoding");
    pinned("\"absolute\"");
    pinned("\"m/z-chunked\"");
    // The chunked-TOF decoding rule the block states. Until 0.9.13 it read "delta-within-chunk;
    // first absolute; cumsum", which describes a layout the writer never produced (the first point
    // is EXCLUDED from the delta array and the chunk rebuilds from `chunk_start`); a consumer who
    // followed the sentence decoded every chunk wrongly. The rule is pinned so it cannot drift
    // back to describing something else.
    pinned("cal[\"chunk_tof_encoding\"] = serde_json::json!(\"chunk_start + cumsum(deltas); first delta is relative to chunk_start\")");
    // Which derivation the (a, b) chord comes from — native GlobalMetadata or the SDK's own
    // tims_index_to_mz — so a reader knows which of two chords (4.28 ppm apart on 2485.d) it holds.
    pinned("\"chord_source\": chord_source");
    pinned("\"global_metadata\"");
    pinned("\"sdk_tims_index_to_mz\"");
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
    let sites = code().matches("\"codec\": \"tof-grid\"").count();
    assert_eq!(sites, 4, "expected 4 `codec: \"tof-grid\"` emission sites, found {sites}");
    assert_eq!(
        code().matches("\"lossless\": \"tof_index\"").count(),
        sites,
        "every `codec: \"tof-grid\"` block must name its exactly-stored column with the spec's \
         `lossless` key; found {} of {sites} emission sites",
        code().matches("\"lossless\": \"tof_index\"").count()
    );
    assert!(
        !src().contains("integer_column"),
        "`integer_column` is a synonym for the spec's `lossless` and was reverted; two keys naming \
         the same column is how they drift apart"
    );
    // The three honest values of `mz_reconstruction`, and the bound that must accompany each
    // inexact one. `exact` is ONE site (Agilent: the vendor's own bin ordinal, re-evaluated through
    // the vendor's own calibration). The Shimadzu profile lane said `exact` until 0.9.13; measured
    // on HEK_PosOAD1, 4,890 of 5,000 gridded points rebuild off the vendor's 1e-9 lattice by up to
    // 0.5 step (4.15e-10 Da) — inside the vendor's ±5e-10 rounding, so accurate to vendor precision,
    // but "exact" read as bit-exact. It now states the bound.
    assert_eq!(
        code().matches("\"mz_reconstruction\": \"exact\"").count(),
        1,
        "only the Agilent lane rebuilds m/z exactly; a new `exact` claim needs the same evidence"
    );
    pinned("\"mz_reconstruction\": \"within-vendor-rounding\"");
    pinned("\"max_error_da\": 5e-10");
    assert_eq!(
        code().matches("\"mz_reconstruction\": \"bounded-lossy\"").count(),
        2,
        "the run-wide and per-spectrum SCIEX grid lanes are bounded-lossy and must say so \
         (the Agilent lane is exact, the Shimadzu lane within vendor rounding)"
    );
    assert_eq!(
        code().matches("\"roundtrip_tolerance_ppm\": tof_grid::ppm_tol()").count(),
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
    // the spectra_metadata column specs (`from_spec(TOF_C0_CURIE, "tof_c0", …)` → `opt_MZP_1000003_tof_c0`).
    pinned("cal[\"per_spectrum\"] = serde_json::json!(\"tof_c0,tof_c1\")");
    pinned("cal[\"exact_per_spectrum\"] = serde_json::json!(true)");
    pinned("cal[\"per_spectrum_chord_frames\"]");
    pinned("\"mzpeak:transform_params_per_spectrum\".to_string(), \"tof_c0,tof_c1\".to_string()");
    pinned("TOF_C0_CURIE,\n                \"tof_c0\",");
    pinned("TOF_C1_CURIE,\n                \"tof_c1\",");
    // The accessions behind the `opt_MZP_1000003_tof_c0` / `opt_MZP_1000004_tof_c1` column names:
    // converter-owned MZP terms since 0.10.1 (`cv/mzpeak.obo`), rendered `MZP:` by the vendored
    // writer. The viewer and the vendored reader bind by name/suffix, so the accession may move
    // again without breaking them — but it must never move back into the PSI-owned `MS:` space.
    pinned("ControlledVocabulary::Unknown, 1_000_003)");
    pinned("ControlledVocabulary::Unknown, 1_000_004)");
    pinned("ControlledVocabulary::Unknown, 1_000_005)");
    assert!(
        !src().contains("ControlledVocabulary::MS, 4_000_9"),
        "the converter's own per-spectrum columns squatted MS:4000900–4000905 until 0.10.1; they are MZP terms now"
    );
}

/// M6: the TOF-grid lanes file each spectrum by the representation its source stated, so BOTH facets
/// must declare the integer axis — one field definition, declared on the data facet and inside the
/// peaks schema — and the grid route must never rewrite `signal_continuity`. Pinned as strings because
/// the only Windows-hosted lane (SCIEX) cannot be compiled here.
#[test]
fn tof_grid_files_by_representation_pinned() {
    pinned("fn tof_index_field(run_wide: (f64, f64), per_spectrum: bool)");
    pinned("fn tof_index_peak_schema(tof_field: std::sync::Arc<arrow::datatypes::Field>)");
    // the peaks schema carries the f64 fallback beside the axis, like the mz-grid lattice facet
    pinned(".add_field(tof_field)\n        .add_field(mzpeak_prototyping::peak_series::MZ_ARRAY.to_field())");
    // every lane that declares the peaks schema also declares the axis on the data facet
    assert_eq!(
        code().matches("tof_index_peak_schema(tof_field.clone())").count(),
        2,
        "the mzML --tof-grid lane and the native SCIEX lane share one peaks schema"
    );
    assert!(
        code().matches(".add_spectrum_field(tof_field)").count() >= 3,
        "mzML tof-grid, Agilent and SCIEX must declare the axis on spectra_data"
    );
    // The four forcing assignments the review found (mzML gridded → Centroid, mzML f64 fallback →
    // Profile, SCIEX gridded → Centroid, SCIEX f64 fallback → Profile) are gone: outside the unit
    // tests no lane assigns Centroid at all, and the one Profile assignment left is the Agilent
    // profile reader stating what MSProfile.bin is.
    let lanes = src().split("#[cfg(test)]").next().unwrap();
    assert_eq!(
        lanes.matches("signal_continuity = mzdata::spectrum::SignalContinuity::Centroid;").count(),
        0,
        "a lane rewrote signal_continuity to Centroid — the representation is not a routing knob (M6)"
    );
    assert_eq!(
        lanes.matches("signal_continuity = mzdata::spectrum::SignalContinuity::Profile;").count(),
        1,
        "only the Agilent MSProfile.bin reader may state Profile; a tof-grid route must carry the source's"
    );
}

/// The `transformations` index block — the invariant's second half, "every transformation declared
/// in the archive" — is written by every mzPeak lane under one key, and the entry names are what a
/// reader (or a corpus audit) matches on.
#[test]
fn transformations_block_pinned() {
    pinned("(\"transformations\".to_string(), serde_json::json!(applied))");
    pinned("\"zero-run-mask\"");
    pinned("\"numpress-linear\"");
    pinned("\"sort-by-mz\"");
    pinned("\"tof-grid:{}ppm\"");
    pinned("\"shimadzu:span-trim\"");
    pinned("\"agilent:drop-zero-samples\"");
    pinned("\"bruker:trace-unit-rescale\"");
    pinned("\"bruker:trace-sort-dedup\"");
}
