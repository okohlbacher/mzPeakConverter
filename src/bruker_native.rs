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

use mzpeak_prototyping::grid::{GridEncoding, GridModelLike, SquareRootLinearGrid, TimsTofMzGrid2, TimsTofTimsLinearGrid2};
use timsrust::converters::{ConvertableDomain, Scan2ImConverter, Tof2MzConverter};
use timsrust::readers::{FrameReader, MetadataReader};
use timsrust::MSLevel;

/// The intensity column of an ims-compact spectrum, in the dtype the archive stores: Int32 native
/// counts by default (the writer BYTE_STREAM_SPLITs the column, ~-16 %, lossless), Float32 under
/// `MZPC_BYTE_PLANE_INTENSITY=0`.
pub(crate) enum ImsIntensity<'a> {
    Counts(&'a [i32]),
    Float(&'a [f32]),
}

/// The mobility of a frame's points, as the grid stores it: TIMS scan numbers under the vendor's
/// ModelType-2 model (the reference implementation's `TimsTofTimsLinearGrid2`), or plain 1/K0 values
/// when there is no such model (`--no-tims-recalibration`: timsrust's linear approximation, which no
/// grid expresses).
pub(crate) enum ImsMobility<'a> {
    Scans(&'a [u32], &'a GridEncoding),
    Values(&'a [f64]),
}

/// A grid model as the Param the reference implementation reads it back from
/// (`GridPolicy::find_grid_model_param` → `GridEncoding::from_param`): the model's own accession
/// (`MS:9999002` timsTOF m/z, `MS:9999001` TIMS, `MS:1003825` sqrt) with its parameter list.
pub(crate) fn grid_param(model: &GridEncoding) -> Param {
    Param::builder()
        .curie(model.grid_type())
        .name("grid model")
        .value(mzdata::params::Value::List(model.parameters().into_iter().map(mzdata::params::Value::Float).collect()))
        .build()
}

/// **The three arrays of an ims-compact frame on the reference implementation's chunk grid**: the
/// m/z of every point evaluated from its TOF bin through the frame's model (`from_index`, so the
/// value the writer re-indexes and the bounds it writes are bit-identical to what a reader decodes),
/// intensity in detector counts, and 1/K0 likewise from the scan number — each grid axis carrying its
/// model as a Param. `tof` must be sorted (a chunk row's indices are `[first, deltas…]`, unsigned).
///
/// One constructor because three places build this triple (the two native builders and the SDK
/// builder) and they MUST agree on ArrayType + dtype + unit + Param. Returns the arrays and the m/z
/// values (for the summary terms).
pub(crate) fn ims_grid_arrays(
    tof: &[i32],
    intensity: ImsIntensity<'_>,
    mobility: ImsMobility<'_>,
    mz_model: &GridEncoding,
) -> anyhow::Result<(BinaryArrayMap, Vec<f64>)> {
    debug_assert!(tof.is_sorted(), "grid indices must be non-decreasing");
    let mz: Vec<f64> = tof
        .iter()
        .map(|&k| u32::try_from(k).map(|k| mz_model.from_index(k)).map_err(|_| anyhow::anyhow!("negative TOF bin {k}")))
        .collect::<anyhow::Result<_>>()?;
    let mut arrays = BinaryArrayMap::new();
    let mut mz_da = DataArray::wrap(&ArrayType::MZArray, BinaryDataArrayType::Float64, Vec::new());
    mz_da.update_buffer(mz.as_slice()).map_err(|e| anyhow::anyhow!("encoding m/z: {e}"))?;
    mz_da.unit = Unit::MZ;
    mz_da.add_param(grid_param(mz_model));
    arrays.add(mz_da);
    let mut int_da = match intensity {
        ImsIntensity::Counts(v) => {
            let mut da = DataArray::wrap(&ArrayType::IntensityArray, BinaryDataArrayType::Int32, Vec::new());
            da.update_buffer(v).map_err(|e| anyhow::anyhow!("encoding intensity: {e}"))?;
            da
        }
        ImsIntensity::Float(v) => {
            let mut da = DataArray::wrap(&ArrayType::IntensityArray, BinaryDataArrayType::Float32, Vec::new());
            da.update_buffer(v).map_err(|e| anyhow::anyhow!("encoding intensity: {e}"))?;
            da
        }
    };
    int_da.unit = Unit::DetectorCounts;
    arrays.add(int_da);
    let mut mob_da = DataArray::wrap(
        &ArrayType::MeanInverseReducedIonMobilityArray,
        BinaryDataArrayType::Float64,
        Vec::new(),
    );
    match mobility {
        ImsMobility::Scans(scans, model) => {
            let k0: Vec<f64> = scans.iter().map(|&s| model.from_index(s)).collect();
            mob_da.update_buffer(k0.as_slice()).map_err(|e| anyhow::anyhow!("encoding mobility: {e}"))?;
            mob_da.add_param(grid_param(model));
        }
        ImsMobility::Values(k0) => {
            mob_da.update_buffer(k0).map_err(|e| anyhow::anyhow!("encoding mobility: {e}"))?;
        }
    }
    mob_da.unit = Unit::VoltSecondPerSquareCentimeter;
    arrays.add(mob_da);
    Ok((arrays, mz))
}

/// The frame's m/z grid model: its `MzCalibration` row (by `Frames.MzCalibration`; a NULL or unknown
/// id takes the lowest-id row) at the frame's `T1`/`T2` as the reference implementation's 7
/// parameters (`TimsTofMzGrid2`; a ModelType-2 row as its quadratic, see [`TdfMzCalibrationRow::
/// grid_parameters`]); a TDF without a usable row, or a row of another model type, falls back to
/// timsrust's two-point chord `(a + b·tof)²` as an `MS:1003825` sqrt model. Both are exact grids in `tof`; only the chord
/// is an approximation of the vendor's m/z (the archive says which model each row carries).
pub(crate) fn frame_mz_grid(
    rows: &HashMap<i64, TdfMzCalibrationRow>,
    chord: TofMzModel,
    t1: Option<f64>,
    t2: Option<f64>,
    cal_id: Option<i64>,
) -> GridEncoding {
    let row = match cal_id.and_then(|id| rows.get(&id)) {
        Some(r) => Some(*r),
        None => rows.iter().min_by_key(|(id, _)| **id).map(|(_, r)| *r),
    };
    row.filter(|r| matches!(r.model_type, 1 | 2))
        .and_then(|r| GridEncoding::from_parameters(TimsTofMzGrid2::ACCESSION, &r.grid_parameters(t1, t2)))
        .unwrap_or_else(|| {
            GridEncoding::from_parameters(SquareRootLinearGrid::ACCESSION, &[chord.a, chord.b, 1.0])
                .expect("3 sqrt-grid parameters")
        })
}

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
    /// The vendor's `MzCalibration` rows, the per-frame m/z grid models' source ([`frame_mz_grid`]);
    /// empty when the table is unreadable (every frame then carries the chord as its model).
    mz_rows: HashMap<i64, TdfMzCalibrationRow>,
    /// The TIMS ModelType-2 model as the reference implementation's grid (`recal` in its 4-parameter
    /// form); `None` under `--no-tims-recalibration`, when 1/K0 is stored as plain values.
    tims_grid: Option<GridEncoding>,
    /// Frames re-ordered for the grid: points gathered scan by scan that were not already in TOF
    /// order when [`Self::ims_grid_spectrum`] sorted them. The finisher declares `sort-by-mz` from it
    /// (the schema probe counts too; it is one frame of the run).
    frames_reordered: std::sync::atomic::AtomicUsize,
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

/// Converter-owned CURIEs for the per-frame calibration inputs (`Frames.T1`, `Frames.T2`,
/// `Frames.MzCalibration`), which the ims-compact writer promotes to `spectra_metadata` columns
/// (`opt_MZP_1000008_tdf_t1`, `opt_MZP_1000009_tdf_t2`, `opt_MZP_1000010_tdf_mz_calibration_id`).
/// `cv/mzpeak.obo` MZP:1000008–1000010, following the `tof_c0`/`tof_c1` terms in `main.rs`; until
/// 0.10.1 they squatted `MS:4000903`–`MS:4000905` (columns `opt_MS_4000903_tdf_t1` …). Readers bind
/// these by the `_tdf_t1` … column-name suffix, so both generations read.
pub(crate) const TDF_T1_CURIE: CURIE = CURIE::new(ControlledVocabulary::Unknown, 1_000_008);
pub(crate) const TDF_T2_CURIE: CURIE = CURIE::new(ControlledVocabulary::Unknown, 1_000_009);
pub(crate) const TDF_MZ_CAL_ID_CURIE: CURIE = CURIE::new(ControlledVocabulary::Unknown, 1_000_010);

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

/// One `MzCalibration` row of `analysis.tdf` as numbers (SQL NULL → 0 for evaluation, matching the
/// vendor library's `sqlite3_column_double` semantics — but see [`Self::quadratic_terms_stored`]),
/// with the exact ModelType-1 TOF→m/z evaluation.
///
/// The model — derived numerically against Bruker's timsdata library and verified to 2.5e-5 ppm on
/// 60 golden points (speXtract `src/TdfMzCalibration.h`, `tests/calibration_golden.json`, mirrored
/// in `tests/fixtures/tdf_calibration_golden.json`):
///
/// ```text
///   t_ns   = tof * DigitizerTimebase + DigitizerDelay
///   C1_eff = C1 * (1 + dC1 * (T1_row - T1_frame) / 1e6)
///   C2*u² + (1e6/sqrt(C1_eff))*u + (C0 - t_ns) = 0,   m/z = u²        (u = sqrt(m/z))
/// ```
///
/// When `C2 = 0` (and `C3 = C4 = dC2 = 0`) the model is EXACTLY linear in `tof` through the sqrt
/// transform the ims-compact archive already declares (`MS:1003825`): `u = c0 + c1·tof` with
/// `c1 = DigitizerTimebase·sqrt(C1_eff)/1e6`, `c0 = (DigitizerDelay − C0)·sqrt(C1_eff)/1e6` —
/// see [`Self::sqrt_linear_coeffs`]. `C1_eff` depends on the FRAME's `T1`, so the pair is per frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct TdfMzCalibrationRow {
    pub id: i64,
    pub model_type: i64,
    pub digitizer_timebase: f64,
    pub digitizer_delay: f64,
    /// Reference digitizer temperatures of the calibration (`MzCalibration.T1` / `T2`).
    pub t1: f64,
    pub t2: f64,
    pub dc1: f64,
    pub dc2: f64,
    pub c0: f64,
    pub c1: f64,
    pub c2: f64,
    pub c3: f64,
    pub c4: f64,
    /// Whether `C2`, `C3`, `C4` and `dC2` were STORED as numbers (INTEGER/REAL). The vendor schema
    /// declares `C0..C4` untyped and nullable, so a NULL `C2` is indistinguishable from a real
    /// zero through `sqlite3_column_double` — a row whose quadratic term is MISSING must not be
    /// declared exact (the reference `TdfMzCalibration.h` refuses it as "C2 <= 0 or missing"); it
    /// is still evaluated with NULL → 0 for the informational `vendor_mz_calibration` block.
    pub quadratic_terms_stored: bool,
    /// ModelType 2 only: the calibrant polynomial the vendor subtracts inside the calibrant range
    /// (`C5`–`C14`); `None` for ModelType 1 or a TDF without those columns.
    pub calibrant: Option<Calibrant>,
}

