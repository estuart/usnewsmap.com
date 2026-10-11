#!/usr/bin/env python3
"""Fail on a script or command-line tool that reads positional arguments
(#277): scripts and CLIs here take named ones (`--env prod --key K`), which
fail loudly when they change, where a positional order breaks silently.

    scripts/ci/no-positional-args.py

Checks every tracked shell script, Python file and Rust file:

- shell: `$1`..`$9` (or `${1...}`) read outside a function and outside the
  flag-parsing loop (`while [ $# -gt 0 ]; do ... done`). A function's own
  arguments are not the script's. Comments, single-quoted text (awk and jq
  programs, `sh -c '...'`) and quoted heredocs don't count.
- Python: `sys.argv[...]`, and an argparse `add_argument` whose name isn't a
  flag. A positional with `choices` names a subcommand (as `jaocr.py run`),
  like `add_subparsers`, and is allowed.
- Rust: any read of `std::env::args()` (or `args_os()`): a CLI parses with
  clap, so a hand-written parser has to be listed in HAND_PARSED after its
  flags-only parsing was checked by eye. And a field of a clap derive
  (`Parser`, `Args`, `Subcommand`) that isn't `#[arg(long ...)]` (or
  short), a subcommand or a flattened group: clap makes it positional.

ALLOWED lists the deprecated positional forms kept for one release, and
HAND_PARSED the reviewed hand-written Rust parsers; the count must match
exactly, so an entry goes when its code does. Standard
library only. Exits 1 listing every finding.
"""

import ast
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent

# The positional forms kept for one release, with a warning, while callers
# move to the named ones (#277): findings per file. Remove each entry with
# its fallback (`grep -rn '#277'` finds them).
ALLOWED = {
    "scripts/bench-cold-searches.py": 1,
    "scripts/bootstrap.sh": 1,
    "scripts/check-index-history.py": 1,
    "scripts/check-version-snapshots.py": 1,
    "scripts/ci/check-api-image.sh": 2,
    "scripts/ci/check-ingest-image.sh": 2,
    "scripts/ci/check-ja-ocr-image.sh": 2,
    "scripts/ci/check-workbooks.py": 1,
    "scripts/ci/web-e2e.sh": 2,
    "scripts/compare-versions.py": 2,
    "scripts/fixture-batches.py": 1,
    "scripts/ja-ocr/build-wordlists.py": 1,
    "scripts/ja-ocr/crops.py": 1,
    "scripts/ja-ocr/quality-rows.sh": 2,
    "scripts/ja-ocr/run.py": 1,
    "scripts/ja-ocr/sample.py": 1,
    "scripts/ja-ocr/score.py": 1,
    "scripts/load-cold-searches.py": 1,
    "scripts/local-azure/down.sh": 2,
    "scripts/local-azure/up.sh": 2,
    "scripts/logs.sh": 2,
    "scripts/provision.sh": 2,
    "scripts/quickwit-fixtures.sh": 2,
    "scripts/qwcluster-report.py": 1,
    "scripts/reconstruct-index-history.py": 1,
    "scripts/searches.sh": 2,
    "scripts/settings.sh": 4,
    "scripts/teardown.sh": 1,
}

POSITIONAL = re.compile(r"(?<!\\)\$(?:[1-9]|\{[1-9])")
FUNCTION = re.compile(r"^(\s*)(?:function\s+)?[A-Za-z_][\w-]*\s*\(\)\s*[{(]?")
PARSE_LOOP = re.compile(r"^(\s*)while\s+(?:\[\[?\s*\$#\s+-gt\s+0\s*\]\]?|\(\(\s*\$#\s*\)\))\s*;\s*do\b")
HEREDOC = re.compile(r"<<-?\s*(['\"]?)([A-Za-z_]\w*)\1")


def code_parts(line: str, state: dict) -> str:
    """The line without comments and single-quoted text; `state` carries an
    open single quote over to the next line, and records where a comment
    starts."""
    state["comment"] = len(line)
    out, i, dq = [], 0, False
    while i < len(line):
        c = line[i]
        if state["sq"]:
            if c == "'":
                state["sq"] = False
            i += 1
            continue
        if c == "\\":
            out.append(line[i:i + 2])
            i += 2
            continue
        if c == '"':
            dq = not dq
        elif c == "'" and not dq:
            state["sq"] = True
            i += 1
            continue
        elif c == "#" and not dq and (i == 0 or line[i - 1] in " \t;(|&"):
            state["comment"] = i
            break
        out.append(c)
        i += 1
    return "".join(out)


