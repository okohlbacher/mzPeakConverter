#!/usr/bin/env python3
"""Render the full-corpus mzpeak/raw ratio table from corpus_full.sh results.

Usage: render_ratio_table.py HOST_RESULTS.tsv BOX_RESULTS.tsv > ratio-table-full.md

HOST_RESULTS.tsv columns: tier format status raw mzpeak ratio unit   (from corpus_full.sh)
BOX_RESULTS.tsv  columns: section display fmt raw mzpeak               (flash-workstation rows)

Box-converted datasets appear in HOST_RESULTS as BOX-DEFERRED (often duplicated across
tiers); they are excluded from the host tiers and rendered once in their box section using
the full vendor footprint measured on the box. Assessment = pooled mzpeak/raw over every
row EXCEPT the pwiz fixtures and units whose raw is <20 MB (the Parquet floor).
"""
import sys, csv
from collections import defaultdict

MB = 1_000_000
FLOOR = 20 * MB  # <20 MB raw excluded from the headline assessment (Parquet floor)

def mb(n): return f"{int(n)/MB:.1f}"

host_path, box_path = sys.argv[1], sys.argv[2]

# section -> list of (unit, fmt, raw, mzpeak, ratio)
sections = defaultdict(list)

with open(host_path) as f:
    for r in csv.DictReader(f, delimiter="\t"):
        if r["status"] != "OK":
            continue
        raw, mp = int(r["raw"]), int(r["mzpeak"])
        ratio = mp / raw if raw else 0.0
        unit = r["unit"].rstrip("/").split("/")[-1]
        sections[r["tier"]].append((unit, r["format"], raw, mp, ratio))

with open(box_path) as f:
    for r in csv.DictReader(f, delimiter="\t"):
        raw, mp = int(r["raw"]), int(r["mzpeak"])
        sections[r["section"]].append((r["display"], r["fmt"], raw, mp, mp / raw))

print("# mzPeak full-corpus ratio table (mzpeak / raw)\n")
print("Current build (intensity BYTE_STREAM_SPLIT encoding; SCIEX grid chromatograms point-encoded).")
print("**Headline exclusions:** pwiz fixtures + <20 MB units; imzML raw includes the `.ibd`;")
print("vendor/box rows converted on the flash-workstation with the full vendor footprint.\n")

assess_raw = assess_mp = 0
for sec in sorted(sections, key=str.lower):
    rows = sorted(sections[sec], key=lambda x: x[4])
    print(f"## {sec} ({len(rows)})\n")
    print("| unit | fmt | raw MB | mzpeak MB | ratio |")
    print("|---|---|--:|--:|--:|")
    for unit, fmt, raw, mp, ratio in rows:
        print(f"| {unit} | {fmt} | {mb(raw)} | {mb(mp)} | {ratio:.3f} |")
        if sec != "pwiz-examples" and raw >= FLOOR:
            assess_raw += raw
            assess_mp += mp
    print()

print("## assessment\n")
print(f"Pooled ratio over non-fixture units ≥20 MB raw: "
      f"**{assess_mp/assess_raw:.3f}** "
      f"({mb(assess_mp)} MB / {mb(assess_raw)} MB).")
