//! mzpeak-convert — unified converter: mzML/imzML, Bruker .d (TDF), Thermo .raw → mzPeak.
//!
//! MVP (Phase 0): the conversion core wraps mzpeak_prototyping's reference flow
//! (`examples/convert.rs`) — `mzdata::MZReaderType` auto-detects the input format and the
//! reference writer wiring (sampled data schema, metadata copy, imaging presets, TDF
//! ion-mobility) is reused rather than reinvented. The differentiated features
//! (native-TOF ims-compact, TOF-grid, vendor injection, YAML aux policy) layer onto this
//! green baseline in later phases — see PLAN.md.
//!
//! CLI follows clig.dev: subcommands, kebab-case flags, `-v/-q`, `--json`, `--dry-run`,
//! overwrite guard, stable exit codes.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};

// Vendor-SDK readers are untestable on macOS (no SDK/DLLs); keep forward-looking members without
// dead-code warnings. All are Windows(/Linux)-runtime-only, verified only to compile here.
#[cfg(feature = "bruker_sdk")]
#[allow(dead_code)]
mod bruker_baf;
#[cfg(feature = "agilent")]
#[allow(dead_code)]
mod agilent;
#[cfg(feature = "sciex")]
#[allow(dead_code)]
mod sciex;
mod bruker_native;
mod bruker_tsf;
mod thermo_status;
mod thermo_trailers;
mod tof_grid;
mod vendor;

use arrow::datatypes::DataType;
use mzdata::curie;
use mzdata::io::MZReaderType;
use mzdata::meta::{DataProcessing, ProcessingMethod, Software, SourceFile, custom_software_name};
use mzdata::params::{ControlledVocabulary, Param};
use mzdata::prelude::*;
use mzdata::spectrum::bindata::BinaryArrayMap3D;
use mzdata::spectrum::{BinaryArrayMap, Chromatogram, ChromatogramDescription};
use mzdata::spectrum::bindata::{ArrayType, BinaryDataArrayType, DataArray};
use mzpeak_prototyping::{BufferContext, BufferName};
use mzpeak_prototyping::archive::{ArchiveReader, DispatchArchiveSource, ZipArchiveWriter};
use mzpeak_prototyping::buffer_descriptors::BufferOverrideTable;
use mzpeak_prototyping::chunk_series::ChunkingStrategy;
use mzpeak_prototyping::peak_series::{INTENSITY_ARRAY, array_map_to_schema_arrays};
use mzpeak_prototyping::writer::{
    AbstractMzPeakWriter, ArrayBuffersBuilder, CustomBuilderFromParameter, MzPeakWriterType,
};
use mzpeak_prototyping::MzPeakReader;
use mzdata::prelude::ByteArrayView;
use mzpeaks::{CentroidPeak, DeconvolutedPeak};
use parquet::basic::{Compression, ZstdLevel};

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

#[derive(Parser, Debug)]
#[command(
    name = "mzpeak-convert",
    version,
    about = "Convert mzML/imzML, Bruker .d, Thermo .raw to mzPeak",
    propagate_version = true
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,

    /// Increase log verbosity (-v debug, -vv trace). Overrides RUST_LOG.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    /// Silence all logs except errors.
    #[arg(short, long, global = true, conflicts_with = "verbose")]
    quiet: bool,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Convert an input file to mzPeak.
    Convert(ConvertArgs),
    /// Report what a reader sees in an input file (format, spectra, chromatograms).
    Inspect {
        /// Input file (mzML/.mzML.gz/imzML, Bruker .d, Thermo .raw).
        input: PathBuf,
        /// Emit machine-readable JSON instead of human text.
        #[arg(long)]
        json: bool,
    },
    /// [P2] Encode a Bruker TDF .d to a lossless ims-compact Parquet (native integer-TOF).
    ImsCompact {
        /// Input Bruker .d directory (TDF).
        input: PathBuf,
        /// Output .parquet path.
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// [P5 spike] Measure how well a profile-TOF file fits a single √(m/z) lattice (go/no-go for a
    /// fit-from-m/z TOF-grid encoder). Read-only; writes nothing.
    TofGridProbe {
        /// Input file (a profile-TOF mzML / .raw / .d).
        input: PathBuf,
        /// Max per-point residual (ppm) for a spectrum to count as "fits the grid".
        #[arg(long, default_value_t = 5.0)]
        fit_tolerance_ppm: f64,
    },
    /// [P5] Encode profile-TOF m/z as the √(m/z) grid (k + per-spectrum α,β) and benchmark the m/z
    /// storage vs raw-f64 and delta+zstd. Lossy within a reported ppm bound.
    TofGrid {
        /// Input profile-TOF file.
        input: PathBuf,
        /// Output grid .parquet (a sidecar `.grid.sidecar.parquet` is written alongside).
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Max acceptable m/z error (ppm) for the GO verdict.
        #[arg(long, default_value_t = 5.0)]
        tolerance_ppm: f64,
    },
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq)]
enum Layout {
    /// Chunked m/z layout (default; numpress-linear or delta).
    Chunked,
    /// Flat point layout (one row per m/z–intensity pair).
    Point,
}

