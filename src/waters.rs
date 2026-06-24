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

use mzdata::curie;
use mzdata::params::{Param, Unit};
use mzdata::prelude::ParamDescribed;
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
type ReadScanFn =
    unsafe extern "C" fn(*mut c_void, c_int, c_int, *mut *mut f32, *mut *mut f32, *mut c_int)
        -> c_int;

/// A native Waters `.raw` reader. Holds the loaded library, the resolved C function pointers, the
/// info + scan reader handles, and the flattened `(function, scan)` spectrum index.
pub struct WatersReader {
    // `_lib` MUST outlive the function pointers + handles below (dropped together with this struct).
    _lib: Library,
    read_scan: ReadScanFn,
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

        Ok(WatersReader {
            _lib: lib,
            read_scan,
            destroy,
            info_reader,
            scan_reader,
            index,
        })
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
        let mut descr = SpectrumDescription {
            // ProteoWizard Waters native-id convention (1-based function/scan).
            id: format!("function={} process=0 scan={}", func + 1, scan + 1),
            index: i,
            ms_level,
            signal_continuity: SignalContinuity::Profile,
            polarity: ScanPolarity::Unknown,
            ..Default::default()
        };
        descr.add_param(
            Param::builder()
                .name("mass spectrum")
                .curie(curie!(MS:1000294))
                .build(),
        );
        // RT is available via readScanItemValue (TODO); leave a default scan event for now.
        descr.acquisition.scans.push(ScanEvent::default());

        Ok(MultiLayerSpectrum::new(descr, Some(arrays), None, None))
    }

    /// A sample spectrum's array map, for deriving the writer's data-facet schema. Uses the first
    /// non-empty spectrum so both m/z and intensity columns are present.
    pub fn sample_arrays(&self) -> Result<BinaryArrayMap> {
        for i in 0..self.len() {
            if let Ok(spec) = self.spectrum(i) {
                if let Some(arrays) = spec.arrays {
                    let non_empty = arrays.mzs().map(|m| !m.is_empty()).unwrap_or(false);
                    if non_empty {
                        return Ok(arrays);
                    }
                }
            }
        }
        Ok(self.spectrum(0)?.arrays.unwrap_or_default())
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
