# mzPeak spec-compliance backlog

Deferred items from the spec cross-check (see also the spec PRs in
`~/Claude/mzPeak-specification`: signal-data grid transforms + index-file vendor blocks).

Status legend: 🔴 blocker · 🟡 deferred · ✅ done this round.

---

## #1 — Grid encoding CV terms ✅ (adopted Josh's seeded PSI-MS terms)

**Done.** Josh Klein had already seeded a coherent "coordinate spacing model" tree in PSI-MS
for grid encoding, which covers both our transforms exactly. The converter now emits the
**assigned** terms (no more `MS:1003903/1003904` placeholders, no more `MZP` for transforms):

| We emit (write) | PSI-MS term | Maps our | Reconstruction |
|---|---|---|---|
| `MS:1003825` | square root grid interpolation | sqrt-from-TOF | `mz = f((b + a·k)²)`, `f`=identity |
| `MS:1003824` | linear grid interpolation | linear m/z | `mz = f(b + a·k)`, `f`=identity |
| `MS:1003826` | coordinate grid encoding | grid-index marker | array stores indices; value = grid id |

Parent tree: `MS:1003820` coordinate spacing model → `MS:1003822` grid coordinate interpolation
→ {`MS:1003824` linear, `MS:1003825` sqrt}. Legacy codings (`MS:1003903/1003904`, `MZP:1000001/2`)
are still recognized on READ (`buffer_descriptors::from_curie`) so older archives decode.

**Are any MZP terms still needed? No.** The coefficients (`tof_c0`/`tof_c1`/`tof_calibration_id`)
are **values**, not CV terms — in PSI's model they ride in the spacing model's value list. We keep
them as per-spectrum columns (Agilent drift) tagged with a converter-owned `MZP:` accession purely
as a column-naming artifact; this can move to the value-list / plain names to drop `MZP` entirely.

### Two PSI terms still to request (NOT MZP — these are real gaps for full fidelity)

1. **recalibration function** — the non-identity `f` referenced by `MS:1003822`/`1003824`/`1003825`
   but not yet instantiated. Needed for Agilent's per-CalibrationID polynomial refinement
   (`mz = (c0+c1·k)² − poly(...)`). The reserved `MS:1003823`/`1003827` slots look intended for this.
2. **polynomial / mobility grid model** — a spacing model for the **TIMS ion-mobility axis**
   (scan index → 1/K0), which is nonlinear (Bruker TimsCalibration ModelType 2) and fits neither
   linear nor sqrt. Lets us encode the mobility grid exactly instead of timsrust's ~0.038 linear approx.

Definition sites:
- `vendor/mzpeak_prototyping/src/buffer_descriptors.rs` — `SQRT_MZ_FROM_TOF`(=MS:1003825),
  `LINEAR_MZ`(=MS:1003824), `COORD_GRID_ENCODING`(=MS:1003826) + legacy-read consts
- `src/main.rs` — `TOF_C0_CURIE` / `TOF_C1_CURIE` / `TOF_CALID_CURIE` (coefficient value columns)

## #2 — `cv_list` must declare the grid CV ✅ (done)

The writer's `cv_list` now declares `MZP` (full name + `cv/mzpeak.obo` URI + version) whenever a
grid path runs, via the `From<ControlledVocabulary::Unknown>` → MZP entry pushed by
`register_mzp_cv()`. Satisfies the validator's `cv_list_consistency` rule (every used CV declared).

## #4 — Open the PSI-MS CV term requests 🟡 (the only true remainder of #1)

File the five term requests from the table above against HUPO-PSI/psi-ms-CV. External
process; long latency — kick off early. Unblocks dropping the provisional namespace.

## #5 — Enforce the index calibration/vendor blocks in the JSON schema ✅ (done)

`tof_calibration` / `ims_calibration` / `vendor_files` / `vendor_metadata` are now typed
optional `metadata` properties (`additionalProperties` kept true) in both the canonical
spec schema (`mzPeak-specification/schema/mzpeak_index.json`) and the validator's bundled
copy (`mzpeak-0.9/schema/json/mzpeak_index.json`), so the validator shape-checks them
(`codec` const, `vendor_files[].action` enum, etc.).

## #6 — Validator MZP-awareness + coverage 🟢 (core done; coverage optional)

