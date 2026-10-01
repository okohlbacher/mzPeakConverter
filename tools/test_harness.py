#!/usr/bin/env python3
"""Offline checks for the corpus harness: tools/corpus_reconvert.py and the host half of
tools/box_convert.sh.

Nothing here reaches the box, S3 or the published corpus. The converter, box_convert.sh, ssh, scp
and the S3 relay are stand-ins, and every corpus is a temporary directory.

    python3 tools/test_harness.py          # the descriptor tests need PyYAML
"""
import base64
import contextlib
import hashlib
import io
import json
import os
import re
import subprocess
import sys
import tempfile
import unittest
import zipfile
from pathlib import Path
from unittest import mock

TOOLS = Path(__file__).resolve().parent
sys.path.insert(0, str(TOOLS))
import corpus_reconvert as cr  # noqa: E402

try:
    import yaml  # noqa: F401
except ImportError:
    yaml = None

VERSION = "9.9.9"

# Stand-in for mzpeak-convert: logs its argv and writes a split-facet zip whose index records the
# version and argv, the way the real binary's add_processing_metadata does. `--write-archive OUT
# VERSION OPTIONS` writes one directly, for the stand-in box.
FAKE_CONVERTER = """#!{python}
import json, os, sys, zipfile
args = sys.argv[1:]

def archive(out, version, options):
    index = {{"metadata": {{
        "software_list": [{{"id": "mzpeak-convert", "version": version}}],
        "data_processing_method_list": [{{"id": "mzpeak_convert_conversion", "methods": [
            {{"order": 1, "parameters": [{{"name": "conversion options", "value": options}}]}}]}}]}}}}
    with zipfile.ZipFile(out, "w") as z:
        z.writestr("spectra_metadata_scans.parquet", b"")
        z.writestr("mzpeak_index.json", json.dumps(index))

if args == ["--version"]:
    print("mzpeak-convert {version}"); sys.exit(0)
if args[0] == "--write-archive":
    archive(*args[1:4]); sys.exit(0)
if args[0].endswith(".wiff"):
    print("error: SciEX .wiff input is available only on Windows", file=sys.stderr); sys.exit(1)
with open(os.environ["FAKE_LOG"], "a") as log:
    log.write(json.dumps(args) + "\\n")
archive(args[args.index("-o") + 1], "{version}", " ".join(args))
"""

# Stand-in for `box_convert.sh [--overwrite] --local-manifest MF --jobs N`: keeps the manifest and
# "converts" every job, recording the argv that ran the way the box does. A unit named *native* was
# read natively, the mzML-lane flags (`--via-msconvert`, `--tof-grid <mode>`) stripped as the box's
# native-first attempt strips them; any other unit records the job's opts as given (a native lane
# the job pinned, or the msconvert fallback with the flags it adds). A unit named *undelivered*
# never comes back; one named *stale* comes back built by another converter version.
FAKE_BOX = """#!/usr/bin/env bash
while [ "$1" != "--local-manifest" ]; do shift; done
mf="$2"; cp "$mf" "$FAKE_MANIFEST"; rc=0
while IFS="$(printf '\\t')" read -r unit out opts; do
  case "$unit$out" in *undelivered*|*s3://*) rc=1; continue ;; esac
  ver="{version}"; case "$unit" in *stale*) ver=0.0.1 ;; esac
  ran=" $opts"; case "$unit" in *native*) ran=$(printf ' %s' $opts | sed -E 's/ --via-msconvert//; s/ --tof-grid [^ ]+//') ;; esac
  "$MZPEAK_CONVERT" --write-archive "$out" "$ver" "$(basename "$unit")$ran -o out.mzpeak --force"
done < "$mf"
exit $rc
"""


def make_corpus(tmp: Path, descriptors: dict, files: dict) -> Path:
    """`tmp/data/<tile>/<id>/...`: descriptors are written as JSON, which YAML reads."""
    root = tmp / "data"
    for rel, doc in descriptors.items():
        (root / rel).parent.mkdir(parents=True, exist_ok=True)
        (root / rel).write_text(json.dumps(doc))
    for rel, body in files.items():
        (root / rel).parent.mkdir(parents=True, exist_ok=True)
        (root / rel).write_bytes(body)
    return root


