//! Native Shimadzu LabSolutions `.lcd` reader → mzdata spectra (native lane, no msconvert).
//!
//! ⚠️ WINDOWS-RUNTIME-ONLY AND UNTESTED. Verified to *compile* behind `#[cfg(windows)]` on any
//! host, but it only *runs* where `Shimadzu.LabSolutions.IO.IoModule.dll` (from a ProteoWizard
//! install, flat in pwiz-bin) and a .NET 8 runtime exist. There is no macOS/Linux build of the
//! Shimadzu stack. The DLL also carries a restrictive Shimadzu EULA — see `glue/shimadzu/README.md`.
//!
//! ## How it works (mirrors `src/sciex.rs`)
//!
//! `.lcd` is a vendor-closed OLE2 container whose only usable reader is the managed
//! `Shimadzu.LabSolutions.IO` .NET assembly (the same DLL ProteoWizard's `Reader_Shimadzu` wraps).
//! Rust hosts a CoreCLR runtime in-process via `netcorehost` and loads a thin C# shim
//! (`ShimadzuGlue.dll`, built from `glue/shimadzu/`). The shim reaches the vendor API through
//! runtime reflection and exposes a small C ABI of `[UnmanagedCallersOnly]` methods.
//!
//! ## Env vars
//!   * `MZPC_SHIMADZU_GLUE` — dir holding `ShimadzuGlue.dll` + `ShimadzuGlue.runtimeconfig.json`.
//!   * `MZPC_PWIZ_DIR`      — ProteoWizard install dir; `Shimadzu.LabSolutions.IO.IoModule.dll`
//!     sits flat there (next to `msconvert.exe`), unlike SciEX's `vendor_api/ABI` subdir.
//!
//! ## C ABI contract (must match `glue/shimadzu/Glue.cs` exactly)
//! Strings cross as NUL-terminated UTF-16 (`*const u16`). Data arrays come back via
//! pointer+len+free. Handles are opaque `i64`. `ShimadzuSpectrumMeta` is 48 bytes / 8-aligned.

use std::ffi::OsStr;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};

use netcorehost::hostfxr::AssemblyDelegateLoader;
use netcorehost::pdcstring::PdCString;
use netcorehost::{nethost, pdcstr};

use mzdata::curie;
use mzdata::params::{Param, Unit};
use mzdata::prelude::ParamDescribed;
use mzdata::spectrum::bindata::{ArrayType, BinaryArrayMap, BinaryDataArrayType, DataArray};
use mzdata::spectrum::{
    MultiLayerSpectrum, ScanEvent, ScanPolarity, SignalContinuity, SpectrumDescription,
};

/// Hard cap on points per spectrum (guards a corrupt/hostile length). ≈1.2 GiB at the max.
const MAX_SHIMADZU_SPECTRUM_POINTS: i64 = 100_000_000;

// --- C ABI mirror ----------------------------------------------------------

/// Scalar per-spectrum metadata filled by `SpectrumMeta`. `#[repr(C)]` matches the managed
/// `ShimadzuSpectrumMeta` (see `glue/shimadzu/Glue.cs`). polarity: 0=pos,1=neg,2=unknown;
/// signal_continuity: 0=profile,1=centroid.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ShimadzuSpectrumMeta {
    scan_number: i64,
    ms_level: i32,
    polarity: i32,
    signal_continuity: i32,
    precursor_charge: i32,
    retention_time_seconds: f64,
    precursor_mz: f64,
    n_points: i64,
}

// Layout assertion: i64 + 4×i32 (16B) + 2×f64 (16B) + i64 = 48 bytes, 8-byte aligned. The C# side
// asserts Marshal.SizeOf == 48 in its static ctor. Field drift fails the build here.
const _: () = assert!(std::mem::size_of::<ShimadzuSpectrumMeta>() == 48);
const _: () = assert!(std::mem::align_of::<ShimadzuSpectrumMeta>() == 8);

