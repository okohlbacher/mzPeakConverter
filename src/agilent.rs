//! Native Agilent MassHunter (`.d`) reader → mzdata spectra (PLAN §3.7), via an **out-of-process
//! .NET Framework 4.8 host** (`AgilentGlueHost.exe`, built from `glue/agilent/`).
//!
//! **Windows-runtime-only.** The module is `#[cfg(windows)]`; it runs only with the Agilent MHDAC
//! (MassHunter Data Access Component) DLLs present, sourced from a ProteoWizard install.
//!
//! History: this subprocess reader shipped at `cc8245e` (2026-06-27) beside the net48 host, was
//! dropped by merge `5a62b90` the next day in favour of the original in-process design that the
//! host no longer implements, and was restored in 0.11.0 (owner decision 2026-09-04, option A)
//! with two adaptations: the MHDAC directory comes from `pwiz_layout::agilent_dll_dir` (the
//! 3.0.26175 installer flattens the vendor DLLs beside `msconvert.exe`), and no blanket
//! `MS:1000294` is attached (the writer types rows from `ms_level`, 0.9.13 rule).
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
//!     `<MZPC_PWIZ_DIR>/vendor_api/Agilent` when that subdirectory exists, else from
//!     `<MZPC_PWIZ_DIR>` itself (`pwiz_layout::agilent_dll_dir`); the directory is passed to the
//!     host as `<mhdacDir>`.
//!
//! ## Cost model
//! The host materialises EVERY scan into one temp file (m/z f64 + intensity f64 per point, i.e.
//! 16 B/point) under `std::env::temp_dir()` before the first spectrum is read back: a 240 MB
//! profile Q-TOF `.d` becomes a ~3 GB temp file. The file is removed on `Drop`.
//!
//! ## Binary protocol (host → us)
//! `AGL2`, parsed by the host-testable `crate::agl` (which documents the layout). Beyond the
//! per-scan records it carries MHDAC's `ScanTypes` — the guard that keeps an MRM/SIM-only run
//! (a 6490 dMRM `.d`) OUT of this lane: MHDAC hands such a run over as one one-point "MS2
//! spectrum" per dwell, while the transition chromatograms the msconvert lane writes are the
//! data. The lane refuses those, so the corpus harness falls back to `--via-msconvert`.
//!
//! ## Scope
//! Non-IM MS only (profile or centroid, MS1/MS2). Agilent ion-mobility (6560 IM-QTOF) needs the
//! separate **MIDAC** SDK — out of scope here (TODO in [`AgilentReader::spectrum`]).

use std::cell::RefCell;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow, bail};

use mzdata::curie;
use mzdata::meta::InstrumentConfiguration;
use mzdata::params::{Param, Unit};
use mzdata::spectrum::bindata::{ArrayType, BinaryArrayMap, BinaryDataArrayType, DataArray};
use mzdata::spectrum::{
    MultiLayerSpectrum, ScanEvent, ScanPolarity, SignalContinuity, SpectrumDescription,
};

use crate::agl::{self, RecordHeader};

const HOST_EXE: &str = "AgilentGlueHost.exe";

/// A native Agilent `.d` reader. `open` runs the net48 host once to materialize a temp binary file
/// of all spectra; `spectrum(i)` seeks to record `i` and decodes it. The temp file is removed on
/// `Drop`.
pub struct AgilentReader {
    file: RefCell<File>,
    index: agl::Index,
    file_len: u64,
    tmp_path: PathBuf,
    /// Scans whose retention time MHDAC could not supply (the host writes NaN); stored as 0.0 and
    /// reported once when the reader closes.
    missing_rt: std::cell::Cell<usize>,
    /// The value rewrites the host counted and reported on stderr (see [`Self::transformations`]).
    host_counts: agl::HostCounts,
}

/// Unique temp filenames without pulling a `tempfile` dep: pid + a process-local counter. (Date/rand
/// are intentionally avoided — pid+counter is collision-free within this process.)
static TMP_CTR: AtomicU64 = AtomicU64::new(0);

