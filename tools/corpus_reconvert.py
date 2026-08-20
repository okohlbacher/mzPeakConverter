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
    # NON-EMPTY: a wrapper can carry a zero-byte `analysis.tdf` stub left by a partial download or
    # archive extraction, which made the wrapper look like an acquisition and fail every run. The
    # converter's own format probes were fixed the same way in v0.7.3.
    if any((path / m).is_file() and (path / m).stat().st_size > 0 for m in markers if m in names):
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
                # Zero-byte files are never acquisitions — corpora carry stubs left by partial
                # downloads (a 0-byte `analysis.tdf` inside a wrapper `.d` was counted as a unit and
                # failed every run).
                try:
                    if p.stat().st_size == 0:
                        continue
                except OSError:
                    continue
                units.append(p)
    return sorted(set(units))


def target_for(unit: Path) -> Path:
    return unit.with_suffix(".mzpeak")


# ── descriptor awareness ────────────────────────────────────────────────────────────────────────
# The corpus is DESCRIBED by `data/<tile>/<id>/<id>.yaml`: each descriptor names the one unit it
# publishes (`convert.input`) and the recipe to build it with (`convert.flags`). Walking the tree for
# raw units alone gets both wrong:
#   * a multi-run deposit (PXD018751 ships 122 runs in one archive) yields 122 targets where the
#     corpus publishes ONE representative, so a full pass recreates 100+ unpublished archives; and
#   * flags are lost — an SDRF demonstrator built with `--sdrf` gets silently rebuilt WITHOUT its
#     embedded `sample_metadata/sdrf.tsv`, i.e. the run loses its sample metadata.
# So: honour the descriptor where there is one. Tiles in MULTI_UNIT_TILES keep every-unit behaviour
# because they are deliberately multi-file (the ProteoWizard reader-regression corpus).
MULTI_UNIT_TILES = {"pwiz-examples"}


def load_recipes(root: Path) -> tuple[dict[Path, list[str]], dict[Path, Path], set[Path], set[Path]]:
    """-> (extra flags by unit, pinned unit by dataset dir, skipped dataset dirs, all governed dirs).

    Missing PyYAML is not fatal: without it we cannot read descriptors, so the caller falls back to
    the every-unit walk rather than silently publishing the wrong set.
    """
    try:
        import yaml  # noqa: PLC0415
    except ImportError:
        print("warn      : PyYAML unavailable — descriptors not read, falling back to every-unit walk")
        return {}, {}, set(), set()
    import shlex  # noqa: PLC0415
    flags: dict[Path, list[str]] = {}
    pinned: dict[Path, Path] = {}
    skipped: set[Path] = set()
    governed: set[Path] = set()
    for desc in sorted(root.glob("*/*/*.yaml")):
        if desc.name in {"_tile.yaml", "TEMPLATE.yaml"}:
            continue
        if desc.parent.parent.name in MULTI_UNIT_TILES:
            continue
        try:
            doc = yaml.safe_load(desc.read_text()) or {}
        except Exception:
            continue
        cv, dd = (doc.get("convert") or {}), desc.parent
        governed.add(dd)
        if cv.get("skip"):
            skipped.add(dd)
            continue
        spec = cv.get("input")
        if spec and spec != "auto":
            unit = dd / spec
            pinned[dd] = unit
            if cv.get("flags"):
                flags[unit] = shlex.split(str(cv["flags"]))
    return flags, pinned, skipped, governed


def apply_recipes(units: list[Path], pinned: dict[Path, Path], skipped: set[Path],
                  governed: set[Path]) -> list[Path]:
    """Narrow the walked units to what the descriptors actually publish.

    For a dataset whose descriptor pins `convert.input`, keep only that unit (or units inside it,
    for a vendor directory). A dataset with no pin is untouched, so auto-detection still applies —
    this only ever narrows where the corpus has stated explicitly what it publishes.
    """
    kept: list[Path] = []
    unpinned: dict[Path, list[Path]] = {}
    for u in units:
        ds = next((d for d in (*pinned, *skipped, *governed) if d in u.parents), None)
        if ds is None:
            kept.append(u)                      # outside a described dataset -> unchanged behaviour
        elif ds in skipped:
            continue                            # convert.skip -> not ours to build
        elif ds in pinned:
            pin = pinned[ds]
            if u == pin or pin in u.parents:
                kept.append(u)                  # the described unit (or a unit inside a vendor dir)
        else:
            unpinned.setdefault(ds, []).append(u)
    # A described dataset with no explicit `convert.input` still publishes ONE archive: a multi-run
    # deposit (PXD018751: 122 runs in one zip) must not yield 122 unpublished archives. Pick one
    # representative deterministically — pin `convert.input` in the descriptor to choose a different
    # one. Single-unit datasets are unaffected.
    for ds, us in sorted(unpinned.items()):
        us = sorted(us)
        kept.append(us[0])
        if len(us) > 1:
            print(f"note      : {ds.name} has {len(us)} units and no convert.input — building only "
                  f"{us[0].name} (one per described set)")
    return kept


