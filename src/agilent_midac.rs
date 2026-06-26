//! Native Agilent **IM-MS** (`.d`) reader via the MIDAC SDK → mzdata spectra (PLAN §3.7 / IM-MS).
//!
//! **Windows-runtime-only and UNTESTED SCAFFOLD.** Compiles on Windows (gated by `cfg(windows)` in
//! `main.rs`); it can only *run* on Windows with the Agilent **MIDAC** (Mass Informatics Data Access
//! Component for ion-mobility) DLLs present. Like the MHDAC reader in [`crate::agilent`], we never
//! reference MIDAC at compile time: an in-process .NET 8 runtime (`netcorehost`) calls a small C#
//! glue (`AgilentMidacGlue.dll`, built from `glue/agilent_midac/`) that reaches MIDAC purely through
//! `System.Reflection`. The whole stack builds without the DLLs; the DLLs are only needed at run time.
//!
//! Difference from MHDAC: MIDAC exposes the **drift (ion-mobility) dimension**. Each Agilent IM frame
//! becomes one mzPeak spectrum carrying m/z + intensity + a per-point
//! `mean_inverse_reduced_ion_mobility` array — the same IM representation the TDF path uses.
//!
//! ## Environment contract (resolved at `open`)
//!   * `MZPC_AGILENT_MIDAC_GLUE` — dir with `AgilentMidacGlue.dll` + its runtimeconfig (the
//!     `dotnet build` output of `glue/agilent_midac/`).
//!   * `MZPC_PWIZ_DIR` — a ProteoWizard install; the MIDAC DLLs live under
//!     `<MZPC_PWIZ_DIR>/vendor_api/Agilent` alongside MHDAC.
//!
//! ## C-ABI contract (Rust ⇄ glue, all `[UnmanagedCallersOnly]` on the C# side)
//! Paths are UTF-16 (`*const u16`) NUL-terminated.
//!   * `Open(path_utf16, midac_dir_utf16) -> i64` — open the IM `.d`; non-negative handle or negative error.
//!   * `FrameCount(handle) -> i64` — number of IM frames; negative on error.
//!   * `GetFrame(handle, index, out: *mut FrameOut) -> i32` — pointer+len+free; 0 on success.
//!   * `FreeFrame(out: *mut FrameOut)` — free the unmanaged arrays of a filled `FrameOut`.
//!   * `Close(handle)` — drop the reader (idempotent).
//!   * `LastError(buf, cap) -> i32` — per-thread diagnostic message; returns full UTF-16 length.
//!   * `HasImsData(path_utf16, midac_dir_utf16) -> i32` — 1 if the `.d` has IM data, 0 if not, <0 error.

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

/// Mirror of the C# `FrameOut` struct (`[StructLayout(LayoutKind.Sequential)]`). One IM frame:
/// three parallel arrays of `n_points` each — m/z (f64), intensity (f64, widened on the wire), and
/// mobility (f64, the per-point inverse reduced ion mobility) — allocated by the glue with
/// `Marshal.AllocHGlobal` and valid until `FreeFrame`.
#[repr(C)]
#[derive(Clone, Copy)]
struct FrameOut {
    mz_ptr: *const f64,
    intensity_ptr: *const f64,
    mobility_ptr: *const f64,
    n_points: i64,
    /// Frame retention time in **minutes**.
    rt_minutes: f64,
    /// 1 = MS1, 2 = MS2.
    ms_level: i32,
    /// 1 = positive, -1 = negative, 0 = unknown.
    polarity: i32,
    /// Vendor frame id, used to build the spectrum id.
    frame_id: i32,
    /// Explicit trailing pad so the 64-bit layout is an unambiguous 56 bytes on both sides.
    _pad: i32,
}

impl FrameOut {
    const fn zeroed() -> Self {
        FrameOut {
            mz_ptr: std::ptr::null(),
            intensity_ptr: std::ptr::null(),
            mobility_ptr: std::ptr::null(),
            n_points: 0,
            rt_minutes: 0.0,
            ms_level: 0,
            polarity: 0,
            frame_id: 0,
            _pad: 0,
        }
    }
}

