//! timsTOF ims-compact: every grid row carries the vendor's exact ModelType-1 calibration
//! (corpus-gated; ~5 s on PXD059079 2485.d, dominated by the conversion itself).
//!
//! 2485.d has a single `MzCalibration` row of ModelType 1 with `C2 = C3 = C4 = dC2 = 0`, so the
//! vendor model
//!
//! ```text
//!   t_ns   = tof·DigitizerTimebase + DigitizerDelay
//!   C1_eff = C1·(1 + dC1·(T1_row − T1_frame)/1e6)
//!   m/z    = ((t_ns − C0)·√C1_eff / 1e6)²
//! ```
//!
//! is EXACTLY `m/z = (c0 + c1·tof)²` per frame. Since 0.14 the native lane writes the reference
//! implementation's chunk grid natively (no 0.12.x TOF layout, no rewrite pass): each frame's rows
//! carry the row's 7 parameters at the FRAME's `T1`/`T2` (`mz_grid`), evaluated by the reader as
//! mzdata does. The default lane must:
//!   * declare the grid in `ims_calibration` (`tof_encoding: grid`, `exact: true`, the model
//!     columns; the chord only as the fallback of a frame without a row);
//!   * carry `Frames.T1/T2/MzCalibration` on EVERY frame as `spectra_metadata` columns (provenance);
//!   * ship a non-zero `total_ion_current` / `base_peak_intensity` on every MS1 row, and keep the
//!     source's TIC/BPC chromatograms (the archive-level pin of invariants 2/3);
//!   * make the vendored reader — on the PEAKS facet, where ims-compact keeps its points
//!     (`get_spectrum_peak_arrays_for` and the collapsed `get_spectrum` peak list) — and therefore
//!     `mzpeak-convert ARCHIVE -o x.mzML` emit m/z that sits on INTEGER digitizer bins of the
//!     vendor formula at each frame's OWN `T1` (1e-12 relative), while the run-wide chord is
//!     > 1 ppm off somewhere;
//!   * write the same values with one chunk per frame (`--no-ims-chunked`).
//!
//! Needs 2485.d from the corpus, so it is `#[ignore]`d: CI reports it as not run rather than as passed.

use std::path::Path;
use std::process::Command;

use arrow::array::{Array, AsArray};
use arrow::datatypes::Float64Type;
use mzdata::io::DetailLevel;
use mzdata::prelude::*;
use mzpeak_prototyping::MzPeakReader;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

#[path = "common/corpus.rs"]
mod corpus;

const DOT_D: &str = "ims-examples/PXD059079/20230830_100SPD_NCI7_0p12ng_HS_01_S1-B1_1_2485.d";
const FRAMES: usize = 3_994;
/// `GlobalMetadata.DigitizerNumSamples` of 2485.d.
const NUM_SAMPLES: i64 = 636_031;

fn run(args: &[&str], envs: &[(&str, &str)]) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"));
    cmd.args(args);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let st = cmd.status().expect("failed to run mzpeak-convert");
    assert!(st.success(), "mzpeak-convert {args:?} failed: {st}");
}

/// The vendor ModelType-1 constants of 2485.d's single `MzCalibration` row, read from the TDF so
/// the test evaluates the formula independently of the converter.
struct Cal {
    timebase: f64,
    delay: f64,
    t1_row: f64,
    dc1: f64,
    c0: f64,
    c1: f64,
}

