#!/usr/bin/env bash
# Convert Windows-only vendor datasets (Sciex .wiff, Agilent .d, Waters .raw dir) to mzPeak on a
# CI runner via the ProteoWizard lane (`--via-msconvert`), record raw-vs-mzpeak sizes, AND capture a
# per-dataset LAYOUT report (inspect output + per-facet parquet sizes + bytes/peak) so the savings
# can be judged against the "non-standard spectra" proposal (grid-TOF / IMS-compaction).
#
# The produced .mzpeak archives + results.tsv + vendor_layouts.{md,tsv} are left in OUT_DIR for the
# workflow to upload as an artifact.
#
# Disk discipline: each vendor input is converted, measured, its layout captured, and THEN the large
# raw input is DELETED before the next dataset — so a runner with a small disk can process several
# multi-GB datasets one at a time. (The .mzpeak archives are kept; they are the deliverable and are
# far smaller than the raw inputs.)
#
# Runs under git-bash on windows-latest (or any platform with msconvert on PATH / $MSCONVERT_PATH).
#
# Usage: tools/convert_vendor_ci.sh INPUT_DIR [OUT_DIR]
#   INPUT_DIR  directory holding vendor datasets (scanned recursively)
#   OUT_DIR    default: ./vendor-mzpeak-out
set -uo pipefail

input="${1:?usage: convert_vendor_ci.sh INPUT_DIR [OUT_DIR]}"
out="${2:-./vendor-mzpeak-out}"
here="$(cd "$(dirname "$0")" && pwd)"
root="$(dirname "$here")"
# release exe on Windows is .exe; fall back to the Unix name elsewhere
bin="$root/target/release/mzpeak-convert.exe"; [ -x "$bin" ] || bin="$root/target/release/mzpeak-convert"
[ -x "$bin" ] || { echo "build first: cargo build --release" >&2; exit 2; }

arch="$out/archives"; logs="$out/logs"; tsv="$out/results.tsv"
layout_md="$out/vendor_layouts.md"; layout_tsv="$out/vendor_layouts.tsv"
mkdir -p "$arch" "$logs"
printf 'id\tformat\tstatus\traw_bytes\tmzpeak_bytes\tratio\tnote\n' > "$tsv"
printf 'id\tformat\tspectra\tpeaks\tmzpeak_bytes\tpeaks_parquet_bytes\tbytes_per_peak\tprofile_or_centroid\tims_axis\tnote\n' > "$layout_tsv"
{
  echo "# Vendor layout exploration (msconvert lane → STANDARD float-m/z mzPeak baseline)"
  echo
  echo "Each dataset below is converted via ProteoWizard \`--via-msconvert\`, which produces the"
  echo "**standard float64-m/z mzPeak** (the measured baseline). The proposal's columnar grid-TOF /"
  echo "IMS-compaction facet is **not** implemented for these vendors, so the projected proposal"
  echo "saving is reasoned per-column and clearly separated from the measured baseline."
  echo
} > "$layout_md"

# portable recursive byte count (GNU du -b, else BSD du -sk*1024)
sizeof() {
  if du -sb "$1" >/dev/null 2>&1; then du -sb "$1" | awk '{print $1}'
  else du -sk "$1" | awk '{print $1*1024}'; fi
}
slug() { echo "${1#$input/}" | sed 's#\.[^./]*$##; s#[^A-Za-z0-9._-]#_#g; s#__*#_#g'; }

# Count rows in the peaks facet of an .mzpeak (a zip of parquet). Best-effort: needs python+pyarrow.
# Prints a peak count, or empty on failure. Also echoes the peaks-facet parquet byte size via stderr
# is not used; we read the size separately from `unzip -l`.
peaks_rowcount() {  # archive
  python - "$1" <<'PY' 2>/dev/null
import sys, zipfile, io
try:
    import pyarrow.parquet as pq
except Exception:
    sys.exit(0)
arc = sys.argv[1]
z = zipfile.ZipFile(arc)
# the peaks facet holds one row per data point; names seen: spectra_peaks.parquet / *_peaks.parquet
cand = [n for n in z.namelist() if n.endswith('.parquet') and 'peak' in n.lower()]
if not cand:
    cand = [n for n in z.namelist() if n.endswith('.parquet') and ('data' in n.lower() or 'point' in n.lower())]
total = 0
for n in cand:
    try:
        total += pq.ParquetFile(io.BytesIO(z.read(n))).metadata.num_rows
    except Exception:
        pass
if total:
    print(total)
PY
}

# Append a per-facet size table for an archive to the layout markdown, and return the peaks-facet
# parquet byte size on stdout (0 if none found).
facet_table() {  # id archive
  local id="$1" a="$2"
  {
    echo "### \`$id\` — mzPeak facet sizes (\`unzip -l\`)"
    echo
    echo '```'
    unzip -l "$a" 2>/dev/null | awk 'NR>3 && $4!="" {printf "%12s  %s\n",$1,$4}' | sed '/^[[:space:]]*[0-9-]* files$/d'
    echo '```'
    echo
  } >> "$layout_md"
  # peaks-facet compressed-or-stored size from the zip central directory
  unzip -l "$a" 2>/dev/null | awk '/parquet/ && /peak/ {s+=$1} END{print s+0}'
}

