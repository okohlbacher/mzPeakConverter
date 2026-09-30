//! Native Waters MassLynx `.raw` reader via the vendor **C API** (`MassLynxRaw.dll`), loaded with
//! `libloading`.
//!
//! WHY NOT A C# GLUE: unlike SciEX Clearcore2 / Agilent MHDAC (managed .NET, reached by reflection),
//! `MassLynxRaw.dll` is a **native C++ library exporting a C interface** — reflection can't touch it.
//! So this binds the C exports directly, the same pattern as the Bruker timsdata reader.
//!
//! The C API, as VERIFIED on the workstation against ProteoWizard's output for the same file
//! (`20181203_Capan2_1.raw`, probe rounds 10–13 of 2026-09-09; Waters publishes no header and the
//! shapes are not what the C++ wrapper suggests):
//!   * `createRawReaderFromPath(path, &reader, type)` — `type`: SCAN=1, INFO=2 (CHROM=3, ANALOG=4).
//!   * `getFunctionCount(info, &n)` / `getScanCount(info, func, &n)` / `isContinuum(info, func, &bool)`.
//!   * `getFunctionType(info, func, &code)` / `getIonMode(info, func, &code)` return CODES (218, 108
//!     on a Synapt), and `getFunctionTypeString(info, code, &str)` / `getIonModeString(info, code,
//!     &str)` translate a CODE ("TOF MS", "ES+") — handed a function index they fail (rc 27/28).
//!   * `getAcquisitionMassRange(info, func, which, &lo, &hi)` — FIVE arguments; the four-argument form
//!     returns rc 15. `which = 0` gives the function's range (50–600 = pwiz's scan window).
//!   * `getRetentionTime(info, func, scan, &minutes)` (0.03705 min = pwiz's first f1 scan).
//!   * `getDriftScanCount(info, func, &bins)` — 200 on every HDMSe function.
//!   * `getDriftTime(info, bin, &ms)` — NO function argument: the table is per run. Every four- or
//!     five-argument spelling access-violates (an integer lands where the out-pointer is expected).
//!     Returns pwiz's table exactly: 0.0, 0.0392495, 0.0784991, …, 7.81066 ms.
//!   * `readScan(scan, func, scan, &masses, &intensities, &n)` — the DRIFT-SUMMED spectrum;
//!     `readDriftScan(scan, func, scan, bin, &masses, &intensities, &n)` — ONE drift bin of it.
//!     Both hand back READER-OWNED `float[n]` buffers valid until the next read; copy, never free
//!     (`releaseMemory` on them corrupts the heap, 0xC0000374).
//!   * `destroyRawReader(reader)`. All return `int` (0 = OK).
//!   * Scan items go through a MassLynx "parameters" object (`createParameters` / `getParameterValue`
//!     / `getParameterKeys` / `destroyParameters`): `getScanItemsInFunction(info, f, params)` lists the
//!     ids as keys, `getScanItemName(info, ids, n, params)` and `getScanItemValue(info, f, scan, ids,
//!     n, params)` fill strings per id. Every direct-out spelling crashed (probe rounds 12–16); the
//!     shapes come from the public MassLynx SDK bindings. `getLockMassFunction(info, &char, &int)`.
//!
//! ION MOBILITY (HDMSe / HDDDA): a function whose `_funcNNN.cdt` exists and whose `getDriftScanCount`
//! is > 0 is read bin by bin and written as ONE spectrum per MassLynx scan — a frame — whose points are
//! sorted by (m/z, drift time) and carry a per-point `raw ion mobility array` (MS:1003007, ms). That
//! is the shape ProteoWizard's own `--combineIonMobilitySpectra` output takes and the shape the Bruker
//! ims-compact lane uses; before 2026-09-09 the lane wrote the drift-SUMMED `readScan` spectrum and the
//! dimension was lost (Capan2: 1,989 summed scans vs pwiz's 397,800 per-bin spectra = 1,989 × 200).
//!
//! RUNTIME: Windows + the MassLynx DLLs (`MassLynxRaw.dll` + deps `cdt.dll`, … — bundled in a
//! ProteoWizard install). Point `MZPC_MASSLYNX_DIR` (or `MZPC_PWIZ_DIR`) at that directory. We
//! prepend it to `PATH` before loading so `MassLynxRaw.dll`'s *dependency* DLLs (notably `cdt.dll`,
//! the compressed-scan decoder that `readScan` needs) resolve — otherwise data reads access-violate
//! even though the reader opens fine.

use std::ffi::{CStr, CString, OsString, c_char, c_int, c_void};
use std::path::{Path, PathBuf};
use std::ptr;

use anyhow::{Context, Result, anyhow, bail};
use libloading::Library;

use mzdata::meta::DissociationMethodTerm;
use mzdata::params::{Param, ParamDescribed, Unit};
use mzdata::spectrum::bindata::{ArrayType, BinaryArrayMap, BinaryDataArrayType, DataArray};
use mzdata::spectrum::{
    Activation, IsolationWindow, IsolationWindowState, MultiLayerSpectrum, Precursor, ScanEvent,
    ScanPolarity, ScanWindow, SelectedIon, SignalContinuity, SpectrumDescription,
};

/// Guard against a corrupt/hostile vendor library returning an enormous length that would exhaust
/// memory before we copy it. 100M points × (8 + 4) bytes ≈ 1.2 GiB.
const MAX_WATERS_SPECTRUM_POINTS: c_int = 100_000_000;
/// A drift dimension larger than this is not a TWIMS/SONAR bin count but a wrong signature.
const MAX_DRIFT_BINS: c_int = 4096;

const ML_TYPE_SCAN: c_int = 1;
const ML_TYPE_INFO: c_int = 2;

// MassLynx C exports. On x86_64 Windows there is a single calling convention, so `extern "C"` is the
// correct (and only) ABI. Reader handles are opaque `void*`.
type CreateFromPathFn = unsafe extern "C" fn(*const c_char, *mut *mut c_void, c_int) -> c_int;
type DestroyReaderFn = unsafe extern "C" fn(*mut c_void) -> c_int;
type GetFunctionCountFn = unsafe extern "C" fn(*mut c_void, *mut c_int) -> c_int;
/// `getScanCount` / `getDriftScanCount` / `getFunctionType` / `getIonMode`: `(info, function, *out int)`.
type GetIntPerFunctionFn = unsafe extern "C" fn(*mut c_void, c_int, *mut c_int) -> c_int;
/// `isContinuum(info, function, *out bool)`.
type IsContinuumFn = unsafe extern "C" fn(*mut c_void, c_int, *mut bool) -> c_int;
/// `getRetentionTime(info, function, scan, *out float minutes)`.
type GetRetentionTimeFn = unsafe extern "C" fn(*mut c_void, c_int, c_int, *mut f32) -> c_int;
/// `getDriftTime(info, bin, *out float ms)` — see the module docs: no function argument.
type GetDriftTimeFn = unsafe extern "C" fn(*mut c_void, c_int, *mut f32) -> c_int;
/// `getAcquisitionMassRange(info, function, which, *out float lo, *out float hi)`.
type GetMassRangeFn = unsafe extern "C" fn(*mut c_void, c_int, c_int, *mut f32, *mut f32) -> c_int;
/// `getFunctionTypeString` / `getIonModeString`: `(info, CODE, *out char*)`.
type CodeToStringFn = unsafe extern "C" fn(*mut c_void, c_int, *mut *const c_char) -> c_int;
type ReadScanFn =
    unsafe extern "C" fn(*mut c_void, c_int, c_int, *mut *mut f32, *mut *mut f32, *mut c_int)
        -> c_int;
type ReadDriftScanFn = unsafe extern "C" fn(
    *mut c_void,
    c_int,
    c_int,
    c_int,
    *mut *mut f32,
    *mut *mut f32,
    *mut c_int,
) -> c_int;

// The scan-item family returns its results through a MassLynx "parameters" object (the shapes the
// public MassLynx SDK bindings declare: getScanItemsInFunction(info, f, params) → item ids as the
// parameter KEYS; getScanItemValue(info, f, scan, ids, n, params) / getScanItemName(info, ids, n,
// params) → strings per id). Every earlier direct-out spelling access-violated (probe rounds 12–16).
type CreateParametersFn = unsafe extern "C" fn(*mut *mut c_void) -> c_int;
type DestroyParametersFn = unsafe extern "C" fn(*mut c_void) -> c_int;
type GetParameterValueFn = unsafe extern "C" fn(*mut c_void, c_int, *mut *const c_char) -> c_int;
type GetParameterKeysFn = unsafe extern "C" fn(*mut c_void, *mut *const c_int, *mut c_int) -> c_int;
type ScanItemsInFunctionFn = unsafe extern "C" fn(*mut c_void, c_int, *mut c_void) -> c_int;
type ScanItemValueFn = unsafe extern "C" fn(*mut c_void, c_int, c_int, *const c_int, c_int, *mut c_void) -> c_int;
type ScanItemNameFn = unsafe extern "C" fn(*mut c_void, *const c_int, c_int, *mut c_void) -> c_int;
/// `getLockMassFunction(info, *out char hasLockMass, *out int whichFunction)`.
type LockMassFunctionFn = unsafe extern "C" fn(*mut c_void, *mut c_char, *mut c_int) -> c_int;

/// MassLynxScanItem ids used only when the DLL's own item names cannot be read: the table this DLL
/// hands back (measured on the box) starts at 401 = LINEAR_DETECTOR_VOLTAGE, so SET_MASS = 477,
/// COLLISION_ENERGY = 462, SONAR_ENABLED = 481.
const SCAN_ITEM_FIRST: c_int = 401;

/// The scan-item API, resolved as a whole (any missing export disables all of it).
#[derive(Clone, Copy)]
struct ScanItemApi {
    create: CreateParametersFn,
    destroy: DestroyParametersFn,
    value: GetParameterValueFn,
    keys: GetParameterKeysFn,
    in_function: ScanItemsInFunctionFn,
    item_value: ScanItemValueFn,
    item_name: ScanItemNameFn,
}

impl ScanItemApi {
    fn resolve(lib: &Library) -> Option<Self> {
        unsafe {
            Some(ScanItemApi {
                create: *lib.get::<CreateParametersFn>(b"createParameters\0").ok()?,
                destroy: *lib.get::<DestroyParametersFn>(b"destroyParameters\0").ok()?,
                value: *lib.get::<GetParameterValueFn>(b"getParameterValue\0").ok()?,
                keys: *lib.get::<GetParameterKeysFn>(b"getParameterKeys\0").ok()?,
                in_function: *lib.get::<ScanItemsInFunctionFn>(b"getScanItemsInFunction\0").ok()?,
                item_value: *lib.get::<ScanItemValueFn>(b"getScanItemValue\0").ok()?,
                item_name: *lib.get::<ScanItemNameFn>(b"getScanItemName\0").ok()?,
            })
        }
    }

    /// Run `fill` against a fresh parameters object, read what `read` wants, destroy it.
    fn with<T>(&self, fill: impl FnOnce(*mut c_void) -> c_int, read: impl FnOnce(*mut c_void) -> T) -> Option<T> {
        let mut p: *mut c_void = ptr::null_mut();
        if unsafe { (self.create)(&mut p) } != 0 || p.is_null() {
            return None;
        }
        let out = if fill(p) == 0 { Some(read(p)) } else { None };
        unsafe { (self.destroy)(p) };
        out
    }

    fn string(&self, p: *mut c_void, key: c_int) -> Option<String> {
        let mut v: *const c_char = ptr::null();
        (unsafe { (self.value)(p, key, &mut v) } == 0 && !v.is_null())
            .then(|| unsafe { CStr::from_ptr(v) }.to_string_lossy().trim().to_string())
    }

    /// The item ids a function records.
    fn available(&self, info: *mut c_void, f: c_int) -> Vec<c_int> {
        self.with(
            |p| unsafe { (self.in_function)(info, f, p) },
            |p| {
                let mut keys: *const c_int = ptr::null();
                let mut n: c_int = 0;
                if unsafe { (self.keys)(p, &mut keys, &mut n) } == 0 && !keys.is_null() && (0..=4096).contains(&n) {
                    unsafe { std::slice::from_raw_parts(keys, n as usize) }.to_vec()
                } else {
                    Vec::new()
                }
            },
        )
        .unwrap_or_default()
    }

    fn names(&self, info: *mut c_void, ids: &[c_int]) -> Vec<(c_int, String)> {
        if ids.is_empty() {
            return Vec::new();
        }
        self.with(
            |p| unsafe { (self.item_name)(info, ids.as_ptr(), ids.len() as c_int, p) },
            |p| ids.iter().filter_map(|&id| self.string(p, id).map(|n| (id, n))).collect(),
        )
        .unwrap_or_default()
    }

    fn values(&self, info: *mut c_void, f: c_int, scan: c_int, ids: &[c_int]) -> Vec<Option<String>> {
        if ids.is_empty() {
            return Vec::new();
        }
        self.with(
            |p| unsafe { (self.item_value)(info, f, scan, ids.as_ptr(), ids.len() as c_int, p) },
            |p| ids.iter().map(|&id| self.string(p, id)).collect(),
        )
        .unwrap_or_else(|| vec![None; ids.len()])
    }
}

/// The scan items this lane reads, resolved by NAME from the DLL's own table (falling back to the
/// SDK enum constants when the names cannot be read).
#[derive(Debug, Clone, Copy, Default)]
struct ScanItemIds {
    set_mass: Option<c_int>,
    collision_energy: Option<c_int>,
    sonar: Option<c_int>,
}

/// `transformations` entry: pixel positions are grid indices fitted to the laser aim positions (mm).
pub const LASER_GRID: &str = "waters:laser-position-fitted-to-grid";
/// `transformations` entry: a few scans (at most [`crate::imaging::MAX_OFF_GRID`] of those with a
/// position) lie off the grid and were written without a pixel; the count is in `waters_imaging`.
pub const OFF_GRID_DROPPED: &str = "waters:off-grid-position-dropped";

/// A Waters imaging run (MALDI or DESI): the grid fitted to every scan's laser aim position, and each
/// spectrum's pixel. MassLynx states positions in mm, not pixel indices, so the step, origin and
/// count are fitted ([`crate::imaging::fit_axis`]), the step the method declares preferred. Several
/// scans may share a pixel (functions a long raster is split into keep their own scan numbers; the
/// pixel comes from the position). When the positions fit no grid the run keeps the reason and
/// writes no positions and no marker, only the `waters_imaging` block (review 2026-09-30 B14).
#[derive(Debug, Clone)]
pub struct WatersImaging {
    grid: Result<LaserGrid, String>,
    /// The item names MassLynx gave for x and y.
    names: (String, String),
    /// The step each axis's acquisition declares: `methodfile.xml` setting name and value (mm).
    steps: [Option<(String, f64)>; 2],
    /// Scans stating a laser position (the lock-mass function's excluded).
    positioned: usize,
    /// Scans of the lock-mass (reference) function: no pixel.
    lockmass_scans: usize,
}

