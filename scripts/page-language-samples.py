#!/usr/bin/env python3
"""Text of known language for the page-language detector's ground-truth test
(#289, 05 §5.5.8): fixtures/page-language/samples.jsonl.

    scripts/page-language-samples.py [--out PATH] [--languages ger,fre,...] [--check]
        [--quality-rows PATH] [--min-pages 50] [--words 300] [--per-source 3] [--pause 1.0]

For each catalog language with a word list (the 30 lists of
crates/usnm-core/data/page-language-words.txt; Croatian and Serbian share
`sh`, and each gets its own samples), it takes --per-source Wikipedia
articles and --per-source Wikisource texts in that language, and keeps about
the first --words words of each one's running prose: the <p> paragraphs of
the page as MediaWiki renders it, without headings, references, tables,
lists, figures, quotations, poems or templates' boxes (`prose`).

- Wikipedia: the first articles of TOPICS (general and historical subjects,
  found in each wiki through their Wikidata items) that the wiki has at some
  length and that give enough prose.
- Wikisource: works whose Wikidata item is in the language (P407), has an
  author (P50) and a publication date (P577) and isn't a poem, song or law,
  from the language's median decade in our corpus:
  the decade where the language's pages, counted by decade in --quality-rows
  (the OCR audit's `by_language_decade` table), reach half. A language with
  fewer than --min-pages pages there takes the whole corpus's median. Only
  works published in 1930 or earlier are taken (public domain in the US), so
  a median after the 1920s gives way to the 1920s and 1930 itself; a decade
  without enough works gives way to the nearest one with them. A page that
  is only a work's contents gives way to its first part with enough prose.
  CURATED adds works Wikidata doesn't date or link for the small
  Wikisources, with where their year comes from; EXCLUDED leaves out pages
  that aren't running prose of their language's time. Each sample records
  the median decade, the decade aimed at and the decade of its text, and why
  they differ; the fixture's README lists the texts.

Each sample is pinned to the revision it was read at (`revid`, and `url`, the
oldid permalink). --languages regenerates those languages' samples and keeps
the others'. A sample's `expect`, which records by hand what the detector
gives when it genuinely can't tell two languages apart on that text
(crates/usnm-core/tests/page_language_samples.rs), is kept when the same page
at the same revision is chosen again.

--check reads every sample's revision again and compares the text (exit 1
when any differs). MediaWiki renders an old revision with today's templates,
and a Wikisource text transcluded from its scans' Page: pages with their
current revisions, so a proofreading change shows up there: regenerate that
language. CI never runs this script: the fixture is committed, and the test
reads only it.

Requests go to the MediaWiki API and the Wikidata query service with a
descriptive User-Agent, one at a time, --pause seconds apart, waiting as long
as a 429's Retry-After asks.
"""

import argparse
import html.parser
import json
import re
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "fixtures" / "page-language" / "samples.jsonl"
QUALITY_ROWS = ROOT / "docs" / "reports" / "2026-10-language-analysis" / "data" / "ocr-quality-v2-10pct.jsonl"
WORDS_FILE = ROOT / "crates" / "usnm-core" / "data" / "page-language-words.txt"
USER_AGENT = "usnewsmap-page-language-tests/1.0 (https://github.com/tgoodyear/usnewsmap.com)"
SPARQL = "https://query.wikidata.org/sparql"
# Published in 1930 or earlier: public domain in the US (more than 95 years
# ago, as of 2026).
LAST_PD_YEAR = 1930
# How far back the nearest decade with enough works may be.
FIRST_YEAR = 1800

