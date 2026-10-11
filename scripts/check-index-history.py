#!/usr/bin/env python3
"""Check the reconstructed index build records (ops/index-history.json)
against their schema (ops/index-history.schema.json), and what a schema
can't say: every object's keys are sorted ascending and unique (as
scripts/reconstruct-index-history.py writes them, like the other keyed data
files), and each run's image was pushed before the run started.

    scripts/check-index-history.py [--path FILE]    (default: ops/index-history.json)

Run it on the reconstruction's output before replacing the file (see
scripts/reconstruct-index-history.py). Needs fastjsonschema
(scripts/ci/requirements-ops.txt). CI runs it whenever the file, its schema,
this script or the reconstruction changes. Exits 1 listing every problem.
"""

import argparse
import json
import re
import sys
from datetime import datetime
from pathlib import Path

import fastjsonschema

ROOT = Path(__file__).resolve().parent.parent
HISTORY = ROOT / "ops" / "index-history.json"
SCHEMA = ROOT / "ops" / "index-history.schema.json"


def when(s: str) -> datetime:
    # Python reads at most microseconds; the runs record nanoseconds.
    return datetime.fromisoformat(re.sub(r"(\.\d{6})\d*", r"\1", s.replace("Z", "+00:00")))


def order_problems(pairs_seen: list[tuple[str, list[str]]]) -> list[str]:
    out = []
    for where, keys in pairs_seen:
        if len(set(keys)) != len(keys):
            out.append(f"{where}: repeated keys")
        elif keys != sorted(keys):
            out.append(f"{where}: keys out of order: {', '.join(keys)}")
    return out


def load(text: str) -> tuple[object, list[tuple[str, list[str]]]]:
    """The document, and every object's keys in file order (with where it is,
    as the keys around it: the first one's)."""
    seen = []

    def hook(pairs):
        keys = [k for k, _ in pairs]
        seen.append((f"object with {keys[0]!r}" if keys else "empty object", keys))
        return dict(pairs)

    return json.loads(text, object_pairs_hook=hook), seen


def problems(path: Path, validate) -> list[str]:
    try:
        doc, seen = load(path.read_text(encoding="utf-8"))
    except ValueError as e:
        return [f"not JSON: {e}"]
    out = order_problems(seen)
    try:
        validate(doc)
    except fastjsonschema.JsonSchemaValueException as e:
        return out + [f"schema: {e.message} (at {'/'.join(map(str, e.path))})"]
    for version, b in doc.items():
        # The schema checks the form, not the calendar (2026-99-99 fits it).
        try:
            pushed, started = when(b["image"]["pushed_at"]), when(b["run_started_at"])
        except ValueError as e:
            out.append(f"{version}: not a real time: {e}")
            continue
        if pushed > started:
            out.append(f"{version}: image pushed at {b['image']['pushed_at']}, after the run started at "
                       f"{b['run_started_at']}")
    return out


def main() -> int:
    validate = fastjsonschema.compile(json.loads(SCHEMA.read_text(encoding="utf-8")))
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--path", type=Path, help="the history to check (default ops/index-history.json)")
    # The positional form (`check-index-history.py FILE`) works for one
    # release, with a warning (#277).
    ap.add_argument("legacy_path", nargs="?", type=Path, help=argparse.SUPPRESS)
    a = ap.parse_args()
    if a.legacy_path is not None:
        if a.path is not None:
            ap.error("give the file once, as --path")
        print("warning: a positional file is deprecated; use --path (#277)", file=sys.stderr)
        a.path = a.legacy_path
    path = a.path or HISTORY
    found = problems(path, validate)
    for p in found:
        print(f"{path.name}: {p}", file=sys.stderr)
    if not found:
        print(f"{path.name}: ok")
    return 1 if found else 0


if __name__ == "__main__":
    sys.exit(main())