/// The grid the laser positions fit.
#[derive(Debug, Clone)]
struct LaserGrid {
    x: crate::imaging::GridAxis,
    y: crate::imaging::GridAxis,
    /// Per spectrum index: its pixel, when its scan states a position on the grid.
    positions: Vec<Option<(i64, i64)>>,
    /// Scans whose position lies off the grid, written without a pixel.
    off_grid: usize,
}

/// Fit the pixel grid to each spectrum's laser position (mm; `None`: none stated, or the lock-mass
/// function's). A scan off either axis's grid loses its pixel, and both axes are fitted again
/// without it: a stray on one axis's grid by chance must not stretch that axis. `Err` says why
/// there is no grid — an axis fits none, or more than [`crate::imaging::MAX_OFF_GRID`] of the
/// positioned scans lie off it.
fn fit_grid(mm: &[Option<(f64, f64)>], steps: [Option<f64>; 2]) -> Result<LaserGrid, String> {
    use crate::imaging::{MAX_OFF_GRID, fit_axis};
    let positioned = mm.iter().flatten().count();
    // A step larger than the whole raster (its central 90 %: a parked scan must not widen it) is not
    // its step: one read in the wrong unit (a MALDI method stating µm) would otherwise hold every
    // position in one pixel.
    // ponytail: the declared step is taken in mm, the unit of the positions and of DESI's settings.
    let (xs, ys): (Vec<f64>, Vec<f64>) = mm.iter().flatten().copied().unzip();
    let span = |mut v: Vec<f64>| {
        v.sort_by(f64::total_cmp);
        let trim = v.len() / 20;
        v[v.len() - 1 - trim] - v[trim]
    };
    let raster = span(xs).max(span(ys));
    let steps = steps.map(|s| s.filter(|d| *d <= raster));
    let mut keep: Vec<bool> = mm.iter().map(Option::is_some).collect();
    loop {
        let (xs, ys): (Vec<f64>, Vec<f64>) = mm.iter().zip(&keep).filter(|(_, k)| **k).filter_map(|(p, _)| *p).unzip();
        let (x, xi) = fit_axis(&xs, steps[0]).ok_or("the x positions lie on no raster")?;
        let (y, yi) = fit_axis(&ys, steps[1]).ok_or("the y positions lie on no raster")?;
        let pixels: Vec<Option<(i64, i64)>> = xi.into_iter().zip(yi).map(|(a, b)| a.zip(b)).collect();
        if pixels.iter().all(Option::is_some) {
            let off_grid = positioned - pixels.len();
            if off_grid as f64 > MAX_OFF_GRID * positioned as f64 {
                return Err(format!("{off_grid} of {positioned} positioned scans lie off the grid (more than {}%)", MAX_OFF_GRID * 100.0));
            }
            let mut pixels = pixels.into_iter();
            let positions = keep.iter().map(|k| if *k { pixels.next().flatten() } else { None }).collect();
            return Ok(LaserGrid { x, y, positions, off_grid });
        }
        let mut pixels = pixels.into_iter();
        for k in keep.iter_mut().filter(|k| **k) {
            *k = pixels.next().flatten().is_some();
        }
    }
}

impl WatersImaging {
    fn read(
        api: ScanItemApi,
        info: *mut c_void,
        index: &[(c_int, c_int, f32)],
        ids: [c_int; 2],
        names: (String, String),
        lockmass: Option<c_int>,
        steps: [Option<(String, f64)>; 2],
    ) -> Option<Self> {
        let mut records: std::collections::HashMap<c_int, bool> = std::collections::HashMap::new();
        let mm = laser_positions(index, lockmass, |f, scan| {
            let has = *records.entry(f).or_insert_with(|| {
                let a = api.available(info, f);
                ids.iter().all(|i| a.contains(i))
            });
            if !has {
                return None;
            }
            let v = api.values(info, f, scan, &ids);
            let num = |k: usize| v.get(k).cloned().flatten()?.trim().parse::<f64>().ok().filter(|x| x.is_finite());
            Some((num(0)?, num(1)?))
        });
        let lockmass_scans = index.iter().filter(|e| Some(e.0) == lockmass).count();
        Self::from_positions(mm, names, steps, lockmass_scans)
    }

    /// The run's imaging from each spectrum's laser position; `None` when fewer than two distinct
    /// positions are stated (no raster: not an imaging run).
    fn from_positions(mm: Vec<Option<(f64, f64)>>, names: (String, String), steps: [Option<(String, f64)>; 2], lockmass_scans: usize) -> Option<Self> {
        let cells: std::collections::BTreeSet<(i64, i64)> = mm.iter().flatten().map(|(x, y)| ((x * 1e3).round() as i64, (y * 1e3).round() as i64)).collect();
        if cells.len() < 2 {
            return None;
        }
        let positioned = mm.iter().flatten().count();
        let grid = fit_grid(&mm, [steps[0].as_ref().map(|s| s.1), steps[1].as_ref().map(|s| s.1)]);
        match &grid {
            Ok(g) => log::info!(
                "Waters imaging: {positioned} scans, grid {} x {} at {:?} x {:?} mm, {} off the grid",
                g.x.count, g.y.count, g.x.pitch, g.y.pitch, g.off_grid
            ),
            Err(why) => log::warn!("MassLynx: {positioned} scans state a laser position, but {why}; no pixel positions written"),
        }
        Some(WatersImaging { grid, names, steps, positioned, lockmass_scans })
    }

    /// The pixel of spectrum `i`.
    pub fn position(&self, i: usize) -> Option<(i64, i64)> {
        self.grid.as_ref().ok()?.positions.get(i).copied().flatten()
    }

    /// The grid: pixel counts always; pixel size (the step, µm) and max dimension on each axis with a
    /// step — a single row keeps the column step (review 2026-09-30 B15). `None` without a grid.
    pub fn scan_settings(&self) -> Option<mzdata::meta::ScanSettings> {
        let g = self.grid.as_ref().ok()?;
        let mut s = mzdata::meta::ScanSettings { id: "scansettings1".into(), ..Default::default() };
        let p = |name: &str, curie, v: mzdata::params::Value, unit| Param::builder().name(name).curie(curie).value(v).unit(unit).build();
        s.params.push(p("max count of pixels x", mzdata::curie!(IMS:1000042), g.x.count.into(), Unit::Unknown));
        s.params.push(p("max count of pixels y", mzdata::curie!(IMS:1000043), g.y.count.into(), Unit::Unknown));
        if let Some(ux) = g.x.pitch.map(|p| p * 1000.0) {
            s.params.push(p("pixel size (x)", mzdata::curie!(IMS:1000046), ux.into(), Unit::Micrometer));
            s.params.push(p("max dimension x", mzdata::curie!(IMS:1000044), (g.x.count as f64 * ux).into(), Unit::Micrometer));
        }
        if let Some(uy) = g.y.pitch.map(|p| p * 1000.0) {
            s.params.push(p("pixel size y", mzdata::curie!(IMS:1000047), uy.into(), Unit::Micrometer));
            s.params.push(p("max dimension y", mzdata::curie!(IMS:1000045), (g.y.count as f64 * uy).into(), Unit::Micrometer));
        }
        Some(s)
    }

    /// The `transformations` entries of a run with a grid.
    pub fn transformations(&self) -> Vec<&'static str> {
        match &self.grid {
            Ok(g) if g.off_grid > 0 => vec![LASER_GRID, OFF_GRID_DROPPED],
            _ => vec![LASER_GRID],
        }
    }

    /// Where an axis's step came from.
    fn step_source(&self, a: usize, axis: &crate::imaging::GridAxis) -> String {
        match (&self.steps[a], axis.declared) {
            (Some((name, _)), true) => format!("declared: methodfile.xml {name}"),
            (Some((name, v)), false) => format!("fitted: the declared methodfile.xml {name} = {v} mm does not hold the positions"),
            (None, _) if axis.pitch.is_none() => "none: a single row or column".into(),
            (None, _) => "fitted".into(),
        }
    }

    /// The `waters_imaging` index block: where the positions came from and how well they fit, or why
    /// they fit no grid.
    pub fn block(&self) -> serde_json::Value {
        let mut b = serde_json::json!({
            "source": format!("MassLynx scan items {:?} / {:?} (laser aim position, mm)", self.names.0, self.names.1),
            "scans_with_laser_position": self.positioned,
            "lockmass_scans_excluded": self.lockmass_scans,
            "declared_steps_mm": self.steps.iter().map(|s| s.as_ref().map(|(name, v)| serde_json::json!({"setting": name, "value": v}))).collect::<Vec<_>>(),
        });
        match &self.grid {
            Ok(g) => {
                let axis = |a: usize, x: &crate::imaging::GridAxis| serde_json::json!({
                    "origin_mm": x.origin, "pitch_mm": x.pitch, "count": x.count, "max_residual_mm": x.max_residual,
                    "step_source": self.step_source(a, x),
                });
                b["positions"] = "grid index = round((position − origin) / pitch) + 1".into();
                b["scans_with_position"] = g.positions.iter().flatten().count().into();
                b["off_grid_scans_dropped"] = g.off_grid.into();
                b["x"] = axis(0, &g.x);
                b["y"] = axis(1, &g.y);
            }
            Err(why) => b["no_grid"] = format!("{why}: no positions and no imaging marker written").into(),
        }
        b
    }

    /// The `provenance` of the `metadata.imaging` marker.
    pub fn provenance(&self) -> serde_json::Value {
        let Ok(g) = &self.grid else { return serde_json::Value::Null };
        let size = |a: &crate::imaging::GridAxis| if a.pitch.is_some() { "the step" } else { "not written: a single row or column" };
        serde_json::json!({
            "detected_from": "laser aim positions in the MassLynx scan items",
            "positions": "grid indices fitted to the positions in mm (waters_imaging)",
            "origin_mm": {"x": g.x.origin, "y": g.y.origin},
            "pixel_size": {"x": size(&g.x), "y": size(&g.y)},
        })
    }
}

/// Each spectrum's laser position (mm) as `read(function, scan)` gives it — except the lock-mass
/// function's: its reference scans sample the lock spray, not the surface, and get no pixel
/// (review 2026-09-30 B15).
fn laser_positions(
    index: &[(c_int, c_int, f32)],
    lockmass: Option<c_int>,
    mut read: impl FnMut(c_int, c_int) -> Option<(f64, f64)>,
) -> Vec<Option<(f64, f64)>> {
    index.iter().map(|&(f, scan, _)| if Some(f) == lockmass { None } else { read(f, scan) }).collect()
}

/// The laser position items among the scan items of every written function but the lock mass —
/// `available(f)` lists a function's item ids, `names(ids)` names them (function 1 alone was asked
/// before, and a run recording them only in a later function lost its positions; review 2026-09-30
/// B15): ids and names for x and y. With no names readable at all, the SDK enum's `LASERAIM_XPOS` /
/// `_YPOS` (stat codes 9 / 10 in `_funcNNN.sts`).
fn laser_items(
    index: &[(c_int, c_int, f32)],
    lockmass: Option<c_int>,
    available: impl FnMut(c_int) -> Vec<c_int>,
    names: impl FnOnce(&[c_int]) -> Vec<(c_int, String)>,
) -> Option<([c_int; 2], (String, String))> {
    let functions: std::collections::BTreeSet<c_int> = index.iter().map(|e| e.0).filter(|f| Some(*f) != lockmass).collect();
    let ids: std::collections::BTreeSet<c_int> = functions.into_iter().flat_map(available).collect();
    let named = names(&ids.into_iter().collect::<Vec<_>>());
    let find = |axis| named.iter().find(|(_, n)| names_position(&n.to_ascii_uppercase().replace(['_', '-'], " "), axis));
    match (find('X'), find('Y')) {
        (Some(x), Some(y)) => Some(([x.0, y.0], (x.1.clone(), y.1.clone()))),
        _ if named.is_empty() => Some(([SCAN_ITEM_FIRST + 8, SCAN_ITEM_FIRST + 9], ("LASERAIM_XPOS".into(), "LASERAIM_YPOS".into()))),
        _ => None,
    }
}

/// The raster step a Waters method declares per axis (`methodfile.xml`: `<Setting Name="DesiXStep"
/// Value="0.1" Mapping="Desi.Pattern.XStep"/>`, mm): the first setting whose name ends in `XStep` /
/// `YStep`, whatever its prefix (MALDI methods may use another). Case as written: the names of
/// `CollisionEnergyStep`, `MaxStep` or `LaserDelayStep` end in `yStep` / `xStep` too, and one
/// before `DesiYStep` would stand in for it (review 2026-09-30).
fn declared_steps(xml: &str) -> [Option<(String, f64)>; 2] {
    let mut steps: [Option<(String, f64)>; 2] = [None, None];
    let mut reader = quick_xml::Reader::from_str(xml);
    while let Ok(ev) = reader.read_event() {
        match ev {
            quick_xml::events::Event::Start(e) | quick_xml::events::Event::Empty(e) if e.local_name().as_ref() == b"Setting" => {
                let (Some(name), Some(value)) = (crate::imaging::attr(&e, b"Name"), crate::imaging::attr(&e, b"Value")) else { continue };
                let Some(v) = value.trim().parse::<f64>().ok().filter(|v| v.is_finite() && *v > 0.0) else { continue };
                for (slot, suffix) in steps.iter_mut().zip(["XStep", "YStep"]) {
                    if slot.is_none() && name.ends_with(suffix) {
                        *slot = Some((name.clone(), v));
                    }
                }
            }
            quick_xml::events::Event::Eof => break,
            _ => {}
        }
    }
    steps
}

/// Does a MassLynx scan-item name (upper-cased, `_`/`-` as spaces) name the `axis` position?
/// `LASERAIM_XPOS`, `X Pos`, `X Position`; not `MAX POS…`.
fn names_position(u: &str, axis: char) -> bool {
    let words: Vec<&str> = u.split_whitespace().collect();
    let pos = |w: &str| w == "POS" || w == "POSITION";
    words.iter().any(|w| w.strip_prefix(axis).is_some_and(pos))
        || words.windows(2).any(|w| w[0].len() == 1 && w[0].starts_with(axis) && pos(w[1]))
}

/// What MassLynx states about one function, resolved once at open.
#[derive(Debug, Clone, Default)]
struct FunctionInfo {
    /// `isContinuum`; `None` when the export is unavailable or failed.
    continuum: Option<bool>,
    /// `getFunctionType(f)`: ProteoWizard's function-type enum + 200 (218 = `TOF MS`, 216 = `TOFD`).
    type_code: Option<c_int>,
    /// `getFunctionTypeString(type_code)`, e.g. `TOF MS` — cosmetic; the CODE drives the rules.
    type_string: Option<String>,
    /// The transfer-cell collision-energy ramp the METHOD states for this function (`_extern.inf`
    /// "Transfer Collision Energy Ramp Start/End (eV)"), when it states one.
    ce_ramp: Option<(f64, f64)>,
    /// `getIonModeString(getIonMode(f))`, e.g. `ES+`.
    ion_mode: Option<String>,
    /// `getAcquisitionMassRange(f, 0)`.
    mass_range: Option<(f32, f32)>,
    /// Drift bins per scan when the function carries a `.cdt` and MassLynx counts bins; 0 otherwise.
    drift_bins: c_int,
    /// SONAR: the "drift" bins are quadrupole positions, not drift times.
    sonar: bool,
    /// The bins of a SONAR function that were summed into its `readScan` spectrum; 0 otherwise.
    sonar_bins: c_int,
    /// `COLLISION_ENERGY` of the function's first scan (eV), when readable.
    collision_energy_0: Option<f64>,
    ms_level: u8,
}

