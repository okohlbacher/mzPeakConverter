#!/usr/bin/env python3
"""Peak-for-peak comparison of a `.mzpeak` against the vendor mzML export of the same run.

This is the release gate for the Shimadzu native lane. It exists because the defect it checks for
is invisible to every self-consistency check: the native lane stored each spectrum's centroid
intensities rotated against the m/z axis (`[s alien values] + truth[0:n-s]`, s in 1..7, plus a
dropped final peak), and the archive's own TIC/BPI/base-peak aggregates were recomputed FROM the
rotated arrays, so they agreed perfectly. Only an external oracle catches it.

Per spectrum it checks:

  count        stored peak count == oracle count      (the rotation also clipped the last peak)
  m/z          ordered, matching within --mz-tol      (chunk lattice quantisation is ~5e-5)
  intensity    BIT-exact against the oracle           (not "close": the values were always right,
                                                       it was the pairing that was wrong)
  rotation     no shift s in 1..8 for which stored[s:] == oracle[:n-s]

Exit status: 0 all checks pass · 1 a check failed · 2 no oracle to compare against (never silently
"pass" a file we could not check).

    tools/compare_lcd_native_mzml.py ARCHIVE.mzpeak [--oracle RUN.mzML]
    tools/compare_lcd_native_mzml.py ARCHIVE.mzpeak --baseline OLD.mzpeak   # metamorphic replay
"""
from __future__ import annotations

import argparse
import base64
import io
import json
import sys
import zipfile
import zlib
import xml.etree.ElementTree as ET
from collections import Counter
from pathlib import Path

import numpy as np
import pyarrow.parquet as pq

NS = "{http://psi.hupo.org/ms/mzml}"
MZ_ARRAY, INTENSITY_ARRAY = "MS:1000514", "MS:1000515"
F64, F32, ZLIB = "MS:1000523", "MS:1000521", "MS:1000574"
CENTROID, PROFILE = "MS:1000127", "MS:1000128"
DELTA_CHUNK = "MS:1003089"

# Suffixes our own conversions add, stripped when guessing the oracle's filename.
LANE_SUFFIXES = (".native.pre-fix-0.9.0", ".native", ".from-mzml")


def decode_binary(bda) -> tuple[str, np.ndarray]:
    """One <binaryDataArray> -> (array accession, values as float64)."""
    acc = {c.get("accession") for c in bda if c.tag == NS + "cvParam"}
    raw = base64.b64decode((bda.find(NS + "binary").text or "").strip() or "")
    if ZLIB in acc:
        raw = zlib.decompress(raw)
    dtype = "<f8" if F64 in acc else "<f4"
    kind = MZ_ARRAY if MZ_ARRAY in acc else (INTENSITY_ARRAY if INTENSITY_ARRAY in acc else "")
    return kind, np.frombuffer(raw, dtype=dtype).astype(np.float64)


def stream_mzml(path: Path):
    """Yield (index, m/z, intensity, is_centroid) one spectrum at a time.

    Streaming is not an optimisation here: the vendor DIA exports are ~4.5 GB with ~280 M points,
    which no dict of decoded arrays survives. The archive side is held in memory instead — a
    centroid facet is orders of magnitude smaller.
    """
    for _, el in ET.iterparse(str(path), events=("end",)):
        if el.tag != NS + "spectrum":
            continue
        acc = {c.get("accession") for c in el.iter(NS + "cvParam")}
        mz = inten = None
        for bda in el.iter(NS + "binaryDataArray"):
            kind, vals = decode_binary(bda)
            if kind == MZ_ARRAY:
                mz = vals
            elif kind == INTENSITY_ARRAY:
                inten = vals
        if mz is not None and inten is not None:
            yield int(el.get("index")), mz, inten, (CENTROID in acc or PROFILE not in acc)
        el.clear()


