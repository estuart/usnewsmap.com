#!/usr/bin/env python3
"""A sample of real pages for the page-language detector's parity test (#289, 05 §5.5.8):
crates/usnm-core/tests/data/page-language-parity.jsonl.zst.

    scripts/page-language-sample.py --out PATH [--languages french,dutch,...] [--per-language 2]
        [--before 1929] [--chars 8000] [--whole 2]

Needs wordfreq 3.1.1 and zstd (Python 3.14, or pyarrow), as ja-ocr/damage_parity.py does.

For each language (LoC's names, as Chronicling America's `language` facet has them; by default
every catalog language with a word list), it asks loc.gov's collection search for issues
before --before, takes up to --per-language of them, and reads the first page of each (and the
second, for a title in several languages), with jaocr.py's pacing. A page's title languages are
the item's, as MARC codes (crates/usnm-core/src/names.rs). Each page is cut to its first --chars
characters at a line break, except the --whole longest, kept whole so the window and
second-script rules run. Each is then scored by ja-ocr/damage_v2.py as damage_parity.py scores
the pages of the damage fixture, in the same format, so the page-language parity test
(crates/usnm-core/tests/page_language_parity.rs) reads both files. It logs each page's doc id and
result, never text.
"""

import argparse
import json
import re
import sys
import urllib.parse
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "ja-ocr"))

SEARCH = "https://www.loc.gov/collections/chronicling-america/"
ISSUE = re.compile(r"/item/([a-z0-9]+)/(\d{4}-\d{2}-\d{2})/(ed-\d+)/?$")


def loc_names() -> dict[str, str]:
    """LoC's language names to MARC codes, from usnm_core::names::LANGUAGES."""
    text = (ROOT / "crates" / "usnm-core" / "src" / "names.rs").read_text(encoding="utf-8")
    table = text[text.index("pub static LANGUAGES"):]
    table = table[:table.index("];")]
    return dict(re.findall(r'\("([^"]+)", "([a-z]{3})"\)', table))


def with_lists(names: dict[str, str], damage_v2) -> list[str]:
    """The LoC names of the catalog languages with a word list."""
    lists = {**damage_v2.WORDFREQ, **damage_v2.LOCAL_WORDLISTS}
    return sorted(n for n, code in names.items() if code in lists)


def issues(language: str, before: int, want: int, api, jaocr) -> list[tuple[str, list[str]]]:
    """Up to `want` issues in `language` before `before`: (lccn_date_ed, LoC language names)."""
    q = urllib.parse.urlencode({"fa": f"language:{language}", "dates": f"1700/{before - 1}",
                                "fo": "json", "c": 50, "at": "results"})
    out, titles = [], set()
    for r in json.loads(jaocr.fetch(f"{SEARCH}?{q}", api)).get("results", []):
        m = ISSUE.search(r.get("id", ""))
        # One issue a title, so the sample spreads over titles.
        if m and m.group(1) not in titles:
            titles.add(m.group(1))
            out.append(("_".join(m.groups()), [str(x) for x in r.get("language") or []]))
        if len(out) == want:
            break
    return out


def cut(text: str, chars: int) -> str:
    """The first `chars` characters, at the last line break before them."""
    if len(text) <= chars:
        return text
    head = text[:chars]
    return head[:head.rfind("\n") + 1] if "\n" in head else head


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--out", required=True, help="the .jsonl.zst file to write")
    ap.add_argument("--languages", help="LoC language names, comma-separated (default: every one with a word list)")
    ap.add_argument("--per-language", type=int, default=2, help="issues per language")
    ap.add_argument("--before", type=int, default=1929, help="only issues before this year")
    ap.add_argument("--chars", type=int, default=8000, help="cut each page to this many characters")
    ap.add_argument("--whole", type=int, default=2, help="keep this many of the longest pages whole")
    a = ap.parse_args()

    import damage_parity
    import damage_v2
    import jaocr

    names = loc_names()
    languages = a.languages.split(",") if a.languages else with_lists(names, damage_v2)
    api, tile, items = jaocr.Pacer(damage_parity.API_GAP), jaocr.Pacer(damage_parity.TILE_GAP), {}
    pages: list[tuple[str, str, tuple]] = []
    seen = set()
    for language in languages:
        for issue, loc_langs in issues(language, a.before, a.per_language, api, jaocr):
            langs = tuple(dict.fromkeys(names[n] for n in loc_langs if n in names))
            for seq in ([1, 2] if len(langs) > 1 else [1]):
                doc_id = f"{issue}_seq-{seq}"
                if doc_id in seen:
                    continue
                seen.add(doc_id)
                try:
                    text = damage_parity.loc_text(doc_id, api, tile, items)
                except Exception as e:  # noqa: BLE001 - a page without text is left out
                    print(f"{doc_id}: skipped ({type(e).__name__})", file=sys.stderr)
                    continue
                pages.append((doc_id, text, langs))
    whole = {d for d, _, _ in sorted(pages, key=lambda p: -len(p[1]))[:a.whole]}
    models = damage_v2.Models()
    records = []
    for doc_id, text, langs in pages:
        r = damage_parity.record(doc_id, text if doc_id in whole else cut(text, a.chars), langs, models)
        records.append(r)
        s = r["score"]
        print(f"{doc_id} {list(langs)}: {s['lang']} {s['decision']} {s['mixed_with'] or ''}", file=sys.stderr)
    with open(a.out, "wb") as f:
        f.write(damage_parity.encode("loc.gov", records))
    print(f"{a.out}: {len(records)} pages", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
