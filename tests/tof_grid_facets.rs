//! M6 — a TOF-grid archive files each spectrum by the representation its SOURCE declares, and the
//! integer axis is declared on both facets:
//!
//! * a PROFILE spectrum on the lattice → `spectra_data`, `tof_index` set, `mz` NULL,
//!   `spectrum_representation = MS:1000128`, `number_of_data_points` set;
//! * a CENTROID spectrum on the lattice → `spectra_peaks`, `tof_index` set, `mz` NULL,
//!   `MS:1000127`, `number_of_peaks` set;
//! * a CENTROID spectrum off the lattice → `spectra_peaks`, `mz` set (exact f64), `tof_index` NULL.
//!
//! Until 0.10.0 every gridded spectrum was forced to Centroid to reach the one facet that knew the
//! axis, and every off-grid one to Profile — the representation was a routing knob. The mzML export
//! of the archive must hand back m/z + intensity only (the raw axis leaked as a third, nameless
//! `MS:1000786` array), with intensities identical and m/z within the declared 5 ppm bound.
//!
//! Self-contained: the input mzML is synthesized with mzdata's writer, so no corpus is needed. The
//! sqrt-space step is forced with `MZPC_TOF_GRID_C1` (the fitter's inference wants real detector
//! spacing statistics; the forced path still gates every point at the ppm bound).

use std::fs::File;
use std::path::Path;
use std::process::Command;

use arrow::array::{Array, AsArray};
use arrow::datatypes::UInt64Type;
use mzdata::io::mzml::MzMLWriter;
use mzdata::mzpeaks::{CentroidPeak, DeconvolutedPeak};
use mzdata::prelude::*;
use mzdata::spectrum::bindata::{ArrayType, BinaryDataArrayType, DataArray};
use mzdata::spectrum::{BinaryArrayMap, MultiLayerSpectrum, SignalContinuity, SpectrumDescription};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

const C0: f64 = 10.0;
// Coarse enough that the half-step quantization at m/z ~137 exceeds the 5 ppm bound, so the
// off-lattice spectra really are off the lattice (at a fine step any m/z snaps within tolerance).
const C1: f64 = 1.0e-4;
const N_PROFILE: usize = 6;
/// Index of the on-lattice centroid spectrum; the three after it are off-lattice centroids.
const CENTROID_ON: usize = N_PROFILE;
const N_TOTAL: usize = N_PROFILE + 4;

fn mz_of(k: i64) -> f64 {
    let r = C0 + C1 * k as f64;
    r * r
}

/// The synthetic run: (m/z, intensity, continuity) per spectrum, in index order.
fn synthetic() -> Vec<(Vec<f64>, Vec<f32>, SignalContinuity)> {
    let mut out = Vec::new();
    for s in 0..N_PROFILE {
        // 200 lattice points per profile spectrum, a different window each (k ≈ 4e4…4.8e4, m/z
        // ~196–225), no zero runs so the writer's zero-run compaction cannot change the point count.
        let mz: Vec<f64> = (0..200).map(|i| mz_of(40_000 + 1_200 * s as i64 + 5 * i)).collect();
        let it: Vec<f32> = (0..200).map(|i| 1.0 + ((i * 7 + s) % 13) as f32).collect();
        out.push((mz, it, SignalContinuity::Profile));
    }
    let mz: Vec<f64> = (0..50).map(|i| mz_of(42_000 + 11 * i)).collect();
    out.push((mz, vec![3.0; 50], SignalContinuity::Centroid));
    for s in 0..3 {
        let mz: Vec<f64> = (0..30).map(|i| 137.0 + 0.131 * i as f64 + 0.017 * (i as f64 + s as f64).sin()).collect();
        out.push((mz, vec![2.0 + s as f32; 30], SignalContinuity::Centroid));
    }
    assert_eq!(out.len(), N_TOTAL);
    out
}

fn write_mzml(path: &Path) {
    let mut w = MzMLWriter::new(File::create(path).unwrap());
    w.set_spectrum_count(N_TOTAL as u64);
    for (i, (mz, it, cont)) in synthetic().into_iter().enumerate() {
        let mut arrays = BinaryArrayMap::new();
        let mut a = DataArray::wrap(&ArrayType::MZArray, BinaryDataArrayType::Float64, Vec::new());
        a.update_buffer(&mz).unwrap();
        arrays.add(a);
        let mut b = DataArray::wrap(&ArrayType::IntensityArray, BinaryDataArrayType::Float32, Vec::new());
        b.update_buffer(&it).unwrap();
        arrays.add(b);
        let mut d = SpectrumDescription::default();
        d.index = i;
        d.id = format!("scan={}", i + 1);
        d.ms_level = if cont == SignalContinuity::Profile { 1 } else { 2 };
        d.signal_continuity = cont;
        let spec: MultiLayerSpectrum<CentroidPeak, DeconvolutedPeak> = MultiLayerSpectrum::new(d, Some(arrays), None, None);
        w.write(&spec).unwrap();
    }
    w.close().unwrap();
}

