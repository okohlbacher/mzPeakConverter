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
  (If you place that runtimeconfig by hand rather than using the `dotnet build` output, it must
  carry the BinaryFormatter switch — see below, or nothing loads at all.)
- `MZPC_PWIZ_DIR` — a ProteoWizard install dir holding `Shimadzu.LabSolutions.IO.IoModule.dll`.
  **Use a current ProteoWizard** — 3.0.26151 verified, which ships that DLL at version
  **5.0.0.0**. ProteoWizard 3.0.22187 (July 2022, the version inside the FLASHApp/OpenMS
  third-party bundle) ships **3.8.4.6016**, which mispairs centroid intensities on profile-less
  `.lcd` files — see the stale-library section below.

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
| `spectra_peaks` (centroids, point layout, never chunked) | `point.tof_index` Int64 (`LinearMz`, `transform_params = "1e-9"`); `point.mz` f64 NULL on lattice rows; `point.intensity` f32 | `m/z = tof_index / 1e9` (the DIVISION — see §9 of the user manual) | `mz_calibration` `{codec: mz-grid, scale: 1e9, vendor: shimadzu, applies_to: spectra_peaks}` |

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

## Stale-library defect: misaligned centroids from `Shimadzu.LabSolutions.IO` **3.8.4.6016**

**This is a bug in one version of the vendor library, and the remedy is a current ProteoWizard —
not a LabSolutions mzML export.** Version **5.0.0.0** of `Shimadzu.LabSolutions.IO.IoModule.dll`,
the one shipped by ProteoWizard **3.0.26151** (verified), reads profile-less `.lcd` files
**correctly** through this glue. Version **3.8.4.6016** — shipped by ProteoWizard 3.0.22187
(July 2022) and by the FLASHApp/OpenMS third-party bundle built on it — does not. Point
`MZPC_PWIZ_DIR` at a current install and everything below is history.

Until v0.9.9 this file, the CHANGELOG, the user manual and the converter's own runtime warning all
called the defect *inherent* and *unreachable*. That was wrong: every measurement behind that claim was taken against 3.8.4.6016,
including the msconvert cross-check, which was driving the very same DLL out of the very same
directory.

### What 3.8.4.6016 returns

For a spectrum that carries **no profile signal**, that version returns a `CentroidList` whose
entries pair the correct `Mass` with the **wrong `Intensity`**. Measured on `DIA_Hela_20ng`, scan 2,
against the LabSolutions mzML export of the same run:

| | stored (3.8.4.6016) | oracle |
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

No API lever reaches it **on that version**: `profileDesired=0`, centroid-only fetch,
centroid-before-profile ordering and two independent decodes all return identical rotated data. And
**msconvert produced byte-identical corrupt output — because it was reading the same 3.8.4.6016
DLL**. That was never independent confirmation of an inherent defect; it only showed that both
readers were sharing one bad library. `--via-msconvert` is therefore not a workaround *while the old
library is what is installed*, and becomes irrelevant once it is not.

### What 5.0.0.0 returns

Measured on `DIA_Hela_20ng.lcd`, spectra 1/10/100/1000, against the LabSolutions 5.128 SP2 export:

| reader | peaks | intensities |
|---|---|---|
| this glue + 3.8.4.6016 | 611 / 14,299 / 13,557 / 11,360 | shifted 1–7, header scalars at the head |
| msconvert + 3.8.4.6016 | identical to the above | identical to the above |
| msconvert + 5.0.0.0 | **612 / 14,300 / 13,558 / 11,361** | **max \|Δ\| = 0**, m/z to 5e-5 (coarse `Mass`) |
| **this glue + 5.0.0.0** | **612 / 14,300 / 13,558 / 11,361** | **max \|Δ\| = 0**, m/z to **2e-13** (`MassHigh`) |

Files that DO store profile signal were always exact through either library
(`Blind_P1_pos_012`: 13,200/13,200 spectra bit-exact).

The LabSolutions mzML export is still exact — its exporter takes a different internal path — but it
is **no longer the recommended route**, and it costs you `MassHigh` precision relative to reading the
`.lcd` natively with 5.0.0.0.

### Which library am I actually running?

The DLL comes from whatever `MZPC_PWIZ_DIR` points at, so check that directory, not the ProteoWizard
you *meant* to use:

```powershell
(Get-Item "$env:MZPC_PWIZ_DIR\Shimadzu.LabSolutions.IO.IoModule.dll").VersionInfo.FileVersion
```

