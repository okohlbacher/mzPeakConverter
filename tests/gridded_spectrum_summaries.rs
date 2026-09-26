//! Grid-routed spectra must carry real per-spectrum summaries (corpus-gated; ~2 s).
//!
//! REGRESSION. Through 0.13 a grid route rebuilt the spectrum around an INTEGER axis (`tof_index` /
//! `tof`) and dropped the `m/z array`. mzdata derives `total_ion_current`, `base_peak_mz`,
//! `base_peak_intensity` and the observed-m/z bounds from the m/z + intensity arrays, so an m/z-less
//! array map folds to `tic = 0`, `base peak = (0, 0)`, `m/z range = (0, 0)` — and the published
//! corpus shipped `total_ion_current = 0` on EVERY gridded spectrum (13,200/13,200 on a Shimadzu
//! run, 2,092/2,101 on a second Shimadzu one, 1,502/1,502 on an Agilent one) while the peak data
//! itself was intact. The chunk-grid route (0.14) hands the writer the grid VALUES as m/z with the
//! model attached, and still states the summary explicitly; this test keeps both halves honest.
//!
//! The mzML `--tof-grid` lane is the one grid lane reachable off Windows, and it gives the sharpest
//! possible assertion: the SAME input converted with and without the grid must describe its data
//! the same way. A gridded archive is not allowed to be a worse description of its own data.
//!
//! WHAT "the same" MEANS, and why it is not bit-equality on m/z. The summary columns describe the
//! points STORED IN THIS ARCHIVE, so the grid lane states the m/z a reader RECONSTRUCTS from the
//! grid row, not the source f64 the fit consumed. The grid accepts a point whose reconstruction
//! lands within `MZPC_TOF_GRID_PPM` (default 5) of the source, so those two differ — which is the
//! encoding being bounded-lossy, exactly as `transformations` says. The alternative
//! (copy the source m/z into the columns) makes the archive contradict ITSELF: the published
//! `20240826_RNAseB_…_MRM_03.mzpeak` states `base_peak_mz = 519.1402875577935` on spectrum 7313
//! while its own stored `tof_index` reconstructs to `519.1426532537401`, so no point in the file
//! sits at the m/z its metadata names and the observed-m/z bounds exclude 4.7 ppm of its own data.
//! Intra-archive consistency is what a reader can check and depend on; cross-lane bit-equality on a
//! quantized axis is not available at all. INTENSITY is stored verbatim, so TIC and
//! `base_peak_intensity` do stay bit-equal between the lanes.
//!
//! Runs on the committed, gzipped copy under `tests/fixtures`, so it runs in CI too.

use std::path::{Path, PathBuf};
use std::process::Command;

use arrow::array::{Array, AsArray};
use arrow::datatypes::{Float32Type, Float64Type};
use arrow::record_batch::RecordBatch;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

/// A SCIEX X500R QTOF SWATH run from the ProteoWizard test set: 201 spectra, every one of which
/// lands on the integer TOF lattice, so `--tof-grid on` routes all 201 through the grid.
const MZML: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/swath.api-sample-centroid.mzML.gz");
const SPECTRA: usize = 201;


fn run(args: &[&str]) {
    let st = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"))
        .args(args)
        .status()
        .expect("failed to run mzpeak-convert");
    assert!(st.success(), "mzpeak-convert {args:?} failed: {st}");
}

fn batches(archive: &Path, member: &str, dir: &Path) -> Vec<RecordBatch> {
    let f = std::fs::File::open(archive).unwrap();
    let mut z = zip::ZipArchive::new(f).unwrap();
    // A facet nothing was filed to may be absent (the peak writer is created lazily).
    let Ok(mut e) = z.by_name(member) else { return Vec::new() };
    let out = dir.join(format!("{}-{member}", archive.file_name().unwrap().to_string_lossy()));
    let mut o = std::fs::File::create(&out).unwrap();
    std::io::copy(&mut e, &mut o).unwrap();
    let rdr = ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(&out).unwrap())
        .unwrap()
        .with_batch_size(1 << 16)
        .build()
        .unwrap();
    rdr.map(|b| b.unwrap()).collect()
}

