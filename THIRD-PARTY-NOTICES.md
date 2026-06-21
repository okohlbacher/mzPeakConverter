# Third-Party Notices

mzPeakConverter is distributed under the [MIT License](LICENSE). It builds on
third-party components, acknowledged here. A complete machine-readable inventory
of every transitive dependency is in [`sbom.cdx.json`](sbom.cdx.json) (CycloneDX
1.5, regenerable with `tools/gen_sbom.py`).

## Vendored source — requires attention

| Component | Location | License |
|---|---|---|
| **`mzpeak_prototyping`** | `vendor/mzpeak_prototyping/` | **not declared upstream** |

`mzpeak_prototyping` is the HUPO-PSI reference mzPeak writer by Joshua Klein
(<https://github.com/mobiusklein>). A snapshot is **vendored** into this
repository because the converter extends it (the native integer-TOF seam, see
[`NATIVE-TOF-DESIGN.md`](NATIVE-TOF-DESIGN.md)) and pins it exactly to the
arrow/parquet/mzdata version graph.

> ⚠️ The upstream crate does not currently ship an explicit license file. It is
> redistributed here in good faith as the public PSI reference implementation;
> its terms remain those of the upstream author. If you redistribute
> mzPeakConverter, confirm the upstream license, or replace the vendored path
> dependency with an upstream git/crates.io dependency.

## Key runtime dependencies (crates.io)

| Crate | Role | License |
|---|---|---|
| [`mzdata`](https://github.com/mobiusklein/mzdata) | format readers (mzML/imzML/Thermo/TDF) | MIT |
| [`mzpeaks`](https://github.com/mobiusklein/mzpeaks) | peak/centroid models | MIT |
| [`arrow`](https://github.com/apache/arrow-rs), [`parquet`](https://github.com/apache/arrow-rs) | columnar storage | Apache-2.0 |
| [`timsrust`](https://github.com/MannLabs/timsrust) | native Bruker TDF integer-TOF | Apache-2.0 |
| [`thermorawfilereader`](https://github.com/mobiusklein/dotnetrawfilereader-sys) | Thermo `.raw` via .NET | MIT |
| [`rusqlite`](https://github.com/rusqlite/rusqlite) / [`zstd`](https://github.com/gyscos/zstd-rs) | Bruker TSF reader | MIT |
| [`zip`](https://github.com/zip-rs/zip2), [`flate2`](https://github.com/rust-lang/flate2-rs) | archive + vendor-blob embedding | MIT/Apache-2.0 |
| [`clap`](https://github.com/clap-rs/clap), [`anyhow`](https://github.com/dtolnay/anyhow), [`serde`](https://github.com/serde-rs/serde) | CLI / errors / config | MIT/Apache-2.0 |

## Dependency license distribution (395 components, all features)

All transitive Cargo dependencies are under permissive licenses (MIT, Apache-2.0,
BSD-2/3-Clause, ISC, Zlib, Unicode-3.0, 0BSD, CC0-1.0, Unlicense, or dual/triple
combinations thereof). No copyleft (GPL/AGPL) obligations apply. The two crates
offering an optional `LGPL-2.1-or-later` alternative are also offered under
`MIT OR Apache-2.0`, which this project takes.

## Vendor instrument SDKs (optional, not bundled)

The optional `bruker_sdk` / `agilent` / `sciex` build features call proprietary
vendor libraries (Bruker `libbaf2sql_c`, Agilent MHDAC, SciEX Clearcore2) that
are **not** included in this repository. They are loaded at runtime from a
licensed vendor install (e.g. ProteoWizard). Their licenses are governed by the
respective vendors. The default build links none of them.
