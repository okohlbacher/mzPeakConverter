//! Bruker TSF (line-spectra timsTOF / MALDI) reader → mzdata spectra (PLAN P3).
//!
//! mzdata reads only Bruker TDF, not TSF, and the `timsrust-tsf` crate pulls a rusqlite/
//! libsqlite3-sys that conflicts with mzdata's pinned one (two copies can't link `sqlite3`). So we
//! read TSF directly (decode ported from BRFP `src/tsf.rs`), reusing the `rusqlite`/`zstd` versions
//! mzdata already links:
//!   * `analysis.tsf` (SQLite) — `GlobalMetadata` for the sqrt calibration, `Frames` for rt / MS
//!     level / polarity / peak count / blob offset.
//!   * `analysis.tsf_bin` — per-frame chunk = 8-byte header `[padded:u32][compressed_len:u32]` then
//!     a zstd payload decoding to `[tof:f64 × n][intensity:f32 × n]`. m/z = `(a + b·tof)²`.
//! Each frame becomes one centroid mzdata spectrum for the standard writer path.

use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags};

use mzdata::meta::DissociationMethodTerm;
use mzdata::params::Unit;
use mzdata::spectrum::bindata::{ArrayType, BinaryArrayMap, BinaryDataArrayType, DataArray};
use mzdata::spectrum::{
    Activation, IsolationWindow, IsolationWindowState, Precursor, SelectedIon,
    MultiLayerSpectrum, ScanEvent, ScanPolarity, SignalContinuity, SpectrumDescription,
};

const TSF_CHUNK_HEADER_BYTES: usize = 8;

/// TOF→m/z sqrt calibration: `m/z = (a + b·tof)²`, `a = √(mz_min)`, `b = (√(mz_max) − a)/digitizer`.
#[derive(Clone, Copy)]
struct TofMz {
    a: f64,
    b: f64,
}
impl TofMz {
    #[inline]
    fn mz(&self, tof: f64) -> f64 {
        let v = self.a + self.b * tof;
        v * v
    }
}

const OTOF_CONTROL_SOFTWARE: &str = "Bruker otofControl";

struct Frame {
    id: i64,
    rt_seconds: f64,
    ms_level: u8,
    polarity: ScanPolarity,
    num_peaks: usize,
    offset: usize,
}

/// One `FrameMsMsInfo` row: what the instrument selected for an MS2 frame.
#[derive(Debug, Clone, Copy)]
struct MsMsInfo {
    parent: Option<i64>,
    trigger_mass: f64,
    isolation_width: f64,
    charge: Option<i32>,
    collision_energy: f64,
}

/// Precursors: `FrameMsMsInfo(Frame, Parent, TriggerMass, IsolationWidth, PrecursorCharge,
/// CollisionEnergy)`, one row per MS2 frame (measured: 3,486 rows for 3,486 MsMsType-2 frames, every
/// Parent an MS1 frame; PrecursorCharge stated on ~28 % of them, NULL elsewhere; CollisionEnergy
/// signed — negative in negative mode — and stored as stated, like the TDF lane). Keyed by frame; a
/// file without the table simply has no precursors. A free function so the mapping can be pinned on
/// an in-memory table: the corpus holds no TSF acquisition to pin it on.
fn read_msms(conn: &Connection) -> std::collections::HashMap<i64, MsMsInfo> {
    let mut msms = std::collections::HashMap::new();
    let Ok(mut stmt) = conn.prepare(
        "SELECT Frame, Parent, TriggerMass, IsolationWidth, PrecursorCharge, CollisionEnergy FROM FrameMsMsInfo",
    ) else {
        return msms;
    };
    let Ok(rows) = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, Option<i64>>(1)?,
            r.get::<_, Option<f64>>(2)?,
            r.get::<_, Option<f64>>(3)?,
            r.get::<_, Option<i64>>(4)?,
            r.get::<_, Option<f64>>(5)?,
        ))
    }) else {
        return msms;
    };
    for (frame, parent, trigger, width, charge, ce) in rows.flatten() {
        let Some(trigger_mass) = trigger.filter(|m| m.is_finite() && *m > 0.0) else { continue };
        msms.insert(
            frame,
            MsMsInfo {
                parent: parent.filter(|p| *p > 0),
                trigger_mass,
                isolation_width: width.filter(|w| w.is_finite() && *w > 0.0).unwrap_or(0.0),
                charge: charge.and_then(|c| i32::try_from(c).ok()).filter(|c| *c != 0),
                collision_energy: ce.filter(|e| e.is_finite()).unwrap_or(0.0),
            },
        );
    }
    msms
}

