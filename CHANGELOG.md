# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/), and the project adheres to
[Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.5.1] — 2026-07-17

### Added — filter a mzPeak straight to a searchable mzML

- **`mzpeak-convert IN.mzpeak --rt A-B --ms-level N -o OUT.mzML`** now writes a **real mzML** of the
  kept spectra (previously the filter only wrote mzPeak; a `.mzML`/`--to mzml` request silently
  produced a mislabeled mzPeak archive). The sync `MzPeakReader` decodes every buffer transform —
  including the timsTOF `tof → m/z` — so the mzML carries real m/z (verified 99.4–1292 on a timsTOF
  slice, not raw tof bins) and MS² **precursors survive**. Unblocks "slice a run to a narrow RT
  window, then hand the small mzML to Sage/MSFragger". Two-pass (metadata sweep → decode survivors);
  ion-mobility is flattened (one m/z+intensity spectrum per frame — mzML has no place for it) and
  vendor/aux facets are dropped (they don't map to mzML).

### Fixed

- **`--no-vendor` is now honored on the filter path** — it strips the embedded vendor side-files
  (`vendor/*`, incl. the multi-GB timsTOF `linespectra`/`analysis.tdf` blobs) from the output, same as
  `--drop-aux 'vendor*'`. Previously ignored, so a small RT slice still carried the whole-run vendor
  blob (reported: a 1,126-spectrum slice was 4.9 GB).

## [0.5.0] — 2026-07-09

### Added — mzPeak → mzPeak filtering (Phase 1 + 2)

- A `.mzpeak` input is now detected and routed to a **filter/repack** path (`src/filter.rs`) — no new
  subcommand; `mzpeak-convert in.mzpeak -o out.mzpeak …` just works. `report_inspect` also summarizes
  a `.mzpeak` (members + spectrum/chromatogram counts).
- **Spectrum-level filters** (surgical, index-stable — spectra are never renumbered):
  - `--rt MIN-MAX` — keep spectra whose `spectrum.time` is in range.
  - `--ms-level N` (repeatable / comma-list) — keep only the given MS level(s).
  Every per-spectrum facet (metadata, peaks/data, and vendor per-spectrum facets like
  `vendor_scan_trailers`/`_wide`) is filtered to the same survivor set; peak values are row-selected,
  never re-computed, so per-scan/per-chunk delta chains stay intact. Facet handling is
  schema-registry-driven and **errors** on an unrecognized per-spectrum facet rather than emitting an
  inconsistent file. Chromatograms are truncated to the RT window (point layout exact;
  numpress-chunked at chunk granularity). `ims_calibration` and other run-global blocks are preserved;
  index counts are refreshed and a `metadata.filter` provenance block is written. Dropped precursors
  leave a one-line warning (fragments are kept).
- **Aux remove/inject:** `--drop-aux '<glob>'` (repeatable) removes matching ZIP members and updates
  `index.files`; `--aux/--image/--sdrf` inject side-files into an existing archive.
- Verified content-preserving on real files (Thermo 8-facet incl. MS1+MS2+precursors+vendor trailers;
  timsTOF point facet, 74.2 M peaks) with 0 cross-facet inconsistencies across no-op/RT/MS-level/aux.

### Fixed — timsTOF retention time (enables `--rt` on timsTOF)

- The native ims-compact path now records each frame's **retention time** in `spectrum.time` (read from
  the TDF `Frames.Time`, seconds → minutes to match the mzML/Thermo convention). Previously
  `spectrum.time` was 0 for every frame, which made `--rt` a no-op on timsTOF. Verified exact
  (`spectrum.time == Frames.Time/60`), monotonic with frame index, and applied to both the archive and
  `--ims-chunked` layouts. Peak data is unchanged (metadata-only). (The opt-in `--bruker-sdk` path is
  not yet updated.)

### Known limitations

- **`--mz` (m/z-range filtering) is not yet implemented** (Phase 3) — it errors clearly.
- A no-op repack is **not byte-identical** — facets are decoded and re-encoded (zstd + best-effort
  encodings); peak content is preserved exactly.

## [0.4.15] — 2026-07-08