type ShimOpen = extern "system" fn(*const u16, *const u16) -> i64;
type ShimClose = extern "system" fn(i64);
type ShimSpectrumCount = extern "system" fn(i64) -> i64;
type ShimSpectrumMetaFn = extern "system" fn(i64, i64, *mut ShimadzuSpectrumMeta) -> i32;
type ShimSpectrumData =
    extern "system" fn(i64, i64, *mut *const f64, *mut *const f32, *mut i64) -> i32;
type ShimDataFree = extern "system" fn(i64, *const f64, *const f32);
type ShimLastError = extern "system" fn(*mut u16, i32) -> i32;

#[derive(Clone)]
struct GlueApi {
    _runtime: Arc<AssemblyDelegateLoader>,
    open: ShimOpen,
    close: ShimClose,
    spectrum_count: ShimSpectrumCount,
    spectrum_meta: ShimSpectrumMetaFn,
    spectrum_data: ShimSpectrumData,
    data_free: ShimDataFree,
    last_error: ShimLastError,
}

impl GlueApi {
    fn load(glue_dir: &Path) -> Result<Self> {
        let runtime_config = glue_dir.join("ShimadzuGlue.runtimeconfig.json");
        let assembly = glue_dir.join("ShimadzuGlue.dll");
        if !runtime_config.is_file() {
            bail!(
                "ShimadzuGlue.runtimeconfig.json not found in {} (set MZPC_SHIMADZU_GLUE to the \
                 glue build output directory, e.g. .../bin/Release/net8.0)",
                glue_dir.display()
            );
        }
        if !assembly.is_file() {
            bail!(
                "ShimadzuGlue.dll not found in {} (build glue/shimadzu with `dotnet build` and \
                 point MZPC_SHIMADZU_GLUE at bin/.../net8.0)",
                glue_dir.display()
            );
        }

        let hostfxr = nethost::load_hostfxr().context(
            "failed to load hostfxr; a .NET 8 runtime must be installed to read Shimadzu .lcd natively",
        )?;
        let context = hostfxr
            .initialize_for_runtime_config(path_to_pdcstring(&runtime_config)?)
            .context("initializing CoreCLR for ShimadzuGlue.runtimeconfig.json")?;
        let loader = Arc::new(
            context
                .get_delegate_loader_for_assembly(path_to_pdcstring(&assembly)?)
                .context("creating delegate loader for ShimadzuGlue.dll")?,
        );

        let ty = pdcstr!("ShimadzuGlue.Api, ShimadzuGlue");
        let open = *loader
            .get_function_with_unmanaged_callers_only::<ShimOpen>(ty, pdcstr!("Open"))
            .map_err(|e| anyhow!("resolving glue export Open: {e}"))?;
        let close = *loader
            .get_function_with_unmanaged_callers_only::<ShimClose>(ty, pdcstr!("Close"))
            .map_err(|e| anyhow!("resolving glue export Close: {e}"))?;
        let spectrum_count = *loader
            .get_function_with_unmanaged_callers_only::<ShimSpectrumCount>(ty, pdcstr!("SpectrumCount"))
            .map_err(|e| anyhow!("resolving glue export SpectrumCount: {e}"))?;
        let spectrum_meta = *loader
            .get_function_with_unmanaged_callers_only::<ShimSpectrumMetaFn>(ty, pdcstr!("SpectrumMeta"))
            .map_err(|e| anyhow!("resolving glue export SpectrumMeta: {e}"))?;
        let spectrum_data = *loader
            .get_function_with_unmanaged_callers_only::<ShimSpectrumData>(ty, pdcstr!("SpectrumData"))
            .map_err(|e| anyhow!("resolving glue export SpectrumData: {e}"))?;
        let data_free = *loader
            .get_function_with_unmanaged_callers_only::<ShimDataFree>(ty, pdcstr!("DataFree"))
            .map_err(|e| anyhow!("resolving glue export DataFree: {e}"))?;
        let last_error = *loader
            .get_function_with_unmanaged_callers_only::<ShimLastError>(ty, pdcstr!("LastError"))
            .map_err(|e| anyhow!("resolving glue export LastError: {e}"))?;

        Ok(Self {
            _runtime: loader,
            open,
            close,
            spectrum_count,
            spectrum_meta,
            spectrum_data,
            data_free,
            last_error,
        })
    }

