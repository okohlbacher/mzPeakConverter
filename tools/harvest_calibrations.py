#!/usr/bin/env python3
"""Harvest Bruker TDF mobility calibrations across many timsTOF runs to reverse-engineer the
ModelType-2 scan→1/K0 formula.

For each `.d.zip` URL in tools/timstof_urls.txt it:
  1. pulls ONLY `analysis.tdf` from the remote zip via HTTP range requests (no GB-scale binary),
  2. drops an empty `analysis.tdf_bin` beside it (the SDK's tims_open needs the file present, but
     tims_scannum_to_oneoverk0 reads only the calibration, not the frame data),
  3. runs `mzpeak-convert <dir>` with MZPC_DUMP_IM_TABLE=1 → the per-scan timsrust vs SDK 1/K0 table,
  4. reads TimsCalibration C0..C9 + CalibrationInfo reference points from the SQLite.

Writes out/calibrations.json (per run: coeffs, nscans, reference points, both 1/K0 columns). Run on
a Windows/Linux CI runner where the SDK is available (opentims_bruker_bridge supplies timsdata).

Usage: python tools/harvest_calibrations.py <path-to-mzpeak-convert-binary>
"""
import json
import os
import shutil
import struct
import subprocess
import sqlite3
import sys

from remotezip import RemoteZip

BIN = sys.argv[1] if len(sys.argv) > 1 else "target/release/mzpeak-convert"
URLS = [l.strip() for l in open("tools/timstof_urls.txt") if l.strip()]
os.makedirs("out", exist_ok=True)


def f64s(blob):
    if blob is None:
        return None
    raw = blob if isinstance(blob, bytes) else blob.encode("latin1")
    return list(struct.unpack(f"<{len(raw)//8}d", raw[: len(raw) // 8 * 8]))


results = []
for i, url in enumerate(URLS):
    d = "sample.d"
    try:
        shutil.rmtree(d, ignore_errors=True)
        os.makedirs(d)
        with RemoteZip(url) as z:
            tdf = [n for n in z.namelist() if n.lower().endswith("analysis.tdf")]
            if not tdf:
                print(f"[{i}] no analysis.tdf in {url.split('/')[-1]}")
                continue
            z.extract(tdf[0], "rz")
            shutil.copy(os.path.join("rz", tdf[0]), os.path.join(d, "analysis.tdf"))
            shutil.rmtree("rz", ignore_errors=True)
        open(os.path.join(d, "analysis.tdf_bin"), "wb").close()  # empty binary

        env = dict(os.environ, MZPC_DUMP_IM_TABLE="1", PYTHONIOENCODING="utf-8")
        p = subprocess.run([BIN, d], capture_output=True, text=True, env=env, timeout=300)
        rows = [r.split(",") for r in p.stdout.strip().splitlines()[1:]]
        timsrust = [float(r[1]) for r in rows if len(r) >= 2 and r[1]]
        sdk = [float(r[2]) for r in rows if len(r) >= 3 and r[2]]

        con = sqlite3.connect(os.path.join(d, "analysis.tdf"))
        c = con.cursor()
        cols = [x[1] for x in c.execute("PRAGMA table_info(TimsCalibration)")]
        cal = dict(zip(cols, c.execute("SELECT * FROM TimsCalibration").fetchone()))
        ns = c.execute("SELECT MAX(NumScans) FROM Frames").fetchone()[0]

        def blob(k):
            x = c.execute("SELECT Value FROM CalibrationInfo WHERE KeyName=?", (k,)).fetchone()
            return f64s(x[0]) if x else None

        ok = len(sdk) == len(timsrust) and len(sdk) > 0
        results.append(dict(
            url=url, nscans=ns,
            coeffs={k: str(cal[k]) for k in cal},
            Vref=blob("MeasuredTimsVoltages"),
            Mref=blob("MobilitiesCorrectedCalibration"),
            pressure=blob("MobilitiyReferencePressure"),
            timsrust=timsrust, sdk=sdk, sdk_ok=ok,
            stderr=(p.stderr or "")[-300:],
        ))
        print(f"[{i}] {url.split('/')[-1][:34]:34s} ns={ns} sdk_ok={ok}")
        con.close()
    except Exception as e:
        print(f"[{i}] ERR {type(e).__name__}: {e}")
    finally:
        shutil.rmtree(d, ignore_errors=True)

json.dump(results, open("out/calibrations.json", "w"))
n_ok = sum(1 for r in results if r.get("sdk_ok"))
print(f"\ncollected {len(results)} runs ({n_ok} with SDK column) -> out/calibrations.json")