### Changed — multi-core parallel peak encoding (≈9× faster timsTOF conversion)

- **Parallel row-group encode for the peak facet** (`spectra_peaks.parquet`). timsTOF conversion was
  measured to be **~97 % encode+zstd-bound in a single writer thread** (parallel frame decode was
  already hidden at ~1.4 s); the Arrow column encode + zstd is now spread across a bounded worker
  pool using parquet's low-level `ArrowColumnWriter` API, while a single collector appends row groups
  in `spectrum_index` order. **Output is byte-identical** to the serial writer (verified sha256 on
  507 M- and 636 M-point files, independent of thread count), so page-pruning and determinism are
  preserved.
  - **Measured speedups (16-core machine):** g99123 1.5 GB archive **23.1 s → 2.5 s (9.2×)**; HeLa
    diaPASEF 60SPD 1.8 GB **33.6 s → 3.5 s (9.6×)**; `--ims-chunked` g99123 **26.4 s → 4.4 s (6.0×)**.
    Both layouts flip from encode-bound to decode-bound — decode (already parallel) is now the floor.
  - **Auto-detects cores** via `available_parallelism()` (no configuration needed). Override with
    `MZPC_ENCODE_THREADS` (or `RAYON_NUM_THREADS`); disable with `MZPC_PARALLEL_ENCODE=0` (serial
    path retained verbatim). In-flight memory is bounded **by bytes** (default `max(256 MB,
    threads×48 MB)`, override `MZPC_ENCODE_INFLIGHT_BYTES`), so memory stays flat as cores scale.
  - Encrypted facets fall back to the serial path.

### Added — instrumentation & corpus-tooling

- **`MZPC_TIMING=1`** prints a per-conversion decode-vs-encode split (`decode busy` / `encode+zstd
  busy` / total, plus detected threads and in-flight budget) for the timsTOF pipeline — the basis for
  the bottleneck diagnosis above.
- **Box tooling:** per-stage box timings (`conv_s`/`msconv_s`/`dl_s`/`up_s`/`raw_bytes`, optional
  `BENCH_TSV`); the size-bench msconvert is gated behind `MZPC_BENCH_MZML=1` (was unconditional,
  doubling `--via-msconvert` work); `--jobs` defaults to 1 with a disk-safety clamp
  (`MZPC_ALLOW_PARALLEL=1` to override) so concurrent multi-GB msconvert intermediates can't fill the
  box disk.

### Notes

- zstd stays at the ims default (L5) — on the archive layout the size is level-insensitive from L1–L5
  on most files, and encode is no longer the bottleneck, so lowering it is unnecessary.
- The serial `convert_file` path (Thermo `.raw` / msconvert-mzML re-read / SCIEX/Waters glue) writes
  the standard data facet, not the peak facet, and is unaffected by this change — parallelizing it is
  future work.

## [0.4.14] — 2026-07-07

### Added — timsTOF `--ims-chunked` (opt-in, m/z-prunable layout for fast slicing)

- **New `--ims-chunked`** (Bruker timsTOF ims-compact only, **OFF BY DEFAULT**) writes peaks in a
  **chunked integer-TOF layout**: each frame's peaks are split into true m/z bins (default 50 Th,
  override with `--chunk-size` in Th), and every chunk records its m/z min/max (`chunk_start`/
  `chunk_end`) as Parquet columns with page statistics, so the m/z axis becomes page-prunable. XIC /
  m/z-slice queries touch only the overlapping chunks (~2–4 % of the peaks) — a measured sweep on
  MSV000099123 ran **~20–30× faster** than the archive layout. TOF is delta-encoded within each chunk
  (cumulative-sum to reconstruct, lossless); the block carries `tof_encoding = m/z-chunked`,
  `chunk_bounds = mz`, and `chunk_width_th`.
- **The default is unchanged — the "archive" layout** (flat per-scan-delta table) stays the default for
  timsTOF: maximum compression and fast whole-spectrum access. `--ims-chunked` is a separate,
  mutually-exclusive opt-in; without it, output is byte-identical to 0.4.13.

### Fixed

