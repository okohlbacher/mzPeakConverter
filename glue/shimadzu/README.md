# ShimadzuGlue — native Shimadzu `.lcd` reader (Windows-only)

A thin C# shim the Rust `shimadzu` path hosts in-process (via `netcorehost`) to read Shimadzu
LabSolutions `.lcd` files through the vendor **`Shimadzu.LabSolutions.IO`** managed API — the same
DLL ProteoWizard's `Reader_Shimadzu` wraps. This lets `mzpeak-convert` read `.lcd` **without
shelling out to `msconvert`**.

## ⚠️ Vendor DLLs are NEVER shipped in this repo

The proprietary Shimadzu assemblies (`Shimadzu.LabSolutions.IO.IoModule.dll` and its siblings)
carry a restrictive EULA. They are **not committed, not bundled, and not referenced at compile
time**:

- `ShimadzuGlue.csproj` has **no** `<Reference>`/`<PackageReference>` to any Shimadzu assembly, so
  the project builds on any platform (including the CI/build host) with the DLLs absent.
- `Glue.cs` reaches the vendor API entirely via **runtime reflection** (`Assembly.LoadFrom`), loading
  the DLL from an **existing ProteoWizard installation** at run time — the directory passed in
  `MZPC_PWIZ_DIR` (where `Shimadzu.LabSolutions.IO.IoModule.dll` sits flat, next to `msconvert.exe`).
- `.gitignore` excludes `glue/**/bin/`, `glue/**/obj/`, `glue/**/*.dll`, and every vendor assembly by
  name — a hard backstop against an accidental `git add`.

You must have a licensed ProteoWizard (or LabSolutions) install on the conversion machine. This repo
supplies only the **source glue**, never the vendor binaries.

## Build

```
dotnet build -c Release          # -> bin/Release/net8.0/ShimadzuGlue.dll (+ .runtimeconfig.json)
```

## Runtime env (set by the Rust side)

- `MZPC_SHIMADZU_GLUE` — directory holding the built `ShimadzuGlue.dll` + `ShimadzuGlue.runtimeconfig.json`.
- `MZPC_PWIZ_DIR` — a ProteoWizard install dir holding `Shimadzu.LabSolutions.IO.IoModule.dll`.

Requires Windows + a .NET 8 runtime. Only the newer LabSolutions `.lcd` (LCMS-9030 Q-TOF, 8000-series
triple-quad, 2020 single-quad) is supported; the legacy **LCMS-IT-TOF** `.lcd` is not (the vendor
library returns `E_UNSUPPORTEDFILE`). For those, no path exists short of Shimadzu's own export.

## EULA

The Shimadzu access libraries are governed by Shimadzu's EULA (bundled inside ProteoWizard), which
scopes use to ProteoWizard-branded work and prohibits reverse-engineering. Using them from another
tool is a legal-review item — see the note in the main handoff. This glue only *calls* the installed
library through its documented managed API; it does not reverse-engineer or redistribute it.
