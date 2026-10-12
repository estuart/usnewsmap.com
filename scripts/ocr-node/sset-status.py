#!/usr/bin/env python3
"""Progress of the on-prem Serial Set fetch (usnm-sset.service, #229).

    sset-status.py [--unit usnm-sset.service] [--out /var/lib/usnm-sset/status.json]
        [--done /var/lib/usnm-sset/done] [--push-url http://<dashboard>/api/sset/<name>]

Reads the unit's journal since it last started and writes a status file:
volumes to fetch, written (curated or empty), incomplete, the rates per
hour and an estimate of the time left. Volumes range from one document to
hundreds, so the estimate goes by documents: the volumes left times the
average documents a volume has, at the documents-per-hour rate so far.
Sets "finished" when the --done marker exists. With --push-url it also
PUTs the status to the OCR dashboard (usnm-ocrdash), which takes it from this
node's address only. Prints it too. usnm-sset-status.timer runs it every 10 minutes.
"""

import argparse
import json
import os
import subprocess
import sys
import tempfile
import urllib.request
from datetime import datetime, timezone


def started(unit: str) -> datetime | None:
    out = subprocess.run(
        ["systemctl", "show", unit, "--property=ExecMainStartTimestamp", "--timestamp=unix"],
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()
    value = out.partition("=")[2].lstrip("@")
    return datetime.fromtimestamp(int(value), timezone.utc) if value.isdigit() else None


def previous(path: str) -> dict:
    try:
        with open(path) as f:
            data = json.load(f)
    except (OSError, ValueError):
        return {}
    return data if isinstance(data, dict) else {}


def status(unit: str, done: str, out_path: str) -> dict:
    since = started(unit)
    active = subprocess.run(["systemctl", "is-active", unit], capture_output=True, text=True).stdout.strip()
    out = {
        "unit": unit,
        "active": active,
        "finished": os.path.exists(done),
        "started": since.isoformat() if since else None,
    }
    if since is None:
        # Not started since boot (the done marker keeps it from starting):
        # keep the totals the last status had rather than zeros.
        return {**previous(out_path), **out, "started": previous(out_path).get("started")}
    lines = subprocess.run(
        ["journalctl", "-u", unit, "-o", "cat", "--no-pager", "--since", f"@{int(since.timestamp())}"],
        capture_output=True,
        text=True,
        check=True,
    ).stdout.splitlines()
    to_fetch = curated = empty = incomplete = pages = 0
    listed_volumes = listed_documents = documents_done = 0
    current = None
    current_documents = 0
    for line in lines:
        try:
            event = json.loads(line)
        except ValueError:
            continue
        if not isinstance(event, dict):
            continue
        fields = event.get("fields", {})
        message = fields.get("message", "")
        if message == "Serial Set volumes":
            to_fetch = int(fields.get("to_fetch", 0))
            listed_volumes = int(fields.get("volumes", 0))
            listed_documents = int(fields.get("documents", 0))
        elif message == "fetching a Serial Set volume":
            current = fields.get("volume")
            current_documents = int(fields.get("documents", 0))
        elif message == "Serial Set volume curated":
            curated += 1
            documents_done += current_documents
            pages += int(fields.get("pages", 0))
        elif message.startswith("every document of the volume is left out"):
            empty += 1
            documents_done += current_documents
        elif message.startswith("documents of the volume weren't read"):
            incomplete += 1
            documents_done += current_documents
    now = datetime.now(timezone.utc)
    hours = max((now - since).total_seconds() / 3600, 1e-9)
    done = curated + empty + incomplete
    per_hour = done / hours
    documents_per_hour = documents_done / hours
    left = max(to_fetch - done, 0)
    average = listed_documents / listed_volumes if listed_volumes else 0
    documents_left = left * average
    out.update(
        {
            "at": now.isoformat(),
            "to_fetch": to_fetch,
            "curated": curated,
            "empty": empty,
            "incomplete": incomplete,
            "pages": pages,
            "current_volume": current,
            "documents_read": documents_done,
            "volumes_per_hour": round(per_hour, 2),
            "documents_per_hour": round(documents_per_hour, 1),
            "left": left,
            "documents_left_estimate": round(documents_left),
            "eta_hours": round(documents_left / documents_per_hour, 1) if documents_per_hour > 0 else None,
        }
    )
    return out


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--unit", default="usnm-sset.service")
    p.add_argument("--out", default="/var/lib/usnm-sset/status.json")
    p.add_argument("--done", default="/var/lib/usnm-sset/done")
    p.add_argument("--push-url", default=None)
    args = p.parse_args()
    s = status(args.unit, args.done, args.out)
    body = json.dumps(s, indent=2) + "\n"
    directory = os.path.dirname(args.out) or "."
    with tempfile.NamedTemporaryFile("w", dir=directory, delete=False) as f:
        f.write(body)
    os.chmod(f.name, 0o644)
    os.replace(f.name, args.out)
    sys.stdout.write(body)
    if args.push_url:
        request = urllib.request.Request(
            args.push_url, data=body.encode(), method="PUT", headers={"Content-Type": "application/json"}
        )
        try:
            urllib.request.urlopen(request, timeout=10).close()
        except OSError as e:
            sys.stderr.write(f"warning: can't push to {args.push_url}: {e}\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
