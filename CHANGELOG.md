# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/), and the project adheres to
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Fixed

- **Dual-representation archives mixed layout families, and readers could not see their centroids.**
  `spectra_data` and `spectra_peaks` are both `entity_type: spectrum`, and `docs/conformance.md:68`
  is explicit: "within an entity, all `array_index` entries share one layout family — either every
  entry is `point` or every entry is one of the `chunk_*` formats; the two **MUST NOT** be mixed".
  The v0.8.0 peaks-facet heuristic chose per facet, by whichever held more points, so archives came
  out `chunk` data + `point` peaks.

  **Correction to the first version of this entry:** it said such archives were "unreadable" and
  that mzPeakViewer could not reach their centroids. That is false, and was checked afterwards
  rather than before writing it. The centroid data is present and readable — verified three ways on
  `HEK_PosOAD1`: the vendored Rust reader (all 1,543,961 peaks), **mzpeakts, the viewer's own
  engine** (`altAvailable = true` on every spectrum, forced-centroid read returns peaks), and
  pyarrow directly. mzpeakts resolves the layout *per source*, so a mixed archive does not break it.
  The viewer symptom that prompted this was a separate, already-fixed viewer bug (mzPeakViewer
  `2c84ee3`, shipped in v0.9.1: parquet-wasm panicked on the second read of a facet over HTTP,
  naming these two Shimadzu files). The defect fixed here is **conformance**, which stands on its
  own: one `entity_type` must have one layout family, and an archive that breaks it is readable only
  by luck of implementation.

  The peaks facet now always takes the data facet's strategy, and `make_peaks_writer` **enforces**
  it — a mismatch is an error at write time, not an archive. Verified by reintroducing the v0.8.0
  behaviour and confirming the conversion fails.

  **Blast radius, corrected and much larger than first reported.** A 0-row facet still ships a full
  array index carrying its own prefix, so the mismatch is not confined to archives with both facets
  populated: **191 of 207** archives in `mzpeak-example-data` (92.3%) are mismatched. It predates
  the heuristic — before `6f0b39e` the peaks facet was `point` *always*, so anything chunked was
  mismatched. Truly dual-representation archives (≥1 spectrum with both counts) machine-wide: 16
  paths, 11 distinct, of which the 2 Shimadzu ones are fixed here and the rest were not written by
  this converter. `090701-LTQVelos-unittest-01` is **not** dual — it is mixed-*mode* (some spectra
  profile-only, some centroid-only, `both = 0`); the earlier entry mislabelled it.

  **Size impact on dual archives cuts both ways** — it depends on how large the centroid facet is,
  since chunking repays its per-chunk columns only on a big peak list:

  | archive | peaks | mixed (illegal) | matched | |
  |---|---:|---:|---:|---:|
  | `HEK_PosOAD1` | 1,543,961 | 35,018,382 | **33,392,223** | **−4.6%** |
  | `Blind_P1_pos_012` | 216,742 | 3,268,711 | **3,533,168** | **+8.1%** |
  | `090701-LTQVelos-unittest-01` | 16,047 | 1,419,533 | **1,464,868** | **+3.2%** |

  So roughly −5% to +8%, and not a win on balance. Conformance is the reason for the change, not
  size. Centroid-only runs are byte-for-byte unaffected and keep the full chunked-peaks win (Bruker
  microTOF −37%, Shimadzu `.lcd` −46%), since their data facet is chunked and the peaks facet simply
  follows.

  Corroborated by mzPeakViewer's own golden dual fixture (`packages/core/test/fixtures/dual.mzpeak`),
  which is `point`/`point` — family-consistent, as ours now are.

  **mzPeakValidator does not catch this** (it passes all three mixed archives); reported separately.

## [0.8.0] — 2026-08-25

### Fixed

- **Unknown signal continuity produced a self-contradictory archive, losing all the data.** The
  metadata side assumed profile — logging "assuming profile", writing `number_of_data_points`, and
  nulling `number_of_peaks` — while the writer routed the bytes to the PEAK facet. So `spectra_data`
  came out empty while the counts told a reader to look there, and the spec's count-driven read
  planning found nothing. Reproduced on a 600-spectrum mzML with the continuity cvParam stripped:
  **17,965 points in, 0 out** of a round-trip. Routing now follows the same profile assumption the
  counts already made, and the round-trip returns all 17,965.

  The same spectra also had `spectrum_representation` = null, which violates a MUST
  (`schema/table_rules.json` `spectrum_must`, requirement_level MUST, MS:1000525). Unknown now
  writes `MS:1000128`, consistent with the assumption made everywhere else. **mzPeakValidator did
  not flag either problem** — neither the missing MUST term nor the count/facet contradiction.

- **`--to mzml` aborted on any archive with a null-typed chromatogram.** The chromatogram visitor
  unwrapped `chromatogram_type` without the null guard its sibling spectrum-continuity visitor has,
  so one such row killed the process (`Option::unwrap` on `None`). The writer emits exactly such a
  row — `write_empty_chromatogram`'s placeholder, id `""`, type null — whenever a run ends up with
  no chromatograms, which meant **every Waters MRM archive crashed on export**. Guarded.

- **Non-indexed mzML silently loses its chromatograms.** mzdata can only enumerate an mzML's
  chromatograms from the EMBEDDED index: on a plain mzML `count_chromatograms()` reports 0 and
  `get_chromatogram_by_index(0)` returns `None` even with a populated `<chromatogramList>`, and
  `build_index()` does not recover them. A Thermo LTQ Velos mzML declaring `TIC` + three `SIM SIC`
  traces yielded **0 from source**; the two synthesized MS1 chromatograms masked it, so the three
  SIM SICs vanished without a word. This is an upstream limitation the converter cannot fix, so it
  now **warns**, naming the file and pointing at re-indexing. Indexed mzML (verified on a 300-
  chromatogram Agilent file) is unaffected and stays silent.

- **Waters: continuity read from the vendor, not assumed.** [src/waters.rs](src/waters.rs)
  hardcoded `SignalContinuity::Profile`, so every centroided MassLynx function was mislabelled and
  its peaks written to the profile facet — the same non-conformance as the Shimadzu defect above.
  `MassLynxRaw.dll` exports `isContinuum` (confirmed by reading the DLL's PE export table, alongside
  `getFunctionType`), so it is now bound and resolved once per FUNCTION, which is the granularity
  MassLynx stores it at. The binding is optional: an older DLL without the export keeps the previous
  behaviour and warns, rather than failing the conversion.