/// The ModelType-2 correction of an `MzCalibration` row: `C5`/`C6` bound the calibrant range, `C7`
/// is the number of coefficients, `C8`… the polynomial in m/z. Bruker's library computes
/// `m/z = m − Σ coeffs[i]·mⁱ` for `lo ≤ m ≤ hi` and `m/z = m` outside, where `m` is the quadratic
/// on `C0`, `C1`, `C2` (`C3`/`C4` repeat `C0`/`C2` in these rows and are not the ModelType-1 cubic
/// and shift). Pinned 2026-09-26 against the SDK's own values to 1e-9 ppm
/// (`tests/fixtures/tdf_modeltype2_sdk_golden.json`); clipping `m` to the range instead is 10 ppm
/// off below it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Calibrant {
    pub lo: f64,
    pub hi: f64,
    pub n: usize,
    pub coeffs: [f64; 7],
}

impl Calibrant {
    /// The m/z the vendor subtracts at the quadratic's `m` (0 outside the calibrant range).
    pub fn correction(&self, m: f64) -> f64 {
        if !(self.lo <= m && m <= self.hi) {
            return 0.0;
        }
        self.coeffs[..self.n].iter().rev().fold(0.0, |acc, c| acc * m + c)
    }

    /// The largest correction over the calibrant range, in ppm of m/z — the bound an archive that
    /// stores only the quadratic declares.
    pub fn max_abs_ppm(&self) -> f64 {
        (0..=2000)
            .map(|i| self.lo + (self.hi - self.lo) * i as f64 / 2000.0)
            .filter(|m| *m > 0.0)
            .map(|m| (self.correction(m) / m).abs() * 1e6)
            .fold(0.0, f64::max)
    }
}

impl TdfMzCalibrationRow {
    /// `C1` corrected for the frame's digitizer temperature `t1_frame` (`Frames.T1`).
    #[inline]
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn c1_eff(&self, t1_frame: f64) -> f64 {
        self.c1 * (1.0 + self.dc1 * (self.t1 - t1_frame) / 1e6)
    }

    /// Exact ModelType-1 (and, with its [`Calibrant`], ModelType-2) m/z for a (possibly fractional)
    /// TOF index at frame temperature `t1_frame`:
    /// `t = C0 + b·u + (C2/cf)·u²` with `u = sqrt(m/z + C4)`, `cf = 1 + dC1·(T1 − t1_frame)/1e6`,
    /// `b = 1e6/sqrt(C1·cf)`, solved for `u`, then `m/z = u² − C4`. The quadratic is solved in its
    /// cancellation-free form `u = 2(t − C0)/(b + sqrt(disc))`. Out-of-model inputs (`t < C0`,
    /// negative discriminant, `C1_eff ≤ 0`) yield NaN, never a plausible-looking m/z. `C2 = 0` takes
    /// the linear branch `u = (t − C0)/b`.
    ///
    /// The `− C4` term (a constant m/z shift; Bruker's "reduced mass" m0) and the temperature scaling
    /// of `C2` were missing until 0.12.5: on a file with `C4 = −0.0686` that was 40–720 ppm, hidden
    /// because every SDK golden so far had `C4 = 0`, and because `C4 ≠ 0` rows never take the exact
    /// per-frame pair anyway ([`Self::is_sqrt_linear`]). Verified to 1e-9 ppm against the SDK on
    /// such a file (`tests/fixtures/tdf_diapasef_sdk_golden.json`).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn tof_to_mz(&self, tof: f64, t1_frame: f64) -> f64 {
        let t = tof * self.digitizer_timebase + self.digitizer_delay;
        let c1_eff = self.c1_eff(t1_frame);
        if !(c1_eff > 0.0) || !(t >= self.c0) {
            return f64::NAN;
        }
        let b = 1e6 / c1_eff.sqrt();
        // C2 carries the same temperature factor as C1 (rustims / mzdata; SDK-verified).
        let c2 = if self.c1 != 0.0 { self.c2 / (c1_eff / self.c1) } else { self.c2 };
        let u = if c2 == 0.0 {
            (t - self.c0) / b
        } else {
            let disc = b * b - 4.0 * c2 * (self.c0 - t);
            if !(disc >= 0.0) {
                return f64::NAN;
            }
            let denom = b + disc.sqrt();
            if !(denom > 0.0) {
                return f64::NAN;
            }
            2.0 * (t - self.c0) / denom
        };
        if self.model_type == 2 {
            // C4 repeats C2 in a ModelType-2 row: no shift; the calibrant polynomial instead.
            let m = u * u;
            return m - self.calibrant.map_or(0.0, |c| c.correction(m));
        }
        u * u - self.c4
    }

    /// The reference implementation's 7 parameters of this row at a frame's temperatures
    /// (`TimsTofMzGrid2`, mzdata `MzCalibrationModel2`): `[C0, 1e6/√(C1·cf), C2/cf, C3, C4,
    /// DigitizerTimebase, DigitizerDelay]` with `cf = 1 + (dC1·(T1 − t1_frame) + dC2·(T2 − t2_frame))/1e6`;
    /// a frame without a finite `T1`/`T2` takes the row's own (that term's `cf` contribution is 0).
    /// Evaluated by the reference implementation as [`Self::tof_to_mz`] is (SDK-verified to 1e-9 ppm
    /// on a `C2 ≠ 0`, `C4 ≠ 0` file). A ModelType-2 row yields its quadratic (`C3 = C4 = 0`), which is
    /// the vendor's m/z minus the calibrant correction.
    pub fn grid_parameters(&self, t1_frame: Option<f64>, t2_frame: Option<f64>) -> [f64; 7] {
        let dt1 = t1_frame.filter(|t| t.is_finite()).map_or(0.0, |t| self.t1 - t);
        let dt2 = t2_frame.filter(|t| t.is_finite()).map_or(0.0, |t| self.t2 - t);
        let cf = 1.0 + (self.dc1 * (if dt1.is_finite() { dt1 } else { 0.0 }) + self.dc2 * (if dt2.is_finite() { dt2 } else { 0.0 })) / 1.0e6;
        let beta = (1.0e12 / (self.c1 * cf)).sqrt();
        // A ModelType-2 row's C3/C4 repeat C0/C2; the reference model would read them as a cubic
        // term and an m/z shift (m/z 270 → 21 on SBA415). Its grid is the quadratic; the calibrant
        // polynomial has no place in `MS:9999002` and is declared instead (`mz_model_summary`).
        let (c3, c4) = if self.model_type == 2 { (0.0, 0.0) } else { (self.c3, self.c4) };
        [self.c0, beta, self.c2 / cf, c3, c4, self.digitizer_timebase, self.digitizer_delay]
    }
}

