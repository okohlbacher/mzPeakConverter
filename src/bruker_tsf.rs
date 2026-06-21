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
use rusqlite::Connection;

use mzdata::curie;
use mzdata::params::{Param, Unit};
use mzdata::prelude::ParamDescribed;
use mzdata::spectrum::bindata::{ArrayType, BinaryArrayMap, BinaryDataArrayType, DataArray};
use mzdata::spectrum::{
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

/// A TSF `.d` reader yielding one centroid [`MultiLayerSpectrum`] per frame.
pub struct TsfReader {
    bin: Vec<u8>,
    frames: Vec<Frame>,
    calib: TofMz,
}

impl TsfReader {
    pub fn open(dot_d: &Path) -> Result<Self> {
        let tsf = dot_d.join("analysis.tsf");
        let conn = Connection::open(&tsf).with_context(|| format!("opening {}", tsf.display()))?;

        // Calibration from GlobalMetadata (best-effort numeric parse).
        let meta = |key: &str| -> Result<f64> {
            conn.query_row(
                "SELECT Value FROM GlobalMetadata WHERE Key = ?1",
                [key],
                |r| r.get::<_, String>(0),
            )
            .ok()
            .and_then(|v| v.trim().parse::<f64>().ok())
            .with_context(|| format!("TSF GlobalMetadata missing/invalid {key}"))
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

        Ok(Self { bin, frames, calib })
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
        descr.add_param(Param::builder().name("mass spectrum").curie(curie!(MS:1000294)).build());
        let mut scan = ScanEvent::default();
        scan.start_time = frame.rt_seconds / 60.0; // mzdata scan start_time is minutes
        descr.acquisition.scans.push(scan);

        Ok(MultiLayerSpectrum::new(descr, Some(arrays), None, None))
    }

    /// A sample spectrum's array map, for deriving the writer's data-facet schema.
    pub fn sample_arrays(&self) -> Result<BinaryArrayMap> {
        // Use the first non-empty frame so the m/z + intensity columns are actually present.
        let i = (0..self.len())
            .find(|&i| self.frames[i].num_peaks > 0)
            .unwrap_or(0);
        self.spectrum(i)?
            .arrays
            .clone()
            .ok_or_else(|| anyhow::anyhow!("sample spectrum has no arrays"))
    }
}
