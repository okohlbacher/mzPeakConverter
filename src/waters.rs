//! Native Waters MassLynx (`.raw` directory) reader → mzdata spectra (native lane).
//!
//! ⚠️ WINDOWS-RUNTIME-ONLY AND UNTESTED. This module is verified to *compile* on any host
//! (the C# glue is reached through `netcorehost`, so no vendor SDK is needed to build), but
//! it can only *run* where the Waters MassLynx managed assemblies are present (sourced from a
//! MassLynx SDK install or a ProteoWizard install's `vendor_api/Waters`) and a .NET 8 runtime
//! is installed. There is no macOS build of the MassLynx stack; do not expect this to execute
//! on the development box.
//!
//! ## How it works
//!
//! Waters `.raw` is a vendor-closed *directory* format whose documented reader is the managed
//! MassLynx SDK (`MassLynxRaw.*`, the same DLLs ProteoWizard ships in `vendor_api/Waters`).
//! Rust cannot call those directly, so we host a CoreCLR runtime in-process via `netcorehost`
//! (mirroring `src/sciex.rs`) and load a thin C# shim (`WatersGlue.dll`, built from
//! `glue/waters/`). The shim, in turn, reaches the MassLynx API entirely through **runtime
//! reflection** (so the C# project itself compiles without the vendor DLLs present), opens the
//! `.raw`, and exposes a small C ABI of `[UnmanagedCallersOnly]` static methods that we call
//! as raw function pointers.
//!
//! ## Boot sequence (mirrors `src/sciex.rs` / `dotnetrawfilereader-sys/src/runtime.rs`)
//!
//!   1. `nethost::load_hostfxr()` — locate the installed hostfxr.
//!   2. `initialize_for_runtime_config(<glue dir>/WatersGlue.runtimeconfig.json)`.
//!   3. `get_delegate_loader_for_assembly(<glue dir>/WatersGlue.dll)`.
//!   4. `get_function_with_unmanaged_callers_only::<fn ...>(type, method)` per export.
//!
//! ## Path resolution (env vars)
//!
//!   * `MZPC_WATERS_GLUE`  — directory holding `WatersGlue.dll` + `WatersGlue.runtimeconfig.json`.
//!   * `MZPC_MASSLYNX_DIR` — MassLynx SDK directory (or a ProteoWizard root whose
//!     `vendor_api/Waters` holds the MassLynx DLLs). We pass that resolved directory to the
//!     glue so its reflection-based loader can `Assembly.LoadFrom` each `MassLynx*.dll`.
//!
//! ## C ABI contract (must match `glue/waters/Glue.cs` exactly)
//!
//! All strings cross the boundary as NUL-terminated UTF-16 (`*const u16`) — the native .NET
//! string encoding, so the C# side can wrap them with zero re-encoding. Buffers come back
//! via the **pointer + len + free** pattern: the managed side heap-allocates (pins) an
//! array, hands us `(ptr, len)`, we copy it into an owned `Vec`, then call the matching
//! `*_free` export so the managed side can release the pin. Handles are opaque `i64`
//! tokens into a managed handle table.
//!
//! ```text
//! Open(path_utf16: *const u16, sdk_dir_utf16: *const u16) -> i64
//!     // > 0 : opaque reader handle
//!     //   0 : open failed (LastError carries a detail string)
//!
//! Close(handle: i64)
//!     // release the reader + free all per-handle managed state. Idempotent on unknown handles.
//!
//! SpectrumCount(handle: i64) -> i64
//!     // total flattened spectra across all (function, scan) pairs, or -1 on error.
//!
//! SpectrumMeta(handle: i64, index: i64, out: *mut WatersSpectrumMeta) -> i32
//!     // 0 on success, non-zero on failure. Fills scalar metadata for one spectrum.
//!
//! GetSpectrum(
//!     handle: i64, index: i64,
//!     out_mz_ptr: *mut *const f64, out_int_ptr: *mut *const f32, out_len: *mut i64,
//! ) -> i32
//!     // 0 on success. On success writes a pointer to a pinned f64 m/z array, a pointer to a
//!     // pinned f32 intensity array, and their (shared) element count. The caller MUST copy
//!     // the data out and then call `FreeSpectrum(handle, mz_ptr, int_ptr)` to release the
//!     // pins. Both arrays have exactly `*out_len` elements.
//!
//! FreeSpectrum(handle: i64, mz_ptr: *const f64, int_ptr: *const f32)
//!     // release the pins handed out by the immediately preceding GetSpectrum.
//!
//! LastError(buf: *mut u16, cap: i32) -> i32
//!     // best-effort diagnostics: copy the glue's stashed last-error message (UTF-16, NOT
//!     // NUL-terminated unless room) into `buf` for up to `cap` code units, and return the FULL
//!     // length in UTF-16 code units (so the caller can detect truncation). A null `buf` or
//!     // `cap <= 0` just returns the needed length. Never throws across the boundary.
//! ```
//!
//! Each flattened spectrum becomes one mzdata [`MultiLayerSpectrum`], built EXACTLY like
//! [`crate::sciex`] / [`crate::bruker_tsf`]: an m/z `f64`/`Unit::MZ` array, an intensity
//! `f32`/`Unit::DetectorCounts` array, and a [`SpectrumDescription`] carrying id / index /
//! ms_level / polarity / the `MS:1000294` "mass spectrum" param, with `start_time` in minutes.

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

