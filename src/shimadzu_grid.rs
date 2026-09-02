//! Exact square-root (flight-time) grid for Shimadzu profile axes.
//!
//! A Shimadzu Q-TOF profile axis read at full precision (`MassHigh`, 1e-9 Da) is a TOF lattice:
//! `sqrt(m/z) = c0 + c1·k` for an integer bin `k`, with `c1` constant across the whole run
//! (measured spread 3e-17 on LCMS-9030 data) and `c0` varying per spectrum. The vendor rounds the
//! reconstructed m/z to 1e-9, so a fit that reproduces every point to within that rounding is an
//! exact representation of what the vendor handed over — and it stores one small integer per
//! point instead of an f64 whose deltas are all distinct (HEK profile facet: 57.9 MB → 14.9 MB).
//!
//! Not every spectrum fits: LabSolutions clamps the first or last profile sample of some MS2
//! spectra to the scan-window bound (a spacing of 0.41 / 0.56 bins), off the lattice. Those keep
//! their f64 m/z, per spectrum — nothing is snapped.

/// The span of a profile array with its zero-intensity edge padding removed.
///
/// LabSolutions pads every profile spectrum with a zero-intensity sample at each scan-window
/// bound (e.g. m/z 50.000000 exactly, intensity 0) — off the flight-time grid by construction.
/// The writer drops zero-intensity profile runs anyway, so these points never reach the archive;
/// fitting on them would reject every spectrum as off-grid. Interior zeros are on the grid and
/// are left to the writer's own policy.
pub fn signal_span(intensity: &[f32]) -> (usize, usize) {
    let start = intensity.iter().position(|v| *v > 0.0).unwrap_or(intensity.len());
    let end = intensity.iter().rposition(|v| *v > 0.0).map_or(start, |e| e + 1);
    (start, end.max(start))
}

/// Per-spectrum sqrt grid: `m/z = (c0 + c1·k)²`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SqrtGrid {
    pub c0: f64,
    pub c1: f64,
}

/// Reconstruction tolerance in Da: the vendor's own 1e-9 rounding is ±5e-10, plus f64 slack.
/// Measured worst residuals on exact spectra: 7.2e-10.
pub const TOL: f64 = 1e-9;

/// Least squares of `r` on `k` with centred sums (k reaches 3e5, r is ~8–40).
fn lstsq(k: &[f64], r: &[f64]) -> (f64, f64) {
    let n = k.len() as f64;
    let mk = k.iter().sum::<f64>() / n;
    let mr = r.iter().sum::<f64>() / n;
    let (mut sxy, mut sxx) = (0.0, 0.0);
    for (x, y) in k.iter().zip(r) {
        sxy += (x - mk) * (y - mr);
        sxx += (x - mk) * (x - mk);
    }
    let c1 = if sxx > 0.0 { sxy / sxx } else { 0.0 };
    (mr - c1 * mk, c1)
}

/// The run-wide step `c1` from dense spectra (≥ 64 points), each fitted on its own with the bin
/// assignment taken from its smallest positive spacing; the median of those fits. The minimum
/// spacing itself is a BIASED estimate (its rounding error accumulates to ~5e-4 Da over 2e5 bins),
/// which is why the per-spectrum least-squares step is what gets pooled.
pub fn run_wide_step(spectra: &[Vec<f64>]) -> Option<f64> {
    let mut steps: Vec<f64> = Vec::new();
    for mz in spectra {
        if mz.len() < 64 {
            continue;
        }
        let r: Vec<f64> = mz.iter().map(|v| v.sqrt()).collect();
        let dr: Vec<f64> = r.windows(2).map(|w| w[1] - w[0]).collect();
        let Some(step0) = dr.iter().copied().filter(|d| *d > 0.0).reduce(f64::min) else { continue };
        let mut k = Vec::with_capacity(r.len());
        let mut acc = 0.0;
        k.push(0.0);
        let mut ambiguous = false;
        for d in &dr {
            let m = d / step0;
            if (m - m.round()).abs() > 0.05 {
                ambiguous = true;
                break;
            }
            acc += m.round();
            k.push(acc);
        }
        if ambiguous {
            continue;
        }
        let (_, c1) = lstsq(&k, &r);
        if c1.is_finite() && c1 > 0.0 {
            steps.push(c1);
        }
    }
    if steps.len() < 3 {
        return None;
    }
    steps.sort_by(|a, b| a.partial_cmp(b).unwrap());
    Some(steps[steps.len() / 2])
}

