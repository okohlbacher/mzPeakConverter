# SciexGlue — native SciEX `.wiff` / `.wiff2` reader shim

A thin .NET 8 C# library that the mzPeakConverter Rust `sciex` module (compiled in automatically
on Windows — there is no cargo feature) hosts in-process (via `netcorehost` / CoreCLR) to read
SciEX WIFF files through the vendor **Clearcore2** .NET API, and exposes the data back to Rust
over a tiny C ABI.

> ⚠️ **Windows-runtime-only.** This project *builds* on any platform (including macOS) because it
> has **no compile-time reference** to Clearcore2 — every vendor call is made through runtime
> reflection. But it only *runs* where the Clearcore2 DLLs and a compatible .NET 8 runtime are
> present. In practice that means Windows (Linux *may* work with the Linux Clearcore2 build but is
> unverified). It is the lane that produced the published native-SCIEX corpus archives (e.g.
> MSV000095995, PXD065872, sciex-qtrap-6500).

## How it fits together

```
mzPeakConverter (Rust, src/sciex.rs — Windows build, no feature flag)
        │  netcorehost: load_hostfxr → initialize_for_runtime_config → delegate loader
        ▼
SciexGlue.dll  (this project)   ── reflection (Assembly.LoadFrom) ──►  Clearcore2*.dll
        ▲                                                                   (from pwiz)
        │  C ABI: SciexAbiVersion / Open / Close / SpectrumCount / SpectrumMetaV2 /
        │         SpectrumDataV2 / DataFree / RunInfo / RunString / LastError
        └─ [UnmanagedCallersOnly] static exports in SciexGlue.Exports
```

`src/sciex.rs` is the Rust side; it documents the exact ABI. `Glue.cs` is the managed side;
its `Exports` class implements that ABI, and `Clearcore2Api` does the reflection. The V1
`SpectrumMeta` / `SpectrumData` exports stay for binaries that predate the handshake.

**The DLL and the executable are one unit.** `SciexAbiVersion` must equal `REQUIRED_ABI_VERSION`
in `src/sciex.rs`; the converter refuses a glue from another commit with a message naming both
versions (a glue without the export counts as version 1). `tests/sciex_abi_pin.rs` holds the two
sources to one contract on every host: the version literal, the struct twins, their sizes and the
exports.

## Building

```sh
cd glue/sciex
dotnet build -c Release          # produces bin/Release/net8.0/SciexGlue.dll (+ runtimeconfig)
```

The build succeeds **without** any Clearcore2 DLLs present — that is the whole point of the
reflection design. (The committed `SciexGlue.runtimeconfig.json` documents the expected
runtime shape; `dotnet build` also emits one next to the DLL.)

## Sourcing the Clearcore2 DLLs (from ProteoWizard)

The Clearcore2 assemblies are **proprietary SciEX code redistributed only inside
ProteoWizard**. They are *not* shipped with this project and must not be committed here.

1. Install ProteoWizard (the version that bundles the SciEX vendor API), e.g.
   `C:\Program Files\ProteoWizard\ProteoWizard 3.x`.
2. The Clearcore2 DLLs live under `<pwiz-install>\vendor_api\ABI\` (look for
   `Clearcore2.Data.dll`, `Clearcore2.Data.AnalystDataProvider.dll`,
   `Clearcore2.Data.WiffReader.dll`, `Clearcore2.RawXYProcessing.dll`, and friends).

## Running (environment variables read by `src/sciex.rs`)

| Variable           | Meaning                                                                      |
| ------------------ | ---------------------------------------------------------------------------- |
| `MZPC_SCIEX_GLUE`  | Directory holding the built `SciexGlue.dll` + `SciexGlue.runtimeconfig.json` (e.g. `glue/sciex/bin/Release/net8.0`). |
| `MZPC_PWIZ_DIR`    | ProteoWizard install root. The glue looks for the Clearcore2 DLLs under `<MZPC_PWIZ_DIR>/vendor_api/ABI` (and accepts `MZPC_PWIZ_DIR` itself if it directly contains them). |

A .NET 8 runtime (`Microsoft.NETCore.App` 8.0+) must be installed on the host.

Example (Windows PowerShell):

```powershell
$env:MZPC_SCIEX_GLUE = "C:\...\mzPeakConverter\glue\sciex\bin\Release\net8.0"
$env:MZPC_PWIZ_DIR   = "C:\Program Files\ProteoWizard\ProteoWizard 3.0.24"
mzpeak-convert sample.wiff -o sample.mzpeak
```

## EULA / licensing caveat

The Clearcore2 / SciEX vendor API is governed by SciEX's and ProteoWizard's license terms.
By using this glue you are loading and executing SciEX proprietary code that you obtained
through your own licensed ProteoWizard install. **Do not redistribute the Clearcore2 DLLs**
with mzPeakConverter, and ensure your use complies with the SciEX vendor-API EULA and the
ProteoWizard license. This project only provides the *bridge*; it ships none of the vendor
binaries.

## Status / caveats

- **In production use** on the Windows conversion box; member names/casing in the Clearcore2
  API drift between releases, so the reflection lookups are deliberately tolerant
  (candidate-name fallbacks) and may need adjustment against a Clearcore2 version other than the
  one ProteoWizard 3.0.26151 bundles. Compare against ProteoWizard's
  `pwiz_aux/msrc/utility/vendor_api/ABI/WiffFile.cpp`.
- **Precursors** (M32) are read where ProteoWizard's ABI reader reads them — a product spectrum's
  `ParentMZ` / `ParentChargeState`, and on a Product experiment `MassRangeInfo[0].IsolationWindow`
  and `Parameters["CE"]` — and the precursor is built on the Rust side (`sciex_run::precursor`).
  Not yet run against a WIFF. Clearcore2 reports no fragmentation mode: the method is beam-type CID
  (pwiz's assumption) on instruments that can only fragment by collision, and unstated on a ZenoTOF,
  which can also fragment by EAD. A precursor-ion scan's fixed mass, a product, is not carried.
- Intensities are narrowed from Clearcore2 `double` to `float` to match the mzPeak schema. On the
  way, NaN becomes 0 and a value beyond ±`float.MaxValue` (±Inf included) is clamped to it, and an
  m/z / intensity pair of unequal length is cut to the shorter one (M9). Each is counted per
  spectrum (`SpectrumDataV2`) and, when it happened, declared in the archive's `transformations`
  as `sciex:nan-intensity-to-zero`, `sciex:clamp-intensity-to-f32` or
  `sciex:truncate-unequal-arrays`; `--to mzml` logs it instead.
- Retention time from Clearcore2 is in **minutes**; the glue converts to seconds at the ABI,
  and the Rust side converts back to minutes for mzdata. (Net: mzdata gets minutes.)
- Polarity is mapped 0 = positive, 1 = negative, other = unknown.
- The reader flattens every `(sample, experiment, cycle)` into a single index; ids follow the
  ProteoWizard `sample=N period=1 cycle=N experiment=N` convention.
