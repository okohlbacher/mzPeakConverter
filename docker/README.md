# Docker harness — native vendor-reader build & runtime verification

The native Agilent/SciEX readers (`--features agilent` / `sciex`) are **compile-verified but not
runtime-tested** on the dev machine, because they need the vendor SDK DLLs (Agilent MHDAC mixed-mode
→ **Windows-only**; SciEX Clearcore2 managed → Windows now, maybe Linux). These containers exist to
run that verification on a host that can provide the DLLs.

> **macOS/Linux note:** Docker on macOS (and Apple Silicon) runs **Linux containers only** — a
> *Windows* container needs a Windows host with Docker in Windows-container mode. So
> `docker/windows/` cannot be built or run on this dev box; it targets a Windows CI runner / Windows
> Docker host. `docker/linux-sciex-spike/` is the cross-platform probe that *can* run here.

## Vendor DLLs (never bundled — license)

Both images expect the vendor DLLs to be **mounted at runtime**, sourced from a **ProteoWizard
install** (it bundles MHDAC under `vendor_api/Agilent` and the Clearcore2 stack under
`vendor_api/ABI`). You accept the vendor EULA by using them (Agilent MHDAC redistribution is
non-commercial-only). Point `MZPC_PWIZ_DIR` at the mounted pwiz dir.

## `docker/windows/` — full native build + test (Windows host)

Builds the C# glues (`dotnet`), the Rust binary with `--features "agilent sciex"`, then converts the
sample datasets and validates. See `windows/Dockerfile` + `windows/build-and-test.ps1`.

```powershell
# On a Windows Docker host, with ProteoWizard + the test corpus available:
docker build -t mzpc-win -f docker/windows/Dockerfile .
docker run --rm `
  -v C:\ProteoWizard:C:\pwiz `                       # MHDAC + Clearcore2 DLLs
  -v C:\data\vendor-agilent-sciex:C:\data `          # the downloaded Agilent/SciEX raw
  -e MZPC_PWIZ_DIR=C:\pwiz `
  mzpc-win
```

## `docker/linux-sciex-spike/` — SciEX-on-Linux probe (runs anywhere, incl. this box)

The handoff's build-order step 1: does Clearcore2 load under .NET 8 on Linux? Builds a Linux .NET 8
+ Rust image with `--features sciex`. It still needs the Clearcore2 DLLs mounted (`MZPC_PWIZ_DIR`);
if they load, SciEX is cross-platform. Agilent MHDAC will NOT load on Linux (mixed-mode) — Windows
only.

```sh
docker build -t mzpc-sciex-linux -f docker/linux-sciex-spike/Dockerfile .
docker run --rm -v /path/to/pwiz:/pwiz -v /path/to/data:/data \
  -e MZPC_PWIZ_DIR=/pwiz mzpc-sciex-linux /data/sciex/<accession>/sample.wiff
```

Until the DLLs are available, `--via-msconvert` (needs a ProteoWizard `msconvert` on PATH) is the
working cross-vendor path and needs no container.
