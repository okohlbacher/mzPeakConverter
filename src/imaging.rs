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

/// The share of positions a grid may leave off it: stray scans (a scan taken with the stage parked
/// far off the raster) must not take the grid away from the whole run (review 2026-09-30 B15).
pub const MAX_OFF_GRID: f64 = 0.01;

/// Positions closer than this (mm) are one position: float32 noise must not pose as a step.
const SAME_POSITION_MM: f64 = 1e-3;

/// Fit a grid axis to positions (mm): the axis and each position's 1-based pixel index, `None` for
/// the at most [`MAX_OFF_GRID`] positions that lie off it. `None` when no grid holds the rest within
/// a quarter step — the positions are not a raster — or there are none.
///
/// A `declared` step (the acquisition's own, e.g. the DESI method's `DesiXStep`) is the pitch when
/// it holds the positions. Otherwise the step is fitted (review 2026-09-30 B14; the most common gap
/// it replaced lost exact sparse rasters, took µm jitter or a serpentine lag for the step):
/// positions closer than a third of the typical gap (the 90th percentile of the gaps between
/// neighbouring positions) are one column — jitter, a lag between rows; the step is the smallest
/// gap between columns, up to twice the typical gap, whose grid ([`grid_at`]) holds the positions.
/// A stray that alone refines that grid (a scan half a step off) is dropped by taking the coarsest
/// multiple of the step that still holds all but [`MAX_OFF_GRID`] of the positions. A fully
/// regularly half-sampled raster is indistinguishable from a coarser grid and fits as one.
///
/// Scale, steps and line all come from the central 90 % of the positions: a few scans parked far off
/// the raster must neither set the scale nor pull the pitch until they sit on the grid (a
/// least-squares line through a point 800 steps away passes it, whatever the raster says).
///
/// ponytail: a single row recorded with jitter above 1 µm has no step to scale by and fits as a
/// µm raster or not at all (a declared step fixes it); jitter with more than ~8 recorded values per
/// column, a raster whose gaps are mostly three steps or more (its one-step neighbours merge), or
/// a stray lying on the grid by chance (it stretches the grid) are not caught either.
pub fn fit_axis(values: &[f64], declared: Option<f64>) -> Option<(GridAxis, Vec<Option<i64>>)> {
    if values.is_empty() || !values.iter().all(|v| v.is_finite()) {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let trim = values.len() / 20;
    let mut core = &sorted[trim..sorted.len() - trim];
    if let Some(fit) = declared.and_then(|d| grid_at(values, core, d, true)) {
        return Some((GridAxis { declared: true, ..fit.0 }, fit.1));
    }
    let gaps_of = |s: &[f64]| -> Vec<f64> { s.windows(2).map(|w| w[1] - w[0]).filter(|g| *g >= SAME_POSITION_MM).collect() };
    // One column: the positions within 1 µm of the core's, the rest (too many: `None`) off it.
    let single = |core: &[f64]| {
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
    let typical = gaps[(gaps.len() - 1) * 9 / 10];
    // Column centres: runs of positions whose neighbours are closer than a third of it, averaged.
    let mut centres: Vec<f64> = Vec::new();
    let mut run = (core[0], 0.0, 0usize);
    for &v in core {
        if v - run.0 >= typical / 3.0 {
            centres.push(run.1 / run.2 as f64);
            run = (v, 0.0, 0);
        }
        run = (v, run.1 + v, run.2 + 1);
    }
    centres.push(run.1 / run.2 as f64);
    let mut steps: Vec<f64> = centres.windows(2).map(|w| w[1] - w[0]).collect();
    steps.sort_by(f64::total_cmp);
    let mut tried = 0.0;
    // A step over twice the typical gap folds neighbouring columns into one pixel (two tissue
    // regions into two pixels) whenever the finer steps failed.
    for c in steps.into_iter().take_while(|c| *c <= 2.0 * typical) {
        // Steps within an eighth of one already tried fit the same grid (the pitch is refined).
        if c < tried * 1.125 {
            continue;
        }
        tried = c;
        if let Some(fit) = grid_at(values, core, c, false) {
            let fit = coarsest(fit, values.len());
            let Some(pitch) = fit.0.pitch else { return Some(fit) };
            // A stage step is set in whole 0.1 µm: snap the fitted one when that grid holds as many
            // positions (float32 noise would otherwise write 99.99995 µm).
            let snapped = (pitch * 1e4).round() / 1e4;
            let off = |f: &(GridAxis, Vec<Option<i64>>)| f.1.iter().filter(|i| i.is_none()).count();
            return Some(match grid_at(values, core, snapped, true) {
                Some(s) if snapped != pitch && off(&s) <= off(&fit) => s,
                _ => fit,
            });
        }
    }
    None
}

/// The least-squares line `v = origin + pitch · k` through `(k, v)`: `(pitch, origin)`, the pitch
/// `fixed` when given. `None` without a positive pitch.
fn line(points: &[(f64, f64)], fixed: Option<f64>) -> Option<(f64, f64)> {
    let n = points.len() as f64;
    let (mk, mv) = points.iter().fold((0.0, 0.0), |(a, b), (k, v)| (a + k / n, b + v / n));
    let (cov, var) = points.iter().fold((0.0, 0.0), |(c, s), (k, v)| (c + (k - mk) * (v - mv), s + (k - mk) * (k - mk)));
    let pitch = fixed.unwrap_or(cov / var);
    (pitch > 0.0).then_some((pitch, mv - pitch * mk))
}

/// The grid step `c` makes of the positions: walking the sorted `core` positions, each gap adds
/// round(gap / c) columns (a gap under half a step is the same column), and the least-squares line
/// through (column, position) gives the pitch (`c` itself when `fixed`) and the origin. `None`
/// unless all but [`MAX_OFF_GRID`] of the `values` lie within a quarter pitch of it.
fn grid_at(values: &[f64], core: &[f64], c: f64, fixed: bool) -> Option<(GridAxis, Vec<Option<i64>>)> {
    if !(c > 0.0) {
        return None;
    }
    let mut k = 0.0;
    let walked: Vec<(f64, f64)> = core
        .iter()
        .zip(std::iter::once(&core[0]).chain(core))
        .map(|(v, prev)| {
            k += ((v - prev) / c).round();
            (k, *v)
        })
        .collect();
    let fixed = fixed.then_some(c);
    let (mut pitch, mut origin) = line(&walked, fixed)?;
    let place = |pitch: f64, origin: f64| -> Vec<(i64, f64)> {
        values
            .iter()
            .map(|&v| {
                let k = ((v - origin) / pitch).round();
                (k as i64, (v - origin - k * pitch).abs())
            })
            .collect()
    };
    let mut placed = place(pitch, origin);
    // The line again through every position it holds: the core cuts the end columns in part (a
    // serpentine's lagged half), which tilts it.
    let held: Vec<(f64, f64)> =
        placed.iter().zip(values).filter(|((_, r), _)| *r <= pitch / 4.0).map(|((k, _), v)| (*k as f64, *v)).collect();
    if let Some((p, o)) = line(&held, fixed) {
        (pitch, origin, placed) = (p, o, place(p, o));
    }
    let on: Vec<&(i64, f64)> = placed.iter().filter(|(_, r)| *r <= pitch / 4.0).collect();
    if ((values.len() - on.len()) as f64) > MAX_OFF_GRID * values.len() as f64 {
        return None;
    }
    let (lo, hi) = on.iter().fold((i64::MAX, i64::MIN), |(lo, hi), (k, _)| (lo.min(*k), hi.max(*k)));
    let max_residual = on.iter().map(|(_, r)| *r).fold(0.0, f64::max);
    let axis = GridAxis { origin: origin + lo as f64 * pitch, pitch: Some(pitch), count: hi - lo + 1, max_residual, declared: false };
    Some((axis, placed.iter().map(|&(k, r)| (r <= pitch / 4.0).then_some(k - lo + 1)).collect()))
}

/// The coarsest multiple of a fitted grid's step on which all but [`MAX_OFF_GRID`] of the `n`
/// positions keep their place (one residue of the index): a stray between two columns makes a gap
/// under the step and a finer grid that still holds everything. A single column left over has no
/// step.
///
/// ponytail: multiples up to 16 — a stray closer than a third of a step to a column merges into it,
/// so the finer grid it makes is a half or a third of the step.
fn coarsest((axis, index): (GridAxis, Vec<Option<i64>>), n: usize) -> (GridAxis, Vec<Option<i64>>) {
    let mut per_index = vec![0usize; axis.count as usize];
    for i in index.iter().flatten() {
        per_index[(*i - 1) as usize] += 1;
    }
    let need = (1.0 - MAX_OFF_GRID) * n as f64;
    for m in (2..=per_index.len().min(16)).rev() {
        let mut classes = vec![0usize; m];
        for (i, c) in per_index.iter().enumerate() {
            classes[i % m] += c;
        }
        let Some(r) = (0..m).find(|&r| classes[r] as f64 >= need) else { continue };
        let keep = |i: i64| ((i - 1) as usize % m == r).then(|| (i - 1 - r as i64) / m as i64);
        let kept: Vec<Option<i64>> = index.iter().map(|i| i.and_then(keep)).collect();
        let hi = kept.iter().flatten().max().copied().unwrap_or(0);
        let lo = kept.iter().flatten().min().copied().unwrap_or(0);
        let pitch = axis.pitch.map(|p| p * m as f64);
        let origin = axis.origin + (r as f64 + lo as f64 * m as f64) * axis.pitch.unwrap_or(0.0);
        let coarse = GridAxis { origin, pitch: pitch.filter(|_| hi > lo), count: hi - lo + 1, ..axis };
        return (coarse, kept.into_iter().map(|i| i.map(|i| i - lo + 1)).collect());
    }
    (axis, index)
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
    }

    /// Review 2026-09-30 B14: a serpentine raster whose return passes lag 20 µm behind has two
    /// positions per column; the lag is not the step.
    #[test]
    fn a_serpentine_lag_is_not_the_step() {
        let xs = raster(10, 30, |r, c| 3.0 + c as f64 * 0.1 + if r % 2 == 1 { 0.02 } else { 0.0 });
        let (a, index) = fit_axis(&xs, None).unwrap();
        assert_eq!((a.pitch, a.count), (Some(0.1), 30), "{a:?}");
        assert!((a.max_residual - 0.01).abs() < 1e-9, "{a:?}");
        assert_eq!(columns(&index), raster(10, 30, |_, c| c as f64 + 1.0).iter().map(|c| *c as i64).collect::<Vec<_>>());
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
        // More than 1 % off: no grid.
        let mut few = raster(1, 20, |_, c| c as f64 * 0.1);
        few.push(0.43);
        assert_eq!(fit_axis(&few, None), None);
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
