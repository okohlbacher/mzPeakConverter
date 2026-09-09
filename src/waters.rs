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
//!   Still unbound: `getScanItemsInFunction` (both pointer spellings crash) and therefore the per-scan
//!   SET_MASS / COLLISION_ENERGY items pwiz reads for precursors and its MSe heuristic.
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

use mzdata::params::{Param, ParamDescribed, Unit};
use mzdata::spectrum::bindata::{ArrayType, BinaryArrayMap, BinaryDataArrayType, DataArray};
use mzdata::spectrum::{
    MultiLayerSpectrum, ScanEvent, ScanPolarity, ScanWindow, SignalContinuity, SpectrumDescription,
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

/// What MassLynx states about one function, resolved once at open.
#[derive(Debug, Clone, Default)]
struct FunctionInfo {
    /// `isContinuum`; `None` when the export is unavailable or failed.
    continuum: Option<bool>,
    /// `getFunctionTypeString(getFunctionType(f))`, e.g. `TOF MS`.
    type_string: Option<String>,
    /// `getIonModeString(getIonMode(f))`, e.g. `ES+`.
    ion_mode: Option<String>,
    /// `getAcquisitionMassRange(f, 0)`.
    mass_range: Option<(f32, f32)>,
    /// Drift bins per scan when the function carries a `.cdt` and MassLynx counts bins; 0 otherwise.
    drift_bins: c_int,
    ms_level: u8,
}

/// A native Waters `.raw` reader. Holds the loaded library, the resolved C function pointers, the
/// info + scan reader handles, the per-function facts and the flattened `(function, scan)` index.
pub struct WatersReader {
    // `_lib` MUST outlive the function pointers + handles below (dropped together with this struct).
    _lib: Library,
    read_scan: ReadScanFn,
    read_drift_scan: Option<ReadDriftScanFn>,
    retention_time: Option<GetRetentionTimeFn>,
    destroy: DestroyReaderFn,
    info_reader: *mut c_void,
    scan_reader: *mut c_void,
    functions: Vec<FunctionInfo>,
    /// bin → drift time in ms, one table per run (MassLynx's `getDriftTime` takes no function).
    drift_time_ms: Vec<f32>,
    /// One entry per spectrum: (function index, scan index), both 0-based.
    index: Vec<(c_int, c_int)>,
    input: PathBuf,
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
        for (name, present) in [
            ("isContinuum", opt(b"isContinuum\0")),
            ("getFunctionType", opt(b"getFunctionType\0")),
            ("getRetentionTime", opt(b"getRetentionTime\0")),
            ("getDriftScanCount", opt(b"getDriftScanCount\0")),
            ("getDriftTime", opt(b"getDriftTime\0")),
            ("readDriftScan", opt(b"readDriftScan\0")),
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
        let mut functions: Vec<FunctionInfo> = Vec::with_capacity(n_functions as usize);
        for f in 0..n_functions {
            let continuum = is_continuum.and_then(|g| {
                let mut flag = false;
                (unsafe { g(info_reader, f, &mut flag) } == 0).then_some(flag)
            });
            let type_string = int_of(function_type, f).and_then(|code| string_of(type_string, code));
            let ion_mode = int_of(ion_mode, f).and_then(|code| string_of(mode_string, code));
            let mass_range = mass_range.and_then(|g| {
                let (mut lo, mut hi): (f32, f32) = (0.0, 0.0);
                (unsafe { g(info_reader, f, 0, &mut lo, &mut hi) } == 0 && hi > lo).then_some((lo, hi))
            });
            // pwiz's rule (WatersRawFile.hpp): a function is ion-mobility data when its `.cdt`
            // exists AND MassLynx counts drift bins for it.
            let has_cdt = ["cdt", "CDT"].iter().any(|ext| {
                input.join(format!("_func{:03}.{ext}", f + 1)).is_file()
                    || input.join(format!("_FUNC{:03}.{ext}", f + 1)).is_file()
            });
            let drift_bins = if has_cdt && read_drift_scan.is_some() {
                int_of(drift_count, f).filter(|n| (1..=MAX_DRIFT_BINS).contains(n)).unwrap_or(0)
            } else {
                0
            };
            functions.push(FunctionInfo { continuum, type_string, ion_mode, mass_range, drift_bins, ms_level: 1 });
        }
        for f in 0..functions.len() {
            functions[f].ms_level = ms_level_for(f, &functions);
        }

        // The run's drift-time table: bin → ms, read once from the first IMS function's bin count.
        let mut drift_time_ms = Vec::new();
        if let (Some(g), Some(n)) = (drift_time, functions.iter().map(|fi| fi.drift_bins).max().filter(|n| *n > 0)) {
            for bin in 0..n {
                let mut ms: f32 = f32::NAN;
                if unsafe { g(info_reader, bin, &mut ms) } != 0 || !ms.is_finite() {
                    drift_time_ms.clear();
                    log::warn!("MassLynx getDriftTime(bin={bin}) failed; drift frames will carry the bin index as their drift value");
                    break;
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

        let mut index = Vec::new();
        for f in 0..n_functions {
            let Some(n_scans) = int_of(Some(read_scan_count), f).filter(|n| *n >= 0) else {
                continue; // skip a function we can't enumerate rather than abort the whole run
            };
            for s in 0..n_scans {
                index.push((f, s));
            }
        }
        if index.is_empty() {
            return Err(close(format!("Waters .raw {} has no readable scans", input.display())));
        }

        Ok(WatersReader {
            _lib: lib,
            read_scan,
            read_drift_scan,
            retention_time,
            destroy,
            info_reader,
            scan_reader,
            functions,
            drift_time_ms,
            index,
            input: input.to_path_buf(),
        })
    }

    pub fn len(&self) -> usize {
        self.index.len()
    }

    /// Does any function carry a drift dimension (and so does the archive carry frames)?
    pub fn has_drift(&self) -> bool {
        self.functions.iter().any(|fi| fi.drift_bins > 0)
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
            "representation": "one spectrum per MassLynx scan (frame); points sorted by (m/z, drift time); per-point raw ion mobility array MS:1003007 in milliseconds",
            "drift_time_unit": "ms",
            "drift_bins_per_function": self.functions.iter().enumerate().map(|(f, fi)| serde_json::json!({"function": f + 1, "drift_bins": fi.drift_bins})).collect::<Vec<_>>(),
            "drift_time_ms": self.drift_time_ms,
            "ccs_calibration_mob_cal_csv": ccs,
        }))
    }

    /// Read one spectrum. A function with drift bins yields a FRAME (every bin's points, sorted by
    /// m/z then drift time, with a per-point drift-time array); any other function the summed scan.
    pub fn spectrum(&self, i: usize) -> Result<MultiLayerSpectrum> {
        let (func, scan) = *self
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

        // Say it once, loudly: this lane carries no precursor at all (M32). Every MSn row it writes
        // is an orphan — no selected ion, no isolation window, no activation — and the archive is
        // otherwise indistinguishable from a complete one. The set mass is a MassLynx scan item;
        // `getScanItemsInFunction` is not bound yet (see the module docs).
        if fi.ms_level > 1 {
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
            // ProteoWizard Waters native-id convention (1-based function/scan); for a frame the scan
            // is the MassLynx scan (pwiz's per-bin ids count `bins × scan + bin`).
            id: format!("function={} process=0 scan={}", func + 1, scan + 1),
            index: i,
            ms_level: fi.ms_level,
            // From the vendor's per-function flag, not assumed. Unknown (export missing or the call
            // failed) keeps the historical Profile default.
            signal_continuity: match fi.continuum {
                Some(true) => SignalContinuity::Profile,
                Some(false) => SignalContinuity::Centroid,
                None => SignalContinuity::Profile,
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
        let mut event = ScanEvent::default();
        if let Some(g) = self.retention_time {
            let mut minutes: f32 = f32::NAN;
            if unsafe { g(self.info_reader, func, scan, &mut minutes) } == 0 && minutes.is_finite() {
                event.start_time = minutes as f64;
            }
        }
        if let Some((lo, hi)) = fi.mass_range {
            event.scan_windows.push(ScanWindow::new(lo, hi));
        }
        // A frame has no single drift time: `ion_mobility_value` stays NULL on purpose (pwiz's
        // combined spectra carry a meaningless mid-range value; TDF frames carry none).
        descr.acquisition.scans.push(event);

        Ok(MultiLayerSpectrum::new(descr, Some(arrays), None, None))
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

/// Copy `n` points out of MassLynx's reader-owned buffers (valid only until the next read; NOT to be
/// freed — `releaseMemory` on them corrupts the heap). m/z widened f32 → f64.
fn copy_points(p_masses: *mut f32, p_intensities: *mut f32, n: c_int) -> Result<(Vec<f64>, Vec<f32>)> {
    if n < 0 || n > MAX_WATERS_SPECTRUM_POINTS {
        bail!("MassLynx read returned implausible point count {n}");
    }
    let n = n as usize;
    if n == 0 || p_masses.is_null() || p_intensities.is_null() {
        return Ok((Vec::new(), Vec::new()));
    }
    let mz = unsafe { std::slice::from_raw_parts(p_masses, n) }.iter().map(|&x| x as f64).collect();
    let intensity = unsafe { std::slice::from_raw_parts(p_intensities, n) }.to_vec();
    Ok((mz, intensity))
}

/// MS level from the vendor's function type, with ProteoWizard's MSe convention.
///
/// A product-ion type (`DAUGHTER`, `MSMS`, `MS/MS`, `PARENT`, `NEUTRAL`, `MRM`) is MS2. Among plain
/// MS functions the SECOND function is the elevated-energy acquisition of an MSe / HDMSe method when
/// it repeats the first function's type — pwiz (`SpectrumList_Waters.cpp`) additionally requires a
/// collision energy > 0, which needs the still-unbound scan items; until then this is the documented
/// assumption. Every other MS function (lock-mass reference, the auxiliary functions of a Synapt
/// method) is MS1, as pwiz labels them. The pre-2026-09-09 rule (every function after the first is
/// MS2) mislabelled functions 3–6 of Capan2.
fn ms_level_for(f: usize, functions: &[FunctionInfo]) -> u8 {
    let ty = |i: usize| functions.get(i).and_then(|fi| fi.type_string.as_deref()).map(|s| s.to_ascii_uppercase());
    let is_product = |s: &str| ["DAUGHTER", "MSMS", "MS/MS", "PARENT", "NEUTRAL", "MRM"].iter().any(|k| s.contains(k));
    match ty(f) {
        Some(s) if is_product(&s) => 2,
        Some(s) if f == 1 && ty(0).as_deref() == Some(s.as_str()) => 2,
        Some(_) => 1,
        // No type information at all (export missing): the historical index rule.
        None => {
            if f == 0 {
                1
            } else {
                2
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn fi(t: Option<&str>, bins: c_int) -> FunctionInfo {
        FunctionInfo { type_string: t.map(str::to_string), drift_bins: bins, ..Default::default() }
    }

    #[test]
    fn ms_levels_follow_the_function_type_and_the_mse_convention() {
        // Capan2: six TOF MS functions → 1, 2, 1, 1, 1, 1 (pwiz's labels).
        let fs: Vec<FunctionInfo> = (0..6).map(|_| fi(Some("TOF MS"), 200)).collect();
        assert_eq!((0..6).map(|f| ms_level_for(f, &fs)).collect::<Vec<_>>(), vec![1, 2, 1, 1, 1, 1]);
        // A DDA method: MS survey + product-ion functions.
        let fs = vec![fi(Some("TOF MS"), 0), fi(Some("TOF DAUGHTER"), 0), fi(Some("TOF DAUGHTER"), 0)];
        assert_eq!((0..3).map(|f| ms_level_for(f, &fs)).collect::<Vec<_>>(), vec![1, 2, 2]);
        // A second function of a DIFFERENT MS type is not the MSe pair.
        let fs = vec![fi(Some("TOF MS"), 0), fi(Some("MS"), 0)];
        assert_eq!(ms_level_for(1, &fs), 1);
        // No type information: the historical index rule.
        let fs = vec![fi(None, 0), fi(None, 0), fi(None, 0)];
        assert_eq!((0..3).map(|f| ms_level_for(f, &fs)).collect::<Vec<_>>(), vec![1, 2, 2]);
    }
}