**MZP-awareness ✅** — `cv/mzpeak.obo` added to the validator profile
(`~/Claude/mzPeakValidator/mzpeak_validator/profiles/mzpeak-0.9/cv/`) + a `{"id":"MZP",…,"role":"cv"}`
artifact in `profile.json`. `cv_inflection` now accepts the `MZP_*` columns and the `MZP:1000001`
transform; archives validate **PASS**. See `~/Claude/mzPeakValidator/HANDOFF-mzp-cv.md`.

**Anchor bug ✅** — `#definitions/cv` → `#/definitions/cv` fixed in the validator's bundled
`cv_list.json` and the spec's `cv_list.json` (the build/lib copy is regenerated, left alone).

**Coverage extension 🟡 (optional hardening, not started):** add rules asserting (a) every
`transform` CURIE resolves against a declared `cv_list` CV; (b) `entity_type` / `data_kind` are in
the controlled sets; (c) a `transform` that needs params carries `mzpeak:transform_params` or
`…_per_spectrum`. Each is a net-new `p_*` primitive + rule entry + fixture in
`~/Claude/mzPeakValidator`. Makes #1–#3 regressions impossible to reintroduce silently.

## #7 — Centroid → naive-encoding fallback 🟢 (root cause fixed; #2 hardening optional)

**Root cause fixed (`142b9ac`).** The drain panic was not really a centroid-encoding gap — it was a
flag bug in the *packed* `MzPeakWriterType`: it built the chromatogram buffer's schema off
`use_chunked_encoding` (the **spectrum** flag) while `write_chromatogram_arrays` branches on
`use_chromatogram_chunked_encoding` (the **chromatogram** flag). Whenever they disagreed —
`convert_sciex_grid`: point spectra + chunked-chromatogram flag — construction was point but the
write was chunked, panicking `drain()` into a 0-byte archive once a real TIC was written. Now both
sides key on the chromatogram flag (the split writer already did), so sub-item 3 is closed and the
`6ae92d1` point-only pin was dropped — SCIEX-grid chromatograms are chunked + consistent again.
Verified: MSV000090136 (the original crash) reconverts clean.

Sub-item 2 below is still worth doing as defense-in-depth but is no longer urgent:

(original framing kept for the remaining optional hardening —)

The SCIEX-grid chromatogram panic fixed point-wise in `6ae92d1` was one instance of a general
fragility: a facet whose **write** path is chunked/grid while its Arrow **schema** was built point
(or vice versa) blows up at `drain()` — `RecordBatch::try_new` fails "number of columns(N) must
match number of fields(M)" and the writer `panic!`s, producing a **0-byte archive** with no clean
error. It only fires when real data lands in that facet (the bug hid until a SWATH run carried MS1
spectra → a synthesized TIC was written), so it passes most fixtures and dies on production data.

Grid encodings (sqrt/linear `tof_index`, `MS:1003824/1003825`) are valid **only** for profile data
on a regular flight-time/m-z lattice. Centroid peak lists and small auxiliary facets (chromatograms)
do not fit and must use naive point `(m/z, intensity)` encoding. Today that fallback is implicit and
per-path (the grid path special-cases empty spectra to the data facet; chromatograms are now
hard-pinned point), so the next path that enables chunked write without a matching schema reopens it.

What to build:
1. ~~One detection point — centroid/won't-grid ⇒ naive point encoding.~~ **Moot** — the panic was a
   writer flag bug, not a routing gap; the existing profile/centroid/empty routing was already correct.
2. **Fail loud, not fatal** 🟡 (optional, still open) — `drain()`'s `unwrap_or_else(|e| panic!(…))`
   (`vendor/mzpeak_prototyping/src/writer/array_buffer.rs:437`) should `bail!` with the facet +
   column names so any *future* schema/data mismatch is a recoverable CONV-ERR, not a panic. Lower
   priority now that the one known trigger is gone; the trait returns `impl Iterator<Item=RecordBatch>`
   so this needs threading `Result` through ~10 call sites.
3. ~~Self-consistent flag.~~ **Done (`142b9ac`)** — packed writer now builds the chromatogram buffer
   off `use_chromatogram_chunked_encoding`; the `convert_sciex_grid` point-only pin was dropped.

## #8 — Upstream mzdata: graceful decode in the sourceFile handler 🟡 (report upstream)

