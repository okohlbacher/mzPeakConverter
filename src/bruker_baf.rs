//! Bruker BAF (otof / Q-TOF line + profile) reader → mzdata spectra.
//!
//! Ported from BRFP `src/baf.rs`, but re-targeted onto the same mzdata
//! `BinaryArrayMap`/`DataArray`/`SpectrumDescription` pattern that
//! [`crate::bruker_tsf`] uses, and onto `anyhow` for errors.
//!
//! BAF is a vendor-closed format: peak arrays live behind the `baf2sql_c`
//! shared library (`libbaf2sql_c.so` on Linux, `baf2sql_c.dll` on Windows —
//! there is no macOS build). The library, given the `.baf` path, materializes a
//! SQLite cache (`analysis.sqlite`) next to it and hands back integer array IDs;
//! the actual m/z + intensity doubles are then read out of the storage handle
//! via FFI. We:
//!   * dynamically load `baf2sql_c` (via `libloading`),
//!   * ask it for the SQLite cache path and open that read-only with `rusqlite`,
//!   * read the `Spectra`/`AcquisitionKeys` tables for rt / MS level / polarity /
//!     array IDs,
//!   * pull the calibrated (or raw) double arrays through the FFI per spectrum,
//!   * and emit one [`MultiLayerSpectrum`] per spectrum.
//!
//! The entire module is gated behind the `bruker_sdk` cargo feature by the
//! caller (`#[cfg(feature = "bruker_sdk")] mod bruker_baf;`), since it depends
//! on `libloading` and only makes sense where the vendor SDK exists.

use std::env;
use std::ffi::{c_char, c_int, CString};
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

/// Hard cap on how many doubles a single BAF array may report. Guards against a
/// corrupt cache / hostile library returning an enormous count that would
/// exhaust memory before we ever read it.
const MAX_BAF_ARRAY_ELEMENTS: u64 = 100_000_000;
/// Hard cap on the transient memory a single spectrum may use while being read:
/// the m/z array (`f64`) plus the intensity array (`f64`) together. Guards
/// against a corrupt/hostile cache reporting two individually-allowed but jointly
/// enormous arrays. 100M elements * 8 bytes * 2 arrays = 1.6 GiB; we cap a touch
/// above that so the element cap is the binding limit for honest data.
const MAX_BAF_SPECTRUM_BYTES: u64 = 2 * MAX_BAF_ARRAY_ELEMENTS * (std::mem::size_of::<f64>() as u64);
/// Upper bound on the SQLite-cache path buffer the SDK may request (a filesystem
/// path is far below this); guards against a buggy/hostile library returning a
/// huge size that would exhaust memory.
const MAX_BAF_PATH_BUFFER_BYTES: u32 = 1 << 20; // 1 MiB
/// Upper bound on the SDK error-message buffer; the error path clamps rather
/// than erroring so reporting cannot itself trigger a huge allocation.
const MAX_BAF_ERROR_BUFFER_BYTES: usize = 64 * 1024;

// --- baf2sql_c C ABI -------------------------------------------------------

type GetSqliteCacheFilename = unsafe extern "C" fn(*mut c_char, u32, *const c_char, c_int) -> u32;
type ArrayOpenStorage = unsafe extern "C" fn(c_int, *const c_char) -> u64;
type ArrayCloseStorage = unsafe extern "C" fn(u64);
type GetLastErrorString = unsafe extern "C" fn(*mut c_char, u32) -> u32;
type ArrayGetNumElements = unsafe extern "C" fn(u64, u64, *mut u64) -> c_int;
type ArrayReadDouble = unsafe extern "C" fn(u64, u64, *mut f64) -> c_int;

/// Resolved + bound function pointers into a loaded `baf2sql_c` library.
#[derive(Clone)]
struct Baf2SqlApi {
    _library: Arc<Library>,
    get_sqlite_cache_filename: GetSqliteCacheFilename,
    array_open_storage: ArrayOpenStorage,
    array_close_storage: ArrayCloseStorage,
    get_last_error_string: GetLastErrorString,
    array_get_num_elements: ArrayGetNumElements,
    array_read_double: ArrayReadDouble,
    library_path: PathBuf,
}

