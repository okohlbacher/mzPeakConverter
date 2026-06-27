//! Native Agilent MassHunter (`.d`) reader → mzdata spectra (PLAN §3.7), via an **out-of-process
//! .NET Framework 4.8 host** (`AgilentGlueHost.exe`, built from `glue/agilent/`).
//!
//! **Windows-runtime-only.** This file compiles on any platform behind the `agilent` cargo feature
//! (the module is `#[cfg(windows)]`, so in practice it only builds on Windows), but it can only run
//! on Windows with the Agilent MHDAC (MassHunter Data Access Component) DLLs present.
//!
//! ## Why a separate .NET Framework process (not in-process .NET via netcorehost)
//! MHDAC is a .NET **Framework 4.x** assembly set. Inside `MassSpecDataReader.OpenDataFile` it calls
//! the legacy `Delegate.BeginInvoke` async pattern, which is **permanently unsupported on .NET
//! Core / .NET 5+** (`PlatformNotSupportedException`, no opt-in flag). So MHDAC cannot be hosted in
//! an in-process .NET 8 runtime. Instead we shell out to a tiny net48 console EXE that reads the
//! `.d` via MHDAC (reflection-only, no compile-time reference) and writes the spectra to a temp
//! binary file we read back here. The whole stack — Rust + C# — still builds without the DLLs.
//!
//! ## How the pieces fit
//! ```text
//!   AgilentReader (this file, Rust)
//!        │  std::process::Command: AgilentGlueHost.exe <in.d> <mhdacDir> <out.bin>
//!        ▼
//!   AgilentGlueHost.exe (glue/agilent/Glue.cs, .NET Framework 4.8)
//!        │  System.Reflection → MHDAC MassSpecDataReader.OpenDataFile / GetSpectrum / GetScanRecord
//!        ▼
//!   MHDAC (Agilent's licensed DLLs, sourced from a ProteoWizard install)
//! ```
//!
//! ## Environment contract (resolved at `open`)
//!   * `MZPC_AGILENT_GLUE` — directory containing `AgilentGlueHost.exe` (the `dotnet build` output of
//!     `glue/agilent/`, i.e. `glue/agilent/bin/Release/net48`).
//!   * `MZPC_PWIZ_DIR` — a ProteoWizard install directory. The MHDAC DLLs are loaded from
//!     `<MZPC_PWIZ_DIR>/vendor_api/Agilent` (passed to the host as `<mhdacDir>`).
//!
//! ## Binary protocol (host → us), little-endian (see glue/agilent/Glue.cs):
//! ```text
//!   "AGL1" (4 bytes) | count u64 | offset[count] u64 (abs file offset of each record)
//!   then per record: rt f64 | msLevel i32 | polarity i32 | isCentroid i32 | scanId i32 |
//!                    nPoints u64 | mz[nPoints] f64 | intensity[nPoints] f64
//! ```
//!
//! ## Scope
//! Non-IM MS only (profile or centroid, MS1/MS2). Agilent ion-mobility (6560 IM-QTOF) needs the
//! separate **MIDAC** SDK — out of scope here (TODO in [`AgilentReader::spectrum`]).

use std::cell::RefCell;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow, bail};

use mzdata::curie;
use mzdata::params::{Param, Unit};
use mzdata::prelude::ParamDescribed;
use mzdata::spectrum::bindata::{ArrayType, BinaryArrayMap, BinaryDataArrayType, DataArray};
use mzdata::spectrum::{
    MultiLayerSpectrum, ScanEvent, ScanPolarity, SignalContinuity, SpectrumDescription,
};

const MAGIC: &[u8; 4] = b"AGL1";
const HOST_EXE: &str = "AgilentGlueHost.exe";

/// Per-record header (everything before the m/z + intensity arrays), as written by the host.
struct RecordHeader {
    rt_minutes: f64,
    ms_level: i32,
    polarity: i32,
    is_centroid: i32,
    scan_id: i32,
    n_points: u64,
}

/// A native Agilent `.d` reader. `open` runs the net48 host once to materialize a temp binary file
/// of all spectra; `spectrum(i)` seeks to record `i` and decodes it. The temp file is removed on
/// `Drop`.
pub struct AgilentReader {
    file: RefCell<File>,
    offsets: Vec<u64>,
    file_len: u64,
    tmp_path: PathBuf,
}