We work around it converter-side (`transcode_legacy_encoding`, `1cf067c`/`4bd8eb7`), but the real bug
is in mzdata 0.65.2 `io/mzml/reading_shared.rs:649`: the `sourceFile` `id`/`name`/`location`
attributes are decoded with `attr.unescape_value().expect("Error decoding …")` — a UTF-8 decode that
**panics** — while every other attribute in the same file uses the Latin-1-aware `decode_latin1_escape`
(lines 191/199/234/284/296/333/500). So one Latin-1 byte in a `sourceFile` attribute panics the whole
reader (DESI imzML declared ISO-8859-1, `<sourceFile name="à">`). Three graceful options, in order:

1. **Consistency one-liner** — use the decoder already in the file:
   `source_file.name = decode_latin1_escape(&attr.value).to_string();` (same for id/location). Never
   panics, matches the rest of the codebase.
2. **No-panic principle** — a parser must never `.expect()` on external input. Audit the mzML/imzML
   readers for decode `.expect(`/`unwrap(` and route them through the existing `handle_xml_error`, or
   `…unwrap_or_else(|_| decode_latin1_escape(&attr.value))`.
3. **Honor the declaration** — quick-xml's `encoding` feature auto-transcodes a declared
   `encoding="ISO-8859-1"`/`Windows-1252`/UTF-16; enabling it makes legacy files Just Work and retires
   the per-attribute Latin-1 dance (and our converter-side transcode) entirely.

Once (1) ships upstream and the pin moves, drop `transcode_legacy_encoding`.

---

## #9 — Agilent `.d`: MHDAC needs .NET Framework, not .NET 8 ✅ (re-hosted out-of-process)

**Done.** MHDAC cannot run in an in-process .NET 8 runtime: `MassSpecDataReader.OpenDataFile`
internally calls `Delegate.BeginInvoke` (`DataFileMgr.ReadNonMSInfoDelegate`), permanently
unsupported on .NET Core/5+ (`PlatformNotSupportedException`, no opt-in flag). Re-hosted as an
**out-of-process .NET Framework 4.8 console EXE** `AgilentGlueHost.exe` (`glue/agilent/`, retargeted
net8 lib → net48 exe via `Microsoft.NETFramework.ReferenceAssemblies` so it still cross-builds with
the `dotnet` SDK). `src/agilent.rs` now spawns it per `.d` and reads back a little-endian binary file
(replacing the `netcorehost`/UnmanagedCallersOnly FFI). On the box: `box_convert_remote.ps1` points
`MZPC_AGILENT_GLUE` at `bin/Release/net48`, and a `pwiz-bin/vendor_api/Agilent → pwiz-bin` junction
supplies the flattened MHDAC DLLs.

Verified: MHDAC now executes under .NET FW (the `BeginInvoke` wall is gone) and returns real vendor
verdicts. Reflection resolution validated against MHDAC `10.0.1.10305` (types across
`MassSpecDataReader`/`BaseDataAccess`/`BaseCommon`; reader API via the `IMsdrDataReader` interface).

**Corpus note (separate annotation issue):** `Alexander_023_B_30x_pos_121820_136.d` carries **no MS
data** — its `AcqData` holds only LC device traces (`BinPump1.cg` pump pressure, `HiP-ALS1.cg`
autosampler), no `MSScan/MSPeak/MSProfile`. MHDAC correctly reports "does not contain any data"; it
should be excluded from MS conversion, not recorded as a failure. The only MS-bearing Agilent `.d` in
the corpus is the profile `LMVCS24HC.d` (host `--agilent-grid` path). Full design: `glue/agilent/README.md`.

---

## Done ✅

- **#5** — the four index blocks are typed optional schema properties in the spec + validator
  copies; the validator now shape-checks them.
- **#6 (core)** — validator is MZP-aware (CV profile loads `cv/mzpeak.obo`) and the `#definitions/cv`
  anchor bug is fixed; corpus archives validate PASS. Handoff: `~/Claude/mzPeakValidator/HANDOFF-mzp-cv.md`.
- **#1** — grid CV de-squatted from `MS:` to the converter-owned `MZP` CV (`cv/mzpeak.obo`);
  wire form now `MZP:1000001` / `MZP_1000003_tof_c0`. Verified on the Agilent grid corpus
  dataset (cv_list = `[MS, UO, MZP]`, transform = `MZP:1000001`, no `MS:` leak).
