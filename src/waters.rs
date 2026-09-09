//! Native Waters MassLynx `.raw` reader via the vendor **C API** (`MassLynxRaw.dll`), loaded with
//! `libloading`.
//!
//! WHY NOT A C# GLUE: unlike SciEX Clearcore2 / Agilent MHDAC (managed .NET, reached by reflection),
//! `MassLynxRaw.dll` is a **native C++ library exporting a C interface** — reflection can't touch it.
//! So this binds the C exports directly, the same pattern as the Bruker timsdata reader.
//!
//! The C API (verified empirically on the workstation + cross-checked against the public MassLynx
//! SDK ctypes binding):
//!   * `createRawReaderFromPath(path, &reader, type)` — `type`: SCAN=1, INFO=2 (CHROM=3, ANALOG=4).
//!   * `getFunctionCount(infoReader, &n)` / `getScanCount(infoReader, func, &n)`.
//!   * `readScan(scanReader, func, scan, &masses, &intensities, &n)` — allocates two `float[n]`
//!     arrays the caller frees with `releaseMemory`.
//!   * `destroyRawReader(reader)`. All return `int` (0 = OK).
//!
//! RUNTIME: Windows + the MassLynx DLLs (`MassLynxRaw.dll` + deps `cdt.dll`, … — bundled in a
//! ProteoWizard install). Point `MZPC_MASSLYNX_DIR` (or `MZPC_PWIZ_DIR`) at that directory. We
//! prepend it to `PATH` before loading so `MassLynxRaw.dll`'s *dependency* DLLs (notably `cdt.dll`,
//! the compressed-scan decoder that `readScan` needs) resolve — otherwise data reads access-violate
//! even though the reader opens fine.

use std::ffi::{CString, OsString, c_char, c_int, c_void};
use std::path::{Path, PathBuf};
use std::ptr;

use anyhow::{Context, Result, anyhow, bail};
use libloading::Library;

use mzdata::params::Unit;
use mzdata::spectrum::bindata::{ArrayType, BinaryArrayMap, BinaryDataArrayType, DataArray};
use mzdata::spectrum::{
    MultiLayerSpectrum, ScanEvent, ScanPolarity, SignalContinuity, SpectrumDescription,
};

/// Guard against a corrupt/hostile vendor library returning an enormous length that would exhaust
/// memory before we copy it. 100M points × (8 + 4) bytes ≈ 1.2 GiB.
const MAX_WATERS_SPECTRUM_POINTS: c_int = 100_000_000;

const ML_TYPE_SCAN: c_int = 1;
const ML_TYPE_INFO: c_int = 2;

// MassLynx C exports. On x86_64 Windows there is a single calling convention, so `extern "C"` is the
// correct (and only) ABI. Reader handles are opaque `void*`.
type CreateFromPathFn = unsafe extern "C" fn(*const c_char, *mut *mut c_void, c_int) -> c_int;
type DestroyReaderFn = unsafe extern "C" fn(*mut c_void) -> c_int;
type GetFunctionCountFn = unsafe extern "C" fn(*mut c_void, *mut c_int) -> c_int;
type GetScanCountFn = unsafe extern "C" fn(*mut c_void, c_int, *mut c_int) -> c_int;
/// `isContinuum(infoReader, function, *out bool)`. Same shape as `getScanCount`: handle, function
/// index, out-param, non-zero return on failure. Exported by `MassLynxRaw.dll` (verified in its PE
/// export table alongside `getFunctionType` / `getFunctionTypeString`).
type IsContinuumFn = unsafe extern "C" fn(*mut c_void, c_int, *mut bool) -> c_int;
type ReadScanFn =
    unsafe extern "C" fn(*mut c_void, c_int, c_int, *mut *mut f32, *mut *mut f32, *mut c_int)
        -> c_int;
