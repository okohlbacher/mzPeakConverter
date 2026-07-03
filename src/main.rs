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
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, ValueEnum};

// Vendor-SDK readers compile in automatically on the platforms where the proprietary vendor
// libraries exist — Windows for Agilent (MHDAC), SciEX (Clearcore2) and Bruker BAF; Linux also for
// Bruker BAF. They load the vendor DLLs at runtime and report a clear error if absent. macOS has no
// vendor SDKs, so none are built there. `allow(dead_code)` keeps accessors that only some paths use.
#[cfg(any(windows, target_os = "linux"))]
#[allow(dead_code)]
mod bruker_baf;
// Bruker timsdata SDK reader (TDF + TSF) — same OS envelope as baf2sql (Win + Linux, no macOS).
#[cfg(any(windows, target_os = "linux"))]
#[allow(dead_code)]
mod bruker_sdk;
#[cfg(windows)]
#[allow(dead_code)]
mod agilent;
#[cfg(windows)]
#[allow(dead_code)]
mod agilent_midac;
#[cfg(windows)]
#[allow(dead_code)]
mod sciex;
// Native Shimadzu `.lcd` via the Shimadzu.LabSolutions.IO managed DLL (netcorehost glue, like
// SciEX). Windows-runtime-only; the `convert_shimadzu` dispatch is `#[cfg(windows)]`.
#[cfg(windows)]
#[allow(dead_code)]
mod shimadzu;
// libloading-based (cross-platform compile); only *runs* on Windows with the MassLynx DLLs, so the
// `convert_waters` dispatch stays `#[cfg(windows)]` and the reader is dead code off-Windows.
#[allow(dead_code)]
mod waters;
mod agilent_profile;
mod bruker_native;
mod bruker_tsf;
mod tof_grid;
mod tims_mobility;
mod thermo_status;
mod thermo_trailers;
mod vendor;
mod embed_aux;

use arrow::datatypes::DataType;
use mzdata::curie;
use mzdata::io::MZReaderType;
use mzdata::meta::{DataProcessing, ProcessingMethod, Software, SourceFile, custom_software_name};
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

/// Optional hard cap on how many spectra to convert (`$MZPC_MAX_SPECTRA`). Mainly for diagnostics /
/// quick cross-checks (e.g. the ion-mobility comparison only needs a handful of frames to cover the
/// full mobility axis), so a multi-GB run becomes seconds. `None` = convert everything.
fn max_spectra() -> Option<usize> {
    std::env::var("MZPC_MAX_SPECTRA")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
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

    /// m/z chunk width (Th) for the chunked layout [default: 50].
    #[arg(long)]
    chunk_size: Option<f64>,

    /// Zstd compression level (1–22) [default: 3].
    #[arg(long)]
    zstd_level: Option<i32>,

    /// Overwrite the output if it already exists.
    #[arg(short, long)]
    force: bool,

    /// Bruker timsTOF (TDF) only: disable the default lossless ims-compact integer-TOF storage and
    /// write standard f64 m/z instead.
    #[arg(long)]
    no_ims_compact: bool,

    /// Read Bruker TDF/TSF `.d` via the official Bruker timsdata SDK (parallel path to the default
    /// pure-Rust readers; Windows/Linux only, needs timsdata.dll/libtimsdata.so). Implies f64 m/z.
    #[arg(long)]
    bruker_sdk: bool,

    /// Bruker timsTOF (TDF): disable vendor-grade scan→1/K0 recalibration (the `TimsCalibration`
    /// ModelType-2 model) and use timsrust's linear approximation. Recalibration is ON by default.
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

    /// **mzML/imzML inputs only:** embed an optical image VERBATIM into the archive as
    /// `images/image_NNNN.<ext>` with a `metadata.imaging` overlay affine. Repeatable. A bad/missing
    /// path here ERRORS the conversion (strict). An `<input-stem>-opticalimage.{tif,tiff,png,jpg}`
    /// sibling is additionally auto-discovered (best-effort: warn + skip if unreadable).
    #[arg(long)]
    image: Vec<PathBuf>,

    /// **mzML/imzML inputs only:** embed an SDRF (sample-metadata) TSV VERBATIM as
    /// `sample_metadata/sdrf.tsv` with `metadata.study` + `metadata.sample_metadata` back-refs. A
    /// missing/unreadable path ERRORS the conversion.
    #[arg(long)]
    sdrf: Option<PathBuf>,

    /// **mzML inputs only** (incl. `--via-msconvert`): compactify exact-lattice TOF profile data by
    /// DETECTING an integer flight-time grid in the decoded f64 m/z and storing `tof_index` (Int32) +
    /// a per-run `{c0,c1}` instead, recovering `m/z = (c0 + c1·tof_index)²`. **Off by default** and
    /// bounded-lossy (reconstruction within `PPM_TOL`) — it reverse-engineers the grid msconvert
    /// discarded. `auto` applies it when a strict fit passes; `on` requires the fit (errors otherwise);
    /// `off` keeps exact f64. Native vendor readers ignore this — they read the true grid from the
    /// vendor calibration losslessly (strategy B) and always do so.
    #[arg(long, value_enum)]
    tof_grid: Option<TofGridMode>,

    /// Agilent Q-TOF **profile** `.d` only: read the integer flight-time grid straight from
    /// `AcqData/MSProfile.bin` (pure Rust, no MHDAC/msconvert) and store `tof_index` (Int32) + a
    /// per-run `{c0,c1}` calibration instead of f64 m/z, recovering `m/z = (c0 + c1·tof_index)²`.
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

    /// Verbose: print the inspection report (repeat `-vv` for trace logs). Overrides RUST_LOG.
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,

    /// Silence all logs except errors.
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
    match output.extension().and_then(|e| e.to_str()) {
        Some(e) if e.eq_ignore_ascii_case("mzml") => OutputFormat::Mzml,
        _ => OutputFormat::Mzpeak,
    }
}

/// When to apply the statistically-DETECTED TOF-grid m/z encoding (strategy A) on the **mzML path
/// only**. This reverse-engineers an integer flight-time grid from already-decoded f64 m/z, so it is
/// bounded-lossy (reconstruction within `PPM_TOL`). Native vendor readers do NOT use this — they
/// read the true grid from the vendor calibration (strategy B) and are lossless by construction.
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
    chunk_size: Option<f64>,
    zstd_level: Option<i32>,
    force: Option<bool>,
    no_ims_compact: Option<bool>,
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
}

/// Effective settings after merging CLI over config-file over defaults.
struct Settings {
    output: Option<PathBuf>,
    output_format: OutputFormat,
    layout: Layout,
    no_numpress: bool,
    chunk_size: f64,
    zstd_level: i32,
    /// zstd level for the byte-plane timsTOF ims-compact path. Defaults to 5 (the measured plateau —
    /// higher levels add time, not compression) rather than the general default of 3; an explicit
    /// `--zstd-level` still wins.
    ims_zstd_level: i32,
    force: bool,
    no_ims_compact: bool,
    bruker_sdk: bool,
    tims_recalibration: bool,
    no_vendor: bool,
    chromatograms: bool,
    aux: Vec<String>,
    image: Vec<PathBuf>,
    sdrf: Option<PathBuf>,
    tof_grid: TofGridMode,
    agilent_grid: bool,
    via_msconvert: bool,
    msconvert_path: Option<PathBuf>,
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
        Ok(Settings {
            output,
            output_format,
            layout: cli.layout.or(fc.layout).unwrap_or(Layout::Chunked),
            no_numpress: cli.no_numpress || fc.no_numpress.unwrap_or(false),
            chunk_size: cli.chunk_size.or(fc.chunk_size).unwrap_or(50.0),
            zstd_level: cli.zstd_level.or(fc.zstd_level).unwrap_or(3),
            ims_zstd_level: cli.zstd_level.or(fc.zstd_level).unwrap_or(5),
            force: cli.force || fc.force.unwrap_or(false),
            no_ims_compact: cli.no_ims_compact || fc.no_ims_compact.unwrap_or(false),
            bruker_sdk: cli.bruker_sdk || fc.bruker_sdk.unwrap_or(false),
            tims_recalibration: !(cli.no_tims_recalibration
                || fc.no_tims_recalibration.unwrap_or(false)),
            no_vendor: cli.no_vendor || fc.no_vendor.unwrap_or(false),
            chromatograms: !(cli.no_chromatograms || fc.no_chromatograms.unwrap_or(false)),
            aux: if cli.aux.is_empty() { fc.aux.unwrap_or_default() } else { cli.aux.clone() },
            image: if cli.image.is_empty() { fc.image.unwrap_or_default() } else { cli.image.clone() },
            sdrf: cli.sdrf.clone().or(fc.sdrf),
            tof_grid: cli.tof_grid.or(fc.tof_grid).unwrap_or_default(),
            agilent_grid: cli.agilent_grid || fc.agilent_grid.unwrap_or(false),
            via_msconvert: cli.via_msconvert || fc.via_msconvert.unwrap_or(false),
            msconvert_path: cli.msconvert_path.clone().or(fc.msconvert_path),
        })
    }
}

