//! Imaging metadata the imzML lane checks or restores instead of copying blindly
//! (HUPO-PSI/mzPeak-specification#23; handoff 2026-09-29):
//!
//! * **Pixel size.** A survey of 784 public imzML headers found pixel sizes without a unit, the
//!   centimetre accession labelled "micrometer", and `IMS:1000046` given as an AREA (its meaning
//!   until 2017). [`pixel_size_fixes`] applies the issue author's rule: x and y with a unit are kept;
//!   x and y without one are taken as micrometre; a single value is tested against pixel count and
//!   extent — an area when `√value × count = extent` (written as its square root), a length when
//!   `value × count = extent` — and anything else is dropped. A lone `IMS:1000046` the rule keeps
//!   is also written as `IMS:1000047`: the vocabulary defines it as the y size too when no
//!   `IMS:1000047` is stated. Nothing changes silently: every change of a value or a unit becomes
//!   a `transformations` entry and a row of the `imaging_pixel_size` index block. The vocabulary's
//!   default made explicit — the y of a kept lone x, and the terms' current names — declares
//!   nothing and is in the row only (its `written_um` and `detail`).
//! * **"one way"** (`IMS:1000411`, obsolete) is written as its stated replacement, flyback
//!   (`IMS:1000413`), and declared.
//! * **File provenance.** mzdata consumes storage mode, UUID and `.ibd` checksum into its
//!   `ImzMLFileMetadata` and leaves them out of `file_description`; [`provenance_params`] puts them
//!   back from the header ([`read_file_content`]), values as stated. The checksum is also CHECKED
//!   ([`check_ibd`]): the stated value stays, a mismatch is declared with the hash found.
//! * **Obsolete integer type terms.** `IMS:1000141`/`142` ("32-bit integer"/"64-bit integer") are
//!   replaced by the PSI-MS terms they were obsoleted for in the reader's copy of the header
//!   ([`replace_obsolete_integer_terms`]), declared; mzdata knows only the PSI-MS ones.
//!
//! mzdata keeps one unit per param (the name's when it knows the name, else the accession's), so
//! the unit accession AND name the file states — needed to see a disagreement — are read from the
//! header here ([`read_scan_settings`]).
//!
//! **Which runs are imaging** is decided by [`detect`], the one detector every lane calls: imzML
//! input always; a Bruker `.d` with `MaldiFrameInfo` positions; any other input whose spectra state
//! `IMS:1000050/51` (an mzML the probes miss is searched in full, [`file_mentions`]). A detected run
//! gets the imaging profile's `metadata.imaging` marker ([`marker_block`]) with its provenance once
//! at least one position was written; nothing else is marked imaging.

use std::collections::HashMap;
use std::io::{BufRead, Read};
use std::path::Path;

use anyhow::{bail, Context, Result};
use mzdata::params::{CURIE, Param, ParamDescribed, Unit};
use mzdata::meta::ScanSettings;
use quick_xml::events::Event;

pub const UNIT_ASSUMED: &str = "imzml:pixel-size-unit-assumed-um";
pub const AREA_TO_LENGTH: &str = "imzml:pixel-size-area-to-length";
pub const DROPPED: &str = "imzml:pixel-size-dropped";
pub const ONE_WAY_AS_FLYBACK: &str = "imzml:one-way-as-flyback";
/// A binary array's data type was declared with the imaging vocabulary's obsolete `IMS:1000141`
/// ("32-bit integer") or `IMS:1000142` ("64-bit integer"): read as `MS:1000519` / `MS:1000522`.
pub const INTEGER_TYPE_AS_PSI_MS: &str = "imzml:obsolete-integer-type-as-psi-ms";
/// The `.ibd` does not hash to the checksum the header states (`IMS:1000090/91/92`). The stated
/// value is kept in `file_description`; the hash found is in `metadata.imaging.provenance`.
pub const IBD_CHECKSUM_MISMATCH: &str = "imzml:ibd-checksum-mismatch";
/// The `.ibd` does not begin with the UUID the header states (`IMS:1000080`): the two files are not
/// the pair the imzML describes. The stated value is kept in `file_description`; the UUID found is
/// in `metadata.imaging.provenance`.
pub const IBD_UUID_MISMATCH: &str = "imzml:ibd-uuid-mismatch";
/// The unit mzdata wrote differs from the unit ACCESSION the file states (mzdata takes the
/// `unitName` when it names a unit mzdata knows, whatever the attribute order).
pub const UNIT_FROM_NAME: &str = "imzml:unit-accession-replaced-by-name";
/// A pixel-size param's unit accession and unit name disagree, and only the accession's reading
/// passes the extent test (`value × count = extent`, or `√value × count` for an area): the value is
/// written in micrometres from the accession's unit. Owner decision D4 (2026-10-01): through
/// 0.17.0-rc.2 only the name's reading was tested, and 0.005 `UO:0000015` named "micrometer" over
/// 3 pixels × 150 µm was dropped.
pub const UNIT_FROM_ACCESSION: &str = "imzml:pixel-size-unit-from-accession";
/// `--pixel-size`: the pixel size written is the one the user supplied, the source settling none —
/// or, under `--force`, a different one, which the `imaging_pixel_size` row records.
pub const USER_SUPPLIED: &str = "imaging:pixel-size-user-supplied";
/// The input states positions but no pixel counts: `IMS:1000042/43` are the largest positions.
pub const COUNT_FROM_POSITIONS: &str = "imaging:pixel-count-from-positions";
/// A declared pixel count did not bound the written positions — smaller than one, not a whole
/// number, or stated for the other axis only: set to the largest position on its axis.
pub const COUNT_RAISED: &str = "imaging:pixel-count-raised-to-positions";
/// A scan's position was not a pixel index the `UInt32` columns can hold (x or y missing, not
/// integral, below 1, above `u32::MAX`): all its position params were removed.
pub const INVALID_POSITION_DROPPED: &str = "imaging:invalid-position-dropped";
/// A scan's x and y were pixel indices but its z was not: only the z param was removed.
pub const INVALID_Z_DROPPED: &str = "imaging:invalid-position-z-dropped";

/// `metadata.imaging.pixel_count_source`: the source stated the counts…
pub const COUNTS_DECLARED: &str = "declared";
/// …or the writer took them from the largest positions.
pub const COUNTS_OBSERVED_MAX: &str = "observed_max";

/// `metadata.imaging.pixel_size_source`: how `pixel_size_um` was settled (converter-defined, owner
/// principle P4; proposed for HUPO-PSI/mzPeak-specification#25 with the issue author's four values
/// verbatim). `unknown` whenever the marker states no `pixel_size_um`.
pub const SOURCE_DECLARED: &str = "declared";
pub const SOURCE_UNIT_ASSUMED: &str = "unit_assumed";
pub const SOURCE_FROM_AREA: &str = "derived_from_area";
pub const SOURCE_USER_SUPPLIED: &str = "user_supplied";
pub const SOURCE_FROM_POSITIONS: &str = "derived_from_positions";
pub const SOURCE_FROM_BEAM: &str = "derived_from_beam_size";
pub const SOURCE_UNKNOWN: &str = "unknown";
/// `provenance.pixel_size` of a marker whose size `--pixel-size` wrote.
pub const USER_SUPPLIED_PROVENANCE: &str = "user supplied (--pixel-size)";

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

/// How many of the first `limit` spectra of an mzML or imzML state a scan start time
/// (`MS:1000016`), on the spectrum or through a param group it references: `(stating, spectra)`.
/// mzdata gives a scan without the term the time 0, so a stated 0 and no time at all read alike;
/// the marker's `provenance.time` said "as stated" of a run where one spectrum of nine states one.
/// One pass over the text (an imzML's is its header and scans); no array is decoded.
pub fn spectra_stating_time(path: &Path, limit: usize) -> Result<(usize, usize)> {
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    spectra_stating_time_from(std::io::BufReader::new(file), limit)
}

