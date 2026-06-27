#!/usr/bin/env bash
# box-convert — URL-driven, S3-relayed, parallel box conversions. See tools/box-convert-DESIGN.md.
#
#   box_convert.sh <raw-url> <out.mzpeak> [-- <converter opts...>]
#   box_convert.sh --manifest jobs.tsv [--jobs N]      # jobs.tsv: raw_url <TAB> out_path <TAB> opts
#
# The raw is pulled from its URL ON THE BOX, converted in an isolated temp dir, and the .mzpeak is
# relayed through S3 (box uploads via a presigned PUT; host downloads, verifies size+md5, deletes).
# Config (env or a gitignored tools/box.env): BOX_SSH BOX_JUMP BOX_SSH_KEY [BOX_CONVERTER]
# [S3_PREFIX] [PUT_EXPIRES] [ARCHIVE=true]  plus s3_relay's S3_BUCKET/S3_ENDPOINT/S3_REGION/AWS_PROFILE.
set -uo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
[ -f "$here/box.env" ] && . "$here/box.env"

usage(){ sed -n '3,8p' "$0" | sed 's/^# \{0,1\}//' >&2; exit 2; }

: "${BOX_SSH:?set BOX_SSH=user@flash-host (env or tools/box.env)}"
: "${BOX_JUMP:?set BOX_JUMP=user@jumphost}"
: "${BOX_SSH_KEY:?set BOX_SSH_KEY=/path/to/ssh/key}"
S3_PREFIX="${S3_PREFIX:-box-convert}"
PUT_EXPIRES="${PUT_EXPIRES:-21600}"
ARCHIVE="${ARCHIVE:-false}"
REMOTE_PS='C:\Users\User\box_convert_remote.ps1'
RELAY=(python3 "$here/s3_relay.py")
PROXY="ProxyCommand=ssh -i $BOX_SSH_KEY -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new -W %h:%p $BOX_JUMP"
SSH=(ssh -i "$BOX_SSH_KEY" -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new
     -o ConnectTimeout=30 -o ServerAliveInterval=30 -o ServerAliveCountMax=240 -o "$PROXY")