- **`BYTE_STREAM_SPLIT` now applies to the chunked layout's nested value columns.** The writer matched
  the leaf names `intensity`/`tof` only, so the chunked `chunk.intensity.list.item` /
  `chunk.tof_chunk_values.list.item` columns silently fell back to dictionary encoding. Restoring BSS
  cuts the chunked size overhead sharply (MSV000099123: **+19 % → +1.9 %** vs the archive layout;
  chunked lands at ~parity with archive and **0.86× the vendor `.d`**). No effect on the default or any
  other layout/format.
- **`ims_calibration.tof_encoding` is now truthful** — emits `per-scan-delta` (default), `absolute`
  (`--no-tof-delta` / SDK path), or `m/z-chunked` (`--ims-chunked`), replacing a hard-coded `"absolute"`
  that mislabeled the default delta output.

### Notes

- Losslessness verified independently (pyarrow reconstruction, not the writer's own check) across the
  reference timsTOF + HeLa diaPASEF sets: ≥1.2 billion peaks, 0 mismatches on tof/intensity/mobility/
  spectrum_index.
- Whole-spectrum random access equals the archive layout once chunk row groups are sized finely; the
  shipped default (8192 chunks/row group) is coarse on very large files — set `MZPC_ROW_GROUP_ROWS` to
  tune, or use the archive layout for whole-spectrum-heavy workloads. Points-based auto-sizing is planned.

## [0.4.13] — 2026-07-06

### Released to main — per-scan delta TOF is the default

- Merges the per-scan delta TOF encoding (0.4.12) into `main`: for timsTOF ims-compact conversion the
  integer TOF axis is stored as per-scan deltas by default (byte-split; lossless — a reader cumulative-
  sums within each mobility scan, keyed on `mzpeak:tof_delta_reset=scan`). ~15% smaller, 0.91× the
  vendor `.d` on the reference diaPASEF run. Use **`--no-tof-delta`** for absolute bins (1.02×) when the
  reader does not understand the delta layer.
- Verified on merge: 27 unit + 3 contract tests green; e2e confirmed default = delta (marker present),
  `--no-tof-delta` = absolute (marker absent), non-TDF conversion unaffected.

## [0.4.12] — 2026-07-06

### Changed — timsTOF ims-compact: per-scan delta TOF is now the default

- **The integer TOF (m/z) axis is now stored as per-scan deltas by default** (the first peak of each
  mobility scan is the absolute bin, the rest are increments), byte-split + zstd. ~15% smaller than
  absolute bins; on the reference diaPASEF run (PXD017703 HeLa 60 SPD) the file is **1682 MB = 0.91× the
  vendor `.d`** — below the raw vendor file. Lossless: a reader reconstructs the absolute TOF by
  cumulative-summing within each mobility scan. Round-trip verified end-to-end (291,531 peaks
  reconstructed exactly, 98.6% via accumulated deltas).
- **New `--no-tof-delta`** stores absolute TOF bins instead (byte-split; 1892 MB = 1.02× the `.d`).
  Replaces the earlier experimental opt-in `--frame-compact-ims` flag with an opt-out.
- The native/SDK `tof` column now uses `BYTE_STREAM_SPLIT` (was delta-packing) in both modes.
- **Reader compatibility:** delta files carry `mzpeak:tof_delta_reset=scan` per spectrum; a reader MUST
  cumulative-sum the `tof` column within each mobility-scan run before applying the m/z model, and access
  is per-frame rather than per-point. Use `--no-tof-delta` for readers that don't understand the delta
  layer.

## [0.4.11] — 2026-07-04

### Added — native Agilent profile `.d` → mzML (all platforms)

- **`--to mzml` now reads Agilent *profile* `.d` with the pure-Rust reader**, so a native
  vendor→mzML conversion works off Windows without msconvert (previously the mzML lane guarded
  Agilent `.d` out on non-Windows → `--via-msconvert`). Each `MSProfile.bin` flight-time bin is mapped
  to m/z with the per-scan calibration, applying MassHunter's polynomial refinement when present (the
  same math the mzPeak grid lane gates against), and emitted as profile spectra.
