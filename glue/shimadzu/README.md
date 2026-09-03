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

## What the archive stores

The Rust side reads `MassHigh` (Int64, 1e-9 Da — what LabSolutions' own mzML exporter writes)
rather than the coarse `Mass` (Int32, 1e-4 Da, what ProteoWizard reads); `MZPC_SHIMADZU_COARSE_MZ=1`
selects `Mass`. Both representations are kept by default (`--representation both|profile|centroid`),
each on the exact integer grid it sits on:

| facet | column(s) | reconstruct | index block |
|---|---|---|---|
| `spectra_data` (profile, point layout) | `tof_index` Int32 + per-spectrum `tof_c0`, `tof_c1`; `mz` f64 NULL on gridded rows | `m/z = (tof_c0 + tof_c1·tof_index)²` | `tof_calibration` `{codec: tof-grid, model: sciex_sqrt_per_spectrum, vendor: shimadzu, run_wide_c1}` |
| `spectra_peaks` (centroids, point layout, never chunked) | `point.tof_index` Int64 (`LinearMz`, `transform_params = "1e-9"`); `point.mz` f64 NULL on lattice rows; `point.intensity` f32 | `m/z = 1e-9 · tof_index` | `mz_calibration` `{codec: mz-grid, scale: 1e9, vendor: shimadzu, applies_to: spectra_peaks}` |

Every spectrum is checked on its own before it is gridded (profile: every point within 1e-9 of the
sqrt grid; centroids: `|m/z·1e9 − k| < max(1e-3, 8 ulp)` and `k` non-decreasing), and one that fails
keeps its f64 m/z in the same facet — nothing is snapped or dropped. Readers resolve the axis **per
facet** (centroids: `mz_calibration`; profile: `tof_calibration`) and treat a finite, positive `mz`
as the f64 fallback that wins over the integer axis. Measured on the reference files: m/z equal to the
LabSolutions export to the last f64 digit, every spectrum on its grid, and archive sizes of 3.79 MB
(Blind_P1_pos_012), 23.9 MB (HEK_PosOAD1) and 1.31 GB (DIA_Hela_20ng; 2.19 GB as f64 `MassHigh`,
839 MB on the coarse `Mass`). The run summary in the log (`N spectra on the sqrt grid / on the
1e-9 m/z lattice`) counts the written spectra. Precursors, scan windows, the instrument
configuration and the source SHA-1 (digested before the DLL locks the file) ride in
`spectra_metadata` / the index as for every other lane.

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

## Removed: the reflective reader dump (it modified the `.lcd`)

`MZPC_SHIMADZU_DUMP_READER=1` walked the `DataObject` graph with reflection, invoking every public
property getter to find where the metadata lived. It is **removed** (2026-09-03) because those
getters make the vendor library rewrite the file it is reading. Measured on a copy of
`HEK_PosOAD1.lcd`, diffed against the pristine file at the structured-storage level:

| | before | after |
|---|---|---|
| size | 63,188,992 B | 63,188,992 B (unchanged) |
| SHA-1 | `3a55dde0…` | `8ba2ca57…` |
| differing bytes | — | 33,661 in 431 ranges, **all** in the OLE2 header + directory/FAT sectors |
| stream *contents* | — | **none changed** |
| streams | 229 | **224** — `Mass Data Load Format/{DDA Filter Parameter, Fragment Table, MIC Table, Precursor Sort Filter Parameter, Profile Load Parameter}` deleted |
| `Root Entry` / `Mass Data Load Format` modify time | original | set to the run time |

Losing `Profile Load Parameter` is why the very next `GetMSSpectrumByScan(profileDesired=true)`
failed with `E_FAIL` in the same process. The cause is structural: `IDataIO.LoadData(path)` has no
read-only overload — it opens the compound file read-write (and byte-range-locks it, hence
`os error 33` when hashing while open), so a lazy getter that normalises the load-format storage
commits straight to disk in OLE2 direct mode.

**The conversion path never writes.** It calls only `LoadData`, `GetMSSpectrumInfo`,
`GetMSSpectrumByScan`, `GetAnalysisTime`, `RetTimeToScan`, `GetMassRawRange`, `SystemName`,
`EventCount`, `GetEventNo`, thirteen property reads and `IO.Close` on drop — no setter, save, write,
commit or update is referenced anywhere in this glue. `Blind_P1_pos_012.lcd` is byte-identical after
dozens of opens, and every `.lcd` in the corpus still hashes to its canonical value after conversion.

## EULA

The Shimadzu access libraries are governed by Shimadzu's EULA (bundled inside ProteoWizard), which
scopes use to ProteoWizard-branded work and prohibits reverse-engineering. Using them from another
tool is a legal-review item — see the note in the main handoff. This glue only *calls* the installed
library through its documented managed API; it does not reverse-engineer or redistribute it.
