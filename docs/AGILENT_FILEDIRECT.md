# Agilent Q-TOF `.d` — file-direct grid-TOF reader

> **2026-06-24 — END-TO-END, FIRST-PARTY, ON A REAL PROFILE `.d`.** The pure-Rust reader
> (`src/agilent_profile.rs` + `convert_agilent_grid` in `src/main.rs`, gated `--agilent-grid`) is
> implemented and measured on a real profile-mode Agilent Q-TOF `.d` — **no MHDAC, no msconvert, no
> synthetic data**. Headline results (single `.d`, 568 spectra, MTBLS1334):
>
> | | bytes | ratio |
> |---|---|---|
> | **(A)** vendor `.d` (AcqData) | 8,228,609 | 1.00× |
> | **(B)** msconvert lane mzPeak (f64 m/z) | 5,168,537 | B/A 0.628× |
> | **(C)** file-direct mzPeak (`tof_index`) | **1,163,592** | **C/A 0.141×**, **C/B 0.225×** |
>
> - **Lossless:** reading the stored mzPeak back and reconstructing m/z via the index-block formula
>   matches rainbow/MassHunter to **7.8e-10 ppm** (floating-point noise); **integer intensities
>   EXACT** (max diff 0). 74,933 points across 7 spectra verified.
> - **Parser validated byte-exact** vs the `rainbow` reference on the real file (per-scan nnz / sum /
>   first / last / max intensity all identical; ScanTime / MSLevel identical).
> - **Polynomial refinement matters:** this `.d`'s `DefaultMassCal.xml` has an active polynomial
>   (`ValueUseFlags=240`), a real up-to-7.5 ppm correction over the run. The bare 2-coeff grid is
>   therefore NOT lossless vs MassHunter; the reader stores the per-`CalibrationID` polynomial in the
>   `tof_calibration` index block + per-spectrum `tof_c0`/`tof_c1`/`tof_calibration_id` columns and
>   reconstructs `t = base + (c0+c1·k)/coeff`, `m/z = (coeff·(t−base))² − poly(clip(t,left,right))`
>   EXACTLY. (Agilent's `coeff`/`base` also drift per scan — a single run-wide grid is ~100 ppm off —
>   so the calibration is genuinely per-spectrum; the columns compress to ~nothing.)
> - **The profile `.d`:** MetaboLights **MTBLS1334** / `STD_neg_MSMS_1min0124.d` (Agilent 6545
>   Q-TOF, neg-mode DI-MS lipid standards), 8.2 MB `.d.zip`, `AcqData/MSProfile.bin` = 7.08 MB,
>   **LZF-compressed** u32 intensities (not 0x90 RLE — the Rust reader handles both). Calibration via
>   `DefaultMassCal.xml` (no `MSMassCal.bin`). 568 spectra (1 MS1 + 567 MS2).
> - **Files changed:** `src/agilent_profile.rs` (new, pure-Rust reader: MSScan.xsd/.bin parser, LZF +
>   0x90-RLE decoders, MSMassCal.bin / DefaultMassCal.xml calibration, closed-form grid mapping);
>   `src/main.rs` (`convert_agilent_grid`, `--agilent-grid` flag + config, dispatch before the
>   vendor guard, `MZPC_DUMP_AGILENT_PROFILE` diagnostic). `cargo test --release` 11/11 green.
> - **Remaining work:** Windows cross-check vs MHDAC m/z on the same file (the rainbow comparison is
>   already the MHDAC algorithm, so this is confirmatory); broaden the corpus (MTBLS1622 etc.); an
>   Int32 intensity column would shrink the intensity term further (here f32 is already bit-exact,
>   max count 109,160 ≪ 2²⁴). A standard mzPeak reader needs to learn the per-spectrum
>   `tof_c0/tof_c1/tof_calibration_id` + index-block polynomial convention to reconstruct m/z.
>
> The original spike write-up (synthetic-profile projection that motivated this) follows.

---

# Agilent Q-TOF `.d` — file-direct grid-TOF reader (spike)

**Date:** 2026-06-24 · **Platform:** macOS (Python spike; Rust integration plan) ·
**Test data:** PXD041903 `20190423_Alex7.d` (Agilent 6500-series Q-TOF, MassHunter
B.06.01), 10,730 scans · **Format authority:** `rainbow` (evanyeyeye/rainbow,
`rainbow/agilent/masshunter.py`), cross-checked byte-for-byte by a round-trip encoder.

---

## TL;DR