    fn last_error(&self) -> Option<String> {
        let needed = (self.last_error)(std::ptr::null_mut(), 0);
        if needed <= 0 {
            return None;
        }
        let mut buf = vec![0u16; needed as usize];
        let written = (self.last_error)(buf.as_mut_ptr(), needed);
        if written <= 0 {
            return None;
        }
        let n = (written as usize).min(buf.len());
        Some(String::from_utf16_lossy(&buf[..n]))
    }
}

/// A native Shimadzu `.lcd` reader yielding one [`MultiLayerSpectrum`] per scan (1-based on the
/// vendor side, 0-based here). ⚠️ Windows-runtime-only and untested.
pub struct ShimadzuReader {
    api: GlueApi,
    handle: i64,
    count: usize,
    lcd_path: PathBuf,
    _not_thread_safe: PhantomData<*const ()>,
}

impl ShimadzuReader {
    pub fn open(path: &Path) -> Result<Self> {
        let glue_dir = std::env::var_os("MZPC_SHIMADZU_GLUE")
            .map(PathBuf::from)
            .ok_or_else(|| {
                anyhow!(
                    "MZPC_SHIMADZU_GLUE is not set; point it at the directory holding ShimadzuGlue.dll \
                     (the `dotnet build` output of glue/shimadzu, e.g. .../bin/Release/net8.0)"
                )
            })?;
        let pwiz_dir = resolve_shimadzu_dll_dir()?;
        let api = GlueApi::load(&glue_dir)?;

        let path_utf16 = to_utf16_nul(path.as_os_str())
            .with_context(|| format!("encoding .lcd path {}", path.display()))?;
        let pwiz_utf16 = to_utf16_nul(pwiz_dir.as_os_str())
            .with_context(|| format!("encoding pwiz dir {}", pwiz_dir.display()))?;

        let handle = (api.open)(path_utf16.as_ptr(), pwiz_utf16.as_ptr());
        if handle <= 0 {
            bail!(
                "Shimadzu glue failed to open {} (Shimadzu.LabSolutions.IO from {} could not read \
                 it — a legacy/IT-TOF .lcd is unsupported, or the file is not a valid .lcd). This \
                 path is Windows-runtime-only and untested: {}",
                path.display(),
                pwiz_dir.display(),
                api.last_error().unwrap_or_default()
            );
        }

        let count_i64 = (api.spectrum_count)(handle);
        if count_i64 < 0 {
            let detail = api.last_error().unwrap_or_default();
            (api.close)(handle);
            bail!("Shimadzu glue reported a spectrum-count error for {}: {detail}", path.display());
        }
        let count = match usize::try_from(count_i64) {
            Ok(c) => c,
            Err(_) => {
                (api.close)(handle);
                bail!("Shimadzu spectrum count {count_i64} does not fit in usize");
            }
        };

        Ok(Self {
            api,
            handle,
            count,
            lcd_path: path.to_path_buf(),
            _not_thread_safe: PhantomData,
        })
    }

    pub fn len(&self) -> usize {
        self.count
    }
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
    pub fn lcd_path(&self) -> &Path {
        &self.lcd_path
    }

    fn meta(&self, i: usize) -> Result<ShimadzuSpectrumMeta> {
        let index = i64::try_from(i).map_err(|_| anyhow!("Shimadzu index {i} does not fit in i64"))?;
        let mut meta = ShimadzuSpectrumMeta::default();
        let rc = (self.api.spectrum_meta)(self.handle, index, &mut meta as *mut _);
        if rc != 0 {
            bail!(
                "Shimadzu glue SpectrumMeta failed for index {i} (rc {rc}): {}",
                self.api.last_error().unwrap_or_default()
            );
        }
        Ok(meta)
    }