impl Baf2SqlApi {
    fn load(sdk_lib: Option<&Path>) -> Result<Self> {
        let library_path = discover_baf2sql_library(sdk_lib)?;
        // SAFETY: The path is user-controlled but loaded as a dynamic library
        // only after discovery. Symbols are looked up immediately and copied as
        // C function pointers while the Library is kept alive by Arc.
        let library = Arc::new(unsafe { Library::new(&library_path) }.with_context(|| {
            format!("failed to load BAF SDK library {}", library_path.display())
        })?);

        // SAFETY: Symbol names and signatures match the public baf2sql_c C API
        // used by Bruker examples and tdf2mzml. If a vendor library is
        // incompatible, symbol lookup fails before any calls are made.
        unsafe {
            let get_sqlite_cache_filename = *library
                .get::<GetSqliteCacheFilename>(b"baf2sql_get_sqlite_cache_filename_v2\0")
                .map_err(|e| missing_symbol(&library_path, e))?;
            let array_open_storage = *library
                .get::<ArrayOpenStorage>(b"baf2sql_array_open_storage\0")
                .map_err(|e| missing_symbol(&library_path, e))?;
            let array_close_storage = *library
                .get::<ArrayCloseStorage>(b"baf2sql_array_close_storage\0")
                .map_err(|e| missing_symbol(&library_path, e))?;
            let get_last_error_string = *library
                .get::<GetLastErrorString>(b"baf2sql_get_last_error_string\0")
                .map_err(|e| missing_symbol(&library_path, e))?;
            let array_get_num_elements = *library
                .get::<ArrayGetNumElements>(b"baf2sql_array_get_num_elements\0")
                .map_err(|e| missing_symbol(&library_path, e))?;
            let array_read_double = *library
                .get::<ArrayReadDouble>(b"baf2sql_array_read_double\0")
                .map_err(|e| missing_symbol(&library_path, e))?;

            Ok(Self {
                _library: library,
                get_sqlite_cache_filename,
                array_open_storage,
                array_close_storage,
                get_last_error_string,
                array_get_num_elements,
                array_read_double,
                library_path,
            })
        }
    }

    /// Ask the SDK for the path of the SQLite cache it maintains for `baf_file`,
    /// materializing it if necessary. Uses the SDK two-call (size, then fill)
    /// pattern.
    fn sqlite_cache_path(&self, baf_file: &Path) -> Result<PathBuf> {
        let baf_cstring = path_to_cstring(baf_file)?;
        // SAFETY: Passing null buffer follows the SDK two-call pattern. The baf
        // path CString remains alive for the duration of the call.
        let required = unsafe {
            (self.get_sqlite_cache_filename)(std::ptr::null_mut(), 0, baf_cstring.as_ptr(), 0)
        };
        if required == 0 {
            bail!(
                "baf2sql_get_sqlite_cache_filename_v2 failed: {}",
                self.last_error()
            );
        }
        if required > MAX_BAF_PATH_BUFFER_BYTES {
            bail!(
                "baf2sql_get_sqlite_cache_filename_v2 requested an implausibly large path buffer \
                 ({required} bytes, limit {MAX_BAF_PATH_BUFFER_BYTES}); refusing to allocate"
            );
        }

        let mut buffer = vec![0u8; required as usize];
        // SAFETY: Buffer has the size requested by the SDK and is writable.
        let written = unsafe {
            (self.get_sqlite_cache_filename)(
                buffer.as_mut_ptr().cast::<c_char>(),
                required,
                baf_cstring.as_ptr(),
                0,
            )
        };
        if written == 0 {
            bail!(
                "baf2sql_get_sqlite_cache_filename_v2 failed: {}",
                self.last_error()
            );
        }

        let nul = buffer.iter().position(|b| *b == 0).unwrap_or(buffer.len());
        let path = String::from_utf8_lossy(&buffer[..nul]).to_string();
        Ok(PathBuf::from(path))
    }

    /// Open a calibrated (raw_flag = 0) or raw (raw_flag = 1) storage handle.
    fn open_storage(&self, baf_file: &Path, calibration: BafCalibrationMode) -> Result<BafStorage> {
        let baf_cstring = path_to_cstring(baf_file)?;
        let raw_flag: c_int = match calibration {
            BafCalibrationMode::Raw => 1,
            BafCalibrationMode::Vendor | BafCalibrationMode::Auto => 0,
        };
        // SAFETY: The C string remains alive for the call. A zero handle is
        // treated as an SDK error and not wrapped.
        let handle = unsafe { (self.array_open_storage)(raw_flag, baf_cstring.as_ptr()) };
        if handle == 0 {
            bail!(
                "baf2sql_array_open_storage failed for {} calibration: {}",
                calibration.as_str(),
                self.last_error()
            );
        }
        Ok(BafStorage {
            api: self.clone(),
            handle,
            calibration_used: calibration,
        })
    }

