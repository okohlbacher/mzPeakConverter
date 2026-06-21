//! Native Agilent MassHunter (`.d`) reader → mzdata spectra (PLAN §3.7).
//!
//! **Windows-runtime-only and UNTESTED.** This file compiles on any platform behind the `agilent`
//! cargo feature, but it can only *run* on Windows with the Agilent MHDAC (MassHunter Data Access
//! Component) DLLs present, because:
//!   * MHDAC is a Windows-only mixed-mode .NET assembly set (`MassSpecDataReader.dll`,
//!     `BaseCommon.dll`, `BaseDataAccess.dll`, `agtsampleinforw.dll`, …). It has no macOS/Linux
//!     build.
//!   * We never reference MHDAC at compile time. Instead we host an in-process .NET 8 runtime
//!     (via `netcorehost`, the same crate the Thermo reader uses) and call a small C# **glue**
//!     assembly (`AgilentGlue.dll`, built from `glue/agilent/`) that reaches into MHDAC purely
//!     through `System.Reflection`. So the whole stack — Rust + C# — builds without the DLLs, and
//!     the DLLs are only needed at run time.
//!
//! ## How the pieces fit
//! ```text
//!   AgilentReader (this file, Rust)
//!        │  netcorehost: load_hostfxr → initialize_for_runtime_config(AgilentGlue.runtimeconfig.json)
//!        │               → get_delegate_loader_for_assembly(AgilentGlue.dll)
//!        │               → get_function_with_unmanaged_callers_only("AgilentGlue.Exports", "...")
//!        ▼
//!   AgilentGlue.dll (glue/agilent/Glue.cs, C#)
//!        │  System.Reflection: Assembly.LoadFrom(<pwiz>/vendor_api/Agilent/MassSpecDataReader.dll)
//!        │               → MassSpecDataReader.OpenDataFile(path)
//!        │               → GetScanRecord(i) / GetSpectrum(i, …) → XArray / YArray
//!        ▼
//!   MHDAC (Agilent's licensed DLLs, sourced from a ProteoWizard install)
//! ```
//!
//! ## Environment contract (resolved at `open`)
//!   * `MZPC_AGILENT_GLUE` — directory containing `AgilentGlue.dll` +
//!     `AgilentGlue.runtimeconfig.json` (the `dotnet build` output of `glue/agilent/`).
//!   * `MZPC_PWIZ_DIR` — a ProteoWizard install directory. The MHDAC DLLs live under
//!     `<MZPC_PWIZ_DIR>/vendor_api/Agilent`. We pass this directory to the glue, which does the
//!     reflection-based `Assembly.LoadFrom`. (MHDAC's EULA is non-commercial-use-only — see the
//!     glue README.)
//!
//! ## C-ABI contract (Rust ⇄ glue, all functions `[UnmanagedCallersOnly]` on the C# side)
//! All strings are UTF-16 (`*const u16`) NUL-terminated, matching .NET's native `char`/`wchar_t`.
//!   * `open(path_utf16, pwiz_dir_utf16) -> i64`
//!       Opens the `.d`. Returns a non-negative opaque **handle** on success, or a negative error
//!       code on failure. The handle indexes a registry of live `MassSpecDataReader` objects kept
//!       alive on the .NET side.
//!   * `spectrum_count(handle) -> i64`
//!       Number of MS scans (`TotalScansPresent`). Negative on error.
//!   * `get_spectrum(handle, index, out: *mut SpectrumOut) -> i32`
//!       **Pointer+len+free** model (chosen over size-then-fill to avoid two managed round-trips
//!       per spectrum). The glue allocates the m/z and intensity arrays with
//!       `Marshal.AllocHGlobal`, fills `SpectrumOut`, and returns 0. Rust copies the arrays out and
//!       then calls `free_spectrum` to release the unmanaged memory. Non-zero return = error and
//!       the out-struct is left zeroed (nothing to free).
//!   * `free_spectrum(out: *mut SpectrumOut)`
//!       Frees the two `AllocHGlobal` buffers referenced by a previously-filled `SpectrumOut`.
//!   * `close(handle)`
//!       Drops the reader from the registry. Idempotent.
//!   * `LastError(buf_utf16: *mut u16, cap: i32) -> i32`
//!       Copies the last error message (per-thread) into `buf` (UTF-16, NOT NUL-terminated unless
//!       there is room), filling up to `cap` code units, and ALWAYS returns the FULL length in
//!       UTF-16 code units so the caller can detect truncation. A null `buf` or `cap <= 0` just
//!       returns the needed length. Never throws across the boundary. Best-effort diagnostics only.
//!
//! ## Scope
//! Non-IM MS only (profile or centroid, MS1/MS2). Agilent ion-mobility (6560 IM-QTOF) needs the
//! separate **MIDAC** SDK, which exposes the drift dimension — out of scope here. The drift
//! dimension is a TODO (see [`AgilentReader::spectrum`]).

