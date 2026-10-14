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


# PAGE 2010 (Europeana): Point children, text on the region only.
PAGE_2010 = """<?xml version="1.0"?>
<PcGts xmlns="http://schema.primaresearch.org/PAGE/gts/pagecontent/2010-03-19">
 <Page imageFilename="p1.tif" imageWidth="40" imageHeight="30">
  <TextRegion id="r1" type="paragraph"><Coords><Point x="2" y="3"/><Point x="22" y="3"/><Point x="22" y="13"/>
   <Point x="2" y="13"/></Coords><TextEquiv><Unicode>Erſte Zeile
zweite</Unicode></TextEquiv></TextRegion>
  <TextRegion id="r2"><Coords><Point x="0" y="0"/><Point x="4" y="4"/></Coords></TextRegion>
 </Page></PcGts>"""


class PageXml2010(unittest.TestCase):
    def test_points_and_region_text(self):
        with tempfile.TemporaryDirectory() as d:
            f = Path(d) / "p.xml"
            f.write_text(PAGE_2010)
            name, regs = gold.parse_page_xml(f)
        self.assertEqual(name, "p1.tif")
        self.assertEqual([(r["box"], r["text"]) for r in regs], [([2, 3, 22, 13], "Erſte Zeile\nzweite")])

    def test_europeana_is_not_for_scoring(self):
        self.assertFalse(gold.PUBLIC["enp-berlin"]["scoring"])


# Line level, with word text that differs from the corrected line text (as on NZZ pages corrected per line).
PAGE_WORDS = """<?xml version="1.0"?>
<PcGts xmlns="http://schema.primaresearch.org/PAGE/gts/pagecontent/2013-07-15">
 <Page imageFilename="p1.png" imageWidth="40" imageHeight="30">
  <TextRegion id="r1"><Coords points="0,0 40,0 40,30 0,30"/>
   <TextLine id="l1"><Coords points="2,3 22,3 22,8 2,8"/>
    <Word id="w1"><Coords points="2,3 8,3 8,8 2,8"/><TextEquiv><Unicode>Zürih</Unicode></TextEquiv></Word>
    <TextEquiv><Unicode>Zürich, 1. Mai.</Unicode></TextEquiv></TextLine>
   <TextLine id="l2"><Coords points="2,9 22,9 22,14 2,14"/><TextEquiv><Unicode></Unicode></TextEquiv></TextLine>
  </TextRegion>
 </Page></PcGts>"""


class LineLevel(unittest.TestCase):
    def test_lines_use_line_text_never_words(self):
        with tempfile.TemporaryDirectory() as d:
            f = Path(d) / "p.xml"
            f.write_text(PAGE_WORDS)
            _, regs = gold.parse_page_xml(f, "line")
        self.assertEqual([(r["region"], r["box"], r["text"]) for r in regs], [("r1/l1", [2, 3, 22, 8], "Zürich, 1. Mai.")])

    def test_nzz_reads_only_the_ground_truth_tree(self):
        try:
            from PIL import Image
        except ImportError:
            self.skipTest("Pillow is not installed")
        with tempfile.TemporaryDirectory() as d:
            src = Path(d)
            for sub, text in (("NZZ_groundtruth", "Zürich, 1. Mai."), ("ABBYY_FineReader_Server11", "Zürih, l. Mai")):
                (src / "xml" / sub).mkdir(parents=True)
                (src / "xml" / sub / "p.xml").write_text(PAGE_WORDS.replace("Zürich, 1. Mai.", text))
            (src / "img").mkdir()
            Image.new("L", (40, 30), 200).save(src / "img" / "p1.png")
            self.assertEqual([r["text"] for r in gold.public_regions("nzz", src)], ["Zürich, 1. Mai."])

    def test_nzz_is_line_level(self):
        self.assertEqual(gold.PUBLIC["nzz"]["level"], "line")


def hipe_doc(doc_id: str, truth: list[str], ocr: list[str], which: str = "dataset3") -> dict:
    def unit(lines):
        text, offs, pos = "\n".join(lines), [], 0
        for ln in lines:
            offs.append([pos, pos + len(ln)])
            pos += len(ln) + 1
        return {"transcription_unit": text, "line_offsets": offs}

    return {"document_metadata": {"document_id": doc_id, "date": "1872", "publication_title": "A TITLE",
                                  "primary_dataset_filename": f"data/overproof/{which}/x.txt"},
            "ground_truth": unit(truth), "ocr_hypothesis": unit(ocr)}