/// Read every `MzCalibration` row of `analysis.tdf` as numbers, keyed by `Id`. The vendor schema
/// declares `C0..C4` untyped and nullable, so NULL (and unparsable text) becomes 0 — the value the
/// vendor library itself sees through `sqlite3_column_double` — while
/// [`TdfMzCalibrationRow::quadratic_terms_stored`] records whether `C2`/`C3`/`C4`/`dC2` were
/// actually stored as numbers (informational since the grid layout: the row is evaluated as the
/// vendor library evaluates it either way).
pub(crate) fn read_mz_calibration_rows(tdf: &Path) -> Result<HashMap<i64, TdfMzCalibrationRow>> {
    let conn = rusqlite::Connection::open_with_flags(tdf, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening {}", tdf.display()))?;
    let mut stmt = conn
        .prepare(
            "SELECT Id, ModelType, DigitizerTimebase, DigitizerDelay, T1, dC1, dC2, C0, C1, C2, C3, C4, T2 \
             FROM MzCalibration ORDER BY Id",
        )
        .context("querying MzCalibration")?;
    let stored = |r: &rusqlite::Row, k: usize| -> Result<bool> {
        Ok(matches!(r.get_ref(k).context("MzCalibration cell")?, ValueRef::Integer(_) | ValueRef::Real(_)))
    };
    let num = |r: &rusqlite::Row, k: usize| -> Result<f64> {
        Ok(match r.get_ref(k).context("MzCalibration cell")? {
            ValueRef::Null => 0.0,
            ValueRef::Integer(i) => i as f64,
            ValueRef::Real(f) => f,
            ValueRef::Text(t) => String::from_utf8_lossy(t).trim().parse::<f64>().unwrap_or(0.0),
            ValueRef::Blob(_) => 0.0,
        })
    };
    let mut out = HashMap::new();
    let mut rows = stmt.query([]).context("reading MzCalibration")?;
    while let Some(r) = rows.next().context("reading MzCalibration")? {
        let id: i64 = r.get(0).context("MzCalibration.Id")?;
        let row = TdfMzCalibrationRow {
            id,
            model_type: num(r, 1)? as i64,
            digitizer_timebase: num(r, 2)?,
            digitizer_delay: num(r, 3)?,
            t1: num(r, 4)?,
            t2: match r.get_ref(12).context("MzCalibration.T2")? {
                ValueRef::Integer(i) => i as f64,
                ValueRef::Real(f) => f,
                _ => f64::NAN,
            },
            dc1: num(r, 5)?,
            dc2: num(r, 6)?,
            c0: num(r, 7)?,
            c1: num(r, 8)?,
            c2: num(r, 9)?,
            c3: num(r, 10)?,
            c4: num(r, 11)?,
            quadratic_terms_stored: stored(r, 6)? && stored(r, 9)? && stored(r, 10)? && stored(r, 11)?,
            calibrant: None,
        };
        out.insert(id, row);
    }
    if out.is_empty() {
        bail!("MzCalibration has no rows");
    }
    drop(rows);
    drop(stmt);
    // The ModelType-2 calibrant polynomial (C5..C14), in a query of its own: a schema without those
    // columns must still yield the rows above.
    if out.values().any(|r| r.model_type == 2) {
        match read_calibrants(&conn) {
            Ok(cal) => {
                for (id, c) in cal {
                    if let Some(row) = out.get_mut(&id).filter(|r| r.model_type == 2) {
                        row.calibrant = Some(c);
                    }
                }
            }
            Err(e) => log::warn!("MzCalibration C5..C14 unreadable ({e:#}); ModelType-2 rows carry no calibrant correction"),
        }
    }
    Ok(out)
}

/// `C5`–`C14` of every `MzCalibration` row that states a usable polynomial (`1 ≤ C7 ≤ 7` numeric
/// coefficients, `C5 < C6`).
fn read_calibrants(conn: &rusqlite::Connection) -> Result<Vec<(i64, Calibrant)>> {
    let mut stmt = conn
        .prepare("SELECT Id, C5, C6, C7, C8, C9, C10, C11, C12, C13, C14 FROM MzCalibration")
        .context("querying MzCalibration C5..C14")?;
    let num = |r: &rusqlite::Row, k: usize| -> Option<f64> {
        match r.get_ref(k).ok()? {
            ValueRef::Integer(i) => Some(i as f64),
            ValueRef::Real(f) => Some(f),
            ValueRef::Text(t) => String::from_utf8_lossy(t).trim().parse::<f64>().ok(),
            _ => None,
        }
    };
    let mut out = Vec::new();
    let mut rows = stmt.query([]).context("reading MzCalibration C5..C14")?;
    while let Some(r) = rows.next().context("reading MzCalibration C5..C14")? {
        let id: i64 = r.get(0).context("MzCalibration.Id")?;
        let (Some(lo), Some(hi), Some(n)) = (num(r, 1), num(r, 2), num(r, 3)) else { continue };
        let n = n as usize;
        if !(lo < hi) || !(1..=7).contains(&n) {
            continue;
        }
        let mut coeffs = [0.0; 7];
        let mut complete = true;
        for (i, c) in coeffs.iter_mut().take(n).enumerate() {
            match num(r, 4 + i) {
                Some(v) if v.is_finite() => *c = v,
                _ => complete = false,
            }
        }
        if complete {
            out.push((id, Calibrant { lo, hi, n, coeffs }));
        }
    }
    Ok(out)
}

/// What the writer states about the fidelity of the m/z the grid rows decode to.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MzModelSummary {
    /// Every frame carries its vendor model exactly (all rows ModelType 1).
    pub exact: bool,
    /// Why not, when not.
    pub note: Option<String>,
    /// The largest difference to the vendor's m/z, in ppm, when it is known.
    pub max_error_ppm: Option<f64>,
    /// The `transformations` entry that declares it, when not exact.
    pub transformation: Option<&'static str>,
}

/// `transformations` entry: a ModelType-2 run's grid rows carry the quadratic, without the vendor's
/// calibrant polynomial (bounded by `ims_calibration.max_error_ppm`).
pub(crate) const CALIBRANT_OMITTED: &str = "bruker:mz-calibrant-omitted";
/// `transformations` entry: m/z on timsrust's two-point chord instead of the vendor model.
pub(crate) const CHORD: &str = "bruker:mz-calibration-chord";

/// [`MzModelSummary`] of a run's calibration rows (the rows [`frame_mz_grid`] resolves frames to).
pub(crate) fn mz_model_summary(rows: &HashMap<i64, TdfMzCalibrationRow>) -> MzModelSummary {
    if rows.is_empty() {
        return MzModelSummary {
            exact: false,
            note: Some("no usable MzCalibration row: every frame carries the two-point chord (chord), an approximation of the vendor model".into()),
            max_error_ppm: None,
            transformation: Some(CHORD),
        };
    }
    let mut unsupported: Vec<i64> = rows.values().filter(|r| !matches!(r.model_type, 1 | 2)).map(|r| r.model_type).collect();
    unsupported.sort_unstable();
    unsupported.dedup();
    if !unsupported.is_empty() {
        return MzModelSummary {
            exact: false,
            note: Some(format!("MzCalibration ModelType {unsupported:?} is not supported: frames on such a row carry the two-point chord (chord)")),
            max_error_ppm: None,
            transformation: Some(CHORD),
        };
    }
    let model2: Vec<&TdfMzCalibrationRow> = rows.values().filter(|r| r.model_type == 2).collect();
    if model2.is_empty() {
        return MzModelSummary { exact: true, note: None, max_error_ppm: None, transformation: None };
    }
    let bound = model2.iter().map(|r| r.calibrant.map(|c| c.max_abs_ppm())).collect::<Option<Vec<f64>>>();
    MzModelSummary {
        exact: false,
        note: Some(
            "MzCalibration ModelType 2: the grid rows carry the quadratic on C0, C1, C2 (C3 = C4 = 0; in these rows C3/C4 repeat C0/C2); \
             the vendor subtracts sum_{i<C7} C[8+i]*m^i from it for C5 <= m <= C6 (the row in vendor_mz_calibration.mz_calibration)"
                .into(),
        ),
        max_error_ppm: bound.map(|b| b.into_iter().fold(0.0, f64::max)),
        transformation: Some(CALIBRANT_OMITTED),
    }
}

