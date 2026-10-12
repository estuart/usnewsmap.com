#!/usr/bin/env python3
"""Check resource names against ADR-0017 (docs/design/adr/0017-resource-naming.md,
"Enforcement"), offline:

    scripts/ci/check-names.py [--envs prod,dev,xabcd] [--bicep bicep] [--base origin/main] [--require-base]

1. Evaluates names() from infra/naming.bicep with `bicep build-params
   infra/names/check.bicepparam` (no Azure): prod pinned to its legacy
   names, and each of --envs in ADR-0017's form with a sample suffix.
2. Every value in ADR-0017's form must parse: `usnm`, a function word from
   the registry in naming.bicep with up to three qualifiers, the type's
   abbreviation, the environment last (only the suffix after it, only on a
   global type), within the type's length limit; compact names
   usnm{fn}{abbr}{env}{sfx}; workbook titles `usnm {Title} ({env})`. A value
   that doesn't is allowed only if its key is in infra/names/exceptions.json
   (for the conventional forms, only with "scope": "all"); an exception
   whose value now conforms must be removed.
3. The pinned prod names must equal infra/names/prod.json: a prod rename
   shows up as a reviewed change to that file, never as a side effect.
4. exceptions.json: keys sorted, each with a reason, the ADR section and an
   `until` (phase-0 to phase-4, never, or an issue #N); and with --base, no
   key that the base revision's file doesn't have, and no scope widened
   from prod to all (the list only shrinks).
   A --base that doesn't resolve or can't be read is a problem; CI also
   passes --require-base, so the check can't run without one.
5. infra/main.bicep and infra/modules/*.bicep build no resource name from a
   string containing usnm or usnewsmap (name, roleName, displayName,
   groupShortName, SyntheticMonitorId), and every guid() seed containing
   usnm is one of the frozen seeds below. Each file is read whole, with its
   comments blanked, so a value or a guid() call split over lines counts.
   An identifier in a name (directly or in a ${...} interpolation) that is
   a var, a parameter's default or a func defined in the same file is
   followed to its definition, and that is checked the same way.
6. The status sanitizer's abbreviation lists (TYPE_ABBREVIATIONS and
   COMPACT_ABBREVIATIONS in crates/usnm-api/src/status/sanitize.rs) equal
   naming.bicep's: every type abbreviation but the `display: true` ones,
   and the `form: 'compact'` ones.

Standard library only. Exits 1 listing every problem.
"""

import argparse
import json
import os
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent
CHECK_PARAMS = ROOT / "infra" / "names" / "check.bicepparam"
PROD = ROOT / "infra" / "names" / "prod.json"
EXCEPTIONS = ROOT / "infra" / "names" / "exceptions.json"
SANITIZER = ROOT / "crates" / "usnm-api" / "src" / "status" / "sanitize.rs"

ENV_RE = re.compile(r"^(prod|dev|x[a-z0-9]{1,4})$")
UNTIL_RE = re.compile(r"^(phase-[0-4]|never|#[0-9]+)$")
TITLE_RE = re.compile(r"^usnm [A-Za-z ]+ \((prod|dev|x[a-z0-9]{1,4})\)$")
QUALIFIER_RE = re.compile(r"^[a-z0-9]{1,12}$")
WORD_RE = re.compile(r"^[a-z][a-z0-9]{1,8}$")

# guid() seeds that contain usnm. They are frozen: changing one rotates a
# role definition's id or a workbook's name, which the stack then deletes
# and recreates (ADR-0017, "Enforcement").
FROZEN_SEEDS = {
    "usnm-archive-writer",
    "usnm-ocr-node-writer",
    "usnm-search-log-writer",
    "usnm-sset-writer",
    "usnm-swa-deployer",
    "usnm-workbook-api",
    "usnm-workbook-ocr-nodes",
    "usnm-workbook-pipeline",
}

