#!/usr/bin/env python3
"""Reconvert every raw unit in the example-data corpus to a sibling `.mzpeak`.

One raw unit -> one `<stem>.mzpeak` next to it. Idempotent: a unit is skipped when its archive is
already current, so re-running converges on a complete corpus instead of redoing finished work.

"Current" is checked against the ARCHIVE, not a timestamp, because timestamps lie after a git
checkout or a copy:
  * it opens as a zip, and
  * it carries the split-facet marker (`spectra_metadata_scans.parquet`, v0.7.0+), and
  * its `.built` stamp records the same converter version we are about to run, and
  * the same recipe: a hash of the descriptor's whole `convert` block, so editing `convert.flags`
    (or any other `convert.*` key) rebuilds the archive without waiting for a converter release.

The `.built` stamp is read out of the archive it describes, never written from the request:
    mzpeak-convert <version>     the archive's own software_list entry; another version is refused
    recipe <hash>                the descriptor recipe it was built under
    options <argv>               the archive's own `conversion options`, i.e. what actually ran
The box strips lane flags and may fall back to msconvert, so only the archive knows what built it.
A stamp from before the recipe line counts as stale.

`--clean` deletes every `.mzpeak` (and stamp) first. It reaches the same end state as the default
idempotent pass, only slower, so prefer the default unless you specifically want a from-scratch run.

Units that cannot convert on this host (vendor SDKs that are Windows-only, or a missing msconvert)
are reported as SKIPPED, never counted as complete — completeness has to mean something. With
`--box` they convert on the flash workstation and come back to the host beside their raw, through a
transient S3 relay slot that is verified and deleted; a unit that does not come back fails the run.
`--publish-s3` instead copies each box archive onto its durable corpus key (s3://v09/...) before any
validator has seen it, so it is opt-in.

An archive is ONE run, so a multi-sample SciEX `.wiff` publishes the samples its descriptor lists,
    convert: {input: En_PPY.wiff, samples: [1, 2]}
each as `<stem>.sample<N>.mzpeak`, converted with `--sample N`. The unit's former single archive is
reported as SUPERSEDED ON DISK, failing the run, until it is removed.

Usage:
    tools/corpus_reconvert.py [ROOT] [--clean] [--jobs N] [--dry-run] [--report-only]
                              [--box [--box-jobs N] [--publish-s3]]

Release day, once the tag is pushed and `mzpeak-convert --version` names it:
    tools/corpus_reconvert.py --box
then validate the archives on the host, and only then publish them from the corpus repository
(scripts/update.sh). `--no-s3-first`, which older notes pass, is still accepted: it is the default.
"""

from __future__ import annotations

import argparse
import concurrent.futures as cf
import hashlib
import json
import os
import shutil
import subprocess
import sys
import time
import zipfile
from datetime import datetime, timezone
from pathlib import Path
from typing import NamedTuple

# Container directories are units in their own right — never descend into them looking for more.
DIR_UNIT_SUFFIXES = {".d", ".raw"}
# Files that mark a vendor directory as holding actual acquisition data (vs. only method/config).
VENDOR_PAYLOAD_MARKERS = (
    "analysis.tdf", "analysis.tsf", "analysis.baf", "AcqData", "_FUNC001.DAT", "_extern.inf",
)
FILE_UNIT_SUFFIXES = {".mzml", ".imzml", ".raw", ".wiff", ".lcd", ".baf", ".tdf"}
# A vendor DIRECTORY that only exists as its download zip (`X.raw.zip`, `X.d.zip`) is a unit too:
# the box extracts it (box_convert_remote.ps1 sniffs .zip) and the corpus publishes `X.mzpeak` for
# it. Without this the walk could not see two Waters units at all, so the completeness line said
# "199/199 (100%)" while 201 archives sat on disk and those two stayed frozen at an old converter.
ZIPPED_UNIT_SUFFIXES = tuple(f"{d}.zip" for d in DIR_UNIT_SUFFIXES)
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
    # target/release BEFORE $PATH: this repo's own build is the authority on "current". A stale
    # ~/.cargo/bin/mzpeak-convert shadows it otherwise (seen in the wild: PATH 0.7.10 masking a
    # target/release 0.8.0), which silently pins the corpus to an old converter — and would make a
    # version-sync driven off this function DOWNGRADE the box. (The older shell harnesses that
    # resolved in this same order were removed in 0.9.13; this is the one resolver left.)
    local = Path(__file__).resolve().parent.parent / "target/release/mzpeak-convert"
    if local.exists():
        return str(local)
    found = shutil.which("mzpeak-convert")
    if found:
        return found
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
            if is_zipped_unit(p):
                units.append(p)
                continue
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