ALTO = """<?xml version="1.0"?>
<alto xmlns="http://www.loc.gov/standards/alto/ns-v2#"><Layout><Page WIDTH="400" HEIGHT="300"><PrintSpace>
 <TextBlock>
  <TextLine><String HPOS="10" VPOS="10" WIDTH="100" HEIGHT="20" CONTENT="Tho"/>
   <String HPOS="120" VPOS="10" WIDTH="100" HEIGHT="20" CONTENT="cat"/></TextLine>
  <TextLine><String HPOS="10" VPOS="40" WIDTH="200" HEIGHT="20" CONTENT="sat"/><HYP CONTENT="-"/></TextLine>
  <TextLine><String HPOS="10" VPOS="70" WIDTH="200" HEIGHT="20" CONTENT="down."/></TextLine>
 </TextBlock></PrintSpace></Page></Layout></alto>"""


class Overproof(unittest.TestCase):
    def test_doc_id(self):
        self.assertEqual(gold.ca_page("sn88074142-11-00280761485-1872040501-1138"),
                         {"lccn": "sn88074142", "date": "1872-04-05", "edition": 1,
                          "tail": "/00280761485/1872040501/1138", "key": "sn88074142-00280761485-1872040501-1138"})
        self.assertEqual(gold.ca_page("sn85066387-3-noissue-190001011-0003")["tail"], "/190001011/0003")
        self.assertEqual(gold.ca_page("sn84026749-5-0028076484A-1922030801-0007")["date"], "1922-03-08")
        self.assertIsNone(gold.ca_page("13990000"))

    def test_soft_hyphen_becomes_a_hyphen(self):
        self.assertEqual(gold.hipe_lines(hipe_doc("x", ["in¬", "stalled"], ["a", "b"]), "ground_truth"), ["in-", "stalled"])

    def test_match_lines(self):
        alto = [{"text": t} for t in ("Tho cat", "sat", "down.", "other line entirely")]
        self.assertEqual(gold.match_lines(["Tho  cat", "sat-", "d0wn.", "nothing like it"], alto), [0, 1, None, None])
        long = [{"text": "the quick brown fox jumps"}, {"text": "over the lazy dog again"}]
        self.assertEqual(gold.match_lines(["the quick brown fox jumps", "ovor the lazy dog again"], long), [0, 1])

    def test_regions_from_matched_lines(self):
        try:
            from PIL import Image
        except ImportError:
            self.skipTest("Pillow is not installed")
        with tempfile.TemporaryDirectory() as d:
            src = Path(d)
            (src / "hipe").mkdir()
            (src / "loc").mkdir()
            key = "sn1-00001-1872040501-0002"
            doc = hipe_doc("sn1-11-00001-1872040501-0002", ["The cat", "sat-", "down.", "Lost line"],
                           ["Tho cat", "sat-", "down.", "zzzz qqqq"])
            smh = hipe_doc("999000", ["Text only"], ["Tcxt only"], "dataset2")
            (src / "hipe" / "a.jsonl").write_text(json.dumps(doc) + "\n" + json.dumps(smh) + "\n")
            (src / "loc" / f"{key}.xml").write_text(ALTO)
            Image.new("L", (400, 300), 200).save(src / "loc" / f"{key}.jpg")
            (src / "loc" / "pages.json").write_text(json.dumps({key: {"image": f"{key}.jpg", "alto": f"{key}.xml"}}))
            regs, stats = gold.overproof_ca_regions(src)
            self.assertEqual(stats, {"articles": 1, "articles_with_page": 1, "lines": 4, "lines_matched": 3})
            self.assertEqual([(r["box"], r["text"]) for r in regs], [([10, 10, 220, 90], "The cat\nsat-\ndown.")])
            smh_regs = gold.public_regions("overproof-smh", src)
            self.assertEqual([(r["image"], r["text"]) for r in smh_regs], [(None, "Text only")])

    def test_text_only_rows_validate(self):
        p = {"page_id": "overproof-smh-1", "region": "1", "text": "Some text", "box": None, "image": None}
        m, t = gold.public_rows("overproof-smh", p, 1, None, None, None, None)
        self.assertIsNone(m["image"])
        self.assertFalse(t["for_scoring"])
        self.assertEqual(gold.validate_rows([t], [m])[0], [])
        with tempfile.TemporaryDirectory() as d:
            self.assertEqual(gold.validate_rows([t], [m], Path(d)), ([], []))

    def test_matches_are_one_to_one(self):
        self.assertEqual(gold.match_lines(["foo", "foo"], [{"text": "foo"}]), [0, None])


