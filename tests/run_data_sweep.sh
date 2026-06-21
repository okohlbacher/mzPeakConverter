#!/usr/bin/env bash
# Full-corpus sweep: discover EVERY convertible input under a data root, convert each to mzPeak
# validate the output with mzpeak-validate, and tabulate. Runs conversions in
# parallel. Archives are deleted right after a passing validation to bound disk; logs are kept only
# for files that fail (convert or validate). Override with KEEP_OUTPUTS=1.
#
# Usage: tests/run_data_sweep.sh [DATA_ROOT]   (default ~/Claude/mzML2mzPeak/data)
# Env:   JOBS=N (small-file parallelism, default 4), MEM_CAP_GB=N (hard RAM ceiling, default 30),
#        SIZE_BIG_MB=N (>=this runs serially, one at a time; default 200),
#        KEEP_OUTPUTS=1, MZPEAK_VALIDATE=/path
set -uo pipefail

# ---- worker mode: one input record "<fmt>\x1f<path>" --------------------------------------------
if [ "${1:-}" = "--worker" ]; then
  rec="$2"; us=$'\x1f'
  fmt="${rec%%${us}*}"; path="${rec#*${us}}"
  rel="${path#"$DATA"/}"
  # Sanitize ALL non-portable chars (incl. newlines/tabs/commas/spaces) so the id is a safe single
  # filename AND a tab/newline can never sneak into a result-TSV row.
  id="$(printf '%s' "$rel" | tr -c 'A-Za-z0-9._-' '_')"
  archive="$OUT/archives/$id.mzpeak"
  clog="$OUT/logs/$id.convert.log"; vlog="$OUT/logs/$id.val.log"

  "$BIN" "$path" -o "$archive" --force >"$clog" 2>&1
  rc=$?
  if [ "$rc" -eq 3 ]; then
    printf '%s\t%s\tSKIP\tunsupported (vendor feature off)\t\n' "$fmt" "$rel" >>"$OUT/results.tsv"
    rm -f "$clog" "$archive"; exit 0
  fi
  if [ "$rc" -ne 0 ]; then
    # rc 137 = SIGKILL (128+9): the memory watchdog reaped this conversion to stay under the RAM cap.
    if [ "$rc" -eq 137 ]; then
      printf '%s\t%s\tOOM-KILL\tkilled by RAM-cap watchdog (rerun this file alone or raise MEM_CAP_GB)\t\n' "$fmt" "$rel" >>"$OUT/results.tsv"
    else
      printf '%s\t%s\tCONV-ERR\trc=%s; see logs/%s.convert.log\t\n' "$fmt" "$rel" "$rc" "$id" >>"$OUT/results.tsv"
    fi
    rm -f "$archive"; exit 0
  fi
  size="$(stat -f%z "$archive" 2>/dev/null || echo 0)"
  if "$VALIDATE" "$archive" >"$vlog" 2>&1; then
    detail="$(grep -m1 'mzPeak validation' "$vlog" | sed 's/^[[:space:]]*//')"
    printf '%s\t%s\tPASS\t%s\t%s\n' "$fmt" "$rel" "${detail:-ok}" "$size" >>"$OUT/results.tsv"
    rm -f "$vlog"
    [ -n "${KEEP_OUTPUTS:-}" ] || rm -f "$archive"
  else
    detail="$(grep -m1 'mzPeak validation' "$vlog" | sed 's/^[[:space:]]*//')"
    printf '%s\t%s\tVAL-FAIL\t%s; see logs/%s.val.log\t%s\n' "$fmt" "$rel" "${detail:-error}" "$id" "$size" >>"$OUT/results.tsv"
    [ -n "${KEEP_OUTPUTS:-}" ] || rm -f "$archive"
  fi
  exit 0
fi

