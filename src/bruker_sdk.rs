//! Bruker **timsdata** SDK reader for TDF + TSF → mzdata spectra.
//!
//! A *parallel* path to the default pure-Rust readers (timsrust / [`crate::bruker_tsf`] /
//! [`crate::bruker_native`]), selected by `--bruker-sdk`. Where those decode the binaries directly,
//! this calls Bruker's official `timsdata` shared library (`timsdata.dll` on Windows,
//! `libtimsdata.so` on Linux — there is NO macOS build), so peak m/z come from the vendor's own
//! index→m/z calibration. One library serves both formats: `tims_*` for TDF (3D PASEF frames) and
//! `tsf_*` for TSF (line/profile spectra). BAF is NOT covered here — that is a different Bruker
//! library, `baf2sql` (see [`crate::bruker_baf`]).
//!
//! Frame metadata (rt / MS level / polarity / scan count) is read from the `analysis.t{d,s}f` SQLite
//! exactly as the pure-Rust readers do; only the peak arrays come through the SDK. Each spectrum is
//! emitted as a [`MultiLayerSpectrum`] so it flows through the same `convert_vendor_reader` seam as
//! every other native reader.
//!
//! Gated `cfg(any(windows, target_os = "linux"))` by the caller, since it needs `libloading` and the
//! vendor library only exists on those platforms.

use std::env;
use std::ffi::{c_char, c_void, CString};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use libloading::Library;
use rusqlite::{Connection, OpenFlags};

use mzdata::curie;
use mzdata::params::{Param, Unit};
use mzdata::prelude::ParamDescribed;
use mzdata::spectrum::bindata::{ArrayType, BinaryArrayMap, BinaryDataArrayType, DataArray};
use mzdata::spectrum::{
    MultiLayerSpectrum, ScanEvent, ScanPolarity, SignalContinuity, SpectrumDescription,
};

/// Per-spectrum peak-count ceiling — guards against a corrupt cache / hostile library returning an
/// enormous count that would exhaust memory before we ever read it.
const MAX_PEAKS_PER_SPECTRUM: usize = 50_000_000;
/// Mobility-scan-per-frame ceiling (real timsTOF frames have hundreds–low thousands).
const MAX_SCANS_PER_FRAME: u32 = 1_000_000;
/// Upper bound on the bytes a single TDF frame buffer may need (`tims_read_scans_v2`). Bounds the
/// grow-and-retry loop so a buggy/hostile library cannot drive an unbounded allocation.
const MAX_FRAME_BUFFER_BYTES: usize = 512 * 1024 * 1024;
/// Upper bound on the SDK error-message buffer; clamped so error reporting cannot itself allocate
/// hugely.
const MAX_ERROR_BUFFER_BYTES: usize = 64 * 1024;

// --- timsdata C ABI --------------------------------------------------------
// Signatures verified against the Bruker SDK and pyTDFSDK's ctypes bindings.

type TsfOpen = unsafe extern "C" fn(*const c_char, u32) -> u64;
type TsfClose = unsafe extern "C" fn(u64);
type TsfGetLastErrorString = unsafe extern "C" fn(*mut c_char, u32) -> u32;
type TsfReadLineSpectrumV2 = unsafe extern "C" fn(u64, i64, *mut f64, *mut f32, i32) -> i32;
type TsfIndexToMz = unsafe extern "C" fn(u64, i64, *const f64, *mut f64, u32) -> u32;

type TimsOpen = unsafe extern "C" fn(*const c_char, u32) -> u64;
type TimsClose = unsafe extern "C" fn(u64);
type TimsGetLastErrorString = unsafe extern "C" fn(*mut c_char, u32) -> u32;
type TimsReadScansV2 = unsafe extern "C" fn(u64, i64, u32, u32, *mut c_void, u32) -> u32;
type TimsIndexToMz = unsafe extern "C" fn(u64, i64, *const f64, *mut f64, u32) -> u32;
type TimsScannumToOneOverK0 = unsafe extern "C" fn(u64, i64, *const f64, *mut f64, u32) -> u32;

/// Resolved + bound function pointers into a loaded `timsdata` library.
#[derive(Clone)]
struct TimsDataApi {
    _library: Arc<Library>,
    tsf_open: TsfOpen,
    tsf_close: TsfClose,
    tsf_get_last_error_string: TsfGetLastErrorString,
    tsf_read_line_spectrum_v2: TsfReadLineSpectrumV2,
    tsf_index_to_mz: TsfIndexToMz,
    tims_open: TimsOpen,
    tims_close: TimsClose,
    tims_get_last_error_string: TimsGetLastErrorString,
    tims_read_scans_v2: TimsReadScansV2,
    tims_index_to_mz: TimsIndexToMz,
    tims_scannum_to_oneoverk0: TimsScannumToOneOverK0,
    library_path: PathBuf,
}

