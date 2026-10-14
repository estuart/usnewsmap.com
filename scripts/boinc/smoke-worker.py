#!/usr/bin/env python3
"""Run a native synthetic worker/collector round trip without a BOINC server."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import time


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--runtime", default="docker")
    p.add_argument("--image", required=True)
    p.add_argument("--build-image", required=True)
    p.add_argument("--arch", choices=["amd64", "arm64"], required=True)
    p.add_argument("--dir", type=Path, required=True)
    a = p.parse_args()
    root = a.dir.resolve()
    root.mkdir(exist_ok=False)

    def call(*args, check=True):
        return subprocess.run([a.runtime, *map(str, args)], capture_output=True, text=True, check=check)

    info = json.loads(call("image", "inspect", a.image).stdout)[0]
    assert info["Architecture"] == a.arch, "worker architecture differs from the native runner"
    build = json.loads(call("image", "inspect", a.build_image).stdout)[0]
    assert build["Architecture"] == a.arch, "fixture generator must be native too"
    image_id = "sha256:" + info["Id"].removeprefix("sha256:")
    owner_args = (["--userns=keep-id"] if Path(a.runtime).name == "podman"
                  else ["--user", f"{os.getuid()}:{os.getgid()}"])
    call("run", "--rm", "--network=none", *owner_args, "-v", f"{root}:/fixture", a.build_image,
         "/src/target/release/examples/pilot_fixture", "--dir", "/fixture")

    def worker(*args, check=True, host_owner=False):
        return call("run", "--rm", "--network=none", "--read-only", "--cap-drop=ALL",
                    "--cap-add=DAC_OVERRIDE", "--security-opt=no-new-privileges", "--cpus=1",
                    "--memory=1536m", "--memory-swap=1536m", "--pids-limit=64",
                    "--tmpfs=/tmp:rw,size=512m", *(owner_args if host_owner else []),
                    "-v", f"{root}:/fixture", a.image,
                    "/usr/local/bin/usnm-boinc", *args, check=check)

    job_id = worker("export", "--manifest", "/fixture/manifest.parquet", "--pages", "/fixture/pages.txt",
                    "--batch", "test_pilot", "--scans", "/fixture/scans", "--root", "/fixture/dispatch",
                    "--plan", "native-ci", "--image", image_id, "--curated-url", "/fixture/curated",
                    "--reference-url", "/fixture/reference", "--state-file", "/fixture/state.json", host_owner=True).stdout.strip()
    job_dir = root / "dispatch/jobs" / job_id
    started = time.monotonic()
    worker("work", "--dir", f"/fixture/dispatch/jobs/{job_id}")
    result = json.loads((job_dir / "result.json").read_text())
    assert (job_dir / "text.txt").read_text().strip() == "HELLO WORLD"
    for name, field in [("text.txt", "text_sha256"), ("layout.xml", "alto_sha256")]:
        assert hashlib.sha256((job_dir / name).read_bytes()).hexdigest() == result[field]
    for name in ("text.txt", "layout.xml", "result.json"):
        assert (job_dir / name).stat().st_mode & 0o777 == 0o644

    args = ["collect", "--root", "/fixture/dispatch", "--spool", f"/fixture/dispatch/jobs/{job_id}",
            "--job", job_id, "--curated-url", "/fixture/curated", "--out-url", "/fixture/out"]
    for name in ("text.txt", "layout.xml", "result.json"):
        (job_dir / f"usnm_{job_id}__file_{name}").write_bytes((job_dir / name).read_bytes())
    manifest = job_dir / f"usnm_{job_id}__file_result.json"
    original = manifest.read_bytes()
    forged = json.loads(original)
    forged["engine"]["image"] = "sha256:" + "0" * 64
    manifest.write_text(json.dumps(forged))
    refused = worker(*args, check=False, host_owner=True)
    assert refused.returncode and "provenance mismatch" in refused.stderr
    manifest.write_bytes(original)
    worker(*args, host_owner=True)

    def outputs():
        return {str(f.relative_to(root / "out")): hashlib.sha256(f.read_bytes()).hexdigest()
                for f in (root / "out").rglob("*") if f.is_file()}

    before = outputs()
    assert sum(n.endswith(".xml") for n in before) == 1
    assert sum(n.endswith(".parquet") for n in before) == 1
    worker(*args, host_owner=True)
    assert outputs() == before, "collection replay changed published objects"
    print(json.dumps({"arch": a.arch, "text": "HELLO WORLD", "worker_elapsed_ms": result["elapsed_ms"],
                      "round_trip_seconds": round(time.monotonic() - started, 3), "replay": "unchanged"}))


if __name__ == "__main__":
    main()
