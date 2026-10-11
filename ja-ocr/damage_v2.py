"""The OCR damage metric, version 2: a page's language and how damaged LoC's OCR of it is.

This is the code of `quality.py --metric v2` that scores one page, moved out of quality.py so the Rust
port (usnm_core::damage, 12 section 12.5 Step 5) is checked against this file alone: the parity fixture
records its sha256 (damage_parity.py). quality.py imports it and its docstring describes the metric.
A change here changes the metric: bump the port and the fixture with it.
"""

from __future__ import annotations

import functools
import os
import re
import unicodedata
from dataclasses import dataclass


# The catalog's MARC codes to wordfreq's (ISO 639-1, "sh" for Serbo-Croatian).
# Only languages wordfreq 3.1 has a list for: it would otherwise fall back to
# the "nearest" one (Yiddish to English, Latin to Italian). Chinese and Korean
# are left out: their lists hold segmented words, and LoC's OCR of those
# pages is Latin letters anyway.
WORDFREQ = {
    "ara": "ar", "bul": "bg", "cze": "cs", "dan": "da", "dut": "nl", "eng": "en", "fin": "fi",
    "fre": "fr", "ger": "de", "gre": "el", "heb": "he", "hrv": "sh", "hun": "hu", "ice": "is",
    "ita": "it", "lav": "lv", "lit": "lt", "nor": "nb", "pol": "pl", "por": "pt", "rum": "ro",
    "rus": "ru", "slo": "sk", "slv": "sl", "spa": "es", "srp": "sh", "swe": "sv", "tgl": "fil",
    "ukr": "uk",
}
# v2 only: languages wordfreq has no list for, with a top-5,000 list of our
# own in wordlists/<code>.txt, built from open corpora (wordlists/README.md).
LOCAL_WORDLISTS = {"haw": "haw", "yid": "yi"}
WORDLIST_DIR = os.path.join(os.path.dirname(os.path.abspath(__file__)), "wordlists")
# Serbian pages in Cyrillic are scored by the Latin-script list, letter for
# letter (Vuk's Cyrillic and Gaj's Latin match one to one).
SERBIAN_LATIN = str.maketrans({
    "а": "a", "б": "b", "в": "v", "г": "g", "д": "d", "ђ": "đ", "е": "e", "ж": "ž", "з": "z", "и": "i",
    "ј": "j", "к": "k", "л": "l", "љ": "lj", "м": "m", "н": "n", "њ": "nj", "о": "o", "п": "p", "р": "r",
    "с": "s", "т": "t", "ћ": "ć", "у": "u", "ф": "f", "х": "h", "ц": "c", "ч": "č", "џ": "dž", "ш": "š",
})
# Yiddish ligatures, as their two letters: OCR and typesetting use either.
LIGATURES = str.maketrans({"\u05f0": "\u05d5\u05d5", "\u05f1": "\u05d5\u05d9", "\u05f2": "\u05d9\u05d9"})
# Languages with words of no vowel (Czech "smrt", Croatian "prst"): the
# no-vowel rule would flag real words.
SYLLABIC_CONSONANTS = {"cze", "slo", "hrv", "srp", "slv"}
UNKNOWN = "und"

CORE = re.compile(r"[^\W_](?:.*[^\W_])?", re.S)  # first to last letter or digit
TRIPLE = re.compile(r"([^\W\d_])\1\1")
ROMAN = re.compile(r"^(?=[mdclxvi]+$)m{0,4}(cm|cd|d?c{0,3})(xc|xl|l?x{0,3})(ix|iv|v?i{0,3})$")
VOWELS = frozenset("aeiouyàáâãäåæèéêëìíîïòóôõöøùúûüýÿāăąēĕėęěīĭįıōŏőœūŭůűų")


def latin(word: str) -> bool:
    return all(ord(c) < 0x250 or 0x1E00 <= ord(c) <= 0x1EFF for c in word)


def mixed_case(word: str) -> bool:
    """A capital inside a lower-case word: "tHe", "ThE", "HeLLoS"; not "McKay", "MacDonald", "USA"."""
    if word.islower() or word.isupper() or len(word) < 2:
        return False
    if word[0].islower():
        return True
    rises = sum(1 for a, b in zip(word, word[1:]) if a.islower() and b.isupper())
    return rises >= 2 or (rises == 1 and word[-1].isupper() and word[-2].islower())


