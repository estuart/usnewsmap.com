"""Tests for the dev lifecycle pieces of scripts/lib/env.sh and for
scripts/dev-status.sh (#275), against a stub `az` (python3 -m unittest
scripts/test_env_scripts.py). Nothing here reaches Azure.

The stub answers from environment variables: STUB_STACK (the stack's
resource count; unset: not found), STUB_GROUPS (group names, one per line),
STUB_TAGS (the project group's "experiment<TAB>started"; unset: no group),
STUB_TAGGED_GROUP (when set, the only group `group show` finds),
STUB_PARAMS (the guard-rail stack's parameters as two tsv rows) and
STUB_COST (Cost Management's answer; STUB_COST_ERROR makes it fail with that
message). For a job execution (scripts/start-job.sh, and
scripts/titles-seed.sh through it): STUB_EXECUTION (the started execution's name) and
STUB_EXEC_STATUS (its status). It logs each call to STUB_LOG."""

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
    if [ -n "${STUB_TAGGED_GROUP:-}" ] && [ "$4" != "$STUB_TAGGED_GROUP" ]; then
      echo "ERROR: (ResourceGroupNotFound) Resource group '$4' could not be found." >&2; exit 3; fi
    if [ -n "${STUB_TAGS+x}" ]; then printf '%b\n' "$STUB_TAGS"
    else echo "ERROR: (ResourceGroupNotFound) Resource group 'rg-usnm-dev' could not be found." >&2; exit 3; fi ;;
  "resource list") printf 'Microsoft.Storage/storageAccounts\tstusnmdev\n' ;;
  "containerapp job")
    case "$*" in
      *"execution show"*) echo "${STUB_EXEC_STATUS:-Succeeded}" ;;
      *"job show"*) echo '{"containers":[{"name":"ingest","image":"cr/usnewsmap-ingest:exp-1","args":["run"],"env":[]}]}' ;;
      *) echo "stub az: unexpected: $*" >&2; exit 9 ;;
    esac ;;
  "rest --method")
    case "$*" in *"/jobs/"*"/start?"*) echo "${STUB_EXECUTION:-}"; exit 0 ;; esac
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
        r = self.lib('echo "$ENV_FILE"', env_name="x1")
        self.assertEqual(r.stdout.strip(), ".azure/x1/.env")


class EnvironmentNames(Stubbed):
    # ADR-0017: prod, dev, or x plus 1-4 lowercase letters or digits.
    def test_the_closed_list(self):
        for name in ("prod", "dev", "x1", "xab12"):
            r = self.lib("true", env_name=name)
            self.assertEqual(r.returncode, 0, f"{name}: {r.stderr}")

    def test_anything_else_is_refused(self):
        # usnm-archive and usnm-guardrails are stacks of their own, ci is a
        # function word, and longer names overflow the resource names.
        for name in ("archive", "guardrails", "ci", "x", "xabcde", "Prod", "dev2", "staging", "x-1"):
            r = self.lib("true", env_name=name)
            self.assertEqual(r.returncode, 2, name)
            self.assertIn("ADR-0017", r.stderr)

    def test_bootstrap_checks_it_before_signing_in(self):
        # The stub az would log a call; the name is refused first.
        r = subprocess.run([os.path.join(HERE, "bootstrap.sh"), "--env", "staging"], cwd=ROOT,
                           env=self.env, capture_output=True, text=True)
        self.assertEqual(r.returncode, 2)
        self.assertIn("ADR-0017", r.stderr)
        self.assertFalse(os.path.exists(self.log))


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

    def test_the_group_in_either_name_form(self):
        # Dev's project group is usnm-core-rg-dev (ADR-0017); rg-usnm-dev is
        # the legacy form, still looked for.
        self.settings_file('USNM_EXPERIMENT="275"\n')
        for group in ("usnm-core-rg-dev", "rg-usnm-dev"):
            r = self.lib("experiment_marker; aget USNM_EXPERIMENT_STARTED", STUB_TAGGED_GROUP=group,
                         STUB_TAGS="275\\t2026-10-01T12:00:00Z")
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertEqual(r.stdout.strip(), "2026-10-01T12:00:00Z", group)
        shows = [c for c in self.calls() if c.startswith("group show")]
        self.assertTrue(shows[0].startswith("group show -n usnm-core-rg-dev "), shows)
        self.assertTrue(shows[-1].startswith("group show -n rg-usnm-dev "), shows)

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

    def test_an_experiment_in_adr_0017_names(self):
        cost = json.dumps({"properties": {
            "columns": [{"name": "Cost"}, {"name": "ResourceGroupName"}, {"name": "Currency"}],
            "rows": [[1.25, "usnm-core-rg-dev", "USD"], [0.5, "USNM-CORE-INFRA-RG-DEV", "USD"],
                     [0.25, "rg-usnm-dev-spot", "USD"], [40.0, "rg-usnm-prod", "USD"],
                     [0.1, "usnm-core-rg-dev2", "USD"]]}})
        r = self.status(STUB_STACK="120", STUB_GROUPS="usnm-core-rg-dev\nusnm-core-infra-rg-dev",
                        STUB_TAGGED_GROUP="usnm-core-rg-dev", STUB_TAGS="275\\t2026-10-01T12:00:00Z",
                        STUB_COST=cost)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("experiment #275, started 2026-10-01T12:00:00Z", r.stdout)
        self.assertIn("cost since 2026-10-01T12:00:00Z: 2 USD", r.stdout)
        self.assertTrue(re.search(r"group usnm-core-infra-rg-dev: 1 resources", r.stdout))
        query = [c for c in self.calls() if c.startswith("group list")][0]
        for g in ("usnm-core-rg-dev", "usnm-core-infra-rg-dev", "rg-usnm-dev-spot", "rg-usnm-dev",
                  "ME_cae-usnm-dev_"):
            self.assertIn(f"'{g}'", query)

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


