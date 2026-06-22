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

use anyhow::{Context, Result, bail};
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
mod bruker_native;
mod bruker_tsf;
mod thermo_status;
mod thermo_trailers;
mod vendor;

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
use mzpeak_prototyping::buffer_descriptors::BufferOverrideTable;
use mzpeak_prototyping::chunk_series::ChunkingStrategy;
use mzpeak_prototyping::peak_series::{INTENSITY_ARRAY, array_map_to_schema_arrays};
use mzpeak_prototyping::writer::{
    AbstractMzPeakWriter, ArrayBuffersBuilder, CustomBuilderFromParameter, MzPeakWriterType,
};
use mzdata::prelude::ByteArrayView;
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
    about = "Convert MS data (mzML/imzML, Bruker, Thermo, ...) to the mzPeak format",
    propagate_version = true
)]
struct Cli {
    /// Input file or vendor directory (mzML/.mzML.gz/imzML, Bruker .d, Thermo .raw).
    input: PathBuf,

    /// Output .mzpeak path. If omitted, NOTHING is written — the input is only inspected and a
    /// report (format, spectra, chromatograms) is printed.
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Config file (YAML) setting defaults for any option below; explicit command-line flags win.
    #[arg(short = 'c', long)]
    config: Option<PathBuf>,

    /// Signal layout [default: chunked].
    #[arg(long, value_enum)]
    layout: Option<Layout>,

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

    /// Do not embed vendor side-files into the archive.
    #[arg(long)]
    no_vendor: bool,

    /// Do not synthesize TIC + base-peak chromatograms from the MS1 spectra (synthesis is on by default).
    #[arg(long)]
    no_chromatograms: bool,

