#!/usr/bin/env python3
"""Export the function words a page's language is found by (#289, 05
§5.5.6): crates/usnm-core/data/page-language-words.txt.

    scripts/build-page-language-words.py [--out path] [--check]

Needs wordfreq 3.1.1 (`pip install wordfreq==3.1.1`, the version the OCR
audit pins in ja-ocr/requirements.txt).

The lists come from the audit's own code, so they can't drift from it:
`damage_v2.Models` (what `quality.py --metric v2` uses) builds each word
list's model, and its function words are written out. For each list (the
wordfreq codes in `damage_v2.WORDFREQ`, where Croatian and Serbian share
`sh`, and our own Hawaiian and Yiddish lists), that is its 100 most
frequent words of 2 letters or more, most frequent first, then for Danish
and Norwegian their `aa` spellings (`paa` for `på`). One line per list:
the code, the script of its words, then the words, separated by spaces.

`usnm_core::page_language` embeds the file. Nothing in the pipeline runs
this script. `--check` compares with the committed file instead of writing
it (exit 1 when it differs or is missing); the Japanese OCR image's tests
run it (ja-ocr/test_page_language_parity.py).
"""

import argparse
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "crates" / "usnm-core" / "data" / "page-language-words.txt"
JA_OCR = ROOT / "ja-ocr"
WORDFREQ_VERSION = "3.1.1"

HEADER = """\
# The function words usnm_core::page_language finds a page's language by
# (#289, 05 §5.5.6): for each word list, its 100 most frequent words of 2
# letters or more, most frequent first, then for Danish and Norwegian their
# "aa" spellings. One line per list: the list's code, the script of its
# words, then the words. Built by scripts/build-page-language-words.py from
# ja-ocr/damage_v2.py's models; don't edit by hand.
#
# Licence: this file is data, not code, and is shared under the Creative
# Commons Attribution-ShareAlike 4.0 licence
# (https://creativecommons.org/licenses/by-sa/4.0/). It is derived from:
# - wordfreq 3.1.1 by Robyn Speer (https://github.com/rspeer/wordfreq), whose
#   data is CC BY-SA 4.0 and draws on Google Books Ngrams (acknowledged here,
#   as the Google Books Ngram Viewer asks), the Leeds Internet Corpus,
#   Wikipedia, ParaCrawl, OPUS OpenSubtitles 2018, the SUBTLEX lists and
#   other web and news text (crates/usnm-core/data/README.md lists them);
# - the Yiddish list (yi) of ja-ocr/wordlists/, derived from Yiddish
#   Wikipedia and Wikisource (CC BY-SA 4.0, Wikimedia contributors);
# - the Hawaiian list (haw) of ja-ocr/wordlists/, from the Hawaiian Corpus
#   Project's frequency list (CC0).
"""


def function_words(damage_v2, models, code: str) -> tuple[str, list[str]]:
    """A list's script and its function words in rank order, checked against the model's set."""
    lang = next(m for m, c in {**damage_v2.WORDFREQ, **damage_v2.LOCAL_WORDLISTS}.items() if c == code)
    model = models.get(lang)
    if model is None:
        raise SystemExit(f"{code}: no model")
    # build_model's own steps, kept in order (its `top` is a set).
    words = [w for w in models.loader(code) if len(w) >= 2 and w.isalpha()]
    top = words[:damage_v2.TOP_FUNCTION]
    if code in damage_v2.AA_SPELLING:
        top += [w.replace("å", "aa") for w in top if "å" in w]
    top = list(dict.fromkeys(top))
    if frozenset(top) != model.top:
        raise SystemExit(f"{code}: the words differ from damage_v2's model")
    if any(not w or any(c.isspace() for c in w) for w in top):
        raise SystemExit(f"{code}: a word is empty or holds whitespace")
    return model.script, top


def render(damage_v2) -> str:
    models = damage_v2.Models()
    codes = sorted(set(damage_v2.WORDFREQ.values()) | set(damage_v2.LOCAL_WORDLISTS.values()))
    lines = []
    for code in codes:
        script, words = function_words(damage_v2, models, code)
        lines.append(" ".join([code, script, *words]))
    return HEADER + "".join(line + "\n" for line in lines)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--out", type=Path, default=OUT, help="the file to write")
    ap.add_argument("--check", action="store_true", help="compare with --out instead of writing it")
    a = ap.parse_args()

    from importlib.metadata import version

    have = version("wordfreq")
    if have != WORDFREQ_VERSION:
        raise SystemExit(f"wordfreq {WORDFREQ_VERSION} is needed, not {have}")
    sys.path.insert(0, str(JA_OCR))
    import damage_v2

    text = render(damage_v2)
    if a.check:
        same = a.out.exists() and a.out.read_text(encoding="utf-8") == text
        print(f"{a.out}: {'up to date' if same else 'differs'}")
        return 0 if same else 1
    a.out.parent.mkdir(parents=True, exist_ok=True)
    a.out.write_text(text, encoding="utf-8")
    print(f"{a.out}: {text.count(chr(10)) - HEADER.count(chr(10))} lists")
    return 0


if __name__ == "__main__":
    sys.exit(main())
