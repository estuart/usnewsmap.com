#!/usr/bin/env python3
"""Golden test of the Rust scorer against gold.py (#457).

Writes seeded synthetic crops and hypotheses and gold.py's `score` outputs for them into --out. The
Rust test (crates/usnm-ocr-survey/tests/golden.rs) scores the same inputs and must give the same files
byte for byte. --check regenerates into a temporary directory and fails if anything differs from --out,
so a change to gold.py's normalization or scoring shows up in CI.

    golden_score.py --out crates/usnm-ocr-survey/tests/golden
    golden_score.py --out crates/usnm-ocr-survey/tests/golden --check
"""

from __future__ import annotations

import argparse
import contextlib
import filecmp
import io
import json
import random
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import gold  # noqa: E402

SEED = 457
# Every character Python's str.isspace accepts, plus characters that are not whitespace but look like it.
SPACES = "".join(c for c in map(chr, range(0x110000)) if c.isspace())
NOT_SPACES = "​᠎﻿"
ALPHABET = ("abcdefghijklmnopqrstuvwxyz ABCDEFGHIJ .,;:'\"-" "äöüßéèçñſﬁﬂ" "æøåþ" "ДЖЯ" "日本" "😀𝔄")


def words(rnd: random.Random, n: int) -> str:
    out = []
    for _ in range(n):
        w = "".join(rnd.choice(ALPHABET.replace(" ", "")) for _ in range(rnd.randint(1, 9)))
        out.append(w)
        out.append(rnd.choice(["  ", " ", " ", " ", "\n", "-\n", "- \r\n ", rnd.choice(SPACES), " "]))
    return "".join(out)


def mutate(rnd: random.Random, s: str, rate: float) -> str:
    chars = list(s)
    out = []
    for c in chars:
        x = rnd.random()
        if x < rate / 3:
            continue
        if x < 2 * rate / 3:
            out.append(rnd.choice(ALPHABET))
        elif x < rate:
            out.append(c)
            out.append(rnd.choice(ALPHABET))
        else:
            out.append(c)
    return "".join(out)


def row(cid: str, group: str, transcription, **kw) -> dict:
    r = {"crop_id": cid, "group": group, "transcription": transcription, "transcriber": "t", "reviewer": "r",
         "hint_copy_reviewed": False, "hints": {"is_truth": False, "texts": {}}}
    r.update(kw)
    return r


