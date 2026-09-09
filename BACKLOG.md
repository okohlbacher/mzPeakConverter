# Backlog

**Since 2026-09-04 the issue list lives in the review ledger**, not here. This file is a pointer plus
the handful of items the ledger does not track. Decided by the owner in the 2026-09-04 interview:
*"ledger is truth; BACKLOG.md becomes a short pointer."*

- **Current issues, ranked, with evidence and status:** the *mzPeakConverter Review Ledger*
  (claude.ai artifact, §8 "Measures" carries a Status column: *done* / *open*). Its source is kept
  at `scratchpad/review2/review-ledger.html` in the maintainer's session; ask for the link.
- **What the ledger says is open** (2026-09-06, after 0.11.0 restored the Agilent lane and swept the
  Windows dead code): precursors on the seven lanes that write orphan MS2
  (SCIEX native, Waters, BAF, TSF, Agilent ×3) — first among vendor-API work; then Waters
  RT/polarity; then a shared .NET host for SCIEX/Agilent/MIDAC; collapse the six archive
  prologue/epilogue copies; shared constants instead of text pins (M28); per-member SHA-1 for
  directory inputs; the box harness stamps the *effective* recipe (native-first stays).
- **Run metadata on the native lanes — landed after 0.11.2** (`src/run_metadata.rs`,
  `agilent_meta.rs`, `waters_meta.rs`, Bruker `GlobalMetadata`, SciEX `RunInfo`; CHANGELOG
  Unreleased). Measured against fresh lane pairs: blank1 now carries the vendor serial, sample and
  MassHunter version; Capan2 the model, sample and MassLynx version; SWATH/Sample002/MRM-HR WIFFs
  model, serial, `SCIEX OS`/`Analyst TF` versions, sample and both digested members. Acquisition
  clocks are tracked in their own item below. **Still open, by lane:**
  - **Orphan MS2 (precursors):** Waters (Capan2: the 682 high-energy MSe scans, which pwiz expands into 136,400 drift-bin
    spectra with a placeholder precursor each; the SDK route is `getScanItemValue(SET_MASS / COLLISION_ENERGY)`, blocked on the
    crashing `getScanItemsInFunction` — see the ion-mobility item; the `_FUNCnnn.STS` layout stays off limits); SciEX (663k in the corpus; needs the
    `SpectrumMetaV2` glue export — S-P2); Agilent MHDAC (`MSScan.bin` precursor decode; no DDA/QQQ
    `.d` on host or in the corpus to verify against); BAF (SQL `Steps`/`Variables` tables; box-only).
    Bruker TDF/TSF and Shimadzu carry theirs.
  - **Waters ion mobility — landed 2026-09-09:** HDMSe/HDDDA functions are read bin by bin and written as
    frames (one spectrum per MassLynx scan, per-point `raw_ion_mobility` in ms, sorted by m/z); RT,
    polarity, scan window and function-type MS levels come from the SDK (W-P2 closed for those fields).
    Verified bin-for-bin against pwiz on ten Capan2 frames. Remaining on this lane: precursors
    (`getScanItemsInFunction` crashes in every spelling tried; SET_MASS / COLLISION_ENERGY unread — the
    MSe rule therefore assumes the second MS function is the elevated-energy one), lock-mass function
    detection (`getLockMassFunction` unbound), SONAR (bins are quadrupole positions; the frame writer
    would mislabel them — refuse or skip until a SONAR file is available), CCS per peak
    (`getCollisionalCrossSection` needs a charge). Viewer: mzPeakViewer keys its mobility panel on the
    `ims_calibration` block and the 1/K0 array name; it needs to recognise `raw_ion_mobility` (ms)
    and the `waters_drift` block.
  - **Device chromatograms** (UV, pressure, temperature; B-P3): the mzML lane gets 620 traces on
    8 Agilent archives and more from pwiz's Waters/SciEX/Thermo readers; the native lanes write
    TIC/BPC only. Agilent `.cg`/`.cd` layouts unknown; Bruker `chromatography-data.sqlite` is
    readable on any host.
  - **Instrument components on non-Bruker lanes:** pwiz asserts hand-tabled sources and detectors
    per model; the native lanes state only what the file says (do-not-guess) — a decision, not a gap.
  - Shimadzu acquisition-software version (LabSolutions; needs a glue export); Waters per-function
    polarity / RT / scan windows (W-P2/W-P5: `_FUNCTNS.INF` + `_FUNCnnn.IDX`, same SDK cross-check);
    `src/agilent_midac.rs` scaffold deletion.
