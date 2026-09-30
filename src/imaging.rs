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
//! `IMS:1000050/51` (an mzML the probes miss is searched in full, [`file_mentions`]). A detected run
//! gets the imaging profile's `metadata.imaging` marker ([`marker_block`]) with its provenance once
//! at least one position was written; nothing else is marked imaging.

use std::collections::HashMap;
use std::io::{BufRead, Read};
use std::path::Path;

use anyhow::{Context, Result};
use mzdata::params::{CURIE, Param, ParamDescribed, Unit};
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
/// A declared pixel count did not bound the written positions — smaller than one, not a whole
/// number, or stated for the other axis only: set to the largest position on its axis.
pub const COUNT_RAISED: &str = "imaging:pixel-count-raised-to-positions";
/// A scan's position was not a pixel index the `UInt32` columns can hold (x or y missing, not
/// integral, below 1, above `u32::MAX`): all its position params were removed.
pub const INVALID_POSITION_DROPPED: &str = "imaging:invalid-position-dropped";

/// `metadata.imaging.pixel_count_source`: the source stated the counts…
pub const COUNTS_DECLARED: &str = "declared";
/// …or the writer took them from the largest positions.
pub const COUNTS_OBSERVED_MAX: &str = "observed_max";

const POSITIONS: [CURIE; 3] = [mzdata::curie!(IMS:1000050), mzdata::curie!(IMS:1000051), mzdata::curie!(IMS:1000052)];
/// The pixel counts, x and y: an entry stating either is the grid entry.
const COUNTS: [CURIE; 2] = [mzdata::curie!(IMS:1000042), mzdata::curie!(IMS:1000043)];

/// What made a run an imaging run.
pub enum Detected {
    /// imzML input: always imaging.
    ImzML,
    /// A Bruker `.d` whose `analysis.tsf`/`.tdf` has `MaldiFrameInfo` positions.
    BrukerMaldi(crate::bruker_maldi::MaldiInfo),
    /// Any other input whose spectra state `IMS:1000050`/`51` (sampled, or an mzML's full text):
    /// an mzML written from imaging data, e.g. this converter's own `--to mzml` export of an imaging
    /// archive.
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
    d.acquisition.scans.iter().find_map(scan_position)
}

fn scan_position(sc: &mzdata::spectrum::ScanEvent) -> Option<(i64, i64)> {
    let v = |c| sc.get_param_by_curie(&c)?.value.to_i64().ok();
    Some((v(mzdata::curie!(IMS:1000050))?, v(mzdata::curie!(IMS:1000051))?))
}

/// Which of `accessions` occur anywhere in a file's bytes. The detector's probes sample six spectra,
/// so positions stated only on the others were lost with no trace (review 2026-09-30 B11); this
/// finds them with one streamed byte search — no array is decoded; 2.5 s on a 6.4 GB mzML. Stops
/// once all are found. The accessions share their first byte (`IMS:` terms): one pass finds that
/// byte and tests each accession there.
pub fn file_mentions<const N: usize>(path: &Path, accessions: [&str; N]) -> std::io::Result<[bool; N]> {
    let first = accessions[0].as_bytes()[0];
    assert!(accessions.iter().all(|a| a.as_bytes().first() == Some(&first)), "{accessions:?}");
    let mut f = std::fs::File::open(path)?;
    // An accession split across two blocks is found in the next: each block keeps the previous
    // block's last `longest − 1` bytes in front.
    let keep = accessions.iter().map(|a| a.len()).max().unwrap_or(1) - 1;
    let mut buf = vec![0u8; keep + (1 << 20)];
    let (mut carried, mut found) = (0, [false; N]);
    loop {
        let n = f.read(&mut buf[carried..])?;
        if n == 0 {
            return Ok(found);
        }
        let block = &buf[..carried + n];
        let mut at = 0;
        while let Some(i) = block[at..].iter().position(|&b| b == first) {
            at += i;
            for (hit, acc) in found.iter_mut().zip(accessions) {
                *hit |= block[at..].starts_with(acc.as_bytes());
            }
            at += 1;
        }
        if found.iter().all(|h| *h) {
            return Ok(found);
        }
        carried = keep.min(block.len());
        let end = block.len();
        buf.copy_within(end - carried..end, 0);
    }
}

