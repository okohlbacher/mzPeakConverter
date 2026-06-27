#!/usr/bin/env bash
# flash_run.sh — drive the flash-workstation (Windows) conversion harness from the Mac.
#
# The Windows box is the only machine with the native vendor DLLs (Agilent/SciEX/Waters/Bruker-BAF
# + .NET for Thermo), so all vendor raws are converted+validated THERE. This script: preflights the
# box, gets datasets onto it (fetch public ones on-box, push Mac-only ones up), runs the Windows
# worker over SSH, and pulls the small .mzpeak archives + results back.
#
# Connection: an ssh host ALIAS that already encodes the jump + user (see ~/.ssh/config):
#     Host flash
#         HostName 192.214.178.124
#         User user
#         ProxyJump hive            # hive.cs.uni-tuebingen.de, User kohlbach (uni key/agent)
# The jump hop authenticates with your hive key; only the final Windows hop needs a password,
# fed via sshpass (FLASH_PW). ponytail: password for first contact — `ssh-copy-id flash` once the
# box runs OpenSSH, then unset FLASH_PW and this drops to key auth. Creds are NOT embedded here.
#
# Usage:
#   tools/flash/flash_run.sh preflight
#   tools/flash/flash_run.sh push  <local-path> [<local-path> ...]   # Mac-only data -> C:\data
#   tools/flash/flash_run.sh fetch [manifest.tsv]                    # download public sets ON the box
#   tools/flash/flash_run.sh convert [--via-msconvert] [--purge]     # run the worker on the box
#   tools/flash/flash_run.sh pull  [local-out-dir]                   # archives+results -> Mac
#   tools/flash/flash_run.sh all   [manifest.tsv]                    # fetch -> convert -> pull
set -uo pipefail
here="$(cd "$(dirname "$0")" && pwd)"; repo="$(cd "$here/../.." && pwd)"

FLASH_SSH="${FLASH_SSH:-flash}"                 # ~/.ssh/config alias (jump + user baked in)
FLASH_PW="${FLASH_PW:-$(sed -nE 's/^PW=(.*)$/\1/p' "$repo/flash-workstation.txt" 2>/dev/null | head -1)}"
FLASH_DATA="${FLASH_DATA:-C:\\data}"
FLASH_OUT="${FLASH_OUT:-C:\\out}"
FLASH_PWIZ="${FLASH_PWIZ:-C:\\ProteoWizard}"
WORKER_WIN='C:\mzpc\tools\flash\flash_convert_validate.ps1'
MANIFEST="${MANIFEST:-$repo/tools/vendor_ci_manifest.tsv}"

# sshpass wraps the Windows-hop password; with a key installed, set FLASH_PW='' for plain ssh.
SSHP=(); [ -n "$FLASH_PW" ] && command -v sshpass >/dev/null && SSHP=(sshpass -p "$FLASH_PW")
[ -n "$FLASH_PW" ] && [ ${#SSHP[@]} -eq 0 ] && echo "WARN: FLASH_PW set but sshpass missing — ssh will prompt (brew install hudochenkov/sshpass/sshpass)" >&2
SSH=("${SSHP[@]}" ssh "$FLASH_SSH")
SCP() { "${SSHP[@]}" scp "$@"; }
ps() { "${SSH[@]}" "powershell -NoProfile -ExecutionPolicy Bypass -Command \"$1\""; }

cmd="${1:-preflight}"; shift || true
case "$cmd" in
  preflight)
    echo "== ssh =="; "${SSH[@]}" "echo connected as %USERNAME% on %COMPUTERNAME%" || { echo "SSH failed — OpenSSH Server enabled on the box? hive key working? right FLASH_PW?"; exit 1; }
    echo "== exe ==";       ps "Test-Path C:\\mzpc\\target\\release\\mzpeak-convert.exe"
    echo "== worker ==";    ps "Test-Path '$WORKER_WIN'"
    echo "== pwiz ==";      ps "Test-Path '$FLASH_PWIZ'"
    echo "== validator =="; ps "(Get-Command mzpeak-validate -ErrorAction SilentlyContinue) -ne \$null"
    ;;
  push)   # push Mac-only datasets (private fixtures, the BAF .d) up to C:\data\<basename>
    [ "$#" -ge 1 ] || { echo "usage: push <local-path> ..."; exit 2; }
    ps "New-Item -ItemType Directory -Force -Path '$FLASH_DATA' | Out-Null"
    for p in "$@"; do echo ">> push $p"; SCP -r "$p" "$FLASH_SSH:$FLASH_DATA\\"; done
    ;;
  fetch)  # download public datasets ON the box from the manifest (dest<TAB>url[<TAB>unzip])
    m="${1:-$MANIFEST}"; [ -f "$m" ] || { echo "manifest not found: $m"; exit 2; }
    ps "New-Item -ItemType Directory -Force -Path '$FLASH_DATA' | Out-Null"
    grep -vE '^\s*#|^\s*$' "$m" | while IFS=$'\t' read -r dest url unzip; do
      [ -n "$url" ] || continue
      win="$FLASH_DATA\\$(echo "$dest" | tr '/' '\\')"
      echo ">> fetch $dest"
      ps "New-Item -ItemType Directory -Force -Path (Split-Path '$win') | Out-Null; curl.exe -fL --retry 3 -o '$win' '$url'"
      [ "${unzip:-}" = "unzip" ] && ps "tar -xf '$win' -C (Split-Path '$win'); Remove-Item '$win'"
    done
    ;;
  convert)
    extra=""; for a in "$@"; do case "$a" in --via-msconvert) extra="$extra -ViaMsconvert";; --purge) extra="$extra -PurgeInputs";; esac; done
    "${SSH[@]}" "powershell -NoProfile -ExecutionPolicy Bypass -File $WORKER_WIN -DataRoot $FLASH_DATA -OutRoot $FLASH_OUT -PwizDir '$FLASH_PWIZ'$extra"
    ;;
  pull)
    dst="${1:-$repo/out/flash}"; mkdir -p "$dst"
    echo ">> pull archives + results -> $dst"
    SCP -r "$FLASH_SSH:$FLASH_OUT\\archives" "$dst/" || true
    SCP    "$FLASH_SSH:$FLASH_OUT\\results.tsv" "$dst/" || true
    SCP -r "$FLASH_SSH:$FLASH_OUT\\logs" "$dst/" || true
    echo "== results =="; column -t -s$'\t' "$dst/results.tsv" 2>/dev/null || cat "$dst/results.tsv"
    ;;
  all)
    "$0" fetch "${1:-$MANIFEST}" && "$0" convert && "$0" pull
    ;;
  *) echo "unknown: $cmd (preflight|push|fetch|convert|pull|all)"; exit 2 ;;
esac
