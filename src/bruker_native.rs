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
//! (frame), within a frame mobility-major (scan ascending) then TOF ascending. The TOF column holds
//! ABSOLUTE bins — the point layout requires values be stored as-is so the Parquet page index stays
//! meaningful. Lossless: `m/z = (a + b·tof)²` with `a,b` in the array index's transform parameters,
//! so a reader recovers the exact integer TOF and thus the exact vendor m/z.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Result, bail};

use mzdata::params::{Param, Unit};
use mzdata::prelude::ParamDescribed;
use mzdata::spectrum::bindata::{ArrayType, BinaryArrayMap, BinaryDataArrayType, DataArray};
use mzdata::curie;
use mzdata::meta::DissociationMethodTerm;
use mzdata::spectrum::{
    Activation, IsolationWindow, IsolationWindowState, MultiLayerSpectrum,
    Precursor, ScanPolarity, SelectedIon, SignalContinuity, SpectrumDescription,
};

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
    /// Per-frame `Frames` columns from `analysis.tdf`. Empty if unavailable or if the row count
    /// disagrees with timsrust's frame count (see `open_with`).
    table: FrameTable,
    /// MS2 isolation windows keyed by 1-based TDF frame Id. Empty for MS1-only runs.
    windows: HashMap<i64, Vec<FrameWindow>>,
}

/// Per-frame `Frames` columns, ordered by `Id` so position `i` matches timsrust's frame index.
///
/// One query for all four, because they share the same position↔Id assumption and so must stand or
/// fall together:
/// * `NumPeaks` — newer timsTOF (acq software 5.1.x) emits empty frames (`NumPeaks=0`) stored as a
///   header-only blob with no zstd payload, which `timsrust` errors on. Recognising them here lets a
///   real decode error on a *non-empty* frame still surface, instead of mzdata's blanket
///   `.ok().unwrap_or_default()` that masks genuine corruption too.
/// * `Time` — retention time in SECONDS.
/// * `MsMsType` — the MS level. This is the ONLY source for empty frames: timsrust cannot decode
///   them, so without it a dia-PASEF MS2 frame with no peaks gets silently written as MS1.
/// * `Polarity` — `+`/`-`; timsrust does not expose it.
#[derive(Default)]
struct FrameTable {
    num_peaks: Vec<u32>,
    rt: Vec<f64>,
    ms_level: Vec<u8>,
    polarity: Vec<ScanPolarity>,
}

