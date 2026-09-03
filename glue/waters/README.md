# WatersGlue — native Waters `.raw` reader shim

> ## ⛔ NOT WIRED — nothing in the converter loads this project.
>
> The Waters lane needs **no glue at all**: `MassLynxRaw.dll` exposes a plain C ABI, and
> `src/waters.rs` loads it directly with `libloading` from `MZPC_MASSLYNX_DIR` (or
> `MZPC_PWIZ_DIR`). No code path reads `MZPC_WATERS_GLUE` or loads `WatersGlue.dll` — the
> variable and the build output below describe an alternative design that was never connected.
> Building this project is harmless; pointing environment variables at it changes nothing.
> Whether to revive or delete it is an open decision (see `BACKLOG.md`). Everything under
> "Running" is therefore documentation of an unreached path, kept for that decision.

A thin .NET 8 C# library that the mzPeakConverter Rust `waters` path hosts in-process
(via `netcorehost` / CoreCLR) to read Waters MassLynx `.raw` directories through the vendor
**MassLynx SDK** managed API, and exposes the data back to Rust over a tiny C ABI.

> ⚠️ **Windows-runtime-only and currently UNTESTED.** This project *builds* on any platform
> (including macOS) because it has **no compile-time reference** to MassLynx — every vendor
> call is made through runtime reflection. But it only *runs* where the MassLynx managed DLLs
> and a compatible .NET 8 runtime are present. In practice that means Windows.

## How it fits together

```
mzPeakConverter (Rust, native Waters path)
        │  netcorehost: load_hostfxr → initialize_for_runtime_config → delegate loader
        ▼
WatersGlue.dll  (this project)   ── reflection (Assembly.LoadFrom) ──►  MassLynx*.dll
        ▲                                                                  (from SDK)
        │  C ABI: Open / Close / SpectrumCount / SpectrumMeta / GetSpectrum / FreeSpectrum / LastError
        └─ [UnmanagedCallersOnly] static exports in WatersGlue.Exports
```

`src/waters.rs` is the Rust side; it documents the exact ABI. `Glue.cs` is the managed side;
its `Exports` class implements that ABI, and `MassLynxApi` does the reflection.

## Building

```sh
cd glue/waters
dotnet build -c Release          # produces bin/Release/net8.0/WatersGlue.dll (+ runtimeconfig)
```

The build succeeds **without** any MassLynx DLLs present — that is the whole point of the
reflection design. (`dotnet build` emits a `WatersGlue.runtimeconfig.json` next to the DLL,
which `netcorehost` needs to boot the runtime — see `EnableDynamicLoading` in the csproj.)

## Sourcing the MassLynx DLLs

The MassLynx managed assemblies are **proprietary Waters code**. They are *not* shipped with
this project and must not be committed here. Two common sources:

1. The **Waters MassLynx SDK** install (the `MassLynxRaw.dll` / `MassLynx*.dll` managed
   assemblies and their native companions).
2. A **ProteoWizard** install that bundles the Waters vendor API — look under
   `<pwiz-install>\vendor_api\Waters\` for `MassLynxRaw.dll` (+ `cdt.dll` and friends).

## Running (environment variables — only `MZPC_MASSLYNX_DIR` is actually read)

| Variable             | Meaning                                                                       |
| -------------------- | ----------------------------------------------------------------------------- |
| `MZPC_WATERS_GLUE`   | ⛔ **Not read by any code.** Would hold the built `WatersGlue.dll` + `WatersGlue.runtimeconfig.json` (e.g. `glue/waters/bin/Release/net8.0`) if this lane were wired. |
| `MZPC_MASSLYNX_DIR`  | Directory holding the MassLynx managed DLLs. From a ProteoWizard install this is `<MZPC_MASSLYNX_DIR>/vendor_api/Waters` (and the glue accepts `MZPC_MASSLYNX_DIR` itself if it directly contains them). |

A .NET 8 runtime (`Microsoft.NETCore.App` 8.0+) must be installed on the host.

Example (Windows PowerShell):

```powershell
# MZPC_WATERS_GLUE is inert; MZPC_MASSLYNX_DIR is the one that matters.
$env:MZPC_MASSLYNX_DIR  = "C:\Program Files\ProteoWizard\ProteoWizard 3.0.24"
mzpeak-convert convert sample.raw -o sample.mzpeak
```

## EULA / licensing caveat

The MassLynx SDK / Waters vendor API is governed by Waters' (and, if sourced that way,
ProteoWizard's) license terms. By using this glue you are loading and executing Waters
proprietary code that you obtained through your own licensed SDK / ProteoWizard install.
**Do not redistribute the MassLynx DLLs** with mzPeakConverter, and ensure your use complies
with the Waters vendor-API EULA. This project only provides the *bridge*; it ships none of the
vendor binaries.

## Status / caveats

- **Untested.** No Waters `.raw` has been read through this path; the MassLynx managed API
  type/method names are **NEEDS-VALIDATION** (see the `// NEEDS-VALIDATION` markers in
  `Glue.cs`) and must be confirmed on Windows against the actual SDK. The riskiest bindings are
  the `MassLynxRawScanReader.ReadScan` out-param shape and the `MassLynxRawInfoReader` method
  names (`GetFunctionCount` / `GetScansInFunction` / `GetRetentionTime` / `IsContinuum` /
  `GetIonMode`).
- Intensities are narrowed from MassLynx `float`/`double` to `float` to match the mzPeak schema.
- Retention time from MassLynx is in **minutes**; the glue converts to seconds at the ABI, and
  the Rust side converts back to minutes for mzdata. (Net: mzdata gets minutes.)
- Polarity is mapped 0 = positive, 1 = negative, other = unknown (from the ion-mode sign).
- The reader flattens every `(function, scan)` into a single index. IMS/TWIMS drift scans
  (`ReadDriftScan` / `GetDriftScanCount`) are wired as optional/best-effort and are NOT
  currently expanded into the flattened index — a basic non-IMS path runs first.
