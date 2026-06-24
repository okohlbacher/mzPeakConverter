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

/// Acceptance tolerance for the grid fit at the NATURAL lattice (the compression-optimal grid where
/// stored samples are one step apart). At that grid the residual is pure grid-snap quantization — NOT
/// instrument jitter — and it halves with each grid refinement, the signature of a true flight-time
/// lattice. The natural-grid quantization depends on the instrument's flight-time step: measured
/// **TripleTOF ≈ 1.75 ppm, ZenoTOF 7600 ≈ 3.94 ppm** (its step is coarser). 5 ppm admits both — and
/// is well within TOF mass accuracy — while the `median_dk == 1` density gate rejects non-TOF data
/// (Orbitrap/QqQ-SRM, off by tens to hundreds of ppm at any dense grid). For a tighter bound a finer
/// grid (smaller ppm, larger `tof_index`) is selected automatically by the refinement fallback.
pub const PPM_TOL: f64 = 5.0;
/// Keep k within signed-31-bit range with a safety margin (DELTA_BINARY_PACKED makes the absolute
/// magnitude almost free, but the column is Int32 so k MUST fit).
pub const MAX_K: i64 = 1_900_000_000;
/// A genuine flight-time lattice is DENSE: within a spectrum, adjacent points sit a small integer
/// number of grid steps apart (median `dk` ≈ 1–8 for SCIEX profile). A non-lattice (Orbitrap/SRM,
/// or smoothly-varying m/z) can be made to *reconstruct* within `PPM_TOL` only by refining the grid
/// arbitrarily fine, which blows the median `dk` up into the hundreds (every point lands on a
/// distant fractional node). This is the decisive discriminator: refine-to-fit cannot fake a dense
/// lattice. Reject if the per-spectrum median `dk` exceeds this.
pub const MAX_MEDIAN_DK: i64 = 4;

