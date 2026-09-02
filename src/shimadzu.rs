//! Native Shimadzu LabSolutions `.lcd` reader → mzdata spectra (native lane, no msconvert).
//!
//! ⚠️ WINDOWS-RUNTIME-ONLY. It only *runs* where `Shimadzu.LabSolutions.IO.IoModule.dll` (from a
//! ProteoWizard install, flat in pwiz-bin) and a .NET 8 runtime exist. There is no macOS/Linux build
//! of the Shimadzu stack. The DLL also carries a restrictive Shimadzu EULA — see
//! `glue/shimadzu/README.md`.
//!
//! **Verified on real data (2026-08-20)**, LCMS-9030 QTOF `HEK_PosOAD1.lcd`: 2,101 spectra, MS1+MS2,
//! m/z 70–1250, RT 0–16.99 min. Against a msconvert conversion of the same file the intensities are
//! bit-identical and m/z agrees to < 2 ppm; msconvert additionally pads each profile spectrum with
//! two zero-intensity points at the scan-window bounds, which this lane does not emit.
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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result, anyhow, bail};

use netcorehost::hostfxr::AssemblyDelegateLoader;
use netcorehost::pdcstring::PdCString;
use netcorehost::{nethost, pdcstr};

use mzpeaks::{CentroidPeak, PeakSet};
use mzdata::params::Unit;
use mzdata::spectrum::bindata::{ArrayType, BinaryArrayMap, BinaryDataArrayType, DataArray};
use mzdata::meta::DissociationMethodTerm;
use mzdata::spectrum::{
    Activation, IsolationWindow, IsolationWindowState, MultiLayerSpectrum, Precursor, ScanEvent,
    ScanPolarity, SelectedIon, SignalContinuity, SpectrumDescription,
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

/// V2 metadata: the V1 layout verbatim as a prefix, plus the precursor/acquisition scalars the
/// native lane was missing against the mzML lane. Filled by the SEPARATE `SpectrumMetaV2` export —
/// see [`GlueApi::load`] for why widening the struct behind the old name would be unsafe.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ShimadzuSpectrumMetaV2 {
    // V1 prefix, byte-for-byte.
    scan_number: i64,
    ms_level: i32,
    polarity: i32,
    signal_continuity: i32,
    precursor_charge: i32,
    retention_time_seconds: f64,
    precursor_mz: f64,
    n_points: i64,
    // V2 additions.
    isolation_target_mz: f64,
    isolation_width_mz: f64,
    collision_energy: f64,
    precursor_scan_number: i64,
    segment_no: i32,
    event_no: i32,
}

// Size AND prefix offsets: a total-size check alone would not catch a reordered prefix, and the two
// entry points must agree about where the shared fields live. The C# side asserts the same pairs.
const _: () = assert!(std::mem::size_of::<ShimadzuSpectrumMetaV2>() == 88);
const _: () = assert!(std::mem::align_of::<ShimadzuSpectrumMetaV2>() == 8);
const _: () = {
    use std::mem::offset_of;
    assert!(offset_of!(ShimadzuSpectrumMetaV2, scan_number) == offset_of!(ShimadzuSpectrumMeta, scan_number));
    assert!(offset_of!(ShimadzuSpectrumMetaV2, ms_level) == offset_of!(ShimadzuSpectrumMeta, ms_level));
    assert!(offset_of!(ShimadzuSpectrumMetaV2, polarity) == offset_of!(ShimadzuSpectrumMeta, polarity));
    assert!(offset_of!(ShimadzuSpectrumMetaV2, signal_continuity) == offset_of!(ShimadzuSpectrumMeta, signal_continuity));
    assert!(offset_of!(ShimadzuSpectrumMetaV2, precursor_charge) == offset_of!(ShimadzuSpectrumMeta, precursor_charge));
    assert!(offset_of!(ShimadzuSpectrumMetaV2, retention_time_seconds) == offset_of!(ShimadzuSpectrumMeta, retention_time_seconds));
    assert!(offset_of!(ShimadzuSpectrumMetaV2, precursor_mz) == offset_of!(ShimadzuSpectrumMeta, precursor_mz));
    assert!(offset_of!(ShimadzuSpectrumMetaV2, n_points) == offset_of!(ShimadzuSpectrumMeta, n_points));
};

