# SciEX-on-Linux probe (handoff build-order step 1): does Clearcore2 load under .NET 8 on Linux?
# Linux container → builds on macOS/Linux Docker. Builds the C# SciEX glue + the Rust binary with
# `--features sciex`. Agilent MHDAC is mixed-mode and will NOT load on Linux — Windows only.
#
# At runtime, mount a ProteoWizard install (Clearcore2 DLLs in vendor_api/ABI) and a .wiff dataset;
# set MZPC_PWIZ_DIR. If Clearcore2 loads + reads a spectrum, SciEX is cross-platform.
FROM mcr.microsoft.com/dotnet/sdk:8.0 AS base

# Rust (msvc not needed on Linux; gnu toolchain). build-essential for the C linker + sqlite/zstd sys.
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential curl ca-certificates pkg-config && rm -rf /var/lib/apt/lists/*
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
ENV PATH="/root/.cargo/bin:${PATH}"

WORKDIR /mzpc
COPY . .

# Reflection glue builds without the vendor DLLs; Rust compiles the sciex feature.
RUN dotnet build glue/sciex/SciexGlue.csproj -c Release
RUN cargo build --release --features sciex

ENV MZPC_SCIEX_GLUE=/mzpc/glue/sciex/bin/Release/net8.0
# Usage: docker run --rm -v /pwiz:/pwiz -v /data:/data -e MZPC_PWIZ_DIR=/pwiz IMG /data/x.wiff
ENTRYPOINT ["/bin/sh", "-c", "exec /mzpc/target/release/mzpeak-convert \"$0\" -o /tmp/out.mzpeak --force --verify"]
