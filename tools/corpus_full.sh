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
only=""; box=""; box_jobs=4; skip_ok=""
while [ $# -gt 0 ]; do case "$1" in
  --only) only=",$2,"; shift 2;;
  --box) box=1; shift;;                       # convert vendor units on the flash-workstation via box_convert
  --box-jobs) box_jobs="$2"; shift 2;;
  --skip-ok) skip_ok="$2"; shift 2;;          # resume: skip units already OK in a prior results.tsv
  *) shift;; esac; done
here="$(cd "$(dirname "$0")" && pwd)"; bin="$(dirname "$here")/target/release/mzpeak-convert"
[ -x "$bin" ] || { echo "build first: cargo build --release" >&2; exit 2; }
out="$root/validator_logs/reconvert-$(date +%Y%m%d-%H%M%S)"; mkdir -p "$out/logs"
tsv="$out/results.tsv"; printf 'tier\tformat\tstatus\traw\tmzpeak\tratio\tunit\n' > "$tsv"
boxtrack="$out/box-units.tsv"; : > "$boxtrack"    # tier<TAB>fmt<TAB>path<TAB>out<TAB>opts (when --box)
echo "reconvert -> $out${box:+  (vendor units via box_convert, --box-jobs $box_jobs)}"

sizeof(){ if [ -d "$1" ]; then du -sk "$1" | awk '{print $1*1024}'; else stat -f %z "$1" 2>/dev/null || echo 0; fi; }
slug(){ echo "${1#$root/}" | sed 's#[^A-Za-z0-9._-]#_#g; s#__*#_#g'; }
ratio(){ awk -v m="$1" -v w="$2" 'BEGIN{if(w>0) printf "%.4f", m/w; else print "NA"}'; }
# JOBS>1 runs host conversions in parallel (independent output files; each appends one short line to
# the TSV, which is atomic under PIPE_BUF). Big inputs (>=BIG_MB) still run one-at-a-time so a cluster
# of multi-GB DIA runs can't blow the RAM ceiling. Portable throttle (no `wait -n`: macOS bash is 3.2).
JOBS="${JOBS:-1}"; BIG_MB="${BIG_MB:-1500}"
throttle(){ local n="$1"; while [ "$(jobs -rp | wc -l | tr -d ' ')" -ge "$n" ]; do sleep 0.3; done; }
# A Thermo RAW file starts with 01 A1 followed by "Finnigan" in UTF-16LE. Peek the first bytes,
# drop the interleaved NULs, and look for the magic — robust to corpus dirs mislabeled by vendor.
raw_is_thermo(){ LC_ALL=C head -c 64 "$1" 2>/dev/null | LC_ALL=C tr -d '\000' | LC_ALL=C grep -qa 'Finnigan'; }
# --skip-ok: a unit (path relative to root) is "done" if it was OK in the prior results.tsv.
okset=""
if [ -n "$skip_ok" ] && [ -f "$skip_ok" ]; then
  okset="$out/.skip-ok"; awk -F'\t' '$3=="OK"{print $7}' "$skip_ok" | sort -u > "$okset"
  echo "resume: skipping $(wc -l < "$okset" | tr -d ' ') units already OK in $skip_ok"
fi
is_done(){ [ -n "$okset" ] && grep -Fxq "$1" "$okset"; }

conv(){ # tier format flags input outpath
  local tier="$1" fmt="$2" flags="$3" in="$4" o="$5"
  [ -n "$only" ] && case "$only" in *",$tier,"*) :;; *) return;; esac
  is_done "${in#$root/}" && return            # --skip-ok: already OK in the prior run
  local id; id="$(slug "$in")"; local raw; raw="$(sizeof "$in")"
  # imzML signal lives in the .ibd sidecar — count it in the raw footprint or ratios are inflated.
  if [ "$fmt" = imzml ]; then local ibd="${in%.*}.ibd"; [ -f "$ibd" ] && raw=$((raw + $(sizeof "$ibd"))); fi
  echo ">> [$tier/$fmt] $id"
  # The actual convert+record. Backgrounded when JOBS>1; big inputs forced serial (gate at JOBS=1).
  _conv_run(){
    if "$bin" "$in" $flags -o "$o" --force > "$out/logs/$id.log" 2>&1; then
      local mp; mp="$(sizeof "$o")"
      printf '%s\t%s\tOK\t%s\t%s\t%s\t%s\n' "$tier" "$fmt" "$raw" "$mp" "$(ratio "$mp" "$raw")" "${in#$root/}" >> "$tsv"
    else
      local rc=$?; local st="CONV-ERR"; [ "$rc" = 3 ] && st="UNSUPPORTED"
      printf '%s\t%s\t%s\t%s\t\t\tsee logs/%s.log\n' "$tier" "$fmt" "$st" "$raw" "$id" >> "$tsv"
    fi
  }
  if [ "$JOBS" -le 1 ]; then
    _conv_run
  elif [ "$raw" -ge $((BIG_MB*1024*1024)) ]; then
    wait; _conv_run            # big file: drain the pool, then run it alone
  else
    throttle "$JOBS"; _conv_run &
  fi
}
defer(){ # tier fmt path — enqueue for box conversion (--box) else record BOX-DEFERRED
  is_done "${3#$root/}" && return              # --skip-ok: already OK in the prior run
  # In resume mode the box phase has no prior results.tsv rows, so treat a box unit as done when its
  # output already exists (no output-changing writer fix since) — only missing ones get reconverted.
  [ -n "$skip_ok" ] && [ -f "$(mz_of "$3")" ] && return
  if [ -n "$box" ]; then
    printf '%s\t%s\t%s\t%s\t%s\n' "$1" "$2" "$3" "$(mz_of "$3")" "$(box_opts "$2")" >> "$boxtrack"
  else
    printf '%s\t%s\tBOX-DEFERRED\t%s\t\t\t%s\n' "$1" "$2" "$(sizeof "$3")" "${3#$root/}" >> "$tsv"
  fi
}
box_opts(){ case "$1" in *) echo "--no-vendor";; esac; }   # vendor units: skip embedding on the box