- `5.0.0.0` → good (ProteoWizard 3.0.26151+).
- `3.8.4.6016` → **stale**; profile-less `.lcd` conversions from it are wrong and must be reconverted.
  The usual source of this version is the **FLASHApp / OpenMS third-party ProteoWizard bundle
  (3.0.22187, July 2022)**. Install a current ProteoWizard and repoint `MZPC_PWIZ_DIR`.

### Loading 5.0.0.0 needs the BinaryFormatter switch

5.0.0.0 deserialises part of the `.lcd` through `BinaryFormatter`, which .NET has disabled by default
since .NET 5; without the switch `LoadData` throws `NotSupportedException` inside a
`TargetInvocationException` and **every** `.lcd` looks unreadable. `ShimadzuGlue.csproj` sets
`<EnableUnsafeBinaryFormatterSerialization>true</EnableUnsafeBinaryFormatterSerialization>` (the
first-class SDK property — a bare `RuntimeHostConfigurationOption` is overridden by the SDK's own
default of `false`). A DLL-only deployment, where the Rust host is pointed at a directory holding
just `ShimadzuGlue.dll` + a hand-placed `ShimadzuGlue.runtimeconfig.json`, only works if that
runtimeconfig carries the same knob:

```json
"configProperties": {
  "System.Runtime.Serialization.EnableUnsafeBinaryFormatterSerialization": true
}
```

If it is missing or `false`, the symptom is that every file fails to load — not that it loads badly.
The `ShimadzuGlue.runtimeconfig.json` committed beside this README carries it; keep the two in sync
whenever the csproj gains a `configProperty`.

Security: the switch is scoped to this glue, whose input is a vendor instrument file the user chose
to convert, the same path ProteoWizard itself runs. .NET 9 removes `BinaryFormatter` outright, so a
retarget needs a vendor DLL that does not use it.

### The reader still stores what the vendor returns

Correcting or dropping vendor data is not this converter's job, so on a stale library the peaks are
stored exactly as returned, unaltered, and a one-shot warning names the library version and the fix.

That warning is gated on **both** conditions, version first: the loaded
`Shimadzu.LabSolutions.IO` reports a major version below 5 (the glue publishes it over the ABI as
`LibraryVersion`, ABI 4), **and** the file stores no profile signal. On a current ProteoWizard
nothing probes the file at all, and a profile-bearing file never warns — warning on the file alone,
as releases before this one did, cried wolf on good data. A version the glue cannot parse is not
ACCUSED, but neither is it cleared: it falls through to the file probe and, on a profile-less
`.lcd`, warns that the alignment COULD NOT BE CHECKED — wording distinct from the accusation a
known-bad library gets. Silence there would mean an unreadable version reads as a good one.

The `.lcd` container does hold the data (a `Centroid Data` stream plus a 24 B/spectrum
`Centroid Index`); decoding it directly stays out of scope, and with 5.0.0.0 there is no reason to.

To reproduce the old behaviour: `MZPC_SHIMADZU_PROBE=6` on a profile-less `.lcd` with a 3.8.4.6016
install.

## Removed: the reflective dumps (they modified the `.lcd`) and the Stage-B fetch levers

`MZPC_SHIMADZU_DUMP_READER=1` walked the `DataObject` graph with reflection, invoking every public
property getter to find where the metadata lived. `MZPC_SHIMADZU_DUMP=<scan>` did the same walk over
one decoded spectrum object. Both are **removed** (DUMP_READER 2026-09-03, DUMP with it) because
those getters make the vendor library rewrite the file it is reading. Measured on a copy of
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

`MZPC_SHIMADZU_FETCH` (`legacy|centroid-first|centroid-only|split`) and
`MZPC_SHIMADZU_PROFILE_DESIRED=0` went with them. They existed to run the factorial experiment that
proved the rotation was the library's, not the file's; that question is answered (it is
3.8.4.6016, and 5.0.0.0 reads the same file correctly), and their conclusion is version-specific, so
nothing is learned by running them again on a newer library. Three of the four modes they selected —
including asking the vendor for centroids with `profileDesired=false` — are the configurations that
produce the ROTATED data, so leaving them reachable meant one stray environment variable could
silently write a corrupt archive. Their removal also lets the one-entry spectrum memo run
unconditionally: it used to be bypassed whenever any lever was set.

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