/// ABI generation this binary requires from the glue DLL. 3 = V2 metadata + MassRange + InstrumentInfo.
const REQUIRED_ABI_VERSION: i32 = 3;

type ShimOpen = extern "system" fn(*const u16, *const u16) -> i64;
type ShimClose = extern "system" fn(i64);
type ShimSpectrumCount = extern "system" fn(i64) -> i64;
type ShimSpectrumMetaFn = extern "system" fn(i64, i64, *mut ShimadzuSpectrumMeta) -> i32;
type ShimSpectrumMetaV2Fn = extern "system" fn(i64, i64, *mut ShimadzuSpectrumMetaV2) -> i32;
type ShimAbiVersion = extern "system" fn() -> i32;
type ShimMassRange = extern "system" fn(i64, i32, i32, *mut f64, *mut f64) -> i32;
type ShimInstrumentInfo = extern "system" fn(i64, *mut u16, i32) -> i32;
/// `which`: 0 = profile, 1 = centroid. Both exist for the same scan; each is fetched separately so
/// an archive can carry both facets (see `Representation`).
type ShimSpectrumData =
    extern "system" fn(i64, i64, i32, *mut *const f64, *mut *const f32, *mut i64) -> i32;
type ShimDataFree = extern "system" fn(i64, *const f64, *const f32);
type ShimLastError = extern "system" fn(*mut u16, i32) -> i32;

#[derive(Clone)]
struct GlueApi {
    _runtime: Arc<AssemblyDelegateLoader>,
    open: ShimOpen,
    close: ShimClose,
    spectrum_count: ShimSpectrumCount,
    spectrum_meta: ShimSpectrumMetaFn,
    spectrum_meta_v2: ShimSpectrumMetaV2Fn,
    mass_range: ShimMassRange,
    instrument_info: ShimInstrumentInfo,
    spectrum_data: ShimSpectrumData,
    data_free: ShimDataFree,
    last_error: ShimLastError,
}

/// The CoreCLR runtime, booted ONCE per process.
///
/// `hostfxr` refuses a second `initialize_for_runtime_config` in the same process
/// ("Initialization request is expected to be non-null for requests other than the first one",
/// 0x80008081). `-v` opens the reader once for the inspection report and again for the conversion,
/// so a per-open init made the two paths mutually exclusive: verbose conversion always failed.
/// The delegate loader is cheap to share and the glue is internally locked, so cache it.
static GLUE: OnceLock<Mutex<Option<GlueApi>>> = OnceLock::new();

impl GlueApi {
    /// Process-wide, initialized on first use. Later calls hand back a clone of the same exports.
    fn shared(glue_dir: &Path) -> Result<Self> {
        let cell = GLUE.get_or_init(|| Mutex::new(None));
        let mut slot = cell
            .lock()
            .map_err(|_| anyhow!("Shimadzu glue lock poisoned by an earlier panic"))?;
        if let Some(api) = slot.as_ref() {
            return Ok(api.clone());
        }
        let api = Self::load(glue_dir)?;
        *slot = Some(api.clone());
        Ok(api)
    }

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