# Vendor-native formats first: several datasets ship the SAME acquisition as both a vendor raw and a
# converted mzML sharing one stem, so both units resolve to one `.mzpeak`. Converting both is
# meaningless and, run concurrently, they race on the output path. Pick one per target — the native
# format, which carries more (vendor trailers, true profile/centroid state) — and fall back to the
# next candidate only if the preferred one cannot be converted on this host.
_FORMAT_RANK = {".d": 0, ".raw": 1, ".wiff": 2, ".lcd": 3, ".baf": 4, ".tdf": 5, ".imzml": 6, ".mzml": 7}


def preference(unit: Path) -> tuple[int, str]:
    return (_FORMAT_RANK.get(unit.suffix.lower(), 99), unit.name)


def group_by_target(units: list[Path]) -> dict[Path, list[Path]]:
    """Map each output archive to its candidate units, best-first."""
    groups: dict[Path, list[Path]] = {}
    for u in units:
        groups.setdefault(target_for(u), []).append(u)
    for t in groups:
        groups[t].sort(key=preference)
    return groups


def stamp_for(archive: Path) -> Path:
    return archive.with_suffix(archive.suffix + ".built")


# Releases that produce BYTE-IDENTICAL archives, newest first within each group. The stamp records
# which converter built an archive, and currency was an exact string match against the installed
# one -- so a release that changed only tooling invalidated the entire corpus and demanded a
# multi-hour rebuild to reproduce the same bytes. Only add a pair here when the release genuinely
# cannot change output (e.g. 0.7.7 touched nothing but this harness); when in doubt, leave it out
# and let the corpus rebuild.
OUTPUT_COMPATIBLE: list[set[str]] = [
    # 0.7.7 changed only this harness. 0.7.8 changed only src/shimadzu.rs and glue/shimadzu/Glue.cs,
    # both reachable solely through the #[cfg(windows)] native `.lcd` lane -- which errored on every
    # file before 0.7.8, so no archive in existence was produced by it. Every other lane is untouched.
    {"mzpeak-convert 0.7.6", "mzpeak-convert 0.7.7", "mzpeak-convert 0.7.8"},
]


def compatible_versions(version: str) -> set[str]:
    """`version` plus any release known to produce identical output."""
    out = {version}
    for group in OUTPUT_COMPATIBLE:
        if version in group:
            out |= group
    return out


def is_current(archive: Path, version: str) -> bool:
    """True when `archive` was produced by `version` (or an output-identical release) AND uses the
    split-facet layout."""
    if not archive.exists():
        return False
    try:
        with zipfile.ZipFile(archive) as z:
            if not any(n.endswith(FORMAT_MARKER) for n in z.namelist()):
                return False
    except Exception:
        return False  # unreadable/truncated -> rebuild
    stamp = stamp_for(archive)
    return stamp.exists() and stamp.read_text().strip() in compatible_versions(version)


def convert(unit: Path, binary: str, version: str, dry: bool,
            extra: list[str] | None = None) -> tuple[Path, str, str]:
    """-> (unit, status, detail). status in {converted, skipped, failed}.

    `extra` carries the descriptor's `convert.flags` (e.g. `--sdrf study.sdrf.tsv`, `--zstd-level 12`)
    so a rebuild reproduces the published archive instead of a bare default conversion.
    """
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
        [binary, str(unit), "-o", str(out), "-f", *(extra or [])],
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


TOOLS = Path(__file__).resolve().parent


def box_ssh(command: str, timeout: int = 3600) -> str:
    """Run a PowerShell command on the flash workstation through the jump host."""
    env = {}
    envfile = TOOLS / "box.env"
    if envfile.exists():
        for line in envfile.read_text().splitlines():
            if "=" in line and not line.strip().startswith("#"):
                k, _, v = line.partition("=")
                env[k.strip()] = v.strip().strip('"').strip("'")
    for k in ("BOX_SSH", "BOX_JUMP", "BOX_SSH_KEY"):
        env.setdefault(k, os.environ.get(k, ""))
        if not env[k]:
            sys.exit(f"box: {k} not set (env or tools/box.env)")
    proxy = (f"ProxyCommand=ssh -i {env['BOX_SSH_KEY']} -o IdentitiesOnly=yes "
             f"-o StrictHostKeyChecking=accept-new -W %h:%p {env['BOX_JUMP']}")
    proc = subprocess.run(
        ["ssh", "-i", env["BOX_SSH_KEY"], "-o", "IdentitiesOnly=yes",
         "-o", "StrictHostKeyChecking=accept-new", "-o", proxy,
         "-o", "ConnectTimeout=30", "-o", "ServerAliveInterval=30",
         env["BOX_SSH"], f"powershell -NoProfile -Command \"{command}\""],
        capture_output=True, text=True, timeout=timeout,
    )
    return (proc.stdout or "").replace("\r", "").strip()