pub fn spectra_stating_time_from(input: impl BufRead, limit: usize) -> Result<(usize, usize)> {
    const TIME: &str = "MS:1000016";
    let mut reader = quick_xml::Reader::from_reader(input);
    let mut buf = Vec::new();
    // The param groups that hold the term, the group being read, and whether the spectrum being
    // read has stated it.
    let mut timed_groups: std::collections::HashSet<String> = Default::default();
    let mut group: Option<String> = None;
    let mut spectrum: Option<bool> = None;
    let (mut stating, mut spectra) = (0, 0);
    while spectra < limit {
        let ev = reader.read_event_into(&mut buf).context("parsing the source's scan times")?;
        match &ev {
            Event::Start(e) | Event::Empty(e) => match e.local_name().as_ref() {
                b"referenceableParamGroup" if matches!(ev, Event::Start(_)) => group = attr(e, b"id"),
                b"spectrum" if matches!(ev, Event::Start(_)) => spectrum = Some(false),
                b"spectrum" => spectra += 1,
                b"cvParam" if attr(e, b"accession").as_deref() == Some(TIME) => {
                    if let Some(stated) = spectrum.as_mut() {
                        *stated = true;
                    } else if let Some(id) = &group {
                        timed_groups.insert(id.clone());
                    }
                }
                b"referenceableParamGroupRef" => {
                    if let (Some(stated), Some(id)) = (spectrum.as_mut(), attr(e, b"ref")) {
                        *stated |= timed_groups.contains(&id);
                    }
                }
                _ => {}
            },
            Event::End(e) => match e.local_name().as_ref() {
                b"referenceableParamGroup" => group = None,
                b"spectrum" => {
                    spectra += 1;
                    stating += usize::from(spectrum.take() == Some(true));
                }
                b"spectrumList" => break,
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok((stating, spectra))
}

/// A position value the `UInt32` position columns can hold as a pixel index: integral, at least 1
/// (the profile counts from 1), at most `u32::MAX`.
fn pixel_index(p: &Param) -> bool {
    p.value.to_f64().is_ok_and(|v| v.fract() == 0.0 && (1.0..=u32::MAX as f64).contains(&v))
}

/// Remove every position a scan cannot carry as the profile has it (review 2026-09-30 B12): x and y
/// must both be pixel indices, else x, y and z are removed together; a z that is not one is removed
/// alone, the scan keeping x and y (`position_z` is optional). The writer used to narrow each value
/// on its own, so a negative or out-of-range one became null on ONE axis ("both set or both null")
/// and a 0 was written as 0. Returns `(scans keeping a position, scans whose position was removed,
/// scans whose z alone was removed)`.
pub fn drop_invalid_positions(d: &mut mzdata::spectrum::SpectrumDescription) -> (usize, usize, usize) {
    let (mut kept, mut dropped, mut z_dropped) = (0, 0, 0);
    for sc in d.acquisition.scans.iter_mut() {
        let [x, y, z] = POSITIONS.map(|c| sc.get_param_by_curie(&c).map(pixel_index));
        if x.is_none() && y.is_none() && z.is_none() {
            continue;
        }
        let removed: &[CURIE] = if x != Some(true) || y != Some(true) {
            dropped += 1;
            &POSITIONS
        } else if z == Some(false) {
            (kept, z_dropped) = (kept + 1, z_dropped + 1);
            &POSITIONS[2..]
        } else {
            kept += 1;
            continue;
        };
        sc.params_mut().retain(|p| !p.curie().is_some_and(|c| removed.contains(&c)));
    }
    (kept, dropped, z_dropped)
}

/// The grid entry of a scan settings list: the one stating the pixel counts (either axis: an entry
/// with only a y count used to be passed over and a second grid entry added).
pub fn grid(list: &[ScanSettings]) -> Option<&ScanSettings> {
    list.iter().find(|s| states(s, &COUNTS))
}

pub fn grid_mut(list: &mut [ScanSettings]) -> Option<&mut ScanSettings> {
    list.iter_mut().find(|s| states(s, &COUNTS))
}

/// The pixel size a grid entry states, both axes in µm (`None` when either is missing, not
/// positive or in a unit that is no length).
pub fn pixel_size_um(grid: &ScanSettings) -> Option<(f64, f64)> {
    let um = |curie| grid.params.iter().find(|p| p.curie() == Some(curie)).and_then(um_of);
    Some((um(mzdata::curie!(IMS:1000046))?, um(mzdata::curie!(IMS:1000047))?))
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

/// How far from its grid point a position may lie on a lattice fitted without a declared step: the
/// positions a grid point holds are within [`SAME_POSITION_MM`] of each other.
const ON_LATTICE_MM: f64 = SAME_POSITION_MM / 2.0;

/// How far from its grid point a position may lie on a declared step, in steps.
const QUARTER: f64 = 0.25;

/// Fit a grid axis to positions (mm): the axis and each position's 1-based pixel index, `None` for
/// the at most [`MAX_OFF_GRID`] positions that lie off it. `None` when no grid holds the rest — the
/// positions are not a raster — or there are none.
///
/// The raster is the central 90 % of the positions and every position reached from it by gaps at
/// most four times the largest gap inside it, and past a longer gap every group of positions that
/// spans columns (a second section, a QC region). A group at one position is scans parked off the
/// raster (at the stage's home): they get no pixel even when they lie on the grid by chance, where
/// they would stretch it by hundreds of pixels (review 2026-09-30 B15) — unless they are more than
/// [`MAX_OFF_GRID`] of the positions, and so a part of the raster.
///
/// A `declared` step (the acquisition's own, e.g. the DESI method's `DesiXStep`) is the pitch when
/// it holds the positions within a quarter step. Otherwise the grid must hold them exactly, as the
/// stage's set points (float32) lie (review 2026-09-30 B14: merging and folding heuristics kept
/// writing wrong grids; this fit refuses rather than guesses). Positions within 1 µm are one; one
/// holding all but [`MAX_OFF_GRID`] of the scans is a single column. Else the step is the largest
/// gap between neighbouring distinct positions, of at least 3 µm, whose lattice holds all but
/// [`MAX_OFF_GRID`] of the scans, decided at that gap: the 1 µm window of the residues modulo it
/// holding the most scans must hold them. Least squares then refines pitch and origin by float
/// noise only, and each position within half a µm of a grid point lies on it. A multiple of the
/// true step leaves columns off its lattice; a stray half a step off makes half the step hold too,
/// but the step is larger.
///
/// ponytail: jittered positions need a declared step — positions more than 1 µm off one lattice
/// (jitter, a serpentine lag, regions rastered from origins off one lattice, a rotated raster) fit
/// none; a step under 3 µm, or one no two neighbouring positions are apart, is not fitted, and a
/// very sparse raster at large stage coordinates may be refused;
/// positions recorded at 3 µm or coarser, or offset by a whole finer step (a lag, a region), fit
/// that finer lattice, each at its own pixel; where the step fails (strays beyond 1 %, or no
/// neighbouring gap), a stray's gap on a fraction of it may hold every column; a single column drops
/// up to 1 % of the scans even on a neighbouring column; and a handful of positions a few µm apart
/// may lie on a lattice by chance.
pub fn fit_axis(values: &[f64], declared: Option<f64>) -> Option<AxisFit> {
    if values.is_empty() || !values.iter().all(|v| v.is_finite()) {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let trim = values.len() / 20;
    let core = &sorted[trim..sorted.len() - trim];
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
    let few = |off: usize| off as f64 <= MAX_OFF_GRID * values.len() as f64;
    let raster = if few(parked) {
        let regions = |s: &[f64]| -> Vec<f64> { s.chunk_by(near).filter(|g| !one(g)).flatten().copied().collect() };
        [regions(&sorted[..a]), sorted[a..=b].to_vec(), regions(&sorted[b + 1..])].concat()
    } else {
        sorted.clone()
    };
    let raster = &raster[..];
    if let Some(fit) = declared.and_then(|d| grid_at(values, raster, d, None, true, QUARTER * d)) {
        return Some((GridAxis { declared: true, ..fit.0 }, fit.1));
    }
    let mean = |r: &[f64]| r.iter().sum::<f64>() / r.len() as f64;
    let distinct: Vec<(f64, usize)> = runs(raster).into_iter().map(|r| (mean(r), r.len())).collect();
    let (centre, _) = distinct.iter().copied().max_by_key(|d| d.1)?;
    let column: Vec<Option<i64>> = values.iter().map(|v| ((v - centre).abs() < SAME_POSITION_MM).then_some(1)).collect();
    let off = |index: &[Option<i64>]| index.iter().filter(|i| i.is_none()).count();
    if few(off(&column)) {
        let max_residual = values.iter().zip(&column).filter(|(_, i)| i.is_some()).map(|(v, _)| (v - centre).abs()).fold(0.0, f64::max);
        return Some((GridAxis { origin: centre, pitch: None, count: 1, max_residual, declared: false }, column));
    }
    // The gaps of 3 µm or more, less a float32 step at the positions' magnitude: a 3 µm step's float32
    // gaps lie on both sides of it.
    let min_step = 3.0 * SAME_POSITION_MM;
    let float32 = f32::EPSILON as f64 * sorted[0].abs().max(sorted[sorted.len() - 1].abs());
    let mut gaps: Vec<f64> = distinct.windows(2).map(|w| w[1].0 - w[0].0).filter(|g| *g >= min_step - float32).collect();
    gaps.sort_by(f64::total_cmp);
    // The step is the first gap whose lattice holds, at that gap itself: a pitch refined before the
    // decision slid from a stray's gap, or a lag's, to a fraction of the step that holds every
    // column (review 2026-09-30, fourth pass). The residues modulo g sorted, and again one g on (a
    // window may wrap around): the window of 1 µm holding the most scans must hold all but
    // MAX_OFF_GRID of them; its mean residue is the phase.
    let holds = |g: f64| -> Option<f64> {
        let mut r: Vec<(f64, usize)> = distinct.iter().map(|&(v, n)| ((v - distinct[0].0).rem_euclid(g), n)).collect();
        r.sort_by(|a, b| a.0.total_cmp(&b.0));
        let n = r.len();
        r.extend_from_within(..);
        r[n..].iter_mut().for_each(|x| x.0 += g);
        let (mut best, mut j, mut held) = ((0, 0, 0), 0, 0);
        for i in 0..n {
            while j < i + n && r[j].0 - r[i].0 < SAME_POSITION_MM {
                (held, j) = (held + r[j].1, j + 1);
            }
            if held > best.0 {
                best = (held, i, j);
            }
            held -= r[i].1;
        }
        let phase = r[best.1..best.2].iter().map(|(x, k)| x * *k as f64).sum::<f64>() / best.0 as f64;
        few(values.len() - best.0).then_some(distinct[0].0 + phase)
    };
    // Largest first, each run of gaps within 1 µm: first the mean of the gaps within four float32
    // steps of its median (a float32 step's variants average to the step over the raster; a stray
    // splitting a column's gap does not pull it off), then up to 16 other gaps of the run, largest
    // first, each two float32 steps from those tried — a sparse raster's scans just off absent
    // columns can outnumber its step's gaps and move the median off the step (review 2026-09-30,
    // fifth pass). Each is tested at itself, then outward by quarter float32 steps to two, the
    // first that holds: a sparse raster's one or two float32 gaps drift past 1 µm over the raster,
    // and a shift that small is noise, never a fraction. A run with a gap that holds ends the
    // search, fitted or not: a smaller run would be a fraction of the step.
    let mut fit = None;
    let mut from_other = false;
    for run in runs(&gaps).into_iter().rev() {
        let median = run[run.len() / 2];
        let centre = mean(&run.iter().copied().filter(|g| (g - median).abs() <= 4.0 * float32).collect::<Vec<_>>());
        let mut tried = vec![centre];
        for &g in run.iter().rev() {
            if tried.len() > 16 {
                break;
            }
            if tried.iter().all(|t| (g - t).abs() > 2.0 * float32) {
                tried.push(g);
            }
        }
        let mut held = false;
        fit = tried.into_iter().find_map(|g0| {
            let (g, origin) = [0, -1, 1, -2, 2, -3, 3, -4, 4, -5, 5, -6, 6, -7, 7, -8, 8]
                .into_iter()
                .map(|i| g0 + i as f64 / 4.0 * float32)
                .find_map(|g| holds(g).map(|o| (g, o)))?;
            held = true;
            from_other = g0 != centre;
            // Least squares refines pitch and origin by float noise only — the grid points move by
            // 1 µm at most over the raster, and it places no scan the grid at g leaves off (a stray
            // half a µm off would slide the pitch to seat it) — else the grid is g's.
            let tight = |f: &AxisFit| f.0.pitch.is_some_and(|p| (p - g).abs() * (f.0.count - 1) as f64 <= SAME_POSITION_MM);
            let fixed = grid_at(values, raster, g, Some(origin), true, ON_LATTICE_MM);
            grid_at(values, raster, g, Some(origin), false, ON_LATTICE_MM)
                .filter(tight)
                .filter(|r| fixed.as_ref().is_none_or(|f| r.1.iter().zip(&f.1).all(|(a, b)| a.is_none() || b.is_some())))
                .or(fixed)
        });
        if held {
            break;
        }
    }
    let fit = fit?;
    // A stage step is set in whole µm or 0.1 µm: snap the fitted pitch to the roundest such value
    // within three standard errors of it (at least 1e-7 of it, a float32 step multiplied up) when
    // that grid holds as many. Float32 noise would otherwise write 99.99995 µm; a 33.33 µm step
    // stays.
    let pitch = fit.0.pitch?;
    let se = standard_error(&fit, values).max(1e-7 * pitch);
    let snapped = [1e3, 1e4].into_iter().map(|u| (pitch * u).round() / u).find(|s| (s - pitch).abs() <= 3.0 * se);
    let fit = match snapped.and_then(|s| grid_at(values, raster, s, Some(fit.0.origin), true, ON_LATTICE_MM)) {
        Some(s) if off(&s.1) <= off(&fit.1) => s,
        // A pitch from a gap other than the run's median must be a round step: sub-µm scatter on a
        // few columns lets such a gap hold a pitch off the step.
        _ if from_other => return None,
        _ => fit,
    };
    (fit.0.pitch.is_some_and(|p| p >= min_step) && !seated_by_strays(&fit.1)).then_some(fit)
}

/// Whether a step-free lattice is a fraction 1/m of a coarser one seated by strays: the columns off
/// the coarser lattice each hold at most half the scans of its median column, and at least two of
/// its columns hold twice the scans of any of them (a raster column holds one scan per row; a
/// stray, one — and a row acquired twice is not a coarser lattice). Such a grid places every scan
/// right but states a pixel size the raster never had (review 2026-09-30, fifth pass).
///
/// ponytail: m up to 64; a single-row raster (one scan per column) cannot tell strays from columns,
/// and tiny plus-shaped cores (1, 3, 1 scans per column) are refused.
fn seated_by_strays(index: &[Option<i64>]) -> bool {
    let mut per = std::collections::BTreeMap::<i64, usize>::new();
    index.iter().flatten().for_each(|&i| *per.entry(i).or_default() += 1);
    (2..=64i64).any(|m| {
        let mut by = vec![0usize; m as usize];
        per.iter().for_each(|(i, n)| by[i.rem_euclid(m) as usize] += n);
        let r = (0..m).max_by_key(|&r| by[r as usize]).unwrap();
        let (mut coarse, fine): (Vec<_>, Vec<_>) = per.iter().partition(|(i, _)| i.rem_euclid(m) == r);
        coarse.sort_by_key(|(_, n)| **n);
        let median = *coarse[coarse.len() / 2].1;
        let most = fine.iter().map(|(_, n)| **n).max().unwrap_or(0);
        2 * most <= median && coarse.iter().filter(|(_, n)| **n >= 2 * most).count() >= 2
    })
}

/// Sorted values in runs within [`SAME_POSITION_MM`] of each run's first.
fn runs(mut sorted: &[f64]) -> Vec<&[f64]> {
    let mut runs = Vec::new();
    while let Some(&first) = sorted.first() {
        let (run, rest) = sorted.split_at(sorted.partition_point(|v| v - first < SAME_POSITION_MM));
        runs.push(run);
        sorted = rest;
    }
    runs
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
/// indices. A position is one sample however many rows repeat it: the rows of a set point are one
/// measurement of it, and counted each they left float32 pitches unsnapped (0.24999987 mm).
fn standard_error((axis, index): &AxisFit, values: &[f64]) -> f64 {
    let pitch = axis.pitch.unwrap_or(0.0);
    let mut on: Vec<(f64, f64)> =
        index.iter().zip(values).filter_map(|(i, v)| Some(((*i)? as f64, v - axis.origin - ((*i)? - 1) as f64 * pitch))).collect();
    on.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1)));
    on.dedup();
    let n = on.len() as f64;
    let mk = on.iter().map(|(k, _)| k).sum::<f64>() / n;
    let spread = on.iter().map(|(k, _)| (k - mk) * (k - mk)).sum::<f64>();
    let float32 = f32::EPSILON as f64 * values.iter().fold(0.0f64, |m, v| m.max(v.abs()));
    let scatter = (on.iter().map(|(_, r)| r * r).sum::<f64>() / (n - 2.0).max(1.0)).max(float32 * float32);
    (scatter / spread).sqrt()
}

/// The grid of step `c` (`fixed`: exactly `c`) through the `raster` positions, placing all the
/// `values`. Its phase is `phase`, or else the circular mean of the raster positions modulo `c`,
/// and the least-squares line through the positions it holds then refines pitch and origin (three
/// rounds, the held positions growing as the pitch improves). A stray moves the circular mean by
/// its share only — the walk over the gaps it replaced slipped a column wherever two strays split a
/// gap, and lost the grid to 0.2 % of strays inside the raster (review 2026-09-30 B14/B15). `None`
/// unless all but [`MAX_OFF_GRID`] of the `values` lie within `tol` (mm) of it; one outside the
/// raster is off it.
fn grid_at(values: &[f64], raster: &[f64], c: f64, phase: Option<f64>, fixed: bool, tol: f64) -> Option<AxisFit> {
    use std::f64::consts::TAU;
    if !(c > 0.0) {
        return None;
    }
    let origin = phase.unwrap_or_else(|| {
        let (sin, cos) = raster.iter().fold((0.0, 0.0), |(s, co), v| {
            let a = TAU * (v / c).rem_euclid(1.0);
            (s + a.sin(), co + a.cos())
        });
        c * sin.atan2(cos) / TAU
    });
    let (mut pitch, mut origin) = (c, origin);
    let place = |v: f64, pitch: f64, origin: f64| {
        let k = ((v - origin) / pitch).round();
        (k, (v - origin - k * pitch).abs())
    };
    for _ in 0..3 {
        let held: Vec<(f64, f64)> =
            raster.iter().map(|&v| (place(v, pitch, origin), v)).filter(|((_, r), _)| *r <= tol).map(|((k, _), v)| (k, v)).collect();
        (pitch, origin) = line(&held, fixed.then_some(c))?;
    }
    let inside = raster[0]..=raster[raster.len() - 1];
    let placed: Vec<Option<i64>> = values
        .iter()
        .map(|&v| Some(place(v, pitch, origin)).filter(|(_, r)| *r <= tol && inside.contains(&v)).map(|(k, _)| k as i64))
        .collect();
    if placed.iter().filter(|k| k.is_none()).count() as f64 > MAX_OFF_GRID * values.len() as f64 {
        return None;
    }
    let (lo, hi) = placed.iter().flatten().fold((i64::MAX, i64::MIN), |(lo, hi), k| (lo.min(*k), hi.max(*k)));
    let max_residual = placed.iter().zip(values).filter_map(|(k, v)| Some((v - origin - (*k)? as f64 * pitch).abs())).fold(0.0, f64::max);
    let axis = GridAxis { origin: origin + lo as f64 * pitch, pitch: Some(pitch), count: hi - lo + 1, max_residual, declared: false };
    Some((axis, placed.iter().map(|k| k.map(|k| k - lo + 1)).collect()))
}

/// What a lone `IMS:1000046` in a grid entry says about y, for [`marker_block`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LoneX {
    /// The source stated the term and the single-value rule read its scan settings (imzML): the
    /// IMS vocabulary defines `IMS:1000046` as "the length of a pixel in the x dimension. If no
    /// pixel size y (IMS:1000047) is explicitly specified, then this also describes the length of
    /// a pixel in the y dimension". The rule writes that y itself ([`apply`]), so this default
    /// only says the same of an entry the rule left as it was.
    AlsoY,
    /// x only, no `pixel_size_um`. The lane wrote the term itself for an axis whose step it
    /// measured and knows no other axis's (a Waters single row: owner decision D5 keeps that
    /// output as it is); or the source stated it and nothing tested it (an mzML with positions:
    /// its lone "pixel size" may be the area the term named until 2017, and an untested value in
    /// the marker would be read as a length on both axes).
    XOnly,
}

/// A length param in micrometres, from any length unit mzdata can state; a size that is not
/// positive, or in a unit that is no length, is no size.
fn um_of(p: &Param) -> Option<f64> {
    let v = p.value.to_f64().ok()?;
    let um = match p.unit {
        Unit::Micrometer => v,
        Unit::Nanometer => v / 1e3,
        Unit::Millimeter => v * 1e3,
        Unit::Centimeter => v * 1e4,
        _ => return None,
    };
    (um.is_finite() && um > 0.0).then_some(um)
}

/// The imaging profile's `metadata.imaging` index block (HUPO-PSI/mzPeak-specification#24): the
/// marker, the coordinate base, the grid as the viewer reads it and where its counts came from
/// (`pixel_count_source`: [`COUNTS_DECLARED`] or [`COUNTS_OBSERVED_MAX`]; review 2026-09-30 B18),
/// and where it all came from. `pixel_size_um` needs both axes as a positive length — in
/// micrometres, converted from nanometres, millimetres or centimetres where the entry states one of
/// those (a size in mm used to give the marker none; the scan settings keep the unit as stated);
/// `lone_x` says whether an `IMS:1000046` without an `IMS:1000047` gives both
/// (HUPO-PSI/mzPeak-specification#23: a reader that does not know the vocabulary's default saw no
/// pixel size at all). `pixel_size_source` is the lane's word on how that size was settled (the
/// [`SOURCE_DECLARED`] family); the block states it as [`SOURCE_UNKNOWN`] whenever it writes no
/// `pixel_size_um`, whatever the lane says.
pub fn marker_block(grid: Option<&ScanSettings>, pixel_count_source: &str, lone_x: LoneX, pixel_size_source: &str, provenance: serde_json::Value) -> serde_json::Value {
    let mut b = serde_json::json!({"is_imaging": true, "coordinate_base": 1, "provenance": provenance});
    let param = |acc| grid?.params.iter().find(|p| p.curie() == Some(acc));
    let int = |acc| param(acc)?.value.to_i64().ok();
    let um = |acc| um_of(param(acc)?);
    if let (Some(x), Some(y)) = (int(mzdata::curie!(IMS:1000042)), int(mzdata::curie!(IMS:1000043))) {
        b["pixel_count"] = serde_json::json!({"x": x, "y": y});
        b["pixel_count_source"] = pixel_count_source.into();
    }
    let x = um(mzdata::curie!(IMS:1000046));
    let y = match param(mzdata::curie!(IMS:1000047)) {
        Some(_) => um(mzdata::curie!(IMS:1000047)),
        None => x.filter(|_| lone_x == LoneX::AlsoY),
    };
    if let (Some(x), Some(y)) = (x, y) {
        b["pixel_size_um"] = serde_json::json!({"x": x, "y": y});
        b["pixel_size_source"] = pixel_size_source.into();
    } else {
        b["pixel_size_source"] = SOURCE_UNKNOWN.into();
    }
    b
}

/// `--pixel-size X[,Y]`: the pixel size the user supplies, in µm, for a source that settles none.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UserPixelSize {
    pub x: f64,
    pub y: f64,
    /// `--force`: a stated size that differs is written over instead of refused.
    pub force: bool,
}

impl UserPixelSize {
    /// `X` or `X,Y` in µm, each a positive finite number.
    pub fn parse(text: &str, force: bool) -> Result<Self> {
        let num = |s: &str| -> Result<f64> {
            let v: f64 = s.trim().parse().map_err(|_| anyhow::anyhow!("--pixel-size {text:?}: {:?} is not a number", s.trim()))?;
            if !(v.is_finite() && v > 0.0) {
                bail!("--pixel-size {text:?}: a pixel size is a positive length in micrometres, not {}", s.trim());
            }
            Ok(v)
        };
        let (x, y) = match text.split(',').collect::<Vec<_>>().as_slice() {
            [x] => (num(x)?, num(x)?),
            [x, y] => (num(x)?, num(y)?),
            _ => bail!("--pixel-size expects X or X,Y in micrometres (10, or 10,20); got {text:?}"),
        };
        Ok(Self { x, y, force })
    }

    /// `10 µm` or `10 × 20 µm`.
    pub fn text(&self) -> String {
        if self.x == self.y { format!("{} µm", self.x) } else { format!("{} × {} µm", self.x, self.y) }
    }
}

/// The `--pixel-size` of this run, for the lanes. Set once by `run` before any lane opens its
/// input; the lanes read it where they build their grid entry (the imzML/mzML lane after its own
/// rule and the pixel counts, the Bruker MALDI reader, the Waters grid fit). A process-wide value,
/// as `MZPC_KEEP_ZERO_RUNS` and the writer's other environment levers are: threading one option
/// through the eleven lane signatures and their callers would touch every lane for a value only the
/// imaging grid reads. Unit tests pass a [`UserPixelSize`] to the functions below instead of setting
/// this: the test binary's threads share it.
static USER_PIXEL_SIZE: std::sync::RwLock<Option<UserPixelSize>> = std::sync::RwLock::new(None);

pub fn set_user_pixel_size(user: Option<UserPixelSize>) {
    *USER_PIXEL_SIZE.write().unwrap_or_else(|e| e.into_inner()) = user;
}

pub fn user_pixel_size() -> Option<UserPixelSize> {
    *USER_PIXEL_SIZE.read().unwrap_or_else(|e| e.into_inner())
}

const PIXEL_SIZE_TERMS: [(CURIE, &str); 2] = [(mzdata::curie!(IMS:1000046), "pixel size (x)"), (mzdata::curie!(IMS:1000047), "pixel size y")];
const MAX_DIMENSION_TERMS: [(CURIE, &str); 2] = [(mzdata::curie!(IMS:1000044), "max dimension x"), (mzdata::curie!(IMS:1000045), "max dimension y")];

/// Where a grid entry's stated pixel size (`None`: not stated on either axis) differs from the
/// supplied one: one line per axis, naming the stated value. A stated axis whose value is no length
/// in micrometres (zero, a unit that is no length) differs too: the source settled something the
/// supplied size would overwrite.
pub fn user_pixel_size_differences(grid: Option<&ScanSettings>, user: &UserPixelSize) -> Vec<String> {
    let Some(grid) = grid else { return Vec::new() };
    PIXEL_SIZE_TERMS
        .iter()
        .zip([user.x, user.y])
        .filter_map(|((curie, name), want)| {
            let p = grid.params.iter().find(|p| p.curie() == Some(*curie))?;
            match um_of(p) {
                Some(v) if approx(v, want) => None,
                Some(v) => Some(format!("{name} ({curie}) = {v} µm")),
                None => Some(format!("{name} ({curie}) = {} {}", p.value, p.unit.to_curie().map(|c| c.to_string()).unwrap_or_else(|| "(no unit)".into()))),
            }
        })
        .collect()
}

/// Refuse a `--pixel-size` that contradicts a size `grid` states, unless `--force` (owner principle
/// P3: the supplied size could be the wrong data). `what` names the source for the message.
pub fn check_user_pixel_size(grid: Option<&ScanSettings>, user: &UserPixelSize, what: &str) -> Result<()> {
    let differing = user_pixel_size_differences(grid, user);
    if differing.is_empty() || user.force {
        return Ok(());
    }
    bail!(
        "--pixel-size {}: {what} states a pixel size that differs ({}); the supplied size is written \
         only where the source settles none. Check which is right; --force writes the supplied size \
         over the stated one, declared as {USER_SUPPLIED} and recorded in imaging_pixel_size",
        user.text(),
        differing.join(", ")
    );
}

/// The `--pixel-size` rule on a grid entry, the same on every lane (the imzML/mzML lane after its
/// own rule and the pixel counts, the Bruker and Waters lanes on the grid they build, the archive
/// rewrite on the archive's list). `IMS:1000046/47` are written in µm where the entry states
/// neither — a stated axis that agrees with the supplied value stays as it is and the other is
/// filled (a Waters single row states the x step alone) — and `IMS:1000044/45` from the pixel counts
/// where the entry states those and no max dimension. A stated size that differs is refused
/// ([`check_user_pixel_size`]) unless `--force`: then the supplied size is written over it, the
/// stated values are recorded, and a stated max dimension is recomputed from the counts. `None`
/// when the entry already states the supplied size on both axes; else the `imaging_pixel_size` row
/// to list, whose transformation is [`USER_SUPPLIED`].
pub fn apply_user_pixel_size(grid: &mut ScanSettings, user: &UserPixelSize, what: &str) -> Result<Option<serde_json::Value>> {
    check_user_pixel_size(Some(grid), user, what)?;
    let at = |grid: &ScanSettings, curie: CURIE| grid.params.iter().position(|p| p.curie() == Some(curie));
    let stated: Vec<serde_json::Value> = PIXEL_SIZE_TERMS
        .iter()
        .filter_map(|(curie, _)| {
            let p = &grid.params[at(grid, *curie)?];
            Some(serde_json::json!({"accession": curie.to_string(), "value": p.value.to_string(), "unit": p.unit.to_curie().map(|c| c.to_string())}))
        })
        .collect();
    let overridden = !user_pixel_size_differences(Some(grid), user).is_empty();
    if stated.len() == 2 && !overridden {
        log::info!("--pixel-size {}: {what} states the same size; nothing to write", user.text());
        return Ok(None);
    }
    let want = [user.x, user.y];
    let mut written = Vec::new();
    for ((curie, name), v) in PIXEL_SIZE_TERMS.iter().zip(want) {
        match at(grid, *curie) {
            Some(_) if !overridden => continue, // stated and agreeing: as it is
            Some(i) => {
                let p = &mut grid.params[i];
                p.name = (*name).into();
                p.value = v.into();
                p.unit = Unit::Micrometer;
            }
            None => grid.params.push(Param::builder().name(*name).curie(*curie).value(v).unit(Unit::Micrometer).build()),
        }
        written.push(serde_json::json!({"accession": curie.to_string(), "value": v, "unit": "UO:0000017", "unit_assumed": false}));
    }
    // The max dimension: count × size on each axis whose count the entry states, where it states
    // none — or where the stated one belonged to the size written over.
    let mut max_dimension = Vec::new();
    for ((count, (curie, name)), v) in COUNTS.iter().zip(MAX_DIMENSION_TERMS).zip(want) {
        let Some(n) = at(grid, *count).and_then(|i| grid.params[i].value.to_f64().ok()).filter(|n| *n > 0.0) else { continue };
        match at(grid, curie) {
            Some(i) if overridden => {
                grid.params[i].value = (n * v).into();
                grid.params[i].unit = Unit::Micrometer;
                max_dimension.push(format!("{curie} recomputed as {n} × {v} µm"));
            }
            Some(_) => {}
            None => {
                grid.params.push(Param::builder().name(name).curie(curie).value(n * v).unit(Unit::Micrometer).build());
                max_dimension.push(format!("{curie} written as {n} × {v} µm"));
            }
        }
    }
    let detail = match (stated.is_empty(), overridden) {
        (true, _) => format!("{what} states no pixel size; {} written as supplied", user.text()),
        (false, true) => format!("{what} states a different pixel size; {} written over it (--force)", user.text()),
        (false, false) => format!("{what} states one axis, which agrees; the other written as supplied ({})", user.text()),
    };
    log::warn!("--pixel-size: {detail}{}", if max_dimension.is_empty() { String::new() } else { format!("; {}", max_dimension.join(", ")) });
    Ok(Some(serde_json::json!({
        "scan_settings": grid.id,
        "case": "user supplied (--pixel-size)",
        "transformation": USER_SUPPLIED,
        "transformations": [USER_SUPPLIED],
        "written_um": written,
        "stated": stated,
        "overridden": overridden,
        "max_dimension": max_dimension,
        "detail": detail,
    })))
}

/// `provenance.time` of an imaging marker whose source states no `scan start time` on any spectrum:
/// the archive stores time 0 for each (a null time is a question for the core spec).
pub const TIME_NOT_STATED: &str = "not stated by the source; index is the source list order";

/// Whether an archive's imaging marker (`metadata.imaging`) says its source stated no scan start
/// time: every stored time is then the 0 the reader fills in, and an mzML export states none.
pub fn states_no_time(marker: &serde_json::Value) -> bool {
    marker["provenance"]["time"] == TIME_NOT_STATED
}

/// Whether a grid entry states a pixel size on either axis (`provenance.pixel_size`: "as stated"
/// only when there is one — it used to say so of a header that states none).
pub fn states_pixel_size(grid: Option<&ScanSettings>) -> bool {
    grid.is_some_and(|g| states(g, &[mzdata::curie!(IMS:1000046), mzdata::curie!(IMS:1000047)]))
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

/// The canonical name of a length unit accession, for the accession/name check, and its size in µm,
/// for the pixel-size test.
fn length_unit(accession: &str) -> Option<(&'static str, f64)> {
    Some(match accession {
        "UO:0000008" => ("meter", 1e6),
        "UO:0000015" => ("centimeter", 1e4),
        "UO:0000016" => ("millimeter", 1e3),
        "UO:0000017" => ("micrometer", 1.0),
        "UO:0000018" => ("nanometer", 1e-3),
        _ => return None,
    })
}

fn normalized_unit_name(name: &str) -> String {
    name.trim().to_lowercase().replace("metre", "meter").replace("µm", "micrometer").replace("μm", "micrometer")
}

/// The unit a param is written in: its unit NAME's when mzdata knows the name — mzdata lets a known
/// name override the accession, whatever the attribute order — else its unit accession as stated.
/// The pixel-size rule tests and keeps this unit, so a tested value is written in the unit it
/// passed in (review 2026-09-30: tested by accession, a centimetre accession named "micrometer"
/// passed as 0.01 cm and was written as 0.01 µm).
///
/// ponytail: an accession mzdata does not know (metre, UO:0000008) counts as stated, though mzdata
/// writes that param without a unit; no corpus imzML states one.
fn written_unit(q: &RawParam) -> Option<String> {
    let by_name = q.unit_name.as_deref().and_then(|n| Unit::from_name(n).to_curie());
    by_name.map(|c| c.to_string()).or_else(|| q.unit_accession.clone())
}

/// One way of reading a param's unit as a length: the unit's canonical name, its size in µm,
/// whether it is the unit ACCESSION's reading where the name says otherwise, and whether it is
/// micrometre assumed for a param with no known length unit.
#[derive(Debug, Clone, Copy)]
struct Reading {
    unit: &'static str,
    size: f64,
    by_accession: bool,
    assumed: bool,
}

/// The length readings of a param's unit, in the order the rule tries them: the unit it is written
/// in ([`written_unit`]) — micrometre, noted as assumed, when that is no known length — and then,
/// where the unit accession names a known length the name contradicts, the accession's (owner
/// decision D4: 31 of 479 public imzML files state `UO:0000015`, centimetre, named "micrometer").
fn readings(q: &RawParam) -> Vec<Reading> {
    let written = written_unit(q).as_deref().and_then(length_unit);
    let mut out = vec![match written {
        Some((unit, size)) => Reading { unit, size, by_accession: false, assumed: false },
        None => Reading { unit: "micrometer", size: 1.0, by_accession: false, assumed: true },
    }];
    if let Some((unit, size)) = q.unit_accession.as_deref().and_then(length_unit) {
        if written.is_none_or(|(_, s)| s != size) {
            out.push(Reading { unit, size, by_accession: true, assumed: false });
        }
    }
    out
}

/// What the pixel-size rule did to one `<scanSettings>`.
#[derive(Debug, Clone, PartialEq)]
pub struct PixelSizeFix {
    pub settings_id: String,
    /// Which case of the rule applied.
    pub case: &'static str,
    /// The `transformations` entries it declares, when a value or a unit changed (empty: nothing
    /// changed). More than one where an area's root was also read by its unit accession, or x took
    /// micrometre as assumed while y was read by its accession.
    pub transformations: Vec<&'static str>,
    /// Pixel sizes to write: accession (`IMS:1000046`/`47`), value, and whether its unit is set to
    /// micrometre (assumed for a param without one, or converted from the unit accession's reading,
    /// `by_accession`). An accession absent here is removed. A lone `IMS:1000046` the single-value
    /// rule keeps is here twice, as itself and as `IMS:1000047` — the vocabulary's default made
    /// explicit, which declares nothing.
    pub write: Vec<(&'static str, f64, bool)>,
    /// The `write` accessions whose value is the unit ACCESSION's reading, converted to micrometres
    /// ([`UNIT_FROM_ACCESSION`]); a lone x read that way lists its y copy too.
    pub by_accession: Vec<&'static str>,
    /// The single-value rule kept the value: it is written under the vocabulary's names (the old
    /// "pixel size" named the AREA term), and a lone x also as y ([`apply`]).
    pub single: bool,
    /// The unit accession each `write` value is in: micrometre where set, else [`written_unit`]'s
    /// (an area's root keeps a stated length unit, so the values are not all µm) — until
    /// [`check_written_units`] replaces it by the unit the writer's param actually carries.
    pub write_units: Vec<Option<String>>,
    /// Unit accession/name disagreements seen on the pixel-size and extent params.
    pub unit_mismatches: Vec<String>,
    /// `(param accession, stated unit accession)` of those params, to compare with what was written.
    pub mismatched: Vec<(String, String)>,
    /// What [`check_written_units`] found: the unit each mismatched param was written with.
    pub written_units: Vec<String>,
    /// x and y both stated: the axes on which `value × count` is not the stated max dimension.
    /// Reported only — which of the three the file has wrong is not decided, so nothing is changed.
    pub extent_mismatches: Vec<String>,
    pub detail: String,
}

const PIXEL_X: &str = "IMS:1000046";
const PIXEL_Y: &str = "IMS:1000047";
/// The vocabulary's names of the two (imagingMS.obo 1.1.0). Until 2017 `IMS:1000046` was "pixel
/// size", an area.
const PIXEL_NAMES: [(&str, &str); 2] = [(PIXEL_X, "pixel size (x)"), (PIXEL_Y, "pixel size y")];
/// `(pixel count, max dimension)` per axis, x then y.
const AXES: [(&str, &str); 2] = [("IMS:1000042", "IMS:1000044"), ("IMS:1000043", "IMS:1000045")];

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
            let (canonical, _) = length_unit(p.unit_accession.as_deref()?)?;
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
            let (canonical, _) = length_unit(ua)?;
            (normalized_unit_name(p.unit_name.as_deref()?) != canonical).then(|| (p.accession.clone(), ua.to_string()))
        })
        .collect();
    let (x, y) = (get(PIXEL_X), get(PIXEL_Y));
    let fix = |case, transformations: Vec<&'static str>, write: Vec<(&'static str, f64, bool)>, detail: String| PixelSizeFix {
        settings_id: s.id.clone(),
        case,
        transformations,
        // A y written from a lone x (`get` finds none) carries the x param's unit.
        write_units: write
            .iter()
            .map(|&(a, _, um)| if um { Some("UO:0000017".into()) } else { get(a).or(x).and_then(written_unit) })
            .collect(),
        write,
        by_accession: Vec::new(),
        single: false,
        unit_mismatches: unit_mismatches.clone(),
        mismatched: mismatched.clone(),
        written_units: Vec::new(),
        extent_mismatches: Vec::new(),
        detail,
    };
    // The axis to test a value against: its count and max dimension where the file states both.
    let testable = |(c, e): (&str, &str)| {
        let (count, extent) = (num(c)?, num(e)?);
        (count > 0.0 && extent > 0.0).then_some((count, extent, get(e)?))
    };
    match (x, y) {
        (None, None) => (!unit_mismatches.is_empty())
            .then(|| fix("no pixel size stated", vec![], vec![], String::new())),
        (Some(px), Some(py)) => {
            let (Some(vx), Some(vy)) = (num(PIXEL_X), num(PIXEL_Y)) else {
                return Some(fix("x and y not numeric", vec![DROPPED], vec![], format!("x={:?} y={:?}", px.value, py.value)));
            };
            // A pixel has a size: zero or a negative number is none (a lone value was always held to
            // this; x = 0, y = −100 µm were written as stated, into the marker too).
            if vx <= 0.0 || vy <= 0.0 {
                return Some(fix("x and y not both positive", vec![DROPPED], vec![], format!("x={vx} y={vy}")));
            }
            // Each axis against its own count and max dimension, where the file states both
            // (HUPO-PSI/mzPeak-specification#23: 50 and 2500 over 150 × 100 µm were written as
            // stated with no word), in the unit it is written in; a stated unit that is no length
            // is not compared. An axis whose unit accession contradicts its unit name (D4) is read
            // both ways: the name's reading first, then the accession's, and the one that passes is
            // written — converted to micrometres when it is the accession's; neither passing, or
            // nothing to test against, drops both sizes (the value could be the wrong data).
            let mut write = Vec::new();
            let mut by_accession = Vec::new();
            let mut extent_mismatches = Vec::new();
            let mut details = Vec::new();
            let mut dropped: Option<(&'static str, String)> = None;
            for ((p, v, acc), axis) in [(px, vx, PIXEL_X), (py, vy, PIXEL_Y)].into_iter().zip(AXES) {
                let tries = readings(p);
                let as_stated = (acc, v, written_unit(p).is_none());
                let Some((count, extent, e)) = testable(axis) else {
                    if tries.len() > 1 {
                        dropped = Some(("x and y: a unit contradiction nothing tests", format!("{acc}={v}: unit {} named {:?}, and no pixel count and max dimension on the axis to tell which; both sizes dropped", tries[1].unit, p.unit_name.as_deref().unwrap_or(""))));
                    }
                    write.push(as_stated);
                    continue;
                };
                let (eu, e_size) = readings(e)[0].unit_size();
                let extent_um = extent * e_size;
                let passes = |r: &Reading| approx(v * r.size * count, extent_um);
                // A stated unit that is no length is not compared (the name's reading is then
                // micrometre assumed, which the two-value case does not write over a stated unit).
                let comparable = written_unit(p).is_none_or(|u| length_unit(&u).is_some());
                if !comparable || passes(&tries[0]) {
                    write.push(as_stated);
                } else if let Some(r) = tries.get(1).filter(|r| passes(r)) {
                    write.push((acc, v * r.size, true));
                    by_accession.push(acc);
                    details.push(format!("{acc}={v} {} by its unit accession (named {:?}): {v} {} × {count} = {extent} {eu}; written as {} micrometer", r.unit, p.unit_name.as_deref().unwrap_or(""), r.unit, v * r.size));
                } else if tries.len() > 1 {
                    dropped = Some(("x and y: a unit contradiction that passes under neither reading", format!("{acc}={v}: neither {v} {} (the unit name) nor {v} {} (the unit accession) × {count} is the max dimension {extent} {eu}; both sizes dropped", tries[0].unit, tries[1].unit)));
                    write.push(as_stated);
                } else {
                    extent_mismatches.push(format!("{acc}={v} {} × {count} pixels is not the max dimension {}={extent} {eu}", tries[0].unit, e.accession));
                    write.push(as_stated);
                }
            }
            if let Some((case, detail)) = dropped {
                return Some(fix(case, vec![DROPPED], vec![], detail));
            }
            // Micrometre set on an axis that states no unit (an axis read by its accession has one).
            let assumed = write.iter().any(|w| w.2 && !by_accession.contains(&w.0));
            let mut transformations = Vec::new();
            if assumed {
                transformations.push(UNIT_ASSUMED);
            }
            if !by_accession.is_empty() {
                transformations.push(UNIT_FROM_ACCESSION);
            }
            let case = match (assumed, by_accession.is_empty()) {
                (false, true) => "x and y with a unit",
                (true, true) => "x and y without a unit: micrometre assumed",
                (false, false) => "x and y with a unit; one read by its unit accession",
                (true, false) => "x and y without a unit: micrometre assumed; one read by its unit accession",
            };
            let detail = if transformations.is_empty() { String::new() } else { std::iter::once(format!("x={vx} y={vy}")).chain(details).collect::<Vec<_>>().join("; ") };
            let f = fix(case, transformations, write, detail);
            let f = PixelSizeFix { extent_mismatches, by_accession, ..f };
            Some(f).filter(|f| !f.transformations.is_empty() || !f.unit_mismatches.is_empty() || !f.extent_mismatches.is_empty())
        }
        (Some(p), None) | (None, Some(p)) => {
            let acc: &'static str = if p.accession == PIXEL_X { PIXEL_X } else { PIXEL_Y };
            let Some(v) = num(acc).filter(|v| *v > 0.0) else {
                return Some(fix("one value, not numeric", vec![DROPPED], vec![], format!("{acc}={:?}", p.value)));
            };
            // Tested against its own axis's count and extent; the other axis's only when its own
            // states none — pixels are square in every surveyed file that states both (review
            // 2026-09-30 B17: x was tried first whatever the axis).
            let axes = if acc == PIXEL_X { AXES } else { [AXES[1], AXES[0]] };
            let tested = axes.into_iter().find_map(testable);
            let Some((count, extent, e)) = tested else {
                // Nothing was tested: its own case, and the detail names what the header lacks (it
                // said "no pixel count and max dimension" of a header stating both counts).
                let counted = AXES.iter().any(|a| num(a.0).is_some_and(|v| v > 0.0));
                let sized = AXES.iter().any(|a| num(a.1).is_some_and(|v| v > 0.0));
                let missing = match (counted, sized) {
                    (true, false) => "no max dimension (IMS:1000044/45)",
                    (false, true) => "no pixel count (IMS:1000042/43)",
                    (false, false) => "no pixel count (IMS:1000042/43) and no max dimension (IMS:1000044/45)",
                    (true, true) => "no axis with both a pixel count and a max dimension",
                };
                return Some(fix("one value, untestable", vec![DROPPED], vec![], format!("{acc}={v}; {missing} to test it against")));
            };
            // Value and extent compared in µm, each in the unit it is written in (B17: the units were
            // ignored); a param without a known length unit is tested as µm, and the detail says so.
            // A value whose unit accession contradicts its unit name is read both ways (D4): the
            // name's reading first, then the accession's.
            let tries = readings(p);
            let er = readings(e)[0];
            let (eu, e_size) = er.unit_size();
            let extent_um = extent * e_size;
            let assumed: Vec<&str> = [(p, tries[0].assumed), (e, er.assumed)].into_iter().filter(|(_, a)| *a).map(|(q, _)| q.accession.as_str()).collect();
            let note = if assumed.is_empty() { String::new() } else { format!(" ({} without a length unit: micrometre assumed)", assumed.join(", ")) };
            // What a kept value is written as: itself, and — a lone x — the same value as y. The
            // vocabulary gives `IMS:1000046` that meaning when no `IMS:1000047` is stated, so the
            // y states nothing new and declares nothing; a lone y says nothing about x.
            let kept = |v: f64, um: bool| if acc == PIXEL_X { vec![(PIXEL_X, v, um), (PIXEL_Y, v, um)] } else { vec![(PIXEL_Y, v, um)] };
            let also_y = if acc == PIXEL_X { "; also written as IMS:1000047, which the vocabulary's IMS:1000046 gives when none is stated" } else { "" };
            // `(reading, as an area)` of the first reading that passes.
            let passed = tries.iter().find_map(|r| {
                if approx(v.sqrt() * r.size * count, extent_um) {
                    Some((*r, true))
                } else if approx(v * r.size * count, extent_um) {
                    Some((*r, false))
                } else {
                    None
                }
            });
            let Some((r, area)) = passed else {
                let both = if tries.len() > 1 { format!("; neither as {} (the unit name) nor as {} (the unit accession)", tries[0].unit, tries[1].unit) } else { String::new() };
                return Some(fix(
                    "one value that tests as neither area nor length",
                    vec![DROPPED],
                    vec![],
                    format!("{acc}={v} {}; count {count}, max dimension {extent} {eu}{note}{both}", tries[0].unit),
                ));
            };
            let vu = r.unit;
            let f = if r.by_accession {
                // The accession's reading passed where the name's did not: written in micrometres,
                // the unit the test was made in, as a length (an area's root).
                let length = if area { v.sqrt() } else { v } * r.size;
                let transformations = if area { vec![AREA_TO_LENGTH, UNIT_FROM_ACCESSION] } else { vec![UNIT_FROM_ACCESSION] };
                let case = if area { "one value: an area (√value × count = extent), by its unit accession" } else { "one value: a length (value × count = extent), by its unit accession" };
                let equation = if area { format!("√({v} {vu}²) × {count} = {extent} {eu}") } else { format!("{v} {vu} × {count} = {extent} {eu}") };
                let f = fix(
                    case,
                    transformations,
                    kept(length, true),
                    format!("{acc}={v} read by its unit accession, {vu} (named {:?}, which does not fit the extent): {equation}; written as {length} micrometer{note}{also_y}", p.unit_name.as_deref().unwrap_or("")),
                );
                PixelSizeFix { by_accession: f.write.iter().map(|w| w.0).collect(), ..f }
            } else if area {
                fix(
                    "one value: an area (√value × count = extent)",
                    vec![AREA_TO_LENGTH],
                    // The square root of an area is a length in the unit the area is the square
                    // of (µm² → µm, mm² → mm): a stated length unit stays, anything else was
                    // tested as µm² and becomes µm.
                    kept(v.sqrt(), r.assumed),
                    format!("{acc}={v} as area; √({v} {vu}²) × {count} = {extent} {eu}{note}{also_y}"),
                )
            } else {
                // A stated unit stays even when it is no length, as in the two-value case:
                // micrometre is written only where none is stated.
                let unit_assumed = written_unit(p).is_none();
                fix(
                    "one value: a length (value × count = extent)",
                    if unit_assumed { vec![UNIT_ASSUMED] } else { vec![] },
                    kept(v, unit_assumed),
                    format!("{acc}={v}; {v} {vu} × {count} = {extent} {eu}{note}{also_y}"),
                )
            };
            Some(PixelSizeFix { single: true, ..f })
        }
    }
}