- **Graceful fallback:** if the reader can't model a `.d` (e.g. the 6560 DTIMS / flat-`MSScan.xsd`
  ion-mobility variant, which has no `SpectrumParamsType`), the lane logs a diagnostic and falls
  through to the typed *"…use `--via-msconvert`"* guidance instead of surfacing a raw schema-parse
  error — no crash, no partial output. (Native support for that IM variant is a separate follow-up.)

## [0.4.10] — 2026-07-04

### Fixed — `--to mzml` on directory-based vendor formats

- **Bruker TDF `.d` → mzML no longer crashes with `EISDIR` (os error 21).** The `--to mzml` lane
  applied the mzML/imzML XML preprocessing (Latin-1 transcode + empty-param-group sanitize)
  unconditionally, and those steps `read()` the input path as a file — which fails on a `.d`
  *directory* before the reader is ever reached. The preprocessing is now gated on a file input, so a
  `.d` goes straight to `open_path`, which reads Bruker TDF directly (verified: `test.d` → 919
  spectra). As a side effect, any unhandled directory input now fails with a clear "unknown format"
  error instead of a bare `EISDIR`.

## [0.4.9] — 2026-07-03

### Fixed — mzML output correctness (adversarial review of the `--to mzml` path)

- **Chromatograms are no longer dropped.** The initial `--to mzml` lane wrote only spectra, so a
  chromatogram-only SRM/MRM mzML converted to an EMPTY file (all 720 SRM traces lost) and any
  source TIC/SIM was discarded. The lane now passes the source's chromatograms through (collected
  before the spectrum pass, since iterating spectra can leave the reader past the chromatogramList),
  dropping only source TIC/base-peak because the mzML writer emits its own spectrum-derived TIC +
  base-peak summary. raw→mzML and raw→mzPeak now carry the same chromatograms (verified: sciex-qtrap
  SRM 722↔722, Agilent IM-QTOF 2↔2, timsTOF TSF 4819 spectra + 2 chromatograms).
- **Zero-spectra crash fixed.** A chromatogram-only input hit `Attempted to transition from Run to
  Run` in the mzML writer; the spectrumList is now opened explicitly so chromatograms have a valid
  state to follow.
- **Correct spectrum count + metadata.** `set_spectrum_count` is set (the `spectrumList` count
  attribute was 0), and the native-reader lane now fills run/source-file metadata (`fixup_run_metadata`)
  instead of emitting a metadata-less mzML.
- **`--via-msconvert --to mzml` surfaces msconvert's stderr** on failure (unknown-instrument /
  unsupported-format), matching the mzPeak `convert_via_msconvert` path.
- Peak data is bit-exact between raw→mzPeak and raw→mzML (m/z & intensity diff = 0).

## [0.4.8] — 2026-07-03

### Added — mzML output (`--to mzml`)

- **The converter can now write plain mzML as well as mzPeak.** Output format is chosen by the `-o`
  extension (`.mzML` → mzML, else mzPeak) or forced with **`--to mzpeak|mzml`**. The mzML lane
  bypasses every mzPeak-specific encoder and streams the read spectra through the mzdata writer, so
  it works for every format the tool reads — mzML/imzML, Thermo `.raw`, Bruker TDF/TSF/BAF, and the
  Windows-native vendor readers (SciEX/Waters/Agilent/Shimadzu) — making it a cross-platform
  vendor→mzML converter. `--via-msconvert --to mzml` runs msconvert straight to the output mzML.
  Verified round-trip (spectrum count + exact m/z) on a real Agilent IM-QTOF mzML.

## [0.4.7] — 2026-07-03

### Added — native Shimadzu `.lcd` reader (Windows, no msconvert) — glue only, hosting UNVERIFIED

- **`glue/shimadzu/` + `src/shimadzu.rs`** — a native Shimadzu LabSolutions `.lcd` reader that
  drives the vendor `Shimadzu.LabSolutions.IO` managed API in-process (the same DLL ProteoWizard's
  `Reader_Shimadzu` wraps), so `.lcd` can convert **without** shelling out to `msconvert`. Mirrors
  the SciEX/Agilent pattern: a net8.0 C# glue reaches the vendor API purely by runtime reflection
  (`Assembly.LoadFrom` from `MZPC_PWIZ_DIR`), and the Rust side hosts CoreCLR via `netcorehost`.
  Wired into `is_lcd()` detection, `convert_shimadzu()`, inspect, and the off-Windows guard.