def sync_box(version: str, repo: str = r"C:\Users\User\src\mzPeakConverter") -> bool:
    """Ensure the box's converter matches `version`, fast-forwarding and rebuilding if not.

    Without this the box silently converts with whatever binary it last built — the same stale-binary
    trap that PATH resolution creates on the host. Per the box-follows-repo rule we only ever check
    out a canonical tag; nothing is committed on the box.
    """
    want = version.split()[-1]  # "mzpeak-convert 0.7.0" -> "0.7.0"
    have = box_ssh(f"cd {repo}; (& .\\target\\release\\mzpeak-convert.exe --version)")
    print(f"box       : has {have or '(no binary)'}, want {version}")
    if have.split()[-1:] == [want]:
        return True
    dirty = box_ssh(f"cd {repo}; ((git status --porcelain) | Measure-Object -Line).Lines")
    if dirty.strip() not in ("0", ""):
        print(f"box       : REFUSING to update — working tree has {dirty} modified file(s)")
        return False
    print(f"box       : updating to v{want} and rebuilding (several minutes)...")
    box_ssh(f"cd {repo}; git fetch origin --tags 2>&1 | Select-Object -Last 1; "
            f"git checkout --detach v{want} 2>&1 | Select-Object -Last 1", timeout=900)
    box_ssh(f"cd {repo}; cargo build --release 2>&1 | Select-Object -Last 2", timeout=7200)
    have = box_ssh(f"cd {repo}; (& .\\target\\release\\mzpeak-convert.exe --version)")
    ok = have.split()[-1:] == [want]
    print(f"box       : now {have} — {'ok' if ok else 'MISMATCH, aborting box phase'}")
    return ok


def run_box(units: list[Path], root: Path, version: str, jobs: int,
            recipes: dict[Path, list[str]] | None = None) -> None:
    """Convert host-unsupported units on the box, relaying the archives back.

    Delegates the transfer to tools/box_convert.sh --local-manifest, which already stages the raw
    through S3, converts in an isolated temp dir on the box, and pulls the .mzpeak back with a
    size+md5 check. Reimplementing that here would fork a second, less-tested transfer path.
    """
    if not units:
        print("box       : nothing to do")
        return
    if not sync_box(version):
        print("box       : skipped (converter could not be brought up to date)")
        return
    manifest = root.parent / "validator_logs" / "box-jobs.tsv"
    manifest.parent.mkdir(parents=True, exist_ok=True)
    with manifest.open("w") as fh:
        for u in units:
            # The descriptor's own flags, so a box-built archive matches its host-built recipe
            # (an SDRF demonstrator keeps `--sdrf`); `--no-vendor` only where none are described.
            flags = (recipes or {}).get(u) or ['--no-vendor']
            fh.write(f"{u}\t{target_for(u)}\t{' '.join(flags)}\n")
    print(f"box       : {len(units)} unit(s) -> {manifest}")
    # box_convert.sh shells out to `python3` for the S3 relay, which needs boto3. The system python3
    # doesn't have it; the anaconda one does. Prepend it rather than patching the shared script.
    env = dict(os.environ)
    for cand in (Path.home() / "anaconda3/bin", Path.home() / "miniconda3/bin"):
        if (cand / "python3").exists():
            env["PATH"] = f"{cand}:{env.get('PATH', '')}"
            break
    proc = subprocess.run(
        ["bash", str(TOOLS / "box_convert.sh"), "--local-manifest", str(manifest), "--jobs", str(jobs)],
        text=True, env=env,
    )
    print(f"box       : box_convert exited {proc.returncode}")
    for u in units:  # stamp whatever came back so the next run sees it as current
        out = target_for(u)
        if out.exists():
            try:
                with zipfile.ZipFile(out) as z:
                    if any(n.endswith(FORMAT_MARKER) for n in z.namelist()):
                        stamp_for(out).write_text(version + "\n")
            except Exception:
                pass