impl TimsDataApi {
    fn load(sdk_lib: Option<&Path>) -> Result<Self> {
        let library_path = discover_timsdata_library(sdk_lib)?;
        // SAFETY: the path is user-controlled but only loaded as a dynamic library after discovery;
        // every symbol is resolved immediately and copied out as a C function pointer while the
        // Library is kept alive by the Arc.
        let library = Arc::new(unsafe { Library::new(&library_path) }.with_context(|| {
            format!("failed to load Bruker timsdata library {}", library_path.display())
        })?);

        // SAFETY: symbol names + signatures match the public timsdata C API. An incompatible library
        // fails symbol lookup here, before any call is made.
        unsafe {
            macro_rules! sym {
                ($t:ty, $name:literal) => {
                    *library
                        .get::<$t>($name)
                        .map_err(|e| missing_symbol(&library_path, e))?
                };
            }
            Ok(Self {
                tsf_open: sym!(TsfOpen, b"tsf_open\0"),
                tsf_close: sym!(TsfClose, b"tsf_close\0"),
                tsf_get_last_error_string: sym!(TsfGetLastErrorString, b"tsf_get_last_error_string\0"),
                tsf_read_line_spectrum_v2: sym!(TsfReadLineSpectrumV2, b"tsf_read_line_spectrum_v2\0"),
                tsf_index_to_mz: sym!(TsfIndexToMz, b"tsf_index_to_mz\0"),
                tims_open: sym!(TimsOpen, b"tims_open\0"),
                tims_close: sym!(TimsClose, b"tims_close\0"),
                tims_get_last_error_string: sym!(TimsGetLastErrorString, b"tims_get_last_error_string\0"),
                tims_read_scans_v2: sym!(TimsReadScansV2, b"tims_read_scans_v2\0"),
                tims_index_to_mz: sym!(TimsIndexToMz, b"tims_index_to_mz\0"),
                tims_scannum_to_oneoverk0: sym!(TimsScannumToOneOverK0, b"tims_scannum_to_oneoverk0\0"),
                _library: library,
                library_path,
            })
        }
    }

    fn last_error(&self, getter: TsfGetLastErrorString) -> String {
        // Two-call pattern: ask for the size, then fill. Clamp so reporting can't itself blow up.
        let required = (unsafe { getter(std::ptr::null_mut(), 0) }.max(1) as usize)
            .min(MAX_ERROR_BUFFER_BYTES);
        let mut buffer = vec![0u8; required];
        // SAFETY: buffer is valid and writable for the requested size.
        unsafe {
            getter(buffer.as_mut_ptr().cast::<c_char>(), required as u32);
        }
        let nul = buffer.iter().position(|b| *b == 0).unwrap_or(buffer.len());
        String::from_utf8_lossy(&buffer[..nul]).to_string()
    }

    fn last_error_tsf(&self) -> String {
        self.last_error(self.tsf_get_last_error_string)
    }
    fn last_error_tims(&self) -> String {
        self.last_error(self.tims_get_last_error_string)
    }
}

fn missing_symbol(path: &Path, error: libloading::Error) -> anyhow::Error {
    anyhow!(
        "Bruker timsdata library {} is missing a required symbol: {error}",
        path.display()
    )
}

/// Discover the `timsdata` shared library: explicit arg first, then `TIMSDATA_LIB_DIR`, treating
/// either as a file or an SDK-root directory (`win64/`, `linux64/`, or flat); finally fall back to
/// the bare library name so the OS loader can resolve it from `PATH` / `LD_LIBRARY_PATH`.
fn discover_timsdata_library(sdk_lib: Option<&Path>) -> Result<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(explicit) = sdk_lib {
        candidates.extend(timsdata_candidates_from_root(explicit));
    }
    if let Some(root) = env::var_os("TIMSDATA_LIB_DIR").map(PathBuf::from) {
        candidates.extend(timsdata_candidates_from_root(&root));
    }
    for candidate in candidates {
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    // Last resort: bare names resolved by the dynamic loader's own search path.
    #[cfg(windows)]
    {
        Ok(PathBuf::from("timsdata.dll"))
    }
    #[cfg(not(windows))]
    {
        Ok(PathBuf::from("libtimsdata.so"))
    }
}

/// Expand an SDK path (file or root directory) into the library-file candidates to probe, covering
/// the flat, `linux64/`, and `win64/` layouts Bruker ships.
fn timsdata_candidates_from_root(root: &Path) -> Vec<PathBuf> {
    if root.is_file() {
        return vec![root.to_path_buf()];
    }
    let mut candidates = Vec::new();
    for name in [
        "libtimsdata.so",
        "timsdata.dll",
        "linux64/libtimsdata.so",
        "win64/timsdata.dll",
    ] {
        candidates.push(root.join(name));
    }
    if let Some(parent) = root.parent() {
        match root.file_name().and_then(|n| n.to_str()) {
            Some("linux64") => candidates.push(parent.join("linux64/libtimsdata.so")),
            Some("win64") => candidates.push(parent.join("win64/timsdata.dll")),
            _ => {}
        }
    }
    candidates
}