- **File-direct Agilent is a real, large win — but only for PROFILE (`MSProfile.bin`)
  acquisitions.** Storing the integer flight-time index (`tof_index : Int32`,
  DELTA_BINARY_PACKED) instead of msconvert's expanded f64 m/z shrinks the m/z axis from
  **7.31 → 0.27 B/peak (≈27×)**, taking a profile spectrum from **8.23 → 1.19 B/peak**.
  Measured (faithful synthetic profile built from the real `.d`'s centroid TOF +
  calibration, 60 spectra, 7.2 bins/peak, zstd-3):
  - **file-direct vs msconvert lane: C/B = 0.144×** (≈7× smaller)
  - **file-direct vs the raw Agilent `.d` itself: C/A = 0.556×** — **decisively beats the
    0.97× target** (mzPeak ≈ 1.8× smaller than the vendor file).
  This is the same mechanism and ~0.2–0.3 B/peak m/z column the SCIEX `tof_grid.rs`
  path already achieves.
- **The model is exactly the proposal's grid-TOF.** Agilent stores `sqrt(m/z)` on an
  affine integer flight-time grid with a 2-coefficient per-run quadratic
  `m/z = (coeff·(t − base))²` — identical in form to `tof_grid.rs`'s
  `sqrt(m/z) = c0 + c1·k`. No new math is needed; the existing writer path is reusable.
- **Blocker for an end-to-end measurement:** the PXD041903 `.d` is **centroid-only**
  (`MSProfile.bin` = **0 bytes**; all data in `MSPeak.bin`). MassHunter ships profile and
  centroid separately and users routinely delete `MSProfile.bin` to save disk — so this
  particular file cannot exercise the profile lane. The centroid lane (`MSPeak.bin`)
  stores **float64 raw-TOF + float32 intensity** with sub-bin-refined apex positions that
  do **not** sit on a clean integer lattice and **non-integer** intensities — so the
  file-direct grid win does **not** apply to centroid `.d`; that data is already close to
  what msconvert emits.

---

## 1. What `MSProfile.bin` / `MSMassCal.bin` actually contain (byte layout)

