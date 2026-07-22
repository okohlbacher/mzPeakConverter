#!/usr/bin/env bash
# End-to-end corpus runner: convert each `e2e` entry in corpus.tsv to mzPeak and validate it
# with mzpeak-validate. Prints a per-file table and a summary; exits non-zero if any conversion
# or validation fails. `skip` entries are listed with their reason but not run.
#
# Usage: tests/run_corpus_e2e.sh [--release] [--out DIR]
set -uo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(dirname "$here")"
manifest="$here/corpus.tsv"
out="${TMPDIR:-/tmp}/mzpc-e2e"
profile="debug"

while [ $# -gt 0 ]; do
  case "$1" in
    --release) profile="release"; shift ;;
    --out) out="$2"; shift 2 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

bin="$root/target/$profile/mzpeak-convert"
[ -x "$bin" ] || { echo "build first: cargo build $([ "$profile" = release ] && echo --release)"; exit 2; }
# Prefer a pyarrow>=14 validator env (BSS-INT32 capable) over the on-PATH script, whose shebang is
# pinned to anaconda *base* python (3.7.4 / pyarrow 12). $MZPEAK_VALIDATE overrides everything.
validate=""
for cand in "${MZPEAK_VALIDATE:-}" \
            "$HOME/anaconda3/envs/mzpeak314/bin/mzpeak-validate" \
            "$HOME/anaconda3/envs/mzpeak/bin/mzpeak-validate"; do
  if [ -n "$cand" ] && [ -x "$cand" ]; then validate="$cand"; break; fi
done
[ -z "$validate" ] && command -v mzpeak-validate >/dev/null && validate="mzpeak-validate"
[ -z "$validate" ] && { echo "mzpeak-validate not found"; exit 2; }
mkdir -p "$out"

pass=0; fail=0; skipped=0; missing=0
printf "%-16s %-6s %-8s %s\n" "ID" "FORMAT" "RESULT" "DETAIL"
printf -- "------------------------------------------------------------------\n"

while IFS=$'\t' read -r id format status path note; do
  # Manifest paths are $HOME/$MZPEAK_CORPUS-relative so the repo carries no operator path.
  # Plain substitution, never eval: the manifest is data, not script.
  path="${path//\$MZPEAK_CORPUS/${MZPEAK_CORPUS:-$HOME/Claude/mzpeak-example-data/data}}"
  path="${path//\$HOME/$HOME}"
  case "$id" in ''|\#*) continue ;; esac          # skip blanks/comments
  if [ "$status" = "skip" ]; then
    printf "%-16s %-6s %-8s %s\n" "$id" "$format" "SKIP" "$note"
    skipped=$((skipped+1)); continue
  fi
  if [ ! -e "$path" ]; then
    printf "%-16s %-6s %-8s %s\n" "$id" "$format" "MISSING" "$path"
    missing=$((missing+1)); continue
  fi
  archive="$out/$id.mzpeak"
  if ! "$bin" "$path" -o "$archive" --force >"$out/$id.convert.log" 2>&1; then
    printf "%-16s %-6s %-8s %s\n" "$id" "$format" "CONV-ERR" "see $out/$id.convert.log"
    fail=$((fail+1)); continue
  fi
  if "$validate" "$archive" >"$out/$id.val.log" 2>&1; then
    detail="$(grep -m1 'mzPeak validation' "$out/$id.val.log" | sed 's/^[[:space:]]*//')"
    printf "%-16s %-6s %-8s %s\n" "$id" "$format" "PASS" "$detail"
    pass=$((pass+1))
  else
    detail="$(grep -m1 'mzPeak validation' "$out/$id.val.log" | sed 's/^[[:space:]]*//')"
    printf "%-16s %-6s %-8s %s\n" "$id" "$format" "VAL-FAIL" "$detail (see $out/$id.val.log)"
    fail=$((fail+1))
  fi
done < "$manifest"

printf -- "------------------------------------------------------------------\n"
printf "pass=%d fail=%d skip=%d missing=%d   outputs in %s\n" "$pass" "$fail" "$skipped" "$missing" "$out"
[ "$fail" -eq 0 ]