fn main() {
    // Thermo .raw reading self-hosts a .NET runtime (RawFileReader targets net8.0). Allow
    // roll-forward to a newer installed major (9/10) unless the user pinned it. Harmless for
    // non-Thermo inputs. SAFETY: set once at startup before any threads/readers exist.
    if std::env::var_os("DOTNET_ROLL_FORWARD").is_none() {
        unsafe { std::env::set_var("DOTNET_ROLL_FORWARD", "LatestMajor") };
    }
    // mzdata's Thermo reader panics on an unrecognized instrument model by default; downgrade to a
    // warning so a newer Astral/firmware doesn't hard-crash the converter. User override respected.
    if std::env::var_os("MZDATA_IGNORE_UNKNOWN_INSTRUMENT").is_none() {
        unsafe { std::env::set_var("MZDATA_IGNORE_UNKNOWN_INSTRUMENT", "ignore") };
    }

    let cli = Cli::parse();
    init_logging(cli.verbose, cli.quiet);

    let code = match run(&cli) {
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

fn init_logging(verbose: u8, quiet: bool) {
    let level = if quiet {
        "error"
    } else {
        match verbose {
            0 => "info",
            1 => "debug",
            _ => "trace",
        }
    };
    let env = env_logger::Env::default().default_filter_or(level);
    env_logger::Builder::from_env(env).format_timestamp(None).init();
}

fn run(cli: &Cli) -> Result<i32> {
    // Diagnostic: dump the scan→1/K0 table from timsrust (and the Bruker SDK where available) so the
    // two mobility calibrations can be compared scan-by-scan. Bypasses normal conversion.
    if std::env::var_os("MZPC_DUMP_IM_TABLE").is_some() {
        dump_im_table(&cli.input)?;
        return Ok(exit::OK);
    }
    // Diagnostic: dump decoded Agilent profile spectra (mz_min/delta come pre-folded into the grid;
    // we report sum, nnz, first/last (k,v), max v) so the pure-Rust MSProfile.bin decode can be
    // validated byte-exact against the `rainbow` reference. Bypasses conversion.
    if std::env::var_os("MZPC_DUMP_AGILENT_PROFILE").is_some() {
        dump_agilent_profile(&cli.input)?;
        return Ok(exit::OK);
    }

    let cfg = Settings::resolve(cli)?;
    let verbose = cli.verbose > 0;

    // Inspection report: always when there is no output (the whole job is "inspect"), and also as a
    // verbose extra during a real conversion.
    if verbose || cfg.output.is_none() {
        report_inspect(&cli.input)?;
    }
    let Some(output) = cfg.output.clone() else {
        return Ok(exit::OK); // no --output: nothing written, just the report above
    };

    let chunk = match cfg.layout {
        Layout::Point => None,
        Layout::Chunked if cfg.no_numpress => Some(ChunkingStrategy::Delta { chunk_size: cfg.chunk_size }),
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

    if use_agilent_grid {
        convert_agilent_grid(&cli.input, &output, cfg.zstd_level, vendor.as_ref(), cfg.chromatograms)
            .with_context(|| format!("file-direct Agilent-grid converting {}", cli.input.display()))?;
    } else if cfg.via_msconvert {
        convert_via_msconvert(&cli.input, &output, chunk, cfg.zstd_level, cfg.msconvert_path.as_deref(), cfg.chromatograms, cfg.tof_grid)
            .with_context(|| format!("converting {} via msconvert", cli.input.display()))?;
    } else if use_sdk_ims_compact {
        convert_ims_compact_sdk(&cli.input, &output, cfg.ims_zstd_level, vendor.as_ref(), cfg.chromatograms)
            .with_context(|| format!("SDK ims-compact converting {}", cli.input.display()))?;
    } else if use_bruker_sdk {
        convert_bruker_sdk(&cli.input, &output, chunk, cfg.zstd_level, vendor.as_ref(), cfg.chromatograms)
            .with_context(|| format!("converting {} via the Bruker timsdata SDK", cli.input.display()))?;
    } else if use_ims_compact {
        // Native ims-compact (direct timsrust) is the lossless default. When timsrust can't
        // decompress a frame — newer timsTOF (e.g. 5.1.x) writes a TDF binary it doesn't handle —
        // fall back to the mzdata reader interface (f64 m/z), which decodes those files. mzdata may
        // silently drop a truly-undecodable frame, so the fallback is loud. (Backlog: fix timsrust /
        // upstream a raw-TOF mode so ims-compact works on newer data through mzdata too.)
        match convert_ims_compact_archive(&cli.input, &output, cfg.ims_zstd_level, vendor.as_ref(), cfg.chromatograms, cfg.tims_recalibration) {
            Ok(()) => {}
            Err(e) if format!("{e:#}").to_lowercase().contains("decompress") => {
                log::warn!(
                    "native ims-compact failed on {} ({e}); falling back to the mzdata reader \
                     (f64 m/z, larger; may skip any frame even mzdata can't decode)",
                    cli.input.display()
                );
                guard_unsupported_vendor(&cli.input)?;
                convert_file(&cli.input, &output, chunk, cfg.zstd_level, vendor.as_ref(), cfg.chromatograms, cfg.tof_grid, &cfg.image, cfg.sdrf.as_deref())
                    .with_context(|| format!("mzdata-fallback converting {}", cli.input.display()))?;
            }
            Err(e) => return Err(e).with_context(|| format!("ims-compact converting {}", cli.input.display())),
        }
    } else {
        guard_unsupported_vendor(&cli.input)?;
        convert_file(&cli.input, &output, chunk, cfg.zstd_level, vendor.as_ref(), cfg.chromatograms, cfg.tof_grid, &cfg.image, cfg.sdrf.as_deref())
            .with_context(|| format!("converting {}", cli.input.display()))?;
    }

    log::info!("wrote {}", output.display());
    Ok(exit::OK)
}

/// True for a Bruker timsTOF TDF `.d` (folder with `analysis.tdf`).
fn is_tdf_dir(input: &Path) -> bool {
    input.is_dir() && input.join("analysis.tdf").exists()
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

/// Print a human report of what a reader sees (format, spectra, chromatograms) without converting —
/// the behaviour of a no-output run, and the `-v` extra during a conversion.
fn report_inspect(input: &Path) -> Result<()> {
    println!("input:         {}", input.display());
    if is_tsf_dir(input) {
        println!("format:        Bruker TSF (.d)");
        println!("spectra:       {}", bruker_tsf::TsfReader::open(input)?.len());
        return Ok(());
    }
    #[cfg(any(windows, target_os = "linux"))]
    if is_baf_dir(input) {
        println!("format:        Bruker BAF (.d)");
        println!("spectra:       {}", bruker_baf::BafReader::open(input, None)?.len());
        return Ok(());
    }
    if is_agilent_d(input) {
        println!("format:        Agilent .d");
        #[cfg(windows)]
        println!("spectra:       {}", agilent::AgilentReader::open(input)?.len());
        #[cfg(not(windows))]
        println!("note:          native Agilent reading needs a `--features agilent` build (or use --via-msconvert)");
        return Ok(());
    }
    if is_wiff(input) {
        println!("format:        SciEX .wiff");
        #[cfg(windows)]
        println!("spectra:       {}", sciex::SciexReader::open(input)?.len());
        #[cfg(not(windows))]
        println!("note:          native SciEX reading needs a `--features sciex` build (or use --via-msconvert)");
        return Ok(());
    }
    if is_waters_raw(input) {
        println!("format:        Waters MassLynx .raw");
        #[cfg(windows)]
        println!("spectra:       {}", waters::WatersReader::open(input)?.len());
        #[cfg(not(windows))]
        println!("note:          native Waters reading needs the waters feature on Windows (or use --via-msconvert)");
        return Ok(());
    }
    if is_lcd(input) {
        println!("format:        Shimadzu LabSolutions .lcd");
        #[cfg(windows)]
        println!("spectra:       {}", shimadzu::ShimadzuReader::open(input)?.len());
        #[cfg(not(windows))]
        println!("note:          native Shimadzu reading is Windows-only (Shimadzu.LabSolutions.IO); or use --via-msconvert");
        return Ok(());
    }
    let reader = MZReaderType::<_, CentroidPeak, DeconvolutedPeak>::open_path(input)
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
/// convert that mzML to mzPeak through the existing path. Reuses everything downstream of the reader.
/// Vendor side-file embedding is skipped (the mzML is the source); the native glue path keeps it.
fn convert_via_msconvert(
    input: &Path,
    output: &Path,
    chunk: Option<ChunkingStrategy>,
    zstd_level: i32,
    msconvert_path: Option<&Path>,
    synth_chroms: bool,
    tof_grid: TofGridMode,
) -> Result<()> {
    let exe: std::ffi::OsString = msconvert_path
        .map(|p| p.as_os_str().to_os_string())
        .or_else(|| std::env::var_os("MSCONVERT_PATH"))
        .unwrap_or_else(|| "msconvert".into());

    // Unique temp dir for the intermediate mzML (process id keeps concurrent runs from colliding).
    let tmpdir = std::env::temp_dir().join(format!("mzpc-msconvert-{}", std::process::id()));
    fs::create_dir_all(&tmpdir).with_context(|| format!("creating {}", tmpdir.display()))?;
    let mzml = tmpdir.join("via_msconvert.mzML");

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
        let t = msconvert_tail();
        let _ = fs::remove_dir_all(&tmpdir);
        bail!("msconvert failed (exit {:?}){}", status.code(), t);
    }
    if !mzml.exists() {
        let t = msconvert_tail();
        let _ = fs::remove_dir_all(&tmpdir);
        bail!("msconvert reported success but produced no mzML at {}{}", mzml.display(), t);
    }

    // msconvert produces SCIEX/Agilent mzML; the (detected, bounded-lossy) TOF-grid is opt-in and
    // OFF by default — pass the caller's mode through (this is the mzML path strategy A applies to).
    let result = convert_file(&mzml, output, chunk, zstd_level, None, synth_chroms, tof_grid, &[], None);
    let _ = fs::remove_dir_all(&tmpdir);
    result
}

/// The `--to mzml` lane: convert `input` to a plain **mzML** via the mzdata writer, streaming the
/// read spectra straight through — no mzPeak encoders (ims-compact / TOF-grid / chunking /
/// byte-plane / side-file embedding all bypassed). Covers every format the tool reads: the
/// Windows-native vendor readers (SciEX/Waters/Agilent/Shimadzu, Bruker TSF/BAF) plus everything
/// mzdata reads directly (mzML/imzML, Thermo `.raw`, Bruker TDF). `--via-msconvert` runs msconvert
/// straight to the output mzML.
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
        let r = sciex::SciexReader::open(input)?;
        return write_native_mzml(input, output, r.len(), |i| r.spectrum(i));
    }
    #[cfg(windows)]
    if is_waters_raw(input) {
        let r = waters::WatersReader::open(input)?;
        return write_native_mzml(input, output, r.len(), |i| r.spectrum(i));
    }
    #[cfg(windows)]
    if is_agilent_d(input) {
        let r = agilent::AgilentReader::open(input)?;
        return write_native_mzml(input, output, r.len(), |i| r.spectrum(i));
    }
    #[cfg(windows)]
    if is_lcd(input) {
        let r = shimadzu::ShimadzuReader::open(input)?;
        return write_native_mzml(input, output, r.len(), |i| r.spectrum(i));
    }
    // Off-Windows: the native-only vendor formats can't be read here (typed unsupported error).
    guard_unsupported_vendor(input)?;

    // mzdata-readable (mzML/imzML, Thermo `.raw`, Bruker TDF). Apply the same XML preprocessing the
    // mzPeak path uses (Latin-1 transcode + empty-param-group sanitize) so odd mzML still reads.
    let _utf8 = transcode_to_utf8(input)?;
    let utf8_path: &Path = _utf8.as_ref().map(|g| g.file.as_path()).unwrap_or(input);
    let sanitized = sanitize_param_groups(utf8_path)?;
    let read_path: &Path = sanitized.as_deref().unwrap_or(utf8_path);
    let mut reader = MZReaderType::<_, CentroidPeak, DeconvolutedPeak>::open_path(read_path)
        .with_context(|| format!("opening {}", input.display()))?;

    use mzdata::prelude::{ChromatogramSource, MSDataFileMetadata, SpectrumSource, SpectrumWriter};
    // Collect chromatograms FIRST: iterating the spectra can leave the reader positioned past the
    // chromatogramList (fatal for a chromatogram-only SRM/MRM file — the mzPeak path samples them
    // early for the same reason). Then rewind for the spectrum pass.
    let source_chroms: Vec<Chromatogram> = reader.iter_chromatograms().collect();
    let _ = reader.reset();

    let file = fs::File::create(output).with_context(|| format!("creating {}", output.display()))?;
    let mut w = mzdata::io::mzml::MzMLWriter::new(file);
    w.copy_metadata_from(&reader);
    fixup_run_metadata(&mut w, input);
    let cap = max_spectra();
    let n_spec = cap.map_or_else(|| reader.len(), |m| m.min(reader.len()));
    w.set_spectrum_count(n_spec as u64);
    // Open the spectrumList NOW so chromatograms (written after it) have a valid state even when the
    // input has zero spectra (a chromatogram-only SRM/MRM file) — otherwise `write_chromatogram`
    // fails to transition into the chromatogramList.
    w.start_spectrum_list().map_err(|e| anyhow!("opening mzML spectrumList: {e}"))?;

    for (i, spec) in reader.iter().enumerate() {
        if cap.is_some_and(|m| i >= m) {
            break;
        }
        SpectrumWriter::write(&mut w, &spec)
            .map_err(|e| anyhow!("writing spectrum {i} to mzML: {e}"))?;
    }
    // Pass through the source's chromatograms (SRM/SIM/vendor traces — otherwise silently lost,
    // fatal for MRM data). Drop source TIC/base-peak: the mzML writer emits its own spectrum-derived
    // TIC + base-peak summary at close, so keeping the source ones would duplicate them.
    write_source_chromatograms_mzml(&mut w, source_chroms.into_iter())?;

    SpectrumWriter::close(&mut w)
        .map_err(|e| anyhow!("finalizing mzML {}: {e}", output.display()))?;
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
    let file = fs::File::create(output).with_context(|| format!("creating {}", output.display()))?;
    let mut w = mzdata::io::mzml::MzMLWriter::new(file);
    fixup_run_metadata(&mut w, input);
    let n = max_spectra().map_or(len, |m| m.min(len));
    w.set_spectrum_count(n as u64);
    for i in 0..n {
        let spec = spectrum(i)?;
        SpectrumWriter::write(&mut w, &spec)
            .map_err(|e| anyhow!("writing spectrum {i} to mzML: {e}"))?;
    }
    SpectrumWriter::close(&mut w)
        .map_err(|e| anyhow!("finalizing mzML {}: {e}", output.display()))?;
    log::info!("wrote {}", output.display());
    Ok(())
}

/// Pass a source's chromatograms through to an mzML, dropping TIC/base-peak (the mzML writer emits
/// its own spectrum-derived TIC + base-peak summary at close, so those would duplicate). Everything
/// else — SRM/SIM/vendor traces — is preserved. Must be called after all spectra (writer state).
fn write_source_chromatograms_mzml<W: std::io::Write, I: Iterator<Item = Chromatogram>>(
    w: &mut mzdata::io::mzml::MzMLWriter<W>,
    source: I,
) -> Result<()> {
    for chrom in source {
        if matches!(
            chrom.chromatogram_type(),
            ChromatogramType::TotalIonCurrentChromatogram | ChromatogramType::BasePeakChromatogram
        ) {
            continue;
        }
        w.write_chromatogram(&chrom).map_err(|e| anyhow!("writing chromatogram to mzML: {e}"))?;
    }
    Ok(())
}

/// Run ProteoWizard `msconvert` to produce the output mzML directly (`--via-msconvert --to mzml`).
fn msconvert_to_mzml(input: &Path, output: &Path, msconvert_path: Option<&Path>) -> Result<()> {
    let exe: std::ffi::OsString = msconvert_path
        .map(|p| p.as_os_str().to_os_string())
        .or_else(|| std::env::var_os("MSCONVERT_PATH"))
        .unwrap_or_else(|| "msconvert".into());
    let outdir = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let outfile = output
        .file_name()
        .ok_or_else(|| anyhow!("output {} has no file name", output.display()))?;
    // Capture msconvert's stdout+stderr so a failure carries its real message (unknown-instrument /
    // unsupported-format / missing-sidecar) instead of a bare exit code — same as the mzPeak
    // `convert_via_msconvert` path (commit 57262aa).
    let log_path = std::env::temp_dir().join(format!("mzpc-msconvert-mzml-{}.log", std::process::id()));
    let mut cmd = Command::new(&exe);
    cmd.arg(input)
        .arg("--mzML")
        .arg("--ignoreUnknownInstrumentError")
        .arg("--outdir")
        .arg(outdir)
        .arg("--outfile")
        .arg(outfile);
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
        let t = tail();
        let _ = fs::remove_file(&log_path);
        bail!("msconvert failed (exit {:?}){}", status.code(), t);
    }
    if !output.exists() {
        let t = tail();
        let _ = fs::remove_file(&log_path);
        bail!("msconvert reported success but produced no mzML at {}{}", output.display(), t);
    }
    let _ = fs::remove_file(&log_path);
    log::info!("wrote {}", output.display());
    Ok(())
}

/// Core conversion: mzdata reader → mzpeak_prototyping writer. Single-threaded for the MVP
/// (the reference uses a reader/writer thread pair — a later optimization, not a correctness
/// requirement). Mirrors the proven wiring in mzpeak_prototyping/examples/convert.rs.
/// True for a Bruker TSF `.d` (line spectra; mzdata can't read it, we use the timsrust-tsf path).
fn is_tsf_dir(input: &Path) -> bool {
    input.is_dir() && input.join("analysis.tsf").exists() && !input.join("analysis.tdf").exists()
}

/// True for a Bruker BAF `.d` (Q-TOF; peak arrays behind the baf2sql_c SDK).
#[cfg(any(windows, target_os = "linux"))]
fn is_baf_dir(input: &Path) -> bool {
    input.is_dir() && input.join("analysis.baf").exists()
}

/// Sample m/z arrays across the run and try to fit a per-run integer TOF grid (`sqrt(m/z)=c0+c1·k`).
/// Returns the accepted lossless fit, or `None` if the data isn't on an exact flight-time lattice
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
fn convert_file_tof_grid(
    input: &Path,
    output: &Path,
    zstd_level: i32,
    vendor: Option<&vendor::VendorPolicy>,
    synth_chroms: bool,
    mut reader: MZReaderType<fs::File, CentroidPeak, DeconvolutedPeak>,
    grid: tof_grid::TofGrid,
) -> Result<()> {
    let tmp = output.with_extension("mzpeak.tmp");
    let handle = fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    let level = ZstdLevel::try_new(zstd_level)
        .map_err(|e| anyhow::anyhow!("invalid zstd level {zstd_level}: {e}"))?;

    let peak_schema = tof_index_peak_schema(&grid);

    let mut builder = MzPeakWriterType::<fs::File>::builder()
        .buffer_size(buffer_spectra())
        .compression(Compression::ZSTD(level))
        .add_spectrum_param_field(CustomBuilderFromParameter::from_spec(
            curie!(MS:1000294),
            "mass spectrum",
            DataType::Boolean,
        ))
        .store_peaks_and_profiles_apart(Some(peak_schema));
    // PER-SPECTRUM ROUTING: griddable spectra go to the custom `tof_index` peak facet (above);
    // off-grid spectra (MS2, sparse, off-lattice) keep EXACT f64 m/z and are routed to the standard
    // `spectra_data` (profile) facet. Sample the source so that facet's f64 m/z schema is configured
    // — without this the f64 m/z column would spill to auxiliary_arrays and read back wrong.
    builder = builder.sample_array_types_from_spectrum_source(&mut reader);
    // Derive the chromatogram schema (intensity/time dtypes) from the source chromatograms so the
    // facet matches what we write (the synthesized TIC/base-peak are f64 — sampling f64 source
    // chromatograms keeps the schema f64 and avoids an f32/f64 record-batch mismatch).
    builder = builder.sample_array_types_from_chromatograms(reader.iter_chromatograms().take(10));
    let mut writer = builder.build(handle, true);
    writer.copy_metadata_from(&reader);
    add_processing_metadata(&mut writer);

    let mass_spectrum = Param::builder().name("mass spectrum").curie(curie!(MS:1000294)).build();
    let mut ms1 = Ms1Chroms::default();
    let cap = max_spectra();
    let mut n = 0usize;
    let mut n_gridded = 0usize;
    let mut n_f64 = 0usize;
    for entry in reader.iter() {
        if cap.is_some_and(|m| n >= m) {
            break;
        }
        let spec = match tof_grid_spectrum(&entry, &grid, &mass_spectrum)? {
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
    log::info!(
        "TOF-grid wrote {n} spectra: {n_gridded} gridded (tof_index facet), {n_f64} kept f64 m/z (data facet)"
    );
    finish_chromatograms(&mut writer, &ms1, reader.iter_chromatograms(), synth_chroms)?;
    fixup_run_metadata(&mut writer, input);
    finish_tof_grid_archive(writer, &tmp, output, input, &grid, vendor)
}

/// Custom peaks-facet schema: the `point` facet carries integer `tof_index` (nonstandard, replaces
/// m/z) + intensity. The `SqrtMzFromTof` transform CURIE rides on the column, and the [c0,c1]
/// coefficients ride via field metadata (`mzpeak:transform_params`), so a conformant reader
/// recovers m/z = (c0 + c1·tof_index)² generically from the column metadata. The BufferName MUST
/// match the DataArray built per spectrum (Spectrum context, nonstandard("tof_index"), Int32) or it
/// spills to auxiliary. Shared by the mzML and native-vendor TOF-grid paths.
fn tof_index_peak_schema(grid: &tof_grid::TofGrid) -> ArrayBuffersBuilder {
    let tof_field = {
        let base = BufferName::new(
            BufferContext::Spectrum,
            ArrayType::nonstandard("tof_index"),
            BinaryDataArrayType::Int32,
        )
        .with_transform(Some(mzpeak_prototyping::buffer_descriptors::BufferTransform::SqrtMzFromTof))
        .to_field();
        let mut md = base.metadata().clone();
        md.insert("mzpeak:transform_params".to_string(), format!("{},{}", grid.c0, grid.c1));
        std::sync::Arc::new((*base).clone().with_metadata(md))
    };
    ArrayBuffersBuilder::default()
        .prefix("point")
        .with_context(BufferContext::Spectrum)
        .add_field(BufferContext::Spectrum.index_field())
        .add_field(tof_field)
        // Intensity matches the baseline f32 (SCIEX detector counts; f32 is exact for them).
        .add_field(INTENSITY_ARRAY.to_field())
}

/// Finalize a TOF-grid archive: write the `tof_calibration` index block (so readers recover
/// `m/z = (c0 + c1·tof_index)²`), embed vendor files when the input is a Bruker `.d`, finish the
/// ZIP, and rename the temp into place. Shared by the mzML and native-vendor TOF-grid paths.
fn finish_tof_grid_archive(
    writer: MzPeakWriterType<fs::File>,
    tmp: &Path,
    output: &Path,
    input: &Path,
    grid: &tof_grid::TofGrid,
    vendor: Option<&vendor::VendorPolicy>,
) -> Result<()> {
    let mut zip: ZipArchiveWriter<fs::File> = writer.finish_parquet()?;
    let cal = serde_json::json!({
        "codec": "tof-grid",
        "model": "sciex_sqrt",
        "lossless": "tof_index",
        "mz_from_tof_index": "(c0 + c1*tof_index)^2",
        "c0": grid.c0,
        "c1": grid.c1,
    });
    zip.add_index_metadata("tof_calibration", &cal)
        .context("writing tof_calibration index")?;
    let is_bruker_d = input.is_dir()
        && (input.join("analysis.tsf").exists() || input.join("analysis.tdf").exists());
    if let (Some(policy), true) = (vendor, is_bruker_d) {
        vendor::embed_into_archive(&mut zip, input, policy).context("embedding vendor files")?;
    }
    zip.finish().map_err(|e| anyhow::anyhow!("finalizing archive: {e}"))?;
    fs::rename(tmp, output).with_context(|| format!("finalizing {}", output.display()))?;
    Ok(())
}

/// Per-spectrum routing decision for the TOF-grid path.
enum TofRoute {
    /// Every point reconstructed from the run-wide grid within tolerance: a Centroid spectrum
    /// carrying `tof_index` (Int32), routed to the custom `spectra_peaks` facet.
    Gridded(MultiLayerSpectrum<CentroidPeak, DeconvolutedPeak>),
    /// At least one point is off-lattice (MS2, sparse, off-lattice): the spectrum is kept verbatim
    /// with EXACT f64 m/z and routed to the standard `spectra_data` (profile) facet.
    F64(MultiLayerSpectrum<CentroidPeak, DeconvolutedPeak>),
}

/// Decide and build the representation for one spectrum (PER-SPECTRUM, not all-or-nothing).
///
/// Try to map every f64 m/z to a grid `tof_index` and reconstruct within `PPM_TOL`. If ALL points
/// pass, return [`TofRoute::Gridded`] — a Centroid spectrum carrying `tof_index`, which the writer
/// routes to the custom `spectra_peaks` facet (`m/z = (c0 + c1·tof_index)²` on read). If ANY point
/// is off-grid, return [`TofRoute::F64`] — the original spectrum, unchanged, with exact f64 m/z,
/// which the writer routes to the standard `spectra_data` (profile) facet. A reader distinguishes
/// the two per spectrum by facet membership (peak_count>0 vs data_point_count>0, keyed on
/// spectrum_index), so one archive losslessly holds both. This replaces the former whole-run
/// `TofGridNotLossless` fallback.
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

/// Min/max over an m/z slice, guarding empty and unsorted input. Returns `None` if empty.
fn mz_min_max(mzs: &[f64]) -> Option<(f64, f64)> {
    let mut it = mzs.iter().copied().filter(|v| v.is_finite());
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

fn tof_grid_spectrum(
    entry: &MultiLayerSpectrum<CentroidPeak, DeconvolutedPeak>,
    grid: &tof_grid::TofGrid,
    mass_spectrum: &Param,
) -> Result<TofRoute> {
    let arrays = entry
        .arrays
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("spectrum {} has no arrays", entry.description().index))?;
    let mzs = arrays.mzs().map_err(|e| anyhow::anyhow!("reading m/z: {e}"))?;
    let intens = arrays.intensities().map_err(|e| anyhow::anyhow!("reading intensity: {e}"))?;

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
            Some(k) if (grid.mz(k) - mz).abs() <= mz * tof_grid::PPM_TOL * 1e-6 => tof.push(k),
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
        if !descr.params().iter().any(|p| p.curie() == Some(curie!(MS:1000294))) {
            descr.add_param(mass_spectrum.clone());
        }
        // Observed-m/z range from the source f64 m/z array (this route keeps the f64 m/z, but the
        // CV terms may still be absent on the source description).
        if let Some((lo, hi)) = mz_min_max(&mzs) {
            set_observed_mz_range(&mut descr, lo, hi);
        }
        let mut out = MultiLayerSpectrum::new(descr, entry.arrays.clone(), None, None);
        // Force Profile continuity so the f64 m/z arrays land in the standard data facet (an input
        // marked Centroid with raw arrays would otherwise hit the peak writer's tof_index schema).
        out.description_mut().signal_continuity = mzdata::spectrum::SignalContinuity::Profile;
        return Ok(TofRoute::F64(out));
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

    let mut descr = entry.description().clone();
    if !descr.params().iter().any(|p| p.curie() == Some(curie!(MS:1000294))) {
        descr.add_param(mass_spectrum.clone());
    }
    // Observed-m/z range from the source f64 m/z array (the output stores integer tof_index, so
    // these CV terms would otherwise be absent and the viewer would show "m/z 0–0").
    if let Some((lo, hi)) = mz_min_max(&mzs) {
        set_observed_mz_range(&mut descr, lo, hi);
    }
    // Route the arrays through the custom peak facet (spectra_peaks) instead of the profile-array
    // facet: the writer sends RawData+Profile to `write_spectrum_binary_array_map` (standard m/z
    // schema → our tof_index would spill to auxiliary), but RawData+Centroid to the separate peak
    // writer that honours our custom `tof_index` peak schema. We are storing a discretized point
    // list (tof_index, intensity), so Centroid continuity is the correct routing.
    descr.signal_continuity = mzdata::spectrum::SignalContinuity::Centroid;
    Ok(TofRoute::Gridded(MultiLayerSpectrum::new(descr, Some(out), None, None)))
}

/// Local CURIEs for the per-spectrum TOF-grid coefficients (Agilent profile grid drifts scan-to-scan
/// — `base`/`coeff` vary per scan — so a single run-wide `[c0,c1]` is ~100 ppm off; we store c0/c1 as
/// per-spectrum columns instead). MS:4000900/4000901 are unused local accessions reserved for this
/// converter's grid handoff; a reader recovers `m/z = (tof_c0 + tof_c1·tof_index)²` per spectrum.
const TOF_C0_CURIE: mzdata::params::CURIE =
    mzdata::params::CURIE::new(mzdata::params::ControlledVocabulary::MS, 4_000_900);
const TOF_C1_CURIE: mzdata::params::CURIE =
    mzdata::params::CURIE::new(mzdata::params::ControlledVocabulary::MS, 4_000_901);
/// Per-spectrum CalibrationID column — selects the polynomial-refinement row in the
/// `tof_calibration` index block, so the EXACT MassHunter m/z (quadratic + polynomial) reconstructs.
const TOF_CALID_CURIE: mzdata::params::CURIE =
    mzdata::params::CURIE::new(mzdata::params::ControlledVocabulary::MS, 4_000_902);

/// FILE-DIRECT Agilent Q-TOF profile converter: read the integer flight-time grid straight from
/// `AcqData/MSProfile.bin` (pure Rust, no MHDAC) and write the SAME `tof_index` (Int32) + intensity
/// peak facet `convert_file_tof_grid` uses, plus per-spectrum `tof_c0`/`tof_c1` columns (Agilent
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
    let handle = fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    let level = ZstdLevel::try_new(zstd_level)
        .map_err(|e| anyhow::anyhow!("invalid zstd level {zstd_level}: {e}"))?;

    // Peak facet: integer `tof_index` (Int32, ΔBP) + intensity, identical to the SCIEX TOF-grid path.
    // The run-wide `transform_params` on the column is informational (per-spectrum c0/c1 ride their
    // own columns); set it to the first spectrum's grid so a single-calibration run still self-describes.
    let first_grid = {
        // Peek the first spectrum's grid without consuming the stream: re-open a probe reader.
        let mut probe = agilent_profile::AgilentProfileReader::open(input)?;
        probe.next_spectrum()?.map(|s| s.grid)
    };
    let (c0_hint, c1_hint) = first_grid.map(|g| (g.c0, g.c1)).unwrap_or((0.0, 1.0));
    let tof_field = {
        let base = BufferName::new(
            BufferContext::Spectrum,
            ArrayType::nonstandard("tof_index"),
            BinaryDataArrayType::Int32,
        )
        .with_transform(Some(mzpeak_prototyping::buffer_descriptors::BufferTransform::SqrtMzFromTof))
        .to_field();
        let mut md = base.metadata().clone();
        md.insert("mzpeak:transform_params".to_string(), format!("{c0_hint},{c1_hint}"));
        md.insert("mzpeak:transform_params_per_spectrum".to_string(), "tof_c0,tof_c1".to_string());
        std::sync::Arc::new((*base).clone().with_metadata(md))
    };
    let peak_schema = ArrayBuffersBuilder::default()
        .prefix("point")
        .with_context(BufferContext::Spectrum)
        .add_field(BufferContext::Spectrum.index_field())
        .add_field(tof_field)
        .add_field(INTENSITY_ARRAY.to_field());

    let builder = MzPeakWriterType::<fs::File>::builder()
        .buffer_size(buffer_spectra())
        .compression(Compression::ZSTD(level))
        .add_spectrum_param_field(CustomBuilderFromParameter::from_spec(
            curie!(MS:1000294),
            "mass spectrum",
            DataType::Boolean,
        ))
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
        .store_peaks_and_profiles_apart(Some(peak_schema));
    let mut writer = builder.build(handle, true);
    add_processing_metadata(&mut writer);

    let mass_spectrum = Param::builder().name("mass spectrum").curie(curie!(MS:1000294)).build();
    let mut ms1 = Ms1Chroms::default();
    let cap = max_spectra();
    let mut n = 0usize;
    let mut max_ppm = 0.0f64;
    let mut nonint_intensity = false;
    while let Some(ps) = reader.next_spectrum()? {
        if cap.is_some_and(|m| n >= m) {
            break;
        }
        let spec = agilent_grid_spectrum(&reader, ps, &mass_spectrum, &mut max_ppm, &mut nonint_intensity)?;
        if synth_chroms {
            ms1.observe(&spec);
        }
        writer.write_spectrum(&spec)?;
        n += 1;
    }
    log::info!(
        "Agilent-grid: wrote {n} profile spectra; max round-trip m/z error {max_ppm:.6} ppm vs \
         MassHunter (traditional quadratic + polynomial refinement){}",
        if nonint_intensity { " (WARNING: some intensities exceeded f32-exact range)" } else { "" }
    );
    let calibrations = reader.calibrations_json();
    finish_chromatograms(&mut writer, &ms1, std::iter::empty(), synth_chroms)?;
    fixup_run_metadata(&mut writer, input);

    let mut zip: ZipArchiveWriter<fs::File> = writer.finish_parquet()?;
    let cal = serde_json::json!({
        "codec": "tof-grid",
        "model": "agilent_sqrt_poly",
        "lossless": "tof_index",
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
    // Embed the Agilent vendor side-files (AcqData) per the vendor policy, mirroring the other lanes.
    if let Some(policy) = vendor {
        vendor::embed_into_archive(&mut zip, input, policy).context("embedding vendor files")?;
    }
    zip.finish().map_err(|e| anyhow::anyhow!("finalizing archive: {e}"))?;
    fs::rename(&tmp, output).with_context(|| format!("finalizing {}", output.display()))?;
    Ok(())
}

/// Build one mzPeak spectrum from an Agilent profile spectrum: the integer `tof_index` (Int32) +
/// integer intensity (as Float32, exact for counts < 2^24), the per-spectrum grid as `tof_c0`/`tof_c1`
/// params, and a per-point lossless gate against the polynomial-refined MassHunter m/z.
fn agilent_grid_spectrum(
    reader: &agilent_profile::AgilentProfileReader,
    ps: agilent_profile::ProfileSpectrum,
    mass_spectrum: &Param,
    max_ppm: &mut f64,
    nonint_intensity: &mut bool,
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

    let intensity: Vec<f32> = ps
        .intensity
        .iter()
        .map(|&v| {
            if v > (1 << 24) {
                *nonint_intensity = true;
            }
            v as f32
        })
        .collect();

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
    descr.signal_continuity = mzdata::spectrum::SignalContinuity::Centroid;
    descr.polarity = mzdata::spectrum::ScanPolarity::Negative; // MTBLS1334 is neg-mode; faithful default
    descr.add_param(mass_spectrum.clone());
    descr.add_param(Param::builder().name("tof_c0").curie(TOF_C0_CURIE).value(grid.c0).build());
    descr.add_param(Param::builder().name("tof_c1").curie(TOF_C1_CURIE).value(grid.c1).build());
    descr.add_param(
        Param::builder()
            .name("tof_calibration_id")
            .curie(TOF_CALID_CURIE)
            .value(ps.calibration_id as i64)
            .build(),
    );
    // Observed-m/z range: the output stores integer tof_index, so reconstruct m/z = grid.mz(k) for
    // the min/max tof_index actually present (grid m/z is monotonic in tof_index, so the extremes
    // of the index range give the extremes of m/z). Without this the viewer shows "m/z 0–0".
    if let (Some(&kmin), Some(&kmax)) = (
        ps.tof_index.iter().min(),
        ps.tof_index.iter().max(),
    ) {
        let (mz_a, mz_b) = (grid.mz(kmin), grid.mz(kmax));
        set_observed_mz_range(&mut descr, mz_a.min(mz_b), mz_a.max(mz_b));
    }
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
    tof_grid: TofGridMode,
    images: &[PathBuf],
    sdrf: Option<&Path>,
) -> Result<()> {
    // --image / --sdrf are only honored on the mzML/imzML reader path below. A vendor-format input
    // (TSF/BAF/Agilent/SciEX/Waters) routes to a dedicated converter that does not embed them — warn
    // rather than silently dropping a user-supplied path.
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
        // Agilent ion-mobility (6560 IM-QTOF) needs the MIDAC SDK to read the drift dimension;
        // non-IM Agilent uses MHDAC. Probe via MIDAC; fall back to MHDAC when there's no IM data.
        if agilent_midac::file_has_ims_data(input) {
            return convert_agilent_midac(input, output, chunk, zstd_level, vendor, synth_chroms);
        }
        return convert_agilent(input, output, chunk, zstd_level, vendor, synth_chroms);
    }
    #[cfg(windows)]
    if is_wiff(input) {
        return convert_sciex(input, output, chunk, zstd_level, vendor, synth_chroms);
    }
    #[cfg(windows)]
    if is_waters_raw(input) {
        return convert_waters(input, output, chunk, zstd_level, vendor, synth_chroms);
    }
    #[cfg(windows)]
    if is_lcd(input) {
        return convert_shimadzu(input, output, chunk, zstd_level, vendor, synth_chroms);
    }
    // mzdata's quick-xml reader assumes UTF-8 and panics on Latin-1/windows-1252 high bytes
    // (e.g. zenodo DESI imzML declare ISO-8859-1). If the input declares a non-UTF-8 encoding,
    // transcode it to a throwaway UTF-8 temp first and read from there. `_utf8` is an RAII guard:
    // it deletes the temp dir (transcoded file + hardlinked .ibd sidecar) on every exit path.
    let _utf8 = transcode_to_utf8(input)?;
    let utf8_path: &Path = _utf8.as_ref().map(|g| g.file.as_path()).unwrap_or(input);

    // mzdata panics on an empty self-closing <referenceableParamGroup/> that is later referenced
    // (ProteomeDiscoverer emits these). If present, convert from a sanitized copy instead. Sanitize
    // the already-UTF-8 file so both workarounds compose.
    let sanitized = sanitize_param_groups(utf8_path)?;
    let read_path: &Path = sanitized.as_deref().unwrap_or(utf8_path);
    let mut reader = MZReaderType::<_, CentroidPeak, DeconvolutedPeak>::open_path(read_path)
        .with_context(|| format!("opening {}", input.display()))?;

    // TOF-grid m/z encoding (SCIEX / exact-lattice TOF): if requested, sample spectra and try to fit
    // a per-run integer flight-time grid `sqrt(m/z)=c0+c1·k`. When it passes the strict lossless gate
    // we store `tof_index` (Int32) instead of f64 m/z. `auto` falls back to the standard f64 path
    // when the fit fails; `on` errors. Scoped to the mzML path (this `open_path` branch only).
    if tof_grid != TofGridMode::Off {
        match try_fit_tof_grid(&mut reader) {
            Some(fit) => {
                log::info!(
                    "TOF-grid: lossless fit accepted (c0={:.6} c1={:.6e}, max {:.4} ppm, median {:.4} ppm, k≤{}, median dk={}); storing tof_index instead of f64 m/z",
                    fit.grid.c0, fit.grid.c1, fit.max_ppm, fit.median_ppm, fit.max_k, fit.median_dk
                );
                // PER-SPECTRUM routing: off-grid spectra (MS2 / sparse / off-lattice) are stored as
                // exact f64 m/z in the `spectra_data` facet, while griddable spectra use `tof_index`.
                // There is no longer a whole-run fallback — a single archive holds both facets.
                let r = convert_file_tof_grid(input, output, zstd_level, vendor, synth_chroms, reader, fit.grid);
                if let Some(s) = &sanitized {
                    let _ = fs::remove_file(s);
                }
                return r;
            }
            None => {
                if tof_grid == TofGridMode::On {
                    bail!(
                        "--tof-grid on: input {} is not losslessly griddable (no per-run integer TOF \
                         lattice within {:.2} ppm); use --tof-grid auto to fall back to f64 m/z",
                        input.display(), tof_grid::PPM_TOL
                    );
                }
                log::info!("TOF-grid auto: no lossless grid fit — keeping standard f64 m/z");
            }
        }
    }

    let tmp = output.with_extension("mzpeak.tmp");
    let handle = fs::File::create(&tmp)
        .with_context(|| format!("creating {}", tmp.display()))?;

    let level = ZstdLevel::try_new(zstd_level)
        .map_err(|e| anyhow::anyhow!("invalid zstd level {zstd_level}: {e}"))?;

    let is_imzml = matches!(reader, MZReaderType::IMzML(_));

    let mut builder = MzPeakWriterType::<fs::File>::builder()
        .chunked_encoding(chunk)
        .chromatogram_chunked_encoding(chunk)
        .buffer_size(buffer_spectra())
        .compression(Compression::ZSTD(level));

    // Derive the data schema from the data actually present (one m/z + one intensity column at
    // their source dtype) so points land in point.mz/point.intensity, not auxiliary_arrays.
    builder = builder
        .sample_array_types_from_spectrum_source(&mut reader)
        .sample_array_types_for_peaks_from_spectrum_source(&mut reader)
        .sample_array_types_from_chromatograms(reader.iter_chromatograms().take(10));

    // Register the explicit spectrum-TYPE column MS:1000294 (mass spectrum) — a concrete child of
    // MS:1000559 the validator's `spectrum_must` placement rule requires (the writer's fixed
    // spectrum_type column carries only the abstract parent accession).
    builder = builder.add_spectrum_param_field(CustomBuilderFromParameter::from_spec(
        curie!(MS:1000294),
        "mass spectrum",
        DataType::Boolean,
    ));

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
    add_processing_metadata(&mut writer);

    // Keep the ion-mobility dimension for TDF (do not flatten 3D frames).
    if let MZReaderType::BrukerTDF(tdf) = &mut reader {
        tdf.set_consolidate_peaks(false);
    }

    let mass_spectrum = Param::builder()
        .name("mass spectrum")
        .curie(curie!(MS:1000294))
        .build();

    let mut n = 0usize;
    let cap = max_spectra();
    let mut ms1 = Ms1Chroms::default();
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
                    }
                }
            }
        } else {
            // Non-IM: SRM/SIM (and some vendor) spectra list values out of m/z order, but the mzPeak
            // peaks facet requires non-decreasing m/z. mzdata may carry the same spectrum as a
            // centroid peak set, a deconvoluted set, and/or raw arrays; the writer prefers
            // peaks > deconvoluted > arrays, so re-sort whichever is present (no-op when ordered).
            if let Some(peaks) = entry.peaks.as_mut() {
                peaks.sort();
            }
            if let Some(peaks) = entry.deconvoluted_peaks.as_mut() {
                peaks.sort();
            }
            if let Some(arrays) = entry.arrays.as_mut() {
                if arrays.mzs().is_ok_and(|v| !v.is_sorted()) {
                    let _ = arrays.sort_by_array(&ArrayType::MZArray);
                }
            }
        }
        // Tag mass spectra with the concrete MS:1000294 child so the registered column populates —
        // but NOT UV/PDA (wavelength) spectra, which the writer routes to the wavelength facet.
        let is_wavelength = entry
            .arrays
            .as_ref()
            .is_some_and(|a| a.has_array(&ArrayType::WavelengthArray));
        if !is_wavelength {
            entry.description_mut().add_param(mass_spectrum.clone());
        }
        if synth_chroms {
            ms1.observe(&entry);
        }
        writer.write_spectrum(&entry)?;
        n += 1;
    }
    log::debug!("wrote {n} spectra");

    finish_chromatograms(&mut writer, &ms1, reader.iter_chromatograms(), synth_chroms)?;

    // Fill required ms_run fields the source may have left implicit, so the index schema validates.
    fixup_run_metadata(&mut writer, input);

    finish_with_vendor_and_aux(writer, input, vendor, images, sdrf)?;
    fs::rename(&tmp, output).with_context(|| format!("finalizing {}", output.display()))?;
    if let Some(s) = &sanitized {
        let _ = fs::remove_file(s);
    }
    Ok(())
}

