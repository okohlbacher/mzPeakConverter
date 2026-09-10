//! Native SciEX (`.wiff` / `.wiff2`) reader → mzdata spectra (PLAN §3.7, native lane).
//!
//! ⚠️ WINDOWS-ONLY. `main.rs` compiles this module only under `#[cfg(windows)]` (there is no cargo
//! feature), and it runs only where the SciEX Clearcore2 managed assemblies (from a ProteoWizard
//! install) and a .NET 8 runtime are present; there is no macOS or Linux build of that stack. The
//! lane built the published native SciEX corpus archives. What it decides without the glue lives in
//! `src/sciex_run.rs`, tested on every host, and `tests/sciex_abi_pin.rs` holds this file and
//! `glue/sciex/Glue.cs` to one ABI. Not yet run on Windows: the precursor read (glue ABI 2), the
//! value-change counts (ABI 3), the version handshake and the once-per-process boot.
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
//! Like that crate's `BUNDLE`, the loaded glue is kept in a process-wide static (`GLUE`): hostfxr
//! cannot be initialised again once the first handle is gone, and `-v` opens the reader twice.
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
//! SciexAbiVersion() -> i32
//!     // the glue's ABI generation. `GlueApi::load` refuses any other than `REQUIRED_ABI_VERSION`
//!     // (a DLL without the export counts as 1) before it resolves a versioned export.
//!
//! sciex_spectrum_meta(handle: i64, index: i64, out: *mut SciexSpectrumMeta) -> i32
//!     // 0 on success, non-zero on failure. Fills scalar metadata for one spectrum. Kept for a
//!     // binary that predates the handshake; this one reads `SpectrumMetaV2`.
//!
//! sciex_spectrum_meta_v2(handle: i64, index: i64, out: *mut SciexSpectrumMetaV2) -> i32
//!     // the same, plus what Clearcore2 states about the precursor (0 = not stated).
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
//! sciex_spectrum_data_v2(handle, index, out_mz_ptr, out_int_ptr, out_len,
//!                        out_changes: *mut SciexValueChanges) -> i32
//!     // the same, plus what the glue changed in that spectrum's arrays (NaN intensities set to 0,
//!     // intensities clamped to ±f32::MAX, points cut from unequal arrays). This binary reads it;
//!     // `sciex_spectrum_data` stays for older ones.
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
//! ms_level / polarity (no blanket `MS:1000294` — the writer types rows from ms_level), with
//! `start_time` in minutes.

use std::ffi::OsStr;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result, anyhow, bail};

use netcorehost::hostfxr::AssemblyDelegateLoader;
use netcorehost::pdcstring::PdCString;
use netcorehost::{nethost, pdcstr};

use mzdata::params::Unit;
use mzdata::spectrum::bindata::{ArrayType, BinaryArrayMap, BinaryDataArrayType, DataArray};
use mzdata::spectrum::{
    MultiLayerSpectrum, ScanEvent, ScanPolarity, SignalContinuity, SpectrumDescription,
};

use crate::sciex_run::SciexRunInfo;

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

/// V2 metadata: the V1 layout verbatim as a prefix, plus what Clearcore2 states about the precursor,
/// read where ProteoWizard's `WiffFile.cpp` reads it; 0 means "not stated" throughout, and the
/// precursor is decided in [`crate::sciex_run::precursor`]. Filled by the SEPARATE `SpectrumMetaV2`
/// export: a wider struct behind the old name would overrun an older binary's buffer.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SciexSpectrumMetaV2 {
    // V1 prefix, byte-for-byte.
    sample: i32,
    experiment: i32,
    cycle: i32,
    ms_level: i32,
    polarity: i32,
    signal_continuity: i32,
    retention_time_seconds: f64,
    // V2 additions.
    /// `ExperimentDetails.ExperimentType` as its integer value (MS 0, Product 1, Precursor 2,
    /// NeutralGainOrLoss 3, SIM 4, MRM 5); -1 when unreadable.
    experiment_type: i32,
    /// `MassSpectrumInfo.ParentChargeState` of a product spectrum.
    precursor_charge: i32,
    /// `MassSpectrumInfo.ParentMZ` when `IsProductSpectrum`.
    parent_mz: f64,
    /// `FragmentBasedScanMassRange.IsolationWindow` (the experiment's first mass range), full width,
    /// on a Product / Precursor experiment.
    isolation_width: f64,
    /// `Details.Parameters["CE"]` Start and Stop, eV as stored (negative on a negative-polarity method).
    collision_energy_start: f64,
    collision_energy_stop: f64,
}