# The properties that hold a resource's name or display name.
NAME_PROPS = ("name", "roleName", "displayName", "groupShortName", "SyntheticMonitorId")
NAME_PROP = re.compile(r"(?:^[ \t]*|[{,]\s*)(" + "|".join(NAME_PROPS) + r")\s*:\s*", re.M)
LITERAL = re.compile(r"'((?:[^'\\]|\\.)*)'")


def string_end(text: str, i: int) -> int:
    """The index just past the string literal that starts at text[i] ('...'
    with backslash escapes, or a multi-line '''...''')."""
    if text.startswith("'''", i):
        j = text.find("'''", i + 3)
        return len(text) if j < 0 else j + 3
    j = i + 1
    while j < len(text):
        if text[j] == "\\":
            j += 2
            continue
        if text[j] == "'":
            return j + 1
        j += 1
    return len(text)


def mask_comments(text: str) -> str:
    """`text` with its // and /* */ comments blanked: the same length and the
    same line breaks, so offsets and line numbers still match the file.
    String literals are kept, comment markers inside them included."""
    out, i = [], 0
    while i < len(text):
        if text[i] == "'":
            j = string_end(text, i)
            out.append(text[i:j])
            i = j
        elif text.startswith("//", i):
            j = text.find("\n", i)
            j = len(text) if j < 0 else j
            out.append(" " * (j - i))
            i = j
        elif text.startswith("/*", i):
            j = text.find("*/", i + 2)
            j = len(text) if j < 0 else j + 2
            out.append(re.sub(r"[^\n]", " ", text[i:j]))
            i = j
        else:
            out.append(text[i])
            i += 1
    return "".join(out)


def guid_calls(text: str) -> list[tuple[int, int]]:
    """Spans of the guid(...) calls in `text`, with balanced parentheses
    (ignoring those inside string literals), across lines."""
    spans = []
    for m in re.finditer(r"\bguid\s*\(", text):
        depth, i = 1, m.end()
        while i < len(text) and depth:
            if text[i] == "'":
                i = string_end(text, i)
                continue
            depth += {"(": 1, ")": -1}.get(text[i], 0)
            i += 1
        spans.append((m.start(), i))
    return spans


def line_of(text: str, offset: int) -> int:
    return text.count("\n", 0, offset) + 1


def evaluate(bicep: str, envs: str) -> dict:
    """The parameters check.bicepparam evaluates to."""
    env = {**os.environ, "USNM_CHECK_ENVS": envs}
    out = subprocess.run([bicep, "build-params", str(CHECK_PARAMS), "--stdout"],
                         capture_output=True, text=True, env=env, cwd=ROOT)
    if out.returncode != 0:
        sys.exit(f"bicep build-params {CHECK_PARAMS.relative_to(ROOT)} failed:\n{out.stderr}")
    params = json.loads(json.loads(out.stdout)["parametersJson"])["parameters"]
    return {k: v["value"] for k, v in params.items()}


def values(v) -> list:
    return v if isinstance(v, list) else [v]


