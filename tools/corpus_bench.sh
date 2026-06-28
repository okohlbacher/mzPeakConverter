#!/usr/bin/env bash
# mzPeak corpus benchmark / comparison harness.
#
# Reads tools/corpus_manifest.tsv and, per dataset: converts raw -> mzPeak, validates the archive,
# and measures raw/mzPeak/ratio + signal bytes-per-point. Emits a detailed TSV + a markdown table
# (sorted by ratio; datasets < 20 MB raw flagged EXCLUDED — Parquet/ZIP floor, not real compression).
#
# Usage:
#   tools/corpus_bench.sh [--box] [--out DIR] [--keep] [--only ID[,ID...]]
#     --box   also run engine=box datasets on flash-workstation (needs /tmp/flashps.sh + creds)
#     --keep  keep the produced .mzpeak archives (default: delete after measuring)
#     --only  restrict to the given manifest ids
set -uo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
manifest="$root/tools/corpus_manifest.tsv"
bin="$root/target/release/mzpeak-convert"
py="$HOME/anaconda3/envs/mzpeak314/bin/python"; [ -x "$py" ] || py="$HOME/anaconda3/bin/python"
# validator: prefer the modern pyarrow env
validate=""
for c in "${MZPEAK_VALIDATE:-}" "$HOME/anaconda3/envs/mzpeak314/bin/mzpeak-validate" "$HOME/anaconda3/envs/mzpeak/bin/mzpeak-validate"; do
  [ -n "$c" ] && [ -x "$c" ] && { validate="$c"; break; }
done

run_box=0; keep=0; only=""; out="${TMPDIR:-/tmp}/corpus_bench"
while [ $# -gt 0 ]; do case "$1" in
  --box) run_box=1; shift;; --keep) keep=1; shift;;
  --out) out="$2"; shift 2;; --only) only=",$2,"; shift 2;;
  *) echo "unknown arg: $1" >&2; exit 2;;
esac; done
mkdir -p "$out"
[ -x "$bin" ] || { echo "build first: cargo build --release" >&2; exit 2; }

tsv="$out/results.tsv"
printf 'id\tvendor\tinstrument\tformat\tengine\tstatus\tvalidate\traw\tmzpeak\tratio\tpoints\tbpp\taccession\n' > "$tsv"

# bytes/point + total signal points from an mzPeak (peaks facet rows + data facet points).
points_and_signal() {  # archive -> "POINTS SIGNAL_BYTES"
  "$py" - "$1" <<'PY' 2>/dev/null
import sys,zipfile,io,pyarrow.parquet as pq
z=zipfile.ZipFile(sys.argv[1]); pts=0; sig=0
for n in z.namelist():
    if not n.endswith('.parquet'): continue
    if 'spectra_peaks' in n:
        m=pq.ParquetFile(io.BytesIO(z.read(n))).metadata; pts+=m.num_rows; sig+=z.getinfo(n).compress_size
    elif 'spectra_data' in n:
        sig+=z.getinfo(n).compress_size
        # data-facet points = sum of intensity list lengths (sample-free: read the column)
        try:
            t=pq.read_table(io.BytesIO(z.read(n)),columns=['chunk'])
            inten=t.column('chunk').combine_chunks().field('intensity')
            pts+=sum(len(x) for x in inten.to_pylist())
        except Exception: pass
print(pts, sig)
PY
}

# raw footprint: a directory (.d/.raw) counts everything; a FILE counts its vendor sidecars too
# (imzML binary data lives in the .ibd; SCIEX .wiff carries .wiff.scan/.wiff2 — handled on the box).
sizeof() {
  local p="$1" t; t=$(du -sk "$p" 2>/dev/null | awk '{print $1*1024}')
  case "$p" in
    *.imzML) local ibd="${p%.imzML}.ibd"; [ -f "$ibd" ] && t=$((t + $(du -sk "$ibd" | awk '{print $1*1024}'))) ;;
  esac
  echo "$t"
}
ratio()  { awk -v m="$1" -v r="$2" 'BEGIN{if(r>0) printf "%.3f", m/r; else print "NA"}'; }
bpp()    { awk -v b="$1" -v p="$2" 'BEGIN{if(p>0) printf "%.2f", b/p; else print "NA"}'; }

