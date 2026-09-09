# mzPeakConverter

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.88%2B-orange.svg)](https://www.rust-lang.org)
[![Release](https://img.shields.io/github/v/release/okohlbacher/mzPeakConverter?sort=semver)](https://github.com/okohlbacher/mzPeakConverter/releases)

> [!IMPORTANT]
> The **mzPeak format is still going through the HUPO-PSI specification process**
> (currently draft v0.9). This converter is a **technical demonstrator, not a
> production tool yet** — the output layout and semantics may change as the
> specification evolves.

A unified converter from mass-spectrometry formats to the **mzPeak** format
(HUPO-PSI, v0.9). It reads via [`mzdata`](https://github.com/mobiusklein/mzdata)
(plus native readers for formats mzdata does not cover) and writes via the
reference `mzpeak_prototyping` writer (vendored under `vendor/`).

## The mzPeak format

- 🌐 Website: **[mzpeak.org](https://mzpeak.org)** — overview, rationale, and the draft specification.
- 📑 Specification repository: **[HUPO-PSI/mzPeak-specification](https://github.com/HUPO-PSI/mzPeak-specification)**.
- 🔬 Inspect & analyze any `.mzpeak` file in your browser — no upload, no backend —
  at **[mzpeak.org/view](https://mzpeak.org/view)**.

`mzpeak-convert` turns one acquisition into a single `.mzpeak` archive — a STORED
ZIP of Apache Parquet facets + a JSON index — that is columnar, analysis-ready, and
preserves vendor metadata and ion-mobility structure.

**Fidelity.** The archive preserves the vendor's signal **to a stated fidelity, and
declares every transformation it applied** in the index (`transformations`). Most lanes
are bit-exact (integer TOF, the fixed-point m/z lattice, centroid m/z). Four transforms
are not, and each is named in the archive with its bound: the default **numpress-linear**
chunk encoding of profile m/z (`--no-numpress` for lossless delta), **zero-run
compaction** of profile baselines (consecutive zeros collapse to one at each peak
boundary), the **`--tof-grid` sqrt grid**, accepted only within a ppm bound
(`MZPC_TOF_GRID_PPM`, default 5), and the **Shimadzu profile pad trim** (the
zero-intensity pad at the scan-window bounds outside the signal span is not stored).

## Documentation

- 📘 **[User Manual](docs/USER_MANUAL.md)** — every option, the config file,
  output layout, vendor metadata handling, requirements, troubleshooting.
- 🌐 The **mzPeak format**: [mzpeak.org](https://mzpeak.org) · spec repo
  [HUPO-PSI/mzPeak-specification](https://github.com/HUPO-PSI/mzPeak-specification)
  · inspect `.mzpeak` files in your browser at [mzpeak.org/view](https://mzpeak.org/view)
- 🏗 [Platform support matrix](docs/PLATFORM_SUPPORT.md) · [Backlog](BACKLOG.md)
- 📦 [SBOM](sbom.cdx.json) (CycloneDX) · [Third-party notices](THIRD-PARTY-NOTICES.md) · [Changelog](CHANGELOG.md)

## Supported formats & operating systems

| Format | Linux | macOS | Windows | Notes |
|---|:---:|:---:|:---:|---|
| mzML, `.mzML.gz` | ✅ | ✅ | ✅ | gzip detected by magic and inflated to a temp copy; and `-o x.mzML.gz` writes gzipped mzML |
| imzML | ✅ | ✅ | ✅ | imaging coords + IMS CV |
| Bruker `.d` **TDF** (timsTOF) | ✅ | ✅ | ✅ | ion mobility; **ims-compact by default** |
| Bruker `.d` **TSF** (line spectra) | ✅ | ✅ | ✅ | MALDI/TOF |
| Thermo `.raw` | ✅ | ✅ | ✅ | needs a **.NET 8+ runtime** |
| Bruker `.d` **BAF** | ✅ | ❌ | ✅ | auto-built; `libbaf2sql_c` at runtime |
| Agilent `.d` (native, scan data) | ❌ | ❌ | ✅ | out-of-process **net48** host (`glue/agilent`) → MHDAC; since 0.11.0. MRM/SIM-only runs are refused (they are chromatograms) — use `--via-msconvert` for those ([details](docs/PLATFORM_SUPPORT.md)) |
| SciEX `.wiff` (native) | ❌ | ❌ | ✅ | in-process .NET glue (`glue/sciex`); Clearcore2 at runtime. MRM/SIM dwell runs are refused (they are chromatograms) — use `--via-msconvert`; multi-sample files take `--sample N` |
| Shimadzu `.lcd` (native) | ❌ | ❌ | ✅ | in-process .NET glue (`glue/shimadzu`); LabSolutions.IO at runtime — **needs a current ProteoWizard**, see [`glue/shimadzu/README.md`](glue/shimadzu/README.md) |
| Agilent / SciEX / … via msconvert | ✅ | ✅ | ✅ | `--via-msconvert`; needs ProteoWizard (Wine off-Windows) |

Thermo `.raw` and Bruker `.d` link their readers in automatically (no build flag).
The SciEX and Shimadzu native readers use a small .NET **glue** under `glue/`
(built once with `dotnet build`, pointed at via `MZPC_*_GLUE` + a ProteoWizard
install for the vendor DLLs — see each `glue/*/README.md`). Point `MZPC_PWIZ_DIR` at a
**current** ProteoWizard (3.0.26151 verified): an old one ships a Shimadzu library that
mispairs centroid intensities on profile-less `.lcd` files. Waters needs no glue —
`src/waters.rs` calls `MassLynxRaw.dll`'s C ABI directly. MHDAC needs .NET Framework, so the
Agilent C# side is a separate **net48 EXE** (`AgilentGlueHost.exe`) that the converter spawns
once per `.d` (restored in 0.11.0; it reads scan spectra — MRM/SIM runs stay on
`--via-msconvert`). Everywhere else, the cross-vendor `--via-msconvert` path covers them.

**Full matrix** — every format × OS, the runtime requirements (.NET 8 for Thermo,
.NET Framework 4.8 for Agilent, the vendor DLLs), and how to build/point at each glue
executable: **[docs/PLATFORM_SUPPORT.md](docs/PLATFORM_SUPPORT.md)**.

## Install

**macOS — Homebrew.** The tap lives in this repository, so one `brew tap` serves it:

```sh
brew tap okohlbacher/mzpeak https://github.com/okohlbacher/mzPeakConverter
brew install okohlbacher/mzpeak/mzpeak-convert          # formula (recommended)
brew install --cask okohlbacher/mzpeak/mzpeak-convert   # or the cask
```

Both install the released binary for your architecture — Apple silicon or Intel, no
Rust toolchain — as `mzpeak-convert`; install one, not both. Name the tap in full:
Homebrew 6 rejects a bare token from a tap it has not been told to trust, and the
two-argument `brew tap` is required because the repository is not named
`homebrew-mzpeak`. `brew upgrade` follows later releases and `brew uninstall` (add
`--cask` if that is how you installed it) removes it.

Prefer the formula. The binaries are ad-hoc signed rather than notarized by Apple, and
Homebrew quarantines every *cask* download, which macOS then refuses to execute — so
the cask has to strip that attribute itself and says so when it installs. Formula
downloads are not quarantined. Each archive is published with a `.sha256` sidecar.

**From source** (every platform):

```sh
git clone https://github.com/okohlbacher/mzPeakConverter.git
cd mzPeakConverter
cargo build --release          # → target/release/mzpeak-convert
```

Requires **Rust ≥ 1.88**. Thermo `.raw` conversion additionally needs a **.NET 8+
runtime** (the binary auto-sets `DOTNET_ROLL_FORWARD=LatestMajor` for newer
runtimes; the first build downloads the `nethost` loader). Nothing else is needed
for mzML/imzML/Bruker.

## Usage

A single command. Give an input and, optionally, an output:

```sh
# No --output → inspect only (prints a report, writes nothing)
mzpeak-convert run.raw

# Convert to mzPeak (-v also prints the inspection report)
mzpeak-convert run.raw -o run.mzpeak --force

# Bruker timsTOF (.d): lossless ims-compact is the DEFAULT (--no-ims-compact to disable)
# Layout defaults to "archive" (absolute TOF bins, max compression, fast whole-spectrum access)
mzpeak-convert experiment.d -o experiment.mzpeak

# Opt into the "chunked" layout for fast m/z-range / XIC queries (m/z-page-prunable, ~parity size)
mzpeak-convert experiment.d -o experiment.mzpeak --ims-chunked

# A format without a native reader in this build, via ProteoWizard
mzpeak-convert agilent.d -o out.mzpeak --via-msconvert

# Drive any option from a config file (CLI flags override it)
mzpeak-convert run.d -c mzpeak-convert.yaml
```

See the **[User Manual](docs/USER_MANUAL.md)** for every option and the config-file schema.

Exit codes: `0` ok · `1` generic error · `3` unsupported.

Conformance validation is intentionally **not** built in — validate archives with
the independent `mzpeak-validate` tool (the e2e harness in `tests/` calls it).

## Tests

```sh
cargo test                     # unit tests
tests/run_corpus_e2e.sh        # convert + mzpeak-validate over tests/corpus.tsv
tests/run_data_sweep.sh DIR    # full-corpus convert+validate sweep (parallel)
```

`tests/corpus.tsv` references real files in sibling data trees (nothing copied).

## License

[MIT](LICENSE) for the original sources. The repository vendors
`mzpeak_prototyping` under its upstream terms — see
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).