/// Result of attempting to fit a run-wide TOF grid over sampled m/z arrays.
pub struct FitOutcome {
    pub grid: TofGrid,
    /// Max reconstruction error over all sampled points, in ppm.
    pub max_ppm: f64,
    /// Median reconstruction error over all sampled points, in ppm.
    pub median_ppm: f64,
    /// Largest `tof_index` produced over the sampled points (for the Int32-range check).
    pub max_k: i64,
    /// Median integer step between adjacent points within a representative spectrum (lattice density
    /// check — see `MAX_MEDIAN_DK`).
    pub median_dk: i64,
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
        // k may be negative — valid lattice points below the c0 reference (e.g. low-m/z MS2 fragment
        // peaks). Int32 + DELTA_BINARY_PACKED store signed values fine; only require it to fit Int32.
        if !k.is_finite() || k.abs() > MAX_K as f64 {
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

/// Median integer step `dk` between adjacent points of one spectrum under `grid`. Sort the
/// spectrum's m/z, map each to its grid index, and take the median of consecutive index gaps.
/// Small (≈1–8) for a dense flight-time lattice; large for a refine-to-fit non-lattice.
fn median_dk(grid: &TofGrid, spec: &[f64]) -> i64 {
    let mut ks: Vec<i64> = spec
        .iter()
        .filter(|&&m| m > 0.0)
        .map(|&m| ((m.sqrt() - grid.c0) / grid.c1).round() as i64)
        .collect();
    if ks.len() < 2 {
        return i64::MAX;
    }
    ks.sort_unstable();
    let mut dks: Vec<i64> = ks.windows(2).map(|w| w[1] - w[0]).filter(|&d| d > 0).collect();
    if dks.is_empty() {
        return i64::MAX;
    }
    dks.sort_unstable();
    dks[dks.len() / 2]
}

/// Robust lattice-density across SEVERAL probe spectra: the MEDIAN of per-spectrum `median_dk`.
/// A genuine per-run flight-time lattice gives a small `median_dk` in EVERY dense spectrum; a
/// non-lattice that a refined grid happens to fit in one spectrum does not hold across spectra.
/// Taking the median over probes prevents a single coincidentally-dense spectrum from passing the
/// density gate (the single-spectrum false-accept that this guards against).
fn robust_median_dk(grid: &TofGrid, probes: &[&[f64]]) -> i64 {
    let mut per: Vec<i64> = probes.iter().map(|p| median_dk(grid, p)).collect();
    if per.is_empty() {
        return i64::MAX;
    }
    per.sort_unstable();
    per[per.len() / 2]
}

/// A per-run lattice claim needs corroboration across spectra. One dense spectrum can be made to
/// refine-fit by coincidence (its `median_dk` is then unreliable — see the single-FT-scan Orbitrap
/// false-accept). Require at least this many distinct dense probe spectra before accepting a grid.
pub const MIN_PROBES: usize = 2;

/// Fit a grid to ONE spectrum's recalibrated f64 m/z, for native vendor readers that expose only
/// decoded f64 (SCIEX Clearcore2). Inverts `sqrt(m/z)=c0+c1·k` PER SPECTRUM, so per-scan c0 drift —
/// which defeats a single run-wide grid (the ZenoTOF failure) — is absorbed into each spectrum's own
/// coefficients. Returns `(grid, tof_index per input point in order, max ppm)` iff every point
/// reconstructs within `PPM_TOL` and fits Int32; `None` otherwise (caller keeps that spectrum f64).
/// No non-TOF rejection here — the caller already knows the instrument is TOF; this is just the
/// lossless gate.
#[cfg_attr(not(windows), allow(dead_code))] // only the cfg(windows) SCIEX grid path calls it
pub fn fit_one(mzs: &[f64]) -> Option<(TofGrid, Vec<i32>, f64)> {
    let base = base_step(mzs)?;
    let sqrt_mz: Vec<f64> = mzs.iter().filter(|&&m| m > 0.0).map(|&m| m.sqrt()).collect();
    if sqrt_mz.len() < 8 {
        return None;
    }
    let s_min = sqrt_mz.iter().cloned().fold(f64::INFINITY, f64::min);
    // The natural digitizer step is ~`base`; accept the COARSEST grid (smallest k → best compression)
    // whose every point reconstructs within PPM_TOL, falling to finer multiples only if the natural
    // quantization is too coarse.
    for mult in [1.0, 0.5, 0.25, 0.125] {
        let step = base * mult;
        let (c0, c1) = fit_with_step(&sqrt_mz, s_min, step);
        if !(c1 > 0.0) || !c0.is_finite() {
            continue;
        }
        let grid = TofGrid { c0, c1 };
        let (max_ppm, _median, max_k) = evaluate(&grid, mzs);
        if max_ppm <= PPM_TOL && max_k <= MAX_K {
            let mut idx = Vec::with_capacity(mzs.len());
            for &m in mzs {
                idx.push(grid.tof_index(m)?);
            }
            return Some((grid, idx, max_ppm));
        }
    }
    None
}

/// Like [`fit_one`] but with a FIXED run-wide `c1` (the SCIEX digitizer clock is global; only the
/// per-scan offset `c0` drifts). Fits just `c0` for the given `c1`, so even SPARSE spectra (SWATH/DIA
/// MS2 windows) that can't self-estimate the step still grid against the shared clock. Returns
/// `(grid, tof_index, max ppm)` iff every point reconstructs within `PPM_TOL`.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn fit_one_c1(mzs: &[f64], c1: f64) -> Option<(TofGrid, Vec<i32>, f64)> {
    if !(c1 > 0.0) {
        return None;
    }
    let s_min = mzs
        .iter()
        .filter(|&&m| m > 0.0)
        .map(|&m| m.sqrt())
        .fold(f64::INFINITY, f64::min);
    if !s_min.is_finite() {
        return None;
    }
    // c0 = mean(√mz − c1·k) with k = round((√mz − s_min)/c1): the best-fit offset for the fixed step.
    let (mut sum, mut cnt) = (0.0f64, 0.0f64);
    for &m in mzs {
        if m > 0.0 {
            let y = m.sqrt();
            let k = ((y - s_min) / c1).round();
            sum += y - c1 * k;
            cnt += 1.0;
        }
    }
    if cnt < 4.0 {
        return None;
    }
    let grid = TofGrid { c0: sum / cnt, c1 };
    let mut idx = Vec::with_capacity(mzs.len());
    let mut max_ppm = 0.0f64;
    for &m in mzs {
        let k = grid.tof_index(m)?;
        let rec = grid.mz(k);
        let ppm = (rec - m).abs() / m * 1e6;
        if ppm > max_ppm {
            max_ppm = ppm;
        }
        idx.push(k);
    }
    if max_ppm <= PPM_TOL {
        Some((grid, idx, max_ppm))
    } else {
        None
    }
}

/// Try to fit a run-wide TOF grid over the pooled sampled m/z from several spectra.
///
/// Strategy — target the NATURAL lattice, not the finest one:
///
/// The fundamental flight-time clock is far finer than the spacing of the stored samples. Sampling
/// at the clock would reconstruct m/z to ~0 ppm but make `tof_index` huge and its deltas large
/// (poor compression). Sampling at the *data spacing* (the coarsest grid where adjacent dense points
/// are one step apart, `median_dk == 1`) is the sweet spot: `tof_index` deltas are tiny
/// (DELTA_BINARY_PACKED shrinks the column to ~0.2 B/peak — the whole point) while the residual is
/// pure grid-snap quantization, bounded at SCIEX TOF instrument accuracy (~1–2 ppm). Each refinement
/// halves the ppm but inflates the column; the natural lattice is the right trade-off and matches
/// the measured 0.21 B/peak in the research spike.
///
/// We scan coarse→fine. The natural lattice is the first grid whose per-spectrum `median_dk` is 1.
/// Accept it iff its max reconstruction error is within `PPM_TOL` (a generous SCIEX-accuracy bound)
/// AND `tof_index` fits Int32. A non-TOF input (Orbitrap/QqQ-SRM) never produces a `median_dk == 1`
/// grid within `PPM_TOL` — its points aren't on any flight-time lattice — so it is rejected (the
/// `median_dk` gate is the decisive discriminator: refine-to-fit cannot fake a dense lattice).
pub fn fit(sample_spectra: &[Vec<f64>]) -> Option<FitOutcome> {
    // Pool all sampled m/z (different spectra share the per-run lattice). The base single-step is
    // estimated from ONE spectrum (pooling interleaves spectra and corrupts the step estimate).
    let mut pooled: Vec<f64> = Vec::new();
    let mut first_step: Option<f64> = None;
    // ALL dense spectra are probes for the cross-spectrum density check (not just the first): a true
    // per-run flight-time lattice is dense in EVERY spectrum. base_step succeeding is the "dense
    // enough" criterion (>= 8 usable points with a measurable single step).
    let mut probes: Vec<&[f64]> = Vec::new();
    for spec in sample_spectra {
        if let Some(s) = base_step(spec) {
            if first_step.is_none() {
                first_step = Some(s); // step estimated from one spectrum (pooling corrupts it)
            }
            probes.push(spec.as_slice());
        }
        pooled.extend(spec.iter().copied().filter(|&m| m > 0.0));
    }
    let base = first_step?;
    if pooled.len() < 16 {
        return None;
    }
    // Need corroboration across spectra: a single dense spectrum is insufficient evidence of a
    // per-run lattice (its refined-grid median_dk is unreliable). Reject if too few dense probes.
    if probes.len() < MIN_PROBES {
        return None;
    }
    let sqrt_mz: Vec<f64> = pooled.iter().map(|&m| m.sqrt()).collect();
    let s_min = sqrt_mz.iter().cloned().fold(f64::INFINITY, f64::min);

    // The natural lattice (compression-optimal) is the COARSEST grid in the scan — start there. We
    // also try a couple of coarser-than-detected steps in case the 30th-percentile estimate slightly
    // under-shot the true single step (which would give median_dk > 1).
    let mut best: Option<FitOutcome> = None;
    for mult in [4.0, 2.0, 1.0] {
        let step = base * mult;
        let (c0, c1) = fit_with_step(&sqrt_mz, s_min, step);
        if !(c1 > 0.0) || !c0.is_finite() {
            continue;
        }
        let grid = TofGrid { c0, c1 };
        let mdk = robust_median_dk(&grid, &probes);
        let (max_ppm, median_ppm, max_k) = evaluate(&grid, &pooled);
        // The natural lattice: dense adjacency (median dk == 1) AND lossless within instrument
        // accuracy AND fits Int32. This is the compression sweet spot.
        if mdk == 1 && max_ppm <= PPM_TOL && max_k <= MAX_K {
            return Some(FitOutcome { grid, max_ppm, median_ppm, max_k, median_dk: mdk });
        }
    }

    // No clean dk==1 lattice at the detected step. Fall back to refinement: accept the COARSEST grid
    // (best compression) that meets the ppm gate while remaining dense (median_dk <= MAX_MEDIAN_DK).
    // This still rejects non-TOF data — its dk explodes long before ppm is met.
    for shift in 0..14u32 {
        let step = base / (1u64 << shift) as f64;
        let (c0, c1) = fit_with_step(&sqrt_mz, s_min, step);
        if !(c1 > 0.0) || !c0.is_finite() {
            continue;
        }
        let grid = TofGrid { c0, c1 };
        let (max_ppm, median_ppm, max_k) = evaluate(&grid, &pooled);
        if max_k > MAX_K {
            break;
        }
        let mdk = robust_median_dk(&grid, &probes);
        if max_ppm <= PPM_TOL {
            if mdk <= MAX_MEDIAN_DK {
                best = Some(FitOutcome { grid, max_ppm, median_ppm, max_k, median_dk: mdk });
            }
            break;
        }
    }
    best
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

    /// A SINGLE dense spectrum is insufficient evidence of a per-run lattice (its refined-grid
    /// density can be coincidental — the single-FT-scan Orbitrap false-accept). Require >= MIN_PROBES.
    #[test]
    fn single_probe_rejected() {
        let truth = TofGrid { c0: 17.32, c1: 1.6e-5 };
        let mut spec = Vec::new();
        let mut k = 100_000i32;
        while k < 1_200_000 {
            spec.push(truth.mz(k));
            k += if k % 7 == 0 { 3 } else { 1 };
        }
        // One probe spectrum only → reject (even though it IS a perfect lattice). Two → accept.
        assert!(fit(&[spec.clone()]).is_none(), "single probe must be rejected");
        assert!(fit(&[spec.clone(), spec]).is_some(), "two probes of a real lattice must fit");
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