/// One quadrupole isolation window within an MS2 frame.
///
/// A TDF MS2 frame is a whole TIMS ramp (900–1600 scans), and the quadrupole retunes *during* the
/// ramp: each `[scan_begin, scan_end)` sub-range gets its own isolation window and collision energy.
/// So one frame carries N windows over disjoint mobility ranges — ~1.6 on average for DDA-PASEF,
/// 5.0 for dia-PASEF. mzdata splits these into N mzML spectra because mzML has nowhere to put the
/// mobility dimension; mzPeak does, so we keep the frame whole and attach N precursors to it.
struct FrameWindow {
    scan_begin: u32,
    scan_end: u32,
    isolation_mz: f64,
    isolation_width: f64,
    collision_energy: f64,
    /// DDA-PASEF only — dia-PASEF has no `Precursors` table, so the window centre is all there is.
    mono_mz: Option<f64>,
    average_mz: Option<f64>,
    charge: Option<i32>,
    intensity: Option<f64>,
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
        let mut table = read_frame_table(&tdf)?;
        // Guard the position-based indexing: if timsrust's frame count disagrees with the Frames
        // row count, drop the whole table rather than risk misattributing a row (a misread empty
        // frame just errors → mzdata fallback; a misaligned RT/MS level silently corrupts).
        if table.num_peaks.len() != frames.len() {
            log::warn!(
                "TDF Frames rows ({}) != timsrust frames ({}); disabling empty-frame fast path, \
                 retention time, MS level and polarity",
                table.num_peaks.len(),
                frames.len()
            );
            table = FrameTable::default();
        }
        let windows = read_frame_windows(&tdf).unwrap_or_else(|e| {
            log::warn!("TDF MS2 isolation windows unavailable ({e}); precursors will be absent");
            HashMap::new()
        });
        Ok(Self { frames, im: meta.im_converter, recal, model, table, windows })
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn frame(&self, i: usize) -> Result<RawFrame> {
        // Empty frame (NumPeaks=0): timsrust can't decode the header-only blob, so build an empty
        // frame directly rather than letting it error the whole run. scan_offsets=[0] => 0 scans.
        if self.table.num_peaks.get(i).copied() == Some(0) {
            return Ok(RawFrame {
                // timsrust reports the 1-based TDF frame Id in `index` (position 0 => Id 1), and
                // `index` only ever becomes the `frame=N` spectrum id. Using the 0-based position
                // here handed every empty frame its predecessor's id — duplicate ids collapse the
                // reader's id_index, which then sizes its per-spectrum vecs short and panics.
                index: i + 1,
                ms_level: self.ms_level_at(i),
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
            // timsrust only models MS1/MS2; anything else (MRM, dia-PASEF variants) falls back to
            // the TDF's own MsMsType rather than being fabricated as MS1.
            _ => self.ms_level_at(i),
        };
        Ok(RawFrame {
            index: f.index,
            ms_level,
            scan_offsets: f.scan_offsets,
            tof: f.tof_indices,
            intensity: f.intensities,
        })
    }

    /// Build the mzdata precursors for frame `i` (0-based position; TDF Id is `i + 1`).
    ///
    /// Follows mzdata's TDF conventions so the two agree — isolation bounds are the window centre
    /// +/- half the FULL width, activation is CID at the window's collision energy — with two
    /// deliberate improvements: a NULL `MonoisotopicMz` falls back to `AverageMz`/`IsolationMz`
    /// instead of mzdata's `0.0`, and the mobility of the window is recorded from its own scan
    /// range rather than the precursor's parent-frame scan number.
    fn precursors_at(&self, i: usize) -> Vec<Precursor> {
        let Some(windows) = self.windows.get(&((i + 1) as i64)) else {
            return Vec::new();
        };
        windows
            .iter()
            .map(|w| {
                let half = (w.isolation_width / 2.0) as f32;
                let mut ion = SelectedIon {
                    mz: w.mono_mz.or(w.average_mz).unwrap_or(w.isolation_mz),
                    intensity: w.intensity.unwrap_or(0.0) as f32,
                    charge: w.charge.filter(|c| *c != 0),
                    ..Default::default()
                };
                // Mobility of the isolation window: the midpoint of the scan range it occupies.
                let mid = (w.scan_begin as f64 + w.scan_end as f64) / 2.0;
                ion.add_param(
                    Param::builder()
                        .name("inverse reduced ion mobility")
                        .curie(curie!(MS:1002815))
                        .value(self.mobility_for_scan(mid as usize))
                        .unit(Unit::VoltSecondPerSquareCentimeter)
                        .build(),
                );
                // The midpoint ALONE is not enough to reconstruct the split: a frame's windows
                // cover generally ASYMMETRIC scan ranges (e.g. [34,602) and [602,944)), so a
                // reader splitting at the midpoint between adjacent centres misplaces the
                // boundary — measured 3.5% of the mobility axis on average, up to 10.3%, on a
                // real dia-PASEF run. Emit the true bounds so readers never have to guess.
                let (mob_a, mob_b) = (
                    self.mobility_for_scan(w.scan_begin as usize),
                    self.mobility_for_scan(w.scan_end as usize),
                );
                // 1/K0 DECREASES as the scan index increases, so order the bounds explicitly
                // rather than assuming begin<end maps to lower<upper.
                let (mob_lo, mob_hi) = if mob_a <= mob_b { (mob_a, mob_b) } else { (mob_b, mob_a) };
                // Emitted WITHOUT a CURIE: mzdata panics ("Cannot encode unknown CV") when a
                // non-standard accession reaches its param encoder, and PSI-MS has no term for
                // an isolation window's mobility bounds. Readers match on the name, as they
                // already do for the nonstandard "tof" / mobility array names.
                for (name, value) in [
                    ("isolation window inverse reduced ion mobility lower limit", mob_lo),
                    ("isolation window inverse reduced ion mobility upper limit", mob_hi),
                ] {
                    ion.add_param(
                        Param::builder()
                            .name(name)
                            .value(value)
                            .unit(Unit::VoltSecondPerSquareCentimeter)
                            .build(),
                    );
                }
                let mut activation = Activation::default();
                activation.energy = w.collision_energy as f32;
                activation
                    .methods_mut()
                    .push(DissociationMethodTerm::CollisionInducedDissociation);
                Precursor {
                    ions: vec![ion],
                    isolation_window: IsolationWindow {
                        target: w.isolation_mz as f32,
                        lower_bound: w.isolation_mz as f32 - half,
                        upper_bound: w.isolation_mz as f32 + half,
                        flags: IsolationWindowState::Complete,
                    },
                    activation,
                    ..Default::default()
                }
            })
            .collect()
    }

    /// MS level for frame `i` from the TDF `MsMsType`, defaulting to MS1 when the table is
    /// unavailable. Never returns 0 — `ms_level` 0 is not a legal MS stage under `MS:1000511`.
    #[inline]
    fn ms_level_at(&self, i: usize) -> u8 {
        self.table.ms_level.get(i).copied().unwrap_or(1)
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
    ) -> Result<MultiLayerSpectrum> {
        let frame = self.frame(i)?;
        let n_scans = frame.scan_offsets.len().saturating_sub(1);
        // Native counts are integers (u32). `int_intensity` stores them as Int32 so the writer can
        // BYTE_STREAM_SPLIT the column (byte-plane layout, ~ -16% on the intensity column, lossless;
        // f32 is also lossy for counts > 2^24). Default keeps f32 for format stability.
        let (mut tof, mut intensity_f32, mut intensity_i32, mut mobility) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        // Absolute TOF extent for the observed-m/z range, tracked as we go so the summary reflects
        // real bins regardless of how the writer later encodes the column.
        let (mut tof_min, mut tof_max) = (i32::MAX, i32::MIN);
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
                tof_min = tof_min.min(bin);
                tof_max = tof_max.max(bin);
                tof.push(bin);
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
            ms_level: frame.ms_level,
            signal_continuity: SignalContinuity::Centroid,
            ..Default::default()
        };
        // Retention time: TDF `Frames.Time` is seconds; mzPeak scan start time / `spectrum.time` are
        // minutes (matching the mzML/Thermo path), so store rt/60. Enables `--rt` on timsTOF.
        if let Some(&rt) = self.table.rt.get(i) {
            descr.acquisition.first_scan_mut().unwrap().start_time = rt / 60.0;
        }
        // Polarity: timsrust does not surface it, so it comes from TDF `Frames.Polarity`.
        descr.polarity = self.table.polarity.get(i).copied().unwrap_or_default();
        descr.precursor = self.precursors_at(i);
        // Observed-m/z range: the output stores integer `tof`, so reconstruct m/z via the model
        // (m/z = (a + b·tof)², monotonic in tof) over the min/max ABSOLUTE TOF bin present. Without
        // this the viewer reports "m/z 0–0".
        if tof_min <= tof_max {
            let (mz_a, mz_b) = (self.model.mz(tof_min), self.model.mz(tof_max));
            crate::set_observed_mz_range(&mut descr, mz_a.min(mz_b), mz_a.max(mz_b));
        }
        Ok(MultiLayerSpectrum::new(descr, Some(arrays), None, None))
    }

    /// GATED `--ims-chunked` variant of [`Self::ims_compact_spectrum`]: emits ABSOLUTE integer `tof`
    /// (no per-scan delta) with the WHOLE FRAME sorted by `tof` (== sorted by m/z, since m/z is
    /// monotonic in tof). The writer then splits these points into true-m/z-bin chunks and
    /// delta-encodes `tof` within each chunk. Sorting mixes mobility scans, which is lossless because
    /// mobility is stored explicitly per point. Same three arrays (`tof`, intensity, mobility) as the
    /// default path; the delta encoding lives entirely in the writer, per chunk.
    pub fn ims_compact_spectrum_chunked(
        &self,
        i: usize,
        int_intensity: bool,
    ) -> Result<MultiLayerSpectrum> {
        let frame = self.frame(i)?;
        let n_scans = frame.scan_offsets.len().saturating_sub(1);
        // Gather every point as (tof_bin, intensity, mobility) across all mobility scans.
        // Sort by TOF (== by m/z): this puts m/z-adjacent points together so the per-chunk delta
        // makes tof deltas near-zero (tof is the largest column, so this dominates). A secondary
        // sort by mobility was tried to shrink the scattered 1/K0 column, but it scrambles tof
        // within each chunk and inflates it more than it saves on mobility — a net loss (measured
        // g99123: mobility −392 MB, tof +577 MB). So m/z order stays.
        let mut pts: Vec<(i32, u32, f64)> = Vec::with_capacity(frame.tof.len());
        let (mut tof_min, mut tof_max) = (i32::MAX, i32::MIN);
        for s in 0..n_scans {
            let (lo, hi) = (frame.scan_offsets[s], frame.scan_offsets[s + 1]);
            if lo >= hi {
                continue;
            }
            let m = self.mobility_for_scan(s);
            for k in lo..hi {
                let bin = i32::try_from(frame.tof[k])
                    .map_err(|_| anyhow::anyhow!("TOF bin {} exceeds i32 range", frame.tof[k]))?;
                tof_min = tof_min.min(bin);
                tof_max = tof_max.max(bin);
                pts.push((bin, frame.intensity[k], m));
            }
        }
        pts.sort_by_key(|p| p.0);

        let (mut tof, mut intensity_i32, mut intensity_f32, mut mobility) = (
            Vec::with_capacity(pts.len()),
            Vec::with_capacity(pts.len()),
            Vec::with_capacity(pts.len()),
            Vec::with_capacity(pts.len()),
        );
        for (bin, inten, m) in pts {
            tof.push(bin);
            if int_intensity {
                intensity_i32.push(
                    i32::try_from(inten)
                        .map_err(|_| anyhow::anyhow!("intensity {} exceeds i32 range", inten))?,
                );
            } else {
                intensity_f32.push(inten as f32);
            }
            mobility.push(m);
        }

        let mut arrays = BinaryArrayMap::new();
        let mut tof_da =
            DataArray::wrap(&ArrayType::nonstandard("tof"), BinaryDataArrayType::Int32, Vec::new());
        tof_da.update_buffer(tof.as_slice()).map_err(|e| anyhow::anyhow!("encoding tof: {e}"))?;
        arrays.add(tof_da);
        let mut int_da = if int_intensity {
            let mut da =
                DataArray::wrap(&ArrayType::IntensityArray, BinaryDataArrayType::Int32, Vec::new());
            da.update_buffer(intensity_i32.as_slice())
                .map_err(|e| anyhow::anyhow!("encoding intensity: {e}"))?;
            da
        } else {
            let mut da = DataArray::wrap(
                &ArrayType::IntensityArray,
                BinaryDataArrayType::Float32,
                Vec::new(),
            );
            da.update_buffer(intensity_f32.as_slice())
                .map_err(|e| anyhow::anyhow!("encoding intensity: {e}"))?;
            da
        };
        int_da.unit = Unit::DetectorCounts;
        arrays.add(int_da);
        let mut mob_da = DataArray::wrap(
            &ArrayType::MeanInverseReducedIonMobilityArray,
            BinaryDataArrayType::Float64,
            Vec::new(),
        );
        mob_da.update_buffer(mobility.as_slice())
            .map_err(|e| anyhow::anyhow!("encoding mobility: {e}"))?;
        arrays.add(mob_da);

        let mut descr = SpectrumDescription {
            id: format!("frame={}", frame.index),
            index: i,
            ms_level: frame.ms_level,
            signal_continuity: SignalContinuity::Centroid,
            ..Default::default()
        };
        // Retention time: TDF `Frames.Time` is seconds; mzPeak scan start time / `spectrum.time` are
        // minutes (matching the mzML/Thermo path), so store rt/60. Enables `--rt` on timsTOF.
        if let Some(&rt) = self.table.rt.get(i) {
            descr.acquisition.first_scan_mut().unwrap().start_time = rt / 60.0;
        }
        // Polarity: timsrust does not surface it, so it comes from TDF `Frames.Polarity`.
        descr.polarity = self.table.polarity.get(i).copied().unwrap_or_default();
        descr.precursor = self.precursors_at(i);
        if tof_min <= tof_max {
            let (mz_a, mz_b) = (self.model.mz(tof_min), self.model.mz(tof_max));
            crate::set_observed_mz_range(&mut descr, mz_a.min(mz_b), mz_a.max(mz_b));
        }
        Ok(MultiLayerSpectrum::new(descr, Some(arrays), None, None))
    }
}

/// Read the MS2 isolation windows from `analysis.tdf`, keyed by 1-based frame Id.
///
/// The two acquisition modes store this completely differently, and a file has only one of them —
/// dia-PASEF `.d` files have no `PasefFrameMsMsInfo`/`Precursors` tables AT ALL, so this probes
/// `sqlite_master` rather than assuming. PRM (`PrmFrameMsMsInfo`) is not handled yet; such a run
/// simply gets no precursors rather than a wrong one.
fn read_frame_windows(tdf: &Path) -> Result<HashMap<i64, Vec<FrameWindow>>> {
    let conn = rusqlite::Connection::open_with_flags(tdf, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| anyhow::anyhow!("opening {} for MS2 info: {e}", tdf.display()))?;
    let has = |name: &str| -> bool {
        conn.query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
            [name],
            |_| Ok(()),
        )
        .is_ok()
    };

