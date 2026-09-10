//! Contract pin for the Shimadzu centroid lattice peaks facet (`mz-grid` codec, 2026-09-02).
//!
//! The Shimadzu lane itself is `#[cfg(windows)]`, so this exercises the archive shape it produces
//! through the vendored writer API directly: a synthetic archive with the custom `spectra_peaks`
//! schema (`point.spectrum_index` UInt64, `point.tof_index` Int64 `LinearMz` "1e-9", `point.mz`
//! Float64 fallback, `point.intensity` Float32) beside the REAL Shimadzu data facet (`tof_index`
//! Int32 `SqrtMzFromTof` with the `(0,1)` placeholder and per-spectrum `tof_c0`/`tof_c1`, f64 `mz`
//! fallback, intensity) — two `tof_index` columns of different dtype and transform in one archive.
//! Four spectra: a dual one (sqrt-gridded profile in `spectra_data`, centroids handed over as
//! explicit lattice arrays), a centroid-only one on the coarse 1e-4 sub-lattice, a centroid-only
//! off-lattice one, and a dual one whose f64 profile AND off-lattice peak SET go through plain
//! `write_spectrum` (both fallbacks at once). Then (a) the vendored reader must hand back
//! `m/z == k / 1e9` on the lattice spectra and the exact f64 on the fallbacks — through
//! `get_spectrum` too, i.e. with the per-spectrum sqrt fixup applied to the profile and NOT to the
//! lattice — and (b) the parquet column metadata must show `point.tof_index` as INT64,
//! DELTA_BINARY_PACKED, ZSTD, dictionary disabled, with `spectrum_array_index` listing it as
//! `LinearMz` with params `[1e-9]`.
//!
//! The schema builders here are VERBATIM copies of `src/mz_lattice.rs::lattice_peak_schema` (which
//! `shimadzu_grid::lattice_peak_schema` binds at 1e-9), `main.rs::tof_index_field` and the grid
//! coefficient CURIEs — a binary crate has nothing an integration test can import — and
//! `write_archive` hands them to the writer as `convert_shimadzu` / `convert_vendor_reader` do.
//! `mirror_is_verbatim` compares each copy with its source byte for byte and pins those call sites,
//! so drift fails there instead of leaving this test exercising a schema production no longer writes.

use std::fs::File;
use std::path::{Path, PathBuf};

use arrow::datatypes::DataType;
use mzdata::params::{Param, Unit};
use mzdata::prelude::*;
use mzdata::spectrum::bindata::{ArrayType, BinaryDataArrayType, DataArray};
use mzdata::spectrum::{
    BinaryArrayMap, Chromatogram, ChromatogramDescription, MultiLayerSpectrum, PeakDataLevel,
    SignalContinuity, SpectrumDescription,
};
use mzpeak_prototyping::buffer_descriptors::BufferTransform;
use mzpeak_prototyping::peak_series::{INTENSITY_ARRAY, MZ_ARRAY};
use mzpeak_prototyping::writer::{
    AbstractMzPeakWriter, ArrayBuffersBuilder, CustomBuilderFromParameter, MzPeakWriterType,
};
use mzpeak_prototyping::{BufferContext, BufferName, MzPeakReader};
use mzpeaks::{CentroidPeak, PeakSet};
use parquet::basic::{Compression, Encoding, Type as PhysicalType, ZstdLevel};
use parquet::file::reader::{FileReader, SerializedFileReader};