impl Cal {
    fn read(tdf: &Path) -> Self {
        let conn =
            rusqlite::Connection::open_with_flags(tdf, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let (n, model_type, c2, c3, c4, dc2): (i64, i64, f64, f64, f64, f64) = conn
            .query_row(
                "SELECT COUNT(*), MAX(ModelType), MAX(IFNULL(C2,0)), MAX(IFNULL(C3,0)), MAX(IFNULL(C4,0)), \
                 MAX(IFNULL(dC2,0)) FROM MzCalibration",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .unwrap();
        assert_eq!((n, model_type), (1, 1), "2485.d: one ModelType-1 MzCalibration row");
        assert_eq!((c2, c3, c4, dc2), (0.0, 0.0, 0.0, 0.0), "2485.d: sqrt-linear row (C2 = 0)");
        conn.query_row(
            "SELECT DigitizerTimebase, DigitizerDelay, T1, dC1, C0, C1 FROM MzCalibration WHERE Id = 1",
            [],
            |r| {
                Ok(Cal {
                    timebase: r.get(0)?,
                    delay: r.get(1)?,
                    t1_row: r.get(2)?,
                    dc1: r.get(3)?,
                    c0: r.get(4)?,
                    c1: r.get(5)?,
                })
            },
        )
        .unwrap()
    }

    fn c1_eff(&self, t1_frame: f64) -> f64 {
        self.c1 * (1.0 + self.dc1 * (self.t1_row - t1_frame) / 1e6)
    }

    /// The vendor formula at the frame's digitizer temperature.
    fn mz(&self, tof: f64, t1_frame: f64) -> f64 {
        let t_ns = tof * self.timebase + self.delay;
        let u = (t_ns - self.c0) * self.c1_eff(t1_frame).sqrt() / 1e6;
        u * u
    }

    /// The (fractional) digitizer bin of an m/z at the frame's temperature — the inverse of [`Self::mz`].
    fn tof(&self, mz: f64, t1_frame: f64) -> f64 {
        let t_ns = mz.sqrt() * 1e6 / self.c1_eff(t1_frame).sqrt() + self.c0;
        (t_ns - self.delay) / self.timebase
    }
}

/// Per-frame `Frames.T1` from `spectra_metadata.parquet` (the `*_tdf_t1` column), in frame order,
/// asserting every frame carries it and the `T2` / calibration-id columns exist.
fn per_frame_t1(archive: &Path, dir: &Path) -> Vec<f64> {
    let mut t1 = Vec::new();
    for b in member_batches(archive, "spectra_metadata.parquet", dir) {
        let col = |suffix: &str| {
            let name = b.schema().fields().iter().map(|f| f.name().clone()).find(|n| n.ends_with(suffix))
                .unwrap_or_else(|| panic!("no `*{suffix}` column in spectra_metadata"));
            b.column_by_name(&name).unwrap().clone()
        };
        let _ = col("_tdf_t2");
        let _ = col("_tdf_mz_calibration_id");
        let c = col("_tdf_t1");
        let a = c.as_primitive::<Float64Type>();
        for i in 0..a.len() {
            assert!(a.is_valid(i), "row {}: NULL tdf_t1", t1.len());
            t1.push(a.value(i));
        }
    }
    t1
}

fn ims_calibration(archive: &Path) -> serde_json::Value {
    let f = std::fs::File::open(archive).unwrap();
    let mut z = zip::ZipArchive::new(f).unwrap();
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut z.by_name("mzpeak_index.json").unwrap(), &mut buf).unwrap();
    let idx: serde_json::Value = serde_json::from_slice(&buf).unwrap();
    idx["metadata"]["ims_calibration"].clone()
}

fn member_batches(archive: &Path, member: &str, dir: &Path) -> Vec<arrow::record_batch::RecordBatch> {
    let f = std::fs::File::open(archive).unwrap();
    let mut z = zip::ZipArchive::new(f).unwrap();
    let mut e = z.by_name(member).unwrap_or_else(|_| panic!("{member} missing"));
    let out = dir.join(format!("{}-{member}", archive.file_name().unwrap().to_string_lossy()));
    let mut o = std::fs::File::create(&out).unwrap();
    std::io::copy(&mut e, &mut o).unwrap();
    ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(&out).unwrap())
        .unwrap()
        .with_batch_size(1 << 16)
        .build()
        .unwrap()
        .map(|b| b.unwrap())
        .collect()
}

/// `(total_ion_current, base_peak_intensity)` of every MS1 row of `spectra_metadata`.
fn ms1_summary_columns(archive: &Path, dir: &Path) -> Vec<(Option<f32>, Option<f32>)> {
    use arrow::datatypes::{Float32Type, UInt8Type};
    let mut out = Vec::new();
    for b in member_batches(archive, "spectra_metadata.parquet", dir) {
        let level = b.column_by_name("ms_level").unwrap().as_primitive::<UInt8Type>();
        let tic = b.column_by_name("total_ion_current").unwrap().as_primitive::<Float32Type>();
        let bpi = b.column_by_name("base_peak_intensity").unwrap().as_primitive::<Float32Type>();
        for i in 0..b.num_rows() {
            if level.value(i) == 1 {
                out.push((tic.is_valid(i).then(|| tic.value(i)), bpi.is_valid(i).then(|| bpi.value(i))));
            }
        }
    }
    out
}

fn chromatogram_ids(archive: &Path, dir: &Path) -> Vec<String> {
    let mut ids = Vec::new();
    for b in member_batches(archive, "chromatograms_metadata.parquet", dir) {
        let col = b.column_by_name("id").unwrap();
        let col = arrow::compute::cast(col, &arrow::datatypes::DataType::Utf8).unwrap();
        ids.extend(col.as_string::<i32>().iter().flatten().map(str::to_string));
    }
    ids
}

fn rel(a: f64, b: f64) -> f64 {
    ((a - b) / b).abs()
}

