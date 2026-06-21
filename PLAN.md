# mzPeakConverter — Architecture & Plan (draft for adversarial review)

Status: DRAFT v0.2, 2026-06-21. Target output: mzPeak v0.9 (validator profile `mzpeak-0.9`).

## 0. MVP status (Phase 0 — DONE)

`mzpeak-convert` binary builds and converts **mzML, imzML, Bruker TDF, and Thermo .raw** via
mzdata → mzPeak via the vendored `mzpeak_prototyping` writer. Joint corpus e2e
(`tests/run_corpus_e2e.sh` over `tests/corpus.tsv`, drawing from the mzML2mzPeak and BRFP
trees) is **green: 9/9 PASS** under `mzpeak-validate` (0 errors), 2 SKIP. SKIPs are Bruker
**TSF + BAF** — not readable by mzdata (TDF-only); their readers are ported in P3. Thermo .raw
self-hosts a .NET runtime; the binary sets `DOTNET_ROLL_FORWARD=LatestMajor` (RawFileReader
targets net8.0) so it runs on installed .NET 9/10. Metadata
fixups ported from mzML2mzPeak to reach validator-clean: empty-chromatogram facet (so the
writer finalizes index metadata + archive opens), `MS:1000294` spectrum-type column, IMS
cv-list for imzML, and `ms_run` required-field defaults. CLI: `convert`/`inspect`,
clig.dev-aligned. (Conformance validation is out of scope — left to the independent
`mzpeak-validate` tool.)

## 1. Goal & scope