/// `MassHigh`-style values (1e-9 Da lattice), up to m/z 1250.
const LATTICE_A: [i64; 3] = [100_000_123_456, 200_000_000_001, 1_250_123_456_789];
/// Coarse `Mass` values (1e-4 Da) as they land on the same lattice: multiples of 1e5.
const LATTICE_B: [i64; 3] = [50_000_100_000, 50_000_200_000, 1_234_567_800_000];
/// An off-lattice list (an interpolated apex, 0.3 of a step off): stays f64, exactly.
const FALLBACK_MZ: [f64; 2] = [100.000_123_456_3, 200.5];
/// A second off-lattice list, carried as the peak SET of a profile spectrum (spectrum 3).
const FALLBACK_DUAL_MZ: [f64; 2] = [150.000_000_000_4, 300.25];
/// The f64 profile the off-grid dual spectrum keeps in `spectra_data` (its `tof_index` is NULL).
const PROFILE_MZ: [f64; 4] = [99.9999, 100.0000, 100.0001, 100.0002];
const PROFILE_INTENSITY: [f32; 4] = [1.0, 5.0, 9.0, 2.0];
/// The sqrt-gridded profile of the dual lattice spectrum: `m/z = (C0 + C1·k)²`.
const GRID_K: [i32; 4] = [10_000, 10_001, 10_002, 10_003];
const GRID_C0: f64 = 8.0;
const GRID_C1: f64 = 0.000_091_602_119_892;

// ---- MIRROR -------------------------------------------------------------------------------------
// Every item below is a VERBATIM copy of the production item named above it, and
// `mirror_is_verbatim` fails the moment one differs. Regenerate by copying the item across; never
// edit it here. (Until 0.12 this was a hand-written look-alike that had already drifted: it still
// declared the grid coefficients as `MS:4000900/4000901`, which production left in 0.10.1.)

// src/mz_lattice.rs
pub const LATTICE_SCALE: f64 = 1e9;

// src/mz_lattice.rs
pub fn transform_params(scale: f64) -> String {
    let e = scale.log10().round();
    if scale > 0.0 && (10f64.powf(e) - scale).abs() <= f64::EPSILON * scale {
        format!("1e-{}", e as i64)
    } else {
        format!("{}", 1.0 / scale)
    }
}

// src/mz_lattice.rs
pub fn lattice_tof_index_field(scale: f64) -> std::sync::Arc<arrow::datatypes::Field> {
    let base = BufferName::new(
        BufferContext::Spectrum,
        ArrayType::nonstandard("tof_index"),
        BinaryDataArrayType::Int64,
    )
    .with_transform(Some(BufferTransform::LinearMz))
    .to_field();
    let mut md = base.metadata().clone();
    md.insert("mzpeak:transform_params".to_string(), transform_params(scale));
    std::sync::Arc::new((*base).clone().with_metadata(md))
}

// src/mz_lattice.rs
pub fn lattice_peak_schema(scale: f64) -> ArrayBuffersBuilder {
    ArrayBuffersBuilder::default()
        .prefix("point")
        .with_context(BufferContext::Spectrum)
        .add_field(BufferContext::Spectrum.index_field())
        .add_field(lattice_tof_index_field(scale))
        .add_field(MZ_ARRAY.to_field())
        .add_field(INTENSITY_ARRAY.to_field())
}

// src/main.rs
fn tof_index_field(run_wide: (f64, f64), per_spectrum: bool) -> std::sync::Arc<arrow::datatypes::Field> {
    let base = BufferName::new(
        BufferContext::Spectrum,
        ArrayType::nonstandard("tof_index"),
        BinaryDataArrayType::Int32,
    )
    .with_transform(Some(mzpeak_prototyping::buffer_descriptors::BufferTransform::SqrtMzFromTof))
    .to_field();
    let mut md = base.metadata().clone();
    md.insert("mzpeak:transform_params".to_string(), format!("{},{}", run_wide.0, run_wide.1));
    if per_spectrum {
        md.insert("mzpeak:transform_params_per_spectrum".to_string(), "tof_c0,tof_c1".to_string());
    }
    std::sync::Arc::new((*base).clone().with_metadata(md))
}

// src/main.rs
pub(crate) const TOF_C0_CURIE: mzdata::params::CURIE =
    mzdata::params::CURIE::new(mzdata::params::ControlledVocabulary::Unknown, 1_000_003);
// src/main.rs
pub(crate) const TOF_C1_CURIE: mzdata::params::CURIE =
    mzdata::params::CURIE::new(mzdata::params::ControlledVocabulary::Unknown, 1_000_004);
// ---- END MIRROR ---------------------------------------------------------------------------------

/// `src` with CRLF folded to LF (the Windows box checks out with `core.autocrlf=true`).
fn lf(src: &str) -> String {
    src.replace("\r\n", "\n")
}