    let mut out: HashMap<i64, Vec<FrameWindow>> = HashMap::new();
    if has("PasefFrameMsMsInfo") && has("Precursors") {
        // DDA-PASEF. LEFT JOIN so a window with a dangling/NULL precursor still yields its
        // isolation window rather than vanishing.
        let mut stmt = conn
            .prepare(
                "SELECT p.Frame, p.ScanNumBegin, p.ScanNumEnd, p.IsolationMz, p.IsolationWidth, \
                        p.CollisionEnergy, pr.MonoisotopicMz, pr.AverageMz, pr.Charge, pr.Intensity \
                 FROM PasefFrameMsMsInfo p LEFT JOIN Precursors pr ON pr.Id = p.Precursor \
                 ORDER BY p.Frame, p.ScanNumBegin",
            )
            .map_err(|e| anyhow::anyhow!("querying PasefFrameMsMsInfo: {e}"))?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    FrameWindow {
                        scan_begin: r.get::<_, i64>(1)?.max(0) as u32,
                        scan_end: r.get::<_, i64>(2)?.max(0) as u32,
                        isolation_mz: r.get(3)?,
                        isolation_width: r.get(4)?,
                        collision_energy: r.get(5)?,
                        mono_mz: r.get(6)?,
                        average_mz: r.get(7)?,
                        charge: r.get(8)?,
                        intensity: r.get(9)?,
                    },
                ))
            })
            .map_err(|e| anyhow::anyhow!("reading PasefFrameMsMsInfo: {e}"))?;
        for row in rows {
            let (frame, w) = row.map_err(|e| anyhow::anyhow!("collecting PasefFrameMsMsInfo: {e}"))?;
            out.entry(frame).or_default().push(w);
        }
    } else if has("DiaFrameMsMsInfo") && has("DiaFrameMsMsWindows") {
        // dia-PASEF: the frame maps to a window GROUP, and the group expands to its windows. There
        // is no per-precursor detail — the window centre is the only m/z available.
        let mut stmt = conn
            .prepare(
                "SELECT d.Frame, w.ScanNumBegin, w.ScanNumEnd, w.IsolationMz, w.IsolationWidth, \
                        w.CollisionEnergy \
                 FROM DiaFrameMsMsInfo d JOIN DiaFrameMsMsWindows w ON w.WindowGroup = d.WindowGroup \
                 ORDER BY d.Frame, w.ScanNumBegin",
            )
            .map_err(|e| anyhow::anyhow!("querying DiaFrameMsMsWindows: {e}"))?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    FrameWindow {
                        scan_begin: r.get::<_, i64>(1)?.max(0) as u32,
                        scan_end: r.get::<_, i64>(2)?.max(0) as u32,
                        isolation_mz: r.get(3)?,
                        isolation_width: r.get(4)?,
                        collision_energy: r.get(5)?,
                        mono_mz: None,
                        average_mz: None,
                        charge: None,
                        intensity: None,
                    },
                ))
            })
            .map_err(|e| anyhow::anyhow!("reading DiaFrameMsMsWindows: {e}"))?;
        for row in rows {
            let (frame, w) = row.map_err(|e| anyhow::anyhow!("collecting DiaFrameMsMsWindows: {e}"))?;
            out.entry(frame).or_default().push(w);
        }
    }
    log::debug!("TDF MS2 windows: {} frames carry isolation windows", out.len());
    Ok(out)
}