# Each catalog language with a word list (usnm_core::lang): its MARC code
# (what the detector reports), its word list, its wikis' code and its
# Wikidata item. Latvian, Tagalog and Hawaiian have no Wikisource of their
# own: their texts are on the multilingual one (wikisource.org), as are a few
# of other languages.
LANGUAGES = [
    ("ara", "ar", "ar", "Q13955"),
    ("bul", "bg", "bg", "Q7918"),
    ("cze", "cs", "cs", "Q9056"),
    ("dan", "da", "da", "Q9035"),
    ("dut", "nl", "nl", "Q7411"),
    ("eng", "en", "en", "Q1860"),
    ("fin", "fi", "fi", "Q1412"),
    ("fre", "fr", "fr", "Q150"),
    ("ger", "de", "de", "Q188"),
    ("gre", "el", "el", "Q36510"),
    ("haw", "haw", "haw", "Q33569"),
    ("heb", "he", "he", "Q9288"),
    ("hrv", "sh", "hr", "Q6654"),
    ("hun", "hu", "hu", "Q9067"),
    ("ice", "is", "is", "Q294"),
    ("ita", "it", "it", "Q652"),
    ("lav", "lv", "lv", "Q9078"),
    ("lit", "lt", "lt", "Q9083"),
    ("nor", "nb", "no", "Q25167"),
    ("pol", "pl", "pl", "Q809"),
    ("por", "pt", "pt", "Q5146"),
    ("rum", "ro", "ro", "Q7913"),
    ("rus", "ru", "ru", "Q7737"),
    ("slo", "sk", "sk", "Q9058"),
    ("slv", "sl", "sl", "Q9063"),
    ("spa", "es", "es", "Q1321"),
    ("srp", "sh", "sr", "Q9299"),
    ("swe", "sv", "sv", "Q9027"),
    ("tgl", "fil", "tl", "Q34057"),
    ("ukr", "uk", "uk", "Q8798"),
    ("yid", "yi", "yi", "Q8641"),
]
# Other Wikidata items a work's language may be recorded as: Norwegian
# (before Bokmål and Nynorsk were told apart), Greek, Serbo-Croatian.
ALSO_LANGUAGE = {
    "nor": ["Q9043"],
    "gre": ["Q9129"],
    "hrv": ["Q9301"],
    "srp": ["Q9301"],
}

# Wikipedia articles, by their English Wikipedia titles, in order of
# preference: general and historical subjects, which every wiki describes in
# its own words, with few English names.
TOPICS = [
    "Agriculture", "Newspaper", "Bread", "Rail transport", "Printing press", "Middle Ages",
    "Industrial Revolution", "Wheat", "Horse", "Forest", "Bridge", "Iron", "Salt", "Paper",
    "Steam engine", "Bronze Age", "Ancient Greece", "Roman Empire", "Renaissance",
    "Black Death", "Crusades", "Reformation", "Potato", "Cattle", "Wine", "Beer", "Coal",
    "Fishing", "Village", "City", "School", "Book", "Writing", "Watermill", "Windmill",
    "Ship", "Mail", "Telegraphy", "Bee", "Milk", "Cheese", "Tobacco", "Cotton", "Silk",
    "Glass", "Pottery", "Iron Age", "Stone Age", "River", "Sea", "Mountain", "Island",
    "Volcano", "Sun", "Moon", "Earth", "Water", "Rain", "Tree", "Rice", "Fish", "Dog",
    "Cat", "Bird", "Language", "History", "Music", "Song", "Family", "House", "Church (building)",
]
# Articles to try after TOPICS, for a small wiki with few long articles on
# them: its own longest articles on its own history and places.
WIKIPEDIA_EXTRA = {
    "haw": ["Hawaiʻi", "Māui", "Molokaʻi", "Keōpūolani", "Hale Aliʻi ʻIolani", "Polenekia"],
}
# A Wikipedia article this many bytes of wikitext long or more is worth reading.
MIN_ARTICLE_BYTES = 6000

# Wikidata classes of works that aren't running prose: poem, song, hymn,
# national anthem, poetry collection, law, statute.
NOT_PROSE = ["Q5185279", "Q7366", "Q484692", "Q23691", "Q12106333", "Q7748", "Q820655"]

