# mzPeakConverter — User Manual

> [!IMPORTANT]
> The **mzPeak format is still going through the HUPO-PSI specification process**
> (currently draft v0.9). This converter is a **technical demonstrator, not a
> production tool yet** — output layout and semantics may change as the spec evolves.

`mzpeak-convert` converts mass-spectrometry raw and exchange formats into the
**mzPeak** format. It reads through [`mzdata`](https://github.com/mobiusklein/mzdata)
(plus native readers for formats mzdata does not cover) and writes through the
reference `mzpeak_prototyping` writer.

It is a **single command**: give it an input and, optionally, an output.

- [1. What it does](#1-what-it-does)
- [2. Installation & requirements](#2-installation--requirements)
- [3. Quick start](#3-quick-start)
- [4. Command-line options](#4-command-line-options)
  - [4.1 mzML output](#41-mzml-output---to-mzml--o-xmzml--o-xmzmlgz)
  - [4.2 Filtering an existing archive](#42-filtering-an-existing-archive-mzpeak-input-with---rt---ms-level---drop-aux)
  - [4.3 Embedding sample metadata and images](#43-embedding-sample-metadata-and-images---sdrf---image)
- [5. Configuration file](#5-configuration-file)
- [6. Supported formats & operating systems](#6-supported-formats--operating-systems)
- [7. The mzPeak output](#7-the-mzpeak-output)
- [8. Vendor-specific metadata handling](#8-vendor-specific-metadata-handling)
- [9. Compression, layout & ims-compact](#9-compression-layout--ims-compact)
- [10. Exit codes & environment](#10-exit-codes--environment)
- [11. Native vendor-SDK readers](#11-native-vendor-sdk-readers)
- [12. Dependencies](#12-dependencies)
- [13. Troubleshooting](#13-troubleshooting)

---

## 1. What it does

`mzpeak-convert <input> [-o <output>] [options]` does one of two things:

- **With `--output`** — converts the input acquisition to a single `.mzpeak`
  archive (a STORED ZIP of Apache Parquet facets + a JSON index) that is columnar and
  analysis-ready, preserves vendor metadata and ion-mobility structure, and preserves the
  vendor's signal **to a stated fidelity with every applied transformation declared** in the
  index (`transformations`) — see §8 for what is not bit-exact and the entry that names each change.
- **Without `--output`** — writes nothing; it just **inspects** the input and prints
  a report (format, spectrum count, chromatogram count).

Passing `-v` prints that same inspection report *and still performs the conversion*. Beside a
conversion the report never opens a vendor library — Thermo RawFileReader, Bruker baf2sql, Agilent
MHDAC, SciEX, Waters or Shimadzu (the conversion opens its own, and `--via-msconvert` needs none) —
and a report that fails is a `note:` line, not the run's error.

## 2. Installation & requirements

| Requirement | Notes |
|---|---|
| Rust ≥ 1.88 | edition 2024; install via <https://rustup.rs> |
| C toolchain | for the bundled native libs (SQLite is compiled from source) |
| .NET 8+ runtime | **only for Thermo `.raw`**; auto-rolls-forward to 9/10 |

**macOS, from a release (Homebrew).** The tap is the converter's own repository:

```sh
brew trust --cask okohlbacher/mzpeak/mzpeak-convert
brew tap okohlbacher/mzpeak https://github.com/okohlbacher/mzPeakConverter
brew install --cask okohlbacher/mzpeak/mzpeak-convert
```

Name all three in full: Homebrew 6 will not load a third-party tap's cask before you
trust it, and a tap whose repository is not called `homebrew-mzpeak` needs the URL
spelled out. This installs the published binary for the machine's architecture, so no
Rust toolchain is needed.

Since 0.12.2 the released macOS binaries are signed with a Developer ID certificate and
notarized by Apple, so the cask no longer has to strip Homebrew's download quarantine.
A `.tar.gz` cannot carry a stapled ticket — no archive format can — so the first run
checks with Apple over the network and every run after that is offline. Check any archive
against the `.sha256` published beside it (`shasum -c`).

**Linux and Windows, from a release.** Each release publishes Linux x86_64 and aarch64
archives (`.tar.gz`, glibc 2.28 or newer, so RHEL/Rocky 8 and 9 clusters included) and Windows
x86_64 and ARM64 archives (`.zip`), each with a `.sha256` beside it. The Windows folder carries the
.NET glue for the native SciEX, Shimadzu and Agilent readers under `glue\`, which releases after
0.11.5 find without any `MZPC_*_GLUE` variable; the vendor DLLs themselves still come from a
ProteoWizard install (`MZPC_PWIZ_DIR`, §11). The vendor readers are unverified on Windows ARM64,
where the x64 archive runs under emulation.

**Any platform, from source:**

```sh
git clone https://github.com/okohlbacher/mzPeakConverter.git
cd mzPeakConverter
cargo build --release          # binary at target/release/mzpeak-convert
```

Non-Thermo conversions need no .NET. See §11 for the native vendor-SDK readers.

## 3. Quick start

```sh
# Inspect only — prints a report, writes nothing
mzpeak-convert run.raw

# Convert to mzPeak
mzpeak-convert run.raw -o run.mzpeak

# Convert and print the inspection report too
mzpeak-convert run.raw -o run.mzpeak -v --force

# Bruker timsTOF (.d): lossless ims-compact integer-TOF is the DEFAULT
mzpeak-convert experiment.d -o experiment.mzpeak              # ims-compact
mzpeak-convert experiment.d -o experiment.mzpeak --no-ims-compact   # standard f64 m/z

# A format without a native reader in this build, via ProteoWizard
mzpeak-convert agilent.d -o out.mzpeak --via-msconvert
```

## 4. Command-line options

`mzpeak-convert [OPTIONS] <INPUT>`

The table follows `mzpeak-convert --help` of the shipped binary (the wording is the help's own,
shortened; `tests/docs_drift.rs` fails when an option has no row here). `--help` is the long form;
`-h` prints a one-line summary per option.

| Option | Default | Description |
|---|---|---|
| `<INPUT>` | — | Input file or vendor directory (mzML / `.mzML.gz` / imzML, Bruker `.d`, Thermo `.raw`, …; positional, required) |
| `-o, --output <OUTPUT>` | *(none → inspect only)* | Output path. `.mzpeak` or `.mzML` — the format is inferred from the extension, any case; `.mzML.gz` writes gzip-compressed mzML (§4.1). **Any other name is refused** (exit 1) unless `--to` states the format: through 0.16.0 `-o run.imzML`, any unknown extension, or none wrote an mzPeak archive under that name. If omitted, **nothing is written** — the input is only inspected and a report (format, spectra, chromatograms) is printed |
| `-c, --config <CONFIG>` | — | Config file (YAML) setting defaults for any option below; explicit command-line flags win (§5) |
| `--layout <chunked\|point>` | `chunked` | Signal layout: `chunked` m/z layout (numpress-linear or delta); `point` — flat point layout, one row per m/z–intensity pair (§9) |
| `--to <mzpeak\|mzml>` | inferred from the `-o` extension (`.mzML` / `.mzML.gz` → `mzml`, `.mzpeak` → `mzpeak`) | `mzml` writes a plain mzML (vendor → mzML) instead of mzPeak, bypassing the mzPeak-specific encoders (§4.1). Required when the output name has any other extension, or none; it wins over the extension |
| `--no-numpress` | off | Delta m/z chunking instead of the default lossy numpress-linear: each m/z is stored as its difference from the one before. Exact for m/z that are 32-bit values (most imzML) and wherever a 64-bit m/z is at most twice its predecessor; a 64-bit m/z more than twice its predecessor in the same chunk can come back one unit in the last place off (1e-15 Da near m/z 10, 5e-13 Da near m/z 4000), and so can the values after it in that chunk. This happens at any mass: a sparse centroid list is cut into chunks far wider than `--chunk-size`, since a chunk is never one point long (6 of 360 points of a ToF-SIMS-like test file; 7 of 6,281 in 200 generated centroid spectra over m/z 50–5000, 3 of them above m/z 1000). The archive declares it (`delta-ulp` in `transformations`), and the `fidelity` block counts the chunks this can happen in and bounds the error (§8). Profile zero runs are still masked. For a bit-exact archive use `--lossless` |
| `--keep-zero-runs` | off | Store every profile point: the writer's zero-run mask (`zero-run-mask`, §8) is off, no profile point is dropped and the entry is not declared. **Continuous-mode imaging data** (imzML `IMS:1000030`), whose pixels share one m/z axis, keeps its zero runs **without this flag** since 0.17.0 (§8, imaging): masked, every pixel kept a different subset of the axis under its own numpress fixed points, so one source m/z decoded to several values across pixels (171.33333 to 8 values in the 9 pixels of `Example_Continuous`; one value now, and `metadata.imaging.shared_mz_axis` says the file's spectra did hold one array). The flag remains the override for processed-mode imzML and any other profile input. `MZPC_KEEP_ZERO_RUNS=1` does the same from the environment (§10). Refused on `--agilent-grid`, whose reader leaves the zero samples out itself; inert on the timsTOF ims-compact lanes, whose frames hold no zero-intensity point; on the native Shimadzu sqrt-grid route the zero pad at the scan-window bounds stays out (`shimadzu:span-trim`, with a warning) |
| `--keep-contact` | off | **mzML/imzML input:** carry the header's `fileDescription/<contact>` — contact name `MS:1000586`, affiliation `MS:1000590`, address `MS:1000587`, URL `MS:1000588`, e-mail `MS:1000589`, every param as stated — into the archive's `file_description.contacts` (the spec's own slot: `contact_name`, `contact_affiliation`, `parameters`) and into the direct mzML export's `<fileDescription>`, after the source files. **By default it is dropped**, with one note naming the contact when a header states one: an archive is copied and published, and a name, an e-mail and a street address would travel with every copy (§4.1, §8). The export of an archive writes whatever its index holds, flag or not; inert on the `.mzpeak` input lanes and on every input without an mzML header. Config key `keep_contact` (§5) |
| `--lossless` | off | A bit-exact archive, or none (§9): every point of the input stored in the input's order, each m/z and intensity with exactly the value the input holds. Selects the point layout with zero runs kept and no numpress, m/z lattice or TOF grid; after writing, the conversion fails (exit 1, nothing written) unless no signal transformation is declared, every point is stored and no column is narrower than the input declares. **mzML and imzML inputs only**; refused on every other lane, and for a Thermo `.raw` or a TDF on the standard lane. Conflicts with `--layout chunked`, `--tof-grid auto\|on`, `--agilent-grid` and an mzML output, and is refused while `MZPC_MAX_SPECTRA` is set (§10) |
| `--no-mz-lattice` | off | Keep exact f64 m/z for centroid lists that sit on a fixed-point **lattice** (Shimadzu `MassHigh`, the LabSolutions mzML export) instead of the reference implementation's fitted linear grid — on every lane, the native Shimadzu `.lcd` one included (`MZPC_NO_MZ_LATTICE=1` does the same from the environment). Use it when the centroid m/z must survive to the last bit rather than to 1e-6 Da (§9). Data that is not on a lattice is unaffected either way |
| `--chunk-size <CHUNK_SIZE>` | `50` | m/z chunk width (Th) for the chunked layout |
| `--zstd-level <ZSTD_LEVEL>` | `3` (timsTOF ims-compact lanes: `22`) | Zstd compression level (1–22). The ims-compact lanes default to 22: their archives are written once and read many times, and 22 is 1.4 % smaller than 5 on PXD059079's 2485.d. An explicit value applies to every lane (§9) |
| `-f, --force` | off | Overwrite the output if it already exists; also converts an imzML whose `.ibd` begins with another UUID than the header states — refused otherwise, since the two files may not be the pair the imzML describes (§8, imaging; forced, the mismatch is warned about, declared as `imzml:ibd-uuid-mismatch` and recorded) |
| `--no-ims-compact` | off | Bruker timsTOF (TDF) only: disable the default lossless ims-compact integer-TOF storage and write standard f64 m/z instead |
| `--representation <both\|profile\|centroid>` | `both` | Which signal representation to read when a vendor supplies BOTH profile and centroid for the same spectrum (Shimadzu `.lcd` does). `both` is faithful to the raw data: profile goes to `spectra_data`, centroid to `spectra_peaks`, and the metadata row carries both `number_of_data_points` and `number_of_peaks`. `profile` / `centroid` force one view; a representation the file does not contain is a warning, not an error — the other one is written. Honoured by the Shimadzu `.lcd` and Bruker BAF readers (BAF: mzPeak output only) |
| `--ims-chunked` | **on** | Bruker timsTOF (TDF) ims-compact only: 50-Th chunks (`--chunk-size` overrides the width) on the reference implementation's chunk grid — every chunk row keeps its real m/z bounds (page-prunable: m/z window queries read only the chunks they need) and its points as integer TOF bins and TIMS scan numbers under the frame's own vendor calibration models (§9). Passing the flag explicitly is inert and says so |
| `--no-ims-chunked` | off | Bruker timsTOF (TDF) ims-compact only: one chunk per frame instead of 50-Th chunks — the same grid rows, whole-frame access in one row, no m/z pruning within a frame. (Through 0.13 this selected a flat point table of absolute TOF bins; that layout is gone) |
| `--bruker-sdk` | off | Read Bruker TDF/TSF `.d` via the official Bruker timsdata SDK (parallel path to the default pure-Rust readers; Windows/Linux only, needs `timsdata.dll` / `libtimsdata.so`, found through `TIMSDATA_LIB_DIR` (§10) or the loader's search path). On a TDF still writes the lossless ims-compact layout; add `--no-ims-compact` for f64 m/z |
| `--no-tims-recalibration` | off | Bruker timsTOF (TDF): disable this converter's scan→1/K0 calibration (the `TimsCalibration` ModelType-2 model, `W = C2 + (C3−C2)(scan−C4−C0)/C1`, `1/K0 = W/(C7 + C6·W)` — identical to the Bruker SDK's `tims_scannum_to_oneoverk0` to 6e-16 on every corpus run) and use timsrust's linear approximation of the nominal acquisition range (up to 0.03 Vs·s/cm² off). The model and the vendor's rows are declared in the `vendor_tims_calibration` index block. Recalibration is ON by default. On the ims-compact grid (§9) the flag stores 1/K0 as plain values (`ion_mobility_grid.column: null`), with a warning: the grid stores 1/K0 as TIMS scan numbers under the exact model, which the linear approximation is not on. The ims-compact path applies the choice to arrays and params alike; `--no-ims-compact` takes its mobility ARRAYS from mzdata's TDF reader, which applies the same ModelType-2 calibration itself, unconditionally, so there the flag switches only the precursor/scan/window-limit 1/K0 params (and warns that the arrays stay on the model). INERT with `--to mzml`, and says so: that export keeps every 1/K0 on the model its mobility arrays use, so each diaPASEF window's limits bracket its own peaks (on timsrust's linear map they would miss 9 % of them) |
| `--no-vendor` | off | Do not embed vendor side-files into the archive (§8) |
| `--no-chromatograms` | off | Do not synthesize TIC + base-peak chromatograms from the MS1 spectra. By default a TIC and a base-peak chromatogram are summed over the MS1 spectra, each only when the source carries no chromatogram of that kind; every chromatogram the source carries is stored in any case |
| `--aux <AUX>` | — | Vendor side-file rule (repeatable): `glob=embed` or `glob=drop`, the glob matched in any letter case against a file's name or its `/`-separated path inside the vendor directory. Highest precedence (§8) |
| `--image <IMAGE>` | — | **standard-lane inputs (mzML/imzML, Thermo `.raw`, TDF with `--no-ims-compact`, `--via-msconvert`), imaging runs only:** embed an optical image VERBATIM into the archive as `images/image_NNNN.<ext>` with a `metadata.imaging` overlay affine (image extent on grid extent; none on a Bruker MALDI run acquired from a FlexImaging sequence, §4.3). Repeatable. A bad/missing path, or a run with no pixel positions, ERRORS the conversion (strict). An `<input-stem>-opticalimage.{tif,tiff,png,jpg}` sibling is additionally auto-discovered (best-effort: warn + skip if unreadable or the run is not imaging) (§4.3) |
| `--sdrf <SDRF>` | — | **standard-lane inputs (mzML/imzML, Thermo `.raw`, TDF with `--no-ims-compact`, `--via-msconvert`):** embed an SDRF (sample-metadata) TSV VERBATIM as `sample_metadata/sdrf.tsv` with `metadata.study` + `metadata.sample_metadata` back-refs. A missing/unreadable path ERRORS the conversion (§4.3) |
| `--rt <MIN-MAX>` | — | mzPeak input only: keep spectra whose time is within MIN-MAX (unit matches the stored `spectrum.time`) (§4.2) |
| `--ms-level <MS_LEVEL>` | — | mzPeak input only: keep spectra with these MS levels (repeatable or comma-list) (§4.2) |
| `--drop-aux <DROP_AUX>` | — | mzPeak input only: drop archive members matching this glob (repeatable) (§4.2) |
| `--tof-grid <off\|auto\|on>` | `off` on mzML lanes; native SCIEX `.wiff`: `auto` when the flag is absent | **Inputs read through mzdata only** (mzML incl. `--via-msconvert`, imzML, Thermo `.raw`, and a TDF read as f64 m/z under `--no-ims-compact` or the ims-compact fallback): compactify exact-lattice TOF profile data by DETECTING an integer flight-time grid in the decoded f64 m/z and storing each spectrum as a chunk-grid row (`MS:1003825` model `[c0, c1, 1]` + integer indices), recovering `m/z = (c0 + c1·k)²`. Bounded-lossy (reconstruction within `MZPC_TOF_GRID_PPM`). `auto` applies it when a strict fit passes; `on` requires the fit (errors otherwise); `off` keeps exact f64. An imaging run (§8) skips it, with a warning. The native vendor lanes other than SCIEX ignore it: the timsTOF ims-compact lanes and `--agilent-grid` store the integer grid of the vendor calibration (lossless), and the Bruker TSF/BAF, Agilent MHDAC, Waters and Shimadzu lanes store the m/z their reader returns (on MHDAC a warning names the alternatives: `--via-msconvert --tof-grid`, or `--agilent-grid` for the flight-time grid of a profile `.d`). **Since 0.10.1 a gridded spectrum keeps the representation its source declares**: a profile spectrum's `tof_index` is filed in `spectra_data` (point layout), a centroid spectrum's in `spectra_peaks`; both facets declare the axis beside an f64 `mz` that is NULL on gridded rows, so `number_of_data_points` / `number_of_peaks` describe the source (until 0.10.0 every gridded spectrum was forced to centroid to reach the one facet that knew the axis — §9). **Native SCIEX `.wiff` (Windows):** Clearcore2 hands over decoded f64 m/z only, so that lane also fits the grid statistically; there the default (flag absent) is `auto` — the per-spectrum fit, unchanged from earlier releases — `off` stores the exact f64 m/z the vendor library returned (the opt-out the fidelity invariant requires), and `on` errors when no run-wide digitizer clock can be fitted |
| `--agilent-grid` | off | Agilent Q-TOF **profile** `.d` only: read the integer flight-time grid straight from `AcqData/MSProfile.bin` (pure Rust, no MHDAC/msconvert) and store the vendor's bin ordinals as chunk-grid rows, each scan under its own `MS:1003825` model (the MassHunter calibration drifts per scan) — in `spectra_data`, since it is profile data. The grid is the bare quadratic; MassHunter's polynomial refinement (up to ~7.5 ppm) rides verbatim in the `agilent_calibration` index block (§8). Far smaller than the msconvert lane (≈0.14×). Only applies when `MSProfile.bin` is non-empty (centroid-only `.d` fall through to the standard path) |
| `--sample <N>` | — | SciEX `.wiff` only: convert sample `N` (1-based) of a multi-sample WIFF. Native lane and both `--via-msconvert` lanes, mzPeak and `--to mzml` (mapped to msconvert's `--runIndexSet N-1`). A multi-sample WIFF without it is refused: the native lane lists its samples, the msconvert lanes give their count once msconvert has written every run. `0`, and a number beyond the file's samples, are refused; on any other input it is inert and warned about. Config key `sample` |
| `--via-msconvert` | off | Read the input via ProteoWizard `msconvert` (→ mzML → mzPeak). Cross-vendor path for formats without a native reader in this build (Agilent `.d`, SciEX `.wiff`, …) |
| `--msconvert-path <MSCONVERT_PATH>` | `$MSCONVERT_PATH`, else `msconvert` on `PATH` | Path to the `msconvert` executable |
| `-v, --verbose` | off | Verbose: print the inspection report and debug logs (repeat `-vv` for trace logs). An explicit `-v` / `-q` WINS over `RUST_LOG`; `RUST_LOG` is consulted only when neither flag is given (default level `info`) |
| `-q, --quiet` | off | Silence all logs except errors (wins over `RUST_LOG`, see `-v`) |
| `-h, --help` / `-V, --version` | — | Print help (`-h` for the summary) / print version |

**Options a lane cannot honour are refused, not dropped.** Since 0.9.13 the converter checks the
options you actually passed **on the command line** — never built-in defaults, and never a
config-file value (a config is a standing profile: its values take effect as defaults but cannot
make a lane refuse) — against the lane it selected, *before* any reader is opened. An option the
lane would **drop**, so that the output came out without it, exits 1 naming the option, the lane
and the remedy, e.g. `--sdrf is not honoured by the timsTOF ims-compact lane: the archive would be
written WITHOUT it and exit 0. convert first, then add them on the archive: …`; earlier releases
wrote the archive without the option and exited 0. An option that merely **cannot change** the
lane's output is named in a warning (`--layout is inert on …`) and the run goes on. The two columns
are `dropped_flags_for` and `inert_flags_for` in `src/main.rs`:

| Lane (how it is selected) | Refused (exit 1) | Warned; the run goes on |
|---|---|---|
| `.mzpeak` → `.mzpeak` filter (§4.2) | — | `--layout --no-numpress --keep-zero-runs --keep-contact --no-mz-lattice --chunk-size --no-ims-compact --representation --ims-chunked --no-ims-grid --bruker-sdk --no-tims-recalibration --no-chromatograms --aux --tof-grid --agilent-grid --via-msconvert --msconvert-path` (the filter re-packs Parquet members verbatim, so `--zstd-level 12` cannot change the output; an archive carries the contacts its index holds, or none). `--ims-grid` is NOT a filter: it rewrites the timsTOF peaks facet into the grid layout (`--zstd-level` and `--grid-encoding` apply to that facet) and copies everything else |
| `.mzpeak` → mzML export (§4.1) | `--image --sdrf --aux` | the filter lane's list without `--aux` |
| `--to mzml` / `-o x.mzML` from a raw or exchange format (§4.1) | `--image --sdrf --aux --bruker-sdk` (the export runs before the SDK backend is chosen, so it never uses it) | `--layout --no-numpress --keep-zero-runs --no-mz-lattice --chunk-size --zstd-level --no-ims-compact --ims-chunked --no-ims-chunked --no-ims-grid --ims-grid --grid-encoding --no-tims-recalibration --no-chromatograms --tof-grid --agilent-grid` |
| `--agilent-grid` on a profile `.d` | `--image --sdrf --via-msconvert --keep-zero-runs` (the reader leaves the zero samples out itself, `agilent:drop-zero-samples`) | `--layout --no-numpress --chunk-size` |
| `--via-msconvert` | `--aux` (the intermediate mzML is the source, so no vendor side-file of the original input can be embedded) and `--bruker-sdk --no-ims-compact --ims-chunked --no-ims-chunked --no-ims-grid --grid-encoding --no-tims-recalibration` (msconvert is chosen before any native backend); `--image` / `--sdrf` ARE embedded since 0.9.13 | — |
| `--bruker-sdk` on a TDF (ims-compact) | `--image --sdrf --no-tims-recalibration` (the SDK lane's 1/K0 comes from the vendor's own scan→1/K0 model) | `--layout --no-numpress --keep-zero-runs` |
| `--bruker-sdk` on a TSF, or a TDF with `--no-ims-compact` | `--image --sdrf --ims-chunked --no-ims-chunked --no-tims-recalibration` | — |
| default timsTOF (TDF) ims-compact | `--image --sdrf` | `--layout --no-numpress --keep-zero-runs` |
| native vendor readers (TSF / BAF / Agilent / `.wiff` / Waters / `.lcd`) | `--image --sdrf` | `--ims-chunked --no-ims-chunked` |
| standard mzdata lane (mzML / imzML / Thermo `.raw` / TDF f64) | — | `--ims-chunked --no-ims-chunked` (also when a timsTOF run falls back to this lane because timsrust cannot decompress it) |

`--lossless` is refused (exit 1) on every lane but the standard one, and there for an input that
is not an mzML or imzML (a Thermo `.raw`, a TDF read as f64): its check compares the stored signal
with what the file declares, which only those two formats state. Unlike the options above it is
refused when it comes from the config file too: it is a promise about the archive, not a codec
preference. For the same reason it is refused while `MZPC_MAX_SPECTRA` is set, whether or not the
cap would bite: a capped run stops early with the completeness check off, and its archive would be
exact in what it holds and silent about what it lacks.

Options a lane has no use for that appear in neither column (`--no-vendor` on an mzML export,
`--tof-grid` on the native Bruker/Agilent lanes, `--bruker-sdk` on a non-Bruker input converted to
mzPeak, `--sample` on anything but a SciEX `.wiff`, `--aux` on a single-file input) are accepted
without a refusal, so a shared recipe or config keeps working; of those, `--tof-grid` on the Agilent
MHDAC lane, `--sample` and `--aux` log a warning. Config-file values never count as supplied, so a
shared profile carrying
`zstd_level: 12` or `sdrf:` sets defaults for the lanes that use them
and is a silent no-op on the lanes that cannot — put such an option on the command line when you
want the refusal to protect you.

### 4.1 mzML output (`--to mzml`, `-o x.mzML`, `-o x.mzML.gz`)

An output name ending in `.mzML`, in any case (or `--to mzml` with any name), writes a **plain mzML** through
the mzdata writer, streaming the read spectra straight through — no mzPeak encoder runs (no
ims-compact, TOF grid, chunking, byte-plane or side-file embedding). It covers every format the
tool reads: everything mzdata reads directly (mzML/imzML, Thermo `.raw`, Bruker TDF) plus the
Windows-native vendor readers (SciEX/Waters/Agilent/Shimadzu, Bruker TSF/BAF); with
`--via-msconvert` msconvert writes the output mzML itself. A name ending in `.mzML.gz` writes
gzip-compressed mzML, compressed as it is written in one pass. An existing `.mzpeak` archive can
also be exported to mzML (`mzpeak-convert a.mzpeak -o a.mzML`), optionally through the filters of
§4.2. All mzML exports are atomic: the file is written as `x.mzML.tmp` (`x.mzML.tmp.gz`) and
renamed into place only on success, so a failed run never leaves a partial `.mzML` and never
destroys a previous output under `--force`.

Every mzML the tool writes itself records the conversion as the default processing of its
`spectrumList` and `chromatogramList` (`defaultDataProcessingRef`), which mzML 1.1 requires and
stock OpenMS 3.5 needs to read the file: a `dataProcessing` (`mzpeak_convert_to_mzml`) whose last
method is software `mzpeak-convert` doing MS:1000544 `Conversion to mzML`, with the command line
(paths reduced to their file names) as a `conversion options` userParam. For a source that states
processing of its own (an mzML), the entry first repeats the methods of the processing the source's
spectra point at by default, as msconvert does, and the source's entries follow it unchanged; a
source that states none (every raw vendor format) gets the step alone. An archive's export
continues the archive's history: after the methods of the archive's default processing come those
of the steps this tool's archive lanes recorded on the way — the conversion that wrote the archive
(`mzpeak_convert_conversion`, with its `transformation` params, §8) and each filter
(`mzpeak_convert_filter`, §4.2), as often as it ran — then the export, each one `order` later; those
entries stay in the list as well. A timsTOF `.d` is
exported with its mobility params as the archive lanes write them: each diaPASEF spectrum's
`ion mobility lower limit` / `upper limit` pair in order (mzdata's reader emits it inverted) and,
with the precursor and scan 1/K0, on the vendor's ModelType-2 model that its mobility array uses,
evaluated as mzdata evaluates the array, so each window's limits bracket its own peaks exactly;
and the window's band as `userParam`s on the selected ion. `--no-tims-recalibration` is inert here.
An archive's export (`a.mzpeak -o a.mzML`) carries each peak's ion mobility where the archive holds
it (every timsTOF archive), and its 32-bit integer intensities, a chromatogram's too, as 64-bit
floats, exactly (below: an archive's export writes no integer-encoded intensity array). A `--no-ims-compact` archive holds one spectrum per diaPASEF window, and
exports like the `.d`. Neither holds the points of an MS2 frame that lie in a TIMS scan outside
every isolation window of the frame: mzdata's TDF reader, which both go through, hands a PASEF frame
over as one spectrum per window and nothing for the scans between and around them, as ProteoWizard
does (PXD059079 2485.d, diaPASEF: 10,614 of the 40,001 points of its first 25 MS2 frames). The archive
declares it (`bruker:out-of-window-points-dropped`, §8) and states the frames' own point count as
its `source_points`; the direct export, having no list to declare it in, warns with the count. An
ims-compact archive holds whole frames, every point of them: each is exported as one spectrum,
with every window's precursor and no mobility limits of its own, and the export says so. A reader
that assigns precursors by mobility window (OpenSWATH's diaPASEF mode) needs the `.d` exported with
`--to mzml`, or a `--no-ims-compact` archive.

The run. Every export states the run as its source does: its **id** (`<run id>`), its
**`startTimeStamp`** when the source states one, its **`defaultSourceFileRef`** (the source's
default: an Agilent `.d`'s `MSScan.bin`, not the first file listed) and its default instrument
configuration. A start time with a UTC offset is written in RFC 3339; a clock the source states
without a zone (an mzML's own zone-less `startTimeStamp`, a Waters or SciEX wall clock, an archive's
`acquisition_time` block, §8) is written as stated, without one — `xs:dateTime` has that form.
Through 0.17.0-rc.1 every export was `<run id="1">` of the first listed source file, undated.
Every id of the header is an `xs:ID`, an XML name, and an archive's ids and a vendor run's name
are plain strings (`MRM Neg C5`, `20181203_Capan2_1`, sample `1`): the run's id and each source
file's, sample's, software's and scan settings' id is written escaped as ProteoWizard
escapes it — each byte a name may not start with (anything but an ASCII letter or `_`) or hold
(anything but those, a digit, `.`, `-`) as `_x00hh_`: `MRM_x0020_Neg_x0020_C5`,
`_x0032_0181203_Capan2_1`, `_x0031_` — with the references that name it; an id that is a name
already is left as it is, and the mzML lanes decode the escapes again on import. Instrument
configurations are `IC1`, `IC2`, … in the order of their numbers, and a processing's id is written
as stated (no lane holds one that is not a name). Ids are not made unique across
the lists (ProteoWizard's own UNIFI files name a source file and a software `UNIFI`).

References. Every reference of an export resolves. The direct export of an mzML or imzML runs the
archive lane's check (§8, `mzml:dangling-reference-dropped`), and an archive's export runs it on the
index's lists: an entry mzdata skips for being self-closing is put back, and a reference that names
nothing is dropped — a scan is written under the run's default configuration (what an mzML scan
without the attribute means), a run default names the first entry of its list, an instrument
configuration's `softwareRef` is left out, a processing method, whose `softwareRef` mzML
requires, names `software_not_stated`, an entry with no version and no term, and in the direct
export, which writes the source's arrays, a `binaryDataArray`'s `dataProcessingRef` is left out
(the array then falls under the export's default processing; a dropped list default, which every
array without a reference of its own inherits, is counted once). One warning counts
each kind; an mzML has no `transformations` list to declare it in. An instrument configuration
without components or software has no `<componentList>` or `<softwareRef>` (the writer's empty ones
are not schema-valid). Against the mzML 1.1.0 schema the header of an export validates, and so does
its body (below: no empty list, a chromatogram's precursor and product as the schema has them);
what remains is a `<componentList>` that lacks a source, an analyzer or a detector the source does
not state.

An archive's export states what the archive holds: the source files, the samples, the software,
the instrument configurations (each scan under the one it was acquired on, one stored without a
configuration under the run's default), the processing history and the run are the archive's
(`file_description`, `sample_list`, `software_list`, `instrument_configuration_list`,
`data_processing_method_list`, `run`) — which is what the direct export of its source states
wherever both lanes read the source through the same reader. Through
0.17.0-rc.1 it stated none of them: one empty instrument configuration, the archive as the only
source file, this tool as the only software — and a run of two analyzers named an `IC2` it did not
declare. Different by design, in the header: the archive itself is listed as a source file
(`mzpeak_archive`, with its SHA-1) after the files the archive lists (an imzML's `.ibd` among them),
never as the default; the processing chain and list hold the archive's conversion (above); a
timsTOF `.d` is read by mzdata's TDF reader for `--to mzml` and natively for an archive, so its
direct export lists mzdata's entries beside the vendor directory's (software `TIMS_SDK` and
`ACQ_SW` after `timsTOF`, sample `SAMPLE_1` with a `TDF:AnalysisId`, a configuration of five
components naming `ACQ_SW`) and its archive's export the native lane's (software `timsTOF`, sample
`sample_1`, the analyzer alone); a
processing method that states no data transformation carries MS:1000530 `file format conversion`
(§8), a software without a term
MS:1000799, a detector without one MS:1000026 and a configuration without a model MS:1000031, which
the archive's writer adds to meet the spec's CvMapping; a term is named as the embedded vocabulary
names it (`Thermo RAW format` where an old source writes `Thermo RAW file`); the run's `sampleRef`
is not written by either export (mzdata's run model has none); and an mzML or imzML whose
`startTimeStamp` has no zone is exported with it by both routes, directly as the source spells it
and from its archive as the `acquisition_time` block holds it (§8: a fraction of a second with 3,
6 or 9 digits, so `…45.00035` comes back as `…45.000350`) — mzdata still logs an
`ERROR … Expected a dateTime value conforming to ISO 8601 standard` line when it reads such a
stamp (once per read of an imzML, so twice on a round trip); the line is the reader's, and the
clock is kept. Not a difference between the two exports, but one a
header diff shows: the parameters two or more instrument configurations share are written once,
as a `referenceableParamGroup`, in an order mzdata's writer does not keep from one run to the next
(an LTQ-FT's serial number, model and four `customization` blocks).

The chromatograms of an archive's export are the
archive's, as stored (times in minutes), each with its type and polarity term. A TIC (`TIC`) or
base-peak chromatogram (`BPC`) is added only for a kind the source or the archive lacks, on every
route the one a conversion synthesizes into an archive: a point per MS1 spectrum written, summed
from the signal written, in time order. (Through 0.17.0-rc.1 the direct export summed every
spectrum, from its stated total ion current where it had one, and named the base-peak trace `BIC`:
201 points where the archive of the same file holds 15.) A run without an MS1 spectrum — MS2 spectra
alone, or an imaging run whose pixels state `ms level` 0 — gets no summed pair: nothing is summed
over it, and a conversion synthesizes none into its archive either. Nor does a run none of whose
MS1 spectra states a start time (§7: every point would sit at time 0; an imaging run without
times, and an mzML without any in its direct export). When it has no other
chromatogram, the export has no `chromatogramList` and no chromatogram index (the schema lets a run
go without the list, not the list without a member); through 0.17.0-rc.1 such a run got a pair
summed over whatever spectra it held. A summed point takes its spectrum's start time; where only
some spectra state one, the others sit at 0, as in the archive. Every precursor, a spectrum's or a
chromatogram's, keeps its isolation window, dissociation method and collision energy; 1/K0 is
MS:1002815 `inverse reduced ion mobility`, once per element. Different by design: a spectrum's total ion current, base peak and
observed m/z range are the archive's, computed from the stored peaks (a timsTOF `.d` states each
window spectrum's frame totals); an ims-compact archive's whole frames state no per-window 1/K0,
`window group` or limits, and it holds HyStar's TIC/base-peak traces but not mzdata's per-window
pair (28 chromatograms where the `.d`'s export has 30), and its diaPASEF frames carry no precursor
`spectrumRef`, because the archive stores no parent frame for a scheduled window
(`precursor_index` null, the decision of 2026-09-03) where mzdata's TDF reader, which the `.d`'s
direct export goes through, names the preceding MS1 frame; a device trace that ProteoWizard writes as
an `intensity array` in pascal, psi, µL/min, °C, percent or absorbance units is written — by the
archive's export and the direct export alike — as a `pressure array`, `flow rate array`,
`temperature array` or a `non-standard data array` named after the chromatogram, in that unit
(mzdata's writer states detector counts for every `intensity array`, which is how the unit was lost
through 0.17.0-rc.1; a reader that takes a chromatogram's values from `intensity array` alone finds
none on such a trace); a chromatogram intensity in counts per second or percent of base peak, an
ion current in any other unit, and an intensity in a unit mzdata does not know are still written
as an `intensity array` in detector counts, with the stated unit's accession in the chromatogram's
`intensity array unit` userParam (§7; the run warns); a spectrum's `sourceFileRef` attribute is a
`userParam` of that name on both routes, its value the id of an entry of the export's own
`sourceFileList` (the direct export lists the source's files, an archive's export the files of the
archive's `file_description`; the id is escaped as that entry's is); a
Thermo precursor that named the spectrum itself, or one of no lower MS level, has no `spectrumRef`
(a run without MS1 named scan 1 on every scan); the direct export of
an mzML or imzML writes each spectrum's arrays as the source holds them — every array, in the
source's data types and order — where an archive's export writes what the archive stores (a plain
centroid spectrum as 64-bit m/z and 32-bit intensity in m/z order, whatever the source's types;
what storing changed of the intensities the archive declares, §8 `intensity-f32-rounding` and
`intensity-type-narrowing`; a peak facet whose intensities are 64-bit floats — a `--lossless`
archive's — is exported with them, as stored, and one whose intensities are 32- or 64-bit integers
— a `--lossless` archive of an integer source's, a timsTOF archive's — with every value, as 64-bit
floats: mzML allows an integer-encoded array, but OpenMS 3.5 refuses a file whose intensity array
is one (`Encoding intensity array as integer is not allowed`, an integer-intensity source mzML and
its direct export, which writes the source's arrays as held, included), and 64-bit floats hold
every 32-bit integer and every 64-bit one below 2^53 (the run warns and counts any beyond), and
an archive's integer chromatogram intensities — an mzML source's integer chromatogram keeps its
type in a default archive too — likewise, since OpenMS decodes an integer-encoded chromatogram
intensity array as empty and refuses the file; a spectrum without a point gets arrays of length 0
in the types a spectrum of its kind with points is written in; through 0.17.0-rc.2 an integer
peak column went through the reader's 32-bit float peak list, which changes every value above
2^24, and an integer profile or chromatogram column was written as the integers it holds); a scan window's
limits, an isolation window's target and offsets, a collision
energy and a selected ion's intensity are held by mzdata's model as 32-bit floats, so both routes
write them within about 1e-7 relative of the source's text (a lower limit of `102.966518275071`
comes back as `102.96651458740234`); the direct export of an mzML or imzML leaves out what that
model has no slot for — a `userParam` under `isolationWindow` or `scanWindow` (an isolation
window's `ms level`, a scan window's `centroided min/max`), the unit `UO:0000324` (square angstrom)
of a collision cross section, and a unit the source spells by a name mzdata does not know
(`number of counts` for MS:1000131; an archive's export keeps the unit its column declares) — all
four filed upstream with mzdata; a
`collision energy`, `peak intensity` or `ion injection time` of 0 that an mzML states in its own
text is kept by its direct export and absent from its archive's, which stores a 0 of these three as
null; and a scan of an mzML that states no start time has none in the direct export and
`scan start time` 0 in the archive's, which stores a time for every spectrum (an imaging archive
excepted, below) — so the export of an archive that is not an imaging one still sums a TIC and
base-peak pair, at time 0, over a run none of whose spectra stated a time. Not in any archive yet,
so not in its export: an SRM trace's product (Q3) window and a spectrum's `sum of spectra`
combination.

An export states nothing that neither its source nor its archive states. A spectrum whose polarity
is unknown gets no polarity term (the run says how many, in one warning); a scan gets an
`ion injection time`, a selected ion a `peak intensity` and an activation a `collision energy` only
where one is known — a vendor reader's or an archive's 0 means "not stated", and only an mzML that
writes the 0 itself keeps it. A scan gets a `scan start time` where one is stated: a vendor reader's
and an archive's time is the scan's, 0 included; a scan of an mzML or imzML that states no time gets
none in the direct export; and an imaging archive whose marker says that its source stated no time
(`imaging.provenance.time`, §8) is exported without any. The direct export of an mzML or imzML reads
which spectra and chromatograms state which of these four, in a `cvParam` of their own or of a
`referenceableParamGroup` they refer to, in one extra pass over the source's text (the same pass
reads each spectrum's `sourceFileRef` and looks for an imaging term; through 0.17.0-rc.2 these were
three passes). Performance: the direct export of an mzML takes 35–50 % longer than 0.16.0 did on
the same file (a 38 MB Shimadzu export 0.80 s where 0.16.0 took 0.60 s, a 182 MB LTQ-XL one 5.3 s
for 3.6 s; min of 3 runs each on a quiet machine, 2026-10-02), almost all of it per spectrum: the lane writes the
source's arrays as held (decoded and encoded again, a 64-bit intensity array deflated as such where
0.16.0 wrote a 32-bit one from the peak list), runs the byte sinks that put the writer's output
right, and digests the file for `<fileChecksum>` (a profile of the LTQ-XL export: zlib's deflate
55 % of the samples, the sinks 7 %, the SHA-1 4 %); a run's fixed cost (`MZPC_MAX_SPECTRA=1`:
opening the file, the text pass, the header and the close) is 0.13–0.31 s on those files, where
0.16.0's was 0.07–0.12 s. Through
0.17.0-rc.1 every export stated `positive scan`, the three zeros and a start time regardless: a
negative-mode imaging run was exported as positive, every pixel of an imaging run without times as
acquired at 0 min, and a reader could not tell a 0 from a measurement. Still written whatever the
source says: `scan start time` 0 by the export of an archive that is not an imaging one, for a scan
whose source stated no time (the archive stores 0, and nothing in it says which scans stated one),
and `base peak m/z` and `base peak intensity` 0 on a spectrum without a peak. A spectrum without a
point is written with an m/z and an intensity array of length 0 and no observed m/z range; every
array of length 0, a chromatogram's included, is declared `no compression` and has an empty
`<binary>` (`encodedLength="0"`), not the zlib stream of nothing that OpenMS 3.5 fails on in an
integer array. No spectrum has an empty `<precursorList>`, no precursor an empty
`<selectedIonList>`, and a chromatogram holds its `<precursor>` and `<product>` directly, as the
mzML 1.1.0 schema has them. Each `<offset>` of the index is the byte position of its `<spectrum>` or
`<chromatogram>` start tag, `<indexListOffset>` that of `<indexList>`, and `<fileChecksum>` the
SHA-1 of the file up to and including the `<fileChecksum>` start tag (of the uncompressed document
for a `.mzML.gz`); through 0.17.0-rc.1 the offsets pointed at the line break before each element and
the checksum matched no export.

The header. An export carries the source's **scan settings** as a `scanSettingsList` (an imaging
run's grid and pixel size, an inclusion list's targets), from an mzML/imzML as read and from an
archive's `scan_settings_list`; an entry's source file references are kept where the export lists
the file, which both exports do. An archive's export
states the archive's `file_description.contents` as its `fileContent`, and the direct export of an
imzML adds the provenance mzdata consumes — storage mode `IMS:1000030/31`, UUID `IMS:1000080`, the
`.ibd` checksum `IMS:1000090/91/92` — as the archive lane does, so both routes state the same. An
export that writes imaging terms (pixel positions `IMS:1000050/51` on the scans, the grid, that
provenance: any imzML, an mzML that mentions an `IMS:` term, an imaging archive) declares the `IMS`
vocabulary in its `cvList`, pinned to the commit the archive's `cv_list` names (§8). Through 0.16.0
no export had a `scanSettingsList`, an archive's export had an empty `fileContent`, and the `cvList`
declared MS and UO whatever the params named. The direct export of an imzML applies the archive
lane's rules to its scan settings (§8: the pixel-size rule, a unit accession its name contradicts,
the obsolete "one way"), so both routes state the same grid; an mzML has no `transformations` list,
so the run's warnings are the only declaration, the last of them naming each rule applied. A Waters
imaging `.raw` exported directly states its fitted grid the same way, with the vocabulary declared.

```sh
mzpeak-convert run.raw -o run.mzML            # Thermo → mzML
mzpeak-convert run.d --to mzml -o run.xml     # format forced, any name
mzpeak-convert run.mzpeak -o run.mzML.gz      # archive → gzipped mzML
```

### 4.2 Filtering an existing archive (`.mzpeak` input with `--rt`, `--ms-level`, `--drop-aux`)

When the **input** is a `.mzpeak`, the converter does not re-encode: it re-packs the archive,
keeping spectra whose retention time is within `--rt MIN-MAX` (same unit as the stored
`spectrum.time`, minutes for every lane this tool writes) and/or whose MS level is in `--ms-level`
(`--ms-level 1 --ms-level 2` or `--ms-level 1,2`), and dropping archive members that match
`--drop-aux <glob>` (`--no-vendor` on this lane is shorthand for `--drop-aux 'vendor*'`). `--rt`
also truncates the chromatograms, in the same minutes, whether their time axis is stored as 64- or
32-bit floats (a PDA or DAD run's mzML; through 0.16 such a trace was copied whole). This converter
stores every chromatogram time in minutes (§8, `chromatogram-time-to-minutes`), and the window is
converted into whatever unit a chromatogram time axis declares, so an mzML-lane archive converted
by 0.11.5 or earlier, whose column declares ProteoWizard's seconds, is cut where the window says too. Such an archive holds its
synthesized TIC/BPC in minutes under that seconds label, so `--rt` cuts those two traces at 60 times
the times it names: rebuild it first. No published corpus archive has a seconds column. Parquet
facets are not re-encoded from the signal: the per-spectrum and chromatogram facets a filter can
change are read and written back (zstd level 5, every column in the encodings the source used, the
source's Parquet format version, sort order and bloom filters, the page limits a conversion gives
the facet, row groups bounded as a conversion's, §10 `MZPC_ROW_GROUP_MB`) and run-global facets
are copied verbatim, so encoder options are inert here — warned about, not refused (see the table above).
Keeping every spectrum of an archive this version wrote, a rewritten spectrum signal facet comes out
between 1.9 % smaller and 0.3 % larger than its source, and a chunked timsTOF grid facet 4.2 %
larger: the lane writes at zstd level 5, the converter at 3 (22 on that facet). A chunked archive
written by 0.16.0 or earlier keeps its
dictionary-encoded chunk bounds through the rewrite, and once the byte cap splits its peak facet,
each row group pays for that dictionary again (MFA381's peak facet +2.3 %); rebuilt from the raw
file, the bounds are byte-stream-split (§9). The same
lane injects `--sdrf` into an existing archive — the documented way to add it to an archive from a
lane that cannot embed it (§4.3; `--image` too, into an imaging archive) — and writes to `<out>.mzpeak.tmp` first, renaming
into place on success. The three filters on a **raw or exchange** input are a hard error with the
two-step remedy printed (convert first, then filter the archive); they used to be silently ignored.

The rewrite records itself in the index: a `filter` block (source name, options, what was dropped,
injected and renumbered, `tool_version`) and an entry of `data_processing_method_list`
(`mzpeak_convert_filter`; a second rewrite's is `mzpeak_convert_filter_2`) whose method — MS:1001486
`data filtering`, the options as a `filter options` param — names the `software_list` entry of the
version that ran: the source's `mzpeak-convert` when this version converted the source, else a new
entry (`mzpeak-convert_2`) beside it. Through 0.17.0-rc.1 the entry's id was fixed and its method
named `mzpeak-convert` whatever that entry's version, so filtering a 0.16.0 archive credited the
step to 0.16.0, and a second filter repeated the id. **The index is where the step is recorded.**
The Parquet footers of the metadata facets (`spectra_metadata*.parquet`,
`chromatograms_data.parquet`) repeat the software and processing lists as the conversion wrote
them, and a rewrite leaves those copies as they are, in a facet it rewrites and in one it copies
byte for byte alike: read an archive's history from `mzpeak_index.json`.

```sh
mzpeak-convert run.mzpeak -o ms2_5to6.mzpeak --ms-level 2 --rt 5-6
mzpeak-convert run.mzpeak -o slim.mzpeak --drop-aux 'vendor/*.tdf_bin'
mzpeak-convert run.mzpeak -o annotated.mzpeak --sdrf run.sdrf.tsv
```

Wavelength (UV/PDA) spectra have no MS level, so `--ms-level` leaves them out, their facets with
them, and `--rt` keeps those whose time lies in the window. The archive → mzML export (§4.1) takes the
same two rules, so filtering into an archive and exporting that writes the spectra a filtered export
does. The export places them among the mass spectra by retention time.

The spectra a filtered archive keeps are numbered 0..n-1 in the order of their old indices, as the
spec's `index` requires, and every column that holds a spectrum index follows: each metadata facet's
`source_index`, the data facets' `spectrum_index`, the Thermo trailer facets' `ordinal`, a
precursor's or selected ion's `precursor_index` (its parent spectrum, for a chromatogram's precursor
too), and the `encoding_prescan` block's `int32_fallback.spectrum_index`. A scans facet's own
`scan_index` and a products facet's `product_index` restart at 0 as well, and the wavelength spectra
are numbered the same way. The ids are the source's, so a spectrum is found in the source by its
`id`. A precursor whose parent spectrum was filtered out loses its `precursor_index` and
`precursor_id`, and a reference by id to a spectrum that is gone (`precursor_id`, a scan's
`spectrum_reference`) is nulled, in the spectrum and the chromatogram facets alike; the mzML export
writes no `spectrumRef` to a spectrum it leaves out either. The index's `filter` block lists what was
renumbered under `renumbered` (`spectrum`, `wavelength_spectrum`; empty when a window starts at the
first spectrum and nothing moved). An archive filtered by 0.16 or earlier keeps sparse indices;
filtering it again numbers what it keeps 0..n-1, while `--drop-aux`, `--sdrf` and `--image` alone
copy the spectra as they are.

### 4.3 Embedding sample metadata and images (`--sdrf`, `--image`)

`--sdrf <file.tsv>` embeds an SDRF verbatim as `sample_metadata/sdrf.tsv` and adds
`metadata.study` + `metadata.sample_metadata` back-references to the index; `--image <file>`
embeds an optical image verbatim as `images/image_NNNN.<ext>` with a `metadata.imaging` overlay
affine (an `<input-stem>-opticalimage.{tif,tiff,png,jpg}` sibling of the converted run is
auto-discovered; none beside an existing archive). Both are
strict: a missing or unreadable path fails the conversion. An image maps onto the pixel grid of
an **imaging run** (§8: the lane wrote pixel positions and its `metadata.imaging` marker) and never
makes a run imaging: on any other run an explicit `--image` fails the conversion and an
auto-discovered sibling is skipped with a warning; `--sdrf` needs nothing.

The affine is the imaging profile's, `[a, b, c, d, e, f]` from 0-based image pixel centres to
1-based MS pixel centres (`x_ms = a·col + b·row + c`, `y_ms = d·col + e·row + f`), and its
`registration_quality` is `assumed_full_extent`: nothing registered the image, the converter lays
its extent on the extent of the Nx × Ny pixel grid. For a W × H image that is `a = Nx/W`,
`c = 0.5 + 0.5·Nx/W`, `e = Ny/H`, `f = 0.5 + 0.5·Ny/H`, `b = d = 0`: the image's left edge
(col −0.5) falls on the left edge of MS pixel 1 (x_ms 0.5), its right edge (col W − 0.5) on the
right edge of MS pixel Nx (x_ms Nx + 0.5). (Through 0.16.0 the matrix mapped the corner pixel
centres onto each other, `a = (Nx − 1)/(W − 1)`, `c = 1`, which is off by up to half an MS pixel at
the edges; archives written before keep that matrix.) One kind of archive gets **no affine**: a
Bruker MALDI run whose `bruker_maldi` block names a FlexImaging sequence (`.mis`). Its grid is the
bounding box of the acquired regions, while the sequence's image is a photo of the whole target, so
the full-extent matrix would misplace it (by up to 11 MS pixels on MassIVE MSV000088438). The image
is embedded and listed in `images[]` without `affine`, with a warning; the registration from the
sequence's teach points is planned. Which lanes embed them:

| Lane | `--sdrf` / `--image` |
|---|---|
| mzML / imzML on the standard lane, **including the `--tof-grid` sub-path** (the same command used to keep or lose the SDRF depending on whether the grid fit passed — fixed in 0.9.13) | embedded (`--sdrf`; `--image` on an imaging run only, which skips the `--tof-grid` sub-path) |
| Thermo `.raw`, Bruker TDF with `--no-ims-compact` (mzdata path) | embedded (`--sdrf`; `--image` on an imaging run only) |
| `--via-msconvert` | embedded (since 0.9.13; it used to hard-code "none") |
| `.mzpeak` → `.mzpeak` (§4.2) | injected into the existing archive (`--sdrf`; `--image` into an imaging archive only, placed on the grid of the source's `metadata.imaging` marker, which is carried and gains the image in `images[]` after any it lists; without an affine on a Bruker MALDI archive acquired from a FlexImaging sequence, see above) |
| default timsTOF ims-compact, both `--bruker-sdk` lanes, `--agilent-grid`, native vendor readers (TSF / BAF / Agilent / `.wiff` / Waters / `.lcd`) | **refused** (exit 1) — convert first, then inject: `mzpeak-convert out.mzpeak -o with.mzpeak --image … --sdrf …` (`--image` only when the archive is imaging: a Bruker MALDI or Waters imaging run, see the row above) |
| `--to mzml`, `.mzpeak` → mzML | **refused** — mzML has no place for them |

## 5. Configuration file

`--config <file.yaml>` loads a configuration file that can set **any** option of §4 except the
two that make no sense in a file — the positional `<INPUT>` and `--config` itself. Every key is
optional; the key is the option's long name with `-` → `_`. Precedence is:

> **explicit command-line flag → config-file value → built-in default**

(Boolean switches such as `no_numpress` are enable-only: a config value of `true`
or the corresponding flag turns them on; a config `quiet: true` under a command-line `-v`
keeps quiet, as `-q -v` always did.) The example below is regenerated from the `FileConfig`
struct in `src/main.rs` and lists every accepted key; the six marked *(0.9.13)* were promised by
`--help` but rejected as unknown fields until then.

```yaml
# mzpeak-convert.yaml — every overridable option, all optional
output: out.mzpeak
to: mzpeak                 # or: mzml (default: inferred from the output extension, .mzpeak or .mzML)
layout: chunked            # or: point
no_numpress: false
keep_zero_runs: false      # true: store every profile point (no zero-run mask)
keep_contact: false        # true: carry an mzML/imzML header's <contact> (§4.1)
lossless: false            # true: a bit-exact archive or a failed conversion (mzML/imzML only)
no_mz_lattice: false
chunk_size: 50
zstd_level: 9
force: true
no_ims_compact: false      # TDF: keep the lossless ims-compact default
ims_chunked: true          # the default since 0.12.1
no_ims_chunked: false      # opt back out to the flat archive layout
bruker_sdk: false
no_tims_recalibration: false
no_vendor: false
no_chromatograms: false
aux:                       # vendor side-file rules (see §8)
  - "*.tdf_bin=drop"
  - "*.method=embed"
image:                     # optical images to embed (§4.3; imzML input)
  - sample-opticalimage.tif
sdrf: sample.sdrf.tsv      # §4.3
tof_grid: off              # off | auto | on (mzML lanes default off; native SCIEX defaults auto)
agilent_grid: false
via_msconvert: false
msconvert_path: /opt/pwiz/msconvert
representation: both       # both | profile | centroid            (0.9.13)
rt: "10-20"                # .mzpeak input only, §4.2               (0.9.13)
ms_level: [1, 2]           # .mzpeak input only                     (0.9.13)
drop_aux:                  # .mzpeak input only                     (0.9.13)
  - "vendor/*.tdf_bin"
verbose: 0                 # 1 = -v, 2 = -vv                        (0.9.13)
quiet: false               #                                        (0.9.13)
sample: 2                  # SciEX .wiff with several samples, §4
```

```sh
mzpeak-convert run.d -c mzpeak-convert.yaml          # uses the file's settings
mzpeak-convert run.d -c mzpeak-convert.yaml --zstd-level 3   # CLI overrides zstd_level
```

Unknown keys are rejected with a clear error. Two consequences of how the file is merged: a config
value is a standing default, *not* something you "supplied" for the per-lane refusal table in §4 —
a profile carrying `zstd_level:` or `sdrf:` makes neither the `.mzpeak` filter lane nor the
ims-compact lane refuse (the value simply has no effect there) — whereas the three filter options
`rt:` / `ms_level:` / `drop_aux:` are checked on the resolved settings and still fail on a raw
input (convert first, then filter), exactly as the flags do; and `verbose` / `quiet` from a file
take effect because settings are resolved before logging is initialised.

## 6. Supported formats & operating systems

| Format | Linux | macOS | Windows | Notes |
|---|:---:|:---:|:---:|---|
| mzML, `.mzML.gz` | ✅ | ✅ | ✅ | full metadata + chromatograms; gzip detected by magic, decompressed to a temp copy |
| imzML | ✅ | ✅ | ✅ | imaging coordinate columns; IMS CV promoted; pixel size checked and file provenance kept (§8) |
| Bruker `.d` **TDF** (timsTOF) | ✅ | ✅ | ✅ | ion mobility; **ims-compact by default**; MALDI imaging positions (§8) |
| Bruker `.d` **TSF** (line spectra) | ✅ | ✅ | ✅ | MALDI/TOF; otofControl m/z correction; MALDI imaging positions (§8) |
| Thermo `.raw` | ✅ | ✅ | ✅ | needs a **.NET 8+ runtime** |
| Bruker `.d` **BAF** | ✅ | ❌ | ✅ | auto-built; needs `libbaf2sql_c` at runtime |
| Agilent `.d` (native, scan data) | ❌ | ❌ | ✅ | net48 `AgilentGlueHost.exe` (§11) → MHDAC, since 0.11.0; **MRM/SIM-only runs are refused** — they are transition chromatograms, use `--via-msconvert` for them |
| SciEX `.wiff` (native) | ❌ | ❌ | ✅ | auto-built; Clearcore2 DLLs at runtime; **MRM/SIM dwell runs are refused** (they are transition chromatograms — `--via-msconvert` writes them as SRM chromatograms); multi-sample files need `--sample N` |
| Shimadzu `.lcd` (native) | ❌ | ❌ | ✅ | LabSolutions.IO DLLs at runtime (§11); profile as a sqrt grid, centroids as an exact lattice (§8, §9); the vendor's per-event TIC/BPC chromatograms, the serial number and model from the file's system configuration |
| Waters `.raw` (native) | ❌ | ❌ | ✅ | `MassLynxRaw.dll` through its C ABI, no .NET glue (§11); HDMSe/HDDDA functions as frames with a per-point drift time; MALDI/DESI imaging positions fitted to a grid (§8) |
| Agilent / SciEX / … via msconvert | ✅ | ✅ | ✅ | `--via-msconvert`; needs ProteoWizard (Windows, or Wine elsewhere) |

The native vendor readers are **compiled in automatically on the platforms where
the vendor libraries exist** — no build flag (see §11). They load the proprietary
DLLs at runtime and report a clear error if those are absent. Inputs with no native
reader on the current platform exit with code **3** and actionable guidance
(usually: use `--via-msconvert`).

## 7. The mzPeak output

A `.mzpeak` file is a **STORED** (uncompressed-container) ZIP. Compression lives
*inside* the Parquet facets, not in the ZIP, so readers can range-read columns.
Contents:

- `mzpeak_index.json` — manifest: facets, schema versions, run metadata,
  `ims_calibration` (for ims-compact), `transformations` (what was done to the signal, §8),
  `fidelity` (by how much: points stored against points read, numeric types, m/z error, §8),
  `partial` (only when `MZPC_MAX_SPECTRA` truncated the run, §10), declared file entries.
  Since 0.9.13 `source_files[].location` never carries the converting machine's path (it is
  reduced to the bare `file://` authority; non-`file` URL schemes are kept), `run.id` is never a
  path, and `default_instrument_id` always resolves: a run without an instrument record gets one
  empty configuration `0` to point at (the spec requires the integer; 0.10.0 briefly wrote `null`,
  which the validator's schema check refuses — fixed in 0.10.2). `cv_list` declares for `MS` the
  `data-version` of the PSI-MS vocabulary mzdata embeds, which every CURIE resolves against, read from
  that copy (4.1.258 with mzdata 0.67.1; archives through 0.16.0 declared 4.1.249); `UO` and `IMS`,
  which mzdata holds no copy of, stay pinned to one release or commit each.
- `spectra_metadata.parquet` — per-spectrum descriptors (id, index, MS level,
  polarity, scan time, precursor info, …).
- `spectra_data.parquet` / `spectra_peaks.parquet` — signal arrays (chunked/point): profile
  spectra in `spectra_data`, centroid spectra in `spectra_peaks`, by the representation the source
  declares — since 0.10.1 for grid-encoded TOF axes too (§9).
- `chromatograms_metadata.parquet` / `chromatograms_data.parquet` — TIC/BPC/SRM and other
  chromatograms: one metadata row each, and their points, times in minutes. Every chromatogram the
  source carries is stored (a LabSolutions export's TIC/BPC pair per acquisition event, a Bruker `.d`'s
  HyStar traces); a TIC and a base-peak chromatogram are summed over the MS1 spectra only for the kind
  the source lacks, and lead the facet (`--no-chromatograms` synthesizes none). An mzML or imzML
  whose spectra state no scan start time (`MS:1000016`) gets no synthesized pair: mzdata reads an
  unstated time as 0, and every point of the trace would sit at time 0 (two corpus imaging runs held
  1,196 and 34,840 such points). A facet with nothing else to hold — that case, or a run with no MS1
  spectrum and no source chromatogram — carries one **placeholder row**: `id` empty,
  `chromatogram_type` null, `number_of_data_points` 0, no row in `chromatograms_data`, and (since
  0.17.0) the parameter `placeholder chromatogram`, whose value says why the row is there. The
  reference reader needs the facet to open the archive; a reader should skip a row that carries the
  parameter, or has an empty id and no points, as this converter's mzML export does, and
  `mzpeak-convert <archive>` reports it as `chromatograms: 0 (one placeholder row: …)`. The two
  footers count it as every facet counts: `chromatograms_metadata` declares `chromatogram_count` 1,
  its rows (the validator's `chromatogram_count_agreement` holds a metadata facet to its row count),
  and `chromatograms_data` declares 0, one past the largest index with a row in that file (decision
  D1, below) — the same pair a centroid-only run leaves on `spectra_metadata` (3) and
  `spectra_data` (0). A value that is not an
  intensity, such as a device trace's pressure, flow rate, temperature or solvent percentage,
  has no column of its own: it is stored in that chromatogram's `auxiliary_arrays` in
  `chromatograms_metadata`, under its name and in its unit, and the trace's `intensity` values in
  `chromatograms_data` are null. A reader that plots `intensity` alone shows such a trace as empty
  or as zeros (mzPeakViewer does, so far); its values are in the auxiliary array. This holds for a
  Bruker `.d`'s HyStar traces and, since 0.17.0, for the same traces on the mzML lane: ProteoWizard
  writes each as an `intensity array` in the trace's unit (pascal, psi, µL/min, °C, percent,
  absorbance unit), and an intensity array in a unit that is not an intensity's is stored as
  the pressure, flow rate or temperature array of a chromatogram of that type, otherwise as a
  non-standard array named after the chromatogram — values and data type untouched. Through
  0.17.0-rc.1 it went into the shared `intensity` column, which is declared in detector counts, and
  the unit was gone from the archive and from both mzML exports (49 chromatograms of 12 corpus mzML
  files). An intensity stays in the `intensity` column: an array in detector counts or stating no
  unit, as it is; one in another unit PSI-MS allows on an intensity array (`MS:1000814` counts per
  second, `MS:1000132` percent of base peak, `MS:1000905` the same times 100), and the array of an
  ion-current chromatogram (TIC, base peak, SIC, SIM, SRM) in whatever unit it states, with the
  stated unit's accession as that chromatogram's parameter **`intensity array unit`** (no accession;
  value e.g. `MS:1000814`), which both mzML exports write as a userParam, and
  `mzml:chromatogram-intensity-unit-as-parameter` declared: the column goes on declaring detector
  counts, which the synthesized TIC and base-peak chromatogram beside it are in. A unit mzdata has
  no name for (it knows 29 units) reads as no unit; the lane reads the accession back
  from the source's `<chromatogram>` and stores the array by the same rule, with the same
  parameter stating the unit (the stored array cannot name a unit mzdata does not know). The
  facet keeps its `intensity` column when the first chromatograms of the source are all device
  traces.
- `vendor/…` — embedded original side-files (optional, see §8).

**Footer count keys.** The spectrum, chromatogram and wavelength facets carry `<entity>_count`
and `<entity>_data_point_count` in their Parquet key–value footers (the `vendor/…` facets carry
neither). The specification does not define these keys; this converter writes them with one
definition (issue #1): on a **data facet** (`spectra_data`, `spectra_peaks`,
`chromatograms_data`, `wavelength_spectra_data`) the count is one past the largest
`<entity>_index` with at least one row *in that file*, and `0` when the file has no rows; the
point count is the points *in that file*. So a centroid-only run's empty `spectra_data` says
`0 / 0`, and `0..spectrum_count` on either data facet reaches every spectrum stored there. It is
an **index bound, not the number of spectra in the file**: indices in a data facet are sparse (a
mixed run's `spectra_data` holds only its profile spectra), so an index below the bound may have
no row. The
**run total** lives on the primary metadata facets (`spectra_metadata`, `chromatograms_metadata`,
`wavelength_spectra_metadata`), which also repeat the data facets' point totals; the secondaries
(`_scans`, `_precursors`, `_selected_ions`) carry no entity count, and a rewrite does not keep the
one an older archive has (through 0.11.5 a conversion stamped the run total there, even on a facet
with no rows, and a rewrite the entities left in that facet). To
plan reads, use the per-spectrum `number_of_data_points` / `number_of_peaks` columns of
`spectra_metadata` (the spec's mechanism) or the actual indices in the facet; a facet with
`num_rows == 0` has nothing to read whatever its footer says. Archives from 0.11.2 to 0.11.5
declare on a data facet the number of entities with rows instead, which is not a bound: iterating
`0..spectrum_count` stops early on a mixed run, around empty spectra, and on any rewritten
archive. Archives from 0.11.1 and earlier
declare the run total on `spectra_data` (and the sum of both data facets' points), and on
`spectra_peaks` the number of centroid spectra handed to it, zero-peak spectra included. An archive
rewritten by 0.11.x (`--rt`, `--ms-level`, `--drop-aux`) also embeds the pre-filter counts in each
rewritten facet's `ARROW:schema`, which Arrow C++ and pyarrow report as the schema metadata; its
key-value footer is the one to read.

**The format itself** — rationale, the draft specification, and the controlled
vocabulary — is documented at:

- 🌐 **[mzpeak.org](https://mzpeak.org)** — overview and specification.
- 📑 **[HUPO-PSI/mzPeak-specification](https://github.com/HUPO-PSI/mzPeak-specification)** — the spec repository.
- 🔬 **[mzpeak.org/view](https://mzpeak.org/view)** — open and analyze any `.mzpeak`
  produced by this tool directly in your browser (streamed over HTTP, no upload,
  no backend).

## 8. Vendor-specific metadata handling

Vendor acquisitions carry rich, format-specific metadata. mzPeakConverter
preserves it along two routes:

**Run metadata the vendor states (native lanes, since 0.11.3).** The mzML lane
inherits ProteoWizard's finished model; the native lanes build one from what each vendor file
STATES, merged field by field (`src/run_metadata.rs`) — nothing is guessed, so a lane records a
serial, a sample or a source (ion source, detector) only where the file says so:

| Lane | Read from | Instrument | Software | Sample | Time | Source members (each with MS:1000569 SHA-1) |
|---|---|---|---|---|---|---|
| Bruker TDF / TSF | `GlobalMetadata` | MS:1003123 timsTOF family + `InstrumentName`, serial, TOF analyzer | `AcquisitionSoftware` + version | `SampleName` | `AcquisitionDateTime` (zoned) | `analysis.tdf`/`.tsf` + `_bin` |
| Bruker BAF | the baf2sql cache's `Properties` table (Windows/Linux) | the series term ProteoWizard arrives at for the raw `InstrumentFamily` code through `translateInstrumentFamily` and `translateAsInstrumentSeries` (1–2 micrOTOF; 6–8, maXis/impact/compact, maXis series; 512 apex; 513 solarix), MS:1000122 Bruker Daltonics instrument model for any other code or an unreadable table; serial | `AcquisitionSoftware` + version | — (not in `Properties`) | `AcquisitionDateTime` | `analysis.baf` + `_idx` + `_xtr` (any host) |
| Agilent `.d` | `AcqData/Devices.xml`, `Contents.xml`, `sample_info.xml` (any host) | MS:1000490 + name, model number, serial, analyzers implied by the device type | MassHunter + `AcqSoftwareVersion` | `Sample Name` | `AcquiredTime` (with its offset) | the AcqData files (no exported text, no dot files) |
| Waters `.raw` | `_HEADER.TXT`, `_extern.inf` (any host); per scan: MassLynxRaw (Windows) | MS:1000126 + model, serial unless `#NotSet` | MassLynx `Created by` version | `Acquired Name` + descriptors | `Acquired Date/Time` (no zone) | `_FUNCnnn.DAT` (Waters nativeID) then the side files |
| SciEX `.wiff` | Clearcore2 sample/instrument details (Windows) | MS:1000121 + `InstrumentName`, serial | Analyst + `SoftwareVersion` | sample name | `AcquisitionDateTime` (no zone) | `.wiff` + `.wiff.scan`, digested before the library opens them |
| Shimadzu `.lcd` | the `.lcd`'s own `File Property` stream (any host) + LabSolutions.IO (Windows) | the model's PSI-MS term (MS:1002998 for an LCMS-9030; MS:1000124 carrying the stated name for a model the vocabulary lacks) + `SystemName`, ESI + quadrupole + TOF from the device id | LabSolutions + `DataFileProperty.szVersion` | `smpl_name` (+ id, vial, operator, injection volume) | `SampleInfo.DateTime`: a UTC FILETIME presented in the writer's stated GMT offset (`+01'00'`) — fully zoned | `.lcd` |
| Thermo `.raw` | mzdata's Thermo reader | complete already | Xcalibur | yes | zoned | `.raw` |

**Acquisition time: stated offset or nothing.** `run.start_time` is an RFC 3339 instant, and
RFC 3339 cannot say "zone unknown". A vendor time that STATES its offset (Agilent
`2022-11-01T13:11:27-04:00`, Bruker, Shimadzu's UTC FILETIME + `+01'00'`) is written verbatim. A wall
clock WITHOUT one (Waters, SciEX) leaves `run.start_time` null and is preserved verbatim in the index:

```json
"acquisition_time": {"wall_clock": "2018-12-03T22:39:33", "zone": "unstated",
                     "source": "Waters _HEADER.TXT", "note": "…"}
```

**Reading the acquisition time.** Use `run.start_time` when it is set. When it is null, read
`metadata.acquisition_time.wall_clock`: the vendor's local clock exactly as the file states it,
zone unstated. Show or compare it as a local wall clock and never attach an offset — neither the
reader's own nor UTC. An archive with neither states no acquisition time. Bruker and Agilent
directories follow the same rule: their clocks carry offsets in every file seen so far, and one
that does not becomes the block too (archives written by 0.11.5 and earlier dropped it on the
lanes that read those directories). So does an **mzML or imzML** whose run `startTimeStamp` has no
offset (`2009-08-11T15:59:44`: five corpus imzML units): the block's `source` is `mzML run
startTimeStamp` / `imzML run startTimeStamp`, and `wall_clock` is the stamp as an ISO 8601 local
time (a fraction of a second is written with 3, 6 or 9 digits). Through 0.17.0-rc.1 such a stamp
was dropped — mzdata reads the attribute as RFC 3339 and discards anything else, with an ERROR
line that still appears in the log — and the archive stated no acquisition time at all. An offset
without its colon (`+0200`, ISO 8601's basic form) is read as the offset it states and gives
`run.start_time`. A stamp that is not read as a date-time (a date alone, free text) is kept
verbatim as `stated`, with no `wall_clock` and no `zone`. An mzML export writes either as the run's
`startTimeStamp` (§4.1): the instant in RFC 3339, the wall clock as stated, without an offset — the
direct export of an mzML or imzML reads the source's stamp the same way and writes a clock without
an offset as the source spells it; a stamp that is not a date-time is not written.

ProteoWizard resolves the same ambiguity by asserting: it labels an unzoned Waters clock `Z`, and
its `adjustUnknownTimeZonesToHostTimeZone` default shifts other readers' values by the converting
host's offset AT CONVERSION TIME (a Shimadzu run that the file states as 10:47:18Z comes out
08:47:18Z when converted in September on a CEST box; SciEX wall clocks come out minus 2 h whatever
their month; blank1's stated 13:11:27-04:00 comes out 18:11:27Z). The archive's value is the
vendor's; the mzML lane's is whatever pwiz computed. `file_description.contents` likewise states what the lane
wrote (MS1/MSn spectrum, centroid/profile, TIC chromatogram), not a generic `mass spectrum`.

**Waters ion mobility (HDMSe / HDDDA) is stored as frames.** A function with a `_funcNNN.cdt` and a
drift-scan count is read bin by bin and written as one spectrum per MassLynx scan whose points are
sorted by (m/z, drift time) and carry a per-point `raw ion mobility array` (MS:1003007, ms) — the
shape of ProteoWizard's `--combineIonMobilitySpectra` output and of the Bruker ims-compact lane —
with the frame's drift-time bounds (MS:1003439/1003440), `sort-by-mz` in `transformations` when a
frame's bins came back out of m/z order (counted frame by frame), and a
`waters_drift` index block holding the run's bin → ms table, the vendor's `mob_cal.csv` CCS
calibration verbatim, the lock-mass function and the functions not written as spectra. Every Waters
archive, with drift bins or without, also carries a `waters_functions` block: each function's type,
MS level, drift bins and SONAR flag, the functions skipped with their reason, the collapsed
functions with whether they were written, and the lock-mass function. Frames keep
every point MassLynx returns: the writer's zero-run mask is off for them (`zero-run-mask` is absent
from `transformations`), because a run of zeros in an interleaved frame is several bins' trace
boundaries meeting. ProteoWizard's default instead writes one spectrum per drift bin (Capan2:
397,800 spectra for 1,989 scans); the two are the same data (verified bin for bin), 531 MB as
frames against 965 MB as bins. Keeping every point has a size cost, accepted for per-bin fidelity:
zero flanks are 44–46 % of the stored points on the corpus HDMSe runs, and a frame archive is about
3.2× the drift-summed one earlier releases wrote (Capan2 166 → 531 MB, PXD077098 2.1 → 9.0 GB).
MassLynx returns only flank zeros plus two sentinels per bin, so a mask that keeps peak boundaries
would save nothing, and dropping every zero could not be undone. Spectra are in acquisition-time
order across functions.

**Imaging.** A run is an imaging run when the converter detects one — imzML input always; a Bruker
`.d` whose `analysis.tsf`/`.tdf` has `MaldiFrameInfo` positions; any other input (an mzML, the mzML
this converter exports from an imaging archive) whose spectra state `IMS:1000050/51`; a Waters `.raw`
whose scans state laser aim positions (below). A detected run follows the imaging profile
(HUPO-PSI/mzPeak-specification#24): positions in the `position_x` / `position_y` (and `position_z`
when stated) columns of `spectra_metadata_scans`, each mapped to its `IMS` term; the grid in
`scan_settings_list` (`IMS:1000042/43` pixel counts always — counted from the largest positions and
declared `imaging:pixel-count-from-positions` when the input states none, raised to them and declared
`imaging:pixel-count-raised-to-positions` when a stated count does not bound them); and the
`metadata.imaging` index block — `is_imaging`, `coordinate_base: 1`, `pixel_count`,
`pixel_count_source` (`declared`, or `observed_max` when the counts are the largest positions: always
on the Bruker and Waters lanes), `pixel_size_um` (when both axes have a positive size in a length
unit: always in micrometres, converted where the grid states nanometres, millimetres or centimetres,
which stay as stated in `scan_settings_list`; a lone `IMS:1000046` the source
states gives both, as the vocabulary defines it) and a `provenance` record of what was detected and
where each value came from: `pixel_size` (`as stated`, `none stated`, or `checked, see
imaging_pixel_size`), and on the imzML and mzML lanes `time` — `as stated` when every spectrum
states `MS:1000016`, `stated on N of M spectra; the others are stored as 0` when only some do
(counted in the source: a stated 0 and no time read alike once stored), or `not stated by the
source; index is the source list order` when none does (every time is then stored as 0, no
TIC or base-peak chromatogram is synthesized, §7, and an mzML export of the archive states no
`scan start time` and sums no such pair, §4.1). A position stated as a scan cvParam (imzML, mzML) is written only as a
pixel index: x and y both present, integers from 1 to 2³² − 1; any other is removed from its scan,
all axes together, and declared `imaging:invalid-position-dropped`. A stated z that is not such an
integer is removed alone, the scan keeping x and y, and declared `imaging:invalid-position-z-dropped`.
An mzML whose sampled spectra state no position is searched in full for `IMS:1000050/51`, so
positions on the other spectra are not lost (an imaging mzML or imzML whose sampled spectra state no
z, for `IMS:1000052`); the `IMS` vocabulary is declared with the position columns, the marker only
once a position was written. `--tof-grid` does not apply to an imaging run: it is converted on the standard
lane with f64 m/z, with a warning. `--image` adds its `images[]` to that block. Positions count
from 1. imzML input keeps its positions and scan settings as stated (imzML already counts from 1),
with these checks since 0.16.0
(HUPO-PSI/mzPeak-specification#23):
the file provenance mzdata consumes — storage mode `IMS:1000030/31`, UUID `IMS:1000080`, the `.ibd`
checksum `IMS:1000090/91/92` — is written back into `file_description`, and the marker states the
**storage mode** (`metadata.imaging.storage_mode`: `continuous` or `processed`, as the header
states it). A **continuous-mode** imzML (`IMS:1000030`: every pixel holds the same m/z array) is
written with its **zero runs kept**, without `--keep-zero-runs` (owner decision D2, 2026-10-01):
masked, each pixel kept a different subset of the one axis under its own numpress fixed points, so
one source m/z decoded to several values across pixels (`Example_Continuous`: 171.33333 to 8
values in 9 pixels, 118 fixed points in 126 chunks; now one axis of 8,399 points for all nine, one
value, within the numpress bound of 1.75e-7 Da — and 243 kB against 263 kB masked, since the nine
spectra share one compression window; a sparse synthetic continuous file measured +12 %). No
`zero-run-mask` is declared, since none was applied; numpress stays the m/z encoding. Whether the
file's spectra did all hold one array is checked as they are written — each m/z array against the
first, bit for bit — and stated as **`metadata.imaging.shared_mz_axis`** (`true`, or `false` with a
warning counting the spectra that differ; absent for processed-mode input, whose pixels are masked
as before, `--keep-zero-runs` the override). A spectrum typed `MS1 spectrum` (`MS:1000579`) that
states **`ms level` 0** — the ms-imaging.org example files, the DESI and the GBM sets all do, in
their shared `spectrum1` param group — is written with `ms_level` 1, declared
`imzml:ms-level-0-as-1` and warned about once with the count (owner decision D6): the type proves
the level, and readers' MS1 filters and the summed TIC/BPC pair (MS1 spectra that state a time)
then apply to it; a spectrum that states no `ms level` at all reads the same way to mzdata and is
treated alike. The marker's **`mz_range`** is `[min, max]` of the **stored** m/z arrays over the run
(owner decision D10): of each spectrum the m/z the writer keeps — every point of a centroid
spectrum, of a profile spectrum with the zero runs kept, and with the mask on the points the mask
keeps, by the writer's own rule (an all-zero stretch at either end goes entirely, so the stored
range is narrower than the source array's on such a spectrum: on 34,839 of the 34,840 bladder
spectra the mask moves the first or the last stored point inward from the array's ends; the bladder
archive's `mz_range` is `[400.00003, 999.99868]`). It is
the range of the values handed to the writer; an m/z encoding with a bound (`fidelity.mz_error`)
moves a stored value within that bound. Written on the imzML and mzML-with-positions lanes. A **TIC
image** needs no structure of its own: it is a join of `spectra_metadata.total_ion_current` (and
`base_peak_intensity`) with the pixel positions in `spectra_metadata_scans`, on `spectrum.index`
— 0.16 s and 0.9 % of the archive's bytes for the bladder's 260 × 134 pixels; an ion image at one
m/z is a range query over the signal facets (`mz_range` says what the archive holds). The
header's **`<contact>`** is dropped unless `--keep-contact` (§4.1). The pixel size follows the
issue author's rule (x and y with a unit are kept; without one, micrometre is assumed; x and y of
which one is zero or negative are no pixel size and are dropped; a single value
is tested against its own axis's count and extent — the other axis's when its own states none — both
converted to one length unit, each by the unit it is written in (its unit name's when mzdata knows the
name, which then overrides the accession, else its unit accession), micrometre where that is no length
unit: it is an area when `√value × count = extent` and written as its square root, in the unit the
area is the square of, a length when `value × count = extent`, and otherwise dropped — `one value,
untestable` when the header lacks the count or the max dimension to test it against, which the row's
detail names), each action
declared and listed in the `imaging_pixel_size` index block together with any unit accession that
disagrees with its unit name; and the obsolete "one way" is written as flyback. A single
`IMS:1000046` the rule keeps is written under the vocabulary's names as both `IMS:1000046` ("pixel
size (x)") and `IMS:1000047` ("pixel size y"), same value and unit: the vocabulary defines
`IMS:1000046` as the y size too when no `IMS:1000047` is stated, so this declares nothing; a single
`IMS:1000047` stays alone. Where x and y are both stated and an axis states its count and max
dimension, `value × count` is compared with the max dimension: a disagreement is warned about and
listed in the row's `extent_mismatches`, the values written as stated. The **`.ibd` is hashed** in
one pass with the algorithm of each checksum the header states (`IMS:1000090` MD5, `IMS:1000091`
SHA-1, `IMS:1000092` SHA-256): `metadata.imaging.provenance.ibd_checksum` is `verified`, `mismatch`
or `not stated` (`not checked` when the `.ibd` could not be found or read for hashing, with a
warning). On a mismatch the conversion goes on — the stated value stays in `file_description`,
one warning names both hashes, `imzml:ibd-checksum-mismatch` is declared and
`provenance.ibd_checksum_found` holds the hash found (`accession`, `value`). The `.ibd` is listed in
`source_files` with the SHA-1 it hashes to. The **UUID** is checked the same way: an `.ibd` begins
with its 16-byte UUID, which the imzML states as `IMS:1000080`, and the two are compared whatever
the spelling (braces, dashes, case). `provenance.ibd_uuid` is `verified`, `mismatch` or `not stated`
(`not checked` with the checksum). A mismatch is the one imaging check that **refuses** rather than
declares (owner decision D7, 2026-10-01): a wrong checksum is a damaged copy of the right data, a
wrong UUID may be the wrong data — the two files are not the pair the imzML describes. The
conversion stops before anything is written, on the archive lane and the direct mzML export alike,
with a message naming both UUIDs (an `.ibd` shorter than the 16 bytes a UUID takes begins with
none, and the message gives its length instead); `--force` converts anyway, and then the stated value stays in
`file_description`, a warning names both, `imzml:ibd-uuid-mismatch` is declared and
`provenance.ibd_uuid_found` holds the 32 hex digits the `.ibd` begins with (0.17.0-rc.2 warned and
recorded without refusing; through 0.17.0-rc.1 a mismatch was one line of mzdata's log and nothing
in the archive, which could read `ibd_checksum: verified` over an `.ibd` that is not the imzML's).
A binary array typed with the imaging vocabulary's
obsolete `IMS:1000141` ("32-bit integer") or `IMS:1000142` ("64-bit integer") is read as
`MS:1000519` / `MS:1000522`, the terms that replaced them, declared
`imzml:obsolete-integer-type-as-psi-ms` (the mzML export does the same, with a warning). An m/z or
intensity array of an imzML or mzML that holds data but states no data type the reader knows
(`MS:1000519/521/522/523`) is an error naming the spectrum and the array, on the archive and the
mzML export lane alike; an empty array may leave its type out. An **mzML with positions** is not
run through the pixel-size rule: its scan settings stay as stated, and a single `IMS:1000046` there
gives the marker no `pixel_size_um` (it may be the area the term named until 2017, and nothing
tested it); x and y both stated in micrometre do. A **Bruker MALDI**
`.d` (TSF or TDF, every timsTOF lane) carries the same position columns from
`MaldiFrameInfo.XIndexPos/YIndexPos` per frame. Those are absolute raster indices on the target, so
they are **shifted so the smallest is 1** — one shift for the whole run,
keeping regions where they lie relative to each other — declared
(`bruker:raster-index-shifted-to-base-1`) and recorded as the imaging profile has it,
`metadata.imaging.position_offset` = the constant subtracted (smallest index − 1), with the smallest
index itself as `origin` in the `bruker_maldi` block; the pixel counts are the shifted extent. FlexImaging's own imzML export
keeps the absolute indices (with the pixel counts set to the largest index), so a `.d` and its imzML
export differ by exactly `origin − 1`. The pixel size is the raster step of the FlexImaging sequence
`<stem>.mis` beside the `.d` (not part of it): `RegionNumber` n is the n-th `<Area>`, which also names
the region; when the acquired regions have different steps no pixel size is written (the profile
describes one grid). A `.mis` the regions do not map onto — a region number with no `<Area>`, or
regions whose `MotorPositionX/Y` no single offset places inside their areas' bounding boxes (within
half a raster step) as the `.mis` teach points locate them — is not used, with a warning and the
reason in the block. Without a usable `.mis` the frames' beam scan size is the fallback, declared as
such, when every positioned frame states the same finite one: `BeamScanSizeX/Y` of the
`MaldiFrameLaserInfo` row the frame's `MaldiFrameInfo.LaserInfo` names (of `MaldiFrameInfo` itself in
a schema that has the columns there), unstated where that row's `BeamScan` is 0 — the beam was not
scanned, as on both MSV000088438 runs. The max dimension `IMS:1000044/45` is
count × size. Each positioned frame's scan states its acquisition region, `MaldiFrameInfo.RegionNumber`,
as the parameter `acquisition region` in its `parameters` list (a parameter without an accession: the
imaging profile names no region column yet), on every timsTOF lane. A frame without a
`MaldiFrameInfo` row, or with a NULL index, has no position: its scan is written with null
`position_x` / `position_y`, the block counts such frames (`frames_without_position`) and the
conversion warns once. An empty frame that has a row keeps its pixel. The `bruker_maldi` index block
holds the regions (number, name, raster step, frames, raw index ranges), the `.mis` it read (or
rejected, and why), the beam scan size and where it was read (`beam_scan_size_source`).
A **Waters imaging** `.raw` (MALDI or DESI; Windows, MassLynx) states each scan's laser aim position
in mm (MassLynx scan items "Laser Aim X/Y Position"), not a pixel: the converter fits a grid to them
and writes the grid index, declared (`waters:laser-position-fitted-to-grid`), with the fit (origin,
step and its source, count, largest residual) in the `waters_imaging` block and each axis's step as
its pixel size. The step is the one the method declares (`methodfile.xml` `DesiXStep`/`DesiYStep`,
any `…XStep`/`…YStep`) when the positions lie within a quarter step of it. Otherwise the positions
must lie on an exact lattice, as the stage's set points do: positions within 1 µm are one, and the
step is the largest gap between neighbouring distinct positions (3 µm or more) whose lattice, laid
at that gap, holds all but 1 % of the scans in one 1 µm window. The step written is that gap refined
by least squares within float noise, and a whole µm or 0.1 µm when the positions cannot tell it from
one; a position within half a µm of its grid point is on it. The fit refuses rather than guesses:
jittered positions, a serpentine lag, regions rastered from origins off one lattice and rotated
rasters fit no lattice and need the declared step; positions recorded at 3 µm or coarser, or lagging
by a whole finer step, fit that finer lattice, each at its own pixel. A lattice that is a fraction of
a coarser one, needed only by columns holding at most half the scans of the coarser lattice's
median column (strays, not raster columns), is refused too; a row acquired twice or a one-row
region is not such a coarser lattice, but tiny plus-shaped cores (1, 3, 1 scans per column) are
refused. Lock-mass scans get no
position. Up to 1 % of the positioned scans may lie off the grid, or far outside the raster at one
position (a parked scan, even one on the grid by chance): they get no position
(`waters:off-grid-position-dropped`). A group far outside that spans columns (a QC region) is part
of the raster, and so are parked scans beyond 1 %. More than 1 % off the grid, or no grid at all:
the run gets no positions and no marker, and `waters_imaging` says why. The vendor SQLite databases the converter
opens itself are opened immutable — or, when a `-wal` beside one still holds rows or its `-journal`
is hot, read from a scratch copy — so those reads write nothing into the `.d` (a read-only open of a
WAL-mode MALDI TSF used to leave `-shm`/`-wal`). A BAF `.d` is the exception: Bruker's baf2sql
library, which the BAF lane reads through, creates its `analysis.sqlite` cache beside `analysis.baf`
when the run has none.

**Waters encodings come from a pre-scan.** Before the run is written, a sample of it (four stretches
of consecutive spectra spread over the run, up to 64 spectra or 2 M points each) is written once per
trial through the same writer, each trial trying another encoding for every data-facet column, and
each column keeps the arm whose compressed bytes came out smallest (a tie keeps the writer default):

| column | arms |
|---|---|
| m/z | delta chunks (exact for these float32 m/z) under dictionary, byte-stream split or plain encoding; numpress-linear unless `--no-numpress` |
| intensity | float32 under byte-stream split or dictionary; the same values as int32 (MS:1000519) under either, when every sampled intensity is an integer in int32 range |
| ion mobility | dictionary, byte-stream split or plain |

The `encoding_prescan` index block states the sample size, every arm's bytes and the choice. Parquet
records its encoding per page, so any reader reads every arm; int32 intensities are the vendor's
values unchanged. If a spectrum the sample did not see carries an intensity int32 cannot hold, the
run is written again with the smallest float32 arm and the block says so (`int32_fallback`).
Measured on PXD063409 `20181112_HDMSE_CK1` (2.1 G points) before this lane had it: numpress m/z
2.02 GB against 1.45 GB delta, float32 byte-stream-split intensity 1.60 GB against 1.08 GB as
int32, and float ion mobility under byte-stream split 3.3× its dictionary size. `MZPC_ENCODING_PRESCAN=0`
(§10) keeps the fixed encodings. The scan
row's `ion_mobility_value` stays NULL on purpose: a frame has no single drift time. Retention time,
polarity, scan window, the MS level (from the function-type code: product-ion types are MS2, the
second function of an MSe pair is MS2, every other MS function — lock mass, auxiliary — is MS1) and
the precursors come from the SDK: a set mass > 0 (DDA) gives a selected ion with a target-only
isolation window and the collision energy; an MSe elevated-energy scan (set mass 0) gets a
precursor whose isolation window is the function's acquisition mass range (target = midpoint), flagged
by the activation parameter `isolation window source = acquisition mass range` and without a selected
ion — ProteoWizard's convention; the file states no narrower window. Chromatogram functions
(SIR, MRM, neutral loss/gain) and non-MS functions (DAD, delay, calibration) are skipped with a log
line; a SONAR function (its bins are quadrupole positions, not drift times) is written as the
drift-summed scan with a warning and flagged in `waters_drift`; "collapsed retention time"
functions (one row per drift bin, the run's summed mobilograms — Capan2 functions 4–6) are
recognised and not written as spectra (`MZPC_WATERS_KEEP_COLLAPSED=1` keeps them). The synthesized TIC/BPC
include the lock-mass function's frames, as ProteoWizard's do. A function whose type, drift-bin
count or SONAR flag the DLL cannot report is written as MS1 / summed / drift-on-trust with a warning
naming the function; a scan without a retention time refuses the conversion.

The native SciEX lane reads each spectrum's precursor where ProteoWizard's ABI reader does. The
selected ion is a product spectrum's parent m/z, with its charge when one is stated. A Product
experiment (a DDA or MRM-HR scan, or one SWATH window: each variable window is its own experiment)
adds the isolation window, parent m/z ± half the experiment's width, target-only when no width is
stated. The collision energy is the experiment's `CE` when it is one value; a ramp is kept as
`collision energy ramp start` / `end` (MS:1002013 / MS:1002014) rather than as a midpoint. The
dissociation method is beam-type CID, as ProteoWizard assumes, on an instrument that can only
fragment in its collision cell. A ZenoTOF can also fragment by EAD, which Clearcore2 does not report
per experiment (ProteoWizard reads the mode only through the `.wiff2` API), so its precursors carry
no method; neither does a file that names no instrument. A precursor-ion scan states no precursor:
its fixed mass is a product, which the archive has no place for. This has not yet been run on a
WIFF.

**What an mzML or imzML conversion keeps as stated, and what it does not carry.** mzdata, which
reads both, types every param value by trial parse and reads only part of the header; the lane
reads the rest back from the source text (`src/mzml_refs.rs`, `src/imaging.rs`):

- A source file's **checksum** — SHA-1 (`MS:1000569`), MD5 (`MS:1000568`), SHA-256 (`MS:1003151`) —
  is the text the header states. A digest of decimal digits only, or of digits with one `e`, used
  to be stored as a number (`…0123` as 123; ProteoWizard's own `tiny.pwiz` example as
  1.2345678901234568e39), as the `.ibd` checksums were through 0.16.0.
- A header value that reads as **NaN or infinity** (a userParam whose text is `NaN` or `Inf`, a
  digest with an exponent beyond a 64-bit float) is stored as the string Rust prints for it —
  `NaN`, `inf`, `-inf` — since JSON has no such number; through 0.17.0-rc.1 it aborted the
  conversion (exit 134, no archive). The spelling is mzdata's reading, not the source's (`Inf`
  becomes `inf`): any other text value that happens to parse as a number is still stored as that
  number.
- A spectrum's **`sourceFileRef`** attribute (the DESI ColAd imzML names one of 135 raw line files
  on each of 17,820 spectra) is the spectrum parameter `sourceFileRef` — no accession; its value is
  the id of an entry of `file_description.source_files` — and a `userParam` of that name in both
  mzML exports, where it names an entry of the export's `sourceFileList`: the direct export lists
  the source's files and an archive's export the archive's own (§4.1). One that names no listed
  source file is dropped and declared (`mzml:dangling-reference-dropped`; the direct mzML export
  leaves it out and counts it in its one warning). The same attribute on a `<scan>` or a
  `<precursor>` (with `externalSpectrumID`: a spectrum of another file) is not carried.
- A source's **processing methods** keep the terms they state. `file format conversion`
  (`MS:1000530`) is added only to a method none of whose terms is a child of `MS:1000452` data
  transformation in the PSI-MS vocabulary the binary embeds (a method with no CV term at all among
  them), because the spec's `processingmethod_must` rule requires one; through 0.17.0-rc.1 every
  source method gained it, so a `low intensity data point removal` step also claimed a format
  conversion. The conversion's own method (`mzpeak_convert_conversion`) states the term itself, on
  every lane, beside `MS:1003901` when it trimmed zeros.
- The run's **`startTimeStamp`** without an offset is the `acquisition_time` block (above).
- A chromatogram **intensity array's unit** other than detector counts is kept: on the array where
  the value is no intensity (a device trace), as the chromatogram's `intensity array unit`
  parameter where it is one, or where mzdata does not know the unit (§7).
- **`fileDescription/<contact>`** — the contact's name, organization, address, URL and e-mail
  (`MS:1000586`–`MS:1000590`) — is **dropped by default**, with one note naming the contact
  (owner decision D11, 2026-10-01): an archive is copied and published, and personal data would
  travel with every copy. `--keep-contact` (config `keep_contact`) carries every `<contact>` of
  the header, params as stated and in order, into the archive's `file_description.contacts` (the
  spec's slot: `contact_name` from `MS:1000586`, `contact_affiliation` from `MS:1000590`,
  `parameters` with every param) and into the direct export's `<fileDescription>`, as `<contact>`
  elements after the source files, where the schema has them. The export of an archive writes the
  contacts its index holds, flag or not — the decision was taken when the archive was written.
  mzdata's model has no contact, so the header is read for them (`src/mzml_contact.rs`), as it
  is for an imzML's provenance. Through 0.17.0-rc.2 nothing of a contact reached any output.
  A spectrum's `ms level` of 0 on a spectrum typed `MS1 spectrum` is written as 1 by both
  exports of an imzML (§8, imaging), with a warning on the direct lane. **Not carried:** a
  spectrum's `spotID`, and the `sourceFileRef` / `externalSpectrumID` of a scan or precursor.

**What the native lanes still do not carry** (tracked in BACKLOG.md): per-scan precursors on
the Agilent-MHDAC and BAF lanes (Bruker TDF/TSF, Shimadzu, Waters and SciEX have them), and the
non-MS device chromatograms (UV, pressure, temperature) the mzML lane gets from pwiz — except on a
Bruker `.d`: every Bruker lane writes the HyStar traces in its `chromatography-data.sqlite` after
the TIC/BPC (not `--to mzml` straight from the `.d`, which writes the TIC/BPC pair only), each
value array in the unit HyStar states and each chromatogram type also as a parameter, so an mzML
export states it (a trace in bar is stated in pascal as 64-bit floats, declared
`bruker:trace-unit-rescale`; a trace stored in overlapping chunks is written in time order with
each repeated sample once, declared `bruker:trace-sort-dedup`). HyStar's own MS traces, its
MS/MS TIC `TIC,±AllMS/MS` among them, give way to the synthesized TIC/BPC. A database in WAL mode
is read like one with a rollback journal, without writing into the input.

**Mapped metadata (into the archive's typed columns).** Where a vendor value has a
PSI controlled-vocabulary meaning, it is mapped onto the standard
`spectra_metadata` columns — MS level, polarity, scan start time, precursor m/z /
charge / isolation window, ion-mobility (`mean inverse reduced ion mobility` for
TDF), and the spectrum type — `MS:1000579 MS1 spectrum` / `MS:1000580 MSn spectrum`, which the
writer infers from `ms_level`. (Since 0.9.13 **no lane adds the generic `MS:1000294 mass spectrum`**;
the vendor lanes used to, and because mzdata's `spectrum_type()` is first-match that parent term
shadowed the inference on every SCIEX / Waters / BAF / SDK / Agilent row — archives from ≤ 0.9.12
carry `MS:1000294` in `spectrum_type` where 579/580 was meant.) Bruker TSF/BAF m/z is produced from the
vendor calibration (TSF applies the otofControl ±Th correction); Bruker TDF stores
every point on the reference implementation's chunk grid under the vendor's **exact** calibration:
each frame's rows carry its `MzCalibration` row as the model's 7 parameters at the frame's own
`T1`/`T2` (`mz_grid`, §9), and the `TimsCalibration` ModelType-2 row as the mobility model
(`mean_inverse_reduced_ion_mobility_grid`), so a reader evaluating the rows — as the reference
implementation does — gets the vendor's m/z and 1/K0 to 1e-9 ppm (SDK-verified on a `C2 ≠ 0`,
`C4 ≠ 0` file). The archive also carries the calibration verbatim: the `vendor_mz_calibration`
index block holds every `analysis.tdf` `MzCalibration` row plus `DigitizerNumSamples` /
`MzAcqRangeLower` / `MzAcqRangeUpper`, `vendor_tims_calibration` the `TimsCalibration` rows, and
`spectra_metadata` gains per-frame `…_tdf_t1`, `…_tdf_t2`, `…_tdf_mz_calibration_id` columns
(`Frames.T1/T2/MzCalibration`; `MZP:1000008`–`MZP:1000010` since 0.10.1, `MS:4000903`–`MS:4000905`
before — match the suffix), the provenance of each frame's model. Both are present with
`--no-vendor` too. `ims_calibration` says `"exact": true` when every row is ModelType 1; its `chord` entry records timsrust's
two-point chord `(a + b·tof)²` (−5…−11 ppm against the vendor model) only as the model a frame
WITHOUT a usable `MzCalibration` row is stored on (as an `MS:1003825` sqrt model), never as the
archive's calibration.

**ModelType 2.** A few timsTOF files carry `MzCalibration` rows of ModelType 2 (in the example
corpus, one of 33: the SBA415 timsTOF Pro run). Bruker's library evaluates them as the ModelType-1
quadratic on `C0`, `C1`, `C2` (no `C4` shift) and then subtracts a calibrant polynomial,
`m/z = m − Σ_{i<C7} C[8+i]·mⁱ` for `C5 ≤ m ≤ C6` and `m/z = m` outside; `C3`/`C4` repeat
`C0`/`C2` in these rows. Pinned against the SDK to 1e-9 ppm (OpenTIMS's `test.d`,
`tests/fixtures/tdf_modeltype2_sdk_golden.json`). The reference implementation's grid model has
no place for the polynomial, so such a run's rows carry the quadratic — the TOF bins stay exact —
and the archive says `"exact": false` with the `approximation` spelled out, `max_error_ppm` (0.66 ppm
on SBA415) and `bruker:mz-calibrant-omitted` in `transformations`; the row itself, polynomial
included, is in `vendor_mz_calibration`. **Archives written by 0.13.0 read ModelType-2 rows as
ModelType 1 and are wrong by an order of magnitude (m/z 270 stored as 21): reconvert them.** mzdata
0.67.1 has the same defect, so the `--no-ims-compact` and fallback lanes read such files on
timsrust's chord instead (`bruker:mz-calibration-chord`), and so does `--to mzml`, with a warning
in place of the declaration an mzML has no list for (through 0.16.0 that export was an order of
magnitude low).

**History.** Through 0.13 the archive stored integer `tof` columns with the chord in
`ims_calibration` (`"exact": false`), and — when every row was ModelType 1 with `C2 = 0` — per-frame
`opt_MZP_1000003_tof_c0` / `opt_MZP_1000004_tof_c1` columns (`m/z = (tof_c0 + tof_c1·tof)²`,
`ims_calibration.per_spectrum`); 0.13.0 rewrote its chunked facet into the grid layout in a second
pass. Since 0.14 the grid is written natively for every frame, `C2 ≠ 0` rows included, and the
`tof_c0`/`tof_c1` columns are gone; archives of every earlier generation still read. The vendor
formula is checked against Bruker's own library: `MZPC_TDF_SDK_GOLDEN=<out.json>` (§10) dumps the
SDK's `tims_index_to_mz` at up to 240 `(frame, tof)` points during a `--bruker-sdk` conversion, and
the dumps of 2485.d (`tests/fixtures/tdf_2485_sdk_golden.json`) and of a `C2 ≠ 0`, `C4 ≠ 0`
diaPASEF run (`tests/fixtures/tdf_diapasef_sdk_golden.json`) hold the grid model to 1e-6 ppm and
every digitizer bin to an exact inversion (`src/bruker_native.rs`).

**Several precursors on one spectrum (timsTOF PASEF).** dia-PASEF writes two precursors
per MS2 frame and DDA-PASEF several, all with the same `(source_index, precursor_index)`
join key — the key the spec gives is not unique per precursor. The reference reader
(vendored here) therefore keeps the precursors in their stored order (a stable sort; the
unstable one reordered them against their ions) and, where a spectrum's precursor and
selected-ion counts agree, pairs them **positionally in row order** — the only reading
the archive supports. Where the counts differ (one precursor with several ions, SPS-MS3;
or ions missing) nothing is assumed and every ion is attached to the first precursor as
before. Other readers should apply the same rule; a per-spectrum precursor ordinal in the
spec is the long-term fix. The HUPO Python reference reader (`hupo-mzpeak/python`, as of
2026-09-30) does not yet: it raises on a spectrum with several precursors and no parent
(`Length of values (1) does not match length of index (4)` on every diaPASEF frame of an
ims-compact archive) and on its retention-time lookups (`KeyError: 'time'` from `.time[…]` and
`extract_tic()`); both are reader-side defects, filed upstream, and the archives read in the Rust
reader and this converter.

**Shimadzu `.lcd` (native, Windows).** Each vendor point carries a coarse `Mass` (Int32,
a 1e-4 Da lattice — what ProteoWizard reads) and `MassHigh` (Int64, 1e-9 Da), and
`MassHigh` is what LabSolutions' own mzML exporter writes: the converter reads `MassHigh`,
so the archive's m/z equals the LabSolutions export (measured 5.7e-14 on Blind_P1_pos_012, i.e.
the last f64 digit).
The scale is established once per file (a power of ten fitted over ≥ 1,000 points) and
never mixed within a file; `MZPC_SHIMADZU_COARSE_MZ=1` restores the coarse `Mass`. Both
representations are kept (`--representation`, §4): profile goes to `spectra_data` as a
per-spectrum sqrt grid and centroids to `spectra_peaks` as an exact Int64 lattice (§9),
nothing is snapped and a spectrum that fails either guard keeps its f64 m/z. Beyond the
signal: precursor m/z / charge / isolation window, the scan window, the instrument
configuration (model, ESI source, quadrupole + TOF analysers, as the API states them — no
detector is invented) and the source `SHA-1` (MS:1000569), which is taken **before** the
vendor DLL opens the file because `Shimadzu.LabSolutions.IO` holds a byte-range lock for as
long as it does. **One library version to avoid:** `Shimadzu.LabSolutions.IO.IoModule.dll`
**3.8.4.6016** returns, for a spectrum that carries no profile signal, a centroid list whose
intensities are shifted against their m/z by 1–7 positions with the last peak missing.
Version **5.0.0.0** — shipped by a current ProteoWizard (3.0.26151 and 3.0.26175 verified) — reads the same
files correctly, so **the remedy is to point `$MZPC_PWIZ_DIR` at a current ProteoWizard**
(§11), not to fall back on a LabSolutions mzML export as this manual advised before v0.9.9.
msconvert appeared to confirm the defect only because it was driving the same 3.8.4.6016 DLL
out of the same directory. Files whose spectra carry profile signal were always bit-exact
through either library. On a stale library the converter still stores the peaks exactly as
returned (correcting vendor data is not its job) and logs one warning per file naming the loaded
version and the fix; that warning fires only when the loaded library reports a major version
below 5 **and** the file stores no profile signal, so a current ProteoWizard never raises it.
Archives
converted from a profile-less `.lcd` before v0.9.9 carry the misaligned intensities and should
be reconverted. See `glue/shimadzu/README.md` for the measurements and for how to check the
installed version.

**Fidelity: what is preserved, and the declared transforms.** The project invariant
(decided 2026-09-04) is that the archive preserves the vendor's signal **as much as possible, to a
stated fidelity, with every transformation declared** in the index's `transformations` list — so a
reader can tell from the archive alone what was done to the data. Retention time, precursor m/z
and charge, centroid m/z (f64, or the bit-exact fixed-point lattice of §9) and integer TOF
round-trip bit-for-bit; verified against mzdata's own mzML output on a 4,880-spectrum DDA run with
zero differences. Four general signal transforms are **not** bit-exact, and each is named in the
archive; the lane-specific changes, each declared when it happens, are in the table under
**The `transformations` index key** below:

1. **numpress-linear** (`numpress-linear`) — the *default* chunk encoding of profile m/z on the `chunked` layout is
   lossy (§9); `--no-numpress` selects the delta encoding, exact for 32-bit m/z values and wherever a
   64-bit m/z is at most twice its predecessor (§4). The bound of every numpress archive is in its
   `fidelity` block.
2. **Profile zero-run compaction** (`zero-run-mask`) — in **profile** spectra, a run of two or more *consecutive*
   zero-intensity points is collapsed to a single zero at each peak boundary —
   `[0,0,0,0,0, 900, 500, 0,0,0, 300, 0]` (12 points) is stored as `[0, 900, 500, 0, 0, 300, 0]`
   (7). The baseline extent of every peak is preserved, so the profile shape is unchanged, but
   `number_of_data_points` reflects the stored count rather than the source's. **Centroid spectra
   are never touched** — isolated and interior zero-intensity centroids round-trip exactly.
   `--keep-zero-runs` turns the compaction off, and a continuous-mode imzML (`IMS:1000030`) has it
   off without the flag, so its pixels decode to the one axis the file stores (imaging, below); how
   many points it dropped is in the `fidelity` block.
3. **`--tof-grid` sqrt grid** (`tof-grid:<ppm>ppm`) — a spectrum is stored on an integer sqrt-space grid (a
   chunk-grid row under its `MS:1003825` model) only when every point reconstructs within the ppm bound
   (`MZPC_TOF_GRID_PPM`, default 5). Spectra outside the bound keep f64 m/z as raw chunk rows.
4. **Fitted linear grid on lattice centroids** (`grid-fit:1e-6Da`) — a centroid list whose m/z sit on a
   fixed-point lattice (Shimadzu `MassHigh`, the LabSolutions mzML export) is stored as a chunk-grid row
   under the reference implementation's fitted `MS:1003824` model, every value within 1e-6 Da (≤ 3e-7 Da
   in practice); `--no-mz-lattice` keeps the exact f64.
5. **Shimadzu profile pad trim** (`shimadzu:span-trim`) — the native `.lcd` route fits and stores the signal span between
   the first and last positive sample; the zero-intensity pad LabSolutions writes at the
   scan-window bounds is not stored (the span bounds are).

Where a lane stores a grid instead of the source m/z, the model a reader evaluates rides on every
grid row (`mz_grid {grid_type, parameters, indices}`, the reference implementation's layout) and the
bound rides in `transformations`: `tof-grid:<ppm>ppm` for the statistically fitted SCIEX / mzML sqrt
grids, `grid-fit:1e-6Da` for lattice centroids; the Shimadzu profile grid and the Agilent file-direct
grid carry the vendor's own bin ordinals under exact models and declare nothing (the Shimadzu fit is
accepted only when every point rebuilds within 1e-9 Da, the vendor's own rounding). Archives written
by 0.13 and earlier carried these grids as integer point columns with `tof_calibration` /
`mz_calibration` index blocks; they still read.

**The `transformations` index key.** Every mzPeak lane writes `metadata.transformations` — a JSON
list of the declared, bounded changes that were APPLIED to this archive's stored data on the way in
(`transformations_block` in `src/main.rs`). Each entry is written only when the change happened at
least once, counted while the archive was written, never inferred from what the lane was configured
to do; so a list with none of the entries that touch spectrum signal (the set `--lossless` refuses,
`SIGNAL_TRANSFORMATIONS` in `src/fidelity.rs`: a point left out, re-ordered or summed, an m/z or
intensity value moved) says the signal is stored as the reader handed it over, and
`fidelity.mz_error` is then empty as well. What a reader library leaves out before the lane sees
it is in the list only where the lane counts it against the file, as it does for a timsTOF
frame's points, and a vendor glue that narrows a value before the lane holds it declares what it
counts under its own entry (the SciEX glue hands over float32 intensities and counts the clamped
ones). Through 0.16.0 three value changes were in no entry: a 64-bit m/z a delta chunk
returns one unit in the last place off (now `delta-ulp`), an intensity stored as the nearest
float32 (now `intensity-f32-rounding`) and an intensity cast into an integer column that does
not hold it (now `intensity-type-narrowing`); an archive written by an earlier version may hold
any of them under an empty list. Archives written by 0.11.5
and earlier listed `zero-run-mask` on every lane and `numpress-linear` whenever the codec was chosen,
whether or not a spectrum was masked or a chunk encoded. An entry names the transformation, never
how often it was applied: a count goes to the run's warning. `tof-grid:<ppm>ppm` and `grid-fit:<Da>Da` are
the two entries with a parameter, and each names the bound its grid was accepted under.
The vocabulary:

| Entry | Written when | Lanes |
|---|---|---|
| `zero-run-mask` | the writer's zero-intensity run compaction shortened at least one profile spectrum (item 2) | every lane whose writer masks (not native Waters frames) |
| `numpress-linear` | at least one m/z chunk is stored with the lossy codec (item 1) | chunked layout without `--no-numpress` |
| `delta-ulp` | a 64-bit m/z facet holds at least one delta chunk whose last m/z is more than twice its first (or that does not start above zero), where `b + (a − b)` can round: a decoded m/z can be one unit in the last place off its source value, and so can the values after it in the chunk. Declared from the writer's count of such chunks, the count `fidelity.mz_error` then reports with the bound; not for a facet whose source m/z are all 32-bit values, which delta returns exactly | chunked layout with `--no-numpress` (or a lane that chose delta: the m/z lattice fallback, a native lane's pre-scan), on sparse 64-bit m/z |
| `intensity-f32-rounding` | at least one intensity was stored as the float32 nearest to a source value no float32 holds (a 64-bit float, or an integer above 2^24). Three ways there: a centroid spectrum reaches the peak facet through mzdata's peak set, whose intensity is a float32 whatever the file declares; a `--tof-grid` grid row carries float32 intensities; and a facet's intensity column has one type, taken from the spectra sampled before the first is written — one that holds every sampled type (the next entry) — so in a file whose intensity arrays change type after the sampled spectra a later 64-bit array (profile signal, or a centroid spectrum with a third per-peak array, which the writer stores from its arrays), or an integer array above 2^24, is cast into a float32 column. Counted against the source arrays as each spectrum is written, for the type the facet's column has; the facet's `intensity_values_rounded` in `fidelity` holds the count and the run's warning states it. 64-bit intensities that are all float32 values (every such file of the example corpus) declare nothing; `--lossless` stores the source's type | mzML/imzML lanes, `--tof-grid`, native vendor readers (a spectrum whose intensities the reader hands over wider than the column) |
| `intensity-type-narrowing` | at least one intensity was cast into a column of another type than float32 that does not hold its value. A facet has one intensity column, typed from the spectra the writer samples before the first is written (every spectrum of a file of 50 or fewer, five spread over a larger one) that the facet stores — the peak facet's from the sampled centroid spectra alone, through the peak set or the arrays they are written from (through rc.2 a sampled profile spectrum's 64-bit intensity array typed it too: a float64 peak column holding float32 peak-set values, larger and no more exact); a file of more than 50 spectra whose five sampled ones are all profile gives the peak facet a float32 intensity, the peak set's. When the sampled arrays mix types the column takes one that holds them all — both integer widths → int64, both float widths → float64, an integer type beside a float type → float64 — and a file whose types the sample shows declares nothing. Through 0.17.0-rc.2 the first sampled spectrum's type was the column's: a file with 32-bit integer counts in its first spectra and 64-bit floats later got an int32 column, and 40,000 of 80,000 floats of a generated file were stored as the int32 maximum. What remains is a switch after the sampled spectra: the later arrays are cast into the column, in both layouts — a float cut to an integer, an integer or a float out of an int32 column's range clamped to it, a 64-bit integer above 2^53 in a float64 column. (Through rc.2 the chunked layout filed an integer array of another type than the column's in the spectrum's `auxiliary_arrays` with the facet row left empty, and the point layout stored a null for each value out of the column's range, which a reader takes as an absent intensity array when a spectrum's are all null.) The facet's `intensity_values_narrowed` holds the count and the run's warning states it; `--lossless` on such a file stores a type that holds every value or fails. A file with one intensity type throughout never declares it | mzML/imzML lanes, `--tof-grid` |
| `sort-by-mz` | at least one spectrum was re-ordered into m/z order before it was stored: by the lane itself, or by the writer's backstop for a spectrum a reader handed over unsorted | generic mzdata lane, `--ims-chunked` (each frame by TOF across mobility scans; mobility is stored per point), `--bruker-sdk` TDF (the SDK hands over mobility-major frames), native Waters frames, and any lane whose reader hands over an unsorted spectrum |
| `sort-by-time` | the writer's backstop re-ordered at least one chromatogram into time order before it was stored | any lane that hands the writer a chromatogram out of time order, a source chromatogram or the MS1 TIC/base-peak trace synthesized in spectrum order |
| `sort-by-wavelength` | the writer's backstop re-ordered at least one wavelength (UV/PDA) spectrum into wavelength order before it was stored | any lane that writes wavelength spectra handed over out of order |
| `chromatogram-time-to-minutes` | at least one chromatogram time recorded in seconds or milliseconds was divided into minutes, the unit `chromatograms_data` declares on every lane, as a 64-bit float (not bit-exact). A time array that states no unit is stored as given | mzML/imzML with source chromatograms (ProteoWizard writes seconds), Bruker `.d` with `chromatography-data.sqlite` (HyStar records seconds) |
| `mzml:chromatogram-intensity-unit-as-parameter` | at least one chromatogram's values in the `intensity` column are in another unit than the detector counts the column declares: an intensity the source states in counts per second (`MS:1000814`), percent of base peak (`MS:1000132`, `MS:1000905`), an ion-current chromatogram's in any other unit, or one in a unit mzdata has no name for. The values are stored as stated; the unit's accession is that chromatogram's parameter `intensity array unit` (§7) | mzML with source chromatograms |
| `tof-grid:<ppm>ppm` | a statistically fitted integer sqrt grid replaced f64 m/z within that bound (item 3); `fidelity.mz_error` states the largest error the accepted grid left beside it | mzML `--tof-grid`, native SCIEX per-spectrum grid |
| `grid-fit:1e-6Da` | a centroid list on a fixed-point lattice was stored under the reference implementation's fitted linear grid, every value within 1e-6 Da (item 4) | generic mzML lane (lattice detected), native Shimadzu `.lcd` centroids |
| `grid-encode:mz`, `grid-encode:mz,ion_mobility` | every point of a timsTOF frame is stored as its integer TOF bin under the frame's own `MzCalibration` model and, in the second form, its TIMS scan number under the vendor's ModelType-2 mobility model (the first form: 1/K0 as plain values, with `--no-tims-recalibration` or without such a row). Exact: the rows hold the file's own integers, the models are in `ims_calibration` and on every row, and the entry has no `fidelity.mz_error` counterpart. It names the encoding, as 0.13.0 declared it; what a ModelType-2 or chord model leaves out is `bruker:mz-calibrant-omitted` / `bruker:mz-calibration-chord` | timsTOF ims-compact (native, `--bruker-sdk`) |
| `shimadzu:span-trim` | the profile sqrt-grid route left the zero-intensity pad at the scan-window bounds out of at least one gridded spectrum (item 5) | native Shimadzu `.lcd` profile |
| `bruker:mz-calibrant-omitted` | a timsTOF run's `MzCalibration` is ModelType 2: the grid rows carry the quadratic on `C0`–`C2`, without the vendor's calibrant polynomial (bounded by `ims_calibration.max_error_ppm`, which `fidelity.mz_error` states with its share of the largest stored m/z; the polynomial verbatim in `vendor_mz_calibration`) | timsTOF ims-compact (native, `--bruker-sdk`) |
| `bruker:mz-calibration-chord` | a timsTOF run's m/z came from timsrust's two-point chord instead of its `MzCalibration` model: no usable row or an unsupported model type (ims-compact), or a ModelType-2 file read through mzdata, which reads every row as ModelType 1 (`--no-ims-compact`, the fallback lane) | timsTOF |
| `bruker:out-of-window-points-dropped` | at least one point of a PASEF MS2 frame lies in a TIMS scan outside every isolation window of the frame and is in no spectrum: mzdata's TDF reader hands such a frame over as one spectrum per window (diaPASEF: per window of the frame's group; ddaPASEF: per precursor) and nothing for the other scans. A diaPASEF frame is recorded over the whole TIMS ramp, so its windows leave points out (2485.d: a quarter of an MS2 frame's); the one ddaPASEF run checked (PXD078573 9629.d) holds no point outside its precursors' scans and declares nothing. Counted per frame, the points handed over against `Frames.NumPeaks`; `fidelity` states the sum of `Frames.NumPeaks` over the frames read as `source_points`, and the run's warning the count. The default ims-compact lane stores every point | timsTOF `--no-ims-compact` (the `.d` → mzML export drops the same points and warns, with no list to declare it in) |
| `shimadzu:coarse-mz` | the glue read the coarse 1e-4 `Mass` field instead of `MassHigh` (`MZPC_SHIMADZU_COARSE_MZ=1`): profile and centroid m/z 100× coarser than the file holds | native Shimadzu `.lcd` |
| `agilent:drop-zero-samples` | the profile grid lane left at least one zero-intensity sample, or an all-zero scan, out of its sparse point lists | `--agilent-grid` |
| `agilent:intensity-f32-rounding` | an integer count above 2^24 was rounded into the Float32 intensity column, counted by the profile grid reader; the same kind of change as `intensity-f32-rounding`, under the name this lane has declared it by since 0.11 | `--agilent-grid` |
| `agilent:nonfinite-intensity-to-zero` | MHDAC returned a NaN or ±Inf intensity, stored as 0 (counted by the net48 host over the scans it exported, which under `MZPC_MAX_SPECTRA` are the written ones) | native Agilent (MHDAC) |
| `agilent:truncate-unequal-arrays` | a spectrum's m/z and intensity arrays differed in length and were cut to the shorter (counted by the net48 host, as above) | native Agilent (MHDAC) |
| `waters:drop-functions` | a MassLynx function was not written as spectra: chromatogram-type (SIR/MRM/NL/NG), not MS (DAD, delay, …), its scan count unreadable (`getScanCount failed`), or a collapsed retention-time summary not kept by `MZPC_WATERS_KEEP_COLLAPSED` | native Waters `.raw` |
| `waters:sonar-summed` | at least one written scan is a SONAR function's quadrupole bins summed into one scan (counted as the scans are read) | native Waters `.raw` |
| `sciex:nan-intensity-to-zero` | the glue mapped at least one NaN intensity Clearcore2 returned to 0 (counted per spectrum; a warning gives the total) | native SciEX `.wiff` |
| `sciex:clamp-intensity-to-f32` | at least one intensity beyond ±`f32::MAX` (±Inf included) was clamped to it when narrowed to the schema's f32 | native SciEX `.wiff` |
| `sciex:truncate-unequal-arrays` | Clearcore2 returned m/z and intensity arrays of different lengths for at least one spectrum, and the longer was cut to the shorter | native SciEX `.wiff` |
| `imzml:pixel-size-unit-assumed-um` | an imzML pixel size stated without a unit was taken as micrometre (§8, imaging) | imzML |
| `imzml:pixel-size-area-to-length` | a single imzML pixel size tested as an area (`√value × count = extent`) and was written as its square root, in the unit the area is the square of (micrometre when no length unit is stated) | imzML |
| `imzml:pixel-size-dropped` | an imzML pixel size tested as neither area nor length, was not numeric, or (x and y both stated) was zero or negative on an axis, and was not written | imzML |
| `imzml:unit-accession-replaced-by-name` | a pixel-size or extent param's unit accession and unit name disagreed and the unit written is not the stated accession (mzdata takes the unit name when it names a unit mzdata knows, whatever the attribute order) | imzML |
| `imzml:one-way-as-flyback` | the obsolete scan term "one way" (`IMS:1000411`) was written as its stated replacement, flyback (`IMS:1000413`) | imzML |
| `imzml:obsolete-integer-type-as-psi-ms` | a binary array's data type was declared with the imaging vocabulary's obsolete `IMS:1000141` ("32-bit integer") or `IMS:1000142` ("64-bit integer") and was read as `MS:1000519` / `MS:1000522`, the PSI-MS terms that replaced them; the values are the ones the `.ibd` holds | imzML |
| `imzml:ibd-checksum-mismatch` | the `.ibd` does not hash to a checksum the header states (`IMS:1000090/91/92`). The stated value is kept in `file_description`; `metadata.imaging.provenance.ibd_checksum_found` holds the hash found, and the `.ibd`'s `source_files` entry its SHA-1 (§8, imaging) | imzML |
| `imzml:ibd-uuid-mismatch` | the `.ibd` does not begin with the UUID the header states (`IMS:1000080`): the two files are not the pair the imzML describes, and the conversion was forced past the refusal with `--force` (without it nothing is written, §8, imaging). The stated value is kept in `file_description`; `metadata.imaging.provenance.ibd_uuid` is `mismatch` and `ibd_uuid_found` holds the UUID the `.ibd` begins with | imzML (`--force`) |
| `imzml:ms-level-0-as-1` | at least one spectrum typed `MS1 spectrum` (`MS:1000579`) stated `ms level` 0 (or none: mzdata reads both as 0) and was written with `ms_level` 1 — the type proves the level, and readers' MS1 filters and the summed TIC/BPC pair then apply to it. The run's warning gives the count (§8, imaging) | imzML (the direct mzML export writes level 1 too, with the warning as its declaration) |
| `bruker:pixel-size-from-beam-scan-size` | a Bruker MALDI run's pixel size (and the max dimension derived from it) is the frames' `BeamScanSizeX/Y` (`MaldiFrameLaserInfo`, through `MaldiFrameInfo.LaserInfo`), not the FlexImaging raster step: no `<stem>.mis` beside the `.d`, or one its regions do not map onto | Bruker TSF / TDF with `MaldiFrameInfo` |
| `waters:laser-position-fitted-to-grid` | a Waters imaging run's pixel positions are grid indices fitted to the laser aim positions (mm) MassLynx states per scan; the fit is in the `waters_imaging` block | native Waters `.raw` with laser positions |
| `waters:off-grid-position-dropped` | at most 1 % of a Waters imaging run's positioned scans lie off the fitted grid, or far outside the raster at one position (a scan taken with the stage parked off it), and were written without a position; the count is `off_grid_scans_dropped` in `waters_imaging` | native Waters `.raw` with laser positions |
| `bruker:raster-index-shifted-to-base-1` | a Bruker MALDI run's positions are `XIndexPos/YIndexPos − origin + 1`, the run's smallest index becoming 1; `origin` is in the `bruker_maldi` block | Bruker TSF / TDF with `MaldiFrameInfo` |
| `imaging:pixel-count-from-positions` | the input states positions but no pixel counts; `IMS:1000042/43` were written as the largest positions | imzML, mzML with `IMS:1000050/51` |
| `imaging:pixel-count-raised-to-positions` | a pixel count the input states does not bound the written positions (a position lies beyond it, it is not a whole number, or only the other axis states one); that `IMS:1000042/43` was set to the largest position on its axis (`pixel_count_source: observed_max`) | imzML, mzML with `IMS:1000050/51` |
| `imaging:invalid-position-dropped` | at least one scan stated a position that is not a pixel index (x or y missing, not an integer, below 1 or above 2³² − 1); its position params (z included) were removed and every position column is null for it. The run's warning gives the count | imzML, mzML with `IMS:1000050/51` |
| `imaging:invalid-position-z-dropped` | at least one scan stated pixel-index x and y but a z that is not one (not an integer, below 1 or above 2³² − 1); only its z param was removed, so `position_z` is null for it and x and y are kept. The run's warning gives the count | imzML, mzML with `IMS:1000052` |
| `thermo:target-only-isolation-window` | at least one precursor isolation window had no width its scan states (no positive `MS<n> Isolation Width` trailer, or an empty or inverted window) and was written target-only; thermorawfilereader computes a quarter-width or inverted window for them. The run's warning gives the count | Thermo `.raw` (`--to mzml` applies the same rule, with no list to declare it in) |
| `thermo:invalid-precursor-reference-dropped` | at least one precursor named, as the spectrum it was selected from, the spectrum itself or a spectrum whose MS level is not below its own, and the reference was cleared (`precursor_id` and `precursor_index` null). mzdata names the reader library's parent index on every scan, and the library reports index 0 where a scan has no parent: on a run without MS1 (SRM) every spectrum named scan 1, scan 1 included. The run's warning gives the count | Thermo `.raw` (`--to mzml` applies the same rule, with no list to declare it in) |
| `bruker:trace-unit-rescale` | a HyStar device trace recorded in a unit mzdata cannot state (bar, mbar, kPa, MPa, mL/min, nL/min, mAU, kV, mV, µs, h, Å) was multiplied by the exact factor into one it can, as 64-bit floats | Bruker `.d` with `chromatography-data.sqlite` |
| `bruker:trace-sort-dedup` | a HyStar device trace was stored out of time order or with repeated samples (overlapping chunks), and was written in time order with each exact (time, value) repeat once | Bruker `.d` with `chromatography-data.sqlite` |
| `mzml:dangling-reference-dropped` | a reference the source states between its own lists names no entry of them, and was dropped: a scan's `instrumentConfigurationRef` (its `instrument_configuration_id` is null), a processing method's or an instrument configuration's `softwareRef` (empty), the run's `defaultInstrumentConfigurationRef` or `defaultSourceFileRef` or the spectrum list's `defaultDataProcessingRef` (each then names the first entry of its list, as for a source that states none — the spec requires all three), a spectrum's `sourceFileRef` (no `sourceFileRef` parameter is written). A self-closing `<software/>`, `<sourceFile/>` or `<instrumentConfiguration/>`, which mzdata skips, is read back from the header first and put back where the source states it, so a reference to it resolves and is kept. The run's warning counts each kind and names the ids as the source states them | mzML, imzML (the mzML exports apply the same rule, §4.1, with no list to declare it in) |

**The list in `data_processing_method_list`.** The same entries are mirrored into the conversion's
own processing method (`mzpeak_convert_conversion`, software `mzpeak-convert`), so a reader of the
processing list alone learns what the conversion applied: each entry as a `transformation`
userParam carrying it verbatim, and, when `zero-run-mask`, `shimadzu:span-trim` or
`agilent:drop-zero-samples` is among them, PSI-MS `MS:1003901` `zero intensity point trimming` first
— the one kind PSI-MS has a term for. A conversion with no entry leaves the method as before.
Archives written through 0.16.0 hold the list only in `transformations`. Ids are unique in the two
lists: a source this tool wrote brings `mzpeak-convert` and, when it was exported from an archive,
that archive's `mzpeak_convert_conversion` with it, so the same version's software entry is reused
(another version's keeps its id; this one's is `mzpeak-convert_2`) and the new conversion is
`mzpeak_convert_conversion_2`. Through 0.17.0-rc.1 both were written a second time under the same id.

**The `fidelity` index key.** `transformations` names what changed; `metadata.fidelity` says by how
much. Every mzPeak lane writes it at close, from the two signal facets as they sit in the archive
(`src/fidelity.rs`):

```json
"fidelity": {
  "spectra_data": {
    "layout": "chunked",
    "source_points": 75591, "stored_points": 36856,
    "source_types": {"mz": ["float32"], "intensity": ["float32"]},
    "stored_types": {"mz": "float64", "intensity": "float32"}
  },
  "mz_error": [
    {"encoding": "numpress-linear", "facet": "spectra_data", "chunks": 126, "min_fixed_point": 2783817.0,
     "max_abs_error": 1.7961e-07, "max_rel_error_ppm": 0.000233, "basis": "bound"}
  ]
}
```

- One entry per signal facet that holds or was handed points (`spectra_data`, `spectra_peaks`).
  `stored_points` is the facet's footer count. `stored_types` says what holds the values: the
  numeric type of the intensity column and of the m/z column (numpress chunks decode to that type,
  with the error `mz_error` states), or `grid:<index type>` for m/z on a chunked facet whose rows
  are grid rows (`MS:1003826`: the values column is empty, and each m/z is computed as a 64-bit
  value from an integer index and the row's model). Every default timsTOF archive reads
  `"mz": "grid:uint32"`, as do the fitted-lattice and `--tof-grid` facets; a facet that mixes the
  two kinds of row (a `--tof-grid` run keeps an off-grid spectrum as 64-bit values) reads
  `float64+grid:uint32`. `source_points`
  is what the reader handed the writer, counted by the lane (the mzML/imzML, `--tof-grid` and native
  vendor-reader lanes; the ims-compact, `--agilent-grid` and SCIEX grid lanes state the stored side
  only): the difference is what the zero-run mask left out. On a `--no-ims-compact` timsTOF archive
  it is the file's own count, the sum of `Frames.NumPeaks` over the frames read, and the
  difference is also what mzdata's reader did not hand over (`bruker:out-of-window-points-dropped`;
  a frame `MZPC_MAX_SPECTRA` cut through counts with the points it was read for). `source_types`
  are the binary data
  types the file declares, on the mzML and imzML lanes only (a vendor library's array types are its
  own choice), as a list because a file may mix them. `intensity_values_rounded`, present when it
  is above zero, counts the intensities stored as the float32 nearest to a source value no float32
  holds (`intensity-f32-rounding`): a centroid spectrum's intensities reach the peak facet as
  float32 on the default lanes whatever the file declares, and an array wider than a float32
  column is cast into it; `--lossless` avoids both. The stored type alone does not show it: where
  the peak facet's column is a float64 (a sampled centroid spectrum with a third per-peak array and
  64-bit intensities; through 0.17.0-rc.2 also a file whose sampled profile spectra carried 64-bit
  intensities, which typed the peak column they never reach), the spectra that went through the
  peak set hold their rounded values all the same. `intensity_values_narrowed` counts the
  intensities a column of another type does not hold (`intensity-type-narrowing`: floats cut to
  integers in an integer column, integers and floats clamped to an int32 one's range). Both are
  counted as the spectra are written, by the way each reaches the column and for the type the
  column has, and equal the number of stored intensities that differ from the file's (tested on
  files that mix types, in both layouts). The column's type holds every type the sampled spectra
  show (§8), so the counts arise from a type switch after them. A stored type narrower than a
  source type with neither count beside it changed no value: every value of the wider arrays is
  one the column holds. (An archive the chunked layout wrote through 0.17.0-rc.2 may instead hold
  an integer array of another type than the column's in the spectrum's `auxiliary_arrays`, at its
  own type, with the facet row empty; one the point layout wrote through rc.2 holds a null for
  each value out of an int32 column's range, the count beside it, and a spectrum whose intensities
  are all null reads back without an intensity array.)
- `mz_error` lists every m/z encoding in the archive that can move a value; an empty list means
  none is present. `max_abs_error` is in m/z units, `max_rel_error_ppm` in ppm, and `basis` says
  what kind of number it is:
  - `bound` (`numpress-linear`, `delta`): not a measurement but a bound computed from the stored
    chunks. For numpress it is, per chunk, `0.5 / fixed point` plus four units in the last place of
    the chunk's largest m/z, the fixed point read from the encoded chunk's first eight bytes; the
    relative figure divides each chunk's bound by its smallest m/z. `0.5 / fixed point` alone is exceeded by about 1e-14 Da
    through f64 rounding, so a strict check against it fails; the recorded bound holds for every
    value (tested by decoding, `fidelity::tests`) and is reached to within 1 % on real data.
    A `delta` entry is a bound as well. It appears only when 64-bit m/z are stored in delta
    chunks whose last m/z is more than twice the first, where `b + (a − b)` can round; it counts
    those chunks (`chunks_not_exact_by_construction`) and gives the largest m/z among them
    (`largest_mz_at_risk`). A decoded value stays within one unit in the last place of its source
    value through the whole chunk, although an error is carried from one value into the next (the
    proof is in `src/fidelity.rs`, and the tests decode against it): `max_abs_error` is that unit
    at the largest m/z at risk (4.5e-13 Da below m/z 4096), `max_rel_error_ppm` is 2.22e-10, the
    most one such unit is of its value. A delta chunk within a factor of two, and any delta chunk
    of 32-bit m/z values, is exact. The entry and `delta-ulp` in `transformations` are written
    together, from the same count of chunks.
    `bruker:mz-calibrant-omitted` carries the bound the lane states in
    `ims_calibration.max_error_ppm` (the largest correction of the vendor's calibrant polynomial,
    in ppm of the stored m/z, rounded up to six digits) and, as `max_abs_error`, that share of the
    largest m/z stored (SBA415: 0.655716 ppm). The lane takes that maximum over 2,001 evenly
    spaced m/z across the calibrant range: the largest of the sampled corrections of a
    low-degree polynomial, not a proven supremum.
  - `tolerance` (`grid-fit:<Da>Da`, `tof-grid:<ppm>ppm`): the bound every fitted value was
    accepted within, absolute for the fitted lattice and relative for the TOF grid, and the other
    figure derived from it over the m/z range of the grid rows stored (the absolute tolerance over
    their smallest m/z, the relative one times their largest). A `tof-grid` entry also states what
    the accepted grid left, measured while the archive was written, every gridded point's stored
    m/z against the m/z the reader handed over: `observed_max_rel_error_ppm` and, on the mzML
    lanes, `observed_max_abs_error` (the native SCIEX lane measures the relative error only). The
    tolerance is what a fit had to stay within; on a flight-time lattice the grid stays far inside
    it (a generated sqrt-lattice profile run: 6.7e-9 ppm under a 5 ppm tolerance).
  - `not measured` (the Bruker chord and Shimadzu coarse-m/z entries, a
    `bruker:mz-calibrant-omitted` entry of a run whose lane states no bound, and a `delta` entry
    whose chunks start at or below m/z 0): no figure.
- The `.mzpeak` → `.mzpeak` filter (§4.2) carries the block unchanged while every spectrum is
  kept, and leaves it out when `--rt` or `--ms-level` removed spectra (its counts would describe
  the source archive); the `filter` block then lists it under `dropped_index_blocks`.

Beside `transformations`, other index keys let a reader audit an archive offline: `metadata.conversion_route` says which timsTOF route built an ims-compact archive (`ims-compact` read by
`timsrust` or `timsdata`, or `mzdata-fallback` with the `reason` — the native reader could not
decompress a frame; the recorded command line is the same on both routes). Only those lanes write
it: a `--no-ims-compact` or f64 `--bruker-sdk` archive has no `conversion_route`, its route being
the one its command line names. `metadata.partial` marks
a run truncated by `MZPC_MAX_SPECTRA` (§10) or an `--agilent-grid` run whose `MSProfile.bin` ends
before its scan records (`cause` says which), and
`ims_calibration.chord_source` (`global_metadata` on the native timsrust lane, `sdk_tims_index_to_mz`
under `--bruker-sdk`) says which of the two (a, b) chords — measured 4.28 ppm apart on 2485.d — an
ims-compact archive holds.

**Verbatim vendor side-files (preserved, not interpreted).** For every vendor directory input
(Bruker `.d` of any kind, Agilent `.d`, Waters `.raw`), the side-files that describe the run
(methods, calibration, acquisition databases, sample and device tables, …) are **embedded by
default** under `vendor/` in the archive — gzip-compressed where they compress and declared
`proprietary` in the index — so nothing the converter does not yet model is lost; the
`vendor_files` manifest records every embed and every drop. `--via-msconvert` embeds none: its
source is the mzML msconvert wrote. What a default archive leaves out, each file by its name in any
letter case, wherever it sits in the directory:

- **The raw signal files of BAF, Agilent MassHunter and Waters MassLynx directories**, which are
  nearly all of such a directory and several times its archive (FM_1-1: `analysis.baf` 714 MB beside
  a 109 MB archive; Capan2: 1.1 GB of `_FUNC*.DAT` and `_func*.cdt` beside 531 MB): `analysis.baf`,
  `analysis.baf_idx`, `analysis.baf_xtr`, DataAnalysis's cached `*.ami` views and the FTMS transients
  `ser` and `fid`; `MSProfile.bin`, `MSPeak.bin` and `IMSFrame.bin`; `_FUNC*.DAT`, `_FUNC*.IDX` and the
  compressed ion-mobility data `_func*.cdt` and `_func*.ind`. What those files hold beyond the
  archive is then in no default archive: the BAF profile unless `--representation profile`, the
  MassHunter representation a lane did not read (the MHDAC host reads profile, else peaks;
  `--agilent-grid` reads profile), and the Waters functions not written as spectra
  (`waters:drop-functions`). Kept: the Agilent scan records `MSScan.bin` (per-scan metadata no lane
  decodes yet, the MSn precursor fields among it; 109 MB on a 1.2 GB-profile run) and
  `MSMassCal.bin`, the `*.cg`/`*.cd` device traces, DataAnalysis's `*.mcf` result containers, and the
  Waters `_FUNC*.STS` scan statistics, `_CHRO*` analog traces and `_mob/` projections.
- **The timsTOF `*_bin`, on the ims-compact lanes only**, which store its exact integer signal
  themselves. The f64 TDF/TSF lanes (TSF, `--no-ims-compact`, `--bruker-sdk`) keep it, as through
  0.11.5: beside the embedded `analysis.tdf` or `analysis.tsf` it is the exact copy of a signal those
  lanes store as calibrated f64 m/z, in a format open readers decode (timsrust a TDF, this converter a
  TSF), and at 70 % of a TSF archive it is the price of that copy.
- **baf2sql's `analysis.sqlite`**, the cache the BAF reader itself creates inside the `.d`.
- **macOS AppleDouble companions `._*`**, the Finder metadata that copying a directory from a Mac to
  NTFS, exFAT or SMB leaves beside every file (`--aux '._*=embed'` keeps them).

Through 0.11.5 only TDF/TSF directories, `--agilent-grid` and ims-compact embedded anything, and
`--agilent-grid` embedded `MSProfile.bin` and `MSPeak.bin` beside the grid it stores. For Thermo
`.raw`, the scan trailers (FAIMS CV, injection time, charge, …) and status log are captured verbatim
into dedicated `vendor_scan_trailers` (tall + wide) and `vendor_status_log` facets.

**Including / excluding.** The embedding is policy-driven (preserve-by-default):

- `--no-vendor` (or `no_vendor: true`) — embed nothing.
- `--aux 'glob=drop'` / `--aux 'glob=embed'` — per-glob rule, highest precedence, repeatable. A glob
  matches, in any letter case, a file's name or its path inside the directory with `/` between the
  parts: `MSProfile.bin` and `AcqData/MSProfile.bin` both name the Agilent profile file, and `*`
  matches across a `/` too. A single-file input (mzML, imzML, Thermo `.raw`, `.wiff`, `.lcd`) has no
  side-files, so the rules change nothing there and the converter says so. The same rules can be
  given as the `aux:` list in the config file (§5). For example, drop the TSF bulk binary but keep
  the method: `--aux '*.tsf_bin=drop' --aux '*.method=embed'`; keep a BAF run's raw signal:
  `--aux 'analysis.baf*=embed'`.

## 9. Compression, layout & ims-compact

- **Layout** — `chunked` (default) groups m/z into chunks (`--chunk-size`, Th) and
  encodes each with numpress-linear (lossy, compact) or, with `--no-numpress`,
  delta (exact except as §4 states). `point` writes one row per (m/z, intensity), each
  at the numeric type of its column, with no encoding that can move a value.
- **Zero runs and bit-exact archives** — three levels, from smallest to exact:
  the default masks profile zero runs and stores m/z with numpress-linear (bounded, the bound in
  `fidelity`, §8); `--keep-zero-runs` stores every point and keeps numpress, which is what gives
  continuous-mode imaging data (one m/z axis for all pixels) one decoded axis — and what a
  continuous-mode imzML gets without the flag since 0.17.0 (§8, imaging); `--lossless` is
  bit-exact or fails. It selects the point layout with zero runs kept and no numpress, lattice or
  TOF grid, writes a centroid spectrum's arrays at the binary types the file declares (the
  default lanes store its intensities as float32), and after writing checks the archive: no
  signal transformation declared (the set is `SIGNAL_TRANSFORMATIONS` in `src/fidelity.rs`;
  entries about metadata, chromatograms or precursor windows, such as
  `mzml:dangling-reference-dropped` or `chromatogram-time-to-minutes`, do not count), both signal
  facets in the point layout with as many points as were read, no m/z encoding with an error, and
  no column narrower than its source type (a 32-bit value in a 64-bit column passes and shows in
  `fidelity`). When the check fails, so does the conversion, and nothing is written; a spectrum
  whose m/z are out of order fails it, since storing it means sorting it. mzML and imzML only.
  The check covers the mass spectra's m/z and intensity; chromatograms (whose times are still
  written in minutes) and wavelength spectra are stored as on the default lanes. Measured on the
  HR2MSI mouse urinary bladder imzML (34,840 profile spectra, 67,916,471 points, 64-bit m/z;
  815 MB `.ibd`): default 173.8 MB with 40,559,444 points stored, `--keep-zero-runs` 193.6 MB
  (+11 %), `--lossless` 409.8 MB (2.4×, every m/z and intensity equal to the `.ibd`); these are
  sizes without an optical image, and the 1.6 MB TIFF the corpus keeps beside this imzML is
  embedded on top when it is there. Where m/z are 32-bit values the exact archive is the smaller
  one: `Example_Continuous` was 263 kB masked (0.17.0-rc.2), is 243 kB by default since its zero
  runs are kept (continuous mode, §8), 238 kB with `--no-numpress` and 230 kB with `--lossless`.
- **zstd** — applied inside Parquet, `--zstd-level` 1–22 (default 3; the timsTOF **ims-compact**
  lanes default to **5**, the measured byte-plane plateau — an explicit `--zstd-level` applies to
  both).
- **Fixed-point m/z lattice → fitted linear grid** *(automatic; `--no-mz-lattice` to disable)* — some
  vendors hand over m/z that are really integers over a power of ten: Shimadzu `MassHigh` at 1e-9 Da,
  its coarse `Mass` field at 1e-4 Da, and the LabSolutions **mzML export** of the same acquisition.
  The converter samples the CENTROID m/z of six spectra spread across the run and, if every one of
  them lands on such a lattice, stores the peaks facet on the reference implementation's **fitted
  linear grid**: 50-Th chunk-grid rows (`chunk_encoding` `MS:1003826`), each spectrum under its own
  `MS:1003824` model `[intercept, slope, scale]` fitted over 2³² slots of the spectrum's padded
  range and accepted only when every value rebuilds within 1e-6 Da (≤ 3e-7 Da on a 1,900-Th
  spectrum) — declared as `grid-fit:1e-6Da`. That is upstream's own `--peak-encoding grid`, and on
  such data it is far smaller than delta chunking: on the 4.5 GB LabSolutions `DIA_Hela_20ng` mzML
  (279.7 M centroids) the m/z bytes go 1,897 MB (delta) → ~0.93 GB. Only the **peaks** facet
  is affected — profile arrays keep the chunked layout and the `--no-numpress` / `--chunk-size` /
  `--layout` choices exactly as before, so a profile-only input converts unchanged. Under
  `--layout point` the grid is not applied (nor, on the native Shimadzu lane, the profile sqrt grid):
  both spectrum facets stay in the point layout with exact f64 m/z, one layout family per entity.
  From 0.9.7 to 0.13 this was an exact Int64 point lattice of the converter's own (`point.tof_index`
  = `round(m/z·scale)`, an `mz_calibration` index block, `m/z = tof_index / scale`); that layout is
  the one representation the reference implementation's chunk grid cannot hold (2³² steps of
  1e-9 Da is 4.29 Th per chunk) and was retired in 0.14 for upstream's encoding. Archives of that
  generation still read. `--no-mz-lattice` (config `no_mz_lattice: true`, or `MZPC_NO_MZ_LATTICE=1`
  in the environment) keeps the exact f64 m/z on every lane, at the delta-chunk size.
- **ims-compact** — for Bruker timsTOF (**TDF**) this is the **default**: the
  native integer `tof` is stored bit-exact (Int32 + `ims_calibration`) instead of
  f64 m/z, roughly halving the m/z bytes with an exact grid. Disable with
  `--no-ims-compact` to write standard f64 m/z. m/z is reconstructed by readers as
  `m/z = (a + b·tof)²` — the chord, marked `"exact": false`; the vendor's exact model sits
  beside it in `vendor_mz_calibration` (§8).
- **TOF-grid archives (`--tof-grid`, native SCIEX `.wiff`, `--agilent-grid`) — chunk-grid rows
  (since 0.14).** Both `spectra_data` and `spectra_peaks` are chunk facets, one chunk per spectrum
  (`chunk_encoding` `MS:1003826`, real m/z bounds, `mz_chunk_values` null): a gridded spectrum's row
  carries `mz_grid {grid_type: MS:1003825, parameters: [c0, c1, 1], indices}` — the integer bins,
  `[first, deltas…]`, decoded as `m/z = (c0 + c1·k)² / 1` exactly as the reference implementation's
  `SquareRootLinearGrid` does — and a spectrum that did not fit is a raw `MS:1000576` row holding its
  exact f64 m/z. A spectrum goes to the facet its declared representation selects and is gridded or
  not independently of that, so `spectrum_representation` / `number_of_data_points` /
  `number_of_peaks` mean what the source said (0.10.1, review M6). Under `--tof-grid off` (SCIEX)
  nothing is gridded and both facets keep the requested chunked layout. Through 0.13 the same
  archives held an Int32 `tof_index` point column with `mzpeak:transform_params` /
  per-spectrum `tof_c0`/`tof_c1` columns and a `tof_calibration` block; they still read. The grid
  values in a row are the row's model at its indices (a run-wide fit whose anchor `c0²` lies above a
  spectrum's lowest m/z is re-anchored for that spectrum, so no index is negative), which keeps
  the bounds and the decoded points bit-identical. The raw rows' m/z (`mz_chunk_values`, null on
  every grid row) are byte-stream-split with the dictionary off, like the chunk bounds: they are
  whole spectra of exact 64-bit values, nearly all distinct, and a dictionary held them over
  again in every row group (a 30 s slice of the PXD011326 TripleTOF 6600 SWATH run through
  `--tof-grid`, 2.35 million off-lattice m/z in 160 of 1,079 spectra: the column −29 %, the facet
  −15 %, values identical). A chunk facet without a grid column keeps the dictionary on its
  delta values.
- **ims-compact layout — the reference implementation's chunk grid** (since 0.13.0; written
  natively since 0.14, `ims_calibration.tof_encoding = "grid"`) — the peaks facet holds each frame
  as `MS:1003826` chunk rows: real m/z bounds (`mz_chunk_start`/`mz_chunk_end`, so an m/z window
  prunes rows without any model), `mz_chunk_values` null, and each dimension's coordinates as
  integer indices in a struct column that carries the model with them: `mz_grid {grid_type,
  parameters, indices}` holds `[first TOF bin, deltas…]` (cumulative sum to reconstruct; the first
  point IS in the list) with the frame's `MzCalibration` row as
  `[C0, 1e6/√(C1·cf), C2/cf, C3, C4, DigitizerTimebase, DigitizerDelay]`
  (`cf = 1 + (dC1·(T1 − Frames.T1) + dC2·(T2 − Frames.T2))/1e6`), and
  `mean_inverse_reduced_ion_mobility_grid` holds the TIMS scan numbers with the `TimsCalibration` row
  as `[C6, C7, offset, slope]`. Decoding: `t = bin·timebase + delay`, solve
  `t = C0 + β·u + C2·u² (+ C3·u³)` for `u`, `m/z = u² − C4`; `1/K0 = 1/(C6 + C7/(offset + slope·scan))` —
  exactly as `mzdata::io::tdf::MzCalibrationModel2::convert_f64` / `TimsCalibrationModel2::convert`
  evaluate them, which is how the bounds were computed, so a bound and its decoded point agree bit
  for bit. A diaPASEF or ddaPASEF window's 1/K0 band on its selected ion (`MZP:1000006/7`) and the
  selected ion's own 1/K0 are evaluated the same way, by the native reader and by `--bruker-sdk`
  (which asks the vendor library for a 1/K0 only where it stores the library's values: without a
  ModelType-2 row, and on its f64 lane): a window's limits are the very values its
  boundary scans' points decode to, so cutting a frame by its stated bands assigns every point to
  its window, as the `--no-ims-compact` archive and the `.d` → mzML export state them. An archive
  written through 0.16.0 holds limits evaluated in the vendor library's order of operations, 1 to
  4 units in the last place off at most scans (2485.d: 5,327 points in 2,945 of 15,977 windows
  above their upper limit); rebuild it to export per-window spectra by the stated bands. Both
  columns are array-index entries of `buffer_format: chunk_transform`,
  `transform: MS:1003826`, with the DECODED type. Every frame is exact (SDK-verified to 1e-9 ppm,
  `C2`/`C4` rows included). Points are sorted by TOF within a frame (declared `sort-by-mz`; mobility
  is stored per point, so nothing is lost); intensity is the native count as Int32 (byte-plane,
  `MZPC_BYTE_PLANE_INTENSITY=0` for Float32). Under `--no-tims-recalibration` 1/K0 is timsrust's
  linear approximation, which no grid expresses: it is stored as plain per-point values and
  `ims_calibration.ion_mobility_grid.column` is null.
  - **Chunk width.** `--ims-chunked` *(default)*: 50-Th chunks (`--chunk-size`), the m/z-prunable
    form. `--no-ims-chunked`: one chunk per frame — whole-frame access in one row, no pruning within
    a frame. Row groups end at 8192 chunks (`MZPC_ROW_GROUP_ROWS`), the measured random-access
    sweet spot (7.5 vs 13.4 ms/frame at +1 % size), or at 48 MiB (`MZPC_ROW_GROUP_MB`), whichever
    comes first — on a dense run 8192 chunks were 270–460 MiB.
  - **Encodings.** Index lists and intensity are byte-stream-split with the dictionary off (measured
    −9.6 % against the reference implementation's dictionary default on 2485; its DELTA intent on
    index lists would be +21 %), `spectrum_index` delta-packed, and the chunk bounds byte-stream-split
    with the dictionary off, as on every chunk facet: nearly every bound is distinct, so a dictionary
    only held the values again, once per row group (on 2485 the bounds are 30 % and the facet 0.6 %
    smaller than with it). Size on 2485.d against the vendor `analysis.tdf_bin`: about parity (the
    0.13.0 corpus Bruker set measured 1.097× with this layout).
  - **History.** 0.12.x wrote a TOF layout (integer TOF bounds, `tof_chunk_values` deltas,
    per-frame `tof_c0`/`tof_c1`; a flat point table of absolute bins under `--no-ims-chunked`), and
    0.13.0 rewrote its chunked facet into the grid in a second pass (`--ims-grid`, `--no-ims-grid`,
    `--grid-encoding`). All of that is gone in 0.14; archives written by 0.12.x and 0.13.0 still
    read. (A per-scan-delta variant up to v0.7.2 was removed in v0.7.3: no reader decoded it
    correctly. Reconvert those, and never use one as a size baseline.) The models' placeholder
    terms `MS:9999001/2` are the reference implementation's until PSI-MS assigns them.
- **Shimadzu `.lcd` (two grids, one per facet)** — the native lane stores each facet on the
  reference implementation's chunk grid:
  - **Profile → per-spectrum sqrt grid** in `spectra_data`: one chunk-grid row per spectrum under
    its own `MS:1003825` model `[c0, c1, 1]` (`m/z = (c0 + c1·k)²`, `c1` constant across the run),
    the vendor's own TOF lattice, verified on every point to ≤ 1e-9 Da before a spectrum is
    gridded. A spectrum that does not fit (LabSolutions clamps the first/last sample of some MS2
    scans to the scan-window bound) is a raw `MS:1000576` row with its f64 m/z.
  - **Centroids → fitted linear grid** in `spectra_peaks` (50-Th chunks, `MS:1003824` per
    spectrum, within 1e-6 Da — `grid-fit:1e-6Da`; see the lattice bullet above). The coarse
    `Mass` field (`MZPC_SHIMADZU_COARSE_MZ=1`) takes the same route and is declared as
    `shimadzu:coarse-mz`.
  - Archives written by 0.9–0.13 carry the profile as an Int32 `tof_index` point column with
    per-spectrum `tof_c0`/`tof_c1` and a `tof_calibration` block, and the centroids as an exact
    Int64 lattice (`mz_calibration`, `m/z = tof_index / 1e9`); both still read.
  - **Size** (`MassHigh` f64 → grid + lattice, measured on the 0.9.5 point layout; the chunk grid is
    −7.5 … +6 % on the profile facet and −6 … −11 % on the dense centroid facets against it):
    29.0 → **23.9 MB**, DIA_Hela_20ng 2.19 GB → **1.31 GB** (839 MB with the coarse 1e-4 `Mass`,
    which is 100× less precise). Peak for peak identical to the f64 archives on all four
    reference files, with zero f64 fallbacks.

## 10. Exit codes & environment

| Code | Meaning |
|---|---|
| 0 | success |
| 1 | generic error |
| 3 | unsupported input/format on this platform |

| Variable | Effect |
|---|---|
| `RUST_LOG` | log filter; an explicit `-v`/`-q` wins, `RUST_LOG` applies only when neither is given |
| `DOTNET_ROLL_FORWARD` | set automatically to `LatestMajor` if unset, **for Thermo `.raw` input only** (since 0.9.13 — set for every input it overrode the Shimadzu glue's own `rollForward: LatestMinor`, which on a .NET 9 host hoists the glue onto a runtime without the `BinaryFormatter` path it needs) |
| `MZDATA_IGNORE_UNKNOWN_INSTRUMENT` | set automatically to `ignore` if unset |
| `MSCONVERT_PATH` | `msconvert` location for `--via-msconvert` |
| `TIMSDATA_LIB_DIR` | Windows/Linux: where the Bruker libraries are, as a library file or an SDK root (`win64/`, `linux64/` or flat) — `timsdata.dll` / `libtimsdata.so` for `--bruker-sdk` (without it the loader's search path is tried) and `baf2sql_c.dll` / `libbaf2sql_c.so` for the BAF lane (without it the lane refuses) |

Every `MZPC_*` variable the converter — or the vendored `mzpeak_prototyping` writer it links, or
the Shimadzu glue it hosts — reads is listed below; `tests/docs_drift.rs` fails when a name quoted in
`src/`, `vendor/` or `glue/` is missing. They fall into three groups: **deployment** (where the vendor libraries and glue live — you will
set these on a Windows conversion host), **output-affecting** (they change what is written —
prefer the equivalent CLI flag where one exists, so the run is reproducible from its command line;
where an archive can tell, the row says which index key records it) and **performance /
diagnostic** (they tune or trace, and the dump levers replace the conversion).

**How a boolean lever is read (since 0.9.13).** Every on/off `MZPC_*` lever in `src/` goes through
one `env_flag()` in `src/main.rs`, `MZPC_WATERS_KEEP_COLLAPSED` included: **unset** → the built-in
default; set to the empty string,
`0`, `false` or `no` (any case) → **off**; anything else → **on**. So `MZPC_DUMP_IM_TABLE=` (empty)
is off, and `MZPC_BYTE_PLANE_INTENSITY=` (empty) is the same opt-out as `=0`. Before 0.9.13 each site
spelt its own rule: the two dump levers fired on mere presence and an empty
`MZPC_BYTE_PLANE_INTENSITY` silently switched the ims-compact intensity column to Float32. The two
levers read by the vendored writer (`MZPC_PARALLEL_ENCODE`, `MZPC_TIMING`), the two read by the
Shimadzu glue and `MZPC_WATERS_PROBE_QUAD`, a level rather than a switch, keep their own spellings,
noted in their rows. Numeric levers ignore a value
that does not parse (they fall back to the default) — except `MZPC_SHIMADZU_PROBE`, where a
non-numeric value is an error.

**Diagnostics never shadow a requested archive.** `MZPC_DUMP_IM_TABLE`, `MZPC_DUMP_AGILENT_PROFILE`
and `MZPC_SHIMADZU_PROBE` print to stdout and write no archive. Since 0.9.13 they **refuse to run
when `-o/--output` is given** (exit 1, naming the variable: *"…is a diagnostic that prints to stdout
and writes NO archive, but --output … was requested"*), instead of printing, exiting 0 and leaving
the requested output missing. Run them without `-o`.

**Deployment (§11).**

| Variable | Effect |
|---|---|
| `MZPC_PWIZ_DIR` | ProteoWizard install supplying the vendor DLLs at runtime (Agilent MHDAC, SciEX Clearcore2, Shimadzu LabSolutions.IO, Waters MassLynx). Both layouts are probed — `vendor_api/<Vendor>` and flat beside `msconvert.exe` (the 3.0.26175 installer is flat; the Agilent host is handed whichever directory holds `MassSpecDataReader.dll`). **Use a current ProteoWizard** (3.0.26151 / 3.0.26175 verified); see §11 for why an old one silently corrupts Shimadzu centroids |
| `MZPC_MASSLYNX_DIR` | Directory holding `MassLynxRaw.dll` (+ `cdt.dll`) for the Waters lane. Wins over `MZPC_PWIZ_DIR`, which is the fallback. The Waters lane has no .NET glue |
| `MZPC_AGILENT_GLUE` | Directory holding the built net48 `AgilentGlueHost.exe` (`glue/agilent/bin/Release/net48`); the converter spawns it once per `.d` and reads its `AGL2` output back (§11) |
| `MZPC_AGILENT_TMPDIR` | Where the Agilent host materialises a run before it is read (16 B/point — about 3 GB for a 240 MB Q-TOF `.d`; removed when the reader closes, and by the panic hook, but not after a Ctrl+C). Default `%TEMP%`; set it to a disk directory when `TEMP` points at a RAM disk (the box scripts do). A value that is not a directory is warned about, and `%TEMP%` is used |
| `MZPC_AGILENT_HOST_TIMEOUT` | Seconds the Agilent host may run before the converter kills it and removes its temp file (default `7200`; `0` = no deadline). Killing the converter itself does not end the host: stop `AgilentGlueHost.exe` as well |
| `MZPC_SCIEX_GLUE` | Directory holding the built `SciexGlue.dll` + runtimeconfig |
| `MZPC_SHIMADZU_GLUE` | Directory holding the built `ShimadzuGlue.dll` + runtimeconfig |

**Output-affecting.** These change the bytes that are written; where a CLI flag exists, use it
instead.

| Variable | Effect | Recorded in the archive? |
|---|---|---|
| `MZPC_NO_MZ_LATTICE=1` | Same as `--no-mz-lattice` (§9): keep exact f64 m/z for lattice centroid lists instead of the fitted linear grid, on every lane (`env_flag` spellings) | implicitly — `transformations` has no `grid-fit` entry and the peaks facet has no grid rows |
| `MZPC_KEEP_ZERO_RUNS=1` | Same as `--keep-zero-runs` (§4): the writer's zero-run mask off, every profile point stored (`env_flag` spellings). Not honoured by `--agilent-grid`, which warns | implicitly — `transformations` has no `zero-run-mask` entry and `fidelity` shows `stored_points` equal to `source_points` |
| `MZPC_SHIMADZU_COARSE_MZ=1` | Shimadzu glue: read the coarse 1e-4 `Mass` instead of `MassHigh` (§8). Read by the C# glue and compared to the literal `1` — only `=1` switches it | yes — `transformations` lists `shimadzu:coarse-mz` |
| `MZPC_WATERS_KEEP_COLLAPSED` | Native Waters lane: write MassLynx's collapsed retention-time functions (run-summed mobilograms) as spectra instead of leaving them out. On/off lever read through the common rule (empty, `0`, `false`, `no` are off) | yes — `collapsed_functions[].written` in `waters_functions` (and `waters_drift`), and `waters:drop-functions` when they were left out |
| `MZPC_BYTE_PLANE_INTENSITY=0` | Opt out of Int32 byte-plane intensity (on by default for timsTOF ims-compact) back to Float32 (`env_flag` spellings: empty, `0`, `false`, `no` all opt out) | yes — `ims_calibration.intensity_dtype` = `int32` \| `float32` (0.9.13) |
| `MZPC_ENCODING_PRESCAN=0` | Native Waters lane: skip the encoding pre-scan (§8) and write the fixed encodings — numpress m/z (or delta under `--no-numpress`), float32 byte-stream-split intensity, dictionary ion mobility (`env_flag` spellings) | yes — the archive then has no `encoding_prescan` block |
| `MZPC_TOF_GRID_PPM=<ppm>` | `--tof-grid` reconstruction tolerance (default 5.0). The lane is bounded-lossy and this number **is** the bound — raising it above the instrument's mass accuracy is not defensible. Logged as a warning when set | yes — `transformations` carries `tof-grid:<ppm>ppm`, and the `tof_calibration` block its `roundtrip_tolerance_ppm` |
| `MZPC_TOF_GRID_C1=<step>` | `--tof-grid`: force the sqrt-space step instead of inferring it (`c1 = quantum / (2·√mz_max)`) | the fitted `{c0,c1}` is stored; the fact that `c1` was forced is not |
| `MZPC_MAX_SPECTRA=<n>` | Stop after `n` spectra. **Deliberately truncating**: it also disables the "all source spectra written" completeness check, so the archive is a partial one that exits 0. Diagnostics only; the WARN stays. `--lossless` is refused while it is set | yes — every mzPeak lane that honours the cap writes `metadata.partial` = `{partial: true, max_spectra, source_declared, spectra_written, cause: "MZPC_MAX_SPECTRA"}` when the cap bit (0.9.13); a cap larger than the file writes no marker |

**Performance / diagnostic.** No effect on the values written (bytes only where noted); zero cost
when unset.

| Variable | Effect |
|---|---|
| `MZPC_BUFFER_SPECTRA=<n>` | Spectra buffered in RAM before the writer flushes a row group (default 256) on the standard f64 paths |
| `MZPC_DECODE_WINDOW=<n>` | Bounded reorder window for the parallel timsTOF decoder (default 8× rayon threads, capped at 128). Output order — and therefore the bytes — is unchanged |
| `MZPC_ROW_GROUP_ROWS=<n>` | Peak-facet parquet row-group size in rows (default 8192 chunks/group on chunked facets, parquet's 2^20 otherwise). Trades size against per-frame random access; a group also ends at `MZPC_ROW_GROUP_MB` |
| `MZPC_ROW_GROUP_MB=<MiB>` | Vendored writer (and the filter lane's rewrites): byte cap of a signal-facet row group — `spectra_data`, `spectra_peaks`, `chromatograms_data`, wavelength data — in MiB of Arrow buffers, fractions allowed (default 48, three quarters of the validator's 64 MiB `data_row_group_not_monolithic` threshold). A group ends at this or at its row cap, whichever comes first. A conversion writes a spectrum's rows as one batch, and the byte rule starts a new group rather than split a spectrum that fits one; the filter lane and the point-column prune rewrite pass on reader batches of 1024 rows, so their byte cuts can fall inside a spectrum, as the row cap's always could. Changes row-group boundaries, so the bytes — not the values — differ |
| `MZPC_ENCODE_THREADS=<n>` | Vendored writer: worker threads for the parallel peak-facet encode (default `available_parallelism()`; `RAYON_NUM_THREADS` is honoured as a fallback; `0` or a non-number is ignored). Output is byte-identical at any thread count |
| `MZPC_ENCODE_INFLIGHT_BYTES=<bytes>` | Vendored writer: byte budget for row groups in flight in that parallel encode (default `max(256 MB, threads × 48 MB)`); never changes the bytes written. A group is charged at most the budget's per-thread share, so groups larger than that share still encode one per worker instead of one at a time. Memory in flight is therefore at most `max(budget, threads × largest row group)` plus Arrow's spare capacity, and the largest group is bounded by `MZPC_ROW_GROUP_MB` (a single chunk larger than that is a group of its own): a budget below `threads × MZPC_ROW_GROUP_MB` no longer lowers memory (2485 at 8 MB: 1.76 GB peak RSS, as at the default budget; through 0.16, which encoded such groups one at a time, 784 MB in seven times the wall time). To bound memory on a small host, lower `MZPC_ENCODE_THREADS` or `MZPC_ROW_GROUP_MB` |
| `MZPC_PARALLEL_ENCODE=0` | Vendored writer: serial peak-facet encode instead of the parallel default (on for every unencrypted archive; encrypted facets are always serial). Output is byte-identical either way. Its own rule: empty or `0` = off, any other value (including `false`) = on |
| `MZPC_FLUSH_MEM_MB=<MB>` | Vendored writer: flush the in-RAM array buffers once they exceed this many MB (default 128), independent of spectrum or point counts. Changes row-group boundaries on the standard f64 facets, so the bytes — not the values — can differ |
| `MZPC_TIMING=1` | Log decode-vs-write busy times for the pipelined timsTOF path (`env_flag` in `src/`; the vendored encoder's own timing lines — its settings, and at the end its row groups and how many encode jobs ran at once on average and at most — use empty-or-`0` = off. That average is wall-clock occupancy: a job the OS has descheduled still counts, so on an oversubscribed host it overstates the parallelism, which process CPU time / wall time measures) |
| `MZPC_SHIMADZU_DEBUG=1` | Shimadzu glue: trace scan-count discovery on stderr (read by the C# glue: empty or `0` = off, anything else on) |
| `MZPC_SHIMADZU_PROBE=<n>` | Shimadzu `.lcd`: print the first `n` spectra as JSON lines and exit **without writing an archive**. Since 0.9.13 it is handled before lane selection (in `run`, `src/main.rs`), so it works without `-o` — it used to live inside the Shimadzu lane, which only runs with `-o`, and therefore always swallowed the requested archive; a value that is not a count (including empty) is an error rather than 10; with `-o` it refuses; on macOS/Linux, where the reader does not exist, it is an error rather than silently ignored |
| `MZPC_DUMP_IM_TABLE=1` | Bruker TDF: dump the scan→1/K0 table (timsrust, and the SDK where available) and exit without converting. Refuses with `-o` |
| `MZPC_DUMP_AGILENT_PROFILE=1` | Agilent: dump decoded profile spectra (sum, nnz, first/last `(k,v)`, max `v`) and exit without converting. Refuses with `-o` |
| `MZPC_WATERS_PROBE_QUAD=<level>` | Waters `.raw` (Windows): during the conversion, log at WARN what the MassLynx DLL states about quadrupole isolation for MSe and DDA functions — level 1 the info-reader exports, 2 the DDA processor's parameters, 3 adds its per-scan info for the first scans, 4 the MSe processor, 5 `getAcquisitionInfo`; one level per run. Its own rule: empty or `0` is off. The archive is written as without it |
| `MZPC_PRESCAN_REPLAY=<in.mzpeak>` | Test harness only (`encoding_prescan_replay`, an ignored test): the archive whose spectra are replayed through the Waters writer path with the encoding pre-scan on — a `--no-numpress` build, so the replayed values are the reader's exact ones. Never read by the converter binary |
| `MZPC_PRESCAN_OUT=<out.mzpeak>` | Test harness only: where `encoding_prescan_replay` writes the replayed archive. Never read by the converter binary |
| `MZPC_TDF_SDK_GOLDEN=<out.json>` | Bruker TDF, `--bruker-sdk` only (Windows/Linux): diagnostic dump of the SDK's `tims_index_to_mz` at up to 240 `(frame, tof)` points — frame 1, the last frame and 10 evenly spaced frames × 20 tof values over `0..DigitizerNumSamples−1` — as `{file, digitizer_num_samples, mz_calibration, points: [{frame, t1, t2, cal_id, tof, mz_sdk}]}`, the ground truth for the ModelType-1 model and the per-spectrum `tof_c0`/`tof_c1` (§8). An empty value is unset; a bad path or an SDK refusal is logged and never fails the conversion |

The harness under `tools/` reads its own `MZPC_*` names (`MZPC_PYTHON`, `MZPC_BOX_*`,
`MZPC_NO_S3_SOURCE`, `MZPC_FETCH_JOBS`, `MZPC_ALLOW_PARALLEL`, `MZPC_BENCH_MZML`); they never reach
the converter binary and are documented in the scripts themselves.

## 11. Native vendor-SDK readers

The Agilent (MHDAC), SciEX (Clearcore2), Shimadzu (LabSolutions.IO), Waters (MassLynxRaw) and
Bruker BAF (libbaf2sql_c) readers are **compiled in automatically** on the platforms where those
vendor libraries exist — Windows for all five, Linux also for Bruker BAF. There is **no build flag** and no
opt-in; macOS gets none (no vendor SDKs exist there). The Agilent (MHDAC) one is the odd one
out: MHDAC needs the .NET **Framework**, so it runs in a separate net48 process
(`AgilentGlueHost.exe`, spawned once per `.d`; the whole run is materialised into a temp file at
16 B/point first — about 3 GB for a 240 MB Q-TOF run — and removed when the reader closes).
It reads scan spectra; an MRM/SIM-only `.d` is refused with a pointer to `--via-msconvert`,
because MHDAC presents each dwell as a one-point "MS2 spectrum" while the data are the
transition chromatograms the msconvert lane writes.

They load the proprietary vendor DLLs at **runtime**, sourced from a ProteoWizard
install: point `$MZPC_PWIZ_DIR` at it, and for the .NET glues set `$MZPC_AGILENT_GLUE` /
`$MZPC_SCIEX_GLUE` / `$MZPC_SHIMADZU_GLUE` to the built C# glue dir (`dotnet build
glue/agilent/AgilentGlue.csproj`, likewise `glue/shimadzu/ShimadzuGlue.csproj`; the Shimadzu
DLL is loaded from `$MZPC_PWIZ_DIR` by reflection — see `glue/shimadzu/README.md`). With a glue
variable unset, a release uses the glue it ships under `glue\` beside the executable (§2).
Both ProteoWizard layouts work: the MHDAC/Clearcore2 assemblies may sit under
`vendor_api/Agilent` / `vendor_api/ABI` (the bundled builds) or flat beside `msconvert.exe`
(the standalone installer); the Agilent lane probes both, subdirectory first. Shimadzu's
`Shimadzu.LabSolutions.IO.IoModule.dll` is always flat. Waters needs no glue: `src/waters.rs` loads
`MassLynxRaw.dll` (with `cdt.dll`) from `$MZPC_MASSLYNX_DIR`, else `$MZPC_PWIZ_DIR`, and calls its C
exports directly.

**Which ProteoWizard: use a current one.** 3.0.26151 and 3.0.26175 are verified, and anything that ships
`Shimadzu.LabSolutions.IO.IoModule.dll` **5.0.0.0** is fine. Older trees ship **3.8.4.6016**,
which mispairs centroid intensities on profile-less Shimadzu `.lcd` files (§8). The known-stale
source is the **FLASHApp / OpenMS third-party bundle, which carries ProteoWizard 3.0.22187
(July 2022)** — if `$MZPC_PWIZ_DIR` points into that bundle, replace it with a current
ProteoWizard install rather than working around the symptom. To check on the conversion host:

```powershell
(Get-Item "$env:MZPC_PWIZ_DIR\Shimadzu.LabSolutions.IO.IoModule.dll").VersionInfo.FileVersion
```
Without the DLLs the reader reports a clear error. Where no native reader exists for
a format on the current platform (e.g. Agilent/SciEX on macOS or Linux), use
`--via-msconvert` — it needs no special build.

## 12. Dependencies

Pure Rust plus a small C# interop layer for Thermo/native vendor readers. Core
crates: `mzdata`, `mzpeaks`, `arrow`/`parquet`, `zip`, `timsrust`,
`rusqlite`(bundled SQLite)/`zstd`, `flate2`, `clap`, `serde`, `anyhow`. The
reference writer `mzpeak_prototyping` is vendored under `vendor/`. Each release attaches a
CycloneDX inventory of every resolved dependency, with its license and source
(`mzpeak-convert-<version>.cdx.json`, generated from `Cargo.lock` by `tools/gen_sbom.py`), and every
release archive carries [THIRD-PARTY-NOTICES.md](../THIRD-PARTY-NOTICES.md).

## 13. Troubleshooting

| Symptom | Fix |
|---|---|
| Thermo `.raw` fails to open | install a .NET 8+ runtime |
| `--via-msconvert` not found | install ProteoWizard or set `--msconvert-path`/`$MSCONVERT_PATH` |
| Agilent/SciEX exits with code 3 | no native reader for that format on this platform (macOS/Linux); use `--via-msconvert` |
| Agilent `.d`: `holds MRM/SIM dwell data only` | the native lane stores scan spectra; MRM/SIM dwells are transition chromatograms — use `--via-msconvert` (the box harness does this on its own) |
| Agilent `.d`: `is an Agilent IM-QTOF run` | the native lane cannot carry the drift dimension (that needs Agilent's MIDAC SDK, which this converter does not read) — use `--via-msconvert` (the box harness does this on its own) |
| Agilent `.d`: `output is the AGL1 format of an older AgilentGlueHost.exe` | rebuild `glue/agilent` (`dotnet build -c Release`) so the host and the converter agree |
| timsTOF `.d`: `lists ._analysis.tdf … before analysis.tdf` | the `.d` was copied from a Mac to NTFS, exFAT or SMB, which leaves an AppleDouble `._*` file (Finder metadata) beside every file, and the volume lists the companion before the file it is named after. The timsTOF reader (timsrust) opens the first file whose name ends in `analysis.tdf` / `analysis.tdf_bin`, so it would read the companion in place of the run (it used to fail with "file is not a database"). Remove the `._*` files from the `.d`, or convert with `--bruker-sdk` (Windows/Linux). On a Mac, a `.d` on such a volume gets `._analysis.tdf` back from macOS as soon as timsrust opens the database for writing; copy the `.d` to an APFS disk instead. The default lane, `--no-ims-compact`, `--to mzml` and inspection refuse such a `.d` before opening it, and name a companion that appears while they open it. A companion listed AFTER its file (a fresh copy onto exFAT lists them so) is never reached: that `.d` converts as it always did, with the warning `holds ._analysis.tdf … beside analysis.tdf …, listed after them` |
| Nothing was written | give `-o/--output`; without it the run only inspects |
| Output exists error | pass `--force` to overwrite |
| UV/PDA spectra missing after `--ms-level` | a wavelength spectrum has no MS level, so `--ms-level` leaves them out, of an archive and of an mzML export alike, and says so (§4.2) |
