"""How good LoC's OCR text is, by language and decade, from a sample of pages.

    python3 jaocr.py quality [--sample-pct 2] [--min-pages 50] [--metric v2]

It reads every curated Parquet part of every batch in the published version
once, keeps a fixed sample of pages (a page is in it when a hash of its
doc_id, mod 10,000, is below sample_pct x 100, so a rerun takes the same
pages), and scores each sampled page's stored text. Word tokens are a
token's letters once surrounding punctuation is stripped, at least 2 of
them, case-folded.

--metric v2 (the default) first finds the page's own language, then scores
the page against it:

- language: among the title's catalog languages, plus English, the one
  whose 100 most frequent words (wordfreq's, 2 letters or more: the function
  words) are the largest share of the page's word tokens. It must reach
  MIN_SHARE with PAGE_TYPES different function words, and have LEAD times
  the runner-up's tokens among the function words only one of the two has;
  otherwise the page is `und` (too short, garbled, or in a language with no
  word list: Dakota, Choctaw, Navajo, Latin, Welsh...). wordfreq supplies
  the lists; Hawaiian's and Yiddish's are our own (wordlists/). Serbian in
  Cyrillic is scored in Latin letters. A page whose words
  are mostly in another script (Hebrew, Cyrillic, Greek, ...) takes the
  title's language in that script (`yid` or `heb` for Hebrew script; the
  best-fitting one if it lists several with word lists), or `und` if it
  lists none. A page of MIXED_MIN_WORDS or more is `mixed` when, cut into
  windows of WINDOW words (a shorter last window only if it has MIN_TAIL),
  two languages each win at least MIXED_SHARE of the windows one wins (and
  2 or more); its windows are then scored against their own languages, and
  windows no language wins aren't scored. A title's language in a second
  script that covers MIXED_SHARE of the words (with WINDOW_TYPES different
  function words) makes a page mixed too.
- function_share: the share of its word tokens in the language's top 100.
- damage_rate: near-misses / (near-misses + tokens that are one of the
  language's 20 most frequent words). A near-miss is one OCR edit from one
  of those 20 (a letter substituted or deleted, a thin letter inserted, or
  one letter read as two thin ones: "tbe", "tlie", "aud", "bie", "ift") and
  not a real word: not in the top 5,000 of the language, or of the title's
  other languages and English (so "she", "an", "or" don't count).
- garbage_share: the share of its whitespace tokens that rmgarbage-style
  rules (Taghva et al.) flag: longer than 40 characters; more than half not
  letters or digits; the same letter 3 times in a row; Latin letters only,
  4 or more, with no vowel or nothing but vowels; mixed case inside the word
  ("tHe", not "McKay").

--metric v1 is the first version: dict_share, the share of word tokens in
wordfreq's top 200,000 words of the title's first catalog language, and
garbage_share. It is lenient: wordfreq's big lists hold common OCR errors
("tbe", "aud", "ift", "bie"), so damaged text still scores high.

Pages of titles that list Japanese are counted on their own and not scored:
our own OCR covers those (04 section 4.8).

It writes audit/ocr-quality-v2-<version>-<pct>pct.json (every table; v1:
audit/ocr-quality-<version>-<pct>pct.json) and one CSV per table next to it
in the curated store (v2 also every sampled page, -pages.csv.gz), logs each table row as an "ocr quality" line, then a
summary, and (v2) keeps status/ocr-quality.json in the reference store up
to date for the status page.

The job's replicas share the work: the parts are cut into chunks of whole
batches, each replica claims chunks and writes their sampled pages under
audit/<name>.run/<execution>/, and when all are done one replica tallies
them and writes the outputs. A rerun (a new execution) overwrites them.
Scoring caches each distinct token's word and garbage flags, since pages
repeat most of their tokens.
"""

from __future__ import annotations

import csv
import gzip
import zlib
import hashlib
import io
import json
import multiprocessing
import os
import statistics
import time
from array import array
from concurrent.futures import FIRST_COMPLETED, ProcessPoolExecutor, wait
from dataclasses import dataclass, field
from datetime import datetime, timedelta, timezone

import jaocr
# The v2 metric's per-page code lives in damage_v2.py (the Rust port's parity reference); these names stay
# importable from here.
from damage_v2 import (  # noqa: F401
    AA_SPELLING, BADLY_DAMAGED, CORE, DAMAGED, DEHYPHENATE, DETECTED, ENGLISH, LANGUAGE_SCRIPTS, LATIN_LETTERS,
    LEAD, LIGATURES, LOCAL_WORDLISTS, MIN_SHARE, MIN_TAIL, MIN_WORDS, MIXED, MIXED_MIN_WORDS, MIXED_SHARE,
    NO_TEXT, PAGE_TYPES, RELATED, ROMAN, SCRIPT_RANGES, SERBIAN_LATIN, SYLLABIC_CONSONANTS, THIN_LETTERS,
    TOP_FUNCTION, TOP_HEAD, TOP_REAL, TRIPLE, UNKNOWN, VOWELS, WINDOW, WINDOW_TYPES, WORDFREQ, WORDLIST_DIR,
    Damage, Fit, LangModel, Language, Models, _edits, _r, build_model, counted, damage, detect, fits, forced,
    garbage, latin, latin_language, mixed_case, one_edit, other_script_language, score_v2, script,
    script_language, scripts_of, text_words, token_info, windows_of, winner, wordfreq_ranked,
)

COLUMNS = ["doc_id", "lccn", "date", "text_status", "text"]
WORDLIST_SIZE = 200_000
POOR, FAIR = 0.5, 0.7
WORST_TITLES, WORST_BATCHES = 100, 50
PROGRESS_EVERY = 60  # seconds between progress lines and status updates
CHUNK_PARTS = 40  # parts per chunk of work a replica claims (whole batches, so a chunk can run over)
POLL_SECONDS = 30  # how often a replica with nothing left to claim checks whether the others are done
RENEW_SECONDS = 300  # how often a replica renews its claims (and the reducer its lock)
JAPANESE = "jpn"


# ---------------------------------------------------------------- per page


def sampled(doc_id: str, cut: int) -> bool:
    """Whether a page is in the sample: a stable hash of its doc_id, mod 10,000, below `cut`."""
    h = int.from_bytes(hashlib.blake2b(doc_id.encode(), digest_size=8).digest(), "big")
    return h % 10_000 < cut