impl AgilentReader {
    /// Open an Agilent `.d` directory: resolve the host EXE (`MZPC_AGILENT_GLUE`) and the MHDAC dir
    /// (`pwiz_layout::agilent_dll_dir`), run the host to write a temp binary, and load its index.
    pub fn open(path: &Path) -> Result<Self> {
        let glue_dir = crate::pwiz_layout::glue_dir("MZPC_AGILENT_GLUE", "agilent").ok_or_else(|| {
            anyhow!(
                "MZPC_AGILENT_GLUE not set and no glue/agilent beside the executable — point it at \
                 the `dotnet build` output dir of glue/agilent/ (containing {HOST_EXE})"
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
                 DLLs are loaded from <MZPC_PWIZ_DIR>/vendor_api/Agilent or <MZPC_PWIZ_DIR> itself"
            )
        })?;
        // Both ProteoWizard layouts: `vendor_api/Agilent` (bundles) or flattened (installers).
        let mhdac_dir = crate::pwiz_layout::agilent_dll_dir(&pwiz_dir);
        // Say what is wrong BEFORE spawning: the host would only report the directory it was given,
        // not the variable the user has to change.
        if !mhdac_dir.join("MassSpecDataReader.dll").is_file() {
            bail!(
                "MassSpecDataReader.dll not found under MZPC_PWIZ_DIR={} (looked in vendor_api/Agilent \
                 and in the directory itself) — point MZPC_PWIZ_DIR at a ProteoWizard install that \
                 carries the Agilent MHDAC DLLs",
                pwiz_dir.display()
            );
        }

        // The whole run lands in this file at 16 B/point (gigabytes for a profile Q-TOF run), so it
        // must never follow a TEMP that was pointed at a RAM disk for msconvert intermediates:
        // `MZPC_AGILENT_TMPDIR` names a disk location explicitly; `temp_dir()` is the default.
        let tmp_dir = std::env::var_os("MZPC_AGILENT_TMPDIR")
            .map(PathBuf::from)
            .filter(|d| d.is_dir())
            .unwrap_or_else(std::env::temp_dir);
        let ctr = TMP_CTR.fetch_add(1, Ordering::Relaxed);
        let tmp_path = tmp_dir.join(format!("mzpc-agilent-{}-{}.bin", std::process::id(), ctr));
        // The host writes `<out>.part` and renames on success; a host that dies natively (an MHDAC
        // access violation bypasses its catch/finally) leaves the `.part` — remove it with the
        // `.bin` on every failure path.
        let part_path = PathBuf::from(format!("{}.part", tmp_path.display()));

        // Run the host. Capture stderr for diagnostics; stdout is reserved/empty. The host exports
        // only the scans a `MZPC_MAX_SPECTRA` cap lets the converter write, so the rewrites it counts
        // (`agl::HostCounts`) are those of the archive's spectra: hand it the cap as parsed here, or
        // none, so an unparsable value cannot cap the host alone.
        let mut host_cmd = Command::new(&host);
        host_cmd.arg(path).arg(&mhdac_dir).arg(&tmp_path);
        match crate::max_spectra() {
            Some(n) => host_cmd.env("MZPC_MAX_SPECTRA", n.to_string()),
            None => host_cmd.env_remove("MZPC_MAX_SPECTRA"),
        };
        let out = host_cmd
            .output()
            .with_context(|| format!("spawning {}", host.display()))?;
        if !out.status.success() {
            let _ = std::fs::remove_file(&tmp_path);
            let _ = std::fs::remove_file(&part_path);
            let err = String::from_utf8_lossy(&out.stderr);
            let err = err.trim();
            bail!(
                "Agilent host failed to convert {} (exit {}): {}",
                path.display(),
                out.status.code().map(|c| c.to_string()).unwrap_or_else(|| "signal".into()),
                if err.is_empty() { "<no stderr>" } else { err }
            );
        }

        // Parse the index in a closure so EVERY failure path below removes the temp file: until the
        // reader exists nothing owns it, and a 3 GB leftover per failed open is not a diagnostic.
        let parse = || -> Result<(File, agl::Index, u64)> {
            let mut file = File::open(&tmp_path)
                .with_context(|| format!("opening host output {}", tmp_path.display()))?;
            let file_len = file.metadata().context("stat host output")?.len();
            let index = agl::parse_index(&mut file, file_len)
                .with_context(|| format!("parsing host output {}", tmp_path.display()))?;
            // MRM / SIM dwell data are chromatograms, not spectra (see the module doc and
            // `agl::is_dwell_only`); the msconvert lane writes them as SRM/SIM traces.
            if agl::is_dwell_only(&index.scan_types) {
                bail!(
                    "{} holds MRM/SIM dwell data only (MHDAC scan types: {}); the native Agilent lane \
                     stores scan spectra, and dwell data are transition chromatograms — convert this \
                     run with --via-msconvert",
                    path.display(),
                    index.scan_types
                );
            }
            Ok((file, index, file_len))
        };
        let (file, index, file_len) = match parse() {
            Ok(v) => v,
            Err(e) => {
                let _ = std::fs::remove_file(&tmp_path);
                let _ = std::fs::remove_file(&part_path);
                return Err(e);
            }
        };
        // A successful host may still have something to say: NaN/Inf intensities it stored as 0,
        // m/z and intensity arrays it cut to one length. Each is a transformation of the values,
        // so it is logged here and declared in the archive through `transformations`.
        let host_notes = String::from_utf8_lossy(&out.stderr);
        for line in host_notes.lines().map(str::trim).filter(|l| !l.is_empty()) {
            log::warn!("Agilent host: {line}");
        }
        let host_counts = agl::host_counts(&host_notes);
        if agl::has_dwell(&index.scan_types) {
            log::warn!(
                "{} mixes scan spectra with MRM/SIM dwell data (MHDAC scan types: {}); the dwells \
                 come through MHDAC as one-point MS2 spectra and are stored as such — the transition \
                 chromatograms are only available via --via-msconvert",
                path.display(),
                index.scan_types
            );
        }
        log::info!(
            "Agilent host materialised {} scans of {} into {} ({} MB; scan types: {}; device: {})",
            index.offsets.len(),
            path.display(),
            tmp_path.display(),
            file_len / 1_000_000,
            if index.scan_types.is_empty() { "unknown" } else { &index.scan_types },
            index.device.replace('\u{1F}', " / ")
        );

        Ok(Self { file: RefCell::new(file), index, file_len, tmp_path, missing_rt: std::cell::Cell::new(0), host_counts })
    }

