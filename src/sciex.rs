//! Native SciEX (`.wiff` / `.wiff2`) reader → mzdata spectra (PLAN §3.7, native lane).
//!
//! ⚠️ WINDOWS-RUNTIME-ONLY AND UNTESTED. This module is verified to *compile* behind the
//! `sciex` cargo feature on any host, but it can only *run* where the SciEX Clearcore2
//! managed assemblies are present (sourced from a ProteoWizard install) and a .NET 8
//! runtime is installed. There is no macOS build of the Clearcore2 stack; do not expect
//! this to execute on the development box.
//!
//! ## How it works
//!
//! SciEX WIFF is a vendor-closed format whose only documented reader is the managed
//! `Clearcore2.*` .NET assembly family (the same DLLs ProteoWizard ships in
//! `vendor_api/ABI`). Rust cannot call those directly, so we host a CoreCLR runtime in-
//! process via `netcorehost` (mirroring `dotnetrawfilereader-sys`'s boot pattern) and load
//! a thin C# shim (`SciexGlue.dll`, built from `glue/sciex/`). The shim, in turn, reaches
//! the Clearcore2 API entirely through **runtime reflection** (so the C# project itself
//! compiles without the vendor DLLs present), opens the WIFF, and exposes a small C ABI of
//! `[UnmanagedCallersOnly]` static methods that we call as raw function pointers.
//!
//! ## Boot sequence (mirrors `dotnetrawfilereader-sys/src/runtime.rs`)
//!
//!   1. `nethost::load_hostfxr()` — locate the installed hostfxr.
//!   2. `initialize_for_runtime_config(<glue dir>/SciexGlue.runtimeconfig.json)`.
//!   3. `get_delegate_loader_for_assembly(<glue dir>/SciexGlue.dll)`.
//!   4. `get_function_with_unmanaged_callers_only::<fn ...>(type, method)` per export.
//!
//! ## Path resolution (env vars)
//!
//!   * `MZPC_SCIEX_GLUE` — directory holding `SciexGlue.dll` + `SciexGlue.runtimeconfig.json`.
//!   * `MZPC_PWIZ_DIR`   — ProteoWizard install root; the Clearcore2 DLLs live under
//!     `<MZPC_PWIZ_DIR>/vendor_api/ABI`. We pass that resolved directory to the glue so its
//!     reflection-based loader can `Assembly.LoadFrom` each `Clearcore2.*.dll`.
//!
//! ## C ABI contract (must match `glue/sciex/Glue.cs` exactly)
//!
//! All strings cross the boundary as NUL-terminated UTF-16 (`*const u16`) — the native .NET
//! string encoding, so the C# side can wrap them with zero re-encoding. Buffers come back
//! via the **pointer + len + free** pattern: the managed side heap-allocates (pins) an
//! array, hands us `(ptr, len)`, we copy it into an owned `Vec`, then call the matching
//! `*_free` export so the managed side can release the pin. Handles are opaque `i64`
//! tokens into a managed handle table.
//!
//! ```text
//! sciex_open(path_utf16: *const u16, pwiz_dir_utf16: *const u16) -> i64
//!     // > 0 : opaque reader handle
//!     //   0 : open failed (no detailed error across the ABI; treated as a generic failure)
//!
//! sciex_close(handle: i64)
//!     // release the reader + free all per-handle managed state. Idempotent on unknown handles.
//!
//! sciex_spectrum_count(handle: i64) -> i64
//!     // total flattened spectra across all samples/experiments/cycles, or -1 on error.
//!
//! sciex_spectrum_meta(handle: i64, index: i64, out: *mut SciexSpectrumMeta) -> i32
//!     // 0 on success, non-zero on failure. Fills scalar metadata for one spectrum.
//!
//! sciex_spectrum_data(
//!     handle: i64, index: i64,
//!     out_mz_ptr: *mut *const f64, out_int_ptr: *mut *const f32, out_len: *mut i64,
//! ) -> i32
//!     // 0 on success. On success writes a pointer to a pinned f64 m/z array, a pointer to a
//!     // pinned f32 intensity array, and their (shared) element count. The caller MUST copy
//!     // the data out and then call `sciex_data_free(handle, mz_ptr, int_ptr)` to release the
//!     // pins. Both arrays have exactly `*out_len` elements.
//!
//! sciex_data_free(handle: i64, mz_ptr: *const f64, int_ptr: *const f32)
//!     // release the pins handed out by the immediately preceding sciex_spectrum_data.
//!
//! LastError(buf: *mut u16, cap: i32) -> i32
//!     // best-effort diagnostics: copy the glue's stashed last-error message (UTF-16, NOT
//!     // NUL-terminated unless room) into `buf` for up to `cap` code units, and return the FULL
//!     // length in UTF-16 code units (so the caller can detect truncation). A null `buf` or
//!     // `cap <= 0` just returns the needed length. Never throws across the boundary.
//! ```
//!
//! Each flattened spectrum becomes one mzdata [`MultiLayerSpectrum`], built EXACTLY like
//! [`crate::bruker_tsf`] / [`crate::bruker_baf`]: an m/z `f64`/`Unit::MZ` array, an intensity
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
const MAX_SCIEX_SPECTRUM_POINTS: i64 = 100_000_000;

