//! Centroid m/z as an exact integer lattice — vendor-neutral (the viewer's `mz-grid` codec).
//!
//! Some acquisitions hand over m/z that are not really floating point at all: they are vendor
//! fixed-point integers divided by a power of ten. Shimadzu's `MassHigh` is an Int64 at 1e-9 Da;
//! its coarse `Mass` fallback is an Int32 at 1e-4 Da (a multiple of 1e5 on the same 1e-9 lattice);
//! LabSolutions' own mzML exporter writes the very same 1e-9 values as f64 `binary`.
//!
//! For such data `k = round(m/z · scale)` is exact by construction, and one integer per point —
//! delta-packed by Parquet — replaces an f64 whose deltas are all distinct. Measured on the
//! LabSolutions mzML export of `DIA_Hela_20ng` (280 M centroids): the m/z columns go from 1.906 GB
//! (lossless delta chunking) or 0.847 GB (LOSSY numpress-linear) to 0.788 GB — lossless AND smaller
//! than either.
//!
//! This module was the Shimadzu-native-lane-only `shimadzu_grid` lattice half; it is now
//! scale-parameterised and used by the ordinary mzML/generic reader lane too. `shimadzu_grid`
//! re-exports it at the fixed 1e-9 scale so that lane and its tests are unchanged.
//!
//! WHAT IS AND IS NOT ROUTED. Only the CENTROID list (`spectra_peaks`) takes the lattice. Profile
//! arrays keep the treatment they already had — the chunked `spectra_data` facet with its
//! delta/numpress refinement — for two reasons: the chunk encoders operate on an f64 main axis and
//! have no integer form, and a profile axis is a flight-time (sqrt) grid rather than a linear m/z
//! one, so it is `shimadzu_grid`'s / `tof_grid`'s business, not this one's. A run with both gets a
//! point-layout peaks facet beside a chunked data facet — the mixed layout family this project
//! accepts on purpose (the writer warns).
//!
//! NOTHING IS SNAPPED. The guard is per spectrum and all-or-nothing: one point off the lattice and
//! the whole spectrum keeps its exact f64 m/z in the peak facet's `mz` column, which is why the
//! schema carries both columns.

use mzdata::params::Unit;
use mzdata::prelude::*;
use mzdata::spectrum::bindata::{ArrayType, BinaryDataArrayType, DataArray};
use mzdata::spectrum::{BinaryArrayMap, MultiLayerSpectrum, SignalContinuity};
use mzpeak_prototyping::buffer_descriptors::BufferTransform;
use mzpeak_prototyping::peak_series::{INTENSITY_ARRAY, MZ_ARRAY};
use mzpeak_prototyping::writer::ArrayBuffersBuilder;
use mzpeak_prototyping::{BufferContext, BufferName};

/// Vendor fixed-point scales seen in the wild, tried COARSEST FIRST so the chosen `k` is the
/// smallest integer that still reproduces every value: 1e3/1e4/1e5 are Shimadzu's `MASSNUMBER_UNIT`
/// family (1e-4 is the `.lcd` coarse field), 1e9 is `MassHigh` — which is also what LabSolutions'
/// mzML exporter writes, and what a Bruker/Thermo mzML never lands on.
///
/// Coarsest-first is free: a value on the 1e-3 lattice is trivially also on 1e-4 and 1e-5, so the
/// finer scales can only ever be reached by data the coarse ones REJECT.
pub const SCALES: [f64; 4] = [1_000.0, 10_000.0, 100_000.0, 1_000_000_000.0];

/// The Shimadzu native lane's scale (`MassHigh`, 1e-9 Da). Kept as a named constant because the
/// `mz-grid` contract pinned by `tests/shimadzu_lattice_peaks.rs` names it.
pub const LATTICE_SCALE: f64 = 1e9;