/// The item whose first line starts with `head` at column 0, through its closing `}` (a fn) or `;`
/// (a const) at the end of a line. Doc comments above the item are not part of it.
fn item<'a>(src: &'a str, head: &str, what: &str) -> &'a str {
    let start = src.find(&format!("\n{head}")).unwrap_or_else(|| panic!("{what}: no item `{head}`")) + 1;
    let rest = &src[start..];
    let end = if head.contains("fn ") { rest.find("\n}\n").map(|e| e + 2) } else { rest.find(";\n").map(|e| e + 1) };
    &rest[..end.unwrap_or_else(|| panic!("{what}: `{head}` is unterminated"))]
}

/// The mirror is worth something only while it IS production: each item byte for byte, and the
/// `convert_shimadzu` / `convert_vendor_reader` lines that hand those schemas to the writer — which
/// `write_archive` below reproduces call for call.
#[test]
fn mirror_is_verbatim() {
    let this = lf(include_str!("shimadzu_lattice_peaks.rs"));
    let lattice = lf(include_str!("../src/mz_lattice.rs"));
    let grid = lf(include_str!("../src/shimadzu_grid.rs"));
    let main = lf(include_str!("../src/main.rs"));
    for (src, what, head) in [
        (&lattice, "src/mz_lattice.rs", "pub const LATTICE_SCALE: f64"),
        (&lattice, "src/mz_lattice.rs", "pub fn transform_params("),
        (&lattice, "src/mz_lattice.rs", "pub fn lattice_tof_index_field("),
        (&lattice, "src/mz_lattice.rs", "pub fn lattice_peak_schema("),
        (&main, "src/main.rs", "fn tof_index_field("),
        (&main, "src/main.rs", "pub(crate) const TOF_C0_CURIE:"),
        (&main, "src/main.rs", "pub(crate) const TOF_C1_CURIE:"),
    ] {
        assert_eq!(
            item(&this, head, "the mirror"),
            item(src, head, what),
            "the mirror of `{head}` differs from {what}: copy the production item over it"
        );
    }
    for (src, what, call) in [
        (&grid, "src/shimadzu_grid.rs", "pub use crate::mz_lattice::{LatticeOutcome, LATTICE_SCALE};"),
        (&grid, "src/shimadzu_grid.rs", "crate::mz_lattice::lattice_peak_schema(LATTICE_SCALE)"),
        (&main, "src/main.rs", "hints.peaks_facet = Some(shimadzu_grid::lattice_peak_schema());"),
        (&main, "src/main.rs", "let tof_field = tof_index_field((0.0, 1.0), true);"),
        (
            &main,
            "src/main.rs",
            "hints.data_facet_fields.push(tof_field);\n        \
             hints.data_facet_fields.push(mzpeak_prototyping::peak_series::MZ_ARRAY.to_field());\n        \
             hints.data_facet_fields.push(INTENSITY_ARRAY.to_field());\n        \
             hints.spectrum_param_fields.push((TOF_C0_CURIE, \"tof_c0\"));\n        \
             hints.spectrum_param_fields.push((TOF_C1_CURIE, \"tof_c1\"));",
        ),
        (
            &main,
            "src/main.rs",
            "CustomBuilderFromParameter::from_spec(\n            curie,\n            name,\n            DataType::Float64,\n        )",
        ),
    ] {
        assert!(src.contains(call), "{what} no longer declares `{call}` — `write_archive` mirrors it");
    }
}

fn f64_arrays(mz: &[f64], intensity: &[f32]) -> BinaryArrayMap {
    let mut out = BinaryArrayMap::new();
    let mut mz_da = DataArray::wrap(&ArrayType::MZArray, BinaryDataArrayType::Float64, Vec::new());
    mz_da.update_buffer(mz).unwrap();
    mz_da.unit = Unit::MZ;
    out.add(mz_da);
    let mut int_da =
        DataArray::wrap(&ArrayType::IntensityArray, BinaryDataArrayType::Float32, Vec::new());
    int_da.update_buffer(intensity).unwrap();
    int_da.unit = Unit::DetectorCounts;
    out.add(int_da);
    out
}

