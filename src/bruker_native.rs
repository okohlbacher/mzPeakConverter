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

use anyhow::{Context, Result, bail};
use mzdata::params::{CURIE, ControlledVocabulary};
use rusqlite::{OptionalExtension, types::ValueRef};

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

/// Re-expresses the 1/K0 values mzdata's TDF reader puts in spectrum METADATA on the vendor model.
///
/// mzdata 0.66.6 converts its signal arrays through the frame's ModelType-2 `TimsCalibration`
/// (`io/tdf/arrays.rs:73`), but every 1/K0 it attaches as a *param* — the selected ion's
/// `inverse reduced ion mobility` (MS:1002815, `io/tdf/reader.rs:1487,1527`), the scan-level
/// MS:1002815 midpoint (`:1607`) and the frame's `ion mobility lower/upper limit` (`:1615-1641`) —
/// goes through timsrust's LINEAR nominal-range interpolation (`metadata.im_converter`). On
/// PXD059079 2485.d that put the `--no-ims-compact` selected ion at 1.317349 against the ims-compact
/// lane's 1.332429 for the same window (0.015 Vs/cm², up to ~0.03 at the high-mobility edge). The
/// linear map is exactly invertible, so this recovers the scan position and re-evaluates the same
/// ModelType-2 model the native lane uses ([`crate::tims_mobility`]); with no ModelType-2 row the
/// values are left as they are (the native lane falls back to the same linear map then).
///
/// It also attaches the window's 1/K0 band to each selected ion as MZP:1000006/7, from mzdata's
/// spectrum-level `ion mobility lower/upper limit` (that spelling is kept), so both lanes spell the
/// selected ion identically. mzdata emits that pair INVERTED — `lower` = convert(ScanNumBegin),
/// `upper` = convert(ScanNumEnd), and 1/K0 decreases with the scan index — so the pair is also put
/// in order here (`lower <= upper`), on the same values the band gets.
pub struct TdfMobilityRemap {
    linear: Scan2ImConverter,
    recal: Option<crate::tims_mobility::TimsMobilityCalibration>,
}

impl TdfMobilityRemap {
    /// Open, choosing whether to re-express the params on the vendor ModelType-2 model
    /// (`recalibrate`, the `--no-tims-recalibration` knob — the same choice as
    /// [`NativeTofReader::open_with`], so both lanes stay on the same model either way) or leave
    /// them on timsrust's linear map. The band is attached in both cases. A missing/unreadable
    /// `TimsCalibration` table is best-effort here as in the native lane: a warning and the linear
    /// values, never a lane without the band.
    pub fn open_with(dot_d: &Path, recalibrate: bool) -> Result<Self> {
        let tdf = dot_d.join("analysis.tdf");
        let linear = MetadataReader::new(&tdf)
            .map_err(|e| anyhow::anyhow!("reading TDF metadata {}: {e}", tdf.display()))?
            .im_converter;
        let recal = if recalibrate {
            crate::tims_mobility::TimsMobilityCalibration::from_tdf_path(&tdf).unwrap_or_else(|e| {
                log::warn!(
                    "TDF TimsCalibration unreadable ({e:#}); mobility params stay on timsrust's \
                     linear approximation"
                );
                None
            })
        } else {
            None
        };
        Ok(Self { linear, recal })
    }

    /// For tests: a remap over explicit models.
    #[cfg(test)]
    fn new(linear: Scan2ImConverter, recal: Option<crate::tims_mobility::TimsMobilityCalibration>) -> Self {
        Self { linear, recal }
    }

    /// A 1/K0 produced by timsrust's linear converter → the same scan position on the vendor model.
    #[inline]
    pub fn remap(&self, im: f64) -> f64 {
        match &self.recal {
            Some(c) => c.one_over_k0(self.linear.invert(im)),
            None => im,
        }
    }

    fn remap_param(&self, p: &mut Param) {
        if let Ok(v) = p.value.to_f64() {
            p.value = mzdata::params::Value::Float(self.remap(v));
        }
    }