class StartQuality(Stubbed):
    # The jobs' names come from the stack's outputs, in either form (ADR-0017).
    SETTINGS = ('AZURE_SUBSCRIPTION_ID="sub-1"\nAZURE_RESOURCE_GROUP="usnm-core-rg-dev"\n'
                'JA_OCR_JOB="usnm-jaocr-caj-dev"\nJA_AUDIT_JOB="usnm-jaone-caj-dev"\n')

    def start(self, *args):
        return subprocess.run([os.path.join(HERE, "ja-ocr", "start-quality.sh"), "--env", "dev", *args],
                              cwd=ROOT, env={**self.env, "STUB_EXECUTION": "usnm-jaocr-caj-dev-abc12"},
                              capture_output=True, text=True)

    def test_the_jobs_come_from_the_settings(self):
        self.settings_file(self.SETTINGS)
        r = self.start("--sample-pct", "10")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertTrue(any(c.startswith("containerapp job show -n usnm-jaocr-caj-dev -g usnm-core-rg-dev")
                            for c in self.calls()))
        self.assertTrue(any("/jobs/usnm-jaocr-caj-dev/start?" in c for c in self.calls()))
        r = self.start("--command", "mixed")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertTrue(any("/jobs/usnm-jaone-caj-dev/start?" in c for c in self.calls()))

    def test_without_the_outputs(self):
        self.settings_file('AZURE_SUBSCRIPTION_ID="sub-1"\nAZURE_RESOURCE_GROUP="rg-usnm-dev"\n')
        r = self.start()
        self.assertEqual(r.returncode, 1)
        self.assertIn("JA_OCR_JOB", r.stderr)
        self.assertFalse(os.path.exists(self.log))


class StartJobWait(Stubbed):
    """scripts/start-job.sh --wait-mins: the teardown runbook's titles-archive
    step must not return before the copy is written (#275)."""
    SETTINGS = ('AZURE_SUBSCRIPTION_ID="sub-1"\nAZURE_RESOURCE_GROUP="rg-usnm-dev"\n'
                'INGEST_JOB="caj-usnm-ingest-dev"\n')

    def start(self, *args, **extra):
        self.settings_file(self.SETTINGS)
        return subprocess.run([os.path.join(HERE, "start-job.sh"), "--env", "dev", "--job", "INGEST_JOB",
                               *args, "--", "titles-archive"], cwd=ROOT,
                              env={**self.env, "STUB_EXECUTION": "caj-usnm-ingest-dev-xyz34", **extra},
                              capture_output=True, text=True)

    def polled(self):
        return [c for c in self.calls() if "execution show" in c]

    def test_without_it_returns_once_started(self):
        r = self.start(STUB_EXEC_STATUS="Running")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(r.stdout, "caj-usnm-ingest-dev-xyz34\n")
        self.assertEqual(self.polled(), [])

    def test_waits_for_success(self):
        r = self.start("--wait-mins", "20")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(r.stdout, "caj-usnm-ingest-dev-xyz34\n")
        self.assertEqual(len(self.polled()), 1)
        self.assertIn("--job-execution-name caj-usnm-ingest-dev-xyz34", self.polled()[0])
        body = json.loads([c for c in self.calls() if c.startswith("rest --method post")][0]
                          .split("--body ", 1)[1].split(" --query", 1)[0])
        self.assertEqual(body["containers"][0]["args"], ["titles-archive"])

    def test_a_failed_execution_fails(self):
        for status in ("Failed", "Stopped", "Degraded"):
            r = self.start("--wait-mins", "20", STUB_EXEC_STATUS=status)
            self.assertEqual(r.returncode, 1, status)
            self.assertIn(f"ended {status}", r.stderr)
            self.assertEqual(r.stdout, "")

    def test_still_running_at_the_deadline_fails(self):
        r = self.start("--wait-mins", "00", STUB_EXEC_STATUS="Running")
        self.assertEqual(r.returncode, 1)
        self.assertIn("still Running after 0 minutes", r.stderr)
        self.assertEqual(r.stdout, "")

    def test_no_execution_fails(self):
        r = self.start("--wait-mins", "20", STUB_EXECUTION="")
        self.assertEqual(r.returncode, 1)
        self.assertIn("started no execution", r.stderr)

    def test_wait_mins_is_a_number(self):
        # An empty value (an unset variable) is refused, not read as no wait.
        for value in ("soon", "", "-5"):
            r = self.start("--wait-mins", value)
            self.assertEqual(r.returncode, 2, repr(value))
            self.assertIn("--wait-mins is a number of minutes", r.stderr)
            self.assertFalse(os.path.exists(self.log))