// Size AND prefix offsets (the Shimadzu pattern): a size check alone would not catch a reordered
// prefix. The C# static ctor asserts the same pairs.
const _: () = assert!(std::mem::size_of::<SciexSpectrumMetaV2>() == 72);
const _: () = assert!(std::mem::align_of::<SciexSpectrumMetaV2>() == 8);
const _: () = {
    use std::mem::offset_of;
    assert!(offset_of!(SciexSpectrumMetaV2, sample) == offset_of!(SciexSpectrumMeta, sample));
    assert!(offset_of!(SciexSpectrumMetaV2, experiment) == offset_of!(SciexSpectrumMeta, experiment));
    assert!(offset_of!(SciexSpectrumMetaV2, cycle) == offset_of!(SciexSpectrumMeta, cycle));
    assert!(offset_of!(SciexSpectrumMetaV2, ms_level) == offset_of!(SciexSpectrumMeta, ms_level));
    assert!(offset_of!(SciexSpectrumMetaV2, polarity) == offset_of!(SciexSpectrumMeta, polarity));
    assert!(offset_of!(SciexSpectrumMetaV2, signal_continuity) == offset_of!(SciexSpectrumMeta, signal_continuity));
    assert!(offset_of!(SciexSpectrumMetaV2, retention_time_seconds) == offset_of!(SciexSpectrumMeta, retention_time_seconds));
};

/// What the glue changed in one spectrum's arrays on their way out (`SpectrumDataV2`): intensity
/// points it mapped from NaN to 0, points it clamped to ±`f32::MAX` (±Inf included), and points it
/// dropped by cutting the longer of an unequal m/z / intensity pair. Summed per lane in
/// [`crate::sciex_run::GlueValueChanges`].
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SciexValueChanges {
    nan_to_zero: i64,
    clamped_to_f32: i64,
    truncated_points: i64,
}
const _: () = assert!(std::mem::size_of::<SciexValueChanges>() == 24);

/// ABI generation this binary requires from the glue DLL. 1 = the glue before the handshake (no
/// `SciexAbiVersion` export); 2 = + `SpectrumMetaV2` (precursor facts); 3 = + `SpectrumDataV2`
/// (value-change counts).
const REQUIRED_ABI_VERSION: i32 = 3;

// Function-pointer signatures for the glue's `[UnmanagedCallersOnly]` exports. The
// `extern "system"` calling convention matches what `UnmanagedCallersOnly` emits and what
// netcorehost's `get_function_with_unmanaged_callers_only` expects.
type SciexOpen = extern "system" fn(*const u16, *const u16) -> i64;
type SciexClose = extern "system" fn(i64);
type SciexSpectrumCount = extern "system" fn(i64) -> i64;
type SciexSpectrumMetaFn = extern "system" fn(i64, i64, *mut SciexSpectrumMeta) -> i32;
type SciexSpectrumMetaV2Fn = extern "system" fn(i64, i64, *mut SciexSpectrumMetaV2) -> i32;
type SciexAbiVersion = extern "system" fn() -> i32;
type SciexSpectrumData =
    extern "system" fn(i64, i64, *mut *const f64, *mut *const f32, *mut i64) -> i32;
type SciexSpectrumDataV2 =
    extern "system" fn(i64, i64, *mut *const f64, *mut *const f32, *mut i64, *mut SciexValueChanges) -> i32;
type SciexDataFree = extern "system" fn(i64, *const f64, *const f32);
/// `LastError(buf: *mut u16, cap: i32) -> i32` — fill-buffer diagnostics getter. Copies the
/// glue's stashed last-error message (UTF-16, NOT NUL-terminated unless room) into `buf` for up
/// to `cap` code units, and returns the FULL length in UTF-16 code units so the caller can detect
/// truncation. A null `buf` or `cap <= 0` just returns the needed length.
type SciexLastError = extern "system" fn(*mut u16, i32) -> i32;
type SciexRunInfoFn = extern "system" fn(i64, *mut i32) -> i32;
type SciexRunStringFn = extern "system" fn(i64, i32, *mut u16, i32) -> i32;

