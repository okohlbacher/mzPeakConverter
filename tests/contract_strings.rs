//! Fixture-free pin of the contract strings mzPeakViewer and the reference reader match on.
//!
//! mzPeakViewer keys the ims-compact TOF→m/z reconstruction off the `ims_calibration` block, and
//! every grid lane since 0.14 stores its model on the grid rows themselves as the PSI-MS terms the
//! reference implementation reads (`MS:1003825` / `MS:1003824`, mzdata's `[intercept, slope, scale]`
//! convention). A silent reformat of any of these would make a reader fail-loud and render empty
//! spectra. These tests assert the literals are still emitted VERBATIM in the converter source, with
//! no corpus fixture needed (we read `src/main.rs` at compile time and search its CODE: comments and
//! the `#[cfg(test)]` module are cut away first, so neither a comment quoting a literal nor a unit
//! test asserting on it can stand in for the emission site — see [`code`]).
//!
//! Changing any pinned string is a BREAKING contract change: bump the version and notify the viewer
//! team. See the calibration emission sites in `src/main.rs` (ims_calibration, sqrt_grid_param).

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
    // The timsTOF layout since 0.13.0, written natively since 0.14: the reference implementation's
    // chunk grid, the models on every row. The viewer keys off these block values.
    pinned("\"layout\": \"grid-transform\"");
    pinned("\"tof_encoding\": \"grid\"");
    pinned("\"chunk_bounds\": \"mz\"");
    pinned("\"column\": \"chunk.mz_grid\"");
    pinned("\"column\": \"chunk.mean_inverse_reduced_ion_mobility_grid\"");
    pinned("\"model\": mzpeak_prototyping::grid::TimsTofMzGrid2::ACCESSION.to_string()");
    pinned("\"model\": mzpeak_prototyping::grid::TimsTofTimsLinearGrid2::ACCESSION.to_string()");
    // Which derivation the fallback chord comes from — native GlobalMetadata or the SDK's own
    // tims_index_to_mz — so a reader knows which of two chords (4.28 ppm apart on 2485.d) a frame
    // without a calibration row sits on.
    pinned("\"source\": chord_source");
    pinned("\"global_metadata\"");
    pinned("\"sdk_tims_index_to_mz\"");
    // The 0.12.x TOF layout is gone: no writer emits its labels or its point-layout axis.
    for gone in ["\"m/z-chunked\"", "\"absolute\"", "tof_chunk_encoding", "fn ims_chunked_peak_schema(", "fn tof_axis_field(", "ArrayType::nonstandard(\"tof\")", "\"exact_per_spectrum\"", "per_spectrum_chord_frames"] {
        assert!(!code().contains(gone), "`{gone}` is back: the 0.12.x timsTOF layout was retired in 0.14");
    }
}

/// The grid model every sqrt-grid lane (mzML `--tof-grid`, native SCIEX, the Shimadzu profile facet,
/// `--agilent-grid`) attaches to its m/z array, and the two writer policies that turn it into the
/// reference implementation's chunk-grid rows: the exact one (tolerance 0, a spectrum without a model
/// is stored raw) and the fitted one for lattice centroid lists (`MS:1003824`, within 1e-6 Da). One
/// emission site each, shared by every lane, so the four cannot drift apart.
#[test]
fn chunk_grid_models_pinned() {
    pinned(".curie(mzdata::curie!(MS:1003825))");
    pinned(".name(\"square root grid interpolation\")");
    // mzdata's parameter order: [intercept, slope, scale] — the reader evaluates (i·slope + intercept)²/scale.
    pinned("[c0, c1, 1.0].map(mzdata::params::Value::Float)");
    assert_eq!(code().matches("fn sqrt_grid_param(").count(), 1, "one model builder");
    assert_eq!(code().matches("mz_da.add_param(sqrt_grid_param(c0, c1));").count(), 1, "attached in `sqrt_grid_arrays` only");
    assert_eq!(code().matches("sqrt_grid_arrays(").count(), 5, "the definition + four lanes (mzML tof-grid, SciEX, Shimadzu, Agilent)");
    pinned("GridPolicy::quadratic(ArrayType::MZArray, Some(mzpeaks::Tolerance::Da(0.0)))");
    pinned("GridPolicy::linear(ArrayType::MZArray, Some(mzpeaks::Tolerance::Da(GRID_FIT_TOL_DA)))");
    pinned("const GRID_FIT_TOL_DA: f64 = 1e-6;");
    pinned("const GRID_FIT_TRANSFORMATION: &str = \"grid-fit:1e-6Da\";");
    // Chunk width: one chunk per spectrum on the per-spectrum sqrt grids, upstream's default on the fit.
    pinned("const GRID_CHUNK_TH: f64 = 1.0e6;");
    pinned("const GRID_PEAK_CHUNK_TH: f64 = 50.0;");
    // No point-layout grid axis is written any more (readers still decode 0.9–0.13 archives).
    for gone in ["fn tof_index_field(", "fn tof_index_peak_schema(", "fn tof_grid_block(", "\"tof_calibration\".to_string()", "\"mz_calibration\".to_string()", "ArrayType::nonstandard(\"tof_index\")"] {
        assert!(!code().contains(gone), "`{gone}` is back: the point-layout grid was retired in 0.14");
    }
    // M6: outside the unit tests no lane assigns Centroid at all, and the one Profile assignment
    // left is the Agilent profile reader stating what MSProfile.bin is.
    let lanes = src().split("#[cfg(test)]").next().unwrap();
    assert_eq!(
        lanes.matches("signal_continuity = mzdata::spectrum::SignalContinuity::Centroid;").count(),
        0,
        "a lane rewrote signal_continuity to Centroid — the representation is not a routing knob (M6)"
    );
    assert_eq!(
        lanes.matches("signal_continuity = mzdata::spectrum::SignalContinuity::Profile;").count(),
        1,
        "only the Agilent MSProfile.bin reader may state Profile; a grid route must carry the source's"
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
    pinned("\"grid-fit:1e-6Da\"");
    pinned("\"grid-encode:mz,ion_mobility\"");
    for (file, source) in [("src/bruker_native.rs", include_str!("../src/bruker_native.rs"))] {
        for entry in ["\"bruker:mz-calibrant-omitted\"", "\"bruker:mz-calibration-chord\""] {
            assert!(source.contains(entry), "{file} no longer declares {entry}");
        }
    }
    pinned("\"shimadzu:span-trim\"");
    pinned("\"shimadzu:coarse-mz\"");
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
