#!/usr/bin/env bash
# Direct-SCP box conversion — NO S3. For each "src<TAB>out" line on stdin: scp the raw (+ vendor
# sidecars) to a box temp dir, convert on the box (box_local_convert.ps1), scp the .mzpeak back to
# <out>, clean the box temp. Converted mzPeak NEVER touches S3 (per user rule). Parallel via -P.
set -uo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
set -a; . "$here/box.env" 2>/dev/null; set +a
: "${BOX_SSH:?}" "${BOX_JUMP:?}" "${BOX_SSH_KEY:?}"
JOBS="${JOBS:-2}"
SSHOPT=(-i "$BOX_SSH_KEY" -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new -o ServerAliveInterval=15 -o ServerAliveCountMax=8 -o TCPKeepAlive=yes)
PROXY="ProxyCommand=ssh -i $BOX_SSH_KEY -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new -o ServerAliveInterval=15 -o ServerAliveCountMax=8 -W %h:%p $BOX_JUMP"

# stage the box-side helper once
scp "${SSHOPT[@]}" -o "$PROXY" "$here/box_local_convert.ps1" "$BOX_SSH:C:/Users/User/box_local_convert.ps1" >/dev/null 2>&1 \
  || { echo "FATAL: cannot stage box_local_convert.ps1" >&2; exit 1; }

export BOX_SSH BOX_JUMP BOX_SSH_KEY
sshb(){ ssh "${SSHOPT[@]}" -J "$BOX_JUMP" "$BOX_SSH" "$@"; }
scpb(){ scp "${SSHOPT[@]}" -o "$PROXY" "$@"; }
export -f sshb scpb
export SSHOPT_STR="-i $BOX_SSH_KEY -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new"
export PROXY

convert_one(){
  local src="$1" out="$2" tag; tag="$(basename "$out")"
  local uid; uid="$(python3 -c 'import uuid;print(uuid.uuid4().hex)')"
  local boxdir="C:/Users/User/scpconv/$uid"
  local d b; d="$(dirname "$src")"; b="$(basename "$src")"
  # vendor sidecars: wiff -> .wiff .wiff.scan .wiff2 ; otherwise just the file
  local members
  case "$b" in
    *.wiff) members="$(cd "$d" && ls "${b%.wiff}".wiff* 2>/dev/null)";;
    *)      members="$b";;
  esac
  ssh $SSHOPT_STR -J "$BOX_JUMP" "$BOX_SSH" "powershell -NoProfile -Command \"New-Item -ItemType Directory -Force -Path '$boxdir' | Out-Null\"" >/dev/null 2>&1 \
    || { echo "[$tag] FAIL: mkdir boxdir"; return 1; }
  local m
  for m in $members; do
    scp $SSHOPT_STR -o "$PROXY" "$d/$m" "$BOX_SSH:$boxdir/$m" >/dev/null 2>&1 \
      || { echo "[$tag] FAIL: scp up $m"; ssh $SSHOPT_STR -J "$BOX_JUMP" "$BOX_SSH" "powershell -NoProfile -Command \"Remove-Item -Recurse -Force '$boxdir'\"" >/dev/null 2>&1; return 1; }
  done
  local res err
  # keep stderr so an ssh/auth/ProxyCommand failure surfaces instead of an empty "FAIL: convert ()".
  res="$(ssh $SSHOPT_STR -J "$BOX_JUMP" "$BOX_SSH" "powershell -NoProfile -ExecutionPolicy Bypass -File C:/Users/User/box_local_convert.ps1 -InPath '$boxdir/$b' -OutPath '$boxdir/out.mzpeak'" 2>/tmp/bxc-$uid.err)"
  err="$(tr '\n' ' ' < /tmp/bxc-$uid.err 2>/dev/null | tail -c 200)"; rm -f /tmp/bxc-$uid.err
  local boxsz; boxsz="$(echo "$res" | grep -oE 'SIZE=[0-9]+' | cut -d= -f2)"
  if echo "$res" | grep -q 'EXIT=0' && [ -n "$boxsz" ] && [ "$boxsz" -gt 0 ] 2>/dev/null; then
    mkdir -p "$(dirname "$out")"
    if scp $SSHOPT_STR -o "$PROXY" "$BOX_SSH:$boxdir/out.mzpeak" "$out" >/dev/null 2>&1; then
      local locsz; locsz="$(stat -f %z "$out" 2>/dev/null || stat -c %s "$out" 2>/dev/null)"
      if [ "$locsz" = "$boxsz" ]; then echo "[$tag] OK (SIZE=$boxsz)"
      else echo "[$tag] FAIL: result size mismatch (box=$boxsz mac=$locsz) — truncated transfer"; rm -f "$out"; fi
    else
      echo "[$tag] FAIL: scp down result"
    fi
  else
    echo "[$tag] FAIL: convert (${res:+$res }${err:+stderr: $err})"
  fi
  ssh $SSHOPT_STR -J "$BOX_JUMP" "$BOX_SSH" "powershell -NoProfile -Command \"Remove-Item -Recurse -Force '$boxdir'\"" >/dev/null 2>&1
}
export -f convert_one

# stdin: "src out" pairs, space-separated (paths must not contain spaces). Run JOBS in parallel.
# `bash -c '...' _ src out` => $0=_, $1=src, $2=out.
grep -v '^[[:space:]]*$' | xargs -P "$JOBS" -L1 bash -c 'convert_one "$1" "$2"' _
echo "=== SCP-CONVERT DONE ==="
