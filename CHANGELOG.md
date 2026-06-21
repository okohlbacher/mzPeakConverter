# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/), and the project adheres to
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Changed — single-command CLI (breaking)

- The tool is now **one command** — `mzpeak-convert <input> [-o <output>] [options]`.
  The `convert`, `inspect`, `ims-compact`, `tof-grid-probe`, and `tof-grid`
  subcommands are removed.
  - **No `--output`** → nothing is written; the input is inspected and a report
    is printed (the former `inspect`).
  - **`-v`** prints that inspection report *and* still converts.
  - **ims-compact** is now an option, **on by default for Bruker timsTOF (TDF)**;
    disable with `--no-ims-compact`. The standalone bare-Parquet encoder is gone.
  - `tof-grid`/`tof-grid-probe` (a measured no-go research spike) are removed.
- **`--config` is now a general configuration file** holding *any* overridable
  option (not just vendor side-file policy). Precedence: CLI flag > config > default.
- SQLite is compiled from source (`rusqlite` `bundled`) — self-contained build on
  all platforms (no system libsqlite3).

### Added

- Windows CI: builds default + all vendor-SDK features, the C# glues, smoke-converts,
  and (separately) installs ProteoWizard from TeamCity to exercise `--via-msconvert`.

## [0.2.0] — 2026-06-21

### Changed

- **Removed the built-in conformance validation** (`validate` subcommand and
  `convert --validate`). Validation is delegated to the independent
  `mzpeak-validate` tool; `--verify` (round-trip fidelity) stays. Exit codes are
  now `0`/`1`/`3` (the old `5` is gone). **Breaking** for anyone scripting the
  `validate` subcommand.
- Documentation now states prominently that the mzPeak format is still in the
  HUPO-PSI specification process (draft v0.9) and this converter is a technical
  demonstrator, not a production tool. Added references to mzpeak.org, the
  HUPO-PSI/mzPeak-specification repo, and the in-browser viewer at mzpeak.org/view.

### Added

- Bare `ims-compact` encoder now streams one frame at a time (constant memory)
  with an independent streaming lossless re-read.
- Unsupported vendor inputs now exit `3` (typed `UnsupportedVendor` error).

### Fixed

- Collapsed three byte-identical `convert_*` writer bodies into one shared path.
- Guard the archive ims-compact TOF cast against i32 overflow.
- Agilent glue export used a non-blittable `char*` across the FFI boundary
  (would mis-marshal on Windows); switched to `ushort*` like the SciEX glue.
- `gen_sbom.py` null-root crash + legacy `/` SPDX normalization; sweep-script id
  sanitization.

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
- **`inspect`** (with `--json`) and **`tof-grid-probe`** / **`tof-grid`** (P5
  TOF-grid feasibility spike).
- **`--verify`** round-trip fidelity check (conformance validation is left to the
  independent `mzpeak-validate` tool).
- Stable exit codes: `0` ok, `1` generic, `3` unsupported.
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

[0.2.0]: https://github.com/okohlbacher/mzPeakConverter/releases/tag/v0.2.0
[0.1.0]: https://github.com/okohlbacher/mzPeakConverter/releases/tag/v0.1.0