/// Hard cap on the number of points a single spectrum may report. Guards against a
/// corrupt/hostile glue or vendor library returning an enormous length that would exhaust
/// memory before we ever copy it. 100M points * (8 + 4) bytes ≈ 1.2 GiB.
const MAX_WATERS_SPECTRUM_POINTS: i64 = 100_000_000;

// --- C ABI mirror ----------------------------------------------------------

/// Scalar per-spectrum metadata, filled by `SpectrumMeta`. `#[repr(C)]` so the layout matches
/// the managed `struct` the glue marshals into (see `glue/waters/Glue.cs`).
///
/// `polarity` uses the same code convention as the SciEX/Bruker readers: 0 = positive, 1 =
/// negative, anything else = unknown. `signal_continuity`: 0 = profile, 1 = centroid.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct WatersSpectrumMeta {
    /// 1-based MassLynx function number.
    function: i32,
    /// 1-based scan number within the function.
    scan: i32,
    /// 1-based drift bin (IMS), or 0 when this is a non-IMS (summed) scan.
    drift: i32,
    /// 1-based public MS level (1 = MS1, 2 = MS2, …).
    ms_level: i32,
    /// 0 = positive, 1 = negative, other = unknown.
    polarity: i32,
    /// 0 = profile, 1 = centroid.
    signal_continuity: i32,
    /// Retention time in **seconds** (converted to minutes on the Rust side for mzdata).
    ///
    /// RT UNIT CONTRACT: the field carries SECONDS across the ABI. The C# glue multiplies
    /// MassLynx's native minutes by 60 to fill it; `WatersReader::spectrum` divides by 60 to
    /// recover minutes for mzdata's `start_time`. The two halves are deliberately symmetric —
    /// keep them in lockstep (see the matching note in `glue/waters/Glue.cs`).
    retention_time_seconds: f64,
}

// ABI layout assertion: 6 × i32 (4B) + 1 × f64 (8B). With `#[repr(C)]` natural alignment the
// f64 lands at offset 24 and the struct is exactly 32 bytes / 8-byte aligned. The C# side has a
// matching `Marshal.SizeOf` == 32 check in `Exports`'s static ctor. Any field drift fails the
// build here rather than corrupting memory at runtime.
const _: () = assert!(std::mem::size_of::<WatersSpectrumMeta>() == 32);
const _: () = assert!(std::mem::align_of::<WatersSpectrumMeta>() == 8);

// Function-pointer signatures for the glue's `[UnmanagedCallersOnly]` exports. The
// `extern "system"` calling convention matches what `UnmanagedCallersOnly` emits and what
// netcorehost's `get_function_with_unmanaged_callers_only` expects.
type WatersOpen = extern "system" fn(*const u16, *const u16) -> i64;
type WatersClose = extern "system" fn(i64);
type WatersSpectrumCount = extern "system" fn(i64) -> i64;
type WatersSpectrumMetaFn = extern "system" fn(i64, i64, *mut WatersSpectrumMeta) -> i32;
type WatersGetSpectrum =
    extern "system" fn(i64, i64, *mut *const f64, *mut *const f32, *mut i64) -> i32;
type WatersFreeSpectrum = extern "system" fn(i64, *const f64, *const f32);
/// `LastError(buf: *mut u16, cap: i32) -> i32` — fill-buffer diagnostics getter. Copies the
/// glue's stashed last-error message (UTF-16, NOT NUL-terminated unless room) into `buf` for up
/// to `cap` code units, and returns the FULL length in UTF-16 code units so the caller can detect
/// truncation. A null `buf` or `cap <= 0` just returns the needed length.
type WatersLastError = extern "system" fn(*mut u16, i32) -> i32;

