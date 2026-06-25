#!/usr/bin/env bash
# rerun-tests — one command to (re)run the entire mzPeak corpus harness and report.
#
# 1. builds the converter (release)
# 2. runs tools/corpus_bench.sh over the whole corpus (add --box to include SCIEX/Waters on
#    flash-workstation)
# 3. prints the results table, then COLLATES every deviation / error / warning across all
#    conversion paths and the validator, and summarizes them for the user.
#
# Usage:  tools/rerun-tests.sh [--box] [--out DIR] [--only ID[,ID...]]
set -uo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
out="${TMPDIR:-/tmp}/rerun_tests"
passthru=()
while [ $# -gt 0 ]; do case "$1" in
  --out) out="$2"; shift 2;;
  --box) passthru+=("$1"); shift;;
  --only) passthru+=("$1" "$2"); shift 2;;
  *) passthru+=("$1"); shift;;
esac; done

echo "================ rerun-tests ================"
echo "[1/3] building converter (release) ..."
( cd "$root" && cargo build --release 2>&1 | grep -iE 'error|warning: unused|Finished' | tail -3 )
[ -x "$root/target/release/mzpeak-convert" ] || { echo "BUILD FAILED — aborting"; exit 1; }

echo "[2/3] running corpus harness ..."
rm -rf "$out"; mkdir -p "$out"
"$root/tools/corpus_bench.sh" --keep --out "$out" "${passthru[@]}"

tsv="$out/results.tsv"
echo
echo "[3/3] collating deviations / errors / warnings ..."
echo

# ---------- SUMMARY ----------
echo "================ SUMMARY ================"
# counts by status
awk -F'\t' 'NR>1{
  tot++; st[$6]++; if($7=="PASS")vp++; else if($7=="FAIL")vf++;
  if($6=="OK" && $8+0>=20000000){rs+=$10; rn++; if($10+0>1){xpd[$1]=$10; nx++}}
}
END{
  printf "Datasets: %d total\n", tot
  printf "  Convert:  OK=%d  CONV-ERR=%d  MISSING=%d  SKIP-box=%d\n", st["OK"], st["CONV-ERR"], st["MISSING"], st["SKIP-box"]
  printf "  Validate: PASS=%d  FAIL=%d\n", vp, vf
  if(rn) printf "  Mean ratio (included, >=20MB): %.3f over %d datasets\n", rs/rn, rn
  if(nx>0){ printf "  Expanders (ratio > 1):"; for(k in xpd) printf " %s=%s", k, xpd[k]; printf "\n" }
}' "$tsv"

# round-trip lossiness deviations (grid paths log "max round-trip X ppm")
echo
echo "-- grid round-trip deviations (max ppm vs vendor m/z) --"
found=0
for log in "$out"/*.log; do
  [ -e "$log" ] || continue
  id="$(basename "$log" .log)"
  ppm="$(grep -oE 'max round-trip [0-9.]+ ppm|max round.trip m/z error [0-9.e-]+ ppm' "$log" | grep -oE '[0-9.e-]+ ppm' | tail -1)"
  [ -n "$ppm" ] && { printf "  %-22s %s\n" "$id" "$ppm"; found=1; }
done
[ "$found" = 0 ] && echo "  (none reported — non-grid paths are bit-exact f64)"

# errors: failed conversions + panics/errors in convert logs
echo
echo "-- errors --"
awk -F'\t' 'NR>1 && $6!="OK" && $6!="SKIP-box"{printf "  %s: %s\n",$1,$6}' "$tsv"
for log in "$out"/*.log; do
  [ -e "$log" ] || continue
  e="$(grep -iE 'panicked at|error: converting|thread .main. panicked|fatal' "$log" | head -1)"
  [ -n "$e" ] && printf "  %s: %s\n" "$(basename "$log" .log)" "$(echo "$e" | cut -c1-100)"
done
echo "  (no further errors)"

# validator warnings: collate distinct warnings across all .val files
echo
echo "-- validator warnings (collated, by frequency) --"
if ls "$out"/*.val >/dev/null 2>&1; then
  # the validator prints per-warning lines beginning "WARNING <code>"; the summary line
  # "(0 errors, N warnings)" is lowercase and excluded by the case-sensitive match.
  n="$(grep -hE 'WARNING' "$out"/*.val 2>/dev/null \
        | sed -E 's/^.*WARNING[: ]*//; s/[0-9]+/N/g; s/  +/ /g' \
        | sort | uniq -c | sort -rn)"
  if [ -n "$n" ]; then echo "$n" | head -20 | sed 's/^/  /'; else echo "  (none)"; fi
else
  echo "  (no validator output — install mzpeak-validate)"
fi

# validator errors
echo
echo "-- validator errors --"
ve=0
for v in "$out"/*.val; do
  [ -e "$v" ] || continue
  if grep -qiE 'FAIL|[1-9][0-9]* error' "$v"; then
    printf "  %s: %s\n" "$(basename "$v" .val)" "$(grep -iE 'error' "$v" | head -1 | cut -c1-90)"; ve=1
  fi
done
[ "$ve" = 0 ] && echo "  (none — all validated archives are spec-clean)"

echo
echo "Full table: $out/REPORT.md   Raw rows: $tsv   Per-dataset logs: $out/<id>.{log,val}"
echo "========================================"