/// The five per-spectrum summary columns, in `index` order.
#[derive(Default)]
struct Summaries {
    tic: Vec<Option<f32>>,
    bp_mz: Vec<Option<f64>>,
    bp_int: Vec<Option<f32>>,
    lo_mz: Vec<Option<f64>>,
    hi_mz: Vec<Option<f64>>,
    /// Gridded: the spectrum's rows are `MS:1003826` grid rows (a non-null `mz_grid`) — in
    /// `spectra_peaks.parquet` for a centroid spectrum, in `spectra_data.parquet` for a profile one.
    /// Since 0.10.1 the facet follows the source's representation (review M6), so neither facet
    /// membership nor `number_of_peaks` says whether a spectrum was gridded; the grid column does.
    gridded: Vec<bool>,
}

/// `(all, gridded)`: the set of `spectrum_index` values whose points live in `member`, and the subset
/// whose rows carry a non-null `mz_grid`. A facet's rows are one `chunk` struct column on both lanes
/// (the mzML→mzPeak default is m/z-chunked, the grid lane is grid-chunked); the struct is located by
/// its `spectrum_index` child, not by name.
fn spectrum_indices_in(
    archive: &Path,
    member: &str,
    dir: &Path,
) -> (std::collections::HashSet<u64>, std::collections::HashSet<u64>) {
    let mut all = std::collections::HashSet::new();
    let mut gridded = std::collections::HashSet::new();
    for b in batches(archive, member, dir) {
        let rows = b
            .columns()
            .iter()
            .filter_map(|c| c.as_struct_opt())
            .find(|st| st.column_by_name("spectrum_index").is_some())
            .unwrap_or_else(|| panic!("{member}: no struct column with a spectrum_index child: {:?}", b.schema()));
        let idx = rows.column_by_name("spectrum_index").unwrap().as_primitive::<arrow::datatypes::UInt64Type>();
        let grid = rows.column_by_name("mz_grid");
        for i in 0..b.num_rows() {
            all.insert(idx.value(i));
            if grid.is_some_and(|c| c.is_valid(i)) {
                gridded.insert(idx.value(i));
            }
        }
    }
    (all, gridded)
}

fn summaries(archive: &Path, dir: &Path) -> Summaries {
    let mut s = Summaries::default();
    let (in_peaks, grid_peaks) = spectrum_indices_in(archive, "spectra_peaks.parquet", dir);
    let (in_data, grid_data) = spectrum_indices_in(archive, "spectra_data.parquet", dir);
    for b in batches(archive, "spectra_metadata.parquet", dir) {
        let index = b.column_by_name("index").unwrap().as_primitive::<arrow::datatypes::UInt64Type>();
        let tic = b.column_by_name("total_ion_current").unwrap().as_primitive::<Float32Type>();
        let bpm = b.column_by_name("base_peak_mz").unwrap().as_primitive::<Float64Type>();
        let bpi = b.column_by_name("base_peak_intensity").unwrap().as_primitive::<Float32Type>();
        let lo = b.column_by_name("lowest_observed_mz").unwrap().as_primitive::<Float64Type>();
        let hi = b.column_by_name("highest_observed_mz").unwrap().as_primitive::<Float64Type>();
        for i in 0..b.num_rows() {
            let ix = index.value(i);
            assert!(
                !(in_peaks.contains(&ix) && in_data.contains(&ix)),
                "spectrum {ix}: points in BOTH spectra_peaks and spectra_data; facet membership is ambiguous"
            );
            s.tic.push((!tic.is_null(i)).then(|| tic.value(i)));
            s.bp_mz.push((!bpm.is_null(i)).then(|| bpm.value(i)));
            s.bp_int.push((!bpi.is_null(i)).then(|| bpi.value(i)));
            s.lo_mz.push((!lo.is_null(i)).then(|| lo.value(i)));
            s.hi_mz.push((!hi.is_null(i)).then(|| hi.value(i)));
            s.gridded.push(grid_peaks.contains(&ix) || grid_data.contains(&ix));
        }
    }
    s
}