use std::path::Path;
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

// ---------------------------------------------------------------------------
// FFI types — must match the C# [StructLayout(LayoutKind.Sequential)] struct.
// ---------------------------------------------------------------------------

/// Mirror of the C# `SpectrumOut` struct returned by `get_spectrum`. `#[repr(C)]` + sequential
/// layout on both sides keeps the field order/sizes identical across the boundary.
///
/// `mz_ptr`/`intensity_ptr` point at `n_points` `f64`s each, allocated by the glue with
/// `Marshal.AllocHGlobal`. They are valid until `free_spectrum` is called. Agilent's `XArray`/
/// `YArray` are `double[]`/`float[]`; the glue widens intensity to `f64` so both columns share one
/// element type across the ABI (we narrow intensity back to `f32` when building the mzdata array,
/// matching the TSF reader's intensity dtype).
#[repr(C)]
#[derive(Clone, Copy)]
struct SpectrumOut {
    mz_ptr: *const f64,
    intensity_ptr: *const f64,
    n_points: i64,
    /// Retention time in **minutes** (Agilent reports minutes natively).
    rt_minutes: f64,
    /// 1 = MS1, 2 = MS2 (MHDAC `MSLevel`: MS=1, MSMS=2).
    ms_level: i32,
    /// 1 = positive, -1 = negative, 0 = unknown/mixed (MHDAC `IonPolarity`).
    polarity: i32,
    /// 1 = centroid/peak, 0 = profile (MHDAC `MSStorageMode`).
    is_centroid: i32,
    /// Vendor scan id (`IMSScanRecord.ScanID`), used to build the mzdata spectrum id.
    scan_id: i32,
}

impl SpectrumOut {
    const fn zeroed() -> Self {
        SpectrumOut {
            mz_ptr: std::ptr::null(),
            intensity_ptr: std::ptr::null(),
            n_points: 0,
            rt_minutes: 0.0,
            ms_level: 0,
            polarity: 0,
            is_centroid: 0,
            scan_id: 0,
        }
    }
}

// ABI drift guard: if the #[repr(C)] layout below ever stops matching the C# `SpectrumOut`
// (StructLayout.Sequential), the size/align will change and this assert fails at compile time
// instead of silently corrupting memory across the boundary. The matching check on the C# side
// lives in Glue.cs (`AbiAssert`, validated at module init). On every 64-bit target the glue
// supports, the layout is: 2×ptr(8) + i64(8) + f64(8) + 4×i32(4) = 48 bytes, 8-byte aligned.
const _: () = {
    assert!(std::mem::size_of::<SpectrumOut>() == 48);
    assert!(std::mem::align_of::<SpectrumOut>() == 8);
};

// Function-pointer signatures for the glue's [UnmanagedCallersOnly] exports. `extern "system"` is
// the calling convention .NET uses for these on every platform `netcorehost` supports.
type OpenFn = extern "system" fn(*const u16, *const u16) -> i64;
type CountFn = extern "system" fn(i64) -> i64;
type GetSpectrumFn = extern "system" fn(i64, i64, *mut SpectrumOut) -> i32;
type FreeSpectrumFn = extern "system" fn(*mut SpectrumOut);
type CloseFn = extern "system" fn(i64);
type LastErrorFn = extern "system" fn(*mut u16, i32) -> i32;