- **#2** — `cv_list` declares `MZP` when a grid path runs (`register_mzp_cv` + the
  `From<Unknown>` → MZP entry); passes the validator's `cv_list_consistency` rule.
- **#3** — embedded vendor side-files now use the controlled `entity_type: other`
  (was the non-conformant `"vendor"`); `data_kind: proprietary` + the `vendor/` path
  prefix still mark them vendor-private. `src/vendor.rs`; spec examples updated to match.

---

## #10 — Adversarial-review follow-ups (deferred from v0.3.0) 🟡

Lower-severity hardening surfaced by the v0.3.0 release review; none affect the
example corpus (which validates 0/0). Fix opportunistically.

**Box tooling (`tools/`):**
- `box_convert_scp.sh` word-splits `src out` pairs and `$members` on whitespace, so it
  can't handle paths with spaces/parens (the corpus has `SBA415(1) Try_…`). The SCP
  path is a fallback (URL-pull supersedes it); to lift the limitation, feed NUL/tab-
  delimited records + bash arrays like `box_convert.sh` does. Header already documents it.
- No trap/`finally` on the SCP path → orphaned `scpconv/<uid>` box temp dirs on kill.
  Mirror `box_convert.sh`'s `trap cleanup EXIT INT TERM` + self-cleaning box-side temp.

**Agilent host (`glue/agilent/Glue.cs`):**
- Missing-RT is silently 0.0 (indistinguishable from a real RT 0); use a NaN sentinel so
  the Rust side can tell "no RT" from "RT 0". Also assert `GetScanRecord`/`GetSpectrum`
  share a row-index space.
- No endianness sentinel — the LE assumption is correct on x64 Windows but unguarded;
  a u64 magic-2 after `AGL1` would catch a future BE/ARM host.
- `MapMsLevel` demotes MS3+/MSn to MS1 (fine for QTOF; wrong for deeper-level instruments).
- X/Y length mismatch is silently clamped to the shorter array rather than flagged.

**Metadata (`vendor/mzpeak_prototyping/src/param.rs`):** `ensure_cv_term_if_bare` keys on
"no accession at all", not "no accession from the rule's branch" — an entry carrying an
unrelated CV term still warns. Needs CV-hierarchy awareness to close fully.

---

## #11 — Converge grid **storage** onto the upstream generic grid facet 🟡

> **Design proposal: [GRID-FACET-DESIGN.md](GRID-FACET-DESIGN.md)** (DRAFT v0.1) — facet
> schema, `grid_id` referencing, parametric + materialized forms, worked Agilent-dedup
> migration, and open questions to settle with upstream before any code. Per the chosen
> approach (design-first, no code), this is the current state of #11/#12.

Prompted by the upstream `mzpeak_prototyping` author's note on grid storage (the person
who designed the polynomial recalibration / null-filling model). Two halves: what's
settled, and what changes.

**Settled — the math has converged, no action.** Our index-interpolating-grid math
(SQRT transform + m/z deltas + polynomial recalibration) independently converged with
both the upstream author's plan *and* TOFEE. So the calibration models we already ship
are validated in spirit and we should keep them:
- SCIEX run-wide `m/z = (c0 + c1·tof_index)²` (`sciex_sqrt`) — [main.rs:941](src/main.rs:941).
- Agilent per-spectrum `(tof_c0 + tof_c1·k)²` + polynomial refine (`agilent_sqrt_poly`) —
  [main.rs:1203](src/main.rs:1203), per-CalibrationID poly in `agilent_profile.rs`.
- Bruker `(a + b·tof)²` (`ims_calibration`) — [bruker_native.rs:149](src/bruker_native.rs:149).

**Changes — storage diverges.** The upstream plan is a *generic grid* stored in an
**additional Parquet facet**, not the three ad-hoc forms we use today:
- **Their model:** one extra Parquet file; rows chunked into segments per grid; an
  **entity-index column = grid ID** (incremental per-grid decode); grid values possibly in
  a **list column**; each grid defined *either* parametrically (index-space equation +
  coefficients) *or* **materialized** (precomputed coordinate list, recalibratable with a
  model). Generic across TOF (parametric) and low-res axes (materializable).