def conforms(value: str, abbr: str, env: str, sfx: str, words: list, abbrs: dict) -> str | None:
    """None when `value` is in ADR-0017's form for `abbr`, else why not."""
    spec = abbrs.get(abbr)
    if spec is None:
        return f"unknown type abbreviation {abbr!r}"
    form, limit = spec["form"], spec["maxLength"]
    if form == "empty":
        return None if value == "" else "must be empty in ADR-0017's form"
    if form == "title":
        if not TITLE_RE.match(value) or not value.endswith(f"({env})"):
            return f"not a title `usnm {{Title}} ({env})`"
        return None if len(value) <= limit else f"{len(value)} characters, over the {limit} {abbr} allows"
    if form == "shortName":
        if value != f"usnm-{env}":
            return f"not `usnm-{env}`"
        return None if len(value) <= limit else f"longer than {limit}"
    if len(value) > limit:
        return f"{len(value)} characters, over the {limit} {abbr} allows"
    if form == "compact":
        head, tail = "usnm", f"{abbr}{env}{sfx}"
        if not (value.startswith(head) and value.endswith(tail)):
            return f"not usnm{{fn}}{abbr}{env}{sfx}"
        fn = value[len(head):-len(tail)]
        if fn not in words:
            return f"function word {fn!r} isn't in the registry"
        if len(fn) > 7:
            return f"function word {fn!r} is over 7 characters, too long for a compact name"
        return None
    tokens = value.split("-")
    if tokens[0] != "usnm":
        return "doesn't start with usnm-"
    if form == "global":
        if tokens[-1] != sfx:
            return f"doesn't end with -{env}-{sfx}"
        tokens = tokens[:-1]
    if tokens[-1] != env:
        return f"doesn't end with -{env}" + (f"-{sfx}" if form == "global" else "")
    if len(tokens) < 4 or tokens[-2] != abbr:
        return f"the token before the environment isn't {abbr!r}"
    desc = tokens[1:-2]
    if desc[0] not in words or not WORD_RE.match(desc[0]):
        return f"function word {desc[0]!r} isn't in the registry"
    if len(desc) > 4:
        return "more than three qualifiers"
    bad = [q for q in desc[1:] if not QUALIFIER_RE.match(q)]
    if bad:
        return f"qualifier {bad[0]!r} isn't 1-12 lowercase letters or digits"
    return None


def check_names(names: dict, env: str, sfx: str, pinned: bool, tables: dict, exceptions: dict) -> list[str]:
    """Problems with one evaluated names() map."""
    out = []
    types, words, abbrs = tables["nameTypes"], tables["functionWords"], tables["typeAbbreviations"]
    create_only = set(tables["createOnlyKeys"])
    label = f"{env} ({'pinned' if pinned else 'ADR-0017 form'})"
    if set(names) != set(types):
        out.append(f"{label}: names() keys and nameTypes differ: "
                   f"{sorted(set(names) ^ set(types))}")
    for key in sorted(names):
        if key not in types:
            continue
        exc = exceptions.get(key)
        excused = exc is not None and (pinned or exc.get("scope") == "all")
        why = None
        for v in values(names[key]):
            if pinned and key in create_only and v == "":
                continue
            why = why or conforms(v, types[key], env, sfx, words, abbrs)
        if why and not excused:
            out.append(f"{label}: {key} = {names[key]!r}: {why}; follow ADR-0017 or list it in "
                       f"{EXCEPTIONS.relative_to(ROOT)}")
        if not why and excused and pinned:
            out.append(f"{label}: {key} = {names[key]!r} follows ADR-0017: remove its exception")
    return out


def check_exceptions(exceptions: dict, base: dict | None) -> list[str]:
    out = []
    keys = list(exceptions)
    if keys != sorted(keys):
        out.append(f"{EXCEPTIONS.relative_to(ROOT)}: keys aren't sorted")
    for key, e in exceptions.items():
        for field in ("reason", "adr", "until"):
            if not isinstance(e.get(field), str) or not e[field]:
                out.append(f"exception {key}: no {field}")
        if isinstance(e.get("until"), str) and not UNTIL_RE.match(e["until"]):
            out.append(f"exception {key}: until {e['until']!r} isn't phase-0..phase-4, never or #N")
        if e.get("scope", "prod") not in ("prod", "all"):
            out.append(f"exception {key}: scope must be prod (the default) or all")
    if base is not None:
        for key in sorted(set(exceptions) - set(base)):
            out.append(f"exception {key} is new: the list only shrinks (ADR-0017, \"Enforcement\")")
        # A wider scope excuses more names: as new as a new key.
        for key in sorted(set(exceptions) & set(base)):
            was = base[key].get("scope", "prod") if isinstance(base[key], dict) else "prod"
            now = exceptions[key].get("scope", "prod") if isinstance(exceptions[key], dict) else "prod"
            if was == "prod" and now != "prod":
                out.append(f"exception {key} widened from scope prod to {now}: as good as a new "
                           "exception, and the list only shrinks (ADR-0017, \"Enforcement\")")
    return out


