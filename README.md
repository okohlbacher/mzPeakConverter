# mzPeakConverter

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.87%2B-orange.svg)](https://www.rust-lang.org)
[![Release](https://img.shields.io/badge/release-v0.2.0-green.svg)](https://github.com/okohlbacher/mzPeakConverter/releases)

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
ZIP of Apache Parquet facets + a JSON index — that is lossless, columnar, and
analysis-ready, preserving vendor metadata and ion-mobility structure.

## Documentation

- 📘 **[User Manual](docs/USER_MANUAL.md)** — every option, the config file,
  output layout, vendor metadata handling, requirements, troubleshooting.
- 🌐 The **mzPeak format**: [mzpeak.org](https://mzpeak.org) · spec repo
  [HUPO-PSI/mzPeak-specification](https://github.com/HUPO-PSI/mzPeak-specification)
  · inspect `.mzpeak` files in your browser at [mzpeak.org/view](https://mzpeak.org/view)
- 🏗 [Architecture & roadmap](PLAN.md) · [Native-TOF design](NATIVE-TOF-DESIGN.md) · [Handoff notes](HANDOFF.md)
- 📦 [SBOM](sbom.cdx.json) (CycloneDX) · [Third-party notices](THIRD-PARTY-NOTICES.md) · [Changelog](CHANGELOG.md)

## Supported formats & operating systems

| Format | Linux | macOS | Windows | Notes |
|---|:---:|:---:|:---:|---|
| mzML, `.mzML.gz` | ✅ | ✅ | ✅ | |
| imzML | ✅ | ✅ | ✅ | imaging coords + IMS CV |
| Bruker `.d` **TDF** (timsTOF) | ✅ | ✅ | ✅ | ion mobility; **ims-compact by default** |
| Bruker `.d` **TSF** (line spectra) | ✅ | ✅ | ✅ | MALDI/TOF |
| Thermo `.raw` | ✅ | ✅ | ✅ | needs a **.NET 8+ runtime** |
| Bruker `.d` **BAF** | ✅ | ❌ | ✅ | auto-built; `libbaf2sql_c` at runtime |
| Agilent `.d` (native) | ❌ | ❌ | ✅ | out-of-process **.NET FW 4.8** host (`glue/agilent`); MHDAC at runtime |
| SciEX `.wiff` (native) | ❌ | ❌ | ✅ | in-process .NET glue (`glue/sciex`); Clearcore2 at runtime |
| Agilent / SciEX / … via msconvert | ✅ | ✅ | ✅ | `--via-msconvert`; needs ProteoWizard (Wine off-Windows) |

Thermo `.raw` and Bruker `.d` link their readers in automatically (no build flag).
The SciEX/Waters/Agilent native readers use a small .NET **glue** under `glue/`
(built once with `dotnet build`, pointed at via `MZPC_*_GLUE` + a ProteoWizard
install for the vendor DLLs — see each `glue/*/README.md`). MHDAC needs .NET
Framework, so Agilent runs as a separate net48 EXE rather than in-process.
Everywhere else, the cross-vendor `--via-msconvert` path covers them.

## Install

```sh
git clone https://github.com/okohlbacher/mzPeakConverter.git
cd mzPeakConverter
cargo build --release          # → target/release/mzpeak-convert
```

Requires **Rust ≥ 1.87**. Thermo `.raw` conversion additionally needs a **.NET 8+
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
mzpeak-convert experiment.d -o experiment.mzpeak

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
