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
FETCH_LIST="$(mktemp "${TMPDIR:-/tmp}/bxc-fetch.XXXXXX")"   # deferred local mirrors (drain_fetch_list)
cleanup(){
  rm -f "$FETCH_LIST"
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
  # >/dev/null is LOad-BEARING: without it the watchdog inherits the caller's stdout, and when that
  # caller is a command substitution -- resp="$(ssh_watchdog ...)" in sync_box_converter -- the
  # orphaned `sleep` keeps the pipe's WRITE end open, so the substitution blocks until the timeout
  # expires even though ssh returned in seconds. Live effect: every invocation sat for the full
  # 7200 s before running its first job.
  ( sleep "$secs"; kill -TERM "$p" 2>/dev/null; sleep 3; kill -KILL "$p" 2>/dev/null ) >/dev/null 2>&1 &
  local w=$!
  wait "$p" 2>/dev/null; local rc=$?
  kill "$w" 2>/dev/null                 # the subshell...
  pkill -P "$w" 2>/dev/null             # ...and the `sleep` it is parked in, which outlives it
  wait "$w" 2>/dev/null
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
      # TRANSIENT, not a failure: another run holds the build lock, or a conversion holds the exe
      # open. The installed exe is still perfectly good, but this fell through to the hard-fail
      # below and aborted the entire corpus run (box_convert.sh exit 3, zero jobs). Retry a few
      # times -- the lock clears in seconds -- and accept immediately if the box already reports the
      # version we want.
      if [ -n "$have" ] && [ -n "${BOX_CONVERTER_VERSION:-}" ] \
         && [ "$have" = "${BOX_CONVERTER_VERSION#v}" ]; then
        echo "box converter: $action, but installed $have is the wanted version" >&2; return 0; fi
      if [ "${_bx_try:-1}" -lt "${BOX_UPDATE_RETRIES:-4}" ]; then
        echo "box converter: $action — retry ${_bx_try:-1}/${BOX_UPDATE_RETRIES:-4} in ${BOX_UPDATE_BACKOFF:-20}s" >&2
        sleep "${BOX_UPDATE_BACKOFF:-20}"
        _bx_try=$(( ${_bx_try:-1} + 1 )); sync_box_converter; return $?
      fi
      echo "box converter: update $action after ${_bx_try:-1} attempt(s) — using the installed exe" >&2;;
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
  job="$(RAW="$raw" PUT="$put" OPTS="$opts" ARCH="$ARCHIVE" CONV="${BOX_CONVERTER:-}" \
         DESC="${UNIT_DESC:-}" \
         RCACHE="${MZPC_BOX_RAW_CACHE:-}" RCLOCK="${MZPC_BOX_CACHE_LOCK_WAIT:-}" \
         RCVERIFY="${MZPC_BOX_CACHE_VERIFY:-}" python3 - <<'PY'
import json,os
j={"raw_url":os.environ["RAW"],"put_url":os.environ["PUT"],"opts":os.environ["OPTS"],
   "archive":os.environ["ARCH"]=="true","converter":os.environ["CONV"] or None}
# Box raw-cache knobs travel in the JOB JSON, not the environment: this ssh invocation forwards no
# env (no SendEnv, and Windows sshd does not accept env by default), so a host-side
# `MZPC_BOX_RAW_CACHE=off box_convert.sh ...` was silently ignored -- the feature's kill-switch was
# reachable only by hand-authoring C:\Users\User\box_convert_env.ps1 on the box. Omitted when unset,
# so the box keeps its own defaults and an older box script simply ignores the extra fields.
for _k, _f in (("RCACHE", "raw_cache"), ("RCLOCK", "raw_cache_lock_wait"), ("RCVERIFY", "raw_cache_verify")):
    if os.environ.get(_k):
        j[_f] = os.environ[_k]
d=json.loads(os.environ["DESC"]) if os.environ.get("DESC") else None
if d:
    # A unit ALREADY in the bucket. A single zipped object rides the original single-URL path (the
    # box already sniffs .zip/.tgz and extracts it); anything else is handed to the box as a member
    # list it reconstructs itself, so multi-object .d / .wiff+.scan units cost the host no bytes.
    if d.get("archive") and len(d["members"]) == 1:
        j["raw_url"], j["archive"] = d["members"][0]["url"], True
    else:
        j["raw_urls"], j["primary"], j["archive"] = d["members"], d["primary"], False
        # Stable unit identity for the box's persistent raw cache. It must come from the HOST: the
        # only thing the box sees otherwise is a presigned URL, whose signature changes every run.
        # Members already carry size+mtime (s3_relay presign-unit), which is what the box diffs
        # against its cached copy. Absent => the box caches nothing and behaves exactly as before.
        if d.get("unit_key"):
            j["unit_key"] = d["unit_key"]
print(json.dumps(j))
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
  # A native-reader failure DEGRADES to msconvert and still exits 0, uploads, and publishes: the only
  # trace is one word inside the note, which nothing counted. That was tolerable while jobs ran one at
  # a time; now that several vendor backends (Shimadzu COM/OLE2, Agilent's net48 glue host, Waters
  # MassLynx, SciEX Clearcore2) can run concurrently, a reentrancy problem would show up EXACTLY as a
  # silent slide into the mzML lane -- which the box itself measures as materially worse output
  # (Waters 946->163 MB, SciEX SWATH 2891->2040 MB). Say so loudly so it cannot pass as a clean run.
  case "$R_NOTE" in
    *native-fail-\>msconvert*)
      echo "[$tag] WARNING: native reader FAILED, output came from the msconvert fallback (larger/slower archive). If this unit converted natively on a serial run, suspect vendor-SDK contention: re-run it with --jobs 1 before trusting the archive." >&2 ;;
  esac
  if [ "$R_UP" = "1" ] && [ "$R_EXIT" = "0" ]; then
    # S3-FIRST: a durable s3:// target is ALREADY the deliverable -- the box PUT it to its final
    # corpus key. Mirror it down for local reference (the corpus keeps a local copy so the
    # descriptor-driven harness can stamp and re-verify it), but never treat the local file as the
    # source of truth. --no-fetch skips the mirror entirely.
    local dst="$out" defer=0
    case "$out" in
      s3://*)
        # dst MUST derive from the DURABLE key. $key is the STAGING key (box-convert/<uuid>.mzpeak),
        # so the old expression mirrored every archive to $CORPUS_ROOT/box-convert/<uuid>.mzpeak: a
        # uuid-named file the corpus never reads, leaving the real path stale. LOCAL_COPY is a single
        # global and cannot be correct for more than one job, so a manifest derives each dst from its
        # own target and LOCAL_COPY stays an override for single-job invocations.
        local dkey="${out#s3://}"; dkey="${dkey#*/}"
        dst="${LOCAL_COPY:-$CORPUS_ROOT/$dkey}"
        if [ "${NO_FETCH:-0}" = "1" ]; then defer=2
        elif [ "${DEFER_FETCH:-0}" = "1" ]; then defer=1
        else echo "[$tag] durable target $out -> mirroring to $dst" >&2; fi ;;
    esac

    if [ "$defer" = "1" ]; then
      # Verify WITHOUT moving bytes: a single-part PUT's ETag IS the body md5, and the box uploads
      # with one presigned PUT (the 5 GB ceiling enforces single-part), so a HEAD proves the object
      # intact. This KEEPS the pre-publish integrity gate that --no-fetch drops, while the slow local
      # pull is batched to the end of the run: the host uplink is the bottleneck and must not sit in
      # the conversion path.
      local hsize hetag
      hsize="$("${RELAY[@]}" head "$key" 2>/dev/null)"
      hetag="$("${RELAY[@]}" head "$key" --etag 2>/dev/null)"
      if [ "$hsize" != "$R_SIZE" ]; then
        echo "[$tag] FAIL: staged size mismatch (box=$R_SIZE s3=${hsize:-<none>})" >&2; return 1; fi
      if [ -n "$R_MD5" ] && [ -n "$hetag" ] && [ "$hetag" != "$R_MD5" ]; then
        echo "[$tag] FAIL: staged md5 mismatch (box=$R_MD5 s3=$hetag)" >&2; return 1; fi
      # Staged, NOT yet published: hold the record per-job; one_job promotes it to $FETCH_LIST only
      # once the server-side publish is confirmed. Appending straight to the shared list made the
      # drain pull whatever happened to sit at the durable key when the publish was REFUSED -- 19
      # multi-GB downloads over a 0.5 MB/s uplink, every one discarded at the md5 check.
      printf '%s\t%s\t%s\t%s\n' "$out" "$dst" "$R_SIZE" "$R_MD5" > "$PENDING_DIR/fetch-$uid"
      echo "[$tag] OK exit=0 size=$R_SIZE -> $out (verified server-side; local pull deferred)"
    elif [ "$defer" = "2" ]; then
      echo "[$tag] OK exit=0 size=$R_SIZE -> $out (no local mirror)"
    else
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
    fi
    if [ -n "${BENCH_TSV:-}" ]; then   # per-stage box timings for benchmark collection
      # `[ -f ] || > file` was a test-then-truncate race: two concurrent jobs both find the file
      # missing and the second `>` truncates rows the first already appended. O_EXCL alone (`set -C`)
      # is not enough either -- it makes the CREATE exclusive but leaves the fd at offset 0, so a row
      # appended by another job between the create and the header write is overwritten at offset 0
      # (reproduced: a 12-byte row came back with its first 7 bytes replaced by "HEADER\n").
      # So: write the header into a temp file and `ln` it into place. link(2) is atomic and fails if
      # the target exists, the file is never visible without its header, and no fd ever sits at
      # offset 0 on the shared path. The data row below is a single short write to an O_APPEND fd.
      if [ ! -s "$BENCH_TSV" ]; then
        # $uid, not $$: bash keeps $$ as the ORIGINAL shell's pid inside a background subshell, so
        # concurrent jobs of one run would share this temp name. $uid is this job's own uuid.
        local _bh="$BENCH_TSV.hdr.$uid"
        printf 'iso_time\tunit\tconverter\topts\traw_bytes\tmzpeak_bytes\tconv_s\tmsconv_s\tdl_s\tup_s\thost\n' > "$_bh" 2>/dev/null \
          && ln "$_bh" "$BENCH_TSV" 2>/dev/null   # loser: target exists, header already written
        rm -f "$_bh"
      fi
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
      cat "$PENDING_DIR/fetch-$uid" >> "$FETCH_LIST" 2>/dev/null   # now safe to mirror down
    else
      echo "[$(basename "$2")] FAIL: could not publish staging key to $2" >&2
      rc=1
    fi
  fi
  rm -f "$PENDING_DIR/fetch-$uid"                    # queued above only if the publish succeeded
  if "${RELAY[@]}" delete "$key" >/dev/null 2>&1; then
    rm -f "$PENDING_DIR/$uid"                        # unregister only on confirmed delete
  else
    echo "[$(basename "$2")] WARN: S3 delete failed; trap will retry $key at exit" >&2
  fi
  return $rc
}