/// Every m/z of `mz` sits on an integer bin of the vendor formula at `t1` (< 1e-3 bins off, and
/// the integer bin re-evaluates to it within 1e-12); returns the worst distance to the chord in ppm.
fn assert_on_vendor_lattice(mz: &[f64], vendor: &Cal, t1: f64, chord: &dyn Fn(f64) -> f64, what: &str) -> f64 {
    let mut vs_chord = 0.0f64;
    for m in mz {
        let k = vendor.tof(*m, t1);
        assert!((k - k.round()).abs() < 1e-3, "{what}: m/z {m} is {k} bins — not on the integer lattice");
        assert!((0.0..NUM_SAMPLES as f64).contains(&k.round()), "{what}: bin {k} outside the digitizer range");
        let exact = vendor.mz(k.round(), t1);
        assert!(rel(*m, exact) < 1e-12, "{what}: reader {m} vs vendor {exact} at bin {k}");
        vs_chord = vs_chord.max(rel(*m, chord(k.round())) * 1e6);
    }
    vs_chord
}

#[test]
#[ignore = "needs the 142 MB 2485.d timsTOF corpus fixture (MZPEAK_CORPUS); run with --include-ignored"]
fn ims_compact_carries_the_exact_vendor_model_on_every_grid_row() {
    let Some(dot_d) = corpus::corpus_path(DOT_D) else { return };
    let tdf = dot_d.join("analysis.tdf");
    let tmp = std::env::temp_dir().join(format!("mzpc-exacttof-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let archive = tmp.join("2485.mzpeak");
    run(&[dot_d.to_str().unwrap(), "-o", archive.to_str().unwrap(), "--force", "--no-vendor"], &[]);

    // ims_calibration: the grid declared, exact; the chord only as the fallback model.
    let cal = ims_calibration(&archive);
    assert_eq!(cal["tof_encoding"], "grid", "{cal}");
    assert_eq!(cal["exact"], true, "{cal}");
    assert_eq!(cal["mz_grid"]["column"], "chunk.mz_grid", "{cal}");
    assert_eq!(cal["ion_mobility_grid"]["column"], "chunk.mean_inverse_reduced_ion_mobility_grid", "{cal}");
    assert!(cal.get("per_spectrum").is_none() && cal.get("lossless").is_none(), "0.13 keys are gone: {cal}");
    assert_eq!(cal["chunk_width_th"], 50.0, "{cal}");
    // Invariant 2/3 at the ARCHIVE level: every MS1 row must carry a real TIC and base-peak
    // intensity — the published corpus once shipped `total_ion_current = 0` on every gridded spectrum.
    let ms1 = ms1_summary_columns(&archive, &tmp);
    assert!(!ms1.is_empty(), "no MS1 rows in spectra_metadata");
    for (i, (tic, bpi)) in ms1.iter().enumerate() {
        assert!(tic.is_some_and(|v| v > 0.0), "MS1 row {i}: total_ion_current is {tic:?}, expected > 0");
        assert!(bpi.is_some_and(|v| v > 0.0), "MS1 row {i}: base_peak_intensity is {bpi:?}, expected > 0");
    }
    // Since 0.12.4 the run's own traces are stored and a summed TIC/BPC is added only for a kind the
    // source lacks. 2485.d carries both, so nothing is synthesized here.
    let ids = chromatogram_ids(&archive, &tmp);
    for want in ["TIC,±MS", "BPC,±MS"] {
        assert!(ids.iter().any(|i| i == want), "the source's {want} trace is missing: {ids:?}");
    }
    assert!(!ids.iter().any(|i| i == "TIC" || i == "BPC"), "a summed TIC/BPC was added although the source carries both kinds: {ids:?}");

    let (a, b) = (cal["chord"]["a"].as_f64().unwrap(), cal["chord"]["b"].as_f64().unwrap());
    let chord = |tof: f64| (a + b * tof).powi(2);

    // Frames.T1 on all 3,994 frames, and it differs from the row's reference T1 (else the
    // temperature term is untested).
    let t1s = per_frame_t1(&archive, &tmp);
    assert_eq!(t1s.len(), FRAMES);
    let vendor = Cal::read(&tdf);
    assert!(t1s.iter().any(|t| *t != vendor.t1_row), "every frame's T1 equals the row's reference T1; test is vacuous");

    // The vendored reader (the `mzpeak-convert ARCHIVE` input path) reconstructs m/z from the grid
    // rows — the vendor formula at the frame's own T1, on integer bins — and NOT from the chord.
    let mut reader = MzPeakReader::new(&archive).unwrap();
    reader.set_detail_level(DetailLevel::Full);
    assert_eq!(reader.len(), FRAMES);
    let (mut reader_vs_chord, mut checked) = (0.0f64, 0usize);
    let mut frames_mz: Vec<(usize, Vec<f64>)> = Vec::new();
    for i in [0usize, 1, 977, 2500, FRAMES - 1] {
        let arrays = reader
            .get_spectrum_peak_arrays_for(i as u64)
            .unwrap()
            .unwrap_or_else(|| panic!("spectrum {i}: no peak-facet arrays"));
        let mz = arrays.mzs().unwrap().to_vec();
        assert!(!mz.is_empty(), "spectrum {i}: empty");
        reader_vs_chord = reader_vs_chord.max(assert_on_vendor_lattice(&mz, &vendor, t1s[i], &chord, &format!("spectrum {i}")));
        checked += mz.len();
        // The collapsed spectrum carries exactly those m/z values (as a multiset).
        let spec = reader.get_spectrum(i).unwrap_or_else(|| panic!("spectrum {i}"));
        let mut from_spec: Vec<f64> = spec
            .peaks
            .as_ref()
            .unwrap_or_else(|| panic!("spectrum {i}: get_spectrum yielded no peak list"))
            .iter()
            .map(|p| p.mz)
            .collect();
        let mut from_arrays = mz.clone();
        from_spec.sort_by(|a, b| a.total_cmp(b));
        from_arrays.sort_by(|a, b| a.total_cmp(b));
        assert_eq!(from_spec.len(), from_arrays.len(), "spectrum {i}: peak count");
        for (a, b) in from_spec.iter().zip(from_arrays.iter()) {
            assert!(rel(*a, *b) < 1e-12, "spectrum {i}: get_spectrum m/z {a} vs peak-facet arrays {b}");
        }
        frames_mz.push((i, mz));
    }
    assert!(checked > 1000, "only {checked} points checked");
    assert!(reader_vs_chord > 1.0, "reader m/z is within {reader_vs_chord} ppm of the chord — the vendor model is not live");
    eprintln!("reader: {checked} points on the vendor lattice; chord up to {reader_vs_chord:.2} ppm away");

    // mzML export of the archive carries the exact m/z (first 3 frames).
    let mzml = tmp.join("2485.mzML");
    run(&[archive.to_str().unwrap(), "-o", mzml.to_str().unwrap(), "--force"], &[("MZPC_MAX_SPECTRA", "3")]);
    let (mut n_mzml, mut mzml_vs_chord) = (0usize, 0.0f64);
    for (i, spec) in mzdata::MZReader::open_path(&mzml).unwrap().enumerate() {
        let mz: Vec<f64> = match spec.peaks.as_ref() {
            Some(p) => p.iter().map(|p| p.mz).collect(),
            None => spec.arrays.as_ref().unwrap_or_else(|| panic!("mzML spectrum {i}: no peaks and no arrays")).mzs().unwrap().to_vec(),
        };
        assert!(!mz.is_empty(), "mzML spectrum {i}: empty");
        mzml_vs_chord = mzml_vs_chord.max(assert_on_vendor_lattice(&mz, &vendor, t1s[i], &chord, &format!("mzML spectrum {i}")));
        n_mzml += mz.len();
    }
    assert!(n_mzml > 100, "mzML export checked only {n_mzml} points");
    assert!(mzml_vs_chord > 1.0, "mzML m/z is within {mzml_vs_chord} ppm of the chord");
    eprintln!("mzML: {n_mzml} points on the vendor lattice; chord up to {mzml_vs_chord:.2} ppm away");
    drop(reader);

    // `--no-ims-chunked`: one chunk per frame, the same values on the same frames.
    let whole = tmp.join("2485.frame-chunks.mzpeak");
    run(&[dot_d.to_str().unwrap(), "-o", whole.to_str().unwrap(), "--force", "--no-vendor", "--no-ims-chunked"], &[]);
    let cal = ims_calibration(&whole);
    assert_eq!(cal["tof_encoding"], "grid", "{cal}");
    assert!(cal["chunk_width_th"].as_f64().unwrap() >= 1e5, "one chunk per frame: {cal}");
    let mut reader = MzPeakReader::new(&whole).unwrap();
    reader.set_detail_level(DetailLevel::Full);
    for (i, want) in &frames_mz {
        let arrays = reader.get_spectrum_peak_arrays_for(*i as u64).unwrap().unwrap_or_else(|| panic!("frame-chunk spectrum {i}: no arrays"));
        assert_eq!(arrays.mzs().unwrap().as_ref(), want.as_slice(), "spectrum {i}: one chunk per frame must store the same values");
    }
    let _ = std::fs::remove_dir_all(&tmp);
}