One Rust binary converting **mzML/imzML, Thermo `.raw`, Bruker `.d`** → mzPeak, reading
via `mzdata`, writing via `mzpeak_prototyping`. Windows + Linux first; macOS best-effort
(no Bruker BAF SDK on macOS). Replaces three divergent prototypes (BRFP=Rust/Bruker,
mzPeak4TRFP=C#/Thermo, mzML2mzPeak=Rust/mzML) with one tool.

## 2. Architecture

Base the flow on **mzML2mzPeak** (cleanest of the three: streaming, dtype-preserving,
integrity preflight, L1/L2 roundtrip verify, de-vendored deps).

```
input → format detect → mzdata reader → [transform/encode] → mzpeak_prototyping writer → [validate]
                                              │
                              ┌───────────────┼────────────────┐
                          ims-compact     TOF grid        vendor injection
                          (Bruker IMS)    (Thermo TOF)    + YAML aux policy
```

- **Readers (mzdata):** mzML/imzML (`MzMLReader`/`ImzMLReader`), Thermo `.raw` (`thermo`
  feature → thermorawfilereader), Bruker TDF (`bruker_tdf` feature → timsrust).
- **Open gap:** mzdata Bruker coverage is TDF-centric. BAF (vendor SDK FFI) and TSF
  (custom reader) live in BRFP, *not* mzdata. The converter must either (a) carry BRFP's
  BAF/TSF readers, or (b) confirm mzdata 0.64 added them. → Research Q1.
- Single-threaded streaming, constant memory (proven to 34k+ spectra). Parallelism deferred.
- Reuse from mzML2mzPeak: integrity preflight (own the checksum gate — mzdata only warns),
  dtype-preserving decode (avoid `mzs()`/`intensities()` coercion), L1/L2 verify, ZSTD
  level + numpress knobs, ZIP-STORED writer.

## 3. Feature ports

### 3.1 ims-compact (Bruker IMS) — PORT (real, built)
Source: BRFP `mzpeak_writer.rs::write_tdf_to_ims_compact` (lines 673–862), lossless,
−34% size. Model: integer TOF index, per-mobility-scan **delta-reset** encoding;
intensity int; mobility f64 (1/K0); sqrt calibration `mz=(a+b·tof)²` in Parquet KV
metadata; sidecar `.frames.parquet` for per-spectrum fine cal. Columns:
`spectrum_index` (DELTA), `tof` (BYTE_STREAM_SPLIT), `intensity` (BYTE_STREAM_SPLIT),
`mobility` (RLE-dict), zstd-19. Optional `--consolidate-ms2` (mobility=0.0 sentinel).
- **Decision (Research Q1):** read **native integer tof** (bypass mzdata → timsrust/SQLite)
  for true losslessness, vs **derive** `tof=round((sqrt(mz)−a)/b)` from mzdata m/z (simpler,
  fidelity hinges on calibration). BRFP's encoder already derives via round(); native read
  would be strictly better but breaks "mzdata for everything."

### 3.2 TOF grid / recalib — IN SCOPE FROM THE START (per user 2026-06-21)
Source: TRFP `docs/single-axis-tof-grid-design.md`. The research + the Bruker impact-II QTOF
POC are sufficient to outline this as a feature now (not deferred). Model (in √(m/z) space, NOT
quadratic-in-m/z): per-segment master lattice + per-spectrum `√(m/z)=α+β·k (+ higher-order)`,
reconstruct `m/z(k)=(α+β·k)²`. Encode occupied bins as **run-spans + exceptions**; zeros
stripped losslessly once the grid is known; **sparse residual fallback → revert-to-explicit**
per spectrum when tolerance exceeded. Fit by **direct/closed-form, NOT WLS** (Toffee lesson).
First-class `coordinate_grid` facet keyed by `grid_id` (analyzer/vendor/mode, k-domain, coeff
basis+order, CV model accession, checksum) — not the empty `mz_delta_model` column.
- **Gating:** per-spectrum on **analyzer = TOF/Astral** (never vendor=Thermo — Orbitrap is
  frequency-domain, model N/A); accept a spectrum's grid only if round-trip-against-source is
  within tolerance, else store explicit m/z. Strongest where native integer grids exist
  (Bruker TDF, Agilent, Sciex); on Thermo Astral the grid is **fitted from m/z** (RawFileReader
  exposes no flight-time bins) with the residual fallback as the safety net.
- **Build order:** prototype + benchmark the encoder against mzPeak's existing
  delta+byte-split+zstd on real data; ship behind `--tof-grid`. Bruker TDF (native bins, shares
  the §3.1 native-TOF path) is the honest first target; Astral follows.

### 3.3 Vendor injection — PORT (richer model from the raw-verbatim-metadata branch)
Bruker source: BRFP `vendor_metadata.rs`. **Thermo source: the `raw-verbatim-metadata`
branch** of mzPeak4TRFP (richer than main) — six facets: `vendor_scan_trailers` (tall) +
`vendor_scan_trailers_wide` (typed, one column per trailer label) + `vendor_trailer_schema`
+ `vendor_file_metadata` + `vendor_status_log` (QC timeseries) + `vendor_error_log`. Port its
discipline: **dual keying** (ordinal join key + native scan_number), **typed values**
(`value` verbatim string + typed `value_float`, no reparse), single-pass streaming,
best-effort/non-fatal. This satisfies the "typed columns, not a junk drawer" review point.
- **stream-embedding (per user 2026-06-21):** embed vendor blobs by streaming them in fixed
  chunks straight into the ZIP — never buffer a whole file (large Bruker SQLite would break the
  constant-memory guarantee). Embedding is opt-in; **preserve-by-default** (any `drop` is
  explicit, recorded in the manifest, and gated by `--allow-drop`).

### 3.4 YAML aux policy — PORT
Source: BRFP `aux_config.rs` + `config/brfp-aux-defaults.yaml`. Glob→action policy
(drop/embed/tables/wavelength/chromatogram/index) with `gzip: true|false|auto`. Precedence:
`--aux` CLI > vendor section > defaults > fallback embed. Generalize to per-vendor configs;
ship sane built-in defaults via `include_str!`.

### 3.5 gzip "where meaningful"
**Constraint:** mzPeak ZIP members must be **STORED (uncompressed)** for random access — so
gzip CANNOT wrap the Parquet facets. gzip applies only to **embedded vendor blobs** under
`vendor/` (the YAML `gzip:auto` path, ~54–93% on text/XML/SQLite, off for incompressible
binary). Parquet facets get zstd internally, not gzip. → Research Q3 confirms the boundary.

## 4. CLI harmonization (clig.dev)

Subcommands: `convert` (default), `inspect`. (Conformance validation is delegated to the
standalone `mzpeak-validate`; `query`/`xic` deferred.)
Principles (clig.dev): human-first output, `--json` for machines, `--help`/`-h`,
`--version`, sensible defaults, `--dry-run`, `--quiet`/`-v`, confirm destructive, stable
exit codes.

Kill from the prototypes: TRFP numeric format aliases (`-f 4`), the `-o`=directory vs file
ambiguity, camelCase flags (`--warningsAreErrors`), dead S3 flags.

Unified surface (proposed):
- Input: positional `<input>` (+ `--input-dir` for batch). Output: `-o/--output` (file).
- `--format {mzpeak,mzml,none}` (default mzpeak; mzpeak is the point of the tool).
- `--layout {chunked,point}`, `--no-numpress` (lossless), `--chunk-size`, `--zstd-level`.
- `--ims-compact`, `--consolidate-ms2`, `--tof-grid` (experimental, off).
- `--ms-level 1,2|1-3`, `--peak-mode`, `--no-peak-picking`.
- `--vendor-metadata {tall,wide,both}`, `--config <yaml>`, `--aux glob=action`, `--dump-policy`.
- `--verify` (round-trip fidelity). Conformance validation is external (`mzpeak-validate`).
- `--log-level {error..trace}`, `--log-file`, `--quiet`, `-v/--verbose`.
- Exit codes (from mzML2mzPeak): 0 ok, 1 generic, 2 integrity, 3 unsupported, 4 coord, 5 verify-fail.

## 5. Spec development

mzPeak v0.9 (draft 5, unratified) lacks first-class facets for grid/ims-compact/vendor.
Approach: define these as **extension facets** declared in `mzpeak_index.json`, keep the
validator profile in lockstep (pin to a commit), and submit spec PRs to HUPO-PSI for:
(1) coordinate/grid facet (TOF), (2) ims-compact conventions, (3) vendor facet conventions.
Until ratified, mark extension facets so `mzpeak-validate` treats them as known-optional.

## 6. Phasing

- **P0 — DONE.** Scaffold + clean CLI + mzML/imzML/TDF/**Thermo .raw**→mzPeak via mzdata +
  validate integration + green joint-corpus e2e (§0).
- **P1 — DONE.** Thermo hardening: out-of-box .NET roll-forward + `MZDATA_IGNORE_UNKNOWN_INSTRUMENT`
  (no panic on new Astral models); `--verify` round-trip fidelity (spectrum-count, encoding- and
  zero-mask-invariant), wired into the e2e harness; README documents the .NET prerequisite.
  KNOWN BEHAVIORS (deferred, not blocking): (a) zero-intensity-run masking is ON by default
  (`build(_, true)`, matching the reference) — lossy on zero points, validator-accepted; add
  `--keep-zeros` if exact point sets are needed. (b) value-level (m/z/intensity) fidelity is
  deferred to P2's lossless path where zero-masking is off; an INDEPENDENT (non-mzdata) oracle for
  Thermo is still future work.
- **P2 — CORE DONE.** Native integer-TOF reader (`src/bruker_native.rs`, via timsrust — the crate
  mzdata wraps — reading raw `u32` TOF bins mzdata's API discards) + **ims-compact** encoder
  (`ims-compact` subcommand). Real diaPASEF: 291k peaks, **0.73 MB vs 2.57 MB standard (−72%,
  BSS-INT32 on tof+intensity + zstd)**,
  **lossless verified THROUGH DISK** (read Parquet back, decode delta-reset, compare to native bins).
  Calibration `a,b` extracted via public `convert(0)/convert(1)`; honest claim is TOF-bin-exact
  (m/z reconstructed `(a+b·tof)²`; a,b may differ from timsrust's internal by ≤1 ULP — bit-exact m/z
  needs an upstream a,b accessor). IN-ARCHIVE INTEGRATION DONE (Track 1, `convert --ims-compact`):
  the `spectra_peaks` point facet carries `{spectrum_index, intensity,
  mean_inverse_reduced_ion_mobility, tof:int32}` (no m/z) via the writer's
  `store_peaks_and_profiles_apart(ArrayBuffersBuilder)` custom schema + a per-spectrum
  `nonstandard("tof")` array; `ims_calibration` (absolute tof, a/b) in the index. Full
  viewer-loadable archive: diaPASEF **1.25 MB vs 2.57 MB standard (−51%)**, validates PASS
  (pyarrow 24); matches BRFP's archive structure + the handoff's expected facet. **Per-frame
  streaming DONE** (bare `ims-compact` encoder: one RecordBatch per frame, ArrowWriter coalesces
  into row groups → constant memory for multi-GB TDF; streaming lockstep verify; diaPASEF 291531
  peaks lossless PASS). REMAINING (lower priority): the optional *registered* TOF→m/z column
  transform (beyond index `ims_calibration`) for generic m/z reconstruction — needs
  `mzpeak_prototyping` writer support (`ChunkTransform` exists; wiring is intricate).
- **P3 — TSF DONE; BAF ported + compiling (untestable on macOS).** BAF reader `src/bruker_baf.rs`
  (subagent port of BRFP's `baf.rs`: libloading FFI to libbaf2sql_c, SDK discovery, SQLite
  Spectra⋈AcquisitionKeys, auto/vendor/raw calibration, safeguards) is behind a `bruker_sdk` cargo
  feature + wired (detection `analysis.baf`, `convert_baf` mirroring `convert_tsf`). **Compiles under
  `cargo build --features bruker_sdk`** but can't RUN here (no vendor SDK on macOS) — verify on
  Windows/Linux with the SDK. Default build stays FFI-free. **codex FFI review applied + verified**
  (10 findings: array_read_double count-recheck before read, line-first default, bail on
  one-missing-array / non-positive IDs, LEFT JOIN + COUNT cross-check, auto-cal probe+raw fallback,
  checked ms_level, OsStr path on Unix, per-spectrum byte budget, !Send/!Sync marker) — clean build,
  3 BAF unit tests pass. Below, the original TSF/BAF context: TSF: ported BRFP's reader (`src/bruker_tsf.rs` —
  rusqlite + zstd `tsf_bin` decode → centroid mzdata spectra; `timsrust-tsf` was unusable, its
  rusqlite/libsqlite3-sys conflicts with mzdata's). Both Urine pos+neg fixtures convert
  validator-clean (**corpus e2e now 11/11 PASS, 1 SKIP**). BAF: needs the `libbaf2sql_c` vendor
  SDK, **not installable/testable on macOS** — implementing untested FFI is irresponsible, so it's
  deferred to a Windows/Linux environment behind a `bruker_sdk` feature (BRFP's `baf.rs` is the
  port source). This is the project's known macOS limitation, not a defect.
  Codex review of the TSF decode applied: **otofControl ±5 Th calibration correction** (real m/z fix),
  exact-length zstd decode (catches under-counted NumPeaks → garbage intensities), checked bounds +
  negative-value rejection, real `Frames.Id` native IDs, MS3 (MsMsType=3) mapping, no out-of-range panic.
- **P4 — CORE DONE (Bruker), viewer-aligned.** `src/vendor.rs`: YAML glob→action policy
  (`--config` / `--aux` / `--no-vendor`), **stream-embedding** of Bruker `.d` side-files under
  `vendor/` as STORED members, gzip-on-read (flate2, bounded memory) for compressible types.
  **Preserve-by-default** (nothing dropped unless asked — keeps the lossy-TDF-path raw bins);
  symlink/path-escape guarded; fatal-on-partial-member + atomic temp→rename; iterative O(p·n) glob.
  Embedded files declared as `proprietary` `FileEntry`s in the index `files[]` and gzipped members
  carry `.gz` — both per the **mzPeakViewer compliance handoff** (the viewer surfaces proprietary
  members + gunzips `vendor/*.gz` on download). `vendor_files` manifest (embed/drop/error + size +
  content_encoding) + `vendor_metadata` (GlobalMetadata) index blocks. Codex-reviewed; e2e 11/11.
  **Thermo trailers DONE (Track 2):** `src/thermo_trailers.rs` reads the verbatim scan-trailer bag
  directly from `thermorawfilereader` (mzdata's spectrum API doesn't surface it) → a
  `vendor_scan_trailers.parquet` proprietary facet (tall: ordinal/label/value/value_float; 1248 rows
  on small.RAW: AGC, Charge State, FT Resolution, Ion Injection Time, Conversion Parameters, …),
  validator-clean. **Wide + status-log facets DONE** (`src/thermo_status.rs`, subagent): adds
  `vendor_scan_trailers_wide.parquet` (one row/spectrum, typed column/label) +
  `vendor_status_log.parquet` (QC timeseries) as proprietary facets — all three embed on small.RAW,
  validator-clean.

## Consumer compliance — mzPeakViewer handoff (ingested 2026-06-21)

Source: `BRFP/docs/mzpeakviewer-compliance-handoff.md` (viewer v0.6.10 read-side assessment).
Aligned into the plan:
- **ims-compact ARCHIVE m/z reconstruction (P2 integration spec).** When ims-compact becomes the
  in-archive `spectra_peaks` facet, the `tof` column (array_accession **MS:1000786**) REPLACES the
  `m/z array` (MS:1000514) — naïve readers then render EMPTY spectra. So the archive variant MUST:
  (a) keep `ims_calibration` in the index, and (b) attach a **registered TOF→m/z transform**
  (chunk_transform-style CURIE + a,b coefficients) to the `tof` column's array metadata so
  conformant readers reconstruct `(a+b·tof)²` generically. Archive variant stores **absolute** tof
  (no delta); the bare `.parquet` variant uses reset-delta.
- **BSS-INT32 now enabled (validator unblocked).** mzPeakValidator was lifted to pyarrow 19, so
  `BYTE_STREAM_SPLIT` on INT32 no longer spuriously fails. ims-compact's `tof` + `intensity` columns
  now use BSS (dictionary off) + zstd: **diaPASEF 0.73 MB (was 0.93), −72% vs the 2.57 MB standard
  archive**, lossless verify still PASS. NOTE: the *local* on-PATH `mzpeak-validate` console script
  still resolves to anaconda pyarrow 12.0.1 — when ims-compact is wired in-archive, validate with
  the upgraded (19) install, or `pip install -U pyarrow` in that env.
- **Chromatograms facet** must match the reference schema (viewer listed 0 stored on BRFP's). Ours
  is written by the reference `mzpeak_prototyping` writer, so it matches by construction — but
  cross-check when wiring ims-compact in-archive.
- **UV/DAD (future):** size `wavelength_spectra_data` row groups for range-streaming (don't emit one
  giant group); sanity-check baseline scans aren't zeroed. (Only relevant once UV is in scope.)
- **P5 — SPIKE DONE; encoder gated.** Bruker-TDF TOF-grid is already delivered by ims-compact
  (native integer grid). For the fit-from-m/z case, `tof-grid-probe` (Track 3, `src/tof_grid.rs`)
  fits `√(mz)=α+β·k` with **k recovered from √(m/z) spacing** (not the gapped point index) and
  reports residuals + projected size. **Measured: SCIEX TripleTOF profile = median 0.0062 ppm, p95
  0.55 ppm, 704/704 spectra fit ≤5 ppm → GO-candidate** (the √-lattice holds for real TOF data).
  Naive point-index fitting gives ~16% residual (proves the zero-suppression/gap risk the research
  flagged). Orbitrap is frequency-domain (1/√mz) → analyzer-gate, never apply.
  **BENCHMARK DONE (`tof-grid` subcommand) → encoder DEFERRED.** Encoded SCIEX m/z three ways
  (zstd): raw f64 = 126.7 MB; **delta-f64 = 71.3 MB**; **√-grid (k:i32 + α,β) = 68.4 MB = 0.96× of
  delta**. So the grid ties mzPeak's existing delta+zstd on size *while being lossy* (2.2 ppm) vs
  delta being lossless. **Verdict: do NOT build the fit-from-m/z grid encoder** — delta+zstd already
  wins. (Bruker's native-integer-tof grid is the genuine win and already ships via ims-compact.)
  Astral remains unmeasured (no data), but the SCIEX result makes a Thermo build unattractive too.
- **P6** Spec PRs + one pinned spec commit shared by converter/writer/validator, CI-enforced.
- **P7 — Agilent + SciEX (future, feature-gated, off by default).** See §3.7. Same posture as
  `bruker_sdk`: vendor SDK hosted in-process, user-supplied DLLs, **Windows-only when enabled**,
  not built by default. Cargo features `agilent` / `sciex`.

Engineering invariants (CI): every `.parquet` ZIP member is STORED; only `vendor/` members may
be compressed; converter + `mzpeak_prototyping` + validator profile share one pinned spec commit.

## 3.7 Agilent + SciEX (future phase — research ingested 2026-06-21)

Source: `BRFP/docs/research/agilent-sciex-converter-handoff.md`. Neither vendor has an open format
or redistributable parser; full fidelity needs proprietary **Windows/.NET vendor SDKs** (Agilent
**MHDAC**, mixed-mode → Windows-locked; SciEX **Clearcore2**, managed-only → Windows now, *maybe*
Linux on .NET Core). mzdata/timsrust support neither.

- **Architecture (don't parse natively):** in-process .NET hosting — exactly the pattern we already
  rely on for Thermo (`thermorawfilereader` / netcorehost). A thin per-vendor **C# glue.dll** (.NET 8)
  references MHDAC/Clearcore2 and exposes a flat C-ABI (open, count, spectrum i → mz[]/intensity[]/
  rt/ms_level/precursor/mobility); Rust boots CoreCLR via `netcorehost` and calls the glue. Decoded
  spectra feed the EXISTING `mzpeak_prototyping` writer path — peaks/metadata/chromatograms/
  vendor_tables/aux-config all reused (same as the Bruker arm).
- **Build config (user's directive):** Cargo features **`agilent`** / **`sciex`**, **off by default**;
  enabling them restricts the build to **Windows** (Agilent) / Windows-or-maybe-Linux (SciEX) and
  requires the user to supply the licensed DLLs via an env var (mirroring `BRFP_BAF2SQL_LIB` /
  our `bruker_sdk` posture). **No bundling** — ship glue source only; the vendor EULA (Agilent MHDAC
  redistribution is non-commercial-only) is inherited at runtime and must be documented + accepted.
- **Sourcing the DLLs (user note 2026-06-21):** the vendor DLLs can be taken from a **ProteoWizard
  install** — pwiz bundles MHDAC (`BaseDataAccess`/`MassSpecDataReader`…) and the SciEX Clearcore2
  stack (~27 DLLs) in its `vendor_api/Agilent` and `vendor_api/ABI` directories (and the bundled
  `EULA.MHDAC`). So the glue can point its SDK-path env var at a pwiz dir rather than a separate
  vendor SDK download. The EULA still applies — pwiz redistributes under those same terms; document
  that the user is accepting them. (This also pairs naturally with the `--via-msconvert` lane, which
  uses the *same* pwiz install.)
- **Mapping:** profile/centroid → `spectra_peaks` + `spectra_metadata`; Agilent IM-MS (separate
  **MIDAC** SDK) drift → `mean_inverse_reduced_ion_mobility`; SciEX DAD/UV → `wavelength_spectra`
  (reuse the BAF UV path); vendor metadata (Agilent XML / SciEX wiff2 SQLite) → `vendor_tables/`;
  TIC/BPC → `chromatograms`. NOTE: `--ims-compact`/integer-TOF is **Bruker-TDF-specific** (TOF↔m/z
  calibration) — do NOT assume it transfers to Agilent/SciEX TOF.
- **Build order:** (1) spike — load Clearcore2 from C# on Linux .NET 8 (decides SciEX cross-platform);
  (2) interim `--via-msconvert` lane (shell to ProteoWizard msconvert → mzML → existing mzdata
  ingest) for a quick user-facing win on both vendors; (3) SciEX glue; (4) Agilent glue (+MIDAC).
- **DONE so far:** Cargo features `agilent` / `sciex` declared (off by default, reserved for the
  native glue). **`--via-msconvert` lane IMPLEMENTED** (`convert --via-msconvert [--msconvert-path]`):
  resolves msconvert (flag → `$MSCONVERT_PATH` → PATH) with an actionable not-found error, runs it to
  a temp mzML, then reuses `convert_file`; cleans up. Verified: not-found error, the Agilent/SciEX
  **vendor guard** (`is_agilent_d`/`is_wiff` → guidance to use the lane), and a full stub-msconvert
  run producing a validator-clean mzPeak. (A real msconvert needs ProteoWizard/Windows.)
  **NATIVE GLUE IMPLEMENTED (compile-verified, runtime-untested):** per-vendor in-process .NET
  readers behind `agilent`/`sciex`:
  - Rust: `src/sciex.rs` / `src/agilent.rs` — boot CoreCLR via `netcorehost` (mirroring
    `dotnetrawfilereader-sys/runtime.rs`), call `[UnmanagedCallersOnly]` glue exports as
    `extern "system"` fn pointers, marshal (pointer+len+free) into mzdata spectra like `bruker_tsf`.
    **Compile clean** under `--features sciex` / `agilent` / both / +bruker_sdk.
  - C# glue: `glue/sciex/` + `glue/agilent/` — **reflection-only** over Clearcore2 / MHDAC (no
    compile-time vendor refs), so they **`dotnet build` on macOS** without the DLLs. DLLs sourced at
    runtime from `$MZPC_PWIZ_DIR` (pwiz `vendor_api/ABI` & `/Agilent`); glue dir via
    `$MZPC_SCIEX_GLUE` / `$MZPC_AGILENT_GLUE`.
  - Wiring: feature-gated `mod` + dispatch (`is_wiff`/`is_agilent_d` → `convert_sciex`/`convert_agilent`
    via a shared `convert_vendor_reader`), `guard_unsupported_vendor` bails only when the feature is OFF.
  - codex (SciEX) + vibe (Agilent) FFI reviews done; fixes applied (exception-guard every export,
    RAII free-guards, pin-table keying, ABI static-layout asserts, UTF-16 paths, RT/intensity).
  **Docker harness added** (`docker/`): `docker/windows/` (Dockerfile + `build-and-test.ps1`) for the
  full native build+convert+validate on a **Windows Docker host** (can't run on macOS — Docker here is
  Linux-only); `docker/linux-sciex-spike/` for the Clearcore2-on-Linux probe (runs anywhere). Both
  mount the pwiz DLLs at runtime via `$MZPC_PWIZ_DIR`; nothing bundled.
  **`LastError` getter DONE** (both glues expose `LastError(buf,cap)`; Rust enriches open/meta/data
  bails with the glue message — compile-verified). **REMAINING:** runtime verification on a Windows
  host (real Agilent/SciEX data + pwiz DLLs); Agilent IM-MS (MIDAC).

## 7. Open questions — status

- **Q1 RESOLVED.** mzdata = TDF only (no BAF/TSF), no native integer-TOF, intensity→f32,
  integrity warn-only. → capability-based readers; native-TOF via vendored mzdata (§3.1).
- **Q2** TOF-grid on Astral: model holds (MR-TOF), but grid must be FITTED from m/z (no native
  bins) — handled by residual-fallback + analyzer gating (§3.2). Now in scope (user directive).
- **Q3 RESOLVED.** STORED-ZIP for facets, zstd inside Parquet + numpress on m/z, gzip only for
  opaque `vendor/` blobs (with `content_encoding` recorded). Enforced in CI.
- **Q4 RESOLVED.** Workspace + per-platform feature gating for vendor SDK/FFI.
- **Q5** Spec extensions opt-in behind `--experimental-extensions`; baseline stays spec-compliant.
