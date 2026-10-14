"""Tests for gold-bundle.sh with stand-ins for gold.py and usnm-ocr-survey.

The stand-in gold.py logs its arguments and writes a marker per dataset; the stand-in survey
writes a tar whose bytes are STUB_TAR_BODY and a lock that is STUB_LOCK_BODY, and reports
STUB_MANIFEST as the manifest's sha256. The script must install the tar only when all three
checks pass."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

HERE = Path(__file__).resolve().parent
SCRIPT = HERE / "gold-bundle.sh"
ROOT = HERE.parents[2]

GOLD_PY = r"""#!/usr/bin/env python3
import os, sys
with open(os.environ["STUB_LOG"], "a") as f:
    f.write("gold " + " ".join(sys.argv[1:]) + "\n")
a = sys.argv[1:]
cmd, ds = a[0], a[a.index("--dataset") + 1]
if ds == os.environ.get("STUB_FAIL_DATASET"):
    sys.exit(1)
d = a[a.index("--src" if cmd == "public-fetch" else "--out") + 1]
os.makedirs(d, exist_ok=True)
open(os.path.join(d, cmd), "w").write(ds)
"""

SURVEY = r"""#!/usr/bin/env python3
import hashlib, json, os, sys
a = sys.argv[1:]
with open(os.environ["STUB_LOG"], "a") as f:
    f.write("survey " + " ".join(a) + "\n")
body = os.environ["STUB_TAR_BODY"].encode()
man = os.environ["STUB_MANIFEST"]
if a[0] == "bundle-build":
    out = a[a.index("--out") + 1]
    os.makedirs(out, exist_ok=True)
    tar = os.path.join(out, "usnm-ocr-gold-v1.tar")
    open(tar, "wb").write(body)
    open(a[a.index("--lock") + 1], "w").write(os.environ["STUB_LOCK_BODY"])
    print(json.dumps({"tar": tar, "tar_sha256": hashlib.sha256(body).hexdigest(), "manifest_sha256": man}))
else:
    print(json.dumps({"verified": True, "manifest_sha256": man}))
"""


class GoldBundle(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.share = self.root / "share"
        self.share.mkdir()
        self.log = self.root / "log"
        for name, body in (("gold.py", GOLD_PY), ("survey", SURVEY)):
            p = self.root / name
            p.write_text(body)
            p.chmod(0o755)
        self.lock = self.root / "lock.json"
        self.lock.write_text('{"bundle_version": 1}\n')
        self.body = "tar bytes"
        self.tar_sha = hashlib.sha256(self.body.encode()).hexdigest()
        self.manifest = "b" * 64

    def tearDown(self):
        self.tmp.cleanup()

    def run_script(self, tar_sha=None, manifest=None, lock_body=None, **env):
        e = {**os.environ, "STUB_LOG": str(self.log), "STUB_TAR_BODY": self.body,
             "STUB_MANIFEST": self.manifest, "STUB_LOCK_BODY": lock_body or self.lock.read_text(), **env}
        return subprocess.run([str(SCRIPT), "--share", str(self.share), "--campaign", "v1-tess550",
                               "--gold-py", str(self.root / "gold.py"), "--survey", str(self.root / "survey"),
                               "--lock", str(self.lock), "--tar-sha256", tar_sha or self.tar_sha,
                               "--manifest-sha256", manifest or self.manifest],
                              env=e, capture_output=True, text=True)

    def dest(self):
        return self.share / "gold" / "v1-tess550" / "bundle.tar"

    def calls(self):
        return self.log.read_text().splitlines() if self.log.exists() else []

    def test_builds_checks_and_installs(self):
        r = self.run_script()
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(self.dest().read_text(), self.body)
        calls = self.calls()
        datasets = ["bln600", "overproof-ca", "newseye-bnf", "newseye-nlf-fi", "newseye-nlf-sv", "nzz"]
        fetches = [c for c in calls if c.startswith("gold public-fetch")]
        self.assertEqual([c.split("--dataset ")[1].split()[0] for c in fetches], datasets)
        for c in fetches:
            self.assertIn("--confirm-download", c)
        self.assertIn("--pages 20", fetches[1])
        converts = {c.split("--dataset ")[1].split()[0]: c for c in calls if c.startswith("gold public-convert")}
        self.assertIn("--count 600 --per-page 1", converts["bln600"])
        self.assertIn("--count 600 --per-page 50", converts["overproof-ca"])
        [build] = [c for c in calls if c.startswith("survey bundle-build")]
        for want in ("--version 1", "--seed 457", "--per-source 30", "--original bln600="):
            self.assertIn(want, build)
        for ds in datasets:
            self.assertIn(f"--source {ds}=", build)
        self.assertTrue([c for c in calls if c.startswith("survey bundle-verify")])
        # A rerun finds the bundle installed and builds nothing.
        os.remove(self.log)
        r = self.run_script()
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(self.calls(), [])

    def test_refuses_another_tar_sha(self):
        r = self.run_script(tar_sha="c" * 64)
        self.assertEqual(r.returncode, 1)
        self.assertIn("the tar's sha256 is", r.stderr)
        self.assertFalse(self.dest().exists())

    def test_refuses_another_manifest(self):
        r = self.run_script(manifest="d" * 64, STUB_MANIFEST="e" * 64)
        self.assertEqual(r.returncode, 1)
        self.assertIn("manifest's sha256", r.stderr)
        self.assertFalse(self.dest().exists())

    def test_refuses_a_different_lock(self):
        r = self.run_script(lock_body='{"bundle_version": 2}\n')
        self.assertEqual(r.returncode, 1)
        self.assertIn("differs from the committed lock", r.stderr)
        self.assertFalse(self.dest().exists())

    def test_a_failed_fetch_stops(self):
        r = self.run_script(STUB_FAIL_DATASET="newseye-bnf")
        self.assertEqual(r.returncode, 1)
        self.assertIn("newseye-bnf failed", r.stderr)
        self.assertFalse([c for c in self.calls() if c.startswith("survey")])

    def test_refuses_to_replace_another_bundle(self):
        self.dest().parent.mkdir(parents=True)
        self.dest().write_text("other")
        r = self.run_script()
        self.assertEqual(r.returncode, 1)
        self.assertIn("exists with another sha256", r.stderr)
        self.assertEqual(self.dest().read_text(), "other")

    def test_a_second_run_stops_on_the_lock(self):
        (self.share / "gold" / ".build-v1.lock").mkdir(parents=True)
        r = self.run_script()
        self.assertEqual(r.returncode, 1)
        self.assertIn("holds", r.stderr)
        self.assertEqual(self.calls(), [])
        self.assertTrue((self.share / "gold" / ".build-v1.lock").is_dir())

    def test_the_lock_is_released(self):
        self.assertEqual(self.run_script().returncode, 0)
        self.assertFalse((self.share / "gold" / ".build-v1.lock").exists())

    def test_defaults_are_the_committed_lock(self):
        lock = json.loads((ROOT / "fixtures/ocr-gold/bundle-v1.lock.json").read_text())
        text = SCRIPT.read_text()
        self.assertIn(f"TAR_SHA={lock['tar_sha256']}", text)
        self.assertIn(f"MANIFEST_SHA={lock['manifest_sha256']}", text)
        self.assertEqual(lock["seed"], 457)


if __name__ == "__main__":
    unittest.main()