def read_facet(archive: Path, member: str) -> dict[int, tuple[np.ndarray, np.ndarray]]:
    """One data facet -> {spectrum_index: (m/z, intensity)}, chunk or point layout."""
    with zipfile.ZipFile(archive) as z:
        if member not in z.namelist():
            return {}
        table = pq.read_table(io.BytesIO(z.read(member)))
    if table.num_rows == 0:
        return {}
    col = table.column_names[0]
    data = table.column(col).combine_chunks()
    si = data.field("spectrum_index").to_numpy(zero_copy_only=False)

    out: dict[int, list[tuple[np.ndarray, np.ndarray]]] = {}
    if col == "chunk":
        starts = data.field("mz_chunk_start").to_numpy(zero_copy_only=False)
        values = data.field("mz_chunk_values").to_pylist()
        encodings = data.field("chunk_encoding").to_pylist()
        intensities = data.field("intensity").to_pylist()
        for k in range(len(si)):
            if encodings[k] != DELTA_CHUNK:
                raise SystemExit(
                    f"{archive.name}: {member} uses chunk encoding {encodings[k]!r}; this tool only "
                    f"decodes {DELTA_CHUNK} (delta). Convert with --chunk-encoding delta to compare."
                )
            deltas = np.asarray(values[k], dtype=np.float64)
            mz = np.empty(len(deltas) + 1, dtype=np.float64)
            mz[0] = starts[k]
            mz[1:] = starts[k] + np.cumsum(deltas)
            out.setdefault(int(si[k]), []).append(
                (mz, np.asarray(intensities[k], dtype=np.float64))
            )
    else:  # point layout: one row per peak
        names = {f.name for f in data.type}
        int_all = data.field("intensity").to_numpy(zero_copy_only=False).astype(np.float64)
        if "tof_index" in names:
            # Shimadzu lattice centroids (v0.9.5+): `tof_index` Int64 = round(m/z * 1e9) on lattice
            # rows (`mz` null there), exact f64 `mz` on the rare fallback row (`tof_index` null).
            idx = data.field("tof_index")
            k = idx.to_numpy(zero_copy_only=False).astype(np.float64)
            mz_all = k * 1e-9  # the vendored reader's LinearMz: m/z = params[0] * k
            if "mz" in names:
                fb = data.field("mz")
                if fb.null_count < len(fb):
                    on_lattice = idx.is_valid().to_numpy(zero_copy_only=False)
                    fb_vals = fb.to_numpy(zero_copy_only=False)
                    mz_all = np.where(on_lattice, mz_all, fb_vals)
        else:
            mz_all = data.field("mz").to_numpy(zero_copy_only=False).astype(np.float64)
        # Rows are written grouped by spectrum: slice at the boundaries instead of one boolean mask
        # per spectrum (280 M rows x 21,500 spectra would never finish).
        bounds = np.flatnonzero(np.diff(si)) + 1
        starts = np.concatenate(([0], bounds))
        ends = np.concatenate((bounds, [len(si)]))
        for a, b in zip(starts, ends):
            out.setdefault(int(si[a]), []).append((mz_all[a:b], int_all[a:b]))
    return {
        s: (np.concatenate([p[0] for p in parts]), np.concatenate([p[1] for p in parts]))
        for s, parts in out.items()
    }


def find_rotation(stored: np.ndarray, oracle: np.ndarray, max_shift: int = 8) -> int | None:
    """Smallest s >= 1 with stored[s:] == oracle[:len(stored)-s], or None."""
    for s in range(1, max_shift + 1):
        if len(stored) > s and len(oracle) >= len(stored) - s:
            if np.array_equal(
                stored[s:].astype(np.float32), oracle[: len(stored) - s].astype(np.float32)
            ):
                return s
    return None


def guess_oracle(archive: Path) -> Path | None:
    stem = archive.name[: -len(archive.suffix)]
    for suffix in LANE_SUFFIXES:
        if stem.endswith(suffix):
            stem = stem[: -len(suffix)]
            break
    for candidate in (archive.with_name(stem + ".mzML"), archive.with_name(stem + ".mzml")):
        if candidate.exists():
            return candidate
    return None


class FacetStats:
    def __init__(self) -> None:
        self.compared = 0
        self.counts: Counter = Counter()
        self.rotations: Counter = Counter()
        self.worst_mz = 0.0
        self.failures: list[dict] = []

    def observe(self, index: int, a_mz, a_int, s_mz, s_int) -> None:
        self.compared += 1
        if len(a_mz) == len(s_mz):
            self.counts["equal"] += 1
        elif len(a_mz) == len(s_mz) - 1:
            self.counts["clipped_last"] += 1
        else:
            self.counts["other"] += 1
        n = min(len(a_mz), len(s_mz))
        if n:
            self.worst_mz = max(self.worst_mz, float(np.max(np.abs(a_mz[:n] - s_mz[:n]))))
        if len(a_int) == len(s_int) and np.array_equal(
            a_int.astype(np.float32), s_int.astype(np.float32)
        ):
            self.rotations[0] += 1
            return
        shift = find_rotation(a_int, s_int)
        self.rotations[shift if shift is not None else -1] += 1
        if len(self.failures) < 5:
            self.failures.append(
                {
                    "spectrum_index": index,
                    "stored_peaks": len(a_mz),
                    "oracle_peaks": len(s_mz),
                    "rotation": shift,
                    "stored_head": [float(v) for v in a_int[:4]],
                    "oracle_head": [float(v) for v in s_int[:4]],
                }
            )

    def summary(self, mz_tol: float) -> dict:
        return {
            "compared": self.compared,
            "counts": dict(self.counts),
            "intensity_bit_exact": self.rotations.get(0, 0),
            "rotations": {str(k): v for k, v in sorted(self.rotations.items()) if k != 0},
            "max_abs_mz_diff": self.worst_mz,
            "mz_within_tol": self.worst_mz <= mz_tol,
            "failures": self.failures,
            "ok": (
                self.compared > 0
                and self.rotations.get(0, 0) == self.compared
                and self.counts["equal"] == self.compared
                and self.worst_mz <= mz_tol
            ),
        }


