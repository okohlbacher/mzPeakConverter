//! Bruker timsTOF mobility calibration: mobility scan -> 1/K0, exactly as the vendor SDK.
//!
//! Drop-in for the reader side (mzdata's `io::tdf`, or any TDF reader). timsrust/mzdata currently
//! approximate 1/K0 by linearly interpolating the *nominal* acquisition range, which is ~0.03
//! Vs·s/cm² off at the high-mobility edge vs Bruker's `timsdata` SDK. This evaluates the actual
//! `ModelType = 2` calibration stored in `analysis.tdf` (`TimsCalibration` table):
//!
//! ```text
//!   W(scan) = C2 + (C3 - C2) * (scan - C4 - C0) / C1    // TIMS ramp voltage; the ramp starts C4 + C0 scans in
//!   1/K0    = W / (C7 + C6 * W)                          // rational voltage -> mobility model
//! ```
//!
//! `scan` is 0-based (the SDK's `tims_scannum_to_oneoverk0` convention). `C0` and `C5` are 1 in every
//! file seen (161 PRIDE timsTOF datasets); `C5`, `C8` and `C9` do not enter. mzdata 0.66's
//! `TimsCalibrationModel2` (`io/tdf/calibration.rs`) evaluates the same expression as
//! `1/(C6 + C7/(offset + slope*scan))`, `slope = (C3 - C2)/C1`, `offset = C2 - slope*(C4 + C0)`.
//!
//! Validation (2026-09-15): identified by exact point pairing against `--bruker-sdk` archives (the
//! SDK's own 1/K0 per point) of all seven corpus timsTOF runs — PXD059079 2485, bruker-timstof-pro
//! SBA415, MSV000099123 8225, MSV000092457 13373, PXD078573 9629, PXD076703 2095, PXD079300 27806
//! (timsControl 4.0.5 – 6.2, 902–1552 scans, C6 of either sign): max |Δ1/K0| **6.7e-16** over every
//! scan of every run. It also reproduces the three `CalibrationInfo` reference
//! ions (`MeasuredTimsVoltages` -> `MobilitiesCorrectedCalibration`) exactly, and the SDK values do
//! not change with `Frames.Pressure` (checked at the run's min/max pressure frames). The earlier
//! reverse-engineered form (ramp `(scan - C0)/(C1 - C0)`, offset anchored on
//! `GlobalMetadata.OneOverK0AcqRangeLower`) was off by up to 1.7e-3 — one to three scan steps.
//!
//! Note the nominal `OneOverK0AcqRangeLower/Upper` are NOT what the model returns at the first/last
//! scan (SBA415: 0.6012 / 1.6383 for a nominal 0.6 / 1.6); the SDK agrees with the model.

/// Bruker timsTOF `ModelType = 2` mobility calibration: mobility scan index -> 1/K0 (Vs·s/cm²).
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
    /// Build from the raw `TimsCalibration` coefficients that enter ModelType 2.
    pub fn new(c0: f64, c1: f64, c2: f64, c3: f64, c4: f64, c6: f64, c7: f64) -> Self {
        Self { c0, c1, c2, c3, c4, c6, c7 }
    }

    /// TIMS ramp voltage at a (possibly fractional) 0-based mobility scan index.
    #[inline]
    pub fn voltage(&self, scan: f64) -> f64 {
        // C1 == 0 only for a degenerate single-scan frame; guard to avoid NaN.
        if self.c1 == 0.0 {
            return self.c2;
        }
        self.c2 + (self.c3 - self.c2) * (scan - self.c4 - self.c0) / self.c1
    }

    /// Inverse reduced ion mobility 1/K0 (Vs·s/cm²) for a mobility scan index (0-based; fractional
    /// indices interpolate, matching the SDK).
    #[inline]
    pub fn one_over_k0(&self, scan: f64) -> f64 {
        let w = self.voltage(scan);
        w / (self.c7 + self.c6 * w)
    }
}

// --- optional SQLite loader (gate behind whatever feature pulls in rusqlite) ----------------------

#[cfg(feature = "rusqlite")]
impl TimsMobilityCalibration {
    /// Load the calibration from an open `analysis.tdf` connection.
    ///
    /// Returns `Ok(None)` when there is **no `ModelType = 2` row** — this model is type-2-specific
    /// (the C-columns mean different things for other model types), so the caller MUST fall back to
    /// the existing linear approximation rather than misapplying this rational. Across 161 sampled
    /// timsTOF datasets every row was ModelType 2, but older/legacy acquisitions may differ.
    pub fn from_tdf(conn: &rusqlite::Connection) -> rusqlite::Result<Option<Self>> {
        use rusqlite::OptionalExtension;
        let row = conn
            .query_row(
                "SELECT C0, C1, C2, C3, C4, C6, C7 FROM TimsCalibration WHERE ModelType = 2 ORDER BY Id LIMIT 1",
                [],
                |r| Ok((r.get::<_, f64>(0)?, r.get::<_, f64>(1)?, r.get::<_, f64>(2)?, r.get::<_, f64>(3)?,
                        r.get::<_, f64>(4)?, r.get::<_, f64>(5)?, r.get::<_, f64>(6)?)),
            )
            .optional()?;
        Ok(row.map(|(c0, c1, c2, c3, c4, c6, c7)| Self::new(c0, c1, c2, c3, c4, c6, c7)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // SDK goldens (`tims_scannum_to_oneoverk0` at whole scans) from `--bruker-sdk` archives.
    #[test]
    fn reproduces_vendor_sdk_per_scan() {
        // PXD059079 2485.d: 1552 scans, nominal range 0.70..1.45.
        let cal = TimsMobilityCalibration::new(
            1.0, 1551.0, 254.40951107260733, 118.71749047939912, 33.64485981308411,
            0.012463618472198826, 172.2839721407802,
        );
        for (scan, sdk) in [(34.0, 1.4503157332197063), (500.0, 1.221493245336357), (1000.0, 0.9744927134550074), (1551.0, 0.7005033292661802)] {
            assert!((cal.one_over_k0(scan) - sdk).abs() < 1e-12);
        }
        // bruker-timstof-pro SBA415: 910 scans, nominal range 0.6..1.6.
        let cal = TimsMobilityCalibration::new(
            1.0, 909.0, 211.45198604901222, 73.95258004355563, 32.72727272727273,
            0.00492817555366883, 131.11541877221117,
        );
        for (scan, sdk) in [(188.0, 1.4246626738807122), (500.0, 1.0691266948698679), (700.0, 0.8405583294217701), (868.0, 0.6481604756745163)] {
            assert!((cal.one_over_k0(scan) - sdk).abs() < 1e-12);
        }
        assert!(cal.one_over_k0(100.0) > cal.one_over_k0(800.0)); // monotonic decreasing in scan
    }
}
