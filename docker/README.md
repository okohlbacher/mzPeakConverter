# Docker harness — native vendor-reader build & runtime verification

The native Agilent (MHDAC) and SciEX (Clearcore2) readers are **Windows-only** and compile in
automatically on a Windows build (no opt-in feature). They are compile-verified but their *runtime*
needs the vendor SDK DLLs, which are not bundled. This container runs that verification on a Windows
host that can provide the DLLs.

> **macOS/Linux note:** Docker on macOS (and Apple Silicon) runs **Linux containers only** — a
> *Windows* container needs a Windows host with Docker in Windows-container mode. So `docker/windows/`
> targets a Windows CI runner / Windows Docker host, not this dev box. Everywhere else, the
> cross-vendor `--via-msconvert` path (a ProteoWizard `msconvert` on PATH) needs no container.

## Vendor DLLs (never bundled — license)

The image expects the vendor DLLs to be **mounted at runtime**, sourced from a **ProteoWizard
install** (it bundles MHDAC under `vendor_api/Agilent` and the Clearcore2 stack under
`vendor_api/ABI`). You accept the vendor EULA by using them (Agilent MHDAC redistribution is
non-commercial-only). Point `MZPC_PWIZ_DIR` at the mounted pwiz dir.

## `docker/windows/` — full native build + test (Windows host)

Builds the C# glues (`dotnet`) and the Rust binary (a default Windows build already includes the
native vendor readers), then converts the sample datasets and validates. See `windows/Dockerfile` +
`windows/build-and-test.ps1`.

```powershell
# On a Windows Docker host, with ProteoWizard + the test corpus available:
docker build -t mzpc-win -f docker/windows/Dockerfile .
docker run --rm `
  -v C:\ProteoWizard:C:\pwiz `                       # MHDAC + Clearcore2 DLLs
  -v C:\data\vendor-agilent-sciex:C:\data `          # the downloaded Agilent/SciEX raw
  -e MZPC_PWIZ_DIR=C:\pwiz `
  mzpc-win
```