// Exports of the same DLL that the lane did not bind until 2026-09-09 (all present in the export
// table of MassLynxRaw.dll 4.9.0.0; signatures INFERRED from the shapes above and from the calls
// ProteoWizard's C++ wrapper makes — `MZPC_WATERS_PROBE=1` validates them against known values
// before anything is written from them). `(infoReader, function, out)` / `(infoReader, function,
// scan|bin, out)` shapes; every call returns int, 0 = OK.
/// `getDriftScanCount(infoReader, function, *out int)` — drift bins per scan of an IMS function.
type GetIntPerFunctionFn = unsafe extern "C" fn(*mut c_void, c_int, *mut c_int) -> c_int;
/// `getDriftTime(infoReader, function, bin, *out float ms)` / `getRetentionTime(infoReader, function, scan, *out float min)`.
type GetFloatPerScanFn = unsafe extern "C" fn(*mut c_void, c_int, c_int, *mut f32) -> c_int;
/// Candidate 5-argument shape of `getDriftTime`: `(reader, function, scan, bin, *out float ms)` — the
/// 4-argument call access-violates (probe round 10), which is what a missing integer argument
/// looks like on x64 (the out-pointer lands in the bin slot).
type GetFloatPerScanBinFn = unsafe extern "C" fn(*mut c_void, c_int, c_int, c_int, *mut f32) -> c_int;
/// `getAcquisitionMassRange(infoReader, function, *out float lo, *out float hi)`.
type GetMassRangeFn = unsafe extern "C" fn(*mut c_void, c_int, *mut f32, *mut f32) -> c_int;
/// `getFunctionTypeString(infoReader, function, *out char*)` / `getIonModeString(...)`. The string
/// is copied out and NEVER released (ownership unknown; see the `readScan` note below).
type GetStringPerFunctionFn = unsafe extern "C" fn(*mut c_void, c_int, *mut *const c_char) -> c_int;
/// `getScanItemsInFunction(infoReader, function, *out int*, *out int n)` — the MassLynxScanItem ids
/// a function records; `getScanItemName(infoReader, item, *out char*)` names one.
type GetScanItemsFn = unsafe extern "C" fn(*mut c_void, c_int, *mut *const c_int, *mut c_int) -> c_int;
type GetScanItemNameFn = unsafe extern "C" fn(*mut c_void, c_int, *mut *const c_char) -> c_int;
/// `getScanItemValue(infoReader, function, scan, item, *out char*)`.
type GetScanItemValueFn = unsafe extern "C" fn(*mut c_void, c_int, c_int, c_int, *mut *const c_char) -> c_int;
/// `readDriftScan(scanReader, function, scan, bin, *out float*, *out float*, *out int n)` — ONE
/// drift bin of one scan, where `readScan` returns the drift-summed spectrum.
type ReadDriftScanFn = unsafe extern "C" fn(
    *mut c_void,
    c_int,
    c_int,
    c_int,
    *mut *mut f32,
    *mut *mut f32,
    *mut c_int,
) -> c_int;

/// A native Waters `.raw` reader. Holds the loaded library, the resolved C function pointers, the
/// info + scan reader handles, and the flattened `(function, scan)` spectrum index.
pub struct WatersReader {
    // `_lib` MUST outlive the function pointers + handles below (dropped together with this struct).
    _lib: Library,
    read_scan: ReadScanFn,
    /// Per-function continuum flag, resolved once at open. `None` when the export is unavailable.
    continuum: Vec<Option<bool>>,
    destroy: DestroyReaderFn,
    info_reader: *mut c_void,
    scan_reader: *mut c_void,
    /// One entry per spectrum: (function index, scan index), both 0-based.
    index: Vec<(c_int, c_int)>,
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
        let read_scan_count: GetScanCountFn = *unsafe { lib.get(b"getScanCount\0") }
            .context("resolving MassLynx export getScanCount")?;
        let read_scan: ReadScanFn =
            *unsafe { lib.get(b"readScan\0") }.context("resolving MassLynx export readScan")?;
        // OPTIONAL: older MassLynx builds may not export it. Absent, continuity falls back to the
        // previous behaviour (profile) rather than failing the whole conversion.
        let is_continuum: Option<IsContinuumFn> =
            unsafe { lib.get::<IsContinuumFn>(b"isContinuum\0") }.ok().map(|f| *f);
        if is_continuum.is_none() {
            log::warn!(
                "MassLynxRaw.dll does not export isContinuum; every function will be labelled \
                 profile, which is wrong for centroided functions"
            );
        }

