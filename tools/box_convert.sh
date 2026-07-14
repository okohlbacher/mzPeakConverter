#!/usr/bin/env bash
# box-convert — URL-driven, S3-relayed, parallel box conversions. See tools/box-convert-DESIGN.md.
#
#   box_convert.sh <raw-url> <out.mzpeak> [-- <converter opts...>]
#   box_convert.sh --local <local-path> <out.mzpeak> [-- <opts>]   # stage a LOCAL unit via S3
#   box_convert.sh --manifest       jobs.tsv [--jobs N]    # url<TAB>out<TAB>opts
#   box_convert.sh --local-manifest jobs.tsv [--jobs N]    # local-path<TAB>out<TAB>opts (corpus use)
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
     -o ConnectTimeout=30 -o ServerAliveInterval=30 -o ServerAliveCountMax=30 -o "$PROXY")

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
          str(r.get("size","")),str(r.get("md5","")),c(r.get("error","")),c(r.get("note","")),
          str(r.get("conv_s","")),str(r.get("msconv_s","")),str(r.get("dl_s","")),str(r.get("up_s","")),str(r.get("raw_bytes",""))]:
    print(v)')"
  [ "$fields" = "ERR" ] && { echo "[$tag] FAIL: unparseable result from box" >&2; return 1; }
  local R_STAGE R_EXIT R_UP R_SIZE R_MD5 R_ERR R_NOTE R_CONV R_MSCONV R_DL R_UPS R_RAW
  { IFS= read -r R_STAGE; IFS= read -r R_EXIT; IFS= read -r R_UP; IFS= read -r R_SIZE
    IFS= read -r R_MD5;   IFS= read -r R_ERR;  IFS= read -r R_NOTE
    IFS= read -r R_CONV;  IFS= read -r R_MSCONV; IFS= read -r R_DL; IFS= read -r R_UPS; IFS= read -r R_RAW; } <<EOF
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
    if [ -n "${BENCH_TSV:-}" ]; then   # per-stage box timings for benchmark collection
      [ -f "$BENCH_TSV" ] || printf 'iso_time\tunit\tconverter\topts\traw_bytes\tmzpeak_bytes\tconv_s\tmsconv_s\tdl_s\tup_s\thost\n' > "$BENCH_TSV"
      printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\tbox\n' \
        "$(date -u +%FT%TZ)" "$(basename "$out")" "${BOX_CONVERTER:-}" "$opts" \
        "${R_RAW:-}" "$R_SIZE" "${R_CONV:-}" "${R_MSCONV:-}" "${R_DL:-}" "${R_UPS:-}" >> "$BENCH_TSV"
    fi
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

_unit_members(){  # tar member names (relative to the unit's dir) for a unit + its vendor sidecars
  local p="$1" d b; d="$(dirname "$p")"; b="$(basename "$p")"
  if [ -d "$p" ]; then printf '%s\n' "$b"; return; fi
  case "$b" in
    *.wiff) ( cd "$d" && ls "${b%.wiff}".wiff* 2>/dev/null );;   # foo.wiff foo.wiff.scan foo.wiff2
    *)      printf '%s\n' "$b";;
  esac
}

