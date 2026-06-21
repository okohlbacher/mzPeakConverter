# mzPeakConverter

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.87%2B-orange.svg)](https://www.rust-lang.org)
[![Release](https://img.shields.io/badge/release-v0.1.0-green.svg)](https://github.com/okohlbacher/mzPeakConverter/releases)

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

- 📘 **[User Manual](docs/USER_MANUAL.md)** — full functionality, every command
  and option, formats, output layout, requirements, troubleshooting.
- 🏗 [Architecture & roadmap](PLAN.md) · [Native-TOF design](NATIVE-TOF-DESIGN.md) · [Handoff notes](HANDOFF.md)
- 📦 [SBOM](sbom.cdx.json) (CycloneDX) · [Third-party notices](THIRD-PARTY-NOTICES.md) · [Changelog](CHANGELOG.md)

## Supported inputs

| Format | Reader | Notes |
|---|---|---|
| mzML / `.mzML.gz` | mzdata | full metadata + chromatograms |
| imzML | mzdata | imaging coordinate columns + IMS CV promoted |
| Bruker `.d` (**TDF**, timsTOF) | mzdata `bruker_tdf` + native `timsrust` | ion mobility preserved; `--ims-compact` for lossless integer-TOF |
| Bruker `.d` (**TSF**, line spectra) | ported reader (rusqlite + zstd) | MALDI / TOF line spectra |
| Thermo `.raw` | mzdata `thermo` (.NET) | verbatim scan-trailer facet |
| Bruker `.d` (BAF) | `--features bruker_sdk` | needs `libbaf2sql_c` (Windows/Linux) |
| Agilent `.d`, SciEX `.wiff` | `--via-msconvert` (or `--features agilent`/`sciex`) | native readers Windows-only |

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

```sh
# Convert (output path inferred), then round-trip verify
mzpeak-convert convert sample.mzML
mzpeak-convert convert run.raw -o run.mzpeak --verify --force

# Bruker timsTOF with lossless integer-TOF storage
mzpeak-convert convert experiment.d -o experiment.mzpeak --ims-compact

# Inspect / cross-vendor fallback
mzpeak-convert inspect run.raw
mzpeak-convert convert agilent.d -o out.mzpeak --via-msconvert
```

See the **[User Manual](docs/USER_MANUAL.md)** for the complete option reference.

Exit codes: `0` ok · `1` generic error · `3` unsupported.

Conformance validation is intentionally **not** built in — validate archives with
the independent `mzpeak-validate` tool (the e2e harness in `tests/` calls it).

## Tests

```sh
cargo test                     # unit tests
tests/run_corpus_e2e.sh        # convert + --verify + mzpeak-validate over tests/corpus.tsv
tests/run_data_sweep.sh DIR    # full-corpus convert+validate sweep (parallel)
```

`tests/corpus.tsv` references real files in sibling data trees (nothing copied).

## License

[MIT](LICENSE) for the original sources. The repository vendors
`mzpeak_prototyping` under its upstream terms — see
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).