def inputs() -> tuple[list[dict], dict[str, list[dict]]]:
    rnd = random.Random(SEED)
    rows: list[dict] = []
    hyps: dict[str, list[dict]] = {"alpha": [], "beta": []}
    # Hand-written edge cases.
    fixed = [
        ("edge-hyphen", "g1", "Clare-\nmont and Clare- \r\n  mont", "Claremont and Claremont"),
        ("edge-guess", "g1", "a[?b]c[?]d [?[?x]] e", "abcd [?x] e"),
        ("edge-nfc", "g1", "café Cäse", "café Cäse"),
        ("edge-space", "g2", "a" + SPACES.replace("\n", "") + "b" + NOT_SPACES + "c", "a b c"),
        ("edge-astral", "g2", "😀 𝔄bc 日本語", "😀 𝔄bd 日本"),
        ("edge-empty-hyp", "g2", "something here", ""),
        ("edge-tie", "g3", "abcdefghijklmnopqrstuvwxyzabcdef", "abcdefghijklmnopqrstuvwxyzabcdeX"),
        ("edge-tie3", "g3", "abcdefghijklmnopqrstuvwxyzabcdef", "abcdefghijklmnopqrstuvwxyzabXYZf"),
        ("edge-long-hyp", "g3", "short", "a much longer hypothesis than the truth"),
        ("edge,comma", "g\"q", "quote \"and\" comma, in ids", "quote and comma in ids"),
        ("edge-emptygroup", "", "the empty group reads all", "the empty group read all"),
    ]
    for cid, group, t, h in fixed:
        rows.append(row(cid, group, t))
        hyps["alpha"].append({"crop_id": cid, "text": h})
    # Rows that fail validation and are left out.
    rows += [
        row("bad-dup", "g1", "first copy"), row("bad-dup", "g1", "second copy"),
        row("bad-empty", "g1", " \n\x1c "),
        row("bad-null", "g1", None),
        row("bad-noreviewer", "g1", "text", reviewer=""),
        row("bad-selfreview", "g1", "text", reviewer="t"),
        row("bad-skip", "g1", "text [...] more"),
        row("bad-unbalanced", "g1", "text [?abc more"),
        row("bad-unknown", "g1", "ab[?]cd"),
        row("bad-hintcopy", "g1", "Copied  text", hints={"texts": {"beta": "Copied text"}}),
        row("ok-hintcopy-reviewed", "g1", "Copied  text", hint_copy_reviewed=True,
            hints={"texts": {"beta": "Copied text"}}),
        row("ok:colon", "g2", "colon in the id"),
    ]
    hyps["alpha"] += [{"crop_id": r["crop_id"], "text": "copy"} for r in rows if r["crop_id"].startswith(("bad-", "ok"))]
    # Seeded random crops, short to long (kept short: gold.py is quadratic).
    for n in range(60):
        size = rnd.choice([5, 20, 60, 120, 250])
        t = words(rnd, size)
        cid = f"rand-{n:03d}"
        rows.append(row(cid, rnd.choice(["g1", "g2", "g3", "g4"]), t,
                        hints={"texts": {"beta": mutate(rnd, t, 0.15)}}))
        hyps["alpha"].append({"crop_id": cid, "text": mutate(rnd, t, rnd.choice([0.0, 0.01, 0.05, 0.3, 0.9]))})
    hyps["alpha"].append({"crop_id": "not-a-crop", "text": "ignored"})
    return rows, hyps


def generate(out: Path) -> None:
    rows, hyps = inputs()
    inp = out / "input"
    gold.write_jsonl(inp / "crops.jsonl", rows)
    for name, rs in hyps.items():
        gold.write_jsonl(inp / f"hyp-{name}.jsonl", rs)
    for name, argv in (("hyp", ["--hyp", f"alpha={inp / 'hyp-alpha.jsonl'}"]),
                       ("hints", ["--hyp", f"alpha={inp / 'hyp-alpha.jsonl'}", "--hints"]),
                       ("unreviewed", ["--hints", "--allow-unreviewed"])):
        exp = out / "expected" / name
        exp.mkdir(parents=True, exist_ok=True)
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf), contextlib.redirect_stderr(io.StringIO()):
            rc = gold.main(["score", "--crops", str(inp / "crops.jsonl"), *argv, "--out", str(exp)])
        if rc:
            raise SystemExit(f"gold.py score failed for {name}")
        (exp / "table.txt").write_text(buf.getvalue(), encoding="utf-8")
        (exp / "args.json").write_text(json.dumps([a.replace(str(inp), "input") for a in argv]) + "\n",
                                       encoding="utf-8")


def same(a: Path, b: Path) -> list[str]:
    cmp = filecmp.dircmp(a, b)
    diffs = [f"only in one: {x}" for x in cmp.left_only + cmp.right_only]
    diffs += [x for x in cmp.common_files if not filecmp.cmp(a / x, b / x, shallow=False)]
    for d in cmp.common_dirs:
        diffs += [f"{d}/{x}" for x in same(a / d, b / d)]
    return diffs


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--check", action="store_true", help="regenerate elsewhere and compare with --out")
    a = ap.parse_args(argv)
    if not a.check:
        generate(a.out)
        return 0
    with tempfile.TemporaryDirectory() as tmp:
        generate(Path(tmp))
        diffs = same(Path(tmp), a.out)
    for d in diffs:
        print(f"differs: {d}")
    if diffs:
        print("the golden files are stale: run scripts/ocr-survey/golden_score.py --out", a.out)
    return 1 if diffs else 0


if __name__ == "__main__":
    sys.exit(main())