def check_prod(pinned: dict, expected: dict) -> list[str]:
    expected = {k: v for k, v in expected.items() if not k.startswith("$")}
    out = []
    for key in sorted(set(pinned) | set(expected)):
        if pinned.get(key) != expected.get(key):
            out.append(f"prod {key}: names() gives {pinned.get(key)!r}, "
                       f"{PROD.relative_to(ROOT)} has {expected.get(key)!r}: a prod rename must "
                       "change prod.json in the same PR")
    return out


# A line ending in one of these, or a next line starting with ? or :, goes on
# with the same expression (a ternary or an operator split across lines).
CONTINUES_AFTER = "?:([{,+-*/%&|=!<>"
CONTINUES_WITH = "?:"


def value_at(code: str, i: int) -> str:
    """The expression starting at code[i], across lines: up to a top-level
    , or a closing bracket, or the end of its last line. A value that starts
    on the next line (`name:` then a newline) is read from there, and a
    ternary or an operator split over lines is read to its end."""
    depth, j, seen = 0, i, False
    while j < len(code):
        c = code[j]
        if c == "'":
            j = string_end(code, j)
            seen = True
            continue
        if c in "([{":
            depth += 1
        elif c in ")]}":
            if depth == 0:
                break
            depth -= 1
        elif c == "," and depth == 0:
            break
        elif c == "\n" and depth == 0 and seen:
            before = code[i:j].rstrip()
            after = code[j:].lstrip()
            if not (before and before[-1] in CONTINUES_AFTER) and not after.startswith(tuple(CONTINUES_WITH)):
                break
        if not c.isspace():
            seen = True
        j += 1
    return code[i:j]


# Definitions a name can come from in the same file: a var, a parameter's
# default and a func's body (Bicep declares each at the top level).
DEFINITION = re.compile(
    r"^[ \t]*(?:var[ \t]+(\w+)[ \t]*=|param[ \t]+(\w+)[ \t]+[^=\n]*?=|"
    r"func[ \t]+(\w+)[ \t]*\([^)]*\)[^=\n]*=>)[ \t]*", re.M)
IDENT = re.compile(r"[A-Za-z_]\w*")


def definitions(code: str) -> dict[str, tuple[int, str]]:
    """Each var, parameter default and func in `code`: its offset and its
    expression."""
    out = {}
    for m in DEFINITION.finditer(code):
        name = m.group(1) or m.group(2) or m.group(3)
        out.setdefault(name, (m.start(), value_at(code, m.end())))
    return out


def interpolations(body: str) -> list[str]:
    """The ${...} expressions in a string literal's body, with balanced
    braces (strings inside them included)."""
    out, i = [], 0
    while True:
        i = body.find("${", i)
        if i < 0:
            return out
        depth, j = 1, i + 2
        while j < len(body) and depth:
            if body[j] == "'":
                j = string_end(body, j)
                continue
            depth += {"{": 1, "}": -1}.get(body[j], 0)
            j += 1
        out.append(body[i + 2:j - 1])
        i = j


def references(expr: str) -> list[str]:
    """The identifiers `expr` refers to, outside string literals and inside
    their ${...} interpolations; not a property after `.`, nor an object key."""
    out, i = [], 0
    while i < len(expr):
        if expr[i] == "'":
            j = string_end(expr, i)
            for inner in interpolations(expr[i + 1:j - 1]):
                out += references(inner)
            i = j
            continue
        m = IDENT.match(expr, i)
        if m:
            prev = expr[:i].rstrip()
            key = re.match(r"\s*:", expr[m.end():]) and not prev.endswith("?")
            if not (expr[i - 1:i] == "." or key):
                out.append(m.group())
            i = m.end()
            continue
        i += 1
    return out