- **The vendor DLL is never shipped.** No compile-time reference, no bundling; loaded at runtime
  from an existing ProteoWizard install. `.gitignore` now excludes every EULA-restricted vendor
  assembly by name and `glue/**/*.dll` as a hard backstop.
- **⚠️ Status: the glue is verified correct (type + all `[UnmanagedCallersOnly]` exports load in a
  net8 host), but the shared `netcorehost` hosting path is UNVERIFIED end-to-end** — resolving the
  first export currently fails with hostfxr `0x8000211D`, a foundation-level issue affecting all
  four `.NET`-glue vendors (SciEX/Waters/Agilent/Shimadzu, all previously untested), not the
  Shimadzu logic. Until that's resolved, convert Shimadzu `.lcd` via `--via-msconvert` (9030-class;
  the legacy IT-TOF `.lcd` is unsupported by ProteoWizard itself).

## [0.4.6] — 2026-07-02

### Fixed — duplicate `intensity array` column blanked the spectrum view

- **One column per logical array (`spectra_peaks` and all facets).** The schema
  sampler could emit a second `intensity array` column at the source precision
  (an `intensity_f64` beside the primary f32 `intensity`, both reusing
  `array_name: "intensity array"`). Written centroid peaks only filled the f32
  primary, leaving the f64 twin 100% null; a reader resolving arrays by
  `array_name` without honoring `buffer_priority` clobbered the real data with
  the null column, rendering MS2 spectra as a flat line at intensity 0
  (`sdrf-examples/PXD011799`). The writer now **coalesces columns by
  `(array_accession, buffer_format)`** so a facet holds at most one column per
  logical array — while leaving a chunked array's distinct-format component
  columns (`chunk_start`/`chunk_end`/`chunk_values`/`chunk_transform`) intact.
- **Precision coercion at the write boundary.** A source encoding a logical
  array at a different precision than its one canonical column is now cast into
  that column (lossless widening for m/z; the format's convention precision for
  intensity) instead of failing record-batch assembly — this also fixes a
  pre-existing `--layout point --no-chromatograms` Float64/Float32 write clash.
- **Invariant guard** (`debug_assert`) that no facet carries two columns with
  the same `(array_accession, buffer_format)`, plus a finish-time backstop that
  prunes any all-null column duplicating a populated sibling's `array_name`.
- Verified byte-identical output on 12 real datasets across 8 vendors (only the
  one twin-affected file changes: `PXD000001`, twin removed, data preserved,
  +0.14 %).

### Fixed — SCIEX `--via-msconvert` (v0.4.5 tip)

- **`--ignoreUnknownInstrumentError`** is passed to the spawned `msconvert`, so
  newer SCIEX acquisitions (ZenoTOF 7600, newer TripleTOF) whose instrument
  model ProteoWizard doesn't recognize convert instead of writing no mzML.
- The spawned `msconvert`'s stdout+stderr are captured and their tail surfaced
  in the failure message, so a `--via-msconvert` error is self-diagnosing.

## [0.3.1] — 2026-06-27

### Added — docs & CI

- **`docs/PLATFORM_SUPPORT.md`** — authoritative per-platform vendor-format
  support matrix (format × OS, reader mechanism, runtime requirement), the
  why-the-split rationale, the four `.NET` glue executables + their env vars,
  and the CI-coverage summary. Linked from the README.
- **macOS CI** — `ci.yml`'s `build-test` is now a `[ubuntu, macos]` matrix;
  each builds that platform's `mzpeak-convert`, runs the tests, and
  smoke-converts the committed fixture. Linux-only deps and the Bruker-SDK
  e2e are gated on the runner OS.