- **Bruker BAF: no longer silently drops a representation, and honours `--representation`.**
  `select_pair` fell back only from profile to line, so a row storing ONLY profile arrays returned
  the empty line pair and was written as an **empty spectrum labelled centroid** — data lost and
  mislabelled on the way out. It now falls back in both directions, reports `Unknown` when neither
  pair is readable instead of inventing a label, and takes its preference from `--representation`
  rather than a hardcoded `prefer_profile = false`. Emitting BOTH facets for a BAF row is still
  open: that needs the second pair read as a peak list.

- **`get_spectrum_by_id` could not see the peaks facet.** It called `get_spectrum_arrays`
  unconditionally, which reads only `spectra_data` — so a centroid-only archive returned an EMPTY
  spectrum by ID while the same spectrum read fine by index. Measured on a Bruker microTOF-Q2
  archive: **by-id 0 points, by-index 937**. It now delegates to the by-index path, which also picks
  up the loading preference and the per-spectrum TOF-grid reconstruction this branch never applied.
  Covered by `by_id_reads_the_peaks_facet_on_a_centroid_only_archive` (corpus-gated), verified to
  fail against the old code.

- **The peak writer no longer panics on a signal-free spectrum.** `mini_peak.rs` had
  `RefPeakDataLevel::Missing => unimplemented!()`. Empty spectra are legitimate (newer-timsTOF blank
  frames) and reach this writer whenever the metadata says peaks; it now records an empty entry.

- **Shimadzu `.lcd` spectra were labelled `profile` regardless of what they actually contained.**
  The glue's `Data()` computed a correct `centroid` flag; `SpectrumData` destructured it into a
  discard, and `Meta()` hardcoded `SignalContinuity = 0`. So `src/shimadzu.rs` labelled every
  spectrum profile, which routed centroid data into `spectra_data.parquet`, stamped `MS:1000128`,
  populated `number_of_data_points`, and left `number_of_peaks` null. mzPeak requires the opposite
  on all four counts for centroid data, so every native-lane Shimadzu archive written before this
  was **non-conformant**, not merely suboptimal. Continuity is now derived from which list the
  vendor API actually returned.

  Verified on `DIA_Hela_20ng` (21,500 spectra) before → after:

  | | before | after |
  |---|---|---|
  | `spectrum_representation` | `MS:1000128` × 21,500 | `MS:1000127` × 21,500 |
  | `number_of_peaks` | null × 21,500 | populated × 21,500 (279,686,550 points) |
  | `number_of_data_points` | populated × 21,500 | null × 21,500 |
  | signal facet | `spectra_data.parquet` | `spectra_peaks.parquet` (1.546 GB) |
  | mzPeakValidator 0.9.1 | — | **PASS, 0 errors, 0 warnings** |

### Added

- **The peaks facet can now use the chunked layout — 46% smaller on centroid data, losslessly.**
  Enabled per file by measurement on every format — see "chosen from the data" below.
  `MZPC_PEAKS_CHUNKED=1`/`=0` overrides either way.

  A centroid peak list is just a sorted m/z array, so it chunks exactly like profile signal. Without
  this the peaks facet is the point layout, where the spec requires values be stored as-is, so
  `point.mz` lands as PLAIN `f64`. Measured on the centroid-only Shimadzu archive, that one column
  was **82% of the file at 1.82× compression**, while `spectrum_index` beside it got 42.9× from
  `DELTA_BINARY_PACKED`.

  | | point layout | chunked peaks | |
  |---|---:|---:|---:|
  | `DIA_Hela_20ng` | 1,547,389,638 | **839,156,909** | −45.8% |
  | `DIA_Hela_100ng` | 1,554,759,745 | **857,062,254** | −44.9% |
  | `blank-centroid` (mzML, forced on) | 21,387,267 | **10,889,211** | −49.1% |

  **Lossless on `.lcd`**: the chunks carry `chunk_encoding = MS:1003089` (delta) with no numpress
  transform, and a round-trip over 2,584,247 points differs in **zero** m/z and **zero** intensity
  values, bit-for-bit. On mzML input the default strategy is numpress-linear instead, which is what
  the data facet already used there — measured at 0.0002 ppm max m/z deviation, intensities exact.
  mzPeakValidator 0.9.1 PASS, 0 errors, 0 warnings, on every archive above.

  **Why the chunked layout rather than just encoding m/z as a scaled integer in place:** the point
  layout requires values be stored as-is precisely so the page index stays meaningful, and it has
  nowhere to declare a transform — its array-index entries pin `transform: null`. A reader would
  have no way to learn the scale factor. The chunked layout carries exactly that declaration, and
  the emitted index shows it: `chunk_start`, `chunk_end`, `chunk_values`, `chunk_encoding`,
  `chunk_secondary`, plus a `chunk_transform` entry when a transform is used. Same compression,
  declared rather than implied.

  Two bugs fixed on the way in, both only reachable from the vendor lanes:

  - `ChunkBuffers` could chunk raw arrays only on the ims `tof` axis with an m/z boundary
    (`add_raw_mz_boundary`, gated to timsTOF). Ordinary centroid m/z arrays fell through to flat
    points. Added `add_raw_chunked` for the default m/z axis; the ims path is untouched.
  - The vendor lanes never sampled the **peak** facet schema — only the data facet — so a chunked
    peak facet panicked in `add_arrays` with `expected Float32 but found LargeList(Float32)`. The
    mzML lane escaped this because it calls `sample_array_types_for_peaks_from_spectrum_source`,
    which needs a `RandomAccessSpectrumSource` the vendor lanes do not have. Added
    `sample_array_types_for_peaks_from_spectra`, the iterator equivalent.

  Cost: conversion time went from ~80 s to ~200 s per file, since the m/z column is now encoded
  rather than dumped.

- **The reader now carries every representation a vendor stores for one spectrum, not just one.**
  When a scan has both, the profile goes to `spectra_data` and the centroid list rides along as a
  peak list in `spectra_peaks`, and the metadata row carries both counts — the shape the writer
  already supported (`writer/base.rs` "Writing both profile signal and peaks") but which no vendor
  lane ever produced, since all of them passed `None` for peaks.

- **`--representation both|profile|centroid`** (default `both`). `both` means "everything the source
  has" — a single-representation scan is normal and silent. An explicit `profile` / `centroid` on a
  file that stores only the other one warns once and writes what the file actually contains with its
  true label, rather than emitting an empty facet tagged as the absent representation. The flag is
  read only by the Shimadzu lane today; setting it elsewhere warns that it is inert.

- One-entry memo in the glue for `Api.Data`, since the reader asks for profile then centroid on the
  same scan — without it every spectrum cost two `GetMSSpectrumByScan` round-trips.

- `--representation` now reaches the `--to mzml` export path too. It previously did not: that path
  called `ShimadzuReader::open` rather than `open_with`, so `--representation profile` and
  `--representation centroid` produced byte-identical mzML. mzML carries one representation per
  spectrum, so the default `both` still collapses on export, but an explicit choice is now honoured.