    /// Vendor side-file rule (repeatable): `glob=embed` or `glob=drop`. Highest precedence.
    #[arg(long)]
    aux: Vec<String>,

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

/// Config-file schema: every overridable option, all optional. Loaded from `--config`. Precedence:
/// explicit command-line flag > config-file value > built-in default.
#[derive(serde::Deserialize, Default, Debug)]
#[serde(default, deny_unknown_fields)]
struct FileConfig {
    output: Option<PathBuf>,
    layout: Option<Layout>,
    no_numpress: Option<bool>,
    chunk_size: Option<f64>,
    zstd_level: Option<i32>,
    force: Option<bool>,
    no_ims_compact: Option<bool>,
    bruker_sdk: Option<bool>,
    no_vendor: Option<bool>,
    no_chromatograms: Option<bool>,
    aux: Option<Vec<String>>,
    via_msconvert: Option<bool>,
    msconvert_path: Option<PathBuf>,
}

/// Effective settings after merging CLI over config-file over defaults.
struct Settings {
    output: Option<PathBuf>,
    layout: Layout,
    no_numpress: bool,
    chunk_size: f64,
    zstd_level: i32,
    force: bool,
    no_ims_compact: bool,
    bruker_sdk: bool,
    no_vendor: bool,
    chromatograms: bool,
    aux: Vec<String>,
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
        Ok(Settings {
            output: cli.output.clone().or(fc.output),
            layout: cli.layout.or(fc.layout).unwrap_or(Layout::Chunked),
            no_numpress: cli.no_numpress || fc.no_numpress.unwrap_or(false),
            chunk_size: cli.chunk_size.or(fc.chunk_size).unwrap_or(50.0),
            zstd_level: cli.zstd_level.or(fc.zstd_level).unwrap_or(3),
            force: cli.force || fc.force.unwrap_or(false),
            no_ims_compact: cli.no_ims_compact || fc.no_ims_compact.unwrap_or(false),
            bruker_sdk: cli.bruker_sdk || fc.bruker_sdk.unwrap_or(false),
            no_vendor: cli.no_vendor || fc.no_vendor.unwrap_or(false),
            chromatograms: !(cli.no_chromatograms || fc.no_chromatograms.unwrap_or(false)),
            aux: if cli.aux.is_empty() { fc.aux.unwrap_or_default() } else { cli.aux.clone() },
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

    let vendor = if cfg.no_vendor {
        None
    } else {
        Some(vendor::VendorPolicy::load(None, &cfg.aux)?)
    };

    // The Bruker SDK path (opt-in) reads TDF/TSF via timsdata and supersedes both ims-compact and the
    // pure-Rust readers for those inputs.
    let use_bruker_sdk = cfg.bruker_sdk && (is_tdf_dir(&cli.input) || is_tsf_dir(&cli.input));
    // ims-compact is the DEFAULT for Bruker timsTOF (TDF); --no-ims-compact (or --bruker-sdk) falls
    // back to f64 m/z.
    let use_ims_compact = is_tdf_dir(&cli.input) && !cfg.no_ims_compact && !use_bruker_sdk;

    if cfg.via_msconvert {
        convert_via_msconvert(&cli.input, &output, chunk, cfg.zstd_level, cfg.msconvert_path.as_deref(), cfg.chromatograms)
            .with_context(|| format!("converting {} via msconvert", cli.input.display()))?;
    } else if use_bruker_sdk {
        convert_bruker_sdk(&cli.input, &output, chunk, cfg.zstd_level, vendor.as_ref(), cfg.chromatograms)
            .with_context(|| format!("converting {} via the Bruker timsdata SDK", cli.input.display()))?;
    } else if use_ims_compact {
        convert_ims_compact_archive(&cli.input, &output, cfg.zstd_level, vendor.as_ref(), cfg.chromatograms)
            .with_context(|| format!("ims-compact converting {}", cli.input.display()))?;
    } else {
        guard_unsupported_vendor(&cli.input)?;
        convert_file(&cli.input, &output, chunk, cfg.zstd_level, vendor.as_ref(), cfg.chromatograms)
            .with_context(|| format!("converting {}", cli.input.display()))?;
    }

    log::info!("wrote {}", output.display());
    Ok(exit::OK)
}

/// True for a Bruker timsTOF TDF `.d` (folder with `analysis.tdf`).
fn is_tdf_dir(input: &Path) -> bool {
    input.is_dir() && input.join("analysis.tdf").exists()
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

/// True for a SciEX wiff/wiff2 file.
fn is_wiff(input: &Path) -> bool {
    input
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("wiff") || e.eq_ignore_ascii_case("wiff2"))
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
) -> Result<()> {
    let exe: std::ffi::OsString = msconvert_path
        .map(|p| p.as_os_str().to_os_string())
        .or_else(|| std::env::var_os("MSCONVERT_PATH"))
        .unwrap_or_else(|| "msconvert".into());

    // Unique temp dir for the intermediate mzML (process id keeps concurrent runs from colliding).
    let tmpdir = std::env::temp_dir().join(format!("mzpc-msconvert-{}", std::process::id()));
    fs::create_dir_all(&tmpdir).with_context(|| format!("creating {}", tmpdir.display()))?;
    let mzml = tmpdir.join("via_msconvert.mzML");

    let mut cmd = Command::new(&exe);
    cmd.arg(input)
        .arg("--mzML")
        .arg("--outdir")
        .arg(&tmpdir)
        .arg("--outfile")
        .arg("via_msconvert.mzML");
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
    if !status.success() {
        let _ = fs::remove_dir_all(&tmpdir);
        bail!("msconvert failed (exit {:?})", status.code());
    }
    if !mzml.exists() {
        let _ = fs::remove_dir_all(&tmpdir);
        bail!("msconvert reported success but produced no mzML at {}", mzml.display());
    }

    let result = convert_file(&mzml, output, chunk, zstd_level, None, synth_chroms);
    let _ = fs::remove_dir_all(&tmpdir);
    result
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

fn convert_file(
    input: &Path,
    output: &Path,
    chunk: Option<ChunkingStrategy>,
    zstd_level: i32,
    vendor: Option<&vendor::VendorPolicy>,
    synth_chroms: bool,
) -> Result<()> {
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
    // mzdata panics on an empty self-closing <referenceableParamGroup/> that is later referenced
    // (ProteomeDiscoverer emits these). If present, convert from a sanitized copy instead.
    let sanitized = sanitize_param_groups(input)?;
    let read_path: &Path = sanitized.as_deref().unwrap_or(input);
    let mut reader = MZReaderType::<_, CentroidPeak, DeconvolutedPeak>::open_path(read_path)
        .with_context(|| format!("opening {}", input.display()))?;

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
    let mut ms1 = Ms1Chroms::default();
    for mut entry in reader.iter() {
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

    finish_with_vendor(writer, input, vendor)?;
    fs::rename(&tmp, output).with_context(|| format!("finalizing {}", output.display()))?;
    if let Some(s) = &sanitized {
        let _ = fs::remove_file(s);
    }
    Ok(())
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
fn convert_ims_compact_archive(
    input: &Path,
    output: &Path,
    zstd_level: i32,
    vendor: Option<&vendor::VendorPolicy>,
    synth_chroms: bool,
) -> Result<()> {
    let reader = bruker_native::NativeTofReader::open(input)?;
    if reader.len() == 0 {
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
            format!("{},{}", reader.model.a, reader.model.b),
        );
        std::sync::Arc::new((*base).clone().with_metadata(md))
    };
    let mob_field = BufferName::new(
        BufferContext::Spectrum,
        ArrayType::MeanInverseReducedIonMobilityArray,
        BinaryDataArrayType::Float64,
    )
    .to_field();
    let peak_schema = ArrayBuffersBuilder::default()
        .prefix("point")
        .with_context(BufferContext::Spectrum)
        .add_field(BufferContext::Spectrum.index_field())
        .add_field(tof_field)
        .add_field(INTENSITY_ARRAY.to_field())
        .add_field(mob_field);

    let builder = MzPeakWriterType::<fs::File>::builder()
        .compression(Compression::ZSTD(level))
        .add_spectrum_param_field(CustomBuilderFromParameter::from_spec(
            curie!(MS:1000294),
            "mass spectrum",
            DataType::Boolean,
        ))
        .store_peaks_and_profiles_apart(Some(peak_schema));
    let mut writer = builder.build(handle, true);
    add_processing_metadata(&mut writer);

    let mut ms1 = Ms1Chroms::default();
    for i in 0..reader.len() {
        let spec = reader.ims_compact_spectrum(i)?;
        if synth_chroms {
            ms1.observe(&spec);
        }
        writer.write_spectrum(&spec)?;
    }
    finish_chromatograms(&mut writer, &ms1, std::iter::empty(), synth_chroms)?;
    fixup_run_metadata(&mut writer, input);

    // Finish: add the ims_calibration index block, embed vendor side-files, finalize, rename.
    let mut zip: ZipArchiveWriter<fs::File> = writer.finish_parquet()?;
    zip.add_index_metadata("ims_calibration", &reader.calibration_json())
        .context("writing ims_calibration index")?;
    if let Some(policy) = vendor {
        vendor::embed_into_archive(&mut zip, input, policy).context("embedding vendor files")?;
    }
    zip.finish().map_err(|e| anyhow::anyhow!("finalizing archive: {e}"))?;
    fs::rename(&tmp, output).with_context(|| format!("finalizing {}", output.display()))?;
    Ok(())
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
    if let Some(policy) = vendor {
        let is_bruker_d = input.is_dir()
            && (input.join("analysis.tsf").exists() || input.join("analysis.tdf").exists());
        if is_bruker_d {
            vendor::embed_into_archive(&mut zip, input, policy)
                .context("embedding vendor files")?;
        } else if is_thermo_raw(input) {
            embed_thermo_trailers(&mut zip, input)?;
        }
    }
    zip.finish().map_err(|e| anyhow::anyhow!("finalizing archive: {e}"))?;
    Ok(())
}

fn is_thermo_raw(input: &Path) -> bool {
    input.is_file()
        && input.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("raw"))
}

/// Build + embed the Thermo `vendor_scan_trailers.parquet` proprietary facet (Track 2). Best-effort:
/// a trailer-read failure is logged but does not abort the (already-written) conversion.
fn embed_thermo_trailers(zip: &mut ZipArchiveWriter<fs::File>, input: &Path) -> Result<()> {
    match thermo_trailers::build_trailer_facet(input) {
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
    match thermo_status::build_status_log_facet(input) {
        Ok(Some(bytes)) => {
            zip.add_file_from_read(&mut std::io::Cursor::new(bytes), None::<&String>, Some(proprietary("vendor_status_log.parquet")))
                .context("embedding vendor_status_log.parquet")?;
            log::info!("embedded Thermo vendor_status_log facet");
        }
        Ok(None) => log::debug!("no Thermo status logs to embed"),
        Err(e) => log::warn!("skipping Thermo status-log facet: {e:#}"),
    }
    match thermo_status::build_trailer_wide_facet(input) {
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
    let reader = sciex::SciexReader::open(input)?;
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
    sample: mzdata::spectrum::bindata::BinaryArrayMap,
    mut spectrum: impl FnMut(usize) -> Result<mzdata::spectrum::MultiLayerSpectrum>,
) -> Result<()> {
    if len == 0 {
        bail!("no spectra in {}", input.display());
    }
    let tmp = output.with_extension("mzpeak.tmp");
    let handle = fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    let level = ZstdLevel::try_new(zstd_level)
        .map_err(|e| anyhow::anyhow!("invalid zstd level {zstd_level}: {e}"))?;
    let mut builder = MzPeakWriterType::<fs::File>::builder()
        .chunked_encoding(chunk)
        .chromatogram_chunked_encoding(chunk)
        .buffer_size(buffer_spectra())
        .compression(Compression::ZSTD(level));
    for field in data_facet_fields_from_samples(&[&sample]) {
        builder = builder.add_spectrum_field(field);
    }
    builder = builder.add_spectrum_param_field(CustomBuilderFromParameter::from_spec(
        curie!(MS:1000294),
        "mass spectrum",
        DataType::Boolean,
    ));
    let mut writer = builder.build(handle, true);
    add_processing_metadata(&mut writer);
    let mut ms1 = Ms1Chroms::default();
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

/// Derive spectra_data POINT-column fields from sample array maps (ported from mzML2mzPeak): runs
/// the reference `array_map_to_schema_arrays` so each array yields one column at its SOURCE dtype,
/// de-duplicated by name. Used when the reader is not an mzdata `SpectrumSource`.
fn data_facet_fields_from_samples(samples: &[&BinaryArrayMap]) -> Vec<arrow::datatypes::FieldRef> {
    let overrides = BufferOverrideTable::default();
    let mut fields: Vec<arrow::datatypes::FieldRef> = Vec::new();
    for map in samples {
        let primary_len = map
            .get(&BufferContext::Spectrum.default_sorted_array())
            .and_then(|a| a.data_len().ok())
            .unwrap_or_default();
        if let Ok((derived, _arrays)) =
            array_map_to_schema_arrays(BufferContext::Spectrum, map, primary_len, 0, None, &overrides)
        {
            for f in derived.iter() {
                if !fields.iter().any(|g| g.name() == f.name()) {
                    fields.push(f.clone());
                }
            }
        }
    }
    fields
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
    let mut i = DataArray::wrap(&ArrayType::IntensityArray, BinaryDataArrayType::Float64, Vec::new());
    i.update_buffer(intensity).map_err(|e| anyhow::anyhow!("encoding chromatogram intensity: {e}"))?;
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