- **Glue-executable verification (Windows CI)** — after the C# glue build,
  `windows.yml` asserts all five produced artifacts exist (`mzpeak-convert.exe`,
  the net48 `AgilentGlueHost.exe`, and the three net8 glue DLLs).

## [0.3.0] — 2026-06-27

### Fixed — validator spec-compliance (mzpeak-0.9 profile)

- **Array-index `unit` is always a CURIE.** Arrays arriving with `Unit::Unknown`
  (mzML intensity, the integer `tof_index` grid column, ion-mobility / charge columns)
  get a conventional fallback unit (intensity → `MS:1000131`, tof_index → `UO:0000189`,
  1/K0 → `MS:1002814`, drift time → ms, charge → `UO:0000186`) instead of an empty /
  `null` unit, in both the Parquet field-metadata and the JSON index. `buffer_priority`
  is now omitted when absent rather than serialized as `null`.
- **Mandatory CV terms injected** where the source omits them: a child of `data
  transformation` (`MS:1000530`) per processing method, `data file content`
  (`MS:1000294`) in `file_description`, `software` (`MS:1000799`), `instrument model`
  (`MS:1000031`), `detector type` (`MS:1000026`) — only when the entry declares no CV
  term, so no duplicate / "too-many" violations.
- **`tof_calibration.lossless`** (`"tof_index"`) is now written on the SciEX-sqrt grid
  path too (it was only on the TSF / Agilent builders).
- Net effect: the example corpus validates **0 errors / 0 warnings** (was 126 FAIL).

### Changed — Agilent native moved out-of-process (.NET Framework 4.8)

- MHDAC's `OpenDataFile` internally calls `Delegate.BeginInvoke`, permanently
  unsupported on .NET Core / 5+. The Agilent native reader is therefore **re-hosted as
  a standalone net48 EXE** (`AgilentGlueHost.exe`, built from `glue/agilent/` via
  `Microsoft.NETFramework.ReferenceAssemblies` so it cross-builds with the dotnet SDK).
  `src/agilent.rs` spawns it per `.d` and reads back a little-endian binary file,
  replacing the in-process `netcorehost` / `UnmanagedCallersOnly` FFI. The host writes
  its output atomically (`.part` + rename); the Rust reader bound-checks declared
  sizes against the on-disk file.

### Added — no-S3 box conversion tooling

- `tools/box_convert_scp.sh` + `box_local_convert.ps1` — convert vendor formats on a
  Windows box via **direct SCP** (raw up, `.mzpeak` back), no S3 round-trip, with ssh
  keepalive for large transfers.
- `tools/box_url_convert.ps1` — the box pulls the raw **straight from its public source**
  (PRIDE / MassIVE) into a local cache (atomic `.part` download), converts, and the
  caller retrieves the result; `-Names` handles sources whose filename is in the query
  string (e.g. MassIVE `DownloadResultFile`).

### Added — data features

- **FILE-DIRECT Agilent Q-TOF *profile* reader** (`--agilent-grid`, off by default;
  pure Rust, no MHDAC/msconvert). Reads the integer flight-time grid straight from
  `AcqData/MSProfile.bin` (0x90-RLE + LZF decoders, MSScan.xsd/.bin parser,
  MSMassCal.bin / DefaultMassCal.xml calibration) and stores `tof_index` (Int32,
  delta-packed) + integer intensity + per-spectrum `tof_c0`/`tof_c1`/
  `tof_calibration_id` and a per-`CalibrationID` polynomial in the `tof_calibration`
  index block. Reconstructs MassHunter m/z exactly (`t = base + (c0+c1·k)/coeff`,
  `m/z = (coeff·(t−base))² − poly(clip(t,left,right))`). Measured on a real profile
  `.d` (MTBLS1334): **0.141× the vendor `.d`, 0.225× the msconvert lane**, m/z lossless
  to 7.8e-10 ppm, integer intensities exact. Only dispatched when `MSProfile.bin` is
  non-empty (centroid-only `.d` fall through unchanged).
- **TIC + base-peak chromatograms synthesized from MS1** (on by default;
  `--no-chromatograms` / `no_chromatograms` to disable), across every convert path.