/// A position value the `UInt32` position columns can hold as a pixel index: integral, at least 1
/// (the profile counts from 1), at most `u32::MAX`.
fn pixel_index(p: &Param) -> bool {
    p.value.to_f64().is_ok_and(|v| v.fract() == 0.0 && (1.0..=u32::MAX as f64).contains(&v))
}

/// Remove every position a scan cannot carry as the profile has it (review 2026-09-30 B12): x and y
/// must both be pixel indices, and z too when stated. The writer used to narrow each value on its
/// own, so a negative or out-of-range one became null on ONE axis ("both set or both null") and a 0
/// was written as 0. Returns `(scans keeping a position, scans whose position was removed)`.
pub fn drop_invalid_positions(d: &mut mzdata::spectrum::SpectrumDescription) -> (usize, usize) {
    let (mut kept, mut dropped) = (0, 0);
    for sc in d.acquisition.scans.iter_mut() {
        let [x, y, z] = POSITIONS.map(|c| sc.get_param_by_curie(&c).map(pixel_index));
        if x.is_none() && y.is_none() && z.is_none() {
            continue;
        }
        if x == Some(true) && y == Some(true) && z != Some(false) {
            kept += 1;
        } else {
            sc.params_mut().retain(|p| !p.curie().is_some_and(|c| POSITIONS.contains(&c)));
            dropped += 1;
        }
    }
    (kept, dropped)
}

/// The grid entry of a scan settings list: the one stating the pixel counts (either axis: an entry
/// with only a y count used to be passed over and a second grid entry added).
pub fn grid(list: &[ScanSettings]) -> Option<&ScanSettings> {
    list.iter().find(|s| states(s, &COUNTS))
}

fn states(s: &ScanSettings, accessions: &[CURIE]) -> bool {
    s.params.iter().any(|p| p.curie().is_some_and(|c| accessions.contains(&c)))
}

/// The largest position written, per axis — what the pixel counts must at least be.
#[derive(Debug, Default)]
pub struct Extent(pub i64, pub i64);

impl Extent {
    /// Every positioned scan of the spectrum, not only the first (review 2026-09-30 B13).
    pub fn observe(&mut self, d: &mzdata::spectrum::SpectrumDescription) {
        for (x, y) in d.acquisition.scans.iter().filter_map(scan_position) {
            (self.0, self.1) = (self.0.max(x), self.1.max(y));
        }
    }

    /// Make the grid bound the written positions. Without a grid entry the counts are the largest
    /// positions ([`COUNT_FROM_POSITIONS`]), added to the entry that states a pixel size (one grid
    /// description), else to a new entry under an id not in use; a declared count that does not
    /// bound the positions (below one, not a whole number, missing beside the other axis') is set
    /// to the largest position ([`COUNT_RAISED`]). The transformation applied, if any.
    pub fn bound(&self, list: &mut Vec<ScanSettings>) -> Option<&'static str> {
        let grids = list.iter().filter(|s| states(s, &COUNTS)).count();
        if grids > 1 {
            // The profile wants exactly one grid entry; which one to keep is an owner decision.
            log::warn!("{grids} scan settings state pixel counts (IMS:1000042/43); the imaging profile describes one grid — the first is the one checked");
        }
        let (i, counted) = match list.iter().position(|s| states(s, &COUNTS)) {
            Some(i) => (i, false),
            None => {
                let sized = list.iter().position(|s| states(s, &[mzdata::curie!(IMS:1000046), mzdata::curie!(IMS:1000047)]));
                let i = sized.unwrap_or_else(|| {
                    let id = (1..).map(|k| format!("scansettings{k}")).find(|id| list.iter().all(|s| &s.id != id)).unwrap();
                    list.push(ScanSettings { id, ..Default::default() });
                    list.len() - 1
                });
                (i, true)
            }
        };
        let s = &mut list[i];
        let raised = raise_count(s, COUNTS[0], "max count of pixels x", self.0)
            | raise_count(s, COUNTS[1], "max count of pixels y", self.1);
        if counted { Some(COUNT_FROM_POSITIONS) } else { raised.then_some(COUNT_RAISED) }
    }
}

