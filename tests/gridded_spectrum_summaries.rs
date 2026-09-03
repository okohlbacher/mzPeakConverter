//! Grid-routed spectra must carry real per-spectrum summaries (corpus-gated; ~2 s).
//!
//! REGRESSION. A grid route rebuilds the spectrum around an INTEGER axis (`tof_index` / `tof`) and
//! drops the `m/z array`. mzdata derives `total_ion_current`, `base_peak_mz`, `base_peak_intensity`
//! and the observed-m/z bounds from the m/z + intensity arrays, so an m/z-less array map folds to
//! `tic = 0`, `base peak = (0, 0)`, `m/z range = (0, 0)` — and the published corpus shipped
//! `total_ion_current = 0` on EVERY gridded spectrum (13,200/13,200 on a Shimadzu run,
//! 2,092/2,101 on a second Shimadzu one, 1,502/1,502 on an Agilent one) while the peak data
//! itself was intact.
//!
//! The mzML `--tof-grid` lane is the one grid lane reachable off Windows, and it gives the sharpest
//! possible assertion: the SAME input converted with and without the grid must describe its data
//! the same way. A gridded archive is not allowed to be a worse description of its own data.
//!
//! WHAT "the same" MEANS, and why it is not bit-equality on m/z. The summary columns describe the
//! points STORED IN THIS ARCHIVE, so the grid lane states the m/z a reader RECONSTRUCTS from
//! `tof_index`, not the source f64 the fit consumed. The grid accepts a point whose reconstruction
//! lands within `MZPC_TOF_GRID_PPM` (default 5) of the source, so those two differ — which is the
//! encoding being bounded-lossy, exactly as the `tof_calibration` block now says. The alternative
//! (copy the source m/z into the columns) makes the archive contradict ITSELF: the published
//! `20240826_RNAseB_…_MRM_03.mzpeak` states `base_peak_mz = 519.1402875577935` on spectrum 7313
//! while its own stored `tof_index` reconstructs to `519.1426532537401`, so no point in the file
//! sits at the m/z its metadata names and the observed-m/z bounds exclude 4.7 ppm of its own data.
//! Intra-archive consistency is what a reader can check and depend on; cross-lane bit-equality on a
//! quantized axis is not available at all. INTENSITY is stored verbatim, so TIC and
//! `base_peak_intensity` do stay bit-equal between the lanes.
//!
//! Skips (passes) when the reference mzML is absent; override the corpus root with `MZPEAK_CORPUS`.

use std::path::{Path, PathBuf};
use std::process::Command;

use arrow::array::{Array, AsArray};
use arrow::datatypes::{Float32Type, Float64Type};
use arrow::record_batch::RecordBatch;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

/// A SCIEX TripleTOF SWATH sample from the ProteoWizard test set: 201 spectra, every one of which
/// lands on the integer TOF lattice, so `--tof-grid on` routes all 201 through the grid.
const MZML: &str = "pwiz-examples/ABI/ABI/Reader_ABI_Test.data/swath.api-sample-centroid.mzML";
const SPECTRA: usize = 201;

fn corpus_root() -> PathBuf {
    std::env::var("MZPEAK_CORPUS").map(PathBuf::from).unwrap_or_else(|_| {
        PathBuf::from(std::env::var("HOME").unwrap_or_default())
            .join("Claude/mzpeak-example-data/data")
    })
}

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
    let mut e = z.by_name(member).unwrap_or_else(|_| panic!("{member} missing"));
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
    /// `number_of_peaks` non-null == routed to the custom peak facet (i.e. gridded).
    gridded: Vec<bool>,
}