- **UV/PDA spectra carried** into a dedicated `wavelength_spectra` facet (Waters /
  Agilent mzML and any wavelength-bearing input); no longer dropped or mislabeled
  as mass spectra.
- **Registered TOF→m/z transform** on the ims-compact `tof` column: the column
  metadata carries the transform CURIE + `[a, b]` coefficients (`transform_params`)
  so readers reconstruct `m/z = (a + b·tof)²` generically (ims_calibration kept too).
  Provisional CURIE pending the PSI term.
- **Native Agilent IM-MS (MIDAC) reader** — Windows-only scaffold, compile-verified
  (untested at runtime; needs MIDAC DLLs + IM-MS data). An Agilent `.d` with ion
  mobility routes to MIDAC, else MHDAC.
- **Bruker timsdata SDK reader (`--bruker-sdk`)** — an opt-in parallel path that reads
  TDF *and* TSF `.d` through Bruker's official `timsdata` library (vendor index→m/z
  calibration; per-peak 1/K0 mobility for TDF), emitting the same `MultiLayerSpectrum`
  structures as the default pure-Rust readers. Windows/Linux only (no macOS SDK);
  loads `timsdata.dll`/`libtimsdata.so` via `TIMSDATA_LIB_DIR`. Implies f64 m/z (not
  ims-compact). BAF is unaffected — it uses the separate `baf2sql` library. Pure
  decode/mapping logic is unit-tested; CI runs a real `.d` e2e when the SDK is
  provisioned on the runner.

### Changed — dependencies

- **mzdata `0.64.1` → `0.65.2`** — pulls upstream TDF/ion-mobility correctness fixes
  (`process_3d_slice` per-frame peak inflation; ion-mobility off-by-one labeling) that
  affect the standard `--no-ims-compact` TDF path. No arrow/parquet/mzpeaks churn.

### Changed — single-command CLI (breaking)

- The tool is now **one command** — `mzpeak-convert <input> [-o <output>] [options]`.
  The `convert`, `inspect`, `ims-compact`, `tof-grid-probe`, and `tof-grid`
  subcommands are removed.
  - **No `--output`** → nothing is written; the input is inspected and a report
    is printed (the former `inspect`).
  - **`-v`** prints that inspection report *and* still converts.
  - **ims-compact** is now an option, **on by default for Bruker timsTOF (TDF)**;
    disable with `--no-ims-compact`. The standalone bare-Parquet encoder is gone.
  - `tof-grid`/`tof-grid-probe` (a measured no-go research spike) are removed.
- **`--config` is now a general configuration file** holding *any* overridable
  option (not just vendor side-file policy). Precedence: CLI flag > config > default.
- **Removed `--verify`** (round-trip count check). Fidelity/conformance checking is
  out of the converter's scope.
- **Vendor-SDK readers are on by default per platform.** The Agilent (MHDAC), SciEX
  (Clearcore2), and Bruker BAF (libbaf2sql_c) readers now compile in automatically
  where the vendor libraries exist (Windows for all three; Linux also for BAF) —
  the `bruker_sdk`/`agilent`/`sciex` cargo features are gone. They load the vendor
  DLLs at runtime; macOS builds none. Inputs with no native reader on the platform
  exit 3 (use `--via-msconvert`).
- SQLite is compiled from source (`rusqlite` `bundled`) — self-contained build on
  all platforms (no system libsqlite3).

### Added

- Windows CI: builds default + all vendor-SDK features, the C# glues, smoke-converts,
  and (separately) installs ProteoWizard from TeamCity to exercise `--via-msconvert`.

## [0.2.0] — 2026-06-21

### Changed

- **Removed the built-in conformance validation** (`validate` subcommand and
  `convert --validate`). Validation is delegated to the independent
  `mzpeak-validate` tool; `--verify` (round-trip fidelity) stays. Exit codes are
  now `0`/`1`/`3` (the old `5` is gone). **Breaking** for anyone scripting the
  `validate` subcommand.
- Documentation now states prominently that the mzPeak format is still in the
  HUPO-PSI specification process (draft v0.9) and this converter is a technical
  demonstrator, not a production tool. Added references to mzpeak.org, the
  HUPO-PSI/mzPeak-specification repo, and the in-browser viewer at mzpeak.org/view.

