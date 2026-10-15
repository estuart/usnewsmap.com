"""Host contract regressions; no registry, system changes or account credentials."""
import importlib.util
import json
import shlex
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import xml.etree.ElementTree as ET
import tomllib


def load(name):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(name + ".py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


pre = load("volunteer_preflight")
setup = load("setup_volunteer")
IMAGE = "registry.example.org/usnm/worker@sha256:" + "a" * 64
LIMITS = {"cpu": "100000 100000", "memory": str(pre.MEMORY), "swap": "0", "pids": "64", "ready": "yes"}
INFO = {"host": {"security": {"rootless": True}, "cgroupVersion": "v2", "cgroupManager": "systemd"}}


class Runtime:
    def __init__(self, values=None, slot_error=False, pause_error=False, cleanup_error=False):
        self.values = values if values is not None else LIMITS
        self.slot_error = slot_error
        self.pause_error = pause_error
        self.cleanup_error = cleanup_error
        self.calls = []
        self.status = "running"

    def __call__(self, args, timeout=30):
        self.calls.append(args)
        if args[1] == "info":
            return json.dumps(INFO)
        if args[1] == "create":
            self.scratch = Path(args[args.index("--volume") + 1].removesuffix(":/work"))
            if not self.slot_error:
                output = self.scratch / "output"
                output.write_text("slot readable and writable\n")
                output.chmod(0o644)
        if args[1] == "logs":
            if self.slot_error:
                return "cat: /work/input: Permission denied\n"
            return "\n".join(f"{key}={value}" for key, value in self.values.items())
        if args[1] == "inspect":
            return json.dumps([{"State": {"Status": "exited" if self.slot_error else self.status}}])
        if args[1] == "pause":
            if self.pause_error:
                raise pre.PreflightError("cannot pause container without a cgroup")
            self.status = "paused"
        if args[1] == "unpause":
            self.status = "running"
        if args[1] == "rm" and self.cleanup_error:
            raise pre.PreflightError("runtime cleanup unavailable")
        return ""


class PreflightTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.data = Path(self.tmp.name)
        (self.data / "slots").mkdir()
        self.uid = patch.object(pre.os, "geteuid", return_value=1000)
        self.uid.start()

    def tearDown(self):
        self.uid.stop()
        self.tmp.cleanup()

    def test_bounded_host_checks_real_slot_and_cleans_only_its_probe(self):
        other = self.data / "slots" / "existing-work"
        other.mkdir()
        runtime = Runtime()
        result = pre.preflight(self.data, IMAGE, runtime)
        self.assertTrue(result["pause_resume"])
        self.assertEqual(list((self.data / "slots").iterdir()), [other])
        create = next(call for call in runtime.calls if call[1] == "create")
        self.assertNotIn(":Z", " ".join(create))
        self.assertNotIn(":z", " ".join(create))
        self.assertIn("--network=none", create)
        self.assertIn("--read-only", create)
        self.assertEqual([call[1] for call in runtime.calls if call[1] in ["pause", "unpause", "rm"]], ["pause", "unpause", "rm"])

    def test_probe_keeps_the_released_wrapper_runtime_policy(self):
        runtime = Runtime()
        pre.preflight(self.data, IMAGE, runtime)
        create = next(call for call in runtime.calls if call[1] == "create")
        job = tomllib.loads((Path(__file__).resolve().parents[2] / "ocr/boinc/job.toml").read_text())
        for option in shlex.split(job["create_args"]):
            self.assertIn(option, create)
        self.assertEqual(job["workdir"], "/work")

    def test_cli_success_with_ignored_kernel_limits_is_rejected(self):
        for key, ignored in [("cpu", "max 100000"), ("memory", "max"), ("swap", "max"), ("pids", "max"), ("pids", "199632")]:
            with self.subTest(key=key):
                runtime = Runtime(dict(LIMITS, **{key: ignored}))
                with self.assertRaises(pre.PreflightError):
                    pre.preflight(self.data, IMAGE, runtime)
                self.assertTrue(any(call[1] == "rm" for call in runtime.calls))
                self.assertFalse(any(call[1] == "pause" for call in runtime.calls))

    def test_selinux_slot_failure_is_actionable_and_cleans_up(self):
        runtime = Runtime(slot_error=True)
        with self.assertRaisesRegex(pre.PreflightError, "container_file_t"):
            pre.preflight(self.data, IMAGE, runtime)
        self.assertEqual(list((self.data / "slots").iterdir()), [])

    def test_pause_failure_and_cleanup_failure_are_fatal(self):
        for runtime in [Runtime(pause_error=True), Runtime(cleanup_error=True)]:
            with self.assertRaises(pre.PreflightError):
                pre.preflight(self.data, IMAGE, runtime)

    def test_rootful_v1_or_cgroupfs_runtime_cannot_pass(self):
        for host in [dict(INFO["host"], security={"rootless": False}), dict(INFO["host"], cgroupVersion="v1"), dict(INFO["host"], cgroupManager="cgroupfs")]:
            with self.assertRaises(pre.PreflightError):
                pre.check_info({"host": host})

    def test_unpinned_image_never_calls_runtime(self):
        runtime = Runtime()
        for image in ["worker:latest", IMAGE + "\nRUN bad", "sha256:" + "a" * 64]:
            with self.assertRaises(pre.PreflightError):
                pre.preflight(self.data, image, runtime)
        self.assertEqual(runtime.calls, [])

    def test_service_environment_override_or_removal_fails_before_podman(self):
        service = "boinc-client.service"
        expected = {
            "HOME": str(self.data),
            "XDG_RUNTIME_DIR": "/run/usnm-boinc-1000-boinc-client",
            "DBUS_SESSION_BUS_ADDRESS": "unix:path=/run/user/1000/bus",
            "CONTAINERS_CONF": f"/etc/usnm-boinc-volunteer/{service}/containers.conf",
            "CONTAINERS_STORAGE_CONF": f"/etc/usnm-boinc-volunteer/{service}/storage.conf",
        }
        with patch.dict(pre.os.environ, expected, clear=True):
            self.assertTrue(pre.preflight(self.data, IMAGE, Runtime(), service=service)["rootless"])
        for key in ["XDG_RUNTIME_DIR", "CONTAINERS_STORAGE_CONF"]:
            overridden = dict(expected, **{key: "/var/lib/unmanaged"})
            runtime = Runtime()
            with patch.dict(pre.os.environ, overridden, clear=True):
                with self.assertRaises(pre.PreflightError):
                    pre.preflight(self.data, IMAGE, runtime, service=service)
            self.assertEqual(runtime.calls, [])
            removed = {k: v for k, v in expected.items() if k != key}
            with patch.dict(pre.os.environ, removed, clear=True):
                with self.assertRaises(pre.PreflightError):
                    pre.preflight(self.data, IMAGE, runtime, service=service)
            self.assertEqual(runtime.calls, [])


class SetupTest(unittest.TestCase):
    def test_overlapping_subordinate_ids_are_rejected_without_reallocation(self):
        setup.check_subids("boinc:100000:65536\nother:200000:65536\n", "boinc", 999)
        setup.check_subids("999:100000:65536\n", "boinc", 999)
        for text in ["boinc:100000:65536\nother:100000:65536\n", "boinc:100000:65536\nother:160000:65536\n", "boinc:100000:1024\n", "other:100000:65536\n"]:
            with self.assertRaises(setup.SetupError):
                setup.check_subids(text, "boinc", 999)

    def test_unsafe_cli_values_fail_before_reading_service_or_files(self):
        with patch.object(setup, "run") as commands:
            for service, data, image in [("x.service\nOther=1", Path("/var/lib/boinc"), IMAGE), ("boinc-client.service", Path("/var/lib/boinc space"), IMAGE), ("boinc-client.service", Path("/var/lib/boinc"), IMAGE + "\nOther=1")]:
                with self.assertRaises(setup.SetupError):
                    setup.setup(service, data, image)
            commands.assert_not_called()

    def test_queued_or_unreported_work_blocks_reconfiguration(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp)
            self.assertFalse(setup.active_work(path))
            for xml in ["<client_state><result><state>1</state></result></client_state>", "<client_state><result><ready_to_report/></result></client_state>", "<client_state><active_task_set><active_task/></active_task_set></client_state>"]:
                (path / "client_state.xml").write_text(xml)
                self.assertTrue(setup.active_work(path))

    def test_preferences_keep_unrelated_settings_and_do_not_touch_account(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "prefs.xml"
            path.write_text("<global_preferences><disk_max_used_gb>12</disk_max_used_gb><run_if_user_active>1</run_if_user_active></global_preferences>")
            result = ET.fromstring(setup.merge_xml(path, "global_preferences", setup.PREFS))
            self.assertEqual(result.findtext("disk_max_used_gb"), "12")
            self.assertEqual(result.findtext("run_if_user_active"), "0")
            self.assertEqual(result.findtext("leave_apps_in_memory"), "1")
            self.assertEqual(result.findtext("suspend_cpu_usage"), "0")

    def test_managed_dropin_preflights_inside_service_and_preserves_runtime(self):
        _, dropin, config, _ = setup.profile("boinc-client.service", Path("/var/lib/boinc"), IMAGE, 999)
        self.assertIn("Requires=user@999.service", dropin)
        self.assertIn("After=user@999.service", dropin)
        self.assertIn("ExecStartPre=/usr/bin/python3 /usr/local/lib/usnm-boinc-volunteer/preflight.py --data-dir=/var/lib/boinc --image=" + IMAGE + " --service=boinc-client.service", dropin)
        self.assertIn("RuntimeDirectoryPreserve=yes", dropin)
        self.assertIn("ProtectControlGroups=false", dropin)
        self.assertIn("DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/999/bus", dropin)
        self.assertIn('conmon_cgroup="pod"', config)
        self.assertNotIn("docker", dropin)

    def test_conflicting_later_dropin_cannot_silently_bypass_profile(self):
        service = "boinc-client.service"
        data = Path("/var/lib/boinc")
        config, _, _, _ = setup.profile(service, data, IMAGE, 999)
        environment = {
            "HOME": str(data), "XDG_RUNTIME_DIR": "/run/usnm-boinc-999-boinc-client",
            "DBUS_SESSION_BUS_ADDRESS": "unix:path=/run/user/999/bus",
            "CONTAINERS_CONF": str(config / "containers.conf"),
            "CONTAINERS_STORAGE_CONF": str(config / "storage.conf"),
        }
        valid_start = "{ path=/usr/bin/boinc ; argv[]=/usr/bin/boinc --dir ${BOINC_DIR} $BOINC_OPTS ; ignore_errors=no ; start_time=[n/a] ; stop_time=[n/a] ; pid=0 ; code=(null) ; status=0/0 }"
        optional_pre = "{ path=/bin/chown ; argv[]=/bin/chown boinc:boinc /var/lib/boinc ; ignore_errors=yes ; start_time=[n/a] ; stop_time=[n/a] ; pid=0 ; code=(null) ; status=0/0 }"
        valid_pre = (
            "{ path=/usr/bin/python3 ; argv[]=/usr/bin/python3 /usr/local/lib/usnm-boinc-volunteer/preflight.py "
            f"--data-dir={data} --image={IMAGE} --service={service} ; ignore_errors=no ; start_time=[n/a] ; stop_time=[n/a] ; pid=0 ; code=(null) ; status=0/0 }}"
        )
        with tempfile.TemporaryDirectory() as tmp:
            env_dir = Path(tmp)
            harmless_env = env_dir / "harmless.env"
            harmless_env.write_text("# comment\nBOINC_DIR=/var/lib/boinc\nBOINC_OPTS=\"--dir /var/lib/boinc\"\n")
            override_dir_env = env_dir / "override-dir.env"
            override_dir_env.write_text("BOINC_OPTS=\"--dir /var/lib/other-boinc\"\n")
            override_boinc_dir = env_dir / "override-boinc-dir.env"
            override_boinc_dir.write_text("BOINC_DIR=/var/lib/other-boinc\n")
            override_runtime = env_dir / "override-runtime.env"
            override_runtime.write_text("XDG_RUNTIME_DIR=/run/unmanaged\n")
            override_storage = env_dir / "override-storage.env"
            override_storage.write_text("export CONTAINERS_STORAGE_CONF=\"/etc/containers/storage.conf\"\n")
            missing_env = env_dir / "missing.env"
            base_env_str = " ".join(f"{k}={v}" for k, v in environment.items())
            properties = {
                "Requires": "basic.target user@999.service",
                "After": "network.target user@999.service",
                "WorkingDirectory": str(data),
                "ExecStart": valid_start,
                "Environment": base_env_str,
                "UnsetEnvironment": "UNRELATED_VAR",
                "EnvironmentFiles": f"{harmless_env} (ignore_errors=no) {missing_env} (ignore_errors=yes)",
                "ProtectControlGroups": "no",
                "RuntimeDirectoryPreserve": "yes",
                "ExecStartPre": f"{optional_pre}\n{valid_pre}",
            }
            def command(args, timeout=180):
                key = next(item.removeprefix("--property=") for item in args if item.startswith("--property="))
                return properties[key]
            with patch.object(setup, "run", side_effect=command):
                setup.check_effective(service, data, config, 999, IMAGE)
                # Conflicting BOINC_OPTS: Environment has /var/lib/boinc, EnvironmentFile overrides it with /var/lib/other-boinc.
                properties["Environment"] = f'{base_env_str} "BOINC_OPTS=--dir /var/lib/boinc"'
                properties["EnvironmentFiles"] = f"{harmless_env} (ignore_errors=no) {override_dir_env} (ignore_errors=yes)"
                with self.assertRaises(setup.SetupError):
                    setup.check_effective(service, data, config, 999, IMAGE)
                properties["Environment"] = base_env_str
                properties["EnvironmentFiles"] = f"{harmless_env} (ignore_errors=no) {missing_env} (ignore_errors=yes)"
                bad_cases = [
                    ("Requires", "basic.target"),
                    ("After", "network.target"),
                    ("WorkingDirectory", "/var/lib/other-boinc"),
                    ("ExecStart", valid_start.replace("$BOINC_OPTS", "--dir /var/lib/other-boinc")),
                    ("ExecStart", valid_start.replace("$BOINC_OPTS", "--dir=/var/lib/other-boinc")),
                    ("ExecStart", valid_start.replace("$BOINC_OPTS", "--dir")),
                    ("EnvironmentFiles", f"{override_dir_env} (ignore_errors=no)"),
                    ("EnvironmentFiles", f"{override_boinc_dir} (ignore_errors=no)"),
                    ("EnvironmentFiles", f"{harmless_env}"),
                    ("Environment", "HOME=/old"),
                    ("UnsetEnvironment", "XDG_RUNTIME_DIR"),
                    ("UnsetEnvironment", f"CONTAINERS_STORAGE_CONF={config / 'storage.conf'}"),
                    ("EnvironmentFiles", f"{override_runtime} (ignore_errors=yes)"),
                    ("EnvironmentFiles", f"{override_storage} (ignore_errors=no)"),
                    ("EnvironmentFiles", f"{missing_env} (ignore_errors=no)"),
                    ("ProtectControlGroups", "yes"),
                    ("RuntimeDirectoryPreserve", "no"),
                    ("ExecStartPre", ""),
                    ("ExecStartPre", optional_pre),
                    ("ExecStartPre", valid_pre.replace("ignore_errors=no", "ignore_errors=yes")),
                    ("ExecStartPre", valid_pre.replace("path=/usr/bin/python3", "path=/bin/true")),
                    ("ExecStartPre", valid_pre.replace(f"--service={service}", "--service=other.service")),
                    ("ExecStartPre", valid_pre.replace(f"--image={IMAGE}", "--image=registry.example.org/usnm/worker@sha256:" + "b" * 64)),
                ]
                for key, bad in bad_cases:
                    with self.subTest(key=key, bad=bad):
                        original = properties[key]
                        properties[key] = bad
                        with self.assertRaises(setup.SetupError):
                            setup.check_effective(service, data, config, 999, IMAGE)
                        properties[key] = original

    def test_backup_records_initial_absence_and_preserves_original_across_reapply(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            data = root / "data"
            backups = root / "backups"
            data.mkdir()
            backups.mkdir()
            cc = data / "cc_config.xml"
            prefs = data / "global_prefs_override.xml"
            original_prefs = "<global_preferences><disk_max_used_gb>12</disk_max_used_gb></global_preferences>\n"
            prefs.write_text(original_prefs)
            with patch.object(setup.os, "fchown"):
                setup.backup(cc, backups)
                setup.backup(prefs, backups)
                self.assertTrue((backups / "cc_config.xml.absent").exists())
                self.assertFalse((backups / "cc_config.xml").exists())
                self.assertEqual((backups / "global_prefs_override.xml").read_text(), original_prefs)
                self.assertFalse((backups / "global_prefs_override.xml.absent").exists())
                cc.write_text(setup.merge_xml(cc, "cc_config", {"ncpus": "1", "no_gpus": "1"}, "options"))
                prefs.write_text(setup.merge_xml(prefs, "global_preferences", setup.PREFS))
                setup.backup(cc, backups)
                setup.backup(prefs, backups)
                self.assertTrue((backups / "cc_config.xml.absent").exists())
                self.assertFalse((backups / "cc_config.xml").exists())
                self.assertEqual((backups / "global_prefs_override.xml").read_text(), original_prefs)

    def test_start_timeout_exceeds_unit_startup_deadline(self):
        self.assertGreater(setup.START_TIMEOUT, 600)
        completed = unittest.mock.Mock(returncode=0, stdout="ok\n", stderr="")
        with patch.object(setup.subprocess, "run", return_value=completed) as subrun:
            setup.run(["systemctl", "stop", "boinc-client.service"])
            setup.run(["systemctl", "start", "boinc-client.service"], timeout=setup.START_TIMEOUT)
            self.assertEqual(subrun.call_args_list[0].kwargs["timeout"], 180)
            self.assertEqual(subrun.call_args_list[1].kwargs["timeout"], setup.START_TIMEOUT)

    def test_duplicate_settings_and_wrong_xml_fail_without_writing(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "prefs.xml"
            for xml in ["<other/>", "<global_preferences><run_if_user_active>0</run_if_user_active><run_if_user_active>1</run_if_user_active></global_preferences>"]:
                path.write_text(xml)
                with self.assertRaises(setup.SetupError):
                    setup.merge_xml(path, "global_preferences", setup.PREFS)
                self.assertEqual(path.read_text(), xml)
            cc = Path(tmp) / "cc_config.xml"
            dup_sections = "<cc_config><options><ncpus>1</ncpus></options><options><ncpus>8</ncpus></options></cc_config>"
            cc.write_text(dup_sections)
            with self.assertRaises(setup.SetupError):
                setup.merge_xml(cc, "cc_config", {"ncpus": "1", "no_gpus": "1"}, "options")
            self.assertEqual(cc.read_text(), dup_sections)


if __name__ == "__main__":
    unittest.main()
