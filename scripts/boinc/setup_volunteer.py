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
START_TIMEOUT = 660
PREFS = {
    "run_on_batteries": "0", "run_if_user_active": "0", "run_gpu_if_user_active": "0",
    "idle_time_to_run": "5", "max_ncpus_pct": "100", "cpu_usage_limit": "100",
    "suspend_cpu_usage": "0", "leave_apps_in_memory": "1",
    "work_buf_min_days": "0", "work_buf_additional_days": "0",
    "ram_max_used_busy_pct": "50", "ram_max_used_idle_pct": "50",
}


class SetupError(RuntimeError):
    pass


def run(args, timeout=180, user=None, group=None):
    result = subprocess.run(args, capture_output=True, text=True, timeout=timeout, user=user, group=group)
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


def check_activity_mode(data_dir):
    path = data_dir / "client_state.xml"
    if not path.exists():
        return
    root = ET.parse(path).getroot()
    for tag in ["user_run_request", "user_gpu_request"]:
        value = (root.findtext(tag) or "").strip()
        if value and value != "2":
            raise SetupError(f"BOINC {tag} is not set to 'Run based on preferences' (2). Select 'Run based on preferences' in BOINC Manager before setup.")


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
        sections = root.findall(section)
        if len(sections) > 1:
            raise SetupError(f"Duplicate {section} sections in {path}; resolve them first.")
        target = sections[0] if sections else ET.SubElement(root, section)
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
[Unit]
Requires=user@{uid}.service
After=user@{uid}.service

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
ExecStartPre=/usr/bin/python3 {preflight} --data-dir={data_dir} --image={image} --service={service}
"""
    # Separate, service-selected config; do not replace a user's global Podman configuration.
    containers = '[engine]\ncgroup_manager="systemd"\nconmon_cgroup="pod"\n'
    storage = f'[storage]\ndriver="overlay"\nrunroot="/run/{runtime}/containers"\ngraphroot="{data_dir}/usnm-podman-storage"\n'
    return config_dir, dropin, containers, storage


EXEC_BLOCK = re.compile(r"\{((?:[^{}]|\$\{[A-Za-z_][A-Za-z0-9_]*\})*)\}")
ENV_FILE_PAIR = re.compile(r"\s*([^\s()]+)\s+\(ignore_errors=(yes|no)\)\s*")


def read_environment_files(service):
    raw = run(["systemctl", "show", service, "--property=EnvironmentFiles", "--value"]).strip()
    if not raw:
        return []
    pairs = []
    pos = 0
    while pos < len(raw):
        match = ENV_FILE_PAIR.match(raw, pos)
        if not match:
            raise SetupError("Unsupported EnvironmentFiles syntax in service configuration; configure this custom service manually.")
        pairs.append((match.group(1), match.group(2) == "yes"))
        pos = match.end()
    entries = []
    for entry, ignore_errors in pairs:
        if entry.startswith("-"):
            ignore_errors = True
            entry = entry[1:].strip()
        if not SAFE_PATH.fullmatch(entry):
            raise SetupError("Unsupported EnvironmentFile path in service configuration; configure this custom service manually.")
        env_path = Path(entry)
        if not env_path.is_file():
            if not ignore_errors:
                raise SetupError(f"Required EnvironmentFile {env_path} is missing. Inspect systemctl cat before starting BOINC.")
            continue
        for line in env_path.read_text().splitlines():
            stripped = line.strip()
            if not stripped or stripped.startswith(("#", ";")):
                continue
            if stripped.startswith("export "):
                stripped = stripped[len("export "):].strip()
            key, sep, value = stripped.partition("=")
            key, value = key.strip(), value.strip()
            if len(value) >= 2 and value[0] == value[-1] and value[0] in ("'", '"'):
                value = value[1:-1]
            entries.append((key, sep, value))
    return entries


def resolve_launch(service):
    working = run(["systemctl", "show", service, "--property=WorkingDirectory", "--value"])
    start = run(["systemctl", "show", service, "--property=ExecStart", "--value"])
    blocks = EXEC_BLOCK.findall(start)
    if len(blocks) != 1 or EXEC_BLOCK.sub("", start).strip():
        raise SetupError("The service must start a single stock BOINC client command; wrapper/custom services need manual configuration.")
    fields = dict(part.strip().split("=", 1) for part in blocks[0].split(";") if "=" in part)
    binary = fields.get("path", "")
    argv = shlex.split(fields.get("argv[]", ""))
    if not binary or Path(binary).name not in ["boinc", "boinc_client"] or not argv or Path(argv[0]).name not in ["boinc", "boinc_client"]:
        raise SetupError("The service must start a stock BOINC client directly; wrapper/custom services need manual configuration.")
    env_vars = {}
    for item in shlex.split(run(["systemctl", "show", service, "--property=Environment", "--value"])):
        if "=" in item:
            key, value = item.split("=", 1)
            env_vars[key] = value
    for key, sep, value in read_environment_files(service):
        if sep:
            env_vars[key] = value
    for token in shlex.split(run(["systemctl", "show", service, "--property=UnsetEnvironment", "--value"])):
        key, sep, value = token.partition("=")
        if not sep or env_vars.get(key) == value:
            env_vars.pop(key, None)
    expanded = []
    for arg in argv[1:]:
        match = re.fullmatch(r"\$(?:([A-Za-z_][A-Za-z0-9_]*)|\{([A-Za-z_][A-Za-z0-9_]*)\})", arg)
        if match:
            var_name = match.group(1) or match.group(2)
            expanded.extend(shlex.split(env_vars.get(var_name, "")))
        elif "$" in arg:
            raise SetupError("Unsupported variable expansion in ExecStart; configure this custom service manually.")
        else:
            expanded.append(arg)
    dir_overrides = []
    if env_vars.get("BOINC_DIR"):
        dir_overrides.append(env_vars["BOINC_DIR"])
    index = 0
    while index < len(expanded):
        arg = expanded[index]
        if arg in ["--dir", "-dir"]:
            if index + 1 >= len(expanded):
                raise SetupError("ExecStart specifies --dir without a directory path.")
            dir_overrides.append(expanded[index + 1])
            index += 2
        elif arg.startswith("--dir=") or arg.startswith("-dir="):
            dir_overrides.append(arg.split("=", 1)[1])
            index += 1
        else:
            index += 1
    return binary, working, dir_overrides


def check_effective(service, data_dir, config_dir, uid, image=None):
    user_unit = f"user@{uid}.service"
    requires = run(["systemctl", "show", service, "--property=Requires", "--value"]).split()
    after = run(["systemctl", "show", service, "--property=After", "--value"]).split()
    if user_unit not in requires or user_unit not in after:
        raise SetupError(f"The service must require and start after {user_unit}.")
    _, working, dir_overrides = resolve_launch(service)
    resolved_data = data_dir.resolve()
    if not working or Path(working).resolve() != resolved_data:
        raise SetupError("Another service drop-in overrides WorkingDirectory. Inspect systemctl cat and resolve the conflict before starting BOINC.")
    for override in dir_overrides:
        candidate = Path(override) if Path(override).is_absolute() else Path(working) / override
        if candidate.resolve() != resolved_data:
            raise SetupError("The service overrides BOINC's data directory via --dir or BOINC_DIR; match --data-dir and WorkingDirectory before starting BOINC.")
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
    unset = shlex.split(run(["systemctl", "show", service, "--property=UnsetEnvironment", "--value"]))
    if any(token.split("=", 1)[0] in expected for token in unset):
        raise SetupError("Another service drop-in unsets the managed Podman environment. Inspect systemctl cat and resolve the conflict before starting BOINC.")
    for key, sep, value in read_environment_files(service):
        if key in expected and (not sep or value != expected[key]):
            raise SetupError("An EnvironmentFile overrides the managed Podman environment. Inspect systemctl cat and resolve the conflict before starting BOINC.")
    if run(["systemctl", "show", service, "--property=ProtectControlGroups", "--value"]) not in ["no", "false"]:
        raise SetupError("Another service drop-in makes cgroups read-only. Resolve that conflict before starting BOINC.")
    if run(["systemctl", "show", service, "--property=RuntimeDirectoryPreserve", "--value"]) != "yes":
        raise SetupError("The service must preserve Podman runtime files across a restart.")
    commands = run(["systemctl", "show", service, "--property=ExecStartPre", "--value"])
    preflight_path = "/usr/local/lib/usnm-boinc-volunteer/preflight.py"
    blocks = EXEC_BLOCK.findall(commands)
    matched = 0
    for block in blocks:
        fields = dict(part.strip().split("=", 1) for part in block.split(";") if "=" in part)
        argv = shlex.split(fields.get("argv[]", ""))
        if preflight_path not in block and preflight_path not in argv:
            continue
        matched += 1
        valid_argv = (
            len(argv) == 5
            and argv[:3] == ["/usr/bin/python3", preflight_path, f"--data-dir={data_dir}"]
            and (argv[3] == f"--image={image}" if image is not None else argv[3].startswith("--image=") and bool(IMAGE.fullmatch(argv[3].removeprefix("--image="))))
            and argv[4] == f"--service={service}"
        )
        if fields.get("path") != "/usr/bin/python3" or not valid_argv or fields.get("ignore_errors") != "no":
            raise SetupError("Another service drop-in altered or weakened the startup preflight. Resolve that conflict before starting BOINC.")
    if not matched:
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
    target = directory / path.name
    absent = directory / (path.name + ".absent")
    if target.exists() or absent.exists():
        return
    if path.exists():
        write_file(target, path.read_text(), mode=0o600)
    else:
        write_file(absent, "", mode=0o600)


def prepare_slots(data_dir, uid, gid, trust=None):
    parent_fd = os.open(data_dir, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        if os.fstat(parent_fd).st_uid != uid:
            raise SetupError("--data-dir must be owned by the BOINC service user.")
        if trust is not None:
            try:
                os.symlink(trust, "ca-bundle.crt", dir_fd=parent_fd)
            except FileExistsError:
                pass
        try:
            os.mkdir("slots", 0o771, dir_fd=parent_fd)
        except FileExistsError:
            pass
        try:
            slots_fd = os.open("slots", os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=parent_fd)
        except OSError as error:
            raise SetupError("The BOINC slots path must be a real directory owned by the service user.") from error
        try:
            st = os.fstat(slots_fd)
            if st.st_uid not in (0, uid):
                raise SetupError("The BOINC slots path must be a real directory owned by the service user.")
            os.fchown(slots_fd, uid, gid)
            os.fchmod(slots_fd, 0o771)
        finally:
            os.close(slots_fd)
    finally:
        os.close(parent_fd)


def setup(service, data_dir, image, apply=False):
    if not SERVICE.fullmatch(service) or (data_dir is not None and not SAFE_PATH.fullmatch(str(data_dir))) or not IMAGE.fullmatch(image):
        raise SetupError("Use a plain systemd service name, an absolute path without spaces/special characters, and a registry image pinned by full sha256 digest.")
    binary, working, dir_overrides = resolve_launch(service)
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
    version = re.match(r"(\d+)\.(\d+)\.(\d+)", run([binary, "--version"]))
    if version is None or tuple(map(int, version.groups())) < (8, 2, 13):
        raise SetupError("Install stock BOINC 8.2.13 or later before configuring this profile.")
    if Path(working).resolve() != data_dir or data_dir.stat().st_uid != account.pw_uid:
        raise SetupError("--data-dir must be the existing BOINC service working directory, owned by its user. This helper does not move account/task data.")
    for override in dir_overrides:
        candidate = Path(override) if Path(override).is_absolute() else Path(working) / override
        if not candidate.exists() or candidate.resolve() != data_dir:
            raise SetupError("The service overrides BOINC's data directory via --dir or BOINC_DIR; match --data-dir and WorkingDirectory before configuring this profile.")
    if not Path("/sys/fs/cgroup/cgroup.controllers").exists():
        raise SetupError("This rootless profile requires cgroup v2.")
    for command in ["podman", "loginctl", "systemctl", "newuidmap", "newgidmap"]:
        if not shutil.which(command):
            raise SetupError(f"Install the prerequisite {command} before configuring this host.")
    for file in ["/etc/subuid", "/etc/subgid"]:
        check_subids(Path(file).read_text(), user, account.pw_uid)
    if active_work(data_dir):
        raise SetupError("BOINC has queued/running work. Select No new tasks and finish/report it before setup; no work will be reset.")
    check_activity_mode(data_dir)
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
    try:
        if active_work(data_dir):
            raise SetupError("Work arrived before the service stopped; left its configuration unchanged. Drain it and retry.")
        check_activity_mode(data_dir)
        for path in [data_dir / "cc_config.xml", data_dir / "global_prefs_override.xml"]:
            if path.is_symlink():
                raise SetupError(f"{path} is a symlink. Configure its owner-managed target manually; this helper will not replace it.")
        cc = merge_xml(data_dir / "cc_config.xml", "cc_config", {"ncpus": "1", "no_gpus": "1"}, "options")
        prefs = merge_xml(data_dir / "global_prefs_override.xml", "global_preferences", PREFS)
    except Exception:
        if was_active:
            run(["systemctl", "start", service], timeout=START_TIMEOUT)
        raise
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
    prepare_slots(data_dir, account.pw_uid, account.pw_gid, trust)
    if selinux:
        pattern = re.escape(str(slots)) + "(/.*)?"
        # Only replace a rule for this exact managed path; keep all other host policy.
        local = run(["semanage", "fcontext", "--list", "--locallist"])
        operation = "--modify" if any(line.split() and line.split()[0] == pattern for line in local.splitlines()) else "--add"
        run(["semanage", "fcontext", operation, "--type", "container_file_t", pattern])
        run(["restorecon", "-RF", str(slots)], user=account.pw_uid, group=account.pw_gid)
    run(["loginctl", "enable-linger", user])
    run(["systemctl", "start", f"user@{account.pw_uid}.service"])
    write_file(target, dropin, mode=0o644)
    run(["systemctl", "daemon-reload"])
    check_effective(service, data_dir, config_dir, account.pw_uid, image)
    run(["systemctl", "start", service], timeout=START_TIMEOUT)
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