Confirmed against `rainbow/agilent/masshunter.py` and validated with a byte-exact
round-trip encoder (`spike/agilent/msprofile_codec.py`, 200/200 random profiles
re-decode identically through rainbow's `decompress_inten_list`).

### `MSScan.bin` (+ `MSScan.xsd`)
Per-scan records (schema in the `.xsd`); each gives `ScanTime`, `PointCount`,
`SpectrumOffset`, `ByteCount`, `UncompressedByteCount`, `CalibrationID`. A Q-TOF that
stores both profile and centroid writes **two** `SpectrumParamValues` blocks per record
(profile block + centroid block); the record stride must be inferred from the file
geometry to stay aligned (rainbow `read_scan_records`).

### `MSProfile.bin` data segment (one per retention time)
- **Header (16 bytes):** two little-endian `float64` = `(m/z_min, m/z_delta)` —
  the raw time-of-flight axis origin and step **before calibration** (NOT m/z).
- **Intensity stream**, one of two encodings:
  - **LZF** (generic / HRMS): the whole segment (header included) is LZF-compressed;
    after decompression the intensities are a contiguous block of **`uint32`**.
  - **0x90 RLE** (Q-TOF profile, the case of interest): 16-byte header left RAW, then:
    - 4-byte word: low 3 bytes = point count, high byte = `0x90` marker.
    - two negated `int32`: initial leading-zero-run length, and a width flag ∈ {1,2,3,4}
      → {1,2,4,8}-byte signed ints.
    - values at the current width: `≥0` = literal intensity; `<0` → `divmod(-v,4)` =
      (zero-run length, new width flag). Trailing zeros are not stored.
  - **Intensities are UNSIGNED INTEGERS** (detector counts).

### `MSMassCal.bin`
- 10 little-endian `float64` per scan from offset `0x4c`, stride 84 (rainbow):
  `[coeff, base, left, right, c0..c5]`.
- **Traditional cal:** `m/z = (coeff·(t − base))²` where `t` is the raw TOF for a point.
- Optional **polynomial refinement** (`DefaultMassCal.xml` `ValueUseFlags`, e.g. 156 in
  this file) subtracted from the result; sub-ppm correction.

### Consequence — this IS the proposal's grid-TOF
The grid ordinal `i` (profile point index) maps to raw TOF `t = m/z_min + i·m/z_delta`
(affine), and `sqrt(m/z)` is then a 2-coefficient function of `i`. So a profile spectrum
is **already** {integer index, integer intensity, per-run quadratic} — store the index
(or omit for a dense grid) + the two coefficients and m/z reconstructs exactly. That is
precisely `tof_grid.rs`'s `sqrt(m/z) = c0 + c1·k` reading of Agilent's quadratic.

---

## 2. Does file-direct beat the baseline? (measured)

We could not run the profile lane on PXD041903 (`MSProfile.bin` empty), so we built a
**faithful synthetic profile run from the real `.d`**: take the real centroid TOF + the
real `MSMassCal.bin` quadratic, lay each centroid onto the instrument integer flight-time
grid as a 7-tap profile peak with integer intensities, and encode it with the
byte-validated `MSProfile.bin` codec. Then compare three on-disk forms of the SAME
profile points (`spike/agilent/measure_filedirect.py`, zstd-3 columnar):

Measured: 60 spectra, 609,152 profile points, 7.2 bins/peak, zstd-3.

| Representation | m/z axis | intensity | **B/peak** | vs `.d` (A) | vs msconvert (B) |
|---|---|---|---|---|---|
| **(A) raw Agilent `.d`** | f64 grid header + RLE int | RLE uint32 | **2.136** | 1.00× | — |
| **(B) msconvert lane** | f64 m/z | f32 | **8.226** (mz 7.311 + int 0.915) | 3.85× | 1.00× |
| **(C) file-direct mzPeak** | `tof_index` Int32, ΔBP | f32 | **1.187** (tof 0.271 + int 0.915) | **0.556×** | **0.144×** |

**The m/z axis collapses 7.31 → 0.27 B/peak (≈27×).** Two headline ratios:
- **C/B = 0.144×** — file-direct is ~7× smaller than mzPeak's own msconvert (f64-m/z) lane.
- **C/A = 0.556×** — file-direct mzPeak is **~1.8× smaller than the vendor `.d` itself**,
  decisively under the **0.97× target**. (And B/A = 3.85× shows msconvert *inflates* the
  `.d` ~4× by expanding the integer grid to f64 m/z — the lane file-direct replaces.)

The intensity column here is f32 (baseline-parity); Agilent profile intensities are
integers, so an Int32/64 ΔBP intensity column would shrink the 0.915 B/peak term too —
unmeasured additional headroom. Note even the f32 intensity column (0.915) already beats
the `.d`'s RLE-integer intensity share, which is why C/A < 1 despite the `.d` being a
fairly compact format.

### Losslessness
Reconstruction `m/z = (c0 + c1·k)²` is lossless to the grid-snap quantization bound that
`tof_grid.rs` already gates at `PPM_TOL = 3 ppm` (and auto-refines finer if needed).
Profile intensities are integers in the file, so an Int32/Int64 intensity column is exact;
the f32 column used above is the conservative (baseline-parity) choice.

---

## 3. Rust integration plan

The reusable writer path **already exists** on `ci/corpus-vendor-ci` (rebased into this
worktree): `src/tof_grid.rs` (the `sqrt(m/z)=c0+c1·k` fit + `TofGrid`) and
`convert_file_tof_grid()` in `src/main.rs` (custom `point` facet with a nonstandard
`tof_index : Int32` column carrying the `SqrtMzFromTof` transform + `[c0,c1]` metadata,
landed as DELTA_BINARY_PACKED; writes a `tof_calibration` index block). **The new work is
only the Agilent FILE PARSER that feeds (tof_index, intensity) into that path** —
bypassing MHDAC, which expands the grid to f64.

Concretely:

1. **`src/agilent_profile.rs` (new):** a native reader for `MSProfile.bin` that, for each
   scan, reads the `MSScan.bin` record (offset/len/PointCount/CalibrationID), reads the
   `(m/z_min, m/z_delta)` header, decodes the intensity stream (LZF or 0x90 RLE), and
   reads the per-scan 10 doubles from `MSMassCal.bin`. Pure Rust, **no SDK, cross-platform**
   (this is the whole point — unlike `src/agilent.rs`, which hosts .NET/MHDAC on Windows).
   The 0x90 RLE decoder is ~40 lines (mirror `decompress_inten_list`); LZF is a small
   crate or ~30 lines.
2. **Grid handoff:** the per-scan header already gives the integer grid `k = i`
   (profile point index) and the quadratic `(coeff, base, …)`. Convert Agilent's
   `m/z = (coeff·(t−base))²` with `t = m/z_min + k·m/z_delta` to the `tof_grid.rs`
   `sqrt(m/z) = c0 + c1·k` form (one affine substitution: `c1 = coeff·m/z_delta`,
   `c0 = coeff·(m/z_min − base)`), yielding a `TofGrid{c0,c1}` directly — **no fitting
   needed**, the coefficients are read from the file. **Verified exact** against this `.d`'s
   real calibration (`spike/agilent/verify_mapping.py`: closed-form vs Agilent's traditional
   formula = 6.5e-10 ppm = floating-point noise). (Optionally still run `tof_grid::fit` on a
   few decoded spectra to validate.) The optional polynomial refinement
   (`ValueUseFlags`, sub-ppm) is the one piece not captured by the 2-coeff form — either
   absorb it into the 3 ppm gate or add a small polynomial term to the calibration block.
3. **Writer:** route to a thin `convert_agilent_grid()` that mirrors
   `convert_file_tof_grid()` but pulls spectra from `agilent_profile.rs` instead of an
   mzML reader, emitting the same `tof_index`+intensity facet and `tof_calibration` block.
   Reuse `tof_grid_spectrum`'s per-point lossless check.
4. **Dispatch (`convert_file` in `src/main.rs`, ~line 884):** add, *before* the MHDAC
   branch, `if is_agilent_d(input) && agilent_profile::has_profile(input) { return
   convert_agilent_grid(...) }`, gated behind a `--agilent-grid` CLI flag (default off
   until proven on a real profile `.d`). `has_profile` = `MSProfile.bin` exists and is
   non-empty. Falls through to the existing MHDAC / msconvert lanes otherwise.

**Status:** parser + win **prototyped and measured in Python** (`spike/agilent/`); Rust
**not yet written** (no profile `.d` on hand to validate end-to-end). The closed-form
coefficient mapping and the byte-exact codec de-risk the port.

---

## 4. Honest blockers

1. **No profile-mode `.d` in hand.** PXD041903's `MSProfile.bin` is 0 bytes. The measured
   C/A = 0.556× / C/B = 0.144× is from a faithful synthetic profile (real TOF positions +
   real calibration + real integer intensities, byte-exact MSProfile.bin codec), not an
   end-to-end convert of a real profile file. The synthetic profile's absolute B/peak and
   the raw-`.d` (A) figure depend on the assumed peak shape (7-tap, 7.2 bins/peak) and the
   digitizer bin period (measured ≈1.0 raw-x unit from within-spectrum gaps); the C/B m/z-
   axis collapse is robust to these (both lanes count the same points). Need a small profile
   `.d` (non-empty `MSProfile.bin`) to confirm the raw-`.d`-vs-mzPeak ratio end-to-end and
   the exact intensity-column behavior. This is the single thing blocking a fully
   first-party "measured beats 0.97×" claim.
2. **Centroid `.d` does not benefit.** `MSPeak.bin` centroids are f64 sub-bin-refined TOF +
   f32 intensity — not an integer lattice, not integer intensity. File-direct only helps
   profile data. (Worth gating the flag to skip centroid-only `.d`.)
3. **Intensity dtype.** Profile intensities are integers (`uint32` in the file); the
   measurement used f32 to match the baseline conservatively. An Int32/64 ΔBP intensity
   column would shrink the 0.66 B/peak intensity term too — additional headroom not yet
   measured.
4. **LZF profile path.** The Q-TOF case here is 0x90 RLE; the older HRMS path is LZF. The
   Rust reader must handle both. LZF is simple but is a second code path to get right.

---

## 5. Waters `_FUNC*.DAT` equivalent (noted, not implemented)

Waters TWIMS/raw stores integer flight-time bins in `_FUNC*.DAT`, but (per the SDK
research) the public MassLynx SDK hides them behind `ReadScan → vector<float>` (calibrated
float m/z + float intensity). A file-direct Waters reader would need to reverse-engineer
`_FUNC*.DAT`'s per-block `CompressedDataCluster` layout (bin width, packing) — a separate
spike of comparable size to this one, with less open-source prior art than rainbow gives
for Agilent. The IMS drift axis is a genuine per-scan integer (RLE→~0, like Bruker);
the m/z axis win depends entirely on parsing the hidden raw bins. **Do Agilent fully
first.**

---

## Artifacts (committed in the worktree, `spike/agilent/`)
- `msprofile_codec.py` — byte-exact `MSProfile.bin` 0x90-RLE encoder, validated by
  round-tripping rainbow's decoder (200/200).
- `probe.py`, `probe2.py` — rainbow-based `.d` decoders; established the centroid-only
  finding and the float-TOF/float-intensity centroid layout.
- `measure_filedirect.py` — the synthetic-profile A-vs-B-vs-C byte measurement.
- `verify_mapping.py` — proves the closed-form Agilent→`tof_grid.rs` `(c0,c1)` mapping is
  exact (6.5e-10 ppm) against the real `.d` calibration.

The reused writer path (`src/tof_grid.rs` + `convert_file_tof_grid`) is present in this
worktree (rebased from `ci/corpus-vendor-ci`) and its unit tests pass (3/3,
`cargo test --bin mzpeak-convert tof_grid`).