        let path_str = input
            .to_str()
            .ok_or_else(|| anyhow!("Waters .raw path is not valid UTF-8: {}", input.display()))?;
        let cpath = CString::new(path_str)
            .map_err(|_| anyhow!("Waters .raw path contains an interior NUL"))?;

        // Create the INFO reader (function/scan counts) and the SCAN reader (data).
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

        // Flatten (function, scan) into a 0-based spectrum index.
        let mut n_functions: c_int = 0;
        let rc = unsafe { get_function_count(info_reader, &mut n_functions) };
        if rc != 0 || n_functions < 0 {
            unsafe {
                destroy(scan_reader);
                destroy(info_reader);
            }
            bail!("MassLynx getFunctionCount failed (rc={rc}, n={n_functions})");
        }
        // Continuity is a per-FUNCTION property in MassLynx, so resolve it once per function rather
        // than per spectrum. It used to be hardcoded to Profile, which mislabelled every centroided
        // function -- the same defect that put Shimadzu centroid data in the profile facet.
        let mut continuum: Vec<Option<bool>> = Vec::new();
        for f in 0..n_functions {
            continuum.push(is_continuum.and_then(|func| {
                let mut flag = false;
                let rc = unsafe { func(info_reader, f, &mut flag) };
                (rc == 0).then_some(flag)
            }));
        }

        // Log what the vendor actually said. Without this, a working `isContinuum` and a silently
        // failing one are indistinguishable from the output whenever the run happens to be all
        // continuum -- which is the common case, and exactly how an unverified binding hides.
        log::info!(
            "MassLynx continuum flags by function: {}",
            continuum
                .iter()
                .enumerate()
                .map(|(f, c)| match c {
                    Some(true) => format!("{f}=continuum"),
                    Some(false) => format!("{f}=centroid"),
                    None => format!("{f}=unknown"),
                })
                .collect::<Vec<_>>()
                .join(" ")
        );

        let mut index = Vec::new();
        for f in 0..n_functions {
            let mut n_scans: c_int = 0;
            let rc = unsafe { read_scan_count(info_reader, f, &mut n_scans) };
            if rc != 0 || n_scans < 0 {
                continue; // skip a function we can't enumerate rather than abort the whole run
            }
            for s in 0..n_scans {
                index.push((f, s));
            }
        }
        if index.is_empty() {
            unsafe {
                destroy(scan_reader);
                destroy(info_reader);
            }
            bail!("Waters .raw {} has no readable scans", input.display());
        }