# in-place target: replace the unit's extension with .mzpeak (deterministic, 1:1 with the unit)
mz_of(){ local p="$1"; case "$p" in *.*) echo "${p%.*}.mzpeak";; *) echo "$p.mzpeak";; esac; }

walk_tier(){ # tier dir  — dispatch every unit in a tile by type
  local tier="$1" dir="$root/$2"; [ -d "$dir" ] || return
  # .d dispatch by CONTENT, not by absence-of-Bruker-markers:
  #  - a .d holding a nested *.d is a container/wrapper (e.g. Bruker SBA415_Try.d wrapping the real
  #    acquisition .d) — skip it; the inner .d is walked on its own and classified correctly.
  #  - analysis.tdf/tsf  -> Bruker, host-native.
  #  - AcqData/ with a non-empty MSProfile.bin -> Agilent profile, host-griddable.
  #  - AcqData/ without profile data -> centroid-only Agilent, needs the Windows MHDAC SDK -> box.
  #  - anything else -> unrecognized, skip loudly (don't feed it to --agilent-grid and crash).
  while IFS= read -r d; do
    if find "$d" -mindepth 1 -maxdepth 1 -type d -iname '*.d' 2>/dev/null | grep -q .; then
      echo "  skip wrapper .d (holds a nested .d): ${d#$root/}" >&2
    elif find "$d" -maxdepth 1 \( -name analysis.tdf -o -name analysis.tsf \) | grep -q .; then
      conv "$tier" bruker "" "$d" "$(mz_of "$d")"
    elif [ -d "$d/AcqData" ]; then
      if [ -s "$d/AcqData/MSProfile.bin" ]; then
        conv "$tier" agilent "--agilent-grid" "$d" "$(mz_of "$d")"
      else
        defer "$tier" agilent-d "$d"   # centroid-only Agilent .d -> Windows MHDAC on the box
      fi
    else
      echo "  skip unrecognized .d (no tdf/tsf/AcqData): ${d#$root/}" >&2
    fi
  done < <(find "$dir" -type d -iname '*.d' 2>/dev/null | sort)
  # .raw FILES are Thermo only if they carry the Finnigan magic (01 A1 "Finnigan" in UTF-16). A .raw
  # without it is not Thermo (mislabeled corpus dir, or another vendor) — record it instead of
  # crashing the Thermo reader on it. (Waters .raw is a DIRECTORY, deferred just below.)
  while IFS= read -r f; do
    if raw_is_thermo "$f"; then
      conv "$tier" thermo "" "$f" "$(mz_of "$f")"
    else
      echo "  skip non-Thermo .raw (no Finnigan magic): ${f#$root/}" >&2
      printf '%s\t%s\tUNSUPPORTED\t%s\t\t\t%s\n' "$tier" raw-unknown "$(sizeof "$f")" "${f#$root/}" >> "$tsv"
    fi
  done < <(find "$dir" -type f -iname '*.raw' 2>/dev/null | sort)
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
wait   # drain any in-flight parallel (JOBS>1) host conversions before the box phase / summary

# --box: convert the enqueued vendor units on the flash-workstation via box_convert (S3-relayed,
# parallel), then record each outcome by whether the sibling .mzpeak now exists.
if [ -n "$box" ] && [ -s "$boxtrack" ]; then
  echo "============ BOX ($(wc -l < "$boxtrack" | tr -d ' ') vendor units, --jobs $box_jobs) ============"
  awk -F'\t' '{print $3"\t"$4"\t"$5}' "$boxtrack" > "$out/box-manifest.tsv"
  "$here/box_convert.sh" --local-manifest "$out/box-manifest.tsv" --jobs "$box_jobs" \
    2>&1 | sed 's/^/  box| /' || true
  while IFS=$'\t' read -r btier bfmt bpath bout bopts; do
    raw="$(sizeof "$bpath")"
    if [ -f "$bout" ]; then
      mp="$(sizeof "$bout")"
      printf '%s\t%s\tOK\t%s\t%s\t%s\t%s\n' "$btier" "$bfmt" "$raw" "$mp" "$(ratio "$mp" "$raw")" "${bpath#$root/}" >> "$tsv"
    else
      printf '%s\t%s\tCONV-ERR\t%s\t\t\t%s (box)\n' "$btier" "$bfmt" "$raw" "${bpath#$root/}" >> "$tsv"
    fi
  done < "$boxtrack"
fi

echo "=== DONE; results: $tsv ==="
awk -F'\t' 'NR>1{s[$3]++} END{for(k in s) printf "  %s=%d\n", k, s[k]}' "$tsv"