- `tools/lcd_streams.py` — report whether a Shimadzu `.lcd` stores profile, centroid, or both, by
  reading the OLE2 stream sizes directly. No Windows, vendor DLL, or conversion needed.

- **mzML export of a dual spectrum no longer mislabels the data.** mzML holds one representation per
  spectrum. Under the faithful `both` default the reader hands the writer profile arrays *plus* a
  centroid peak list, and mzdata then serialises the typed peaks while taking continuity from the
  description — writing centroid data labelled `profile spectrum`. `.lcd → mzML` now collapses to
  the profile (less-processed) view up front, so the bytes and the label agree; an explicit
  `--representation centroid` still exports the peak lists. Because `Representation::Profile` falls
  back to whatever the file actually stores, a centroid-only `.lcd` still exports correctly-labelled
  centroids.

- **`mzPeak → mzML` says what it drops.** The reader's default preference is profile, so a
  dual-facet archive silently exported half its content. It now warns with the count
  (`2101/2101 spectra carry both a profile and a peak facet; …the peak lists are dropped`).
  Required making `ReaderMetadata::spectra` public so a caller can see the two facet counts.

- **Both chunk-encoding defaults are now chosen from the data, not the filename.** Two heuristics
  keyed on the file extension; both were measurably wrong, and both are replaced by a probe of ~6
  sample spectra spread across the run.

  *Which m/z chunk encoding.* `is_lcd()` decided delta-vs-numpress. But a Shimadzu `.lcd` read
  natively is on an exact 1e-4 lattice (residual 9.3e-10) where delta is ~3x smaller **and**
  bit-exact, while msconvert's mzML **of the same acquisition** is off that lattice (residual ~0.5)
  where delta is 1.6x **larger** than numpress. Same instrument, same run, opposite answers — the
  extension cannot decide it. `is_fixed_point_lattice()` now tests the values, requiring *every*
  sampled m/z to land on the grid rather than merely most.

  *Whether to chunk the peaks facet.* A blanket "always chunk" regresses profile-dominated runs: the
  per-chunk `chunk_start`/`chunk_end`/index columns are a fixed cost per chunk. On the peaks facet:

  | run | peaks facet | |
  |---|---|---|
  | Bruker microTOF-Q2 (centroid-only) | 38.0 MB -> 23.8 MB | **-37%** |
  | Shimadzu `.lcd` (centroid-only) | 1,547 MB -> 839 MB | **-46%** |
  | Thermo LTQ Velos (profile + peak sidecar) | 133 KB -> 178 KB | **+34%** |

  Which facet holds more points separates these cleanly on every file tested, so that is the rule.
  Across a seven-format sample the wins are kept (-12.4% Thermo FT-ICR, -37.1% Bruker microTOF) and
  every regression is gone (LTQ Velos, Waters, Agilent, Bruker CsI all unchanged). All PASS.

### Notes

- **The dual-facet path is now demonstrated on real data.** Of the four `.lcd` files available, two
  store BOTH representations and two store centroids only — settled at the container level rather
  than inferred from the reader. A `.lcd` is an OLE2 compound file carrying a symmetric pair of raw
  streams, and the unused one is present at ZERO length:

  | file | `QTFL RawData/Profile Data` | `QTFL RawData/Centroid Data` | stores |
  |---|---:|---:|---|
  | `Blind_P1_pos_012` | 48,503,000 B (87.4%) | 3,016,418 B (5.4%) | **profile + centroid** |
  | `HEK_PosOAD1` | 42,156,216 B (66.7%) | 18,511,430 B (29.3%) | **profile + centroid** |
  | `DIA_Hela_20ng` | **0 B** | 2,803,102,880 B (99.8%) | centroid only |
  | `DIA_Hela_100ng` | **0 B** | 2,828,715,992 B (99.8%) | centroid only |

  So the DIA runs were acquired with profile saving off — not a reader limitation. `tools/lcd_streams.py`
  reports this for any `.lcd` without Windows, the vendor DLL, or a conversion. Corroborated by
  enumerating all 1,280 types in `Shimadzu.LabSolutions.IO.IoModule` v3.8.4.6016: the only
  spectrum-level profile accessor is `MassSpectrumObject.ProfileList`, which is what the glue reads.

  Converting the two dual files writes both facets, every row carrying both counts:

  | | spectra | `spectra_data` | `spectra_peaks` | rows with both counts |
  |---|---:|---:|---:|---:|
  | `Blind_P1_pos_012` | 13,200 | 1,225,829 pts | 216,742 peaks | **13,200 / 13,200** |
  | `HEK_PosOAD1` | 2,101 | 11,119,571 pts | 1,543,961 peaks | **2,101 / 2,101** |

  Both PASS mzPeakValidator 0.9.1 with 0 errors and 0 warnings. The profile is genuinely dense
  (median m/z spacing 0.0044, ~6.3 points per peak, a third of them flanking zeros) and the centroid
  intensity is peak AREA rather than apex — centroid/Σprofile ≈ 0.86–0.93 versus centroid/apex ≈ 4.4–7.1.

- **The correctness fix initially cost 89% in size (818 MB → 1,547 MB), now recovered.** Chunked
  encoding was applied by `write_spectrum_binary_array_map` on the data facet only; the peaks facet
  stored flat points, with chunking gated to the ims/tof-grid path. Correctly routing this data to
  `spectra_peaks` therefore took it out of the encoding that produced the 0.7.10 win. Extending the
  chunked layout to the peaks facet (see Added) brings it to **839 MB** — within 3% of the old
  mislabelled archive, and losslessly.

  **This corrects the 0.7.10 entry below**, which described its 818 MB measurement as being on "the
  profile data the native `.lcd` lane reads". That data was centroid data mislabelled as profile.
  The delta-vs-numpress comparison itself stands — both were measured on the same bytes — but it no
  longer describes the output of the shipped default, because those bytes no longer travel through
  the chunked encoder.

- Found by two independent adversarial reviews (Codex, Kimi), both of which confirmed the diagnosis
  line-by-line and independently identified the same systemic pattern: **every** vendor lane passes
  `None` for peaks, `src/waters.rs` hardcodes `Profile` while its own glue already computes
  `IsContinuum`, `src/bruker_baf.rs` has `prefer_profile` pinned to `false` with no flag reaching it,
  and the `--tof-grid` lanes deliberately overwrite continuity to steer data into a facet. Codex also
  caught a real bug in the first draft of this change (an explicit `profile` request on a
  centroid-only file emitted an empty centroid-labelled spectrum), fixed before this entry.

## [0.7.10] — 2026-08-22

### Changed

