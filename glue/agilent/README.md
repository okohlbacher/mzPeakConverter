# AgilentGlue — native Agilent MassHunter (`.d`) glue for mzPeakConverter

A tiny .NET 8 class library that lets the Rust `agilent` cargo feature read Agilent MassHunter
`.d` data through Agilent's **MHDAC** (MassHunter Data Access Component) DLLs.

**Status: Windows-runtime-only and UNTESTED.** It *builds* on macOS/Linux/Windows (no Agilent DLLs
needed at build time), but only *runs* on x64 Windows with the MHDAC DLLs present.

## Why reflection (no compile-time reference)

MHDAC is a Windows-only mixed-mode assembly set with a restrictive license; it cannot be committed
or referenced on a build box that doesn't have it. So `Glue.cs` touches **no** MHDAC type at compile
time — every call goes through `System.Reflection`. Result: `dotnet build` works anywhere a .NET 8
SDK is installed, and the DLLs are required only at run time.

## Build

```sh
dotnet build glue/agilent/AgilentGlue.csproj -c Release
```

Output: `bin/Release/net8.0/AgilentGlue.dll` + `AgilentGlue.runtimeconfig.json`.

The Rust side hosts this assembly in-process (via `netcorehost`) and calls its
`[UnmanagedCallersOnly]` exports.

## Run-time configuration (set before running mzPeakConverter `--features agilent`)

| Env var             | Meaning                                                                              |
|---------------------|--------------------------------------------------------------------------------------|
| `MZPC_AGILENT_GLUE` | Directory containing `AgilentGlue.dll` + `AgilentGlue.runtimeconfig.json` (the build output above). |
| `MZPC_PWIZ_DIR`     | A ProteoWizard install directory. MHDAC DLLs are loaded from `<MZPC_PWIZ_DIR>/vendor_api/Agilent`.  |

## Sourcing the MHDAC DLLs (from ProteoWizard)

ProteoWizard bundles the Agilent MHDAC assemblies under:

```
<ProteoWizard install>/vendor_api/Agilent/
    MassSpecDataReader.dll      (entry point used by the glue)
    BaseCommon.dll
    BaseDataAccess.dll
    BaseError.dll
    agtsampleinforw.dll
    MassSpecDataReader.xml      (optional)
    ... and other Agilent dependencies
```

The glue loads `MassSpecDataReader.dll` via `Assembly.LoadFrom` and registers an `AssemblyResolve`
handler so the sibling dependencies resolve from the same directory.

> **License note.** The Agilent MHDAC redistributable is licensed for **non-commercial use only**.
> Do not redistribute the DLLs with this project. Obtain them from your own ProteoWizard install,
> which carries Agilent's EULA. This glue contains none of Agilent's code — only reflection calls
> against DLLs you supply.

## Scope

Non-IM MS only (MS1/MS2, profile or centroid). Agilent ion-mobility (6560 IM-QTOF) requires the
separate **MIDAC** SDK to read the drift dimension and is **out of scope** (TODO in `src/agilent.rs`).

## C-ABI (mirror of `src/agilent.rs`)

All strings are UTF-16 NUL-terminated.

```
long Open(char* path, char* mhdacDir)             -> handle >= 0, or negative error code
long SpectrumCount(long handle)                   -> count >= 0, or negative error code
int  GetSpectrum(long handle, long i, SpectrumOut* out) -> 0 ok / non-zero error (out zeroed on error)
void FreeSpectrum(SpectrumOut* out)               -> frees the two HGlobal buffers
void Close(long handle)                           -> idempotent
int  LastError(char* buf, int cap)                -> chars written (or required length if cap too small)
```

`SpectrumOut` (sequential layout, identical on both sides):

```
double* MzPtr; double* IntensityPtr; long NPoints;
double RtMinutes; int MsLevel; int Polarity; int IsCentroid; int ScanId;
```

Memory model: `GetSpectrum` allocates `MzPtr`/`IntensityPtr` with `Marshal.AllocHGlobal`; the Rust
caller copies them out and then calls `FreeSpectrum`.
