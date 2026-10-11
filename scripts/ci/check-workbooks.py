#!/usr/bin/env python3
"""Check the Azure Monitor workbook definitions infra/modules/workbooks.bicep
loads: each file it names parses as JSON, has a workbook's top level (a
`version` string and a non-empty `items` array of objects), queries the
workspace through the module's placeholder (which provisioning replaces with
the environment's workspace) and holds no workspace resource id of its own
(left over from copying the JSON out of the portal).

    scripts/ci/check-workbooks.py [--module FILE]    (default: infra/modules/workbooks.bicep)

Reads the placeholder and the file names from the module, so they can't
drift from it. Standard library only. Exits 1 listing every problem.
"""

import argparse
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent
MODULE = ROOT / "infra" / "modules" / "workbooks.bicep"


def module_parts(src: str) -> tuple[str | None, list[str]]:
    """The placeholder and the files the module loads (relative to it)."""
    m = re.search(r"^var placeholder = '([^']+)'", src, re.M)
    return (m.group(1) if m else None), re.findall(r"loadTextContent\('([^']+)'\)", src)


def problems(path: Path, placeholder: str) -> list[str]:
    try:
        text = path.read_text(encoding="utf-8")
        doc = json.loads(text)
    except (OSError, ValueError) as e:
        return [f"not readable JSON: {e}"]
    out = []
    if not isinstance(doc, dict):
        return ["not a JSON object"]
    if not isinstance(doc.get("version"), str) or not doc["version"]:
        out.append("no `version` string")
    items = doc.get("items")
    if not isinstance(items, list) or not items:
        out.append("no `items` array, or an empty one")
    elif not all(isinstance(i, dict) for i in items):
        out.append("an item that isn't an object")
    if placeholder not in text:
        out.append(f"never names the workspace placeholder {placeholder}")
    if re.search(r"/subscriptions/[^/\"]+/resourcegroups/", text, re.I):
        out.append(f"holds a resource id: put {placeholder} back in its place")
    return out


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--module", type=Path, help="the Bicep module (default infra/modules/workbooks.bicep)")
    # The positional form (`check-workbooks.py FILE`) works for one release,
    # with a warning (#277).
    ap.add_argument("legacy_module", nargs="?", type=Path, help=argparse.SUPPRESS)
    a = ap.parse_args()
    if a.legacy_module is not None:
        if a.module is not None:
            ap.error("give the module once, as --module")
        print("warning: a positional module is deprecated; use --module (#277)", file=sys.stderr)
        a.module = a.legacy_module
    module = a.module or MODULE
    placeholder, files = module_parts(module.read_text(encoding="utf-8"))
    if placeholder is None or not files:
        print(f"{module}: no `var placeholder` or no loadTextContent: update this check", file=sys.stderr)
        return 1
    bad = 0
    for f in files:
        path = (module.parent / f).resolve()
        found = problems(path, placeholder)
        for p in found:
            print(f"{f}: {p}", file=sys.stderr)
        bad += bool(found)
        if not found:
            print(f"{f}: ok")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