def convert_target(cands: list[Path], binary: str, version: str, dry: bool, recipes: dict | None = None) -> tuple[Path, str, str]:
    """Convert the best candidate for one output archive; fall back on the next if it cannot run.

    Only a `skipped` outcome falls through — a genuine `failed` is reported as-is rather than being
    masked by silently converting a lesser source.
    """
    last = None
    for i, u in enumerate(cands):
        unit, status, detail = convert(u, binary, version, dry, (recipes or {}).get(u))
        if status != "skipped":
            if i:
                detail = (detail + " " if detail else "") + f"(fallback from {cands[0].name})"
            return unit, status, detail
        last = (unit, status, detail)
    return last


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("root", nargs="?", default=os.path.expanduser("~/Claude/mzpeak-example-data/data"))
    ap.add_argument("--clean", action="store_true", help="delete every .mzpeak first, then convert all")
    ap.add_argument("--jobs", type=int, default=max(1, (os.cpu_count() or 4) // 2))
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--report-only", action="store_true", help="audit completeness, convert nothing")
    ap.add_argument("--box", action="store_true",
                    help="also convert host-unsupported vendor units on the flash workstation")
    ap.add_argument("--box-jobs", type=int, default=1, help="box concurrency (disk-bound; default 1)")
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

    # Honour the descriptors: build what the corpus PUBLISHES, with the recipe it publishes it under.
    recipe_flags, pinned, desc_skipped, governed = load_recipes(root)
    if governed:
        before = len(units)
        units = apply_recipes(units, pinned, desc_skipped, governed)
        dropped = before - len(units)
        print(f"descriptors: {len(pinned)} pinned, {len(desc_skipped)} skipped"
              + (f" -> {dropped} undescribed unit(s) not built" if dropped else "")
              + (f"; {len(recipe_flags)} carry convert.flags" if recipe_flags else ""))

    if args.clean and not (args.dry_run or args.report_only):
        removed = 0
        for p in list(root.rglob("*.mzpeak")) + list(root.rglob("*.mzpeak.built")):
            shutil.rmtree(p, ignore_errors=True) if p.is_dir() else p.unlink(missing_ok=True)
            removed += 1
        print(f"cleaned   : {removed} existing archives/stamps removed")

    groups = group_by_target(units)
    dup = {t: c for t, c in groups.items() if len(c) > 1}
    if dup:
        print(f"note      : {len(dup)} target(s) have several source formats; converting the "
              f"native one and skipping the duplicate(s)")
        for t, c in dup.items():
            print(f"            {t.name}  <- {', '.join(x.name for x in c)}")
    todo = [t for t in groups if not is_current(t, version)]
    fresh = len(groups) - len(todo)
    print(f"archives  : {len(groups)} (from {len(units)} units)\nalready ok: {fresh}\nto convert: {len(todo)}\n")

    results: dict[str, list] = {"converted": [], "skipped": [], "failed": [], "would-convert": []}
    for t in groups:
        if t not in todo:
            results["converted"].append((groups[t][0], ""))

    if not args.report_only and todo:
        started = time.time()
        with cf.ThreadPoolExecutor(max_workers=args.jobs) as pool:
            futs = {pool.submit(convert_target, groups[t], binary, version, args.dry_run, recipe_flags): t for t in todo}
            for i, fut in enumerate(cf.as_completed(futs), 1):
                unit, status, detail = fut.result()
                results[status].append((unit, detail))
                mark = {"converted": "ok", "skipped": "--", "failed": "FAIL", "would-convert": "..."}[status]
                print(f"  [{i}/{len(todo)}] {mark:4} {unit.relative_to(root)}"
                      + (f"  ({detail})" if detail else ""), flush=True)
        print(f"\nelapsed   : {time.time() - started:.0f}s")

    # ---- box phase ----------------------------------------------------------
    # Units the host cannot convert (Windows-only vendor SDKs, missing msconvert) go to the flash
    # workstation. Payload-missing units are excluded: no binary can convert data that isn't there.
    if args.box and not (args.report_only or args.dry_run):
        print()
        deferred = [u for u, d in results["skipped"] if "payload missing" not in d]
        run_box(deferred, root, version, args.box_jobs, recipe_flags)

    # ---- report -------------------------------------------------------------
    have = [t for t in groups if is_current(t, version)]
    print("\n" + "=" * 72)
    print(f"COMPLETENESS  {len(have)}/{len(groups)} archives current"
          f"  ({100.0 * len(have) / max(1, len(groups)):.1f}%)   [from {len(units)} raw units]")
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
        pairs = [(t.stat().st_mtime, t) for t in have]
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
