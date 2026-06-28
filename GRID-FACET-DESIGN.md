# Generic grid facet — design proposal

Status: DRAFT v0.1, 2026-06-28. Addresses backlog **#11** (converge grid *storage*) and
**#12** (materialized low-res grids). **No code yet** — this is a proposal to align with the
upstream `mzpeak_prototyping` author (Josh) and the HUPO-PSI spec process *before*
implementing, so we don't ship a grid representation that diverges from what upstream lands.

## Why this exists

Index-interpolating grids (TOF→m/z, and any regularly-sampled axis) currently live in the
converter in **three inconsistent ad-hoc forms**. Josh's note confirms the *math* has
converged (SQRT transform + m/z deltas + polynomial recalibration ≈ our approach ≈ TOFEE),
but proposes a **generic grid *storage*** model we don't have:

> "I planned to store the grids in an additional Parquet file that would chunk the rows into
> segments of each grid, with grid values potentially stored in a list column. The entity
> index column would be the grid ID, and would allow for incremental decoding of the grid as
> needed. The grid could be defined in terms of an index space equation and coefficients, or
> with a precomputed list of coordinates ('materialized') that may be recalibrated with a
> similar model."

This proposal turns that into a concrete facet schema, a migration for each of our three
current forms, and an explicit set of questions to settle with upstream.

## Current state (what we'd replace)

| Source | Where grid lives today | Per-spectrum cost | Files |
|---|---|---|---|
| **SCIEX** (run-wide `(c0+c1·k)²`) | column transform metadata `mzpeak:transform_params` + index-JSON `tof_calibration` block | none (run-wide) | [main.rs:941](src/main.rs:941) |
| **Agilent** (per-scan `(tof_c0+tof_c1·k)²` + per-cal polynomial) | **per-spectrum columns** `tof_c0`/`tof_c1`/`tof_calibration_id` in `spectra_metadata` + index-JSON `calibrations` map | 2×f64 + 1×i64 / spectrum | [main.rs:1158](src/main.rs:1158), [main.rs:1203](src/main.rs:1203) |
| **Bruker** (run-wide `(a+b·tof)²`) | column transform + index-JSON `ims_calibration` block | none (run-wide) | [bruker_native.rs:149](src/bruker_native.rs:149) |

Problems: (1) three different shapes for the same concept; (2) Agilent puts grid coefficients
in `spectra_metadata`, the facet a reader must materialize wholesale on open (the same class
of cost the v0.3.1 intensity-bloat fix removed); (3) no first-class grid entity that many
entities can reference by ID; (4) JSON-in-index blocks are invisible to the columnar reader
and not incrementally decodable.

## Proposed facet: `grids.parquet`

A new facet registered in `mzpeak_index.json` `files[]` as
`{ "name": "grids.parquet", "entity_type": "grid", "data_kind": "grid" }`.

**One row per (grid, segment).** A grid may be a single row (parametric, or a small
materialized axis) or many rows (a large materialized axis chunked into segments for
incremental decode). Rows are sorted by `(grid_id, segment_index)` so a reader pulls only the
segment(s) covering the index range it needs via Parquet row-group + `grid_id` predicate
pushdown — Josh's "incremental decoding of the grid as needed."

```text
grid: struct {
  grid_id:         uint32,          // ENTITY INDEX — entities reference a grid by this id
  segment_index:   uint32,          // 0-based segment within this grid (0 for single-row grids)
  measure:         CURIE,           // what the axis measures: m/z MS:1000040, time UO:0000031,
                                    //   inverse reduced ion mobility MS:1002814, …
  kind:            string,          // "parametric" | "materialized"
  index_start:     int64,           // first index-space position this segment covers
  index_count:     int64,           // number of index positions in this segment

  // --- parametric (kind = "parametric"): value = f(index; coefficients) ---
  model:           CURIE,           // transform model, e.g. MZP:1000001 sqrt_mz_from_tof
  coefficients:    list<double>,    // model coefficients (see per-model layout below)

  // --- materialized (kind = "materialized"): value = coordinates[index - index_start] ---
  coordinates:     list<double>,    // precomputed coordinate values for this segment
  recal_model:     CURIE,           // OPTIONAL recalibration applied on top of coordinates
  recal_coefficients: list<double>, // …its coefficients (null when no recal)
}
```

**Referencing.** An entity (spectrum / chromatogram) carries a `grid_id` (`uint32`) column —
Agilent already emits the moral equivalent as `tof_calibration_id`. The entity stores only
its *index-space* values (e.g. `tof_index`, already in `spectra_peaks`); m/z (or time, or
1/K0) is reconstructed by applying the referenced grid's parametric model or materialized
lookup. The grid is stored **once** and shared by every entity that references it.

**Why a facet beats the status quo even at 1:1.** Even when every spectrum has a unique grid
(no dedup), moving coefficients out of `spectra_metadata` (materialized wholesale on open)
into `grids.parquet` (lazily, incrementally decoded) is the same reader-open win as the
v0.3.1 bloat fix — the per-open cost stops scaling with grid metadata.

### Per-model `coefficients` layout (parametric)

| `model` | reconstruction | `coefficients` |
|---|---|---|
| `sqrt_mz_from_tof` (SCIEX/Bruker) | `mz = (c0 + c1·k)²` | `[c0, c1]` |
| `agilent_sqrt_poly` | `t = base + (c0 + c1·k)/coeff ; mz = (coeff·(t−base))² − poly(clip(t,left,right))` | `[c0, c1, coeff, base, left, right, p0…p5, use_flags]` |