fn summaries(archive: &Path, dir: &Path) -> Summaries {
    let mut s = Summaries::default();
    for b in batches(archive, "spectra_metadata.parquet", dir) {
        let tic = b.column_by_name("total_ion_current").unwrap().as_primitive::<Float32Type>();
        let bpm = b.column_by_name("base_peak_mz").unwrap().as_primitive::<Float64Type>();
        let bpi = b.column_by_name("base_peak_intensity").unwrap().as_primitive::<Float32Type>();
        let lo = b.column_by_name("lowest_observed_mz").unwrap().as_primitive::<Float64Type>();
        let hi = b.column_by_name("highest_observed_mz").unwrap().as_primitive::<Float64Type>();
        let npk = b.column_by_name("number_of_peaks").unwrap();
        for i in 0..b.num_rows() {
            s.tic.push((!tic.is_null(i)).then(|| tic.value(i)));
            s.bp_mz.push((!bpm.is_null(i)).then(|| bpm.value(i)));
            s.bp_int.push((!bpi.is_null(i)).then(|| bpi.value(i)));
            s.lo_mz.push((!lo.is_null(i)).then(|| lo.value(i)));
            s.hi_mz.push((!hi.is_null(i)).then(|| hi.value(i)));
            s.gridded.push(!npk.is_null(i));
        }
    }
    s
}

#[test]
fn gridded_archive_summaries_match_the_f64_lane() {
    let input = corpus_root().join(MZML);
    if !input.exists() {
        eprintln!("skipping: {} not present", input.display());
        return;
    }
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

/// The `tof_calibration` block a real gridded archive carries must be self-consistent with the
/// summary columns asserted above: it must NAME the integer axis a reader has to evaluate, and it
/// must ADMIT that m/z is quantized — because the columns state the reconstructed coordinate, not
/// the source f64, and a reader comparing them against an mzML needs to know the difference is
/// encoding loss and not a defect.
///
/// This is the archive-level half of `contract_strings::tof_grid_reconstruction_keys_pinned`, which
/// pins the same keys in the source. Both exist because the string pin cannot see whether the block
/// actually reaches the file, and this one cannot run without the corpus.
#[test]
fn gridded_archive_states_its_reconstruction_contract() {
    let input = corpus_root().join(MZML);
    if !input.exists() {
        eprintln!("skipping: {} not present", input.display());
        return;
    }
    let dir = std::env::temp_dir().join(format!("mzpc-gridcal-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let gridded = dir.join("grid.mzpeak");
    run(&[input.to_str().unwrap(), "-o", gridded.to_str().unwrap(), "--tof-grid", "on"]);

    let f = std::fs::File::open(&gridded).unwrap();
    let mut z = zip::ZipArchive::new(f).unwrap();
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut z.by_name("mzpeak_index.json").unwrap(), &mut buf).unwrap();
    let idx: serde_json::Value = serde_json::from_slice(&buf).unwrap();
    let cal = idx
        .get("metadata")
        .and_then(|m| m.get("tof_calibration"))
        .expect("metadata.tof_calibration present in a --tof-grid archive");

    assert_eq!(cal.get("codec").and_then(|v| v.as_str()), Some("tof-grid"));
    assert_eq!(cal.get("model").and_then(|v| v.as_str()), Some("sciex_sqrt"));
    assert_eq!(
        cal.get("lossless").and_then(|v| v.as_str()),
        Some("tof_index"),
        "the block must name its exactly-stored column with the spec's `lossless` key; got {cal}"
    );
    assert_eq!(
        cal.get("mz_reconstruction").and_then(|v| v.as_str()),
        Some("bounded-lossy"),
        "the run-wide grid accepts a reconstruction within tolerance, so it is not exact; got {cal}"
    );
    assert!(
        cal.get("roundtrip_tolerance_ppm").and_then(|v| v.as_f64()).is_some_and(|v| v > 0.0),
        "a bounded-lossy block must state its bound; got {cal}"
    );
    // The two keys answer DIFFERENT questions and must not be conflated: `lossless` names the
    // column stored exactly, `mz_reconstruction` rates the m/z rebuilt from it. `integer_column`
    // was a short-lived synonym for the first and must not come back.
    assert!(
        cal.get("integer_column").is_none(),
        "`integer_column` duplicated the spec's `lossless`; got {cal}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