# Works Wikidata doesn't date (or doesn't put in their language), by
# language: (wiki, page, year, author, Wikidata item or None, where the year
# comes from).
CURATED: dict[str, list[tuple[str, str, int, str, str | None, str]]] = {
    "hrv": [
        ("hr.wikisource.org", "Kako je Potjeh tražio istinu", 1916, "Ivana Brlić-Mažuranić", "Q65135039",
         "Wikidata (P577) of Priče iz davnine (Q5187168), the collection it was first published in"),
    ],
    "lav": [
        ("wikisource.org", "1.daļa (Mērnieku laiki)", 1879, "Matīss Kaudzīte, Reinis Kaudzīte", "Q11252749",
         "Wikidata (P577)"),
        ("wikisource.org", "Zelta zirgs", 1909, "Rainis", "Q20566451", "Wikidata (P577)"),
    ],
    "lit": [
        ("lt.wikisource.org", "Liūdna pasaka", 1907, "Jonas Biliūnas", None,
         "Lithuanian Wikipedia's list of Jonas Biliūnas's works"),
    ],
    "tgl": [
        ("tl.wikisource.org", "Banaag at Sikat", 1906, "Lope K. Santos", "Q4853755", "Wikidata (P577)"),
    ],
    "slo": [
        ("sk.wikisource.org", "Dva dni v Chujave", 1873, "Jonáš Záborský", "Q13407298",
         "Slovak Wikipedia's list of Jonáš Záborský's works, and the page's year category"),
    ],
}
# Pages that aren't running prose of their language's time: (wiki, page).
EXCLUDED: set[tuple[str, str]] = {
    # An index of chapter numbers, then Ben Sira's Hebrew of about 180 BC.
    ("he.wikisource.org", "בן סירא (מהדורת דוד כהנא)"),
}

_last_request = [0.0]


def fetch_json(url: str, pause: float, data: bytes | None = None) -> dict:
    """GET (or POST) a JSON URL, one request at a time, waiting out 429s and 5xx errors."""
    for attempt in range(40):
        wait = _last_request[0] + pause - time.monotonic()
        if wait > 0:
            time.sleep(wait)
        _last_request[0] = time.monotonic()
        req = urllib.request.Request(url, data=data, headers={"User-Agent": USER_AGENT, "Accept": "application/json"})
        try:
            with urllib.request.urlopen(req, timeout=180) as r:
                return json.load(r)
        except urllib.error.HTTPError as e:
            if e.code not in (429, 500, 502, 503, 504):
                raise
            after = e.headers.get("Retry-After") or ""
            delay = min(300, int(after) if after.isdigit() else 10 * (attempt + 1))
        except (urllib.error.URLError, TimeoutError, json.JSONDecodeError) as e:
            delay = 10 * (attempt + 1)
            after = type(e).__name__
        print(f"  waiting {delay}s ({after or 'retry'})", file=sys.stderr)
        time.sleep(delay)
    raise RuntimeError(f"gave up on {url}")


def api(wiki: str, pause: float, **params) -> dict:
    """A MediaWiki API request to `wiki` (a domain)."""
    params = {"format": "json", "formatversion": "2", "maxlag": "5", **params}
    out = fetch_json(f"https://{wiki}/w/api.php?" + urllib.parse.urlencode(params), pause)
    if "error" in out:
        raise LookupError(f"{wiki}: {out['error'].get('code')}: {out['error'].get('info')}")
    return out


def sparql(query: str, pause: float) -> list[dict[str, str]]:
    data = urllib.parse.urlencode({"query": query, "format": "json"}).encode()
    out = fetch_json(SPARQL, pause, data)
    return [{k: v["value"] for k, v in b.items()} for b in out["results"]["bindings"]]


def parse(wiki: str, pause: float, title: str | None = None, oldid: int | None = None) -> tuple[str, int, str]:
    """A page as MediaWiki renders it: (title, revision id, HTML)."""
    params = {"action": "parse", "prop": "text|revid", "disableeditsection": "1",
              "disablelimitreport": "1", "disabletoc": "1"}
    if oldid is not None:
        params["oldid"] = str(oldid)
    else:
        params.update(page=title, redirects="1")
    p = api(wiki, pause, **params)["parse"]
    return p["title"], p["revid"], p["text"]