def garbage(token: str, core: str | None, lang: str) -> bool:
    """rmgarbage-style rules (Taghva, Nartker, Condit and Borsack 2001), on a whitespace token."""
    if len(token) > 40:
        return True
    if len(token) >= 2 and sum(1 for c in token if not c.isalnum()) * 2 > len(token):
        return True
    if not core:
        return False
    folded = core.casefold()
    if TRIPLE.search(folded) and not ROMAN.match(folded):
        return True
    if core.isalpha():
        if len(core) >= 4 and latin(core):
            vowels = sum(1 for c in folded if c in VOWELS)
            if vowels == len(folded) or (vowels == 0 and lang not in SYLLABIC_CONSONANTS):
                return True
        if mixed_case(core):
            return True
    return False


# ---------------------------------------------------------------- v2: the page's language and OCR damage

TOP_FUNCTION, TOP_HEAD, TOP_REAL = 100, 20, 5000
MIN_WORDS = 20  # a page with fewer word tokens than this: und
MIN_SHARE = 0.10  # the winner's function-word share must reach this,
PAGE_TYPES, WINDOW_TYPES = 10, 5  # with this many different function words (a page, a window),
LEAD = 2.0  # and this many times the runner-up's tokens among the function words only one of them has
WINDOW = 50  # word tokens per window for bilingual pages, on pages of MIXED_MIN_WORDS or more
MIXED_MIN_WORDS = 4 * WINDOW
MIN_TAIL = WINDOW // 2  # a page's last, shorter window counts only with this many words
MIXED_SHARE = 0.2  # a second language winning this share of decided windows (and 2 or more) makes a page mixed
DAMAGED, BADLY_DAMAGED = 0.1, 0.25
ENGLISH = "eng"
DETECTED, MIXED, NO_TEXT = "detected", "mixed", "no_text"
LATIN_LETTERS = "abcdefghijklmnopqrstuvwxyz"
# Close languages whose words (in the top 5,000) are real words of each other's pages: Dano-Norwegian
# was the written Norwegian of the period; Czech and Slovak.
RELATED = {"nb": ("da",), "da": ("nb",), "cs": ("sk",), "sk": ("cs",)}
AA_SPELLING = {"da", "nb"}
THIN_LETTERS = "cfijlnrtv"  # what a speck or a split letter reads as: inserted, or two for one ("li" for "h")

# Scripts other than Latin, by code point, and the catalog languages written in
# each. Serbian is either. Every other language is taken to be in Latin script.
SCRIPT_RANGES = (
    ("hebrew", 0x0590, 0x05FF), ("hebrew", 0xFB1D, 0xFB4F), ("cyrillic", 0x0400, 0x052F),
    ("greek", 0x0370, 0x03FF), ("greek", 0x1F00, 0x1FFF), ("arabic", 0x0600, 0x06FF),
    ("arabic", 0x0750, 0x077F), ("arabic", 0xFB50, 0xFDFF), ("arabic", 0xFE70, 0xFEFF),
    ("cherokee", 0x13A0, 0x13FF), ("cherokee", 0xAB70, 0xABBF), ("cjk", 0x3040, 0x30FF),
    ("cjk", 0x3400, 0x9FFF), ("cjk", 0xAC00, 0xD7AF),
)
LANGUAGE_SCRIPTS = {
    "yid": ("hebrew",), "heb": ("hebrew",), "lad": ("hebrew",), "rus": ("cyrillic",), "ukr": ("cyrillic",),
    "bul": ("cyrillic",), "bel": ("cyrillic",), "mac": ("cyrillic",), "srp": ("latin", "cyrillic"),
    "gre": ("greek",), "ara": ("arabic",), "per": ("arabic",), "chr": ("cherokee",), "chi": ("cjk",),
    "kor": ("cjk",), "jpn": ("cjk",),
}


def script(word: str) -> str:
    o = ord(word[0])
    if o < 0x250 or 0x1E00 <= o <= 0x1EFF:
        return "latin"
    for name, lo, hi in SCRIPT_RANGES:
        if lo <= o <= hi:
            return name
    return "other"