(The Agilent polynomial part that is shared per-calibration can either be inlined per grid row
or split into a second "calibration" grid the per-scan grid references — see open questions.)

## Worked migration #1 — Agilent per-spectrum dedup (first target)

Today ([main.rs:1158](src/main.rs:1158)): every profile spectrum carries `tof_c0`, `tof_c1`
(authoritative per-scan calibration) and `tof_calibration_id` (selects the shared polynomial
in the index `calibrations` map) as columns in `spectra_metadata`.

**After:** each distinct `(tof_c0, tof_c1, tof_calibration_id)` tuple → **one parametric grid
row** in `grids.parquet` (`model = agilent_sqrt_poly`, `coefficients` per the table above).
Each spectrum keeps a single `grid_id` reference; the three coefficient columns leave
`spectra_metadata` entirely.

- **Dedup:** MassHunter re-fits calibration only periodically, so long runs of scans share an
  identical `(c0, c1)` pair → they collapse to one grid row. Worst case (c0/c1 unique per
  scan) is 1:1, which still removes the columns from the open-path metadata facet.
- **Exactness preserved:** `tof_index` stays the native integer bin in `spectra_peaks`;
  reconstruction is the same quadratic + polynomial, so it remains lossless vs MassHunter
  (`max_roundtrip_ppm: 0.0`). This migration is **representation-only**.
- **Validation:** round-trip every spectrum's reconstructed m/z against the pre-migration
  output; assert bit-identical. Measure `spectra_metadata` shrink and grid-row count (dedup
  ratio) on the MTBLS1334 profile `.d`.

## Migrations — SCIEX and Bruker

- **SCIEX** (`sciex_sqrt`, run-wide): one `parametric` grid row, `model = sqrt_mz_from_tof`,
  `coefficients = [c0, c1]`, `grid_id = 0` referenced by all gridded spectra. Replaces the
  `mzpeak:transform_params` field metadata + index-JSON `tof_calibration` block.
- **Bruker** (`(a+b·tof)²`, run-wide): one `parametric` grid row, `coefficients = [a, b]`.
  Replaces the index-JSON `ims_calibration` block. (The integer `tof` column in
  `spectra_peaks` is unchanged — see the separate encoding note in backlog #13.)

After all three migrate, the index-JSON `tof_calibration` / `ims_calibration` blocks and the
per-spectrum coefficient columns are retired in favor of the one facet.

## #12 — materialized grids for time (NOT ion mobility)

Josh: a materialized grid "would work for any sufficiently low resolution measure like time
or ion mobility … a time grid would be convenient for storing many, many chromatographic
traces."

- **Time — yes.** A shared chromatographic time axis becomes one `materialized` grid
  (`measure = UO:0000031`, `coordinates = [t0, t1, …]`, optionally segment-chunked); many
  traces / XICs reference it by `grid_id` instead of each re-storing the axis. Natural home
  for SRM/MRM transition sets if we add them.
- **Ion mobility — no.** Backlog **#13** measured this on real timsTOF (SBA415, 3.0 M peaks):
  a materialized 1/K0 grid saves **~0%** vs the plain f64 column, because Parquet's
  `RLE_DICTIONARY` already *is* the materialized grid (718 distinct values; the dictionary
  page holds the axis, the data page holds the indices). 1/K0 stays an inline f64 column.
  The materialized form is justified only where the axis is *shared across many entities*
  (time/traces), not where it's one column Parquet already dictionaries.

## Index + CV

- `grids.parquet` registered in `files[]` (`entity_type: "grid"`). Entities gain a `grid_id`
  reference column; `tof_calibration_id` is the precedent.
- `model` / `measure` / `recal_model` are CURIEs. `sqrt_mz_from_tof` is already the
  converter-owned `MZP:1000001` (backlog #1); `agilent_sqrt_poly` and any
  `materialized`-recal model would need MZP terms (or PSI terms if upstreamed).

## Open questions for upstream (Josh)

1. **Facet name / entity_type** — `grids.parquet` + `entity_type: "grid"`, or fold into an
   existing facet? Does the spec's `entity_type` enum need a `grid` member?
2. **Segment row schema** — one row per (grid, segment) with `index_start`/`index_count`, or a
   different chunking key? Is `coefficients` as `list<double>` acceptable, or typed columns?
3. **Per-scan vs per-calibration split (Agilent)** — inline the shared polynomial in every
   per-scan grid row, or model it as a parent "calibration" grid the per-scan grid references
   (two-level grid graph)? The latter dedups the polynomial but adds a reference hop.
4. **Recalibration model** — how is `recal_model` applied to materialized `coordinates`
   (multiply/add/compose), and is it the same model family as the parametric grids?
5. **CV terms** — which of `agilent_sqrt_poly`, the materialized-recal model, and `measure`
   axes become PSI-MS vs converter-owned MZP terms?

## Non-goals / scope guards

- **No default-output change in this step.** When implemented, the facet lands behind an
  off-by-default flag (e.g. `--grid-facet`) until the schema is agreed with upstream, so we
  never ship a divergent default format (the forward-compat risk in #11).
- **No new math.** Reconstruction formulas are unchanged; this is storage only. Losslessness
  is asserted by round-trip against the current output.
- **Ion mobility stays inline** (per #13). This facet is for grids that are *referenced*, not
  for every regularly-sampled column.