# Elements whose text isn't running prose: lists, tables, figures, quotations,
# notes, maths; and the classes of templates' boxes, references, page
# numbers, poems, headers and navigation.
SKIP_TAGS = {
    "table", "ul", "ol", "dl", "figure", "figcaption", "blockquote", "sup", "sub", "style", "script",
    "math", "h1", "h2", "h3", "h4", "h5", "h6", "noscript", "ruby", "rt", "code", "pre", "small", "q",
}
SKIP_CLASS = re.compile(
    r"(?:^|\s)(?:noprint|reference|references|mw-references-wrap|poem|ws-header|headertemplate|"
    r"ws-noexport|navigation-not-searchable|navbox|metadata|hatnote|infobox|thumb|toc|pagenum|"
    r"\S*pagenum\S*|IPA|mw-cite-backlink|Z3988|error|sisterproject|hauptartikel|sieheauch|gallery|"
    r"mw-empty-elt|mwe-math-element|coordinates|geo|plainlist|sidebar|dablink|rellink|"
    r"shortdescription|mw-heading|audiolink|reference-text|footer|licenseContainer|"
    r"licence|license|wst-center|center|sidenote|wst-sidenote|ws-summary)(?:$|\s)",
    re.IGNORECASE,
)
VOID = {"br", "img", "meta", "link", "hr", "input", "wbr", "area", "base", "col", "embed", "source", "track"}


class Paragraphs(html.parser.HTMLParser):
    """The text of each <p> outside the elements and classes above, whitespace collapsed."""

    def __init__(self) -> None:
        super().__init__(convert_charrefs=True)
        self.stack: list[tuple[str, bool]] = []
        self.paras: list[str] = []
        self.cur: list[str] | None = None

    def skipping(self) -> bool:
        return any(skip for _, skip in self.stack)

    def handle_starttag(self, tag, attrs):
        if tag in VOID:
            if tag == "br" and self.cur is not None and not self.skipping():
                self.cur.append(" ")
            return
        a = dict(attrs)
        style = (a.get("style") or "").replace(" ", "").lower()
        skip = tag in SKIP_TAGS or bool(SKIP_CLASS.search(a.get("class") or "")) or "display:none" in style
        self.stack.append((tag, skip))
        if tag == "p" and self.cur is None and not self.skipping():
            self.cur = []

    def handle_endtag(self, tag):
        if tag in VOID or not any(t == tag for t, _ in self.stack):
            return
        while self.stack:
            t, _ = self.stack.pop()
            if t == "p" and self.cur is not None and not any(x == "p" for x, _ in self.stack):
                self.paras.append(" ".join("".join(self.cur).split()))
                self.cur = None
            if t == tag:
                break

    def handle_data(self, data):
        if self.cur is not None and not self.skipping():
            self.cur.append(data)


SENTENCE_END = re.compile(r"[.!?…։؟।]['\"»«”“’)\]]*$")


def prose(page_html: str, words: int, min_paragraph: int = 6, slack: int = 60) -> tuple[str, int]:
    """About the first `words` words of a page's running prose, one paragraph a line: the
    paragraphs of `min_paragraph` words or more, the last cut at the end of the sentence that
    reaches `words` (within `slack` more words, else at `words`). With its word count."""
    p = Paragraphs()
    p.feed(page_html)
    p.close()
    out, n = [], 0
    for para in p.paras:
        toks = para.split()
        if len(toks) < min_paragraph:
            continue
        if n + len(toks) >= words:
            need = words - n
            end = need
            for i in range(need - 1, min(len(toks), need + slack)):
                if SENTENCE_END.search(toks[i]):
                    end = i + 1
                    break
            out.append(" ".join(toks[:end]))
            return "\n".join(out), n + end
        out.append(para)
        n += len(toks)
    return "\n".join(out), n