/// A native Waters `.raw` reader. Holds the loaded library, the resolved C function pointers, the
/// info + scan reader handles, the per-function facts and the flattened `(function, scan)` index.
pub struct WatersReader {
    // `_lib` MUST outlive the function pointers + handles below (dropped together with this struct).
    _lib: Library,
    read_scan: ReadScanFn,
    read_drift_scan: Option<ReadDriftScanFn>,
    destroy: DestroyReaderFn,
    info_reader: *mut c_void,
    scan_reader: *mut c_void,
    functions: Vec<FunctionInfo>,
    /// bin → drift time in ms, one table per run (MassLynx's `getDriftTime` takes no function).
    drift_time_ms: Vec<f32>,
    /// One entry per spectrum: (function index, scan index, retention time in minutes), the
    /// indices 0-based, ORDERED BY TIME across functions (stable: function, then scan, on ties) —
    /// the order ProteoWizard uses and the order the reader's time lookups assume.
    index: Vec<(c_int, c_int, f32)>,
    input: PathBuf,
    /// Functions MassLynx appends as "collapsed retention time data": one row per drift bin holding
    /// the run-summed spectrum of that bin, with the drift time in the RT slot — derived summaries
    /// of `summary_of`, not acquisitions. Skipped unless `MZPC_WATERS_KEEP_COLLAPSED` is set.
    collapsed: Vec<(c_int, Option<c_int>)>,
    scan_items: Option<ScanItemApi>,
    item_ids: ScanItemIds,
    /// The lock-mass reference function, when MassLynx names one.
    lockmass_function: Option<c_int>,
    /// Functions not written as spectra, with the reason: chromatogram-type or not MS (MassLynx's
    /// type code), or a scan count MassLynx could not return.
    skipped: Vec<(c_int, String)>,
    /// `MZPC_WATERS_KEEP_COLLAPSED`, read once: whether the collapsed functions were written.
    keep_collapsed: bool,
    /// Frames whose bins came back out of (m/z, drift time) order and were re-sorted by
    /// [`Self::spectrum`]. Shared with the converter, which counts it over the written spectra only
    /// (`VendorHints::counters`) and declares `sort-by-mz` when it moved.
    resorted: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Scans [`Self::spectrum`] read as a SONAR function's quadrupole bins summed (`readScan`).
    /// Shared the same way, declaring [`SONAR_SUMMED`].
    sonar_summed: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// The pixel grid of an imaging run (laser aim positions); `None` otherwise.
    imaging: Option<WatersImaging>,
}

impl WatersReader {
    /// Open a Waters `.raw` directory and build the flattened spectrum index.
    pub fn open(input: &Path) -> Result<Self> {
        let dir = resolve_masslynx_dir()?;
        prepend_dir_to_path(&dir);
        let dll = dir.join("MassLynxRaw.dll");
        if !dll.is_file() {
            bail!(
                "MassLynxRaw.dll not found in {} (set MZPC_MASSLYNX_DIR / MZPC_PWIZ_DIR to a \
                 ProteoWizard pwiz-bin directory)",
                dir.display()
            );
        }

        // SAFETY: loading the vendor DLL + resolving its documented C exports. The DLL's own deps
        // (cdt.dll, …) resolve via the PATH we just prepended.
        let lib =
            unsafe { Library::new(&dll) }.with_context(|| format!("loading {}", dll.display()))?;
        // Copy the raw function pointers out of the borrowed Symbols; they stay valid as long as
        // `lib` is loaded (kept alive in this struct).
        let create: CreateFromPathFn = *unsafe { lib.get(b"createRawReaderFromPath\0") }
            .context("resolving MassLynx export createRawReaderFromPath")?;
        let destroy: DestroyReaderFn = *unsafe { lib.get(b"destroyRawReader\0") }
            .context("resolving MassLynx export destroyRawReader")?;
        let get_function_count: GetFunctionCountFn = *unsafe { lib.get(b"getFunctionCount\0") }
            .context("resolving MassLynx export getFunctionCount")?;
        let read_scan_count: GetIntPerFunctionFn = *unsafe { lib.get(b"getScanCount\0") }
            .context("resolving MassLynx export getScanCount")?;
        let read_scan: ReadScanFn =
            *unsafe { lib.get(b"readScan\0") }.context("resolving MassLynx export readScan")?;
        // OPTIONAL exports: an older MassLynx build without them degrades (profile default, no RT,
        // summed scans) rather than failing the conversion; each absence is logged.
        let opt = |name: &[u8]| -> bool { unsafe { lib.get::<*const c_void>(name) }.is_ok() };
        let is_continuum: Option<IsContinuumFn> = unsafe { lib.get(b"isContinuum\0") }.ok().map(|f| *f);
        let function_type: Option<GetIntPerFunctionFn> = unsafe { lib.get(b"getFunctionType\0") }.ok().map(|f| *f);
        let type_string: Option<CodeToStringFn> = unsafe { lib.get(b"getFunctionTypeString\0") }.ok().map(|f| *f);
        let ion_mode: Option<GetIntPerFunctionFn> = unsafe { lib.get(b"getIonMode\0") }.ok().map(|f| *f);
        let mode_string: Option<CodeToStringFn> = unsafe { lib.get(b"getIonModeString\0") }.ok().map(|f| *f);
        let mass_range: Option<GetMassRangeFn> = unsafe { lib.get(b"getAcquisitionMassRange\0") }.ok().map(|f| *f);
        let retention_time: Option<GetRetentionTimeFn> = unsafe { lib.get(b"getRetentionTime\0") }.ok().map(|f| *f);
        let drift_count: Option<GetIntPerFunctionFn> = unsafe { lib.get(b"getDriftScanCount\0") }.ok().map(|f| *f);
        let drift_time: Option<GetDriftTimeFn> = unsafe { lib.get(b"getDriftTime\0") }.ok().map(|f| *f);
        let read_drift_scan: Option<ReadDriftScanFn> = unsafe { lib.get(b"readDriftScan\0") }.ok().map(|f| *f);
        let scan_items = ScanItemApi::resolve(&lib);
        let lockmass_fn: Option<LockMassFunctionFn> = unsafe { lib.get(b"getLockMassFunction\0") }.ok().map(|f| *f);
        for (name, present) in [
            ("isContinuum", opt(b"isContinuum\0")),
            ("getFunctionType", opt(b"getFunctionType\0")),
            ("getRetentionTime", opt(b"getRetentionTime\0")),
            ("getDriftScanCount", opt(b"getDriftScanCount\0")),
            ("getDriftTime", opt(b"getDriftTime\0")),
            ("readDriftScan", opt(b"readDriftScan\0")),
            ("getScanItemValue (+ parameters object)", scan_items.is_some()),
        ] {
            if !present {
                log::warn!("MassLynxRaw.dll does not export {name}; the archive will lack what it provides");
            }
        }

        let path_str = input
            .to_str()
            .ok_or_else(|| anyhow!("Waters .raw path is not valid UTF-8: {}", input.display()))?;
        let cpath = CString::new(path_str)
            .map_err(|_| anyhow!("Waters .raw path contains an interior NUL"))?;

        // Create the INFO reader (function/scan facts) and the SCAN reader (data).
        let mut info_reader: *mut c_void = ptr::null_mut();
        let rc = unsafe { create(cpath.as_ptr(), &mut info_reader, ML_TYPE_INFO) };
        if rc != 0 || info_reader.is_null() {
            bail!(
                "MassLynx createRawReaderFromPath(INFO) failed (rc={rc}) for {}",
                input.display()
            );
        }
        let mut scan_reader: *mut c_void = ptr::null_mut();
        let rc = unsafe { create(cpath.as_ptr(), &mut scan_reader, ML_TYPE_SCAN) };
        if rc != 0 || scan_reader.is_null() {
            unsafe { destroy(info_reader) };
            bail!(
                "MassLynx createRawReaderFromPath(SCAN) failed (rc={rc}) for {}",
                input.display()
            );
        }
        let close = |why: String| -> anyhow::Error {
            unsafe {
                destroy(scan_reader);
                destroy(info_reader);
            }
            anyhow!(why)
        };

        let mut n_functions: c_int = 0;
        let rc = unsafe { get_function_count(info_reader, &mut n_functions) };
        if rc != 0 || n_functions < 0 {
            return Err(close(format!("MassLynx getFunctionCount failed (rc={rc}, n={n_functions})")));
        }

        // The lock-mass reference function: MassLynx's answer, else the method text
        // (`_extern.inf`: "Function Parameters - Function N - REFERENCE").
        let lockmass_function = lockmass_fn
            .and_then(|g| {
                let mut has: c_char = 0;
                let mut which: c_int = -1;
                (unsafe { g(info_reader, &mut has, &mut which) } == 0 && has != 0 && which >= 0).then_some(which)
            })
            .or_else(|| reference_functions_from_extern_inf(input).first().copied());
        // The scan items the DLL records, by NAME, so the ids do not depend on the enum base.
        let mut item_ids = ScanItemIds::default();
        if let Some(api) = scan_items {
            let ids = api.available(info_reader, 0);
            let named = api.names(info_reader, &ids);
            log::info!(
                "MassLynx scan items (function 1, {} ids): {}",
                ids.len(),
                named.iter().map(|(i, n)| format!("{i}:{n}")).collect::<Vec<_>>().join(" | ")
            );
            for (id, name) in &named {
                let u = name.to_ascii_uppercase().replace(['_', '-'], " ");
                if u.contains("SET MASS") && !u.contains("CAL") && !u.contains("SUPPORTED") && item_ids.set_mass.is_none() {
                    item_ids.set_mass = Some(*id);
                } else if u == "COLLISION ENERGY" || (u.contains("COLLISION ENERGY") && !u.contains('2') && item_ids.collision_energy.is_none()) {
                    item_ids.collision_energy = Some(*id);
                } else if u.contains("SONAR") && item_ids.sonar.is_none() {
                    item_ids.sonar = Some(*id);
                }
            }
            if named.is_empty() {
                log::warn!("MassLynx scan item names unreadable; using the SDK enum ids (first item {SCAN_ITEM_FIRST})");
                item_ids = ScanItemIds {
                    set_mass: Some(SCAN_ITEM_FIRST + 76),
                    collision_energy: Some(SCAN_ITEM_FIRST + 61),
                    sonar: Some(SCAN_ITEM_FIRST + 80),
                };
            }
            log::info!("MassLynx scan item ids: {item_ids:?}; lock-mass function: {:?}", lockmass_function.map(|f| f + 1));
        }

        // The method text: per-function transfer collision-energy ramps (MSe's elevated energy lives
        // there, not in the per-scan COLLISION_ENERGY item, which is the trap cell's 4 eV).
        let method_ramps = method_ce_ramps_from_extern_inf(input);

        // Per-function facts, each an independent optional call (a failed one leaves its field None).
        let int_of = |g: Option<GetIntPerFunctionFn>, f: c_int| -> Option<c_int> {
            g.and_then(|g| {
                let mut v: c_int = 0;
                (unsafe { g(info_reader, f, &mut v) } == 0).then_some(v)
            })
        };
        let string_of = |g: Option<CodeToStringFn>, code: c_int| -> Option<String> {
            g.and_then(|g| {
                let mut p: *const c_char = ptr::null();
                let rc = unsafe { g(info_reader, code, &mut p) };
                if rc != 0 || p.is_null() {
                    return None;
                }
                // Copied, never released: ownership of these strings is undocumented.
                Some(unsafe { CStr::from_ptr(p) }.to_string_lossy().trim().to_string())
            })
        };
        let mut sonar_unchecked: Vec<c_int> = Vec::new();
        let mut functions: Vec<FunctionInfo> = Vec::with_capacity(n_functions as usize);
        for f in 0..n_functions {
            let continuum = is_continuum.and_then(|g| {
                // 8-byte slot: a BOOL-writing export cannot clobber a neighbour; byte 0 is the answer.
                let mut slot = [0u8; 8];
                (unsafe { g(info_reader, f, slot.as_mut_ptr() as *mut bool) } == 0).then_some(slot[0] != 0)
            });
            let type_code = int_of(function_type, f);
            let type_string = type_code.and_then(|code| string_of(type_string, code));
            let ion_mode = int_of(ion_mode, f).and_then(|code| string_of(mode_string, code));
            let mass_range = mass_range.and_then(|g| {
                let (mut lo, mut hi): (f32, f32) = (0.0, 0.0);
                (unsafe { g(info_reader, f, 0, &mut lo, &mut hi) } == 0 && hi >= lo && hi > 0.0).then_some((lo, hi))
            });
            // pwiz's rule (WatersRawFile.hpp): a function is ion-mobility data when its `.cdt`
            // exists AND MassLynx counts drift bins for it.
            let has_cdt = ["cdt", "CDT"].iter().any(|ext| {
                input.join(format!("_func{:03}.{ext}", f + 1)).is_file()
                    || input.join(format!("_FUNC{:03}.{ext}", f + 1)).is_file()
            });
            let mut drift_bins = if has_cdt && read_drift_scan.is_some() {
                match int_of(drift_count, f) {
                    Some(n) if (1..=MAX_DRIFT_BINS).contains(&n) => n,
                    Some(n) if n > MAX_DRIFT_BINS => {
                        log::warn!("MassLynx function {}: getDriftScanCount says {n} bins (> {MAX_DRIFT_BINS}); treated as a summed scan", f + 1);
                        0
                    }
                    Some(_) => 0,
                    None => {
                        log::warn!("MassLynx function {}: has a .cdt but getDriftScanCount failed; written as the summed scan", f + 1);
                        0
                    }
                }
            } else {
                0
            };
            // SONAR: the bins are quadrupole positions (pwiz gates its drift-time labelling on it);
            // until the lane can state them as such, the summed scan is the honest product.
            let mut sonar = false;
            let mut collision_energy_0 = None;
            if let Some(api) = scan_items {
                let ids: Vec<c_int> = [item_ids.sonar, item_ids.collision_energy].into_iter().flatten().collect();
                let vals = api.values(info_reader, f, 0, &ids);
                let get = |want: Option<c_int>| ids.iter().position(|&i| Some(i) == want).and_then(|k| vals.get(k).cloned().flatten());
                if let Some(v) = get(item_ids.sonar) {
                    sonar = !(v.trim() == "0" || v.trim().is_empty() || v.eq_ignore_ascii_case("false"));
                }
                collision_energy_0 = get(item_ids.collision_energy).and_then(|v| v.trim().parse::<f64>().ok()).map(f64::abs);
            }
            if drift_bins > 0 && !sonar && (scan_items.is_none() || item_ids.sonar.is_none()) {
                sonar_unchecked.push(f + 1);
            }
            let mut sonar_bins = 0;
            if sonar && drift_bins > 0 {
                log::warn!(
                    "MassLynx function {}: SONAR — its {} bins are quadrupole positions, not drift times; \
                     written as the summed scan (SONAR support pending)",
                    f + 1,
                    drift_bins
                );
                sonar_bins = drift_bins;
                drift_bins = 0;
            }
            let ce_ramp = method_ramps.get(&f).copied();
            functions.push(FunctionInfo { continuum, type_code, type_string, ion_mode, mass_range, drift_bins, sonar, sonar_bins, collision_energy_0, ce_ramp, ms_level: 1 });
        }
        if !sonar_unchecked.is_empty() {
            log::warn!(
                "MassLynx: {} — the drift bins of function(s) {:?} are written as drift times on trust (a SONAR function would be mislabelled)",
                if scan_items.is_none() { "scan items unavailable, SONAR not checked" } else { "the item table has no `Sonar Enabled`" },
                sonar_unchecked
            );
        }
        for f in 0..functions.len() {
            functions[f].ms_level = ms_level_for(f, &functions, lockmass_function);
        }
        // Functions ProteoWizard never emits as spectra: SIR/MRM-family (chromatogram data), neutral
        // loss/gain, DAD (a wavelength axis), delay/concatenated/off/AutoSpec scan types. Written as
        // spectra they would be MS1 rows with no m/z (DAD) or one-point "spectra" without their
        // transition (MRM) — the same defect class the Agilent and SciEX lanes refuse.
        let mut skipped_functions: Vec<c_int> = Vec::new();
        let mut skipped: Vec<(c_int, String)> = Vec::new();
        for (f, fi) in functions.iter().enumerate() {
            if let Some(kind) = fi.type_code.and_then(FunctionKind::from_code) {
                if let FunctionKind::Chromatogram(what) | FunctionKind::NotMs(what) = kind {
                    log::warn!(
                        "MassLynx function {} ({}) is {what}; not written as spectra (ProteoWizard writes SIR/MRM as chromatograms and skips the rest)",
                        f + 1,
                        fi.type_string.as_deref().unwrap_or("?")
                    );
                    skipped_functions.push(f as c_int);
                    skipped.push((f as c_int, what.to_string()));
                }
            }
        }
        if skipped_functions.len() == functions.len() {
            return Err(close(format!(
                "{}: every MassLynx function is chromatogram-type or non-MS ({}); the native lane writes no spectra for those. \
                 Use --via-msconvert, which writes SRM/SIM chromatograms.",
                input.display(),
                functions.iter().map(|fi| fi.type_string.clone().unwrap_or_else(|| "?".into())).collect::<Vec<_>>().join(", ")
            )));
        }

        // The run's drift-time table: bin → ms, read once for the largest bin count of any IMS function.
        let mut drift_time_ms = Vec::new();
        if let Some(n) = functions.iter().map(|fi| fi.drift_bins).max().filter(|n| *n > 0) {
            let Some(g) = drift_time else {
                return Err(close("MassLynxRaw.dll has drift bins but no getDriftTime export; refusing to label frames".into()));
            };
            for bin in 0..n {
                let mut ms: f32 = f32::NAN;
                if unsafe { g(info_reader, bin, &mut ms) } != 0 || !ms.is_finite() {
                    return Err(close(format!("MassLynx getDriftTime(bin={bin}) failed; refusing to write drift frames with an unknown time axis")));
                }
                drift_time_ms.push(ms);
            }
        }

        // Log what the vendor actually said, once: a working binding and a silently failing one are
        // otherwise indistinguishable whenever the run happens to be all continuum / all MS1.
        log::info!(
            "MassLynx functions: {}",
            functions
                .iter()
                .enumerate()
                .map(|(f, fi)| {
                    format!(
                        "{}={} {} {} {} drift_bins={} ms{}",
                        f + 1,
                        fi.type_string.as_deref().unwrap_or("?"),
                        fi.ion_mode.as_deref().unwrap_or("?"),
                        match fi.continuum {
                            Some(true) => "continuum",
                            Some(false) => "centroid",
                            None => "continuity-unknown",
                        },
                        fi.mass_range.map(|(lo, hi)| format!("{lo}-{hi}")).unwrap_or_else(|| "range-unknown".into()),
                        fi.drift_bins,
                        fi.ms_level
                    )
                })
                .collect::<Vec<_>>()
                .join(" | ")
        );
        if !drift_time_ms.is_empty() {
            log::info!(
                "MassLynx drift table: {} bins, {} .. {} ms",
                drift_time_ms.len(),
                drift_time_ms[0],
                drift_time_ms[drift_time_ms.len() - 1]
            );
        }

        // Every scan's retention time (pwiz calls it per scan too); the index is sorted by it.
        let mut index: Vec<(c_int, c_int, f32)> = Vec::new();
        let mut collapsed: Vec<(c_int, Option<c_int>)> = Vec::new();
        // Read ONCE, the way every on/off lever is read (`crate::env_flag`: empty, 0, false and no are
        // off). This value decides below AND is what the index blocks report as `written`: the two
        // used to parse the variable differently, so `=0` skipped the functions yet said written.
        let keep_collapsed = crate::env_flag("MZPC_WATERS_KEEP_COLLAPSED").unwrap_or(false);
        let mut rt_failures = 0usize;
        let mut acquired_ims_functions: Vec<c_int> = Vec::new();
        for f in 0..n_functions {
            if skipped_functions.contains(&f) {
                continue;
            }
            let Some(n_scans) = int_of(Some(read_scan_count), f).filter(|n| *n >= 0) else {
                log::warn!("MassLynx getScanCount(function {}) failed; the function is skipped", f + 1);
                // Recorded like a type-code skip, so `waters_functions` and `waters:drop-functions`
                // say the archive lacks it.
                skipped.push((f, "getScanCount failed".to_string()));
                continue;
            };
            let mut rts: Vec<f32> = Vec::with_capacity(n_scans as usize);
            for scan in 0..n_scans {
                let mut minutes: f32 = f32::NAN;
                let ok = retention_time.is_some_and(|g| unsafe { g(info_reader, f, scan, &mut minutes) } == 0 && minutes.is_finite());
                if !ok {
                    rt_failures += 1;
                    minutes = 0.0;
                }
                rts.push(minutes);
            }
            // "Save Collapsed Retention Time Data": a function with exactly one scan per drift bin whose
            // "retention times" ARE the drift table is MassLynx's run-summed mobilogram of an earlier
            // function — derived data, and poison as spectra (the run's ions a second time, folded
            // into the first 7.8 "minutes" of the TIC). Detected structurally, never by index.
            let fi = &functions[f as usize];
            let is_collapsed = fi.drift_bins > 0
                && n_scans == fi.drift_bins
                && drift_time_ms.len() == n_scans as usize
                && rts.iter().zip(&drift_time_ms).all(|(rt, dt)| (rt - dt).abs() < 1e-4);
            if is_collapsed {
                let summary_of = acquired_ims_functions.get(collapsed.len()).copied();
                collapsed.push((f, summary_of));
                log::info!(
                    "MassLynx function {}: collapsed retention-time data (one row per drift bin, run-summed{}); {}",
                    f + 1,
                    summary_of.map(|s| format!(" — a summary of function {}", s + 1)).unwrap_or_default(),
                    if keep_collapsed { "kept (MZPC_WATERS_KEEP_COLLAPSED)" } else { "not written as spectra" }
                );
                if !keep_collapsed {
                    continue;
                }
            } else if fi.drift_bins > 0 {
                acquired_ims_functions.push(f);
            }
            for (scan, rt) in rts.into_iter().enumerate() {
                index.push((f, scan as c_int, rt));
            }
        }
        if rt_failures > 0 {
            return Err(close(format!(
                "MassLynx getRetentionTime failed for {rt_failures} scans; refusing to write an index whose time order is unknown"
            )));
        }
        // Time order across functions (stable, so ties keep function then scan order).
        index.sort_by(|a, b| a.2.total_cmp(&b.2).then(a.0.cmp(&b.0)).then(a.1.cmp(&b.1)));
        if index.is_empty() {
            return Err(close(format!("Waters .raw {} has no readable scans", input.display())));
        }

        // Imaging (MALDI / DESI): every scan's laser aim position, fitted to a pixel grid — the items
        // looked up in every written function but the lock mass, the step the method declares.
        let imaging = scan_items.and_then(|api| {
            let (items, names) = laser_items(&index, lockmass_function, |f| api.available(info_reader, f), |ids| api.names(info_reader, ids))?;
            let method = crate::run_metadata::read_text_lossy(&input.join("methodfile.xml")).unwrap_or_default();
            WatersImaging::read(api, info_reader, &index, items, names, lockmass_function, declared_steps(&method))
        });

        if let Some(level) = std::env::var("MZPC_WATERS_PROBE_QUAD").ok().filter(|v| !v.is_empty() && v != "0") {
            probe_quad_windows(&lib, info_reader, scan_reader, functions.len(), &index, scan_items, &level);
        }
        let reader = WatersReader {
            _lib: lib,
            read_scan,
            read_drift_scan,
            destroy,
            info_reader,
            scan_reader,
            functions,
            drift_time_ms,
            index,
            input: input.to_path_buf(),
            collapsed,
            scan_items,
            item_ids,
            lockmass_function,
            skipped,
            keep_collapsed,
            resorted: Default::default(),
            sonar_summed: Default::default(),
            imaging,
        };
        Ok(reader)
    }