/// Read the per-frame [`FrameTable`] from `analysis.tdf`, ordered by `Id` so position `i` matches
/// timsrust's frame index.
fn read_frame_table(tdf: &Path) -> Result<FrameTable> {
    let conn = rusqlite::Connection::open_with_flags(tdf, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| anyhow::anyhow!("opening {} for Frames: {e}", tdf.display()))?;
    let mut stmt = conn
        .prepare("SELECT NumPeaks, Time, MsMsType, Polarity FROM Frames ORDER BY Id")
        .map_err(|e| anyhow::anyhow!("querying Frames: {e}"))?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?.max(0) as u32,
                r.get::<_, f64>(1)?,
                // TDF MsMsType: 0 is full-scan MS1; every other value (2 MRM, 8 PASEF, 9 dia-PASEF)
                // is a fragmentation frame, i.e. MS2.
                if r.get::<_, i64>(2)? == 0 { 1u8 } else { 2u8 },
                match r.get::<_, String>(3)?.trim() {
                    "+" => ScanPolarity::Positive,
                    "-" => ScanPolarity::Negative,
                    _ => ScanPolarity::Unknown,
                },
            ))
        })
        .map_err(|e| anyhow::anyhow!("reading Frames: {e}"))?;
    let mut t = FrameTable::default();
    for row in rows {
        let (n, rt, lvl, pol) = row.map_err(|e| anyhow::anyhow!("collecting Frames: {e}"))?;
        t.num_peaks.push(n);
        t.rt.push(rt);
        t.ms_level.push(lvl);
        t.polarity.push(pol);
    }
    Ok(t)
}
