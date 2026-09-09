#!/usr/bin/env bash
# box-convert — URL-driven, S3-relayed, parallel box conversions. See tools/box-convert-DESIGN.md.
#
#   box_convert.sh <src> <dst> [-- <converter opts...>]
#       <src> : local path | http(s) URL | s3://<bucket>/<key>   (s3 => the BOX pulls it directly)
#       <dst> : local path | s3://<bucket>/<key>                 (s3 => the box PUTs the FINAL key)
#   box_convert.sh --local <local-path> <dst> [-- <opts>]  # always host-stages the raw via S3
#   box_convert.sh --manifest       jobs.tsv [--jobs N]    # url|s3://<TAB>out<TAB>opts
#   box_convert.sh --local-manifest jobs.tsv [--jobs N]    # local-path<TAB>out<TAB>opts (corpus use)
#   flags: --local-copy PATH  where an s3:// dst is mirrored locally (default $CORPUS_ROOT/<key>)
#          --no-fetch         do not mirror an s3:// dst back to the host
#
# S3-FIRST: an s3:// source is handed to the box as a presigned GET, so bytes already in the corpus
# bucket are never round-tripped through the host. An s3:// target is PUT straight to its corpus key
# and is NOT deleted afterwards (it is the deliverable, not a relay slot); the host then mirrors it
# down for local reference. Multi-object units (.d, Waters .raw, .wiff+.scan, .imzML+.ibd) cannot be
# a single presigned GET, so those fall back to host staging automatically.
#
# On every invocation the box converter is brought to the newest RELEASE TAG before any job runs
# (BOX_AUTOUPDATE=1|check|0, BOX_CONVERTER_VERSION=vX.Y.Z, BOX_REQUIRE_VERSION=1 to abort if stale).
#
# The raw is pulled from its URL ON THE BOX, converted in an isolated temp dir, and the .mzpeak is
# relayed through S3 (box uploads via a presigned PUT; host downloads, verifies size+md5, deletes).
# Config (env or a gitignored tools/box.env): BOX_SSH BOX_JUMP BOX_SSH_KEY [BOX_CONVERTER]
# [S3_PREFIX] [PUT_EXPIRES] [ARCHIVE=true]  plus s3_relay's S3_BUCKET/S3_ENDPOINT/S3_REGION/AWS_PROFILE.
set -uo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
[ -f "$here/box.env" ] && . "$here/box.env"

usage(){ sed -n '3,12p' "$0" | sed 's/^# \{0,1\}//' >&2; exit 2; }

: "${BOX_SSH:?set BOX_SSH=user@flash-host (env or tools/box.env)}"
: "${BOX_JUMP:?set BOX_JUMP=user@jumphost}"
: "${BOX_SSH_KEY:?set BOX_SSH_KEY=/path/to/ssh/key}"
S3_PREFIX="${S3_PREFIX:-box-convert}"
PUT_EXPIRES="${PUT_EXPIRES:-21600}"
ARCHIVE="${ARCHIVE:-false}"
CORPUS_ROOT="${CORPUS_ROOT:-$HOME/Claude/mzpeak-example-data/data}"   # where s3:// targets mirror to
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
trap cleanup EXIT
# INT/TERM: clean up AND die. `trap cleanup INT TERM` alone runs the handler and then RESUMES, so
# Ctrl-C tore down the relay keys of in-flight jobs while the pool kept launching more.
trap 'cleanup; trap - EXIT; exit 130' INT
trap 'cleanup; trap - EXIT; exit 143' TERM

uuid(){ python3 -c 'import uuid;print(uuid.uuid4().hex)'; }

stage_remote(){  # push the current remote scripts once (idempotent)
  # Both, always: host and box logic must never drift apart.
  local f
  for f in box_convert_remote.ps1 box_update_remote.ps1; do
    scp -i "$BOX_SSH_KEY" -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new -o "$PROXY" \
      "$here/$f" "$BOX_SSH:C:\\Users\\User\\$f" >/dev/null 2>&1 \
      || { echo "FATAL: could not stage $f to the box" >&2; return 1; }
  done
}

