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
            if sonar && drift_bins > 0 {
                log::warn!(
                    "MassLynx function {}: SONAR — its {} bins are quadrupole positions, not drift times; \
                     written as the summed scan (SONAR support pending)",
                    f + 1,
                    drift_bins
                );
                drift_bins = 0;
            }
            let ce_ramp = method_ramps.get(&f).copied();
            functions.push(FunctionInfo { continuum, type_code, type_string, ion_mode, mass_range, drift_bins, sonar, collision_energy_0, ce_ramp, ms_level: 1 });
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
        for (f, fi) in functions.iter().enumerate() {
            if let Some(kind) = fi.type_code.and_then(FunctionKind::from_code) {
                if let FunctionKind::Chromatogram(what) | FunctionKind::NotMs(what) = kind {
                    log::warn!(
                        "MassLynx function {} ({}) is {what}; not written as spectra (ProteoWizard writes SIR/MRM as chromatograms and skips the rest)",
                        f + 1,
                        fi.type_string.as_deref().unwrap_or("?")
                    );
                    skipped_functions.push(f as c_int);
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
        let keep_collapsed = std::env::var_os("MZPC_WATERS_KEEP_COLLAPSED").is_some_and(|v| !v.is_empty() && v != "0");
        let mut rt_failures = 0usize;
        let mut acquired_ims_functions: Vec<c_int> = Vec::new();
        for f in 0..n_functions {
            if skipped_functions.contains(&f) {
                continue;
            }
            let Some(n_scans) = int_of(Some(read_scan_count), f).filter(|n| *n >= 0) else {
                log::warn!("MassLynx getScanCount(function {}) failed; the function is skipped", f + 1);
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
        };
        Ok(reader)
    }

    pub fn len(&self) -> usize {
        self.index.len()
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
            "collapsed_functions": self.collapsed.iter().map(|(f, of)| serde_json::json!({"function": f + 1, "summary_of": of.map(|o| o + 1), "written": std::env::var_os("MZPC_WATERS_KEEP_COLLAPSED").is_some()})).collect::<Vec<_>>(),
            "ccs_calibration_mob_cal_csv": ccs,
        }))
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
            points.sort_unstable_by(|a, b| a.0.total_cmp(&b.0).then(a.2.total_cmp(&b.2)));
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
        // A set mass names the selected ion and the isolation target; its WIDTH is not stated
        // anywhere in the file or the DLL (the DDA processor's quad-isolation-window parameters
        // come back 0/0 — caller-supplied, not acquired; probe round 23). A set mass of 0 is MSe:
        // the quadrupole transmitted the whole acquisition range, and nothing in the file states
        // any narrower window (getFunction/IndexPrecursorMassRange and getPrecursorMass fail on
        // every MSe and DDA function; they answer only for SONAR). So the MSe row states the
        // acquisition range as its isolation window — target = midpoint, bounds = the range —
        // exactly what ProteoWizard writes, so consumers that key all-ion data on that window
        // (Skyline, DIA-Umpire, …) see the same thing; a parameter on the activation says where the
        // window came from. No selected ion: nothing was selected.
        let (ions, isolation_window) = if set_mass > 0.0 {
            (
                vec![SelectedIon { mz: set_mass, ..Default::default() }],
                IsolationWindow { target: set_mass as f32, lower_bound: 0.0, upper_bound: 0.0, flags: IsolationWindowState::Complete },
            )
        } else if let Some((lo, hi)) = fi.mass_range.filter(|(lo, hi)| hi > lo) {
            activation.add_param(Param::new_key_value("isolation window source", "acquisition mass range (MSe: no quadrupole isolation; ProteoWizard's convention)"));
            (Vec::new(), IsolationWindow { target: (lo + hi) / 2.0, lower_bound: lo, upper_bound: hi, flags: IsolationWindowState::Complete })
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
/// the first scans; level 4 = the MSE processor type (8) with the same parameter getters. Every
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
        other => say(format!("unknown level {other:?} (use 1, 2, 3 or 4)")),
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

/// `_extern.inf`: per-function "Transfer Collision Energy Ramp Start (eV) 20.0" / "… End (eV) 50.0",
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
        if l.starts_with("Transfer Collision Energy Ramp Start") {
            start = number(l);
        } else if l.starts_with("Transfer Collision Energy Ramp End") {
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

    #[test]
    fn method_ramps_and_reference_functions_parse_from_extern_inf() {
        let dir = std::env::temp_dir().join(format!("mzpc-waters-extern-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("_extern.inf"), "Function Parameters - Function 1 - MOBILITY MS FUNCTION\r\nUsing Auto Transfer Collision Energy (eV)\t2.000000\r\nFunction Parameters - Function 2 - MOBILITY MS FUNCTION\r\nTransfer Collision Energy Ramp Start (eV)\t20.0\r\nTransfer Collision Energy Ramp End (eV)\t50.0\r\nFunction Parameters - Function 3 - REFERENCE\r\nTrap Collision Energy (eV)\t4.0\r\n").unwrap();
        let ramps = method_ce_ramps_from_extern_inf(&dir);
        assert_eq!(ramps.get(&1), Some(&(20.0, 50.0)));
        assert!(ramps.get(&0).is_none() && ramps.get(&2).is_none());
        assert_eq!(reference_functions_from_extern_inf(&dir), vec![2]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
