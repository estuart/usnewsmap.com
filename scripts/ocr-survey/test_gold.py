"""scripts/ocr-survey/gold.py: selection, validation and CER (#296)."""

import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import gold  # noqa: E402


def pages(n: int, group: str = "g") -> list[dict]:
    out = []
    for i in range(n):
        pid = f"sn{group}_{1850 + i * 7}-01-01_ed1_seq{i + 1}"
        out.append({**gold.parse_page_id(pid), "page_id": pid, "group": group, "lang": "eng"})
    return out


class Select(unittest.TestCase):
    def test_quotas_share_equally_and_pass_on_the_rest(self):
        self.assertEqual(gold.quotas({"a": 15, "b": 15, "c": 5}, 30), {"a": 13, "b": 12, "c": 5})
        self.assertEqual(gold.quotas({"a": 2, "b": 2}, 10), {"a": 2, "b": 2})
        self.assertEqual(sum(gold.quotas({"a": 9, "b": 9, "c": 9}, 20).values()), 20)

    def test_seeded_and_one_crop_per_page(self):
        ps = pages(20) + pages(20, "h")
        a, b = gold.select(ps, 12, 1), gold.select(ps, 12, 1)
        self.assertEqual(a, b)
        self.assertEqual(len(a), 12)
        self.assertEqual(len({p["page_id"] for p in a}), 12)
        self.assertNotEqual([p["page_id"] for p in a], [p["page_id"] for p in gold.select(ps, 12, 2)])

    def test_picks_cover_the_eras(self):
        picked = gold.select(pages(30), 6, 1)
        self.assertEqual(len({p["era"] for p in picked}), 6)


class Score(unittest.TestCase):
    def test_normalization(self):
        self.assertEqual(gold.normalize("Die Zei-\ntung  ist\nda"), "Die Zeitung ist da")
        self.assertEqual(gold.normalize("a [?b] c[?]d", truth=True), "a b cd")

    def test_cer_and_wer(self):
        r = gold.cer_wer("the cat sat", "the cot sat")
        self.assertEqual((r["ref_chars"], r["char_edits"], r["ref_words"], r["word_edits"]), (11, 1, 3, 1))
        self.assertEqual(gold.cer_wer("abc", "")["char_edits"], 3)


class Validate(unittest.TestCase):
    def row(self, **kw):
        base = {"crop_id": "g-01", "transcription": "line one\nline two", "transcriber": "A", "reviewer": "B",
                "hints": {"texts": {"eng": "line wun\nline too"}}}
        return {**base, **kw}

    def test_complete_row_passes(self):
        self.assertEqual(gold.validate_rows([self.row()])[0], [])

    def test_empty_transcription_fails(self):
        self.assertTrue(gold.validate_rows([self.row(transcription=None)])[0])

    def test_hint_copy_fails_unless_reviewed(self):
        r = self.row(transcription="line wun line too")
        self.assertTrue(gold.validate_rows([r])[0])
        self.assertEqual(gold.validate_rows([{**r, "hint_copy_reviewed": True}])[0], [])

    def test_review_by_a_second_person(self):
        self.assertTrue(gold.validate_rows([self.row(reviewer=None)])[0])
        self.assertTrue(gold.validate_rows([self.row(reviewer="A")])[0])
        self.assertEqual(gold.validate_rows([self.row(reviewer=None)], require_review=False)[0], [])

    def test_unknown_share_and_markers(self):
        self.assertTrue(gold.validate_rows([self.row(transcription="a[?]" * 10)])[0])
        self.assertTrue(gold.validate_rows([self.row(transcription="abc [?de")])[0])

    def test_multi_character_guess_counts_each_character(self):
        self.assertAlmostEqual(gold.unknown_share("[?abc]" + "x" * 97), 0.03)
        self.assertTrue(gold.validate_rows([self.row(transcription="[?abc]" + "x" * 97)])[0])

    def test_image_must_match_manifest(self):
        m = [{"crop_id": "g-01", "image": "images/g-01.jpg", "crop_sha256": ""}]
        self.assertEqual(gold.validate_rows([self.row(image="images/g-01.jpg")], manifest=m)[0], [])
        self.assertTrue(gold.validate_rows([self.row(image="images/g-02.jpg")], manifest=m)[0])

    def test_skipped_span_marker_fails(self):
        self.assertTrue(gold.validate_rows([self.row(transcription="abc\n[...]\ndef")])[0])