def word_lists() -> list[str]:
    """The word lists' codes, from the embedded file."""
    return [line.split(" ", 1)[0] for line in WORDS_FILE.read_text(encoding="utf-8").splitlines()
            if line and not line.startswith("#")]


def median_decade(pages: dict[int, int]) -> int:
    """The decade where the pages, counted from the earliest, reach half."""
    total, seen = sum(pages.values()), 0
    for decade in sorted(pages):
        seen += pages[decade]
        if 2 * seen >= total:
            return decade
    raise ValueError("no pages")


def medians(path: Path, min_pages: int) -> tuple[dict[str, int], dict[str, int], int]:
    """Each language's median decade (MARC code; the corpus's for one with fewer than
    `min_pages` pages), its page count, and the corpus's median decade."""
    by_language: dict[str, dict[int, int]] = {}
    corpus: dict[int, int] = {}
    with path.open(encoding="utf-8") as f:
        for line in f:
            r = json.loads(line)
            if r.get("table") != "by_language_decade":
                continue
            d, n = int(r["decade"]), int(r["pages"])
            by_language.setdefault(r["language"], {})
            by_language[r["language"]][d] = by_language[r["language"]].get(d, 0) + n
            corpus[d] = corpus.get(d, 0) + n
    whole = median_decade(corpus)
    counts = {lang: sum(p.values()) for lang, p in by_language.items()}
    out = {lang: median_decade(p) if counts[lang] >= min_pages else whole for lang, p in by_language.items()}
    return out, counts, whole


def target_decade(median: int) -> tuple[int, str | None]:
    """The decade to take a language's Wikisource texts from, and why it isn't the median."""
    last = LAST_PD_YEAR // 10 * 10 - 10
    if median > last:
        return last, (f"the median decade, the {median}s, is after {LAST_PD_YEAR}: the {last}s (to "
                      f"{LAST_PD_YEAR}) are the closest whose works are public domain in the US")
    return median, None


def decade_order(target: int) -> list[int]:
    """Decades nearest `target` first (the earlier of two as near), up to the last public-domain one."""
    last = LAST_PD_YEAR // 10 * 10
    decades = range(FIRST_YEAR, last + 1, 10)
    return sorted(decades, key=lambda d: (abs(d - target), d))


def wikipedia_titles(wikis: list[str], pause: float) -> dict[str, list[str]]:
    """Each wiki's titles for TOPICS, in TOPICS' order."""
    values = " ".join(f'"{t}"@en' for t in TOPICS)
    sites = " ".join(f"<https://{w}.wikipedia.org/>" for w in wikis)
    rows = sparql(f"""SELECT ?en ?site ?title WHERE {{
  VALUES ?en {{ {values} }}
  VALUES ?site {{ {sites} }}
  ?enl schema:isPartOf <https://en.wikipedia.org/>; schema:name ?en; schema:about ?item.
  ?sl schema:about ?item; schema:isPartOf ?site; schema:name ?title.
}}""", pause)
    order = {t: i for i, t in enumerate(TOPICS)}
    out: dict[str, list[tuple[int, str]]] = {}
    for r in rows:
        wiki = urllib.parse.urlparse(r["site"]).netloc.split(".")[0]
        out.setdefault(wiki, []).append((order[r["en"]], r["title"]))
    return {w: [t for _, t in sorted(ts)] for w, ts in out.items()}