/// A TSF `.d` reader yielding one centroid [`MultiLayerSpectrum`] per frame.
pub struct TsfReader {
    bin: Vec<u8>,
    frames: Vec<Frame>,
    calib: TofMz,
    /// Keyed by frame id. Absent for MS1 frames and for files without the table.
    msms: std::collections::HashMap<i64, MsMsInfo>,
}

impl TsfReader {
    pub fn open(dot_d: &Path) -> Result<Self> {
        let tsf = dot_d.join("analysis.tsf");
        // Read-only. A plain `Connection::open` is read-write and CREATES a missing file, so opening a
        // `.d` that has no TSF left an empty `analysis.tsf` inside the user's raw data — the corpus
        // still holds one, beside a real TDF, and it had passed for a TSF fixture.
        let conn = Connection::open_with_flags(&tsf, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
            .with_context(|| format!("opening {}", tsf.display()))?;

        // Calibration from GlobalMetadata. SQLite's own error stays in the chain: a read-only open of a
        // file with a hot journal fails here, and "missing/invalid" would have misreported it.
        let meta = |key: &str| -> Result<f64> {
            let v: String = conn
                .query_row("SELECT Value FROM GlobalMetadata WHERE Key = ?1", [key], |r| r.get(0))
                .with_context(|| format!("reading TSF GlobalMetadata {key} from {}", tsf.display()))?;
            v.trim().parse::<f64>().with_context(|| format!("TSF GlobalMetadata {key} is not a number: {v:?}"))
        };
        let mut mz_min = meta("MzAcqRangeLower")?;
        let mut mz_max = meta("MzAcqRangeUpper")?;
        let digitizer = meta("DigitizerNumSamples")?;
        // Bruker otofControl runs need the acquisition range widened by ±5 Th before fitting the
        // sqrt model (matches BRFP / the vendor SDK). Without it m/z is systematically off.
        let otof = conn
            .query_row("SELECT Value FROM GlobalMetadata WHERE Key='AcquisitionSoftware'", [], |r| {
                r.get::<_, String>(0)
            })
            .map(|v| v.trim() == OTOF_CONTROL_SOFTWARE)
            .unwrap_or(false);
        if otof {
            mz_min -= 5.0;
            mz_max += 5.0;
        }
        if !mz_min.is_finite() || mz_min <= 0.0 {
            bail!("TSF MzAcqRangeLower must be finite and positive (got {mz_min})");
        }
        if !mz_max.is_finite() || mz_max < mz_min {
            bail!("TSF MzAcqRangeUpper must be finite and >= lower (got {mz_max})");
        }
        if !digitizer.is_finite() || digitizer <= 0.0 {
            bail!("TSF DigitizerNumSamples must be positive (got {digitizer})");
        }
        let a = mz_min.sqrt();
        let calib = TofMz { a, b: (mz_max.sqrt() - a) / digitizer };

        // Frames in Id order (matches the tsf_bin layout / native scan order). MsMsType: 0=MS1,
        // 3=MS3, everything else (2 / PASEF 8-10) is treated MS2. Negative NumPeaks/TimsId are
        // rejected (a negative cast to usize would be enormous).
        let mut stmt = conn
            .prepare("SELECT Id, Time, MsMsType, Polarity, NumPeaks, TimsId FROM Frames ORDER BY Id")
            .context("preparing Frames query")?;
        let frames = stmt
            .query_map([], |r| {
                let polarity: String = r.get(3)?;
                let num_peaks: i64 = r.get(4)?;
                let offset: i64 = r.get(5)?;
                Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?, r.get::<_, i64>(2)?, polarity, num_peaks, offset))
            })
            .context("querying Frames")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("reading Frames rows")?
            .into_iter()
            .map(|(id, time, msms, polarity, num_peaks, offset)| {
                if num_peaks < 0 || offset < 0 {
                    bail!("TSF frame {id}: negative NumPeaks ({num_peaks}) or TimsId ({offset})");
                }
                Ok(Frame {
                    id,
                    rt_seconds: time,
                    ms_level: match msms {
                        0 => 1,
                        3 => 3,
                        _ => 2,
                    },
                    polarity: match polarity.trim() {
                        "+" => ScanPolarity::Positive,
                        "-" => ScanPolarity::Negative,
                        _ => ScanPolarity::Unknown,
                    },
                    num_peaks: num_peaks as usize,
                    offset: offset as usize,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let bin = std::fs::read(dot_d.join("analysis.tsf_bin"))
            .with_context(|| format!("reading {}", dot_d.join("analysis.tsf_bin").display()))?;

        let msms = read_msms(&conn);
        Ok(Self { bin, frames, calib, msms })
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// Decode one frame's `(m/z, intensity)` peak list from the zstd-chunked tsf_bin.
    fn peaks(&self, frame: &Frame) -> Result<(Vec<f64>, Vec<f32>)> {
        if frame.num_peaks == 0 {
            return Ok((Vec::new(), Vec::new()));
        }
        let off = frame.offset;
        let header_end = off.checked_add(TSF_CHUNK_HEADER_BYTES).context("TSF offset overflow")?;
        let header = self.bin.get(off..header_end).context("TSF chunk header out of range")?;
        let padded = u32::from_le_bytes(header[0..4].try_into().expect("4 bytes")) as usize;
        let compressed_len = u32::from_le_bytes(header[4..8].try_into().expect("4 bytes")) as usize;
        if padded < TSF_CHUNK_HEADER_BYTES || padded < compressed_len {
            bail!("invalid TSF chunk header at {off}: padded={padded}, compressed={compressed_len}");
        }
        let end = header_end.checked_add(compressed_len).context("TSF chunk overflow")?;
        let compressed = self.bin.get(header_end..end).context("TSF compressed chunk out of range")?;

        let expected = frame.num_peaks.checked_mul(12).context("TSF size overflow")?; // [tof:f64][int:f32]
        let mut decompressed = Vec::with_capacity(expected);
        zstd::stream::read::Decoder::new(compressed)
            .context("init TSF zstd decoder")?
            .read_to_end(&mut decompressed)
            .context("decompress TSF chunk")?;
        // EXACT length: a longer payload means NumPeaks under-counts and the tof/intensity split
        // would land mid-array, silently producing garbage. Require the precise expected size.
        if decompressed.len() != expected {
            bail!("TSF chunk size {} != expected {expected} (num_peaks={})", decompressed.len(), frame.num_peaks);
        }
        let (tof_bytes, int_bytes) = decompressed[..expected].split_at(frame.num_peaks * 8);
        let mz = tof_bytes
            .chunks_exact(8)
            .map(|c| self.calib.mz(f64::from_le_bytes(c.try_into().expect("8 bytes"))))
            .collect();
        let intensity = int_bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes(c.try_into().expect("4 bytes")))
            .collect();
        Ok((mz, intensity))
    }

    /// Build the centroid mzdata spectrum for frame `i` (0-based reader order).
    pub fn spectrum(&self, i: usize) -> Result<MultiLayerSpectrum> {
        let frame = self.frames.get(i).with_context(|| format!("TSF frame index {i} out of range"))?;
        // An MSn frame without a `FrameMsMsInfo` row is a genuine orphan: say so once, loudly,
        // because the archive would otherwise be indistinguishable from a complete one.
        if frame.ms_level > 1 && !self.msms.contains_key(&frame.id) {
            static PRECURSOR_GAP_SAID: std::sync::Once = std::sync::Once::new();
            PRECURSOR_GAP_SAID.call_once(|| {
                log::warn!(
                    "Bruker TSF (native): frame {} is MS{} but analysis.tsf has no FrameMsMsInfo row for it; \
                     such rows are written without a precursor",
                    frame.id, frame.ms_level
                );
            });
        }
        let (mz, intensity) = self.peaks(frame)?;

        let mut arrays = BinaryArrayMap::new();
        let mut mz_da = DataArray::wrap(&ArrayType::MZArray, BinaryDataArrayType::Float64, Vec::new());
        mz_da.update_buffer(mz.as_slice()).map_err(|e| anyhow::anyhow!("encoding m/z: {e}"))?;
        mz_da.unit = Unit::MZ;
        arrays.add(mz_da);
        let mut int_da =
            DataArray::wrap(&ArrayType::IntensityArray, BinaryDataArrayType::Float32, Vec::new());
        int_da.update_buffer(intensity.as_slice()).map_err(|e| anyhow::anyhow!("encoding intensity: {e}"))?;
        int_da.unit = Unit::DetectorCounts;
        arrays.add(int_da);

        let mut descr = SpectrumDescription {
            id: format!("frame={}", frame.id),
            index: i,
            ms_level: frame.ms_level,
            signal_continuity: SignalContinuity::Centroid,
            polarity: frame.polarity,
            ..Default::default()
        };
        // No blanket `MS:1000294 "mass spectrum"` here (0.9.13). mzdata's `spectrum_type()` is a first-match
        // lookup, so that parent term wins over the specific one and the writer's inference
        // (`writer/visitor.rs`: ms_level 1 -> MS:1000579, else MS:1000580) never runs; with it absent the
        // writer types each row from `ms_level`, as the mzML, Shimadzu and Bruker-native lanes already do.
        let mut scan = ScanEvent::default();
        scan.start_time = frame.rt_seconds / 60.0; // mzdata scan start_time is minutes
        descr.acquisition.scans.push(scan);

        if let Some(m) = self.msms.get(&frame.id) {
            descr.precursor.push(Self::precursor(m));
        }

        Ok(MultiLayerSpectrum::new(descr, Some(arrays), None, None))
    }

    /// The vendor's selection, verbatim: selected ion = `TriggerMass` (with the stated charge when
    /// there is one), isolation window `TriggerMass ± IsolationWidth/2` — or target-only when the
    /// width is not stated, which the writer keeps as null offsets — CID at the stated (signed)
    /// collision energy, and `precursor_id = frame=<Parent>` so the writer resolves the MS1 it was
    /// selected from. ProteoWizard emits the same window and ion for TSF but no energy and no parent.
    fn precursor(m: &MsMsInfo) -> Precursor {
        let ion = SelectedIon { mz: m.trigger_mass, charge: m.charge, ..Default::default() };
        let half = (m.isolation_width / 2.0) as f32;
        let target = m.trigger_mass as f32;
        let mut activation = Activation::default();
        activation.energy = m.collision_energy as f32;
        activation.methods_mut().push(DissociationMethodTerm::CollisionInducedDissociation);
        Precursor {
            ions: vec![ion],
            isolation_window: IsolationWindow {
                target,
                lower_bound: if half > 0.0 { target - half } else { 0.0 },
                upper_bound: if half > 0.0 { target + half } else { 0.0 },
                flags: IsolationWindowState::Complete,
            },
            activation,
            precursor_id: m.parent.map(|p| format!("frame={p}")),
            ..Default::default()
        }
    }

}

#[cfg(test)]
mod msms_tests {
    use super::*;

    fn frame_msms_info(rows: &str) -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(&format!(
            "CREATE TABLE FrameMsMsInfo (Frame INTEGER, Parent INTEGER, TriggerMass REAL, IsolationWidth REAL,
                                         PrecursorCharge INTEGER, CollisionEnergy REAL); {rows}"
        ))
        .unwrap();
        c
    }

    /// The mapping the TSF precursors (0.11.3) hang on. The end-to-end pin in
    /// tests/run_metadata_native.rs needs a TSF acquisition the corpus does not have, so without this
    /// the feature had no automated check at all.
    #[test]
    fn frame_msms_info_becomes_precursors() {
        let m = read_msms(&frame_msms_info(
            "INSERT INTO FrameMsMsInfo VALUES (2, 1, 301.5, 2.0, 2, -25.0);
             INSERT INTO FrameMsMsInfo VALUES (3, 1, 402.25, 3.0, NULL, 30.0);
             INSERT INTO FrameMsMsInfo VALUES (4, 0, 503.0, 2.0, 0, 30.0);
             INSERT INTO FrameMsMsInfo VALUES (5, 1, 0.0, 2.0, 2, 30.0);
             INSERT INTO FrameMsMsInfo VALUES (6, -1, 604.0, NULL, 1, NULL);",
        ));
        let p = &m[&2];
        assert_eq!((p.parent, p.trigger_mass, p.isolation_width, p.charge, p.collision_energy), (Some(1), 301.5, 2.0, Some(2), -25.0),
            "a stated charge and a signed collision energy are carried as stated");
        assert_eq!(m[&3].charge, None, "an unstated charge stays unknown, never invented");
        assert_eq!((m[&4].parent, m[&4].charge), (None, None), "Parent 0 and charge 0 mean unknown");
        assert!(!m.contains_key(&5), "a row without a trigger mass is no precursor");
        assert_eq!((m[&6].parent, m[&6].isolation_width, m[&6].collision_energy), (None, 0.0, 0.0));
        assert_eq!(m.len(), 4);
    }

    #[test]
    fn a_file_without_the_table_has_no_precursors() {
        assert!(read_msms(&Connection::open_in_memory().unwrap()).is_empty());
    }

    /// Opening a `.d` that has no `analysis.tsf` must fail without creating one in the input.
    #[test]
    fn open_never_writes_into_the_input_directory() {
        let d = std::env::temp_dir().join(format!("mzpc-tsf-nostub-{}.d", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let opened = TsfReader::open(&d).is_ok();
        let created = d.join("analysis.tsf").exists();
        let _ = std::fs::remove_dir_all(&d);
        assert!(!opened, "a directory without analysis.tsf is not a TSF run");
        assert!(!created, "TsfReader::open created analysis.tsf inside the input directory");
    }
}
