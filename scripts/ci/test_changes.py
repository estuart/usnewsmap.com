"""scripts/ci/changes.sh: the Quickwit, image and web end-to-end jobs run only for what they build or load.

Each test commits a change in a scratch clone of this checkout and runs
changes.sh on it as a push. The lock-file cases resolve packages with cargo
(the registry index, over the network), so they skip without it.
"""

import os
import re
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
CARGO = shutil.which("cargo")


def git(repo: Path, *args: str) -> str:
    return subprocess.run(["git", "-C", str(repo), *args], check=True,
                          capture_output=True, text=True).stdout.strip()


class Quickwit(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.repo = self.tmp / "repo"
        subprocess.run(["git", "clone", "-q", "--no-hardlinks", str(ROOT), str(self.repo)], check=True)
        # The working tree's changes.sh, committed or not.
        shutil.copy(ROOT / "scripts/ci/changes.sh", self.repo / "scripts/ci/changes.sh")
        self.commit("this changes.sh")

    def tearDown(self):
        shutil.rmtree(self.tmp)

    def commit(self, message: str) -> str:
        git(self.repo, "add", "-A")
        git(self.repo, "-c", "user.name=t", "-c", "user.email=t@example.com",
            "commit", "-q", "--allow-empty", "-m", message)
        return git(self.repo, "rev-parse", "HEAD")

    def outputs(self) -> dict:
        before = git(self.repo, "rev-parse", "HEAD")
        self.change()
        after = self.commit("change")
        out = self.tmp / "out"
        out.write_text("")
        env = dict(os.environ, GITHUB_EVENT_NAME="push", BEFORE_SHA=before,
                   GITHUB_SHA=after, GITHUB_OUTPUT=str(out))
        subprocess.run(["scripts/ci/changes.sh"], cwd=self.repo, env=env, check=True,
                       capture_output=True, text=True)
        return {k: v == "true" for k, v in re.findall(r"^(\w+)=(\w+)$", out.read_text(), re.M)}

    def quickwit(self) -> bool:
        return self.outputs()["quickwit"]

    def touch(self, path: str):
        f = self.repo / path
        with f.open("a") as h:
            h.write("\n// touched\n")

    def test_a_crate_outside_the_closure_doesnt_run_it(self):
        self.change = lambda: self.touch("crates/usnm-boinc/src/lib.rs")
        self.assertFalse(self.quickwit())

    def test_the_ocr_survey_crate_doesnt_run_it(self):
        self.change = lambda: self.touch("crates/usnm-ocr-survey/src/lib.rs")
        self.assertFalse(self.quickwit())

    def test_a_crate_in_the_closure_runs_it(self):
        self.change = lambda: self.touch("crates/usnm-core/src/lib.rs")
        self.assertTrue(self.quickwit())

    def test_a_boinc_only_change_runs_no_image_or_e2e(self):
        self.change = lambda: self.touch("crates/usnm-boinc/src/lib.rs")
        o = self.outputs()
        self.assertFalse(o["image"])
        self.assertFalse(o["e2e"])

    def test_a_crate_in_the_api_closure_runs_image_and_e2e(self):
        self.change = lambda: self.touch("crates/usnm-api/src/main.rs")
        o = self.outputs()
        self.assertTrue(o["image"])
        self.assertTrue(o["e2e"])

    def test_an_ingest_only_crate_runs_image_not_e2e(self):
        self.change = lambda: self.touch("crates/usnm-ingest/src/lib.rs")
        o = self.outputs()
        self.assertTrue(o["image"])
        self.assertFalse(o["e2e"])

    def test_a_fixture_runs_it(self):
        self.change = lambda: (self.repo / "fixtures/new.txt").write_text("x\n")
        self.assertTrue(self.quickwit())

    @unittest.skipUnless(CARGO, "no cargo")
    def test_a_new_crate_outside_the_closure_doesnt_run_it(self):
        def change():
            crate = self.repo / "crates/zz-new"
            (crate / "src").mkdir(parents=True)
            (crate / "src/lib.rs").write_text("")
            (crate / "Cargo.toml").write_text(
                '[package]\nname = "zz-new"\nversion = "0.1.0"\nedition = "2021"\n\n'
                '[dependencies]\nserde_json.workspace = true\n')
            root = self.repo / "Cargo.toml"
            root.write_text(root.read_text().replace("members = [\n", 'members = [\n    "crates/zz-new",\n', 1))
            subprocess.run([CARGO, "metadata", "--format-version", "1", "-q"], cwd=self.repo,
                           check=True, capture_output=True)
            self.assertIn('name = "zz-new"', (self.repo / "Cargo.lock").read_text())
        self.change = change
        self.assertFalse(self.quickwit())

    def test_a_profile_setting_runs_it(self):
        def change():
            with (self.repo / "Cargo.toml").open("a") as h:
                h.write("\n[profile.test]\noverflow-checks = false\n")
        self.change = change
        self.assertTrue(self.quickwit())

    @unittest.skipUnless(CARGO, "no cargo")
    def test_a_dependency_bump_under_usnm_search_runs_it(self):
        def change():
            lock = (self.repo / "Cargo.lock").read_text()
            version = re.search(r'name = "thiserror"\nversion = "(2\.0\.(\d+))"', lock)
            older = f"2.0.{int(version.group(2)) - 1}"
            subprocess.run([CARGO, "update", "-q", "-p", f"thiserror@{version.group(1)}",
                            "--precise", older], cwd=self.repo, check=True, capture_output=True)
        self.change = change
        self.assertTrue(self.quickwit())


if __name__ == "__main__":
    unittest.main()
