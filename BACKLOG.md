# mzPeak spec-compliance backlog

Deferred items from the spec cross-check (see also the spec PRs in
`~/Claude/mzPeak-specification`: signal-data grid transforms + index-file vendor blocks).

Status legend: 🔴 blocker · 🟡 deferred · ✅ done this round.

---

## #1 — Provisional accessions squatted the PSI-owned `MS:` namespace ✅ (de-squatted to MZP)

**Done.** The five grid terms moved out of `MS:` into a converter-owned CV with prefix
`MZP` (`cv/mzpeak.obo`, declared in the archive `cv_list`). On the wire they now read
`MZP:1000001` (transform) and `MZP_1000003_tof_c0` (column), never `MS:…`.

Implementation (the mzdata blocker was sidestepped, not by vendoring): MZP terms are
represented as `ControlledVocabulary::Unknown` CURIEs, and the `MZP:` prefix is supplied
at the single (de)serialisation boundary in `mzpeak_prototyping::param`
(`curie_to_string` / `parse_curie`), which every CURIE string already funnels through.
Grid paths call `register_mzp_cv()` before `finish_parquet()` so `cv_list` declares MZP.

MZP → suggested-PSI-MS mapping (for the eventual #4 swap):
`MZP:1000001`→sqrt-from-TOF transform, `MZP:1000002`→linear-m/z transform,
`MZP:1000003/4/5`→tof_c0 / tof_c1 / tof_calibration_id.

### Suggested PSI-MS terms to request (for #4)

Submit these to the PSI-MS CV (HUPO-PSI/psi-ms-CV) and replace the provisional accessions
once assigned. Accession numbers are **to be assigned by PSI**; the current placeholders
are listed for traceability.

| Provisional | Suggested name | Value / params | Definition | Suggested parent |
|---|---|---|---|---|
| `MS:1003903` | square-root m/z from TOF index transformation | params `[c0, c1]` (double) | A reconstruction transform recovering an m/z array from a stored integer time-of-flight index `k` as `m/z = (c0 + c1·k)²`, with `c0`,`c1` carried as the array's transform parameters. | sibling of the null-marking transforms `MS:1003901` / `MS:1003902` (binary data / array transformation) |
| `MS:1003904` | linear m/z grid transformation | param `[s]` (double) | A reconstruction transform recovering an m/z array from a stored integer index `k` as `m/z = s·k`, with bin width `s` carried as a transform parameter. Used for centroided TOF data off the flight-time lattice. | same parent as `MS:1003903` |
| `MS:4000900` | time-of-flight grid coefficient c0 | value: double | The constant term `c0` of the per-spectrum sqrt-TOF reconstruction `m/z = (c0 + c1·k)²`. | spectrum / scan attribute (value-type term) |
| `MS:4000901` | time-of-flight grid coefficient c1 | value: double | The linear term `c1` of the per-spectrum sqrt-TOF reconstruction. | spectrum / scan attribute (value-type term) |
| `MS:4000902` | time-of-flight calibration identifier | value: nonNegativeInteger | Selects the per-spectrum polynomial-refinement row in the `tof_calibration` index block, so the exact vendor m/z reconstructs. | spectrum / scan attribute (value-type term) |

Definition sites (single source of truth for the #4 swap to assigned MS terms):
- `vendor/mzpeak_prototyping/src/buffer_descriptors.rs` — `SQRT_MZ_FROM_TOF`, `LINEAR_MZ`
- `src/main.rs` — `TOF_C0_CURIE`, `TOF_C1_CURIE`, `TOF_CALID_CURIE`
- `cv/mzpeak.obo` — the term definitions; `mzpeak_prototyping::param` — prefix (de)serialisation

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
