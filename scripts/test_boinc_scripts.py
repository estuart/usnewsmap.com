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

    def test_handoff_target_sets_handoff_step_in_the_main_group(self):
        base = {"AZURE_CLIENT_ID": "client-2", "HANDOFF_URL": "https://x.blob.core.windows.net/boinc-handoff",
                "COSMOS_ENDPOINT": "https://c.documents.azure.com:443/", "CURATED_URL": "https://d/curated",
                "REFERENCE_URL": "https://d/reference", "OUT_URL": "https://x/ocr-out"}
        tmpl = json.dumps({"containers": [{"name": "handoff", "env": [{"name": k, "value": v} for k, v in base.items()]}]})
        r = self.run_script("--target", "handoff", "--step", "collect", "--var", "JOB_ID=" + "a" * 64,
                            STUB_TEMPLATE=tmpl)
        self.assertEqual(r.returncode, 0, r.stderr)
        update = [c for c in self.calls() if c.startswith("containerapp job update")][0]
        self.assertIn("--name usnm-boinc-handoff-caj-prod --resource-group rg-usnm-prod", update)
        self.assertIn("HANDOFF_STEP=collect", update)
        self.assertNotIn("BRIDGE_STEP", update)
        r = self.run_script("--target", "handoff", "--step", "collect", "--var", "HANDOFF_STEP=x",
                            STUB_TEMPLATE=tmpl)
        self.assertEqual(r.returncode, 1)


class BaseVarsMatchBicep(unittest.TestCase):
    def test_lists_agree(self):
        with open(SCRIPT) as f:
            m = re.search(r"^BASE_VARS=\(([^)]*)\)", f.read(), re.M)
        script = m.group(1).split()
        with open(os.path.join(ROOT, "contrib-compute/infra/boinc/modules/bridge-job.bicep")) as f:
            bicep = re.findall(r"\{ name: '([A-Z_]+)', value:", f.read())
        self.assertEqual(sorted(script), sorted(bicep))

    def test_handoff_lists_agree(self):
        with open(SCRIPT) as f:
            m = re.search(r"^HANDOFF_BASE_VARS=\(([^)]*)\)", f.read(), re.M)
        script = m.group(1).split()
        with open(os.path.join(ROOT, "infra/modules/boinc-handoff-job.bicep")) as f:
            bicep = re.findall(r"\{ name: '([A-Z_]+)', value:", f.read())
        self.assertEqual(sorted(script), sorted(bicep))


TEST_CLIENT_SCRIPT = os.path.join(HERE, "boinc-test-client.sh")
APP_RELEASE_SCRIPT = os.path.join(HERE, "boinc-app-release.sh")
HANDOFF_JOB_SCRIPT = os.path.join(ROOT, "contrib-compute/images/bridge/handoff-job.sh")
BRIDGE_JOB_SCRIPT = os.path.join(ROOT, "contrib-compute/images/server/bridge-job.sh")

STUB_TEST_CLIENT_AZ = r"""#!/usr/bin/env bash
printf '%s\n' "$*" >> "$STUB_LOG"
case "$*" in
  "account show"*) echo "$STUB_SUB" ;;
  "group show"*) ;;
  "stack group show"*)
    if [ -n "${STUB_NO_VAULT:-}" ]; then echo '{}'
    else echo '{"keyVaultName": {"type": "String", "value": "usnmboinckvprod"}}'; fi ;;
  "group create"*) ;;
  "group exists"*) echo "${STUB_GROUP_EXISTS:-true}" ;;
  "group delete"*) ;;
  "deployment group create"*)
    for arg in "$@"; do
      case "$arg" in
        @*) cp "${arg#@}" "$STUB_PARAMS_OUT" ;;
      esac
    done ;;
  *) echo "stub az: unexpected call: $*" >&2; exit 9 ;;
esac
"""