fn path_to_cstring(path: &Path) -> Result<CString> {
    // On Unix build from raw bytes so non-UTF8 paths survive; Windows has no byte-accurate view, so
    // fall back to the lossy conversion there.
    #[cfg(unix)]
    let bytes = {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    };
    #[cfg(not(unix))]
    let bytes = path.to_string_lossy().into_owned().into_bytes();
    CString::new(bytes)
        .map_err(|_| anyhow!("path contains an interior NUL byte: {}", path.display()))
}

/// The `.d` analysis directory the SDK wants (it takes the directory, not the `analysis.*` file).
fn analysis_dir(input: &Path, marker: &str) -> Result<PathBuf> {
    if input.is_dir() && input.join(marker).is_file() {
        return Ok(input.to_path_buf());
    }
    if input.file_name().and_then(|v| v.to_str()) == Some(marker) {
        if let Some(parent) = input.parent() {
            return Ok(parent.to_path_buf());
        }
    }
    bail!("{marker} not found for {}", input.display())
}

// --- shared frame metadata (from the analysis.* SQLite) --------------------

/// One `Frames` row: everything we need that is NOT in the binary peak data.
#[derive(Debug, Clone)]
struct FrameMeta {
    id: i64,
    rt_seconds: f64,
    ms_level: u8,
    polarity: ScanPolarity,
    /// Mobility scans in this frame (TDF only; 0 for TSF).
    num_scans: u32,
}

/// MsMsType → 1-based MS level. 0 is MS1 for both TDF and TSF; every non-zero acquisition type
/// (8 = DDA-PASEF, 9 = DIA-PASEF, 2/… for TSF MS/MS) is MS2.
fn msms_type_to_ms_level(msms: i64) -> u8 {
    if msms == 0 {
        1
    } else {
        2
    }
}

fn polarity_from_str(s: &str) -> ScanPolarity {
    match s {
        "+" => ScanPolarity::Positive,
        "-" => ScanPolarity::Negative,
        _ => ScanPolarity::Unknown,
    }
}

/// Read the `Frames` table in Id order. `with_scans` pulls `NumScans` (TDF); TSF has no such column.
fn read_frames(conn: &Connection, with_scans: bool) -> Result<Vec<FrameMeta>> {
    let sql = if with_scans {
        "SELECT Id, Time, MsMsType, Polarity, NumScans FROM Frames ORDER BY Id"
    } else {
        "SELECT Id, Time, MsMsType, Polarity FROM Frames ORDER BY Id"
    };
    let mut stmt = conn.prepare(sql).context("preparing Frames query")?;
    let rows = stmt
        .query_map([], |row| {
            let polarity: String = row.get::<_, Option<String>>(3)?.unwrap_or_default();
            Ok(FrameMeta {
                id: row.get(0)?,
                rt_seconds: row.get::<_, Option<f64>>(1)?.unwrap_or(0.0),
                ms_level: msms_type_to_ms_level(row.get::<_, Option<i64>>(2)?.unwrap_or(0)),
                polarity: polarity_from_str(&polarity),
                num_scans: if with_scans {
                    row.get::<_, Option<i64>>(4)?.unwrap_or(0).clamp(0, i64::from(u32::MAX)) as u32
                } else {
                    0
                },
            })
        })
        .context("querying Frames")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("reading Frames rows")?;
    Ok(rows)
}

