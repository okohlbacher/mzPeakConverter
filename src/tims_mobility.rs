//! Bruker timsTOF mobility calibration (scan → 1/K0): the `TimsCalibration` `ModelType = 2` model
//! stored in `analysis.tdf`, evaluated exactly as Bruker's `timsdata` SDK
//! (`tims_scannum_to_oneoverk0`) evaluates it.
//!
//! timsrust (which both mzdata's TDF reader and our [`crate::bruker_native`] path wrap) approximates
//! 1/K0 by linearly interpolating the nominal acquisition range — ~0.03 Vs·s/cm² off at the
//! high-mobility edge vs the SDK. The vendor model is
//!
//! ```text
//!   W(scan) = C2 + (C3 - C2) * (scan - C4 - C0) / C1    // TIMS ramp voltage; the ramp starts C4 + C0 scans in
//!   1/K0    = W / (C7 + C6 * W)                          // rational voltage → mobility model
//! ```
//!
//! with `scan` 0-based. `C0` and `C5` are 1 in every file seen (161 PRIDE timsTOF datasets) and
//! `C8`/`C9` do not enter. Identified on 2026-09-15 by exact point pairing against the SDK archives
//! of all seven corpus timsTOF runs (PXD059079 2485, bruker-timstof-pro SBA415, MSV000099123 8225,
//! MSV000092457 13373, PXD078573 9629, PXD076703 2095, PXD079300 27806; timsControl 4.0.5 – 6.2,
//! 902 – 1552 scans, C6 of either sign): max |Δ1/K0| 6.7e-16 over every scan of every run, the
//! three `CalibrationInfo` reference ions (`MeasuredTimsVoltages` → `MobilitiesCorrectedCalibration`)
//! reproduced exactly, no dependence on `Frames.Pressure`. mzdata 0.66's `TimsCalibrationModel2`
//! (`io/tdf/calibration.rs`, which the `--no-ims-compact` lane's signal arrays go through) evaluates
//! the same expression, `1/(C6 + C7/(offset + slope·scan))` with `slope = (C3 - C2)/C1` and
//! `offset = C2 - slope·(C4 + C0)`. Releases v0.9.6 – v0.12.4 evaluated the rational on a ramp
//! `C2 + (C3 - C2)(scan - C0)/(C1 - C0)` with an offset anchored on `OneOverK0AcqRangeLower`; that
//! was off by up to 1.7e-3 (one to three scan steps).
//! **ModelType-2 only** — the C-columns mean different things for other model types, so
//! [`from_tdf_path`](TimsMobilityCalibration::from_tdf_path) returns `None` for anything else and
//! the caller keeps the linear path.

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension};

/// Bruker timsTOF `ModelType = 2` mobility calibration: mobility scan index → 1/K0 (Vs·s/cm²).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TimsMobilityCalibration {
    c0: f64,
    c1: f64,
    c2: f64,
    c3: f64,
    c4: f64,
    c6: f64,
    c7: f64,
}

impl TimsMobilityCalibration {
    /// Build from the raw `TimsCalibration` coefficients that enter the model.
    pub fn new(c0: f64, c1: f64, c2: f64, c3: f64, c4: f64, c6: f64, c7: f64) -> Self {
        Self { c0, c1, c2, c3, c4, c6, c7 }
    }

    /// TIMS ramp voltage at a (possibly fractional) 0-based mobility scan index.
    #[inline]
    pub fn voltage(&self, scan: f64) -> f64 {
        if self.c1 == 0.0 {
            return self.c2;
        }
        self.c2 + (self.c3 - self.c2) * (scan - self.c4 - self.c0) / self.c1
    }

    /// Inverse reduced ion mobility 1/K0 (Vs·s/cm²) for a 0-based mobility scan index (fractional
    /// indices interpolate on the ramp, as the SDK does).
    #[inline]
    pub fn one_over_k0(&self, scan: f64) -> f64 {
        let w = self.voltage(scan);
        w / (self.c7 + self.c6 * w)
    }

    /// Load from an open `analysis.tdf` connection. `Ok(None)` when there is no `ModelType = 2` row
    /// (caller must fall back to the linear approximation — this model is type-2-specific).
    pub fn from_tdf(conn: &Connection) -> Result<Option<Self>> {
        let row = conn
            .query_row(
                "SELECT C0, C1, C2, C3, C4, C6, C7 FROM TimsCalibration WHERE ModelType = 2 ORDER BY Id LIMIT 1",
                [],
                |r| Ok((r.get::<_, f64>(0)?, r.get::<_, f64>(1)?, r.get::<_, f64>(2)?, r.get::<_, f64>(3)?,
                        r.get::<_, f64>(4)?, r.get::<_, f64>(5)?, r.get::<_, f64>(6)?)),
            )
            .optional()
            .context("reading TimsCalibration")?;
        Ok(row.map(|(c0, c1, c2, c3, c4, c6, c7)| Self::new(c0, c1, c2, c3, c4, c6, c7)))
    }