// ABI drift guard: 3×ptr(8) + i64(8) + f64(8) + 4×i32(4) = 56 bytes, 8-byte aligned. The C# side
// asserts Marshal.SizeOf == 56 at module init.
const _: () = {
    assert!(std::mem::size_of::<FrameOut>() == 56);
    assert!(std::mem::align_of::<FrameOut>() == 8);
};

type OpenFn = extern "system" fn(*const u16, *const u16) -> i64;
type CountFn = extern "system" fn(i64) -> i64;
type GetFrameFn = extern "system" fn(i64, i64, *mut FrameOut) -> i32;
type FreeFrameFn = extern "system" fn(*mut FrameOut);
type CloseFn = extern "system" fn(i64);
type LastErrorFn = extern "system" fn(*mut u16, i32) -> i32;
type HasImsFn = extern "system" fn(*const u16, *const u16) -> i32;

struct GlueApi {
    open: netcorehost::hostfxr::ManagedFunction<OpenFn>,
    count: netcorehost::hostfxr::ManagedFunction<CountFn>,
    get_frame: netcorehost::hostfxr::ManagedFunction<GetFrameFn>,
    free_frame: netcorehost::hostfxr::ManagedFunction<FreeFrameFn>,
    close: netcorehost::hostfxr::ManagedFunction<CloseFn>,
    last_error: netcorehost::hostfxr::ManagedFunction<LastErrorFn>,
    has_ims: netcorehost::hostfxr::ManagedFunction<HasImsFn>,
    _loader: Arc<AssemblyDelegateLoader>,
}

// SAFETY: bound function pointers are immutable after creation; the C# side serializes its handle
// registry. Mirrors crate::agilent.
unsafe impl Send for GlueApi {}
unsafe impl Sync for GlueApi {}

