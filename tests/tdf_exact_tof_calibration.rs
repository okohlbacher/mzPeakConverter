//! timsTOF ims-compact: exact per-frame `tof_c0`/`tof_c1` on a sqrt-linear vendor calibration
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
//! is EXACTLY `m/z = (c0 + c1·tof)²` per frame. The default (native timsrust) lane must:
//!   * declare it in `ims_calibration` (`per_spectrum`, `exact_per_spectrum`; `a`/`b` and
//!     `exact: false` kept for legacy readers);
//!   * carry the pair on EVERY frame as `spectra_metadata` columns `…_tof_c0` / `…_tof_c1`;
//!   * reproduce the vendor formula from the pair to 1e-12 relative (50 frames × 10 tof values,
//!     each with its OWN `Frames.T1`), while the run-wide chord is > 1 ppm off somewhere;
//!   * make the vendored reader — on the PEAKS facet, where ims-compact keeps its points
//!     (`get_spectrum_peak_arrays_for` and the collapsed `get_spectrum` peak list) — and therefore
//!     `mzpeak-convert ARCHIVE -o x.mzML` emit the per-frame m/z, not the chord.
//!
//! Skips (passes) when the reference `.d` is absent; override the corpus root with `MZPEAK_CORPUS`.

use std::path::{Path, PathBuf};
use std::process::Command;

use arrow::array::{Array, AsArray};
use arrow::datatypes::{Float64Type, Int64Type};
use mzdata::io::DetailLevel;
use mzdata::prelude::*;
use mzdata::spectrum::bindata::{ArrayType, BinaryDataArrayType};
use mzpeak_prototyping::MzPeakReader;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

const DOT_D: &str = "ims-examples/PXD059079/20230830_100SPD_NCI7_0p12ng_HS_01_S1-B1_1_2485.d";
const FRAMES: usize = 3_994;
/// `GlobalMetadata.DigitizerNumSamples` of 2485.d.
const NUM_SAMPLES: i64 = 636_031;

fn corpus_root() -> PathBuf {
    std::env::var("MZPEAK_CORPUS").map(PathBuf::from).unwrap_or_else(|_| {
        PathBuf::from(std::env::var("HOME").unwrap_or_default()).join("Claude/mzpeak-example-data/data")
    })
}

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

    /// The vendor formula at the frame's digitizer temperature.
    fn mz(&self, tof: f64, t1_frame: f64) -> f64 {
        let t_ns = tof * self.timebase + self.delay;
        let c1_eff = self.c1 * (1.0 + self.dc1 * (self.t1_row - t1_frame) / 1e6);
        let u = (t_ns - self.c0) * c1_eff.sqrt() / 1e6;
        u * u
    }
}

/// Per-frame `(tof_c0, tof_c1, Frames.T1)` from `spectra_metadata.parquet`, in frame order,
/// asserting every frame carries the pair (no nulls) and the calibration-id column.
fn per_frame_pairs(archive: &Path, dir: &Path) -> Vec<(f64, f64, f64)> {
    let f = std::fs::File::open(archive).unwrap();
    let mut z = zip::ZipArchive::new(f).unwrap();
    let mut e = z.by_name("spectra_metadata.parquet").unwrap();
    let out = dir.join("spectra_metadata.parquet");
    std::io::copy(&mut e, &mut std::fs::File::create(&out).unwrap()).unwrap();
    let rdr = ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(&out).unwrap())
        .unwrap()
        .with_batch_size(1 << 16)
        .build()
        .unwrap();
    let mut rows = Vec::new();
    for b in rdr {
        let b = b.unwrap();
        let col = |suffix: &str| {
            b.schema()
                .fields()
                .iter()
                .position(|f| f.name().ends_with(suffix))
                .map(|i| b.column(i).clone())
                .unwrap_or_else(|| panic!("no spectra_metadata column ending in {suffix}: {:?}", b.schema()))
        };
        let c0 = col("_tof_c0");
        let c1 = col("_tof_c1");
        let t1 = col("_tdf_t1");
        let id = col("_tdf_mz_calibration_id");
        let (c0, c1, t1) = (
            c0.as_primitive::<Float64Type>(),
            c1.as_primitive::<Float64Type>(),
            t1.as_primitive::<Float64Type>(),
        );
        let id = id.as_primitive::<Int64Type>();
        for i in 0..b.num_rows() {
            assert!(!c0.is_null(i) && !c1.is_null(i), "row {}: tof_c0/tof_c1 null", rows.len());
            assert!(!t1.is_null(i), "row {}: tdf_t1 null", rows.len());
            assert_eq!(id.value(i), 1, "row {}: MzCalibration id", rows.len());
            rows.push((c0.value(i), c1.value(i), t1.value(i)));
        }
    }
    rows
}