    fn last_error(&self) -> String {
        // SAFETY: Null/0 call follows SDK two-call pattern.
        let required = (unsafe { (self.get_last_error_string)(std::ptr::null_mut(), 0) }.max(1)
            as usize)
            .min(MAX_BAF_ERROR_BUFFER_BYTES);
        let mut buffer = vec![0u8; required];
        // SAFETY: Buffer is valid and writable for the requested size.
        unsafe {
            (self.get_last_error_string)(buffer.as_mut_ptr().cast::<c_char>(), required as u32);
        }
        let nul = buffer.iter().position(|b| *b == 0).unwrap_or(buffer.len());
        String::from_utf8_lossy(&buffer[..nul]).to_string()
    }
}

fn missing_symbol(path: &Path, error: libloading::Error) -> anyhow::Error {
    anyhow!(
        "BAF SDK library {} is missing a required symbol: {error}",
        path.display()
    )
}

// --- calibration mode ------------------------------------------------------

/// How to interpret BAF arrays: vendor-calibrated m/z, raw arrays, or
/// auto (try calibrated, fall back to raw).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BafCalibrationMode {
    /// Try vendor-calibrated arrays; fall back to raw if that fails.
    #[default]
    Auto,
    /// Force vendor-calibrated arrays.
    Vendor,
    /// Force raw (uncalibrated) arrays.
    Raw,
}

impl BafCalibrationMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Vendor => "vendor",
            Self::Raw => "raw",
        }
    }
}

// --- storage handle (FFI array reads) --------------------------------------

struct BafStorage {
    api: Baf2SqlApi,
    handle: u64,
    calibration_used: BafCalibrationMode,
}

impl BafStorage {
    /// Query the SDK for the element count of a single BAF array, validated
    /// against the per-array element cap. Does not allocate or read.
    fn array_element_count(&self, array_id: u64) -> Result<u64> {
        let mut count = 0u64;
        // SAFETY: The storage handle is valid while self is alive. The SDK
        // writes one u64 count to the provided pointer.
        let ok = unsafe {
            (self.api.array_get_num_elements)(self.handle, array_id, &mut count as *mut u64)
        };
        if ok == 0 {
            bail!(
                "baf2sql_array_get_num_elements failed for array {array_id}: {}",
                self.api.last_error()
            );
        }
        if count > MAX_BAF_ARRAY_ELEMENTS {
            bail!(
                "BAF array {array_id} reports {count} elements, exceeding safety limit \
                 {MAX_BAF_ARRAY_ELEMENTS}"
            );
        }
        Ok(count)
    }

    /// Read one BAF array (m/z or intensity) as `f64`s, given an element count
    /// that was already fetched and validated (see [`Self::read_pair`]).
    ///
    /// SAFETY / SDK INVARIANT: `baf2sql_array_read_double` writes exactly the
    /// number of doubles the SDK reports for `array_id`; it does NOT take a
    /// caller-supplied capacity, so the destination buffer MUST be sized to that
    /// count or the C side writes out of bounds (undefined behavior). Rust cannot
    /// enforce this invariant — the count and the read are two separate FFI calls
    /// and the SDK could in principle return a different count between them. We
    /// re-fetch the count immediately before the read and require it to still
    /// equal `expected_count`, which is the strongest check the API allows.
    fn read_array_double(&self, array_id: u64, expected_count: u64) -> Result<Vec<f64>> {
        if expected_count == 0 {
            return Ok(Vec::new());
        }
        // Re-check the count immediately before allocating + reading: the buffer
        // must match whatever the SDK will actually write right now, not what it
        // reported earlier.
        let count = self.array_element_count(array_id)?;
        if count != expected_count {
            bail!(
                "BAF array {array_id} element count changed between probe ({expected_count}) and \
                 read ({count}); refusing to read into a mismatched buffer"
            );
        }
        let len = usize::try_from(count).map_err(|_| {
            anyhow!("BAF array {array_id} has too many elements for this platform: {count}")
        })?;
        let mut values = vec![0.0f64; len];
        // SAFETY: values has len f64 elements, matching the count the SDK just
        // reported for this array ID (re-checked above). See the SDK INVARIANT
        // note on this method.
        let ok =
            unsafe { (self.api.array_read_double)(self.handle, array_id, values.as_mut_ptr()) };
        if ok == 0 {
            bail!(
                "baf2sql_array_read_double failed for array {array_id}: {}",
                self.api.last_error()
            );
        }
        Ok(values)
    }