# ---- driver -------------------------------------------------------------------------------------
here="$(cd "$(dirname "$0")" && pwd)"; root="$(dirname "$here")"
DATA="${1:-$HOME/Claude/mzML2mzPeak/data}"
DATA="$(cd "$DATA" && pwd)"
OUT="${TMPDIR:-/tmp}/mzpc-sweep"
JOBS="${JOBS:-4}"
# Hard RAM ceiling for the whole sweep (converter + validator processes). macOS has no cgroups, so a
# background watchdog SIGKILLs the largest conversion whenever the aggregate RSS exceeds the cap —
# freeing its memory immediately (SIGSTOP would not). The reaped file is recorded as OOM-KILL. Keep
# JOBS modest so this rarely triggers; the watchdog is the backstop that stops the machine crashing.
MEM_CAP_GB="${MEM_CAP_GB:-30}"
# Inputs at/above this size are converted one at a time (serially); smaller ones run JOBS-parallel.
SIZE_BIG_MB="${SIZE_BIG_MB:-200}"
BIN="$root/target/release/mzpeak-convert"
[ -x "$BIN" ] || { echo "build first: cargo build --release"; exit 2; }

VALIDATE=""
for cand in "${MZPEAK_VALIDATE:-}" \
            "$HOME/anaconda3/envs/mzpeak314/bin/mzpeak-validate" \
            "$HOME/anaconda3/envs/mzpeak/bin/mzpeak-validate"; do
  [ -n "$cand" ] && [ -x "$cand" ] && { VALIDATE="$cand"; break; }
done
[ -z "$VALIDATE" ] && command -v mzpeak-validate >/dev/null && VALIDATE="mzpeak-validate"
[ -z "$VALIDATE" ] && { echo "mzpeak-validate not found"; exit 2; }

rm -rf "$OUT"; mkdir -p "$OUT/archives" "$OUT/logs"
: >"$OUT/results.tsv"
export DATA OUT BIN VALIDATE KEEP_OUTPUTS

# Build NUL-delimited "<fmt>\x1f<path>" records, ROUTED BY SIZE: inputs at/above SIZE_BIG_MB go to a
# serial queue (one big file at a time — the memory-heavy ones), everything else to a parallel queue.
recs_small="$OUT/records.small"; recs_big="$OUT/records.big"; : >"$recs_small"; : >"$recs_big"
big_bytes=$(( SIZE_BIG_MB * 1024 * 1024 ))
emit() {
  local fmt="$1" path="$2" sz
  if [ -d "$path" ]; then
    sz=$(du -sk "$path" 2>/dev/null | awk '{print $1*1024}')   # .d directory total
  else
    sz=$(stat -f%z "$path" 2>/dev/null || echo 0)
  fi
  if [ "${sz:-0}" -ge "$big_bytes" ]; then
    printf '%s\x1f%s\0' "$fmt" "$path" >>"$recs_big"
  else
    printf '%s\x1f%s\0' "$fmt" "$path" >>"$recs_small"
  fi
}
while IFS= read -r -d '' f; do emit mzML   "$f"; done < <(find "$DATA" -type f -iname '*.mzML'  -print0)
while IFS= read -r -d '' f; do emit imzML  "$f"; done < <(find "$DATA" -type f -iname '*.imzML' -print0)
while IFS= read -r -d '' f; do emit raw    "$f"; done < <(find "$DATA" -type f -iname '*.raw'   -print0)
while IFS= read -r -d '' f; do emit wiff   "$f"; done < <(find "$DATA" -type f -iname '*.wiff'  -print0)
# .d directories, classified by the marker file they contain (skips wrapper dirs with no marker).
while IFS= read -r -d '' d; do
  if   [ -e "$d/analysis.tdf" ]; then emit TDF "$d"
  elif [ -e "$d/analysis.tsf" ]; then emit TSF "$d"
  elif [ -e "$d/analysis.baf" ]; then emit BAF "$d"
  elif [ -e "$d/AcqData" ];      then emit Agilent "$d"
  fi
done < <(find "$DATA" -type d -iname '*.d' -print0)