- **Ours today (divergent, 3 inconsistent representations):**
  1. SCIEX — column transform metadata (`mzpeak:transform_params`) + a `tof_calibration`
     JSON block in `mzpeak_index.json`.
  2. Agilent — **per-spectrum columns** `tof_c0`/`tof_c1`/`tof_calibration_id` in
     `spectra_metadata.parquet` + a `calibrations` map in the index JSON.
  3. Bruker — `ims_calibration` JSON block + column transform.
  There is **no grid entity/facet**; nothing is referenced by a first-class grid ID.

**Impact / why it matters:**
- **Forward-compat risk.** If upstream lands the grid facet, our JSON-index blocks +
  per-spectrum coefficient columns become a legacy dialect the reference reader /
  `mzpeak.org/view` / validator won't interpret. Treat our current grid forms as
  **provisional** and plan a migration once the upstream facet stabilizes.
- **Continuation of the metadata-bloat fix ([[no-converted-mzpeak-to-s3]] sibling work).**
  Agilent repeats `tof_c0`/`tof_c1` on *every* spectrum even though they are shared by all
  spectra with the same `tof_calibration_id` (which we already emit). A grid facet keyed by
  grid ID stores each distinct grid **once** and lets spectra reference it by ID. (Marginal
  now — ~3×f64/spectrum, compressible — but it's exactly the per-spectrum-repetition smell
  we just removed for intensity; the facet is the clean home.)
- **New capability we lack:** the **materialized** grid mode (coordinate list + optional
  recal model). All our grids are parametric today.

Action: track the upstream design; keep our math; when the facet API exists, migrate the
three representations onto it (grid-ID-referenced, parametric + materialized) and drop the
per-spectrum coefficient columns. Pairs with #12.

## #12 — Materialized low-res grids for **time** and **ion-mobility** axes 🟡

Depends on the grid facet from #11. The upstream note calls out that a **materialized**
grid (precomputed coordinates, recalibratable) "would work for any sufficiently low
resolution measure like time or ion mobility … a time grid would be convenient for storing
many, many chromatographic traces."

We have no materialized-grid path today — both of these are stored **per-point, inline**:
- **1/K0 (ion mobility):** one f64 per peak in `MeanInverseReducedIonMobilityArray`,
  inline in `spectra_peaks` — [bruker_native.rs](src/bruker_native.rs) (`mobility.push`).
  The 1/K0 ramp is largely shared across frames yet re-stored per point.
- **Chromatogram time:** one f64 per point in `TimeArray` in `chromatograms_data.parquet` —
  [main.rs:2361](src/main.rs:2361) (`synth_chromatogram`). A time axis shared by many
  traces is re-stored per trace.

Opportunity: once #11's grid facet exists, store a shared low-res axis **once** as a
materialized grid and reference it by grid ID from many entities — compact storage for
many chromatographic traces / XICs (and a natural home for SRM/MRM transitions if we add
them), plus dedup of the 1/K0 ramp. New writer/reader paths for the materialized form.

**But for ion mobility specifically, #13 measured this and the answer is: don't.**

## #13 — Measured: a materialized grid for timsTOF ion mobility saves ~nothing ✅ (analysis)

Josh's follow-up claim: "ion mobility is so low in resolution compared to m/z that we do
not actually need to do anything special. Parquet's RLE_DICTIONARY effectively does this
[a materialized grid] while adaptively selecting the smallest index size… DELTA_BINARY_PACKED
could squeeze a little more from long monotonic runs [but] ion mobility lacks this when it
is a tertiary sorting dimension." Measured on real timsTOF (SBA415 `.d`, 400 frames →
3,008,870 peaks, ims-compact peaks facet). **He is right on every point.**

**The 1/K0 column (`mean_inverse_reduced_ion_mobility`, f64):**

| encoding of the SAME data (ZSTD) | size | note |
|---|---|---|
| f64 + dictionary (**what we ship — Parquet auto-picks RLE_DICTIONARY**) | **405 KB** | |
| f64 PLAIN | 551 KB | dict already saves 27% over this, for free |
| f64 BYTE_STREAM_SPLIT | 2,495 KB | wrong tool for low cardinality |
| **materialized grid** (uint16 index + one 718-value table) | **406 KB** | **+0.3% — i.e. 1.2 KB *worse*** |
| materialized grid, DELTA-packed index | 474 KB | worse (IM non-monotonic as tertiary dim — as Josh predicted) |

