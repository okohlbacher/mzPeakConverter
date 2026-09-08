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
  Unreleased). Measured against fresh lane pairs: blank1 now carries the vendor serial, sample,
  MassHunter version and the STATED `-04:00` start time where pwiz's is shifted by the box's zone;
  Capan2 carries the model, sample, MassLynx version and its unzoned wall clock in
  `acquisition_time`; SWATH/Sample002/MRM-HR WIFFs carry model, serial, `SCIEX OS`/`Analyst TF`
  versions, sample and both digested members. **Still open, by lane:**
  - **Orphan MS2 (precursors):** Waters (Capan2: the 682 high-energy MSe scans, which pwiz expands into 136,400 drift-bin
    spectra with a placeholder precursor each; per-scan `SET_MASS` and collision energies live in `_FUNCnnn.STS`, a reverse-engineered layout the 2026-09-08 review refused to
    publish from before an SDK side-by-side on the box); SciEX (663k in the corpus; needs the
    `SpectrumMetaV2` glue export — S-P2); Agilent MHDAC (`MSScan.bin` precursor decode; no DDA/QQQ
    `.d` on host or in the corpus to verify against); BAF (SQL `Steps`/`Variables` tables; box-only).
    Bruker TDF/TSF and Shimadzu carry theirs.
  - **Waters ion mobility is not read natively:** the native lane writes the mobility-COMBINED scan
    (Capan2: 1,989 spectra of ~44k points) where pwiz expands each HDMSe scan into its 200 drift bins
    (397,800 spectra); `_FUNCnnn.CDT` is unread, so the drift dimension is lost on this lane. It also
    labels functions 3–6 MS2 (pwiz: MS1) and writes RT 0.0 everywhere (W-P2).
  - **Device chromatograms** (UV, pressure, temperature; B-P3): the mzML lane gets 620 traces on
    8 Agilent archives and more from pwiz's Waters/SciEX/Thermo readers; the native lanes write
    TIC/BPC only. Agilent `.cg`/`.cd` layouts unknown; Bruker `chromatography-data.sqlite` is
    readable on any host.
  - **Instrument components on non-Bruker lanes:** pwiz asserts hand-tabled sources and detectors
    per model; the native lanes state only what the file says (do-not-guess) — a decision, not a gap.
  - Shimadzu acquisition-software version (LabSolutions; needs a glue export); Waters per-function
    polarity / RT / scan windows (W-P2/W-P5: `_FUNCTNS.INF` + `_FUNCnnn.IDX`, same SDK cross-check);
    `src/agilent_midac.rs` scaffold deletion.
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