/// Resolved + bound function pointers into the loaded `WatersGlue` assembly.
#[derive(Clone)]
struct GlueApi {
    // Keep the runtime alive for as long as any function pointer is held.
    _runtime: Arc<AssemblyDelegateLoader>,
    open: WatersOpen,
    close: WatersClose,
    spectrum_count: WatersSpectrumCount,
    spectrum_meta: WatersSpectrumMetaFn,
    get_spectrum: WatersGetSpectrum,
    free_spectrum: WatersFreeSpectrum,
    last_error: WatersLastError,
}

impl GlueApi {
    /// Boot the CoreCLR runtime against `WatersGlue.runtimeconfig.json` in `glue_dir`, load
    /// `WatersGlue.dll`, and resolve every `[UnmanagedCallersOnly]` export.
    fn load(glue_dir: &Path) -> Result<Self> {
        let runtime_config = glue_dir.join("WatersGlue.runtimeconfig.json");
        let assembly = glue_dir.join("WatersGlue.dll");
        if !runtime_config.is_file() {
            bail!(
                "WatersGlue.runtimeconfig.json not found in {} (set MZPC_WATERS_GLUE to the glue \
                 build output directory)",
                glue_dir.display()
            );
        }
        if !assembly.is_file() {
            bail!(
                "WatersGlue.dll not found in {} (build glue/waters with `dotnet build` and point \
                 MZPC_WATERS_GLUE at bin/.../net8.0)",
                glue_dir.display()
            );
        }

        let hostfxr = nethost::load_hostfxr().context(
            "failed to load hostfxr; a .NET 8 runtime must be installed to read Waters .raw natively",
        )?;

        let runtime_config_enc = path_to_pdcstring(&runtime_config)?;
        let context = hostfxr
            .initialize_for_runtime_config(runtime_config_enc)
            .context("initializing CoreCLR for WatersGlue.runtimeconfig.json")?;

        let assembly_enc = path_to_pdcstring(&assembly)?;
        let loader = Arc::new(
            context
                .get_delegate_loader_for_assembly(assembly_enc)
                .context("creating delegate loader for WatersGlue.dll")?,
        );

        // Assembly-qualified type name + method name for each export. The type is
        // `WatersGlue.Exports` in assembly `WatersGlue` (see Glue.cs).
        // `pdcstr!` isn't const-evaluable in netcorehost 0.18, so bind it at runtime (it's a
        // `&'static PdCStr`, reusable by reference across the resolves below).
        let ty = pdcstr!("WatersGlue.Exports, WatersGlue");

        let open = *loader
            .get_function_with_unmanaged_callers_only::<WatersOpen>(ty, pdcstr!("Open"))
            .map_err(|e| anyhow!("resolving glue export Open: {e}"))?;
        let close = *loader
            .get_function_with_unmanaged_callers_only::<WatersClose>(ty, pdcstr!("Close"))
            .map_err(|e| anyhow!("resolving glue export Close: {e}"))?;
        let spectrum_count = *loader
            .get_function_with_unmanaged_callers_only::<WatersSpectrumCount>(
                ty,
                pdcstr!("SpectrumCount"),
            )
            .map_err(|e| anyhow!("resolving glue export SpectrumCount: {e}"))?;
        let spectrum_meta = *loader
            .get_function_with_unmanaged_callers_only::<WatersSpectrumMetaFn>(
                ty,
                pdcstr!("SpectrumMeta"),
            )
            .map_err(|e| anyhow!("resolving glue export SpectrumMeta: {e}"))?;
        let get_spectrum = *loader
            .get_function_with_unmanaged_callers_only::<WatersGetSpectrum>(
                ty,
                pdcstr!("GetSpectrum"),
            )
            .map_err(|e| anyhow!("resolving glue export GetSpectrum: {e}"))?;
        let free_spectrum = *loader
            .get_function_with_unmanaged_callers_only::<WatersFreeSpectrum>(
                ty,
                pdcstr!("FreeSpectrum"),
            )
            .map_err(|e| anyhow!("resolving glue export FreeSpectrum: {e}"))?;
        let last_error = *loader
            .get_function_with_unmanaged_callers_only::<WatersLastError>(ty, pdcstr!("LastError"))
            .map_err(|e| anyhow!("resolving glue export LastError: {e}"))?;

        Ok(Self {
            _runtime: loader,
            open,
            close,
            spectrum_count,
            spectrum_meta,
            get_spectrum,
            free_spectrum,
            last_error,
        })
    }

