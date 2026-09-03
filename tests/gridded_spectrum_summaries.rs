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
//! possible assertion: the SAME input converted with and without the grid must produce the SAME
//! summary columns. A gridded archive is not allowed to be a worse description of its own data.
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

    // 2. The stronger statement: gridding must not change what the summary SAYS. Both lanes see the
    //    same f64 (m/z, intensity); the grid lane just has to state it explicitly instead of letting
    //    the writer derive it. Exact equality — these are the same numbers by construction.
    for i in 0..SPECTRA {
        assert_eq!(g.tic[i], f.tic[i], "spectrum {i}: total_ion_current differs between lanes");
        assert_eq!(
            g.bp_int[i], f.bp_int[i],
            "spectrum {i}: base_peak_intensity differs between lanes"
        );
        // base_peak_mz may legitimately differ on an INTENSITY TIE: the grid lane resolves ties to
        // the lowest m/z, mzdata's derived summary resolves them first-in-array. Both name a real
        // maximum, so the height must match exactly (asserted above) and the m/z is only allowed to
        // move when there is a tie to move within.
        if g.bp_mz[i] != f.bp_mz[i] {
            let (gm, fm) = (g.bp_mz[i].unwrap(), f.bp_mz[i].unwrap());
            assert!(
                gm <= fm,
                "spectrum {i}: base_peak_mz {gm} > {fm}; a tie must resolve to the LOWER m/z"
            );
        }
        assert_eq!(g.lo_mz[i], f.lo_mz[i], "spectrum {i}: lowest_observed_mz differs between lanes");
        assert_eq!(g.hi_mz[i], f.hi_mz[i], "spectrum {i}: highest_observed_mz differs between lanes");
    }

    let _ = std::fs::remove_dir_all(&dir);
}
