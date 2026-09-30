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
//!   profile describes one grid). Without a `.mis`, the frames' `BeamScanSizeX/Y` (µm) is the
//!   fallback, declared as such ([`PIXEL_FROM_BEAM`]), and only when every frame states the same size.
//!   Either way written as `IMS:1000046/47`, with `IMS:1000044/45` max dimension = count × size.
//!
//! Column names are Bruker's (`MaldiFrameInfo(Frame, …, RegionNumber, XIndexPos, YIndexPos, …,
//! BeamScanSizeX, BeamScanSizeY)`), confirmed by the issue author on a real acquisition; the corpus
//! holds none, so the tests build the table.

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

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spot {
    pub x: i64,
    pub y: i64,
    pub region: Option<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct MaldiInfo {
    /// Keyed by `Frames.Id`.
    pub spots: HashMap<i64, Spot>,
    /// Every distinct `(BeamScanSizeX, BeamScanSizeY)` stated, in µm.
    pub beam: Vec<(f64, f64)>,
    /// Smallest and largest `(XIndexPos, YIndexPos)` of the run: `min` becomes position (1, 1).
    pub min: (i64, i64),
    pub max: (i64, i64),
    /// The FlexImaging sequence beside the `.d`, if there is one.
    pub mis: Option<Mis>,
}

/// One `<Area>` of a FlexImaging `.mis`: its name and raster step (µm).
#[derive(Debug, Clone, PartialEq)]
pub struct MisArea {
    pub name: Option<String>,
    pub raster: Option<(f64, f64)>,
}

/// A FlexImaging sequence: its areas in file order. `MaldiFrameInfo.RegionNumber` n is the n-th
/// `<Area>` — checked on MassIVE MSV000088438 against timsControl's poslog (`R00`…) and flexImaging's
/// spot list (region names).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Mis {
    pub file: String,
    pub areas: Vec<MisArea>,
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
    let mut mis = Mis { file: file.into(), areas: Vec::new() };
    let (mut in_area, mut in_raster) = (false, false);
    loop {
        match reader.read_event_into(&mut buf).ok()? {
            Event::Start(e) if e.local_name().as_ref() == b"Area" => {
                in_area = true;
                mis.areas.push(MisArea { name: crate::imaging::attr(&e, b"Name"), raster: None });
            }
            Event::Empty(e) if e.local_name().as_ref() == b"Area" => {
                mis.areas.push(MisArea { name: crate::imaging::attr(&e, b"Name"), raster: None });
            }
            Event::Start(e) if in_area && e.local_name().as_ref() == b"Raster" => in_raster = true,
            Event::Text(t) if in_raster => {
                let text = String::from_utf8_lossy(&t).into_owned();
                let mut v = text.split(',').map(|v| v.trim().parse::<f64>());
                if let (Some(Ok(x)), Some(Ok(y)), Some(a)) = (v.next(), v.next(), mis.areas.last_mut()) {
                    a.raster = (x > 0.0 && y > 0.0).then_some((x, y));
                }
            }
            Event::End(e) => match e.local_name().as_ref() {
                b"Area" => in_area = false,
                b"Raster" => in_raster = false,
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    (!mis.areas.is_empty()).then_some(mis)
}

/// `MaldiFrameInfo` of an open TSF/TDF database; `None` without the table or its position columns.
pub fn read(conn: &Connection) -> Option<MaldiInfo> {
    let cols: Vec<String> = conn
        .prepare("SELECT name FROM pragma_table_info('MaldiFrameInfo')")
        .ok()?
        .query_map([], |r| r.get::<_, String>(0))
        .ok()?
        .flatten()
        .collect();
    let has = |c: &str| cols.iter().any(|x| x == c);
    if !(has("Frame") && has("XIndexPos") && has("YIndexPos")) {
        return None;
    }
    let opt = |c: &str| if has(c) { c.to_string() } else { "NULL".to_string() };
    let sql = format!(
        "SELECT Frame, XIndexPos, YIndexPos, {}, {}, {} FROM MaldiFrameInfo",
        opt("RegionNumber"),
        opt("BeamScanSizeX"),
        opt("BeamScanSizeY")
    );
    let mut stmt = conn.prepare(&sql).ok()?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<i64>>(1)?,
                r.get::<_, Option<i64>>(2)?,
                r.get::<_, Option<i64>>(3)?,
                r.get::<_, Option<f64>>(4)?,
                r.get::<_, Option<f64>>(5)?,
            ))
        })
        .ok()?;
    let mut info = MaldiInfo::default();
    for (frame, x, y, region, bx, by) in rows.flatten() {
        let (Some(x), Some(y)) = (x, y) else { continue };
        info.spots.insert(frame, Spot { x, y, region });
        if let (Some(bx), Some(by)) = (bx, by) {
            if bx > 0.0 && by > 0.0 && !info.beam.contains(&(bx, by)) {
                info.beam.push((bx, by));
            }
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
    Some(info)
}

impl MaldiInfo {
    /// Put the frame's position on the spectrum's first scan. Spectra are matched by the
    /// `frame=<Frames.Id>` token of their id: the whole id on the TSF, native TDF and SDK lanes,
    /// `merged=… frame=… startScan=…` through mzdata.
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
        true
    }

    /// Pixel counts of the shifted grid.
    pub fn count(&self) -> (i64, i64) {
        (self.max.0 - self.min.0 + 1, self.max.1 - self.min.1 + 1)
    }

    /// Pixel size in µm and its source: the `.mis` raster step when every acquired region maps to an
    /// area with one and they agree (none when they differ); without a usable `.mis`, the beam scan
    /// size when every frame states the same one.
    pub fn pixel_size_from(&self) -> Option<((f64, f64), PixelSource)> {
        if let Some(steps) = self.mis_rasters() {
            let [step] = steps.as_slice() else { return None };
            return Some((*step, PixelSource::Mis));
        }
        let [b] = self.beam.as_slice() else { return None };
        Some((*b, PixelSource::Beam))
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
        match (self.pixel_size_from(), &self.mis) {
            (Some((_, PixelSource::Mis)), Some(m)) => format!("the raster step in {}", m.file),
            (Some((_, PixelSource::Beam)), _) => "the beam scan size (no FlexImaging .mis beside the .d)".into(),
            (None, Some(m)) if self.mis_rasters().is_some() => format!("not written: the regions of {} have different raster steps", m.file),
            _ => "not written: no single beam scan size stated".into(),
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
        let marker = crate::imaging::marker_block(
            Some(&self.scan_settings()),
            serde_json::json!({
                "detected_from": "MaldiFrameInfo in analysis.tsf/.tdf",
                "positions": "XIndexPos/YIndexPos − origin + 1",
                "origin": {"x": self.min.0, "y": self.min.1},
                "pixel_size": self.pixel_size_note(),
            }),
        );
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
            "source": "analysis.tsf/.tdf MaldiFrameInfo (XIndexPos, YIndexPos, RegionNumber, BeamScanSizeX/Y)",
            "coordinates": "positions are XIndexPos/YIndexPos − origin + 1; x_index/y_index and the regions give the raw indices",
            "origin": {"x": self.min.0, "y": self.min.1},
            "frames_with_position": self.spots.len(),
            "x_index": range(|s| s.x, &mut self.spots.values()),
            "y_index": range(|s| s.y, &mut self.spots.values()),
            "mis": self.mis.as_ref().map(|m| &m.file),
            "regions": regions.iter().map(|(r, spots)| serde_json::json!({
                "region_number": r,
                "name": self.region_name(*r),
                "raster_step_um": self.mis.as_ref().and_then(|m| m.areas.get(usize::try_from((*r)?).ok()?)?.raster).map(|(x, y)| serde_json::json!({"x": x, "y": y})),
                "frames": spots.len(),
                "x_index": range(|s| s.x, &mut spots.iter().copied()),
                "y_index": range(|s| s.y, &mut spots.iter().copied()),
            })).collect::<Vec<_>>(),
            "beam_scan_size_um": self.beam.iter().map(|(x, y)| serde_json::json!({"x": x, "y": y})).collect::<Vec<_>>(),
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
        assert_eq!(info.spots[&1], Spot { x: 669, y: 700, region: Some(0) });
        assert_eq!(info.spots[&3], Spot { x: 837, y: 812, region: Some(1) });
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
        assert_eq!(marker["provenance"]["origin"], serde_json::json!({"x": 669, "y": 700}));
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
        info.mis = Some(Mis { file: "other.mis".into(), areas: vec![mis.areas[0].clone()] });
        assert_eq!(info.pixel_size_from(), Some(((20.0, 20.0), PixelSource::Beam)));
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
        spec.description_mut().id = "frame=99".into();
        assert!(!MaldiInfo::default().attach(&mut spec));
        spec.description_mut().id = "scan=2".into();
        assert!(!info.attach(&mut spec), "only frame ids are matched");
        let mut merged = MultiLayerSpectrum::default();
        merged.description_mut().id = "merged=0 frame=3 startScan=1 endScan=900".into();
        assert!(info.attach(&mut merged), "mzdata's TDF ids carry the frame as a token");
        assert_eq!(pos(&merged), (169, 113));
    }
}