    /// Best-effort retrieval of the glue's stashed last-error message. Calls `LastError` twice:
    /// once with a null buffer to learn the full UTF-16 length, then again to fill an exact-sized
    /// buffer. Returns `None` when there is no message (or the getter reports nothing). The getter
    /// is documented never to throw across the boundary, so this is purely diagnostic.
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

// --- public reader ---------------------------------------------------------

/// A native Waters `.raw` reader yielding one [`MultiLayerSpectrum`] per flattened
/// (function, scan) spectrum, built the same way the SciEX / Bruker readers build theirs.
///
/// ⚠️ Windows-runtime-only and untested (see module docs).
pub struct WatersReader {
    api: GlueApi,
    handle: i64,
    count: usize,
    raw_path: PathBuf,
    /// The managed handle / runtime is not known to be thread-safe and FFI calls through it
    /// must not happen concurrently. A raw-pointer marker makes [`WatersReader`] neither `Send`
    /// nor `Sync`, so the type system prevents cross-thread sharing. Sound for the existing
    /// single-threaded convert path.
    _not_thread_safe: PhantomData<*const ()>,
}

impl WatersReader {
    /// Open a Waters `.raw` directory. `MZPC_WATERS_GLUE` must point at the directory holding the
    /// built `WatersGlue.dll` + `WatersGlue.runtimeconfig.json`; `MZPC_MASSLYNX_DIR` must point at
    /// a MassLynx SDK directory (or a ProteoWizard install whose `vendor_api/Waters` subdirectory
    /// holds the MassLynx DLLs).
    pub fn open(path: &Path) -> Result<Self> {
        let glue_dir = std::env::var_os("MZPC_WATERS_GLUE")
            .map(PathBuf::from)
            .ok_or_else(|| {
                anyhow!(
                    "MZPC_WATERS_GLUE is not set; point it at the directory holding WatersGlue.dll \
                     (the `dotnet build` output of glue/waters, e.g. .../bin/Release/net8.0)"
                )
            })?;

        let sdk_dir = resolve_masslynx_dir()?;

        let api = GlueApi::load(&glue_dir)?;

        let path_utf16 = to_utf16_nul(path.as_os_str())
            .with_context(|| format!("encoding Waters .raw path {}", path.display()))?;
        let sdk_utf16 = to_utf16_nul(sdk_dir.as_os_str())
            .with_context(|| format!("encoding MassLynx dir {}", sdk_dir.display()))?;

        // SAFETY: both buffers are NUL-terminated UTF-16 owned for the duration of the call;
        // the glue copies what it needs (it does not retain the pointers).
        let handle = (api.open)(path_utf16.as_ptr(), sdk_utf16.as_ptr());
        if handle <= 0 {
            bail!(
                "Waters glue failed to open {} (MassLynx DLLs from {} could not read it, or the \
                 directory is not a valid Waters .raw). This path is Windows-runtime-only and \
                 untested: {}",
                path.display(),
                sdk_dir.display(),
                api.last_error().unwrap_or_default()
            );
        }

        let count_i64 = (api.spectrum_count)(handle);
        if count_i64 < 0 {
            let detail = api.last_error().unwrap_or_default();
            (api.close)(handle);
            bail!("Waters glue reported a spectrum-count error for {}: {detail}", path.display());
        }
        // close-on-error so a count that overflows usize doesn't leak the open handle.
        let count = match usize::try_from(count_i64) {
            Ok(c) => c,
            Err(_) => {
                (api.close)(handle);
                bail!("Waters spectrum count {count_i64} does not fit in usize");
            }
        };

        Ok(Self {
            api,
            handle,
            count,
            raw_path: path.to_path_buf(),
            _not_thread_safe: PhantomData,
        })
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn raw_path(&self) -> &Path {
        &self.raw_path
    }

    /// Fetch one spectrum's scalar metadata via the glue.
    fn meta(&self, i: usize) -> Result<WatersSpectrumMeta> {
        let index = i64::try_from(i).map_err(|_| anyhow!("Waters index {i} does not fit in i64"))?;
        let mut meta = WatersSpectrumMeta::default();
        // SAFETY: `meta` is a valid, writable, correctly-laid-out destination for the glue.
        let rc = (self.api.spectrum_meta)(self.handle, index, &mut meta as *mut _);
        if rc != 0 {
            bail!(
                "Waters glue SpectrumMeta failed for index {i} (rc {rc}): {}",
                self.api.last_error().unwrap_or_default()
            );
        }
        Ok(meta)
    }

    /// Fetch one spectrum's `(m/z f64, intensity f32)` arrays via the glue, copying them into
    /// owned `Vec`s and releasing the managed pins. Both arrays share one length.
    fn peaks(&self, i: usize) -> Result<(Vec<f64>, Vec<f32>)> {
        let index = i64::try_from(i).map_err(|_| anyhow!("Waters index {i} does not fit in i64"))?;
        let mut mz_ptr: *const f64 = std::ptr::null();
        let mut int_ptr: *const f32 = std::ptr::null();
        let mut len: i64 = 0;

        // SAFETY: all three out-params are valid writable locals. On success the glue writes
        // two pinned array pointers and a shared length; we own the obligation to call
        // `free_spectrum` afterwards (done unconditionally below).
        let rc = (self.api.get_spectrum)(
            self.handle,
            index,
            &mut mz_ptr as *mut _,
            &mut int_ptr as *mut _,
            &mut len as *mut _,
        );
        if rc != 0 {
            bail!(
                "Waters glue GetSpectrum failed for index {i} (rc {rc}): {}",
                self.api.last_error().unwrap_or_default()
            );
        }

        // RAII guard: FreeSpectrum must run for the pins GetSpectrum handed out, even if a panic
        // unwinds through the validation/copy below. A manual call at the end would be skipped on
        // panic, leaking the managed pins (and the underlying arrays) permanently. The guard's Drop
        // releases them; we disarm it only after the copy has completed.
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
                    // Passing a null pointer is a no-op on the glue side.
                    (self.api.free_spectrum)(self.handle, self.mz_ptr, self.int_ptr);
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

        // Validate length before trusting the pointers; the guard frees the pins regardless of
        // the validation outcome (including the early bails below).
        if len < 0 {
            bail!("Waters spectrum {i} reports negative length {len}");
        }
        if len > MAX_WATERS_SPECTRUM_POINTS {
            bail!(
                "Waters spectrum {i} reports {len} points, exceeding safety limit \
                 {MAX_WATERS_SPECTRUM_POINTS}"
            );
        }
        let n = len as usize;
        if n == 0 {
            // Nothing pinned for an empty spectrum, but still let the guard call free_spectrum
            // (null pointers => no-op) for uniformity.
            return Ok((Vec::new(), Vec::new()));
        }
        if mz_ptr.is_null() || int_ptr.is_null() {
            bail!("Waters spectrum {i} reports {n} points but a data pointer is null");
        }
        // SAFETY: the glue guarantees both arrays hold exactly `n` elements, pinned and
        // valid until `free_spectrum`. We copy (not alias) into owned Vecs here.
        let mz = unsafe { std::slice::from_raw_parts(mz_ptr, n) }.to_vec();
        let intensity = unsafe { std::slice::from_raw_parts(int_ptr, n) }.to_vec();

        // Copy completed; release the pins now (guard would do the same, but make it explicit and
        // disarm so Drop doesn't double-free — free_spectrum already removed the entry, but
        // disarming keeps the contract single-call).
        guard.armed = false;
        (self.api.free_spectrum)(self.handle, mz_ptr, int_ptr);

        Ok((mz, intensity))
    }

    /// Build the mzdata spectrum for spectrum `i` (0-based reader order). Built identically to
    /// [`crate::sciex::SciexReader::spectrum`].
    pub fn spectrum(&self, i: usize) -> Result<MultiLayerSpectrum> {
        if i >= self.count {
            bail!("Waters spectrum index {i} out of range (len {})", self.count);
        }
        let meta = self.meta(i)?;
        let (mz, intensity) = self.peaks(i)?;

        let mut arrays = BinaryArrayMap::new();
        let mut mz_da =
            DataArray::wrap(&ArrayType::MZArray, BinaryDataArrayType::Float64, Vec::new());
        mz_da
            .update_buffer(mz.as_slice())
            .map_err(|e| anyhow!("encoding m/z: {e}"))?;
        mz_da.unit = Unit::MZ;
        arrays.add(mz_da);
        let mut int_da = DataArray::wrap(
            &ArrayType::IntensityArray,
            BinaryDataArrayType::Float32,
            Vec::new(),
        );
        int_da
            .update_buffer(intensity.as_slice())
            .map_err(|e| anyhow!("encoding intensity: {e}"))?;
        int_da.unit = Unit::DetectorCounts;
        arrays.add(int_da);

        let ms_level = u8::try_from(meta.ms_level.max(1))
            .map_err(|_| anyhow!("Waters spectrum {i} reports implausible MS level {}", meta.ms_level))?;
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
            // Waters native ids follow a "function=N process=0 scan=N" convention so downstream
            // identifiers line up with msconvert output for the same file.
            id: format!(
                "function={} process=0 scan={}",
                meta.function, meta.scan
            ),
            index: i,
            ms_level,
            signal_continuity,
            polarity,
            ..Default::default()
        };
        descr.add_param(
            Param::builder()
                .name("mass spectrum")
                .curie(curie!(MS:1000294))
                .build(),
        );
        let mut scan = ScanEvent::default();
        // RT UNIT CONTRACT: the ABI field is seconds (C# multiplies MassLynx minutes by 60);
        // mzdata's scan start_time is minutes, so divide by 60 here. See the field doc on
        // `WatersSpectrumMeta` and the matching note in `glue/waters/Glue.cs`.
        scan.start_time = meta.retention_time_seconds / 60.0;
        descr.acquisition.scans.push(scan);

        Ok(MultiLayerSpectrum::new(descr, Some(arrays), None, None))
    }