        // ABI handshake. Nothing else makes a version mismatch detectable: each side asserts only
        // its OWN struct size and exports resolve by name, so a stale DLL beside a new binary would
        // have written 48 bytes into an 88-byte buffer (silent garbage metadata) and a stale binary
        // beside a new DLL would have taken a 40-byte out-param overrun. Resolved OPTIONALLY —
        // absence means a pre-handshake build, i.e. version 1 — so the error names the real problem
        // instead of surfacing as a missing-export failure.
        let abi_version = loader
            .get_function_with_unmanaged_callers_only::<ShimAbiVersion>(ty, pdcstr!("ShimadzuAbiVersion"))
            .map(|f| (*f)())
            .unwrap_or(1);
        if abi_version != REQUIRED_ABI_VERSION {
            bail!(
                "ShimadzuGlue.dll in {} reports ABI version {abi_version}, this binary needs \
                 {REQUIRED_ABI_VERSION}. The DLL and the executable are one unit — rebuild the glue \
                 (`dotnet build -c Release` in glue/shimadzu) from the same commit as this binary.",
                glue_dir.display()
            );
        }
        let spectrum_meta_v2 = *loader
            .get_function_with_unmanaged_callers_only::<ShimSpectrumMetaV2Fn>(ty, pdcstr!("SpectrumMetaV2"))
            .map_err(|e| anyhow!("resolving glue export SpectrumMetaV2: {e}"))?;
        let mass_range = *loader
            .get_function_with_unmanaged_callers_only::<ShimMassRange>(ty, pdcstr!("MassRange"))
            .map_err(|e| anyhow!("resolving glue export MassRange: {e}"))?;
        let instrument_info = *loader
            .get_function_with_unmanaged_callers_only::<ShimInstrumentInfo>(ty, pdcstr!("InstrumentInfo"))
            .map_err(|e| anyhow!("resolving glue export InstrumentInfo: {e}"))?;
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
            spectrum_meta_v2,
            mass_range,
            instrument_info,
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
/// Which representation(s) to read from a `.lcd`.
///
/// Shimadzu exposes `ProfileList` and `CentroidList` for the SAME scan, and mzPeak carries both by
/// design (`spectra_data` + `spectra_peaks`). `Both` is the faithful default; the others force one
/// view when that is what a caller wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Representation {
    #[default]
    Both,
    Profile,
    Centroid,
}

/// Instrument identity as the vendor API states it (`IO.SystemName()`, `Parameters.DeviceID`,
/// `SampleInfo.AnalysisDate`, spectrum `IfKind`). Every field optional: absent is absent.
#[derive(Debug, Default, Clone)]
pub struct ShimadzuInstrumentInfo {
    pub system_name: Option<String>,
    /// e.g. `MSID_QTFL` — the LCMS-9030 Q-TOF.
    pub device_id: Option<String>,
    /// ISO 8601, local instrument time, no zone.
    pub analysis_date: Option<String>,
    /// e.g. `ESI`.
    pub ionization: Option<String>,
}

pub struct ShimadzuReader {
    api: GlueApi,
    handle: i64,
    count: usize,
    lcd_path: PathBuf,
    representation: Representation,
    /// One-shot latch so the "you asked for X, the file has Y" warning is emitted once per file
    /// rather than once per spectrum.
    fallback_warned: AtomicBool,
    /// Memoised answer to "does this `.lcd` store profile signal at all?", which decides whether
    /// the vendor-defect warning applies. `None` until the first centroid fetch probes for it.
    stores_profile: std::cell::Cell<Option<bool>>,
    /// One-shot latch for that warning.
    rotation_warned: AtomicBool,
    _not_thread_safe: PhantomData<*const ()>,
}

