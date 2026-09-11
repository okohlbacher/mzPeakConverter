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
    // string AND this exact tof_to_mz formula, so both are load-bearing. Since 0.12.0 the model and
    // the reconstruction claim reach the block through `tof_grid_block`, so the pin is the CALL —
    // both lanes that use this model string (native SciEX, and the Shimadzu profile grid, which
    // shares the formula family).
    pinned("tof_grid_block(\"sciex_sqrt_per_spectrum\", MzReconstruction::BoundedLossyPpm");
    pinned("\"sciex_sqrt_per_spectrum\",\n            MzReconstruction::WithinVendorRoundingDa(shimadzu_grid::TOL),");
    // KEY AND VALUE, and counted: both lanes that state this formula must keep the key too. A pin on
    // the value alone would stay green through a rename of the key the viewer matches on.
    for (needle, what) in [
        ("cal.insert(\"tof_to_mz\".to_string(), serde_json::json!(\"mz = (tof_c0 + tof_c1*tof_index)^2\"));", "tof_to_mz"),
        ("cal.insert(\"per_spectrum_columns\".to_string(), serde_json::json!([\"tof_c0\", \"tof_c1\"]));", "per_spectrum_columns"),
    ] {
        assert_eq!(
            code().matches(needle).count(),
            2,
            "both per-spectrum sqrt lanes (native SciEX, Shimadzu profile grid) must emit `{what}` \
             verbatim; found {} of 2",
            code().matches(needle).count()
        );
    }
}

#[test]
fn agilent_and_sciex_global_models_pinned() {
    pinned("tof_grid_block(\"agilent_sqrt_poly\", MzReconstruction::Exact)");
    // The global-coefficient mzML `--tof-grid` path (distinct from the per-spectrum SciEX encoding).
    pinned("tof_grid_block(\"sciex_sqrt\", MzReconstruction::BoundedLossyPpm");
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
    // Since 0.12.0 the shared keys are written once, by `tof_grid_block`, and every lane builds its
    // block through it — so `codec`, `lossless` and `mz_reconstruction` are guaranteed to agree by
    // construction and what is worth counting is the CALL SITES. A fifth lane that hand-rolls its
    // own JSON instead of calling the builder is the failure this catches.
    let sites = code().matches("tof_grid_block(").count() - 1; // less the definition
    assert_eq!(sites, 4, "expected 4 `tof_grid_block(` call sites, found {sites}");
    // Counted on the VALUE literal, not on a `"codec": "tof-grid",` spelling: a hand-rolled block
    // with the key last (no trailing comma), or with no space after the colon, or built through
    // `serde_json::Map::insert`, would slip past a spelling-sensitive guard — two of those four
    // spellings slipped past the pre-0.12.0 site count too. `"--tof-grid"` and `"tof-grid:{}ppm"`
    // do not contain the quoted token, so the only match is the builder's own.
    assert_eq!(
        code().matches("\"tof-grid\"").count(),
        1,
        "only `tof_grid_block` may write the `codec: \"tof-grid\"` value; found {}",
        code().matches("\"tof-grid\"").count()
    );
    pinned("block.insert(\"codec\".to_string(), serde_json::json!(\"tof-grid\"));");
    pinned("block.insert(\"lossless\".to_string(), serde_json::json!(\"tof_index\"));");
    assert!(
        !src().contains("integer_column"),
        "`integer_column` is a synonym for the spec's `lossless` and was reverted; two keys naming \
         the same column is how they drift apart"
    );
    // The three honest values of `mz_reconstruction`, and the bound that must accompany each
    // inexact one. `exact` is ONE site (Agilent: the vendor's own bin ordinal, re-evaluated through
    // the vendor's own calibration). The Shimadzu profile lane said `exact` until 0.9.13, then
    // `max_error_da: 5e-10` — a bound its fit never enforced. The fit accepts a spectrum when every
    // point rebuilds within `shimadzu_grid::TOL` (1e-9 Da); refitting HEK_PosOAD1's nine f64 spectra
    // puts 169 of 32,434 points between 5e-10 and 5.47e-10 Da off (none past 1e-9). The earlier
    // evidence, "≤ 0.5 step off the lattice", holds for any value by definition. The block declares
    // the gate itself, so the bound and the check cannot diverge again.
    assert_eq!(
        code().matches("MzReconstruction::Exact").count(),
        2, // the variant's own arm in `tof_grid_block`, and the one lane that claims it
        "only the Agilent lane rebuilds m/z exactly; a new `exact` claim needs the same evidence"
    );
    pinned("block.insert(\"mz_reconstruction\".to_string(), serde_json::json!(\"exact\"));");
    pinned("block.insert(\"mz_reconstruction\".to_string(), serde_json::json!(\"within-vendor-rounding\"));");
    pinned("block.insert(\"max_error_da\".to_string(), serde_json::json!(da));");
    pinned("MzReconstruction::WithinVendorRoundingDa(shimadzu_grid::TOL)");
    assert!(
        include_str!("../src/shimadzu_grid.rs").contains("pub const TOL: f64 = 1e-9;"),
        "the Shimadzu block declares the fit's acceptance gate as its bound: 1e-9 Da"
    );
    assert_eq!(
        code().matches("MzReconstruction::BoundedLossyPpm(tof_grid::ppm_tol())").count(),
        2,
        "the run-wide and per-spectrum SCIEX grid lanes are bounded-lossy and must say so \
         (the Agilent lane is exact, the Shimadzu lane within vendor rounding)"
    );
    // A bounded claim cannot be written without its bound any more: the bound is the variant's
    // payload, and the builder writes the key from it. This pins that it still does.
    pinned("block.insert(\"mz_reconstruction\".to_string(), serde_json::json!(\"bounded-lossy\"));");
    pinned("block.insert(\"roundtrip_tolerance_ppm\".to_string(), serde_json::json!(ppm));");
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
    // One axis definition, two column names, and which lane uses which is the pin: the sqrt-grid
    // lanes store `tof_index`, ims-compact stores `tof` (and its index block says `"lossless": "tof"`
    // to match). Swapping them would leave a reader looking for a column that is not there.
    pinned("tof_axis_field(\"tof_index\", run_wide, per_spectrum)");
    pinned("tof_axis_field(\"tof\", (model_a, model_b), exact_per_spectrum.is_some())");
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
    pinned("\"sort-by-time\"");
    pinned("\"sort-by-wavelength\"");
    pinned("\"chromatogram-time-to-minutes\"");
    pinned("\"tof-grid:{}ppm\"");
    pinned("\"shimadzu:span-trim\"");
    pinned("\"agilent:drop-zero-samples\"");
    // A fixed identifier: how many windows were rewritten goes to the run's warning.
    pinned("\"thermo:target-only-isolation-window\"");
    pinned("\"agilent:intensity-f32-rounding\"");
    pinned("\"bruker:trace-unit-rescale\"");
    pinned("\"bruker:trace-sort-dedup\"");
    // The entries the reader modules declare from their own counts, pinned in those modules' code
    // (their `#[cfg(test)]` modules cut away, as `code` does for main.rs).
    for (file, source, entries) in [
        ("src/agl.rs", include_str!("../src/agl.rs"), ["\"agilent:nonfinite-intensity-to-zero\"", "\"agilent:truncate-unequal-arrays\""]),
        ("src/waters.rs", include_str!("../src/waters.rs"), ["\"waters:drop-functions\"", "\"waters:sonar-summed\""]),
    ] {
        let source = source.replace("\r\n", "\n");
        let lanes = source.split("\n#[cfg(test)]").next().unwrap();
        for entry in entries {
            assert!(lanes.contains(entry), "{file} no longer declares {entry}");
        }
    }
}