    /// Read a matched `(m/z, intensity)` pair. Fetches BOTH element counts FIRST,
    /// requires them to be equal, enforces a joint per-spectrum byte budget, and
    /// only THEN allocates and reads each array. This ordering is the finding-1
    /// guard: we never allocate a huge buffer for one array before learning the
    /// other array disagrees or the pair blows the budget.
    fn read_pair(&self, mz_id: u64, intensity_id: u64) -> Result<(Vec<f64>, Vec<f64>)> {
        let mz_count = self.array_element_count(mz_id)?;
        let intensity_count = self.array_element_count(intensity_id)?;
        if mz_count != intensity_count {
            bail!(
                "BAF m/z array {mz_id} reports {mz_count} elements but intensity array \
                 {intensity_id} reports {intensity_count}; the cache pair is inconsistent"
            );
        }
        // Joint byte budget over both f64 arrays (finding 9): cap transient
        // memory by total bytes, not just element count.
        let pair_bytes = mz_count
            .checked_add(intensity_count)
            .and_then(|elements| elements.checked_mul(std::mem::size_of::<f64>() as u64))
            .ok_or_else(|| {
                anyhow!("BAF pair (m/z {mz_id}, intensity {intensity_id}) element count overflows")
            })?;
        if pair_bytes > MAX_BAF_SPECTRUM_BYTES {
            bail!(
                "BAF pair (m/z {mz_id}, intensity {intensity_id}) needs {pair_bytes} bytes, \
                 exceeding per-spectrum limit {MAX_BAF_SPECTRUM_BYTES}"
            );
        }
        let mz_values = self.read_array_double(mz_id, mz_count)?;
        let intensities = self.read_array_double(intensity_id, intensity_count)?;
        Ok((mz_values, intensities))
    }
}

impl Drop for BafStorage {
    fn drop(&mut self) {
        if self.handle != 0 {
            // SAFETY: The handle was returned by baf2sql_array_open_storage and
            // is closed exactly once here.
            unsafe {
                (self.api.array_close_storage)(self.handle);
            }
            self.handle = 0;
        }
    }
}

/// Find the first row that has a complete `(m/z, intensity)` pair for the
/// selected continuity, to use as a probe target when validating a calibrated
/// handle (finding 6). Returns the ids of a pair the SDK should be able to read.
fn first_readable_pair(rows: &[BafSpectrumRow], prefer_profile: bool) -> Option<(u64, u64)> {
    rows.iter()
        .find_map(|row| match BafReader::select_pair(row, prefer_profile) {
            (Some(mz_id), Some(intensity_id), _) => Some((mz_id, intensity_id)),
            _ => None,
        })
}

/// Probe a freshly opened storage handle by actually reading one real non-empty
/// pair. Returns Ok only if the read succeeds, so the caller can commit to this
/// calibration mode (finding 6: don't trust a calibrated handle that opens but
/// then fails on the first real read).
fn probe_storage(storage: &BafStorage, rows: &[BafSpectrumRow], prefer_profile: bool) -> Result<()> {
    let Some((mz_id, intensity_id)) = first_readable_pair(rows, prefer_profile) else {
        // No non-empty pair to probe; nothing to validate, treat as fine.
        return Ok(());
    };
    storage.read_pair(mz_id, intensity_id).map(|_| ())
}

fn open_storage_with_fallback(
    api: &Baf2SqlApi,
    baf_file: &Path,
    calibration: BafCalibrationMode,
    rows: &[BafSpectrumRow],
    prefer_profile: bool,
) -> Result<BafStorage> {
    match calibration {
        BafCalibrationMode::Raw => api.open_storage(baf_file, BafCalibrationMode::Raw),
        BafCalibrationMode::Vendor => api.open_storage(baf_file, BafCalibrationMode::Vendor),
        // Finding 6: in auto mode the calibrated handle can open successfully but
        // fail on the first real array read. Probe a real non-empty pair before
        // committing; if either the open or the probe read fails, fall back to a
        // raw handle (which is likewise probed by the convert path's reads).
        BafCalibrationMode::Auto => {
            let calibrated = api
                .open_storage(baf_file, BafCalibrationMode::Vendor)
                .and_then(|storage| {
                    probe_storage(&storage, rows, prefer_profile).map(|()| storage)
                });
            match calibrated {
                Ok(storage) => Ok(storage),
                Err(calibrated_error) => api
                    .open_storage(baf_file, BafCalibrationMode::Raw)
                    .with_context(|| {
                        format!(
                            "BAF array access failed in auto calibration mode; calibrated error: \
                             {calibrated_error}; raw fallback also failed"
                        )
                    }),
            }
        }
    }
}