while IFS=$'\t' read -r id vendor instrument format engine raw flags accession; do
  case "$id" in '#'*|'') continue;; esac
  [ -n "$only" ] && case "$only" in *",$id,"*) :;; *) continue;; esac
  echo ">> $id ($vendor / $instrument)"

  if [ "$engine" = box ]; then
    if [ "$run_box" -ne 1 ]; then
      printf '%s\t%s\t%s\t%s\t%s\tSKIP-box\t-\t-\t-\t-\t-\t-\t%s\n' "$id" "$vendor" "$instrument" "$format" "$engine" "$accession" >> "$tsv"
      continue
    fi
    # convert on flash-workstation, read size there (full transfer optional via --keep)
    res="$("$root/tools/corpus_box_convert.sh" "$raw" "$flags" 2>/dev/null)"  # echoes: RAW MZPEAK BOXPATH
    rawb="$(echo "$res" | awk '{print $1}')"; mp="$(echo "$res" | awk '{print $2}')"
    if [ -z "$mp" ] || [ "$mp" = 0 ]; then
      printf '%s\t%s\t%s\t%s\t%s\tCONV-ERR\t-\t%s\t-\t-\t-\t-\t%s\n' "$id" "$vendor" "$instrument" "$format" "$engine" "${rawb:-?}" "$accession" >> "$tsv"
      continue
    fi
    printf '%s\t%s\t%s\t%s\t%s\tOK\t(box)\t%s\t%s\t%s\t-\t-\t%s\n' "$id" "$vendor" "$instrument" "$format" "$engine" "$rawb" "$mp" "$(ratio "$mp" "$rawb")" "$accession" >> "$tsv"
    continue
  fi

  # HOST conversion
  [ -e "$raw" ] || { printf '%s\t%s\t%s\t%s\t%s\tMISSING\t-\t-\t-\t-\t-\t-\t%s\n' "$id" "$vendor" "$instrument" "$format" "$engine" "$accession" >> "$tsv"; continue; }
  rawb="$(sizeof "$raw")"
  arch="$out/$id.mzpeak"
  if DOTNET_ROLL_FORWARD=LatestMajor "$bin" "$raw" $flags -o "$arch" --force > "$out/$id.log" 2>&1 && [ -f "$arch" ]; then
    mp="$(stat -f %z "$arch" 2>/dev/null || stat -c %s "$arch")"
    read -r pts sig <<<"$(points_and_signal "$arch")"
    val="?"
    if [ -n "$validate" ]; then "$validate" "$arch" > "$out/$id.val" 2>&1 && val="PASS" || val="FAIL"; fi
    printf '%s\t%s\t%s\t%s\t%s\tOK\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
      "$id" "$vendor" "$instrument" "$format" "$engine" "$val" "$rawb" "$mp" "$(ratio "$mp" "$rawb")" "${pts:-0}" "$(bpp "${sig:-0}" "${pts:-1}")" "$accession" >> "$tsv"
    [ "$keep" -eq 1 ] || rm -f "$arch"
  else
    printf '%s\t%s\t%s\t%s\t%s\tCONV-ERR\t-\t%s\t-\t-\t-\t-\t%s\n' "$id" "$vendor" "$instrument" "$format" "$engine" "$rawb" "$accession" >> "$tsv"
  fi
done < "$manifest"

# ---- render the comparison table (markdown), sorted by ratio, < 20 MB flagged EXCLUDED ----
md="$out/REPORT.md"
{
  echo "# mzPeak corpus benchmark"
  echo
  echo "mzPeak / raw on the data representation (\`--no-vendor\`). Datasets < 20 MB raw are EXCLUDED from the"
  echo "headline (Parquet/ZIP floor). bpp = signal bytes/point. Generated by tools/corpus_bench.sh."
  echo
  echo "| dataset | vendor / instrument | raw | mzPeak | ratio | bpp | validate | note |"
  echo "|---|---|--:|--:|--:|--:|:--:|---|"
  awk -F'\t' 'NR>1{print}' "$tsv" | sort -t$'\t' -k10,10g | while IFS=$'\t' read -r id vendor instrument format engine status validate raw mp r pts bp acc; do
    [ "$status" = OK ] || { echo "| $id | $vendor $instrument | - | - | - | - | - | $status |"; continue; }
    excl=""; [ "$raw" -lt 20000000 ] 2>/dev/null && excl="EXCLUDED <20MB"
    rh=$(awk -v b="$raw" 'BEGIN{printf "%.0f MB", b/1e6}')
    mh=$(awk -v b="$mp"  'BEGIN{printf "%.0f MB", b/1e6}')
    echo "| $id | $vendor $instrument | $rh | $mh | **$r** | $bp | $validate | $excl |"
  done
  echo
  echo "Included-only mean ratio:"
  awk -F'\t' 'NR>1 && $6=="OK" && $8>=20000000 {s+=$10; n++} END{if(n) printf "  **%.3f** over %d datasets\n", s/n, n}' "$tsv"
} > "$md"
echo
echo "=== wrote $tsv and $md ==="
cat "$md"
