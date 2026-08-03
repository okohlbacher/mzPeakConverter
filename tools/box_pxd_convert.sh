#!/usr/bin/env bash
# PXD014690 (.wiff) -> mzML + mzPeak on the Windows box, results pulled back to the Mac.
#
# WHY the box: SCIEX .wiff is readable only through the vendor's managed Clearcore2 assemblies.
# The box already carries them inside OpenMS's bundled ProteoWizard (licence accepted by that
# installer), plus msconvert.exe and a current mzpeak-convert.exe -- so no SciexGlue and no
# licence click-through are needed. There is no macOS or Linux Clearcore2 build; this MUST run
# on the box.
#
# The box downloads each raw straight from PRIDE, so the ~29 GB never transits this machine.
#   box_pxd_convert.sh <basename-without-extension> [...]
set -uo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
set -a; . "$here/box.env" 2>/dev/null; set +a
: "${BOX_SSH:?}" "${BOX_JUMP:?}" "${BOX_SSH_KEY:?}" "${BOX_CONVERTER:?}"

PWIZ='C:\Program Files\OpenMS-3.5.0-pre-FVdeploy-2026-01-29\share\OpenMS\THIRDPARTY\pwiz-bin'
FTP=ftp://ftp.pride.ebi.ac.uk/pride/data/archive/2019/11/PXD014690
OUT=${OUT:-$HOME/Claude/mzPeak/data/PXD014690}
mkdir -p "$OUT"
SSHOPT=(-q -i "$BOX_SSH_KEY" -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new
        -o ServerAliveInterval=15 -o ServerAliveCountMax=8 -o TCPKeepAlive=yes)
sshb(){ ssh "${SSHOPT[@]}" -J "$BOX_JUMP" "$BOX_SSH" "$@"; }
scpb(){ scp "${SSHOPT[@]}" -o "ProxyCommand=ssh ${SSHOPT[*]} -W %h:%p $BOX_JUMP" "$@"; }

for B in "$@"; do
  echo "=== $B ==="
  W="C:/Users/User/pxd014690/$B"
  # .wiff.scan holds the actual data; both members are required side by side.
  sshb "powershell -NoProfile -ExecutionPolicy Bypass -Command \"
    \$ErrorActionPreference='Stop'; \$ProgressPreference='SilentlyContinue';
    New-Item -ItemType Directory -Force -Path 'C:/Users/User/pxd014690' | Out-Null;
    foreach (\$ext in @('.wiff','.wiff.scan')) {
      \$dst = '$W' + \$ext;
      if (-not (Test-Path \$dst) -or (Get-Item \$dst).Length -eq 0) {
        Invoke-WebRequest -Uri ('$FTP/$B' + \$ext) -OutFile (\$dst + '.part');
        Move-Item -Force (\$dst + '.part') \$dst
      }
    }
    Write-Output ('RAW_OK ' + ((Get-Item ('$W' + '.wiff.scan')).Length))
  \"" || { echo "  FAIL: download"; continue; }

  # 1) .wiff -> mzML (msconvert; the only path that can read Clearcore2)
  sshb "powershell -NoProfile -ExecutionPolicy Bypass -Command \"
    \$ErrorActionPreference='Continue';
    & '$PWIZ\\msconvert.exe' '$W.wiff' --mzML --zlib --64 -o 'C:/Users/User/pxd014690' 2>&1 | Select-Object -Last 3
  \"" || { echo "  FAIL: msconvert"; continue; }

  # 2) mzML -> mzPeak (our converter)
  sshb "powershell -NoProfile -ExecutionPolicy Bypass -Command \"
    & '$BOX_CONVERTER' '$W.mzML' -o '$W.mzpeak' 2>&1 | Select-Object -Last 2
  \"" || { echo "  FAIL: mzpeak-convert"; continue; }

  for ext in mzML mzpeak; do
    scpb "$BOX_SSH:$W.$ext" "$OUT/$B.$ext" && echo "  got $B.$ext ($(du -h "$OUT/$B.$ext" | cut -f1))"
  done
done
