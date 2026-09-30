//! Imaging metadata the imzML lane checks or restores instead of copying blindly
//! (HUPO-PSI/mzPeak-specification#23; handoff 2026-09-29):
//!
//! * **Pixel size.** A survey of 784 public imzML headers found pixel sizes without a unit, the
//!   centimetre accession labelled "micrometer", and `IMS:1000046` given as an AREA (its meaning
//!   until 2017). [`pixel_size_fixes`] applies the issue author's rule: x and y with a unit are kept;
//!   x and y without one are taken as micrometre; a single value is tested against pixel count and
//!   extent — an area when `√value × count = extent` (written as its square root), a length when
//!   `value × count = extent` — and anything else is dropped. Nothing changes silently: every
//!   action becomes a `transformations` entry and a row of the `imaging_pixel_size` index block.
//! * **"one way"** (`IMS:1000411`, obsolete) is written as its stated replacement, flyback
//!   (`IMS:1000413`), and declared.
//! * **File provenance.** mzdata consumes storage mode, UUID and `.ibd` checksum into its
//!   `ImzMLFileMetadata` and leaves them out of `file_description`; [`provenance_params`] puts them
//!   back from the header ([`read_file_content`]), values as stated.
//!
//! mzdata keeps a unit by its accession only, so the unit NAME the file states — needed to see an
//! accession/name disagreement — is read from the header here ([`read_scan_settings`]).
//!
//! **Which runs are imaging** is decided by [`detect`], the one detector every lane calls: imzML
//! input always; a Bruker `.d` with `MaldiFrameInfo` positions; any other input whose spectra state
//! `IMS:1000050/51`. A detected run gets the imaging profile's `metadata.imaging` marker
//! ([`marker_block`]) with its provenance; nothing else is marked imaging.

use std::collections::HashMap;
use std::io::BufRead;
use std::path::Path;

use anyhow::{Context, Result};
use mzdata::params::{Param, ParamDescribed, Unit};
use mzdata::meta::ScanSettings;
use quick_xml::events::Event;

pub const UNIT_ASSUMED: &str = "imzml:pixel-size-unit-assumed-um";
pub const AREA_TO_LENGTH: &str = "imzml:pixel-size-area-to-length";
pub const DROPPED: &str = "imzml:pixel-size-dropped";
pub const ONE_WAY_AS_FLYBACK: &str = "imzml:one-way-as-flyback";
/// The unit mzdata wrote differs from the unit ACCESSION the file states (mzdata takes whichever of
/// `unitAccession` / `unitName` comes last, so a disagreeing pair resolves by attribute order).
pub const UNIT_FROM_NAME: &str = "imzml:unit-accession-replaced-by-name";
/// The input states positions but no pixel counts: `IMS:1000042/43` are the largest positions.
pub const COUNT_FROM_POSITIONS: &str = "imaging:pixel-count-from-positions";

/// What made a run an imaging run.
pub enum Detected {
    /// imzML input: always imaging.
    ImzML,
    /// A Bruker `.d` whose `analysis.tsf`/`.tdf` has `MaldiFrameInfo` positions.
    BrukerMaldi(crate::bruker_maldi::MaldiInfo),
    /// Any other input whose sampled spectra state `IMS:1000050`/`51`: an mzML written from imaging
    /// data, e.g. this converter's own `--to mzml` export of an imaging archive.
    ScanPositions,
}

impl Detected {
    pub fn bruker(self) -> Option<crate::bruker_maldi::MaldiInfo> {
        match self {
            Detected::BrukerMaldi(m) => Some(m),
            _ => None,
        }
    }
}

/// The imaging detector for every lane that knows its input from the path and sampled spectra.
/// `probes` are the lane's sampled spectra (empty where a lane has none yet). A Waters imaging
/// `.raw` is detected by its reader instead, which needs MassLynx open (`WatersReader::imaging`).
pub fn detect(input: &Path, is_imzml: bool, probes: &[mzdata::spectrum::MultiLayerSpectrum]) -> Option<Detected> {
    if is_imzml {
        return Some(Detected::ImzML);
    }
    if input.is_dir() {
        if let Some(m) = crate::bruker_maldi::read_dot_d(input) {
            return Some(Detected::BrukerMaldi(m));
        }
    }
    probes.iter().any(|s| position_of(&s.description).is_some()).then_some(Detected::ScanPositions)
}

/// The `(x, y)` position a spectrum's scans state, if any.
pub fn position_of(d: &mzdata::spectrum::SpectrumDescription) -> Option<(i64, i64)> {
    d.acquisition.scans.iter().find_map(|sc| {
        let v = |c| sc.get_param_by_curie(&c)?.value.to_i64().ok();
        Some((v(mzdata::curie!(IMS:1000050))?, v(mzdata::curie!(IMS:1000051))?))
    })
}

/// The grid entry of a scan settings list: the one stating the pixel counts.
pub fn grid(list: &[ScanSettings]) -> Option<&ScanSettings> {
    list.iter().find(|s| s.params.iter().any(|p| p.curie() == Some(mzdata::curie!(IMS:1000042))))
}

/// The largest position written — the pixel counts of an input that states positions but no grid.
#[derive(Debug, Default)]
pub struct Extent(pub i64, pub i64);

impl Extent {
    pub fn observe(&mut self, d: &mzdata::spectrum::SpectrumDescription) {
        if let Some((x, y)) = position_of(d) {
            (self.0, self.1) = (self.0.max(x), self.1.max(y));
        }
    }

    pub fn settings(&self) -> ScanSettings {
        let mut s = ScanSettings { id: "scansettings1".into(), ..Default::default() };
        s.params.push(Param::builder().name("max count of pixels x").curie(mzdata::curie!(IMS:1000042)).value(self.0).build());
        s.params.push(Param::builder().name("max count of pixels y").curie(mzdata::curie!(IMS:1000043)).value(self.1).build());
        s
    }
}

/// One axis of a pixel grid fitted to stage positions in mm (Waters states laser positions, not pixel
/// indices): the position of pixel 1, the step (`None` for a single column without a declared
/// step), the pixel count, the farthest any on-grid position lies from its grid point, and whether
/// the step is the one the acquisition declared.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GridAxis {
    pub origin: f64,
    pub pitch: Option<f64>,
    pub count: i64,
    pub max_residual: f64,
    pub declared: bool,
}

/// A fitted axis and each position's 1-based pixel index (`None`: off the grid).
type AxisFit = (GridAxis, Vec<Option<i64>>);

/// The share of positions a grid may leave off it: stray scans (a scan taken with the stage parked
/// far off the raster) must not take the grid away from the whole run (review 2026-09-30 B15).
pub const MAX_OFF_GRID: f64 = 0.01;

/// Positions closer than this (mm) are one position: float32 noise must not pose as a step.
const SAME_POSITION_MM: f64 = 1e-3;

/// How far from its grid point a position may lie, in steps.
const QUARTER: f64 = 0.25;

/// The same on a multiple of the fitted step, which folds two of its columns into one pixel (the
/// two halves of a serpentine lag): a twentieth less, so a raster split exactly in half — every
/// other column a quarter of the coarser step either side of its grid point — is never folded.
const FOLD: f64 = 0.95 * QUARTER;