impl GlueApi {
    fn load(glue_dir: &Path) -> Result<Self> {
        let runtime_config = glue_dir.join("AgilentMidacGlue.runtimeconfig.json");
        if !runtime_config.exists() {
            bail!(
                "Agilent MIDAC glue runtimeconfig not found at {} — set MZPC_AGILENT_MIDAC_GLUE to \
                 the `dotnet build` output dir of glue/agilent_midac/",
                runtime_config.display()
            );
        }
        let assembly = glue_dir.join("AgilentMidacGlue.dll");
        if !assembly.exists() {
            bail!("Agilent MIDAC glue assembly not found at {}", assembly.display());
        }

        let hostfxr =
            nethost::load_hostfxr().context("loading hostfxr (is a .NET 8 runtime installed?)")?;
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

        let ty = pdcstr!("AgilentMidacGlue.Exports, AgilentMidacGlue");
        let open = loader
            .get_function_with_unmanaged_callers_only::<OpenFn>(ty, pdcstr!("Open"))
            .context("binding Open")?;
        let count = loader
            .get_function_with_unmanaged_callers_only::<CountFn>(ty, pdcstr!("FrameCount"))
            .context("binding FrameCount")?;
        let get_frame = loader
            .get_function_with_unmanaged_callers_only::<GetFrameFn>(ty, pdcstr!("GetFrame"))
            .context("binding GetFrame")?;
        let free_frame = loader
            .get_function_with_unmanaged_callers_only::<FreeFrameFn>(ty, pdcstr!("FreeFrame"))
            .context("binding FreeFrame")?;
        let close = loader
            .get_function_with_unmanaged_callers_only::<CloseFn>(ty, pdcstr!("Close"))
            .context("binding Close")?;
        let last_error = loader
            .get_function_with_unmanaged_callers_only::<LastErrorFn>(ty, pdcstr!("LastError"))
            .context("binding LastError")?;
        let has_ims = loader
            .get_function_with_unmanaged_callers_only::<HasImsFn>(ty, pdcstr!("HasImsData"))
            .context("binding HasImsData")?;

        Ok(GlueApi {
            open,
            count,
            get_frame,
            free_frame,
            close,
            last_error,
            has_ims,
            _loader: loader,
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
    let units: Vec<u16> = path.to_string_lossy().encode_utf16().collect();
    if units.contains(&0) {
        bail!("path contains an interior NUL: {}", path.display());
    }
    let mut out = units;
    out.push(0);
    Ok(out)
}

fn resolve_dirs() -> Result<(std::path::PathBuf, std::path::PathBuf)> {
    let glue_dir = std::env::var_os("MZPC_AGILENT_MIDAC_GLUE")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| {
            anyhow!(
                "MZPC_AGILENT_MIDAC_GLUE not set — point it at the `dotnet build` output dir of \
                 glue/agilent_midac/"
            )
        })?;
    let pwiz_dir = std::env::var_os("MZPC_PWIZ_DIR").map(std::path::PathBuf::from).ok_or_else(|| {
        anyhow!("MZPC_PWIZ_DIR not set — the MIDAC DLLs load from <MZPC_PWIZ_DIR>/vendor_api/Agilent")
    })?;
    Ok((glue_dir, pwiz_dir.join("vendor_api").join("Agilent")))
}

/// RAII guard freeing a filled [`FrameOut`]'s unmanaged buffers on drop (panic/early-return path).
struct FrameGuard<'a> {
    api: &'a GlueApi,
    out: FrameOut,
}
impl<'a> FrameGuard<'a> {
    fn disarm(self) -> FrameOut {
        let out = self.out;
        std::mem::forget(self);
        out
    }
}
impl<'a> Drop for FrameGuard<'a> {
    fn drop(&mut self) {
        (self.api.free_frame)(&mut self.out as *mut FrameOut);
    }
}

/// Best-effort detection: does this Agilent `.d` carry ion-mobility data (so the MIDAC reader, not
/// MHDAC, should be used)? Returns false (and logs) if the MIDAC glue/DLLs are unavailable, so the
/// caller cleanly falls back to the MHDAC path.
pub fn file_has_ims_data(path: &Path) -> bool {
    let probe = || -> Result<bool> {
        let (glue_dir, midac_dir) = resolve_dirs()?;
        let api = GlueApi::load(&glue_dir)?;
        let path_utf16 = to_utf16_nul(path)?;
        let midac_utf16 = to_utf16_nul(&midac_dir)?;
        let rc = (api.has_ims)(path_utf16.as_ptr(), midac_utf16.as_ptr());
        if rc < 0 {
            bail!("HasImsData failed (code {rc}): {}", api.last_error().unwrap_or_default());
        }
        Ok(rc == 1)
    };
    match probe() {
        Ok(v) => v,
        Err(e) => {
            log::debug!("MIDAC IM-data probe unavailable for {}: {e}", path.display());
            false
        }
    }
}

/// A native Agilent IM-MS `.d` reader yielding one [`MultiLayerSpectrum`] per IM frame, each with a
/// `mean_inverse_reduced_ion_mobility` array alongside m/z + intensity.
pub struct AgilentMidacReader {
    api: GlueApi,
    handle: i64,
    count: usize,
}

impl AgilentMidacReader {
    pub fn open(path: &Path) -> Result<Self> {
        let (glue_dir, midac_dir) = resolve_dirs()?;
        let api = GlueApi::load(&glue_dir)?;
        let path_utf16 = to_utf16_nul(path)?;
        let midac_utf16 = to_utf16_nul(&midac_dir)?;
        let handle = (api.open)(path_utf16.as_ptr(), midac_utf16.as_ptr());
        if handle < 0 {
            bail!(
                "Agilent MIDAC glue failed to open {} (code {handle}): {}",
                path.display(),
                api.last_error().unwrap_or_default()
            );
        }
        let count = (api.count)(handle);
        if count < 0 {
            let detail = api.last_error().unwrap_or_default();
            (api.close)(handle);
            bail!("Agilent MIDAC glue failed to count frames in {} (code {count}): {detail}", path.display());
        }
        Ok(Self { api, handle, count: count as usize })
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Pull one frame's three arrays across the boundary, copy them out, then free.
    fn fetch(&self, i: usize) -> Result<(Vec<f64>, Vec<f32>, Vec<f64>, FrameOut)> {
        let mut out = FrameOut::zeroed();
        let rc = (self.api.get_frame)(self.handle, i as i64, &mut out as *mut FrameOut);
        if rc != 0 {
            bail!(
                "Agilent MIDAC GetFrame({i}) failed (code {rc}): {}",
                self.api.last_error().unwrap_or_default()
            );
        }
        let guard = FrameGuard { api: &self.api, out };
        let n = guard.out.n_points;
        if n < 0 {
            bail!("Agilent MIDAC GetFrame({i}) returned negative n_points {n}");
        }
        let n = n as usize;
        let (mz, intensity, mobility) = if n == 0
            || guard.out.mz_ptr.is_null()
            || guard.out.intensity_ptr.is_null()
            || guard.out.mobility_ptr.is_null()
        {
            (Vec::new(), Vec::new(), Vec::new())
        } else {
            // SAFETY: on rc==0 with n>0 the glue guarantees each pointer references `n` f64s, valid
            // until FreeFrame. We copy before freeing.
            let mz = unsafe { std::slice::from_raw_parts(guard.out.mz_ptr, n) }.to_vec();
            let intensity: Vec<f32> = unsafe { std::slice::from_raw_parts(guard.out.intensity_ptr, n) }
                .iter()
                .map(|&v| v as f32)
                .collect();
            let mobility = unsafe { std::slice::from_raw_parts(guard.out.mobility_ptr, n) }.to_vec();
            (mz, intensity, mobility)
        };
        let mut out = guard.disarm();
        (self.api.free_frame)(&mut out as *mut FrameOut);
        Ok((mz, intensity, mobility, out))
    }

    /// Build the mzdata spectrum for frame `i`: Float64 MZArray + Float32 IntensityArray + Float64
    /// `MeanInverseReducedIonMobilityArray` (the drift dimension MIDAC exposes), plus the
    /// `MS:1000294 "mass spectrum"` param and a `ScanEvent` with `start_time` in minutes.
    pub fn spectrum(&self, i: usize) -> Result<MultiLayerSpectrum> {
        if i >= self.count {
            bail!("Agilent MIDAC frame index {i} out of range (count {})", self.count);
        }
        let (mz, intensity, mobility, meta) = self.fetch(i)?;

        let mut arrays = BinaryArrayMap::new();
        let mut mz_da = DataArray::wrap(&ArrayType::MZArray, BinaryDataArrayType::Float64, Vec::new());
        mz_da.update_buffer(mz.as_slice()).map_err(|e| anyhow!("encoding m/z: {e}"))?;
        mz_da.unit = Unit::MZ;
        arrays.add(mz_da);
        let mut int_da =
            DataArray::wrap(&ArrayType::IntensityArray, BinaryDataArrayType::Float32, Vec::new());
        int_da.update_buffer(intensity.as_slice()).map_err(|e| anyhow!("encoding intensity: {e}"))?;
        int_da.unit = Unit::DetectorCounts;
        arrays.add(int_da);
        let mut mob_da = DataArray::wrap(
            &ArrayType::MeanInverseReducedIonMobilityArray,
            BinaryDataArrayType::Float64,
            Vec::new(),
        );
        mob_da.update_buffer(mobility.as_slice()).map_err(|e| anyhow!("encoding mobility: {e}"))?;
        arrays.add(mob_da);

        let polarity = match meta.polarity {
            1 => ScanPolarity::Positive,
            -1 => ScanPolarity::Negative,
            _ => ScanPolarity::Unknown,
        };
        let ms_level = if meta.ms_level >= 1 { meta.ms_level as u8 } else { 1 };

        let mut descr = SpectrumDescription {
            id: format!("frameId={}", meta.frame_id),
            index: i,
            ms_level,
            signal_continuity: SignalContinuity::Profile,
            polarity,
            ..Default::default()
        };
        descr.add_param(Param::builder().name("mass spectrum").curie(curie!(MS:1000294)).build());
        let mut scan = ScanEvent::default();
        scan.start_time = meta.rt_minutes;
        descr.acquisition.scans.push(scan);

        Ok(MultiLayerSpectrum::new(descr, Some(arrays), None, None))
    }
}

impl Drop for AgilentMidacReader {
    fn drop(&mut self) {
        (self.api.close)(self.handle);
    }
}