_unit_members(){  # unit path -> the names (relative to its PARENT) that together FORM the unit
  # This was CALLED by local_job and never defined anywhere: the call failed, members[] came back
  # empty, and the caller's fallback shipped just the basename. For a directory unit that is right
  # by accident (tar recurses), but a SCIEX .wiff is a 13 MB stub whose payload lives in a sibling
  # .wiff.scan (1.73 GB for VD_170826) -- so the box was handed a unit with no data in it.
  local p="$1" d b sc
  d="$(dirname "$p")"; b="$(basename "$p")"
  printf '%s\n' "$b"
  [ -d "$p" ] && return 0                      # .d / Waters .raw: one name, tar recurses into it
  case "$b" in
    *.wiff)          for sc in "$b.scan" "${b%.wiff}.wiff2"; do [ -e "$d/$sc" ] && printf '%s\n' "$sc"; done ;;
    # break after the first hit: only ONE .ibd exists, and a case-insensitive host filesystem
    # matches both spellings, which would put the same file in the tar twice.
    *.imzML|*.imzml) for sc in "${b%.*}.ibd" "${b%.*}.IBD";  do [ -e "$d/$sc" ] && { printf '%s\n' "$sc"; break; }; done ;;
  esac
  return 0
}

unit_presign(){  # local corpus path -> unit descriptor JSON on stdout; non-zero => not in the bucket
  local abs key
  abs="$(cd "$(dirname "$1")" 2>/dev/null && pwd)/$(basename "$1")" || return 2
  case "$abs" in "$CORPUS_ROOT"/*) key="${abs#"$CORPUS_ROOT"/}" ;; *) return 2 ;; esac
  "${RELAY[@]}" presign-unit "$key" --expires "$PUT_EXPIRES" 2>/dev/null
}

fetch_one(){  # out dst size md5 slot — one deferred mirror; runs in its own background subshell
  local out="$1" dst="$2" size="$3" md5="$4" slot="$5" bkt dkey part got gotmd5
  bkt="${out#s3://}"; dkey="${bkt#*/}"; bkt="${bkt%%/*}"
  mkdir -p "$(dirname "$dst")" 2>/dev/null || true
  # BOTH halves are needed. $$ alone is wrong: bash keeps it as the ORIGINAL shell's pid inside a
  # background subshell, so every concurrent pull of ONE run would pick the same name. $slot alone is
  # wrong too: slots restart at 1 in every process, so two overlapping box_convert.sh runs on this
  # host (a corpus rebuild plus a manual invocation) would collide on `<dst>.1.part`.
  part="$dst.$$.$slot.part"
  if ! S3_BUCKET="$bkt" "${RELAY[@]}" get "$dkey" "$part"; then
    echo "  FAIL get $out" >&2; rm -f "$part"; return 1; fi
  got="$(wc -c < "$part" | tr -d ' ')"
  if [ "$got" != "$size" ]; then
    echo "  FAIL size $out (want=$size got=$got)" >&2; rm -f "$part"; return 1; fi
  gotmd5="$("${RELAY[@]}" md5 "$part")"
  if [ -n "$md5" ] && [ "$gotmd5" != "$md5" ]; then
    echo "  FAIL md5 $out" >&2; rm -f "$part"; return 1; fi
  mv -f "$part" "$dst" && echo "  pulled $dst" >&2
}

drain_fetch_list(){  # pull every deferred durable target down to its local reference copy
  # Runs ONCE, after the whole manifest. Conversion never waits on the host's ~0.5 MB/s uplink.
  #
  # This loop was the last strictly-serial leg of a rebuild: N multi-GB GETs one after another, and
  # between them a full md5 of the file just written (~20-30 s of pure CPU on a 10 GB archive) with
  # the link completely idle. boto3's download_file is already internally multi-threaded, so the win
  # here is not more sockets — it is overlapping one archive's verify with the next one's transfer.
  # Bounded because each pull holds a full .part copy of its archive on the host disk.
  [ -s "$FETCH_LIST" ] || return 0
  local n; n="$(wc -l < "$FETCH_LIST" | tr -d ' ')"
  local par="${MZPC_FETCH_JOBS:-3}"
  case "$par" in ''|*[!0-9]*) par=3 ;; esac
  [ "$par" -lt 1 ] && par=1
  echo "post-run: pulling $n published archive(s) from S3 for local reference ($par at a time)" >&2
  local fails=0 out dst size md5 slot=0 pids=() p
  while IFS="$(printf '\t')" read -r out dst size md5; do
    [ -z "${out:-}" ] && continue
    slot=$((slot+1))
    fetch_one "$out" "$dst" "$size" "$md5" "$slot" & pids+=("$!")
    if [ "${#pids[@]}" -ge "$par" ]; then wait "${pids[0]}" || fails=$((fails+1)); pids=("${pids[@]:1}"); fi
  done < "$FETCH_LIST"
  for p in ${pids[@]+"${pids[@]}"}; do wait "$p" || fails=$((fails+1)); done
  : > "$FETCH_LIST"
  [ "$fails" -eq 0 ] || { echo "post-run: $fails local pull(s) FAILED (published objects are intact)" >&2; return 1; }
}

local_job(){  # local-path out opts — stage a LOCAL unit to S3, convert via its presigned-GET url
  local path="$1" out="$2" opts="${3:-}" tag; tag="$(basename "$out")"
  [ -e "$path" ] || { echo "[$tag] FAIL: no such path: $path" >&2; return 1; }

  # S3-FIRST (no host bytes): if this unit is ALREADY in the corpus bucket, hand the box presigned
  # GETs for its members instead of tarring the local copy and pushing it back up the host's
  # ~0.5 MB/s uplink. Measured on the v0.9.0 corpus run: that upload was 86% of box-job wall clock
  # (conversion was 7.4%), and 20 of 21 units were already in the bucket. MZPC_NO_S3_SOURCE=1
  # forces the original host-staging path.
  local desc nmem
  if [ "${MZPC_NO_S3_SOURCE:-0}" != "1" ] && desc="$(unit_presign "$path")" && [ -n "$desc" ]; then
    nmem="$(printf '%s' "$desc" | python3 -c 'import sys,json;print(len(json.load(sys.stdin)["members"]))' 2>/dev/null)"
    if [ "${nmem:-0}" -gt 0 ] 2>/dev/null; then
      echo "[$tag] S3-first: $nmem member(s) already in bucket - box pulls directly (no host upload)" >&2
      UNIT_DESC="$desc" one_job "" "$out" "$opts"
      return $?
    fi
  fi

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
    # A non-numeric --jobs used to sail through into run_pool, where `[ ${#pids[@]} -ge abc ]` errors
    # on EVERY iteration and the window never closes: the whole manifest is launched at once. Harmless
    # while the clamp forced 1; not harmless now that parallel is the default.
    case "$jobs" in ''|*[!0-9]*) jobs=1 ;; esac
    [ "$jobs" -lt 1 ] && jobs=1
    # DISK SAFETY — bounded, not serial. The old clamp to 1 predates the box raw cache: every job
    # also DOWNLOADED its own raw, so N jobs meant N concurrent multi-GB writes to the box temp disk
    # on top of N conversions. With the cache that leg is a local copy, and the measured box phase is
    # ~98 % transfer / ~2 % conversion — serialising is almost pure waste.
    # Cap 4, justified: the worst case is the --via-msconvert FALLBACK, which writes a full mzML
    # intermediate (tens of GB for SWATH/DIA) plus the job's own copy of the raw and the output
    # archive, i.e. ~4 x (30 + 3 + 5) GB ~= 150 GB against 2,944 GB free. 24 cores absorb four
    # converters comfortably, and it keeps concurrent ssh sessions through the jumphost modest.
    # CAVEAT — disk is not the binding constraint on every box configuration. If MZPC_MZML_TMPDIR is
    # set on the box (box_convert_remote.ps1 documents pointing it at a RAMDISK), the mzML lands in
    # the box's 63 GB of RAM, not on C:, and four concurrent fallbacks do not fit. Size the cap to
    # that volume there -- MZPC_BOX_JOBS_CAP=1 is the safe setting; the box also refuses a fallback
    # that would not fit and says so, rather than filling the ramdisk. Untested at the time of
    # writing: whether the vendor SDKs (Shimadzu COM, Agilent net48 glue, Waters, SciEX) are safe to
    # run concurrently at all -- a native failure now prints a WARNING per job so a regression to the
    # msconvert lane is visible instead of silent.
    # MZPC_BOX_JOBS_CAP moves the cap; MZPC_ALLOW_PARALLEL=1 removes it (deliberate experiments).
    cap="${MZPC_BOX_JOBS_CAP:-4}"
    case "$cap" in ''|*[!0-9]*) cap=4 ;; esac
    [ "$cap" -lt 1 ] && cap=1
    if [ "$jobs" -gt "$cap" ] && [ "${MZPC_ALLOW_PARALLEL:-}" != "1" ]; then
      echo "box_convert: --jobs $jobs clamped to $cap (box disk safety; MZPC_BOX_JOBS_CAP=N to move the cap, MZPC_ALLOW_PARALLEL=1 to lift it)" >&2
      jobs="$cap"
    fi
    [ -f "$mf" ] || { echo "no such manifest: $mf" >&2; exit 2; }
    # The local mirror of each durable target is DEFERRED to a single pass after the manifest, so
    # the host's slow uplink never sits between two conversions. Integrity is not deferred: each
    # object is verified server-side (size + ETag/md5) before it is published.
    [ "${NO_FETCH:-0}" = "1" ] || DEFER_FETCH="${DEFER_FETCH:-1}"
    mrc=0
    if [ "$mode" = "--local-manifest" ]; then run_pool "$mf" local_job "$jobs" || mrc=$?
    else run_pool "$mf" one_job "$jobs" || mrc=$?; fi
    drain_fetch_list || mrc=1
    exit "$mrc"
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