/// Set a pixel count to `max` unless it already states an integer of at least that (a fractional
/// count is replaced, even by a smaller `max`: it is no count of pixels). `true` when it changed.
fn raise_count(s: &mut ScanSettings, curie: CURIE, name: &str, max: i64) -> bool {
    match s.params.iter_mut().find(|p| p.curie() == Some(curie)) {
        Some(p) if p.value.to_f64().is_ok_and(|v| v.fract() == 0.0 && v >= max as f64) => false,
        Some(p) => {
            p.value = max.into();
            true
        }
        None => {
            s.params.push(Param::builder().name(name).curie(curie).value(max).build());
            true
        }
    }
}

/// One axis of a pixel grid fitted to stage positions in mm (Waters states laser positions, not pixel
/// indices): the position of pixel 1, the step (`None` for a single column), the pixel count, and
/// the farthest any position lies from its grid point.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GridAxis {
    pub origin: f64,
    pub pitch: Option<f64>,
    pub count: i64,
    pub max_residual: f64,
}

impl GridAxis {
    /// The 1-based pixel index of a position.
    pub fn index(&self, v: f64) -> i64 {
        self.pitch.map_or(1, |p| ((v - self.origin) / p).round() as i64 + 1)
    }
}

/// Fit a grid axis to positions (mm). The step is the most common gap between neighbouring distinct
/// positions, positions closer than 1 µm counting as one (float32 noise must not pose as a step);
/// every position must then lie within a quarter step of its grid point. `None` when they do not —
/// the positions are not a raster — or there are none.
pub fn fit_axis(values: &[f64]) -> Option<GridAxis> {
    let origin = values.iter().copied().fold(f64::INFINITY, f64::min);
    if !origin.is_finite() {
        return None;
    }
    // Distinct positions in 0.1 µm units, merged within 1 µm.
    let mut keys: Vec<i64> = values.iter().map(|v| ((v - origin) * 1e4).round() as i64).collect();
    keys.sort_unstable();
    let mut distinct: Vec<i64> = Vec::new();
    for k in keys {
        if distinct.last().is_none_or(|&last| k - last >= 10) {
            distinct.push(k);
        }
    }
    let spread = |pitch: f64| values.iter().map(move |&v| {
        let k = ((v - origin) / pitch).round();
        (k as i64 + 1, (v - (origin + k * pitch)).abs())
    });
    if distinct.len() < 2 {
        let max_residual = values.iter().map(|&v| v - origin).fold(0.0, f64::max);
        return Some(GridAxis { origin, pitch: None, count: 1, max_residual });
    }
    let mut gaps: HashMap<i64, usize> = HashMap::new();
    for w in distinct.windows(2) {
        *gaps.entry(w[1] - w[0]).or_default() += 1;
    }
    let (&step, _) = gaps.iter().max_by_key(|(g, n)| (**n, std::cmp::Reverse(**g)))?;
    let pitch = step as f64 / 1e4;
    let (count, max_residual) = spread(pitch).fold((1, 0.0f64), |(c, r), (k, d)| (c.max(k), r.max(d)));
    (max_residual <= pitch / 4.0).then_some(GridAxis { origin, pitch: Some(pitch), count, max_residual })
}

