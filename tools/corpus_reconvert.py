#!/usr/bin/env python3
"""Reconvert every raw unit in the example-data corpus to a sibling `.mzpeak`.

One raw unit -> one `<stem>.mzpeak` next to it. Idempotent: a unit is skipped when its archive is
already current, so re-running converges on a complete corpus instead of redoing finished work.

"Current" is checked against the ARCHIVE, not a timestamp, because timestamps lie after a git
checkout or a copy:
  * it opens as a zip, and
  * it carries the split-facet marker (`spectra_metadata_scans.parquet`, v0.7.0+), and
  * its `.built` stamp records the same converter version we are about to run.

`--clean` deletes every `.mzpeak` (and stamp) first. It reaches the same end state as the default
idempotent pass, only slower, so prefer the default unless you specifically want a from-scratch run.

Units that cannot convert on this host (vendor SDKs that are Windows-only, or a missing msconvert)
are reported as SKIPPED, never counted as complete — completeness has to mean something.

Usage:
    tools/corpus_reconvert.py [ROOT] [--clean] [--jobs N] [--dry-run] [--report-only]
"""

from __future__ import annotations

import argparse
import concurrent.futures as cf
import os
import shutil
import subprocess
import sys
import time
import zipfile
from datetime import datetime, timezone
from pathlib import Path

# Container directories are units in their own right — never descend into them looking for more.
DIR_UNIT_SUFFIXES = {".d", ".raw"}
# Files that mark a vendor directory as holding actual acquisition data (vs. only method/config).
VENDOR_PAYLOAD_MARKERS = (
    "analysis.tdf", "analysis.tsf", "analysis.baf", "AcqData", "_FUNC001.DAT", "_extern.inf",
)
FILE_UNIT_SUFFIXES = {".mzml", ".imzml", ".raw", ".wiff", ".lcd", ".baf", ".tdf"}
FORMAT_MARKER = "spectra_metadata_scans.parquet"  # split-facet layout, v0.7.0+

# Host can't do these; the message is the converter's own, matched loosely.
UNSUPPORTED_MARKERS = (
    "available only on Windows",
    "available only on Windows/Linux",
    "msconvert not found",
)


def converter() -> str:
    """Resolve the converter the same way the corpus pipeline does, honouring $MZPEAK_CONVERT."""
    env = os.environ.get("MZPEAK_CONVERT")
    if env and Path(env).exists():
        return env
    found = shutil.which("mzpeak-convert")
    if found:
        return found
    local = Path(__file__).resolve().parent.parent / "target/release/mzpeak-convert"
    if local.exists():
        return str(local)
    sys.exit("mzpeak-convert not found: set $MZPEAK_CONVERT or build target/release")


def converter_version(binary: str) -> str:
    out = subprocess.run([binary, "--version"], capture_output=True, text=True)
    return (out.stdout or out.stderr).strip() or "unknown"


def is_vendor_dir_unit(path: Path) -> bool:
    """True when a `.d`/`.raw` DIRECTORY is itself the acquisition, not a wrapper around one.

    Some datasets ship `Foo_Try.d/Foo(1) Try_Slot1-1_1_8270.d/` — the outer directory just carries
    the name and holds the real acquisition inside. Converting the wrapper fails ("Is a directory"),
    so a directory only counts as a unit when it holds recognisable vendor content; otherwise we
    descend and find the real one.
    """
    markers = VENDOR_PAYLOAD_MARKERS
    try:
        names = {p.name for p in path.iterdir()}
    except OSError:
        return False
    if any(m in names for m in markers):
        return True
    # Thermo/Waters `.raw` directories and method-only dirs: accept when there is no nested unit.
    return not any(Path(n).suffix.lower() in DIR_UNIT_SUFFIXES for n in names)


def find_units(root: Path) -> list[Path]:
    """Every raw unit under `root`, without descending into unit or output directories."""
    units: list[Path] = []
    for dirpath, dirnames, filenames in os.walk(root):
        here = Path(dirpath)
        # `__MACOSX` holds AppleDouble resource forks from a zip, never real acquisitions.
        if "__MACOSX" in here.parts:
            dirnames[:] = []
            continue
        # Prune: never walk into a unit directory, an output archive, or VCS/scratch dirs.
        keep = []
        for d in dirnames:
            suffix = Path(d).suffix.lower()
            if suffix in DIR_UNIT_SUFFIXES and is_vendor_dir_unit(here / d):
                units.append(here / d)
            elif d.endswith(".mzpeak") or d in {".git", "validator_logs", "__pycache__", "__MACOSX"}:
                pass
            else:
                keep.append(d)  # includes wrapper `.d` dirs, so the real unit inside is found
        dirnames[:] = keep
        for f in filenames:
            p = here / f
            if p.suffix.lower() in FILE_UNIT_SUFFIXES:
                # A Thermo `.raw` FILE is a unit; a Waters `.raw` DIRECTORY was caught above.
                units.append(p)
    return sorted(set(units))


def target_for(unit: Path) -> Path:
    return unit.with_suffix(".mzpeak")


def stamp_for(archive: Path) -> Path:
    return archive.with_suffix(archive.suffix + ".built")


def is_current(archive: Path, version: str) -> bool:
    """True when `archive` was produced by `version` AND uses the split-facet layout."""
    if not archive.exists():
        return False
    try:
        with zipfile.ZipFile(archive) as z:
            if not any(n.endswith(FORMAT_MARKER) for n in z.namelist()):
                return False
    except Exception:
        return False  # unreadable/truncated -> rebuild
    stamp = stamp_for(archive)
    return stamp.exists() and stamp.read_text().strip() == version


