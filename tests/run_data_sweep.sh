#!/usr/bin/env bash
# Full-corpus sweep: discover EVERY convertible input under a data root, convert each to mzPeak
# (with --verify), validate the output with mzpeak-validate, and tabulate. Runs conversions in
# parallel. Archives are deleted right after a passing validation to bound disk; logs are kept only
# for files that fail (convert or validate). Override with KEEP_OUTPUTS=1.
#
# Usage: tests/run_data_sweep.sh [DATA_ROOT]   (default ~/Claude/mzML2mzPeak/data)
# Env:   JOBS=N (parallel, default 6), KEEP_OUTPUTS=1, MZPEAK_VALIDATE=/path, NO_VERIFY=1
set -uo pipefail

# ---- worker mode: one input record "<fmt>\x1f<path>" --------------------------------------------
if [ "${1:-}" = "--worker" ]; then
  rec="$2"; us=$'\x1f'
  fmt="${rec%%${us}*}"; path="${rec#*${us}}"
  rel="${path#"$DATA"/}"
  id="$(printf '%s' "$rel" | tr '/ ,()' '_____')"
  archive="$OUT/archives/$id.mzpeak"
  clog="$OUT/logs/$id.convert.log"; vlog="$OUT/logs/$id.val.log"
  verify=(--verify); [ -n "${NO_VERIFY:-}" ] && verify=()

  "$BIN" convert "$path" -o "$archive" --force "${verify[@]}" >"$clog" 2>&1
  rc=$?
  if [ "$rc" -eq 3 ]; then
    printf '%s\t%s\tSKIP\tunsupported (vendor feature off)\t\n' "$fmt" "$rel" >>"$OUT/results.tsv"
    rm -f "$clog" "$archive"; exit 0
  fi
  if [ "$rc" -ne 0 ]; then
    printf '%s\t%s\tCONV-ERR\trc=%s; see logs/%s.convert.log\t\n' "$fmt" "$rel" "$rc" "$id" >>"$OUT/results.tsv"
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
JOBS="${JOBS:-6}"
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
export DATA OUT BIN VALIDATE KEEP_OUTPUTS NO_VERIFY

# Build NUL-delimited "<fmt>\x1f<path>" records.
recs="$OUT/records"; : >"$recs"
emit() { printf '%s\x1f%s\0' "$1" "$2" >>"$recs"; }
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

total="$(tr -cd '\0' <"$recs" | wc -c | tr -d ' ')"
echo "sweep: $total inputs from $DATA  (jobs=$JOBS, verify=$([ -n "${NO_VERIFY:-}" ] && echo off || echo on))"
echo "output/logs: $OUT"
start=$(date +%s)
xargs -0 -P "$JOBS" -n1 bash "$0" --worker <"$recs"
end=$(date +%s)

echo; echo "==== SWEEP SUMMARY ($((end-start))s) ===="
awk -F'\t' '
  { st[$3]++; total++; by[$1"\t"$3]++ }
  END {
    print "by format:";
    for (k in by) print "  "k": "by[k] | "sort";
    close("sort");
    print "----";
    printf "TOTAL=%d  PASS=%d  CONV-ERR=%d  VAL-FAIL=%d  SKIP=%d\n",
           total, st["PASS"], st["CONV-ERR"], st["VAL-FAIL"], st["SKIP"];
  }' "$OUT/results.tsv"

echo; echo "failures (CONV-ERR / VAL-FAIL):"
awk -F'\t' '$3=="CONV-ERR"||$3=="VAL-FAIL"{printf "  [%s] %s — %s\n",$3,$2,$4}' "$OUT/results.tsv" | sort | head -100
fails="$(awk -F'\t' '$3=="CONV-ERR"||$3=="VAL-FAIL"' "$OUT/results.tsv" | wc -l | tr -d ' ')"
echo "(full results: $OUT/results.tsv)"
[ "$fails" -eq 0 ]
