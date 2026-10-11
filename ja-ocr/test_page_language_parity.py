"""The page-language detector's Rust port (usnm_core::page_language, #289) against this directory's Python:
python3 -m unittest discover ja-ocr (wordfreq, and pyarrow for zstd before Python 3.14).

- The function words the port embeds (crates/usnm-core/data/page-language-words.txt) are the ones
  damage_v2.Models builds: scripts/build-page-language-words.py renders the file again and it must match.
- The pages of the parity fixture (crates/usnm-core/tests/data/damage-parity.jsonl.zst) still get the
  language, decision, second language, script and shares recorded there, which the Rust test checks
  the port against (crates/usnm-core/tests/page_language_parity.rs).

Both need the repository around this directory (CI mounts it); without it, or without wordfreq, they skip.
"""

import importlib.util
import json
import os
import unittest

import damage_parity
import damage_v2

try:
    import wordfreq
except ImportError:  # the image has it
    wordfreq = None

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BUILDER = os.path.join(ROOT, "scripts", "build-page-language-words.py")
WORDS = os.path.join(ROOT, "crates", "usnm-core", "data", "page-language-words.txt")
FIXTURE = os.path.join(ROOT, "crates", "usnm-core", "tests", "data", "damage-parity.jsonl.zst")


def builder():
    spec = importlib.util.spec_from_file_location("build_page_language_words", BUILDER)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


@unittest.skipIf(wordfreq is None, "needs wordfreq")
@unittest.skipUnless(os.path.exists(BUILDER) and os.path.exists(WORDS), "needs the repository")
class WordList(unittest.TestCase):
    def test_committed_file_is_the_models_function_words(self):
        with open(WORDS, encoding="utf-8") as f:
            committed = f.read()
        self.assertEqual(builder().render(damage_v2), committed,
                         "run scripts/build-page-language-words.py")

    def test_one_line_per_list(self):
        lines = [line.split(" ") for line in builder().render(damage_v2).splitlines() if not line.startswith("#")]
        codes = sorted(set(damage_v2.WORDFREQ.values()) | set(damage_v2.LOCAL_WORDLISTS.values()))
        self.assertEqual([line[0] for line in lines], codes)
        models = damage_v2.Models()
        for code, script, *words in lines:
            lang = next(m for m, c in {**damage_v2.WORDFREQ, **damage_v2.LOCAL_WORDLISTS}.items() if c == code)
            self.assertEqual(script, models.get(lang).script, code)
            self.assertEqual(frozenset(words), models.get(lang).top, code)


def zstd_available() -> bool:
    try:
        damage_parity.zstd_decompress(damage_parity.zstd_compress(b"x"))
        return True
    except ImportError:
        return False


@unittest.skipIf(wordfreq is None, "needs wordfreq")
@unittest.skipUnless(os.path.exists(FIXTURE), "needs the repository")
@unittest.skipUnless(zstd_available(), "needs zstd (Python 3.14 or pyarrow)")
class Fixture(unittest.TestCase):
    def test_detect_gives_what_the_fixture_records(self):
        with open(FIXTURE, "rb") as f:
            lines = damage_parity.zstd_decompress(f.read()).decode().splitlines()
        models = damage_v2.Models()
        fields = ("lang", "decision", "mixed_with", "script", "share", "runner_up", "runner_up_share")
        pages = [json.loads(line) for line in lines[1:] if line.strip()]
        self.assertGreaterEqual(len(pages), 60)
        for p in pages:
            got = damage_v2.score_v2(p["text"], tuple(p["languages"]), models)
            self.assertEqual({k: got[k] for k in fields}, {k: p["score"][k] for k in fields}, p["doc_id"])


if __name__ == "__main__":
    unittest.main()
