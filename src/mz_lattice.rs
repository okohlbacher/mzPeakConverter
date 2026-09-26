//! Detection of fixed-point m/z lattices — vendor-neutral.
//!
//! Some acquisitions hand over m/z that are not really floating point at all: they are vendor
//! fixed-point integers divided by a power of ten. Shimadzu's `MassHigh` is an Int64 at 1e-9 Da;
//! its coarse `Mass` fallback is an Int32 at 1e-4 Da (a multiple of 1e5 on the same 1e-9 lattice);
//! LabSolutions' own mzML exporter writes the very same 1e-9 values as f64 `binary`.
//!
//! [`fixed_point_lattice_scale`] recognises such data from the values alone. What the converter
//! DOES with it changed with the vendoring exit (item 1, 2026-09-23): from 0.9.7 to 0.13 the
//! peaks facet of a lattice run was an Int64 point column (`k = round(m/z · scale)`, exact); that
//! layout is ours alone, and the reference implementation's chunk grid cannot hold it (2³² steps
//! of 1e-9 Da is 4.29 Th per chunk). A lattice run now takes upstream's own FITTED linear grid on
//! the peaks facet (`main.rs::lattice_fit_grid_policy`): per-spectrum `MS:1003824` models, every
//! value within 1e-6 Da (≤ 2e-7 Da in practice), −10.6 % against the lattice on DIA_Hela_20ng.
//! Only the CENTROID list is affected; profile arrays keep the treatment they already had.
//! Archives from 0.9.7–0.13 still read (the vendored reader keeps the `LinearMz` decode path).

use mzdata::prelude::*;
use mzdata::spectrum::{MultiLayerSpectrum, SignalContinuity};

/// Vendor fixed-point scales seen in the wild, tried COARSEST FIRST so the chosen `k` is the
/// smallest integer that still reproduces every value: 1e3/1e4/1e5 are Shimadzu's `MASSNUMBER_UNIT`
/// family (1e-4 is the `.lcd` coarse field), 1e9 is `MassHigh` — which is also what LabSolutions'
/// mzML exporter writes, and what a Bruker/Thermo mzML never lands on.
///
/// Coarsest-first is free: a value on the 1e-3 lattice is trivially also on 1e-4 and 1e-5, so the
/// finer scales can only ever be reached by data the coarse ones REJECT.
pub const SCALES: [f64; 4] = [1_000.0, 10_000.0, 100_000.0, 1_000_000_000.0];

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

/// The centroid m/z of a spectrum, i.e. the list that would reach the `spectra_peaks` facet: the
/// peak set when one is attached (a dual Shimadzu `.lcd` scan, or an mzML mzdata chose to pick),
/// otherwise the raw arrays of a Centroid-continuity spectrum. `None` for a profile-only spectrum.
pub fn centroid_mzs(spec: &MultiLayerSpectrum) -> Option<Vec<f64>> {
    if let Some(peaks) = spec.peaks.as_ref() {
        Some(peaks.iter().map(|p| p.mz).collect())
    } else if spec.signal_continuity() == SignalContinuity::Centroid {
        spec.arrays.as_ref().and_then(|a| a.mzs().ok().map(|m| m.to_vec()))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