/// Fit one spectrum on the run-wide step: bins are assigned relative to the first point, then
/// `(c0, c1)` are refit by least squares for this spectrum. `Some` only if EVERY point
/// reconstructs within [`TOL`] — otherwise the caller keeps the spectrum's f64 m/z.
pub fn fit_spectrum(mz: &[f64], step: f64) -> Option<(SqrtGrid, Vec<i32>)> {
    if mz.len() < 2 || !(step > 0.0) {
        return None;
    }
    let r: Vec<f64> = mz.iter().map(|v| v.sqrt()).collect();
    let k: Vec<f64> = r.iter().map(|v| ((v - r[0]) / step).round()).collect();
    if k.iter().any(|v| *v < 0.0 || *v > i32::MAX as f64) {
        return None;
    }
    let (c0, c1) = lstsq(&k, &r);
    if !(c1 > 0.0) {
        return None;
    }
    let grid = SqrtGrid { c0, c1 };
    for (kk, m) in k.iter().zip(mz) {
        let rec = c0 + c1 * kk;
        if (rec * rec - m).abs() > TOL {
            return None;
        }
    }
    Some((grid, k.iter().map(|v| *v as i32).collect()))
}

// ---------------------------------------------------------------------------------------------
// Centroid m/z as an exact integer lattice (the viewer's `mz-grid` codec).
//
// The vendor's centroid m/z is `MassHigh`, an Int64 at 1e-9 Da; the coarse `Mass` fallback
// (`MZPC_SHIMADZU_COARSE_MZ=1`, Int32 at 1e-4 Da) is a multiple of 1e5 on the same lattice. So
// `k = round(m/z · 1e9)` is exact by construction and one Int64 per centroid, delta-packed,
// replaces an f64 whose deltas are all distinct. The peaks facet keeps a Float64 `mz` column
// beside it — NULL on lattice rows — so a spectrum that fails the guard is stored, never refused.
// ---------------------------------------------------------------------------------------------

use mzdata::params::Unit;
use mzdata::prelude::*;
use mzdata::spectrum::bindata::{ArrayType, BinaryDataArrayType, DataArray};
use mzdata::spectrum::{BinaryArrayMap, MultiLayerSpectrum, SignalContinuity};
use mzpeak_prototyping::buffer_descriptors::BufferTransform;
use mzpeak_prototyping::peak_series::{INTENSITY_ARRAY, MZ_ARRAY};
use mzpeak_prototyping::writer::ArrayBuffersBuilder;
use mzpeak_prototyping::{BufferContext, BufferName};

/// Lattice scale: `k = round(m/z · LATTICE_SCALE)`.
pub const LATTICE_SCALE: f64 = 1e9;
/// The `mzpeak:transform_params` string the reader multiplies `k` by (`LinearMz`: m/z = p₀·k).
/// Kept as the literal `"1e-9"` so the archive carries exactly what the contract names.
pub const LATTICE_TRANSFORM_PARAMS: &str = "1e-9";
/// Floor of the per-point guard on the scaled value: `|m/z·1e9 − k| < lattice_tolerance(m/z·1e9)`.
/// The product's own rounding grows with m/z (ulp of 1.25e12 is 2.4e-4; of 4e12 — m/z 4000 — it is
/// 4.9e-4) and the coarse `Mass` path rounds twice (`massInt · 1e-4`, then `· 1e9`), so a fixed
/// 1e-3 had a <10 % margin at the top of a wide Q-TOF range; [`lattice_tolerance`] adds a relative
/// term (8 ulp, as `is_fixed_point_lattice` does) so the margin holds at any m/z. An off-lattice
/// value (an interpolated apex not from `MassHigh`) is uniformly off by up to 0.5, so
/// discrimination is unaffected.
pub const LATTICE_TOL: f64 = 1e-3;

