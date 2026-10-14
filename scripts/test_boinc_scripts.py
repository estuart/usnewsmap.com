"""Tests for scripts/start-boinc-bridge.sh against a stub az.

The stub answers the calls the script makes and logs each one to STUB_LOG. STUB_TEMPLATE is
the job's template (JSON), STUB_EXEC_STATUS an execution's status, and STUB_NO_JOB=1 gives a
stack with no bridge job. The settings are the committed contrib-compute/infra/boinc/env/prod.json,
read only."""

import json
import os
import re
import shutil
import subprocess
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
SCRIPT = os.path.join(HERE, "start-boinc-bridge.sh")
SUB = json.load(open(os.path.join(ROOT, "contrib-compute/infra/boinc/env/prod.json")))["subscriptionId"]

STUB_AZ = r"""#!/usr/bin/env bash
printf '%s\n' "$*" >> "$STUB_LOG"
case "$*" in
  "account show"*) echo "$STUB_SUB" ;;
  "group show"*) ;;
  "stack group show"*)
    if [ -n "${STUB_NO_JOB:-}" ]; then echo '{}'
    else echo '{"bridgeJobName": {"type": "String", "value": "usnm-boinc-bridge-caj-prod"}}'; fi ;;
  "containerapp job show"*) printf '%s\n' "$STUB_TEMPLATE" ;;
  "containerapp job update"*) ;;
  "containerapp job start"*) echo usnm-boinc-bridge-caj-prod-abc12 ;;
  "containerapp job execution list"*) printf '%s' "${STUB_RUNNING:-}" ;;
  "containerapp job execution show"*"--query properties.status"*) echo "${STUB_EXEC_STATUS:-Succeeded}" ;;
  "containerapp job execution show"*"properties.template"*)
    if [ -n "${STUB_EXEC_ENV:-}" ]; then printf '%s\n' "$STUB_EXEC_ENV"
    else grep '^containerapp job update' "$STUB_LOG" | tail -n 1 |
      sed -e 's/.*--replace-env-vars //' -e 's/ -o none.*//' | tr ' ' '\n' |
      jq -R 'split("=") | {name: .[0], value: (.[1:] | join("="))}' | jq -s .; fi ;;
  *) echo "stub az: unexpected call: $*" >&2; exit 9 ;;
esac
"""

BASE = {
    "AZURE_CLIENT_ID": "client-1",
    "KEY_VAULT_NAME": "usnmboinckvprod",
    "DB_HOST": "db.example.org",
    "DB_NAME": "usnmocr",
    "MASTER_HOST": "boinc.usnewsmap.com",
    "PROJECT_DIR": "/project",
    "HANDOFF_DIR": "/project/handoff",
}


def template(extra=None, drop=None):
    env = [{"name": k, "value": v} for k, v in BASE.items() if k != drop]
    env += [{"name": k, "value": v} for k, v in (extra or {}).items()]
    return json.dumps({"containers": [{"name": "bridge", "image": "x.azurecr.io/b@sha256:" + "0" * 64,
                                       "env": env, "volumeMounts": [{"volumeName": "project"}]}],
                       "volumes": [{"name": "project"}]})