def convert(unit: Path, binary: str, version: str, dry: bool) -> tuple[Path, str, str]:
    """-> (unit, status, detail). status in {converted, skipped, failed}."""
    out = target_for(unit)
    if dry:
        return unit, "would-convert", ""
    tmp = out.with_suffix(".mzpeak.partial")
    if tmp.exists():
        shutil.rmtree(tmp, ignore_errors=True) if tmp.is_dir() else tmp.unlink()
    # A vendor directory with no payload is an incomplete DOWNLOAD, not a conversion fault. Calling
    # it "failed" would hide a data-availability problem behind a converter error.
    if unit.is_dir() and not any((unit / m).exists() for m in VENDOR_PAYLOAD_MARKERS):
        return unit, "skipped", "vendor payload missing (incomplete download)"
    proc = subprocess.run(
        [binary, str(unit), "-o", str(out), "-f"],
        capture_output=True,
        text=True,
    )
    blob = (proc.stdout or "") + (proc.stderr or "")
    if proc.returncode != 0 or not out.exists():
        if any(m in blob for m in UNSUPPORTED_MARKERS):
            return unit, "skipped", "not convertible on this host (vendor SDK / msconvert)"
        first = next(
            (ln for ln in blob.splitlines() if ln.lower().startswith("error")),
            f"exit {proc.returncode}",
        )
        return unit, "failed", first.strip()[:200]
    stamp_for(out).write_text(version + "\n")
    return unit, "converted", ""


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("root", nargs="?", default=os.path.expanduser("~/Claude/mzpeak-example-data/data"))
    ap.add_argument("--clean", action="store_true", help="delete every .mzpeak first, then convert all")
    ap.add_argument("--jobs", type=int, default=max(1, (os.cpu_count() or 4) // 2))
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--report-only", action="store_true", help="audit completeness, convert nothing")
    args = ap.parse_args()

    root = Path(args.root).expanduser()
    if not root.is_dir():
        sys.exit(f"not a directory: {root}")
    binary = converter()
    version = converter_version(binary)
    print(f"converter : {binary}\n            {version}")
    print(f"root      : {root}")

    units = find_units(root)
    print(f"raw units : {len(units)}")

    if args.clean and not (args.dry_run or args.report_only):
        removed = 0
        for p in list(root.rglob("*.mzpeak")) + list(root.rglob("*.mzpeak.built")):
            shutil.rmtree(p, ignore_errors=True) if p.is_dir() else p.unlink(missing_ok=True)
            removed += 1
        print(f"cleaned   : {removed} existing archives/stamps removed")

    todo = [u for u in units if not is_current(target_for(u), version)]
    fresh = len(units) - len(todo)
    print(f"already ok: {fresh}\nto convert: {len(todo)}\n")

    results: dict[str, list] = {"converted": [], "skipped": [], "failed": [], "would-convert": []}
    for u in units:
        if u not in todo:
            results["converted"].append((u, ""))

    if not args.report_only and todo:
        started = time.time()
        with cf.ThreadPoolExecutor(max_workers=args.jobs) as pool:
            futs = {pool.submit(convert, u, binary, version, args.dry_run): u for u in todo}
            for i, fut in enumerate(cf.as_completed(futs), 1):
                unit, status, detail = fut.result()
                results[status].append((unit, detail))
                mark = {"converted": "ok", "skipped": "--", "failed": "FAIL", "would-convert": "..."}[status]
                print(f"  [{i}/{len(todo)}] {mark:4} {unit.relative_to(root)}"
                      + (f"  ({detail})" if detail else ""), flush=True)
        print(f"\nelapsed   : {time.time() - started:.0f}s")

    # ---- report -------------------------------------------------------------
    have = [u for u in units if is_current(target_for(u), version)]
    print("\n" + "=" * 72)
    print(f"COMPLETENESS  {len(have)}/{len(units)} raw units have a current .mzpeak"
          f"  ({100.0 * len(have) / max(1, len(units)):.1f}%)")
    for key, label in (("skipped", "SKIPPED (host cannot convert)"), ("failed", "FAILED")):
        rows = results[key]
        if rows:
            print(f"\n{label}: {len(rows)}")
            for u, d in sorted(rows)[:40]:
                print(f"  - {u.relative_to(root)}" + (f"\n      {d}" if d else ""))

    stale = []
    for a in sorted(root.rglob("*.mzpeak")):
        try:
            names = zipfile.ZipFile(a).namelist()
        except Exception:
            stale.append((a, "unreadable")); continue
        if not any(n.endswith(FORMAT_MARKER) for n in names):
            stale.append((a, "pre-0.7.0 packed layout — unreadable by this build"))
    if stale:
        print(f"\nSTALE ON DISK: {len(stale)} archive(s) left from an earlier format")
        for a, why in stale[:40]:
            print(f"  - {a.relative_to(root)}\n      {why}")

    if have:
        # "Last update" of the SET is governed by its oldest member: the set is only as fresh as
        # the staleast archive in it.
        pairs = [(target_for(u).stat().st_mtime, target_for(u)) for u in have]
        oldest_ts, oldest_p = min(pairs)
        newest_ts, _ = max(pairs)
        fmt = lambda t: datetime.fromtimestamp(t, timezone.utc).astimezone().strftime("%Y-%m-%d %H:%M:%S %Z")
        print(f"\nLAST UPDATE   {fmt(oldest_ts)}   (oldest archive in the set)")
        print(f"              {oldest_p.relative_to(root)}")
        print(f"newest        {fmt(newest_ts)}")
    print("=" * 72)
    return 1 if results["failed"] else 0


if __name__ == "__main__":
    sys.exit(main())