impl ShimadzuReader {
    /// Open with the faithful default: read whichever representations the file actually has.
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with(path, Representation::default())
    }

    pub fn open_with(path: &Path, representation: Representation) -> Result<Self> {
        let glue_dir = std::env::var_os("MZPC_SHIMADZU_GLUE")
            .map(PathBuf::from)
            .ok_or_else(|| {
                anyhow!(
                    "MZPC_SHIMADZU_GLUE is not set; point it at the directory holding ShimadzuGlue.dll \
                     (the `dotnet build` output of glue/shimadzu, e.g. .../bin/Release/net8.0)"
                )
            })?;
        let pwiz_dir = resolve_shimadzu_dll_dir()?;
        let api = GlueApi::shared(&glue_dir)?;

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
            representation,
            fallback_warned: AtomicBool::new(false),
            stores_profile: std::cell::Cell::new(None),
            rotation_warned: AtomicBool::new(false),
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

    fn meta(&self, i: usize) -> Result<ShimadzuSpectrumMetaV2> {
        let index = i64::try_from(i).map_err(|_| anyhow!("Shimadzu index {i} does not fit in i64"))?;
        let mut meta = ShimadzuSpectrumMetaV2::default();
        let rc = (self.api.spectrum_meta_v2)(self.handle, index, &mut meta as *mut _);
        if rc != 0 {
            bail!(
                "Shimadzu glue SpectrumMeta failed for index {i} (rc {rc}): {}",
                self.api.last_error().unwrap_or_default()
            );
        }
        Ok(meta)
    }

    /// Scan window of one (segment, event) in m/z, or `None` when the vendor reports no range.
    /// The glue memoises per pair, so this is one vendor call per event, not per scan.
    fn mass_range(&self, segment_no: i32, event_no: i32) -> Option<(f64, f64)> {
        let (mut lo, mut hi) = (0.0f64, 0.0f64);
        let rc = (self.api.mass_range)(self.handle, segment_no, event_no, &mut lo, &mut hi);
        if rc != 0 || !(hi > lo) || lo < 0.0 {
            return None;
        }
        Some((lo, hi))
    }

    /// What the vendor states about the instrument and the run — nothing more.
    pub fn instrument_info(&self) -> ShimadzuInstrumentInfo {
        let needed = (self.api.instrument_info)(self.handle, std::ptr::null_mut(), 0);
        if needed <= 0 {
            return ShimadzuInstrumentInfo::default();
        }
        let mut buf = vec![0u16; needed as usize];
        let written = (self.api.instrument_info)(self.handle, buf.as_mut_ptr(), needed);
        let n = (written.max(0) as usize).min(buf.len());
        let text = String::from_utf16_lossy(&buf[..n]);
        let mut fields = text.split('\u{1F}').map(|f| f.trim().to_string());
        let mut next = || fields.next().filter(|f| !f.is_empty());
        ShimadzuInstrumentInfo {
            system_name: next(),
            device_id: next(),
            analysis_date: next(),
            ionization: next(),
        }
    }

    /// Does this `.lcd` store profile signal at all? Probes the head of the run plus a stride
    /// across it, and stops at the first spectrum that yields profile points.
    ///
    /// This is the discriminator for the A5 gate below: measured against the LabSolutions mzML
    /// exports, the native centroid lists come back correct on files that carry profile signal
    /// (Blind_P1_pos_012: 13,200/13,200 spectra, all 216,742 intensities bit-exact) and rotated —
    /// `[s alien values] + truth[0:n-s]`, plus a clipped final peak — on files that carry none
    /// (the DIA_Hela pair).
    fn stores_profile(&self) -> Result<bool> {
        if let Some(known) = self.stores_profile.get() {
            return Ok(known);
        }
        let head = self.count.min(16);
        let stride = (self.count / 8).max(1);
        let probes = (0..head).chain((0..self.count).step_by(stride));
        let mut found = false;
        for i in probes {
            if !self.peaks(i, 0)?.0.is_empty() {
                found = true;
                break;
            }
        }
        self.stores_profile.set(Some(found));
        Ok(found)
    }

    /// Warn — once per file — when this `.lcd` stores no profile signal, because the vendor API
    /// returns misaligned centroid intensities for exactly those spectra (see
    /// [`Self::stores_profile`]).
    ///
    /// This converter's job is to STORE what the vendor interface returns, not to correct or
    /// second-guess it, so this does not refuse the conversion and does not alter a single value.
    /// It says plainly what the data is, and leaves the science to the reader — msconvert stores
    /// the same bytes silently.
    fn warn_if_rotated_centroids(&self) {
        if self.rotation_warned.load(Ordering::Relaxed) {
            return;
        }
        match self.stores_profile() {
            Ok(false) => {
                if !self.rotation_warned.swap(true, Ordering::Relaxed) {
                    log::warn!(
                        "{} stores no profile signal. For such spectra Shimadzu.LabSolutions.IO \
                         returns centroid intensities misaligned against their m/z (shifted by 1-7 \
                         positions, the last peak missing) — a VENDOR-side defect that msconvert \
                         reproduces byte-identically. These peaks are stored exactly as the vendor \
                         API returned them, unaltered. For scientifically correct centroids from \
                         this file use a LabSolutions mzML export, whose exporter takes a different \
                         internal path and is exact.",
                        self.lcd_path.display()
                    );
                }
            }
            Ok(true) => {
                self.rotation_warned.store(true, Ordering::Relaxed);
            }
            Err(e) => log::debug!("could not probe {} for profile signal: {e}", self.lcd_path.display()),
        }
    }

    /// Fetch one representation of spectrum `i`. `which`: 0 = profile, 1 = centroid.
    fn peaks(&self, i: usize, which: i32) -> Result<(Vec<f64>, Vec<f32>)> {
        if which == 1 {
            self.warn_if_rotated_centroids();
        }
        let index = i64::try_from(i).map_err(|_| anyhow!("Shimadzu index {i} does not fit in i64"))?;
        let mut mz_ptr: *const f64 = std::ptr::null();
        let mut int_ptr: *const f32 = std::ptr::null();
        let mut len: i64 = 0;

        let rc = (self.api.spectrum_data)(
            self.handle,
            index,
            which,
            &mut mz_ptr as *mut _,
            &mut int_ptr as *mut _,
            &mut len as *mut _,
        );
        // RAII guard: DataFree must release the managed pins even on a panic/early bail.
        // Armed BEFORE the rc check: a partially-successful call can have pinned one array and then
        // failed, and bailing straight out of here would strand that pin for the process lifetime.
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

        if rc != 0 {
            bail!(
                "Shimadzu glue SpectrumData failed for index {i} (rc {rc}): {}",
                self.api.last_error().unwrap_or_default()
            );
        }
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
        // Shimadzu exposes profile AND centroid for the same scan. Fetch what was asked for and
        // report which one actually carried data, so the spectrum can be LABELLED correctly --
        // previously the continuity flag was dropped at the ABI boundary and every spectrum was
        // written as "profile" whatever it held.
        let want_profile = self.representation != Representation::Centroid;
        let want_centroid = self.representation != Representation::Profile;
        let mut profile = if want_profile { self.peaks(i, 0)? } else { (Vec::new(), Vec::new()) };
        let mut centroid = if want_centroid { self.peaks(i, 1)? } else { (Vec::new(), Vec::new()) };
        // An explicit `--representation profile` on a file that stores only centroids would otherwise
        // leave both arrays empty and then LABEL that emptiness -- writing a zero-point spectrum
        // tagged as the representation that is absent. Fall back to the representation the file does
        // have, keep its true label, and say so once. NOTE: on a profile-less file that fallback now
        // hits the A5 gate and hard-errors instead of quietly emitting rotated centroids — closing
        // the hole that made `--representation profile` look like a safe lane for those files.
        if profile.0.is_empty() && centroid.0.is_empty() {
            match self.representation {
                Representation::Profile => centroid = self.peaks(i, 1)?,
                Representation::Centroid => profile = self.peaks(i, 0)?,
                Representation::Both => {}
            }
            if (!profile.0.is_empty() || !centroid.0.is_empty())
                && !self.fallback_warned.swap(true, Ordering::Relaxed)
            {
                // Neutral wording on purpose: the mzML export path maps the `both` DEFAULT onto
                // `Profile` to collapse to one representation, so naming the requested variant here
                // would report a choice the user never made.
                log::warn!(
                    "this file does not store the requested representation ({:?}); writing the one \
                     it does contain, correctly labelled",
                    self.representation
                );
            }
        }

        // Which facets does this spectrum actually carry? When BOTH are present the raw profile goes
        // into the data facet and the centroid list travels alongside it as a peak list -- the writer
        // emits `spectra_data` + `spectra_peaks` and fills both `number_of_data_points` and
        // `number_of_peaks` (base.rs `write_spectrum_data`, the "Writing both profile signal and
        // peaks" branch). With only one present, that one is written and labelled for what it is.
        let has_profile = !profile.0.is_empty();
        let has_centroid = !centroid.0.is_empty();
        // The centroid list rides along as a peak list only when a profile occupies the data facet;
        // otherwise it IS the data facet (moved into `mz`/`intensity` below) and the writer derives
        // the peaks itself. Built before the move so it can consume `centroid` in place.
        let peak_set = if has_profile && has_centroid {
            Some(PeakSet::new(
                centroid
                    .0
                    .iter()
                    .zip(centroid.1.iter())
                    .enumerate()
                    .map(|(k, (m, it))| CentroidPeak::new(*m, *it, k as u32))
                    .collect(),
            ))
        } else {
            None
        };
        let (mz, intensity, is_profile) = if has_profile {
            (profile.0, profile.1, true)
        } else {
            (centroid.0, centroid.1, false)
        };

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
        // From the DATA, not from `meta.signal_continuity` -- the glue hardcodes that field to 0
        // (profile), so trusting it labelled centroid data as profile on every Shimadzu spectrum,
        // routing it into the profile facet and populating number_of_data_points instead of
        // number_of_peaks.
        let signal_continuity = if is_profile {
            SignalContinuity::Profile
        } else {
            SignalContinuity::Centroid
        };

        let mut descr = SpectrumDescription {
            id: format!("scan={}", meta.scan_number),
            index: i,
            ms_level,
            signal_continuity,
            polarity,
            ..Default::default()
        };
        // No blanket `MS:1000294 "mass spectrum"` here. mzdata's `spectrum_type()` is a first-match
        // lookup, so that parent term wins over the specific one and the writer's inference
        // (`writer/visitor.rs`: ms_level 1 -> MS:1000579, else MS:1000580) never runs — every
        // Shimadzu spectrum was typed "mass spectrum" while the mzML lane recorded MS1/MSn.
        let mut scan = ScanEvent::default();
        // ABI carries seconds; mzdata scan start_time is minutes.
        scan.start_time = meta.retention_time_seconds / 60.0;
        // Scan window = the acquisition event's configured m/z range (`GetMassRawRange`), which the
        // mzML lanes record as MS:1000501/MS:1000500. One range per (segment, event), memoised.
        if let Some((lo, hi)) = self.mass_range(meta.segment_no, meta.event_no) {
            scan.scan_windows.push(mzdata::spectrum::ScanWindow {
                lower_bound: lo as f32,
                upper_bound: hi as f32,
            });
        }
        descr.acquisition.scans.push(scan);

        // Precursor, mirroring `bruker_native::build_precursors`. The vendor reports the isolation
        // window as a centre (`AcqModeMz`) plus a FULL width (`QTransmissionWidthMz`), so the bounds
        // are half the width either side — that reproduces the mzML lane's ±8.5 from a 17.0 Th
        // window. `precursor_id` names the parent scan so the writer's id→index map can resolve it.
        if ms_level > 1 && meta.precursor_mz > 0.0 {
            let half = (meta.isolation_width_mz / 2.0) as f32;
            let target = meta.isolation_target_mz.max(meta.precursor_mz) as f32;
            let ion = SelectedIon {
                mz: meta.precursor_mz,
                charge: (meta.precursor_charge != 0).then_some(meta.precursor_charge),
                ..Default::default()
            };
            let mut activation = Activation::default();
            activation.energy = meta.collision_energy as f32;
            activation
                .methods_mut()
                .push(DissociationMethodTerm::CollisionInducedDissociation);
            descr.precursor = vec![Precursor {
                ions: vec![ion],
                isolation_window: if half > 0.0 {
                    IsolationWindow {
                        target,
                        lower_bound: target - half,
                        upper_bound: target + half,
                        flags: IsolationWindowState::Complete,
                    }
                } else {
                    IsolationWindow { target, ..Default::default() }
                },
                activation,
                precursor_id: (meta.precursor_scan_number > 0)
                    .then(|| format!("scan={}", meta.precursor_scan_number)),
                ..Default::default()
            }];
        }

        Ok(MultiLayerSpectrum::new(descr, Some(arrays), peak_set, None))
    }

    /// A sample spectrum's array map, for deriving the writer's data-facet schema.
    pub fn sample_arrays(&self) -> Result<BinaryArrayMap> {
        // Surface the vendor-defect warning up front rather than mid-run.
        self.warn_if_rotated_centroids();
        // Look through BOTH representations: on a centroid-only file every `which = 0` fetch comes
        // back empty, and settling for spectrum 0 would hand the writer an empty schema sample.
        let mut chosen = 0usize;
        'outer: for i in 0..self.count {
            for which in [0, 1] {
                if let Ok((mz, _)) = self.peaks(i, which) {
                    if !mz.is_empty() {
                        chosen = i;
                        break 'outer;
                    }
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