def is_zipped_unit(p: Path) -> bool:
    """True for `X.raw.zip` / `X.d.zip` when `X.raw` / `X.d` is NOT extracted beside it.

    When the directory has been extracted, the zip is its packaging, not a second acquisition: the
    directory is the unit (native readers want the directory) and the zip stays invisible, exactly
    as before. Only a zip with no extracted sibling is a unit in its own right.
    """
    name = p.name.lower()
    if not name.endswith(ZIPPED_UNIT_SUFFIXES):
        return False
    return not p.with_suffix("").exists()


def target_for(unit: Path, sample: int | None = None) -> Path:
    """`unit`'s archive; with `sample`, that sample's own `<stem>.sample<N>.mzpeak`."""
    if is_zipped_unit(unit):
        out = unit.with_suffix("").with_suffix(".mzpeak")   # X.raw.zip -> X.mzpeak
    else:
        out = unit.with_suffix(".mzpeak")
    return out if sample is None else out.with_name(f"{out.stem}.sample{sample}.mzpeak")


def sample_flags(sample: int | None) -> list[str]:
    return [] if sample is None else ["--sample", str(sample)]


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


class Recipe(NamedTuple):
    flags: list[str]  # convert.flags, path-valued arguments absolutised
    rid: str          # recipe_id of the descriptor's `convert` block: what a .built stamp must name
    samples: tuple[int, ...] = ()   # convert.samples: one archive per listed sample of a multi-sample WIFF


def recipe_id(cv: dict) -> str:
    """Hash of a descriptor's whole `convert` block. The `.built` stamp names the recipe its archive
    was built under, so editing `convert.flags` (or any `convert.*` key) makes the archive stale, as
    the corpus repository's `.sig` does, instead of leaving it "current" until the next release."""
    return hashlib.sha256(json.dumps(cv, sort_keys=True, default=str).encode()).hexdigest()[:16]


UNDESCRIBED = Recipe([], recipe_id({}))


def load_recipes(root: Path) -> tuple[dict[Path, Recipe], dict[Path, Path], set[Path], set[Path]]:
    """-> (recipe by dataset dir, pinned unit by dataset dir, skipped dataset dirs, all governed dirs).

    Missing PyYAML is not fatal: without it we cannot read descriptors, so the caller falls back to
    the every-unit walk rather than silently publishing the wrong set.
    """
    try:
        import yaml  # noqa: PLC0415
    except ImportError:
        print("warn      : PyYAML unavailable — descriptors not read, falling back to every-unit walk")
        return {}, {}, set(), set()
    import shlex  # noqa: PLC0415
    recipes: dict[Path, Recipe] = {}
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
        samples = cv.get("samples") or []
        if not isinstance(samples, list) or not all(type(n) is int and n >= 1 for n in samples):
            print(f"warn      : {desc.relative_to(root)}: convert.samples must list sample numbers >= 1"
                  f" -- dataset not built")
            skipped.add(dd)
            continue
        spec = cv.get("input")
        if spec and spec != "auto":
            pinned[dd] = dd / spec
        # Flags belong to the DATASET, whatever picks its unit. Recorded only beside a pinned
        # `convert.input`, every `input: auto` or input-less descriptor was built bare: three imzML
        # demonstrators were published without their `--image`, eleven archives without their
        # `--zstd-level 12`, and four lane pins never reached the box.
        recipes[dd] = Recipe(resolve_flag_paths(shlex.split(str(cv.get("flags") or "")), dd, root.parent, root),
                             recipe_id(cv), tuple(sorted(set(samples))))
    return recipes, pinned, skipped, governed


