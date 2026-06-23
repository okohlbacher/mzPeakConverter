//! TOF-grid m/z encoding for SCIEX (and other exact-lattice TOF) mzML.
//!
//! SCIEX TripleTOF/ZenoTOF profile m/z lie on an EXACT integer flight-time grid: a digitizer
//! samples flight time at a constant clock, and the vendor's TOF→m/z calibration is
//! `m/z = (c0 + c1·t)²`, i.e. `sqrt(m/z) = c0 + c1·k` for an integer flight-time index `k`. One
//! `(c0, c1)` per run reconstructs every spectrum's m/z. So instead of storing f64 m/z
//! (5.91 B/peak after zstd) we store `tof_index : Int32 = k` (≈0.21 B/peak after
//! DELTA_BINARY_PACKED) and a per-run `{c0, c1}` calibration block; the reader recovers
//! `m/z = (c0 + c1·k)²`.
//!
//! This is only applied when a strict lossless check passes (SCIEX TOF passes; Orbitrap / QqQ-SRM
//! fail and fall back to f64 m/z). The acceptance gate is on ppm error, matching the proven
//! research (SCIEX TripleTOF reconstructs to <0.1 ppm with a 2-coefficient per-run calibration).

/// A fitted per-run TOF grid: `sqrt(m/z) = c0 + c1·k`, `k` a non-negative integer (`tof_index`).
/// Reconstruction: `m/z = (c0 + c1·k)²`.
#[derive(Clone, Copy, Debug)]
pub struct TofGrid {
    pub c0: f64,
    pub c1: f64,
}

/// Acceptance tolerance for the lossless grid fit. SCIEX TOF reconstructs to ~0.05–0.1 ppm with a
/// fine enough grid; Orbitrap/QqQ-SRM miss by orders of magnitude. 0.2 ppm is the gate from the
/// research spike (`WATERS_SCIEX_COMPACT.md`): SCIEX passes, non-TOF fails. We also require the
/// resulting `tof_index` to fit comfortably in Int32.
pub const PPM_TOL: f64 = 0.2;
/// Keep k within signed-31-bit range with a safety margin (DELTA_BINARY_PACKED makes the absolute
/// magnitude almost free, but the column is Int32 so k MUST fit).
pub const MAX_K: i64 = 1_900_000_000;

/// Result of attempting to fit a run-wide TOF grid over sampled m/z arrays.
pub struct FitOutcome {
    pub grid: TofGrid,
    /// Max reconstruction error over all sampled points, in ppm.
    pub max_ppm: f64,
    /// Median reconstruction error over all sampled points, in ppm.
    pub median_ppm: f64,
    /// Largest `tof_index` produced over the sampled points (for the Int32-range check).
    pub max_k: i64,
}

impl TofGrid {
    /// Reconstruct m/z from a `tof_index`.
    #[inline]
    pub fn mz(&self, k: i32) -> f64 {
        let r = self.c0 + self.c1 * (k as f64);
        r * r
    }

    /// Map a calibrated f64 m/z to its nearest grid `tof_index`. Returns `None` if the value is
    /// non-positive or the index would not fit in Int32.
    #[inline]
    pub fn tof_index(&self, mz: f64) -> Option<i32> {
        if !(mz > 0.0) {
            return None;
        }
        let k = ((mz.sqrt() - self.c0) / self.c1).round();
        if !k.is_finite() || k < 0.0 || k > MAX_K as f64 {
            return None;
        }
        Some(k as i32)
    }
}

/// Estimate the single-step grid spacing in sqrt-m/z space from one spectrum's (unsorted) m/z.
/// Profile TOF spectra are dense and monotone, so the typical small consecutive gap is one grid
/// step. Returns `None` if there aren't enough usable points.
fn base_step(mzs: &[f64]) -> Option<f64> {
    let mut s: Vec<f64> = mzs.iter().filter(|&&m| m > 0.0).map(|&m| m.sqrt()).collect();
    if s.len() < 8 {
        return None;
    }
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mut diffs: Vec<f64> = s.windows(2).map(|w| w[1] - w[0]).filter(|&d| d > 1e-10).collect();
    if diffs.len() < 4 {
        return None;
    }
    diffs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    // Use a low-percentile diff as the single-step estimate (dense regions step by exactly one grid
    // unit; gaps are integer multiples). The 30th percentile robustly lands on the single-step gap.
    let idx = diffs.len() * 30 / 100;
    let step = diffs[idx];
    if step > 0.0 { Some(step) } else { None }
}

/// Least-squares fit of `sqrt(m/z) = c0 + c1·k` given a fixed step (k = round((sqrt(mz)-s_min)/step)).
fn fit_with_step(sqrt_mz: &[f64], s_min: f64, step: f64) -> (f64, f64) {
    // Solve [1 k][c0 c1]^T = sqrt_mz in least squares via normal equations.
    let n = sqrt_mz.len() as f64;
    let mut sum_k = 0.0;
    let mut sum_kk = 0.0;
    let mut sum_y = 0.0;
    let mut sum_ky = 0.0;
    for &y in sqrt_mz {
        let k = ((y - s_min) / step).round();
        sum_k += k;
        sum_kk += k * k;
        sum_y += y;
        sum_ky += k * y;
    }
    let det = n * sum_kk - sum_k * sum_k;
    if det.abs() < 1e-30 {
        return (s_min, step);
    }
    let c0 = (sum_kk * sum_y - sum_k * sum_ky) / det;
    let c1 = (n * sum_ky - sum_k * sum_y) / det;
    (c0, c1)
}

