#!/usr/bin/env python3
"""Offline checks for the corpus harness: tools/corpus_reconvert.py and the host half of
tools/box_convert.sh.

Nothing here reaches the box, S3 or the published corpus. The converter and box_convert.sh are
stand-ins, and every corpus is a temporary directory.

    python3 tools/test_harness.py          # the descriptor tests need PyYAML
"""
import contextlib
import io
import json
import os
import sys
import tempfile
import unittest
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
# version and argv, the way the real binary's add_processing_metadata does.
FAKE_CONVERTER = """#!{python}
import json, os, sys, zipfile
args = sys.argv[1:]
if args == ["--version"]:
    print("mzpeak-convert {version}"); sys.exit(0)
if args[0].endswith(".wiff"):
    print("error: SciEX .wiff input is available only on Windows", file=sys.stderr); sys.exit(1)
with open(os.environ["FAKE_LOG"], "a") as log:
    log.write(json.dumps(args) + "\\n")
out = args[args.index("-o") + 1]
index = {{"metadata": {{
    "software_list": [{{"id": "mzpeak-convert", "version": "{version}"}}],
    "data_processing_method_list": [{{"id": "mzpeak_convert_conversion", "methods": [
        {{"order": 1, "parameters": [{{"name": "conversion options", "value": " ".join(args)}}]}}]}}]}}}}
with zipfile.ZipFile(out, "w") as z:
    z.writestr("spectra_metadata_scans.parquet", b"")
    z.writestr("mzpeak_index.json", json.dumps(index))
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
        self.env = mock.patch.dict(os.environ, {"MZPEAK_CONVERT": str(fake), "FAKE_LOG": str(self.log)})
        self.env.start()

    def tearDown(self):
        self.env.stop()
        self._tmp.cleanup()

    def run_main(self, root: Path, *extra: str) -> tuple[int, str]:
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            rc = cr.main([str(root), "--jobs", "1", *extra])
        return rc, buf.getvalue()

    def converted(self) -> dict[str, list[str]]:
        """Output archive name -> the argv the converter ran it with."""
        runs = [json.loads(line) for line in self.log.read_text().splitlines()] if self.log.exists() else []
        return {Path(a[a.index("-o") + 1]).name: a for a in runs}


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


if __name__ == "__main__":
    unittest.main()