def recipe_for(unit: Path, recipes: dict[Path, Recipe]) -> Recipe:
    """The recipe of the described dataset that holds `unit`, looked up through its parents (a pinned
    vendor directory's inner unit and an `auto` pick belong to the same dataset)."""
    return next((recipes[d] for d in unit.parents if d in recipes), UNDESCRIBED)


# Flags that name a FILE. A descriptor writes them relative to its own directory
# (`--sdrf PXD020187.sdrf.tsv` sits beside the descriptor, while the input is `mzml/D2_Nat_2.mzML`),
# but the converter resolves a relative path against ITS cwd, not the descriptor's. Every SDRF
# demonstrator therefore failed with "opening SDRF <name>: No such file or directory" even though
# the file was present. Absolutise here, where the descriptor's directory is still known.
PATH_FLAGS = {"--sdrf", "--image"}


def resolve_flag_paths(tokens: list[str], *bases: Path) -> list[str]:
    """Absolutise path-valued flags against the first `bases` entry that actually has the file.

    Descriptors are inconsistent about what a relative path is relative to, and both spellings are
    in use: sdrf-examples writes `--sdrf PXD020187.sdrf.tsv` (beside the descriptor), while
    zenodo-DESI writes `--image "data/imzml-examples/.../90,100,110,120.jpg"` (from the CORPUS
    ROOT'S PARENT, i.e. including the `data/` component). Try each base rather than pick one.
    """
    out, expect_path = [], False
    for tok in tokens:
        if expect_path and not tok.startswith("-"):
            hit = next((b / tok for b in bases if (b / tok).exists()), None)
            out.append(str(hit.resolve()) if hit else tok)
            expect_path = False
            continue
        expect_path = tok in PATH_FLAGS
        out.append(tok)
    return out


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


def group_by_target(units: list[Path],
                    recipes: dict[Path, Recipe] | None = None) -> dict[Path, list[tuple[Path, int | None]]]:
    """Map each output archive to its candidate (unit, sample), best unit first. A unit whose
    descriptor lists `convert.samples` yields one archive per sample instead of one for the unit."""
    groups: dict[Path, list[tuple[Path, int | None]]] = {}
    for u in units:
        for n in recipe_for(u, recipes or {}).samples or (None,):
            groups.setdefault(target_for(u, n), []).append((u, n))
    for t in groups:
        groups[t].sort(key=lambda c: preference(c[0]))
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


def is_current(archive: Path, version: str, rid: str) -> bool:
    """True when `archive` was produced by `version` (or an output-identical release) under recipe
    `rid` AND uses the split-facet layout. A stamp with no recipe line predates recipes: stale."""
    if not archive.exists():
        return False
    try:
        with zipfile.ZipFile(archive) as z:
            if not any(n.endswith(FORMAT_MARKER) for n in z.namelist()):
                return False
    except Exception:
        return False  # unreadable/truncated -> rebuild
    stamp = stamp_for(archive)
    lines = stamp.read_text().splitlines() if stamp.exists() else []
    return bool(lines) and lines[0].strip() in compatible_versions(version) and f"recipe {rid}" in lines[1:]


def write_stamp(archive: Path, version: str, rid: str) -> str | None:
    """Stamp `archive` from its OWN index; -> None, or why it was left unstamped.

    What was requested says nothing reliable about what built an archive: the box strips lane flags
    and may fall back to msconvert, and `BOX_AUTOUPDATE=0` skips its version check, after which the
    host's version string used to be written beside whatever the box's exe produced. The archive
    records its converter (software_list) and its argv (`conversion options`), so the stamp copies
    those, and an archive another converter version built is refused rather than labelled current.
    """
    try:
        with zipfile.ZipFile(archive) as z:
            if not any(n.endswith(FORMAT_MARKER) for n in z.namelist()):
                return "no split-facet layout"
            md = json.loads(z.read("mzpeak_index.json")).get("metadata") or {}
    except Exception as e:  # truncated zip, no index, unparsable index
        return f"unreadable archive index ({e})"
    built = next((s.get("version") for s in md.get("software_list") or []
                  if s.get("id") == "mzpeak-convert"), None)
    options = next((p.get("value") for dp in md.get("data_processing_method_list") or []
                    for m in dp.get("methods") or [] for p in m.get("parameters") or []
                    if p.get("name") == "conversion options"), None) or ""
    if built != version.split()[-1]:
        return f"built by mzpeak-convert {built or '<unrecorded>'}, not {version}"
    stamp_for(archive).write_text(f"{version}\nrecipe {rid}\noptions {options}\n")
    return None


