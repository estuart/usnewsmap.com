"""Tests for the dev lifecycle pieces of scripts/lib/env.sh and for
scripts/dev-status.sh (#275), against a stub `az` (python3 -m unittest
scripts/test_env_scripts.py). Nothing here reaches Azure.

The stub answers from environment variables: STUB_STACK (the stack's
resource count; unset: not found), STUB_GROUPS (group names, one per line),
STUB_TAGS (the project group's "experiment<TAB>started"; unset: no group),
STUB_PARAMS (the guard-rail stack's parameters as two tsv rows) and
STUB_COST (Cost Management's answer; STUB_COST_ERROR makes it fail with that
message). It logs each call to STUB_LOG."""

import json
import os
import re
import shutil
import subprocess
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)

STUB_AZ = r"""#!/usr/bin/env bash
printf '%s\n' "$*" >> "$STUB_LOG"
case "$1 $2" in
  "account show") case "$*" in *"--query id"*) echo sub-1 ;; esac ;;
  "stack sub")
    case "$*" in
      "stack sub create"*) ;;
      *"--query location"*) echo eastus2 ;;
      *"keys(parameters"*) printf '%s\n' "${STUB_PARAMS:-}" ;;
      *) if [ -n "${STUB_STACK:-}" ]; then echo "$STUB_STACK"
         else echo "ERROR: DeploymentStackNotFound: usnm-dev could not be found." >&2; exit 3; fi ;;
    esac ;;
  "group list") [ -z "${STUB_GROUPS:-}" ] || printf '%s\n' "$STUB_GROUPS" ;;
  "group show")
    if [ -n "${STUB_TAGS+x}" ]; then printf '%b\n' "$STUB_TAGS"
    else echo "ERROR: (ResourceGroupNotFound) Resource group 'rg-usnm-dev' could not be found." >&2; exit 3; fi ;;
  "resource list") printf 'Microsoft.Storage/storageAccounts\tstusnmdev\n' ;;
  "rest --method")
    if [ -n "${STUB_COST_ERROR:-}" ]; then echo "$STUB_COST_ERROR" >&2; exit 1; fi
    printf '%s\n' "${STUB_COST:-}" ;;
  *) echo "stub az: unexpected: $*" >&2; exit 9 ;;
esac
"""

COST = json.dumps({"properties": {
    "columns": [{"name": "Cost"}, {"name": "ResourceGroupName"}, {"name": "Currency"}],
    "rows": [[1.25, "rg-usnm-dev", "USD"], [0.5, "me_cae-usnm-dev_rg-usnm-dev_eastus2", "USD"],
             [40.0, "rg-usnm-prod", "USD"], [0.1, "rg-usnm-dev2", "USD"]]}})


