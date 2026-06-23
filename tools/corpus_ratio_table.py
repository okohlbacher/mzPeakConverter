#!/usr/bin/env python3
"""Render results.tsv from convert_corpus.sh into a Markdown mzpeak/raw size-ratio table.

Usage: tools/corpus_ratio_table.py RESULTS.tsv [MORE.tsv ...] > table.md
Multiple TSVs (e.g. host + CI vendor results) are concatenated.
"""
import sys, csv, statistics as st

def human(n):
    n = float(n)
    for u in ("B", "KB", "MB", "GB", "TB"):
        if n < 1024 or u == "TB":
            return f"{n:.1f} {u}" if u != "B" else f"{int(n)} B"
        n /= 1024

rows = []
for path in sys.argv[1:]:
    with open(path) as f:
        for r in csv.DictReader(f, delimiter="\t"):
            rows.append(r)
if not rows:
    sys.exit("no rows")

def ratio(r):
    try: return float(r["ratio"])
    except (ValueError, KeyError): return None

ok = [r for r in rows if r["status"] == "OK" and ratio(r) is not None]

print("# mzPeak vs raw — compression ratios\n")
print(f"{len(ok)} converted of {len(rows)} datasets "
      f"(ratio = mzPeak size / raw size; <1 = smaller than raw).\n")

# detail table, grouped by format then best ratio
print("| dataset | format | raw | mzPeak | ratio |")
print("|---|---|--:|--:|--:|")
for r in sorted(rows, key=lambda r: (r["format"], ratio(r) if ratio(r) is not None else 9)):
    rt = ratio(r)
    rcol = f"{rt:.3f}" if rt is not None else f"**{r['status']}**"
    mp = human(r["mzpeak_bytes"]) if r.get("mzpeak_bytes") else "—"
    raw = human(r["raw_bytes"]) if r.get("raw_bytes") else "—"
    print(f"| {r['id']} | {r['format']} | {raw} | {mp} | {rcol} |")

# per-format + overall summary
print("\n## Summary by format\n")
print("| format | n (ok) | raw total | mzPeak total | pooled ratio | median ratio |")
print("|---|--:|--:|--:|--:|--:|")
fmts = sorted(set(r["format"] for r in rows))
tot_raw = tot_mp = 0
for fmt in fmts:
    g = [r for r in ok if r["format"] == fmt]
    if not g:
        n_un = sum(1 for r in rows if r["format"] == fmt)
        print(f"| {fmt} | 0 / {n_un} | — | — | — | — |")
        continue
    raw = sum(int(r["raw_bytes"]) for r in g)
    mp = sum(int(r["mzpeak_bytes"]) for r in g)
    tot_raw += raw; tot_mp += mp
    med = st.median(ratio(r) for r in g)
    print(f"| {fmt} | {len(g)} | {human(raw)} | {human(mp)} | {mp/raw:.3f} | {med:.3f} |")
print(f"| **all** | **{len(ok)}** | **{human(tot_raw)}** | **{human(tot_mp)}** | "
      f"**{tot_mp/tot_raw:.3f}** | **{st.median(ratio(r) for r in ok):.3f}** |")
