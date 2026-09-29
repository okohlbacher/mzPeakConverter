//! Bruker MALDI imaging: per-frame raster positions from `MaldiFrameInfo` (timsTOF fleX, TSF and
//! TDF), so an imaging run converted straight from the `.d` can be rebuilt as an image — until now
//! only the imzML path wrote positions (HUPO-PSI/mzPeak-specification#23).
//!
//! * `XIndexPos` / `YIndexPos` become `IMS:1000050` / `IMS:1000051` on the frame's scan, the same
//!   params — and so the same `opt_IMS_*_position_*` columns — as the imzML path. They are written
//!   AS STORED: absolute raster indices on the target (669–837 in the issue author's file), not
//!   shifted to start at 1. Whether mzPeak fixes a coordinate base is an open specification decision.
//! * The `bruker_maldi` index block keeps what has no mzPeak home yet: the regions
//!   (`RegionNumber`, frames and index ranges each), the index ranges, the beam scan size.
//! * Pixel size: the FlexImaging `.mis` holds the raster step but is not part of the `.d`; the
//!   frames' `BeamScanSizeX/Y` (µm) is the stated fallback, written as `IMS:1000046/47` and declared
//!   as such ([`PIXEL_FROM_BEAM`]). Only when every frame states the same size.
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

pub const PIXEL_FROM_BEAM: &str = "bruker:pixel-size-from-beam-scan-size";

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
    (!info.spots.is_empty()).then_some(info)
}

/// [`read`] on a Bruker `.d` (its `analysis.tsf`, else `analysis.tdf`); `None` for anything else.
pub fn read_dot_d(dot_d: &Path) -> Option<MaldiInfo> {
    ["analysis.tsf", "analysis.tdf"].iter().find_map(|name| {
        let db = dot_d.join(name);
        if !std::fs::metadata(&db).is_ok_and(|m| m.is_file() && m.len() > 0) {
            return None;
        }
        read(&crate::vendor_sqlite::open(&db).ok()?)
    })
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
        scans[0].add_param(Param::builder().name("position x").curie(mzdata::curie!(IMS:1000050)).value(spot.x).build());
        scans[0].add_param(Param::builder().name("position y").curie(mzdata::curie!(IMS:1000051)).value(spot.y).build());
        true
    }

    /// Pixel size from the beam scan size, when every frame states the same one.
    pub fn scan_settings(&self) -> Option<ScanSettings> {
        let [(bx, by)] = self.beam.as_slice() else { return None };
        let mut s = ScanSettings { id: "scansettings1".into(), ..Default::default() };
        s.params.push(Param::builder().name("pixel size x").curie(mzdata::curie!(IMS:1000046)).value(*bx).unit(Unit::Micrometer).build());
        s.params.push(Param::builder().name("pixel size y").curie(mzdata::curie!(IMS:1000047)).value(*by).unit(Unit::Micrometer).build());
        Some(s)
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
            "coordinates": "XIndexPos/YIndexPos as stored: absolute raster indices on the target, not shifted to start at 1 (the mzPeak coordinate base is an open specification decision)",
            "frames_with_position": self.spots.len(),
            "x_index": range(|s| s.x, &mut self.spots.values()),
            "y_index": range(|s| s.y, &mut self.spots.values()),
            "regions": regions.iter().map(|(r, spots)| serde_json::json!({
                "region_number": r,
                "frames": spots.len(),
                "x_index": range(|s| s.x, &mut spots.iter().copied()),
                "y_index": range(|s| s.y, &mut spots.iter().copied()),
            })).collect::<Vec<_>>(),
            "beam_scan_size_um": self.beam.iter().map(|(x, y)| serde_json::json!({"x": x, "y": y})).collect::<Vec<_>>(),
            "pixel_size": if self.scan_settings().is_some() {
                "the beam scan size (the FlexImaging .mis raster step is not part of the .d)"
            } else {
                "not written: no single beam scan size stated"
            },
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
        let b = info.block();
        assert_eq!(b["x_index"], serde_json::json!([669, 837]));
        assert_eq!(b["regions"].as_array().unwrap().len(), 2);
        let ss = info.scan_settings().unwrap();
        assert_eq!(ss.params[0].value.to_f64().unwrap(), 20.0);
        assert_eq!(ss.params[0].unit, Unit::Micrometer);
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
        let scan = &spec.description().acquisition.scans[0];
        assert_eq!(scan.get_param_by_curie(&mzdata::curie!(IMS:1000050)).unwrap().value.to_i64().unwrap(), 670);
        assert_eq!(scan.get_param_by_curie(&mzdata::curie!(IMS:1000051)).unwrap().value.to_i64().unwrap(), 700);
        spec.description_mut().id = "frame=99".into();
        assert!(!MaldiInfo { spots: HashMap::new(), beam: vec![] }.attach(&mut spec));
        spec.description_mut().id = "scan=2".into();
        assert!(!info.attach(&mut spec), "only frame ids are matched");
        let mut merged = MultiLayerSpectrum::default();
        merged.description_mut().id = "merged=0 frame=3 startScan=1 endScan=900".into();
        assert!(info.attach(&mut merged), "mzdata's TDF ids carry the frame as a token");
    }
}
