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


---

## Addendum 2026-09-02 — `vendor_mz_calibration` (exact timsTOF TOF→m/z) — **new keys, additive**

`ims_calibration` is **unchanged** (`codec`, `mz_from_tof`, `tof_encoding`, `a`, `b` — the pinned
strings stay verbatim). Its `a,b` is timsrust's two-point chord, which speXtract measured at
**−5…−11 ppm** (m/z dependent) against Bruker's SDK. The archive now also carries the vendor's
exact model, from both the native and `--bruker-sdk` lanes, so a reader can offer vendor-grade m/z
without the embedded `vendor/analysis.tdf.gz` (also present with `--no-vendor`):

```json
"vendor_mz_calibration": {
  "source": "analysis.tdf",
  "mz_calibration": [ { "Id": 1, "ModelType": 1, "DigitizerTimebase": 0.125, "DigitizerDelay": 26464.125,
                        "T1": 25.6148127740566, "T2": 25.1594285616696, "dC1": 20.0, "dC2": 0.0,
                        "C0": 1008.59723408404, "C1": 154314.98518964, "C2": 0.0, "C3": 0.0, "C4": 0.0 } ],
  "global_metadata": { "DigitizerNumSamples": 636031, "MzAcqRangeLower": 99.993933, "MzAcqRangeUpper": 1700.0 },
  "per_frame_columns": [ "…_tdf_t1", "…_tdf_t2", "…_tdf_mz_calibration_id" ],
  "model_type_1": "t_ns = tof*DigitizerTimebase + DigitizerDelay; C1_eff = C1*(1 + dC1*(T1 - tdf_t1)/1e6); t_ns = C0 + (1e6/sqrt(C1_eff))*sqrt(mz) + C2*mz, solve for sqrt(mz) (C2 = 0: mz = ((t_ns - C0)*sqrt(C1_eff)/1e6)^2)",
  "model_type_1_verified": "2.5e-5 ppm vs Bruker timsdata SDK (speXtract v0.2.0); dC2 = 0 on every file seen, T2 role unverified"
}
```

- `mz_calibration` — every `MzCalibration` row, all columns verbatim (a run may reference more
  than one row).
- Three new **per-spectrum** `spectra_metadata` columns: `Frames.T1`, `Frames.T2` (Float64) and
  `Frames.MzCalibration` (Int64). Match them by **suffix** (`_tdf_t1`, `_tdf_t2`,
  `_tdf_mz_calibration_id`), exactly as you already do for `_tof_c0`/`_tof_c1` — the accession
  prefix (`opt_MS_4000903_…`) is the same local-CURIE convention. `per_frame_columns` lists the
  exact names. The temperature term needs the per-frame `T1` (it drifts within a run: 3 mK on 2485.d ≈ 0.06 ppm,
  ~30 mK on speXtract's runs ≈ 0.7 ppm); the id selects the `mz_calibration` row.
- Reader recipe (ModelType 1 only; anything else → stay on `ims_calibration`):
  `t = tof·DigitizerTimebase + DigitizerDelay`; `c1 = C1·(1 + dC1·(row.T1 − tdf_t1)/1e6)`;
  `b = 1e6/√c1`; if `C2 == 0`: `mz = ((t − C0)/b)²`, else solve `C2·u² + b·u + (C0 − t) = 0` for
  `u = √mz` (positive root) and `mz = u²`. Dropping `C2·mz` costs −11…−40 ppm on files where it is
  non-zero.
- Measured on PXD059079 2485.d (`C2 = 0`): chord − exact runs from +3.2 ppm at tof 0 to −4.2 ppm at
  the top of the range.
- Nothing to do if you don't want it: `ims_calibration` still reconstructs as before.

---

## Addendum 2026-09-02 — integer axes are resolved PER FACET (Shimadzu grid + lattice)

The `ims_calibration` rule above (one `tof` column, one block, one formula) is the timsTOF case.
The Shimadzu lane writes **two** integer axes, both named `tof_index`, in different facets, with
different dtypes and transforms — and the viewer must pick the resolver by facet, not by column name:

| facet | axis | block to consult first | reconstruct |
|---|---|---|---|
| `spectra_data` (profile) | `tof_index` Int32, `SqrtMzFromTof`, per-spectrum `tof_c0`/`tof_c1` | `tof_calibration` | `(tof_c0 + tof_c1·tof_index)²` |
| `spectra_peaks` (centroids) | `point.tof_index` Int64, `LinearMz`, params `[1e-9]` | `mz_calibration` (`codec: mz-grid, scale: 1e9`) | `1e-9 · tof_index` |

On both facets the fallback rule is per row and independent of row order: `mz` finite and > 0
wins (that row kept f64 m/z), the axis is authoritative only where `mz` is the NULL fill. The
converter routes whole spectra (a spectrum is either all-lattice or all-f64; blind / hek / d100
carry 0 mixed spectra), and a fallback spectrum inside the lattice facet most likely arrives with
NO `tof_index` key at all — mzpeakts drops a column that is all-null within the selected rows —
or, where a NULL Int64 cell is materialised, as `tof_index 0n` beside its real `mz`. Either shape
reads `mz`; never reconstruct from the zero. The `1e-9 · tof_index` column above is literal: the
viewer multiplies by `1/scale` (`=== 1e-9`), matching the Parquet transform and the reference
reader's `s · k` bit for bit — NOT `tof_index / scale`, which is 1 ulp off on ~40 % of lattice
values. **[Reversed in v0.9.8 — see the addendum below: the division is the exact form and the
reference reader now divides.]** Pre-lattice Shimadzu archives (`tof_calibration` only, plain f64 centroids) keep reading
their centroids verbatim. This is what `engine/spectrum.ts resolveFacetGridMz` / `readFacetSignal`
implement now; every other reader entry point goes through the same resolver.


## Addendum 2026-09-03 (converter v0.9.8) — the reconstruction is the DIVISION, and it reverses the paragraph above

The 2026-09-02 addendum told you to multiply by the column's `mzpeak:transform_params` (`1/scale`)
and called `tof_index / scale` the inexact one. **That is backwards, and v0.9.8 fixed the reference
reader accordingly.**

`1e-9` is not exactly 10⁻⁹, so `k · 1e-9` is not the correctly-rounded value of `k / 1e9`. Measured
on `Blind_P1_pos_012`: the two disagree for **85,706 of 216,742** centroids (~40 %), by up to
1.137e-13 Da. The `mz_calibration` block has said `mz_from_tof_index: "tof_index / scale"` since
v0.9.0; the reference reader was the thing that disagreed with the archive's own stated formula.

**Normative rule for every reader, mzPeakViewer included:**

- Recover the integer scale from `mz_calibration.scale` (or by rounding `1 / transform_params` and
  verifying that `1 / scale` reproduces the stored parameter bit for bit) and compute
  **`m/z = tof_index / scale`**.
- Fall back to multiplying by the stored parameter only when it is *not* an exact reciprocal of an
  integer scale.

`vendor/mzpeak_prototyping/src/reader/point.rs` does this as of v0.9.8, the round trip is bit-exact
at every scale (Shimadzu native archives included), and `tests/shimadzu_lattice_peaks.rs` pins
`k / 1e9` where it previously pinned `k · 1e-9`. A viewer still multiplying will differ from the
converter, from the mzML round trip and from the archive's own declared formula on ~40 % of lattice
peaks — small in absolute terms (≤ ~1.1e-13 Da), but it makes "lossless" untrue for the path the
reader actually takes.
