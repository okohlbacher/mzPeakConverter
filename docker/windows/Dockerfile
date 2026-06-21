# Windows-container build + native runtime test for the Agilent/SciEX readers.
# REQUIRES a Windows Docker host in Windows-container mode (cannot build on macOS/Linux Docker).
#
# Provides: .NET 8 SDK (for the C# glues), Rust + MSVC build tools (for cargo --features), and the
# repo. Vendor DLLs are NOT baked in (license) — mount a ProteoWizard install at runtime and set
# MZPC_PWIZ_DIR (see docker/README.md and build-and-test.ps1).
# escape=`
FROM mcr.microsoft.com/dotnet/sdk:8.0-windowsservercore-ltsc2022

SHELL ["powershell", "-NoProfile", "-Command", "$ErrorActionPreference='Stop';"]

# Chocolatey → Visual Studio C++ Build Tools (MSVC linker, required by the Rust msvc toolchain) + Rust.
RUN Set-ExecutionPolicy Bypass -Scope Process -Force; `
    iex ((New-Object System.Net.WebClient).DownloadString('https://community.chocolatey.org/install.ps1'))
RUN choco install -y visualstudio2022buildtools --package-parameters `
      '--add Microsoft.VisualStudio.Workload.VCTools --includeRecommended --quiet --norestart'
RUN choco install -y rustup.install; `
    C:\Users\ContainerAdministrator\.cargo\bin\rustup.exe default stable-x86_64-pc-windows-msvc
ENV PATH="C:\Users\ContainerAdministrator\.cargo\bin;${PATH}"

WORKDIR C:\mzpc
# Copy the converter source (vendored deps included). .dockerignore should exclude target/.
COPY . .

# Pre-build the glues + the binary so `docker run` just executes the test. The cargo build needs
# network for crates (or vendor them); the glues are reflection-only so they build without the DLLs.
RUN dotnet build glue\sciex\SciexGlue.csproj -c Release; `
    dotnet build glue\agilent\AgilentGlue.csproj -c Release
RUN cargo build --release --features "agilent sciex"

# Default: run the verification script (expects datasets + pwiz DLLs mounted; see build-and-test.ps1).
ENTRYPOINT ["powershell", "-NoProfile", "-File", "C:\\mzpc\\docker\\windows\\build-and-test.ps1"]
