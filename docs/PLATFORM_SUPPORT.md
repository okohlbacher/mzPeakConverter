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
| Agilent `.d` (non-IM, native) | ❌ | ❌ | ✅ (scan data; MRM/SIM-only runs refused) | out-of-process **net48** host (`AgilentGlueHost.exe`) → MHDAC, `AGL2` file protocol | MHDAC DLLs (ProteoWizard), .NET Framework 4.8 |
| Agilent `.d` IM-MS (6560) | ❌ | ❌ | ❌ (refused: no MIDAC reader) | `--via-msconvert` only | a ProteoWizard install |
| Agilent `.d` **profile** (`--agilent-grid`) | ⚠️ | ⚠️ | ⚠️ | pure Rust (reads `MSProfile.bin`) | — (two known decode gaps, below) |
| SciEX `.wiff` (native) | ❌ | ❌ | ✅ | in-process .NET glue (`SciexGlue.dll`) → Clearcore2 | Clearcore2 DLLs |
| Shimadzu `.lcd` (native) | ❌ | ❌ | ✅ | in-process .NET glue (`ShimadzuGlue.dll`) → LabSolutions.IO | LabSolutions.IO DLLs from a **current** ProteoWizard |
| Waters `.raw` (native) | ❌ | ❌ | ✅ | `libloading` → `MassLynxRaw.dll` C exports (no .NET glue) | MassLynx/pwiz DLLs |
| **anything** via ProteoWizard | ✅ | ✅ | ✅ | `--via-msconvert` subprocess | a ProteoWizard install (Wine off-Windows) |

✅ native on that OS · ⚠️ partial, see the note · ⛔ present in the tree but not connected ·
❌ not native (use `--via-msconvert`). The compile-time gates are
`#[cfg(windows)]` (Agilent/SciEX/Waters) and `#[cfg(any(windows, target_os = "linux"))]`
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
  `MZPC_PWIZ_DIR`) at the directory holding that DLL.
- **Agilent (MHDAC) — ✅ out-of-process since 0.11.0.** MHDAC is a **.NET Framework 4.x**
  assembly set whose `OpenDataFile` calls `Delegate.BeginInvoke`, permanently unsupported on
  .NET Core/5+, so it cannot be hosted in-process under .NET 8. The converter therefore spawns
  `AgilentGlueHost.exe` (net48, `glue/agilent`) once per `.d`; the host reads every scan through
  MHDAC (reflection only) and writes one `AGL2` file — scan types, instrument identity, an
  offset table, then per-scan records — that `src/agilent.rs` reads back through the
  host-testable parser in `src/agl.rs`. The subprocess reader shipped at cc8245e (2026-06-27),
  was dropped by merge 5a62b90 the next day in favour of an in-process design the host no longer
  implemented, and was restored on 2026-09-06 (owner decision, option A). Verified on the box
  against the msconvert lane: a 5977B GC-MS run (7,017 scans) is identical spectrum for spectrum
  (same points, same TIC to the last digit, RT to 1e-13); a 6545 Q-TOF profile run (1,502 scans,
  181 M points, 242 MB `.d`) converts in 20 s with the same points and RT. **MRM/SIM-only runs
  are refused** with a pointer to `--via-msconvert`: MHDAC hands a 6490 dMRM `.d` over as one
  one-point "MS2 spectrum" per dwell (27,674 of them for MTBLS243) while the data are the 113
  transition chromatograms the msconvert lane writes; the guard reads MHDAC's `ScanTypes`, so
  the corpus harness falls back to msconvert for those units. Not carried yet: precursor
  metadata for MS2 scans (the lane warns once per run when it writes MS2 rows without one), MRM
  chromatograms (by design), the flight-time grid (`--tof-grid` is not applied on this lane; m/z
  is the f64 MHDAC returns, numpress-chunked by default).
  Cost model: the host materialises the whole run into a temp file at 16 B/point before the
  first spectrum is read (~3 GB for the 242 MB Q-TOF run), removed on close and by the panic
  hook (a Ctrl+C, which ends both processes, still leaves it). The host runs under a deadline
  (`MZPC_AGILENT_HOST_TIMEOUT`, default two hours) inside a kill-on-close Job Object, so a stuck
  host is killed and a killed converter does not leave it running.

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
| Agilent (MHDAC) | `glue/agilent` (**net48**, out-of-process) | `bin/Release/net48/AgilentGlueHost.exe` | `MZPC_AGILENT_GLUE` |
| SciEX (Clearcore2) | `glue/sciex` (net8) | `bin/Release/net8.0/SciexGlue.dll` | `MZPC_SCIEX_GLUE` |
| Shimadzu (LabSolutions.IO) | `glue/shimadzu` (net8) | `bin/Release/net8.0/ShimadzuGlue.dll` | `MZPC_SHIMADZU_GLUE` |

```sh
dotnet build glue/sciex/SciexGlue.csproj      -c Release   # → SciexGlue.dll
dotnet build glue/shimadzu/ShimadzuGlue.csproj -c Release   # → ShimadzuGlue.dll
dotnet build glue/agilent/AgilentGlue.csproj   -c Release   # → AgilentGlueHost.exe (net48)
```

**In a release archive** these three builds ship under `glue\<name>\` (`sciex`, `shimadzu`,
`agilent`) beside `mzpeak-convert.exe`, and the converter looks there whenever the
variable is unset — so an unpacked Windows release needs no glue variables (releases after 0.11.5;
0.11.5's archive needs them set). The variable, when set, always wins.

**.NET 8 support ends on 2026-11-10** (Microsoft's policy; .NET 9 ends the same day, .NET 10 on
2028-11-14). The SciEX and Shimadzu glues target `net8.0` and stay there for now, so after that date
they run on an out-of-support runtime. Shimadzu cannot simply move up: its vendor library
deserialises through `BinaryFormatter`, which throws on .NET 9 and later unless Microsoft's
unsupported compatibility package is added, untested inside a glue like this one (see
[`glue/shimadzu/README.md`](../glue/shimadzu/README.md)). The Agilent host is .NET Framework 4.8
and is not affected.

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
- **`release.yml`** — the release archives: macOS arm64 + x86_64, Linux x86_64 + aarch64 in
  `manylinux_2_28` (the glibc 2.28 floor is asserted on the binary), Windows x86_64 + ARM64 with
  the glue, each built and smoke-converted natively on its own architecture. A pull request that
  edits the workflow runs the whole matrix as a dry run.
- **`windows.yml`** — Windows: build with the native vendor readers; build the glues `src/`
  loads (`glue/sciex`, `glue/shimadzu` and the Agilent net48 host) and verify each artifact is
  produced (for Shimadzu also that the generated runtimeconfig carries
  `EnableUnsafeBinaryFormatterSerialization=true`, the switch whose absence broke 0.9.11); run the
  tests after the glue builds; open the Shimadzu glue again after a reader was dropped, in a
  process of its own and without vendor DLLs (hostfxr refuses that second start once the first
  runtime was freed, which broke every verbose Shimadzu conversion before 0446ea3);
  smoke-convert the fixture; and (separate jobs) exercise the `--via-msconvert` lane and a real
  timsTOF ion-mobility comparison.
