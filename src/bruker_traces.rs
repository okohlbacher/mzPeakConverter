//! Bruker HyStar device traces → non-MS chromatograms.
//!
//! A timsTOF `.d` (TDF or TSF) carries its LC system's traces — pump pressure and flow, solvent
//! composition, column and tray temperature, UV — in `chromatography-data.sqlite`, which HyStar
//! writes beside `analysis.tdf`/`analysis.tsf`. ProteoWizard reads it
//! (`pwiz_aux/msrc/utility/vendor_api/Bruker/ChromatographyDataSqliteReader.cpp`), so until this
//! module only the mzML lane had them. The layout, as HyStar 6.0–6.3 writes it on the corpus runs
//! (`ims-examples/PXD059079/…_2485.d`, PXD076703, PXD078573, PXD079300) and the private TSF run:
//!   * `TraceSources(Id, Description, Instrument, InstrumentId, Type, Unit, TimeOffset, Color)` —
//!     one row per trace. `Type` and `Unit` are HyStar enums whose values Bruker gave ProteoWizard
//!     (`pwiz/data/vendor_readers/Bruker/hystar_enum_definitions.txt`).
//!   * `TraceChunks(Trace, Times, Intensities)` — the samples, chunk by chunk in rowid order:
//!     `Times` little-endian f64 seconds on the frame clock (plus the trace's `TimeOffset`),
//!     `Intensities` little-endian f32, one per time.
//!
//! Each trace becomes one chromatogram whose value array is the one its kind names — pressure,
//! flow rate or temperature array; the intensity array only when the values are detector counts;
//! otherwise a non-standard array named after the trace — in the unit HyStar states. The writer
//! stores an array outside the facet's `time`/`intensity` columns as an auxiliary array that keeps
//! its own unit; an intensity array in percent would instead land in the `intensity` column, which
//! is declared as detector counts, and silently lose it.

use std::path::Path;

use mzdata::params::{Param, Unit};
use mzdata::prelude::*;
use mzdata::spectrum::bindata::{ArrayType, BinaryArrayMap, BinaryDataArrayType, DataArray};
use mzdata::spectrum::{Chromatogram, ChromatogramDescription, ChromatogramType};
use rusqlite::{Connection, OpenFlags};

/// The `transformations` entry of a run with a trace HyStar recorded in a unit mzdata cannot state
/// (bar, mbar, kPa, MPa, mL/min, nL/min, mAU, kV, mV, µs, h, Å): its values were multiplied by the
/// exact factor into one it can (pascal, µL/min, absorbance unit, volt, millisecond, minute, nm).
pub const UNIT_RESCALE: &str = "bruker:trace-unit-rescale";

/// A `.d`'s device traces, and whether any was rescaled ([`UNIT_RESCALE`]).
#[derive(Default)]
pub struct DeviceTraces {
    pub chromatograms: Vec<Chromatogram>,
    pub rescaled: bool,
}

/// One `TraceSources` row.
struct Source {
    id: i64,
    description: String,
    instrument: String,
    kind: i64,
    unit: i64,
    time_offset: f64,
}

/// The device traces in `dot_d/chromatography-data.sqlite`, opened read-only. No such file (any
/// input that is not a HyStar-acquired Bruker `.d`) means no traces; an unreadable one is a warning
/// rather than a failed conversion, since no spectrum depends on it.
pub fn read(dot_d: &Path) -> DeviceTraces {
    let path = dot_d.join("chromatography-data.sqlite");
    if !path.is_file() {
        return DeviceTraces::default();
    }
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    match Connection::open_with_flags(&path, flags).and_then(|conn| read_traces(&conn)) {
        Ok(traces) => {
            log::info!("{}: {} device traces", path.display(), traces.chromatograms.len());
            traces
        }
        Err(e) => {
            log::warn!("{}: device traces not read ({e}); the archive has no LC chromatograms", path.display());
            DeviceTraces::default()
        }
    }
}

