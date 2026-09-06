# Backlog

**Since 2026-09-04 the issue list lives in the review ledger**, not here. This file is a pointer plus
the handful of items the ledger does not track. Decided by the owner in the 2026-09-04 interview:
*"ledger is truth; BACKLOG.md becomes a short pointer."*

- **Current issues, ranked, with evidence and status:** the *mzPeakConverter Review Ledger*
  (claude.ai artifact, §8 "Measures" carries a Status column: *done* / *open*). Its source is kept
  at `scratchpad/review2/review-ledger.html` in the maintainer's session; ask for the link.
- **What the ledger says is open** (2026-09-04 evening): Agilent native lane — restore the subprocess
  reader from `cc8245e`, time-boxed, delete as fallback; M6 grid-facet routing (gridded profile rows
  mislabelled centroid) — fix before the next corpus rerun; F5 — move `tof_c0`/`tof_c1` accessions
  from `MS:4000900/1` to `MZP:100000x` (same rebuild); precursors on the seven lanes that write
  orphan MS2 (SCIEX native, Waters, BAF, TSF, Agilent ×3) — first among vendor-API work; then Waters
  RT/polarity; then a shared .NET host for SCIEX/Agilent/MIDAC; collapse the six archive
  prologue/epilogue copies; shared constants instead of text pins (M28); per-member SHA-1 for
  directory inputs; the box harness stamps the *effective* recipe (native-first stays).
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
done; #9 was regressed by merge `5a62b90` and is the Agilent item above; #4, #8, #10–#12, #15, #22
are the deferred spec/research items listed here or superseded by the ledger.