def scripts_of(lang: str) -> tuple:
    return LANGUAGE_SCRIPTS.get(lang, ("latin",))


@dataclass(frozen=True)
class LangModel:
    """A language's function words (top 100), its top 20, and their OCR near-misses that aren't real words."""

    code: str
    top: frozenset
    head: tuple
    near: dict  # near-miss -> the top-20 word it is one edit from
    real: frozenset  # the top 5,000: real words, never near-misses
    script: str = "latin"  # of its words


def one_edit(word: str, letters: str, inserted: str, split: str):
    """Every string one OCR edit from `word`, in this order: a letter substituted (any of `letters`), deleted,
    inserted (one of `inserted`), or read as two (two of `split`: "h" as "li", "m" as "rn")."""
    for kind in range(4):
        yield from _edits(word, kind, letters, inserted, split)


def _edits(word: str, kind: int, letters: str, inserted: str, split: str):
    for i, ch in enumerate(word):
        if kind == 0:
            for c in letters:
                if c != ch:
                    yield word[:i] + c + word[i + 1:]
        elif kind == 1:
            yield word[:i] + word[i + 1:]
        elif kind == 3:
            for a in split:
                for b in split:
                    yield word[:i] + a + b + word[i + 1:]
    if kind == 2:
        for i in range(len(word) + 1):
            for c in inserted:
                yield word[:i] + c + word[i:]


def build_model(code: str, ranked: list[str], also_real=()) -> LangModel:
    """A LangModel from a language's most frequent words, most frequent first (wordfreq's top 5,000).
    `also_real`: more words that aren't near-misses (a closely related language's)."""
    real = set(ranked[:TOP_REAL]) | set(also_real)
    words = [w for w in ranked if len(w) >= 2 and w.isalpha()]
    top, head = words[:TOP_FUNCTION], words[:TOP_HEAD]
    if code in AA_SPELLING:  # "paa" for "på", as Danish and Norwegian were printed until the 1900s
        top += [w.replace("å", "aa") for w in top if "å" in w]
        head += [w.replace("å", "aa") for w in head if "å" in w]
        real |= {w.replace("å", "aa") for w in real if "å" in w}
    real, head = frozenset(real), tuple(head)
    own = {c for w in top for c in w}
    if all(latin(c) for c in own):
        letters, inserted, split = "".join(sorted(own | set(LATIN_LETTERS))), THIN_LETTERS, THIN_LETTERS
    else:
        letters = inserted = "".join(sorted(own))
        split = ""
    near: dict[str, str] = {}
    for kind in range(4):  # a near-miss is credited to the word it is the simplest edit of
        for base in head:
            for v in _edits(base, kind, letters, inserted, split):
                if len(v) >= 2 and v not in real and v not in near:
                    near[v] = base
    return LangModel(code=code, top=frozenset(top), head=head, near=near, real=real, script=script(top[0]))


def wordfreq_ranked(code: str) -> list[str] | None:
    """wordfreq's top 5,000 words for a language, most frequent first, or None if it has no list of its own;
    for LOCAL_WORDLISTS, ours."""
    import wordfreq

    if code in LOCAL_WORDLISTS.values():
        with open(os.path.join(WORDLIST_DIR, f"{code}.txt"), encoding="utf-8") as f:
            return [w for w in f.read().split("\n") if w][:TOP_REAL]
    if code not in wordfreq.available_languages("best"):
        return None
    words = wordfreq.top_n_list(code, TOP_REAL)
    wordfreq.top_n_list.cache_clear()
    wordfreq.get_frequency_list.cache_clear()
    return words


class Models:
    """LangModels by catalog language, each built once. `loader` (wordfreq code -> ranked words) is replaceable."""

    def __init__(self, loader=None):
        self.loader = loader or wordfreq_ranked
        self.by_code: dict[str, LangModel | None] = {}

    def get(self, lang: str) -> LangModel | None:
        code = WORDFREQ.get(lang) or LOCAL_WORDLISTS.get(lang)
        if code is None:
            return None
        if code not in self.by_code:
            ranked = self.loader(code)
            also = [w for c in RELATED.get(code, ()) for w in (self.loader(c) or ())[:TOP_REAL]]
            self.by_code[code] = build_model(code, ranked, also) if ranked else None
        return self.by_code[code]