def convert(unit: Path, out: Path, binary: str, version: str, dry: bool,
            extra: list[str] | None = None, rid: str = UNDESCRIBED.rid) -> tuple[Path, str, str]:
    """Convert `unit` into `out` (its archive, or one sample's). -> (unit, status, detail), with
    status in {converted, skipped, failed}.

    `extra` carries the descriptor's `convert.flags` (e.g. `--sdrf study.sdrf.tsv`, `--zstd-level 12`)
    so a rebuild reproduces the published archive instead of a bare default conversion.
    """
    if dry:
        return unit, "would-convert", ""
    tmp = out.with_suffix(".mzpeak.partial")
    if tmp.exists():
        shutil.rmtree(tmp, ignore_errors=True) if tmp.is_dir() else tmp.unlink()
    # A vendor directory with no payload is an incomplete DOWNLOAD, not a conversion fault. Calling
    # it "failed" would hide a data-availability problem behind a converter error.
    if unit.is_dir() and not any((unit / m).exists() for m in VENDOR_PAYLOAD_MARKERS):
        return unit, "skipped", "vendor payload missing (incomplete download)"
    # The converter reads directories, not zips of them; box_convert_remote.ps1 extracts a .zip
    # before converting, so a zipped unit is `skipped` here and thereby deferred to the box phase.
    if is_zipped_unit(unit):
        return unit, "skipped", "zipped vendor directory — extracted and converted on the box"
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
    why = write_stamp(out, version, rid)
    if why:
        return unit, "failed", f"not stamped: {why}"
    return unit, "converted", ""


TOOLS = Path(__file__).resolve().parent


def s3_target(local: Path) -> str:
    """Local archive path -> its s3:// corpus URI, via corpus_lib (the one mapping the sync uses)."""
    import importlib.util
    lib = Path(os.path.expanduser("~/Claude/mzpeak-example-data/scripts/corpus_lib.py"))
    spec = importlib.util.spec_from_file_location("corpus_lib", lib)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod.s3_uri(str(local))