class Stubbed(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        bin_dir = os.path.join(self.tmp, "bin")
        os.mkdir(bin_dir)
        with open(os.path.join(bin_dir, "az"), "w") as f:
            f.write(STUB_AZ)
        os.chmod(os.path.join(bin_dir, "az"), 0o755)
        # Settings in a directory of the test's own, never a checkout's.
        self.settings = os.path.join(self.tmp, "settings")
        os.makedirs(os.path.join(self.settings, "dev"))
        self.log = os.path.join(self.tmp, "az.log")
        self.env = {"PATH": bin_dir + os.pathsep + os.environ["PATH"], "HOME": self.tmp,
                    "USNM_SETTINGS_HOME": self.settings, "STUB_LOG": self.log}

    def tearDown(self):
        shutil.rmtree(self.tmp)

    def settings_file(self, text, env="dev"):
        os.makedirs(os.path.join(self.settings, env), exist_ok=True)
        with open(os.path.join(self.settings, env, ".env"), "w") as f:
            f.write(text)

    def lib(self, script, env_name="dev", **extra):
        """Runs `script` with scripts/lib/env.sh sourced, as the scripts do."""
        prog = f'set -euo pipefail\ndie() {{ echo "error: $*" >&2; exit 1; }}\nENV_NAME={env_name}\n' \
               f'. scripts/lib/env.sh\n{script}\n'
        return subprocess.run(["bash", "-c", prog], cwd=ROOT, env={**self.env, **extra},
                              capture_output=True, text=True)

    def calls(self):
        with open(self.log) as f:
            return f.read().splitlines()


class SettingsLocation(Stubbed):
    def test_the_shared_directory_wins(self):
        r = self.lib('echo "$ENV_FILE"')
        self.assertEqual(r.stdout.strip(), os.path.join(self.settings, "dev", ".env"))

    def test_without_it_the_checkout_holds_them(self):
        r = self.lib('echo "$ENV_FILE"', env_name="nosuchenv")
        self.assertEqual(r.stdout.strip(), ".azure/nosuchenv/.env")


class ReservedNames(Stubbed):
    def test_other_stacks_names_are_reserved(self):
        # usnm-archive and usnm-guardrails are stacks of their own.
        for name in ("archive", "guardrails"):
            r = self.lib("true", env_name=name)
            self.assertEqual(r.returncode, 2, name)
            self.assertIn("reserved", r.stderr)


class ExperimentMarker(Stubbed):
    def test_dev_needs_an_experiment(self):
        self.settings_file('AZURE_ENV_NAME="dev"\n')
        r = self.lib("experiment_marker")
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("USNM_EXPERIMENT", r.stderr)

    def test_teardown_needs_none(self):
        self.settings_file('AZURE_ENV_NAME="dev"\n')
        r = self.lib("experiment_marker", NO_EXPERIMENT_CHECK="true")
        self.assertEqual(r.returncode, 0, r.stderr)

    def test_teardown_reads_no_marker(self):
        # Not even a malformed one, nor the group's tags.
        self.settings_file('USNM_EXPERIMENT="search"\n')
        r = self.lib("experiment_marker", NO_EXPERIMENT_CHECK="true")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertFalse(os.path.exists(self.log) and any(c.startswith("group show") for c in self.calls()))

    def test_other_environments_need_none(self):
        r = self.lib("experiment_marker", env_name="prod")
        self.assertEqual(r.returncode, 0, r.stderr)

    def test_an_issue_number(self):
        self.settings_file('USNM_EXPERIMENT="search"\n')
        r = self.lib("experiment_marker")
        self.assertNotEqual(r.returncode, 0)

    def test_a_new_group_starts_now(self):
        self.settings_file('USNM_EXPERIMENT="275"\n')
        r = self.lib("experiment_marker; aget USNM_EXPERIMENT_STARTED")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertRegex(r.stdout.strip(), r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ$")

    def test_the_same_experiment_keeps_its_start(self):
        self.settings_file('USNM_EXPERIMENT="275"\n')
        r = self.lib("experiment_marker; aget USNM_EXPERIMENT_STARTED",
                     STUB_TAGS="275\\t2026-10-01T12:00:00Z")
        self.assertEqual(r.stdout.strip(), "2026-10-01T12:00:00Z")

    def test_another_experiment_starts_again(self):
        self.settings_file('USNM_EXPERIMENT="301"\n')
        r = self.lib("experiment_marker; aget USNM_EXPERIMENT_STARTED",
                     STUB_TAGS="275\\t2026-10-01T12:00:00Z")
        self.assertNotEqual(r.stdout.strip(), "2026-10-01T12:00:00Z")
        self.assertRegex(r.stdout.strip(), r"^\d{4}-")


class GuardrailBudget(Stubbed):
    def create_call(self):
        return [c for c in self.calls() if c.startswith("stack sub create")][0]

    def test_a_new_budget_starts_this_month(self):
        self.settings_file('USNM_ALERT_EMAILS="ops@example.org"\n')
        r = self.lib("deploy_env_guardrails")
        self.assertEqual(r.returncode, 0, r.stderr)
        call = self.create_call()
        self.assertIn("devBudgetEmails=ops@example.org", call)
        self.assertRegex(call, r"devBudgetStart=\d{4}-\d\d-01 ")

    def test_an_environment_without_recipients_keeps_the_budget(self):
        self.settings_file('AZURE_ENV_NAME="dev"\n')
        r = self.lib("deploy_env_guardrails",
                     STUB_PARAMS="devbudgetEmails\tDevBudgetStart\nops@example.org\t2026-10-01")
        self.assertEqual(r.returncode, 0, r.stderr)
        call = self.create_call()
        self.assertIn("devBudgetEmails=ops@example.org devBudgetStart=2026-10-01 ", call)

    def test_new_recipients_keep_the_start(self):
        self.settings_file('USNM_ALERT_EMAILS="new@example.org"\n')
        self.lib("deploy_env_guardrails",
                 STUB_PARAMS="devBudgetEmails\tdevBudgetStart\nops@example.org\t2026-10-01")
        self.assertIn("devBudgetEmails=new@example.org devBudgetStart=2026-10-01 ", self.create_call())


    def test_dev_names_its_region(self):
        self.settings_file('AZURE_LOCATION="westus3"\n')
        self.lib("deploy_env_guardrails")
        self.assertIn("devBudgetLocation=westus3", self.create_call())

    def test_other_environments_keep_the_region(self):
        self.settings_file('AZURE_LOCATION="eastus2"\n', env="prod")
        r = self.lib("deploy_env_guardrails", env_name="prod",
                     STUB_PARAMS="devBudgetEmails\tdevBudgetStart\tdevBudgetLocation\n"
                                 "ops@example.org\t2026-10-01\twestus3")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("devBudgetLocation=westus3", self.create_call())


    def test_the_archive_keeps_the_budget(self):
        # archive-store.sh deploy passes no recipients and no region.
        r = self.lib("deploy_guardrails eastus2", env_name="prod",
                     STUB_PARAMS="devBudgetEmails\tdevBudgetStart\tdevBudgetLocation\n"
                                 "ops@example.org\t2026-10-01\twestus3")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("devBudgetEmails=ops@example.org devBudgetStart=2026-10-01 "
                      "devBudgetLocation=westus3 ", self.create_call())


class DevStatus(Stubbed):
    def status(self, **extra):
        return subprocess.run([os.path.join(HERE, "dev-status.sh"), "--subscription", "sub-1"], cwd=ROOT,
                              env={**self.env, "STUB_COST": COST, **extra}, capture_output=True, text=True)

    def test_nothing_there(self):
        r = self.status()
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("dev holds nothing", r.stdout)

    def test_resources_without_a_tag(self):
        r = self.status(STUB_STACK="120", STUB_GROUPS="rg-usnm-dev\nrg-usnm-dev-spot", STUB_TAGS="\\t")
        self.assertEqual(r.returncode, 1)
        self.assertIn("no experiment tag", r.stderr)

    def test_a_group_left_behind(self):
        r = self.status(STUB_GROUPS="ME_cae-usnm-dev_rg-usnm-dev_eastus2")
        self.assertEqual(r.returncode, 1)

    def test_an_experiment(self):
        r = self.status(STUB_STACK="120", STUB_GROUPS="rg-usnm-dev",
                        STUB_TAGS="275\\t2026-10-01T12:00:00Z")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("experiment #275, started 2026-10-01T12:00:00Z", r.stdout)
        # dev's groups only, whatever their case: not prod's, not dev2's.
        self.assertIn("cost since 2026-10-01T12:00:00Z: 1.75 USD", r.stdout)
        self.assertTrue(re.search(r"group rg-usnm-dev: 1 resources", r.stdout))

    def test_cost_without_the_reader_role_is_a_warning(self):
        r = self.status(STUB_STACK="120", STUB_GROUPS="rg-usnm-dev", STUB_TAGS="275\\t2026-10-01T12:00:00Z",
                        STUB_COST_ERROR="ERROR: (AuthorizationFailed) no access")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("needs Cost Management Reader", r.stderr)

    def test_any_other_cost_error_fails(self):
        r = self.status(STUB_STACK="120", STUB_GROUPS="rg-usnm-dev", STUB_TAGS="275\\t2026-10-01T12:00:00Z",
                        STUB_COST_ERROR="ERROR: (TooManyRequests) throttled")
        self.assertEqual(r.returncode, 1)
        self.assertIn("can't read the cost", r.stderr)


if __name__ == "__main__":
    unittest.main()