// --- C ABI mirror ----------------------------------------------------------

/// Scalar per-spectrum metadata, filled by `sciex_spectrum_meta`. `#[repr(C)]` so the layout
/// matches the managed `struct` the glue marshals into (see `glue/sciex/Glue.cs`).
///
/// `polarity` uses the same code convention as the Bruker readers: 0 = positive, 1 = negative,
/// anything else = unknown. `signal_continuity`: 0 = profile, 1 = centroid.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SciexSpectrumMeta {
    /// 1-based sample number within the WIFF.
    sample: i32,
    /// 1-based experiment (period/experiment) number within the sample.
    experiment: i32,
    /// 1-based cycle (scan) number within the experiment.
    cycle: i32,
    /// 1-based public MS level (1 = MS1, 2 = MS2, …).
    ms_level: i32,
    /// 0 = positive, 1 = negative, other = unknown.
    polarity: i32,
    /// 0 = profile, 1 = centroid.
    signal_continuity: i32,
    /// Retention time in **seconds** (converted to minutes on the Rust side for mzdata).
    ///
    /// RT UNIT CONTRACT: the field carries SECONDS across the ABI. The C# glue multiplies
    /// Clearcore2's native minutes by 60 to fill it; `SciexReader::spectrum` divides by 60 to
    /// recover minutes for mzdata's `start_time`. The two halves are deliberately symmetric —
    /// keep them in lockstep (see the matching note in `glue/sciex/Glue.cs`).
    retention_time_seconds: f64,
}

// ABI layout assertion (finding #11): 6 × i32 (4B) + 1 × f64 (8B). With `#[repr(C)]` natural
// alignment the f64 lands at offset 24 and the struct is exactly 32 bytes / 8-byte aligned. The
// C# side has a matching `Marshal.SizeOf` == 32 check in `Exports`'s static ctor. Any field
// drift fails the build here rather than corrupting memory at runtime.
const _: () = assert!(std::mem::size_of::<SciexSpectrumMeta>() == 32);
const _: () = assert!(std::mem::align_of::<SciexSpectrumMeta>() == 8);

// Function-pointer signatures for the glue's `[UnmanagedCallersOnly]` exports. The
// `extern "system"` calling convention matches what `UnmanagedCallersOnly` emits and what
// netcorehost's `get_function_with_unmanaged_callers_only` expects.
type SciexOpen = extern "system" fn(*const u16, *const u16) -> i64;
type SciexClose = extern "system" fn(i64);
type SciexSpectrumCount = extern "system" fn(i64) -> i64;
type SciexSpectrumMetaFn = extern "system" fn(i64, i64, *mut SciexSpectrumMeta) -> i32;
type SciexSpectrumData =
    extern "system" fn(i64, i64, *mut *const f64, *mut *const f32, *mut i64) -> i32;
