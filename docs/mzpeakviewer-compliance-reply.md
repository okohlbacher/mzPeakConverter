# mzPeakConverter ⇄ mzPeakViewer — reply to the compliance handoff

**From:** mzPeakConverter (the Rust converter) · **For:** mzPeakViewer
**Re:** [`mzpeakviewer-compliance-handoff.md`](../../BRFP/docs/mzpeakviewer-compliance-handoff.md)
(mzPeakViewer ⇄ BRFP, v0.6.10)

> Context: the original handoff assessed **BRFP** output. mzPeakConverter is the
> production Rust converter that supersedes the BRFP prototype, so several of the
> findings now have a different (often resolved) status. This is the converter-side
> reply, section-by-section. As always, the mzPeak format itself is still in the
> HUPO-PSI specification process (draft v0.9) — this converter is a technical
> demonstrator, not a production tool yet.

---

## §2 — ims-compact archive m/z reconstruction (headline) — **contract kept; transform still deferred**

Confirmed and unchanged in mzPeakConverter, and now the **default** for Bruker
timsTOF (TDF): the in-archive ims-compact `spectra_peaks` `point` struct carries
integer **`tof` (Int32) in place of `m/z array`**, and the index keeps
`metadata.ims_calibration` as the contract:

```json
{ "codec": "ims-compact", "lossless": "tof", "mz_from_tof": "(a + b*tof)^2",
  "tof_encoding": "absolute", "a": <f64>, "b": <f64> }
```

So the viewer's planned read-boundary reconstruction — detect a `tof` array +
`ims_calibration`, compute `mz = (a + b·tof)²` when no `m/z array` is present — is
exactly right and remains the integration point. `tof` in the **archive** is
**absolute** (no delta), so reconstruction is a direct per-point map.

On the **registered TOF→m/z transform** (a `chunk_transform`-style CURIE +
coefficients on the column so generic readers need no ims-compact special-casing):
mzPeakConverter still **defers** this. It needs writer support in
`mzpeak_prototyping` (the `ChunkTransform` concept exists but wiring it through the
custom peaks schema is intricate and unverified). Until then, `ims_calibration` in
the index stays the contract — we will not remove it. If/when the spec blesses a
registered transform, we will emit it *in addition to* `ims_calibration`.

`--no-ims-compact` produces a standard f64 `m/z array` archive for any reader that
prefers not to special-case at all.

## §3 — validator can't validate ims-compact archives — **resolved (environment)**

This was a stale-pyarrow problem, not a file defect, and it is **resolved** on our
side: the validator has been lifted past pyarrow 12 (BSS-INT32 needs
parquet-format ≥ 2.10). mzPeakConverter's e2e harness selects a `pyarrow ≥ 14`
environment, and **all TDF ims-compact archives now validate clean (0 errors)** in
our corpus runs. No spurious FAILs remain. (Note: mzPeakConverter's in-archive
`tof` column is Int32 + zstd; the BSS-INT32 encoding lived in the now-removed bare
encoder — see §6 — so the original BSS error class doesn't even arise for the
archive path, but the pyarrow bump is the right fix regardless.)

## §4 — stored chromatograms (TIC/BPC) not surfaced — **converter writes an empty chromatogram facet (by design, for now)**

Behavior differs from BRFP here. mzPeakConverter currently emits an **empty**
`chromatograms` facet for the ims-compact (and other custom-reader) paths — it does
**not** synthesize TIC/BPC. So "0 stored chromatograms" in the viewer is *expected*
for mzPeakConverter output, not a schema mismatch. The facet that *is* written
conforms to the reference chromatogram schema from `mzpeak_prototyping`/mzML2mzPeak
(the writer finalizes index metadata via that facet). Synthesizing TIC/BPC during
conversion is a reasonable enhancement and is on our backlog; flag if the viewer
wants it prioritized. For mzML/Thermo inputs, source chromatograms *are* carried
through.

## §5 — UV/DAD (`baf_uv_wavelength`) — **not produced by mzPeakConverter (N/A)**

The `wavelength_spectra_data` UV/VIS facet is a BRFP feature. mzPeakConverter does
**not** currently carry UV/PDA (non-MS) spectra — they are a known, documented
limitation (the converter drops non-MS spectra). So the baseline-scan and
row-group-streaming follow-ups don't apply to mzPeakConverter output today. If UV/DAD
support lands, we'll size `wavelength_spectra_data` row groups for range-streaming as
suggested. No action from us now.

## §6 — bare `tdf_ims_bare.parquet` — **removed from the converter**

mzPeakConverter has **removed** the standalone bare-parquet ims-compact encoder
(and its CLI command). ims-compact is now produced **only as an in-archive facet**
(a conformant ZIP). So the viewer's "not a ZIP, can't open" track is **moot for
mzPeakConverter output** — every mzPeakConverter file is a standard `.mzpeak`
archive. The viewer's deferral of a bare single-parquet loader is fine; nothing from
mzPeakConverter will require it.

## §7 — vendor embedding etc. — **aligned / compliant**

mzPeakConverter matches the behavior the viewer validated:
- Vendor side-files embedded under `vendor/` (preserve-by-default), gzipped, declared
  `data_kind: "proprietary"` in the index `files[]`; `analysis.tdf_bin` droppable via
  policy. Embedding is controllable with `--no-vendor` / `--aux glob=embed|drop` (and
  the same rules in the config file).
- `mean_inverse_reduced_ion_mobility` (MS:1003006) is written for TDF.
- Thermo vendor scan-trailers + status log are carried as dedicated proprietary
  facets.
No changes needed; this stays compatible with the viewer's Structure-inspector +
on-the-fly gunzip.

---

## Summary for the viewer

| Handoff item | mzPeakConverter status |
|---|---|
| §2 m/z reconstruction from `ims_calibration` | **Contract kept** (now the TDF default); registered transform deferred |
| §3 validator BSS-INT32 failures | **Resolved** (pyarrow ≥ 14); archives validate 0 errors |
| §4 stored chromatograms = 0 | **By design for now** (empty facet; TIC/BPC synthesis backlogged) |
| §5 UV/DAD | **N/A** (UV/PDA not yet carried) |
| §6 bare `.parquet` | **Removed** — converter only emits ZIP archives |
| §7 vendor embedding / mobility | **Aligned / compliant** |

**Net:** the one cross-cutting integration item the viewer should implement —
`(a+b·tof)²` reconstruction from `ims_calibration` when peaks carry `tof` — is
correct and remains the plan; mzPeakConverter holds up its half of that contract by
keeping `ims_calibration` in every ims-compact archive index. The validator gap is
resolved. The bare-parquet and BSS concerns no longer apply to mzPeakConverter
output. Open joint item if desired: whether the converter should synthesize TIC/BPC
chromatograms (§4).

— mzPeakConverter (see [README](../README.md) · [User Manual](USER_MANUAL.md);
format: [mzpeak.org](https://mzpeak.org), viewer at [mzpeak.org/view](https://mzpeak.org/view))
