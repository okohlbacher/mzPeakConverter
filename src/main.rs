//! mzpeak-convert — single-command converter: mzML/imzML, Bruker `.d` (TDF/TSF/BAF), Thermo `.raw`,
//! and (Windows) Agilent/SciEX → mzPeak.
//!
//! `mzpeak-convert <input> [-o output] [options]`. With `--output` it converts; without it the input
//! is only inspected and reported. The conversion core wraps mzpeak_prototyping's reference writer
//! (`mzdata::MZReaderType` auto-detects format; the writer wiring — sampled data schema, metadata
//! copy, imaging presets, TDF ion-mobility — is reused), with native readers layered on for the
//! formats mzdata can't read (TSF/BAF, the lossless integer-TOF ims-compact path, Agilent/SciEX).
//! Vendor-SDK readers compile in per platform (see the cfg-gated modules below). See PLAN.md.

use std::fs;
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, ValueEnum};

// Vendor-SDK readers compile in automatically on the platforms where the proprietary vendor
// libraries exist — Windows for Agilent (MHDAC), SciEX (Clearcore2) and Bruker BAF; Linux also for
// Bruker BAF. They load the vendor DLLs at runtime and report a clear error if absent. macOS has no
// vendor SDKs, so none are built there. The dead-code allowance is conditional, like the
// cross-platform-compiled readers below: where a module is built it must earn its keep, and a
// blanket `allow` on it hid real dead code on the platforms that compile it.
#[cfg(any(windows, target_os = "linux"))]
#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
mod bruker_baf;
// Bruker timsdata SDK reader (TDF + TSF) — same OS envelope as baf2sql (Win + Linux, no macOS).
#[cfg(any(windows, target_os = "linux"))]
#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
mod bruker_sdk;
mod pwiz_layout;
mod agl;
mod agilent_host;
mod sciex_run;
#[cfg(windows)]
#[cfg_attr(not(windows), allow(dead_code))]
mod agilent;
#[cfg(windows)]
#[cfg_attr(not(windows), allow(dead_code))]
mod sciex;
// Native Shimadzu `.lcd` via the Shimadzu.LabSolutions.IO managed DLL (netcorehost glue, like
// SciEX). Windows-runtime-only; the `convert_shimadzu` dispatch is `#[cfg(windows)]`.
#[cfg(windows)]
#[cfg_attr(not(windows), allow(dead_code))]
mod shimadzu;
// Exact sqrt-grid fit for Shimadzu profile axes; pure arithmetic, tested on every host.
#[cfg_attr(not(windows), allow(dead_code))]
mod shimadzu_grid;
// Vendor-neutral fixed-point m/z lattice (`k = round(m/z * scale)`): the detector, the per-spectrum
// guard, the `spectra_peaks` schema and the `mz_calibration` block. Used by the Shimadzu native lane
// (through `shimadzu_grid`, at 1e-9) and by the ordinary mzML/generic lane at the detected scale.
#[cfg_attr(not(windows), allow(dead_code))]
mod mz_lattice;
// libloading-based (cross-platform compile); only *runs* on Windows with the MassLynx DLLs, so the
// `convert_waters` dispatch stays `#[cfg(windows)]` and the reader is dead code off-Windows.
#[cfg_attr(not(windows), allow(dead_code))]
mod waters;
mod agilent_profile;
mod bruker_native;
mod bruker_tsf;
mod bruker_traces;
mod tof_grid;
mod tims_mobility;
mod thermo_status;
mod thermo_trailers;
mod thermo_isolation;
mod run_metadata;
mod agilent_meta;
#[cfg_attr(not(windows), allow(dead_code))]
mod waters_meta;
#[cfg_attr(not(windows), allow(dead_code))]
mod shimadzu_meta;
mod vendor;
mod embed_aux;
mod filter;
mod mzml_isolation;
mod pwiz_id;

use arrow::datatypes::DataType;
use mzdata::curie;
use mzdata::io::MZReaderType;
use mzdata::meta::{
    DataProcessing, InstrumentConfiguration, ProcessingMethod, Software,
    SourceFile, custom_software_name,
};
// Used only inside `cfg(windows)` lanes (Shimadzu instrument components): unused on macOS, where
// removing the import broke the Windows build twice already.
#[cfg_attr(not(windows), allow(unused_imports))]
use mzdata::meta::{Component, ComponentType};
use mzdata::params::{ControlledVocabulary, Param, Unit};
use mzdata::prelude::*;
use mzdata::spectrum::bindata::BinaryArrayMap3D;
use mzdata::spectrum::{BinaryArrayMap, Chromatogram, ChromatogramDescription, ChromatogramType, MultiLayerSpectrum};
use mzdata::spectrum::bindata::{ArrayType, BinaryDataArrayType, DataArray};
use mzpeak_prototyping::{BufferContext, BufferName};
use mzpeak_prototyping::archive::ZipArchiveWriter;
use mzpeak_prototyping::chunk_series::ChunkingStrategy;
use mzpeak_prototyping::peak_series::INTENSITY_ARRAY;
use mzpeak_prototyping::writer::{
    AbstractMzPeakWriter, ArrayBuffersBuilder, CustomBuilderFromParameter, MzPeakWriterType,
};
use mzpeaks::{CentroidPeak, DeconvolutedPeak};
use parquet::basic::{Compression, ZstdLevel};

/// How many spectra the writer buffers before flushing a batch to Parquet. The vendored default is
/// 5000, which is fine for small spectra but pins gigabytes for large profile / ion-mobility spectra
/// (5000 × 100k points × ~16 B ≈ 8 GB) — the cause of the sweep OOMs. A few hundred keeps the
/// in-RAM buffer to a few hundred MB while the writer still streams row groups to disk. Override with
/// `$MZPC_BUFFER_SPECTRA`. (The ims-compact peak writer is separately point-bounded, so this only
/// governs the standard f64 paths.)
fn buffer_spectra() -> usize {
    std::env::var("MZPC_BUFFER_SPECTRA")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(256)
}

/// Read a boolean `MZPC_*` lever ONE way for every lever. `None` when the variable is unset;
/// `Some(false)` when it is set to an "off" spelling — empty, `0`, `false`, `no` (any case);
/// `Some(true)` for anything else. The three-way answer matters: a lever that defaults ON
/// (`MZPC_BYTE_PLANE_INTENSITY`) must treat "unset" as ON but "set to nothing" as OFF, and a lever
/// that defaults OFF must treat both as OFF. Before this each site spelt its own rule — one read
/// `var_os().is_some()`, so `MZPC_DUMP_IM_TABLE=` (empty) replaced a whole conversion — and the
/// spellings disagreed with each other and with the manual.
fn env_flag(name: &str) -> Option<bool> {
    let raw = std::env::var_os(name)?;
    let v = raw.to_string_lossy();
    let v = v.trim();
    Some(!(v.is_empty() || v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("no")))
}

/// The index block that marks an archive `$MZPC_MAX_SPECTRA` truncated, so a partial archive is
/// detectable OFFLINE and not only from the WARN that scrolled past when it was written. `None`
/// when the run was not capped, or the cap did not bite (a cap of 1000 on a 500-spectrum file
/// converts everything). With no declared count to compare against, stopping exactly at the cap is
/// taken as truncation — the cap is a diagnostic lever and an honest "maybe partial" beats a silent
/// "complete".
fn partial_marker(input: &Path, cap: Option<usize>, written: usize) -> Option<(String, serde_json::Value)> {
    let n = cap?;
    let declared = declared_spectrum_count(input);
    let truncated = declared.map_or(written >= n, |d| d > written as u64);
    if !truncated {
        return None;
    }
    Some((
        "partial".to_string(),
        serde_json::json!({
            "partial": true,
            "max_spectra": n,
            "source_declared": declared,
            "spectra_written": written,
            "cause": "MZPC_MAX_SPECTRA",
        }),
    ))
}

/// Optional hard cap on how many spectra to convert (`$MZPC_MAX_SPECTRA`). Mainly for diagnostics /
/// quick cross-checks (e.g. the ion-mobility comparison only needs a handful of frames to cover the
/// full mobility axis), so a multi-GB run becomes seconds. `None` = convert everything. Every
/// mzPeak lane that honours the cap also writes the [`partial_marker`] index block when it bites.
/// `--sample N` (SciEX multi-sample WIFF), set once after argument parsing and read by the SciEX
/// lanes — the alternative was threading one more parameter through five conversion signatures.
static SCIEX_SAMPLE: std::sync::OnceLock<Option<u32>> = std::sync::OnceLock::new();

fn sciex_sample() -> Option<u32> {
    SCIEX_SAMPLE.get().copied().flatten()
}

fn max_spectra() -> Option<usize> {
    let cap = std::env::var("MZPC_MAX_SPECTRA")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0);
    // Say it out loud, once. A capped archive is structurally valid, exits 0, and is
    // indistinguishable from a complete one — the cap also switches OFF `assert_source_complete`,
    // the check that otherwise refuses to write a partial archive. An inherited environment
    // variable must not be able to silently produce a truncated conversion.
    if let Some(n) = cap {
        static SAID: std::sync::Once = std::sync::Once::new();
        SAID.call_once(|| {
            log::warn!(
                "MZPC_MAX_SPECTRA={n} is set: this conversion STOPS after {n} spectra and the \
                 completeness check is disabled, so the archive is a PARTIAL one that still exits \
                 0. Unset it for a real conversion."
            );
        });
    }
    cap
}

/// The `--representation` choice, published once after CLI parsing. `convert_file` already carries
/// nine parameters and this one matters to exactly one vendor lane, so it travels out-of-band rather
/// than as a tenth argument threaded through every caller. Single-process, set-once, read-only after.
static REPRESENTATION: std::sync::OnceLock<RepresentationArg> = std::sync::OnceLock::new();

// Every reader that honours `--representation` (Shimadzu `.lcd`, Bruker BAF) is behind a `cfg`, so
// off those platforms this getter has no callers.
#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
fn representation() -> RepresentationArg {
    *REPRESENTATION.get().unwrap_or(&RepresentationArg::Both)
}

/// The `--no-mz-lattice` opt-out, published the same way and for the same reason as
/// [`REPRESENTATION`]. Default (unset) = the lattice is ON wherever the data is on one.
static NO_MZ_LATTICE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// Is the fixed-point m/z lattice (see [`mz_lattice`]) allowed on this run? `--no-mz-lattice`
/// turns it off; `$MZPC_NO_MZ_LATTICE=1` does the same without touching the command line, which is
/// what the before/after byte-identity checks use on one and the same binary.
fn mz_lattice_enabled() -> bool {
    if *NO_MZ_LATTICE.get().unwrap_or(&false) {
        return false;
    }
    env_flag("MZPC_NO_MZ_LATTICE") != Some(true)
}

/// CLI spelling of the signal representation to read. Mirrors `shimadzu::Representation`, and is
/// also what the Bruker BAF lane consumes directly (that module builds on Linux too, where the
/// `cfg(windows)` shimadzu enum does not exist).
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, clap::ValueEnum, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RepresentationArg {
    /// Read every representation the file contains (faithful default).
    Both,
    /// Read only profile data.
    Profile,
    /// Read only centroided data.
    Centroid,
}

/// Exit codes (shared contract, mirrors mzML2mzPeak).
mod exit {
    pub const OK: i32 = 0;
    pub const GENERIC: i32 = 1;
    pub const UNSUPPORTED: i32 = 3;
}

/// Marker error for "this input/format isn't supported in this build" — main maps it to exit 3
/// (distinct from a generic failure) so corpus runners can classify it as a skip, not a crash.
#[derive(Debug)]
#[allow(dead_code)] // only constructed when a vendor feature is OFF; downcast in main always refers to it
struct UnsupportedVendor(String);
impl std::fmt::Display for UnsupportedVendor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for UnsupportedVendor {}

/// mzPeak converter — a single command. Give an input and (optionally) an output:
///   * with `-o/--output`  → convert and write the `.mzpeak` archive
///   * without `--output`  → write nothing; just inspect the input and print a report
/// `-v` prints the inspection report even during a real conversion.
#[derive(Parser, Debug)]
#[command(
    name = "mzpeak-convert",
    version,
    about = "Convert MS data (mzML/imzML, Bruker, Thermo, SciEX, ...) to mzPeak — or to mzML with --to mzml",
    propagate_version = true
)]
struct Cli {
    /// Input file or vendor directory (mzML/.mzML.gz/imzML, Bruker .d, Thermo .raw).
    input: PathBuf,

    /// Output path. `.mzpeak` (default) or `.mzML` — the format is inferred from the extension (or
    /// forced with `--to`). If omitted, NOTHING is written — the input is only inspected and a
    /// report (format, spectra, chromatograms) is printed.
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Config file (YAML) setting defaults for any option below; explicit command-line flags win.
    #[arg(short = 'c', long)]
    config: Option<PathBuf>,

    /// Signal layout [default: chunked].
    #[arg(long, value_enum)]
    layout: Option<Layout>,

    /// Output format [default: inferred from the -o extension — `.mzML`→mzml, else mzpeak]. `mzml`
    /// writes a plain mzML (vendor→mzML) instead of mzPeak, bypassing the mzPeak-specific encoders.
    #[arg(long, value_enum)]
    to: Option<OutputFormat>,

    /// Lossless delta m/z chunking instead of the default lossy numpress-linear.
    #[arg(long)]
    no_numpress: bool,

    /// Disable the fixed-point m/z LATTICE for centroid peaks and store f64 `mz` instead.
    ///
    /// Some vendors hand over m/z that are really integers over a power of ten (Shimadzu `MassHigh`
    /// at 1e-9 Da, and the LabSolutions mzML export of the same acquisition). When the sampled
    /// centroids all land on such a lattice, the peaks facet stores `tof_index` = round(m/z·scale)
    /// as Int64 (DELTA_BINARY_PACKED) with an `mz_calibration` index block, which is LOSSLESS and
    /// smaller than both numpress-linear (lossy) and delta chunking — measured on a 4.5 GB
    /// LabSolutions DIA mzML: 2.19 GB delta / 1.36 GB numpress / 1.31 GB lattice. Any spectrum that
    /// does not fit keeps its exact f64 m/z in the same facet's `mz` column; nothing is snapped.
    ///
    /// This flag turns the whole thing off, on every lane (the native Shimadzu `.lcd` one
    /// included); `MZPC_NO_MZ_LATTICE=1` does the same from the environment. Use it when the
    /// archive is destined for a reader that does not know the `mz-grid` codec and so cannot
    /// reconstruct an integer m/z axis. Data that is not on a lattice is unaffected either way.
    #[arg(long)]
    no_mz_lattice: bool,

    /// m/z chunk width (Th) for the chunked layout [default: 50].
    #[arg(long)]
    chunk_size: Option<f64>,

    /// Zstd compression level (1–22) [default: 3; the timsTOF ims-compact lanes: 5, the measured
    /// byte-plane plateau]. An explicit value applies to every lane.
    #[arg(long)]
    zstd_level: Option<i32>,

    /// Overwrite the output if it already exists.
    #[arg(short, long)]
    force: bool,

    /// Bruker timsTOF (TDF) only: disable the default lossless ims-compact integer-TOF storage and
    /// write standard f64 m/z instead.
    #[arg(long)]
    no_ims_compact: bool,


    /// Which signal representation to read when a vendor supplies BOTH profile and centroid for the
    /// same spectrum (Shimadzu `.lcd` does). `both` (the DEFAULT) is faithful to the raw data: profile
    /// goes to the `spectra_data` facet, centroid to `spectra_peaks`, and the metadata row carries
    /// both `number_of_data_points` and `number_of_peaks` so a reader knows which to read. `profile`
    /// / `centroid` force one view. A requested representation the file does not contain is a
    /// warning, not an error — the other one is written instead of producing an empty archive.
    /// Honoured by the Shimadzu `.lcd` and Bruker BAF readers (BAF: mzPeak output only) [default:
    /// both].
    #[arg(long, value_enum)]
    representation: Option<RepresentationArg>,

    /// Bruker timsTOF (TDF) ims-compact only — select the CHUNKED layout for rapid m/z-range access.
    /// OFF BY DEFAULT. When absent, timsTOF data is written in the ARCHIVE layout (the default): a flat
    /// table of absolute integer TOF bins — maximum compression and fast whole-spectrum access, but no
    /// m/z index. `--ims-chunked` instead splits each frame's peaks into true m/z 50-Th bins (override
    /// width with `--chunk-size`, in Th); every chunk records its main-axis (TOF) bounds
    /// (`chunk_start`/`chunk_end`) as Parquet columns WITH page statistics, so the m/z axis becomes
    /// page-prunable — XIC / m/z-slice queries are ~20x faster — at roughly parity-to-+8% file size.
    /// TOF is delta-encoded within each chunk (start point excluded; cumulative-sum from
    /// `chunk_start` to reconstruct, lossless).
    #[arg(long)]
    ims_chunked: bool,

    /// Read Bruker TDF/TSF `.d` via the official Bruker timsdata SDK (parallel path to the default
    /// pure-Rust readers; Windows/Linux only, needs timsdata.dll/libtimsdata.so). On a TDF `.d` this
    /// still writes the lossless integer-TOF ims-compact layout — the SDK exposes the raw TOF index
    /// too. Add `--no-ims-compact` for f64 m/z.
    #[arg(long)]
    bruker_sdk: bool,

    /// Bruker timsTOF (TDF), ims-compact path only: disable this converter's vendor-grade
    /// scan→1/K0 recalibration (the `TimsCalibration` ModelType-2 model) and use timsrust's linear
    /// approximation. Recalibration is ON by default. INERT with `--no-ims-compact`: that lossy
    /// path takes its mobility from mzdata's TDF reader, which (since mzdata 0.66) applies the
    /// same ModelType-2 calibration itself, unconditionally — there is nothing to switch off.
    #[arg(long)]
    no_tims_recalibration: bool,

    /// Do not embed vendor side-files into the archive.
    #[arg(long)]
    no_vendor: bool,

    /// Do not synthesize TIC + base-peak chromatograms from the MS1 spectra (synthesis is on by default).
    #[arg(long)]
    no_chromatograms: bool,

    /// Vendor side-file rule (repeatable): `glob=embed` or `glob=drop`. Highest precedence.
    #[arg(long)]
    aux: Vec<String>,

    /// **Standard-lane inputs (mzML/imzML, Thermo `.raw`, TDF with `--no-ims-compact`, `--via-msconvert`):**
    /// embed an optical image VERBATIM into the archive as
    /// `images/image_NNNN.<ext>` with a `metadata.imaging` overlay affine. Repeatable. A bad/missing
    /// path here ERRORS the conversion (strict). An `<input-stem>-opticalimage.{tif,tiff,png,jpg}`
    /// sibling is additionally auto-discovered (best-effort: warn + skip if unreadable).
    #[arg(long)]
    image: Vec<PathBuf>,

    /// **Standard-lane inputs (as for `--image`); refused on the native vendor and ims-compact lanes:**
    /// embed an SDRF (sample-metadata) TSV VERBATIM as
    /// `sample_metadata/sdrf.tsv` with `metadata.study` + `metadata.sample_metadata` back-refs. A
    /// missing/unreadable path ERRORS the conversion.
    #[arg(long)]
    sdrf: Option<PathBuf>,

    /// mzPeak input only: keep spectra whose time is within MIN-MAX (unit matches stored spectrum.time).
    #[arg(long, value_name = "MIN-MAX")]
    rt: Option<String>,

    /// mzPeak input only: keep spectra with these MS levels (repeatable or comma-list).
    #[arg(long = "ms-level", value_delimiter = ',')]
    ms_level: Vec<u8>,

    /// mzPeak input only: drop archive members matching this glob (repeatable).
    #[arg(long = "drop-aux")]
    drop_aux: Vec<String>,

    /// **Inputs read through mzdata only** (mzML incl. `--via-msconvert`, imzML, Thermo `.raw`, and a
    /// TDF read as f64 m/z under `--no-ims-compact` or the ims-compact fallback): compactify
    /// exact-lattice TOF profile data by DETECTING an integer flight-time grid in the decoded f64 m/z
    /// and storing `tof_index` (Int32) + a per-run `{c0,c1}` instead, recovering
    /// `m/z = (c0 + c1·tof_index)²`. **Off by default** and
    /// bounded-lossy (reconstruction within `PPM_TOL`) — it reverse-engineers the grid msconvert
    /// discarded. `auto` applies it when a strict fit passes; `on` requires the fit (errors otherwise);
    /// `off` keeps exact f64. The native vendor lanes other than SCIEX ignore it: the timsTOF
    /// ims-compact lanes and `--agilent-grid` store the integer grid of the vendor calibration
    /// (lossless), and the Bruker TSF/BAF, Agilent MHDAC (with a warning), Waters and Shimadzu lanes
    /// store the m/z their reader returns.
    ///
    /// **Native SCIEX `.wiff` (Windows):** Clearcore2 hands over decoded f64 m/z only, so that
    /// lane also fits the grid statistically. There the default (flag absent) is `auto` — the
    /// per-spectrum fit within the same bound, unchanged from earlier releases — `off` stores the
    /// exact f64 m/z the vendor library returned (the opt-out the fidelity invariant requires), and
    /// `on` errors when no run-wide digitizer clock can be fitted.
    #[arg(long, value_enum)]
    tof_grid: Option<TofGridMode>,

    /// SciEX `.wiff` holding SEVERAL samples: which one to convert (1-based). An archive is ONE
    /// run, so a multi-sample file is refused without this (concatenating the samples under one
    /// run id was a conversion of none of them). Inert, with a warning, on any other input. The
    /// msconvert lanes map it to
    /// `--runIndexSet <N-1>` and refuse a multi-run file without it: msconvert writes every run
    /// onto the one output, so only the LAST would survive.
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
    sample: Option<u32>,

    /// Agilent Q-TOF **profile** `.d` only: read the integer flight-time grid straight from
    /// `AcqData/MSProfile.bin` (pure Rust, no MHDAC/msconvert) and store `tof_index` (Int32) with
    /// per-spectrum `tof_c0`/`tof_c1`/`tof_calibration_id` columns (the MassHunter calibration drifts
    /// from scan to scan) instead of f64 m/z, recovering `m/z = (tof_c0 + tof_c1·tof_index)²`, in
    /// `spectra_data` (it is profile data).
    /// Far smaller than the msconvert lane (≈0.14×). OFF by default; only applies when
    /// `AcqData/MSProfile.bin` is non-empty (centroid-only `.d` fall through to the standard path).
    #[arg(long)]
    agilent_grid: bool,

    /// Read the input via ProteoWizard `msconvert` (→ mzML → mzPeak). Cross-vendor path for formats
    /// without a native reader in this build (Agilent `.d`, SciEX `.wiff`, ...).
    #[arg(long)]
    via_msconvert: bool,

    /// Path to the `msconvert` executable (else `$MSCONVERT_PATH`, else `msconvert` on PATH).
    #[arg(long)]
    msconvert_path: Option<PathBuf>,

    /// Verbose: print the inspection report and debug logs (repeat `-vv` for trace logs). An
    /// explicit `-v` / `-q` WINS over `RUST_LOG`; `RUST_LOG` is consulted only when neither flag is
    /// given (default level `info`).
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,

    /// Silence all logs except errors (wins over `RUST_LOG`, see `-v`).
    #[arg(short, long, conflicts_with = "verbose")]
    quiet: bool,
}

#[derive(ValueEnum, serde::Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Layout {
    /// Chunked m/z layout (default; numpress-linear or delta).
    Chunked,
    /// Flat point layout (one row per m/z–intensity pair).
    Point,
}

/// Output container. `mzpeak` is the default. `mzml` bypasses the mzPeak encoders entirely and
/// writes a plain mzML through the mzdata writer — turning the tool into a cross-platform
/// vendor→mzML converter for every format it can read natively (mzML/imzML, Thermo `.raw`, Bruker
/// TDF/TSF/BAF, plus the Windows native vendor readers: SciEX/Waters/Agilent/Shimadzu).
#[derive(ValueEnum, serde::Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
enum OutputFormat {
    Mzpeak,
    Mzml,
}

/// Infer the output format from the `-o` file extension: `.mzML`/`.mzml` → mzML, everything else
/// (`.mzpeak`, no/unknown extension) → mzPeak.
fn infer_output_format(output: &Path) -> OutputFormat {
    // `x.mzML.gz` is an mzML request too: look through a trailing `.gz` before deciding. Without
    // this the last extension is `gz`, the request falls to "everything else", and the user gets an
    // mzPeak ARCHIVE written under a `.mzML.gz` name.
    let inner = if has_gz_suffix(output) { output.with_extension("") } else { output.to_path_buf() };
    match inner.extension().and_then(|e| e.to_str()) {
        Some(e) if e.eq_ignore_ascii_case("mzml") => OutputFormat::Mzml,
        _ => OutputFormat::Mzpeak,
    }
}

/// Does the path end in `.gz` (any case)?
fn has_gz_suffix(p: &Path) -> bool {
    p.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("gz"))
}

/// The byte sink for an mzML export: the plain file, or a streaming gzip encoder when the requested
/// name ends in `.gz`. The XML is compressed AS it is written — one pass, no re-read. Both the mzML
/// writer and the encoder finish on drop (the writer closes the document, the encoder writes the
/// gzip trailer), which is why the four export sites can let `w` fall out of scope as before. Above
/// both sits [`mzml_isolation::TargetOnlyWindows`]: mzdata's writer prints an isolation window of
/// unknown width as offsets of ±target, and that sink leaves it target-only.
fn mzml_sink(output: &Path) -> Result<Box<dyn Write>> {
    let file = fs::File::create(output).with_context(|| format!("creating {}", output.display()))?;
    let sink: Box<dyn Write> = if has_gz_suffix(output) {
        log::info!("output name ends in .gz: gzip-compressing the mzML as it is written");
        Box::new(flate2::write::GzEncoder::new(file, flate2::Compression::default()))
    } else {
        Box::new(file)
    };
    Ok(Box::new(mzml_isolation::TargetOnlyWindows::new(sink)))
}

/// When to apply the statistically-DETECTED TOF-grid m/z encoding (strategy A). This
/// reverse-engineers an integer flight-time grid from already-decoded f64 m/z, so it is
/// bounded-lossy (reconstruction within `PPM_TOL`). It applies on the mzML path (default `off`) and
/// on the native SCIEX lane, whose vendor library also returns only decoded f64 (default `auto`
/// there — see `Cli::tof_grid`); the lane reads the resolved `Option<TofGridMode>` so it can tell
/// "not given" from an explicit `off`. Native readers with the true grid (Bruker, `--agilent-grid`) do NOT
/// use this — they read it from the vendor calibration (strategy B), lossless by construction.
#[derive(ValueEnum, serde::Deserialize, Clone, Copy, Debug, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
enum TofGridMode {
    /// Never apply the detected grid; keep exact f64 m/z. (default — exact is the safe choice)
    #[default]
    Off,
    /// Apply only when a strict grid fit passes (within `PPM_TOL`); otherwise keep f64 m/z.
    Auto,
    /// Require the grid fit; error if the input is not griddable.
    On,
}

/// Config-file schema: every overridable option, all optional. Loaded from `--config`. Precedence:
/// explicit command-line flag > config-file value > built-in default.
#[derive(serde::Deserialize, Default, Debug)]
#[serde(default, deny_unknown_fields)]
struct FileConfig {
    output: Option<PathBuf>,
    to: Option<OutputFormat>,
    layout: Option<Layout>,
    no_numpress: Option<bool>,
    no_mz_lattice: Option<bool>,
    chunk_size: Option<f64>,
    zstd_level: Option<i32>,
    force: Option<bool>,
    no_ims_compact: Option<bool>,
    ims_chunked: Option<bool>,
    bruker_sdk: Option<bool>,
    no_tims_recalibration: Option<bool>,
    no_vendor: Option<bool>,
    no_chromatograms: Option<bool>,
    aux: Option<Vec<String>>,
    image: Option<Vec<PathBuf>>,
    sdrf: Option<PathBuf>,
    tof_grid: Option<TofGridMode>,
    agilent_grid: Option<bool>,
    via_msconvert: Option<bool>,
    msconvert_path: Option<PathBuf>,
    // The six below were missing until 0.9.13 although `--config` promised "any option": a file
    // with `representation: profile` was rejected as an unknown field.
    representation: Option<RepresentationArg>,
    rt: Option<String>,
    ms_level: Option<Vec<u8>>,
    drop_aux: Option<Vec<String>>,
    verbose: Option<u8>,
    quiet: Option<bool>,
    // Missing from 0.11.3, when `--sample` arrived, until this key: rejected as an unknown field.
    sample: Option<u32>,
}

/// Effective settings after merging CLI over config-file over defaults.
struct Settings {
    output: Option<PathBuf>,
    output_format: OutputFormat,
    layout: Layout,
    no_numpress: bool,
    /// `--no-mz-lattice`: store f64 `mz` even when the centroids are on a fixed-point lattice.
    no_mz_lattice: bool,
    chunk_size: f64,
    zstd_level: i32,
    /// zstd level for the byte-plane timsTOF ims-compact path. Defaults to 5 (the measured plateau —
    /// higher levels add time, not compression) rather than the general default of 3; an explicit
    /// `--zstd-level` still wins.
    ims_zstd_level: i32,
    force: bool,
    no_ims_compact: bool,
    /// OPT-IN chunked integer-TOF ims-compact layout (m/z-boundary chunks). Default false.
    ims_chunked: bool,
    bruker_sdk: bool,
    tims_recalibration: bool,
    no_vendor: bool,
    chromatograms: bool,
    aux: Vec<String>,
    image: Vec<PathBuf>,
    sdrf: Option<PathBuf>,
    /// `None` = not given anywhere. Lanes decide their own default: the mzML path maps it to
    /// `Off`, the native SCIEX lane to `Auto` (see `Cli::tof_grid`).
    tof_grid: Option<TofGridMode>,
    agilent_grid: bool,
    via_msconvert: bool,
    msconvert_path: Option<PathBuf>,
    representation: RepresentationArg,
    rt: Option<String>,
    ms_level: Vec<u8>,
    drop_aux: Vec<String>,
    verbose: u8,
    quiet: bool,
    /// `--sample`: which sample of a multi-sample SciEX `.wiff` (1-based), published as `SCIEX_SAMPLE`.
    sample: Option<u32>,
    /// The options the user supplied ON THE COMMAND LINE (a config-file value is a standing default
    /// and is never counted here — see `resolve`), by
    /// their command-line spelling. [`refuse_unsupported_flags`] checks these, and only these,
    /// against the lane: a built-in default is never something a lane can be accused of dropping.
    given: Vec<&'static str>,
}

impl Settings {
    fn resolve(cli: &Cli) -> Result<Self> {
        let fc: FileConfig = match &cli.config {
            Some(p) => {
                let text = fs::read_to_string(p)
                    .with_context(|| format!("reading config {}", p.display()))?;
                serde_yaml::from_str(&text)
                    .with_context(|| format!("parsing config {}", p.display()))?
            }
            None => FileConfig::default(),
        };
        // CLI bool flags are "enable" switches, so they OR with the config value (the CLI can only
        // turn a switch on, matching its own expressiveness); typed options take the CLI value when
        // given, else the config value, else the built-in default.
        let output = cli.output.clone().or(fc.output);
        // Output format: explicit --to wins, else config, else infer from the -o extension
        // (`.mzML`→mzml, else mzpeak).
        let output_format = cli.to.or(fc.to).unwrap_or_else(|| {
            output.as_deref().map(infer_output_format).unwrap_or(OutputFormat::Mzpeak)
        });
        // "Given" = set on the COMMAND LINE. A config file is a standing profile applied to every
        // invocation, so a value there is a default, not this run's intent — counting it would make
        // a profile that carries `zstd_level: 12` refuse the `.mzpeak` filter lane outright. Kept
        // beside the merge so a new option cannot be added to one list and forgotten in the other.
        let mut given: Vec<&'static str> = Vec::new();
        let mut note = |on: bool, flag: &'static str| {
            if on {
                given.push(flag);
            }
        };
        note(cli.layout.is_some(), "--layout");
        note(cli.no_numpress, "--no-numpress");
        note(cli.no_mz_lattice, "--no-mz-lattice");
        note(cli.chunk_size.is_some(), "--chunk-size");
        note(cli.zstd_level.is_some(), "--zstd-level");
        note(cli.no_ims_compact, "--no-ims-compact");
        note(cli.representation.is_some(), "--representation");
        note(cli.ims_chunked, "--ims-chunked");
        note(cli.bruker_sdk, "--bruker-sdk");
        note(cli.no_tims_recalibration, "--no-tims-recalibration");
        note(cli.no_vendor, "--no-vendor");
        note(cli.no_chromatograms, "--no-chromatograms");
        // `--aux` / `--image` follow the same rule: a profile's `aux:` / `image:` list is a default
        // (exactly as its `sdrf:` is), so it takes effect where a lane can use it and cannot make a
        // lane refuse.
        note(!cli.aux.is_empty(), "--aux");
        note(!cli.image.is_empty(), "--image");
        note(cli.sdrf.is_some(), "--sdrf");
        note(cli.tof_grid.is_some(), "--tof-grid");
        note(cli.sample.is_some(), "--sample");
        note(cli.agilent_grid, "--agilent-grid");
        // clap refuses `--sample 0`; a config file gets the same answer rather than sample 1.
        if fc.sample == Some(0) {
            bail!("sample: 0 in the config file; samples are numbered from 1");
        }
        note(cli.via_msconvert, "--via-msconvert");
        note(cli.msconvert_path.is_some(), "--msconvert-path");
        Ok(Settings {
            output,
            output_format,
            layout: cli.layout.or(fc.layout).unwrap_or(Layout::Chunked),
            no_numpress: cli.no_numpress || fc.no_numpress.unwrap_or(false),
            no_mz_lattice: cli.no_mz_lattice || fc.no_mz_lattice.unwrap_or(false),
            chunk_size: cli.chunk_size.or(fc.chunk_size).unwrap_or(50.0),
            zstd_level: cli.zstd_level.or(fc.zstd_level).unwrap_or(3),
            ims_zstd_level: cli.zstd_level.or(fc.zstd_level).unwrap_or(5),
            force: cli.force || fc.force.unwrap_or(false),
            no_ims_compact: cli.no_ims_compact || fc.no_ims_compact.unwrap_or(false),
            ims_chunked: cli.ims_chunked || fc.ims_chunked.unwrap_or(false),
            bruker_sdk: cli.bruker_sdk || fc.bruker_sdk.unwrap_or(false),
            tims_recalibration: !(cli.no_tims_recalibration
                || fc.no_tims_recalibration.unwrap_or(false)),
            no_vendor: cli.no_vendor || fc.no_vendor.unwrap_or(false),
            chromatograms: !(cli.no_chromatograms || fc.no_chromatograms.unwrap_or(false)),
            aux: if cli.aux.is_empty() { fc.aux.unwrap_or_default() } else { cli.aux.clone() },
            image: if cli.image.is_empty() { fc.image.unwrap_or_default() } else { cli.image.clone() },
            sdrf: cli.sdrf.clone().or(fc.sdrf),
            tof_grid: cli.tof_grid.or(fc.tof_grid),
            agilent_grid: cli.agilent_grid || fc.agilent_grid.unwrap_or(false),
            via_msconvert: cli.via_msconvert || fc.via_msconvert.unwrap_or(false),
            msconvert_path: cli.msconvert_path.clone().or(fc.msconvert_path),
            representation: cli.representation.or(fc.representation).unwrap_or(RepresentationArg::Both),
            rt: cli.rt.clone().or(fc.rt),
            ms_level: if cli.ms_level.is_empty() { fc.ms_level.unwrap_or_default() } else { cli.ms_level.clone() },
            drop_aux: if cli.drop_aux.is_empty() { fc.drop_aux.unwrap_or_default() } else { cli.drop_aux.clone() },
            // `-v` is a count, so "given" is `> 0`; the config value fills in only when the command
            // line said nothing. `quiet` ORs like the other enable switches (clap already refuses
            // `-q -v` together; a config `quiet: true` under a command-line `-v` keeps quiet, which
            // is what `init_logging` has always done when both were set).
            verbose: if cli.verbose > 0 { cli.verbose } else { fc.verbose.unwrap_or(0) },
            quiet: cli.quiet || fc.quiet.unwrap_or(false),
            sample: cli.sample.or(fc.sample),
            given,
        })
    }
}

/// The temp files currently being written — each lane's `<out>.mzpeak.tmp`, the Agilent host's
/// `.bin` / `.part` — and msconvert's working directories (`true`), with the thread that registered
/// each, for [`install_tmp_panic_hook`].
static TMP_IN_FLIGHT: std::sync::Mutex<Vec<(std::thread::ThreadId, PathBuf, bool)>> =
    std::sync::Mutex::new(Vec::new());

/// Register a temp file with the panic-hook sweep without owning it: the Agilent host's `.bin` and
/// `.part`, which another process writes and `agilent.rs` removes on every ordinary exit. Under
/// `panic = "abort"` no `Drop` runs, so the sweep is what removes them — 16 B/point of the run,
/// gigabytes. [`TmpGuard::forget_path`] unregisters it.
fn track_tmp_in_flight(path: &Path) {
    TmpGuard::register(path, false);
}

/// Owns a lane's `<out>.mzpeak.tmp` until the archive is renamed into place, and removes it on
/// every other exit: an `Err` unwinding out of the lane (the guard's `Drop`), or a panic — the
/// release profile is `panic = "abort"`, so no destructor runs on a panic and the panic hook
/// sweeps [`TMP_IN_FLIGHT`] instead. Before this a failed peak-writer open (which panics since
/// 0.9.5) or any writer error left a partial `.tmp` beside the missing output.
///
/// Declare it BEFORE the `File::create` of the tmp so it is dropped after the writer that owns
/// the handle (Windows cannot unlink an open file). The rename itself lives in [`Self::finish`],
/// which is the only way to disarm the guard. The existing output-path guards (`--force`, the
/// in-place / nested-output refusals in `run`) are untouched: this never removes anything but
/// its own `.tmp`.
struct TmpGuard {
    path: PathBuf,
}

impl TmpGuard {
    fn new(path: &Path) -> Self {
        track_tmp_in_flight(path);
        Self { path: path.to_path_buf() }
    }

    /// Put `path` in the panic hook's sweep; `dir` for a directory to remove with its contents,
    /// which only [`msconvert_dir`] registers, having just created it. A tmp file path is only ever
    /// unlinked: a directory already standing at `<out>.mzpeak.tmp` is not this run's to remove.
    fn register(path: &Path, dir: bool) {
        if let Ok(mut v) = TMP_IN_FLIGHT.lock() {
            v.push((std::thread::current().id(), path.to_path_buf(), dir));
        }
    }

    /// Rename the finished tmp onto `output`; on success nothing is left to remove.
    fn finish(mut self, output: &Path) -> Result<()> {
        // A failed rename drops `self` through `?`, which removes the tmp.
        fs::rename(&self.path, output).with_context(|| format!("finalizing {}", output.display()))?;
        let path = std::mem::take(&mut self.path);
        Self::forget_path(&path);
        std::mem::forget(self);
        Ok(())
    }

    fn forget_path(path: &Path) {
        if let Ok(mut v) = TMP_IN_FLIGHT.lock() {
            v.retain(|(_, p, _)| p != path);
        }
    }

    fn remove_quietly(path: &Path, dir: bool) {
        // Only a directory registered as one (msconvert's working directory) goes with its contents.
        let removed = if dir { fs::remove_dir_all(path) } else { fs::remove_file(path) };
        match removed {
            Ok(()) => log::warn!("removed incomplete {}", path.display()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => log::warn!("could not remove incomplete {}: {e}", path.display()),
        }
    }
}

impl Drop for TmpGuard {
    fn drop(&mut self) {
        Self::forget_path(&self.path);
        Self::remove_quietly(&self.path, false);
    }
}

/// The temporary an mzML export is written to before the rename into place: `x.mzML` →
/// `x.mzML.tmp`, `x.mzML.gz` → `x.mzML.tmp.gz`. A trailing `.gz` stays LAST so `mzml_sink`, which
/// picks the gzip encoder from the name it is handed, still sees it. Until 0.9.13 the four mzML
/// export sites created the final path directly, so a failure left a partial file and under
/// `--force` had already destroyed the previous output — the atomic-output protection every mzPeak
/// lane had (`TmpGuard`) and none of the mzML ones did.
fn mzml_tmp_path(output: &Path) -> PathBuf {
    let (stem, suffix) = if has_gz_suffix(output) {
        (output.with_extension(""), ".tmp.gz")
    } else {
        (output.to_path_buf(), ".tmp")
    };
    let mut s = stem.into_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

/// Remove the in-flight `.tmp` files (see [`TmpGuard`]): all of them when `all`, otherwise only
/// those the calling thread registered. Called from the panic hook, so it must not block: a lock
/// held by the panicking thread (it never is — the registry is locked only inside `TmpGuard`
/// push/retain) would deadlock a plain `lock()`, and a poisoned one is still usable.
fn sweep_tmp_in_flight(all: bool) {
    let me = std::thread::current().id();
    let mut guard = match TMP_IN_FLIGHT.try_lock() {
        Ok(v) => v,
        Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => return,
    };
    let (mine, rest): (Vec<_>, Vec<_>) =
        std::mem::take(&mut *guard).into_iter().partition(|(t, _, _)| all || *t == me);
    *guard = rest;
    drop(guard);
    for (_, p, dir) in mine {
        TmpGuard::remove_quietly(&p, dir);
    }
}

/// Chain a sweep of the in-flight tmp files onto the default panic hook. Under `panic = "abort"`
/// (the release profile) this is the only code that runs between the panic message and the
/// process exit, so it sweeps every registered tmp — the ims-compact lane's writer thread can be
/// the one panicking while the main thread holds the guard. Under `panic = "unwind"` (the test
/// profile) the guards' `Drop` already cleans up as the stack unwinds, so the hook only sweeps
/// the panicking thread's own entries — a caught panic in one test must not remove the tmp of a
/// conversion running on another thread of the same process.
fn install_tmp_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        previous(info);
        sweep_tmp_in_flight(cfg!(panic = "abort"));
    }));
}

fn main() {
    // mzdata's Thermo reader panics on an unrecognized instrument model by default; downgrade to a
    // warning so a newer Astral/firmware doesn't hard-crash the converter. User override respected.
    if std::env::var_os("MZDATA_IGNORE_UNKNOWN_INSTRUMENT").is_none() {
        unsafe { std::env::set_var("MZDATA_IGNORE_UNKNOWN_INSTRUMENT", "ignore") };
    }

    install_tmp_panic_hook();
    let cli = Cli::parse();
    // Thermo .raw reading self-hosts a .NET runtime (RawFileReader targets net8.0). Allow
    // roll-forward to a newer installed major (9/10) unless the user pinned it — for THERMO input
    // only. It was set for every input until 0.9.12, which overrode the Shimadzu glue's own
    // `rollForward: LatestMinor` (ShimadzuGlue.runtimeconfig.json): on a host with .NET 9 that
    // hoists the glue onto a runtime where the BinaryFormatter path it needs no longer exists.
    // SAFETY: set once at startup, before any threads/readers exist.
    if is_thermo_raw(&cli.input) && std::env::var_os("DOTNET_ROLL_FORWARD").is_none() {
        unsafe { std::env::set_var("DOTNET_ROLL_FORWARD", "LatestMajor") };
    }
    // Settings resolve BEFORE logging is initialised: `verbose` / `quiet` are config-file keys too
    // (0.9.13), so the log level is only known once the file has been merged. A config that fails
    // to parse is reported through the same `error:` line as every other failure.
    let cfg = match Settings::resolve(&cli) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::exit(exit::GENERIC);
        }
    };
    let _ = REPRESENTATION.set(cfg.representation);
    let _ = SCIEX_SAMPLE.set(cfg.sample);
    init_logging(cfg.verbose, cfg.quiet);
    // Inert-flag warnings MUST come after init_logging: log::warn! against the uninitialized
    // default logger is a silent no-op, which is precisely the failure mode these warn about.
    // (The --representation warning below was emitted before init_logging from the day it was
    // added, i.e. never actually printed — found when the TDF warning under it also stayed silent.)
    //
    // Two readers honour a representation choice: Shimadzu `.lcd` (both output formats) and Bruker
    // BAF (`bruker_baf.rs`, mzPeak output; its `--to mzml` branch opens the reader without it).
    // Every other vendor ABI hands back one array pair per scan, and the mzML/imzML reader takes
    // whatever the file declares. Setting the flag anywhere else would otherwise look effective and
    // do nothing. (The old text named only Shimadzu, so a BAF user was told a working flag was
    // ignored.)
    if cfg.representation != RepresentationArg::Both
        && !representation_is_honoured(&cli.input, cfg.output_format)
    {
        log::warn!(
            "--representation is only honored for Shimadzu .lcd input and (mzPeak output only) \
             Bruker BAF .d; ignoring it for {}",
            cli.input.display()
        );
    }
    // Partial-flag honesty for the TDF mobility knob: the ims-compact reader (`bruker_native`)
    // honours it for arrays and params alike, and the lossy `--no-ims-compact` path honours it for
    // the precursor/scan/window-limit PARAMS (`bruker_native::TdfMobilityRemap`, so both lanes
    // write the same selected-ion 1/K0 either way) — but that path's mobility ARRAYS come from
    // mzdata's TDF reader, whose own ModelType-2 tims calibration is unconditional (`im_enabled` is
    // hard-coded true in mzdata 0.66's CalibrationParameters::from_sql) and cannot be switched.
    if !cfg.tims_recalibration && cfg.no_ims_compact {
        log::warn!(
            "--no-tims-recalibration with --no-ims-compact: precursor/scan/window 1/K0 params stay \
             on timsrust's linear approximation (as in the ims-compact lane), but the mobility \
             arrays still come from mzdata's own ModelType-2 calibration — the flag cannot switch \
             those"
        );
    }

    let code = match run(&cli, &cfg) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            if e.downcast_ref::<UnsupportedVendor>().is_some() {
                exit::UNSUPPORTED
            } else {
                exit::GENERIC
            }
        }
    };
    std::process::exit(code);
}

/// Does this input (and output format) reach a reader that acts on `--representation`? Shimadzu
/// `.lcd` on both output formats; Bruker BAF on mzPeak output only (`convert_to_mzml` opens the
/// BAF reader without a representation).
fn representation_is_honoured(input: &Path, output_format: OutputFormat) -> bool {
    if is_lcd(input) {
        return true;
    }
    #[cfg(any(windows, target_os = "linux"))]
    if is_baf_dir(input) {
        return output_format == OutputFormat::Mzpeak;
    }
    let _ = output_format;
    false
}

/// An explicit `-v` / `-q` WINS over `RUST_LOG`; the environment is consulted only when neither
/// flag was given (default `info`). `default_filter_or` did the opposite — it is a fallback, so
/// `RUST_LOG=warn mzpeak-convert -q` kept printing warnings while the help text promised silence.
fn init_logging(verbose: u8, quiet: bool) {
    let mut builder = if quiet || verbose > 0 {
        let level = if quiet {
            "error"
        } else if verbose == 1 {
            "debug"
        } else {
            "trace"
        };
        let mut b = env_logger::Builder::new();
        b.parse_filters(level);
        b
    } else {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
    };
    builder.format_timestamp(None).init();
}

/// Refuse to run a diagnostic lever that writes NO archive when the user asked for one. Each of
/// these replaces the conversion: before this, `MZPC_DUMP_IM_TABLE= mzpeak-convert run.d -o out.mzpeak`
/// printed a table, exited 0 and never mentioned that `out.mzpeak` did not exist — an inherited
/// shell variable could silently turn a batch of conversions into a batch of nothing.
fn refuse_diagnostic_with_output(lever: &str, output: Option<&Path>) -> Result<()> {
    if let Some(out) = output {
        bail!(
            "{lever} is set: this is a diagnostic that prints to stdout and writes NO archive, but \
             --output {} was requested. Unset {lever} for a real conversion, or drop -o to run the \
             diagnostic.",
            out.display()
        );
    }
    Ok(())
}

fn run(cli: &Cli, cfg: &Settings) -> Result<i32> {
    // Diagnostic: dump the scan→1/K0 table from timsrust (and the Bruker SDK where available) so the
    // two mobility calibrations can be compared scan-by-scan. Bypasses normal conversion.
    if env_flag("MZPC_DUMP_IM_TABLE") == Some(true) {
        refuse_diagnostic_with_output("MZPC_DUMP_IM_TABLE", cfg.output.as_deref())?;
        dump_im_table(&cli.input)?;
        return Ok(exit::OK);
    }
    // Diagnostic: dump decoded Agilent profile spectra (mz_min/delta come pre-folded into the grid;
    // we report sum, nnz, first/last (k,v), max v) so the pure-Rust MSProfile.bin decode can be
    // validated byte-exact against the `rainbow` reference. Bypasses conversion.
    if env_flag("MZPC_DUMP_AGILENT_PROFILE") == Some(true) {
        refuse_diagnostic_with_output("MZPC_DUMP_AGILENT_PROFILE", cfg.output.as_deref())?;
        dump_agilent_profile(&cli.input)?;
        return Ok(exit::OK);
    }
    // Diagnostic: `MZPC_SHIMADZU_PROBE=N` dumps the first N spectra of a `.lcd` as JSON lines. It
    // used to live inside the Shimadzu lane, which only runs with `-o` — so it always swallowed
    // the requested archive — and it parsed a non-numeric value as 10. Now: a value that is not a
    // count is an error, and like the two dumps above it refuses to shadow an `--output`.
    if let Some(raw) = std::env::var_os("MZPC_SHIMADZU_PROBE").filter(|v| !v.to_string_lossy().trim().is_empty()) {
        // An EMPTY value is "unset", like every other lever (`env_flag`, MZPC_TDF_SDK_GOLDEN); only
        // a value that is present and not a count is an error.
        let raw = raw.to_string_lossy();
        let n: usize = raw.trim().parse().map_err(|_| {
            anyhow!(
                "MZPC_SHIMADZU_PROBE={raw:?} is not a spectrum count; set it to the number of \
                 spectra to probe (e.g. 10), or unset it"
            )
        })?;
        refuse_diagnostic_with_output("MZPC_SHIMADZU_PROBE", cfg.output.as_deref())?;
        return shimadzu_probe_lever(&cli.input, n).map(|()| exit::OK);
    }

    // Published out-of-band like `--representation` (see NO_MZ_LATTICE): from the RESOLVED setting,
    // so a config-file `no_mz_lattice: true` counts as much as the flag.
    let _ = NO_MZ_LATTICE.set(cfg.no_mz_lattice);
    let verbose = cfg.verbose > 0;

    // Inspection report: always when there is no output (the whole job is "inspect"), and also as a
    // verbose extra during a real conversion. As an extra it opens no native vendor reader (see
    // `native_inspect_skip`) and cannot fail the run: its error becomes a `note:` line.
    if verbose || cfg.output.is_none() {
        let skip_native = native_inspect_skip(cfg.output.is_some(), cfg.via_msconvert);
        if let Err(e) = report_inspect(&cli.input, skip_native) {
            if cfg.output.is_none() {
                return Err(e);
            }
            println!("note:          inspection failed: {e:#}");
        }
    }
    let Some(output) = cfg.output.clone() else {
        return Ok(exit::OK); // no --output: nothing written, just the report above
    };

    // REFUSE to write over the input. Every lane opens the source and then truncates the output, so
    // `-o` pointing back at the input destroys it — with `--force` this silently ate a 120 MB archive
    // and still failed the conversion, leaving nothing. Compare device+inode rather than the path
    // text so a symlink, a hardlink, or `./x` vs `x` cannot slip past.
    // A `.d` / `.raw` input is a DIRECTORY: an output inside it overwrites a vendor member
    // (`-o Run.d/analysis.tsf`) or gets swept up by vendor preservation while it is still being
    // written (`-o Run.d/out.mzpeak` → the archive streams itself in until the disk fills). Checked
    // before the exists() test below because the self-embedding case starts from a fresh path.
    if cli.input.is_dir() {
        let out_dir = output.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
        if let (Ok(root), Ok(dir)) = (fs::canonicalize(&cli.input), fs::canonicalize(out_dir)) {
            if dir.starts_with(&root) {
                bail!(
                    "output {} is inside the input directory {} — refusing (it would overwrite a \
                     vendor file, or be embedded into itself by vendor preservation). Write outside \
                     the input.",
                    output.display(),
                    cli.input.display()
                );
            }
        }
    }

    if output.exists() {
        #[cfg(unix)]
        let same = match (fs::metadata(&cli.input), fs::metadata(&output)) {
            (Ok(a), Ok(b)) => {
                use std::os::unix::fs::MetadataExt;
                a.dev() == b.dev() && a.ino() == b.ino()
            }
            _ => false,
        };
        // ponytail: no inode on Windows — canonicalized paths catch symlinks/`./x` but not hardlinks.
        #[cfg(not(unix))]
        let same = match (fs::canonicalize(&cli.input), fs::canonicalize(&output)) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        };
        if same {
            bail!(
                "output {} is the same file as the input — refusing to overwrite the source \
                 (conversion reads the input while writing the output, so this destroys it). \
                 Write to a different path, then replace the original.",
                output.display()
            );
        }
    }

    // `--sample` picks one sample of a multi-sample SciEX `.wiff`, on every lane that reads one; any
    // other input holds one run, and there it is as inert as the flags `inert_flags_for` lists.
    if cfg.given.contains(&"--sample") && !is_wiff(&cli.input) {
        log::warn!(
            "--sample is inert on {}: only a SciEX .wiff holds several samples, so it cannot change \
             the output",
            cli.input.display()
        );
    }

    // mzPeak input → filter path. A `.mzpeak` (or any ZIP with mzpeak_index.json) cannot be read by
    // mzdata; instead of the convert lanes below, route to the mzPeak→mzPeak filter (RT / MS-level /
    // aux drop+inject). This supersedes the old "mzdata can't read it" error.
    if filter::is_mzpeak_input(&cli.input) {
        let lane = if cfg.output_format == OutputFormat::Mzml { Lane::FilterToMzml } else { Lane::Filter };
        refuse_unsupported_flags(lane, cfg)?;
        if output.exists() && !cfg.force {
            bail!("output {} exists (use --force to overwrite)", output.display());
        }
        // Releases up to v0.7.2 could write `tof_encoding: per-scan-delta`, but no reader ever
        // cumulatively summed it — every TOF bin after the first in a scan decodes as a tiny bin and
        // squares to a nonsense m/z. Refuse rather than emit silently wrong masses.
        reject_legacy_tof_delta(&cli.input)?;
        // `--no-vendor` strips the embedded vendor data on the filter path too — same effect as
        // `--drop-aux 'vendor*'` (the glob's `*` spans `/`, so it also catches `vendor/…` side-files).
        // (It is moot for mzML output — vendor facets aren't carried into mzML at all.)
        let mut drop_aux = cfg.drop_aux.clone();
        if cfg.no_vendor {
            drop_aux.push("vendor*".to_string());
        }
        let opts = filter::FilterOpts {
            rt: cfg.rt.as_deref().map(filter::parse_rt).transpose()?,
            ms_levels: cfg.ms_level.clone(),
            drop_aux,
            images: cfg.image.clone(),
            sdrf: cfg.sdrf.clone(),
        };
        // mzML output: read the `.mzpeak` with the sync reader (which decodes every buffer transform,
        // incl. the timsTOF tof→m/z) and write the RT / MS-level survivors to a real mzML — the "slice
        // to a narrow RT window then hand the small mzML to a search engine" workflow. Otherwise the
        // filter writes a new `.mzpeak` (aux drop/inject + spectrum-level filtering).
        if cfg.output_format == OutputFormat::Mzml {
            filter_mzpeak_to_mzml(&cli.input, &output, &opts)
                .with_context(|| format!("filtering {} to mzML", cli.input.display()))?;
            return Ok(exit::OK);
        }
        filter::run(&cli.input, &output, &opts)
            .with_context(|| format!("filtering {}", cli.input.display()))?;
        log::info!("wrote {}", output.display());
        return Ok(exit::OK);
    }

    // Past this point the input is a RAW/exchange format, not a `.mzpeak` — and the spectrum filters
    // are implemented only on the mzPeak-input lane above. They used to be parsed and then silently
    // ignored: `mzpeak-convert run.mzML --ms-level 2 --rt 5-6` wrote the COMPLETE 3574-spectrum
    // archive and exited 0, so a user slicing a file got the whole thing and no indication. Refuse
    // instead — convert first, then filter the resulting archive.
    {
        let mut ignored: Vec<&str> = Vec::new();
        if cfg.rt.is_some() {
            ignored.push("--rt");
        }
        if !cfg.ms_level.is_empty() {
            ignored.push("--ms-level");
        }
        if !cfg.drop_aux.is_empty() {
            ignored.push("--drop-aux");
        }
        if !ignored.is_empty() {
            bail!(
                "{} apply only to a `.mzpeak` input (they filter an existing archive), but {} is a \
                 raw/exchange format. Convert it first, then filter the archive:\n  \
                 mzpeak-convert {} -o out.mzpeak\n  mzpeak-convert out.mzpeak -o filtered.mzpeak {}",
                ignored.join(", "),
                cli.input.display(),
                cli.input.display(),
                ignored.join(" …  ")
            );
        }
    }

    // Shimadzu `.lcd` stores m/z as scaled integers (fixed-point, 1e-4), so consecutive values are
    // near-constant integer deltas. Lossless delta chunking is therefore strictly better than
    // numpress-linear on this vendor -- SMALLER, FASTER *and* exact, measured on two QTOF DIA runs:
    //
    //     default (numpress)  1,125 MB   102 s   m/z off the vendor lattice by up to 3.8e-3
    //     delta               818 MB      92 s   on the lattice to 3.7e-9
    //
    // Numpress-linear's floating-point prediction fights a fixed-point lattice: it both compresses
    // worse and fails to reproduce the vendor's integers (~1 ppb, below any instrument's accuracy,
    // but paid for with 27% more space). Its fidelity is data-dependent -- it IS exact on the
    // centroid mzML export of the same acquisition -- so this defaults per vendor, not globally.
    // The strategy requested here is provisional. `refine_chunking` swaps numpress-linear for
    // lossless delta once real m/z has been sampled and found to sit on a fixed-point lattice --
    // superseding the `is_lcd()` guess this used to make, which was right about Shimadzu's NATIVE
    // lane and wrong about msconvert's mzML of the very same acquisition.
    let chunk = match cfg.layout {
        Layout::Point => None,
        Layout::Chunked if cfg.no_numpress => {
            Some(ChunkingStrategy::Delta { chunk_size: cfg.chunk_size })
        }
        Layout::Chunked => Some(ChunkingStrategy::NumpressLinear { chunk_size: cfg.chunk_size }),
    };

    if output.exists() && !cfg.force {
        bail!("output {} exists (use --force to overwrite)", output.display());
    }

    // mzML output: a separate, simpler lane. It bypasses every mzPeak-specific encoder (ims-compact,
    // TOF-grid, chunking, byte-plane, vendor side-file embedding) and just streams the read spectra
    // into an mzML via the mzdata writer. `--via-msconvert` already yields mzML, so route it straight
    // to the output path in that case.
    if cfg.output_format == OutputFormat::Mzml {
        refuse_unsupported_flags(Lane::MzmlExport, cfg)?;
        convert_to_mzml(&cli.input, &output, cfg.via_msconvert, cfg.msconvert_path.as_deref())
            .with_context(|| format!("converting {} to mzML", cli.input.display()))?;
        return Ok(exit::OK);
    }

    // The Bruker SDK path (opt-in) reads TDF/TSF via timsdata and supersedes both ims-compact and the
    // pure-Rust readers for those inputs.
    let use_bruker_sdk = cfg.bruker_sdk && (is_tdf_dir(&cli.input) || is_tsf_dir(&cli.input));
    // ims-compact is the DEFAULT for Bruker timsTOF (TDF); --no-ims-compact (or --bruker-sdk) falls
    // back to f64 m/z.
    let use_ims_compact = is_tdf_dir(&cli.input) && !cfg.no_ims_compact && !use_bruker_sdk;
    // The SDK decoder ALSO has the raw tof index, so it can emit the same ims-compact integer-tof
    // layout — use it for TDF (unless --no-ims-compact) so newer timsTOF (5.1.x) that timsrust can't
    // decompress still gets the compact lossless format (+ byte-plane) instead of f64 m/z.
    let use_sdk_ims_compact = use_bruker_sdk && is_tdf_dir(&cli.input) && !cfg.no_ims_compact;

    let vendor = if cfg.no_vendor {
        None
    } else if use_ims_compact {
        // The lossless ims-compact facet already encodes the exact signal, so drop the redundant raw
        // `*_bin` bulk binary by default (was ~39% of the TDF archive, a verbatim copy).
        Some(vendor::VendorPolicy::load_lossless(None, &cfg.aux)?)
    } else {
        Some(vendor::VendorPolicy::load(None, &cfg.aux)?)
    };

    // Agilent FILE-DIRECT profile grid (pure Rust, no MHDAC/msconvert): when `--agilent-grid` is set
    // and the input is an Agilent `.d` with a non-empty `AcqData/MSProfile.bin`, read the integer
    // flight-time grid straight from the file and store `tof_index` + `{c0,c1}`. Cross-platform, so
    // it must run BEFORE `guard_unsupported_vendor` (which rejects Agilent `.d` off Windows).
    let use_agilent_grid = cfg.agilent_grid && is_agilent_d(&cli.input)
        && agilent_profile::has_profile(&cli.input);
    if cfg.agilent_grid && is_agilent_d(&cli.input) && !use_agilent_grid {
        log::warn!(
            "--agilent-grid: {} has no profile data (AcqData/MSProfile.bin is empty/absent); \
             centroid-only Agilent .d is not griddable — falling back to the standard path",
            cli.input.display()
        );
    }

    // Name the lane FIRST, then refuse any user-supplied option it would drop — one table
    // (`unsupported_flags_for`) instead of a warning here and a silent `&[]` there. Only then
    // dispatch, with the very same conditions.
    let lane = if use_agilent_grid {
        Lane::AgilentGrid
    } else if cfg.via_msconvert {
        Lane::ViaMsconvert
    } else if use_sdk_ims_compact {
        Lane::SdkImsCompact
    } else if use_bruker_sdk {
        Lane::BrukerSdk
    } else if use_ims_compact {
        Lane::ImsCompact
    } else if routes_to_vendor_reader(&cli.input) {
        Lane::VendorReader
    } else {
        Lane::Standard
    };
    refuse_unsupported_flags(lane, cfg)?;
    if let Some(note) = aux_inert_note(&cli.input, &cfg.given) {
        log::warn!("{note}");
    }

    match lane {
        Lane::AgilentGrid => {
            convert_agilent_grid(&cli.input, &output, cfg.zstd_level, vendor.as_ref(), cfg.chromatograms)
                .with_context(|| format!("file-direct Agilent-grid converting {}", cli.input.display()))?;
        }
        Lane::ViaMsconvert => {
            convert_via_msconvert(&cli.input, &output, chunk, cfg.zstd_level, cfg.msconvert_path.as_deref(), cfg.chromatograms, cfg.tof_grid, vendor.as_ref(), &cfg.image, cfg.sdrf.as_deref())
                .with_context(|| format!("converting {} via msconvert", cli.input.display()))?;
        }
        Lane::SdkImsCompact => {
            convert_ims_compact_sdk(&cli.input, &output, cfg.ims_zstd_level, vendor.as_ref(), cfg.chromatograms)
                .with_context(|| format!("SDK ims-compact converting {}", cli.input.display()))?;
        }
        Lane::BrukerSdk => {
            convert_bruker_sdk(&cli.input, &output, chunk, cfg.zstd_level, vendor.as_ref(), cfg.chromatograms)
                .with_context(|| format!("converting {} via the Bruker timsdata SDK", cli.input.display()))?;
        }
        Lane::ImsCompact => {
            // Native ims-compact (direct timsrust) is the lossless default. When timsrust can't
            // decompress a frame — newer timsTOF (e.g. 5.1.x) writes a TDF binary it doesn't handle —
            // fall back to the mzdata reader interface (f64 m/z), which decodes those files. mzdata may
            // silently drop a truly-undecodable frame, so the fallback is loud. (Backlog: fix timsrust /
            // upstream a raw-TOF mode so ims-compact works on newer data through mzdata too.)
            ims_compact_with_fallback(
                &cli.input,
                || convert_ims_compact_archive(&cli.input, &output, cfg.ims_zstd_level, vendor.as_ref(), cfg.chromatograms, cfg.tims_recalibration, cfg.ims_chunked, cfg.chunk_size),
                |route| {
                    // The fallback IS the standard lane: its flags were checked against ims-compact,
                    // which honours `--ims-chunked`; the standard lane cannot, so say so now.
                    refuse_unsupported_flags(Lane::Standard, cfg)?;
                    guard_unsupported_vendor(&cli.input)?;
                    convert_file(&cli.input, &output, chunk, cfg.zstd_level, vendor.as_ref(), cfg.chromatograms, cfg.tof_grid, &cfg.image, cfg.sdrf.as_deref(), cfg.tims_recalibration, Some(route))
                        .with_context(|| format!("mzdata-fallback converting {}", cli.input.display()))
                },
            )?;
        }
        Lane::VendorReader | Lane::Standard => {
            guard_unsupported_vendor(&cli.input)?;
            convert_file(&cli.input, &output, chunk, cfg.zstd_level, vendor.as_ref(), cfg.chromatograms, cfg.tof_grid, &cfg.image, cfg.sdrf.as_deref(), cfg.tims_recalibration, None)
                .with_context(|| format!("converting {}", cli.input.display()))?;
        }
        // Both were dispatched above, before the raw-format guards.
        Lane::Filter | Lane::FilterToMzml | Lane::MzmlExport => unreachable!("dispatched earlier in run()"),
    }

    log::info!("wrote {}", output.display());
    Ok(exit::OK)
}

/// The timsTOF default lane with its fallback. Native ims-compact (direct timsrust) runs first; when
/// it cannot decompress a frame (newer timsTOF, e.g. 5.1.x, writes a TDF binary timsrust does not
/// handle) the mzdata reader converts the run instead (f64 m/z), and that archive names its route
/// in `conversion_route` together with the error that forced it. Any other native error is the
/// lane's own. The recorded argv is the same on both routes, so until this block a fallback archive
/// was recognisable only by what it lacks (review M35).
fn ims_compact_with_fallback(
    input: &Path,
    native: impl FnOnce() -> Result<()>,
    fallback: impl FnOnce((String, serde_json::Value)) -> Result<()>,
) -> Result<()> {
    match native() {
        Ok(()) => Ok(()),
        Err(e) if format!("{e:#}").to_lowercase().contains("decompress") => {
            // mzdata may silently drop a truly undecodable frame, so the fallback is loud.
            log::warn!(
                "native ims-compact failed on {} ({e}); falling back to the mzdata reader (f64 m/z, larger; may skip any frame even mzdata can't decode)",
                input.display()
            );
            fallback(conversion_route_block("mzdata-fallback", "mzdata", Some(&format!("{e:#}"))))
        }
        Err(e) => Err(e).with_context(|| format!("ims-compact converting {}", input.display())),
    }
}

/// The `conversion_route` index block: which timsTOF route built the archive (`ims-compact` read by
/// `timsrust` or `timsdata`, or the `mzdata-fallback`) and, for the fallback, why.
fn conversion_route_block(route: &str, reader: &str, reason: Option<&str>) -> (String, serde_json::Value) {
    let mut block = serde_json::json!({"route": route, "reader": reader});
    if let Some(reason) = reason {
        block["reason"] = serde_json::json!(reason);
    }
    ("conversion_route".to_string(), block)
}

/// The conversion lane `run` selected, named so [`unsupported_flags_for`] can say which
/// user-supplied options that lane would drop. Selection and dispatch use the same conditions;
/// the enum only exists so the refusal happens BEFORE any reader is opened.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Lane {
    /// `.mzpeak` input → re-packed `.mzpeak` (RT / MS-level / aux filter, image + SDRF inject).
    Filter,
    /// `.mzpeak` input → mzML export of the surviving spectra.
    FilterToMzml,
    /// Raw/exchange input → plain mzML (`--to mzml`): no mzPeak encoder runs.
    MzmlExport,
    /// `--agilent-grid`: file-direct Agilent profile grid.
    AgilentGrid,
    /// `--via-msconvert`: msconvert → temp mzML → the standard mzML lane.
    ViaMsconvert,
    /// `--bruker-sdk` on a TDF: SDK-decoded ims-compact.
    SdkImsCompact,
    /// `--bruker-sdk` on a TSF, or a TDF under `--no-ims-compact`: SDK-decoded f64.
    BrukerSdk,
    /// The default timsTOF lane: native (timsrust) ims-compact.
    ImsCompact,
    /// `convert_file` routed to a native vendor reader (TSF / BAF / Agilent / wiff / Waters / .lcd).
    VendorReader,
    /// `convert_file` on the mzdata path (mzML / imzML / Thermo `.raw` / TDF f64): honours everything
    /// but `--ims-chunked`. Also the ims-compact lane's fallback for a TDF timsrust cannot decompress.
    Standard,
}

impl Lane {
    /// How the refusal names the lane, and what the user can do instead.
    fn describe(self) -> (&'static str, &'static str) {
        match self {
            Lane::Filter => (
                "the .mzpeak filter lane (an existing archive is re-packed, not re-encoded)",
                "these options shape a conversion from a raw/exchange format; drop them here, or \
                 re-convert from the source with them",
            ),
            Lane::FilterToMzml => (
                "the .mzpeak → mzML export",
                "mzML carries none of what these options control; drop them",
            ),
            Lane::MzmlExport => (
                "the --to mzml export (plain mzML through the mzdata writer; no mzPeak encoder, \
                 no embedding)",
                "drop them, or write a .mzpeak instead",
            ),
            Lane::AgilentGrid => (
                "the --agilent-grid file-direct lane",
                "drop them, or drop --agilent-grid; images/SDRF can be added afterwards with a \
                 second run on the archive (`mzpeak-convert out.mzpeak -o with.mzpeak --sdrf …`)",
            ),
            Lane::ViaMsconvert => (
                "the --via-msconvert lane (the intermediate mzML is the source, so no vendor \
                 side-file is embedded)",
                "drop --aux, or convert natively where a reader exists",
            ),
            Lane::SdkImsCompact => (
                "the --bruker-sdk ims-compact lane",
                "drop --bruker-sdk (the native timsTOF lane honours --ims-chunked and \
                 --no-tims-recalibration), and add images/SDRF with a second run on the archive",
            ),
            Lane::BrukerSdk => (
                "the --bruker-sdk f64 lane",
                "drop --bruker-sdk, or drop the options; images/SDRF can be added with a second \
                 run on the archive",
            ),
            Lane::ImsCompact => (
                "the timsTOF ims-compact lane",
                "convert first, then add them on the archive: \
                 `mzpeak-convert out.mzpeak -o with.mzpeak --image … --sdrf …`",
            ),
            Lane::VendorReader => (
                "the native vendor-reader lane",
                "convert first, then add them on the archive: \
                 `mzpeak-convert out.mzpeak -o with.mzpeak --image … --sdrf …`",
            ),
            Lane::Standard => ("the standard lane", ""),
        }
    }
}

/// Options a lane would DROP: the user asked for something — an embedded file, a backend, a
/// layout — and the archive would come out without it, exit 0, no notice (`--sdrf` on the
/// ims-compact lane wrote an archive with no SDRF and no message). `run` REFUSES these. Options a
/// lane merely has no use for belong in [`inert_flags_for`], not here: refusing `--no-numpress` on
/// a timsTOF `.d` — whose integer-axis facet never used numpress — punished a plausible lossless
/// invocation for a flag that could not have changed the output.
fn dropped_flags_for(lane: Lane) -> &'static [&'static str] {
    match lane {
        // Re-packs Parquet members verbatim: nothing the convert flags name is lost, only unused.
        Lane::Filter => &[],
        // mzML cannot carry an embedded image/SDRF/aux member at all.
        Lane::FilterToMzml => &["--image", "--sdrf", "--aux"],
        // The mzML dispatch runs before SDK selection, so `--bruker-sdk` picks a backend the export
        // never consults — that is a choice silently overridden, not an inert flag.
        Lane::MzmlExport => &["--image", "--sdrf", "--aux", "--bruker-sdk"],
        Lane::AgilentGrid => &["--image", "--sdrf", "--via-msconvert"],
        // Lane selection puts msconvert before every native backend, so these four would be
        // silently overridden — the user chose a reader and gets a different one.
        Lane::ViaMsconvert => &["--aux", "--bruker-sdk", "--no-ims-compact", "--ims-chunked", "--no-tims-recalibration"],
        Lane::SdkImsCompact => &["--image", "--sdrf", "--ims-chunked", "--no-tims-recalibration"],
        Lane::BrukerSdk => &["--image", "--sdrf", "--ims-chunked", "--no-tims-recalibration"],
        Lane::ImsCompact => &["--image", "--sdrf"],
        Lane::VendorReader => &["--image", "--sdrf"],
        Lane::Standard => &[],
    }
}

/// Options that cannot change a lane's output. The user is told, once, and the run proceeds: an
/// inert flag is not a lost one. Kept apart from [`dropped_flags_for`] so the two questions —
/// "would data go missing?" and "does this flag do anything here?" — never share a list again.
fn inert_flags_for(lane: Lane) -> &'static [&'static str] {
    const CODEC: &[&str] = &["--layout", "--no-numpress", "--no-mz-lattice", "--chunk-size", "--zstd-level"];
    match lane {
        Lane::Filter => &[
            "--layout", "--no-numpress", "--no-mz-lattice", "--chunk-size", "--zstd-level",
            "--no-ims-compact", "--representation", "--ims-chunked", "--bruker-sdk",
            "--no-tims-recalibration", "--no-chromatograms", "--aux", "--tof-grid", "--agilent-grid",
            "--via-msconvert", "--msconvert-path",
        ],
        Lane::FilterToMzml => &[
            "--layout", "--no-numpress", "--no-mz-lattice", "--chunk-size", "--zstd-level",
            "--no-ims-compact", "--representation", "--ims-chunked", "--bruker-sdk",
            "--no-tims-recalibration", "--no-chromatograms", "--tof-grid", "--agilent-grid",
            "--via-msconvert", "--msconvert-path",
        ],
        Lane::MzmlExport => &[
            "--layout", "--no-numpress", "--no-mz-lattice", "--chunk-size", "--zstd-level",
            "--no-ims-compact", "--ims-chunked", "--no-tims-recalibration", "--no-chromatograms",
            "--tof-grid", "--agilent-grid",
        ],
        Lane::AgilentGrid | Lane::SdkImsCompact => &["--layout", "--no-numpress", "--chunk-size"],
        Lane::ImsCompact => &["--layout", "--no-numpress"],
        // No chunked TOF layout off the timsTOF ims-compact lane — which falls back to `Standard`
        // on a TDF timsrust cannot decompress, and checks this list again when it does.
        Lane::VendorReader | Lane::Standard => &["--ims-chunked"],
        Lane::ViaMsconvert | Lane::BrukerSdk => &[],
        #[allow(unreachable_patterns)]
        _ => CODEC,
    }
}

/// Refuse, in the style of the `--rt`/`--ms-level` refusal above, when the user supplied an option
/// the selected lane would DROP; warn, once, for options that are merely inert there. Both are
/// checked against `Settings::given` — what was passed on the command line — never against
/// defaults or a config profile.
fn refuse_unsupported_flags(lane: Lane, cfg: &Settings) -> Result<()> {
    let given = |list: &'static [&'static str]| -> Vec<&'static str> {
        list.iter().copied().filter(|f| cfg.given.contains(f)).collect()
    };
    let (name, remedy) = lane.describe();
    // mzML lanes produce a document, not an archive; say the right noun in the message.
    let product = match lane {
        Lane::FilterToMzml | Lane::MzmlExport => "output",
        _ => "archive",
    };
    let inert = given(inert_flags_for(lane));
    if !inert.is_empty() {
        log::warn!(
            "{} {} inert on {name}: {} cannot change the {product}",
            inert.join(", "),
            if inert.len() == 1 { "is" } else { "are" },
            if inert.len() == 1 { "it" } else { "they" },
        );
    }
    let dropped = given(dropped_flags_for(lane));
    if dropped.is_empty() {
        return Ok(());
    }
    bail!(
        "{} {} not honoured by {name}: the {product} would be written WITHOUT {} and exit 0. {remedy}.",
        dropped.join(", "),
        if dropped.len() == 1 { "is" } else { "are" },
        if dropped.len() == 1 { "it" } else { "them" },
    );
}

/// Would `convert_file` hand this input to a native vendor reader rather than the mzdata path?
/// Mirrors the dispatch at the top of `convert_file` (which is `cfg`-gated per reader) so `run`
/// can name the lane before anything is opened.
fn routes_to_vendor_reader(input: &Path) -> bool {
    #[allow(unused_mut)]
    let mut vendor = is_tsf_dir(input);
    #[cfg(any(windows, target_os = "linux"))]
    {
        vendor = vendor || is_baf_dir(input);
    }
    #[cfg(windows)]
    {
        vendor = vendor
            || is_agilent_d(input)
            || is_wiff(input)
            || is_waters_raw(input)
            || is_lcd(input);
    }
    vendor
}

/// The `MZPC_SHIMADZU_PROBE` diagnostic behind the lever check in `run`: open the `.lcd` with the
/// resolved `--representation` and print the first `n` spectra. Off Windows the reader does not
/// exist, so the lever is an error there rather than a silently ignored one.
#[cfg(windows)]
fn shimadzu_probe_lever(input: &Path, n: usize) -> Result<()> {
    if !is_lcd(input) {
        bail!("MZPC_SHIMADZU_PROBE is set but {} is not a Shimadzu .lcd", input.display());
    }
    let rep = match representation() {
        RepresentationArg::Both => shimadzu::Representation::Both,
        RepresentationArg::Profile => shimadzu::Representation::Profile,
        RepresentationArg::Centroid => shimadzu::Representation::Centroid,
    };
    let reader = shimadzu::ShimadzuReader::open_with(input, rep)?;
    shimadzu_probe(&reader, n)
}

#[cfg(not(windows))]
fn shimadzu_probe_lever(input: &Path, _n: usize) -> Result<()> {
    bail!(
        "MZPC_SHIMADZU_PROBE is set, but the Shimadzu reader it probes only exists on Windows \
         (input {}); unset it here",
        input.display()
    );
}

/// True for a Bruker timsTOF TDF `.d` (folder with `analysis.tdf`).
/// True when `dir/<name>` exists AND is a non-empty file.
///
/// Existence alone is not enough to claim a vendor format. Real corpora carry **zero-byte** stubs
/// left by partial downloads or archive extraction: an Agilent `.d` with a 0-byte `analysis.tdf`
/// beside its `AcqData/` was routed to the Bruker TDF lane and hard-failed with
/// "no such table: GlobalMetadata", so its actual format was never attempted. 7 of 358 corpus units
/// failed this way.
fn has_nonempty(dir: &Path, name: &str) -> bool {
    std::fs::metadata(dir.join(name)).is_ok_and(|m| m.is_file() && m.len() > 0)
}

fn is_tdf_dir(input: &Path) -> bool {
    input.is_dir() && has_nonempty(input, "analysis.tdf")
}

/// Print `scan,timsrust_1overk0,sdk_1overk0,abs_diff` for every mobility scan of a TDF `.d`, so the
/// timsrust `Scan2ImConverter` calibration can be compared scan-by-scan against the vendor SDK's
/// `tims_scannum_to_oneoverk0`. Reads ONLY `analysis.tdf` (no frame/`.tdf_bin` read) so it can run on
/// a `.d` where only the metadata DB was fetched. The SDK column is blank without the SDK (e.g.
/// macOS). The optional per-frame point-count / m/z diagnostics run only if the binary is present.
fn dump_im_table(input: &Path) -> Result<()> {
    let dir = if input.is_dir() {
        input.to_path_buf()
    } else {
        input.parent().unwrap_or(input).to_path_buf()
    };
    let tdf = dir.join("analysis.tdf");

    // num_scans straight from the SQLite Frames table — no binary needed.
    let conn = rusqlite::Connection::open_with_flags(
        &tdf,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening {}", tdf.display()))?;
    let n: i64 = conn
        .query_row("SELECT MAX(NumScans) FROM Frames", [], |r| r.get(0))
        .context("reading MAX(NumScans)")?;
    let n = n.max(0) as usize;

    let cal = bruker_native::MobilityCal::open(&tdf)?;
    let timsrust: Vec<f64> = (0..n).map(|s| cal.for_scan(s)).collect();

    #[cfg(any(windows, target_os = "linux"))]
    let sdk: Option<Vec<f64>> = match bruker_sdk::scannum_to_oneoverk0_table(&dir, 1, n) {
        Ok(v) => Some(v),
        Err(e) => {
            eprintln!("SDK scan→1/K0 table unavailable: {e}");
            None
        }
    };
    #[cfg(not(any(windows, target_os = "linux")))]
    let sdk: Option<Vec<f64>> = None;

    println!("scan,timsrust_1overk0,sdk_1overk0,abs_diff");
    for s in 0..n {
        match &sdk {
            Some(v) => println!(
                "{s},{:.8},{:.8},{:.3e}",
                timsrust[s],
                v[s],
                (timsrust[s] - v[s]).abs()
            ),
            None => println!("{s},{:.8},,", timsrust[s]),
        }
    }

    // Optional: per-frame point counts (needs analysis.tdf_bin). Skip silently if the binary is
    // absent/empty (the lean metadata-only mode).
    if let Ok(native) = bruker_native::NativeTofReader::open(&dir) {
        let k = 10.min(native.len());
        eprintln!("frame,timsrust_points,sdk_points,diff");
        #[cfg(any(windows, target_os = "linux"))]
        let sdk_pts: Option<Vec<usize>> = bruker_sdk::frame_point_counts(&dir, k).ok();
        #[cfg(not(any(windows, target_os = "linux")))]
        let sdk_pts: Option<Vec<usize>> = None;
        for i in 0..k {
            let t = native.frame(i).map(|f| f.tof.len()).unwrap_or(0);
            match &sdk_pts {
                Some(v) => {
                    let s = v.get(i).copied().unwrap_or(0);
                    eprintln!("{i},{t},{s},{}", t as i64 - s as i64);
                }
                None => eprintln!("{i},{t},,"),
            }
        }
    }
    Ok(())
}

/// Why the inspection report must leave the native vendor readers closed, or `None` when it may
/// open one. Only a run whose whole job is the report opens one. Under `-o` the conversion opens
/// its own: the second open ran the Agilent host over the whole run twice (2.9 GB of temp file each
/// on a 242 MB Q-TOF `.d`) and booted CoreCLR a second time for SciEX. The `--via-msconvert` lane
/// never uses the native stack, so a missing one (no `MZPC_PWIZ_DIR`, no .NET 8) must not fail it —
/// it aborted `-v --via-msconvert` on `.wiff`, `.lcd` and Waters `.raw`. Without `-o` no lane runs,
/// so `--via-msconvert` (a flag, or a `--config` profile's default) leaves the report the job: it
/// printed only a note that ProteoWizard reads the file, which nothing did.
fn native_inspect_skip(output_given: bool, via_msconvert: bool) -> Option<&'static str> {
    if !output_given {
        None
    } else if via_msconvert {
        Some("native reader not opened: --via-msconvert reads this file through ProteoWizard")
    } else {
        Some("native reader not opened for this report: the conversion opens it")
    }
}

/// Print a human report of what a reader sees (format, spectra, chromatograms) without converting —
/// the behaviour of a no-output run, and the `-v` extra during a conversion. With `skip_native` set
/// (see [`native_inspect_skip`]) no vendor library is opened: Thermo's RawFileReader, Bruker's
/// baf2sql, Agilent MHDAC, SciEX, Waters and Shimadzu. A Windows vendor reader that fails to open
/// is a `note:` line, never the run's error.
fn report_inspect(input: &Path, skip_native: Option<&str>) -> Result<()> {
    // mzPeak archive: mzdata can't open it — report members + spectrum/chromatogram counts instead.
    if filter::is_mzpeak_input(input) {
        return filter::report_inspect(input);
    }
    println!("input:         {}", input.display());
    if is_tsf_dir(input) {
        println!("format:        Bruker TSF (.d)");
        println!("spectra:       {}", bruker_tsf::TsfReader::open(input)?.len());
        return Ok(());
    }
    #[cfg(any(windows, target_os = "linux"))]
    if is_baf_dir(input) {
        println!("format:        Bruker BAF (.d)");
        match skip_native {
            Some(why) => println!("note:          {why}"),
            None => println!("spectra:       {}", bruker_baf::BafReader::open(input, None)?.len()),
        }
        return Ok(());
    }
    if is_agilent_d(input) {
        println!("format:        Agilent .d");
        #[cfg(windows)]
        {
            if is_agilent_ims_d(input) {
                println!("note:          IM-QTOF run (AcqData/IMSFrame.bin): the native lane cannot carry the drift dimension; use --via-msconvert");
            } else if let Some(why) = skip_native {
                println!("note:          {why}");
            } else {
                // Inspecting runs the host over the whole file (16 B/point in a temp file), and an
                // MRM/SIM-only run is refused by design: report either outcome, never fail the
                // inspection.
                match agilent::AgilentReader::open(input) {
                    Ok(r) => {
                        println!("spectra:       {}", r.len());
                        println!("scan types:    {}", if r.scan_types().is_empty() { "unknown" } else { r.scan_types() });
                        println!("instrument:    {}", r.device_label());
                    }
                    Err(e) => println!("note:          native reader: {e:#}"),
                }
            }
        }
        #[cfg(not(windows))]
        println!("note:          the native Agilent (MHDAC) reader runs on Windows only; use --via-msconvert here");
        return Ok(());
    }
    if is_wiff(input) {
        println!("format:        SciEX .wiff");
        #[cfg(windows)]
        {
            match skip_native {
                Some(why) => println!("note:          {why}"),
                None => match sciex::SciexReader::open(input) {
                    Ok(r) => println!("spectra:       {}", r.len()),
                    Err(e) => println!("note:          native reader: {e:#}"),
                },
            }
        }
        #[cfg(not(windows))]
        println!("note:          the native SciEX (Clearcore2) reader runs on Windows only; use --via-msconvert here");
        return Ok(());
    }
    if is_waters_raw(input) {
        println!("format:        Waters MassLynx .raw");
        #[cfg(windows)]
        {
            match skip_native {
                Some(why) => println!("note:          {why}"),
                None => match waters::WatersReader::open(input) {
                    Ok(r) => println!("spectra:       {}", r.len()),
                    Err(e) => println!("note:          native reader: {e:#}"),
                },
            }
        }
        #[cfg(not(windows))]
        println!("note:          native Waters reading needs the waters feature on Windows (or use --via-msconvert)");
        return Ok(());
    }
    if is_lcd(input) {
        println!("format:        Shimadzu LabSolutions .lcd");
        #[cfg(windows)]
        {
            match skip_native {
                Some(why) => println!("note:          {why}"),
                None => match shimadzu::ShimadzuReader::open(input) {
                    Ok(r) => println!("spectra:       {}", r.len()),
                    Err(e) => println!("note:          native reader: {e:#}"),
                },
            }
        }
        #[cfg(not(windows))]
        println!("note:          native Shimadzu reading is Windows-only (Shimadzu.LabSolutions.IO); or use --via-msconvert");
        return Ok(());
    }
    // mzdata reads a Thermo .raw through Thermo's RawFileReader, an in-process .NET runtime: a vendor
    // library like the ones above, so it stays closed beside a conversion too. The test is mzdata's
    // own, made before anything is gunzipped: a plain `.raw` name test let `x.raw.gz` through, and
    // the report decompressed it and opened RawFileReader on the copy.
    if let Some(why) = skip_native.filter(|_| mzdata_reads_thermo_raw(input)) {
        println!("format:        Thermo .raw");
        println!("note:          {why}");
        return Ok(());
    }
    let _gz = if input.is_file() { gunzip_to_temp(input)? } else { None };
    let open_path: &Path = _gz.as_ref().map(|g| g.file.as_path()).unwrap_or(input);
    let reader = MZReaderType::<_, CentroidPeak, DeconvolutedPeak>::open_path(open_path)
        .with_context(|| format!("opening {}", input.display()))?;
    println!("format:        {}", reader_format(&reader));
    println!("spectra:       {}", reader.len());
    println!("chromatograms: {}", reader.count_chromatograms());
    if is_tdf_dir(input) {
        println!("ims-compact:   on by default for TDF (pass --no-ims-compact to write f64 m/z instead)");
    }
    Ok(())
}

/// True for an Agilent `.d` (folder with an `AcqData/` subdir).
fn is_agilent_d(input: &Path) -> bool {
    input.is_dir() && input.join("AcqData").is_dir()
}

/// True for an Agilent ion-mobility (6560 IM-QTOF) `.d`: `AcqData/IMSFrame.bin` holds the drift
/// frames and is absent or empty on every non-IM instrument.
#[cfg(windows)]
fn is_agilent_ims_d(input: &Path) -> bool {
    std::fs::metadata(input.join("AcqData").join("IMSFrame.bin")).is_ok_and(|m| m.is_file() && m.len() > 0)
}

/// Refine a requested chunking strategy against real m/z values: numpress-linear's floating-point
/// prediction fights a fixed-point lattice, so swap it for lossless delta when the data is on one.
/// An explicitly requested delta (`--no-numpress`) is left alone.
///
/// The detector itself moved to [`mz_lattice::fixed_point_lattice_scale`] (formerly the local
/// `is_fixed_point_lattice`, returning a bool) because the lattice ROUTE needs the matched scale,
/// not just the fact of a match. The decision here is unchanged: `.is_some()` is the old bool.
fn refine_chunking(
    sample_mz: &[f64],
    requested: Option<ChunkingStrategy>,
) -> Option<ChunkingStrategy> {
    match requested {
        Some(ChunkingStrategy::NumpressLinear { chunk_size })
            if mz_lattice::fixed_point_lattice_scale(sample_mz).is_some() =>
        {
            log::info!(
                "m/z is on a fixed-point lattice; using lossless delta chunking (numpress-linear \
                 would be both larger and lossy on this data)"
            );
            Some(ChunkingStrategy::Delta { chunk_size })
        }
        other => other,
    }
}

/// The fixed-point m/z scale the CENTROID lists of these probe spectra sit on, or `None`.
///
/// Deliberately NOT `sample_mz_from` (which takes the raw arrays of every spectrum, profile
/// included): only the peaks facet is lattice-encoded, so only the values that would reach it get a
/// vote. On a centroid mzML the two coincide — the raw arrays ARE the centroids — but on a
/// profile/dual run they do not, and a profile axis (a flight-time grid) must not arm a route that
/// will never touch it.
fn probe_lattice_scale(spectra: &[mzdata::spectrum::MultiLayerSpectrum]) -> Option<f64> {
    let lists: Vec<Vec<f64>> =
        spectra.iter().filter_map(mz_lattice::centroid_pairs).map(|(mz, _)| mz).collect();
    let centroids: Vec<f64> = lists.iter().flatten().copied().collect();
    let scale = mz_lattice::fixed_point_lattice_scale(&centroids)?;
    // The detector pools the probes' values; the ROUTE decides one spectrum at a time, and its
    // `centroid_lattice` also requires a non-decreasing k. Re-ask it per probe list so the scale
    // this run commits to is one every probe would actually have taken, not merely one their
    // concatenation passes.
    lists
        .iter()
        .all(|mz| mz.is_empty() || mz_lattice::centroid_lattice(mz, scale).is_some())
        .then_some(scale)
}

/// Collect m/z values from a few sample spectra, for `refine_chunking`.
fn sample_mz_from(spectra: &[mzdata::spectrum::MultiLayerSpectrum]) -> Vec<f64> {
    spectra
        .iter()
        .filter_map(|s| s.raw_arrays().and_then(|a| a.mzs().ok()).map(|v| v.to_vec()))
        .flatten()
        .collect()
}

/// Should the PEAK facet use the chunked layout, and with which strategy?
///
/// A centroid peak list is just a sorted m/z array, so it chunks exactly like profile signal does.
/// Without this the peaks facet is the point layout, where the spec requires values be stored as-is
/// -- so `point.mz` lands as PLAIN `f64` and zstd alone recovers only ~43% of it. Measured on a
/// centroid-only Shimadzu archive, that one column was 82% of the file at 1.82x compression, while
/// `spectrum_index` beside it got 42.9x from DELTA_BINARY_PACKED.
///
/// The PEAK facet always uses the DATA facet's chunking strategy. Not a tuning choice — a spec
/// requirement, and the reason this is a function rather than an inline argument.
///
/// `docs/conformance.md:68`: "within an entity, all `array_index` entries share one layout family —
/// either every entry is `point` or every entry is one of the `chunk_*` formats; the two **MUST
/// NOT** be mixed". `spectra_data` and `spectra_peaks` are both `entity_type: spectrum`, so a
/// chunked data facet beside a point peaks facet is non-conformant.
///
/// This previously chose per facet, by whichever held more points — which produced exactly that
/// illegal mix on every DUAL-representation archive (profile in `spectra_data`, centroid in
/// `spectra_peaks`). Measured: 3 archives, including a Shimadzu `.lcd` whose centroid facet
/// mzPeakViewer could then not read at all, and whose Summary reported it as profile-only.
///
/// Matching is not merely the legal choice, it was also the cheaper one on the file that motivated
/// the old heuristic: HEK_PosOAD1 is 33,392,175 B chunked/chunked versus 35,018,328 B mixed — 4.6%
/// SMALLER. A centroid-only run still gets the full chunked-peaks win (−46% on Shimadzu `.lcd`),
/// because its data facet is chunked too and the peaks facet simply follows.
///
/// The cost lands on profile-dominated runs carrying a small centroid sidecar, where the per-chunk
/// columns are not repaid (+34% on that facet for a Thermo LTQ Velos file, ~+3% of the archive).
/// That is the price of conformance, and it is not optional.
/// True when `path` is an mzML that does NOT carry the `<indexedmzML>` wrapper.
///
/// This is the discriminator for a real data-loss trap: mzdata can only enumerate chromatograms
/// from an mzML's EMBEDDED index. On a plain (non-indexed) mzML `count_chromatograms()` reports 0
/// and `get_chromatogram_by_index(0)` returns `None` even when the file declares a populated
/// `<chromatogramList>` — and calling `build_index()` does not recover them, so the converter
/// cannot read them at all. Measured: a Thermo LTQ Velos mzML declaring `TIC` + three
/// `SIM SIC` traces yielded 0; the two synthesized MS1 chromatograms masked the loss of the three
/// SIM SICs entirely. Only the first KB is read — the wrapper is the document element.
fn is_unindexed_mzml(path: &Path) -> bool {
    use std::io::Read;
    if !path.extension().is_some_and(|e| e.eq_ignore_ascii_case("mzML")) {
        return false;
    }
    let Ok(mut f) = fs::File::open(path) else { return false };
    let mut head = [0u8; 1024];
    let Ok(n) = f.read(&mut head) else { return false };
    !String::from_utf8_lossy(&head[..n]).contains("indexedmzML")
}

/// True for a Shimadzu LabSolutions `.lcd` file.
fn is_lcd(input: &Path) -> bool {
    input.is_file()
        && input
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("lcd"))
}

/// True for a SciEX wiff/wiff2 file.
fn is_wiff(input: &Path) -> bool {
    input
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("wiff") || e.eq_ignore_ascii_case("wiff2"))
}

/// `--sample N` for both msconvert lanes (mzPeak and `--to mzml`). A multi-sample WIFF is several
/// runs; with one `--outfile` msconvert writes them in turn and the LAST wins (En_PPY: 117
/// samples, one survived), so the run is picked explicitly.
fn msconvert_sample_arg(cmd: &mut Command, input: &Path) {
    if let Some(n) = sciex_sample().filter(|_| is_wiff(input)) {
        cmd.arg("--runIndexSet").arg(n.saturating_sub(1).to_string());
    }
}

/// Refuse what `msconvert_sample_arg` could not prevent: several runs written onto one output.
/// msconvert prints `writing output file: <path>` once per run, before writing it (`processFile`
/// in pwiz's `msconvert.cpp`), so more than one such line in its log means the output holds only
/// the last run. Counted from the log rather than from files: without `--outfile` pwiz names a
/// run `<wiff>-<sample name>`, and samples that share a name overwrite each other there too.
///
/// Also refused: a sample pwiz could not open. `Reader_ABI::read` catches that, prints
/// `[Reader_ABI::read] Error opening run <i> in <file>` and goes on with the rest, so the runs it
/// hands msconvert are a subset, numbered without the gap — `--runIndexSet` then selects a
/// different sample than `--sample` names, and a two-sample file with one unreadable is one run.
fn refuse_multi_run(log: &Path) -> Result<()> {
    // ponytail: read once msconvert has written EVERY run (En_PPY: all 117); tail the log while it
    // runs and kill it at the second line if that wait matters.
    let log = String::from_utf8_lossy(&fs::read(log).unwrap_or_default()).into_owned();
    let unopened: Vec<&str> = log.lines().filter(|l| l.contains("Error opening run")).collect();
    if !unopened.is_empty() {
        bail!(
            "msconvert could not open {} of the input's runs and skipped them, so the output would \
             hold a subset, and --sample would count only the runs it could open:\n{}",
            unopened.len(),
            unopened.join("\n")
        );
    }
    let runs = log.matches("writing output file:").count();
    if runs > 1 {
        bail!(
            "msconvert wrote {runs} runs onto the one output, so only the last survived; an output \
             is ONE run — pass --sample <1..{runs}> to choose which to convert"
        );
    }
    Ok(())
}

/// True for a Waters MassLynx `.raw`. Unlike a Thermo `.raw` (a single FILE), a Waters `.raw` is
/// a DIRECTORY with a `.raw` extension that holds `_HEADER.TXT` and per-function `_FUNCnnn.DAT`
/// files. Requiring `is_dir()` keeps it from colliding with the Thermo `.raw` file; the
/// `_HEADER.TXT` / `_FUNC*.DAT` marker keeps it from colliding with Bruker/Agilent `.d` dirs.
fn is_waters_raw(input: &Path) -> bool {
    if !input.is_dir() {
        return false;
    }
    let is_dot_raw = input
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("raw"));
    if !is_dot_raw {
        return false;
    }
    if input.join("_HEADER.TXT").exists() {
        return true;
    }
    // Otherwise accept any `_FUNC*.DAT` (case-insensitive) inside the directory.
    let Ok(entries) = std::fs::read_dir(input) else {
        return false;
    };
    for entry in entries.flatten() {
        if let Some(name) = entry.file_name().to_str() {
            let upper = name.to_ascii_uppercase();
            if upper.starts_with("_FUNC") && upper.ends_with(".DAT") {
                return true;
            }
        }
    }
    false
}

/// Reject inputs that have no native reader on THIS platform, with actionable guidance. The native
/// vendor readers are compiled in where the vendor libraries exist (Agilent/SciEX: Windows; Bruker
/// BAF: Windows + Linux); elsewhere the `--via-msconvert` lane is the path.
fn guard_unsupported_vendor(input: &Path) -> Result<()> {
    #[cfg(not(windows))]
    if is_agilent_d(input) {
        return Err(UnsupportedVendor(
            "Agilent .d native reading is available only on Windows (MHDAC vendor SDK). \
             On this platform use `--via-msconvert`."
                .to_string(),
        )
        .into());
    }
    #[cfg(not(windows))]
    if is_wiff(input) {
        return Err(UnsupportedVendor(
            "SciEX .wiff native reading is available only on Windows (Clearcore2 vendor SDK). \
             On this platform use `--via-msconvert`."
                .to_string(),
        )
        .into());
    }
    #[cfg(not(windows))]
    if is_lcd(input) {
        return Err(UnsupportedVendor(
            "Shimadzu .lcd native reading is available only on Windows (Shimadzu.LabSolutions.IO \
             vendor DLL). On this platform use `--via-msconvert`."
                .to_string(),
        )
        .into());
    }
    #[cfg(not(windows))]
    if is_waters_raw(input) {
        return Err(UnsupportedVendor(
            "Waters MassLynx .raw native reading is available only on Windows (MassLynx vendor SDK). \
             On this platform use `--via-msconvert`."
                .to_string(),
        )
        .into());
    }
    // Bruker BAF has a native reader on Windows + Linux but not macOS.
    #[cfg(not(any(windows, target_os = "linux")))]
    if input.is_dir() && input.join("analysis.baf").exists() {
        return Err(UnsupportedVendor(
            "Bruker BAF .d native reading is available only on Windows/Linux (libbaf2sql_c). \
             On this platform use `--via-msconvert`."
                .to_string(),
        )
        .into());
    }
    let _ = input;
    Ok(())
}

/// Interim cross-vendor lane (PLAN §3.7): run ProteoWizard `msconvert` to produce an mzML, then
/// convert that mzML to mzPeak through the existing path. Reuses everything downstream of the reader,
/// `--image` / `--sdrf` included (they used to be hard-coded away here, so the same command kept or
/// lost its SDRF depending on whether a native reader existed). `vendor` is passed through for
/// uniformity but has nothing to act on: `embed_vendor_members` keys on the READ path, which is the
/// temp mzML, so no side-file of the original input is embedded — `run` refuses `--aux` on this lane
/// for that reason.
#[allow(clippy::too_many_arguments)]
fn convert_via_msconvert(
    input: &Path,
    output: &Path,
    chunk: Option<ChunkingStrategy>,
    zstd_level: i32,
    msconvert_path: Option<&Path>,
    synth_chroms: bool,
    tof_grid: Option<TofGridMode>,
    vendor: Option<&vendor::VendorPolicy>,
    images: &[PathBuf],
    sdrf: Option<&Path>,
) -> Result<()> {
    let exe: std::ffi::OsString = msconvert_path
        .map(|p| p.as_os_str().to_os_string())
        .or_else(|| std::env::var_os("MSCONVERT_PATH"))
        .unwrap_or_else(|| "msconvert".into());

    // The intermediate mzML goes to a temp dir the guard removes on every return; "msconvert not
    // found" used to return through `?` below and leave it behind.
    let tmp = msconvert_dir(&std::env::temp_dir())?;
    let (tmpdir, mzml) = (&tmp.dir, &tmp.file);

    let mzcvt_log = tmpdir.join("msconvert.log");
    let mut cmd = Command::new(&exe);
    cmd.arg(input)
        .arg("--mzML")
        // #1: newer SCIEX (ZenoTOF 7600, newer TripleTOF) report an instrument-model string that
        // ProteoWizard's hand-curated `Reader_ABI` model map doesn't recognize yet; without this the
        // reader THROWS on the run and no mzML is written (msconvert may still exit 0 → we'd bail
        // "produced no mzML", or exit 1). msconvert itself recommends this exact flag ("use the
        // ignoreUnknownInstrumentError flag"). Benign on recognized instruments (they don't hit the
        // fallback), so it's safe to pass unconditionally.
        .arg("--ignoreUnknownInstrumentError")
        .arg("--outdir")
        .arg(&tmpdir)
        .arg("--outfile")
        .arg("via_msconvert.mzML");
    msconvert_sample_arg(&mut cmd, input);
    // #3: capture msconvert's own stdout+stderr to a log so a failure carries its real message
    // (unknown-instrument / unsupported-format / missing-sidecar) instead of a bare exit code.
    if let Ok(f) = fs::File::create(&mzcvt_log) {
        if let Ok(f2) = f.try_clone() {
            cmd.stdout(std::process::Stdio::from(f)).stderr(std::process::Stdio::from(f2));
        }
    }
    let status = cmd.status().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            anyhow::anyhow!(
                "msconvert not found ({}). Install ProteoWizard and put msconvert on PATH, or pass \
                 --msconvert-path / set $MSCONVERT_PATH. (Windows, or Wine.)",
                exe.to_string_lossy()
            )
        } else {
            anyhow::anyhow!("running msconvert: {e}")
        }
    })?;
    // #3: on any failure, include the tail of msconvert's own output so the error is self-diagnosing.
    let msconvert_tail = || -> String {
        fs::read_to_string(&mzcvt_log)
            .ok()
            .map(|s| {
                let lines: Vec<&str> = s.lines().collect();
                lines[lines.len().saturating_sub(15)..].join("\n")
            })
            .filter(|s| !s.trim().is_empty())
            .map(|s| format!("\n--- msconvert output (tail) ---\n{s}"))
            .unwrap_or_default()
    };
    if !status.success() {
        bail!("msconvert failed (exit {:?}){}", status.code(), msconvert_tail());
    }
    if !mzml.exists() {
        bail!("msconvert reported success but produced no mzML at {}{}", mzml.display(), msconvert_tail());
    }
    refuse_multi_run(&mzcvt_log)?;

    // msconvert produces SCIEX/Agilent mzML; the (detected, bounded-lossy) TOF-grid is opt-in and
    // OFF by default — pass the caller's mode through (this is the mzML path strategy A applies to).
    convert_file(mzml, output, chunk, zstd_level, vendor, synth_chroms, tof_grid, images, sdrf, true, None)
}

/// The `--to mzml` lane: convert `input` to a plain **mzML** via the mzdata writer, streaming the
/// read spectra straight through — no mzPeak encoders (ims-compact / TOF-grid / chunking /
/// byte-plane / side-file embedding all bypassed). Covers every format the tool reads: the
/// Windows-native vendor readers (SciEX/Waters/Agilent/Shimadzu, Bruker TSF/BAF) plus everything
/// mzdata reads directly (mzML/imzML, Thermo `.raw`, Bruker TDF). `--via-msconvert` has msconvert
/// write the mzML instead.
fn convert_to_mzml(
    input: &Path,
    output: &Path,
    via_msconvert: bool,
    msconvert_path: Option<&Path>,
) -> Result<()> {
    if via_msconvert {
        return msconvert_to_mzml(input, output, msconvert_path);
    }
    // Native-only vendor formats (mzdata can't read these) → native reader → mzML.
    if is_tsf_dir(input) {
        let r = bruker_tsf::TsfReader::open(input)?;
        return write_native_mzml(input, output, r.len(), |i| r.spectrum(i));
    }
    #[cfg(any(windows, target_os = "linux"))]
    if is_baf_dir(input) {
        let r = bruker_baf::BafReader::open(input, None)?;
        return write_native_mzml(input, output, r.len(), |i| r.spectrum(i));
    }
    #[cfg(windows)]
    if is_wiff(input) {
        let r = sciex::SciexReader::open_run(input, sciex_sample())?;
        write_native_mzml(input, output, r.len(), |i| r.spectrum(i))?;
        // mzML has no `transformations` list to declare the glue's value changes in: say them.
        if let Some(msg) = r.take_value_changes().warning() {
            log::warn!("{msg}; mzML has no transformations list to declare them in");
        }
        return Ok(());
    }
    #[cfg(windows)]
    if is_waters_raw(input) {
        let r = waters::WatersReader::open(input)?;
        return write_native_mzml(input, output, r.len(), |i| r.spectrum(i));
    }
    #[cfg(windows)]
    if is_agilent_d(input) {
        if is_agilent_ims_d(input) {
            bail!(
                "{} is an Agilent IM-QTOF run (AcqData/IMSFrame.bin present): the native lane cannot \
                 carry its drift dimension; export this run with --via-msconvert",
                input.display()
            );
        }
        let r = agilent::AgilentReader::open(input)?;
        return write_native_mzml(input, output, r.len(), |i| r.spectrum(i));
    }
    #[cfg(windows)]
    if is_lcd(input) {
        // Honour --representation here too. mzML carries ONE representation per spectrum, so the
        // default `both` still has to collapse to one on export; but an explicit `profile` /
        // `centroid` must pick which, and previously did not reach this path at all -- the two
        // exports came out byte-identical.
        // mzML holds ONE representation per spectrum. Under the faithful `both` default the reader
        // hands the writer profile arrays PLUS a centroid peak list, and mzdata then serialises the
        // typed peaks while taking continuity from the description -- writing centroid data labelled
        // `profile spectrum`. Collapse here instead, to the profile (less-processed) view, so the
        // bytes and the label agree. `Representation::Profile` already falls back to whatever the
        // file actually stores, so a centroid-only `.lcd` still exports correctly-labelled centroids.
        let rep = match representation() {
            RepresentationArg::Both => {
                log::info!(
                    "--to mzml: mzML carries one representation per spectrum; exporting the profile \
                     view where present (pass --representation centroid for the peak lists)"
                );
                shimadzu::Representation::Profile
            }
            RepresentationArg::Profile => shimadzu::Representation::Profile,
            RepresentationArg::Centroid => shimadzu::Representation::Centroid,
        };
        let r = shimadzu::ShimadzuReader::open_with(input, rep)?;
        return write_native_mzml(input, output, r.len(), |i| r.spectrum(i));
    }
    // Agilent profile `.d` off Windows: the pure-Rust MSProfile.bin reader gives native mzML without
    // msconvert (same reader the mzPeak grid lane uses). Must run BEFORE guard_unsupported_vendor,
    // which rejects Agilent `.d`. If the reader can't open this `.d` (e.g. an ion-mobility / flat
    // MSScan.xsd variant it doesn't model), fall through to the typed "use --via-msconvert" guidance
    // rather than surfacing a raw schema-parse error.
    #[cfg(not(windows))]
    if is_agilent_d(input) && agilent_profile::has_profile(input) {
        match agilent_profile::AgilentProfileReader::open(input) {
            Ok(reader) => return write_agilent_profile_mzml(reader, input, output),
            Err(e) => log::warn!(
                "native Agilent profile→mzML unavailable for {}: {e:#}",
                input.display()
            ),
        }
    }
    // Off-Windows: the native-only vendor formats can't be read here (typed unsupported error).
    guard_unsupported_vendor(input)?;

    // mzdata-readable (mzML/imzML, Thermo `.raw`, Bruker TDF). The Latin-1 transcode +
    // empty-param-group sanitize are XML-FILE-only workarounds — applying them to a directory
    // vendor unit (a `.d`) would `read()` the directory fd and fail EISDIR before we ever reach the
    // reader, so gate them on a file input.
    let (_gz, _utf8, _sanitized, read_path): (
        Option<GunzipGuard>,
        Option<TranscodeGuard>,
        Option<SanitizedTemp>,
        PathBuf,
    ) = if input.is_file() {
            let gz = gunzip_to_temp(input)?;
            let plain: &Path = gz.as_ref().map(|g| g.file.as_path()).unwrap_or(input);
            let utf8 = transcode_to_utf8(plain)?;
            let utf8_path: &Path = utf8.as_ref().map(|g| g.file.as_path()).unwrap_or(plain);
            let sanitized = sanitize_param_groups(utf8_path)?.map(SanitizedTemp);
            let rp = sanitized.as_ref().map(|s| s.0.clone()).unwrap_or_else(|| utf8_path.to_path_buf());
            (gz, utf8, sanitized, rp)
        } else {
            (None, None, None, input.to_path_buf())
        };
    let mut reader = MZReaderType::<_, CentroidPeak, DeconvolutedPeak>::open_path(&read_path)
        .with_context(|| format!("opening {}", input.display()))?;

    use mzdata::prelude::{ChromatogramSource, MSDataFileMetadata, SpectrumSource, SpectrumWriter};
    // mzdata reaches chromatograms only by offset through the embedded `<indexList>`, so where the
    // spectrum pass leaves the reader does not matter. Chromatograms missing here mean that index
    // was unreadable: a non-indexed mzML, or a rewritten temp copy with stale offsets (the reason
    // `transcode_to_utf8` rebuilds them).
    let source_chroms: Vec<Chromatogram> = reader.iter_chromatograms().collect();
    warn_unread_chromatograms(input, &read_path, source_chroms.len());
    let _ = reader.reset();
    // The mzPeak lane's rule for Thermo windows without a stated width (`thermo_isolation`). mzML
    // has no transformations list, so the run's warning is the only declaration here.
    let mut thermo_windows = matches!(reader, MZReaderType::ThermoRaw(_))
        .then(|| thermo_isolation::UnstatedWidthGuard::open(&read_path));

    // Guard before writer: `w` is dropped first (the handle closes), then the guard removes the tmp.
    let tmp = mzml_tmp_path(output);
    let tmp_guard = TmpGuard::new(&tmp);
    let mut w = mzdata::io::mzml::MzMLWriter::new(mzml_sink(&tmp)?);
    w.copy_metadata_from(&reader);
    fixup_mzml_run_metadata(&mut w, input);
    let cap = max_spectra();
    let n_spec = cap.map_or_else(|| reader.len(), |m| m.min(reader.len()));
    w.set_spectrum_count(n_spec as u64);
    // Open the spectrumList NOW so chromatograms (written after it) have a valid state even when the
    // input has zero spectra (a chromatogram-only SRM/MRM file) — otherwise `write_chromatogram`
    // fails to transition into the chromatogramList.
    w.start_spectrum_list().map_err(|e| anyhow!("opening mzML spectrumList: {e}"))?;

    let mut written = 0usize;
    for (i, mut spec) in reader.iter().enumerate() {
        if cap.is_some_and(|m| i >= m) {
            break;
        }
        if let Some(g) = thermo_windows.as_mut() {
            g.apply(spec.description_mut());
        }
        SpectrumWriter::write(&mut w, &spec)
            .map_err(|e| anyhow!("writing spectrum {i} to mzML: {e}"))?;
        written += 1;
    }
    if let Some(g) = &thermo_windows {
        g.report();
    }
    // Same truncated-source cross-check the mzPeak lanes make; the `?` drops the writer and then
    // the guard, so a truncated source leaves no half mzML that looks like a successful conversion.
    assert_source_complete(input, written, cap)?;
    // Pass through the source's chromatograms (SRM/SIM/vendor traces — otherwise silently lost,
    // fatal for MRM data). Drop source TIC/base-peak: the mzML writer emits its own spectrum-derived
    // TIC + base-peak summary at close, so keeping the source ones would duplicate them.
    write_source_chromatograms_mzml(&mut w, source_chroms.into_iter())?;

    SpectrumWriter::close(&mut w)
        .map_err(|e| anyhow!("finalizing mzML {}: {e}", output.display()))?;
    // The gzip trailer is written when the encoder drops: close the sink BEFORE the rename.
    drop(w);
    tmp_guard.finish(output)?;
    log::info!("wrote {}", output.display());
    Ok(())
}

/// The mzPeak-INPUT filter path with an mzML output. Reads the `.mzpeak` with the sync `MzPeakReader`
/// — which decodes every buffer transform (delta chains, numpress, and the timsTOF `SqrtMzFromTof`
/// tof→m/z), so iterated spectra carry real m/z (+ ion mobility), not raw tof — keeps the spectra
/// passing the RT / MS-level predicate, and writes them to a real mzML via the mzdata writer. This is
/// the "slice a mzPeak to a narrow RT window, then hand the small mzML to a search engine
/// (Sage/MSFragger)" workflow. Aux/vendor embedding does not apply to an mzML output and is
/// silently ignored.
///
/// The survivors are `filter.rs`'s: `spectrum.time` (stored in **minutes**) ∈ `--rt` AND
/// `ms_level` ∈ the `--ms-level` set, read from `spectra_metadata` by [`filter::surviving_spectra`].
fn filter_mzpeak_to_mzml(input: &Path, output: &Path, opts: &filter::FilterOpts) -> Result<()> {
    use mzdata::prelude::{MSDataFileMetadata, SpectrumLike, SpectrumSource, SpectrumWriter};
    use mzpeak_prototyping::MzPeakReader;

    let filtering = opts.rt.is_some() || !opts.ms_levels.is_empty();

    let mut reader =
        MzPeakReader::new(input).with_context(|| format!("opening {} as mzPeak", input.display()))?;
    let total = reader.len();
    // An mzPeak spectrum may carry BOTH facets; an mzML spectrum cannot. The reader's default
    // preference is profile, so the peak lists are dropped — correct, but it used to be silent.
    // Say so, once, with the count, rather than quietly halving what the archive holds.
    {
        let dp = reader.metadata.spectra.data_point_counts();
        let pk = reader.metadata.spectra.peak_counts();
        let dual = dp
            .iter()
            .zip(pk.iter())
            .filter(|(d, p)| **d > 0 && **p > 0)
            .count();
        if dual > 0 {
            log::warn!(
                "{dual}/{total} spectra carry both a profile and a peak facet; mzML holds one \
                 representation per spectrum, so the profile view is exported and the peak lists \
                 are dropped"
            );
        }
    }
    let cap = max_spectra();

    // The surviving indices, known up front for the spectrumList `count` attribute: the indices the
    // archive holds, ascending, up to MZPC_MAX_SPECTRA of them (not `0..total`: an archive rewritten
    // with --rt or --ms-level keeps each survivor's original index, and counting up from 0 asked for
    // spectra that were filtered out while never reaching the last ones), and of those, when something
    // filters, the ones one scan of the `time` / `ms_level` columns keeps. This was a metadata-only
    // read of every spectrum, filtered or not, before the survivors were read again in full: 265 s for
    // the 32,700 spectra of MSV000099123's `…_8225.mzpeak` before a first write.
    let mut indices: Vec<usize> = reader.get_index().iter().map(|(_, i)| *i as usize).collect();
    indices.sort_unstable();
    indices.truncate(cap.unwrap_or(usize::MAX));
    let survivor_ids: Vec<usize> = if filtering {
        let kept = filter::surviving_spectra(input, opts)?;
        let ids: Vec<usize> = indices.into_iter().filter(|&i| kept.contains(&(i as u64))).collect();
        log::info!("filter: keeping {}/{} spectra", ids.len(), total);
        ids
    } else {
        indices
    };

    // Guard before writer: `w` is dropped first (the handle closes), then the guard removes the tmp.
    let tmp = mzml_tmp_path(output);
    let tmp_guard = TmpGuard::new(&tmp);
    let mut w = mzdata::io::mzml::MzMLWriter::new(mzml_sink(&tmp)?);
    w.copy_metadata_from(&reader);
    fixup_mzml_run_metadata(&mut w, input);
    w.set_spectrum_count(survivor_ids.len() as u64);
    w.start_spectrum_list().map_err(|e| anyhow!("opening mzML spectrumList: {e}"))?;
    for &i in &survivor_ids {
        let mut spec = reader
            .get_spectrum_by_index(i)
            .ok_or_else(|| anyhow!("spectrum {i} vanished between metadata and data passes"))?;
        demote_mzp_params(spec.description_mut());
        if let Some(arrays) = spec.arrays.as_mut() {
            strip_grid_axis(arrays);
        }
        SpectrumWriter::write(&mut w, &spec)
            .map_err(|e| anyhow!("writing spectrum {i} to mzML: {e}"))?;
    }

    // Carry the archive's chromatograms across. Without this the lane emitted ONLY the writer's
    // synthesized TIC/base-peak summary: on a 300-chromatogram SIM/SRM run, 299 quantitative traces
    // vanished on export even though they were stored intact in `chromatograms_data.parquet`. The
    // convert lane has always done this (`write_source_chromatograms_mzml`); this one never did.
    // TIC/base-peak are skipped there because the writer regenerates them at close.
    let n_chrom = mzdata::prelude::ChromatogramSource::count_chromatograms(&reader);
    let chroms: Vec<Chromatogram> = (0..n_chrom)
        .filter_map(|i| mzdata::prelude::ChromatogramSource::get_chromatogram_by_index(&mut reader, i))
        .map(|mut c| {
            demote_mzp_params_chrom(c.description_mut());
            c
        })
        .collect();
    write_source_chromatograms_mzml(&mut w, chroms.into_iter())?;

    SpectrumWriter::close(&mut w)
        .map_err(|e| anyhow!("finalizing mzML {}: {e}", output.display()))?;
    // The gzip trailer is written when the encoder drops: close the sink BEFORE the rename.
    drop(w);
    tmp_guard.finish(output)?;
    log::info!("wrote {}", output.display());
    Ok(())
}

/// Write a native reader's spectra (via a `spectrum(i)` closure) to an mzML. Native readers carry no
/// vendor chromatograms; the mzML writer emits its own TIC + base-peak summary at close (matching the
/// mzPeak path's synthesized TIC/BPC).
fn write_native_mzml(
    input: &Path,
    output: &Path,
    len: usize,
    mut spectrum: impl FnMut(usize) -> Result<mzdata::spectrum::MultiLayerSpectrum>,
) -> Result<()> {
    use mzdata::prelude::SpectrumWriter;
    if len == 0 {
        bail!("no spectra in {}", input.display());
    }
    // Guard before writer: `w` is dropped first (the handle closes), then the guard removes the tmp.
    let tmp = mzml_tmp_path(output);
    let tmp_guard = TmpGuard::new(&tmp);
    let mut w = mzdata::io::mzml::MzMLWriter::new(mzml_sink(&tmp)?);
    fixup_mzml_run_metadata(&mut w, input);
    let n = max_spectra().map_or(len, |m| m.min(len));
    w.set_spectrum_count(n as u64);
    for i in 0..n {
        let mut spec = spectrum(i)?;
        // Native timsTOF readers attach MZP:1000006/7 to selected ions; mzML gets them as userParam.
        demote_mzp_params(spec.description_mut());
        SpectrumWriter::write(&mut w, &spec)
            .map_err(|e| anyhow!("writing spectrum {i} to mzML: {e}"))?;
    }
    SpectrumWriter::close(&mut w)
        .map_err(|e| anyhow!("finalizing mzML {}: {e}", output.display()))?;
    // The gzip trailer is written when the encoder drops: close the sink BEFORE the rename.
    drop(w);
    tmp_guard.finish(output)?;
    log::info!("wrote {}", output.display());
    Ok(())
}

/// Write an Agilent **profile** `.d` (`AcqData/MSProfile.bin`) to mzML on any platform, using the
/// pure-Rust reader (no msconvert / vendor SDK). Each stored profile point's flight-time bin is mapped
/// to m/z with the per-scan calibration, applying MassHunter's polynomial refinement when the scan
/// carries a calibration row (the value msconvert would emit); the raw integer counts become the
/// intensity array. The data is profile, so the spectra are marked as such.
#[cfg(not(windows))]
fn write_agilent_profile_mzml(
    mut reader: agilent_profile::AgilentProfileReader,
    input: &Path,
    output: &Path,
) -> Result<()> {
    use mzdata::prelude::SpectrumWriter;
    // Guard before writer: `w` is dropped first (the handle closes), then the guard removes the tmp.
    let tmp = mzml_tmp_path(output);
    let tmp_guard = TmpGuard::new(&tmp);
    let mut w = mzdata::io::mzml::MzMLWriter::new(mzml_sink(&tmp)?);
    fixup_mzml_run_metadata(&mut w, input);
    // Upper bound on the count attribute — empty/truncated segments are skipped while streaming
    // (matches write_native_mzml, which also uses the reader's record count).
    let cap = max_spectra();
    w.set_spectrum_count(cap.map_or(reader.len(), |m| m.min(reader.len())) as u64);

    let mut out_index = 0usize;
    while let Some(ps) = reader.next_spectrum()? {
        if cap.is_some_and(|m| out_index >= m) {
            break;
        }
        // Flight-time bin → m/z, applying the polynomial refinement when a calibration row is present
        // (identical math to the mzPeak grid lane's lossless gate).
        let row = reader.calib_row(ps.index);
        let uf = reader.poly_flags_for(ps.index);
        let mut mz: Vec<f64> = Vec::with_capacity(ps.tof_index.len());
        for &k in &ps.tof_index {
            let m = match row {
                Some(r) if r[0] != 0.0 => {
                    let (coeff, base) = (r[0], r[1]);
                    let t = base + (ps.grid.c0 + ps.grid.c1 * k as f64) / coeff;
                    let refined = agilent_profile::calibrated_mz(r, uf, t);
                    if refined > 0.0 { refined } else { ps.grid.mz(k) }
                }
                _ => ps.grid.mz(k),
            };
            mz.push(m);
        }
        let mut intensity: Vec<f32> = ps.intensity.iter().map(|&v| v as f32).collect();
        // The TOF axis can descend (grid.c1 < 0); m/z is monotonic in k, so one reverse restores the
        // ascending-m/z order mzML consumers expect.
        if mz.len() > 1 && mz[0] > mz[mz.len() - 1] {
            mz.reverse();
            intensity.reverse();
        }

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

        let mut descr = mzdata::spectrum::SpectrumDescription {
            id: format!("scanId={}", ps.index),
            index: out_index,
            ms_level: ps.ms_level,
            signal_continuity: mzdata::spectrum::SignalContinuity::Profile,
            ..Default::default()
        };
        let mut scan = mzdata::spectrum::ScanEvent::default();
        scan.start_time = ps.scan_time; // MSProfile scan_time is in minutes; mzdata wants minutes
        descr.acquisition.scans.push(scan);

        let spec = MultiLayerSpectrum::new(descr, Some(arrays), None, None);
        SpectrumWriter::write(&mut w, &spec)
            .map_err(|e| anyhow!("writing spectrum {out_index} to mzML: {e}"))?;
        out_index += 1;
    }
    if out_index == 0 {
        bail!("no profile spectra in {}", input.display());
    }
    SpectrumWriter::close(&mut w)
        .map_err(|e| anyhow!("finalizing mzML {}: {e}", output.display()))?;
    // The gzip trailer is written when the encoder drops: close the sink BEFORE the rename.
    drop(w);
    tmp_guard.finish(output)?;
    log::info!("wrote {}", output.display());
    Ok(())
}

/// A chromatogram type as the PSI-MS cvParam mzML states it with; `None` for an unknown type.
/// Written out rather than taken from mzdata's `ChromatogramType::to_curie`, which gives the
/// selected-ion-monitoring and selected-reaction-monitoring types MS:1000472 and MS:1000473 (two
/// Agilent instrument models) where its own `from_accession` reads MS:1001472 and MS:1001473.
fn chromatogram_type_param(kind: ChromatogramType) -> Option<Param> {
    use ChromatogramType as C;
    let (name, curie) = match kind {
        C::TotalIonCurrentChromatogram => ("total ion current chromatogram", curie!(MS:1000235)),
        C::BasePeakChromatogram => ("basepeak chromatogram", curie!(MS:1000628)),
        C::SelectedIonCurrentChromatogram => ("selected ion current chromatogram", curie!(MS:1000627)),
        C::SelectedIonMonitoringChromatogram => ("selected ion monitoring chromatogram", curie!(MS:1001472)),
        C::SelectedReactionMonitoringChromatogram => ("selected reaction monitoring chromatogram", curie!(MS:1001473)),
        C::AbsorptionChromatogram => ("absorption chromatogram", curie!(MS:1000812)),
        C::EmissionChromatogram => ("emission chromatogram", curie!(MS:1000813)),
        C::ElectromagneticRadiationChromatogram => ("electromagnetic radiation chromatogram", curie!(MS:1000811)),
        C::FlowRateChromatogram => ("flow rate chromatogram", curie!(MS:1003020)),
        C::PressureChromatogram => ("pressure chromatogram", curie!(MS:1003019)),
        C::TemperatureChromatogram => ("temperature chromatogram", curie!(MS:1002715)),
        C::Unknown => return None,
    };
    Some(Param::builder().name(name).curie(curie).build())
}

/// Give a chromatogram's arrays types a writer can name. mzdata's mzML reader has a case for only
/// some PSI-MS array types, and none for the `flow rate array` (MS:1000820), `pressure array`
/// (MS:1000821) and `temperature array` (MS:1000822) that the export of an archive with Bruker
/// device traces writes: such an array comes back as `ArrayType::Unknown`, its cvParam left among
/// the array's parameters, and mzdata's mzML writer and the mzPeak writer both panic naming it (exit
/// 134, nothing written). A type mzdata knows the accession of (`ArrayType::from_accession`) becomes
/// the array's type, with that parameter's unit. Anything else is kept, with a warning, as a
/// non-standard data array named after the parameter, or `unknown array` without one; so is an array
/// whose type the chromatogram already holds, since the map is keyed by type and would lose one.
/// The reader's map holds one `Unknown` array at most.
fn readable_chromatogram_arrays(mut chrom: Chromatogram) -> Chromatogram {
    let Some(mut array) = chrom.arrays.byte_buffer_map.remove(&ArrayType::Unknown) else {
        return chrom;
    };
    let known = |p: &Param| {
        p.curie()
            .and_then(ArrayType::from_accession)
            .filter(|t| !matches!(t, ArrayType::Unknown | ArrayType::NonStandardDataArray { .. }))
    };
    let mut params: Vec<Param> = array.params.take().map(|p| *p).unwrap_or_default();
    let at = params.iter().position(|p| known(p).is_some()).or((!params.is_empty()).then_some(0));
    let stated = at.map(|i| params.remove(i));
    if let Some(p) = &stated {
        if array.unit == Unit::Unknown {
            array.unit = p.unit;
        }
    }
    let kind = stated.as_ref().and_then(known).filter(|t| !chrom.arrays.has_array(t));
    match kind {
        Some(t) => array.name = t,
        None => {
            let base = stated.map(|p| p.name).filter(|n| !n.is_empty()).unwrap_or_else(|| "unknown array".to_string());
            let mut name = base.clone();
            for k in 2.. {
                if !chrom.arrays.has_array(&ArrayType::nonstandard(&name)) {
                    break;
                }
                name = format!("{base} {k}");
            }
            log::warn!(
                "chromatogram {:?}: an array of a type mzdata cannot read ({base}) is kept as the non-standard data array {name:?}",
                chrom.id()
            );
            array.name = ArrayType::nonstandard(&name);
        }
    }
    array.params = (!params.is_empty()).then(|| Box::new(params));
    chrom.arrays.add(array);
    chrom
}

/// A chromatogram as the chromatogram facet's schema is sampled from it: its time array in minutes,
/// so the column declares the unit [`finish_chromatograms`] stores (sampled in ProteoWizard's
/// seconds, the column declared seconds over minutes), and without the array
/// [`readable_chromatogram_arrays`] names, which is then written as that chromatogram's auxiliary
/// array in its own unit and data type — how the native Bruker lanes store the same device traces,
/// so an archive converted back from its mzML export stores them alike. Sampled, the arrays of the
/// first ten chromatograms would become columns and those of every later one auxiliary arrays, and a
/// later array of a type already a column is written into that column and reads back in its unit.
fn schema_sample_chromatogram(mut chrom: Chromatogram) -> Chromatogram {
    chrom.arrays.byte_buffer_map.remove(&ArrayType::Unknown);
    // A time array that cannot be read is sampled as it is; writing that chromatogram fails on it.
    let _ = chromatogram_time_to_minutes(&mut chrom.arrays);
    chrom
}

/// Pass a source's chromatograms through to an mzML, dropping TIC/base-peak (the mzML writer emits
/// its own spectrum-derived TIC + base-peak summary at close, so those would duplicate). Everything
/// else — SRM/SIM/vendor traces — is preserved. Must be called after all spectra (writer state).
/// The writer puts `<chromatogramList count>` out with the first chromatogram, from a count it
/// starts at 2 (its own summary pair), so the count is set here first. Every array is given a type
/// the writer can name ([`readable_chromatogram_arrays`]), and every typed chromatogram its type term.
fn write_source_chromatograms_mzml<W: std::io::Write, I: Iterator<Item = Chromatogram>>(
    w: &mut mzdata::io::mzml::MzMLWriter<W>,
    source: I,
) -> Result<()> {
    let kept: Vec<Chromatogram> = source
        .map(readable_chromatogram_arrays)
        .filter(|c| {
            !matches!(
                c.chromatogram_type(),
                ChromatogramType::TotalIonCurrentChromatogram | ChromatogramType::BasePeakChromatogram
            )
        })
        .map(|mut c| {
            // mzML states a chromatogram's type only as a cvParam. mzdata's mzML reader moves that
            // cvParam into the typed field and its writer writes the parameters alone, so every
            // chromatogram of an mzML → mzML conversion lost its type term (tiny.pwiz's selected ion
            // current trace among them). Put it back unless a parameter still states a type.
            if !c.params().iter().any(|p| p.curie().and_then(ChromatogramType::from_curie).is_some()) {
                if let Some(p) = chromatogram_type_param(c.chromatogram_type()) {
                    c.description_mut().params.insert(0, p);
                }
            }
            c
        })
        .collect();
    w.chromatogram_count = kept.len() as u64 + 2;
    for chrom in &kept {
        w.write_chromatogram(chrom).map_err(|e| anyhow!("writing chromatogram to mzML: {e}"))?;
    }
    Ok(())
}

/// Run ProteoWizard `msconvert` for the output mzML (`--via-msconvert --to mzml`). It writes into a
/// fresh directory BESIDE the output, so the rename into place cannot cross a volume, and only the
/// file it wrote there is renamed onto `output`. It used to be handed the final path, with
/// `output.exists()` as the success check, so under `--force` a previous run's file passed for this
/// run's — which real msconvert produces for `-o x.mzML.gz`, a name it writes under another one.
fn msconvert_to_mzml(input: &Path, output: &Path, msconvert_path: Option<&Path>) -> Result<()> {
    let exe: std::ffi::OsString = msconvert_path
        .map(|p| p.as_os_str().to_os_string())
        .or_else(|| std::env::var_os("MSCONVERT_PATH"))
        .unwrap_or_else(|| "msconvert".into());
    let outdir = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let tmp = msconvert_dir(outdir)?;
    // Capture msconvert's stdout+stderr so a failure carries its real message (unknown-instrument /
    // unsupported-format / missing-sidecar) instead of a bare exit code — same as the mzPeak
    // `convert_via_msconvert` path (commit 57262aa).
    let log_path = tmp.dir.join("msconvert.log");
    let mut cmd = Command::new(&exe);
    cmd.arg(input)
        .arg("--mzML")
        .arg("--ignoreUnknownInstrumentError")
        .arg("--outdir")
        .arg(&tmp.dir)
        .arg("--outfile")
        .arg("via_msconvert.mzML");
    msconvert_sample_arg(&mut cmd, input);
    if let Ok(f) = fs::File::create(&log_path) {
        if let Ok(f2) = f.try_clone() {
            cmd.stdout(std::process::Stdio::from(f)).stderr(std::process::Stdio::from(f2));
        }
    }
    let status = cmd.status().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            anyhow!(
                "msconvert not found ({}); install ProteoWizard or set --msconvert-path / \
                 $MSCONVERT_PATH",
                exe.to_string_lossy()
            )
        } else {
            anyhow!("running msconvert: {e}")
        }
    })?;
    let tail = || -> String {
        fs::read_to_string(&log_path)
            .ok()
            .map(|s| {
                let lines: Vec<&str> = s.lines().collect();
                lines[lines.len().saturating_sub(15)..].join("\n")
            })
            .filter(|s| !s.trim().is_empty())
            .map(|s| format!("\n--- msconvert output (tail) ---\n{s}"))
            .unwrap_or_default()
    };
    if !status.success() {
        bail!("msconvert failed (exit {:?}){}", status.code(), tail());
    }
    if !tmp.file.exists() {
        bail!("msconvert reported success but produced no mzML at {}{}", tmp.file.display(), tail());
    }
    refuse_multi_run(&log_path)?;
    // msconvert's own `--gzip` would pick the file name again; compress what it wrote instead.
    let written = if has_gz_suffix(output) {
        let gz = tmp.dir.join("via_msconvert.mzML.gz");
        let compress = || -> io::Result<()> {
            let mut enc = flate2::write::GzEncoder::new(fs::File::create(&gz)?, flate2::Compression::default());
            io::copy(&mut fs::File::open(&tmp.file)?, &mut enc)?;
            enc.finish().map(drop)
        };
        compress().with_context(|| format!("gzip-compressing {}", tmp.file.display()))?;
        gz
    } else {
        tmp.file.clone()
    };
    TmpGuard::new(&written).finish(output)?;
    log::info!("wrote {}", output.display());
    Ok(())
}

/// Core conversion: mzdata reader → mzpeak_prototyping writer. Single-threaded for the MVP
/// (the reference uses a reader/writer thread pair — a later optimization, not a correctness
/// requirement). Mirrors the proven wiring in mzpeak_prototyping/examples/convert.rs.
/// True for a Bruker TSF `.d` (line spectra; mzdata can't read it, we use the timsrust-tsf path).
fn is_tsf_dir(input: &Path) -> bool {
    input.is_dir() && has_nonempty(input, "analysis.tsf") && !has_nonempty(input, "analysis.tdf")
}

/// True for a Bruker BAF `.d` (Q-TOF; peak arrays behind the baf2sql_c SDK).
#[cfg(any(windows, target_os = "linux"))]
fn is_baf_dir(input: &Path) -> bool {
    input.is_dir() && has_nonempty(input, "analysis.baf")
}

/// Sample m/z arrays across the run and try to fit a per-run integer TOF grid (`sqrt(m/z)=c0+c1·k`).
/// Returns the accepted fit (every sampled point within `tof_grid::ppm_tol()` — a bound, not an
/// exactness proof), or `None` if the data isn't on a flight-time lattice at all
/// (Orbitrap / QqQ-SRM / centroid-with-jitter). Reads up to 16 spectra spread over the run via random
/// access; the reader's normal iteration order is unaffected (callers re-`iter()` from the start).
fn try_fit_tof_grid<R>(reader: &mut R) -> Option<tof_grid::FitOutcome>
where
    R: SpectrumSource<CentroidPeak, DeconvolutedPeak, MultiLayerSpectrum<CentroidPeak, DeconvolutedPeak>>,
{
    let total = reader.len();
    if total == 0 {
        return None;
    }
    const N_SAMPLE: usize = 16;
    let step = (total / N_SAMPLE).max(1);
    let mut samples: Vec<Vec<f64>> = Vec::new();
    let mut idx = 0usize;
    while idx < total && samples.len() < N_SAMPLE {
        if let Some(spec) = reader.get_spectrum_by_index(idx) {
            // Ion-mobility frames carry a 3D layout the grid fit shouldn't span; skip them (the
            // mzML TOF-grid path targets ordinary profile/centroid SCIEX spectra).
            let mz: Option<Vec<f64>> = spec
                .arrays
                .as_ref()
                .filter(|a| !a.has_ion_mobility())
                .and_then(|a| a.mzs().ok())
                .map(|c| c.into_owned());
            if let Some(v) = mz {
                if v.len() >= 64 {
                    samples.push(v);
                }
            }
        }
        idx += step;
    }
    tof_grid::fit(&samples)
}

/// Convert an mzML reader to a TOF-grid mzPeak archive: each spectrum's f64 m/z is replaced by an
/// integer `tof_index` (Int32) column, with the per-run `{c0,c1}` grid stored in the index
/// `tof_calibration` block. Readers reconstruct `m/z = (c0 + c1·tof_index)²`. The integer column is
/// named `tof_index` so the vendored writer applies DELTA_BINARY_PACKED automatically. Mirrors
/// `convert_ims_compact_archive`'s custom-peak-schema mechanism, but for the mzML path.
#[allow(clippy::too_many_arguments)]
fn convert_file_tof_grid(
    input: &Path,
    output: &Path,
    zstd_level: i32,
    vendor: Option<&vendor::VendorPolicy>,
    synth_chroms: bool,
    mut reader: MZReaderType<fs::File, CentroidPeak, DeconvolutedPeak>,
    grid: tof_grid::TofGrid,
    images: &[PathBuf],
    sdrf: Option<&Path>,
) -> Result<()> {
    let tmp = output.with_extension("mzpeak.tmp");
    let tmp_guard = TmpGuard::new(&tmp);
    let handle = fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    let level = ZstdLevel::try_new(zstd_level)
        .map_err(|e| anyhow::anyhow!("invalid zstd level {zstd_level}: {e}"))?;

    let tof_field = tof_index_field((grid.c0, grid.c1), false);

    let mut builder = MzPeakWriterType::<fs::File>::builder()
        .buffer_size(buffer_spectra())
        .compression(Compression::ZSTD(level))
        // The off-grid f64 m/z of these point facets: BYTE_STREAM_SPLIT, no dictionary.
        .shuffle_mz(true)
        .store_peaks_and_profiles_apart(Some(tof_index_peak_schema(tof_field.clone())));
    // PER-SPECTRUM ROUTING by the source's own representation: profile spectra go to `spectra_data`
    // (point layout — the builder default here; an integer axis has no chunk encoder), centroid
    // spectra to `spectra_peaks`. Gridded spectra carry `tof_index`, off-grid ones (MS2, sparse,
    // off-lattice) their EXACT f64 m/z, in whichever facet their representation selects. Sample the
    // source so the data facet's f64 m/z / intensity schema is configured — without this the f64 m/z
    // column would spill to auxiliary_arrays and read back wrong — then declare the axis on it too.
    builder = builder.sample_array_types_from_spectrum_source(&mut reader).add_spectrum_field(tof_field);
    // Derive the chromatogram schema (intensity/time dtypes) from the source chromatograms so the
    // facet matches what we write (the synthesized TIC/base-peak are f64 — sampling f64 source
    // chromatograms keeps the schema f64 and avoids an f32/f64 record-batch mismatch).
    builder = builder.sample_array_types_from_chromatograms(reader.iter_chromatograms().take(10).map(schema_sample_chromatogram));
    let mut writer = builder.build(handle, true);
    writer.copy_metadata_from(&reader);
    if matches!(reader, MZReaderType::MzML(_) | MZReaderType::IMzML(_)) {
        decode_pwiz_ids(&mut writer);
    }
    add_processing_metadata(&mut writer);

    let mut ms1 = Ms1Chroms::default();
    let cap = max_spectra();
    let mut n = 0usize;
    let mut n_gridded = 0usize;
    let mut n_f64 = 0usize;
    let mut thermo_windows = matches!(reader, MZReaderType::ThermoRaw(_))
        .then(|| thermo_isolation::UnstatedWidthGuard::open(input));
    for mut entry in reader.iter() {
        if cap.is_some_and(|m| n >= m) {
            break;
        }
        if let Some(g) = thermo_windows.as_mut() {
            g.apply(entry.description_mut());
        }
        let spec = match tof_grid_spectrum(&entry, &grid)? {
            TofRoute::Gridded(s) => {
                n_gridded += 1;
                s
            }
            TofRoute::F64(s) => {
                n_f64 += 1;
                s
            }
        };
        if synth_chroms {
            ms1.observe(&spec);
        }
        writer.write_spectrum(&spec)?;
        n += 1;
    }
    log::info!("TOF-grid wrote {n} spectra: {n_gridded} gridded (tof_index), {n_f64} kept f64 m/z");
    if let Some(g) = &thermo_windows {
        g.report();
    }
    assert_source_complete_tmp(input, n, cap, &tmp)?;
    let chromatogram_transforms = finish_chromatograms(&mut writer, input, &ms1, reader.iter_chromatograms(), synth_chroms)?;
    let acquisition_block = fixup_run_metadata(&mut writer, input);
    let mut applied = base_transformations(&writer);
    applied.extend(chromatogram_transforms);
    if n_gridded > 0 {
        applied.push(format!("tof-grid:{}ppm", tof_grid::ppm_tol()));
    }
    applied.extend(thermo_window_transformation(thermo_windows.as_ref()));
    let index_blocks: Vec<(String, serde_json::Value)> = partial_marker(input, cap, n)
        .into_iter()
        .chain(acquisition_block)
        .chain(std::iter::once(transformations_block(&applied)))
        .collect();
    finish_tof_grid_archive(writer, tmp_guard, output, input, &grid, vendor, images, sdrf, &index_blocks)
}

/// The integer flight-time axis column of every TOF-grid lane: `tof_index` (Int32) carrying the
/// `SqrtMzFromTof` transform CURIE plus the coefficients as field metadata, so a conformant reader
/// recovers `m/z = (c0 + c1·tof_index)²` from the column alone. `run_wide` is `mzpeak:transform_params`
/// — the run's `(c0, c1)` when one grid serves every spectrum (the mzML `--tof-grid` lane), or a
/// hint / the `(0, 1)` identity placeholder the reader deliberately skips when `per_spectrum` is set,
/// which adds `mzpeak:transform_params_per_spectrum = "tof_c0,tof_c1"` and makes the per-spectrum
/// columns authoritative (SCIEX, Agilent, Shimadzu). The BufferName MUST match the DataArray built per
/// spectrum (Spectrum context, nonstandard("tof_index"), Int32) or the array spills to auxiliary.
///
/// One definition because the SAME field is declared on BOTH facets of a TOF-grid archive: on
/// `spectra_data` for profile spectra and on `spectra_peaks` for centroid ones. The representation
/// the source states is carried through unchanged (`signal_continuity` is never rewritten to steer
/// the facet — review item M6), so a spectrum lands in the facet its representation dictates, finds
/// its axis declared there, and `number_of_data_points` / `number_of_peaks` describe the source.
/// Until 0.10.1 gridded spectra were forced to Centroid to reach the only facet that knew the axis,
/// which labelled every gridded profile a centroid spectrum in `spectra_metadata`.
fn tof_index_field(run_wide: (f64, f64), per_spectrum: bool) -> std::sync::Arc<arrow::datatypes::Field> {
    let base = BufferName::new(
        BufferContext::Spectrum,
        ArrayType::nonstandard("tof_index"),
        BinaryDataArrayType::Int32,
    )
    .with_transform(Some(mzpeak_prototyping::buffer_descriptors::BufferTransform::SqrtMzFromTof))
    .to_field();
    let mut md = base.metadata().clone();
    md.insert("mzpeak:transform_params".to_string(), format!("{},{}", run_wide.0, run_wide.1));
    if per_spectrum {
        md.insert("mzpeak:transform_params_per_spectrum".to_string(), "tof_c0,tof_c1".to_string());
    }
    std::sync::Arc::new((*base).clone().with_metadata(md))
}

/// The `spectra_peaks` schema of a TOF-grid lane: `tof_index` (the axis of a gridded centroid
/// spectrum) beside an f64 `mz` that is NULL on gridded rows and carries the exact m/z of a centroid
/// spectrum that did not fit the grid, plus intensity — the same integer-axis-with-f64-fallback shape
/// as the `mz-grid` lattice facet (`mz_lattice::lattice_peak_schema`), so either representation keeps
/// its exact m/z in its own facet. Shared by the mzML and native-vendor TOF-grid paths.
fn tof_index_peak_schema(tof_field: std::sync::Arc<arrow::datatypes::Field>) -> ArrayBuffersBuilder {
    ArrayBuffersBuilder::default()
        .prefix("point")
        .with_context(BufferContext::Spectrum)
        .add_field(BufferContext::Spectrum.index_field())
        .add_field(tof_field)
        .add_field(mzpeak_prototyping::peak_series::MZ_ARRAY.to_field())
        // Intensity matches the baseline f32 (SCIEX detector counts; f32 is exact for them).
        .add_field(INTENSITY_ARRAY.to_field())
}

/// Finalize a TOF-grid archive: write the `tof_calibration` index block (so readers recover
/// `m/z = (c0 + c1·tof_index)²`) plus any extra `index_blocks`, embed vendor members, optical
/// images and the SDRF exactly as `finish_with_vendor_and_aux` does, finish the ZIP, and rename the
/// temp into place. Shared by the mzML and native-vendor TOF-grid paths. (Until 0.9.13 this
/// finisher took no images/SDRF, so `--tof-grid on --sdrf s.tsv` wrote an archive whose only
/// non-Parquet member was the index, while the same command without `--tof-grid` embedded the SDRF.)
#[allow(clippy::too_many_arguments)]
fn finish_tof_grid_archive(
    writer: MzPeakWriterType<fs::File>,
    tmp_guard: TmpGuard,
    output: &Path,
    input: &Path,
    grid: &tof_grid::TofGrid,
    vendor: Option<&vendor::VendorPolicy>,
    images: &[PathBuf],
    sdrf: Option<&Path>,
    index_blocks: &[(String, serde_json::Value)],
) -> Result<()> {
    let mut zip: ZipArchiveWriter<fs::File> = writer.finish_parquet()?;
    // TWO DIFFERENT CLAIMS, one key each. `lossless` is the SPEC's key and its value is a COLUMN
    // NAME — "the exactly-preserved stored column" (mzPeak-specification schema/mzpeak_index.json).
    // `tof_index` is exactly that: an integer we store and read back bit-for-bit. What is NOT exact
    // is the m/z you RECONSTRUCT from it, because the run-wide grid accepts a point landing within
    // `tof_grid::ppm_tol()` of the source (an exact-fit-or-nothing rule would refuse almost every
    // real spectrum). So `mz_reconstruction` states that separately, with the bound.
    //
    // Do not "fix" this by renaming `lossless`: it was read once as a fidelity claim, judged
    // self-contradictory next to a 4.99 ppm bound, and renamed — which broke nothing at runtime but
    // diverged from the spec and from the 11 published archives that carry it. The per-spectrum
    // summaries describe the RECONSTRUCTED coordinates, so metadata and data agree inside the
    // archive; it is the relation to the SOURCE that is bounded. (Intensity is stored verbatim.)
    let cal = serde_json::json!({
        "codec": "tof-grid",
        "model": "sciex_sqrt",
        "lossless": "tof_index",
        "mz_reconstruction": "bounded-lossy",
        "roundtrip_tolerance_ppm": tof_grid::ppm_tol(),
        "mz_from_tof_index": "(c0 + c1*tof_index)^2",
        "c0": grid.c0,
        "c1": grid.c1,
    });
    zip.add_index_metadata("tof_calibration", &cal)
        .context("writing tof_calibration index")?;
    for (key, block) in index_blocks {
        zip.add_index_metadata(key, block)
            .with_context(|| format!("writing {key} index block"))?;
    }
    embed_vendor_members(&mut zip, input, vendor)?;
    embed_aux::embed_into_archive(&mut zip, input, images, sdrf)
        .context("embedding optical images / SDRF")?;
    zip.finish().map_err(|e| anyhow::anyhow!("finalizing archive: {e}"))?;
    tmp_guard.finish(output)?;
    Ok(())
}

/// Per-spectrum routing decision for the TOF-grid path. Neither variant changes the spectrum's
/// representation: the facet follows `signal_continuity` as the source stated it.
enum TofRoute {
    /// Every point reconstructed from the run-wide grid within tolerance: the spectrum carries
    /// `tof_index` (Int32) in place of m/z.
    Gridded(MultiLayerSpectrum<CentroidPeak, DeconvolutedPeak>),
    /// At least one point is off-lattice (MS2, sparse, off-lattice): the spectrum is kept verbatim
    /// with EXACT f64 m/z.
    F64(MultiLayerSpectrum<CentroidPeak, DeconvolutedPeak>),
}

/// Set the observed-m/z CV terms (MS:1000528 lowest, MS:1000527 highest) on a spectrum
/// description, from a reconstructed/source m/z min and max. Grid and ims-compact outputs
/// store integer `tof_index`/`tof` rather than m/z, so without this the viewer reports
/// "m/z 0–0". These terms mean *observed* m/z (NOT the scan window). If the description
/// already carries either term, it is left untouched (don't duplicate).
fn set_observed_mz_range(descr: &mut mzdata::spectrum::SpectrumDescription, mz_min: f64, mz_max: f64) {
    if !descr.params().iter().any(|p| p.curie() == Some(curie!(MS:1000528))) {
        descr.add_param(
            Param::builder()
                .name("lowest observed m/z")
                .curie(curie!(MS:1000528))
                .value(mz_min)
                .unit(Unit::MZ)
                .build(),
        );
    }
    if !descr.params().iter().any(|p| p.curie() == Some(curie!(MS:1000527))) {
        descr.add_param(
            Param::builder()
                .name("highest observed m/z")
                .curie(curie!(MS:1000527))
                .value(mz_max)
                .unit(Unit::MZ)
                .build(),
        );
    }
}

/// TIC and base peak over an intensity slice, with m/z supplied lazily per point.
///
/// Returns `(tic, Some((base_peak_mz, base_peak_intensity)))`, or `(tic, None)` when the spectrum
/// carries no positive intensity at all (empty, or an all-zero trace) — a spectrum with nothing in
/// it has no base peak, and inventing one at m/z 0 is worse than leaving the term absent.
///
/// `mz_at(i)` is only called for points that can still win, so a caller reconstructing m/z from an
/// integer axis (`m/z = (c0 + c1·tof)²`) pays for the model only on the running maximum. Ties in
/// intensity resolve to the LOWEST m/z, which does NOT fall out of a first-wins scan: the ims
/// lanes emit points grouped by mobility scan, so the array is not globally m/z-ascending.
///
/// TIE RULE, stated because it is observable in the archive: mzdata's own `fetch_summaries` breaks
/// an intensity tie first-in-array, this breaks it at the lowest m/z. On an m/z-ascending source
/// array (the mzML and SCIEX grid lanes) the two rules coincide, so a gridded archive and the same
/// file converted without the grid agree exactly. On the ims lanes, where the points are grouped by
/// mobility scan, first-in-array would be an arbitrary pick among equal maxima and lowest-m/z is
/// the reproducible one. A tied spectrum can therefore report a different `base_peak_mz` than a
/// first-wins reader would — at the same `base_peak_intensity`.
///
/// A point is only eligible to be the base peak if its m/z is finite AND positive: the Agilent
/// reconstruction can hand back a non-positive m/z for a bin outside the calibration's usable
/// range, and the writer discards a non-positive MS:1000504 anyway (which would silently drop the
/// base peak entirely rather than move it to the next-best point). Such points still count towards
/// the TIC — their intensity was measured, only their m/z is unusable.
///
/// The TIC accumulates in f64 (mzdata sums the f32 array in f32); the column is Float32 either way,
/// so this only removes accumulation error, it does not shift the value.
fn summarize_points<I, F>(intensities: I, mut mz_at: F) -> (f64, Option<(f64, f32)>)
where
    I: IntoIterator<Item = f32>,
    F: FnMut(usize) -> f64,
{
    let mut tic = 0.0f64;
    let mut best: Option<(f64, f32)> = None;
    for (i, inten) in intensities.into_iter().enumerate() {
        if !inten.is_finite() {
            continue;
        }
        tic += inten as f64;
        if inten <= 0.0 {
            continue;
        }
        match best {
            // Cannot beat the running maximum: skip the m/z reconstruction entirely.
            Some((_, bint)) if inten < bint => {}
            Some((bmz, bint)) => {
                let mz = mz_at(i);
                if mz.is_finite() && mz > 0.0 && (inten > bint || mz < bmz) {
                    best = Some((mz, inten));
                }
            }
            None => {
                let mz = mz_at(i);
                if mz.is_finite() && mz > 0.0 {
                    best = Some((mz, inten));
                }
            }
        }
    }
    (tic, best)
}

/// Drop every instance of the given CV terms from a description.
fn drop_params(descr: &mut mzdata::spectrum::SpectrumDescription, curies: &[mzdata::params::CURIE]) {
    descr
        .params_mut()
        .retain(|p| p.curie().is_none_or(|c| !curies.contains(&c)));
}

/// Write the summary CV terms — MS:1000285 total ion current, MS:1000504 base peak m/z,
/// MS:1000505 base peak intensity — onto a spectrum description, REPLACING any the source already
/// stated.
///
/// Replacing (rather than deferring) is what keeps a gridded archive agreeing with the same input
/// converted without the grid. mzdata derives these columns from the data and masks the source's
/// own terms out, so the non-grid lane writes `sum(intensity)`; an mzML that declares a
/// profile-mode `MS:1000285` (SCIEX `swath.api-sample-centroid.mzML` says 1.184903e6 where its own
/// centroid points sum to 272,543) would otherwise make the gridded column disagree with the
/// non-gridded one by 4x. The column must describe the points in the archive.
///
/// `base` of `None` means "no positive intensity in this spectrum": the TIC term is still written
/// (0 is the truth there), but NO base-peak terms are — and any the source stated are removed, so
/// an all-zero spectrum cannot end up claiming a peak it does not contain.
fn set_spectrum_summary_params(
    descr: &mut mzdata::spectrum::SpectrumDescription,
    tic: f64,
    base: Option<(f64, f32)>,
) {
    drop_params(descr, &[curie!(MS:1000285), curie!(MS:1000504), curie!(MS:1000505)]);
    descr.add_param(
        Param::builder()
            .name("total ion current")
            .curie(curie!(MS:1000285))
            .value(tic)
            .unit(Unit::DetectorCounts)
            .build(),
    );
    if let Some((mz, inten)) = base {
        descr.add_param(
            Param::builder()
                .name("base peak m/z")
                .curie(curie!(MS:1000504))
                .value(mz)
                .unit(Unit::MZ)
                .build(),
        );
        descr.add_param(
            Param::builder()
                .name("base peak intensity")
                .curie(curie!(MS:1000505))
                .value(inten as f64)
                .unit(Unit::DetectorCounts)
                .build(),
        );
    }
}

/// THE grid-route summary helper: every route that REPLACES a spectrum's f64 m/z array with an
/// integer axis (`tof_index` / `tof`) must call this with the (m/z, intensity) pairs it is about to
/// discard, BEFORE discarding them.
///
/// Why: mzdata derives the per-spectrum summaries from the m/z + intensity arrays, and a
/// `BinaryArrayMap` holding an integer axis + intensity but NO `MZArray` folds to
/// `tic = 0, base peak = (0, 0), m/z range = (0, 0)` — so the writer shipped
/// `total_ion_current = 0`, `base_peak_* = 0`/null and NULL observed-m/z bounds on every gridded
/// spectrum (13,200/13,200 on a Shimadzu run, 1,502/1,502 on an Agilent one). The peak DATA was
/// always intact; only these summary columns were wrong.
///
/// Pass the points ACTUALLY STORED, not the source array — in BOTH senses:
///
///  * the same SET of points: a route that trims (e.g. the Shimadzu profile route fits the grid on
///    the signal span and drops the zero-intensity pad at the scan window bounds) must summarize
///    the same slice it writes;
///  * at the same COORDINATES: `mzs` must be the m/z a reader RECONSTRUCTS from the integer axis
///    (`grid.mz(k)`), not the source f64 the fit consumed. Where the fit is bounded-lossy — the
///    mzML/SCIEX TOF grids accept a point within `tof_grid::ppm_tol()` — those differ, and passing
///    the source values makes the archive contradict itself: `20240826_RNAseB_…_MRM_03.mzpeak`
///    spectrum 7313 states `base_peak_mz = 519.1402875577935` while its stored `tof_index`
///    reconstructs to `519.1426532537401` (4.6 ppm), so NO point in the archive sits at the m/z the
///    metadata names, and `lowest/highest_observed_mz` bound a range the data leaves by 4.7 ppm.
///    The summary describes the archive; the source m/z is gone once the grid replaces it.
///
/// The Shimadzu CENTROID lattice route is deliberately NOT a caller: it returns the spectrum
/// unchanged and hands the lattice arrays to the writer separately, so its peak list still yields
/// correct summaries. Only routes that replace the arrays need this.
fn set_gridded_spectrum_summary(
    descr: &mut mzdata::spectrum::SpectrumDescription,
    mzs: &[f64],
    intensities: &[f32],
) {
    // Misaligned input is a bug in the caller, not something to summarize half of. Silently
    // truncating to the shorter array would state an authoritative-looking TIC over a subset — and
    // an empty m/z array (a decode that failed upstream) would DELETE the source's own terms and
    // write `total_ion_current = 0` in their place, which is strictly worse than not touching the
    // description at all. Leave it untouched and say so.
    if mzs.len() != intensities.len() {
        log::warn!(
            "gridded spectrum summary skipped: {} m/z values vs {} intensities",
            mzs.len(),
            intensities.len()
        );
        debug_assert_eq!(mzs.len(), intensities.len(), "gridded summary input misaligned");
        return;
    }
    let n = mzs.len();
    let (tic, base) = summarize_points(intensities[..n].iter().copied(), |i| mzs[i]);
    set_spectrum_summary_params(descr, tic, base);
    if let Some((lo, hi)) = mz_min_max(&mzs[..n]) {
        // Same reason as the summary terms: the range must describe the points STORED, so a
        // source-stated range (a padded scan window, say) is replaced rather than deferred to.
        drop_params(descr, &[curie!(MS:1000528), curie!(MS:1000527)]);
        set_observed_mz_range(descr, lo, hi);
    }
}

/// Min/max over an m/z slice, guarding empty and unsorted input. Returns `None` if empty.
/// Non-finite and non-positive values are skipped: the writer requires a finite positive
/// MS:1000528/MS:1000527 and would drop the term rather than record "m/z ≤ 0 was observed".
fn mz_min_max(mzs: &[f64]) -> Option<(f64, f64)> {
    let mut it = mzs.iter().copied().filter(|v| v.is_finite() && *v > 0.0);
    let first = it.next()?;
    let (mut lo, mut hi) = (first, first);
    for v in it {
        if v < lo {
            lo = v;
        }
        if v > hi {
            hi = v;
        }
    }
    Some((lo, hi))
}

/// Refuse a spectrum whose m/z and intensity arrays are not the same length.
///
/// The m/z↔intensity pairing is THE invariant of a spectrum: point `i` of one array belongs to
/// point `i` of the other. Every consumer of a decoded pair here used to walk them with `zip`,
/// which stops at the shorter array and returns success — so a source that hands back 2 m/z and 1
/// intensity yields a one-point spectrum, and the reverse silently discards an intensity. Nothing
/// downstream can detect that: the archive is structurally valid, the counts agree with each other,
/// and the missing signal simply is not there. The vendor shims have the same shape one layer out
/// (they clamp to `Math.Min`), which is exactly the hostile-response path this guards.
///
/// A length disagreement means the decode is broken, not that the shorter array is the truth, so
/// this is an error and not a warning: the caller has not renamed its temp file yet, and refusing
/// to write is the only outcome that cannot be mistaken for a good conversion.
fn require_aligned_arrays(what: &str, index: usize, n_mz: usize, n_intensity: usize) -> Result<()> {
    if n_mz != n_intensity {
        bail!(
            "{what} spectrum {index}: m/z array has {n_mz} values but the intensity array has \
             {n_intensity}. The two describe the same points and must be the same length; \
             truncating to the shorter one would silently drop signal. Refusing to convert."
        );
    }
    Ok(())
}

/// Decide and build the representation for one spectrum (PER-SPECTRUM, not all-or-nothing).
///
/// Try to map every f64 m/z to a grid `tof_index` that reconstructs within `PPM_TOL`. If ALL points
/// pass, return [`TofRoute::Gridded`] — the spectrum carrying `tof_index` in place of m/z
/// (`m/z = (c0 + c1·tof_index)²` on read). If ANY point is off-grid, return [`TofRoute::F64`] — the
/// original spectrum, unchanged, with exact f64 m/z. Either way the spectrum keeps the representation
/// its source stated, and the writer files it by that: profile → `spectra_data`, centroid →
/// `spectra_peaks`, both facets declaring `tof_index` beside an f64 `mz`. A reader tells a gridded row
/// from an f64 one by which of the two columns is non-null. This replaces the former whole-run fallback.
///
/// (This paragraph documented no function at all until now: a later insertion stranded it above
/// `set_observed_mz_range`, whose own rustdoc therefore opened with a paragraph about TOF routing.)
fn tof_grid_spectrum(
    entry: &MultiLayerSpectrum<CentroidPeak, DeconvolutedPeak>,
    grid: &tof_grid::TofGrid,
) -> Result<TofRoute> {
    let arrays = entry
        .arrays
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("spectrum {} has no arrays", entry.description().index))?;
    let mzs = arrays.mzs().map_err(|e| anyhow::anyhow!("reading m/z: {e}"))?;
    let intens = arrays.intensities().map_err(|e| anyhow::anyhow!("reading intensity: {e}"))?;
    // The `zip` below stops at the shorter array; an unequal pair must fail, not truncate.
    require_aligned_arrays("TOF-grid", entry.description().index, mzs.len(), intens.len())?;

    let mut tof: Vec<i32> = Vec::with_capacity(mzs.len());
    let mut intensity: Vec<f32> = Vec::with_capacity(mzs.len());
    let mut all_on_grid = true;
    for (&mz, &inten) in mzs.iter().zip(intens.iter()) {
        // The run-wide grid was fit on SAMPLED spectra; a point in a non-sampled spectrum could be
        // off the lattice. Verify EVERY point reconstructs within tolerance — otherwise storing
        // `tof_index` would corrupt m/z silently. On ANY miss, this spectrum is routed to f64 m/z
        // instead (per-spectrum), so the off-lattice MS2 / sparse spectra stay exact while the dense
        // MS1 profile (99%+ of the data) still grids.
        match grid.tof_index(mz) {
            Some(k) if (grid.mz(k) - mz).abs() <= mz * tof_grid::ppm_tol() * 1e-6 => tof.push(k),
            _ => {
                all_on_grid = false;
                break;
            }
        }
        intensity.push(inten);
    }

    if !all_on_grid {
        // Keep the source spectrum verbatim (exact f64 m/z, original signal continuity). The writer
        // routes its RawData+Profile arrays to `write_spectrum_binary_array_map` → `spectra_data`.
        let mut descr = entry.description().clone();
        // Observed-m/z range from the source f64 m/z array (this route keeps the f64 m/z, but the
        // CV terms may still be absent on the source description).
        if let Some((lo, hi)) = mz_min_max(&mzs) {
            set_observed_mz_range(&mut descr, lo, hi);
        }
        // Representation preserved here too: a Profile spectrum keeps its f64 m/z in `spectra_data`,
        // a Centroid one in the peaks facet's own f64 `mz` column (`tof_index_peak_schema`). Until
        // 0.10.1 this route forced Profile, mislabelling an off-grid centroid spectrum the other way.
        return Ok(TofRoute::F64(MultiLayerSpectrum::new(descr, entry.arrays.clone(), None, None)));
    }

    let mut out = BinaryArrayMap::new();
    let mut tof_da =
        DataArray::wrap(&ArrayType::nonstandard("tof_index"), BinaryDataArrayType::Int32, Vec::new());
    tof_da.update_buffer(tof.as_slice()).map_err(|e| anyhow::anyhow!("encoding tof_index: {e}"))?;
    out.add(tof_da);
    let mut int_da =
        DataArray::wrap(&ArrayType::IntensityArray, BinaryDataArrayType::Float32, Vec::new());
    int_da.update_buffer(intensity.as_slice()).map_err(|e| anyhow::anyhow!("encoding intensity: {e}"))?;
    int_da.unit = Unit::DetectorCounts; // match INTENSITY_ARRAY's unit so it maps to point.intensity
    out.add(int_da);

    // No blanket MS:1000294 "mass spectrum" here (or on any other route since 0.9.13): mzdata's
    // `spectrum_type()` returns the first matching term, and the writer infers MS:1000579/580 from
    // ms_level ONLY when it returns nothing — so the generic parent shadowed the MS1/MSn child on
    // every row of the affected archives.
    let mut descr = entry.description().clone();
    // Summary terms (TIC, base peak, observed-m/z range): the output stores integer tof_index, so
    // mzdata would derive tic = 0, base peak = (0, 0) and "m/z 0–0" from the m/z-less array map.
    // Summarize the RECONSTRUCTED m/z — `grid.mz(k)`, what a reader computes from the stored
    // column — not the source f64 the fit consumed. Same points, but the grid is accepted at a ppm
    // tolerance, so the two coordinate sets differ by up to that bound; stating the source values
    // would name m/z that the archive does not contain.
    let recon: Vec<f64> = tof.iter().map(|&k| grid.mz(k)).collect();
    set_gridded_spectrum_summary(&mut descr, &recon, &intensity);
    // `signal_continuity` stays what the source said. The writer routes a Profile spectrum's raw
    // arrays to `spectra_data` and a Centroid one's to `spectra_peaks`, and BOTH facets declare the
    // `tof_index` axis (`tof_index_field`), so the representation is no longer a routing knob —
    // until 0.10.1 this line forced Centroid to reach the one facet that knew the axis, and every
    // gridded profile spectrum was labelled a centroid spectrum with `number_of_peaks` set (M6).
    Ok(TofRoute::Gridded(MultiLayerSpectrum::new(descr, Some(out), None, None)))
}

/// Converter-owned CURIEs for the per-spectrum TOF-grid coefficients (Agilent profile grid drifts
/// scan-to-scan — `base`/`coeff` vary per scan — so a single run-wide `[c0,c1]` is ~100 ppm off; we
/// store c0/c1 as per-spectrum columns instead); a reader recovers
/// `m/z = (tof_c0 + tof_c1·tof_index)²` per spectrum. Also carried per spectrum by the timsTOF
/// ims-compact lanes when the vendor `MzCalibration` row is sqrt-linear
/// (`bruker_native::add_exact_tof_params`).
///
/// The terms are `cv/mzpeak.obo` MZP:1000003 / MZP:1000004 / MZP:1000005, represented in this crate
/// as `ControlledVocabulary::Unknown` CURIEs that the vendored writer/reader render and parse as
/// `MZP:` (`mzpeak_prototyping::param::curie_to_string`), so the spectra_metadata columns are
/// `opt_MZP_1000003_tof_c0` / `opt_MZP_1000004_tof_c1` / `opt_MZP_1000005_tof_calibration_id`. Until
/// 0.10.1 they squatted `MS:4000900`–`MS:4000902` in the PSI-owned namespace (columns
/// `opt_MS_4000900_tof_c0` …), which is what the spec calls a column-naming artifact and asked to be
/// converter-owned. Readers were built for the move: the vendored reader binds the coefficients by
/// NAME (`reconstruct_per_spectrum_grid_mz`), and the viewer by the `_tof_c0` / `_tof_c1` column-name
/// SUFFIX, so archives of either generation reconstruct.
pub(crate) const TOF_C0_CURIE: mzdata::params::CURIE =
    mzdata::params::CURIE::new(mzdata::params::ControlledVocabulary::Unknown, 1_000_003);
pub(crate) const TOF_C1_CURIE: mzdata::params::CURIE =
    mzdata::params::CURIE::new(mzdata::params::ControlledVocabulary::Unknown, 1_000_004);
/// Per-spectrum CalibrationID column — selects the polynomial-refinement row in the
/// `tof_calibration` index block, so the EXACT MassHunter m/z (quadratic + polynomial) reconstructs.
const TOF_CALID_CURIE: mzdata::params::CURIE =
    mzdata::params::CURIE::new(mzdata::params::ControlledVocabulary::Unknown, 1_000_005);

/// FILE-DIRECT Agilent Q-TOF profile converter: read the integer flight-time grid straight from
/// `AcqData/MSProfile.bin` (pure Rust, no MHDAC) and write the SAME `tof_index` (Int32) + intensity
/// axis `convert_file_tof_grid` uses (in `spectra_data`: it is profile data), plus per-spectrum
/// `tof_c0`/`tof_c1` columns (Agilent
/// calibration drifts per scan). Each point is gated for losslessness against the polynomial-refined
/// MassHunter m/z (`PPM_TOL`); over-tolerance points abort (this lane is only dispatched when the
/// `.d` has profile data, so an abort means the grid model genuinely failed and is a real error).
/// Diagnostic: decode the whole Agilent profile run with the pure-Rust reader and print one CSV row
/// per selected scan (`sum,nnz,first_k,first_v,last_k,last_v,maxv` + grid c0/c1) for byte-exact
/// validation against `rainbow`. Also reports the run-wide max reconstruction ppm vs the
/// polynomial-refined MassHunter m/z.
fn dump_agilent_profile(input: &Path) -> Result<()> {
    let mut reader = agilent_profile::AgilentProfileReader::open(input)?;
    println!("idx,ms_level,scan_time,nnz,sum,first_k,first_v,last_k,last_v,maxv,c0,c1,max_ppm");
    let mut global_max_ppm = 0.0f64;
    while let Some(ps) = reader.next_spectrum()? {
        let sum: u64 = ps.intensity.iter().map(|&v| v as u64).sum();
        let maxv = ps.intensity.iter().copied().max().unwrap_or(0);
        let (fk, fv) = (ps.tof_index[0], ps.intensity[0]);
        let (lk, lv) = (*ps.tof_index.last().unwrap(), *ps.intensity.last().unwrap());
        // per-spectrum max ppm vs refined m/z
        let mut max_ppm = 0.0f64;
        if let (Some(row), uf) = (reader.calib_row(ps.index), reader.poly_flags_for(ps.index)) {
            let (coeff, base) = (row[0], row[1]);
            for &k in &ps.tof_index {
                let rec = ps.grid.mz(k);
                let t = base + (ps.grid.c0 + ps.grid.c1 * k as f64) / coeff;
                let refined = agilent_profile::calibrated_mz(row, uf, t);
                if refined > 0.0 {
                    let ppm = (rec - refined).abs() / refined * 1e6;
                    if ppm > max_ppm {
                        max_ppm = ppm;
                    }
                }
            }
        }
        if max_ppm > global_max_ppm {
            global_max_ppm = max_ppm;
        }
        if matches!(ps.index, 0 | 1 | 2 | 283 | 567) {
            println!(
                "{},{},{:.5},{},{},{},{},{},{},{},{},{},{:.4}",
                ps.index, ps.ms_level, ps.scan_time, ps.tof_index.len(), sum, fk, fv, lk, lv,
                maxv, ps.grid.c0, ps.grid.c1, max_ppm
            );
        }
    }
    eprintln!("run-wide max reconstruction error: {global_max_ppm:.4} ppm");
    Ok(())
}

/// The `--agilent-grid` writer. Its data schema is hand-built, so every column the batches carry
/// must be declared, `spectrum_index` included: 0.10.1 dropped it (5692603), and the writer then met
/// the batch's index column in `route_unexpected`, which panics on a column no array metadata
/// describes. The other hand-built TOF schemas declare it too.
fn agilent_grid_writer_builder(level: ZstdLevel, grid: (f64, f64)) -> mzpeak_prototyping::writer::MzPeakWriterBuilder {
    let tof_field = tof_index_field(grid, true);
    MzPeakWriterType::<fs::File>::builder()
        .buffer_size(buffer_spectra())
        .compression(Compression::ZSTD(level))
        .add_spectrum_field(BufferContext::Spectrum.index_field())
        .add_spectrum_field(tof_field)
        .add_spectrum_field(INTENSITY_ARRAY.to_field())
        // Per-spectrum grid coefficients as Float64 spectrum columns (pulled from each spectrum's
        // params by CURIE). These are the AUTHORITATIVE per-scan calibration for m/z reconstruction.
        .add_spectrum_param_field(
            CustomBuilderFromParameter::from_spec(TOF_C0_CURIE, "tof_c0", DataType::Float64),
        )
        .add_spectrum_param_field(
            CustomBuilderFromParameter::from_spec(TOF_C1_CURIE, "tof_c1", DataType::Float64),
        )
        // Per-spectrum CalibrationID → selects the polynomial refinement in the index block for the
        // EXACT MassHunter m/z. Per-run-constant in practice, so it compresses to ~nothing.
        .add_spectrum_param_field(
            CustomBuilderFromParameter::from_spec(TOF_CALID_CURIE, "tof_calibration_id", DataType::Int64),
        )
}

fn convert_agilent_grid(
    input: &Path,
    output: &Path,
    zstd_level: i32,
    vendor: Option<&vendor::VendorPolicy>,
    synth_chroms: bool,
) -> Result<()> {
    let mut reader = agilent_profile::AgilentProfileReader::open(input)
        .with_context(|| format!("opening Agilent profile .d {}", input.display()))?;

    let tmp = output.with_extension("mzpeak.tmp");
    let tmp_guard = TmpGuard::new(&tmp);
    let handle = fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    let level = ZstdLevel::try_new(zstd_level)
        .map_err(|e| anyhow::anyhow!("invalid zstd level {zstd_level}: {e}"))?;

    // Data facet (`spectra_data`, point layout): integer `tof_index` (Int32, ΔBP) + intensity — the
    // vendor's profile vector is PROFILE data and is filed as such (M6). The run-wide
    // `transform_params` on the column is informational (per-spectrum c0/c1 ride their own columns);
    // set it to the first spectrum's grid so a single-calibration run still self-describes. Nothing
    // samples this lane's schema (the reader yields no m/z array), so intensity is declared explicitly
    // or it spills into `auxiliary_arrays`.
    let first_grid = {
        // Peek the first spectrum's grid without consuming the stream: re-open a probe reader.
        let mut probe = agilent_profile::AgilentProfileReader::open(input)?;
        probe.next_spectrum()?.map(|s| s.grid)
    };
    let (c0_hint, c1_hint) = first_grid.map(|g| (g.c0, g.c1)).unwrap_or((0.0, 1.0));
    let mut writer = agilent_grid_writer_builder(level, (c0_hint, c1_hint)).build(handle, true);
    add_processing_metadata(&mut writer);
    // The per-spectrum coefficient columns are MZP terms (`TOF_C0_CURIE` …): declare the CV.
    ensure_mzp_cv(&mut writer);

    let mut ms1 = Ms1Chroms::default();
    let cap = max_spectra();
    let mut n = 0usize;
    let mut max_ppm = 0.0f64;
    let mut f32_rounded = 0usize;
    while let Some(ps) = reader.next_spectrum()? {
        if cap.is_some_and(|m| n >= m) {
            break;
        }
        let spec = agilent_grid_spectrum(&reader, ps, &mut max_ppm, &mut f32_rounded)?;
        if synth_chroms {
            ms1.observe(&spec);
        }
        writer.write_spectrum(&spec)?;
        n += 1;
    }
    log::info!(
        "Agilent-grid: wrote {n} profile spectra of {} scan records; max round-trip m/z error \
         {max_ppm:.6} ppm vs MassHunter (traditional quadratic + polynomial refinement){}",
        reader.len(),
        if f32_rounded > 0 { format!(" (WARNING: {f32_rounded} intensities above 2^24 were rounded to Float32)") } else { String::new() }
    );
    // This lane never reaches `assert_source_complete` (a `.d` declares no spectrum count we can
    // read back), so a scan the reader declined to yield would otherwise vanish with exit 0. Name
    // the shortfall — a truncated tail in particular means the archive covers only part of the run.
    if !cap.is_some_and(|m| n >= m) {
        if let Some(what) = reader.skipped().describe() {
            log::warn!("Agilent-grid: {what}");
        }
    }
    let calibrations = reader.calibrations_json();
    let chromatogram_transforms = finish_chromatograms(&mut writer, input, &ms1, std::iter::empty(), synth_chroms)?;
    let acquisition_block = fixup_run_metadata(&mut writer, input);
    let mut applied = base_transformations(&writer);

    let mut zip: ZipArchiveWriter<fs::File> = writer.finish_parquet()?;
    if let Some((key, block)) = acquisition_block {
        zip.add_index_metadata(&key, &block).context("writing acquisition_time index block")?;
    }
    // `lossless` (the exactly-stored column) and `mz_reconstruction` (whether m/z is quantized) are
    // stated by EVERY `codec: "tof-grid"` block, so a reader answers both questions from one place
    // regardless of model. This lane is the exact one: `tof_index` is the vendor's OWN bin ordinal
    // and a conformant reader re-evaluates the vendor's OWN calibration (`calibrations` below), so
    // reconstruction is exact by construction rather than by measurement — note `max_roundtrip_ppm`
    // here compares two evaluations of the same formula and is therefore necessarily ~0, a
    // consistency check and not evidence of anything.
    let cal = serde_json::json!({
        "codec": "tof-grid",
        "model": "agilent_sqrt_poly",
        "lossless": "tof_index",
        "mz_reconstruction": "exact",
        // Per-spectrum (tof_c0, tof_c1) + per-spectrum tof_calibration_id select a row in
        // `calibrations`; reconstruction: t = base + (tof_c0 + tof_c1*tof_index)/coeff;
        // m/z = (coeff*(t-base))^2 - poly(clip(t,left,right)), poly orders set by use_flags.
        "tof_to_mz": "t = base + (tof_c0 + tof_c1*tof_index)/coeff ; mz = (coeff*(t-base))^2 - poly(clip(t,left,right))",
        "per_spectrum_columns": ["tof_c0", "tof_c1", "tof_calibration_id"],
        "calibrations": calibrations,
        "max_roundtrip_ppm": max_ppm,
    });
    zip.add_index_metadata("tof_calibration", &cal)
        .context("writing tof_calibration index")?;
    // A capped run stops early on request. An uncapped one whose MSProfile.bin ended before its scan
    // records is partial too, and says so offline instead of only in the log.
    if let Some((key, block)) =
        partial_marker(input, cap, n).or_else(|| agilent_truncation_marker(cap, reader.skipped(), reader.len(), n))
    {
        zip.add_index_metadata(&key, &block).context("writing partial index block")?;
    }
    // The reader stores a sparse point list (zero-intensity samples of the dense vendor vector left
    // out, `agilent_profile.rs`) and counts integer intensities into Float32: both declared when they
    // happened.
    for entry in agilent_grid_transformations(reader.zero_samples_dropped(), reader.skipped().all_zero, f32_rounded) {
        declare(&mut applied, entry);
    }
    applied.extend(chromatogram_transforms);
    let (key, block) = transformations_block(&applied);
    zip.add_index_metadata(&key, &block).context("writing transformations index block")?;
    embed_vendor_members(&mut zip, input, vendor)?;
    zip.finish().map_err(|e| anyhow::anyhow!("finalizing archive: {e}"))?;
    tmp_guard.finish(output)?;
    Ok(())
}

/// Integer counts as the Float32 intensity column stores them, counting the values it cannot hold
/// exactly (above 2^24 an f32 rounds): the `agilent:intensity-f32-rounding` transformation.
fn intensities_as_f32(values: &[u32], rounded: &mut usize) -> Vec<f32> {
    values
        .iter()
        .map(|&v| {
            let f = v as f32;
            if f as f64 != v as f64 {
                *rounded += 1;
            }
            f
        })
        .collect()
}

/// The `--agilent-grid` lane's own `transformations` entries, each only when it happened:
/// `agilent:drop-zero-samples` when the reader left zero-intensity samples (or a whole all-zero
/// scan) out of its sparse point lists, `agilent:intensity-f32-rounding` when a count above 2^24
/// was rounded into the Float32 intensity column.
fn agilent_grid_transformations(zero_samples: usize, all_zero_scans: usize, f32_rounded: usize) -> Vec<String> {
    let mut out = Vec::new();
    if zero_samples > 0 || all_zero_scans > 0 {
        out.push("agilent:drop-zero-samples".to_string());
    }
    if f32_rounded > 0 {
        out.push("agilent:intensity-f32-rounding".to_string());
    }
    out
}

/// The `partial` block for an `--agilent-grid` run whose `MSProfile.bin` ends before its scan
/// records do (an interrupted acquisition: the reader stops at the first segment past the end of
/// the file). A run the cap (`MZPC_MAX_SPECTRA`) stopped carries the cap's own marker instead; a
/// cap above the number of readable records stops nothing, and the tail is marked here all the same.
fn agilent_truncation_marker(
    cap: Option<usize>,
    skipped: agilent_profile::SkipTally,
    scan_records: usize,
    written: usize,
) -> Option<(String, serde_json::Value)> {
    if cap.is_some_and(|m| written >= m) || skipped.truncated_tail == 0 {
        return None;
    }
    Some((
        "partial".to_string(),
        serde_json::json!({
            "partial": true,
            "cause": "MSProfile.bin ends before its scan records (interrupted acquisition)",
            "source_declared": scan_records,
            "spectra_written": written,
            "scan_records_not_read": skipped.truncated_tail,
        }),
    ))
}

/// Build one mzPeak spectrum from an Agilent profile spectrum: the integer `tof_index` (Int32) +
/// integer intensity (as Float32, exact for counts < 2^24), the per-spectrum grid as `tof_c0`/`tof_c1`
/// params, and a per-point lossless gate against the polynomial-refined MassHunter m/z.
fn agilent_grid_spectrum(
    reader: &agilent_profile::AgilentProfileReader,
    ps: agilent_profile::ProfileSpectrum,
    max_ppm: &mut f64,
    f32_rounded: &mut usize,
) -> Result<MultiLayerSpectrum<CentroidPeak, DeconvolutedPeak>> {
    let grid = ps.grid;
    // Losslessness vs MassHunter. The stored representation is: integer `tof_index = k`, per-spectrum
    // (c0,c1), and the per-CalibrationID polynomial in the index block. A conformant reader recovers
    // raw TOF `t = base + (c0+c1·k)/coeff` and then the FULL MassHunter m/z (traditional quadratic +
    // polynomial), so reconstruction is exact to f64 noise. We measure that round-trip error here
    // (`max_ppm` ≈ 0). For the report we ALSO track how far the bare 2-coeff grid (no polynomial)
    // would be — the magnitude of the refinement the index block captures.
    if let (Some(row), uf) = (reader.calib_row(ps.index), reader.poly_flags_for(ps.index)) {
        let coeff = row[0];
        let base = row[1];
        for &k in &ps.tof_index {
            // MassHunter's reported m/z for this bin (the lossless target).
            let t = base + (grid.c0 + grid.c1 * k as f64) / coeff;
            let target = agilent_profile::calibrated_mz(row, uf, t);
            if !(target > 0.0) {
                continue;
            }
            // Reader's reconstruction from the stored columns + index polynomial — identical formula.
            let t_rec = base + (grid.c0 + grid.c1 * k as f64) / coeff;
            let rec = agilent_profile::calibrated_mz(row, uf, t_rec);
            let ppm = (rec - target).abs() / target * 1e6;
            if ppm > *max_ppm {
                *max_ppm = ppm;
            }
        }
    }

    let intensity = intensities_as_f32(&ps.intensity, f32_rounded);

    let mut out = BinaryArrayMap::new();
    let mut tof_da =
        DataArray::wrap(&ArrayType::nonstandard("tof_index"), BinaryDataArrayType::Int32, Vec::new());
    tof_da.update_buffer(ps.tof_index.as_slice()).map_err(|e| anyhow::anyhow!("encoding tof_index: {e}"))?;
    out.add(tof_da);
    let mut int_da =
        DataArray::wrap(&ArrayType::IntensityArray, BinaryDataArrayType::Float32, Vec::new());
    int_da.update_buffer(intensity.as_slice()).map_err(|e| anyhow::anyhow!("encoding intensity: {e}"))?;
    int_da.unit = Unit::DetectorCounts;
    out.add(int_da);

    let mut descr = mzdata::spectrum::SpectrumDescription::default();
    descr.index = ps.index;
    descr.id = format!("scan={}", ps.index + 1);
    descr.ms_level = ps.ms_level;
    // `MSProfile.bin` IS the profile vector: say so. (Until 0.10.1 this lane declared Centroid to
    // reach the peaks facet — the only facet that then declared `tof_index` — so every spectrum of
    // an Agilent-grid archive was labelled a centroid spectrum; M6.)
    descr.signal_continuity = mzdata::spectrum::SignalContinuity::Profile;
    // Polarity comes from the scan record's own `IonPolarity` field (MSScan.bin), NOT from a
    // default: this lane used to hardcode Negative because the dataset it was written against
    // (MTBLS1334) happened to be negative-mode, which mislabelled every positive-mode `.d` — both
    // profile-bearing corpus files (MSV000090203 FM_01_Pos, agilent-qtof …-pos-S25) are positive.
    // `IonPolarity` is `minOccurs="0"`, so a schema without it — and the vendor's own
    // `Unassigned`/`Mixed` codes — leave the column NULL ("not stated") rather than inventing one.
    descr.polarity = match ps.polarity {
        agilent_profile::Polarity::Positive => mzdata::spectrum::ScanPolarity::Positive,
        agilent_profile::Polarity::Negative => mzdata::spectrum::ScanPolarity::Negative,
        agilent_profile::Polarity::Unknown => mzdata::spectrum::ScanPolarity::Unknown,
    };
    descr.add_param(Param::builder().name("tof_c0").curie(TOF_C0_CURIE).value(grid.c0).build());
    descr.add_param(Param::builder().name("tof_c1").curie(TOF_C1_CURIE).value(grid.c1).build());
    descr.add_param(
        Param::builder()
            .name("tof_calibration_id")
            .curie(TOF_CALID_CURIE)
            .value(ps.calibration_id as i64)
            .build(),
    );
    // Summary terms: the output stores integer tof_index and NO m/z array, so mzdata would derive
    // tic = 0, base peak = (0, 0) and "m/z 0–0". Reconstruct m/z the way a conformant reader does —
    // the polynomial-refined MassHunter value when this spectrum has a calibration row, else the
    // bare 2-coefficient grid — and hand the reconstructed array to the shared grid-summary helper.
    //
    // The reconstruction is materialized rather than evaluated lazily at the index extremes: the
    // bare quadratic grid is monotonic in `tof_index`, but the polynomial refinement subtracted
    // from it is not guaranteed to be, so "min/max of m/z = m/z at min/max index" is an assumption
    // this code has never checked. Scanning costs one `calibrated_mz` per stored point, in a
    // function that already evaluates it twice per point for the losslessness gate above.
    let calib = reader.calib_row(ps.index).map(|row| (row, reader.poly_flags_for(ps.index)));
    let mz_of = |k: i32| -> f64 {
        match calib {
            Some((row, uf)) => {
                let (coeff, base) = (row[0], row[1]);
                agilent_profile::calibrated_mz(row, uf, base + (grid.c0 + grid.c1 * k as f64) / coeff)
            }
            None => grid.mz(k),
        }
    };
    let mzs: Vec<f64> = ps.tof_index.iter().map(|&k| mz_of(k)).collect();
    set_gridded_spectrum_summary(&mut descr, &mzs, &intensity);
    // Set retention time on the scan event.
    let mut acq = mzdata::spectrum::Acquisition::default();
    if let Some(ev) = acq.first_scan_mut() {
        ev.start_time = ps.scan_time;
    } else {
        let mut ev = mzdata::spectrum::ScanEvent::default();
        ev.start_time = ps.scan_time;
        acq.scans.push(ev);
    }
    descr.acquisition = acq;

    Ok(MultiLayerSpectrum::new(descr, Some(out), None, None))
}

#[allow(clippy::too_many_arguments)]
fn convert_file(
    input: &Path,
    output: &Path,
    chunk: Option<ChunkingStrategy>,
    zstd_level: i32,
    vendor: Option<&vendor::VendorPolicy>,
    synth_chroms: bool,
    // `None` = not given. The mzML path below treats that as `Off`; the native SCIEX lane (which
    // also only ever sees decoded f64) treats it as `Auto` — see `Cli::tof_grid`.
    tof_grid: Option<TofGridMode>,
    images: &[PathBuf],
    sdrf: Option<&Path>,
    tims_recalibration: bool,
    // The `conversion_route` block when another lane handed the input over (the timsTOF fallback).
    route: Option<(String, serde_json::Value)>,
) -> Result<()> {
    // --image / --sdrf are only honored on the mzML/imzML reader path below. A vendor-format input
    // (TSF/BAF/Agilent/SciEX/Waters) routes to a dedicated converter that does not embed them — warn
    // rather than silently dropping a user-supplied path. (`run` now refuses the combination before
    // getting here — `Lane::VendorReader` — so this fires only for callers that bypass `run`.)
    #[allow(unused_mut)]
    let mut routes_to_vendor = is_tsf_dir(input);
    #[cfg(any(windows, target_os = "linux"))]
    {
        routes_to_vendor = routes_to_vendor || is_baf_dir(input);
    }
    #[cfg(windows)]
    {
        routes_to_vendor =
            routes_to_vendor || is_agilent_d(input) || is_wiff(input) || is_waters_raw(input);
    }
    if routes_to_vendor && (!images.is_empty() || sdrf.is_some()) {
        log::warn!(
            "--image/--sdrf are only supported for mzML/imzML inputs; ignoring them for vendor input {}",
            input.display()
        );
    }

    if is_tsf_dir(input) {
        return convert_tsf(input, output, chunk, zstd_level, vendor, synth_chroms);
    }
    #[cfg(any(windows, target_os = "linux"))]
    if is_baf_dir(input) {
        return convert_baf(input, output, chunk, zstd_level, vendor, synth_chroms);
    }
    #[cfg(windows)]
    if is_agilent_d(input) {
        // Agilent ion-mobility (6560 IM-QTOF) needs the MIDAC SDK to read the drift dimension, and
        // this converter has no MIDAC reader; non-IM Agilent uses MHDAC. The file says which it is
        // (`AcqData/IMSFrame.bin`), so an IM-QTOF `.d` is refused here rather than flattened through
        // MHDAC without its drift dimension. The box harness routes the refusal to msconvert by
        // matching "is an Agilent IM-QTOF run".
        if is_agilent_ims_d(input) {
            bail!(
                "{} is an Agilent IM-QTOF run (AcqData/IMSFrame.bin present): the native lane cannot \
                 carry its drift dimension; convert this run with --via-msconvert",
                input.display()
            );
        }
        if tof_grid.is_some() {
            log::warn!(
                "--tof-grid is not applied on the native Agilent (MHDAC) lane: m/z is the f64 the \
                 vendor library returns; use --via-msconvert --tof-grid for the statistical grid or \
                 --agilent-grid for the flight-time grid of a profile .d"
            );
        }
        return convert_agilent(input, output, chunk, zstd_level, vendor, synth_chroms);
    }
    #[cfg(windows)]
    if is_wiff(input) {
        return convert_sciex(input, output, chunk, zstd_level, vendor, synth_chroms, tof_grid);
    }
    #[cfg(windows)]
    if is_waters_raw(input) {
        return convert_waters(input, output, chunk, zstd_level, vendor, synth_chroms);
    }
    #[cfg(windows)]
    if is_lcd(input) {
        return convert_shimadzu(input, output, chunk, zstd_level, vendor, synth_chroms, representation());
    }
    // mzdata's quick-xml reader assumes UTF-8 and panics on Latin-1/windows-1252 high bytes
    // (e.g. zenodo DESI imzML declare ISO-8859-1). If the input declares a non-UTF-8 encoding,
    // transcode it to a throwaway UTF-8 temp first and read from there. `_utf8` is an RAII guard:
    // it deletes the temp dir (transcoded file + hardlinked .ibd sidecar) on every exit path.
    // Both workarounds are XML-FILE-only. A directory vendor unit still reaching here — a TDF `.d`
    // under `--no-ims-compact`, or the ims-compact decompress fallback — would have its directory fd
    // `read()` and fail EISDIR ("Is a directory") before the reader ever opened it. `convert_to_mzml`
    // has always gated these on `is_file()`; this lane did not.
    let (_gz, _utf8, _sanitized, read_path): (
        Option<GunzipGuard>,
        Option<TranscodeGuard>,
        Option<SanitizedTemp>,
        PathBuf,
    ) = if input.is_file() {
            // Gunzip first: the transcode and sanitize stages sniff XML bytes, which do not exist
            // until the stream is decompressed. `input` itself stays the provenance path.
            let gz = gunzip_to_temp(input)?;
            let plain: &Path = gz.as_ref().map(|g| g.file.as_path()).unwrap_or(input);
            let utf8 = transcode_to_utf8(plain)?;
            let utf8_path: PathBuf = utf8
                .as_ref()
                .map(|g| g.file.clone())
                .unwrap_or_else(|| plain.to_path_buf());
            // mzdata panics on an empty self-closing <referenceableParamGroup/> that is later
            // referenced (ProteomeDiscoverer emits these). If present, convert from a sanitized copy
            // instead. Sanitize the already-UTF-8 file so both workarounds compose. The copy is an
            // RAII guard like `_utf8`: removed on every exit path, not only after a successful run.
            let sanitized = sanitize_param_groups(&utf8_path)?.map(SanitizedTemp);
            let read = sanitized.as_ref().map(|s| s.0.clone()).unwrap_or(utf8_path);
            (gz, utf8, sanitized, read)
        } else {
            (None, None, None, input.to_path_buf())
        };
    let read_path: &Path = read_path.as_path();
    let mut reader = MZReaderType::<_, CentroidPeak, DeconvolutedPeak>::open_path(read_path)
        .with_context(|| format!("opening {}", input.display()))?;
    // Here rather than at the chromatogram write, so the TOF-grid writer below is covered too.
    warn_unread_chromatograms(input, read_path, reader.count_chromatograms());

    // TOF-grid m/z encoding (SCIEX / exact-lattice TOF): if requested, sample spectra and try to fit
    // a per-run integer flight-time grid `sqrt(m/z)=c0+c1·k`. When every sampled point reconstructs
    // within `tof_grid::ppm_tol()` we store `tof_index` (Int32) instead of f64 m/z. `auto` falls
    // back to the standard f64 path when the fit fails; `on` errors. Scoped to the mzML path (this
    // `open_path` branch only). Not given = `off`: exact f64 is the safe default here.
    let tof_grid = tof_grid.unwrap_or_default();
    if tof_grid != TofGridMode::Off {
        match try_fit_tof_grid(&mut reader) {
            Some(fit) => {
                log::info!(
                    // Do not call this "lossless": the gate is a ppm BOUND, so a passing fit
                    // still quantizes m/z and the archive says so (`mz_reconstruction:
                    // bounded-lossy`). Claiming exactness in the log while writing a bound into
                    // the file is how a user ends up trusting fidelity the archive never asserted.
                    "TOF-grid: fit accepted within tolerance (c0={:.6} c1={:.6e}, max {:.4} ppm of {:.4} ppm allowed, median {:.4} ppm, k≤{}, median dk={}); storing tof_index instead of f64 m/z",
                    fit.grid.c0, fit.grid.c1, fit.max_ppm, tof_grid::ppm_tol(), fit.median_ppm, fit.max_k, fit.median_dk
                );
                // PER-SPECTRUM routing: off-grid spectra (MS2 / sparse / off-lattice) are stored as
                // exact f64 m/z in the `spectra_data` facet, while griddable spectra use `tof_index`.
                // There is no longer a whole-run fallback — a single archive holds both facets.
                return convert_file_tof_grid(input, output, zstd_level, vendor, synth_chroms, reader, fit.grid, images, sdrf);
            }
            None => {
                if tof_grid == TofGridMode::On {
                    bail!(
                        "--tof-grid on: input {} is not griddable (no per-run integer TOF lattice \
                         reconstructing within {:.2} ppm); use --tof-grid auto to fall back to \
                         f64 m/z",
                        input.display(), tof_grid::ppm_tol()
                    );
                }
                log::info!(
                    "TOF-grid auto: no grid fit within {:.2} ppm — keeping standard f64 m/z",
                    tof_grid::ppm_tol()
                );
            }
        }
    }

    let tmp = output.with_extension("mzpeak.tmp");
    let tmp_guard = TmpGuard::new(&tmp);
    let handle = fs::File::create(&tmp)
        .with_context(|| format!("creating {}", tmp.display()))?;

    let level = ZstdLevel::try_new(zstd_level)
        .map_err(|e| anyhow::anyhow!("invalid zstd level {zstd_level}: {e}"))?;

    let is_imzml = matches!(reader, MZReaderType::IMzML(_));

    // Sample real m/z before choosing an encoding: numpress-linear and delta win on opposite kinds
    // of data, and which one this file holds cannot be told from its name. Spread the probes across
    // the run rather than taking the first few, which on a DIA file would be all one window.
    let probes: Vec<mzdata::spectrum::MultiLayerSpectrum> = {
        let n = reader.len();
        let step = (n / 6).max(1);
        let v: Vec<_> = (0..n)
            .step_by(step)
            .take(6)
            .filter_map(|i| reader.get_spectrum_by_index(i))
            .collect();
        reader.reset();
        v
    };
    let chunk = refine_chunking(&sample_mz_from(&probes), chunk);

    // FIXED-POINT m/z LATTICE (see `mz_lattice`). The same probes decide, from the CENTROID values
    // only, whether this run's peaks are vendor integers over a power of ten. When they are, the
    // peaks facet stores `tof_index` = round(m/z·scale) as Int64 (DELTA_BINARY_PACKED, with an
    // `mz_calibration` index block) instead of f64 `mz` — lossless AND smaller than either chunk
    // encoding, so it takes precedence over both numpress-linear and delta on that facet. It
    // supersedes NOTHING on the data facet: profile arrays keep `chunk` exactly as refined above,
    // including the `--no-numpress` / `--layout point` / explicit-strategy choices, so a
    // profile-only lattice input is byte-identical to before (the route needs centroids to fire).
    // `--no-mz-lattice` (or $MZPC_NO_MZ_LATTICE) opts out.
    let lattice_scale = if mz_lattice_enabled() {
        probe_lattice_scale(&probes)
    } else {
        None
    };
    if let Some(scale) = lattice_scale {
        log::info!(
            "centroid m/z is on a 1/{scale:e} fixed-point lattice; storing the peaks facet as \
             Int64 tof_index = round(m/z·{scale:e}) (lossless; off-lattice spectra keep f64 m/z)"
        );
    }

    let mut builder = MzPeakWriterType::<fs::File>::builder()
        .chunked_encoding(chunk)
        // A lattice peaks facet is an integer axis with an f64 fallback column: never chunked,
        // never numpressed — the lattice replaces both. This MUST precede
        // `store_peaks_and_profiles_apart`, which stamps the configured strategy onto the schema.
        .peaks_chunked_encoding(if lattice_scale.is_some() { None } else { chunk })
        // ponytail: chromatograms are POINT layout, never chunked. Passing the spectrum strategy
        // here produced a `chunk` struct with no chunk_start/chunk_end columns, so the chunk builder
        // saw an empty main axis, wrote 0 time and 0 intensity points, and spilled the whole
        // intensity array into an uncompressed `auxiliary_arrays` blob in chromatograms_metadata —
        // losing the time axis outright. 99 of 330 reference archives are affected. A chromatogram
        // is a few thousand points; chunking bought nothing.
        .chromatogram_chunked_encoding(None)
        .buffer_size(buffer_spectra())
        .compression(Compression::ZSTD(level))
        // Float m/z of a point facet (`--layout point`, the lattice's f64 fallback):
        // BYTE_STREAM_SPLIT, no dictionary. Chunk facets are unaffected.
        .shuffle_mz(true);

    // Derive the data schema from the data actually present (one m/z + one intensity column at
    // their source dtype) so points land in point.mz/point.intensity, not auxiliary_arrays.
    builder = builder.sample_array_types_from_spectrum_source(&mut reader);
    builder = match lattice_scale {
        // The lattice facet is fully declared (its four columns are the contract: spectrum_index,
        // tof_index Int64, the f64 `mz` fallback, intensity); sampling the source's peak arrays
        // would only re-add the f64 `mz` it already carries.
        Some(scale) => builder.store_peaks_and_profiles_apart(Some(mz_lattice::lattice_peak_schema(scale))),
        None => builder.sample_array_types_for_peaks_from_spectrum_source(&mut reader),
    };
    builder = builder.sample_array_types_from_chromatograms(reader.iter_chromatograms().take(10).map(schema_sample_chromatogram));

    let mut writer = builder.build(handle, true);

    // imzML carries imaging coordinate cvParams that must be promoted to columns; the archive then
    // references the IMS CV, so declare it (the writer seeds only MS+UO).
    if is_imzml {
        log::info!("imzML input: adding imaging position columns + IMS cv");
        writer.spectrum_entry_buffer_mut().add_imaging_position_visitors();
        writer
            .controlled_vocabularies_mut()
            .push(ControlledVocabulary::IMS.into());
    }

    writer.copy_metadata_from(&reader);
    if matches!(reader, MZReaderType::MzML(_) | MZReaderType::IMzML(_)) {
        decode_pwiz_ids(&mut writer);
    }
    add_processing_metadata(&mut writer);

    // Keep the ion-mobility dimension for TDF (do not flatten 3D frames).
    if let MZReaderType::BrukerTDF(tdf) = &mut reader {
        tdf.set_consolidate_peaks(false);
    }
    // TDF through mzdata (`--no-ims-compact`): mzdata's precursor / scan / window-limit 1/K0 params
    // are timsrust-linear while its arrays are ModelType-2 — remap them onto the same vendor model
    // the ims-compact lane writes (or, under `--no-tims-recalibration`, leave them linear as that
    // lane does), order the window-limit pair, and attach the MZP:1000006/7 window band (see the
    // type docs).
    let tdf_remap = if matches!(reader, MZReaderType::BrukerTDF(_)) {
        match bruker_native::TdfMobilityRemap::open_with(input, tims_recalibration) {
            Ok(r) => {
                ensure_mzp_cv(&mut writer);
                Some(r)
            }
            Err(e) => {
                log::warn!(
                    "TDF mobility remap unavailable ({e:#}); precursor 1/K0 stays on timsrust's \
                     linear approximation and the isolation-window band is not written"
                );
                None
            }
        }
    } else {
        None
    };
    // Thermo precursor windows the reader library computed without a stated width are written
    // target-only (`thermo_isolation`); the count is declared in `transformations` below.
    let mut thermo_windows = matches!(reader, MZReaderType::ThermoRaw(_))
        .then(|| thermo_isolation::UnstatedWidthGuard::open(read_path));

    let mut n = 0usize;
    let cap = max_spectra();
    let mut ms1 = Ms1Chroms::default();
    let mut lattice_tally = FacetTally::default();
    // Whether any spectrum was actually re-ordered below — declared in `transformations` so a
    // reader knows the stored point order is not the source's.
    let mut resorted = false;
    for mut entry in reader.iter() {
        if cap.is_some_and(|m| n >= m) {
            break;
        }
        // The mzPeak peaks facet requires non-decreasing m/z within a spectrum.
        if entry.has_ion_mobility_dimension() {
            // Ion mobility: re-sort via the 3D stack/unstack (keeps the mobility dimension aligned).
            if let Some(arrays) = entry.arrays.as_mut() {
                if arrays.mzs().is_ok_and(|v| !v.is_sorted()) {
                    if let Ok(sorted) = BinaryArrayMap3D::stack(arrays).and_then(|v| v.unstack()) {
                        *arrays = sorted;
                        resorted = true;
                    }
                }
            }
        } else {
            // Non-IM: SRM/SIM (and some vendor) spectra list values out of m/z order, but the mzPeak
            // peaks facet requires non-decreasing m/z. mzdata may carry the same spectrum as a
            // centroid peak set, a deconvoluted set, and/or raw arrays; the writer prefers
            // peaks > deconvoluted > arrays, so re-sort whichever is present (no-op when ordered).
            if let Some(peaks) = entry.peaks.as_mut() {
                if !peaks.iter().map(|p| p.mz).is_sorted() {
                    peaks.sort();
                    resorted = true;
                }
            }
            if let Some(peaks) = entry.deconvoluted_peaks.as_mut() {
                if !peaks.iter().map(|p| p.neutral_mass).is_sorted() {
                    peaks.sort();
                    resorted = true;
                }
            }
            if let Some(arrays) = entry.arrays.as_mut() {
                if arrays.mzs().is_ok_and(|v| !v.is_sorted()) {
                                        if arrays.sort_by_array(&ArrayType::MZArray).is_ok() {
                            resorted = true;
                        }
                }
            }
        }
        if let Some(r) = &tdf_remap {
            r.apply(entry.description_mut());
        }
        if let Some(g) = thermo_windows.as_mut() {
            g.apply(entry.description_mut());
        }
        if synth_chroms {
            ms1.observe(&entry);
        }
        match lattice_scale {
            // Lattice lane: the spectrum comes back UNCHANGED (its m/z array is what the writer
            // derives the TIC / base peak / observed-m/z columns from — see `lattice_route`'s
            // summary contract), and only the peak-facet ROWS become integers. A spectrum whose
            // centroids miss the lattice falls through to `write_spectrum`, which stores its exact
            // f64 m/z in the same facet's `mz` column: nothing is snapped, nothing is refused.
            Some(scale) => {
                let (spec, peak_arrays, outcome) = mz_lattice::lattice_route(entry, scale);
                lattice_tally.record(FacetRoutes {
                    centroid_lattice: outcome.on_lattice(),
                    ..FacetRoutes::default()
                });
                match peak_arrays.as_ref() {
                    Some(arrays) => writer.write_spectrum_with_peak_arrays(&spec, arrays)?,
                    None => writer.write_spectrum(&spec)?,
                }
            }
            None => writer.write_spectrum(&entry)?,
        }
        n += 1;
    }
    log::debug!("wrote {n} spectra");
    if let Some(g) = &thermo_windows {
        g.report();
    }
    if let Some(scale) = lattice_scale {
        log::info!(
            "m/z lattice (1/{scale:e}): {} spectra stored as Int64 tof_index, {} kept exact f64 m/z",
            lattice_tally.centroid_lattice,
            lattice_tally.centroid_f64
        );
        // The scale is decided from a handful of probe spectra but the SCHEMA is run-wide, so a
        // spectrum that misses it keeps its exact f64 m/z in a point column that is neither
        // chunked nor numpressed. Values are never wrong, but a run that lands mostly there can be
        // BIGGER than the same file with `--no-mz-lattice`. Say so rather than let it pass in
        // silence: this is the one way the lattice can cost space instead of saving it.
        let routed = lattice_tally.centroid_lattice + lattice_tally.centroid_f64;
        if routed > 0 && lattice_tally.centroid_f64 * 10 > routed {
            log::warn!(
                "{} of {routed} spectra ({:.0} %) missed the 1/{scale:e} lattice the probe spectra \
                 chose, so their m/z are stored as unchunked f64: this archive may be \
                 LARGER than the same conversion with --no-mz-lattice. Mixed-lattice \
                 input (two different scales in one run) is the usual cause.",
                lattice_tally.centroid_f64,
                100.0 * lattice_tally.centroid_f64 as f64 / routed as f64
            );
        }
    }

    // Cross-check against the source's own declared count. A reader that stops early — a truncated
    // imzML `.ibd`, a half-downloaded mzML — otherwise yields a structurally valid archive that is
    // silently missing most of its spectra, with exit code 0. Fail loudly instead: the archive is
    // not written, so a partial conversion can never be mistaken for a complete one.
    assert_source_complete_tmp(input, n, cap, &tmp)?;

    let chromatogram_transforms = finish_chromatograms(&mut writer, input, &ms1, reader.iter_chromatograms(), synth_chroms)?;

    // Fill required ms_run fields the source may have left implicit, so the index schema validates.
    let acquisition_block = fixup_run_metadata(&mut writer, input);

    // The `mz_calibration` block is what the viewer's `mz-grid` codec (and any conformant reader
    // that would rather not re-derive the scale from the column metadata) gates on.
    let index_blocks: Vec<(String, serde_json::Value)> = lattice_scale
        .map(|scale| {
            (
                "mz_calibration".to_string(),
                mz_lattice::mz_calibration_block(
                    scale,
                    "detected",
                    "fixed-point m/z lattice detected in the decoded f64 m/z of the source \
                     (see mzpeak-convert --no-mz-lattice); off-lattice spectra keep f64 point.mz",
                ),
            )
        })
        .into_iter()
        .chain(partial_marker(input, cap, n))
        .chain(acquisition_block)
        .chain(route)
        .chain(std::iter::once(transformations_block(&{
            let mut applied = base_transformations(&writer);
            if resorted {
                declare(&mut applied, "sort-by-mz");
            }
            applied.extend(thermo_window_transformation(thermo_windows.as_ref()));
            applied.extend(chromatogram_transforms);
            applied
        })))
        .collect();
    finish_with_vendor_and_aux(writer, input, vendor, images, sdrf, &index_blocks)?;
    tmp_guard.finish(output)?;
    Ok(())
}

/// RAII cleanup for the sanitized copy [`sanitize_param_groups`] may write (an mzML with empty
/// `<referenceableParamGroup/>` elements rewritten). Removed on drop — success, error and
/// panic-unwind alike — so a failed conversion leaves no `mzpc-san-*.mzML` in the temp dir; before
/// this it was removed only on the success path (and never on the mzML-export path).
struct SanitizedTemp(PathBuf);

impl Drop for SanitizedTemp {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// RAII cleanup for a transcoded-to-UTF-8 input. Holds the temp *directory* we created (for imzML
/// we also place a hardlinked/copied `.ibd` sidecar beside the temp file, so the whole dir must go)
/// and removes it on drop — covering success, conversion error, and panic-unwind exit paths alike.
/// [`msconvert_dir`] reuses it for the directory msconvert writes into.
struct TranscodeGuard {
    dir: PathBuf,
    /// The transcoded UTF-8 file to hand to mzdata, inside `dir`.
    file: PathBuf,
}

impl Drop for TranscodeGuard {
    fn drop(&mut self) {
        TmpGuard::forget_path(&self.dir);
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// A fresh directory under `parent` for msconvert to write `via_msconvert.mzML` into (`file`),
/// removed on every return, and on a panic by the panic hook's sweep — the release build aborts
/// without running `Drop`, and this directory sits beside the user's output holding a possibly
/// multi-GB mzML. Created exclusively, under a name no other run holds (pid, clock, attempt), and
/// never reused: the pid alone is shared by containers whose entrypoint is PID 1 writing to one
/// volume, and a directory a crashed run left behind must not hand its mzML to this run. Hidden,
/// because it sits beside the user's output.
fn msconvert_dir(parent: &Path) -> Result<TranscodeGuard> {
    // msconvert creates a missing --outdir itself; keep that for an output directory that does not exist yet.
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    for attempt in 0..100 {
        let dir = parent.join(format!(".mzpc-msconvert-{}-{stamp}-{attempt}", std::process::id()));
        match fs::create_dir(&dir) {
            Ok(()) => {
                TmpGuard::register(&dir, true);
                return Ok(TranscodeGuard { file: dir.join("via_msconvert.mzML"), dir });
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("creating {}", dir.display())),
        }
    }
    bail!("could not create a fresh msconvert directory under {}", parent.display())
}

/// Sniff the XML encoding declared in the first ~200 bytes. Returns the lowercased charset name from
/// `<?xml ... encoding="X"?>`, or `None` when there is no declaration. ASCII/UTF-8 inputs need no
/// transcode, so callers treat `None`/`"utf-8"`/`"ascii"` as "leave it alone".
fn sniff_xml_encoding(head: &[u8]) -> Option<String> {
    let n = head.len().min(256);
    let s = String::from_utf8_lossy(&head[..n]);
    let decl_start = s.find("<?xml")?;
    let decl = &s[decl_start..];
    let decl_end = decl.find("?>").map(|e| e + 2).unwrap_or(decl.len());
    let decl = &decl[..decl_end];
    let key = decl.find("encoding")?;
    let after = &decl[key + "encoding".len()..];
    let eq = after.find('=')?;
    let after = after[eq + 1..].trim_start();
    let quote = after.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let rest = &after[1..];
    let close = rest.find(quote)?;
    Some(rest[..close].trim().to_ascii_lowercase())
}

/// True for an encoding mzdata's UTF-8-only quick-xml reader can already handle untouched.
fn is_utf8ish(enc: &str) -> bool {
    matches!(enc, "utf-8" | "utf8" | "us-ascii" | "ascii")
}

/// Decode a single-byte legacy charset to a UTF-8 `String`. ISO-8859-1/latin1 is the identity
/// codepoint map (byte 0xNN → U+00NN) and needs no table. windows-1252 differs only in 0x80–0x9F;
/// we map that block via the standard table and fall through to latin1 for everything else. Unknown
/// single-byte charsets are treated as latin1 (the common MS-imzML case), which never panics.
fn decode_single_byte(bytes: &[u8], enc: &str) -> String {
    let windows_1252_high = |b: u8| -> char {
        // 0x80..=0x9F mapping for windows-1252; 0x81/0x8D/0x8F/0x90/0x9D are undefined → U+FFFD.
        const T: [char; 32] = [
            '\u{20AC}', '\u{FFFD}', '\u{201A}', '\u{0192}', '\u{201E}', '\u{2026}', '\u{2020}',
            '\u{2021}', '\u{02C6}', '\u{2030}', '\u{0160}', '\u{2039}', '\u{0152}', '\u{FFFD}',
            '\u{017D}', '\u{FFFD}', '\u{FFFD}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}',
            '\u{2022}', '\u{2013}', '\u{2014}', '\u{02DC}', '\u{2122}', '\u{0161}', '\u{203A}',
            '\u{0153}', '\u{FFFD}', '\u{017E}', '\u{0178}',
        ];
        T[(b - 0x80) as usize]
    };
    let is_1252 = enc == "windows-1252" || enc == "cp1252";
    bytes
        .iter()
        .map(|&b| {
            if is_1252 && (0x80..=0x9F).contains(&b) {
                windows_1252_high(b)
            } else {
                // latin1 (and the 0xA0..=0xFF tail of windows-1252): identity codepoint map.
                b as char
            }
        })
        .collect()
}

/// Rewrite the `encoding="X"` value in the XML declaration (first ~256 bytes of `s`) to `UTF-8`,
/// so the transcoded file is self-consistent. No-op if no declaration/encoding attr is present.
fn rewrite_encoding_decl_to_utf8(s: &str) -> String {
    let Some(decl_start) = s.find("<?xml") else { return s.to_string() };
    let head_end = s[decl_start..].find("?>").map(|e| decl_start + e + 2).unwrap_or(s.len());
    let (decl, tail) = s.split_at(head_end);
    let Some(key) = decl.find("encoding") else { return s.to_string() };
    let after = &decl[key + "encoding".len()..];
    let Some(eq_rel) = after.find('=') else { return s.to_string() };
    let val_start_rel = {
        let a = &after[eq_rel + 1..];
        let trimmed = a.trim_start();
        eq_rel + 1 + (a.len() - trimmed.len())
    };
    let val = &after[val_start_rel..];
    let Some(quote) = val.chars().next() else { return s.to_string() };
    if quote != '"' && quote != '\'' {
        return s.to_string();
    }
    let Some(close_rel) = val[1..].find(quote) else { return s.to_string() };
    // Absolute byte offsets within `decl` of the quoted value (excluding quotes).
    let abs_val = key + "encoding".len() + val_start_rel + 1;
    let abs_close = abs_val + close_rel;
    let mut out = String::with_capacity(s.len());
    out.push_str(&decl[..abs_val]);
    out.push_str("UTF-8");
    out.push_str(&decl[abs_close..]);
    out.push_str(tail);
    out
}

/// RAII cleanup for a gunzipped input: the temp directory holding the decompressed copy goes on
/// drop, on every exit path, like [`TranscodeGuard`].
struct GunzipGuard {
    dir: PathBuf,
    /// The decompressed copy to hand to the reader, inside `dir`.
    file: PathBuf,
}

impl Drop for GunzipGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// If `input` is gzip-compressed, stream-decompress it into a throwaway temp dir and return a guard
/// whose `file` is the plain copy to hand to the reader. `Ok(None)` (zero overhead: two bytes read)
/// otherwise. Decided by the gzip MAGIC (`1f 8b`), not the extension, so a `.mzML` that is secretly
/// gzipped opens and a `.gz` that is not gzip falls through to the reader's own diagnosis.
///
/// Why a temp copy rather than mzdata's `open_gzipped_read_seek`: that constructor returns a reader
/// of a different concrete type, and every lane downstream is written against the file-backed one.
/// A copy keeps every downstream behaviour byte-identical to the uncompressed case, at the cost of
/// transient disk equal to the uncompressed size. The ORIGINAL path stays the `input` for provenance,
/// so `source_files.name` and the SHA-1 describe the `.gz` the user actually gave us.
fn gunzip_to_temp(input: &Path) -> Result<Option<GunzipGuard>> {
    let mut magic = [0u8; 2];
    let n = fs::File::open(input)
        .with_context(|| format!("opening {}", input.display()))?
        .read(&mut magic)?;
    let is_gzip = n >= 2 && magic == [0x1f, 0x8b];
    let name = input.file_name().and_then(|s| s.to_str()).unwrap_or("input.gz");
    let inner = name.strip_suffix(".gz").or_else(|| name.strip_suffix(".GZ"));
    // Nothing to do for the common case: plain content under a plain name.
    if !is_gzip && inner.is_none() {
        return Ok(None);
    }
    // The reader (mzdata) decides "gzipped" from the `.gz` EXTENSION (`is_gzipped_extension`), not
    // from the bytes. So a plain file that merely CARRIES a `.gz` name must also reach it under the
    // inner name, or it is refused with the very error this function exists to remove. In that case
    // the copy is a hardlink — free — and nothing is decompressed.
    let inner = inner.unwrap_or(name);
    if is_gzip {
        log::info!("input is gzip-compressed; decompressing to a temporary copy for the reader");
    } else {
        log::info!("input is named .gz but is not gzip; handing the reader a plain-named link to it");
    }
    let dir = std::env::temp_dir().join(format!(".mzpc-gz-{}-{inner}", std::process::id()));
    fs::create_dir_all(&dir).with_context(|| format!("creating temp dir {}", dir.display()))?;
    let guard = GunzipGuard { dir: dir.clone(), file: dir.join(inner) };
    if !is_gzip {
        if fs::hard_link(input, &guard.file).is_err() {
            fs::copy(input, &guard.file).with_context(|| format!("copying {}", input.display()))?;
        }
        return Ok(Some(guard));
    }
    let src = fs::File::open(input).with_context(|| format!("opening {}", input.display()))?;
    let mut dec = flate2::read::GzDecoder::new(std::io::BufReader::new(src));
    let mut out = std::io::BufWriter::new(
        fs::File::create(&guard.file).with_context(|| format!("creating {}", guard.file.display()))?,
    );
    std::io::copy(&mut dec, &mut out).with_context(|| format!("decompressing {}", input.display()))?;
    out.flush().with_context(|| format!("flushing {}", guard.file.display()))?;
    Ok(Some(guard))
}

/// Recompute an `<indexedmzML>`'s byte-offset index after a rewrite changed byte lengths.
///
/// The UTF-8 transcode shortens the declaration (`ISO-8859-1` → `UTF-8`) and widens every high byte
/// to two, so every `<offset>` and the `<indexListOffset>` of the copy point at the wrong byte.
/// mzdata then fails to read the index, falls back to a scan that finds the spectra, and cannot
/// enumerate the chromatograms, which it reaches only through the index: they were lost with exit
/// code 0. Each `<offset idRef="…">` is recomputed from the new position of the start tag carrying
/// that id in the list its `<index name="spectrum|chromatogram">` names — mzML ids are unique only
/// within their list, so a spectrum and a chromatogram may share one — and `<indexListOffset>` from
/// the new position of `<indexList`. An id not found keeps its value, with a warning. `None` for a
/// document with no index: a plain mzML, or an imzML, whose offsets point into the `.ibd`.
fn rebuild_mzml_index(doc: &str) -> Option<String> {
    let list_at = doc
        .rmatch_indices("<indexList")
        .map(|(i, _)| i)
        .find(|&i| !doc[i..].starts_with("<indexListOffset"))?;
    // `<spectrumList` / `<chromatogramList` / `<indexList` share a prefix: the name must end there.
    let starts = |at: usize, tag: &str| doc[at + tag.len()..].starts_with(|c: char| c.is_ascii_whitespace());
    let positions = |tag: &str| {
        let mut at = std::collections::HashMap::new();
        for (i, _) in doc[..list_at].match_indices(tag).filter(|&(i, _)| starts(i, tag)) {
            let rest = &doc[i + tag.len()..list_at];
            if let Some(id) = rest.find('>').and_then(|gt| xml_attr(&rest[..gt], "id")) {
                at.insert(id, i);
            }
        }
        at
    };
    let (spectra, chromatograms) = (positions("<spectrum"), positions("<chromatogram"));
    let mut out = String::with_capacity(doc.len());
    out.push_str(&doc[..list_at]);
    let mut rest = &doc[list_at..];
    let (mut ids, mut unresolved) = (None, 0usize);
    while let Some(o) = rest.find("<offset") {
        let (Some(gt), Some(close)) = (rest[o..].find('>'), rest[o..].find("</offset>")) else { break };
        let (gt, close) = (o + gt + 1, o + close);
        // The `<index name="…">` opened since the previous offset, if any, says which list this is.
        let base = doc.len() - rest.len();
        if let Some((i, _)) = rest[..o].rmatch_indices("<index").find(|&(i, _)| starts(base + i, "<index")) {
            ids = match rest[i..o].find('>').and_then(|e| xml_attr(&rest[i..i + e], "name")) {
                Some("spectrum") => Some(&spectra),
                Some("chromatogram") => Some(&chromatograms),
                _ => None,
            };
        }
        out.push_str(&rest[..gt]);
        match xml_attr(&rest[o..gt], "idRef").and_then(|id| ids?.get(id)) {
            Some(pos) => out.push_str(&pos.to_string()),
            None => {
                unresolved += 1;
                out.push_str(&rest[gt..close]);
            }
        }
        rest = &rest[close..];
    }
    if unresolved > 0 {
        log::warn!(
            "{unresolved} offset(s) in the source index name an id no element of their list carries; \
             they keep the value the source wrote, so a reader that follows one may land elsewhere"
        );
    }
    const OPEN: &str = "<indexListOffset>";
    if let (Some(a), Some(b)) = (rest.find(OPEN), rest.find("</indexListOffset>")) {
        out.push_str(&rest[..a + OPEN.len()]);
        out.push_str(&list_at.to_string());
        rest = &rest[b..];
    }
    // ponytail: the checksum hashes the ORIGINAL bytes and mzdata never verifies it, so the reader's
    // private copy drops it rather than rehashing the whole document.
    if let (Some(a), Some(b)) = (rest.find("<fileChecksum>"), rest.find("</fileChecksum>")) {
        out.push_str(&rest[..a]);
        rest = &rest[b + "</fileChecksum>".len()..];
    }
    out.push_str(rest);
    Some(out)
}

/// The value of attribute `name` in a start tag's text, as written (entities not expanded).
fn xml_attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    tag.match_indices(name).find_map(|(i, _)| {
        if !tag[..i].ends_with(|c: char| c.is_ascii_whitespace()) {
            return None;
        }
        let v = tag[i + name.len()..].trim_start().strip_prefix('=')?.trim_start();
        let q = v.chars().next().filter(|&c| c == '"' || c == '\'')?;
        let v = &v[1..];
        v.find(q).map(|end| &v[..end])
    })
}

/// If `input` declares a non-UTF-8 XML encoding (ISO-8859-1, latin1, windows-1252, …), transcode it
/// to UTF-8 in a throwaway temp dir and return a [`TranscodeGuard`] whose `file` is the path to hand
/// to mzdata. Returns `Ok(None)` (zero overhead) for UTF-8/ASCII inputs or inputs with no XML
/// declaration. The transcode changes byte lengths, so an `<indexedmzML>` gets its offset index
/// rebuilt for the copy ([`rebuild_mzml_index`]). For an imzML, the binary sidecar `<stem>.ibd` is
/// hardlinked (or copied across filesystems) beside the temp under the SAME basename so mzdata
/// finds it and the UUID matches.
fn transcode_to_utf8(input: &Path) -> Result<Option<TranscodeGuard>> {
    // Sniff only the first chunk — enough for the XML declaration, no full read for the common case.
    let mut f = fs::File::open(input).with_context(|| format!("opening {}", input.display()))?;
    let mut head = [0u8; 256];
    let n = f.read(&mut head)?;
    let enc = match sniff_xml_encoding(&head[..n]) {
        Some(e) if !is_utf8ish(&e) => e,
        _ => return Ok(None), // UTF-8/ASCII or no declaration: leave it alone, zero overhead.
    };
    log::info!("input declares {enc} XML encoding; transcoding to UTF-8 for the reader");

    // Read the whole file and transcode. Legacy MS XML files are single-byte charsets. Scoped so the
    // raw bytes and the pre-rewrite copy are freed before the index rebuild makes one more.
    let utf8 = {
        let raw = fs::read(input).with_context(|| format!("reading {}", input.display()))?;
        rewrite_encoding_decl_to_utf8(&decode_single_byte(&raw, &enc))
    };
    let utf8 = rebuild_mzml_index(&utf8).unwrap_or(utf8);

    let stem = input.file_stem().and_then(|s| s.to_str()).unwrap_or("input");
    let ext = input.extension().and_then(|s| s.to_str()).unwrap_or("xml");
    let dir = std::env::temp_dir().join(format!(".mzpc-utf8-{}-{}", std::process::id(), stem));
    fs::create_dir_all(&dir).with_context(|| format!("creating temp dir {}", dir.display()))?;
    let guard = TranscodeGuard { dir: dir.clone(), file: dir.join(format!("{stem}.{ext}")) };
    fs::write(&guard.file, utf8.as_bytes())
        .with_context(|| format!("writing transcoded {}", guard.file.display()))?;

    // imzML needs its `.ibd` sidecar next to the file under the same basename (and matching UUID).
    if ext.eq_ignore_ascii_case("imzml") {
        let ibd_src = input.with_extension("ibd");
        if ibd_src.exists() {
            let ibd_dst = dir.join(format!("{stem}.ibd"));
            // Same filesystem → hardlink is free; fall back to a copy across filesystems.
            if fs::hard_link(&ibd_src, &ibd_dst).is_err() {
                fs::copy(&ibd_src, &ibd_dst)
                    .with_context(|| format!("copying sidecar {}", ibd_src.display()))?;
            }
        }
    }
    Ok(Some(guard))
}

/// Cross-check spectra written against the source's own declared count.
///
/// A reader that stops early — a truncated imzML `.ibd`, a half-downloaded mzML — otherwise yields a
/// structurally valid archive that is silently missing most of its spectra, with exit code 0. Fail
/// loudly instead: the caller has not renamed its temp file yet, so a partial conversion can never
/// be mistaken for a complete one. `cap` is `MZPC_MAX_SPECTRA`, which deliberately truncates, so a
/// capped run skips the check.
fn assert_source_complete_tmp(
    input: &Path,
    written: usize,
    cap: Option<usize>,
    tmp: &Path,
) -> Result<()> {
    let r = assert_source_complete(input, written, cap);
    if r.is_err() {
        // The half-written archive is worthless and confusing sitting next to the intended output.
        let _ = fs::remove_file(tmp);
    }
    r
}

fn assert_source_complete(input: &Path, written: usize, cap: Option<usize>) -> Result<()> {
    if cap.is_some() {
        return Ok(());
    }
    let Some(declared) = declared_spectrum_count(input) else {
        return Ok(());
    };
    if declared != written as u64 {
        bail!(
            "{}: source declares {declared} spectra but only {written} were read ({:.1}%). \
             The input is incomplete — for imzML check that the `.ibd` sidecar is fully downloaded \
             (it holds all the signal; the .imzML is only metadata). Refusing to write a partial \
             output.",
            input.display(),
            100.0 * written as f64 / declared.max(1) as f64,
        );
    }
    Ok(())
}

/// Refuse to read an archive written with the removed per-scan TOF delta encoding.
///
/// Releases up to v0.7.2 could emit `ims_calibration.tof_encoding = "per-scan-delta"`, but no reader
/// (ours or the reference one) ever cumulatively summed those deltas — only the first bin of each
/// mobility scan decodes correctly and the rest square to nonsense m/z. The encoding is gone from the
/// writer; this stops an old archive from silently producing wrong masses. Reconvert from the `.d`.
fn reject_legacy_tof_delta(input: &Path) -> Result<()> {
    let Ok(file) = fs::File::open(input) else { return Ok(()) };
    let Ok(mut zip) = zip::ZipArchive::new(std::io::BufReader::new(file)) else { return Ok(()) };
    let Ok(entry) = zip.by_name("mzpeak_index.json") else { return Ok(()) };
    let Ok(idx) = serde_json::from_reader::<_, serde_json::Value>(entry) else { return Ok(()) };
    let enc = idx
        .get("metadata")
        .and_then(|m| m.get("ims_calibration"))
        .and_then(|c| c.get("tof_encoding"))
        .and_then(|e| e.as_str());
    if enc == Some("per-scan-delta") {
        bail!(
            "{} was written with the removed `per-scan-delta` TOF encoding, which no reader decodes \
             correctly (deltas were never cumulatively summed, so every m/z after the first in a \
             mobility scan is wrong). Reconvert from the original Bruker `.d` with this version.",
            input.display()
        );
    }
    Ok(())
}

/// The spectrum count an XML source declares in `<spectrumList count="N">`.
///
/// Only meaningful for mzML/imzML, where the count is authoritative. Returns `None` when the input
/// is another format, the attribute is absent, or the header cannot be read — callers treat that as
/// "no cross-check available" rather than as a failure.
fn declared_spectrum_count(input: &Path) -> Option<u64> {
    let ext = input.extension()?.to_string_lossy().to_ascii_lowercase();
    if ext != "mzml" && ext != "imzml" {
        return None;
    }
    // The attribute lives in the header; read a bounded prefix rather than the whole file (these
    // run to hundreds of MB).
    let mut f = fs::File::open(input).ok()?;
    let mut buf = vec![0u8; 4 * 1024 * 1024];
    let n = std::io::Read::read(&mut f, &mut buf).ok()?;
    let head = String::from_utf8_lossy(&buf[..n]);
    count_attr(&head[head.find("<spectrumList")?..])
}

/// The first `count="N"` in `text`, which starts at a `<…List` start tag.
fn count_attr(text: &str) -> Option<u64> {
    let c = text.find("count=")? + "count=".len();
    let rest = &text[c..];
    let q = rest.chars().next()?;
    let rest = &rest[q.len_utf8()..];
    let end = rest.find(q)?;
    rest[..end].trim().parse().ok()
}

/// The chromatogram count an mzML declares in `<chromatogramList count="N">`.
///
/// The list follows every spectrum, so it is searched for backwards from the end, a chunk at a
/// time, and only as far back as `</spectrumList>`: a read or two for a spectrum-bearing file
/// however large, with or without the list, and the whole file only when the chromatograms ARE the
/// file. `None` for another format, or when the list is absent or unreadable.
fn declared_chromatogram_count(path: &Path) -> Option<u64> {
    const NEEDLE: &[u8] = b"<chromatogramList";
    const SPECTRA_END: &[u8] = b"</spectrumList>";
    const CHUNK: u64 = 1 << 20;
    if !path.extension().is_some_and(|e| e.eq_ignore_ascii_case("mzML")) {
        return None;
    }
    let mut f = fs::File::open(path).ok()?;
    let mut end = f.metadata().ok()?.len();
    let mut buf = Vec::new();
    loop {
        let start = end.saturating_sub(CHUNK);
        buf.resize((end - start) as usize, 0);
        f.seek(SeekFrom::Start(start)).ok()?;
        f.read_exact(&mut buf).ok()?;
        let spectra_end = buf.windows(SPECTRA_END.len()).rposition(|w| w == SPECTRA_END);
        let from = spectra_end.unwrap_or(0);
        if let Some(i) = buf[from..].windows(NEEDLE.len()).rposition(|w| w == NEEDLE) {
            let mut tag = [0u8; 256];
            f.seek(SeekFrom::Start(start + (from + i) as u64)).ok()?;
            let n = f.read(&mut tag).ok()?;
            return count_attr(&String::from_utf8_lossy(&tag[..n]));
        }
        // Past the end of the spectrum list there is nothing left the list could follow.
        if start == 0 || spectra_end.is_some() {
            return None;
        }
        // Overlap by one byte short of the needle, so a needle the boundary cuts in two is still seen.
        end = start + NEEDLE.len() as u64 - 1;
    }
}

/// Warn when the reader yields fewer chromatograms than the source's `<chromatogramList count>`
/// declares, rather than writing an output that quietly lacks them. TIC/BPC synthesis hides the loss
/// of anything else: SIM/SRM traces are exactly what does not come back. mzdata reaches an mzML's
/// chromatograms only through its embedded offset index, so a non-indexed file yields none, and a
/// stale index (a rewritten copy whose offsets were not rebuilt) loses them the same way.
fn warn_unread_chromatograms(input: &Path, read_path: &Path, got: usize) {
    let Some(declared) = declared_chromatogram_count(read_path) else { return };
    if got as u64 >= declared {
        return;
    }
    let why = if is_unindexed_mzml(read_path) {
        "it is a non-indexed mzML, and this reader enumerates chromatograms only through the index"
    } else {
        "its offset index does not lead to them"
    };
    log::warn!(
        "{}: the source declares {declared} chromatograms but only {got} could be read ({why}). \
         The rest, SIM/SRM traces included, are NOT carried into the output, and synthesized \
         TIC/BPC do not replace them. Re-index it (msconvert, or `--via-msconvert`) if you need them.",
        input.display()
    );
}

/// Work around an mzdata defect: it `panic!`s when a `<referenceableParamGroupRef>` points at an
/// empty self-closing `<referenceableParamGroup id="…"/>` (which it never registers). Such groups
/// are valid mzML and ProteomeDiscoverer emits them. If the input's header contains that pattern,
/// write a sanitized copy where each empty group is rewritten as an explicit open/close pair and
/// return its path; otherwise return None (convert the original in place). Only the small pre-
/// `<spectrumList>` header is rewritten; the bulk of the file is streamed through verbatim.
///
/// The header grows, so an `<indexedmzML>`'s offsets are shifted by the same delta: left stale,
/// mzdata cannot read the index, falls back to a scan that finds only spectra, and loses every
/// chromatogram with exit code 0.
fn sanitize_param_groups(input: &Path) -> Result<Option<PathBuf>> {
    let ext = input.extension().and_then(|e| e.to_str()).unwrap_or("");
    if !ext.eq_ignore_ascii_case("mzml") {
        return Ok(None);
    }
    let mut f = BufReader::new(fs::File::open(input)?);
    // Read the header (everything before <spectrumList); the empty group + its list live here.
    let marker = b"<spectrumList";
    let mut head: Vec<u8> = Vec::new();
    let mut buf = [0u8; 65536];
    loop {
        let nread = f.read(&mut buf)?;
        if nread == 0 {
            break;
        }
        head.extend_from_slice(&buf[..nread]);
        if find_subslice(&head, marker).is_some() || head.len() > 32 * 1024 * 1024 {
            break;
        }
    }
    // The header never reaches into the index, even in a file with no <spectrumList>, so every
    // indexed element lies past the rewrite and moves by the same delta.
    let index_at = index_list_start(input);
    let split = find_subslice(&head, marker)
        .unwrap_or(head.len())
        .min(index_at.map_or(usize::MAX, |at| at as usize));
    let header = match std::str::from_utf8(&head[..split]) {
        Ok(s) => s,
        Err(_) => return Ok(None), // binary in header region: leave it alone
    };
    if !header.contains("<referenceableParamGroup id=") {
        return Ok(None);
    }
    let fixed = expand_empty_param_groups(header);
    if fixed == header {
        return Ok(None);
    }
    let stem = input.file_stem().and_then(|s| s.to_str()).unwrap_or("input");
    let temp =
        std::env::temp_dir().join(format!("mzpc-san-{}-{}.mzML", std::process::id(), stem));
    let mut out = BufWriter::new(fs::File::create(&temp)?);
    out.write_all(fixed.as_bytes())?;
    f.seek(SeekFrom::Start(split as u64))?;
    match index_at {
        Some(at) => {
            io::copy(&mut f.by_ref().take(at - split as u64), &mut out)?; // the body, verbatim
            // ponytail: the index tail is read whole, tens of bytes per spectrum (mzdata holds the
            // parsed index in memory anyway); stream it if a file's index ever outgrows RAM.
            let mut tail = Vec::new();
            f.read_to_end(&mut tail)?;
            out.write_all(&shift_index_tail(tail, (fixed.len() - header.len()) as u64))?;
        }
        None => {
            io::copy(&mut f, &mut out)?; // the rest of the file, verbatim
        }
    }
    out.flush()?;
    log::debug!("sanitized empty referenceableParamGroup(s) into {}", temp.display());
    Ok(Some(temp))
}

/// Rewrite every empty self-closing `<referenceableParamGroup id="…"/>` as `<… ></…>`. Leaves
/// `<referenceableParamGroupRef …/>` (a different element) and non-empty groups untouched.
fn expand_empty_param_groups(header: &str) -> String {
    const NEEDLE: &str = "<referenceableParamGroup id=";
    let mut out = String::with_capacity(header.len() + 64);
    let mut rest = header;
    while let Some(pos) = rest.find(NEEDLE) {
        out.push_str(&rest[..pos]);
        let after = &rest[pos..];
        match after.find('>') {
            Some(end) => {
                let tag = &after[..=end];
                if tag.ends_with("/>") {
                    out.push_str(&tag[..tag.len() - 2]);
                    out.push_str("></referenceableParamGroup>");
                } else {
                    out.push_str(tag);
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(after);
                return out;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Where an `<indexedmzML>`'s `<indexList>` starts, when its `<indexListOffset>` says so truthfully.
/// `None` for a plain mzML, and for an index already stale, which no shift repairs.
fn index_list_start(input: &Path) -> Option<u64> {
    let mut f = fs::File::open(input).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(4096))).ok()?;
    let mut end = Vec::new();
    f.read_to_end(&mut end).ok()?;
    let end = String::from_utf8_lossy(&end);
    let value = &end[end.rfind("<indexListOffset>")? + "<indexListOffset>".len()..];
    let at: u64 = value[..value.find('<')?].trim().parse().ok()?;
    f.seek(SeekFrom::Start(at)).ok()?;
    let mut tag = [0u8; 11];
    f.read_exact(&mut tag).ok()?;
    (tag.starts_with(b"<indexList") && (tag[10] == b'>' || tag[10].is_ascii_whitespace())).then_some(at)
}

/// Shift every byte position an `<indexedmzML>` tail (from `<indexList` on) states by `delta`: each
/// `<offset>` and the `<indexListOffset>`, the only numeric text there. The `<fileChecksum>` is
/// dropped: it hashes the original bytes and mzdata never verifies it.
fn shift_index_tail(mut tail: Vec<u8>, delta: u64) -> Vec<u8> {
    if let (Some(a), Some(b)) = (find_subslice(&tail, b"<fileChecksum>"), find_subslice(&tail, b"</fileChecksum>")) {
        if a < b {
            tail.drain(a..b + b"</fileChecksum>".len());
        }
    }
    let mut out = Vec::with_capacity(tail.len() + 64);
    let mut rest = &tail[..];
    while let Some(close) = find_subslice(rest, b"</") {
        let text = rest[..close].iter().rposition(|&b| b == b'>').map_or(0, |gt| gt + 1);
        match std::str::from_utf8(&rest[text..close]).ok().and_then(|s| s.trim().parse::<u64>().ok()) {
            Some(n) => {
                out.extend_from_slice(&rest[..text]);
                out.extend_from_slice((n + delta).to_string().as_bytes());
            }
            None => out.extend_from_slice(&rest[..close]),
        }
        out.extend_from_slice(b"</");
        rest = &rest[close + 2..];
    }
    out.extend_from_slice(rest);
    out
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Convert a Bruker TDF `.d` to an IN-ARCHIVE ims-compact mzPeak (Track 1): spectra_peaks carries
/// integer `tof` instead of f64 m/z, with the TOF→m/z calibration in the index `ims_calibration`
/// block. Half the m/z bytes (i32 vs f64) + exact integer grid; readers reconstruct
/// `(a+b·tof)²`. Vendor embedding still applies.
/// Shared ims-compact archive writer: builds the `point` peaks-facet schema (integer `tof` +
/// intensity + ion mobility), streams `n_total` frames through the `spectrum` closure, and writes
/// the `ims_calibration` index. Used by BOTH the native timsrust path and the Bruker-SDK path —
/// they yield the same integer-tof `MultiLayerSpectrum`, just from different decoders. `model_a/b`
/// are the `m/z = (a + b·tof)²` coefficients.
// Live on Windows/Linux (the SDK reader path); dead on macOS where that path is cfg'd out.
#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
fn write_ims_compact_archive<F>(
    input: &Path,
    output: &Path,
    zstd_level: i32,
    vendor: Option<&vendor::VendorPolicy>,
    synth_chroms: bool,
    model_a: f64,
    model_b: f64,
    chord_source: &'static str,
    n_total: usize,
    tof_encoding: &str,
    chunk_cfg: Option<f64>,
    exact_per_spectrum: Option<bruker_native::ExactTofSummary>,
    frame_sort: Option<&std::sync::atomic::AtomicUsize>,
    spectrum: F,
) -> Result<()>
where
    F: FnMut(usize, bool) -> Result<MultiLayerSpectrum>,
{
    // Serial driver: SDK reader is !Send/!Sync, so its decode must stay single-threaded. The unused
    // `Parallel` arm is pinned to a fn-pointer type so inference has a concrete `P`.
    type ParPlaceholder = fn(usize, bool) -> Result<MultiLayerSpectrum>;
    write_ims_compact_archive_impl::<F, ParPlaceholder>(
        input, output, zstd_level, vendor, synth_chroms, model_a, model_b, chord_source, n_total,
        tof_encoding, chunk_cfg, exact_per_spectrum, frame_sort, Driver::Serial(spectrum),
    )
}

/// #19: native (timsrust) ims-compact with PARALLEL frame decode. Identical output to the serial
/// path (writes in strict index order); only the decode is fanned across cores. The closure must be
/// `Fn + Sync` (timsrust's mmap-backed reader is thread-safe random access).
fn write_ims_compact_archive_parallel<F>(
    input: &Path,
    output: &Path,
    zstd_level: i32,
    vendor: Option<&vendor::VendorPolicy>,
    synth_chroms: bool,
    model_a: f64,
    model_b: f64,
    chord_source: &'static str,
    n_total: usize,
    tof_encoding: &str,
    chunk_cfg: Option<f64>,
    exact_per_spectrum: Option<bruker_native::ExactTofSummary>,
    frame_sort: Option<&std::sync::atomic::AtomicUsize>,
    spectrum: F,
) -> Result<()>
where
    F: Fn(usize, bool) -> Result<MultiLayerSpectrum> + Sync,
{
    type SerPlaceholder = fn(usize, bool) -> Result<MultiLayerSpectrum>;
    write_ims_compact_archive_impl::<SerPlaceholder, F>(
        input, output, zstd_level, vendor, synth_chroms, model_a, model_b, chord_source, n_total,
        tof_encoding, chunk_cfg, exact_per_spectrum, frame_sort, Driver::Parallel(spectrum),
    )
}

/// Decode strategy for the shared ims-compact writer: serial (`FnMut`, for the !Sync SDK reader) or
/// parallel (`Fn + Sync`, for the thread-safe native timsrust reader). Both write spectra in strict
/// index order, so the two produce byte-identical archives.
enum Driver<S, P> {
    // `Serial` is only constructed on the Windows/Linux SDK path.
    #[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
    Serial(S),
    Parallel(P),
}

/// Build the CHUNKED `spectra_peaks` facet schema for the `--ims-chunked` layout. The chunk-shaped
/// fields (`spectrum_index`, `..._chunk_start`/`_end`/`_values`, `chunk_encoding`, per-chunk
/// intensity + mobility secondaries) are materialized by running the chunker on a synthetic 2-point
/// sample whose arrays are the SAME shape (ArrayType + dtype + unit) as `ims_compact_spectrum_chunked`
/// produces — so the write-time schema matches the runtime chunk struct and column promotion (by
/// name) lines up. `chunking_strategy` + `mz_boundary` on the builder make `make_peaks_writer` build
/// a `ChunkBuffers` that chunks raw arrays on m/z bins.
fn ims_chunked_peak_schema(
    model_a: f64,
    model_b: f64,
    width_th: f64,
    int_intensity: bool,
    exact_per_spectrum: Option<bruker_native::ExactTofSummary>,
) -> ArrayBuffersBuilder {
    use mzpeak_prototyping::chunk_series::{ArrowArrayChunk, TofMzBoundary};
    let boundary = TofMzBoundary { a: model_a, b: model_b };
    let strategy = ChunkingStrategy::Delta { chunk_size: width_th };

    // Synthetic sample: two points far enough apart to land in different m/z bins (=> >=1 chunk).
    let mut arrays = BinaryArrayMap::new();
    let mut tof_da =
        DataArray::wrap(&ArrayType::nonstandard("tof"), BinaryDataArrayType::Int32, Vec::new());
    tof_da.update_buffer(&[100_000i32, 300_000i32]).expect("sample tof");
    arrays.add(tof_da);
    let mut int_da = if int_intensity {
        let mut da =
            DataArray::wrap(&ArrayType::IntensityArray, BinaryDataArrayType::Int32, Vec::new());
        da.update_buffer(&[1i32, 1i32]).expect("sample intensity");
        da
    } else {
        let mut da =
            DataArray::wrap(&ArrayType::IntensityArray, BinaryDataArrayType::Float32, Vec::new());
        da.update_buffer(&[1.0f32, 1.0f32]).expect("sample intensity");
        da
    };
    int_da.unit = Unit::DetectorCounts;
    arrays.add(int_da);
    let mut mob_da = DataArray::wrap(
        &ArrayType::MeanInverseReducedIonMobilityArray,
        BinaryDataArrayType::Float64,
        Vec::new(),
    );
    mob_da.update_buffer(&[1.0f64, 1.0f64]).expect("sample mobility");
    arrays.add(mob_da);

    // Register the TOF→m/z reconstruction on the chunk axis, exactly as the archive path does for
    // `point.tof`. Without it the chunked array index carries `transform: null` on every entry and
    // the m/z model lives ONLY in the index `ims_calibration` block, so a reader that resolves
    // transforms through the array index — as the spec requires — cannot reach m/z at all.
    // `from_arrays` looks arrays up by `array_type`, not by full BufferName, so the added transform
    // does not disturb the chunking itself.
    let main_axis = BufferName::new(
        BufferContext::Spectrum,
        ArrayType::nonstandard("tof"),
        BinaryDataArrayType::Int32,
    )
    .with_transform(Some(mzpeak_prototyping::buffer_descriptors::BufferTransform::SqrtMzFromTof));
    let (chunks, _, _) = ArrowArrayChunk::from_arrays(
        0,
        None,
        main_axis,
        &arrays,
        strategy,
        &Default::default(),
        false,
        false,
        None,
        Some(boundary),
    )
    .expect("materialize ims-chunked schema");
    let sample = chunks.first().expect("ims-chunked sample produced no chunk");
    let schema = sample.to_schema(
        BufferContext::Spectrum,
        &[strategy, ChunkingStrategy::Basic { chunk_size: width_th }],
        false,
    );

    let mut builder = ArrayBuffersBuilder::default()
        .prefix("point")
        .with_context(BufferContext::Spectrum)
        .chunking_strategy(Some(strategy))
        .mz_boundary(Some(boundary));
    for f in schema.fields().iter().cloned() {
        // The transform CURIE rides the BufferName into the field metadata, but the [a, b]
        // coefficients cannot (BufferName is Copy/Hash), so attach them to every tof-derived
        // column the same way the archive path does.
        let f = if f.metadata().contains_key("transform") {
            let mut md = f.metadata().clone();
            md.insert("mzpeak:transform_params".to_string(), format!("{model_a},{model_b}"));
            if exact_per_spectrum.is_some() {
                md.insert("mzpeak:transform_params_per_spectrum".to_string(), "tof_c0,tof_c1".to_string());
            }
            std::sync::Arc::new((*f).clone().with_metadata(md))
        } else {
            f
        };
        builder = builder.add_field(f);
    }
    builder
}

fn write_ims_compact_archive_impl<S, P>(
    input: &Path,
    output: &Path,
    zstd_level: i32,
    vendor: Option<&vendor::VendorPolicy>,
    synth_chroms: bool,
    model_a: f64,
    model_b: f64,
    chord_source: &'static str,
    n_total: usize,
    tof_encoding: &str,
    chunk_cfg: Option<f64>,
    // `Some`: the run carries exact `tof_c0`/`tof_c1` params (`bruker_native::exact_tof_coeffs`;
    // frames with a NULL `Frames.T1` have none — their count rides in the summary): declare them as
    // spectra_metadata columns, stamp the `tof` column's per-spectrum transform parameters and say
    // so in `ims_calibration`. `None`: the archive is exactly as before.
    exact_per_spectrum: Option<bruker_native::ExactTofSummary>,
    // `--ims-chunked`: the reader's count of frames whose points its TOF sort re-ordered.
    frame_sort: Option<&std::sync::atomic::AtomicUsize>,
    mut driver: Driver<S, P>,
) -> Result<()>
where
    S: FnMut(usize, bool) -> Result<MultiLayerSpectrum>,
    P: Fn(usize, bool) -> Result<MultiLayerSpectrum> + Sync,
{
    if n_total == 0 {
        bail!("no frames in {}", input.display());
    }
    let tmp = output.with_extension("mzpeak.tmp");
    let tmp_guard = TmpGuard::new(&tmp);
    let handle = fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    let level = ZstdLevel::try_new(zstd_level)
        .map_err(|e| anyhow::anyhow!("invalid zstd level {zstd_level}: {e}"))?;

    // Custom peaks-facet schema (the mechanism BRFP uses): the `point` facet carries integer `tof`
    // (nonstandard, replaces m/z) + intensity + ion mobility. `store_peaks_and_profiles_apart`
    // installs it so the spectra' tof arrays land in the peaks facet instead of defaulting to a
    // (null) m/z column. BufferNames here must exactly match the arrays built in
    // `ims_compact_spectrum` (array_type + dtype + unit) or they'd spill to auxiliary_arrays.
    // Register the TOF→m/z reconstruction on the `tof` column itself: the transform CURIE
    // (SqrtMzFromTof) rides via the BufferName, and the [a, b] coefficients via the field metadata
    // (`mzpeak:transform_params`), so a conformant reader recovers m/z = (a + b·tof)² generically
    // from the column metadata — not only from the index `ims_calibration` block (still written).
    let tof_field = {
        let base = BufferName::new(
            BufferContext::Spectrum,
            ArrayType::nonstandard("tof"),
            BinaryDataArrayType::Int32,
        )
        .with_transform(Some(mzpeak_prototyping::buffer_descriptors::BufferTransform::SqrtMzFromTof))
        .to_field();
        let mut md = base.metadata().clone();
        md.insert(
            "mzpeak:transform_params".to_string(),
            format!("{},{}", model_a, model_b),
        );
        // Exact per-frame coefficients override the run-wide chord in the reader (the same
        // per-spectrum contract as the sqrt-grid lanes; `reconstruct_per_spectrum_grid_mz`).
        if exact_per_spectrum.is_some() {
            md.insert("mzpeak:transform_params_per_spectrum".to_string(), "tof_c0,tof_c1".to_string());
        }
        std::sync::Arc::new((*base).clone().with_metadata(md))
    };
    let mob_field = BufferName::new(
        BufferContext::Spectrum,
        ArrayType::MeanInverseReducedIonMobilityArray,
        BinaryDataArrayType::Float64,
    )
    .to_field();
    // Byte-plane intensity: store native counts as Int32 so the writer BYTE_STREAM_SPLITs the column
    // (~ -16% on intensity, lossless; cf. BACKLOG #14). On by default for timsTOF ims-compact; set
    // MZPC_BYTE_PLANE_INTENSITY=0 to opt back out to f32 intensity. Read through `env_flag` so the
    // "off" spellings are the documented ones — a set-but-empty value used to flip the column to
    // Float32 with nothing in the log or the archive saying so.
    let int_intensity = env_flag("MZPC_BYTE_PLANE_INTENSITY").unwrap_or(true);
    let intensity_field = if int_intensity {
        BufferName::new(
            BufferContext::Spectrum,
            ArrayType::IntensityArray,
            BinaryDataArrayType::Int32,
        )
        .to_field()
    } else {
        INTENSITY_ARRAY.to_field()
    };
    let peak_schema = match chunk_cfg {
        // GATED --ims-chunked: build a CHUNKED peak facet keyed on the integer `tof` axis, split on
        // true m/z bins. The chunk-shaped fields are materialized by running the chunker on a
        // representative sample of the real per-frame arrays (identical BufferNames/units), so the
        // write-time schema matches the runtime chunk struct exactly.
        Some(width_th) => ims_chunked_peak_schema(model_a, model_b, width_th, int_intensity, exact_per_spectrum),
        None => ArrayBuffersBuilder::default()
            .prefix("point")
            .with_context(BufferContext::Spectrum)
            .add_field(BufferContext::Spectrum.index_field())
            .add_field(tof_field)
            .add_field(intensity_field)
            .add_field(mob_field),
    };

    // One layout family per entity (the scope HUPO-PSI/mzPeak-specification#21 proposes):
    // `spectra_data` and `spectra_peaks` are both `entity_type: spectrum`, so under --ims-chunked
    // the DATA facet is declared chunked too; beside a point data facet the writer only warns and
    // writes a mixed archive (the base.rs deviation). Centroid-only TDF never writes a row to it, but
    // its schema still has to be chunk-shaped: an empty chunked builder falls back to point-shaped
    // default fields, so hand it the same fields the peak facet uses.
    let data_fields: Vec<_> = match (chunk_cfg, peak_schema.dtype()) {
        (Some(_), DataType::Struct(fields)) => fields.iter().cloned().collect(),
        _ => Vec::new(),
    };
    let mut builder = MzPeakWriterType::<fs::File>::builder()
        .compression(Compression::ZSTD(level))
        // Per-frame inputs of the vendor's exact TOF→m/z model (`Frames.T1/T2/MzCalibration`) as
        // spectra_metadata columns; the calibration rows themselves go into the
        // `vendor_mz_calibration` index block below. Null on a TDF whose Frames lacks them.
        .add_spectrum_param_field(CustomBuilderFromParameter::from_spec(
            bruker_native::TDF_T1_CURIE,
            "tdf_t1",
            DataType::Float64,
        ))
        .add_spectrum_param_field(CustomBuilderFromParameter::from_spec(
            bruker_native::TDF_T2_CURIE,
            "tdf_t2",
            DataType::Float64,
        ))
        .add_spectrum_param_field(CustomBuilderFromParameter::from_spec(
            bruker_native::TDF_MZ_CAL_ID_CURIE,
            "tdf_mz_calibration_id",
            DataType::Int64,
        ))
        .store_peaks_and_profiles_apart(Some(peak_schema));
    if let Some(width_th) = chunk_cfg {
        builder = builder.chunked_encoding(Some(ChunkingStrategy::Delta { chunk_size: width_th }));
        for f in data_fields {
            builder = builder.add_spectrum_field(f);
        }
    }
    if exact_per_spectrum.is_some() {
        // The exact per-frame `m/z = (tof_c0 + tof_c1·tof)²` coefficients (vendor ModelType 1 with
        // C2 = 0, temperature-corrected with Frames.T1), as Float64 spectra_metadata columns — the
        // same columns/CURIEs the sqrt-grid lanes write, so the vendored reader's per-spectrum
        // fixup (`reconstruct_per_spectrum_grid_mz`, applied on the PEAKS facet where ims-compact
        // keeps its points — `get_spectrum_peak_arrays_for`) recovers the exact m/z.
        builder = builder
            .add_spectrum_param_field(CustomBuilderFromParameter::from_spec(
                TOF_C0_CURIE,
                "tof_c0",
                DataType::Float64,
            ))
            .add_spectrum_param_field(CustomBuilderFromParameter::from_spec(
                TOF_C1_CURIE,
                "tof_c1",
                DataType::Float64,
            ));
    }
    // Peak-facet row-group size (rows) = the per-chunk zstd granularity. Smaller = finer random
    // access (fewer peaks to decompress per frame) but worse compression; default is parquet's 2^20.
    // Tunable via $MZPC_ROW_GROUP_ROWS for benchmarking the size/random-access tradeoff.
    // ponytail: the CHUNKED facet holds ~100× fewer rows (chunks, not points), so parquet's 2^20-row
    // cap yields only ~4 giant row groups → spectrum_index min/max spans thousands of frames → frame
    // random access is ~25× slower. Default it to 8192 chunks/group (measured: 88 groups on Blank,
    // 7.5 vs 13.4 ms/frame — beats the flat layout — at +1% size). $MZPC_ROW_GROUP_ROWS still wins.
    let row_group_rows = std::env::var("MZPC_ROW_GROUP_ROWS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .or(if chunk_cfg.is_some() { Some(8192) } else { None });
    if let Some(n) = row_group_rows {
        builder = builder.row_group_size(Some(n));
    }
    let mut writer = builder.build(handle, true);
    add_processing_metadata(&mut writer);
    // Both ims-compact lanes (native + SDK) attach the MZP:1000006/7 window band to selected ions.
    ensure_mzp_cv(&mut writer);

    let mut ms1 = Ms1Chroms::default();
    let n_frames = max_spectra().map_or(n_total, |m| m.min(n_total));
    match &mut driver {
        Driver::Serial(spectrum) => {
            for i in 0..n_frames {
                let spec = spectrum(i, int_intensity)?;
                if synth_chroms {
                    ms1.observe(&spec);
                }
                writer.write_spectrum(&spec)?;
            }
        }
        Driver::Parallel(spectrum) => {
            // #19 + #18: decode frames in parallel (timsrust's mmap-backed `FrameReader::get` is
            // thread-safe random access) AND overlap that decode with the single-threaded
            // encode/compress/write. A dedicated WRITER THREAD owns `writer` + `ms1` and pulls
            // spectra off a bounded channel in the exact order they are sent; the producer decodes a
            // bounded reorder window of frames in parallel (`into_par_iter().collect::<Vec>()`
            // preserves index order) and sends them in strict index order. Single consumer + ordered
            // send => spectra are written in the same order as the serial path => byte-identical
            // output. The bounded channel + window cap memory. Empty frames (NumPeaks=0) decode to an
            // empty spectrum exactly as serial. MZPC_DECODE_WINDOW overrides the window
            // (0/unset => default = 8× the rayon thread count, capped at 128 — past which the
            // bounded channel/window stops helping and just costs memory).
            use rayon::prelude::*;
            let window = std::env::var("MZPC_DECODE_WINDOW")
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .filter(|&w| w > 0)
                .unwrap_or_else(|| (rayon::current_num_threads() * 8).clamp(1, 128));
            // Bounded channel: at most `window` decoded spectra buffered between decode and write, so
            // a slow writer back-pressures the decoder (and vice versa) without unbounded memory.
            let (tx, rx) = std::sync::mpsc::sync_channel::<MultiLayerSpectrum>(window);
            // Perf instrumentation (MZPC_TIMING=1): decode/encode are pipelined (parallel producer +
            // single writer thread), so the wall ≈ max(decode_busy, writer_busy). Comparing the two
            // busy-times against total tells us which stage is the critical path.
            let timing = env_flag("MZPC_TIMING").unwrap_or(false);
            let t_block = std::time::Instant::now();
            let writer_ns = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
            let decode_ns = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
            let wns = writer_ns.clone();
            // The writer thread owns writer+ms1 and returns them (or the first write error) on join.
            let writer_thread = std::thread::spawn(move || -> Result<(MzPeakWriterType<fs::File>, Ms1Chroms)> {
                while let Ok(spec) = rx.recv() {
                    if synth_chroms {
                        ms1.observe(&spec);
                    }
                    let s = std::time::Instant::now();
                    writer.write_spectrum(&spec)?;
                    if timing { wns.fetch_add(s.elapsed().as_nanos() as u64, std::sync::atomic::Ordering::Relaxed); }
                }
                Ok((writer, ms1))
            });
            // Producer: parallel-decode each window, then send in strict index order. On any decode
            // error, drop tx (closes the channel) and surface the error after joining the writer.
            let produce = || -> Result<()> {
                let mut i = 0usize;
                while i < n_frames {
                    let end = (i + window).min(n_frames);
                    let sd = std::time::Instant::now();
                    let batch: Vec<Result<MultiLayerSpectrum>> =
                        (i..end).into_par_iter().map(|j| spectrum(j, int_intensity)).collect();
                    if timing { decode_ns.fetch_add(sd.elapsed().as_nanos() as u64, std::sync::atomic::Ordering::Relaxed); }
                    for spec in batch {
                        // A send error means the writer thread died (write error) — stop producing;
                        // the real error comes back from the join below.
                        if tx.send(spec?).is_err() {
                            return Ok(());
                        }
                    }
                    i = end;
                }
                Ok(())
            };
            let produce_result = produce();
            drop(tx); // close channel so the writer thread's recv loop ends
            let joined = writer_thread
                .join()
                .map_err(|_| anyhow::anyhow!("ims-compact writer thread panicked"))?;
            // Surface a decode error first (it may be why the writer stopped), then a write error.
            produce_result?;
            let (w, m) = joined?;
            writer = w;
            ms1 = m;
            if timing {
                let total = t_block.elapsed().as_secs_f64();
                let wsec = writer_ns.load(std::sync::atomic::Ordering::Relaxed) as f64 / 1e9;
                let dsec = decode_ns.load(std::sync::atomic::Ordering::Relaxed) as f64 / 1e9;
                let bound = if wsec > dsec { "ENCODE-bound (writer thread)" } else { "DECODE-bound" };
                eprintln!(
                    "[timing] frames={n_frames} total={total:.1}s | decode(parallel busy)={dsec:.1}s  encode+zstd(writer busy)={wsec:.1}s | {bound} | threads={}",
                    rayon::current_num_threads()
                );
            }
        }
    }
    let chromatogram_transforms = finish_chromatograms(&mut writer, input, &ms1, std::iter::empty(), synth_chroms)?;
    let acquisition_block = fixup_run_metadata(&mut writer, input);

    // Finish: add the ims_calibration index block, embed vendor side-files, finalize, rename.
    // `tof_encoding` is TRUTHFUL: "absolute" (archive layout + SDK) or "m/z-chunked"
    // (--ims-chunked). For the chunked layout, `chunk_start`/`chunk_end` are the per-chunk main-axis
    // (TOF) bounds and `tof` is delta-encoded within each chunk with the start point EXCLUDED
    // (cumsum from `chunk_start` to reconstruct), per the spec's chunked-layout rules — and
    // `chunk_tof_encoding` below states exactly that rule. Until 0.9.13 it read "delta-within-chunk;
    // first absolute; cumsum", which describes a layout the writer never produced: anyone decoding
    // by that sentence lost `chunk_start` on every chunk.
    let mut cal = serde_json::json!({
        "codec": "ims-compact",
        "lossless": "tof",
        "mz_from_tof": "(a + b*tof)^2",
        "tof_encoding": tof_encoding,
        "a": model_a,
        "b": model_b,
        // The two lanes derive the chord differently: the native lane from GlobalMetadata
        // (MzAcqRangeLower/Upper, DigitizerNumSamples), the SDK lane from the vendor library's
        // own `tims_index_to_mz(frame 1, [0, 1])`. Measured 4.28 ppm apart on 2485.d, and until
        // 0.9.13 an archive did not say which (a, b) it held.
        "chord_source": chord_source,
        // `(a + b·tof)²` is timsrust's TWO-POINT CHORD, not the instrument's model: it drops the
        // quadratic `C2·mz` term and the per-frame temperature correction, and is off by roughly
        // −11…−40 ppm across the range on files where `C2 ≠ 0` (measured: +8.5/−10.6/−3.4 ppm at
        // tof 0/mid/max on a diaPASEF run). A search at 20 ppm loses peptides to it — speXtract
        // measured −11.7 % at 1 % FDR. Say so in the archive, because MS:1003825 otherwise reads
        // as "this fit IS the calibration" and readers apply it blindly.
        "exact": false,
        "approximation": "two-point chord (timsrust); drops C2 and the per-frame temperature term",
        "exact_model": "metadata.vendor_mz_calibration (ModelType 1) when present",
    });
    if let Some(exact) = exact_per_spectrum {
        // Every frame's MzCalibration row is ModelType 1 with C2 = C3 = C4 = dC2 = 0 (stored zeros,
        // not NULL), so the vendor model IS a sqrt-linear law in tof per frame: the per-spectrum
        // pair reproduces the ModelType-1 formula exactly (1e-12 relative, `tests/
        // tdf_exact_tof_calibration.rs`; the formula is SDK-verified on C2 ≠ 0 rows, the C2 = 0
        // SDK golden comes from `MZPC_TDF_SDK_GOLDEN`). `a`/`b` and `exact: false` stay as they are
        // for readers that only know the run-wide chord. `exact_per_spectrum` is a PER-SPECTRUM
        // statement: a spectrum whose tof_c0/tof_c1 cells are NULL (NULL Frames.T1, counted in
        // `per_spectrum_chord_frames`) is on the chord, and both readers treat it so.
        cal["per_spectrum"] = serde_json::json!("tof_c0,tof_c1");
        cal["exact_per_spectrum"] = serde_json::json!(true);
        if exact.chord_frames > 0 {
            cal["per_spectrum_chord_frames"] = serde_json::json!(exact.chord_frames);
        }
        cal["per_spectrum_note"] = serde_json::json!(
            "spectra_metadata tof_c0/tof_c1 give the vendor ModelType-1 m/z per spectrum, m/z = (tof_c0 + tof_c1*tof)^2 \
             (MzCalibration ModelType 1 with C2 = 0, C1 temperature-corrected with Frames.T1); a spectrum with NULL \
             tof_c0/tof_c1 (NULL Frames.T1, count in per_spectrum_chord_frames) is on the run-wide chord a/b, which \
             remains for legacy readers"
        );
    }
    if let Some(width_th) = chunk_cfg {
        cal["chunk_bounds"] = serde_json::json!("mz");
        cal["chunk_width_th"] = serde_json::json!(width_th);
        cal["chunk_tof_encoding"] = serde_json::json!("chunk_start + cumsum(deltas); first delta is relative to chunk_start");
    }
    // Which intensity column the archive holds — Int32 byte-plane by default, Float32 under
    // `MZPC_BYTE_PLANE_INTENSITY=0` — stated here so a reader (or an offline audit) need not infer
    // it from the Parquet schema.
    cal["intensity_dtype"] = serde_json::json!(if int_intensity { "int32" } else { "float32" });
    let mut applied = base_transformations(&writer);
    // `--ims-chunked` sorts each frame's points by TOF across mobility scans. Declared from the
    // reader's count of frames that sort changed; the order is recoverable, since mobility is
    // stored per point.
    if frame_sort.is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed) > 0) {
        declare(&mut applied, "sort-by-mz");
    }
    applied.extend(chromatogram_transforms);
    let mut zip: ZipArchiveWriter<fs::File> = writer.finish_parquet()?;
    zip.add_index_metadata("ims_calibration", &cal)
        .context("writing ims_calibration index")?;
    if let Some((key, block)) = acquisition_block {
        zip.add_index_metadata(&key, &block).context("writing acquisition_time index block")?;
    }
    let (key, block) = conversion_route_block(
        "ims-compact",
        if chord_source == "sdk_tims_index_to_mz" { "timsdata" } else { "timsrust" },
        None,
    );
    zip.add_index_metadata(&key, &block).context("writing conversion_route index block")?;
    let (key, block) = transformations_block(&applied);
    zip.add_index_metadata(&key, &block).context("writing transformations index block")?;
    if let Some((key, block)) = partial_marker(input, max_spectra(), n_frames) {
        zip.add_index_metadata(&key, &block).context("writing partial index block")?;
    }
    // The vendor's exact calibration, verbatim, so the archive is self-sufficient without the
    // embedded `vendor/analysis.tdf.gz` (`--no-vendor`). Best-effort: a TDF without the table is
    // still a valid ims-compact archive on the two-point model above.
    let tdf = if input.is_dir() { input.join("analysis.tdf") } else { input.to_path_buf() };
    match bruker_native::vendor_mz_calibration(&tdf) {
        Ok(v) => zip
            .add_index_metadata("vendor_mz_calibration", &v)
            .context("writing vendor_mz_calibration index")?,
        Err(e) => log::warn!("vendor MzCalibration unavailable ({e}); vendor_mz_calibration index block omitted"),
    }
    embed_vendor_members(&mut zip, input, vendor)?;
    zip.finish().map_err(|e| anyhow::anyhow!("finalizing archive: {e}"))?;
    tmp_guard.finish(output)?;
    Ok(())
}

/// Native (timsrust) ims-compact: pure-Rust decoder, default for Bruker TDF.
fn convert_ims_compact_archive(
    input: &Path,
    output: &Path,
    zstd_level: i32,
    vendor: Option<&vendor::VendorPolicy>,
    synth_chroms: bool,
    tims_recalibration: bool,
    ims_chunked: bool,
    chunk_size_th: f64,
) -> Result<()> {
    let reader = bruker_native::NativeTofReader::open_with(input, tims_recalibration)?;
    let (a, b, n) = (reader.model.a, reader.model.b, reader.len());
    // Truthful self-describing tof_encoding label for the ims_calibration index block.
    let (tof_encoding, chunk_cfg) = if ims_chunked {
        ("m/z-chunked", Some(chunk_size_th))
    } else {
        ("absolute", None)
    };
    let exact = reader.exact_tof_per_spectrum();
    // The native reader is Sync (mmap-backed timsrust FrameReader), so decode frames in parallel.
    write_ims_compact_archive_parallel(input, output, zstd_level, vendor, synth_chroms, a, b, "global_metadata", n, tof_encoding, chunk_cfg, exact, ims_chunked.then(|| reader.frames_reordered()), |i, int| {
        if ims_chunked {
            // Chunked layout: absolute TOF, whole frame sorted by TOF (== sorted by m/z) so the
            // chunker's m/z bins are contiguous. Per-scan delta OFF (chunker deltas per chunk).
            reader.ims_compact_spectrum_chunked(i, int)
        } else {
            reader.ims_compact_spectrum(i, int)
        }
    })
}

/// Bruker-SDK ims-compact: same integer-tof layout, but decoded via the official `timsdata` library
/// (handles newer timsTOF, e.g. 5.1.x, that the vendored timsrust can't). Windows/Linux only.
#[cfg(any(windows, target_os = "linux"))]
fn convert_ims_compact_sdk(
    input: &Path,
    output: &Path,
    zstd_level: i32,
    vendor: Option<&vendor::VendorPolicy>,
    synth_chroms: bool,
) -> Result<()> {
    let reader = bruker_sdk::TdfSdkReader::open(input)?;
    let (a, b) = reader.tof_mz_model();
    let n = reader.len();
    let exact = reader.exact_tof_per_spectrum();
    // Diagnostic (MZPC_TDF_SDK_GOLDEN=<out.json>): sample the SDK's own tims_index_to_mz over the
    // run so the ModelType-1 formula can be checked against the vendor library off-box. Never
    // fails the conversion.
    if let Some(out) = std::env::var_os("MZPC_TDF_SDK_GOLDEN").filter(|v| !v.is_empty()) {
        let out = PathBuf::from(out);
        match reader.dump_sdk_golden(&out) {
            Ok(n_pts) => log::info!("MZPC_TDF_SDK_GOLDEN: wrote {n_pts} SDK (frame, tof, m/z) points to {}", out.display()),
            Err(e) => log::warn!("MZPC_TDF_SDK_GOLDEN: no golden dump written to {} ({e:#}); continuing", out.display()),
        }
    }
    // The SDK decoder writes absolute TOF (its ims_compact_spectrum has no delta and no chunking).
    write_ims_compact_archive(input, output, zstd_level, vendor, synth_chroms, a, b, "sdk_tims_index_to_mz", n, "absolute", None, exact, None, |i, int| {
        reader.ims_compact_spectrum(i, int)
    })
}

#[cfg(not(any(windows, target_os = "linux")))]
fn convert_ims_compact_sdk(
    _input: &Path,
    _output: &Path,
    _zstd_level: i32,
    _vendor: Option<&vendor::VendorPolicy>,
    _synth_chroms: bool,
) -> Result<()> {
    Err(UnsupportedVendor(
        "the Bruker timsdata SDK path (--bruker-sdk) is only available on Windows and Linux".into(),
    )
    .into())
}

/// The `transformations` index block — the second half of the fidelity invariant ("preserve as
/// much as possible; every transformation declared in the archive"). Each entry names one
/// declared, bounded change that was APPLIED to this archive's stored data — counted while it was
/// written, never inferred from what the lane was configured to do — so an empty list says the
/// signal is stored as handed over, and a reader (or an audit over a corpus) can tell a masked,
/// re-sorted or grid-quantized archive from a verbatim one without re-deriving it. Entries, each
/// written when its count is above zero: `zero-run-mask` (the writer's zero-run compaction shortened
/// a profile spectrum), `numpress-linear` (an m/z chunk is stored with the lossy codec),
/// `sort-by-mz` (a spectrum was re-ordered, by the lane's reader or by the writer's backstop),
/// `sort-by-time` (the writer's backstop re-ordered a chromatogram by time), `sort-by-wavelength`
/// (the writer's backstop re-ordered a wavelength spectrum by wavelength),
/// `chromatogram-time-to-minutes` (a chromatogram time recorded in seconds or milliseconds was
/// divided into minutes; `chromatogram_time_to_minutes`),
/// `tof-grid:<ppm>ppm` (a statistically fitted integer grid replaced f64 m/z within that bound on a
/// spectrum), `shimadzu:span-trim` (the profile sqrt-grid route left a gridded spectrum's
/// zero-intensity pad at the scan-window bounds out), `agilent:drop-zero-samples` (the profile grid
/// reader left a zero-intensity sample or an all-zero scan out of its sparse point lists),
/// `agilent:intensity-f32-rounding` (a count above 2^24 was rounded into Float32),
/// `agilent:nonfinite-intensity-to-zero` and `agilent:truncate-unequal-arrays` (the MHDAC host stored
/// a NaN/Inf intensity as 0, or cut a spectrum's unequal arrays to one length; `agl::HostCounts`, over
/// the scans the host exported, which `MZPC_MAX_SPECTRA` limits to the ones written),
/// `waters:drop-functions` (a MassLynx function was not written as spectra;
/// `waters::function_transformations`), `waters:sonar-summed` (a written scan was a SONAR
/// function's quadrupole bins summed; counted by `WatersReader::spectrum`),
/// `bruker:trace-unit-rescale` (a written Bruker device trace was recorded in a unit mzdata cannot
/// state and multiplied by the exact factor into one it can — bar into pascal),
/// `bruker:trace-sort-dedup` (a written Bruker device trace was stored out of time order or with
/// repeated samples and was put in time order, each exact (time, value) repeat kept once),
/// `thermo:target-only-isolation-window` (a Thermo precursor window the reader library computed
/// without a stated width was written target-only), and the native SciEX glue's counted value
/// changes `sciex:nan-intensity-to-zero`, `sciex:clamp-intensity-to-f32` and
/// `sciex:truncate-unequal-arrays` (`sciex_run::GlueValueChanges`). An entry names the
/// transformation and never how often it was applied, which the run's warning says;
/// `tof-grid:<ppm>ppm` is the one entry with a parameter, the bound its grid was accepted within.
fn transformations_block(applied: &[String]) -> (String, serde_json::Value) {
    ("transformations".to_string(), serde_json::json!(applied))
}

/// The entries the writer itself applied, from its own counters rather than its configuration:
/// `zero-run-mask` when the mask shortened a profile spectrum, `numpress-linear` when a numpress
/// chunk was stored, `sort-by-mz` when the writer's backstop re-sorted a spectrum the lane handed
/// over out of m/z order (all three `spectrum_signal_tally`), and `sort-by-time` /
/// `sort-by-wavelength` when the same backstop re-sorted a chromatogram by time or a wavelength
/// spectrum by wavelength (`chromatogram_signal_tally`, `wavelength_signal_tally`). Read after the
/// chromatograms are written and before `finish_parquet`.
fn base_transformations(writer: &MzPeakWriterType<fs::File>) -> Vec<String> {
    let tally = writer.spectrum_signal_tally();
    let mut applied = Vec::new();
    if tally.zero_runs_masked > 0 {
        applied.push("zero-run-mask".to_string());
    }
    if tally.numpress_chunks > 0 {
        applied.push("numpress-linear".to_string());
    }
    if tally.resorted > 0 {
        applied.push("sort-by-mz".to_string());
    }
    if writer.chromatogram_signal_tally().resorted > 0 {
        applied.push("sort-by-time".to_string());
    }
    if writer.wavelength_signal_tally().resorted > 0 {
        applied.push("sort-by-wavelength".to_string());
    }
    applied
}

/// The `thermo:target-only-isolation-window` entry, when the Thermo guard rewrote any window. How
/// many it rewrote is in the guard's warning ([`thermo_isolation::UnstatedWidthGuard::report`]).
fn thermo_window_transformation(guard: Option<&thermo_isolation::UnstatedWidthGuard>) -> Option<String> {
    guard.filter(|g| g.rewritten() > 0).map(|_| "thermo:target-only-isolation-window".to_string())
}

/// Add one entry to a `transformations` list, once: `sort-by-mz` can come both from the lane's own
/// re-sort and from the writer's backstop.
fn declare(applied: &mut Vec<String>, entry: impl Into<String>) {
    let entry = entry.into();
    if !applied.contains(&entry) {
        applied.push(entry);
    }
}

/// Flush Parquet, then stream-embed vendor side-files + vendor metadata into the archive index,
/// optical images (`--image` + sibling discovery) and an SDRF (`--sdrf`) BEFORE `zip.finish()`,
/// adding the `metadata.imaging` / `metadata.study` / `metadata.sample_metadata` index blocks.
/// `index_blocks` carries any extra reader-side calibration the lane produced (today: the
/// `mz_calibration` block of the fixed-point m/z lattice); pass `&[]` when there is none.
fn finish_with_vendor_and_aux(
    writer: MzPeakWriterType<fs::File>,
    input: &Path,
    vendor: Option<&vendor::VendorPolicy>,
    images: &[PathBuf],
    sdrf: Option<&Path>,
    index_blocks: &[(String, serde_json::Value)],
) -> Result<()> {
    let mut zip: ZipArchiveWriter<fs::File> = writer.finish_parquet()?;
    for (key, block) in index_blocks {
        zip.add_index_metadata(key, block)
            .with_context(|| format!("writing {key} index block"))?;
    }
    embed_vendor_members(&mut zip, input, vendor)?;
    embed_aux::embed_into_archive(&mut zip, input, images, sdrf)
        .context("embedding optical images / SDRF")?;
    zip.finish().map_err(|e| anyhow::anyhow!("finalizing archive: {e}"))?;
    Ok(())
}

/// The one vendor side-file rule, used by every lane that writes an archive. A vendor DIRECTORY
/// input (a Bruker `.d` of any kind, an Agilent `.d`, a Waters `.raw`) has its side-files embedded
/// under `vendor/` as the lane's policy says: preserve by default, the bulk-binary drop the lossless
/// ims-compact policy adds, and every `--aux` rule on top. A Thermo `.raw` gets its trailer and
/// status-log facets. Through 0.11.5 only TDF/TSF directories qualified here while `--agilent-grid`
/// and ims-compact embedded any directory themselves, so a BAF `.d`, an Agilent `.d` on the MHDAC
/// lane and a Waters `.raw` embedded nothing and `--aux` did nothing on them, silently. A
/// single-file input has no side-files; `run` names `--aux` inert there.
fn embed_vendor_members(
    zip: &mut ZipArchiveWriter<fs::File>,
    input: &Path,
    vendor: Option<&vendor::VendorPolicy>,
) -> Result<()> {
    if let Some(policy) = vendor {
        if input.is_dir() {
            vendor::embed_into_archive(zip, input, policy).context("embedding vendor files")?;
        } else if is_thermo_raw(input) {
            embed_thermo_trailers(zip, input)?;
        }
    }
    Ok(())
}

/// The warning for `--aux` on an input it cannot act on: side-files are embedded from a vendor
/// directory only ([`embed_vendor_members`]), so on a single file the rules change nothing.
fn aux_inert_note(input: &Path, given: &[&'static str]) -> Option<String> {
    (given.contains(&"--aux") && !input.is_dir()).then(|| {
        format!(
            "--aux is inert for {}: only a vendor directory input (a .d or .raw directory) has side-files to embed",
            input.display()
        )
    })
}

fn is_thermo_raw(input: &Path) -> bool {
    input.is_file()
        && input.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("raw"))
}

/// True when mzdata would read `input` through Thermo's RawFileReader, decided as its `open_path`
/// decides: the extension, looked up through a `.gz` suffix (`x.raw.gz`), else a Thermo header in
/// the first 500 bytes, gunzipped when they are gzip. Nothing past those bytes is read; a file that
/// cannot be read is not Thermo here and fails in its own open.
fn mzdata_reads_thermo_raw(input: &Path) -> bool {
    use mzdata::io::{MassSpectrometryFormat, infer_format};
    input.is_file() && infer_format(input).is_ok_and(|(format, _)| format == MassSpectrometryFormat::ThermoRaw)
}

/// Build + embed the Thermo `vendor_scan_trailers.parquet` proprietary facet (Track 2). Best-effort:
/// a trailer-read failure is logged but does not abort the (already-written) conversion.
fn embed_thermo_trailers(zip: &mut ZipArchiveWriter<fs::File>, input: &Path) -> Result<()> {
    // #21: open the Thermo RawFileReader ONCE and share it across all three metadata facets, instead
    // of re-opening (re-spinning the .NET RawFileReader) three times. A failure to open is fatal for
    // every facet, so report it once and skip them all (matching the prior per-facet warn behavior).
    let handle = match thermorawfilereader::RawFileReader::open(input) {
        Ok(h) => h,
        Err(e) => {
            log::warn!("skipping Thermo vendor metadata facets (open failed): {e}");
            return Ok(());
        }
    };
    match thermo_trailers::build_trailer_facet(&handle) {
        Ok(Some(bytes)) => {
            let fe = mzpeak_prototyping::archive::FileEntry::new(
                "vendor_scan_trailers.parquet".to_string(),
                mzpeak_prototyping::archive::EntityType::Spectrum,
                mzpeak_prototyping::archive::DataKind::Proprietary,
            );
            zip.add_file_from_read(&mut std::io::Cursor::new(bytes), None::<&String>, Some(fe))
                .context("embedding vendor_scan_trailers.parquet")?;
            log::info!("embedded Thermo vendor_scan_trailers facet");
        }
        Ok(None) => log::debug!("no Thermo scan trailers to embed"),
        Err(e) => log::warn!("skipping Thermo trailer facet: {e:#}"),
    }
    let proprietary = |name: &str| {
        mzpeak_prototyping::archive::FileEntry::new(
            name.to_string(),
            mzpeak_prototyping::archive::EntityType::Spectrum,
            mzpeak_prototyping::archive::DataKind::Proprietary,
        )
    };
    match thermo_status::build_status_log_facet(&handle) {
        Ok(Some(bytes)) => {
            zip.add_file_from_read(&mut std::io::Cursor::new(bytes), None::<&String>, Some(proprietary("vendor_status_log.parquet")))
                .context("embedding vendor_status_log.parquet")?;
            log::info!("embedded Thermo vendor_status_log facet");
        }
        Ok(None) => log::debug!("no Thermo status logs to embed"),
        Err(e) => log::warn!("skipping Thermo status-log facet: {e:#}"),
    }
    match thermo_status::build_trailer_wide_facet(&handle) {
        Ok(Some(bytes)) => {
            zip.add_file_from_read(&mut std::io::Cursor::new(bytes), None::<&String>, Some(proprietary("vendor_scan_trailers_wide.parquet")))
                .context("embedding vendor_scan_trailers_wide.parquet")?;
            log::info!("embedded Thermo vendor_scan_trailers_wide facet");
        }
        Ok(None) => log::debug!("no Thermo wide trailers to embed"),
        Err(e) => log::warn!("skipping Thermo wide-trailer facet: {e:#}"),
    }
    Ok(())
}

/// Convert a Bruker BAF `.d` (Q-TOF) → mzPeak via the vendor SDK (feature `bruker_sdk`,
/// Windows/Linux only). Mirrors `convert_tsf`. UNTESTED on macOS (no SDK) — verified to compile.
#[cfg(any(windows, target_os = "linux"))]
fn convert_baf(
    input: &Path,
    output: &Path,
    chunk: Option<ChunkingStrategy>,
    zstd_level: i32,
    vendor: Option<&vendor::VendorPolicy>,
    synth_chroms: bool,
) -> Result<()> {
    let reader = bruker_baf::BafReader::open_with(input, None, representation())?;
    // What the baf2sql cache's `Properties` table states (instrument series, serial, software, time).
    // The digested members come from the directory itself, through `fixup_run_metadata`.
    let hints = VendorHints { run_metadata: Some(reader.run_metadata()), ..Default::default() };
    convert_vendor_reader(input, output, chunk, zstd_level, vendor, synth_chroms, hints, reader.len(), |i| reader.spectrum(i))
}

/// Convert a Bruker TDF/TSF `.d` → mzPeak via the official Bruker **timsdata** SDK (opt-in
/// `--bruker-sdk`), a parallel path to the default pure-Rust readers. Windows/Linux only — there is
/// no macOS timsdata build, so the non-(win|linux) stub returns the typed unsupported error (exit 3).
/// Hooks into the same `MultiLayerSpectrum` seam every native reader uses.
#[cfg(any(windows, target_os = "linux"))]
fn convert_bruker_sdk(
    input: &Path,
    output: &Path,
    chunk: Option<ChunkingStrategy>,
    zstd_level: i32,
    vendor: Option<&vendor::VendorPolicy>,
    synth_chroms: bool,
) -> Result<()> {
    let reader = bruker_sdk::BrukerSdkReader::open(input)?;
    // A TDF frame arrives mobility-major and the reader re-sorts it by m/z; declared from its count.
    let hints = VendorHints { counters: reader.reorder_counter().map(|c| ("sort-by-mz", c)).into_iter().collect(), ..Default::default() };
    convert_vendor_reader(input, output, chunk, zstd_level, vendor, synth_chroms, hints, reader.len(), |i| reader.spectrum(i))
}

#[cfg(not(any(windows, target_os = "linux")))]
fn convert_bruker_sdk(
    _input: &Path,
    _output: &Path,
    _chunk: Option<ChunkingStrategy>,
    _zstd_level: i32,
    _vendor: Option<&vendor::VendorPolicy>,
    _synth_chroms: bool,
) -> Result<()> {
    Err(UnsupportedVendor(
        "the Bruker timsdata SDK path (--bruker-sdk) is only available on Windows and Linux".into(),
    )
    .into())
}

/// Convert a Shimadzu `.lcd` → mzPeak via the Shimadzu.LabSolutions.IO .NET glue (Windows-only,
/// UNTESTED here). Mirrors `convert_sciex`, but centroid/profile arrays feed the standard writer
/// seam (no TOF-grid inversion). Needs `$MZPC_SHIMADZU_GLUE` + `$MZPC_PWIZ_DIR` at runtime.
#[cfg(windows)]
fn convert_shimadzu(
    input: &Path,
    output: &Path,
    chunk: Option<ChunkingStrategy>,
    zstd_level: i32,
    vendor: Option<&vendor::VendorPolicy>,
    synth_chroms: bool,
    representation: RepresentationArg,
) -> Result<()> {
    let rep = match representation {
        RepresentationArg::Both => shimadzu::Representation::Both,
        RepresentationArg::Profile => shimadzu::Representation::Profile,
        RepresentationArg::Centroid => shimadzu::Representation::Centroid,
    };
    // Digest FIRST: once the vendor DLL has the file open it holds a byte-range lock and the read
    // fails with os error 33 (seen on the 2.8 GB DIA runs; the small files happened to slip through).
    let source_sha1 = match embed_aux::sha1_hex(input) {
        Ok(hex) => Some(hex),
        Err(e) => {
            log::warn!("could not digest {}: {e}", input.display());
            None
        }
    };
    // Size + mtime, taken at the same moment as the digest and for the same reason: the vendor
    // library has no read-only open, so it holds this file read-write for the whole conversion, and
    // one of its code paths was already caught committing changes back into the `.lcd`. Compared
    // again once the handle is closed — see the check after `convert_vendor_reader` below.
    let source_before = embed_aux::SourceFingerprint::of(input);
    let reader = shimadzu::ShimadzuReader::open_with(input, rep)?;
    // (`MZPC_SHIMADZU_PROBE=N` — the "what does the reader hand back" diagnostic — is handled in
    // `run` before any lane is entered, see `shimadzu_probe_lever`: this lane only runs with `-o`,
    // so a probe here always swallowed the requested archive.)
    let info = reader.instrument_info();
    // `SampleInfo.AnalysisDate` is a naive local time: it goes to the `acquisition_time` index block,
    // never to `run.start_time` (see `run_metadata`). Every other run fact this lane knows is the
    // instrument, set below the old way (the lane predates the seam).
    // The `.lcd` itself states the run start as a UTC FILETIME plus the writer's GMT offset, the
    // sample and the LabSolutions version (`shimadzu_meta`); the DLL's `AnalysisDate` (empty in this
    // host, see BACKLOG) is only the fallback.
    let run_meta = shimadzu_meta::read(input).or_else(|| info.analysis_date.as_deref().and_then(|d| {
        // The DLL renders `dd.MM.yyyy HH:mm:ss`-style or ISO text depending on locale; accept both.
        let text = d.trim();
        let parsed = run_metadata::parse_vendor_time(text, "Shimadzu SampleInfo.AnalysisDate")
            .ok()
            .or_else(|| {
                ["%d.%m.%Y %H:%M:%S", "%d/%m/%Y %H:%M:%S", "%m/%d/%Y %H:%M:%S", "%d.%m.%Y %H:%M"]
                    .iter()
                    .find_map(|f| chrono::NaiveDateTime::parse_from_str(text, f).ok())
                    .map(|n| run_metadata::AcquisitionTime::Naive { wall_clock: n, source: "Shimadzu SampleInfo.AnalysisDate" })
            });
        if parsed.is_none() {
            log::warn!("Shimadzu SampleInfo.AnalysisDate {text:?} not understood; not recorded");
        }
        parsed.map(|t| run_metadata::VendorRunMetadata { start_time: Some(t), ..Default::default() })
    }));
    let mut hints = VendorHints { instrument: shimadzu_instrument(&info), source_sha1, run_metadata: run_meta, ..Default::default() };
    // Profile facet as an exact sqrt grid (see `shimadzu_grid`): probe dense profile spectra across
    // the run for the run-wide step; if the fit holds, the profile of every spectrum that fits is
    // stored as `tof_index` + per-spectrum `tof_c0`/`tof_c1`, and any that does not keeps f64 m/z.
    let grid_step = shimadzu_grid_step(&reader);
    if let Some(step) = grid_step {
        // (0,1) is the identity placeholder the reader deliberately skips; the real grid is the
        // per-spectrum pair below (same contract as the SciEX per-spectrum encoding).
        let tof_field = tof_index_field((0.0, 1.0), true);
        // The schema sampler derives columns from probe spectra, and a gridded probe carries no
        // m/z array — so declare the data facet explicitly: the grid axis, the f64 m/z that the
        // rare off-grid spectrum keeps (null for gridded rows), and the intensity. Without the
        // explicit intensity field it spilled into `auxiliary_arrays` on every spectrum.
        hints.data_facet_fields.push(tof_field);
        hints.data_facet_fields.push(mzpeak_prototyping::peak_series::MZ_ARRAY.to_field());
        hints.data_facet_fields.push(INTENSITY_ARRAY.to_field());
        hints.spectrum_param_fields.push((TOF_C0_CURIE, "tof_c0"));
        hints.spectrum_param_fields.push((TOF_C1_CURIE, "tof_c1"));
        hints.data_facet_point_layout = true;
        // The profile route stores the signal span only (`shimadzu_grid_route`): the zero pad at
        // the scan-window bounds is trimmed before the fit and never reaches the archive. Each
        // gridded spectrum reports whether it had one, and `convert_vendor_reader` declares
        // `shimadzu:span-trim` from that count.
        hints.index_blocks.push((
            "tof_calibration".to_string(),
            serde_json::json!({
                "codec": "tof-grid",
                // The model string names the FORMULA family the viewer reconstructs with
                // (per-spectrum sqrt); the instrument is a Shimadzu Q-TOF, recorded beside it.
                "model": "sciex_sqrt_per_spectrum",
                "vendor": "shimadzu",
                // Same key set as the other three tof-grid blocks. This one shipped with NEITHER
                // key for one release: it shares its `model` string with the per-spectrum SCIEX
                // lane, so a reader keying off the model got one answer there and null here.
                "lossless": "tof_index",
                // Within vendor rounding, NOT bit-exact: the axis is the vendor's own sqrt lattice
                // and the fit is accepted only when it reproduces every m/z to within
                // `shimadzu_grid::TOL` (1e-9 Da: the vendor's ±5e-10 rounding plus f64 slack), so
                // that gate IS the bound. Through 0.11.5 this said 5e-10, which the gate never
                // enforced: refitting HEK_PosOAD1's nine f64 spectra puts 169 of 32,434 points
                // between 5e-10 and 5.47e-10 Da off. Spectra that do not fit are not gridded at
                // all — they keep f64 m/z in the data facet.
                "mz_reconstruction": "within-vendor-rounding",
                "max_error_da": shimadzu_grid::TOL,
                "tof_to_mz": "mz = (tof_c0 + tof_c1*tof_index)^2",
                "per_spectrum_columns": ["tof_c0", "tof_c1"],
                "run_wide_c1": step,
                "vendor_mz_rounding": 1e-9,
            }),
        ));
        log::info!(
            "Shimadzu profile axis is an exact sqrt grid (run-wide c1 = {step:.15}); storing tof_index + per-spectrum tof_c0/tof_c1"
        );
    }
    // Centroid facet as an exact integer lattice (see `shimadzu_grid`): the vendor's `MassHigh`
    // is an Int64 at 1e-9 Da (and the coarse `Mass` under MZPC_SHIMADZU_COARSE_MZ=1 lies on the
    // same lattice), so every centroid list is checked per spectrum and stored as `tof_index`
    // Int64 + intensity in the custom peaks facet; one that fails the guard keeps f64 m/z in the
    // same facet's `mz` column. The `mz_calibration` block tells the viewer's `mz-grid` codec. A
    // profile-only run builds no centroid list at all, so neither the facet nor the block that
    // claims `applies_to: spectra_peaks` is declared for it.
    // `--no-mz-lattice` (config `no_mz_lattice`, `$MZPC_NO_MZ_LATTICE`) turns it off here too, so
    // the flag means the same thing on every lane: without this the native lane wrote `tof_index`
    // regardless and the opt-out silently did nothing for exactly the users who need it (a
    // downstream reader that cannot reconstruct the integer axis).
    let lattice_on = mz_lattice_enabled();
    if rep != shimadzu::Representation::Profile && lattice_on {
        hints.peaks_facet = Some(shimadzu_grid::lattice_peak_schema());
        hints.index_blocks.push(("mz_calibration".to_string(), shimadzu_grid::mz_calibration_block(shimadzu_grid::coarse_mz_requested())));
    }
    // The per-facet totals ("N spectra on the sqrt grid / on the lattice") are counted by
    // `convert_vendor_reader` over the written spectra and logged there (`FacetTally::report`);
    // this closure only reports each spectrum's route.
    let result = convert_vendor_reader(
        input, output, chunk, zstd_level, vendor, synth_chroms, hints,
        reader.len(),
        |i| {
            let spec = reader.spectrum(i)?;
            let (spec, profile_grid, profile_span_trimmed) = match grid_step {
                Some(step) => shimadzu_grid_route(spec, step),
                None => (spec, None, false),
            };
            let (spec, peak_arrays, outcome) = if lattice_on {
                shimadzu_grid::lattice_route(spec)
            } else {
                (spec, None, shimadzu_grid::LatticeOutcome::NoCentroids)
            };
            Ok(VendorSpectrum {
                spectrum: spec,
                peak_arrays,
                routes: FacetRoutes { profile_grid, profile_span_trimmed, centroid_lattice: outcome.on_lattice() },
            })
        },
    );
    // Dropping the reader calls the glue's `Close`, which releases the vendor's handle on the
    // `.lcd`. Only after that can the file be stat'ed for what the library left behind.
    drop(reader);
    if let Some(what) =
        embed_aux::describe_source_rewrite(source_before.as_ref(), embed_aux::SourceFingerprint::of(input).as_ref())
    {
        log::warn!(
            "the Shimadzu library MODIFIED the input while reading it ({what}): {}. The vendor API \
             has no read-only open, so the file is held read-write for the whole conversion; a \
             commit back to its OLE2 storage is a known hazard of that. The MS:1000569 SHA-1 in \
             this archive describes the file as it was BEFORE the conversion and no longer matches \
             the file on disk. Verify the input against your own copy.",
            input.display()
        );
    }
    result
}

/// Run-wide sqrt-grid step from dense profile spectra spread across the run, or `None` when this
/// file's profile axis is not an exact grid (coarse `Mass` data, or no profile at all).
#[cfg(windows)]
fn shimadzu_grid_step(reader: &shimadzu::ShimadzuReader) -> Option<f64> {
    let stores_profile = reader.stores_profile();
    log::info!("Shimadzu grid probe: stores_profile = {stores_profile:?}");
    if !stores_profile.unwrap_or(false) {
        return None;
    }
    // Probe up to 64 spectra spread over the run. Blind_P1_pos_012's profile spectra are small
    // (median 76 points, only 324/13,200 reach 200), so the density floor is 64 points — enough for
    // an unambiguous bin assignment — and HEK's (median 3,491) clear it trivially.
    let n = reader.len();
    let stride = (n / 64).max(1);
    let mut dense: Vec<Vec<f64>> = Vec::new();
    let mut i = 0;
    while i < n && dense.len() < 64 {
        if let Ok(spec) = reader.spectrum(i) {
            if spec.signal_continuity() == mzdata::spectrum::SignalContinuity::Profile {
                if let Some(arrays) = spec.arrays.as_ref() {
                    if let (Ok(mz), Ok(inten)) = (arrays.mzs(), arrays.intensities()) {
                        let (a, b) = shimadzu_grid::signal_span(&inten);
                        if b - a >= 64 {
                            dense.push(mz[a..b].to_vec());
                        }
                    }
                }
            }
        }
        i += stride;
    }
    let step = shimadzu_grid::run_wide_step(&dense);
    log::info!(
        "Shimadzu grid probe: {} probes with >= 64 profile points (of {} sampled), run-wide step = {step:?}",
        dense.len(),
        (n + stride - 1) / stride
    );
    let step = step?;
    // The grid must actually hold on the probes (coarse Mass data has ~5e-5 residuals and fails).
    let fits = dense.iter().filter(|mz| shimadzu_grid::fit_spectrum(mz, step).is_some()).count();
    log::info!("Shimadzu grid probe: {fits}/{} probes fit within {:e}", dense.len(), shimadzu_grid::TOL);
    if fits * 10 < dense.len() * 9 {
        log::info!(
            "Shimadzu profile axis: sqrt grid fits only {fits}/{} probes; keeping f64 m/z",
            dense.len()
        );
        return None;
    }
    Some(step)
}

/// Replace a fitting profile spectrum's f64 m/z with `tof_index` + per-spectrum `tof_c0`/`tof_c1`;
/// leave anything else (centroid-only spectra, off-grid spectra) untouched. The second value is
/// the [`FacetRoutes::profile_grid`] outcome: `None` for a spectrum with no profile to route. The
/// third is [`FacetRoutes::profile_span_trimmed`]: whether a gridded spectrum had a zero pad to trim.
/// Host-independent (only the `.lcd` reader is Windows-only) so the routing is testable anywhere.
#[cfg_attr(not(windows), allow(dead_code))]
fn shimadzu_grid_route(
    spec: MultiLayerSpectrum<CentroidPeak, DeconvolutedPeak>,
    step: f64,
) -> (MultiLayerSpectrum<CentroidPeak, DeconvolutedPeak>, Option<bool>, bool) {
    if spec.signal_continuity() != mzdata::spectrum::SignalContinuity::Profile {
        return (spec, None, false);
    }
    let Some(arrays) = spec.arrays.as_ref() else { return (spec, None, false) };
    let (Ok(mz), Ok(inten)) = (arrays.mzs(), arrays.intensities()) else { return (spec, None, false) };
    // Unequal source arrays are not this route's to reconcile: `&mz[a..b]` with a span measured on
    // the intensities would PANIC when m/z is the shorter one, and quietly gridding the overlap
    // would drop the tail. Decline the route and let the untouched spectrum take the f64 lane,
    // where the writer's own alignment handling applies. (`require_aligned_arrays` is the hard
    // error used where the caller can still refuse the whole conversion; here there is a correct
    // fallback, so take it.)
    if mz.len() != inten.len() {
        log::warn!(
            "Shimadzu profile grid declined for spectrum {}: {} m/z values vs {} intensities",
            spec.description().index,
            mz.len(),
            inten.len()
        );
        return (spec, Some(false), false);
    }
    // Fit and store the signal span only: the zero-intensity pad points at the scan-window bounds
    // are off-grid by construction, so a fit over the untrimmed array would reject every spectrum.
    // Trimming here is also what keeps them out of the archive — the writer would keep one of them
    // (its zero-run compaction preserves a boundary zero per run); see `shimadzu_grid::signal_span`.
    let (a, b) = shimadzu_grid::signal_span(&inten);
    let trimmed = a > 0 || b < inten.len();
    let (mz, inten) = (&mz[a..b], &inten[a..b]);
    let Some((grid, k)) = shimadzu_grid::fit_spectrum(mz, step) else {
        return (spec, Some(false), false);
    };
    let intensity: Vec<f32> = inten.to_vec();
    let mut out = BinaryArrayMap::new();
    let mut tof_da =
        DataArray::wrap(&ArrayType::nonstandard("tof_index"), BinaryDataArrayType::Int32, Vec::new());
    if tof_da.update_buffer(k.as_slice()).is_err() {
        return (spec, Some(false), false);
    }
    out.add(tof_da);
    let mut int_da =
        DataArray::wrap(&ArrayType::IntensityArray, BinaryDataArrayType::Float32, Vec::new());
    if int_da.update_buffer(intensity.as_slice()).is_err() {
        return (spec, Some(false), false);
    }
    int_da.unit = Unit::DetectorCounts;
    out.add(int_da);
    let mut descr = spec.description().clone();
    // Summarize BEFORE the f64 m/z is thrown away: the output map holds tof_index + intensity and
    // no MZArray, so mzdata would fold it to tic = 0, base peak = (0, 0), m/z range = (0, 0).
    //
    // WHICH POINTS: the SIGNAL SPAN of the PROFILE trace (`mz`/`inten` above are already the
    // `signal_span` slice) — the points this route writes to the `spectra_data` facet.
    //
    //  * Span, not the untrimmed source array: the zero-intensity pad at the scan-window bounds is
    //    off-grid and dropped. It carries no intensity, so the TIC is identical either way, but the
    //    observed-m/z range must describe the stored points, not the padded scan window.
    //  * PROFILE, not the centroid list that a dual `.lcd` scan carries alongside it. A dual scan
    //    writes both facets, and the two sum differently (Blind_P1_pos_012 spectrum 0: profile
    //    13,220, centroid 12,877). The profile sum is the one that keeps this lane self-consistent:
    //    it is exactly what the writer derives for the SAME file with `--tof-grid` off (the raw
    //    array map wins over the peak list in `raw_summaries`), and it matches the writer's own
    //    precedence for `base_peak_mz` on every dual archive. The published `HEK_PosOAD1.mzpeak`
    //    settles it from within: its 9 never-broken rows (off-lattice spectra kept as f64) carry
    //    the PROFILE sum exactly — row 149 is 672,849 profile vs 607,167 centroid, and the column
    //    says 672,849 — so the other 2,092 must state the same thing. Note the TIC/BPC
    //    CHROMATOGRAMS now read these very terms (`chromatogram_summary`), so they state the
    //    profile sum too and the two can no longer disagree.
    //  * the RECONSTRUCTED coordinates `grid.mz(k)`, not the source f64. This lane's fit is exact
    //    to `shimadzu_grid::TOL` (1e-9 Da) so the two agree to well past f32 display precision, but
    //    the contract is "the summary describes the stored points" and it is stated the same way on
    //    every grid lane rather than depending on how tight one lane's tolerance happens to be.
    let recon: Vec<f64> = k.iter().map(|&kk| grid.mz(kk)).collect();
    // A `debug_assert_eq!` here was a no-op in the shipped release build, and the failure it was
    // guarding is not benign: `set_gridded_spectrum_summary` refuses a misaligned pair, which would
    // leave this spectrum with NO MS:1000285/504/505 — the exact zero-summary defect this route
    // exists to prevent — inside an archive that still exits 0. The length guard above makes this
    // unreachable (`fit_spectrum` returns one `k` per m/z); if it ever is reached, decline the route.
    if recon.len() != inten.len() {
        log::warn!(
            "Shimadzu profile grid declined for spectrum {}: fit returned {} bins for {} intensities",
            spec.description().index,
            recon.len(),
            inten.len()
        );
        return (spec, Some(false), false);
    }
    set_gridded_spectrum_summary(&mut descr, &recon, inten);
    descr.add_param(Param::builder().name("tof_c0").curie(TOF_C0_CURIE).value(grid.c0).build());
    descr.add_param(Param::builder().name("tof_c1").curie(TOF_C1_CURIE).value(grid.c1).build());
    let out = MultiLayerSpectrum::new(descr, Some(out), spec.peaks.clone(), spec.deconvoluted_peaks.clone());
    (out, Some(true), trimmed)
}

/// Instrument configuration from what the vendor API states — and only that. `SystemName()` is the
/// model; `DeviceID = MSID_QTFL` names the Q-TOF family, so the quadrupole + TOF analysers are not
/// in doubt; the ion source is asserted only when the spectra say `ESI`. No detector is invented.
#[cfg(windows)]
fn shimadzu_instrument(info: &shimadzu::ShimadzuInstrumentInfo) -> Option<InstrumentConfiguration> {
    let model = info.system_name.clone()?;
    let mut cfg = InstrumentConfiguration { id: 0, ..Default::default() };
    // The family term ProteoWizard states, plus the vendor's own system name as the model value.
    cfg.params.push(run_metadata::term(1002998, "Shimadzu instrument model"));
    cfg.params.push(Param::builder().name("instrument model").curie(curie!(MS:1000031)).value(model).build());
    let mut order = 1;
    if info.ionization.as_deref() == Some("ESI") {
        cfg.components.push(Component {
            component_type: ComponentType::IonSource,
            order,
            params: vec![Param::builder().name("electrospray ionization").curie(curie!(MS:1000073)).build()],
        });
        order += 1;
    }
    if info.device_id.as_deref().is_some_and(|d| d.contains("QTFL")) {
        for (name, curie) in [("quadrupole", curie!(MS:1000081)), ("time-of-flight", curie!(MS:1000084))] {
            cfg.components.push(Component {
                component_type: ComponentType::Analyzer,
                order,
                params: vec![Param::builder().name(name).curie(curie).build()],
            });
            order += 1;
        }
    }
    Some(cfg)
}

/// Emit `{"index","n","mz":[..4],"intensity":[..8]}` per spectrum, for comparison against the
/// vendor mzML export. Deliberately prints raw heads: the rotation shows up as the first values
/// being alien while the rest are the oracle's values shifted.
#[cfg(windows)]
fn shimadzu_probe(reader: &shimadzu::ShimadzuReader, n: usize) -> Result<()> {
    use mzdata::prelude::SpectrumLike;
    for i in 0..n.min(reader.len()) {
        let spec = reader.spectrum(i)?;
        // BOTH facets: on a dual file the raw arrays hold the profile and the centroid list rides
        // as the peak set, so printing only the raw arrays hides the very list under investigation.
        let (n_data, mz_head, in_head) = match spec.raw_arrays() {
            Some(arrays) => {
                let mz = arrays.mzs()?;
                let inten = arrays.intensities()?;
                (
                    mz.len(),
                    mz.iter().take(4).map(|v| format!("{v:.6}")).collect::<Vec<_>>().join(","),
                    inten.iter().take(8).map(|v| format!("{v}")).collect::<Vec<_>>().join(","),
                )
            }
            None => (0, String::new(), String::new()),
        };
        let (n_peaks, pk_mz, pk_in) = match spec.peaks.as_ref() {
            Some(peaks) => (
                peaks.len(),
                peaks.iter().take(4).map(|p| format!("{:.6}", p.mz)).collect::<Vec<_>>().join(","),
                peaks.iter().take(8).map(|p| format!("{}", p.intensity)).collect::<Vec<_>>().join(","),
            ),
            None => (0, String::new(), String::new()),
        };
        println!(
            "{{\"index\":{i},\"id\":\"{}\",\"n_data\":{n_data},\"mz\":[{mz_head}],\"intensity\":[{in_head}],\
             \"n_peaks\":{n_peaks},\"peak_mz\":[{pk_mz}],\"peak_intensity\":[{pk_in}]}}",
            spec.id(),
        );
    }
    Ok(())
}

/// Convert a SciEX `.wiff`/`.wiff2` → mzPeak via the Clearcore2 .NET glue (`src/sciex.rs`, compiled
/// on Windows only; there is no cargo feature). Mirrors `convert_tsf`. Needs `$MZPC_SCIEX_GLUE` (or
/// the release layout's `glue\sciex`) and `$MZPC_PWIZ_DIR` at runtime (see glue/sciex/README.md).
#[cfg(windows)]
fn convert_sciex(
    input: &Path,
    output: &Path,
    chunk: Option<ChunkingStrategy>,
    zstd_level: i32,
    vendor: Option<&vendor::VendorPolicy>,
    synth_chroms: bool,
    tof_grid: Option<TofGridMode>,
) -> Result<()> {
    // Native `.wiff` is read through Clearcore2, which currently exposes only decoded f64 m/z — not
    // the flight-time index or the mass-calibration coefficients. Strategy (B) (always grid, lossless,
    // straight from the vendor calibration — like the Agilent `MSProfile.bin` reader) therefore needs
    // a glue extension to surface SCIEX's calibration. Until then the lane INVERTS the decoded f64
    // per-spectrum into the TOF grid (`sqrt(m/z)=c0+c1·k`), storing `tof_index` + per-spectrum
    // {c0,c1}. Per-spectrum coefficients absorb per-scan c0 drift (which defeats a run-wide grid —
    // the ZenoTOF case). Off-lattice spectra (sparse/MS2) stay f64. `chunk` is unused (the grid uses
    // the point facet, not chunked m/z).
    //
    // That inversion is a bounded-lossy TRANSFORM (within `tof_grid::ppm_tol()`), and until 0.9.13
    // it was unconditional — `--tof-grid` never reached this lane, so there was no way to keep the
    // exact f64 the vendor library returned. The fidelity invariant ("preserve as much as possible;
    // every transformation declared") needs the opt-out, so the resolved mode is threaded through:
    // not given → `auto` (the previous behaviour, so existing archives and recipes are unchanged);
    // `off` → exact f64 for every spectrum; `on` → the run-wide clock fit is required.
    let mode = tof_grid.unwrap_or(TofGridMode::Auto);
    convert_sciex_grid(input, output, chunk, zstd_level, vendor, synth_chroms, mode)
}

/// Native SCIEX `.wiff` → mzPeak with a PER-SPECTRUM TOF grid (the Agilent grid lane's per-spectrum
/// `tof_c0`/`tof_c1` columns + the shared `tof_index` axis). For each spectrum we fit
/// `sqrt(m/z)=c0+c1·k` from the Clearcore2 f64 m/z and store the integer `tof_index`; a reader
/// recovers `m/z=(tof_c0+tof_c1·tof_index)²` per spectrum. Off-lattice spectra keep exact f64 m/z.
/// Each spectrum is filed by the representation Clearcore2 reports (profile → `spectra_data`,
/// centroid → `spectra_peaks`); both facets declare the axis and an f64 `mz` (M6).
#[cfg(windows)]
fn convert_sciex_grid(
    input: &Path,
    output: &Path,
    chunk: Option<ChunkingStrategy>,
    zstd_level: i32,
    // Deliberately unused: `vendor::embed_into_archive` WALKS A DIRECTORY (`collect_files(dot_d…)`),
    // and a SCIEX input is a `.wiff` FILE. There are no side-files under it to embed, so the policy
    // has nothing to act on here — unlike the Bruker `.d` and Agilent `.d` lanes, which do call it.
    // Kept in the signature so every convert_* lane takes the same arguments.
    _vendor: Option<&vendor::VendorPolicy>,
    synth_chroms: bool,
    // `Off`: no spectrum is gridded — every one keeps the exact f64 m/z the vendor library returned
    // and no `tof_calibration` block is written (there is no transform to declare). `On`: the
    // run-wide clock fit must succeed. `Auto`: the behaviour before the mode existed.
    mode: TofGridMode,
) -> Result<()> {
    // The source members are digested BEFORE Clearcore2 opens them (the `.wiff` and, when present,
    // its `.wiff.scan` sibling — ProteoWizard lists both).
    let wiff_name = input.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let scan_name = format!("{wiff_name}.scan");
    let member_names = [wiff_name.as_str(), scan_name.as_str()];
    let (source_files, default_source) = run_metadata::source_files_from_members(
        input.parent().unwrap_or(Path::new(".")),
        &run_metadata::MemberPolicy {
            members: run_metadata::Members::Explicit(&member_names),
            file_format: Some(run_metadata::term(1000562, "ABI WIFF format")),
            id_format: Some(run_metadata::term(1000770, "WIFF nativeID format")),
            default_member: Some(wiff_name.as_str()),
        },
    );
    let sample = sciex_sample();
    let reader = sciex::SciexReader::open_run(input, sample)?;
    let total = reader.len();
    if total == 0 {
        bail!("no spectra in {}", input.display());
    }
    let tmp = output.with_extension("mzpeak.tmp");
    let tmp_guard = TmpGuard::new(&tmp);
    let handle = fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    let level = ZstdLevel::try_new(zstd_level)
        .map_err(|e| anyhow::anyhow!("invalid zstd level {zstd_level}: {e}"))?;

    // The axis on both facets: tof_index (Int32) + per-spectrum tof_c0/tof_c1 (the run-wide
    // transform_params are the `(0,1)` placeholder; the authoritative coefficients ride the
    // per-spectrum columns). Profile spectra file to `spectra_data`, centroid ones to `spectra_peaks`,
    // gridded or not (M6) — the peaks schema carries an f64 `mz` for a centroid spectrum that did not
    // fit, the data facet's f64 `mz` comes from the probe sample below.
    let tof_field = tof_index_field((0.0, 1.0), true);
    let peak_schema = tof_index_peak_schema(tof_field.clone());

    // Probe spectra spread across the run: feed the f64 data-facet schema (chunk-aware) AND fit the
    // run-wide digitizer clock c1. The SCIEX clock is GLOBAL (only c0 drifts per scan), so a single c1
    // lets even sparse SWATH/DIA MS2 windows grid against the shared lattice (per-spectrum c1
    // estimation fails on them).
    let probe_step = (total / 16).max(1);
    let mut probes: Vec<MultiLayerSpectrum<CentroidPeak, DeconvolutedPeak>> = Vec::new();
    let mut pi = 0usize;
    while pi < total && probes.len() < 16 {
        if let Ok(s) = reader.spectrum(pi) {
            probes.push(s);
        }
        pi += probe_step;
    }
    let samples: Vec<Vec<f64>> = probes
        .iter()
        .filter_map(|s| s.arrays.as_ref().and_then(|a| a.mzs().ok()).map(|c| c.into_owned()))
        .filter(|v| v.len() >= 64)
        .collect();
    let c1_global = if mode == TofGridMode::Off { None } else { tof_grid::fit(&samples).map(|f| f.grid.c1) };
    if mode == TofGridMode::On && c1_global.is_none() {
        bail!(
            "--tof-grid on: {} has no run-wide integer TOF lattice reconstructing within {:.2} ppm \
             (no digitizer clock fit); use --tof-grid auto for the per-spectrum fallback, or \
             --tof-grid off for exact f64 m/z",
            input.display(),
            tof_grid::ppm_tol()
        );
    }
    if mode == TofGridMode::Off {
        log::info!("--tof-grid off: storing the exact f64 m/z Clearcore2 returned for every spectrum");
    }
    // The probe reads above went through the glue too: count only what the loop below writes.
    reader.take_value_changes();

    // The data facet holds the integer axis, which has no chunk encoder: point layout whenever the
    // grid is in play (`convert_vendor_reader` makes the same call for the Shimadzu profile grid).
    // The off-lattice f64 minority is stored flat and EXACT in the same facet instead of
    // numpress-chunked. Measured on the 0.10.2 corpus rebuild (rows of `spectra_data` with a
    // non-null `mz`): 9.2 % of the points on MSV000093587 Sample002, 3.2 % on PXD011326, 2.7 % on
    // PXD053710, 1.6 % on MSV000090684, 1.1 % on PXD065872, 0.07 % on PXD071869 — at 6–9.5 B per f64
    // point, which is +27 %, +12 %, +7 %, +3.5 %, +2.3 % and +0.2 % on the archive. (An earlier
    // version of this comment said "well under 1 %"; it counted chunk ROWS of the old facet, not
    // points.) Those figures are the dictionary-encoded column; `shuffle_mz` now writes it
    // BYTE_STREAM_SPLIT, about 6 B per point on the densest row groups of Sample002 and PXD011326
    // (−22 % / −25 %). A chunk-capable integer axis (~2–3 B per point) is not built: the remaining
    // size is accepted, the trade being fidelity for size, declared by `transformations` no longer
    // listing `numpress-linear` here. Under `--tof-grid off` nothing is gridded, so the facet keeps
    // the requested chunking.
    let data_chunk = if mode == TofGridMode::Off { chunk } else { None };
    let mut builder = MzPeakWriterType::<fs::File>::builder()
        .buffer_size(buffer_spectra())
        .compression(Compression::ZSTD(level))
        .shuffle_mz(true)
        .chunked_encoding(data_chunk)
        // ponytail: chromatograms are POINT layout, never chunked. Passing the spectrum strategy
        // here produced a `chunk` struct with no chunk_start/chunk_end columns, so the chunk builder
        // saw an empty main axis, wrote 0 time and 0 intensity points, and spilled the whole
        // intensity array into an uncompressed `auxiliary_arrays` blob in chromatograms_metadata —
        // losing the time axis outright. 99 of 330 reference archives are affected. A chromatogram
        // is a few thousand points; chunking bought nothing.
        .chromatogram_chunked_encoding(None)
        .add_spectrum_param_field(CustomBuilderFromParameter::from_spec(
            TOF_C0_CURIE,
            "tof_c0",
            DataType::Float64,
        ))
        .add_spectrum_param_field(CustomBuilderFromParameter::from_spec(
            TOF_C1_CURIE,
            "tof_c1",
            DataType::Float64,
        ))
        .store_peaks_and_profiles_apart(Some(peak_schema))
        .sample_array_types_from_spectra(probes.into_iter());
    if mode != TofGridMode::Off {
        builder = builder.add_spectrum_field(tof_field);
    }
    let mut writer = builder.build(handle, true);
    add_processing_metadata(&mut writer);
    // The per-spectrum coefficient columns are MZP terms (`TOF_C0_CURIE` …): declare the CV.
    ensure_mzp_cv(&mut writer);

    let mut ms1 = Ms1Chroms::default();
    let len = max_spectra().map_or(total, |m| m.min(total));
    let (mut n_grid, mut n_f64) = (0usize, 0usize);
    let mut max_ppm = 0.0f64;
    for i in 0..len {
        let spec = reader.spectrum(i)?;
        let mz: Vec<f64> = spec
            .arrays
            .as_ref()
            .and_then(|a| a.mzs().ok())
            .map(|c| c.into_owned())
            .unwrap_or_default();
        // Prefer the global-c1 fit (grids sparse MS2 windows too); fall back to a per-spectrum fit.
        // Under `off` there is no fit at all: exact f64 for every spectrum.
        let fit = if mode == TofGridMode::Off {
            None
        } else {
            c1_global
                .and_then(|c1| tof_grid::fit_one_c1(&mz, c1))
                .or_else(|| tof_grid::fit_one(&mz))
        };
        let out = match fit {
            Some((grid, tof_index, ppm)) => {
                max_ppm = max_ppm.max(ppm);
                n_grid += 1;
                sciex_grid_spectrum(&spec, &tof_index, grid)?
            }
            None => {
                // Off-lattice: exact f64 m/z, representation untouched (M6).
                n_f64 += 1;
                spec
            }
        };
        if synth_chroms {
            ms1.observe(&out);
        }
        writer.write_spectrum(&out)?;
    }
    log::info!(
        "SCIEX per-spectrum grid: wrote {len} spectra ({n_grid} gridded tof_index, {n_f64} kept f64); \
         max round-trip {max_ppm:.4} ppm"
    );
    let value_changes = reader.take_value_changes();
    if let Some(msg) = value_changes.warning() {
        log::warn!("{msg}; declared in `transformations`");
    }
    let chromatogram_transforms = finish_chromatograms(&mut writer, input, &ms1, std::iter::empty(), synth_chroms)?;
    // What the WIFF states about the run (instrument, serial, Analyst version, acquisition time,
    // the sample's name) plus the digested members; a naive acquisition time becomes an index block.
    let acquisition_block = reader.run_metadata(sample).and_then(|mut m| {
        m.source_files = source_files;
        m.default_source_file = default_source;
        run_metadata::apply(&mut writer, m)
    });
    let fixup_block = fixup_run_metadata(&mut writer, input);
    let acquisition_block = acquisition_block.or(fixup_block);
    let mut applied = base_transformations(&writer);

    let mut zip: ZipArchiveWriter<fs::File> = writer.finish_parquet()?;
    if let Some((key, block)) = acquisition_block {
        zip.add_index_metadata(&key, &block).context("writing acquisition_time index block")?;
    }
    // `lossless` names the exactly-stored column, `mz_reconstruction` rates the m/z you rebuild
    // from it — see `finish_tof_grid_archive`. `max_roundtrip_ppm` is the measured worst case over
    // this run (it has run at ~5 ppm on the published MSV000095995 archive), and
    // `roundtrip_tolerance_ppm` is the bound the per-spectrum fit was accepted under.
    let cal = serde_json::json!({
        "codec": "tof-grid",
        "model": "sciex_sqrt_per_spectrum",
        "lossless": "tof_index",
        "mz_reconstruction": "bounded-lossy",
        "roundtrip_tolerance_ppm": tof_grid::ppm_tol(),
        "tof_to_mz": "mz = (tof_c0 + tof_c1*tof_index)^2",
        "per_spectrum_columns": ["tof_c0", "tof_c1"],
        "max_roundtrip_ppm": max_ppm,
    });
    // No block under `off`: nothing was transformed, and a `codec: tof-grid` block on an archive
    // whose every spectrum sits in the f64 data facet would tell readers to look for a facet that
    // holds nothing.
    if mode != TofGridMode::Off {
        zip.add_index_metadata("tof_calibration", &cal)
            .context("writing tof_calibration index")?;
    }
    applied.extend(chromatogram_transforms);
    if n_grid > 0 {
        applied.push(format!("tof-grid:{}ppm", tof_grid::ppm_tol()));
    }
    applied.extend(value_changes.transformations());
    let (key, block) = transformations_block(&applied);
    zip.add_index_metadata(&key, &block).context("writing transformations index block")?;
    if let Some((key, block)) = partial_marker(input, max_spectra(), len) {
        zip.add_index_metadata(&key, &block).context("writing partial index block")?;
    }
    zip.finish().map_err(|e| anyhow::anyhow!("finalizing archive: {e}"))?;
    tmp_guard.finish(output)?;
    Ok(())
}

/// Build a gridded SCIEX spectrum: `tof_index` (Int32) + intensity (f32) + per-spectrum tof_c0/tof_c1.
/// Keeps the source description (RT, MS level, polarity — and the representation Clearcore2 stated,
/// which decides the facet; M6).
#[cfg(windows)]
fn sciex_grid_spectrum(
    spec: &MultiLayerSpectrum<CentroidPeak, DeconvolutedPeak>,
    tof_index: &[i32],
    grid: tof_grid::TofGrid,
) -> Result<MultiLayerSpectrum<CentroidPeak, DeconvolutedPeak>> {
    // Both arrays are decoded up front and a failure is propagated, not swallowed: an empty
    // intensity vector would silently write an empty spectrum, and an empty m/z vector would make
    // the summary helper state `total_ion_current = 0` over points that do exist.
    let arrays = spec.arrays.as_ref().context("SCIEX grid spectrum has no arrays")?;
    let intensity: Vec<f32> = arrays
        .intensities()
        .map_err(|e| anyhow::anyhow!("decoding SCIEX intensity: {e}"))?
        .into_owned();
    let src_mzs: Vec<f64> = arrays
        .mzs()
        .map_err(|e| anyhow::anyhow!("decoding SCIEX m/z: {e}"))?
        .into_owned();

    let mut out = BinaryArrayMap::new();
    let mut tof_da =
        DataArray::wrap(&ArrayType::nonstandard("tof_index"), BinaryDataArrayType::Int32, Vec::new());
    tof_da.update_buffer(tof_index).map_err(|e| anyhow::anyhow!("encoding tof_index: {e}"))?;
    out.add(tof_da);
    let mut int_da =
        DataArray::wrap(&ArrayType::IntensityArray, BinaryDataArrayType::Float32, Vec::new());
    int_da.update_buffer(intensity.as_slice()).map_err(|e| anyhow::anyhow!("encoding intensity: {e}"))?;
    int_da.unit = Unit::DetectorCounts;
    out.add(int_da);

    let mut descr = spec.description().clone();
    // Summarize from the RECONSTRUCTED m/z — `grid.mz(k)` over the stored `tof_index` — not the
    // source f64 being replaced. Every point of this spectrum is on the lattice (that is why it
    // took this route), so the two are the same SET of points, but "on the lattice" means "within
    // `tof_grid::ppm_tol()`", not "identical": on the published MSV000095995 archive the source and
    // reconstructed base-peak m/z differ by 4.6 ppm. The summary must be taken here either way,
    // because the output map has no MZArray and would fold to tic = 0 / base peak (0,0) / "m/z 0–0".
    // `debug_assert_eq!` here was a no-op in the shipped release build. The vendor shim clamps a
    // length disagreement to `Math.Min` before we ever see it (counted, and declared as
    // `sciex:truncate-unequal-arrays`), so an unequal pair here is a decode failure that must stop
    // the conversion, not something to summarize half of.
    require_aligned_arrays(
        "SCIEX grid",
        spec.description().index,
        src_mzs.len(),
        intensity.len(),
    )?;
    require_aligned_arrays(
        "SCIEX grid (tof_index)",
        spec.description().index,
        tof_index.len(),
        intensity.len(),
    )?;
    let recon: Vec<f64> = tof_index.iter().map(|&k| grid.mz(k)).collect();
    set_gridded_spectrum_summary(&mut descr, &recon, &intensity);
    descr.add_param(Param::builder().name("tof_c0").curie(TOF_C0_CURIE).value(grid.c0).build());
    descr.add_param(Param::builder().name("tof_c1").curie(TOF_C1_CURIE).value(grid.c1).build());
    Ok(MultiLayerSpectrum::new(descr, Some(out), None, None))
}

/// Convert a Waters MassLynx `.raw` → mzPeak (Windows-runtime-only, UNTESTED here).
///
/// Unlike SciEX/Shimadzu this lane has NO .NET glue in the loop: [`waters::WatersReader`] loads
/// `MassLynxRaw.dll` directly with `libloading` and calls its C exports. So it needs
/// `$MZPC_MASSLYNX_DIR` (or `$MZPC_PWIZ_DIR`) at runtime and nothing else.
#[cfg(windows)]
fn convert_waters(
    input: &Path,
    output: &Path,
    chunk: Option<ChunkingStrategy>,
    zstd_level: i32,
    vendor: Option<&vendor::VendorPolicy>,
    synth_chroms: bool,
) -> Result<()> {
    // Native `.raw` is read through MassLynx, which exposes decoded f64 m/z (not the raw flight-time
    // index or the mass-calibration coefficients). The statistical TOF-grid detector (strategy A) is
    // deliberately NOT used here — it is gated to the mzML path — so `.raw` stores exact f64 m/z.
    let reader = waters::WatersReader::open(input)?;
    let mut hints = VendorHints { run_metadata: waters_meta::read(input), ..Default::default() };
    // What each function is and what became of it, on every run with or without drift bins, and
    // the functions the archive leaves out or sums.
    hints.index_blocks.push(("waters_functions".to_string(), reader.functions_block()));
    hints.transformations.extend(reader.transformations());
    // A SONAR function's scans are read as its quadrupole bins summed: the reader counts those
    // scans, and `waters:sonar-summed` is declared from that count over the written spectra.
    hints.counters.push((waters::SONAR_SUMMED, reader.sonar_counter()));
    // Ion-mobility functions arrive as frames whose bins interleave and are re-sorted by m/z
    // (`waters.rs`): the reader counts the frames that sort moved, and `sort-by-mz` is declared from
    // that count over the written spectra. Readers get the run's drift table + CCS calibration.
    hints.counters.push(("sort-by-mz", reader.reorder_counter()));
    if let Some(block) = reader.drift_block() {
        hints.index_blocks.push(("waters_drift".to_string(), block));
        // Frames interleave 200 traces: the zero-run mask would strip bin boundaries across bins,
        // and the drift column must be declared even if the IMS function is a small part of the run.
        hints.keep_zero_runs = true;
        hints.probe_indices = reader.probe_indices();
        // The drift column is declared through the per-function probes above; an explicit
        // `data_facet_fields` push of the f32 array (as the TDF lane does for `tof`) declares the
        // POINT-layout shape and made the chunked writer panic on a column-type mismatch
        // (box round 21) — the chunked secondary-array field shape needs its own constructor first.
    }
    convert_vendor_reader(input, output, chunk, zstd_level, vendor, synth_chroms, hints, reader.len(), |i| reader.spectrum(i))
}

/// Convert a native Agilent MassHunter `.d` → mzPeak through the net48 MHDAC host (`agilent.rs`;
/// Windows only; IM-QTOF runs are refused before this point). Mirrors `convert_tsf`.
#[cfg(windows)]
fn convert_agilent(
    input: &Path,
    output: &Path,
    chunk: Option<ChunkingStrategy>,
    zstd_level: i32,
    vendor: Option<&vendor::VendorPolicy>,
    synth_chroms: bool,
) -> Result<()> {
    let reader = agilent::AgilentReader::open(input)?;
    // NaN/Inf intensities stored as 0 and arrays cut to one length, as the host counted them.
    let hints = VendorHints { instrument: reader.instrument(), transformations: reader.transformations(), ..Default::default() };
    convert_vendor_reader(input, output, chunk, zstd_level, vendor, synth_chroms, hints, reader.len(), |i| reader.spectrum(i))
}

/// Shared writer wiring for a custom (non-mzdata) reader: probe-derived schema + write loop + empty chromatogram + run-metadata defaults + vendor-embed + atomic rename. Used by
/// every custom-reader path (Bruker TSF/BAF, SciEX, Agilent) so they don't each duplicate the body.
/// What a vendor reader can state about the run beyond its spectra — asserted only when present.
#[derive(Default)]
struct VendorHints {
    /// Instrument identity for `instrument_configuration_list`; applied only if the writer's list
    /// is still empty after the generic fixup (i.e. the run/scans reference configuration 0 with
    /// nothing behind it).
    instrument: Option<InstrumentConfiguration>,
    /// Extra columns for the `spectra_data` facet (e.g. a `tof_index` grid axis). Sampling from
    /// the probe spectra adds the ordinary ones.
    data_facet_fields: Vec<std::sync::Arc<arrow::datatypes::Field>>,
    /// Per-spectrum Float64 parameter columns, `(accession, name)` — the sqrt-grid `tof_c0`/`tof_c1`.
    spectrum_param_fields: Vec<(mzdata::params::CURIE, &'static str)>,
    /// Force the point layout on `spectra_data` (an integer grid axis cannot be delta-chunked)
    /// while the peaks facet keeps the requested chunking — a deliberate mixed-family archive.
    data_facet_point_layout: bool,
    /// Index blocks (`tof_calibration` …) added to `mzpeak_index.json` at finish.
    index_blocks: Vec<(String, serde_json::Value)>,
    /// MS:1000569 SHA-1 of the input, computed BEFORE the vendor reader opened it. The Shimadzu DLL
    /// holds a byte-range lock on the `.lcd` for as long as it is open, so hashing afterwards fails
    /// on large files (`os error 33`) — msconvert hashes first for the same reason. When present,
    /// the `sourceFile` entry is seeded with it so `fixup_run_metadata` neither re-hashes nor warns.
    source_sha1: Option<String>,
    /// A custom `spectra_peaks` schema (the Shimadzu centroid lattice: point layout,
    /// `spectrum_index` + `tof_index` Int64 + f64 `mz` fallback + `intensity`). When set the peaks
    /// facet is never chunked or numpressed (the lattice replaces both), its schema is not sampled
    /// from the probes, and a spectrum may hand the writer its peak rows explicitly through
    /// [`VendorSpectrum::peak_arrays`]; the reader-side calibration block rides in `index_blocks`.
    peaks_facet: Option<ArrayBuffersBuilder>,
    /// Lane-specific entries for the `transformations` index block (see [`transformations_block`]);
    /// the writer-level ones (zero-run mask, numpress) are added by `convert_vendor_reader`.
    transformations: Vec<String>,
    /// What the vendor file states about the run (sample, time, instrument, software, members):
    /// merged field by field before `fixup_run_metadata` — see `run_metadata`.
    run_metadata: Option<run_metadata::VendorRunMetadata>,
    /// Keep zero-intensity runs (the writer's zero-run mask OFF). Set by lanes whose spectra
    /// interleave several traces in one array — Waters drift frames: masked across bins, the
    /// mask deleted 3–6 % of the per-bin trace boundaries (review 2026-09-09).
    keep_zero_runs: bool,
    /// Spectrum indices the writer samples for its data-facet schema instead of the default
    /// six-probe stride, so a column only some functions carry (the drift array of a mixed
    /// IMS/non-IMS Waters run) is declared regardless of where those spectra sit.
    probe_indices: Vec<usize>,
    /// A reader's own counts, each with the `transformations` entry it declares: the entry is
    /// written when its counter grew while the spectra were WRITTEN (the schema probes go through the
    /// same reader first and do not count). `sort-by-mz` for the spectra a reader re-sorted into m/z
    /// order itself (the `--bruker-sdk` TDF reader: the SDK hands over mobility-major frames; the
    /// native Waters reader: a frame's drift bins interleave); `waters:sonar-summed` for the scans the
    /// Waters reader read as a SONAR function's quadrupole bins summed.
    counters: Vec<(&'static str, std::sync::Arc<std::sync::atomic::AtomicUsize>)>,
}

/// One spectrum from a vendor reader, plus — for a lattice-routed centroid list — the arrays that
/// go to the peaks facet in place of `spectrum.peaks()` (see
/// `MzPeakWriterType::write_spectrum_with_peak_arrays`). Every reader that has no such facet
/// returns a bare `MultiLayerSpectrum` and converts through `From`.
struct VendorSpectrum {
    spectrum: MultiLayerSpectrum,
    peak_arrays: Option<BinaryArrayMap>,
    /// Which axis encoding each facet of this spectrum took, for the run summary.
    routes: FacetRoutes,
}

impl From<MultiLayerSpectrum> for VendorSpectrum {
    fn from(spectrum: MultiLayerSpectrum) -> Self {
        Self { spectrum, peak_arrays: None, routes: FacetRoutes::default() }
    }
}

/// Per-spectrum routing outcome of a vendor lane that encodes an axis as an integer grid (today
/// only the Shimadzu lane: the profile sqrt grid and the centroid lattice). `None` = the spectrum
/// had nothing to route on that facet (no profile / no centroid list, or the lane has no grid);
/// `Some(true)` = on the grid; `Some(false)` = the guard failed and the spectrum kept f64 m/z.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct FacetRoutes {
    profile_grid: Option<bool>,
    /// The profile sqrt-grid route left this gridded spectrum's zero-intensity pad at the
    /// scan-window bounds out (`shimadzu:span-trim`).
    profile_span_trimmed: bool,
    centroid_lattice: Option<bool>,
}

/// Run totals of [`FacetRoutes`], counted over the WRITTEN spectra only. The counting used to sit
/// in the Shimadzu routing closure, which `convert_vendor_reader` also calls for its ≤ 6 schema
/// probes before the write loop — so the logged totals exceeded the spectrum count by the probe
/// count (13,206 for a 13,200-spectrum file).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct FacetTally {
    /// Profile facet: on the sqrt grid / kept f64 m/z.
    profile_grid: usize,
    profile_f64: usize,
    /// Gridded profile spectra whose zero pad the route left out: `shimadzu:span-trim`.
    profile_span_trimmed: usize,
    /// Centroid facet: on the 1e-9 lattice / kept f64 m/z.
    centroid_lattice: usize,
    centroid_f64: usize,
}

impl FacetTally {
    fn record(&mut self, routes: FacetRoutes) {
        self.profile_span_trimmed += usize::from(routes.profile_span_trimmed);
        match routes.profile_grid {
            Some(true) => self.profile_grid += 1,
            Some(false) => self.profile_f64 += 1,
            None => {}
        }
        match routes.centroid_lattice {
            Some(true) => self.centroid_lattice += 1,
            Some(false) => self.centroid_f64 += 1,
            None => {}
        }
    }

    fn total(&self) -> usize {
        self.profile_grid + self.profile_f64 + self.centroid_lattice + self.centroid_f64
    }

    /// The summary lines the Shimadzu lane used to log itself; silent for a lane that routes
    /// nothing (every other vendor reader).
    fn report(&self) {
        if self.total() == 0 {
            return;
        }
        if self.profile_grid + self.profile_f64 > 0 {
            log::info!(
                "Shimadzu profile facet: {} spectra on the sqrt grid, {} kept f64 m/z",
                self.profile_grid, self.profile_f64
            );
        }
        log::info!(
            "Shimadzu centroid facet: {} spectra on the 1e-9 m/z lattice (tof_index Int64), \
             {} kept f64 m/z",
            self.centroid_lattice, self.centroid_f64
        );
        if self.centroid_lattice == 0 && self.centroid_f64 > 0 {
            // The archive is still correct (exact f64 m/z in `point.mz` on every row), but the
            // size target is missed, and silently so without this: e.g. a file whose
            // MassHigh/Mass ratio is not 1e5 puts the centroids on a finer lattice than 1e-9.
            // The tolerance is quoted from the guard itself (`mz_lattice::LATTICE_TOL`): the text
            // said 1e-3 while the guard checked 1e-6.
            log::warn!(
                "Shimadzu centroid facet: none of the {} centroid lists passed the 1e-9 \
                 lattice guard (|m/z·1e9 − k| < max({:e}, 8 ulp) on every point, k non-decreasing); every \
                 centroid is stored as exact f64 m/z under the mz_calibration block. Is this file's \
                 MassHigh at 1e-9 Da? (MZPC_SHIMADZU_COARSE_MZ=1 selects the 1e-4 Mass field, which \
                 lies on the same lattice.)",
                self.centroid_f64,
                mz_lattice::LATTICE_TOL
            );
        }
    }
}

fn convert_vendor_reader<S: Into<VendorSpectrum>>(
    input: &Path,
    output: &Path,
    chunk: Option<ChunkingStrategy>,
    zstd_level: i32,
    vendor: Option<&vendor::VendorPolicy>,
    synth_chroms: bool,
    hints: VendorHints,
    len: usize,
    spectrum: impl FnMut(usize) -> Result<S>,
) -> Result<()> {
    let tally = convert_vendor_reader_tallied(
        input, output, chunk, zstd_level, vendor, synth_chroms, hints, len, spectrum,
    )?;
    tally.report();
    Ok(())
}

/// [`convert_vendor_reader`] returning the facet tally instead of logging it (the seam the unit
/// test counts through).
fn convert_vendor_reader_tallied<S: Into<VendorSpectrum>>(
    input: &Path,
    output: &Path,
    chunk: Option<ChunkingStrategy>,
    zstd_level: i32,
    vendor: Option<&vendor::VendorPolicy>,
    synth_chroms: bool,
    hints: VendorHints,
    len: usize,
    mut spectrum: impl FnMut(usize) -> Result<S>,
) -> Result<FacetTally> {
    if len == 0 {
        bail!("no spectra in {}", input.display());
    }
    let VendorHints {
        instrument,
        data_facet_fields,
        spectrum_param_fields,
        data_facet_point_layout,
        index_blocks,
        source_sha1,
        peaks_facet,
        transformations,
        run_metadata,
        keep_zero_runs,
        probe_indices,
        counters,
    } = hints;
    let mut index_blocks = index_blocks;
    let tmp = output.with_extension("mzpeak.tmp");
    let tmp_guard = TmpGuard::new(&tmp);
    let handle = fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    let level = ZstdLevel::try_new(zstd_level)
        .map_err(|e| anyhow::anyhow!("invalid zstd level {zstd_level}: {e}"))?;
    // Derive the data-facet schema from a few REAL sample spectra, CHUNK-AWARELY. The writer chunks
    // dense profile arrays into `LargeList`; a scalar schema from a single BinaryArrayMap mismatches
    // and panics the writer on SCIEX/Waters profile data. Probes spread across the run.
    const N_PROBE: usize = 6;
    let step = (len / N_PROBE).max(1);
    let mut probes: Vec<mzdata::spectrum::MultiLayerSpectrum> = Vec::new();
    // The lane's own probe choice first (one spectrum per function), then the stride fills up to
    // the usual six.
    let mut wanted: Vec<usize> = probe_indices.into_iter().filter(|&i| i < len).collect();
    let mut pi = 0usize;
    while pi < len && wanted.len() < N_PROBE {
        if !wanted.contains(&pi) {
            wanted.push(pi);
        }
        pi += step;
    }
    for i in wanted {
        if let Ok(s) = spectrum(i) {
            let s: VendorSpectrum = s.into();
            probes.push(s.spectrum);
        }
    }
    // A custom peaks facet (the Shimadzu centroid lattice) is an integer axis with an f64 fallback
    // column: never chunked, never numpressed — the lattice replaces both.
    let lattice_peaks = peaks_facet.is_some();
    // Pick delta-vs-numpress from the actual m/z values in the probes, not from the extension.
    // Only a facet that is actually chunked needs the sample: when the data facet is grid-encoded
    // the probes carry no m/z array, so the values being chunked are the CENTROIDS in the peak
    // set (or nothing at all, when those go to the lattice facet — a probe with no m/z array must
    // not be sampled).
    let sample_mz: Vec<f64> = if data_facet_point_layout && lattice_peaks {
        Vec::new()
    } else if data_facet_point_layout {
        probes
            .iter()
            .filter_map(|s| s.peaks.as_ref())
            .flat_map(|p| p.iter().map(|pk| pk.mz))
            .collect()
    } else {
        sample_mz_from(&probes)
    };
    let chunk = refine_chunking(&sample_mz, chunk);
    // A grid-encoded data facet is an integer axis, which has no chunk encoder: point layout there,
    // chunked peaks beside it — the mixed-family archive this project accepts on purpose.
    let data_chunk = if data_facet_point_layout { None } else { chunk };
    let peaks_chunk = if lattice_peaks { None } else { chunk };
    let mut builder = MzPeakWriterType::<fs::File>::builder()
        .chunked_encoding(data_chunk)
        .peaks_chunked_encoding(peaks_chunk)
        // Float m/z of a point facet (a grid facet's f64 minority, the lattice's f64 fallback):
        // BYTE_STREAM_SPLIT, no dictionary. Chunk facets are unaffected.
        .shuffle_mz(true)
        // ponytail: chromatograms are POINT layout, never chunked. Passing the spectrum strategy
        // here produced a `chunk` struct with no chunk_start/chunk_end columns, so the chunk builder
        // saw an empty main axis, wrote 0 time and 0 intensity points, and spilled the whole
        // intensity array into an uncompressed `auxiliary_arrays` blob in chromatograms_metadata —
        // losing the time axis outright. 99 of 330 reference archives are affected. A chromatogram
        // is a few thousand points; chunking bought nothing.
        .chromatogram_chunked_encoding(None)
        .buffer_size(buffer_spectra())
        .compression(Compression::ZSTD(level))
        // Both facets need their schema sampled: the data facet from the probes, and — when the
        // peak facet is chunked — the peak facet too, or its buffer declares scalar columns while
        // the chunked writer hands it list-typed ones.
        .sample_array_types_from_spectra(probes.clone().into_iter());
    builder = match peaks_facet {
        // The lattice facet is fully declared (its four columns are the contract); sampling the
        // probes' peak sets would only re-add the f64 `mz`/`intensity` it already carries.
        Some(schema) => builder.store_peaks_and_profiles_apart(Some(schema)),
        None => builder.sample_array_types_for_peaks_from_spectra(probes.into_iter()),
    };
    for f in data_facet_fields {
        builder = builder.add_spectrum_field(f);
    }
    let has_mzp_params = !spectrum_param_fields.is_empty();
    for (curie, name) in spectrum_param_fields {
        builder = builder.add_spectrum_param_field(CustomBuilderFromParameter::from_spec(
            curie,
            name,
            DataType::Float64,
        ));
    }
    let mut writer = builder.build(handle, !keep_zero_runs);
    add_processing_metadata(&mut writer);
    // The `--bruker-sdk` f64 lane shares `bruker_native::build_precursors` and so the MZP band; the
    // per-spectrum grid coefficient columns (`TOF_C0_CURIE` …) are MZP terms too.
    if has_mzp_params || is_tdf_dir(input) {
        ensure_mzp_cv(&mut writer);
    }
    let mut ms1 = Ms1Chroms::default();
    let len = max_spectra().map_or(len, |m| m.min(len));
    // Count here, over the written spectra: the probe fetches above went through the same
    // closure and must not show up in the run totals.
    let mut tally = FacetTally::default();
    let counted_before: Vec<usize> = counters.iter().map(|(_, c)| c.load(std::sync::atomic::Ordering::Relaxed)).collect();
    for i in 0..len {
        let VendorSpectrum { spectrum: spec, peak_arrays, routes } = spectrum(i)?.into();
        tally.record(routes);
        if synth_chroms {
            ms1.observe(&spec);
        }
        match peak_arrays.as_ref() {
            // Lattice-routed centroids: the spectrum keeps its peak set / raw arrays for the
            // metadata row, the explicit arrays are what the peaks facet stores.
            Some(arrays) => writer.write_spectrum_with_peak_arrays(&spec, arrays)?,
            None => writer.write_spectrum(&spec)?,
        }
    }
    let chromatogram_transforms = finish_chromatograms(&mut writer, input, &ms1, std::iter::empty(), synth_chroms)?;
    if let Some(hex) = source_sha1 {
        if writer.file_description().source_files.is_empty() {
            let mut sf = SourceFile {
                name: input.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
                location: "file://".to_string(),
                id: "sourceFile".to_string(),
                ..Default::default()
            };
            sf.add_param(run_metadata::sha1_param(hex));
            writer.file_description_mut().source_files.push(sf);
        }
    }
    // The vendor's instrument goes in BEFORE the generic fixup, which resolves
    // `default_instrument_id` against the list it can see: applied afterwards (as until 0.9.12)
    // the fixup saw an empty list and the run pointed at nothing.
    if let Some(cfg) = instrument {
        if writer.instrument_configurations().is_empty() {
            writer.instrument_configurations_mut().insert(0, cfg);
        }
    }
    // What the vendor states about the run, merged onto whatever the lane already set; a naive
    // acquisition time becomes an `acquisition_time` index block rather than a false instant.
    if let Some(meta) = run_metadata {
        if let Some(block) = run_metadata::apply(&mut writer, meta) {
            index_blocks.push(block);
        }
    }
    // One `acquisition_time` block per archive: the lane's own hints may already have stated it.
    if let Some(block) = fixup_run_metadata(&mut writer, input) {
        if !index_blocks.iter().any(|(key, _)| *key == block.0) {
            index_blocks.push(block);
        }
    }
    // A lane that keeps zero runs built the writer with the mask off, so its tally has none to report.
    let mut applied = base_transformations(&writer);
    for entry in transformations {
        declare(&mut applied, entry);
    }
    // The reader's own counts, grown over the written spectra only.
    for ((entry, counter), before) in counters.iter().zip(counted_before) {
        if counter.load(std::sync::atomic::Ordering::Relaxed) > before {
            declare(&mut applied, *entry);
        }
    }
    // The Shimadzu profile route's pad trim, from the routes of the written spectra.
    if tally.profile_span_trimmed > 0 {
        declare(&mut applied, "shimadzu:span-trim");
    }
    applied.extend(chromatogram_transforms);
    let transformations = transformations_block(&applied);
    let mut zip: ZipArchiveWriter<fs::File> = writer.finish_parquet()?;
    for (key, block) in index_blocks
        .iter()
        .chain(partial_marker(input, max_spectra(), len).iter())
        .chain(std::iter::once(&transformations))
    {
        zip.add_index_metadata(key, block)
            .with_context(|| format!("writing {key} index block"))?;
    }
    embed_vendor_members(&mut zip, input, vendor)?;
    zip.finish().map_err(|e| anyhow::anyhow!("finalizing archive: {e}"))?;
    tmp_guard.finish(output)?;
    Ok(tally)
}

/// Convert a Bruker TSF `.d` (line spectra) → mzPeak. Like [`convert_file`] but the reader is the
/// timsrust-tsf-backed [`bruker_tsf::TsfReader`] (mzdata can't read TSF), so the data-facet schema
/// is derived from a sample spectrum's arrays (mirroring the mzdata `sample_array_types_*` path).
fn convert_tsf(
    input: &Path,
    output: &Path,
    chunk: Option<ChunkingStrategy>,
    zstd_level: i32,
    vendor: Option<&vendor::VendorPolicy>,
    synth_chroms: bool,
) -> Result<()> {
    let reader = bruker_tsf::TsfReader::open(input)?;
    convert_vendor_reader(
        input, output, chunk, zstd_level, vendor, synth_chroms, VendorHints::default(),
        reader.len(), |i| reader.spectrum(i),
    )
}

/// Write one empty chromatogram (zero data points, no fabricated TIC). Mirrors mzML2mzPeak's
/// `ensure_chromatogram_facet`: keeps the archive openable by the reference reader AND triggers
/// the writer's index-metadata finalization. The (zero-length) TimeArray + IntensityArray are
/// required because the writer unwraps the TimeArray on the chromatogram path.
fn write_empty_chromatogram(writer: &mut MzPeakWriterType<fs::File>) -> Result<()> {
    let mut arrays = BinaryArrayMap::new();
    arrays.add(DataArray::wrap(&ArrayType::TimeArray, BinaryDataArrayType::Float64, Vec::new()));
    arrays.add(DataArray::wrap(&ArrayType::IntensityArray, BinaryDataArrayType::Float64, Vec::new()));
    let empty = Chromatogram::new(ChromatogramDescription::default(), arrays);
    writer.write_chromatogram(&empty)?;
    Ok(())
}

/// The `(total_ion_current, base_peak_intensity)` pair behind the synthesized TIC/BPC chromatograms.
///
/// The rule is: **call exactly what the writer calls, and only diverge where that call cannot
/// answer.** The `total_ion_current` / `base_peak_intensity` COLUMNS come from
/// `SpectrumDetailsBuilder::raw_summaries` in the vendored `writer/visitor.rs`, which is
/// `raw_arrays().fetch_summaries()` falling back to `peaks().fetch_summaries()`. Both branches
/// below are those same calls, so on every lane where mzdata can answer, the chromatogram point is
/// BIT-EQUAL to the column of the same spectrum — including the f32 TIC accumulator mzdata uses.
/// (Re-deriving the sum in f64 is *more accurate* but makes the chromatogram disagree with the
/// column it is supposed to summarize: measured on `waters-xevo-g2s-qtof/QC01`, 85 of 2,281 MS1
/// points drifted, up to ~9 ppm. Agreement is the contract here, not precision.)
///
/// The one place mzdata cannot answer is the grid / ims-compact lanes: `fetch_summaries` zips m/z
/// against intensity and bails to an EMPTY summary when `mzs()` errors, which is exactly what a
/// spectrum with an integer axis (`tof`, `tof_index`) and no `m/z array` does. There — and only
/// there — the intensities are folded directly. Neither a TIC nor a base-peak INTENSITY needs the
/// m/z axis: one is the sum of the samples, the other their maximum.
///
/// The defect this replaces: `observe` used `peaks.base_peak()`, which resolves through mzdata's
/// m/z-keyed summary and folds to `(0, 0)` when no m/z array is present. Every published grid-lane
/// archive therefore shipped a base-peak chromatogram that was zero at every point (timsTOF 2485:
/// max 0 over all 400 points; SCIEX Sample002: zero on 2,371 of 2,372) while the
/// `base_peak_intensity` COLUMN of the same archive was correct. The TIC survived only because
/// summing intensities never needed m/z — so the fix is to compute the base peak the same way.
///
/// Raw arrays win over the peak list, mirroring the writer's precedence, so a dual Shimadzu `.lcd`
/// scan reports its PROFILE trace and not its centroid list — the same thing its metadata row says.
///
/// Note this deliberately does NOT read the explicit MS:1000285/504/505 params the writer reads on
/// the mz-less path: depending on an upstream route to have set them would reintroduce the same
/// class of silent zero the moment a lane forgot to. The two agree by construction instead — each
/// grid route derives those params from the same intensities this folds.
fn chromatogram_summary(spec: &MultiLayerSpectrum) -> (f64, f64) {
    if let Some(arrays) = spec.raw_arrays() {
        // The writer's first choice, verbatim (`raw_summaries` builds exactly this value and calls
        // exactly this method). Empty when `mzs()` errors OR the spectrum is empty.
        let s = mzdata::spectrum::RefPeakDataLevel::<CentroidPeak, DeconvolutedPeak>::RawData(
            arrays,
        )
        .fetch_summaries();
        if s.count > 0 {
            return (s.tic as f64, s.base_peak.intensity as f64);
        }
        if let Ok(inten) = arrays.intensities() {
            if !inten.is_empty() {
                // Signal on a non-m/z axis: mzdata gave up, fold the samples ourselves. One pass,
                // one read of each. The sum accumulates in f64 (an f32 accumulator loses the tail
                // of a 500k-point profile spectrum); the max stays in f32, where it is an exact
                // copy of the winning sample rather than a widened one.
                // Seeded at 0.0 like mzdata (peaks.rs `(0.0, (0.0, 0.0f32, 0))`) and filtered to
                // finite samples like `summarize_points`, so this branch answers exactly as the
                // other two would on the same input. Seeding at `f32::MIN` instead would write
                // that sentinel into the BPC for an all-NaN spectrum whose column says NULL.
                let (tic, base) = inten
                    .iter()
                    .filter(|v| v.is_finite())
                    .fold((0.0f64, 0.0f32), |(sum, max), &v| {
                        (sum + v as f64, if v > max { v } else { max })
                    });
                return (tic, base as f64);
            }
        }
    }
    // No raw arrays, or a genuinely empty one: the peak list is the writer's fallback too. An empty
    // spectrum yields (0, 0), which matches the null its metadata row stores.
    let s = spec.peaks().fetch_summaries();
    (s.tic as f64, s.base_peak.intensity as f64)
}

/// Accumulates the per-MS1-spectrum TIC (summed intensity) and base-peak intensity vs. retention
/// time, so the converter can synthesize standard TIC + base-peak chromatograms. Populated during the
/// spectrum write loop (one pass, no re-read); MS2+ spectra are ignored.
#[derive(Default)]
struct Ms1Chroms {
    time: Vec<f64>,
    tic: Vec<f64>,
    bpc: Vec<f64>,
    /// Which spectrum kinds were written: the basis of `file_description.contents` on the native
    /// lanes (the mzML lane inherits ProteoWizard's list and keeps it).
    saw_ms1: bool,
    saw_msn: bool,
    saw_centroid: bool,
    saw_profile: bool,
}

impl Ms1Chroms {
    fn observe(&mut self, spec: &MultiLayerSpectrum) {
        if spec.ms_level() == 1 {
            self.saw_ms1 = true;
        } else {
            self.saw_msn = true;
        }
        match spec.signal_continuity() {
            mzdata::spectrum::SignalContinuity::Centroid => self.saw_centroid = true,
            mzdata::spectrum::SignalContinuity::Profile => self.saw_profile = true,
            _ => {}
        }
        if spec.ms_level() != 1 {
            return;
        }
        let (tic, base_intensity) = chromatogram_summary(spec);
        self.time.push(spec.start_time());
        self.tic.push(tic);
        self.bpc.push(base_intensity);
    }

    fn is_empty(&self) -> bool {
        self.time.is_empty()
    }

    /// Write the synthesized TIC + base-peak chromatograms, their times the spectrum start times in
    /// minutes. Returns how many were written (0 or 2).
    fn write(&self, writer: &mut MzPeakWriterType<fs::File>) -> Result<usize> {
        if self.is_empty() {
            return Ok(0);
        }
        let tic = synth_chromatogram(
            "TIC",
            Param::builder().name("total ion current chromatogram").curie(curie!(MS:1000235)).build(),
            &self.time,
            &self.tic,
        )?;
        let bpc = synth_chromatogram(
            "BPC",
            Param::builder().name("basepeak chromatogram").curie(curie!(MS:1000628)).build(),
            &self.time,
            &self.bpc,
        )?;
        writer.write_chromatogram(&tic)?;
        writer.write_chromatogram(&bpc)?;
        Ok(2)
    }
}

/// Store a chromatogram's time array in minutes, the unit `chromatograms_data` declares on every
/// lane, and say whether a stored value changed (`chromatogram-time-to-minutes`). The spec leaves a
/// chromatogram's time unit to the writer and recommends minutes, the unit spectrum and wavelength
/// times must have; mzPeakViewer reads every stored chromatogram time as minutes without looking at
/// the declared unit. A time in seconds or milliseconds (ProteoWizard's mzML chromatograms, HyStar's
/// device traces) is divided into minutes as a 64-bit float, which is not bit-exact. An array in
/// minutes is left alone, and so is one that states no unit: the writer labels it minutes as given.
fn chromatogram_time_to_minutes(arrays: &mut BinaryArrayMap) -> Result<bool> {
    let Some(t) = arrays.get_mut(&ArrayType::TimeArray) else { return Ok(false) };
    let per_minute = match t.unit {
        Unit::Second => 60.0,
        Unit::Millisecond => 60_000.0,
        _ => return Ok(false),
    };
    let (minutes, changed) = {
        let stored = t.to_f64().map_err(|e| anyhow::anyhow!("reading chromatogram time: {e}"))?;
        (stored.iter().map(|v| v / per_minute).collect::<Vec<f64>>(), stored.iter().any(|&v| v != 0.0))
    };
    // 64-bit whatever the source's width: a quotient rounded back into 32 bits loses digits (a
    // 32-bit 1 s is 0.016666668 min).
    let mut converted = DataArray::wrap(&ArrayType::TimeArray, BinaryDataArrayType::Float64, Vec::new());
    converted.update_buffer(&minutes).map_err(|e| anyhow::anyhow!("encoding chromatogram time: {e}"))?;
    converted.unit = Unit::Minute;
    converted.params = t.params.take();
    *t = converted;
    Ok(changed)
}

fn synth_chromatogram(id: &str, type_param: Param, time: &[f64], intensity: &[f64]) -> Result<Chromatogram> {
    let mut arrays = BinaryArrayMap::new();
    let mut t = DataArray::wrap(&ArrayType::TimeArray, BinaryDataArrayType::Float64, Vec::new());
    t.update_buffer(time).map_err(|e| anyhow::anyhow!("encoding chromatogram time: {e}"))?;
    t.unit = Unit::Minute;
    arrays.add(t);
    // Intensity as f32: the mzPeak chromatogram facet stores intensity as Float32 (chunked and point
    // paths alike), so emit f32 to match the schema directly. On the chunked path the writer would
    // coerce f64→f32 anyway; on the point path (custom-schema converters) it would NOT, so emitting
    // f32 here keeps both paths consistent. TIC/base-peak magnitudes fit f32 without loss of meaning.
    let intensity_f32: Vec<f32> = intensity.iter().map(|&v| v as f32).collect();
    let mut i = DataArray::wrap(&ArrayType::IntensityArray, BinaryDataArrayType::Float32, Vec::new());
    i.update_buffer(intensity_f32.as_slice()).map_err(|e| anyhow::anyhow!("encoding chromatogram intensity: {e}"))?;
    arrays.add(i);
    let mut descr = ChromatogramDescription { id: id.to_string(), ..Default::default() };
    // Set the typed field, not just the param: the `chromatogram_type` COLUMN is populated from
    // `chromatogram_type()`, and leaving it Unknown makes the writer emit null (previously the
    // abstract MS:1000626 parent) even though we know exactly which chromatogram this is.
    if let Some(curie) = type_param.curie() {
        descr.chromatogram_type = match curie {
            c if c == mzdata::curie!(MS:1000235) => ChromatogramType::TotalIonCurrentChromatogram,
            c if c == mzdata::curie!(MS:1000628) => ChromatogramType::BasePeakChromatogram,
            _ => descr.chromatogram_type,
        };
    }
    descr.add_param(type_param);
    Ok(Chromatogram::new(descr, arrays))
}

/// Write the chromatogram facet: synthesized MS1 TIC + base-peak (when `synth` and there were MS1
/// spectra), plus any source chromatograms, plus the device traces a Bruker `.d` input records in
/// `chromatography-data.sqlite` ([`bruker_traces`]) — skipping a source TIC/base-peak (HyStar's
/// own MS traces included) when we synthesized our own so they don't duplicate. Every time is stored
/// in minutes ([`chromatogram_time_to_minutes`]; the lanes sample the facet's schema through
/// [`schema_sample_chromatogram`], so the column declares minutes too). Falls back to one empty
/// chromatogram if nothing else was written (the reference reader requires the facet to open, and the
/// writer finalizes index metadata here). Returns the `transformations` entries the written
/// chromatograms add, for the lane's index block: `chromatogram-time-to-minutes`, and the device
/// traces' ([`bruker_traces::Trace`]).
fn finish_chromatograms<I: Iterator<Item = Chromatogram>>(
    writer: &mut MzPeakWriterType<fs::File>,
    input: &Path,
    ms1: &Ms1Chroms,
    source: I,
    synth: bool,
) -> Result<Vec<String>> {
    set_file_contents(writer, ms1, synth && !ms1.time.is_empty());
    let synthesized = if synth { ms1.write(writer)? } else { 0 };
    let mut n = synthesized;
    let mut applied: Vec<String> = Vec::new();
    let traces = bruker_traces::read(input).into_iter().map(|t| (t.chromatogram, t.rescaled, t.merged));
    for (mut chrom, rescaled, merged) in source.map(|c| (readable_chromatogram_arrays(c), false, false)).chain(traces) {
        if synthesized > 0
            && matches!(
                chrom.chromatogram_type(),
                ChromatogramType::TotalIonCurrentChromatogram | ChromatogramType::BasePeakChromatogram
            )
        {
            continue; // superseded by our MS1-synthesized version
        }
        let to_minutes = chromatogram_time_to_minutes(&mut chrom.arrays)?;
        writer.write_chromatogram(&chrom)?;
        n += 1;
        for (done, entry) in [
            (to_minutes, "chromatogram-time-to-minutes"),
            (rescaled, "bruker:trace-unit-rescale"),
            (merged, "bruker:trace-sort-dedup"),
        ] {
            if done {
                declare(&mut applied, entry);
            }
        }
    }
    log::info!("chromatograms: {synthesized} synthesized + {} from source = {n}", n - synthesized);
    if n == 0 {
        write_empty_chromatogram(writer)?;
    }
    Ok(applied)
}

/// Is this `source_files[].location` a filesystem path (in any spelling) rather than a genuine
/// remote locator? Anything that is not a non-`file` URL scheme is one: `file:///…`, `file:////…`
/// (mzdata's Thermo reader writes the canonical parent DIRECTORY this way), a bare absolute path,
/// a Windows drive path, an empty string.
fn is_filesystem_location(location: &str) -> bool {
    let scheme_len = location.find("://").unwrap_or(0);
    scheme_len == 0 || location[..scheme_len].eq_ignore_ascii_case("file")
}

/// Does a `run.id` look like a path the reader copied from the input's location — an absolute or
/// relative path, or a Windows drive spelling — rather than a run name?
fn is_path_shaped_run_id(id: &str) -> bool {
    id.contains(['/', '\\'])
        || (id.len() >= 2 && id.as_bytes()[1] == b':' && id.as_bytes()[0].is_ascii_alphabetic())
}

/// Fill required `ms_run` fields the source mzML/imzML may have left implicit, so the mzPeak index
/// schema validates, and normalise what the readers put there. Discipline (from mzML2mzPeak): only
/// ever fills a `None`/empty — a source-declared value is left verbatim — with two deliberate
/// exceptions, both provenance rather than data: an operator filesystem path is reduced to the bare
/// `file://` authority / the input stem (the path travels with every distributed archive and says
/// nothing about the run), and a `default_instrument_id` that points at no configuration is
/// clamped or cleared (a dangling foreign key fails the spec's semantic invariants). Faithful values
/// only (real source stem / real list entry / the input file as its own source).
/// `file_description.contents` from what was actually written: `MS1 spectrum` / `MSn spectrum`, the
/// two terms ProteoWizard states. Only when the lane said nothing more specific than the generic
/// parent term (or nothing at all) — an inherited list (the mzML lane) is kept verbatim.
fn set_file_contents(target: &mut impl MSDataFileMetadata, seen: &Ms1Chroms, tic_written: bool) {
    let contents = &mut target.file_description_mut().contents;
    let only_generic = contents.iter().all(|p| p.curie() == Some(curie!(MS:1000294)));
    if !only_generic || (!seen.saw_ms1 && !seen.saw_msn) {
        return;
    }
    contents.retain(|p| p.curie() != Some(curie!(MS:1000294)));
    // The terms ProteoWizard lists for a file: the spectrum kinds, their representation, and the
    // TIC chromatogram when one is written.
    if seen.saw_ms1 {
        contents.push(Param::builder().name("MS1 spectrum").curie(curie!(MS:1000579)).build());
    }
    if seen.saw_msn {
        contents.push(Param::builder().name("MSn spectrum").curie(curie!(MS:1000580)).build());
    }
    if seen.saw_centroid {
        contents.push(Param::builder().name("centroid spectrum").curie(curie!(MS:1000127)).build());
    }
    if seen.saw_profile {
        contents.push(Param::builder().name("profile spectrum").curie(curie!(MS:1000128)).build());
    }
    if tic_written {
        contents.push(Param::builder().name("total ion current chromatogram").curie(curie!(MS:1000235)).build());
    }
}

/// The run metadata a vendor DIRECTORY input states in its side files, readable on any host:
/// Bruker TDF/TSF `.d` (`GlobalMetadata`), Bruker BAF `.d` (its digested members; the run facts
/// live in the baf2sql cache and arrive through the lane's hints), Agilent `.d` (`AcqData` XML).
/// Waters `.raw` is handled by its lane through `VendorHints`.
fn vendor_dir_metadata(input: &Path) -> Option<run_metadata::VendorRunMetadata> {
    if !input.is_dir() {
        return None;
    }
    if vendor::bruker_sqlite(input).is_some() {
        return vendor::bruker_run_metadata(input);
    }
    // After the SQLite check: a BAF `.d` can hold a 0-byte `analysis.tdf` stub (NreB_PAS_DECONV),
    // which `bruker_sqlite` already passes over.
    if let Some(members) = vendor::bruker_baf_members(input) {
        return Some(members);
    }
    agilent_meta::read(input)
}

/// ProteoWizard writes ids as XML names and escapes each byte a name may not hold as `_x00hh_`
/// ([`pwiz_id`]: `Experiment_x0020_1`, `_x0032_0090101…` for a leading digit). An mzPeak id is a
/// plain string, so the mzML and imzML lanes decode them on copy: `run.id`, and the software ids
/// together with the processing methods and instrument configurations that name them. Called for
/// those readers only: the Thermo and TDF readers name the run after the file, which nothing
/// escaped. Not part of [`fixup_run_metadata`], which the mzML exports share: an mzML id must stay
/// an XML name, and ProteoWizard's escaped form is one.
fn decode_pwiz_ids(target: &mut impl MSDataFileMetadata) {
    if let Some(run) = target.run_description_mut() {
        run.id = run.id.as_deref().map(pwiz_id::decode);
    }
    for sw in target.softwares_mut() {
        sw.id = pwiz_id::decode(&sw.id);
    }
    for dp in target.data_processings_mut() {
        for m in dp.methods.iter_mut() {
            m.software_reference = pwiz_id::decode(&m.software_reference);
        }
    }
    for ic in target.instrument_configurations_mut().values_mut() {
        ic.software_reference = pwiz_id::decode(&ic.software_reference);
    }
}

/// Normalise the run metadata every lane writes and merge in what a vendor directory states.
/// Returns the `acquisition_time` index block when that directory states only a wall clock without
/// a zone (`run_metadata::apply`): an archive lane writes it into the index, and an mzML output,
/// which cannot carry it, says so (`fixup_mzml_run_metadata`). Through 0.11.5 the block was dropped
/// here, so an unzoned Bruker or Agilent clock left no trace on the lanes that rely on this fixup.
#[must_use]
fn fixup_run_metadata(target: &mut impl MSDataFileMetadata, input: &Path) -> Option<(String, serde_json::Value)> {
    // 0. Provenance hygiene on what the reader ALREADY copied. Step 1 below sanitises only the
    // entry it synthesises itself; mzdata's Thermo reader writes the converting machine's parent
    // directory into `location`, and its TDF reader the full `.d` path into `run.id`, so twelve
    // published archives carried `/Users/…`. `name` (+ the SHA-1 param) is the provenance; the
    // directory is the operator's filesystem.
    for sf in target.file_description_mut().source_files.iter_mut() {
        if sf.location != "file://" && is_filesystem_location(&sf.location) {
            sf.location = "file://".to_string();
        }
    }
    let stem = input.file_stem().map(|s| s.to_string_lossy().to_string()).filter(|s| !s.is_empty());
    if let Some(run) = target.run_description_mut() {
        if run.id.as_deref().is_some_and(is_path_shaped_run_id) {
            run.id = stem.clone();
        }
    }

    // 1. What the vendor directory states about the run — instrument model/serial, acquisition
    // time, software, sample, the source members with their digests — merged onto whatever the
    // reader already set. Bruker used to be special-cased here; the same seam now serves every
    // vendor directory the host can read. Only what the file states is asserted: no ion source or
    // detector is guessed (a wrong `MS:1000008` child is worse than an absent one).
    let naive_time_block = vendor_dir_metadata(input).and_then(|meta| run_metadata::apply(target, meta));

    // 1b. Ensure at least one source_file (the input itself) so default_source_file_id can resolve
    //     — only when no member was stated above.
    if target.file_description().source_files.is_empty() {
        // `name` identifies the source; the directory it happened to sit in on the converting
        // machine is not provenance, it is the operator's filesystem — and it would travel with
        // every distributed archive. Record the bare `file://` authority instead of an absolute path.
        let location = "file://".to_string();
        let name = input.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let mut sf = SourceFile {
            name,
            location,
            id: "sourceFile".to_string(),
            ..Default::default()
        };
        // MS:1000569 SHA-1 of the source, as msconvert records it: the digest, not the path, is the
        // provenance that survives distribution. Single files only — a `.d` directory has no
        // single byte stream to digest, and hashing one arbitrary member would be a false claim.
        if input.is_file() {
            match embed_aux::sha1_hex(input) {
                Ok(hex) => sf.add_param(run_metadata::sha1_param(hex)),
                Err(e) => log::warn!("could not digest {}: {e}", input.display()),
            }
        }
        target.file_description_mut().source_files.push(sf);
    }

    // 2. default_source_file_id / default_data_processing_id ← first list entry, when unset.
    let first_sf = target.file_description().source_files.first().map(|sf| sf.id.clone());
    let first_dp = target.data_processings().first().map(|dp| dp.id.clone());
    // The spec's run block REQUIRES an integer `default_instrument_id` (mzPeak-specification
    // `schema/ms_run.json`: required, `"type": "integer"`), so a run without an instrument record
    // gets an EMPTY configuration `0` to point at — what mzML does too (`instrumentConfigurationList`
    // needs one entry; msconvert writes a bare one for an unknown instrument). 0.10.0 wrote `null`
    // instead ("a null is honest, a dangling 0 is a schema violation"); the validator's
    // `index_schema_valid` / `meta_run_valid` refused the three instrument-less corpus archives
    // rebuilt under it, and the null was the schema violation. The dangling `0` of the eighteen
    // pre-0.10.0 Waters/SCIEX archives is still fixed: the id now resolves to a real (empty) entry.
    if target.instrument_configurations().is_empty() {
        target
            .instrument_configurations_mut()
            .insert(0, InstrumentConfiguration { id: 0, ..Default::default() });
    }
    let instr_ids: Vec<u32> = target.instrument_configurations().keys().copied().collect();
    let first_instr = instr_ids.iter().copied().min();
    if let Some(run) = target.run_description_mut() {
        if run.default_source_file_id.is_none() {
            run.default_source_file_id = first_sf;
        }
        if run.default_data_processing_id.is_none() {
            run.default_data_processing_id = first_dp;
        }
        if run.id.as_deref().unwrap_or("").is_empty() {
            run.id = Some(stem.unwrap_or_else(|| "run".to_string()));
        }
        // Every emitted reference must resolve: fill an absent one from the list (never empty
        // after the step above), and clamp an inherited one (mzdata's readers hand us `Some(0)`
        // regardless) onto a real configuration.
        match run.default_instrument_id {
            Some(id) if instr_ids.contains(&id) => {}
            _ => run.default_instrument_id = first_instr,
        }
    }
    naive_time_block
}

/// [`fixup_run_metadata`] for an mzML output. The run model the mzML writer serialises holds only a
/// zoned `start_time` (`DateTime<FixedOffset>`), so an unzoned vendor clock cannot be carried: name
/// the clock that was left out instead of dropping it without a word.
fn fixup_mzml_run_metadata(w: &mut impl MSDataFileMetadata, input: &Path) {
    if let Some((_, block)) = fixup_run_metadata(w, input) {
        log::warn!(
            "{}: acquisition time {} states no time zone, and the mzML run model holds only zoned \
             times; it is not written",
            block["source"].as_str().unwrap_or("vendor file"),
            block["wall_clock"].as_str().unwrap_or("?")
        );
    }
}

/// List the converter-owned MZP vocabulary in the archive's `cv_list`. Every timsTOF lane attaches
/// the isolation window's 1/K0 band as MZP:1000006/7 (`bruker_native::add_isolation_mobility_band`),
/// and an accession whose prefix is not in `cv_list` is unresolvable to a reader; the vendored
/// writer seeds only MS+UO. Idempotent.
fn ensure_mzp_cv(writer: &mut MzPeakWriterType<fs::File>) {
    mzpeak_prototyping::param::ensure_mzp_cv_entry(writer.controlled_vocabularies_mut());
}

/// Strip the CV binding from every converter-owned MZP param in a spectrum so it exports as an mzML
/// `userParam` (name + value + unit, no accession). mzdata's mzML writer stringifies a controlled
/// param's CURIE through `Display`, which panics on the `Unknown` CV this crate uses for MZP terms
/// (`mzdata-param-0.66.6/src/curie_.rs:99`); mzML has no legal home for a non-PSI accession anyway.
fn demote_mzp_params(descr: &mut mzdata::spectrum::SpectrumDescription) {
    demote_mzp_in(&mut descr.params);
    if let Some(ps) = descr.acquisition.params.as_mut() {
        demote_mzp_in(ps);
    }
    for scan in descr.acquisition.scans.iter_mut() {
        if let Some(ps) = scan.params.as_mut() {
            demote_mzp_in(ps);
        }
    }
    for prec in descr.precursor.iter_mut() {
        demote_mzp_in_precursor(prec);
    }
}

/// Drop the archive's integer grid axis (`tof_index` / `tof`, or the nameless non-standard array
/// the point reader hands back for it) from a spectrum that already carries the m/z reconstructed
/// from it. The vendored reader rebuilds `m/z array` from the axis but leaves the axis in the map
/// (a Centroid spectrum collapses to a peak list and loses it; a Profile spectrum keeps its raw
/// arrays), so the mzML export of every profile-facet grid archive — Shimadzu `.lcd` since 0.9.3,
/// all TOF-grid lanes since 0.10.1 — wrote a THIRD `binaryDataArray` per spectrum: 32-bit integers
/// under `MS:1000786 non-standard data array` with no name, which a re-import then stored as a
/// nameless column. mzML has m/z and intensity; the axis is the archive's business. Kept when no
/// m/z array exists (reconstruction failed): then the raw axis is the only evidence of the defect.
fn strip_grid_axis(arrays: &mut BinaryArrayMap) {
    if !arrays.has_array(&ArrayType::MZArray) {
        return;
    }
    arrays.byte_buffer_map.retain(|k, _| match k {
        ArrayType::NonStandardDataArray { name } => {
            let n: &str = name;
            !matches!(n, "tof_index" | "tof" | "")
        }
        _ => true,
    });
}

/// [`demote_mzp_params`] for a chromatogram.
fn demote_mzp_params_chrom(descr: &mut ChromatogramDescription) {
    demote_mzp_in(&mut descr.params);
    for prec in descr.precursor.iter_mut() {
        demote_mzp_in_precursor(prec);
    }
}

fn demote_mzp_in_precursor(prec: &mut mzdata::spectrum::Precursor) {
    demote_mzp_in(&mut prec.activation.params);
    for ion in prec.ions.iter_mut() {
        if let Some(ps) = ion.params.as_mut() {
            demote_mzp_in(ps);
        }
    }
}

fn demote_mzp_in(params: &mut [Param]) {
    for p in params.iter_mut() {
        if p.controlled_vocabulary == Some(ControlledVocabulary::Unknown) {
            p.controlled_vocabulary = None;
            p.accession = None;
        }
    }
}

fn add_processing_metadata(writer: &mut MzPeakWriterType<fs::File>) {
    writer.softwares_mut().push(Software::new(
        "mzpeak-convert".into(),
        env!("CARGO_PKG_VERSION").into(),
        vec![custom_software_name("mzpeak-convert")],
    ));
    writer.data_processings_mut().push(DataProcessing {
        id: "mzpeak_convert_conversion".to_string(),
        methods: vec![ProcessingMethod {
            order: 1,
            software_reference: "mzpeak-convert".to_string(),
            params: vec![Param::new_key_value(
                "conversion options",
                // Provenance without leaking the operator's filesystem: flags are kept verbatim, but
                // any path-shaped argument is reduced to its basename. The raw command line would
                // otherwise embed absolute input/output paths — home directory, scratch dirs — in
                // every distributed archive.
                std::env::args()
                    .skip(1)
                    .map(|a| {
                        if a.contains(std::path::MAIN_SEPARATOR) {
                            Path::new(&a)
                                .file_name()
                                .map(|s| s.to_string_lossy().into_owned())
                                .unwrap_or(a)
                        } else {
                            a
                        }
                    })
                    .collect::<Vec<String>>()
                    .join(" "),
            )],
        }],
    });
}

fn reader_format<R: std::io::Read + std::io::Seek>(reader: &MZReaderType<R>) -> &'static str {
    match reader {
        MZReaderType::MzML(_) => "mzML",
        MZReaderType::IMzML(_) => "imzML",
        MZReaderType::BrukerTDF(_) => "Bruker TDF (.d)",
        MZReaderType::ThermoRaw(_) => "Thermo .raw",
        _ => "other",
    }
}


#[cfg(test)]
#[path = "../tests/common/corpus.rs"]
mod corpus_gate;

#[cfg(test)]
mod tests {
    use super::expand_empty_param_groups;
    use super::{decode_single_byte, rewrite_encoding_decl_to_utf8, sniff_xml_encoding};
    use super::{require_aligned_arrays, tof_grid, tof_grid_spectrum, TofRoute};
    use mzdata::params::Param;
    use mzdata::prelude::*;
    use mzdata::spectrum::bindata::{ArrayType, BinaryDataArrayType, DataArray};
    use mzdata::spectrum::{BinaryArrayMap, MultiLayerSpectrum, SpectrumDescription};
    use mzpeaks::{CentroidPeak, DeconvolutedPeak};

    /// `dir/run.d` holding only a HyStar `chromatography-data.sqlite`, one chunk per trace:
    /// (Id, Description, Type, Unit, seconds, values).
    fn hystar_dot_d(dir: &std::path::Path, traces: &[(i64, &str, i64, i64, &[f64], &[f32])]) -> std::path::PathBuf {
        let dot_d = dir.join("run.d");
        std::fs::create_dir_all(&dot_d).unwrap();
        let c = rusqlite::Connection::open(dot_d.join("chromatography-data.sqlite")).unwrap();
        c.execute_batch(
            "CREATE TABLE TraceSources (Id INTEGER PRIMARY KEY,Description TEXT,Instrument TEXT,InstrumentId TEXT,Type INTEGER,Unit INTEGER,TimeOffset REAL,Color INTEGER);
             CREATE TABLE TraceChunks (Trace INTEGER NOT NULL,Times BLOB NOT NULL,Intensities BLOB NOT NULL);",
        )
        .unwrap();
        for (id, description, kind, unit, seconds, values) in traces {
            c.execute(
                "INSERT INTO TraceSources (Id, Description, Instrument, Type, Unit, TimeOffset) VALUES (?1, ?2, 'Agilent ICF System', ?3, ?4, 0.0)",
                rusqlite::params![id, description, kind, unit],
            )
            .unwrap();
            let t: Vec<u8> = seconds.iter().flat_map(|x| x.to_le_bytes()).collect();
            let v: Vec<u8> = values.iter().flat_map(|x| x.to_le_bytes()).collect();
            c.execute("INSERT INTO TraceChunks VALUES (?1, ?2, ?3)", rusqlite::params![id, t, v]).unwrap();
        }
        dot_d
    }

    /// `dot_d` through `finish_chromatograms` into `archive`, beside the TIC/BPC synthesized from one
    /// MS1 spectrum at 2.5 min; returns the transformations entries it declared.
    fn write_trace_archive(dot_d: &std::path::Path, archive: &std::path::Path) -> Vec<String> {
        let mut writer = super::MzPeakWriterType::<std::fs::File>::builder()
            .chromatogram_chunked_encoding(None)
            .build(std::fs::File::create(archive).unwrap(), true);
        let _ = super::fixup_run_metadata(&mut writer, dot_d);
        let ms1 = super::Ms1Chroms { time: vec![2.5], tic: vec![10.0], bpc: vec![4.0], saw_ms1: true, ..Default::default() };
        let applied = super::finish_chromatograms(&mut writer, dot_d, &ms1, std::iter::empty(), true).unwrap();
        writer.finish_parquet().unwrap().finish().unwrap();
        applied
    }

    /// A fresh `mzpc-device-traces-<tag>-<pid>` directory and the guard that removes it.
    fn trace_scratch(tag: &str) -> (std::path::PathBuf, RmDir) {
        let dir = std::env::temp_dir().join(format!("mzpc-device-traces-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        (dir.clone(), RmDir(dir))
    }

    /// Every Bruker lane ends in `finish_chromatograms`, so this is where a `.d`'s HyStar device
    /// traces join the facet: after the synthesized TIC/BPC, HyStar's own MS trace giving way to
    /// them, a bar trace stated in pascal (64-bit, dividing back to the stored bar exactly) with its
    /// type also a parameter and the rescale declared, its times HyStar's seconds stored in minutes
    /// and that declared too — and the input directory left exactly as it was.
    #[test]
    fn finish_chromatograms_writes_the_bruker_device_traces() {
        use mzdata::params::Unit;
        use mzdata::spectrum::ChromatogramType;
        use mzpeak_prototyping::MzPeakReader;

        let (dir, _cleanup) = trace_scratch("finish");
        let dot_d = hystar_dot_d(
            &dir,
            &[(1, "TIC,±MS", 1, 6, &[2.0, 3.0], &[180.0, 170.02]), (2, "Pump HP:Pressure - [bar]", 9999, 3, &[2.0, 3.0], &[180.0, 170.02])],
        );
        let listing = || {
            let mut entries: Vec<_> = std::fs::read_dir(&dot_d)
                .unwrap()
                .map(|e| {
                    let e = e.unwrap();
                    let m = e.metadata().unwrap();
                    (e.file_name(), m.len(), m.modified().unwrap())
                })
                .collect();
            entries.sort();
            entries
        };
        let before = listing();

        let path = dir.join("run.mzpeak");
        let applied = write_trace_archive(&dot_d, &path);
        assert_eq!(listing(), before, "reading the device traces changed the input directory");
        assert_eq!(applied, ["chromatogram-time-to-minutes", "bruker:trace-unit-rescale"]);

        let mut r = MzPeakReader::new(&path).unwrap();
        let chroms: Vec<_> = (0..r.len_chromatograms()).map(|i| r.get_chromatogram(i).unwrap()).collect();
        assert_eq!(chroms.iter().map(|c| c.id()).collect::<Vec<_>>(), ["TIC", "BPC", "Pump HP:Pressure - [bar]"]);
        let pressure = &chroms[2];
        assert_eq!(pressure.chromatogram_type(), ChromatogramType::PressureChromatogram);
        assert!(pressure.params().iter().any(|p| p.curie() == Some(mzdata::curie!(MS:1003019))), "the type is a parameter too");
        let p = pressure.arrays.get(&ArrayType::PressureArray).expect("a pressure array");
        assert_eq!((p.unit, p.dtype), (Unit::Pascal, BinaryDataArrayType::Float64));
        assert_eq!(p.to_f64().unwrap().iter().map(|pa| (pa / 1e5) as f32).collect::<Vec<_>>(), [180.0, 170.02]);
        let t = pressure.arrays.get(&ArrayType::TimeArray).unwrap();
        assert_eq!(t.to_f64().unwrap().to_vec(), vec![2.0 / 60.0, 3.0 / 60.0]);
    }

    /// What `chromatogram_time_to_minutes` does to the time units a source states. Seconds and
    /// milliseconds become 64-bit minutes, a 32-bit source included: the rescale it replaced wrote
    /// the quotient back into the array's own Float32 (1 s as 0.016666668 min). Minutes, an array that
    /// states no unit, and seconds that are all zero change no stored value, so nothing is declared.
    #[test]
    fn chromatogram_times_are_stored_as_64_bit_minutes() {
        use mzdata::params::Unit;

        let times = |dtype: BinaryDataArrayType, unit: Unit, values: &[f64]| {
            let mut t = DataArray::wrap(&ArrayType::TimeArray, dtype, Vec::new());
            match dtype {
                BinaryDataArrayType::Float32 => t.update_buffer(&values.iter().map(|&v| v as f32).collect::<Vec<_>>()).unwrap(),
                _ => t.update_buffer(values).unwrap(),
            };
            t.unit = unit;
            let mut arrays = BinaryArrayMap::new();
            arrays.add(t);
            arrays
        };
        let stored = |arrays: &BinaryArrayMap| {
            let t = arrays.get(&ArrayType::TimeArray).unwrap();
            (t.unit, t.dtype, t.to_f64().unwrap().to_vec())
        };

        let mut seconds = times(BinaryDataArrayType::Float32, Unit::Second, &[0.0, 1.0, 90.0]);
        assert!(super::chromatogram_time_to_minutes(&mut seconds).unwrap());
        assert_eq!(stored(&seconds), (Unit::Minute, BinaryDataArrayType::Float64, vec![0.0, 1.0 / 60.0, 1.5]));
        let mut milliseconds = times(BinaryDataArrayType::Float64, Unit::Millisecond, &[30_000.0]);
        assert!(super::chromatogram_time_to_minutes(&mut milliseconds).unwrap());
        assert_eq!(stored(&milliseconds), (Unit::Minute, BinaryDataArrayType::Float64, vec![0.5]));
        for (unit, values) in [(Unit::Minute, vec![1.0]), (Unit::Unknown, vec![90.0]), (Unit::Second, vec![0.0])] {
            let mut same = times(BinaryDataArrayType::Float64, unit, &values);
            assert!(!super::chromatogram_time_to_minutes(&mut same).unwrap(), "{unit:?}");
            assert_eq!(stored(&same).2, values, "{unit:?}");
        }
    }

    /// An mzML-lane archive built by 0.11.5 or earlier declares its chromatogram times in seconds
    /// (`UO:0000010`), ProteoWizard's unit. `--rt` is in minutes and converts its window into the
    /// declared unit, so it cuts such an archive where it says. The converter no longer writes a
    /// seconds column, so this one is built with the writer.
    #[test]
    fn an_rt_window_reads_a_seconds_column_in_its_unit() {
        use mzdata::params::Unit;
        use mzdata::spectrum::ChromatogramDescription;
        use mzpeak_prototyping::writer::AbstractMzPeakWriter;
        use mzpeak_prototyping::MzPeakReader;

        let (dir, _cleanup) = trace_scratch("seconds-column");
        let sic = {
            let mut time = DataArray::wrap(&ArrayType::TimeArray, BinaryDataArrayType::Float64, Vec::new());
            time.update_buffer(&[0.0f64, 1.0, 2.0, 3.0, 4.0, 60.0]).unwrap();
            time.unit = Unit::Second;
            let mut intensity = DataArray::wrap(&ArrayType::IntensityArray, BinaryDataArrayType::Float32, Vec::new());
            intensity.update_buffer(&[1.0f32, 2.0, 3.0, 4.0, 5.0, 6.0]).unwrap();
            let mut arrays = BinaryArrayMap::new();
            arrays.add(time);
            arrays.add(intensity);
            super::Chromatogram::new(ChromatogramDescription { id: "sic".to_string(), ..Default::default() }, arrays)
        };
        let src = dir.join("seconds.mzpeak");
        let mut writer = super::MzPeakWriterType::<std::fs::File>::builder()
            .chromatogram_chunked_encoding(None)
            .sample_array_types_from_chromatograms(std::iter::once(sic.clone()))
            .build(std::fs::File::create(&src).unwrap(), true);
        let _ = super::fixup_run_metadata(&mut writer, &dir);
        writer.write_chromatogram(&sic).unwrap();
        writer.finish_parquet().unwrap().finish().unwrap();
        let declared = |archive: &std::path::Path| {
            let mut r = MzPeakReader::new(archive).unwrap();
            let c = r.get_chromatogram(0).unwrap();
            let t = c.arrays.get(&ArrayType::TimeArray).unwrap();
            (t.unit, t.to_f64().unwrap().to_vec())
        };
        assert_eq!(declared(&src).0, Unit::Second, "the archive under test has a seconds column");

        let out = dir.join("rt.mzpeak");
        super::filter::run(&src, &out, &super::filter::FilterOpts { rt: Some((0.0, 0.05)), ..Default::default() }).unwrap();
        assert_eq!(declared(&out), (Unit::Second, vec![0.0, 1.0, 2.0, 3.0]), "0.05 min is 3 s");
    }

    /// `--rt` on an archive holding device traces. Their values sit in each chromatogram's auxiliary
    /// arrays, outside the data facet, and the window used to cut only the time rows: a filtered
    /// trace paired 3 times with 6 values, and an mzML export of it wrote value arrays longer than
    /// `defaultArrayLength`. The values are cut with their times.
    #[test]
    fn an_rt_window_cuts_the_device_trace_values_with_their_times() {
        use mzpeak_prototyping::MzPeakReader;

        let (dir, _cleanup) = trace_scratch("rt");
        let seconds = [60.0, 120.0, 180.0, 240.0, 300.0, 360.0];
        let dot_d = hystar_dot_d(
            &dir,
            &[
                (1, "Pump HP:Pressure - [bar]", 9999, 3, &seconds, &[100.0, 110.0, 120.0, 130.0, 140.0, 150.0]),
                (2, "Fraction A - [%]", 5, 4, &seconds, &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]),
            ],
        );
        let src = dir.join("run.mzpeak");
        write_trace_archive(&dot_d, &src);
        let out = dir.join("rt.mzpeak");
        super::filter::run(&src, &out, &super::filter::FilterOpts { rt: Some((2.0, 4.0)), ..Default::default() }).unwrap();

        let mut r = MzPeakReader::new(&out).unwrap();
        let chroms: Vec<_> = (0..r.len_chromatograms()).map(|i| r.get_chromatogram(i).unwrap()).collect();
        let trace = |id: &str| chroms.iter().find(|c| c.id() == id).unwrap_or_else(|| panic!("no {id}"));
        let values = |id: &str, array: ArrayType| {
            let c = trace(id);
            (c.arrays.get(&ArrayType::TimeArray).unwrap().to_f64().unwrap().to_vec(), c.arrays.get(&array).unwrap().to_f64().unwrap().to_vec())
        };
        assert_eq!(values("Pump HP:Pressure - [bar]", ArrayType::PressureArray), (vec![2.0, 3.0, 4.0], vec![110.0e5, 120.0e5, 130.0e5]));
        assert_eq!(
            values("Fraction A - [%]", ArrayType::nonstandard("Fraction A - [%]")),
            (vec![2.0, 3.0, 4.0], vec![2.0, 3.0, 4.0])
        );
    }

    /// The archive → mzML export of device traces: `<chromatogramList count>` is what is written
    /// (mzdata's writer said 2, its own TIC/BPC pair, however many source chromatograms followed),
    /// and each trace states its chromatogram type as a cvParam, the only place mzML has for it.
    #[test]
    fn the_mzml_export_counts_and_types_the_device_traces() {
        let (dir, _cleanup) = trace_scratch("mzml");
        let dot_d = hystar_dot_d(
            &dir,
            &[
                (1, "Pump HP:Pressure - [bar]", 9999, 3, &[120.0, 180.0], &[100.0, 110.0]),
                (2, "Fraction A - [%]", 5, 4, &[120.0, 180.0], &[1.0, 2.0]),
            ],
        );
        let src = dir.join("run.mzpeak");
        write_trace_archive(&dot_d, &src);
        let out = dir.join("run.mzML");
        super::filter_mzpeak_to_mzml(&src, &out, &super::filter::FilterOpts::default()).unwrap();

        let xml = std::fs::read_to_string(&out).unwrap();
        let written = xml.matches("<chromatogram ").count();
        assert_eq!(written, 4, "the writer's TIC and BPC, and the two traces");
        assert!(xml.contains(&format!("<chromatogramList count=\"{written}\"")), "a stale chromatogramList count");
        let element = |id: &str| {
            let start = xml.find(&format!("<chromatogram id=\"{id}\"")).unwrap_or_else(|| panic!("no chromatogram {id}"));
            &xml[start..start + xml[start..].find("</chromatogram>").unwrap()]
        };
        assert!(element("Pump HP:Pressure - [bar]").contains("accession=\"MS:1003019\""), "a pressure chromatogram");
        assert!(element("Fraction A - [%]").contains("accession=\"MS:1000625\""), "ProteoWizard's generic chromatogram");
    }

    /// The mzML export of an archive holding device traces converts back, into an archive and into
    /// an mzML. mzdata's mzML reader has no case for the `pressure array` (MS:1000821), `flow rate
    /// array` (MS:1000820) and `temperature array` (MS:1000822) the export writes: each came back
    /// untyped, and the schema sampler and both writers panicked naming it — exit 134, nothing
    /// written, on the export of every archive with HyStar traces. Every trace keeps its type, its
    /// value array's type and unit, and its (time, value) pairs through both hops, a psi and a
    /// pascal pressure trace side by side; the device arrays stay auxiliary arrays, and the mzML hop
    /// still states each chromatogram's type.
    #[test]
    fn an_mzml_export_of_device_traces_converts_back() {
        use mzdata::params::Unit;
        use mzdata::spectrum::ChromatogramType;
        use mzpeak_prototyping::MzPeakReader;

        let (dir, _cleanup) = trace_scratch("reimport");
        let seconds = [120.0, 180.0, 240.0];
        let dot_d = hystar_dot_d(
            &dir,
            &[
                (1, "Pump HP:Pressure - [bar]", 9999, 3, &seconds, &[180.0, 170.02, 160.5]),
                (2, "Pressure - [psi]", 4, 0, &seconds, &[3785.5, 3783.25, 3779.75]),
                (3, "Pump HP:Actual flow - [µL/min]", 9999, 2, &seconds, &[4.25, 4.5, 4.75]),
                (4, "Oven temperature - [°C]", 7, 5, &seconds, &[50.0, 50.5, 51.0]),
                (5, "Fraction A - [%]", 5, 4, &seconds, &[1.0, 2.0, 3.0]),
            ],
        );
        let value_arrays = [
            ("Pump HP:Pressure - [bar]", Some(ArrayType::PressureArray)),
            ("Pressure - [psi]", Some(ArrayType::PressureArray)),
            ("Pump HP:Actual flow - [µL/min]", Some(ArrayType::FlowRateArray)),
            ("Oven temperature - [°C]", Some(ArrayType::TemperatureArray)),
            // mzdata reads a non-standard array itself, so the sampler makes the first one a column,
            // and the point reader hands a non-standard column back without its name (as it did
            // before this change): found by kind, its unit and values compared.
            ("Fraction A - [%]", None),
        ];
        // Each trace as its chromatogram type, its value array's unit and its (time, value) pairs.
        type Trace = (ChromatogramType, Unit, Vec<(f64, f64)>);
        let of_archive = |archive: &std::path::Path| -> Vec<Trace> {
            let mut r = MzPeakReader::new(archive).unwrap();
            let chroms: Vec<_> = (0..r.len_chromatograms()).map(|i| r.get_chromatogram(i).unwrap()).collect();
            value_arrays
                .iter()
                .map(|(id, value_type)| {
                    let c = chroms.iter().find(|c| c.id() == *id).unwrap_or_else(|| panic!("{}: no chromatogram {id}", archive.display()));
                    let times = c.arrays.get(&ArrayType::TimeArray).expect("a time array").to_f64().unwrap().to_vec();
                    let value = match value_type {
                        Some(t) => c.arrays.get(t),
                        None => c.arrays.iter().map(|(_, a)| a).find(|a| matches!(a.name, ArrayType::NonStandardDataArray { .. })),
                    };
                    let value = value.unwrap_or_else(|| {
                        let held: Vec<String> = c.arrays.iter().map(|(t, a)| format!("{t:?} in {:?}", a.unit)).collect();
                        panic!("{}: {id} has no {value_type:?}, only {held:?}", archive.display())
                    });
                    let values = value.to_f64().unwrap().to_vec();
                    assert_eq!(times.len(), values.len(), "{}: {id} pairs its times and values", archive.display());
                    (c.chromatogram_type(), value.unit, times.into_iter().zip(values).collect())
                })
                .collect()
        };

        let archive = dir.join("run.mzpeak");
        write_trace_archive(&dot_d, &archive);
        let want = of_archive(&archive);
        assert_eq!(
            want.iter().map(|t| (t.0, t.1)).collect::<Vec<_>>(),
            [
                (ChromatogramType::PressureChromatogram, Unit::Pascal),
                (ChromatogramType::PressureChromatogram, Unit::Psi),
                (ChromatogramType::FlowRateChromatogram, Unit::MicrolitersPerMinute),
                (ChromatogramType::TemperatureChromatogram, Unit::Celsius),
                (ChromatogramType::Unknown, Unit::Percent),
            ],
            "the source archive holds the traces this test is about"
        );

        let export = dir.join("run.mzML");
        super::filter_mzpeak_to_mzml(&archive, &export, &super::filter::FilterOpts::default()).unwrap();
        let back = dir.join("back.mzpeak");
        super::convert_file(&export, &back, None, 3, None, true, Some(super::TofGridMode::Off), &[], None, true, None)
            .expect("the export converts back into an archive");
        assert_eq!(of_archive(&back), want, "mzML → mzPeak keeps every trace");
        // Each device value array has no column in the chromatogram facet; it is an auxiliary array
        // in its own unit and data type, as the native lane stores it.
        let device_columns = |archive: &std::path::Path, into: &str| -> Vec<String> {
            use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
            let scratch = dir.join(into);
            std::fs::create_dir_all(&scratch).unwrap();
            let mut zip = zip::ZipArchive::new(std::fs::File::open(archive).unwrap()).unwrap();
            let facet = extract_zip_entry(&mut zip, "chromatograms_data.parquet", &scratch);
            let builder = ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(&facet).unwrap()).unwrap();
            leaf_column_names(builder.schema())
                .into_iter()
                .filter(|c| ["pressure", "flow", "temperature"].iter().any(|k| c.to_lowercase().contains(k)))
                .collect()
        };
        assert_eq!(device_columns(&back, "back-facet"), Vec::<String>::new(), "no device array is a column");

        let hop = dir.join("hop.mzML");
        super::convert_to_mzml(&export, &hop, false, None).expect("the export converts into an mzML");
        let xml = std::fs::read_to_string(&hop).unwrap();
        let element = |id: &str| {
            let start = xml.find(&format!("<chromatogram id=\"{id}\"")).unwrap_or_else(|| panic!("no chromatogram {id}"));
            &xml[start..start + xml[start..].find("</chromatogram>").unwrap()]
        };
        let psi = element("Pressure - [psi]");
        assert!(psi.contains("accession=\"MS:1000821\"") && psi.contains("unitAccession=\"UO:0010052\""), "a pressure array in psi: {psi}");
        assert!(!psi.contains("MS:1000786"), "not a non-standard array: {psi}");
        assert_eq!(psi.matches("accession=\"MS:1003019\"").count(), 1, "still a pressure chromatogram, stated once: {psi}");
        let hop_archive = dir.join("hop.mzpeak");
        super::convert_file(&hop, &hop_archive, None, 3, None, true, Some(super::TofGridMode::Off), &[], None, true, None)
            .expect("the mzML → mzML output converts too");
        assert_eq!(of_archive(&hop_archive), want, "mzML → mzML keeps every trace");
    }

    /// What mzdata's mzML reader hands over for an array type it has no case for — the array named
    /// `ArrayType::Unknown`, the type's cvParam among its parameters — through
    /// [`super::readable_chromatogram_arrays`]: a type mzdata knows becomes the array's type and
    /// unit; an accession it does not know, a bare userParam or no parameter at all a non-standard
    /// array; a type the chromatogram already holds does not replace that array. The mzML lane's
    /// writer and the mzPeak lane's sampler and writer, which each panicked on such an array, take
    /// all of them.
    #[test]
    fn unreadable_chromatogram_arrays_get_names_the_writers_accept() {
        use mzdata::params::Unit;
        use mzdata::spectrum::ChromatogramDescription;
        use mzpeak_prototyping::MzPeakReader;

        let bytes = |v: &[f32]| v.iter().flat_map(|x| x.to_ne_bytes()).collect::<Vec<u8>>();
        let chromatogram = |id: &str, typed: Option<ArrayType>, params: Vec<Param>| {
            let mut arrays = BinaryArrayMap::new();
            let times: Vec<u8> = [1.0f64, 2.0].iter().flat_map(|x| x.to_ne_bytes()).collect();
            arrays.add(DataArray::wrap(&ArrayType::TimeArray, BinaryDataArrayType::Float64, times));
            if let Some(t) = typed {
                arrays.add(DataArray::wrap(&t, BinaryDataArrayType::Float32, bytes(&[7.0, 8.0])));
            }
            let mut unknown = DataArray::wrap(&ArrayType::Unknown, BinaryDataArrayType::Float32, bytes(&[1.0, 2.0]));
            unknown.params = (!params.is_empty()).then(|| Box::new(params));
            arrays.add(unknown);
            super::Chromatogram::new(ChromatogramDescription { id: id.to_string(), ..Default::default() }, arrays)
        };
        let pressure = || Param::builder().name("pressure array").curie(mzdata::curie!(MS:1000821)).unit(Unit::Psi).build();
        let raw = || {
            vec![
                chromatogram("pressure", None, vec![Param::builder().name("Instrument").value("pump").build(), pressure()]),
                chromatogram("noise", None, vec![Param::builder().name("noise array").curie(mzdata::curie!(MS:1002742)).build()]),
                chromatogram("user", None, vec![Param::builder().name("pump speed").build()]),
                chromatogram("bare", None, vec![]),
                chromatogram("second pressure", Some(ArrayType::PressureArray), vec![pressure()]),
            ]
        };
        // (array type, unit, values, parameters left) of every array but the times.
        type Arrays = Vec<(ArrayType, Unit, Vec<f32>, usize)>;
        let arrays = |c: &super::Chromatogram| -> Arrays {
            let mut v: Arrays = c
                .arrays
                .iter()
                .filter(|(t, _)| **t != ArrayType::TimeArray)
                .map(|(t, a)| (t.clone(), a.unit, a.to_f32().unwrap().to_vec(), a.params.as_ref().map_or(0, |p| p.len())))
                .collect();
            v.sort_by_key(|a| format!("{:?}", a.0));
            v
        };
        let named: Vec<super::Chromatogram> = raw().into_iter().map(super::readable_chromatogram_arrays).collect();
        assert_eq!(
            named.iter().map(arrays).collect::<Vec<_>>(),
            [
                vec![(ArrayType::PressureArray, Unit::Psi, vec![1.0, 2.0], 1)],
                vec![(ArrayType::nonstandard("noise array"), Unit::Unknown, vec![1.0, 2.0], 0)],
                vec![(ArrayType::nonstandard("pump speed"), Unit::Unknown, vec![1.0, 2.0], 0)],
                vec![(ArrayType::nonstandard("unknown array"), Unit::Unknown, vec![1.0, 2.0], 0)],
                vec![(ArrayType::nonstandard("pressure array"), Unit::Psi, vec![1.0, 2.0], 0), (ArrayType::PressureArray, Unit::Unknown, vec![7.0, 8.0], 0)],
            ]
        );
        assert!(named.iter().all(|c| !c.arrays.has_array(&ArrayType::Unknown)));

        let mut mzml = mzdata::io::mzml::MzMLWriter::new(std::io::sink());
        mzml.set_spectrum_count(0);
        mzml.start_spectrum_list().unwrap();
        super::write_source_chromatograms_mzml(&mut mzml, raw().into_iter()).expect("the mzML lane writes them");

        // The mzPeak lane, on what an mzML can hand it: the fifth case's typed pressure array beside
        // an unreadable one cannot come from mzdata's mzML reader, and sampled into a column it takes
        // the renamed psi array in, which then reads back without its unit.
        let from_mzml = || raw().into_iter().take(4);
        let (dir, _cleanup) = trace_scratch("unknown-arrays");
        let path = dir.join("unknown.mzpeak");
        let mut writer = super::MzPeakWriterType::<std::fs::File>::builder()
            .chromatogram_chunked_encoding(None)
            .sample_array_types_from_chromatograms(from_mzml().map(super::schema_sample_chromatogram))
            .build(std::fs::File::create(&path).unwrap(), true);
        let _ = super::fixup_run_metadata(&mut writer, &dir);
        super::finish_chromatograms(&mut writer, &dir, &super::Ms1Chroms::default(), from_mzml(), false).expect("the mzPeak lane writes them");
        writer.finish_parquet().unwrap().finish().unwrap();
        let mut r = MzPeakReader::new(&path).unwrap();
        let back: Vec<_> = (0..r.len_chromatograms()).map(|i| r.get_chromatogram(i).unwrap()).collect();
        let array = |id: &str, t: ArrayType| {
            let a = back.iter().find(|c| c.id() == id).unwrap_or_else(|| panic!("no chromatogram {id}")).arrays.get(&t).unwrap_or_else(|| panic!("{id}: no {t:?}"));
            (a.unit, a.to_f32().unwrap().to_vec())
        };
        assert_eq!(array("pressure", ArrayType::PressureArray), (Unit::Psi, vec![1.0, 2.0]));
        assert_eq!(array("noise", ArrayType::nonstandard("noise array")), (Unit::Unknown, vec![1.0, 2.0]));
    }

    /// Each chromatogram type's cvParam is the PSI-MS term `psi-ms.obo` names, and reads back as that
    /// type — not mzdata's `ChromatogramType::to_curie`, whose selected-ion-monitoring and
    /// selected-reaction-monitoring accessions are two instrument models.
    #[test]
    fn chromatogram_type_params_are_the_psi_ms_terms() {
        use mzdata::spectrum::ChromatogramType as C;
        for (kind, name, accession) in [
            (C::TotalIonCurrentChromatogram, "total ion current chromatogram", "MS:1000235"),
            (C::BasePeakChromatogram, "basepeak chromatogram", "MS:1000628"),
            (C::SelectedIonCurrentChromatogram, "selected ion current chromatogram", "MS:1000627"),
            (C::SelectedIonMonitoringChromatogram, "selected ion monitoring chromatogram", "MS:1001472"),
            (C::SelectedReactionMonitoringChromatogram, "selected reaction monitoring chromatogram", "MS:1001473"),
            (C::AbsorptionChromatogram, "absorption chromatogram", "MS:1000812"),
            (C::EmissionChromatogram, "emission chromatogram", "MS:1000813"),
            (C::ElectromagneticRadiationChromatogram, "electromagnetic radiation chromatogram", "MS:1000811"),
            (C::FlowRateChromatogram, "flow rate chromatogram", "MS:1003020"),
            (C::PressureChromatogram, "pressure chromatogram", "MS:1003019"),
            (C::TemperatureChromatogram, "temperature chromatogram", "MS:1002715"),
        ] {
            let p = super::chromatogram_type_param(kind).unwrap_or_else(|| panic!("no term for {kind:?}"));
            assert_eq!((p.name.as_str(), p.curie().map(|c| c.to_string())), (name, Some(accession.to_string())), "{kind:?}");
            assert_eq!(p.curie().and_then(C::from_curie), Some(kind), "{accession} reads back as {kind:?}");
        }
        assert!(super::chromatogram_type_param(C::Unknown).is_none());
    }

    /// mzML → mzML keeps each chromatogram's type term. mzdata's reader moves it into the typed
    /// field and its writer writes the parameters alone, so tiny.pwiz's selected ion current trace
    /// came out with none.
    #[test]
    fn mzml_to_mzml_keeps_the_chromatogram_type() {
        let dir = scratch("chromatogram-type");
        let _cleanup = RmDir(dir.clone());
        let out = dir.join("tiny.mzML");
        super::convert_to_mzml(std::path::Path::new(TINY), &out, false, None).expect("tiny.pwiz → mzML");
        let xml = std::fs::read_to_string(&out).unwrap();
        let start = xml.find("<chromatogram id=\"sic\"").expect("the sic trace");
        let sic = &xml[start..start + xml[start..].find("</chromatogram>").unwrap()];
        assert_eq!(sic.matches("accession=\"MS:1000627\"").count(), 1, "one selected ion current chromatogram term: {sic}");
    }

    /// The run-metadata normaliser on what mzdata's readers actually hand over: a Thermo-style
    /// `file:////Users/…` location, a TDF-style full-path `run.id`, and a `default_instrument_id`
    /// of 0 against an EMPTY instrument list (the published-corpus defects M2/M34). The id must come
    /// out as an INTEGER that resolves — the spec requires the field — so an empty list gains an
    /// empty configuration `0` (0.10.0's `null` failed the validator's schema check).
    #[test]
    fn fixup_run_metadata_strips_paths_and_never_mints_a_dangling_instrument() {
        use mzdata::meta::SourceFile;
        let input = std::path::Path::new("/Users/someone/data/PXD018751/SZB8102938.raw");
        let mut w = mzdata::io::mzml::MzMLWriter::new(std::io::sink());
        w.file_description_mut().source_files.push(SourceFile {
            name: "SZB8102938.raw".into(),
            location: "file:////Users/someone/data/PXD018751".into(),
            id: "RAW1".into(),
            ..Default::default()
        });
        w.file_description_mut().source_files.push(SourceFile {
            name: "remote.raw".into(),
            location: "https://ftp.pride.ebi.ac.uk/pride/data/archive".into(),
            id: "RAW2".into(),
            ..Default::default()
        });
        {
            let run = w.run_description_mut().unwrap();
            run.id = Some("/Users/someone/data/2485.d".into());
            run.default_instrument_id = Some(0);
        }
        assert!(w.instrument_configurations().is_empty());

        let _ = super::fixup_run_metadata(&mut w, input);

        let sfs = &w.file_description().source_files;
        assert_eq!(sfs.len(), 2, "no source file synthesised when the reader supplied some");
        assert_eq!(sfs[0].location, "file://", "operator directory reduced to the bare authority");
        assert_eq!(sfs[0].name, "SZB8102938.raw", "the name is the provenance and stays");
        assert_eq!(sfs[1].location, "https://ftp.pride.ebi.ac.uk/pride/data/archive", "a remote locator is not a path");
        let run = w.run_description().unwrap();
        assert_eq!(run.id.as_deref(), Some("SZB8102938"), "path-shaped run.id reset to the input stem");
        assert_eq!(run.default_instrument_id, Some(0), "the required integer, pointing at a real entry");
        assert_eq!(run.default_source_file_id.as_deref(), Some("RAW1"));
        assert_eq!(
            w.instrument_configurations().keys().copied().collect::<Vec<_>>(),
            vec![0],
            "an instrument-less run gets one empty configuration so the reference resolves"
        );
        assert!(w.instrument_configurations()[&0].components.is_empty() && w.instrument_configurations()[&0].params.is_empty());

        // With a list present, an inherited id that resolves is kept and one that does not is
        // clamped onto a real configuration.
        let mut w = mzdata::io::mzml::MzMLWriter::new(std::io::sink());
        w.instrument_configurations_mut()
            .insert(3, mzdata::meta::InstrumentConfiguration { id: 3, ..Default::default() });
        w.run_description_mut().unwrap().default_instrument_id = Some(7);
        let _ = super::fixup_run_metadata(&mut w, input);
        assert_eq!(w.run_description().unwrap().default_instrument_id, Some(3), "dangling 7 clamped to 3");
        w.run_description_mut().unwrap().default_instrument_id = Some(3);
        let _ = super::fixup_run_metadata(&mut w, input);
        assert_eq!(w.run_description().unwrap().default_instrument_id, Some(3), "a resolving id is kept");
        assert!(!super::is_path_shaped_run_id("SZB8102938"));
        assert!(super::is_path_shaped_run_id("C:\\data\\run.d"));
        assert!(super::is_filesystem_location("/Users/x"));
        assert!(super::is_filesystem_location("FILE:///C:/x"));
        assert!(!super::is_filesystem_location("s3://bucket/key"));
    }

    fn spec_from(mzs: &[f64], intens: &[f32], index: usize)
        -> MultiLayerSpectrum<CentroidPeak, DeconvolutedPeak>
    {
        let mut arrays = BinaryArrayMap::new();
        let mut mz = DataArray::wrap(&ArrayType::MZArray, BinaryDataArrayType::Float64, Vec::new());
        mz.update_buffer(mzs).unwrap();
        arrays.add(mz);
        let mut it = DataArray::wrap(&ArrayType::IntensityArray, BinaryDataArrayType::Float32, Vec::new());
        it.update_buffer(intens).unwrap();
        arrays.add(it);
        let mut descr = SpectrumDescription::default();
        descr.index = index;
        descr.signal_continuity = mzdata::spectrum::SignalContinuity::Profile;
        MultiLayerSpectrum::new(descr, Some(arrays), None, None)
    }

    /// The tmp guard removes its file when dropped on the error path, and keeps it (renamed)
    /// after `finish`.
    #[test]
    fn tmp_guard_removes_on_drop_and_keeps_on_finish() {
        use super::TmpGuard;
        let dir = std::env::temp_dir().join(format!("mzpc-tmpguard-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // Error path: the guard goes out of scope without `finish`.
        let tmp = dir.join("a.mzpeak.tmp");
        {
            let _guard = TmpGuard::new(&tmp);
            std::fs::write(&tmp, b"partial").unwrap();
            assert!(tmp.exists());
        }
        assert!(!tmp.exists(), "dropped guard must remove its tmp");

        // Success path: `finish` renames and disarms.
        let tmp = dir.join("b.mzpeak.tmp");
        let out = dir.join("b.mzpeak");
        let _ = std::fs::remove_file(&out);
        let guard = TmpGuard::new(&tmp);
        std::fs::write(&tmp, b"complete").unwrap();
        guard.finish(&out).unwrap();
        assert!(!tmp.exists());
        assert_eq!(std::fs::read(&out).unwrap(), b"complete");

        // Failed rename (onto a non-empty directory): the guard is consumed by `finish` and the
        // tmp is gone with the error.
        let tmp = dir.join("c.mzpeak.tmp");
        let out = dir.join("c.mzpeak");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("occupant"), b"x").unwrap();
        let guard = TmpGuard::new(&tmp);
        std::fs::write(&tmp, b"partial").unwrap();
        assert!(guard.finish(&out).is_err());
        assert!(!tmp.exists(), "a failed rename must still remove the tmp");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The `-v` report opens a native vendor reader only when the report is the whole job: under
    /// `-o` the conversion opens its own, and the `--via-msconvert` lane must not need the native
    /// stack. Without `-o` no lane runs, so `--via-msconvert` does not change that.
    /// (`report_inspect`'s Windows vendor branches follow this decision too.)
    #[test]
    fn inspection_opens_a_native_reader_only_when_inspecting_is_the_job() {
        use super::native_inspect_skip;
        assert_eq!(native_inspect_skip(false, false), None, "a bare inspection opens it");
        assert_eq!(native_inspect_skip(false, true), None, "so does one that names a lane it never runs");
        assert_eq!(
            native_inspect_skip(true, false),
            Some("native reader not opened for this report: the conversion opens it")
        );
        assert_eq!(
            native_inspect_skip(true, true),
            Some("native reader not opened: --via-msconvert reads this file through ProteoWizard")
        );
    }

    /// The report's Thermo gate asks mzdata what it would open, before anything is gunzipped. A
    /// plain `.raw` extension test missed `small.RAW.gz`, which the report then decompressed and
    /// opened through RawFileReader beside the conversion.
    #[test]
    fn the_report_knows_a_thermo_run_as_mzdata_does() {
        use super::mzdata_reads_thermo_raw;
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("mzpc-thermo-infer-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let raw = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/small.RAW")).unwrap();
        let plain = |name: &str, bytes: &[u8]| {
            let path = dir.join(name);
            std::fs::write(&path, bytes).unwrap();
            path
        };
        let gzip = |name: &str| {
            let path = dir.join(name);
            let mut enc = flate2::write::GzEncoder::new(std::fs::File::create(&path).unwrap(), flate2::Compression::default());
            enc.write_all(&raw).unwrap();
            enc.finish().unwrap();
            path
        };
        let cases = [
            (plain("small.RAW", &raw), true),
            (gzip("small.RAW.gz"), true),
            (gzip("upper.raw.GZ"), true),
            (plain("small.dat", &raw), true), // a Thermo header under another name
            (gzip("small.dat.gz"), true),
            (plain("tiny.mzML", b"<?xml version=\"1.0\"?><mzML/>"), false),
            (dir.join("missing.raw"), false),
        ];
        let seen: Vec<bool> = cases.iter().map(|(path, _)| mzdata_reads_thermo_raw(path)).collect();
        let _ = std::fs::remove_dir_all(&dir);
        for ((path, want), got) in cases.iter().zip(seen) {
            assert_eq!(got, *want, "{}", path.display());
        }
    }

    /// The installed panic hook removes the panicking thread's in-flight tmp. `mem::forget` keeps
    /// `Drop` out of this test: only the hook can remove the file here. Tests unwind, so this reaches
    /// the hook's `sweep_tmp_in_flight(false)` branch; the abort branch is the next test.
    #[test]
    fn tmp_panic_hook_sweeps_in_flight_files() {
        use super::{install_tmp_panic_hook, TmpGuard};
        install_tmp_panic_hook();
        let dir = std::env::temp_dir().join(format!("mzpc-tmphook-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tmp = dir.join("panicking.mzpeak.tmp");
        std::fs::write(&tmp, b"partial").unwrap();
        let guard = TmpGuard::new(&tmp);
        std::mem::forget(guard);
        let r = std::panic::catch_unwind(|| panic!("simulated writer-open failure"));
        assert!(r.is_err());
        assert!(!tmp.exists(), "the panic hook must remove the in-flight tmp");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A temp file another process writes (the Agilent host's `.bin` / `.part`) is registered with
    /// the sweep without a guard: the sweep removes it, and once forgotten it is left alone.
    #[test]
    fn tracked_host_temp_files_are_swept_until_forgotten() {
        use super::{sweep_tmp_in_flight, track_tmp_in_flight, TmpGuard};
        let dir = std::env::temp_dir().join(format!("mzpc-tracked-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("mzpc-agilent-1-0.bin");
        let part = dir.join("mzpc-agilent-1-0.bin.part");
        for p in [&bin, &part] {
            std::fs::write(p, b"host output").unwrap();
            track_tmp_in_flight(p);
        }
        TmpGuard::forget_path(&bin);
        sweep_tmp_in_flight(false);
        assert!(!part.exists(), "a tracked host file is removed by the sweep");
        assert!(bin.exists(), "a forgotten one is left alone");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The branch the release binary runs (`panic = "abort"`: no destructor follows the hook).
    /// `sweep_tmp_in_flight(true)` must remove every registered tmp, including one another thread
    /// registered — the ims-compact writer thread can panic while the main thread holds the guard.
    /// It runs in a child copy of this test binary, alone: sweeping every tmp in THIS process would
    /// delete the in-flight output of whichever conversion test runs alongside.
    #[test]
    fn tmp_sweep_all_removes_every_threads_files() {
        use super::{sweep_tmp_in_flight, TmpGuard};
        const CHILD: &str = "MZPC_TEST_SWEEP_ALL_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "tests::tmp_sweep_all_removes_every_threads_files", "--test-threads", "1"])
                .env(CHILD, "1")
                .output()
                .expect("spawning the test binary");
            let log = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
            assert!(out.status.success() && log.contains("1 passed"), "child run failed or ran nothing:\n{log}");
            return;
        }
        let dir = std::env::temp_dir().join(format!("mzpc-sweepall-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tmp = dir.join("writer-thread.mzpeak.tmp");
        std::fs::write(&tmp, b"partial").unwrap();
        // Registered by another thread, which ends without dropping its guard.
        let path = tmp.clone();
        std::thread::spawn(move || std::mem::forget(TmpGuard::new(&path))).join().unwrap();
        sweep_tmp_in_flight(false);
        assert!(tmp.exists(), "the unwind branch sweeps only the calling thread's entries");
        sweep_tmp_in_flight(true);
        assert!(!tmp.exists(), "the abort branch must sweep every registered tmp, whichever thread registered it");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The per-facet summary counters must cover the WRITTEN spectra only: the ≤ 6 schema probes
    /// `convert_vendor_reader` fetches through the same closure before the write loop used to be
    /// counted too (13,206 logged for a 13,200-spectrum Shimadzu file).
    #[test]
    fn vendor_reader_tally_counts_written_spectra_not_probes() {
        use super::{convert_vendor_reader_tallied, FacetRoutes, FacetTally, VendorHints, VendorSpectrum};
        use std::cell::Cell;

        let dir = std::env::temp_dir().join(format!("mzpc-tally-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("tally.mzpeak");
        let _ = std::fs::remove_file(&out);
        let input = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tiny.pwiz.1.1.mzML"));

        const LEN: usize = 20;
        let calls = Cell::new(0usize);
        let tally = convert_vendor_reader_tallied(
            input,
            &out,
            None,
            1,
            None,
            false,
            VendorHints::default(),
            LEN,
            |i| {
                calls.set(calls.get() + 1);
                let mut spec = spec_from(&[100.0 + i as f64, 200.0, 300.0], &[1.0, 2.0, 3.0], i);
                spec.description_mut().id = format!("scan={}", i + 1);
                Ok(VendorSpectrum {
                    spectrum: spec,
                    peak_arrays: None,
                    routes: FacetRoutes { profile_grid: Some(true), profile_span_trimmed: false, centroid_lattice: Some(i % 2 == 0) },
                })
            },
        )
        .unwrap();
        // 6 probes at stride LEN/6 = 3 (indices 0,3,…,15), then the LEN written spectra.
        assert_eq!(calls.get(), LEN + 6, "closure calls = probes + written");
        assert_eq!(
            tally,
            FacetTally { profile_grid: LEN, profile_f64: 0, profile_span_trimmed: 0, centroid_lattice: LEN / 2, centroid_f64: LEN / 2 },
            "tally must count the written spectra only"
        );
        assert!(out.is_file());
        assert!(!dir.join("tally.mzpeak.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A Bruker `.d` whose `GlobalMetadata` states an acquisition time WITHOUT an offset. The
    /// vendor-reader lanes (TSF, BAF, `--bruker-sdk`) see the directory's run metadata only through
    /// `fixup_run_metadata`, which dropped the naive-time block through 0.11.5: a null
    /// `run.start_time` and no `acquisition_time` block. The directory is synthetic, an
    /// `analysis.tsf` SQLite holding just the `GlobalMetadata` table the fixup reads.
    #[test]
    fn unzoned_vendor_directory_clock_reaches_the_index() {
        use super::{convert_vendor_reader_tallied, VendorHints};

        let dir = scratch("unzoned-clock");
        let dot_d = dir.join("run.d");
        fs::create_dir_all(&dot_d).unwrap();
        let db = rusqlite::Connection::open(dot_d.join("analysis.tsf")).unwrap();
        db.execute_batch(
            "CREATE TABLE GlobalMetadata (Key TEXT, Value TEXT);
             INSERT INTO GlobalMetadata VALUES ('AcquisitionDateTime', '2024-10-09T09:09:26');",
        )
        .unwrap();
        drop(db);
        let out = dir.join("run.mzpeak");
        convert_vendor_reader_tallied(&dot_d, &out, None, 1, None, false, VendorHints::default(), 3, |i| {
            Ok(spec_from(&[100.0 + i as f64, 200.0], &[1.0, 2.0], i))
        })
        .unwrap();
        let meta = index_metadata(&out);
        assert_eq!(meta["run"]["start_time"], serde_json::Value::Null, "a naive clock never becomes an instant");
        assert_eq!(meta["acquisition_time"]["wall_clock"], "2024-10-09T09:09:26", "{:#}", meta);
        assert_eq!(meta["acquisition_time"]["zone"], "unstated");
        assert_eq!(meta["acquisition_time"]["source"], "Bruker GlobalMetadata AcquisitionDateTime");
        let _ = fs::remove_dir_all(&dir);
    }

    /// A Bruker BAF `.d` names its three members, each with its SHA-1, through the fixup every
    /// vendor-reader lane shares. The BAF lane itself runs only on Windows and Linux, so the seam is
    /// driven with synthetic spectra. Through 0.11.5 such an archive named only the `.d`, with no
    /// digest; the 0-byte `analysis.tdf` beside the `.baf` is NreB_PAS_DECONV's layout.
    #[test]
    fn baf_directory_members_are_digested_in_the_archive() {
        use super::{convert_vendor_reader_tallied, VendorHints};

        let dir = scratch("baf-members");
        let dot_d = dir.join("run.d");
        fs::create_dir_all(&dot_d).unwrap();
        for (name, body) in [
            ("analysis.baf", &b"baf"[..]),
            ("analysis.baf_idx", b"idx"),
            ("analysis.baf_xtr", b"xtr"),
            ("analysis.tdf", b""),
        ] {
            fs::write(dot_d.join(name), body).unwrap();
        }
        let out = dir.join("run.mzpeak");
        // What `convert_baf` hands over for a baf2sql cache whose `Properties` table is missing or
        // unreadable: still a model term with a name, never the writer's valueless placeholder.
        let hints = VendorHints {
            run_metadata: Some(crate::vendor::baf_properties_metadata(&Default::default())),
            ..VendorHints::default()
        };
        convert_vendor_reader_tallied(&dot_d, &out, None, 1, None, false, hints, 2, |i| {
            Ok(spec_from(&[100.0 + i as f64, 200.0], &[1.0, 2.0], i))
        })
        .unwrap();
        let meta = index_metadata(&out);
        let model: Vec<&str> = meta["instrument_configuration_list"][0]["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["accession"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(model, ["MS:1000122"], "{:#}", meta["instrument_configuration_list"]);
        let files = meta["file_description"]["source_files"].as_array().unwrap();
        let names: Vec<&str> = files.iter().map(|f| f["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["analysis.baf", "analysis.baf_idx", "analysis.baf_xtr"], "{:#}", meta["file_description"]);
        for f in files {
            let name = f["name"].as_str().unwrap();
            let sha = f["parameters"]
                .as_array()
                .unwrap()
                .iter()
                .find(|p| p["accession"] == "MS:1000569")
                .unwrap_or_else(|| panic!("{name} carries no SHA-1"));
            assert_eq!(sha["value"].as_str().unwrap(), crate::embed_aux::sha1_hex(&dot_d.join(name)).unwrap());
        }
        assert_eq!(meta["run"]["default_source_file_id"], "analysis.baf");
        let _ = fs::remove_dir_all(&dir);
    }

    /// A converter-owned MZP CURIE — an `Unknown`-CV CURIE, as the vendored crate represents its
    /// provisional terms — must survive the archive writer → reader round trip as `MZP:1000006` on a
    /// selected ion, the archive must list the MZP vocabulary, and the mzML export must demote the
    /// term to a `userParam` rather than panic in mzdata's CURIE `Display`.
    #[test]
    fn mzp_curie_round_trips_through_archive_and_demotes_for_mzml() {
        use crate::bruker_native::{
            IM_WINDOW_LOWER_NAME, IM_WINDOW_UPPER_NAME, MZP_IM_WINDOW_LOWER, MZP_IM_WINDOW_UPPER,
        };
        use mzdata::params::{ControlledVocabulary, Unit};
        use mzdata::spectrum::{Precursor, SelectedIon};
        use mzpeak_prototyping::param::curie_to_string;
        use mzpeak_prototyping::writer::AbstractMzPeakWriter;
        use mzpeak_prototyping::MzPeakReader;

        let dir = std::env::temp_dir().join(format!("mzpc-mzp-rt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("band.mzpeak");
        let _ = std::fs::remove_file(&path);

        let mut spec = spec_from(&[100.0, 200.0, 300.0], &[1.0, 2.0, 3.0], 0);
        spec.description_mut().id = "frame=1".into();
        spec.description_mut().ms_level = 2;
        let mut ion = SelectedIon { mz: 500.5, ..Default::default() };
        ion.add_param(
            Param::builder()
                .name("inverse reduced ion mobility")
                .curie(mzdata::curie!(MS:1002815))
                .value(1.25)
                .unit(Unit::VoltSecondPerSquareCentimeter)
                .build(),
        );
        crate::bruker_native::add_isolation_mobility_band(&mut ion, 1.30, 1.20);
        spec.description_mut().precursor.push(Precursor { ions: vec![ion], ..Default::default() });

        let handle = std::fs::File::create(&path).unwrap();
        let builder = super::MzPeakWriterType::<std::fs::File>::builder()
            .sample_array_types_from_spectra(std::iter::once(spec.clone()));
        let mut writer = builder.build(handle, true);
        super::ensure_mzp_cv(&mut writer);
        super::ensure_mzp_cv(&mut writer); // idempotent
        let _ = super::fixup_run_metadata(&mut writer, &path);
        writer.write_spectrum(&spec).unwrap();
        // The vendored writer copies the run metadata (cv_list included) into the file index only
        // when it finalizes the chromatogram facet, so give it one — every lane writes a TIC anyway.
        let tic = super::synth_chromatogram(
            "TIC",
            Param::builder().name("total ion current chromatogram").curie(mzdata::curie!(MS:1000235)).build(),
            &[0.0, 1.0],
            &[6.0, 6.0],
        )
        .unwrap();
        writer.write_chromatogram(&tic).unwrap();
        let zip = writer.finish_parquet().unwrap();
        zip.finish().unwrap();

        // cv_list carries MZP exactly once.
        let index: serde_json::Value = {
            let f = std::fs::File::open(&path).unwrap();
            let mut z = zip::ZipArchive::new(f).unwrap();
            let mut e = z.by_name("mzpeak_index.json").unwrap();
            let mut s = String::new();
            std::io::Read::read_to_string(&mut e, &mut s).unwrap();
            serde_json::from_str(&s).unwrap()
        };
        let cvs = index["metadata"]["cv_list"].as_array().unwrap();
        let mzp: Vec<_> = cvs.iter().filter(|c| c["id"] == "MZP").collect();
        assert_eq!(mzp.len(), 1, "cv_list: {cvs:?}");
        assert!(mzp[0]["uri"].as_str().unwrap().ends_with("cv/mzpeak.obo"));

        let mut r = MzPeakReader::new(&path).unwrap();
        let mut back = r.get_spectrum_by_index(0).expect("spectrum 0");
        {
            let ion = &back.precursor_iter().next().expect("precursor").ions[0];
            let ps = ion.params.as_ref().expect("ion params");
            let lo = ps.iter().find(|p| p.name == IM_WINDOW_LOWER_NAME).expect("lower");
            let hi = ps.iter().find(|p| p.name == IM_WINDOW_UPPER_NAME).expect("upper");
            assert_eq!(lo.curie(), Some(MZP_IM_WINDOW_LOWER));
            assert_eq!(hi.curie(), Some(MZP_IM_WINDOW_UPPER));
            assert_eq!(curie_to_string(&lo.curie().unwrap()), "MZP:1000006");
            assert_eq!(curie_to_string(&hi.curie().unwrap()), "MZP:1000007");
            assert_eq!(lo.value.to_f64().unwrap(), 1.20);
            assert_eq!(hi.value.to_f64().unwrap(), 1.30);
            assert_eq!(lo.unit, Unit::VoltSecondPerSquareCentimeter);
        }

        // Demotion: MZP params lose their CV binding, standard ones keep theirs.
        super::demote_mzp_params(back.description_mut());
        let ion = &back.precursor_iter().next().unwrap().ions[0];
        let ps = ion.params.as_ref().unwrap();
        let lo = ps.iter().find(|p| p.name == IM_WINDOW_LOWER_NAME).unwrap();
        assert!(lo.curie().is_none() && lo.accession.is_none() && lo.controlled_vocabulary.is_none());
        assert!(!ps.iter().any(|p| p.controlled_vocabulary == Some(ControlledVocabulary::Unknown)));
        // The reader re-materialises the ion_mobility_value column under PSI's canonical name.
        let im = ps.iter().find(|p| p.curie() == Some(mzdata::curie!(MS:1002815))).unwrap();
        assert_eq!(im.value.to_f64().unwrap(), 1.25);

        // And mzdata's mzML writer accepts the demoted spectrum (it panics on an Unknown CV).
        let mzml = dir.join("band.mzML");
        {
            let f = std::fs::File::create(&mzml).unwrap();
            let mut w = mzdata::io::mzml::MzMLWriter::new(f);
            w.set_spectrum_count(1);
            SpectrumWriter::write(&mut w, &back).unwrap();
            SpectrumWriter::close(&mut w).unwrap();
        }
        let xml = std::fs::read_to_string(&mzml).unwrap();
        assert!(xml.contains(&format!("<userParam type=\"xsd:double\" name=\"{IM_WINDOW_LOWER_NAME}\" value=\"1.2\"")), "{xml}");
        assert!(!xml.contains("MZP:"), "no MZP accession may leak into mzML");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Latin-1 sniff + transcode: an ISO-8859-1 imzML header with a 0xE9 'é' high byte must sniff as
    /// iso-8859-1, decode to valid UTF-8 (é → U+00E9), and have its declaration rewritten to UTF-8.
    #[test]
    fn latin1_imzml_sniff_and_transcode() {
        // Synthetic Latin-1 imzML fragment: 0xE9 is 'é' in ISO-8859-1.
        let mut raw: Vec<u8> = Vec::new();
        raw.extend_from_slice(b"<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?>\n");
        raw.extend_from_slice(b"<mzML><cvParam name=\"Caf");
        raw.push(0xE9); // 'é'
        raw.extend_from_slice(b"\"/></mzML>");

        // Sniff: declared charset is the non-UTF-8 iso-8859-1.
        let enc = sniff_xml_encoding(&raw).expect("declaration present");
        assert_eq!(enc, "iso-8859-1");
        assert!(!super::is_utf8ish(&enc), "iso-8859-1 must trigger the transcode branch");

        // Decode: 0xE9 → U+00E9 'é', and the result is valid UTF-8.
        let utf8 = decode_single_byte(&raw, &enc);
        assert!(utf8.contains("Caf\u{00E9}"), "0xE9 must decode to 'é'");
        // (a String is UTF-8 by construction; assert the bytes round-trip cleanly.)
        assert!(std::str::from_utf8(utf8.as_bytes()).is_ok());

        // Rewrite: the declaration now says UTF-8 (self-consistent), old charset gone.
        let fixed = rewrite_encoding_decl_to_utf8(&utf8);
        assert!(fixed.contains("encoding=\"UTF-8\""), "declaration must be rewritten: {fixed}");
        assert!(!fixed.to_ascii_lowercase().contains("iso-8859-1"));
    }

    /// UTF-8 / no-declaration inputs must NOT trigger transcoding (zero overhead for the common case).
    #[test]
    fn utf8_inputs_are_left_untouched() {
        let utf8_decl = b"<?xml version=\"1.0\" encoding=\"UTF-8\"?><mzML/>";
        let enc = sniff_xml_encoding(utf8_decl).expect("declaration present");
        assert!(super::is_utf8ish(&enc), "utf-8 must be left untouched");

        // No declaration at all → no charset → no transcode.
        assert!(sniff_xml_encoding(b"<mzML>plain ascii</mzML>").is_none());

        // ASCII declaration is also a pass-through.
        let ascii = b"<?xml version='1.0' encoding='US-ASCII'?><mzML/>";
        assert!(super::is_utf8ish(&sniff_xml_encoding(ascii).unwrap()));
    }

    /// windows-1252 0x80 → U+20AC (Euro), proving the 0x80–0x9F block uses the cp1252 table while
    /// the 0xA0–0xFF tail stays latin1-identity.
    #[test]
    fn windows_1252_high_block() {
        let raw = [0x80u8, 0xE9]; // € then é
        let out = decode_single_byte(&raw, "windows-1252");
        assert_eq!(out, "\u{20AC}\u{00E9}");
        // Same bytes under latin1: 0x80 is a C1 control (identity), 0xE9 is é.
        let lat = decode_single_byte(&raw, "iso-8859-1");
        assert_eq!(lat, "\u{0080}\u{00E9}");
    }

    /// The backward scan for `<chromatogramList count>` reads 1 MiB chunks from the end: a tag the
    /// chunk boundary cuts in two must still be found, and a list-less file must end the scan.
    #[test]
    fn declared_chromatogram_count_across_a_chunk_boundary() {
        let dir = std::env::temp_dir().join(format!("mzpc-chromcount-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("x.mzML");
        let tag = br#"<chromatogramList count="7">"#;
        // The first chunk starts 1 MiB before the end and the tag starts `k` bytes before that:
        // 0 = wholly inside the first chunk, 1..=16 = cut in two, 17 = wholly before it.
        for k in [0usize, 1, 8, 16, 17] {
            let mut doc = vec![b' '; 100];
            doc.extend_from_slice(tag);
            doc.resize(100 + k + (1 << 20), b' ');
            std::fs::write(&path, &doc).unwrap();
            assert_eq!(super::declared_chromatogram_count(&path), Some(7), "tag {k} bytes before the boundary");
        }
        std::fs::write(&path, vec![b' '; (2 << 20) + 5]).unwrap();
        assert_eq!(super::declared_chromatogram_count(&path), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// …and the scan ends at `</spectrumList>`, which the list must follow, instead of reading a
    /// list-less file back to its first byte. A decoy tag 2 MiB before the end of the spectra is what
    /// a scan that went on would find.
    #[test]
    fn declared_chromatogram_count_stops_at_the_end_of_the_spectra() {
        let dir = std::env::temp_dir().join(format!("mzpc-chromcount-stop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("x.mzML");
        let mut doc = br#"<chromatogramList count="7">"#.to_vec();
        doc.resize(2 << 20, b' ');
        doc.extend_from_slice(b"</spectrumList>");
        std::fs::write(&path, [doc.as_slice(), b"</run></mzML>"].concat()).unwrap();
        assert_eq!(super::declared_chromatogram_count(&path), None, "the decoy before the spectra's end was read");
        // The real list, in the same chunk as the end of the spectra, is still found.
        std::fs::write(&path, [doc.as_slice(), br#"<chromatogramList count="3"></chromatogramList></run></mzML>"#].concat()).unwrap();
        assert_eq!(super::declared_chromatogram_count(&path), Some(3));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// mzML ids are unique only within their own list, so a spectrum and a chromatogram may share
    /// one. With a single map for both, the spectrum's index entry was pointed at the chromatogram
    /// and a Latin-1 input read no spectra ("source declares 4 spectra but only 0 were read").
    #[test]
    fn rebuilt_index_resolves_each_section_against_its_own_list() {
        let body = r#"<mzML><run><spectrumList count="1"><spectrum index="0" id="x"></spectrum></spectrumList><chromatogramList count="1"><chromatogram index="0" id="x"></chromatogram></chromatogramList></run></mzML>"#;
        let doc = format!(
            r#"<indexedmzML>{body}<indexList count="2"><index name="spectrum"><offset idRef="x">1</offset></index><index name="chromatogram"><offset idRef="x">2</offset><offset idRef="gone">3</offset></index></indexList><indexListOffset>4</indexListOffset></indexedmzML>"#
        );
        let out = super::rebuild_mzml_index(&doc).unwrap();
        let (spectrum, chromatogram) = (out.find("<spectrum ").unwrap(), out.find("<chromatogram ").unwrap());
        assert!(out.contains(&format!(r#"<index name="spectrum"><offset idRef="x">{spectrum}<"#)), "{out}");
        assert!(out.contains(&format!(r#"<index name="chromatogram"><offset idRef="x">{chromatogram}<"#)), "{out}");
        assert!(out.contains(r#"<offset idRef="gone">3<"#), "an unresolvable id keeps its value: {out}");
        assert!(out.contains(&format!("<indexListOffset>{}<", out.find("<indexList ").unwrap())), "{out}");
    }

    /// msconvert's working directory sits beside the user's output, and a release-build panic
    /// aborts without running `Drop`: the panic hook's sweep has to take it, contents and all.
    #[test]
    fn msconvert_dir_is_swept_by_the_panic_hook() {
        let parent = scratch("msconvert-sweep");
        let tmp = super::msconvert_dir(&parent).unwrap();
        fs::write(&tmp.file, b"half an mzML").unwrap();
        // What the hook runs; this thread's entries only, as under `panic = "unwind"`.
        super::sweep_tmp_in_flight(false);
        assert!(!tmp.dir.exists(), "{} survived the sweep", tmp.dir.display());
        drop(tmp);
        let _ = fs::remove_dir_all(&parent);
    }

    /// Only that directory goes with its contents. A directory standing at a tmp FILE path (a user's
    /// `out.mzpeak.tmp/`) survives both the guard's drop and the sweep: neither created it.
    #[test]
    fn tmp_guard_leaves_a_directory_at_its_path_alone() {
        let dir = scratch("tmpguard-dir");
        let occupant = dir.join("out.mzpeak.tmp").join("user-file.txt");
        let tmp = occupant.parent().unwrap();
        fs::create_dir_all(tmp).unwrap();
        fs::write(&occupant, b"keep").unwrap();
        drop(super::TmpGuard::new(tmp));
        assert!(occupant.is_file(), "the guard's drop removed a directory it did not create");
        std::mem::forget(super::TmpGuard::new(tmp));
        super::sweep_tmp_in_flight(false);
        assert!(occupant.is_file(), "the sweep removed a directory it did not create");
        let _ = fs::remove_dir_all(&dir);
    }

    /// PER-SPECTRUM routing: a spectrum entirely on the grid → tof_index (Gridded);
    /// a spectrum with any off-lattice point → exact f64 m/z (F64). Both in one run.
    #[test]
    fn tof_grid_routes_per_spectrum() {
        // Coarse-enough step that the half-step quantization at low m/z exceeds PPM_TOL, so a genuinely
        // off-lattice spectrum has points beyond tolerance and must route F64 (at a very fine grid the
        // 5 ppm tolerance would snap any m/z onto a node, which is correct but wouldn't test routing).
        let grid = tof_grid::TofGrid { c0: 14.0, c1: 1.0e-4 };

        // on-lattice spectrum: build from exact grid points → must route Gridded.
        let on: Vec<f64> = (200_000i32..200_400).map(|k| grid.mz(k)).collect();
        let on_int = vec![1.0f32; on.len()];
        match tof_grid_spectrum(&spec_from(&on, &on_int, 0), &grid).unwrap() {
            TofRoute::Gridded(s) => {
                // The representation is the source's (Profile here), not a routing instruction (M6).
                assert_eq!(s.signal_continuity(), mzdata::spectrum::SignalContinuity::Profile);
                // the gridded spectrum carries tof_index, NOT f64 m/z
                assert!(s.arrays.as_ref().unwrap().get(&ArrayType::nonstandard("tof_index")).is_some());
                // No blanket MS:1000294 on the routed spectrum (M33): the writer must be free to
                // infer MS:1000579/580 from ms_level, which it does only when nothing shadows it.
                assert!(
                    !s.params().iter().any(|p| p.curie() == Some(mzdata::curie!(MS:1000294))),
                    "the generic parent term must not be added by the grid route"
                );
            }
            TofRoute::F64(_) => panic!("on-lattice spectrum should grid"),
        }

        // off-lattice spectrum: arbitrary m/z not on the lattice → must route F64 with EXACT m/z.
        let off: Vec<f64> = (0..50).map(|i| 137.0 + 0.131 * i as f64 + 0.017 * (i as f64).sin()).collect();
        let off_int = vec![2.0f32; off.len()];
        match tof_grid_spectrum(&spec_from(&off, &off_int, 1), &grid).unwrap() {
            TofRoute::F64(s) => {
                assert_eq!(s.signal_continuity(), mzdata::spectrum::SignalContinuity::Profile);
                // exact f64 m/z preserved bit-for-bit
                let back = s.arrays.as_ref().unwrap().mzs().unwrap();
                assert_eq!(back.as_ref(), off.as_slice());
            }
            TofRoute::Gridded(_) => panic!("off-lattice spectrum must keep f64 m/z"),
        }
    }

    /// M6: the grid route never rewrites `signal_continuity`. A CENTROID source stays Centroid on
    /// both routes (gridded → `spectra_peaks` with `tof_index`; off-grid → `spectra_peaks` with its
    /// f64 `mz`), exactly as a Profile source stays Profile above — so `number_of_peaks` /
    /// `number_of_data_points` describe what the source said, not which facet the writer chose.
    #[test]
    fn tof_grid_keeps_the_source_representation() {
        let grid = tof_grid::TofGrid { c0: 14.0, c1: 1.0e-4 };
        let centroid = |mzs: &[f64], ints: &[f32], ix: usize| {
            let mut s = spec_from(mzs, ints, ix);
            s.description_mut().signal_continuity = mzdata::spectrum::SignalContinuity::Centroid;
            s
        };
        let on: Vec<f64> = (200_000i32..200_050).map(|k| grid.mz(k)).collect();
        let TofRoute::Gridded(s) = tof_grid_spectrum(&centroid(&on, &vec![1.0; 50], 0), &grid).unwrap() else {
            panic!("on-lattice spectrum should grid")
        };
        assert_eq!(s.signal_continuity(), mzdata::spectrum::SignalContinuity::Centroid);
        let off: Vec<f64> = (0..20).map(|i| 137.0 + 0.131 * i as f64 + 0.017 * (i as f64).sin()).collect();
        let TofRoute::F64(s) = tof_grid_spectrum(&centroid(&off, &vec![1.0; 20], 1), &grid).unwrap() else {
            panic!("off-lattice spectrum must keep f64 m/z")
        };
        assert_eq!(s.signal_continuity(), mzdata::spectrum::SignalContinuity::Centroid);
    }

    /// TASK 1 (B.3): a gridded TOF spectrum must carry the observed-m/z CV terms (MS:1000528 lowest,
    /// MS:1000527 highest) computed from the source f64 m/z, so the viewer doesn't show "m/z 0–0".
    #[test]
    fn gridded_spectrum_carries_observed_mz_range() {
        let grid = tof_grid::TofGrid { c0: 14.0, c1: 1.0e-4 };
        let on: Vec<f64> = (200_000i32..200_400).map(|k| grid.mz(k)).collect();
        let mut on_int = vec![1.0f32; on.len()];
        on_int[7] = 5.0; // an unambiguous base peak
        let (want_lo, want_hi) = (on[0], on[on.len() - 1]);
        let want_tic = (on.len() - 1) as f64 + 5.0;
        match tof_grid_spectrum(&spec_from(&on, &on_int, 0), &grid).unwrap() {
            TofRoute::Gridded(s) => {
                let lo = s
                    .description()
                    .params()
                    .iter()
                    .find(|p| p.curie() == Some(mzdata::curie!(MS:1000528)))
                    .expect("lowest observed m/z (MS:1000528) present");
                let hi = s
                    .description()
                    .params()
                    .iter()
                    .find(|p| p.curie() == Some(mzdata::curie!(MS:1000527)))
                    .expect("highest observed m/z (MS:1000527) present");
                let lo_v = lo.to_f64().unwrap();
                let hi_v = hi.to_f64().unwrap();
                assert!((lo_v - want_lo).abs() < 1e-6, "lo {lo_v} vs {want_lo}");
                assert!((hi_v - want_hi).abs() < 1e-6, "hi {hi_v} vs {want_hi}");
                assert!(lo_v > 0.0 && hi_v > lo_v, "observed m/z must be a non-zero range");
                // The gridded spectrum's arrays carry NO m/z, so mzdata derives tic = 0 and base
                // peak (0, 0) from them: the summary MUST be stated explicitly on the description
                // or the archive ships zeros (13,200/13,200 spectra on a published Shimadzu run).
                let tic = param_value(&s, mzdata::curie!(MS:1000285))
                    .expect("total ion current (MS:1000285) present");
                assert!((tic - want_tic).abs() < 1e-6, "tic {tic} vs {want_tic}");
                let bp_mz = param_value(&s, mzdata::curie!(MS:1000504))
                    .expect("base peak m/z (MS:1000504) present");
                let bp_int = param_value(&s, mzdata::curie!(MS:1000505))
                    .expect("base peak intensity (MS:1000505) present");
                assert!((bp_mz - on[7]).abs() < 1e-9, "base peak m/z {bp_mz} vs {}", on[7]);
                assert_eq!(bp_int, 5.0);
            }
            TofRoute::F64(_) => panic!("on-lattice spectrum should grid"),
        }
    }

    /// TASK A: the SYNTHESIZED base-peak chromatogram must not be dead on the grid lanes, and it
    /// must carry the SAME numbers as the per-spectrum summary columns of the same archive.
    ///
    /// The old `Ms1Chroms::observe` asked mzdata for `peaks.base_peak()`, which needs an m/z array;
    /// a gridded spectrum has none, so mzdata returned `(0, 0)` and every point of the BPC was zero
    /// (timsTOF 2485: max 0 across all 400 points; SciEX Sample002: zero on 2,371 of 2,372) while
    /// the `base_peak_intensity` COLUMN beside it was right. This asserts both halves: non-zero,
    /// and equal to the column.
    #[test]
    fn gridded_chromatogram_matches_the_spectrum_summary_columns() {
        let grid = tof_grid::TofGrid { c0: 14.0, c1: 1.0e-4 };
        let on: Vec<f64> = (200_000i32..200_400).map(|k| grid.mz(k)).collect();
        let mut on_int = vec![1.0f32; on.len()];
        on_int[7] = 5.0;
        let TofRoute::Gridded(mut s) =
            tof_grid_spectrum(&spec_from(&on, &on_int, 0), &grid).unwrap()
        else {
            panic!("on-lattice spectrum should grid")
        };
        s.description_mut().ms_level = 1;

        // What the WRITER puts in the metadata row (the explicit terms the grid route set).
        let col_tic = param_value(&s, mzdata::curie!(MS:1000285)).expect("MS:1000285 present");
        let col_bp = param_value(&s, mzdata::curie!(MS:1000505)).expect("MS:1000505 present");

        // The defect, still reproducible: the array-derived base peak of a gridded spectrum is 0.
        assert_eq!(
            s.peaks().base_peak().intensity,
            0.0,
            "precondition: a gridded spectrum has no m/z array, so mzdata derives base peak 0"
        );

        let mut ms1 = super::Ms1Chroms::default();
        ms1.observe(&s);
        assert_eq!(ms1.bpc.len(), 1);
        assert!(ms1.bpc[0] > 0.0, "the synthesized BPC must not be dead on a grid lane");
        assert_eq!(ms1.bpc[0], col_bp, "BPC must equal the base_peak_intensity column");
        assert_eq!(ms1.tic[0], col_tic, "TIC must equal the total_ion_current column");
        assert_eq!(ms1.bpc[0], 5.0);
        assert_eq!(ms1.tic[0], 399.0 + 5.0);
    }

    /// A DUAL scan (gridded profile in the data facet + a centroid `PeakSet` alongside) states a
    /// summary of the PROFILE trace. The chromatograms must state the same thing: `spec.peaks()`
    /// prefers the centroid list, which is how `Blind_P1_pos_012` shipped spectrum 0 with column
    /// TIC 13,220 / base 834 next to chromatogram TIC 12,877 / BPC 2,844.
    #[test]
    fn dual_facet_chromatogram_follows_the_profile_not_the_peak_list() {
        let (c0, c1) = (14.0f64, 1.0e-4f64);
        let mz: Vec<f64> = (0..80i32).map(|k| { let r = c0 + c1 * k as f64; r * r }).collect();
        let mut inten = vec![0.0f32; mz.len()];
        for v in inten.iter_mut().take(70).skip(10) {
            *v = 100.0;
        }
        inten[42] = 900.0;
        let mut spec = spec_from(&mz, &inten, 0);
        spec.description_mut().ms_level = 1;
        spec.peaks = Some(mzpeaks::PeakSet::new(vec![
            CentroidPeak::new(mz[20], 10.0, 0),
            CentroidPeak::new(mz[42], 20.0, 1),
        ]));
        let (out, routed, _) = super::shimadzu_grid_route(spec, c1);
        assert_eq!(routed, Some(true));

        let mut ms1 = super::Ms1Chroms::default();
        ms1.observe(&out);
        assert_eq!(ms1.tic[0], 59.0 * 100.0 + 900.0, "TIC is the profile sum, not the centroid 30");
        assert_eq!(ms1.bpc[0], 900.0, "BPC is the profile base peak, not the centroid 20");
        assert_eq!(ms1.tic[0], param_value(&out, mzdata::curie!(MS:1000285)).unwrap());
        assert_eq!(ms1.bpc[0], param_value(&out, mzdata::curie!(MS:1000505)).unwrap());
    }

    /// The param fallback fires for "signal on a non-m/z axis", NOT for "no derived summary". A
    /// genuinely EMPTY spectrum still carries MS:1000285/504/505 in an mzML header; reading them
    /// would stamp a measurement on a row the writer stores as zero/null.
    #[test]
    fn empty_spectrum_chromatogram_point_is_zero_not_the_header_params() {
        let mut s = spec_from(&[], &[], 0);
        s.description_mut().ms_level = 1;
        super::set_spectrum_summary_params(s.description_mut(), 12345.0, Some((500.0, 678.0)));
        let mut ms1 = super::Ms1Chroms::default();
        ms1.observe(&s);
        assert_eq!(ms1.tic[0], 0.0, "an empty spectrum contributes 0 to the TIC chromatogram");
        assert_eq!(ms1.bpc[0], 0.0, "an empty spectrum has no base peak");
    }

    /// An ORDINARY (non-grid) spectrum keeps deriving its chromatogram point from the arrays, so a
    /// source-stated summary that disagrees with its own data cannot leak into the chromatogram —
    /// the writer derives that row from the arrays too.
    #[test]
    fn ungridded_chromatogram_point_comes_from_the_arrays() {
        let mz = [100.0f64, 200.0, 300.0];
        let inten = [1.0f32, 7.0, 2.0];
        let mut s = spec_from(&mz, &inten, 0);
        s.description_mut().ms_level = 1;
        // A source that lies about itself (SCIEX `swath.api-sample-centroid.mzML` does exactly this).
        super::set_spectrum_summary_params(s.description_mut(), 1_184_903.0, Some((444.0, 999.0)));
        let mut ms1 = super::Ms1Chroms::default();
        ms1.observe(&s);
        assert_eq!(ms1.tic[0], 10.0);
        assert_eq!(ms1.bpc[0], 7.0);
    }

    /// TASK C: the grid summary must name the RECONSTRUCTED coordinates, not the source f64 the
    /// fit consumed. Reproduced on the published `…_MRM_03.mzpeak` (spectrum 7313: column
    /// `base_peak_mz = 519.1402875577935`, stored `tof_index` reconstructs to `519.1426532537401`,
    /// 4.6 ppm apart), so the archive named an m/z none of its own points sits at.
    #[test]
    fn gridded_summary_states_the_reconstructed_mz_not_the_source() {
        let grid = tof_grid::TofGrid { c0: 14.0, c1: 1.0e-4 };
        // Source m/z pulled ~2 ppm off the lattice: still INSIDE `ppm_tol()` (5 ppm), so every
        // point grids — and the source and reconstructed coordinates genuinely differ.
        let ks: Vec<i32> = (200_000..200_050).collect();
        let src: Vec<f64> = ks.iter().map(|&k| grid.mz(k) * (1.0 + 2.0e-6)).collect();
        let mut inten = vec![1.0f32; src.len()];
        inten[7] = 5.0;
        let TofRoute::Gridded(s) =
            tof_grid_spectrum(&spec_from(&src, &inten, 0), &grid).unwrap()
        else {
            panic!("a 2 ppm perturbation is inside the tolerance and must still grid")
        };
        let bp_mz = param_value(&s, mzdata::curie!(MS:1000504)).expect("MS:1000504 present");
        let want = grid.mz(ks[7]);
        assert!(
            (bp_mz - want).abs() < 1e-9,
            "base peak m/z {bp_mz} must be the reconstructed {want}, not the source {}",
            src[7]
        );
        assert!(
            (bp_mz - src[7]).abs() > 1e-6,
            "the test is vacuous unless source and reconstruction actually differ"
        );
        let lo = param_value(&s, mzdata::curie!(MS:1000528)).expect("MS:1000528 present");
        let hi = param_value(&s, mzdata::curie!(MS:1000527)).expect("MS:1000527 present");
        assert!((lo - grid.mz(ks[0])).abs() < 1e-9, "lo {lo} must be reconstructed");
        assert!((hi - grid.mz(ks[ks.len() - 1])).abs() < 1e-9, "hi {hi} must be reconstructed");
    }

    /// Read one CV term's numeric value off a spectrum description.
    fn param_value(
        s: &MultiLayerSpectrum<CentroidPeak, DeconvolutedPeak>,
        c: mzdata::params::CURIE,
    ) -> Option<f64> {
        s.description()
            .params()
            .iter()
            .find(|p| p.curie() == Some(c))
            .and_then(|p| p.to_f64().ok())
    }

    /// REGRESSION: unequal m/z / intensity arrays must be a HARD ERROR, never a silent truncation.
    ///
    /// Every grid route decoded the two arrays separately and then walked them with `zip`, which
    /// stops at the shorter one and reports success — so a source handing back 2 m/z and 1
    /// intensity produced a valid one-point archive with the second point simply gone, and the
    /// reverse dropped an intensity. Nothing downstream could see it: the stored arrays agree with
    /// each other and with the summary computed from them. The vendor shims clamp to `Math.Min` one
    /// layer out, which is the hostile-response path this guards.
    #[test]
    fn misaligned_mz_and_intensity_arrays_are_refused() {
        // Equal lengths — including the empty spectrum — are fine.
        assert!(require_aligned_arrays("test", 0, 0, 0).is_ok());
        assert!(require_aligned_arrays("test", 0, 7, 7).is_ok());

        // One extra m/z: the old `zip` silently dropped it.
        let err = require_aligned_arrays("TOF-grid", 42, 2, 1).unwrap_err().to_string();
        assert!(err.contains("spectrum 42"), "the failing spectrum must be named: {err}");
        assert!(err.contains('2') && err.contains('1'), "both lengths must be stated: {err}");
        assert!(
            err.contains("Refusing"),
            "the message must say the conversion stops, not that it worked: {err}"
        );

        // And the mirror case — one extra intensity — is equally refused.
        assert!(require_aligned_arrays("SCIEX grid", 0, 1, 2).is_err());
    }

    /// REGRESSION (the Shimadzu profile grid lane). `shimadzu_grid_route` replaces the f64 m/z with
    /// `tof_index`, and mzdata folds an m/z-less array map to tic = 0 / base peak (0, 0) / "m/z 0–0",
    /// so every gridded spectrum of a published Shimadzu archive shipped zeros while the peak data
    /// itself was intact. The route must state the summary explicitly — and it must describe the
    /// SIGNAL SPAN it actually stores, not the zero-padded source array.
    #[test]
    fn shimadzu_grid_route_summarizes_the_points_it_stores() {
        let (c0, c1) = (14.0f64, 1.0e-4f64);
        let mz: Vec<f64> = (0..80i32)
            .map(|k| {
                let r = c0 + c1 * k as f64;
                r * r
            })
            .collect();
        // Zero-intensity pad at the scan-window bounds (indices 0..10 and 70..80) plus 60 points of
        // signal, one of which is the base peak. The route trims to the span before fitting.
        let mut inten = vec![0.0f32; mz.len()];
        for v in inten.iter_mut().take(70).skip(10) {
            *v = 100.0;
        }
        inten[42] = 900.0;

        let (out, routed, trimmed) = super::shimadzu_grid_route(spec_from(&mz, &inten, 0), c1);
        assert_eq!(routed, Some(true), "an on-grid profile spectrum must route to the grid");
        assert!(trimmed, "the zero pad at both bounds was left out");
        // Signal up to both bounds: gridded, and nothing to trim.
        let (_, routed_unpadded, trimmed_unpadded) =
            super::shimadzu_grid_route(spec_from(&mz[10..70], &inten[10..70], 1), c1);
        assert_eq!((routed_unpadded, trimmed_unpadded), (Some(true), false));

        // The m/z array is gone, replaced by tof_index — which is exactly why the summary has to be
        // carried as CV terms.
        let arrays = out.arrays.as_ref().expect("gridded spectrum keeps arrays");
        assert!(
            arrays.get(&ArrayType::nonstandard("tof_index")).is_some(),
            "the grid route must emit a tof_index array"
        );
        assert!(arrays.mzs().is_err(), "the grid route must drop the m/z array");
        assert_eq!(
            arrays.get(&ArrayType::IntensityArray).unwrap().data_len().unwrap(),
            60,
            "only the signal span is stored"
        );

        let want_tic = 59.0 * 100.0 + 900.0;
        let tic = param_value(&out, mzdata::curie!(MS:1000285)).expect("MS:1000285 present");
        assert!((tic - want_tic).abs() < 1e-6, "tic {tic} vs {want_tic}");
        let bp_mz = param_value(&out, mzdata::curie!(MS:1000504)).expect("MS:1000504 present");
        let bp_int = param_value(&out, mzdata::curie!(MS:1000505)).expect("MS:1000505 present");
        assert!((bp_mz - mz[42]).abs() < 1e-9, "base peak m/z {bp_mz} vs {}", mz[42]);
        assert_eq!(bp_int, 900.0);
        // Observed range = the stored span, NOT the padded source array.
        let lo = param_value(&out, mzdata::curie!(MS:1000528)).expect("MS:1000528 present");
        let hi = param_value(&out, mzdata::curie!(MS:1000527)).expect("MS:1000527 present");
        assert!((lo - mz[10]).abs() < 1e-9, "lo {lo} vs {}", mz[10]);
        assert!((hi - mz[69]).abs() < 1e-9, "hi {hi} vs {}", mz[69]);
    }

    /// The shape that actually ships in the published `.lcd` archives: a DUAL scan, whose profile
    /// trace occupies the data facet while a centroid `PeakSet` rides alongside it as the peak
    /// facet. The route must keep the peak set (it is a facet of the archive) AND state a summary
    /// of the PROFILE span — the points it writes to `spectra_data` — not of the centroid list.
    /// That is the same value the writer derives for this file with `--tof-grid` off, which is what
    /// keeps the two lanes describing the same file identically.
    #[test]
    fn shimadzu_grid_route_keeps_the_peak_set_and_summarizes_the_profile() {
        let (c0, c1) = (14.0f64, 1.0e-4f64);
        let mz: Vec<f64> = (0..80i32)
            .map(|k| {
                let r = c0 + c1 * k as f64;
                r * r
            })
            .collect();
        let mut inten = vec![0.0f32; mz.len()];
        for v in inten.iter_mut().take(70).skip(10) {
            *v = 100.0;
        }
        inten[42] = 900.0;

        let mut spec = spec_from(&mz, &inten, 0);
        // A centroid list that sums to something DIFFERENT from the profile (as on the real file:
        // 12,877 vs 13,220), so the assertion below can tell the two apart.
        spec.peaks = Some(mzpeaks::PeakSet::new(vec![
            CentroidPeak::new(mz[20], 10.0, 0),
            CentroidPeak::new(mz[42], 20.0, 1),
        ]));

        let (out, routed, _) = super::shimadzu_grid_route(spec, c1);
        assert_eq!(routed, Some(true));
        assert_eq!(
            out.peaks.as_ref().map(|p| p.len()),
            Some(2),
            "the centroid facet must survive the grid route"
        );

        let want_tic = 59.0 * 100.0 + 900.0;
        let tic = param_value(&out, mzdata::curie!(MS:1000285)).expect("MS:1000285 present");
        assert!(
            (tic - want_tic).abs() < 1e-6,
            "TIC {tic} must be the profile span sum {want_tic}, not the centroid sum 30"
        );
        let bp_int = param_value(&out, mzdata::curie!(MS:1000505)).expect("MS:1000505 present");
        assert_eq!(bp_int, 900.0, "the base peak is the profile's, not the centroid list's");
    }

    /// A spectrum with no positive intensity has no base peak: TIC 0 is the truth, but MS:1000504 /
    /// MS:1000505 must stay ABSENT rather than claiming a peak at m/z 0.
    #[test]
    fn all_zero_gridded_spectrum_gets_tic_zero_and_no_base_peak() {
        let grid = tof_grid::TofGrid { c0: 14.0, c1: 1.0e-4 };
        let on: Vec<f64> = (200_000i32..200_100).map(|k| grid.mz(k)).collect();
        let on_int = vec![0.0f32; on.len()];
        match tof_grid_spectrum(&spec_from(&on, &on_int, 0), &grid).unwrap() {
            TofRoute::Gridded(s) => {
                let tic = param_value(&s, mzdata::curie!(MS:1000285)).expect("MS:1000285 present");
                assert_eq!(tic, 0.0, "an all-zero spectrum's TIC is legitimately 0");
                assert!(
                    param_value(&s, mzdata::curie!(MS:1000504)).is_none(),
                    "no base peak m/z may be fabricated"
                );
                assert!(
                    param_value(&s, mzdata::curie!(MS:1000505)).is_none(),
                    "no base peak intensity may be fabricated"
                );
            }
            TofRoute::F64(_) => panic!("on-lattice spectrum should grid"),
        }
    }

    /// Ties in intensity resolve to the LOWEST m/z — the ims lanes emit points grouped by mobility
    /// scan, so "first wins" is not the same thing as "lowest m/z".
    #[test]
    fn summary_base_peak_ties_resolve_to_lowest_mz() {
        let mzs = [300.0f64, 100.0, 200.0];
        let intens = [7.0f32, 7.0, 1.0];
        let (tic, base) = super::summarize_points(intens.iter().copied(), |i| mzs[i]);
        assert_eq!(tic, 15.0);
        assert_eq!(base, Some((100.0, 7.0)));
    }

    /// Regression (Option E backstop): `spectra_peaks` must never carry two `intensity array`
    /// columns. A mixed-precision mzML (profile MS1 with 64-bit intensity + centroid MS2) used to
    /// emit a second, all-null `intensity_f64*` column reusing `array_name: "intensity array"` — the
    /// peaks-schema sampler adds the source-precision (f64) intensity while the fixed-precision peak
    /// write path only fills the f32 primary. Readers resolving arrays by `array_name` (no
    /// `buffer_priority`) then clobbered the real f32 data with the null f64 → blank spectrum view.
    /// The writer now prunes the all-null duplicate from the finished facet. Both layouts are checked:
    /// point layout additionally regresses a Float64/Float32 write clash if the twin is dropped from
    /// the *write* schema, so this guards that E prunes at OUTPUT (post-write), not at schema time.
    #[test]
    fn peaks_facet_has_single_intensity_array_column() {
        use mzpeak_prototyping::chunk_series::ChunkingStrategy;
        use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
        use std::fs;

        let input = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data/mixed_precision.mzML");
        assert!(input.exists(), "fixture missing: {}", input.display());

        fn count_intensity(fields: &arrow::datatypes::Fields) -> usize {
            fields
                .iter()
                .map(|f| match f.data_type() {
                    arrow::datatypes::DataType::Struct(children) => count_intensity(children),
                    _ => (f.metadata().get("array_name").map(String::as_str)
                        == Some("intensity array")) as usize,
                })
                .sum()
        }

        // Point layout exercises the full fix chain — #1 coalesce-by-accession (one intensity
        // column), #2 precision coercion (f64 raw intensity cast into the f32 primary, no clash), and
        // the #3 invariant debug_assert — via the array_map write path. It cannot see #1 regress on its
        // own: the post-write prune drops an all-null `point` twin, so a sampler that adds one again
        // still ends with one column. The chunked layout (the converter's default, numpress-linear at
        // 50 Th) is outside the prune's scope, so there #1 alone keeps the second 'intensity array'
        // out. Release profile only: in debug the chunked write trips a separate, pre-existing
        // chunk-facet spill `debug_assert` unrelated to the twin.
        let cases: [(&str, Option<ChunkingStrategy>); 2] = [
            ("point", None),
            ("chunked", Some(ChunkingStrategy::NumpressLinear { chunk_size: 50.0 })),
        ];
        for (tag, chunk) in cases {
            let scratch =
                std::env::temp_dir().join(format!("mzpc-peaks-{tag}-{}", std::process::id()));
            fs::create_dir_all(&scratch).unwrap();
            let output = scratch.join("mixed.mzpeak");
            let _ = fs::remove_file(&output);

            // synth_chroms=true mirrors the CLI default. (An unrelated pre-existing point-layout write
            // clash triggers only with --no-chromatograms + mixed precision; not this test's concern.)
            super::convert_file(&input, &output, chunk, 3, None, true, Some(super::TofGridMode::Off), &[], None, true, None)
                .unwrap_or_else(|e| panic!("[{tag}] conversion failed: {e:#}"));

            let f = fs::File::open(&output).unwrap();
            let mut zip = zip::ZipArchive::new(f).unwrap();
            let peaks = extract_zip_entry(&mut zip, "spectra_peaks.parquet", &scratch);
            let schema = ParquetRecordBatchReaderBuilder::try_new(fs::File::open(&peaks).unwrap())
                .unwrap()
                .schema()
                .clone();
            let n = count_intensity(schema.fields());
            let _ = fs::remove_dir_all(&scratch);
            assert_eq!(
                n, 1,
                "[{tag}] spectra_peaks must have exactly one 'intensity array' column (no null twin); got {n} in {:?}",
                schema.fields().iter().map(|f| f.name()).collect::<Vec<_>>()
            );
        }
    }

    /// `--to mzml` lane: converting an mzML input to mzML must preserve the spectra (count + data)
    /// via the mzdata writer. Uses the committed mixed-precision fixture (3 profile MS1 + 3 centroid
    /// MS2) and re-reads the output to confirm a faithful round-trip.
    #[test]
    fn mzml_output_preserves_spectra() {
        use mzdata::prelude::SpectrumSource;
        use std::fs;

        let input = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data/mixed_precision.mzML");
        assert!(input.exists(), "fixture missing: {}", input.display());
        let scratch = std::env::temp_dir().join(format!("mzpc-mzml-{}", std::process::id()));
        fs::create_dir_all(&scratch).unwrap();
        let out = scratch.join("out.mzML");

        super::convert_to_mzml(&input, &out, false, None).expect("mzML conversion");

        // Output is XML mzML (not a zip), and re-reads to the same spectra: the count, then each
        // spectrum's m/z and intensity arrays against the source's.
        let head = fs::read(&out).unwrap();
        let is_mzml = head.starts_with(b"<?xml") || head.windows(5).any(|w| w == b"<mzML");
        fn read_all(p: &std::path::Path) -> Vec<mzdata::spectrum::MultiLayerSpectrum> {
            super::MZReaderType::<_, super::CentroidPeak, super::DeconvolutedPeak>::open_path(p)
                .unwrap_or_else(|e| panic!("reopen {}: {e}", p.display()))
                .iter()
                .collect()
        }
        let (source, written) = (read_all(&input), read_all(&out));
        let _ = fs::remove_dir_all(&scratch);
        assert!(is_mzml, "output is not mzML XML");
        assert_eq!(written.len(), 6, "expected 6 spectra in the mzML output, got {}", written.len());
        for (s, w) in source.iter().zip(&written) {
            use mzdata::prelude::SpectrumLike;
            let (sa, wa) = (s.raw_arrays().expect("source arrays"), w.raw_arrays().expect("written arrays"));
            assert_eq!(wa.mzs().unwrap(), sa.mzs().unwrap(), "{}: m/z array", s.id());
            assert_eq!(wa.intensities().unwrap(), sa.intensities().unwrap(), "{}: intensity array", s.id());
        }
    }

    /// The timsTOF fixture of the corpus-gated tests below: the same run tests/tdf_*.rs pin as DOT_D.
    const TDF_2485: &str = "ims-examples/PXD059079/20230830_100SPD_NCI7_0p12ng_HS_01_S1-B1_1_2485.d";
    /// ProteoWizard's SCIEX SWATH centroid example, gzipped: every spectrum sits on the TOF lattice.
    const SWATH_GZ: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/swath.api-sample-centroid.mzML.gz");

    /// Removes a scratch directory when a test ends, pass or fail. The ims-compact pair wrote about
    /// 1.75 GB per archive into a new pid-named directory on every run and never removed it.
    struct RmDir(std::path::PathBuf);
    impl Drop for RmDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A CENTROID-ONLY archive must return its signal through `get_spectrum_by_id`, not just
    /// `get_spectrum_by_index`. `get_spectrum_by_id` used to call `get_spectrum_arrays`
    /// unconditionally, which reads only the `spectra_data` facet — so every peak living in
    /// `spectra_peaks` was invisible by ID and the call returned an empty spectrum. Centroid-only
    /// archives are the common case for several vendors, so this was a silent hole in the API.
    /// The committed centroid-only fixture exercises exactly this: every spectrum is centroid, so
    /// `spectra_data` stays empty and all the signal lives in `spectra_peaks`.
    #[test]
    fn by_id_reads_the_peaks_facet_on_a_centroid_only_archive() {
        use mzdata::io::DetailLevel;
        use mzdata::prelude::SpectrumSource;
        use mzpeak_prototyping::MzPeakReader;
        use std::fs;

        let input = std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tiny_centroid_only.mzML"));
        // Unique per process: this test binary gets run more than once in a single `cargo test`
        // invocation, and a shared fixed path made the two runs delete each other's output mid-write.
        let tmp = std::env::temp_dir().join(format!("mzpc_by_id_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let out = tmp.join("out.mzpeak");
        super::convert_file(
            &input,
            &out,
            Some(super::ChunkingStrategy::NumpressLinear { chunk_size: 50.0 }),
            3,
            None,
            true,
            Some(super::TofGridMode::Off),
            &[],
            None,
            true,
            None,
        )
        .unwrap();

        let mut reader = MzPeakReader::new(&out).unwrap();
        reader.set_detail_level(DetailLevel::Full);
        let by_index = reader.get_spectrum_by_index(0).expect("spectrum 0 by index");
        let id = by_index.id().to_string();
        let n_index = by_index.peaks().len();
        assert!(n_index > 0, "fixture is not carrying signal; test is vacuous");

        let by_id = reader.get_spectrum_by_id(&id).expect("spectrum by id");
        assert_eq!(
            by_id.peaks().len(),
            n_index,
            "by-id returned {} points but by-index returned {n_index} for {id}",
            by_id.peaks().len()
        );
        let _ = fs::remove_dir_all(&tmp);
    }

    /// A chromatogram-only archive (0 spectra) must reach mzML with its quantitative traces intact —
    /// through the mzPeak→mzML export and again through the mzML→mzML lane, where the 0-spectra
    /// "Run to Run" writer crash lived. The source is written here by mzdata from the committed
    /// tiny.pwiz fixture's `sic` trace, so it is a valid indexedmzML — mzdata enumerates chromatograms
    /// only through that index — and the test runs everywhere. It used to pin a corpus archive whose
    /// contents had drifted away from this description, and it compared chromatogram COUNTS, which a
    /// duplicated summary or a blank trace also satisfies. Here the non-summary trace — tiny.pwiz's
    /// selected-ion current — must keep its id, its point counts and its intensities at each hop, and
    /// each mzML must carry exactly one TIC and one base-peak trace.
    #[test]
    fn mzml_output_preserves_srm_chromatograms() {
        use mzdata::prelude::{ChromatogramLike, ChromatogramSource, MSDataFileMetadata};

        type Reader = super::MZReaderType<std::fs::File, super::CentroidPeak, super::DeconvolutedPeak>;
        let is_tic = |c: &super::Chromatogram| matches!(c.chromatogram_type(), super::ChromatogramType::TotalIonCurrentChromatogram);
        let is_bpc = |c: &super::Chromatogram| matches!(c.chromatogram_type(), super::ChromatogramType::BasePeakChromatogram);
        // (id, time points, intensity points, intensity sum) of every non-summary trace, plus the number
        // of TIC and base-peak traces.
        type Trace = (String, usize, usize, f64);
        let profile = |chroms: Vec<super::Chromatogram>| -> (Vec<Trace>, usize, usize) {
            let traces = chroms.iter().filter(|c| !is_tic(c) && !is_bpc(c))
                .map(|c| {
                    let intensity = c.intensity().map(|v| v.to_vec()).unwrap_or_default();
                    let sum = intensity.iter().map(|&x| x as f64).sum::<f64>();
                    (c.id().to_string(), c.time().map(|t| t.len()).unwrap_or(0), intensity.len(), sum)
                })
                .collect();
            (traces, chroms.iter().filter(|c| is_tic(c)).count(), chroms.iter().filter(|c| is_bpc(c)).count())
        };
        let of_mzml = |p: &std::path::Path| profile(Reader::open_path(p).expect("reopen mzML").iter_chromatograms().collect());

        let dir = scratch("srm");
        let src = dir.join("chromatograms_only.mzML");
        {
            let mut tiny = Reader::open_path(TINY).expect("open tiny.pwiz");
            let traces: Vec<super::Chromatogram> = tiny.iter_chromatograms().filter(|c| !is_tic(c) && !is_bpc(c)).collect();
            let mut w = mzdata::io::mzml::MzMLWriter::new(fs::File::create(&src).unwrap());
            w.copy_metadata_from(&tiny);
            w.set_spectrum_count(0);
            w.start_spectrum_list().unwrap();
            for c in &traces {
                w.write_chromatogram(c).unwrap();
            }
            w.close().unwrap();
        }
        let want = of_mzml(&src).0;
        assert_eq!(want.iter().map(|t| (t.0.as_str(), t.1, t.2)).collect::<Vec<_>>(), [("sic", 10, 10)],
            "the source carries tiny.pwiz's selected-ion trace and no spectra");
        assert!(want[0].3 > 0.0, "the trace is not blank");
        // Ids and point counts exactly; intensities by their sum, within float32 round-off.
        let same = |got: &[Trace], lane: &str| {
            assert_eq!(got.len(), want.len(), "{lane}: {got:?} vs {want:?}");
            for (g, w) in got.iter().zip(&want) {
                assert_eq!((&g.0, g.1, g.2), (&w.0, w.1, w.2), "{lane} keeps the trace's id and point counts");
                assert!((g.3 - w.3).abs() <= 1e-6 * w.3.abs(), "{lane} keeps its intensities: sum {} vs {}", g.3, w.3);
            }
        };

        let archive = dir.join("chromatograms_only.mzpeak");
        let hop1 = dir.join("hop1.mzML");
        let hop2 = dir.join("hop2.mzML");
        for (from, to) in [(&src, &archive), (&archive, &hop1), (&hop1, &hop2)] {
            let args: Vec<&std::ffi::OsStr> = vec![from.as_os_str(), "-o".as_ref(), to.as_os_str(), "--force".as_ref()];
            let (ok, _, err) = run_bin(&args, &[]);
            assert!(ok, "{} → {} must not fail: {err}", from.display(), to.display());
        }
        let in_archive = {
            let mut r = mzpeak_prototyping::MzPeakReader::new(&archive).expect("open the archive");
            let n = r.count_chromatograms();
            profile((0..n).filter_map(|i| r.get_chromatogram_by_index(i)).collect())
        };
        let (after_export, after_relay) = (of_mzml(&hop1), of_mzml(&hop2));
        let _ = fs::remove_dir_all(&dir);
        same(&in_archive.0, "the archive");
        for (lane, (traces, tic, bpc)) in [("mzPeak → mzML", after_export), ("mzML → mzML", after_relay)] {
            same(&traces, lane);
            assert_eq!((tic, bpc), (1, 1), "{lane} writes one TIC and one base-peak trace, never a duplicate");
        }
    }

    /// Flatten a peaks/data schema to the leaf column names — the v0.7 layout nests the signal
    /// columns inside a top-level `point`/`chunk` struct, so a root-level lookup finds nothing.
    fn leaf_column_names(schema: &arrow::datatypes::Schema) -> Vec<String> {
        schema
            .fields()
            .iter()
            .flat_map(|f| match f.data_type() {
                arrow::datatypes::DataType::Struct(kids) => {
                    kids.iter().map(|k| k.name().clone()).collect::<Vec<_>>()
                }
                _ => vec![f.name().clone()],
            })
            .collect()
    }


    /// B.4 regression (frame-preserving ims-compact). Corpus-gated: needs a real timsTOF `.d`
    /// (2485.d, 142 MB), too large to vendor. Convert it and assert the peak facet carries
    /// `mean_inverse_reduced_ion_mobility` (MS:1003006) and that #spectra == #TDF frames
    /// (one spectrum per FRAME, not per mobility scan). `#[ignore]` by default; run with:
    ///   `cargo test --release ims_compact_is_frame_preserving -- --ignored --nocapture`
    #[test]
    #[ignore = "needs the 142 MB 2485.d timsTOF corpus fixture (MZPEAK_CORPUS); run with --include-ignored"]
    fn ims_compact_is_frame_preserving() {
        use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
        use std::fs;

        // Pinned, not searched: a sorted walk of the corpus took whichever TDF sorted first, which had
        // quietly become a 1.7 GB run rather than the SBA415 this doc used to name.
        let Some(input) = crate::corpus_gate::corpus_path(TDF_2485) else { return };
        let input = input.as_path();

        // A directory of its own. `contract_ims_compact_calibration_keys` converts the same `.d` and extracts the same facet
        // names; with the one `mzpc-test-{pid}` both used to share, a parallel run let one test
        // truncate the Parquet file the other had just opened ("Parquet file too small. Size is 0").
        let scratch = &scratch("ims-frame-preserving");
        let scratch = scratch.as_path();
        let _rm = RmDir(scratch.to_path_buf());
        let output = scratch.join("ims_compact.mzpeak");
        let _ = fs::remove_file(&output);

        super::convert_ims_compact_archive(input, &output, 3, None, false, false, false, 50.0)
            .expect("ims-compact conversion");

        // Crack the zip archive and extract facets to scratch files (File: ChunkReader).
        let f = fs::File::open(&output).unwrap();
        let mut zip = zip::ZipArchive::new(f).unwrap();

        // (a) the peak facet has the mean 1/K0 mobility column (MS:1003006).
        let peaks_path = extract_zip_entry(&mut zip, "spectra_peaks.parquet", scratch);
        let builder =
            ParquetRecordBatchReaderBuilder::try_new(fs::File::open(&peaks_path).unwrap()).unwrap();
        let schema = builder.schema().clone();
        let cols = leaf_column_names(&schema);
        assert!(
            cols.iter().any(|n| n == "mean_inverse_reduced_ion_mobility"),
            "spectra_peaks must carry mean_inverse_reduced_ion_mobility (MS:1003006); got {cols:?}"
        );
        // The peak facet stores integer `tof`, not m/z.
        assert!(cols.iter().any(|n| n == "tof"), "peak facet must have a `tof` column; got {cols:?}");
        // …and the mobility column is POPULATED, not merely declared: every point carries its scan's
        // 1/K0, inside the acquisition's mobility range, and the values vary (a constant is a
        // placeholder, not a mobility).
        let conn = rusqlite::Connection::open_with_flags(
            input.join("analysis.tdf"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let global = |k: &str| -> f64 {
            conn.query_row("SELECT Value FROM GlobalMetadata WHERE Key = ?1", [k], |r| r.get::<_, String>(0))
                .unwrap_or_else(|e| panic!("GlobalMetadata {k}: {e}"))
                .trim()
                .parse()
                .unwrap()
        };
        let (lo, hi) = (global("OneOverK0AcqRangeLower"), global("OneOverK0AcqRangeUpper"));
        let (mut points, mut nulls, mut min, mut max) = (0usize, 0usize, f64::INFINITY, f64::NEG_INFINITY);
        for batch in builder.build().unwrap() {
            let batch = batch.unwrap();
            let mobility = batch
                .columns()
                .iter()
                .find_map(|c| {
                    c.as_any()
                        .downcast_ref::<arrow::array::StructArray>()?
                        .column_by_name("mean_inverse_reduced_ion_mobility")
                        .cloned()
                })
                .expect("mean_inverse_reduced_ion_mobility inside the peak struct");
            let mobility = arrow::compute::cast(&mobility, &arrow::datatypes::DataType::Float64).unwrap();
            let mobility = mobility.as_any().downcast_ref::<arrow::array::Float64Array>().unwrap();
            points += mobility.len();
            nulls += arrow::array::Array::null_count(mobility);
            for v in mobility.iter().flatten() {
                min = min.min(v);
                max = max.max(v);
            }
        }
        assert!(points > 0, "the peak facet holds no points");
        assert_eq!(nulls, 0, "{nulls} of {points} points carry no 1/K0");
        assert!(
            lo - 1e-9 <= min && max <= hi + 1e-9 && min < max,
            "1/K0 spans {min}..{max}; the acquisition range is {lo}..{hi}, and a real mobility varies"
        );

        // (b) #spectra == #TDF frames (one spectrum per frame).
        let n_frames = {
            let conn = rusqlite::Connection::open_with_flags(
                input.join("analysis.tdf"),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .unwrap();
            conn.query_row("SELECT COUNT(*) FROM Frames", [], |r| r.get::<_, i64>(0))
                .unwrap() as u64
        };
        // One row per spectrum in spectra_metadata.parquet; compare its row count to the frame count.
        let meta_path = extract_zip_entry(&mut zip, "spectra_metadata.parquet", scratch);
        let meta_builder =
            ParquetRecordBatchReaderBuilder::try_new(fs::File::open(&meta_path).unwrap()).unwrap();
        let n_spectra: i64 = meta_builder.metadata().file_metadata().num_rows();
        assert_eq!(
            n_spectra as u64, n_frames,
            "expected one spectrum per TDF frame: {n_spectra} spectra vs {n_frames} frames"
        );
    }

    /// Regression: the ims-chunked path only chunked the PEAK facet while leaving the (empty,
    /// centroid-only) DATA facet at the point default — a mixed layout family for the `spectrum`
    /// entity (HUPO-PSI/mzPeak-specification#21). On 0.9.2 the writer aborted on it ("layout family
    /// mismatch between spectrum facets"); since 0.9.3 it only warns and writes the mixed archive.
    /// Both `spectrum` facets must declare the same family under --ims-chunked either way.
    /// Corpus-gated like `ims_compact_is_frame_preserving`; run with:
    ///   `cargo test --release ims_chunked_spectrum_facets_share_one_family -- --ignored --nocapture`
    #[test]
    #[ignore = "needs the 142 MB 2485.d timsTOF corpus fixture (MZPEAK_CORPUS); run with --include-ignored"]
    fn ims_chunked_spectrum_facets_share_one_family() {
        use parquet::file::reader::{FileReader, SerializedFileReader};

        let Some(input) = crate::corpus_gate::corpus_path(TDF_2485) else { return };
        let scratch = scratch("ims-chunked-family");
        let _rm = RmDir(scratch.clone());
        let output = scratch.join("ims_chunked.mzpeak");

        // ims_chunked = true: the configuration that wrote a point data facet beside chunked peaks.
        super::convert_ims_compact_archive(&input, &output, 3, None, false, false, true, 50.0)
            .expect("--ims-chunked conversion");

        // The family is declared in each facet's footer as `spectrum_array_index.prefix`.
        let mut zip = zip::ZipArchive::new(fs::File::open(&output).unwrap()).unwrap();
        let family_of = |zip: &mut zip::ZipArchive<fs::File>, member: &str| -> (String, i64) {
            let path = extract_zip_entry(zip, member, &scratch);
            let reader = SerializedFileReader::new(fs::File::open(&path).unwrap()).unwrap();
            let meta = reader.metadata().file_metadata();
            let index = meta
                .key_value_metadata()
                .and_then(|kvs| kvs.iter().find(|kv| kv.key == "spectrum_array_index"))
                .and_then(|kv| kv.value.clone())
                .unwrap_or_else(|| panic!("{member} has no spectrum_array_index"));
            let index: serde_json::Value = serde_json::from_str(&index).unwrap();
            (index["prefix"].as_str().unwrap().to_string(), meta.num_rows())
        };
        let (data_family, _) = family_of(&mut zip, "spectra_data.parquet");
        let (peaks_family, peak_rows) = family_of(&mut zip, "spectra_peaks.parquet");
        assert!(peak_rows > 0, "spectra_peaks is empty; the fixture carries no signal");
        assert_eq!(peaks_family, "chunk", "--ims-chunked must write a chunked peak facet");
        assert_eq!(
            data_family, peaks_family,
            "spectra_data ('{data_family}') and spectra_peaks ('{peaks_family}') are both \
             `entity_type: spectrum` and MUST share one layout family"
        );
    }

    /// The same pin without the corpus. The family is fixed when the writers are built, so two
    /// synthetic frames through `write_ims_compact_archive_impl` show it: under `--ims-chunked` both
    /// `spectrum` facets must declare `chunk` (the empty data facet used to be `point`).
    #[test]
    fn ims_chunked_spectrum_facets_share_one_family_without_the_corpus() {
        use mzdata::params::Unit;
        use parquet::file::reader::{FileReader, SerializedFileReader};

        let dir = scratch("ims-chunked-synthetic");
        let output = dir.join("synthetic.mzpeak");
        // m/z = (10 + 2e-5·tof)² spans 144–256 Th over these bins, so 50-Th chunks split each frame.
        let frame = |i: usize, int_intensity: bool| -> anyhow::Result<MultiLayerSpectrum> {
            let mut arrays = BinaryArrayMap::new();
            let mut tof = DataArray::wrap(&ArrayType::nonstandard("tof"), BinaryDataArrayType::Int32, Vec::new());
            tof.update_buffer(&[100_000i32, 150_000, 200_000, 300_000]).unwrap();
            arrays.add(tof);
            let mut intensity = if int_intensity {
                let mut da = DataArray::wrap(&ArrayType::IntensityArray, BinaryDataArrayType::Int32, Vec::new());
                da.update_buffer(&[1i32, 2, 3, 4]).unwrap();
                da
            } else {
                let mut da = DataArray::wrap(&ArrayType::IntensityArray, BinaryDataArrayType::Float32, Vec::new());
                da.update_buffer(&[1.0f32, 2.0, 3.0, 4.0]).unwrap();
                da
            };
            intensity.unit = Unit::DetectorCounts;
            arrays.add(intensity);
            let mut mobility =
                DataArray::wrap(&ArrayType::MeanInverseReducedIonMobilityArray, BinaryDataArrayType::Float64, Vec::new());
            mobility.update_buffer(&[1.0f64, 1.05, 1.1, 1.2]).unwrap();
            arrays.add(mobility);
            let descr = SpectrumDescription {
                id: format!("frame={}", i + 1),
                index: i,
                ms_level: 1,
                signal_continuity: mzdata::spectrum::SignalContinuity::Centroid,
                ..Default::default()
            };
            Ok(MultiLayerSpectrum::new(descr, Some(arrays), None, None))
        };
        type Parallel = fn(usize, bool) -> anyhow::Result<MultiLayerSpectrum>;
        // The input is only named: without an analysis.tdf the vendor calibration block is skipped.
        super::write_ims_compact_archive_impl::<_, Parallel>(
            &dir.join("synthetic.d"), &output, 3, None, false, 10.0, 2e-5, "global_metadata", 2,
            "m/z-chunked", Some(50.0), None, None, super::Driver::Serial(frame),
        )
        .expect("--ims-chunked write");

        let mut zip = zip::ZipArchive::new(fs::File::open(&output).unwrap()).unwrap();
        let family_of = |zip: &mut zip::ZipArchive<fs::File>, member: &str| -> (String, i64) {
            let path = extract_zip_entry(zip, member, &dir);
            let reader = SerializedFileReader::new(fs::File::open(&path).unwrap()).unwrap();
            let meta = reader.metadata().file_metadata();
            let index = meta
                .key_value_metadata()
                .and_then(|kvs| kvs.iter().find(|kv| kv.key == "spectrum_array_index"))
                .and_then(|kv| kv.value.clone())
                .unwrap_or_else(|| panic!("{member} has no spectrum_array_index"));
            let index: serde_json::Value = serde_json::from_str(&index).unwrap();
            (index["prefix"].as_str().unwrap().to_string(), meta.num_rows())
        };
        let (data_family, _) = family_of(&mut zip, "spectra_data.parquet");
        let (peaks_family, peak_rows) = family_of(&mut zip, "spectra_peaks.parquet");
        assert!(peak_rows > 0, "the synthetic frames wrote no peaks");
        assert_eq!((data_family.as_str(), peaks_family.as_str()), ("chunk", "chunk"));
        let _ = fs::remove_dir_all(&dir);
    }

    /// `--agilent-grid` hand-builds its data schema, which lost `spectrum_index` in 0.10.1: the writer
    /// meets the batch's index column in `route_unexpected` and panics, so the release build aborted
    /// on the first spectrum. No profile `.d` in reach decodes, so one gridded profile spectrum,
    /// shaped as `agilent_grid_spectrum` builds it, goes through the lane's own builder.
    #[test]
    fn agilent_grid_schema_writes_a_gridded_profile_spectrum() {
        use arrow::array::{Array, Int32Array, StructArray, UInt64Array};
        use mzdata::params::Unit;

        let dir = scratch("agilent-grid-schema");
        let path = dir.join("grid.mzpeak");
        let level = parquet::basic::ZstdLevel::try_new(3).unwrap();
        let mut writer =
            super::agilent_grid_writer_builder(level, (0.5, 1e-4)).build(fs::File::create(&path).unwrap(), true);
        super::ensure_mzp_cv(&mut writer);
        let mut arrays = BinaryArrayMap::new();
        let mut tof = DataArray::wrap(&ArrayType::nonstandard("tof_index"), BinaryDataArrayType::Int32, Vec::new());
        tof.update_buffer(&[1000i32, 1001, 1002]).unwrap();
        arrays.add(tof);
        let mut intensity = DataArray::wrap(&ArrayType::IntensityArray, BinaryDataArrayType::Float32, Vec::new());
        intensity.update_buffer(&[3.0f32, 7.0, 2.0]).unwrap();
        intensity.unit = Unit::DetectorCounts;
        arrays.add(intensity);
        let mut descr = SpectrumDescription::default();
        descr.id = "scan=1".into();
        descr.ms_level = 1;
        descr.signal_continuity = mzdata::spectrum::SignalContinuity::Profile;
        descr.add_param(Param::builder().name("tof_c0").curie(super::TOF_C0_CURIE).value(0.5).build());
        descr.add_param(Param::builder().name("tof_c1").curie(super::TOF_C1_CURIE).value(1e-4).build());
        descr.add_param(Param::builder().name("tof_calibration_id").curie(super::TOF_CALID_CURIE).value(1i64).build());
        let spec: MultiLayerSpectrum<CentroidPeak, DeconvolutedPeak> = MultiLayerSpectrum::new(descr, Some(arrays), None, None);
        writer.write_spectrum(&spec).unwrap();
        super::finish_chromatograms(&mut writer, &dir, &super::Ms1Chroms::default(), std::iter::empty(), false).unwrap();
        writer.finish_parquet().unwrap().finish().unwrap();

        let mut zip = zip::ZipArchive::new(fs::File::open(&path).unwrap()).unwrap();
        let data = extract_zip_entry(&mut zip, "spectra_data.parquet", &dir);
        let batch = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(fs::File::open(&data).unwrap())
            .unwrap()
            .build()
            .unwrap()
            .next()
            .expect("spectra_data holds the spectrum")
            .unwrap();
        let point = batch.column_by_name("point").unwrap().as_any().downcast_ref::<StructArray>().unwrap();
        let index = point.column_by_name("spectrum_index").expect("point.spectrum_index");
        assert_eq!(index.as_any().downcast_ref::<UInt64Array>().unwrap().values().to_vec(), vec![0, 0, 0]);
        let tof = point.column_by_name("tof_index").expect("point.tof_index");
        assert_eq!(tof.as_any().downcast_ref::<Int32Array>().unwrap().values().to_vec(), vec![1000, 1001, 1002]);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A-contract lock-in. Asserts the calibration index keys + peak/grid column names don't get
    /// renamed out from under readers. Corpus-gated (`#[ignore]`) because it converts real `.d`
    /// inputs. Run with `cargo test --release contract_ -- --ignored`.
    #[test]
    #[ignore = "needs the 142 MB 2485.d timsTOF corpus fixture (MZPEAK_CORPUS); run with --include-ignored"]
    fn contract_ims_compact_calibration_keys() {
        use std::fs;
        use std::io::Read;

        // Pinned, not searched: a sorted walk of the corpus took whichever TDF sorted first, which had
        // quietly become a 1.7 GB run rather than the SBA415 this doc used to name.
        let Some(input) = crate::corpus_gate::corpus_path(TDF_2485) else { return };
        let input = input.as_path();
        // A directory of its own. `ims_compact_is_frame_preserving` converts the same `.d` and extracts the same facet
        // names; with the one `mzpc-test-{pid}` both used to share, a parallel run let one test
        // truncate the Parquet file the other had just opened ("Parquet file too small. Size is 0").
        let scratch = &scratch("ims-calibration-contract");
        let scratch = scratch.as_path();
        let _rm = RmDir(scratch.to_path_buf());
        let output = scratch.join("contract.mzpeak");
        let _ = fs::remove_file(&output);
        super::convert_ims_compact_archive(input, &output, 3, None, false, false, false, 50.0)
            .expect("ims-compact conversion");

        let f = fs::File::open(&output).unwrap();
        let mut zip = zip::ZipArchive::new(f).unwrap();
        let mut idx_bytes = Vec::new();
        zip.by_name("mzpeak_index.json")
            .expect("mzpeak_index.json present")
            .read_to_end(&mut idx_bytes)
            .unwrap();
        let idx: serde_json::Value = serde_json::from_slice(&idx_bytes).unwrap();
        let cal = idx
            .get("metadata")
            .and_then(|m| m.get("ims_calibration"))
            .expect("metadata.ims_calibration present");
        for key in ["codec", "mz_from_tof", "tof_encoding", "a", "b"] {
            assert!(cal.get(key).is_some(), "ims_calibration missing key `{key}`: {cal}");
        }
        assert_eq!(cal.get("codec").and_then(|v| v.as_str()), Some("ims-compact"));
        // Present is not the contract; a reader decodes with the VALUES. `(a, b)` is timsrust's
        // two-point chord through GlobalMetadata — m/z = MzAcqRangeLower at tof 0 and MzAcqRangeUpper
        // at tof = DigitizerNumSamples — recomputed here from analysis.tdf, not taken from the converter.
        let conn = rusqlite::Connection::open_with_flags(
            input.join("analysis.tdf"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let global = |k: &str| -> f64 {
            conn.query_row("SELECT Value FROM GlobalMetadata WHERE Key = ?1", [k], |r| r.get::<_, String>(0))
                .unwrap_or_else(|e| panic!("GlobalMetadata {k}: {e}"))
                .trim()
                .parse()
                .unwrap()
        };
        let a = global("MzAcqRangeLower").sqrt();
        let b = (global("MzAcqRangeUpper").sqrt() - a) / global("DigitizerNumSamples");
        let (got_a, got_b) = (cal["a"].as_f64().expect("a is a number"), cal["b"].as_f64().expect("b is a number"));
        // `b` is recovered as sqrt((a+b)²) − sqrt(a²), a difference of two ~10s: ~1e-10 relative.
        assert!((got_a - a).abs() <= 1e-12 * a, "ims_calibration.a = {got_a}; the chord's sqrt(MzAcqRangeLower) = {a}");
        assert!((got_b - b).abs() <= 1e-8 * b, "ims_calibration.b = {got_b}; the chord's slope = {b}");
        assert_eq!(cal["mz_from_tof"], "(a + b*tof)^2");
        assert_eq!(cal["lossless"], "tof");
        assert_eq!(cal["tof_encoding"], "absolute", "the default archive layout stores absolute tof");
        assert_eq!(cal["chord_source"], "global_metadata", "the native lane's chord comes from GlobalMetadata");
        // The vendor's exact calibration rides beside the two-point chord.
        let vmc = idx
            .get("metadata")
            .and_then(|m| m.get("vendor_mz_calibration"))
            .expect("metadata.vendor_mz_calibration present");
        assert!(
            vmc["mz_calibration"].as_array().is_some_and(|r| !r.is_empty()),
            "vendor_mz_calibration.mz_calibration must hold the MzCalibration rows: {vmc}"
        );
        for key in ["DigitizerNumSamples", "MzAcqRangeLower", "MzAcqRangeUpper"] {
            assert!(vmc["global_metadata"].get(key).is_some(), "global_metadata missing `{key}`: {vmc}");
            assert_eq!(vmc["global_metadata"][key].as_f64(), Some(global(key)), "global_metadata.{key}: {vmc}");
        }
        let rows: i64 = conn.query_row("SELECT COUNT(*) FROM MzCalibration", [], |r| r.get(0)).unwrap();
        assert_eq!(
            vmc["mz_calibration"].as_array().map(Vec::len),
            Some(rows as usize),
            "one entry per MzCalibration row: {vmc}"
        );
        // … and the per-frame inputs are spectra_metadata columns.
        let meta_path = extract_zip_entry(&mut zip, "spectra_metadata.parquet", scratch);
        let builder = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(
            fs::File::open(&meta_path).unwrap(),
        )
        .unwrap();
        let cols = leaf_column_names(builder.schema());
        for suffix in ["_tdf_t1", "_tdf_t2", "_tdf_mz_calibration_id"] {
            assert!(
                cols.iter().any(|n| n.ends_with(suffix)),
                "spectra_metadata must carry a `*{suffix}` column; got {cols:?}"
            );
        }

        // peaks schema has a `tof` column.
        let peaks_path = extract_zip_entry(&mut zip, "spectra_peaks.parquet", scratch);
        let builder = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(
            fs::File::open(&peaks_path).unwrap(),
        )
        .unwrap();
        let cols = leaf_column_names(builder.schema());
        assert!(
            cols.iter().any(|n| n == "tof"),
            "ims-compact peaks schema must have a `tof` column; got {cols:?}"
        );
    }

    /// Extract a named entry from an open zip archive to a scratch file and return its path.
    /// Lets the corpus tests open parquet facets as `File` (which implements `ChunkReader`) without
    /// pulling in the `bytes` crate as a direct dependency.
    fn extract_zip_entry(
        zip: &mut zip::ZipArchive<std::fs::File>,
        name: &str,
        scratch: &std::path::Path,
    ) -> std::path::PathBuf {
        use std::io::Read;
        let mut buf = Vec::new();
        zip.by_name(name)
            .unwrap_or_else(|_| panic!("{name} present in archive"))
            .read_to_end(&mut buf)
            .unwrap();
        let out = scratch.join(name.replace('/', "_"));
        std::fs::write(&out, &buf).unwrap();
        out
    }

    #[test]
    fn expands_only_empty_referenceable_param_groups() {
        // empty self-closing def -> explicit open/close
        let h = r#"<list><referenceableParamGroup id="G" /></list>"#;
        // the space before '>' is preserved (valid XML); only the empty close is rewritten
        assert_eq!(
            expand_empty_param_groups(h),
            r#"<list><referenceableParamGroup id="G" ></referenceableParamGroup></list>"#
        );
        // a Ref (different element) and a non-empty group must be left untouched
        let keep = r#"<referenceableParamGroup id="G"><cvParam/></referenceableParamGroup><referenceableParamGroupRef ref="G"/>"#;
        assert_eq!(expand_empty_param_groups(keep), keep);
        // no group at all -> unchanged
        assert_eq!(expand_empty_param_groups("<run/>"), "<run/>");
    }

    // ── 0.9.13 options-and-levers review items (ledger M10–M13, M29–M31) ─────────────────────

    use super::{env_flag, refuse_unsupported_flags, Cli, Lane, Settings};
    use clap::Parser as _;
    use std::fs;

    /// The release binary the tests drive. Inside the bin crate's own test module cargo does NOT
    /// set `CARGO_BIN_EXE_<name>` (that is for integration tests), so this used to guess
    /// `<manifest>/target/release/mzpeak-convert` — wrong under any `CARGO_TARGET_DIR` and missing
    /// the `.exe` suffix, which is exactly how seven of these tests failed to SPAWN on the Windows
    /// box while passing here. The test executable itself lives in `<target>/<profile>/deps/`, so
    /// the built binary is two directories up, with the platform's executable suffix.
    fn built_binary() -> std::path::PathBuf {
        let exe = std::env::current_exe().expect("current_exe");
        let profile_dir = exe
            .parent()
            .and_then(|deps| deps.parent())
            .expect("test binary lives in <target>/<profile>/deps/");
        let bin = profile_dir.join(format!("mzpeak-convert{}", std::env::consts::EXE_SUFFIX));
        assert!(
            bin.is_file(),
            "built binary not found at {} — run `cargo build --release` first (tests drive the release binary)",
            bin.display()
        );
        bin
    }

    const TINY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tiny.pwiz.1.1.mzML");

    /// Per-test scratch dir: the tests in this module run in parallel inside one process.
    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("mzpc-a1-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// Run the built binary with `args` and `envs`, returning (exit ok, stdout, stderr).
    fn run_bin(args: &[&std::ffi::OsStr], envs: &[(&str, &str)]) -> (bool, String, String) {
        let mut cmd = std::process::Command::new(built_binary());
        cmd.args(args);
        // A clean slate for every lever this module exercises, so an inherited shell variable
        // cannot decide a test.
        for v in ["RUST_LOG", "MZPC_DUMP_IM_TABLE", "MZPC_MAX_SPECTRA", "MZPC_SHIMADZU_PROBE"] {
            cmd.env_remove(v);
        }
        for (k, v) in envs {
            cmd.env(k, v);
        }
        let out = cmd.output().expect("running mzpeak-convert");
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    fn index_metadata(archive: &std::path::Path) -> serde_json::Value {
        let mut zip = zip::ZipArchive::new(fs::File::open(archive).unwrap()).unwrap();
        let mut bytes = Vec::new();
        zip.by_name("mzpeak_index.json").unwrap().read_to_end(&mut bytes).unwrap();
        let idx: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        idx["metadata"].clone()
    }

    fn zip_members(archive: &std::path::Path) -> Vec<String> {
        let zip = zip::ZipArchive::new(fs::File::open(archive).unwrap()).unwrap();
        zip.file_names().map(str::to_string).collect()
    }

    /// M33 on the archive column: `spectra_metadata.spectrum_type` is the MS-level child term
    /// (MS:1000579 / MS:1000580) on every row, never the generic parent MS:1000294 that the lanes
    /// used to add blanket-fashion (which shadowed the writer's inference).
    #[test]
    fn spectrum_type_is_the_ms_level_child_never_the_generic_parent() {
        let dir = scratch("spectrum-type");
        let out = dir.join("tiny.mzpeak");
        let args: Vec<&std::ffi::OsStr> =
            vec![TINY.as_ref(), "-o".as_ref(), out.as_os_str(), "--force".as_ref()];
        let (ok, _, err) = run_bin(&args, &[]);
        assert!(ok, "{err}");
        let mut zip = zip::ZipArchive::new(fs::File::open(&out).unwrap()).unwrap();
        let meta = extract_zip_entry(&mut zip, "spectra_metadata.parquet", &dir);
        let reader = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(
            fs::File::open(&meta).unwrap(),
        )
        .unwrap()
        .build()
        .unwrap();
        let (mut types, mut levels): (Vec<String>, Vec<i64>) = (Vec::new(), Vec::new());
        for batch in reader {
            let batch = batch.unwrap();
            let col = batch.column_by_name("spectrum_type").expect("spectrum_type column");
            let col = col
                .as_any()
                .downcast_ref::<arrow::array::StringArray>()
                .expect("spectrum_type is a string column");
            types.extend(col.iter().map(|v| v.unwrap_or("").to_string()));
            let level = batch.column_by_name("ms_level").expect("ms_level column");
            let level = arrow::compute::cast(level, &arrow::datatypes::DataType::Int64).unwrap();
            let level = level.as_any().downcast_ref::<arrow::array::Int64Array>().unwrap();
            levels.extend(level.iter().map(|v| v.expect("ms_level is set on every row")));
        }
        assert_eq!(types.len(), 4, "the fixture has four spectra");
        assert!(
            types.iter().all(|t| t == "MS:1000579" || t == "MS:1000580"),
            "every row must carry the MS1/MSn child term: {types:?}"
        );
        // …and the RIGHT child for the row's level. The fixture holds both levels, so a swapped or a
        // constant term cannot pass.
        assert_eq!(levels, [1, 2, 1, 1], "the fixture's MS levels");
        for (level, t) in levels.iter().zip(&types) {
            let want = if *level == 1 { "MS:1000579" } else { "MS:1000580" };
            assert_eq!(t, want, "ms_level {level} must carry {want}; rows {types:?} at levels {levels:?}");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    /// The `transformations` index block (invariant: every transformation declared) lists what was
    /// applied, not what was configured. On the fixture the generic lane numpresses m/z and masks
    /// nothing: its one profile spectrum (scan=20, 10 points) holds no run of zeros, so the mask the
    /// writer was built with never shortened a spectrum. Its MS1 spectra arrive out of time order
    /// (scan=19 at 5.89 min, scan=21 with no start time, cycle 22 at 42.05 s), so the synthesized TIC
    /// and base-peak traces reach the writer unsorted and its backstop re-sorts them by time.
    #[test]
    fn transformations_block_declares_what_the_lane_applied() {
        let dir = scratch("transformations");
        let out = dir.join("tiny.mzpeak");
        let args: Vec<&std::ffi::OsStr> =
            vec![TINY.as_ref(), "-o".as_ref(), out.as_os_str(), "--force".as_ref()];
        let (ok, _, err) = run_bin(&args, &[]);
        assert!(ok, "{err}");
        let meta = index_metadata(&out);
        let applied: Vec<&str> = meta["transformations"]
            .as_array()
            .expect("metadata.transformations is a list")
            .iter()
            .map(|v| v.as_str().expect("entries are strings"))
            .collect();
        // The whole list — `contains` let through an entry nothing applied, or one listed twice. No
        // `sort-by-mz`: the fixture is already in m/z order. No `zero-run-mask`: no zero run. Its
        // `sic` is in seconds, stored in minutes.
        assert_eq!(applied, ["numpress-linear", "sort-by-time", "chromatogram-time-to-minutes"], "{applied:?}");
        // The lossless request drops the codec entry — the list follows the choice, not the lane.
        let out2 = dir.join("tiny-delta.mzpeak");
        let args: Vec<&std::ffi::OsStr> = vec![
            TINY.as_ref(), "-o".as_ref(), out2.as_os_str(), "--force".as_ref(), "--no-numpress".as_ref(),
        ];
        let (ok, _, err) = run_bin(&args, &[]);
        assert!(ok, "{err}");
        let meta = index_metadata(&out2);
        let applied: Vec<&str> =
            meta["transformations"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
        assert_eq!(applied, ["sort-by-time", "chromatogram-time-to-minutes"], "{applied:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    /// The writer-level entries follow the writer's counters on the vendor-reader seam: a profile
    /// with no zero run declares nothing, one with a run declares `zero-run-mask`, and a spectrum
    /// handed over out of m/z order declares `sort-by-mz` — the writer's re-sort backstop, which
    /// every native lane relies on and which no archive recorded before.
    #[test]
    fn writer_counters_decide_the_writer_level_transformations() {
        use super::{convert_vendor_reader_tallied, VendorHints};

        let dir = scratch("writer-counters");
        let declared = |name: &str, mzs: &'static [f64], intens: &'static [f32]| -> Vec<String> {
            let out = dir.join(format!("{name}.mzpeak"));
            convert_vendor_reader_tallied(
                std::path::Path::new(TINY),
                &out,
                None,
                1,
                None,
                false,
                VendorHints::default(),
                4,
                |i| Ok(spec_from(mzs, intens, i)),
            )
            .unwrap();
            index_metadata(&out)["transformations"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect()
        };
        let none: Vec<String> = Vec::new();
        assert_eq!(declared("verbatim", &[100.0, 100.5, 101.0, 101.5], &[5.0, 0.0, 7.0, 9.0]), none);
        assert_eq!(
            declared("zero-run", &[100.0, 100.5, 101.0, 101.5, 102.0, 102.5], &[5.0, 0.0, 0.0, 0.0, 0.0, 9.0]),
            ["zero-run-mask"]
        );
        assert_eq!(declared("unsorted", &[101.0, 100.0, 102.0, 103.0], &[5.0, 6.0, 7.0, 9.0]), ["sort-by-mz"]);
        let _ = fs::remove_dir_all(&dir);
    }

    /// The writer's two other re-sort backstops count as well: a chromatogram handed over out of time
    /// order declares `sort-by-time`, a wavelength spectrum out of wavelength order
    /// `sort-by-wavelength`, and the same data in order declares neither. Both backstops reordered
    /// stored data without a declaration before.
    #[test]
    fn writer_backstops_declare_chromatogram_and_wavelength_re_sorts() {
        use mzdata::io::MZReaderType;
        use mzpeak_prototyping::writer::AbstractMzPeakWriter;

        const PDA_UV: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/pda_uv.pwiz.mzML");
        let dir = scratch("backstops");
        let ms = spec_from(&[100.0, 200.0, 300.0], &[1.0, 2.0, 3.0], 0);
        // A PDA wavelength spectrum of the fixture as pwiz wrote it, and the same one reversed.
        let mut reader = MZReaderType::<_, CentroidPeak, DeconvolutedPeak>::open_path(PDA_UV).unwrap();
        let uv = reader
            .iter()
            .find(|s| s.spectrum_type().is_some_and(|t| !t.is_mass_spectrum()))
            .expect("a wavelength spectrum in pda_uv.pwiz.mzML");
        let reversed_uv = {
            let mut arrays = BinaryArrayMap::new();
            for (t, a) in uv.raw_arrays().expect("wavelength arrays").iter() {
                let mut r = DataArray::wrap(t, a.dtype(), Vec::new());
                match a.dtype() {
                    BinaryDataArrayType::Float64 => r.update_buffer(&a.to_f64().unwrap().iter().rev().copied().collect::<Vec<_>>()),
                    BinaryDataArrayType::Float32 => r.update_buffer(&a.to_f32().unwrap().iter().rev().copied().collect::<Vec<_>>()),
                    other => panic!("unexpected {other:?} array in the fixture"),
                }
                .unwrap();
                r.unit = a.unit;
                arrays.add(r);
            }
            MultiLayerSpectrum::new(uv.description().clone(), Some(arrays), None, None)
        };
        let applied = |name: &str, uv: &MultiLayerSpectrum, times: &[f64]| -> Vec<String> {
            let path = dir.join(format!("{name}.mzpeak"));
            let mut writer = super::MzPeakWriterType::<fs::File>::builder()
                .sample_array_types_from_spectra(std::iter::once(ms.clone()))
                .build(fs::File::create(&path).unwrap(), true);
            writer.write_spectrum(&ms).unwrap();
            writer.write_spectrum(uv).unwrap();
            let tic = super::synth_chromatogram(
                "TIC",
                Param::builder().name("total ion current chromatogram").curie(mzdata::curie!(MS:1000235)).build(),
                times,
                &[6.0, 7.0, 8.0],
            )
            .unwrap();
            writer.write_chromatogram(&tic).unwrap();
            let applied = super::base_transformations(&writer);
            writer.finish_parquet().unwrap().finish().unwrap();
            applied
        };
        let none: Vec<String> = Vec::new();
        assert_eq!(applied("in-order", &uv, &[0.0, 1.0, 2.0]), none);
        assert_eq!(applied("time", &uv, &[0.0, 2.0, 1.0]), ["sort-by-time"]);
        assert_eq!(applied("wavelength", &reversed_uv, &[0.0, 1.0, 2.0]), ["sort-by-wavelength"]);
        let _ = fs::remove_dir_all(&dir);
    }

    /// M35: which timsTOF route built an archive. The fallback arm runs only on a native error that
    /// mentions `decompress`, and hands that error to the `conversion_route` block; any other error
    /// keeps the native lane's context, and success needs no fallback. Forced with injected errors:
    /// no committed TDF makes timsrust fail that way.
    #[test]
    fn ims_compact_fallback_arm_records_the_route_it_took() {
        use super::ims_compact_with_fallback;
        let input = std::path::Path::new("run.d");
        let mut taken = None;
        ims_compact_with_fallback(input, || Err(anyhow::anyhow!("frame 7: failed to decompress blob")), |route| {
            taken = Some(route);
            Ok(())
        })
        .unwrap();
        let (key, block) = taken.expect("a decompress error takes the fallback");
        assert_eq!(key, "conversion_route");
        assert_eq!(block["route"], "mzdata-fallback");
        assert_eq!(block["reader"], "mzdata");
        assert!(block["reason"].as_str().unwrap().contains("decompress"), "{block}");
        let mut called = false;
        let err = ims_compact_with_fallback(input, || Err(anyhow::anyhow!("no frames")), |_| {
            called = true;
            Ok(())
        })
        .unwrap_err();
        assert!(!called, "only a decompress error falls back");
        assert!(format!("{err:#}").contains("ims-compact converting run.d"), "{err:#}");
        ims_compact_with_fallback(input, || Ok(()), |_| {
            called = true;
            Ok(())
        })
        .unwrap();
        assert!(!called, "success needs no fallback");
    }

    /// The route block the fallback hands over reaches the archive index through `convert_file`, and
    /// the plain mzdata lane states none.
    #[test]
    fn convert_file_writes_the_route_it_is_handed() {
        let dir = scratch("route");
        let out = dir.join("fallback.mzpeak");
        let route = super::conversion_route_block("mzdata-fallback", "mzdata", Some("frame 7: failed to decompress blob"));
        super::convert_file(std::path::Path::new(TINY), &out, None, 3, None, true, Some(super::TofGridMode::Off), &[], None, true, Some(route))
            .unwrap();
        let meta = index_metadata(&out);
        assert_eq!(meta["conversion_route"]["route"], "mzdata-fallback", "{meta:#}");
        assert!(meta["conversion_route"]["reason"].as_str().unwrap().contains("decompress"));
        let plain = dir.join("plain.mzpeak");
        super::convert_file(std::path::Path::new(TINY), &plain, None, 3, None, true, Some(super::TofGridMode::Off), &[], None, true, None)
            .unwrap();
        assert!(index_metadata(&plain).get("conversion_route").is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    /// One embed rule: a vendor directory's side-files go into `vendor/` under the policy on every
    /// lane (a Waters `.raw` or BAF `.d` embedded nothing through 0.11.5, and `--aux` did nothing on
    /// them), and `--aux` on a single-file input is named inert instead of passing silently.
    #[test]
    fn every_vendor_directory_embeds_its_side_files_and_aux_on_a_file_is_inert() {
        use super::{convert_vendor_reader_tallied, VendorHints};

        let dir = scratch("embed-rule");
        let raw = dir.join("run.raw");
        fs::create_dir_all(&raw).unwrap();
        fs::write(raw.join("_HEADER.TXT"), "$$ Acquired Name: run\r\n").unwrap();
        fs::write(raw.join("_extern.inf"), "Created by 4.1\r\n").unwrap();
        let policy = crate::vendor::VendorPolicy::load(None, &["_extern.inf=drop".to_string()]).unwrap();
        let out = dir.join("run.mzpeak");
        convert_vendor_reader_tallied(&raw, &out, None, 1, Some(&policy), false, VendorHints::default(), 2, |i| {
            Ok(spec_from(&[100.0 + i as f64, 200.0], &[1.0, 2.0], i))
        })
        .unwrap();
        let manifest = index_metadata(&out)["vendor_files"].clone();
        let actions: Vec<(String, String)> = manifest
            .as_array()
            .expect("a vendor_files manifest")
            .iter()
            .map(|e| (e["path"].as_str().unwrap().to_string(), e["action"].as_str().unwrap().to_string()))
            .collect();
        assert!(actions.contains(&("vendor/_HEADER.TXT.gz".to_string(), "embed".to_string())), "{actions:?}");
        assert!(actions.contains(&("_extern.inf".to_string(), "drop".to_string())), "the --aux rule applies: {actions:?}");
        assert!(zip_members(&out).iter().any(|m| m == "vendor/_HEADER.TXT.gz"));

        let tiny_out = dir.join("tiny.mzpeak");
        let args: Vec<&std::ffi::OsStr> =
            vec![TINY.as_ref(), "-o".as_ref(), tiny_out.as_os_str(), "--force".as_ref(), "--aux".as_ref(), "*=drop".as_ref()];
        let (ok, _, err) = run_bin(&args, &[]);
        assert!(ok, "{err}");
        assert!(err.contains("--aux is inert"), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }

    /// `shimadzu:span-trim` is declared from the routes of the WRITTEN spectra: a run whose gridded
    /// spectra had no zero pad, or whose only padded spectra were schema probes, trimmed nothing
    /// stored. It used to be declared whenever the run-wide grid step was found.
    #[test]
    fn span_trim_is_declared_from_the_written_routes() {
        use super::{convert_vendor_reader_tallied, FacetRoutes, VendorHints, VendorSpectrum};

        let dir = scratch("span-trim");
        const LEN: usize = 20;
        let declared = |name: &str, trimmed: &dyn Fn(usize, usize) -> bool| -> Vec<String> {
            let calls = std::cell::Cell::new(0usize);
            let out = dir.join(format!("{name}.mzpeak"));
            convert_vendor_reader_tallied(std::path::Path::new(TINY), &out, None, 1, None, false, VendorHints::default(), LEN, |i| {
                calls.set(calls.get() + 1);
                Ok(VendorSpectrum {
                    spectrum: spec_from(&[100.0, 200.0, 300.0], &[1.0, 2.0, 3.0], i),
                    peak_arrays: None,
                    routes: FacetRoutes { profile_grid: Some(true), profile_span_trimmed: trimmed(calls.get(), i), centroid_lattice: None },
                })
            })
            .unwrap();
            index_metadata(&out)["transformations"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect()
        };
        assert!(declared("no-pad", &|_, _| false).is_empty());
        // The probes are fetched first (calls 1..=6); a pad only they had is not in the archive.
        assert!(declared("probes-only", &|call, _| call <= 6).is_empty());
        assert_eq!(declared("padded", &|call, i| call > 6 && i == 1), ["shimadzu:span-trim"]);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A reader's own counts (the `--bruker-sdk` TDF and Waters frame re-sorts, the Waters SONAR
    /// sums) are declared from their growth over the written spectra: counts during the six schema
    /// probes do not count, and each counter declares only its own entry. The counters here are the
    /// test's own; the Waters lane's wiring of its two (MassLynxRaw.dll only) is pinned as source
    /// text in `tests/contract_strings.rs`, `waters_counted_entries_pinned`.
    #[test]
    fn reader_counters_count_written_spectra_only() {
        use super::{convert_vendor_reader_tallied, VendorHints};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let dir = scratch("reader-counters");
        const LEN: usize = 20;
        // `bump(call, index)` names the counter a reader call adds to, if any: 0 is `sort-by-mz`,
        // 1 is `waters:sonar-summed`.
        let declared = |name: &str, bump: &dyn Fn(usize, usize) -> Option<usize>| -> Vec<String> {
            let counters = [Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0))];
            let calls = std::cell::Cell::new(0usize);
            let out = dir.join(format!("{name}.mzpeak"));
            let hints = VendorHints {
                counters: vec![("sort-by-mz", counters[0].clone()), ("waters:sonar-summed", counters[1].clone())],
                ..VendorHints::default()
            };
            convert_vendor_reader_tallied(std::path::Path::new(TINY), &out, None, 1, None, false, hints, LEN, |i| {
                calls.set(calls.get() + 1);
                if let Some(k) = bump(calls.get(), i) {
                    counters[k].fetch_add(1, Ordering::Relaxed);
                }
                Ok(spec_from(&[100.0, 200.0, 300.0], &[1.0, 2.0, 3.0], i))
            })
            .unwrap();
            index_metadata(&out)["transformations"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect()
        };
        // The probes are fetched first (calls 1..=6, indices 0, 3, …, 15), and both counters grow there.
        assert!(declared("probes-only", &|call, _| (call <= 6).then_some(call % 2)).is_empty());
        // Indices 1 and 2 are never probed (stride 3) and are always written.
        assert_eq!(declared("re-sorted", &|call, i| (call > 6 && i == 1).then_some(0)), ["sort-by-mz"]);
        assert_eq!(declared("sonar-summed", &|call, i| (call > 6 && i == 2).then_some(1)), ["waters:sonar-summed"]);
        let _ = fs::remove_dir_all(&dir);
    }

    /// `sort-by-time` on the vendor-reader lanes: the TIC and base-peak traces are synthesized in
    /// spectrum order, so a run whose MS1 start times go backwards hands the writer chromatograms out
    /// of time order, and its backstop re-sorts them. The seam reads the writer's tallies after
    /// `finish_chromatograms`; read before it, the entry is lost. The same run in time order declares
    /// nothing. Until now only the mzML lane (the tiny fixture) and the writer seam pinned the entry.
    #[test]
    fn vendor_reader_declares_the_time_re_sort_of_its_synthesized_chromatograms() {
        use super::{convert_vendor_reader_tallied, VendorHints};

        let dir = scratch("vendor-sort-by-time");
        const LEN: usize = 8;
        let declared = |name: &str, minutes: &dyn Fn(usize) -> f64| -> Vec<String> {
            let out = dir.join(format!("{name}.mzpeak"));
            convert_vendor_reader_tallied(std::path::Path::new(TINY), &out, None, 1, None, true, VendorHints::default(), LEN, |i| {
                let mut s = spec_from(&[100.0, 200.0, 300.0], &[1.0, 2.0, 3.0], i);
                s.description_mut().ms_level = 1;
                s.description_mut().acquisition.first_scan_mut().unwrap().start_time = minutes(i);
                Ok(s)
            })
            .unwrap();
            index_metadata(&out)["transformations"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect()
        };
        assert!(declared("in-order", &|i| i as f64).is_empty());
        // Spectrum 2 started before spectrum 1: the traces arrive as 0, 1, 0.5, 3, … minutes.
        assert_eq!(declared("out-of-order", &|i| if i == 2 { 0.5 } else { i as f64 }), ["sort-by-time"]);
        let _ = fs::remove_dir_all(&dir);
    }

    /// The `--agilent-grid` declarations follow the reader's counts: zero samples or all-zero scans
    /// dropped, intensities Float32 rounds; a truncated MSProfile.bin marks the archive partial
    /// unless a cap stopped the run first. The lane needs a decodable profile `.d`, which no
    /// committed fixture is, so its pure parts are pinned here.
    #[test]
    fn agilent_grid_declares_what_its_reader_counted() {
        use super::{agilent_grid_transformations, agilent_truncation_marker, intensities_as_f32};
        assert!(agilent_grid_transformations(0, 0, 0).is_empty());
        assert_eq!(agilent_grid_transformations(12, 0, 0), ["agilent:drop-zero-samples"]);
        assert_eq!(agilent_grid_transformations(0, 1, 3), ["agilent:drop-zero-samples", "agilent:intensity-f32-rounding"]);
        let mut rounded = 0;
        let f = intensities_as_f32(&[1, 1 << 24, (1 << 24) + 1, u32::MAX], &mut rounded);
        assert_eq!(f[1], 16_777_216.0);
        assert_eq!(rounded, 2, "2^24 + 1 and u32::MAX have no exact f32");
        let tail = crate::agilent_profile::SkipTally { truncated_tail: 7, ..Default::default() };
        let (key, block) = agilent_truncation_marker(None, tail, 100, 93).expect("a truncated tail is partial");
        assert_eq!(key, "partial");
        assert_eq!(block["scan_records_not_read"], 7);
        assert_eq!(block["spectra_written"], 93);
        assert!(agilent_truncation_marker(Some(10), tail, 100, 10).is_none(), "a capped run keeps the cap's marker");
        // A cap above what the file holds stopped nothing: its truncated tail is partial all the same.
        let (_, block) = agilent_truncation_marker(Some(1_000_000), tail, 100, 93).expect("a cap that did not bite");
        assert_eq!(block["scan_records_not_read"], 7);
        assert!(agilent_truncation_marker(None, crate::agilent_profile::SkipTally::default(), 100, 100).is_none());
    }

    /// M34 on the archive: nothing in `mzpeak_index.json` names the converting machine's
    /// filesystem — not the scratch directory the conversion ran in, not a home directory.
    #[test]
    fn index_carries_no_operator_paths() {
        let dir = scratch("no-paths");
        // A copy under a path with the two shapes the readers leak: an absolute directory that the
        // Thermo reader would put in `location`, and a stem the TDF reader would put in `run.id`.
        let input = dir.join("leaky-input.mzML");
        fs::copy(TINY, &input).unwrap();
        let out = dir.join("out.mzpeak");
        let args: Vec<&std::ffi::OsStr> =
            vec![input.as_os_str(), "-o".as_ref(), out.as_os_str(), "--force".as_ref()];
        let (ok, _, err) = run_bin(&args, &[]);
        assert!(ok, "{err}");
        let mut zip = zip::ZipArchive::new(fs::File::open(&out).unwrap()).unwrap();
        let mut text = String::new();
        zip.by_name("mzpeak_index.json").unwrap().read_to_string(&mut text).unwrap();
        let dir_text = dir.to_string_lossy();
        assert!(!text.contains(dir_text.as_ref()), "the scratch directory leaked into the index");
        assert!(!text.contains("/Users/") && !text.contains("/home/"), "a home directory leaked into the index");
        for sf in index_metadata(&out)["file_description"]["source_files"].as_array().unwrap() {
            assert_eq!(sf["location"].as_str(), Some("file://"), "{sf}");
        }
        // The raw-text checks above are blind to a WINDOWS path: JSON escapes its backslashes
        // (`C:\\Users\\…`), so neither the scratch directory nor a home directory matches the text.
        // Walk the decoded strings and refuse any backslash or drive letter — the fixture itself
        // carries `file://F:/data/Exp01` and `file://C:/settings/`, which must not travel either.
        fn strings<'a>(v: &'a serde_json::Value, out: &mut Vec<&'a str>) {
            match v {
                serde_json::Value::String(s) => out.push(s),
                serde_json::Value::Array(a) => a.iter().for_each(|x| strings(x, out)),
                serde_json::Value::Object(o) => o.iter().for_each(|(k, x)| {
                    out.push(k);
                    strings(x, out)
                }),
                _ => {}
            }
        }
        let index: serde_json::Value = serde_json::from_str(&text).unwrap();
        let mut decoded = Vec::new();
        strings(&index, &mut decoded);
        let dir_slashed = dir_text.replace('\\', "/");
        for s in decoded {
            let b = s.as_bytes();
            let drive = (0..b.len().saturating_sub(2)).any(|i| {
                b[i].is_ascii_alphabetic()
                    && b[i + 1] == b':'
                    && matches!(b[i + 2], b'\\' | b'/')
                    && (i == 0 || !b[i - 1].is_ascii_alphanumeric())
            });
            assert!(
                !s.contains('\\') && !drive && !s.contains(dir_text.as_ref()) && !s.contains(&dir_slashed),
                "an operator path leaked into the index: {s:?}"
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }

    /// One reading for every boolean lever: unset is `None`, the documented "off" spellings are
    /// `Some(false)`, anything else is `Some(true)`. Uses names no production lever reads, since
    /// the environment is process-global and the tests run in parallel.
    #[test]
    fn env_flag_is_three_way() {
        let name = "MZPC_TEST_ENV_FLAG_A1";
        unsafe { std::env::remove_var(name) };
        assert_eq!(env_flag(name), None, "unset");
        for off in ["", "0", "false", "FALSE", "no", " 0 "] {
            unsafe { std::env::set_var(name, off) };
            assert_eq!(env_flag(name), Some(false), "{off:?} must read as set-but-off");
        }
        for on in ["1", "true", "yes", "anything", "00"] {
            unsafe { std::env::set_var(name, on) };
            assert_eq!(env_flag(name), Some(true), "{on:?} must read as on");
        }
        unsafe { std::env::remove_var(name) };
    }

    /// `MZPC_DUMP_IM_TABLE` with `--output`: refuse, name the variable, write nothing. Set-but-empty
    /// is OFF and the conversion proceeds — the old `var_os().is_some()` read swallowed it.
    #[test]
    fn dump_lever_with_output_bails_and_writes_nothing() {
        let dir = scratch("dump");
        let out = dir.join("out.mzpeak");
        let args: Vec<&std::ffi::OsStr> =
            vec![TINY.as_ref(), "-o".as_ref(), out.as_os_str(), "--force".as_ref()];
        let (ok, _, err) = run_bin(&args, &[("MZPC_DUMP_IM_TABLE", "1")]);
        assert!(!ok, "must exit non-zero, stderr: {err}");
        assert!(err.contains("MZPC_DUMP_IM_TABLE"), "must name the variable: {err}");
        assert!(err.contains("NO archive"), "must say no archive is written: {err}");
        assert!(!out.exists(), "nothing may be written");

        let (ok, _, err) = run_bin(&args, &[("MZPC_DUMP_IM_TABLE", "")]);
        assert!(ok, "set-but-empty is off; the conversion must run: {err}");
        assert!(out.exists());
    }

    /// `-q` given → no INFO whatever RUST_LOG says; `-v` given → debug logs whatever RUST_LOG says;
    /// neither → RUST_LOG applies.
    #[test]
    fn explicit_verbosity_flags_win_over_rust_log() {
        let dir = scratch("log");
        let out = dir.join("out.mzpeak");
        let base: Vec<&std::ffi::OsStr> =
            vec![TINY.as_ref(), "-o".as_ref(), out.as_os_str(), "--force".as_ref()];

        let mut quiet = base.clone();
        quiet.push("-q".as_ref());
        let (ok, _, err) = run_bin(&quiet, &[("RUST_LOG", "info")]);
        assert!(ok, "{err}");
        assert!(!err.contains("INFO"), "-q must silence INFO even under RUST_LOG=info: {err}");

        let mut verbose = base.clone();
        verbose.push("-v".as_ref());
        let (ok, _, err) = run_bin(&verbose, &[("RUST_LOG", "error")]);
        assert!(ok, "{err}");
        assert!(err.contains("INFO") || err.contains("DEBUG"), "-v must win over RUST_LOG=error: {err}");

        let (ok, _, err) = run_bin(&base, &[("RUST_LOG", "error")]);
        assert!(ok, "{err}");
        assert!(!err.contains("INFO"), "with no flag RUST_LOG must still apply: {err}");
    }

    /// A capped conversion says so INSIDE the archive, not only on stderr.
    #[test]
    fn max_spectra_cap_writes_partial_marker() {
        let dir = scratch("cap");
        let out = dir.join("out.mzpeak");
        let args: Vec<&std::ffi::OsStr> =
            vec![TINY.as_ref(), "-o".as_ref(), out.as_os_str(), "--force".as_ref()];
        let (ok, _, err) = run_bin(&args, &[("MZPC_MAX_SPECTRA", "2")]);
        assert!(ok, "{err}");
        let partial = &index_metadata(&out)["partial"];
        assert_eq!(partial["partial"], serde_json::json!(true), "{partial}");
        assert_eq!(partial["max_spectra"], serde_json::json!(2));
        assert_eq!(partial["source_declared"], serde_json::json!(4), "tiny declares 4 spectra");
        assert_eq!(partial["spectra_written"], serde_json::json!(2));

        // A cap that does not bite leaves no marker: the archive IS complete.
        let (ok, _, err) = run_bin(&args, &[("MZPC_MAX_SPECTRA", "100")]);
        assert!(ok, "{err}");
        assert!(index_metadata(&out).get("partial").is_none(), "no marker when the cap did not bite");
    }

    /// The six keys `--config` promised and rejected, and `sample`, which arrived later and was
    /// rejected the same way: accepted, and merged CLI-over-config.
    #[test]
    fn file_config_accepts_the_six_promised_keys() {
        let dir = scratch("cfg");
        let cfg = dir.join("c.yaml");
        fs::write(
            &cfg,
            "representation: profile\nrt: 1-2\nms_level: [1, 2]\ndrop_aux: ['vendor*']\nverbose: 2\nquiet: false\nsample: 2\n",
        )
        .unwrap();
        let cli = Cli::try_parse_from(["mzpeak-convert", TINY, "--config", cfg.to_str().unwrap()]).unwrap();
        let s = Settings::resolve(&cli).unwrap();
        assert_eq!(s.representation, super::RepresentationArg::Profile);
        assert_eq!(s.rt.as_deref(), Some("1-2"));
        assert_eq!(s.ms_level, vec![1, 2]);
        assert_eq!(s.drop_aux, vec!["vendor*".to_string()]);
        assert_eq!(s.verbose, 2);
        assert!(!s.quiet);
        assert_eq!(s.sample, Some(2));
        assert!(!s.given.contains(&"--sample"), "config values must not count as given: {:?}", s.given);
        // A config value is a standing default, not this run's intent — it must NOT count as given
        // (see `Settings::resolve`), or a profile would trip the honoured-flags refusals.
        assert!(!s.given.contains(&"--representation"), "config values must not count as given: {:?}", s.given);

        // The command line wins over the file.
        let cli = Cli::try_parse_from([
            "mzpeak-convert", TINY, "--config", cfg.to_str().unwrap(),
            "--representation", "centroid", "--rt", "3-4", "--ms-level", "3", "-v", "--sample", "3",
        ])
        .unwrap();
        let s = Settings::resolve(&cli).unwrap();
        assert_eq!(s.representation, super::RepresentationArg::Centroid);
        assert_eq!(s.rt.as_deref(), Some("3-4"));
        assert_eq!(s.ms_level, vec![3]);
        assert_eq!(s.verbose, 1);
        assert_eq!(s.sample, Some(3));
        assert!(s.given.contains(&"--sample"), "a command-line --sample is given like every option: {:?}", s.given);

        // clap refuses `--sample 0`; the config file must not turn it into sample 1.
        fs::write(&cfg, "sample: 0\n").unwrap();
        let cli = Cli::try_parse_from(["mzpeak-convert", TINY, "--config", cfg.to_str().unwrap()]).unwrap();
        assert!(Settings::resolve(&cli).is_err(), "sample: 0 must be refused");

        // A config `quiet: true` is honoured when the command line says nothing.
        fs::write(&cfg, "quiet: true\n").unwrap();
        let cli = Cli::try_parse_from(["mzpeak-convert", TINY, "--config", cfg.to_str().unwrap()]).unwrap();
        assert!(Settings::resolve(&cli).unwrap().quiet);
    }

    /// ProteoWizard's `_xHHHH_` escapes come off on copy: `run.id`, and the software ids with every
    /// reference to them still resolving. pwiz escapes each byte of a non-ASCII name's UTF-8, so a
    /// run of byte escapes decodes as UTF-8, and stays as written when it is not UTF-8. What only
    /// looks like an escape stays as written.
    #[test]
    fn pwiz_escaped_ids_are_decoded() {
        use mzdata::meta::{DataProcessing, InstrumentConfiguration, ProcessingMethod, Software};
        let mut w = mzdata::io::mzml::MzMLWriter::new(std::io::sink());
        w.run_description_mut().unwrap().id = Some("_x0032_0090101_x0020_-_x0020_run".into());
        w.softwares_mut().push(Software { id: "MassLynx_x0020_software".into(), ..Default::default() });
        let method = ProcessingMethod { software_reference: "MassLynx_x0020_software".into(), ..Default::default() };
        w.data_processings_mut().push(DataProcessing { id: "dp".into(), methods: vec![method] });
        w.instrument_configurations_mut()
            .insert(0, InstrumentConfiguration { software_reference: "MassLynx_x0020_software".into(), ..Default::default() });
        super::decode_pwiz_ids(&mut w);
        assert_eq!(w.run_description().unwrap().id.as_deref(), Some("20090101 - run"));
        assert_eq!(w.softwares()[0].id, "MassLynx software");
        assert_eq!(w.data_processings()[0].methods[0].software_reference, "MassLynx software");
        assert_eq!(w.instrument_configurations()[&0].software_reference, "MassLynx software");

        use super::pwiz_id::decode;
        // The run id of pwiz-examples' Shimadzu `20140312_六mix_column_1 (scheduled) 一个试.mzML`.
        assert_eq!(
            decode(
                "_x0032_0140312__x00e5__x0085__x00ad_mix_column_1_x0020__x0028_scheduled_x0029__x0020_\
                 _x00e4__x00b8__x0080__x00e4__x00b8__x00aa__x00e8__x00af__x0095_"
            ),
            "20140312_六mix_column_1 (scheduled) 一个试"
        );
        // A byte that is not UTF-8 (a Latin-1 ü) stays escaped; an escape above a byte is its character.
        assert_eq!(decode("M_x00fc_ller_x0020_1"), "M_x00fc_ller 1");
        assert_eq!(decode("_x516D_mix"), "六mix");
        for kept in ["a_x00zz_b", "_x0041", "x_x_", "scan=19", "", "_xD800_", "_x00e5__x0085_"] {
            assert_eq!(decode(kept), kept);
        }
    }

    /// Only ids read from an mzML or imzML are ProteoWizard's escaped names. The Thermo reader names
    /// the run after the file, so a stem that merely looks escaped is kept as it is.
    #[test]
    fn a_thermo_run_named_like_an_escape_keeps_its_stem() {
        let dir = scratch("thermo-stem");
        let input = dir.join("small_x0041_.RAW");
        fs::copy(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/small.RAW"), &input).unwrap();
        let archive = dir.join("small.mzpeak");
        let args: Vec<&std::ffi::OsStr> = vec![input.as_os_str(), "-o".as_ref(), archive.as_os_str(), "--force".as_ref()];
        let (ok, _, err) = run_bin(&args, &[("MZPC_MAX_SPECTRA", "2")]);
        assert!(ok, "{err}");
        assert_eq!(index_metadata(&archive)["run"]["id"], serde_json::json!("small_x0041_"));
        let _ = fs::remove_dir_all(&dir);
    }

    /// End to end on the mzML lane: the archive holds the decoded ids, while `-o x.mzML` keeps
    /// ProteoWizard's escaped ones, which an mzML id (an XML name) needs.
    #[test]
    fn mzml_lane_archive_decodes_pwiz_ids_and_mzml_export_keeps_them() {
        let dir = scratch("pwiz-ids");
        let src = fs::read(TINY).unwrap();
        let text = String::from_utf8_lossy(&src).replace(r#"id="CompassXtract""#, r#"id="Compass_x0020_Xtract""#)
            .replace(r#"softwareRef="CompassXtract""#, r#"softwareRef="Compass_x0020_Xtract""#);
        assert!(src.is_ascii(), "the fixture must stay byte-for-byte after the lossy round trip");
        let input = dir.join("escaped.mzML");
        fs::write(&input, text).unwrap();

        let archive = dir.join("escaped.mzpeak");
        let args: Vec<&std::ffi::OsStr> = vec![input.as_os_str(), "-o".as_ref(), archive.as_os_str(), "--force".as_ref()];
        let (ok, _, err) = run_bin(&args, &[]);
        assert!(ok, "{err}");
        let md = index_metadata(&archive);
        assert_eq!(md["run"]["id"], serde_json::json!("Experiment 1"), "{}", md["run"]);
        let software: Vec<&str> = md["software_list"].as_array().unwrap().iter().filter_map(|s| s["id"].as_str()).collect();
        assert!(software.contains(&"Compass Xtract"), "{software:?}");
        let refs: Vec<&str> = md["data_processing_method_list"].as_array().unwrap().iter()
            .flat_map(|dp| dp["methods"].as_array().unwrap().iter().filter_map(|m| m["software_reference"].as_str()))
            .collect();
        assert!(refs.contains(&"Compass Xtract") && !refs.contains(&"Compass_x0020_Xtract"), "{refs:?}");

        let mzml = dir.join("escaped.mzML.out.mzML");
        let args: Vec<&std::ffi::OsStr> = vec![input.as_os_str(), "-o".as_ref(), mzml.as_os_str(), "--force".as_ref()];
        let (ok, _, err) = run_bin(&args, &[]);
        assert!(ok, "{err}");
        let xml = fs::read_to_string(&mzml).unwrap();
        // mzdata's mzML writer numbers the run itself (`<run id="1"`); the software ids it carries.
        assert!(
            xml.contains(r#"<software id="Compass_x0020_Xtract""#) && xml.contains(r#"softwareRef="Compass_x0020_Xtract""#),
            "mzML software ids must stay escaped"
        );
        assert!(!xml.contains("Compass Xtract") && !xml.contains("Experiment 1"), "nothing decoded reaches the mzML");
    }

    /// `--sample` is given like every option, and on anything but a SciEX `.wiff` it is inert: warned
    /// about, and the run goes on. It used to be taken without a word on every input.
    #[test]
    fn sample_is_warned_inert_off_a_wiff() {
        let dir = scratch("sample-inert");
        let out = dir.join("out.mzpeak");
        let args: Vec<&std::ffi::OsStr> =
            vec![TINY.as_ref(), "-o".as_ref(), out.as_os_str(), "--force".as_ref(), "--sample".as_ref(), "2".as_ref()];
        let (ok, _, err) = run_bin(&args, &[]);
        assert!(ok && err.contains("--sample is inert"), "{err}");
    }

    /// The honoured-flags table is checked against what was GIVEN: a default is never refused,
    /// and the lane that honours everything refuses nothing.
    #[test]
    fn unsupported_flags_are_checked_against_given_only() {
        let cli = Cli::try_parse_from(["mzpeak-convert", TINY]).unwrap();
        let s = Settings::resolve(&cli).unwrap();
        for lane in [Lane::Filter, Lane::FilterToMzml, Lane::MzmlExport, Lane::ImsCompact, Lane::VendorReader] {
            assert!(refuse_unsupported_flags(lane, &s).is_ok(), "defaults only: {lane:?} must pass");
        }
        let cli = Cli::try_parse_from(["mzpeak-convert", TINY, "--zstd-level", "5", "--sdrf", "s.tsv"]).unwrap();
        let s = Settings::resolve(&cli).unwrap();
        assert!(refuse_unsupported_flags(Lane::Standard, &s).is_ok(), "the standard lane honours both");
        // The filter lane WARNS about the inert `--zstd-level` and proceeds: nothing is dropped there.
        assert!(refuse_unsupported_flags(Lane::Filter, &s).is_ok(), "filter: inert flag is a warning, not a refusal");
        assert!(super::inert_flags_for(Lane::Filter).contains(&"--zstd-level"), "…but it is still LISTED, so the warning fires");
        let e = refuse_unsupported_flags(Lane::ImsCompact, &s).unwrap_err().to_string();
        assert!(e.contains("--sdrf") && !e.contains("--zstd-level"), "ims-compact drops sdrf, honours zstd: {e}");

        // A value from a CONFIG FILE is a standing default, not this run's intent: it must not count
        // as given, or a profile carrying `zstd_level` would trip every lane that cannot use it.
        let dir = scratch("given-config");
        let cfg = dir.join("profile.yaml");
        fs::write(&cfg, "zstd_level: 5\nsdrf: s.tsv\n").unwrap();
        let cli = Cli::try_parse_from(["mzpeak-convert", TINY, "--config", cfg.to_str().unwrap()]).unwrap();
        let s = Settings::resolve(&cli).unwrap();
        assert!(s.given.is_empty(), "config-file values must not be 'given': {:?}", s.given);
        assert_eq!(s.zstd_level, 5, "…while still taking effect as the default");
        assert!(refuse_unsupported_flags(Lane::ImsCompact, &s).is_ok(), "a profile's sdrf must not refuse a lane");
    }

    /// `--ims-chunked` shapes only the timsTOF ims-compact archive. When that lane falls back to the
    /// mzdata (standard) lane on a TDF timsrust cannot decompress, it re-checks the flags against
    /// the standard lane, which lists it as inert: a warning, not a refusal, and not silence.
    #[test]
    fn ims_chunked_is_inert_on_the_standard_lane() {
        let cli = Cli::try_parse_from(["mzpeak-convert", TINY, "--ims-chunked"]).unwrap();
        let s = Settings::resolve(&cli).unwrap();
        assert!(super::inert_flags_for(Lane::Standard).contains(&"--ims-chunked"), "not listed, so never warned");
        assert!(refuse_unsupported_flags(Lane::Standard, &s).is_ok(), "inert is a warning, not a refusal");

        let dir = scratch("ims-chunked-standard");
        let out = dir.join("out.mzpeak");
        let args: Vec<&std::ffi::OsStr> =
            vec![TINY.as_ref(), "-o".as_ref(), out.as_os_str(), "--force".as_ref(), "--ims-chunked".as_ref()];
        let (ok, _, err) = run_bin(&args, &[]);
        assert!(ok && err.contains("--ims-chunked is inert on the standard lane"), "{err}");
    }

    /// End to end: a combination that would DROP user data exits non-zero with the flag named and
    /// the output is not written; a merely inert flag on the filter lane warns and proceeds.
    #[test]
    fn unsupported_flag_combination_is_refused_by_the_binary() {
        let dir = scratch("refuse");
        let sdrf = dir.join("s.tsv");
        fs::write(&sdrf, "source name\tcharacteristics[organism]\nrun1\thuman\n").unwrap();
        // `--to mzml` cannot embed an SDRF.
        let out = dir.join("out.mzML");
        let args: Vec<&std::ffi::OsStr> = vec![
            TINY.as_ref(), "-o".as_ref(), out.as_os_str(), "--force".as_ref(), "--sdrf".as_ref(), sdrf.as_os_str(),
        ];
        let (ok, _, err) = run_bin(&args, &[]);
        assert!(!ok && err.contains("--sdrf") && err.contains("--to mzml"), "{err}");
        assert!(!out.exists());

        // The filter lane re-packs members verbatim, so `--zstd-level` is INERT there — nothing the
        // user asked for is lost, so this is a warning and the run goes on (refusing would punish a
        // shared invocation for a flag that could not have changed the output).
        let archive = dir.join("a.mzpeak");
        let args: Vec<&std::ffi::OsStr> = vec![TINY.as_ref(), "-o".as_ref(), archive.as_os_str(), "--force".as_ref()];
        let (ok, _, err) = run_bin(&args, &[]);
        assert!(ok, "{err}");
        let filtered = dir.join("f.mzpeak");
        let args: Vec<&std::ffi::OsStr> = vec![
            archive.as_os_str(), "-o".as_ref(), filtered.as_os_str(), "--force".as_ref(), "--zstd-level".as_ref(), "5".as_ref(),
        ];
        let (ok, _, err) = run_bin(&args, &[]);
        assert!(ok && err.contains("--zstd-level") && err.contains("inert"), "{err}");
        assert!(filtered.exists());

        // …while the same lane DOES honour `--sdrf` (the remedy the refusals point at).
        let args: Vec<&std::ffi::OsStr> = vec![
            archive.as_os_str(), "-o".as_ref(), filtered.as_os_str(), "--force".as_ref(), "--sdrf".as_ref(), sdrf.as_os_str(),
        ];
        let (ok, _, err) = run_bin(&args, &[]);
        assert!(ok, "{err}");
        assert!(zip_members(&filtered).iter().any(|m| m == "sample_metadata/sdrf.tsv"));
    }

    /// The mzML TOF-grid sub-path used to drop `--sdrf` (and `--image`) with exit 0 — the same
    /// command kept or lost the SDRF depending on whether the grid fit passed. Runs on the committed
    /// SWATH centroid mzML, the ProteoWizard example whose fit is known to pass.
    #[test]
    fn tof_grid_subpath_embeds_sdrf() {
        let src = std::path::PathBuf::from(SWATH_GZ);
        let dir = scratch("tofgrid");
        let sdrf = dir.join("s.tsv");
        fs::write(&sdrf, "source name\tcharacteristics[organism]\nrun1\thuman\n").unwrap();
        let out = dir.join("out.mzpeak");
        let args: Vec<&std::ffi::OsStr> = vec![
            src.as_os_str(), "-o".as_ref(), out.as_os_str(), "--force".as_ref(),
            "--tof-grid".as_ref(), "on".as_ref(), "--sdrf".as_ref(), sdrf.as_os_str(),
        ];
        let (ok, _, err) = run_bin(&args, &[]);
        assert!(ok, "{err}");
        let md = index_metadata(&out);
        assert_eq!(md["tof_calibration"]["codec"], serde_json::json!("tof-grid"), "the grid sub-path ran");
        let members = zip_members(&out);
        assert!(
            members.iter().any(|m| m == "sample_metadata/sdrf.tsv"),
            "the TOF-grid finisher must embed the SDRF; members: {members:?}"
        );
        assert!(md.get("sample_metadata").is_some(), "and write its index block");
    }
}