### Added

- Bare `ims-compact` encoder now streams one frame at a time (constant memory)
  with an independent streaming lossless re-read.
- Unsupported vendor inputs now exit `3` (typed `UnsupportedVendor` error).

### Fixed

- Collapsed three byte-identical `convert_*` writer bodies into one shared path.
- Guard the archive ims-compact TOF cast against i32 overflow.
- Agilent glue export used a non-blittable `char*` across the FFI boundary
  (would mis-marshal on Windows); switched to `ushort*` like the SciEX glue.
- `gen_sbom.py` null-root crash + legacy `/` SPDX normalization; sweep-script id
  sanitization.

## [0.1.0] — 2026-06-21

First public release.

### Added

- **`convert`** — unified conversion to mzPeak (HUPO-PSI v0.9) for:
  - mzML / `.mzML.gz`
  - imzML (imaging coordinate columns + IMS CV promoted)
  - Bruker `.d` **TDF** (timsTOF) with ion mobility preserved
  - Bruker `.d` **TSF** (MALDI/line spectra; ported rusqlite + zstd reader)
  - Thermo `.raw` (via a self-hosted .NET runtime) with a verbatim
    `vendor_scan_trailers` facet (+ wide + status-log)
- **Signal layout** options: `chunked` (numpress-linear default, or lossless
  delta via `--no-numpress`) and `point`; configurable `--chunk-size`,
  `--zstd-level`.
- **`--ims-compact`** (Bruker TDF) — store the lossless native integer-`tof`
  signal in `spectra_peaks` (+ `ims_calibration` in the index) instead of f64
  m/z; ~50 % smaller, bit-exact TOF grid. Standalone **`ims-compact`**
  subcommand encodes a bare Parquet and streams one frame at a time (constant
  memory) with an independent lossless re-read verification.
- **Vendor side-file embedding** (`vendor/` in the archive): preserve-by-default,
  gzipped, declared `proprietary`; YAML policy via `--config`, per-glob override
  via `--aux`, opt-out via `--no-vendor`.
- **`--via-msconvert`** — cross-vendor interim path through ProteoWizard
  `msconvert` (Agilent `.d`, SciEX `.wiff`, and anything msconvert reads).
- **`inspect`** (with `--json`) and **`tof-grid-probe`** / **`tof-grid`** (P5
  TOF-grid feasibility spike).
- **`--verify`** round-trip fidelity check (conformance validation is left to the
  independent `mzpeak-validate` tool).
- Stable exit codes: `0` ok, `1` generic, `3` unsupported.
- Optional, off-by-default build features for native vendor SDK readers:
  `bruker_sdk` (BAF), `agilent` (MHDAC), `sciex` (Clearcore2) — Windows-runtime,
  compile-verified.
- End-to-end corpus harness (`tests/run_corpus_e2e.sh`) and a full-data sweep
  runner (`tests/run_data_sweep.sh`).
- Documentation: README, [user manual](docs/USER_MANUAL.md), architecture
  ([PLAN.md](PLAN.md)), native-TOF design ([NATIVE-TOF-DESIGN.md](NATIVE-TOF-DESIGN.md)),
  CycloneDX SBOM, and third-party notices.

### Known limitations

- Native Agilent/SciEX/BAF readers are compile-verified but not yet
  runtime-tested (require a Windows host + licensed vendor DLLs).
- UV/PDA (non-MS) spectra in some mzML files are not carried into the archive.
- Thermo instrument error-log facet and the registered tof→m/z column transform
  are deferred pending upstream API support (see [HANDOFF.md](HANDOFF.md)).

[0.3.1]: https://github.com/okohlbacher/mzPeakConverter/releases/tag/v0.3.1
[0.3.0]: https://github.com/okohlbacher/mzPeakConverter/releases/tag/v0.3.0
[0.2.0]: https://github.com/okohlbacher/mzPeakConverter/releases/tag/v0.2.0
[0.1.0]: https://github.com/okohlbacher/mzPeakConverter/releases/tag/v0.1.0