type SciexDataFree = extern "system" fn(i64, *const f64, *const f32);
/// `LastError(buf: *mut u16, cap: i32) -> i32` — fill-buffer diagnostics getter. Copies the
/// glue's stashed last-error message (UTF-16, NOT NUL-terminated unless room) into `buf` for up
/// to `cap` code units, and returns the FULL length in UTF-16 code units so the caller can detect
/// truncation. A null `buf` or `cap <= 0` just returns the needed length.
type SciexLastError = extern "system" fn(*mut u16, i32) -> i32;

/// Resolved + bound function pointers into the loaded `SciexGlue` assembly.
#[derive(Clone)]
struct GlueApi {
    // Keep the runtime alive for as long as any function pointer is held.
    _runtime: Arc<AssemblyDelegateLoader>,
    open: SciexOpen,
    close: SciexClose,
    spectrum_count: SciexSpectrumCount,
    spectrum_meta: SciexSpectrumMetaFn,
    spectrum_data: SciexSpectrumData,
    data_free: SciexDataFree,
    last_error: SciexLastError,
}

impl GlueApi {
    /// Boot the CoreCLR runtime against `SciexGlue.runtimeconfig.json` in `glue_dir`, load
    /// `SciexGlue.dll`, and resolve every `[UnmanagedCallersOnly]` export.
    fn load(glue_dir: &Path) -> Result<Self> {
        let runtime_config = glue_dir.join("SciexGlue.runtimeconfig.json");
        let assembly = glue_dir.join("SciexGlue.dll");
        if !runtime_config.is_file() {
            bail!(
                "SciexGlue.runtimeconfig.json not found in {} (set MZPC_SCIEX_GLUE to the glue \
                 build output directory)",
                glue_dir.display()
            );
        }
        if !assembly.is_file() {
            bail!(
                "SciexGlue.dll not found in {} (build glue/sciex with `dotnet build` and point \
                 MZPC_SCIEX_GLUE at bin/.../net8.0)",
                glue_dir.display()
            );
        }

        let hostfxr = nethost::load_hostfxr().context(
            "failed to load hostfxr; a .NET 8 runtime must be installed to read SciEX WIFF natively",
        )?;

        let runtime_config_enc = path_to_pdcstring(&runtime_config)?;
        let context = hostfxr
            .initialize_for_runtime_config(runtime_config_enc)
            .context("initializing CoreCLR for SciexGlue.runtimeconfig.json")?;

        let assembly_enc = path_to_pdcstring(&assembly)?;
        let loader = Arc::new(
            context
                .get_delegate_loader_for_assembly(assembly_enc)
                .context("creating delegate loader for SciexGlue.dll")?,
        );

        // Assembly-qualified type name + method name for each export. The type is
        // `SciexGlue.Exports` in assembly `SciexGlue` (see Glue.cs).
        // `pdcstr!` isn't const-evaluable in netcorehost 0.18, so bind it at runtime (it's a
        // `&'static PdCStr`, reusable by reference across the resolves below).
        let ty = pdcstr!("SciexGlue.Exports, SciexGlue");

        let open = *loader
            .get_function_with_unmanaged_callers_only::<SciexOpen>(ty, pdcstr!("Open"))
            .map_err(|e| anyhow!("resolving glue export Open: {e}"))?;
        let close = *loader
            .get_function_with_unmanaged_callers_only::<SciexClose>(ty, pdcstr!("Close"))
            .map_err(|e| anyhow!("resolving glue export Close: {e}"))?;
        let spectrum_count = *loader
            .get_function_with_unmanaged_callers_only::<SciexSpectrumCount>(
                ty,
                pdcstr!("SpectrumCount"),
            )
            .map_err(|e| anyhow!("resolving glue export SpectrumCount: {e}"))?;
        let spectrum_meta = *loader
            .get_function_with_unmanaged_callers_only::<SciexSpectrumMetaFn>(
                ty,
                pdcstr!("SpectrumMeta"),
            )
            .map_err(|e| anyhow!("resolving glue export SpectrumMeta: {e}"))?;
        let spectrum_data = *loader
            .get_function_with_unmanaged_callers_only::<SciexSpectrumData>(
                ty,
                pdcstr!("SpectrumData"),
            )
            .map_err(|e| anyhow!("resolving glue export SpectrumData: {e}"))?;
        let data_free = *loader
            .get_function_with_unmanaged_callers_only::<SciexDataFree>(ty, pdcstr!("DataFree"))
            .map_err(|e| anyhow!("resolving glue export DataFree: {e}"))?;
        let last_error = *loader
            .get_function_with_unmanaged_callers_only::<SciexLastError>(ty, pdcstr!("LastError"))
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

/// A native SciEX `.wiff`/`.wiff2` reader yielding one [`MultiLayerSpectrum`] per flattened
/// (sample, experiment, cycle) spectrum, built the same way the Bruker readers build theirs.
///
/// ⚠️ Windows-runtime-only and untested (see module docs).
pub struct SciexReader {
    api: GlueApi,
    handle: i64,
    count: usize,
    wiff_path: PathBuf,
    /// The managed handle / runtime is not known to be thread-safe and FFI calls through it
    /// must not happen concurrently. A raw-pointer marker makes [`SciexReader`] neither `Send`
    /// nor `Sync`, so the type system prevents cross-thread sharing. Sound for the existing
    /// single-threaded convert path.
    _not_thread_safe: PhantomData<*const ()>,
}

impl SciexReader {
    /// Open a WIFF file. `MZPC_SCIEX_GLUE` must point at the directory holding the built
    /// `SciexGlue.dll` + `SciexGlue.runtimeconfig.json`; `MZPC_PWIZ_DIR` must point at a
    /// ProteoWizard install whose `vendor_api/ABI` subdirectory holds the Clearcore2 DLLs.
    pub fn open(path: &Path) -> Result<Self> {
        let glue_dir = std::env::var_os("MZPC_SCIEX_GLUE")
            .map(PathBuf::from)
            .ok_or_else(|| {
                anyhow!(
                    "MZPC_SCIEX_GLUE is not set; point it at the directory holding SciexGlue.dll \
                     (the `dotnet build` output of glue/sciex, e.g. .../bin/Release/net8.0)"
                )
            })?;

        let pwiz_dir = resolve_clearcore2_dir()?;

        let api = GlueApi::load(&glue_dir)?;

        let path_utf16 = to_utf16_nul(path.as_os_str())
            .with_context(|| format!("encoding WIFF path {}", path.display()))?;
        let pwiz_utf16 = to_utf16_nul(pwiz_dir.as_os_str())
            .with_context(|| format!("encoding pwiz dir {}", pwiz_dir.display()))?;

        // SAFETY: both buffers are NUL-terminated UTF-16 owned for the duration of the call;
        // the glue copies what it needs (it does not retain the pointers).
        let handle = (api.open)(path_utf16.as_ptr(), pwiz_utf16.as_ptr());
        if handle <= 0 {
            bail!(
                "SciEX glue failed to open {} (Clearcore2 DLLs from {} could not read it, or the \
                 file is not a valid WIFF). This path is Windows-runtime-only and untested: {}",
                path.display(),
                pwiz_dir.display(),
                api.last_error().unwrap_or_default()
            );
        }

        let count_i64 = (api.spectrum_count)(handle);
        if count_i64 < 0 {
            let detail = api.last_error().unwrap_or_default();
            (api.close)(handle);
            bail!("SciEX glue reported a spectrum-count error for {}: {detail}", path.display());
        }
        // Finding #10: close-on-error so a count that overflows usize doesn't leak the open handle.
        let count = match usize::try_from(count_i64) {
            Ok(c) => c,
            Err(_) => {
                (api.close)(handle);
                bail!("SciEX spectrum count {count_i64} does not fit in usize");
            }
        };

        Ok(Self {
            api,
            handle,
            count,
            wiff_path: path.to_path_buf(),
            _not_thread_safe: PhantomData,
        })
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn wiff_path(&self) -> &Path {
        &self.wiff_path
    }

    /// Fetch one spectrum's scalar metadata via the glue.
    fn meta(&self, i: usize) -> Result<SciexSpectrumMeta> {
        let index = i64::try_from(i).map_err(|_| anyhow!("SciEX index {i} does not fit in i64"))?;
        let mut meta = SciexSpectrumMeta::default();
        // SAFETY: `meta` is a valid, writable, correctly-laid-out destination for the glue.
        let rc = (self.api.spectrum_meta)(self.handle, index, &mut meta as *mut _);
        if rc != 0 {
            bail!(
                "SciEX glue SpectrumMeta failed for index {i} (rc {rc}): {}",
                self.api.last_error().unwrap_or_default()
            );
        }
        Ok(meta)
    }

    /// Fetch one spectrum's `(m/z f64, intensity f32)` arrays via the glue, copying them into
    /// owned `Vec`s and releasing the managed pins. Both arrays share one length.
    fn peaks(&self, i: usize) -> Result<(Vec<f64>, Vec<f32>)> {
        let index = i64::try_from(i).map_err(|_| anyhow!("SciEX index {i} does not fit in i64"))?;
        let mut mz_ptr: *const f64 = std::ptr::null();
        let mut int_ptr: *const f32 = std::ptr::null();
        let mut len: i64 = 0;

        // SAFETY: all three out-params are valid writable locals. On success the glue writes
        // two pinned array pointers and a shared length; we own the obligation to call
        // `data_free` afterwards (done unconditionally below).
        let rc = (self.api.spectrum_data)(
            self.handle,
            index,
            &mut mz_ptr as *mut _,
            &mut int_ptr as *mut _,
            &mut len as *mut _,
        );
        if rc != 0 {
            bail!(
                "SciEX glue SpectrumData failed for index {i} (rc {rc}): {}",
                self.api.last_error().unwrap_or_default()
            );
        }

        // RAII guard (finding #3): DataFree must run for the pins SpectrumData handed out, even
        // if a panic unwinds through the validation/copy below. A manual call at the end would be
        // skipped on panic, leaking the managed pins (and the underlying arrays) permanently. The
        // guard's Drop releases them; we disarm it only after the copy has completed.
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

        // Validate length before trusting the pointers; the guard frees the pins regardless of
        // the validation outcome (including the early bails below).
        if len < 0 {
            bail!("SciEX spectrum {i} reports negative length {len}");
        }
        if len > MAX_SCIEX_SPECTRUM_POINTS {
            bail!(
                "SciEX spectrum {i} reports {len} points, exceeding safety limit \
                 {MAX_SCIEX_SPECTRUM_POINTS}"
            );
        }
        let n = len as usize;
        if n == 0 {
            // Nothing pinned for an empty spectrum, but still let the guard call data_free (null
            // pointers => no-op) for uniformity.
            return Ok((Vec::new(), Vec::new()));
        }
        if mz_ptr.is_null() || int_ptr.is_null() {
            bail!("SciEX spectrum {i} reports {n} points but a data pointer is null");
        }
        // SAFETY: the glue guarantees both arrays hold exactly `n` elements, pinned and
        // valid until `data_free`. We copy (not alias) into owned Vecs here.
        let mz = unsafe { std::slice::from_raw_parts(mz_ptr, n) }.to_vec();
        let intensity = unsafe { std::slice::from_raw_parts(int_ptr, n) }.to_vec();

        // Copy completed; release the pins now (guard would do the same, but make it explicit and
        // disarm so Drop doesn't double-free — data_free already removed the entry, but disarming
        // keeps the contract single-call).
        guard.armed = false;
        (self.api.data_free)(self.handle, mz_ptr, int_ptr);

        Ok((mz, intensity))
    }

    /// Build the mzdata spectrum for spectrum `i` (0-based reader order). Built identically to
    /// [`crate::bruker_tsf::TsfReader::spectrum`].
    pub fn spectrum(&self, i: usize) -> Result<MultiLayerSpectrum> {
        if i >= self.count {
            bail!("SciEX spectrum index {i} out of range (len {})", self.count);
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
            .map_err(|_| anyhow!("SciEX spectrum {i} reports implausible MS level {}", meta.ms_level))?;
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
            // SciEX native ids follow the ProteoWizard "sample=N period=N cycle=N experiment=N"
            // convention so downstream identifiers line up with msconvert output.
            id: format!(
                "sample={} period=1 cycle={} experiment={}",
                meta.sample, meta.cycle, meta.experiment
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
        // RT UNIT CONTRACT: the ABI field is seconds (C# multiplies Clearcore2 minutes by 60);
        // mzdata's scan start_time is minutes, so divide by 60 here. See the field doc on
        // `SciexSpectrumMeta` and the matching note in `glue/sciex/Glue.cs`.
        scan.start_time = meta.retention_time_seconds / 60.0;
        descr.acquisition.scans.push(scan);

        Ok(MultiLayerSpectrum::new(descr, Some(arrays), None, None))
    }

    /// A sample spectrum's array map, for deriving the writer's data-facet schema (mirrors
    /// `TsfReader::sample_arrays` / `BafReader::sample_arrays`). Uses the first non-empty
    /// spectrum so both columns are actually present.
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

impl Drop for SciexReader {
    fn drop(&mut self) {
        if self.handle > 0 {
            // SAFETY: handle was returned by the glue's Open and is closed exactly once.
            (self.api.close)(self.handle);
            self.handle = 0;
        }
    }
}

// --- helpers ---------------------------------------------------------------

/// Resolve the directory holding the Clearcore2 vendor DLLs from `MZPC_PWIZ_DIR`.
///
/// `MZPC_PWIZ_DIR` is a ProteoWizard install root; the vendor assemblies live under
/// `vendor_api/ABI`. If `MZPC_PWIZ_DIR` already *is* that `ABI` directory (or otherwise
/// directly contains the Clearcore2 DLLs) we accept it as-is.
fn resolve_clearcore2_dir() -> Result<PathBuf> {
    let root = std::env::var_os("MZPC_PWIZ_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| {
            anyhow!(
                "MZPC_PWIZ_DIR is not set; point it at a ProteoWizard install whose vendor_api/ABI \
                 directory holds the Clearcore2 DLLs"
            )
        })?;

    // Probe, in order: <root>/vendor_api/ABI, then <root> itself.
    let abi = root.join("vendor_api").join("ABI");
    let candidates = [abi, root.clone()];
    for cand in &candidates {
        if cand.is_dir() && dir_has_clearcore2(cand) {
            return Ok(cand.clone());
        }
    }
    // Fall back to the canonical subdir even if we can't confirm the DLLs, so the glue can
    // emit the more specific error — but prefer the ABI path if it at least exists.
    let abi = root.join("vendor_api").join("ABI");
    if abi.is_dir() {
        return Ok(abi);
    }
    Ok(root)
}

/// True if `dir` contains at least one `Clearcore2*.dll` (best-effort confirmation).
fn dir_has_clearcore2(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        if let Some(name) = entry.file_name().to_str() {
            let lower = name.to_ascii_lowercase();
            if lower.starts_with("clearcore2") && lower.ends_with(".dll") {
                return true;
            }
        }
    }
    false
}

/// Encode an `OsStr` path as a NUL-terminated UTF-16 buffer for the managed string boundary.
///
/// Finding #8: on Windows we use `OsStrExt::encode_wide`, which preserves the exact UTF-16 of the
/// underlying path (no lossy re-encoding that could mangle non-UTF-8 names). On other platforms
/// (the dev box) we fall back to `to_string_lossy` — those targets never actually run this code,
/// but it must still compile. Either way we reject an interior NUL: it would truncate the C string
/// the managed side reads with `Marshal.PtrToStringUni`, silently pointing at the wrong file.
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
