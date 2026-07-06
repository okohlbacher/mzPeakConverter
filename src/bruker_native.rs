//! Native Bruker TDF integer-TOF reader + `ims-compact` encoder (PLAN P2).
//!
//! mzdata's TDF API converts the raw `u32` TOF bins to `f64` m/z and discards the integer
//! (`io/tdf/arrays.rs`), so a *lossless* compact encoder cannot get its inputs there. We read the
//! native frames via `timsrust` — the exact crate mzdata wraps — so the TOF bins are the true
//! instrument values, not a derived `round((√mz-a)/b)`. The reader is exposed behind the
//! [`NativeTofReader`] capability so it can later be re-pointed at an upstream mzdata accessor
//! without touching the encoder (NATIVE-TOF-DESIGN.md).
//!
//! Encoding (ported from BRFP `write_tdf_to_ims_compact`): rows grouped by `spectrum_index`
//! (frame), within a frame mobility-major (scan ascending) then TOF ascending; the TOF column is
//! **delta-reset per (frame, scan)** — first peak of each scan holds the absolute bin, the rest
//! hold non-negative increments. Lossless: `m/z = (a + b·tof)²` with `a,b` stored in file KV
//! metadata; the decoder recovers the exact integer TOF and thus the exact m/z the instrument
//! calibration produces.

use std::path::Path;

use anyhow::{Result, bail};

use mzdata::curie;
use mzdata::params::{Param, Unit};
use mzdata::prelude::ParamDescribed;
use mzdata::spectrum::bindata::{ArrayType, BinaryArrayMap, BinaryDataArrayType, DataArray};
use mzdata::spectrum::{MultiLayerSpectrum, SignalContinuity, SpectrumDescription};

use timsrust::converters::{ConvertableDomain, Scan2ImConverter, Tof2MzConverter};
use timsrust::readers::{FrameReader, MetadataReader};
use timsrust::MSLevel;

/// The TOF→m/z calibration model: `m/z = (a + b·tof)²`. `a = √(mz_min)`, `b = (√(mz_max)−a)/tof_max`.
#[derive(Debug, Clone, Copy)]
pub struct TofMzModel {
    pub a: f64,
    pub b: f64,
}

impl TofMzModel {
    /// Extract the exact coefficients from a timsrust converter through its public `convert`
    /// (the fields are private): `convert(0)=a²`, `convert(1)=(a+b)²`.
    fn from_converter(c: &Tof2MzConverter) -> Self {
        let a = c.convert(0u32).sqrt();
        let b = c.convert(1u32).sqrt() - a;
        Self { a, b }
    }

    /// Reconstruct m/z from a TOF bin: `m/z = (a + b·tof)²`. Monotonic in `tof` (a,b ≥ 0), so the
    /// min/max m/z of a spectrum come from the min/max TOF bin present.
    pub fn mz(&self, tof: i32) -> f64 {
        let v = self.a + self.b * tof as f64;
        v * v
    }
}

/// One native TIMS frame == one mzPeak spectrum. `scan_offsets[s]..scan_offsets[s+1]` indexes the
/// peaks belonging to mobility scan `s`.
pub struct RawFrame {
    pub index: usize,
    pub ms_level: u8,
    pub scan_offsets: Vec<usize>,
    pub tof: Vec<u32>,
    pub intensity: Vec<u32>,
}

/// Lean scan→1/K0 calibrator: timsrust's `Scan2ImConverter` built from `analysis.tdf` ALONE — no
/// frame / `analysis.tdf_bin` read. Lets the mobility calibration be dumped from just the metadata
/// DB (so CI can pull only the small `analysis.tdf` from a remote `.d.zip`, not the GB-scale binary).
pub struct MobilityCal {
    im: Scan2ImConverter,
}

impl MobilityCal {
    pub fn open(tdf: &Path) -> Result<Self> {
        let meta = MetadataReader::new(tdf)
            .map_err(|e| anyhow::anyhow!("reading TDF metadata {}: {e}", tdf.display()))?;
        Ok(Self { im: meta.im_converter })
    }

