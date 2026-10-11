#!/usr/bin/env python3
"""Check the index version snapshots (every ops/version-snapshots/*.json but the schema)
against their schema (ops/version-snapshots/schema.json), and what a schema
can't say: the file is named after its index_version, and each answered
search's per-bucket hits number the bucket count and add up to its total.

    scripts/check-version-snapshots.py [--path FILE ...]    (default: every snapshot, whatever its name)

Needs fastjsonschema (scripts/ci/requirements-ops.txt). CI runs it whenever a
snapshot, the schema, this script or scripts/compare-versions.py changes.
Exits 1 listing every problem.
"""

import argparse
import json
import sys
from pathlib import Path

import fastjsonschema

ROOT = Path(__file__).resolve().parent.parent
DIR = ROOT / "ops" / "version-snapshots"


def problems(path: Path, validate) -> list[str]:
    try:
        doc = json.loads(path.read_text(encoding="utf-8"))
    except ValueError as e:
        return [f"not JSON: {e}"]
    try:
        validate(doc)
    except fastjsonschema.JsonSchemaValueException as e:
        return [f"schema: {e.message} (at {'/'.join(map(str, e.path))})"]
    out = []
    if path.stem != doc["index_version"]:
        out.append(f"named {path.name} but captures {doc['index_version']}")
    for name, s in doc["searches"].items():
        if s["status"] != 200:
            continue
        hits, count = s["series_hits"], s["bucket"]["count"]
        if len(hits) != count:
            out.append(f"{name}: {len(hits)} series values for {count} buckets")
        if sum(hits) != s["total"]["hits"]:
            out.append(f"{name}: series adds up to {sum(hits)}, total says {s['total']['hits']}")
    return out


def main() -> int:
    validate = fastjsonschema.compile(json.loads((DIR / "schema.json").read_text(encoding="utf-8")))
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--path", type=Path, action="append", default=[],
                    help="a snapshot to check (repeatable; default every snapshot)")
    # The positional form (`check-version-snapshots.py FILE ...`) works for
    # one release, with a warning (#277).
    ap.add_argument("legacy_paths", nargs="*", type=Path, help=argparse.SUPPRESS)
    a = ap.parse_args()
    if a.legacy_paths:
        print("warning: positional files are deprecated; use --path (#277)", file=sys.stderr)
    paths = a.path + a.legacy_paths or sorted(p for p in DIR.glob("*.json") if p.name != "schema.json")
    if not paths:
        print("no snapshots to check", file=sys.stderr)
        return 1
    bad = 0
    for path in paths:
        found = problems(path, validate)
        for p in found:
            print(f"{path.name}: {p}", file=sys.stderr)
        bad += bool(found)
        if not found:
            print(f"{path.name}: ok")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
