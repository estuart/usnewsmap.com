#!/usr/bin/env python3
"""Fail closed before BOINC starts when its rootless Podman cannot run a bounded task."""
import argparse
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import uuid

MEMORY = 1536 * 1024 * 1024
IMAGE = re.compile(r"[A-Za-z0-9][A-Za-z0-9._:/-]*@sha256:[0-9a-f]{64}\Z")
PROBE = r"""set -eu
test "$(cat /work/input)" = usnm-preflight
printf 'slot readable and writable\n' > /work/output
chmod 0644 /work/output
test -f /sys/fs/cgroup/cgroup.controllers
printf 'cpu=%s\n' "$(cat /sys/fs/cgroup/cpu.max)"
printf 'memory=%s\n' "$(cat /sys/fs/cgroup/memory.max)"
printf 'swap=%s\n' "$(cat /sys/fs/cgroup/memory.swap.max)"
printf 'pids=%s\n' "$(cat /sys/fs/cgroup/pids.max)"
if touch /usnm-preflight-write 2>/dev/null; then exit 21; fi
test "$(wc -l < /proc/net/route)" -eq 1
printf 'ready=yes\n'
remaining=120
while [ "$remaining" -gt 0 ]; do sleep 1; remaining=$((remaining - 1)); done
"""


class PreflightError(RuntimeError):
    pass


def run(args, timeout=30):
    try:
        result = subprocess.run(args, capture_output=True, text=True, timeout=timeout)
    except (OSError, subprocess.TimeoutExpired) as error:
        raise PreflightError(f"Cannot run {args[0]}: {error}") from error
    if result.returncode:
        detail = (result.stderr or result.stdout).strip()[-3000:]
        raise PreflightError(f"{args[0]} {args[1]} failed: {detail}")
    return result.stdout


def check_limits(values):
    """Inspect kernel readings, not OCI configuration or accepted CLI arguments."""
    try:
        quota, period = values["cpu"].split()
        bounded_cpu = quota != "max" and 0 < int(quota) <= int(period)
        bounded_memory = int(values["memory"]) == MEMORY
        no_extra_swap = int(values["swap"]) == 0
        bounded_pids = int(values["pids"]) == 64
    except (KeyError, ValueError):
        raise PreflightError("Missing/unbounded cgroup v2 limits. Configure the BOINC user systemd session and writable cgroups.")
    if not all([bounded_cpu, bounded_memory, no_extra_swap, bounded_pids]):
        raise PreflightError(f"Kernel limits differ from the task policy: {values}. Fix cgroup delegation; do not accept work.")


def check_info(info):
    host = info.get("host", {})
    if not host.get("security", {}).get("rootless", False):
        raise PreflightError("Podman must run rootless as the BOINC service user, not root.")
    if host.get("cgroupVersion") != "v2" or host.get("cgroupManager") != "systemd":
        raise PreflightError("Rootless Podman needs cgroup v2 and its systemd cgroup manager. Use the volunteer setup helper.")


def state(name, runner):
    return json.loads(runner(["podman", "inspect", name]))[0]["State"]


def wait_state(name, status, runner):
    for _ in range(20):
        current = state(name, runner)
        if current.get("Status") == status:
            return current
        time.sleep(0.1)
    raise PreflightError(f"Podman did not enter {status}; pause/resume is unusable.")


def preflight(data_dir, image, runner=None):
    runner = runner or run
    if not IMAGE.fullmatch(image):
        raise PreflightError("--image must name a registry image pinned by its full sha256 digest.")
    if os.geteuid() == 0:
        raise PreflightError("Run the preflight as the BOINC service user inside its systemd service, not with sudo/root.")
    check_info(json.loads(runner(["podman", "info", "--format=json"])))
    slots = data_dir.resolve(strict=True) / "slots"
    if not slots.is_dir() or not os.access(slots, os.W_OK):
        raise PreflightError("The BOINC slots directory must exist and be writable by the service user.")
    # Work under the real slots parent; no :z/:Z shortcut that hides a broken host setup.
    # The official client also runs an unqualified hello-world during runtime detection.
    runner(["podman", "run", "--rm", "docker.io/library/hello-world"], timeout=180)
    try:
        runner(["podman", "image", "inspect", image])
    except PreflightError:
        runner(["podman", "pull", image], timeout=180)
    name = "usnm-preflight-" + uuid.uuid4().hex
    scratch = Path(tempfile.mkdtemp(prefix=".usnm-preflight-", dir=slots))
    try:
        scratch.chmod(0o771)
        input_file = scratch / "input"
        input_file.write_text("usnm-preflight\n")
        input_file.chmod(0o600)
        runner([
            "podman", "create", "--name", name, "--pull=never",
            "--network=none", "--read-only", "--cap-drop=ALL", "--cap-add=DAC_OVERRIDE",
            "--security-opt=no-new-privileges", "--cpus=1", "--memory=1536m",
            "--memory-swap=1536m", "--pids-limit=64", "--tmpfs=/tmp:rw,size=512m",
            "--log-driver=k8s-file", "--volume", f"{scratch}:/work",
            "--entrypoint=/bin/sh", image, "-c", PROBE,
        ])
        runner(["podman", "start", name])
        values = {}
        for _ in range(100):
            logs = runner(["podman", "logs", name])
            values = dict(line.split("=", 1) for line in logs.splitlines() if "=" in line)
            if values.get("ready") == "yes":
                break
            if state(name, runner).get("Status") == "exited":
                raise PreflightError(f"Slot/isolation probe failed: {logs[-2000:]}. On SELinux hosts, label only the BOINC slots as container_file_t.")
            time.sleep(0.1)
        else:
            raise PreflightError("Slot probe timed out. Inspect Podman/SELinux diagnostics before accepting work.")
        check_limits(values)
        output_file = scratch / "output"
        if output_file.read_text() != "slot readable and writable\n" or output_file.stat().st_mode & 0o777 != 0o644:
            raise PreflightError("Container output is not readable by the BOINC service user with mode 0644.")
        runner(["podman", "pause", name])
        wait_state(name, "paused", runner)
        runner(["podman", "unpause", name])
        wait_state(name, "running", runner)
        return {"rootless": True, "slot_io": True, "pause_resume": True, "kernel_limits": values}
    finally:
        # Never target a volunteer task or prune the user's runtime/storage.
        cleanup_error = None
        try:
            runner(["podman", "rm", "--force", "--ignore", name])
        except PreflightError as error:
            cleanup_error = error
        if cleanup_error is None:
            shutil.rmtree(scratch)
        else:
            raise PreflightError(f"Preflight cleanup failed; BOINC remains stopped: {cleanup_error}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--data-dir", type=Path, required=True)
    parser.add_argument("--image", required=True)
    args = parser.parse_args()
    def interrupted(signum, frame):
        raise PreflightError("Startup preflight interrupted; no work may be accepted.")
    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGINT, interrupted)
    try:
        result = preflight(args.data_dir, args.image)
    except (PreflightError, OSError, json.JSONDecodeError) as error:
        print(f"BOINC host preflight FAILED: {error}", file=sys.stderr)
        return 1
    print("BOINC host preflight passed: " + json.dumps(result, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main())
