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
# "converts" every job the way the box does, native first with the lane flags stripped. A unit named
# *undelivered* never comes back; one named *stale* comes back built by another converter version.
FAKE_BOX = """#!/usr/bin/env bash
while [ "$1" != "--local-manifest" ]; do shift; done
mf="$2"; cp "$mf" "$FAKE_MANIFEST"; rc=0
while IFS="$(printf '\\t')" read -r unit out opts; do
  case "$unit$out" in *undelivered*|*s3://*) rc=1; continue ;; esac
  ver="{version}"; case "$unit" in *stale*) ver=0.0.1 ;; esac
  "$MZPEAK_CONVERT" --write-archive "$out" "$ver" "$(basename "$unit") --no-vendor -o out.mzpeak --force"
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

    def test_box_archives_are_stamped_from_their_own_index(self):
        lane = {"input": "auto", "flags": "--via-msconvert --tof-grid auto"}
        root = make_corpus(self.tmp, {
            "general-ms/sciex/sciex.yaml": {"convert": lane},
            "general-ms/old/old.yaml": {"convert": {"input": "auto"}},
        }, {
            "general-ms/sciex/run.wiff": b"x",
            "general-ms/sciex/run.wiff.scan": b"x",
            "general-ms/old/stale.wiff": b"x",
        })
        rc, out = self.run_main(root, "--box", "--no-s3-first")
        self.assertEqual(self.manifest()["run.mzpeak"][1], "--via-msconvert --tof-grid auto")
        self.assertEqual((root / "general-ms/sciex/run.mzpeak.built").read_text().splitlines(),
                         [f"mzpeak-convert {VERSION}", f"recipe {cr.recipe_id(lane)}",
                          "options run.wiff --no-vendor -o out.mzpeak --force"])
        self.assertTrue((root / "general-ms/old/stale.mzpeak").exists())
        self.assertFalse((root / "general-ms/old/stale.mzpeak.built").exists(),
                         "an archive another converter version built was stamped current")
        self.assertIn("built by mzpeak-convert 0.0.1", out)


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
        self.assertIn("BOX NOT DELIVERED: 2 unit(s), box_convert exit 1", out)
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
# canned BOXRESULT (and records Remove-Item calls), `relay` stands in for s3_relay.py, and `scp`
# copies a local file.
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
    *) cat > "$T/job.json"; printf '<<<BOXRESULT\n%s\nBOXRESULT>>>\n' "$RESULT_B64" ;;
  esac
}
scp(){ printf '%s\n' "${@: -2:1}" >> "$T/scp"; cp "$FAKE_OBJECT" "${@: -1}"; }
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
        script = DRIVER + shell_functions("run_job") + '\nrun_job raw "$OUT" "$OPTS" box-convert/k.mzpeak 0123abcd; echo "rc=$?"\n'
        return self.bash(script, **env, FAKE_OBJECT=str(self.tmp / "object"), OUT=out, OPTS=opts,
                         RESULT_B64=base64.b64encode(json.dumps(result).encode()).decode())

    def test_the_bench_row_names_the_options_that_ran(self):
        out, bench = self.tmp / "run.mzpeak", self.tmp / "bench.tsv"
        rc, log = self.run_job({"argv": "--no-vendor"}, str(out), "--no-vendor --via-msconvert --tof-grid auto",
                               BENCH_TSV=str(bench))
        self.assertEqual(rc, 0, log)
        self.assertEqual(out.read_bytes(), b"archive bytes")
        self.assertEqual(bench.read_text().splitlines()[1].split("\t")[3], "--no-vendor")


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


if __name__ == "__main__":
    unittest.main()