/// Fit a grid axis to positions (mm): the axis and each position's 1-based pixel index, `None` for
/// the at most [`MAX_OFF_GRID`] positions that lie off it. `None` when no grid holds the rest within
/// a quarter step — the positions are not a raster — or there are none.
///
/// The raster is the central 90 % of the positions and every position reached from it by gaps at
/// most four times the largest gap inside it, and past a longer gap every group of positions that
/// spans columns (a second section, a QC region). A group at one position is scans parked off the
/// raster (at the stage's home): they get no pixel even when they lie on the grid by chance, where
/// they would stretch it by hundreds of pixels (review 2026-09-30 B15) — unless they are more than
/// [`MAX_OFF_GRID`] of the positions, and so a part of the raster.
///
/// A `declared` step (the acquisition's own, e.g. the DESI method's `DesiXStep`) is the pitch when
/// it holds the positions. Otherwise the step is fitted (review 2026-09-30 B14; the most common gap
/// it replaced lost exact sparse rasters, took µm jitter or a serpentine lag for the step) by two
/// passes, each trying the gaps between its columns, or between a column and the next but one (a
/// larger lag splits each column in two), smallest first, for a grid ([`grid_at`]) that holds the
/// positions, then the coarsest multiple of it that still does ([`coarsest`]). One takes positions
/// closer than 30 % of the typical gap as one column — jitter, a small lag. The other takes each
/// distinct position as a column and holds it within 1 µm, as stage set points (float32) are: it
/// wins where the first folds its columns other than as strays or a lag do ([`folds_as_lag`]: three
/// or more in a pixel, or pairs in some pixels only or off one coarser grid), so small regions
/// (spots, tissue-microarray cores) keep their step however many long gaps lie between them. A fully
/// regularly half-sampled raster is indistinguishable from a coarser grid and fits as one.
///
/// ponytail: a step under 3 µm is taken for the recording lattice (jitter recorded at µm
/// resolution), and jitter recorded more coarsely than that for a raster at that resolution; a
/// single column whose positions spread over 5 µm fits no grid unless a step is declared; jittered
/// positions whose gaps are more than a tenth over three steps merge one-step neighbours; a lag of
/// 0.45 step or more is not told from a grid of half the step; columns that come only in pairs, the
/// pairs on one coarser grid (an array of spots two pixels wide; any two such regions), fold like a
/// lag's two halves.
pub fn fit_axis(values: &[f64], declared: Option<f64>) -> Option<AxisFit> {
    if values.is_empty() || !values.iter().all(|v| v.is_finite()) {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let trim = values.len() / 20;
    let mut core = &sorted[trim..sorted.len() - trim];
    // The raster: the core, and past it every position within four of its largest gaps of the last;
    // beyond, the groups that span columns — a group at one position is parked.
    let reach = 4.0 * core.windows(2).map(|w| w[1] - w[0]).fold(SAME_POSITION_MM, f64::max);
    let (mut a, mut b) = (trim, sorted.len() - 1 - trim);
    while a > 0 && sorted[a] - sorted[a - 1] <= reach {
        a -= 1;
    }
    while b + 1 < sorted.len() && sorted[b + 1] - sorted[b] <= reach {
        b += 1;
    }
    let near = |x: &f64, y: &f64| y - x <= reach;
    let one = |g: &&[f64]| g[g.len() - 1] - g[0] < SAME_POSITION_MM;
    let parked: usize = sorted[..a].chunk_by(near).chain(sorted[b + 1..].chunk_by(near)).filter(one).map(<[f64]>::len).sum();
    let raster = if parked as f64 <= MAX_OFF_GRID * values.len() as f64 {
        let regions = |s: &[f64]| -> Vec<f64> { s.chunk_by(near).filter(|g| !one(g)).flatten().copied().collect() };
        [regions(&sorted[..a]), sorted[a..=b].to_vec(), regions(&sorted[b + 1..])].concat()
    } else {
        sorted.clone()
    };
    let raster = &raster[..];
    if let Some(fit) = declared.and_then(|d| grid_at(values, raster, d, true, QUARTER)) {
        return Some((GridAxis { declared: true, ..fit.0 }, fit.1));
    }
    let gaps_of = |s: &[f64]| -> Vec<f64> { s.windows(2).map(|w| w[1] - w[0]).filter(|g| *g >= SAME_POSITION_MM).collect() };
    // One column: the positions within 1 µm of the core's, the rest (too many: `None`) off it. Gaps
    // under 1 µm chain a continuum of positions (random ones over a millimetre) into one: a core
    // wider than 5 µm is no column.
    let single = |core: &[f64]| {
        if core[core.len() - 1] - core[0] > 5.0 * SAME_POSITION_MM {
            return None;
        }
        let on = |v: &f64| (core[0] - SAME_POSITION_MM..=core[core.len() - 1] + SAME_POSITION_MM).contains(v);
        let n_on = values.iter().filter(|v| on(v)).count();
        if (values.len() - n_on) as f64 > MAX_OFF_GRID * values.len() as f64 {
            return None;
        }
        let origin = values.iter().filter(|v| on(v)).sum::<f64>() / n_on as f64;
        let max_residual = values.iter().filter(|v| on(v)).map(|v| (v - origin).abs()).fold(0.0, f64::max);
        let index = values.iter().map(|v| on(v).then_some(1)).collect();
        Some((GridAxis { origin, pitch: None, count: 1, max_residual, declared: false }, index))
    };
    let mut gaps = gaps_of(core);
    if gaps.is_empty() {
        if let Some(fit) = single(core) {
            return Some(fit);
        }
        // More than a few positions outside: they are columns of their own (a short second row).
        core = &sorted;
        gaps = gaps_of(core);
        if gaps.is_empty() {
            return single(core);
        }
    }
    gaps.sort_by(f64::total_cmp);
    // The typical gap: the 90th percentile of the gaps of at least r, r doubling from 1 µm while the
    // typical gap is under 3 r (the recording lattice: jitter recorded at a µm resolution splits a
    // column into so many distinct positions that their gaps would be the typical ones, review
    // 2026-09-30 B14), or under 10 r and twice the median gap (unrecorded jitter). A typical gap of
    // 3 r or more that most gaps share is the step: a 5 µm raster's.
    let pct = |s: &[f64], q: usize| s[(s.len() - 1) * q / 10];
    let (mut r, mut typical, mut median) = (SAME_POSITION_MM, pct(&gaps, 9), pct(&gaps, 5));
    while typical < 3.0 * r || (typical < 10.0 * r && median < 0.5 * typical) {
        let above = &gaps[gaps.partition_point(|g| *g < 2.0 * r)..];
        if above.is_empty() {
            break;
        }
        (r, typical, median) = (2.0 * r, pct(above, 9), pct(above, 5));
    }
    // One pass: the columns are runs of positions whose neighbours are closer than `merge`, averaged,
    // and the first step whose grid holds them — within a quarter step, or within 1 µm (`exact`): the
    // fit, and each position's index on that grid before [`coarsest`].
    let pass = |merge: f64, exact: bool| {
        let mut centres: Vec<f64> = Vec::new();
        let mut run = (core[0], 0.0, 0usize);
        for &v in core {
            if v - run.0 >= merge {
                centres.push(run.1 / run.2 as f64);
                run = (v, 0.0, 0);
            }
            run = (v, run.1 + v, run.2 + 1);
        }
        centres.push(run.1 / run.2 as f64);
        let mut steps: Vec<f64> = centres.windows(2).map(|w| w[1] - w[0]).chain(centres.windows(3).map(|w| w[2] - w[0])).collect();
        steps.sort_by(f64::total_cmp);
        // A step from one gap is refined by all: the median of the gaps between centres 1, 4, 16 and
        // 64 apart, each per step it spans (those within a fifth of a whole number of steps). The
        // median ignores the strays' gaps; the longer spans pin the pitch down so that it does not
        // drift a quarter step across a wide raster.
        let refine = |mut c: f64| {
            for apart in [1, 4, 16, 64] {
                let mut per: Vec<f64> = centres
                    .iter()
                    .zip(&centres[apart.min(centres.len())..])
                    .filter_map(|(a, b)| {
                        let n = ((b - a) / c).round();
                        (n >= 1.0 && ((b - a) / c - n).abs() <= 0.2).then(|| (b - a) / n)
                    })
                    .collect();
                if per.is_empty() {
                    break;
                }
                per.sort_by(f64::total_cmp);
                c = per[per.len() / 2];
            }
            c
        };
        let mut tried = 0.0;
        // A step over twice the typical gap folds neighbouring columns into one pixel (two tissue
        // regions into two pixels) whenever the finer steps failed.
        for c in steps.into_iter().take_while(|c| *c <= 2.0 * typical) {
            // Steps within an eighth of one already tried fit the same grid (the pitch is refined);
            // steps under 3 µm are the recording lattice.
            if c < tried * 1.125 || c < 3.0 * SAME_POSITION_MM {
                continue;
            }
            tried = c;
            let band = if exact { (SAME_POSITION_MM / c).min(QUARTER) } else { QUARTER };
            if let Some(fine) = grid_at(values, raster, refine(c), false, band) {
                let fit = coarsest(fine.clone(), values, raster);
                let Some(pitch) = fit.0.pitch else { return Some((fit, fine.1)) };
                // A stage step is set in whole µm or 0.1 µm: snap the fitted pitch to the roundest
                // such value within three standard errors of it (at least 1e-7 of it, a float32 step
                // multiplied up). Float32 noise would otherwise write 99.99995 µm; a 33.33 µm step
                // stays. Every position within a quarter of the step is then placed, when that grid
                // holds as many.
                let se = standard_error(&fit, values).max(1e-7 * pitch);
                let snapped = [1e3, 1e4].into_iter().map(|u| (pitch * u).round() / u).find(|s| (s - pitch).abs() <= 3.0 * se);
                let off = |f: &AxisFit| f.1.iter().filter(|i| i.is_none()).count();
                let fit = match grid_at(values, raster, snapped.unwrap_or(pitch), true, QUARTER) {
                    Some(s) if off(&s) <= off(&fit) => s,
                    _ => fit,
                };
                return Some((fit, fine.1));
            }
        }
        None
    };
    // Positions closer than 30 % of the typical gap are one column (jitter, a small serpentine lag).
    // Where that folds the distinct positions other than as strays or a lag do — small regions
    // (spots, tissue-microarray cores) with more than a tenth of the gaps between them — the stage
    // set points are exact (float32), and the grid holding each within 1 µm is the step (review
    // 2026-09-30 B14). The fold is judged on that grid before it is coarsened: coarsened, it may
    // have folded pairs of columns already, and the merged grid four columns into a pixel.
    let merged = pass(0.3 * typical, false);
    let exact = pass(SAME_POSITION_MM, true);
    match (merged, exact) {
        (Some(m), Some(e)) if !folds_as_lag(&m.0.1, &e.1, values.len()) => Some(e.0),
        (m, e) => m.or(e).map(|f| f.0),
    }
}

/// The least-squares line `v = origin + pitch · k` through `(k, v)`: `(pitch, origin)`, the pitch
/// `fixed` when given. `None` without points or a positive pitch.
fn line(points: &[(f64, f64)], fixed: Option<f64>) -> Option<(f64, f64)> {
    if points.is_empty() {
        return None;
    }
    let n = points.len() as f64;
    let (mk, mv) = points.iter().fold((0.0, 0.0), |(a, b), (k, v)| (a + k / n, b + v / n));
    let (cov, var) = points.iter().fold((0.0, 0.0), |(c, s), (k, v)| (c + (k - mk) * (v - mv), s + (k - mk) * (k - mk)));
    let pitch = fixed.unwrap_or(cov / var);
    (pitch > 0.0).then_some((pitch, mv - pitch * mk))
}

/// The standard error of a fitted pitch: the positions' scatter about the grid — at least a float32
/// step at their magnitude, the precision MassLynx stores them in — over the spread of their
/// indices.
fn standard_error((axis, index): &AxisFit, values: &[f64]) -> f64 {
    let pitch = axis.pitch.unwrap_or(0.0);
    let on: Vec<(f64, f64)> =
        index.iter().zip(values).filter_map(|(i, v)| Some(((*i)? as f64, v - axis.origin - ((*i)? - 1) as f64 * pitch))).collect();
    let n = on.len() as f64;
    let mk = on.iter().map(|(k, _)| k).sum::<f64>() / n;
    let spread = on.iter().map(|(k, _)| (k - mk) * (k - mk)).sum::<f64>();
    let float32 = f32::EPSILON as f64 * values.iter().fold(0.0f64, |m, v| m.max(v.abs()));
    let scatter = (on.iter().map(|(_, r)| r * r).sum::<f64>() / (n - 2.0).max(1.0)).max(float32 * float32);
    (scatter / spread).sqrt()
}

/// The grid of step `c` (`fixed`: exactly `c`) through the `raster` positions, placing all the
/// `values`. Its phase is the circular mean of the raster positions modulo `c`, and the
/// least-squares line through the positions it holds then refines pitch and origin (three rounds,
/// the held positions growing as the pitch improves). A stray moves the phase by its share only —
/// the walk over the gaps it replaced slipped a column wherever two strays split a gap, and lost
/// the grid to 0.2 % of strays inside the raster (review 2026-09-30 B14/B15). `None` unless all but
/// [`MAX_OFF_GRID`] of the `values` lie within `band` steps of it; one outside the raster is off it.
fn grid_at(values: &[f64], raster: &[f64], c: f64, fixed: bool, band: f64) -> Option<AxisFit> {
    use std::f64::consts::TAU;
    if !(c > 0.0) {
        return None;
    }
    let (sin, cos) = raster.iter().fold((0.0, 0.0), |(s, co), v| {
        let a = TAU * (v / c).rem_euclid(1.0);
        (s + a.sin(), co + a.cos())
    });
    let (mut pitch, mut origin) = (c, c * sin.atan2(cos) / TAU);
    let place = |v: f64, pitch: f64, origin: f64| {
        let k = ((v - origin) / pitch).round();
        (k, (v - origin - k * pitch).abs())
    };
    for _ in 0..3 {
        let held: Vec<(f64, f64)> =
            raster.iter().map(|&v| (place(v, pitch, origin), v)).filter(|((_, r), _)| *r <= band * pitch).map(|((k, _), v)| (k, v)).collect();
        (pitch, origin) = line(&held, fixed.then_some(c))?;
    }
    let inside = raster[0]..=raster[raster.len() - 1];
    let placed: Vec<Option<i64>> = values
        .iter()
        .map(|&v| Some(place(v, pitch, origin)).filter(|(_, r)| *r <= band * pitch && inside.contains(&v)).map(|(k, _)| k as i64))
        .collect();
    if placed.iter().filter(|k| k.is_none()).count() as f64 > MAX_OFF_GRID * values.len() as f64 {
        return None;
    }
    let (lo, hi) = placed.iter().flatten().fold((i64::MAX, i64::MIN), |(lo, hi), k| (lo.min(*k), hi.max(*k)));
    let max_residual = placed.iter().zip(values).filter_map(|(k, v)| Some((v - origin - (*k)? as f64 * pitch).abs())).fold(0.0, f64::max);
    let axis = GridAxis { origin: origin + lo as f64 * pitch, pitch: Some(pitch), count: hi - lo + 1, max_residual, declared: false };
    Some((axis, placed.iter().map(|k| k.map(|k| k - lo + 1)).collect()))
}

/// The coarsest multiple of a fitted grid's step, up to 16, whose grid still holds the positions
/// within [`FOLD`] of its step, puts two of them in neighbouring pixels, and folds the finer columns
/// as strays or a serpentine lag do ([`folds_as_lag`]): a stray between two columns refines the grid
/// to a fraction of the step, and so can the two halves of a lag — the coarser grid folds them back
/// (review 2026-09-30 B14). A multiple that leaves no two pixels in use side by side has no step
/// the positions show (four columns in one pixel).
///
/// ponytail: multiples up to 16 — a stray under 30 % of a step from a column merges into it, so a
/// finer grid a stray makes is a half, a third or a quarter of the step.
fn coarsest(fit: AxisFit, values: &[f64], raster: &[f64]) -> AxisFit {
    let Some(p) = fit.0.pitch else { return fit };
    let neighbours = |(_, index): &AxisFit| {
        let mut k: Vec<i64> = index.iter().flatten().copied().collect();
        k.sort_unstable();
        k.dedup();
        k.windows(2).any(|w| w[1] == w[0] + 1)
    };
    let as_lag = |f: &AxisFit| folds_as_lag(&f.1, &fit.1, values.len());
    (2..=16).rev().find_map(|m| grid_at(values, raster, m as f64 * p, true, FOLD).filter(|f| neighbours(f) && as_lag(f))).unwrap_or(fit)
}

/// Whether a grid (`index`) puts the columns of a finer one (`fine`: each position's index on it)
/// into its pixels as strays or a serpentine lag do: strays put at most [`MAX_OFF_GRID`] of the `n`
/// positions beside a pixel's fullest column; a lag splits the pixels of all but a tenth of the
/// positions on both grids into two columns, the second holding at least a third of them, at one
/// offset: the same number of finer steps apart in every pixel, the first columns on one grid of
/// whole finer steps per pixel. A third column in a pixel is neither — whole regions folded into one
/// (a spot array, tissue-microarray cores; review 2026-09-30 B14) — nor are pairs in some pixels
/// only, at different distances or off that grid (regions one and two pixels wide, at any gaps).
fn folds_as_lag(index: &[Option<i64>], fine: &[Option<i64>], n: usize) -> bool {
    let mut pairs: Vec<(i64, i64)> = index.iter().zip(fine).filter_map(|(k, f)| Some(((*k)?, (*f)?))).collect();
    pairs.sort_unstable();
    // Each split pixel: its index, its first column's finer index, the distance to its second.
    let (mut second, mut beyond, mut unsplit, mut split) = (0, 0, 0, Vec::new());
    for pixel in pairs.chunk_by(|a, b| a.0 == b.0) {
        let mut k: Vec<(usize, i64)> = pixel.chunk_by(|a, b| a == b).map(|c| (c.len(), c[0].1)).collect();
        k.sort_unstable_by(|a, b| b.0.cmp(&a.0));
        if let [(_, f0), (two, f1), ..] = k[..] {
            second += two;
            split.push((pixel[0].0, f0.min(f1), f0.abs_diff(f1)));
        } else {
            unsplit += k[0].0;
        }
        beyond += k.iter().skip(2).map(|c| c.0).sum::<usize>();
    }
    let few = |k: usize| k as f64 <= MAX_OFF_GRID * n as f64;
    let lag = match (split.first(), split.last()) {
        (Some(&(k0, a0, d0)), Some(&(k1, a1, _))) => {
            let m = if k1 > k0 { (a1 - a0) / (k1 - k0) } else { 0 };
            3 * second >= pairs.len() && 10 * unsplit <= pairs.len() && split.iter().all(|&(k, a, d)| d == d0 && a - a0 == m * (k - k0))
        }
        _ => false,
    };
    few(beyond) && (few(second) || lag)
}

/// The imaging profile's `metadata.imaging` index block (HUPO-PSI/mzPeak-specification#24): the
/// marker, the coordinate base, the grid as the viewer reads it, and where it all came from.
pub fn marker_block(grid: Option<&ScanSettings>, provenance: serde_json::Value) -> serde_json::Value {
    let mut b = serde_json::json!({"is_imaging": true, "coordinate_base": 1, "provenance": provenance});
    let param = |acc| grid?.params.iter().find(|p| p.curie() == Some(acc));
    let int = |acc| param(acc)?.value.to_i64().ok();
    let um = |acc| param(acc).filter(|p| p.unit == Unit::Micrometer)?.value.to_f64().ok();
    if let (Some(x), Some(y)) = (int(mzdata::curie!(IMS:1000042)), int(mzdata::curie!(IMS:1000043))) {
        b["pixel_count"] = serde_json::json!({"x": x, "y": y});
    }
    // ponytail: micrometre only; a pixel size in another length unit stays in the scan settings.
    if let (Some(x), Some(y)) = (um(mzdata::curie!(IMS:1000046)), um(mzdata::curie!(IMS:1000047))) {
        b["pixel_size_um"] = serde_json::json!({"x": x, "y": y});
    }
    b
}

/// One `cvParam` of a `<scanSettings>`, as the file states it.
#[derive(Debug, Clone, PartialEq)]
pub struct RawParam {
    pub accession: String,
    pub value: String,
    pub unit_accession: Option<String>,
    pub unit_name: Option<String>,
}

/// A `<scanSettings>` element: its id and cvParams, referenced param groups expanded.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RawSettings {
    pub id: String,
    pub params: Vec<RawParam>,
}