- **Shimadzu `.lcd` now defaults to lossless delta m/z chunking instead of numpress-linear.**
  This vendor stores m/z as scaled integers (fixed-point, 1e-4), so consecutive values are
  near-constant deltas, whose byte patterns zstd compresses far better than numpress-linear's
  floating-point prediction residual. Measured on two QTOF DIA runs (21,500 and 22,113 spectra):

  | | archive | time | m/z fidelity |
  |---|---:|---:|---|
  | numpress-linear (old default) | 1,125 MB | 102 s | off the vendor lattice by up to 3.8e-3 |
  | **delta (new default)** | **818 MB** | **92 s** | **on the lattice to 3.7e-9** |

  **Smaller (−27.3%), faster, and exact** — numpress was losing on all three. The loss it introduced
  is ~1 ppb, far below any instrument's mass accuracy, so archives already written are not
  scientifically compromised; they are simply 27% larger than they needed to be.

  Numpress fidelity is **data-dependent** — it is exact on the centroid mzML export of the same
  acquisition (verified: 279,707,903 points, zero difference) and lossy on the profile data the native
  `.lcd` lane reads. So this is defaulted per vendor rather than globally. `--no-numpress` continues
  to work everywhere and is now a no-op for `.lcd`.

  **Correction (2026-08-22):** the original wording of this entry said the delta path uses
  `DELTA_BINARY_PACKED`. It does not. `null_delta_encode` is generic over `Float` and emits a
  **Float64** array (`vendor/mzpeak_prototyping/src/filter.rs:799`), and `DELTA_BINARY_PACKED` is
  assigned only to columns whose name ends in `_index`
  (`vendor/mzpeak_prototyping/src/writer/base.rs:1369`). The m/z delta column is Float64 stored PLAIN
  and compressed by zstd. The measured 27.3% win is unaffected — only the stated mechanism was wrong.
  Found by two independent adversarial reviews (Codex and Kimi), which flagged it separately with the
  same citations.

  Also measured and rejected: chunk width is already optimal at the default 50 Th (5 Th costs +15%,
  200 Th costs more too), `--layout point` is +38%, and `--zstd-level 19` buys a further 1.6% for
  2.1× the runtime — worth it for archival, not for bulk conversion.

## [0.7.9] — 2026-08-22

### Added

- **`MZPC_TOF_GRID_PPM`** overrides the TOF-grid reconstruction tolerance (default 5 ppm). Whether a
  vendor's m/z lies on a flight-time lattice is a property of the *data*, not of this code, so the
  bound needs to be adjustable to measure a candidate before committing to it. The lane is
  bounded-lossy by construction and this number IS the bound, so anything above the instrument's own
  mass accuracy is not defensible.
- **`MZPC_TOF_GRID_C1`** forces the sqrt-space grid step instead of inferring it.

  `base_step` infers the step from the spacing between *adjacent* points, which is the detector step
  only for dense profile data. A vendor that peak-detects and then rounds m/z presents sparse points
  whose spacing is orders of magnitude coarser than the lattice they actually lie on — so the fit
  "succeeds" onto a grid far too coarse and the error is pure quantization, not misfit. Measured on a
  Shimadzu QTOF DIA run: the inferred grid mapped **490,646 distinct m/z onto 42,817 indices** with
  **72.8 ppm** error, while forcing `c1 = 1.2e-6` (the step that resolves that vendor's 1e-4 m/z
  quantum at the top of its range) gives **0.12 ppm max, 0.024 ppm median**, with `k ≤ 26,023,743` —
  comfortably Int32.

  The principled choice is `c1 = quantum / (2·sqrt(mz_max))`. Deriving it automatically from the data
  is the obvious follow-up; this exposes the knob so the effect can be measured first.

### Note

  Conversion fidelity for the mzML → mzPeak path was verified exhaustively on Shimadzu QTOF DIA data:
  **21,500 spectra and 279,707,903 points compared against the source mzML with zero difference** in
  m/z, intensity, retention time, precursor m/z, spectrum id, MS level and peak counts — including
  through chunked storage and numpress-linear m/z.

## [0.7.8] — 2026-08-20

### Fixed

- **The native Shimadzu `.lcd` lane now works — it never had.** Three defects, each fatal on its own,
  none of which could surface as more than "0 spectra" or a bare HRESULT:
  - **`Api.Open` was overloaded.** The exported `[UnmanagedCallersOnly] Open(ushort*, ushort*)` sat
    beside an internal `Open(string, string)` in the same class, and the Rust host resolves exports
    **by name** through reflection. The lookup was ambiguous, so every conversion died at startup
    with `AmbiguousMatchException` (`0x8000211D`) before touching the file.
  - **CoreCLR was initialized per reader-open.** `hostfxr` refuses a second
    `initialize_for_runtime_config` in one process (`0x80008081`), so `-v` — which opens the reader
    for the inspection report and again for the conversion — could never convert. The delegate loader
    is now a process-wide singleton.
  - **Reflected calls passed boxed `int` where the vendor API declares `short`, `uint` or an enum.**
    `Invoke` demands exact value types, so the scan-count path and its probe fallback both threw
    `ArgumentException` into bare `catch` blocks and reported 0 spectra for *every* file. Arguments
    are now coerced to each method's declared parameter types, which also absorbs the signature drift
    between LabSolutions releases.
- **The m/z axis was wrong by 500×.** `MASSNUMBER_UNIT` is not exposed by the vendor assembly at all
  (verified by dumping every static field mentioning UNIT/MASS/SCALE — none), so the reflection
  lookup always fell through to a guessed `20.0`. Pinned to `10000` (masses as integers with four
  decimals), established against a msconvert conversion of the same file: raw 700000–12500000 against
  m/z 70–1250, exactly 10000 on both bounds.

### Added

- `MZPC_SHIMADZU_DEBUG=1` traces scan-count discovery and mass-unit resolution to stderr. Both failed
  silently before, which is why broken glue looked like an unsupported `.lcd` variant.

### Verification

  `MTBLS5861/HEK_PosOAD1.lcd` (LCMS-9030 QTOF) converted natively on the Windows box: **2,101
  spectra, MS1+MS2, m/z 70–1250, RT 0–16.99 min** — identical to msconvert on all four. Point-for-point
  over 16,075 points: **intensities bit-identical**, m/z within **7.9e-4 (< 2 ppm)**. msconvert
  additionally pads each profile spectrum with two zero-intensity points at the scan-window bounds;
  the native lane emits only real vendor points.

  Note for `REMOVED.md`: MTBLS432's *native* failure was attributed to an unsupported `.lcd` variant,
  but that error was `0x8000211D` — the overload bug above, which hit every file. Its msconvert
  failure (`E_UNSUPPORTEDFILE`) was genuine, so the removal stands, but that file is worth retrying.