@dataclass
class Damage:
    words: int = 0
    function: int = 0  # tokens in the top 100
    head: int = 0  # tokens in the top 20
    near: int = 0  # near-misses of the top 20

    def add(self, model: LangModel, words, others: tuple = ()) -> None:
        """Count `words` against `model`; a near-miss that is a real word in one of `others` doesn't count."""
        for w in words:
            self.words += 1
            if w in model.top:
                self.function += 1
                if w in model.head:
                    self.head += 1
            elif w in model.near and not any(w in o.real for o in others):
                self.near += 1

    def merge(self, other: "Damage") -> None:
        self.words += other.words
        self.function += other.function
        self.head += other.head
        self.near += other.near

    def function_share(self) -> float | None:
        return self.function / self.words if self.words else None

    def damage_rate(self) -> float | None:
        n = self.near + self.head
        return self.near / n if n else None


def damage(model: LangModel, words, others: tuple = ()) -> Damage:
    d = Damage()
    d.add(model, words, others)
    return d


def counted(words) -> dict[str, int]:
    counts: dict[str, int] = {}
    for w in words:
        counts[w] = counts.get(w, 0) + 1
    return counts


@dataclass
class Fit:
    lang: str
    model: LangModel
    share: float  # of the word tokens, the share in the language's top 100
    types: int  # distinct top-100 words seen


def fits(cands: list[tuple[str, LangModel]], counts: dict[str, int], n: int) -> list[Fit]:
    out = []
    for lang, m in cands:
        hits = types = 0
        for w, c in counts.items():
            if w in m.top:
                hits += c
                types += 1
        out.append(Fit(lang, m, hits / n if n else 0.0, types))
    out.sort(key=lambda f: (-f.share, f.lang))
    return out


def winner(fs: list[Fit], counts: dict[str, int], min_types: int) -> str | None:
    """The best-fitting language if it clearly wins: enough function words, of enough kinds, and at least
    LEAD times the runner-up's tokens among the function words only one of the two has."""
    if not fs or fs[0].share < MIN_SHARE or fs[0].types < min_types:
        return None
    if len(fs) > 1:
        a, b = fs[0].model.top, fs[1].model.top
        only_a = sum(c for w, c in counts.items() if w in a and w not in b)
        only_b = sum(c for w, c in counts.items() if w in b and w not in a)
        if only_a == 0 or only_a < LEAD * only_b:
            return None
    return fs[0].lang


@dataclass
class Language:
    lang: str  # a catalog language code, or und
    decision: str  # detected, mixed or und
    share: float | None = None  # the winner's function-word share
    runner_up: str | None = None
    runner_up_share: float | None = None
    mixed_with: str | None = None
    script: str = "latin"
    damage: Damage | None = None  # against the detected language(s); None if it has no word list


def _r(x: float | None) -> float | None:
    return None if x is None else round(x, 4)


def detect(words: list[str], languages: tuple, models: Models) -> Language:
    """The page's language among its title's catalog languages (plus English), and its damage against it.

    `words` are the page's word tokens (letters only, at least 2, case-folded), in order. The script most
    of them are in picks the path; a title's language in another script that covers MIXED_SHARE of the
    words makes the page mixed.
    """
    if len(words) < MIN_WORDS:
        return Language(UNKNOWN, UNKNOWN)
    by_script: dict[str, list[str]] = {}
    for w in words:
        by_script.setdefault(script(w), []).append(w)
    main = max(by_script, key=lambda s: (len(by_script[s]), s == "latin", s))
    out = script_language(main, by_script[main], languages, models, PAGE_TYPES)
    if out.decision != DETECTED:
        return out
    # The 20% share is the only size test for a second script's words (10 words of a 50-word page);
    # they need as many different function words as a window does.
    for s, sw in sorted(by_script.items(), key=lambda kv: (-len(kv[1]), kv[0])):
        if s != main and len(sw) >= MIXED_SHARE * len(words):
            other = script_language(s, sw, languages, models, WINDOW_TYPES)
            if other.decision == DETECTED and other.lang != out.lang:
                out.decision, out.mixed_with = MIXED, other.lang
                if other.damage is not None:
                    if out.damage is None:
                        out.damage = Damage()
                    out.damage.merge(other.damage)
                break
    return out