/// Resolved + bound function pointers into the loaded `SciexGlue` assembly.
#[derive(Clone)]
struct GlueApi {
    // Keep the runtime alive for as long as any function pointer is held.
    _runtime: Arc<AssemblyDelegateLoader>,
    open: SciexOpen,
    close: SciexClose,
    spectrum_count: SciexSpectrumCount,
    spectrum_meta_v2: SciexSpectrumMetaV2Fn,
    spectrum_data_v2: SciexSpectrumDataV2,
    data_free: SciexDataFree,
    last_error: SciexLastError,
    run_info: SciexRunInfoFn,
    run_string: SciexRunStringFn,
}

/// The CoreCLR runtime, booted ONCE per process — as `dotnetrawfilereader-sys` keeps its `BUNDLE` and
/// `src/shimadzu.rs` its `GLUE`.
///
/// A second `initialize_for_runtime_config` succeeds while a handle from the first is still alive,
/// but not after the last one has been dropped: netcorehost then frees hostfxr, the reloaded copy
/// takes the first-context path, and hostpolicy (still loaded) rejects it ("Initialization request is
/// expected to be non-null for requests other than the first one", 0x80008081). `-v` opens a reader
/// for the inspection report, drops it, and opens another for the conversion, so booting per open
/// failed every verbose native SciEX conversion and `--to mzml` export on Windows — the failure the
/// box showed for Shimadzu before it cached its glue (0446ea3). The glue locks its own state, so one
/// set of exports serves every reader.
static GLUE: OnceLock<Mutex<Option<GlueApi>>> = OnceLock::new();

impl GlueApi {
    /// Process-wide, loaded on first use; later calls hand back a clone of the same exports (the first
    /// caller's glue directory wins).
    fn shared(glue_dir: &Path) -> Result<Self> {
        let cell = GLUE.get_or_init(|| Mutex::new(None));
        let mut slot = cell
            .lock()
            .map_err(|_| anyhow!("SciEX glue lock poisoned by an earlier panic"))?;
        if let Some(api) = slot.as_ref() {
            return Ok(api.clone());
        }
        let api = Self::load(glue_dir)?;
        *slot = Some(api.clone());
        Ok(api)
    }

    /// Boot the CoreCLR runtime against `SciexGlue.runtimeconfig.json` in `glue_dir`, load
    /// `SciexGlue.dll`, and resolve every `[UnmanagedCallersOnly]` export. Only through
    /// [`GlueApi::shared`].
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
        // The V1 export is resolved by name (a glue that lost it fails here, and its struct twin stays
        // part of the pinned ABI — `tests/sciex_abi_pin.rs`) but never called: every metadata read
        // goes through `SpectrumMetaV2`.
        let _spectrum_meta: SciexSpectrumMetaFn = *loader
            .get_function_with_unmanaged_callers_only::<SciexSpectrumMetaFn>(
                ty,
                pdcstr!("SpectrumMeta"),
            )
            .map_err(|e| anyhow!("resolving glue export SpectrumMeta: {e}"))?;

        // ABI handshake (the `src/shimadzu.rs` pattern), before any versioned export is resolved.
        // Exports resolve by name and each side asserts only its OWN struct sizes, so nothing else
        // makes a mismatch visible: a stale DLL would fail below on a missing export without saying
        // why, and a layout change behind an unchanged name would not fail at all. Resolved
        // OPTIONALLY — absence means a pre-handshake glue, i.e. version 1 — so the error names the
        // real problem.
        let abi_version = loader
            .get_function_with_unmanaged_callers_only::<SciexAbiVersion>(ty, pdcstr!("SciexAbiVersion"))
            .map(|f| (*f)())
            .unwrap_or(1);
        if abi_version != REQUIRED_ABI_VERSION {
            bail!(
                "SciexGlue.dll in {} reports ABI version {abi_version}, this binary needs \
                 {REQUIRED_ABI_VERSION}. The DLL and the executable are one unit — rebuild the glue \
                 (`dotnet build -c Release` in glue/sciex) from the same commit as this binary.",
                glue_dir.display()
            );
        }
        let spectrum_meta_v2 = *loader
            .get_function_with_unmanaged_callers_only::<SciexSpectrumMetaV2Fn>(
                ty,
                pdcstr!("SpectrumMetaV2"),
            )
            .map_err(|e| anyhow!("resolving glue export SpectrumMetaV2: {e}"))?;
        // Like `SpectrumMeta`: the V1 export is resolved by name but never called.
        let _spectrum_data: SciexSpectrumData = *loader
            .get_function_with_unmanaged_callers_only::<SciexSpectrumData>(
                ty,
                pdcstr!("SpectrumData"),
            )
            .map_err(|e| anyhow!("resolving glue export SpectrumData: {e}"))?;
        let spectrum_data_v2 = *loader
            .get_function_with_unmanaged_callers_only::<SciexSpectrumDataV2>(
                ty,
                pdcstr!("SpectrumDataV2"),
            )
            .map_err(|e| anyhow!("resolving glue export SpectrumDataV2: {e}"))?;
        let data_free = *loader
            .get_function_with_unmanaged_callers_only::<SciexDataFree>(ty, pdcstr!("DataFree"))
            .map_err(|e| anyhow!("resolving glue export DataFree: {e}"))?;
        let last_error = *loader
            .get_function_with_unmanaged_callers_only::<SciexLastError>(ty, pdcstr!("LastError"))
            .map_err(|e| anyhow!("resolving glue export LastError: {e}"))?;
        // Required since the handshake: every glue that passes it has both.
        let run_info = *loader
            .get_function_with_unmanaged_callers_only::<SciexRunInfoFn>(ty, pdcstr!("RunInfo"))
            .map_err(|e| anyhow!("resolving glue export RunInfo: {e}"))?;
        let run_string = *loader
            .get_function_with_unmanaged_callers_only::<SciexRunStringFn>(ty, pdcstr!("RunString"))
            .map_err(|e| anyhow!("resolving glue export RunString: {e}"))?;

