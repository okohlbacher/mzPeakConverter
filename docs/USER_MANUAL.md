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
  index (`transformations`) — see §8 for the four transforms that are not bit-exact.
- **Without `--output`** — writes nothing; it just **inspects** the input and prints
  a report (format, spectrum count, chromatogram count).

Passing `-v` prints that same inspection report *and still performs the conversion*.

## 2. Installation & requirements

| Requirement | Notes |
|---|---|
| Rust ≥ 1.88 | edition 2024; install via <https://rustup.rs> |
| C toolchain | for the bundled native libs (SQLite is compiled from source) |
| .NET 8+ runtime | **only for Thermo `.raw`**; auto-rolls-forward to 9/10 |

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

The table is regenerated from `mzpeak-convert --help` of the shipped binary (29 options; the
wording is the help's own, shortened). `--help` is the long form; `-h` prints a one-line summary
per option.

| Option | Default | Description |
|---|---|---|
| `<INPUT>` | — | Input file or vendor directory (mzML / `.mzML.gz` / imzML, Bruker `.d`, Thermo `.raw`, …; positional, required) |
| `-o, --output <OUTPUT>` | *(none → inspect only)* | Output path. `.mzpeak` (default) or `.mzML` — the format is inferred from the extension (or forced with `--to`); `.mzML.gz` writes gzip-compressed mzML (§4.1). If omitted, **nothing is written** — the input is only inspected and a report (format, spectra, chromatograms) is printed |
| `-c, --config <CONFIG>` | — | Config file (YAML) setting defaults for any option below; explicit command-line flags win (§5) |
| `--layout <chunked\|point>` | `chunked` | Signal layout: `chunked` m/z layout (numpress-linear or delta); `point` — flat point layout, one row per m/z–intensity pair (§9) |
| `--to <mzpeak\|mzml>` | inferred from the `-o` extension (`.mzML` → `mzml`, else `mzpeak`) | `mzml` writes a plain mzML (vendor → mzML) instead of mzPeak, bypassing the mzPeak-specific encoders (§4.1) |
| `--no-numpress` | off | Lossless delta m/z chunking instead of the default lossy numpress-linear |
| `--no-mz-lattice` | off | Disable the fixed-point m/z **lattice** for centroid peaks and store f64 `mz` instead — on every lane, the native Shimadzu `.lcd` one included (`MZPC_NO_MZ_LATTICE=1` does the same from the environment). Use it when the archive is destined for a reader that does not know the `mz-grid` codec (§9). Data that is not on a lattice is unaffected either way |
| `--chunk-size <CHUNK_SIZE>` | `50` | m/z chunk width (Th) for the chunked layout |
| `--zstd-level <ZSTD_LEVEL>` | `3` (timsTOF ims-compact lanes: `5`) | Zstd compression level (1–22). The ims-compact lanes default to 5, the measured byte-plane plateau; an explicit value applies to both (§9) |
| `-f, --force` | off | Overwrite the output if it already exists |
| `--no-ims-compact` | off | Bruker timsTOF (TDF) only: disable the default lossless ims-compact integer-TOF storage and write standard f64 m/z instead |
| `--representation <both\|profile\|centroid>` | `both` | Which signal representation to read when a vendor supplies BOTH profile and centroid for the same spectrum (Shimadzu `.lcd` does). `both` is faithful to the raw data: profile goes to `spectra_data`, centroid to `spectra_peaks`, and the metadata row carries both `number_of_data_points` and `number_of_peaks`. `profile` / `centroid` force one view; a representation the file does not contain is a warning, not an error — the other one is written. Honoured by the Shimadzu `.lcd` and Bruker BAF readers (BAF: mzPeak output only) |
| `--ims-chunked` | off | Bruker timsTOF (TDF) ims-compact only: select the **chunked** layout for rapid m/z-range access instead of the default **archive** layout (a flat table of absolute integer TOF bins). Splits each frame's peaks into 50-Th m/z bins (`--chunk-size` overrides), each chunk recording its TOF bounds as page-prunable Parquet columns — XIC / m/z-slice queries ~20× faster at parity-to-+8 % size. TOF is delta-encoded within each chunk: `chunk_start + cumsum(deltas)`, lossless (§9) |
| `--bruker-sdk` | off | Read Bruker TDF/TSF `.d` via the official Bruker timsdata SDK (parallel path to the default pure-Rust readers; Windows/Linux only, needs `timsdata.dll` / `libtimsdata.so`). On a TDF still writes the lossless ims-compact layout; add `--no-ims-compact` for f64 m/z |
| `--no-tims-recalibration` | off | Bruker timsTOF (TDF), ims-compact path only: disable this converter's vendor-grade scan→1/K0 recalibration (the `TimsCalibration` ModelType-2 model) and use timsrust's linear approximation. Recalibration is ON by default. INERT with `--no-ims-compact`: that path takes its mobility from mzdata's TDF reader, which applies the same ModelType-2 calibration itself, unconditionally |
| `--no-vendor` | off | Do not embed vendor side-files into the archive (§8) |
| `--no-chromatograms` | off | Do not synthesize TIC + base-peak chromatograms from the MS1 spectra (synthesis is on by default) |
| `--aux <AUX>` | — | Vendor side-file rule (repeatable): `glob=embed` or `glob=drop`. Highest precedence (§8) |
| `--image <IMAGE>` | — | **standard-lane inputs (mzML/imzML, Thermo `.raw`, TDF with `--no-ims-compact`, `--via-msconvert`):** embed an optical image VERBATIM into the archive as `images/image_NNNN.<ext>` with a `metadata.imaging` overlay affine. Repeatable. A bad/missing path ERRORS the conversion (strict). An `<input-stem>-opticalimage.{tif,tiff,png,jpg}` sibling is additionally auto-discovered (best-effort: warn + skip if unreadable) (§4.3) |
| `--sdrf <SDRF>` | — | **standard-lane inputs (mzML/imzML, Thermo `.raw`, TDF with `--no-ims-compact`, `--via-msconvert`):** embed an SDRF (sample-metadata) TSV VERBATIM as `sample_metadata/sdrf.tsv` with `metadata.study` + `metadata.sample_metadata` back-refs. A missing/unreadable path ERRORS the conversion (§4.3) |
| `--rt <MIN-MAX>` | — | mzPeak input only: keep spectra whose time is within MIN-MAX (unit matches the stored `spectrum.time`) (§4.2) |
| `--ms-level <MS_LEVEL>` | — | mzPeak input only: keep spectra with these MS levels (repeatable or comma-list) (§4.2) |
| `--drop-aux <DROP_AUX>` | — | mzPeak input only: drop archive members matching this glob (repeatable) (§4.2) |
| `--tof-grid <off\|auto\|on>` | `off` on mzML lanes; native SCIEX `.wiff`: `auto` when the flag is absent | **mzML inputs only** (incl. `--via-msconvert`): compactify exact-lattice TOF profile data by DETECTING an integer flight-time grid in the decoded f64 m/z and storing `tof_index` (Int32) + a per-run `{c0,c1}`, recovering `m/z = (c0 + c1·tof_index)²`. Bounded-lossy (reconstruction within `MZPC_TOF_GRID_PPM`). `auto` applies it when a strict fit passes; `on` requires the fit (errors otherwise); `off` keeps exact f64. Native readers ignore it: Bruker reads the integer grid from the vendor calibration, and the native Agilent (MHDAC) lane stores the f64 m/z the vendor library returns (a warning names the alternatives: `--via-msconvert --tof-grid`, or `--agilent-grid` for the flight-time grid of a profile `.d`). **Since 0.10.1 a gridded spectrum keeps the representation its source declares**: a profile spectrum's `tof_index` is filed in `spectra_data` (point layout), a centroid spectrum's in `spectra_peaks`; both facets declare the axis beside an f64 `mz` that is NULL on gridded rows, so `number_of_data_points` / `number_of_peaks` describe the source (until 0.10.0 every gridded spectrum was forced to centroid to reach the one facet that knew the axis — §9). **Native SCIEX `.wiff` (Windows):** Clearcore2 hands over decoded f64 m/z only, so that lane also fits the grid statistically; there the default (flag absent) is `auto` — the per-spectrum fit, unchanged from earlier releases — `off` stores the exact f64 m/z the vendor library returned (the opt-out the fidelity invariant requires), and `on` errors when no run-wide digitizer clock can be fitted |
| `--agilent-grid` | off | Agilent Q-TOF **profile** `.d` only: read the integer flight-time grid straight from `AcqData/MSProfile.bin` (pure Rust, no MHDAC/msconvert) and store `tof_index` (Int32) + per-spectrum `tof_c0`/`tof_c1`/`tof_calibration_id` columns (the MassHunter calibration drifts per scan) instead of f64 m/z — in `spectra_data`, since it is profile data (0.10.1; earlier releases filed it as centroid). Far smaller than the msconvert lane (≈0.14×). Only applies when `MSProfile.bin` is non-empty (centroid-only `.d` fall through to the standard path) |
| `--sample <N>` | — | SciEX `.wiff` only: convert sample `N` (1-based) of a multi-sample WIFF. Native lane and `--via-msconvert` (mapped to msconvert's `--runIndexSet N-1`). A multi-sample WIFF without it is refused and its samples are listed; a single-sample WIFF ignores it |
| `--via-msconvert` | off | Read the input via ProteoWizard `msconvert` (→ mzML → mzPeak). Cross-vendor path for formats without a native reader in this build (Agilent `.d`, SciEX `.wiff`, …) |
| `--msconvert-path <MSCONVERT_PATH>` | `$MSCONVERT_PATH`, else `msconvert` on `PATH` | Path to the `msconvert` executable |
| `-v, --verbose` | off | Verbose: print the inspection report and debug logs (repeat `-vv` for trace logs). An explicit `-v` / `-q` WINS over `RUST_LOG`; `RUST_LOG` is consulted only when neither flag is given (default level `info`) |
| `-q, --quiet` | off | Silence all logs except errors (wins over `RUST_LOG`, see `-v`) |
| `-h, --help` / `-V, --version` | — | Print help (`-h` for the summary) / print version |

**Options a lane cannot honour are refused, not dropped.** Since 0.9.13 the converter checks the
options you actually passed **on the command line** — never built-in defaults, and never a
config-file value (a config is a standing profile: its values take effect as defaults but cannot
make a lane refuse) — against the lane it selected, *before* any reader is opened, and exits 1
naming the option, the lane and the remedy, e.g. `--sdrf is not honoured by the timsTOF ims-compact
lane: the archive would be written WITHOUT it and exit 0. convert first, then add them on the
archive: …`. Earlier releases wrote the archive without the option and exited 0. The one exception
is the `.mzpeak` → `.mzpeak` filter lane, where a listed option loses nothing (the members are
re-packed verbatim): it is warned about by name and the run goes on. The table:

| Lane (how it is selected) | Refused options |
|---|---|
| `.mzpeak` → `.mzpeak` filter (§4.2) | **warned, not refused** — every convert-only option: `--layout --no-numpress --no-mz-lattice --chunk-size --zstd-level --no-ims-compact --representation --ims-chunked --bruker-sdk --no-tims-recalibration --no-chromatograms --aux --tof-grid --agilent-grid --via-msconvert --msconvert-path` is inert there (the filter re-packs Parquet members verbatim, so `--zstd-level 12` cannot change the output); the run continues with a warning naming the flag |
| `.mzpeak` → mzML export (§4.1) | the convert-only options above, plus `--image --sdrf` |
| `--to mzml` / `-o x.mzML` from a raw or exchange format (§4.1) | `--layout --no-numpress --no-mz-lattice --chunk-size --zstd-level --no-ims-compact --ims-chunked --bruker-sdk --no-tims-recalibration --no-chromatograms --aux --image --sdrf --tof-grid --agilent-grid` (`--representation` is honoured by BAF for mzPeak output only and is warned about, not refused) |
| `--agilent-grid` | `--image --sdrf --via-msconvert --layout --no-numpress --chunk-size` |
| `--via-msconvert` | `--aux` (the intermediate mzML is the source, so no vendor side-file of the original input can be embedded); `--image` / `--sdrf` ARE embedded since 0.9.13 |
| `--bruker-sdk` on a TDF (ims-compact) | `--image --sdrf --ims-chunked --no-tims-recalibration --layout --no-numpress --chunk-size` |
| `--bruker-sdk` on a TSF, or a TDF with `--no-ims-compact` | `--image --sdrf --ims-chunked --no-tims-recalibration` |
| default timsTOF (TDF) ims-compact | `--image --sdrf --layout --no-numpress` |
| native vendor readers (TSF / BAF / Agilent / `.wiff` / Waters / `.lcd`) | `--image --sdrf` |
| standard mzdata lane (mzML / imzML / Thermo `.raw` / TDF f64) | nothing |

Options a lane merely has no use for but that cannot change its output (`--no-vendor` on an mzML
export, `--tof-grid` on the native Bruker/Agilent lanes, `--bruker-sdk` on a non-Bruker input) are
deliberately *not* refused, so a shared recipe or config keeps working. Config-file values never
count as supplied, so a shared profile carrying `zstd_level: 12` or `sdrf:` sets defaults for the
lanes that use them and is a silent no-op on the lanes that cannot — put such an option on the
command line when you want the refusal to protect you.

### 4.1 mzML output (`--to mzml`, `-o x.mzML`, `-o x.mzML.gz`)

An output name ending in `.mzML` (or `--to mzml` with any name) writes a **plain mzML** through
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
`--drop-aux <glob>` (`--no-vendor` on this lane is shorthand for `--drop-aux 'vendor*'`). Parquet
facets are copied verbatim, so encoder options are inert here — warned about, not refused (see the
table above). The same
lane injects `--image` / `--sdrf` into an existing archive — the documented way to add them to an
archive from a lane that cannot embed them (§4.3) — and writes to `<out>.mzpeak.tmp` first, renaming
into place on success. The three filters on a **raw or exchange** input are a hard error with the
two-step remedy printed (convert first, then filter the archive); they used to be silently ignored.

```sh
mzpeak-convert run.mzpeak -o ms2_5to6.mzpeak --ms-level 2 --rt 5-6
mzpeak-convert run.mzpeak -o slim.mzpeak --drop-aux 'vendor/*.tdf_bin'
mzpeak-convert run.mzpeak -o annotated.mzpeak --sdrf run.sdrf.tsv
```

### 4.3 Embedding sample metadata and images (`--sdrf`, `--image`)

`--sdrf <file.tsv>` embeds an SDRF verbatim as `sample_metadata/sdrf.tsv` and adds
`metadata.study` + `metadata.sample_metadata` back-references to the index; `--image <file>`
embeds an optical image verbatim as `images/image_NNNN.<ext>` with a `metadata.imaging` overlay
affine (an `<input-stem>-opticalimage.{tif,tiff,png,jpg}` sibling is auto-discovered). Both are
strict: a missing or unreadable path fails the conversion. An explicit `--image` needs the MS
pixel grid (`IMS:1000042/43`) to map onto, so it is effectively **imzML-only**; `--sdrf` needs
nothing. Which lanes embed them:

| Lane | `--sdrf` / `--image` |
|---|---|
| mzML / imzML on the standard lane, **including the `--tof-grid` sub-path** (the same command used to keep or lose the SDRF depending on whether the grid fit passed — fixed in 0.9.13) | embedded |
| Thermo `.raw`, Bruker TDF with `--no-ims-compact` (mzdata path) | embedded (`--sdrf`; `--image` needs an imzML grid) |
| `--via-msconvert` | embedded (since 0.9.13; it used to hard-code "none") |
| `.mzpeak` → `.mzpeak` (§4.2) | injected into the existing archive |
| default timsTOF ims-compact, both `--bruker-sdk` lanes, `--agilent-grid`, native vendor readers (TSF / BAF / Agilent / `.wiff` / Waters / `.lcd`) | **refused** (exit 1) — convert first, then inject: `mzpeak-convert out.mzpeak -o with.mzpeak --sdrf …` |
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
to: mzpeak                 # or: mzml (default: inferred from the output extension)
layout: chunked            # or: point
no_numpress: false
no_mz_lattice: false
chunk_size: 50
zstd_level: 9
force: true
no_ims_compact: false      # TDF: keep the lossless ims-compact default
ims_chunked: false
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
| imzML | ✅ | ✅ | ✅ | imaging coordinate columns; IMS CV promoted |
| Bruker `.d` **TDF** (timsTOF) | ✅ | ✅ | ✅ | ion mobility; **ims-compact by default** |
| Bruker `.d` **TSF** (line spectra) | ✅ | ✅ | ✅ | MALDI/TOF; otofControl m/z correction |
| Thermo `.raw` | ✅ | ✅ | ✅ | needs a **.NET 8+ runtime** |
| Bruker `.d` **BAF** | ✅ | ❌ | ✅ | auto-built; needs `libbaf2sql_c` at runtime |
| Agilent `.d` (native, scan data) | ❌ | ❌ | ✅ | net48 `AgilentGlueHost.exe` (§11) → MHDAC, since 0.11.0; **MRM/SIM-only runs are refused** — they are transition chromatograms, use `--via-msconvert` for them |
| SciEX `.wiff` (native) | ❌ | ❌ | ✅ | auto-built; Clearcore2 DLLs at runtime; **MRM/SIM dwell runs are refused** (they are transition chromatograms — `--via-msconvert` writes them as SRM chromatograms); multi-sample files need `--sample N` |
| Shimadzu `.lcd` (native) | ❌ | ❌ | ✅ | LabSolutions.IO DLLs at runtime (§11); profile as a sqrt grid, centroids as an exact lattice (§8, §9) |
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
  `partial` (only when `MZPC_MAX_SPECTRA` truncated the run, §10), declared file entries.
  Since 0.9.13 `source_files[].location` never carries the converting machine's path (it is
  reduced to the bare `file://` authority; non-`file` URL schemes are kept), `run.id` is never a
  path, and `default_instrument_id` always resolves: a run without an instrument record gets one
  empty configuration `0` to point at (the spec requires the integer; 0.10.0 briefly wrote `null`,
  which the validator's schema check refuses — fixed in 0.10.2).
- `spectra_metadata.parquet` — per-spectrum descriptors (id, index, MS level,
  polarity, scan time, precursor info, …).
- `spectra_data.parquet` / `spectra_peaks.parquet` — signal arrays (chunked/point): profile
  spectra in `spectra_data`, centroid spectra in `spectra_peaks`, by the representation the source
  declares — since 0.10.1 for grid-encoded TOF axes too (§9).
- `chromatograms.parquet` — TIC/BPC/SRM and other chromatograms.
- `vendor/…` — embedded original side-files (optional, see §8).

**Footer count keys.** The spectrum, chromatogram and wavelength facets carry `<entity>_count`
and `<entity>_data_point_count` in their Parquet key–value footers (the `vendor/…` facets carry
neither). The specification does not define these keys; this converter writes them with one
definition (since 0.11.2, issue #1): on a **data facet** (`spectra_data`,
`spectra_peaks`, `chromatograms_data`, `wavelength_spectra_data`) the count is the number of
entities with at least one row *in that file* and the point count is the points *in that
file* — so a centroid-only run's empty `spectra_data` says `0 / 0`, and a mixed run's
`spectra_data` counts only its profile spectra. It is a cardinality, **not an index bound**:
`spectrum_index` values in a data facet are sparse, so never iterate `0..spectrum_count`. The
**run total** lives on the primary metadata facets (`spectra_metadata`, `chromatograms_metadata`,
`wavelength_spectra_metadata`), which also repeat the data facets' point totals; the secondaries
(`_scans`, `_precursors`, `_selected_ions`) carry the run total after a direct conversion (an
archive rewrite re-stamps them with the entities present in that facet — nothing reads them). To
plan reads, use the per-spectrum `number_of_data_points` / `number_of_peaks` columns of
`spectra_metadata` (the spec's mechanism) or the actual indices in the facet; a facet with
`num_rows == 0` has nothing to read whatever its footer says. Archives from 0.11.1 and earlier
declare the run total on `spectra_data` (and the sum of both data facets' points), and on
`spectra_peaks` the number of centroid spectra handed to it, zero-peak spectra included.

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

**Run metadata the vendor states (native lanes, since the release after 0.11.2).** The mzML lane
inherits ProteoWizard's finished model; the native lanes build one from what each vendor file
STATES, merged field by field (`src/run_metadata.rs`) — nothing is guessed, so a lane records a
serial, a sample or a source (ion source, detector) only where the file says so:

| Lane | Read from | Instrument | Software | Sample | Time | Source members (each with MS:1000569 SHA-1) |
|---|---|---|---|---|---|---|
| Bruker TDF / TSF | `GlobalMetadata` | MS:1003123 timsTOF family + `InstrumentName`, serial, TOF analyzer | `AcquisitionSoftware` + version | `SampleName` | `AcquisitionDateTime` (zoned) | `analysis.tdf`/`.tsf` + `_bin` |
| Agilent `.d` | `AcqData/Devices.xml`, `Contents.xml`, `sample_info.xml` (any host) | MS:1000490 + name, model number, serial, analyzers implied by the device type | MassHunter + `AcqSoftwareVersion` | `Sample Name` | `AcquiredTime` (with its offset) | the AcqData files (no exported text, no dot files) |
| Waters `.raw` | `_HEADER.TXT`, `_extern.inf` (any host); per scan: MassLynxRaw (Windows) | MS:1000126 + model, serial unless `#NotSet` | MassLynx `Created by` version | `Acquired Name` + descriptors | `Acquired Date/Time` (no zone) | `_FUNCnnn.DAT` (Waters nativeID) then the side files |
| SciEX `.wiff` | Clearcore2 sample/instrument details (Windows) | MS:1000121 + `InstrumentName`, serial | Analyst + `SoftwareVersion` | sample name | `AcquisitionDateTime` (no zone) | `.wiff` + `.wiff.scan`, digested before the library opens them |
| Shimadzu `.lcd` | the `.lcd`'s own `File Property` stream (any host) + LabSolutions.IO (Windows) | MS:1002998 + `SystemName`, ESI + quadrupole + TOF from the device id | LabSolutions + `DataFileProperty.szVersion` | `smpl_name` (+ id, vial, operator, injection volume) | `SampleInfo.DateTime`: a UTC FILETIME presented in the writer's stated GMT offset (`+01'00'`) — fully zoned | `.lcd` |
| Thermo `.raw` | mzdata's Thermo reader | complete already | Xcalibur | yes | zoned | `.raw` |

**Acquisition time: stated offset or nothing.** `run.start_time` is an RFC 3339 instant, and
RFC 3339 cannot say "zone unknown". A vendor time that STATES its offset (Agilent
`2022-11-01T13:11:27-04:00`, Bruker, Shimadzu's UTC FILETIME + `+01'00'`) is written verbatim. A wall
clock WITHOUT one (Waters, SciEX) leaves `run.start_time` null and is preserved verbatim in the index:

```json
"acquisition_time": {"wall_clock": "2018-12-03T22:39:33", "zone": "unstated",
                     "source": "Waters _HEADER.TXT", "note": "…"}
```

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
with the frame's drift-time bounds (MS:1003439/1003440), `sort-by-mz` in `transformations`, and a
`waters_drift` index block holding the run's bin → ms table, the vendor's `mob_cal.csv` CCS
calibration verbatim, the lock-mass function and the functions not written as spectra. Frames keep
every point MassLynx returns: the writer's zero-run mask is off for them (`zero-run-mask` is absent
from `transformations`), because a run of zeros in an interleaved frame is several bins' trace
boundaries meeting. ProteoWizard's default instead writes one spectrum per drift bin (Capan2:
397,800 spectra for 1,989 scans); the two are the same data (verified bin for bin), 531 MB as
frames against 965 MB as bins. Spectra are in acquisition-time order across functions. The scan
row's `ion_mobility_value` stays NULL on purpose: a frame has no single drift time. Retention time,
polarity, scan window, the MS level (from the function-type code: product-ion types are MS2, the
second function of an MSe pair is MS2, every other MS function — lock mass, auxiliary — is MS1) and
the precursors come from the SDK: a set mass > 0 (DDA) gives a selected ion with a target-only
isolation window and the collision energy; an MSe elevated-energy scan (set mass 0) gets a
precursor stating the activation only — no isolation window is invented. Chromatogram functions
(SIR, MRM, neutral loss/gain) and non-MS functions (DAD, delay, calibration) are skipped with a log
line; a SONAR function (its bins are quadrupole positions) is refused; "collapsed retention time"
functions (one row per drift bin, the run's summed mobilograms — Capan2 functions 4–6) are
recognised and not written as spectra (`MZPC_WATERS_KEEP_COLLAPSED=1` keeps them).

**What the native lanes still do not carry** (tracked in BACKLOG.md): per-scan precursors on
the Agilent-MHDAC, BAF and SciEX lanes (Bruker TDF/TSF, Shimadzu and Waters have them), and the
non-MS device chromatograms (UV, pressure, temperature) the mzML lane gets from pwiz.

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
the native integer TOF grid plus the `a,b` calibration in `ims_calibration` so a
reader reconstructs `m/z = (a + b·tof)²` exactly. That `a,b` is timsrust's two-point
chord (−5…−11 ppm against the vendor SDK), so the archive also carries the vendor's
**exact** calibration: the `vendor_mz_calibration` index block holds every
`analysis.tdf` `MzCalibration` row verbatim plus `DigitizerNumSamples` /
`MzAcqRangeLower` / `MzAcqRangeUpper`, and `spectra_metadata` gains per-frame
`…_tdf_t1`, `…_tdf_t2`, `…_tdf_mz_calibration_id` columns (`Frames.T1/T2/MzCalibration`;
`MZP:1000008`–`MZP:1000010` since 0.10.1, `MS:4000903`–`MS:4000905` before — match the suffix).
The block spells out the ModelType-1 expression a reader evaluates —
`t_ns = tof·DigitizerTimebase + DigitizerDelay`,
`C1_eff = C1·(1 + dC1·(T1 − tdf_t1)/1e6)`, `t_ns = C0 + (1e6/√C1_eff)·√mz + C2·mz`
solved for √mz — verified in speXtract to 2.5e-5 ppm against Bruker's SDK. It is
present with `--no-vendor` too. Because the chord is an approximation, `ims_calibration`
says so: `"exact": false`, with an `approximation` note (two-point chord; drops `C2·mz`
and the per-frame temperature term) and `exact_model` pointing at `vendor_mz_calibration`.
How far off the chord is depends on the file: on PXD059079 2485.d (`C2 = 0`) it runs from
+3.2 ppm at TOF 0 to −4.2 ppm at the top of the range; on a diaPASEF run with `C2 ≠ 0` it
was +8.5 / −10.6 / −3.4 ppm at TOF 0 / mid / max, and a 20 ppm search on it lost 11.7 % of
the peptides at 1 % FDR. A reader that wants vendor-grade m/z evaluates the ModelType-1
expression; one that does not is still exact to the archive's own `tof` grid.

**Exact per-spectrum coefficients when `C2 = 0`.** When every `MzCalibration` row a run
references is ModelType 1 with `C2 = C3 = C4 = dC2 = 0` — stored as numeric zeros; a NULL or
text cell is a *missing* term, not a zero, and keeps the run on the chord (PXD059079 2485.d is
such a file), the vendor model is *exactly* a sqrt-linear law in `tof` per frame:
`m/z = (tof_c0 + tof_c1·tof)²` with `tof_c1 = DigitizerTimebase·√C1_eff/1e6`,
`tof_c0 = (DigitizerDelay − C0)·√C1_eff/1e6` and `C1_eff` temperature-corrected with the
frame's `Frames.T1`. Both ims-compact lanes (native and `--bruker-sdk`) then write the pair as
per-spectrum Float64 columns `…_tof_c0` / `…_tof_c1` in `spectra_metadata` (the same columns
and accessions the SciEX/Agilent/Shimadzu sqrt grids use: the converter-owned `MZP:1000003` /
`MZP:1000004` terms of `cv/mzpeak.obo`, so the columns are `opt_MZP_1000003_tof_c0` /
`opt_MZP_1000004_tof_c1`; archives written before 0.10.1 carry them as `opt_MS_4000900_tof_c0` /
`opt_MS_4000901_tof_c1`, and every reader in the family binds by the `_tof_c0` / `_tof_c1` suffix
or by name, so both generations reconstruct), stamp the `tof` column with
`mzpeak:transform_params_per_spectrum = "tof_c0,tof_c1"`, and add
`"per_spectrum": "tof_c0,tof_c1"`, `"exact_per_spectrum": true` and a note to
`ims_calibration`; `a`/`b` and `"exact": false` stay for readers that only know the run-wide
chord. The vendored reader — and therefore `mzpeak-convert ARCHIVE -o x.mzML` — reconstructs
m/z from the per-spectrum pair (1e-12 relative to the ModelType-1 model, versus up to 4.2 ppm
for the chord on 2485.d). A frame whose `Frames.T1` is NULL cannot be evaluated and gets *no*
pair: its `tof_c0`/`tof_c1` cells are NULL, readers fall back to the chord for that spectrum,
and the count appears as `"per_spectrum_chord_frames"` — so `"exact_per_spectrum": true` is a
per-spectrum statement (a spectrum *with* the pair is on the model). A run with any `C2 ≠ 0`
row, or a TDF whose `Frames` table lacks `T1`/`MzCalibration` (both lanes), gets no
`tof_c0`/`tof_c1` columns and no `per_spectrum` keys — nothing changes for it. One caveat: the
ModelType-1 formula is SDK-verified on `C2 ≠ 0` runs only, so the `C2 = 0` pair reproduces the
*formula* exactly; `MZPC_TDF_SDK_GOLDEN=<out.json>` (§10) dumps the SDK's own `tims_index_to_mz`
at up to 240 `(frame, tof)` points during a `--bruker-sdk` conversion, and dropping that dump of
2485.d in as `tests/fixtures/tdf_calibration_golden_c2zero.json` turns the converter's
`c2_zero_sdk_goldens_match_the_sqrt_linear_pair` test into the missing vendor check.

**Several precursors on one spectrum (timsTOF PASEF).** dia-PASEF writes two precursors
per MS2 frame and DDA-PASEF several, all with the same `(source_index, precursor_index)`
join key — the key the spec gives is not unique per precursor. The reference reader
(vendored here) therefore keeps the precursors in their stored order (a stable sort; the
unstable one reordered them against their ions) and, where a spectrum's precursor and
selected-ion counts agree, pairs them **positionally in row order** — the only reading
the archive supports. Where the counts differ (one precursor with several ions, SPS-MS3;
or ions missing) nothing is assumed and every ion is attached to the first precursor as
before. Other readers should apply the same rule; a per-spectrum precursor ordinal in the
spec is the long-term fix.

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

**Fidelity: what is preserved, and the four declared transforms.** The project invariant
(decided 2026-09-04) is that the archive preserves the vendor's signal **as much as possible, to a
stated fidelity, with every transformation declared** in the index's `transformations` list — so a
reader can tell from the archive alone what was done to the data. Retention time, precursor m/z
and charge, centroid m/z (f64, or the bit-exact fixed-point lattice of §9) and integer TOF
round-trip bit-for-bit; verified against mzdata's own mzML output on a 4,880-spectrum DDA run with
zero differences. Four transforms are **not** bit-exact, and each is named in the archive:

1. **numpress-linear** (`numpress-linear`) — the *default* chunk encoding of profile m/z on the `chunked` layout is
   lossy (§9); `--no-numpress` selects the lossless delta encoding.
2. **Profile zero-run compaction** (`zero-run-mask`) — in **profile** spectra, a run of two or more *consecutive*
   zero-intensity points is collapsed to a single zero at each peak boundary —
   `[0,0,0,0,0, 900, 500, 0,0,0, 300, 0]` (12 points) is stored as `[0, 900, 500, 0, 0, 300, 0]`
   (7). The baseline extent of every peak is preserved, so the profile shape is unchanged, but
   `number_of_data_points` reflects the stored count rather than the source's. **Centroid spectra
   are never touched** — isolated and interior zero-intensity centroids round-trip exactly.
3. **`--tof-grid` sqrt grid** (`tof-grid:<ppm>ppm`) — a profile is stored on an integer sqrt-space grid only when every
   point reconstructs within the ppm bound (`MZPC_TOF_GRID_PPM`, default 5); the achieved
   `max_roundtrip_ppm` is recorded. Spectra outside the bound keep f64 m/z.
4. **Shimadzu profile pad trim** (`shimadzu:span-trim`) — the native `.lcd` route fits and stores the signal span between
   the first and last positive sample; the zero-intensity pad LabSolutions writes at the
   scan-window bounds is not stored (the span bounds are).

Where the source m/z is on a lattice the archive says how exactly it reconstructs
(`mz_reconstruction` with its `max_error_da` bound) rather than claiming "exact": the Shimadzu
profile block says `within-vendor-rounding` with `max_error_da: 5e-10` (measured: 4,890 of 5,000
gridded points rebuild off the vendor's 1e-9 lattice by ≤ 0.5 step, inside the vendor's own
rounding), the Agilent file-direct block is the one lane that says `exact`, and the two SCIEX
lanes say `bounded-lossy` with `roundtrip_tolerance_ppm`.

**The `transformations` index key.** Every mzPeak lane writes `metadata.transformations` — a JSON
list of the declared, bounded changes the converter made to the vendor signal on its way in
(`transformations_block`, `src/main.rs:4683`). An empty list is a statement too. The vocabulary:

| Entry | Written when | Lanes |
|---|---|---|
| `zero-run-mask` | always — the writer's zero-intensity run compaction (item 2) | every lane |
| `numpress-linear` | the lossy m/z chunk codec was chosen on any facet (item 1) | chunked layout without `--no-numpress` |
| `sort-by-mz` | at least one spectrum arrived out of m/z order and was re-sorted (tracked per run, not assumed) | generic mzdata lane |
| `tof-grid:<ppm>ppm` | a statistically fitted integer grid replaced f64 m/z within that bound (item 3) | mzML `--tof-grid`, native SCIEX per-spectrum grid |
| `shimadzu:span-trim` | the profile sqrt-grid route stored the signal span only (item 4) | native Shimadzu `.lcd` profile |
| `agilent:drop-zero-samples` | the profile grid lane stored a sparse point list, dropping zero-intensity samples and all-zero scans | `--agilent-grid` |

Not declared today, on purpose and worth knowing: the `--ims-chunked` ims-compact layout sorts each
frame by TOF before chunking, which re-orders points across scans (an entry of the `sort-by-mz`
class; tracked in `BACKLOG.md`). Beside `transformations`, two other 0.9.13 index keys let a reader
audit an archive offline: `metadata.partial` marks a run truncated by `MZPC_MAX_SPECTRA` (§10), and
`ims_calibration.chord_source` (`global_metadata` on the native timsrust lane, `sdk_tims_index_to_mz`
under `--bruker-sdk`) says which of the two (a, b) chords — measured 4.28 ppm apart on 2485.d — an
ims-compact archive holds.

**Verbatim vendor side-files (preserved, not interpreted).** For Bruker `.d`, the
original side-files (methods, calibration, acquisition databases, …) are
**embedded by default** under `vendor/` in the archive — gzip-compressed and
declared `proprietary` in the index — so nothing the converter does not yet model
is lost. For Thermo `.raw`, the scan trailers (FAIMS CV, injection time, charge,
…) and status log are captured verbatim into dedicated `vendor_scan_trailers`
(tall + wide) and `vendor_status_log` facets.

**Including / excluding.** The embedding is policy-driven (preserve-by-default):

- `--no-vendor` (or `no_vendor: true`) — embed nothing.
- `--aux 'glob=drop'` / `--aux 'glob=embed'` — per-glob rule, highest precedence,
  repeatable. The same rules can be given as the `aux:` list in the config file
  (§5). For example, drop the bulk binaries but keep the method:
  `--aux '*.tdf_bin=drop' --aux '*.method=embed'`.

## 9. Compression, layout & ims-compact

- **Layout** — `chunked` (default) groups m/z into chunks (`--chunk-size`, Th) and
  encodes each with numpress-linear (lossy, compact) or, with `--no-numpress`,
  lossless delta. `point` writes one row per (m/z, intensity).
- **zstd** — applied inside Parquet, `--zstd-level` 1–22 (default 3; the timsTOF **ims-compact**
  lanes default to **5**, the measured byte-plane plateau — an explicit `--zstd-level` applies to
  both).
- **Fixed-point m/z lattice** *(automatic; `--no-mz-lattice` to disable)* — some vendors hand over
  m/z that are really integers over a power of ten: Shimadzu `MassHigh` at 1e-9 Da, its coarse
  `Mass` field at 1e-4 Da, and the LabSolutions **mzML export** of the same acquisition. The
  converter samples the CENTROID m/z of six spectra spread across the run and, if every one of them
  lands on such a lattice, stores the peaks facet as `point.tof_index` = `round(m/z · scale)`
  (Int64, DELTA_BINARY_PACKED) with an `mz_calibration` index block (`"codec": "mz-grid"`) and a
  `LinearMz` transform on the column; readers recover `m/z = tof_index / scale` — the DIVISION, not
  a multiplication by the column's `mzpeak:transform_params` (`1/scale`), which is a different
  number: `1e-9` is not exactly 10⁻⁹, so `tof_index · 1e-9` lands one ulp (~1e-13 Da) off the
  source value on about 40 % of points. Dividing by the `scale` in the `mz_calibration` block
  reproduces the vendor's f64 **bit for bit**, which is what makes the lattice **lossless
  and smaller than either chunk encoding**, so on the peaks facet it supersedes both numpress-linear
  and delta. Measured on the 4.5 GB LabSolutions `DIA_Hela_20ng` mzML (279.7 M centroids):
  **2,188 MB** (lossless delta) or 1,355 MB (lossy numpress) → **1,312 MB**, with the m/z bytes
  going 1,897 MB → 1,035 MB (−45 %); on the 13,200-spectrum `Blind_P1_pos_012.mzML`,
  3,709 kB → 2,264 kB (−39 %). Nothing is snapped: a spectrum with even one off-lattice value keeps
  its exact f64 m/z in the same facet's `mz` column, per spectrum. Only the **peaks** facet is
  affected — profile arrays keep the chunked layout and the `--no-numpress` / `--chunk-size` /
  `--layout` choices exactly as before, so a profile-only input converts unchanged. The native
  Shimadzu `.lcd` lane has done this at 1e-9 since v0.9.0; this is the same mechanism applied to any
  input whose data earns it. `--tof-grid` (a different, sqrt/flight-time grid) still wins where it
  is asked for and fits.
- **Reader support for the lattice, and when to turn it off.** A lattice archive's peaks facet has
  an Int64 `point.tof_index` and an all-NULL `point.mz` on the routed rows, so a reader that does
  not know the `mz-grid` codec sees no m/z there (`mzpeak-convert`'s own vendored reader and
  mzPeakViewer do know it; other tools in the mzPeak family — OpenMS's `MzPeakFile`, mzPeakJ,
  mzPeakIV, mzPeakExplorer, mzPeakValidator — do not, at the time of writing, and read those cells
  as 0). Until they do, pass **`--no-mz-lattice`** (config `no_mz_lattice: true`, or
  `MZPC_NO_MZ_LATTICE=1` in the environment) when the archive is destined for one of them: it
  stores plain f64 `mz` instead, on **every** lane — the mzML/generic one and the native Shimadzu
  `.lcd` one alike. Note this is a change of on-disk representation for ordinary mzML input, which
  before v0.9.7 always got f64 `mz`; the values are the same either way.
- **ims-compact** — for Bruker timsTOF (**TDF**) this is the **default**: the
  native integer `tof` is stored bit-exact (Int32 + `ims_calibration`) instead of
  f64 m/z, roughly halving the m/z bytes with an exact grid. Disable with
  `--no-ims-compact` to write standard f64 m/z. m/z is reconstructed by readers as
  `m/z = (a + b·tof)²` — the chord, marked `"exact": false`; the vendor's exact model sits
  beside it in `vendor_mz_calibration` (§8).
- **TOF-grid archives (`--tof-grid`, native SCIEX `.wiff`, `--agilent-grid`) — one axis, two
  facets (since 0.10.1).** The Int32 `tof_index` column (`SqrtMzFromTof`, `mzpeak:transform_params`
  or `…_per_spectrum` on the field) is declared on BOTH `spectra_data` (point layout) and
  `spectra_peaks`, each beside an f64 `mz`. A spectrum goes to the facet its declared representation
  selects and is gridded or not independently of that: on a gridded row `tof_index` is set and `mz`
  NULL, on an off-lattice row `mz` holds the exact f64 and `tof_index` is NULL — never both. Readers
  therefore decide per row, not per facet, and `spectrum_representation` / `number_of_data_points` /
  `number_of_peaks` mean what the source said. Until 0.10.0 only the peaks facet knew the axis, so
  every gridded spectrum was rewritten to centroid to reach it: the 13 published TOF-grid archives
  labelled 1.6 M profile spectra as centroid spectra with `number_of_peaks` set (review item M6;
  fixed together with the accession move above, one corpus rebuild). Under `--tof-grid off`
  (SCIEX) nothing is gridded and `spectra_data` keeps the requested chunked layout. Size: the
  off-lattice profile minority of a native SCIEX run is now stored as exact f64 points rather than
  numpress chunks — on the corpus that share is 0.07–9.2 % of the points and the archives grew
  0.2–27 % (see the 0.10.1 changelog for the per-file numbers).
- **ims-compact TOF layout (two modes)** — the peak facet has two mutually-exclusive layouts,
  recorded in `ims_calibration.tof_encoding`:
  - **Archive** *(default)* — a flat table of **absolute integer TOF bins** (`absolute`). Maximum
    compression and fast whole-spectrum access; no m/z index (an m/z-range query is a full scan).
    Size vs the vendor `analysis.tdf_bin`: DDA-PASEF runs come out below it; a dense diaPASEF run
    (S30, 2.47 G peaks) is **+3–5 %** at zstd 3–15, with `tof` two thirds of the table. (A per-scan
    delta variant existed up to v0.7.2 and was removed in v0.7.3 — no reader decoded it correctly and
    its m/z is wrong after the first peak of each scan; such archives are ~8 % smaller only because
    small deltas byte-shuffle well. Reconvert them, and never use one as a size baseline.)
  - **Chunked** *(`--ims-chunked`)* — each frame's peaks are split into true m/z bins (`--chunk-size`,
    default 50 Th); each chunk stores its main-axis (TOF) bounds (`chunk_start`/`chunk_end`, Parquet
    page-prunable) and delta-encodes TOF within the chunk (`m/z-chunked`). **m/z-slice / XIC queries
    are ~20–30× faster** (they touch only the overlapping chunks) at roughly parity size. Reconstruct
    a chunk's absolute TOF as **`chunk_start + cumsum(deltas)`** — the first point is `chunk_start`
    itself and is *not* in the delta array; summing the array alone is wrong from the first point of
    every chunk. Whole-spectrum access matches archive when
    row groups are sized finely (`MZPC_ROW_GROUP_ROWS`); the default (8192 chunks/row group) is coarse
    on very large files. On the diaPASEF S30 run the chunked table is **−2 %** vs the vendor file
    (−8 % on a DDA run): TOF deltas shrink to a fifth, but the per-peak `1/K0` column becomes half the
    table — sorting each frame by TOF scrambles the scan id, ~1.2 B/peak of irreducible entropy — so a
    per-scan mobility representation would not help here either.
- **Shimadzu `.lcd` (two integer axes, one per facet)** — the native lane stores each facet on
  the exact integer grid the vendor data sits on; both are lossless and reproduce the vendor's
  m/z to the last digit.
  - **Profile → per-spectrum sqrt grid** in `spectra_data` (point layout): `tof_index` (Int32,
    delta-packed) with per-spectrum `tof_c0` / `tof_c1` columns and an f64 `mz` column that is
    NULL on gridded rows; `m/z = (tof_c0 + tof_c1·tof_index)²`, `tof_c1` constant across the run
    (`tof_calibration`: `{codec: tof-grid, model: sciex_sqrt_per_spectrum, vendor: shimadzu,
    run_wide_c1, per_spectrum_columns}`), verified on every point to ≤ 1e-9 before a spectrum
    is gridded. A spectrum that does not fit (LabSolutions clamps the first/last sample of some
    MS2 scans to the scan-window bound) keeps f64 m/z in the same facet.
  - **Centroids → exact Int64 lattice** in `spectra_peaks` (point layout, never chunked or
    numpressed): `point.tof_index` Int64 with `LinearMz` and `mzpeak:transform_params = "1e-9"`,
    i.e. `m/z = tof_index / 1e9` (the division, not `1e-9 · tof_index` — see above), plus the
    f64 `point.mz` fallback (NULL on lattice rows) and
    `point.intensity`; index block `mz_calibration: {codec: mz-grid, scale: 1e9, vendor:
    shimadzu, lossless: tof_index, applies_to: spectra_peaks}`. Each centroid list is checked on
    its own (`|m/z·1e9 − k| < max(1e-3, 8 ulp)`, `k` non-decreasing); one that fails keeps f64.
  - **Reader rule, per facet:** for the centroid facet consult `mz_calibration` first, for the
    profile facet `tof_calibration` first; in both, a row whose `mz` is finite and > 0 is an f64
    fallback and wins over the axis (a NULL Int64 cell materialises as 0 in some Arrow bindings —
    never reconstruct from it). The two `tof_index` columns differ in dtype and transform.
  - **Size** (`MassHigh` f64 → grid + lattice): Blind_P1_pos_012 5.24 → **3.79 MB**, HEK_PosOAD1
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

Every `MZPC_*` variable the converter — or the vendored `mzpeak_prototyping` writer it links, or
the Shimadzu glue it hosts — reads is listed below: 26 names, reconciled against the tree
(19 read in `src/`, 4 in the vendored writer, 2 in `glue/shimadzu/Glue.cs`, 1 comment-only).
They fall into three groups: **deployment** (where the vendor libraries and glue live — you will
set these on a Windows conversion host), **output-affecting** (they change what is written —
prefer the equivalent CLI flag where one exists, so the run is reproducible from its command line;
where an archive can tell, the row says which index key records it) and **performance /
diagnostic** (they tune or trace, and the dump levers replace the conversion).

**How a boolean lever is read (since 0.9.13).** Every on/off `MZPC_*` lever in `src/` goes through
one `env_flag()` (`src/main.rs:113`): **unset** → the built-in default; set to the empty string,
`0`, `false` or `no` (any case) → **off**; anything else → **on**. So `MZPC_DUMP_IM_TABLE=` (empty)
is off, and `MZPC_BYTE_PLANE_INTENSITY=` (empty) is the same opt-out as `=0`. Before 0.9.13 each site
spelt its own rule: the two dump levers fired on mere presence and an empty
`MZPC_BYTE_PLANE_INTENSITY` silently switched the ims-compact intensity column to Float32. The two
levers read by the vendored writer (`MZPC_PARALLEL_ENCODE`, `MZPC_TIMING`) and the two read by the
Shimadzu glue keep their own, narrower spellings, noted in their rows. Numeric levers ignore a value
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
| `MZPC_PWIZ_DIR` | ProteoWizard install supplying the vendor DLLs at runtime (Agilent MHDAC/MIDAC, SciEX Clearcore2, Shimadzu LabSolutions.IO, Waters MassLynx). Both layouts are probed — `vendor_api/<Vendor>` and flat beside `msconvert.exe` (the 3.0.26175 installer is flat; the Agilent host is handed whichever directory holds `MassSpecDataReader.dll`). **Use a current ProteoWizard** (3.0.26151 / 3.0.26175 verified); see §11 for why an old one silently corrupts Shimadzu centroids |
| `MZPC_MASSLYNX_DIR` | Directory holding `MassLynxRaw.dll` (+ `cdt.dll`) for the Waters lane. Wins over `MZPC_PWIZ_DIR`, which is the fallback. (`MZPC_WATERS_GLUE` is **not read by any code path** — the Waters lane has no .NET glue; the name survives only in a comment) |
| `MZPC_AGILENT_GLUE` | Directory holding the built net48 `AgilentGlueHost.exe` (`glue/agilent/bin/Release/net48`); the converter spawns it once per `.d` and reads its `AGL2` output back (§11) |
| `MZPC_AGILENT_TMPDIR` | Where the Agilent host materialises a run before it is read (16 B/point — about 3 GB for a 240 MB Q-TOF `.d`; removed when the reader closes). Default `%TEMP%`; set it to a disk directory when `TEMP` points at a RAM disk (the box scripts do) |
| `MZPC_AGILENT_MIDAC_GLUE` | Directory holding the built `AgilentMidacGlue.dll` + runtimeconfig (Agilent ion mobility; the MIDAC lane is a scaffold — an IM-QTOF `.d` is refused by the native lane and goes through `--via-msconvert`) |
| `MZPC_SCIEX_GLUE` | Directory holding the built `SciexGlue.dll` + runtimeconfig |
| `MZPC_SHIMADZU_GLUE` | Directory holding the built `ShimadzuGlue.dll` + runtimeconfig |

**Output-affecting.** These change the bytes that are written; where a CLI flag exists, use it
instead.

| Variable | Effect | Recorded in the archive? |
|---|---|---|
| `MZPC_NO_MZ_LATTICE=1` | Same as `--no-mz-lattice` (§9): store f64 `mz` instead of the fixed-point lattice, on every lane (`env_flag` spellings) | implicitly — the peaks facet has an `mz` column and no `mz_calibration` block |
| `MZPC_SHIMADZU_COARSE_MZ=1` | Shimadzu glue: read the coarse 1e-4 `Mass` instead of `MassHigh` (§8). Read by the C# glue and compared to the literal `1` — only `=1` switches it | no — `mz_calibration.source` is the same string either way (open item) |
| `MZPC_BYTE_PLANE_INTENSITY=0` | Opt out of Int32 byte-plane intensity (on by default for timsTOF ims-compact) back to Float32 (`env_flag` spellings: empty, `0`, `false`, `no` all opt out) | yes — `ims_calibration.intensity_dtype` = `int32` \| `float32` (0.9.13) |
| `MZPC_TOF_GRID_PPM=<ppm>` | `--tof-grid` reconstruction tolerance (default 5.0). The lane is bounded-lossy and this number **is** the bound — raising it above the instrument's mass accuracy is not defensible. Logged as a warning when set | yes — `transformations` carries `tof-grid:<ppm>ppm`, and the `tof_calibration` block its `roundtrip_tolerance_ppm` |
| `MZPC_TOF_GRID_C1=<step>` | `--tof-grid`: force the sqrt-space step instead of inferring it (`c1 = quantum / (2·√mz_max)`) | the fitted `{c0,c1}` is stored; the fact that `c1` was forced is not |
| `MZPC_MAX_SPECTRA=<n>` | Stop after `n` spectra. **Deliberately truncating**: it also disables the "all source spectra written" completeness check, so the archive is a partial one that exits 0. Diagnostics only; the WARN stays | yes — every mzPeak lane that honours the cap writes `metadata.partial` = `{partial: true, max_spectra, source_declared, spectra_written, cause: "MZPC_MAX_SPECTRA"}` when the cap bit (0.9.13); a cap larger than the file writes no marker |

**Performance / diagnostic.** No effect on the values written (bytes only where noted); zero cost
when unset.

| Variable | Effect |
|---|---|
| `MZPC_BUFFER_SPECTRA=<n>` | Spectra buffered in RAM before the writer flushes a row group (default 256) on the standard f64 paths |
| `MZPC_DECODE_WINDOW=<n>` | Bounded reorder window for the parallel timsTOF decoder (default 8× rayon threads, capped at 128). Output order — and therefore the bytes — is unchanged |
| `MZPC_ROW_GROUP_ROWS=<n>` | Peak-facet parquet row-group size in rows (default 8192 chunks/group on chunked facets, parquet's 2^20 otherwise). Trades size against per-frame random access |
| `MZPC_ENCODE_THREADS=<n>` | Vendored writer: worker threads for the parallel peak-facet encode (default `available_parallelism()`; `RAYON_NUM_THREADS` is honoured as a fallback; `0` or a non-number is ignored). Output is byte-identical at any thread count |
| `MZPC_ENCODE_INFLIGHT_BYTES=<bytes>` | Vendored writer: byte budget for row groups in flight in that parallel encode (default `max(256 MB, threads × 48 MB)`); bounds memory, never the bytes written |
| `MZPC_PARALLEL_ENCODE=0` | Vendored writer: serial peak-facet encode instead of the parallel default (on for every unencrypted archive; encrypted facets are always serial). Output is byte-identical either way. Its own rule: empty or `0` = off, any other value (including `false`) = on |
| `MZPC_FLUSH_MEM_MB=<MB>` | Vendored writer: flush the in-RAM array buffers once they exceed this many MB (default 128), independent of spectrum or point counts. Changes row-group boundaries on the standard f64 facets, so the bytes — not the values — can differ |
| `MZPC_TIMING=1` | Log decode-vs-write busy times for the pipelined timsTOF path (`env_flag` in `src/`; the vendored encoder's own timing line uses empty-or-`0` = off) |
| `MZPC_SHIMADZU_DEBUG=1` | Shimadzu glue: trace scan-count discovery on stderr (read by the C# glue: empty or `0` = off, anything else on) |
| `MZPC_SHIMADZU_PROBE=<n>` | Shimadzu `.lcd`: print the first `n` spectra as JSON lines and exit **without writing an archive**. Since 0.9.13 it is handled before lane selection (`src/main.rs:917`), so it works without `-o` — it used to live inside the Shimadzu lane, which only runs with `-o`, and therefore always swallowed the requested archive; a value that is not a count (including empty) is an error rather than 10; with `-o` it refuses; on macOS/Linux, where the reader does not exist, it is an error rather than silently ignored |
| `MZPC_DUMP_IM_TABLE=1` | Bruker TDF: dump the scan→1/K0 table (timsrust, and the SDK where available) and exit without converting. Refuses with `-o` |
| `MZPC_DUMP_AGILENT_PROFILE=1` | Agilent: dump decoded profile spectra (sum, nnz, first/last `(k,v)`, max `v`) and exit without converting. Refuses with `-o` |
| `MZPC_TDF_SDK_GOLDEN=<out.json>` | Bruker TDF, `--bruker-sdk` only (Windows/Linux): diagnostic dump of the SDK's `tims_index_to_mz` at up to 240 `(frame, tof)` points — frame 1, the last frame and 10 evenly spaced frames × 20 tof values over `0..DigitizerNumSamples−1` — as `{file, digitizer_num_samples, mz_calibration, points: [{frame, t1, t2, cal_id, tof, mz_sdk}]}`, the ground truth for the ModelType-1 model and the per-spectrum `tof_c0`/`tof_c1` (§8). An empty value is unset; a bad path or an SDK refusal is logged and never fails the conversion |

The harness under `tools/` reads its own `MZPC_*` names (`MZPC_PYTHON`, `MZPC_BOX_*`,
`MZPC_NO_S3_SOURCE`, `MZPC_FETCH_JOBS`, `MZPC_ALLOW_PARALLEL`, `MZPC_BENCH_MZML`); they never reach
the converter binary and are documented in the scripts themselves.

## 11. Native vendor-SDK readers

The Agilent (MHDAC), SciEX (Clearcore2), Shimadzu (LabSolutions.IO) and Bruker BAF
(libbaf2sql_c) readers are **compiled in automatically** on the platforms where those
vendor libraries exist — Windows for all four, Linux also for Bruker BAF. There is **no build flag** and no
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
DLL is loaded from `$MZPC_PWIZ_DIR` by reflection — see `glue/shimadzu/README.md`).
Both ProteoWizard layouts work: the MHDAC/Clearcore2 assemblies may sit under
`vendor_api/Agilent` / `vendor_api/ABI` (the bundled builds) or flat beside `msconvert.exe`
(the standalone installer); the Agilent lane probes both, subdirectory first. Shimadzu's
`Shimadzu.LabSolutions.IO.IoModule.dll` is always flat.

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
reference writer `mzpeak_prototyping` is vendored under `vendor/`. A complete
inventory of all transitive dependencies (with licenses) is in
[`sbom.cdx.json`](../sbom.cdx.json); see [THIRD-PARTY-NOTICES.md](../THIRD-PARTY-NOTICES.md).

## 13. Troubleshooting

| Symptom | Fix |
|---|---|
| Thermo `.raw` fails to open | install a .NET 8+ runtime |
| `--via-msconvert` not found | install ProteoWizard or set `--msconvert-path`/`$MSCONVERT_PATH` |
| Agilent/SciEX exits with code 3 | no native reader for that format on this platform (macOS/Linux); use `--via-msconvert` |
| Agilent `.d`: `holds MRM/SIM dwell data only` | the native lane stores scan spectra; MRM/SIM dwells are transition chromatograms — use `--via-msconvert` (the box harness does this on its own) |
| Agilent `.d`: `is an Agilent IM-QTOF run` | the drift dimension needs the MIDAC lane, which is not available — use `--via-msconvert` |
| Agilent `.d`: `output is the AGL1 format of an older AgilentGlueHost.exe` | rebuild `glue/agilent` (`dotnet build -c Release`) so the host and the converter agree |
| Nothing was written | give `-o/--output`; without it the run only inspects |
| Output exists error | pass `--force` to overwrite |
| UV/PDA spectra missing | non-MS spectra are not yet carried (known limitation) |
