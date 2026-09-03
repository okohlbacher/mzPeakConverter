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
| `--no-mz-lattice` | off | Disable the fixed-point m/z **lattice** for centroid peaks (see §9) and store f64 `mz` instead — on every lane, the native Shimadzu `.lcd` one included. Also `MZPC_NO_MZ_LATTICE=1`. No effect on data that is not on a lattice |
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

**Exact per-spectrum coefficients when `C2 = 0`.** When every `MzCalibration` row a run
references is ModelType 1 with `C2 = C3 = C4 = dC2 = 0` — stored as numeric zeros; a NULL or
text cell is a *missing* term, not a zero, and keeps the run on the chord (PXD059079 2485.d is
such a file), the vendor model is *exactly* a sqrt-linear law in `tof` per frame:
`m/z = (tof_c0 + tof_c1·tof)²` with `tof_c1 = DigitizerTimebase·√C1_eff/1e6`,
`tof_c0 = (DigitizerDelay − C0)·√C1_eff/1e6` and `C1_eff` temperature-corrected with the
frame's `Frames.T1`. Both ims-compact lanes (native and `--bruker-sdk`) then write the pair as
per-spectrum Float64 columns `…_tof_c0` / `…_tof_c1` in `spectra_metadata` (the same columns
and accessions the SciEX/Agilent/Shimadzu sqrt grids use), stamp the `tof` column with
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
Version **5.0.0.0** — shipped by a current ProteoWizard (3.0.26151 verified) — reads the same
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
| `RUST_LOG` | log filter (overridden by `-v`/`-q`) |
| `DOTNET_ROLL_FORWARD` | set automatically to `LatestMajor` if unset (Thermo) |
| `MZDATA_IGNORE_UNKNOWN_INSTRUMENT` | set automatically to `ignore` if unset |
| `MSCONVERT_PATH` | `msconvert` location for `--via-msconvert` |

Every `MZPC_*` variable the converter reads is listed below. They fall into three groups:
**deployment** (where the vendor libraries and glue live — you will set these on a Windows
conversion host), **output-affecting** (they change what is written, and are not recorded in the
archive — prefer the equivalent CLI flag where one exists) and **diagnostic** (they dump or trace
and are not for production runs).

**Deployment (§11).**

| Variable | Effect |
|---|---|
| `MZPC_PWIZ_DIR` | ProteoWizard install supplying the vendor DLLs at runtime (Agilent MHDAC, SciEX Clearcore2, Shimadzu LabSolutions.IO, Waters MassLynx). Both layouts are probed — `vendor_api/<Vendor>` and flat beside `msconvert.exe`. **Use a current ProteoWizard** (3.0.26151 verified); see §11 for why an old one silently corrupts Shimadzu centroids |
| `MZPC_MASSLYNX_DIR` | Directory holding `MassLynxRaw.dll` for the Waters lane. Wins over `MZPC_PWIZ_DIR`, which is the fallback |
| `MZPC_AGILENT_GLUE` | Directory holding the built `AgilentGlueHost.exe` (net48) |
| `MZPC_AGILENT_MIDAC_GLUE` | Directory holding the built `AgilentMidacGlue.dll` + runtimeconfig (Agilent ion mobility) |
| `MZPC_SCIEX_GLUE` | Directory holding the built `SciexGlue.dll` + runtimeconfig |
| `MZPC_SHIMADZU_GLUE` | Directory holding the built `ShimadzuGlue.dll` + runtimeconfig |

**Output-affecting.** These change the bytes that are written and leave no trace in the archive;
where a CLI flag exists, use it instead so the run is reproducible from its command line.

| Variable | Effect |
|---|---|
| `MZPC_NO_MZ_LATTICE=1` | Same as `--no-mz-lattice` (§9): store f64 `mz` instead of the fixed-point lattice, on every lane |
| `MZPC_SHIMADZU_COARSE_MZ=1` | Shimadzu: read the coarse 1e-4 `Mass` instead of `MassHigh` (§8) |
| `MZPC_BYTE_PLANE_INTENSITY=0` | Opt out of Int32 byte-plane intensity (on by default for timsTOF ims-compact) back to f32 |
| `MZPC_TOF_GRID_PPM=<ppm>` | `--tof-grid` reconstruction tolerance (default 5.0). The lane is bounded-lossy and this number **is** the bound — raising it above the instrument's mass accuracy is not defensible |
| `MZPC_TOF_GRID_C1=<step>` | `--tof-grid`: force the sqrt-space step instead of inferring it (`c1 = quantum / (2·√mz_max)`). Logged as a warning when set |
| `MZPC_MAX_SPECTRA=<n>` | Stop after `n` spectra. **Deliberately truncating**: it also disables the "all source spectra written" completeness check, so the archive is a partial one that exits 0. Diagnostics only |