def script_language(s: str, words: list[str], languages: tuple, models: Models, min_types: int) -> Language:
    """The language of a page's words in one script. The caller has checked there are enough of them."""
    if s == "latin":
        return latin_language(words, languages, models, min_types)
    return other_script_language(s, words, languages, models, min_types)


def windows_of(words: list[str]) -> list[list[str]]:
    """A page of MIXED_MIN_WORDS or more cut into WINDOW-word windows; a last, shorter one only if it has
    MIN_TAIL words. A shorter page: none."""
    if len(words) < MIXED_MIN_WORDS:
        return []
    out = [words[i:i + WINDOW] for i in range(0, len(words), WINDOW)]
    return out if len(out[-1]) >= MIN_TAIL else out[:-1]


def latin_language(words: list[str], languages: tuple, models: Models, min_types: int = PAGE_TYPES) -> Language:
    """Latin-script words: the title's languages with word lists, and English, compete on function words."""
    if not words:
        return Language(UNKNOWN, UNKNOWN)
    cands, seen = [], set()
    for lang in [*languages, ENGLISH]:
        m = models.get(lang) if "latin" in scripts_of(lang) else None
        if m is not None and m.script == "latin" and m.code not in seen:
            seen.add(m.code)
            cands.append((lang, m))
    counts = counted(words)
    fs = fits(cands, counts, len(words))
    out = Language(UNKNOWN, UNKNOWN, share=_r(fs[0].share) if fs else None,
                   runner_up=fs[1].lang if len(fs) > 1 else None,
                   runner_up_share=_r(fs[1].share) if len(fs) > 1 else None)
    model_of = dict(cands)

    def others(lang):
        return tuple(m for x, m in cands if x != lang)

    # Windows of the page, each given a language if one clearly wins in it: two languages each winning a
    # fair share of the windows make the page mixed. Each window is then scored against its own language,
    # and windows no language wins (garbled, too few function words) aren't scored.
    windows = windows_of(words)
    if windows:
        winners = []
        for win in windows:
            c = counted(win)
            winners.append(winner(fits(cands, c, len(win)), c, WINDOW_TYPES))
        votes = counted(w for w in winners if w)
        decided = sum(votes.values())
        strong = sorted((lang for lang, v in votes.items() if v >= 2 and v >= MIXED_SHARE * decided),
                        key=lambda lang: (-votes[lang], lang))
        if len(strong) >= 2:
            a, b = strong[0], strong[1]
            out.lang, out.decision, out.mixed_with = a, MIXED, b
            out.share = _r(next(f.share for f in fs if f.lang == a))
            out.runner_up, out.runner_up_share = b, _r(next(f.share for f in fs if f.lang == b))
            d = Damage()
            for win, w in zip(windows, winners):
                if w:
                    d.add(model_of[w], win, others(w))
            out.damage = d
            return out
    best = winner(fs, counts, min_types)
    if best is not None:
        out.lang, out.decision = best, DETECTED
        out.damage = damage(model_of[best], words, others(best))
    return out


def other_script_language(s: str, words: list[str], languages: tuple, models: Models,
                          min_types: int = PAGE_TYPES) -> Language:
    """Words in a script other than Latin: the title's language in that script; if it lists several, the
    one with a word list that clearly fits best, else the one without a list; else und."""
    langs = [lang for lang in languages if s in scripts_of(lang)]
    if not langs or not words:
        return Language(UNKNOWN, UNKNOWN, script=s)
    # Serbian in Cyrillic is scored by its Latin-script list on the words transliterated; the transliterated
    # words join the counts beside the Cyrillic ones (the two never share a word), so Serbian competes with
    # the title's other Cyrillic languages on equal terms.
    serbian = [w.translate(SERBIAN_LATIN) for w in words] if s == "cyrillic" and "srp" in langs else None
    modelled = [(lang, m) for lang in langs if (m := models.get(lang)) is not None
                and (m.script == s or (serbian is not None and lang == "srp"))]
    counts = counted(words)
    if serbian is not None:
        for w, c in counted(serbian).items():
            counts[w] = counts.get(w, 0) + c
    fs = fits(modelled, counts, len(words))
    best = winner(fs, counts, min_types)
    if best is None:
        unmodelled = [lang for lang in langs if lang not in dict(modelled)]
        if len(langs) == 1:
            best = langs[0]
        elif len(unmodelled) == 1:
            best = unmodelled[0]
        else:
            return Language(UNKNOWN, UNKNOWN, script=s)
    share = next((f.share for f in fs if f.lang == best), None)
    rest = [f for f in fs if f.lang != best]
    m = dict(modelled).get(best)
    scored = serbian if best == "srp" and serbian is not None else words
    return Language(best, DETECTED, share=_r(share), runner_up=rest[0].lang if rest else None,
                    runner_up_share=_r(rest[0].share) if rest else None, script=s,
                    damage=damage(m, scored, tuple(f.model for f in rest)) if m else None)


