#!/usr/bin/env bash
# Full-corpus reconvert for ~/Claude/mzPeak/data (the canonical corpus).
# Walks every host-convertible unit, converts in-place to a sibling .mzpeak (deterministic name,
# overwrites same-named), records raw/mzpeak/ratio/status. Tiered MOST-RELEVANT-FIRST so grid/vendor
# signal lands before the bulk mzML. Non-destructive: never deletes existing archives (preserves
# path-compare/ pairs + box outputs). SCIEX .wiff / Waters .raw are recorded BOX-DEFERRED (need the
# Windows vendor reader). Validation is a separate step (validate_everything.py over the same root).
#
# Usage: tools/corpus_full.sh [DATA_ROOT] [--only TIER[,TIER...]]
set -uo pipefail
root="${1:-$HOME/Claude/mzPeak/data}"; shift || true
only=""; while [ $# -gt 0 ]; do case "$1" in --only) only=",$2,"; shift 2;; *) shift;; esac; done
here="$(cd "$(dirname "$0")" && pwd)"; bin="$(dirname "$here")/target/release/mzpeak-convert"
[ -x "$bin" ] || { echo "build first: cargo build --release" >&2; exit 2; }
out="$root/validator_logs/reconvert-$(date +%Y%m%d-%H%M%S)"; mkdir -p "$out/logs"
tsv="$out/results.tsv"; printf 'tier\tformat\tstatus\traw\tmzpeak\tratio\tunit\n' > "$tsv"
echo "reconvert -> $out"

sizeof(){ if [ -d "$1" ]; then du -sk "$1" | awk '{print $1*1024}'; else stat -f %z "$1" 2>/dev/null || echo 0; fi; }
slug(){ echo "${1#$root/}" | sed 's#[^A-Za-z0-9._-]#_#g; s#__*#_#g'; }
ratio(){ awk -v m="$1" -v w="$2" 'BEGIN{if(w>0) printf "%.4f", m/w; else print "NA"}'; }

conv(){ # tier format flags input outpath
  local tier="$1" fmt="$2" flags="$3" in="$4" o="$5"
  [ -n "$only" ] && case "$only" in *",$tier,"*) :;; *) return;; esac
  local id; id="$(slug "$in")"; local raw; raw="$(sizeof "$in")"
  echo ">> [$tier/$fmt] $id"
  if "$bin" "$in" $flags -o "$o" --force > "$out/logs/$id.log" 2>&1; then
    local mp; mp="$(sizeof "$o")"
    printf '%s\t%s\tOK\t%s\t%s\t%s\t%s\n' "$tier" "$fmt" "$raw" "$mp" "$(ratio "$mp" "$raw")" "${in#$root/}" >> "$tsv"
  else
    local rc=$?; local st="CONV-ERR"; [ "$rc" = 3 ] && st="UNSUPPORTED"
    printf '%s\t%s\t%s\t%s\t\t\tsee logs/%s.log\n' "$tier" "$fmt" "$st" "$raw" "$id" >> "$tsv"
  fi
}
defer(){ printf '%s\t%s\tBOX-DEFERRED\t%s\t\t\t%s\n' "$1" "$2" "$(sizeof "$3")" "${3#$root/}" >> "$tsv"; }

# in-place target: replace the unit's extension with .mzpeak (deterministic, 1:1 with the unit)
mz_of(){ local p="$1"; case "$p" in *.*) echo "${p%.*}.mzpeak";; *) echo "$p.mzpeak";; esac; }

walk_tier(){ # tier dir  — dispatch every unit in a tile by type
  local tier="$1" dir="$root/$2"; [ -d "$dir" ] || return
  # Agilent .d (no analysis.tdf/tsf) -> grid; Bruker .d (has tdf/tsf) -> native
  while IFS= read -r d; do
    if find "$d" -maxdepth 1 \( -name analysis.tdf -o -name analysis.tsf \) | grep -q .; then
      conv "$tier" bruker "" "$d" "$(mz_of "$d")"
    else
      conv "$tier" agilent "--agilent-grid" "$d" "$(mz_of "$d")"
    fi
  done < <(find "$dir" -type d -iname '*.d' 2>/dev/null | sort)
  while IFS= read -r f; do conv "$tier" thermo "" "$f" "$(mz_of "$f")"; done < <(find "$dir" -type f -iname '*.raw' 2>/dev/null | sort)
  # Waters .raw is a DIRECTORY (MassLynx) — host-unconvertible on macOS, box-deferred.
  while IFS= read -r d; do defer "$tier" waters-raw "$d"; done < <(find "$dir" -type d -iname '*.raw' 2>/dev/null | sort)
  while IFS= read -r f; do
    local ibd="${f%.*}.ibd"; conv "$tier" imzml "" "$f" "$(mz_of "$f")"
  done < <(find "$dir" -type f -iname '*.imzML' 2>/dev/null | sort)
  while IFS= read -r f; do conv "$tier" mzml "" "$f" "$(mz_of "$f")"; done < <(find "$dir" -type f -iname '*.mzML' ! -iname '*.imzML' 2>/dev/null | sort)
  while IFS= read -r f; do defer "$tier" sciex-wiff "$f"; done < <(find "$dir" -type f -iname '*.wiff' 2>/dev/null | sort)
}

# TIERS — most relevant (grid/vendor changes in this build) first, bulk mzML last.
for t in tof-grid-examples vendor-agilent-sciex vendor-flash-data vendor-waters vendor-bruker-baf \
         raw-bench raw-examples raw-replacements imzml-examples mzML-examples pwiz-examples sdrf-examples demo; do
  echo "============ TILE $t ============"
  walk_tier "$t" "$t"
done
echo "=== DONE; results: $tsv ==="
awk -F'\t' 'NR>1{s[$3]++} END{for(k in s) printf "  %s=%d\n", k, s[k]}' "$tsv"