/// Floor of the per-point guard on the scaled value: `|m/z·scale − k| < lattice_tolerance(m/z·scale)`.
///
/// The real term is the RELATIVE one: the f64 product's own rounding grows with the scaled
/// magnitude (ulp of 1.25e12 is 2.4e-4; of 4e12 — m/z 4000 at 1e9 — it is 4.9e-4), and a
/// coarse-field path rounds twice (`massInt · 1e-4`, then `· 1e9`), so 8 ulp is what makes the
/// margin hold at any m/z. This floor only matters for SMALL scaled magnitudes, where 8 ulp is
/// vanishing — e.g. m/z 100 at scale 1e3 is 1e5, whose 8 ulp is 1.2e-10.
///
/// It was 1e-3 while this module was the Shimadzu-1e-9-only lane, where 1e-3 scaled means 1e-12 Da
/// and is harmless. At the 1e3/1e4/1e5 scales the generic lane added it would have meant up to
/// 1e-6 Da, i.e. the guard would have SNAPPED genuinely off-lattice values onto the lattice
/// instead of keeping their exact f64 — the opposite of this module's invariant, and 1000× looser
/// than the detector that armed the route. Both now go through [`on_lattice_scaled`], so they
/// cannot drift apart again.
pub const LATTICE_TOL: f64 = 1e-6;

/// The guard for one scaled value: `max(LATTICE_TOL, 8 ulp of the scaled value)`.
pub fn lattice_tolerance(scaled: f64) -> f64 {
    (scaled.abs() * 8.0 * f64::EPSILON).max(LATTICE_TOL)
}

/// THE lattice predicate: is this scaled value a vendor integer within [`lattice_tolerance`]?
///
/// [`fixed_point_lattice_scale`] (which ARMS the route) and [`centroid_lattice`] (which decides
/// each spectrum once it is armed) must agree, or the run stores values the detector would have
/// rejected. One function, called by both.
pub fn on_lattice_scaled(scaled: f64) -> bool {
    (scaled - scaled.round()).abs() < lattice_tolerance(scaled)
}

/// Does this m/z axis sit on a fixed-point lattice — i.e. are the values vendor-stored scaled
/// integers? `Some(scale)` names the COARSEST scale in [`SCALES`] that reproduces EVERY sampled
/// value; `None` means the data is ordinary floating point.
///
/// This decides delta-vs-numpress — and now the lattice route itself — from the DATA rather than
/// from the file extension, which is a bad proxy and was measurably wrong. A Shimadzu `.lcd` read
/// natively is on an exact 1e-4 lattice (residual 9.3e-10) where delta chunking is ~3x smaller than
/// numpress-linear AND bit-exact. But msconvert's mzML **of the same acquisition** is off that
/// lattice (residual ~0.5, uniform), and there delta is 1.6x LARGER than numpress. Same instrument,
/// same run, opposite answers — so the extension cannot decide this and the values have to be
/// looked at.
///
/// (Formerly `is_fixed_point_lattice`, returning a bool. Callers that only need the boolean say
/// `.is_some()`; the scale itself is what the lattice route stores.)
pub fn fixed_point_lattice_scale(mzs: &[f64]) -> Option<f64> {
    let sample: Vec<f64> = mzs.iter().copied().filter(|v| v.is_finite() && *v > 0.0).collect();
    if sample.len() < 64 {
        return None;
    }
    SCALES.iter().copied().find(|&scale| {
        // Every value must land on the grid, not merely most: a genuine lattice has NO exceptions,
        // while off-lattice data occasionally lands near an integer by chance. The tolerance is
        // relative: at the 1e-9 scale m/z 1700 becomes 1.7e12, whose f64 ulp (~2.4e-4) is far above
        // a fixed 1e-6 — while off-lattice residuals are uniform on [0, 0.5], so a few ulps still
        // discriminate (P(64 chance hits) ~ 0).
        sample.iter().all(|v| on_lattice_scaled(v * scale))
    })
}

/// The `mzpeak:transform_params` string the reader multiplies `k` by (`LinearMz`: m/z = p₀·k).
///
/// A power of ten is written in the exact literal form the contract names (`1e9` → `"1e-9"`), so
/// the Shimadzu archives keep byte-identical column metadata; anything else falls back to the
/// shortest round-tripping decimal of `1/scale`.
pub fn transform_params(scale: f64) -> String {
    let e = scale.log10().round();
    if scale > 0.0 && (10f64.powf(e) - scale).abs() <= f64::EPSILON * scale {
        format!("1e-{}", e as i64)
    } else {
        format!("{}", 1.0 / scale)
    }
}