fn ims_calibration(archive: &Path) -> serde_json::Value {
    let f = std::fs::File::open(archive).unwrap();
    let mut z = zip::ZipArchive::new(f).unwrap();
    let mut e = z.by_name("mzpeak_index.json").unwrap();
    let mut s = String::new();
    std::io::Read::read_to_string(&mut e, &mut s).unwrap();
    let v: serde_json::Value = serde_json::from_str(&s).unwrap();
    v["metadata"]["ims_calibration"].clone()
}

/// The integer `tof` array of a decoded spectrum. The reader hands the ims-compact grid column
/// back as a non-standard Int32 array whose name may be empty, so locate it by kind + dtype.
fn tof_of(arrays: &mzdata::spectrum::BinaryArrayMap, what: &str) -> Vec<i32> {
    let mut found = None;
    for (k, da) in arrays.iter() {
        if matches!(k, ArrayType::NonStandardDataArray { .. }) && da.dtype == BinaryDataArrayType::Int32 {
            assert!(found.is_none(), "{what}: several Int32 non-standard arrays");
            found = Some(da.to_i32().unwrap().to_vec());
        }
    }
    found.unwrap_or_else(|| {
        panic!("{what}: no Int32 tof array among {:?}", arrays.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>())
    })
}

fn rel(a: f64, b: f64) -> f64 {
    (a - b).abs() / b.abs()
}