local_job(){  # local-path out opts — stage a LOCAL unit to S3, convert via its presigned-GET url
  local path="$1" out="$2" opts="${3:-}" tag; tag="$(basename "$out")"
  [ -e "$path" ] || { echo "[$tag] FAIL: no such path: $path" >&2; return 1; }
  local uid; uid="$(uuid)"; local d; d="$(dirname "$path")"
  local members=(); while IFS= read -r m; do [ -n "$m" ] && members+=("$m"); done < <(_unit_members "$path")
  [ "${#members[@]}" -eq 0 ] && members=("$(basename "$path")")
  local archive rawkey tmptar=""
  if [ -d "$path" ] || [ "${#members[@]}" -gt 1 ]; then
    tmptar="$(mktemp -t bxcraw.XXXXXX)" || return 1            # bundle (uncompressed: binary)
    # COPYFILE_DISABLE: stop macOS tar injecting ._* AppleDouble files into the archive
    ( cd "$d" && COPYFILE_DISABLE=1 tar -cf "$tmptar" "${members[@]}" ) \
      || { echo "[$tag] FAIL: tar" >&2; rm -f "$tmptar"; return 1; }
    rawkey="$S3_PREFIX/raw/$uid.tar"; archive=true
  else
    rawkey="$S3_PREFIX/raw/$uid/$(basename "$path")"; archive=false   # keep ext for format detection
  fi
  printf '%s' "$rawkey" > "$PENDING_DIR/raw-$uid"               # raw key joins the cleanup trap
  local rc=0
  if "${RELAY[@]}" put "$rawkey" "${tmptar:-$path}"; then
    local raw_url; raw_url="$("${RELAY[@]}" presign-get "$rawkey")"
    ARCHIVE=$archive one_job "$raw_url" "$out" "$opts" || rc=$?
  else
    echo "[$tag] FAIL: raw upload to S3" >&2; rc=1
  fi
  [ -n "$tmptar" ] && rm -f "$tmptar"
  "${RELAY[@]}" delete "$rawkey" >/dev/null 2>&1 && rm -f "$PENDING_DIR/raw-$uid"
  return $rc
}

run_pool(){  # manifest job_fn jobs — bounded FIFO pool over path/url<TAB>out<TAB>opts lines
  local mf="$1" fn="$2" jobs="$3" pids=() fails=0 a out opts
  while IFS=$'\t' read -r a out opts || [ -n "$a" ]; do
    case "$a" in ''|'#'*) continue;; esac
    [ -z "$out" ] && { echo "skip: manifest line missing out_path for $a" >&2; continue; }
    "$fn" "$a" "$out" "$opts" & pids+=("$!")
    if [ "${#pids[@]}" -ge "$jobs" ]; then wait "${pids[0]}" || fails=$((fails+1)); pids=("${pids[@]:1}"); fi
  done < "$mf"
  for p in ${pids[@]+"${pids[@]}"}; do wait "$p" || fails=$((fails+1)); done
  echo "manifest done: $fails job(s) failed" >&2
  [ "$fails" -eq 0 ]
}

# ---- entry ----
stage_remote || exit 1
mode="${1:-}"
case "$mode" in
  --manifest|--local-manifest)
    mf="${2:?manifest path}"; jobs=1
    [ "${3:-}" = "--jobs" ] && jobs="${4:-1}"
    # DISK SAFETY — sequential by default. Each concurrent --via-msconvert job writes a full
    # multi-GB (SWATH/DIA: tens-of-GB) mzML intermediate to the box temp disk; running several at
    # once fills the disk and the jobs stall mid-convert. Clamp to 1 unless explicitly overridden.
    if [ "$jobs" -gt 1 ] && [ "${MZPC_ALLOW_PARALLEL:-}" != "1" ]; then
      echo "box_convert: --jobs $jobs clamped to 1 (box disk safety; set MZPC_ALLOW_PARALLEL=1 to override)" >&2
      jobs=1
    fi
    [ -f "$mf" ] || { echo "no such manifest: $mf" >&2; exit 2; }
    if [ "$mode" = "--local-manifest" ]; then run_pool "$mf" local_job "$jobs"
    else run_pool "$mf" one_job "$jobs"; fi
    ;;
  --local)
    [ $# -ge 3 ] || usage
    path="$2"; out="$3"; shift 3; [ "${1:-}" = "--" ] && shift
    local_job "$path" "$out" "$*"
    ;;
  *)
    [ $# -ge 2 ] || usage
    raw="$1"; out="$2"; shift 2
    [ "${1:-}" = "--" ] && shift
    one_job "$raw" "$out" "$*"
    ;;
esac