class Agree(unittest.TestCase):
    def test_pairwise_and_mean(self):
        rows = [{"crop_id": "g-01", "group": "g", "hints": {"texts": {"a": "the cat", "b": "the cat", "c": "the cot"}}}]
        pairs, summary = gold.agreement(rows)
        self.assertEqual(len(pairs), 6)
        by = {s["engine"]: s["disagreement_cer"] for s in summary if s["group"] == "all"}
        self.assertEqual(by["a"], by["b"])
        self.assertGreater(by["c"], by["a"])

    def test_no_texts_no_rows(self):
        self.assertEqual(gold.agreement([{"crop_id": "g-01", "group": "g", "hints": {"texts": {"a": "x"}}}]), ([], []))


COCO = {"images": [{"id": 0, "file_name": "p1.png", "width": 40, "height": 30}],
        "categories": [{"id": 1, "name": "article"}],
        "annotations": [
            {"id": 7, "image_id": 0, "category_id": 1, "bbox": [2, 3, 20, 10], "text": "Mr. Fitz-\nwalter spoke.\n"},
            {"id": 8, "image_id": 0, "category_id": 0, "bbox": [0, 0, 5, 5], "text": "HEADLINE"},
            {"id": 9, "image_id": 0, "category_id": 1, "bbox": [0, 0, 5, 5], "text": "  "}]}

PAGE = """<?xml version="1.0"?>
<PcGts xmlns="http://schema.primaresearch.org/PAGE/gts/pagecontent/2013-07-15">
 <Page imageFilename="p1.png" imageWidth="40" imageHeight="30">
  <TextRegion id="r1"><Coords points="2,3 22,3 22,13 2,13"/>
   <TextLine id="l1"><TextEquiv><Unicode>Die Zei-</Unicode></TextEquiv></TextLine>
   <TextLine id="l2"><TextEquiv><Unicode> tung ſagt </Unicode></TextEquiv></TextLine>
   <TextEquiv><Unicode>region text, not used</Unicode></TextEquiv></TextRegion>
  <TextRegion id="r2"><Coords points="0,0 4,0 4,4 0,4"/></TextRegion>
  <ImageRegion id="r3"/>
 </Page></PcGts>"""

NDL = {"boxes": [{"box_id": 1, "text": "五百三十八", "orientation": "vertical",
                  "quad": {"x1": 2, "y1": 3, "x2": 6, "y2": 3, "x3": 6, "y3": 20, "x4": 2, "y4": 20}},
                 {"box_id": 2, "text": "", "quad": {"x1": 0, "y1": 0, "x2": 1, "y2": 0, "x3": 1, "y3": 1, "x4": 0, "y4": 1}}],
       "dontcare": []}


class PublicParsers(unittest.TestCase):
    def test_coco_keeps_articles_with_text(self):
        with tempfile.TemporaryDirectory() as d:
            f = Path(d) / "g.json"
            f.write_text(json.dumps(COCO))
            r = gold.parse_coco_gold(f)
        self.assertEqual(len(r), 1)
        self.assertEqual((r[0]["page"], r[0]["box"], r[0]["text"]), ("p1.png", [2, 3, 22, 13], "Mr. Fitz-\nwalter spoke."))

    def test_page_xml_uses_lines_and_skips_empty_regions(self):
        with tempfile.TemporaryDirectory() as d:
            f = Path(d) / "p.xml"
            f.write_text(PAGE)
            name, regs = gold.parse_page_xml(f)
        self.assertEqual(name, "p1.png")
        self.assertEqual(len(regs), 1)
        self.assertEqual((regs[0]["box"], regs[0]["text"]), ([2, 3, 22, 13], "Die Zei-\ntung ſagt"))

    def test_ndl_json(self):
        with tempfile.TemporaryDirectory() as d:
            f = Path(d) / "x.json"
            f.write_text(json.dumps(NDL))
            r = gold.parse_ndl_json(f)
        self.assertEqual([(x["text"], x["box"], x["orientation"]) for x in r], [("五百三十八", [2, 3, 6, 20], "vertical")])

    def test_truth_joins_hyphenated_lines_for_scoring(self):
        self.assertEqual(gold.cer_wer("Die Zei-\ntung ſagt", "Die Zeitung ſagt")["char_edits"], 0)