/// The guard for one scaled value: `max(LATTICE_TOL, 8 ulp of the scaled value)`.
pub fn lattice_tolerance(scaled: f64) -> f64 {
    (scaled.abs() * 8.0 * f64::EPSILON).max(LATTICE_TOL)
}

/// `Some(k)` when EVERY centroid lies on the 1e-9 lattice within [`lattice_tolerance`] and `k` is
/// non-decreasing; `None` (keep f64 m/z) for an empty list, a non-finite or negative value, an
/// off-lattice point, or a descending pair. Nothing is snapped.
pub fn centroid_lattice(mz: &[f64]) -> Option<Vec<i64>> {
    if mz.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(mz.len());
    let mut prev = i64::MIN;
    for &m in mz {
        if !m.is_finite() || m < 0.0 {
            return None;
        }
        let scaled = m * LATTICE_SCALE;
        let k = scaled.round();
        if (scaled - k).abs() >= lattice_tolerance(scaled) {
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

/// The `point.tof_index` field: Int64, nonstandard `tof_index`, `LinearMz` with params `"1e-9"`.
/// The name ends in `_index` ON PURPOSE — the vendored writer disables the dictionary and applies
/// DELTA_BINARY_PACKED to `*_index` columns (writer/base.rs `spectrum_data_writer_props`).
pub fn lattice_tof_index_field() -> std::sync::Arc<arrow::datatypes::Field> {
    let base = BufferName::new(
        BufferContext::Spectrum,
        ArrayType::nonstandard("tof_index"),
        BinaryDataArrayType::Int64,
    )
    .with_transform(Some(BufferTransform::LinearMz))
    .to_field();
    let mut md = base.metadata().clone();
    md.insert("mzpeak:transform_params".to_string(), LATTICE_TRANSFORM_PARAMS.to_string());
    std::sync::Arc::new((*base).clone().with_metadata(md))
}

/// Custom `spectra_peaks` schema for the Shimadzu native lane (point layout, prefix `point`):
/// `spectrum_index` UInt64, `tof_index` Int64 (the lattice), `mz` Float64 (the per-spectrum f64
/// fallback, NULL on lattice rows), `intensity` Float32. The BufferNames MUST match the DataArrays
/// [`lattice_peak_arrays`] builds, or the columns spill to auxiliary.
pub fn lattice_peak_schema() -> ArrayBuffersBuilder {
    ArrayBuffersBuilder::default()
        .prefix("point")
        .with_context(BufferContext::Spectrum)
        .add_field(BufferContext::Spectrum.index_field())
        .add_field(lattice_tof_index_field())
        .add_field(MZ_ARRAY.to_field())
        .add_field(INTENSITY_ARRAY.to_field())
}

/// The `mz_calibration` index block the viewer's `mz-grid` codec gates on (`scale` MUST be a JSON
/// number > 0).
pub fn mz_calibration_block() -> serde_json::Value {
    serde_json::json!({
        "codec": "mz-grid",
        "scale": LATTICE_SCALE,
        "vendor": "shimadzu",
        "lossless": "tof_index",
        "applies_to": "spectra_peaks",
        "mz_from_tof_index": "tof_index / scale",
        "source": "MassHigh (Int64, 1e-9 Da); Mass (Int32, 1e-4 Da) under MZPC_SHIMADZU_COARSE_MZ=1 lies on the same lattice",
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

/// Per-spectrum routing of the CENTROID list. The list is the peak set when the spectrum also
/// carries a profile (a dual `.lcd`), or the raw arrays of a Centroid spectrum otherwise. On
/// success the spectrum is returned UNCHANGED (its peak set / raw arrays still feed the metadata
/// summaries — counts, TIC, base peak) together with the lattice arrays the writer sends to the
/// peak facet through `write_spectrum_with_peak_arrays`; otherwise `None` and the ordinary
/// `write_spectrum` path stores the f64 m/z in the same facet's `mz` column.
pub fn lattice_route(
    spec: MultiLayerSpectrum,
) -> (MultiLayerSpectrum, Option<BinaryArrayMap>, LatticeOutcome) {
    let centroids: Option<(Vec<f64>, Vec<f32>)> = if let Some(peaks) = spec.peaks.as_ref() {
        Some(peaks.iter().map(|p| (p.mz, p.intensity)).unzip())
    } else if spec.signal_continuity() == SignalContinuity::Centroid {
        spec.arrays.as_ref().and_then(|a| match (a.mzs(), a.intensities()) {
            (Ok(mz), Ok(inten)) => Some((mz.to_vec(), inten.to_vec())),
            _ => None,
        })
    } else {
        None
    };
    // An empty list is not an off-lattice one: report it as nothing-to-route so the summary
    // counters do not call every empty scan "kept f64 m/z".
    let Some((mz, intensity)) = centroids.filter(|(mz, _)| !mz.is_empty()) else {
        return (spec, None, LatticeOutcome::NoCentroids);
    };
    let Some(k) = centroid_lattice(&mz) else {
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

    /// Synthetic LCMS-9030-like axis: c1 = 9.16e-5, m/z rounded to 1e-9 as the vendor does.
    fn synth(c0: f64, c1: f64, bins: &[i32]) -> Vec<f64> {
        bins.iter()
            .map(|&k| {
                let r = c0 + c1 * k as f64;
                (r * r * 1e9).round() / 1e9
            })
            .collect()
    }

    #[test]
    fn exact_grid_round_trips_within_the_vendor_rounding() {
        let c1 = 0.000091602119892;
        // Irregular bin gaps (1..=4), as a real profile axis has after the vendor drops empty bins —
        // a uniform stride would look like a lattice with a multiple of the true step.
        let mut bins: Vec<i32> = Vec::new();
        let mut k = 0i32;
        for i in 0..1200 {
            k += 1 + (i * 7 % 4) as i32;
            bins.push(k);
        }
        let dense: Vec<Vec<f64>> = (0..5).map(|s| synth(8.0 + 0.01 * s as f64, c1, &bins)).collect();
        let step = run_wide_step(&dense).expect("step");
        assert!((step - c1).abs() < 1e-12, "step {step} vs {c1}");
        let (g, k) = fit_spectrum(&dense[2], step).expect("fits");
        assert!((g.c1 - c1).abs() < 1e-12);
        // bins are relative to the first point
        let rel: Vec<i32> = bins.iter().map(|b| b - bins[0]).collect();
        assert_eq!(k, rel);
        for (kk, m) in k.iter().zip(&dense[2]) {
            let r = g.c0 + g.c1 * *kk as f64;
            assert!((r * r - m).abs() <= TOL);
        }
    }

    #[test]
    fn zero_intensity_pad_points_are_outside_the_signal_span() {
        let inten = [0.0f32, 0.0, 91.0, 215.0, 299.0, 0.0, 58.0, 0.0];
        assert_eq!(signal_span(&inten), (2, 7));
        assert_eq!(signal_span(&[0.0f32, 0.0]), (2, 2));
        assert_eq!(signal_span(&[5.0f32]), (0, 1));
    }

    #[test]
    fn a_clamped_edge_sample_routes_the_spectrum_to_f64() {
        let c1 = 0.000091602119892;
        let bins: Vec<i32> = (0..2000).collect();
        let mut mz = synth(8.0, c1, &bins);
        mz[0] = 70.0; // vendor clamp to the scan-window bound: 0.41 bins off the lattice
        mz.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert!(fit_spectrum(&mz, c1).is_none());
    }

    #[test]
    fn coarse_lattice_data_does_not_fit() {
        // Mass at 1e-4 (the coarse field): residuals ~5e-5 ≫ TOL, so the grid is refused and the
        // lane falls back to plain f64 for the whole file.
        let c1 = 0.000091602119892;
        let bins: Vec<i32> = (0..2000).collect();
        let coarse: Vec<f64> = synth(8.0, c1, &bins).iter().map(|m| (m * 1e4).round() / 1e4).collect();
        assert!(fit_spectrum(&coarse, c1).is_none());
    }
}

#[cfg(test)]
mod lattice_tests {
    use super::*;
    use mzdata::spectrum::SpectrumDescription;
    use mzpeak_prototyping::writer::ArrayBufferWriter;
    use mzpeaks::{CentroidPeak, PeakSet};

    fn on_lattice(ks: &[i64]) -> Vec<f64> {
        ks.iter().map(|&k| k as f64 * 1e-9).collect()
    }

    #[test]
    fn masshigh_values_recover_their_integer_exactly() {
        // 1e-9 Da lattice up to m/z 1250 (k ≈ 1.25e12, still exact in f64).
        let ks = [50_000_000_000i64, 123_456_789_012, 999_999_999_999, 1_250_123_456_789];
        assert_eq!(centroid_lattice(&on_lattice(&ks)).unwrap(), ks);
        // Equal neighbours (a duplicated centroid) are non-decreasing, hence allowed.
        assert_eq!(centroid_lattice(&on_lattice(&[7, 7, 8])).unwrap(), vec![7, 7, 8]);
    }

    #[test]
    fn coarse_mass_values_lie_on_the_same_lattice() {
        // MZPC_SHIMADZU_COARSE_MZ=1: `Mass` at 1e-4 Da → multiples of 1e5 on the 1e-9 lattice.
        let coarse: Vec<f64> = [500_001i64, 500_002, 12_345_678].iter().map(|&m| m as f64 * 1e-4).collect();
        let k = centroid_lattice(&coarse).unwrap();
        assert_eq!(k, vec![50_000_100_000, 50_000_200_000, 1_234_567_800_000]);
        assert!(k.iter().all(|v| v % 100_000 == 0));
    }

    #[test]
    fn an_off_lattice_point_keeps_the_spectrum_in_f64() {
        let mut mz = on_lattice(&[100_000_000_000, 200_000_000_000, 300_000_000_000]);
        mz[1] += 0.3e-9; // an interpolated apex, 0.3 of a lattice step off
        assert!(centroid_lattice(&mz).is_none());
        // Inside the guard is still accepted (the f64 product's own rounding).
        let mut mz = on_lattice(&[100_000_000_000, 200_000_000_000]);
        mz[1] += 0.5e-12;
        assert!(centroid_lattice(&mz).is_some());
    }

    #[test]
    fn the_guard_scales_with_mz_at_the_top_of_a_wide_range() {
        // MassHigh at m/z 4000–4500 (k up to 4.5e12): 8 ulp there is ≈7e-3, still ≪ 0.5.
        let ks: Vec<i64> = (0..4000).map(|i| 4_000_000_000_000 + i * 123_456_789).collect();
        assert_eq!(centroid_lattice(&on_lattice(&ks)).unwrap(), ks);
        // The coarse `Mass` path rounds twice (massInt·1e-4, then ·1e9) — its worst case near
        // m/z 4000 is ≈9e-4, inside the relative guard where the fixed 1e-3 floor had <10 % margin.
        let ints: Vec<i64> = (39_990_000..40_010_000).step_by(7).collect();
        let coarse: Vec<f64> = ints.iter().map(|&m| m as f64 * 1e-4).collect();
        let k = centroid_lattice(&coarse).expect("coarse Mass at m/z 4000 is on the lattice");
        assert_eq!(k, ints.iter().map(|m| m * 100_000).collect::<Vec<_>>());
        // An interpolated apex 0.3 of a step off at m/z 4000 is still refused.
        let mut mz = on_lattice(&[4_000_000_000_000, 4_000_000_000_001]);
        mz[1] += 0.3e-9;
        assert!(centroid_lattice(&mz).is_none());
        assert!(lattice_tolerance(4e12) > LATTICE_TOL && lattice_tolerance(4e12) < 0.01);
        assert_eq!(lattice_tolerance(1e11), LATTICE_TOL);
    }

    #[test]
    fn descending_empty_and_non_finite_input_is_refused() {
        assert!(centroid_lattice(&on_lattice(&[5, 4])).is_none());
        assert!(centroid_lattice(&[]).is_none());
        assert!(centroid_lattice(&[100.0, f64::NAN]).is_none());
        assert!(centroid_lattice(&[-1e-9]).is_none());
    }

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
        let (spec, lattice, outcome) = lattice_route(spec);
        assert_eq!(outcome, LatticeOutcome::Lattice);
        let lattice = lattice.expect("lattice arrays");
        assert_eq!(tof_index_of(&lattice), vec![100_000_123_456, 200_000_000_001]);
        assert_eq!(lattice.intensities().unwrap().to_vec(), vec![5.0, 7.0]);
        // The spectrum itself is untouched: the profile still feeds the data facet and the peak
        // set still feeds `number_of_peaks` / base peak.
        assert_eq!(spec.signal_continuity(), SignalContinuity::Profile);
        assert_eq!(spec.arrays.as_ref().unwrap().mzs().unwrap().len(), 3);
        assert_eq!(spec.peaks.as_ref().unwrap().len(), 2);
    }

    #[test]
    fn a_centroid_only_spectrum_routes_its_raw_arrays() {
        let raw = arrays(&[100.000_123_456, 200.000_000_001], &[5.0, 7.0]);
        let spec = spectrum(SignalContinuity::Centroid, Some(raw), None);
        let (spec, lattice, outcome) = lattice_route(spec);
        assert_eq!(outcome, LatticeOutcome::Lattice);
        assert_eq!(tof_index_of(&lattice.unwrap()), vec![100_000_123_456, 200_000_000_001]);
        assert!(spec.arrays.is_some());
    }

    #[test]
    fn off_lattice_centroids_keep_f64_and_profile_only_has_nothing_to_route() {
        let raw = arrays(&[100.000_123_456_3, 200.0], &[5.0, 7.0]);
        let spec = spectrum(SignalContinuity::Centroid, Some(raw), None);
        let (_, lattice, outcome) = lattice_route(spec);
        assert_eq!(outcome, LatticeOutcome::KeptF64);
        assert!(lattice.is_none());

        let spec = spectrum(SignalContinuity::Profile, Some(arrays(&[100.0], &[1.0])), None);
        let (_, lattice, outcome) = lattice_route(spec);
        assert_eq!(outcome, LatticeOutcome::NoCentroids);
        assert!(lattice.is_none());
    }

    #[test]
    fn an_empty_centroid_list_is_nothing_to_route_not_kept_f64() {
        // A 0-point scan: shimadzu.rs builds it as Centroid with empty raw arrays …
        let spec = spectrum(SignalContinuity::Centroid, Some(arrays(&[], &[])), None);
        let (_, lattice, outcome) = lattice_route(spec);
        assert_eq!(outcome, LatticeOutcome::NoCentroids);
        assert!(lattice.is_none());
        // … and a dual spectrum with an empty peak set is the same case.
        let spec = spectrum(SignalContinuity::Profile, Some(arrays(&[100.0], &[1.0])), Some(&[]));
        let (_, lattice, outcome) = lattice_route(spec);
        assert_eq!(outcome, LatticeOutcome::NoCentroids);
        assert!(lattice.is_none());
    }

    #[test]
    fn the_peak_schema_declares_the_four_columns() {
        let f = lattice_tof_index_field();
        // The builder marks the first array of each type primary and shortens its name to
        // `tof_index` at build time (the mzML lane's Int32 field goes `tof_index_i32` → `tof_index`
        // the same way); the raw field carries the dtype suffix.
        assert!(f.name().starts_with("tof_index"), "{}", f.name());
        assert_eq!(f.data_type(), &arrow::datatypes::DataType::Int64);
        assert_eq!(f.metadata().get("mzpeak:transform_params").map(String::as_str), Some("1e-9"));
        let buffers = lattice_peak_schema().build(
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
        assert_eq!(tof.metadata().get("mzpeak:transform_params").map(String::as_str), Some("1e-9"));
        assert!(
            BufferName::from_field(BufferContext::Spectrum, tof.clone())
                .is_some_and(|b| b.transform == Some(BufferTransform::LinearMz)),
            "tof_index must carry the LinearMz transform: {:?}",
            tof.metadata()
        );
    }

    #[test]
    fn the_mz_calibration_block_is_what_the_viewer_gates_on() {
        let b = mz_calibration_block();
        assert_eq!(b["codec"], "mz-grid");
        assert_eq!(b["applies_to"], "spectra_peaks");
        assert_eq!(b["lossless"], "tof_index");
        assert_eq!(b["mz_from_tof_index"], "tof_index / scale");
        let scale = b["scale"].as_f64().expect("scale is a JSON number");
        assert!(scale > 0.0 && scale == 1e9);
    }
}