# Inspect the raw input (no -o → format / #spectra / #chromatograms) and stash the text. Returns the
# spectra count on stdout (empty if not parseable).
inspect() {  # id input
  local id="$1" in="$2" rep="$logs/$id.inspect.txt"
  "$bin" "$in" > "$rep" 2>&1 || true
  {
    echo "### \`$id\` — inspect (no conversion)"
    echo
    echo '```'
    cat "$rep"
    echo '```'
    echo
  } >> "$layout_md"
  awk -F: '/^spectra:/ {gsub(/ /,"",$2); print $2; exit}' "$rep"
}

# heuristic: profile vs centroid + IMS axis from the inspect report / format string
classify() {  # id  -> "profile_or_centroid<TAB>ims_axis"
  local rep="$logs/$1.inspect.txt"; local pc="unknown" ims="none"
  grep -qi 'profile' "$rep" 2>/dev/null && pc="profile"
  grep -qi 'centroid' "$rep" 2>/dev/null && pc="centroid"
  grep -qi 'ims-compact\|mobility\|ion.mobility\|tims\|twims\|dtims' "$rep" 2>/dev/null && ims="present"
  printf '%s\t%s' "$pc" "$ims"
}

convert() {  # id format input raw_bytes
  local id="$1" fmt="$2" in="$3" raw="$4" a="$arch/$1.mzpeak"
  echo ">> [$fmt] $id"
  echo "## $id ($fmt)" >> "$layout_md"; echo >> "$layout_md"

  # 1) INSPECT (proposal "explore" half) — format, #spectra, #chromatograms
  local spectra; spectra="$(inspect "$id" "$in")"

  # 2) CONVERT (measured baseline)
  if "$bin" "$in" --via-msconvert -o "$a" --force >"$logs/$id.log" 2>&1 && [ -f "$a" ]; then
    local mp; mp="$(sizeof "$a")"
    local r; r="$(awk -v m="$mp" -v w="$raw" 'BEGIN{ if(w>0) printf "%.4f", m/w; else print "NA"}')"
    printf '%s\t%s\tOK\t%s\t%s\t%s\t%s\n' "$id" "$fmt" "$raw" "$mp" "$r" "$(basename "$in")" >> "$tsv"

    # 3) LAYOUT — per-facet sizes + bytes/peak
    local peakbytes; peakbytes="$(facet_table "$id" "$a")"
    local npeaks; npeaks="$(peaks_rowcount "$a")"
    local bpp=""
    [ -n "$npeaks" ] && [ "$npeaks" -gt 0 ] 2>/dev/null && \
      bpp="$(awk -v b="$peakbytes" -v n="$npeaks" 'BEGIN{ if(n>0) printf "%.3f", b/n }')"
    local cls; cls="$(classify "$id")"
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
      "$id" "$fmt" "${spectra:-}" "${npeaks:-}" "$mp" "${peakbytes:-}" "${bpp:-}" "$cls" "$(basename "$in")" >> "$layout_tsv"
    {
      echo "**Measured:** raw=$raw B, mzPeak=$mp B, ratio=$r, spectra=${spectra:-?}, peaks=${npeaks:-?}, peaks-parquet=${peakbytes:-?} B, bytes/peak=${bpp:-n/a}."
      echo
    } >> "$layout_md"
  else
    printf '%s\t%s\tCONV-ERR\t%s\t\t\tsee logs/%s.log\n' "$id" "$fmt" "$raw" "$id" >> "$tsv"
    local cls; cls="$(classify "$id")"
    printf '%s\t%s\t%s\t\t\t\t\t%s\tCONV-ERR\n' "$id" "$fmt" "${spectra:-}" "$cls" >> "$layout_tsv"
    { echo "**CONV-ERR** — see \`logs/$id.log\`. raw=$raw B, spectra=${spectra:-?}."; echo; } >> "$layout_md"
  fi

  # 4) DELETE the large raw input now that it has been measured (disk discipline).
  rm -rf "$in" 2>/dev/null || true
}

# Sciex .wiff (raw size includes .wiff.scan / .wiff2 sidecars). Convert one at a time, delete after.
while IFS= read -r f; do
  [ -f "$f" ] || continue
  id="$(slug "$f")"; raw="$(sizeof "$f")"
  for sc in "$f.scan" "${f}2"; do [ -f "$sc" ] && raw=$((raw + $(sizeof "$sc"))); done
  convert "$id" "sciex-wiff" "$f" "$raw"
  # remove the wiff sidecars too (convert() deleted the .wiff itself)
  rm -f "$f.scan" "${f}2" 2>/dev/null || true
done < <(find "$input" -type f -iname '*.wiff' | sort)

# Agilent .d (directories without Bruker analysis.tdf/tsf)
while IFS= read -r d; do
  [ -d "$d" ] || continue
  if [ -z "$(find "$d" -name analysis.tdf -o -name analysis.tsf 2>/dev/null | head -1)" ]; then
    id="$(slug "$d")"; convert "$id" "agilent-d" "$d" "$(sizeof "$d")"
  fi
done < <(find "$input" -type d -iname '*.d' | sort)

# Waters .raw (a DIRECTORY; Thermo .raw is a file and is handled on the host, not here)
while IFS= read -r d; do
  [ -d "$d" ] || continue
  id="$(slug "$d")"; convert "$id" "waters-raw" "$d" "$(sizeof "$d")"
done < <(find "$input" -type d -iname '*.raw' | sort)

echo "=== done; results in $tsv ==="
cat "$tsv"
echo "=== layout summary in $layout_tsv ==="
cat "$layout_tsv"
