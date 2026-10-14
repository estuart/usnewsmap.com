#!/usr/bin/env python3
"""Preview/install a rootless Podman BOINC profile and mandatory startup preflight."""
import argparse
import json
import os
from pathlib import Path
import pwd
import re
import shlex
import shutil
import subprocess
import sys
import xml.etree.ElementTree as ET

SERVICE = re.compile(r"[A-Za-z0-9_-]{1,80}\.service\Z")
SAFE_PATH = re.compile(r"/[A-Za-z0-9_./-]+\Z")
IMAGE = re.compile(r"[A-Za-z0-9][A-Za-z0-9._:/-]*@sha256:[0-9a-f]{64}\Z")
PREFS = {
    "run_on_batteries": "0", "run_if_user_active": "0", "run_gpu_if_user_active": "0",
    "idle_time_to_run": "5", "max_ncpus_pct": "100", "cpu_usage_limit": "100",
    "suspend_cpu_usage": "0", "leave_apps_in_memory": "1",
    "work_buf_min_days": "0", "work_buf_additional_days": "0",
    "ram_max_used_busy_pct": "50", "ram_max_used_idle_pct": "50",
}


class SetupError(RuntimeError):
    pass


def run(args):
    result = subprocess.run(args, capture_output=True, text=True, timeout=180)
    if result.returncode:
        raise SetupError(f"{args[0]} failed: {(result.stderr or result.stdout).strip()[-2000:]}")
    return result.stdout.strip()


def active_work(data_dir):
    path = data_dir / "client_state.xml"
    if not path.exists():
        return False
    root = ET.parse(path).getroot()
    # Include downloaded and queued tasks, not just a currently running application.
    return bool(root.findall("result") or root.findall(".//active_task"))


def check_subids(text, user, uid):
    entries = []
    for line in text.splitlines():
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        owner, start, count = line.split(":")
        start, count = int(start), int(count)
        if start < 0 or count <= 0:
            raise SetupError("Malformed subordinate ID allocation; ask the host administrator to repair it.")
        entries.append((owner, start, start + count))
    mine = [entry for entry in entries if entry[0] in [user, str(uid)]]
    if not any(end - start >= 65536 for _, start, end in mine):
        raise SetupError("The BOINC service user needs at least 65536 subordinate IDs; ask the host administrator to allocate a free range.")
    for _, start, end in mine:
        for owner, other_start, other_end in entries:
            if owner not in [user, str(uid)] and start < other_end and other_start < end:
                raise SetupError("The BOINC subordinate ID range overlaps another user. Ask the host administrator to allocate a free range before running rootless containers.")


def merge_xml(path, root_name, edits, section=None):
    root = ET.parse(path).getroot() if path.exists() else ET.Element(root_name)
    if root.tag != root_name:
        raise SetupError(f"Unexpected XML root in {path}; refusing to replace it.")
    target = root
    if section:
        target = root.find(section)
        if target is None:
            target = ET.SubElement(root, section)
    for name, value in edits.items():
        existing = target.findall(name)
        if len(existing) > 1:
            raise SetupError(f"Duplicate {name} settings in {path}; resolve them first.")
        element = existing[0] if existing else ET.SubElement(target, name)
        element.text = value
    ET.indent(root, space="  ")
    return ET.tostring(root, encoding="unicode") + "\n"


