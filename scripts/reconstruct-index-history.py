#!/usr/bin/env python3
"""Reconstruct the build records of index versions released before #161.

    scripts/reconstruct-index-history.py --env prod --merge ops/index-history.json > /tmp/h.json \
      && scripts/check-index-history.py --path /tmp/h.json && mv /tmp/h.json ops/index-history.json

(scripts/check-index-history.py checks the output against
ops/index-history.schema.json, as CI checks the committed file.)

Releases record what built them since #161 (`build` in the version's
manifest and its `index_runs` item). For the versions before that, this
works it out:

- The commit: every ingest execution ran `usnewsmap-ingest:main`, which CI
  pushes with a tag per commit. A run's commit is the last tag pushed before
  the execution it ran in started (a run can start hours into its
  execution, after titles-sync, so the run's own start time won't do).
- Everything else from that commit in git: the ingest crate version, the
  Quickwit image `Dockerfile.ingest` pins, the feature versions in
  `usnm-core`, and the index templates' sha256 (`pages`, and `pages-ja`
  once releases built a Japanese index).

Reads every run from the live `/v1/versions` (`--from-status` reads
`/v1/status` instead, for an API without that endpoint: its latest 20 runs
only), the executions and image tags with `az` (signed in as the operator),
and git from this checkout (fetch first). Runs whose build was recorded are
left out. `--merge` keeps the given file's entries for runs this reading
doesn't cover, so older history isn't lost. Each entry records the run's
start time (`run_started_at`): the API gives a reconstructed record only to
the run with that exact start, since version names repeat across
environments. Prints JSON with sorted keys; fails rather than print an empty
history when the pipeline state isn't readable.
"""

import argparse
import hashlib
import json
import re
import subprocess
import sys
import urllib.request
from datetime import datetime
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
NOTE = ("Reconstructed: the commit is the last usnewsmap-ingest image pushed before the execution "
        "started (every execution ran usnewsmap-ingest:main); the rest is from that commit in git.")


def setting(env: str, key: str) -> str:
    return subprocess.run([str(ROOT / "scripts/settings.sh"), "--env", env, "--key", key], capture_output=True,
                          text=True, check=True).stdout.strip()


def az(*args: str):
    out = subprocess.run(["az", *args, "-o", "json"], capture_output=True, text=True, check=True).stdout
    return json.loads(out)


def git(*args: str) -> str | None:
    r = subprocess.run(["git", "-C", str(ROOT), *args], capture_output=True, text=True)
    return r.stdout if r.returncode == 0 else None


def when(s: str) -> datetime:
    s = re.sub(r"(\.\d{6})\d*", r"\1", s.replace("Z", "+00:00"))
    return datetime.fromisoformat(s)


def const(commit: str, path: str, name: str) -> int | None:
    src = git("show", f"{commit}:{path}")
    m = src and re.search(rf"pub const {name}: u32 = (\d+);", src)
    return int(m.group(1)) if m else None


def sha256_at(commit: str, path: str) -> str | None:
    src = git("show", f"{commit}:{path}")
    return hashlib.sha256(src.encode()).hexdigest() if src is not None else None