fn read_traces(conn: &Connection) -> rusqlite::Result<DeviceTraces> {
    let mut stmt = conn.prepare("SELECT Id, Description, Instrument, Type, Unit, TimeOffset FROM TraceSources ORDER BY Id")?;
    let sources = stmt
        .query_map([], |r| {
            Ok(Source {
                id: r.get(0)?,
                description: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                instrument: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                kind: r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                unit: r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                time_offset: r.get::<_, Option<f64>>(5)?.unwrap_or(0.0),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut chunks = conn.prepare("SELECT Times, Intensities FROM TraceChunks WHERE Trace = ?1 ORDER BY rowid")?;
    let mut out = DeviceTraces::default();
    for s in &sources {
        let (mut seconds, mut values) = (Vec::new(), Vec::new());
        let mut aligned = true;
        let mut rows = chunks.query([s.id])?;
        while let Some(row) = rows.next()? {
            let (t, v): (Vec<u8>, Vec<u8>) = (row.get(0)?, row.get(1)?);
            if t.len() % 8 != 0 || v.len() % 4 != 0 || t.len() / 8 != v.len() / 4 {
                aligned = false;
                break;
            }
            seconds.extend(t.chunks_exact(8).map(|b| f64::from_le_bytes(b.try_into().unwrap()) + s.time_offset));
            values.extend(v.chunks_exact(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())));
        }
        if !aligned {
            log::warn!("device trace {} ({:?}): a chunk's times and values differ in length; trace skipped", s.id, s.description);
            continue;
        }
        let (unit, scale) = unit(s);
        out.rescaled |= scale != 1.0;
        out.chromatograms.push(chromatogram(s, unit, scale, &seconds, &values));
    }
    Ok(out)
}

/// One trace as a chromatogram: times in minutes (the facet's unit), values multiplied by `scale`
/// into `unit`, the title and instrument as ProteoWizard states them.
fn chromatogram(s: &Source, unit: Unit, scale: f64, seconds: &[f64], values: &[f32]) -> Chromatogram {
    let kind = chromatogram_type(s);
    let name = if s.description.is_empty() { format!("trace {}", s.id) } else { s.description.clone() };
    let value_type = match kind {
        ChromatogramType::PressureChromatogram => ArrayType::PressureArray,
        ChromatogramType::FlowRateChromatogram => ArrayType::FlowRateArray,
        ChromatogramType::TemperatureChromatogram => ArrayType::TemperatureArray,
        _ if matches!(unit, Unit::DetectorCounts) => ArrayType::IntensityArray,
        _ => ArrayType::nonstandard(&name),
    };
    // mzdata keeps decoded samples as native-endian bytes.
    let minutes = seconds.iter().flat_map(|t| (t / 60.0).to_ne_bytes()).collect();
    let scaled = values
        .iter()
        .flat_map(|&v| if scale == 1.0 { v } else { (v as f64 * scale) as f32 }.to_ne_bytes())
        .collect();
    let mut time = DataArray::wrap(&ArrayType::TimeArray, BinaryDataArrayType::Float64, minutes);
    time.unit = Unit::Minute;
    let mut value = DataArray::wrap(&value_type, BinaryDataArrayType::Float32, scaled);
    value.unit = unit;
    let mut arrays = BinaryArrayMap::new();
    arrays.add(time);
    arrays.add(value);
    let mut descr = ChromatogramDescription { id: name, chromatogram_type: kind, ..Default::default() };
    if !s.description.is_empty() {
        descr.add_param(
            Param::builder().name("chromatogram title").curie(mzdata::curie!(MS:1000809)).value(s.description.clone()).build(),
        );
    }
    if !s.instrument.is_empty() {
        descr.add_param(Param::builder().name("Instrument").value(s.instrument.clone()).build());
    }
    Chromatogram::new(descr, arrays)
}

/// HyStar's `Unit` as the mzdata unit that states it and the factor into that unit. A code mzdata
/// has no unit for even after an exact rescale (UnknownUnit, current, luminescence, molarity, power,
/// refractive index, °F, viscosity, energy) stays [`Unit::Unknown`] with its values as stored — the
/// trace's title usually names it ("Pump A:Displacement - [µL]").
fn unit(s: &Source) -> (Unit, f64) {
    match s.unit {
        // NoneUnit: ProteoWizard takes a pressure trace's unit from its title and calls the rest
        // intensity; only an MS trace's values are detector counts, so the others stay unknown.
        0 if s.kind == 4 && s.description.contains("[psi]") => (Unit::Psi, 1.0),
        0 if s.kind == 4 && s.description.to_ascii_lowercase().contains("[bar]") => (Unit::Pascal, 1e5),
        0 if s.kind == 1 => (Unit::DetectorCounts, 1.0),
        1 => (Unit::Nanometer, 1.0),
        2 => (Unit::MicrolitersPerMinute, 1.0),
        3 => (Unit::Pascal, 1e5),
        4 => (Unit::Percent, 1.0),
        5 => (Unit::Celsius, 1.0),
        6 | 10 => (Unit::DetectorCounts, 1.0),
        8 => (Unit::AbsorbanceUnit, 1.0),
        9 => (Unit::AbsorbanceUnit, 1e-3),
        14 => (Unit::MicrolitersPerMinute, 1e3),
        15 => (Unit::MicrolitersPerMinute, 1e-3),
        16 => (Unit::Centimeter, 1.0),
        17 => (Unit::Millimeter, 1.0),
        18 => (Unit::Micrometer, 1.0),
        24 => (Unit::Pascal, 1e2),
        25 => (Unit::Pascal, 1e3),
        26 => (Unit::Pascal, 1e6),
        27 => (Unit::Psi, 1.0),
        31 => (Unit::Minute, 60.0),
        32 => (Unit::Minute, 1.0),
        33 => (Unit::Second, 1.0),
        34 => (Unit::Millisecond, 1.0),
        35 => (Unit::Millisecond, 1e-3),
        37 => (Unit::Volt, 1e3),
        38 => (Unit::Volt, 1.0),
        39 => (Unit::Volt, 1e-3),
        40 => (Unit::Liter, 1.0),
        41 => (Unit::Milliliter, 1.0),
        42 => (Unit::Microliter, 1.0),
        46 => (Unit::Nanometer, 0.1),
        _ => (Unit::Unknown, 1.0),
    }
}

/// HyStar's `Type` as the PSI-MS chromatogram type — ProteoWizard's `traceTypeToCVID`, extended in
/// one respect: a user-defined trace (9999) whose unit is a pressure or a flow rate is that
/// chromatogram, as ProteoWizard already rules for a temperature (the corpus TDF run's Agilent pump
/// traces are all user-defined). Solvent composition and everything else stay unknown, which the
/// writer stores as a null `chromatogram_type`.
fn chromatogram_type(s: &Source) -> ChromatogramType {
    const PRESSURE: [i64; 5] = [3, 24, 25, 26, 27];
    const FLOW: [i64; 3] = [2, 14, 15];
    const TEMPERATURE: [i64; 2] = [5, 30];
    match s.kind {
        1 if s.description.starts_with("BPC") => ChromatogramType::BasePeakChromatogram,
        1 => ChromatogramType::TotalIonCurrentChromatogram,
        3 => ChromatogramType::AbsorptionChromatogram,
        4 => ChromatogramType::PressureChromatogram,
        6 => ChromatogramType::FlowRateChromatogram,
        7 => ChromatogramType::TemperatureChromatogram,
        9999 if PRESSURE.contains(&s.unit) => ChromatogramType::PressureChromatogram,
        9999 if FLOW.contains(&s.unit) => ChromatogramType::FlowRateChromatogram,
        9999 if TEMPERATURE.contains(&s.unit) => ChromatogramType::TemperatureChromatogram,
        _ => ChromatogramType::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two tables exactly as HyStar 6.0 declares them (`…_2485.d`).
    const SCHEMA: &str = "CREATE TABLE TraceSources (Id INTEGER PRIMARY KEY,Description TEXT,Instrument TEXT,InstrumentId TEXT,Type INTEGER,Unit INTEGER,TimeOffset REAL,Color INTEGER);
        CREATE TABLE TraceChunks (Trace INTEGER NOT NULL,Times BLOB NOT NULL,Intensities BLOB NOT NULL,FOREIGN KEY (Trace) REFERENCES TraceSources(Id));";

    type Chunk<'a> = (i64, &'a [f64], &'a [f32]);

    fn hystar(sources: &[(i64, &str, &str, i64, i64, f64)], chunks: &[Chunk]) -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(SCHEMA).unwrap();
        for (id, description, instrument, kind, unit, offset) in sources {
            c.execute(
                "INSERT INTO TraceSources (Id, Description, Instrument, Type, Unit, TimeOffset) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![id, description, instrument, kind, unit, offset],
            )
            .unwrap();
        }
        for (trace, seconds, values) in chunks {
            let t: Vec<u8> = seconds.iter().flat_map(|x| x.to_le_bytes()).collect();
            let v: Vec<u8> = values.iter().flat_map(|x| x.to_le_bytes()).collect();
            c.execute("INSERT INTO TraceChunks VALUES (?1, ?2, ?3)", rusqlite::params![trace, t, v]).unwrap();
        }
        c
    }

    /// The value array and the times (minutes) of a trace chromatogram.
    fn arrays(c: &Chromatogram) -> (&DataArray, Vec<f64>) {
        let value = c.arrays.iter().map(|(_, a)| a).find(|a| a.name != ArrayType::TimeArray).unwrap();
        (value, c.arrays.get(&ArrayType::TimeArray).unwrap().to_f64().unwrap().to_vec())
    }

    /// Every kind on the corpus runs: the TSF run's Elute traces (a pressure whose unit is only in
    /// its title, solvent percent, °C), the TDF run's Agilent ICF traces (user-defined, in bar and
    /// µL/min, typed by that unit; a setpoint of unknown unit) and HyStar's own MS trace.
    #[test]
    fn hystar_traces_become_typed_chromatograms_in_their_units() {
        let c = hystar(
            &[
                (1, "BPC,±MS", "Bruker OTOF MS", 1, 6, 0.0),
                (2, "Pressure - [psi]", "Elute Pump", 4, 0, 0.0),
                (3, "Fraction A - [%]", "Elute Pump", 5, 4, 0.0),
                (4, "Oven temperature - [°C]", "Elute Column Oven", 7, 5, 0.0),
                (5, "Pump HP:Pressure - [bar]", "Agilent ICF System", 9999, 3, 0.0),
                (6, "Pump HP:Actual flow - [µL/min]", "Agilent ICF System", 9999, 2, 1.5),
                (7, "Pump A:Setpoint - []", "Agilent ICF System", 9999, 7, 0.0),
            ],
            &[
                (2, &[2.109, 2.5], &[3785.5, 3783.25]),
                (1, &[1.15], &[8938.0]),
                (2, &[3.0], &[3779.75]),
                (3, &[2.109], &[98.0]),
                (4, &[2.109], &[50.0]),
                (5, &[1.75, 2.0], &[180.0, 172.5]),
                (6, &[1.75], &[4.25]),
                (7, &[1.95], &[20.0]),
            ],
        );
        let traces = read_traces(&c).unwrap();
        let ch = &traces.chromatograms;
        assert_eq!(
            ch.iter().map(|c| c.id()).collect::<Vec<_>>(),
            [
                "BPC,±MS",
                "Pressure - [psi]",
                "Fraction A - [%]",
                "Oven temperature - [°C]",
                "Pump HP:Pressure - [bar]",
                "Pump HP:Actual flow - [µL/min]",
                "Pump A:Setpoint - []"
            ]
        );
        use ChromatogramType::*;
        assert_eq!(
            ch.iter().map(|c| c.chromatogram_type()).collect::<Vec<_>>(),
            [BasePeakChromatogram, PressureChromatogram, Unknown, TemperatureChromatogram, PressureChromatogram, FlowRateChromatogram, Unknown]
        );
        assert_eq!(
            ch.iter().map(|c| { let (v, _) = arrays(c); (v.name.clone(), v.unit) }).collect::<Vec<_>>(),
            [
                (ArrayType::IntensityArray, Unit::DetectorCounts),
                (ArrayType::PressureArray, Unit::Psi),
                (ArrayType::nonstandard("Fraction A - [%]"), Unit::Percent),
                (ArrayType::TemperatureArray, Unit::Celsius),
                (ArrayType::PressureArray, Unit::Pascal),
                (ArrayType::FlowRateArray, Unit::MicrolitersPerMinute),
                (ArrayType::nonstandard("Pump A:Setpoint - []"), Unit::Unknown),
            ]
        );
        let (psi, minutes) = arrays(&ch[1]);
        assert_eq!(psi.to_f32().unwrap().as_ref(), [3785.5f32, 3783.25, 3779.75], "chunks concatenate in rowid order");
        assert_eq!(minutes, [2.109 / 60.0, 2.5 / 60.0, 3.0 / 60.0]);
        let (bar, _) = arrays(&ch[4]);
        assert_eq!(bar.to_f32().unwrap().as_ref(), [18_000_000.0f32, 17_250_000.0], "bar is stated as pascal");
        assert!(traces.rescaled, "and the rescale is declared");
        assert_eq!(arrays(&ch[5]).1, [(1.75 + 1.5) / 60.0], "TimeOffset is seconds, added before the minute conversion");
        let title = ch[4].params().iter().find(|p| p.name == "chromatogram title").unwrap();
        assert_eq!((title.curie(), title.value.to_string()), (Some(mzdata::curie!(MS:1000809)), "Pump HP:Pressure - [bar]".to_string()));
        assert!(ch[4].params().iter().any(|p| p.name == "Instrument" && p.value.to_string() == "Agilent ICF System" && p.curie().is_none()));
    }

    /// A chunk whose blobs disagree in length is corrupt: its trace is skipped rather than
    /// re-aligned, and the other traces still read.
    #[test]
    fn a_malformed_chunk_drops_only_its_own_trace() {
        let c = hystar(
            &[(1, "Flow - [µl/min]", "Elute Pump", 6, 2, 0.0), (2, "Oven temperature - [°C]", "Elute Column Oven", 7, 5, 0.0)],
            &[(1, &[1.0, 2.0], &[400.0]), (2, &[1.0], &[50.0])],
        );
        let traces = read_traces(&c).unwrap();
        assert_eq!(traces.chromatograms.iter().map(|c| c.id()).collect::<Vec<_>>(), ["Oven temperature - [°C]"]);
        assert!(!traces.rescaled, "no trace here needed a rescale");
    }

    /// No `chromatography-data.sqlite` (every non-Bruker input, and `.d`s from before HyStar wrote
    /// one) is no traces, and looking for it creates nothing; a database without the tables is an
    /// error `read` turns into a warning.
    #[test]
    fn no_file_no_traces_and_nothing_created() {
        let d = std::env::temp_dir().join(format!("mzpc-hystar-none-{}.d", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let traces = read(&d);
        let entries = std::fs::read_dir(&d).unwrap().count();
        let _ = std::fs::remove_dir_all(&d);
        assert!(traces.chromatograms.is_empty());
        assert_eq!(entries, 0, "read() created a file in the input directory");
        assert!(read_traces(&Connection::open_in_memory().unwrap()).is_err());
    }
}