def profile(service, data_dir, image, uid):
    config_dir = Path("/etc/usnm-boinc-volunteer") / service
    runtime = f"usnm-boinc-{uid}-{service.removesuffix('.service')}"
    preflight = "/usr/local/lib/usnm-boinc-volunteer/preflight.py"
    dropin = f"""# Managed by scripts/boinc/setup_volunteer.py. See docs/boinc-volunteer.md.
[Service]
Environment=HOME={data_dir}
Environment=XDG_RUNTIME_DIR=/run/{runtime}
Environment=DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/{uid}/bus
Environment=CONTAINERS_CONF={config_dir}/containers.conf
Environment=CONTAINERS_STORAGE_CONF={config_dir}/storage.conf
RuntimeDirectory={runtime}
RuntimeDirectoryMode=0700
RuntimeDirectoryPreserve=yes
Delegate=yes
# The rootless runtime needs writable cgroups; UID permissions still restrict access.
ProtectControlGroups=false
ProtectHome=read-only
ReadWritePaths={data_dir} /tmp /var/tmp
TimeoutStartSec=10min
ExecStartPre=/usr/bin/python3 {preflight} --data-dir={data_dir} --image={image}
"""
    # Separate, service-selected config; do not replace a user's global Podman configuration.
    containers = '[engine]\ncgroup_manager="systemd"\nconmon_cgroup="pod"\n'
    storage = f'[storage]\ndriver="overlay"\nrunroot="/run/{runtime}/containers"\ngraphroot="{data_dir}/usnm-podman-storage"\n'
    return config_dir, dropin, containers, storage


def check_effective(service, data_dir, config_dir, uid):
    environment = dict(item.split("=", 1) for item in shlex.split(run(["systemctl", "show", service, "--property=Environment", "--value"])) if "=" in item)
    runtime = f"usnm-boinc-{uid}-{service.removesuffix('.service')}"
    expected = {
        "HOME": str(data_dir), "XDG_RUNTIME_DIR": f"/run/{runtime}",
        "DBUS_SESSION_BUS_ADDRESS": f"unix:path=/run/user/{uid}/bus",
        "CONTAINERS_CONF": str(config_dir / "containers.conf"),
        "CONTAINERS_STORAGE_CONF": str(config_dir / "storage.conf"),
    }
    if any(environment.get(key) != value for key, value in expected.items()):
        raise SetupError("Another service drop-in overrides the managed Podman environment. Inspect systemctl cat and resolve the conflict before starting BOINC.")
    if run(["systemctl", "show", service, "--property=ProtectControlGroups", "--value"]) not in ["no", "false"]:
        raise SetupError("Another service drop-in makes cgroups read-only. Resolve that conflict before starting BOINC.")
    if run(["systemctl", "show", service, "--property=RuntimeDirectoryPreserve", "--value"]) != "yes":
        raise SetupError("The service must preserve Podman runtime files across a restart.")
    commands = run(["systemctl", "show", service, "--property=ExecStartPre", "--value"])
    if "/usr/local/lib/usnm-boinc-volunteer/preflight.py" not in commands:
        raise SetupError("Another service drop-in removed the startup preflight. Resolve that conflict before starting BOINC.")


def write_file(path, content, uid=0, gid=0, mode=0o640):
    # A root-owned temporary file and atomic replace; never follow a target symlink.
    if path.is_symlink():
        raise SetupError(f"Refusing to overwrite symlink {path}")
    temp = path.with_name(path.name + ".usnm-new")
    descriptor = os.open(temp, os.O_WRONLY | os.O_CREAT | os.O_EXCL, mode)
    try:
        with os.fdopen(descriptor, "w") as output:
            output.write(content)
            output.flush()
            os.fchown(output.fileno(), uid, gid)
            os.fchmod(output.fileno(), mode)
            os.fsync(output.fileno())
        os.replace(temp, path)
    finally:
        temp.unlink(missing_ok=True)


def backup(path, directory):
    if path.exists():
        target = directory / path.name
        if not target.exists():
            write_file(target, path.read_text(), mode=0o600)


