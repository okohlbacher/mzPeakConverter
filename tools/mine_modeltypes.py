#!/usr/bin/env python3
"""Mine the Bruker `TimsCalibration.ModelType` across many timsTOF runs to check whether any model
other than ModelType 2 occurs in the wild (our scan->1/K0 recalibration is type-2-only).

Reads tools/modeltype_mine_urls.txt (TAB-separated: repo, accession, .d.zip URL). For each, pulls
ONLY `analysis.tdf` from the remote zip via HTTP range requests (no GB-scale binary) and reads the
ModelType(s). No SDK / converter needed — pure metadata. Writes out/modeltypes.json + a summary.
"""
import json
import os
import shutil
import sqlite3
import sys
from collections import Counter

from remotezip import RemoteZip

LINES = [l.rstrip("\n") for l in open("tools/modeltype_mine_urls.txt") if l.strip()]
os.makedirs("out", exist_ok=True)
results = []

for i, line in enumerate(LINES):
    parts = line.split("\t")
    repo, acc, url = parts if len(parts) == 3 else ("?", "?", parts[-1])
    d, con = "md", None
    try:
        shutil.rmtree(d, ignore_errors=True)
        shutil.rmtree("rz", ignore_errors=True)
        os.makedirs(d, exist_ok=True)
        with RemoteZip(url) as z:
            tdf = [n for n in z.namelist() if n.lower().endswith("analysis.tdf")]
            if not tdf:
                print(f"[{i}] {acc}: no analysis.tdf")
                continue
            z.extract(tdf[0], "rz")
            shutil.copy(os.path.join("rz", tdf[0]), os.path.join(d, "analysis.tdf"))
            shutil.rmtree("rz", ignore_errors=True)
        con = sqlite3.connect(os.path.join(d, "analysis.tdf"))
        c = con.cursor()
        rows = c.execute("SELECT Id, ModelType FROM TimsCalibration ORDER BY Id").fetchall()
        ns = c.execute("SELECT MAX(NumScans) FROM Frames").fetchone()[0]
        mts = [r[1] for r in rows]
        results.append(dict(repo=repo, acc=acc, modeltypes=mts, nscans=ns))
        print(f"[{i}] {repo} {acc}: ModelType={mts} nscans={ns}")
    except Exception as e:
        print(f"[{i}] {acc}: ERR {type(e).__name__}: {e}")
    finally:
        if con is not None:
            con.close()
        shutil.rmtree(d, ignore_errors=True)

json.dump(results, open("out/modeltypes.json", "w"))
mt = Counter()
for r in results:
    for m in r["modeltypes"]:
        mt[m] += 1
print(f"\n=== {len(results)} timsTOF datasets read; ModelType distribution: {dict(mt)} ===")
non2 = [r for r in results if any(m != 2 for m in r["modeltypes"])]
if non2:
    print("NON-ModelType-2 datasets:")
    for r in non2:
        print(f"  {r['repo']} {r['acc']}: {r['modeltypes']}")
else:
    print("All datasets are ModelType 2.")