- Cardinality is **718 distinct 1/K0 over 3.0M points (0.024%)** — the mobility scan count,
  fixed by the instrument regardless of run length, so this ratio only gets more extreme on
  bigger runs.
- A materialized grid **is** RLE_DICTIONARY: the dictionary page already holds the 718
  distinct values (the "grid"), the data page holds the indices. Building it by hand just
  reimplements dictionary encoding and adds a read-time join, for **0% gain**.
- Stakes are tiny anyway: the whole mobility column is **3.9% of the peaks facet**; deleting
  it outright would shrink the file <4%.
- **Decision: do nothing special for ion mobility.** Keep the plain f64 column; let Parquet
  pick the encoding (it picks RLE_DICTIONARY). Scope #12's materialized-grid work to *time*
  axes (many chromatographic traces), not IM.

**"How much does our byte-packed (int `tof`) representation save on top of that?"** — also
less than intuition suggests, and for the same reason:

| `tof` axis representation (ZSTD) | size | |
|---|---|---|
| int32 dictionary | 5.56 MB | smallest |
| **int32 DELTA_BINARY_PACKED (what we ship)** | **6.18 MB** | |
| f64 m/z `(a+b·tof)²` dictionary | 5.74 MB | ≈ int32 dict (±3%) |
| f64 m/z PLAIN | 9.45 MB | |
| f64 m/z BYTE_STREAM_SPLIT | 17.75 MB | |

- Real timsTOF m/z has only **~40k distinct values** (it *is* a tof grid), so under dictionary
  encoding the int-vs-float width advantage nearly vanishes: byte-packed int32 (5.56 MB) vs
  best f64 m/z (5.74 MB) is a **~3%** difference. The big wins (41% vs f64 PLAIN, 69% vs BSS)
  only exist against *un*-dictionaried float storage. **So ims-compact's real value is
  losslessness (bit-exact integer grid), not size — its size ≈ dictionary-encoded f64 m/z.**
