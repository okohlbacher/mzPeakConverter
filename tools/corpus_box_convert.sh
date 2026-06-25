#!/usr/bin/env bash
# Convert one vendor dataset on flash-workstation (SCIEX .wiff / Waters .raw) and echo:
#   RAW_BYTES  MZPEAK_BYTES  BOX_OUTPUT_PATH
# Used by tools/corpus_bench.sh --box. Vendor env is selected from the file extension.
set -uo pipefail
raw="$1"; flags="${2:---no-vendor}"
pwizdir='C:/Program Files (x86)/FLASHApp-1.0.0/FLASHApp-1.0.0/share/OpenMS/THIRDPARTY/pwiz-bin'
case "$raw" in
  *.wiff)  glue='MZPC_SCIEX_GLUE';  gluedir='glue\sciex\bin\Release\net8.0';  dlldir="$pwizdir"; dllvar='MZPC_PWIZ_DIR';;
  *.raw)   glue='MZPC_WATERS_GLUE'; gluedir='glue\waters\bin\Release\net8.0'; dlldir="$pwizdir"; dllvar='MZPC_MASSLYNX_DIR';;
  *) echo "0 0 -"; exit 0;;
esac
stem="$(basename "$raw")"; out="C:/Users/User/bench_${stem%.*}.mzpeak"
/tmp/flashps.sh <<PS 2>/dev/null | sed -E 's/\x1b\[[0-9;]*[mGKA-Z]//g; s/_x000D__x000A_/\n/g; s/<[^>]*>//g' | grep -E '^BENCH ' | tail -1 | sed 's/^BENCH //'
\$ErrorActionPreference='Continue'; \$ProgressPreference='SilentlyContinue'
Set-Location C:\Users\User\src\mzPeakConverter
\$env:DOTNET_ROOT='C:\Users\User\dotnet8'; \$env:DOTNET_ROLL_FORWARD='LatestMajor'
\$env:$glue=(Resolve-Path '$gluedir').Path
\$env:$dllvar='$dlldir'
\$raw='$(echo "$raw" | sed 's#/#\\#g')'
\$out='$(echo "$out" | sed 's#/#\\#g')'
# raw size: a .wiff counts its sidecars; a .raw dir counts the directory
if (Test-Path \$raw -PathType Leaf) {
  \$base=[IO.Path]::GetFileNameWithoutExtension(\$raw); \$dir=Split-Path \$raw
  \$rb=(Get-ChildItem \$dir -Filter "\$base.*" | Measure-Object Length -Sum).Sum
} else { \$rb=(Get-ChildItem \$raw -Recurse -File | Measure-Object Length -Sum).Sum }
& .\target\release\mzpeak-convert.exe \$raw $flags -o \$out --force *> \$null
\$mp = if (Test-Path \$out) { (Get-Item \$out).Length } else { 0 }
Write-Output "BENCH \$rb \$mp \$out"
PS