**Performance / diagnostic.** No effect on the bytes written (except as noted), zero cost when unset.

| Variable | Effect |
|---|---|
| `MZPC_BUFFER_SPECTRA=<n>` | Spectra buffered in RAM before the writer flushes a row group (default 256) on the standard f64 paths |
| `MZPC_DECODE_WINDOW=<n>` | Bounded reorder window for the parallel timsTOF decoder (default 8× rayon threads, capped at 128). Output order — and therefore the bytes — is unchanged |
| `MZPC_ROW_GROUP_ROWS=<n>` | Peak-facet parquet row-group size in rows (default 8192 chunks/group on chunked facets, parquet's 2^20 otherwise). Trades size against per-frame random access |
| `MZPC_TIMING=1` | Log decode-vs-write busy times for the pipelined timsTOF path |
| `MZPC_SHIMADZU_DEBUG=1` | Shimadzu glue: trace scan-count discovery on stderr |
| `MZPC_SHIMADZU_PROBE=<n>` | Shimadzu: dump the first `n` spectra as JSON lines and exit **without writing an archive** |
| `MZPC_DUMP_IM_TABLE=1` | Bruker TDF: dump the scan→1/K0 table (timsrust, and the SDK where available) and exit without converting |
| `MZPC_DUMP_AGILENT_PROFILE=1` | Agilent: dump decoded profile spectra (sum, nnz, first/last `(k,v)`, max `v`) and exit without converting |
| `MZPC_TDF_SDK_GOLDEN=<out.json>` | Bruker TDF, `--bruker-sdk` only (Windows/Linux): diagnostic dump of the SDK's `tims_index_to_mz` at up to 240 `(frame, tof)` points — frame 1, the last frame and 10 evenly spaced frames × 20 tof values over `0..DigitizerNumSamples−1` — as `{file, digitizer_num_samples, mz_calibration, points: [{frame, t1, t2, cal_id, tof, mz_sdk}]}`, the ground truth for the ModelType-1 model and the per-spectrum `tof_c0`/`tof_c1` (§8). A bad path or an SDK refusal is logged and never fails the conversion |

## 11. Native vendor-SDK readers

The Agilent (MHDAC), SciEX (Clearcore2), Shimadzu (LabSolutions.IO) and Bruker BAF
(libbaf2sql_c) readers are **compiled in automatically** on the platforms where those
vendor libraries exist — Windows for all four, Linux also for Bruker BAF. There is **no build flag** and no
opt-in; macOS gets none (no vendor SDKs exist there).

They load the proprietary vendor DLLs at **runtime**, sourced from a ProteoWizard
install: point `$MZPC_PWIZ_DIR` at it, and for the .NET glues set `$MZPC_AGILENT_GLUE` /
`$MZPC_SCIEX_GLUE` / `$MZPC_SHIMADZU_GLUE` to the built C# glue dir (`dotnet build
glue/agilent/AgilentGlue.csproj`, likewise `glue/shimadzu/ShimadzuGlue.csproj`; the Shimadzu
DLL is loaded from `$MZPC_PWIZ_DIR` by reflection — see `glue/shimadzu/README.md`).
Both ProteoWizard layouts work: the MHDAC/Clearcore2 assemblies may sit under
`vendor_api/Agilent` / `vendor_api/ABI` (the bundled builds) or flat beside `msconvert.exe`
(the standalone installer); the Agilent lane probes both, subdirectory first. Shimadzu's
`Shimadzu.LabSolutions.IO.IoModule.dll` is always flat.

**Which ProteoWizard: use a current one.** 3.0.26151 is verified, and anything that ships
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
| Agilent/SciEX exits with code 3 | expected without the build feature; use `--via-msconvert` |
| Nothing was written | give `-o/--output`; without it the run only inspects |
| Output exists error | pass `--force` to overwrite |
| UV/PDA spectra missing | non-MS spectra are not yet carried (known limitation) |