DEHYPHENATE = re.compile(r"(\w)-\n\s*(?=\w)")


def text_words(text: str) -> tuple[int, list[str], int, int]:
    """(whitespace tokens, word tokens case-folded, tokens rmgarbage flags, of those the ones it flags in a
    language whose words can lack vowels). A word broken across lines with a hyphen is joined first."""
    tokens = DEHYPHENATE.sub(r"\1", text).split()
    words, flagged, flagged_syllabic = [], 0, 0
    for tok in tokens:
        word, g, g_syllabic = token_info(tok)
        flagged += g
        flagged_syllabic += g_syllabic
        if word is not None:
            words.append(word)
    return len(tokens), words, flagged, flagged_syllabic


@functools.lru_cache(maxsize=1 << 18)
def token_info(tok: str) -> tuple[str | None, bool, bool]:
    """(the token's word, case-folded, or None; rmgarbage flags it; it flags it in a language whose words
    can lack vowels). Cached: a page repeats most of its tokens, and pages repeat each other's."""
    m = CORE.search(tok)
    core = m.group(0) if m else None
    g = garbage(tok, core, ENGLISH)
    g_syllabic = g and garbage(tok, core, "cze")
    word = None
    if core:
        if not core.isalpha():  # points and accents as combining marks: "פֿאר", "e\u0301"
            core = "".join(c for c in unicodedata.normalize("NFC", core) if not unicodedata.combining(c))
        core = core.translate(LIGATURES)  # before the length check: "װ" is a two-letter word
        if len(core) >= 2 and core.isalpha():
            word = core.casefold()
    return word, g, g_syllabic


def score_v2(text: str, languages: tuple, models: Models) -> dict:
    """A page's language and scores: the fields scan_part adds to the page."""
    n_tokens, words, flagged, flagged_syllabic = text_words(text)
    lang = detect(words, languages, models)
    first = languages[0] if languages else UNKNOWN
    g_lang = lang.lang if lang.lang != UNKNOWN else first
    g = flagged_syllabic if g_lang in SYLLABIC_CONSONANTS else flagged
    d = lang.damage
    return {
        "lang": lang.lang, "decision": lang.decision, "share": lang.share, "runner_up": lang.runner_up,
        "runner_up_share": lang.runner_up_share, "mixed_with": lang.mixed_with, "script": lang.script,
        "function_share": d.function_share() if d else None, "damage_rate": d.damage_rate() if d else None,
        "garbage_share": g / n_tokens if n_tokens else None,
        "chars": len(text), "tokens": n_tokens, "words": len(words),
    }


def forced(text: str, lang: str, models: Models) -> dict | None:
    """A page's damage against one given language (a MARC code), with no detection: what the OCR engine
    survey's score.py computes. Serbian is transliterated to Latin letters; on a page not scored as English,
    a near-miss that is a real English word doesn't count. None if the language has no word list."""
    model = models.get(lang)
    if model is None:
        return None
    n_tokens, words, _, _ = text_words(text)
    if lang == "srp":
        words = [w.translate(SERBIAN_LATIN) for w in words]
    others = () if lang == ENGLISH else tuple(m for m in (models.get(ENGLISH),) if m is not None)
    d = damage(model, words, others)
    return {"tokens": n_tokens, "words": len(words), "head": d.head, "near": d.near, "function": d.function,
            "function_share": d.function_share(), "damage_rate": d.damage_rate(),
            "known_words": sum(1 for w in words if w in model.real)}