- **Acquisition clocks — every open point in one place (2026-09-09).** `run.start_time` is an RFC 3339
  instant and RFC 3339 cannot say "zone unknown", so the converter's rule (branch
  `feat/native-run-metadata`, `src/run_metadata.rs`) is: a vendor time that STATES its UTC offset is
  written verbatim; a wall clock WITHOUT a zone leaves `run.start_time` null and is preserved verbatim in
  `metadata.acquisition_time = {wall_clock, zone: "unstated", source, note}`. Nothing is shifted by the
  converting host's zone. Measured per vendor on the six lane pairs (native vs ProteoWizard 3.0.26175):
  | vendor (pair) | what the file states | native lane | ProteoWizard writes |
  |---|---|---|---|
  | Agilent `.d` (blank1) | `Contents.xml` `AcquiredTime` `2022-11-01T13:11:27.729717400-04:00` (= 17:11:27Z) | `run.start_time` verbatim | `2022-11-01T18:11:27Z` — off by +1 h from the stated instant; mechanism not established (not the box's offset either: CET would give 16:11Z or 12:11Z) |
  | Bruker TDF/TSF | `GlobalMetadata.AcquisitionDateTime` with offset | verbatim | verbatim (agrees) |
  | Waters `.raw` (Capan2) | `_HEADER.TXT` `Acquired Date/Time` `03-Dec-2018 22:39:33`, no zone | null + block `22:39:33` | `2018-12-03T22:39:33Z` — labels the wall clock UTC |
  | SciEX `.wiff` (SWATH, Sample002, MRM_03) | Clearcore2 `AcquisitionDateTime`, `Kind = Unspecified` | null + block (`09:52:14`, `19:58:37`, `02:56:30`) | wall clock − 2 h on all three (`07:52:14Z`, `17:58:37Z`, `00:56:30Z`) although the runs are from November, February and August — the box's CURRENT offset, not the acquisition date's |
  | Shimadzu `.lcd` (Blind) | `File Property` stream: `SampleInfo.DateTime` = UTC FILETIME `2024-02-15T10:47:18.756Z` beside `szLocGMTDiffGenDateTime = +01'00'` (measured: OLE2 directory FILETIMEs and the MS-CAB local stamps in the same file agree) | `2024-02-15T11:47:18.756+01:00` — read from the file on any host (`src/shimadzu_meta.rs`; the DLL's `SampleInfo` is empty in the .NET 8 host) | `2024-02-15T08:47:18Z` — the DLL's UTC value minus the box's CURRENT offset (+2 h in September): `ShimadzuReader.cpp::getAnalysisDate` adds `universal_time() − local_time()` evaluated at conversion time (`adjustUnknownTimeZonesToHostTimeZone`) |
  | Thermo `.raw` | mzdata's reader, zoned | verbatim | verbatim (agrees) |
  The SciEX and Shimadzu shifts are the same ProteoWizard mechanism — the host's offset at CONVERSION
  time applied to a value that already is UTC (Shimadzu) or that the reader treats as unzoned (SciEX);
  the Agilent +1 h is not yet pinned to it. Open, in order: (1) **Shimadzu — resolved 2026-09-09** by
  reading the file (option c); the DLL-side emptiness is still being analysed (Codex's leading
  hypothesis: the 1252 code page has no decoder in .NET 8 unless the provider is registered — the glue
  now registers it; on the next Blind run the debug dump that fires on an empty date no longer fired). (2) **Consumer
  guidance + validator rule:** readers must fall back to `acquisition_time.wall_clock` when
  `run.start_time` is null; the validator should flag a null `run.start_time` WITHOUT the block on a
  vendor-derived archive, and never flag the block itself (handoff to mzPeakValidator pending). (3)
  **Decision recorded, revisit only on request:** we do NOT assert a zone for an unzoned wall clock for
  consumer compatibility — pwiz's `Z`/host-offset labels are exactly the false precision the rule avoids.
  (4) **Upstream:** the Agilent +1 h and the SciEX current-offset shifts are ProteoWizard behaviours worth
  a report once the mechanism is pinned (pwiz's `Reader_Agilent` / `Reader_ABI` date handling; not
  reproduced by us). (5) Agilent `Contents.xml` without `AcquiredTime` and Waters headers without
  `Acquired Date` are handled (nothing recorded); a vendor time that fails to parse is logged at WARN
  with the raw text and dropped — pinned by `run_metadata::tests::parse_vendor_time`.
- **SciEX MRM/SIM dwell runs — refused after 0.11.2** (`refuse_if_unsupported`; the box harness
  routes the refusal to `--via-msconvert`). `En_PPY.wiff` and `IPX0002633001_D-239.wiff` will be
  rebuilt on the msconvert lane at the next corpus rerun (154,520 / 2,215 one-point "spectra" → 4 /
  95 SRM chromatograms). Reading the transitions natively (Q1/Q3, compound, CE, RT window — S-P4)
  stays open; MRM-HR scan runs convert natively as before.
- **Not in the ledger — Agilent native lane follow-ups (0.11.0, 2026-09-06):** `--tof-grid` on the
  MHDAC lane (the Q-TOF profile points sit on the flight-time lattice — the msconvert+`--tof-grid`
  build of the same run is 200 MB against 245 MB numpress-chunked f64; a SciEX-style per-run fit
  would close that); MRM/SIM transition chromatograms through MHDAC (today refused → msconvert);
  `agilent_midac` is still the in-process net8 design MHDAC cannot run under and has never opened
  a file — port to the net48 host or delete; the temp-file materialisation (16 B/point, whole run)
  could stream, and the inspect path (no `-o`) pays it in full just to print a scan count (a host
  `--count` mode would fix both); the instrument serial number (msconvert records it; the MHDAC member
  the host tries is not it); no timeout or kill-on-parent-death for the host process (a killed converter
  orphans it); per-record scan types in the protocol so a mixed Scan+MRM method can drop the dwell rows
  instead of storing them as one-point spectra (today: a warning).
- **Not in the ledger — surfaced by the 0.10.2 corpus rebuild (2026-09-06):** a chunk-capable
  integer axis. M6 put gridded profile spectra into `spectra_data`, which therefore has to be point
  layout, so a native SCIEX run's off-lattice profile minority is now stored as exact f64 points
  instead of numpress chunks: 9.2 % of the points on MSV000093587 Sample002 (+27 % archive), 3.2 %
  on PXD011326 (+12 %), 1–3 % on three more SWATH runs. A `tof_index` list column beside the chunk
  encoding (or a per-facet mixed layout) would recover it. Owner's call: fidelity vs size.
- **Not in the ledger — spec and CV items, all deferred by decision:**
  - PSI-MS term request for the per-window ion-mobility band (currently `MZP:1000006/1000007`).
    *Decision: keep MZP, do not file for now.*
  - Spec proposal: a per-spectrum precursor ordinal column in both precursor facets, so the join
    key is unique for multi-precursor spectra. *Decision: propose it; keep reader-side positional
    pairing meanwhile.*
  - From the speXtract S30 handoff, spec asks not converter work: a TIMS scan→1/K0 grid term
    (signal-data.md §grid transforms); a quantified request for per-(frame, scan) chunking (ruled
    out by chunked-layout.md:62–63; would give ≈ −6 % vs the vendor file); whether the mixed
    layout-family deviation (conformance.md item 4) should be raised with PSI.
  - Upstream mzdata: graceful decode in the sourceFile handler (old #8).
- **Windows-only dead code surfaced 2026-09-04** by switching the six vendor modules to the
  `cfg_attr(not(windows), allow(dead_code))` form: `library_path`, `calibration_used`,
  `spectrum_meta`, `analysis_date`, `lcd_path`, `sample_arrays`, and four `is_empty` methods are
  never read on the only host that compiles them. Sweep on the box.

## History

Everything this file used to contain — 23 numbered items with their analyses, measurements and
resolutions (grid CV terms, the generic grid facet, timsTOF mobility grids, the `tof` column
encoding, timsrust 5.1.x decompression, the performance section, the Agilent hosting mismatch) —
is preserved in git: `git show v0.9.12:BACKLOG.md`. Of those, #1–#3, #5–#7, #13–#14, #16–#21 were
done; #9 was regressed by merge `5a62b90` and restored in 0.11.0; #4, #8, #10–#12, #15, #22
are the deferred spec/research items listed here or superseded by the ledger.