ssh_watchdog(){  # secs cmd... -> run a box command under a hard wall-clock cap
  # macOS has no timeout(1); ServerAlive* only catches an IDLE link, and a cargo build is long but
  # not idle. Without this a wedged build hangs the invocation forever.
  local secs="$1"; shift
  "${SSH[@]}" "$BOX_SSH" "$@" & local p=$!
  ( sleep "$secs"; kill -TERM "$p" 2>/dev/null; sleep 3; kill -KILL "$p" 2>/dev/null ) & local w=$!
  wait "$p" 2>/dev/null; local rc=$?
  kill "$w" 2>/dev/null; wait "$w" 2>/dev/null
  return $rc
}

sync_box_converter(){  # once per invocation, before any job: make the box's converter current
  [ "${BOX_AUTOUPDATE:-1}" = "0" ] && return 0
  local want="${BOX_CONVERTER_VERSION:-latest}" build=true
  [ "${BOX_AUTOUPDATE:-1}" = "check" ] && build=false
  local job resp b64 fields ok action have wantv exe err
  job="$(REPO="${BOX_REPO:-C:\\Users\\User\\src\\mzPeakConverter}" WANT="$want" \
         BIN="${BOX_BIN_DIR:-C:\\Users\\User\\bin}" BUILD="$build" python3 -c '
import json,os
print(json.dumps({"repo":os.environ["REPO"],"want":os.environ["WANT"],
                  "bin_dir":os.environ["BIN"],"build":os.environ["BUILD"]=="true"}))')"
  resp="$(printf '%s' "$job" | ssh_watchdog "${BOX_UPDATE_TIMEOUT:-7200}" \
          'powershell -NoProfile -ExecutionPolicy Bypass -File C:\\Users\\User\\box_update_remote.ps1' 2>/dev/null)"
  b64="$(printf '%s\n' "$resp" | sed -n '/<<<BOXSYNC/,/BOXSYNC>>>/p' | sed '1d;$d' | tr -d '\r\n ')"
  if [ -z "$b64" ]; then
    echo "box converter: no reply from the update probe — using the installed exe" >&2
    [ "${BOX_REQUIRE_VERSION:-0}" = 1 ] && return 1 || return 0
  fi
  fields="$(printf '%s' "$b64" | python3 -c '
import sys,base64,json
try: r=json.loads(base64.b64decode(sys.stdin.read()))
except Exception: print("ERR"); sys.exit()
c=lambda s:str(s).replace("\t"," ").replace("\n"," ").replace("\r"," ")
for v in [c(r.get("action","")),c(r.get("have","")),c(r.get("want","")),c(r.get("exe","")),c(r.get("error",""))]:
    print(v)')"
  { IFS= read -r action; IFS= read -r have; IFS= read -r wantv; IFS= read -r exe; IFS= read -r err; } <<EOF
$fields
EOF
  case "$action" in
    current|updated)
      [ -n "$exe" ] && export BOX_CONVERTER="$exe"
      echo "box converter: $action (have $have, want $wantv)" >&2; return 0;;
    behind)
      echo "box converter: BEHIND (have $have, latest $wantv) — check mode, not building" >&2;;
    skipped-busy|skipped-locked)
      echo "box converter: update $action — using the installed exe" >&2;;
    *)
      echo "box converter: update FAILED ($action${err:+: $err}) — using the installed exe" >&2;;
  esac
  # Soft by default: an ad-hoc conversion should not die because GitHub was unreachable. Corpus
  # callers set BOX_REQUIRE_VERSION=1, because they STAMP archives with a version string — a stale
  # box exe there produces silently mislabelled output.
  [ "${BOX_REQUIRE_VERSION:-0}" = 1 ] && return 1 || return 0
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
    # S3-FIRST: a durable s3:// target is ALREADY the deliverable -- the box PUT it to its final
    # corpus key. Mirror it down for local reference (the corpus keeps a local copy so the
    # descriptor-driven harness can stamp and re-verify it), but never treat the local file as the
    # source of truth. --no-fetch skips the mirror entirely.
    local dst="$out"
    case "$out" in
      s3://*)
        if [ "${NO_FETCH:-0}" = "1" ]; then
          echo "[$tag] OK exit=0 size=$R_SIZE -> $out (no local mirror)"; return 0
        fi
        dst="${LOCAL_COPY:-$CORPUS_ROOT/$key}"
        echo "[$tag] durable target $out -> mirroring to $dst" >&2 ;;
    esac
    mkdir -p "$(dirname "$dst")" 2>/dev/null || true
    local part="$dst.$uid.part"
    "${RELAY[@]}" get "$key" "$part" \
      || { echo "[$tag] FAIL: S3 get" >&2; rm -f "$part"; return 1; }
    local gotsize; gotsize="$(wc -c < "$part" | tr -d ' ')"
    if [ "$gotsize" != "$R_SIZE" ]; then
      echo "[$tag] FAIL: size mismatch (box=$R_SIZE got=$gotsize)" >&2; rm -f "$part"; return 1; fi
    local gotmd5; gotmd5="$("${RELAY[@]}" md5 "$part")"
    if [ "$gotmd5" != "$R_MD5" ]; then
      echo "[$tag] FAIL: md5 mismatch (box=$R_MD5 got=$gotmd5)" >&2; rm -f "$part"; return 1; fi
    mv -f "$part" "$dst"
    if [ "$dst" = "$out" ]; then echo "[$tag] OK exit=0 size=$R_SIZE -> $out"
    else echo "[$tag] OK exit=0 size=$R_SIZE -> $out (mirrored to $dst)"; fi
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
  # NOT UPLOADED => FAILURE, whatever the converter's own exit code was. The box reports the
  # CONVERTER's exit in `exit` and ships/doesn't-ship in `uploaded`; it deliberately refuses to ship
  # in three cases while leaving exit=0 (archive over the 5 GB single-PUT limit, the PUT itself
  # failing, and exit-0-but-no-output). Returning $R_EXIT there handed back 0 = SUCCESS: the pool
  # counted a pass, "0 job(s) failed" was printed, and corpus_reconvert stamped the STALE local
  # archive with the new converter version. Live case: a 10.55 GB archive is 2x the PUT ceiling.
  [ "$R_UP" = "1" ] || return 1
  case "$R_EXIT" in ''|0|*[!0-9]*) return 1;; *) return "$R_EXIT";; esac   # guard non-numeric exit
}