def compare(archive: Path, oracle: Path, mz_tol: float, limit: int | None) -> dict:
    facets = {
        "spectra_peaks.parquet": read_facet(archive, "spectra_peaks.parquet"),
        "spectra_data.parquet": read_facet(archive, "spectra_data.parquet"),
    }
    stats = {member: FacetStats() for member in facets}

    for index, s_mz, s_int, is_centroid in stream_mzml(oracle):
        member = "spectra_peaks.parquet" if is_centroid else "spectra_data.parquet"
        stored = facets[member].get(index)
        if stored is None:
            continue
        if limit and stats[member].compared >= limit:
            if all(limit and st.compared >= limit for m, st in stats.items() if facets[m]):
                break
            continue
        stats[member].observe(index, stored[0], stored[1], s_mz, s_int)

    report: dict = {"archive": str(archive), "oracle": str(oracle), "facets": {}}
    for member, st in stats.items():
        if st.compared:
            report["facets"][member] = st.summary(mz_tol)
    return report


def metamorphic(archive: Path, baseline: Path, limit: int | None) -> dict:
    """Replay the known defect: for each spectrum, corrected[i] must equal corrupt[i+s]."""
    out: dict = {"archive": str(archive), "baseline": str(baseline), "facets": {}}
    for member in ("spectra_peaks.parquet", "spectra_data.parquet"):
        new, old = read_facet(archive, member), read_facet(baseline, member)
        if not new or not old:
            continue
        keys = sorted(set(new) & set(old))
        if limit:
            keys = keys[:limit]
        shifts = Counter()
        for k in keys:
            if np.array_equal(old[k][1].astype(np.float32), new[k][1].astype(np.float32)):
                shifts["identical"] += 1
            else:
                shifts[find_rotation(old[k][1], new[k][1])] += 1
        out["facets"][member] = {
            "compared": len(keys),
            "shift_histogram": {str(k): v for k, v in sorted(shifts.items(), key=lambda kv: str(kv[0]))},
        }
    return out


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("archive", type=Path)
    ap.add_argument("--oracle", type=Path, help="vendor mzML export (default: sibling .mzML)")
    ap.add_argument("--baseline", type=Path, help="preserved pre-fix archive, for metamorphic replay")
    ap.add_argument("--mz-tol", type=float, default=1e-4, help="absolute m/z tolerance (default 1e-4)")
    ap.add_argument("--max-spectra", type=int, default=None, help="stop after N spectra per facet")
    ap.add_argument("--json", type=Path, help="write the full report here")
    args = ap.parse_args()

    if not args.archive.exists():
        print(f"no such archive: {args.archive}", file=sys.stderr)
        return 2

    oracle = args.oracle or guess_oracle(args.archive)
    if oracle is None or not oracle.exists():
        print(
            f"NO ORACLE for {args.archive.name}: no vendor mzML export found next to it. "
            f"This file is UNVERIFIED — pass --oracle explicitly, or check it another way.",
            file=sys.stderr,
        )
        return 2

    report = compare(args.archive, oracle, args.mz_tol, args.max_spectra)
    if args.baseline:
        report["metamorphic"] = metamorphic(args.archive, args.baseline, args.max_spectra)
    if args.json:
        args.json.write_text(json.dumps(report, indent=2))

    ok = True
    for member, facet in report["facets"].items():
        verdict = "PASS" if facet["ok"] else "FAIL"
        ok &= facet["ok"]
        print(
            f"{verdict} {member}: {facet['compared']} spectra · counts {facet['counts']} · "
            f"bit-exact {facet['intensity_bit_exact']}/{facet['compared']} · "
            f"rotations {facet['rotations'] or '{}'} · max|dmz| {facet['max_abs_mz_diff']:.3e}"
        )
        for f in facet["failures"]:
            print(f"    spectrum {f['spectrum_index']}: {f['stored_peaks']} vs {f['oracle_peaks']} "
                  f"peaks, rotation {f['rotation']}, stored {f['stored_head']} vs oracle {f['oracle_head']}")
    if "metamorphic" in report:
        for member, facet in report["metamorphic"]["facets"].items():
            print(f"     replay {member}: shift histogram {facet['shift_histogram']}")
    if not report["facets"]:
        print(f"NO COMPARABLE FACET between {args.archive.name} and {oracle.name}", file=sys.stderr)
        return 2
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