def run_box(jobs: list[tuple[Path, Path, int | None]], root: Path, version: str, box_jobs: int,
            recipes: dict[Path, Recipe] | None = None, publish_s3: bool = False) -> tuple[int, list[str]]:
    """Convert host-unsupported units on the box, relaying the archives back. `jobs` holds one
    (unit, archive, sample) per archive, so a multi-sample WIFF is one box job per listed sample.
    -> (box_convert.sh's exit code, names of the archives that did not arrive stamped).

    Delegates the transfer to tools/box_convert.sh --local-manifest, which already stages the raw
    through S3, converts in an isolated temp dir on the box, and pulls the .mzpeak back with a
    size+md5 check. Reimplementing that here would fork a second, less-tested transfer path.
    """
    if not jobs:
        print("box       : nothing to do")
        return 0, []
    # ONE updater. This harness used to run its own sync_box() (ssh + git checkout + cargo build)
    # and then box_convert.sh ran box_update_remote.ps1 on top of it: two updaters, two locks, and
    # on 2026-09-03 a box_update_remote.ps1 nobody had asked for sat beside five idle convert
    # workers for 32 minutes. box_convert.sh's updater is the only one now; it is told the exact
    # version to bring the box to, and BOX_REQUIRE_VERSION=1 (set by main) makes it abort instead
    # of converting with a stale exe. The stamps written below do not rely on that: each is read
    # from its archive's own index, so even a BOX_AUTOUPDATE=0 run cannot mislabel an archive.
    want = version.split()[-1]  # "mzpeak-convert 0.9.12" -> "0.9.12"
    manifest = root.parent / "validator_logs" / "box-jobs.tsv"
    manifest.parent.mkdir(parents=True, exist_ok=True)
    with manifest.open("w") as fh:
        for u, t, n in jobs:
            # The descriptor's own flags, so a box-built archive matches its host-built recipe
            # (an SDRF demonstrator keeps `--sdrf`); `--no-vendor` only where none are described.
            flags = (recipe_for(u, recipes or {}).flags or ['--no-vendor']) + sample_flags(n)
            # The archive comes back to the host by default: box_convert.sh relays it through a
            # staging key that it verifies and deletes. --publish-s3 names the DURABLE corpus key
            # instead, and box_convert.sh then copies the verified object onto s3://v09/..., the
            # public distribution bucket, before any validator has run. That used to be the default,
            # with `--no-s3-first` the one thing standing between a forgotten flag and unvalidated
            # public objects; it is opt-in now. corpus_lib owns the key mapping, so a conversion
            # target and a sync destination can never disagree.
            out = s3_target(t) if publish_s3 else t
            fh.write(f"{u}\t{out}\t{' '.join(flags)}\n")
    print(f"box       : {len(jobs)} job(s) -> {manifest}  (box converter pinned to v{want})")
    # box_convert.sh resolves a boto3-capable interpreter for the S3 relay itself (MZPC_PYTHON
    # overrides); nothing to arrange here.
    env = dict(os.environ)
    env["BOX_CONVERTER_VERSION"] = f"v{want}"
    # Fingerprint every target BEFORE the run. "The archive exists" does NOT mean the box just
    # delivered it: with --publish-s3 the box PUTs to the corpus KEY and the local copy stays stale by
    # design until the deferred pull. Stamping on existence alone labelled 21 August archives as
    # 0.9.0 after box_convert.sh had aborted (exit 3) without converting anything, and then reported
    # "COMPLETENESS 199/199 (100.0%)". Only a CHANGED file may be stamped.
    before = {}
    for _, out, _ in jobs:
        try:
            st = out.stat()
            before[out] = (st.st_mtime_ns, st.st_size)
        except OSError:
            before[out] = None
    proc = subprocess.run(
        # --overwrite, read only for a --publish-s3 target: the durable target is an ALREADY-PUBLISHED
        # corpus key, and replacing it is the whole point of a reconvert. Without it box_convert.sh
        # refuses the publish after a successful conversion ("REFUSING to overwrite existing"), drops
        # the staging key, and the box's work is thrown away -- 19 of 21 units converted and then
        # discarded.
        ["bash", str(TOOLS / "box_convert.sh"), "--overwrite",
         "--local-manifest", str(manifest), "--jobs", str(box_jobs)],
        text=True, env=env,
    )
    print(f"box       : box_convert exited {proc.returncode}")
    stamped, unchanged = 0, []
    for u, out, _ in jobs:
        try:
            st = out.stat()
            now = (st.st_mtime_ns, st.st_size)
        except OSError:
            unchanged.append(out.name)
            continue
        if before.get(out) == now:
            unchanged.append(out.name)
            continue
        why = write_stamp(out, version, recipe_for(u, recipes or {}).rid)
        if why:
            print(f"box       : {out.name} arrived but is left unstamped: {why}")
            unchanged.append(out.name)
        else:
            stamped += 1
    print(f"box       : stamped {stamped} delivered archive(s)")
    if unchanged:
        print(f"box       : {len(unchanged)} NOT delivered, left unstamped: "
              + ", ".join(sorted(unchanged)[:6]) + (" ..." if len(unchanged) > 6 else ""))
    return proc.returncode, unchanged


