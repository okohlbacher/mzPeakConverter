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

## Known vendor defect: misaligned centroids on profile-less `.lcd`

For a spectrum that carries **no profile signal**, `Shimadzu.LabSolutions.IO` returns a
`CentroidList` whose entries pair the correct `Mass` with the **wrong `Intensity`**. Measured on
`DIA_Hela_20ng`, scan 2, against the LabSolutions mzML export of the same run:

| | stored | oracle |
|---|---|---|
| `CentroidList[0].Mass` | 1002162 → m/z 100.2162 | 100.2162 ✓ |
| `CentroidList[0].Intensity` | 12455 | 68 ✗ |
| `CentroidList.Count` | 15,484 | 15,485 |

The intensities lag their m/z by 1–7 positions (3 in ~94 % of spectra) and the leading "intensities"
are the spectrum's own header scalars — `BPInt = 45640` from the same object appears verbatim as the
second value. The vendor's own `Count` is short by one, which is where the missing final peak comes
from. Values are otherwise bit-exact once shifted, so the numbers are right and only the pairing is
wrong; because the archive's TIC/BPI are recomputed from the stored arrays, nothing self-consistent
can detect it.

**It is not reachable through the API.** `profileDesired=0`, centroid-only fetch,
centroid-before-profile ordering and two independent decodes all return identical rotated data, and
**msconvert — reading the same DLL — produces byte-identical corrupt output**, so `--via-msconvert`
is *not* a workaround. Spectra that carry profile signal are unaffected through every path
(`Blind_P1_pos_012`: 13,200/13,200 spectra bit-exact).

Consequence: the reader **stores these peaks exactly as the vendor API returned them** and emits a
one-shot warning naming the defect. Correcting or dropping vendor data is not this converter's job —
it stores vendor data in a new format — and msconvert writes the same bytes without saying anything.
For scientifically correct centroids from such a file, use a **LabSolutions mzML export**, whose
exporter takes a different internal path and is exact. The `.lcd` container does hold the data — a
`Centroid Data` stream plus a 24 B/spectrum `Centroid Index` — but decoding it directly is
deliberately out of scope.

To reproduce: `MZPC_SHIMADZU_PROBE=6` on a profile-less `.lcd`, optionally with
`MZPC_SHIMADZU_FETCH=legacy|centroid-first|centroid-only|split` and `MZPC_SHIMADZU_DUMP=<scan>`.

## EULA

The Shimadzu access libraries are governed by Shimadzu's EULA (bundled inside ProteoWizard), which
scopes use to ProteoWizard-branded work and prohibits reverse-engineering. Using them from another
tool is a legal-review item — see the note in the main handoff. This glue only *calls* the installed
library through its documented managed API; it does not reverse-engineer or redistribute it.