resolve_source(){  # src -> prints a URL the BOX can fetch; rc=2 means "fall back to host staging"
  # S3-FIRST: when the raw already lives in the corpus bucket, hand the box a presigned GET so it
  # pulls DIRECTLY. The old path downloaded it to the host, re-tarred it and re-uploaded it to the
  # same bucket purely so the box could fetch it -- a full round-trip of bytes that were already
  # there, and the source of the stranded box-convert/raw/*.tar keys when a run is killed.
  #
  # NOTE this runs in a COMMAND SUBSTITUTION, i.e. a subshell: it can only communicate on stdout and
  # through its exit code. Never set a caller-visible variable here (an earlier draft set ARCHIVE=true
  # for .tar sources and it was silently discarded, so the box never unpacked them). ARCHIVE is
  # therefore decided by the caller, from the same key, via source_is_archive().
  case "$1" in
    s3://*)
      local rest bkt key n
      rest="${1#s3://}"; bkt="${rest%%/*}"; key="${rest#*/}"
      [ "$bkt" = "$rest" ] && { echo "malformed s3 uri: $1" >&2; return 2; }
      # EXACT object, not a prefix: `ls <key>` is a prefix match, so a sibling `<key>.built` (which
      # update.sh does NOT exclude, unlike *.sig) would make a perfectly good single object look
      # like a 2-object unit. head_object is exact.
      S3_BUCKET="$bkt" "${RELAY[@]}" head "$key" >/dev/null 2>&1 || {
        # not an object -- is it a directory-style unit? then it must be host-staged.
        n="$(S3_BUCKET="$bkt" "${RELAY[@]}" ls "$key/" --count 2>/dev/null)"
        [ "${n:-0}" -gt 0 ] 2>/dev/null \
          && echo "s3 source $1 is a $n-object unit (.d/.raw/.wiff set) -- staging locally" >&2 \
          || echo "s3 source $1 not found -- staging locally" >&2
        return 2; }
      case "${key##*/}" in *.*) ;; *) echo "s3 key has no extension; box format detection would fail" >&2; return 2;; esac
      S3_BUCKET="$bkt" "${RELAY[@]}" presign-get "$key" --expires "$PUT_EXPIRES" ;;
    http://*|https://*) printf '%s\n' "$1" ;;
    *) return 2 ;;
  esac
}

source_is_archive(){  # the box only sniffs .zip/.tgz/.tar.gz from the URL; .tar must be told
  case "${1%%\?*}" in *.tar|*.tgz|*.tar.gz|*.zip) return 0 ;; *) return 1 ;; esac
}