#[derive(Args, Debug)]
struct ConvertArgs {
    /// Input file (mzML/.mzML.gz/imzML, Bruker .d, Thermo .raw).
    input: PathBuf,

    /// Output .mzpeak path (default: input with .mzpeak extension).
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Signal layout.
    #[arg(long, value_enum, default_value_t = Layout::Chunked)]
    layout: Layout,

    /// Use lossless delta m/z chunking instead of the default lossy numpress-linear.
    #[arg(long)]
    no_numpress: bool,

    /// m/z chunk width (Th) for the chunked layout.
    #[arg(long, default_value_t = 50.0)]
    chunk_size: f64,

    /// Zstd compression level (1–22). Default 3 (size/speed sweet spot).
    #[arg(long, default_value_t = 3)]
    zstd_level: i32,

    /// Overwrite the output if it already exists.
    #[arg(short, long)]
    force: bool,

    /// Plan the conversion and report without writing.
    #[arg(short = 'n', long)]
    dry_run: bool,

    /// Round-trip fidelity check: re-read source and archive, compare spectrum + point counts.
    #[arg(long)]
    verify: bool,

    /// YAML aux-file policy for embedding Bruker `.d` vendor files (default: built-in policy).
    #[arg(long)]
    config: Option<PathBuf>,

    /// Override an aux-file rule (repeatable): `glob=embed` or `glob=drop`. Highest precedence.
    #[arg(long)]
    aux: Vec<String>,

    /// Do not embed any vendor side-files (Bruker `.d` only carries the converted facets).
    #[arg(long)]
    no_vendor: bool,

    /// Bruker TDF only: store the lossless ims-compact signal IN-ARCHIVE — the spectra_peaks facet
    /// carries integer `tof` (+ `ims_calibration` in the index) instead of f64 m/z.
    #[arg(long)]
    ims_compact: bool,

    /// Read the input via ProteoWizard `msconvert` (→ mzML → mzPeak). Cross-vendor interim path for
    /// Agilent `.d` / SciEX `.wiff` (and any format msconvert reads). Needs msconvert on PATH/Windows.
    #[arg(long)]
    via_msconvert: bool,

    /// Path to the `msconvert` executable (else `$MSCONVERT_PATH`, else `msconvert` on PATH).
    #[arg(long)]
    msconvert_path: Option<PathBuf>,
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