class TitlesSeed(Stubbed):
    SETTINGS = ('AZURE_SUBSCRIPTION_ID="sub-1"\nAZURE_RESOURCE_GROUP="rg-usnm-dev"\n'
                'INGEST_JOB="caj-usnm-ingest-dev"\n'
                'USNM_ARCHIVE_ACCOUNT="/subscriptions/sub-1/resourceGroups/rg-usnm-archive/providers/'
                'Microsoft.Storage/storageAccounts/stusnmarchivex"\n')

    def seed(self, *args, env="dev", **extra):
        return subprocess.run([os.path.join(HERE, "titles-seed.sh"), "--env", env, *args], cwd=ROOT,
                              env={**self.env, "STUB_EXECUTION": "caj-usnm-ingest-dev-abc12", **extra},
                              capture_output=True, text=True)

    def test_runs_titles_seed_in_the_ingest_job_and_waits(self):
        self.settings_file(self.SETTINGS)
        r = self.seed()
        self.assertEqual(r.returncode, 0, r.stderr)
        start = [c for c in self.calls() if c.startswith("rest --method post")]
        self.assertEqual(len(start), 1)
        self.assertIn("/jobs/caj-usnm-ingest-dev/start?", start[0])
        # Only the arguments change: the job's own image and settings stay.
        body = json.loads(start[0].split("--body ", 1)[1].split(" --query", 1)[0])
        self.assertEqual(body["containers"][0]["args"], ["titles-seed"])
        self.assertEqual(body["containers"][0]["image"], "cr/usnewsmap-ingest:exp-1")
        self.assertTrue(any("--job-execution-name caj-usnm-ingest-dev-abc12" in c for c in self.calls()))
        self.assertIn("--query-name titles-archive", r.stdout)
        # Nothing here touches the archival account or its stack.
        self.assertFalse(any(c.startswith(("storage", "stack", "lock", "role")) for c in self.calls()))

    def test_a_failed_execution_fails(self):
        self.settings_file(self.SETTINGS)
        r = self.seed(STUB_EXEC_STATUS="Failed")
        self.assertEqual(r.returncode, 1)
        self.assertIn("ended Failed", r.stderr)

    def test_still_running_at_the_deadline(self):
        self.settings_file(self.SETTINGS)
        r = self.seed("--wait-mins", "0", STUB_EXEC_STATUS="Running")
        self.assertEqual(r.returncode, 1)
        self.assertIn("still Running", r.stderr)

    def test_wait_mins_with_a_leading_zero_is_decimal(self):
        # `08` isn't octal: the wait is 8 minutes, not "value too great for base".
        self.settings_file(self.SETTINGS)
        r = self.seed("--wait-mins", "08")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertNotIn("base", r.stderr)
        r = self.seed("--wait-mins", "00", STUB_EXEC_STATUS="Running")
        self.assertEqual(r.returncode, 1)
        self.assertIn("still Running after 0 minutes", r.stderr)

    def test_needs_the_archive(self):
        self.settings_file('AZURE_SUBSCRIPTION_ID="sub-1"\nINGEST_JOB="caj-usnm-ingest-dev"\n')
        r = self.seed()
        self.assertEqual(r.returncode, 1)
        self.assertIn("USNM_ARCHIVE_ACCOUNT", r.stderr)
        self.assertFalse(os.path.exists(self.log))

    def test_needs_the_ingest_job(self):
        self.settings_file(self.SETTINGS.replace('INGEST_JOB="caj-usnm-ingest-dev"\n', ""))
        r = self.seed()
        self.assertEqual(r.returncode, 1)
        self.assertIn("no ingest job", r.stderr)
        self.assertFalse(os.path.exists(self.log))

    def test_never_prod(self):
        r = self.seed(env="prod")
        self.assertEqual(r.returncode, 2)
        self.assertFalse(os.path.exists(self.log))

    def test_named_arguments_only(self):
        r = subprocess.run([os.path.join(HERE, "titles-seed.sh"), "dev"], cwd=ROOT, env=self.env,
                           capture_output=True, text=True)
        self.assertEqual(r.returncode, 2)


if __name__ == "__main__":
    unittest.main()
