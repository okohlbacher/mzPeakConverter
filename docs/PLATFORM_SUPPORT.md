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
| Agilent `.d` (non-IM, native) | ❌ | ❌ | ⛔ **not wired** | see note below | — |
| Agilent `.d` IM-MS (6560, native) | ❌ | ❌ | ⚠️ scaffold | in-process .NET glue → MIDAC | MIDAC DLLs |
| Agilent `.d` **profile** (`--agilent-grid`) | ⚠️ | ⚠️ | ⚠️ | pure Rust (reads `MSProfile.bin`) | — (two known decode gaps, below) |
| SciEX `.wiff` (native) | ❌ | ❌ | ✅ | in-process .NET glue (`SciexGlue.dll`) → Clearcore2 | Clearcore2 DLLs |
| Shimadzu `.lcd` (native) | ❌ | ❌ | ✅ | in-process .NET glue (`ShimadzuGlue.dll`) → LabSolutions.IO | LabSolutions.IO DLLs from a **current** ProteoWizard |
| Waters `.raw` (native) | ❌ | ❌ | ✅ | `libloading` → `MassLynxRaw.dll` C exports (no .NET glue) | MassLynx/pwiz DLLs |
| **anything** via ProteoWizard | ✅ | ✅ | ✅ | `--via-msconvert` subprocess | a ProteoWizard install (Wine off-Windows) |

✅ native on that OS · ⚠️ partial, see the note · ⛔ present in the tree but not connected ·
❌ not native (use `--via-msconvert`). The compile-time gates are
`#[cfg(windows)]` (Agilent/MIDAC/SciEX/Waters) and `#[cfg(any(windows, target_os = "linux"))]`
(BAF, timsdata SDK) in `src/main.rs`; macOS gets none of those.

## Why the platform split

- **Pure-Rust readers** (mzML, imzML, Bruker TDF/TSF, Agilent profile grid) build and run
  everywhere — no SDK, no runtime.
- **Thermo `.raw`** uses a managed .NET reader that runs on any OS with a **.NET 8+** runtime
  (the binary sets `DOTNET_ROLL_FORWARD=LatestMajor`; the first build downloads `nethost`).
- **Bruker BAF / timsdata SDK** need Bruker's native libraries, which exist for **Linux and
  Windows only** — hence no macOS.
- **SciEX (Clearcore2)** is a Windows-only managed SDK, hosted **in-process** via a small
  reflection-only .NET glue (`glue/sciex`).
- **Waters (MassLynx)** needs no glue at all: `MassLynxRaw.dll` exposes a plain C ABI, which
  `src/waters.rs` loads with `libloading` and calls directly. Point `MZPC_MASSLYNX_DIR` (or
  `MZPC_PWIZ_DIR`) at the directory holding that DLL. The `glue/waters` C# project is a
  never-wired alternative to this lane — no code path reads it or `MZPC_WATERS_GLUE`.
- **Agilent (MHDAC) — ⛔ the two halves do not meet, so this lane opens nothing.** MHDAC is a
  **.NET Framework 4.x** assembly set whose `OpenDataFile` calls `Delegate.BeginInvoke`,
  permanently unsupported on .NET Core/5+, so it cannot be hosted in-process under .NET 8. The
  C# side was accordingly rewritten as a **separate net48 executable** (`AgilentGlueHost.exe`,
  `OutputType=Exe`, no `[UnmanagedCallersOnly]` exports, speaking an `AGL1` file protocol) — but
  `src/agilent.rs` was never adapted: it still requires `AgilentGlue.dll` +
  `AgilentGlue.runtimeconfig.json` and resolves six exports from `AgilentGlue.Exports`, none of
  which the current project produces, and nothing in `src/` spawns the EXE or reads `AGL1`.
  Every entry point (`convert`, `--to mzml`, `inspect`) therefore fails at open on Windows, loudly.
  Use `--via-msconvert` meanwhile. Revive-or-delete is tracked in `BACKLOG.md`; see also
  [`glue/agilent/README.md`](../glue/agilent/README.md).

- **Agilent profile (`--agilent-grid`) — ⚠️ pure Rust, two known decode gaps.** Neither
  profile-bearing `.d` in the project corpus converts today: one fails LZF decompression of an
  `MSProfile.bin` segment (`LZF: back-reference before output start`), the other has an IM-QTOF
  `MSScan.xsd` with no `SpectrumParamsType`, which the schema walk rejects. Files outside those
  two shapes are expected to work; there is no corpus coverage proving it.

## The .NET glue executables (Windows)

The SDK-backed vendor readers do **not** link into the Rust binary; they use a small .NET glue
that touches the vendor types only through reflection (so the glue **builds without the vendor
DLLs**, on any OS with a .NET SDK). Build each once and point the converter at it:

| Glue | Project | Build output | Env var |
|---|---|---|---|
| Agilent (MHDAC) ⛔ | `glue/agilent` (**net48**) | `bin/Release/net48/AgilentGlueHost.exe` | `MZPC_AGILENT_GLUE` (read, but the lane cannot open a file — see above) |
| Agilent IM (MIDAC) | `glue/agilent_midac` (net8) | `bin/Release/net8.0/AgilentMidacGlue.dll` | `MZPC_AGILENT_MIDAC_GLUE` |
| SciEX (Clearcore2) | `glue/sciex` (net8) | `bin/Release/net8.0/SciexGlue.dll` | `MZPC_SCIEX_GLUE` |
| Shimadzu (LabSolutions.IO) | `glue/shimadzu` (net8) | `bin/Release/net8.0/ShimadzuGlue.dll` | `MZPC_SHIMADZU_GLUE` |

```sh
dotnet build glue/sciex/SciexGlue.csproj      -c Release   # → SciexGlue.dll
dotnet build glue/shimadzu/ShimadzuGlue.csproj -c Release   # → ShimadzuGlue.dll
# glue/agilent builds AgilentGlueHost.exe, but no code path launches it yet — see above.
```

The vendor DLLs themselves are sourced at **runtime** from a ProteoWizard install
(`MZPC_PWIZ_DIR`); both layouts are probed — `<pwiz>/vendor_api/<Vendor>` as the bundled
builds arrange it, and flat beside `msconvert.exe` as the standalone installer does (Shimadzu's
DLL is always flat). They carry vendor EULAs and are never committed. See each
`glue/*/README.md` for the per-vendor specifics.

**Use a current ProteoWizard** — 3.0.26151 is verified. Older trees ship
`Shimadzu.LabSolutions.IO.IoModule.dll` **3.8.4.6016**, which mispairs centroid intensities on
profile-less `.lcd` files; 5.0.0.0 does not. The known-stale source is the FLASHApp/OpenMS
third-party bundle (ProteoWizard 3.0.22187, July 2022). See
[`glue/shimadzu/README.md`](../glue/shimadzu/README.md).

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