    /// A sample spectrum's array map, for deriving the writer's data-facet schema (mirrors
    /// `SciexReader::sample_arrays`). Uses the first non-empty spectrum so both columns are
    /// actually present.
    pub fn sample_arrays(&self) -> Result<BinaryArrayMap> {
        let mut chosen = 0usize;
        for i in 0..self.count {
            // A non-empty spectrum is preferable so the m/z + intensity columns are present.
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

impl Drop for WatersReader {
    fn drop(&mut self) {
        if self.handle > 0 {
            // SAFETY: handle was returned by the glue's Open and is closed exactly once.
            (self.api.close)(self.handle);
            self.handle = 0;
        }
    }
}

// --- helpers ---------------------------------------------------------------

/// Resolve the directory holding the MassLynx managed DLLs from `MZPC_MASSLYNX_DIR`.
///
/// `MZPC_MASSLYNX_DIR` is either a MassLynx SDK directory that directly holds the managed DLLs,
/// or a ProteoWizard install root whose Waters vendor assemblies live under `vendor_api/Waters`.
/// If `MZPC_MASSLYNX_DIR` already directly contains the MassLynx DLLs we accept it as-is.
fn resolve_masslynx_dir() -> Result<PathBuf> {
    let root = std::env::var_os("MZPC_MASSLYNX_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| {
            anyhow!(
                "MZPC_MASSLYNX_DIR is not set; point it at a MassLynx SDK directory (or a \
                 ProteoWizard install whose vendor_api/Waters directory holds the MassLynx DLLs)"
            )
        })?;

    // Probe, in order: <root>/vendor_api/Waters, then <root> itself.
    let waters = root.join("vendor_api").join("Waters");
    let candidates = [waters, root.clone()];
    for cand in &candidates {
        if cand.is_dir() && dir_has_masslynx(cand) {
            return Ok(cand.clone());
        }
    }
    // Fall back to the canonical subdir even if we can't confirm the DLLs, so the glue can
    // emit the more specific error — but prefer the Waters path if it at least exists.
    let waters = root.join("vendor_api").join("Waters");
    if waters.is_dir() {
        return Ok(waters);
    }
    Ok(root)
}

/// True if `dir` contains at least one `MassLynx*.dll` (best-effort confirmation).
fn dir_has_masslynx(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        if let Some(name) = entry.file_name().to_str() {
            let lower = name.to_ascii_lowercase();
            if lower.starts_with("masslynx") && lower.ends_with(".dll") {
                return true;
            }
        }
    }
    false
}

/// Encode an `OsStr` path as a NUL-terminated UTF-16 buffer for the managed string boundary.
///
/// On Windows we use `OsStrExt::encode_wide`, which preserves the exact UTF-16 of the underlying
/// path (no lossy re-encoding that could mangle non-UTF-8 names). On other platforms (the dev box)
/// we fall back to `to_string_lossy` — those targets never actually run this code, but it must
/// still compile. Either way we reject an interior NUL: it would truncate the C string the managed
/// side reads with `Marshal.PtrToStringUni`, silently pointing at the wrong file.
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

/// Encode a path as a `PdCString` (the platform-native wide/narrow C string netcorehost wants).
fn path_to_pdcstring(p: &Path) -> Result<PdCString> {
    p.to_string_lossy()
        .parse()
        .map_err(|e| anyhow!("encoding path {} for the .NET host: {e}", p.display()))
}