def shell_findings(text: str) -> list[int]:
    """Line numbers that read the script's positional arguments."""
    found = []
    state = {"sq": False}
    blocks = []  # (indent, kind) of open functions and parsing loops
    heredoc = None  # (delimiter, quoted)
    for n, raw in enumerate(text.split("\n"), 1):
        if heredoc:
            if raw.strip() == heredoc[0]:
                heredoc = None
            elif not heredoc[1] and not blocks and POSITIONAL.search(raw):
                found.append(n)
            continue
        if state["sq"]:
            line = code_parts(raw, state)
            continue
        line = code_parts(raw, state)
        if blocks and re.match(rf"^{blocks[-1][0]}(?:\}}|\)|done)(?=[\s;|&>]|$)", line):
            blocks.pop()
            continue
        m = FUNCTION.match(line)
        if m:
            rest = line[m.end():]
            # A function whose body closes on the same line opens nothing.
            if line.rstrip().endswith(("{", "(")) or not rest.strip():
                blocks.append((m.group(1), "function"))
            continue
        m = PARSE_LOOP.match(line)
        if m:
            blocks.append((m.group(1), "loop"))
            continue
        # On the raw line: a quoted delimiter is single-quoted text.
        h = HEREDOC.search(raw[:state["comment"]])
        if h:
            heredoc = (h.group(2), bool(h.group(1)))
        if not blocks and POSITIONAL.search(line):
            found.append(n)
    return found


def python_findings(text: str) -> list[int]:
    found = []
    for node in ast.walk(ast.parse(text)):
        if (isinstance(node, ast.Subscript) and isinstance(node.value, ast.Attribute)
                and node.value.attr == "argv" and isinstance(node.value.value, ast.Name)
                and node.value.value.id == "sys"):
            found.append(node.lineno)
        elif (isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute)
              and node.func.attr == "add_argument" and node.args
              and isinstance(node.args[0], ast.Constant) and isinstance(node.args[0].value, str)
              and not node.args[0].value.startswith("-")
              and not any(k.arg == "choices" for k in node.keywords)):
            found.append(node.lineno)
    return sorted(found)


RUST = re.compile(r"\benv::args(?:_os)?\(\)")
CLAP_DERIVE = re.compile(r"#\[derive\([^)]*\b(?:Parser|Args|Subcommand)\b")
FIELD = re.compile(r"^\s*(?:pub\s+)?[a-z_]\w*\s*:\s*\S")


def rust_findings(text: str) -> list[int]:
    lines = [re.split(r"(?:^|\s)//", line)[0] for line in text.split("\n")]
    found = [n for n, line in enumerate(lines, 1) if RUST.search(line)]
    # Each field of a clap derive: `#[arg(long ...)]` (or short), or a
    # subcommand or flattened group; anything else is a positional.
    n = 0
    while n < len(lines):
        if not CLAP_DERIVE.search(lines[n]):
            n += 1
            continue
        depth, attrs, attr_depth = 0, "", 0
        n += 1
        while n < len(lines):
            line = lines[n]
            if attr_depth or line.lstrip().startswith("#["):
                attrs += line
                attr_depth += line.count("[") - line.count("]")
            elif depth in (1, 2) and FIELD.match(line):
                if not re.search(r"#\[(?:arg|clap)\([^]]*\b(?:long|short)\b|#\[command\((?:subcommand|flatten)",
                                 attrs):
                    found.append(n + 1)
                attrs = ""
            elif line.strip():
                attrs = ""
            depth += line.count("{") - line.count("}")
            n += 1
            if depth <= 0 and "}" in line or depth == 0 and line.rstrip().endswith(";"):
                break
    return sorted(found)


def findings(path: str, text: str) -> list[int]:
    if path.endswith(".sh") or text.startswith(("#!/usr/bin/env bash", "#!/bin/bash", "#!/bin/sh")):
        return shell_findings(text)
    if path.endswith(".py"):
        return python_findings(text)
    if path.endswith(".rs"):
        return rust_findings(text)
    return []


def tracked() -> list[str]:
    out = subprocess.run(["git", "-C", str(ROOT), "ls-files", "*.sh", "*.py", "*.rs"],
                         check=True, capture_output=True, text=True).stdout
    return out.split()


def check(files: dict[str, str], allowed: dict[str, int]) -> list[str]:
    problems = []
    for path, text in sorted(files.items()):
        lines = findings(path, text)
        want = allowed.get(path, 0)
        if len(lines) > want:
            problems.append(f"{path}:{','.join(map(str, lines))}: reads positional arguments; "
                            f"take named ones (--flag VALUE) instead (#277)")
        elif len(lines) < want:
            problems.append(f"{path}: {len(lines)} positional reads, {want} allowed: "
                            f"lower its entry in ALLOWED (or remove it)")
    for path in sorted(set(allowed) - set(files)):
        problems.append(f"{path}: in ALLOWED but not tracked; remove the entry")
    return problems


# Dev examples that parse their own flags by hand (named flags only, and
# an error for anything else), checked by eye: reads of `env::args()` per
# file. A new one is reviewed and added here, or uses clap.
HAND_PARSED = {
    "crates/usnm-core/examples/add_text_cg.rs": 1,
    "crates/usnm-core/examples/term_skew.rs": 1,
    "crates/usnm-ingest/examples/decade_fixture.rs": 1,
    "crates/usnm-ingest/examples/place-report.rs": 1,
    "crates/usnm-ingest/examples/single_field_fixture.rs": 1,
}


def main() -> int:
    files = {p: (ROOT / p).read_text(encoding="utf-8") for p in tracked()}
    problems = check(files, {**ALLOWED, **HAND_PARSED})
    for p in problems:
        print(p, file=sys.stderr)
    if not problems:
        print(f"{len(files)} files: no positional arguments outside the allowed fallbacks")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