/// The `(frame index, tof)` sampling plan of the `MZPC_TDF_SDK_GOLDEN` diagnostic: frame 1, the
/// last frame and 10 evenly spaced frames in between (deduplicated, ascending), × 20 TOF values
/// evenly spread over `0..=num_samples-1` — at most 240 points per run.
// Only the Windows/Linux SDK lane calls it; the plan itself is host-testable.
#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
pub(crate) fn sdk_golden_sample_plan(n_frames: usize, num_samples: i64) -> (Vec<usize>, Vec<f64>) {
    let mut frames: Vec<usize> = Vec::new();
    if n_frames > 0 {
        let last = n_frames - 1;
        frames.push(0);
        for k in 1..=10usize {
            frames.push(((k as f64) * (last as f64) / 11.0).round() as usize);
        }
        frames.push(last);
        frames.sort_unstable();
        frames.dedup();
    }
    let max_tof = (num_samples - 1).max(0) as f64;
    let tofs: Vec<f64> = (0..20).map(|j| (j as f64 * max_tof / 19.0).round()).collect();
    (frames, tofs)
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
    let rows_out = table_rows_json(&conn, "MzCalibration")?;
    let global = global_metadata_json(&conn, &["DigitizerNumSamples", "MzAcqRangeLower", "MzAcqRangeUpper"])?;
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
        "model_type_1": "t_ns = tof*DigitizerTimebase + DigitizerDelay; cf = 1 + dC1*(T1 - tdf_t1)/1e6 (+ dC2*(T2 - tdf_t2)/1e6, dC2 = 0 on every file seen); u = sqrt(mz + C4); t_ns = C0 + (1e6/sqrt(C1*cf))*u + (C2/cf)*u^2, solve for u; mz = u^2 - C4 (C2 = 0: mz = ((t_ns - C0)*sqrt(C1*cf)/1e6)^2 - C4)",
        "model_type_1_verified": "1e-9 ppm vs Bruker timsdata SDK on a C2 != 0, C4 != 0 file (mzdata diaPASEF.d, 2026-09-22); 2.5e-5 ppm on speXtract S30/S08/S23 (C4 = 0); 1e-7 ppm on PXD059079 2485 (C2 = C4 = 0)",
        "model_type_2": "m = the model_type_1 quadratic on C0, C1, C2 with no C4 shift (C3/C4 repeat C0/C2 in these rows); mz = m - sum_{i<C7} C[8+i]*m^i if C5 <= m <= C6, else mz = m",
        "model_type_2_verified": "1e-9 ppm vs Bruker timsdata SDK values on OpenTIMS's test.d (10 points, 8 inside [C5, C6], 2 below; 2026-09-26)",
    }))
}