class StartBridge(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        bin_dir = os.path.join(self.tmp, "bin")
        os.mkdir(bin_dir)
        with open(os.path.join(bin_dir, "az"), "w") as f:
            f.write(STUB_AZ)
        os.chmod(os.path.join(bin_dir, "az"), 0o755)
        self.log = os.path.join(self.tmp, "az.log")
        self.env = {"PATH": bin_dir + os.pathsep + os.environ["PATH"], "HOME": self.tmp,
                    "STUB_LOG": self.log, "STUB_SUB": SUB, "STUB_TEMPLATE": template()}

    def tearDown(self):
        shutil.rmtree(self.tmp)

    def run_script(self, *args, **extra):
        return subprocess.run([SCRIPT, "--env", "prod", *args], cwd=ROOT, env={**self.env, **extra},
                              capture_output=True, text=True)

    def calls(self):
        if not os.path.exists(self.log):
            return []
        with open(self.log) as f:
            return f.read().splitlines()

    def update_vars(self):
        [call] = [c for c in self.calls() if c.startswith("containerapp job update")]
        tail = call.split("--replace-env-vars ", 1)[1].split(" -o none", 1)[0]
        return tail.split(" ")

    def test_sets_base_settings_and_step_then_starts_without_override(self):
        r = self.run_script("--step", "gold-prepare", "--var", "GOLD_CAMPAIGN=v1-tess550")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(r.stdout, "usnm-boinc-bridge-caj-prod-abc12\n")
        self.assertEqual(self.update_vars(), [f"{k}={v}" for k, v in BASE.items()]
                         + ["BRIDGE_STEP=gold-prepare", "GOLD_CAMPAIGN=v1-tess550"])
        update = [c for c in self.calls() if c.startswith("containerapp job update")][0]
        self.assertIn("--container-name bridge", update)
        self.assertIn(f"--subscription {SUB}", update)
        [start] = [c for c in self.calls() if c.startswith("containerapp job start")]
        for flag in ("--env-vars", "--image", "--args", "--command", "--container-name"):
            self.assertNotIn(flag, start)
        order = [c.split(" ")[2] for c in self.calls() if c.startswith("containerapp job ")]
        self.assertEqual(order, ["show", "execution", "update", "start", "execution"])

    def test_drops_a_previous_runs_settings(self):
        r = self.run_script("--step", "register-apps",
                            STUB_TEMPLATE=template({"BRIDGE_STEP": "submit", "JOB_ID": "old"}))
        self.assertEqual(r.returncode, 0, r.stderr)
        vars_ = self.update_vars()
        self.assertIn("BRIDGE_STEP=register-apps", vars_)
        self.assertFalse([v for v in vars_ if v.startswith("JOB_ID=") or v == "BRIDGE_STEP=submit"])

    def test_refuses_to_override_the_jobs_own_settings(self):
        for bad in ("PROJECT_DIR=/tmp", "BRIDGE_STEP=submit", "lower=1", "NOEQUALS"):
            r = self.run_script("--step", "submit", "--var", bad)
            self.assertEqual(r.returncode, 1, bad)
        self.assertEqual(self.calls(), [])

    def test_refuses_a_repeated_var(self):
        r = self.run_script("--step", "submit", "--var", "JOB_ID=a", "--var", "JOB_ID=b")
        self.assertEqual(r.returncode, 1)
        self.assertIn("given twice", r.stderr)

    def test_stops_when_a_base_setting_is_missing(self):
        r = self.run_script("--step", "submit", STUB_TEMPLATE=template(drop="DB_HOST"))
        self.assertEqual(r.returncode, 1)
        self.assertIn("no DB_HOST", r.stderr)
        self.assertFalse([c for c in self.calls() if "job update" in c or "job start" in c])

    def test_no_bridge_job(self):
        r = self.run_script("--step", "submit", STUB_NO_JOB="1")
        self.assertEqual(r.returncode, 1)
        self.assertIn("no bridge job", r.stderr)

    def test_refuses_while_an_execution_runs(self):
        r = self.run_script("--step", "register-apps", STUB_RUNNING="usnm-boinc-bridge-caj-prod-old11\n")
        self.assertEqual(r.returncode, 1)
        self.assertIn("still running usnm-boinc-bridge-caj-prod-old11", r.stderr)
        self.assertFalse([c for c in self.calls() if "job update" in c or "job start" in c])

    def test_fails_when_the_execution_has_another_runs_settings(self):
        r = self.run_script("--step", "register-apps",
                            STUB_EXEC_ENV='[{"name": "BRIDGE_STEP", "value": "submit"}]')
        self.assertEqual(r.returncode, 1)
        self.assertIn("did not start with this run's settings", r.stderr)
        self.assertIn("job stop", r.stderr)

    def test_what_if_changes_nothing(self):
        r = self.run_script("--step", "register-apps", "--what-if")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("BRIDGE_STEP=register-apps", r.stdout)
        self.assertFalse([c for c in self.calls() if "job update" in c or "job start" in c])

    def test_wait_reports_a_failed_execution(self):
        r = self.run_script("--step", "register-apps", "--wait-mins", "5", STUB_EXEC_STATUS="Failed")
        self.assertEqual(r.returncode, 1)
        self.assertIn("ended Failed", r.stderr)
        r = self.run_script("--step", "register-apps", "--wait-mins", "5")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(r.stdout, "usnm-boinc-bridge-caj-prod-abc12\n")


class BaseVarsMatchBicep(unittest.TestCase):
    def test_lists_agree(self):
        with open(SCRIPT) as f:
            m = re.search(r"^BASE_VARS=\(([^)]*)\)", f.read(), re.M)
        script = m.group(1).split()
        with open(os.path.join(ROOT, "contrib-compute/infra/boinc/modules/bridge-job.bicep")) as f:
            bicep = re.findall(r"\{ name: '([A-Z_]+)', value:", f.read())
        self.assertEqual(sorted(script), sorted(bicep))


if __name__ == "__main__":
    unittest.main()
