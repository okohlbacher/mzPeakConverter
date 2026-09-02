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
  archive (a STORED ZIP of Apache Parquet facets + a JSON index) that is lossless,
  columnar, and analysis-ready, preserving vendor metadata and ion-mobility structure.
- **Without `--output`** — writes nothing; it just **inspects** the input and prints
  a report (format, spectrum count, chromatogram count).

Passing `-v` prints that same inspection report *and still performs the conversion*.

## 2. Installation & requirements

| Requirement | Notes |
|---|---|
| Rust ≥ 1.87 | edition 2024; install via <https://rustup.rs> |
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

| Option | Default | Description |
|---|---|---|
| `<INPUT>` | — | Input file or vendor directory (positional, required) |
| `-o, --output <PATH>` | *(none → inspect only)* | Output `.mzpeak` archive; if omitted, nothing is written |
| `-c, --config <FILE>` | — | YAML config file setting defaults for any option (see §5) |
| `--layout <chunked\|point>` | `chunked` | Signal layout (see §9) |
| `--no-numpress` | off | Lossless delta m/z chunking instead of lossy numpress-linear |
| `--chunk-size <Th>` | `50` | m/z chunk width for the chunked layout |
| `--zstd-level <1–22>` | `3` | Parquet zstd level |
| `--no-ims-compact` | off | Bruker TDF: write standard f64 m/z instead of the default ims-compact |
| `--ims-chunked` | off | Bruker TDF ims-compact: opt into the **chunked** layout (m/z-page-prunable, fast XIC/slice) instead of the default **archive** layout (see §9) |
| `--bruker-sdk` | off | Read Bruker TDF/TSF via the official `timsdata` SDK (Win/Linux only; needs `TIMSDATA_LIB_DIR`). Parallel path to the default pure-Rust readers; on TDF still writes ims-compact — add `--no-ims-compact` for f64 m/z |
| `--no-tims-recalibration` | off | Bruker TDF: disable vendor-grade scan→1/K0 recalibration (`TimsCalibration` ModelType-2 model) and use timsrust's linear approximation. Recalibration is **on by default** (~22× closer to the SDK). Governs the ims-compact lane's mobility arrays and the precursor/scan/window 1/K0 params of BOTH lanes (they agree either way); with `--no-ims-compact` the mobility *arrays* come from mzdata's own ModelType-2 calibration and cannot be switched |
| `--representation <both\|profile\|centroid>` | `both` | When a vendor supplies BOTH profile and centroid for a spectrum (Shimadzu `.lcd`): `both` writes profile to `spectra_data` and centroids to `spectra_peaks`; `profile` / `centroid` keep one. A representation the file lacks is a warning, the other one is written |
| `--no-vendor` | off | Do not embed vendor side-files (see §8) |
| `--aux <glob=embed\|drop>` | — | Vendor side-file rule (repeatable, highest precedence) |
| `--via-msconvert` | off | Read via ProteoWizard `msconvert` → mzML → mzPeak |
| `--msconvert-path <PATH>` | `$MSCONVERT_PATH` / PATH | Location of `msconvert` |
| `-f, --force` | off | Overwrite an existing output |
| `-v, --verbose` | off | Print the inspection report (repeat `-vv` for trace logs) |
| `-q, --quiet` | off | Silence all logs except errors |

## 5. Configuration file

`--config <file.yaml>` loads a configuration file that can set **any** of the
options above — it is a general configuration, not a vendor-only concern. Every
key is optional. Precedence is:

> **explicit command-line flag → config-file value → built-in default**

(Boolean switches such as `no_numpress` are enable-only: a config value of `true`
or the corresponding flag turns them on.)

```yaml
# mzpeak-convert.yaml — every overridable option, all optional
output: out.mzpeak
layout: chunked            # or: point
no_numpress: false
chunk_size: 50
zstd_level: 9
no_ims_compact: false      # TDF: keep the lossless ims-compact default
no_vendor: false
aux:                       # vendor side-file rules (see §8)
  - "*.tdf_bin=drop"
  - "*.method=embed"
via_msconvert: false
msconvert_path: /opt/pwiz/msconvert
force: true
```

```sh
mzpeak-convert run.d -c mzpeak-convert.yaml          # uses the file's settings
mzpeak-convert run.d -c mzpeak-convert.yaml --zstd-level 3   # CLI overrides zstd_level
```

Unknown keys are rejected with a clear error.

## 6. Supported formats & operating systems

| Format | Linux | macOS | Windows | Notes |
|---|:---:|:---:|:---:|---|
| mzML, `.mzML.gz` | ✅ | ✅ | ✅ | full metadata + chromatograms |
| imzML | ✅ | ✅ | ✅ | imaging coordinate columns; IMS CV promoted |
| Bruker `.d` **TDF** (timsTOF) | ✅ | ✅ | ✅ | ion mobility; **ims-compact by default** |
| Bruker `.d` **TSF** (line spectra) | ✅ | ✅ | ✅ | MALDI/TOF; otofControl m/z correction |
| Thermo `.raw` | ✅ | ✅ | ✅ | needs a **.NET 8+ runtime** |
| Bruker `.d` **BAF** | ✅ | ❌ | ✅ | auto-built; needs `libbaf2sql_c` at runtime |
| Agilent `.d` (native) | ❌ | ❌ | ✅ | auto-built; MHDAC DLLs at runtime |
| SciEX `.wiff` (native) | ❌ | ❌ | ✅ | auto-built; Clearcore2 DLLs at runtime |
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
  `ims_calibration` (for ims-compact), declared file entries.
- `spectra_metadata.parquet` — per-spectrum descriptors (id, index, MS level,
  polarity, scan time, precursor info, …).