fn run(args: &[&Path], env: &[(&str, &str)]) {
    let mut c = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"));
    c.args(args).arg("--force");
    for (k, v) in env {
        c.env(k, v);
    }
    let out = c.output().expect("failed to run mzpeak-convert");
    assert!(out.status.success(), "mzpeak-convert {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
}

fn member(archive: &Path, name: &str, dir: &Path) -> Vec<arrow::record_batch::RecordBatch> {
    let mut z = zip::ZipArchive::new(File::open(archive).unwrap()).unwrap();
    let Ok(mut e) = z.by_name(name) else { return Vec::new() };
    let p = dir.join(name);
    std::io::copy(&mut e, &mut File::create(&p).unwrap()).unwrap();
    ParquetRecordBatchReaderBuilder::try_new(File::open(&p).unwrap())
        .unwrap()
        .with_batch_size(1 << 16)
        .build()
        .unwrap()
        .map(|b| b.unwrap())
        .collect()
}

/// Per spectrum index: (rows, rows with non-null `tof_index`, rows with non-null `mz`) in a facet.
fn facet_rows(archive: &Path, name: &str, dir: &Path) -> std::collections::BTreeMap<u64, (usize, usize, usize)> {
    let mut m = std::collections::BTreeMap::new();
    for b in member(archive, name, dir) {
        let st = b
            .columns()
            .iter()
            .filter_map(|c| c.as_struct_opt())
            .find(|st| st.column_by_name("spectrum_index").is_some())
            .expect("a struct column with spectrum_index");
        let idx = st.column_by_name("spectrum_index").unwrap().as_primitive::<UInt64Type>();
        let tof = st.column_by_name("tof_index").expect("both facets declare tof_index");
        let mz = st.column_by_name("mz").expect("both facets declare an f64 mz");
        for i in 0..st.len() {
            let e = m.entry(idx.value(i)).or_insert((0, 0, 0));
            e.0 += 1;
            e.1 += tof.is_valid(i) as usize;
            e.2 += mz.is_valid(i) as usize;
        }
    }
    m
}

#[test]
fn facet_follows_the_declared_representation_and_the_export_is_clean() {
    let dir = std::env::temp_dir().join(format!("mzpc-tofgrid-facets-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("synthetic.mzML");
    let archive = dir.join("synthetic.mzpeak");
    let exported = dir.join("exported.mzML");
    write_mzml(&src);
    run(&[&src, Path::new("-o"), &archive, Path::new("--tof-grid"), Path::new("on")], &[("MZPC_TOF_GRID_C1", "1e-4")]);

    // --- metadata: the representation is the source's, and the count column follows it ---
    let mut rep: Vec<Option<String>> = vec![None; N_TOTAL];
    let mut n_pts: Vec<Option<u64>> = vec![None; N_TOTAL];
    let mut n_pks: Vec<Option<u64>> = vec![None; N_TOTAL];
    for b in member(&archive, "spectra_metadata.parquet", &dir) {
        let index = b.column_by_name("index").unwrap().as_primitive::<UInt64Type>();
        let r = b.column_by_name("spectrum_representation").unwrap();
        let r = r.as_string_opt::<i32>().map(|a| a.iter().map(|v| v.map(str::to_string)).collect::<Vec<_>>())
            .or_else(|| r.as_string_opt::<i64>().map(|a| a.iter().map(|v| v.map(str::to_string)).collect()))
            .expect("spectrum_representation is a string column");
        let pts = b.column_by_name("number_of_data_points").unwrap().as_primitive::<UInt64Type>();
        let pks = b.column_by_name("number_of_peaks").unwrap().as_primitive::<UInt64Type>();
        for i in 0..b.num_rows() {
            let ix = index.value(i) as usize;
            rep[ix] = r[i].clone();
            n_pts[ix] = (!pts.is_null(i)).then(|| pts.value(i));
            n_pks[ix] = (!pks.is_null(i)).then(|| pks.value(i));
        }
    }
    for ix in 0..N_PROFILE {
        assert_eq!(rep[ix].as_deref(), Some("MS:1000128"), "spectrum {ix}: a gridded PROFILE spectrum stays profile");
        assert_eq!(n_pts[ix], Some(200), "spectrum {ix}: number_of_data_points describes the source");
        assert_eq!(n_pks[ix], None, "spectrum {ix}: no number_of_peaks on a profile row");
    }
    for ix in N_PROFILE..N_TOTAL {
        assert_eq!(rep[ix].as_deref(), Some("MS:1000127"), "spectrum {ix}: a centroid spectrum stays centroid, gridded or not");
        assert_eq!(n_pks[ix], Some(if ix == CENTROID_ON { 50 } else { 30 }), "spectrum {ix}: number_of_peaks");
        assert_eq!(n_pts[ix], None, "spectrum {ix}: no number_of_data_points on a centroid row");
    }

    // --- facets: profile → spectra_data with the axis; centroid → spectra_peaks, axis or f64 ---
    let data = facet_rows(&archive, "spectra_data.parquet", &dir);
    let peaks = facet_rows(&archive, "spectra_peaks.parquet", &dir);
    assert_eq!(data.keys().copied().collect::<Vec<_>>(), (0..N_PROFILE as u64).collect::<Vec<_>>(), "spectra_data holds exactly the profile spectra");
    for (ix, (rows, tof, mz)) in &data {
        assert_eq!((*rows, *tof, *mz), (200, 200, 0), "spectrum {ix}: gridded profile rows carry tof_index and a NULL mz");
    }
    assert_eq!(peaks.keys().copied().collect::<Vec<_>>(), (N_PROFILE as u64..N_TOTAL as u64).collect::<Vec<_>>(), "spectra_peaks holds exactly the centroid spectra");
    assert_eq!(peaks[&(CENTROID_ON as u64)], (50, 50, 0), "the on-lattice centroid spectrum is gridded in the peaks facet");
    for ix in CENTROID_ON as u64 + 1..N_TOTAL as u64 {
        assert_eq!(peaks[&ix], (30, 0, 30), "spectrum {ix}: an off-lattice centroid keeps exact f64 mz in the peaks facet");
    }

    // --- export: two arrays per spectrum, no leaked axis, values within the declared bound ---
    run(&[&archive, Path::new("-o"), &exported], &[]);
    let xml = std::fs::read_to_string(&exported).unwrap();
    assert!(!xml.contains("MS:1000786"), "the raw grid axis must not be exported as a non-standard array");
    // Spectra only: the writer's own TIC / base-peak chromatograms follow in the chromatogramList.
    let spectra_xml = xml.split("<chromatogramList").next().unwrap();
    assert_eq!(spectra_xml.matches("binaryDataArrayList count=\"3\"").count(), 0);
    assert_eq!(spectra_xml.matches("binaryDataArrayList count=\"2\"").count(), N_TOTAL, "one m/z + one intensity array per spectrum");
    let mut reader = mzdata::MZReader::open_path(&exported).unwrap();
    let truth = synthetic();
    let mut seen = 0;
    for spec in reader.iter() {
        let ix = spec.description().index;
        let (mz, it, cont) = &truth[ix];
        assert_eq!(spec.signal_continuity(), *cont, "spectrum {ix}: representation survives the round trip");
        let arrays = spec.arrays.as_ref().unwrap();
        let got_mz = arrays.mzs().unwrap();
        let got_it = arrays.intensities().unwrap();
        assert_eq!(got_it.as_ref(), it.as_slice(), "spectrum {ix}: intensities are stored verbatim");
        assert_eq!(got_mz.len(), mz.len(), "spectrum {ix}: point count");
        let gridded = ix < N_PROFILE || ix == CENTROID_ON;
        for (a, b) in got_mz.iter().zip(mz) {
            let ppm = (a - b).abs() / b * 1e6;
            if gridded {
                assert!(ppm <= 5.0, "spectrum {ix}: reconstructed m/z {a} vs {b} is {ppm:.3} ppm off (bound 5)");
            } else {
                assert_eq!(a, b, "spectrum {ix}: an off-lattice spectrum keeps exact f64 m/z");
            }
        }
        seen += 1;
    }
    assert_eq!(seen, N_TOTAL);
    let _ = std::fs::remove_dir_all(&dir);
}