// --- SQLite metadata rows --------------------------------------------------

/// A raw `Spectra ⋈ AcquisitionKeys` row before validation: array ids are still
/// signed/optional SQLite cells and the acquisition-key fields are `Option`
/// because the LEFT JOIN leaves them NULL when the key is missing.
#[derive(Debug, Clone)]
struct BafSpectrumRowRaw {
    id: i64,
    retention_time_seconds: f64,
    line_mz_id: Option<i64>,
    line_intensity_id: Option<i64>,
    profile_mz_id: Option<i64>,
    profile_intensity_id: Option<i64>,
    ms_level: Option<i64>,
    polarity: Option<i64>,
}

#[derive(Debug, Clone)]
struct BafSpectrumRow {
    id: i64,
    retention_time_seconds: f64,
    line_mz_id: Option<u64>,
    line_intensity_id: Option<u64>,
    profile_mz_id: Option<u64>,
    profile_intensity_id: Option<u64>,
    /// 0-based vendor MS level (0 == MS1).
    ms_level: i64,
    /// Vendor polarity code (0 == positive, 1 == negative).
    polarity: i64,
}

fn read_spectrum_rows(connection: &Connection) -> Result<Vec<BafSpectrumRow>> {
    // Finding 5: count the Spectra table up front so we can detect rows the
    // join would otherwise silently drop (missing AcquisitionKey). We use a
    // LEFT JOIN below and bail if any spectrum lost its acquisition key.
    let spectra_total: i64 = connection
        .query_row("SELECT COUNT(*) FROM Spectra", [], |row| row.get(0))
        .context("counting BAF Spectra rows")?;

    let mut stmt = connection
        .prepare(
            "SELECT s.Id, s.Rt, s.LineMzId, s.LineIntensityId, \
             s.ProfileMzId, s.ProfileIntensityId, ak.MsLevel, ak.Polarity \
             FROM Spectra s LEFT JOIN AcquisitionKeys ak ON s.AcquisitionKey = ak.Id \
             ORDER BY s.Id",
        )
        .context("preparing BAF Spectra query")?;
    let rows = stmt
        .query_map([], |row| {
            let id: i64 = row.get(0)?;
            // ms_level / polarity come from the LEFT-joined AcquisitionKeys row
            // and are NULL when the key is missing; carry that through as Option
            // so the post-pass can name the offending spectrum.
            Ok(BafSpectrumRowRaw {
                id,
                retention_time_seconds: row.get::<_, Option<f64>>(1)?.unwrap_or(0.0),
                line_mz_id: row.get::<_, Option<i64>>(2)?,
                line_intensity_id: row.get::<_, Option<i64>>(3)?,
                profile_mz_id: row.get::<_, Option<i64>>(4)?,
                profile_intensity_id: row.get::<_, Option<i64>>(5)?,
                ms_level: row.get::<_, Option<i64>>(6)?,
                polarity: row.get::<_, Option<i64>>(7)?,
            })
        })
        .context("querying BAF Spectra")?;

    let raw_rows = rows
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("reading BAF Spectra rows")?;

    // Finding 5: the LEFT JOIN keeps every Spectra row, so a count mismatch here
    // would indicate a deeper cache problem rather than a dropped key.
    if (raw_rows.len() as i64) != spectra_total {
        bail!(
            "BAF Spectra query returned {} rows but the Spectra table has {}; \
             the cache may be corrupt",
            raw_rows.len(),
            spectra_total
        );
    }

    raw_rows
        .into_iter()
        .map(|raw| {
            // Finding 5: a missing AcquisitionKey is corrupt metadata; name the
            // spectrum rather than silently dropping it (as an INNER JOIN would).
            let ms_level = raw.ms_level.ok_or_else(|| {
                anyhow!(
                    "BAF spectrum {} has no matching AcquisitionKeys row (missing MsLevel)",
                    raw.id
                )
            })?;
            let polarity = raw.polarity.ok_or_else(|| {
                anyhow!(
                    "BAF spectrum {} has no matching AcquisitionKeys row (missing Polarity)",
                    raw.id
                )
            })?;
            Ok(BafSpectrumRow {
                id: raw.id,
                retention_time_seconds: raw.retention_time_seconds,
                line_mz_id: sql_array_id(raw.line_mz_id, raw.id, "LineMzId")?,
                line_intensity_id: sql_array_id(raw.line_intensity_id, raw.id, "LineIntensityId")?,
                profile_mz_id: sql_array_id(raw.profile_mz_id, raw.id, "ProfileMzId")?,
                profile_intensity_id: sql_array_id(
                    raw.profile_intensity_id,
                    raw.id,
                    "ProfileIntensityId",
                )?,
                ms_level,
                polarity,
            })
        })
        .collect()
}