#[test]
fn gridded_archive_summaries_match_the_f64_lane() {
    let input = PathBuf::from(MZML);
    let dir = std::env::temp_dir().join(format!("mzpc-gridsummary-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let gridded = dir.join("grid.mzpeak");
    let plain = dir.join("f64.mzpeak");
    run(&[input.to_str().unwrap(), "-o", gridded.to_str().unwrap(), "--tof-grid", "on"]);
    run(&[input.to_str().unwrap(), "-o", plain.to_str().unwrap(), "--tof-grid", "off"]);

    let g = summaries(&gridded, &dir);
    let f = summaries(&plain, &dir);
    assert_eq!(g.tic.len(), SPECTRA, "expected {SPECTRA} spectra in the gridded archive");
    assert_eq!(f.tic.len(), SPECTRA, "expected {SPECTRA} spectra in the f64 archive");
    assert_eq!(
        g.gridded.iter().filter(|v| **v).count(),
        SPECTRA,
        "every spectrum of this file is on the lattice, so all {SPECTRA} must be grid-routed"
    );

    // 1. The defect itself: no grid-routed spectrum may ship a zero/absent summary.
    for i in 0..SPECTRA {
        assert!(
            g.tic[i].is_some_and(|v| v > 0.0),
            "spectrum {i}: gridded total_ion_current is {:?}, expected a positive value",
            g.tic[i]
        );
        assert!(g.bp_mz[i].is_some_and(|v| v > 0.0), "spectrum {i}: gridded base_peak_mz missing");
        assert!(
            g.bp_int[i].is_some_and(|v| v > 0.0),
            "spectrum {i}: gridded base_peak_intensity missing"
        );
        assert!(g.lo_mz[i].is_some(), "spectrum {i}: gridded lowest_observed_mz is NULL");
        assert!(g.hi_mz[i].is_some(), "spectrum {i}: gridded highest_observed_mz is NULL");
    }

    // 2. The stronger statement: gridding must not change what the summary SAYS — exactly for the
    //    intensity-derived columns (intensity is stored verbatim), and to within the grid's own
    //    round-trip bound for the m/z columns (see the module header).
    let tol = std::env::var("MZPC_TOF_GRID_PPM")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| *v > 0.0)
        .unwrap_or(5.0)
        * 1e-6;
    let near = |a: f64, b: f64| (a - b).abs() <= b.abs() * tol;
    for i in 0..SPECTRA {
        assert_eq!(g.tic[i], f.tic[i], "spectrum {i}: total_ion_current differs between lanes");
        assert_eq!(
            g.bp_int[i], f.bp_int[i],
            "spectrum {i}: base_peak_intensity differs between lanes"
        );
        // base_peak_mz names the SAME point in both lanes, at that point's own coordinate in each:
        // within the grid tolerance of the f64 lane's value. It may also differ further on an
        // INTENSITY TIE — the grid lane resolves ties to the lowest m/z, mzdata's derived summary
        // resolves them first-in-array — so a value BELOW the f64 lane's is allowed outright, while
        // a value above it is only allowed by the quantization bound.
        // Bounded on BOTH sides, or the tie allowance swallows the assertion: "anything at or below
        // the f64 lane's value" would accept the spectrum's LOWEST m/z as its base peak. The floor
        // is a coordinate that must exist in this spectrum — its own observed-m/z minimum — so a tie
        // may only move the answer to another real point of the same spectrum.
        let (gm, fm) = (g.bp_mz[i].unwrap(), f.bp_mz[i].unwrap());
        assert!(
            gm <= fm * (1.0 + tol),
            "spectrum {i}: base_peak_mz {gm} exceeds {fm} by more than the grid tolerance"
        );
        assert!(
            gm >= g.lo_mz[i].unwrap() * (1.0 - tol),
            "spectrum {i}: base_peak_mz {gm} is below the archive's own lowest_observed_mz {:?}",
            g.lo_mz[i]
        );
        let (glo, flo) = (g.lo_mz[i].unwrap(), f.lo_mz[i].unwrap());
        assert!(near(glo, flo), "spectrum {i}: lowest_observed_mz {glo} vs {flo} off-tolerance");
        let (ghi, fhi) = (g.hi_mz[i].unwrap(), f.hi_mz[i].unwrap());
        assert!(near(ghi, fhi), "spectrum {i}: highest_observed_mz {ghi} vs {fhi} off-tolerance");
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// A string column as `StringArray`, whether Arrow materialised it as plain or dictionary-encoded
/// Utf8 (the grid struct's `grid_type` comes back dictionary-encoded).
fn strings(col: &arrow::array::ArrayRef) -> arrow::array::StringArray {
    arrow::compute::cast(col, &arrow::datatypes::DataType::Utf8).unwrap().as_string::<i32>().clone()
}

/// A real gridded archive must ADMIT that m/z is quantized — the summary columns state the
/// reconstructed coordinate, not the source f64, and a reader comparing them against an mzML needs
/// to know the difference is encoding loss and not a defect — and its rows must carry the model a
/// reader evaluates: `MS:1003826` chunk rows under the PSI-MS sqrt model `MS:1003825`.
///
/// This is the archive-level half of `contract_strings::chunk_grid_models_pinned`, which pins the
/// same strings in the source. Both exist because the string pin cannot see whether the model
/// actually reaches the file, and this one reads the file itself.
#[test]
fn gridded_archive_states_its_reconstruction_contract() {
    let input = PathBuf::from(MZML);
    let dir = std::env::temp_dir().join(format!("mzpc-gridcal-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let gridded = dir.join("grid.mzpeak");
    run(&[input.to_str().unwrap(), "-o", gridded.to_str().unwrap(), "--tof-grid", "on"]);

    // Every row of the peaks facet (this fixture is centroid-only) is a grid row under the sqrt model.
    let (mut rows, mut grid_rows) = (0usize, 0usize);
    for b in batches(&gridded, "spectra_peaks.parquet", &dir) {
        let chunk = b.column_by_name("chunk").expect("chunk facet").as_struct();
        let enc = strings(chunk.column_by_name("chunk_encoding").unwrap());
        let grid = chunk.column_by_name("mz_grid").expect("mz_grid column").as_struct();
        let kind = strings(grid.column_by_name("grid_type").unwrap());
        for i in 0..b.num_rows() {
            rows += 1;
            if enc.value(i) == "MS:1003826" {
                grid_rows += 1;
                assert_eq!(kind.value(i), "MS:1003825", "row {i}: the sqrt model");
            }
        }
    }
    assert_eq!(rows, SPECTRA, "one chunk per spectrum");
    assert_eq!(grid_rows, SPECTRA, "every spectrum of this file is on the lattice");

    let f = std::fs::File::open(&gridded).unwrap();
    let mut z = zip::ZipArchive::new(f).unwrap();
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut z.by_name("mzpeak_index.json").unwrap(), &mut buf).unwrap();
    let idx: serde_json::Value = serde_json::from_slice(&buf).unwrap();
    // No 0.13 `tof_calibration` block: the model rides on every grid row.
    assert!(idx["metadata"]["tof_calibration"].is_null(), "the point-layout calibration block is gone: {idx}");
    // Storing a quantized axis IS a transformation, and `transformations` lists what a conversion
    // APPLIED (0.12.0). An archive that quietly re-encoded m/z without saying so is the failure.
    let applied = idx
        .get("metadata")
        .and_then(|m| m.get("transformations"))
        .and_then(|t| t.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect::<Vec<_>>())
        .unwrap_or_default();
    assert!(
        applied.iter().any(|e| e.starts_with("tof-grid:") && e.ends_with("ppm")),
        "a gridded archive must declare the grid and its bound in `transformations`; got {applied:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
