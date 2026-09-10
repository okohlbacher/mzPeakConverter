# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/), and the project adheres to
[Semantic Versioning](https://semver.org/).

## [Unreleased]

**Output change.** An indexed mzML that declares a non-UTF-8 encoding now keeps its source
chromatograms in both lanes (below). No corpus archive is affected: the one corpus mzML that is
both indexed and non-UTF-8, `general-ms/thermo-ltq-orbitrap-velos/`
`TMT_Erwinia_1uLSike_Top10HCD_isol2_45stepped_60min_01-20141210.mzML` (ISO-8859-1), declares only
a `TIC`, which synthesis replaces anyway, and its archive
`TMT_Erwinia_1uLSike_Top10HCD_isol2_45stepped_60min_01.mzpeak` is converted from the `.raw`. The
other non-UTF-8 XML sources are a non-indexed mzML without a `<chromatogramList>`
(`bruker-microtof-q2`) and imzML.

### Added

- **Release archives for Linux and Windows.** Beside the two macOS archives, every release now
  publishes Linux x86_64 and aarch64 (`.tar.gz`, built in `manylinux_2_28` so they start on glibc
  2.28 and newer — RHEL/Rocky/Alma 8 and 9, which a build against Ubuntu 22.04's glibc 2.35 would
  not) and Windows x86_64 and ARM64 (`.zip`, with the .NET glue for the native SciEX, Shimadzu and
  Agilent readers under `glue\`). Each archive is built natively on its own architecture and
  verified there — the PE machine field or the glibc floor, a smoke conversion, the Agilent host
  booting, Shimadzu's BinaryFormatter switch — and published with a `.sha256` sidecar. One job now
  attaches everything after re-checking every sidecar, so the platform jobs no longer race to create
  the release, and a platform that fails no longer holds back the others. A pull request that edits
  the workflow runs the whole matrix as a dry run; `workflow_dispatch` takes `only` to backfill a
  platform onto an existing tag without rebuilding the archives already published. The vendor
  readers are unverified on Windows ARM64: the vendor DLLs are x64, so use the x64 archive there.
- **The converter finds its .NET glue beside the executable.** With `MZPC_SCIEX_GLUE`,
  `MZPC_SHIMADZU_GLUE`, `MZPC_AGILENT_GLUE` or `MZPC_AGILENT_MIDAC_GLUE` unset it looks in
  `glue\<name>\` next to `mzpeak-convert.exe` — the Windows release archive's layout — so an
  unpacked release needs none of them; a variable that is set still wins. Pinned host-independently
  by `pwiz_layout::tests::glue_dir_prefers_the_variable_then_the_release_layout`.
- **Native Bruker archives carry the LC system's device traces.** A timsTOF `.d` records its
  pumps, column oven and autosampler in HyStar's `chromatography-data.sqlite`, which only the mzML
  lane (through ProteoWizard) used to read; the native TDF and TSF lanes wrote the synthesized TIC
  and BPC alone. Every Bruker lane now opens that file read-only and writes each trace after the
  TIC/BPC, with ProteoWizard's `chromatogram title` and `Instrument` parameters: a pressure,
  flow-rate or temperature trace as that PSI-MS chromatogram type with a pressure, flow-rate or
  temperature array, anything else (solvent composition, setpoints, valve angles) as ProteoWizard's
  generic `chromatogram` (MS:1000625) with a non-standard array named after the trace, each in the
  unit HyStar states. The type is a parameter as well as the typed column, so an mzML export of the
  archive states it. The value arrays are stored as auxiliary arrays, which keep their own unit, and
  times are minutes like every other chromatogram. HyStar's own MS traces give way to the
  synthesized TIC/BPC, as a source TIC/BPC always has; that includes its MS/MS TIC
  (`TIC,±AllMS/MS`, on every corpus run), which the spectra still yield. A user-defined trace whose
  unit is a pressure or a flow rate is typed as one (ProteoWizard does that only for a temperature).
  mzdata has no unit for bar, so a trace in bar is stated in pascal, as 64-bit floats that divide
  back to the stored value exactly, and the archive declares `bruker:trace-unit-rescale` in
  `transformations`. HyStar stored the four Thermo pump and column-oven traces of PXD079300's
  `…_27806.d` in overlapping chunks, every sample three times and out of time order (1,079,478
  points for 359,826 samples on each pressure trace): such a trace is written in time order with
  each exact (time, value) repeat once, and the archive declares `bruker:trace-sort-dedup`. A
  database in WAL mode is skipped with a warning, since SQLite cannot open one without creating
  files beside it; no corpus file is in WAL mode. The 32 corpus runs with the file hold 698 such
  traces, 139 of them in bar (5 on each of the 27 PXD059079 runs, 2 each on PXD076703 and
  PXD078573); their published archives change only when reconverted. On the TSF run behind
  ProteoWizard's `timsTOF_autoMSMS_Urine_50s_neg` test file the six Elute traces match its mzML in
  values, times, unit and chromatogram type (the two solvent traces through the `chromatogram`
  parameter; their typed column stays null). Pinned by `bruker_traces::tests` (an in-memory HyStar
  database) and `tests::finish_chromatograms_writes_the_bruker_device_traces`, which also checks
  that the input directory is left untouched.

### Fixed

- **An indexed mzML declaring a non-UTF-8 encoding lost all its chromatograms, with exit code 0.**
  mzdata's reader is UTF-8 only, so an ISO-8859-1 / latin1 / windows-1252 input is transcoded into
  a UTF-8 temp copy first, and that rewrite changes byte lengths: `encoding="ISO-8859-1"` becomes
  the five-bytes-shorter `encoding="UTF-8"`, and every high byte becomes two. Every `<offset>` in
  the copy's `<indexList>`, and its `<indexListOffset>`, then pointed at the wrong byte. mzdata
  failed to read the index (said only at debug level: `close tag </indexList> does not match any
  open tag`), fell back to a scan that finds the spectra, and could no longer enumerate the
  chromatograms, which it reaches only through that index. On `tests/fixtures/tiny.pwiz.1.1.mzML`
  the mzPeak lane logged `2 synthesized + 0 from source`, and `-o x.mzML` wrote only the writer's
  own TIC/BPC: the `sic` trace was gone from both. The transcoder now rebuilds the copy's index,
  each offset from the new position of the `<spectrum`/`<chromatogram` tag carrying its id and
  `<indexListOffset>` from `<indexList`, and drops its `<fileChecksum>`, which hashes the original
  bytes and which mzdata never checks. imzML goes through the same transcoder but has no XML offset
  index (its offsets point into the `.ibd`): a Latin-1 imzML and a same-length UTF-8-declared copy
  convert identically. The `convert_to_mzml` comment that blamed spectrum iteration for lost
  chromatograms was describing this failure and is corrected. Pinned by
  `tests/latin1_indexed_mzml_chromatograms.rs`, including a variant with high bytes.
- **Reading fewer chromatograms than the source declares is now a warning**, in both lanes and
  whatever the cause: `<chromatogramList count>` is checked against what the reader yields. The
  only warning before fired for a non-indexed mzML, so a stale index lost traces in silence.
- **An indexed mzML with an empty `<referenceableParamGroup id="…"/>` lost all its chromatograms,
  with exit code 0.** mzdata panics on such a group once it is referenced (ProteomeDiscoverer
  emits them), so the converter reads a sanitized copy in which each is written as an open/close
  pair. Only the header before `<spectrumList` changes, but it grows, so every `<offset>` in the
  copy's `<indexList>`, and its `<indexListOffset>`, pointed short of its element. mzdata failed to
  read the index (said only at debug level: `close tag </run> does not match any open tag`), fell
  back to a scan that finds the spectra, and could no longer enumerate the chromatograms, which it
  reaches only through that index: `tests/fixtures/tiny.pwiz.1.1.mzML` declared UTF-8 with one
  empty group logged `2 synthesized + 0 from source`, and the `sic` trace was gone from both lanes.
  The rewrite is header-only, so every indexed element moves by the same number of bytes: the
  copy's offsets are now shifted by that delta, the body is still streamed, and only the index tail
  is read into memory. The copy drops its `<fileChecksum>`, which hashes the original bytes and
  which mzdata never checks. A source whose `<indexListOffset>` does not point at its `<indexList>`
  is copied as before. No corpus archive is affected: no corpus mzML carries an empty group. Pinned
  by `tests/empty_param_group_indexed_mzml_chromatograms.rs`.
- **`--ims-chunked` on timsTOF now writes ONE layout family for the spectrum entity.** The path
  chunked only the peak facet and left the — empty, centroid-only — data facet at the point default,
  so every chunked timsTOF archive was point `spectra_data` beside chunk `spectra_peaks`: on 0.9.2
  the writer's one-family-per-entity check aborted the conversion outright ("layout family mismatch
  between spectrum facets", ~2 s in, on two diaPASEF runs); since 0.9.3 relaxed that check to a
  warning it wrote the mixed archive and warned. The data facet is now declared chunked too, with
  the same chunk-shaped fields as the peak facet, so an empty `spectra_data` still carries a
  well-formed chunk schema and the entity keeps one layout family (the scope proposed in
  HUPO-PSI/mzPeak-specification#21) without leaning on the relaxation, which is untouched:
  dual-representation archives still pick per facet. Only the empty `spectra_data` member changes.
  Verified on PXD059079 `…_2499.d` and a private diaPASEF run: both `spectrum_array_index` footers
  say `chunk`, and `validate_everything.py` passes at max sensitivity (0 errors, 0 warnings). The
  default (point) layout is unchanged.
  Regression test: `ims_chunked_spectrum_facets_share_one_family` (corpus-gated, `--ignored`).
- **Opening a Bruker TSF `.d` no longer writes into it.** `TsfReader::open` used rusqlite's default
  open, which is read-write and CREATES a missing file, so opening a `.d` that has no `analysis.tsf`
  left an empty one inside the user's raw data. A normal conversion reaches the TSF reader only for
  a `.d` with a non-empty `analysis.tsf` and no non-empty `analysis.tdf`, so it never did this; a
  direct open did, and the corpus still holds one such stub — created 2026-08-10, beside a TDF run's
  real 192 MB `analysis.tdf` — that had passed for a TSF fixture. Opened read-only now, like the
  converter's other SQLite opens (timsrust, which the TDF lanes use, still opens an existing
  `analysis.tdf` read-write, after an existence check). A read-only open of a file with a hot
  journal now reports SQLite's own error rather than "GlobalMetadata missing/invalid". Pinned by
  `bruker_tsf::msms_tests::open_never_writes_into_the_input_directory`.
- **`--to mzml` on a SciEX `.wiff` refuses what the mzPeak lane refuses, and honours
  `--sample`.** The 0.11.3 refusals (MRM/SIM dwell runs, unreadable samples, a multi-sample
  file without `--sample`) and the sample selection ran on the native mzPeak lane only; the
  mzML export opened the reader and streamed every spectrum. On Windows,
  `En_PPY.wiff -o x.mzML` wrote every dwell of all 117 samples as one-point spectra of a
  single run and exited 0, and `--sample N` was ignored. Both lanes now open the file
  through `SciexReader::open_run`, and the decision moved out of the Windows-only reader
  into `src/sciex_run.rs`, whose tests run on every host. The refusal messages are
  unchanged; a `--sample` above `i32::MAX` is now refused as out of range instead of
  wrapping past the bound.
- **`--via-msconvert --to mzml` no longer passes a previous run's mzML off as this run's.**
  msconvert was handed the final output path as `--outdir`/`--outfile`, and success meant
  `output.exists()`. Under `--force` a file left by an earlier run satisfied that check, so
  the command exited 0, logged "wrote …" and left the old mzML in place. Real ProteoWizard
  gets there with `--force` over an existing `-o x.mzML.gz` (or `.mzml` on Linux), for which it
  writes a different file name. msconvert now writes into a fresh directory beside the output
  (not the temp dir, so the rename cannot cross a volume), created exclusively under a hidden
  `.mzpc-msconvert-<pid>-<clock>-<n>` name and never reused, so neither a same-pid run on shared
  storage nor a crashed run's leftovers can supply its mzML; only the file it
  wrote there is renamed into place through `TmpGuard` like every other mzML export, and a
  `.mzML.gz` output is gzip-compressed from that file rather than left to msconvert's
  naming. The directory is removed on every error return, taking a crashed msconvert's stray
  `.partial` with it. The same guard stops the `--via-msconvert` mzPeak lane leaking
  `mzpc-msconvert-<pid>` in the temp dir when msconvert is not found, which returned before
  any cleanup. `tests/mzml_export_atomic.rs` drives the lane with stand-in scripts: against
  the unfixed build, one that exits 0 without writing produced exit 0 and
  "wrote …/out.mzML" over the untouched previous file; it must now fail with that file
  byte-identical and nothing beside it. One that writes its mzML must land under the
  requested name, gzipped for `.mzML.gz`, and reparse.
- **`--via-msconvert` refuses a multi-sample WIFF without `--sample` instead of keeping its
  last sample.** With one `--outfile`, msconvert writes every run of a multi-run source onto
  that path in turn and the last one wins (En_PPY: 117 samples, one survived), under exit 0
  and without a warning. Only the native SciEX lane refused such a file, and it runs on
  Windows alone, while the msconvert lanes also run under Wine with a user-supplied msconvert.
  msconvert prints `writing output file:` once per run before writing it (`processFile` in
  pwiz's `msconvert.cpp`), so both msconvert lanes, mzPeak and `--to mzml`, now count those
  lines in the log they already capture and refuse more than one, giving the count for
  `--sample <1..N>`; the `--to mzml` lane removes the file msconvert left at the output path,
  and now passes `--sample` on as `--runIndexSet` as well, without which the refusal would
  have had no way out. Dropping `--outfile` and counting the mzML files instead is not
  enough: pwiz names each run `<wiff>-<sample name>`, so samples that share a name overwrite
  each other there too. The refusal comes after msconvert has converted every run, as the
  truncated conversion did. `--sample 0` is refused for every lane; the msconvert lanes used
  to turn it into run index 0, sample 1. `tests/msconvert_multi_run.rs` pins both directions
  with a stand-in msconvert that writes two runs, or one when `--runIndexSet` picks it.
- **The `.mzpeak` filter lane no longer refuses archives with wavelength spectra.** Every
  Parquet member is classified, and the UV/PDA scans facet (`entity_type=wavelength_spectrum`,
  keyed by `source_index`) fell into the "index does not identify its entity" refusal. `--rt`,
  `--ms-level`, `--drop-aux`, `--no-vendor`, `--sdrf` and `--image` therefore all exited 1 on any
  archive holding a `wavelength_spectrum` facet — Waters and Agilent PDA/UV runs included — even
  with no spectrum filter given. Wavelength facets reference only each other and are now copied
  whole; `--rt` says once that it does not truncate them.
- **`--drop-aux` refuses to remove a core facet.** Drop globs matched every member, so
  `--drop-aux '*.parquet'` wrote an archive holding nothing but its index, and dropping
  `spectra_peaks.parquet` or `spectra_metadata_precursors.parquet` wrote an unreadable one — each
  with exit 0. A glob that matches a `spectrum` facet whose `data_kind` is not proprietary/other
  now exits 1 before anything is written. `--no-vendor` still drops the Thermo `vendor_*` facets,
  which are declared proprietary, and `--drop-aux 'wavelength_spectra*'` still strips a UV/PDA
  trace: those facets reference only each other.
- **`--ms-level` and `--rt` fail on a missing or retyped column.** An `ms_level` that was absent
  or not UInt8 read as level 0, and a `time` that was absent or not Float64 as NaN, so a writer
  type change would have made either filter keep 0 spectra and exit 0.
- **`--rt` refreshes chromatogram point counts in current archives.** The refresh knew only the
  pre-0.7 nested `chromatogram` struct, so the flat `chromatograms_metadata.parquet` the converter
  writes today was copied verbatim: on `tiny.pwiz.1.1` converted by 0.11.5, `--rt 0-0.0001` left
  2 points in `chromatograms_data` while the metadata still declared `[3, 3]` and a footer total
  of 6. Both now follow the truncation. Without `--rt` the facet is copied verbatim rather than
  re-encoded.
- **A release is built only from a commit that passed CI.** `release.yml` runs no tests, and
  `windows.yml` cancelled a push's run as soon as the next commit reached `main` — so v0.10.0
  (85afceb), v0.10.1 (5692603) and v0.11.3 (4ff30a6) were released with their `windows` job
  cancelled, never having finished a Windows build and test. A first job now reads the commit's
  check runs and waits, up to 90 minutes, until `build-test (ubuntu-latest)`,
  `build-test (macos-latest)` and `windows` have concluded; anything but `success`, cancellation
  included, stops the release before a platform job starts and names the check. The newest run of a
  job counts, so re-running a cancelled one clears the way — which a backfill of those three tags
  now needs first. A `workflow_dispatch` checks the commit its tag points at, a pull request's dry
  run the pull request's head. Pushes to `main` no longer cancel each other's Windows run, and each
  gets its own concurrency group: with cancellation off GitHub still replaces a run *pending* in a
  group, so the middle one of three quick pushes would never run. Pull requests still cancel a
  superseded run.
- **Reading an archive back keeps each spectrum's precursors in their source order.** The vendored
  reader attached a spectrum's (and a chromatogram's) precursors in reversed row order, so
  mzML → mzPeak → mzML turned `[(445.3, 445.34), (645.3, 645.34)]` (isolation target, selected ion)
  into `[(645.3, 645.34), (445.3, 445.34)]`, and mzdata's `precursor()`, the first, named a
  different precursor depending on whether the spectrum was read from mzML or from mzPeak. Every
  `-o x.mzML` export of a multi-precursor spectrum (PASEF, SPS-MS3, MSX) was affected; the archives
  were written in source order and do not change, and mzpeakts, the HUPO-PSI python reader and
  OpenMS read them in that order. Found by the strengthened `tests/multi_precursor_roundtrip.rs`.

### Changed

- **The ignored tests run, and no test that runs by default passes without asserting.** Of the six
  `#[ignore]`d tests, four now run by default on every platform, on data already in the repository:
  `by_id_reads_the_peaks_facet_on_a_centroid_only_archive` on the committed centroid-only fixture
  instead of a 59 MB corpus mzML; `random_access_to_empty_spectrum_does_not_abort` converts that
  fixture's genuinely empty spectrum to the point layout and asserts it comes back found and empty
  while its neighbours keep their peaks (its corpus walk opened about 3 GB of archives and never
  checked); `fractional_scan_number_moves_mobility` loads PXD078573 9629.d's calibration row through
  `from_tdf` from an in-memory table and asserts the 1/K0 values its corpus version printed,
  exactly; and `mzml_output_preserves_srm_chromatograms` writes its chromatogram-only mzML in the
  test and checks the selected-ion trace's id, point counts and intensities at each hop, plus
  exactly one TIC and one base-peak trace in each mzML — it had pinned a corpus archive whose
  contents had drifted, and compared counts.
- **Eight tests that printed `skipping`, or nothing at all, and passed wherever the corpus was
  absent — CI included — now run on 1.3 MB of committed public fixtures** (sources in
  `tests/fixtures/README.md`): both `gridded_spectrum_summaries` tests and
  `tof_grid_subpath_embeds_sdrf` (the ProteoWizard SWATH mzML, gzipped), the Agilent scan-record
  polarity test, and the Waters, Shimadzu and two Agilent run-metadata tests. `tests/fixtures/**` is
  marked `-text`, so a Windows checkout keeps every fixture byte-identical — git's autocrlf would
  otherwise rewrite `tiny.pwiz.1.1.mzML` and invalidate its indexedmzML offsets.
- **The seven tests that genuinely need data too large to commit are `#[ignore]`d, with the
  reason,** so CI reports them as not run rather than as passed: the two ims-compact tests and the
  two `tests/tdf_*` tests (2485.d, 142 MB), the TSF pin (the corpus holds no TSF acquisition, and
  the private runs it was checked against cannot be committed), and the two lane-parity tests (pairs
  built on the Windows box). The ims-compact pair is pinned to 2485.d — a sorted walk of the corpus
  had silently switched it to a 1.7 GB run — and now removes its scratch, which left about 5–7 GB in
  `$TMPDIR` per run; `unexpected_and_stale` no longer passes on an empty or mistyped pair directory;
  and the TSF pin refuses a directory that is not a TSF run.
- **One gate for the corpus tests** (`tests/common/corpus.rs`, shared by the unit and integration
  tests): `MZPEAK_CORPUS`, else `~/Claude/mzpeak-example-data/data`. A missing fixture prints
  `SKIPPED` straight to stderr, and `MZPC_REQUIRE_CORPUS=1` turns it into a failure. The TSF and
  lane-parity pins skip the same loud way when their own variable is unset — no corpus can supply
  those inputs, so they skip even under `MZPC_REQUIRE_CORPUS=1` — and fail on a path that does not
  exist. The gates had disagreed: some read only `$HOME`, and several returned without a word.
- **`ims_compact_is_frame_preserving` and `contract_ims_compact_calibration_keys` no longer share a
  scratch directory.** Both used `mzpc-test-{pid}` and extracted the same facet names, so in a
  parallel run one truncated the Parquet file the other had just opened ("Parquet file too small.
  Size is 0"); run one at a time they passed.
- **The `FrameMsMsInfo` → precursor mapping behind the 0.11.3 TSF precursors is pinned on an
  in-memory table** (`bruker_tsf::msms_tests`), since no TSF acquisition is available for the
  end-to-end pin.
- **CI saves Rust caches only from `main`, and the release workflow saves none.** The repository's
  Actions cache stood at 10.58 GB against GitHub's 10 GB limit, 13 of its 17 entries (7.0 GB) being
  release-build caches saved by pull-request dry runs, a backfill dispatch and a tag build. The
  Windows jobs now share one cache, which only the `windows` job saves.
- README: run the suite with `cargo test --release`, as CI does; the vendored writer's
  `debug_assert`s can fail a plain debug run on inputs the release build handles.
- **The filter lane has tests.** `tests/filter_lane.rs` is the first for `src/filter.rs`: on
  `tiny.pwiz.1.1.mzML` converted in the test, `--ms-level 2` keeps one spectrum and nulls its
  `precursor_index`; `--rt 0-0.0001` keeps one spectrum, the chromatogram points inside the window and matching
  `number_of_data_points`; both again through `-o f.mzML`; `--rt` open bounds (`10-`, `-30`) and
  refused ranges (`5-1`, `a-b`). It also pins the four filter-lane fixes above: `pda_uv.pwiz.mzML`
  filtered with `--ms-level 1 --sdrf`, and `drop_aux_refuses_to_remove_a_core_facet`. Each test
  owns its scratch directory, so the `mzpc-test-{pid}` race above cannot recur there.
- **The Thermo `.raw` lane is tested.** Nothing exercised it before, although CI installs
  .NET 8 for this reader. The untested parts were the `vendor_scan_trailers`,
  `vendor_status_log` and `vendor_scan_trailers_wide` facets (`src/thermo_trailers.rs`,
  `src/thermo_status.rs`), their embedding, and the Thermo-only `DOTNET_ROLL_FORWARD`
  default (0.9.12). `tests/thermo_raw.rs` converts `tests/data/small.RAW` with the built
  binary, runs by default and needs no corpus. The file is mzdata's 48-spectrum LTQ FT run
  (Apache-2.0, 1.5 MB; provenance in `tests/fixtures/README.md`). `DOTNET_ROLL_FORWARD` is
  removed from the child's environment. The test asserts 48 spectra, trailer ordinals 0–47,
  a non-empty status log and one wide-trailer row per spectrum. On a host without .NET 8
  the conversion depends on that default: with `DOTNET_ROLL_FORWARD=Disable` it fails with
  "It was not possible to find a compatible framework version". With .NET 8 installed, as
  on CI, that part passes either way.
  `thermo_status::tests::sanitize_label_collapses_runs_and_trims` pins the wide facet's
  column names (`Ion Injection Time (ms):` → `Ion_Injection_Time_ms`, `::` → `col`).

## [0.11.5] — 2026-09-09

### Fixed

- **An omitted selected-ion m/z is written as `null`, not `0.0`.** mzdata's `SelectedIon.mz`
  is a plain `f64`, so a source that does not report `MS:1000744` handed the writer a `0.0`
  that no consumer could tell from a measured value. Bruker diaTracer mzML routinely omits
  the term, carrying only charge and peak intensity. Every consumer that prefers a *present*
  selected-ion m/z over the isolation-window target then worked from precursor m/z 0:
  measured on a 3,086,644-spectrum diaPASEF run, FASTag returned **0 tags where the same run
  as mzML gives 62,347,705**, with exit code 0 and no warning. `null` is how the spec spells
  "absent" (docs/layouts/metadata-tables.md, *Null semantics for metadata*), and it is
  already what this writer does for `intensity`, `ion_injection_time` and `ion_mobility`.
  A stated m/z is unaffected. Archives written before this fix are readable either way:
  mzpeak-openms 929650f treats a stored `0.0` as absent.

## [0.11.4] — 2026-09-09

### Added

- **Homebrew on macOS.** The tap lives in this repository, so a released build installs
  without a Rust toolchain:

  ```sh
  brew trust --cask okohlbacher/mzpeak/mzpeak-convert
  brew tap okohlbacher/mzpeak https://github.com/okohlbacher/mzPeakConverter
  brew install --cask okohlbacher/mzpeak/mzpeak-convert
  ```

  Homebrew 6 loads nothing from a third-party tap until it is trusted, and a repository
  not named `homebrew-mzpeak` needs its URL given to `brew tap` — hence all three names
  in full. A `v*` tag now builds both macOS architectures (`x86_64` cross-compiles on the
  arm64 runner in ~90 s, so no Intel runner is needed), publishes each archive with a
  `.sha256` sidecar and repoints the cask at them (`.github/workflows/release.yml`,
  `tools/update_homebrew.sh`). Homebrew quarantines every cask download and macOS kills a
  quarantined binary that carries no Developer ID, so the cask strips that attribute from
  the executable it installs and its caveats say so; signing and notarizing the release
  would let that stanza go, and the workflow marks where that belongs, including the
  entitlements the .NET-based Thermo reader needs. A formula was prototyped and dropped:
  in one tap it needed a second `brew trust`, collided with the cask on `bin/mzpeak-convert`,
  and its `on_macos`-nested URL made `brew tap` fail validation outright.

### Fixed

- **Waters MSe on Xevo: the elevated-energy ramp is read from the method text.** Xevo MSe methods
  state it as `MS Collision Energy Low (eV)` / `MS Collision Energy High (eV)` under a `TOF PARENT
  FUNCTION` section, not as the Synapt's `Transfer Collision Energy Ramp Start/End (eV)`; the 0.11.3
  corpus archive of PXD052561 (`20231129_NM4_Xevo_MSe.raw`, 65–75 eV) therefore carried no
  MS:1002013/1002014 on its 1,358 MSe precursors. Both spellings are read now.

## [0.11.3] — 2026-09-09

**Output change.** The native lanes now write run metadata they used to drop (below), the Bruker
TSF lane writes precursors, and the SciEX native lane refuses MRM/SIM dwell runs. Archives of
those lanes written by 0.11.2 are not current; the corpus is rebuilt once with the next release.

### Added

- **Run metadata the vendor states, read natively (`src/run_metadata.rs`).** Measured by
  `tests/lane_metadata_parity.rs` on 2026-09-07, the native lanes carried none of the run-level
  metadata the mzML lane inherits from ProteoWizard: no sample, no acquisition time, no serial,
  no vendor model term, no acquisition-software version, one synthesised source-file entry
  instead of the digested members, and a generic `file_description.contents`. A shared seam now
  merges what each vendor file STATES onto the archive, field by field and idempotently, and
  every lane declares its members with MS:1000569 SHA-1s:
  - **Bruker TDF/TSF** — `GlobalMetadata` (`analysis.tdf`/`analysis.tsf`, read-only): timsTOF
    family term MS:1003123, `InstrumentName` as the MS:1000031 value, serial, TOF analyzer,
    `AcquisitionSoftware` + version (MS:1000692), `SampleName`, the zoned
    `AcquisitionDateTime`; members `analysis.tdf` + `analysis.tdf_bin` (MS:1002817/MS:1002818) or
    `.tsf` + `.tsf_bin` (MS:1003282/MS:1003283). A 0-byte `analysis.tsf` beside a real `.tdf`
    (PXD076703) no longer wins the lookup.
  - **Agilent `.d`** (`src/agilent_meta.rs`, any host) — `AcqData/Devices.xml` (MS:1000490,
    model name, model number, serial, the analyzers the device type implies), `Contents.xml`
    (`AcquiredTime` WITH its stated UTC offset, MassHunter MS:1000678 + `AcqSoftwareVersion`),
    `sample_info.xml` (sample name); members = the AcqData files ProteoWizard lists, minus
    exported text formats and dot/AppleDouble names.
  - **Waters `.raw`** (`src/waters_meta.rs`, any host) — `_HEADER.TXT` (`Instrument` →
    MS:1000126 + model + serial unless `#NotSet`, `Acquired Name` + descriptors → sample,
    `Acquired Date/Time`), `_extern.inf` (`Created by` → MassLynx MS:1000534 version); members
    `_FUNCnnn.DAT` in numeric order (Waters nativeID format) then the side files.
  - **SciEX `.wiff`** (glue `RunInfo`/`RunString`, Windows) — MS:1000121 + `InstrumentName`,
    serial, Analyst MS:1000551 + `SoftwareVersion` (e.g. `SCIEX OS 3.0.0.3339`), the sample's
    name, `AcquisitionDateTime`; members `.wiff` + `.wiff.scan` (MS:1000562/MS:1000770),
    digested BEFORE Clearcore2 opens them.
  - **Shimadzu `.lcd`** — the MS:1002998 family term beside the `SystemName` value;
    `SampleInfo.AnalysisDate`.
  `file_description.contents` now states what was written (MS1/MSn spectrum, centroid/profile,
  TIC chromatogram) instead of the generic `mass spectrum` (the adapter injects MS:1000294 only
  when no data-file-content child is present).
- **Acquisition time policy.** A vendor time WITH a stated offset (Agilent `Contents.xml`,
  Bruker `GlobalMetadata`) becomes `run.start_time` verbatim. A wall clock WITHOUT a zone
  (Waters, Shimadzu, SciEX) does NOT: RFC 3339 has no "zone unknown", so `run.start_time` stays
  null and the clock is preserved verbatim in a `metadata.acquisition_time` index block
  `{wall_clock, zone: "unstated", source, note}`. ProteoWizard labels the same clock `Z`
  (Capan2) or shifts a stated one by the CONVERTING host's zone (blank1: 18:11Z for a file that
  says 13:11:27-04:00 = 17:11Z) — claims the native lane refuses to make.
- **Bruker TSF precursors.** Every MS2 frame carries its `FrameMsMsInfo` row: selected ion
  `TriggerMass` with the stated charge (unstated stays null), isolation window ±`IsolationWidth`/2
  (target only when the width is unstated), CID with the signed `CollisionEnergy`,
  `precursor_id = frame=<Parent>` so `precursor_index` resolves. 30/30 against the pwiz twin of
  the urine fixture. Pinned by `tests/run_metadata_native.rs` (set `MZPC_TSF_FIXTURE`).
- **Waters ion mobility (HDMSe / HDDDA) read natively, as frames.** The native `.raw` lane read the
  drift-SUMMED spectrum (`readScan`) and lost the drift dimension: Capan2 (Synapt G2-Si HDMSe) came
  out as 1,989 summed scans where ProteoWizard writes 397,800 per-drift-bin spectra (1,989 × 200).
  Every function whose `_funcNNN.cdt` exists and whose `getDriftScanCount` is > 0 is now read bin by
  bin (`readDriftScan`) and written as ONE spectrum per MassLynx scan — a frame — whose points are
  sorted by (m/z, drift time) and carry a per-point `raw ion mobility array` (MS:1003007, ms), the
  shape of pwiz's own `--combineIonMobilitySpectra` output and of the Bruker ims-compact lane. The
  frame states its drift-time bounds (MS:1003439/1003440), `transformations` gains `sort-by-mz`,
  and a `waters_drift` index block carries the run's bin → ms table, `mob_cal.csv` verbatim, the
  lock-mass function and the functions that were not written (below). Frames are written with the
  writer's zero-run mask OFF — in the interleaved frame a run of zeros is several bins' trace
  boundaries meeting, and the mask kept only its first and last zero — so every point MassLynx
  returns is stored, and the spectra are in acquisition-time order across functions, as pwiz orders
  them. Verified bin by bin against pwiz's per-bin spectra on Capan2 frames of every function:
  on nine frames of functions 1–3 the non-zero point multisets and intensities are identical in
  1,799/1,799 populated bins, m/z within 2.6e-7 (numpress on both sides), the drift table identical,
  frame TIC = Σ pwiz per-bin TIC, RT identical; the only difference is MassLynx's two zero-intensity
  sentinel points at the scan-window bounds (m/z 49.98 and 600.06 on Capan2) that every bin carries —
  the native lane keeps them, pwiz strips them. The frame archive is 531 MB where the per-bin twin is 965 MB and the summed archive was
  166 MB.
  **Per-scan metadata and precursors from the SDK.** Retention time (`getRetentionTime`; it was 0.0
  on every row), polarity (`getIonMode`), scan window (`getAcquisitionMassRange`), the MS level from
  the function-type CODE (`getFunctionType`, pwiz's table: MSMS / MS2 / TOFD / QUAD AUTO DAU are MS2,
  the second function of an MSe pair is MS2, every other MS function is MS1; SIR / MRM / NL / NG
  functions are chromatograms and DAD / DLY / CAT / OFF / PSD / AutoSpec ones are not spectra — both
  are skipped with a log line, and a file with no spectrum function is refused with the msconvert
  remedy), the lock-mass function (`getLockMassFunction`, else the method's REFERENCE section) and
  the **scan items** through the MassLynx parameters object (`createParameters` /
  `getScanItemsInFunction` / `getScanItemValue`): SET_MASS > 0 becomes a selected ion with a
  target-only isolation window, COLLISION_ENERGY the activation energy (beam-type CID), and an MSe
  elevated-energy scan whose set mass is 0 gets a precursor whose isolation window IS the function's
  acquisition mass range (target = midpoint, bounds = the range) with an activation parameter
  `isolation window source = acquisition mass range` and no selected ion — ProteoWizard's convention,
  adopted after establishing that nothing in the file or the DLL states any narrower window (the
  quadrupole is non-resolving in MSe; `getFunction/IndexPrecursorMassRange` and `getPrecursorMass`
  answer only for SONAR, and the DDA processor's quad-isolation-window parameters are 0/0 unless a
  `_dda.inf` sidecar supplies them). A DDA set mass keeps its target-only window: its width is not
  stated either; the tune page's quadrupole settings (`LM/HM Resolution`, `MS Profile Type`,
  `MSProfileMass/Dwell/Ramp 1..3`) travel verbatim as instrument-configuration parameters so a reader can
  bound the RF-only pass band (Capan2: 682 precursor rows on the 682 high-energy scans, where pwiz writes the same
  window on each of the 136,400 drift-bin spectra). On a Fast-DDA HDDDA run (PXD073126, ten IMS functions) the frames are identical to pwiz's bins the
  same way and 600/600 precursor rows agree with pwiz's (selected ion, target-only window, CE). The
  transfer collision-energy ramp
  comes from the method text (`_extern.inf`) as MS:1002013/1002014. `SONAR Enabled` is read per
  function: a SONAR function's bins are quadrupole positions, not drift times, so it is written as the
  drift-summed scan with a warning and `sonar: true` in the block rather than mislabelled as drift
  (`sonar_checked` is false when the file's item table has no such item — Capan2's 44-entry table). **Collapsed retention-time functions** (Capan2 functions 4–6:
  one row per drift bin whose "retention time" is the drift table — run-summed mobilograms of
  functions 1–3, which pwiz writes as 200 × 200 mostly empty spectra) are recognised structurally
  and not written as spectra (`MZPC_WATERS_KEEP_COLLAPSED=1` keeps them). The C shapes of all these
  exports differ from what pwiz's C++ wrapper suggests and were established by probing on the box
  (`src/waters.rs` header): `getDriftTime` has no function argument, `getAcquisitionMassRange` has
  a fifth, function type and ion mode come back as codes, and scan items are reachable only through
  the parameters object (the DLL's item table starts at 401).
- **Shimadzu acquisition start, sample and LabSolutions version from the `.lcd` itself
  (`src/shimadzu_meta.rs`, any host).** The file's OLE2 `File Property` stream holds
  `SampleInfo.DateTime` as a UTC FILETIME (split into `dwLow/dwHighDateTime`) beside the writing PC's
  own GMT offset (`szLocGMTDiffGenDateTime`, `+01'00'`), so the native lane now writes a fully zoned
  `run.start_time` (Blind: `2024-02-15T11:47:18.756+01:00`), the sample name/id/vial/operator/
  injection volume and `LabSolutions 5.114`. Established by a byte-level search of the file (the
  OLE2 directory FILETIMEs and the MS-CAB local stamps inside the same file agree on UTC and on the
  +1 h). The vendor DLL exposes the same instant as `SampleInfo.AnalysisDate`, which comes back empty
  in this converter's .NET 8 host; ProteoWizard reads it and then shifts it by the CONVERTING host's
  current offset (`adjustUnknownTimeZonesToHostTimeZone`), writing `08:47:18Z` — two hours early.
  The `cfb` crate is a new dependency.
- **`--sample N`** selects the sample of a multi-sample WIFF (1-based; the msconvert lane maps it
  to `--runIndexSet N-1`). A multi-sample WIFF without `--sample` is refused and lists its samples.

### Fixed

- **The writer masked zero-intensity runs on every profile spectrum of the chunked data facet,
  whatever it was built with.** `write_spectrum` passed `is_profile` where the chunk builder expects
  the `drop_zero_intensity` flag, so `mask_zero_intensity_runs = false` never reached the data facet.
  No released archive changes — every lane passed `true` and declared `zero-run-mask` — but the
  Waters frame lane, the first to turn the mask off, still lost its bins' zero flanks until this
  fix. `tests/frame_zero_runs.rs` pins it: a four-bin interleaved frame round-trips 56/56 points
  with the mask off under numpress, delta and basic chunking, and 52/56 with it on.

- **SciEX MRM/SIM dwell runs are refused by the native lane** (`En_PPY.wiff`,
  `IPX0002633001_D-239.wiff` in the corpus): it stored each dwell as a one-point spectrum without
  its transition (154,520 and 2,215 "spectra"), where msconvert writes SRM chromatograms with Q1/Q3,
  compound and collision energy. The glue classifies every experiment by type; a run whose
  experiments are all MRM/SIM dwells exits 1 naming `--via-msconvert`, and the box harness routes
  it there (`tools/box_convert_remote.ps1`). MRM-HR (scan) runs still convert natively. Unreadable
  samples are refused too, instead of being read as empty.
- **Target-only isolation windows keep NULL offsets.** An isolation window whose width the
  source does not state was written with lower/upper offsets of ±target (measured on RS080806,
  Minimal_DDA and En_PPY). Pinned by `tests/fixtures/target_only_window.mzML`.
**Output change.** Archives of a centroid ion-mobility mzML gain the per-peak mobility column
they were missing (below); nothing else changes bytes. The corpus rebuild picks them up.
- **The mzML lane silently dropped the per-peak ion-mobility array of a CENTROID IMS spectrum.**
  mzdata's mzML reader builds a `CentroidPeak` set eagerly for every centroid spectrum, and the
  vendored writer followed `peaks()` for both the peak-facet schema
  (`ArrayTypesSampler::visit_spectrum`) and the peak rows (`write_spectrum_data`). That set holds
  only m/z + intensity, so an MS:1003006 `mean inverse reduced ion mobility array` (pwiz
  `--combineIonMobilitySpectra` output — or any other per-peak array, a charge array included)
  left behind in `raw_arrays()` reached neither a column nor `auxiliary_arrays`:
  `spectrum_array_index` listed m/z + intensity and `number_of_auxiliary_arrays` was 0 on every
  `*-combineIMS-*centroid` archive of the pwiz Bruker corpus. A centroid spectrum whose raw
  arrays carry more than its peak set is now routed — schema and rows — through its raw arrays
  (`centroid_arrays_beyond_peaks`, `vendor/mzpeak_prototyping/src/writer/base.rs`), so the
  mobility lands as `chunk.mean_inverse_reduced_ion_mobility` (chunked layout) or
  `point.mean_inverse_reduced_ion_mobility` (point layout) and reads back bit-identical. A
  spectrum with nothing beyond m/z + intensity keeps the peak-set path and its schema unchanged.
  A second hole on the same path is closed with it: `ChunkBuffers::add_raw_chunked` and
  `add_raw_mz_boundary` discarded the chunker's auxiliary arrays, so an array absent from the
  chunk schema was dropped instead of spilled to `auxiliary_arrays`; they are now handed up.
  Pinned by `tests/ims_centroid_mobility_array.rs` over the new
  `tests/data/pasef_combineims_centroid.pwiz.mzML` (pwiz `Reader_Bruker_Test.data`, PASEF frame 6
  combined over its 100 scans, 1391 peaks), in both layouts.

- **pwiz Waters MSe archives carried `isolation_window_lower_offset = 0`.** mzdata 0.66.6's mzML
  reader keeps only the FIRST isolation-window offset when both offsets precede the target m/z (the
  second falls into a `_ => {}` arm while the window is in its `Offset` state), and ProteoWizard's
  Waters writer emits exactly that order — upper offset, lower offset, target
  (`SpectrumList_Waters.cpp:308-316`). Every precursor of the Capan2 pwiz twin (136,400/136,400)
  read `{target 325, lower 0, upper 275}` for a window pwiz declares as 325 ± 275 (50..600). Fixed
  in the reader on our mzdata fork, pinned through `[patch.crates-io]` at the same 0.66.6
  (`Cargo.toml`), and sent upstream as
  [mobiusklein/mzdata#58](https://github.com/mobiusklein/mzdata/pull/58) — drop the patch and bump
  the pin once a release carries it. Pinned by `tests/isolation_window_offset_order.rs` on a fixture
  that lists the offsets in both orders. Any mzML-lane archive whose source lists the offsets before
  the target (all pwiz Waters MSe/HDMSe twins) is not current and needs a rebuild; the native Waters
  lane is unaffected (it builds its precursors itself: set mass, acquisition-range MSe window).

### Changed

- **Empty spectra stay.** The SciEX native lane writes every acquired spectrum, including the
  zero-point ones ProteoWizard drops (SWATH: 11,583 of 148,571; a scheduled MRM-HR run: 16,790 of
  23,646). Measured cost after re-encoding the metadata facets without them: 0.007–0.012 % of the
  SWATH archive and 0.2–0.6 % of the MRM-HR archive — an empty row compresses to ~7–20 B. Keeping
  them preserves the cycle structure; nothing changes here.
- `tests/lane_metadata_parity.rs` compares VALUES, not just presence: `run.start_time` as an
  instant, serial and model strings, per-member digests, `id@version` software, sample names,
  the contents set and the `acquisition_time` wall clock; a rule that no longer fires is an error.
## [0.11.2] — 2026-09-07

**Output change.** The footer count keys of the data facets change meaning (below) — every
`spectra_data`, every `spectra_peaks` with zero-peak spectra, and every `chromatograms_data` and
`wavelength_spectra_data` (new keys) — so archives written by 0.11.1 are not current: the corpus
is rebuilt once with this release, as usual. Parquet data bytes do not change.

### Fixed

- **Data-facet footer counts describe THIS file, not the run — and an empty `spectra_data`
  says 0 (issue #1, pjones).** The vendored writer stamped the archive-wide spectrum ordinal as
  `spectrum_count` on `spectra_data` and every metadata facet (and on `spectra_peaks` the number
  of centroid spectra handed to it, zero-peak spectra included), and on `spectra_data` a
  `spectrum_data_point_count` that was the SUM of both data facets' points. A centroid-only run therefore shipped an empty
  `spectra_data.parquet` declaring every spectrum and every peak of `spectra_peaks` (84 of the 201
  published archives; thermo-orbitrap-astral declared 307,590 on 0 rows), a mixed profile/centroid
  run over-declared its non-empty `spectra_data` (at least 22 archives — the largest facets were
  not scanned; PXD076001 declared 255,623 where 2,521 spectra have rows), and a reader that plans from the footer — the issue's C++ dumper —
  queried hundreds of thousands of spectra that were not there. The definition now: on a DATA facet
  (`spectra_data`, `spectra_peaks`, `chromatograms_data`), `<entity>_count` is the number of
  entities with at least one row in this file — a cardinality, never an index bound (facet indices
  are sparse), never the run total — and `<entity>_data_point_count` is the points in this file.
  The run total stays on the primary metadata facet (`spectra_metadata`, `chromatograms_metadata`).
  Implemented as an `entry_count` beside `point_count` in the data-facet buffers
  (`vendor/mzpeak_prototyping/src/writer/array_buffer.rs`), counted where rows are stored, so every
  write path (peak lists, raw arrays, chunked, ims-chunked, TOF-grid) is covered; `spectra_peaks`
  now counts spectra WITH rows rather than spectra handed to it (a zero-peak spectrum is not an
  entry — at least 18 corpus archives were off by exactly their zero-peak spectra).
  `chromatograms_data` gains `chromatogram_count` and `wavelength_spectra_data` gains
  `wavelength_spectrum_count` under the same definition. The metadata secondaries
  (`_scans`, `_precursors`, `_selected_ions`) keep the run total: no count of theirs is "the"
  count, and nothing reads it. The archive rewrite path (`src/filter.rs`) already used this
  definition on data facets. Pinned by `tests/footer_counts.rs` over `tiny.pwiz.1.1.mzML` (1/10
  profile, 2/30 peaks with the empty spectrum excluded, run total 4) and the new
  `tests/fixtures/tiny_centroid_only.mzML` (0/0 on `spectra_data`) and the PDA-UV
  `tests/fixtures/pda_uv.pwiz.mzML` (8/8 on the wavelength facets). Data bytes are untouched: the
  Parquet bodies of the fixture archives are byte-identical to their 0.11.1 builds, only the
  footers differ. The keys are not defined by the specification; the definition above is to be
  proposed to HUPO-PSI (draft in the analysis directory), together with an issue against the
  upstream python reader, which uses `spectrum_count` as its default iteration bound and already
  reads 0 of 52 spectra from an `--rt`-filtered archive — it needs to prefer its row-group-max
  fallback.
- **`wavelength_spectra_metadata_scans` declared `wavelength_spectrum_count = 0` on a populated
  table.** The count was read from the metadata builder AFTER `finish_spectrum()` had drained it;
  the published PDA-UV archive shows 8 rows / 0. Taken from the builder's ordinal counter now,
  before the drain.

### Added

- **`tests/lane_metadata_parity.rs` — does the native lane carry the same metadata as the mzML
  lane?** Given pairs of archives built from the SAME source by both lanes, it extracts a
  normalised metadata surface from each (index blocks, run block, source files and checksums,
  instrument configurations, software, the column set and population of every metadata facet, the
  chromatogram inventory, the categorical histograms), diffs them, and classifies every difference
  as `ByDesign` (the lanes legitimately differ — layout, codec blocks, or the native lane carrying
  MORE, such as its MZP grid coefficients) or `Defect` (metadata the native lane could carry and
  does not). Anything matching neither fails the test, so a new loss cannot land unnoticed; the
  `EXPECTED` table is the written record of the known ones. `tools/lane_pairs.ps1` builds the pairs
  on the Windows box from its raw cache, since the native lanes exist nowhere else; without
  `MZPC_LANE_PAIRS` the test skips like the other corpus-gated tests.
- First run over four pairs — Shimadzu `.lcd`, Agilent 5977B GC-MS `.d`, and two SciEX `.wiff`:
  83/12/7, 83/6/11, 39/12/51 and 40/12/50 facts identical / by design / tracked losses. The two
  SciEX units are MRM acquisitions, where the lanes disagree about what the data ARE — see
  `BACKLOG.md`.

## [0.11.1] — 2026-09-07

### Fixed

- **The by-design refusal introduced in 0.11.0 skipped the msconvert fallback it was supposed to
  label.** `tools/box_convert_remote.ps1` gained an `elseif` that recognised a refused Agilent unit
  (MRM/SIM dwell data, IM-QTOF) and set `path=refused->msconvert` so `box_convert.sh` would not
  raise its native-failure alarm — but that branch sat ahead of the `else` that actually runs
  msconvert, so it intercepted exactly the units that need the fallback. The native non-zero exit
  survived, no archive was written, the upload gate rejected the job, and the driver reported
  CONV-FAIL one line after announcing that the msconvert archive was the intended output. The three
  refused corpus units (PC_Allan1, MTBLS243, FM_01_Pos) would have hard-failed the next rebuild —
  which 0.11.0 forces, since every `.built` stamp reads 0.10.2 and `OUTPUT_COMPATIBLE` groups only
  0.7.6–0.7.8. Nothing could have been lost (the box publishes only on verified success), but the
  units would have frozen at 0.10.2 and the run would have reported failures. Classification and
  fallback are now separate: the branch chooses the note, then every non-zero exit falls through to
  the conversion, with the ramdisk free-space guard covering the refusal route too (FM_01_Pos is a
  1.2 GB `.d` that always takes it). The classifier also collapses whitespace before matching,
  because PowerShell wraps native stderr under `*>` and a line-wise match can miss a split phrase;
  a misclassification now costs only the wording of a warning, never an archive.

## [0.11.0] — 2026-09-06

The Agilent native lane, restored (owner decision 2026-09-04, option A, time-boxed and gated on a
real conversion), plus the Windows-only dead-code sweep the same decision bundled with it.

### Added

- **Native Agilent `.d` (MHDAC) conversion on Windows.** `src/agilent.rs` is the subprocess reader
  from cc8245e (2026-06-27) that merge 5a62b90 dropped: it spawns the net48 `AgilentGlueHost.exe`
  (`glue/agilent`, unchanged design) once per `.d` and reads the host's binary output back
  through a new host-testable parser, `src/agl.rs` (3 unit tests that run on every OS, the same
  pattern as `pwiz_layout`). Two adaptations from cc8245e: the MHDAC directory comes from
  `pwiz_layout::agilent_dll_dir` (both ProteoWizard layouts), and no blanket `MS:1000294` is
  attached. Verified on the box against the msconvert lane: a 5977B GC-MS run (MTBLS11742
  blank1.D, 7,017 scans) is identical spectrum for spectrum — same 83,850 points, per-spectrum
  TIC equal to the last digit, RT to 1e-13 min; a 6545 Q-TOF profile run (agilent-qtof S25,
  242 MB, 1,502 scans, 181,196,503 points) converts in 20 s with the same points and RT and,
  on the spectrum compared point by point, intensities equal to the last digit (m/z within
  1.07 ppm of the msconvert+`--tof-grid` build, which is that build's own quantization).
- **Protocol `AGL2`.** The host now writes MHDAC's `ScanTypes` and the instrument identity
  (device type, device name, serial) ahead of the offset table; the converter refuses an `AGL1`
  file with "rebuild glue/agilent" instead of guessing where the table starts.
- ⚠️ **MRM/SIM-only runs are refused by the native lane** and pointed at `--via-msconvert`.
  MHDAC presents a 6490 dMRM `.d` as one one-point "MS2 spectrum" per dwell — 27,674 of them for
  MTBLS243, 11,728 for PC_Allan1 — while the data are the 113 / 201 SRM transition chromatograms
  the msconvert lane writes; the guard (`agl::is_dwell_only`) keys on MHDAC's scan-type names, so
  a run with any scan type keeps its scans and the corpus harness falls back to msconvert for the
  dwell-only ones — verified on the box: both 6490 units exit 1 with the message and no output,
  the GC-MS and Q-TOF runs report `Scan` and convert. The instrument model is recorded from MHDAC's
  device type and name (`QTOF (QuadrupoleTimeOfFlight)`, `SingleQuadrupole`); the serial number is
  not yet (the msconvert lane has it — the MHDAC member is still to be found, backlog).

### Changed

- ⚠️ **IM-QTOF runs are refused by the native lane too.** `AcqData/IMSFrame.bin` non-empty means a
  6560 drift-tube run; the MIDAC lane that would carry the drift dimension is still the in-process
  scaffold MHDAC-family DLLs cannot run under, so the converter refuses rather than flattening the
  run through MHDAC. The corpus unit MSV000090203 FM_01_Pos.d is such a run (it was misremembered
  as Bruker); it stays on msconvert.
- **Safety and diagnostics around the host** (review findings, all landed): the `.part` file a
  natively crashed host leaves is removed with the `.bin`; `MZPC_AGILENT_TMPDIR` places the
  materialised run on disk when `TEMP` points at a RAM disk (the box scripts set it); a missing
  `MassSpecDataReader.dll` is reported before the spawn, naming `MZPC_PWIZ_DIR`; a successful
  host's stderr is logged at warn level (it now reports how many NaN/Inf intensities it stored as
  0); a scan whose retention time MHDAC could not supply is written as NaN by the host and stored
  as 0.0 with one counted warning (0.0 used to be both the value and the sentinel); a mixed
  Scan+MRM method warns that its dwells are stored as one-point spectra; `--tof-grid` on this
  lane warns that it is not applied; inspecting an Agilent `.d` never fails the run (it prints the
  scan types and instrument, or the refusal as a note); the box scripts mark a refusal as
  `path=refused->msconvert` so it does not trip the native-failure alarm. The Shimadzu
  centroid-rotation warning, once raised by the removed `sample_arrays()` before the writer
  opened, is now raised at the first centroid fetch — same warning, later.
- **Pins and CI.** `agl::glue_writes_what_this_parser_reads` pins the C# writer's magic, string
  order and record layout against the Rust parser on every host (the Shimadzu-style source pin,
  before drift can ship); the Windows workflow boots the built `AgilentGlueHost.exe` and expects
  its usage exit, the only MHDAC-free execution available.
- ⚠️ **Corpus routing.** Under the native-first box policy the Q-TOF unit `agilent-qtof/…S25` will
  convert natively on the next rerun (f64 m/z, numpress-chunked by default: 245 MB against the
  200 MB msconvert+`--tof-grid` build; `--tof-grid` on this lane is a backlog item), the two
  6490 dMRM units stay on msconvert via the refusal, the 5977B GC-MS unit converts natively with
  identical spectra.
- **Windows-only dead code swept** (the seven items the 0.10.0 `cfg_attr` change surfaced, plus
  what the restore made unused): the ignored `sample` parameter of the shared vendor writer and
  every reader's `sample_arrays()` (probes supply the schema since 0.9.x), `library_path`
  (BAF, timsdata), `calibration_used` (BAF), `is_empty` on five readers, Shimadzu's unread
  `spectrum_meta` v1 pointer (the export is still resolved by name), `analysis_date` (never
  recorded — a naive local time) and `lcd_path()`. The Windows build is warning-free apart from
  the vendored reader's one.
- The stale net8 `glue/agilent/AgilentGlue.runtimeconfig.json` is deleted;
  `tools/box_local_convert.ps1` points at the same ProteoWizard 3.0.26175 as the remote script;
  README, `docs/PLATFORM_SUPPORT.md`, `docs/USER_MANUAL.md`, `glue/agilent/README.md` and
  `BACKLOG.md` say the lane is wired, what it refuses, and what it costs (the host materialises
  the run at 16 B/point, ~3 GB for the 242 MB Q-TOF `.d`, removed on close).

## [0.10.2] — 2026-09-06

### Fixed

- ⚠️ **`run.default_instrument_id` is an integer again.** 0.10.0 wrote `null` for a run without an
  instrument record, calling the previous `0` a dangling reference. The specification's run block
  requires the field as an integer (`schema/ms_run.json`), so the first corpus rebuild under the
  0.10.x line produced three archives the validator refuses (`index_schema_valid`,
  `meta_run_valid`: "None is not of type 'integer'"): an LA-ESI imzML, a timsTOF `.d` without an
  instrument record, and a Bruker impact II `.d`. The normaliser now inserts one EMPTY instrument
  configuration `0` when the list is empty and points the run at it — what mzML does for an
  unknown instrument — so the reference resolves and the schema holds. The 0.10.0 changelog entry
  and manual §7 sentence that announced the `null` are superseded by this one. Found by the
  validator on the 0.10.1 corpus rerun, which was stopped and restarts under this tag.

## [0.10.1] — 2026-09-06

The two archive-content items the owner sequenced first after 0.10.0 (review ledger M6 and the
speXtract handoff's F5), sharing one corpus rebuild. ⚠️ marks an entry that changes what an archive
contains; the fix is verified on macOS (host suite green) and on the Windows box.

### Changed

- ⚠️ **TOF-grid lanes file each spectrum by the representation its source declares (M6).** The
  integer `tof_index` axis is now declared on BOTH facets — `spectra_data` (point layout) for
  profile spectra and `spectra_peaks` for centroid ones, each beside an f64 `mz` that is NULL on
  gridded rows — through one shared field definition (`tof_index_field`) and one peaks schema
  (`tof_index_peak_schema`, now carrying the f64 fallback like the `mz-grid` lattice facet). No
  route rewrites `signal_continuity` any more: the four forcing assignments (mzML `--tof-grid`
  gridded → centroid and off-grid → profile, native SCIEX gridded → centroid and off-grid →
  profile) and the Agilent `--agilent-grid` lane's blanket centroid are gone; the Agilent lane
  states Profile, which is what `MSProfile.bin` holds, and writes to `spectra_data`. Consequences
  in the archive: `spectrum_representation`, `number_of_data_points` and `number_of_peaks` mean
  what the source said (a gridded profile row now carries `MS:1000128` and `number_of_data_points`
  where it carried `MS:1000127` and `number_of_peaks`), and a reader tells a gridded row from an
  f64 one by which column is non-null, never by facet. Verified: the pwiz ABI 7600 ZenoTOF profile
  example (21 spectra, all gridded at 3.9 ppm) round-trips through `-o x.mzML` with every intensity
  identical and every m/z within the declared 5 ppm, its 118,835 points all in `spectra_data`; the
  ABI SWATH centroid example (201 spectra) stays in `spectra_peaks` with `number_of_peaks`; the
  validator passes both; the viewer reads both facets (new golden fixtures + test in mzPeakViewer,
  the first to exercise the run-wide `sciex_sqrt` grid on the profile facet). The 13 published
  TOF-grid archives (1.6 M spectra, 50 % of the corpus) carried the wrong label and are rebuilt.
- ⚠️ **Native SCIEX `spectra_data` is point layout under `--tof-grid auto|on`.** The axis has no
  chunk encoder, so the facet that now holds gridded profile spectra cannot be chunked; the
  off-lattice minority is stored flat and exact instead of numpress-chunked, and `transformations`
  no longer lists `numpress-linear` for that lane. `--tof-grid off` keeps the requested chunking
  (nothing is gridded). **Size effect, measured on the 0.10.2 corpus rebuild** (this entry first
  claimed "well under 1 % of the points" — that counted chunk rows of the old facet, not points):
  the off-lattice share is 9.2 % of the points on MSV000093587 Sample002 (758 → 966 MB, +27 %),
  3.2 % on PXD011326 (1,090 → 1,218 MB, +12 %), 2.7 % on PXD053710 (+7 %), 1.6 % on MSV000090684
  (+3.5 %), 1.1 % on PXD065872 (+2.3 %) and 0.07 % on PXD071869 (+0.2 %); the f64 `mz` column
  costs 6–9.5 B per point where numpress cost ~2–3. The other seven TOF-grid archives are within
  0.3 % of their previous size. Fidelity up, size up; a chunk-capable integer axis would recover
  it and is a backlog item, owner's call.
- ⚠️ **Converter-owned per-spectrum columns are MZP terms (F5).** `tof_c0` / `tof_c1` /
  `tof_calibration_id` moved from `MS:4000900`–`MS:4000902` to `MZP:1000003`–`MZP:1000005`, and
  the timsTOF frame inputs `tdf_t1` / `tdf_t2` / `tdf_mz_calibration_id` from `MS:4000903`–
  `MS:4000905` to new `MZP:1000008`–`MZP:1000010` (`cv/mzpeak.obo`), so the `spectra_metadata`
  columns are `opt_MZP_1000003_tof_c0` … instead of squatting the PSI-owned `MS:` namespace — what
  the specification calls a column-naming artifact and asks to be converter-owned. The SCIEX,
  Agilent and Shimadzu lanes now add the `MZP` `cv_list` entry the timsTOF lanes already wrote.
  Readers were built for the move: the vendored reader binds the coefficients by name and the
  viewer by the `_tof_c0` / `_tof_c1` / `_tdf_t1` … column-name suffix, so archives of either
  generation reconstruct; `tests/contract_strings.rs` now refuses any `MS:40009xx` accession.

### Fixed

- **mzML export of a profile-facet grid archive carried a third binary array per spectrum.** The
  vendored reader rebuilds `m/z array` from the integer axis but leaves the axis in a Profile
  spectrum's raw arrays, so `mzpeak-convert ARCHIVE -o x.mzML` wrote the raw `tof_index` as a
  nameless `MS:1000786 non-standard data array` (32-bit integers) beside m/z and intensity — on
  every native Shimadzu `.lcd` archive since 0.9.3 (13,200 of 13,200 spectra of the published
  Blind run), and, after M6 moved profile grids into `spectra_data`, on every TOF-grid archive.
  A re-import then stored it as a nameless column. The export now drops the axis once m/z has
  been reconstructed (`strip_grid_axis`); it is kept only when no m/z array exists, so a failed
  reconstruction stays visible. Verified: the ZenoTOF grid archive and the Blind native archive
  export with exactly two arrays per spectrum and no `MS:1000786`.
- `docs/USER_MANUAL.md`: `--agilent-grid` said "a per-run `{c0,c1}`" (it has always written
  per-spectrum `tof_c0`/`tof_c1`/`tof_calibration_id`); §7, §8 and §9 state the facet rule, the
  accession move and the legacy column names; the verified ProteoWizard list includes 3.0.26175.

### Tests

- `tests::tof_grid_keeps_the_source_representation` (unit, both routes, centroid source);
  `contract_strings::tof_grid_files_by_representation_pinned` (the shared field + schema, the
  axis declared on the data facet of all three lanes, no lane assigns Centroid, only the Agilent
  reader states Profile); `tests/gridded_spectrum_summaries.rs` decides "gridded" by a non-null
  `tof_index` in either facet and tolerates an absent facet; `tests/tof_grid_facets.rs` synthesizes
  a profile + centroid mzML with mzdata, converts it with `--tof-grid on`, and asserts the facet of
  every spectrum, the representation and count columns, the two-array export and the round-trip
  values (self-contained, no corpus); mzPeakViewer `tof-grid.golden.test.ts` with
  `tof-grid-profile.mzpeak` / `tof-grid-centroid.mzpeak`.

## [0.10.0] — 2026-09-04

This cycle implements the verified findings of the 2026-09-04/05 two-model adversarial review
(the ledger's blocker B2, majors M2–M5, M7–M14, M19, M22–M27, M29–M34 (open: M1, M6, M15–M18, M20–M21, M28, M35–M36 — see the review ledger) and documentation items D1–D10) under the
invariant decided on 2026-09-04: **the data is preserved as much as possible, and every
transformation is declared in the archive.** ⚠️ marks an entry that changes what an archive
*contains* — a new or reworded index key, a removed CV term, a moved value — so every archive
written by ≤ 0.9.12 differs there and the published corpus needs a reconversion to carry it.
Everything under `#[cfg(windows)]` / `#[cfg(any(windows, target_os = "linux"))]` was edited on a
macOS host and is unbuilt until the next box build; expect new (non-fatal) dead-code warnings in
the vendor modules, see *Removed*.

### Fixed

- **`.mzML.gz` input never worked.** Three support tables advertised it from the first release,
  and mzdata's `open_path` refused every gzipped file outright ("Gzipped files are not supported
  with this method"). Found by the 2026-09-04 two-model review (Fable, lens Q4), verified live.
  A gzipped input is now detected by its `1f 8b` magic — not its name — and stream-decompressed to
  a temp copy before the Latin-1 transcode and param-group sanitize stages (both sniff XML bytes,
  which do not exist until the stream is inflated), so every downstream lane sees exactly what it
  would see for the plain file: the archive is facet-for-facet identical, and `source_files` and
  the SHA-1 still describe the `.gz` the user gave us. A plain file that merely *carries* a `.gz`
  name is handed to the reader under its inner name (mzdata decides "gzipped" from the extension),
  via a hardlink, nothing decompressed. Inspection without `-o` handles both too.
  `tests/gzip_mzml.rs` pins all four cases.
- ⚠️ **No lane adds the blanket `MS:1000294 mass spectrum` any more (M33).** mzdata's
  `spectrum_type()` is first-match and the mzPeak writer infers `MS:1000579` / `MS:1000580` from
  `ms_level` only when nothing shadows it — so the parent term the vendor lanes added put
  `MS:1000294` in `spectrum_type` on every row of every SCIEX / Waters / BAF / SDK / Agilent archive
  in the corpus (83,931 of 83,931 on the SWATH archive, 154,520 of 154,520 on the QTRAP one) while
  the manual documented it as the design. Removed at `src/sciex.rs:531`, `src/waters.rs:303`,
  `src/bruker_baf.rs:860`, `src/bruker_sdk.rs:333` (`make_description`, shared by the TDF and TSF
  SDK readers), `src/bruker_tsf.rs:229`, `src/agilent.rs:479`, `src/agilent_midac.rs:399`, and the
  four unconditional plus four add-if-absent sites in `src/main.rs` (see `src/main.rs:2921`),
  together with the `mass_spectrum: &Param` parameter of `tof_grid_spectrum`, `agilent_grid_spectrum`,
  `sciex_grid_spectrum` and `sciex_f64_spectrum`. mzdata's mzML writer emits 579/580 itself. Pinned
  by `tests::spectrum_type_is_the_ms_level_child_never_the_generic_parent` and an absence assertion
  on the gridded route in `tests::tof_grid_routes_per_spectrum`.
- ⚠️ **Run metadata no longer leaks the converting machine's filesystem or a dangling instrument
  reference (M2, M34).** `fixup_run_metadata` (`src/main.rs:6139`; helpers `is_filesystem_location`
  / `is_path_shaped_run_id` at `:6119` / `:6126`) reduces every `source_files[].location` that is a
  filesystem path in any spelling (`file:////Users/…`, bare path, drive letter) to the bare `file://`
  authority — non-`file` URL schemes (`https://`, `s3://`) are kept — resets a path-shaped `run.id`
  (mzdata's TDF reader copies the full `.d` path) to the input stem, and never mints
  `default_instrument_id = 0` against an empty `instrument_configuration_list`: an absent or
  non-resolving id is clamped onto an existing configuration or left `null`. `VendorHints.instrument`
  is applied BEFORE the fixup in `convert_vendor_reader` (`src/main.rs:5716`) so the Shimadzu/SDK
  instrument is what the reference resolves against. Archives of runs without an instrument record
  (Waters, SCIEX, mzML without one) now carry `default_instrument_id: null` instead of a dangling
  `0`. Not fixed (needs the vendored writer / mzdata): the per-scan `instrument_configuration_id` —
  mzdata's `ScanEvent` has no None state and defaults to 0 (carried to `BACKLOG.md`).
- ⚠️ **`ims_calibration.chunk_tof_encoding` states the real decoding rule (M5):**
  `"chunk_start + cumsum(deltas); first delta is relative to chunk_start"` (`src/main.rs:4564`).
  The old `"delta-within-chunk; first absolute; cumsum"` described a layout the writer never
  produced — anyone implementing the stated rule decoded every chunk wrongly from its first point.
  Pinned in `tests/contract_strings.rs:51`; the manual's §9 sentence says the same.
- ⚠️ **The Shimadzu profile `tof_calibration` block says `"mz_reconstruction":
  "within-vendor-rounding"` with `"max_error_da": 5e-10` instead of `"exact"`** (ledger 0.2;
  `src/main.rs:4944`). Measured: 4,890 of 5,000 gridded points rebuild off the vendor's 1e-9 lattice
  by ≤ 0.5 step (4.15e-10 Da), inside the vendor's own ±5e-10 rounding — accurate to vendor
  precision, not bit-exact. `tests/contract_strings.rs` now asserts exactly ONE `exact` site
  (Agilent file-direct) and pins the new value (`:120`).
- ⚠️ **Shimadzu isolation window is keyed on the vendor's isolation target (`AcqModeMz`)**, no
  longer on `max(target, selected ion)`, which shifted the window whenever the selected ion was
  heavier (M7): `src/shimadzu.rs:779–810`. The selected ion is only the fallback target when the
  vendor reports none, and a window with a target but no selected ion is still written (precursor
  row without selected-ion rows) rather than dropped — the vendored writer emits the precursor row
  independently of selected-ion rows (`vendor/mzpeak_prototyping/src/writer/visitor.rs:2397–2410`,
  checked before allowing empty `ions`). Unbuilt here (`cfg(windows)`); no corpus `.lcd` with
  differing `AcqModeMz` / selected ion was checked.
- **The four mzML export lanes and the mzPeak→mzPeak filter are atomic (M14).** They wrote the
  FINAL path directly, so a failure left a partial file and under `--force` had already destroyed
  the previous output. All five now write to a temp under `TmpGuard` and rename on success:
  `mzml_tmp_path` (`src/main.rs:724`; `x.mzML` → `x.mzML.tmp`, `x.mzML.gz` → `x.mzML.tmp.gz` —
  `.gz` kept LAST so the untouched `mzml_sink` still picks the gzip encoder), and `src/filter.rs:219`
  (`<out>.mzpeak.tmp` via `crate::TmpGuard`). The gzip trailer is written by dropping the writer
  before the rename. New `tests/mzml_export_atomic.rs` (4 tests).
- **Environment levers read one way (M29–M31).** One `env_flag()` helper (`src/main.rs:113`:
  unset → `None`; empty / `0` / `false` / `no` → `Some(false)`; else `Some(true)`) now serves
  `MZPC_NO_MZ_LATTICE` (`:190`), `MZPC_DUMP_IM_TABLE` / `MZPC_DUMP_AGILENT_PROFILE` (`:904`, `:912`),
  `MZPC_BYTE_PLANE_INTENSITY` (`:4326`) and `MZPC_TIMING` (`:4447`). Before, `MZPC_BYTE_PLANE_INTENSITY=`
  (empty) silently switched the ims-compact intensity column to Float32, and the two dump levers
  fired on mere presence. ⚠️ `ims_calibration.intensity_dtype` (`int32` | `float32`,
  `src/main.rs:4569`) now records which intensity column an ims-compact archive holds, so the
  `MZPC_BYTE_PLANE_INTENSITY=0` opt-out is declared in the archive. `tests::env_flag_is_three_way`.
- **The diagnostic levers refuse to shadow a requested archive (M29, M30).** `MZPC_DUMP_IM_TABLE`,
  `MZPC_DUMP_AGILENT_PROFILE` and `MZPC_SHIMADZU_PROBE` with `--output` now exit non-zero naming the
  variable and saying no archive would be written (`refuse_diagnostic_with_output`,
  `src/main.rs:889`); they used to print and exit 0 with nothing written. `MZPC_SHIMADZU_PROBE` is
  handled in `run()` (`src/main.rs:917`; `shimadzu_probe_lever` `:1412`) instead of inside the
  Shimadzu lane — which only runs with `-o`, so the probe always swallowed the archive — a
  non-numeric or empty value is an error rather than 10, and off Windows the set lever is an error
  instead of silently ignored. `tests::dump_lever_with_output_bails_and_writes_nothing`.
- ⚠️ **A `MZPC_MAX_SPECTRA`-truncated archive is detectable offline.** Every mzPeak lane that
  honours the cap writes a `metadata.partial` index block `{partial: true, max_spectra,
  source_declared, spectra_written, cause: "MZPC_MAX_SPECTRA"}` when the cap bit (`partial_marker`,
  `src/main.rs:126`; eight sites: `convert_file`, the TOF-grid finisher, the Agilent grid, both
  ims-compact lanes, the vendor readers, native SCIEX). "No declared count and written == cap" is
  taken as truncated (an honest "maybe partial" beats a silent "complete"); a cap larger than the
  file writes no marker. The WARN stays. `tests::max_spectra_cap_writes_partial_marker`.
- **`--config` accepts the six options it promised and rejected (M12):** `representation`, `rt`,
  `ms_level`, `drop_aux`, `verbose`, `quiet` (`FileConfig`, `src/main.rs:497`; merged
  CLI-over-config-over-default in `Settings::resolve`). `--representation` is now an `Option` on the
  CLI (default `both`), and `-v` / `-q` from a config file take effect because settings resolve
  before logging is initialised (`main`, `src/main.rs:770`).
  `tests::file_config_accepts_the_six_promised_keys`.
- **`-v` / `-q` win over `RUST_LOG` (M13):** `init_logging` (`src/main.rs:867`) builds the filter
  from an explicit flag and consults `RUST_LOG` only when neither is given; the `-v` / `-q` help
  text says exactly that (it used to say the opposite of what the code did).
  `tests::explicit_verbosity_flags_win_over_rust_log`.
- **Flags dropped with exit 0 are now either honoured or refused (M10, M11).** (a) `--image` /
  `--sdrf` are threaded through the mzML TOF-grid sub-path (`convert_file_tof_grid` →
  `finish_tof_grid_archive`, `src/main.rs:2434`, `:2547`, which embeds vendor members + images +
  SDRF exactly as `finish_with_vendor_and_aux`) — the same command used to keep or lose the SDRF
  depending on whether the grid fit passed — and through `--via-msconvert` (`convert_via_msconvert`,
  `:1810`, no longer hard-codes `&[], None`). (b) A per-lane honoured-flags table (`Lane`,
  `unsupported_flags_for`, `refuse_unsupported_flags`, `src/main.rs:1214`–`:1387`) is checked in
  `run()` right after lane selection against the options the user actually passed on the command
  line (`Settings::given` — never defaults, and never a config profile's values: a profile is a
  standing default that takes effect where a lane can use it and cannot make a lane refuse; this
  holds for its `aux:` / `image:` / `sdrf:` lists too), bailing in the existing
  `--rt` / `--ms-level` style with the flag, the lane and the remedy: `.mzpeak → mzML` and `--to mzml`
  (encoder / embedding options, `--bruker-sdk`), `--agilent-grid` (images / SDRF,
  `--via-msconvert`), `--via-msconvert` (`--aux`), both `--bruker-sdk` lanes (images / SDRF,
  `--ims-chunked`, `--no-tims-recalibration`), native ims-compact and the native vendor readers
  (images / SDRF). The `.mzpeak → .mzpeak` filter lane only WARNS about its listed convert-only
  options (the dead `--zstd-level` included): it re-packs members verbatim, so an inert flag there
  loses nothing. Options a lane merely has no use for but that cannot change its output are
  deliberately not refused, so shared recipes keep working — the corpus descriptors and
  `tools/convert_vendor_ci.sh` were checked and none is refused. The `--representation` inert-flag
  warning now knows BAF honours it for mzPeak output (`representation_is_honoured`,
  `src/main.rs:852`). `tests::unsupported_flags_are_checked_against_given_only`,
  `tests::unsupported_flag_combination_is_refused_by_the_binary`, `tests::tof_grid_subpath_embeds_sdrf`.
- **`--tof-grid` reaches native SCIEX `.wiff` (M3 — the first application of the invariant).** The
  resolved mode is an `Option<TofGridMode>` (`Settings::tof_grid`) threaded through `convert_file`
  to `convert_sciex` (`src/main.rs:5238`): not given → `auto` (unchanged output), `off` → exact f64
  m/z for every spectrum via `sciex_f64_spectrum` with no `tof_calibration` block (nothing
  transformed — the opt-out the invariant requires; the published MSV000095995 archive records a
  4.986 ppm grid with no way to have switched it off), `on` → the run-wide clock fit is required.
  The mzML lane maps not-given → `off` as before. Help text updated. Unbuilt here (`cfg(windows)`).
- **`DOTNET_ROLL_FORWARD=LatestMajor` is set for Thermo `.raw` input only (M8;
  `src/main.rs:785`).** Set for every input it overrode the Shimadzu glue's own `rollForward:
  LatestMinor`, which on a .NET 9 host hoists the glue onto a runtime without the `BinaryFormatter`
  path it needs. The Shimadzu-on-.NET-9 scenario itself was not reproducible (the box has .NET 8).
- **The Shimadzu lattice-guard warning quotes the guard's tolerance from `mz_lattice::LATTICE_TOL`
  (1e-6)** instead of a hardcoded `1e-3` (`src/main.rs:5710`).
- **`mzml_output_preserves_srm_chromatograms` (`src/main.rs:7303`) is pinned to
  `general-ms/sciex-qtrap-6500/En_PPY.mzpeak`**, checks both the mzPeak→mzML and the mzML→mzML lane,
  and FAILS when the file is missing or carries no chromatograms (M27); it used to pick any
  `*MRM*.mzML` and return quietly. Still `#[ignore]` (corpus); run explicitly it passes in ~30 s.
  It drives the binary with `MZPC_MAX_SPECTRA=2000` because a full in-process pass over the
  154,520-spectrum archive through `filter_mzpeak_to_mzml` ran > 3 minutes without writing a byte
  (`MzPeakReader` per-index access: parquet page decode + `skip_records` per spectrum) — a
  performance finding for `BACKLOG.md`, not touched here. Chromatograms are carried whole regardless
  of the cap.

### Changed

- `run()` takes the resolved settings (`fn run(cli: &Cli, cfg: &Settings)`, `src/main.rs:901`);
  `Settings::resolve` runs in `main()` before `init_logging`; `convert_file`'s `tof_grid` parameter
  is `Option<TofGridMode>`.
- `tests/gridded_spectrum_summaries.rs:82` — "gridded" is keyed on FACET MEMBERSHIP (the
  `spectrum_index` appears in `spectra_peaks.parquet`, not `spectra_data.parquet`) instead of
  `number_of_peaks` being non-null, so the assertion survives the M6 fix (carrying
  `spectrum_representation` through unchanged instead of forcing centroid to steer the facet). The
  helper locates the row struct by its `spectrum_index` child so it reads both the grid lane's
  `point` and the f64 lane's `chunk` layout, and asserts no spectrum sits in both facets.
- `tools/box_convert_remote.ps1:103`: one-line note that ProteoWizard 3.0.26175 (same
  Shimadzu.LabSolutions.IO 5.0.0.0) is installed beside the pinned 3.0.26151; the pin is unchanged.

### Added

- ⚠️ **Every mzPeak lane writes a `metadata.transformations` index block** (the second half of
  the invariant; M9): `transformations_block` / `base_transformations` at `src/main.rs:4683` /
  `:4689`. Entries: `zero-run-mask` (every lane), `numpress-linear` (when that chunk codec is chosen
  on any facet), `sort-by-mz` (the generic lane actually re-ordered at least one spectrum —
  `src/main.rs:3664` tracks it instead of assuming), `tof-grid:<ppm>ppm` (a fitted grid replaced
  f64 m/z; mzML tof-grid and SCIEX per-spectrum lanes), `shimadzu:span-trim` (`src/main.rs:4924`,
  profile sqrt-grid route), `agilent:drop-zero-samples` (`src/main.rs:3136`, Agilent profile grid
  lane). `VendorHints` gained `transformations: Vec<String>`. Pinned in
  `tests/contract_strings.rs:164`; asserted on the tiny-fixture archive
  (`tests::transformations_block_declares_what_the_lane_applied`). Not declared, on purpose: the
  `--ims-chunked` layout's per-frame TOF sort (re-orders points across scans) — `BACKLOG.md`.
- ⚠️ **`ims_calibration.chord_source`** (M4; `src/main.rs:4529`): `"global_metadata"` on the
  native timsrust lane, `"sdk_tims_index_to_mz"` on `--bruker-sdk`, so a reader knows which of the
  two (a, b) chords — measured 4.28 ppm apart on 2485.d — an archive holds. Threaded as a parameter
  through `write_ims_compact_archive{,_parallel,_impl}`; pinned in `tests/contract_strings.rs:54`.
- **One loud, once-per-process `log::warn!`** (`std::sync::Once`) when a lane that carries no
  precursor writes an `ms_level > 1` row — "this reader does not yet extract precursors; MS2 rows
  will have none" (M32 interim measure): `src/sciex.rs:502`, `src/waters.rs:282`,
  `src/bruker_baf.rs:825`, `src/bruker_tsf.rs:202`, `src/agilent_profile.rs:265`. The message
  names the vendor API that carries the data.
- **`tests/shimadzu_abi_pin.rs`** (new, fixture-free, host-independent; M26): `include_str!`s
  `src/shimadzu.rs` and `glue/shimadzu/Glue.cs` and pins the Shimadzu C ABI across the language
  boundary — `REQUIRED_ABI_VERSION` (`src/shimadzu.rs:119`) equals `ShimadzuAbiVersion() => N`
  (`glue/shimadzu/Glue.cs:865`); the ordered fields and counts of `ShimadzuSpectrumMeta` /
  `ShimadzuSpectrumMetaV2` match their C# twins and V2 starts with V1 on both sides; any
  `[StructLayout]` on a twin is plain `Sequential`; every `pdcstr!("…")` export the loader resolves
  has an `[UnmanagedCallersOnly(EntryPoint = "…")]` with the same parameter count. Failure messages
  name the drifted field/export; mutation-tested (a bumped version literal, a renamed C# field and a
  renamed Rust export each fail exactly one assertion by name). It reads the sources as text, so it
  runs on macOS although the module is `cfg(windows)`; it cannot tell whether the glue DLL was
  rebuilt — that remains the release checklist's job.
- **`tests/tdf_exact_tof_calibration.rs`** (corpus-gated) now also asserts, after building
  2485.mzpeak, `metadata.ims_calibration.lossless == "tof"`, that every MS1 row of
  `spectra_metadata` carries `total_ion_current > 0` and `base_peak_intensity > 0`, and that the
  synthesized BPC (MS:1000628) and TIC (MS:1000235) traces are bit-equal, point for point, to those
  columns (400 MS1 frames) — the archive-level pin of the 0.9.11 chromatogram fix.
- **`tests/mzml_export_atomic.rs`** (new): a failed `.mzML` / `.mzML.gz` export, a failed
  `.mzpeak` filter and a failed `.mzpeak → mzML` export leave no `.tmp` behind; a successful export
  is renamed into place. Further `src/main.rs` unit tests:
  `fixup_run_metadata_strips_paths_and_never_mints_a_dangling_instrument`,
  `index_carries_no_operator_paths` (no scratch or home directory anywhere in
  `mzpeak_index.json`; every `source_files[].location == file://`), and the A1 set listed above.
- **Harness (`tools/`, ledger M22–M23):** `tools/corpus_reconvert.py:43–47`, `:143–159`, `:361` —
  a `.raw.zip` / `.d.zip` whose directory is NOT extracted beside it is a raw unit; it maps to
  `<stem>.mzpeak`, ranks below every extracted format, and is `skipped` on the host (the converter
  reads directories, not zips) so it is deferred to the box, whose remote script already extracts
  archives. The corpus now reports 201 units / 201 archives instead of 199 with two Waters archives
  (PXD052561, PXD077098) invisible. `tools/corpus_reconvert.py:616` — the report asserts
  archives-on-disk == targets; any `.mzpeak` no recognised unit produces is printed under
  `UNACCOUNTED ON DISK` and makes the run exit 1. `tools/box_convert.sh:46–63` —
  `resolve_relay_python()` picks one boto3-capable interpreter for `s3_relay.py` at startup
  (`$MZPC_PYTHON` if set, verified, no silent fallback; else the first of `python3`,
  `python3.14/13/12`, `~/anaconda3/bin/python3`, `~/miniconda3/bin/python3`, the `mzpeak314` env,
  the repo `.venv` that can `import boto3`; otherwise exit 2 naming the fix). The bare `python3`
  previously used had no boto3 on this host, so the documented single-job invocation staged
  nothing.
- **CI (`.github/workflows/windows.yml:55–100`, ledger M25):** the Windows job builds
  `glue/shimadzu/ShimadzuGlue.csproj`, asserts `glue/shimadzu/bin/Release/net8.0/ShimadzuGlue.dll`
  and its generated `ShimadzuGlue.runtimeconfig.json` exist, and asserts that config carries
  `System.Runtime.Serialization.EnableUnsafeBinaryFormatterSerialization: true` — the switch
  without which Shimadzu.LabSolutions.IO 5.0.0.0 makes every `.lcd` unreadable while the DLL still
  builds. Unrun CI (no `pwsh` on the host); the paths and the expected value were confirmed against
  a real `dotnet build` output in a scratch copy.

### Removed

- The off-Windows `convert_shimadzu` stub (its own comment said it had no caller) and the
  corpus-only test `contract_tof_grid_calibration_keys` (could not pass on its documented input;
  superseded by `tests/gridded_spectrum_summaries.rs`).
- The unconditional `#[allow(dead_code)]` on `mod bruker_baf / bruker_sdk / agilent / agilent_midac /
  sciex / shimadzu` (`src/main.rs:26–57`) is now `#[cfg_attr(not(<the module's own cfg>),
  allow(dead_code))]` — where a module is compiled it must earn its keep. The box build may show
  new dead-code WARNINGS in those modules (not errors; no `-D warnings` anywhere).
- `src/waters.rs`: the never-read `WatersReader.is_continuum` field (the per-function
  `continuum: Vec<Option<bool>>` is what is read) — the last Windows dead-code warning.
- `src/bruker_native.rs`: the placeholder test `c2_zero_sdk_goldens_match_the_sqrt_linear_pair`,
  which read a fixture that never existed and printed "skipping" forever; the same assertion runs
  against `tests/fixtures/tdf_2485_sdk_golden.json` in `sqrt_linear_pair_matches_the_vendor_sdk_goldens`.
- `src/shimadzu_grid.rs`: the unused `pub use crate::mz_lattice::{lattice_tolerance, LATTICE_TOL}`
  re-export (`:146` now re-exports only `LatticeOutcome` and `LATTICE_SCALE`) and the three
  test-only wrappers `centroid_lattice` / `lattice_tof_index_field` / `lattice_peak_arrays`;
  `LATTICE_TRANSFORM_PARAMS` is `#[cfg(test)]` (`:154`). The 0.9.12 comment claiming
  `convert_shimadzu` "calls `crate::mz_lattice::*` directly" was false — it calls
  `lattice_peak_schema` / `mz_calibration_block` / `lattice_route`; the comment and
  `mod lattice_tests` (`:249`) now say and pin exactly that, asserting through those three
  production wrappers. The now-unused `mzdata::curie`, `Param` and `ParamDescribed` imports were
  dropped from the seven vendor files of the MS:1000294 change.
- **The superseded corpus/box harness set (ledger M24** — they inferred success from a file's
  existence, the defect class 0.9.12 fixed in the live harness): `tools/convert_corpus.sh`,
  `corpus_full.sh`, `corpus_bench.sh`, `rerun-tests.sh`, `corpus_box_convert.sh`,
  `corpus_manifest.tsv`, `box_pxd_convert.sh`, `box_url_convert.ps1`, `pxd014690_convert.ps1`,
  `tof_grid_verify_lossless.py`, `tof_grid_make_mixed_mzml.py` and `tools/flash/`. Nothing live
  referenced them; git keeps the history. `.github/workflows/windows.yml:249` names the live entry
  point (`tools/corpus_reconvert.py`); `tools/corpus_ratio_table.py:2` and
  `tools/render_ratio_table.py:2` note that the TSV producers they render are gone.
- `tools/corpus_reconvert.py`: `sync_box()` and its `box_ssh()` helper (ledger M19). `box_convert.sh`
  already runs `box_update_remote.ps1`; two updaters on every `--box` run were the stray process in
  the 32-minute 2026-09-03 hang. `run_box` now passes `BOX_CONVERTER_VERSION=v<x.y.z>` to
  `box_convert.sh` explicitly (`tools/corpus_reconvert.py:404–429`), which with the existing
  `BOX_REQUIRE_VERSION=1` gives one updater, one lock, one stamp of the version that ran. The
  matching PATH-prepend workaround for the relay interpreter is gone too.
- `.github/workflows/windows.yml:55–79`: the Windows job no longer builds or asserts `glue/waters`
  (ledger M25) — no code path in `src/` loads `WatersGlue.dll`; `src/waters.rs` drives
  `MassLynxRaw.dll` directly.

### Documentation

- **Fidelity language rewritten to the decided invariant (D10).** `README.md` and
  `docs/USER_MANUAL.md` §1 no longer call the archive "lossless" or zero-run compaction "the one
  transform that is not byte-for-byte"; §8 gains a *Fidelity* block naming the four non-bit-exact
  encodings with their bounds, the six-entry `transformations` vocabulary (which lane writes which
  entry, and the one re-order not yet declared), the `mz_reconstruction` values per lane
  (`within-vendor-rounding` + `max_error_da` for Shimadzu, `exact` for Agilent file-direct,
  `bounded-lossy` + `roundtrip_tolerance_ppm` for SCIEX), and the two other new audit keys
  (`metadata.partial`, `ims_calibration.chord_source`). §7 lists `transformations` / `partial` in
  the index and states the new provenance guarantees (`source_files[].location` is never an
  operator path, `run.id` never a path, `default_instrument_id` may be `null`).
- **`docs/USER_MANUAL.md` §4 regenerated from `mzpeak-convert --help` (D1):** all 28 options with
  the help's wording and defaults (`--to`, `--rt`, `--ms-level`, `--drop-aux`, `--image`, `--sdrf`,
  `--no-chromatograms`, `--agilent-grid`, `--tof-grid` were missing; `--zstd-level` now says 3, and
  5 on the ims-compact lanes — D7), a "refused, not dropped" paragraph with the per-lane table
  generated from `unsupported_flags_for` (including the rules that config-file values never count
  as supplied and that the filter lane warns instead of refusing), and
  three subsections for the modes that existed only in `--help`: §4.1 mzML output (`--to mzml`,
  `-o x.mzML`, `-o x.mzML.gz`, atomic temp names), §4.2 filtering an existing archive, §4.3
  embedding (`--sdrf` / `--image`) with the lane-by-lane embedded / injected / refused table and the
  imzML-only nature of an explicit `--image`.
- **§5 regenerated from `FileConfig` (M12):** all 27 keys, the six new ones marked; the example was
  run against the built binary (the full file parses — the `rt`/`ms_level` keys trigger the
  documented raw-input refusal; the convert-only subset converts). States that only `<INPUT>` and
  `--config` are command-line only, and that a config value is a standing default that never counts
  as "supplied" for the refusal table.
- **§10 reconciled with the code (D8):** all 26 `MZPC_*` names (19 in `src/`, 4 in the vendored
  writer, 2 in the Shimadzu glue, 1 comment-only `MZPC_WATERS_GLUE`), the `env_flag()` semantics
  (unset / `""` / `0` / `false` / `no`), the narrower spellings the vendored writer and the C# glue
  keep (`MZPC_SHIMADZU_COARSE_MZ` is compared to the literal `1`), that the dump/probe levers refuse
  to run with `-o`, a "recorded in the archive?" column for the output-affecting levers
  (`intensity_dtype`, `partial`, `transformations`), the `MZPC_SHIMADZU_PROBE` relocation and
  numeric-only rule, and `DOTNET_ROLL_FORWARD` as Thermo-only. §8 documents the spectrum type as
  `MS:1000579` / `MS:1000580` inferred from `ms_level` (it documented `MS:1000294`); §9's chunked-TOF
  sentence says `chunk_start + cumsum(deltas)` with the first point excluded (M5).
- **The Agilent native lane is documented as not wired (B1):** `docs/USER_MANUAL.md` §6 / §11,
  `docs/PLATFORM_SUPPORT.md` and `BACKLOG.md` #9 (✅ struck, regressed by merge 5a62b90; options A /
  B / C costed under #23, decision pending) say the same thing. `BACKLOG.md` also corrects the
  `tof_c0` / `tof_c1` accession (`MS:4000900` / `MS:4000901`, not `MZP:`), marks #17–#21 as shipped,
  notes the byte-plane lever as shipped, and points at the review ledger as the current issue list.
  `README.md` badges: Rust 1.88+ (was 1.87, could obstruct a build) and the live GitHub release
  badge (was hard-coded v0.3.1) (D5). `src/bruker_baf.rs:20–22` names the real gate
  (`#[cfg(any(windows, target_os = "linux"))]`), not a non-existent `bruker_sdk` cargo feature.

## [0.9.12] — 2026-09-04

No converter behaviour change: no change to any spectrum, chromatogram or
index value against 0.9.11; the recorded converter version differs (the binary
writes `CARGO_PKG_VERSION` into every archive's processing metadata, so the
bytes are not identical — an earlier wording of this entry claimed they were).
This release carries the conversion HARNESS and two build-warning corrections.

### Fixed

- **The box pipeline could report a corpus fully converted while converting
  nothing.** Four independent defects in `tools/`. The ssh watchdog inherited
  its caller's stdout, so inside a command substitution the orphaned `sleep`
  held the pipe open and every invocation blocked for the full 7200 s timeout
  even though ssh had returned in seconds. A busy build lock aborted the entire
  run with exit 3 instead of retrying. Archives were stamped on EXISTENCE rather
  than on having changed — and because S3-first leaves the local copy stale by
  design, that labelled August archives as freshly built after the box had
  aborted without converting anything, then reported `199/199 (100.0%)`. Stale
  update locks expired after two hours rather than thirty minutes, so a host
  killed mid-update blocked every later run.
- **Two false `never used` warnings on Windows, silenced with the reason rather
  than a deletion.** `shimadzu_grid`'s four lattice wrappers bind the shared
  lattice to Shimadzu's 1e-9 `MassHigh` scale; `mod lattice_tests` exists to pin
  exactly that, and `cargo build` does not compile tests — so deleting them
  deletes the thing under test. `convert_sciex_grid`'s `vendor` parameter is
  genuinely unused and correctly so: `vendor::embed_into_archive` walks a
  DIRECTORY and a SCIEX input is a `.wiff` FILE.

## [0.9.11] — 2026-09-03

### Fixed

- **The synthesized BASE-PEAK chromatogram was dead on every grid lane.** `Ms1Chroms::observe`
  called `peaks.base_peak()`, which resolves through mzdata's m/z-keyed summary and folds to
  `(0, 0)` when the spectrum has no m/z array — precisely the case on the `tof_index` / `tof`
  lanes. Measured on published corpus archives: timsTOF `…_2485.mzpeak` BPC max **0** across all
  400 points; SciEX `Sample002.mzpeak` zero on **2,371 of 2,372**. The TIC was unaffected because
  summing intensities never needed m/z. This is the same defect class as the metadata fix in
  `bc8497c`, one facet over. `observe` now goes through a single `chromatogram_summary`, which
  calls exactly what the writer's `raw_summaries` calls (so on every ordinary lane the chromatogram
  point is BIT-EQUAL to the `total_ion_current` / `base_peak_intensity` column of the same
  spectrum — verified on `waters-xevo-g2s-qtof/QC01`, 2,281 of 2,281 MS1 rows, max diff 0.0) and
  folds the intensities directly only where mzdata cannot answer.
  **Published grid-lane archives carry a zero BPC and are worth reconverting** — measured across the
  201-archive corpus, 12 carry a genuinely dead BPC beside a healthy TIC (the grid / `ims-compact`
  set; a further 5 all-zero hits are empty single-spectrum pwiz test files, and 59 archives have no
  BPC chromatogram at all, which is expected for MS2-only and imzML inputs).
  ⚠️ **Value-moving on dual-facet archives too.** Where a spectrum carries BOTH raw arrays and a
  centroid peak list — a Shimadzu `.lcd` under `--representation both` — the synthesized TIC/BPC now
  state the PROFILE trace, because that is what the writer's own `raw_summaries` picks and the
  chromatogram must agree with the metadata column beside it. Previously the two disagreed:
  `Blind_P1_pos_012` spectrum 0 shipped column TIC 13,220 / base 834 next to chromatogram TIC
  12,877 / BPC 2,844. Those chromatogram values change on reconversion; they were never consistent
  with their own archive.
- **Agilent profile spectra were all hardcoded to negative polarity.** `src/main.rs` set
  `descr.polarity = ScanPolarity::Negative` unconditionally, with a comment admitting it was
  because one dataset happened to be negative-mode. Polarity now comes from the scan record's own
  `IonPolarity` field (resolved in the `ScanRecordType` offset walk). Only the two unambiguous
  codes become a polarity — `Unassigned` (2), `Mixed` (3), an unknown code and a schema without the
  optional element all stay `Unknown`, which the writer stores as NULL. Both profile-bearing corpus
  files are positive-mode, so the old hardcode was wrong for 100% of the profile data we hold.
- **`glue/shimadzu/ShimadzuGlue.runtimeconfig.json` could not load the vendor library.** The
  committed hand-maintained config lacked the
  `System.Runtime.Serialization.EnableUnsafeBinaryFormatterSerialization` property that the csproj
  sets, and `Shimadzu.LabSolutions.IO` 5.0.0.0 deserializes part of the `.lcd` through
  `BinaryFormatter` — so on the documented DLL-only deployment `LoadData` threw
  `NotSupportedException` and **every** `.lcd` looked unreadable. The property is now in the
  committed file (verified a strict superset of a fresh `dotnet build` output), with a
  keep-in-sync note on both sides.
- **`glue/shimadzu/Glue.cs` multiplied by a reciprocal where the vendor divides by a scale.**
  `HighMassMul = MassMultiplier / R` then `mz = MassHigh * HighMassMul` is two roundings deep and
  disagrees with the exact form on ~40 % of lattice values. Every integer→m/z site now divides by
  the integer scale it is given: `MassUnit` replaces `MassMultiplier`, `HighMassDivisor =
  MassUnit * R` (both exact integers, product ≤ 1e13, so exact in `double`) replaces `HighMassMul`,
  and `PrecursorMzUnit` / `MillisecondsPerSecond` replace their `1/x` constants. The Int64 m/z
  lattice re-rounds and was unaffected; `--no-mz-lattice`, precursor m/z and mass ranges carried the
  inexact value.
- **`tof_calibration` blocks did not say whether reconstructed m/z is exact, and one said nothing
  at all.** A reader could not tell a lane that rebuilds m/z exactly from one that accepts anything
  within `MZPC_TOF_GRID_PPM`: on `MSV000095995` the source and reconstructed base-peak m/z differ
  by 4.6 ppm. All **four** `codec: "tof-grid"` models now emit the same key set — `lossless` names
  the exactly-stored integer column (the spec's key, unchanged), and a new `mz_reconstruction` says
  `"exact"` (Agilent file-direct, Shimadzu profile) or `"bounded-lossy"` + `roundtrip_tolerance_ppm`
  (both SCIEX lanes). The fourth block, the Shimadzu profile lane, previously emitted NEITHER key
  while sharing its `model` string with the per-spectrum SCIEX lane, so a reader keying off the
  model got one answer there and null here; two published archives (`HEK_PosOAD1`,
  `Blind_P1_pos_012`) carry that gap. `tests/contract_strings.rs::tof_grid_reconstruction_keys_pinned`
  now counts emission SITES rather than asserting a fixed number, which is what let the fourth hide.
  **Additive: no key changed name or meaning, so existing readers keep working.** An interim commit
  in this cycle did rename `lossless` to `integer_column`, on the reading that it asserted a
  fidelity the lane lacked; the spec defines it as "name of the exactly-preserved stored column",
  which `tof_index` genuinely is, so the rename was reverted before release. The converter's own log
  line no longer says "lossless fit accepted" either — that one WAS a false fidelity claim.
- **Gridded spectra summarized coordinates no point in the archive occupies.** `base_peak_mz`,
  `lowest_observed_mz` and `highest_observed_mz` named the SOURCE f64 m/z while the archive stores
  an integer axis a reader can only evaluate as `(c0 + c1·k)²`. Every grid route now summarizes the
  RECONSTRUCTED coordinate, so the observed-m/z bounds contain the file's own data.
  ⚠️ **Values move**: on `swath.api-sample-centroid` all 201 spectra shift (`base_peak_mz` ≤ 2.17
  ppm, `lowest_observed_mz` ≤ 3.10 ppm, `highest_observed_mz` ≤ 1.01 ppm). Intensity-derived
  columns are untouched — intensity is stored verbatim.
  `tests/gridded_spectrum_summaries.rs` asserted the opposite contract and was relaxed to the
  grid's own round-trip bound for the three m/z columns only.
- **Unequal m/z / intensity arrays were silently truncated.** `tof_grid_spectrum` walked the pair
  with `zip`, which stops at the shorter array and returns success, so a broken decode produced a
  structurally valid archive with signal missing and no diagnostic; `sciex_grid_spectrum` guarded
  it with a `debug_assert_eq!`, a no-op in the shipped release build. Both now go through
  `require_aligned_arrays`, which refuses the conversion naming the spectrum and both lengths.
- **An Agilent scan naming an undefined `CalibrationID` used an arbitrary polynomial.**
  `load_calibration` fell back to `default_rows.values().next()` — a HashMap row chosen by a
  per-process random seed, so the same input reconstructed different m/z on different runs. Now:
  exactly one defined calibration is still used for every scan (the vendor's single-calibration
  files are unaffected); more than one with the requested ID missing is an error listing the
  missing and the defined IDs. ⚠️ **A `.d` that converted at 0.9.10 with wrong m/z now fails to
  convert.** That is deliberate — it was never producing usable data — but it is a behaviour change.
- **`MZPC_MAX_SPECTRA` truncated an archive in total silence**, and suppressed the source
  completeness check while doing it. It now emits one `WARN` per run saying the archive is PARTIAL
  and that the check is disabled.
- **The Agilent profile reader dropped scan records without a trace** — empty segments, all-zero
  intensities, and a segment past EOF, which additionally **abandoned the entire rest of the run**.
  This lane never reaches `assert_source_complete` (a `.d` declares no readable spectrum count), so
  a truncated acquisition produced a short archive and exit 0. A per-cause skip tally now backs a
  "wrote N profile spectra of M scan records" line and a warning naming each cause.
- **Nothing re-checked the `.lcd` after the vendor DLL had it open.** The source SHA-1 must be taken
  BEFORE the open (the open takes a byte-range lock), and nothing looked again afterwards. A source
  fingerprint (length + mtime, in the ungated `src/embed_aux.rs`) is now compared after the reader
  is dropped, so an OLE2 commit by the vendor library is reported instead of leaving a digest that
  silently no longer describes the file. Size+mtime rather than a re-digest: an OLE2 commit moves
  both, and re-hashing would cost a second full read of a multi-GB run on every conversion.
- **The rotated-centroid warning fired on good libraries and could stay silent on bad ones.** It now
  requires the loaded `Shimadzu.LabSolutions.IO` to be a version carrying the defect (major < 5)
  AND the file to store no profile signal, checked in that order — so a current ProteoWizard never
  probes the file at all. A version we cannot read is treated as its own state: it still warns, but
  with "could not check", never with an accusation. The glue reports the version over a new
  `LibraryVersion` export, taking the HIGHER of AssemblyVersion and FileVersion so a vendor DLL that
  pins a low AssemblyVersion cannot be called stale.
- 🚨 **`ShimadzuGlue` ABI 3 → 4** (`LibraryVersion` added). **Any prebuilt `ShimadzuGlue.dll` —
  the Flash box's included — must be rebuilt from this commit** (`dotnet build -c Release` in
  `glue/shimadzu`) before the next `.lcd` conversion: a stale DLL reports ABI 3 against a binary
  needing 4 and the first open aborts on the handshake. That refusal is by design, but the rebuild
  is a required step of this upgrade, not an optional one.

### Removed

- **The Shimadzu Stage-B experiment levers.** `MZPC_SHIMADZU_FETCH`
  (`legacy|centroid-first|centroid-only|split`), `MZPC_SHIMADZU_PROFILE_DESIRED` and
  `MZPC_SHIMADZU_DUMP` existed to prove the centroid rotation belonged to the library and not to the
  file. That question is answered (3.8.4.6016 rotates; 5.0.0.0 reads the same file correctly), and
  the answer is version-specific, so re-running the matrix on a newer library teaches nothing. They
  were also live hazards: three of the four `FETCH` modes — `PROFILE_DESIRED=0` included — select the
  very configuration that returns rotated intensities, so one stray environment variable could write
  a corrupt archive from a supported build; and `MZPC_SHIMADZU_DUMP` walked the decoded spectrum
  object's public getters by reflection, the same mechanism that got `MZPC_SHIMADZU_DUMP_READER`
  removed for rewriting the `.lcd` it was reading. Their removal also lets the glue's one-entry
  spectrum memo run unconditionally (it was bypassed whenever any lever was set), and collapses
  `SpecFor`/`FetchSpectrum` into one memoised vendor call. `MZPC_SHIMADZU_PROBE` and
  `MZPC_SHIMADZU_DEBUG` stay — neither touches the vendor object outside the ordinary read path.
- **`--mz`**, which never filtered anything: it was declared "NOT YET IMPLEMENTED" and its only
  behaviour, on both the mzPeak→mzPeak and mzPeak→mzML filter lanes, was to abort the run. `--rt`,
  `--ms-level` and `--drop-aux` are unaffected.
- **`MZPC_WATERS_GLUE` from the documentation.** No code path has ever read it: the Waters lane
  loads `MassLynxRaw.dll` directly with `libloading` and takes its directory from
  `MZPC_MASSLYNX_DIR` / `MZPC_PWIZ_DIR`. `docs/USER_MANUAL.md` §10, `docs/PLATFORM_SUPPORT.md`'s
  glue table and its `dotnet build glue/waters` line told operators to build and point at a project
  the converter never loads. (The `glue/waters/` project itself is left in the tree; it is dead
  weight, but deleting it is a call for its owner.)
- Dead code with no caller anywhere in the tree: `finish_with_vendor` (superseded by
  `finish_with_vendor_and_aux`), `filter::u8_child`, `bruker_sdk::frame_mz_minmax` (a diagnostic for
  a resolved allocation bug), `agilent_profile::d_dir` (an identity function "kept for symmetry"),
  and the unread `SciexReader::wiff_path` / `BafReader::baf_file` accessors together with the fields
  behind them.

### Changed

- `#[allow(dead_code)]` on `mod shimadzu_grid`, `mod mz_lattice` and `mod waters` is now
  `#[cfg_attr(not(windows), allow(dead_code))]`, and `src/pwiz_layout.rs` / `src/embed_aux.rs` say
  per item (or once, at module scope) *why* an item has no caller on this host. A blanket allow on a
  module that compiles everywhere hides real rot; the four dead functions above were found underneath
  exactly such an allow.

### Documentation

- **Every text that still called the Shimadzu centroid defect inherent and unreachable is
  corrected** (the gap 0.9.9 recorded). The defect belongs to
  `Shimadzu.LabSolutions.IO.IoModule.dll` **3.8.4.6016**; version **5.0.0.0**, shipped by a current
  ProteoWizard (**3.0.26151** verified), reads the same profile-less `.lcd` files correctly, and
  msconvert only appeared to confirm the defect because it was driving the same old DLL out of the
  same directory. The remedy is a current ProteoWizard — **not** a LabSolutions mzML export.
  - `glue/shimadzu/README.md`: the "Known vendor defect" section is now "Stale-library defect:
    misaligned centroids from `Shimadzu.LabSolutions.IO` **3.8.4.6016**", keeping the measured 3.8.4
    evidence as history beside the 5.0.0.0 numbers, plus how to check the installed `FileVersion`,
    and why a hand-placed `ShimadzuGlue.runtimeconfig.json` must carry the
    `System.Runtime.Serialization.EnableUnsafeBinaryFormatterSerialization` property or **every**
    `.lcd` fails to load.
  - `docs/USER_MANUAL.md` §8 and §11, and `docs/PLATFORM_SUPPORT.md`: the minimum ProteoWizard
    expectation is stated where `MZPC_PWIZ_DIR` is documented, and the **FLASHApp/OpenMS third-party
    bundle is named as a known-stale source** (it carries ProteoWizard 3.0.22187, July 2022, hence
    3.8.4.6016). Both DLL layouts (`vendor_api/<Vendor>` and flat beside `msconvert.exe`) are now
    described, matching the 0.9.9 probe.
  - `REPLY-mzpeak-converter-S30.md` (the speXtract collaborator reply) gains a dated addendum. Note
    for the record: that reply never actually carried the Shimadzu guidance the 0.9.9 "Known gaps"
    note attributed to it — it is a timsTOF document — so the addendum states the corrected
    guidance rather than retracting text that was never there.
  - Older CHANGELOG entries are left as released history with short bracketed
    *[superseded — see 0.9.9]* pointers; nothing measured has been rewritten.
- **`docs/USER_MANUAL.md` §10 now documents every `MZPC_*` variable the converter reads**, split
  into deployment, output-affecting and diagnostic groups: the table named six of the twenty-five
  it reads. The output-affecting group carries the warning that these change the written bytes without
  being recorded in the archive (`MZPC_MAX_SPECTRA` in particular truncates AND disables the
  completeness check, so a partial archive exits 0), and points at the equivalent CLI flag where one
  exists.
- **`docs/mzpeakviewer-compliance-reply.md` told readers to reconstruct lattice m/z the wrong way
  round.** Its 2026-09-02 addendum said the viewer should multiply by `mzpeak:transform_params`
  (`1/scale`) and called `tof_index / scale` "1 ulp off on ~40 % of lattice values" — the exact
  inverse of the contract v0.9.8 shipped, where the DIVISION is normative and the reference reader
  was fixed to divide. The stale paragraph now carries an inline reversal marker and a dated
  addendum states the normative rule (recover the integer scale, divide; multiply only for a
  transform that is not an exact reciprocal). `glue/shimadzu/README.md`'s reconstruct column said
  `1e-9 · tof_index` and now says `tof_index / 1e9`, matching `docs/USER_MANUAL.md` §9, which was
  already correct.
- **Three in-code comments asserted behaviour the code does not have.** `src/shimadzu.rs` claimed
  the `--representation profile` fallback "hits the A5 gate and hard-errors" on a profile-less file
  — there is no such gate and `warn_if_rotated_centroids` deliberately only warns, as its own doc
  says two screens up. `src/shimadzu_grid.rs` and `src/main.rs` justified trimming the scan-window
  zero pad by claiming "the writer drops zero-intensity profile runs anyway"; it does not — the
  compaction keeps one boundary zero per run (the published `HEK_PosOAD1.mzpeak` stores 4.76 M
  zeros), so the route's own `signal_span` trim is the only thing keeping the pad out. `bruker_baf`'s
  `prefer_profile` field still described itself as "currently always false" after `--representation
  profile` started setting it. Also reunited a `sanitize_param_groups` doc comment that a later
  insertion had split in half, leaving `assert_source_complete_tmp` documented by a truncated
  paragraph about an unrelated mzML workaround.
- `README.md` gains the missing **Shimadzu `.lcd` (native)** row, and `docs/PLATFORM_SUPPORT.md`
  the missing **Shimadzu** row in the .NET glue table plus its `dotnet build` line.
- **Two support-matrix rows advertised lanes that open nothing.**
  - *Agilent `.d` (non-IM, native)* was ✅ on Windows "via `AgilentGlueHost.exe`". The two halves
    are different programs: `glue/agilent/Glue.cs` has no `Exports` type and zero
    `[UnmanagedCallersOnly]` attributes, its csproj builds a **net48 EXE** (so no `AgilentGlue.dll`
    and no runtimeconfig), while `src/agilent.rs` still requires both files and resolves six
    exports from `AgilentGlue.Exports` — and nothing in `src/` spawns the EXE or reads its `AGL1`
    output. Every entry point (`convert`, `--to mzml`, `inspect`) fails at open, loudly. The row is
    now ⛔ **not wired**, with the diagnosis and the revive-or-delete pointer to `BACKLOG.md`; the
    matching glue-table row and `dotnet build` line are annotated the same way.
  - *Agilent `.d` profile (`--agilent-grid`)* was ✅ on all three platforms. Neither profile-bearing
    `.d` in the corpus converts today (`LZF: back-reference before output start` on one, an IM-QTOF
    `MSScan.xsd` with no `SpectrumParamsType` on the other), so the row is ⚠️ with both gaps named.
    Consequence for the polarity fix above: it is verified at the scan-record level, not end to end.
- **`glue/waters/` is labelled ⛔ NOT WIRED** at the top of its README and in `WatersGlue.csproj`.
  Both still told operators to build it and point `MZPC_WATERS_GLUE` at it, contradicting the
  removal of that variable from the user-facing docs — and they are the files an operator working
  that lane actually opens. `src/waters.rs` loads `MassLynxRaw.dll` directly from
  `MZPC_MASSLYNX_DIR` / `MZPC_PWIZ_DIR`.
- Two more in-code comments corrected beyond the three above: `tests/shimadzu_lattice_peaks.rs`
  documented itself as asserting `m/z == k · 1e-9` while its assertions pin `k / 1e9` (the division
  is normative — `1e-9` is not exactly 10⁻⁹, which is the whole point of that test), and the
  `tof_grid_spectrum` rustdoc had been stranded by a later insertion above `set_observed_mz_range`,
  leaving that function documented by a paragraph about TOF routing. Same defect class as the
  `sanitize_param_groups` split above; the paragraph is back on its function.

## [0.9.10] — 2026-09-03

### Fixed

- **v0.9.9 did not compile on Windows** — a syntax error inside `#[cfg(windows)] mod agilent`, where a
  doc comment split a multi-line `use`. The host cannot parse that module at all, so `cargo build` and
  `cargo test` were green on macOS while the box failed; the release shell chain then carried on with
  the previous binary, so the box silently reported `0.9.8` and the four Shimadzu reconversions ran
  with the wrong executable. The unit test written to cover that helper never ran either
  (`0 passed; 78 filtered out`) because it lived inside the same gated module.
  `agilent_dll_dir` now lives in a new, UNGATED `src/pwiz_layout.rs` with its test, so it compiles and
  runs on every host (`1 passed`, verified); only the FFI stays Windows-gated. Third Windows-only
  break of the day — the durable fix is to require the box to build the exact commit and print the
  expected `--version` before any tag, and to confirm a new test actually ran rather than trusting a
  green suite.

## [0.9.9] — 2026-09-03

### Fixed

- **The Shimadzu "vendor defect" was a STALE LIBRARY, and it is now fixed.** Since 0.9.0 this project
  has documented, in the CHANGELOG, `glue/shimadzu/README.md` and a collaborator reply, that centroid
  intensities on profile-less `.lcd` files come back misaligned against their m/z (shifted 1–7
  positions, the highest-m/z peak dropped), that no API lever reaches it, and that msconvert
  reproduces it byte-identically — so the only remedy was a LabSolutions mzML export. **Every one of
  those statements is true only of `Shimadzu.LabSolutions.IO.IoModule.dll` 3.8.4.6016**, the version
  bundled with the ProteoWizard tree `MZPC_PWIZ_DIR` had been pointed at (the FLASHApp/OpenMS
  third-party bundle: ProteoWizard **3.0.22187, July 2022**). Version **5.0.0.0** of the same library,
  shipped by current ProteoWizard (**3.0.26151** verified), reads those files CORRECTLY. msconvert
  appeared to confirm the defect only because it was driving the same old library.

  Measured on `DIA_Hela_20ng.lcd`, spectra 1/10/100/1000, against the LabSolutions 5.128 SP2 export:

  | reader | peaks | intensities |
  |---|---|---|
  | this converter + 3.8.4.6016 | 611 / 14,299 / 13,557 / 11,360 | shifted 1–7, header scalars at the head |
  | msconvert + 3.8.4.6016 | identical to the above | identical to the above |
  | msconvert + 5.0.0.0 | **612 / 14,300 / 13,558 / 11,361** | **max \|Δ\| = 0**, m/z to 5e-5 (coarse `Mass`) |
  | **this converter + 5.0.0.0** | **612 / 14,300 / 13,558 / 11,361** | **max \|Δ\| = 0**, m/z to **2e-13** (`MassHigh`) |

  Files that DO store profile signal (`Blind_P1_pos_012.lcd`) were always exact, through either
  library. Reader guidance is therefore reversed: use a current ProteoWizard, not the mzML export.

- **The glue can load the newer library at all.** 5.0.0.0 deserialises part of the `.lcd` through
  `BinaryFormatter`, disabled by default since .NET 5, so `LoadData` threw `NotSupportedException`
  inside a `TargetInvocationException` and every file looked unreadable. Enabled with
  `<EnableUnsafeBinaryFormatterSerialization>` in the glue project. The bare
  `RuntimeHostConfigurationOption` form does NOT work — the SDK's own default wins and the generated
  `runtimeconfig.json` still reads `false`. Note .NET 9 removes `BinaryFormatter` outright, so a
  retarget needs a vendor DLL that does not use it. Security: the switch is scoped to this glue, whose
  input is a vendor instrument file the user chose to convert — the same path ProteoWizard itself runs.

- **The Agilent lane no longer pins the whole box to one ProteoWizard layout.** It hard-required
  `<MZPC_PWIZ_DIR>/vendor_api/Agilent`, which only the bundled build provides; the standalone
  installer flattens those DLLs beside `msconvert.exe`. `agilent_dll_dir` probes both (subdirectory
  preferred), so one current ProteoWizard serves every lane. This is what had kept the box on the
  bundle, and thus on the broken Shimadzu reader.

### Known gaps in this release

- The one-shot runtime warning, `glue/shimadzu/README.md`, the 0.9.x entries above and the
  `REPLY-mzpeak-converter-S30.md` sent to speXtract still describe the defect as inherent and
  unreachable. The CODE is correct as of this release; those TEXTS are not yet corrected, and a
  reader of them will be misled about which library to use. Correcting them, and deciding whether the
  warning should fire at all (ideally only on a known-bad library version, which the glue could report
  over the existing ABI), is the next change.
- Archives converted before this release from a profile-less Shimadzu `.lcd` carry the misaligned
  intensities. Reconvert them with a current ProteoWizard.

## [0.9.8] — 2026-09-03

### Added

- **The fixed-point m/z lattice is no longer Shimadzu-native-only: any input whose centroids are
  vendor integers over a power of ten now gets it, the mzML lane included.** Some vendors' m/z are
  not really floating point — Shimadzu `MassHigh` is an Int64 at 1e-9 Da, its coarse `Mass` field an
  Int32 at 1e-4 Da, and LabSolutions' own **mzML export** writes those same 1e-9 values as f64
  `binary`. The native `.lcd` lane has stored them as `point.tof_index = round(m/z · 1e9)` (Int64,
  DELTA_BINARY_PACKED) since v0.9.0; the mzML lane had to choose between LOSSY numpress-linear and
  67 % more space. Measured on the 4.5 GB LabSolutions `DIA_Hela_20ng` mzML, 279,707,903 centroids:

  | encoding | archive | m/z bytes | lossless |
  |---|---|---|---|
  | delta chunking (today's default on this data) | 2,188,011,754 B | 1,896,603,579 B | yes |
  | numpress-linear | 1,354,604,168 B | ~847 MB | **no** |
  | **m/z lattice (this change)** | **1,311,772,262 B** | **1,034,748,033 B** | yes (bit-exact) |

  i.e. −40.0 % on the archive and −45.4 % on the m/z column against the lossless baseline, and
  smaller than the lossy one. `Blind_P1_pos_012.mzML` (13,200 spectra, 216,742 centroids):
  3,708,762 B → 2,264,008 B, −39.0 %, with all 216,742 m/z reconstructing to the source f64
  **bit for bit** (`mzpeak-convert … --to mzml` back out of the archive: 0 of 216,742 differ).

  - **`src/mz_lattice.rs`** is the vendor-neutral mechanism, parameterised by scale: the detector,
    the per-spectrum guard, the `spectra_peaks` schema (`spectrum_index`, `tof_index` Int64 with
    `LinearMz` + `mzpeak:transform_params = 1/scale`, the f64 `mz` fallback, `intensity`) and the
    `mz_calibration` index block (`"codec": "mz-grid"`). `src/shimadzu_grid.rs` now delegates to it
    at a fixed 1e-9, so that lane's archive shape is unchanged (`tests/shimadzu_lattice_peaks.rs`
    still pins it; its one edit is the reconstruction formula, below).
  - **The detector returns the scale.** `is_fixed_point_lattice` (bool) became
    `mz_lattice::fixed_point_lattice_scale` → `Option<f64>`, naming the COARSEST of 1e3/1e4/1e5/1e9
    that reproduces every sampled value (coarsest first keeps `k` as small as possible; a value on
    the 1e-3 lattice is trivially also on the finer ones). `refine_chunking`'s delta-vs-numpress
    decision is unchanged — it reads the same answer as `.is_some()`.
  - **Nothing is snapped and no spectrum is refused.** The guard is per spectrum and
    all-or-nothing: one value off the lattice (an interpolated apex) and that spectrum's exact f64
    m/z goes to the same facet's `mz` column instead. `tof_index` is NULL on those rows, `mz` is
    NULL on the lattice rows.
  - **Only the peaks facet.** Profile arrays keep the chunked data facet and every existing choice
    (`--layout`, `--chunk-size`, `--no-numpress`, an explicit strategy) exactly as before, so a
    profile-only input is byte-identical. A run with both gets a point peaks facet beside a chunked
    data facet — the mixed layout family this converter writes on purpose (the writer warns).
  - **Precedence and opt-outs.** On the peaks facet the lattice supersedes both numpress-linear and
    delta, because it is lossless AND smaller than either. `--tof-grid` (the sqrt/flight-time grid,
    a different thing) still takes the file where it is asked for and fits. New `--no-mz-lattice`
    (config `no_mz_lattice:`, or `$MZPC_NO_MZ_LATTICE`) turns it off entirely.
  - **The summaries stay real.** A lattice-routed spectrum keeps its m/z array on the
    `MultiLayerSpectrum` — only the facet ROWS become integers — so the writer's own MS:1000285 /
    MS:1000504 / MS:1000505 / MS:1000527 / MS:1000528 derivation is still fed real m/z. Verified,
    not assumed: on `Blind_P1_pos_012` all five columns are identical, value for value, to the same
    file converted with `--no-mz-lattice` (13,200/13,200), and `tests/mz_lattice_mzml.rs` asserts
    that equality on every fixture spectrum. This is the bc8497c regression, which this route must
    not reintroduce.
  - **Reconstruction is the DIVISION, and now exact.** The `mz_calibration` block has said
    `mz_from_tof_index: "tof_index / scale"` since v0.9.0, but the reference reader multiplied by
    the column's `mzpeak:transform_params` (`1/scale`) instead — and those are different numbers:
    `1e-9` is not exactly 10⁻⁹, so `k · 1e-9` is not the correctly-rounded `k / 1e9` for about 40 %
    of `k` (measured on `Blind_P1_pos_012`: 85,706 of 216,742 centroids, up to 1.137e-13 Da). That
    made "lossless" false for the path readers actually took, and would have turned the mzML lane's
    previously bit-exact round trip into a one-ulp-lossy one. `vendor/mzpeak_prototyping/src/
    reader/point.rs` now recovers the integer scale from the params (rounding `1/s` and verifying
    that `1/scale` reproduces `s` bit for bit) and DIVIDES, falling back to the multiply for a
    transform that is not an exact reciprocal. The round trip is bit-exact at every scale, on the
    Shimadzu native archives too, and `tests/shimadzu_lattice_peaks.rs` pins `k / 1e9` where it
    previously pinned `k · 1e-9`.
  - **The per-spectrum guard is no longer scale-blind.** `LATTICE_TOL`, the floor of the guard on
    the SCALED value, was 1e-3 — sized for the 1e-9 lane, where it means 1e-12 Da. At the 1e3/1e4/
    1e5 scales this change adds it would have meant up to 1e-6 Da, i.e. a genuinely off-lattice
    value would have been SNAPPED onto the lattice instead of keeping its f64 — the opposite of the
    stated invariant, and 1000× looser than the detector that armed the route. The floor is now
    1e-6 and both the detector and the guard go through one `on_lattice_scaled`, so they cannot
    drift apart again. No effect on real data: `Blind_P1_pos_012` still routes 13,200/13,200
    spectra and `DIA_Hela_20ng` 21,500/21,500.
  - **A probe-derived scale is checked, and a heavy fallback is no longer silent.** The scale comes
    from six probe spectra but the schema is run-wide, so a spectrum that misses it stores f64 m/z
    in a point column that is neither chunked nor numpressed — correct values, but a run that lands
    mostly there can be BIGGER than the same file with `--no-mz-lattice`. `probe_lattice_scale` now
    re-asks `centroid_lattice` per probe list (the pooled detector does not check the
    non-decreasing-`k` rule), and the post-loop tally warns when more than 10 % of the routed
    spectra kept f64, naming `--no-mz-lattice`. Mixed-lattice input — two different scales in one
    run — is the case that provokes it.
  - **Tests.** 17 unit tests in `mz_lattice` (each of the four scales detected by name, the
    coarsest-wins rule, non-lattice / short / f32-rounded input rejected, the `1/scale` params
    string, the per-spectrum guard at both scales, a coarse-scale value 5e-7 Da off its lattice
    NOT being snapped, an off-lattice spectrum falling back beside an
    on-lattice one, the four-column schema and the calibration block at two scales), two
    scale-binding pins left in `shimadzu_grid`, and `tests/mz_lattice_mzml.rs` — three end-to-end
    conversions through the real binary over two new committed fixtures (`tests/data/
    mz_lattice_1e9.mzML`, one of whose 12 spectra is deliberately off-lattice, and
    `mz_lattice_1e4.mzML`), asserting the index block, the Int64/DELTA_BINARY_PACKED/ZSTD/no-dictionary
    column contract, the reader round trip against the source mzML, the null pattern of the
    `tof_index`/`mz` pair, the summary columns, and that a NON-lattice input converts to
    byte-identical parquet members with the lattice on and off. Both fixtures now span a realistic
    120–1900 Da and the round trip is asserted BIT-FOR-BIT rather than against an epsilon: at
    m/z 512 and above, one ulp is larger than the old `1e-4/scale` tolerance, so that assertion
    could not have told the exact quotient from the one-ulp-off product it now pins.

### Changed

- **An mzML whose centroids are on a fixed-point lattice now converts to an INTEGER m/z axis by
  default.** Its peaks facet carries `point.tof_index` (Int64) with `point.mz` NULL on the routed
  rows, where before v0.9.7 every mzML archive had f64 `point.mz`. `mzpeak-convert`'s own reader
  and mzPeakViewer reconstruct it (the `mz-grid` codec); the other readers in the mzPeak family —
  OpenMS's `MzPeakFile`, mzPeakJ, mzPeakIV, mzPeakExplorer, mzPeakValidator — do not yet, and read
  those cells as 0. Pass `--no-mz-lattice` (config `no_mz_lattice: true`, or
  `MZPC_NO_MZ_LATTICE=1`) for an archive destined for one of them; the flag now applies to EVERY
  lane, the native Shimadzu `.lcd` one included, where it previously did nothing. Which files this
  affects is decided by the data, not the extension: on a ~80-file general-MS sweep exactly the
  Shimadzu LabSolutions exports fire.
- Re-converting a lattice archive through the mzpeak-to-mzpeak filter path is not size-preserving
  (that lane rewrites the peaks facet without the `*_index` writer props, so `tof_index` loses
  DELTA_BINARY_PACKED). Pre-existing — the pre-change binary produces a byte-identical inflated
  archive from the same input — but until now only the Windows-only Shimadzu native lane could
  produce such archives. Tracked separately.

### Fixed

- **Grid-routed spectra shipped `total_ion_current = 0`, no base peak, and NULL observed-m/z
  bounds.** Every route that replaces a spectrum's f64 `m/z array` with an integer axis
  (`tof_index` / `tof`) handed the writer a `BinaryArrayMap` with no m/z, and mzdata derives the
  per-spectrum summaries from m/z + intensity: an m/z-less map folds to `tic = 0`,
  `base peak = (0, 0)`, `m/z range = (0, 0)`. Measured on the published corpus: Shimadzu
  `Blind_P1_pos_012.mzpeak` 13,200/13,200 spectra with TIC 0, `MTBLS5861/HEK_PosOAD1.mzpeak`
  2,092/2,101 (exactly the gridded set — the 9 f64 fallbacks were correct),
  `agilent-qtof/…-S25.mzpeak` 1,502/1,502, and the SCIEX `Sample002.mzpeak` 112,610 gridded rows
  with TIC 0 and NULL base peak. The peak DATA was always intact — recomputing TIC from the archive
  peaks reproduces the LabSolutions oracle exactly, and the archive TIC/BPC chromatograms are
  bit-identical to it — so only these summary columns were wrong. Fixed in both halves:
  - **Converter.** A shared helper (`summarize_points` / `set_spectrum_summary_params` /
    `set_gridded_spectrum_summary`, next to `set_observed_mz_range` in `src/main.rs`) computes
    MS:1000285 total ion current, MS:1000504/MS:1000505 base peak m/z + intensity and
    MS:1000528/MS:1000527 observed range from the (m/z, intensity) pairs the route is about to
    discard, and states them on the `SpectrumDescription`. Applied at every such site: the mzML
    `--tof-grid` lane, the Shimadzu profile grid route, the SCIEX per-spectrum grid, the Agilent
    `MSProfile.bin` grid (m/z reconstructed the way a reader does — polynomial-refined when the
    spectrum has a calibration row), and both timsTOF ims-compact lanes (native + `--bruker-sdk`),
    which had the same defect on all 3,994 frames of PXD059079 2485.d. Ties in intensity resolve to
    the lowest m/z (mzdata breaks them first-in-array; the two coincide on an m/z-ascending source
    array, and lowest-m/z is the reproducible rule on the ims lanes, whose points are grouped by
    mobility scan); a point whose m/z is not finite AND positive can never be the base peak, though
    its intensity still counts towards the TIC; an empty or all-zero spectrum keeps TIC 0 and gains
    NO base peak. The terms REPLACE any the source stated, so a gridded archive agrees with the same
    input converted without the grid (`swath.api-sample-centroid.mzML` declares a profile-mode
    `MS:1000285` of 1.184903e6 where its own centroid points sum to 272,543).

    On a DUAL Shimadzu scan (profile trace + centroid list, the shape of both published `.lcd`
    archives) the stated summary is the PROFILE SIGNAL SPAN — the points written to the
    `spectra_data` facet, not the zero-padded source array and not the centroid list riding
    alongside. That is what the writer derives for the same file with `--tof-grid` off (the raw
    array map wins over the peak list) and what its own `base_peak_mz` precedence already used, so
    the two lanes describe the file identically. Concretely, `Blind_P1_pos_012` spectrum 0 now
    reports 13,220 (profile), not 12,877 (centroid); the TIC/BPC chromatograms are built from
    `peaks()` and stay centroid-derived at 12,877, a split that predates this change. The published
    `HEK_PosOAD1.mzpeak` settles it from within: its 9 never-broken rows (the off-lattice spectra
    kept as f64) carry the PROFILE sum exactly — row 149 is 672,849 profile vs 607,167 centroid,
    and the column says 672,849 — so stating the profile sum on the other 2,092 makes the column
    mean the same thing on every row of the file.
  - **Vendored writer (defence in depth).** `writer/visitor.rs` now falls back to the explicit
    MS:1000285 / MS:1000504 / MS:1000505 params — the same fallback the observed-m/z range already
    had — at both the spectrum and the wavelength builder, so no future lane can ship zeros. The
    gate names the CAUSE, "the raw arrays carry signal on a non-m/z axis", rather than the symptom
    "no summary came out anywhere". Both halves matter: a dual `.lcd` scan keeps its centroid
    `PeakSet` through the grid route, so a symptom gate would skip the fallback and leave the two
    published Shimadzu archives at TIC 0; and an mzML `defaultArrayLength="0"` scan still carries
    MS:1000285/504/505 in its header, so a symptom gate would stamp a measurement onto a row with
    zero data points and zero peaks. A spectrum whose raw arrays DO carry m/z is untouched, and a
    genuinely empty spectrum still gets NULL bounds, no base peak and TIC 0 even when its header
    states otherwise.

  Verified on this host: `swath.api-sample-centroid.mzML --tof-grid on` went from 201/201 spectra
  with TIC 0 and a NULL base peak to 0/201, with all five summary columns now bit-identical to the
  `--tof-grid off` lane; PXD059079 2485.d went from 3,994/3,994 frames with TIC 0 to 0/3,994 on
  BOTH ims lanes (compact and `--ims-chunked`), the TIC column matching the sum of the archive's own
  stored points to Float32 precision (max relative error 5.8e-8) and the base peak lying inside the
  observed range on all 3,994 rows. No ordinary lane moved: the 31 MB Thermo FT-ICR mzML
  (4,880 spectra, 2,847 of them empty) and the Waters `DDA_IsolationWindow.mzML` (2 empty spectra)
  are byte-for-byte unchanged in all five summary columns against the pre-fix binary. The Shimadzu
  and Agilent `.d`/`.lcd` lanes are Windows-only and must be re-measured on the Flash box.

### Removed

- **The reflective Shimadzu reader dump (`MZPC_SHIMADZU_DUMP_READER=1`) is gone.** It walked the
  vendor `DataObject` graph invoking every public property getter, and those getters make the vendor
  library rewrite the `.lcd` it is reading: measured on a copy of `HEK_PosOAD1.lcd`, the size stayed
  at 63,188,992 B but 33,661 bytes changed across the OLE2 header and directory sectors, five streams
  under `Mass Data Load Format` were DELETED (including `Profile Load Parameter`, which is why the
  next profile read failed with `E_FAIL`), and two storage timestamps were reset — while no stream's
  content changed. `IDataIO.LoadData` has no read-only overload, so a lazy getter commits straight to
  disk. The lever had already served its purpose (finding scan windows and instrument identity) and
  nothing depends on it. The conversion path calls only readers plus `IO.Close` and does not write;
  the read-only diagnostics (`MZPC_SHIMADZU_PROBE`, `_FETCH`, `_DUMP`) are unaffected.

## [0.9.7] — 2026-09-03

### Added

- **SDK-verified (2026-09-03):** Bruker's own `tims_index_to_mz` (timsdata SDK, via
  `MZPC_TDF_SDK_GOLDEN` on the Flash box) agrees with the archive's per-frame pair to **1.0e-7 ppm**
  on 240 (frame, tof) points over 12 frames of PXD059079 2485.d; the run-wide chord is 4.28 ppm off.
  Pinned corpus-free by `sqrt_linear_pair_matches_the_vendor_sdk_goldens` with the fixture
  `tests/fixtures/tdf_2485_sdk_golden.json`.
- **timsTOF ims-compact: exact per-frame `tof_c0`/`tof_c1` when the vendor calibration is
  sqrt-linear (`C2 = 0`).** The Bruker ModelType-1 model (speXtract, 2.5e-5 ppm vs the SDK) is
  `C2·u² + (1e6/√C1_eff)·u + (C0 − t_ns) = 0`, `m/z = u²`, with `C1_eff` temperature-corrected
  per frame; when `C2 = C3 = C4 = dC2 = 0` it is EXACTLY `m/z = (c0 + c1·tof)²` per frame with
  `c1 = DigitizerTimebase·√C1_eff/1e6`, `c0 = (DigitizerDelay − C0)·√C1_eff/1e6`. Both ims-compact
  lanes (native timsrust and `--bruker-sdk`) now resolve each frame's `MzCalibration` row
  (`Frames.MzCalibration → Id`, `Frames.T1` for the temperature) and, when EVERY referenced row
  is of that form — `C2`/`C3`/`C4`/`dC2` STORED as numeric zeros: a NULL or text cell reads as 0
  for the informational model but is a missing term, not a zero, and keeps the run on the chord,
  as the reference `TdfMzCalibration.h` refuses it — attach the pair as
  per-spectrum Float64 params `tof_c0`/`tof_c1` (`TOF_C0_CURIE`/`TOF_C1_CURIE`, the sqrt-grid
  lanes' columns), stamp the `tof` column with `mzpeak:transform_params_per_spectrum =
  "tof_c0,tof_c1"` and add `per_spectrum: "tof_c0,tof_c1"`, `exact_per_spectrum: true` and a
  note to `ims_calibration` (`a`/`b` and `exact: false` kept for legacy readers). A frame whose
  `Frames.T1` is NULL cannot be evaluated (the vendor library would see 0 K through
  `sqlite3_column_double`, ~5e-4 on `C1`, or drop the term — unknowable) and gets NO pair: NULL
  `tof_c0`/`tof_c1` cells, which both the vendored reader and the viewer already treat as "chord
  for this spectrum"; the count is recorded as `ims_calibration.per_spectrum_chord_frames`, and
  `exact_per_spectrum: true` is a per-spectrum statement (a spectrum WITH the pair is on the
  model). Any `C2 ≠ 0` row → no columns, no keys, archive unchanged. The `--bruker-sdk` lane
  claims exactness only when the `T1`/`MzCalibration` columns were actually read (`read_frames`
  now reports it), mirroring the native lane's `table.t1.len() == frames.len()` guard — a TDF
  without them stays on the chord on both lanes. `src/bruker_native.rs`: `TdfMzCalibrationRow`
  (`tof_to_mz` general formula, cancellation-free quadratic; `is_sqrt_linear`;
  `sqrt_linear_coeffs`; `quadratic_terms_stored`), `read_mz_calibration_rows` (NULL → 0 +
  stored-ness), `exact_tof_coeffs` / `exact_tof_coeffs_for` (all-or-nothing on the model,
  per-frame `None` for NULL `T1`, self-checked against the general formula), `ExactTofSummary`,
  `add_exact_tof_params`; `NativeTofReader.exact_tof` + `observed_mz_range` on the exact pair;
  `src/bruker_sdk.rs` `TdfSdkReader.exact_tof`; `src/main.rs` `write_ims_compact_archive*`
  gain `exact_per_spectrum: Option<ExactTofSummary>`. The vendored reader's per-spectrum fixup
  (`reader/point.rs reconstruct_per_spectrum_grid_mz`) keys on the `SqrtMzFromTof` array index
  entry, not on an array name, so it covers ims-compact's `tof` column — but it only ran on the
  PROFILE facet, and ims-compact keeps its points in the PEAKS facet (see Fixed below);
  `mzpeak-convert ARCHIVE -o x.mzML` now emits the ModelType-1 m/z. On PXD059079 2485.d (one
  row, `C2 = 0`): 3,994/3,994 frames carry the pair, `(c0 + c1·tof)²` vs the model 5.9e-16
  relative, chord off by up to 4.29 ppm; the vendored reader's m/z (654,681 points over 5 frames,
  flat and `--ims-chunked`) and the mzML export's (577,096 points, back on the integer tof
  lattice to 1.2e-10 bins) sit on the per-frame pair, 4.2–4.3 ppm from the chord. Caveat: the
  ModelType-1 formula is SDK-verified on `C2 ≠ 0` rows only (all 60 goldens), so the `C2 = 0`
  branch reproduces the FORMULA exactly and is not yet itself checked against `tims_index_to_mz`;
  a `MZPC_TDF_SDK_GOLDEN` dump of 2485.d from the box, dropped in as
  `tests/fixtures/tdf_calibration_golden_c2zero.json`, is picked up by
  `c2_zero_sdk_goldens_match_the_sqrt_linear_pair` (< 1e-4 ppm per point; the test says
  "skipping" until the fixture exists).
- `MZPC_TDF_SDK_GOLDEN=<out.json>` (`--bruker-sdk` TDF conversions, Windows/Linux): samples the
  SDK's `tims_index_to_mz` at up to 240 points (frame 1, last, 10 evenly spaced × 20 tof values
  over `0..DigitizerNumSamples−1`; `bruker_native::sdk_golden_sample_plan`) and writes
  `{file, digitizer_num_samples, mz_calibration, points: [{frame, t1, t2, cal_id, tof, mz_sdk}]}`
  (`TdfSdkReader::dump_sdk_golden`). Zero cost unset; never fails the conversion. Manual §8/§10.
- Tests: `bruker_native::exact_tof_calibration_tests` — the derived pair reproduces the
  ModelType-1 formula at tof 0/1e5/3e5/6.36e5 to 1e-12 relative at a frame `T1` ≠ row `T1`, the
  quadratic branch reproduces all 60 speXtract SDK goldens (`tests/fixtures/
  tdf_calibration_golden.json`, BSD-3) to < 1e-4 ppm, NaN on out-of-model input, per-frame
  resolution (NULL/NaN `T1` → no pair, NULL id, mixed rows, unknown id, unstored quadratic
  terms), sqlite NULL/text `C2`/`C3`/`C4`/`dC2` → chord and NULL `Frames.T1` → no pair on a
  synthetic TDF, sampling-plan shape, the fixture-gated `C2 = 0` SDK golden check;
  `tests/contract_strings.rs ims_compact_per_spectrum_exact_pinned` pins the viewer contract
  (`per_spectrum`/`exact_per_spectrum`/`per_spectrum_chord_frames` keys, the
  `mzpeak:transform_params_per_spectrum` stamp, the `tof_c0`/`tof_c1` column names behind the
  `_tof_c0`/`_tof_c1` suffix the viewer matches, and the MS:4000900/4000901 accessions);
  `tests/tdf_exact_tof_calibration.rs` (corpus-gated, ~3 s) converts 2485.d with the default
  lane and asserts the `ims_calibration` keys, the pair on all 3,994 frames, 50 frames × 10 tof
  values vs the model < 1e-12, the vendored reader's peak-facet arrays
  (`get_spectrum_peak_arrays_for`) AND the collapsed `get_spectrum` peak list equal to the
  per-frame reconstruction (not the chord) with the chord > 1 ppm off, the mzML export's m/z back
  on the integer tof lattice of the per-frame pair, and the same on an `--ims-chunked` archive
  (chunk-reader branch).
- The viewer counterpart (mzPeakViewer `packages/core`: per-spectrum exact pair on every ims-compact
  tof → m/z path and the XIC) is recorded in `~/Claude/mzPeakViewer/CHANGELOG.md`, not here.

### Fixed

- **Vendored reader: per-spectrum sqrt TOF-grid m/z on the PEAKS facet.** The per-spectrum
  `tof_c0`/`tof_c1` fixup (`reader/point.rs reconstruct_per_spectrum_grid_mz`) ran only on the
  profile facet (`get_spectrum_arrays`, and `get_spectrum`'s post-pass over `spectrum.arrays`);
  ims-compact stores its points in the peaks facet, whose four read branches (single row group
  via the peak cache, multi row group, and both chunked paths) materialised the run-wide chord and
  collapsed straight into a `PeakDataLevel` — so every consumer of an ims-compact archive
  (`get_spectrum`, `mzpeak-convert ARCHIVE -o x.mzML`) stayed on the chord no matter what
  `spectra_metadata` carried. `MzPeakReaderType::get_spectrum_peaks_for` now goes through a new
  `get_spectrum_peak_arrays_for` (public: the peak-facet arrays with the integer `tof` still
  alongside the reconstructed m/z), which applies the per-spectrum fixup before the collapse when
  the facet's grid column is `SqrtMzFromTof`; `get_spectrum` hands it the description params it
  already read (no second metadata row fetch). Same on the async reader (that feature does not
  build in this checkout for unrelated reasons — missing `AsyncChunkReader` — so it is mirrored,
  not compiled). The now-unused `PointDataReader::get_peak_list_for` (sync + async) is gone.
  `vendor/mzpeak_prototyping/src/reader.rs`, `reader/object_store_async.rs`, `reader/point.rs`.
  Two consequences to know about: (1) the fixup is gated on the grid column's
  `mzpeak:transform_params_per_spectrum` stamp, read from the peaks facet's footer schema that
  is already in hand (`reader/point.rs schema_declares_per_spectrum_grid`, unit-tested on nested
  point and chunk schemas) — so the stamp the converter has written since the sqrt-grid lanes is
  now load-bearing, and a legacy chord-only ims-compact archive neither re-reads the
  spectrum-metadata row per spectrum nor changes output (verified: a v0.9.6-built 2485 archive
  exports the same chord lattice through the new reader); (2) every OTHER `SqrtMzFromTof` peaks
  facet gets the per-spectrum pair too: native SciEX (`point.tof_index` with the run-wide `0,1`
  placeholder) previously yielded NO m/z on the peaks facet, so `get_spectrum` returned
  `PeakDataLevel::Missing` and the mzML export of a SciEX archive was empty — it now yields a
  Centroid peak list on the spectrum's own `tof_c0`/`tof_c1`; Agilent tof-grid archives
  previously collapsed every spectrum on the FIRST spectrum's chord hint and now use each
  spectrum's own pair (the polynomial refinement the viewer applies is still not applied by the
  vendored reader). Neither lane is exercised on the macOS host; the box's SciEX `.wiff` →
  `.mzpeak` → mzML round trip is the check.

### Changed

- `tools/compare_lcd_native_mzml.py` (the Shimadzu native-lane release gate) reads the v0.9.5+ lattice
  centroid facet (`point.tof_index` × 1e-9 with the f64 `point.mz` fallback) and slices the point layout
  at spectrum boundaries instead of one boolean mask per spectrum (280 M rows × 21,500 spectra never
  finished). On the v0.9.6 DIA_Hela_20ng archive it reports the same vendor-defect signature as on the
  0.9.4 f64 archive and max |Δm/z| 2.27e-13 against the LabSolutions export.

## [0.9.6] — 2026-09-02

### Added

- **timsTOF isolation-window mobility band now carries accessions (MZP:1000006 / MZP:1000007).**
  The `isolation window inverse reduced ion mobility lower/upper limit` params on every selected
  ion (`src/bruker_native.rs` `add_isolation_mobility_band`) were name-only because PSI-MS has no
  term for an isolation window's 1/K0 bounds and mzdata's CURIE `Display` panics on a non-PSI
  prefix. They are now the converter's provisional `cv/mzpeak.obo` terms, represented as
  `Unknown`-CV CURIEs and rendered `MZP:` by the vendored writer/reader (`param::curie_to_string` /
  `parse_curie`). Unit stays MS:1002814. Both ims-compact lanes (native, `--bruker-sdk`) and the
  `--no-ims-compact` lane write them; on PXD059079 2485.d: 15,977/15,977 selected ions,
  `lower <= ion_mobility_value <= upper` everywhere, validator PASS on both lanes.
- The archive `cv_list` lists the MZP vocabulary
  (`{id: MZP, full_name: "mzPeak converter provisional controlled vocabulary", version: 0.1.0,
  uri: …/cv/mzpeak.obo}`) on every timsTOF conversion — also an MS1-only run that never writes a
  band; the entry is spec-legal and costs one row (`ensure_mzp_cv` in `main.rs`;
  `mzpeak_prototyping::param::mzp_cv_entry` / `ensure_mzp_cv_entry`).
- `tests/tdf_ims_window_band.rs` (corpus-gated, ~10 s): converts 2485.d both ways, asserts the
  band + accession on every selected-ion row of both archives, lane agreement on 1/K0 and band to
  1e-9, the pinned ModelType-2 value on frame 2 / m/z 1276.05, the spectrum-level
  `ion mobility lower/upper limit` pair ordered and equal to the band wherever present,
  `--no-tims-recalibration` giving the same (linear, 1.317349) value in both lanes, and a
  panic-free mzML export with the band as `userParam`. Unit tests: MZP CURIE string/JSON round trip (vendored `param.rs`),
  writer→reader round trip + demotion + mzdata mzML write of an MZP param (`main.rs`),
  `TdfMobilityRemap` arithmetic and band ordering (`bruker_native.rs`).
- Two-row `MzCalibration` fixture (`bruker_native::vendor_mz_calibration_tests::
  two_calibration_rows_select_per_frame_and_null_frame_is_tolerated`): both rows carried verbatim
  in `vendor_mz_calibration`, `tdf_mz_calibration_id` selects the right row per frame, and a
  `Frames` row with NULL `T1` yields no per-frame calibration for that frame only (no abort, no
  index shift).

- `tests/tmp_cleanup.rs`: a conversion whose final rename fails (the output path is an occupied
  directory, `--force`) exits non-zero and leaves no `<out>.mzpeak.tmp`; unit tests for the guard's
  drop / finish / failed-rename paths, the panic hook, and the vendor-reader tally
  (`vendor_reader_tally_counts_written_spectra_not_probes`).
- **User manual:** the Shimadzu native lane (§6, §8, §9, §10, §11 — `MassHigh` vs the coarse
  `Mass`, `MZPC_SHIMADZU_COARSE_MZ`, the per-spectrum sqrt grid and the Int64 centroid lattice with
  their index blocks and the per-facet reader rule, measured sizes and fidelity, the vendor
  centroid-intensity defect on profile-less `.lcd` files *[that manual text described the defect as
  inherent; corrected after 0.9.9 — it is specific to `Shimadzu.LabSolutions.IO` 3.8.4.6016]*),
  `--representation` in the option table,
  and for timsTOF: `ims_calibration.exact = false` with the chord's measured error, and the
  multi-precursor pairing rule. `glue/shimadzu/README.md` gains a "What the archive stores"
  section; the viewer compliance reply an addendum on resolving integer axes per facet.

### Fixed

- **A failed conversion no longer leaves `<out>.mzpeak.tmp` behind.** Every lane writes to the
  tmp and renames it into place at the end, and any failure in between — a writer error, the rename
  itself, or the peak-writer-open panic introduced in 0.9.5 (which fires on the main thread before
  the ims-compact writer thread exists) — left the partial file beside the missing output. A
  `TmpGuard` (`main.rs`) now owns the tmp in all six lanes (mzML/imzML/Thermo, TOF-grid, Agilent
  grid, ims-compact, SciEX grid, and the shared vendor-reader path used by TSF/BAF/SDK/Agilent
  MIDAC/Waters/Shimadzu): it is removed when the guard drops on an error and, because the release
  profile is `panic = "abort"` and runs no destructors, by a panic hook that sweeps the in-flight
  tmp files (all of them under abort; only the panicking thread's own under unwind, so a caught
  panic in a test process cannot remove another conversion's file). The successful rename is the
  only thing that disarms it. The output path is never touched: the `--force`, in-place and
  nested-output refusals are as before. The sanitized copy `sanitize_param_groups` writes for an
  mzML with empty `<referenceableParamGroup/>` elements (`mzpc-san-*.mzML` in the temp dir) is now
  an RAII guard too (`SanitizedTemp`): it used to survive every failed conversion and every
  `-o x.mzML` export (`tests/tmp_cleanup.rs::failed_conversion_leaves_no_sanitized_copy_behind`).
- **Shimadzu run summary counted the schema probes.** `convert_vendor_reader` fetches up to six
  spectra through the lane's routing closure before the write loop to sample the facet schema, and
  the "N spectra on the sqrt grid / on the 1e-9 m/z lattice" counters lived in that closure, so the
  logged totals exceeded the spectrum count by the probe count (13,206 for the 13,200-spectrum
  Blind_P1_pos_012). Each spectrum now reports its route (`FacetRoutes` on `VendorSpectrum`) and
  the counting moved into the write loop of the host-independent `convert_vendor_reader`
  (`FacetTally`), which also logs the summary; `shimadzu_grid_route` lost its counter arguments and
  its `#[cfg(windows)]` (only the `.lcd` reader is Windows-only), so the routing compiles and is
  tested on every host. Archives are unchanged.

- **Viewer (`mzPeakViewer/packages/core`): the non-engine spectrum readers decode grid-encoded
  facets instead of reading the null-filled `mz` verbatim.** Since the Shimadzu Int64 `mz-grid`
  lattice landed, the engine's `reconstructSpectrum` resolved the grid per facet, but two readers
  still took `centroids[i].mz` / `dataArrays["m/z array"]` as-is — `src/reader/arrays.ts
  harvestDataArraysOrNull` (the ion-image / mean / ROI source read) and `src/reader/explorer/
  browse.ts getSpectrumArrays` (the Browse tab) — and on a lattice archive saw the 0 that mzpeakts
  materialises for the NULL fallback column; the previous fix wave turned that into a throw
  (`assertNoGridAxis`). Both now decode each facet through a new engine export,
  `readFacetSignal` (`src/engine/spectrum.ts`) — the same per-facet resolver (`resolveFacetGridMz`)
  and ims-compact calibration the Spectra view uses, BigInt axes coerced — and only fall back to
  `assertNoGridAxis` when no resolver exists for the file at all; a facet whose rows need an axis
  its resolver cannot supply throws the new `UnresolvedGridAxisError` instead of returning zeros.
- **Viewer: the grid-axis rule is now per row on BOTH facets, independent of row order.** `mz`
  finite and > 0 wins, the axis is authoritative only where `mz` is the null fill (absent / null
  / 0). A fallback f64 spectrum inside a lattice facet arrives either with NO axis key at all
  (mzpeakts drops a column that is all-null within the selected rows — the most likely shape,
  as for the Int32 profile case) or, where a NULL Int64 cell is materialised, as `tof_index 0n`
  beside its real `mz`; either way it is read from `mz` and never reconstructed from the zero
  (the converter routes per spectrum, so the real archives are homogeneous per spectrum — blind
  / hek / d100: 0 mixed spectra — and the mixed shapes are defensive). `readCentroids` located
  the axis column from row 0 only, so a fallback row FIRST followed by a gridded row read the
  gridded row's null-fill 0 verbatim as m/z 0; the axis is now located from the first gridded
  row. `readDataArrays` previously let the axis win over any `m/z array` and, with no profile
  resolver, fell through to the zero-filled `m/z array` silently — both closed. Pre-lattice
  Shimadzu archives (`tof_calibration` only, plain f64 centroids) keep reading their centroids
  verbatim. Unit tests: `src/reader/arrays.test.ts` (synthetic readers with bigint axes, both row
  orders, absent-axis fallback); smoke on the real lattice + pre-lattice archives showed every
  sampled spectrum non-zero, ascending and value-equal to the engine on all entry points.
- **Viewer: the `mz-grid` codec multiplies by `1/scale` instead of dividing by `scale`.** The
  archive's Parquet-level contract is a multiplier (`point.tof_index` MS:1003824 with
  `transform_params [1e-9]`; the reference reader computes `s · k`, `vendor/mzpeak_prototyping/
  src/reader/point.rs`) and `k / 1e9` differs from `k · 1e-9` by 1 ulp on ~40 % of lattice
  values (100000123456 → 100.000123456 vs 100.00012345600001), so the viewer was 1 ulp off every
  other conformant reader — and off the converter's own `--to mzml` export — on the same archive.
  `1/1e9 === 1e-9` and `1/1e4 === 1e-4` exactly, so `axis * (1/scale)` is bit-identical to the
  transform (0 mismatches over 1e6 random lattice values). Round lattice values now read as the
  reference reader's value (5e11 → 500.00000000000006, not 500); ordinary display formatting hides
  it. Spec follow-up (not viewer work): `index-file.md` still lists only `tof_calibration` /
  `ims_calibration` / `vendor_files` / `vendor_metadata` — `mz_calibration` (`{codec: mz-grid,
  scale}`) and `vendor_mz_calibration` are undocumented, and `signal-data.md` §grid says the
  transform applies when the rank-0 axis is "not stored at all", whereas the lattice facet stores
  an all-NULL `mz` beside the axis (null-marking would tell a generic reader to interpolate).
  Needs: both blocks in the index table, and the rule "a null-filled rank-0 axis MAY sit beside an
  MS:1003824/MS:1003825 grid column; where the rank-0 value is NULL the grid column is
  authoritative, no null-marking interpolation".

- **The two timsTOF lanes disagreed on a selected ion's 1/K0 by up to 0.03 Vs/cm².** For the same
  dia-PASEF window (frame 2, m/z 1276.05 of 2485.d) ims-compact wrote 1.332429 and
  `--no-ims-compact` 1.317349. Both are the window's scan midpoint `(ScanNumBegin+ScanNumEnd)/2`
  (this file has no `Precursors` table); the difference is the calibration: the native lane
  evaluates the vendor ModelType-2 `TimsCalibration` (`src/tims_mobility.rs`, the model
  `timsdata`'s `tims_scannum_to_oneoverk0` implements, validated to ~1e-3 against the SDK), while
  mzdata 0.66.6 builds every 1/K0 it attaches as a *param* — the selected ion's MS:1002815, the
  scan-level midpoint and the frame's `ion mobility lower/upper limit` — from timsrust's LINEAR
  nominal-range interpolation (`io/tdf/reader.rs:1487,1527,1607-1641`), even though its own signal
  arrays use ModelType-2 (`io/tdf/arrays.rs:73`). The `--no-ims-compact` lane now inverts the
  (exactly invertible) linear map back to the scan position and re-evaluates the ModelType-2 model
  (`bruker_native::TdfMobilityRemap`, applied in `convert_file`); both lanes agree to 4e-16 on all
  15,977 windows. mzdata's spectrum-level `ion mobility lower/upper limit` spelling is kept: the
  pair is remapped AND put in order — mzdata emits it inverted (`lower` = convert(ScanNumBegin),
  `upper` = convert(ScanNumEnd), and 1/K0 falls with the scan index: 15,977/15,977 MS2 spectra of
  the v0.9.5 archive had `lower > upper`), so the archive no longer carries a negative-width
  spectrum-level window beside the correctly ordered selected-ion band. The selected ion
  additionally gets the MZP:1000006/7 band. `--no-tims-recalibration` is honoured on this lane
  too (the params then stay on timsrust's linear map, as in the ims-compact lane — the lanes agree
  either way; the mzdata arrays cannot be switched and the warning says so), and an unreadable
  `TimsCalibration` table degrades to the linear values with a warning instead of dropping the
  band (as in the native lane). Remaining, separate: the mzdata lane's mobility *arrays* use
  mzdata's own unanchored ModelType-2 form, ≤5e-4 Vs/cm² from `tims_mobility.rs` (1.467115 vs
  1.466959 at scan 0).
- `mzpeak-convert ARCHIVE -o x.mzML` (and the native-reader `--to mzml` paths) no longer panic on
  an MZP accession: `demote_mzp_params` strips the CV binding so the term is written as a
  `userParam` (name + value + unit), since mzdata's mzML writer unwraps `curie().to_string()`.

## [0.9.5] — 2026-09-02

### Added

- **timsTOF ims-compact archives now carry the vendor's exact TOF→m/z calibration.** Until now the
  archive held only `ims_calibration.a/b` — timsrust's two-point chord, through
  (`MzAcqRangeLower`, 0) and (`MzAcqRangeUpper`, `DigitizerNumSamples`) on most files but through a
  ±5 Th widened range on `Bruker otofControl` TDFs, so do not re-derive it from `GlobalMetadata`;
  speXtract measured it at −5…−11 ppm (m/z dependent) against Bruker's SDK — and the exact model was
  reachable
  only through the embedded `vendor/analysis.tdf.gz`; `--no-vendor` archives lost it entirely. Two
  additions, both lanes (native timsrust and `--bruker-sdk`), cost ≈13 numbers per calibration row
  plus 3 values per frame:
  - a `vendor_mz_calibration` index block: every `analysis.tdf` `MzCalibration` row **verbatim**
    (`Id, ModelType, DigitizerTimebase, DigitizerDelay, T1, T2, dC1, dC2, C0…C4` — all columns,
    as stored, so future schema columns ride along), the `GlobalMetadata` constants the chord is
    built from (`DigitizerNumSamples`, `MzAcqRangeLower/Upper`), the exact per-frame column names,
    and the ModelType-1 expression readers are expected to evaluate (speXtract v0.2.0, verified to
    2.5e-5 ppm against the Bruker SDK):
    `t_ns = tof·DigitizerTimebase + DigitizerDelay`, `C1_eff = C1·(1 + dC1·(T1 − T1_frame)/1e6)`,
    `t_ns = C0 + (1e6/√C1_eff)·√mz + C2·mz`, solved for √mz;
  - three per-frame `spectra_metadata` columns, `…_tdf_t1`, `…_tdf_t2`, `…_tdf_mz_calibration_id`
    (`Frames.T1`, `T2`, `MzCalibration`), because the model is temperature-compensated per frame
    (T1 drifts ~3 mK within PXD059079 2485.d) and a run may reference more than one calibration
    row. Nulls on a TDF whose `Frames` lacks the columns; the block is best-effort (a TDF without
    the table still converts).
  `ims_calibration` is unchanged and remains the reader contract; the exact model sits beside it.
  Verified on PXD059079 2485.d (ModelType 1, `C2 = 0`, 3,994 frames, one calibration row): rows
  and per-frame values bit-identical to `analysis.tdf`, present with `--no-vendor`; on this file the
  chord runs from +3.2 ppm (tof 0) to −4.2 ppm (top of range) against the exact model.

### Fixed

- **A spectrum with several precursors got all of its selected ions on the first one.** The
  precursor/selected-ion join key `(source_index, precursor_index)` is not unique for such a
  spectrum — dia-PASEF writes two precursors per MS2 frame with the same pair on each row — so the
  reader's scan matched the first precursor every time and its siblings came back empty. Two fixes:
  the precursor sort is stable (an unstable sort on the tied key also reordered the precursors
  against the ions they were being matched to), and where a spectrum's precursor and selected-ion
  counts agree, the ions are paired positionally in row order, which is the only reading the archive
  supports. Where the counts differ — one precursor with several ions (SPS-MS3), or ions missing —
  nothing is assumed and the previous behaviour stands. Round-tripping PXD059079 2485.d to mzML now
  gives exactly one ion on each of its 15,977 precursors (previously patterns like 0,0,0,0,5).
  Pinned by `tests/multi_precursor_roundtrip.rs`. Found by the speXtract S30 analysis; the
  underlying ambiguity is a spec matter — the key needs a per-spectrum precursor ordinal to be
  unique — and is unchanged here.

- **A failed peak-writer open no longer produces a silently wrong archive.** Both writers logged the
  failure and fell back to a default `(m/z f64, intensity f32)` point peak writer that cannot describe
  a chunked or grid facet: on diaPASEF that killed the parallel encoder mid-run (`--ims-chunked` on a
  TDF, before the layout-family check became a warning in 0.9.3), and on a DDA `.d` it exited 0 with
  all-null m/z and the real data in a 163 MB `auxiliary_arrays` blob. There is no correct archive on
  that path, so the writer now refuses to write one.

### Changed

- **Shimadzu native lane stores centroid m/z as an exact Int64 lattice.** The vendor's
  `MassHigh` is an Int64 at 1e-9 Da, so every centroid is `k = round(m/z · 1e9)` exactly — one
  delta-packed integer per peak instead of an f64 whose deltas are all distinct. `spectra_peaks`
  (point layout) now carries `point.tof_index` Int64 (`LinearMz`, `mzpeak:transform_params =
  "1e-9"`, dictionary off + DELTA_BINARY_PACKED via the writer's `*_index` rule), a Float64
  `point.mz` fallback that is NULL on lattice rows, and `point.intensity` Float32 as before; the
  facet is never chunked or numpressed. Every spectrum is checked on its own (`|m/z·1e9 − k| <
  max(1e-3, 8 ulp)` on every point — the relative term keeps the margin at the top of a wide
  Q-TOF range, where the coarse `Mass` path's double rounding approaches 1e-3 — and `k`
  non-decreasing) and one that fails keeps f64 m/z in the same facet — nothing is snapped,
  nothing is refused; a run in which no centroid list passes the guard is logged as a warning,
  and an empty scan counts as nothing-to-route, not as kept-f64. `--representation profile`
  declares neither the facet nor the block. `mzpeak_index.json` gains an `mz_calibration` block
  (`{"codec":"mz-grid","scale":1e9,"vendor":"shimadzu","lossless":"tof_index","applies_to":
  "spectra_peaks",…}`) beside the profile facet's `tof_calibration`; the vendored reader
  reconstructs m/z from the column metadata alone (`m/z = 1e-9 · k`), the viewer's existing
  `mz-grid` codec from the block. `MZPC_SHIMADZU_COARSE_MZ=1` (`Mass`, 1e-4 Da) lands on the same
  lattice as multiples of 1e5 and takes the same route. The profile facet (per-spectrum sqrt grid),
  precursors, scan windows, instrument configuration and the source SHA-1 are unchanged.
  Measured on the box, every spectrum of all four reference files on the lattice with zero f64
  fallbacks, and peak for peak identical to the f64 archives (ids, intensities, and the lattice
  reproduces each m/z exactly; DIA 20ng 279,686,550 peaks, 100ng 278,399,598): archive sizes
  Blind 5,239,837 → 3,794,397 B, HEK 28,959,271 → 23,914,635 B, DIA_Hela_20ng 2,187,984,551 →
  1,311,817,690 B (m/z column 1.90 GB → 1.03 GB; 553 MB at the old 1e-4 lattice),
  DIA_Hela_100ng 2,198,869,148 → 1,324,910,848 B. Writer: the vendored
  `MzPeakWriterType` gains `write_spectrum_with_peak_arrays`, which lets a profile spectrum hand
  its centroid list to the custom peaks schema as raw arrays — `write_spectrum` could only route
  a peak SET as f64 `CentroidPeak` columns. Pinned by unit tests in `src/shimadzu_grid.rs` and
  `tests/shimadzu_lattice_peaks.rs` (synthetic archive with the real two-facet shape — Int32
  sqrt-grid `tof_index` in `spectra_data` beside the Int64 lattice `tof_index` in
  `spectra_peaks` — through the vendored writer, read back with the vendored reader including
  the per-spectrum sqrt fixup, column encodings inspected with the parquet crate).

## [0.9.4] — 2026-09-02

### Fixed

- **0.9.3 did not compile on Windows.** `MZ_ARRAY` was imported for the `#[cfg(windows)]` Shimadzu
  grid path only, and the import had been dropped as unused on other hosts. No other change; 0.9.3
  archives (none were built) would have been identical.

## [0.9.3] — 2026-09-02

### Fixed

- **Numpress-linear data facets were written UNCOMPRESSED.** The all-null-twin prune backstop
  rewrote the finished peak facet with parquet's default `WriterProperties` (no ZSTD, no
  byte-stream-split, no delta packing, Parquet 1.0, encryption dropped) and a numpress facet always
  tripped it — its `mz_chunk_values` column is empty by design beside `mz_numpress_linear_bytes`
  — so `--zstd-level` had no effect on any numpress archive (DIA_Hela_20ng: ~380 MB of the mzML-lane
  archive). The rewrite now re-applies the facet's own properties, skips chunk facets altogether
  (nothing to prune, no 1 GB re-encode), and drops the pruned columns from `spectrum_array_index`
  instead of leaving an entry that pointed at a column that no longer existed. Pinned by
  `tests/data_facet_compression.rs`. Every numpress archive written since the backstop landed is
  affected; the corpus rebuild under this release covers them.

### Changed

- **Shimadzu native lane reads the vendor's high-resolution m/z.** Each vendor point carries a
  coarse `Mass` (Int32, 1e-4 Da lattice — what ProteoWizard reads) and `MassHigh` (Int64, 1e-9 Da)
  — and `MassHigh` is what LabSolutions' own mzML exporter writes (Blind_P1_pos_012: the native
  archive now matches the export to 5.7e-14 instead of 5.0e-5). The scale is established once per
  file — a power of ten fitted over ≥1,000 points with `|MassHigh − Mass×R| ≤ R/2` on every one,
  monotone within each list — and the whole file stays on `Mass` otherwise; precision is never
  mixed inside a file. `MZPC_SHIMADZU_COARSE_MZ=1` restores the old behaviour. Measured on three
  LCMS-9030 runs (R = 100,000; profile points exactly on the coarse grid, centroids carrying the
  sub-lattice digits an interpolated apex has). Physically this is ~100× below the instrument's
  mass accuracy (isotope-spacing residual 9.045 → 9.026 mDa); it is done for fidelity to the
  vendor's stated value, and it costs archive size — the 1e-4 lattice made delta encoding cheap.
- **Shimadzu profile facet stored as an exact sqrt grid.** With `MassHigh` the profile axis is a
  flight-time lattice — `sqrt(m/z) = c0 + c1·k`, `c1` constant across the run (spread 3e-17), `c0`
  per spectrum — that the vendor rounds to 1e-9. It is now stored as `tof_index` (Int32) with
  per-spectrum `tof_c0`/`tof_c1` (the same per-spectrum sqrt contract mzPeakViewer already
  reconstructs), verified on every point to ≤ 1e-9 before a spectrum is gridded; a spectrum that
  does not fit (LabSolutions clamps the first/last sample of some MS2 scans to the scan-window
  bound) keeps its f64 m/z beside it — nothing is snapped. Centroids stay f64 delta. Measured:
  Blind_P1_pos_012 13,200/13,200 spectra gridded, worst reconstruction error 7.4e-10; HEK_PosOAD1
  2,092 gridded + 9 f64 fallbacks (bit-exact), worst 5.8e-10; reader round-trip exact on both.
  Archive sizes (coarse `Mass` → `MassHigh` f64 → grid): Blind 3,540,575 → 5,456,550 → 5,241,787 B;
  HEK 33,439,279 → 72,164,027 → 28,961,221 B — the grid undoes the size cost of `MassHigh` on the
  profile facet (HEK ends 13 % below the old coarse archive) while keeping the vendor-exact values.
  Viewer contract: the grid axis is authoritative whenever `tof_index` is present and resolvable —
  a gridded row's null-filled `mz` column reads back as zeros through mzpeakts, and mzPeakViewer
  (`engine/spectrum.ts`) no longer treats that as an m/z array.
- **Mixed layout families per entity are accepted** (project decision, 2026-09-02 — a deliberate
  deviation from the spec text). The writer's one-family-per-entity check is a warning, not an
  error: a point-layout grid facet beside chunked centroids is the canonical case, `--ims-chunked`
  on timsTOF no longer aborts, and readers that resolve the layout per source read such archives
  correctly.
- **Lattice detection extended to the 1e-9 scale** (with a ulp-relative tolerance), so both
  `MassHigh` data and LabSolutions mzML exports take the lossless delta route instead of lossy
  numpress-linear. On the Blind export that is −13 % (4,247,050 → 3,708,846 B) AND lossless.

## [0.9.2] — 2026-09-02

### Fixed

- **Shimadzu: the source-file SHA-1 was missing on large `.lcd` inputs.** The digest ran after the
  vendor reader had opened the file, and `Shimadzu.LabSolutions.IO` holds a byte-range lock for as
  long as the file is open (`os error 33`); the 55–63 MB files slipped through, the 2.8 GB DIA runs
  did not. The digest is now taken before the reader opens the file — as msconvert does — and
  seeds the `sourceFile` entry, so MS:1000569 is present on every single-file Shimadzu input.

- **`scan_settings_list[*].targets` violated the mzPeak 0.9 schema.** Each target was written as a
  bare JSON list of params; the schema (`scan_settings_list.json`, definition `target`) requires an
  object `{"parameters": [...]}`, and mzPeakValidator's `meta_scan_settings_valid` rejected every
  archive converted from an mzML with a `<scanSettings>` target list (e.g. the tiny pwiz fixture).
  Present in 0.9.0 and 0.9.1. Targets are now written as objects; the reader still accepts the old
  bare-list form so archives written by earlier versions open unchanged.

## [0.9.1] — 2026-09-02

### Fixed

- **Data-point counters in the Parquet footers were wrong in both layouts.** A `.mzpeak` could
  declare `chromatogram_data_point_count = 0` while holding thousands of rows. Two independent
  causes: `PointBuffers`' inherent `add_arrays` shadows the `ArrayBufferWriter` impl that did the
  counting, so enum dispatch landed on the inherent one and nothing incremented; and five chunked
  call sites passed `chunks.len()` — the number of chunk ROWS — as the point count. Counting now
  happens in the inherent methods, and the chunk sites pass `n_pts`. Verified on the tiny pwiz
  fixture (chromatograms 0 → 6, matching the stored rows) and on a 13,200-spectrum run
  (chromatograms 0 → 26,400; spectra now 216,742 points rather than 59,948 chunks), and pinned by
  `tests/footer_counts.rs`, which fails on the pre-fix code.

- **Shimadzu native lane: warn when a `.lcd` stores no profile signal.**
  *[Superseded — see 0.9.9: everything below is true only of `Shimadzu.LabSolutions.IO` 3.8.4.6016.
  Version 5.0.0.0, shipped by a current ProteoWizard, reads these files correctly; the msconvert
  cross-check was driving the same old DLL, and the LabSolutions mzML export is NOT the remedy.]*
  For those spectra the
  vendor API returns centroid intensities rotated against the m/z axis
  (`[s alien values] + truth[0:n−s]`, s ∈ 1..7, and the final peak dropped), which poisons TIC, BPI
  and base-peak m/z while leaving the archive perfectly self-consistent. Measured scope, against
  the LabSolutions mzML exports: files that carry profile signal are unaffected —
  `Blind_P1_pos_012` compares 13,200/13,200 spectra with all 216,742 centroid intensities
  bit-exact — while the centroid-only DIA files are affected throughout.

  The peaks are stored exactly as the vendor interface returned them: this converter's job is to
  store vendor data in a new format, not to correct or second-guess it, and msconvert stores the
  same bytes silently. What changes is that the archive is no longer produced *quietly* — a
  one-shot warning names the defect and points at the LabSolutions mzML export, whose exporter
  takes a different internal path and is exact.

  **Root cause: a defect in `Shimadzu.LabSolutions.IO` itself, not in this converter.** The vendor
  API's own `CentroidList[i]` carries the right `Mass` beside the wrong `Intensity` — for DIA scan
  2, `Mass = 1002162` (m/z 100.2162, exactly the oracle's first peak) with `Intensity = 12455`
  where the oracle has 68 — and the alien leading values are the spectrum's own header scalars
  (`BPInt = 45640` appears verbatim as the second "intensity"). The vendor's `CentroidList.Count`
  is itself short by one, which is where the clipped final peak comes from. It is not reachable
  through any API lever: `profileDesired=0`, centroid-only fetch, centroid-before-profile ordering
  and two independent decodes all return identical rotated data, and **msconvert, reading the same
  DLL, produces byte-identical corrupt output** — so `--via-msconvert` is not a workaround for
  these files. Files that carry profile signal are unaffected through every path. The only correct
  source for a profile-less `.lcd` is a LabSolutions mzML export, whose exporter takes a different
  internal path and is exact.

- **Shimadzu spectra were all typed `MS:1000294 "mass spectrum"`.** That parent term was added
  unconditionally and, because mzdata's `spectrum_type()` is a first-match lookup, it shadowed the
  writer's inference — so no Shimadzu spectrum ever carried `MS:1000579`/`MS:1000580` the way the
  mzML lane does. Removed; the writer now infers from MS level.

- **Shimadzu FFI boundary hardening.** The glue now rejects a fetch whose m/z and intensity arrays
  differ in length (Rust reads both with one length), and no longer pins empty arrays — every
  zero-length fetch keyed its pin on `(handle, null)`, so the second one overwrote the first and
  leaked both `GCHandle`s. On the Rust side the pin guard is armed before the return-code check, so
  a failed call cannot strand a pin.

- **Shimadzu native lane: MSn spectra had no precursor at all, and the one precursor field that
  did cross the ABI was the wrong number.** The lane now emits a precursor per MSn spectrum with
  isolation window, CID activation with collision energy, and a `precursor_id` naming the parent
  scan — 1,837 precursors for 1,837 MS2 spectra on `HEK_PosOAD1`, where the native lane previously
  wrote zero. The m/z is taken from the vendor's own selection record on the same fixed-point scale
  as the m/z axis (`AcqModeMz` where the vendor sets it, validated by DIA against the mzML lane's
  452.5 isolation target; `PrecursorMzList` otherwise). What it is NOT taken from: the scalar
  `GetSpectrumInfo` returns, which on `HEK_PosOAD1` scan 18 is 2241279 — that spectrum's own
  **base peak mass**, not its precursor. Through the old 1e-9 multiplier that reached archives as
  m/z 0.0022; msconvert reads the same field and publishes it as 2.24e07. Neither value is inside
  the instrument's 70–1250 range; the corrected output spans 202–1249 (median 367).

- **Shimadzu ABI: a runtime version handshake, because mixed binary/DLL pairs failed silently.**
  Each side asserted only its own struct size and exports resolve by name, so a new binary against
  a stale `ShimadzuGlue.dll` read uninitialised tail bytes as metadata, and a stale binary against
  a new DLL took a 40-byte out-param overrun — and the box updater ships the executable without the
  DLL, so that pairing is a real deployment. `ShimadzuAbiVersion()` is now resolved optionally
  (absent ⇒ version 1) and a mismatch aborts with a message naming the cause; the widened metadata
  arrives through a separate `SpectrumMetaV2` entry point rather than behind the old name, and both
  sides assert the shared field offsets rather than just the totals.

- **Shimadzu native lane now carries the metadata the mzML lane had and it lacked.** Precursors
  on every MSn spectrum (isolation target with half-width offsets from `QTransmissionWidthMz`, the
  selected ion, CID with collision energy, and the parent-scan link — 1,837/1,837 on
  `HEK_PosOAD1`); scan windows on every scan from the acquisition event's configured range
  (`GetMassRawRange`: 70–1250 on HEK, 50–700 on Blind, with every observed m/z inside); an
  instrument configuration stating only what the API states — `SystemName()` as MS:1000031,
  ESI when the spectra say so, and the quadrupole + TOF analysers implied by `DeviceID = MSID_QTFL`
  (no detector, no serial: the API exposes neither); and the source file's SHA-1 (MS:1000569),
  the same digest msconvert records, so the two lanes agree on provenance byte-for-byte. The
  digest is added wherever the converter has to synthesise the `sourceFile` entry itself — every
  single-file vendor input, and an mzML that declares no `sourceFileList` of its own — never for
  a `.d` directory, which has no single byte stream to digest. The
  glue ABI moved to version 3 behind a runtime handshake, so a stale DLL beside a new binary is a
  clean load error rather than a silently mis-read struct. Along the way: the value the lane had
  been treating as the precursor m/z was the base-peak mass, mis-scaled — fixed.

### Added

- **`tools/compare_lcd_native_mzml.py`** — peak-for-peak comparison of an archive against the
  vendor mzML export of the same run: per-spectrum peak count, ordered m/z within tolerance,
  bit-exact intensities, and rotation detection. Exits 2 rather than passing when no oracle exists,
  and `--baseline` replays the known defect against a preserved pre-fix archive. This is the gate
  the Shimadzu lane is released against; the earlier ad-hoc check compared profile data only and
  was never committed.

## [0.9.0] — 2026-09-01

### Changed

- **TDF lossy path (`--no-ims-compact`): mobility now comes from mzdata's own ModelType-2
  calibration — adopted as the default, deliberately.** Before the mzdata 0.66 bump this path
  carried timsrust's linear 1/K0 approximation (documented error ~0.03 Vs·s/cm² against the vendor
  model); mzdata 0.66.3 implemented the true `TimsCalibrationModel2` and applies it unconditionally
  (`im_enabled` is hard-coded true in its `CalibrationParameters::from_sql`), so the lossy path's mobility axis shifted
  by ≤0.0318 toward vendor truth. Measured on `SBA415(1) Try`: m/z and intensity byte-identical,
  mobility residuals vs the exact vendor model 4.2e-4 → 1.3e-4 (median). This entry records that
  shift as the intended default, not an accident of the dependency bump.

  Consequences: `--no-tims-recalibration` is **inert** on this path (it governs only the converter's
  own ModelType-2 recalibration in the default ims-compact reader) — its help text now says so and
  combining the two flags warns instead of silently doing nothing. The default ims-compact path is
  untouched: its mobility is computed by `tims_mobility.rs`, validated against the Bruker SDK on 68
  datasets. Lossy-path TDF archives written before and after mzdata 0.66 differ in their mobility
  axis; the corpus default is ims-compact, so no corpus reconvert is triggered.

- **Box conversion is S3-first and self-updating.** `box_convert.sh` accepts `s3://` sources
  (handed to the box as a presigned GET — bytes already in the corpus bucket never round-trip
  through the host) and `s3://` targets (PUT straight to the final key, host mirrors down), and
  brings the box's converter to the newest release tag before any job runs
  (`BOX_AUTOUPDATE`/`BOX_REQUIRE_VERSION`). Fixed alongside: a failed upload no longer reports
  SUCCESS, a cross-bucket `s3://` target is no longer discarded, and the INT/TERM trap now cleans
  up minted relay keys.

### Dependencies

- **mzdata `=0.65.5` → `=0.66.6`** (mirrored in `vendor/mzpeak_prototyping`; the pins must move
  together), plus 108 registry crates via `cargo update`. mzdata 0.66.0 split into sub-crates and
  added `ArrayType::IndexArray`, fixed with an explicit match arm. **MSRV 1.87 → 1.88** (`time`
  0.3.55 / `serde_with` 3.22 via mzdata 0.66).

### Fixed

- **PASEF/IMS mzML aborted with "expected Float32 but found LargeList(Float32)".** The chunked
  schema sampler can only learn field types from spectra that contain data, and its five fixed
  probe positions all landed on empty ramp slots (~80% of a PASEF mzML is `defaultArrayLength="0"`),
  so `build_chunked` fell back to layout-blind point-shaped defaults inside a chunked buffer and the
  first real spectrum panicked the writer. The five-point sample is now a fast path with a lazy
  forward scan for the first spectra that actually carry data. Corpus completeness 191/199 →
  **199/199 (100%)**; point counts round-trip exactly (2073 → 2073, 682 → 682).

- **The `--representation` inert-flag warning never actually printed.** It was emitted before
  `init_logging`, so `log::warn!` hit the uninitialized default logger and was silently dropped —
  from the day the warning was added. Found because the new `--no-tims-recalibration` +
  `--no-ims-compact` warning placed beside it stayed silent too. Both now fire after logger init;
  verified all fire/no-fire combinations.

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