/// `Some(k)` when EVERY centroid lies on the `scale` lattice within [`lattice_tolerance`] and `k` is
/// non-decreasing; `None` (keep f64 m/z) for an empty list, a non-finite or negative value, an
/// off-lattice point, a value too large for an `i64`, or a descending pair. Nothing is snapped.
pub fn centroid_lattice(mz: &[f64], scale: f64) -> Option<Vec<i64>> {
    if mz.is_empty() || !(scale > 0.0) {
        return None;
    }
    let mut out = Vec::with_capacity(mz.len());
    let mut prev = i64::MIN;
    for &m in mz {
        if !m.is_finite() || m < 0.0 {
            return None;
        }
        let scaled = m * scale;
        if !on_lattice_scaled(scaled) {
            return None;
        }
        let k = scaled.round();
        // `as i64` SATURATES in Rust, so an absurd scaled value would silently become i64::MAX and
        // read back as a wrong m/z rather than falling back to f64.
        if k > i64::MAX as f64 {
            return None;
        }
        let k = k as i64;
        if k < prev {
            return None;
        }
        prev = k;
        out.push(k);
    }
    Some(out)
}

/// The `point.tof_index` field: Int64, nonstandard `tof_index`, `LinearMz` with params `1/scale`.
/// The name ends in `_index` ON PURPOSE — the vendored writer disables the dictionary and applies
/// DELTA_BINARY_PACKED to `*_index` columns (writer/base.rs `spectrum_data_writer_props`).
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

/// Custom `spectra_peaks` schema for a lattice lane (point layout, prefix `point`):
/// `spectrum_index` UInt64, `tof_index` Int64 (the lattice), `mz` Float64 (the per-spectrum f64
/// fallback, NULL on lattice rows), `intensity` Float32. The BufferNames MUST match the DataArrays
/// [`lattice_peak_arrays`] builds, or the columns spill to auxiliary.
pub fn lattice_peak_schema(scale: f64) -> ArrayBuffersBuilder {
    ArrayBuffersBuilder::default()
        .prefix("point")
        .with_context(BufferContext::Spectrum)
        .add_field(BufferContext::Spectrum.index_field())
        .add_field(lattice_tof_index_field(scale))
        .add_field(MZ_ARRAY.to_field())
        .add_field(INTENSITY_ARRAY.to_field())
}

/// The `mz_calibration` index block the viewer's `mz-grid` codec gates on (`scale` MUST be a JSON
/// number > 0). `vendor`/`source` describe where the integers came from, for a human reading the
/// index; the codec itself only needs `codec`, `scale` and `applies_to`.
pub fn mz_calibration_block(scale: f64, vendor: &str, source: &str) -> serde_json::Value {
    serde_json::json!({
        "codec": "mz-grid",
        "scale": scale,
        "vendor": vendor,
        "lossless": "tof_index",
        "applies_to": "spectra_peaks",
        "mz_from_tof_index": "tof_index / scale",
        "source": source,
    })
}

/// The peak-facet arrays for one lattice spectrum: `tof_index` Int64 + `intensity` Float32
/// (detector counts, matching `INTENSITY_ARRAY` so it maps to `point.intensity`).
pub fn lattice_peak_arrays(k: &[i64], intensity: &[f32]) -> Option<BinaryArrayMap> {
    if k.len() != intensity.len() {
        return None;
    }
    let mut out = BinaryArrayMap::new();
    let mut tof_da =
        DataArray::wrap(&ArrayType::nonstandard("tof_index"), BinaryDataArrayType::Int64, Vec::new());
    tof_da.update_buffer(k).ok()?;
    out.add(tof_da);
    let mut int_da =
        DataArray::wrap(&ArrayType::IntensityArray, BinaryDataArrayType::Float32, Vec::new());
    int_da.update_buffer(intensity).ok()?;
    int_da.unit = Unit::DetectorCounts;
    out.add(int_da);
    Some(out)
}

/// What [`lattice_route`] decided for one spectrum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LatticeOutcome {
    /// Every centroid on the lattice: the peak facet gets `tof_index` + intensity.
    Lattice,
    /// At least one centroid off the lattice (or descending): the spectrum keeps f64 m/z.
    KeptF64,
    /// No centroid list on this spectrum (profile-only), or an empty one (a 0-point scan):
    /// nothing to route, and nothing kept in f64 either.
    NoCentroids,
}