    pub fn len(&self) -> usize {
        self.index.len()
    }

    /// The pixel grid, when this is an imaging run.
    pub fn imaging(&self) -> Option<&WatersImaging> {
        self.imaging.as_ref()
    }

    /// Does any function carry a drift dimension (and so does the archive carry frames)?
    pub fn has_drift(&self) -> bool {
        self.functions.iter().any(|fi| fi.drift_bins > 0)
    }

    /// Spectrum indices the writer should sample for its data-facet schema: the first scan of
    /// every function, so the drift column is declared even when the IMS function is a small part
    /// of a mixed run (the default six-probe stride would miss it).
    pub fn probe_indices(&self) -> Vec<usize> {
        let mut seen = std::collections::HashSet::new();
        self.index
            .iter()
            .enumerate()
            .filter(|(_, (f, _, _))| seen.insert(*f))
            .map(|(i, _)| i)
            .collect()
    }

    /// The `waters_drift` index block: the run's bin → ms table, the bins per function and the
    /// vendor's CCS calibration file verbatim (`mob_cal.csv`, when present). What a reader needs to
    /// interpret the per-point `raw_ion_mobility` column and to go from drift time to CCS.
    pub fn drift_block(&self) -> Option<serde_json::Value> {
        if !self.has_drift() {
            return None;
        }
        let ccs = std::fs::read_to_string(self.input.join("mob_cal.csv"))
            .ok()
            .map(|t| t.lines().map(|l| l.trim_end().to_string()).collect::<Vec<_>>());
        Some(serde_json::json!({
            "vendor": "waters",
            "source": "MassLynxRaw getDriftScanCount / getDriftTime / readDriftScan",
            "representation": "one spectrum per MassLynx scan (frame); points sorted by (m/z, drift time); per-point raw ion mobility array MS:1003007 in milliseconds (the DLL's f32 values); the writer's zero-run mask is OFF for this archive so every bin's zero flanks survive",
            "ids": "function=F process=0 scan=B addresses the MassLynx scan (ProteoWizard's per-bin ids are scan=(B-1)*bins+bin+1; its combined-frame ids are merged=I function=F block=B)",
            "drift_time_unit": "ms",
            "drift_bins_per_function": self.functions.iter().enumerate().map(|(f, fi)| serde_json::json!({"function": f + 1, "drift_bins": fi.drift_bins, "sonar": fi.sonar})).collect::<Vec<_>>(),
            "drift_time_ms": self.drift_time_ms,
            "sonar_checked": self.scan_items.is_some() && self.item_ids.sonar.is_some(),
            "lockmass_function": self.lockmass_function.map(|f| f + 1),
            "collapsed_functions": self.collapsed.iter().map(|(f, of)| serde_json::json!({"function": f + 1, "summary_of": of.map(|o| o + 1), "written": self.keep_collapsed})).collect::<Vec<_>>(),
            "ccs_calibration_mob_cal_csv": ccs,
        }))
    }

    /// The `waters_functions` index block, written on EVERY Waters archive (see [`functions_block`]).
    pub fn functions_block(&self) -> serde_json::Value {
        functions_block(
            &self.functions,
            &self.skipped,
            &self.collapsed,
            self.keep_collapsed,
            self.lockmass_function,
            self.scan_items.is_some() && self.item_ids.sonar.is_some(),
        )
    }

    /// The `transformations` entries this run's functions call for (see [`function_transformations`]).
    pub fn transformations(&self) -> Vec<String> {
        function_transformations(&self.skipped, &self.collapsed, self.keep_collapsed)
    }

    /// The frame re-sort counter, for `VendorHints::counters` (see [`sort_frame_points`]).
    pub fn reorder_counter(&self) -> std::sync::Arc<std::sync::atomic::AtomicUsize> {
        self.resorted.clone()
    }

    /// The SONAR summed-scan counter, for `VendorHints::counters` under [`SONAR_SUMMED`].
    pub fn sonar_counter(&self) -> std::sync::Arc<std::sync::atomic::AtomicUsize> {
        self.sonar_summed.clone()
    }