/// Bound function pointers into the loaded glue assembly. `ManagedFunction` derefs to the raw
/// `fn(..)` pointer, so we keep them and call through `*self.open` etc.
struct GlueApi {
    open: netcorehost::hostfxr::ManagedFunction<OpenFn>,
    count: netcorehost::hostfxr::ManagedFunction<CountFn>,
    get_spectrum: netcorehost::hostfxr::ManagedFunction<GetSpectrumFn>,
    free_spectrum: netcorehost::hostfxr::ManagedFunction<FreeSpectrumFn>,
    close: netcorehost::hostfxr::ManagedFunction<CloseFn>,
    last_error: netcorehost::hostfxr::ManagedFunction<LastErrorFn>,
    // Keep the loader alive for as long as we hold function pointers into it.
    _loader: Arc<AssemblyDelegateLoader>,
}

// The .NET runtime + the resolved glue functions are process-global state. SAFETY: the bound
// function pointers are immutable after creation, and the C# side serializes its own state via the
// handle registry; we never mutate `GlueApi` after building it.
unsafe impl Send for GlueApi {}
unsafe impl Sync for GlueApi {}

/// Assembly-qualified type name and method names of the glue exports, as the .NET loader expects
/// them: `"Namespace.Type, AssemblyName"`.

impl GlueApi {
    /// Boot the in-process .NET runtime against the glue's runtimeconfig and bind every export.
    /// Mirrors `dotnetrawfilereader-sys`'s `try_create_runtime`, but the bundle is on disk
    /// (resolved from `MZPC_AGILENT_GLUE`) rather than embedded.
    fn load(glue_dir: &Path) -> Result<Self> {
        let runtime_config = glue_dir.join("AgilentGlue.runtimeconfig.json");
        if !runtime_config.exists() {
            bail!(
                "Agilent glue runtimeconfig not found at {} — set MZPC_AGILENT_GLUE to the \
                 `dotnet build` output dir of glue/agilent/",
                runtime_config.display()
            );
        }
        let assembly = glue_dir.join("AgilentGlue.dll");
        if !assembly.exists() {
            bail!("Agilent glue assembly not found at {}", assembly.display());
        }

        // load_hostfxr → initialize_for_runtime_config → delegate loader for our assembly.
        let hostfxr = nethost::load_hostfxr()
            .context("loading hostfxr (is a .NET 8 runtime installed?)")?;
        let runtime_config_enc: PdCString = runtime_config
            .to_string_lossy()
            .parse()
            .map_err(|e| anyhow!("encoding runtimeconfig path: {e}"))?;
        let context = hostfxr
            .initialize_for_runtime_config(runtime_config_enc)
            .context("initialize_for_runtime_config")?;
        let assembly_enc: PdCString = assembly
            .to_string_lossy()
            .parse()
            .map_err(|e| anyhow!("encoding assembly path: {e}"))?;
        let loader = Arc::new(
            context
                .get_delegate_loader_for_assembly(assembly_enc)
                .context("get_delegate_loader_for_assembly")?,
        );

        // Bind each [UnmanagedCallersOnly] export by name.
        let open = loader
            .get_function_with_unmanaged_callers_only::<OpenFn>(pdcstr!("AgilentGlue.Exports, AgilentGlue"), pdcstr!("Open"))
            .context("binding Open")?;
        let count = loader
            .get_function_with_unmanaged_callers_only::<CountFn>(pdcstr!("AgilentGlue.Exports, AgilentGlue"), pdcstr!("SpectrumCount"))
            .context("binding SpectrumCount")?;
        let get_spectrum = loader
            .get_function_with_unmanaged_callers_only::<GetSpectrumFn>(
                pdcstr!("AgilentGlue.Exports, AgilentGlue"),
                pdcstr!("GetSpectrum"),
            )
            .context("binding GetSpectrum")?;
        let free_spectrum = loader
            .get_function_with_unmanaged_callers_only::<FreeSpectrumFn>(
                pdcstr!("AgilentGlue.Exports, AgilentGlue"),
                pdcstr!("FreeSpectrum"),
            )
            .context("binding FreeSpectrum")?;
        let close = loader
            .get_function_with_unmanaged_callers_only::<CloseFn>(pdcstr!("AgilentGlue.Exports, AgilentGlue"), pdcstr!("Close"))
            .context("binding Close")?;
        let last_error = loader
            .get_function_with_unmanaged_callers_only::<LastErrorFn>(pdcstr!("AgilentGlue.Exports, AgilentGlue"), pdcstr!("LastError"))
            .context("binding LastError")?;

        Ok(GlueApi {
            open,
            count,
            get_spectrum,
            free_spectrum,
            close,
            last_error,
            _loader: loader,
        })
    }

