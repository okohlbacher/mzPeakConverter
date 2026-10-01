//! Bruker MALDI imaging: per-frame raster positions from `MaldiFrameInfo` (timsTOF fleX, TSF and
//! TDF), so an imaging run converted straight from the `.d` can be rebuilt as an image — until now
//! only the imzML path wrote positions (HUPO-PSI/mzPeak-specification#23).
//!
//! * `XIndexPos` / `YIndexPos` become `IMS:1000050` / `IMS:1000051` on the frame's scan, the same
//!   params — and so the same `position_x` / `position_y` columns — as the imzML path. Bruker's
//!   indices are absolute on the target (669–837 in the issue author's file); the archive counts
//!   from 1 (owner decision 2026-09-30), so every index is shifted by the smallest one of the run —
//!   one shift for all regions, which keeps them where they lie relative to each other — and the
//!   shift is declared ([`SHIFTED_TO_BASE_1`]) and recorded (`origin` in the `bruker_maldi` block).
//!   FlexImaging's own imzML export writes the indices unshifted, with the pixel counts set to the
//!   largest index; the two differ by exactly `origin − 1`.
//! * The grid: `IMS:1000042/43` pixel counts (the shifted extent), always.
//! * The `bruker_maldi` index block keeps what has no mzPeak home yet: the regions
//!   (`RegionNumber`, frames and index ranges each), the raw index ranges, the beam scan size.
//! * Pixel size: the raster step of the FlexImaging sequence, `<stem>.mis` beside the `.d` (not part
//!   of it) — `RegionNumber` n is the n-th `<Area>`, each with its `<Raster>` in µm and its name —
//!   when every acquired region has the same step; regions on different steps get no pixel size (the
//!   profile describes one grid). A `.mis` whose areas the regions do not map onto is not used
//!   ([`MaldiInfo::mis_mismatch`]). Without a usable `.mis`, the frames' `BeamScanSizeX/Y` (µm) is
//!   the fallback, declared as such ([`PIXEL_FROM_BEAM`]), and only when every positioned frame
//!   states the same finite size. Either way written as `IMS:1000046/47`, with `IMS:1000044/45` max
//!   dimension = count × size.
//! * The acquisition region: each positioned frame's `RegionNumber` is a parameter of its scan
//!   ([`REGION_PARAM`], no accession: the imaging profile names no region column yet), which the
//!   block's `regions` list maps to the region's name. The bounding boxes there cannot tell the
//!   regions apart once two overlap, and two of the four MSV000088438 areas are polygons.
//! * A frame without a `MaldiFrameInfo` row, or with a NULL index, has no position: its spectrum is
//!   written with null `position_x` / `position_y`, which the profile allows; such frames are
//!   counted (`frames_without_position`) and warned about once ([`read_dot_d`]). An empty frame
//!   (`NumPeaks = 0`) that has a row keeps its pixel.
//!
//! Column names are Bruker's (`MaldiFrameInfo(Frame, …, RegionNumber, XIndexPos, YIndexPos, …,
//! LaserInfo)`), read off MassIVE MSV000088438 (TSF schema 3.3, TDF schema 3.5). There the beam scan
//! size is not a column of `MaldiFrameInfo` but of `MaldiFrameLaserInfo(Id, …, BeamScan,
//! BeamScanSizeX, BeamScanSizeY, …)`, which `MaldiFrameInfo.LaserInfo` references, and `BeamScan = 0`
//! (both runs) says the beam was not scanned: the sizes beside it (0.0) are then no pixel size. A
//! schema with `BeamScanSizeX/Y` on `MaldiFrameInfo` itself, as the issue author described his, is
//! read from there. The corpus holds no MALDI run, so the tests build the tables.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use mzdata::meta::ScanSettings;
use mzdata::params::{Param, ParamDescribed, Unit};
use mzdata::prelude::*;
use mzdata::spectrum::{MultiLayerSpectrum, ScanEvent};
use rusqlite::Connection;
use quick_xml::events::Event;

pub const PIXEL_FROM_BEAM: &str = "bruker:pixel-size-from-beam-scan-size";

/// Where a Bruker MALDI run's pixel size came from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PixelSource {
    /// The FlexImaging `.mis` raster step.
    Mis,
    /// The frames' `BeamScanSizeX/Y` (declared, [`PIXEL_FROM_BEAM`]).
    Beam,
}
/// Positions are the raster indices minus the run's smallest index plus 1.
pub const SHIFTED_TO_BASE_1: &str = "bruker:raster-index-shifted-to-base-1";
/// The name of the scan parameter holding a frame's `MaldiFrameInfo.RegionNumber`. A parameter
/// without an accession, in the scan's `parameters` list: the imaging profile has a region column
/// as an open item (HUPO-PSI/mzPeak-specification#25) and no term names one.
pub const REGION_PARAM: &str = "acquisition region";

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spot {
    pub x: i64,
    pub y: i64,
    pub region: Option<i64>,
    /// `MotorPositionX/Y`, stage µm, when stated: what [`MaldiInfo::mis_mismatch`] checks the
    /// `.mis` areas against.
    pub motor: Option<(f64, f64)>,
}

#[derive(Debug, Clone, Default)]
pub struct MaldiInfo {
    /// Keyed by `Frames.Id`.
    pub spots: HashMap<i64, Spot>,
    /// Every distinct finite, positive `(BeamScanSizeX, BeamScanSizeY)` stated, in µm.
    pub beam: Vec<(f64, f64)>,
    /// Positioned frames whose beam scan size is NULL, no number, not finite or not positive: one is
    /// enough to rule the beam fallback out (review 2026-09-30 B16: NULLs were skipped and +inf
    /// passed).
    pub beam_unstated: usize,
    /// Where the beam scan size was read: `MaldiFrameInfo`'s own columns, `MaldiFrameLaserInfo`
    /// through `LaserInfo`, or nowhere (neither has the columns).
    pub beam_source: Option<&'static str>,
    /// Frames with no position: of the `Frames` table, those without a `MaldiFrameInfo` row or with
    /// a NULL `XIndexPos` / `YIndexPos` there (without a `Frames` table, the rows with a NULL index).
    pub unpositioned: usize,
    /// Smallest and largest `(XIndexPos, YIndexPos)` of the run: `min` becomes position (1, 1).
    pub min: (i64, i64),
    pub max: (i64, i64),
    /// The FlexImaging sequence beside the `.d`, if there is one and it is used.
    pub mis: Option<Mis>,
    /// A `.mis` beside the `.d` that is not used, and why ([`Self::mis_mismatch`]).
    pub mis_rejected: Option<(String, String)>,
}

/// One `<Area>` of a FlexImaging `.mis`: its name, raster step (µm) and outline.
#[derive(Debug, Clone, PartialEq)]
pub struct MisArea {
    pub name: Option<String>,
    pub raster: Option<(f64, f64)>,
    /// The corners (image px) of a rectangle (`Type="0"`: its two stated corners, expanded to all
    /// four) or polygon (`Type="3"`: its `<Point>`s); empty for any other type, whose outline is not
    /// known here.
    pub points: Vec<(f64, f64)>,
}

/// A FlexImaging sequence: its areas in file order. `MaldiFrameInfo.RegionNumber` n is the n-th
/// `<Area>` — checked on MassIVE MSV000088438 against timsControl's poslog (`R00`…) and flexImaging's
/// spot list (region names).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Mis {
    pub file: String,
    pub areas: Vec<MisArea>,
    /// The `<TeachPoint>`s, `imgx,imgy;stagex,stagey`: image px and stage µm of the same point.
    pub teach: Vec<((f64, f64), (f64, f64))>,
}