    fn peaks(&self, i: usize) -> Result<(Vec<f64>, Vec<f32>)> {
        let index = i64::try_from(i).map_err(|_| anyhow!("Shimadzu index {i} does not fit in i64"))?;
        let mut mz_ptr: *const f64 = std::ptr::null();
        let mut int_ptr: *const f32 = std::ptr::null();
        let mut len: i64 = 0;

        let rc = (self.api.spectrum_data)(
            self.handle,
            index,
            &mut mz_ptr as *mut _,
            &mut int_ptr as *mut _,
            &mut len as *mut _,
        );
        if rc != 0 {
            bail!(
                "Shimadzu glue SpectrumData failed for index {i} (rc {rc}): {}",
                self.api.last_error().unwrap_or_default()
            );
        }

        // RAII guard: DataFree must release the managed pins even on a panic/early bail.
        struct PinGuard<'a> {
            api: &'a GlueApi,
            handle: i64,
            mz_ptr: *const f64,
            int_ptr: *const f32,
            armed: bool,
        }
        impl Drop for PinGuard<'_> {
            fn drop(&mut self) {
                if self.armed {
                    (self.api.data_free)(self.handle, self.mz_ptr, self.int_ptr);
                }
            }
        }
        let mut guard = PinGuard {
            api: &self.api,
            handle: self.handle,
            mz_ptr,
            int_ptr,
            armed: true,
        };

        if len < 0 {
            bail!("Shimadzu spectrum {i} reports negative length {len}");
        }
        if len > MAX_SHIMADZU_SPECTRUM_POINTS {
            bail!(
                "Shimadzu spectrum {i} reports {len} points, exceeding safety limit \
                 {MAX_SHIMADZU_SPECTRUM_POINTS}"
            );
        }
        let n = len as usize;
        if n == 0 {
            return Ok((Vec::new(), Vec::new()));
        }
        if mz_ptr.is_null() || int_ptr.is_null() {
            bail!("Shimadzu spectrum {i} reports {n} points but a data pointer is null");
        }
        // SAFETY: the glue guarantees both arrays hold `n` elements, pinned until `data_free`.
        let mz = unsafe { std::slice::from_raw_parts(mz_ptr, n) }.to_vec();
        let intensity = unsafe { std::slice::from_raw_parts(int_ptr, n) }.to_vec();

        guard.armed = false;
        (self.api.data_free)(self.handle, mz_ptr, int_ptr);
        Ok((mz, intensity))
    }

    /// Build the mzdata spectrum for spectrum `i` (0-based reader order).
    pub fn spectrum(&self, i: usize) -> Result<MultiLayerSpectrum> {
        if i >= self.count {
            bail!("Shimadzu spectrum index {i} out of range (len {})", self.count);
        }
        let meta = self.meta(i)?;
        let (mz, intensity) = self.peaks(i)?;

        let mut arrays = BinaryArrayMap::new();
        let mut mz_da = DataArray::wrap(&ArrayType::MZArray, BinaryDataArrayType::Float64, Vec::new());
        mz_da.update_buffer(mz.as_slice()).map_err(|e| anyhow!("encoding m/z: {e}"))?;
        mz_da.unit = Unit::MZ;
        arrays.add(mz_da);
        let mut int_da =
            DataArray::wrap(&ArrayType::IntensityArray, BinaryDataArrayType::Float32, Vec::new());
        int_da
            .update_buffer(intensity.as_slice())
            .map_err(|e| anyhow!("encoding intensity: {e}"))?;
        int_da.unit = Unit::DetectorCounts;
        arrays.add(int_da);

        let ms_level = u8::try_from(meta.ms_level.max(1)).unwrap_or(1);
        let polarity = match meta.polarity {
            0 => ScanPolarity::Positive,
            1 => ScanPolarity::Negative,
            _ => ScanPolarity::Unknown,
        };
        let signal_continuity = match meta.signal_continuity {
            1 => SignalContinuity::Centroid,
            _ => SignalContinuity::Profile,
        };

        let mut descr = SpectrumDescription {
            id: format!("scan={}", meta.scan_number),
            index: i,
            ms_level,
            signal_continuity,
            polarity,
            ..Default::default()
        };
        descr.add_param(Param::builder().name("mass spectrum").curie(curie!(MS:1000294)).build());
        let mut scan = ScanEvent::default();
        // ABI carries seconds; mzdata scan start_time is minutes.
        scan.start_time = meta.retention_time_seconds / 60.0;
        descr.acquisition.scans.push(scan);

        // NOTE: precursor m/z for MSn is available on `meta.precursor_mz` but the precursor linkage
        // is not yet threaded into the SpectrumDescription — parity item vs. msconvert (see e2e).

        Ok(MultiLayerSpectrum::new(descr, Some(arrays), None, None))
    }

    /// A sample spectrum's array map, for deriving the writer's data-facet schema.
    pub fn sample_arrays(&self) -> Result<BinaryArrayMap> {
        let mut chosen = 0usize;
        for i in 0..self.count {
            if let Ok((mz, _)) = self.peaks(i) {
                if !mz.is_empty() {
                    chosen = i;
                    break;
                }
            }
        }
        self.spectrum(chosen)?
            .arrays
            .clone()
            .ok_or_else(|| anyhow!("sample spectrum has no arrays"))
    }
}