## [0.7.7] — 2026-08-12

### Fixed

- **The corpus harness's `--box` phase crashed on an undefined name.** `run_box()` referenced
  `box_recipes`, which does not exist in its scope, so `--box` died with a `NameError` the moment it
  had any unit to defer — after the host pass had run, and before a single unit reached the
  workstation. The recipes are now threaded through as a parameter, which also fixes what the name
  was reaching for: a box-built archive uses its descriptor's own `convert.flags`, so it matches the
  recipe a host-built one would use (an SDRF demonstrator keeps `--sdrf` and its embedded
  `sample_metadata/sdrf.tsv`). `--no-vendor` remains the fallback only where no flags are described.

### Note

  Completeness is measured against the **descriptors** (`data/<tile>/<id>/<id>.yaml`), not a walk of
  the tree — the corpus publishes one representative per multi-run deposit. The harness falls back to
  the walk when PyYAML is unavailable, which inflates the denominator; run it with an interpreter
  that has PyYAML. Corpus at this release: **201/201 described archives current (100.0%)**, all
  passing mzPeakValidator 0.9.16.

## [0.7.6] — 2026-08-11

### Fixed

- **Non-finite values were written into numeric metadata columns instead of `null`.** Two paths:
  - **`lowest_observed_mz` was `+inf` on empty spectra.** A min over an empty peak list folds to
    `f64::INFINITY`, and the `> 0.0` guard meant to write null for empty spectra let it through
    because `inf > 0.0` is true. **546 empty centroid spectra across 4 reference archives** carried
    `+inf` while `highest_observed_mz` on the same rows was null — the two bounds disagreeing about
    how to express "absent", and poisoning any reader computing a file-level m/z range by
    aggregation. Both bounds now require a finite positive value.
  - **`ion_mobility_value` was `NaN`.** An Agilent 6560 DTIMS mzML in the corpus declares
    `MS:1002476 ion mobility drift time` with `value="nan"` on all 982 scans (the real drift times
    were lost upstream, before this converter ever saw the file). We propagated the NaN verbatim, so
    readers gating on `isnull()` saw 982 present-but-unusable values. Non-finite mobility is now
    `null`, and `ion_mobility_type` is only declared when a usable value exists — a declared mobility
    dimension with nothing in it makes a reader draw an empty axis.

  Found by an adversarial review of a downstream viewer's bug report; the `+inf` case was in neither
  the report nor my own triage.

## [0.7.5] — 2026-08-11

### Fixed

- **A blank profile spectrum was stored with zero points.** An all-zero intensity array satisfies the
  zero-run skip condition at *every* index, so the whole spectrum was discarded: its m/z extent was
  erased and, because the reader gates on `count > 0`, it read back with no profile at all. The first
  and last points are now kept — the same boundary zeros the filter preserves around any other run.
  Verified: a 10-point all-zero profile scan now stores 2 points and round-trips with its extent
  (m/z 200.00–200.09) intact.
- **Random access to an empty spectrum aborted the process.** `PointDataReader::slice_to_arrays_of`
  called `panic!("Could not find start and end in binary search")` when the binary search found no
  span — which is exactly what an empty spectrum looks like. Under this crate's `panic = "abort"`
  profile that terminates the host on an ordinary read, and newer timsTOF (5.1.x) emits precisely
  such frames, which this build converts. It now returns the empty array map callers already handle.
  Verified end to end on a point-layout archive containing two zero-length spectra.
- **Ion mobility was declared as a column but written as an empty list.** The chunk schema is fixed
  by sampling a few spectra; a secondary array whose decoded width differs from the sampled one
  (Waters and Agilent ion mobility arrives as f64 where the sampler declared f32) hashed to a
  different `BufferName`, missed the schema lookup, and was spilled into `auxiliary_arrays`. The
  declared column then sat EMPTY beside a full intensity list — a parallel-length violation, with the
  mobility reachable only as an opaque blob. **4,136 chunk rows across 18 corpus archives.** The
  other float widths are now aliased onto the schema's field. Verified: mobility 0 → 468,914 points,
  exactly matching intensity, with no auxiliary spill.

## [0.7.4] — 2026-08-10

### Fixed

- **mzPeak→mzML dropped every source chromatogram.** `filter_mzpeak_to_mzml` never read the
  archive's chromatogram facet, so the export carried only the writer's synthesized TIC/base-peak
  summary. On a 300-chromatogram SIM/SRM run, **299 quantitative traces vanished** on export while
  sitting intact in `chromatograms_data.parquet` — any MRM/SRM/SIM experiment round-tripped through
  mzPeak lost its quantitation. The convert lane has always carried them across; this lane never did.
  Verified: 2 → 301 chromatograms, ids identical to a direct mzdata export, 2,017,054 chromatogram
  points compared with zero mismatches. *(This corrects an earlier report in this session that said
  the defect was not reproducible — that check only saw the synthesized TIC/BPC.)*
- **A collision energy of `0.0 eV` was fabricated on every MS2 whose source declared none.**
  `Activation::energy` is a plain f32 defaulting to `0.0`, and the writer appended `MS:1000045` from
  it unconditionally — asserting a measured value that was never measured, indistinguishable from a
  real one. It is now emitted only when non-zero, matching how `peak_intensity` and
  `ion_injection_time` already treat absent-vs-zero. Verified: a source declaring 35.0 eV keeps
  `35.0` with its `UO:0000266` unit; the same file with its CE params stripped now yields none
  (previously 21 fabricated `0.0 eV`).
- **A chunk sitting at coordinate zero was silently dropped.** `decode_arrow` opened with
  `if start == 0.0 && end == 0.0 { return 0 }`, standing in for "this chunk row is absent" — but the
  bounds were read past their null mask, so a null bound and a real bound of `0.0` were
  indistinguishable. TOF bin 0 occurs in real timsTOF data (`min(tof) == 0` on the reference DDA
  run), so a chunk whose only point sits there decoded to nothing while its intensity and mobility
  arrays kept their entries: one point silently lost plus a length desync. Absence now comes from the
  null mask. Reachable with a small `--chunk-size` (measured: exactly one point recovered on a
  0.01 Th run, 993,975 → 993,976); the default 50 Th is unaffected and byte-identical (354,690 peaks
  compared, zero differences).

## [0.7.3] — 2026-08-10

### Removed

