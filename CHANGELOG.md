# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/), and the project adheres to
[Semantic Versioning](https://semver.org/).

## [0.1.0] — 2026-06-21

First public release.

### Added

- **`convert`** — unified conversion to mzPeak (HUPO-PSI v0.9) for:
  - mzML / `.mzML.gz`
  - imzML (imaging coordinate columns + IMS CV promoted)
  - Bruker `.d` **TDF** (timsTOF) with ion mobility preserved
  - Bruker `.d` **TSF** (MALDI/line spectra; ported rusqlite + zstd reader)
  - Thermo `.raw` (via a self-hosted .NET runtime) with a verbatim
    `vendor_scan_trailers` facet (+ wide + status-log)
- **Signal layout** options: `chunked` (numpress-linear default, or lossless
  delta via `--no-numpress`) and `point`; configurable `--chunk-size`,
  `--zstd-level`.
- **`--ims-compact`** (Bruker TDF) — store the lossless native integer-`tof`
  signal in `spectra_peaks` (+ `ims_calibration` in the index) instead of f64
  m/z; ~50 % smaller, bit-exact TOF grid. Standalone **`ims-compact`**
  subcommand encodes a bare Parquet and streams one frame at a time (constant
  memory) with an independent lossless re-read verification.
- **Vendor side-file embedding** (`vendor/` in the archive): preserve-by-default,
  gzipped, declared `proprietary`; YAML policy via `--config`, per-glob override
  via `--aux`, opt-out via `--no-vendor`.
- **`--via-msconvert`** — cross-vendor interim path through ProteoWizard
  `msconvert` (Agilent `.d`, SciEX `.wiff`, and anything msconvert reads).
- **`inspect`** (with `--json`), **`validate`** (wraps `mzpeak-validate`),
  **`tof-grid-probe`** / **`tof-grid`** (P5 TOF-grid feasibility spike).
- **`--verify`** round-trip check and **`--validate`** hook.
- Stable exit codes: `0` ok, `1` generic, `3` unsupported, `5` verify/validate
  failure.
- Optional, off-by-default build features for native vendor SDK readers:
  `bruker_sdk` (BAF), `agilent` (MHDAC), `sciex` (Clearcore2) — Windows-runtime,
  compile-verified.
- End-to-end corpus harness (`tests/run_corpus_e2e.sh`) and a full-data sweep
  runner (`tests/run_data_sweep.sh`).
- Documentation: README, [user manual](docs/USER_MANUAL.md), architecture
  ([PLAN.md](PLAN.md)), native-TOF design ([NATIVE-TOF-DESIGN.md](NATIVE-TOF-DESIGN.md)),
  CycloneDX SBOM, and third-party notices.

### Known limitations

- Native Agilent/SciEX/BAF readers are compile-verified but not yet
  runtime-tested (require a Windows host + licensed vendor DLLs).
- UV/PDA (non-MS) spectra in some mzML files are not carried into the archive.
- Thermo instrument error-log facet and the registered tof→m/z column transform
  are deferred pending upstream API support (see [HANDOFF.md](HANDOFF.md)).

[0.1.0]: https://github.com/okohlbacher/mzPeakConverter/releases/tag/v0.1.0