def wikisource_candidates(marc: str, wiki: str, qid: str, pause: float) -> list[dict]:
    """Works in the language with an author, a publication date in 1930 or earlier and a page on
    its Wikisource or the multilingual one, but not poems, songs or laws: (wiki, page, year,
    author, Wikidata item)."""
    langs = " ".join(f"wd:{q}" for q in [qid, *ALSO_LANGUAGE.get(marc, [])])
    rows = sparql(f"""SELECT ?item ?site ?title (MIN(YEAR(?date)) AS ?year) (SAMPLE(?authorLabel) AS ?author) WHERE {{
  VALUES ?lang {{ {langs} }}
  VALUES ?site {{ <https://{wiki}.wikisource.org/> <https://wikisource.org/> }}
  ?item wdt:P407 ?lang; wdt:P577 ?date; wdt:P50 ?a.
  ?sl schema:about ?item; schema:isPartOf ?site; schema:name ?title.
  FILTER(STRSTARTS(STR(?a), "http://www.wikidata.org/entity/"))
  FILTER NOT EXISTS {{ ?item wdt:P31 ?type. VALUES ?type {{ {" ".join("wd:" + q for q in NOT_PROSE)} }} }}
  OPTIONAL {{ ?a rdfs:label ?own. FILTER(LANG(?own) = "{wiki}") }}
  OPTIONAL {{ ?a rdfs:label ?en. FILTER(LANG(?en) = "en") }}
  BIND(COALESCE(?own, ?en, STRAFTER(STR(?a), "entity/")) AS ?authorLabel)
}} GROUP BY ?item ?site ?title""", pause)
    out = []
    for r in rows:
        year = int(r["year"]) if r.get("year", "").lstrip("-").isdigit() else None
        if year is None or not FIRST_YEAR <= year <= LAST_PD_YEAR or ":" in r["title"]:
            continue
        # A page whose title names another year (an edition's) than Wikidata's date.
        named = re.findall(r"\b(1[5-9]\d\d|20\d\d)\b", r["title"])
        if named and str(year) not in named:
            continue
        out.append({"wiki": urllib.parse.urlparse(r["site"]).netloc, "title": r["title"], "year": year,
                    "author": r.get("author"), "work": r["item"].rsplit("/", 1)[-1]})
    for w, title, year, author, work, source in CURATED.get(marc, []):
        out.append({"wiki": w, "title": title, "year": year, "author": author, "work": work,
                    "year_source": source, "curated": True})
    return out


def subpages(page_html: str, title: str) -> list[str]:
    """The subpages a page links to, in order: a work's parts, for a page that is only its contents."""
    prefix = "/wiki/" + urllib.parse.quote(title.replace(" ", "_"), safe="/:(),'!*") + "/"
    out: list[str] = []
    for href in re.findall(r'href="([^"]+)"', page_html):
        if href.startswith(prefix) and "#" not in href:
            sub = urllib.parse.unquote(href[len("/wiki/"):]).replace("_", " ")
            if sub not in out:
                out.append(sub)
    return out


def wikipedia_samples(marc: str, wordlist: str, wiki_code: str, titles: list[str], a) -> list[dict]:
    wiki = f"{wiki_code}.wikipedia.org"
    out: list[dict] = []
    titles = titles + [t for t in WIKIPEDIA_EXTRA.get(marc, []) if t not in titles]
    length: dict[str, int] = {}
    for i in range(0, len(titles), 50):
        info = api(wiki, a.pause, action="query", prop="info", titles="|".join(titles[i:i + 50]))
        length.update({p["title"]: p.get("length", 0) for p in info["query"]["pages"]})
    for t in titles:
        if len(out) == a.per_source:
            break
        if length.get(t, 0) < MIN_ARTICLE_BYTES or (wiki, t) in EXCLUDED:
            continue
        title, revid, page_html = parse(wiki, a.pause, title=t)
        text, n = prose(page_html, a.words)
        print(f"  {wiki} {title}: {n} words", file=sys.stderr)
        if n < a.words:
            continue
        out.append({"language": marc, "wordlist": wordlist, "source": "wikipedia", "wiki": wiki,
                    "title": title, "revid": revid, "url": f"https://{wiki}/w/index.php?oldid={revid}",
                    "licence": "CC BY-SA 4.0", "words": n, "text": text})
    return out