- **`--tof-delta` (per-scan TOF delta encoding) is gone, and archives that used it are rejected.**
  The writer stored per-scan deltas, but no reader — ours or the reference one — ever cumulatively
  summed them, so every TOF bin after the first in a mobility scan decoded as a tiny bin and squared
  to a nonsense m/z. Its unit test only checked a hypothetical inverse in isolation and never
  exercised the real reader, which is why round-trip checks passed. The flag, the
  `mzpeak:tof_delta_reset` marker, and the `per-scan-delta` `tof_encoding` label are all removed; the
  archive layout writes absolute bins. Reading a `.mzpeak` whose index still declares
  `tof_encoding: per-scan-delta` (anything written by ≤ 0.7.2 with the flag on) now **fails with a
  reconvert-from-`.d` message** rather than emitting silently wrong masses. No file in the reference
  corpus is affected — all ims archives there are `absolute`.

### Fixed

- **Refuse to write the output over the input.** `-o` pointing back at the source (via any path
  spelling, a symlink, or a hardlink) truncated it while the conversion was still reading — with
  `--force` this destroyed a 120 MB archive and then failed, leaving nothing. Identity is compared by
  device+inode on Unix and by canonicalized path elsewhere.
- **The filter now removes secondary metadata rows for dropped spectra.** On the v0.7 split layout,
  `classify_facet` fell through to a schema guess and copied `spectra_metadata_scans` /
  `_precursors` / `_selected_ions` wholesale, so an `--ms-level` filter left orphaned rows pointing at
  `source_index` values no longer present. Classification is now driven by the index's
  `entity_type`/`data_kind`, with the pre-0.7 schema guess retained as the fallback.
- **Portability:** the in-place guard no longer breaks the `x86_64-pc-windows-msvc` build.
- **Every `--ims-chunked` archive was unreadable.** A single-point chunk stores an empty
  chunk-values list (the start point lives in `chunk_start`), and the reader fed a hard-coded empty
  **Float64** array into the decoder for that case — pushing an `f64` into the `Int32` `tof`
  accumulator and panicking with `DataTypeSizeMismatch`. Single-point chunks are not exotic: 105 of
  415 chunks at the default 50 Th width. The placeholder now takes the main axis's own dtype.
  Verified by decoding 354,690 peaks bit-identically against the archive layout, plus a pathological
  0.01 Th run with 389,345 single-point chunks. Default (non-chunked) output is unaffected.
- **DDA-PASEF precursors now carry their parent survey frame.** `precursor_id` / `precursor_index`
  were null on every row (0 of 232,203 on the reference run), so an MS2 could not be traced to the
  MS1 it was selected from. `Precursors.Parent` is now emitted as `frame=<Id>`, which the writer
  resolves into `precursor_index` — 232,203 of 232,203 populated.
- **DDA precursor mobility uses the true fractional scan position.** We recorded the isolation
  window's integer scan midpoint instead of `Precursors.ScanNumber`, a systematic error of up to a
  full scan (747 vs 747.519 on frame 2 of the reference run, 2.8e-4 in 1/K0). dia-PASEF has no such
  value and still uses the window midpoint.
- **The truncated-source cross-check now covers the TOF-grid and `--to mzml` lanes**, which could
  previously write a silently short output with exit code 0. A refused conversion also removes its
  partial `.tmp` / output rather than leaving it beside the intended one.
- **The filter fails loudly on a malformed index instead of guessing.** An unknown spectrum
  `data_kind` used to fall through and be copied unfiltered (leaving orphans); a `source_index`-keyed
  facet whose entity the index does not identify, or an entry whose `entity_type` contradicts the
  member name, is now an error. The primary metadata member is read from the index rather than
  assumed to be `spectra_metadata.parquet`.