one_job(){  # raw out opts — mints+registers a staging key, runs, publishes only what verified
  # An s3:// out is a DURABLE corpus key. The box must never PUT straight to it: the host can only
  # verify AFTER the upload, so a truncated or corrupted body would already have replaced published
  # data with no way back (and --no-fetch would not even notice). Instead the box always writes an
  # ephemeral staging key; the host verifies size (and md5, unless --no-fetch); only then is the
  # object published with a SERVER-SIDE copy, which moves no bytes through this host. The staging
  # key stays registered with the exit trap throughout, so a kill leaves nothing behind.
  local uid key rc=0 dst_s3=""
  uid="$(uuid)"
  local dst_bkt=""
  case "$2" in
    s3://*) dst_bkt="${2#s3://}"; dst_s3="${dst_bkt#*/}"; dst_bkt="${dst_bkt%%/*}"
            # The staging key, the publish copy and the delete must all live in the bucket the
            # CALLER named -- not S3_BUCKET's default -- or a cross-bucket target silently writes
            # somewhere else. s3_relay reads S3_BUCKET, so pin it for this job.
            [ -n "$dst_bkt" ] && export S3_BUCKET="$dst_bkt" ;;
  esac
  key="$S3_PREFIX/$uid.mzpeak"
  printf '%s' "$key" > "$PENDING_DIR/$uid"          # exit-trap backstop
  run_job "$1" "$2" "${3:-}" "$key" "$uid" || rc=$?
  if [ "$rc" = 0 ] && [ -n "$dst_s3" ]; then
    if [ "${OVERWRITE:-0}" != "1" ] && "${RELAY[@]}" head "$dst_s3" >/dev/null 2>&1; then
      echo "[$(basename "$2")] REFUSING to overwrite existing $2 (pass --overwrite)" >&2
      rc=5
    elif "${RELAY[@]}" copy "$key" "$dst_s3" >/dev/null 2>&1; then
      echo "[$(basename "$2")] published -> s3://${S3_BUCKET:-v09}/$dst_s3" >&2
    else
      echo "[$(basename "$2")] FAIL: could not publish staging key to $2" >&2
      rc=1
    fi
  fi
  if "${RELAY[@]}" delete "$key" >/dev/null 2>&1; then
    rm -f "$PENDING_DIR/$uid"                        # unregister only on confirmed delete
  else
    echo "[$(basename "$2")] WARN: S3 delete failed; trap will retry $key at exit" >&2
  fi
  return $rc
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
# S3-first knobs: --local-copy PATH (where an s3:// target mirrors locally), --no-fetch (skip it)
while [ $# -gt 0 ]; do
  case "$1" in
    --local-copy) LOCAL_COPY="$2"; shift 2 ;;
    --no-fetch)   NO_FETCH=1; shift ;;
    --overwrite)  OVERWRITE=1; shift ;;
    *) break ;;
  esac
done

stage_remote || exit 1
sync_box_converter || exit 3   # exit 3 only when BOX_REQUIRE_VERSION=1
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
    # Scheme dispatch: s3:// and http(s):// are fetched BY THE BOX (S3-first); a local path still
    # gets host-staged via local_job. A bare URL behaves exactly as it always did.
    [ $# -ge 2 ] || usage
    raw="$1"; out="$2"; shift 2
    [ "${1:-}" = "--" ] && shift
    if src="$(resolve_source "$raw")"; then
      source_is_archive "$raw" && ARCHIVE=true    # set HERE: resolve_source runs in a subshell
      one_job "$src" "$out" "$*"
    else
      [ $? -eq 2 ] || exit 1
      # Falling back to host staging. local_job takes a LOCAL PATH, so an s3:// source (a
      # multi-object .d/.raw/.wiff unit, which cannot be one presigned GET) must first be mapped
      # back to its local corpus copy. Handing local_job the URI itself would tar a path that does
      # not exist and fail deep inside the relay with a confusing error.
      stage="$raw"
      case "$raw" in
        s3://*) stage="$CORPUS_ROOT/$(k="${raw#s3://}"; echo "${k#*/}")"
                [ -e "$stage" ] || { echo "cannot stage $raw: no local copy at $stage" >&2; exit 1; } ;;
      esac
      local_job "$stage" "$out" "$*"
    fi
    ;;
esac