/// The vendor's scan→1/K0 calibration, verbatim — every `TimsCalibration` row and the nominal
/// acquisition range — with the ModelType-2 expression the ims-compact lane evaluates for
/// `mean_inverse_reduced_ion_mobility` ([`crate::tims_mobility`]), so a reader can go from a stored
/// 1/K0 back to the vendor's scan coordinate without the SDK. Best-effort like the m/z block.
pub fn vendor_tims_calibration(tdf: &Path) -> Result<serde_json::Value> {
    let conn = rusqlite::Connection::open_with_flags(tdf, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening {}", tdf.display()))?;
    let rows_out = table_rows_json(&conn, "TimsCalibration")?;
    let global = global_metadata_json(&conn, &["OneOverK0AcqRangeLower", "OneOverK0AcqRangeUpper"])?;
    Ok(serde_json::json!({
        "source": "analysis.tdf",
        "tims_calibration": rows_out,
        "global_metadata": global,
        "model_type_2": "W = C2 + (C3 - C2)*(scan - C4 - C0)/C1; 1/K0 = W/(C7 + C6*W); scan 0-based (C5, C8, C9 do not enter; C0 = C5 = 1 on every file seen)",
        "model_type_2_verified": "6.7e-16 vs Bruker timsdata SDK tims_scannum_to_oneoverk0 on every scan of seven corpus runs, timsControl 4.0.5-6.2 (2026-09-15); the nominal OneOverK0AcqRange is not the model's value at the first/last scan",
        "applies_to": "mean_inverse_reduced_ion_mobility of every point and the 1/K0 of every precursor/isolation band, unless --no-tims-recalibration (then timsrust's linear map of the nominal range)",
    }))
}

/// Every row of a `.tdf` table as JSON objects keyed by column name (blobs as a size note).
fn table_rows_json(conn: &rusqlite::Connection, table: &str) -> Result<Vec<serde_json::Value>> {
    let mut stmt = conn
        .prepare(&format!("SELECT * FROM {table} ORDER BY Id"))
        .with_context(|| format!("querying {table}"))?;
    let cols: Vec<String> = stmt.column_names().iter().map(|c| c.to_string()).collect();
    let mut rows_out = Vec::new();
    let mut rows = stmt.query([]).with_context(|| format!("reading {table}"))?;
    while let Some(row) = rows.next().with_context(|| format!("reading {table}"))? {
        let mut obj = serde_json::Map::new();
        for (k, name) in cols.iter().enumerate() {
            let v = match row.get_ref(k).with_context(|| format!("{table} cell"))? {
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
        bail!("{table} has no rows");
    }
    Ok(rows_out)
}

/// Selected `GlobalMetadata` keys; the values are TEXT, stored as numbers where they parse.
fn global_metadata_json(conn: &rusqlite::Connection, keys: &[&str]) -> Result<serde_json::Map<String, serde_json::Value>> {
    let mut global = serde_json::Map::new();
    for key in keys {
        let v: Option<String> = conn
            .query_row("SELECT Value FROM GlobalMetadata WHERE Key = ?1", [key], |r| r.get(0))
            .optional()
            .with_context(|| format!("reading GlobalMetadata.{key}"))?;
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
    Ok(global)
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
        // The vendor's m/z calibration rows: the per-frame grid models. A TDF without the table
        // keeps every frame on the chord (as an `MS:1003825` model), logged once.
        let mz_rows = match read_mz_calibration_rows(&tdf) {
            Ok(rows) if !rows.is_empty() => rows,
            Ok(_) => {
                log::warn!("MzCalibration is empty; every frame's grid model is timsrust's two-point chord");
                HashMap::new()
            }
            Err(e) => {
                log::warn!("MzCalibration unreadable ({e}); every frame's grid model is timsrust's two-point chord");
                HashMap::new()
            }
        };
        let tims_grid = recal.as_ref().and_then(|c| GridEncoding::from_parameters(TimsTofTimsLinearGrid2::ACCESSION, &c.grid_parameters()));
        Ok(Self { frames, im: meta.im_converter, recal, model, table, windows, mz_rows, tims_grid, frames_reordered: Default::default() })
    }

    /// What the grid rows' m/z amount to against the vendor's model ([`mz_model_summary`]).
    pub fn mz_model_summary(&self) -> MzModelSummary {
        mz_model_summary(&self.mz_rows)
    }

    /// Whether 1/K0 is stored as TIMS scan numbers under the vendor's ModelType-2 grid (else as
    /// plain values: `--no-tims-recalibration`, or no ModelType-2 row).
    pub fn mobility_grid(&self) -> bool {
        self.tims_grid.is_some()
    }

    /// Frame `i`'s m/z grid model ([`frame_mz_grid`]).
    fn frame_mz_grid(&self, i: usize) -> GridEncoding {
        frame_mz_grid(
            &self.mz_rows,
            self.model,
            self.table.t1.get(i).copied().flatten(),
            self.table.t2.get(i).copied().flatten(),
            self.table.mz_cal_id.get(i).copied().flatten(),
        )
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
                    isolation_window: IsolationWindow::new(
                        w.isolation_mz as f32,
                        w.isolation_mz as f32 - half,
                        w.isolation_mz as f32 + half,
                        IsolationWindowState::Complete,
                    ),
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

    /// Per-frame `T1`/`T2`/`MzCalibration` → spectrum params (absent when the table lacks them):
    /// the inputs of the grid model every row of the frame carries, kept as provenance.
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

    /// Build the IN-ARCHIVE ims-compact spectrum for frame `i` on the reference implementation's
    /// chunk grid: every point's m/z from its TOF bin through the frame's own `MzCalibration` model
    /// and its 1/K0 from its TIMS scan through the vendor's ModelType-2 model, each array carrying
    /// the model as a Param ([`ims_grid_arrays`]); the writer turns them back into the integer bins
    /// and scan numbers (`MS:1003826` rows, `mz_grid` / `mean_inverse_reduced_ion_mobility_grid`).
    /// The WHOLE FRAME is sorted by TOF (== by m/z, monotonic): the chunk rows' index lists are
    /// unsigned deltas. Sorting mixes mobility scans, which is lossless because mobility is stored
    /// per point; frames the sort moved are counted for `sort-by-mz`. Intensity: native counts as
    /// Int32 (byte-plane, lossless) or Float32 under `MZPC_BYTE_PLANE_INTENSITY=0`.
    pub fn ims_grid_spectrum(&self, i: usize, int_intensity: bool) -> Result<MultiLayerSpectrum> {
        let frame = self.frame(i)?;
        let n_scans = frame.scan_offsets.len().saturating_sub(1);
        // Gather every point as (tof_bin, intensity, scan) across all mobility scans, then sort by
        // TOF. A secondary sort by mobility was tried to shrink the scattered 1/K0 column, but it
        // scrambles tof within each chunk and inflates it more than it saves — a net loss (measured
        // g99123: mobility −392 MB, tof +577 MB). So m/z order stays.
        let mut pts: Vec<(i32, u32, u32)> = Vec::with_capacity(frame.tof.len());
        for s in 0..n_scans {
            let (lo, hi) = (frame.scan_offsets[s], frame.scan_offsets[s + 1]);
            for k in lo..hi {
                let bin = i32::try_from(frame.tof[k])
                    .map_err(|_| anyhow::anyhow!("TOF bin {} exceeds i32 range", frame.tof[k]))?;
                pts.push((bin, frame.intensity[k], s as u32));
            }
        }
        if !pts.is_sorted_by_key(|p| p.0) {
            self.frames_reordered.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        pts.sort_by_key(|p| p.0);

        let tof: Vec<i32> = pts.iter().map(|p| p.0).collect();
        let scans: Vec<u32> = pts.iter().map(|p| p.2).collect();
        let (mut intensity_i32, mut intensity_f32) = (Vec::new(), Vec::new());
        for &(_, inten, _) in &pts {
            if int_intensity {
                intensity_i32.push(i32::try_from(inten).map_err(|_| anyhow::anyhow!("intensity {inten} exceeds i32 range"))?);
            } else {
                intensity_f32.push(inten as f32);
            }
        }
        let mz_model = self.frame_mz_grid(i);
        let linear_k0: Vec<f64>;
        let mobility = match self.tims_grid.as_ref() {
            Some(model) => ImsMobility::Scans(&scans, model),
            None => {
                linear_k0 = scans.iter().map(|&s| self.mobility_for_scan(s as usize)).collect();
                ImsMobility::Values(&linear_k0)
            }
        };
        let (arrays, mz) = ims_grid_arrays(
            &tof,
            if int_intensity { ImsIntensity::Counts(&intensity_i32) } else { ImsIntensity::Float(&intensity_f32) },
            mobility,
            &mz_model,
        )?;

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
        // Summary terms from the STORED points — the grid values — stated explicitly, as on every
        // grid lane (the same numbers mzdata would derive from these arrays).
        if let (Some(&lo), Some(&hi)) = (mz.first(), mz.last()) {
            crate::set_observed_mz_range(&mut descr, lo.min(hi), lo.max(hi));
        }
        let (tic, base) = if int_intensity {
            crate::summarize_points(intensity_i32.iter().map(|&v| v as f32), |k| mz[k])
        } else {
            crate::summarize_points(intensity_f32.iter().copied(), |k| mz[k])
        };
        crate::set_spectrum_summary_params(&mut descr, tic, base);
        Ok(MultiLayerSpectrum::new(descr, Some(arrays), None, None))
    }

    /// Frames [`Self::ims_grid_spectrum`] has re-ordered so far (see the field).
    pub fn frames_reordered(&self) -> &std::sync::atomic::AtomicUsize {
        &self.frames_reordered
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
            1.0, 909.0, 211.45198604901222, 73.95258004355563, 32.72727272727273,
            0.00492817555366883, 131.11541877221117,
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
    fn random_access_to_empty_spectrum_does_not_abort() {
        use mzpeak_prototyping::MzPeakReader;
        // The committed centroid fixture's spectrum index 1 (scan=21) has defaultArrayLength 0.
        // Converted to the POINT layout, whose span search was the defect, it gives an archive with
        // a genuinely empty spectrum. The corpus walk this replaces opened about 3 GB of archives
        // looking for one, and never checked that it had found one.
        let input = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tiny_centroid_only.mzML"));
        let dir = std::env::temp_dir().join(format!("mzpc-empty-span-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let archive = dir.join("point.mzpeak");
        crate::convert_file(input, &archive, None, 3, None, true, Some(crate::TofGridMode::Off), &[], None, true, None)
            .expect("convert the fixture to the point layout");
        let mut r = MzPeakReader::new(&archive).expect("open the archive");
        let n = r.len();
        // (found, points) per index. `Ok(None)` would also look empty, so the shape is asserted exactly:
        // the empty spectrum must come back FOUND and empty, and its neighbours with their points — a
        // reader that returned nothing for every index passed the old `empties >= 1`.
        let mut shape = Vec::with_capacity(n);
        for i in 0..n {
            match r.get_spectrum_peaks_for(i as u64) {
                Ok(peaks) => shape.push((peaks.is_some(), peaks.map_or(0, |p| p.len()))),
                Err(e) => panic!("read of spectrum {i} failed: {e}"),
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(n, 3, "the fixture's three spectra");
        assert_eq!(shape[1], (true, 0), "index 1 (scan=21) must be found and empty: {shape:?}");
        assert!(shape[0].1 > 0 && shape[2].1 > 0, "the spectra around it keep their peaks: {shape:?}");
    }
}

#[cfg(test)]
mod grid_model_tests {

    /// The FULL model against the SDK on a file with `C2 ≠ 0`, `C4 ≠ 0` and a real temperature
    /// offset (mzdata's `diaPASEF.d`: C2 = 0.002115, C4 = −0.068565, dC1 = 27, frame T1 − row T1 =
    /// +0.056; `MZPC_TDF_SDK_GOLDEN` on the Flash box, 2026-09-22; 100 points on 5 frames). Without
    /// `− C4` the error is 40–720 ppm; without the temperature scaling of `C2` it is 1e-4 ppm.
    #[test]
    fn full_model_type_1_matches_the_vendor_sdk_on_a_c4_file() {
        let raw = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tdf_diapasef_sdk_golden.json")).unwrap();
        let g: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let rows: Vec<super::TdfMzCalibrationRow> = g["mz_calibration"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                let f = |k: &str| r[k].as_f64().unwrap_or(0.0);
                super::TdfMzCalibrationRow {
                    id: r["Id"].as_i64().unwrap(),
                    model_type: r["ModelType"].as_i64().unwrap(),
                    digitizer_timebase: f("DigitizerTimebase"),
                    digitizer_delay: f("DigitizerDelay"),
                    t1: f("T1"),
                    t2: f("T2"),
                    dc1: f("dC1"),
                    dc2: f("dC2"),
                    c0: f("C0"),
                    c1: f("C1"),
                    c2: f("C2"),
                    c3: f("C3"),
                    c4: f("C4"),
                    quadratic_terms_stored: true,
                    calibrant: None,
                }
            })
            .collect();
        assert!(rows.iter().any(|r| r.c2 != 0.0 && r.c4 != 0.0), "the fixture must exercise C2 and C4");
        let pts = g["points"].as_array().unwrap();
        assert!(pts.len() >= 100, "fixture has {} points", pts.len());
        let (mut worst_ppm, mut worst_grid_ppm): (f64, f64) = (0.0, 0.0);
        for p in pts {
            let sdk = p["mz_sdk"].as_f64().unwrap();
            if !(sdk > 0.0) {
                continue;
            }
            let row = rows.iter().find(|r| r.id == p["cal_id"].as_i64().unwrap()).expect("calibration row for the point");
            let (tof, t1, t2) = (p["tof"].as_f64().unwrap(), p["t1"].as_f64(), p["t2"].as_f64());
            let mz = row.tof_to_mz(tof, t1.unwrap());
            worst_ppm = worst_ppm.max(((mz - sdk) / sdk).abs() * 1e6);
            // The grid rows carry this row as the reference implementation's 7 parameters; its
            // evaluation is the SDK's too, and it inverts the bin exactly (what the writer relies on).
            let grid = GridEncoding::from_parameters(TimsTofMzGrid2::ACCESSION, &row.grid_parameters(t1, t2)).unwrap();
            let via_grid = grid.from_index(tof as u32);
            worst_grid_ppm = worst_grid_ppm.max(((via_grid - sdk) / sdk).abs() * 1e6);
            assert_eq!(grid.to_index(via_grid), tof as u32, "bin {tof} does not invert");
        }
        assert!(worst_ppm < 1e-6, "full ModelType-1 vs SDK: worst {worst_ppm:.2e} ppm");
        assert!(worst_grid_ppm < 1e-6, "grid model vs SDK: worst {worst_grid_ppm:.2e} ppm");
    }

    /// The grid model against Bruker's OWN `tims_index_to_mz` (timsdata SDK on the Flash box,
    /// `MZPC_TDF_SDK_GOLDEN`, 2026-09-03): 240 (frame, tof) points on 12 frames of PXD059079 2485.d,
    /// each with the frame's T1 and calibration id — the reference implementation's evaluation of
    /// the row's 7 parameters reproduces the SDK (the 0.13 per-frame pair did to 1.0e-7 ppm; the
    /// run-wide chord was 4.28 ppm off) — and EVERY digitizer bin of the run inverts to itself
    /// through `to_index(from_index(k))`, which is what lets the writer re-index the grid values
    /// without loss. Corpus-free: everything needed is in the fixture.
    #[test]
    fn grid_model_matches_the_2485_sdk_goldens_and_inverts_every_bin() {
        let raw = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/tdf_2485_sdk_golden.json"
        ))
        .unwrap();
        let g: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let rows: Vec<super::TdfMzCalibrationRow> = g["mz_calibration"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                let f = |k: &str| r[k].as_f64().unwrap_or(0.0);
                super::TdfMzCalibrationRow {
                    id: r["Id"].as_i64().unwrap(),
                    model_type: r["ModelType"].as_i64().unwrap(),
                    digitizer_timebase: f("DigitizerTimebase"),
                    digitizer_delay: f("DigitizerDelay"),
                    t1: f("T1"),
                    t2: f("T2"),
                    dc1: f("dC1"),
                    dc2: f("dC2"),
                    c0: f("C0"),
                    c1: f("C1"),
                    c2: f("C2"),
                    c3: f("C3"),
                    c4: f("C4"),
                    quadratic_terms_stored: true,
                    calibrant: None,
                }
            })
            .collect();
        let pts = g["points"].as_array().unwrap();
        assert!(pts.len() >= 200, "fixture has {} points", pts.len());
        let mut worst_ppm: f64 = 0.0;
        let mut first: Option<super::GridEncoding> = None;
        for p in pts {
            let sdk = p["mz_sdk"].as_f64().unwrap();
            if !(sdk > 0.0) {
                continue;
            }
            let id = p["cal_id"].as_i64().unwrap();
            let row = rows.iter().find(|r| r.id == id).expect("calibration row for the point");
            let grid = super::GridEncoding::from_parameters(
                super::TimsTofMzGrid2::ACCESSION,
                &row.grid_parameters(p["t1"].as_f64(), p["t2"].as_f64()),
            )
            .unwrap();
            let tof = p["tof"].as_f64().unwrap();
            let mz = grid.from_index(tof as u32);
            worst_ppm = worst_ppm.max(((mz - sdk) / sdk).abs() * 1e6);
            first.get_or_insert(grid);
        }
        assert!(worst_ppm < 1e-5, "grid model vs SDK worst {worst_ppm:.3e} ppm (expected ~1e-7)");
        // Every bin of the digitizer range (DigitizerNumSamples = 636,031 on this file) inverts.
        let grid = first.expect("a golden point");
        for k in 0..=636_030u32 {
            assert_eq!(grid.to_index(grid.from_index(k)), k, "bin {k} does not invert through the reference model");
        }
    }
    use super::*;

    /// PXD059079 2485.d's single `MzCalibration` row (ModelType 1, C2 = 0).
    fn row_2485() -> TdfMzCalibrationRow {
        TdfMzCalibrationRow {
            id: 1,
            model_type: 1,
            digitizer_timebase: 0.125,
            digitizer_delay: 26464.125,
            t1: 25.6148127740566,
            t2: 25.1594285616696,
            dc1: 20.0,
            dc2: 0.0,
            c0: 1008.59723408404,
            c1: 154314.98518964,
            c2: 0.0,
            c3: 0.0,
            c4: 0.0,
            quadratic_terms_stored: true,
            calibrant: None,
        }
    }

    /// The row's grid parameters at a frame temperature reproduce the ModelType-1 formula EXACTLY
    /// (1e-12 relative) — i.e. the temperature term is folded into `beta`, not dropped.
    #[test]
    fn grid_parameters_reproduce_model_type_1_at_frame_temperature() {
        let row = row_2485();
        let t1_frame = 25.6193764709235; // Frames.T1 of frame 1: 4.6 mK off the reference
        assert_ne!(t1_frame, row.t1);
        let grid = GridEncoding::from_parameters(TimsTofMzGrid2::ACCESSION, &row.grid_parameters(Some(t1_frame), Some(row.t2))).unwrap();
        for tof in [0u32, 100_000, 300_000, 636_000] {
            let via_grid = grid.from_index(tof);
            let model = row.tof_to_mz(tof as f64, t1_frame);
            assert!(model.is_finite() && model > 0.0, "tof {tof}: model {model}");
            let rel = (via_grid - model).abs() / model;
            assert!(rel < 1e-12, "tof {tof}: grid {via_grid} vs model {model} (rel {rel:e})");
        }
        // The temperature term is live: beta at the reference T1 differs from beta at the frame's.
        let at_ref = row.grid_parameters(Some(row.t1), Some(row.t2))[1];
        let at_frame = row.grid_parameters(Some(t1_frame), Some(row.t2))[1];
        assert!((at_ref - at_frame).abs() / at_frame > 1e-9, "temperature correction must move beta: {at_ref} vs {at_frame}");
        // Sanity: tof 0 near the file's MzAcqRangeLower (~100 Th), the last sample near 1700 Th.
        assert!((99.0..101.0).contains(&grid.from_index(0)), "tof 0 → {}", grid.from_index(0));
        assert!((1690.0..1710.0).contains(&grid.from_index(636_030)), "tof max → {}", grid.from_index(636_030));
    }

    /// The general (quadratic) branch of the formula reproduces all 60 speXtract golden points
    /// (Bruker timsdata SDK `tims_index_to_mz` on three diaPASEF runs) to < 1e-4 ppm — and those
    /// rows (C2 ≠ 0) are correctly refused as sqrt-linear.
    #[test]
    fn quadratic_branch_matches_the_sdk_goldens() {
        let text = include_str!("../tests/fixtures/tdf_calibration_golden.json");
        let files: Vec<serde_json::Value> = serde_json::from_str(text).unwrap();
        assert_eq!(files.len(), 3);
        let mut n = 0usize;
        let mut worst = 0.0f64;
        for f in &files {
            let row = TdfMzCalibrationRow {
                id: 1,
                model_type: f["model_type"].as_i64().unwrap(),
                digitizer_timebase: f["timebase"].as_f64().unwrap(),
                digitizer_delay: f["delay"].as_f64().unwrap(),
                t1: f["T1_ref"].as_f64().unwrap(),
                t2: f64::NAN,
                dc1: f["dC1"].as_f64().unwrap(),
                dc2: 0.0,
                c0: f["C0"].as_f64().unwrap(),
                c1: f["C1"].as_f64().unwrap(),
                c2: f["C2"].as_f64().unwrap(),
                c3: 0.0,
                c4: 0.0,
                quadratic_terms_stored: true,
                calibrant: None,
            };
            assert!(row.c2 != 0.0, "{}: a C2 ≠ 0 row", f["file"]);
            for c in f["cases"].as_array().unwrap() {
                let (t1, tof, mz_sdk) =
                    (c["t1"].as_f64().unwrap(), c["tof"].as_f64().unwrap(), c["mz"].as_f64().unwrap());
                let mz = row.tof_to_mz(tof, t1);
                let ppm = (mz - mz_sdk).abs() / mz_sdk * 1e6;
                worst = worst.max(ppm);
                assert!(ppm < 1e-4, "{} frame {} tof {tof}: {mz} vs SDK {mz_sdk} ({ppm:e} ppm)", f["file"], c["frame"]);
                // … and the reference implementation's quadratic branch agrees with the formula at
                // the nearest integer bin (the goldens sample fractional bins), inverted exactly.
                let grid = GridEncoding::from_parameters(TimsTofMzGrid2::ACCESSION, &row.grid_parameters(Some(t1), None)).unwrap();
                let k = tof.round() as u32;
                let (via_grid, model) = (grid.from_index(k), row.tof_to_mz(k as f64, t1));
                assert!((via_grid - model).abs() / model < 1e-12, "{} frame {} bin {k}: grid {via_grid} vs model {model}", f["file"], c["frame"]);
                assert_eq!(grid.to_index(via_grid), k);
                n += 1;
            }
        }
        assert_eq!(n, 60, "all 60 golden points exercised");
        eprintln!("worst golden deviation: {worst:e} ppm");
    }

    /// Out-of-model inputs give NaN, never a plausible m/z.
    #[test]
    fn out_of_model_inputs_are_nan() {
        let row = row_2485();
        // t_ns < C0 (arrival before the calibration zero) — impossible with this delay, so force it.
        let mut early = row;
        early.digitizer_delay = 0.0;
        assert!(early.tof_to_mz(0.0, row.t1).is_nan());
        let mut bad_c1 = row;
        bad_c1.c1 = -1.0;
        assert!(bad_c1.tof_to_mz(1.0e5, row.t1).is_nan());
    }

    /// A ModelType-2 row from the golden fixture, read the way `read_mz_calibration_rows` reads one.
    fn model_type_2_golden() -> (TdfMzCalibrationRow, serde_json::Value) {
        let g: serde_json::Value = serde_json::from_str(include_str!("../tests/fixtures/tdf_modeltype2_sdk_golden.json")).unwrap();
        let r = &g["mz_calibration"][0];
        let f = |k: &str| r[k].as_f64().unwrap();
        let mut coeffs = [0.0; 7];
        for (i, c) in coeffs.iter_mut().enumerate() {
            *c = f(&format!("C{}", 8 + i));
        }
        let row = TdfMzCalibrationRow {
            id: r["Id"].as_i64().unwrap(),
            model_type: r["ModelType"].as_i64().unwrap(),
            digitizer_timebase: f("DigitizerTimebase"),
            digitizer_delay: f("DigitizerDelay"),
            t1: f("T1"),
            t2: f("T2"),
            dc1: f("dC1"),
            dc2: f("dC2"),
            c0: f("C0"),
            c1: f("C1"),
            c2: f("C2"),
            c3: f("C3"),
            c4: f("C4"),
            quadratic_terms_stored: true,
            calibrant: Some(Calibrant { lo: f("C5"), hi: f("C6"), n: f("C7") as usize, coeffs }),
        };
        (row, g)
    }

    /// ModelType 2 against Bruker's own library (OpenTIMS's `test.d`, ten `tims_index_to_mz`
    /// values, eight inside the calibrant range and two below it): the quadratic on C0, C1, C2 minus
    /// the calibrant polynomial inside [C5, C6] and nothing outside reproduces every point to 1e-6 ppm.
    /// Reading C3/C4 as the ModelType-1 cubic and shift — what mzdata 0.67.1 and 0.13.0 did — is off
    /// by an order of magnitude.
    #[test]
    fn model_type_2_formula_matches_the_vendor_sdk() {
        let (row, g) = model_type_2_golden();
        assert_eq!(row.model_type, 2);
        assert_eq!((row.c3, row.c4), (row.c0, row.c2), "the fixture's C3/C4 repeat C0/C2");
        let t1_of = |frame: i64| g["frames"].as_array().unwrap().iter().find(|x| x["frame"] == frame).unwrap()["t1"].as_f64().unwrap();
        let cal = row.calibrant.unwrap();
        let (mut worst, mut inside, mut outside) = (0.0f64, 0, 0);
        for p in g["points"].as_array().unwrap() {
            let (tof, sdk, t1) = (p["tof"].as_f64().unwrap(), p["mz_sdk"].as_f64().unwrap(), t1_of(p["frame"].as_i64().unwrap()));
            let mz = row.tof_to_mz(tof, t1);
            worst = worst.max(((mz - sdk) / sdk).abs() * 1e6);
            if (cal.lo..=cal.hi).contains(&sdk) { inside += 1 } else { outside += 1 }
            // As ModelType 1 (C3 cubic, C4 shift): the reference model evaluates the row verbatim.
            let verbatim = [row.c0, row.grid_parameters(Some(t1), None)[1], row.c2, row.c3, row.c4, row.digitizer_timebase, row.digitizer_delay];
            let wrong = GridEncoding::from_parameters(TimsTofMzGrid2::ACCESSION, &verbatim).unwrap().from_index(tof as u32);
            assert!(wrong < 0.2 * sdk, "the ModelType-1 reading must be the catastrophic one: {wrong} vs {sdk}");
        }
        assert!(worst < 1e-6, "ModelType-2 formula vs SDK: worst {worst:.2e} ppm");
        assert!(inside >= 5 && outside >= 2, "the golden must exercise both sides of C5: {inside} inside, {outside} outside");
    }

    /// What the archive stores for a ModelType-2 row: the reference model's quadratic (C3 = C4 = 0),
    /// every digitizer bin inverting exactly, and m/z within the declared bound of the SDK — exactly
    /// the SDK outside the calibrant range, where the vendor applies no correction.
    #[test]
    fn model_type_2_grid_is_the_quadratic_within_the_declared_bound() {
        let (row, g) = model_type_2_golden();
        let rows: HashMap<i64, TdfMzCalibrationRow> = [(row.id, row)].into_iter().collect();
        let chord = TofMzModel { a: 7.0, b: 8.0e-5 };
        let summary = mz_model_summary(&rows);
        assert!(!summary.exact, "a ModelType-2 run is not exact on the reference model");
        let bound = summary.max_error_ppm.expect("the calibrant correction is bounded");
        assert!(bound > 0.0 && bound < 10.0, "bound {bound} ppm");
        assert!(summary.note.as_deref().is_some_and(|n| n.contains("ModelType 2")), "{summary:?}");
        let cal = row.calibrant.unwrap();
        for p in g["points"].as_array().unwrap() {
            let frame = &g["frames"].as_array().unwrap()[p["frame"].as_u64().unwrap() as usize - 1];
            let grid = frame_mz_grid(&rows, chord, frame["t1"].as_f64(), frame["t2"].as_f64(), frame["cal_id"].as_i64());
            assert_eq!(grid.grid_type(), TimsTofMzGrid2::ACCESSION);
            let params = grid.parameters();
            assert_eq!((params[3], params[4]), (0.0, 0.0), "C3/C4 must not reach the reference model");
            let (k, sdk) = (p["tof"].as_u64().unwrap() as u32, p["mz_sdk"].as_f64().unwrap());
            let mz = grid.from_index(k);
            assert_eq!(grid.to_index(mz), k, "bin {k} must invert");
            let ppm = ((mz - sdk) / sdk).abs() * 1e6;
            assert!(ppm <= bound + 1e-6, "bin {k}: {ppm} ppm exceeds the declared {bound} ppm");
            if !(cal.lo..=cal.hi).contains(&mz) {
                assert!(ppm < 1e-6, "outside the calibrant range the quadratic IS the vendor value: {ppm} ppm");
            }
        }
        // An unknown model type leaves the frame on the chord, and says so.
        let mut other = row;
        other.model_type = 3;
        let rows3: HashMap<i64, TdfMzCalibrationRow> = [(other.id, other)].into_iter().collect();
        let g3 = frame_mz_grid(&rows3, chord, None, None, Some(other.id));
        assert_eq!(g3.grid_type(), SquareRootLinearGrid::ACCESSION);
        let s3 = mz_model_summary(&rows3);
        assert!(!s3.exact && s3.note.as_deref().is_some_and(|n| n.contains("ModelType [3]")), "{s3:?}");
        // ModelType 1 alone is exact.
        assert_eq!(mz_model_summary(&[(1, row_2485())].into_iter().collect()), MzModelSummary { exact: true, note: None, max_error_ppm: None, transformation: None });
        assert_eq!(summary.transformation, Some(CALIBRANT_OMITTED));
        assert_eq!(s3.transformation, Some(CHORD));
    }

    /// `read_mz_calibration_rows` reads a ModelType-2 row's calibrant columns, and a TDF whose table
    /// lacks C5..C14 still yields its rows (no calibrant).
    #[test]
    fn calibrant_columns_are_read_and_optional() {
        let (golden, _) = model_type_2_golden();
        let dir = std::env::temp_dir().join(format!("mzpc-mt2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tdf = dir.join("analysis.tdf");
        let _ = std::fs::remove_file(&tdf);
        let conn = rusqlite::Connection::open(&tdf).unwrap();
        let c = golden.calibrant.unwrap();
        conn.execute_batch(&format!(
            "CREATE TABLE MzCalibration (Id INTEGER PRIMARY KEY, ModelType INTEGER, DigitizerTimebase REAL, DigitizerDelay REAL, \
             T1 REAL, T2 REAL, dC1 REAL, dC2 REAL, C0, C1, C2, C3, C4, C5, C6, C7, C8, C9, C10, C11, C12, C13, C14); \
             INSERT INTO MzCalibration VALUES (1, 2, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {});",
            golden.digitizer_timebase, golden.digitizer_delay, golden.t1, golden.t2, golden.dc1, golden.dc2,
            golden.c0, golden.c1, golden.c2, golden.c3, golden.c4, c.lo, c.hi, c.n,
            c.coeffs[0], c.coeffs[1], c.coeffs[2], c.coeffs[3], c.coeffs[4], c.coeffs[5], c.coeffs[6]
        ))
        .unwrap();
        drop(conn);
        let rows = read_mz_calibration_rows(&tdf).unwrap();
        assert_eq!(rows[&1].calibrant, Some(c));
        assert_eq!(rows[&1].model_type, 2);
        let _ = std::fs::remove_file(&tdf);
        let conn = rusqlite::Connection::open(&tdf).unwrap();
        conn.execute_batch(
            "CREATE TABLE MzCalibration (Id INTEGER PRIMARY KEY, ModelType INTEGER, DigitizerTimebase REAL, DigitizerDelay REAL, \
             T1 REAL, T2 REAL, dC1 REAL, dC2 REAL, C0, C1, C2, C3, C4); \
             INSERT INTO MzCalibration VALUES (1, 2, 0.2, 26001.6, 25.1, 22.5, 27.0, 0.0, 315.78, 151558.2, -0.00045, 315.78, -0.00045);",
        )
        .unwrap();
        drop(conn);
        let rows = read_mz_calibration_rows(&tdf).unwrap();
        assert_eq!((rows[&1].model_type, rows[&1].calibrant), (2, None), "no C5..C14 columns: the row still reads");
        assert!(!mz_model_summary(&rows).exact);
        assert_eq!(mz_model_summary(&rows).max_error_ppm, None, "no calibrant: no bound to state");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Per-frame resolution of the grid model: the row named by `Frames.MzCalibration`, else the
    /// lowest-id row (a NULL or unknown id), at the frame's temperatures — a frame without a finite
    /// `T1` takes the row's own; a TDF without rows falls back to the chord as an `MS:1003825`
    /// model. Every frame gets a model.
    #[test]
    fn frame_mz_grid_resolves_rows_and_falls_back_to_the_chord() {
        let lin = row_2485();
        let mut quad = lin;
        quad.id = 2;
        quad.c2 = 1.26e-3;
        let chord = TofMzModel { a: 10.0, b: 2.0e-5 };
        let two: HashMap<i64, TdfMzCalibrationRow> = [(1, lin), (2, quad)].into_iter().collect();

        let by_id = frame_mz_grid(&two, chord, Some(25.62), Some(25.16), Some(2));
        assert_eq!(by_id.grid_type(), TimsTofMzGrid2::ACCESSION);
        assert_eq!(by_id.parameters(), quad.grid_parameters(Some(25.62), Some(25.16)).to_vec(), "the row the frame names");
        for id in [None, Some(7)] {
            let g = frame_mz_grid(&two, chord, Some(25.62), Some(25.16), id);
            assert_eq!(g.parameters(), lin.grid_parameters(Some(25.62), Some(25.16)).to_vec(), "{id:?} → the lowest-id row");
        }
        // NULL / non-finite T1: the row's own temperature (cf = 1), never a 0 K substitution.
        for t1 in [None, Some(f64::NAN)] {
            let g = frame_mz_grid(&two, chord, t1, None, Some(1));
            assert_eq!(g.parameters(), lin.grid_parameters(Some(lin.t1), Some(lin.t2)).to_vec(), "{t1:?}");
        }
        // No rows at all: the chord as a sqrt model.
        let g = frame_mz_grid(&HashMap::new(), chord, Some(25.62), None, Some(1));
        assert_eq!(g.grid_type(), SquareRootLinearGrid::ACCESSION);
        assert_eq!(&g.parameters()[..2], &[10.0, 2.0e-5], "intercept, slope (the scale 1 is implied)");
        assert_eq!(g.from_index(100_000), (10.0f64 + 2.0e-5 * 100_000.0).powi(2));
    }

    /// `read_mz_calibration_rows` on a synthetic TDF with the vendor's untyped `C0..C4`: NULL / text
    /// `C2`/`C3`/`C4`/`dC2` read as 0 (the vendor library's own `sqlite3_column_double` semantics),
    /// `T2` is read, and the frame table's `(T1, T2, MzCalibration)` select the row at the frame's
    /// temperatures — evaluated as the ModelType-1 formula to 1e-12.
    #[test]
    fn calibration_rows_read_null_terms_as_zero_and_t2() {
        let dir = std::env::temp_dir().join(format!("mzpc-exact-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tdf = dir.join("analysis.tdf");
        let _ = std::fs::remove_file(&tdf);
        let conn = rusqlite::Connection::open(&tdf).unwrap();
        conn.execute_batch(
            "CREATE TABLE MzCalibration (Id INTEGER PRIMARY KEY, ModelType INTEGER, DigitizerTimebase REAL, \
             DigitizerDelay REAL, T1 REAL, T2 REAL, dC1 REAL, dC2, C0, C1, C2, C3, C4); \
             INSERT INTO MzCalibration VALUES (1, 1, 0.125, 26464.125, 25.6148127740566, 25.1594285616696, \
             20.0, NULL, 1008.59723408404, 154314.98518964, NULL, NULL, '0'); \
             CREATE TABLE Frames (Id INTEGER PRIMARY KEY, NumPeaks INTEGER, Time REAL, MsMsType INTEGER, \
             Polarity TEXT, T1 REAL, T2 REAL, MzCalibration INTEGER); \
             INSERT INTO Frames VALUES (1, 10, 0.5, 0, '+', 25.61, 25.16, 1); \
             INSERT INTO Frames VALUES (2, 10, 0.6, 9, '+', NULL, 25.16, 1); \
             INSERT INTO Frames VALUES (3, 10, 0.7, 0, '+', 25.63, 25.17, 1);",
        )
        .unwrap();
        drop(conn);
        let rows = read_mz_calibration_rows(&tdf).unwrap();
        let row = rows[&1];
        assert_eq!((row.c2, row.c3, row.c4, row.dc2), (0.0, 0.0, 0.0, 0.0), "NULL/text read as 0 for evaluation");
        assert_eq!((row.c1, row.t2), (154314.98518964, 25.1594285616696));
        assert!(!row.quadratic_terms_stored);
        assert!(row.tof_to_mz(1.0e5, 25.61).is_finite(), "the model still evaluates with NULL → 0");

        let table = read_frame_table(&tdf).unwrap();
        let chord = TofMzModel { a: 10.0, b: 2.0e-5 };
        for i in 0..3 {
            let g = frame_mz_grid(&rows, chord, table.t1[i], table.t2[i], table.mz_cal_id[i]);
            assert_eq!(g.grid_type(), TimsTofMzGrid2::ACCESSION);
            let t1 = table.t1[i].unwrap_or(row.t1);
            let (via_grid, model) = (g.from_index(100_000), row.tof_to_mz(1.0e5, t1));
            assert!((via_grid - model).abs() / model < 1e-12, "frame {i}: grid {via_grid} vs model {model}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The golden sampling plan: ≤ 12 frames × 20 tof values, first/last frame and tof 0 / N−1
    /// always included, monotonic, and degenerate runs do not panic.
    #[test]
    fn golden_sample_plan_shape() {
        let (frames, tofs) = sdk_golden_sample_plan(3994, 636031);
        assert_eq!(frames.len(), 12);
        assert_eq!((frames[0], *frames.last().unwrap()), (0, 3993));
        assert!(frames.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(tofs.len(), 20);
        assert_eq!((tofs[0], tofs[19]), (0.0, 636030.0));
        assert!(tofs.windows(2).all(|w| w[0] < w[1]));
        assert!(frames.len() * tofs.len() <= 240);
        let (frames, _) = sdk_golden_sample_plan(1, 10);
        assert_eq!(frames, vec![0]);
        let (frames, tofs) = sdk_golden_sample_plan(0, 0);
        assert!(frames.is_empty());
        assert!(tofs.iter().all(|&t| t == 0.0));
    }
}