def regions(n_pages: int, per: int) -> list[dict]:
    return [{"page_id": f"d-{p}", "region": f"r{r}", "text": "x" * (30 + r), "box": [0, 0, 1, 1], "image": Path("i")}
            for p in range(n_pages) for r in range(per)]


class PublicPick(unittest.TestCase):
    def test_seeded_round_robin_over_pages(self):
        a = gold.pick_public(regions(5, 4), 7, 2, 1, 20, 100)
        self.assertEqual(a, gold.pick_public(regions(5, 4), 7, 2, 1, 20, 100))
        self.assertEqual(len(a), 7)
        self.assertEqual(len({r["page_id"] for r in a[:5]}), 5)  # one from each page before any second one
        self.assertNotEqual([r["page_id"] for r in a], [r["page_id"] for r in gold.pick_public(regions(5, 4), 7, 2, 2, 20, 100)])

    def test_per_page_and_length_limits(self):
        self.assertEqual(len(gold.pick_public(regions(2, 6), 99, 3, 1, 20, 100)), 6)
        self.assertEqual(gold.pick_public(regions(2, 3), 99, 3, 1, 500, 900), [])

    def test_rows_validate(self):
        p = {"page_id": "d-0", "region": "r0", "text": "Die Zei-\ntung", "box": [0, 0, 1, 1], "image": Path("p.jpg")}
        m, t = gold.public_rows("reichsanzeiger", p, 1, b"jpeg", [0, 0, 1, 1], "sha", "Pillow x")
        self.assertEqual(m["crop_id"], t["crop_id"])
        self.assertEqual(gold.validate_rows([t], [m])[0], [])

    def test_convert_refuses_off_policy_source(self):
        self.assertEqual(gold.main(["public-convert", "--dataset", "ndl-pdm", "--src", "/nonexistent-gold-test",
                                    "--out", "/nonexistent-gold-test/out"]), 2)

    def test_fetch_needs_the_download_flag(self):
        self.assertEqual(gold.main(["public-fetch", "--dataset", "austrian", "--src", "/nonexistent-gold-test"]), 2)


try:
    import PIL  # noqa: F401
except ImportError:
    PIL = None


@unittest.skipUnless(PIL, "Pillow is not installed")
class PublicConvert(unittest.TestCase):
    def test_no_regions_fails(self):
        with tempfile.TemporaryDirectory() as d:
            self.assertEqual(gold.main(["public-convert", "--dataset", "americanstories", "--src", d, "--not-for-scoring",
                                        "--out", str(Path(d) / "out")]), 1)

    def test_americanstories_and_page_xml_end_to_end(self):
        from PIL import Image

        for dataset in ("americanstories", "reichsanzeiger"):
            with tempfile.TemporaryDirectory() as d:
                src, out = Path(d) / "src", Path(d) / "out"
                src.mkdir()
                Image.new("L", (40, 30), 200).save(src / "p1.png")
                if dataset == "americanstories":
                    (src / "gold_hand_transcription.json").write_text(json.dumps(COCO))
                else:
                    (src / "p1.xml").write_text(PAGE)
                self.assertEqual(gold.main(["public-convert", "--dataset", dataset, "--src", str(src), "--out", str(out),
                                            "--min-chars", "5", "--not-for-scoring"]), 0)
                man = gold.read_jsonl(out / "manifest.jsonl")
                self.assertEqual(len(man), 1)
                self.assertEqual(man[0]["box"], [2, 3, 22, 13])
                self.assertTrue((out / man[0]["image"]).exists())
                rows = gold.read_jsonl(out / "crops.jsonl")
                self.assertEqual(gold.validate_rows(rows, man, out / "images"), ([], []))
                self.assertEqual(rows[0]["truth_source"], dataset)
                perfect = gold.cer_wer(rows[0]["transcription"], rows[0]["transcription"].replace("-\n", ""))
                self.assertEqual(perfect["char_edits"], 0)


if __name__ == "__main__":
    unittest.main()