def origins(expr: str, defs: dict, seen: set | None = None) -> list[tuple[str, tuple[str, ...]]]:
    """The string literals `expr` is built from, each with the chain of
    definitions it came through: its own (an empty chain), and those of each
    var, parameter default or func it refers to in the same file, followed
    through further definitions."""
    seen = set() if seen is None else seen
    out = [(lit, ()) for lit in LITERAL.findall(expr)]
    for ident in references(expr):
        if ident in seen or ident not in defs:
            continue
        seen.add(ident)
        out += [(lit, (ident, *via)) for lit, via in origins(defs[ident][1], defs, seen)]
    return out


def resolved(expr: str, defs: dict) -> str:
    """`expr`, through any chain of bare identifiers defined in the file."""
    expr, seen = expr.strip(), set()
    while re.fullmatch(r"[A-Za-z_]\w*", expr) and expr in defs and expr not in seen:
        seen.add(expr)
        expr = defs[expr][1].strip()
    return expr


def check_sources(paths: list[Path]) -> list[str]:
    """Resource names built from a usnm string, and unknown guid() seeds. Each
    file is read whole, so a property or a guid() call split over lines is
    checked like one on a single line; line numbers are the file's. A name
    that refers to a var, a parameter default or a func of the same file is
    checked through it, at any depth (names() is defined in naming.bicep, so
    a name taken from it is never followed into a usnm literal)."""
    out = []
    for path in paths:
        rel = path.relative_to(ROOT)
        code = mask_comments(path.read_text(encoding="utf-8"))
        calls = guid_calls(code)
        for start, end in calls:
            for m in LITERAL.finditer(code, start, end):
                seed = m.group(1)
                if ("usnm" in seed or "usnewsmap" in seed) and seed not in FROZEN_SEEDS:
                    out.append(f"{rel}:{line_of(code, m.start())}: guid() seed {seed!r} isn't a known "
                               "frozen seed")
        # A guid() seed is not a name: each call becomes a placeholder value
        # of the same length and line breaks, so offsets still match.
        for start, end in calls:
            code = code[:start] + "_" + re.sub(r"[^\n]", " ", code[start + 1:end]) + code[end:]
        defs = definitions(code)
        for m in NAME_PROP.finditer(code):
            value = value_at(code, m.end())
            # The Cosmos database is a child, named in code: exactly 'usnm'.
            if m.group(1) == "name" and resolved(value, defs) == "'usnm'":
                continue
            for lit, via in origins(value, defs):
                if "usnm" in lit or "usnewsmap" in lit:
                    steps = [f"{d} (line {line_of(code, defs[d][0])})" for d in via]
                    through = f" (through {' then '.join(steps)})" if via else ""
                    out.append(f"{rel}:{line_of(code, m.start(1))}: {m.group(1)} built from {lit!r}"
                               f"{through}: take it from names() in infra/naming.bicep (ADR-0017)")
    return out


def rust_list(text: str, name: str) -> list[str] | None:
    """The items of `const {name}: [&str; N] = [...];` in Rust source,
    leaving out any in a // comment."""
    m = re.search(r"const " + name + r": \[&str; \d+\] = \[(.*?)\];", text, re.S)
    return None if m is None else re.findall(r'"([^"\\]*)"', re.sub(r"//[^\n]*", "", m.group(1)))


def check_sanitizer(abbrs: dict, text: str) -> list[str]:
    """The status sanitizer's abbreviation lists against naming.bicep's
    table (ADR-0017, "Enforcement")."""
    rel = SANITIZER.relative_to(ROOT)
    expected = {
        "TYPE_ABBREVIATIONS": {a for a, spec in abbrs.items() if not spec.get("display")},
        "COMPACT_ABBREVIATIONS": {a for a, spec in abbrs.items() if spec["form"] == "compact"},
    }
    out = []
    for name, want in expected.items():
        got = rust_list(text, name)
        if got is None:
            out.append(f"{rel}: no `const {name}: [&str; N] = [...]` to compare with infra/naming.bicep")
            continue
        if len(got) != len(set(got)):
            out.append(f"{rel}: {name} lists an abbreviation twice")
        if set(got) != want:
            out.append(f"{rel}: {name} differs from infra/naming.bicep's typeAbbreviations: missing "
                       f"{sorted(want - set(got))}, extra {sorted(set(got) - want)}")
    return out