/// Convert a SQLite array-id cell to a positive storage id.
///
/// `NULL` legitimately means "this spectrum has no array of this kind" and maps
/// to `None`. A *present but non-positive* id is a corruption signal in the cache
/// (storage ids are positive); finding 4 requires we bail rather than silently
/// map it to `None`, so a stale/damaged cache cannot masquerade as empty data.
fn sql_array_id(value: Option<i64>, spectrum_id: i64, field: &str) -> Result<Option<u64>> {
    match value {
        None => Ok(None),
        Some(raw) => match u64::try_from(raw) {
            Ok(0) => bail!(
                "BAF spectrum {spectrum_id} has a zero {field} array id; corrupt cache metadata"
            ),
            Ok(id) => Ok(Some(id)),
            Err(_) => bail!(
                "BAF spectrum {spectrum_id} has a negative {field} array id ({raw}); \
                 corrupt cache metadata"
            ),
        },
    }
}

/// Map the 0-based vendor MS level onto the public 1-based level.
fn checked_baf_ms_level_to_public(value: i64) -> Result<u8> {
    if value < 0 {
        bail!("BAF AcquisitionKeys.MsLevel is negative ({value})");
    }
    if value == 0 {
        return Ok(1);
    }
    // Finding 7: use checked_add so a corrupt MsLevel near i64::MAX cannot
    // overflow before the u8 cast.
    value
        .checked_add(1)
        .and_then(|level| u8::try_from(level).ok())
        .ok_or_else(|| anyhow!("BAF AcquisitionKeys.MsLevel is too large ({value})"))
}

fn baf_polarity(value: i64) -> ScanPolarity {
    match value {
        0 => ScanPolarity::Positive,
        1 => ScanPolarity::Negative,
        _ => ScanPolarity::Unknown,
    }
}

// --- path resolution + SDK discovery ---------------------------------------

struct BafPaths {
    baf_file: PathBuf,
}

impl BafPaths {
    fn resolve(path: &Path) -> Result<Self> {
        if path.file_name().and_then(|v| v.to_str()) == Some("analysis.baf") && path.is_file() {
            return Ok(Self {
                baf_file: path.to_path_buf(),
            });
        }
        let baf_file = path.join("analysis.baf");
        if !baf_file.is_file() {
            bail!("analysis.baf not found in {}", path.display());
        }
        Ok(Self { baf_file })
    }
}

/// Discover the `baf2sql_c` shared library: explicit arg first, then
/// `TIMSDATA_LIB_DIR`, treating either as a file or an SDK-root directory
/// (`win64/`, `linux64/`, or flat).
fn discover_baf2sql_library(sdk_lib: Option<&Path>) -> Result<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(explicit) = sdk_lib {
        candidates.extend(baf_library_candidates_from_root(explicit));
    }
    if let Some(root) = env::var_os("TIMSDATA_LIB_DIR").map(PathBuf::from) {
        candidates.extend(baf_library_candidates_from_root(&root));
    }

    for candidate in candidates {
        if candidate.is_file() {
            return Ok(candidate);
        }
    }

    bail!(
        "BAF conversion requires libbaf2sql_c.so on Linux or baf2sql_c.dll on Windows; \
         pass an explicit --sdk-lib path or set TIMSDATA_LIB_DIR"
    )
}

/// Expand an SDK path (file or root directory) into the library file candidates
/// to probe, covering the flat, `linux64/`, and `win64/` layouts.
fn baf_library_candidates_from_root(root: &Path) -> Vec<PathBuf> {
    if root.is_file() {
        return vec![root.to_path_buf()];
    }
    let mut candidates = Vec::new();
    for name in [
        "libbaf2sql_c.so",
        "baf2sql_c.dll",
        "linux64/libbaf2sql_c.so",
        "win64/baf2sql_c.dll",
    ] {
        candidates.push(root.join(name));
    }
    // If the path already points at a platform subdir, also probe its sibling.
    if let Some(parent) = root.parent() {
        match root.file_name().and_then(|n| n.to_str()) {
            Some("linux64") => candidates.push(parent.join("linux64/libbaf2sql_c.so")),
            Some("win64") => candidates.push(parent.join("win64/baf2sql_c.dll")),
            _ => {}
        }
    }
    candidates
}