impl LatticeOutcome {
    /// `Some(true)` on the lattice, `Some(false)` kept f64, `None` nothing to route — the shape
    /// the converter's per-facet summary counters record.
    pub fn on_lattice(self) -> Option<bool> {
        match self {
            LatticeOutcome::Lattice => Some(true),
            LatticeOutcome::KeptF64 => Some(false),
            LatticeOutcome::NoCentroids => None,
        }
    }
}

/// The centroid (m/z, intensity) pairs of a spectrum, i.e. the list that would reach the
/// `spectra_peaks` facet: the peak set when one is attached (a dual Shimadzu `.lcd` scan, or an
/// mzML mzdata chose to pick), otherwise the raw arrays of a Centroid-continuity spectrum.
/// `None` for a profile-only spectrum — there is nothing for this module to route.
pub fn centroid_pairs(spec: &MultiLayerSpectrum) -> Option<(Vec<f64>, Vec<f32>)> {
    if let Some(peaks) = spec.peaks.as_ref() {
        Some(peaks.iter().map(|p| (p.mz, p.intensity)).unzip())
    } else if spec.signal_continuity() == SignalContinuity::Centroid {
        spec.arrays.as_ref().and_then(|a| match (a.mzs(), a.intensities()) {
            (Ok(mz), Ok(inten)) => Some((mz.to_vec(), inten.to_vec())),
            _ => None,
        })
    } else {
        None
    }
}