    #[inline]
    pub fn for_scan(&self, scan: usize) -> f64 {
        self.im.convert(scan as u32)
    }
}

/// Native integer-TOF reader over a Bruker `.d` (TDF). The mzdata-integration seam: a future
/// upstream native-TOF API would back this same surface.
pub struct NativeTofReader {
    frames: FrameReader,
    im: Scan2ImConverter,
    /// Vendor-grade ModelType-2 scan→1/K0 recalibration; `None` = use timsrust's linear approx
    /// (when recalibration is disabled, or the calibration isn't ModelType 2).
    recal: Option<crate::tims_mobility::TimsMobilityCalibration>,
    pub model: TofMzModel,
    /// Per-frame `NumPeaks` (from `analysis.tdf`, ordered by Id to match timsrust's frame index).
    /// Newer timsTOF (acq software 5.1.x) emits empty frames (`NumPeaks=0`) stored as a header-only
    /// blob with no zstd payload; `timsrust` errors decoding the empty payload. We short-circuit
    /// those to an empty frame so a real decode error on a *non-empty* frame still surfaces, instead
    /// of mzdata's blanket `.ok().unwrap_or_default()` that masks genuine corruption too.
    num_peaks: Vec<u32>,
}

impl NativeTofReader {
    /// Open with vendor mobility recalibration ON (the default).
    pub fn open(dot_d: &Path) -> Result<Self> {
        Self::open_with(dot_d, true)
    }