n_small="$(tr -cd '\0' <"$recs_small" | wc -c | tr -d ' ')"
n_big="$(tr -cd '\0' <"$recs_big" | wc -c | tr -d ' ')"
echo "sweep: $((n_small + n_big)) inputs from $DATA"
echo "  small (<${SIZE_BIG_MB}MB): $n_small parallel x$JOBS    big (>=${SIZE_BIG_MB}MB): $n_big serial    RAM cap ${MEM_CAP_GB}GB"
echo "output/logs: $OUT"

# RAM-cap watchdog: every 2s, sum the RSS of the sweep's heavy processes (converter + validator);
# while it exceeds the cap, SIGKILL the single largest one (freeing its RAM) until back under. Runs
# in the background; trapped so it dies with the driver.
mem_watchdog() {
  local cap_kb=$(( MEM_CAP_GB * 1024 * 1024 ))
  while :; do
    sleep 2
    local pids
    pids="$({ pgrep -f "$BIN"; pgrep -f mzpeak-validate; } 2>/dev/null | sort -u | tr '\n' ' ')"
    [ -z "${pids// /}" ] && continue
    local total
    total="$(ps -o rss= -p "${pids// /,}" 2>/dev/null | awk '{s+=$1} END{print s+0}')"
    while [ "${total:-0}" -gt "$cap_kb" ]; do
      local victim
      victim="$(ps -o pid=,rss= -p "${pids// /,}" 2>/dev/null | sort -k2 -rn | awk 'NR==1{print $1}')"
      [ -z "$victim" ] && break
      kill -9 "$victim" 2>/dev/null
      echo "[mem-watchdog] RSS $((total/1024/1024))GB > ${MEM_CAP_GB}GB cap — killed pid $victim" >&2
      sleep 1
      pids="$({ pgrep -f "$BIN"; pgrep -f mzpeak-validate; } 2>/dev/null | sort -u | tr '\n' ' ')"
      [ -z "${pids// /}" ] && break
      total="$(ps -o rss= -p "${pids// /,}" 2>/dev/null | awk '{s+=$1} END{print s+0}')"
    done
  done
}
mem_watchdog & watchdog_pid=$!
disown "$watchdog_pid" 2>/dev/null || true  # keep job-control quiet when we kill it at the end
trap 'kill "$watchdog_pid" 2>/dev/null' EXIT

start=$(date +%s)
# Small files: parallel. Big files: strictly one at a time (so a single huge conversion never shares
# RAM with another). The watchdog still guards both passes against any single runaway.
xargs -0 -P "$JOBS" -n1 bash "$0" --worker <"$recs_small"
xargs -0 -P 1     -n1 bash "$0" --worker <"$recs_big"
end=$(date +%s)
kill "$watchdog_pid" 2>/dev/null; trap - EXIT

echo; echo "==== SWEEP SUMMARY ($((end-start))s) ===="
awk -F'\t' '
  { st[$3]++; total++; by[$1"\t"$3]++ }
  END {
    print "by format:";
    for (k in by) print "  "k": "by[k] | "sort";
    close("sort");
    print "----";
    printf "TOTAL=%d  PASS=%d  CONV-ERR=%d  VAL-FAIL=%d  OOM-KILL=%d  SKIP=%d\n",
           total, st["PASS"], st["CONV-ERR"], st["VAL-FAIL"], st["OOM-KILL"], st["SKIP"];
  }' "$OUT/results.tsv"

echo; echo "failures (CONV-ERR / VAL-FAIL / OOM-KILL):"
awk -F'\t' '$3=="CONV-ERR"||$3=="VAL-FAIL"||$3=="OOM-KILL"{printf "  [%s] %s — %s\n",$3,$2,$4}' "$OUT/results.tsv" | sort | head -100
fails="$(awk -F'\t' '$3=="CONV-ERR"||$3=="VAL-FAIL"||$3=="OOM-KILL"' "$OUT/results.tsv" | wc -l | tr -d ' ')"
echo "(full results: $OUT/results.tsv)"
[ "$fails" -eq 0 ]
