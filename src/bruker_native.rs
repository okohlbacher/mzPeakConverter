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
        Ok(Self { frames, im: meta.im_converter, recal, model })
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn frame(&self, i: usize) -> Result<RawFrame> {
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

    /// The `ims_calibration` JSON for the archive index. Archive variant stores ABSOLUTE tof (no
    /// delta), so readers reconstruct `m/z = (a + b·tof)²` directly.
    pub fn calibration_json(&self) -> serde_json::Value {
        serde_json::json!({
            "codec": "ims-compact",
            "lossless": "tof",
            "mz_from_tof": "(a + b*tof)^2",
            "tof_encoding": "absolute",
            "a": self.model.a,
            "b": self.model.b,
        })
    }

    /// Build the IN-ARCHIVE ims-compact spectrum for frame `i`: the signal arrays are
    /// `nonstandard("tof")` (Int32, replaces `m/z array`) + `IntensityArray` (f32) +
    /// `MeanInverseReducedIonMobilityArray` (f64). m/z is reconstructed by readers from the index
    /// `ims_calibration` (per the mzPeakViewer handoff). Peaks are mobility-major then TOF order.
    pub fn ims_compact_spectrum(&self, i: usize) -> Result<MultiLayerSpectrum> {
        let frame = self.frame(i)?;
        let n_scans = frame.scan_offsets.len().saturating_sub(1);
        let (mut tof, mut intensity, mut mobility) = (Vec::new(), Vec::new(), Vec::new());
        for s in 0..n_scans {
            let (lo, hi) = (frame.scan_offsets[s], frame.scan_offsets[s + 1]);
            if lo >= hi {
                continue;
            }
            let m = self.mobility_for_scan(s);
            for k in lo..hi {
                // TOF bins fit i32 in practice (digitizer ~4e5), but the column type is Int32 — guard
                // the cast so an out-of-range bin is a hard error, never a silent wrap to garbage m/z.
                let bin = i32::try_from(frame.tof[k])
                    .map_err(|_| anyhow::anyhow!("TOF bin {} exceeds i32 range", frame.tof[k]))?;
                tof.push(bin);
                intensity.push(frame.intensity[k] as f32);
                mobility.push(m);
            }
        }

        let mut arrays = BinaryArrayMap::new();
        let mut tof_da = DataArray::wrap(&ArrayType::nonstandard("tof"), BinaryDataArrayType::Int32, Vec::new());
        tof_da.update_buffer(tof.as_slice()).map_err(|e| anyhow::anyhow!("encoding tof: {e}"))?;
        arrays.add(tof_da);
        let mut int_da = DataArray::wrap(&ArrayType::IntensityArray, BinaryDataArrayType::Float32, Vec::new());
        int_da.update_buffer(intensity.as_slice()).map_err(|e| anyhow::anyhow!("encoding intensity: {e}"))?;
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
        Ok(MultiLayerSpectrum::new(descr, Some(arrays), None, None))
    }

}
