# AgilentGlueHost — native Agilent MassHunter (`.d`) reader for mzPeakConverter

A small **.NET Framework 4.8 console EXE** (`AgilentGlueHost.exe`) that lets the Rust `agilent`
reader read Agilent MassHunter `.d` data through Agilent's **MHDAC** (MassHunter Data Access
Component) DLLs — **out of process**. The Rust converter spawns it once per `.d`.

**Status: Windows-runtime-only; runs.** It *builds* on macOS/Linux/Windows (no Agilent DLLs needed at
build time — see "Why reflection"), and *runs* on x64 Windows with the MHDAC DLLs present. Verified
against MHDAC `10.0.1.10305`.

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

It opens the `.d` via MHDAC, reads every MS scan, and writes a little-endian binary file that the
Rust side (`src/agilent.rs`) reads back:

```
magic "AGL1" (4 bytes) | count u64 | offset[count] u64 (abs file offset of each record)
then per record:
  rt f64 | msLevel i32 | polarity i32 | isCentroid i32 | scanId i32 |
  nPoints u64 | mz[nPoints] f64 | intensity[nPoints] f64
```

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
| `MZPC_AGILENT_GLUE` | Directory containing `AgilentGlueHost.exe` (the build output above).                  |
| `MZPC_PWIZ_DIR`     | A ProteoWizard install directory. MHDAC DLLs are loaded from `<MZPC_PWIZ_DIR>/vendor_api/Agilent`. |

> **Box note.** Some ProteoWizard builds (e.g. the FLASHApp bundle) flatten the Agilent DLLs directly
> into `pwiz-bin/` instead of a `vendor_api/Agilent/` subdir. In that case create a directory junction
> `pwiz-bin/vendor_api/Agilent → pwiz-bin` so `<MZPC_PWIZ_DIR>/vendor_api/Agilent` resolves.

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