pub(crate) fn attr(e: &quick_xml::events::BytesStart, key: &[u8]) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.as_ref() == key)
        .map(|a| String::from_utf8_lossy(&a.value).into_owned())
        .filter(|v| !v.is_empty())
}

fn raw_param(e: &quick_xml::events::BytesStart) -> Option<RawParam> {
    Some(RawParam {
        accession: attr(e, b"accession")?,
        value: attr(e, b"value").unwrap_or_default(),
        unit_accession: attr(e, b"unitAccession"),
        unit_name: attr(e, b"unitName"),
    })
}

/// The `<scanSettings>` of an imzML header. Reading stops at `<run>`; spectra are never touched.
pub fn read_scan_settings(path: &Path) -> Result<Vec<RawSettings>> {
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    read_scan_settings_from(std::io::BufReader::new(file))
}

pub fn read_scan_settings_from(input: impl BufRead) -> Result<Vec<RawSettings>> {
    let mut reader = quick_xml::Reader::from_reader(input);
    let mut buf = Vec::new();
    let mut groups: HashMap<String, Vec<RawParam>> = HashMap::new();
    let mut group: Option<(String, Vec<RawParam>)> = None;
    let mut settings: Vec<RawSettings> = Vec::new();
    let mut current: Option<RawSettings> = None;
    loop {
        let ev = reader.read_event_into(&mut buf).context("parsing the imzML header")?;
        match &ev {
            Event::Start(e) | Event::Empty(e) => {
                let empty = matches!(ev, Event::Empty(_));
                match e.local_name().as_ref() {
                    b"run" => break,
                    b"referenceableParamGroup" if !empty => {
                        group = attr(e, b"id").map(|id| (id, Vec::new()));
                    }
                    b"scanSettings" => {
                        let s = RawSettings { id: attr(e, b"id").unwrap_or_default(), params: Vec::new() };
                        if empty { settings.push(s) } else { current = Some(s) }
                    }
                    b"cvParam" => {
                        if let Some(p) = raw_param(e) {
                            if let Some(s) = current.as_mut() {
                                s.params.push(p);
                            } else if let Some((_, g)) = group.as_mut() {
                                g.push(p);
                            }
                        }
                    }
                    b"referenceableParamGroupRef" => {
                        if let (Some(s), Some(r)) = (current.as_mut(), attr(e, b"ref")) {
                            s.params.extend(groups.get(&r).cloned().unwrap_or_default());
                        }
                    }
                    _ => {}
                }
            }
            Event::End(e) => match e.local_name().as_ref() {
                b"referenceableParamGroup" => {
                    if let Some((id, g)) = group.take() {
                        groups.insert(id, g);
                    }
                }
                b"scanSettings" => settings.extend(current.take()),
                b"scanSettingsList" => break,
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(settings)
}

/// The canonical name of a length unit accession, for the accession/name check.
fn length_unit_name(accession: &str) -> Option<&'static str> {
    Some(match accession {
        "UO:0000008" => "meter",
        "UO:0000015" => "centimeter",
        "UO:0000016" => "millimeter",
        "UO:0000017" => "micrometer",
        "UO:0000018" => "nanometer",
        _ => return None,
    })
}

fn normalized_unit_name(name: &str) -> String {
    name.trim().to_lowercase().replace("metre", "meter").replace("µm", "micrometer").replace("μm", "micrometer")
}

/// What the pixel-size rule did to one `<scanSettings>`.
#[derive(Debug, Clone, PartialEq)]
pub struct PixelSizeFix {
    pub settings_id: String,
    /// Which case of the rule applied.
    pub case: &'static str,
    /// The `transformations` entry it declares, when the values changed.
    pub transformation: Option<&'static str>,
    /// Pixel sizes to write: accession (`IMS:1000046`/`47`), value, and whether its unit is set to
    /// micrometre. An accession absent here is removed.
    pub write: Vec<(&'static str, f64, bool)>,
    /// Unit accession/name disagreements seen on the pixel-size and extent params.
    pub unit_mismatches: Vec<String>,
    /// `(param accession, stated unit accession)` of those params, to compare with what was written.
    pub mismatched: Vec<(String, String)>,
    /// What [`check_written_units`] found: the unit each mismatched param was written with.
    pub written_units: Vec<String>,
    pub detail: String,
}

const PIXEL_X: &str = "IMS:1000046";
const PIXEL_Y: &str = "IMS:1000047";

fn approx(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-6 * a.abs().max(b.abs()).max(1.0)
}

/// The rule for one `<scanSettings>`; `None` when it states no pixel size and no mismatch.
pub fn pixel_size_fix(s: &RawSettings) -> Option<PixelSizeFix> {
    let get = |acc: &str| s.params.iter().find(|p| p.accession == acc);
    let num = |acc: &str| get(acc).and_then(|p| p.value.trim().parse::<f64>().ok()).filter(|v| v.is_finite());
    let unit_mismatches: Vec<String> = ["IMS:1000044", "IMS:1000045", PIXEL_X, PIXEL_Y]
        .iter()
        .filter_map(|acc| get(acc))
        .filter_map(|p| {
            let canonical = length_unit_name(p.unit_accession.as_deref()?)?;
            let stated = p.unit_name.as_deref()?;
            (normalized_unit_name(stated) != canonical).then(|| {
                format!("{}: unit {} is {canonical} but named {stated:?}", p.accession, p.unit_accession.as_deref().unwrap_or(""))
            })
        })
        .collect();
    let mismatched: Vec<(String, String)> = ["IMS:1000044", "IMS:1000045", PIXEL_X, PIXEL_Y]
        .iter()
        .filter_map(|acc| get(acc))
        .filter_map(|p| {
            let ua = p.unit_accession.as_deref()?;
            let canonical = length_unit_name(ua)?;
            (normalized_unit_name(p.unit_name.as_deref()?) != canonical).then(|| (p.accession.clone(), ua.to_string()))
        })
        .collect();
    let (x, y) = (get(PIXEL_X), get(PIXEL_Y));
    let fix = |case, transformation, write, detail: String| PixelSizeFix {
        settings_id: s.id.clone(),
        case,
        transformation,
        write,
        unit_mismatches: unit_mismatches.clone(),
        mismatched: mismatched.clone(),
        written_units: Vec::new(),
        detail,
    };
    match (x, y) {
        (None, None) => (!unit_mismatches.is_empty())
            .then(|| fix("no pixel size stated", None, vec![], String::new())),
        (Some(px), Some(py)) => {
            let (Some(vx), Some(vy)) = (num(PIXEL_X), num(PIXEL_Y)) else {
                return Some(fix("x and y not numeric", Some(DROPPED), vec![], format!("x={:?} y={:?}", px.value, py.value)));
            };
            if px.unit_accession.is_some() && py.unit_accession.is_some() {
                Some(fix("x and y with a unit", None, vec![(PIXEL_X, vx, false), (PIXEL_Y, vy, false)], String::new()))
                    .filter(|f| !f.unit_mismatches.is_empty())
            } else {
                Some(fix(
                    "x and y without a unit: micrometre assumed",
                    Some(UNIT_ASSUMED),
                    vec![(PIXEL_X, vx, px.unit_accession.is_none()), (PIXEL_Y, vy, py.unit_accession.is_none())],
                    format!("x={vx} y={vy}"),
                ))
            }
        }
        (Some(p), None) | (None, Some(p)) => {
            let acc: &'static str = if p.accession == PIXEL_X { PIXEL_X } else { PIXEL_Y };
            let Some(v) = num(acc).filter(|v| *v > 0.0) else {
                return Some(fix("one value, not numeric", Some(DROPPED), vec![], format!("{acc}={:?}", p.value)));
            };
            // Test against the same axis first, then the other: pixels are square in every
            // surveyed file that states both.
            let axes = [("IMS:1000042", "IMS:1000044"), ("IMS:1000043", "IMS:1000045")];
            let tested: Vec<(f64, f64)> =
                axes.iter().filter_map(|(c, e)| Some((num(c)?, num(e)?))).filter(|(c, e)| *c > 0.0 && *e > 0.0).collect();
            if let Some((count, extent)) = tested.iter().find(|(c, e)| approx(v.sqrt() * c, *e)) {
                Some(fix(
                    "one value: an area (√value × count = extent)",
                    Some(AREA_TO_LENGTH),
                    // The square root of an area is a length: micrometre, whatever unit the area had.
                    vec![(acc, v.sqrt(), true)],
                    format!("{acc}={v} as area; √{v} × {count} = {extent}"),
                ))
            } else if let Some((count, extent)) = tested.iter().find(|(c, e)| approx(v * c, *e)) {
                let unit_assumed = p.unit_accession.is_none();
                Some(fix(
                    "one value: a length (value × count = extent)",
                    unit_assumed.then_some(UNIT_ASSUMED),
                    vec![(acc, v, unit_assumed)],
                    format!("{acc}={v}; {v} × {count} = {extent}"),
                ))
            } else {
                Some(fix(
                    "one value that tests as neither area nor length",
                    Some(DROPPED),
                    vec![],
                    format!("{acc}={v}; count/extent {tested:?}"),
                ))
            }
        }
    }
}

/// All fixes for a header.
pub fn pixel_size_fixes(settings: &[RawSettings]) -> Vec<PixelSizeFix> {
    settings.iter().filter_map(pixel_size_fix).collect()
}

/// Apply a fix to the writer's copy of the same `<scanSettings>`: the pixel-size params become
/// exactly `fix.write` (micrometre where the unit was assumed).
pub fn apply(fix: &PixelSizeFix, settings: &mut ScanSettings) {
    if fix.transformation.is_none() {
        return;
    }
    for acc in [PIXEL_X, PIXEL_Y] {
        let wanted = fix.write.iter().find(|(a, _, _)| *a == acc);
        let pos = settings.params.iter().position(|p| p.curie().is_some_and(|c| c.to_string() == acc));
        match (pos, wanted) {
            (Some(i), None) => {
                settings.params.remove(i);
            }
            (Some(i), Some((_, v, assume_um))) => {
                let p = &mut settings.params[i];
                p.value = (*v).into();
                if *assume_um {
                    p.unit = Unit::Micrometer;
                }
            }
            _ => {}
        }
    }
}

/// After [`apply`]: the unit each mismatched param was actually written with. `true` when one of
/// them differs from the accession the file states — mzdata resolved the pair by the name.
pub fn check_written_units(fix: &mut PixelSizeFix, settings: &ScanSettings) -> bool {
    let mut replaced = false;
    for (acc, stated) in &fix.mismatched {
        let Some(p) = settings.params.iter().find(|p| p.curie().is_some_and(|c| c.to_string() == *acc)) else { continue };
        let written = p.unit.to_curie().map(|c| c.to_string()).unwrap_or_else(|| "none".into());
        replaced |= written != *stated;
        fix.written_units.push(format!("{acc}: stated {stated}, written {written}"));
    }
    replaced
}

/// Replace the obsolete scan term "one way" (`IMS:1000411`) by flyback (`IMS:1000413`), its stated
/// replacement with the same definition. `true` when one was replaced.
pub fn one_way_to_flyback(settings: &mut ScanSettings) -> bool {
    let mut replaced = false;
    for p in settings.params.iter_mut() {
        if p.curie().is_some_and(|c| c.to_string() == "IMS:1000411") {
            *p = Param::builder().name("flyback").curie(mzdata::curie!(IMS:1000413)).build();
            replaced = true;
        }
    }
    replaced
}

/// The `imaging_pixel_size` index block row for a fix.
pub fn fix_json(f: &PixelSizeFix) -> serde_json::Value {
    serde_json::json!({
        "scan_settings": f.settings_id,
        "case": f.case,
        "transformation": f.transformation,
        "written_um": f.write.iter().map(|(a, v, assumed)| serde_json::json!({"accession": a, "value": v, "unit_assumed": assumed})).collect::<Vec<_>>(),
        "unit_mismatches": f.unit_mismatches,
        "written_units": f.written_units,
        "detail": f.detail,
    })
}

/// The imzML provenance terms: storage mode, UUID, `.ibd` checksum.
const PROVENANCE: [(mzdata::params::CURIE, &str); 6] = [
    (mzdata::curie!(IMS:1000030), "continuous"),
    (mzdata::curie!(IMS:1000031), "processed"),
    (mzdata::curie!(IMS:1000080), "universally unique identifier"),
    (mzdata::curie!(IMS:1000090), "ibd MD5"),
    (mzdata::curie!(IMS:1000091), "ibd SHA-1"),
    (mzdata::curie!(IMS:1000092), "ibd SHA-256"),
];

/// `file_description.contents` params for the imzML provenance mzdata consumed, with the values
/// exactly as the header states them (mzdata parses the UUID, and writing its parse back re-spelled
/// `686ec248…` as `{686EC248-…}`: fidelity L0 keeps the identifier as stated).
pub fn provenance_params(content: &[RawParam]) -> Vec<Param> {
    PROVENANCE
        .iter()
        .filter_map(|(curie, name)| {
            let p = content.iter().find(|p| p.accession == curie.to_string())?;
            let b = Param::builder().name(*name).curie(*curie);
            Some(if p.value.is_empty() { b.build() } else { b.value(p.value.clone()).build() })
        })
        .collect()
}

/// The `<fileContent>` cvParams of an imzML header, values as stated, param-group references
/// expanded (the groups are declared after `<fileDescription>`, so they resolve at the end).
pub fn read_file_content(path: &Path) -> Result<Vec<RawParam>> {
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    read_file_content_from(std::io::BufReader::new(file))
}

pub fn read_file_content_from(input: impl BufRead) -> Result<Vec<RawParam>> {
    let mut reader = quick_xml::Reader::from_reader(input);
    let mut buf = Vec::new();
    let mut groups: HashMap<String, Vec<RawParam>> = HashMap::new();
    let mut group: Option<(String, Vec<RawParam>)> = None;
    let mut in_content = false;
    // A param, or the id of a referenced group.
    let mut content: Vec<Result<RawParam, String>> = Vec::new();
    loop {
        let ev = reader.read_event_into(&mut buf).context("parsing the imzML header")?;
        match &ev {
            Event::Start(e) | Event::Empty(e) => {
                let empty = matches!(ev, Event::Empty(_));
                match e.local_name().as_ref() {
                    b"run" => break,
                    b"fileContent" if !empty => in_content = true,
                    b"referenceableParamGroup" if !empty => group = attr(e, b"id").map(|id| (id, Vec::new())),
                    b"cvParam" => {
                        if let Some(p) = raw_param(e) {
                            if in_content {
                                content.push(Ok(p));
                            } else if let Some((_, g)) = group.as_mut() {
                                g.push(p);
                            }
                        }
                    }
                    b"referenceableParamGroupRef" if in_content => content.extend(attr(e, b"ref").map(Err)),
                    _ => {}
                }
            }
            Event::End(e) => match e.local_name().as_ref() {
                b"fileContent" => in_content = false,
                b"referenceableParamGroup" => {
                    if let Some((id, g)) = group.take() {
                        groups.insert(id, g);
                    }
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(content
        .into_iter()
        .flat_map(|c| match c {
            Ok(p) => vec![p],
            Err(r) => groups.get(&r).cloned().unwrap_or_default(),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provenance_is_copied_as_the_header_states_it() {
        // A bare lowercase UUID (ltpmsi-chilli) and a checksum reached through a param group.
        let xml = r#"<mzML><fileDescription><fileContent>
            <cvParam cvRef="IMS" accession="IMS:1000031" name="processed" value=""/>
            <cvParam cvRef="IMS" accession="IMS:1000080" name="universally unique identifier" value="686ec248523749d8a17590dde78ab130"/>
            <referenceableParamGroupRef ref="sums"/>
            </fileContent></fileDescription>
            <referenceableParamGroupList><referenceableParamGroup id="sums">
            <cvParam cvRef="IMS" accession="IMS:1000091" name="ibd SHA-1" value="ABCDEF0123"/>
            </referenceableParamGroup></referenceableParamGroupList><run/></mzML>"#;
        let p = provenance_params(&read_file_content_from(xml.as_bytes()).unwrap());
        let got: Vec<(String, String)> = p.iter().map(|p| (p.curie().unwrap().to_string(), p.value.to_string())).collect();
        assert_eq!(got, [
            ("IMS:1000031".to_string(), String::new()),
            ("IMS:1000080".to_string(), "686ec248523749d8a17590dde78ab130".to_string()),
            ("IMS:1000091".to_string(), "ABCDEF0123".to_string()),
        ]);
    }

    /// Deterministic noise in [-1, 1) (an LCG: the tests need no rand crate).
    fn noise(seed: &mut u64) -> f64 {
        *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (*seed >> 11) as f64 / (1u64 << 52) as f64 - 1.0
    }

    /// A raster of `rows` passes over `cols` columns 0.1 mm apart, each position mapped by `at(row, col)`.
    fn raster(rows: usize, cols: usize, mut at: impl FnMut(usize, usize) -> f64) -> Vec<f64> {
        (0..rows).flat_map(|r| (0..cols).map(move |c| (r, c))).map(|(r, c)| at(r, c)).collect()
    }

    fn columns(index: &[Option<i64>]) -> Vec<i64> {
        index.iter().map(|i| i.expect("every position on the grid")).collect()
    }

    #[test]
    fn a_raster_of_float32_stage_positions_fits_its_grid() {
        // The shape of the Waters DESI run MTBLS14771: 104 columns 0.1 mm apart from 80.3673 mm,
        // stored as f32, 103 rows.
        let xs = raster(103, 104, |_, c| (80.3673f32 + c as f32 * 0.1) as f64);
        let (a, index) = fit_axis(&xs, None).unwrap();
        assert_eq!((a.pitch, a.count, a.declared), (Some(0.1), 104, false), "the float32 noise is snapped off");
        assert!(a.max_residual < 1e-4 && (a.origin - 80.3673).abs() < 1e-4, "{a:?}");
        assert_eq!(columns(&index), raster(103, 104, |_, c| c as f64 + 1.0).iter().map(|c| *c as i64).collect::<Vec<_>>());
        // The step the method declares (DesiXStep) is taken as it is.
        let (d, _) = fit_axis(&xs, Some(0.1)).unwrap();
        assert_eq!((d.pitch, d.count, d.declared), (Some(0.1), 104, true));
        // A declared step the positions do not lie on is not taken: the fit is.
        let (f, _) = fit_axis(&xs, Some(0.07)).unwrap();
        assert_eq!((f.pitch, f.count, f.declared), (Some(0.1), 104, false));
        // Missing columns keep their place.
        let (g, index) = fit_axis(&[0.0, 0.05, 0.10, 0.25, 0.30], None).unwrap();
        assert_eq!((g.pitch, g.count, index[3]), (Some(0.05), 7, Some(6)));
        // A regular raster is never folded into pixels of twice its step (nor a small one into one
        // pixel): every column is a pixel.
        for (cols, p) in [(2, 0.1), (3, 0.1), (4, 0.1), (7, 0.05), (40, 0.02)] {
            let (a, _) = fit_axis(&raster(10, cols, |_, c| (3.0f32 + c as f32 * p as f32) as f64), None).unwrap();
            assert_eq!((a.pitch, a.count), (Some(p), cols as i64), "{cols} columns at {p}");
        }
        // A step that is no whole 0.1 µm stays as fitted: 33.33 µm is not snapped to 33.3 µm.
        let (s, _) = fit_axis(&raster(5, 100, |_, c| 1.0 + c as f64 * 0.03333), None).unwrap();
        assert!((s.pitch.unwrap() - 0.03333).abs() < 1e-9 && s.count == 100, "{s:?}");
    }

    /// Review 2026-09-30 B14: the most common gap of exact sparse positions is 0.2 mm, which fails
    /// the quarter-step check — every position was lost.
    #[test]
    fn exact_sparse_positions_fit_the_smallest_step_the_gaps_share() {
        let (a, index) = fit_axis(&[0.0, 0.1, 0.3, 0.5, 0.7], None).unwrap();
        assert_eq!((a.pitch, a.count), (Some(0.1), 8));
        assert_eq!(columns(&index), [1, 2, 4, 6, 8]);
    }

    /// Review 2026-09-30 B14: jitter must not pose as the step — continuous, and recorded at 1 µm
    /// resolution (which the 1 µm merge of the most common gap turned into a 2 µm step).
    #[test]
    fn jittered_positions_fit_the_raster_step() {
        let mut seed = 7;
        let jittered = raster(20, 30, |_, c| 12.0 + c as f64 * 0.1 + 0.002 * noise(&mut seed));
        let recorded = jittered.iter().map(|v| (v * 1e3).round() / 1e3).collect::<Vec<_>>();
        let want: Vec<i64> = raster(20, 30, |_, c| c as f64 + 1.0).iter().map(|c| *c as i64).collect();
        for xs in [jittered, recorded] {
            let (a, index) = fit_axis(&xs, None).unwrap();
            assert_eq!((a.pitch, a.count), (Some(0.1), 30), "{a:?}");
            assert!(a.max_residual <= 0.0025, "{a:?}");
            assert_eq!(columns(&index), want);
        }
        // ±10 µm recorded at 2 µm over 100 rows: 11 distinct positions per column, whose 2 µm gaps
        // outnumber the gaps between columns nine to one (the 2 µm "step" of the first fix).
        let mut seed = 7;
        let coarse: Vec<f64> = raster(100, 30, |_, c| 12.0 + c as f64 * 0.1 + 0.01 * noise(&mut seed)).iter().map(|v| (v * 500.0).round() / 500.0).collect();
        let (a, index) = fit_axis(&coarse, None).unwrap();
        assert_eq!((a.pitch, a.count), (Some(0.1), 30), "{a:?}");
        assert_eq!(columns(&index), raster(100, 30, |_, c| c as f64 + 1.0).iter().map(|c| *c as i64).collect::<Vec<_>>());
    }

    /// Review 2026-09-30 B14: a serpentine raster whose return passes lag behind has two positions
    /// per column; the lag is not the step — 20 µm, and 30 or 45 µm (a lag over a quarter step made
    /// the grid a third or a half of the step), fitted or declared.
    #[test]
    fn a_serpentine_lag_is_not_the_step() {
        let want: Vec<i64> = raster(10, 30, |_, c| c as f64 + 1.0).iter().map(|c| *c as i64).collect();
        for lag in [0.02, 0.03, 0.045] {
            let xs = raster(10, 30, |r, c| 3.0 + c as f64 * 0.1 + if r % 2 == 1 { lag } else { 0.0 });
            for declared in [None, Some(0.1)] {
                let (a, index) = fit_axis(&xs, declared).unwrap();
                assert_eq!((a.pitch, a.count), (Some(0.1), 30), "lag {lag}: {a:?}");
                assert!((a.max_residual - lag / 2.0).abs() < 1e-6, "lag {lag}: {a:?}");
                assert_eq!(columns(&index), want, "lag {lag}");
            }
        }
    }

    #[test]
    fn a_single_column_is_one_pixel_with_the_declared_step_or_none() {
        let (a, index) = fit_axis(&[5.0, 5.0000004, 5.0], None).unwrap();
        assert_eq!((a.pitch, a.count, index), (None, 1, vec![Some(1); 3]));
        let (d, _) = fit_axis(&[5.0, 5.0000004, 5.0], Some(0.05)).unwrap();
        assert_eq!((d.pitch, d.count, d.declared), (Some(0.05), 1, true));
    }

    #[test]
    fn positions_on_no_raster_fit_none() {
        let sqrt: Vec<f64> = (0..30).map(|k| (k as f64).sqrt()).collect();
        assert_eq!(fit_axis(&sqrt, None), None);
        let mut seed = 11;
        let scattered: Vec<f64> = (0..200).map(|_| 5.0 + 5.0 * noise(&mut seed)).collect();
        assert_eq!(fit_axis(&scattered, None), None);
        assert_eq!(fit_axis(&[], None), None);
        assert_eq!(fit_axis(&[1.0, f64::NAN], None), None);
        // A continuum whose gaps are all under 1 µm is no column: 20000 random positions over 1 mm
        // chained into one pixel (review 2026-09-30, second pass).
        let dense: Vec<f64> = (0..20_000).map(|_| 10.5 + 0.5 * noise(&mut seed)).collect();
        assert_eq!(fit_axis(&dense, None), None);
    }

    /// Review 2026-09-30 B15: a stray scan must not take the grid away from the run. Up to 1 % of
    /// the positions may lie off it; they get no pixel and do not stretch the grid.
    #[test]
    fn a_few_strays_lose_their_pixel_and_keep_the_grid() {
        let mut xs = raster(103, 104, |_, c| (80.3673f32 + c as f32 * 0.1) as f64);
        // Parked far off the raster.
        xs.push(0.0);
        let (a, index) = fit_axis(&xs, None).unwrap();
        assert_eq!((a.pitch, a.count), (Some(0.1), 104), "{a:?}");
        assert!((a.origin - 80.3673).abs() < 1e-4, "{a:?}");
        assert_eq!(index.iter().filter(|i| i.is_none()).count(), 1);
        assert_eq!(index.last(), Some(&None));
        // Half a step off, inside the raster: its gaps are half the step, and so the smallest step
        // the positions share — the coarsest grid holding all but it wins.
        xs.pop();
        xs.push(80.3673 + 5.05);
        let (a, index) = fit_axis(&xs, None).unwrap();
        assert_eq!((a.pitch, a.count), (Some(0.1), 104), "{a:?}");
        assert_eq!(index.last(), Some(&None));
        // Few columns, several parked scans: their gaps are a third of all gaps, and must not set
        // the scale that decides what one column is.
        let mut xs = raster(100, 10, |_, c| 80.3673 + c as f64 * 0.1);
        xs.extend([0.0, 1.0, 2.0, 3.0, 4.0]);
        let (a, index) = fit_axis(&xs, None).unwrap();
        assert_eq!((a.pitch, a.count), (Some(0.1), 10), "{a:?}");
        assert_eq!(index.iter().filter(|i| i.is_none()).count(), 5);
        // Parked at the stage's home, which the grid of a raster from 10 mm holds by chance: it
        // gets no pixel all the same and does not stretch the grid 200 pixels down.
        let mut xs = raster(100, 100, |_, c| (10.0f32 + c as f32 * 0.05) as f64);
        xs.push(0.0);
        for declared in [None, Some(0.05)] {
            let (a, index) = fit_axis(&xs, declared).unwrap();
            assert_eq!((a.pitch, a.count, index[0], index.last()), (Some(0.05), 100, Some(1), Some(&None)), "{a:?}");
        }
        // More than 1 % off: no grid.
        let mut few = raster(1, 20, |_, c| c as f64 * 0.1);
        few.push(0.43);
        assert_eq!(fit_axis(&few, None), None);
    }

    /// Review 2026-09-30 B14/B15: strays inside the raster, far fewer than 1 %, took the grid away
    /// (or wrote a wrong one: 0.0501 mm) — declared step or not. The grid's phase came from a walk
    /// over the gaps that slipped a column wherever two strays split one.
    #[test]
    fn strays_inside_the_raster_keep_the_grid() {
        for seed in 1000..1030u64 {
            let mut seed = seed;
            let mut xs = raster(100, 100, |_, c| (30.1234f32 + c as f32 * 0.05) as f64);
            for _ in 0..50 {
                xs.push(30.1234 + (noise(&mut seed) + 1.0) / 2.0 * 0.05 * 99.0);
            }
            for declared in [None, Some(0.05)] {
                let (a, index) = fit_axis(&xs, declared).unwrap();
                assert_eq!((a.pitch, a.count), (Some(0.05), 100), "seed {seed}: {a:?}");
                assert_eq!(columns(&index[..10_000]), raster(100, 100, |_, c| c as f64 + 1.0).iter().map(|c| *c as i64).collect::<Vec<_>>());
            }
        }
    }

    /// Review 2026-09-30 B15, second pass: only scans at one position far off the raster are
    /// parked. A small region on the grid 1 mm off it (a QC spot, 0.25 % of the scans) lost every
    /// position.
    #[test]
    fn a_far_region_on_the_grid_keeps_its_pixels() {
        let mut xs = raster(100, 100, |_, c| (20.0f32 + c as f32 * 0.1) as f64);
        xs.extend(raster(5, 5, |_, c| (20.0f32 + (110 + c) as f32 * 0.1) as f64));
        for declared in [None, Some(0.1)] {
            let (a, index) = fit_axis(&xs, declared).unwrap();
            assert_eq!((a.pitch, a.count), (Some(0.1), 115), "{a:?}");
            assert_eq!(columns(&index[10_000..]), [111, 112, 113, 114, 115].repeat(5));
        }
    }

    /// v0.16.0's fit (2c5cb06): the most common gap between distinct positions (1 µm apart) is the
    /// step when every position lies within a quarter of it from the smallest — `(pitch, count)`.
    fn v0_16_fit(values: &[f64]) -> Option<(f64, i64)> {
        let origin = values.iter().copied().fold(f64::INFINITY, f64::min);
        let mut keys: Vec<i64> = values.iter().map(|v| ((v - origin) * 1e4).round() as i64).collect();
        keys.sort_unstable();
        keys.dedup_by(|k, last| *k - *last < 10);
        let mut gaps: HashMap<i64, usize> = HashMap::new();
        for w in keys.windows(2) {
            *gaps.entry(w[1] - w[0]).or_default() += 1;
        }
        let (&step, _) = gaps.iter().max_by_key(|(g, n)| (**n, std::cmp::Reverse(**g)))?;
        let pitch = step as f64 / 1e4;
        let place = |v: f64| ((v - origin) / pitch).round();
        let holds = values.iter().all(|&v| (v - origin - place(v) * pitch).abs() <= pitch / 4.0);
        holds.then(|| (pitch, values.iter().map(|&v| place(v) as i64 + 1).max().unwrap()))
    }

    /// Review 2026-09-30 B14, second pass: the fit must keep every exact layout v0.16.0 fitted.
    /// Without a declared step, small regions with more than a tenth of the gaps between them
    /// became a pixel each (4 × 4 spots of 5 pixels at a 12-pixel pitch: 4 pixels of 1.2 mm), and a
    /// 5 µm raster with three columns missing lost its grid. Third pass: regions one and two columns
    /// wide passed for a serpentine lag's halves (0.621 mm for a 0.1 mm raster), and pairs at
    /// irregular gaps four columns into a pixel (10.231 mm for 0.2 mm).
    #[test]
    fn exact_layouts_v0_16_fitted_keep_their_step() {
        let regions = |n: i64, width: i64, pitch: i64| -> Vec<i64> { (0..n).flat_map(|k| (0..width).map(move |c| k * pitch + c)).collect() };
        let mut layouts: Vec<(f32, Vec<i64>)> = vec![
            (0.1, regions(4, 5, 12)),
            (0.1, regions(10, 6, 15)),
            (0.1, regions(10, 5, 20)),
            (0.1, regions(2, 6, 11)),
            (0.1, regions(3, 4, 34)),
            (0.05, regions(5, 6, 11)),
            (0.005, regions(2, 100, 103)),
            (0.005, regions(2, 50, 250)),
            (0.008, regions(2, 100, 103)),
            (0.1, vec![0, 6, 7, 12, 13]),
            (0.1, vec![0, 13, 14]),
            (0.2, vec![0, 14, 15, 25, 26, 42]),
            (0.1, vec![0, 1, 11, 13, 33, 34]),
            (0.03, vec![0, 2, 6, 9, 15, 20, 21]),
            (0.2, vec![0, 1, 8, 9, 37, 38, 44, 45]),
        ];
        let mut seed = 5;
        for _ in 0..40 {
            layouts.push((0.1, (0..60).filter(|_| noise(&mut seed) > 0.0).collect()));
        }
        let mut compared = 0;
        for (step, cols) in layouts {
            let xs = raster(7, cols.len(), |_, i| (41.3f32 + cols[i] as f32 * step) as f64);
            let (pitch, count) = ((step as f64 * 1e4).round() / 1e4, cols[cols.len() - 1] - cols[0] + 1);
            if v0_16_fit(&xs).is_none_or(|(p, n)| (p - pitch).abs() > 1e-9 || n != count) {
                continue;
            }
            compared += 1;
            let (a, index) = fit_axis(&xs, None).unwrap_or_else(|| panic!("{step} {cols:?}"));
            assert_eq!((a.pitch, a.count), (Some(pitch), count), "{cols:?}");
            assert_eq!(columns(&index), raster(7, cols.len(), |_, i| (cols[i] - cols[0] + 1) as f64).iter().map(|c| *c as i64).collect::<Vec<_>>());
        }
        assert!(compared >= 47, "{compared}");
    }

    fn settings(params: &[(&str, &str, Option<(&str, &str)>)]) -> RawSettings {
        RawSettings {
            id: "s1".into(),
            params: params
                .iter()
                .map(|(a, v, u)| RawParam {
                    accession: a.to_string(),
                    value: v.to_string(),
                    unit_accession: u.map(|u| u.0.to_string()),
                    unit_name: u.map(|u| u.1.to_string()),
                })
                .collect(),
        }
    }
    const UM: Option<(&str, &str)> = Some(("UO:0000017", "micrometer"));

    #[test]
    fn x_and_y_with_a_unit_are_kept() {
        let s = settings(&[("IMS:1000046", "50", UM), ("IMS:1000047", "50", UM)]);
        assert_eq!(pixel_size_fix(&s), None);
    }

    #[test]
    fn x_and_y_without_a_unit_become_micrometre() {
        let f = pixel_size_fix(&settings(&[("IMS:1000046", "20", None), ("IMS:1000047", "25", None)])).unwrap();
        assert_eq!(f.transformation, Some(UNIT_ASSUMED));
        assert_eq!(f.write, vec![(PIXEL_X, 20.0, true), (PIXEL_Y, 25.0, true)]);
    }

    #[test]
    fn one_value_is_tested_as_area_then_length() {
        // 100 px × 10 µm = 1000 µm: 100 (µm²) is an area, 10 a length.
        let area = pixel_size_fix(&settings(&[("IMS:1000046", "100", None), ("IMS:1000042", "100", None), ("IMS:1000044", "1000", UM)])).unwrap();
        assert_eq!((area.transformation, area.write.clone()), (Some(AREA_TO_LENGTH), vec![(PIXEL_X, 10.0, true)]));
        let length = pixel_size_fix(&settings(&[("IMS:1000046", "10", UM), ("IMS:1000042", "100", None), ("IMS:1000044", "1000", UM)])).unwrap();
        assert_eq!((length.transformation, length.write.clone()), (None, vec![(PIXEL_X, 10.0, false)]));
        // Only the y axis states count and extent: it is used.
        let y_axis = pixel_size_fix(&settings(&[("IMS:1000046", "400", UM), ("IMS:1000043", "50", None), ("IMS:1000045", "1000", UM)])).unwrap();
        assert_eq!(y_axis.transformation, Some(AREA_TO_LENGTH));
        let neither = pixel_size_fix(&settings(&[("IMS:1000046", "7", UM), ("IMS:1000042", "100", None), ("IMS:1000044", "1000", UM)])).unwrap();
        assert_eq!((neither.transformation, neither.write.is_empty()), (Some(DROPPED), true));
        let untestable = pixel_size_fix(&settings(&[("IMS:1000046", "7", UM)])).unwrap();
        assert_eq!(untestable.transformation, Some(DROPPED), "no count/extent: nothing to test against");
    }

    #[test]
    fn a_unit_accession_that_disagrees_with_its_name_is_reported() {
        let cm = Some(("UO:0000015", "micrometer"));
        let f = pixel_size_fix(&settings(&[("IMS:1000046", "50", cm), ("IMS:1000047", "50", cm)])).unwrap();
        assert_eq!(f.transformation, None, "reported, not rewritten");
        assert_eq!(f.unit_mismatches.len(), 2, "{f:?}");
        let fine = Some(("UO:0000017", "micrometre"));
        assert_eq!(pixel_size_fix(&settings(&[("IMS:1000046", "50", fine), ("IMS:1000047", "50", fine)])), None);
    }

    #[test]
    fn header_params_come_with_units_and_expanded_groups() {
        let xml = r#"<?xml version="1.0"?><mzML><referenceableParamGroupList count="1">
            <referenceableParamGroup id="px"><cvParam cvRef="IMS" accession="IMS:1000046" name="pixel size" value="100" unitCvRef="UO" unitAccession="UO:0000015" unitName="micrometer"/></referenceableParamGroup>
          </referenceableParamGroupList>
          <scanSettingsList count="1"><scanSettings id="scansettings1">
            <referenceableParamGroupRef ref="px"/>
            <cvParam cvRef="IMS" accession="IMS:1000042" name="max count of pixel x" value="3"/>
            <cvParam cvRef="IMS" accession="IMS:1000411" name="one way" value=""/>
          </scanSettings></scanSettingsList><run id="r"><spectrumList count="0"/></run></mzML>"#;
        let s = read_scan_settings_from(xml.as_bytes()).unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].id, "scansettings1");
        let accs: Vec<&str> = s[0].params.iter().map(|p| p.accession.as_str()).collect();
        assert_eq!(accs, ["IMS:1000046", "IMS:1000042", "IMS:1000411"]);
        assert_eq!(s[0].params[0].unit_name.as_deref(), Some("micrometer"));
        assert_eq!(s[0].params[0].unit_accession.as_deref(), Some("UO:0000015"));
    }

    #[test]
    fn apply_rewrites_only_what_the_fix_says() {
        let mut ss = ScanSettings { id: "s1".into(), ..Default::default() };
        ss.params.push(Param::builder().name("pixel size x").curie(mzdata::curie!(IMS:1000046)).value(100.0).build());
        ss.params.push(Param::builder().name("one way").curie(mzdata::curie!(IMS:1000411)).build());
        let fix = PixelSizeFix {
            settings_id: "s1".into(),
            case: "area",
            transformation: Some(AREA_TO_LENGTH),
            write: vec![(PIXEL_X, 10.0, true)],
            unit_mismatches: vec![],
            mismatched: vec![],
            written_units: vec![],
            detail: String::new(),
        };
        apply(&fix, &mut ss);
        assert_eq!(ss.params[0].value.to_f64().unwrap(), 10.0);
        assert_eq!(ss.params[0].unit, Unit::Micrometer);
        assert!(one_way_to_flyback(&mut ss));
        assert_eq!(ss.params[1].curie().unwrap().to_string(), "IMS:1000413");
        let drop = PixelSizeFix { write: vec![], transformation: Some(DROPPED), ..fix };
        apply(&drop, &mut ss);
        assert!(ss.params.iter().all(|p| p.curie().unwrap().to_string() != "IMS:1000046"));
    }
}
