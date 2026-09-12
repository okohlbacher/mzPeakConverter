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
are bit-exact (integer TOF, the fixed-point m/z lattice, centroid m/z). Four general
signal transforms are not, and each is named in the archive with its bound: the default
**numpress-linear** chunk encoding of profile m/z (`--no-numpress` for lossless delta),
**zero-run compaction** of profile baselines (consecutive zeros collapse to one at each
peak boundary), the **`--tof-grid` sqrt grid**, accepted only within a ppm bound
(`MZPC_TOF_GRID_PPM`, default 5), and the **Shimadzu profile pad trim** (the
zero-intensity pad at the scan-window bounds outside the signal span is not stored).
Beside them, a lane declares each change of its own when it makes one (a vendor
library's NaN intensity stored as 0, a Thermo isolation window of unstated width written
target-only, chromatogram times in seconds stored in minutes, …); the user manual's §8
table lists every entry.

## Documentation

- 📘 **[User Manual](docs/USER_MANUAL.md)** — every option, the config file,
  output layout, vendor metadata handling, requirements, troubleshooting.
- 🌐 The **mzPeak format**: [mzpeak.org](https://mzpeak.org) · spec repo
  [HUPO-PSI/mzPeak-specification](https://github.com/HUPO-PSI/mzPeak-specification)
  · inspect `.mzpeak` files in your browser at [mzpeak.org/view](https://mzpeak.org/view)
- 🏗 [Platform support matrix](docs/PLATFORM_SUPPORT.md) · [Backlog](BACKLOG.md)
- 📦 SBOM (CycloneDX, `mzpeak-convert-<version>.cdx.json` on each [release](https://github.com/okohlbacher/mzPeakConverter/releases)) · [Third-party notices](THIRD-PARTY-NOTICES.md) · [Changelog](CHANGELOG.md)

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
| SciEX `.wiff` (native) | ❌ | ❌ | ✅ | in-process .NET glue (`glue/sciex`); Clearcore2 at runtime. MSn precursors (selected ion, isolation window, collision energy) are read but not yet run on Windows. MRM/SIM dwell runs are refused (they are chromatograms) — use `--via-msconvert`; multi-sample files take `--sample N` |
| Shimadzu `.lcd` (native) | ❌ | ❌ | ✅ | in-process .NET glue (`glue/shimadzu`); LabSolutions.IO at runtime — **needs a current ProteoWizard**, see [`glue/shimadzu/README.md`](glue/shimadzu/README.md) |
| Waters `.raw` (native) | ❌ | ❌ | ✅ | `MassLynxRaw.dll` called directly, no .NET glue (`MZPC_MASSLYNX_DIR`, else `MZPC_PWIZ_DIR`); HDMSe/HDDDA functions are written as frames with a per-point drift time |
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

**macOS — Homebrew.** The tap is this repository, so three commands install a released
build with no Rust toolchain:

```sh
brew trust --cask okohlbacher/mzpeak/mzpeak-convert
brew tap okohlbacher/mzpeak https://github.com/okohlbacher/mzPeakConverter
brew install --cask okohlbacher/mzpeak/mzpeak-convert
```

All three names must be given in full. Homebrew 6 refuses to load anything from a
third-party tap until you trust it, and it will not guess the repository URL for a tap
whose repository is not named `homebrew-mzpeak`. You get `mzpeak-convert` for your
architecture, Apple silicon or Intel; `brew upgrade --cask okohlbacher/mzpeak/mzpeak-convert`
follows later releases and `brew uninstall --cask` removes it.

The binaries are signed with a Developer ID certificate and notarized by Apple, so Homebrew's
download quarantine is no longer something the cask has to work around. A `.tar.gz` cannot carry
a stapled ticket, so the first run checks with Apple over the network and every run after that is
offline. Each archive is published with a `.sha256` sidecar you can check with `shasum -c`.

**Linux and Windows — release archives.** Every release also publishes a ready-to-run build
for Linux on x86_64 and aarch64 (glibc 2.28 or newer: RHEL/Rocky/Alma 8 and later, Debian 10+,
Ubuntu 20.04+) and for Windows on x86_64 and ARM64. Download the archive for your platform from
[Releases](https://github.com/okohlbacher/mzPeakConverter/releases) and check it against its
`.sha256`:

```sh
tar xzf mzpeak-convert-<version>-x86_64-unknown-linux-gnu.tar.gz && ./mzpeak-convert --version
```

The Windows `.zip` unpacks to a folder holding `mzpeak-convert.exe` and, under `glue\`, the .NET
glue for the native SciEX, Shimadzu and Agilent readers. Releases after 0.11.5 find that folder by
themselves; the `MZPC_*_GLUE` variables still override it, and 0.11.5's archive needs them set.
Those readers also need the vendor's own DLLs, taken from a ProteoWizard install (`MZPC_PWIZ_DIR`):
they carry vendor licences and are not redistributed. On Windows ARM64 the vendor readers are
unverified — the vendor DLLs are x64 builds — so for those formats use the x64 archive, which
Windows 11 runs under emulation. Thermo `.raw` needs a .NET 8+ runtime on every platform.

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
cargo test --release           # the test suite CI runs
```

Use the release profile, as CI does: the vendored writer carries `debug_assert`s that a plain
debug `cargo test` can trip on inputs the release build handles. The suite also needs a **.NET 8+
runtime**, as Thermo `.raw` conversion does: `tests/thermo_raw.rs` converts the committed
`tests/data/small.RAW` and fails without one. The tests that need data too large
to commit — real timsTOF runs, a Bruker TSF acquisition, lane pairs built on the Windows box — are
`#[ignore]`d, so a run without them reports them as not run rather than as passed. With the
reference corpus:

```sh
MZPEAK_CORPUS=/path/to/mzpeak-example-data/data MZPC_REQUIRE_CORPUS=1 \
  cargo test --release -- --include-ignored
```

`MZPC_REQUIRE_CORPUS=1` turns a missing corpus fixture into a failure instead of a skip
(`tests/common/corpus.rs`). The TSF and lane-parity pins need inputs no corpus holds — point
`MZPC_TSF_FIXTURE` at a Bruker TSF `.d` and `MZPC_LANE_PAIRS` at pairs built on the Windows box — and
without them those three tests still skip, even under `MZPC_REQUIRE_CORPUS=1`. `tests/fixtures/README.md`
lists where the corpus-derived fixtures come from.

`tests/run_corpus_e2e.sh` (over `tests/corpus.tsv`) and `tests/run_data_sweep.sh` are older
convert-and-validate harnesses over files in sibling data trees; nothing runs them.

## License

[MIT](LICENSE) for the original sources. The repository vendors
`mzpeak_prototyping` under its upstream terms — see
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).