def read_text(wiki: str, title: str, a, depth: int = 0) -> tuple[str, int, str, int]:
    """A Wikisource page's prose: (title, revision, text, words). A page that is only a work's
    contents gives way to the first of its first few parts with enough prose, two levels down."""
    title, revid, page_html = parse(wiki, a.pause, title=title)
    text, n = prose(page_html, a.words)
    if n < a.words and depth < 2:
        for sub in subpages(page_html, title)[:4]:
            if (wiki, sub) in EXCLUDED:
                continue
            found = read_text(wiki, sub, a, depth + 1)
            if found[3] >= a.words:
                return found
    return title, revid, text, n


def wikisource_samples(marc: str, wordlist: str, wiki_code: str, qid: str, median: int, a) -> list[dict]:
    target, why = target_decade(median)
    cands = wikisource_candidates(marc, wiki_code, qid, a.pause)
    rank = {d: i for i, d in enumerate(decade_order(target))}
    # Curated works first within a decade, then the language's own Wikisource, then by year.
    cands.sort(key=lambda c: (rank[c["year"] // 10 * 10], not c.get("curated"),
                              c["wiki"] == "wikisource.org", c["year"], c["title"]))
    out: list[dict] = []
    authors: set[str] = set()
    tried: dict[tuple[str, str], tuple[str, int, str, int] | None] = {}
    # One text an author; then, if that isn't enough, an author's second text.
    for distinct in (True, False):
        for c in cands:
            if len(out) == a.per_source:
                break
            key = (c["wiki"], c["title"])
            if distinct and c["author"] in authors or key in EXCLUDED or (key in tried and tried[key] is None):
                continue
            if key not in tried:
                try:
                    tried[key] = read_text(c["wiki"], c["title"], a)
                except LookupError as e:
                    print(f"  {e}", file=sys.stderr)
                    tried[key] = None
                    continue
                print(f"  {c['wiki']} {tried[key][0]} ({c['year']}): {tried[key][3]} words", file=sys.stderr)
            title, revid, text, n = tried[key]
            if n < a.words or any(s["wiki"] == c["wiki"] and s["title"] == title for s in out):
                tried[key] = None
                continue
            decade = c["year"] // 10 * 10
            notes = [why] if why else []
            if decade != target:
                notes.append(f"too few public-domain works of the {target}s with enough prose on Wikisource: "
                             f"the nearest decade with them")
            authors.add(c["author"])
            s = {"language": marc, "wordlist": wordlist, "source": "wikisource", "wiki": c["wiki"],
                 "title": title, "revid": revid, "url": f"https://{c['wiki']}/w/index.php?oldid={revid}",
                 "work": c["work"], "author": c["author"], "year": c["year"],
                 "year_source": c.get("year_source", "Wikidata (P577)"), "decade": decade,
                 "median_decade": median, "target_decade": target}
            if notes:
                s["decade_note"] = "; ".join(notes)
            s["copyright"] = f"published in {c['year']}, before {LAST_PD_YEAR + 1}: public domain in the US"
            s.update(words=n, text=text)
            out.append(s)
    return out


def read_fixture(path: Path) -> list[dict]:
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]


def write_fixture(path: Path, samples: list[dict]) -> None:
    order = {m: i for i, (m, *_rest) in enumerate(LANGUAGES)}
    samples.sort(key=lambda s: (order[s["language"]], s["source"], s["wiki"], s["title"]))
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8") as f:
        for s in samples:
            f.write(json.dumps(s, ensure_ascii=False, sort_keys=False) + "\n")


README = OUT.parent / "README.md"
LIST_START = "<!-- The list below is written by scripts/page-language-samples.py. -->"
LIST_END = "<!-- End of the list. -->"


