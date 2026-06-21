# mzPeakConverter — handoff summary

Status as of 2026-06-21. Built autonomously with per-phase adversarial review (codex/vibe/kimi).
See `PLAN.md` for full detail, `README.md` for usage.

## What works (built, tested, validator-clean)

Single Rust binary `mzpeak-convert` (`cargo build`): `convert` / `inspect` / `validate` /
`ims-compact` / `tof-grid-probe`. Reads via `mzdata` (+ direct vendor readers where mzdata is
insufficient); writes via the vendored `mzpeak_prototyping` writer.

| Input | Path | Notes |
|---|---|---|
| mzML / mzML.gz | mzdata | |
| imzML | mzdata | imaging coord columns + IMS cv |
| Bruker **TDF** | mzdata `bruker_tdf` | + `--ims-compact` lossless integer-tof in-archive (−51%), or bare `ims-compact` parquet (−72%, BSS) |
| Bruker **TSF** | ported reader (`bruker_tsf.rs`, rusqlite + zstd) | otofControl-corrected m/z |
| Thermo **.raw** | mzdata `thermo` (.NET) | + verbatim `vendor_scan_trailers` facet |

**Vendor injection** (`vendor.rs`): YAML glob→action policy (`--config`/`--aux`/`--no-vendor`),
stream-embedding into `vendor/` (STORED, gzip-on-read), preserve-by-default, declared `proprietary`
in the index `files[]`, gzipped members `.gz` — aligned to the mzPeakViewer consumer.

**Verification:** joint corpus `tests/corpus.tsv` + `tests/run_corpus_e2e.sh` → **11/11 PASS**
(convert + `--verify` + `mzpeak-validate`), 1 SKIP (BAF). `cargo test` → 2 unit tests pass.
Every output validates clean (0 errors). Each phase was adversarially reviewed and the findings
applied (e.g. ims-compact through-disk lossless check; TSF otofControl m/z fix; vendor
preserve-by-default + path-escape guards).

## Deferred — and why

| Item | Reason |
|---|---|
| Bruker **BAF** reader — *runtime test* | `src/bruker_baf.rs` is ported + **compiles under `--features bruker_sdk`**, but needs `libbaf2sql_c` (Windows/Linux only) to run/verify — not on macOS. Test there. |
| **TOF-grid encoder** (fit-from-m/z) — *won't build* | Benchmarked (`tof-grid`): on SCIEX the √-grid m/z (68.4 MB) ties delta+zstd (71.3 MB) *while being lossy* (2.2 ppm). Delta+zstd already wins → encoder DEFERRED/dropped. (Bruker native-integer grid ships via ims-compact.) |
| ims-compact: registered tof→m/z column transform | **Per-frame streaming DONE** (`encode_ims_compact` writes one RecordBatch per frame; ArrowWriter coalesces into row groups → memory bounded by the largest frame, not the run; verify streams both sides too). Calibration in `ims_calibration` KV (BRFP-equivalent). Still deferred: the *registered* column transform that lets readers reconstruct m/z generically (viewer handoff §2) — needs `mzpeak_prototyping` writer support (`ChunkTransform` exists but wiring it is intricate; m/z is reconstructable today from the `a,b` calibration). |
| Thermo trailers: error-log facet | tall + wide + status-log facets shipped (`thermo_trailers.rs`, `thermo_status.rs`); the error-log is **blocked** — `thermorawfilereader` 0.7.0 exposes no error-log API. Needs an upstream addition. |
| **P6 CI** (STORED-zip + spec-commit pinning) | This dir is not a git repo, so no CI to wire. NOTE: STORED-zip is already enforced by `mzpeak-validate`'s `zip_stored` rule (passes on all outputs). |
| **Agilent + SciEX native** — *runtime test* | IMPLEMENTED + compile-verified: `src/{sciex,agilent}.rs` (netcorehost) + `glue/{sciex,agilent}/` (reflection C# glue, `dotnet build`s on mac). Behind `--features sciex`/`agilent` (Windows-runtime-only). codex+vibe FFI-reviewed; fixes applied. **Not runtime-tested** (no vendor DLLs here) — verify on Windows with `$MZPC_PWIZ_DIR` (pwiz DLLs) + built glue. `--via-msconvert` works today meanwhile. Agilent IM-MS (MIDAC) still TODO. |

## Gotchas for whoever picks this up

- **Validator env:** the on-PATH `mzpeak-validate` shebang is pinned to anaconda **base** python
  (3.7.4 / pyarrow 12.0.1) — *not* updated by the env-level Python 3.14 bump. The pyarrow-24
  validators live in conda envs: **`~/anaconda3/envs/mzpeak314/bin/mzpeak-validate`** (python 3.14)
  and `…/envs/mzpeak/bin/…` (python 3.11). The e2e harness prefers `mzpeak314` → `mzpeak` → PATH,
  overridable with `$MZPEAK_VALIDATE`. (Per owner: left as-is; the harness uses the env.)
- **Thermo .raw needs .NET**; the binary auto-sets `DOTNET_ROLL_FORWARD=LatestMajor` +
  `MZDATA_IGNORE_UNKNOWN_INSTRUMENT=ignore`.
- **`timsrust-tsf` is unusable** with mzdata (rusqlite/libsqlite3-sys native-lib conflict) — TSF is
  read directly with the versions mzdata already links.
- ims-compact in-archive: the writer emits a `tof` column only via
  `store_peaks_and_profiles_apart(ArrayBuffersBuilder)`, NOT `add_spectrum_field` (which leaves the
  default null m/z column).
- `mzpeak_prototyping` (and the spec) are vendored at `vendor/`; pin converter + writer + validator
  to one spec commit when CI exists.
