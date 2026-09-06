# AgilentGlueHost — native Agilent MassHunter (`.d`) reader for mzPeakConverter

> **Status (0.11.0): wired and verified.** `src/agilent.rs` spawns this EXE once per `.d` and
> reads its `AGL2` output through the host-testable parser `src/agl.rs`. Verified on the Windows
> box against the msconvert lane: a 5977B GC-MS run (7,017 scans) is identical spectrum for
> spectrum, a 6545 Q-TOF profile run (1,502 scans, 181 M points) has the same points, RT and
> intensities. History: the subprocess reader shipped at cc8245e (2026-06-27), was dropped by
> merge 5a62b90 the next day, and was restored on 2026-09-06 (owner decision, option A).
>
> **Scope guard.** MRM/SIM-only runs (a 6490 dMRM `.d`) are refused by the converter with a
> pointer to `--via-msconvert`: MHDAC presents each dwell as a one-point "MS2 spectrum" while the
> data are the transition chromatograms the msconvert lane writes. The host reports MHDAC's
> `ScanTypes` for exactly that decision.

A small **.NET Framework 4.8 console EXE** (`AgilentGlueHost.exe`) that lets a Rust `agilent`
reader read Agilent MassHunter `.d` data through Agilent's **MHDAC** (MassHunter Data Access
Component) DLLs — **out of process**. The Rust side spawns it once per `.d`.

**Status: builds everywhere; runs on Windows (see the banner).** It *builds* on
macOS/Linux/Windows (no Agilent DLLs needed at build time — see "Why reflection"), and *runs* on x64
Windows with the MHDAC DLLs present. Reflection names were validated against MHDAC `10.0.1.10305`
(one version).

## Why a separate .NET Framework process (and not in-process .NET 8)

The first design hosted MHDAC **in-process** under .NET 8 (via `netcorehost`/hostfxr). That can never
work: MHDAC was built for **.NET Framework 4.x** and, inside `MassSpecDataReader.OpenDataFile`, calls
the legacy `Delegate.BeginInvoke` async pattern (`DataFileMgr.ReadNonMSInfoDelegate.BeginInvoke`).
`Delegate.BeginInvoke`/`EndInvoke` are **permanently unsupported on .NET Core / .NET 5+** — they throw
`PlatformNotSupportedException`, with no opt-in flag (unlike `BinaryFormatter`). The call is internal
to `OpenDataFile`, so it can't be avoided through MHDAC's public API.

The fix is to run MHDAC under the runtime it was built for. `AgilentGlueHost.exe` is a **net48** EXE;
the .NET Framework 4.8 runtime ships with Windows, so it just runs. Rust drives it as a subprocess.

## How it works (one-shot, file-based)

```
AgilentGlueHost.exe  <in.d>  <mhdacDir>  <out.bin>
```

It opens the `.d` via MHDAC, reads every MS scan, and writes a little-endian binary file that
`src/agilent.rs` reads back (`src/agl.rs` is the parser, with tests that run on every host):

```
magic "AGL2" (4 bytes) | count u64 |
scanTypes: len u32 + UTF-8   (MSScanFileInformation.ScanTypes, e.g. "Scan" or "MultipleReaction, SelectedIon")
device:    len u32 + UTF-8   ("<DeviceType>" U+001F "<device name>" U+001F "<serial>", parts empty when unreadable)
offset[count] u64            (abs file offset of each record)
then per record:
  rt f64 | msLevel i32 | polarity i32 | isCentroid i32 | scanId i32 |
  nPoints u64 | mz[nPoints] f64 | intensity[nPoints] f64
```

`AGL1` (0.9.x) had no strings; a converter given one asks for a rebuilt host. The whole run is
materialised before the first spectrum is read — 16 B/point, about 3 GB for a 240 MB Q-TOF `.d` —
and the file is removed when the reader closes.

Exit 0 on success; non-zero with one diagnostic line on **stderr** on failure (and `out.bin` is
removed). stdout is left clean. The Rust side seeks per-record via the offset table, so spectra are
read on demand without holding them all in memory.

## Why reflection (no compile-time reference)

MHDAC is a Windows-only mixed-mode assembly set with a restrictive license; it cannot be committed or
referenced on a build box that doesn't have it. So `Glue.cs` touches **no** MHDAC type at compile
time — every call goes through `System.Reflection`. MHDAC also spreads its types across several
assemblies (`MassSpecDataReader.dll`, `BaseDataAccess.dll`, `BaseCommon.dll`, …) and which assembly
owns a given interface varies by version, so the host searches the Agilent assemblies in `<mhdacDir>`
rather than hard-coding the owner. The reader API (`OpenDataFile`, `GetSpectrum`, `GetScanRecord`,
`MSScanFileInformation`) is exposed as **explicit `IMsdrDataReader` interface implementations** —
invisible on the concrete `MassSpecDataReader` — so it's resolved from the interface and invoked on
the concrete instance. Net result: `dotnet build` works anywhere a .NET SDK is installed; the DLLs
are required only at run time.

## Build

```sh
dotnet build glue/agilent/AgilentGlue.csproj -c Release
```

Output: `bin/Release/net48/AgilentGlueHost.exe`. `Microsoft.NETFramework.ReferenceAssemblies`
(a `PackageReference`) supplies the net48 reference assemblies, so this cross-compiles with the
ordinary `dotnet` SDK — no Visual Studio / .NET Framework targeting pack required.

## Run-time configuration

| Env var             | Meaning                                                                              |
|---------------------|--------------------------------------------------------------------------------------|
| `MZPC_AGILENT_GLUE` | Directory containing `AgilentGlueHost.exe` (the build output above). |
| `MZPC_PWIZ_DIR`     | A ProteoWizard install directory. MHDAC DLLs are loaded from `<MZPC_PWIZ_DIR>/vendor_api/Agilent` when that subdirectory exists, else from `<MZPC_PWIZ_DIR>` itself (the 3.0.26175 installer is flat). |

> **Layouts.** Bundled ProteoWizard trees keep `vendor_api/Agilent/`; the standalone installer
> flattens the DLLs beside `msconvert.exe`. The converter probes both (`pwiz_layout::agilent_dll_dir`)
> and hands the host the directory that holds `MassSpecDataReader.dll` — no junction needed.

## Sourcing the MHDAC DLLs (from ProteoWizard)

ProteoWizard bundles the Agilent MHDAC assemblies (`MassSpecDataReader.dll` + `BaseCommon.dll`,
`BaseDataAccess.dll`, `BaseError.dll`, `agtsampleinforw.dll`, …). The host loads
`MassSpecDataReader.dll` via `Assembly.LoadFrom` and registers an `AssemblyResolve` handler so the
siblings resolve from the same directory.

> **License note.** The Agilent MHDAC redistributable is licensed for **non-commercial use only**.
> Do not redistribute the DLLs with this project. Obtain them from your own ProteoWizard install,
> which carries Agilent's EULA. This host contains none of Agilent's code — only reflection calls
> against DLLs you supply.

## Scope

Non-IM MS only (MS1/MS2, profile or centroid). Agilent ion-mobility (6560 IM-QTOF) requires the
separate **MIDAC** SDK to read the drift dimension and is **out of scope** here (the MIDAC glue in
`src/agilent_midac.rs` is still the in-process .NET 8 design and would hit the same `BeginInvoke`
wall — port it to this out-of-process net48 pattern when IM-MS support is needed).
