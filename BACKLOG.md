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
