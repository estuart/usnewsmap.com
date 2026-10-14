"""Tests for the nightly update's choice of release (#471):
scripts/ocr-node/update-worker.sh --dry-run against a stub `curl` and a stub
`systemctl` (python3 -m unittest -v scripts/ocr-node/test_update_worker.py).
Needs jq. Nothing here reaches GitHub or changes the machine.

The stub curl answers the releases API from STUB_RELEASES (JSON) and the
leader's status from STUB_LEADER (JSON; unset: the request fails). The
stub systemctl says usnm-ocr-worker.service is enabled unless
STUB_DISABLED is set, and that the units in STUB_ACTIVE are running. The
node's files live under a directory of each test's own
(USNM_OCR_TEST_ROOT)."""

import datetime
import json
import os
import subprocess
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
SCRIPT = os.path.join(HERE, "update-worker.sh")

STUB_CURL = r"""#!/usr/bin/env bash
for a in "$@"; do url=$a; done
case "$url" in
  */releases\?*) cat "$STUB_RELEASES" ;;
  *) [ -n "${STUB_LEADER:-}" ] || exit 7; printf '%s' "$STUB_LEADER" ;;
esac
"""

STUB_SYSTEMCTL = r"""#!/usr/bin/env bash
case "$1" in
  is-enabled) [ -z "${STUB_DISABLED:-}" ] ;;
  is-active) for u in ${STUB_ACTIVE:-}; do [ "$u" != "$3" ] || exit 0; done; exit 3 ;;
  *) exit 1 ;;
esac
"""


def iso(hours_ago):
    t = datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(hours=hours_ago)
    return t.strftime("%Y-%m-%dT%H:%M:%SZ")


def release(tag, hours_ago, draft=False):
    return {"tag_name": tag, "published_at": iso(hours_ago), "draft": draft, "prerelease": False}


OLD = "ocr-worker-20261001-aaaaaaa"
MID = "ocr-worker-20261005-bbbbbbb"
NEW = "ocr-worker-20261012-ccccccc"


class UpdateWorkerTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        d = self.tmp.name
        self.root = os.path.join(d, "root")
        bindir = os.path.join(d, "bin")
        os.makedirs(bindir)
        for name, body in (("curl", STUB_CURL), ("systemctl", STUB_SYSTEMCTL)):
            path = os.path.join(bindir, name)
            with open(path, "w") as f:
                f.write(body)
            os.chmod(path, 0o755)
        self.releases = os.path.join(d, "releases.json")
        self.env = dict(os.environ, PATH=bindir + os.pathsep + os.environ["PATH"],
                        USNM_OCR_TEST_ROOT=self.root, STUB_RELEASES=self.releases)
        self.env.pop("JOURNAL_STREAM", None)
        os.makedirs(os.path.join(self.root, "usr/local/bin"))
        os.makedirs(os.path.join(self.root, "var/lib/usnm-ocr-update"))

    def tearDown(self):
        self.tmp.cleanup()

    def current(self, tag):
        lib = os.path.join(self.root, "usr/local/lib/usnm-ocr-worker", tag)
        os.makedirs(lib, exist_ok=True)
        os.symlink(os.path.join(lib, "usnm-ocr-worker"), os.path.join(self.root, "usr/local/bin/usnm-ocr-worker"))

    def reject(self, tag):
        with open(os.path.join(self.root, "var/lib/usnm-ocr-update/rejected"), "a") as f:
            f.write(tag + "\n")

    def run_it(self, releases, *args, **env):
        with open(self.releases, "w") as f:
            json.dump(releases, f)
        p = subprocess.run([SCRIPT, "--dry-run", *args], env=dict(self.env, **env),
                           capture_output=True, text=True, check=False)
        return p.returncode, p.stdout + p.stderr

    def test_takes_the_newest_release(self):
        self.current(OLD)
        rc, out = self.run_it([release(NEW, 30), release(MID, 200), release(OLD, 300)], "--delay-hours", "0")
        self.assertEqual(rc, 0, out)
        self.assertIn(f"would update from {OLD} to {NEW}", out)

    def test_up_to_date(self):
        self.current(NEW)
        rc, out = self.run_it([release(NEW, 30), release(OLD, 300)], "--delay-hours", "0")
        self.assertEqual(rc, 0, out)
        self.assertIn(f"up to date: {NEW}", out)

    def test_delay_holds_a_new_release_back(self):
        self.current(OLD)
        rc, out = self.run_it([release(NEW, 5), release(MID, 200), release(OLD, 300)], "--delay-hours", "24")
        self.assertEqual(rc, 0, out)
        self.assertIn(f"would update from {OLD} to {MID}", out)

    def test_skips_rejected_drafts_and_other_tags(self):
        self.current(OLD)
        self.reject(NEW)
        rels = [release(NEW, 30), release("ocr-worker-20261013-ddddddd", 2, draft=True),
                release("v1.2.3", 1), release(MID, 200), release(OLD, 300)]
        rc, out = self.run_it(rels, "--delay-hours", "0")
        self.assertEqual(rc, 0, out)
        self.assertIn(f"would update from {OLD} to {MID}", out)

    def test_keeps_a_newer_release_installed_by_hand(self):
        self.current(NEW)
        self.reject(NEW)
        rc, out = self.run_it([release(NEW, 30), release(OLD, 300)], "--delay-hours", "0")
        self.assertEqual(rc, 0, out)
        self.assertIn(f"{NEW} is newer than {OLD}", out)

    def test_waits_for_the_leader(self):
        self.current(OLD)
        rels = [release(NEW, 30), release(OLD, 300)]
        leader = "http://leader.example.org:8381/status"
        cases = [
            ({"version": "0.1.0 (commit aaaaaaa1234)", "started_at": iso(48)}, "the leader runs 0.1.0"),
            ({"version": "0.1.0 (commit ccccccc1234)", "started_at": iso(0.5)}, "under 2 h"),
        ]
        for status, why in cases:
            rc, out = self.run_it(rels, "--delay-hours", "24", "--leader-status", leader,
                                  STUB_LEADER=json.dumps(status))
            self.assertEqual(rc, 0, out)
            self.assertIn(why, out)
            self.assertNotIn("would update", out)
        ok = {"version": "0.1.0 (commit ccccccc1234)", "started_at": iso(3).replace("Z", ".123456Z")}
        rc, out = self.run_it(rels, "--delay-hours", "24", "--leader-status", leader, STUB_LEADER=json.dumps(ok))
        self.assertEqual(rc, 0, out)
        self.assertIn(f"would update from {OLD} to {NEW}", out)

    def test_unreachable_leader_is_a_warning(self):
        self.current(OLD)
        rc, out = self.run_it([release(NEW, 30)], "--delay-hours", "0", "--leader-status",
                              "http://leader.example.org:8381/status")
        self.assertEqual(rc, 1, out)
        self.assertIn("can't read the leader's status", out)

    def test_does_nothing_when_disabled_or_busy(self):
        self.current(OLD)
        rc, out = self.run_it([release(NEW, 30)], "--delay-hours", "0", STUB_DISABLED="1")
        self.assertEqual(rc, 0, out)
        self.assertIn("isn't enabled", out)
        rc, out = self.run_it([release(NEW, 30)], "--delay-hours", "0", STUB_ACTIVE="usnm-ocr-reboot.service")
        self.assertEqual(rc, 0, out)
        self.assertIn("usnm-ocr-reboot.service is running", out)

    def test_named_flags_only(self):
        rc, out = self.run_it([], "24")
        self.assertEqual(rc, 2, out)


if __name__ == "__main__":
    unittest.main()