impl Reading {
    /// `(unit name, size in µm)`.
    fn unit_size(&self) -> (&'static str, f64) {
        (self.unit, self.size)
    }
}

impl PixelSizeFix {
    /// `metadata.imaging.pixel_size_source` for the grid this fix describes: how the size it keeps
    /// was settled, or [`SOURCE_UNKNOWN`] when it keeps none.
    pub fn source(&self) -> &'static str {
        if self.write.is_empty() {
            SOURCE_UNKNOWN
        } else if self.transformations.contains(&AREA_TO_LENGTH) {
            SOURCE_FROM_AREA
        } else if self.transformations.contains(&UNIT_ASSUMED) {
            SOURCE_UNIT_ASSUMED
        } else {
            SOURCE_DECLARED
        }
    }
}

/// All fixes for a header.
pub fn pixel_size_fixes(settings: &[RawSettings]) -> Vec<PixelSizeFix> {
    settings.iter().filter_map(pixel_size_fix).collect()
}

/// Apply a fix to the writer's copy of the same `<scanSettings>`: the pixel-size params become
/// exactly `fix.write` (micrometre where the unit was assumed). A value the single-value rule kept
/// gets the vocabulary's name, and a lone `IMS:1000046` is followed by an `IMS:1000047` of the same
/// value and unit — also where the value itself is kept as stated (a length with its unit).
pub fn apply(fix: &PixelSizeFix, settings: &mut ScanSettings) {
    let at = |settings: &ScanSettings, acc: &str| settings.params.iter().position(|p| p.curie().is_some_and(|c| c.to_string() == acc));
    if !fix.transformations.is_empty() {
        for acc in [PIXEL_X, PIXEL_Y] {
            let wanted = fix.write.iter().find(|(a, _, _)| *a == acc);
            match (at(settings, acc), wanted) {
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
    if !fix.single {
        return;
    }
    for (acc, name) in PIXEL_NAMES {
        if let Some(i) = at(settings, acc) {
            settings.params[i].name = name.into();
        }
    }
    let y_wanted = fix.write.iter().any(|(a, _, _)| *a == PIXEL_Y);
    if let (Some(i), None, true) = (at(settings, PIXEL_X), at(settings, PIXEL_Y), y_wanted) {
        let y = Param { name: PIXEL_NAMES[1].1.into(), accession: Some(1000047), ..settings.params[i].clone() };
        settings.params.insert(i + 1, y);
    }
}

/// After [`apply`]: the unit each mismatched param was actually written with, and each `write`
/// value's unit as written (the index row must not contradict the archive). `true` when a
/// mismatched param's differs from the accession the file states — mzdata resolved the pair by the
/// name.
pub fn check_written_units(fix: &mut PixelSizeFix, settings: &ScanSettings) -> bool {
    let param = |acc: &str| settings.params.iter().find(|p| p.curie().is_some_and(|c| c.to_string() == acc));
    let mut replaced = false;
    for (acc, stated) in &fix.mismatched {
        let Some(p) = param(acc) else { continue };
        let written = p.unit.to_curie().map(|c| c.to_string()).unwrap_or_else(|| "none".into());
        replaced |= written != *stated;
        fix.written_units.push(format!("{acc}: stated {stated}, written {written}"));
    }
    for ((acc, _, _), unit) in fix.write.iter().zip(fix.write_units.iter_mut()) {
        if let Some(p) = param(acc) {
            *unit = p.unit.to_curie().map(|c| c.to_string());
        }
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
        // The first entry under the key readers of 0.17.0-rc archives know; all of them under the next.
        "transformation": f.transformations.first(),
        "transformations": f.transformations,
        // The key predates `unit` (review 2026-09-30: an mm² area's root is written in mm).
        "written_um": f
            .write
            .iter()
            .zip(&f.write_units)
            .map(|((a, v, um), unit)| {
                let by_accession = f.by_accession.contains(a);
                serde_json::json!({"accession": a, "value": v, "unit": unit, "unit_assumed": *um && !by_accession, "unit_from_accession": by_accession})
            })
            .collect::<Vec<_>>(),
        "unit_mismatches": f.unit_mismatches,
        "written_units": f.written_units,
        "extent_mismatches": f.extent_mismatches,
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

/// The imaging vocabulary's obsolete binary data type terms and the PSI-MS terms they were
/// obsoleted for (imagingMS.obo: "32-bit integer" / "64-bit integer", the same names in PSI-MS;
/// the obo's comment on `IMS:1000142` names `MS:1000520`, which is PSI-MS's "16-bit float" —
/// its "64-bit integer" is `MS:1000522`).
const OBSOLETE_INTEGER_TERMS: [(&str, &str); 2] = [("IMS:1000141", "MS:1000519"), ("IMS:1000142", "MS:1000522")];

/// The accessions [`replace_obsolete_integer_terms`] looks for, for [`file_mentions`].
pub const OBSOLETE_INTEGER_ACCESSIONS: [&str; 2] = [OBSOLETE_INTEGER_TERMS[0].0, OBSOLETE_INTEGER_TERMS[1].0];

/// An imzML's text with every cvParam naming an obsolete integer type term (`accession="IMS:1000141"`
/// or `…142`) naming the PSI-MS term instead, `cvRef` included; `None` when there is none. mzdata
/// maps only the PSI-MS terms to a data type, so such an array stayed untyped and the writer panicked
/// on it ("not implemented: intensity_unknown_dc", through 0.16.0). Each tag keeps its byte length —
/// the two characters saved become spaces between its attributes — so no offset into the text moves.
pub fn replace_obsolete_integer_terms(text: &[u8]) -> Option<Vec<u8>> {
    let mut out: Option<Vec<u8>> = None;
    let mut at = 0;
    while let Some(i) = find(&text[at..], b"IMS:100014").map(|i| at + i) {
        // The tag around the mention: from its `<` to its `>`.
        let start = text[..i].iter().rposition(|&b| b == b'<').unwrap_or(i);
        let end = text[i..].iter().position(|&b| b == b'>').map_or(text.len(), |e| i + e);
        at = end;
        let Ok(tag) = std::str::from_utf8(&text[start..end]) else { continue };
        if !tag.starts_with("<cvParam") {
            continue;
        }
        let mut new = tag.to_string();
        for q in ['"', '\''] {
            for (old, ms) in OBSOLETE_INTEGER_TERMS {
                new = new.replace(&format!("accession={q}{old}{q}"), &format!("accession={q}{ms}{q} "));
            }
            if new != tag {
                new = new.replace(&format!("cvRef={q}IMS{q}"), &format!("cvRef={q}MS{q} "));
            }
        }
        if new != tag {
            out.get_or_insert_with(|| text.to_vec()).splice(start..end, new.bytes());
        }
    }
    out
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// The `.ibd` beside an imzML, where mzdata looks for it: `<stem>.ibd`, else `<stem>.IBD`.
pub fn ibd_beside(imzml: &Path) -> Option<std::path::PathBuf> {
    ["ibd", "IBD"].iter().map(|ext| imzml.with_extension(ext)).find(|p| p.is_file())
}

/// What hashing the `.ibd` found ([`check_ibd`]).
#[derive(Debug, Clone, PartialEq)]
pub struct IbdCheck {
    /// SHA-1 of the `.ibd`, lower-case hex: its `MS:1000569` in `source_files`.
    pub sha1: String,
    /// Per checksum the header states with a value: the term, the value as stated, the hash found.
    pub stated: Vec<(&'static str, String, String)>,
    /// The UUID the header states (`IMS:1000080`, as stated) and the one the `.ibd` begins with
    /// (its first 16 bytes, 32 lower-case hex digits; fewer for a shorter file). `None` when the
    /// header states none.
    pub uuid: Option<(String, String)>,
}

/// A UUID as 32 lower-case hex digits: braces, dashes and whitespace removed (`{554A27FA-79D2-…}`
/// and `554a27fa79d2…` are the same identifier).
fn uuid_hex(stated: &str) -> String {
    stated.chars().filter(|c| !matches!(c, '{' | '}' | '-') && !c.is_whitespace()).flat_map(char::to_lowercase).collect()
}

impl IbdCheck {
    /// `metadata.imaging.provenance.ibd_checksum`.
    pub fn status(&self) -> &'static str {
        match (self.stated.is_empty(), self.mismatches().next().is_some()) {
            (true, _) => "not stated",
            (false, true) => "mismatch",
            (false, false) => "verified",
        }
    }

    /// The stated checksums the `.ibd` does not hash to: term, stated, found. Hex compares without
    /// case (pyimzML writes upper case).
    pub fn mismatches(&self) -> impl Iterator<Item = &(&'static str, String, String)> {
        self.stated.iter().filter(|(_, stated, found)| !stated.trim().eq_ignore_ascii_case(found))
    }

    /// `metadata.imaging.provenance.ibd_uuid`: whether the `.ibd` begins with the UUID the header
    /// states. The imzML specification pairs the two files by it, and where the header states no
    /// checksum it is the one pairing check there is.
    pub fn uuid_status(&self) -> &'static str {
        match &self.uuid {
            None => "not stated",
            Some(_) if self.uuid_mismatch().is_some() => "mismatch",
            Some(_) => "verified",
        }
    }

    /// `(stated, found)` when the header states a UUID the `.ibd` does not begin with.
    pub fn uuid_mismatch(&self) -> Option<(&str, &str)> {
        let (stated, found) = self.uuid.as_ref()?;
        (uuid_hex(stated) != *found).then_some((stated.as_str(), found.as_str()))
    }

    /// `metadata.imaging.provenance.ibd_checksum_found`: the hash found for each stated checksum
    /// the `.ibd` does not match. `None` without a mismatch.
    pub fn found_json(&self) -> Option<serde_json::Value> {
        let rows: Vec<serde_json::Value> = self.mismatches().map(|(acc, _, found)| serde_json::json!({"accession": acc, "value": found})).collect();
        (!rows.is_empty()).then(|| rows.into())
    }
}

/// Hash the `.ibd` in one streamed pass: SHA-1 always (its source-file digest), and the algorithm
/// of each checksum the header's file content states — `IMS:1000090` MD5, `IMS:1000091` SHA-1,
/// `IMS:1000092` SHA-256 — and compare its first 16 bytes with the UUID the header states
/// (`IMS:1000080`; through 0.17.0-rc.1 only mzdata's log line said when they differ, and the archive
/// could read `ibd_checksum: verified` over a `.ibd` that is not the imzML's).
/// Nothing hashed the `.ibd` through 0.16.0: the stated checksum was copied
/// into the archive whether or not the `.ibd` matched it (HUPO-PSI/mzPeak-specification#23; the
/// public chilli set states a SHA-1 its `.ibd` does not have).
pub fn check_ibd(ibd: &Path, content: &[RawParam]) -> Result<IbdCheck> {
    use sha1::Digest;
    let stated = |acc: &str| content.iter().find(|p| p.accession == acc).map(|p| p.value.clone()).filter(|v| !v.trim().is_empty());
    let (md5_stated, sha1_stated, sha256_stated) = (stated("IMS:1000090"), stated("IMS:1000091"), stated("IMS:1000092"));
    let mut sha1 = sha1::Sha1::new();
    let mut md5 = md5_stated.is_some().then(md5::Md5::new);
    let mut sha256 = sha256_stated.is_some().then(sha2::Sha256::new);
    let mut file = std::fs::File::open(ibd).with_context(|| format!("opening {}", ibd.display()))?;
    let mut buf = vec![0u8; 1 << 20];
    // The `.ibd` begins with its UUID, 16 bytes.
    let mut head: Vec<u8> = Vec::with_capacity(16);
    loop {
        let n = file.read(&mut buf).with_context(|| format!("reading {}", ibd.display()))?;
        if n == 0 {
            break;
        }
        if head.len() < 16 {
            head.extend_from_slice(&buf[..n.min(16 - head.len())]);
        }
        sha1.update(&buf[..n]);
        if let Some(h) = md5.as_mut() {
            h.update(&buf[..n]);
        }
        if let Some(h) = sha256.as_mut() {
            h.update(&buf[..n]);
        }
    }
    let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let sha1 = hex(&sha1.finalize());
    let stated = [
        ("IMS:1000090", md5_stated, md5.map(|h| hex(&h.finalize()))),
        ("IMS:1000091", sha1_stated, Some(sha1.clone())),
        ("IMS:1000092", sha256_stated, sha256.map(|h| hex(&h.finalize()))),
    ]
    .into_iter()
    .filter_map(|(acc, stated, found)| Some((acc, stated?, found?)))
    .collect();
    let uuid = content.iter().find(|p| p.accession == "IMS:1000080").map(|p| p.value.clone()).filter(|v| !v.trim().is_empty()).map(|v| (v, hex(&head)));
    Ok(IbdCheck { sha1, stated, uuid })
}

/// `file_description.contents` params for the imzML provenance mzdata consumed, with the values
/// exactly as the header states them (mzdata parses the UUID, and writing its parse back re-spelled
/// `686ec248…` as `{686EC248-…}`: fidelity L0 keeps the identifier as stated).
pub fn provenance_params(content: &[RawParam]) -> Vec<Param> {
    PROVENANCE
        .iter()
        .filter_map(|(curie, name)| {
            let p = content.iter().find(|p| p.accession == curie.to_string())?;
            let b = Param::builder().name(*name).curie(*curie);
            // A string whatever it spells: `Value::from(String)` parses, and a checksum of decimal
            // digits only (or one reading as a float, `12e4…`) was written as a number.
            Some(if p.value.is_empty() { b.build() } else { b.value(mzdata::params::Value::String(p.value.clone())).build() })
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
            <cvParam cvRef="IMS" accession="IMS:1000090" name="ibd MD5" value="00000000000000000000000000000123"/>
            </referenceableParamGroup></referenceableParamGroupList><run/></mzML>"#;
        let p = provenance_params(&read_file_content_from(xml.as_bytes()).unwrap());
        let got: Vec<(String, String)> = p.iter().map(|p| (p.curie().unwrap().to_string(), p.value.to_string())).collect();
        assert_eq!(got, [
            ("IMS:1000031".to_string(), String::new()),
            ("IMS:1000080".to_string(), "686ec248523749d8a17590dde78ab130".to_string()),
            ("IMS:1000090".to_string(), "00000000000000000000000000000123".to_string()),
            ("IMS:1000091".to_string(), "ABCDEF0123".to_string()),
        ]);
        // A checksum of decimal digits only stays the string it is (it was written as the number 123).
        assert!(p.iter().skip(1).all(|p| matches!(p.value, mzdata::params::Value::String(_))), "{p:?}");
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
    /// scan whole — x, y and z together — instead of becoming null on one axis. A z that is not one
    /// leaves alone: the scan keeps its x and y.
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
            scan(&[(X, "2"), (Y, "3"), (Z, "-1")]),
            scan(&[(X, "0"), (Y, "2"), (Z, "0")]),
            scan(&[(Z, "1")]),
            scan(&[other]),
        ];
        assert_eq!(drop_invalid_positions(&mut d), (4, 7, 2));
        let left: Vec<Vec<CURIE>> = d.acquisition.scans.iter().map(|s| s.params().iter().filter_map(|p| p.curie()).filter(|c| POSITIONS.contains(c)).collect()).collect();
        assert_eq!(left, [vec![X, Y], vec![X, Y, Z], vec![], vec![], vec![], vec![], vec![], vec![X, Y], vec![X, Y], vec![], vec![], vec![]]);
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
        // A 3 µm step's float32 gaps lie on both sides of 3 µm (2.99835 and 3.00026 µm at 20 mm):
        // the step is 3 µm, not refused as under it (review 2026-09-30, fourth pass).
        for (from, cols) in [(20.397747f32, 6), (187.63855, 113)] {
            let (a, _) = fit_axis(&raster(100, cols, |_, c| (from + c as f32 * 0.003) as f64), None).unwrap();
            assert_eq!((a.pitch, a.count), (Some(0.003), cols as i64), "{from}");
        }
    }

    /// Review 2026-09-30 B14: the most common gap of exact sparse positions is 0.2 mm, which fails
    /// the quarter-step check — every position was lost. The largest lattice holding them is 0.1 mm;
    /// a stray half a step off makes 0.05 mm hold too, but 0.1 mm is larger.
    #[test]
    fn exact_sparse_positions_fit_the_smallest_step_the_gaps_share() {
        let (a, index) = fit_axis(&[0.0, 0.1, 0.3, 0.5, 0.7], None).unwrap();
        assert_eq!((a.pitch, a.count), (Some(0.1), 8));
        assert_eq!(columns(&index), [1, 2, 4, 6, 8]);
        let mut xs = raster(20, 5, |_, c| (6.0f32 + [0.0, 0.1, 0.3, 0.5, 0.7][c]) as f64);
        xs.push(6.35);
        let (a, index) = fit_axis(&xs, None).unwrap();
        assert_eq!((a.pitch, a.count), (Some(0.1), 8), "{a:?}");
        assert_eq!((columns(&index[..5]), index[100]), (vec![1, 2, 4, 6, 8], None));
    }

    /// Review 2026-09-30 B14: jitter must not pose as the step — continuous, and recorded at 1 µm
    /// or 2 µm (the 2 µm "step" of the first fix). Merging it into columns kept writing wrong grids:
    /// jittered positions fit no lattice, and need the step the method declares.
    #[test]
    fn jittered_positions_fit_the_declared_step_or_none() {
        let mut seed = 7;
        let jittered = raster(20, 30, |_, c| 12.0 + c as f64 * 0.1 + 0.002 * noise(&mut seed));
        let recorded = jittered.iter().map(|v| (v * 1e3).round() / 1e3).collect::<Vec<_>>();
        // ±10 µm recorded at 2 µm over 100 rows: 11 distinct positions per column.
        let mut seed = 7;
        let coarse: Vec<f64> = raster(100, 30, |_, c| 12.0 + c as f64 * 0.1 + 0.01 * noise(&mut seed)).iter().map(|v| (v * 500.0).round() / 500.0).collect();
        for (rows, xs) in [(20, jittered), (20, recorded), (100, coarse)] {
            assert_eq!(fit_axis(&xs, None), None);
            let (a, index) = fit_axis(&xs, Some(0.1)).unwrap();
            assert_eq!((a.pitch, a.count, a.declared), (Some(0.1), 30, true), "{a:?}");
            assert!(a.max_residual <= 0.0101, "{a:?}");
            assert_eq!(columns(&index), raster(rows, 30, |_, c| c as f64 + 1.0).iter().map(|c| *c as i64).collect::<Vec<_>>());
        }
    }

    /// Review 2026-09-30 B14: a serpentine raster whose return passes lag behind has two positions
    /// per column; the lag is not the step. Declared, the step holds both halves (a lag of 20, 30 or
    /// 45 µm). Fitted, 30 and 45 µm lie on no lattice of 3 µm or more — no grid; a lag of a whole
    /// finer step (20 µm of 100) puts every position on that finer lattice, each its own pixel.
    #[test]
    fn a_serpentine_lag_is_not_the_step() {
        let want: Vec<i64> = raster(10, 30, |_, c| c as f64 + 1.0).iter().map(|c| *c as i64).collect();
        for lag in [0.02, 0.03, 0.045] {
            let xs = raster(10, 30, |r, c| 3.0 + c as f64 * 0.1 + if r % 2 == 1 { lag } else { 0.0 });
            let (a, index) = fit_axis(&xs, Some(0.1)).unwrap();
            assert_eq!((a.pitch, a.count, a.declared), (Some(0.1), 30, true), "lag {lag}: {a:?}");
            assert!((a.max_residual - lag / 2.0).abs() < 1e-6, "lag {lag}: {a:?}");
            assert_eq!(columns(&index), want, "lag {lag}");
            let fitted = fit_axis(&xs, None).map(|(a, index)| (a.pitch, a.count, columns(&index)[..2].to_vec()));
            assert_eq!(fitted, (lag == 0.02).then(|| (Some(0.02), 147, vec![1, 6])), "lag {lag}");
        }
        // Lags near a finer lattice's step, which a pitch refined before the decision slid onto
        // (20, 25, 33.33, 50 µm: up to 147 columns; review 2026-09-30, fourth pass).
        for lag in [0.0201, 0.0248, 0.0252, 0.033, 0.0336, 0.0495, 0.0505, 0.0665, 0.0752, 0.0801] {
            let xs = raster(10, 30, |r, c| 3.0 + c as f64 * 0.1 + if r % 2 == 1 { lag } else { 0.0 });
            assert_eq!(fit_axis(&xs, None), None, "lag {lag}");
        }
    }

    /// Two regions each rastered on its own lattice, 30 µm apart (not a whole step), or 33 µm in the
    /// same range (an oversampling pass: 89 columns of 33.33 µm, review 2026-09-30 fourth pass): no
    /// lattice holds both, and there is no grid rather than a wrong one.
    #[test]
    fn regions_on_different_lattices_fit_none() {
        for (from, apart) in [(23.03f32, "side by side"), (28.03, "5 mm apart"), (20.033, "interleaved")] {
            let mut xs = raster(20, 30, |_, c| (20.0f32 + c as f32 * 0.1) as f64);
            xs.extend(raster(20, 30, |_, c| (from + c as f32 * 0.1) as f64));
            assert_eq!(fit_axis(&xs, None), None, "{apart}");
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
        // Positions a few µm apart on no lattice of 3 µm or more: least squares refined a gap of
        // 3.11 µm to a pitch of 2.2 µm (review 2026-09-30, fourth pass).
        let few = [50.027000906319515, 50.027000906319515, 50.027000906319515, 50.00696263927214, 50.00696263927214];
        assert_eq!(fit_axis(&[&few[..], &[50.01007604845414, 50.024819139010326, 50.024819139010326]].concat(), None), None);
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
        // Half a step off, inside the raster: the lattice of half the step holds it too, but the
        // step's is larger and holds all but it.
        xs.pop();
        xs.push(80.3673 + 5.05);
        let (a, index) = fit_axis(&xs, None).unwrap();
        assert_eq!((a.pitch, a.count), (Some(0.1), 104), "{a:?}");
        assert_eq!(index.last(), Some(&None));
        // Few columns, several parked scans, each at a position of its own: their gaps are a third
        // of all gaps, and none of them is a column.
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

    /// Review 2026-09-30, fourth pass: 4 strays in 304 scans (1.3 %) lose the 0.1 mm grid, and a
    /// pitch refined before the decision slid from a stray's gap (81.8 µm) to 0.1/11 mm, on which
    /// every column and stray lies: 320 columns of 9.09 µm. No grid; the declared step holds.
    #[test]
    fn strays_over_one_percent_seat_no_finer_lattice() {
        let col = |c: usize| (20.0f32 + c as f32 * 0.1) as f64;
        let mut xs = raster(10, 30, |_, c| col(c));
        xs.extend([col(6) + 0.0091, col(6) + 0.0182, col(13) + 0.0091, col(13) + 0.0182]);
        assert_eq!(fit_axis(&xs, None), None);
        let (a, index) = fit_axis(&xs, Some(0.1)).unwrap();
        assert_eq!((a.pitch, a.count, &index[..3]), (Some(0.1), 30, &[Some(1), Some(2), Some(3)][..]));
    }

    /// Review 2026-09-30, fourth pass: strays in the gaps of a sparse raster (every fourth column
    /// missing, a stray 0.6 µm off the middle of each 30 µm gap, 0.62 % of the scans) make gaps of
    /// 14.4 and 15.6 µm beside the 15 µm ones; their mean (14.8 µm) was no step. The step is 15 µm.
    #[test]
    fn a_stray_splitting_a_gap_does_not_pull_the_step_off() {
        let cols: Vec<usize> = (0..60).filter(|c| c % 4 != 3).collect();
        let at = |c: f32| (81.13209f32 + c * 0.015) as f64;
        let mut xs = raster(50, cols.len(), |_, i| at(cols[i] as f32));
        xs.extend((0..14).map(|k| at(4.0 * k as f32 + 3.0) + 0.0006));
        let (a, index) = fit_axis(&xs, None).unwrap();
        assert_eq!((a.pitch, a.count), (Some(0.015), 59), "{a:?}");
        assert_eq!(index.iter().filter(|i| i.is_none()).count(), 14);
    }

    /// Review 2026-09-30, fifth pass: in a sparse raster, scans a fraction of a µm off absent
    /// columns outnumbered the step's own gaps and moved the run's median off the step, so the step
    /// was never tried and one stray half a step off seated a half-step lattice.
    #[test]
    fn scans_just_off_absent_columns_do_not_hide_the_step() {
        let cols: Vec<usize> = [0, 1].into_iter().chain((4..=88).step_by(3)).collect();
        let at = |c: f32| (20.0f32 + c * 0.1) as f64;
        let mut xs = raster(10, cols.len(), |_, i| at(cols[i] as f32));
        xs.extend([at(5.0) + 0.0003, at(8.0) + 0.0003, at(10.0) + 0.05]);
        let (a, index) = fit_axis(&xs, None).unwrap();
        assert_eq!((a.pitch, a.count), (Some(0.1), 89), "{a:?}");
        assert_eq!(index.iter().filter(|i| i.is_none()).count(), 1);
    }

    /// Review 2026-09-30, sixth pass: the stray-seated-fraction check took a row acquired twice, or
    /// a one-row region, for the coarser lattice and every other row for strays.
    #[test]
    fn a_row_acquired_twice_is_no_coarser_lattice() {
        let y = |r: f32| (40.0f32 + r * 0.1) as f64;
        let mut ys = raster(20, 30, |r, _| y(r as f32));
        ys.extend((0..30).map(|_| y(7.0)));
        assert_eq!(fit_axis(&ys, None).map(|f| (f.0.pitch, f.0.count)), Some((Some(0.1), 20)));
        let y5 = |r: f32| (40.0f32 + r * 0.05) as f64;
        let mut ys = raster(20, 20, |r, _| y5(r as f32));
        ys.extend((0..40).map(|_| y5(10.0)));
        assert_eq!(fit_axis(&ys, None).map(|f| (f.0.pitch, f.0.count)), Some((Some(0.05), 20)));
    }

    /// Review 2026-09-30, sixth pass: trying every gap of a run at 17 offsets took 21 s to refuse
    /// 100 rows of 10,000 columns with ±0.8 µm stage error per column (26 ms before).
    #[test]
    fn a_refusal_stays_fast() {
        let mut seed = 5u64;
        let off: Vec<f64> = (0..10_000).map(|_| 0.0008 * noise(&mut seed)).collect();
        let xs = raster(100, 10_000, |_, c| 30.0 + c as f64 * 0.05 + off[c]);
        let t = std::time::Instant::now();
        assert!(fit_axis(&xs, None).is_none());
        assert!(t.elapsed().as_secs_f64() < 3.0, "{:?}", t.elapsed());
    }

    /// Review 2026-09-30, sixth pass: sub-µm scatter on three 5 µm columns let a gap other than the
    /// median hold 4.769 µm; and least squares slid the pitch to seat a stray 0.55 µm off (4.988 µm).
    #[test]
    fn neither_scatter_nor_a_stray_moves_the_step() {
        let xs = [
            141.86657616777987, 141.87166224238672, 141.87626637913908, 141.86631059022847, 141.8716289639936,
            141.87603692814076, 141.86616945213697, 141.87154144273012, 141.87581197003428,
        ];
        assert!(fit_axis(&xs, None).is_none_or(|f| f.0.pitch == Some(0.005)));
        let at = |c: f32| (138.66708f32 + c * 0.005) as f64;
        let mut xs = raster(38, 6, |_, c| at(c as f32));
        xs.extend([at(14.0) - 0.0003, at(18.0) - 0.00055]);
        assert_eq!(fit_axis(&xs, None).unwrap().0.pitch, Some(0.005));
    }

    /// Review 2026-09-30, fifth pass: a very sparse raster at 195 mm whose step's only neighbouring
    /// gaps are float32 values 0.009 µm off it (1.16 µm of drift over 127 steps) was refused — and
    /// with one stray half a step off it got a 75 µm lattice.
    #[test]
    fn a_sparse_float32_raster_keeps_its_step() {
        let cols = [0, 6, 13, 21, 23, 24, 32, 42, 51, 55, 70, 73, 78, 85, 93, 94, 96, 100, 107, 109, 112, 118, 121, 127];
        let at = |c: f32| (195.51346f32 + c * 0.15) as f64;
        let mut xs = raster(12, cols.len(), |_, i| at(cols[i] as f32));
        for stray in [None, Some(at(106.5))] {
            xs.extend(stray);
            let (a, index) = fit_axis(&xs, None).unwrap();
            assert_eq!((a.pitch, a.count), (Some(0.15), 128), "{stray:?}: {a:?}");
            assert_eq!(index.iter().filter(|i| i.is_none()).count(), stray.iter().count());
        }
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
        assert_eq!(f.transformations.clone(), vec![UNIT_ASSUMED]);
        assert_eq!(f.write, vec![(PIXEL_X, 20.0, true), (PIXEL_Y, 25.0, true)]);
    }

    #[test]
    fn one_value_is_tested_as_area_then_length() {
        // 100 px × 10 µm = 1000 µm: 100 (µm²) is an area, 10 a length.
        // A lone x that is kept is also the y size (the vocabulary's default): written as both.
        let area = pixel_size_fix(&settings(&[("IMS:1000046", "100", None), ("IMS:1000042", "100", None), ("IMS:1000044", "1000", UM)])).unwrap();
        assert_eq!((area.transformations.clone(), area.write.clone()), (vec![AREA_TO_LENGTH], vec![(PIXEL_X, 10.0, true), (PIXEL_Y, 10.0, true)]));
        let length = pixel_size_fix(&settings(&[("IMS:1000046", "10", UM), ("IMS:1000042", "100", None), ("IMS:1000044", "1000", UM)])).unwrap();
        assert_eq!((length.transformations.clone(), length.write.clone()), (Vec::<&str>::new(), vec![(PIXEL_X, 10.0, false), (PIXEL_Y, 10.0, false)]));
        assert_eq!(length.write_units, vec![Some("UO:0000017".to_string()); 2], "the y carries the x's unit");
        assert!(area.single && length.single && length.detail.contains("also written as IMS:1000047"), "{}", length.detail);
        // Only the y axis states count and extent: it is used.
        let y_axis = pixel_size_fix(&settings(&[("IMS:1000046", "400", UM), ("IMS:1000043", "50", None), ("IMS:1000045", "1000", UM)])).unwrap();
        assert_eq!(y_axis.transformations.clone(), vec![AREA_TO_LENGTH]);
        let neither = pixel_size_fix(&settings(&[("IMS:1000046", "7", UM), ("IMS:1000042", "100", None), ("IMS:1000044", "1000", UM)])).unwrap();
        assert_eq!((neither.case, neither.transformations.clone(), neither.write.is_empty()), ("one value that tests as neither area nor length", vec![DROPPED], true));
        // Nothing to test against is its own case, and the detail names what the header lacks: it
        // said "no pixel count and max dimension" of a header stating both counts
        // (HUPO-PSI/mzPeak-specification#23, pixel_size_area_old_name_no_extent).
        let untestable = |rest: &[(&str, &str, Option<(&str, &str)>)]| {
            let f = pixel_size_fix(&settings(&[&[("IMS:1000046", "7", UM)], rest].concat())).unwrap();
            assert_eq!((f.case, f.transformations.clone(), f.write.is_empty(), f.single), ("one value, untestable", vec![DROPPED], true, false));
            f.detail
        };
        assert_eq!(untestable(&[]), "IMS:1000046=7; no pixel count (IMS:1000042/43) and no max dimension (IMS:1000044/45) to test it against");
        assert_eq!(untestable(&[("IMS:1000042", "3", None), ("IMS:1000043", "2", None)]), "IMS:1000046=7; no max dimension (IMS:1000044/45) to test it against");
        assert_eq!(untestable(&[("IMS:1000045", "100", UM)]), "IMS:1000046=7; no pixel count (IMS:1000042/43) to test it against");
        assert_eq!(untestable(&[("IMS:1000042", "3", None), ("IMS:1000045", "100", UM)]), "IMS:1000046=7; no axis with both a pixel count and a max dimension to test it against");
    }

    /// HUPO-PSI/mzPeak-specification#23: x and y both stated were never held against the extent
    /// (50 and 2500 over 150 × 100 µm passed in silence). Each axis that states count and max
    /// dimension is compared and a disagreement reported; the values are written as stated.
    #[test]
    fn x_and_y_are_compared_with_the_extent() {
        let grid = [("IMS:1000042", "3", None), ("IMS:1000043", "2", None), ("IMS:1000044", "150", UM), ("IMS:1000045", "100", UM)];
        let with = |x: (&'static str, Option<(&'static str, &'static str)>), y: (&'static str, Option<(&'static str, &'static str)>)| {
            pixel_size_fix(&settings(&[&[("IMS:1000046", x.0, x.1), ("IMS:1000047", y.0, y.1)], &grid[..]].concat()))
        };
        assert_eq!(with(("50", UM), ("50", UM)), None, "consistent: nothing to report");
        let f = with(("50", UM), ("2500", UM)).unwrap();
        assert_eq!((f.case, f.transformations.clone(), f.write.clone()), ("x and y with a unit", Vec::<&str>::new(), vec![(PIXEL_X, 50.0, false), (PIXEL_Y, 2500.0, false)]));
        assert_eq!(f.extent_mismatches, ["IMS:1000047=2500 micrometer × 2 pixels is not the max dimension IMS:1000045=100 micrometer"]);
        assert_eq!(fix_json(&f)["extent_mismatches"].as_array().unwrap().len(), 1);
        // Without a unit: micrometre assumed, as before, and the same comparison.
        let f = with(("50", None), ("2500", None)).unwrap();
        assert_eq!((f.transformations.clone(), f.extent_mismatches.len()), (vec![UNIT_ASSUMED], 1));
        assert!(with(("50", None), ("50", None)).unwrap().extent_mismatches.is_empty());
        // Compared in one length unit: 0.05 mm × 3 = 150 µm.
        const MM: Option<(&str, &str)> = Some(("UO:0000016", "millimeter"));
        assert_eq!(with(("0.05", MM), ("0.05", MM)), None);
        // A unit that is no length is not compared, and an axis without count or extent is not.
        assert_eq!(with(("7", Some(("UO:0000186", "dimensionless unit"))), ("50", UM)), None);
        assert_eq!(pixel_size_fix(&settings(&[("IMS:1000046", "7", UM), ("IMS:1000047", "7", UM), ("IMS:1000042", "3", None)])), None);
        // Applying such a fix changes nothing.
        let mut ss = ScanSettings { id: "s1".into(), ..Default::default() };
        ss.params.push(Param::builder().name("pixel size").curie(mzdata::curie!(IMS:1000046)).value(50).unit(Unit::Micrometer).build());
        ss.params.push(Param::builder().name("pixel size y").curie(mzdata::curie!(IMS:1000047)).value(2500).unit(Unit::Micrometer).build());
        let before = ss.clone();
        apply(&with(("50", UM), ("2500", UM)).unwrap(), &mut ss);
        assert_eq!(ss.params, before.params);
    }

    /// Review 2026-09-30 B17: a single y is tested against y (x was tried first), and value and
    /// extent are compared in one length unit, by accession.
    #[test]
    fn one_value_is_tested_on_its_own_axis_in_one_unit() {
        const MM: Option<(&str, &str)> = Some(("UO:0000016", "millimeter"));
        // y = 100 µm: an area on x (√100 × 10 = 100) but a length on its own axis (100 × 5 = 500).
        let y = pixel_size_fix(&settings(&[
            ("IMS:1000047", "100", UM),
            ("IMS:1000042", "10", None), ("IMS:1000044", "100", UM),
            ("IMS:1000043", "5", None), ("IMS:1000045", "500", UM),
        ]))
        .unwrap();
        assert_eq!((y.case, y.transformations.clone(), y.write), ("one value: a length (value × count = extent)", Vec::<&str>::new(), vec![(PIXEL_Y, 100.0, false)]));
        // 0.01 mm × 100 = 1000 µm: a length, once both are in µm.
        let mm = pixel_size_fix(&settings(&[("IMS:1000046", "0.01", MM), ("IMS:1000042", "100", None), ("IMS:1000044", "1000", UM)])).unwrap();
        assert_eq!((mm.transformations.clone(), mm.write), (Vec::<&str>::new(), vec![(PIXEL_X, 0.01, false), (PIXEL_Y, 0.01, false)]));
        let nm = pixel_size_fix(&settings(&[("IMS:1000046", "10", UM), ("IMS:1000042", "100", None), ("IMS:1000044", "1000000", Some(("UO:0000018", "nanometer")))])).unwrap();
        assert_eq!(nm.transformations.clone(), Vec::<&str>::new(), "10 µm × 100 = 10⁶ nm");
        // An area in mm²: its square root in mm (√0.0001 mm² = 0.01 mm; × 100 = 1 mm), the unit kept.
        let area = pixel_size_fix(&settings(&[("IMS:1000046", "0.0001", MM), ("IMS:1000042", "100", None), ("IMS:1000044", "1", MM)])).unwrap();
        assert_eq!((area.transformations.clone(), area.write.clone()), (vec![AREA_TO_LENGTH], vec![(PIXEL_X, 0.01, false), (PIXEL_Y, 0.01, false)]));
        // The index row says which unit that is (its key, `written_um`, predates the mm case).
        assert_eq!(fix_json(&area)["written_um"][0]["unit"], "UO:0000016");
        assert_eq!(fix_json(&area)["written_um"][1], serde_json::json!({"accession": "IMS:1000047", "value": 0.01, "unit": "UO:0000016", "unit_assumed": false, "unit_from_accession": false}));
        // No unit anywhere: micrometre, and the detail says so.
        let bare = pixel_size_fix(&settings(&[("IMS:1000046", "20", None), ("IMS:1000042", "100", None), ("IMS:1000044", "2000", None)])).unwrap();
        assert_eq!((bare.transformations.clone(), bare.write.clone()), (vec![UNIT_ASSUMED], vec![(PIXEL_X, 20.0, true), (PIXEL_Y, 20.0, true)]));
        assert_eq!(fix_json(&bare)["written_um"][0]["unit"], "UO:0000017");
        assert!(bare.detail.contains("(IMS:1000046, IMS:1000044 without a length unit: micrometre assumed)"), "{}", bare.detail);
        assert!(!mm.detail.contains("assumed"), "{}", mm.detail);
        // The detail's equation carries its units: the numbers are in different ones.
        assert!(mm.detail.contains("0.01 millimeter × 100 = 1000 micrometer"), "{}", mm.detail);
        assert!(area.detail.contains("√(0.0001 millimeter²) × 100 = 1 millimeter"), "{}", area.detail);
        // A stated unit that is no length (UO:0000186, dimensionless) is tested as µm but kept, as
        // the two-value case keeps it: nothing is declared.
        let odd = pixel_size_fix(&settings(&[("IMS:1000046", "100", Some(("UO:0000186", "dimensionless unit"))), ("IMS:1000042", "3", None), ("IMS:1000044", "300", UM)])).unwrap();
        assert_eq!((odd.transformations.clone(), odd.write.clone()), (Vec::<&str>::new(), vec![(PIXEL_X, 100.0, false), (PIXEL_Y, 100.0, false)]));
        assert_eq!(fix_json(&odd)["written_um"][0]["unit"], "UO:0000186");
        assert!(odd.detail.contains("(IMS:1000046 without a length unit: micrometre assumed)"), "{}", odd.detail);
    }

    #[test]
    fn a_unit_accession_that_disagrees_with_its_name_is_reported() {
        let cm = Some(("UO:0000015", "micrometer"));
        // With an extent the name's reading fits (50 µm × 3 = 150 µm): reported, not rewritten.
        let extent = [("IMS:1000042", "3", None), ("IMS:1000044", "150", UM), ("IMS:1000043", "2", None), ("IMS:1000045", "100", UM)];
        let f = pixel_size_fix(&settings(&[&[("IMS:1000046", "50", cm), ("IMS:1000047", "50", cm)], &extent[..]].concat())).unwrap();
        assert_eq!(f.transformations.clone(), Vec::<&str>::new(), "reported, not rewritten");
        assert_eq!(f.unit_mismatches.len(), 2, "{f:?}");
        // Without one, nothing tells 50 µm from 50 cm: dropped (owner decision D4).
        let untested = pixel_size_fix(&settings(&[("IMS:1000046", "50", cm), ("IMS:1000047", "50", cm)])).unwrap();
        assert_eq!((untested.case, untested.transformations.clone(), untested.write.len()), ("x and y: a unit contradiction nothing tests", vec![DROPPED], 0));
        // Written as micrometre (mzdata took the name): the index row gives that unit, not the
        // stated centimetre accession.
        let mut ss = ScanSettings { id: "s1".into(), ..Default::default() };
        for acc in [mzdata::curie!(IMS:1000046), mzdata::curie!(IMS:1000047)] {
            ss.params.push(Param::builder().name("pixel size").curie(acc).value(50.0).unit(Unit::Micrometer).build());
        }
        let mut f = f;
        apply(&f, &mut ss);
        assert!(check_written_units(&mut f, &ss));
        let units: Vec<serde_json::Value> = fix_json(&f)["written_um"].as_array().unwrap().iter().map(|e| e["unit"].clone()).collect();
        assert_eq!(units, ["UO:0000017", "UO:0000017"]);
        let fine = Some(("UO:0000017", "micrometre"));
        assert_eq!(pixel_size_fix(&settings(&[("IMS:1000046", "50", fine), ("IMS:1000047", "50", fine)])), None);
    }

    /// Review 2026-09-30: a single value is tested in the unit mzdata writes it in — the unit
    /// name's when mzdata knows the name — not by its accession, so what passes is written as it
    /// passed. The extent is 3 px × 300 µm throughout.
    #[test]
    fn one_value_is_tested_in_the_unit_it_is_written_in() {
        let one_y = |v: &str, unit: Option<(&str, &str)>| {
            pixel_size_fix(&settings(&[("IMS:1000047", v, unit), ("IMS:1000043", "3", None), ("IMS:1000045", "300", UM)])).unwrap()
        };
        let cm_named_um = Some(("UO:0000015", "micrometer"));
        // Written as 100 µm: a length. By accession (100 cm) it was dropped.
        let kept = one_y("100", cm_named_um);
        assert_eq!((kept.transformations.clone(), kept.write.clone(), kept.write_units.clone()), (Vec::<&str>::new(), vec![(PIXEL_Y, 100.0, false)], vec![Some("UO:0000017".into())]));
        // 0.01 µm (the name) is no 100 µm pixel; 0.01 cm (the accession) × 3 = 300 µm is: written
        // as 100 µm from the accession's reading (D4; dropped through 0.17.0-rc.2, when it passed
        // by accession and was written as 0.01 µm before review B17, then dropped).
        let by_acc = one_y("0.01", cm_named_um);
        assert_eq!((by_acc.transformations.clone(), by_acc.write.clone(), by_acc.write_units.clone()), (vec![UNIT_FROM_ACCESSION], vec![(PIXEL_Y, 100.0, true)], vec![Some("UO:0000017".into())]));
        // The micrometre accession named "millimeter": 100 mm is no 100 µm pixel, 100 µm is.
        assert_eq!(one_y("100", Some(("UO:0000017", "millimeter"))).write, vec![(PIXEL_Y, 100.0, true)]);
        // A unit stated by its name alone is a unit: 0.1 mm, kept in mm, nothing assumed.
        let mut s = settings(&[("IMS:1000047", "0.1", Some(("", "millimeter"))), ("IMS:1000043", "3", None), ("IMS:1000045", "300", UM)]);
        s.params[0].unit_accession = None;
        let by_name = pixel_size_fix(&s).unwrap();
        assert_eq!((by_name.transformations.clone(), by_name.write.clone(), by_name.write_units), (Vec::<&str>::new(), vec![(PIXEL_Y, 0.1, false)], vec![Some("UO:0000016".into())]));
        // So is it for x and y: kept as stated, no micrometre over it.
        let mut xy = settings(&[("IMS:1000046", "0.1", Some(("", "millimeter"))), ("IMS:1000047", "0.1", Some(("", "millimeter")))]);
        xy.params.iter_mut().for_each(|p| p.unit_accession = None);
        assert_eq!(pixel_size_fix(&xy), None);
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
            transformations: vec![AREA_TO_LENGTH],
            write: vec![(PIXEL_X, 10.0, true)],
            by_accession: vec![],
            single: false,
            write_units: vec![],
            unit_mismatches: vec![],
            mismatched: vec![],
            written_units: vec![],
            extent_mismatches: vec![],
            detail: String::new(),
        };
        apply(&fix, &mut ss);
        assert_eq!(ss.params[0].value.to_f64().unwrap(), 10.0);
        assert_eq!(ss.params[0].unit, Unit::Micrometer);
        assert!(one_way_to_flyback(&mut ss));
        assert_eq!(ss.params[1].curie().unwrap().to_string(), "IMS:1000413");
        // An area whose unit is stated keeps it: √(mm²) is mm.
        let mut in_mm = ScanSettings { id: "s1".into(), ..Default::default() };
        in_mm.params.push(Param::builder().name("pixel size x").curie(mzdata::curie!(IMS:1000046)).value(0.0001).unit(Unit::Millimeter).build());
        apply(&PixelSizeFix { write: vec![(PIXEL_X, 0.01, false)], ..fix.clone() }, &mut in_mm);
        assert_eq!((in_mm.params[0].value.to_f64().unwrap(), in_mm.params[0].unit), (0.01, Unit::Millimeter));
        let drop = PixelSizeFix { write: vec![], transformations: vec![DROPPED], ..fix };
        apply(&drop, &mut ss);
        assert!(ss.params.iter().all(|p| p.curie().unwrap().to_string() != "IMS:1000046"));
    }

    /// HUPO-PSI/mzPeak-specification#23: a lone `IMS:1000046` the single-value rule keeps is
    /// written as both sizes under the vocabulary's names — the area's root and a length kept as
    /// stated alike — and the marker reads a lone x the same way, unless the lane says the x is a
    /// measured axis only (the Waters single row).
    #[test]
    fn a_lone_x_that_is_kept_is_written_as_x_and_y() {
        let named = |s: &ScanSettings| -> Vec<(String, String, f64, Unit)> {
            s.params.iter().map(|p| (p.curie().unwrap().to_string(), p.name.clone(), p.value.to_f64().unwrap(), p.unit)).collect()
        };
        let grid = [("IMS:1000042", "3", None), ("IMS:1000044", "150", UM)];
        let source = |value: &str, unit: Unit| {
            let mut ss = ScanSettings { id: "s1".into(), ..Default::default() };
            ss.params.push(Param::builder().name("max count of pixels x").curie(mzdata::curie!(IMS:1000042)).value(3).build());
            ss.params.push(Param::builder().name("pixel size").curie(mzdata::curie!(IMS:1000046)).value(value.parse::<i64>().unwrap()).unit(unit).build());
            ss.params.push(Param::builder().name("max dimension x").curie(mzdata::curie!(IMS:1000044)).value(150).unit(Unit::Micrometer).build());
            ss
        };
        // 2500 with no unit: an area; 50 µm on both axes, the y right after the x.
        let mut area = pixel_size_fix(&settings(&[&[("IMS:1000046", "2500", None)], &grid[..]].concat())).unwrap();
        let mut ss = source("2500", Unit::Unknown);
        apply(&area, &mut ss);
        assert_eq!(
            named(&ss)[1..3],
            [("IMS:1000046".to_string(), "pixel size (x)".to_string(), 50.0, Unit::Micrometer), ("IMS:1000047".to_string(), "pixel size y".to_string(), 50.0, Unit::Micrometer)]
        );
        assert!(!check_written_units(&mut area, &ss));
        assert_eq!(area.write_units, vec![Some("UO:0000017".to_string()); 2]);
        // 50 µm, a length with its unit: nothing declared, the value untouched (still the integer
        // the source wrote), renamed, and given as y too.
        let length = pixel_size_fix(&settings(&[&[("IMS:1000046", "50", UM)], &grid[..]].concat())).unwrap();
        assert_eq!(length.transformations.clone(), Vec::<&str>::new());
        let mut ss = source("50", Unit::Micrometer);
        apply(&length, &mut ss);
        assert_eq!(named(&ss)[1..3], named(&{ let mut a = source("2500", Unit::Unknown); apply(&area, &mut a); a })[1..3]);
        assert_eq!((ss.params[1].value.clone(), ss.params[2].value.clone()), (50i64.into(), 50i64.into()));
        let marker = |s: &ScanSettings, lone_x| marker_block(Some(s), COUNTS_DECLARED, lone_x, SOURCE_DECLARED, serde_json::json!({}));
        assert_eq!(marker(&ss, LoneX::XOnly)["pixel_size_um"], serde_json::json!({"x": 50.0, "y": 50.0}), "both are written");
        // A lone y says nothing about x: kept alone, renamed.
        let lone_y = pixel_size_fix(&settings(&[("IMS:1000047", "50", UM), ("IMS:1000043", "2", None), ("IMS:1000045", "100", UM)])).unwrap();
        assert_eq!((lone_y.single, lone_y.write.clone()), (true, vec![(PIXEL_Y, 50.0, false)]));
        let mut ss = ScanSettings { id: "s1".into(), ..Default::default() };
        ss.params.push(Param::builder().name("pixel size").curie(mzdata::curie!(IMS:1000047)).value(50).unit(Unit::Micrometer).build());
        apply(&lone_y, &mut ss);
        assert_eq!(named(&ss), [("IMS:1000047".to_string(), "pixel size y".to_string(), 50.0, Unit::Micrometer)]);
        assert!(marker(&ss, LoneX::AlsoY).get("pixel_size_um").is_none());
        // The marker: a lone x gives both by the vocabulary, or neither where the lane measured x
        // alone; a stated y is the y, in micrometres whatever length unit it is in, and a size in
        // a unit that is no length gives none.
        let mut lone = ScanSettings { id: "s1".into(), ..Default::default() };
        lone.params.push(Param::builder().name("pixel size (x)").curie(mzdata::curie!(IMS:1000046)).value(50.0).unit(Unit::Micrometer).build());
        assert_eq!(marker(&lone, LoneX::AlsoY)["pixel_size_um"], serde_json::json!({"x": 50.0, "y": 50.0}));
        assert!(marker(&lone, LoneX::XOnly).get("pixel_size_um").is_none());
        assert!(states_pixel_size(Some(&lone)) && !states_pixel_size(Some(&ScanSettings::default())) && !states_pixel_size(None));
        lone.params.push(Param::builder().name("pixel size y").curie(mzdata::curie!(IMS:1000047)).value(0.05).unit(Unit::Millimeter).build());
        assert_eq!(marker(&lone, LoneX::AlsoY)["pixel_size_um"], serde_json::json!({"x": 50.0, "y": 50.0}), "a stated y in mm, in µm");
        lone.params[1].value = 0.02.into();
        assert_eq!(marker(&lone, LoneX::AlsoY)["pixel_size_um"], serde_json::json!({"x": 50.0, "y": 20.0}), "a stated y is not replaced by the x");
        lone.params[1].unit = Unit::Second;
        assert!(marker(&lone, LoneX::AlsoY).get("pixel_size_um").is_none(), "a stated y that is no length is not replaced by the x");
        lone.params.clear();
        lone.params.push(Param::builder().name("pixel size (x)").curie(mzdata::curie!(IMS:1000046)).value(0.05).unit(Unit::Millimeter).build());
        assert_eq!(marker(&lone, LoneX::AlsoY)["pixel_size_um"], serde_json::json!({"x": 50.0, "y": 50.0}));
        lone.params[0].unit = Unit::Unknown;
        assert!(marker(&lone, LoneX::AlsoY).get("pixel_size_um").is_none(), "no unit, no micrometres");
    }

    /// Owner decision D4 (2026-10-01): a pixel-size param whose unit accession and unit name
    /// disagree is read both ways against count × max dimension. The name's reading is tried first
    /// (kept as it was); when only the accession's passes, the value is written in micrometres from
    /// the accession's unit and declared; neither passing, or nothing to test against (the issue
    /// author's `pixel_size_unit_contradiction` fixture), drops it.
    #[test]
    fn a_unit_contradiction_is_read_both_ways_against_the_extent() {
        let cm_named_um = Some(("UO:0000015", "micrometer"));
        let grid = [("IMS:1000042", "3", None), ("IMS:1000044", "150", UM)];
        let one = |v: &str| pixel_size_fix(&settings(&[&[("IMS:1000046", v, cm_named_um)], &grid[..]].concat())).unwrap();
        // 50 "micrometer" × 3 = 150 µm: the name's reading passes, as before.
        let by_name = one("50");
        assert_eq!((by_name.case, by_name.transformations.clone(), by_name.write.clone()), ("one value: a length (value × count = extent)", vec![], vec![(PIXEL_X, 50.0, false), (PIXEL_Y, 50.0, false)]));
        assert_eq!(by_name.source(), SOURCE_DECLARED);
        // 0.005 cm × 3 = 150 µm: the checker's variant v9, dropped through rc.2; now the accession's
        // reading, written as 50 µm and declared.
        let by_acc = one("0.005");
        assert_eq!(by_acc.case, "one value: a length (value × count = extent), by its unit accession");
        assert_eq!(by_acc.transformations.clone(), vec![UNIT_FROM_ACCESSION]);
        assert_eq!(by_acc.write, vec![(PIXEL_X, 50.0, true), (PIXEL_Y, 50.0, true)]);
        assert_eq!(by_acc.by_accession, vec![PIXEL_X, PIXEL_Y]);
        assert_eq!(by_acc.source(), SOURCE_DECLARED);
        let row = fix_json(&by_acc);
        assert_eq!(row["transformations"], serde_json::json!([UNIT_FROM_ACCESSION]));
        assert_eq!(row["transformation"], UNIT_FROM_ACCESSION);
        assert_eq!(row["written_um"][0], serde_json::json!({"accession": PIXEL_X, "value": 50.0, "unit": "UO:0000017", "unit_assumed": false, "unit_from_accession": true}));
        assert!(by_acc.detail.contains("0.005 centimeter × 3 = 150 micrometer") && by_acc.detail.contains("written as 50 micrometer"), "{}", by_acc.detail);
        // An area by accession: √(0.000025 cm²) = 0.005 cm; × 3 = 150 µm.
        let area = one("0.000025");
        assert_eq!(area.transformations.clone(), vec![AREA_TO_LENGTH, UNIT_FROM_ACCESSION]);
        assert!((area.write[0].1 - 50.0).abs() < 1e-9 && area.write[0].2, "{:?}", area.write);
        assert_eq!(area.source(), SOURCE_FROM_AREA);
        // Neither reading: dropped, the detail naming both.
        let neither = one("7");
        assert_eq!((neither.transformations.clone(), neither.write.len(), neither.source()), (vec![DROPPED], 0, SOURCE_UNKNOWN));
        assert!(neither.detail.contains("neither as micrometer (the unit name) nor as centimeter (the unit accession)"), "{}", neither.detail);
        // Nothing to test against: dropped (Theodoros's pixel_size_unit_contradiction).
        let untestable = pixel_size_fix(&settings(&[("IMS:1000046", "50", cm_named_um)])).unwrap();
        assert_eq!((untestable.case, untestable.transformations.clone()), ("one value, untestable", vec![DROPPED]));
        // x and y both stated: each axis read both ways on its own extent.
        let xy = [("IMS:1000042", "3", None), ("IMS:1000043", "2", None), ("IMS:1000044", "150", UM), ("IMS:1000045", "100", UM)];
        let both = |x: &str, y: &str| pixel_size_fix(&settings(&[&[("IMS:1000046", x, cm_named_um), ("IMS:1000047", y, cm_named_um)], &xy[..]].concat()));
        let f = both("50", "50").unwrap();
        assert!(f.transformations.is_empty() && f.extent_mismatches.is_empty() && !f.unit_mismatches.is_empty(), "{f:?}");
        let f = both("0.005", "0.005").unwrap();
        assert_eq!((f.case, f.transformations.clone()), ("x and y with a unit; one read by its unit accession", vec![UNIT_FROM_ACCESSION]));
        assert_eq!(f.write, vec![(PIXEL_X, 50.0, true), (PIXEL_Y, 50.0, true)]);
        assert_eq!(f.source(), SOURCE_DECLARED);
        let mixed = both("50", "0.005").unwrap();
        assert_eq!((mixed.write.clone(), mixed.by_accession.clone()), (vec![(PIXEL_X, 50.0, false), (PIXEL_Y, 50.0, true)], vec![PIXEL_Y]));
        assert_eq!(fix_json(&mixed)["written_um"][1]["unit_from_accession"], true);
        let f = both("7", "50").unwrap();
        assert_eq!((f.case, f.transformations.clone(), f.write.len()), ("x and y: a unit contradiction that passes under neither reading", vec![DROPPED], 0));
        // A plain unit without a contradiction keeps today's report: written as stated, the axis listed.
        let plain = pixel_size_fix(&settings(&[&[("IMS:1000046", "7", UM), ("IMS:1000047", "50", UM)], &xy[..]].concat())).unwrap();
        assert_eq!((plain.transformations.len(), plain.extent_mismatches.len(), plain.write.len()), (0, 1, 2));
        // Applying the accession's reading writes 50 µm over the stated 0.005 (held as µm, by the name).
        let mut ss = ScanSettings { id: "s1".into(), ..Default::default() };
        ss.params.push(Param::builder().name("pixel size").curie(mzdata::curie!(IMS:1000046)).value(0.005).unit(Unit::Micrometer).build());
        apply(&by_acc, &mut ss);
        let written: Vec<(String, f64, Unit)> = ss.params.iter().map(|p| (p.curie().unwrap().to_string(), p.value.to_f64().unwrap(), p.unit)).collect();
        assert_eq!(written, [("IMS:1000046".to_string(), 50.0, Unit::Micrometer), ("IMS:1000047".to_string(), 50.0, Unit::Micrometer)]);
    }

    /// `--pixel-size`, the lanes' one rule on a grid entry: fills an unsized grid (max dimension
    /// from the counts), leaves an agreeing size alone, fills the axis a lone x leaves open, refuses
    /// a differing size naming both, and writes over it under --force with the max dimension
    /// recomputed; the marker says `user_supplied`, and `unknown` of any grid without a size.
    #[test]
    fn a_user_pixel_size_fills_only_what_the_source_leaves_unsettled() {
        let user = |x: f64, y: f64, force: bool| UserPixelSize { x, y, force };
        let count = |acc, name: &str, n: i64| Param::builder().name(name).curie(acc).value(n).build();
        let size = |acc, name: &str, v: f64, unit| Param::builder().name(name).curie(acc).value(v).unit(unit).build();
        let values = |s: &ScanSettings| s.params.iter().map(|p| (p.curie().unwrap().to_string(), p.value.to_f64().unwrap(), p.unit)).collect::<Vec<_>>();
        let mut counted = ScanSettings { id: "g".into(), ..Default::default() };
        counted.params.push(count(mzdata::curie!(IMS:1000042), "max count of pixels x", 260));
        counted.params.push(count(mzdata::curie!(IMS:1000043), "max count of pixels y", 134));
        let marker = |g: &ScanSettings, source: &str| marker_block(Some(g), COUNTS_DECLARED, LoneX::XOnly, source, serde_json::json!({}));
        assert_eq!(marker(&counted, SOURCE_DECLARED)["pixel_size_source"], "unknown", "no size, whatever the lane says");
        // Unsized: both sizes and both max dimensions written.
        let mut g = counted.clone();
        let row = apply_user_pixel_size(&mut g, &user(10.0, 10.0, false), "the test").unwrap().unwrap();
        assert_eq!(values(&g)[2..], [("IMS:1000046".into(), 10.0, Unit::Micrometer), ("IMS:1000047".into(), 10.0, Unit::Micrometer), ("IMS:1000044".into(), 2600.0, Unit::Micrometer), ("IMS:1000045".into(), 1340.0, Unit::Micrometer)]);
        assert_eq!((&row["transformation"], &row["overridden"], row["stated"].as_array().unwrap().len()), (&serde_json::json!(USER_SUPPLIED), &serde_json::json!(false), 0));
        assert_eq!(row["max_dimension"].as_array().unwrap().len(), 2, "{row:#}");
        assert_eq!(pixel_size_um(&g), Some((10.0, 10.0)));
        let m = marker(&g, SOURCE_USER_SUPPLIED);
        assert_eq!((&m["pixel_size_um"], &m["pixel_size_source"]), (&serde_json::json!({"x": 10.0, "y": 10.0}), &serde_json::json!("user_supplied")));
        // Without counts: the sizes alone.
        let mut bare = ScanSettings { id: "g".into(), ..Default::default() };
        apply_user_pixel_size(&mut bare, &user(10.0, 20.0, false), "the test").unwrap().unwrap();
        assert_eq!(values(&bare), [("IMS:1000046".into(), 10.0, Unit::Micrometer), ("IMS:1000047".into(), 20.0, Unit::Micrometer)]);
        // A stated size that agrees (in mm, to 1e-6): nothing written, no row.
        let mut stated = counted.clone();
        stated.params.push(size(mzdata::curie!(IMS:1000046), "pixel size (x)", 0.01, Unit::Millimeter));
        stated.params.push(size(mzdata::curie!(IMS:1000047), "pixel size y", 0.01, Unit::Millimeter));
        let before = stated.clone();
        assert!(apply_user_pixel_size(&mut stated, &user(10.0, 10.0, false), "the test").unwrap().is_none());
        assert_eq!(stated.params, before.params);
        // A differing one: refused naming both; --force writes over it and recomputes the max
        // dimension it states.
        let e = apply_user_pixel_size(&mut stated, &user(20.0, 20.0, false), "the test").unwrap_err().to_string();
        assert!(e.contains("--pixel-size 20 µm") && e.contains("the test states a pixel size that differs") && e.contains("pixel size (x) (IMS:1000046) = 10 µm") && e.contains("--force"), "{e}");
        assert_eq!(stated.params, before.params, "refused: untouched");
        stated.params.push(size(mzdata::curie!(IMS:1000044), "max dimension x", 2.6, Unit::Millimeter));
        let row = apply_user_pixel_size(&mut stated, &user(20.0, 20.0, true), "the test").unwrap().unwrap();
        assert_eq!(values(&stated)[2..], [("IMS:1000046".into(), 20.0, Unit::Micrometer), ("IMS:1000047".into(), 20.0, Unit::Micrometer), ("IMS:1000044".into(), 5200.0, Unit::Micrometer), ("IMS:1000045".into(), 2680.0, Unit::Micrometer)]);
        assert_eq!((&row["overridden"], row["stated"].as_array().unwrap().len()), (&serde_json::json!(true), 2));
        assert!(row["stated"][0]["value"].as_str().unwrap().contains("0.01"), "{row:#}");
        // A lone x that agrees: the y is filled, the x untouched.
        let mut lone = counted.clone();
        lone.params.push(size(mzdata::curie!(IMS:1000046), "pixel size (x)", 50.0, Unit::Micrometer));
        let row = apply_user_pixel_size(&mut lone, &user(50.0, 100.0, false), "the test").unwrap().unwrap();
        assert_eq!(values(&lone)[2..4], [("IMS:1000046".into(), 50.0, Unit::Micrometer), ("IMS:1000047".into(), 100.0, Unit::Micrometer)]);
        assert_eq!(row["written_um"].as_array().unwrap().len(), 1);
        assert!(row["detail"].as_str().unwrap().contains("states one axis, which agrees"), "{}", row["detail"]);
        // A lone x that disagrees is a contradiction like any other.
        let mut lone = counted.clone();
        lone.params.push(size(mzdata::curie!(IMS:1000046), "pixel size (x)", 50.0, Unit::Micrometer));
        assert!(apply_user_pixel_size(&mut lone, &user(20.0, 20.0, false), "the test").is_err());
        // A stated size that is no length in µm (zero) is a contradiction too, named as stated.
        let mut zero = counted.clone();
        zero.params.push(size(mzdata::curie!(IMS:1000046), "pixel size (x)", 0.0, Unit::Micrometer));
        let e = apply_user_pixel_size(&mut zero, &user(20.0, 20.0, false), "the test").unwrap_err().to_string();
        assert!(e.contains("= 0 UO:0000017"), "{e}");
        // Parsing.
        assert_eq!(UserPixelSize::parse("10", false).unwrap(), user(10.0, 10.0, false));
        assert_eq!(UserPixelSize::parse(" 10 , 20.5 ", true).unwrap(), user(10.0, 20.5, true));
        for bad in ["", "0", "-5", "abc", "1,2,3", "10,", "inf", "nan"] {
            assert!(UserPixelSize::parse(bad, false).is_err(), "{bad:?}");
        }
        assert_eq!(user(10.0, 10.0, false).text(), "10 µm");
        assert_eq!(user(10.0, 20.0, false).text(), "10 × 20 µm");
    }

    /// The obsolete integer type terms become the PSI-MS ones, `cvRef` with them, in place: the
    /// text keeps its length (an imzML may carry byte offsets), and nothing else is touched.
    #[test]
    fn obsolete_integer_type_terms_become_the_psi_ms_terms() {
        let text = r#"<referenceableParamGroup id="intensities">
  <cvParam cvRef="IMS" accession="IMS:1000141" name="32-bit integer" value=""/>
  <cvParam accession='IMS:1000142' cvRef='IMS' name='64-bit integer'/>
  <cvParam cvRef="IMS" accession="IMS:1000101" name="external data" value="true"/>
  <userParam name="note" value="was IMS:1000141"/>
</referenceableParamGroup>"#;
        let out = String::from_utf8(replace_obsolete_integer_terms(text.as_bytes()).unwrap()).unwrap();
        assert_eq!(out.len(), text.len());
        assert!(out.contains(r#"<cvParam cvRef="MS"  accession="MS:1000519"  name="32-bit integer" value=""/>"#), "{out}");
        assert!(out.contains("<cvParam accession='MS:1000522'  cvRef='MS'  name='64-bit integer'/>"), "{out}");
        assert!(out.contains(r#"<cvParam cvRef="IMS" accession="IMS:1000101""#) && out.contains(r#"value="was IMS:1000141""#), "{out}");
        assert_eq!(read_file_content_from(format!("<mzML><fileDescription><fileContent>{out}</fileContent></fileDescription></mzML>").as_bytes()).unwrap().len(), 3, "still well-formed");
        assert_eq!(replace_obsolete_integer_terms(br#"<cvParam cvRef="MS" accession="MS:1000519" name="32-bit integer"/><userParam value="IMS:1000141"/>"#), None);
    }

    /// The `.ibd` is hashed with each algorithm the header states, in one pass, and always with
    /// SHA-1 (its source-file digest); hex compares without case; a term stated without a value is
    /// not a stated checksum.
    #[test]
    fn the_ibd_is_hashed_with_the_stated_algorithm() {
        let dir = std::env::temp_dir().join(format!("mzpc-ibd-check-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ibd = dir.join("x.ibd");
        std::fs::write(&ibd, b"abc").unwrap();
        let content = |params: &[(&str, &str)]| -> Vec<RawParam> {
            params.iter().map(|(a, v)| RawParam { accession: a.to_string(), value: v.to_string(), unit_accession: None, unit_name: None }).collect()
        };
        // The FIPS 180 / RFC 1321 test vectors of "abc".
        const SHA1: &str = "a9993e364706816aba3e25717850c26c9cd0d89d";
        const MD5: &str = "900150983cd24fb0d6963f7d28e17f72";
        const SHA256: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        let all = check_ibd(&ibd, &content(&[("IMS:1000090", MD5), ("IMS:1000091", &SHA1.to_uppercase()), ("IMS:1000092", SHA256)])).unwrap();
        assert_eq!((all.sha1.as_str(), all.stated.len(), all.status(), all.found_json()), (SHA1, 3, "verified", None));
        let none = check_ibd(&ibd, &content(&[("IMS:1000031", ""), ("IMS:1000091", " ")])).unwrap();
        assert_eq!((none.sha1.as_str(), none.status(), none.found_json()), (SHA1, "not stated", None));
        for (acc, found) in [("IMS:1000090", MD5), ("IMS:1000091", SHA1), ("IMS:1000092", SHA256)] {
            let wrong = check_ibd(&ibd, &content(&[(acc, "00ff")])).unwrap();
            assert_eq!(wrong.status(), "mismatch", "{acc}");
            assert_eq!(wrong.found_json(), Some(serde_json::json!([{"accession": acc, "value": found}])));
        }
        assert_eq!(ibd_beside(&dir.join("x.imzML")), Some(ibd));
        assert_eq!(ibd_beside(&dir.join("y.imzML")), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The `.ibd`'s first 16 bytes against the UUID the header states, however it is spelt (braces,
    /// dashes, case); a header that states none, and a file shorter than a UUID.
    #[test]
    fn the_ibd_s_first_bytes_are_compared_with_the_stated_uuid() {
        let dir = std::env::temp_dir().join(format!("mzpc-ibd-uuid-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ibd = dir.join("x.ibd");
        let uuid: Vec<u8> = (0x10..0x20).collect();
        std::fs::write(&ibd, [&uuid[..], b"signal"].concat()).unwrap();
        let stating = |v: &str| vec![RawParam { accession: "IMS:1000080".into(), value: v.into(), unit_accession: None, unit_name: None }];
        const HEX: &str = "101112131415161718191a1b1c1d1e1f";
        for spelling in ["{10111213-1415-1617-1819-1A1B1C1D1E1F}", "10111213-1415-1617-1819-1a1b1c1d1e1f", HEX, " 101112131415161718191A1B1C1D1E1F "] {
            let c = check_ibd(&ibd, &stating(spelling)).unwrap();
            assert_eq!((c.uuid_status(), c.uuid_mismatch()), ("verified", None), "{spelling}");
        }
        let other = "{00111213-1415-1617-1819-1A1B1C1D1E1F}";
        let c = check_ibd(&ibd, &stating(other)).unwrap();
        assert_eq!((c.uuid_status(), c.uuid_mismatch()), ("mismatch", Some((other, HEX))));
        // The checksum's status is its own: nothing stated, whatever the UUID says.
        assert_eq!(c.status(), "not stated");
        for unstated in [Vec::new(), stating(""), stating("  ")] {
            let c = check_ibd(&ibd, &unstated).unwrap();
            assert_eq!((c.uuid_status(), c.uuid_mismatch()), ("not stated", None));
        }
        std::fs::write(&ibd, &uuid[..5]).unwrap();
        let c = check_ibd(&ibd, &stating(HEX)).unwrap();
        assert_eq!(c.uuid_mismatch(), Some((HEX, "1011121314")), "a file shorter than a UUID does not begin with it");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Spectra are counted as stating a scan start time when the term is on the spectrum or in a
    /// param group it references — not when it only sits in a group nothing references, and not
    /// beyond the spectra that were read.
    #[test]
    fn spectra_stating_a_scan_time_are_counted() {
        let time = r#"<cvParam cvRef="MS" accession="MS:1000016" name="scan start time" value="0"/>"#;
        let doc = format!(
            r#"<mzML><referenceableParamGroupList>
              <referenceableParamGroup id="timed">{time}</referenceableParamGroup>
              <referenceableParamGroup id="plain"><cvParam accession="MS:1000511" value="1"/></referenceableParamGroup>
            </referenceableParamGroupList><run><spectrumList>
              <spectrum id="a"><scanList><scan>{time}</scan></scanList></spectrum>
              <spectrum id="b"><scanList><scan><referenceableParamGroupRef ref="timed"/></scan></scanList></spectrum>
              <spectrum id="c"><referenceableParamGroupRef ref="plain"/><scanList><scan/></scanList></spectrum>
              <spectrum id="d"/>
              <spectrum id="e"><scanList><scan>{time}</scan></scanList></spectrum>
            </spectrumList><chromatogramList><chromatogram>{time}</chromatogram></chromatogramList></run></mzML>"#
        );
        assert_eq!(spectra_stating_time_from(doc.as_bytes(), usize::MAX).unwrap(), (3, 5));
        assert_eq!(spectra_stating_time_from(doc.as_bytes(), 4).unwrap(), (2, 4), "the first four only");
        let none = doc.replace("MS:1000016", "MS:1000017");
        assert_eq!(spectra_stating_time_from(none.as_bytes(), usize::MAX).unwrap(), (0, 5));
    }

    /// `pixel_size_um` is micrometres whatever length unit the grid states, and only a positive
    /// size: 0.1 mm and 100000 nm are 100 µm; a zero or negative size gives the marker none.
    #[test]
    fn the_marker_s_pixel_size_is_micrometres_from_any_length_unit() {
        let grid = |x: f64, y: f64, unit: Unit| {
            let mut s = ScanSettings { id: "grid".into(), ..Default::default() };
            let size = |name: &str, acc, v: f64| Param::builder().name(name).curie(acc).value(v).unit(unit).build();
            s.add_param(Param::builder().name("max count of pixels x").curie(mzdata::curie!(IMS:1000042)).value(3).build());
            s.add_param(Param::builder().name("max count of pixels y").curie(mzdata::curie!(IMS:1000043)).value(3).build());
            s.add_param(size("pixel size (x)", mzdata::curie!(IMS:1000046), x));
            s.add_param(size("pixel size y", mzdata::curie!(IMS:1000047), y));
            s
        };
        let marker = |s: &ScanSettings| marker_block(Some(s), COUNTS_DECLARED, LoneX::AlsoY, SOURCE_DECLARED, serde_json::json!({}));
        let um = serde_json::json!({"x": 100.0, "y": 50.0});
        for (x, y, unit) in [(100.0, 50.0, Unit::Micrometer), (0.1, 0.05, Unit::Millimeter), (100000.0, 50000.0, Unit::Nanometer), (0.01, 0.005, Unit::Centimeter)] {
            assert_eq!(marker(&grid(x, y, unit))["pixel_size_um"], um, "{x} {unit:?}");
        }
        for (x, y, unit) in [(0.0, 100.0, Unit::Micrometer), (100.0, -100.0, Unit::Micrometer), (f64::NAN, 100.0, Unit::Micrometer), (100.0, 100.0, Unit::Second), (100.0, 100.0, Unit::Unknown)] {
            assert!(marker(&grid(x, y, unit)).get("pixel_size_um").is_none(), "{x} {y} {unit:?}");
        }
    }

    /// x and y both stated, one of them zero or negative: no pixel size, dropped and declared, as a
    /// lone value that is not positive always was.
    #[test]
    fn x_and_y_that_are_not_both_positive_are_dropped() {
        for (x, y) in [("0", "-100"), ("100", "0"), ("-5", "5")] {
            let f = pixel_size_fix(&settings(&[("IMS:1000046", x, UM), ("IMS:1000047", y, UM)])).unwrap();
            assert_eq!((f.case, f.transformations.clone(), f.write.len()), ("x and y not both positive", vec![DROPPED], 0), "{x} {y}");
        }
    }
}