@dataclass
class PageScore:
    dict_share: float | None
    garbage_share: float | None
    chars: int
    tokens: int
    words: int


def score(text: str, words: frozenset | None, lang: str) -> PageScore:
    """dict_share (None without a word list or word tokens), garbage_share, chars, tokens, words."""
    tokens = text.split()
    flagged = known = n_words = 0
    for tok in tokens:
        m = CORE.search(tok)
        core = m.group(0) if m else None
        if garbage(tok, core, lang):
            flagged += 1
        if core and len(core) >= 2 and core.isalpha():
            n_words += 1
            if words is not None and core.casefold() in words:
                known += 1
    return PageScore(
        dict_share=known / n_words if words is not None and n_words else None,
        garbage_share=flagged / len(tokens) if tokens else None,
        chars=len(text), tokens=len(tokens), words=n_words,
    )


# ---------------------------------------------------------------- languages


def title_language(title: dict | None) -> tuple[str, bool]:
    """The page's language (the title's first catalog language) and whether the title lists more."""
    langs = (title or {}).get("languages") or []
    return (langs[0] if langs else UNKNOWN), len(langs) > 1


def wordlist_code(lang: str) -> str | None:
    return WORDFREQ.get(lang)


def wordfreq_words(code: str) -> frozenset | None:
    """wordfreq's top words for a language, or None if it has no list of its own."""
    import wordfreq

    if code not in wordfreq.available_languages("best"):
        return None
    words = frozenset(wordfreq.top_n_list(code, WORDLIST_SIZE))
    # top_n_list and the frequency list it reads are cached; the set is all we need.
    wordfreq.top_n_list.cache_clear()
    wordfreq.get_frequency_list.cache_clear()
    return words


class WordLists:
    """Word lists by wordfreq code, each loaded once. `loader` is replaceable for tests."""

    def __init__(self, loader=None):
        self.loader = loader or wordfreq_words
        self.lists: dict[str, frozenset | None] = {}

    def get(self, code: str | None) -> frozenset | None:
        if code is None:
            return None
        if code not in self.lists:
            self.lists[code] = self.loader(code)
        return self.lists[code]


# ---------------------------------------------------------------- per part (in a worker)


@dataclass
class TitleInfo:
    lang: str
    multilingual: bool
    wordlist: str | None
    japanese: bool
    languages: tuple = ()


_W: dict = {}


def init_worker(curated, titles: dict[str, TitleInfo], cut: int, loader=None, metric: str = "v1") -> None:
    _W.update(curated=curated, titles=titles, cut=cut, metric=metric,
              words=WordLists(loader) if metric == "v1" else Models(loader))