def git(*args: str) -> subprocess.CompletedProcess:
    return subprocess.run(["git", *args], cwd=ROOT, capture_output=True, text=True)


def base_exceptions(ref: str, required: bool) -> tuple[dict | None, list[str]]:
    """The base revision's exceptions.json, for the shrink check, and any
    problem reading it. Without --base there's nothing to compare (a local
    run), unless --require-base (CI), where that is a problem. A revision
    that doesn't resolve, or whose file doesn't parse, is always a problem.
    A base without the file is accepted only while it has no naming.bicep
    either (the change that adds both)."""
    if not ref:
        return None, (["--require-base: no --base revision given, so the exceptions can't be "
                       "checked to only shrink"] if required else [])
    if git("rev-parse", "--verify", "--quiet", f"{ref}^{{commit}}").returncode != 0:
        return None, [f"--base {ref} isn't a revision here (was it fetched?): the exceptions can't "
                      "be checked to only shrink"]
    rel = EXCEPTIONS.relative_to(ROOT)
    shown = git("show", f"{ref}:{rel}")
    if shown.returncode != 0:
        if git("cat-file", "-e", f"{ref}:infra/naming.bicep").returncode == 0:
            return None, [f"--base {ref} has infra/naming.bicep but no readable {rel}: "
                          f"{shown.stderr.strip()}"]
        print(f"note: {ref} predates {rel} (no infra/naming.bicep either); nothing to shrink from")
        return None, []
    try:
        return json.loads(shown.stdout), []
    except json.JSONDecodeError as e:
        return None, [f"--base {ref}: {rel} doesn't parse: {e}"]


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--envs", default="prod,dev,xabcd",
                    help="environments to evaluate in ADR-0017's form, comma separated")
    ap.add_argument("--bicep", default="bicep", help="the Bicep CLI (CI's pinned version)")
    ap.add_argument("--base", default="",
                    help="a git revision whose exceptions.json the current one may only shrink from")
    ap.add_argument("--require-base", action="store_true",
                    help="fail unless --base names a revision whose exceptions.json can be read (CI)")
    a = ap.parse_args()
    envs = [e.strip() for e in a.envs.split(",") if e.strip()]
    bad = [e for e in envs if not ENV_RE.match(e)]
    if bad:
        ap.error(f"not environment names: {bad}")

    p = evaluate(a.bicep, ",".join(envs))
    exceptions = json.loads(EXCEPTIONS.read_text(encoding="utf-8"))
    base, problems = base_exceptions(a.base, a.require_base)
    problems += check_exceptions(exceptions, base)
    problems += check_names(p["prodPinned"], "prod", p["legacyProdSuffix"], True, p, exceptions)
    problems += check_prod(p["prodPinned"], json.loads(PROD.read_text(encoding="utf-8")))
    for env in envs:
        problems += check_names(p["conventional"][env], env, p["sampleSuffix"], False, p, exceptions)
    sources = [ROOT / "infra" / "main.bicep"] + sorted((ROOT / "infra" / "modules").glob("*.bicep"))
    problems += check_sources(sources)
    problems += check_sanitizer(p["typeAbbreviations"], SANITIZER.read_text(encoding="utf-8"))

    for line in problems:
        print(line)
    if problems:
        print(f"{len(problems)} problem(s)", file=sys.stderr)
        return 1
    n = len(p["prodPinned"])
    print(f"ok: {n} names for prod (pinned) and in ADR-0017's form for {', '.join(envs)}; "
          f"{len(exceptions)} exceptions; {len(sources)} templates; sanitizer lists match"
          + (f"; exceptions only shrink against {a.base}" if base is not None else ""))
    return 0


if __name__ == "__main__":
    sys.exit(main())