/// The native Waters lane's two counted entries, `sort-by-mz` (a frame re-sorted) and
/// `waters:sonar-summed`, run only where no CI host can: `convert_waters` is `cfg(windows)` and
/// `WatersReader::spectrum` reads through MassLynxRaw.dll. The host test of the counter seam
/// (`reader_counters_count_written_spectra_only`) builds its own counters, so deleting the lane's
/// push or the reader's bump would leave the suite green while the archive stopped declaring what
/// it did. Both halves are pinned: the reader bumps each counter where the transformation happens
/// and hands out that same counter, and the lane pairs it with its entry and passes the hints on.
#[test]
fn waters_counted_entries_pinned() {
    /// `text` from `head` up to the first `close` after it: the body of one item.
    fn body<'a>(text: &'a str, head: &str, close: &str) -> &'a str {
        let start = text.find(head).unwrap_or_else(|| panic!("`{head}` is gone"));
        let rest = &text[start..];
        &rest[..rest.find(close).unwrap_or(rest.len())]
    }
    let waters = include_str!("../src/waters.rs").replace("\r\n", "\n");
    let reader: String =
        waters.split("\n#[cfg(test)]").next().unwrap().lines().map(strip_comment).collect::<Vec<_>>().join("\n");
    let spectrum = body(&reader, "pub fn spectrum(&self, i: usize)", "\n    }\n");
    let lane = body(code(), "fn convert_waters(", "\n}\n");
    let missing: Vec<&str> = [
        (spectrum, "if sort_frame_points(&mut points) {\n                self.resorted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);"),
        (spectrum, "if fi.sonar_bins > 0 {\n                self.sonar_summed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);"),
        (reader.as_str(), "pub fn reorder_counter(&self) -> std::sync::Arc<std::sync::atomic::AtomicUsize> {\n        self.resorted.clone()"),
        (reader.as_str(), "pub fn sonar_counter(&self) -> std::sync::Arc<std::sync::atomic::AtomicUsize> {\n        self.sonar_summed.clone()"),
        (reader.as_str(), "pub const SONAR_SUMMED: &str = \"waters:sonar-summed\";"),
        (lane, "hints.counters.push((waters::SONAR_SUMMED, reader.sonar_counter()));"),
        (lane, "hints.counters.push((\"sort-by-mz\", reader.reorder_counter()));"),
        (lane, "convert_vendor_reader(input, output, chunk, zstd_level, vendor, synth_chroms, hints, reader.len(), |i| reader.spectrum(i))"),
    ]
    .into_iter()
    .filter(|(text, needle)| !text.contains(needle))
    .map(|(_, needle)| needle)
    .collect();
    assert!(missing.is_empty(), "the Waters lane no longer counts what it declares; missing: {missing:#?}");
}