    /// The `transformations` entries for what the host rewrote in this run's values, each declared
    /// only when the host counted at least one (`agl::HostCounts::transformations`).
    pub fn transformations(&self) -> Vec<String> {
        self.host_counts.transformations()
    }

    /// MHDAC's `ScanTypes` flags as reported by the host ("Scan", "MultipleReaction, SelectedIon", …;
    /// empty when the host could not read them).
    pub fn scan_types(&self) -> &str {
        &self.index.scan_types
    }

    /// The instrument identity as one readable line (device type / name / serial), or "unknown".
    pub fn device_label(&self) -> String {
        let parts: Vec<&str> = self.index.device.split('\u{1F}').map(str::trim).filter(|p| !p.is_empty()).collect();
        if parts.is_empty() { "unknown".to_string() } else { parts.join(" / ") }
    }

    /// What MHDAC said about the instrument: `MS:1000031 instrument model` carrying the device
    /// type / name, `MS:1000529 instrument serial number` when reported. `None` when the host
    /// learned nothing, so the run-metadata normaliser writes its empty configuration instead of
    /// an invented one.
    pub fn instrument(&self) -> Option<InstrumentConfiguration> {
        let mut parts = self.index.device.split('\u{1F}').map(str::trim);
        let (device_type, name, serial) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
        let model = match (device_type, name) {
            ("", "") => return None,
            (t, "") => t.to_string(),
            ("", n) => n.to_string(),
            (t, n) if n.contains(t) => n.to_string(),
            (t, n) => format!("{n} ({t})"),
        };
        let mut cfg = InstrumentConfiguration { id: 0, ..Default::default() };
        cfg.params.push(Param::builder().name("instrument model").curie(curie!(MS:1000031)).value(model).build());
        if !serial.is_empty() {
            cfg.params.push(Param::builder().name("instrument serial number").curie(curie!(MS:1000529)).value(serial.to_string()).build());
        }
        Some(cfg)
    }

    pub fn len(&self) -> usize {
        self.index.offsets.len()
    }

    /// Seek to record `i` and decode its header + the m/z (f64) and intensity (f64→f32) arrays.
    fn fetch(&self, i: usize) -> Result<(Vec<f64>, Vec<f32>, RecordHeader)> {
        let off = *self
            .index
            .offsets
            .get(i)
            .ok_or_else(|| anyhow!("Agilent spectrum index {i} out of range (count {})", self.len()))?;
        let mut f = self.file.borrow_mut();
        agl::read_record(&mut *f, off, self.file_len).with_context(|| format!("Agilent record {i}"))
    }

    /// Build the mzdata spectrum for scan `i` (0-based). Built EXACTLY like `bruker_tsf.rs`:
    /// Float64 MZArray (`Unit::MZ`) + Float32 IntensityArray (`Unit::DetectorCounts`) and a single
    /// `ScanEvent` with `start_time` in minutes. No blanket `MS:1000294 "mass spectrum"`: mzdata's
    /// `spectrum_type()` is a first-match lookup, so that parent term would shadow the writer's
    /// `MS:1000579`/`MS:1000580` inference from `ms_level` (0.9.13).
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
        let mut scan = ScanEvent::default();
        // Agilent reports RT in minutes; mzdata ScanEvent.start_time is also minutes. NaN is the
        // host's "scan record unavailable": store 0.0 and count it rather than write NaN.
        scan.start_time = if meta.rt_minutes.is_nan() {
            self.missing_rt.set(self.missing_rt.get() + 1);
            0.0
        } else {
            meta.rt_minutes
        };
        descr.acquisition.scans.push(scan);

        // TODO(IM-MS): Agilent 6560 IM-QTOF stores a drift dimension that MHDAC does not expose;
        // the MIDAC SDK is required to read per-frame ion-mobility arrays.

        Ok(MultiLayerSpectrum::new(descr, Some(arrays), None, None))
    }
}

impl Drop for AgilentReader {
    fn drop(&mut self) {
        if self.missing_rt.get() > 0 {
            log::warn!(
                "{} scan(s) had no retention time from MHDAC and were stored with start_time 0.0",
                self.missing_rt.get()
            );
        }
        // Remove the temp binary the host wrote. Best-effort.
        let _ = std::fs::remove_file(&self.tmp_path);
    }
}