/// Unique temp filenames without pulling a `tempfile` dep: pid + a process-local counter. (Date/rand
/// are intentionally avoided — pid+counter is collision-free within this process.)
static TMP_CTR: AtomicU64 = AtomicU64::new(0);

fn read_exact_vec(f: &mut impl Read, n: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    f.read_exact(&mut buf).context("short read")?;
    Ok(buf)
}
fn read_u64(f: &mut impl Read) -> Result<u64> {
    let mut b = [0u8; 8];
    f.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}
fn read_i32(f: &mut impl Read) -> Result<i32> {
    let mut b = [0u8; 4];
    f.read_exact(&mut b)?;
    Ok(i32::from_le_bytes(b))
}
fn read_f64(f: &mut impl Read) -> Result<f64> {
    let mut b = [0u8; 8];
    f.read_exact(&mut b)?;
    Ok(f64::from_le_bytes(b))
}

impl AgilentReader {
    /// Open an Agilent `.d` directory: resolve the host EXE (`MZPC_AGILENT_GLUE`) and the MHDAC dir
    /// (`<MZPC_PWIZ_DIR>/vendor_api/Agilent`), run the host to write a temp binary, and load its index.
    pub fn open(path: &Path) -> Result<Self> {
        let glue_dir = std::env::var_os("MZPC_AGILENT_GLUE").map(PathBuf::from).ok_or_else(|| {
            anyhow!(
                "MZPC_AGILENT_GLUE not set — point it at the `dotnet build` output dir of \
                 glue/agilent/ (containing {HOST_EXE})"
            )
        })?;
        let host = glue_dir.join(HOST_EXE);
        if !host.exists() {
            bail!(
                "{HOST_EXE} not found in MZPC_AGILENT_GLUE dir {} — build glue/agilent/ \
                 (`dotnet build -c Release`) on this (Windows) box",
                glue_dir.display()
            );
        }
        let pwiz_dir = std::env::var_os("MZPC_PWIZ_DIR").map(PathBuf::from).ok_or_else(|| {
            anyhow!(
                "MZPC_PWIZ_DIR not set — point it at a ProteoWizard install; the Agilent MHDAC \
                 DLLs are loaded from <MZPC_PWIZ_DIR>/vendor_api/Agilent"
            )
        })?;
        let mhdac_dir = pwiz_dir.join("vendor_api").join("Agilent");

        let ctr = TMP_CTR.fetch_add(1, Ordering::Relaxed);
        let tmp_path =
            std::env::temp_dir().join(format!("mzpc-agilent-{}-{}.bin", std::process::id(), ctr));

        // Run the host. Capture stderr for diagnostics; stdout is reserved/empty.
        let out = Command::new(&host)
            .arg(path)
            .arg(&mhdac_dir)
            .arg(&tmp_path)
            .output()
            .with_context(|| format!("spawning {}", host.display()))?;
        if !out.status.success() {
            let _ = std::fs::remove_file(&tmp_path);
            let err = String::from_utf8_lossy(&out.stderr);
            let err = err.trim();
            bail!(
                "Agilent host failed to convert {} (exit {}): {}",
                path.display(),
                out.status.code().map(|c| c.to_string()).unwrap_or_else(|| "signal".into()),
                if err.is_empty() { "<no stderr>" } else { err }
            );
        }

        let mut file = File::open(&tmp_path)
            .with_context(|| format!("opening host output {}", tmp_path.display()))?;
        let mut magic = [0u8; 4];
        file.read_exact(&mut magic).context("reading magic")?;
        if &magic != MAGIC {
            let _ = std::fs::remove_file(&tmp_path);
            bail!("Agilent host output {} has bad magic {magic:?}", tmp_path.display());
        }
        let file_len = file.metadata().context("stat host output")?.len();
        let count = read_u64(&mut file).context("reading count")? as usize;
        // Guard a corrupt/truncated host file: the offset table (count×8) must fit, and count×8 must
        // not overflow. (With the host's atomic .part publish this should never trip, but never
        // allocate or index off an unchecked on-disk length.)
        let table_bytes = count
            .checked_mul(8)
            .filter(|b| *b as u64 <= file_len.saturating_sub(12))
            .ok_or_else(|| {
                anyhow!("Agilent host output declares {count} records, too many for its {file_len} bytes")
            })?;
        let offsets_bytes = read_exact_vec(&mut file, table_bytes).context("reading offset table")?;
        let offsets: Vec<u64> =
            offsets_bytes.chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect();

        Ok(Self { file: RefCell::new(file), offsets, file_len, tmp_path })
    }