class Bln600(unittest.TestCase):
    def test_fetch_checks_the_checksum(self):
        with tempfile.TemporaryDirectory() as d:
            (Path(d) / "BLN600.zip").write_bytes(b"not the real zip")  # download() keeps a file that is there
            self.assertEqual(gold.main(["public-fetch", "--dataset", "bln600", "--src", d, "--confirm-download"]), 1)
            self.assertFalse((Path(d) / "BLN600").exists())

    def test_fetch_needs_the_download_flag(self):
        self.assertEqual(gold.main(["public-fetch", "--dataset", "bln600", "--src", "/nonexistent-gold-test"]), 2)

    def test_convert_copies_the_original_image_and_the_licence(self):
        try:
            from PIL import Image
        except ImportError:
            self.skipTest("Pillow is not installed")
        with tempfile.TemporaryDirectory() as d:
            src, out = Path(d) / "src", Path(d) / "out"
            root = src / "BLN600"
            (root / "Ground Truth").mkdir(parents=True)
            (root / "Images").mkdir()
            text = "A made-up excerpt for the test.\r\nIt is not from the dataset, and it is long enough." * 5
            (root / "Ground Truth" / "1000000001.txt").write_text(text, encoding="utf-8")
            (root / "Ground Truth" / "1000000002.txt").write_text("no image for this one", encoding="utf-8")
            Image.new("L", (30, 20), 255).save(root / "Images" / "1000000001.tif")
            (root / "metadata.json").write_text(json.dumps([{"short_id": "1000000001", "date": "18820527",
                                                              "publication": "A Weekly"}]))
            (root / "LICENSE").write_text("licence text")
            self.assertEqual(gold.main(["public-convert", "--dataset", "bln600", "--src", str(src), "--out", str(out)]), 0)
            man = gold.read_jsonl(out / "manifest.jsonl")
            rows = gold.read_jsonl(out / "crops.jsonl")
            self.assertEqual(len(man), 1)
            self.assertEqual(man[0]["image"], "images/pub-bln600-001.tif")
            self.assertEqual((out / man[0]["image"]).read_bytes(), (root / "Images" / "1000000001.tif").read_bytes())
            self.assertEqual(man[0]["box"], [0, 0, 30, 20])
            self.assertEqual(rows[0]["date"], "1882-05-27")
            self.assertNotIn("\r", rows[0]["transcription"])
            self.assertEqual(gold.validate_rows(rows, man, out / "images"), ([], []))
            self.assertEqual((out / "LICENSES" / "LICENSE").read_text(), "licence text")
            self.assertEqual(json.loads((out / "LICENSES" / "SOURCE.json").read_text())["dataset"], "bln600")


class NewsEye(unittest.TestCase):
    def test_subset_flag_and_download_gate(self):
        self.assertEqual(gold.main(["public-fetch", "--dataset", "newseye-nlf-fi", "--subset", "validation",
                                    "--src", "/nonexistent-gold-test"]), 2)
        self.assertEqual(set(gold.NEWSEYE), {"austrian", "newseye-bnf", "newseye-nlf-fi", "newseye-nlf-sv"})



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


class DownloadZip(unittest.TestCase):
    """download_zip() refetches a cached file that is not a readable zip, including one whose
    central directory is intact but whose member data is corrupt."""

    def run_with(self, first: bytes, good: bytes) -> list[str]:
        calls = []
        with tempfile.TemporaryDirectory() as d:
            dest = Path(d) / "a.zip"
            dest.write_bytes(first)

            def fake_download(url, path):
                calls.append(url)
                if not path.exists():
                    path.write_bytes(good)

            orig = gold.download
            gold.download = fake_download
            import time
            real_sleep, time.sleep = time.sleep, lambda s: None
            try:
                gold.download_zip("https://example.org/a.zip", dest)
                self.assertEqual(dest.read_bytes(), good)
            finally:
                gold.download = orig
                time.sleep = real_sleep
        return calls

    @staticmethod
    def make_zip(data: bytes = b"hello world " * 100) -> bytes:
        import io
        import zipfile
        buf = io.BytesIO()
        with zipfile.ZipFile(buf, "w", zipfile.ZIP_STORED) as zf:
            zf.writestr("x.txt", data)
        return buf.getvalue()

    def test_good_zip_kept(self):
        good = self.make_zip()
        self.assertEqual(len(self.run_with(good, good)), 1)

    def test_html_reply_refetched(self):
        self.assertEqual(len(self.run_with(b"<html>rate limited</html>", self.make_zip())), 2)

    def test_corrupt_member_refetched(self):
        good = self.make_zip()
        bad = bytearray(good)
        i = bad.find(b"hello world")
        bad[i:i + 5] = b"HELLO"  # same length: headers intact, CRC wrong
        self.assertTrue(__import__("zipfile").is_zipfile(__import__("io").BytesIO(bytes(bad))))
        self.assertEqual(len(self.run_with(bytes(bad), good)), 2)

    def test_damaged_deflate_stream_refetched(self):
        import io
        import zipfile
        buf = io.BytesIO()
        with zipfile.ZipFile(buf, "w", zipfile.ZIP_DEFLATED) as zf:
            zf.writestr("x.txt", b"hello world " * 200)
        bad = bytearray(buf.getvalue())
        start = 30 + len("x.txt")  # local header, then the compressed data
        bad[start] = 0xFF  # an invalid deflate block type: zlib.error, not BadZipFile
        self.assertEqual(len(self.run_with(bytes(bad), self.make_zip())), 2)


if __name__ == "__main__":
    unittest.main()