def convert_target(target: Path, cands: list[tuple[Path, int | None]], binary: str, version: str,
                   dry: bool, recipes: dict | None = None) -> tuple[Path, str, str]:
    """Convert the best candidate for one output archive; fall back on the next if it cannot run.

    Only a `skipped` outcome falls through — a genuine `failed` is reported as-is rather than being
    masked by silently converting a lesser source.
    """
    last = None
    for i, (u, n) in enumerate(cands):
        r = recipe_for(u, recipes or {})
        unit, status, detail = convert(u, target, binary, version, dry, [*r.flags, *sample_flags(n)], r.rid)
        if status != "skipped":
            if i:
                detail = (detail + " " if detail else "") + f"(fallback from {cands[0][0].name})"
            return unit, status, detail
        last = (unit, status, detail)
    return last


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("root", nargs="?", default=os.path.expanduser("~/Claude/mzpeak-example-data/data"))
    ap.add_argument("--clean", action="store_true", help="delete every .mzpeak first, then convert all")
    ap.add_argument("--jobs", type=int, default=max(1, (os.cpu_count() or 4) // 2))
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--report-only", action="store_true", help="audit completeness, convert nothing")
    ap.add_argument("--box", action="store_true",
                    help="also convert host-unsupported vendor units on the flash workstation")
    # Was 1, because every box job re-downloaded its own raw and N of those at once thrashed the box
    # temp disk. box_convert_remote.ps1 now keeps a persistent raw cache, so the download leg is a
    # local copy and the box sits idle ~98 % of the run. 3 is under box_convert.sh's own cap of 4,
    # which still clamps this (and MZPC_ALLOW_PARALLEL=1 there lifts the cap).
    ap.add_argument("--box-jobs", type=int, default=3,
                    help="box concurrency (default 3; box_convert.sh caps at MZPC_BOX_JOBS_CAP=4)")
    ap.add_argument("--publish-s3", action="store_true",
                    help="box copies each verified archive onto its durable corpus key (s3://v09/...) "
                         "instead of returning it to the host; nothing validates it first")
    # The default since --publish-s3 became opt-in; still accepted so an older release-day command runs.
    ap.add_argument("--no-s3-first", action="store_true", help=argparse.SUPPRESS)
    args = ap.parse_args(argv)

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
    recipes, pinned, desc_skipped, governed = load_recipes(root)
    if governed:
        before = len(units)
        units = apply_recipes(units, pinned, desc_skipped, governed)
        dropped = before - len(units)
        flagged = sum(1 for r in recipes.values() if r.flags)
        print(f"descriptors: {len(pinned)} pinned, {len(desc_skipped)} skipped"
              + (f" -> {dropped} undescribed unit(s) not built" if dropped else "")
              + (f"; {flagged} carry convert.flags" if flagged else ""))

    if args.clean and not (args.dry_run or args.report_only):
        removed = 0
        for p in list(root.rglob("*.mzpeak")) + list(root.rglob("*.mzpeak.built")):
            shutil.rmtree(p, ignore_errors=True) if p.is_dir() else p.unlink(missing_ok=True)
            removed += 1
        print(f"cleaned   : {removed} existing archives/stamps removed")

    groups = group_by_target(units, recipes)
    dup = {t: c for t, c in groups.items() if len(c) > 1}
    if dup:
        print(f"note      : {len(dup)} target(s) have several source formats; converting the "
              f"native one and skipping the duplicate(s)")
        for t, c in dup.items():
            print(f"            {t.name}  <- {', '.join(x.name for x, _ in c)}")
    rid = {t: recipe_for(c[0][0], recipes).rid for t, c in groups.items()}
    todo = [t for t in groups if not is_current(t, version, rid[t])]
    fresh = len(groups) - len(todo)
    print(f"archives  : {len(groups)} (from {len(units)} units)\nalready ok: {fresh}\nto convert: {len(todo)}\n")

    results: dict[str, list] = {"converted": [], "skipped": [], "failed": [], "would-convert": []}
    for t in groups:
        if t not in todo:
            results["converted"].append((groups[t][0][0], "", t))

    if not args.report_only and todo:
        started = time.time()
        with cf.ThreadPoolExecutor(max_workers=args.jobs) as pool:
            futs = {pool.submit(convert_target, t, groups[t], binary, version, args.dry_run, recipes): t for t in todo}
            for i, fut in enumerate(cf.as_completed(futs), 1):
                unit, status, detail = fut.result()
                t = futs[fut]
                results[status].append((unit, detail, t))
                mark = {"converted": "ok", "skipped": "--", "failed": "FAIL", "would-convert": "..."}[status]
                print(f"  [{i}/{len(todo)}] {mark:4} {unit.relative_to(root)}"
                      + (f" -> {t.name}" if t != target_for(unit) else "")
                      + (f"  ({detail})" if detail else ""), flush=True)
        print(f"\nelapsed   : {time.time() - started:.0f}s")

    # ---- box phase ----------------------------------------------------------
    # Units the host cannot convert (Windows-only vendor SDKs, missing msconvert) go to the flash
    # workstation. Payload-missing units are excluded: no binary can convert data that isn't there.
    box_rc, undelivered = 0, []
    if args.box and not (args.report_only or args.dry_run):
        print()
        deferred = [(u, t, dict(groups[t])[u]) for u, d, t in results["skipped"] if "payload missing" not in d]
        # BOX_REQUIRE_VERSION: this harness STAMPS archives with a version string, so converting
        # with a stale box exe would mislabel them. Abort instead.
        os.environ.setdefault("BOX_REQUIRE_VERSION", "1")
        box_rc, undelivered = run_box(deferred, root, version, args.box_jobs, recipes,
                                      publish_s3=args.publish_s3)

    # ---- report -------------------------------------------------------------
    have = [t for t in groups if is_current(t, version, rid[t])]
    print("\n" + "=" * 72)
    print(f"COMPLETENESS  {len(have)}/{len(groups)} archives current"
          f"  ({100.0 * len(have) / max(1, len(groups)):.1f}%)   [from {len(units)} raw units]")
    for key, label in (("skipped", "SKIPPED (host cannot convert)"), ("failed", "FAILED")):
        rows = results[key]
        if rows:
            print(f"\n{label}: {len(rows)}")
            for u, d, t in sorted(rows)[:40]:
                print(f"  - {u.relative_to(root)}" + (f" -> {t.name}" if t != target_for(u) else "")
                      + (f"\n      {d}" if d else ""))
    if box_rc or undelivered:
        # A unit the box did not deliver is as unbuilt as a FAILED one. Deferred units sit in
        # `skipped`, so the exit code used to say 0 while PXD077098 failed every rebuild at the relay.
        print(f"\nBOX NOT DELIVERED: {len(undelivered)} unit(s), box_convert exit {box_rc} -- left "
              f"unstamped, so the next run retries them")
        for name in sorted(undelivered)[:40]:
            print(f"  - {name}")

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

    # The denominator must be the corpus, not what the walk happened to recognise. An archive on
    # disk that no recognised unit produces is a unit the walk cannot see -- the failure mode that
    # hid two Waters `.raw.zip` units behind a "199/199 (100%)" line -- so it is a hard failure,
    # not a footnote. (An archive missing from disk is already visible in the COMPLETENESS count.)
    on_disk = {a for a in root.rglob("*.mzpeak") if not any(q.suffix == ".mzpeak" for q in a.parents)}
    # A unit whose descriptor lists convert.samples publishes per-sample archives; its former single
    # archive (En_PPY.mzpeak held 1 of 117 samples) must not stay beside them, publishable, in silence.
    superseded = sorted(on_disk & {target_for(u) for u in units if recipe_for(u, recipes).samples})
    if superseded:
        print(f"\nSUPERSEDED ON DISK: {len(superseded)} archive(s) whose descriptor now lists "
              f"convert.samples -- remove each, and its stamps, before publishing")
        for a in superseded[:40]:
            print(f"  - {a.relative_to(root)}")
    unaccounted = sorted(on_disk - set(groups) - set(superseded))
    if unaccounted:
        print(f"\nUNACCOUNTED ON DISK: {len(unaccounted)} archive(s) that no recognised raw unit "
              f"produces ({len(on_disk)} archives on disk vs {len(groups)} targets) -- the walk is "
              f"blind to their units; fix find_units, do not trust the completeness line")
        for a in unaccounted[:40]:
            print(f"  - {a.relative_to(root)}")

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
    return 1 if results["failed"] or unaccounted or superseded or undelivered or box_rc else 0


if __name__ == "__main__":
    sys.exit(main())