# --- always delete any S3 key we minted, even on Ctrl-C / SIGTERM / mid-job crash ---
PENDING_DIR="$(mktemp -d "${TMPDIR:-/tmp}/bxc-pending.XXXXXX")"
cleanup(){
  [ -d "$PENDING_DIR" ] || return 0
  for f in "$PENDING_DIR"/*; do
    [ -e "$f" ] || continue
    "${RELAY[@]}" delete "$(cat "$f")" >/dev/null 2>&1
  done
  rm -rf "$PENDING_DIR"
}
trap cleanup EXIT INT TERM

uuid(){ python3 -c 'import uuid;print(uuid.uuid4().hex)'; }

stage_remote(){  # push the current remote script once (idempotent)
  scp -i "$BOX_SSH_KEY" -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new -o "$PROXY" \
    "$here/box_convert_remote.ps1" "$BOX_SSH:$REMOTE_PS" >/dev/null 2>&1 \
    || { echo "FATAL: could not stage box_convert_remote.ps1 to the box" >&2; return 1; }
}

run_job(){  # raw_url out_path opts key uuid -> converter exit (1 on relay/ssh/verify failure)
  local raw="$1" out="$2" opts="${3:-}" key="$4" uid="$5" tag; tag="$(basename "$out")"
  local put
  put="$("${RELAY[@]}" presign-put "$key" --expires "$PUT_EXPIRES")" \
    || { echo "[$tag] FAIL: presign" >&2; return 1; }
  # job json built by python so quoting/escaping is correct; URLs passed via ENV (not argv, so they
  # don't show in the host's `ps`) and reach the box only on its STDIN
  local job
  job="$(RAW="$raw" PUT="$put" OPTS="$opts" ARCH="$ARCHIVE" CONV="${BOX_CONVERTER:-}" python3 - <<'PY'
import json,os
print(json.dumps({"raw_url":os.environ["RAW"],"put_url":os.environ["PUT"],"opts":os.environ["OPTS"],
                  "archive":os.environ["ARCH"]=="true","converter":os.environ["CONV"] or None}))
PY
)"
  local resp
  resp="$(printf '%s' "$job" | "${SSH[@]}" "$BOX_SSH" \
            "powershell -NoProfile -ExecutionPolicy Bypass -File $REMOTE_PS" 2>/dev/null)"
  local b64
  b64="$(printf '%s\n' "$resp" | sed -n '/<<<BOXRESULT/,/BOXRESULT>>>/p' | sed '1d;$d' | tr -d '\r\n ')"
  [ -z "$b64" ] && { echo "[$tag] FAIL: no result from box (ssh/powershell)" >&2; return 1; }
  # decode base64(json) -> one scalar field PER LINE (newline-delimited preserves empty fields,
  # which a tab/whitespace-IFS read would collapse). uploaded normalised to 1/0; values stripped of
  # CR/LF/TAB so each is exactly one line.
  local fields
  fields="$(printf '%s' "$b64" | python3 -c '
import sys,base64,json
try: r=json.loads(base64.b64decode(sys.stdin.read()))
except Exception: print("ERR"); sys.exit()
c=lambda s:str(s).replace("\t"," ").replace("\n"," ").replace("\r"," ")
for v in [c(r.get("stage","")),str(r.get("exit","")),"1" if r.get("uploaded") else "0",
          str(r.get("size","")),str(r.get("md5","")),c(r.get("error","")),c(r.get("note",""))]:
    print(v)')"
  [ "$fields" = "ERR" ] && { echo "[$tag] FAIL: unparseable result from box" >&2; return 1; }
  local R_STAGE R_EXIT R_UP R_SIZE R_MD5 R_ERR R_NOTE
  { IFS= read -r R_STAGE; IFS= read -r R_EXIT; IFS= read -r R_UP; IFS= read -r R_SIZE
    IFS= read -r R_MD5;   IFS= read -r R_ERR;  IFS= read -r R_NOTE; } <<EOF
$fields
EOF
  [ -n "$R_NOTE" ] && echo "[$tag] note: $R_NOTE" >&2
  if [ "$R_UP" = "1" ] && [ "$R_EXIT" = "0" ]; then
    mkdir -p "$(dirname "$out")" 2>/dev/null || true
    local part="$out.$uid.part"
    "${RELAY[@]}" get "$key" "$part" \
      || { echo "[$tag] FAIL: S3 get" >&2; rm -f "$part"; return 1; }
    local gotsize; gotsize="$(wc -c < "$part" | tr -d ' ')"
    if [ "$gotsize" != "$R_SIZE" ]; then
      echo "[$tag] FAIL: size mismatch (box=$R_SIZE got=$gotsize)" >&2; rm -f "$part"; return 1; fi
    local gotmd5; gotmd5="$("${RELAY[@]}" md5 "$part")"
    if [ "$gotmd5" != "$R_MD5" ]; then
      echo "[$tag] FAIL: md5 mismatch (box=$R_MD5 got=$gotmd5)" >&2; rm -f "$part"; return 1; fi
    mv -f "$part" "$out"
    echo "[$tag] OK exit=0 size=$R_SIZE -> $out"
    return 0
  fi
  echo "[$tag] CONV-FAIL stage=$R_STAGE exit=$R_EXIT error=${R_ERR:-<none>}" >&2
  printf '%s' "$b64" | python3 -c 'import sys,base64,json;print(json.loads(base64.b64decode(sys.stdin.read())).get("log","") or "")' \
    | tail -8 | sed "s/^/[$tag] log| /" >&2
  case "$R_EXIT" in ''|*[!0-9]*) return 1;; *) return "$R_EXIT";; esac   # guard non-numeric exit
}

one_job(){  # raw out opts — mints+registers a key, runs, ALWAYS deletes (surfacing real errors)
  local uid; uid="$(uuid)"; local key="$S3_PREFIX/$uid.mzpeak" rc=0
  printf '%s' "$key" > "$PENDING_DIR/$uid"          # register for the exit-trap backstop
  run_job "$1" "$2" "${3:-}" "$key" "$uid" || rc=$?
  if "${RELAY[@]}" delete "$key" >/dev/null 2>&1; then
    rm -f "$PENDING_DIR/$uid"                        # unregister only on confirmed delete
  else
    echo "[$(basename "$2")] WARN: S3 delete failed; trap will retry $key at exit" >&2
  fi
  return $rc
}

# ---- entry ----
stage_remote || exit 1
if [ "${1:-}" = "--manifest" ]; then
  mf="${2:?manifest path}"; jobs=4
  [ "${3:-}" = "--jobs" ] && jobs="${4:-4}"
  [ -f "$mf" ] || { echo "no such manifest: $mf" >&2; exit 2; }
  # Simple head-of-line FIFO pool (true wait-any needs bash>=4.3; macOS ships 3.2). Fine for
  # similar-duration jobs; a slow head can briefly under-fill — acceptable for the corpus.
  pids=(); fails=0
  while IFS=$'\t' read -r raw out opts || [ -n "$raw" ]; do
    case "$raw" in ''|'#'*) continue;; esac
    [ -z "$out" ] && { echo "skip: manifest line missing out_path for $raw" >&2; continue; }
    one_job "$raw" "$out" "$opts" & pids+=("$!")
    if [ "${#pids[@]}" -ge "$jobs" ]; then
      wait "${pids[0]}" || fails=$((fails+1)); pids=("${pids[@]:1}")
    fi
  done < "$mf"
  for p in ${pids[@]+"${pids[@]}"}; do wait "$p" || fails=$((fails+1)); done
  echo "manifest done: $fails job(s) failed" >&2
  [ "$fails" -eq 0 ]
else
  [ $# -ge 2 ] || usage
  raw="$1"; out="$2"; shift 2
  [ "${1:-}" = "--" ] && shift
  one_job "$raw" "$out" "$*"
fi