- **Chromatogram signal was being written into the metadata facet as an opaque blob.** The spectrum
  chunking strategy was passed through to chromatograms, producing a `chunk` struct with no
  `chunk_start`/`chunk_end` columns; the chunk builder then saw an empty main axis, wrote **0 time
  and 0 intensity points** into `chromatograms_data.parquet`, and spilled the whole intensity array
  into an uncompressed `auxiliary_arrays` blob in `chromatograms_metadata.parquet` — losing the time
  axis outright. The writer's own guard was logging `BUG: signal array IntensityArray is being
  spilled`. **99 of 330 reference-corpus archives are affected and need reconversion.** Chromatograms
  now always use the point layout (a chromatogram is a few thousand points; chunking bought nothing).
  Verified: TIC/BPC restored to 3,574 points each with a correct 0.008–20.019 min time range, and a
  300-chromatogram SRM file round-trips 1,011,900 points with zero spilled arrays.
- **Filter flags on a raw input were silently ignored.** `mzpeak-convert run.mzML --ms-level 2
  --rt 5-6` wrote the complete 3,574-spectrum archive and exited 0 — `--rt` / `--ms-level` / `--mz` /
  `--drop-aux` are implemented only on the mzPeak-input lane. They are now a hard error naming the
  convert-then-filter sequence instead of quietly producing an unfiltered result.
- **`--no-ims-compact` on a Bruker `.d` failed outright** with "Is a directory (os error 21)": the
  Latin-1 transcode and param-group sanitize workarounds are XML-file-only and were applied to the
  directory. `convert_to_mzml` had always gated them on `is_file()`; this lane did not. The same gap
  hit the ims-compact decompress fallback.
- **`--ims-chunked` scan/range queries mis-attributed points.** The scan decoder's single-point-chunk
  branch recovered the point but never extended the entity-index accumulator, leaving it one short
  per single-point chunk — a length mismatch at assembly, or points attributed to the wrong spectrum
  if that validation were bypassed. (Found by review; the per-spectrum decoder was unaffected, which
  is why the round-trip check passed.)
- **`--bruker-sdk` help and manual claimed it "implies f64 m/z".** On a TDF `.d` it writes the
  integer-TOF ims-compact layout like the native path; f64 needs `--no-ims-compact` as well.
- **Manual §9 still described `--ims-chunked` bounds as m/z min/max**; they hold main-axis TOF values.
- **Filtering no longer leaves dangling parent references.** With the new precursor linkage in place,
  an `--ms-level 2` filter drops exactly the survey spectra the precursors point at, so all 289
  precursor and selected-ion rows on the reference run kept a `precursor_id` / `precursor_index`
  naming a spectrum no longer in the archive — breaking the conformance MUST that every non-null
  foreign key resolves. Those columns are now nulled when the parent does not survive (survivors keep
  their original `index`, so no remapping is needed). Verified: 0 dangling refs, 0 orphans in every
  facet.
- **Filtering a pre-0.7 packed archive silently attached the wrong precursors.** Its `spectrum` /
  `scan` / `precursor` / `selected_ion` columns are parallel but independently packed — precursor
  slot *j* holds the *j*-th precursor in the run, not the precursor of spectrum *j* — so masking rows
  by `spectrum.index` keeps the wrong slots. Measured on a real 4,880-spectrum packed archive built
  with v0.6.0: `--ms-level 2` left 3,904 MS2 spectra of which **782 lost their precursor entirely and
  3,082 got someone else's — only 40 were correct**, with no warning. Spectrum filtering on that
  layout is now refused with a reconvert message (matching the mzPeak→mzML lane, which already
  refused it); a pure aux drop/inject still works, since it copies facets verbatim.
- **The `--bruker-sdk` lane wrote every MS2 frame with no precursor at all.** It never set
  `descr.precursor`, so the lane that exists specifically for files the native reader cannot decode
  silently dropped the entire precursor facet. Both lanes now share one `build_precursors`, with the
  SDK lane using the vendor's own `tims_scannum_to_oneoverk0` for the mobility. *(Compiles here but
  runtime-unverified — the SDK lane needs Windows/Linux and the timsdata library.)*
- **`cv_list` declared a psi-ms version that does not resolve.** It claimed `4.1.248` behind a
  versioned OBO purl that 404s for every release. CURIEs actually resolve against the CV bundled in
  the mzdata we link, which is `4.1.249`; the declaration now states that version behind a tagged
  URL that returns 200.
- **A zero-byte vendor marker misrouted the input to the wrong reader.** `is_tdf_dir` / `is_tsf_dir` /
  `is_baf_dir` tested only that `analysis.tdf` / `.tsf` / `.baf` *existed*. Real corpora carry
  zero-byte stubs from partial downloads or archive extraction: an Agilent `.d` with a 0-byte
  `analysis.tdf` beside its `AcqData/` was sent to the Bruker TDF lane and died with "no such table:
  GlobalMetadata", so its real format was never attempted. 7 of 358 corpus units failed this way;
  they now report their actual format correctly.
- **Corpus-gated tests had silently rotted.** All four pinned corpus paths that no longer exist and
  looked up signal columns at the schema root, where the v0.7 layout nests them inside the
  `point`/`chunk` struct — so they failed for reasons unrelated to the code. Fixtures are now located
  by search (missing ⇒ skip, not fail) and column checks flatten the struct. Three
  machine-specific absolute scratch paths were also removed from the committed source.

### Known issues

- **Layout-family purity conflicts with the split peaks/profiles layout** (see
  `mzPeak-spec-issue-layout-family-purity.md`). The reference writer routes chunked profile arrays to
  `spectra_data.parquet` and point-format centroids to `spectra_peaks.parquet`, both
  `entity_type: spectrum`, which the conformance MUST forbids. 140 of 330 corpus archives declare two
  families; 6 have both populated with real data. Raised with the specification rather than patched
  unilaterally, since the reference layout design and the rule disagree.

## [0.7.2] — 2026-08-04

### Changed

- **Vendored `mzpeak_prototyping` `474a7c2` → `589d6e3`.** `column_mapping.path` is now a
  dot-delimited string rather than an array of segments, matching the array index and the
  specification's normative form. Also brings upstream's layout-aware cache load, its own chunk
  dispatch in `get_spectrum_peaks_for`, and split (threaded) point queries.

### Fixed

- **Absent values are written as `null`, not `0.0`.** `peak_intensity` and `ion_injection_time` were
  0.0 on backends that do not report them (dia-PASEF has no `Precursors` table; neither timsTOF nor
  imzML records an injection time), which asserted a measured zero and round-tripped into literal
  `peak intensity = 0` cvParams. Real values are untouched.
- **The `time` column is now mapped** (`MS:1000016`, `UO:0000031`), so its minute unit — a MUST in
  `docs/schemas/spectra.md` — is discoverable from the index.

### Known

- `isolation_window_target` remains float32 against metadata-tables.md's "prefer 64-bit doubles".
  Widening it would imply precision we do not have: mzdata's `IsolationWindow.target` is itself f32,
  so the f64 TDF value is downcast before reaching the writer. Needs an upstream mzdata change.
- `--ims-chunked` still declares a `point` array index on its empty `spectra_data` facet beside the
  chunked peaks facet, against the new layout-family purity rule in `conformance.md`. The index is
  emitted at writer construction, before row counts are known, so suppressing it needs restructuring.

## [0.7.1] — 2026-07-28

### Fixed

- **Truncated XML sources no longer produce a silently partial archive.** An imzML whose `.ibd`
  sidecar was an incomplete download converted to a structurally valid archive containing 4,550 of
  its 34,840 spectra, with exit code 0 and nothing naming the loss. mzML/imzML conversions now compare
  the written spectrum count against the source's own `<spectrumList count=>` and abort without
  writing when they differ. Skipped when `MZPC_MAX_SPECTRA` caps the run, and for non-XML formats,
  which declare no authoritative count.

### Added

- **`tools/corpus_reconvert.py`** — idempotent raw→mzPeak harness for the example-data corpus.
  Currency is judged from the archive (zip opens, split-facet marker present, `.built` stamp matches
  the converter version) rather than timestamps, so re-running converges instead of redoing work.
  `--box` sends host-unsupported vendor formats to the flash workstation, verifying and if necessary
  rebuilding its converter first — the box had no version guard, so it could silently convert with a
  stale binary. Reports completeness, host-unconvertible units, stale pre-0.7.0 archives, and dates
  the set by its oldest member.

## [0.7.0] — 2026-07-23

### Changed — BREAKING: split-facet metadata, bare column names

Re-vendors `mzpeak_prototyping` from upstream `d0fdb0b` → `474a7c2`, adopting the metadata-storage
refactor the reference implementation made to track the specification's revised metadata-table model
(HUPO-PSI/mzPeak-specification `e7f3447`).

- **Metadata facets are now separate Parquet files** joined by `source_index` —
  `spectra_metadata_scans/_precursors/_selected_ions` and the chromatogram equivalents — instead of
  nested struct columns in one packed table.
- **Columns use bare names** (`ms_level`, not `MS_1000511_ms_level`), with the CV binding carried in
  the index's `column_mapping`, which the converter now emits.
- **`data_kind`** uses the new controlled values (`data_arrays`, `scans`, `precursors`,
  `selected_ions`), written as `data_arrays` with a `data arrays` read alias.
- **Reading pre-0.7.0 packed archives is no longer supported** — upstream removed that path. Such
  files now fail with a clear message telling you to reconvert, rather than panicking deep in the
  reader. The mzPeak→mzPeak **filter still reads both layouts**, so existing archives stay filterable
  until the corpus is reconverted.

### Fixed

- **`MS:1003901` / `MS:1003902` were swapped** in the vendored copy (`MS:1003901` is zero-intensity
  trimming, `MS:1003902` the interpolation variant). Upstream has it right; this corrects reading
  spec-conformant third-party files.
- **Chromatogram `scan_polarity`** wrote `0`, which is not a legal value (`1`, `-1` or null).
- **Chromatogram `chromatogram_type`** wrote the abstract parent `MS:1000626`; it now carries the
  real children `MS:1000235` (TIC) / `MS:1000628` (base peak).
- The spectrum-metadata RAM spool is dropped as **obsolete, not regressed** — it existed because the
  packed metadata entry could only open at finish, and each facet now has its own streaming writer.

### Dependencies

- **mzdata 0.65.4 → 0.65.5**; `serde_arrow` stays at 0.14.2 / arrow-59. Incoming upstream code was
  ported from arrow 57 to our arrow 59 pin.

## [0.6.0] — 2026-07-22

### Added — timsTOF MS2 precursors and isolation windows

- **Precursor information is now extracted from the TDF.** It was previously discarded entirely:
  `precursor` and `selected_ion` were null for every spectrum, including all MS2, which made DDA and
  dia-PASEF output unusable for identification. A TDF MS2 frame is a full TIMS ramp and the
  quadrupole retunes during it, so one frame carries N isolation windows over disjoint mobility
  ranges (~1.6 per frame for DDA-PASEF, 5.0 for dia-PASEF). mzML has nowhere to put the mobility
  dimension so mzdata splits these into N spectra; mzPeak does, so the frame stays whole and carries
  N precursors. The two acquisition modes are stored in completely different tables and a file has
  only one set, so the loader probes `sqlite_master` rather than assuming. Verified against ground
  truth: 23,818 precursor rows vs 23,818 TDF windows (DDA), 15,977 vs 15,977 (dia-PASEF).

### Fixed — more conformance with mzPeak-specification HEAD (`9e61e32`)

- **`--ims-chunked` archives read back as empty spectra.** `PeakMetadata::from_metadata` hardcoded
  the Point index variant regardless of the facet's actual layout, and `get_spectrum_peaks_for` had
  no chunk branch. Both now select on the facet's own array-index prefix. Verified: the same input
  converted archive-vs-chunked decodes to identical content — 18/18 spectra agree on the m/z
  multiset and on (m/z, intensity) pairs, differing only in ordering.
- **Per-scan TOF delta encoding is now opt-in (`--tof-delta`), default off.** The point layout
  requires values be stored as-is so the Parquet page index stays meaningful, and delta encoding
  defeats that. Retained as a flag because it is genuinely useful (~3% smaller) and is a proposed
  spec change; the help text says plainly that it is non-conformant.
- **`--ims-chunked` chunk bounds** now hold the first/last value of the main axis (TOF bins), matching
  the declared `MS:1000786` / `UO:0000189`, and the delta **start point is excluded** from
  `chunk_values` as the chunked layout requires.
- **A dangling instrument-configuration foreign key** — `run.default_instrument_id` and every
  `scan.instrument_configuration_ref` pointed at configuration 0 while the list was empty. The model,
  serial and TOF analyzer are now promoted from the `.d`'s GlobalMetadata. The ion source and detector
  are deliberately not guessed from Bruker's opaque `InstrumentSourceType` code.
- **`run.start_time`** is filled from the vendor acquisition timestamp.

### Fixed — no operator paths in archives or the repository

- Archives embedded the operator's filesystem twice: the verbatim command line as the "conversion
  options" param, and an absolute `file://` directory as the source file's `location`. Conversion
  options now keep the flags but reduce path-shaped arguments to basenames, and `location` records
  the bare `file://` authority — the source is already identified by `name`.