/// The imaging profile's `metadata.imaging` index block (HUPO-PSI/mzPeak-specification#24): the
/// marker, the coordinate base, the grid as the viewer reads it and where its counts came from
/// (`pixel_count_source`: [`COUNTS_DECLARED`] or [`COUNTS_OBSERVED_MAX`]; review 2026-09-30 B18),
/// and where it all came from.
pub fn marker_block(grid: Option<&ScanSettings>, pixel_count_source: &str, provenance: serde_json::Value) -> serde_json::Value {
    let mut b = serde_json::json!({"is_imaging": true, "coordinate_base": 1, "provenance": provenance});
    let param = |acc| grid?.params.iter().find(|p| p.curie() == Some(acc));
    let int = |acc| param(acc)?.value.to_i64().ok();
    let um = |acc| param(acc).filter(|p| p.unit == Unit::Micrometer)?.value.to_f64().ok();
    if let (Some(x), Some(y)) = (int(mzdata::curie!(IMS:1000042)), int(mzdata::curie!(IMS:1000043))) {
        b["pixel_count"] = serde_json::json!({"x": x, "y": y});
        b["pixel_count_source"] = pixel_count_source.into();
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

    fn scan(params: &[(CURIE, &str)]) -> mzdata::spectrum::ScanEvent {
        let mut sc = mzdata::spectrum::ScanEvent::default();
        for (c, v) in params {
            sc.add_param(Param::builder().name("p").curie(*c).value(v.parse::<mzdata::params::Value>().unwrap()).build());
        }
        sc
    }
    const X: CURIE = mzdata::curie!(IMS:1000050);
    const Y: CURIE = mzdata::curie!(IMS:1000051);
    const Z: CURIE = mzdata::curie!(IMS:1000052);

    /// Review 2026-09-30 B12: a position the `UInt32` columns cannot hold as a pixel index leaves the
    /// scan whole — x, y and z together — instead of becoming null on one axis.
    #[test]
    fn positions_that_are_not_pixel_indices_are_removed() {
        let mut d = mzdata::spectrum::SpectrumDescription::default();
        let other = (mzdata::curie!(MS:1000016), "1.5");
        d.acquisition.scans = vec![
            scan(&[(X, "1"), (Y, "4294967295"), other]),
            scan(&[(X, "3.0"), (Y, "2"), (Z, "7")]),
            scan(&[(X, "0"), (Y, "2")]),
            scan(&[(X, "-3"), (Y, "2")]),
            scan(&[(X, "2.5"), (Y, "2")]),
            scan(&[(X, "4294967296"), (Y, "2")]),
            scan(&[(X, "2"), other]),
            scan(&[(X, "2"), (Y, "2"), (Z, "0")]),
            scan(&[other]),
        ];
        assert_eq!(drop_invalid_positions(&mut d), (2, 6));
        let left: Vec<usize> = d.acquisition.scans.iter().map(|s| s.params().iter().filter(|p| p.curie().is_some_and(|c| POSITIONS.contains(&c))).count()).collect();
        assert_eq!(left, [2, 3, 0, 0, 0, 0, 0, 0, 0]);
        assert!(d.acquisition.scans[6].get_param_by_curie(&mzdata::curie!(MS:1000016)).is_some(), "other params stay");
    }

    /// Review 2026-09-30 B13: every positioned scan counts toward the extent; the counts are
    /// derived into the entry that states the pixel size, else a new entry under a free id, and
    /// declared counts that do not bound the positions are raised.
    #[test]
    fn the_extent_sees_every_scan_and_bounds_the_grid() {
        let mut d = mzdata::spectrum::SpectrumDescription::default();
        d.acquisition.scans = vec![scan(&[(X, "1"), (Y, "2")]), scan(&[(X, "4"), (Y, "1")])];
        let mut e = Extent::default();
        e.observe(&d);
        assert_eq!((e.0, e.1), (4, 2));
        let count = |s: &ScanSettings, c: CURIE| s.params.iter().find(|p| p.curie() == Some(c)).map(|p| p.value.to_i64().unwrap());
        let settings = |id: &str, params: &[(CURIE, &str)]| {
            let mut s = ScanSettings { id: id.into(), ..Default::default() };
            s.params = scan(params).params().to_vec();
            s
        };
        let (cx, cy) = (mzdata::curie!(IMS:1000042), mzdata::curie!(IMS:1000043));

        // No list at all: a new entry.
        let mut list = Vec::new();
        assert_eq!(e.bound(&mut list), Some(COUNT_FROM_POSITIONS));
        assert_eq!((list[0].id.as_str(), count(&list[0], cx), count(&list[0], cy)), ("scansettings1", Some(4), Some(2)));
        // An entry without a grid takes the next free id…
        let mut list = vec![settings("scansettings1", &[(mzdata::curie!(IMS:1000044), "300")])];
        assert_eq!(e.bound(&mut list), Some(COUNT_FROM_POSITIONS));
        assert_eq!((list.len(), list[1].id.as_str(), count(&list[1], cx)), (2, "scansettings2", Some(4)));
        // …unless it states the pixel size: the counts join it, one grid description.
        let mut list = vec![settings("s", &[(mzdata::curie!(IMS:1000046), "100")])];
        assert_eq!(e.bound(&mut list), Some(COUNT_FROM_POSITIONS));
        assert_eq!((list.len(), count(&list[0], cx), count(&list[0], cy)), (1, Some(4), Some(2)));
        // Declared counts: kept when they bound the positions, raised per axis when not.
        let mut list = vec![settings("s", &[(cx, "5"), (cy, "5")])];
        assert_eq!(e.bound(&mut list), None);
        assert_eq!((count(&list[0], cx), count(&list[0], cy)), (Some(5), Some(5)));
        let mut list = vec![settings("s", &[(cx, "3"), (cy, "2")])];
        assert_eq!(e.bound(&mut list), Some(COUNT_RAISED));
        assert_eq!((count(&list[0], cx), count(&list[0], cy)), (Some(4), Some(2)));
        // A y count alone makes the grid entry: x joins it (no second entry), y is kept.
        let mut list = vec![settings("s", &[(cy, "50")])];
        assert_eq!(e.bound(&mut list), Some(COUNT_RAISED));
        assert_eq!((list.len(), count(&list[0], cx), count(&list[0], cy)), (1, Some(4), Some(50)));
        assert_eq!(grid(&list).map(|s| s.id.as_str()), Some("s"));
        // A fractional count is no count of pixels: replaced by the largest position.
        let mut list = vec![settings("s", &[(cx, "25.5"), (cy, "5")])];
        assert_eq!(e.bound(&mut list), Some(COUNT_RAISED));
        assert_eq!(count(&list[0], cx), Some(4));
    }

    /// Review 2026-09-30 B11: the full-input search finds an accession anywhere, including one
    /// that straddles two read blocks, and only the ones present.
    #[test]
    fn file_mentions_finds_accessions_across_blocks() {
        let p = std::env::temp_dir().join(format!("mzpc-imaging-mentions-{}", std::process::id()));
        let keep = "IMS:1000050".len() - 1;
        let mut bytes = vec![b'I'; keep + (1 << 20) - 5];
        bytes.extend_from_slice(b"IMS:1000051 IMS:1000050");
        std::fs::write(&p, &bytes).unwrap();
        assert_eq!(file_mentions(&p, ["IMS:1000050", "IMS:1000051", "IMS:1000052"]).unwrap(), [true, true, false]);
        std::fs::write(&p, "no positions here").unwrap();
        assert_eq!(file_mentions(&p, ["IMS:1000050", "IMS:1000051"]).unwrap(), [false, false]);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_raster_of_float32_stage_positions_fits_its_grid() {
        // The shape of a Waters DESI run (MTBLS14771): 0.1 mm steps from 80.3673 mm, stored as f32.
        let xs: Vec<f64> = (0..104).map(|k| (80.3673f32 + k as f32 * 0.1) as f64).collect();
        let a = fit_axis(&xs).unwrap();
        assert_eq!((a.pitch, a.count), (Some(0.1), 104));
        assert!(a.max_residual < 1e-4, "{a:?}");
        assert_eq!((a.index(xs[0]), a.index(xs[103])), (1, 104));
        // Missing columns keep their place; the step is still the common gap.
        let gappy: Vec<f64> = [0.0, 0.05, 0.10, 0.25, 0.30].to_vec();
        let g = fit_axis(&gappy).unwrap();
        assert_eq!((g.pitch, g.count, g.index(0.25)), (Some(0.05), 7, 6));
        // One column: a single pixel, no step.
        assert_eq!(fit_axis(&[5.0, 5.0000004]).unwrap().count, 1);
        // Not a raster: positions far off any common step.
        assert!(fit_axis(&[0.0, 0.1, 0.2, 0.37, 0.4]).is_none());
        assert!(fit_axis(&[]).is_none());
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