fn lattice_arrays(k: &[i64], intensity: &[f32]) -> BinaryArrayMap {
    let mut out = BinaryArrayMap::new();
    let mut tof_da =
        DataArray::wrap(&ArrayType::nonstandard("tof_index"), BinaryDataArrayType::Int64, Vec::new());
    tof_da.update_buffer(k).unwrap();
    out.add(tof_da);
    let mut int_da =
        DataArray::wrap(&ArrayType::IntensityArray, BinaryDataArrayType::Float32, Vec::new());
    int_da.update_buffer(intensity).unwrap();
    int_da.unit = Unit::DetectorCounts;
    out.add(int_da);
    out
}

/// The sqrt-gridded profile arrays (Int32 `tof_index` + intensity), as `shimadzu_grid_route` builds.
fn grid_profile_arrays(k: &[i32], intensity: &[f32]) -> BinaryArrayMap {
    let mut out = BinaryArrayMap::new();
    let mut tof_da =
        DataArray::wrap(&ArrayType::nonstandard("tof_index"), BinaryDataArrayType::Int32, Vec::new());
    tof_da.update_buffer(k).unwrap();
    out.add(tof_da);
    let mut int_da =
        DataArray::wrap(&ArrayType::IntensityArray, BinaryDataArrayType::Float32, Vec::new());
    int_da.update_buffer(intensity).unwrap();
    int_da.unit = Unit::DetectorCounts;
    out.add(int_da);
    out
}

/// What the reader must reconstruct for the gridded profile (same expression as the reader's).
fn grid_profile_mz() -> Vec<f64> {
    GRID_K
        .iter()
        .map(|&k| {
            let r = GRID_C0 + GRID_C1 * k as f64;
            r * r
        })
        .collect()
}

fn description(index: usize, continuity: SignalContinuity) -> SpectrumDescription {
    SpectrumDescription {
        id: format!("scan={}", index + 1),
        index,
        ms_level: 1,
        signal_continuity: continuity,
        ..Default::default()
    }
}

fn peak_set(mz: &[f64], intensity: &[f32]) -> PeakSet {
    PeakSet::new(
        mz.iter()
            .zip(intensity.iter())
            .enumerate()
            .map(|(i, (m, it))| CentroidPeak::new(*m, *it, i as u32))
            .collect(),
    )
}

/// The archive's own contract, `m/z = tof_index / scale` (the `mz_calibration` block's
/// `mz_from_tof_index`), NOT `k · 1e-9`: `1e-9` is not exactly 10⁻⁹, so multiplying by it differs
/// from the correctly-rounded quotient by one ulp on ~40 % of k — the difference between
/// reproducing the vendor's f64 bit for bit and not.
fn lattice_mz(k: &[i64]) -> Vec<f64> {
    k.iter().map(|&v| v as f64 / 1e9).collect()
}