class Harness(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = Path(self._tmp.name)
        self.log = self.tmp / "converter.log"
        fake = self.tmp / "mzpeak-convert"
        fake.write_text(FAKE_CONVERTER.format(python=sys.executable, version=VERSION))
        fake.chmod(0o755)
        self.box_tools = self.tmp / "tools"
        self.box_tools.mkdir()
        (self.box_tools / "box_convert.sh").write_text(FAKE_BOX.format(version=VERSION))
        patches = [
            mock.patch.dict(os.environ, {"MZPEAK_CONVERT": str(fake), "FAKE_LOG": str(self.log),
                                         "FAKE_MANIFEST": str(self.tmp / "manifest.tsv")}),
            mock.patch.object(cr, "TOOLS", self.box_tools),
        ]
        for p in patches:
            p.start()
            self.addCleanup(p.stop)
        self.addCleanup(self._tmp.cleanup)

    def run_main(self, root: Path, *extra: str) -> tuple[int, str]:
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            rc = cr.main([str(root), "--jobs", "1", *extra])
        return rc, buf.getvalue()

    def converted(self) -> dict[str, list[str]]:
        """Output archive name -> the argv the host converter ran it with."""
        runs = [json.loads(line) for line in self.log.read_text().splitlines()] if self.log.exists() else []
        return {Path(a[a.index("-o") + 1]).name: a for a in runs}

    def runs(self) -> int:
        return len(self.log.read_text().splitlines()) if self.log.exists() else 0

    def manifest(self) -> dict[str, tuple[str, str]]:
        """Box job output name -> (output as written, opts)."""
        rows = [line.split("\t") for line in (self.tmp / "manifest.tsv").read_text().splitlines()]
        return {Path(out).name: (out, opts) for _, out, opts in rows}


@unittest.skipIf(yaml is None, "PyYAML not installed")
class DescriptorFlags(Harness):
    def test_flags_reach_the_unit_whatever_picks_it(self):
        root = make_corpus(self.tmp, {
            # no convert.input at all: the imzML demonstrators that lost their --image
            "imzml-examples/ltp/ltp.yaml": {"convert": {"flags": "--image data/imzml-examples/ltp/img/CHJ2.png"}},
            "general-ms/auto/auto.yaml": {"convert": {"input": "auto", "flags": "--zstd-level 12"}},
            "general-ms/pin/pin.yaml": {"convert": {"input": "run.mzML", "flags": "--zstd-level 12"}},
            "general-ms/bare/bare.yaml": {"convert": {"input": "auto"}},
        }, {
            "imzml-examples/ltp/img/CHJ2.png": b"png",
            "imzml-examples/ltp/img/chilli.imzML": b"x",
            "imzml-examples/ltp/img/chilli.ibd": b"x",
            "general-ms/auto/a.mzML": b"x",
            "general-ms/pin/run.mzML": b"x",
            "general-ms/bare/b.mzML": b"x",
        })
        rc, out = self.run_main(root)
        self.assertEqual(rc, 0, out)
        argv = self.converted()
        image = str((root / "imzml-examples/ltp/img/CHJ2.png").resolve())
        self.assertEqual(argv["chilli.mzpeak"][-2:], ["--image", image])
        self.assertEqual(argv["a.mzpeak"][-2:], ["--zstd-level", "12"])
        self.assertEqual(argv["run.mzpeak"][-2:], ["--zstd-level", "12"])
        self.assertEqual(argv["b.mzpeak"][-1], "-f")


@unittest.skipIf(yaml is None, "PyYAML not installed")
class Stamps(Harness):
    def test_a_recipe_change_or_a_recipeless_stamp_rebuilds(self):
        cv = {"input": "auto", "flags": "--zstd-level 12"}
        root = make_corpus(self.tmp, {"general-ms/ds/ds.yaml": {"convert": cv}}, {"general-ms/ds/a.mzML": b"x"})
        stamp = root / "general-ms/ds/a.mzpeak.built"
        self.run_main(root)
        self.run_main(root)
        self.assertEqual(self.runs(), 1, "a current archive was rebuilt")
        lines = stamp.read_text().splitlines()
        self.assertEqual(lines[:2], [f"mzpeak-convert {VERSION}", f"recipe {cr.recipe_id(cv)}"])
        self.assertRegex(lines[2], r"^options .* --zstd-level 12$")

        (root / "general-ms/ds/ds.yaml").write_text(json.dumps({"convert": {**cv, "flags": "--zstd-level 9"}}))
        self.run_main(root)
        self.assertEqual(self.runs(), 2, "a convert.flags edit did not rebuild the archive")

        stamp.write_text(f"mzpeak-convert {VERSION}\n")   # the stamp format before recipes
        self.run_main(root)
        self.assertEqual(self.runs(), 3, "a stamp naming no recipe counted as current")

    def test_a_stamp_recording_another_lane_than_the_descriptor_is_stale(self):
        cv = {"input": "auto", "flags": "--zstd-level 12"}
        root = make_corpus(self.tmp, {"general-ms/ds/ds.yaml": {"convert": cv}}, {"general-ms/ds/a.mzML": b"x"})
        stamp = root / "general-ms/ds/a.mzpeak.built"
        self.run_main(root)
        lines = stamp.read_text().splitlines()
        # the box's msconvert fallback, recorded under a descriptor pinning no lane: three SciEX
        # archives of the corpus were stamped so before the check and counted as current
        stamp.write_text("\n".join(lines[:2] + [lines[2] + " --via-msconvert --tof-grid auto"]) + "\n")
        rc, out = self.run_main(root)
        self.assertEqual(self.runs(), 2, "an archive stamped on another lane than its descriptor pins counted as current")
        self.assertIn("stale     : a.mzpeak: lane mismatch: the descriptor pins no lane flag, the archive ran with "
                      "--tof-grid auto --via-msconvert (options: ", out)
        self.assertRegex(stamp.read_text().splitlines()[2], r"^options .* --zstd-level 12$")
        self.run_main(root)
        self.assertEqual(self.runs(), 2, "the rebuilt archive is current")
        # the reverse: the descriptor pins a lane the stamp does not record
        (root / "general-ms/ds/ds.yaml").write_text(json.dumps({"convert": {**cv, "flags": "--zstd-level 12 --via-msconvert"}}))
        self.run_main(root)
        self.assertEqual(self.runs(), 3, "a recipe change rebuilds")
        lines = stamp.read_text().splitlines()
        self.assertIn(" --via-msconvert", lines[2])
        stamp.write_text("\n".join(lines[:2] + [lines[2].replace(" --via-msconvert", "")]) + "\n")
        rc, out = self.run_main(root)
        self.assertEqual(self.runs(), 4)
        self.assertIn("lane mismatch: the descriptor pins --via-msconvert, the archive ran with no lane flag", out)
        # a stamp without an options line is current under a descriptor pinning no lane, stale under one that does
        stamp.write_text("\n".join(lines[:2]) + "\n")
        self.run_main(root)
        self.assertEqual(self.runs(), 5)
        (root / "general-ms/ds/ds.yaml").write_text(json.dumps({"convert": cv}))
        self.run_main(root)   # the recipe changed back: one rebuild ...
        stamp.write_text("\n".join(stamp.read_text().splitlines()[:2]) + "\n")
        self.run_main(root)   # ... and the stamp without an options line is current
        self.assertEqual(self.runs(), 6)

    def test_box_archives_are_stamped_from_their_own_index(self):
        lane = {"input": "auto", "flags": "--via-msconvert --tof-grid auto"}
        root = make_corpus(self.tmp, {
            "general-ms/sciex/sciex.yaml": {"convert": lane},
            # pins the msconvert lane; the box reads the unit natively (D17: agilent-qtof)
            "general-ms/agilent/agilent.yaml": {"convert": {"input": "auto", "flags": "--via-msconvert --zstd-level 12"}},
            "general-ms/old/old.yaml": {"convert": {"input": "auto"}},
        }, {
            "general-ms/sciex/run.wiff": b"x",
            "general-ms/sciex/run.wiff.scan": b"x",
            "general-ms/agilent/native.wiff": b"x",
            "general-ms/agilent/native.wiff.scan": b"x",
            "general-ms/old/stale.wiff": b"x",
        })
        rc, out = self.run_main(root, "--box", "--no-s3-first")
        self.assertEqual(self.manifest()["run.mzpeak"][1], "--via-msconvert --tof-grid auto")
        self.assertEqual((root / "general-ms/sciex/run.mzpeak.built").read_text().splitlines(),
                         [f"mzpeak-convert {VERSION}", f"recipe {cr.recipe_id(lane)}",
                          "options run.wiff --via-msconvert --tof-grid auto -o out.mzpeak --force"])
        self.assertTrue((root / "general-ms/agilent/native.mzpeak").exists())
        self.assertFalse((root / "general-ms/agilent/native.mzpeak.built").exists(),
                         "an archive built on another lane than its descriptor pins was stamped current")
        self.assertIn("native.mzpeak arrived but is left unstamped: lane mismatch: the descriptor pins "
                      "--via-msconvert, the archive ran with no lane flag "
                      "(options: native.wiff --zstd-level 12 -o out.mzpeak --force)", out)
        self.assertTrue((root / "general-ms/old/stale.mzpeak").exists())
        self.assertFalse((root / "general-ms/old/stale.mzpeak.built").exists(),
                         "an archive another converter version built was stamped current")
        self.assertIn("built by mzpeak-convert 0.0.1", out)
        self.assertEqual(rc, 1, out)


def edit_index(archive: Path, edit) -> None:
    """Rewrite `archive`'s index metadata through `edit(metadata)`, the other members kept."""
    with zipfile.ZipFile(archive) as z:
        members = {n: z.read(n) for n in z.namelist()}
    index = json.loads(members["mzpeak_index.json"])
    edit(index["metadata"])
    members["mzpeak_index.json"] = json.dumps(index).encode()
    with zipfile.ZipFile(archive, "w") as z:
        for n, body in members.items():
            z.writestr(n, body)


class StampChecks(Harness):
    """`write_stamp` against stub archives: the fixture converter writes one with a given argv
    (`--write-archive`), and `edit_index` changes what its index records."""

    def stub(self, name: str, options: str, version: str = VERSION) -> Path:
        out = self.tmp / name
        subprocess.run([os.environ["MZPEAK_CONVERT"], "--write-archive", str(out), version, options], check=True)
        return out

    def test_the_descriptor_lane_must_be_the_one_the_archive_ran_and_no_other(self):
        v = f"mzpeak-convert {VERSION}"
        native = self.stub("native.mzpeak", "x.d --zstd-level 12 -o out.mzpeak --force")
        self.assertIsNone(cr.write_stamp(native, v, "r", ["--zstd-level", "12"]))
        self.assertEqual(cr.stamp_for(native).read_text().splitlines()[2], "options x.d --zstd-level 12 -o out.mzpeak --force")
        # a flag outside the lane set is the recipe hash's business, not the stamp's
        self.assertIsNone(cr.write_stamp(native, v, "r", []))
        self.assertIsNone(cr.write_stamp(native, v, "r", ["--zstd-level", "9", "--sample", "2"]))
        # agilent-qtof: the descriptor pins the msconvert lane, the box built the archive natively
        self.assertEqual(cr.write_stamp(native, v, "r", ["--via-msconvert", "--tof-grid", "auto", "--zstd-level", "12"]),
                         "lane mismatch: the descriptor pins --tof-grid auto --via-msconvert, the archive ran with "
                         "no lane flag (options: x.d --zstd-level 12 -o out.mzpeak --force)")
        pwiz = self.stub("pwiz.mzpeak", "x.d --via-msconvert --tof-grid auto -o out.mzpeak --force")
        self.assertIsNone(cr.write_stamp(pwiz, v, "r", ["--via-msconvert", "--tof-grid", "auto"]))
        self.assertIsNone(cr.write_stamp(pwiz, v, "r", ["--tof-grid=auto", "--via-msconvert"]), "one pin, two spellings")
        # the reverse: the msconvert fallback ran under a descriptor pinning nothing ...
        self.assertEqual(cr.write_stamp(pwiz, v, "r", []),
                         "lane mismatch: the descriptor pins no lane flag, the archive ran with --tof-grid auto "
                         "--via-msconvert (options: x.d --via-msconvert --tof-grid auto -o out.mzpeak --force)")
        # ... or under agilent-6490-triplequad's, which pins the lane but not the fallback's grid
        self.assertIn("the descriptor pins --via-msconvert, the archive ran with --tof-grid auto --via-msconvert",
                      cr.write_stamp(pwiz, v, "r", ["--via-msconvert"]))
        self.assertIn("lane mismatch", cr.write_stamp(pwiz, v, "r", ["--via-msconvert", "--tof-grid", "on"]))
        sdk = self.stub("sdk.mzpeak", "x.d --bruker-sdk --no-vendor -o out.mzpeak --force")
        self.assertIsNone(cr.write_stamp(sdk, v, "r", ["--bruker-sdk"]))
        self.assertIn("the descriptor pins --via-msconvert, the archive ran with --bruker-sdk",
                      cr.write_stamp(sdk, v, "r", ["--via-msconvert"]))
        # the version check comes first and is unchanged
        self.assertEqual(cr.write_stamp(native, "mzpeak-convert 0.0.1", "r", []),
                         f"built by mzpeak-convert {VERSION}, not mzpeak-convert 0.0.1")
        self.assertEqual(sorted(p.name for p in self.tmp.glob("*.built")), ["native.mzpeak.built", "pwiz.mzpeak.built", "sdk.mzpeak.built"])

    def test_a_recorded_argv_the_shell_splitter_rejects_is_split_on_whitespace(self):
        # an apostrophe in a vendor folder's name: `shlex.split` raises "No closing quotation"
        self.assertEqual(cr.argv_of("O'Neil.d --via-msconvert -o out.mzpeak"), ["O'Neil.d", "--via-msconvert", "-o", "out.mzpeak"])
        self.assertEqual(cr.argv_of('"My Run.d" --bruker-sdk -o out.mzpeak'), ["My Run.d", "--bruker-sdk", "-o", "out.mzpeak"])
        v = f"mzpeak-convert {VERSION}"
        quoted = self.stub("quoted.mzpeak", "O'Neil.d --via-msconvert -o out.mzpeak --force")
        self.assertIsNone(cr.write_stamp(quoted, v, "r", ["--via-msconvert"]))
        self.assertIn("the archive ran with --via-msconvert", cr.write_stamp(quoted, v, "r", []))

    def test_the_version_and_argv_are_the_last_conversions_whatever_its_software_id(self):
        v = f"mzpeak-convert {VERSION}"
        # an mzML this tool exported from a 0.16.0 archive, converted again: the source brings the
        # earlier conversion's software entry and method along, and the new ones are numbered
        twice = self.stub("twice.mzpeak", "old.raw -o a.mzpeak")
        conversion = lambda n, ref, options: {"id": f"mzpeak_convert_conversion{n}", "methods": [
            {"order": 1, "software_reference": ref, "parameters": [{"name": "conversion options", "value": options}]}]}

        def numbered(md):
            md["software_list"] = [{"id": "pwiz", "version": "3"}, {"id": "mzpeak-convert", "version": "0.16.0"},
                                   {"id": "mzpeak-convert_2", "version": VERSION}]
            md["data_processing_method_list"] = [conversion("", "mzpeak-convert", "old.raw -o a.mzpeak"),
                                                 conversion("_2", "mzpeak-convert_2", "a.mzML --via-msconvert -o b.mzpeak")]
        edit_index(twice, numbered)
        self.assertIsNone(cr.write_stamp(twice, v, "r", ["--via-msconvert"]))
        self.assertEqual(cr.stamp_for(twice).read_text().splitlines(),
                         [v, "recipe r", "options a.mzML --via-msconvert -o b.mzpeak"])
        self.assertEqual(cr.write_stamp(twice, "mzpeak-convert 0.16.0", "r", []), f"built by mzpeak-convert {VERSION}, not mzpeak-convert 0.16.0")
        # the same version converting its own export reuses the plain id; a method without a
        # software reference falls back to the last of this tool's entries
        def unreferenced(md):
            md["software_list"] = [{"id": "mzpeak-convert", "version": "0.16.0"}, {"id": "mzpeak-convert_2", "version": VERSION}]
            for dp in md["data_processing_method_list"]:
                for m in dp["methods"]:
                    m.pop("software_reference", None)
        edit_index(twice, unreferenced)
        self.assertIsNone(cr.write_stamp(twice, v, "r", ["--via-msconvert"]))
        # an index recording no conversion at all is refused, as before
        plain = self.stub("plain.mzpeak", "x.mzML -o x.mzpeak")
        edit_index(plain, lambda md: md.update(software_list=[], data_processing_method_list=[]))
        self.assertEqual(cr.write_stamp(plain, v, "r", []), f"built by mzpeak-convert <unrecorded>, not {v}")


@unittest.skipIf(yaml is None, "PyYAML not installed")
class BoxPhase(Harness):
    def test_a_box_unit_that_did_not_arrive_fails_the_run(self):
        root = make_corpus(self.tmp, {
            "general-ms/ok/ok.yaml": {"convert": {"input": "auto"}},
            "general-ms/lost/lost.yaml": {"convert": {"input": "auto"}},
            "general-ms/old/old.yaml": {"convert": {"input": "auto"}},
        }, {
            "general-ms/ok/run.wiff": b"x",
            "general-ms/lost/undelivered.wiff": b"x",
            "general-ms/old/stale.wiff": b"x",
        })
        rc, out = self.run_main(root, "--box")
        self.assertEqual(rc, 1, out)
        self.assertIn("BOX NOT DELIVERED: 2 archive(s), box_convert exit 1", out)
        self.assertEqual(out.split("BOX NOT DELIVERED")[1].count("  - "), 2)
        self.assertTrue((root / "general-ms/ok/run.mzpeak.built").exists())

    def test_box_archives_return_to_the_host_unless_publishing_is_asked_for(self):
        root = make_corpus(self.tmp, {"general-ms/ds/ds.yaml": {"convert": {"input": "auto"}}},
                           {"general-ms/ds/run.wiff": b"x"})
        with mock.patch.object(cr, "s3_target", side_effect=AssertionError("a durable key by default")):
            rc, out = self.run_main(root, "--box")
        self.assertEqual(rc, 0, out)
        self.assertEqual(self.manifest()["run.mzpeak"][0], str(root / "general-ms/ds/run.mzpeak"))

        (root / "general-ms/ds/run.mzpeak.built").unlink()
        with mock.patch.object(cr, "s3_target", side_effect=lambda p: f"s3://v09/{p.relative_to(root)}"):
            self.run_main(root, "--box", "--publish-s3")
        self.assertEqual(self.manifest()["run.mzpeak"][0], "s3://v09/general-ms/ds/run.mzpeak")

    def test_convert_samples_builds_one_archive_per_sample(self):
        sciex = self.tmp / "data/general-ms/sciex"
        root = make_corpus(self.tmp, {"general-ms/sciex/sciex.yaml": {"convert": {"input": "En_PPY.wiff",
                                                                                   "samples": [117, 2]}}},
                           {"general-ms/sciex/En_PPY.wiff": b"x", "general-ms/sciex/En_PPY.wiff.scan": b"x"})
        with zipfile.ZipFile(sciex / "En_PPY.mzpeak", "w") as z:     # the one-sample-of-117 archive
            z.writestr(cr.FORMAT_MARKER, b"")
        rc, out = self.run_main(root, "--box")
        jobs = self.manifest()
        self.assertEqual(sorted(jobs), ["En_PPY.sample117.mzpeak", "En_PPY.sample2.mzpeak"])
        self.assertEqual(jobs["En_PPY.sample2.mzpeak"], (str(sciex / "En_PPY.sample2.mzpeak"), "--no-vendor --sample 2"))
        self.assertEqual(jobs["En_PPY.sample117.mzpeak"][1], "--no-vendor --sample 117")
        for n in (2, 117):
            self.assertTrue((sciex / f"En_PPY.sample{n}.mzpeak.built").exists(), out)
        # the single archive the samples replace must not stay beside them, publishable, in silence
        self.assertEqual(rc, 1, out)
        self.assertIn("SUPERSEDED ON DISK: 1 archive(s)", out)
        self.assertNotIn("UNACCOUNTED ON DISK", out)


class BoxScripts(unittest.TestCase):
    def test_no_box_script_sets_dotnet_roll_forward(self):
        # mzpeak-convert sets LatestMajor for Thermo .raw only (src/main.rs). A box-wide export lifts
        # the Shimadzu and SciEX glues onto whatever newer .NET major sits in the dotnet8 root.
        for ps1 in sorted(TOOLS.glob("*.ps1")):
            for n, line in enumerate(ps1.read_text(encoding="utf-8").splitlines(), 1):
                self.assertNotRegex(line.split("#", 1)[0], r"DOTNET_ROLL_FORWARD\s*=", f"{ps1.name}:{n}")


# ---- box_convert.sh ---------------------------------------------------------------------------
# Its functions run in a bash with every network edge replaced: `box` answers the ssh call with a
# canned BOXRESULT (and records Remove-Item calls; BOX_HANG=<s> makes it hang first), `relay` stands
# in for s3_relay.py, and `scp` either takes a job file up to the box (kept as $T/job.json) or
# copies a local file down.
DRIVER = r"""
set -uo pipefail
RELAY=(relay); SSH=(box)
relay(){
  case "$1" in
    presign-put) echo "https://relay.invalid/put" ;;
    get) cp "$FAKE_OBJECT" "$3" ;;
    md5) python3 -c 'import hashlib,sys;print(hashlib.md5(open(sys.argv[1],"rb").read()).hexdigest())' "$2" ;;
    *) return 1 ;;
  esac
}
box(){
  shift
  case "$*" in
    *Remove-Item*) printf '%s\n' "$*" >> "$T/removed" ;;
    *) printf '%s\n' "$*" >> "$T/remote"; [ -n "${BOX_HANG:-}" ] && exec sleep "$BOX_HANG"  # one process, like ssh
       printf '<<<BOXRESULT\n%s\nBOXRESULT>>>\n' "$RESULT_B64" ;;
  esac
}
scp(){
  [ -n "${SCP_HANG:-}" ] && exec sleep "$SCP_HANG"
  case "${@: -1}" in
    *@*:*) cp "${@: -2:1}" "$T/job.json"; printf '%s\n' "${@: -1}" >> "$T/scp_up" ;;
    *) printf '%s\n' "${@: -2:1}" >> "$T/scp"; cp "$FAKE_OBJECT" "${@: -1}" ;;
  esac
}
BOX_SSH=user@box BOX_SSH_KEY=/dev/null PROXY=ProxyCommand=true REMOTE_PS='C:\box_convert_remote.ps1'
ARCHIVE=false PUT_EXPIRES=60 CORPUS_ROOT="$T/corpus" PENDING_DIR="$T/pending" FETCH_LIST="$T/fetch"
mkdir -p "$PENDING_DIR"
"""


def shell_functions(*names: str) -> str:
    text = (TOOLS / "box_convert.sh").read_text()
    found = []
    for name in names:
        m = re.search(rf"^{name}\(\)\{{.*?^\}}$", text, re.S | re.M)
        if not m:
            raise AssertionError(f"box_convert.sh defines no {name}()")
        found.append(m.group(0))
    return "\n".join(found)


class Shell(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.tmp = Path(self._tmp.name)
        (self.tmp / "bin").mkdir()
        (self.tmp / "bin" / "python3").symlink_to(sys.executable)   # box_convert.sh calls python3

    def bash(self, script: str, **env: str) -> tuple[int, str]:
        """Run `script`, which ends by echoing rc=$?; -> (that rc, stdout+stderr)."""
        p = subprocess.run(["bash", "-c", script], capture_output=True, text=True, env={
            **os.environ, "PATH": f"{self.tmp / 'bin'}:{os.environ['PATH']}", "T": str(self.tmp), **env})
        m = re.search(r"^rc=(\d+)$", p.stdout, re.M)
        self.assertIsNotNone(m, p.stdout + p.stderr)
        return int(m.group(1)), p.stdout + p.stderr


class BoxJob(Shell):
    def run_job(self, result: dict, out: str, opts: str, obj: bytes = b"archive bytes",
                **env: str) -> tuple[int, str]:
        """Run box_convert.sh's run_job against a box that answers `result`; -> (rc, output)."""
        (self.tmp / "object").write_bytes(obj)
        result = {"stage": "done", "exit": 0, "uploaded": True, "size": len(obj),
                  "md5": hashlib.md5(obj).hexdigest(), "error": "", "note": "", **result}
        script = DRIVER + shell_functions("pull_held", "scp_up", "ssh_watchdog", "run_job") + '\nrun_job raw "$OUT" "$OPTS" box-convert/k.mzpeak 0123abcd; echo "rc=$?"\n'
        (self.tmp / "tmp").mkdir(exist_ok=True)
        return self.bash(script, **{"TMPDIR": str(self.tmp / "tmp"), **env}, FAKE_OBJECT=str(self.tmp / "object"),
                         OUT=out, OPTS=opts, RESULT_B64=base64.b64encode(json.dumps(result).encode()).decode())

    def test_the_job_goes_to_the_box_as_a_file(self):
        # ssh's stdin never reached the box from some networks, and ReadToEnd() waited forever.
        rc, log = self.run_job({}, str(self.tmp / "run.mzpeak"), "--no-vendor")
        self.assertEqual(rc, 0, log)
        job = json.loads((self.tmp / "job.json").read_text())
        self.assertEqual(job["put_url"], "https://relay.invalid/put")
        (dst,) = (self.tmp / "scp_up").read_text().splitlines()
        self.assertRegex(dst, r"^user@box:C:\\Users\\User\\AppData\\Local\\Temp\\bxc-job-0123abcd-\w+\.json$")
        self.assertIn(f"-JobFile {dst.split(':', 1)[1]}", (self.tmp / "remote").read_text())
        self.assertEqual(list((self.tmp / "tmp").iterdir()), [], "the host copy (presigned URLs) is removed")

    def test_a_stalled_job_upload_fails_instead_of_hanging(self):
        import time
        t0 = time.monotonic()
        rc, log = self.run_job({}, str(self.tmp / "run.mzpeak"), "--no-vendor", SCP_HANG="60", BOX_SCP_TIMEOUT="2")
        self.assertEqual(rc, 1, log)
        self.assertIn("could not copy the job to the box", log)
        self.assertLess(time.monotonic() - t0, 30)
        self.assertEqual(list((self.tmp / "tmp").iterdir()), [], "the host copy is removed on failure too")

    def test_a_job_that_hangs_is_ended_by_the_watchdog(self):
        import time
        t0 = time.monotonic()
        rc, log = self.run_job({}, str(self.tmp / "run.mzpeak"), "--no-vendor", BOX_HANG="60", BOX_JOB_TIMEOUT="2")
        self.assertEqual(rc, 1, log)
        self.assertIn("no result from box within BOX_JOB_TIMEOUT=2 s", log)
        self.assertLess(time.monotonic() - t0, 30)

    def test_the_bench_row_names_the_options_that_ran(self):
        out, bench = self.tmp / "run.mzpeak", self.tmp / "bench.tsv"
        rc, log = self.run_job({"argv": "--no-vendor"}, str(out), "--no-vendor --via-msconvert --tof-grid auto",
                               BENCH_TSV=str(bench))
        self.assertEqual(rc, 0, log)
        self.assertEqual(out.read_bytes(), b"archive bytes")
        self.assertEqual(bench.read_text().splitlines()[1].split("\t")[3], "--no-vendor")

    HOLD = "C:/Users/User/bxc-hold/bxc-" + "0123456789abcdef" * 2 + ".mzpeak"
    TOO_BIG = {"stage": "too-big", "uploaded": False, "error": "mzpeak 9039127239 B exceeds the 5 GB single-PUT limit"}

    def test_an_archive_over_the_relay_ceiling_comes_back_by_scp(self):
        out = self.tmp / "corpus/ims/run.mzpeak"
        rc, log = self.run_job({**self.TOO_BIG, "hold": self.HOLD}, str(out), "--no-vendor")
        self.assertEqual(rc, 0, log)
        self.assertEqual(out.read_bytes(), b"archive bytes")
        self.assertTrue(json.loads((self.tmp / "job.json").read_text())["hold_oversize"])
        self.assertEqual((self.tmp / "scp").read_text(), f"user@box:{self.HOLD}\n")
        self.assertIn(self.HOLD, (self.tmp / "removed").read_text())
        self.assertEqual(list(self.tmp.rglob("*.part")), [])

    def test_a_corrupt_scp_pull_is_refused_and_the_box_copy_still_removed(self):
        out = self.tmp / "run.mzpeak"
        rc, log = self.run_job({**self.TOO_BIG, "hold": self.HOLD, "md5": "0" * 32}, str(out), "--no-vendor")
        self.assertEqual(rc, 1, log)
        self.assertIn("md5 mismatch", log)
        self.assertFalse(out.exists())
        self.assertEqual(list(self.tmp.rglob("*.part")), [])
        self.assertIn(self.HOLD, (self.tmp / "removed").read_text())

    def test_an_s3_target_asks_the_box_to_hold_nothing(self):
        rc, log = self.run_job(self.TOO_BIG, "s3://v09/ims/run.mzpeak", "--no-vendor")
        self.assertEqual(rc, 1, log)
        self.assertNotIn("hold_oversize", json.loads((self.tmp / "job.json").read_text()))
        self.assertFalse((self.tmp / "scp").exists())


class Pool(Shell):
    def test_a_slot_frees_when_any_job_ends(self):
        # The pool used to wait on its OLDEST job, so one long unit kept the other slots idle.
        mf = self.tmp / "jobs.tsv"
        mf.write_text("slow\tout0\n" + "".join(f"quick{i}\tout{i}\n" for i in range(1, 5)))
        script = shell_functions("run_pool") + r"""
job(){ date +%s.%N > "$T/start-$1" 2>/dev/null || python3 -c 'import time;print(time.time())' > "$T/start-$1"
       case "$1" in slow) sleep 8 ;; *) sleep 1 ;; esac
       python3 -c 'import time;print(time.time())' > "$T/end-$1"; }
run_pool "$MF" job 2; echo "rc=$?"
"""
        rc, log = self.bash(script, MF=str(mf))
        self.assertEqual(rc, 0, log)
        t = lambda kind, name: float((self.tmp / f"{kind}-{name}").read_text().strip().replace("N", "0"))
        self.assertLess(t("start", "quick4"), t("end", "slow"), "quick4 waited for the slow job")


class SyncBox(Shell):
    """sync_box_converter against a stand-in box_update_remote.ps1 reply, as a corpus run calls it."""

    def sync(self, **reply: str) -> tuple[int, str]:
        stub = r"""ssh_watchdog(){ cat >/dev/null; printf '<<<BOXSYNC\n%s\nBOXSYNC>>>\n' "$REPLY_B64"; }"""
        script = f"set -uo pipefail\n{stub}\n{shell_functions('sync_box_converter')}\nsync_box_converter; echo \"rc=$?\"\n"
        return self.bash(script, BOX_CONVERTER_VERSION="v0.11.5", BOX_REQUIRE_VERSION="1",
                         REPLY_B64=base64.b64encode(json.dumps(reply).encode()).decode())

    def test_a_failed_update_of_a_box_on_the_wanted_version_proceeds(self):
        rc, out = self.sync(action="failed", have="0.11.5", error="git fetch failed (network/auth?)")
        self.assertEqual(rc, 0, out)
        self.assertIn("installed 0.11.5 is the wanted version", out)

    def test_a_stale_or_dirty_box_still_stops_the_run(self):
        self.assertEqual(self.sync(action="failed", have="0.11.4", error="cargo build failed")[0], 1)
        self.assertEqual(self.sync(action="refused-dirty", have="0.11.5", error="2 uncommitted change(s)")[0], 1)

    def probe(self, reports: str, **env: str) -> tuple[int, str]:
        """sync_box_converter under BOX_AUTOUPDATE=0, the box's exe answering `mzpeak-convert <reports>`."""
        stub = r"""ssh_watchdog(){ shift; printf '%s\n' "$*" >> "$T/probe"; printf 'mzpeak-convert %s\r\n' "$REPORTS"; }"""
        fns = shell_functions("assert_box_version", "sync_box_converter")
        script = f"set -uo pipefail\n{stub}\n{fns}\nsync_box_converter; echo \"rc=$?\"\n"
        return self.bash(script, **{"BOX_AUTOUPDATE": "0", "BOX_CONVERTER_VERSION": "v0.11.5",
                                    "BOX_REQUIRE_VERSION": "1", "REPORTS": reports, **env})

    def test_without_the_updater_the_box_version_is_still_asserted(self):
        # BOX_AUTOUPDATE=0 used to skip every check: the first word on the box's version came from
        # the archives' software_list, after the jobs.
        rc, out = self.probe("0.11.4")
        self.assertEqual(rc, 1, out)
        self.assertIn("reports 0.11.4, but this run requires 0.11.5", out)
        self.assertIn("no job dispatched", out)
        self.assertIn(r"& 'C:\Users\User\src\mzPeakConverter\target\release\mzpeak-convert.exe' --version",
                      (self.tmp / "probe").read_text(), "the exe box_convert_remote.ps1 runs by default")
        self.assertEqual(self.probe("0.11.5")[0], 0)
        rc, out = self.probe("0.11.4", BOX_REQUIRE_VERSION="0")
        self.assertEqual(rc, 0, "soft without BOX_REQUIRE_VERSION=1")
        self.assertIn("requires 0.11.5", out)

    def test_the_probe_asks_the_exe_the_jobs_run_and_only_when_a_version_is_named(self):
        self.assertEqual(self.probe("0.11.5", BOX_CONVERTER=r"C:\Users\User\bin\mzpeak-convert-0.11.5.exe")[0], 0)
        self.assertIn(r"& 'C:\Users\User\bin\mzpeak-convert-0.11.5.exe' --version", (self.tmp / "probe").read_text())
        (self.tmp / "probe").unlink()
        self.assertEqual(self.probe("0.0.1", BOX_CONVERTER_VERSION="")[0], 0)
        self.assertFalse((self.tmp / "probe").exists(), "an ad-hoc run names no version: nothing to ask")


class BoxEntry(Shell):
    """The whole of box_convert.sh, from a checkout of its own, against stand-in ssh, scp and relay."""

    SSH = r"""#!/bin/sh
printf '%s\n' "$*" >> "$T/ssh"
case "$*" in *--version*) printf 'mzpeak-convert %s\r\n' "$BOX_REPORTS" ;; esac
"""
    SCP = "#!/bin/sh\nexit 0\n"
    RELAY = """#!/bin/sh
[ "$1" = -c ] && exit 0            # resolve_relay_python's boto3 probe
case "$2" in presign-put) echo https://relay.invalid/put ;; delete) ;; *) exit 1 ;; esac
"""

    def setUp(self):
        super().setUp()
        for name, body in (("ssh", self.SSH), ("scp", self.SCP), ("relay", self.RELAY)):
            (self.tmp / "bin" / name).write_text(body)
            (self.tmp / "bin" / name).chmod(0o755)
        self.jobs = self.tmp / "jobs.tsv"
        self.jobs.write_text(f"https://example.invalid/run.mzML\t{self.tmp / 'run.mzpeak'}\t--no-vendor\n")

    def checkout(self, where: Path) -> Path:
        """A copy of box_convert.sh in `where`/tools; -> that tools directory."""
        tools = where / "tools"
        tools.mkdir(parents=True, exist_ok=True)
        (tools / "box_convert.sh").write_text((TOOLS / "box_convert.sh").read_text())
        return tools

    def run_script(self, tools: Path, **env: str) -> tuple[int, str]:
        clean = {k: v for k, v in os.environ.items() if not k.startswith(("BOX_", "S3_", "MZPC_"))}
        p = subprocess.run(["bash", str(tools / "box_convert.sh"), "--manifest", str(self.jobs)],
                           capture_output=True, text=True, cwd=self.tmp, env={
                               **clean, "PATH": f"{self.tmp / 'bin'}:{os.environ['PATH']}", "T": str(self.tmp),
                               "TMPDIR": str(self.tmp), "MZPC_PYTHON": str(self.tmp / "bin" / "relay"), **env})
        return p.returncode, p.stdout + p.stderr

    def dispatched(self) -> bool:
        return "-JobFile" in (self.tmp / "ssh").read_text()

    BOX = {"BOX_SSH": "user@box", "BOX_JUMP": "user@jump", "BOX_SSH_KEY": "/dev/null",
           "BOX_AUTOUPDATE": "0", "BOX_CONVERTER_VERSION": "v0.11.5", "BOX_REQUIRE_VERSION": "1"}

    def test_a_box_on_another_version_gets_no_job(self):
        tools = self.checkout(self.tmp / "plain")
        rc, out = self.run_script(tools, **self.BOX, BOX_REPORTS="0.11.4")
        self.assertEqual(rc, 3, out)
        self.assertIn("but this run requires 0.11.5", out)
        self.assertFalse(self.dispatched(), out)

        (self.tmp / "ssh").unlink()
        rc, out = self.run_script(tools, **self.BOX, BOX_REPORTS="0.11.5")
        self.assertTrue(self.dispatched(), out)

    def worktree(self) -> tuple[Path, Path, Path]:
        """A main checkout with a tools/box.env and a git worktree of it; -> (main, worktree, box.env)."""
        git = ["git", "-c", "user.name=t", "-c", "user.email=t@t", "-c", "init.defaultBranch=main"]
        main, wt = self.tmp / "main", self.tmp / "wt"
        subprocess.run([*git, "init", "-q", str(main)], check=True)
        subprocess.run([*git, "-C", str(main), "commit", "-q", "--allow-empty", "-m", "init"], check=True)
        subprocess.run([*git, "-C", str(main), "worktree", "add", "-q", "--detach", str(wt)], check=True)
        env_file = self.checkout(main) / "box.env"
        env_file.write_text("".join(f"{k}={v}\n" for k, v in self.BOX.items()))
        return main, wt, env_file

    def test_a_worktree_uses_the_main_checkouts_box_env_and_says_so(self):
        main, wt, env_file = self.worktree()
        rc, out = self.run_script(self.checkout(wt), BOX_REPORTS="0.11.5")
        self.assertIn(f"using the main checkout's {env_file.resolve()}", out)
        self.assertTrue(self.dispatched(), out)
        self.assertIn("user@box", (self.tmp / "ssh").read_text())

        (self.tmp / "ssh").unlink()
        (wt / "tools" / "box.env").write_text(env_file.read_text())
        rc, out = self.run_script(wt / "tools", BOX_REPORTS="0.11.5")
        self.assertNotIn("main checkout", out, "a worktree's own box.env wins")
        self.assertTrue(self.dispatched(), out)

    def test_the_scp_tool_from_a_worktree_uses_the_main_checkouts_box_env_too(self):
        # box_convert_scp.sh sourced only its own checkout's box.env and died on BOX_SSH:? in a worktree.
        main, wt, env_file = self.worktree()
        tools = wt / "tools"
        tools.mkdir(parents=True, exist_ok=True)
        (tools / "box_convert_scp.sh").write_text((TOOLS / "box_convert_scp.sh").read_text())
        clean = {k: v for k, v in os.environ.items() if not k.startswith(("BOX_", "S3_", "MZPC_"))}
        p = subprocess.run(["bash", str(tools / "box_convert_scp.sh")], input="", capture_output=True, text=True,
                           cwd=self.tmp, env={**clean, "PATH": f"{self.tmp / 'bin'}:{os.environ['PATH']}",
                                              "T": str(self.tmp)})
        out = p.stdout + p.stderr
        self.assertEqual(p.returncode, 0, out)
        self.assertIn(f"using the main checkout's {env_file.resolve()}", out)
        self.assertIn("SCP-CONVERT DONE", out)


if __name__ == "__main__":
    unittest.main()