/// Evaluate a grid against pooled m/z: max/median ppm error and max k.
fn evaluate(grid: &TofGrid, mzs: &[f64]) -> (f64, f64, i64) {
    let mut ppms: Vec<f64> = Vec::with_capacity(mzs.len());
    let mut max_k: i64 = 0;
    for &mz in mzs {
        if mz <= 0.0 {
            continue;
        }
        let k = ((mz.sqrt() - grid.c0) / grid.c1).round();
        max_k = max_k.max(k as i64);
        let rec = grid.mz(k as i32);
        ppms.push((rec - mz).abs() / mz * 1e6);
    }
    if ppms.is_empty() {
        return (f64::INFINITY, f64::INFINITY, max_k);
    }
    let max = ppms.iter().cloned().fold(0.0f64, f64::max);
    ppms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = ppms[ppms.len() / 2];
    (max, median, max_k)
}

/// Try to fit a run-wide TOF grid over the pooled sampled m/z from several spectra.
///
/// Strategy: estimate a coarse single-step from the first usable spectrum, then refine the step by
/// successive halving (the lattice is finer than the densest observed gap — each halving roughly
/// halves the reconstruction error, which is the signature of a perfect grid vs. instrument jitter).
/// Accept the finest grid that (a) reconstructs every pooled point within `PPM_TOL` and (b) keeps k
/// within Int32. Return `None` if no refinement meets the tolerance (non-TOF / jittered data).
pub fn fit(sample_spectra: &[Vec<f64>]) -> Option<FitOutcome> {
    // Pool all sampled m/z (different spectra share the per-run lattice).
    let mut pooled: Vec<f64> = Vec::new();
    let mut first_step: Option<f64> = None;
    for spec in sample_spectra {
        if first_step.is_none() {
            first_step = base_step(spec);
        }
        pooled.extend(spec.iter().copied().filter(|&m| m > 0.0));
    }
    let base = first_step?;
    if pooled.len() < 16 {
        return None;
    }
    let sqrt_mz: Vec<f64> = pooled.iter().map(|&m| m.sqrt()).collect();
    let s_min = sqrt_mz.iter().cloned().fold(f64::INFINITY, f64::min);

    let mut best: Option<FitOutcome> = None;
    // Refine the step by halving. Stop once k would overflow Int32 or we've gone deep enough.
    for shift in 0..12u32 {
        let step = base / (1u64 << shift) as f64;
        let (c0, c1) = fit_with_step(&sqrt_mz, s_min, step);
        if !(c1 > 0.0) || !c0.is_finite() {
            continue;
        }
        let grid = TofGrid { c0, c1 };
        let (max_ppm, median_ppm, max_k) = evaluate(&grid, &pooled);
        if max_k > MAX_K {
            break; // finer grids only push k higher
        }
        let outcome = FitOutcome { grid, max_ppm, median_ppm, max_k };
        // Keep refining while it helps; record the best (finest passing) grid.
        let improved = match &best {
            None => true,
            Some(b) => max_ppm < b.max_ppm,
        };
        if improved {
            best = Some(outcome);
        }
        if max_ppm <= PPM_TOL {
            // Good enough and lossless within tolerance; one more refinement only tightens it, but
            // we already meet the gate — take the first passing fine grid to keep k smaller.
            break;
        }
    }

    match best {
        Some(b) if b.max_ppm <= PPM_TOL && b.max_k <= MAX_K => Some(b),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic SCIEX-like exact TOF lattice: m/z = (c0 + c1·k)² for integer k. Must fit losslessly.
    #[test]
    fn exact_lattice_fits_lossless() {
        let c0 = 17.32;
        let c1 = 1.6e-5;
        let truth = TofGrid { c0, c1 };
        // Build a dense-ish spectrum of lattice points.
        let mut spec = Vec::new();
        let mut k = 100_000i32;
        while k < 1_200_000 {
            spec.push(truth.mz(k));
            // dense regions step by 1, with occasional gaps
            k += if k % 7 == 0 { 3 } else { 1 };
        }
        let out = fit(&[spec.clone(), spec.clone()]).expect("should fit exact lattice");
        assert!(out.max_ppm <= PPM_TOL, "max_ppm {} over tol", out.max_ppm);
        // Round-trip every point losslessly within tolerance.
        for &mz in &spec {
            let k = out.grid.tof_index(mz).expect("index");
            let rec = out.grid.mz(k);
            let ppm = (rec - mz).abs() / mz * 1e6;
            assert!(ppm <= PPM_TOL, "roundtrip ppm {ppm} for mz {mz}");
        }
    }

    /// Non-TOF data (random-ish m/z not on any lattice, e.g. Orbitrap/QqQ-SRM) must be rejected.
    #[test]
    fn non_lattice_rejected() {
        // SRM-style: a handful of arbitrary transition m/z, plus noise — no flight-time lattice.
        let mut spec = Vec::new();
        let mut x = 100.0f64;
        for i in 0..2000 {
            // irregular spacing that is not an affine function of an integer in sqrt space
            x += 0.013 + 0.0007 * ((i as f64) * 1.7).sin().abs();
            spec.push(x);
        }
        assert!(fit(&[spec.clone(), spec]).is_none(), "non-lattice data must reject the grid");
    }

    #[test]
    fn tof_index_rejects_bad_mz() {
        let g = TofGrid { c0: 17.32, c1: 1.6e-5 };
        assert!(g.tof_index(-1.0).is_none());
        assert!(g.tof_index(0.0).is_none());
        assert!(g.tof_index(500.0).is_some());
    }
}