/// Read a `.mis`; `None` when it cannot be read or has no `<Area>`.
pub fn read_mis(path: &Path) -> Option<Mis> {
    let f = std::fs::File::open(path).ok()?;
    let file = path.file_name()?.to_string_lossy().into_owned();
    read_mis_from(&file, std::io::BufReader::new(f))
}

pub fn read_mis_from(file: &str, input: impl std::io::BufRead) -> Option<Mis> {
    let mut reader = quick_xml::Reader::from_reader(input);
    let mut buf = Vec::new();
    let mut mis = Mis { file: file.into(), ..Default::default() };
    // `in_area`, whether its `<Point>`s are an outline (rectangle or polygon), whether a rectangle,
    // and the element whose text is read.
    let (mut in_area, mut outline, mut rect, mut element) = (false, false, false, Vec::new());
    let pair = |s: &str| {
        let mut v = s.split(',').map(|v| v.trim().parse::<f64>().ok().filter(|v| v.is_finite()));
        Some((v.next()??, v.next()??))
    };
    loop {
        match reader.read_event_into(&mut buf).ok()? {
            Event::Start(e) if e.local_name().as_ref() == b"Area" => {
                in_area = true;
                let kind = crate::imaging::attr(&e, b"Type");
                outline = matches!(kind.as_deref(), Some("0" | "3"));
                rect = kind.as_deref() == Some("0");
                mis.areas.push(MisArea { name: crate::imaging::attr(&e, b"Name"), raster: None, points: Vec::new() });
            }
            Event::Empty(e) if e.local_name().as_ref() == b"Area" => {
                mis.areas.push(MisArea { name: crate::imaging::attr(&e, b"Name"), raster: None, points: Vec::new() });
            }
            Event::Start(e) => element = e.local_name().as_ref().to_vec(),
            Event::Text(t) => {
                let text = String::from_utf8_lossy(&t).into_owned();
                match (element.as_slice(), mis.areas.last_mut()) {
                    (b"Raster", Some(a)) if in_area => a.raster = pair(&text).filter(|(x, y)| *x > 0.0 && *y > 0.0),
                    (b"Point", Some(a)) if in_area && outline => a.points.extend(pair(&text)),
                    (b"TeachPoint", _) => {
                        if let Some((image, stage)) = text.split_once(';') {
                            mis.teach.extend(pair(image).zip(pair(stage)));
                        }
                    }
                    _ => {}
                }
            }
            Event::End(e) => {
                if e.local_name().as_ref() == b"Area" {
                    in_area = false;
                    // Two opposite corners in image px; the teach-point map rotates and shears
                    // (both MSV000088438 maps do), so the other two can lie outside the stage box
                    // of these.
                    if let (true, Some(a)) = (rect, mis.areas.last_mut()) {
                        if let [(x1, y1), (x2, y2)] = a.points[..] {
                            a.points = vec![(x1, y1), (x2, y1), (x2, y2), (x1, y2)];
                        }
                    }
                }
                element.clear();
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    (!mis.areas.is_empty()).then_some(mis)
}

/// `MaldiFrameInfo` of an open TSF/TDF database; `None` without the table or its position columns.
pub fn read(conn: &Connection) -> Option<MaldiInfo> {
    let columns = |table: &str| -> Vec<String> {
        let Ok(mut stmt) = conn.prepare("SELECT name FROM pragma_table_info(?1)") else { return Vec::new() };
        stmt.query_map([table], |r| r.get::<_, String>(0)).map(|rows| rows.flatten().collect()).unwrap_or_default()
    };
    let cols = columns("MaldiFrameInfo");
    let has = |c: &str| cols.iter().any(|x| x == c);
    if !(has("Frame") && has("XIndexPos") && has("YIndexPos")) {
        return None;
    }
    let opt = |c: &str| if has(c) { format!("m.{c}") } else { "NULL".to_string() };
    // The beam scan size: on `MaldiFrameInfo` itself where that has the columns; else in
    // `MaldiFrameLaserInfo`, the row `LaserInfo` names (both MSV000088438 runs — through 0.16.0 only
    // the first was tried, so the fallback could not fire on a real file). `BeamScan = 0` there
    // means the beam was not scanned: the sizes beside it are unstated.
    let laser = columns("MaldiFrameLaserInfo");
    let in_laser = |c: &str| laser.iter().any(|x| x == c);
    let (beam_x, beam_y, join, beam_source) = if has("BeamScanSizeX") || has("BeamScanSizeY") {
        (opt("BeamScanSizeX"), opt("BeamScanSizeY"), "", Some("MaldiFrameInfo.BeamScanSizeX/Y"))
    } else if has("LaserInfo") && in_laser("Id") && in_laser("BeamScanSizeX") && in_laser("BeamScanSizeY") {
        let scanned = |c: &str| if in_laser("BeamScan") { format!("CASE WHEN l.BeamScan = 0 THEN NULL ELSE l.{c} END") } else { format!("l.{c}") };
        (
            scanned("BeamScanSizeX"),
            scanned("BeamScanSizeY"),
            " LEFT JOIN MaldiFrameLaserInfo l ON l.Id = m.LaserInfo",
            Some("MaldiFrameLaserInfo.BeamScanSizeX/Y of the row MaldiFrameInfo.LaserInfo names (unstated where BeamScan = 0)"),
        )
    } else {
        ("NULL".to_string(), "NULL".to_string(), "", None)
    };
    let sql = format!(
        "SELECT m.Frame, m.XIndexPos, m.YIndexPos, {}, {beam_x}, {beam_y}, {}, {} FROM MaldiFrameInfo m{join}",
        opt("RegionNumber"),
        opt("MotorPositionX"),
        opt("MotorPositionY")
    );
    let mut stmt = conn.prepare(&sql).ok()?;
    let rows = stmt
        .query_map([], |r| {
            // Beam size and motor position only feed the pixel size and the .mis check: a value that
            // is no number (text 'n/a' in a REAL column) is unstated there, not a lost row — the
            // frame keeps its position.
            let f = |i| r.get::<_, Option<f64>>(i).ok().flatten();
            Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?, r.get::<_, Option<i64>>(2)?, r.get::<_, Option<i64>>(3)?, f(4), f(5), f(6), f(7)))
        })
        .ok()?;
    let mut info = MaldiInfo { beam_source, ..Default::default() };
    for (frame, x, y, region, bx, by, mx, my) in rows.flatten() {
        let (Some(x), Some(y)) = (x, y) else {
            info.unpositioned += 1;
            continue;
        };
        let motor = mx.zip(my).filter(|(x, y)| x.is_finite() && y.is_finite());
        info.spots.insert(frame, Spot { x, y, region, motor });
        match bx.zip(by).filter(|&(bx, by)| bx.is_finite() && by.is_finite() && bx > 0.0 && by > 0.0) {
            Some(b) if !info.beam.contains(&b) => info.beam.push(b),
            Some(_) => {}
            None => info.beam_unstated += 1,
        }
    }
    // Every frame is a spectrum, with or without a `MaldiFrameInfo` row: count against `Frames`.
    if let Ok(mut frames) = conn.prepare("SELECT Id FROM Frames") {
        if let Ok(ids) = frames.query_map([], |r| r.get::<_, i64>(0)) {
            info.unpositioned = ids.flatten().filter(|id| !info.spots.contains_key(id)).count();
        }
    }
    let (xs, ys) = (info.spots.values().map(|s| s.x), info.spots.values().map(|s| s.y));
    info.min = (xs.clone().min()?, ys.clone().min()?);
    info.max = (xs.max()?, ys.max()?);
    Some(info)
}

/// [`read`] on a Bruker `.d` (its `analysis.tsf`, else `analysis.tdf`), with the `<stem>.mis` beside
/// it; `None` for anything else.
pub fn read_dot_d(dot_d: &Path) -> Option<MaldiInfo> {
    let mut info = ["analysis.tsf", "analysis.tdf"].iter().find_map(|name| {
        let db = dot_d.join(name);
        if !std::fs::metadata(&db).is_ok_and(|m| m.is_file() && m.len() > 0) {
            return None;
        }
        read(&crate::vendor_sqlite::open(&db).ok()?)
    })?;
    info.mis = read_mis(&dot_d.with_extension("mis"));
    info.check_mis();
    // Once per conversion: every lane reads the run's positions here, once.
    if info.unpositioned > 0 {
        log::warn!(
            "Bruker MALDI: {} of {} frames have no raster position (no MaldiFrameInfo row, or a NULL \
             XIndexPos/YIndexPos); their spectra are written with a null position",
            info.unpositioned,
            info.spots.len() + info.unpositioned
        );
    }
    Some(info)
}

impl MaldiInfo {
    /// Put the frame's position, and its region when the frame states one ([`REGION_PARAM`]), on the
    /// spectrum's first scan. Spectra are matched by the `frame=<Frames.Id>` token of their id: the
    /// whole id on the TSF, native TDF and SDK lanes, `merged=… frame=… startScan=…` through mzdata.
    pub fn attach(&self, spec: &mut MultiLayerSpectrum) -> bool {
        let frame = spec.id().split_whitespace().find_map(|t| t.strip_prefix("frame=")?.parse::<i64>().ok());
        let Some(frame) = frame else { return false };
        let Some(spot) = self.spots.get(&frame) else { return false };
        let scans = &mut spec.description_mut().acquisition.scans;
        if scans.is_empty() {
            scans.push(ScanEvent::default());
        }
        let (x, y) = (spot.x - self.min.0 + 1, spot.y - self.min.1 + 1);
        scans[0].add_param(Param::builder().name("position x").curie(mzdata::curie!(IMS:1000050)).value(x).build());
        scans[0].add_param(Param::builder().name("position y").curie(mzdata::curie!(IMS:1000051)).value(y).build());
        if let Some(region) = spot.region {
            scans[0].add_param(Param::builder().name(REGION_PARAM).value(region).build());
        }
        true
    }

    /// Pixel counts of the shifted grid.
    pub fn count(&self) -> (i64, i64) {
        (self.max.0 - self.min.0 + 1, self.max.1 - self.min.1 + 1)
    }

    /// Pixel size in µm and its source: the `.mis` raster step when every acquired region maps to an
    /// area with one and they agree (none when they differ); without a usable `.mis`, the beam scan
    /// size when every positioned frame states the same finite one.
    pub fn pixel_size_from(&self) -> Option<((f64, f64), PixelSource)> {
        if let Some(steps) = self.mis_rasters() {
            let [step] = steps.as_slice() else { return None };
            return Some((*step, PixelSource::Mis));
        }
        let [b] = self.beam.as_slice() else { return None };
        (self.beam_unstated == 0).then_some((*b, PixelSource::Beam))
    }

    /// Why `mis` does not describe these regions, if it does not. The mapping is by `<Area>` order
    /// alone — right on both real datasets, but a re-saved `.mis` would misattribute names and steps
    /// silently (review 2026-09-30 B16) — so two checks:
    /// * every `RegionNumber` is an area: 0 ≤ n < the number of `<Area>`s;
    /// * the regions lie where their areas are. The first three `<TeachPoint>`s fix the affine map
    ///   image px → stage µm, which puts each area's corners (a rectangle's four, a polygon's
    ///   vertices) on the stage; their bounding box there holds the whole area. `MotorPositionX/Y`
    ///   are stage µm as well, but from another origin — a translation stated in neither file
    ///   (+54000.8, −45642.5 µm on both MSV000088438 runs, by flexImaging's spot list) — so the test
    ///   is whether ONE translation puts every region's spots inside its area's stage bounding box,
    ///   within half a raster step: per axis, the intersection of each region's interval of
    ///   translations. On MSV000088438 the file's mapping fits and every permutation of the areas
    ///   misses by ≥ 5 mm.
    ///
    /// ponytail: the area's stage bounding box, not its shape (a rotated rectangle or a polygon is
    /// smaller), and a translation, not a fitted affine — enough to catch areas shifted, swapped or
    /// deleted by a re-save; a mapping that keeps every region inside its box passes. Regions
    /// without motor positions, areas of another type and a `.mis` without three teach points are
    /// not tested.
    pub fn mis_mismatch(&self, mis: &Mis) -> Option<String> {
        let unmapped = |r: i64| usize::try_from(r).map_or(true, |a| a >= mis.areas.len());
        // The largest unmapped RegionNumber: the reason is recorded, so it must not follow hash order.
        if let Some(r) = self.spots.values().filter_map(|s| s.region).filter(|&r| unmapped(r)).max() {
            return Some(format!("RegionNumber {r} has no <Area> (the file has {})", mis.areas.len()));
        }
        let mut regions: BTreeMap<usize, Vec<(f64, f64)>> = BTreeMap::new();
        for s in self.spots.values() {
            if let Some(r) = s.region {
                regions.entry(r as usize).or_default().extend(s.motor); // 0 ≤ r < areas, checked above
            }
        }
        let Some(&[(a, sa), (b, sb), (c, sc)]) = mis.teach.get(..3) else { return None };
        let d = (b.1 - c.1) * (a.0 - c.0) + (c.0 - b.0) * (a.1 - c.1);
        if d.abs() < 1e-6 {
            return None; // collinear teach points: no map
        }
        // Barycentric coordinates in the image triangle, applied to the stage triangle.
        let stage = |(u, v): (f64, f64)| {
            let l1 = ((b.1 - c.1) * (u - c.0) + (c.0 - b.0) * (v - c.1)) / d;
            let l2 = ((c.1 - a.1) * (u - c.0) + (a.0 - c.0) * (v - c.1)) / d;
            let l3 = 1.0 - l1 - l2;
            [l1 * sa.0 + l2 * sb.0 + l3 * sc.0, l1 * sa.1 + l2 * sb.1 + l3 * sc.1]
        };
        let bounds = |v: &mut dyn Iterator<Item = f64>| v.fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), x| (lo.min(x), hi.max(x)));
        let (mut lo, mut hi) = ([f64::NEG_INFINITY; 2], [f64::INFINITY; 2]);
        for (&r, spots) in &regions {
            let area = &mis.areas[r];
            if area.points.is_empty() || spots.is_empty() {
                continue;
            }
            let corners: Vec<[f64; 2]> = area.points.iter().map(|&p| stage(p)).collect();
            let spots: Vec<[f64; 2]> = spots.iter().map(|&(x, y)| [x, y]).collect();
            let tol = area.raster.map_or([0.0; 2], |(x, y)| [x / 2.0, y / 2.0]);
            for k in 0..2 {
                let (a_lo, a_hi) = bounds(&mut corners.iter().map(|p| p[k]));
                let (s_lo, s_hi) = bounds(&mut spots.iter().map(|p| p[k]));
                lo[k] = lo[k].max(a_lo - s_lo - tol[k]);
                hi[k] = hi[k].min(a_hi - s_hi + tol[k]);
            }
        }
        (lo[0] > hi[0] || lo[1] > hi[1]).then(|| {
            "the regions' MotorPositionX/Y do not lie inside their <Area> outlines (placed by the teach points) under one common offset".into()
        })
    }

    /// Stop using the `.mis` when [`Self::mis_mismatch`] finds it does not describe these regions:
    /// neither its names nor its raster steps; warned, and the reason kept for the block.
    pub fn check_mis(&mut self) {
        let Some(reason) = self.mis.as_ref().and_then(|m| self.mis_mismatch(m)) else { return };
        let file = self.mis.take().map(|m| m.file).unwrap_or_default();
        log::warn!("Bruker MALDI: {file} beside the .d is not used: {reason}");
        self.mis_rejected = Some((file, reason));
    }

    pub fn pixel_size(&self) -> Option<(f64, f64)> {
        self.pixel_size_from().map(|(p, _)| p)
    }

    /// The distinct raster steps of the acquired regions; `None` when there is no `.mis` or a region
    /// does not map to an `<Area>` with a step.
    fn mis_rasters(&self) -> Option<Vec<(f64, f64)>> {
        let mis = self.mis.as_ref()?;
        let mut steps = Vec::new();
        for r in self.spots.values().map(|s| s.region).collect::<std::collections::BTreeSet<_>>() {
            let step = mis.areas.get(usize::try_from(r?).ok()?)?.raster?;
            if !steps.contains(&step) {
                steps.push(step);
            }
        }
        Some(steps)
    }

    fn pixel_size_note(&self) -> String {
        let no_mis = match (&self.mis, &self.mis_rejected) {
            (_, Some((file, reason))) => format!("{file} not used: {reason}"),
            (Some(m), _) => format!("{} gives no raster step for every region", m.file),
            _ => "no FlexImaging .mis beside the .d".into(),
        };
        match (self.pixel_size_from(), &self.mis) {
            (Some((_, PixelSource::Mis)), Some(m)) => format!("the raster step in {}", m.file),
            (Some((_, PixelSource::Beam)), _) => format!("the beam scan size ({no_mis})"),
            (None, Some(m)) if self.mis_rasters().is_some() => format!("not written: the regions of {} have different raster steps", m.file),
            _ => format!("not written: {no_mis}, and the positioned frames do not all state one finite beam scan size"),
        }
    }

    /// The name the `.mis` gives region `r`.
    fn region_name(&self, r: Option<i64>) -> Option<&str> {
        self.mis.as_ref()?.areas.get(usize::try_from(r?).ok()?)?.name.as_deref()
    }

    /// The grid: pixel counts always; pixel size and max dimension when [`Self::pixel_size`] is known.
    pub fn scan_settings(&self) -> ScanSettings {
        let (nx, ny) = self.count();
        let mut s = ScanSettings { id: "scansettings1".into(), ..Default::default() };
        let p = |name: &str, curie, v: mzdata::params::Value, unit| Param::builder().name(name).curie(curie).value(v).unit(unit).build();
        s.params.push(p("max count of pixels x", mzdata::curie!(IMS:1000042), nx.into(), Unit::Unknown));
        s.params.push(p("max count of pixels y", mzdata::curie!(IMS:1000043), ny.into(), Unit::Unknown));
        if let Some((bx, by)) = self.pixel_size() {
            s.params.push(p("pixel size (x)", mzdata::curie!(IMS:1000046), bx.into(), Unit::Micrometer));
            s.params.push(p("pixel size y", mzdata::curie!(IMS:1000047), by.into(), Unit::Micrometer));
            s.params.push(p("max dimension x", mzdata::curie!(IMS:1000044), (nx as f64 * bx).into(), Unit::Micrometer));
            s.params.push(p("max dimension y", mzdata::curie!(IMS:1000045), (ny as f64 * by).into(), Unit::Micrometer));
        }
        s
    }

    /// The `metadata.imaging` marker block, the `bruker_maldi` block, and the transformations to
    /// declare.
    pub fn index_blocks(&self) -> (Vec<(String, serde_json::Value)>, Vec<&'static str>) {
        let mut applied = Vec::new();
        if self.min != (1, 1) {
            applied.push(SHIFTED_TO_BASE_1);
        }
        if matches!(self.pixel_size_from(), Some((_, PixelSource::Beam))) {
            applied.push(PIXEL_FROM_BEAM);
        }
        let mut marker = crate::imaging::marker_block(
            Some(&self.scan_settings()),
            crate::imaging::COUNTS_OBSERVED_MAX,
            serde_json::json!({
                "detected_from": "MaldiFrameInfo in analysis.tsf/.tdf",
                "positions": "XIndexPos/YIndexPos − position_offset",
                "pixel_size": self.pixel_size_note(),
            }),
        );
        // The imaging profile's record of the shift: the constant SUBTRACTED from each source index
        // (origin − 1), absent when nothing moved.
        if self.min != (1, 1) {
            marker["position_offset"] = serde_json::json!({"x": self.min.0 - 1, "y": self.min.1 - 1});
        }
        (vec![("imaging".into(), marker), ("bruker_maldi".into(), self.block())], applied)
    }

    /// The `bruker_maldi` index block.
    pub fn block(&self) -> serde_json::Value {
        let range = |f: fn(&Spot) -> i64, spots: &mut dyn Iterator<Item = &Spot>| {
            spots.map(f).fold(None, |acc: Option<(i64, i64)>, v| Some(acc.map_or((v, v), |(lo, hi)| (lo.min(v), hi.max(v)))))
        };
        let mut regions: BTreeMap<Option<i64>, Vec<&Spot>> = BTreeMap::new();
        for s in self.spots.values() {
            regions.entry(s.region).or_default().push(s);
        }
        serde_json::json!({
            "source": "analysis.tsf/.tdf MaldiFrameInfo (XIndexPos, YIndexPos, RegionNumber); the beam scan size as beam_scan_size_source says",
            "coordinates": "positions are XIndexPos/YIndexPos − origin + 1 (metadata.imaging.position_offset = origin − 1); x_index/y_index and the regions give the raw indices",
            "origin": {"x": self.min.0, "y": self.min.1},
            "frames_with_position": self.spots.len(),
            "frames_without_position": self.unpositioned,
            "x_index": range(|s| s.x, &mut self.spots.values()),
            "y_index": range(|s| s.y, &mut self.spots.values()),
            "mis": self.mis.as_ref().map(|m| &m.file),
            "mis_rejected": self.mis_rejected.as_ref().map(|(file, reason)| serde_json::json!({"file": file, "reason": reason})),
            "region_parameter": self.spots.values().any(|s| s.region.is_some()).then(|| format!("each positioned frame's scan states its region_number as the parameter '{REGION_PARAM}'")),
            "regions": regions.iter().map(|(r, spots)| serde_json::json!({
                "region_number": r,
                "name": self.region_name(*r),
                "raster_step_um": self.mis.as_ref().and_then(|m| m.areas.get(usize::try_from((*r)?).ok()?)?.raster).map(|(x, y)| serde_json::json!({"x": x, "y": y})),
                "frames": spots.len(),
                "x_index": range(|s| s.x, &mut spots.iter().copied()),
                "y_index": range(|s| s.y, &mut spots.iter().copied()),
            })).collect::<Vec<_>>(),
            "beam_scan_size_um": self.beam.iter().map(|(x, y)| serde_json::json!({"x": x, "y": y})).collect::<Vec<_>>(),
            "beam_scan_size_source": self.beam_source,
            "frames_without_beam_scan_size": self.beam_unstated,
            "pixel_size": self.pixel_size_note(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn maldi_table(conn: &Connection) {
        conn.execute_batch(
            "CREATE TABLE MaldiFrameInfo (Frame INTEGER PRIMARY KEY, Chip INTEGER, SpotName TEXT, RegionNumber INTEGER,
                                          XIndexPos INTEGER, YIndexPos INTEGER, MotorPositionX REAL, MotorPositionY REAL,
                                          BeamScanSizeX REAL, BeamScanSizeY REAL);
             INSERT INTO MaldiFrameInfo VALUES (1, 0, 'R00X669Y700', 0, 669, 700, 1.0, 2.0, 20.0, 20.0);
             INSERT INTO MaldiFrameInfo VALUES (2, 0, 'R00X670Y700', 0, 670, 700, 1.0, 2.0, 20.0, 20.0);
             INSERT INTO MaldiFrameInfo VALUES (3, 0, 'R01X837Y812', 1, 837, 812, 1.0, 2.0, 20.0, 20.0);",
        )
        .unwrap();
    }

    #[test]
    fn positions_regions_and_beam_are_read() {
        let c = Connection::open_in_memory().unwrap();
        assert!(read(&c).is_none(), "no table, no imaging");
        maldi_table(&c);
        let info = read(&c).unwrap();
        assert_eq!(info.spots[&1], Spot { x: 669, y: 700, region: Some(0), motor: Some((1.0, 2.0)) });
        assert_eq!(info.spots[&3], Spot { x: 837, y: 812, region: Some(1), motor: Some((1.0, 2.0)) });
        assert_eq!(info.beam, vec![(20.0, 20.0)]);
        assert_eq!((info.min, info.max, info.count()), ((669, 700), (837, 812), (169, 113)));
        let b = info.block();
        assert_eq!(b["x_index"], serde_json::json!([669, 837]), "the block keeps the raw indices");
        assert_eq!(b["origin"], serde_json::json!({"x": 669, "y": 700}));
        assert_eq!(b["regions"].as_array().unwrap().len(), 2);
        let ss = info.scan_settings();
        let get = |acc: &str| ss.params.iter().find(|p| p.curie().unwrap().to_string() == acc).map(|p| (p.value.to_f64().unwrap(), p.unit));
        assert_eq!(get("IMS:1000042"), Some((169.0, Unit::Unknown)));
        assert_eq!(get("IMS:1000043"), Some((113.0, Unit::Unknown)));
        assert_eq!(get("IMS:1000046"), Some((20.0, Unit::Micrometer)));
        assert_eq!(get("IMS:1000044"), Some((3380.0, Unit::Micrometer)));
        assert_eq!(get("IMS:1000045"), Some((2260.0, Unit::Micrometer)));
        let (blocks, applied) = info.index_blocks();
        assert_eq!(applied, vec![SHIFTED_TO_BASE_1, PIXEL_FROM_BEAM]);
        let marker = &blocks[0].1;
        assert_eq!((blocks[0].0.as_str(), &marker["is_imaging"], &marker["coordinate_base"]), ("imaging", &serde_json::json!(true), &serde_json::json!(1)));
        assert_eq!(marker["pixel_count"], serde_json::json!({"x": 169, "y": 113}));
        assert_eq!(marker["pixel_size_um"], serde_json::json!({"x": 20.0, "y": 20.0}));
        assert_eq!(marker["position_offset"], serde_json::json!({"x": 668, "y": 699}), "the constant subtracted, origin − 1");
    }

    #[test]
    fn a_run_already_counting_from_1_is_not_shifted() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE MaldiFrameInfo (Frame INTEGER PRIMARY KEY, XIndexPos INTEGER, YIndexPos INTEGER);
             INSERT INTO MaldiFrameInfo VALUES (1, 1, 1), (2, 2, 1);",
        )
        .unwrap();
        let (blocks, applied) = read(&c).unwrap().index_blocks();
        assert!(blocks[0].1.get("position_offset").is_none(), "absent when nothing was shifted");
        assert!(applied.is_empty());
    }

    #[test]
    fn without_a_single_beam_size_only_the_counts_are_written() {
        let c = Connection::open_in_memory().unwrap();
        maldi_table(&c);
        c.execute_batch("UPDATE MaldiFrameInfo SET BeamScanSizeX = 10.0 WHERE Frame = 3;").unwrap();
        let info = read(&c).unwrap();
        assert!(info.pixel_size().is_none());
        let accs: Vec<String> = info.scan_settings().params.iter().map(|p| p.curie().unwrap().to_string()).collect();
        assert_eq!(accs, ["IMS:1000042", "IMS:1000043"]);
        assert_eq!(info.index_blocks().1, vec![SHIFTED_TO_BASE_1]);
    }

    /// The beam fallback needs EVERY positioned frame to state the same finite size (review
    /// 2026-09-30 B16): a NULL was skipped and +inf passed as a pixel size.
    #[test]
    fn the_beam_fallback_needs_every_positioned_frame_to_state_it() {
        for (update, why) in [
            ("UPDATE MaldiFrameInfo SET BeamScanSizeX = NULL WHERE Frame = 3;", "one frame states none"),
            ("UPDATE MaldiFrameInfo SET BeamScanSizeY = NULL;", "no frame states y"),
            ("UPDATE MaldiFrameInfo SET BeamScanSizeX = 1e999, BeamScanSizeY = 1e999;", "+inf on every frame"),
            ("UPDATE MaldiFrameInfo SET BeamScanSizeX = 1e999 WHERE Frame = 2;", "+inf on one frame"),
        ] {
            let c = Connection::open_in_memory().unwrap();
            maldi_table(&c);
            c.execute_batch(update).unwrap();
            let info = read(&c).unwrap();
            assert_eq!(info.pixel_size(), None, "{why}");
            assert!(info.beam.iter().all(|(x, y)| x.is_finite() && y.is_finite()), "{why}: {:?}", info.beam);
            assert!(info.beam_unstated > 0, "{why}");
            assert!(!info.index_blocks().1.contains(&PIXEL_FROM_BEAM), "{why}");
            assert!(info.block()["pixel_size"].as_str().unwrap().starts_with("not written"), "{why}");
        }
        // A frame without a position does not count.
        let c = Connection::open_in_memory().unwrap();
        maldi_table(&c);
        c.execute_batch("INSERT INTO MaldiFrameInfo VALUES (4, 0, 'calib', NULL, NULL, NULL, 0.0, 0.0, NULL, NULL);").unwrap();
        let info = read(&c).unwrap();
        assert_eq!((info.pixel_size(), info.beam_unstated), (Some((20.0, 20.0)), 0));
        assert_eq!(info.block()["frames_without_beam_scan_size"], 0);
    }

    /// The tables of a real run (MassIVE MSV000088438, TSF schema 3.3 and TDF schema 3.5): the beam
    /// scan size is a column of `MaldiFrameLaserInfo`, which `MaldiFrameInfo.LaserInfo` references.
    fn laser_tables(conn: &Connection, laser_rows: &str) {
        conn.execute_batch(&format!(
            "CREATE TABLE MaldiFrameLaserInfo (Id INTEGER PRIMARY KEY, LaserApplicationName TEXT, BeamScan INTEGER NOT NULL,
                                               BeamScanSizeX REAL, BeamScanSizeY REAL, SpotSize REAL);
             INSERT INTO MaldiFrameLaserInfo VALUES {laser_rows};
             CREATE TABLE MaldiFrameInfo (Frame INTEGER PRIMARY KEY NOT NULL, Chip INTEGER NOT NULL, SpotName TEXT, RegionNumber INTEGER,
                                          XIndexPos INTEGER, YIndexPos INTEGER, MotorPositionX REAL, MotorPositionY REAL,
                                          LaserInfo INTEGER NOT NULL);
             INSERT INTO MaldiFrameInfo VALUES (1, 0, 'R00X019Y012', 0, 19, 12, 1.0, 2.0, 1),
                                               (2, 0, 'R00X020Y012', 0, 20, 12, 1.0, 2.0, 1),
                                               (3, 0, 'R01X021Y013', 1, 21, 13, 1.0, 2.0, 2);"
        ))
        .unwrap();
    }

    /// The beam fallback on the real schema. Through 0.16.0 the sizes were selected from
    /// `MaldiFrameInfo`, which does not have them there, so every frame counted as stating none and
    /// the fallback could not fire on a real file. `BeamScan = 0` (both MSV000088438 runs, with
    /// sizes 0.0) says the beam was not scanned: unstated, whatever the sizes beside it.
    #[test]
    fn the_beam_scan_size_is_read_through_laser_info() {
        let read_with = |laser_rows: &str| {
            let c = Connection::open_in_memory().unwrap();
            laser_tables(&c, laser_rows);
            read(&c).unwrap()
        };
        let info = read_with("(1, 'Custom', 1, 20.0, 20.0, 950.0), (2, 'Custom', 1, 20.0, 20.0, 950.0)");
        assert_eq!((info.beam.clone(), info.beam_unstated), (vec![(20.0, 20.0)], 0));
        assert_eq!(info.pixel_size_from(), Some(((20.0, 20.0), PixelSource::Beam)));
        assert!(info.index_blocks().1.contains(&PIXEL_FROM_BEAM));
        let b = info.block();
        assert_eq!(b["beam_scan_size_um"], serde_json::json!([{"x": 20.0, "y": 20.0}]));
        assert!(b["beam_scan_size_source"].as_str().unwrap().starts_with("MaldiFrameLaserInfo"), "{}", b["beam_scan_size_source"]);
        assert_eq!(info.spots[&3], Spot { x: 21, y: 13, region: Some(1), motor: Some((1.0, 2.0)) }, "the join loses nothing else");

        // Still only when EVERY positioned frame states the same finite positive size.
        for (laser_rows, why) in [
            ("(1, 'Custom', 0, 0.0, 0.0, 950.0), (2, 'Custom', 0, 0.0, 0.0, 950.0)", "the real runs: beam not scanned"),
            ("(1, 'Custom', 0, 20.0, 20.0, 950.0), (2, 'Custom', 0, 20.0, 20.0, 950.0)", "BeamScan = 0 beside a size"),
            ("(1, 'Custom', 1, 20.0, 20.0, 950.0), (2, 'Custom', 0, 20.0, 20.0, 950.0)", "one laser setting not scanned"),
            ("(1, 'Custom', 1, 20.0, 20.0, 950.0), (2, 'Custom', 1, 50.0, 50.0, 950.0)", "two sizes"),
            ("(1, 'Custom', 1, 20.0, 20.0, 950.0)", "a frame whose LaserInfo names no row"),
            ("(1, 'Custom', 1, 20.0, NULL, 950.0), (2, 'Custom', 1, 20.0, NULL, 950.0)", "no y"),
        ] {
            let info = read_with(laser_rows);
            assert_eq!(info.pixel_size(), None, "{why}");
            assert!(info.beam_unstated > 0 || info.beam.len() > 1, "{why}");
            assert_eq!(info.spots.len(), 3, "{why}: every positioned frame is kept");
            assert!(!info.index_blocks().1.contains(&PIXEL_FROM_BEAM), "{why}");
        }

        // A laser table without the BeamScan flag: the sizes as they are.
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE MaldiFrameLaserInfo (Id INTEGER PRIMARY KEY, BeamScanSizeX REAL, BeamScanSizeY REAL);
             INSERT INTO MaldiFrameLaserInfo VALUES (7, 10.0, 15.0);
             CREATE TABLE MaldiFrameInfo (Frame INTEGER PRIMARY KEY, XIndexPos INTEGER, YIndexPos INTEGER, LaserInfo INTEGER);
             INSERT INTO MaldiFrameInfo VALUES (1, 1, 1, 7), (2, 2, 1, 7);",
        )
        .unwrap();
        assert_eq!(read(&c).unwrap().pixel_size(), Some((10.0, 15.0)));

        // Columns on MaldiFrameInfo itself win: the schema the fallback was written for.
        let c = Connection::open_in_memory().unwrap();
        maldi_table(&c);
        c.execute_batch(
            "CREATE TABLE MaldiFrameLaserInfo (Id INTEGER PRIMARY KEY, BeamScan INTEGER, BeamScanSizeX REAL, BeamScanSizeY REAL);
             INSERT INTO MaldiFrameLaserInfo VALUES (1, 1, 99.0, 99.0);",
        )
        .unwrap();
        let info = read(&c).unwrap();
        assert_eq!((info.pixel_size(), info.beam_source), (Some((20.0, 20.0)), Some("MaldiFrameInfo.BeamScanSizeX/Y")));
        // Neither table has them: no source, every frame unstated.
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE MaldiFrameInfo (Frame INTEGER PRIMARY KEY, XIndexPos INTEGER, YIndexPos INTEGER); INSERT INTO MaldiFrameInfo VALUES (1, 1, 1);").unwrap();
        let info = read(&c).unwrap();
        assert_eq!((info.beam_source, info.beam_unstated, info.block()["beam_scan_size_source"].clone()), (None, 1, serde_json::Value::Null));
    }

    /// Frames without a position are counted: against `Frames` (a frame with no `MaldiFrameInfo` row
    /// at all, a row with a NULL index), and without that table the NULL-index rows. They used to
    /// be skipped with no trace but the difference to the spectrum count.
    #[test]
    fn frames_without_a_position_are_counted() {
        let c = Connection::open_in_memory().unwrap();
        maldi_table(&c); // frames 1-3
        assert_eq!((read(&c).unwrap().unpositioned, read(&c).unwrap().block()["frames_without_position"].clone()), (0, serde_json::json!(0)));
        c.execute_batch(
            "INSERT INTO MaldiFrameInfo VALUES (4, 0, 'calib', NULL, NULL, NULL, 0.0, 0.0, NULL, NULL);
             INSERT INTO MaldiFrameInfo VALUES (5, 0, 'half', 0, 671, NULL, 0.0, 0.0, 20.0, 20.0);",
        )
        .unwrap();
        let info = read(&c).unwrap();
        assert_eq!((info.spots.len(), info.unpositioned), (3, 2), "no Frames table: the rows with a NULL index");
        c.execute_batch(
            "CREATE TABLE Frames (Id INTEGER PRIMARY KEY, NumPeaks INTEGER);
             INSERT INTO Frames VALUES (1, 5), (2, 0), (3, 5), (4, 5), (5, 5), (6, 5), (7, 0);",
        )
        .unwrap();
        let info = read(&c).unwrap();
        assert_eq!((info.spots.len(), info.unpositioned), (3, 4), "frames 4 and 5 (NULL index), 6 and 7 (no row)");
        assert!(info.spots.contains_key(&2), "an empty frame with a row keeps its position");
        let b = info.block();
        assert_eq!((b["frames_with_position"].clone(), b["frames_without_position"].clone()), (serde_json::json!(3), serde_json::json!(4)));
    }

    /// A motor position or beam size that is no number (SQLite keeps text in a REAL column) is
    /// unstated; the frame keeps its position (it lost the whole row, and so its position, silently).
    #[test]
    fn an_unreadable_motor_or_beam_value_keeps_the_frame() {
        let c = Connection::open_in_memory().unwrap();
        maldi_table(&c);
        c.execute_batch(
            "UPDATE MaldiFrameInfo SET MotorPositionX = 'n/a' WHERE Frame = 2;
             UPDATE MaldiFrameInfo SET BeamScanSizeY = 'n/a' WHERE Frame = 3;",
        )
        .unwrap();
        let info = read(&c).unwrap();
        assert_eq!(info.spots.len(), 3, "every positioned frame is kept");
        assert_eq!(info.spots[&2], Spot { x: 670, y: 700, region: Some(0), motor: None });
        assert_eq!(info.spots[&3], Spot { x: 837, y: 812, region: Some(1), motor: Some((1.0, 2.0)) });
        assert_eq!((info.beam_unstated, info.pixel_size()), (1, None), "an unreadable beam size is unstated");
    }

    /// The shape of a flexImaging 5.1 sequence (MassIVE MSV000088438): no XML declaration, CRLF,
    /// polygon (`Type="3"`) and rectangle (`Type="0"`) areas, each with its own raster step.
    const MIS: &str = "<ImagingSequence flexImagingVersion=\"5.1.52.0_1664_120\">\r\n<Comment>1000 um</Comment>\r\n\
        <TeachPoint>1204,778;-22965,15855</TeachPoint>\r\n\
        <Area Type=\"3\" Name=\"vc_rugose_1\" Enabled=\"0\">\r\n<Raster>1000,1000</Raster>\r\n<Point>1250,1817</Point>\r\n</Area>\r\n\
        <Area Type=\"0\" Name=\"agar_1\" Enabled=\"0\">\r\n<Raster>1000,1000</Raster>\r\n<Point>1962,4015</Point>\r\n</Area>\r\n\
        </ImagingSequence>\r\n";

    #[test]
    fn the_mis_gives_the_raster_step_and_the_region_names() {
        let mis = read_mis_from("run.mis", MIS.as_bytes()).unwrap();
        assert_eq!(mis.areas.len(), 2);
        assert_eq!((mis.areas[0].name.as_deref(), mis.areas[0].raster), (Some("vc_rugose_1"), Some((1000.0, 1000.0))));
        assert_eq!(mis.areas[1].name.as_deref(), Some("agar_1"));

        let c = Connection::open_in_memory().unwrap();
        maldi_table(&c); // regions 0 and 1, beam 20 µm
        let mut info = read(&c).unwrap();
        assert_eq!(info.pixel_size_from(), Some(((20.0, 20.0), PixelSource::Beam)), "no .mis: the beam scan size");
        info.mis = Some(mis.clone());
        assert_eq!(info.pixel_size_from(), Some(((1000.0, 1000.0), PixelSource::Mis)), "the raster step wins");
        assert_eq!(info.index_blocks().1, vec![SHIFTED_TO_BASE_1], "a stated step is not declared as a fallback");
        let b = info.block();
        assert_eq!(b["mis"], "run.mis");
        assert_eq!(b["regions"][0]["name"], "vc_rugose_1");
        assert_eq!(b["regions"][1]["name"], "agar_1");
        assert_eq!(b["regions"][1]["raster_step_um"], serde_json::json!({"x": 1000.0, "y": 1000.0}));
        let grid = info.scan_settings();
        let v = |acc: &str| grid.params.iter().find(|p| p.curie().unwrap().to_string() == acc).map(|p| p.value.to_f64().unwrap());
        assert_eq!((v("IMS:1000046"), v("IMS:1000044")), (Some(1000.0), Some(169_000.0)));

        // Regions on different steps: no pixel size at all, not the beam fallback.
        info.mis.as_mut().unwrap().areas[1].raster = Some((500.0, 500.0));
        assert_eq!(info.pixel_size_from(), None);
        assert!(info.block()["pixel_size"].as_str().unwrap().contains("different raster steps"));
        // A .mis the regions do not map onto is ignored.
        info.mis = Some(Mis { file: "other.mis".into(), areas: vec![mis.areas[0].clone()], teach: vec![] });
        assert_eq!(info.pixel_size_from(), Some(((20.0, 20.0), PixelSource::Beam)));
    }

    /// A region number with no `<Area>`: the `.mis` is not used at all — no names, no raster step —
    /// and the block says why (review 2026-09-30 B16). Through [`read_dot_d`], which checks it.
    #[test]
    fn a_mis_with_fewer_areas_than_regions_is_not_used() {
        let dir = std::env::temp_dir().join(format!("mzpc-mis-count-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("run.d")).unwrap();
        maldi_table(&Connection::open(dir.join("run.d").join("analysis.tsf")).unwrap()); // regions 0 and 1
        let one_area = format!("{}</ImagingSequence>\r\n", &MIS[..MIS.rfind("<Area").unwrap()]);
        assert_eq!(read_mis_from("run.mis", one_area.as_bytes()).unwrap().areas.len(), 1);
        std::fs::write(dir.join("run.mis"), &one_area).unwrap();
        let info = read_dot_d(&dir.join("run.d")).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(info.mis, None, "not used");
        let (file, reason) = info.mis_rejected.clone().unwrap();
        assert_eq!((file.as_str(), reason.as_str()), ("run.mis", "RegionNumber 1 has no <Area> (the file has 1)"));
        let b = info.block();
        assert_eq!((b["mis"].clone(), b["regions"][0]["name"].clone()), (serde_json::Value::Null, serde_json::Value::Null), "no names from it");
        assert_eq!(b["mis_rejected"]["reason"], reason);
        assert_eq!(info.pixel_size_from(), Some(((20.0, 20.0), PixelSource::Beam)), "no raster step from it");
        assert!(b["pixel_size"].as_str().unwrap().contains("run.mis not used"), "{}", b["pixel_size"]);
    }

    /// Several unmapped regions: the reason names the largest, whatever the hash order of the frames
    /// (it was the first met, so the recorded reason changed from run to run).
    #[test]
    fn the_largest_unmapped_region_is_reported() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE MaldiFrameInfo (Frame INTEGER PRIMARY KEY, RegionNumber INTEGER, XIndexPos INTEGER, YIndexPos INTEGER);")
            .unwrap();
        for r in 0..16 {
            c.execute("INSERT INTO MaldiFrameInfo VALUES (?1, ?2, ?1, 1)", [r + 1, r]).unwrap();
        }
        let info = read(&c).unwrap();
        let mis = read_mis_from("run.mis", MIS.as_bytes()).unwrap();
        assert_eq!(info.mis_mismatch(&mis).as_deref(), Some("RegionNumber 15 has no <Area> (the file has 2)"));
    }

    /// The geometric check on MassIVE MSV000088438 (`20210921_vc_rugose_tims_gordon`): its teach
    /// points, its two rectangle areas `agar_1` / `agar_2` (regions 2 and 3 there, 0 and 1 here) and
    /// the corner frames' `MotorPositionX/Y`, which lie (+54000.8, −45642.5) µm from the teach-point
    /// stage frame. The file's order fits; the same areas swapped, as a re-saved sequence might list
    /// them, do not.
    #[test]
    fn regions_must_lie_inside_their_areas_under_one_offset() {
        let mis_xml = |first: &str, second: &str| {
            format!(
                "<ImagingSequence>\r\n<TeachPoint>1204,778;-22965,15855</TeachPoint>\r\n<TeachPoint>6706,648;21901,17125</TeachPoint>\r\n\
                 <TeachPoint>2550,4990;-11848,-18292</TeachPoint>\r\n{first}{second}</ImagingSequence>\r\n"
            )
        };
        let agar_1 = "<Area Type=\"0\" Name=\"agar_1\"><Raster>1000,1000</Raster><Point>1962,4015</Point><Point>2454,4635</Point></Area>\r\n";
        let agar_2 = "<Area Type=\"0\" Name=\"agar_2\"><Raster>1000,1000</Raster><Point>2686,3987</Point><Point>3246,4663</Point></Area>\r\n";
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE MaldiFrameInfo (Frame INTEGER PRIMARY KEY, RegionNumber INTEGER, XIndexPos INTEGER, YIndexPos INTEGER,
                                          MotorPositionX REAL, MotorPositionY REAL);
             INSERT INTO MaldiFrameInfo VALUES (197, 0, 16, 33, 38036.85943861803, -56788.52517336607),
                                               (216, 0, 19, 37, 41036.85872336229, -60788.48926156759),
                                               (217, 1, 22, 32, 44036.85723697146, -55788.50793272257),
                                               (240, 1, 25, 37, 47036.85652171572, -60788.48926156759);",
        )
        .unwrap();
        let mut info = read(&c).unwrap();
        let right = read_mis_from("run.mis", mis_xml(agar_1, agar_2).as_bytes()).unwrap();
        assert_eq!((right.teach.len(), &right.areas[0].points), (3, &vec![(1962.0, 4015.0), (2454.0, 4015.0), (2454.0, 4635.0), (1962.0, 4635.0)]));
        assert_eq!(info.mis_mismatch(&right), None);
        let swapped = read_mis_from("run.mis", mis_xml(agar_2, agar_1).as_bytes()).unwrap();
        assert!(info.mis_mismatch(&swapped).is_some_and(|r| r.contains("MotorPositionX/Y")));
        // check_mis keeps the one and drops the other.
        info.mis = Some(right);
        info.check_mis();
        assert_eq!((info.mis.is_some(), info.pixel_size()), (true, Some((1000.0, 1000.0))));
        info.mis = Some(swapped);
        info.check_mis();
        assert_eq!((info.mis.is_some(), info.pixel_size(), info.region_name(Some(0))), (false, None, None));
        // Without motor positions, or with an area whose outline is unknown, there is nothing to test.
        info.spots.values_mut().for_each(|s| s.motor = None);
        assert_eq!(info.mis_mismatch(&read_mis_from("run.mis", mis_xml(agar_2, agar_1).as_bytes()).unwrap()), None);
    }

    /// A rectangle is two corners in image px, and the teach-point map rotates and shears: the other
    /// two corners lie outside the stage box of the stated ones (by 151 µm in y here, the 20210920
    /// map on a 1250-px square). Spots at all four, as MotorPositionX/Y (+54000.8, −45642.5 µm), fit
    /// a 20 µm raster; checked on the two stated corners, the rectangle was rejected.
    #[test]
    fn a_rectangle_is_checked_on_all_four_corners() {
        let mis = read_mis_from(
            "run.mis",
            "<ImagingSequence>\r\n<TeachPoint>1252,776;-22963,15832</TeachPoint>\r\n<TeachPoint>6842,696;21879,17145</TeachPoint>\r\n\
             <TeachPoint>2610,5064;-11828,-18267</TeachPoint>\r\n\
             <Area Type=\"0\" Name=\"tissue\"><Raster>20,20</Raster><Point>1500,1500</Point><Point>2750,2750</Point></Area>\r\n\
             </ImagingSequence>\r\n"
                .as_bytes(),
        )
        .unwrap();
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE MaldiFrameInfo (Frame INTEGER PRIMARY KEY, RegionNumber INTEGER, XIndexPos INTEGER, YIndexPos INTEGER,
                                          MotorPositionX REAL, MotorPositionY REAL);
             INSERT INTO MaldiFrameInfo VALUES (1, 0, 1, 1, 33067.98, -35565.63),
                                               (2, 0, 2, 1, 43096.26, -35414.97),
                                               (3, 0, 1, 2, 33138.02, -45553.59),
                                               (4, 0, 2, 2, 43166.31, -45402.92);",
        )
        .unwrap();
        assert_eq!(read(&c).unwrap().mis_mismatch(&mis), None);
    }

    #[test]
    fn a_table_without_positions_is_not_imaging() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE MaldiFrameInfo (Frame INTEGER, SpotName TEXT); INSERT INTO MaldiFrameInfo VALUES (1, 'A1');").unwrap();
        assert!(read(&c).is_none());
    }

    #[test]
    fn attach_matches_the_frame_id() {
        let c = Connection::open_in_memory().unwrap();
        maldi_table(&c);
        let info = read(&c).unwrap();
        let mut spec = MultiLayerSpectrum::default();
        spec.description_mut().id = "frame=2".into();
        assert!(info.attach(&mut spec));
        let pos = |s: &MultiLayerSpectrum| {
            let scan = &s.description().acquisition.scans[0];
            let v = |c| scan.get_param_by_curie(&c).unwrap().value.to_i64().unwrap();
            (v(mzdata::curie!(IMS:1000050)), v(mzdata::curie!(IMS:1000051)))
        };
        assert_eq!(pos(&spec), (2, 1), "index (670, 700) on a run starting at (669, 700)");
        // The frame's RegionNumber, as a parameter without an accession.
        let region = |s: &MultiLayerSpectrum| {
            let p: Vec<_> = s.description().acquisition.scans[0].params().iter().filter(|p| p.name == REGION_PARAM).collect();
            assert!(p.len() <= 1 && p.iter().all(|p| p.accession.is_none()), "{p:?}");
            p.first().map(|p| p.value.to_i64().unwrap())
        };
        assert_eq!(region(&spec), Some(0));
        spec.description_mut().id = "frame=99".into();
        assert!(!MaldiInfo::default().attach(&mut spec));
        spec.description_mut().id = "scan=2".into();
        assert!(!info.attach(&mut spec), "only frame ids are matched");
        let mut merged = MultiLayerSpectrum::default();
        merged.description_mut().id = "merged=0 frame=3 startScan=1 endScan=900".into();
        assert!(info.attach(&mut merged), "mzdata's TDF ids carry the frame as a token");
        assert_eq!(pos(&merged), (169, 113));
        assert_eq!(region(&merged), Some(1));
        // A run whose table states no region: the position alone.
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE MaldiFrameInfo (Frame INTEGER PRIMARY KEY, XIndexPos INTEGER, YIndexPos INTEGER); INSERT INTO MaldiFrameInfo VALUES (1, 4, 4);").unwrap();
        let info = read(&c).unwrap();
        let mut spec = MultiLayerSpectrum::default();
        spec.description_mut().id = "frame=1".into();
        assert!(info.attach(&mut spec));
        assert_eq!((pos(&spec), region(&spec)), ((1, 1), None));
        assert_eq!(info.block()["region_parameter"], serde_json::Value::Null);
    }
}