    /// Open, choosing whether to recalibrate scan→1/K0 against the Bruker `TimsCalibration` model
    /// (ModelType 2) instead of timsrust's linear approximation.
    pub fn open_with(dot_d: &Path, recalibrate: bool) -> Result<Self> {
        let tdf = dot_d.join("analysis.tdf");
        if !tdf.exists() {
            bail!("{} is not a TDF .d (no analysis.tdf)", dot_d.display());
        }
        let meta = MetadataReader::new(&tdf)
            .map_err(|e| anyhow::anyhow!("reading TDF metadata: {e}"))?;
        let frames = FrameReader::new(dot_d)
            .map_err(|e| anyhow::anyhow!("opening TDF frames: {e}"))?;
        let model = TofMzModel::from_converter(&meta.mz_converter);
        // Best-effort: a missing/other-ModelType calibration just leaves us on the linear path.
        let recal = if recalibrate {
            crate::tims_mobility::TimsMobilityCalibration::from_tdf_path(&tdf).unwrap_or(None)
        } else {
            None
        };
        let mut num_peaks = read_frame_num_peaks(&tdf)?;
        // Guard the position-based indexing: if timsrust's frame count disagrees with the Frames
        // row count, drop the fast-path rather than risk nulling a real frame (a misread empty frame
        // just errors → mzdata fallback; silently dropping a populated one would lose data).
        if num_peaks.len() != frames.len() {
            log::warn!(
                "TDF NumPeaks rows ({}) != timsrust frames ({}); disabling empty-frame fast path",
                num_peaks.len(),
                frames.len()
            );
            num_peaks.clear();
        }
        Ok(Self { frames, im: meta.im_converter, recal, model, num_peaks })
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn frame(&self, i: usize) -> Result<RawFrame> {
        // Empty frame (NumPeaks=0): timsrust can't decode the header-only blob, so build an empty
        // frame directly rather than letting it error the whole run. scan_offsets=[0] => 0 scans.
        if self.num_peaks.get(i).copied() == Some(0) {
            return Ok(RawFrame {
                index: i,
                ms_level: 0,
                scan_offsets: vec![0],
                tof: Vec::new(),
                intensity: Vec::new(),
            });
        }
        let f = self
            .frames
            .get(i)
            .map_err(|e| anyhow::anyhow!("reading frame {i}: {e}"))?;
        let ms_level = match f.ms_level {
            MSLevel::MS1 => 1,
            MSLevel::MS2 => 2,
            _ => 0,
        };
        Ok(RawFrame {
            index: f.index,
            ms_level,
            scan_offsets: f.scan_offsets,
            tof: f.tof_indices,
            intensity: f.intensities,
        })
    }

    #[inline]
    pub fn mobility_for_scan(&self, scan: usize) -> f64 {
        match &self.recal {
            Some(c) => c.one_over_k0(scan as f64), // vendor ModelType-2 rational
            None => self.im.convert(scan as u32),  // timsrust linear
        }
    }

    /// Build the IN-ARCHIVE ims-compact spectrum for frame `i`: the signal arrays are
    /// `nonstandard("tof")` (Int32, replaces `m/z array`) + `IntensityArray` (f32) +
    /// `MeanInverseReducedIonMobilityArray` (f64). m/z is reconstructed by readers from the index
    /// `ims_calibration` (per the mzPeakViewer handoff). Peaks are mobility-major then TOF order.
    pub fn ims_compact_spectrum(
        &self,
        i: usize,
        int_intensity: bool,
        tof_delta: bool,
    ) -> Result<MultiLayerSpectrum> {
        let frame = self.frame(i)?;
        let n_scans = frame.scan_offsets.len().saturating_sub(1);
        // Native counts are integers (u32). `int_intensity` stores them as Int32 so the writer can
        // BYTE_STREAM_SPLIT the column (byte-plane layout, ~ -16% on the intensity column, lossless;
        // f32 is also lossy for counts > 2^24). Default keeps f32 for format stability.
        let (mut tof, mut intensity_f32, mut intensity_i32, mut mobility) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        // Absolute TOF extent for the observed-m/z range — tracked separately because `tof_delta`
        // rewrites the `tof` column to per-scan deltas (so `tof.iter().min()` would be a delta, not a bin).
        let (mut tof_min, mut tof_max) = (i32::MAX, i32::MIN);
        for s in 0..n_scans {
            let (lo, hi) = (frame.scan_offsets[s], frame.scan_offsets[s + 1]);
            if lo >= hi {
                continue;
            }
            let m = self.mobility_for_scan(s);
            let mut prev = 0i32;
            for k in lo..hi {
                // TOF bins fit i32 in practice (digitizer ~4e5), but the column type is Int32 — guard
                // the cast so an out-of-range bin is a hard error, never a silent wrap to garbage m/z.
                let bin = i32::try_from(frame.tof[k])
                    .map_err(|_| anyhow::anyhow!("TOF bin {} exceeds i32 range", frame.tof[k]))?;
                tof_min = tof_min.min(bin);
                tof_max = tof_max.max(bin);
                // per-scan delta (the default; --no-tof-delta disables it): first peak of a scan =
                // absolute bin, the rest = non-negative increments; TOF is ascending within a scan. A
                // reader cumsum's
                // within each mobility scan to recover the absolute bin before the m/z model. Smaller
                // magnitudes ⇒ far more byte-plane redundancy ⇒ ~-20% on the TOF column.
                let stored = if tof_delta {
                    let d = if k == lo { bin } else { bin - prev };
                    prev = bin;
                    d
                } else {
                    bin
                };
                tof.push(stored);
                if int_intensity {
                    intensity_i32.push(i32::try_from(frame.intensity[k]).map_err(|_| {
                        anyhow::anyhow!("intensity {} exceeds i32 range", frame.intensity[k])
                    })?);
                } else {
                    intensity_f32.push(frame.intensity[k] as f32);
                }
                mobility.push(m);
            }
        }

        let mut arrays = BinaryArrayMap::new();
        let mut tof_da = DataArray::wrap(&ArrayType::nonstandard("tof"), BinaryDataArrayType::Int32, Vec::new());
        tof_da.update_buffer(tof.as_slice()).map_err(|e| anyhow::anyhow!("encoding tof: {e}"))?;
        arrays.add(tof_da);
        let mut int_da = if int_intensity {
            let mut da = DataArray::wrap(&ArrayType::IntensityArray, BinaryDataArrayType::Int32, Vec::new());
            da.update_buffer(intensity_i32.as_slice()).map_err(|e| anyhow::anyhow!("encoding intensity: {e}"))?;
            da
        } else {
            let mut da = DataArray::wrap(&ArrayType::IntensityArray, BinaryDataArrayType::Float32, Vec::new());
            da.update_buffer(intensity_f32.as_slice()).map_err(|e| anyhow::anyhow!("encoding intensity: {e}"))?;
            da
        };
        int_da.unit = Unit::DetectorCounts;
        arrays.add(int_da);
        let mut mob_da = DataArray::wrap(
            &ArrayType::MeanInverseReducedIonMobilityArray,
            BinaryDataArrayType::Float64,
            Vec::new(),
        );
        mob_da.update_buffer(mobility.as_slice()).map_err(|e| anyhow::anyhow!("encoding mobility: {e}"))?;
        arrays.add(mob_da);

        let mut descr = SpectrumDescription {
            id: format!("frame={}", frame.index),
            index: i,
            ms_level: frame.ms_level.max(1),
            signal_continuity: SignalContinuity::Centroid,
            ..Default::default()
        };
        descr.add_param(Param::builder().name("mass spectrum").curie(curie!(MS:1000294)).build());
        if tof_delta {
            // Self-describing marker for the per-scan-delta TOF encoding: a reader must cumsum the
            // `tof` column within each mobility-scan run before applying the m/z model.
            descr.add_param(Param::builder().name("mzpeak:tof_delta_reset").value("scan").build());
        }
        // Observed-m/z range: the output stores integer `tof`, so reconstruct m/z via the model
        // (m/z = (a + b·tof)², monotonic in tof) over the min/max ABSOLUTE TOF bin present. Without
        // this the viewer reports "m/z 0–0".
        if tof_min <= tof_max {
            let (mz_a, mz_b) = (self.model.mz(tof_min), self.model.mz(tof_max));
            crate::set_observed_mz_range(&mut descr, mz_a.min(mz_b), mz_a.max(mz_b));
        }
        Ok(MultiLayerSpectrum::new(descr, Some(arrays), None, None))
    }

}

/// `NumPeaks` for every frame, ordered by `Id` so position `i` matches timsrust's frame index.
/// Lets [`NativeTofReader::frame`] recognize empty frames without reading the binary.
fn read_frame_num_peaks(tdf: &Path) -> Result<Vec<u32>> {
    let conn = rusqlite::Connection::open_with_flags(tdf, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| anyhow::anyhow!("opening {} for NumPeaks: {e}", tdf.display()))?;
    let mut stmt = conn
        .prepare("SELECT NumPeaks FROM Frames ORDER BY Id")
        .map_err(|e| anyhow::anyhow!("querying NumPeaks: {e}"))?;
    let rows = stmt
        .query_map([], |r| r.get::<_, i64>(0).map(|n| n.max(0) as u32))
        .map_err(|e| anyhow::anyhow!("reading NumPeaks: {e}"))?;
    rows.collect::<rusqlite::Result<Vec<u32>>>()
        .map_err(|e| anyhow::anyhow!("collecting NumPeaks: {e}"))
}

#[cfg(test)]
mod tof_delta_tests {
    /// Per-scan delta TOF (the default; `--no-tof-delta` disables it) stores the first peak of a scan
    /// as the absolute bin and the rest as increments (TOF ascends within a scan); a reader recovers
    /// absolute TOF by cumulative sum within each scan. That transform MUST be exactly invertible —
    /// this guards losslessness. Mirrors the encode step in [`super::NativeTofReader::ims_compact_spectrum`].
    #[test]
    fn per_scan_delta_is_lossless() {
        let scans: Vec<Vec<i32>> = vec![
            vec![10, 12, 500, 501, 402_000],
            vec![3, 400_000],
            vec![7],
            vec![],
        ];
        for scan in &scans {
            let mut enc = Vec::new();
            let mut prev = 0i32;
            for (k, &bin) in scan.iter().enumerate() {
                enc.push(if k == 0 { bin } else { bin - prev });
                prev = bin;
            }
            let mut dec = Vec::with_capacity(enc.len());
            let mut acc = 0i32;
            for (k, &d) in enc.iter().enumerate() {
                acc = if k == 0 { d } else { acc + d };
                dec.push(acc);
            }
            assert_eq!(&dec, scan, "per-scan delta round-trip must reconstruct absolute TOF");
        }
    }
}
