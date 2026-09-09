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
- **Not in the ledger — temporary `[patch.crates-io]` on mzdata (2026-09-09).** The mzML reader's
  isolation-window fix ([mobiusklein/mzdata#58](https://github.com/mobiusklein/mzdata/pull/58)) is
  pinned from our fork at 0.66.6. When upstream releases it: bump the `mzdata` pin, delete the patch
  block in `Cargo.toml`, keep `tests/isolation_window_offset_order.rs`. Until then every fresh
  `cargo build` fetches the fork over git (the box included). Which published mzML-lane archives
  carry a zero lower offset has not been swept; the Waters MSe/HDMSe twins certainly do.
- **Not in the ledger — the native lanes lose vendor metadata the mzML lane carries (measured
  2026-09-07 by `tests/lane_metadata_parity.rs`).** The two lanes take metadata from different
  places: the mzML lane inherits ProteoWizard's finished model via `copy_metadata_from`, the native
  lanes build an archive from the per-spectrum descriptions plus `VendorHints`, so every field must
  be plumbed by hand. Measured on four pairs (Shimadzu `.lcd`, Agilent GC-MS `.d`, 2× SciEX `.wiff`),
  the native side does not carry: the **sample list and the sample name in `run.id`**; the
  **acquisition start time**; the **instrument serial (MS:1000529)**, the vendor model term and the
  **instrument components** (only the Shimadzu lane builds components); the **per-member source
  files and their MS:1000569 checksums** (one synthesised entry instead of 2–24 real ones); the
  **acquisition software version**; the specific **`file_description.contents`** terms; and the
  **non-MS device chromatograms**. Cheapest first: the Agilent serial and model are plain XML in
  `AcqData/Devices.xml`, and per-member hashing of a directory input is the existing backlog item.
- **Not in the ledger — the SciEX native lane stores MRM/SIM dwells as one-point spectra
  (2026-09-07).** The same defect class Agilent 0.11.0 fixed by refusing: `En_PPY.wiff` and
  `IPX0002633001_D-239.wiff` are MRM acquisitions whose PUBLISHED corpus archives are native builds
  with 154,520 and 2,215 one-point "spectra", 2 chromatograms and **zero transition identity**;
  msconvert produces 4 and 95 SRM chromatograms carrying compound name, Q1/Q3, collision energy and
  RT window. SciEX has no `is_dwell_only` equivalent, so nothing routes those runs to msconvert.
  Decide as for Agilent: refuse and fall back, or read the transitions natively.
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
