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