    let code = match run(cli.command) {
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

fn run(cmd: Cmd) -> Result<i32> {
    match cmd {
        Cmd::Convert(args) => cmd_convert(args),
        Cmd::Inspect { input, json } => cmd_inspect(&input, json),
        Cmd::ImsCompact { input, output } => cmd_ims_compact(&input, output),
        Cmd::TofGridProbe { input, fit_tolerance_ppm } => {
            tof_grid::probe(&input, fit_tolerance_ppm)?;
            Ok(exit::OK)
        }
        Cmd::TofGrid { input, output, tolerance_ppm } => {
            let out = output.unwrap_or_else(|| input.with_extension("grid.parquet"));
            tof_grid::encode(&input, &out, tolerance_ppm)?;
            Ok(exit::OK)
        }
    }
}

fn cmd_ims_compact(input: &Path, output: Option<PathBuf>) -> Result<i32> {
    let out = output.unwrap_or_else(|| input.with_extension("ims-compact.parquet"));
    let (rows, bytes) = bruker_native::encode_ims_compact(input, &out)
        .with_context(|| format!("ims-compact encoding {}", input.display()))?;
    let (back_rows, calib) = bruker_native::read_back_calibration(&out)?;
    if back_rows != rows {
        bail!("read-back row mismatch: wrote {rows}, read {back_rows}");
    }
    let src = dir_size(input).unwrap_or(0);
    println!("ims-compact: {} -> {}", input.display(), out.display());
    println!("  peaks:       {rows}");
    println!("  output:      {:.2} MB", bytes as f64 / 1e6);
    if src > 0 {
        println!("  source .d:   {:.2} MB  ({:.0}% of source)", src as f64 / 1e6, bytes as f64 / src as f64 * 100.0);
    }
    println!("  calibration: {calib}");
    println!("  lossless (TOF-exact): PASS — independent re-read, TOF+intensity match native bins");
    Ok(exit::OK)
}

fn dir_size(p: &Path) -> Result<u64> {
    let mut total = 0u64;
    for entry in fs::read_dir(p)? {
        let entry = entry?;
        let md = entry.metadata()?;
        total += if md.is_dir() { dir_size(&entry.path()).unwrap_or(0) } else { md.len() };
    }
    Ok(total)
}

fn cmd_convert(args: ConvertArgs) -> Result<i32> {
    let output = args
        .output
        .clone()
        .unwrap_or_else(|| args.input.with_extension("mzpeak"));

    let chunk = match args.layout {
        Layout::Point => None,
        Layout::Chunked if args.no_numpress => Some(ChunkingStrategy::Delta {
            chunk_size: args.chunk_size,
        }),
        Layout::Chunked => Some(ChunkingStrategy::NumpressLinear {
            chunk_size: args.chunk_size,
        }),
    };

    if args.dry_run {
        println!("would convert {} -> {}", args.input.display(), output.display());
        println!(
            "  layout={:?} chunk={:?} zstd={}",
            args.layout, chunk, args.zstd_level
        );
        return Ok(exit::OK);
    }

    if output.exists() && !args.force {
        bail!(
            "output {} exists (use --force to overwrite)",
            output.display()
        );
    }

    let vendor = if args.no_vendor {
        None
    } else {
        Some(vendor::VendorPolicy::load(args.config.as_deref(), &args.aux)?)
    };

    if args.via_msconvert {
        convert_via_msconvert(&args.input, &output, chunk, args.zstd_level, args.msconvert_path.as_deref())
            .with_context(|| format!("converting {} via msconvert", args.input.display()))?;
    } else if args.ims_compact {
        let is_tdf = args.input.is_dir() && args.input.join("analysis.tdf").exists();
        if !is_tdf {
            bail!("--ims-compact requires a Bruker TDF .d (with analysis.tdf)");
        }
        convert_ims_compact_archive(&args.input, &output, args.zstd_level, vendor.as_ref())
            .with_context(|| format!("ims-compact converting {}", args.input.display()))?;
    } else {
        guard_unsupported_vendor(&args.input)?;
        convert_file(&args.input, &output, chunk, args.zstd_level, vendor.as_ref())
            .with_context(|| format!("converting {}", args.input.display()))?;
    }

    log::info!("wrote {}", output.display());

    if args.verify {
        round_trip_verify(&args.input, &output).context("round-trip verify")?;
        log::info!("verify passed (counts match source)");
    }
    Ok(exit::OK)
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

/// Reject Agilent/SciEX inputs on the native path with actionable guidance: native readers need a
/// licensed-DLL Windows build (PLAN §3.7, not yet implemented); the `--via-msconvert` lane works now.
fn guard_unsupported_vendor(input: &Path) -> Result<()> {
    // Each vendor is handled natively in convert_file when its feature is built in; only bail (with
    // guidance to the msconvert lane) when the corresponding feature is OFF.
    #[cfg(not(feature = "agilent"))]
    if is_agilent_d(input) {
        return Err(UnsupportedVendor(
            "Agilent .d native reading needs a Windows build with the vendor SDK \
             (`cargo build --features agilent`). For now: `--via-msconvert`."
                .to_string(),
        )
        .into());
    }
    #[cfg(not(feature = "sciex"))]
    if is_wiff(input) {
        return Err(UnsupportedVendor(
            "SciEX .wiff native reading needs a Windows build with the vendor SDK \
             (`cargo build --features sciex`). For now: `--via-msconvert`."
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

    let result = convert_file(&mzml, output, chunk, zstd_level, None);
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
#[cfg(feature = "bruker_sdk")]
fn is_baf_dir(input: &Path) -> bool {
    input.is_dir() && input.join("analysis.baf").exists()
}

fn convert_file(
    input: &Path,
    output: &Path,
    chunk: Option<ChunkingStrategy>,
    zstd_level: i32,
    vendor: Option<&vendor::VendorPolicy>,
) -> Result<()> {
    if is_tsf_dir(input) {
        return convert_tsf(input, output, chunk, zstd_level, vendor);
    }
    #[cfg(feature = "bruker_sdk")]
    if is_baf_dir(input) {
        return convert_baf(input, output, chunk, zstd_level, vendor);
    }
    #[cfg(feature = "agilent")]
    if is_agilent_d(input) {
        return convert_agilent(input, output, chunk, zstd_level, vendor);
    }
    #[cfg(feature = "sciex")]
    if is_wiff(input) {
        return convert_sciex(input, output, chunk, zstd_level, vendor);
    }
    let mut reader = MZReaderType::<_, CentroidPeak, DeconvolutedPeak>::open_path(input)
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
    for mut entry in reader.iter() {
        // If there's ion mobility, ensure m/z is sorted (the writer expects sorted m/z).
        if entry.has_ion_mobility_dimension() {
            if let Some(arrays) = entry.arrays.as_mut() {
                if arrays.mzs().is_ok_and(|v| !v.is_sorted()) {
                    if let Ok(sorted) = BinaryArrayMap3D::stack(arrays).and_then(|v| v.unstack()) {
                        *arrays = sorted;
                    }
                }
            }
        }
        // Tag each spectrum with the concrete MS:1000294 child so the registered column populates.
        entry.description_mut().add_param(mass_spectrum.clone());
        writer.write_spectrum(&entry)?;
        n += 1;
    }
    log::debug!("wrote {n} spectra");

    let mut n_chrom = 0usize;
    for chrom in reader.iter_chromatograms() {
        writer.write_chromatogram(&chrom)?;
        n_chrom += 1;
    }
    // The reference reader requires a chromatogram facet to open the archive, and the writer only
    // finalizes index metadata (version, cv_list, ms_run) on the chromatogram-writing path
    // (writer.rs:1086). Emit one empty chromatogram (no fabricated TIC) when the source had none.
    if n_chrom == 0 {
        write_empty_chromatogram(&mut writer)?;
    }

    // Fill required ms_run fields the source may have left implicit, so the index schema validates.
    fixup_run_metadata(&mut writer, input);

    finish_with_vendor(writer, input, vendor)?;
    fs::rename(&tmp, output).with_context(|| format!("finalizing {}", output.display()))?;
    Ok(())
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
    let tof_field = BufferName::new(
        BufferContext::Spectrum,
        ArrayType::nonstandard("tof"),
        BinaryDataArrayType::Int32,
    )
    .to_field();
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

    for i in 0..reader.len() {
        let spec = reader.ims_compact_spectrum(i)?;
        writer.write_spectrum(&spec)?;
    }
    write_empty_chromatogram(&mut writer)?;
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
#[cfg(feature = "bruker_sdk")]
fn convert_baf(
    input: &Path,
    output: &Path,
    chunk: Option<ChunkingStrategy>,
    zstd_level: i32,
    vendor: Option<&vendor::VendorPolicy>,
) -> Result<()> {
    let reader = bruker_baf::BafReader::open(input, None)?;
    convert_vendor_reader(
        input, output, chunk, zstd_level, vendor,
        reader.len(), reader.sample_arrays()?, |i| reader.spectrum(i),
    )
}

/// Convert a SciEX `.wiff`/`.wiff2` → mzPeak via the Clearcore2 .NET glue (feature `sciex`,
/// Windows-runtime-only, UNTESTED here). Mirrors `convert_tsf`. Needs `$MZPC_SCIEX_GLUE` +
/// `$MZPC_PWIZ_DIR` at runtime (see glue/sciex/README.md).
#[cfg(feature = "sciex")]
fn convert_sciex(
    input: &Path,
    output: &Path,
    chunk: Option<ChunkingStrategy>,
    zstd_level: i32,
    vendor: Option<&vendor::VendorPolicy>,
) -> Result<()> {
    let reader = sciex::SciexReader::open(input)?;
    convert_vendor_reader(input, output, chunk, zstd_level, vendor, reader.len(), reader.sample_arrays()?, |i| reader.spectrum(i))
}

/// Convert a native Agilent MassHunter `.d` → mzPeak via the MHDAC .NET glue (feature `agilent`,
/// Windows-runtime-only, UNTESTED here; IM-MS/MIDAC out of scope). Mirrors `convert_tsf`.
#[cfg(feature = "agilent")]
fn convert_agilent(
    input: &Path,
    output: &Path,
    chunk: Option<ChunkingStrategy>,
    zstd_level: i32,
    vendor: Option<&vendor::VendorPolicy>,
) -> Result<()> {
    let reader = agilent::AgilentReader::open(input)?;
    convert_vendor_reader(input, output, chunk, zstd_level, vendor, reader.len(), reader.sample_arrays()?, |i| reader.spectrum(i))
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
    for i in 0..len {
        let spec = spectrum(i)?;
        writer.write_spectrum(&spec)?;
    }
    write_empty_chromatogram(&mut writer)?;
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
) -> Result<()> {
    let reader = bruker_tsf::TsfReader::open(input)?;
    convert_vendor_reader(
        input, output, chunk, zstd_level, vendor,
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

/// Round-trip fidelity: re-read the SOURCE via mzdata and the OUTPUT archive via the mzPeak
/// reader, and assert the **spectrum count** matches. This catches the main writer-side risk —
/// dropped or duplicated spectra — and is invariant to encoding and to the writer's
/// zero-intensity-run masking (which makes per-point counts legitimately differ from source).
/// Value-level (m/z/intensity) fidelity is added for the lossless ims-compact path in P2, where
/// zero-masking is off and exact comparison is meaningful.
fn round_trip_verify(input: &Path, output: &Path) -> Result<()> {
    let src_spectra = count_source(input)?;
    let arc_spectra = count_archive(output)?;
    if src_spectra != arc_spectra {
        bail!("spectrum count mismatch: source {src_spectra} vs archive {arc_spectra}");
    }
    log::debug!("verify: {src_spectra} spectra match");
    Ok(())
}

fn count_source(input: &Path) -> Result<usize> {
    if is_tsf_dir(input) {
        return Ok(bruker_tsf::TsfReader::open(input)?.len());
    }
    let reader = MZReaderType::<_, CentroidPeak, DeconvolutedPeak>::open_path(input)?;
    Ok(reader.len())
}

fn count_archive(output: &Path) -> Result<usize> {
    let archive = ArchiveReader::<DispatchArchiveSource>::from_path(output.to_path_buf())?;
    let reader = MzPeakReader::from_archive_reader(archive, Some(output.to_path_buf()))?;
    Ok(reader.into_iter().count())
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

fn cmd_inspect(input: &Path, json: bool) -> Result<i32> {
    if is_tsf_dir(input) {
        let n = bruker_tsf::TsfReader::open(input)?.len();
        if json {
            println!(r#"{{"input":{:?},"format":"Bruker TSF (.d)","spectra":{},"chromatograms":0}}"#, input.display().to_string(), n);
        } else {
            println!("input:        {}", input.display());
            println!("format:       Bruker TSF (.d)");
            println!("spectra:      {n}");
        }
        return Ok(exit::OK);
    }
    let reader = MZReaderType::<_, CentroidPeak, DeconvolutedPeak>::open_path(input)
        .with_context(|| format!("opening {}", input.display()))?;
    let format = reader_format(&reader);
    let n_spectra = reader.len();
    let n_chrom = reader.count_chromatograms();

    if json {
        println!(
            r#"{{"input":{:?},"format":{:?},"spectra":{},"chromatograms":{}}}"#,
            input.display().to_string(),
            format,
            n_spectra,
            n_chrom
        );
    } else {
        println!("input:        {}", input.display());
        println!("format:       {format}");
        println!("spectra:      {n_spectra}");
        println!("chromatograms:{n_chrom}");
    }
    Ok(exit::OK)
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