class TestClient(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        bin_dir = os.path.join(self.tmp, "bin")
        os.mkdir(bin_dir)
        with open(os.path.join(bin_dir, "az"), "w") as f:
            f.write(STUB_TEST_CLIENT_AZ)
        os.chmod(os.path.join(bin_dir, "az"), 0o755)
        self.log = os.path.join(self.tmp, "az.log")
        self.params_out = os.path.join(self.tmp, "params.json")
        self.env = {
            "PATH": bin_dir + os.pathsep + os.environ["PATH"],
            "HOME": self.tmp,
            "TMPDIR": self.tmp,
            "STUB_LOG": self.log,
            "STUB_SUB": SUB,
            "STUB_PARAMS_OUT": self.params_out,
        }

    def tearDown(self):
        shutil.rmtree(self.tmp)

    def run_script(self, *args, **extra):
        return subprocess.run(
            [TEST_CLIENT_SCRIPT, "--env", "prod", *args],
            cwd=ROOT,
            env={**self.env, **extra},
            capture_output=True,
            text=True,
        )

    def calls(self):
        if not os.path.exists(self.log):
            return []
        with open(self.log) as f:
            return f.read().splitlines()

    def test_create_defaults_to_disposable_account_and_deploys_test_group(self):
        r = self.run_script("--create")
        self.assertEqual(r.returncode, 0, r.stderr)
        calls = self.calls()
        self.assertTrue(any(c.startswith("group create --name usnm-boinc-test-rg-prod") for c in calls))
        self.assertTrue(any(c.startswith("deployment group create --name usnm-boinc-test-prod --resource-group usnm-boinc-test-rg-prod") for c in calls))
        with open(self.params_out) as f:
            params = json.load(f)["parameters"]
        self.assertEqual(params["env"]["value"], "prod")
        self.assertEqual(params["vmSize"]["value"], "Standard_NC4as_T4_v3")
        self.assertIs(params["spot"]["value"], True)
        self.assertEqual(params["projectUrl"]["value"], "https://boinc.usnewsmap.com/usnmocr/")
        self.assertEqual(params["projectAuthenticator"]["value"], "")

    def test_create_supports_paygo_custom_sku_and_authenticator_file(self):
        auth_file = os.path.join(self.tmp, "auth.txt")
        with open(auth_file, "w") as f:
            f.write("0123456789abcdef0123456789abcdef\n")
        r = self.run_script("--create", "--paygo", "--sku", "Standard_D4s_v5", "--authenticator-file", auth_file)
        self.assertEqual(r.returncode, 0, r.stderr)
        with open(self.params_out) as f:
            params = json.load(f)["parameters"]
        self.assertEqual(params["vmSize"]["value"], "Standard_D4s_v5")
        self.assertIs(params["spot"]["value"], False)
        self.assertEqual(params["projectAuthenticator"]["value"], "0123456789abcdef0123456789abcdef")

    def test_create_what_if_does_not_create_group_or_deployment(self):
        r = self.run_script("--create", "--what-if")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("would create usnm-boinc-test-rg-prod", r.stdout)
        self.assertFalse([c for c in self.calls() if c.startswith("group create") or c.startswith("deployment group create")])

    def test_create_fails_when_stack_or_authenticator_file_is_invalid(self):
        r = self.run_script("--create", STUB_NO_VAULT="1")
        self.assertEqual(r.returncode, 1)
        self.assertIn("has no keyVaultName output", r.stderr)
        self.assertFalse([c for c in self.calls() if c.startswith("group create")])

        empty_auth = os.path.join(self.tmp, "empty.txt")
        open(empty_auth, "w").close()
        r = self.run_script("--create", "--authenticator-file", empty_auth)
        self.assertEqual(r.returncode, 1)
        self.assertIn("must contain a non-empty BOINC authenticator", r.stderr)

    def test_delete_removes_group_or_respects_what_if(self):
        r = self.run_script("--delete", "--what-if")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("would delete resource group usnm-boinc-test-rg-prod", r.stdout)
        self.assertFalse([c for c in self.calls() if c.startswith("group delete")])

        r = self.run_script("--delete")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertTrue(any(c.startswith("group delete --name usnm-boinc-test-rg-prod --yes --no-wait") for c in self.calls()))

    def test_rejects_invalid_arguments(self):
        for bad in (["--create", "--delete"], [], ["--create", "--sku", "bad/sku"], ["--create", "--authenticator-file", "/nonexistent/auth"]):
            r = self.run_script(*bad)
            self.assertNotEqual(r.returncode, 0, bad)


class AppReleaseArgs(unittest.TestCase):
    def run_release(self, *args):
        return subprocess.run(
            [APP_RELEASE_SCRIPT, "--env", "prod", "--version", "1.0", "--signing-host", "bad host!", *args],
            cwd=ROOT,
            capture_output=True,
            text=True,
        )

    def test_allows_quality_and_qwen_plan_classes(self):
        img = "ghcr.io/tgoodyear/usnm-boinc-worker@sha256:" + "a" * 64
        r = self.run_release("--app", "usnm_quality", "--plan-class", "docker")
        self.assertIn("--signing-host must be a host name", r.stderr)

        for pc in ("docker_mt", "docker_nvidia_cuda"):
            r = self.run_release("--app", "usnm_qwen", "--plan-class", pc, "--worker-image", img)
            self.assertIn("--signing-host must be a host name", r.stderr)

    def test_rejects_qwen_without_worker_image_or_wrong_plan_class(self):
        r = self.run_release("--app", "usnm_qwen", "--plan-class", "docker_nvidia_cuda")
        self.assertEqual(r.returncode, 1)
        self.assertIn("usnm_qwen has its own worker image: give --worker-image", r.stderr)

        r = self.run_release("--app", "usnm_qwen", "--plan-class", "docker")
        self.assertEqual(r.returncode, 1)
        self.assertIn("--app and --plan-class", r.stderr)

        r = self.run_release("--app", "usnm_quality", "--plan-class", "docker_mt")
        self.assertEqual(r.returncode, 1)
        self.assertIn("--app and --plan-class", r.stderr)


class HandoffAndBridgeQualitySteps(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        bin_dir = os.path.join(self.tmp, "bin")
        os.mkdir(bin_dir)
        self.log = os.path.join(self.tmp, "usnm-boinc.log")
        stub = os.path.join(bin_dir, "usnm-boinc")
        with open(stub, "w") as f:
            f.write(
                '#!/usr/bin/env bash\n'
                'printf "%s\\n" "$*" >> "$STUB_LOG"\n'
                'while [ $# -gt 0 ]; do\n'
                '  if [ "$1" = "--to" ]; then\n'
                '    mkdir -p "$2" && : > "$2/sample.jsonl"\n'
                '    shift 2\n'
                '  else\n'
                '    shift\n'
                '  fi\n'
                'done\n'
            )
        os.chmod(stub, 0o755)
        self.env = {
            "PATH": bin_dir + os.pathsep + os.environ["PATH"],
            "TMPDIR": self.tmp,
            "STUB_LOG": self.log,
        }

    def tearDown(self):
        shutil.rmtree(self.tmp)

    def calls(self):
        if not os.path.exists(self.log):
            return []
        with open(self.log) as f:
            return f.read().splitlines()

    def test_handoff_texts_export(self):
        env = {
            **self.env,
            "HANDOFF_STEP": "texts-export",
            "HANDOFF_URL": "https://usnmst.blob.core.windows.net/boinc-handoff",
            "CURATED_URL": "https://usnmst.blob.core.windows.net/curated",
            "COSMOS_ENDPOINT": "https://c.documents.azure.com:443/",
            "REFERENCE_URL": "https://usnmst.blob.core.windows.net/reference",
            "OUT_URL": "https://usnmst.blob.core.windows.net/ocr-out",
            "HANDOFF_PAGES": "sn83045462_1890-05-03_ed-1_seq-1,sn83045462_1890-05-03_ed-1_seq-2",
            "HANDOFF_SAMPLE": "sample.jsonl",
        }
        r = subprocess.run([HANDOFF_JOB_SCRIPT], cwd=ROOT, env=env, capture_output=True, text=True)
        self.assertEqual(r.returncode, 0, r.stderr)
        calls = self.calls()
        self.assertEqual(len(calls), 3)
        self.assertIn("handoff get --from-url https://usnmst.blob.core.windows.net/boinc-handoff --prefix quality-samples", calls[0])
        self.assertIn("quality texts-export --curated-url https://usnmst.blob.core.windows.net/curated", calls[1])
        self.assertIn("--doc-id sn83045462_1890-05-03_ed-1_seq-1 --doc-id sn83045462_1890-05-03_ed-1_seq-2", calls[1])
        self.assertIn("sample.jsonl", calls[1])
        self.assertIn("handoff put", calls[2])
        self.assertIn("--prefix texts", calls[2])

    def test_bridge_quality_and_qwen_steps(self):
        share = os.path.join(self.tmp, "share")
        proj = os.path.join(share, "usnmocr")
        os.makedirs(proj)
        open(os.path.join(proj, ".usnm-initialised"), "w").close()
        bridge_copy = os.path.join(self.tmp, "bridge-job.sh")
        lib_path = os.path.join(ROOT, "contrib-compute/images/server/lib.sh")
        with open(BRIDGE_JOB_SCRIPT) as src, open(bridge_copy, "w") as dst:
            dst.write(src.read().replace("/usr/local/lib/usnm-boinc/lib.sh", lib_path))
        os.chmod(bridge_copy, 0o755)
        base = {
            **self.env,
            "KEY_VAULT_NAME": "usnmboinckvprod",
            "DB_HOST": "db.example.org",
            "DB_NAME": "usnmocr",
            "MASTER_HOST": "boinc.usnewsmap.com",
            "PROJECT_DIR": share,
            "QUALITY_CAMPAIGN": "pilot-1",
            "QWEN_CAMPAIGN": "pilot-1",
            "QUALITY_ENGINE_IMAGE": "sha256:" + "0" * 64,
            "QWEN_ENGINE_IMAGE": "sha256:" + "1" * 64,
            "QWEN_ENGINE_LLAMA": "sha256:" + "2" * 64,
            "QWEN_ENGINE_WEIGHTS": "sha256:" + "3" * 64,
        }
        r = subprocess.run(
            [bridge_copy],
            cwd=ROOT,
            env={
                **base,
                "BRIDGE_STEP": "quality-stage",
                "HANDOFF_URL": "https://usnmst.blob.core.windows.net/boinc-handoff",
                "QUALITY_LANGS": "eng,deu",
                "QUALITY_PAGES": "probe:eng:sn83045462_1890-05-03_ed-1_seq-1:dlc_abc_ver01",
            },
            capture_output=True,
            text=True,
        )
        self.assertEqual(r.returncode, 0, r.stderr)
        calls = self.calls()
        self.assertTrue(any("handoff get --from-url https://usnmst.blob.core.windows.net/boinc-handoff --prefix texts" in c for c in calls))
        self.assertTrue(any("quality function-words --lang eng --lang deu" in c for c in calls))
        self.assertTrue(any("quality stage --page probe:eng:sn83045462_1890-05-03_ed-1_seq-1:dlc_abc_ver01" in c for c in calls))

        r = subprocess.run(
            [bridge_copy],
            cwd=ROOT,
            env={**base, "BRIDGE_STEP": "qwen-stage"},
            capture_output=True,
            text=True,
        )
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertTrue(any("qwen stage --quality-root" in c for c in self.calls()))

        r = subprocess.run(
            [bridge_copy],
            cwd=ROOT,
            env={**base, "BRIDGE_STEP": "quality-collect", "QUALITY_SPOOL": "/spool/q", "QWEN_SPOOL": "/spool/v"},
            capture_output=True,
            text=True,
        )
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertTrue(any("quality collect --campaign pilot-1" in c for c in self.calls()))


if __name__ == "__main__":
    unittest.main()
