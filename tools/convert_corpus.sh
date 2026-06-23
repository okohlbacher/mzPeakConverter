#!/usr/bin/env bash
# Convert every macOS-supported raw dataset under a corpus root to mzPeak, keep the archives,
# and record raw-vs-mzpeak sizes for a compression-ratio table. Windows-only vendor formats
# (Sciex .wiff, Agilent .d, Waters .raw) are recorded as UNSUPPORTED for the CI path.
#
# Usage: tools/convert_corpus.sh [DATA_ROOT] [OUT_DIR]
set -uo pipefail

data="${1:-$HOME/Claude/mzML2mzPeak/data}"
out="${2:-$HOME/Claude/mzML2mzPeak/out/mzpeak-corpus}"
here="$(cd "$(dirname "$0")" && pwd)"
bin="$(dirname "$here")/target/release/mzpeak-convert"
[ -x "$bin" ] || { echo "build first: cargo build --release" >&2; exit 2; }

arch="$out/archives"; logs="$out/logs"; tsv="$out/results.tsv"
mkdir -p "$arch" "$logs"
printf 'id\tformat\tstatus\traw_bytes\tmzpeak_bytes\tratio\tnote\n' > "$tsv"

# bytes of a file or recursively of a directory
sizeof() { if [ -d "$1" ]; then du -sk "$1" | awk '{print $1*1024}'; else stat -f '%z' "$1"; fi; }
# slug a path (relative to data root) into a flat id
slug() { echo "${1#$data/}" | sed 's#\.[^./]*$##; s#[^A-Za-z0-9._-]#_#g; s#__*#_#g'; }

row() { printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$1" "$2" "$3" "$4" "$5" "$6" "$7" >> "$tsv"; }

convert() {  # id format input raw_bytes note
  local id="$1" fmt="$2" in="$3" raw="$4" note="$5"
  local a="$arch/$id.mzpeak"
  echo ">> [$fmt] $id"
  if "$bin" "$in" -o "$a" --force >"$logs/$id.log" 2>&1; then
    local mp; mp="$(sizeof "$a")"
    local r; r="$(awk -v m="$mp" -v w="$raw" 'BEGIN{ if(w>0) printf "%.4f", m/w; else print "NA"}')"
    row "$id" "$fmt" "OK" "$raw" "$mp" "$r" "$note"
  else
    row "$id" "$fmt" "CONV-ERR" "$raw" "" "" "see logs/$id.log"
  fi
}

# --- Thermo .raw / .RAW -------------------------------------------------------------------
while IFS= read -r f; do
  id="$(slug "$f")"; convert "$id" "thermo" "$f" "$(sizeof "$f")" "$(basename "$f")"
done < <(find "$data" -type f -iname '*.raw' | sort)

# --- Bruker TDF / TSF .d (dir directly holding analysis.tdf/tsf) --------------------------
while IFS= read -r d; do
  id="$(slug "$d")"; convert "$id" "bruker-tdf" "$d" "$(sizeof "$d")" "$(basename "$d")"
done < <(find "$data" -type d -iname '*.d' -exec test -f '{}/analysis.tdf' \; -print | sort)
while IFS= read -r d; do
  id="$(slug "$d")"; convert "$id" "bruker-tsf" "$d" "$(sizeof "$d")" "$(basename "$d")"
done < <(find "$data" -type d -iname '*.d' -exec test -f '{}/analysis.tsf' \; -print | sort)

# --- imzML (raw size = .imzML + matching .ibd) -------------------------------------------
while IFS= read -r f; do
  id="$(slug "$f")"; ibd="${f%.*}.ibd"
  raw="$(sizeof "$f")"; [ -f "$ibd" ] && raw=$((raw + $(sizeof "$ibd")))
  convert "$id" "imzml" "$f" "$raw" "$(basename "$f")"
done < <(find "$data" -type f -iname '*.imzML' | sort)

# --- UNSUPPORTED on macOS: record for the CI/Windows path --------------------------------
# Sciex .wiff/.wiff2
while IFS= read -r f; do
  id="$(slug "$f")"; row "$id" "sciex-wiff" "UNSUPPORTED" "$(sizeof "$f")" "" "" "needs Windows vendor reader (CI)"
done < <(find "$data" -type f \( -iname '*.wiff' -o -iname '*.wiff2' \) | sort)
# Agilent/other .d (no analysis.tdf/tsf and not a wrapper around one)
while IFS= read -r d; do
  if [ -z "$(find "$d" -name analysis.tdf -o -name analysis.tsf 2>/dev/null | head -1)" ]; then
    id="$(slug "$d")"; row "$id" "agilent-d" "UNSUPPORTED" "$(sizeof "$d")" "" "" "needs Windows vendor reader (CI)"
  fi
done < <(find "$data" -type d -iname '*.d' ! -exec test -f '{}/analysis.tdf' \; ! -exec test -f '{}/analysis.tsf' \; -print | sort)

echo "=== done; results in $tsv ==="
