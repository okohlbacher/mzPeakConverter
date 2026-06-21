# mzPeakConverter — User Manual

`mzpeak-convert` converts mass-spectrometry raw and exchange formats into the
**mzPeak** format (HUPO-PSI, v0.9). It reads through
[`mzdata`](https://github.com/mobiusklein/mzdata) (plus native readers for
formats mzdata does not cover) and writes through the reference
`mzpeak_prototyping` writer.

- [1. What it does](#1-what-it-does)
- [2. Installation & requirements](#2-installation--requirements)
- [3. Quick start](#3-quick-start)
- [4. Commands](#4-commands)
  - [4.1 convert](#41-convert)
  - [4.2 inspect](#42-inspect)
  - [4.3 validate](#43-validate)
  - [4.4 ims-compact](#44-ims-compact)
  - [4.5 tof-grid-probe / tof-grid](#45-tof-grid-probe--tof-grid)
- [5. Input formats](#5-input-formats)
- [6. The mzPeak output](#6-the-mzpeak-output)
- [7. Vendor side-files](#7-vendor-side-files)
- [8. Compression & layout](#8-compression--layout)
- [9. Verification & validation](#9-verification--validation)
- [10. Exit codes & environment](#10-exit-codes--environment)
- [11. Optional vendor-SDK builds](#11-optional-vendor-sdk-builds)
- [12. Dependencies](#12-dependencies)
- [13. Troubleshooting](#13-troubleshooting)

---

## 1. What it does

mzPeakConverter takes one input acquisition (a file or a vendor directory) and
produces a single `.mzpeak` archive — a STORED ZIP holding Apache Parquet facets
(`spectra_metadata`, `spectra_data`, `spectra_peaks`, `chromatograms`) plus an
`mzpeak_index.json`. The goal is a **lossless, columnar, analysis-ready**
representation that preserves vendor metadata and ion-mobility structure.

Highlights:

- One binary for mzML/imzML, Bruker `.d` (TDF/TSF), and Thermo `.raw`.
- Lossless **ims-compact** encoding for Bruker timsTOF integer-TOF data.
- Verbatim Thermo scan-trailer capture (FAIMS CV, injection time, charge, …).
- Vendor side-file embedding so the original metadata travels with the data.
- A cross-vendor fallback through ProteoWizard `msconvert`.

## 2. Installation & requirements

### Build requirements

| Requirement | Notes |
|---|---|
| Rust ≥ 1.87 | edition 2024; install via <https://rustup.rs> |
| C toolchain | for the bundled native libs (sqlite, zstd) |
| .NET 8+ runtime | **only for Thermo `.raw`**; auto-rolls-forward to 9/10 |

```sh
git clone https://github.com/okohlbacher/mzPeakConverter.git
cd mzPeakConverter
cargo build --release
# binary at target/release/mzpeak-convert
```

The first build downloads the `nethost` loader for the Thermo interop layer.
Non-Thermo conversions need no .NET.

### Runtime requirements

- Nothing beyond the binary for mzML/imzML/Bruker conversions.
- A .NET 8+ runtime for Thermo `.raw` (<https://dotnet.microsoft.com/download>).
- `mzpeak-validate` (from the mzPeak tooling) for `--validate` / the `validate`
  command. Use a `pyarrow ≥ 14` environment for ims-compact archives.
- `msconvert` (ProteoWizard) for `--via-msconvert`.

## 3. Quick start

```sh
# Convert an mzML file (output path inferred: sample.mzpeak)
mzpeak-convert convert sample.mzML

# Convert a Thermo .raw, verify the round trip, and validate the result
mzpeak-convert convert run.raw -o run.mzpeak --verify --validate --force

# See what a reader makes of an input without converting
mzpeak-convert inspect run.raw

# Convert a Bruker timsTOF .d with lossless integer-TOF storage
mzpeak-convert convert experiment.d -o experiment.mzpeak --ims-compact
```

## 4. Commands

Global options (accepted by every subcommand): `-v`/`--verbose` (repeat for more
detail; overrides `RUST_LOG`), `-q`/`--quiet`, `-h`/`--help`, `-V`/`--version`.

### 4.1 convert

`mzpeak-convert convert [OPTIONS] <INPUT>`

Convert an input file/directory to an mzPeak archive.

| Option | Default | Description |
|---|---|---|
| `-o, --output <PATH>` | `<input>.mzpeak` | Output archive path |
| `--layout <chunked\|point>` | `chunked` | Signal layout (see §8) |
| `--no-numpress` | off | Lossless delta m/z chunking instead of lossy numpress-linear |
| `--chunk-size <Th>` | `50` | m/z chunk width for the chunked layout |
| `--zstd-level <1–22>` | `3` | Parquet zstd level |
| `-f, --force` | off | Overwrite an existing output |
| `-n, --dry-run` | off | Plan and report; write nothing |
| `--verify` | off | Round-trip check (re-read source + archive; compare spectrum & point counts) |
| `--validate` | off | Run `mzpeak-validate` on the output |
| `--ims-compact` | off | Bruker TDF: store lossless integer-`tof` in `spectra_peaks` (§5) |
| `--config <YAML>` | built-in | Vendor side-file embedding policy (§7) |
| `--aux <glob=embed\|drop>` | — | Override one embedding rule (repeatable, highest precedence) |
| `--no-vendor` | off | Do not embed any vendor side-files |
| `--via-msconvert` | off | Read via ProteoWizard `msconvert` → mzML → mzPeak (§5) |
| `--msconvert-path <PATH>` | `$MSCONVERT_PATH` / PATH | Location of the `msconvert` executable |

### 4.2 inspect

`mzpeak-convert inspect [--json] <INPUT>`

Report what the reader sees — detected format, spectrum count, MS levels,
chromatograms — without converting. `--json` emits a machine-readable summary.

### 4.3 validate

`mzpeak-convert validate [--json <PATH>] <ARCHIVE>`

Validate an existing `.mzpeak` archive (or unpacked directory) against the mzPeak
schema/conformance rules by invoking `mzpeak-validate`. `--json <PATH>` writes the
validator's full JSON report.

### 4.4 ims-compact

`mzpeak-convert ims-compact -o <OUT.parquet> <INPUT.d>`

Encode a Bruker **TDF** `.d` to a standalone lossless **ims-compact** Parquet
(native integer-TOF). The encoder streams one frame at a time (constant memory)
and verifies losslessness with an independent native re-read before finalizing.
Use this to inspect or benchmark the encoding in isolation; for a full archive,
use `convert --ims-compact`.

### 4.5 tof-grid-probe / tof-grid

Research/feasibility tools for profile-TOF m/z grid encoding (PSI P5 spike):

- `tof-grid-probe [--fit-tolerance-ppm N] <INPUT>` — read-only; measures how well
  a profile-TOF acquisition fits a single √(m/z) lattice (a go/no-go signal).
- `tof-grid [-o OUT] [--tolerance-ppm N] <INPUT>` — encodes m/z as the √(m/z)
  grid and benchmarks storage vs raw-f64 and delta+zstd. **Lossy** within the
  reported ppm bound.

## 5. Input formats

| Format | Reader | Notes |
|---|---|---|
| mzML, `.mzML.gz` | mzdata | full metadata + chromatograms |
| imzML | mzdata | imaging x/y(/z) coordinate columns; IMS CV promoted |
| Bruker `.d` (TDF) | mzdata `bruker_tdf` + native `timsrust` | ion mobility preserved; `--ims-compact` for lossless integer-TOF |
| Bruker `.d` (TSF) | ported reader (rusqlite + zstd) | MALDI / line spectra; otofControl m/z correction |
| Thermo `.raw` | mzdata `thermo` (.NET) | verbatim scan-trailer facets |
| Bruker `.d` (BAF) | `bruker_sdk` feature | needs `libbaf2sql_c` (Windows/Linux) |
| Agilent `.d`, SciEX `.wiff` | `--via-msconvert`, or `agilent`/`sciex` features | native readers Windows-only (§11) |

Inputs for unsupported vendors exit with code **3** and actionable guidance
(usually: use `--via-msconvert`).

## 6. The mzPeak output

A `.mzpeak` file is a **STORED** (uncompressed-container) ZIP. Compression lives
*inside* the Parquet facets, not in the ZIP, so readers can range-read columns.
Contents:

- `mzpeak_index.json` — manifest: facets, schema versions, run metadata,
  `ims_calibration` (for ims-compact), declared file entries.
- `spectra_metadata.parquet` — per-spectrum descriptors (id, index, MS level,
  polarity, scan time, precursor info, …).
- `spectra_data.parquet` / `spectra_peaks.parquet` — signal arrays (chunked or
  point layout).
- `chromatograms.parquet` — TIC/BPC/SRM and other chromatograms.
- `vendor/…` — embedded original side-files (optional, see §7).

## 7. Vendor side-files

For Bruker `.d`, the original side-files are **embedded by default** under
`vendor/` in the archive (preserve-by-default), gzip-compressed, and declared as
`proprietary` entries in the index. Control this with:

- `--no-vendor` — embed nothing.
- `--config policy.yaml` — a YAML glob→action policy (`embed` / `drop`).
- `--aux 'glob=drop'` / `--aux 'glob=embed'` — per-glob override, highest
  precedence, repeatable.

For Thermo `.raw`, vendor scan trailers (FAIMS CV, injection time, charge, …) are
captured into dedicated `vendor_scan_trailers` facets (tall + wide) and a
`vendor_status_log` facet.

## 8. Compression & layout

- **Layout** — `chunked` (default) groups m/z into chunks (`--chunk-size`, Th)
  and encodes each with numpress-linear (lossy, compact) or, with
  `--no-numpress`, lossless delta. `point` writes one row per (m/z, intensity).
- **zstd** — applied inside Parquet, `--zstd-level` 1–22 (default 3).
- **ims-compact** — Bruker TDF integer-TOF stored bit-exact with delta-reset +
  BYTE_STREAM_SPLIT; ~50 % smaller than f64 m/z. m/z is reconstructed by readers
  as `m/z = (a + b·tof)²` from the `ims_calibration` index entry.

## 9. Verification & validation

- `--verify` performs an in-process **round-trip**: it re-reads both the source
  and the freshly written archive and asserts spectrum (and point) counts match.
  A mismatch fails the conversion (exit 5).
- `--validate` (or the `validate` command) runs **`mzpeak-validate`** for schema
  and conformance checking.

These are independent: `--verify` checks fidelity to the source; `--validate`
checks conformance to the mzPeak spec.

## 10. Exit codes & environment

| Code | Meaning |
|---|---|
| 0 | success |
| 1 | generic error |
| 3 | unsupported input/format in this build |
| 5 | verify or validate failure |

Environment variables:

| Variable | Effect |
|---|---|
| `RUST_LOG` | log filter (overridden by `-v`/`-q`) |
| `DOTNET_ROLL_FORWARD` | set automatically to `LatestMajor` if unset (Thermo) |
| `MZDATA_IGNORE_UNKNOWN_INSTRUMENT` | set automatically to `ignore` if unset |
| `MSCONVERT_PATH` | `msconvert` location for `--via-msconvert` |
| `MZPEAK_VALIDATE` | path to the `mzpeak-validate` to use |
| `MZPC_PWIZ_DIR`, `MZPC_SCIEX_GLUE`, `MZPC_AGILENT_GLUE` | native vendor-SDK builds (§11) |

## 11. Optional vendor-SDK builds

Off by default (they need licensed vendor DLLs and are platform-restricted):

```sh
cargo build --release --features bruker_sdk   # Bruker BAF via libbaf2sql_c
cargo build --release --features agilent      # Agilent MHDAC (native .NET glue)
cargo build --release --features sciex        # SciEX Clearcore2 (native .NET glue)
```

The Agilent/SciEX readers are **Windows-runtime-only** and currently
compile-verified but not runtime-tested. They need a .NET 8 runtime, the built
C# glue (`glue/{agilent,sciex}/`, pointed to by `$MZPC_{AGILENT,SCIEX}_GLUE`),
and vendor DLLs from a ProteoWizard install (`$MZPC_PWIZ_DIR`). The C# glues are
reflection-only and build anywhere (`dotnet build glue/sciex/SciexGlue.csproj`).

For everyday cross-vendor needs, prefer `--via-msconvert` — it needs no special
build.

## 12. Dependencies

mzPeakConverter is pure Rust plus a small C# interop layer for Thermo/native
vendor readers. Core crates: `mzdata`, `mzpeaks`, `arrow`/`parquet`, `zip`,
`timsrust`, `rusqlite`/`zstd`, `flate2`, `clap`, `serde`, `anyhow`. The reference
writer `mzpeak_prototyping` is vendored under `vendor/`. A complete inventory of
all 395 transitive dependencies (with licenses) is in
[`sbom.cdx.json`](../sbom.cdx.json); see [THIRD-PARTY-NOTICES.md](../THIRD-PARTY-NOTICES.md).

## 13. Troubleshooting

| Symptom | Fix |
|---|---|
| Thermo `.raw` fails to open | install a .NET 8+ runtime |
| `validate` uses the wrong pyarrow | point `MZPEAK_VALIDATE` at a `pyarrow ≥ 14` env |
| `--via-msconvert` not found | install ProteoWizard or set `--msconvert-path`/`$MSCONVERT_PATH` |
| Agilent/SciEX exits with code 3 | expected without `--features`; use `--via-msconvert` |
| UV/PDA spectra missing | non-MS spectra are not yet carried (known limitation) |
| Output exists error | pass `--force` to overwrite |