def setup(service, data_dir, image, apply=False):
    if not SERVICE.fullmatch(service) or (data_dir is not None and not SAFE_PATH.fullmatch(str(data_dir))) or not IMAGE.fullmatch(image):
        raise SetupError("Use a plain systemd service name, an absolute path without spaces/special characters, and a registry image pinned by full sha256 digest.")
    working = run(["systemctl", "show", service, "--property=WorkingDirectory", "--value"])
    if data_dir is None:
        data_dir = Path(working)
        if not SAFE_PATH.fullmatch(str(data_dir)):
            raise SetupError("The service has no supported absolute working directory; use --data-dir for its existing directory.")
    data_dir = data_dir.resolve(strict=True)
    if not SAFE_PATH.fullmatch(str(data_dir)):
        raise SetupError("The resolved data directory contains unsupported special characters; configure this custom service manually.")
    if os.uname().machine != "x86_64":
        raise SetupError("This pilot profile supports Linux x86-64 only.")
    user = run(["systemctl", "show", service, "--property=User", "--value"])
    account = pwd.getpwnam(user)
    if account.pw_uid == 0:
        raise SetupError("BOINC must already run as an unprivileged service user.")
    start = run(["systemctl", "show", service, "--property=ExecStart", "--value"])
    binary = re.search(r"path=([^ ;]+)", start)
    if binary is None or Path(binary.group(1)).name not in ["boinc", "boinc_client"]:
        raise SetupError("The service must start a stock BOINC client directly; wrapper/custom services need manual configuration.")
    version = re.match(r"(\d+)\.(\d+)\.(\d+)", run([binary.group(1), "--version"]))
    if version is None or tuple(map(int, version.groups())) < (8, 2, 13):
        raise SetupError("Install stock BOINC 8.2.13 or later before configuring this profile.")
    if Path(working).resolve() != data_dir or data_dir.stat().st_uid != account.pw_uid:
        raise SetupError("--data-dir must be the existing BOINC service working directory, owned by its user. This helper does not move account/task data.")
    if not Path("/sys/fs/cgroup/cgroup.controllers").exists():
        raise SetupError("This rootless profile requires cgroup v2.")
    for command in ["podman", "loginctl", "systemctl", "newuidmap", "newgidmap"]:
        if not shutil.which(command):
            raise SetupError(f"Install the prerequisite {command} before configuring this host.")
    for file in ["/etc/subuid", "/etc/subgid"]:
        check_subids(Path(file).read_text(), user, account.pw_uid)
    if active_work(data_dir):
        raise SetupError("BOINC has queued/running work. Select No new tasks and finish/report it before setup; no work will be reset.")
    slots = data_dir / "slots"
    if slots.is_symlink() or (slots.exists() and (not slots.is_dir() or slots.stat().st_uid != account.pw_uid)):
        raise SetupError("The BOINC slots path must be a real directory owned by the service user.")
    for path in [data_dir / "cc_config.xml", data_dir / "global_prefs_override.xml"]:
        if path.is_symlink():
            raise SetupError(f"{path} is a symlink. Configure its owner-managed target manually; this helper will not replace it.")
    trust = next((Path(path) for path in ["/etc/pki/tls/certs/ca-bundle.crt", "/etc/ssl/certs/ca-certificates.crt"] if Path(path).is_file()), None)
    if trust is None:
        raise SetupError("Install the distribution's ca-certificates package first.")
    selinux = bool(shutil.which("getenforce") and run(["getenforce"]) != "Disabled")
    if selinux and (not shutil.which("semanage") or not shutil.which("restorecon")):
        raise SetupError("Install SELinux policy-management tools (RHEL: policycoreutils-python-utils) first.")
    config_dir, dropin, containers, storage = profile(service, data_dir, image, account.pw_uid)
    cc = merge_xml(data_dir / "cc_config.xml", "cc_config", {"ncpus": "1", "no_gpus": "1"}, "options")
    prefs = merge_xml(data_dir / "global_prefs_override.xml", "global_preferences", PREFS)
    print(f"Service: {service}; unprivileged user: {user}; data stays at {data_dir}")
    print(f"Worker image: {image}")
    print("Install rootless Podman configuration and a mandatory startup slot/limits/pause check.")
    print("Set one BOINC CPU, no GPUs, idle after 5 minutes, no battery use, no task buffer, and 50% memory allowance (task containers remain capped at 1536 MiB).")
    print("Preserve accounts and unrelated preferences; back up changed XML before writing.")
    print("Enable the user's systemd session at boot; allow writable cgroups for rootless resource controls.")
    print(f"SELinux: {'label only ' + str(data_dir / 'slots') + ' as container_file_t' if selinux else 'disabled/not installed; no policy changes'}")
    if not apply:
        print("Preview only. Re-run with --apply to install, verify, and start this profile.")
        return
    if os.geteuid() != 0:
        raise SetupError("Installation needs root; preview does not change the system.")
    was_active = run(["systemctl", "show", service, "--property=ActiveState", "--value"]) == "active"
    run(["systemctl", "stop", service])
    if active_work(data_dir):
        if was_active:
            run(["systemctl", "start", service])
        raise SetupError("Work arrived before the service stopped; left its configuration unchanged. Drain it and retry.")
    config_dir.mkdir(parents=True, exist_ok=True)
    backups = config_dir / "backups"
    backups.mkdir(mode=0o700, exist_ok=True)
    os.chown(config_dir, 0, account.pw_gid)
    config_dir.chmod(0o750)
    backups.chmod(0o700)
    target = Path("/etc/systemd/system") / (service + ".d") / "zz-usnm-volunteer.conf"
    target.parent.mkdir(parents=True, exist_ok=True)
    for path in [data_dir / "cc_config.xml", data_dir / "global_prefs_override.xml", target]:
        backup(path, backups)
    write_file(config_dir / "containers.conf", containers, gid=account.pw_gid)
    write_file(config_dir / "storage.conf", storage, gid=account.pw_gid)
    preflight_dir = Path("/usr/local/lib/usnm-boinc-volunteer")
    preflight_dir.mkdir(parents=True, exist_ok=True)
    write_file(preflight_dir / "preflight.py", Path(__file__).with_name("volunteer_preflight.py").read_text(), mode=0o755)
    write_file(data_dir / "cc_config.xml", cc, uid=account.pw_uid, gid=account.pw_gid)
    write_file(data_dir / "global_prefs_override.xml", prefs, uid=account.pw_uid, gid=account.pw_gid)
    ca_bundle = data_dir / "ca-bundle.crt"
    if not ca_bundle.exists() and not ca_bundle.is_symlink():
        ca_bundle.symlink_to(trust)
    slots = data_dir / "slots"
    if not slots.exists():
        slots.mkdir(mode=0o771)
        os.chown(slots, account.pw_uid, account.pw_gid)
    if selinux:
        pattern = re.escape(str(slots)) + "(/.*)?"
        # Only replace a rule for this exact managed path; keep all other host policy.
        local = run(["semanage", "fcontext", "--list", "--locallist"])
        operation = "--modify" if any(line.split() and line.split()[0] == pattern for line in local.splitlines()) else "--add"
        run(["semanage", "fcontext", operation, "--type", "container_file_t", pattern])
        run(["restorecon", "-RF", str(slots)])
    run(["loginctl", "enable-linger", user])
    run(["systemctl", "start", f"user@{account.pw_uid}.service"])
    write_file(target, dropin, mode=0o644)
    run(["systemctl", "daemon-reload"])
    check_effective(service, data_dir, config_dir, account.pw_uid)
    run(["systemctl", "start", service])
    print("Profile installed; BOINC started only after its preflight passed. Attach through BOINC Manager, then check one task's upload and validation.")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--service", default="boinc-client.service")
    parser.add_argument("--data-dir", type=Path, help="Defaults to the service's existing working directory; never moves data")
    parser.add_argument("--image", help="Defaults to the worker digest released in this checkout's prod.json")
    parser.add_argument("--apply", action="store_true")
    args = parser.parse_args()
    try:
        image = args.image
        if image is None:
            settings = Path(__file__).resolve().parents[2] / "contrib-compute/infra/boinc/env/prod.json"
            image = json.loads(settings.read_text())["images"]["worker"]
        setup(args.service, args.data_dir, image, args.apply)
    except (SetupError, OSError, KeyError, TypeError, ValueError, ET.ParseError, subprocess.TimeoutExpired) as error:
        print(f"Volunteer setup failed: {error}", file=sys.stderr)
        print("After an installation failure, BOINC may remain stopped. Inspect journalctl -u " + args.service + " and the setup backups before retrying.", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