        let reader = WatersReader {
            _lib: lib,
            read_scan,
            destroy,
            info_reader,
            scan_reader,
            index,
            continuum,
        };
        // `MZPC_WATERS_PROBE=1`: exercise the exports the drift/RT work needs and log what they
        // return, so their inferred C signatures can be checked against ProteoWizard's values for
        // the same file (Capan2 f1 scan1: RT 0.03705 min, drift bins 0.0 / 0.0392495 / … / 7.8107 ms,
        // bin 0 = 42 points, 200 bins per function). Diagnostics only; nothing is written from it.
        if std::env::var_os("MZPC_WATERS_PROBE").is_some_and(|v| !v.is_empty() && v != "0") {
            reader.probe(input, n_functions);
        }
        Ok(reader)
    }

    /// Log-only exercise of the not-yet-used exports (see `open`). Staged by `MZPC_WATERS_PROBE=N`
    /// — 1: integer/float out-params, 2: + string out-params, 3: + scan items, 4: + `readDriftScan` —
    /// and every call is announced BEFORE it is made, so an access violation names its culprit.
    fn probe(&self, input: &Path, n_functions: c_int) {
        let level: u8 = std::env::var("MZPC_WATERS_PROBE").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(1);
        let lib = &self._lib;
        let names: [&[u8]; 13] = [
            b"getDriftScanCount\0",
            b"getDriftTime\0",
            b"readDriftScan\0",
            b"getRetentionTime\0",
            b"getFunctionType\0",
            b"getFunctionTypeString\0",
            b"getIonMode\0",
            b"getIonModeString\0",
            b"getAcquisitionMassRange\0",
            b"getScanItemsInFunction\0",
            b"getScanItemName\0",
            b"getScanItemValue\0",
            b"getCollisionalCrossSection\0",
        ];
        for name in names {
            let present = unsafe { lib.get::<*const c_void>(name) }.is_ok();
            log::info!(
                "waters-probe: export {} {}",
                String::from_utf8_lossy(&name[..name.len() - 1]),
                if present { "present" } else { "ABSENT" }
            );
        }
        let drift_count: Option<GetIntPerFunctionFn> = unsafe { lib.get(b"getDriftScanCount\0") }.ok().map(|f| *f);
        let function_type: Option<GetIntPerFunctionFn> = unsafe { lib.get(b"getFunctionType\0") }.ok().map(|f| *f);
        let ion_mode: Option<GetIntPerFunctionFn> = unsafe { lib.get(b"getIonMode\0") }.ok().map(|f| *f);
        let drift_time: Option<GetFloatPerScanFn> = unsafe { lib.get(b"getDriftTime\0") }.ok().map(|f| *f);
        let retention_time: Option<GetFloatPerScanFn> = unsafe { lib.get(b"getRetentionTime\0") }.ok().map(|f| *f);
        let mass_range: Option<GetMassRangeFn> = unsafe { lib.get(b"getAcquisitionMassRange\0") }.ok().map(|f| *f);
        let type_string: Option<GetStringPerFunctionFn> = unsafe { lib.get(b"getFunctionTypeString\0") }.ok().map(|f| *f);
        let mode_string: Option<GetStringPerFunctionFn> = unsafe { lib.get(b"getIonModeString\0") }.ok().map(|f| *f);
        let scan_items: Option<GetScanItemsFn> = unsafe { lib.get(b"getScanItemsInFunction\0") }.ok().map(|f| *f);
        let item_name: Option<GetScanItemNameFn> = unsafe { lib.get(b"getScanItemName\0") }.ok().map(|f| *f);
        let item_value: Option<GetScanItemValueFn> = unsafe { lib.get(b"getScanItemValue\0") }.ok().map(|f| *f);
        let read_drift: Option<ReadDriftScanFn> = unsafe { lib.get(b"readDriftScan\0") }.ok().map(|f| *f);
        let cstr = |p: *const c_char| -> String {
            if p.is_null() {
                "<null>".into()
            } else {
                unsafe { std::ffi::CStr::from_ptr(p) }.to_string_lossy().chars().take(80).collect()
            }
        };
        // Out-params live in 16-byte slots: a callee that writes a double (or two floats) into what we
        // think is a float cannot corrupt a neighbour, and the raw bytes are logged. Every result is
        // logged the moment it is known — a later crash must not take it along.
        let dt_variant: u8 = std::env::var("MZPC_WATERS_DT_VARIANT").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(0);
        let drift_time5: Option<GetFloatPerScanBinFn> = unsafe { lib.get(b"getDriftTime\0") }.ok().map(|f| *f);
        for f in 0..n_functions {
            let cdt = input.join(format!("_func{:03}.cdt", f + 1)).is_file() || input.join(format!("_FUNC{:03}.CDT", f + 1)).is_file();
            log::info!("waters-probe: function {} cdt={cdt}", f + 1);
            let mut n_drift: c_int = 0;
            if let Some(g) = function_type {
                log::info!("waters-probe: calling getFunctionType(f={})", f + 1);
                let mut slot = [0i32; 4];
                let rc = unsafe { g(self.info_reader, f, slot.as_mut_ptr()) };
                log::info!("waters-probe: function {} type={} rc={rc} raw={:?}", f + 1, slot[0], slot);
            }
            if let Some(g) = ion_mode {
                log::info!("waters-probe: calling getIonMode(f={})", f + 1);
                let mut slot = [0i32; 4];
                let rc = unsafe { g(self.info_reader, f, slot.as_mut_ptr()) };
                log::info!("waters-probe: function {} ionMode={} rc={rc} raw={:?}", f + 1, slot[0], slot);
            }
            if let Some(g) = drift_count {
                log::info!("waters-probe: calling getDriftScanCount(f={})", f + 1);
                let mut slot = [0i32; 4];
                let rc = unsafe { g(self.info_reader, f, slot.as_mut_ptr()) };
                log::info!("waters-probe: function {} driftScanCount={} rc={rc} raw={:?}", f + 1, slot[0], slot);
                if rc == 0 {
                    n_drift = slot[0];
                }
            }
            if let Some(g) = mass_range {
                log::info!("waters-probe: calling getAcquisitionMassRange(f={})", f + 1);
                let mut lo = [0f32; 4];
                let mut hi = [0f32; 4];
                let rc = unsafe { g(self.info_reader, f, lo.as_mut_ptr(), hi.as_mut_ptr()) };
                log::info!("waters-probe: function {} massRange={}..{} rc={rc} rawLo={:?} rawHi={:?}", f + 1, lo[0], hi[0], lo, hi);
            }
            if let Some(g) = retention_time {
                for scan in [0, 1] {
                    log::info!("waters-probe: calling getRetentionTime(f={}, scan={})", f + 1, scan + 1);
                    let mut slot = [0f32; 4];
                    let rc = unsafe { g(self.info_reader, f, scan, slot.as_mut_ptr()) };
                    let as_f64 = f64::from_bits((slot[0].to_bits() as u64) | ((slot[1].to_bits() as u64) << 32));
                    log::info!("waters-probe: function {} rt[scan{}]={} (as f64 {as_f64}) rc={rc} raw={:?}", f + 1, scan + 1, slot[0], slot);
                }
            }
            // getDriftTime: the 4-argument (info, f, bin, *f32) form crashed in round 10. Variants, one
            // process each: 2 = (info, f, scan=0, bin, *f32); 3 = (scan reader, f, bin, *f32);
            // 4 = (scan reader, f, scan=0, bin, *f32); 1 = the original. Only on functions with bins.
            if n_drift > 0 && dt_variant > 0 {
                for bin in [0, 1, n_drift - 1] {
                    let mut slot = [0f32; 4];
                    log::info!("waters-probe: calling getDriftTime variant {dt_variant} (f={}, bin={bin})", f + 1);
                    let rc = match dt_variant {
                        1 => drift_time.map(|g| unsafe { g(self.info_reader, f, bin, slot.as_mut_ptr()) }),
                        2 => drift_time5.map(|g| unsafe { g(self.info_reader, f, 0, bin, slot.as_mut_ptr()) }),
                        3 => drift_time.map(|g| unsafe { g(self.scan_reader, f, bin, slot.as_mut_ptr()) }),
                        4 => drift_time5.map(|g| unsafe { g(self.scan_reader, f, 0, bin, slot.as_mut_ptr()) }),
                        _ => None,
                    };
                    let as_f64 = f64::from_bits((slot[0].to_bits() as u64) | ((slot[1].to_bits() as u64) << 32));
                    log::info!("waters-probe: function {} dt[bin{bin}] variant {dt_variant} = {} (as f64 {as_f64}) rc={rc:?} raw={:?}", f + 1, slot[0], slot);
                }
            }
            let nd = n_drift;
            if level >= 2 {
                if let Some(g) = type_string {
                    log::info!("waters-probe: calling getFunctionTypeString(f={})", f + 1);
                    let mut ps: [*const c_char; 4] = [ptr::null(); 4];
                    let rc = unsafe { g(self.info_reader, f, ps.as_mut_ptr()) };
                    log::info!("waters-probe: function {} typeString={:?} rc={rc}", f + 1, if rc == 0 { cstr(ps[0]) } else { String::new() });
                }
                if let Some(g) = mode_string {
                    log::info!("waters-probe: calling getIonModeString(f={})", f + 1);
                    let mut ps: [*const c_char; 4] = [ptr::null(); 4];
                    let rc = unsafe { g(self.info_reader, f, ps.as_mut_ptr()) };
                    log::info!("waters-probe: function {} ionModeString={:?} rc={rc}", f + 1, if rc == 0 { cstr(ps[0]) } else { String::new() });
                }
            }
            if level >= 3 {
                if let (Some(gi), Some(gn)) = (scan_items, item_name) {
                    log::info!("waters-probe: calling getScanItemsInFunction(f={})", f + 1);
                    let mut items: [*const c_int; 4] = [ptr::null(); 4];
                    let mut n_items = [0i32; 4];
                    let rc = unsafe { gi(self.info_reader, f, items.as_mut_ptr(), n_items.as_mut_ptr()) };
                    log::info!("waters-probe: function {} scan items rc={rc} n={} ptr_null={}", f + 1, n_items[0], items[0].is_null());
                    if rc == 0 && !items[0].is_null() && (0..=256).contains(&n_items[0]) {
                        let ids: Vec<c_int> = unsafe { std::slice::from_raw_parts(items[0], n_items[0] as usize) }.to_vec();
                        log::info!("waters-probe: function {} scan item ids {:?}", f + 1, ids);
                        let mut described = Vec::new();
                        for id in ids {
                            log::info!("waters-probe: calling getScanItemName(item={id})");
                            let mut pn: [*const c_char; 4] = [ptr::null(); 4];
                            let rcn = unsafe { gn(self.info_reader, id, pn.as_mut_ptr()) };
                            let name = if rcn == 0 { cstr(pn[0]) } else { format!("rc={rcn}") };
                            let value = item_value.map(|gv| {
                                log::info!("waters-probe: calling getScanItemValue(f={}, scan=1, item={id})", f + 1);
                                let mut pv: [*const c_char; 4] = [ptr::null(); 4];
                                let rcv = unsafe { gv(self.info_reader, f, 0, id, pv.as_mut_ptr()) };
                                if rcv == 0 { cstr(pv[0]) } else { format!("rc={rcv}") }
                            });
                            described.push(format!("{id}:{name}={}", value.unwrap_or_default()));
                        }
                        log::info!("waters-probe: function {} scan items (id:name=value@scan1): {}", f + 1, described.join(" | "));
                    }
                }
            }
            if level >= 4 {
                if let Some(g) = read_drift {
                    for bin in [0, 1, 199] {
                        log::info!("waters-probe: calling readDriftScan(f={}, scan=1, bin={bin})", f + 1);
                        let mut pm: *mut f32 = ptr::null_mut();
                        let mut pi: *mut f32 = ptr::null_mut();
                        let mut n = [-1i32; 4];
                        let rc = unsafe { g(self.scan_reader, f, 0, bin, &mut pm, &mut pi, n.as_mut_ptr()) };
                        let n0 = n[0];
                        if rc == 0 && n0 >= 0 && n0 <= MAX_WATERS_SPECTRUM_POINTS && !pm.is_null() && !pi.is_null() {
                            let m = unsafe { std::slice::from_raw_parts(pm, n0 as usize) };
                            let i = unsafe { std::slice::from_raw_parts(pi, n0 as usize) };
                            let tic: f64 = i.iter().map(|&x| x as f64).sum();
                            log::info!(
                                "waters-probe: readDriftScan(f={}, scan=1, bin={bin}) n={n0} tic={tic} first={:?}",
                                f + 1,
                                m.iter().zip(i).take(3).collect::<Vec<_>>()
                            );
                        } else {
                            log::info!("waters-probe: readDriftScan(f={}, scan=1, bin={bin}) rc={rc} n={n0}", f + 1);
                        }
                    }
                }
            }
        }
    }

    pub fn len(&self) -> usize {
        self.index.len()
    }

    /// Read one spectrum: `readScan` → m/z (f64, widened from the vendor's f32) + intensity (f32).
    pub fn spectrum(&self, i: usize) -> Result<MultiLayerSpectrum> {
        let (func, scan) = *self
            .index
            .get(i)
            .ok_or_else(|| anyhow!("Waters spectrum index {i} out of range (len {})", self.len()))?;

        let mut p_masses: *mut f32 = ptr::null_mut();
        let mut p_intensities: *mut f32 = ptr::null_mut();
        let mut n: c_int = 0;
        let rc = unsafe {
            (self.read_scan)(
                self.scan_reader,
                func,
                scan,
                &mut p_masses,
                &mut p_intensities,
                &mut n,
            )
        };
        if rc != 0 {
            bail!("MassLynx readScan(func={func}, scan={scan}) failed (rc={rc})");
        }
        if n < 0 || n > MAX_WATERS_SPECTRUM_POINTS {
            bail!("MassLynx readScan returned implausible point count {n}");
        }
        let n = n as usize;

        // The masses/intensities arrays are READER-OWNED — internal buffers valid until the next
        // readScan (or reader destroy), NOT caller-allocated. Copy out immediately; do NOT free them
        // (calling releaseMemory on them corrupts the heap — 0xC0000374). m/z widened f32→f64.
        let mz: Vec<f64> = if n > 0 && !p_masses.is_null() {
            unsafe { std::slice::from_raw_parts(p_masses, n) }
                .iter()
                .map(|&x| x as f64)
                .collect()
        } else {
            Vec::new()
        };
        let intensity: Vec<f32> = if n > 0 && !p_intensities.is_null() {
            unsafe { std::slice::from_raw_parts(p_intensities, n) }.to_vec()
        } else {
            Vec::new()
        };

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

        // MS level: function 0 is the MS1 acquisition; higher functions are MS2/lockmass. Refining
        // this needs getFunctionType (TODO); function index is a sound first approximation.
        let ms_level: u8 = if func == 0 { 1 } else { 2 };
        // Say it once, loudly: this lane carries no precursor at all (M32). Every MSn row it writes
        // is an orphan — no selected ion, no isolation window, no activation — and the archive is
        // otherwise indistinguishable from a complete one. The set mass is reachable via
        // `getScanItemValue`; not wired yet.
        if ms_level > 1 {
            static PRECURSOR_GAP_SAID: std::sync::Once = std::sync::Once::new();
            PRECURSOR_GAP_SAID.call_once(|| {
                log::warn!(
                    "Waters MassLynx: this reader does not yet extract precursors; \
                     MS2 rows will have none (no selected ion, isolation window or collision energy \
                     in the archive)"
                );
            });
        }
        let mut descr = SpectrumDescription {
            // ProteoWizard Waters native-id convention (1-based function/scan).
            id: format!("function={} process=0 scan={}", func + 1, scan + 1),
            index: i,
            ms_level,
            // From the vendor's per-function flag, not assumed. Unknown (export missing or the call
            // failed) keeps the historical Profile default.
            signal_continuity: match self.continuum.get(func as usize).copied().flatten() {
                Some(true) => SignalContinuity::Profile,
                Some(false) => SignalContinuity::Centroid,
                None => SignalContinuity::Profile,
            },
            polarity: ScanPolarity::Unknown,
            ..Default::default()
        };
        // No blanket `MS:1000294 "mass spectrum"` here (0.9.13). mzdata's `spectrum_type()` is a first-match
        // lookup, so that parent term wins over the specific one and the writer's inference
        // (`writer/visitor.rs`: ms_level 1 -> MS:1000579, else MS:1000580) never runs; with it absent the
        // writer types each row from `ms_level`, as the mzML, Shimadzu and Bruker-native lanes already do.
        // RT is available via readScanItemValue (TODO); leave a default scan event for now.
        descr.acquisition.scans.push(ScanEvent::default());

        Ok(MultiLayerSpectrum::new(descr, Some(arrays), None, None))
    }
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