def write_readme_list(path: Path, samples: list[dict]) -> None:
    """The README's list of Wikisource texts (author, title, year, wiki), between its markers."""
    if not path.exists():
        return
    lines = [LIST_START, ""]
    for s in samples:
        if s["source"] == "wikisource":
            lines.append(f"- `{s['language']}`: {s['title']}, by {s['author']}, {s['year']} ({s['wiki']})")
    lines += ["", LIST_END]
    text = path.read_text(encoding="utf-8")
    start, end = text.find(LIST_START), text.find(LIST_END)
    if start < 0 or end < 0:
        print(f"{path}: no list markers; not updated", file=sys.stderr)
        return
    path.write_text(text[:start] + "\n".join(lines) + text[end + len(LIST_END):], encoding="utf-8")


def check(samples: list[dict], a) -> int:
    bad = 0
    for s in samples:
        title, revid, page_html = parse(s["wiki"], a.pause, oldid=s["revid"])
        text, n = prose(page_html, a.words)
        ok = (title, revid, text) == (s["title"], s["revid"], s["text"])
        print(f"{'ok' if ok else 'DIFFERS'}: {s['language']} {s['wiki']} {s['title']} {s['revid']}", file=sys.stderr)
        bad += not ok
    print(f"{len(samples) - bad} of {len(samples)} samples unchanged", file=sys.stderr)
    return 1 if bad else 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--out", type=Path, default=OUT, help="the fixture (default: %(default)s)")
    ap.add_argument("--languages", help="MARC codes, comma-separated, to regenerate (default: every one)")
    ap.add_argument("--check", action="store_true", help="read each sample's revision again and compare")
    ap.add_argument("--quality-rows", type=Path, default=QUALITY_ROWS,
                    help="the OCR audit's table rows, for the median decades (default: %(default)s)")
    ap.add_argument("--min-pages", type=int, default=50,
                    help="a language with fewer pages there takes the corpus's median decade")
    ap.add_argument("--words", type=int, default=300, help="about this many words of each text")
    ap.add_argument("--per-source", type=int, default=3, help="Wikipedia articles, and Wikisource texts, a language")
    ap.add_argument("--pause", type=float, default=1.0, help="seconds between requests")
    a = ap.parse_args()

    covered = sorted({w for _, w, _, _ in LANGUAGES})
    if covered != sorted(word_lists()):
        print(f"LANGUAGES covers {covered}, the word lists are {sorted(word_lists())}", file=sys.stderr)
        return 1
    existing = read_fixture(a.out)
    if a.check:
        return check(existing, a)

    by_marc, pages, corpus = medians(a.quality_rows, a.min_pages)
    wanted = set(a.languages.split(",")) if a.languages else {m for m, *_ in LANGUAGES}
    unknown = wanted - {m for m, *_ in LANGUAGES}
    if unknown:
        print(f"no word list for {sorted(unknown)}", file=sys.stderr)
        return 1
    kept = [s for s in existing if s["language"] not in wanted]
    expect = {(s["wiki"], s["title"], s["revid"]): s["expect"] for s in existing if "expect" in s}
    todo = [entry for entry in LANGUAGES if entry[0] in wanted]
    titles = wikipedia_titles(sorted({w for _, _, w, _ in todo}), a.pause)
    new: list[dict] = []
    for marc, wordlist, wiki_code, qid in todo:
        median = by_marc.get(marc, corpus)
        print(f"{marc} ({wordlist}): median decade {median}s ({pages.get(marc, 0)} pages)", file=sys.stderr)
        got = wikipedia_samples(marc, wordlist, wiki_code, titles.get(wiki_code, []), a)
        got += wikisource_samples(marc, wordlist, wiki_code, qid, median, a)
        for s in got:
            if (k := (s["wiki"], s["title"], s["revid"])) in expect:
                s["expect"] = expect[k]
        new += got
        for source in ("wikipedia", "wikisource"):
            n = sum(s["source"] == source for s in got)
            if n < a.per_source:
                print(f"  {marc}: only {n} {source} samples", file=sys.stderr)
        # Write as we go: a long run keeps what it has read.
        write_fixture(a.out, kept + new)
    if a.out == OUT:
        write_readme_list(README, kept + new)
    print(f"{a.out}: {len(kept) + len(new)} samples", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