    /// Convenience: open `analysis.tdf` read-only and load the calibration.
    pub fn from_tdf_path(tdf: &Path) -> Result<Option<Self>> {
        let conn = Connection::open_with_flags(
            tdf,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("opening {}", tdf.display()))?;
        Self::from_tdf(&conn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SDK goldens: `tims_scannum_to_oneoverk0` at whole scans, read back from `--bruker-sdk`
    /// archives of two corpus runs (the SDK lane stores the SDK's own 1/K0 per point).
    #[test]
    fn reproduces_vendor_sdk_per_scan() {
        // PXD059079 2485.d: timsControl 4.0.5, 1552 scans, nominal range 0.70..1.45.
        let cal = TimsMobilityCalibration::new(
            1.0, 1551.0, 254.40951107260733, 118.71749047939912, 33.64485981308411,
            0.012463618472198826, 172.2839721407802,
        );
        for (scan, sdk) in [
            (34.0, 1.4503157332197063),
            (500.0, 1.221493245336357),
            (1000.0, 0.9744927134550074),
            (1551.0, 0.7005033292661802),
        ] {
            assert!((cal.one_over_k0(scan) - sdk).abs() < 1e-12, "2485 scan {scan}: {} vs SDK {sdk}", cal.one_over_k0(scan));
        }
        // bruker-timstof-pro SBA415: timsControl 6.0.6, 910 scans, nominal range 0.6..1.6.
        let cal = TimsMobilityCalibration::new(
            1.0, 909.0, 211.45198604901222, 73.95258004355563, 32.72727272727273,
            0.00492817555366883, 131.11541877221117,
        );
        for (scan, sdk) in [
            (188.0, 1.4246626738807122),
            (500.0, 1.0691266948698679),
            (700.0, 0.8405583294217701),
            (868.0, 0.6481604756745163),
        ] {
            assert!((cal.one_over_k0(scan) - sdk).abs() < 1e-12, "SBA415 scan {scan}: {} vs SDK {sdk}", cal.one_over_k0(scan));
        }
        // The nominal acquisition range is NOT what the model returns at the ends (the SDK agrees).
        assert!((cal.one_over_k0(909.0) - 0.6011506084764608).abs() < 1e-12);
        assert!((cal.one_over_k0(0.0) - 1.6382917966084387).abs() < 1e-12);
        assert!(cal.one_over_k0(100.0) > cal.one_over_k0(800.0)); // monotonic
    }
}

#[cfg(test)]
mod scan_number_precision_tests {
    use super::*;

    /// The isolation window's integer midpoint is NOT the precursor's mobility: `Precursors.
    /// ScanNumber` is fractional. Pin the size of the error we were making on a real DDA frame.
    ///
    /// The calibration is PXD078573 `9629.d`'s `TimsCalibration` row, loaded through `from_tdf`
    /// from an in-memory table holding the columns it reads — so the SQL path stays covered and the
    /// test runs everywhere, rather than only beside a 1.5 GB corpus run where it returned without
    /// a word when `MZPEAK_CORPUS` was unset.
    #[test]
    fn fractional_scan_number_moves_mobility() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE TimsCalibration (Id INTEGER, ModelType INTEGER, C0 REAL, C1 REAL, C2 REAL, C3 REAL,
                                           C4 REAL, C5 REAL, C6 REAL, C7 REAL, C8 REAL, C9 REAL);
             INSERT INTO TimsCalibration VALUES (1, 2, 1, 1509, 187.64779662120463, 78.53435821105788,
                                                 32.72727272727273, 1, -0.007497305527082165, 135.44099330647055,
                                                 13.4184398563705, 1708.689491089318);",
        )
        .unwrap();
        let cal = TimsMobilityCalibration::from_tdf(&conn).unwrap().expect("a ModelType-2 row");
        // Frame 2, first window: ScanNumBegin=735, ScanNumEnd=759 -> old midpoint 747.
        // Precursors.ScanNumber for that precursor = 747.5194174757281.
        let old = cal.one_over_k0(747.0);
        let new = cal.one_over_k0(747.5194174757281);
        // The vendor model at both positions (same arithmetic evaluated on the real row).
        assert!((old - 1.0122848417736254).abs() < 1e-12, "{old}");
        assert!((new - 1.0120033137012396).abs() < 1e-12, "{new}");
        assert!((new - old).abs() > 1e-5, "fractional scan number must move 1/K0 measurably: {old} vs {new}");
    }

    #[test]
    fn other_model_types_are_declined() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE TimsCalibration (Id INTEGER, ModelType INTEGER, C0 REAL, C1 REAL, C2 REAL, C3 REAL,
                                           C4 REAL, C5 REAL, C6 REAL, C7 REAL, C8 REAL, C9 REAL);
             INSERT INTO TimsCalibration VALUES (1, 1, 1, 1509, 0, 0, 0, 0, 0, 0, 0, 0);",
        )
        .unwrap();
        assert!(TimsMobilityCalibration::from_tdf(&conn).unwrap().is_none());
    }
}