/// Per-spectrum routing of the CENTROID list at `scale`. On success the spectrum is returned
/// UNCHANGED together with the lattice arrays the writer sends to the peak facet through
/// `write_spectrum_with_peak_arrays`; otherwise `None` and the ordinary `write_spectrum` path
/// stores the f64 m/z in the same facet's `mz` column.
///
/// RETURNING THE SPECTRUM UNCHANGED IS THE SUMMARY CONTRACT. Every route that REPLACES a
/// spectrum's f64 m/z array with an integer axis must hand the writer explicit MS:1000285 /
/// MS:1000504 / MS:1000505 / MS:1000527 / MS:1000528 computed from the pre-grid values, or the
/// writer derives `tic = 0`, `base peak = (0,0)` and a null m/z range from the m/z-less array map
/// (the defect fixed in bc8497c, and `main.rs::set_gridded_spectrum_summary`). This route does not
/// replace anything: the spectrum keeps its peak set / raw m/z array and only the FACET ROWS are
/// integers, so the writer's own derivation is fed real m/z and the summaries are right by
/// construction. `tests/mz_lattice_mzml.rs` asserts that on a converted archive rather than
/// trusting it — a future refactor that strips the arrays here must fail that test.
pub fn lattice_route(
    spec: MultiLayerSpectrum,
    scale: f64,
) -> (MultiLayerSpectrum, Option<BinaryArrayMap>, LatticeOutcome) {
    // An empty list is not an off-lattice one: report it as nothing-to-route so the summary
    // counters do not call every empty scan "kept f64 m/z".
    let Some((mz, intensity)) = centroid_pairs(&spec).filter(|(mz, _)| !mz.is_empty()) else {
        return (spec, None, LatticeOutcome::NoCentroids);
    };
    let Some(k) = centroid_lattice(&mz, scale) else {
        return (spec, None, LatticeOutcome::KeptF64);
    };
    match lattice_peak_arrays(&k, &intensity) {
        Some(arrays) => (spec, Some(arrays), LatticeOutcome::Lattice),
        None => (spec, None, LatticeOutcome::KeptF64),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mzdata::spectrum::SpectrumDescription;
    use mzpeak_prototyping::writer::ArrayBufferWriter;
    use mzpeaks::{CentroidPeak, PeakSet};

    fn on_lattice(ks: &[i64], scale: f64) -> Vec<f64> {
        ks.iter().map(|&k| k as f64 / scale).collect()
    }

    // ---- the scale-returning detector -------------------------------------------------------

    #[test]
    fn the_detector_names_each_of_the_four_scales() {
        // Enough values to clear the 64-sample floor, and irregular so a chance fit is impossible.
        let axis = |scale: f64| -> Vec<f64> {
            (0..400).map(|i| (100.0 * scale + (i as f64 * 7919.0) % 3_000_000.0) / scale).collect()
        };
        for scale in [1e3, 1e4, 1e5, 1e9] {
            assert_eq!(
                fixed_point_lattice_scale(&axis(scale)),
                Some(scale),
                "scale {scale:e} not detected"
            );
        }
        // The real 1e-9 Shimadzu case: 1e-9 values are NOT on 1e-3/1e-4/1e-5, so the coarse-first
        // order cannot mis-name them.
        let masshigh: Vec<f64> =
            (0..200).map(|i| (445_118_843_583i64 + i * 999_983) as f64 * 1e-9).collect();
        assert_eq!(fixed_point_lattice_scale(&masshigh), Some(1e9));
    }

    #[test]
    fn the_detector_prefers_the_coarsest_scale_that_fits() {
        // Values that are multiples of 1e-3 are on every finer lattice too; the coarsest wins so
        // `k` stays as small as possible.
        let coarse: Vec<f64> = (0..200).map(|i| (100_000 + i * 37) as f64 / 1e3).collect();
        assert_eq!(fixed_point_lattice_scale(&coarse), Some(1e3));
    }

    #[test]
    fn ordinary_floating_point_mz_is_not_a_lattice() {
        // A Thermo/Orbitrap-style axis: irrational spacing, nothing lands on a decimal grid.
        let orbi: Vec<f64> = (0..300).map(|i| 300.0 + (i as f64) * std::f64::consts::PI / 7.0).collect();
        assert_eq!(fixed_point_lattice_scale(&orbi), None);
        // Too few values to decide is also `None` — a 63-point lattice must not arm the route.
        let short: Vec<f64> = (0..63).map(|i| (100_000 + i) as f64 / 1e4).collect();
        assert_eq!(fixed_point_lattice_scale(&short), None);
        // f32-rounded m/z (a common mzML) is off every lattice.
        let f32ish: Vec<f64> = (0..200).map(|i| ((300.0 + i as f64 * 0.37) as f32) as f64).collect();
        assert_eq!(fixed_point_lattice_scale(&f32ish), None);
    }

    #[test]
    fn the_transform_params_string_is_the_reciprocal_power_of_ten() {
        assert_eq!(transform_params(1e9), "1e-9");
        assert_eq!(transform_params(1e4), "1e-4");
        assert_eq!(transform_params(1e5), "1e-5");
        assert_eq!(transform_params(1e3), "1e-3");
        // Every string the reader will see must parse back to the scale it names.
        for scale in SCALES {
            let p: f64 = transform_params(scale).parse().expect("parses");
            assert!((p * scale - 1.0).abs() < 1e-12, "{scale:e} -> {p:e}");
        }
    }

    // ---- the per-spectrum guard -------------------------------------------------------------

    #[test]
    fn masshigh_values_recover_their_integer_exactly() {
        let ks = [50_000_000_000i64, 123_456_789_012, 999_999_999_999, 1_250_123_456_789];
        assert_eq!(centroid_lattice(&on_lattice(&ks, 1e9), 1e9).unwrap(), ks);
        // Equal neighbours (a duplicated centroid) are non-decreasing, hence allowed.
        assert_eq!(centroid_lattice(&on_lattice(&[7, 7, 8], 1e9), 1e9).unwrap(), vec![7, 7, 8]);
    }

    #[test]
    fn a_coarse_scale_recovers_its_own_integers() {
        let ks = [1_000_123i64, 1_000_124, 45_678_901];
        assert_eq!(centroid_lattice(&on_lattice(&ks, 1e4), 1e4).unwrap(), ks);
        // The same values at the 1e-9 scale are the 1e5 multiples of the same lattice.
        let k9 = centroid_lattice(&on_lattice(&ks, 1e4), 1e9).unwrap();
        assert!(k9.iter().all(|v| v % 100_000 == 0));
    }

    #[test]
    fn an_off_lattice_point_keeps_the_spectrum_in_f64() {
        let mut mz = on_lattice(&[100_000_000_000, 200_000_000_000, 300_000_000_000], 1e9);
        mz[1] += 0.3e-9; // an interpolated apex, 0.3 of a lattice step off
        assert!(centroid_lattice(&mz, 1e9).is_none());
        // Inside the guard is still accepted: one ulp of the m/z value itself (2.8e-14 Da at
        // m/z 200, i.e. 2.8e-5 in scaled units) is the f64 product's own rounding, not an
        // off-lattice value.
        let mut mz = on_lattice(&[100_000_000_000, 200_000_000_000], 1e9);
        mz[1] = f64::from_bits(mz[1].to_bits() + 1);
        assert!(centroid_lattice(&mz, 1e9).is_some());
    }

    #[test]
    fn a_coarse_scale_does_not_snap_a_value_off_its_own_lattice() {
        // 5e-7 Da off the 1e-3 lattice — half a part in 2000 of a step, far outside anything f64
        // rounding can produce, and precisely what the old scale-blind 1e-3 floor (1e-6 Da at this
        // scale) silently snapped. The detector rejects it, so the guard must too.
        let mut mz = on_lattice(&[100_037, 100_038, 100_039], 1e3);
        mz[1] += 5e-7;
        assert!(!on_lattice_scaled(mz[1] * 1e3));
        assert!(centroid_lattice(&mz, 1e3).is_none());
        // ... and the guard is never looser than the detector that armed the route, at any scale.
        for scale in SCALES {
            for mz in [80.0f64, 400.0, 1700.0, 4000.0] {
                let off = mz + 0.4 / scale; // 0.4 of a lattice step: genuinely off
                assert!(!on_lattice_scaled(off * scale), "scale {scale:e} m/z {mz}");
                assert!(on_lattice_scaled((mz * scale).round() / scale * scale));
            }
        }
    }

    #[test]
    fn the_guard_scales_with_mz_at_the_top_of_a_wide_range() {
        let ks: Vec<i64> = (0..4000).map(|i| 4_000_000_000_000 + i * 123_456_789).collect();
        assert_eq!(centroid_lattice(&on_lattice(&ks, 1e9), 1e9).unwrap(), ks);
        let ints: Vec<i64> = (39_990_000..40_010_000).step_by(7).collect();
        let coarse: Vec<f64> = ints.iter().map(|&m| m as f64 * 1e-4).collect();
        let k = centroid_lattice(&coarse, 1e9).expect("coarse Mass at m/z 4000 is on the lattice");
        assert_eq!(k, ints.iter().map(|m| m * 100_000).collect::<Vec<_>>());
        let mut mz = on_lattice(&[4_000_000_000_000, 4_000_000_000_001], 1e9);
        mz[1] += 0.3e-9;
        assert!(centroid_lattice(&mz, 1e9).is_none());
        assert!(lattice_tolerance(4e12) > LATTICE_TOL && lattice_tolerance(4e12) < 0.01);
        // Below ~5.6e9 scaled the relative term falls under the floor, and the floor takes over.
        assert_eq!(lattice_tolerance(1e5), LATTICE_TOL);
    }

    #[test]
    fn descending_empty_non_finite_and_oversized_input_is_refused() {
        assert!(centroid_lattice(&on_lattice(&[5, 4], 1e9), 1e9).is_none());
        assert!(centroid_lattice(&[], 1e9).is_none());
        assert!(centroid_lattice(&[100.0, f64::NAN], 1e9).is_none());
        assert!(centroid_lattice(&[-1e-9], 1e9).is_none());
        assert!(centroid_lattice(&[1e-9], 0.0).is_none());
        // `as i64` saturates rather than wrapping, so an out-of-range value must be refused here.
        assert!(centroid_lattice(&[1e30], 1e9).is_none());
    }

    // ---- routing ------------------------------------------------------------------------------

    fn arrays(mz: &[f64], inten: &[f32]) -> BinaryArrayMap {
        let mut out = BinaryArrayMap::new();
        let mut mz_da = DataArray::wrap(&ArrayType::MZArray, BinaryDataArrayType::Float64, Vec::new());
        mz_da.update_buffer(mz).unwrap();
        out.add(mz_da);
        let mut int_da =
            DataArray::wrap(&ArrayType::IntensityArray, BinaryDataArrayType::Float32, Vec::new());
        int_da.update_buffer(inten).unwrap();
        out.add(int_da);
        out
    }

    fn spectrum(
        continuity: SignalContinuity,
        raw: Option<BinaryArrayMap>,
        peaks: Option<&[(f64, f32)]>,
    ) -> MultiLayerSpectrum {
        let descr = SpectrumDescription { signal_continuity: continuity, ..Default::default() };
        let peak_set = peaks.map(|p| {
            PeakSet::new(
                p.iter().enumerate().map(|(i, (m, it))| CentroidPeak::new(*m, *it, i as u32)).collect(),
            )
        });
        MultiLayerSpectrum::new(descr, raw, peak_set, None)
    }

    fn tof_index_of(arrays: &BinaryArrayMap) -> Vec<i64> {
        arrays.get(&ArrayType::nonstandard("tof_index")).unwrap().to_i64().unwrap().to_vec()
    }

    #[test]
    fn a_dual_spectrum_routes_its_peak_set_and_keeps_the_profile() {
        let profile = arrays(&[100.0, 100.0001, 100.0002], &[1.0, 5.0, 2.0]);
        let centroids = [(100.000_123_456, 5.0f32), (200.000_000_001, 7.0)];
        let spec = spectrum(SignalContinuity::Profile, Some(profile), Some(&centroids));
        let (spec, lattice, outcome) = lattice_route(spec, 1e9);
        assert_eq!(outcome, LatticeOutcome::Lattice);
        let lattice = lattice.expect("lattice arrays");
        assert_eq!(tof_index_of(&lattice), vec![100_000_123_456, 200_000_000_001]);
        assert_eq!(lattice.intensities().unwrap().to_vec(), vec![5.0, 7.0]);
        // The spectrum itself is untouched: the profile still feeds the data facet and the peak
        // set still feeds `number_of_peaks` / base peak / TIC.
        assert_eq!(spec.signal_continuity(), SignalContinuity::Profile);
        assert_eq!(spec.arrays.as_ref().unwrap().mzs().unwrap().len(), 3);
        assert_eq!(spec.peaks.as_ref().unwrap().len(), 2);
    }

    /// The generic (mzML) lane: a centroid mzML spectrum carries its m/z as RAW ARRAYS, with no
    /// peak set at all — this is the shape `convert_file` hands over.
    #[test]
    fn a_centroid_only_spectrum_routes_its_raw_arrays_at_either_scale() {
        for (scale, mz, want) in [
            (1e9, vec![100.000_123_456, 200.000_000_001], vec![100_000_123_456i64, 200_000_000_001]),
            (1e4, vec![100.0123, 200.5], vec![1_000_123i64, 2_005_000]),
        ] {
            let raw = arrays(&mz, &[5.0, 7.0]);
            let spec = spectrum(SignalContinuity::Centroid, Some(raw), None);
            let (spec, lattice, outcome) = lattice_route(spec, scale);
            assert_eq!(outcome, LatticeOutcome::Lattice, "scale {scale:e}");
            assert_eq!(tof_index_of(&lattice.unwrap()), want, "scale {scale:e}");
            // The m/z array survives on the spectrum: that is what feeds the summary columns.
            assert_eq!(spec.arrays.as_ref().unwrap().mzs().unwrap().len(), 2);
        }
    }

    #[test]
    fn one_off_lattice_spectrum_falls_back_to_f64_while_the_run_stays_on_the_lattice() {
        // Same run, same scale: the first spectrum grids, the second does not (an interpolated
        // apex). Nothing is snapped, and the off-lattice one is not refused either.
        let good = spectrum(
            SignalContinuity::Centroid,
            Some(arrays(&[100.0123, 200.5], &[5.0, 7.0])),
            None,
        );
        let (_, lattice, outcome) = lattice_route(good, 1e4);
        assert_eq!(outcome, LatticeOutcome::Lattice);
        assert!(lattice.is_some());

        let bad = spectrum(
            SignalContinuity::Centroid,
            Some(arrays(&[100.012_34, 200.5], &[5.0, 7.0])),
            None,
        );
        let (bad, lattice, outcome) = lattice_route(bad, 1e4);
        assert_eq!(outcome, LatticeOutcome::KeptF64);
        assert!(lattice.is_none());
        // The exact f64 is still on the spectrum for `write_spectrum` to store in `point.mz`.
        assert_eq!(bad.arrays.as_ref().unwrap().mzs().unwrap()[0], 100.012_34);
    }

    #[test]
    fn off_lattice_centroids_keep_f64_and_profile_only_has_nothing_to_route() {
        let raw = arrays(&[100.000_123_456_3, 200.0], &[5.0, 7.0]);
        let spec = spectrum(SignalContinuity::Centroid, Some(raw), None);
        let (_, lattice, outcome) = lattice_route(spec, 1e9);
        assert_eq!(outcome, LatticeOutcome::KeptF64);
        assert!(lattice.is_none());

        let spec = spectrum(SignalContinuity::Profile, Some(arrays(&[100.0], &[1.0])), None);
        let (_, lattice, outcome) = lattice_route(spec, 1e9);
        assert_eq!(outcome, LatticeOutcome::NoCentroids);
        assert!(lattice.is_none());
    }

    #[test]
    fn an_empty_centroid_list_is_nothing_to_route_not_kept_f64() {
        let spec = spectrum(SignalContinuity::Centroid, Some(arrays(&[], &[])), None);
        let (_, lattice, outcome) = lattice_route(spec, 1e9);
        assert_eq!(outcome, LatticeOutcome::NoCentroids);
        assert!(lattice.is_none());
        let spec = spectrum(SignalContinuity::Profile, Some(arrays(&[100.0], &[1.0])), Some(&[]));
        let (_, lattice, outcome) = lattice_route(spec, 1e9);
        assert_eq!(outcome, LatticeOutcome::NoCentroids);
        assert!(lattice.is_none());
    }

    // ---- the archive-side contract ------------------------------------------------------------

    #[test]
    fn the_peak_schema_declares_the_four_columns_at_any_scale() {
        for scale in [1e9, 1e4] {
            let f = lattice_tof_index_field(scale);
            // The builder marks the first array of each type primary and shortens its name to
            // `tof_index` at build time; the raw field carries the dtype suffix.
            assert!(f.name().starts_with("tof_index"), "{}", f.name());
            assert_eq!(f.data_type(), &arrow::datatypes::DataType::Int64);
            assert_eq!(
                f.metadata().get("mzpeak:transform_params").map(String::as_str),
                Some(transform_params(scale).as_str())
            );
            let buffers = lattice_peak_schema(scale).build(
                std::sync::Arc::new(arrow::datatypes::Schema::empty()),
                BufferContext::Spectrum,
                false,
            );
            let schema = buffers.schema();
            let point = schema.field_with_name("point").expect("point struct");
            let arrow::datatypes::DataType::Struct(children) = point.data_type() else {
                panic!("point is not a struct: {:?}", point.data_type())
            };
            let dtype = |name: &str| {
                children
                    .iter()
                    .find(|c| c.name() == name)
                    .unwrap_or_else(|| panic!("no `{name}` column in {children:?}"))
                    .data_type()
                    .clone()
            };
            assert_eq!(children.len(), 4, "{children:?}");
            assert_eq!(children[0].name(), "spectrum_index");
            assert_eq!(dtype("spectrum_index"), arrow::datatypes::DataType::UInt64);
            assert_eq!(dtype("tof_index"), arrow::datatypes::DataType::Int64);
            assert_eq!(dtype("mz"), arrow::datatypes::DataType::Float64);
            assert_eq!(dtype("intensity"), arrow::datatypes::DataType::Float32);
            let tof = children.iter().find(|c| c.name() == "tof_index").unwrap();
            assert!(
                BufferName::from_field(BufferContext::Spectrum, tof.clone())
                    .is_some_and(|b| b.transform == Some(BufferTransform::LinearMz)),
                "tof_index must carry the LinearMz transform: {:?}",
                tof.metadata()
            );
        }
    }

    #[test]
    fn the_mz_calibration_block_is_what_the_viewer_gates_on() {
        for scale in [1e9, 1e4] {
            let b = mz_calibration_block(scale, "shimadzu", "MassHigh");
            assert_eq!(b["codec"], "mz-grid");
            assert_eq!(b["applies_to"], "spectra_peaks");
            assert_eq!(b["lossless"], "tof_index");
            assert_eq!(b["mz_from_tof_index"], "tof_index / scale");
            let got = b["scale"].as_f64().expect("scale is a JSON number");
            assert!(got > 0.0 && got == scale);
        }
    }
}