    /// Best-effort retrieval of the glue's stashed last-error message. Calls `LastError` twice:
    /// once with a null buffer to learn the full UTF-16 length, then again to fill an exact-sized
    /// buffer. The fill-buffer contract returns the FULL length (not the count written), so we
    /// clamp to our buffer size. Returns `None` when there is no message. Never throws across the
    /// boundary, so this is purely diagnostic.
    fn last_error(&self) -> Option<String> {
        let needed = (self.last_error)(std::ptr::null_mut(), 0);
        if needed <= 0 {
            return None;
        }
        let mut buf = vec![0u16; needed as usize];
        // SAFETY: `buf` is a valid, writable region of `needed` u16s. The glue copies up to that
        // many code units and returns the full length again; we clamp to what fits in our buffer.
        let written = (self.last_error)(buf.as_mut_ptr(), needed);
        if written <= 0 {
            return None;
        }
        let n = (written as usize).min(buf.len());
        Some(String::from_utf16_lossy(&buf[..n]))
    }
}

/// UTF-16 NUL-terminated encoding for a path, as the C# side expects (`char*`).
///
/// On Windows we go through `OsStrExt::encode_wide` so the OS-native wide form is preserved
/// losslessly (no UTF-8 round-trip / lossy replacement of unpaired surrogates that a real
/// `.d` path could legitimately carry). On other platforms — where this code only ever
/// builds, never runs — we fall back to the lossy string form. Either way we reject an
/// interior NUL: a NUL would silently truncate the path on the C# `new string(char*)` side,
/// so we surface it as an error here instead.
#[cfg(windows)]
fn to_utf16_nul(path: &Path) -> Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;
    let units: Vec<u16> = path.as_os_str().encode_wide().collect();
    if units.contains(&0) {
        bail!("path contains an interior NUL: {}", path.display());
    }
    let mut out = units;
    out.push(0);
    Ok(out)
}

#[cfg(not(windows))]
fn to_utf16_nul(path: &Path) -> Result<Vec<u16>> {
    let s = path.to_string_lossy();
    let units: Vec<u16> = s.encode_utf16().collect();
    if units.contains(&0) {
        bail!("path contains an interior NUL: {}", path.display());
    }
    let mut out = units;
    out.push(0);
    Ok(out)
}

/// RAII guard that frees a filled [`SpectrumOut`]'s unmanaged buffers on drop. Used to plug the
/// leak window between a successful `get_spectrum` and the explicit `free_spectrum`: if the copy in
/// [`AgilentReader::fetch`] panics, this guard's `Drop` still releases the glue's `AllocHGlobal`
/// buffers. The normal path disarms it via [`SpectrumGuard::disarm`] and frees explicitly.
struct SpectrumGuard<'a> {
    api: &'a GlueApi,
    out: SpectrumOut,
}

impl<'a> SpectrumGuard<'a> {
    /// Take ownership of the `SpectrumOut` and suppress the guard's `Drop` (so the caller is now
    /// responsible for freeing the buffers).
    fn disarm(self) -> SpectrumOut {
        let out = self.out;
        std::mem::forget(self);
        out
    }
}