fn path_to_cstring(path: &Path) -> Result<CString> {
    // Finding 8: on Unix, build the CString from the path's raw bytes so
    // non-UTF8 paths survive intact (to_string_lossy would corrupt them by
    // substituting U+FFFD). On Windows there is no byte-accurate OsStr view, so
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

// --- public reader ---------------------------------------------------------

/// A Bruker BAF `.d` reader yielding one [`MultiLayerSpectrum`] per spectrum,
/// built the same way [`crate::bruker_tsf::TsfReader`] builds its spectra.
pub struct BafReader {
    baf_file: PathBuf,
    rows: Vec<BafSpectrumRow>,
    storage: BafStorage,
    /// When true, read profile arrays for spectra that have them; otherwise
    /// default to line arrays (finding 2). Currently always false (line-first);
    /// kept as a field so a profile-requesting entry point is a one-liner.
    prefer_profile: bool,
    /// Finding 10: the baf2sql_c storage handle is not known to be thread-safe,
    /// and FFI calls through it must not happen concurrently. A raw-pointer
    /// marker makes `BafReader` neither `Send` nor `Sync`, so the type system
    /// prevents it from being shared across threads. This is sound for the
    /// existing single-threaded convert path.
    _not_thread_safe: PhantomData<*const ()>,
}

impl BafReader {
    /// Open a BAF `.d` directory (or a direct `analysis.baf` path). `sdk_lib`
    /// optionally points at the `baf2sql_c` library or an SDK root; otherwise
    /// `TIMSDATA_LIB_DIR` is consulted. Uses auto calibration (calibrated, with
    /// raw fallback).
    pub fn open(dot_d: &Path, sdk_lib: Option<&Path>) -> Result<Self> {
        let paths = BafPaths::resolve(dot_d)?;
        let api = Baf2SqlApi::load(sdk_lib)?;
        let sqlite_cache = api.sqlite_cache_path(&paths.baf_file)?;
        let connection = Connection::open_with_flags(
            &sqlite_cache,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("opening BAF SQLite cache {}", sqlite_cache.display()))?;
        let rows = read_spectrum_rows(&connection)?;
        // The connection was only needed to read the metadata rows up front.
        drop(connection);
        // Default to line-first (finding 2).
        let prefer_profile = false;
        let storage = open_storage_with_fallback(
            &api,
            &paths.baf_file,
            BafCalibrationMode::Auto,
            &rows,
            prefer_profile,
        )?;

        Ok(Self {
            baf_file: paths.baf_file,
            rows,
            storage,
            prefer_profile,
            _not_thread_safe: PhantomData,
        })
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn baf_file(&self) -> &Path {
        &self.baf_file
    }

    /// Choose which `(m/z id, intensity id)` pair and continuity to read for a
    /// row.
    ///
    /// Finding 2: default to the LINE (centroid) arrays, matching BRFP's
    /// "line-first unless profile requested" policy — profile arrays can be
    /// orders of magnitude larger and reading them by default can explode memory.
    /// Profile is selected only when explicitly preferred AND present.
    ///
    /// Finding 3: a pair is valid only if BOTH ids are present (→ read) or BOTH
    /// absent (→ empty spectrum). Exactly one present is corruption and the
    /// caller bails naming the spectrum.
    fn select_pair(
        row: &BafSpectrumRow,
        prefer_profile: bool,
    ) -> (Option<u64>, Option<u64>, SignalContinuity) {
        if prefer_profile && row.profile_mz_id.is_some() && row.profile_intensity_id.is_some() {
            (
                row.profile_mz_id,
                row.profile_intensity_id,
                SignalContinuity::Profile,
            )
        } else {
            (
                row.line_mz_id,
                row.line_intensity_id,
                SignalContinuity::Centroid,
            )
        }
    }

    /// Read the `(m/z, intensity)` peak arrays for one row, defaulting to the line
    /// arrays (see [`Self::select_pair`]). Returns the continuity used so the
    /// spectrum can be tagged correctly.
    fn peaks(&self, row: &BafSpectrumRow) -> Result<(Vec<f64>, Vec<f64>, SignalContinuity)> {
        let (mz_id, intensity_id, continuity) = Self::select_pair(row, self.prefer_profile);

        match (mz_id, intensity_id) {
            // Both absent: a legitimately empty spectrum.
            (None, None) => Ok((Vec::new(), Vec::new(), continuity)),
            // Both present: read the matched pair (counts checked inside).
            (Some(mz_id), Some(intensity_id)) => {
                let (mz_values, intensities) = self.storage.read_pair(mz_id, intensity_id)?;
                Ok((mz_values, intensities, continuity))
            }
            // Finding 3: exactly one present is corruption; bail naming the
            // spectrum and the missing field rather than emitting wrong data.
            (Some(_), None) => bail!(
                "BAF spectrum {} has an m/z array but no matching intensity array id",
                row.id
            ),
            (None, Some(_)) => bail!(
                "BAF spectrum {} has an intensity array but no matching m/z array id",
                row.id
            ),
        }
    }

    /// Build the mzdata spectrum for spectrum `i` (0-based reader order).
    pub fn spectrum(&self, i: usize) -> Result<MultiLayerSpectrum> {
        let row = self
            .rows
            .get(i)
            .with_context(|| format!("BAF spectrum index {i} out of range"))?;
        let ms_level = checked_baf_ms_level_to_public(row.ms_level)?;
        let (mz, intensity, continuity) = self.peaks(row)?;

        let mut arrays = BinaryArrayMap::new();
        let mut mz_da =
            DataArray::wrap(&ArrayType::MZArray, BinaryDataArrayType::Float64, Vec::new());
        mz_da
            .update_buffer(mz.as_slice())
            .map_err(|e| anyhow!("encoding m/z: {e}"))?;
        mz_da.unit = Unit::MZ;
        arrays.add(mz_da);
        // Intensity is stored as f32 (MS:1000521) to match the mzPeak writer's
        // standardized intensity schema; baf2sql hands back f64, so narrow it.
        let intensity_f32: Vec<f32> = intensity.iter().map(|&v| v as f32).collect();
        let mut int_da = DataArray::wrap(
            &ArrayType::IntensityArray,
            BinaryDataArrayType::Float32,
            Vec::new(),
        );
        int_da
            .update_buffer(intensity_f32.as_slice())
            .map_err(|e| anyhow!("encoding intensity: {e}"))?;
        int_da.unit = Unit::DetectorCounts;
        arrays.add(int_da);

        let mut descr = SpectrumDescription {
            id: format!("scan={}", row.id),
            index: i,
            ms_level,
            signal_continuity: continuity,
            polarity: baf_polarity(row.polarity),
            ..Default::default()
        };
        descr.add_param(
            Param::builder()
                .name("mass spectrum")
                .curie(curie!(MS:1000294))
                .build(),
        );
        let mut scan = ScanEvent::default();
        scan.start_time = row.retention_time_seconds / 60.0; // mzdata scan start_time is minutes
        descr.acquisition.scans.push(scan);

        Ok(MultiLayerSpectrum::new(descr, Some(arrays), None, None))
    }

    /// A sample spectrum's array map, for deriving the writer's data-facet
    /// schema (mirrors `TsfReader::sample_arrays`).
    pub fn sample_arrays(&self) -> Result<BinaryArrayMap> {
        let i = (0..self.len())
            .find(|&i| {
                matches!(
                    Self::select_pair(&self.rows[i], self.prefer_profile),
                    (Some(_), Some(_), _)
                )
            })
            .unwrap_or(0);
        self.spectrum(i)?
            .arrays
            .clone()
            .ok_or_else(|| anyhow!("sample spectrum has no arrays"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_baf_ms_levels_from_zero_based_cache_values() {
        assert_eq!(checked_baf_ms_level_to_public(0).unwrap(), 1);
        assert_eq!(checked_baf_ms_level_to_public(1).unwrap(), 2);
        assert_eq!(checked_baf_ms_level_to_public(2).unwrap(), 3);
        assert!(checked_baf_ms_level_to_public(-1).is_err());
    }

    #[test]
    fn builds_baf_library_candidates_from_plain_directory() {
        let candidates = baf_library_candidates_from_root(Path::new("/sdk"));
        assert!(candidates.contains(&PathBuf::from("/sdk/libbaf2sql_c.so")));
        assert!(candidates.contains(&PathBuf::from("/sdk/linux64/libbaf2sql_c.so")));
        assert!(candidates.contains(&PathBuf::from("/sdk/win64/baf2sql_c.dll")));
    }

    #[test]
    fn maps_polarity_codes() {
        assert_eq!(baf_polarity(0), ScanPolarity::Positive);
        assert_eq!(baf_polarity(1), ScanPolarity::Negative);
        assert_eq!(baf_polarity(7), ScanPolarity::Unknown);
    }
}