    pub fn len(&self) -> usize {
        self.offsets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.offsets.is_empty()
    }

    /// Seek to record `i` and decode its header + the m/z (f64) and intensity (f64→f32) arrays.
    fn fetch(&self, i: usize) -> Result<(Vec<f64>, Vec<f32>, RecordHeader)> {
        let off = *self
            .offsets
            .get(i)
            .ok_or_else(|| anyhow!("Agilent spectrum index {i} out of range (count {})", self.len()))?;
        let mut f = self.file.borrow_mut();
        f.seek(SeekFrom::Start(off)).with_context(|| format!("seeking to record {i}"))?;

        let hdr = RecordHeader {
            rt_minutes: read_f64(&mut *f)?,
            ms_level: read_i32(&mut *f)?,
            polarity: read_i32(&mut *f)?,
            is_centroid: read_i32(&mut *f)?,
            scan_id: read_i32(&mut *f)?,
            n_points: read_u64(&mut *f)?,
        };
        let n = hdr.n_points as usize;
        // Bound the two n×8 arrays against the file so a corrupt n_points can't drive a huge alloc.
        // The 32-byte record header (rt8 + 4×i32 + n8) precedes the arrays at `off`.
        n.checked_mul(16)
            .filter(|b| *b as u64 <= self.file_len.saturating_sub(off + 32))
            .ok_or_else(|| anyhow!("Agilent record {i}: n_points {n} exceeds the host file bounds"))?;
        let mz_bytes = read_exact_vec(&mut *f, n * 8)?;
        let int_bytes = read_exact_vec(&mut *f, n * 8)?;
        let mz: Vec<f64> =
            mz_bytes.chunks_exact(8).map(|c| f64::from_le_bytes(c.try_into().unwrap())).collect();
        // Narrow intensity to f32 to match the TSF reader's IntensityArray dtype.
        let intensity: Vec<f32> = int_bytes
            .chunks_exact(8)
            .map(|c| f64::from_le_bytes(c.try_into().unwrap()) as f32)
            .collect();
        Ok((mz, intensity, hdr))
    }

    /// Build the mzdata spectrum for scan `i` (0-based). Built EXACTLY like `bruker_tsf.rs`:
    /// Float64 MZArray (`Unit::MZ`) + Float32 IntensityArray (`Unit::DetectorCounts`), the
    /// `MS:1000294 "mass spectrum"` param, and a single `ScanEvent` with `start_time` in minutes.
    pub fn spectrum(&self, i: usize) -> Result<MultiLayerSpectrum> {
        let (mz, intensity, meta) = self.fetch(i)?;

        let mut arrays = BinaryArrayMap::new();
        let mut mz_da =
            DataArray::wrap(&ArrayType::MZArray, BinaryDataArrayType::Float64, Vec::new());
        mz_da.update_buffer(mz.as_slice()).map_err(|e| anyhow!("encoding m/z: {e}"))?;
        mz_da.unit = Unit::MZ;
        arrays.add(mz_da);
        let mut int_da =
            DataArray::wrap(&ArrayType::IntensityArray, BinaryDataArrayType::Float32, Vec::new());
        int_da.update_buffer(intensity.as_slice()).map_err(|e| anyhow!("encoding intensity: {e}"))?;
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
        // the MIDAC SDK is required to read per-frame ion-mobility arrays.

        Ok(MultiLayerSpectrum::new(descr, Some(arrays), None, None))
    }
}

impl Drop for AgilentReader {
    fn drop(&mut self) {
        // Remove the temp binary the host wrote. Best-effort.
        let _ = std::fs::remove_file(&self.tmp_path);
    }
}