def scan_part(path: str) -> dict:
    """Read one curated part and score its sampled pages: {pages_read, pages: [...]} or {error}."""
    import pyarrow as pa
    import pyarrow.parquet as pq

    titles, cut, lists, v2 = _W["titles"], _W["cut"], _W["words"], _W["metric"] == "v2"
    try:
        f = pq.ParquetFile(io.BytesIO(_W["curated"].read(path)))
        read, out = 0, []
        for rb in f.iter_batches(batch_size=4096, columns=COLUMNS):
            ids = rb.column("doc_id").to_pylist()
            read += len(ids)
            keep = [sampled(d, cut) for d in ids]
            if not any(keep):
                continue
            for r in rb.filter(pa.array(keep)).to_pylist():
                t = titles.get(r["lccn"])
                status = r["text_status"]
                page = {"lccn": r["lccn"], "decade": r["date"].year // 10 * 10, "status": status}
                if v2:
                    page.update(doc_id=r["doc_id"], date=r["date"].isoformat())
                if status == "ok" and r["text"] and not (t and t.japanese) and v2:
                    page.update(score_v2(r["text"], t.languages if t else (), lists))
                elif status == "ok" and r["text"] and not (t and t.japanese):
                    lang = t.lang if t else UNKNOWN
                    s = score(r["text"], lists.get(t.wordlist if t else None), lang)
                    page.update(dict_share=s.dict_share, garbage_share=s.garbage_share,
                                chars=s.chars, tokens=s.tokens)
                out.append(page)
        return {"pages_read": read, "pages": out}
    except Exception as e:  # noqa: BLE001 - one part mustn't end the audit
        return {"error": f"{type(e).__name__}: {e}"[:300]}


# ---------------------------------------------------------------- aggregation


@dataclass
class Stats:
    pages: int = 0
    empty: int = 0
    short: int = 0
    multilingual: int = 0
    dict_shares: array = field(default_factory=lambda: array("d"))
    garbage_sum: float = 0.0
    garbage_n: int = 0
    chars: int = 0
    tokens: int = 0
    scored_text: int = 0  # ok pages with text that were scored

    def add(self, page: dict, multilingual: bool) -> None:
        self.pages += 1
        self.multilingual += multilingual
        if page["status"] == "empty":
            self.empty += 1
        elif page["status"] == "short":
            self.short += 1
        if "chars" in page:
            self.scored_text += 1
            self.chars += page["chars"]
            self.tokens += page["tokens"]
            if page["garbage_share"] is not None:
                self.garbage_sum += page["garbage_share"]
                self.garbage_n += 1
            if page["dict_share"] is not None:
                self.dict_shares.append(page["dict_share"])

    def merge(self, other: "Stats") -> "Stats":
        self.pages += other.pages
        self.empty += other.empty
        self.short += other.short
        self.multilingual += other.multilingual
        self.dict_shares.extend(other.dict_shares)
        self.garbage_sum += other.garbage_sum
        self.garbage_n += other.garbage_n
        self.chars += other.chars
        self.tokens += other.tokens
        self.scored_text += other.scored_text
        return self

    def row(self) -> dict:
        d = self.dict_shares
        n = len(d)

        def r(x):
            return None if x is None else round(x, 4)

        return {
            "pages": self.pages,
            "multilingual_pages": self.multilingual,
            "empty_share": r(self.empty / self.pages) if self.pages else None,
            "short_share": r(self.short / self.pages) if self.pages else None,
            "scored_pages": n,
            "dict_share_mean": r(sum(d) / n) if n else None,
            "dict_share_median": r(statistics.median(d)) if n else None,
            "poor_share": r(sum(1 for x in d if x < POOR) / n) if n else None,
            "fair_share": r(sum(1 for x in d if x < FAIR) / n) if n else None,
            "garbage_share_mean": r(self.garbage_sum / self.garbage_n) if self.garbage_n else None,
            "chars_mean": round(self.chars / self.scored_text) if self.scored_text else None,
            "tokens_mean": round(self.tokens / self.scored_text) if self.scored_text else None,
        }


class Tally:
    """Everything the report needs, from the sampled pages, one part at a time."""

    def __init__(self, titles: dict[str, TitleInfo]):
        self.titles = titles
        self.by_lang_decade: dict[tuple[str, int], Stats] = {}
        self.by_title: dict[str, Stats] = {}
        self.by_batch: dict[str, Stats] = {}
        self.batch_langs: dict[str, dict[str, int]] = {}
        self.japanese = Stats()
        self.untitled = 0

    def add(self, batch: str, pages: list[dict]) -> None:
        for p in pages:
            t = self.titles.get(p["lccn"])
            if t is None:
                self.untitled += 1
            if t and t.japanese:
                self.japanese.add(p, t.multilingual)
                continue
            lang, multi = (t.lang, t.multilingual) if t else (UNKNOWN, False)
            self.by_lang_decade.setdefault((lang, p["decade"]), Stats()).add(p, multi)
            if t and t.wordlist:
                self.by_title.setdefault(p["lccn"], Stats()).add(p, multi)
                self.by_batch.setdefault(batch, Stats()).add(p, multi)
                langs = self.batch_langs.setdefault(batch, {})
                langs[lang] = langs.get(lang, 0) + 1

    def tables(self, catalog: dict[str, dict], min_pages: int) -> dict[str, list[dict]]:
        by_lang: dict[str, Stats] = {}
        by_decade: dict[int, Stats] = {}
        for (lang, decade), s in self.by_lang_decade.items():
            by_lang.setdefault(lang, Stats()).merge(s)
            by_decade.setdefault(decade, Stats()).merge(s)

        def lrow(lang, decade, s):
            return {"language": lang, "wordlist": wordlist_code(lang) if lang != "all" else None,
                    "decade": decade, **s.row()}

        lang_decade = [lrow(lang, dec, s) for (lang, dec), s in
                       sorted(self.by_lang_decade.items(), key=lambda kv: (-by_lang[kv[0][0]].pages, kv[0]))]
        languages = [lrow(lang, "all", s) for lang, s in sorted(by_lang.items(), key=lambda kv: (-kv[1].pages, kv[0]))]
        decades = [lrow("all", dec, s) for dec, s in sorted(by_decade.items())]

        def worst(groups: dict[str, Stats], n: int):
            ok = [(k, s.row()) for k, s in groups.items() if len(s.dict_shares) >= min_pages]
            ok.sort(key=lambda kv: (kv[1]["dict_share_median"], kv[0]))
            return ok[:n]

        titles = []
        for rank, (lccn, row) in enumerate(worst(self.by_title, WORST_TITLES), 1):
            c, t = catalog.get(lccn, {}), self.titles[lccn]
            titles.append({"rank": rank, "lccn": lccn, "name": c.get("name", ""), "language": t.lang,
                           "languages": " ".join(c.get("languages") or []), "multilingual": t.multilingual,
                           "place": c.get("place_of_publication") or c.get("place_id", ""),
                           "state": c.get("state", ""), **row})
        batches = []
        for rank, (batch, row) in enumerate(worst(self.by_batch, WORST_BATCHES), 1):
            mix = sorted(self.batch_langs[batch].items(), key=lambda kv: (-kv[1], kv[0]))
            batches.append({"rank": rank, "batch": batch,
                            "languages": " ".join(f"{k}:{v}" for k, v in mix), **row})
        return {"by_language_decade": lang_decade, "by_language": languages, "by_decade": decades,
                "worst_titles": titles, "worst_batches": batches}


# ---------------------------------------------------------------- v2: tables (the metric itself: damage_v2.py)

WORST_UNDETERMINED = 100


def bucket(page: dict) -> str:
    """The page's row in the by-language tables: its detected language, mixed, und or no_text."""
    if "decision" not in page:
        return NO_TEXT
    return MIXED if page["decision"] == MIXED else page["lang"]


def agreement_label(page: dict) -> str:
    """The page's column in the language crosstab: as bucket(), with a mixed page's two languages."""
    if page.get("decision") == MIXED:
        return f"mixed:{page['lang']}+{page['mixed_with']}"
    return bucket(page)


PAGE_COLUMNS = ["doc_id", "lccn", "date", "batch", "title_language", "title_languages", "status", "language",
                "decision", "mixed_with", "share", "runner_up", "runner_up_share", "script", "function_share",
                "damage_rate", "garbage_share", "words"]


def _csv_num(x: float | None) -> str:
    return "" if x is None else f"{x:.4f}".rstrip("0").rstrip(".")


def quantile(sorted_xs, q: float) -> float:
    return sorted_xs[round(q * (len(sorted_xs) - 1))]


@dataclass
class Stats2:
    pages: int = 0
    empty: int = 0
    short: int = 0
    multilingual: int = 0
    text_pages: int = 0  # ok pages with text
    und: int = 0
    mixed: int = 0
    function_shares: array = field(default_factory=lambda: array("d"))
    damage_rates: array = field(default_factory=lambda: array("d"))
    garbage_sum: float = 0.0
    garbage_n: int = 0
    chars: int = 0
    tokens: int = 0

    def add(self, page: dict, multilingual: bool) -> None:
        self.pages += 1
        self.multilingual += multilingual
        if page["status"] == "empty":
            self.empty += 1
        elif page["status"] == "short":
            self.short += 1
        if "decision" not in page:
            return
        self.text_pages += 1
        self.und += page["decision"] == UNKNOWN
        self.mixed += page["decision"] == MIXED
        self.chars += page["chars"]
        self.tokens += page["tokens"]
        if page["garbage_share"] is not None:
            self.garbage_sum += page["garbage_share"]
            self.garbage_n += 1
        if page["function_share"] is not None:
            self.function_shares.append(page["function_share"])
        if page["damage_rate"] is not None:
            self.damage_rates.append(page["damage_rate"])

    def merge(self, other: "Stats2") -> "Stats2":
        for f in ("pages", "empty", "short", "multilingual", "text_pages", "und", "mixed", "garbage_sum",
                  "garbage_n", "chars", "tokens"):
            setattr(self, f, getattr(self, f) + getattr(other, f))
        self.function_shares.extend(other.function_shares)
        self.damage_rates.extend(other.damage_rates)
        return self

    def row(self) -> dict:
        def r(x):
            return None if x is None else round(x, 4)

        def share(k, n):
            return r(k / n) if n else None

        f, d = sorted(self.function_shares), sorted(self.damage_rates)
        nf, nd = len(f), len(d)
        return {
            "pages": self.pages,
            "multilingual_pages": self.multilingual,
            "empty_share": share(self.empty, self.pages),
            "short_share": share(self.short, self.pages),
            "text_pages": self.text_pages,
            "und_share": share(self.und, self.text_pages),
            "mixed_share": share(self.mixed, self.text_pages),
            "scored_pages": nf,
            "function_share_mean": r(sum(f) / nf) if nf else None,
            "function_share_median": r(statistics.median(f)) if nf else None,
            "function_share_p10": r(quantile(f, 0.1)) if nf else None,
            "function_share_p25": r(quantile(f, 0.25)) if nf else None,
            "function_share_p75": r(quantile(f, 0.75)) if nf else None,
            "function_share_p90": r(quantile(f, 0.9)) if nf else None,
            "damage_pages": nd,
            "damage_rate_mean": r(sum(d) / nd) if nd else None,
            "damage_rate_median": r(statistics.median(d)) if nd else None,
            "damaged_share": share(sum(1 for x in d if x > DAMAGED), nd),
            "badly_damaged_share": share(sum(1 for x in d if x > BADLY_DAMAGED), nd),
            "garbage_share_mean": share(self.garbage_sum, self.garbage_n),
            "chars_mean": round(self.chars / self.text_pages) if self.text_pages else None,
            "tokens_mean": round(self.tokens / self.text_pages) if self.text_pages else None,
        }


@dataclass
class Agreement:
    text_pages: int = 0
    same: int = 0  # detected as the title's first language
    other: int = 0  # detected as another language
    und: int = 0
    mixed: int = 0
    mixed_with_first: int = 0  # mixed, one of the two the title's first language

    def add(self, page: dict, first: str) -> None:
        if "decision" not in page:
            return
        self.text_pages += 1
        if page["decision"] == UNKNOWN:
            self.und += 1
        elif page["decision"] == MIXED:
            self.mixed += 1
            self.mixed_with_first += first in (page["lang"], page["mixed_with"])
        elif page["lang"] == first:
            self.same += 1
        else:
            self.other += 1

    def row(self) -> dict:
        n = self.text_pages

        def share(k):
            return round(k / n, 4) if n else None

        return {"text_pages": n, "same_share": share(self.same), "differs_share": share(n - self.same),
                "other_language_share": share(self.other), "und_share": share(self.und),
                "mixed_share": share(self.mixed), "mixed_with_first_share": share(self.mixed_with_first)}


# The per-page file is gzipped as pages come in, so memory holds only the compressed file; past this many
# rows (a sample of over about 12% of the corpus) the file is dropped, since the tables cover such a sample.
MAX_PAGE_ROWS = 3_000_000


class Tally2:
    """The v2 tables, from the sampled pages, one part at a time."""

    def __init__(self, titles: dict[str, TitleInfo], max_page_rows: int = MAX_PAGE_ROWS):
        self.titles = titles
        self.by_lang_decade: dict[tuple[str, int], Stats2] = {}
        self.by_title: dict[str, Stats2] = {}
        self.by_batch: dict[str, Stats2] = {}
        self.title_mix: dict[str, dict[str, int]] = {}
        self.batch_mix: dict[str, dict[str, int]] = {}
        self.crosstab: dict[tuple[str, str], int] = {}
        self.agreement: dict[str, Agreement] = {}
        self.japanese = Stats2()
        self.untitled = 0
        # Every sampled page (not Japanese) as CSV, gzipped as it comes in (MAX_PAGE_ROWS).
        self.max_page_rows = max_page_rows
        self.page_count = 0
        self._gz = zlib.compressobj(6, zlib.DEFLATED, 31)
        self.pages_gz: list[bytes] = [self._gz.compress((",".join(PAGE_COLUMNS) + "\n").encode())]

    def add(self, batch: str, pages: list[dict]) -> None:
        buf = io.StringIO()
        out = csv.writer(buf, lineterminator="\n")
        for p in pages:
            t = self.titles.get(p["lccn"])
            if t is None:
                self.untitled += 1
            if t and t.japanese:
                self.japanese.add(p, t.multilingual)
                continue
            first, multi = (t.lang, t.multilingual) if t else (UNKNOWN, False)
            b = bucket(p)
            out.writerow([p.get("doc_id", ""), p["lccn"], p.get("date", ""), batch, first,
                          " ".join(t.languages) if t else "", p["status"], b if "decision" not in p else p["lang"],
                          p.get("decision", NO_TEXT), p.get("mixed_with") or "", _csv_num(p.get("share")),
                          p.get("runner_up") or "", _csv_num(p.get("runner_up_share")), p.get("script", ""),
                          _csv_num(p.get("function_share")), _csv_num(p.get("damage_rate")),
                          _csv_num(p.get("garbage_share")), p.get("words", "")])
            self.by_lang_decade.setdefault((b, p["decade"]), Stats2()).add(p, multi)
            self.by_title.setdefault(p["lccn"], Stats2()).add(p, multi)
            self.by_batch.setdefault(batch, Stats2()).add(p, multi)
            for mix, key in ((self.title_mix, p["lccn"]), (self.batch_mix, batch)):
                m = mix.setdefault(key, {})
                m[b] = m.get(b, 0) + 1
            label = agreement_label(p)
            if buf.tell() > 1 << 16:
                self._flush_pages(buf)
            self.crosstab[(first, label)] = self.crosstab.get((first, label), 0) + 1
            kind = "untitled_pages" if t is None else "multilingual_titles" if multi else "single_language_titles"
            for scope in ("all", kind, f"title_language:{first}"):
                self.agreement.setdefault(scope, Agreement()).add(p, first)
        if buf.tell():
            self._flush_pages(buf)

    def _flush_pages(self, buf: io.StringIO) -> None:
        if self._gz is not None:
            self.page_count += buf.getvalue().count("\n")
            if self.page_count > self.max_page_rows:
                # Too large to keep: drop the file and stop collecting it.
                self._gz = None
                self.pages_gz = []
                jaocr.log("ocr quality pages file dropped", max_rows=self.max_page_rows)
            else:
                self.pages_gz.append(self._gz.compress(buf.getvalue().encode()))
        buf.seek(0)
        buf.truncate()

    def pages_csv_gz(self) -> bytes | None:
        """Every sampled page (but Japanese titles'): its language, the decision and its scores, gzipped;
        None when the sample was too large to export."""
        if self._gz is None:
            return None
        return b"".join(self.pages_gz) + self._gz.flush()

    def tables(self, catalog: dict[str, dict], min_pages: int) -> dict[str, list[dict]]:
        by_lang: dict[str, Stats2] = {}
        by_decade: dict[int, Stats2] = {}
        for (lang, decade), s in self.by_lang_decade.items():
            by_lang.setdefault(lang, Stats2()).merge(s)
            by_decade.setdefault(decade, Stats2()).merge(s)

        def lrow(lang, decade, s):
            return {"metric": "v2", "language": lang, "wordlist": WORDFREQ.get(lang) or LOCAL_WORDLISTS.get(lang),
                    "decade": decade, **s.row()}

        lang_decade = [lrow(lang, dec, s) for (lang, dec), s in
                       sorted(self.by_lang_decade.items(), key=lambda kv: (-by_lang[kv[0][0]].pages, kv[0]))]
        languages = [lrow(lang, "all", s) for lang, s in sorted(by_lang.items(), key=lambda kv: (-kv[1].pages, kv[0]))]
        decades = [lrow("all", dec, s) for dec, s in sorted(by_decade.items())]

        firsts: dict[str, int] = {}
        for (first, _), n in self.crosstab.items():
            firsts[first] = firsts.get(first, 0) + n
        crosstab = [{"metric": "v2", "title_language": first, "detected": label, "pages": n,
                     "share": round(n / firsts[first], 4)}
                    for (first, label), n in sorted(self.crosstab.items(),
                                                    key=lambda kv: (-firsts[kv[0][0]], kv[0][0], -kv[1], kv[0][1]))]

        def scope_key(kv):
            k = kv[0]
            fixed = {"all": 0, "single_language_titles": 1, "multilingual_titles": 2, "untitled_pages": 3}
            return (fixed.get(k, 4), -kv[1].text_pages, k)

        summary = [{"metric": "v2", "scope": k, **a.row()} for k, a in sorted(self.agreement.items(), key=scope_key)]

        def mix(counts: dict[str, int]) -> str:
            return " ".join(f"{k}:{v}" for k, v in sorted(counts.items(), key=lambda kv: (-kv[1], kv[0])))

        def title_row(rank, lccn, row):
            c, t = catalog.get(lccn, {}), self.titles.get(lccn)
            return {"metric": "v2", "rank": rank, "lccn": lccn, "name": c.get("name", ""),
                    "language": t.lang if t else UNKNOWN, "languages": " ".join(c.get("languages") or []),
                    "multilingual": bool(t and t.multilingual),
                    "place": c.get("place_of_publication") or c.get("place_id", ""), "state": c.get("state", ""),
                    "detected": mix(self.title_mix[lccn]), **row}

        def worst(groups: dict[str, Stats2], n: int):
            ok = [(k, s.row()) for k, s in groups.items() if len(s.damage_rates) >= min_pages]
            ok.sort(key=lambda kv: (-kv[1]["damage_rate_median"], kv[0]))
            return ok[:n]

        titles = [title_row(rank, lccn, row) for rank, (lccn, row) in enumerate(worst(self.by_title, WORST_TITLES), 1)]
        batches = [{"metric": "v2", "rank": rank, "batch": batch, "detected": mix(self.batch_mix[batch]), **row}
                   for rank, (batch, row) in enumerate(worst(self.by_batch, WORST_BATCHES), 1)]
        und = [(k, s.row()) for k, s in self.by_title.items() if s.text_pages >= min_pages and s.und]
        und.sort(key=lambda kv: (-kv[1]["und_share"], -kv[1]["text_pages"], kv[0]))
        undetermined = [title_row(rank, lccn, row) for rank, (lccn, row) in enumerate(und[:WORST_UNDETERMINED], 1)]
        return {"by_language_decade": lang_decade, "by_language": languages, "by_decade": decades,
                "language_agreement": crosstab, "agreement_summary": summary,
                "worst_titles": titles, "worst_batches": batches, "undetermined_titles": undetermined}


# ---------------------------------------------------------------- the run


def title_infos(titles: list[dict]) -> dict[str, TitleInfo]:
    out = {}
    for t in titles:
        lang, multi = title_language(t)
        out[t["lccn"]] = TitleInfo(lang=lang, multilingual=multi, wordlist=wordlist_code(lang),
                                   japanese=JAPANESE in (t.get("languages") or []),
                                   languages=tuple(t.get("languages") or ()))
    return out


def load_titles(reference, version: str) -> dict[str, dict]:
    """The published titles, plus the working catalog's for any it lacks (as jaocr.targets reads them)."""
    by_lccn = {t["lccn"]: t for t in json.loads(reference.read(f"{version}/titles.json"))}
    if reference.exists("catalog/titles.json"):
        for t in json.loads(reference.read("catalog/titles.json")):
            by_lccn.setdefault(t["lccn"], t)
    return by_lccn


def batch_name(b: dict) -> str:
    v = (b.get("curated") or {}).get("version")
    return f"{b['batch']}_ver{v:02d}" if isinstance(v, int) else b["batch"]


def pct_label(pct: float) -> str:
    return f"{pct:g}"


def results(tasks, workers: int, initargs: tuple):
    """(task, result) for each task (a tuple whose last item is the part's path), in completion order, with
    at most 2 x workers parts in flight. `tasks` may be a generator: it is drawn on as parts finish."""
    if workers <= 1:
        init_worker(*initargs)
        for task in tasks:
            yield task, scan_part(task[-1])
        return
    # Spawned, not forked: each worker unpickles the store and opens its own
    # blob client, rather than sharing the parent's connections.
    ctx = multiprocessing.get_context("spawn")
    with ProcessPoolExecutor(max_workers=workers, mp_context=ctx, initializer=init_worker,
                             initargs=initargs) as ex:
        pending: dict = {}
        it = iter(tasks)
        try:
            while True:
                while len(pending) < 2 * workers:
                    task = next(it, None)
                    if task is None:
                        break
                    pending[ex.submit(scan_part, task[-1])] = task
                if not pending:
                    return
                done, _ = wait(pending, return_when=FIRST_COMPLETED)
                for fut in done:
                    yield pending.pop(fut), fut.result()
        finally:
            ex.shutdown(cancel_futures=True)


def write_csv(rows: list[dict]) -> bytes:
    buf = io.StringIO()
    w = csv.DictWriter(buf, fieldnames=list(rows[0]) if rows else ["empty"])
    w.writeheader()
    w.writerows(rows)
    return buf.getvalue().encode()


def chunk_tasks(tasks: list[tuple[str, str]], size: int) -> list[list[tuple[str, str]]]:
    """(batch, part) tasks cut into chunks of whole batches, each of `size` parts or a little more."""
    chunks: list[list[tuple[str, str]]] = []
    current: list[tuple[str, str]] = []
    for i, task in enumerate(tasks):
        current.append(task)
        last_of_batch = i + 1 == len(tasks) or tasks[i + 1][0] != task[0]
        if last_of_batch and len(current) >= size:
            chunks.append(current)
            current = []
    if current:
        chunks.append(current)
    return chunks


def run_name(owner: str) -> str:
    """The job execution this replica belongs to: its replicas share one run. A Container Apps job replica
    is named <execution>-<suffix>."""
    name = os.environ.get("CONTAINER_APP_JOB_EXECUTION_NAME")
    if name:
        return name
    head, _, tail = owner.rpartition("-")
    return head if head and len(tail) == 5 else owner


class Chunks:
    """Claims on the run's chunks, in the curated store under `root`: claims/<k> while a replica scans chunk
    k, pages/<k>.jsonl.gz and then done/<k>.json once it has."""

    def __init__(self, curated, root: str, count: int, lease, stale: timedelta, owner: str):
        self.curated, self.root, self.count, self.lease, self.stale = curated, root, count, lease, stale
        # Replicas start at different chunks, so they rarely race for the same claim.
        start = int.from_bytes(hashlib.blake2b(owner.encode(), digest_size=4).digest(), "big") % max(count, 1)
        self.order = [(start + i) % count for i in range(count)]

    def path(self, kind: str, k: int, ext: str = "json") -> str:
        return f"{self.root}/{kind}/{k:05d}.{ext}"

    def done(self) -> set[int]:
        return {int(p.rsplit("/", 1)[1].split(".")[0]) for p in self.curated.list(f"{self.root}/done/")}

    def claim(self, active=()) -> int | None:
        """A chunk no replica has finished or is scanning (or one whose claim went stale), now ours. `active`:
        the chunks this replica is scanning, which it mustn't claim again."""
        done = self.done()
        for k in self.order:
            if k in done or k in active:
                continue
            claim = self.path("claims", k)
            if self.curated.create(claim, self.lease()) or self.curated.take_over(claim, self.lease(), self.stale):
                if self.curated.exists(self.path("done", k)):  # finished between the listing and the claim
                    continue
                return k
        return None

    def renew(self, k: int, owner: str) -> bool:
        """Keep our claim on chunk k fresh; False if another replica has taken it over."""
        return self.curated.renew(self.path("claims", k), self.lease(), owner)


def quality(reference, curated, sample_pct: float = 2.0, min_pages: int = 50,
            workers: int | None = None, loader=None, metric: str = "v2", run: str | None = None) -> dict | None:
    """Run the audit. `metric` "v2" (the default) or "v1"; `loader` replaces wordfreq in tests (v1: code ->
    word set; v2: code -> ranked words). `run` names the run the job's replicas share (default: the job
    execution). Returns the report, or None in a replica that leaves writing it to another."""
    if metric not in ("v1", "v2"):
        raise ValueError(f"--metric must be v1 or v2, not {metric!r}")
    if not 0 < sample_pct <= 100:
        raise ValueError(f"--sample-pct must be above 0 and at most 100, not {sample_pct}")
    if min_pages < 1:
        raise ValueError(f"--min-pages must be at least 1, not {min_pages}")
    cut = round(sample_pct * 100)
    if cut < 1:
        raise ValueError(f"--sample-pct {sample_pct} samples nothing: use at least 0.01")
    current = json.loads(reference.read("current.json"))
    version = current["reference"]
    if not jaocr.SAFE_SEGMENT.match(version):
        raise ValueError(f"current.json names an unsafe reference version: {version!r}")
    tag = "ocr-quality-v2" if metric == "v2" else "ocr-quality"
    base = f"audit/{tag}-{version}-{pct_label(sample_pct)}pct"

    # The job's replicas share the work. The parts are cut into chunks of
    # whole batches; a replica claims a chunk (a claim older than
    # JAOCR_AUDIT_LOCK_MINUTES, 60, is taken over), scans it, and writes its
    # sampled pages under the run's directory. Once every chunk is done, the
    # replica that creates the run's reduce lock tallies them all and writes
    # the outputs; the others stop. A run is one job execution, so a rerun
    # starts afresh and overwrites the outputs.
    owner = os.environ.get("CONTAINER_APP_REPLICA_NAME") or os.environ.get("HOSTNAME") or "local"
    run = run or run_name(owner)
    if not jaocr.SAFE_SEGMENT.match(run):
        raise ValueError(f"unsafe run name: {run!r}")
    root = f"{base}.run/{run}"

    def lease() -> bytes:
        return json.dumps({"owner": owner, "at": datetime.now(timezone.utc).isoformat()}).encode()

    stale = timedelta(minutes=float(os.environ.get("JAOCR_AUDIT_LOCK_MINUTES", "60")))

    catalog = load_titles(reference, version)
    titles = title_infos(list(catalog.values()))
    batches = json.loads(reference.read(f"{version}/batches.json"))
    tasks = [(batch_name(b), p) for b in batches for p in (b.get("curated") or {}).get("parts") or []]
    chunks = chunk_tasks(tasks, CHUNK_PARTS)
    chunk_batches = [len({b for b, _ in c}) for c in chunks]
    empty_batches = len(batches) - len({b for b, _ in tasks})  # batches with no parts
    if workers is None:
        workers = int(os.environ.get("JAOCR_QUALITY_WORKERS", "6"))
    jaocr.log("ocr quality starting", metric=metric, version=version, sample_pct=sample_pct,
              min_pages=min_pages, batches=len(batches), parts=len(tasks), chunks=len(chunks), titles=len(titles),
              workers=workers, run=run)

    started = time.time()
    now = _iso(datetime.now(timezone.utc))
    status = {"batches": {"done": empty_batches, "total": len(batches)}, "finished_at": None, "metric": metric,
              "pages_sampled": 0, "sample_pct": float(sample_pct), "started_at": now, "summary": None,
              "updated_at": now, "version": version}
    if curated.create(f"{root}/started.json", lease()):
        if metric == "v2":
            write_status(reference, status)
    else:
        status["started_at"] = _iso(datetime.fromisoformat(json.loads(curated.read(f"{root}/started.json"))["at"]))

    work = Chunks(curated, root, len(chunks), lease, stale, owner)
    finished = f"{root}/finished.json"
    reduce_lock = f"{root}/reduce.lock"
    if curated.exists(finished):  # a retried replica of a run that is over
        jaocr.log("ocr quality run already finished", run=run)
        return None
    last_progress = last_renew = time.time()

    def progress() -> None:
        totals = {"pages_read": 0, "pages_sampled": 0, "parts_failed": 0}
        done = work.done()
        if len(done) == len(chunks):
            return  # the final status is the reducing replica's to write
        for k in done:
            d = json.loads(curated.read(work.path("done", k)))
            for key in totals:
                totals[key] += d[key]
        batches_done = empty_batches + sum(chunk_batches[k] for k in done)
        jaocr.log("ocr quality progress", metric=metric, batches_done=batches_done, batches=len(batches),
                  chunks_done=len(done), chunks=len(chunks), elapsed_s=round(time.time() - started), **totals)
        if metric == "v2" and not curated.exists(reduce_lock):
            status["batches"]["done"], status["pages_sampled"] = batches_done, totals["pages_sampled"]
            status["updated_at"] = _iso(datetime.now(timezone.utc))
            write_status(reference, status)

    left: dict[int, int] = {}
    parts_out: dict[int, list[dict]] = {}

    def claimed():
        while (k := work.claim(active=left)) is not None:
            left[k], parts_out[k] = len(chunks[k]), []
            for batch, part in chunks[k]:
                yield k, batch, part

    while True:
        for (k, batch, part), res in results(claimed(), workers, (curated, titles, cut, loader, metric)):
            parts_out[k].append({"batch": batch, "part": part, **res})
            left[k] -= 1
            if time.time() - last_renew >= RENEW_SECONDS:
                for j in list(left):
                    if not work.renew(j, owner):
                        jaocr.log("ocr quality chunk taken over", run=run, chunk=j)
                last_renew = time.time()
            if left[k]:
                continue
            del left[k]
            recs = parts_out.pop(k)
            # Publish only while the claim is ours. A chunk's pages are the same whoever scans it (the sample
            # and the scores are deterministic), so a replica that loses this race writes what the winner did.
            if not work.renew(k, owner):
                jaocr.log("ocr quality chunk taken over; dropping our copy", run=run, chunk=k)
                continue
            text = "".join(json.dumps(r, ensure_ascii=False, separators=(",", ":")) + "\n" for r in recs)
            curated.write(work.path("pages", k, "jsonl.gz"), gzip.compress(text.encode(), 6))
            curated.write(work.path("done", k), json.dumps({
                "owner": owner, "parts": len(recs),
                "pages_read": sum(r.get("pages_read", 0) for r in recs),
                "pages_sampled": sum(len(r.get("pages", ())) for r in recs),
                "parts_failed": sum(1 for r in recs if "error" in r)}).encode())
            if time.time() - last_progress >= PROGRESS_EVERY:
                progress()
                last_progress = time.time()
        if len(work.done()) == len(chunks):
            break
        time.sleep(POLL_SECONDS)  # other replicas still scanning; take over any claim that goes stale

    # One replica tallies and publishes; the others wait for its finished
    # marker, and take over if its lock goes stale (it died or was retried).
    while not (curated.create(reduce_lock, lease()) or curated.take_over(reduce_lock, lease(), stale)):
        if curated.exists(finished):
            done_status = json.loads(curated.read(finished)).get("status")
            if metric == "v2" and done_status and not _status_finished(reference):
                write_status(reference, done_status)  # a late progress write replaced the final status
            jaocr.log("ocr quality results written by another replica", run=run)
            return None
        time.sleep(POLL_SECONDS)

    def keep_reduce_lock() -> None:
        if not curated.renew(reduce_lock, lease(), owner):
            raise RuntimeError("ocr quality: another replica took over writing the results")

    tally = Tally2(titles) if metric == "v2" else Tally(titles)
    pages_read = pages_sampled = parts_failed = 0
    last_renew = time.time()
    for k in range(len(chunks)):
        if time.time() - last_renew >= RENEW_SECONDS:
            keep_reduce_lock()
            last_renew = time.time()
        for line in gzip.decompress(curated.read(work.path("pages", k, "jsonl.gz"))).splitlines():
            r = json.loads(line)
            if "error" in r:
                parts_failed += 1
                jaocr.log("ocr quality part failed", batch=r["batch"], part=r["part"], error=r["error"])
                continue
            pages_read += r["pages_read"]
            pages_sampled += len(r["pages"])
            tally.add(r["batch"], r["pages"])
    keep_reduce_lock()
    batches_done = len(batches)

    tables = tally.tables(catalog, min_pages)
    japanese = {"language": JAPANESE, **tally.japanese.row()}
    if metric == "v2":
        japanese = {"metric": "v2", **japanese}
    summary = {
        "metric": metric, "version": version, "sample_pct": sample_pct, "min_pages": min_pages,
        "batches": len(batches), "parts": len(tasks), "parts_failed": parts_failed,
        "pages_read": pages_read, "pages_sampled": pages_sampled,
        "japanese_pages_skipped": tally.japanese.pages, "pages_without_title": tally.untitled,
        "elapsed_s": round(time.time() - started),
    }
    if metric == "v2":
        agree = {r["scope"]: r for r in tables["agreement_summary"]}
        for scope in ("all", "multilingual_titles", "single_language_titles"):
            a = agree.get(scope) or {}
            for k in ("differs_share", "und_share", "mixed_share"):
                summary[f"{k}_{scope}"] = a.get(k)
        wordlists = {"source": "wordfreq top_n_list; for local_codes, ja-ocr/wordlists/<code>.txt (README.md there)",
                     "local_codes": LOCAL_WORDLISTS, "function_words": TOP_FUNCTION, "damage_words": TOP_HEAD,
                     "real_words": TOP_REAL, "codes": WORDFREQ, "version": _wordfreq_version()}
        method = {"min_words": MIN_WORDS, "min_share": MIN_SHARE, "page_types": PAGE_TYPES,
                  "window_types": WINDOW_TYPES, "lead": LEAD, "window": WINDOW,
                  "mixed_min_words": MIXED_MIN_WORDS, "min_tail": MIN_TAIL, "mixed_share": MIXED_SHARE,
                  "thin_letters": THIN_LETTERS, "damaged": DAMAGED, "badly_damaged": BADLY_DAMAGED}
    else:
        wordlists = {"source": "wordfreq top_n_list", "size": WORDLIST_SIZE, "codes": WORDFREQ,
                     "version": _wordfreq_version()}
        method = {"poor": POOR, "fair": FAIR}
    report = {"summary": summary, "generated_at": datetime.now(timezone.utc).isoformat(),
              "wordlists": wordlists, "method": method, "japanese_skipped": japanese, **tables}
    # Outputs are per version and sample size, shared by every execution: one publishes at a time (an
    # overlapping run waits; a lock not renewed for JAOCR_AUDIT_LOCK_MINUTES is taken over).
    publish_lock = f"{base}.publish.lock"
    while not (curated.create(publish_lock, lease()) or curated.renew(publish_lock, lease(), "")
               or curated.take_over(publish_lock, lease(), stale)):
        keep_reduce_lock()
        time.sleep(POLL_SECONDS)

    def publish(path: str, data: bytes) -> None:
        """Write one output while both locks are still ours; a reducer that lost either stops here."""
        keep_reduce_lock()
        if not curated.renew(publish_lock, lease(), owner):
            raise RuntimeError("ocr quality: another run took over publishing the outputs")
        curated.write(path, data)

    publish(f"{base}.json", json.dumps(report, ensure_ascii=False, indent=1).encode())
    paths = [f"{base}.json"]
    for name, rows in tables.items():
        path = f"{base}-{name.replace('_', '-')}.csv"
        publish(path, write_csv(rows))
        paths.append(path)
    if metric == "v2":
        pages_gz = tally.pages_csv_gz()
        if pages_gz is None:
            jaocr.log("ocr quality pages file skipped", max_rows=MAX_PAGE_ROWS)
        else:
            publish(f"{base}-pages.csv.gz", pages_gz)
            paths.append(f"{base}-pages.csv.gz")

    first = ("by_language", "by_decade", "by_language_decade")
    for name in first:
        for r in tables[name]:
            jaocr.log("ocr quality", table=name, version=version, **{"metric": metric, **r})
    jaocr.log("ocr quality", table="japanese_skipped", version=version, **{"metric": metric, **japanese})
    for name in tables:
        if name not in first:
            for r in tables[name]:
                jaocr.log("ocr quality", table=name, version=version, **{"metric": metric, **r})
    jaocr.log("ocr quality finished", **summary, outputs=paths)
    keep_reduce_lock()
    if metric == "v2":
        now = _iso(datetime.now(timezone.utc))
        status.update(batches={"done": batches_done, "total": len(batches)}, pages_sampled=pages_sampled,
                      updated_at=now, finished_at=now, summary=status_summary(tables))
        write_status(reference, status)
    keep_reduce_lock()
    if not curated.renew(publish_lock, json.dumps({"owner": "", "released_by": owner}).encode(), owner):
        raise RuntimeError("ocr quality: another run took over publishing the outputs")
    curated.write(finished, json.dumps({"owner": owner, "at": _iso(datetime.now(timezone.utc)), "outputs": paths,
                                        "status": status if metric == "v2" else None}).encode())
    return report


# Progress and headline numbers for the site's status page, in the reference store (which the API
# reads), as jaocr.py's status/ocr-ja.json: written by the replica that starts the run, by any
# replica with each progress line, and by the one that writes the results at the end. A run that fails leaves its last progress, no finished_at.
STATUS = "status/ocr-quality.json"
STATUS_MIN_PAGES = 200  # a detected language needs this many sampled pages to be listed


def _status_finished(reference) -> bool:
    try:
        return json.loads(reference.read(STATUS)).get("finished_at") is not None
    except Exception:  # noqa: BLE001 - missing or unreadable: rewrite it
        return False


def _iso(t: datetime) -> str:
    return t.astimezone(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def status_summary(tables: dict[str, list[dict]]) -> dict:
    """The v2 numbers the status page shows: language agreement, and per detected language its damage.

    Every field is present; these are null when there is nothing to measure: agreement.differs_share,
    mixed_share and und_share (no sampled page with text), agreement.multilingual_differs_share (no page
    of a multilingual title with text), and a language's function_share_median, damage_rate_median and
    damaged_share (no page of it scored: a language with no word list, such as Cherokee)."""
    agree = {r["scope"]: r for r in tables["agreement_summary"]}
    a, m = agree.get("all") or {}, agree.get("multilingual_titles") or {}
    languages = [{"damage_rate_median": r["damage_rate_median"], "damaged_share": r["damaged_share"],
                  "function_share_median": r["function_share_median"], "language": r["language"],
                  "pages": r["pages"]}
                 for r in tables["by_language"]
                 if r["language"] not in (UNKNOWN, MIXED, NO_TEXT) and r["pages"] >= STATUS_MIN_PAGES]
    languages.sort(key=lambda r: (-r["pages"], r["language"]))
    return {"agreement": {"differs_share": a.get("differs_share"), "mixed_share": a.get("mixed_share"),
                          "multilingual_differs_share": m.get("differs_share"), "und_share": a.get("und_share")},
            "languages": languages}


def write_status(reference, body: dict) -> None:
    try:
        reference.write(STATUS, json.dumps(body, sort_keys=True).encode())
    except Exception as e:  # noqa: BLE001 - the status page is not worth stopping the audit for
        jaocr.log("ocr quality status write failed", error=f"{type(e).__name__}: {e}"[:300])


def _wordfreq_version() -> str | None:
    try:
        from importlib.metadata import version

        return version("wordfreq")
    except Exception:  # noqa: BLE001 - only for the report
        return None