fn write_archive(path: &Path) {
    // 0: dual — sqrt-gridded profile raw arrays (+ per-spectrum tof_c0/tof_c1) and the centroid
    // list as a peak set (metadata); the lattice arrays go to the peaks facet explicitly.
    let intensity_a = [5.0f32, 7.0, 9.0];
    let mut descr = description(0, SignalContinuity::Profile);
    descr.add_param(Param::builder().name("tof_c0").curie(TOF_C0_CURIE).value(GRID_C0).build());
    descr.add_param(Param::builder().name("tof_c1").curie(TOF_C1_CURIE).value(GRID_C1).build());
    let dual: MultiLayerSpectrum = MultiLayerSpectrum::new(
        descr,
        Some(grid_profile_arrays(&GRID_K, &PROFILE_INTENSITY)),
        Some(peak_set(&lattice_mz(&LATTICE_A), &intensity_a)),
        None,
    );
    // 1: centroid-only on the coarse sub-lattice — raw arrays carry the f64 view for the metadata.
    let intensity_b = [1.0f32, 2.0, 3.0];
    let coarse: MultiLayerSpectrum = MultiLayerSpectrum::new(
        description(1, SignalContinuity::Centroid),
        Some(f64_arrays(&lattice_mz(&LATTICE_B), &intensity_b)),
        None,
        None,
    );
    // 2: off-lattice — the ordinary path stores exact f64 m/z in the same facet's `mz` column.
    let fallback: MultiLayerSpectrum = MultiLayerSpectrum::new(
        description(2, SignalContinuity::Centroid),
        Some(f64_arrays(&FALLBACK_MZ, &[4.0, 6.0])),
        None,
        None,
    );
    // 3: dual, both fallbacks at once — an off-grid f64 profile and an off-lattice peak SET, so
    // plain `write_spectrum`'s "both profile signal and peaks" branch serialises the set into the
    // custom schema's `point.mz` (tof_index NULL) and the profile into `spectra_data`'s f64 `mz`.
    let fallback_dual: MultiLayerSpectrum = MultiLayerSpectrum::new(
        description(3, SignalContinuity::Profile),
        Some(f64_arrays(&PROFILE_MZ, &PROFILE_INTENSITY)),
        Some(peak_set(&FALLBACK_DUAL_MZ, &[4.0, 6.0])),
        None,
    );

    let handle = File::create(path).unwrap();
    let builder = MzPeakWriterType::<File>::builder()
        .compression(Compression::ZSTD(ZstdLevel::try_new(3).unwrap()))
        // As `convert_vendor_reader` does for the Shimadzu lane: the data facet's schema is sampled
        // from a (gridded) probe AND declared explicitly — the grid axis, the f64 `mz` fallback,
        // the intensity, the per-spectrum coefficients; the peaks facet is the lattice schema.
        .sample_array_types_from_spectra(std::iter::once(dual.clone()))
        .store_peaks_and_profiles_apart(Some(lattice_peak_schema(LATTICE_SCALE)))
        .add_spectrum_field(tof_index_field((0.0, 1.0), true))
        .add_spectrum_field(MZ_ARRAY.to_field())
        .add_spectrum_field(INTENSITY_ARRAY.to_field())
        .add_spectrum_param_field(CustomBuilderFromParameter::from_spec(
            TOF_C0_CURIE,
            "tof_c0",
            DataType::Float64,
        ))
        .add_spectrum_param_field(CustomBuilderFromParameter::from_spec(
            TOF_C1_CURIE,
            "tof_c1",
            DataType::Float64,
        ));
    let mut writer = builder.build(handle, true);

    writer
        .write_spectrum_with_peak_arrays(&dual, &lattice_arrays(&LATTICE_A, &intensity_a))
        .unwrap();
    writer
        .write_spectrum_with_peak_arrays(&coarse, &lattice_arrays(&LATTICE_B, &intensity_b))
        .unwrap();
    writer.write_spectrum(&fallback).unwrap();
    writer.write_spectrum(&fallback_dual).unwrap();

    // One empty chromatogram keeps the archive openable by the reference reader.
    let mut arrays = BinaryArrayMap::new();
    arrays.add(DataArray::wrap(&ArrayType::TimeArray, BinaryDataArrayType::Float64, Vec::new()));
    arrays.add(DataArray::wrap(&ArrayType::IntensityArray, BinaryDataArrayType::Float64, Vec::new()));
    writer.write_chromatogram(&Chromatogram::new(ChromatogramDescription::default(), arrays)).unwrap();

    let mut zip = writer.finish_parquet().unwrap();
    zip.add_index_metadata(
        "mz_calibration",
        &serde_json::json!({
            "codec": "mz-grid",
            "scale": 1e9,
            "vendor": "shimadzu",
            "lossless": "tof_index",
            "applies_to": "spectra_peaks",
            "mz_from_tof_index": "tof_index / scale",
        }),
    )
    .unwrap();
    zip.finish().unwrap();
}

fn peak_mzs(level: PeakDataLevel) -> Vec<f64> {
    match level {
        PeakDataLevel::Centroid(peaks) => peaks.iter().map(|p| p.mz).collect(),
        PeakDataLevel::RawData(arrays) => arrays.mzs().unwrap().to_vec(),
        other => panic!("unexpected peak level: {}", other.len()),
    }
}