    /// Rewrite one mzdata-produced TDF spectrum description in place (see the type docs).
    pub fn apply(&self, descr: &mut SpectrumDescription) {
        let im_term = curie!(MS:1002815);
        let (mut lo, mut hi) = (None, None);
        let (mut lo_at, mut hi_at) = (None, None);
        for (i, p) in descr.params.iter_mut().enumerate() {
            match p.name.as_str() {
                "ion mobility lower limit" => {
                    self.remap_param(p);
                    lo = p.value.to_f64().ok();
                    lo_at = Some(i);
                }
                "ion mobility upper limit" => {
                    self.remap_param(p);
                    hi = p.value.to_f64().ok();
                    hi_at = Some(i);
                }
                _ => {}
            }
        }
        // mzdata writes the pair as (convert(ScanNumBegin), convert(ScanNumEnd)), which is
        // (larger, smaller) because 1/K0 falls with the scan index — put it in order, so the
        // spectrum-level pair and the selected-ion band never contradict each other.
        if let (Some(a), Some(b), Some(i), Some(j)) = (lo, hi, lo_at, hi_at) {
            if a > b {
                descr.params[i].value = mzdata::params::Value::Float(b);
                descr.params[j].value = mzdata::params::Value::Float(a);
                (lo, hi) = (Some(b), Some(a));
            }
        }
        for scan in descr.acquisition.scans.iter_mut() {
            if let Some(ps) = scan.params.as_mut() {
                for p in ps.iter_mut().filter(|p| p.curie() == Some(im_term)) {
                    self.remap_param(p);
                }
            }
        }
        for prec in descr.precursor.iter_mut() {
            for ion in prec.ions.iter_mut() {
                if let Some(ps) = ion.params.as_mut() {
                    for p in ps.iter_mut().filter(|p| p.curie() == Some(im_term)) {
                        self.remap_param(p);
                    }
                }
                let has_band = ion
                    .params
                    .as_ref()
                    .is_some_and(|ps| ps.iter().any(|p| p.curie() == Some(MZP_IM_WINDOW_LOWER)));
                if let (Some(a), Some(b), false) = (lo, hi, has_band) {
                    add_isolation_mobility_band(ion, a, b);
                }
            }
        }
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
    /// `T1` / `T2` / `MzCalibration` — the per-frame inputs of the vendor's exact ModelType-1
    /// TOF→m/z model (see [`vendor_mz_calibration`]). Empty when the schema lacks the columns;
    /// `None` for a frame whose own value is NULL, which must not abort the conversion (nor shift
    /// the per-frame indices, so the entry is kept and the columns come out null for that frame).
    t1: Vec<Option<f64>>,
    t2: Vec<Option<f64>>,
    mz_cal_id: Vec<Option<i64>>,
}

/// Local CURIEs for the per-frame calibration inputs (`Frames.T1`, `Frames.T2`,
/// `Frames.MzCalibration`), which the ims-compact writer promotes to `spectra_metadata` columns
/// (`…_tdf_t1`, `…_tdf_t2`, `…_tdf_mz_calibration_id`). MS:4000903–4000905 are unused local
/// accessions, following the `tof_c0`/`tof_c1` precedent in `main.rs`.
pub(crate) const TDF_T1_CURIE: CURIE = CURIE::new(ControlledVocabulary::MS, 4_000_903);
pub(crate) const TDF_T2_CURIE: CURIE = CURIE::new(ControlledVocabulary::MS, 4_000_904);
pub(crate) const TDF_MZ_CAL_ID_CURIE: CURIE = CURIE::new(ControlledVocabulary::MS, 4_000_905);

/// Converter-owned accessions for an isolation window's 1/K0 band (`cv/mzpeak.obo` MZP:1000006 /
/// MZP:1000007). PSI-MS has no term for the mobility bounds of an isolation window (children of
/// MS:1000792 / MS:1002892 checked at 4.1.259); the provisional MZP vocabulary is represented in
/// this crate as `ControlledVocabulary::Unknown` CURIEs, which the vendored writer/reader render and
/// parse as `MZP:` (see `mzpeak_prototyping::param::curie_to_string`). mzdata's own `Display` panics
/// on `Unknown`, so these must never reach an mzdata writer un-demoted (`demote_mzp_params` in
/// `main.rs` handles the mzML export).
pub(crate) const MZP_IM_WINDOW_LOWER: CURIE = CURIE::new(ControlledVocabulary::Unknown, 1_000_006);
pub(crate) const MZP_IM_WINDOW_UPPER: CURIE = CURIE::new(ControlledVocabulary::Unknown, 1_000_007);
pub(crate) const IM_WINDOW_LOWER_NAME: &str = "isolation window inverse reduced ion mobility lower limit";
pub(crate) const IM_WINDOW_UPPER_NAME: &str = "isolation window inverse reduced ion mobility upper limit";

/// Attach the isolation window's 1/K0 band `[lo, hi]` to a selected ion as MZP:1000006/7 params.
/// Shared by every timsTOF lane so the band is spelled identically whichever reader produced the
/// spectrum. `lo`/`hi` are ordered here: 1/K0 DECREASES as the scan index increases, so callers
/// converting scan bounds must not assume begin<end maps to lower<upper.
pub(crate) fn add_isolation_mobility_band(ion: &mut SelectedIon, a: f64, b: f64) {
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    for (name, curie, value) in [
        (IM_WINDOW_LOWER_NAME, MZP_IM_WINDOW_LOWER, lo),
        (IM_WINDOW_UPPER_NAME, MZP_IM_WINDOW_UPPER, hi),
    ] {
        ion.add_param(
            Param::builder()
                .name(name)
                .curie(curie)
                .value(value)
                .unit(Unit::VoltSecondPerSquareCentimeter)
                .build(),
        );
    }
}

/// Attach one frame's calibration inputs as spectrum params. Shared by the native and `--bruker-sdk`
/// ims-compact lanes so both write identical columns.
pub(crate) fn add_frame_calibration_params(
    descr: &mut SpectrumDescription,
    t1: f64,
    t2: f64,
    mz_cal_id: i64,
) {
    descr.add_param(Param::builder().name("tdf_t1").curie(TDF_T1_CURIE).value(t1).build());
    descr.add_param(Param::builder().name("tdf_t2").curie(TDF_T2_CURIE).value(t2).build());
    descr.add_param(
        Param::builder()
            .name("tdf_mz_calibration_id")
            .curie(TDF_MZ_CAL_ID_CURIE)
            .value(mz_cal_id)
            .build(),
    );
}

/// The vendor's exact TOF→m/z calibration, carried verbatim so an archive is self-sufficient even
/// with `--no-vendor`: every `MzCalibration` row of `analysis.tdf` (all columns, as stored) plus the
/// `GlobalMetadata` constants timsrust's two-point `ims_calibration` chord is built from. The
/// per-frame inputs (`Frames.T1/T2/MzCalibration`) ride in `spectra_metadata` via
/// [`add_frame_calibration_params`].
///
/// The expression readers are expected to evaluate for `ModelType = 1` — derived and verified in
/// speXtract v0.2.0 to 2.5e-5 ppm against Bruker's timsdata SDK (three diaPASEF runs, 60 golden
/// points); `dC2 = 0` on every file seen, so `T2`'s role is unverified and it is carried as-is:
///
/// ```text
///   t_ns   = tof * DigitizerTimebase + DigitizerDelay
///   C1_eff = C1 * (1 + dC1 * (T1 - tdf_t1) / 1e6)             // T1: calibration row; tdf_t1: the frame
///   t_ns   = C0 + (1e6 / sqrt(C1_eff)) * sqrt(mz) + C2 * mz   // solve for sqrt(mz); C2 = 0 → pure sqrt
/// ```
///
/// Dropping `C2·mz` costs −11…−40 ppm, dropping the temperature term ~0.7 ppm over speXtract's ~30 mK
/// runs (0.06 ppm over 2485.d's 3 mK); the two-point chord in `ims_calibration` is −5…−11 ppm
/// biased there and +3.2…−4.2 ppm on 2485.d (m/z dependent). `ims_calibration.a/b` stay the reader
/// contract; this block is the exact model beside it.
pub fn vendor_mz_calibration(tdf: &Path) -> Result<serde_json::Value> {
    let conn = rusqlite::Connection::open_with_flags(tdf, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening {}", tdf.display()))?;
    let mut stmt = conn
        .prepare("SELECT * FROM MzCalibration ORDER BY Id")
        .context("querying MzCalibration")?;
    let cols: Vec<String> = stmt.column_names().iter().map(|c| c.to_string()).collect();
    let mut rows_out = Vec::new();
    let mut rows = stmt.query([]).context("reading MzCalibration")?;
    while let Some(row) = rows.next().context("reading MzCalibration")? {
        let mut obj = serde_json::Map::new();
        for (k, name) in cols.iter().enumerate() {
            let v = match row.get_ref(k).context("MzCalibration cell")? {
                ValueRef::Null => serde_json::Value::Null,
                ValueRef::Integer(i) => i.into(),
                ValueRef::Real(f) => f.into(),
                ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned().into(),
                ValueRef::Blob(b) => format!("<blob {} bytes>", b.len()).into(),
            };
            obj.insert(name.clone(), v);
        }
        rows_out.push(serde_json::Value::Object(obj));
    }
    if rows_out.is_empty() {
        bail!("MzCalibration has no rows");
    }
    let mut global = serde_json::Map::new();
    for key in ["DigitizerNumSamples", "MzAcqRangeLower", "MzAcqRangeUpper"] {
        let v: Option<String> = conn
            .query_row("SELECT Value FROM GlobalMetadata WHERE Key = ?1", [key], |r| r.get(0))
            .optional()
            .with_context(|| format!("reading GlobalMetadata.{key}"))?;
        // GlobalMetadata values are TEXT; store the ones that parse as numbers.
        let v = match v {
            Some(s) => s
                .trim()
                .parse::<i64>()
                .map(serde_json::Value::from)
                .or_else(|_| s.trim().parse::<f64>().map(serde_json::Value::from))
                .unwrap_or(serde_json::Value::String(s)),
            None => serde_json::Value::Null,
        };
        global.insert(key.to_string(), v);
    }
    // The exact spectra_metadata column names the writer derives for the per-frame params.
    let per_frame_columns: Vec<String> = [
        (TDF_T1_CURIE, "tdf_t1"),
        (TDF_T2_CURIE, "tdf_t2"),
        (TDF_MZ_CAL_ID_CURIE, "tdf_mz_calibration_id"),
    ]
    .iter()
    .map(|(c, n)| mzpeak_prototyping::writer::inflect_cv_term_to_column_name(*c, n, None))
    .collect();
    Ok(serde_json::json!({
        "source": "analysis.tdf",
        "mz_calibration": rows_out,
        "global_metadata": global,
        "per_frame_columns": per_frame_columns,
        "per_frame_columns_note": "spectra_metadata columns holding Frames.T1, Frames.T2, Frames.MzCalibration per spectrum (in this order); the id selects the mz_calibration row by Id",
        "model_type_1": "t_ns = tof*DigitizerTimebase + DigitizerDelay; C1_eff = C1*(1 + dC1*(T1 - tdf_t1)/1e6); t_ns = C0 + (1e6/sqrt(C1_eff))*sqrt(mz) + C2*mz, solve for sqrt(mz) (C2 = 0: mz = ((t_ns - C0)*sqrt(C1_eff)/1e6)^2)",
        "model_type_1_verified": "2.5e-5 ppm vs Bruker timsdata SDK (speXtract v0.2.0); dC2 = 0 on every file seen, T2 role unverified",
    }))
}

/// One quadrupole isolation window within an MS2 frame.
///
/// A TDF MS2 frame is a whole TIMS ramp (900–1600 scans), and the quadrupole retunes *during* the
/// ramp: each `[scan_begin, scan_end)` sub-range gets its own isolation window and collision energy.
/// So one frame carries N windows over disjoint mobility ranges — ~1.6 on average for DDA-PASEF,
/// 5.0 for dia-PASEF. mzdata splits these into N mzML spectra because mzML has nowhere to put the
/// mobility dimension; mzPeak does, so we keep the frame whole and attach N precursors to it.
pub(crate) struct FrameWindow {
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
    /// `Precursors.Parent` — the TDF Id of the survey (MS1) frame this precursor was detected in.
    /// Becomes the `precursor_id` (`frame=N`), which the writer resolves into `precursor_index`.
    parent: Option<i64>,
    /// `Precursors.ScanNumber` — the FRACTIONAL scan position of the precursor's mobility peak.
    /// Strictly better than the isolation window's integer midpoint, which is off by up to a full
    /// scan (measured 0.882 on frame 2 of the reference DDA run).
    scan_number: Option<f64>,
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
        build_precursors(windows, |scan| self.mobility_for_scan_f(scan))
    }
}

/// Build the mzdata precursors for one frame's isolation windows.
///
/// Free-standing so BOTH timsTOF lanes share it: the native (timsrust) reader and the `--bruker-sdk`
/// reader, which previously wrote every MS2 frame with no precursor at all. `mobility` maps a
/// (fractional) TIMS scan position to 1/K0 — the native lane passes its ModelType-2 recalibration,
/// the SDK lane passes the vendor's own `tims_scannum_to_oneoverk0`.
pub(crate) fn build_precursors(
    windows: &[FrameWindow],
    mobility: impl Fn(f64) -> f64,
) -> Vec<Precursor> {
    {
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
                // Mobility of the precursor. DDA-PASEF records the FRACTIONAL scan position of the
                // detected mobility peak in `Precursors.ScanNumber` — use it. Only when it is
                // absent (dia-PASEF, or a dangling precursor) fall back to the isolation window's
                // midpoint, which is off by up to a full scan.
                let scan = w
                    .scan_number
                    .unwrap_or_else(|| (w.scan_begin as f64 + w.scan_end as f64) / 2.0);
                ion.add_param(
                    Param::builder()
                        .name("inverse reduced ion mobility")
                        .curie(curie!(MS:1002815))
                        .value(mobility(scan))
                        .unit(Unit::VoltSecondPerSquareCentimeter)
                        .build(),
                );
                // The midpoint ALONE is not enough to reconstruct the split: a frame's windows
                // cover generally ASYMMETRIC scan ranges (e.g. [34,602) and [602,944)), so a
                // reader splitting at the midpoint between adjacent centres misplaces the
                // boundary — measured 3.5% of the mobility axis on average, up to 10.3%, on a
                // real dia-PASEF run. Emit the true bounds so readers never have to guess.
                // Carried as MZP:1000006/7 (converter-owned provisional terms; PSI-MS has none
                // for an isolation window's mobility bounds). The vendored writer/reader route
                // every CURIE through `curie_to_string`/`parse_curie`, so the Unknown-CV
                // representation is safe there; the mzML export demotes them to userParam.
                add_isolation_mobility_band(
                    &mut ion,
                    mobility(w.scan_begin as f64),
                    mobility(w.scan_end as f64),
                );
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
                    // Parent survey frame. Spectrum ids are `frame=<TDF Id>` and the writer resolves
                    // `precursor_id` against its id→index map to fill `precursor_index`, so naming
                    // the parent here is the whole linkage — without it an MS2 cannot be traced back
                    // to the MS1 it was selected from.
                    precursor_id: w.parent.map(|p| format!("frame={p}")),
                    ..Default::default()
                }
            })
            .collect()
    }
}