#[test]
fn ims_compact_carries_exact_per_frame_tof_coefficients_on_a_c2_zero_tdf() {
    let dot_d = corpus_root().join(DOT_D);
    let tdf = dot_d.join("analysis.tdf");
    if !tdf.exists() {
        eprintln!("skipping: {} not present", dot_d.display());
        return;
    }
    let tmp = std::env::temp_dir().join(format!("mzpc-exacttof-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let archive = tmp.join("2485.mzpeak");
    run(&[dot_d.to_str().unwrap(), "-o", archive.to_str().unwrap(), "--force", "--no-vendor"], &[]);

    // ims_calibration: per-spectrum declared, legacy chord kept.
    let cal = ims_calibration(&archive);
    assert_eq!(cal["per_spectrum"], "tof_c0,tof_c1", "{cal}");
    assert_eq!(cal["exact_per_spectrum"], true, "{cal}");
    assert_eq!(cal["exact"], false, "run-wide a/b stay approximate: {cal}");
    assert!(cal["per_spectrum_note"].as_str().is_some_and(|s| s.contains("C2 = 0")), "{cal}");
    assert!(
        cal.get("per_spectrum_chord_frames").is_none(),
        "2485.d has no NULL Frames.T1, so no frame stays on the chord: {cal}"
    );
    let (a, b) = (cal["a"].as_f64().unwrap(), cal["b"].as_f64().unwrap());
    let chord = |tof: f64| (a + b * tof).powi(2);

    // The pair on all 3,994 frames.
    let pairs = per_frame_pairs(&archive, &tmp);
    assert_eq!(pairs.len(), FRAMES);

    // 50 frames × 10 tof values: the pair reproduces the vendor formula at the FRAME's T1 to 1e-12,
    // the chord does not (> 1 ppm somewhere). The frames' T1 differ from the row's T1, so a pair
    // that dropped the temperature term would miss by ~1e-7 (dC1·ΔT/1e6 ≈ 20·4e-3/1e6 / 2).
    let vendor = Cal::read(&tdf);
    let tofs: Vec<f64> = (0..10).map(|j| (j as f64 * (NUM_SAMPLES - 1) as f64 / 9.0).round()).collect();
    let (mut worst_pair, mut worst_chord_ppm, mut worst_no_temp) = (0.0f64, 0.0f64, 0.0f64);
    for k in 0..50 {
        let i = k * (FRAMES - 1) / 49;
        let (c0, c1, t1) = pairs[i];
        assert_ne!(t1, vendor.t1_row, "frame {i}: T1 equals the row's reference T1; test is vacuous");
        for &tof in &tofs {
            let model = vendor.mz(tof, t1);
            assert!(model.is_finite() && model > 0.0);
            let lin = (c0 + c1 * tof).powi(2);
            worst_pair = worst_pair.max(rel(lin, model));
            worst_chord_ppm = worst_chord_ppm.max(rel(chord(tof), model) * 1e6);
            worst_no_temp = worst_no_temp.max(rel(vendor.mz(tof, vendor.t1_row), model));
        }
    }
    assert!(worst_pair < 1e-12, "pair vs vendor formula: {worst_pair:e} relative");
    assert!(worst_chord_ppm > 1.0, "chord is only {worst_chord_ppm} ppm off — the exact path proves nothing here");
    assert!(worst_no_temp > 1e-9, "temperature term is inert on this file ({worst_no_temp:e}); test is vacuous");
    eprintln!("pair vs model {worst_pair:e} rel; chord {worst_chord_ppm:.2} ppm; temp term {worst_no_temp:e} rel");

    // The vendored reader (the `mzpeak-convert ARCHIVE` input path) reconstructs m/z from the
    // per-spectrum pair — equal to (c0 + c1·tof)² to 1e-12 — and NOT from the chord. ims-compact
    // stores its points in the PEAKS facet, so the check runs on the peak-facet arrays (where the
    // integer `tof` is still alongside the reconstructed m/z) and then on the collapsed spectrum
    // `get_spectrum` hands to every consumer (a centroid set: same m/z values, possibly re-sorted).
    let mut reader = MzPeakReader::new(&archive).unwrap();
    reader.set_detail_level(DetailLevel::Full);
    assert_eq!(reader.len(), FRAMES);
    let mut reader_vs_chord = 0.0f64;
    let mut checked = 0usize;
    for i in [0usize, 1, 977, 2500, FRAMES - 1] {
        let arrays = reader
            .get_spectrum_peak_arrays_for(i as u64)
            .unwrap()
            .unwrap_or_else(|| panic!("spectrum {i}: no peak-facet arrays"));
        let tof = tof_of(&arrays, &format!("spectrum {i}"));
        let mz = arrays.mzs().unwrap();
        assert_eq!(tof.len(), mz.len(), "spectrum {i}");
        assert!(!mz.is_empty(), "spectrum {i}: empty");
        let (c0, c1, t1) = pairs[i];
        for (t, m) in tof.iter().zip(mz.iter()) {
            let exact = (c0 + c1 * *t as f64).powi(2);
            assert!(rel(*m, exact) < 1e-12, "spectrum {i} tof {t}: reader {m} vs exact {exact}");
            assert!(rel(*m, vendor.mz(*t as f64, t1)) < 1e-12, "spectrum {i} tof {t}: reader {m} vs vendor");
            reader_vs_chord = reader_vs_chord.max(rel(*m, chord(*t as f64)) * 1e6);
            checked += 1;
        }
        // The collapsed spectrum carries exactly those m/z values (as a multiset).
        let spec = reader.get_spectrum(i).unwrap_or_else(|| panic!("spectrum {i}"));
        let mut from_spec: Vec<f64> = spec
            .peaks
            .as_ref()
            .unwrap_or_else(|| panic!("spectrum {i}: get_spectrum yielded no peak list"))
            .iter()
            .map(|p| p.mz)
            .collect();
        let mut from_arrays: Vec<f64> = mz.to_vec();
        from_spec.sort_by(|a, b| a.total_cmp(b));
        from_arrays.sort_by(|a, b| a.total_cmp(b));
        assert_eq!(from_spec.len(), from_arrays.len(), "spectrum {i}: peak count");
        for (a, b) in from_spec.iter().zip(from_arrays.iter()) {
            assert!(rel(*a, *b) < 1e-12, "spectrum {i}: get_spectrum m/z {a} vs peak-facet arrays {b}");
        }
    }
    assert!(checked > 1000, "only {checked} points checked");
    assert!(reader_vs_chord > 1.0, "reader m/z is within {reader_vs_chord} ppm of the chord — the per-spectrum path is not live");
    eprintln!("reader: {checked} points on the exact pair; chord up to {reader_vs_chord:.2} ppm away");

    // mzML export of the archive carries the exact m/z (first 3 frames). The mzML holds m/z +
    // intensity only (the integer `tof` does not survive the peak-list collapse), so invert each
    // m/z through the frame's pair: an exact-lane m/z lands on an INTEGER tof to < 1e-3 bins and
    // round-trips to < 1e-9 relative; a chord m/z is > 1 ppm (≈ 0.8 bins) off the same lattice.
    let mzml = tmp.join("2485.mzML");
    run(&[archive.to_str().unwrap(), "-o", mzml.to_str().unwrap(), "--force"], &[("MZPC_MAX_SPECTRA", "3")]);
    let mut n_mzml = 0usize;
    let mut mzml_vs_chord = 0.0f64;
    let mut worst_bin_offset = 0.0f64;
    for (i, spec) in mzdata::MZReader::open_path(&mzml).unwrap().enumerate() {
        let (c0, c1, _) = pairs[i];
        let mz: Vec<f64> = match spec.peaks.as_ref() {
            Some(p) => p.iter().map(|p| p.mz).collect(),
            None => spec
                .arrays
                .as_ref()
                .unwrap_or_else(|| panic!("mzML spectrum {i}: no peaks and no arrays"))
                .mzs()
                .unwrap()
                .to_vec(),
        };
        assert!(!mz.is_empty(), "mzML spectrum {i}: empty");
        for m in &mz {
            let k = (m.sqrt() - c0) / c1;
            worst_bin_offset = worst_bin_offset.max((k - k.round()).abs());
            let exact = (c0 + c1 * k.round()).powi(2);
            assert!(rel(*m, exact) < 1e-9, "mzML spectrum {i}: {m} is not on the exact lattice (tof {k})");
            mzml_vs_chord = mzml_vs_chord.max(rel(*m, chord(k.round())) * 1e6);
            n_mzml += 1;
        }
    }
    assert!(n_mzml > 100, "mzML export checked only {n_mzml} points");
    assert!(worst_bin_offset < 1e-3, "mzML m/z off the integer tof lattice by {worst_bin_offset} bins");
    assert!(mzml_vs_chord > 1.0, "mzML m/z is within {mzml_vs_chord} ppm of the chord");
    eprintln!("mzML: {n_mzml} points on the exact lattice (worst {worst_bin_offset:e} bins); chord up to {mzml_vs_chord:.2} ppm away");
    drop(reader);

    // `--ims-chunked`: the same pair on the same frames, and the CHUNKED peaks facet (the chunk
    // reader branch of the per-spectrum fixup) reconstructs from it too.
    let chunked = tmp.join("2485.chunked.mzpeak");
    run(
        &[dot_d.to_str().unwrap(), "-o", chunked.to_str().unwrap(), "--force", "--no-vendor", "--ims-chunked"],
        &[],
    );
    let cal = ims_calibration(&chunked);
    assert_eq!(cal["per_spectrum"], "tof_c0,tof_c1", "{cal}");
    assert_eq!(cal["exact_per_spectrum"], true, "{cal}");
    assert_eq!(cal["chunk_bounds"], "mz", "{cal}");
    let chunked_pairs = per_frame_pairs(&chunked, &tmp);
    assert_eq!(chunked_pairs, pairs, "chunked and flat layouts carry the same per-frame pair");
    let mut reader = MzPeakReader::new(&chunked).unwrap();
    reader.set_detail_level(DetailLevel::Full);
    let (mut n_chunked, mut chunked_vs_chord) = (0usize, 0.0f64);
    for i in [0usize, 977, FRAMES - 1] {
        let arrays = reader
            .get_spectrum_peak_arrays_for(i as u64)
            .unwrap()
            .unwrap_or_else(|| panic!("chunked spectrum {i}: no peak-facet arrays"));
        let tof = tof_of(&arrays, &format!("chunked spectrum {i}"));
        let mz = arrays.mzs().unwrap();
        assert_eq!(tof.len(), mz.len(), "chunked spectrum {i}");
        assert!(!mz.is_empty(), "chunked spectrum {i}: empty");
        let (c0, c1, _) = pairs[i];
        for (t, m) in tof.iter().zip(mz.iter()) {
            let exact = (c0 + c1 * *t as f64).powi(2);
            assert!(rel(*m, exact) < 1e-12, "chunked spectrum {i} tof {t}: reader {m} vs exact {exact}");
            chunked_vs_chord = chunked_vs_chord.max(rel(*m, chord(*t as f64)) * 1e6);
            n_chunked += 1;
        }
    }
    assert!(n_chunked > 1000, "chunked: only {n_chunked} points checked");
    assert!(chunked_vs_chord > 1.0, "chunked reader m/z is within {chunked_vs_chord} ppm of the chord");
    eprintln!("chunked: {n_chunked} points on the exact pair; chord up to {chunked_vs_chord:.2} ppm away");
    let _ = std::fs::remove_dir_all(&tmp);
}