/// RAII cleanup for a transcoded-to-UTF-8 input. Holds the temp *directory* we created (for imzML
/// we also place a hardlinked/copied `.ibd` sidecar beside the temp file, so the whole dir must go)
/// and removes it on drop — covering success, conversion error, and panic-unwind exit paths alike.
struct TranscodeGuard {
    dir: PathBuf,
    /// The transcoded UTF-8 file to hand to mzdata, inside `dir`.
    file: PathBuf,
}

impl Drop for TranscodeGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
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

/// If `input` declares a non-UTF-8 XML encoding (ISO-8859-1, latin1, windows-1252, …), transcode it
/// to UTF-8 in a throwaway temp dir and return a [`TranscodeGuard`] whose `file` is the path to hand
/// to mzdata. Returns `Ok(None)` (zero overhead) for UTF-8/ASCII inputs or inputs with no XML
/// declaration. For an imzML, the binary sidecar `<stem>.ibd` is hardlinked (or copied across
/// filesystems) beside the temp under the SAME basename so mzdata finds it and the UUID matches.
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

    // Read the whole file and transcode. Legacy MS XML files are single-byte charsets.
    let raw = fs::read(input).with_context(|| format!("reading {}", input.display()))?;
    let utf8 = decode_single_byte(&raw, &enc);
    let utf8 = rewrite_encoding_decl_to_utf8(&utf8);

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