- **Side finding (RETRACTED — see #14):** the table above was measured on a **400-frame
  subset**, where int32 dictionary looked ~10% smaller than the shipped DELTA. **This does
  not hold at scale.** On the full SBA415 (821 M points) the shipped absolute+DELTA is
  **1.633 B/pt** while dictionary is **2.195 B/pt (worse)**. Small-subset encoding
  measurements don't predict full-file behavior here — always validate on the whole file.

Net: ion-mobility grid work is unjustified. The `tof` column is already near-optimally
encoded (see #14, which also refutes an external "per-scan-delta" handoff).

## #14 — `tof` column encoding: handoff refuted; current encoding is near-optimal ✅ (analysis)

External handoff (`~/Downloads/mzpeak-timstof-tof-encoding-regression-handoff.md`) claimed the
ims-compact `tof` column regressed by storing **absolute** flight-time bins, and that restoring
BRFP's **per-(frame,scan) delta-reset** would drop `tof` from ~1.36 GB to ~0.3–0.5 GB (3–4×).
**Measured on the full SBA415 (821 M points) — the fix is wrong and would make it worse.**

| `tof` encoding (full file, ZSTD) | B/pt | |
|---|---|---|
| **absolute + DELTA_BINARY_PACKED (what we ship)** | **1.633** | best of the simple options |
| absolute + dictionary (the retracted #13 idea) | 2.195 | worse |
| per-(frame,scan) delta-reset + DELTA (the handoff's fix) | 2.056 | **worse** |
| per-scan-delta + dictionary | 1.995 | worse |

- **Why the handoff is wrong:** it assumes within a mobility scan `tof` rises in "a handful of
  bins." It doesn't — scans are **sparse (~34 peaks across a ~400k-bin flight-time axis;
  intra-scan delta median 189, max 353k)**, so per-scan deltas are large and pack worse than
  the absolute values. The premise is false; the fix regresses `tof`.
- **The real (modest) lever the handoff missed is point *sort order*, not delta encoding.**
  `tof` and `mobility` cost trade against each other: the point list has one order.
  - current **scan-major**: `tof` 1.633 + `mobility` 0.071 = 1.704 B/pt (coords)
  - **tof-sorted within spectrum**: `tof` 0.341 + `mobility` 1.227 = 1.568 B/pt (coords)
  - Net **−0.136 B/pt (~5% of the peaks facet)** — tof-sorting cheapens `tof` exactly as the
    handoff wanted, but balloons `mobility` (RLE_DICTIONARY runs scramble), and reorders
    points away from mobility-major (breaks per-peak ion-mobility locality readers may rely
    on). Modest win, real semantic cost — **not recommended without a consumer-side reason.**
- **Intensity** (handoff dismissed it as fine — but it's the real slack): f32 BYTE_STREAM_SPLIT
  1.17 B/pt, yet its order-0 **symbol entropy is 0.94 B/pt** (4,865 distinct values, median 65).
  No stock Parquet codec reaches it (int32 dict 1.21, dict+gzip-9 1.21, raw zlib uint16 1.22) —
  byte-oriented coders can't capture a 16-bit symbol histogram. ~0.23 B/pt (~190 MB) sits here,
  recoverable only with a symbol-level entropy coder (FSE/range), which Parquet doesn't expose.
- **"Larger than the raw `.d`" — correction: ~1.10× is NOT a fundamental floor**, it's the
  floor of Parquet's *stock byte codecs*. The data's order-0 entropy is ~2.5 B/pt ≈ **2.05 GB,
  *below* raw (2.21 GB)** — Bruker's `.d` isn't even optimal. Two ways to approach raw:
  - **Pure-Parquet: tof-major sort → ~2.31 GB (≈1.04× raw), essentially matching** — `tof`
    deltas go tiny (1.63→0.34) but `mobility` balloons (0.07→1.23) and point order leaves
    mobility-major (breaks per-peak IM locality). Nearly matches, real semantic cost.
  - **Match/beat raw (≤2.21 GB, down to ~2.05 GB):** needs a custom symbol-entropy-coded
    intensity column (FSE/range) or Bruker's 2D nested-frame layout — engineering, not a flag.

**Decision: keep the current `tof` encoding (absolute + DELTA) as the default.** It's the best
of the stock options *and* preserves mobility-major locality. The handoff's per-scan-delta fix
is refuted. The size levers (tof-major sort; custom-entropy intensity) are real but each carries
a cost (IM locality / a bespoke codec) — revisit only if matching/beating raw becomes a goal.
Lesson logged twice: (1) measure encodings on the full file, not a subset; (2) "Parquet
can't go lower" ≠ "the data can't go lower" — check the symbol entropy.

### Reproducing Bruker's byte-order encoding (how to ~match the raw `.d`)

Bruker's TDF compactness comes from a **byte-plane shuffle** (decompose the integer array into
byte planes — all byte-0s, then byte-1s, … — so the mostly-zero high bytes form long runs) then
zstd. Reproduced and measured on the full SBA415:

| column | current Parquet | byte-shuffle + zstd | entropy |
|---|---|---|---|
| `intensity` (as **uint16**) | 1.169 | **0.984** ✓ (hits floor) | 0.94 |
| `tof` (uint32) | 1.633 | 1.643 (no help) | 1.54 |

- **Intensity is the whole win.** Stored as a uint16 with byte-shuffle it drops to the 0.94 B/pt
  floor (median count 65 → high byte plane ≈ all zeros). `tof` doesn't benefit (full ~18-bit
  range, no zero planes — already near entropy).
- **Projected file: ~2.26 GB ≈ the raw `.d` (2.21 GB, ~1.02×)**, down from 2.42 GB (1.10×) —
  scan-major layout kept (no IM-locality cost). Just the intensity column changes.
- **What it needs:** intensity stored as an **integer** (lossless for native counts) + BYTE_STREAM_SPLIT.
  BSS on a *float* column (what we ship) gives only 1.17 — the 4-byte float carries mantissa noise;
  the win requires the 2-byte integer dtype. The vendored **parquet crate is 42.0.0**, which only
  does BSS on FLOAT/DOUBLE; int-BSS (Parquet format v2.10) needs arrow-rs ≈ v53+. So shipping this
  is either **(a) bump parquet/arrow-rs to ≥53** and store integer intensity + BSS (clean, standard,
  reader-portable), or **(b) a manual byte-shuffle codec** (works on crate 42 but makes the column
  non-standard). Bruker-/integer-intensity path only (generic f32 intensity is unaffected).

Net: matching the raw `.d` *is* reproducible, entirely via the intensity column. Gated on the
arrow-rs upgrade (or a bespoke codec); ~6% file size for a dependency bump — pursue if size parity
with vendor `.d` is a goal.