- `spectra_data.parquet` / `spectra_peaks.parquet` — signal arrays (chunked/point).
- `chromatograms.parquet` — TIC/BPC/SRM and other chromatograms.
- `vendor/…` — embedded original side-files (optional, see §8).

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

**Mapped metadata (into the archive's typed columns).** Where a vendor value has a
PSI controlled-vocabulary meaning, it is mapped onto the standard
`spectra_metadata` columns — MS level, polarity, scan start time, precursor m/z /
charge / isolation window, ion-mobility (`mean inverse reduced ion mobility` for
TDF), and the `MS:1000294` spectrum-type. Bruker TSF/BAF m/z is produced from the
vendor calibration (TSF applies the otofControl ±Th correction); Bruker TDF stores
the native integer TOF grid plus the `a,b` calibration in `ims_calibration` so a
reader reconstructs `m/z = (a + b·tof)²` exactly. That `a,b` is timsrust's two-point
chord (−5…−11 ppm against the vendor SDK), so the archive also carries the vendor's
**exact** calibration: the `vendor_mz_calibration` index block holds every
`analysis.tdf` `MzCalibration` row verbatim plus `DigitizerNumSamples` /
`MzAcqRangeLower` / `MzAcqRangeUpper`, and `spectra_metadata` gains per-frame
`…_tdf_t1`, `…_tdf_t2`, `…_tdf_mz_calibration_id` columns (`Frames.T1/T2/MzCalibration`).
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
long as it does. One vendor defect to know about: for a spectrum that carries **no profile
signal**, the vendor API returns a centroid list whose intensities are shifted against
their m/z by 1–7 positions with the last peak missing — msconvert, reading the same DLL,
writes byte-identical data. The converter stores those peaks exactly as returned and logs
one warning per file naming the defect; files whose spectra carry profile signal are
bit-exact through every path. For scientifically correct centroids from a profile-less
`.lcd`, use a LabSolutions mzML export (its exporter takes a different path and is exact).
See `glue/shimadzu/README.md` for the measurements.

**Profile zero-run compaction (the one transform that is not byte-for-byte).** In **profile**
spectra, a run of two or more *consecutive* zero-intensity points is collapsed to a single zero at
each peak boundary — `[0,0,0,0,0, 900, 500, 0,0,0, 300, 0]` (12 points) is stored as
`[0, 900, 500, 0, 0, 300, 0]` (7). The baseline extent of every peak is preserved, so the profile
shape is unchanged, but `number_of_data_points` reflects the stored count rather than the source's.
**Centroid spectra are never touched** — isolated and interior zero-intensity centroids round-trip
exactly. Everything else (m/z at full f64 precision, intensity, retention time, precursor m/z and
charge) round-trips bit-for-bit; verified against mzdata's own mzML output on a 4,880-spectrum DDA
run with zero differences.

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
- **zstd** — applied inside Parquet, `--zstd-level` 1–22 (default 3).
- **ims-compact** — for Bruker timsTOF (**TDF**) this is the **default**: the
  native integer `tof` is stored bit-exact (Int32 + `ims_calibration`) instead of
  f64 m/z, roughly halving the m/z bytes with an exact grid. Disable with
  `--no-ims-compact` to write standard f64 m/z. m/z is reconstructed by readers as
  `m/z = (a + b·tof)²` — the chord, marked `"exact": false`; the vendor's exact model sits
  beside it in `vendor_mz_calibration` (§8).
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
    a chunk's absolute TOF by cumulative-summing its values. Whole-spectrum access matches archive when
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
    i.e. `m/z = 1e-9 · tof_index`, plus the f64 `point.mz` fallback (NULL on lattice rows) and
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
| `RUST_LOG` | log filter (overridden by `-v`/`-q`) |
| `DOTNET_ROLL_FORWARD` | set automatically to `LatestMajor` if unset (Thermo) |
| `MZDATA_IGNORE_UNKNOWN_INSTRUMENT` | set automatically to `ignore` if unset |
| `MSCONVERT_PATH` | `msconvert` location for `--via-msconvert` |
| `MZPC_PWIZ_DIR`, `MZPC_SCIEX_GLUE`, `MZPC_AGILENT_GLUE`, `MZPC_SHIMADZU_GLUE` | native vendor-SDK runtime (§11) |
| `MZPC_SHIMADZU_COARSE_MZ=1` | Shimadzu: read the coarse 1e-4 `Mass` instead of `MassHigh` (§8) |

## 11. Native vendor-SDK readers

The Agilent (MHDAC), SciEX (Clearcore2), Shimadzu (LabSolutions.IO) and Bruker BAF
(libbaf2sql_c) readers are **compiled in automatically** on the platforms where those
vendor libraries exist — Windows for all four, Linux also for Bruker BAF. There is **no build flag** and no
opt-in; macOS gets none (no vendor SDKs exist there).

They load the proprietary vendor DLLs at **runtime**, sourced from a ProteoWizard
install: point `$MZPC_PWIZ_DIR` at it (MHDAC under `vendor_api/Agilent`, Clearcore2
under `vendor_api/ABI`), and for the .NET glues set `$MZPC_AGILENT_GLUE` /
`$MZPC_SCIEX_GLUE` / `$MZPC_SHIMADZU_GLUE` to the built C# glue dir (`dotnet build
glue/agilent/AgilentGlue.csproj`, likewise `glue/shimadzu/ShimadzuGlue.csproj`; the Shimadzu
DLL is loaded from `$MZPC_PWIZ_DIR` by reflection — see `glue/shimadzu/README.md`).
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
| Agilent/SciEX exits with code 3 | expected without the build feature; use `--via-msconvert` |
| Nothing was written | give `-o/--output`; without it the run only inspects |
| Output exists error | pass `--force` to overwrite |
| UV/PDA spectra missing | non-MS spectra are not yet carried (known limitation) |
