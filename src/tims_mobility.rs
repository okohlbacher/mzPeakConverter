//! Vendor-grade Bruker timsTOF mobility recalibration (scan → 1/K0) from the `TimsCalibration`
//! `ModelType = 2` model stored in `analysis.tdf`.
//!
//! timsrust (which both mzdata's TDF reader and our [`crate::bruker_native`] path wrap) approximates
//! 1/K0 by linearly interpolating the nominal acquisition range — ~0.03 Vs·s/cm² off at the
//! high-mobility edge vs Bruker's `timsdata` SDK. This evaluates the actual vendor model:
//!
//! ```text
//!   V(scan) = C2 + (C3 - C2) * (scan - C0) / (C1 - C0)     // linear TIMS voltage ramp
//!   1/K0    = (V + delta) / (C7 + C6 * V)                  // vendor rational model
//! ```
//!
//! `delta` is anchored so the lowest-mobility scan (V = C3) reproduces
//! `GlobalMetadata.OneOverK0AcqRangeLower`. Reverse-engineered + validated against the Bruker SDK on
//! 68 PRIDE timsTOF datasets (nscans 473..2831): per-dataset max error median 1.4e-3 / worst 2.8e-3
//! Vs·s/cm², vs ~3.0e-2 for the linear approximation. **ModelType-2 only** — the C-columns mean
//! different things for other model types, so [`from_tdf_path`](TimsMobilityCalibration::from_tdf_path)
//! returns `None` for anything else and the caller keeps the linear path.

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
    c6: f64,
    c7: f64,
    /// Voltage offset, anchored so V = C3 maps to `OneOverK0AcqRangeLower`.
    delta: f64,
}

impl TimsMobilityCalibration {
    /// Build from the raw `TimsCalibration` coefficients + `GlobalMetadata.OneOverK0AcqRangeLower`.
    pub fn new(c0: f64, c1: f64, c2: f64, c3: f64, c6: f64, c7: f64, one_over_k0_lower: f64) -> Self {
        // Anchor: at the lowest-mobility scan V = C3,
        //   one_over_k0_lower = (C3 + delta)/(C7 + C6*C3)  =>  delta = lower*(C7 + C6*C3) - C3.
        let delta = one_over_k0_lower * (c7 + c6 * c3) - c3;
        Self { c0, c1, c2, c3, c6, c7, delta }
    }

    /// TIMS ramp voltage at a (possibly fractional) mobility scan index.
    #[inline]
    pub fn voltage(&self, scan: f64) -> f64 {
        let span = self.c1 - self.c0;
        if span == 0.0 {
            return self.c2;
        }
        self.c2 + (self.c3 - self.c2) * (scan - self.c0) / span
    }

    /// Inverse reduced ion mobility 1/K0 (Vs·s/cm²) for a 0-based mobility scan index.
    #[inline]
    pub fn one_over_k0(&self, scan: f64) -> f64 {
        let v = self.voltage(scan);
        let denom = self.c7 + self.c6 * v;
        // A malformed ModelType-2 row could drive the denominator to 0; return NaN rather than
        // writing ±inf mobility into the archive.
        if denom.abs() < f64::EPSILON {
            return f64::NAN;
        }
        (v + self.delta) / denom
    }

    /// Load from an open `analysis.tdf` connection. `Ok(None)` when there is no `ModelType = 2` row
    /// (caller must fall back to the linear approximation — this model is type-2-specific).
    pub fn from_tdf(conn: &Connection) -> Result<Option<Self>> {
        let row = conn
            .query_row(
                "SELECT C0, C1, C2, C3, C6, C7 FROM TimsCalibration WHERE ModelType = 2 ORDER BY Id LIMIT 1",
                [],
                |r| Ok((r.get::<_, f64>(0)?, r.get::<_, f64>(1)?, r.get::<_, f64>(2)?,
                        r.get::<_, f64>(3)?, r.get::<_, f64>(4)?, r.get::<_, f64>(5)?)),
            )
            .optional()
            .context("reading TimsCalibration")?;
        let Some((c0, c1, c2, c3, c6, c7)) = row else {
            return Ok(None);
        };
        let lower: f64 = conn
            .query_row(
                "SELECT CAST(Value AS REAL) FROM GlobalMetadata WHERE Key = 'OneOverK0AcqRangeLower'",
                [],
                |r| r.get(0),
            )
            .context("reading OneOverK0AcqRangeLower")?;
        Ok(Some(Self::new(c0, c1, c2, c3, c6, c7, lower)))
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

    #[test]
    fn reproduces_vendor_sdk_at_endpoints() {
        // SBA415 TimsCalibration row + acq-range lower; SDK 1/K0 ground truth at the endpoints.
        let cal = TimsMobilityCalibration::new(
            1.0, 909.0, 211.45198604901222, 73.95258004355563, 0.00492817555366883,
            131.11541877221117, 0.600,
        );
        assert!((cal.one_over_k0(909.0) - 0.600).abs() < 1e-9); // anchored at last scan
        assert!((cal.one_over_k0(0.0) - 1.6385).abs() < 1e-3); // SDK 1.6383 (linear gives 1.600)
        assert!(cal.one_over_k0(100.0) > cal.one_over_k0(800.0)); // monotonic
    }
}
