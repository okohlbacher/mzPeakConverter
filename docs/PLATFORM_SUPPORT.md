# Vendor format support by platform

Which input formats `mzpeak-convert` can read **natively** depends on the OS, because the
vendor SDKs are platform-specific. This page is the authoritative matrix; the README has a
condensed version. Anything a platform can't read natively is still reachable through
ProteoWizard with `--via-msconvert` (all platforms).

## Matrix

| Format | Linux | macOS | Windows | Reader / mechanism | Runtime requirement |
|---|:---:|:---:|:---:|---|---|
| mzML, `.mzML.gz` | ✅ | ✅ | ✅ | pure Rust | — |
| imzML (+ `.ibd`) | ✅ | ✅ | ✅ | pure Rust | — |
| Bruker `.d` **TDF** (timsTOF) | ✅ | ✅ | ✅ | pure Rust (`timsrust`); ims-compact default | — |
| Bruker `.d` **TSF** (line spectra) | ✅ | ✅ | ✅ | pure Rust (`timsrust`) | — |
| Thermo `.raw` | ✅ | ✅ | ✅ | `dotnetrawfilereader` (managed, in-process) | **.NET 8+ runtime** |
| Bruker `.d` **BAF** | ✅ | ❌ | ✅ | `libbaf2sql_c` (native C, in-process) | `libbaf2sql_c` at runtime |
| Bruker `.d` via **timsdata SDK** (`--bruker-sdk`) | ✅ | ❌ | ✅ | Bruker `timsdata` lib (opt-in) | `libtimsdata.so`/`.dll` via `TIMSDATA_LIB_DIR` |
| Agilent `.d` (non-IM, native) | ❌ | ❌ | ✅ | **out-of-process net48 EXE** (`AgilentGlueHost.exe`) → MHDAC | **.NET Framework 4.8** + MHDAC DLLs |
| Agilent `.d` IM-MS (6560, native) | ❌ | ❌ | ✅ | in-process .NET glue → MIDAC (scaffold) | MIDAC DLLs |
| Agilent `.d` **profile** (`--agilent-grid`) | ✅ | ✅ | ✅ | pure Rust (reads `MSProfile.bin`) | — |
| SciEX `.wiff` (native) | ❌ | ❌ | ✅ | in-process .NET glue (`SciexGlue.dll`) → Clearcore2 | Clearcore2 DLLs |
| Waters `.raw` (native) | ❌ | ❌ | ✅ | in-process .NET glue (`WatersGlue.dll`) → MassLynx | MassLynx/pwiz DLLs |
| **anything** via ProteoWizard | ✅ | ✅ | ✅ | `--via-msconvert` subprocess | a ProteoWizard install (Wine off-Windows) |

✅ native on that OS · ❌ not native (use `--via-msconvert`). The compile-time gates are
`#[cfg(windows)]` (Agilent/MIDAC/SciEX/Waters) and `#[cfg(any(windows, target_os = "linux"))]`
(BAF, timsdata SDK) in `src/main.rs`; macOS gets none of those.

## Why the platform split

- **Pure-Rust readers** (mzML, imzML, Bruker TDF/TSF, Agilent profile grid) build and run
  everywhere — no SDK, no runtime.
- **Thermo `.raw`** uses a managed .NET reader that runs on any OS with a **.NET 8+** runtime
  (the binary sets `DOTNET_ROLL_FORWARD=LatestMajor`; the first build downloads `nethost`).
- **Bruker BAF / timsdata SDK** need Bruker's native libraries, which exist for **Linux and
  Windows only** — hence no macOS.
- **SciEX (Clearcore2) and Waters (MassLynx)** are Windows-only managed/mixed SDKs, hosted
  **in-process** via a small reflection-only .NET glue (`glue/sciex`, `glue/waters`).
- **Agilent (MHDAC)** is a **.NET Framework 4.x** assembly set whose `OpenDataFile` calls
  `Delegate.BeginInvoke` — permanently unsupported on .NET Core/5+. It therefore cannot be
  hosted in-process under .NET 8 and runs as a **separate net48 executable**
  (`AgilentGlueHost.exe`, built from `glue/agilent`) that the converter spawns per `.d`. See
  [`glue/agilent/README.md`](../glue/agilent/README.md).

## The .NET glue executables (Windows)

The SDK-backed vendor readers do **not** link into the Rust binary; they use a small .NET glue
that touches the vendor types only through reflection (so the glue **builds without the vendor
DLLs**, on any OS with a .NET SDK). Build each once and point the converter at it:

| Glue | Project | Build output | Env var |
|---|---|---|---|
| Agilent (MHDAC) | `glue/agilent` (**net48**) | `bin/Release/net48/AgilentGlueHost.exe` | `MZPC_AGILENT_GLUE` |
| Agilent IM (MIDAC) | `glue/agilent_midac` (net8) | `bin/Release/net8.0/AgilentMidacGlue.dll` | `MZPC_AGILENT_MIDAC_GLUE` |
| SciEX (Clearcore2) | `glue/sciex` (net8) | `bin/Release/net8.0/SciexGlue.dll` | `MZPC_SCIEX_GLUE` |
| Waters (MassLynx) | `glue/waters` (net8) | `bin/Release/net8.0/WatersGlue.dll` | `MZPC_WATERS_GLUE` |

```sh
dotnet build glue/sciex/SciexGlue.csproj      -c Release   # → SciexGlue.dll
dotnet build glue/waters/WatersGlue.csproj    -c Release   # → WatersGlue.dll
dotnet build glue/agilent/AgilentGlue.csproj  -c Release   # → AgilentGlueHost.exe (net48)
```

The vendor DLLs themselves are sourced at **runtime** from a ProteoWizard install
(`MZPC_PWIZ_DIR` → `<pwiz>/vendor_api/<Vendor>`); they carry vendor EULAs and are never
committed. See each `glue/*/README.md` for the per-vendor specifics.

## CI coverage

The matrix above is exercised by CI (`.github/workflows/`):

- **`ci.yml`** — Linux **and** macOS: build the default features, run the test suite, and
  smoke-convert the committed `tests/fixtures/tiny.pwiz.1.1.mzML`. On Linux the BAF/timsdata
  readers compile in; on macOS they're correctly excluded. (Optional licensed-SDK e2e runs
  only when a runner provides the SDK + sample data.)
- **`windows.yml`** — Windows: build with the native vendor readers, run tests, build **all
  four .NET glue executables/assemblies** and verify each artifact is produced, smoke-convert
  the fixture, and (separate jobs) exercise the `--via-msconvert` lane and a real timsTOF
  ion-mobility comparison.