impl<'a> Drop for SpectrumGuard<'a> {
    fn drop(&mut self) {
        // Only runs on the panic/early-return path; the happy path forgets the guard via disarm().
        (self.api.free_spectrum)(&mut self.out as *mut SpectrumOut);
    }
}

/// A native Agilent `.d` reader yielding one [`MultiLayerSpectrum`] per MS scan.
///
/// Lifecycle: `open` boots the runtime (once-ish; hostfxr is process-global) and opens the `.d`,
/// `Drop` calls the glue's `close`.
pub struct AgilentReader {
    api: GlueApi,
    handle: i64,
    count: usize,
}

impl AgilentReader {
    /// Open an Agilent `.d` directory. Resolves the glue dir from `MZPC_AGILENT_GLUE` and the
    /// MHDAC dir from `<MZPC_PWIZ_DIR>/vendor_api/Agilent`, boots the .NET runtime, and opens the
    /// reader on the C# side.
    pub fn open(path: &Path) -> Result<Self> {
        let glue_dir = std::env::var_os("MZPC_AGILENT_GLUE").map(std::path::PathBuf::from).ok_or_else(|| {
            anyhow!(
                "MZPC_AGILENT_GLUE not set — point it at the `dotnet build` output dir of \
                 glue/agilent/ (containing AgilentGlue.dll + AgilentGlue.runtimeconfig.json)"
            )
        })?;
        let pwiz_dir = std::env::var_os("MZPC_PWIZ_DIR").map(std::path::PathBuf::from).ok_or_else(|| {
            anyhow!(
                "MZPC_PWIZ_DIR not set — point it at a ProteoWizard install; the Agilent MHDAC \
                 DLLs are loaded from <MZPC_PWIZ_DIR>/vendor_api/Agilent"
            )
        })?;
        // MHDAC DLL directory inside the pwiz tree. The glue does the actual Assembly.LoadFrom; we
        // just hand it this directory.
        let mhdac_dir = pwiz_dir.join("vendor_api").join("Agilent");

        let api = GlueApi::load(&glue_dir)?;

        let path_utf16 = to_utf16_nul(path)?;
        let mhdac_utf16 = to_utf16_nul(&mhdac_dir)?;
        let handle = (api.open)(path_utf16.as_ptr(), mhdac_utf16.as_ptr());
        if handle < 0 {
            bail!(
                "Agilent glue failed to open {} (code {handle}): {}",
                path.display(),
                api.last_error().unwrap_or_default()
            );
        }

        let count = (api.count)(handle);
        if count < 0 {
            let detail = api.last_error().unwrap_or_default();
            (api.close)(handle);
            bail!(
                "Agilent glue failed to count spectra in {} (code {count}): {detail}",
                path.display()
            );
        }

        Ok(Self { api, handle, count: count as usize })
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Pull one spectrum's data across the FFI boundary: call `get_spectrum`, copy the two arrays
    /// into owned `Vec`s, then immediately `free_spectrum` to release the unmanaged buffers.
    fn fetch(&self, i: usize) -> Result<(Vec<f64>, Vec<f32>, SpectrumOut)> {
        let mut out = SpectrumOut::zeroed();
        let rc = (self.api.get_spectrum)(self.handle, i as i64, &mut out as *mut SpectrumOut);
        if rc != 0 {
            bail!(
                "Agilent glue GetSpectrum({i}) failed (code {rc}): {}",
                self.api.last_error().unwrap_or_default()
            );
        }

        // From here on the glue may have allocated unmanaged buffers referenced by `out`. Wrap it
        // in an RAII guard whose Drop calls free_spectrum, so the buffers are released even if the
        // copy below panics (e.g. allocation failure widening to f32). We disarm the guard after a
        // successful copy and free explicitly to keep the normal path identical to before.
        let guard = SpectrumGuard { api: &self.api, out };
        let n = guard.out.n_points;
        if n < 0 {
            // Guard's Drop frees whatever may have been (partially) allocated.
            bail!("Agilent glue GetSpectrum({i}) returned negative n_points {n}");
        }
        let n = n as usize;

        // SAFETY: on rc==0 with n>0 the glue guarantees mz_ptr/intensity_ptr point at `n` f64s
        // each, valid until free_spectrum. We copy out of them before freeing. Null with n==0 is
        // a legitimate empty spectrum.
        let (mz, intensity) =
            if n == 0 || guard.out.mz_ptr.is_null() || guard.out.intensity_ptr.is_null() {
                (Vec::new(), Vec::new())
            } else {
                let mz_slice = unsafe { std::slice::from_raw_parts(guard.out.mz_ptr, n) };
                let int_slice = unsafe { std::slice::from_raw_parts(guard.out.intensity_ptr, n) };
                let mz = mz_slice.to_vec();
                // Narrow intensity to f32 to match the TSF reader's IntensityArray dtype.
                let intensity: Vec<f32> = int_slice.iter().map(|&v| v as f32).collect();
                (mz, intensity)
            };

        // Copy succeeded: disarm the guard and free the unmanaged buffers explicitly.
        let mut out = guard.disarm();
        (self.api.free_spectrum)(&mut out as *mut SpectrumOut);
        Ok((mz, intensity, out))
    }

    /// Build the mzdata spectrum for scan `i` (0-based). Built EXACTLY like `bruker_tsf.rs`:
    /// Float64 MZArray (`Unit::MZ`) + Float32 IntensityArray (`Unit::DetectorCounts`), the
    /// `MS:1000294 "mass spectrum"` param, and a single `ScanEvent` with `start_time` in minutes.
    pub fn spectrum(&self, i: usize) -> Result<MultiLayerSpectrum> {
        if i >= self.count {
            bail!("Agilent spectrum index {i} out of range (count {})", self.count);
        }
        let (mz, intensity, meta) = self.fetch(i)?;

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

        let polarity = match meta.polarity {
            1 => ScanPolarity::Positive,
            -1 => ScanPolarity::Negative,
            _ => ScanPolarity::Unknown,
        };
        let continuity = if meta.is_centroid != 0 {
            SignalContinuity::Centroid
        } else {
            SignalContinuity::Profile
        };
        let ms_level = if meta.ms_level >= 1 { meta.ms_level as u8 } else { 1 };

        let mut descr = SpectrumDescription {
            id: format!("scanId={}", meta.scan_id),
            index: i,
            ms_level,
            signal_continuity: continuity,
            polarity,
            ..Default::default()
        };
        descr.add_param(Param::builder().name("mass spectrum").curie(curie!(MS:1000294)).build());

        let mut scan = ScanEvent::default();
        // Agilent reports RT in minutes; mzdata ScanEvent.start_time is also minutes.
        scan.start_time = meta.rt_minutes;
        descr.acquisition.scans.push(scan);

        // TODO(IM-MS): Agilent 6560 IM-QTOF stores a drift dimension that MHDAC does not expose;
        // the MIDAC SDK is required to read per-frame ion-mobility arrays. When that reader lands,
        // attach a mean_inverse_reduced_ion_mobility / IonMobilityArray here.

        Ok(MultiLayerSpectrum::new(descr, Some(arrays), None, None))
    }

    /// A sample spectrum's array map, for deriving the writer's data-facet schema. Mirrors
    /// `TsfReader::sample_arrays`: prefer the first non-empty spectrum so both columns are present.
    pub fn sample_arrays(&self) -> Result<BinaryArrayMap> {
        let mut idx = 0usize;
        for i in 0..self.count {
            // Cheap probe: a spectrum with points. fetch() is the only way to know the length, so
            // we accept the first that yields a non-empty m/z array, falling back to index 0.
            if let Ok((mz, _, _)) = self.fetch(i) {
                if !mz.is_empty() {
                    idx = i;
                    break;
                }
            }
        }
        self.spectrum(idx)?
            .arrays
            .clone()
            .ok_or_else(|| anyhow!("sample spectrum has no arrays"))
    }
}

impl Drop for AgilentReader {
    fn drop(&mut self) {
        // Release the .NET-side reader. Idempotent on the glue side.
        (self.api.close)(self.handle);
    }
}