fn open_sqlite(dir: &Path, marker: &str) -> Result<Connection> {
    let db = dir.join(marker);
    Connection::open_with_flags(
        &db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening {}", db.display()))
}

/// Build the mzdata spectrum description shared by both readers.
fn make_description(i: usize, frame: &FrameMeta, continuity: SignalContinuity) -> SpectrumDescription {
    let mut descr = SpectrumDescription {
        id: format!("frame={}", frame.id),
        index: i,
        ms_level: frame.ms_level,
        signal_continuity: continuity,
        polarity: frame.polarity,
        ..Default::default()
    };
    descr.add_param(
        Param::builder()
            .name("mass spectrum")
            .curie(curie!(MS:1000294))
            .build(),
    );
    let mut scan = ScanEvent::default();
    scan.start_time = frame.rt_seconds / 60.0; // mzdata scan start_time is minutes
    descr.acquisition.scans.push(scan);
    descr
}

// --- TSF reader (line spectra) ---------------------------------------------

pub struct TsfSdkReader {
    api: TimsDataApi,
    handle: u64,
    frames: Vec<FrameMeta>,
    /// timsdata handles are not known to be thread-safe; this marker makes the reader neither `Send`
    /// nor `Sync` so it cannot be shared across threads (sound for the single-threaded convert path).
    _not_thread_safe: PhantomData<*const ()>,
}

impl TsfSdkReader {
    pub fn open(input: &Path) -> Result<Self> {
        let dir = analysis_dir(input, "analysis.tsf")?;
        let api = TimsDataApi::load(None)?;
        let handle = open_handle(api.tsf_open, &dir).map_err(|e| {
            anyhow!("tsf_open failed for {}: {e} ({})", dir.display(), api.last_error_tsf())
        })?;
        let conn = open_sqlite(&dir, "analysis.tsf")?;
        let frames = read_frames(&conn, false)?;
        Ok(Self { api, handle, frames, _not_thread_safe: PhantomData })
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// Read one frame's line spectrum: returns `(m/z f64, intensity f32)`, vendor-calibrated.
    fn read_line(&self, frame_id: i64) -> Result<(Vec<f64>, Vec<f32>)> {
        let mut cap: usize = 1024;
        loop {
            if cap > MAX_PEAKS_PER_SPECTRUM {
                bail!("TSF frame {frame_id} exceeds the {MAX_PEAKS_PER_SPECTRUM}-peak safety limit");
            }
            let cap_i32 = i32::try_from(cap).unwrap_or(i32::MAX);
            let mut indices = vec![0f64; cap];
            let mut intensities = vec![0f32; cap];
            // SAFETY: both buffers hold `cap` elements; we pass `cap_i32` as the capacity so the SDK
            // never writes past them. A return > cap means "too small" (retry), <0 means error.
            let required = unsafe {
                (self.api.tsf_read_line_spectrum_v2)(
                    self.handle,
                    frame_id,
                    indices.as_mut_ptr(),
                    intensities.as_mut_ptr(),
                    cap_i32,
                )
            };
            if required < 0 {
                bail!(
                    "tsf_read_line_spectrum_v2 failed for frame {frame_id}: {}",
                    self.api.last_error_tsf()
                );
            }
            let n = required as usize;
            if n > cap {
                cap = n; // exact required size; loop re-validates against the peak cap
                continue;
            }
            indices.truncate(n);
            intensities.truncate(n);
            let mz = self.indices_to_mz(frame_id, &indices)?;
            return Ok((mz, intensities));
        }
    }

    fn indices_to_mz(&self, frame_id: i64, indices: &[f64]) -> Result<Vec<f64>> {
        if indices.is_empty() {
            return Ok(Vec::new());
        }
        let count = u32::try_from(indices.len())
            .map_err(|_| anyhow!("TSF frame {frame_id} has too many peaks to convert"))?;
        let mut mz = vec![0f64; indices.len()];
        // SAFETY: `mz` and `indices` both hold `count` f64 elements.
        let ok = unsafe {
            (self.api.tsf_index_to_mz)(self.handle, frame_id, indices.as_ptr(), mz.as_mut_ptr(), count)
        };
        if ok == 0 {
            bail!(
                "tsf_index_to_mz failed for frame {frame_id}: {}",
                self.api.last_error_tsf()
            );
        }
        Ok(mz)
    }

    pub fn spectrum(&self, i: usize) -> Result<MultiLayerSpectrum> {
        let frame = self
            .frames
            .get(i)
            .with_context(|| format!("TSF frame index {i} out of range"))?;
        let (mz, intensity) = self.read_line(frame.id)?;
        let arrays = mz_intensity_arrays(&mz, &intensity, None)?;
        let descr = make_description(i, frame, SignalContinuity::Centroid);
        Ok(MultiLayerSpectrum::new(descr, Some(arrays), None, None))
    }

    pub fn sample_arrays(&self) -> Result<BinaryArrayMap> {
        sample_first_nonempty(self.len(), |i| self.spectrum(i))
    }
}

impl Drop for TsfSdkReader {
    fn drop(&mut self) {
        if self.handle != 0 {
            // SAFETY: handle came from tsf_open and is closed exactly once.
            unsafe { (self.api.tsf_close)(self.handle) };
            self.handle = 0;
        }
    }
}

// --- TDF reader (3D PASEF frames) ------------------------------------------

pub struct TdfSdkReader {
    api: TimsDataApi,
    handle: u64,
    frames: Vec<FrameMeta>,
    _not_thread_safe: PhantomData<*const ()>,
}

impl TdfSdkReader {
    pub fn open(input: &Path) -> Result<Self> {
        let dir = analysis_dir(input, "analysis.tdf")?;
        let api = TimsDataApi::load(None)?;
        let handle = open_handle(api.tims_open, &dir).map_err(|e| {
            anyhow!("tims_open failed for {}: {e} ({})", dir.display(), api.last_error_tims())
        })?;
        let conn = open_sqlite(&dir, "analysis.tdf")?;
        let frames = read_frames(&conn, true)?;
        Ok(Self { api, handle, frames, _not_thread_safe: PhantomData })
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// Read one PASEF frame as flat `(index, intensity, scan_number)` peaks across all its mobility
    /// scans via `tims_read_scans_v2`, growing the buffer until it fits.
    fn read_frame_peaks(&self, frame: &FrameMeta) -> Result<Vec<RawPeak>> {
        if frame.num_scans == 0 {
            return Ok(Vec::new());
        }
        if frame.num_scans > MAX_SCANS_PER_FRAME {
            bail!(
                "TDF frame {} reports {} scans, exceeding the {MAX_SCANS_PER_FRAME} safety limit",
                frame.id,
                frame.num_scans
            );
        }
        let max_u32 = MAX_FRAME_BUFFER_BYTES / 4;
        let mut cap: usize = (frame.num_scans as usize).max(1024);
        loop {
            if cap > max_u32 {
                bail!("TDF frame {} exceeds the {MAX_FRAME_BUFFER_BYTES}-byte buffer limit", frame.id);
            }
            let mut buf = vec![0u32; cap];
            let len_bytes = (cap * 4) as u32;
            // SAFETY: buffer holds `cap` u32 (= len_bytes bytes); the SDK writes at most that and
            // returns the byte length it needs.
            let required = unsafe {
                (self.api.tims_read_scans_v2)(
                    self.handle,
                    frame.id,
                    0,
                    frame.num_scans,
                    buf.as_mut_ptr().cast::<c_void>(),
                    len_bytes,
                )
            };
            if required == 0 {
                bail!(
                    "tims_read_scans_v2 failed for frame {}: {}",
                    frame.id,
                    self.api.last_error_tims()
                );
            }
            let required = required as usize;
            if required > MAX_FRAME_BUFFER_BYTES {
                bail!("TDF frame {} needs {required} bytes, over the limit", frame.id);
            }
            if required > len_bytes as usize {
                cap = required / 4 + 1;
                continue;
            }
            buf.truncate(required / 4);
            return decode_scan_buffer(&buf, frame.num_scans as usize)
                .with_context(|| format!("decoding TDF frame {}", frame.id));
        }
    }

    pub fn spectrum(&self, i: usize) -> Result<MultiLayerSpectrum> {
        let frame = self
            .frames
            .get(i)
            .with_context(|| format!("TDF frame index {i} out of range"))?;
        let peaks = self.read_frame_peaks(frame)?;

        // Convert all indices → m/z and all scan numbers → 1/K0 in single SDK calls, then sort by m/z
        // so the peaks facet's non-decreasing-m/z invariant holds (the SDK yields mobility-major).
        let indices: Vec<f64> = peaks.iter().map(|p| p.index as f64).collect();
        let scans: Vec<f64> = peaks.iter().map(|p| p.scan as f64).collect();
        let mz = self.convert(self.api.tims_index_to_mz, frame.id, &indices, "tims_index_to_mz")?;
        let mobility = self.convert(
            self.api.tims_scannum_to_oneoverk0,
            frame.id,
            &scans,
            "tims_scannum_to_oneoverk0",
        )?;

        let mut triples: Vec<(f64, f32, f64)> = (0..peaks.len())
            .map(|k| (mz[k], peaks[k].intensity as f32, mobility[k]))
            .collect();
        triples.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mz: Vec<f64> = triples.iter().map(|t| t.0).collect();
        let intensity: Vec<f32> = triples.iter().map(|t| t.1).collect();
        let mob: Vec<f64> = triples.iter().map(|t| t.2).collect();

        let arrays = mz_intensity_arrays(&mz, &intensity, Some(&mob))?;
        let descr = make_description(i, frame, SignalContinuity::Centroid);
        Ok(MultiLayerSpectrum::new(descr, Some(arrays), None, None))
    }

    /// `m/z = (a + b·tof)²` coefficients, recovered from the SDK's run-constant index→m/z calibration
    /// (`a = √(index_to_mz(0))`, `b = √(index_to_mz(1)) − a`) — mirrors `bruker_native::TofMzModel`.
    /// Falls back to the (0,1) identity placeholder (which readers guard) if the SDK call fails.
    pub fn tof_mz_model(&self) -> (f64, f64) {
        let fid = self.frames.first().map(|f| f.id).unwrap_or(0);
        match self.convert(self.api.tims_index_to_mz, fid, &[0.0_f64, 1.0_f64], "tims_index_to_mz") {
            Ok(v) if v.len() == 2 => {
                let a = v[0].max(0.0).sqrt();
                let b = v[1].max(0.0).sqrt() - a;
                (a, b)
            }
            _ => (0.0, 1.0),
        }
    }

    /// Build the ims-compact spectrum for frame `i`: integer `tof` (raw index) + intensity + per-peak
    /// 1/K0, in the SDK's native mobility-major order (NO m/z sort — the ims-compact reader recovers
    /// m/z from the tof grid). Same layout as `bruker_native::ims_compact_spectrum`, so it flows
    /// through `write_ims_compact_archive`. `int_intensity` stores counts as Int32 for byte-plane.
    pub fn ims_compact_spectrum(&self, i: usize, int_intensity: bool) -> Result<MultiLayerSpectrum> {
        let frame = self
            .frames
            .get(i)
            .with_context(|| format!("TDF frame index {i} out of range"))?;
        let peaks = self.read_frame_peaks(frame)?;
        let scans: Vec<f64> = peaks.iter().map(|p| p.scan as f64).collect();
        let mobility = self.convert(
            self.api.tims_scannum_to_oneoverk0,
            frame.id,
            &scans,
            "tims_scannum_to_oneoverk0",
        )?;

        let mut tof: Vec<i32> = Vec::with_capacity(peaks.len());
        let (mut int_f32, mut int_i32): (Vec<f32>, Vec<i32>) = (Vec::new(), Vec::new());
        for p in &peaks {
            tof.push(i32::try_from(p.index).map_err(|_| anyhow!("TOF index {} exceeds i32", p.index))?);
            if int_intensity {
                int_i32.push(i32::try_from(p.intensity).map_err(|_| anyhow!("intensity {} exceeds i32", p.intensity))?);
            } else {
                int_f32.push(p.intensity as f32);
            }
        }

        let mut arrays = BinaryArrayMap::new();
        let mut tof_da = DataArray::wrap(&ArrayType::nonstandard("tof"), BinaryDataArrayType::Int32, Vec::new());
        tof_da.update_buffer(tof.as_slice()).map_err(|e| anyhow!("encoding tof: {e}"))?;
        arrays.add(tof_da);
        let mut int_da = if int_intensity {
            let mut da = DataArray::wrap(&ArrayType::IntensityArray, BinaryDataArrayType::Int32, Vec::new());
            da.update_buffer(int_i32.as_slice()).map_err(|e| anyhow!("encoding intensity: {e}"))?;
            da
        } else {
            let mut da = DataArray::wrap(&ArrayType::IntensityArray, BinaryDataArrayType::Float32, Vec::new());
            da.update_buffer(int_f32.as_slice()).map_err(|e| anyhow!("encoding intensity: {e}"))?;
            da
        };
        int_da.unit = Unit::DetectorCounts;
        arrays.add(int_da);
        let mut mob_da = DataArray::wrap(
            &ArrayType::MeanInverseReducedIonMobilityArray,
            BinaryDataArrayType::Float64,
            Vec::new(),
        );
        mob_da.update_buffer(mobility.as_slice()).map_err(|e| anyhow!("encoding mobility: {e}"))?;
        arrays.add(mob_da);

        let mut descr = make_description(i, frame, SignalContinuity::Centroid);
        // Observed-m/z range: the output stores integer `tof`, so reconstruct m/z = (a + b·tof)²
        // (monotonic in tof) over the min/max TOF index present. Without this the viewer shows
        // "m/z 0–0".
        if let (Some(&tmin), Some(&tmax)) = (tof.iter().min(), tof.iter().max()) {
            let (a, b) = self.tof_mz_model();
            let mz = |t: i32| -> f64 {
                let v = a + b * t as f64;
                v * v
            };
            let (mz_a, mz_b) = (mz(tmin), mz(tmax));
            crate::set_observed_mz_range(&mut descr, mz_a.min(mz_b), mz_a.max(mz_b));
        }
        Ok(MultiLayerSpectrum::new(descr, Some(arrays), None, None))
    }

    /// Shared wrapper for the two per-peak conversion calls (index→m/z, scan→1/K0): both take a
    /// `*const f64` in and a `*mut f64` out with the same count, returning 0 on error.
    fn convert(
        &self,
        f: TimsIndexToMz,
        frame_id: i64,
        input: &[f64],
        name: &str,
    ) -> Result<Vec<f64>> {
        if input.is_empty() {
            return Ok(Vec::new());
        }
        let count = u32::try_from(input.len())
            .map_err(|_| anyhow!("TDF frame {frame_id} has too many peaks for {name}"))?;
        let mut out = vec![0f64; input.len()];
        // SAFETY: `out` and `input` both hold `count` f64 elements.
        let ok = unsafe { f(self.handle, frame_id, input.as_ptr(), out.as_mut_ptr(), count) };
        if ok == 0 {
            bail!("{name} failed for frame {frame_id}: {}", self.api.last_error_tims());
        }
        Ok(out)
    }

    pub fn sample_arrays(&self) -> Result<BinaryArrayMap> {
        sample_first_nonempty(self.len(), |i| self.spectrum(i))
    }
}

impl Drop for TdfSdkReader {
    fn drop(&mut self) {
        if self.handle != 0 {
            // SAFETY: handle came from tims_open and is closed exactly once.
            unsafe { (self.api.tims_close)(self.handle) };
            self.handle = 0;
        }
    }
}

/// A single decoded TDF peak before calibration: raw TOF `index`, raw `intensity`, owning mobility
/// `scan` number.
#[derive(Debug, Clone, Copy, PartialEq)]
struct RawPeak {
    index: u32,
    intensity: u32,
    scan: u32,
}

/// Decode a `tims_read_scans_v2` output buffer into flat peaks. Layout (all `u32`): the first
/// `num_scans` words are the per-scan peak counts; then, per scan in order, `npeaks` index words
/// followed by `npeaks` intensity words. Every offset is bounds-checked so a corrupt/hostile buffer
/// yields an error, never an out-of-bounds read.
fn decode_scan_buffer(buf: &[u32], num_scans: usize) -> Result<Vec<RawPeak>> {
    if buf.len() < num_scans {
        bail!("scan buffer shorter ({}) than its {num_scans}-scan header", buf.len());
    }
    let mut peaks = Vec::new();
    let mut d = num_scans; // data starts after the per-scan count header
    for scan in 0..num_scans {
        let npeaks = buf[scan] as usize;
        let end_idx = d
            .checked_add(npeaks)
            .filter(|&e| e <= buf.len())
            .ok_or_else(|| anyhow!("scan {scan} index block overruns the buffer"))?;
        let idx_block = &buf[d..end_idx];
        d = end_idx;
        let end_int = d
            .checked_add(npeaks)
            .filter(|&e| e <= buf.len())
            .ok_or_else(|| anyhow!("scan {scan} intensity block overruns the buffer"))?;
        let int_block = &buf[d..end_int];
        d = end_int;
        if peaks.len().saturating_add(npeaks) > MAX_PEAKS_PER_SPECTRUM {
            bail!("frame exceeds the {MAX_PEAKS_PER_SPECTRUM}-peak safety limit");
        }
        let scan_u32 = scan as u32;
        for k in 0..npeaks {
            peaks.push(RawPeak {
                index: idx_block[k],
                intensity: int_block[k],
                scan: scan_u32,
            });
        }
    }
    Ok(peaks)
}

// --- shared helpers --------------------------------------------------------

/// Open a TDF/TSF handle, preferring the vendor-recalibrated state (flag 1) and falling back to the
/// stored calibration (flag 0). Returns the non-zero handle or an error.
fn open_handle(open: unsafe extern "C" fn(*const c_char, u32) -> u64, dir: &Path) -> Result<u64> {
    let c_dir = path_to_cstring(dir)?;
    // SAFETY: the CString lives for both calls; a zero return means failure.
    let handle = unsafe { open(c_dir.as_ptr(), 1) };
    if handle != 0 {
        return Ok(handle);
    }
    let handle = unsafe { open(c_dir.as_ptr(), 0) };
    if handle != 0 {
        return Ok(handle);
    }
    bail!("open returned a null handle")
}

/// Build the standard `(m/z f64, intensity f32[, mobility f64])` array map used by both readers.
fn mz_intensity_arrays(mz: &[f64], intensity: &[f32], mobility: Option<&[f64]>) -> Result<BinaryArrayMap> {
    let mut arrays = BinaryArrayMap::new();
    let mut mz_da = DataArray::wrap(&ArrayType::MZArray, BinaryDataArrayType::Float64, Vec::new());
    mz_da.update_buffer(mz).map_err(|e| anyhow!("encoding m/z: {e}"))?;
    mz_da.unit = Unit::MZ;
    arrays.add(mz_da);

    let mut int_da =
        DataArray::wrap(&ArrayType::IntensityArray, BinaryDataArrayType::Float32, Vec::new());
    int_da.update_buffer(intensity).map_err(|e| anyhow!("encoding intensity: {e}"))?;
    int_da.unit = Unit::DetectorCounts;
    arrays.add(int_da);

    if let Some(mob) = mobility {
        let mut mob_da = DataArray::wrap(
            &ArrayType::MeanInverseReducedIonMobilityArray,
            BinaryDataArrayType::Float64,
            Vec::new(),
        );
        mob_da.update_buffer(mob).map_err(|e| anyhow!("encoding mobility: {e}"))?;
        arrays.add(mob_da);
    }
    Ok(arrays)
}

/// Find the first spectrum with non-empty arrays for schema sampling (mirrors the other readers).
fn sample_first_nonempty(
    len: usize,
    mut spectrum: impl FnMut(usize) -> Result<MultiLayerSpectrum>,
) -> Result<BinaryArrayMap> {
    if len == 0 {
        bail!("no frames to sample");
    }
    for i in 0..len {
        let spec = spectrum(i)?;
        if let Some(arrays) = spec.arrays {
            if !arrays.is_empty() {
                return Ok(arrays);
            }
        }
    }
    // All empty: fall back to the first spectrum's (empty) arrays so the schema still has the columns.
    spectrum(0)?
        .arrays
        .ok_or_else(|| anyhow!("sample spectrum has no arrays"))
}

// --- unified entry point ---------------------------------------------------

/// A Bruker `.d` reader over the timsdata SDK, dispatching to TDF or TSF by the marker file present.
pub enum BrukerSdkReader {
    Tsf(TsfSdkReader),
    Tdf(TdfSdkReader),
}

impl BrukerSdkReader {
    pub fn open(input: &Path) -> Result<Self> {
        // TDF takes precedence: a hybrid dir with both markers is a PASEF acquisition.
        if input.is_dir() && input.join("analysis.tdf").is_file() {
            Ok(Self::Tdf(TdfSdkReader::open(input)?))
        } else if input.is_dir() && input.join("analysis.tsf").is_file() {
            Ok(Self::Tsf(TsfSdkReader::open(input)?))
        } else {
            bail!("{} is not a Bruker TDF/TSF .d directory", input.display())
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Self::Tsf(r) => r.len(),
            Self::Tdf(r) => r.len(),
        }
    }

    pub fn sample_arrays(&self) -> Result<BinaryArrayMap> {
        match self {
            Self::Tsf(r) => r.sample_arrays(),
            Self::Tdf(r) => r.sample_arrays(),
        }
    }

    pub fn spectrum(&self, i: usize) -> Result<MultiLayerSpectrum> {
        match self {
            Self::Tsf(r) => r.spectrum(i),
            Self::Tdf(r) => r.spectrum(i),
        }
    }
}

/// Diagnostic: the vendor SDK's 1/K0 for scan numbers `[0, n)` of `frame_id`, via
/// `tims_scannum_to_oneoverk0`. Used to compare the SDK's mobility calibration against timsrust's
/// `Scan2ImConverter` scan-by-scan (isolating calibration from peak coverage).
pub fn scannum_to_oneoverk0_table(input: &Path, frame_id: i64, n: usize) -> Result<Vec<f64>> {
    let dir = analysis_dir(input, "analysis.tdf")?;
    let api = TimsDataApi::load(None)?;
    let handle = open_handle(api.tims_open, &dir)
        .map_err(|e| anyhow!("tims_open failed: {e} ({})", api.last_error_tims()))?;
    let scans: Vec<f64> = (0..n).map(|s| s as f64).collect();
    let mut out = vec![0f64; n.max(1)];
    let count = u32::try_from(n).unwrap_or(u32::MAX);
    // SAFETY: scans + out both hold `n` f64; handle is valid until tims_close below.
    let ok = unsafe {
        (api.tims_scannum_to_oneoverk0)(handle, frame_id, scans.as_ptr(), out.as_mut_ptr(), count)
    };
    unsafe { (api.tims_close)(handle) };
    if ok == 0 {
        bail!("tims_scannum_to_oneoverk0 failed: {}", api.last_error_tims());
    }
    out.truncate(n);
    Ok(out)
}

/// Diagnostic: the SDK's m/z `(min, max, n)` for one frame, via `tims_index_to_mz`. A garbage/huge
/// m/z here would explode the chunked layout into millions of empty m/z chunks — the suspected cause
/// of the 21 GB allocation.
pub fn frame_mz_minmax(input: &Path, frame_idx: usize) -> Result<(f64, f64, usize)> {
    let r = TdfSdkReader::open(input)?;
    let frame = &r.frames[frame_idx.min(r.frames.len().saturating_sub(1))];
    let peaks = r.read_frame_peaks(frame)?;
    let indices: Vec<f64> = peaks.iter().map(|p| p.index as f64).collect();
    let mz = r.convert(r.api.tims_index_to_mz, frame.id, &indices, "tims_index_to_mz")?;
    let mn = mz.iter().copied().fold(f64::INFINITY, f64::min);
    let mx = mz.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    Ok((mn, mx, mz.len()))
}

/// Diagnostic: number of points the SDK's `tims_read_scans_v2` returns for the first `n_frames`
/// frames — to compare against timsrust's raw frame read (do both decode the same stored peaks?).
pub fn frame_point_counts(input: &Path, n_frames: usize) -> Result<Vec<usize>> {
    let r = TdfSdkReader::open(input)?;
    let k = n_frames.min(r.frames.len());
    let mut out = Vec::with_capacity(k);
    for i in 0..k {
        out.push(r.read_frame_peaks(&r.frames[i])?.len());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ms_level_mapping() {
        assert_eq!(msms_type_to_ms_level(0), 1); // MS1
        assert_eq!(msms_type_to_ms_level(8), 2); // DDA-PASEF
        assert_eq!(msms_type_to_ms_level(9), 2); // DIA-PASEF
        assert_eq!(msms_type_to_ms_level(2), 2); // TSF MS/MS
    }

    #[test]
    fn polarity_mapping() {
        assert_eq!(polarity_from_str("+"), ScanPolarity::Positive);
        assert_eq!(polarity_from_str("-"), ScanPolarity::Negative);
        assert_eq!(polarity_from_str(""), ScanPolarity::Unknown);
    }

    #[test]
    fn library_candidates_cover_layouts() {
        let c = timsdata_candidates_from_root(Path::new("/sdk"));
        assert!(c.contains(&PathBuf::from("/sdk/libtimsdata.so")));
        assert!(c.contains(&PathBuf::from("/sdk/win64/timsdata.dll")));
        assert!(c.contains(&PathBuf::from("/sdk/linux64/libtimsdata.so")));
    }

    #[test]
    fn decode_two_scans() {
        // 2 scans: scan0 has 2 peaks, scan1 has 1 peak.
        // header: [2, 1]; scan0 indices [10,11] intensities [100,101]; scan1 index [20] intensity [200].
        let buf = [2u32, 1, 10, 11, 100, 101, 20, 200];
        let peaks = decode_scan_buffer(&buf, 2).unwrap();
        assert_eq!(
            peaks,
            vec![
                RawPeak { index: 10, intensity: 100, scan: 0 },
                RawPeak { index: 11, intensity: 101, scan: 0 },
                RawPeak { index: 20, intensity: 200, scan: 1 },
            ]
        );
    }

    #[test]
    fn decode_rejects_truncated_buffer() {
        // header claims scan0 has 5 peaks but the buffer is too short.
        let buf = [5u32, 0, 1, 2];
        assert!(decode_scan_buffer(&buf, 2).is_err());
    }

    #[test]
    fn decode_empty_frame() {
        // 3 scans, all empty.
        let buf = [0u32, 0, 0];
        assert!(decode_scan_buffer(&buf, 3).unwrap().is_empty());
    }
}