impl NativeTofReader {
    /// MS level for frame `i` from the TDF `MsMsType`, defaulting to MS1 when the table is
    /// unavailable. Never returns 0 — `ms_level` 0 is not a legal MS stage under `MS:1000511`.
    #[inline]
    fn ms_level_at(&self, i: usize) -> u8 {
        self.table.ms_level.get(i).copied().unwrap_or(1)
    }

    /// Per-frame `T1`/`T2`/`MzCalibration` → spectrum params (absent when the table lacks them).
    fn add_frame_calibration(&self, descr: &mut SpectrumDescription, i: usize) {
        if let Some((t1, t2, id)) = frame_calibration_at(&self.table, i) {
            add_frame_calibration_params(descr, t1, t2, id);
        }
    }

    #[inline]
    pub fn mobility_for_scan(&self, scan: usize) -> f64 {
        self.mobility_for_scan_f(scan as f64)
    }

    /// 1/K0 at a FRACTIONAL scan position. `Precursors.ScanNumber` is fractional (the mobility peak
    /// apex, not a scan boundary), so rounding it to an integer throws away up to a full scan of
    /// precision. The vendor model is continuous and takes the value directly; timsrust's linear
    /// converter is integer-only, so interpolate between the neighbouring scans — exact for a linear
    /// model, which is what that path is.
    pub fn mobility_for_scan_f(&self, scan: f64) -> f64 {
        match &self.recal {
            Some(c) => c.one_over_k0(scan), // vendor ModelType-2 rational
            None => {
                let lo = scan.floor().max(0.0);
                let frac = scan - lo;
                let a = self.im.convert(lo as u32);
                if frac == 0.0 {
                    a
                } else {
                    a + frac * (self.im.convert(lo as u32 + 1) - a)
                }
            }
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
        self.add_frame_calibration(&mut descr, i);
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
        self.add_frame_calibration(&mut descr, i);
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
pub(crate) fn read_frame_windows(tdf: &Path) -> Result<HashMap<i64, Vec<FrameWindow>>> {
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
                        p.CollisionEnergy, pr.MonoisotopicMz, pr.AverageMz, pr.Charge, pr.Intensity, \
                        pr.Parent, pr.ScanNumber \
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
                        parent: r.get(10)?,
                        scan_number: r.get(11)?,
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
                        // dia-PASEF windows are scheduled, not detected: there is no parent survey
                        // frame and no precursor mobility peak to record.
                        parent: None,
                        scan_number: None,
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

/// The calibration inputs (`T1`, `T2`, `MzCalibration`) of frame `i`, or `None` when the table
/// lacks the columns or any of the three is NULL for that frame — such a frame simply gets no
/// per-frame calibration columns; it never aborts the conversion.
fn frame_calibration_at(table: &FrameTable, i: usize) -> Option<(f64, f64, i64)> {
    match (table.t1.get(i), table.t2.get(i), table.mz_cal_id.get(i)) {
        (Some(&Some(t1)), Some(&Some(t2)), Some(&Some(id))) => Some((t1, t2, id)),
        _ => None,
    }
}

/// Read the per-frame [`FrameTable`] from `analysis.tdf`, ordered by `Id` so position `i` matches
/// timsrust's frame index.
fn read_frame_table(tdf: &Path) -> Result<FrameTable> {
    let conn = rusqlite::Connection::open_with_flags(tdf, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| anyhow::anyhow!("opening {} for Frames: {e}", tdf.display()))?;
    // T1/T2/MzCalibration are in every TDF schema seen; should one lack them, keep the core four
    // rather than failing the conversion.
    match read_frame_rows(&conn, true) {
        Ok(t) => Ok(t),
        Err(e) => {
            log::warn!("TDF Frames T1/T2/MzCalibration unavailable ({e}); per-frame calibration columns omitted");
            read_frame_rows(&conn, false)
        }
    }
}

fn read_frame_rows(conn: &rusqlite::Connection, with_cal: bool) -> Result<FrameTable> {
    let sql = if with_cal {
        "SELECT NumPeaks, Time, MsMsType, Polarity, T1, T2, MzCalibration FROM Frames ORDER BY Id"
    } else {
        "SELECT NumPeaks, Time, MsMsType, Polarity FROM Frames ORDER BY Id"
    };
    let mut stmt = conn.prepare(sql).map_err(|e| anyhow::anyhow!("querying Frames: {e}"))?;
    let mut rows = stmt.query([]).map_err(|e| anyhow::anyhow!("reading Frames: {e}"))?;
    let mut t = FrameTable::default();
    while let Some(r) = rows.next().map_err(|e| anyhow::anyhow!("collecting Frames: {e}"))? {
        t.num_peaks.push(r.get::<_, i64>(0)?.max(0) as u32);
        t.rt.push(r.get::<_, f64>(1)?);
        // TDF MsMsType: 0 is full-scan MS1; every other value (2 MRM, 8 PASEF, 9 dia-PASEF)
        // is a fragmentation frame, i.e. MS2.
        t.ms_level.push(if r.get::<_, i64>(2)? == 0 { 1u8 } else { 2u8 });
        t.polarity.push(match r.get::<_, String>(3)?.trim() {
            "+" => ScanPolarity::Positive,
            "-" => ScanPolarity::Negative,
            _ => ScanPolarity::Unknown,
        });
        if with_cal {
            t.t1.push(r.get::<_, Option<f64>>(4)?);
            t.t2.push(r.get::<_, Option<f64>>(5)?);
            t.mz_cal_id.push(r.get::<_, Option<i64>>(6)?);
        }
    }
    Ok(t)
}

#[cfg(test)]
mod vendor_mz_calibration_tests {
    /// Rows come back verbatim (every column, typed) and the TEXT GlobalMetadata constants parse to
    /// numbers. Values are the PXD059079 2485.d calibration.
    #[test]
    fn carries_rows_verbatim_and_global_constants() {
        let dir = std::env::temp_dir().join(format!("mzpc-vmc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tdf = dir.join("analysis.tdf");
        let _ = std::fs::remove_file(&tdf);
        let conn = rusqlite::Connection::open(&tdf).unwrap();
        conn.execute_batch(
            "CREATE TABLE MzCalibration (Id INTEGER PRIMARY KEY, ModelType INTEGER, DigitizerTimebase REAL, \
             DigitizerDelay REAL, T1 REAL, T2 REAL, dC1 REAL, dC2 REAL, C0 REAL, C1 REAL, C2 REAL, C3 REAL, C4 REAL); \
             INSERT INTO MzCalibration VALUES (1, 1, 0.125, 26464.125, 25.6148127740566, 25.1594285616696, \
             20.0, 0.0, 1008.59723408404, 154314.98518964, 0.0, 0.0, 0.0); \
             CREATE TABLE GlobalMetadata (Key TEXT, Value TEXT); \
             INSERT INTO GlobalMetadata VALUES ('DigitizerNumSamples', '636031'), \
             ('MzAcqRangeLower', '99.993933'), ('MzAcqRangeUpper', '1700.000000');",
        )
        .unwrap();
        drop(conn);
        let v = super::vendor_mz_calibration(&tdf).unwrap();
        let rows = v["mz_calibration"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["Id"], 1);
        assert_eq!(rows[0]["ModelType"], 1);
        assert_eq!(rows[0]["DigitizerTimebase"], 0.125);
        assert_eq!(rows[0]["DigitizerDelay"], 26464.125);
        assert_eq!(rows[0]["dC1"], 20.0);
        assert_eq!(rows[0]["C1"], 154314.98518964);
        assert_eq!(rows[0].as_object().unwrap().len(), 13, "all 13 columns: {}", rows[0]);
        assert_eq!(v["global_metadata"]["DigitizerNumSamples"], 636031);
        assert_eq!(v["global_metadata"]["MzAcqRangeLower"], 99.993933);
        assert_eq!(v["global_metadata"]["MzAcqRangeUpper"], 1700.0);
        assert!(v["model_type_1"].as_str().unwrap().contains("DigitizerTimebase"));
        let cols: Vec<&str> = v["per_frame_columns"].as_array().unwrap().iter().map(|c| c.as_str().unwrap()).collect();
        assert!(cols[0].ends_with("_tdf_t1") && cols[1].ends_with("_tdf_t2") && cols[2].ends_with("_tdf_mz_calibration_id"), "{cols:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A run may reference MORE THAN ONE `MzCalibration` row (the instrument recalibrates
    /// mid-run): both rows must be carried verbatim, in `Id` order, and each frame's
    /// `tdf_mz_calibration_id` must select ITS row. A frame with NULL `T1` (seen on interrupted
    /// runs) yields no per-frame calibration for that frame only — never an abort, and never a
    /// shift of the neighbouring frames' positions.
    #[test]
    fn two_calibration_rows_select_per_frame_and_null_frame_is_tolerated() {
        let dir = std::env::temp_dir().join(format!("mzpc-vmc2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tdf = dir.join("analysis.tdf");
        let _ = std::fs::remove_file(&tdf);
        let conn = rusqlite::Connection::open(&tdf).unwrap();
        conn.execute_batch(
            "CREATE TABLE MzCalibration (Id INTEGER PRIMARY KEY, ModelType INTEGER, DigitizerTimebase REAL, \
             DigitizerDelay REAL, T1 REAL, T2 REAL, dC1 REAL, dC2 REAL, C0 REAL, C1 REAL, C2 REAL, C3 REAL, C4 REAL); \
             INSERT INTO MzCalibration VALUES (1, 1, 0.125, 26464.125, 25.6148127740566, 25.1594285616696, \
             20.0, 0.0, 1008.59723408404, 154314.98518964, 0.0, 0.0, 0.0); \
             INSERT INTO MzCalibration VALUES (2, 1, 0.125, 26464.125, 25.7001, 25.2002, \
             20.0, 0.0, 1008.61, 154315.5, 1.26e-3, 0.0, 0.0); \
             CREATE TABLE GlobalMetadata (Key TEXT, Value TEXT); \
             INSERT INTO GlobalMetadata VALUES ('DigitizerNumSamples', '636031'), \
             ('MzAcqRangeLower', '99.993933'), ('MzAcqRangeUpper', '1700.000000'); \
             CREATE TABLE Frames (Id INTEGER PRIMARY KEY, NumPeaks INTEGER, Time REAL, MsMsType INTEGER, \
             Polarity TEXT, T1 REAL, T2 REAL, MzCalibration INTEGER); \
             INSERT INTO Frames VALUES (1, 10, 0.5, 0, '+', 25.61, 25.16, 1); \
             INSERT INTO Frames VALUES (2, 10, 0.6, 9, '+', 25.62, 25.16, 2); \
             INSERT INTO Frames VALUES (3, 10, 0.7, 0, '+', NULL, 25.16, 1); \
             INSERT INTO Frames VALUES (4, 10, 0.8, 9, '+', 25.64, 25.17, 2); \
             INSERT INTO Frames VALUES (5, 10, 0.9, 0, '+', 25.65, 25.17, 1);",
        )
        .unwrap();
        drop(conn);

        // Index block: both rows, verbatim, Id-ordered.
        let v = super::vendor_mz_calibration(&tdf).unwrap();
        let rows = v["mz_calibration"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["Id"], 1);
        assert_eq!(rows[1]["Id"], 2);
        assert_eq!(rows[0]["C2"], 0.0);
        assert_eq!(rows[1]["C2"], 1.26e-3);
        assert_eq!(rows[1]["T1"], 25.7001);
        assert_eq!(rows[1]["C1"], 154315.5);
        assert_eq!(rows[1].as_object().unwrap().len(), 13);

        // Per-frame table: five entries (the NULL frame keeps its slot), ids select the right row.
        let table = super::read_frame_table(&tdf).unwrap();
        assert_eq!(table.num_peaks.len(), 5);
        assert_eq!(table.mz_cal_id, vec![Some(1), Some(2), Some(1), Some(2), Some(1)]);
        assert_eq!(table.t1, vec![Some(25.61), Some(25.62), None, Some(25.64), Some(25.65)]);
        assert_eq!(table.ms_level, vec![1, 2, 1, 2, 1]);
        assert_eq!(super::frame_calibration_at(&table, 0), Some((25.61, 25.16, 1)));
        assert_eq!(super::frame_calibration_at(&table, 1), Some((25.62, 25.16, 2)));
        assert_eq!(super::frame_calibration_at(&table, 2), None, "NULL T1 frame yields no params");
        assert_eq!(super::frame_calibration_at(&table, 3), Some((25.64, 25.17, 2)));
        assert_eq!(super::frame_calibration_at(&table, 4), Some((25.65, 25.17, 1)));
        assert_eq!(super::frame_calibration_at(&table, 5), None, "past the end");

        // The spectrum params the writer promotes to columns name the row per frame.
        use mzdata::prelude::ParamDescribed;
        let mut d = mzdata::spectrum::SpectrumDescription::default();
        let (t1, t2, id) = super::frame_calibration_at(&table, 1).unwrap();
        super::add_frame_calibration_params(&mut d, t1, t2, id);
        let id_param = d.get_param_by_curie(&super::TDF_MZ_CAL_ID_CURIE).unwrap();
        assert_eq!(id_param.value.to_i64().unwrap(), 2);
        assert_eq!(d.get_param_by_curie(&super::TDF_T1_CURIE).unwrap().value.to_f64().unwrap(), 25.62);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod isolation_mobility_band_tests {
    use super::*;
    use mzdata::spectrum::ScanEvent;

    fn im(p: &Param) -> f64 {
        p.value.to_f64().unwrap()
    }

    /// The window band is carried as MZP:1000006/7 (Unknown-CV CURIEs rendered `MZP:` by the
    /// vendored writer), ordered lower <= upper whatever order the scan bounds arrive in.
    #[test]
    fn band_is_accessioned_and_ordered() {
        let mut ion = SelectedIon::default();
        add_isolation_mobility_band(&mut ion, 1.36, 1.30); // scan_begin (high 1/K0) first
        let ps = ion.params.as_ref().unwrap();
        assert_eq!(ps.len(), 2);
        assert_eq!(ps[0].curie(), Some(MZP_IM_WINDOW_LOWER));
        assert_eq!(ps[1].curie(), Some(MZP_IM_WINDOW_UPPER));
        assert_eq!(ps[0].name, IM_WINDOW_LOWER_NAME);
        assert_eq!(im(&ps[0]), 1.30);
        assert_eq!(im(&ps[1]), 1.36);
        assert_eq!(ps[0].unit, Unit::VoltSecondPerSquareCentimeter);
        assert_eq!(
            mzpeak_prototyping::param::curie_to_string(&ps[0].curie().unwrap()),
            "MZP:1000006"
        );
        assert_eq!(
            mzpeak_prototyping::param::curie_to_string(&ps[1].curie().unwrap()),
            "MZP:1000007"
        );
    }

    /// mzdata's TDF reader puts timsrust-LINEAR 1/K0 in the selected ion, the scan and the frame's
    /// `ion mobility lower/upper limit`; the remap must move all three onto the ModelType-2 model at
    /// the SAME scan position (what the ims-compact lane writes) and attach the MZP band from the
    /// limits — without touching anything else, and idempotently for the band.
    ///
    /// The input is spelled the way mzdata 0.66 spells it (`io/tdf/reader.rs`): `lower` =
    /// convert(ScanNumBegin), `upper` = convert(ScanNumEnd) — i.e. INVERTED, since 1/K0 falls with
    /// the scan index. The remap must leave the pair ordered.
    #[test]
    fn remap_moves_linear_values_onto_vendor_model_and_adds_band() {
        // SBA415: nominal 1/K0 range 0.600..1.600 over 909 scans; ModelType-2 row from its TDF.
        let linear = Scan2ImConverter::from_boundaries(0.600, 1.600, 909);
        let recal = crate::tims_mobility::TimsMobilityCalibration::new(
            1.0, 909.0, 211.45198604901222, 73.95258004355563, 0.00492817555366883,
            131.11541877221117, 0.600,
        );
        let remap = TdfMobilityRemap::new(linear, Some(recal));
        let (sb, se) = (100u32, 160u32);
        let mid = (sb + se) as f64 / 2.0;
        // The premise of the ordering fix: mzdata's "lower" (scan begin) is the LARGER 1/K0.
        assert!(linear.convert(sb) > linear.convert(se));

        let mut descr = SpectrumDescription::default();
        descr.add_param(
            Param::new_key_value("ion mobility lower limit", linear.convert(sb))
                .with_unit_t(&Unit::VoltSecondPerSquareCentimeter),
        );
        descr.add_param(
            Param::new_key_value("ion mobility upper limit", linear.convert(se))
                .with_unit_t(&Unit::VoltSecondPerSquareCentimeter),
        );
        descr.add_param(Param::new_key_value("window group", 3i64));
        let mut scan = ScanEvent::default();
        scan.add_param(
            Param::builder()
                .name("inverse reduced ion mobility")
                .curie(curie!(MS:1002815))
                .value(linear.convert(mid))
                .unit(Unit::VoltSecondPerSquareCentimeter)
                .build(),
        );
        descr.acquisition.scans.push(scan);
        let mut ion = SelectedIon { mz: 500.0, ..Default::default() };
        ion.add_param(
            Param::builder()
                .name("inverse reduced ion mobility")
                .curie(curie!(MS:1002815))
                .value(linear.convert(mid))
                .unit(Unit::VoltSecondPerSquareCentimeter)
                .build(),
        );
        descr.precursor.push(Precursor { ions: vec![ion], ..Default::default() });

        remap.apply(&mut descr);
        remap.apply(&mut descr); // idempotent for the band (values re-remap only if linear again)

        let lo = descr.get_param_by_name("ion mobility lower limit").unwrap();
        let hi = descr.get_param_by_name("ion mobility upper limit").unwrap();
        // Applied twice: the second pass inverts a ModelType-2 value through the linear map, which
        // is NOT the identity — so check the first pass's arithmetic on a fresh description below
        // and here only that the band exists once and is ordered.
        assert!(im(lo) <= im(hi));
        let ion = &descr.precursor[0].ions[0];
        let ps = ion.params.as_ref().unwrap();
        assert_eq!(ps.iter().filter(|p| p.curie() == Some(MZP_IM_WINDOW_LOWER)).count(), 1);
        assert_eq!(ps.iter().filter(|p| p.curie() == Some(MZP_IM_WINDOW_UPPER)).count(), 1);
        assert!(descr.get_param_by_name("window group").is_some());

        // Fresh description, single pass: exact ModelType-2 values at the same scan positions, and
        // the pair comes out ORDERED (lower = the scan-end value, the smaller one).
        let mut d = SpectrumDescription::default();
        d.add_param(Param::new_key_value("ion mobility lower limit", linear.convert(sb)));
        d.add_param(Param::new_key_value("ion mobility upper limit", linear.convert(se)));
        let mut ion = SelectedIon::default();
        ion.add_param(
            Param::builder()
                .name("inverse reduced ion mobility")
                .curie(curie!(MS:1002815))
                .value(linear.convert(mid))
                .build(),
        );
        d.precursor.push(Precursor { ions: vec![ion], ..Default::default() });
        remap.apply(&mut d);
        let tol = 1e-12;
        assert!((im(d.get_param_by_name("ion mobility lower limit").unwrap()) - recal.one_over_k0(se as f64)).abs() < tol);
        assert!((im(d.get_param_by_name("ion mobility upper limit").unwrap()) - recal.one_over_k0(sb as f64)).abs() < tol);
        let ps = d.precursor[0].ions[0].params.as_ref().unwrap();
        let v = ps.iter().find(|p| p.curie() == Some(curie!(MS:1002815))).unwrap();
        assert!((im(v) - recal.one_over_k0(mid)).abs() < tol, "{} vs {}", im(v), recal.one_over_k0(mid));
        // The linear value really was different (else this test proves nothing).
        assert!((linear.convert(mid) - recal.one_over_k0(mid)).abs() > 1e-3);
        let band_lo = ps.iter().find(|p| p.curie() == Some(MZP_IM_WINDOW_LOWER)).unwrap();
        let band_hi = ps.iter().find(|p| p.curie() == Some(MZP_IM_WINDOW_UPPER)).unwrap();
        assert!((im(band_lo) - recal.one_over_k0(se as f64)).abs() < tol);
        assert!((im(band_hi) - recal.one_over_k0(sb as f64)).abs() < tol);
        assert!(im(band_lo) <= im(v) && im(v) <= im(band_hi));

        // No ModelType-2 row (or `--no-tims-recalibration`): values stay linear but the pair is
        // still put in order, and the band is still attached (from the ordered linear limits).
        let none = TdfMobilityRemap::new(linear, None);
        let mut d = SpectrumDescription::default();
        d.add_param(Param::new_key_value("ion mobility lower limit", linear.convert(sb)));
        d.add_param(Param::new_key_value("ion mobility upper limit", linear.convert(se)));
        d.precursor.push(Precursor { ions: vec![SelectedIon::default()], ..Default::default() });
        none.apply(&mut d);
        assert_eq!(im(d.get_param_by_name("ion mobility lower limit").unwrap()), linear.convert(se));
        assert_eq!(im(d.get_param_by_name("ion mobility upper limit").unwrap()), linear.convert(sb));
        let ps = d.precursor[0].ions[0].params.as_ref().unwrap();
        assert_eq!(im(ps.iter().find(|p| p.curie() == Some(MZP_IM_WINDOW_LOWER)).unwrap()), linear.convert(se));
        assert_eq!(im(ps.iter().find(|p| p.curie() == Some(MZP_IM_WINDOW_UPPER)).unwrap()), linear.convert(sb));

        // An already-ordered pair is left alone (idempotent ordering).
        let mut d = SpectrumDescription::default();
        d.add_param(Param::new_key_value("ion mobility lower limit", 0.9));
        d.add_param(Param::new_key_value("ion mobility upper limit", 1.1));
        none.apply(&mut d);
        assert_eq!(im(d.get_param_by_name("ion mobility lower limit").unwrap()), 0.9);
        assert_eq!(im(d.get_param_by_name("ion mobility upper limit").unwrap()), 1.1);
    }
}

#[cfg(test)]
mod single_point_chunk_tests {
    use mzdata::prelude::*;
    use mzdata::spectrum::{ArrayType, BinaryDataArrayType, DataArray};
    use mzpeak_prototyping::chunk_series::ChunkingStrategy;

    /// A chunk whose only point sits at coordinate ZERO must still decode to that point.
    ///
    /// `decode_arrow` used to open with `if start == 0.0 && end == 0.0 { return 0 }`, standing in for
    /// "this chunk row is absent" — but null bounds and a real bound of 0.0 were indistinguishable
    /// because the bounds were read past their null mask. TOF bin 0 occurs in real timsTOF data
    /// (`min(tof) == 0` on the reference DDA run), so with a small `--chunk-size` a genuine chunk at
    /// zero decoded to nothing while its intensity/mobility arrays kept their entries: one point
    /// silently lost, and a length desync. Absence is now taken from the null mask instead.
    #[test]
    fn zero_bounded_chunk_still_decodes_its_point() {
        let empty = arrow::array::new_empty_array(&arrow::datatypes::DataType::Int32);
        let mut acc = DataArray::from_name_and_type(
            &ArrayType::nonstandard("tof"),
            BinaryDataArrayType::Int32,
        );
        let n = (ChunkingStrategy::Delta { chunk_size: 50.0 })
            .decode_arrow(&empty, 0.0, 0.0, &mut acc, None);
        assert_eq!(n, 1, "a single-point chunk at coordinate 0 must decode to one point");
        assert_eq!(acc.to_i32().unwrap().to_vec(), vec![0]);
    }

    /// A single-point chunk stores an EMPTY values list (the start point lives in `chunk_start`).
    /// The reader used to feed a hard-coded empty **Float64** array into the decoder for that case,
    /// which pushed an f64 into the Int32 `tof` accumulator and panicked with `DataTypeSizeMismatch`
    /// — making every `--ims-chunked` archive unreadable, since 105 of 415 chunks are single-point
    /// at the default 50 Th width. Decoding an empty Int32 chunk must yield exactly the start point.
    #[test]
    fn empty_int32_chunk_decodes_to_its_start_point() {
        let empty = arrow::array::new_empty_array(&arrow::datatypes::DataType::Int32);
        let mut acc = DataArray::from_name_and_type(
            &ArrayType::nonstandard("tof"),
            BinaryDataArrayType::Int32,
        );
        let n = (ChunkingStrategy::Delta { chunk_size: 50.0 })
            .decode_arrow(&empty, 123_456.0, 123_456.0, &mut acc, None);
        assert_eq!(n, 1, "a single-point chunk decodes to exactly one point");
        assert_eq!(acc.to_i32().unwrap().to_vec(), vec![123_456]);
    }
}

#[cfg(test)]
mod empty_frame_read_tests {
    /// Random access to an EMPTY spectrum must not abort the process.
    ///
    /// Newer timsTOF (5.1.x) writes frames with `NumPeaks = 0`, which this build converts. The point
    /// reader's binary search finds no span for such an index and used to `panic!`; with this crate's
    /// `panic = "abort"` profile that kills the host on an ordinary read. Exercised end to end: build
    /// an archive containing an empty spectrum and read every spectrum back by index.
    #[test]
    #[ignore = "needs the reference corpus (MZPEAK_CORPUS)"]
    fn random_access_to_empty_spectrum_does_not_abort() {
        use mzpeak_prototyping::MzPeakReader;
        let Ok(root) = std::env::var("MZPEAK_CORPUS") else { return };
        let archive = match std::env::var("MZPEAK_EMPTY_ARCHIVE") {
            Ok(v) => std::path::PathBuf::from(v),
            Err(_) => match walk(std::path::Path::new(&root), 6) {
                Some(p) => p,
                None => {
                    eprintln!("skipping: no .mzpeak with an empty spectrum under {root}");
                    return;
                }
            },
        };
        let mut r = match MzPeakReader::new(&archive) {
            Ok(r) => r,
            Err(e) => { eprintln!("skipping: {}: {e}", archive.display()); return }
        };
        let n = r.len().min(500);
        let mut empties = 0usize;
        for i in 0..n {
            match r.get_spectrum_peaks_for(i as u64) {
                Ok(peaks) => {
                    if peaks.map(|p| p.is_empty()).unwrap_or(true) { empties += 1 }
                }
                Err(e) => panic!("read of spectrum {i} failed: {e}"),
            }
        }
        eprintln!("read {n} spectra from {}; {empties} empty", archive.display());
    }

    /// First archive under `dir` that reports at least one zero-point spectrum.
    fn walk(dir: &std::path::Path, depth: usize) -> Option<std::path::PathBuf> {
        if depth == 0 { return None }
        let mut entries: Vec<_> = std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).collect();
        entries.sort();
        for p in &entries {
            if p.extension().is_some_and(|e| e == "mzpeak") && p.is_file() && has_empty_spectrum(p) {
                return Some(p.clone());
            }
        }
        entries.into_iter().filter(|p| p.is_dir()).find_map(|p| walk(&p, depth - 1))
    }

    fn has_empty_spectrum(archive: &std::path::Path) -> bool {
        let Ok(mut r) = mzpeak_prototyping::MzPeakReader::new(archive) else { return false };
        use mzdata::prelude::SpectrumLike;
        (0..r.len().min(4000)).any(|i| {
            mzdata::prelude::SpectrumSource::get_spectrum_by_index(&mut r, i)
                .is_some_and(|s| s.peaks().len() == 0)
        })
    }
}