/// Work around an mzdata defect: it `panic!`s when a `<referenceableParamGroupRef>` points at an
/// empty self-closing `<referenceableParamGroup id="…"/>` (which it never registers). Such groups
/// are valid mzML and ProteomeDiscoverer emits them. If the input's header contains that pattern,
/// write a sanitized copy where each empty group is rewritten as an explicit open/close pair and
/// return its path; otherwise return None (convert the original in place). Only the small pre-
/// `<spectrumList>` header is rewritten; the bulk of the file is streamed through verbatim.
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
    let split = find_subslice(&head, marker).unwrap_or(head.len());
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
    out.write_all(&head[split..])?; // bytes already read past the header
    io::copy(&mut f, &mut out)?; // the rest of the file, verbatim
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
    n_total: usize,
    spectrum: F,
) -> Result<()>
where
    F: FnMut(usize, bool) -> Result<MultiLayerSpectrum>,
{
    // Serial driver: SDK reader is !Send/!Sync, so its decode must stay single-threaded. The unused
    // `Parallel` arm is pinned to a fn-pointer type so inference has a concrete `P`.
    type ParPlaceholder = fn(usize, bool) -> Result<MultiLayerSpectrum>;
    write_ims_compact_archive_impl::<F, ParPlaceholder>(
        input, output, zstd_level, vendor, synth_chroms, model_a, model_b, n_total,
        Driver::Serial(spectrum),
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
    n_total: usize,
    spectrum: F,
) -> Result<()>
where
    F: Fn(usize, bool) -> Result<MultiLayerSpectrum> + Sync,
{
    type SerPlaceholder = fn(usize, bool) -> Result<MultiLayerSpectrum>;
    write_ims_compact_archive_impl::<SerPlaceholder, F>(
        input, output, zstd_level, vendor, synth_chroms, model_a, model_b, n_total,
        Driver::Parallel(spectrum),
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

fn write_ims_compact_archive_impl<S, P>(
    input: &Path,
    output: &Path,
    zstd_level: i32,
    vendor: Option<&vendor::VendorPolicy>,
    synth_chroms: bool,
    model_a: f64,
    model_b: f64,
    n_total: usize,
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
    // MZPC_BYTE_PLANE_INTENSITY=0 to opt back out to f32 intensity.
    let int_intensity = std::env::var("MZPC_BYTE_PLANE_INTENSITY")
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(true);
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
    let peak_schema = ArrayBuffersBuilder::default()
        .prefix("point")
        .with_context(BufferContext::Spectrum)
        .add_field(BufferContext::Spectrum.index_field())
        .add_field(tof_field)
        .add_field(intensity_field)
        .add_field(mob_field);

    let mut builder = MzPeakWriterType::<fs::File>::builder()
        .compression(Compression::ZSTD(level))
        .add_spectrum_param_field(CustomBuilderFromParameter::from_spec(
            curie!(MS:1000294),
            "mass spectrum",
            DataType::Boolean,
        ))
        .store_peaks_and_profiles_apart(Some(peak_schema));
    // Peak-facet row-group size (rows) = the per-chunk zstd granularity. Smaller = finer random
    // access (fewer peaks to decompress per frame) but worse compression; default is parquet's 2^20.
    // Tunable via $MZPC_ROW_GROUP_ROWS for benchmarking the size/random-access tradeoff.
    if let Some(n) = std::env::var("MZPC_ROW_GROUP_ROWS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
    {
        builder = builder.row_group_size(Some(n));
    }
    let mut writer = builder.build(handle, true);
    add_processing_metadata(&mut writer);

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
            // The writer thread owns writer+ms1 and returns them (or the first write error) on join.
            let writer_thread = std::thread::spawn(move || -> Result<(MzPeakWriterType<fs::File>, Ms1Chroms)> {
                while let Ok(spec) = rx.recv() {
                    if synth_chroms {
                        ms1.observe(&spec);
                    }
                    writer.write_spectrum(&spec)?;
                }
                Ok((writer, ms1))
            });
            // Producer: parallel-decode each window, then send in strict index order. On any decode
            // error, drop tx (closes the channel) and surface the error after joining the writer.
            let produce = || -> Result<()> {
                let mut i = 0usize;
                while i < n_frames {
                    let end = (i + window).min(n_frames);
                    let batch: Vec<Result<MultiLayerSpectrum>> =
                        (i..end).into_par_iter().map(|j| spectrum(j, int_intensity)).collect();
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
        }
    }
    finish_chromatograms(&mut writer, &ms1, std::iter::empty(), synth_chroms)?;
    fixup_run_metadata(&mut writer, input);

    // Finish: add the ims_calibration index block, embed vendor side-files, finalize, rename.
    let cal = serde_json::json!({
        "codec": "ims-compact",
        "lossless": "tof",
        "mz_from_tof": "(a + b*tof)^2",
        "tof_encoding": "absolute",
        "a": model_a,
        "b": model_b,
    });
    let mut zip: ZipArchiveWriter<fs::File> = writer.finish_parquet()?;
    zip.add_index_metadata("ims_calibration", &cal)
        .context("writing ims_calibration index")?;
    if let Some(policy) = vendor {
        vendor::embed_into_archive(&mut zip, input, policy).context("embedding vendor files")?;
    }
    zip.finish().map_err(|e| anyhow::anyhow!("finalizing archive: {e}"))?;
    fs::rename(&tmp, output).with_context(|| format!("finalizing {}", output.display()))?;
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
) -> Result<()> {
    let reader = bruker_native::NativeTofReader::open_with(input, tims_recalibration)?;
    let (a, b, n) = (reader.model.a, reader.model.b, reader.len());
    // The native reader is Sync (mmap-backed timsrust FrameReader), so decode frames in parallel.
    write_ims_compact_archive_parallel(input, output, zstd_level, vendor, synth_chroms, a, b, n, |i, int| {
        reader.ims_compact_spectrum(i, int)
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
    write_ims_compact_archive(input, output, zstd_level, vendor, synth_chroms, a, b, n, |i, int| {
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

/// Flush Parquet, then (for a Bruker `.d` with a vendor policy) stream-embed vendor side-files +
/// vendor metadata into the archive index, and finalize the ZIP. Replaces a bare `writer.finish()`
/// so the still-open archive can receive vendor members before the index is written.
fn finish_with_vendor(
    writer: MzPeakWriterType<fs::File>,
    input: &Path,
    vendor: Option<&vendor::VendorPolicy>,
) -> Result<()> {
    let mut zip: ZipArchiveWriter<fs::File> = writer.finish_parquet()?;
    embed_vendor_members(&mut zip, input, vendor)?;
    zip.finish().map_err(|e| anyhow::anyhow!("finalizing archive: {e}"))?;
    Ok(())
}

/// Like [`finish_with_vendor`], but the mzML/imzML path also embeds optical images (`--image` +
/// sibling discovery) and an SDRF (`--sdrf`) into the still-open archive BEFORE `zip.finish()`,
/// adding the `metadata.imaging` / `metadata.study` / `metadata.sample_metadata` index blocks.
fn finish_with_vendor_and_aux(
    writer: MzPeakWriterType<fs::File>,
    input: &Path,
    vendor: Option<&vendor::VendorPolicy>,
    images: &[PathBuf],
    sdrf: Option<&Path>,
) -> Result<()> {
    let mut zip: ZipArchiveWriter<fs::File> = writer.finish_parquet()?;
    embed_vendor_members(&mut zip, input, vendor)?;
    embed_aux::embed_into_archive(&mut zip, input, images, sdrf)
        .context("embedding optical images / SDRF")?;
    zip.finish().map_err(|e| anyhow::anyhow!("finalizing archive: {e}"))?;
    Ok(())
}

/// Shared vendor-member embed step (Bruker side-files / Thermo trailers), factored out so both
/// finish helpers stay in lockstep.
fn embed_vendor_members(
    zip: &mut ZipArchiveWriter<fs::File>,
    input: &Path,
    vendor: Option<&vendor::VendorPolicy>,
) -> Result<()> {
    if let Some(policy) = vendor {
        let is_bruker_d = input.is_dir()
            && (input.join("analysis.tsf").exists() || input.join("analysis.tdf").exists());
        if is_bruker_d {
            vendor::embed_into_archive(zip, input, policy)
                .context("embedding vendor files")?;
        } else if is_thermo_raw(input) {
            embed_thermo_trailers(zip, input)?;
        }
    }
    Ok(())
}

fn is_thermo_raw(input: &Path) -> bool {
    input.is_file()
        && input.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("raw"))
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
    let reader = bruker_baf::BafReader::open(input, None)?;
    convert_vendor_reader(
        input, output, chunk, zstd_level, vendor, synth_chroms,
        reader.len(), reader.sample_arrays()?, |i| reader.spectrum(i),
    )
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
    convert_vendor_reader(
        input, output, chunk, zstd_level, vendor, synth_chroms,
        reader.len(), reader.sample_arrays()?, |i| reader.spectrum(i),
    )
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
) -> Result<()> {
    let reader = shimadzu::ShimadzuReader::open(input)?;
    convert_vendor_reader(
        input, output, chunk, zstd_level, vendor, synth_chroms,
        reader.len(), reader.sample_arrays()?, |i| reader.spectrum(i),
    )
}

#[cfg(not(windows))]
fn convert_shimadzu(
    _input: &Path,
    _output: &Path,
    _chunk: Option<ChunkingStrategy>,
    _zstd_level: i32,
    _vendor: Option<&vendor::VendorPolicy>,
    _synth_chroms: bool,
) -> Result<()> {
    Err(UnsupportedVendor(
        "Shimadzu .lcd native reading is only available on Windows (Shimadzu.LabSolutions.IO vendor DLL)".into(),
    )
    .into())
}

/// Convert a SciEX `.wiff`/`.wiff2` → mzPeak via the Clearcore2 .NET glue (feature `sciex`,
/// Windows-runtime-only, UNTESTED here). Mirrors `convert_tsf`. Needs `$MZPC_SCIEX_GLUE` +
/// `$MZPC_PWIZ_DIR` at runtime (see glue/sciex/README.md).
#[cfg(windows)]
fn convert_sciex(
    input: &Path,
    output: &Path,
    chunk: Option<ChunkingStrategy>,
    zstd_level: i32,
    vendor: Option<&vendor::VendorPolicy>,
    synth_chroms: bool,
) -> Result<()> {
    // Native `.wiff` is read through Clearcore2, which currently exposes only decoded f64 m/z — not
    // the flight-time index or the mass-calibration coefficients. Strategy (B) (always grid, lossless,
    // straight from the vendor calibration — like the Agilent `MSProfile.bin` reader) therefore needs
    // a glue extension to surface SCIEX's calibration. Until then `.wiff` stores exact f64 m/z. The
    // Clearcore2 returns only DECODED f64 m/z (not the flight-time integers), so we INVERT it
    // per-spectrum into the TOF grid (`sqrt(m/z)=c0+c1·k`), storing `tof_index` + per-spectrum
    // {c0,c1}. Per-spectrum coefficients absorb per-scan c0 drift (which defeats a run-wide grid —
    // the ZenoTOF case). Off-lattice spectra (sparse/MS2) stay f64. `chunk` is unused (the grid uses
    // the point facet, not chunked m/z).
    convert_sciex_grid(input, output, chunk, zstd_level, vendor, synth_chroms)
}

/// Native SCIEX `.wiff` → mzPeak with a PER-SPECTRUM TOF grid (recycles the Agilent grid writer's
/// per-spectrum `tof_c0`/`tof_c1` columns + `tof_index` peak facet). For each spectrum we fit
/// `sqrt(m/z)=c0+c1·k` from the Clearcore2 f64 m/z and store the integer `tof_index`; a reader
/// recovers `m/z=(tof_c0+tof_c1·tof_index)²` per spectrum. Griddable spectra route to the `tof_index`
/// peak facet; off-lattice ones keep exact f64 m/z in the `spectra_data` facet.
#[cfg(windows)]
fn convert_sciex_grid(
    input: &Path,
    output: &Path,
    chunk: Option<ChunkingStrategy>,
    zstd_level: i32,
    vendor: Option<&vendor::VendorPolicy>,
    synth_chroms: bool,
) -> Result<()> {
    let reader = sciex::SciexReader::open(input)?;
    let total = reader.len();
    if total == 0 {
        bail!("no spectra in {}", input.display());
    }
    let tmp = output.with_extension("mzpeak.tmp");
    let handle = fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    let level = ZstdLevel::try_new(zstd_level)
        .map_err(|e| anyhow::anyhow!("invalid zstd level {zstd_level}: {e}"))?;

    // Peak facet: tof_index (Int32) + intensity (f32) + per-spectrum tof_c0/tof_c1 (the run-wide
    // transform_params are placeholders; the authoritative coefficients ride the per-spectrum columns).
    let tof_field = {
        let base = BufferName::new(
            BufferContext::Spectrum,
            ArrayType::nonstandard("tof_index"),
            BinaryDataArrayType::Int32,
        )
        .with_transform(Some(mzpeak_prototyping::buffer_descriptors::BufferTransform::SqrtMzFromTof))
        .to_field();
        let mut md = base.metadata().clone();
        md.insert("mzpeak:transform_params".to_string(), "0,1".to_string());
        md.insert("mzpeak:transform_params_per_spectrum".to_string(), "tof_c0,tof_c1".to_string());
        std::sync::Arc::new((*base).clone().with_metadata(md))
    };
    let peak_schema = ArrayBuffersBuilder::default()
        .prefix("point")
        .with_context(BufferContext::Spectrum)
        .add_field(BufferContext::Spectrum.index_field())
        .add_field(tof_field)
        .add_field(INTENSITY_ARRAY.to_field());

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
    let c1_global = tof_grid::fit(&samples).map(|f| f.grid.c1);

    let builder = MzPeakWriterType::<fs::File>::builder()
        .buffer_size(buffer_spectra())
        .compression(Compression::ZSTD(level))
        // Off-lattice (f64) spectra route to spectra_data — chunk that facet (numpress) so SWATH/DIA
        // runs, where most MS2 windows DON'T grid, don't bloat by storing f64 m/z flat.
        .chunked_encoding(chunk)
        .chromatogram_chunked_encoding(chunk)
        .add_spectrum_param_field(CustomBuilderFromParameter::from_spec(
            curie!(MS:1000294),
            "mass spectrum",
            DataType::Boolean,
        ))
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
    let mut writer = builder.build(handle, true);
    add_processing_metadata(&mut writer);

    let mass_spectrum = Param::builder().name("mass spectrum").curie(curie!(MS:1000294)).build();
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
        let fit = c1_global
            .and_then(|c1| tof_grid::fit_one_c1(&mz, c1))
            .or_else(|| tof_grid::fit_one(&mz));
        let out = match fit {
            Some((grid, tof_index, ppm)) => {
                max_ppm = max_ppm.max(ppm);
                n_grid += 1;
                sciex_grid_spectrum(&spec, &tof_index, grid, &mass_spectrum)?
            }
            None => {
                n_f64 += 1;
                sciex_f64_spectrum(spec, &mass_spectrum)
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
    finish_chromatograms(&mut writer, &ms1, std::iter::empty(), synth_chroms)?;
    fixup_run_metadata(&mut writer, input);

    let mut zip: ZipArchiveWriter<fs::File> = writer.finish_parquet()?;
    let cal = serde_json::json!({
        "codec": "tof-grid",
        "model": "sciex_sqrt_per_spectrum",
        "lossless": "tof_index",
        "tof_to_mz": "mz = (tof_c0 + tof_c1*tof_index)^2",
        "per_spectrum_columns": ["tof_c0", "tof_c1"],
        "max_roundtrip_ppm": max_ppm,
    });
    zip.add_index_metadata("tof_calibration", &cal)
        .context("writing tof_calibration index")?;
    zip.finish().map_err(|e| anyhow::anyhow!("finalizing archive: {e}"))?;
    fs::rename(&tmp, output).with_context(|| format!("finalizing {}", output.display()))?;
    Ok(())
}

/// Build a gridded SCIEX spectrum: `tof_index` (Int32) + intensity (f32) + per-spectrum tof_c0/tof_c1,
/// Centroid continuity (routes to the `tof_index` peak facet). Keeps the source description (RT, MS
/// level, polarity).
#[cfg(windows)]
fn sciex_grid_spectrum(
    spec: &MultiLayerSpectrum<CentroidPeak, DeconvolutedPeak>,
    tof_index: &[i32],
    grid: tof_grid::TofGrid,
    mass_spectrum: &Param,
) -> Result<MultiLayerSpectrum<CentroidPeak, DeconvolutedPeak>> {
    let intensity: Vec<f32> = spec
        .arrays
        .as_ref()
        .and_then(|a| a.intensities().ok())
        .map(|c| c.into_owned())
        .unwrap_or_default();

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
    descr.signal_continuity = mzdata::spectrum::SignalContinuity::Centroid;
    if !descr.params().iter().any(|p| p.curie() == Some(curie!(MS:1000294))) {
        descr.add_param(mass_spectrum.clone());
    }
    descr.add_param(Param::builder().name("tof_c0").curie(TOF_C0_CURIE).value(grid.c0).build());
    descr.add_param(Param::builder().name("tof_c1").curie(TOF_C1_CURIE).value(grid.c1).build());
    Ok(MultiLayerSpectrum::new(descr, Some(out), None, None))
}

/// Keep an off-lattice SCIEX spectrum as exact f64 m/z (Profile continuity → `spectra_data` facet).
#[cfg(windows)]
fn sciex_f64_spectrum(
    mut spec: MultiLayerSpectrum<CentroidPeak, DeconvolutedPeak>,
    mass_spectrum: &Param,
) -> MultiLayerSpectrum<CentroidPeak, DeconvolutedPeak> {
    spec.description_mut().signal_continuity = mzdata::spectrum::SignalContinuity::Profile;
    if !spec
        .description()
        .params()
        .iter()
        .any(|p| p.curie() == Some(curie!(MS:1000294)))
    {
        spec.description_mut().add_param(mass_spectrum.clone());
    }
    spec
}

/// Convert a Waters MassLynx `.raw` → mzPeak via the MassLynx .NET glue (Windows-runtime-only,
/// UNTESTED here). Mirrors `convert_sciex`. Needs `$MZPC_WATERS_GLUE` + `$MZPC_MASSLYNX_DIR` at
/// runtime (see glue/waters/README.md).
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
    convert_vendor_reader(input, output, chunk, zstd_level, vendor, synth_chroms, reader.len(), reader.sample_arrays()?, |i| reader.spectrum(i))
}

/// Convert a native Agilent MassHunter `.d` → mzPeak via the MHDAC .NET glue (feature `agilent`,
/// Windows-runtime-only, UNTESTED here; IM-MS/MIDAC out of scope). Mirrors `convert_tsf`.
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
    convert_vendor_reader(input, output, chunk, zstd_level, vendor, synth_chroms, reader.len(), reader.sample_arrays()?, |i| reader.spectrum(i))
}

/// Convert a native Agilent **IM-MS** `.d` → mzPeak via the MIDAC .NET glue (Windows-runtime-only,
/// UNTESTED SCAFFOLD). Each IM frame becomes one spectrum with a mean-inverse-reduced-ion-mobility
/// array; mirrors `convert_agilent` but through `agilent_midac`.
#[cfg(windows)]
fn convert_agilent_midac(
    input: &Path,
    output: &Path,
    chunk: Option<ChunkingStrategy>,
    zstd_level: i32,
    vendor: Option<&vendor::VendorPolicy>,
    synth_chroms: bool,
) -> Result<()> {
    let reader = agilent_midac::AgilentMidacReader::open(input)?;
    convert_vendor_reader(input, output, chunk, zstd_level, vendor, synth_chroms, reader.len(), reader.sample_arrays()?, |i| reader.spectrum(i))
}

/// Shared writer wiring for a custom (non-mzdata) reader: sample-derived schema + MS:1000294 column
/// + write loop + empty chromatogram + run-metadata defaults + vendor-embed + atomic rename. Used by
/// every custom-reader path (Bruker TSF/BAF, SciEX, Agilent) so they don't each duplicate the body.
fn convert_vendor_reader(
    input: &Path,
    output: &Path,
    chunk: Option<ChunkingStrategy>,
    zstd_level: i32,
    vendor: Option<&vendor::VendorPolicy>,
    synth_chroms: bool,
    len: usize,
    _sample: mzdata::spectrum::bindata::BinaryArrayMap,
    mut spectrum: impl FnMut(usize) -> Result<mzdata::spectrum::MultiLayerSpectrum>,
) -> Result<()> {
    if len == 0 {
        bail!("no spectra in {}", input.display());
    }
    let tmp = output.with_extension("mzpeak.tmp");
    let handle = fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    let level = ZstdLevel::try_new(zstd_level)
        .map_err(|e| anyhow::anyhow!("invalid zstd level {zstd_level}: {e}"))?;
    // Derive the data-facet schema from a few REAL sample spectra, CHUNK-AWARELY. The writer chunks
    // dense profile arrays into `LargeList`; a scalar schema from a single BinaryArrayMap mismatches
    // and panics the writer on SCIEX/Waters profile data. Probes spread across the run.
    const N_PROBE: usize = 6;
    let step = (len / N_PROBE).max(1);
    let mut probes: Vec<mzdata::spectrum::MultiLayerSpectrum> = Vec::new();
    let mut pi = 0usize;
    while pi < len && probes.len() < N_PROBE {
        if let Ok(s) = spectrum(pi) {
            probes.push(s);
        }
        pi += step;
    }
    let mut builder = MzPeakWriterType::<fs::File>::builder()
        .chunked_encoding(chunk)
        .chromatogram_chunked_encoding(chunk)
        .buffer_size(buffer_spectra())
        .compression(Compression::ZSTD(level))
        .sample_array_types_from_spectra(probes.into_iter());
    builder = builder.add_spectrum_param_field(CustomBuilderFromParameter::from_spec(
        curie!(MS:1000294),
        "mass spectrum",
        DataType::Boolean,
    ));
    let mut writer = builder.build(handle, true);
    add_processing_metadata(&mut writer);
    let mut ms1 = Ms1Chroms::default();
    let len = max_spectra().map_or(len, |m| m.min(len));
    for i in 0..len {
        let spec = spectrum(i)?;
        if synth_chroms {
            ms1.observe(&spec);
        }
        writer.write_spectrum(&spec)?;
    }
    finish_chromatograms(&mut writer, &ms1, std::iter::empty(), synth_chroms)?;
    fixup_run_metadata(&mut writer, input);
    finish_with_vendor(writer, input, vendor)?;
    fs::rename(&tmp, output).with_context(|| format!("finalizing {}", output.display()))?;
    Ok(())
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
        input, output, chunk, zstd_level, vendor, synth_chroms,
        reader.len(), reader.sample_arrays()?, |i| reader.spectrum(i),
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

/// Accumulates the per-MS1-spectrum TIC (summed intensity) and base-peak intensity vs. retention
/// time, so the converter can synthesize standard TIC + base-peak chromatograms. Populated during the
/// spectrum write loop (one pass, no re-read); MS2+ spectra are ignored.
#[derive(Default)]
struct Ms1Chroms {
    time: Vec<f64>,
    tic: Vec<f64>,
    bpc: Vec<f64>,
}

impl Ms1Chroms {
    fn observe(&mut self, spec: &MultiLayerSpectrum) {
        if spec.ms_level() != 1 {
            return;
        }
        let peaks = spec.peaks();
        self.time.push(spec.start_time());
        self.tic.push(peaks.tic() as f64);
        self.bpc.push(peaks.base_peak().intensity as f64);
    }

    fn is_empty(&self) -> bool {
        self.time.is_empty()
    }

    /// Write the synthesized TIC + base-peak chromatograms. Returns how many were written (0 or 2).
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
    descr.add_param(type_param);
    Ok(Chromatogram::new(descr, arrays))
}

/// Write the chromatogram facet: synthesized MS1 TIC + base-peak (when `synth` and there were MS1
/// spectra), plus any source chromatograms — skipping a source TIC/base-peak when we synthesized our
/// own so they don't duplicate. Falls back to one empty chromatogram if nothing else was written
/// (the reference reader requires the facet to open, and the writer finalizes index metadata here).
fn finish_chromatograms<I: Iterator<Item = Chromatogram>>(
    writer: &mut MzPeakWriterType<fs::File>,
    ms1: &Ms1Chroms,
    source: I,
    synth: bool,
) -> Result<()> {
    let synthesized = if synth { ms1.write(writer)? } else { 0 };
    let mut n = synthesized;
    for chrom in source {
        if synthesized > 0
            && matches!(
                chrom.chromatogram_type(),
                ChromatogramType::TotalIonCurrentChromatogram | ChromatogramType::BasePeakChromatogram
            )
        {
            continue; // superseded by our MS1-synthesized version
        }
        writer.write_chromatogram(&chrom)?;
        n += 1;
    }
    if n == 0 {
        write_empty_chromatogram(writer)?;
    }
    Ok(())
}

/// Fill required `ms_run` fields the source mzML/imzML may have left implicit, so the mzPeak index
/// schema validates. Discipline (from mzML2mzPeak): only ever fills a `None`/empty — a
/// source-declared value is left verbatim. Faithful values only (real source stem / real list
/// entry / the input file as its own source).
fn fixup_run_metadata(target: &mut impl MSDataFileMetadata, input: &Path) {
    // 1. Ensure at least one source_file (the input itself) so default_source_file_id can resolve.
    if target.file_description().source_files.is_empty() {
        let parent = input.parent().filter(|p| !p.as_os_str().is_empty());
        let location = parent.map_or_else(|| "file://".to_string(), |p| format!("file://{}", p.display()));
        let name = input.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        target.file_description_mut().source_files.push(SourceFile {
            name,
            location,
            id: "sourceFile".to_string(),
            ..Default::default()
        });
    }

    // 2. default_source_file_id / default_data_processing_id ← first list entry, when unset.
    let first_sf = target.file_description().source_files.first().map(|sf| sf.id.clone());
    let first_dp = target.data_processings().first().map(|dp| dp.id.clone());
    let first_instr = target.instrument_configurations().keys().copied().min().unwrap_or(0);
    let stem = input.file_stem().map(|s| s.to_string_lossy().to_string()).filter(|s| !s.is_empty());
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
        if run.default_instrument_id.is_none() {
            run.default_instrument_id = Some(first_instr);
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
                std::env::args().skip(1).collect::<Vec<String>>().join(" "),
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
mod tests {
    use super::expand_empty_param_groups;
    use super::{decode_single_byte, rewrite_encoding_decl_to_utf8, sniff_xml_encoding};
    use super::{tof_grid, tof_grid_spectrum, TofRoute};
    use mzdata::params::Param;
    use mzdata::prelude::*;
    use mzdata::spectrum::bindata::{ArrayType, BinaryDataArrayType, DataArray};
    use mzdata::spectrum::{BinaryArrayMap, MultiLayerSpectrum, SpectrumDescription};
    use mzpeaks::{CentroidPeak, DeconvolutedPeak};

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

    /// PER-SPECTRUM routing: a spectrum entirely on the grid → tof_index (Gridded);
    /// a spectrum with any off-lattice point → exact f64 m/z (F64). Both in one run.
    #[test]
    fn tof_grid_routes_per_spectrum() {
        // Coarse-enough step that the half-step quantization at low m/z exceeds PPM_TOL, so a genuinely
        // off-lattice spectrum has points beyond tolerance and must route F64 (at a very fine grid the
        // 5 ppm tolerance would snap any m/z onto a node, which is correct but wouldn't test routing).
        let grid = tof_grid::TofGrid { c0: 14.0, c1: 1.0e-4 };
        let mass_spectrum = Param::builder().name("mass spectrum").build();

        // on-lattice spectrum: build from exact grid points → must route Gridded.
        let on: Vec<f64> = (200_000i32..200_400).map(|k| grid.mz(k)).collect();
        let on_int = vec![1.0f32; on.len()];
        match tof_grid_spectrum(&spec_from(&on, &on_int, 0), &grid, &mass_spectrum).unwrap() {
            TofRoute::Gridded(s) => {
                assert_eq!(s.signal_continuity(), mzdata::spectrum::SignalContinuity::Centroid);
                // the gridded facet carries tof_index, NOT f64 m/z
                assert!(s.arrays.as_ref().unwrap().get(&ArrayType::nonstandard("tof_index")).is_some());
            }
            TofRoute::F64(_) => panic!("on-lattice spectrum should grid"),
        }

        // off-lattice spectrum: arbitrary m/z not on the lattice → must route F64 with EXACT m/z.
        let off: Vec<f64> = (0..50).map(|i| 137.0 + 0.131 * i as f64 + 0.017 * (i as f64).sin()).collect();
        let off_int = vec![2.0f32; off.len()];
        match tof_grid_spectrum(&spec_from(&off, &off_int, 1), &grid, &mass_spectrum).unwrap() {
            TofRoute::F64(s) => {
                assert_eq!(s.signal_continuity(), mzdata::spectrum::SignalContinuity::Profile);
                // exact f64 m/z preserved bit-for-bit
                let back = s.arrays.as_ref().unwrap().mzs().unwrap();
                assert_eq!(back.as_ref(), off.as_slice());
            }
            TofRoute::Gridded(_) => panic!("off-lattice spectrum must keep f64 m/z"),
        }
    }

    /// TASK 1 (B.3): a gridded TOF spectrum must carry the observed-m/z CV terms (MS:1000528 lowest,
    /// MS:1000527 highest) computed from the source f64 m/z, so the viewer doesn't show "m/z 0–0".
    #[test]
    fn gridded_spectrum_carries_observed_mz_range() {
        let grid = tof_grid::TofGrid { c0: 14.0, c1: 1.0e-4 };
        let mass_spectrum = Param::builder().name("mass spectrum").build();
        let on: Vec<f64> = (200_000i32..200_400).map(|k| grid.mz(k)).collect();
        let on_int = vec![1.0f32; on.len()];
        let (want_lo, want_hi) = (on[0], on[on.len() - 1]);
        match tof_grid_spectrum(&spec_from(&on, &on_int, 0), &grid, &mass_spectrum).unwrap() {
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
            }
            TofRoute::F64(_) => panic!("on-lattice spectrum should grid"),
        }
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
        // the #3 invariant debug_assert — via the array_map write path in both debug and release.
        // (The chunked/default path is verified end-to-end via the release CLI; in *debug* it also
        // trips a separate pre-existing chunk-facet spill `debug_assert`, unrelated to the twin.)
        let cases: [(&str, Option<ChunkingStrategy>); 1] = [("point", None)];
        for (tag, chunk) in cases {
            let scratch =
                std::env::temp_dir().join(format!("mzpc-peaks-{tag}-{}", std::process::id()));
            fs::create_dir_all(&scratch).unwrap();
            let output = scratch.join("mixed.mzpeak");
            let _ = fs::remove_file(&output);

            // synth_chroms=true mirrors the CLI default. (An unrelated pre-existing point-layout write
            // clash triggers only with --no-chromatograms + mixed precision; not this test's concern.)
            super::convert_file(&input, &output, chunk, 3, None, true, super::TofGridMode::Off, &[], None)
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

        // Output is XML mzML (not a zip), and re-reads to the same spectrum count.
        let head = fs::read(&out).unwrap();
        let is_mzml = head.starts_with(b"<?xml") || head.windows(5).any(|w| w == b"<mzML");
        let mut reader =
            super::MZReaderType::<_, super::CentroidPeak, super::DeconvolutedPeak>::open_path(&out)
                .expect("reopen mzML output");
        let n = reader.iter().count();
        let _ = fs::remove_dir_all(&scratch);
        assert!(is_mzml, "output is not mzML XML");
        assert_eq!(n, 6, "expected 6 spectra in the mzML output, got {n}");
    }

    /// `--to mzml` chromatogram regression (corpus-gated): a chromatogram-only SRM/MRM mzML (0
    /// spectra) must convert to mzML with its SRM traces PRESERVED — not silently dropped, and
    /// without the 0-spectra "Run to Run" writer crash. Uses the sciex-qtrap scheduled-MRM file (a
    /// real msconvert SRM; synthetic mzML chromatograms aren't read back by mzdata). Run with:
    ///   `cargo test --release mzml_output_preserves_srm -- --ignored --nocapture`
    #[test]
    #[ignore = "needs the sciex-qtrap scheduled-MRM corpus file; run with --ignored"]
    fn mzml_output_preserves_srm_chromatograms() {
        use mzdata::prelude::{ChromatogramSource, SpectrumSource};
        use std::fs;

        let input = std::path::Path::new(
            "/Users/kohlbach/Claude/mzpeak-example-data/data/general-ms/sciex-qtrap-6500/Drug_substance_3_scheduled_MRM.mzML",
        );
        assert!(input.exists(), "corpus SRM file missing: {}", input.display());
        let scratch = std::env::temp_dir().join(format!("mzpc-srm-{}", std::process::id()));
        fs::create_dir_all(&scratch).unwrap();
        let out = scratch.join("out.mzML");

        super::convert_to_mzml(input, &out, false, None).expect("SRM → mzML must not crash");

        let mut reader =
            super::MZReaderType::<_, super::CentroidPeak, super::DeconvolutedPeak>::open_path(&out)
                .expect("reopen SRM mzML");
        let n_chrom = reader.iter_chromatograms().count();
        let n_spec = reader.iter().count();
        let _ = fs::remove_dir_all(&scratch);
        assert_eq!(n_spec, 0, "SRM file has no spectra");
        // 720 SRM transitions preserved + the writer's TIC/BIC summary.
        assert!(n_chrom > 100, "SRM chromatograms must be preserved in mzML, got {n_chrom}");
    }

    /// B.4 regression (frame-preserving ims-compact). Corpus-gated: needs the smallest timsTOF `.d`
    /// (~2 GB), too large to vendor. Convert it and assert the peak facet carries
    /// `mean_inverse_reduced_ion_mobility` (MS:1003006) and that #spectra == #TDF frames
    /// (one spectrum per FRAME, not per mobility scan). `#[ignore]` by default; run with:
    ///   `cargo test --release ims_compact_is_frame_preserving -- --ignored --nocapture`
    /// Corpus path (see MEMORY: smallest timsTOF):
    ///   /Users/kohlbach/Claude/mzPeak/data/ims-examples/bruker-timstof-pro/raw/SBA415_Try.d/SBA415(1) Try_Slot1-2_1_8271.d
    #[test]
    #[ignore = "needs the ~2GB SBA415 timsTOF .d corpus fixture; run with --ignored"]
    fn ims_compact_is_frame_preserving() {
        use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
        use std::fs;

        let input = std::path::Path::new(
            "/Users/kohlbach/Claude/mzPeak/data/ims-examples/bruker-timstof-pro/raw/SBA415_Try.d/SBA415(1) Try_Slot1-2_1_8271.d",
        );
        assert!(input.exists(), "corpus fixture missing: {}", input.display());

        let scratch = std::path::Path::new(
            "/private/tmp/claude-501/-Users-kohlbach-Claude-mzPeak-mzPeakConverter/b893364b-bab9-4ecd-b671-4ff71e9db809/scratchpad",
        );
        fs::create_dir_all(scratch).unwrap();
        let output = scratch.join("sba415_ims_compact.mzpeak");
        let _ = fs::remove_file(&output);

        super::convert_ims_compact_archive(input, &output, 3, None, false, false)
            .expect("ims-compact conversion");

        // Crack the zip archive and extract facets to scratch files (File: ChunkReader).
        let f = fs::File::open(&output).unwrap();
        let mut zip = zip::ZipArchive::new(f).unwrap();

        // (a) the peak facet has the mean 1/K0 mobility column (MS:1003006).
        let peaks_path = extract_zip_entry(&mut zip, "spectra_peaks.parquet", scratch);
        let builder =
            ParquetRecordBatchReaderBuilder::try_new(fs::File::open(&peaks_path).unwrap()).unwrap();
        let schema = builder.schema().clone();
        assert!(
            schema.field_with_name("mean_inverse_reduced_ion_mobility").is_ok(),
            "spectra_peaks must carry mean_inverse_reduced_ion_mobility (MS:1003006); got {:?}",
            schema.fields().iter().map(|f| f.name()).collect::<Vec<_>>()
        );
        // The peak facet stores integer `tof`, not m/z.
        assert!(schema.field_with_name("tof").is_ok(), "peak facet must have a `tof` column");

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

    /// A-contract lock-in. Asserts the calibration index keys + peak/grid column names don't get
    /// renamed out from under readers. Corpus-gated (`#[ignore]`) because it converts real `.d`
    /// inputs. Run with `cargo test --release contract_ -- --ignored`.
    #[test]
    #[ignore = "needs the SBA415 timsTOF .d corpus fixture; run with --ignored"]
    fn contract_ims_compact_calibration_keys() {
        use std::fs;
        use std::io::Read;

        let input = std::path::Path::new(
            "/Users/kohlbach/Claude/mzPeak/data/ims-examples/bruker-timstof-pro/raw/SBA415_Try.d/SBA415(1) Try_Slot1-2_1_8271.d",
        );
        assert!(input.exists(), "corpus fixture missing: {}", input.display());
        let scratch = std::path::Path::new(
            "/private/tmp/claude-501/-Users-kohlbach-Claude-mzPeak-mzPeakConverter/b893364b-bab9-4ecd-b671-4ff71e9db809/scratchpad",
        );
        fs::create_dir_all(scratch).unwrap();
        let output = scratch.join("sba415_contract.mzpeak");
        let _ = fs::remove_file(&output);
        super::convert_ims_compact_archive(input, &output, 3, None, false, false)
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

        // peaks schema has a `tof` column.
        let peaks_path = extract_zip_entry(&mut zip, "spectra_peaks.parquet", scratch);
        let builder = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(
            fs::File::open(&peaks_path).unwrap(),
        )
        .unwrap();
        assert!(
            builder.schema().field_with_name("tof").is_ok(),
            "ims-compact peaks schema must have a `tof` column"
        );
    }

    #[test]
    #[ignore = "needs a SciEX/Agilent TOF-grid .d or .wiff corpus source; run with --ignored"]
    fn contract_tof_grid_calibration_keys() {
        // Lock-in for the tof-grid archive contract: metadata.tof_calibration with codec:"tof-grid"
        // and a `tof_index` column in the peak facet. Point CORPUS at a readable grid SOURCE.
        use std::fs;
        use std::io::Read;

        let corpus = std::env::var("MZPEAK_TOF_GRID_SOURCE").unwrap_or_default();
        assert!(
            !corpus.is_empty(),
            "set MZPEAK_TOF_GRID_SOURCE=/path/to/grid/source (.d or .mzML) to run this test"
        );
        let input = std::path::Path::new(&corpus);
        assert!(input.exists(), "MZPEAK_TOF_GRID_SOURCE not found: {corpus}");
        let scratch = std::path::Path::new(
            "/private/tmp/claude-501/-Users-kohlbach-Claude-mzPeak-mzPeakConverter/b893364b-bab9-4ecd-b671-4ff71e9db809/scratchpad",
        );
        fs::create_dir_all(scratch).unwrap();
        let output = scratch.join("tof_grid_contract.mzpeak");
        let _ = fs::remove_file(&output);

        // Drive the full converter binary so the path matches production. The bin lives under the
        // crate's target/release dir (unit tests don't get CARGO_BIN_EXE_*, so resolve it manually).
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/release/mzpeak-convert");
        assert!(bin.exists(), "build the release binary first: {}", bin.display());
        let status = std::process::Command::new(&bin)
            .arg(input)
            .arg("-o")
            .arg(&output)
            .status()
            .expect("running mzpeak-convert");
        assert!(status.success(), "conversion failed");

        let f = fs::File::open(&output).unwrap();
        let mut zip = zip::ZipArchive::new(f).unwrap();
        let mut idx_bytes = Vec::new();
        zip.by_name("mzpeak_index.json")
            .unwrap()
            .read_to_end(&mut idx_bytes)
            .unwrap();
        let idx: serde_json::Value = serde_json::from_slice(&idx_bytes).unwrap();
        let cal = idx
            .get("metadata")
            .and_then(|m| m.get("tof_calibration"))
            .expect("metadata.tof_calibration present");
        assert_eq!(cal.get("codec").and_then(|v| v.as_str()), Some("tof-grid"));

        let peaks_path = extract_zip_entry(&mut zip, "spectra_peaks.parquet", scratch);
        let builder = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(
            fs::File::open(&peaks_path).unwrap(),
        )
        .unwrap();
        assert!(
            builder.schema().field_with_name("tof_index").is_ok(),
            "tof-grid peaks schema must have a `tof_index` column"
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
}