- 26 hardcoded `/Users/...` paths in tracked files (test constants and the two corpus manifests) are
  gone; the corpus root resolves through `MZPEAK_CORPUS`, defaulting under `$HOME`.
- `.gitignore` now covers `tools/box.env*`, so a `.bak` of the credentials file cannot be staged.

### Changed — dependency pins

- **arrow / parquet 57.0.0 → 59.1.0**, **mzdata 0.65.2 → 0.65.4**, **thermorawfilereader 0.7.0 →
  0.7.2** (forced by mzdata 0.65.4). `timsrust` (0.4.1) and `rusqlite` (0.31) stay put — mzdata pins
  both transitively, so 0.6.3 / 0.40.1 are unreachable until mzdata moves. Output is unchanged:
  parallel vs serial encoding stays byte-identical and the reconstructed peak fingerprint over
  74,162,464 peaks matches the arrow-57 value exactly.

### Fixed — conformance with mzPeak-specification HEAD (`9e61e32`)

An adversarial review against the specification found these; all are verified against real output.

- **`--ims-chunked` output was undecodable.** Every array-index entry carried `transform: null` and
  no coefficients, so the TOF→m/z model lived only in the non-standard `ims_calibration` block. A
  reader resolving transforms through the array index — as `docs/conformance.md` requires — could not
  reach m/z at all. The transform CURIE and its `[a, b]` parameters are now declared on the chunk
  axis, matching the archive path. Chunk payloads are unchanged.
- **Empty dia-PASEF frames were written as MS1.** They are `MsMsType=8` (MS2) in the TDF; the
  empty-frame path had no MS level to report and a downstream `.max(1)` clamp turned that into a
  fabricated MS1. MS level now comes from `Frames.MsMsType`.
- **Scan polarity was dropped for timsTOF** although `Frames.Polarity` carries it.
- **`MS_1000559_spectrum_type` read `MS:1000294` for every spectrum**, including all MS2, because a
  valueless per-spectrum param outranked the MS-level-derived value. Now correctly `MS:1000579` /
  `MS:1000580`.
- **The `MS_1000294_mass_spectrum` Boolean column asserted `false` on every mass spectrum.** Removed
  from all six writer paths; the corrected `spectrum_type` column already satisfies the
  `spectrum_must` placement rule it was added for.

### Fixed — duplicate spectrum ids on timsTOF runs containing empty frames

- **Empty TDF frames were given the *previous* frame's spectrum id.** The empty-frame fast path
  (`NumPeaks=0`, where timsrust can't decode the header-only blob) filled `RawFrame.index` with the
  0-based loop position, while timsrust reports the **1-based** TDF frame `Id` for every other frame
  — so an empty frame at position *p* was written as `frame=p` instead of `frame=p+1`. On a 41,175
  frame run with 32 empty frames this produced **26 duplicate ids**.
- Duplicate ids collapse the reader's `id_index`, which sizes the per-spectrum metadata vectors by
  the *unique* id count and then indexes them by the `index` column — so any full-metadata read
  (notably `-o out.mzML`) **panicked** with `index out of bounds`.
- Peak data was never affected (ids are metadata only); the fix is id-only and leaves the encoded
  peaks byte-for-byte identical. **Files converted before this fix keep the duplicate ids and should
  be reconverted** if they contain empty frames.

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