    /// Read one spectrum. A function with drift bins yields a FRAME (every bin's points, sorted by
    /// m/z then drift time, with a per-point drift-time array); any other function the summed scan.
    pub fn spectrum(&self, i: usize) -> Result<MultiLayerSpectrum> {
        let (func, scan, rt_minutes) = *self
            .index
            .get(i)
            .ok_or_else(|| anyhow!("Waters spectrum index {i} out of range (len {})", self.len()))?;
        let fi = &self.functions[func as usize];

        let mut mz: Vec<f64> = Vec::new();
        let mut intensity: Vec<f32> = Vec::new();
        let mut drift: Vec<f32> = Vec::new();
        let mut drift_range: Option<(f32, f32)> = None;
        if let (Some(read_drift), true) = (self.read_drift_scan, fi.drift_bins > 0) {
            // Every bin of this scan; the vendor returns each bin's own m/z axis, so the frame is
            // sorted afterwards (the chunked layout needs a monotone main axis).
            let mut points: Vec<(f64, f32, f32)> = Vec::new();
            for bin in 0..fi.drift_bins {
                let dt = self.drift_time_ms.get(bin as usize).copied().unwrap_or(bin as f32);
                let (m, it) = self.read_bin(read_drift, func, scan, bin)?;
                if !m.is_empty() {
                    drift_range = Some(match drift_range {
                        Some((lo, hi)) => (lo.min(dt), hi.max(dt)),
                        None => (dt, dt),
                    });
                }
                points.extend(m.into_iter().zip(it).map(|(x, y)| (x, y, dt)));
            }
            if sort_frame_points(&mut points) {
                self.resorted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            mz.reserve(points.len());
            intensity.reserve(points.len());
            drift.reserve(points.len());
            for (x, y, d) in points {
                mz.push(x);
                intensity.push(y);
                drift.push(d);
            }
        } else {
            let (m, it) = self.read_summed(func, scan)?;
            // A SONAR function has no drift path (its bins are quadrupole positions): this scan IS
            // its bins summed, which the archive declares.
            if fi.sonar_bins > 0 {
                self.sonar_summed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            mz = m;
            intensity = it;
        }

        let mut arrays = BinaryArrayMap::new();
        let mut mz_da =
            DataArray::wrap(&ArrayType::MZArray, BinaryDataArrayType::Float64, Vec::new());
        mz_da
            .update_buffer(mz.as_slice())
            .map_err(|e| anyhow!("encoding m/z: {e}"))?;
        mz_da.unit = Unit::MZ;
        arrays.add(mz_da);
        let mut int_da =
            DataArray::wrap(&ArrayType::IntensityArray, BinaryDataArrayType::Float32, Vec::new());
        int_da
            .update_buffer(intensity.as_slice())
            .map_err(|e| anyhow!("encoding intensity: {e}"))?;
        int_da.unit = Unit::DetectorCounts;
        arrays.add(int_da);
        if fi.drift_bins > 0 {
            let mut im_da = DataArray::wrap(
                &ArrayType::RawIonMobilityArray,
                BinaryDataArrayType::Float32,
                Vec::new(),
            );
            im_da
                .update_buffer(drift.as_slice())
                .map_err(|e| anyhow!("encoding drift time: {e}"))?;
            im_da.unit = Unit::Millisecond;
            arrays.add(im_da);
        }

        // The precursor, from the scan's own SET_MASS and COLLISION_ENERGY items (what pwiz reads;
        // `SpectrumList_Waters.cpp:276-330`); see `precursor` for what is and is not stated.
        let precursor = if fi.ms_level > 1 {
            self.precursor(func, scan, fi)
        } else {
            None
        };
        if fi.ms_level > 1 && precursor.is_none() {
            static PRECURSOR_GAP_SAID: std::sync::Once = std::sync::Once::new();
            PRECURSOR_GAP_SAID.call_once(|| {
                log::warn!(
                    "Waters MassLynx: the scan-item API is unavailable in this MassLynxRaw.dll; \
                     MS2 rows carry no precursor"
                );
            });
        }
        let mut descr = SpectrumDescription {
            // ProteoWizard Waters native-id convention (1-based function/scan); for a frame the scan
            // is the MassLynx scan (pwiz's per-bin ids count `bins × scan + bin`).
            id: format!("function={} process=0 scan={}", func + 1, scan + 1),
            index: i,
            ms_level: fi.ms_level,
            // From the vendor's per-function flag, not assumed — unknown stays unknown.
            signal_continuity: match fi.continuum {
                Some(true) => SignalContinuity::Profile,
                Some(false) => SignalContinuity::Centroid,
                None => SignalContinuity::Unknown,
            },
            polarity: match fi.ion_mode.as_deref().and_then(|m| m.chars().last()) {
                Some('+') => ScanPolarity::Positive,
                Some('-') => ScanPolarity::Negative,
                _ => ScanPolarity::Unknown,
            },
            ..Default::default()
        };
        // No blanket `MS:1000294 "mass spectrum"` here (0.9.13): mzdata's `spectrum_type()` is a
        // first-match lookup and that parent term would shadow the writer's MS1/MSn inference.
        if let Some((lo, hi)) = drift_range {
            descr.add_param(
                Param::builder()
                    .name("lowest observed ion mobility")
                    .curie(mzdata::curie!(MS:1003439))
                    .value(lo as f64)
                    .unit(Unit::Millisecond)
                    .build(),
            );
            descr.add_param(
                Param::builder()
                    .name("highest observed ion mobility")
                    .curie(mzdata::curie!(MS:1003440))
                    .value(hi as f64)
                    .unit(Unit::Millisecond)
                    .build(),
            );
        }
        let mut event = ScanEvent { start_time: rt_minutes as f64, ..Default::default() };
        if let Some((x, y)) = self.imaging.as_ref().and_then(|im| im.position(i)) {
            event.add_param(Param::builder().name("position x").curie(mzdata::curie!(IMS:1000050)).value(x).build());
            event.add_param(Param::builder().name("position y").curie(mzdata::curie!(IMS:1000051)).value(y).build());
        }
        if let Some((lo, hi)) = fi.mass_range {
            event.scan_windows.push(ScanWindow::new(lo, hi));
        }
        // The function number, where pwiz puts it (the writer maps MS:1000616 to its own column).
        event.add_param(
            Param::builder()
                .name("preset scan configuration")
                .curie(mzdata::curie!(MS:1000616))
                .value((func + 1) as i64)
                .build(),
        );
        // A frame has no single drift time: `ion_mobility_value` stays NULL on purpose (pwiz's
        // combined spectra carry a meaningless mid-range value; TDF frames carry none).
        descr.acquisition.scans.push(event);
        if let Some(p) = precursor {
            descr.precursor.push(p);
        }

        Ok(MultiLayerSpectrum::new(descr, Some(arrays), None, None))
    }

    /// The precursor MassLynx states for one MSn scan, or `None` when the scan-item API is absent.
    fn precursor(&self, func: c_int, scan: c_int, fi: &FunctionInfo) -> Option<Precursor> {
        let api = self.scan_items?;
        let ids: Vec<c_int> = [self.item_ids.set_mass, self.item_ids.collision_energy].into_iter().flatten().collect();
        let vals = api.values(self.info_reader, func, scan, &ids);
        let get = |want: Option<c_int>| ids.iter().position(|&i| Some(i) == want).and_then(|k| vals.get(k).cloned().flatten());
        let set_mass = get(self.item_ids.set_mass).and_then(|v| v.trim().parse::<f64>().ok()).unwrap_or(0.0);
        let energy = get(self.item_ids.collision_energy).and_then(|v| v.trim().parse::<f64>().ok()).map(f64::abs).unwrap_or(0.0);
        let mut activation = Activation::default();
        // No Waters instrument has a trap collision cell (pwiz's note): beam-type CID.
        activation.methods_mut().push(DissociationMethodTerm::BeamTypeCollisionInducedDissociation);
        // The per-scan COLLISION_ENERGY item (what pwiz writes as MS:1000045; on a Synapt it is the
        // trap cell's energy) — and, when the method ramps the transfer cell for this function (MSe's
        // elevated energy), the ramp under its own terms.
        if energy > 0.0 {
            activation.energy = energy as f32;
        }
        if let Some((start, end)) = fi.ce_ramp {
            activation.add_param(Param::builder().name("collision energy ramp start").curie(mzdata::curie!(MS:1002013)).value(start).unit(Unit::Electronvolt).build());
            activation.add_param(Param::builder().name("collision energy ramp end").curie(mzdata::curie!(MS:1002014)).value(end).unit(Unit::Electronvolt).build());
        }
        // A set mass names the selected ion and the isolation target; its WIDTH is not stated by
        // the acquisition: the DDA processor's quad-isolation-window parameters (keys 1900/1901)
        // are read by the DLL from an optional `_dda.inf` sidecar that Waters' post-acquisition
        // tooling writes from user-entered offsets — absent on every run seen, so 0/0 (probe round
        // 23; research 2026-09-09). A set mass of 0 is MSe: the quadrupole ran non-resolving over
        // the acquisition range and nothing in the file, the method text or the DLL states any
        // narrower window (getFunction/IndexPrecursorMassRange and getPrecursorMass answer only
        // for SONAR). So the MSe row states the acquisition range as its isolation window —
        // target = midpoint, bounds = the range — the numbers ProteoWizard writes (its author calls
        // them a placeholder; Skyline recomputes the MSe window itself, DIA-Umpire ignores it, and
        // OpenSWATH / MSFragger-DIA need a numeric centre, which is why a NULL window is the one
        // shape that breaks something). A parameter on the activation names the source. No
        // selected ion: nothing was selected. The PSI DIA recommendation's marker for this case,
        // MS:1003159 "no isolation" (= "isolation window full range"), belongs on the window's own
        // parameter list, which mzdata's IsolationWindow cannot carry — BACKLOG.
        let (ions, isolation_window) = if set_mass > 0.0 {
            (
                vec![SelectedIon { mz: set_mass, ..Default::default() }],
                IsolationWindow::new(set_mass as f32, 0.0, 0.0, IsolationWindowState::Complete),
            )
        } else if let Some((lo, hi)) = fi.mass_range.filter(|(lo, hi)| hi > lo) {
            activation.add_param(Param::new_key_value("isolation window source", "acquisition mass range (MSe: no quadrupole isolation; ProteoWizard's convention)"));
            (Vec::new(), IsolationWindow::new((lo + hi) / 2.0, lo, hi, IsolationWindowState::Complete))
        } else {
            (Vec::new(), IsolationWindow::default())
        };
        Some(Precursor { ions, isolation_window, activation, ..Default::default() })
    }

    /// One drift bin of one scan, copied out of the reader-owned buffers.
    fn read_bin(&self, g: ReadDriftScanFn, func: c_int, scan: c_int, bin: c_int) -> Result<(Vec<f64>, Vec<f32>)> {
        let mut p_masses: *mut f32 = ptr::null_mut();
        let mut p_intensities: *mut f32 = ptr::null_mut();
        let mut n: c_int = 0;
        let rc = unsafe { g(self.scan_reader, func, scan, bin, &mut p_masses, &mut p_intensities, &mut n) };
        if rc != 0 {
            bail!("MassLynx readDriftScan(func={func}, scan={scan}, bin={bin}) failed (rc={rc})");
        }
        Ok(copy_points(p_masses, p_intensities, n)?)
    }

    /// The drift-summed scan (`readScan`), copied out of the reader-owned buffers.
    fn read_summed(&self, func: c_int, scan: c_int) -> Result<(Vec<f64>, Vec<f32>)> {
        let mut p_masses: *mut f32 = ptr::null_mut();
        let mut p_intensities: *mut f32 = ptr::null_mut();
        let mut n: c_int = 0;
        let rc = unsafe {
            (self.read_scan)(self.scan_reader, func, scan, &mut p_masses, &mut p_intensities, &mut n)
        };
        if rc != 0 {
            bail!("MassLynx readScan(func={func}, scan={scan}) failed (rc={rc})");
        }
        copy_points(p_masses, p_intensities, n)
    }
}

/// The REFERENCE (lock-mass) functions the method text names, 0-based:
/// `Function Parameters - Function 3 - REFERENCE` in `_extern.inf`.
fn reference_functions_from_extern_inf(raw: &Path) -> Vec<c_int> {
    let Ok(text) = crate::run_metadata::read_text_lossy(&raw.join("_extern.inf")) else { return Vec::new() };
    let mut out = Vec::new();
    for line in text.lines() {
        let l = line.trim();
        if let Some(rest) = l.strip_prefix("Function Parameters - Function ") {
            let mut parts = rest.splitn(2, " - ");
            if let (Some(num), Some(kind)) = (parts.next(), parts.next()) {
                if kind.trim().eq_ignore_ascii_case("REFERENCE") {
                    if let Ok(n) = num.trim().parse::<c_int>() {
                        out.push(n - 1);
                    }
                }
            }
        }
    }
    out
}

/// Copy `n` points out of MassLynx's reader-owned buffers (valid only until the next read; NOT to be
/// freed — `releaseMemory` on them corrupts the heap). m/z widened f32 → f64.
fn copy_points(p_masses: *mut f32, p_intensities: *mut f32, n: c_int) -> Result<(Vec<f64>, Vec<f32>)> {
    if n < 0 || n > MAX_WATERS_SPECTRUM_POINTS {
        bail!("MassLynx read returned implausible point count {n}");
    }
    if n > 0 && (p_masses.is_null() || p_intensities.is_null()) {
        bail!("MassLynx read returned {n} points but a NULL buffer");
    }
    let n = n as usize;
    if n == 0 {
        return Ok((Vec::new(), Vec::new()));
    }
    let mz = unsafe { std::slice::from_raw_parts(p_masses, n) }.iter().map(|&x| x as f64).collect();
    let intensity = unsafe { std::slice::from_raw_parts(p_intensities, n) }.to_vec();
    Ok((mz, intensity))
}

/// PROBE (lever `MZPC_WATERS_PROBE_QUAD=1|2|3|4`, one level per process): does the DLL state a
/// quadrupole isolation / transmission window for MSe and DDA functions? Level 1 = the info-reader
/// exports whose shapes the public SDK bindings declare (`getFunctionPrecursorMassRange(info, f,
/// *lo, *hi)`, `getIndexPrecursorMassRange(info, f, idx, *lo, *hi)`, `getPrecursorMass(info, f, idx,
/// *mass)`, `getAcquisitionMassRange(info, f, which, *lo, *hi)` for which = 0..3); level 2 = the DDA
/// processor (`createRawProcessor(&p, DDA = 7, NULL, NULL)`, `setRawReader(p, scanReader)`,
/// `getQuadIsolationWindowParameters(p, params)`, `getDDAParameters(p, params)` — every key/value
/// dumped); level 3 = level 2 + `ddaGetScanCount(p, *n)` and `ddaGetScanInfo(p, idx, params)` for
/// the first scans; level 4 = the MSE processor type (8) with the same parameter getters; level 5
/// = `getAcquisitionInfo(info, params)` dumped. Every
/// call is announced before it runs so a crash log names it. Results go to the log only.
fn probe_quad_windows(lib: &Library, info: *mut c_void, scan_reader: *mut c_void, n_functions: usize, index: &[(c_int, c_int, f32)], scan_items: Option<ScanItemApi>, level: &str) {
    type FnRangeFn = unsafe extern "C" fn(*mut c_void, c_int, *mut f32, *mut f32) -> c_int;
    type IdxRangeFn = unsafe extern "C" fn(*mut c_void, c_int, c_int, *mut f32, *mut f32) -> c_int;
    type IdxMassFn = unsafe extern "C" fn(*mut c_void, c_int, c_int, *mut f32) -> c_int;
    type CreateProcFn = unsafe extern "C" fn(*mut *mut c_void, c_int, *const c_void, *const c_void) -> c_int;
    type ProcReaderFn = unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_int;
    type ProcParamsFn = unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_int;
    type ProcCountFn = unsafe extern "C" fn(*mut c_void, *mut c_int) -> c_int;
    type ProcIdxParamsFn = unsafe extern "C" fn(*mut c_void, c_int, *mut c_void) -> c_int;
    type DestroyProcFn = unsafe extern "C" fn(*mut c_void) -> c_int;
    let say = |m: String| log::warn!("[probe-quad L{level}] {m}");
    let last_scan = |f: usize| index.iter().filter(|e| e.0 == f as c_int).map(|e| e.1).max().unwrap_or(0);
    let dump = |api: &ScanItemApi, p: *mut c_void, what: &str| {
        let mut keys: *const c_int = ptr::null();
        let mut n: c_int = 0;
        let rc = unsafe { (api.keys)(p, &mut keys, &mut n) };
        if rc != 0 || keys.is_null() || !(0..=4096).contains(&n) {
            say(format!("{what}: getParameterKeys rc={rc} n={n}"));
            return;
        }
        let ks = unsafe { std::slice::from_raw_parts(keys, n as usize) }.to_vec();
        let kv: Vec<String> = ks.iter().map(|&k| format!("{k}={:?}", api.string(p, k))).collect();
        say(format!("{what}: {} keys: {}", ks.len(), kv.join(" | ")));
    };
    match level {
        "1" => {
            let fr = unsafe { lib.get::<FnRangeFn>(b"getFunctionPrecursorMassRange\0") }.ok().map(|g| *g);
            let ir = unsafe { lib.get::<IdxRangeFn>(b"getIndexPrecursorMassRange\0") }.ok().map(|g| *g);
            let pm = unsafe { lib.get::<IdxMassFn>(b"getPrecursorMass\0") }.ok().map(|g| *g);
            let ar = unsafe { lib.get::<IdxRangeFn>(b"getAcquisitionMassRange\0") }.ok().map(|g| *g);
            say(format!("exports: functionPrecursorMassRange={} indexPrecursorMassRange={} precursorMass={} acquisitionMassRange={}", fr.is_some(), ir.is_some(), pm.is_some(), ar.is_some()));
            for f in 0..n_functions as c_int {
                let last = last_scan(f as usize);
                if let Some(g) = fr {
                    let (mut lo, mut hi) = ([f32::NAN; 4], [f32::NAN; 4]);
                    say(format!("calling getFunctionPrecursorMassRange(f={f})"));
                    let rc = unsafe { g(info, f, lo.as_mut_ptr(), hi.as_mut_ptr()) };
                    say(format!("function {} getFunctionPrecursorMassRange rc={rc} lo={} hi={}", f + 1, lo[0], hi[0]));
                }
                if let Some(g) = ir {
                    for idx in [0, 1, last] {
                        let (mut lo, mut hi) = ([f32::NAN; 4], [f32::NAN; 4]);
                        say(format!("calling getIndexPrecursorMassRange(f={f}, idx={idx})"));
                        let rc = unsafe { g(info, f, idx, lo.as_mut_ptr(), hi.as_mut_ptr()) };
                        say(format!("function {} scan {idx} getIndexPrecursorMassRange rc={rc} lo={} hi={}", f + 1, lo[0], hi[0]));
                    }
                }
                if let Some(g) = pm {
                    for idx in [0, 1, last] {
                        let mut m = [f32::NAN; 4];
                        say(format!("calling getPrecursorMass(f={f}, idx={idx})"));
                        let rc = unsafe { g(info, f, idx, m.as_mut_ptr()) };
                        say(format!("function {} scan {idx} getPrecursorMass rc={rc} mass={}", f + 1, m[0]));
                    }
                }
                if let Some(g) = ar {
                    for which in 0..4 {
                        let (mut lo, mut hi) = ([f32::NAN; 4], [f32::NAN; 4]);
                        say(format!("calling getAcquisitionMassRange(f={f}, which={which})"));
                        let rc = unsafe { g(info, f, which, lo.as_mut_ptr(), hi.as_mut_ptr()) };
                        say(format!("function {} getAcquisitionMassRange(which={which}) rc={rc} lo={} hi={}", f + 1, lo[0], hi[0]));
                    }
                }
            }
        }
        "2" | "3" | "4" => {
            let Some(api) = scan_items else {
                say("parameters API unavailable; nothing to read".into());
                return;
            };
            let (Some(create), Some(set_reader), Some(destroy)) = (
                unsafe { lib.get::<CreateProcFn>(b"createRawProcessor\0") }.ok().map(|g| *g),
                unsafe { lib.get::<ProcReaderFn>(b"setRawReader\0") }.ok().map(|g| *g),
                unsafe { lib.get::<DestroyProcFn>(b"destroyRawProcessor\0") }.ok().map(|g| *g),
            ) else {
                say("processor exports missing".into());
                return;
            };
            let quad = unsafe { lib.get::<ProcParamsFn>(b"getQuadIsolationWindowParameters\0") }.ok().map(|g| *g);
            let dda_params = unsafe { lib.get::<ProcParamsFn>(b"getDDAParameters\0") }.ok().map(|g| *g);
            let count = unsafe { lib.get::<ProcCountFn>(b"ddaGetScanCount\0") }.ok().map(|g| *g);
            let info_fn = unsafe { lib.get::<ProcIdxParamsFn>(b"ddaGetScanInfo\0") }.ok().map(|g| *g);
            let ptype: c_int = if level == "4" { 8 } else { 7 };
            let mut proc_: *mut c_void = ptr::null_mut();
            say(format!("calling createRawProcessor(type={ptype})"));
            let rc = unsafe { create(&mut proc_, ptype, ptr::null(), ptr::null()) };
            say(format!("createRawProcessor rc={rc} handle_null={}", proc_.is_null()));
            if rc != 0 || proc_.is_null() {
                return;
            }
            say("calling setRawReader(proc, scanReader)".into());
            let rc = unsafe { set_reader(proc_, scan_reader) };
            say(format!("setRawReader rc={rc}"));
            if let Some(g) = quad {
                say("calling getQuadIsolationWindowParameters(proc, params)".into());
                api.with(|p| unsafe { g(proc_, p) }, |p| dump(&api, p, "getQuadIsolationWindowParameters"))
                    .unwrap_or_else(|| say("getQuadIsolationWindowParameters: rc != 0 (or no parameters object)".into()));
            }
            if let Some(g) = dda_params {
                say("calling getDDAParameters(proc, params)".into());
                api.with(|p| unsafe { g(proc_, p) }, |p| dump(&api, p, "getDDAParameters"))
                    .unwrap_or_else(|| say("getDDAParameters: rc != 0".into()));
            }
            if level == "3" {
                if let Some(g) = count {
                    let mut n = [0 as c_int; 4];
                    say("calling ddaGetScanCount(proc, *n)".into());
                    let rc = unsafe { g(proc_, n.as_mut_ptr()) };
                    say(format!("ddaGetScanCount rc={rc} n={}", n[0]));
                    if let (Some(gi), true) = (info_fn, rc == 0 && n[0] > 0) {
                        for idx in 0..n[0].min(4) {
                            say(format!("calling ddaGetScanInfo(proc, {idx}, params)"));
                            api.with(|p| unsafe { gi(proc_, idx, p) }, |p| dump(&api, p, &format!("ddaGetScanInfo({idx})")))
                                .unwrap_or_else(|| say(format!("ddaGetScanInfo({idx}): rc != 0")));
                        }
                    }
                }
            }
            say("calling destroyRawProcessor".into());
            let rc = unsafe { destroy(proc_) };
            say(format!("destroyRawProcessor rc={rc}"));
        }
        "5" => {
            // `getAcquisitionInfo(info, params)` (v5 SDK: argtypes [c_void_p, c_void_p]; keys 1650..):
            // run-level acquisition facts, said to include a PRECURSOR_MASS_START/END pair.
            let Some(api) = scan_items else {
                say("parameters API unavailable; nothing to read".into());
                return;
            };
            let Some(g) = unsafe { lib.get::<ProcParamsFn>(b"getAcquisitionInfo\0") }.ok().map(|g| *g) else {
                say("getAcquisitionInfo export missing".into());
                return;
            };
            say("calling getAcquisitionInfo(info, params)".into());
            api.with(|p| unsafe { g(info, p) }, |p| dump(&api, p, "getAcquisitionInfo"))
                .unwrap_or_else(|| say("getAcquisitionInfo: rc != 0".into()));
        }
        other => say(format!("unknown level {other:?} (use 1, 2, 3, 4 or 5)")),
    }
}

/// What ProteoWizard makes of a MassLynx function type (`Reader_Waters_Detail.cpp`
/// `translateFunctionType`), keyed by the CODE `getFunctionType` returns (pwiz's enum + 200; the
/// strings the DLL prints for them are `MS SIR DLY CAT OFF PAR MSMS NL NG MRM Q1F MS2 DAD TOF PSD
/// TOFS TOFD MTOF "TOF MS" "TOF P" ASP…`, measured from the binary).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FunctionKind {
    /// A mass spectrum at this MS level.
    Ms(u8),
    /// SIR / MRM-family data: chromatograms, never spectra.
    Chromatogram(&'static str),
    /// No mass spectrum at all (diode array, delay, off, AutoSpec scan types).
    NotMs(&'static str),
}

impl FunctionKind {
    fn from_code(code: c_int) -> Option<Self> {
        Some(match code - 200 {
            0 => Self::Ms(1),                                   // MS
            1 => Self::Chromatogram("SIR (selected ion recording)"),
            2 => Self::NotMs("a delay function"),               // DLY
            3 => Self::NotMs("a concatenated function"),        // CAT
            4 => Self::NotMs("off"),                            // OFF
            5 => Self::Ms(1),                                   // PAR (precursor-ion scan: pwiz labels MS1)
            6 => Self::Ms(2),                                   // MSMS
            7 => Self::Chromatogram("a constant-neutral-loss function"),
            8 => Self::Chromatogram("a constant-neutral-gain function"),
            9 => Self::Chromatogram("MRM"),
            10 => Self::Ms(1),                                  // Q1F
            11 => Self::Ms(2),                                  // MS2
            12 => Self::NotMs("a diode-array (wavelength) function"),
            13 => Self::Ms(1),                                  // TOF
            14 => Self::NotMs("a TOF PSD function"),
            15 => Self::Ms(1),                                  // TOFS (survey)
            16 => Self::Ms(2),                                  // TOFD (TOF daughter / MS-MS)
            17 => Self::Ms(1),                                  // MTOF
            18 => Self::Ms(1),                                  // TOF MS (+ the MSe rule)
            19 => Self::Ms(1),                                  // TOF P (parent)
            20..=23 => Self::NotMs("an AutoSpec voltage/magnet scan"),
            24 => Self::Ms(2),                                  // QUAD AUTO DAU
            25..=27 | 30 => Self::NotMs("an AutoSpec scan type"),
            28 => Self::Chromatogram("AutoSpec MIKES"),
            29 | 31 => Self::Chromatogram("AutoSpec MRM"),
            _ => return None,
        })
    }
}

/// MS level from the vendor's function-type CODE, with ProteoWizard's MSe convention
/// (`SpectrumList_Waters.cpp:161-185`): a product-ion type is MS2; among MS functions the SECOND
/// function is the elevated-energy acquisition of an MSe / HDMSe method when it repeats the first
/// function's type, ion mode and mass range, is not the lock-mass function, and its first scan
/// carries a collision energy > 0 (pwiz's test; when the energy is unreadable the repeated
/// type/mode/range decides). Every other MS function (lock-mass reference, auxiliary) is MS1.
/// What each MassLynx function is and what became of it: the functions not written as spectra
/// (chromatogram-type or non-MS, and collapsed retention-time summaries with whether the lever wrote
/// them), SONAR functions written as their drift-summed scan, and the lock-mass reference. Through
/// 0.11.5 these facts lived only in `waters_drift`, which a run without drift bins never gets, so a
/// non-IMS archive recorded none of them. Free of the DLL so every host tests it.
fn functions_block(
    functions: &[FunctionInfo],
    skipped: &[(c_int, String)],
    collapsed: &[(c_int, Option<c_int>)],
    keep_collapsed: bool,
    lockmass_function: Option<c_int>,
    sonar_checked: bool,
) -> serde_json::Value {
    serde_json::json!({
        "vendor": "waters",
        "source": "MassLynxRaw getFunctionType / getDriftScanCount / scan item Sonar Enabled / getLockMassFunction",
        "functions": functions.iter().enumerate().map(|(f, fi)| serde_json::json!({
            "function": f + 1,
            "type": fi.type_string,
            "ms_level": fi.ms_level,
            "drift_bins": fi.drift_bins,
            "sonar": fi.sonar,
            "sonar_bins_summed": fi.sonar_bins,
        })).collect::<Vec<_>>(),
        "skipped_functions": skipped.iter().map(|(f, why)| serde_json::json!({"function": f + 1, "reason": why})).collect::<Vec<_>>(),
        "collapsed_functions": collapsed.iter().map(|(f, of)| serde_json::json!({"function": f + 1, "summary_of": of.map(|o| o + 1), "written": keep_collapsed})).collect::<Vec<_>>(),
        "lockmass_function": lockmass_function.map(|f| f + 1),
        "sonar_checked": sonar_checked,
    })
}

/// The `transformations` entry a Waters run's function table declares: `waters:drop-functions` when
/// a function was not written as spectra (chromatogram-type or non-MS, its scan count unreadable, or
/// a collapsed retention-time summary the lever did not keep). A SONAR function's summed scans are
/// counted as they are read instead ([`SONAR_SUMMED`]), so an archive declares the sum only when it
/// holds such a scan.
fn function_transformations(
    skipped: &[(c_int, String)],
    collapsed: &[(c_int, Option<c_int>)],
    keep_collapsed: bool,
) -> Vec<String> {
    let mut out = Vec::new();
    if !skipped.is_empty() || (!collapsed.is_empty() && !keep_collapsed) {
        out.push("waters:drop-functions".to_string());
    }
    out
}

/// The `transformations` entry for a written scan that [`WatersReader::spectrum`] read as a SONAR
/// function's quadrupole bins summed, declared from [`WatersReader::sonar_counter`].
pub const SONAR_SUMMED: &str = "waters:sonar-summed";

/// Sort a frame's points by (m/z, drift time), the monotone main axis the chunked layout needs.
/// `true` when that changed their order (the bins' own m/z axes interleaved), which is what the
/// archive's `sort-by-mz` declares; a frame with one populated bin is already in order. Free of the
/// DLL so every host tests it.
fn sort_frame_points(points: &mut [(f64, f32, f32)]) -> bool {
    let order = |a: &(f64, f32, f32), b: &(f64, f32, f32)| a.0.total_cmp(&b.0).then(a.2.total_cmp(&b.2));
    if points.is_sorted_by(|a, b| order(a, b).is_le()) {
        return false;
    }
    points.sort_unstable_by(order);
    true
}

fn ms_level_for(f: usize, functions: &[FunctionInfo], lockmass: Option<c_int>) -> u8 {
    let fi = &functions[f];
    match fi.type_code.and_then(FunctionKind::from_code) {
        Some(FunctionKind::Ms(2)) => 2,
        Some(FunctionKind::Ms(_)) => {
            let first = &functions[0];
            let mse_pair = f == 1
                && lockmass != Some(1)
                && first.type_code == fi.type_code
                && first.ion_mode == fi.ion_mode
                && first.mass_range == fi.mass_range
                && match fi.collision_energy_0 {
                    Some(ce) => ce > 0.0,
                    None => true,
                };
            if mse_pair {
                2
            } else {
                1
            }
        }
        Some(_) => 1, // skipped before the index is built
        // No type information at all (export missing or the call failed): MS1, said out loud —
        // guessing MS2 from the function's position invented precursorless MS2 rows.
        None => {
            log::warn!("MassLynx function {}: type unknown (getFunctionType unavailable or failed); written as MS1", f + 1);
            1
        }
    }
}

/// `_extern.inf`: per-function "Transfer Collision Energy Ramp Start (eV) 20.0" / "… End (eV) 50.0"
/// (Synapt) or "MS Collision Energy Low (eV) 65.0" / "… High (eV) 75.0" (Xevo MSe),
/// keyed by 0-based function. The section headers read `Function Parameters - Function N - <kind>`.
fn method_ce_ramps_from_extern_inf(raw: &Path) -> std::collections::HashMap<c_int, (f64, f64)> {
    let mut out = std::collections::HashMap::new();
    let Ok(text) = crate::run_metadata::read_text_lossy(&raw.join("_extern.inf")) else { return out };
    let mut current: Option<c_int> = None;
    let mut start: Option<f64> = None;
    let mut end: Option<f64> = None;
    let flush = |current: Option<c_int>, start: &mut Option<f64>, end: &mut Option<f64>, out: &mut std::collections::HashMap<c_int, (f64, f64)>| {
        if let (Some(f), Some(a), Some(b)) = (current, *start, *end) {
            out.insert(f, (a, b));
        }
        *start = None;
        *end = None;
    };
    for line in text.lines() {
        let l = line.trim();
        if let Some(rest) = l.strip_prefix("Function Parameters - Function ") {
            flush(current, &mut start, &mut end, &mut out);
            current = rest.split(' ').next().and_then(|n| n.parse::<c_int>().ok()).map(|n| n - 1);
            continue;
        }
        let number = |s: &str| s.split_whitespace().last().and_then(|v| v.parse::<f64>().ok());
        // Synapt HDMSe methods: "Transfer Collision Energy Ramp Start/End (eV)"; Xevo MSe methods
        // (TOF PARENT FUNCTION): "MS Collision Energy Low (eV)" / "MS Collision Energy High (eV)".
        if l.starts_with("Transfer Collision Energy Ramp Start") || l.starts_with("MS Collision Energy Low") {
            start = number(l);
        } else if l.starts_with("Transfer Collision Energy Ramp End") || l.starts_with("MS Collision Energy High") {
            end = number(l);
        }
    }
    flush(current, &mut start, &mut end, &mut out);
    out
}

impl Drop for WatersReader {
    fn drop(&mut self) {
        unsafe {
            (self.destroy)(self.scan_reader);
            (self.destroy)(self.info_reader);
        }
    }
}

/// Resolve the directory holding `MassLynxRaw.dll`. `MZPC_MASSLYNX_DIR` wins; otherwise reuse
/// `MZPC_PWIZ_DIR` (the same pwiz-bin that carries Clearcore2 also carries MassLynxRaw) — unifying
/// the env-var convention across the SciEX / Waters native paths.
fn resolve_masslynx_dir() -> Result<PathBuf> {
    if let Some(d) = std::env::var_os("MZPC_MASSLYNX_DIR") {
        return Ok(PathBuf::from(d));
    }
    if let Some(d) = std::env::var_os("MZPC_PWIZ_DIR") {
        return Ok(PathBuf::from(d));
    }
    bail!(
        "neither MZPC_MASSLYNX_DIR nor MZPC_PWIZ_DIR is set; point one at a ProteoWizard pwiz-bin \
         directory containing MassLynxRaw.dll (+ cdt.dll)"
    )
}

/// Prepend `dir` to the process `PATH` so the vendor DLL's dependency DLLs resolve at load time.
fn prepend_dir_to_path(dir: &Path) {
    let mut new_path = OsString::from(dir);
    if let Some(old) = std::env::var_os("PATH") {
        new_path.push(if cfg!(windows) { ";" } else { ":" });
        new_path.push(old);
    }
    // SAFETY: set during conversion startup, before any worker threads read PATH.
    unsafe { std::env::set_var("PATH", new_path) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn laser_position_items_are_found_by_name() {
        let u = |n: &str| n.to_ascii_uppercase().replace(['_', '-'], " ");
        assert!(names_position(&u("LASERAIM_XPOS"), 'X') && !names_position(&u("LASERAIM_XPOS"), 'Y'));
        assert!(names_position(&u("LASERAIM_YPOS"), 'Y'));
        assert!(names_position(&u("Laser Aim X Position"), 'X'));
        assert!(!names_position(&u("RAMP MAX POS"), 'X'), "MAX POS is not an x position");
        assert!(!names_position(&u("COLLISION ENERGY"), 'X'));
    }

    /// Review 2026-09-30 B15: the items were looked up in function 1 only; a run recording them in
    /// a later function lost its positions. The lock-mass function is not asked.
    #[test]
    fn laser_items_are_looked_up_in_every_written_function() {
        let index = [(0, 0, 0.1), (2, 0, 0.12), (1, 0, 0.15), (0, 1, 0.2)];
        let table = |f: c_int| match f {
            0 => vec![462],
            1 => vec![462, 409, 410],
            _ => vec![409, 410],
        };
        let name = |ids: &[c_int]| -> Vec<(c_int, String)> {
            ids.iter().map(|&i| (i, match i { 409 => "LASERAIM_XPOS", 410 => "LASERAIM_YPOS", _ => "COLLISION_ENERGY" }.to_string())).collect()
        };
        let mut asked = Vec::new();
        let found = laser_items(&index, Some(2), |f| {
            asked.push(f);
            table(f)
        }, name);
        assert_eq!(found, Some(([409, 410], ("LASERAIM_XPOS".to_string(), "LASERAIM_YPOS".to_string()))));
        assert_eq!(asked, [0, 1], "every written function but the lock mass");
        // Only x named: no position.
        assert_eq!(laser_items(&index, None, |_| vec![409, 462], name), None);
        // No names readable at all: the SDK enum's ids.
        assert_eq!(laser_items(&index, None, table, |_| Vec::new()).map(|l| l.0), Some([409, 410]));
    }

    /// Review 2026-09-30 B15: the lock-mass (reference) function's scans sample the lock spray; they
    /// get no position and are never read for one.
    #[test]
    fn lock_mass_scans_get_no_position() {
        let index = [(0, 0, 0.1), (2, 0, 0.15), (0, 1, 0.2)];
        let mut asked = Vec::new();
        let mm = laser_positions(&index, Some(2), |f, scan| {
            asked.push((f, scan));
            Some((f as f64, scan as f64))
        });
        assert_eq!(mm, [Some((0.0, 0.0)), None, Some((0.0, 1.0))]);
        assert_eq!(asked, [(0, 0), (0, 1)]);
    }

    #[test]
    fn the_method_declares_the_raster_step() {
        // MTBLS14771's methodfile.xml, abridged.
        let xml = "<?xml version=\"1.0\"?>\r\n<MsMethod InstrumentType=\"QTof\" InstrumentModel=\"Select Series MRT\" Version=\"1.0\">\r\n    <Settings>\r\n        <Setting Name=\"StartMass\" Value=\"50\"/>\r\n        <Setting Name=\"DesiXStart\" Value=\"79.9552\" Mapping=\"Desi.Pattern.XStart\"/>\r\n        <Setting Name=\"DesiXStep\" Value=\"0.1\" Mapping=\"Desi.Pattern.XStep\"/>\r\n        <Setting Name=\"DesiXRate\" Value=\"66.6667\" Mapping=\"Desi.Pattern.XRate\"/>\r\n        <Setting Name=\"DesiYStep\" Value=\"0.1\" Mapping=\"Desi.Pattern.YStep\"/>\r\n    </Settings>\r\n</MsMethod>\r\n";
        assert_eq!(declared_steps(xml), [Some(("DesiXStep".to_string(), 0.1)), Some(("DesiYStep".to_string(), 0.1))]);
        // Any prefix; a value that is no step is passed over.
        let other = r#"<S><Setting Name="LaserXStep" Value="x"/><Setting Name="MaldiXStep" Value="0.05"/></S>"#;
        assert_eq!(declared_steps(other), [Some(("MaldiXStep".to_string(), 0.05)), None]);
        // Review 2026-09-30: names ending in `yStep` / `xStep` are no raster steps, and must not
        // stand in for the DESI steps that follow them.
        let others = r#"<S><Setting Name="CollisionEnergyStep" Value="2"/><Setting Name="MaxStep" Value="4"/><Setting Name="LaserDelayStep" Value="1.5"/><Setting Name="DesiXStep" Value="0.1"/><Setting Name="DesiYStep" Value="0.1"/></S>"#;
        assert_eq!(declared_steps(others), [Some(("DesiXStep".to_string(), 0.1)), Some(("DesiYStep".to_string(), 0.1))]);
        assert_eq!(declared_steps(""), [None, None]);
    }

    /// The shape of the DESI run MTBLS14771: `rows` × `cols` positions 0.1 mm apart as f32, in
    /// raster order.
    fn desi(rows: usize, cols: usize) -> Vec<Option<(f64, f64)>> {
        (0..rows)
            .flat_map(|r| (0..cols).map(move |c| Some(((80.3673f32 + c as f32 * 0.1) as f64, (45.9005f32 + r as f32 * 0.1) as f64))))
            .collect()
    }

    fn laser_names() -> (String, String) {
        ("LASERAIM_XPOS".into(), "LASERAIM_YPOS".into())
    }

    fn step(name: &str, mm: f64) -> Option<(String, f64)> {
        Some((name.to_string(), mm))
    }

    fn accessions(s: &mzdata::meta::ScanSettings) -> Vec<String> {
        s.params.iter().map(|p| p.curie().unwrap().to_string()).collect()
    }

    #[test]
    fn the_declared_step_is_the_pitch_and_its_source_is_recorded() {
        let im = WatersImaging::from_positions(desi(103, 104), laser_names(), [step("DesiXStep", 0.1), step("DesiYStep", 0.1)], 0).unwrap();
        let g = im.grid.as_ref().unwrap();
        assert_eq!((g.x.count, g.y.count, g.x.pitch, g.y.pitch, g.off_grid), (104, 103, Some(0.1), Some(0.1), 0));
        assert_eq!((im.position(0), im.position(104 * 103 - 1)), (Some((1, 1)), Some((104, 103))));
        let b = im.block();
        assert_eq!(b["x"]["step_source"], "declared: methodfile.xml DesiXStep");
        assert_eq!(b["declared_steps_mm"][1]["setting"], "DesiYStep");
        assert_eq!(im.transformations(), [LASER_GRID]);
        // A step larger than the whole raster (a µm value read as mm) is not taken.
        let im = WatersImaging::from_positions(desi(10, 10), laser_names(), [step("MaldiXStep", 50.0), step("MaldiYStep", 50.0)], 0).unwrap();
        let g = im.grid.as_ref().unwrap();
        assert_eq!((g.x.count, g.x.pitch, g.x.declared), (10, Some(0.1), false));
        assert!(im.block()["x"]["step_source"].as_str().unwrap().starts_with("fitted: the declared methodfile.xml MaldiXStep"));
    }

    /// Review 2026-09-30 B15: a scan off the grid (parked at the stage origin before the raster)
    /// took imaging away from the whole run. Now it loses its pixel, counted and declared — and does
    /// not stretch the y grid, on which it lies by chance (459 steps below the first row).
    #[test]
    fn a_parked_scan_loses_its_pixel_and_is_declared() {
        let mut mm = desi(103, 104);
        mm.insert(0, Some((0.0, 0.0)));
        let im = WatersImaging::from_positions(mm, laser_names(), [None, None], 0).unwrap();
        let g = im.grid.as_ref().unwrap();
        assert_eq!((g.x.count, g.y.count, g.off_grid), (104, 103, 1));
        assert_eq!((im.position(0), im.position(1)), (None, Some((1, 1))));
        assert_eq!(im.transformations(), [LASER_GRID, OFF_GRID_DROPPED]);
        assert_eq!(im.block()["off_grid_scans_dropped"], 1);
        // On both grids by chance (the raster starts at 80.3 mm): it stretched them to 907 × 562
        // pixels, dropping nothing, declared step or not (review 2026-09-30).
        let mut mm: Vec<Option<(f64, f64)>> = desi(103, 104).into_iter().map(|p| p.map(|(x, y)| (x - 0.0673, y))).collect();
        mm.insert(0, Some((0.0, 0.0)));
        for steps in [[None, None], [step("DesiXStep", 0.1), step("DesiYStep", 0.1)]] {
            let im = WatersImaging::from_positions(mm.clone(), laser_names(), steps, 0).unwrap();
            let g = im.grid.as_ref().unwrap();
            assert_eq!((g.x.count, g.y.count, g.off_grid), (104, 103, 1));
            assert_eq!((im.position(0), im.position(1)), (None, Some((1, 1))));
        }
    }

    /// Review 2026-09-30 B14: when the positions fit no grid, the archive says so — a
    /// `waters_imaging` block with the reason — and writes no positions and no grid.
    #[test]
    fn positions_on_no_grid_leave_the_reason_and_nothing_else() {
        let mut mm = desi(10, 10);
        mm.push(Some((0.0, 0.0)));
        mm.push(Some((1.0, 1.0)));
        let im = WatersImaging::from_positions(mm, laser_names(), [None, None], 3).unwrap();
        assert!(im.grid.is_err());
        assert!(im.position(0).is_none() && im.scan_settings().is_none());
        let b = im.block();
        assert!(b["no_grid"].as_str().unwrap().contains("no positions and no imaging marker"), "{b}");
        assert_eq!((b["scans_with_laser_position"].as_u64(), b["lockmass_scans_excluded"].as_u64()), (Some(102), Some(3)));
        assert!(b.get("x").is_none());
        // Fewer than two distinct positions: no raster, not an imaging run at all.
        assert!(WatersImaging::from_positions(vec![Some((1.0, 1.0)); 5], laser_names(), [None, None], 0).is_none());
        // A continuum of positions (20000 over 1 × 1 mm, gaps under 1 µm) is no 1 × 1 grid, which
        // marked it imaging with every scan at (1, 1) (review 2026-09-30, second pass).
        let mut seed = 9u64;
        let mut u = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 11) as f64 / (1u64 << 53) as f64
        };
        let walk: Vec<Option<(f64, f64)>> = (0..20_000).map(|_| Some((10.0 + u(), 20.0 + u()))).collect();
        assert!(WatersImaging::from_positions(walk, laser_names(), [None, None], 0).unwrap().grid.is_err());
        // Two regions rastered on lattices 30 µm apart in x (not a whole step), no step declared:
        // no lattice holds both — refused, not a wrong grid.
        let mut two = desi(20, 30);
        two.extend(desi(20, 30).into_iter().map(|p| p.map(|(x, y)| (x + 3.03, y))));
        let im = WatersImaging::from_positions(two, laser_names(), [None, None], 0).unwrap();
        assert!(im.grid.is_err() && im.position(0).is_none() && im.scan_settings().is_none());
        assert!(im.block()["no_grid"].as_str().unwrap().starts_with("the x positions lie on no raster"), "{}", im.block());
        // Four strays in 304 scans (1.3 %) lose the 0.1 mm grid, and the pitch no longer slides to
        // the fraction of it that seats them: 320 columns of 9.09 µm (review 2026-09-30, fourth pass).
        let mut strays = desi(10, 30);
        let at = |c: f32, d: f64| Some(((80.3673f32 + c * 0.1) as f64 + d, (45.9005f32 + 3.0 * 0.1) as f64));
        strays.extend([at(6.0, 0.0091), at(6.0, 0.0182), at(13.0, 0.0091), at(13.0, 0.0182)]);
        let im = WatersImaging::from_positions(strays.clone(), laser_names(), [None, None], 0).unwrap();
        assert!(im.grid.is_err() && im.position(0).is_none() && im.scan_settings().is_none(), "{}", im.block());
        let steps = [Some(("DesiXStep".to_string(), 0.1)), Some(("DesiYStep".to_string(), 0.1))];
        let g = WatersImaging::from_positions(strays, laser_names(), steps, 0).unwrap().grid.unwrap();
        assert_eq!((g.x.pitch, g.x.count, g.y.count), (Some(0.1), 30, 10));
    }

    /// Review 2026-09-30 B14/B15, second pass: without a declared step a spot array (4 × 4 spots
    /// of 5 × 5 pixels at a 12-pixel pitch) became 4 × 4 pixels of 1.2 mm; and a small region on the
    /// grid far off the raster (a QC region, 0.9 % of the scans) lost every position as if parked.
    #[test]
    fn small_and_far_regions_keep_the_step() {
        let spot = |s: i64, k: i64, from: f32| Some((from + (s * 12 + k) as f32 * 0.1) as f64);
        let spots: Vec<Option<(f64, f64)>> = (0..4)
            .flat_map(|sy| (0..5).flat_map(move |r| (0..4).flat_map(move |sx| (0..5).map(move |c| spot(sx, c, 30.0).zip(spot(sy, r, 20.0))))))
            .collect();
        let im = WatersImaging::from_positions(spots.clone(), laser_names(), [None, None], 0).unwrap();
        let g = im.grid.as_ref().unwrap();
        assert_eq!((g.x.count, g.y.count, g.x.pitch, g.y.pitch, g.off_grid), (41, 41, Some(0.1), Some(0.1), 0));
        assert_eq!((im.position(1), im.position(5), im.position(399)), (Some((2, 1)), Some((13, 1)), Some((41, 41))));
        let mut mm = desi(103, 104);
        mm.extend((0..8).flat_map(|r| (0..12).map(move |c| Some(((80.3673f32 + (150 + c) as f32 * 0.1) as f64, (45.9005f32 + (20 + r) as f32 * 0.1) as f64)))));
        for steps in [[None, None], [step("DesiXStep", 0.1), step("DesiYStep", 0.1)]] {
            let im = WatersImaging::from_positions(mm.clone(), laser_names(), steps, 0).unwrap();
            let g = im.grid.as_ref().unwrap();
            assert_eq!((g.x.count, g.y.count, g.off_grid), (162, 103, 0));
            assert_eq!((im.position(0), im.position(mm.len() - 1)), (Some((1, 1)), Some((162, 28))));
            assert_eq!(im.transformations(), [LASER_GRID]);
        }
    }

    /// Review 2026-09-30 B15: a single row dropped the pixel size of the axis whose step is known.
    #[test]
    fn a_single_row_keeps_the_column_step() {
        let row: Vec<Option<(f64, f64)>> = (0..50).map(|c| Some((10.0 + c as f64 * 0.05, 20.0))).collect();
        let im = WatersImaging::from_positions(row.clone(), laser_names(), [None, None], 0).unwrap();
        let s = im.scan_settings().unwrap();
        assert_eq!(accessions(&s), ["IMS:1000042", "IMS:1000043", "IMS:1000046", "IMS:1000044"]);
        assert!((s.params[2].value.to_f64().unwrap() - 50.0).abs() < 1e-9);
        assert_eq!(im.block()["y"]["step_source"], "none: a single row or column");
        // With the method's y step, both axes have a pixel size.
        let im = WatersImaging::from_positions(row, laser_names(), [None, step("DesiYStep", 0.05)], 0).unwrap();
        let s = im.scan_settings().unwrap();
        assert_eq!(accessions(&s), ["IMS:1000042", "IMS:1000043", "IMS:1000046", "IMS:1000044", "IMS:1000047", "IMS:1000045"]);
    }

    /// The real DESI run (MetaboLights MTBLS14771): `_func001.sts` holds each scan's laser aim
    /// position (a u16 header size at byte 0, record size at 4, item count at 6; 48-byte item
    /// descriptors from 0x20: u16 code, type (3 = f32), offset; codes 9 / 10 = x / y in mm), and
    /// `methodfile.xml` its 0.1 mm steps. Declared or fitted, the grid is 104 × 103 at 0.1 mm.
    #[test]
    #[ignore = "needs MTBLS14771's .raw under ~/Claude/mzPeak/data/imaging-examples; run with --include-ignored"]
    fn mtbls14771_fits_104_by_103_at_a_tenth_of_a_millimetre() {
        let raw = PathBuf::from(std::env::var("HOME").unwrap())
            .join("Claude/mzPeak/data/imaging-examples/MTBLS14771/20250327_MG3_HBackupACN_CLMC.raw");
        let sts = std::fs::read(raw.join("_func001.sts")).expect("_func001.sts");
        let u16_at = |o: usize| u16::from_le_bytes([sts[o], sts[o + 1]]) as usize;
        let (header, record, items) = (u16_at(0), u16_at(4), u16_at(6));
        let offset = |code| {
            (0..items).map(|i| 0x20 + 48 * i).find(|&d| u16_at(d) == code && u16_at(d + 2) == 3).map(|d| u16_at(d + 4)).unwrap()
        };
        let (ox, oy) = (offset(9), offset(10));
        let f32_at = |o: usize| f32::from_le_bytes(sts[o..o + 4].try_into().unwrap()) as f64;
        let mm: Vec<Option<(f64, f64)>> =
            (0..(sts.len() - header) / record).map(|s| header + s * record).map(|r| Some((f32_at(r + ox), f32_at(r + oy)))).collect();
        assert_eq!(mm.len(), 104 * 103);
        let steps = declared_steps(&std::fs::read_to_string(raw.join("methodfile.xml")).unwrap());
        assert_eq!(steps, [step("DesiXStep", 0.1), step("DesiYStep", 0.1)]);
        for (steps, declared) in [(steps, true), ([None, None], false)] {
            let im = WatersImaging::from_positions(mm.clone(), laser_names(), steps, 0).unwrap();
            let g = im.grid.as_ref().unwrap();
            assert_eq!((g.x.count, g.y.count, g.x.pitch, g.y.pitch, g.off_grid), (104, 103, Some(0.1), Some(0.1), 0));
            assert_eq!((g.x.declared, g.y.declared), (declared, declared));
            assert!(g.x.max_residual < 1e-4 && g.y.max_residual < 1e-4, "{g:?}");
        }
    }

    fn fi(code: Option<c_int>, bins: c_int, ce: Option<f64>) -> FunctionInfo {
        FunctionInfo { type_code: code, drift_bins: bins, collision_energy_0: ce, ion_mode: Some("ES+".into()), mass_range: Some((50.0, 600.0)), ..Default::default() }
    }

    #[test]
    fn ms_levels_follow_the_function_type_code_and_the_mse_convention() {
        const TOF_MS: c_int = 218;
        const TOFD: c_int = 216;
        // Capan2: six TOF MS functions, the second with a collision energy, the third the lock-mass
        // reference → 1, 2, 1, 1, 1, 1 (pwiz's labels).
        let mut fs: Vec<FunctionInfo> = (0..6).map(|_| fi(Some(TOF_MS), 200, Some(4.0))).collect();
        assert_eq!((0..6).map(|f| ms_level_for(f, &fs, Some(2))).collect::<Vec<_>>(), vec![1, 2, 1, 1, 1, 1]);
        // The second function at collision energy 0 is not MSe.
        fs[1].collision_energy_0 = Some(0.0);
        assert_eq!(ms_level_for(1, &fs, None), 1);
        // The second function being the lock-mass function (PXD059353: MS + REFERENCE) is never MS2.
        fs[1].collision_energy_0 = Some(4.0);
        assert_eq!(ms_level_for(1, &fs, Some(1)), 1);
        // A polarity-switching pair is not MSe.
        fs[1].ion_mode = Some("ES-".into());
        assert_eq!(ms_level_for(1, &fs, None), 1);
        // HDDDA (PXD073126): TOF MS survey + TOFD product-ion functions + REFERENCE → 1, 2, 2, 1.
        let fs = vec![fi(Some(TOF_MS), 200, Some(4.0)), fi(Some(TOFD), 200, None), fi(Some(TOFD), 200, None), fi(Some(TOF_MS), 200, Some(4.0))];
        assert_eq!((0..4).map(|f| ms_level_for(f, &fs, Some(3))).collect::<Vec<_>>(), vec![1, 2, 2, 1]);
        // MSMS, MS2, QUAD AUTO DAU are product-ion kinds; MRM/SIR/NL/NG are chromatograms; DAD is not MS.
        assert_eq!(FunctionKind::from_code(206), Some(FunctionKind::Ms(2)));
        assert_eq!(FunctionKind::from_code(211), Some(FunctionKind::Ms(2)));
        assert_eq!(FunctionKind::from_code(224), Some(FunctionKind::Ms(2)));
        assert!(matches!(FunctionKind::from_code(209), Some(FunctionKind::Chromatogram(_))));
        assert!(matches!(FunctionKind::from_code(201), Some(FunctionKind::Chromatogram(_))));
        assert!(matches!(FunctionKind::from_code(207), Some(FunctionKind::Chromatogram(_))));
        assert!(matches!(FunctionKind::from_code(212), Some(FunctionKind::NotMs(_))));
        // No type information: MS1 everywhere (with a warning), never a positional MS2 guess.
        let fs = vec![fi(None, 0, None), fi(None, 0, None), fi(None, 0, None)];
        assert_eq!((0..3).map(|f| ms_level_for(f, &fs, None)).collect::<Vec<_>>(), vec![1, 1, 1]);
    }

    /// A run without drift bins still records its functions: a skipped SIR function, a SONAR
    /// function written as its summed scan and the lock mass. Before, all of it sat behind
    /// `has_drift()` and a non-IMS archive recorded none of it.
    #[test]
    fn functions_block_and_entries_do_not_need_drift_bins() {
        const TOF_MS: c_int = 218;
        const SIR: c_int = 209;
        let mut fs = vec![fi(Some(TOF_MS), 0, None), fi(Some(SIR), 0, None), fi(Some(TOF_MS), 0, Some(4.0))];
        fs[2].sonar = true;
        fs[2].sonar_bins = 200;
        let skipped = vec![(1, "a chromatogram".to_string())];
        let block = functions_block(&fs, &skipped, &[], false, Some(0), true);
        assert_eq!(block["skipped_functions"][0]["function"], 2);
        assert_eq!(block["skipped_functions"][0]["reason"], "a chromatogram");
        assert_eq!(block["functions"][2]["sonar_bins_summed"], 200);
        assert_eq!(block["lockmass_function"], 1);
        // The function table declares the drop only: a SONAR function's summed scans are counted as
        // they are read (`sonar_counter`), so a table holding one declares nothing for it.
        assert_eq!(function_transformations(&skipped, &[], false), ["waters:drop-functions"]);
        // Nothing dropped: no entry.
        let plain = vec![fi(Some(TOF_MS), 0, None)];
        assert!(function_transformations(&[], &[], false).is_empty());
        // Collapsed summaries: dropped unless the lever kept them, and `written` says which.
        let collapsed = vec![(3, Some(0))];
        assert_eq!(function_transformations(&[], &collapsed, false), ["waters:drop-functions"]);
        assert!(function_transformations(&[], &collapsed, true).is_empty());
        assert_eq!(functions_block(&plain, &[], &collapsed, false, None, false)["collapsed_functions"][0]["written"], false);
        assert_eq!(functions_block(&plain, &[], &collapsed, true, None, false)["collapsed_functions"][0]["written"], true);
    }

    /// `sort-by-mz` on a frame follows what the sort did: interleaved bins are re-ordered and
    /// counted, a frame already in (m/z, drift time) order is not. Before, every run with drift bins
    /// declared it whether or not a frame moved.
    #[test]
    fn a_frame_counts_as_re_sorted_only_when_its_order_changed() {
        let mut one_bin = vec![(100.0, 1.0, 2.0), (200.0, 1.0, 2.0), (300.0, 4.0, 2.0)];
        assert!(!sort_frame_points(&mut one_bin));
        let mut interleaved = vec![(100.0, 1.0, 2.0), (300.0, 2.0, 2.0), (150.0, 3.0, 2.5), (250.0, 4.0, 2.5)];
        assert!(sort_frame_points(&mut interleaved));
        let order: Vec<(f64, f32)> = interleaved.iter().map(|p| (p.0, p.1)).collect();
        assert_eq!(order, [(100.0, 1.0), (150.0, 3.0), (250.0, 4.0), (300.0, 2.0)]);
        // The same m/z in two bins: drift time orders them, and a sorted frame stays unmoved.
        let mut tie = vec![(100.0, 1.0, 3.0), (100.0, 2.0, 2.0)];
        assert!(sort_frame_points(&mut tie));
        assert_eq!(tie[0].2, 2.0);
        assert!(!sort_frame_points(&mut tie));
    }

    #[test]
    fn method_ramps_and_reference_functions_parse_from_extern_inf() {
        let dir = std::env::temp_dir().join(format!("mzpc-waters-extern-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("_extern.inf"), "Function Parameters - Function 1 - MOBILITY MS FUNCTION\r\nUsing Auto Transfer Collision Energy (eV)\t2.000000\r\nFunction Parameters - Function 2 - MOBILITY MS FUNCTION\r\nTransfer Collision Energy Ramp Start (eV)\t20.0\r\nTransfer Collision Energy Ramp End (eV)\t50.0\r\nFunction Parameters - Function 3 - REFERENCE\r\nTrap Collision Energy (eV)\t4.0\r\nFunction Parameters - Function 4 - TOF PARENT FUNCTION\r\nRamp High Energy from\t\t\t\t65.0 to 75.0\r\n[COLLISION ENERGY]\r\nMS Collision Energy Low (eV)\t\t\t65.0\r\nMS Collision Energy High (eV)\t\t\t75.0\r\n").unwrap();
        let ramps = method_ce_ramps_from_extern_inf(&dir);
        assert_eq!(ramps.get(&1), Some(&(20.0, 50.0)));
        assert!(ramps.get(&0).is_none() && ramps.get(&2).is_none());
        assert_eq!(ramps.get(&3), Some(&(65.0, 75.0)), "Xevo MSe key names");
        assert_eq!(reference_functions_from_extern_inf(&dir), vec![2]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