        Ok(Self {
            _runtime: loader,
            open,
            close,
            spectrum_count,
            spectrum_meta_v2,
            spectrum_data_v2,
            data_free,
            last_error,
            run_info,
            run_string,
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
/// ⚠️ Windows-only (see module docs).
pub struct SciexReader {
    api: GlueApi,
    handle: i64,
    count: usize,
    /// When one sample of a multi-sample WIFF is selected: the flattened glue indices that belong
    /// to it, in order. `None` = every index (a single-sample file).
    selected: Option<Vec<usize>>,
    /// The instrument can fragment only by collision, so a precursor states beam-type CID
    /// ([`crate::sciex_run::collision_only_instrument`]).
    collision_only_instrument: bool,
    /// The glue's value changes, summed over the spectra read since the last
    /// [`take_value_changes`](Self::take_value_changes).
    value_changes: std::cell::Cell<crate::sciex_run::GlueValueChanges>,
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
        let glue_dir = crate::pwiz_layout::glue_dir("MZPC_SCIEX_GLUE", "sciex")
            .ok_or_else(|| {
                anyhow!(
                    "MZPC_SCIEX_GLUE is not set and there is no glue/sciex beside the executable; \
                     point it at the directory holding SciexGlue.dll (the `dotnet build` output of \
                     glue/sciex, e.g. .../bin/Release/net8.0)"
                )
            })?;

        let pwiz_dir = resolve_clearcore2_dir()?;

        let api = GlueApi::shared(&glue_dir)?;

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

        let mut reader = Self {
            api,
            handle,
            count,
            selected: None,
            collision_only_instrument: false,
            value_changes: Default::default(),
            _not_thread_safe: PhantomData,
        };
        let instrument = reader.run_string(1);
        reader.collision_only_instrument = crate::sciex_run::collision_only_instrument(&instrument);
        if !reader.collision_only_instrument {
            log::info!(
                "SciEX: instrument {instrument:?} may fragment by EAD, which Clearcore2 does not report \
                 (or is not named); precursors state no dissociation method"
            );
        }
        Ok(reader)
    }

    pub fn len(&self) -> usize {
        self.selected.as_ref().map_or(self.count, Vec::len)
    }

    /// The glue's value changes over the spectra read since the last call; resets them to none.
    pub fn take_value_changes(&self) -> crate::sciex_run::GlueValueChanges {
        self.value_changes.take()
    }

    /// The glue's flattened index behind reader index `i`.
    fn raw_index(&self, i: usize) -> usize {
        self.selected.as_ref().map_or(i, |v| v[i])
    }

    /// Run-level counts, or `None` when the glue could not produce them.
    pub fn run_info(&self) -> Option<SciexRunInfo> {
        let f = self.api.run_info;
        let mut out = [0i32; 5];
        if f(self.handle, out.as_mut_ptr()) != 0 {
            log::warn!("SciEX glue RunInfo failed: {}", self.api.last_error().unwrap_or_default());
            return None;
        }
        Some(SciexRunInfo {
            samples: out[0],
            unreadable_samples: out[1],
            dwell_experiments: out[2],
            scan_experiments: out[3],
            total_experiments: out[4],
        })
    }

    /// One of the glue's run strings (see `RunString` in Glue.cs); empty when absent.
    fn run_string(&self, which: i32) -> String {
        let f = self.api.run_string;
        let full = f(self.handle, which, std::ptr::null_mut(), 0);
        if full <= 0 {
            return String::new();
        }
        let mut buf = vec![0u16; full as usize + 1];
        let n = f(self.handle, which, buf.as_mut_ptr(), buf.len() as i32);
        if n < 0 {
            return String::new();
        }
        String::from_utf16_lossy(&buf[..(n as usize).min(buf.len())])
    }

    /// [`open`](Self::open) for a CONVERSION: refuse what this lane cannot store faithfully
    /// ([`crate::sciex_run::refusal`]) before any spectrum is written, then restrict the reader to
    /// `sample`. Both `.wiff` lanes (mzPeak and `--to mzml`) open through here, so they refuse the
    /// same files.
    pub fn open_run(path: &Path, sample: Option<u32>) -> Result<Self> {
        let mut reader = Self::open(path)?;
        if let Some(info) = reader.run_info() {
            // The last error BEFORE the run strings: a string call that throws overwrites it.
            let last_error = reader.api.last_error().unwrap_or_default();
            let (types, names) = (reader.run_string(0), reader.run_string(5));
            if let Some(msg) = crate::sciex_run::refusal(path, &info, sample, &types, &names, &last_error) {
                bail!(msg);
            }
        }
        if let Some(n) = sample {
            reader.select_sample(n)?;
        }
        Ok(reader)
    }

    /// Restrict the reader to sample `n` (1-based). Walks every flattened entry's metadata once.
    fn select_sample(&mut self, n: u32) -> Result<()> {
        let mut keep = Vec::new();
        for i in 0..self.count {
            let m = self.meta_raw(i)?;
            if m.sample == n as i32 {
                keep.push(i);
            }
        }
        if keep.is_empty() {
            bail!("--sample {n}: no spectra belong to that sample");
        }
        log::info!("SciEX: converting sample {n} only ({} of {} spectra)", keep.len(), self.count);
        self.selected = Some(keep);
        Ok(())
    }

    /// What the file states about the run, for `run_metadata::apply`: instrument, serial,
    /// software version, acquisition time (a DateTime whose Kind decides whether it is an instant
    /// or a wall clock), the selected sample's name. The source members (`.wiff` + `.wiff.scan`)
    /// are digested by the caller.
    pub fn run_metadata(&self, sample: Option<u32>) -> Option<crate::run_metadata::VendorRunMetadata> {
        use crate::run_metadata::{term, term_str, AcquisitionTime, VendorRunMetadata};
        use mzdata::meta::{InstrumentConfiguration, Sample, Software};
        let mut out = VendorRunMetadata::default();
        let instrument = self.run_string(1);
        let serial = self.run_string(2);
        if !instrument.is_empty() || !serial.is_empty() {
            let mut cfg = InstrumentConfiguration { id: 0, ..Default::default() };
            cfg.params.push(term(1000121, "SCIEX instrument model"));
            if !instrument.is_empty() {
                cfg.params.push(term_str(1000031, "instrument model", &instrument));
            }
            if !serial.is_empty() {
                cfg.params.push(term_str(1000529, "instrument serial number", &serial));
            }
            out.instrument = Some(cfg);
        }
        let version = self.run_string(3);
        out.acquisition_software = Some(Software::new(
            "Analyst".to_string(),
            if version.is_empty() { "unknown".to_string() } else { version },
            vec![term(1000551, "Analyst")],
        ));
        let time = self.run_string(4);
        if let Some((iso, kind)) = time.split_once('|') {
            // Kind Utc / Local carry an offset in the "o" form; Unspecified renders without one.
            match crate::run_metadata::parse_vendor_time(iso, "SciEX AcquisitionDateTime") {
                Ok(t) => {
                    out.start_time = Some(match (t, kind) {
                        (AcquisitionTime::Stated(dt), "Utc" | "Local") => AcquisitionTime::Stated(dt),
                        (AcquisitionTime::Stated(dt), _) => AcquisitionTime::Naive {
                            wall_clock: dt.naive_local(),
                            source: "SciEX AcquisitionDateTime",
                        },
                        (naive, _) => naive,
                    });
                }
                Err(e) => log::warn!("{e}"),
            }
        }
        let names: Vec<String> = self.run_string(5).split('\u{1F}').map(str::to_string).collect();
        let idx = sample.map_or(0, |n| n.saturating_sub(1) as usize);
        if let Some(name) = names.get(idx).filter(|n| !n.is_empty()) {
            out.samples.push(Sample::new(format!("sample_{}", idx + 1), Some(name.clone()), vec![]));
        }
        let resolved = self.run_string(6);
        if !resolved.is_empty() {
            log::info!("SciEX run metadata resolved from: {resolved}");
        }
        Some(out)
    }

    /// Fetch one spectrum's scalar metadata via the glue.
    fn meta(&self, i: usize) -> Result<SciexSpectrumMetaV2> {
        self.meta_raw(self.raw_index(i))
    }

    fn meta_raw(&self, i: usize) -> Result<SciexSpectrumMetaV2> {
        let index = i64::try_from(i).map_err(|_| anyhow!("SciEX index {i} does not fit in i64"))?;
        let mut meta = SciexSpectrumMetaV2::default();
        // SAFETY: `meta` is a valid, writable, correctly-laid-out destination for the glue.
        let rc = (self.api.spectrum_meta_v2)(self.handle, index, &mut meta as *mut _);
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
        let mut changes = SciexValueChanges::default();

        // SAFETY: all four out-params are valid writable locals. On success the glue writes two
        // pinned array pointers, a shared length and its value-change counts; we own the
        // obligation to call `data_free` afterwards (done unconditionally below).
        let rc = (self.api.spectrum_data_v2)(
            self.handle,
            index,
            &mut mz_ptr as *mut _,
            &mut int_ptr as *mut _,
            &mut len as *mut _,
            &mut changes as *mut _,
        );
        if rc != 0 {
            bail!(
                "SciEX glue SpectrumDataV2 failed for index {i} (rc {rc}): {}",
                self.api.last_error().unwrap_or_default()
            );
        }
        let mut tally = self.value_changes.get();
        tally.add(changes.nan_to_zero, changes.clamped_to_f32, changes.truncated_points);
        self.value_changes.set(tally);

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
        if i >= self.len() {
            bail!("SciEX spectrum index {i} out of range (len {})", self.len());
        }
        let meta = self.meta(i)?;
        let (mz, intensity) = self.peaks(self.raw_index(i))?;

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
        // No blanket `MS:1000294 "mass spectrum"` here (0.9.13). mzdata's `spectrum_type()` is a first-match
        // lookup, so that parent term wins over the specific one and the writer's inference
        // (`writer/visitor.rs`: ms_level 1 -> MS:1000579, else MS:1000580) never runs; with it absent the
        // writer types each row from `ms_level`, as the mzML, Shimadzu and Bruker-native lanes already do.
        let mut scan = ScanEvent::default();
        // RT UNIT CONTRACT: the ABI field is seconds (C# multiplies Clearcore2 minutes by 60);
        // mzdata's scan start_time is minutes, so divide by 60 here. See the field doc on
        // `SciexSpectrumMeta` and the matching note in `glue/sciex/Glue.cs`.
        scan.start_time = meta.retention_time_seconds / 60.0;
        descr.acquisition.scans.push(scan);

        // The precursor Clearcore2 states, decided host-independently in `sciex_run::precursor`.
        descr.precursor.extend(crate::sciex_run::precursor(&crate::sciex_run::PrecursorFacts {
            experiment_type: meta.experiment_type,
            parent_mz: meta.parent_mz,
            charge: meta.precursor_charge,
            isolation_width: meta.isolation_width,
            collision_energy: (meta.collision_energy_start, meta.collision_energy_stop),
            collision_only_instrument: self.collision_only_instrument,
        }));
        // An MSn row the file states no precursor for is written as an orphan — no selected ion,
        // isolation window or activation — and the archive is otherwise indistinguishable from a
        // complete one: say so once, loudly.
        if ms_level > 1 && descr.precursor.is_empty() {
            static PRECURSOR_GAP_SAID: std::sync::Once = std::sync::Once::new();
            PRECURSOR_GAP_SAID.call_once(|| {
                log::warn!(
                    "SciEX native (Clearcore2): {} is MS{ms_level} but states no precursor (no parent \
                     m/z, or a precursor-ion scan whose fixed mass is a product); such rows are written \
                     without a precursor",
                    descr.id
                );
            });
        }

        Ok(MultiLayerSpectrum::new(descr, Some(arrays), None, None))
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