fn extract(archive: &Path, member: &str) -> PathBuf {
    let mut zip = zip::ZipArchive::new(File::open(archive).unwrap()).unwrap();
    let extracted =
        std::env::temp_dir().join(format!("mzpc-lattice-{}-{member}", std::process::id()));
    let mut src = zip.by_name(member).unwrap_or_else(|_| panic!("{member} missing"));
    let mut dst = File::create(&extracted).unwrap();
    std::io::copy(&mut src, &mut dst).unwrap();
    extracted
}

#[test]
fn lattice_peaks_facet_round_trips_and_is_delta_packed_int64() {
    let out = std::env::temp_dir().join(format!("mzpc-lattice-{}.mzpeak", std::process::id()));
    let _ = std::fs::remove_file(&out);
    write_archive(&out);

    // (a) Read back with the vendored reader: m/z == k / 1e9 on the lattice spectra (DIVISION by
    // the integer scale is normative — `1e-9` is not exactly 10^-9, so multiplying by it differs on
    // ~40% of lattice values; see the assertions below and docs/mzpeakviewer-compliance-reply.md),
    // from the
    // column metadata alone (the `mz_calibration` block is for the viewer); the exact f64 on the
    // fallback; the profile facet untouched.
    let mut reader = MzPeakReader::new(&out).unwrap();
    assert_eq!(reader.len(), 4);
    for (index, ks) in [(0u64, &LATTICE_A), (1, &LATTICE_B)] {
        let mz = peak_mzs(reader.get_spectrum_peaks_for(index).unwrap().expect("peaks"));
        assert_eq!(mz, lattice_mz(ks), "spectrum {index}: m/z must be exactly k / 1e9");
    }
    let mz = peak_mzs(reader.get_spectrum_peaks_for(2).unwrap().expect("fallback peaks"));
    assert_eq!(mz, FALLBACK_MZ.to_vec(), "the off-lattice spectrum keeps its exact f64 m/z");
    let mz = peak_mzs(reader.get_spectrum_peaks_for(3).unwrap().expect("fallback dual peaks"));
    assert_eq!(mz, FALLBACK_DUAL_MZ.to_vec(), "an off-lattice peak SET keeps its exact f64 m/z");
    // Profile facet: the gridded spectrum reconstructs (c0 + c1·k)² from its per-spectrum
    // coefficients; the off-grid one keeps its exact f64 m/z.
    let profile = reader.get_spectrum_arrays(0).unwrap().expect("profile of the dual spectrum");
    assert_eq!(profile.mzs().unwrap().to_vec(), grid_profile_mz());
    assert_eq!(profile.intensities().unwrap().to_vec(), PROFILE_INTENSITY.to_vec());
    let profile = reader.get_spectrum_arrays(3).unwrap().expect("profile of the fallback dual");
    assert_eq!(profile.mzs().unwrap().to_vec(), PROFILE_MZ.to_vec());
    assert_eq!(profile.intensities().unwrap().to_vec(), PROFILE_INTENSITY.to_vec());
    // Metadata-row parity: a dual spectrum's `number_of_peaks` is its peak set's length and
    // `number_of_data_points` its profile's, whichever entry point wrote it.
    assert_eq!(&reader.metadata.spectra.peak_counts()[..4], &[3, 3, 2, 2]);
    assert_eq!(&reader.metadata.spectra.data_point_counts()[..4], &[4, 0, 0, 4]);
    // The full `get_spectrum` path — the one that applies the per-spectrum sqrt fixup — must
    // touch the profile only: the lattice centroids are still k/1e9, not (c0 + c1·k)². (The
    // reader's default preference loads only the profile of a dual spectrum; ask for both.)
    reader.set_prefer_spectra_peaks(
        mzpeak_prototyping::reader::SignalLoadingPreference::ProfilesAndCentroids,
    );
    let spec = reader.get_spectrum(0).expect("spectrum 0");
    assert_eq!(spec.arrays.as_ref().unwrap().mzs().unwrap().to_vec(), grid_profile_mz());
    let centroids: Vec<f64> = spec.peaks.as_ref().expect("centroid set").iter().map(|p| p.mz).collect();
    assert_eq!(centroids, lattice_mz(&LATTICE_A));
    let spec = reader.get_spectrum(3).expect("spectrum 3");
    assert_eq!(spec.arrays.as_ref().unwrap().mzs().unwrap().to_vec(), PROFILE_MZ.to_vec());
    let centroids: Vec<f64> = spec.peaks.as_ref().expect("centroid set").iter().map(|p| p.mz).collect();
    assert_eq!(centroids, FALLBACK_DUAL_MZ.to_vec());

    // Two `tof_index` entries, one per facet, with different dtype and transform.
    let data_index = reader.metadata.spectrum_array_indices();
    let profile_tof = data_index
        .iter()
        .find(|e| e.path.ends_with("tof_index"))
        .expect("spectrum_array_index of the data facet lists point.tof_index");
    assert_eq!(profile_tof.data_type, DataType::Int32);
    assert_eq!(profile_tof.transform, Some(BufferTransform::SqrtMzFromTof));
    assert_eq!(profile_tof.transform_params, Some(vec![0.0, 1.0]));

    let peak_index = reader.metadata.peak_array_indices().expect("peaks facet array index");
    let tof = peak_index
        .iter()
        .find(|e| e.path.ends_with("tof_index"))
        .expect("spectrum_array_index lists point.tof_index");
    assert_eq!(tof.path, "point.tof_index");
    assert_eq!(tof.data_type, DataType::Int64);
    assert_eq!(tof.transform, Some(BufferTransform::LinearMz));
    assert_eq!(tof.transform_params, Some(vec![1e-9]));
    assert!(
        peak_index.iter().any(|e| e.path == "point.mz" && e.data_type == DataType::Float64),
        "the f64 `mz` fallback column must be declared beside the lattice"
    );

    // (b) Parquet column metadata of the peaks facet.
    let extracted = extract(&out, "spectra_peaks.parquet");
    let pq = SerializedFileReader::new(File::open(&extracted).unwrap()).unwrap();
    let total_rows: i64 = pq.metadata().row_groups().iter().map(|rg| rg.num_rows()).sum();
    assert_eq!(total_rows, 10, "3 + 3 lattice rows + 2 + 2 fallback rows");
    let (mut tof_nulls, mut mz_nulls) = (0i64, 0i64);
    for rg in pq.metadata().row_groups() {
        let tof = rg
            .columns()
            .iter()
            .find(|c| c.column_path().string() == "point.tof_index")
            .expect("point.tof_index column");
        assert_eq!(tof.column_type(), PhysicalType::INT64);
        assert!(matches!(tof.compression(), Compression::ZSTD(_)), "tof_index is {}", tof.compression());
        let encodings: Vec<Encoding> = tof.encodings().collect();
        assert!(
            encodings.contains(&Encoding::DELTA_BINARY_PACKED),
            "tof_index must be DELTA_BINARY_PACKED (encodings: {encodings:?})"
        );
        assert!(
            !encodings.iter().any(|e| matches!(e, Encoding::RLE_DICTIONARY | Encoding::PLAIN_DICTIONARY)),
            "tof_index must not be dictionary-encoded (encodings: {encodings:?})"
        );
        tof_nulls += tof.statistics().and_then(|s| s.null_count_opt()).unwrap_or(0) as i64;
        let mz = rg
            .columns()
            .iter()
            .find(|c| c.column_path().string() == "point.mz")
            .expect("point.mz column");
        assert_eq!(mz.column_type(), PhysicalType::DOUBLE);
        mz_nulls += mz.statistics().and_then(|s| s.null_count_opt()).unwrap_or(0) as i64;
    }
    assert_eq!(mz_nulls, 6, "the f64 mz fallback is NULL on every lattice row");
    assert_eq!(tof_nulls, 4, "tof_index is NULL on every fallback row (peak set or raw arrays)");
    let _ = std::fs::remove_file(&extracted);
    let _ = std::fs::remove_file(&out);
}