def sort_keys(o):
    if isinstance(o, dict):
        return {k: sort_keys(o[k]) for k in sorted(o)}
    if isinstance(o, list):
        return [sort_keys(x) for x in o]
    return o


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--env", help="the environment (default prod)")
    ap.add_argument("--site", default="https://usnewsmap.com")
    # Without these, from scripts/settings.sh (which reads .azure/<env>/.env).
    ap.add_argument("--from-status", action="store_true")
    ap.add_argument("--merge", type=Path, help="an existing history to keep entries from")
    ap.add_argument("--resource-group")
    ap.add_argument("--job")
    ap.add_argument("--acr")
    # The positional form (`reconstruct-index-history.py ENV`) works for one
    # release, with a warning (#277).
    ap.add_argument("legacy_env", nargs="?", help=argparse.SUPPRESS)
    a = ap.parse_args()
    if a.legacy_env is not None:
        if a.env is not None:
            ap.error("give the environment once, as --env")
        print("warning: a positional environment is deprecated; use --env (#277)", file=sys.stderr)
        a.env = a.legacy_env
    a.env = a.env or "prod"
    rg = a.resource_group or setting(a.env, "AZURE_RESOURCE_GROUP")
    job = a.job or setting(a.env, "INGEST_JOB")
    acr = a.acr or setting(a.env, "ACR_NAME")
    if a.from_status:
        with urllib.request.urlopen(f"{a.site}/v1/status", timeout=30) as r:
            doc = json.load(r)
        if not doc["indexing"].get("available", True) or "runs" not in doc["indexing"]:
            sys.exit("/v1/status has no pipeline state: nothing to reconstruct from")
        runs = doc["indexing"]["runs"]
    else:
        with urllib.request.urlopen(f"{a.site}/v1/versions", timeout=30) as r:
            doc = json.load(r)
        if not doc.get("available"):
            sys.exit(f"/v1/versions has no pipeline state ({doc.get('reason')}): nothing to reconstruct from")
        runs = [v for v in doc["versions"] if v.get("build_source") != "recorded"]
    executions = []
    for e in az("containerapp", "job", "execution", "list", "-n", job, "-g", rg):
        image = e["properties"]["template"]["containers"][0]["image"]
        if not image.endswith("usnewsmap-ingest:main"):
            sys.exit(f"{e['name']} ran {image}, not usnewsmap-ingest:main: work this one out by hand")
        executions.append((when(e["properties"]["startTime"]), e["name"]))
    tags = [(when(t["lastUpdateTime"]), t["name"], t["digest"])
            for t in az("acr", "repository", "show-tags", "-n", acr, "--repository", "usnewsmap-ingest",
                        "--detail")
            if re.fullmatch(r"[0-9a-f]{40}", t["name"])]
    out = json.loads(a.merge.read_text()) if a.merge else {}
    for run in runs:
        started = when(run["started_at"])
        ex_start, ex_name = max((e for e in executions if e[0] <= started), default=(None, None))
        if ex_name is None:
            sys.exit(f"no execution found for {run['index_version']}")
        pushed, commit, digest = max(t for t in tags if t[0] <= ex_start)
        qw = re.search(r"quickwit/quickwit:v([\d.]+)", git("show", f"{commit}:Dockerfile.ingest") or "")
        cargo = re.search(r'^version = "([^"]+)"', git("show", f"{commit}:crates/usnm-ingest/Cargo.toml") or "",
                          re.M)
        templates = {"pages": {"sha256": sha256_at(commit, "infra/quickwit/pages-index.yaml")}}
        # Releases from this commit build a Japanese index whenever the OCR overlay has pages.
        if "build_ja_index" in (git("show", f"{commit}:crates/usnm-ingest/src/release.rs") or ""):
            templates["pages-ja"] = {"sha256": sha256_at(commit, "infra/quickwit/pages-ja-index.yaml")}
        out[run["index_version"]] = {
            "commit": commit,
            "engine": f"Quickwit {qw.group(1)}" if qw else None,
            "execution": ex_name,
            "features": {
                "common_grams": const(commit, "crates/usnm-core/src/common_grams.rs", "VERSION"),
                "ja_fold": const(commit, "crates/usnm-core/src/ja.rs", "FOLD_VERSION"),
            },
            "full": run["full"],
            "image": {"digest": digest, "pushed_at": pushed.strftime("%Y-%m-%dT%H:%M:%SZ"),
                      "tag": "usnewsmap-ingest:main"},
            "ingest": cargo.group(1) if cargo else None,
            "reconstructed": NOTE,
            "run_started_at": run["started_at"],
            "templates": templates,
        }
    json.dump(sort_keys(out), sys.stdout, indent=2, ensure_ascii=False)
    print()


if __name__ == "__main__":
    main()