impl Drop for ShimadzuReader {
    fn drop(&mut self) {
        if self.handle > 0 {
            (self.api.close)(self.handle);
            self.handle = 0;
        }
    }
}

// --- helpers ---------------------------------------------------------------

/// Resolve the directory holding `Shimadzu.LabSolutions.IO.IoModule.dll` from `MZPC_PWIZ_DIR`.
/// Unlike SciEX (`vendor_api/ABI`), the Shimadzu DLLs sit FLAT in the pwiz-bin dir, so we accept
/// `MZPC_PWIZ_DIR` as-is (probing a couple of sensible subdirs first).
fn resolve_shimadzu_dll_dir() -> Result<PathBuf> {
    let root = std::env::var_os("MZPC_PWIZ_DIR").map(PathBuf::from).ok_or_else(|| {
        anyhow!(
            "MZPC_PWIZ_DIR is not set; point it at a ProteoWizard install whose directory holds \
             Shimadzu.LabSolutions.IO.IoModule.dll"
        )
    })?;
    let candidates = [root.clone(), root.join("vendor_api").join("Shimadzu")];
    for cand in &candidates {
        if cand.is_dir() && dir_has_shimadzu(cand) {
            return Ok(cand.clone());
        }
    }
    Ok(root)
}

fn dir_has_shimadzu(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        if let Some(name) = entry.file_name().to_str() {
            let lower = name.to_ascii_lowercase();
            if lower.starts_with("shimadzu.labsolutions") && lower.ends_with(".dll") {
                return true;
            }
        }
    }
    false
}

fn to_utf16_nul(s: &OsStr) -> Result<Vec<u16>> {
    #[cfg(windows)]
    let mut v: Vec<u16> = {
        use std::os::windows::ffi::OsStrExt;
        s.encode_wide().collect()
    };
    #[cfg(not(windows))]
    let mut v: Vec<u16> = s.to_string_lossy().encode_utf16().collect();

    if v.contains(&0) {
        bail!("path contains an interior NUL, which is not a valid filesystem path");
    }
    v.push(0);
    Ok(v)
}

fn path_to_pdcstring(p: &Path) -> Result<PdCString> {
    p.to_string_lossy()
        .parse()
        .map_err(|e| anyhow!("encoding path {} for the .NET host: {e}", p.display()))
}
