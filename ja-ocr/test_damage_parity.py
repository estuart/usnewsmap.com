"""Tests for damage_parity.py: python3 -m unittest discover ja-ocr (pyarrow for zstd before Python 3.14)."""

import csv
import gzip
import io
import json
import tempfile
import unittest

import damage_parity
import damage_v2
import jaocr

try:
    import wordfreq
except ImportError:  # the image has it
    wordfreq = None

TEXT = ("The council of the city met on Tuesday evening at the court house, and after a long debate it was voted "
        "that the new school should be built on the lot which the board had bought last year. ")


def zstd_available() -> bool:
    try:
        damage_parity.zstd_compress(b"x")
        return True
    except ImportError:
        return False


class Spread(unittest.TestCase):
    def test_groups_take_turns(self):
        rows = [{"doc_id": f"e{i}", "language": "eng", "decision": "detected", "script": "latin"} for i in range(50)]
        rows += [{"doc_id": "g1", "language": "ger", "decision": "detected", "script": "latin"},
                 {"doc_id": "y1", "language": "yid", "decision": "detected", "script": "hebrew"},
                 {"doc_id": "u1", "language": "und", "decision": "und", "script": "latin"}]
        got = damage_parity.spread(rows, 6)
        self.assertEqual(len(got), 6)
        self.assertTrue({"g1", "y1", "u1"} <= {r["doc_id"] for r in got})
        self.assertEqual(damage_parity.spread(rows, 6), got, "deterministic")
        self.assertEqual(len(damage_parity.spread(rows, 1000)), len(rows))


@unittest.skipIf(wordfreq is None, "wordfreq not installed")
class Records(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.models = damage_v2.Models()

    def test_record_holds_scores_counts_and_forced_scores(self):
        r = damage_parity.record("d1", TEXT * 3, ("ger", "eng"), self.models)
        self.assertEqual(r["score"]["lang"], "eng")
        self.assertEqual(r["damage"]["words"], r["score"]["words"])
        self.assertEqual(set(r["forced"]), {"ger", "eng"})
        self.assertEqual(r["forced"]["eng"]["damage_rate"], 0.0)
        no_list = damage_parity.record("d2", TEXT, ("cho",), self.models)
        self.assertIsNone(no_list["forced"]["cho"])

    def test_forced_matches_a_direct_damage(self):
        got = damage_v2.forced(TEXT.replace("the ", "tbe "), "eng", self.models)
        self.assertGreater(got["damage_rate"], 0.25)
        self.assertLessEqual(got["known_words"], got["words"])
        self.assertIsNone(damage_v2.forced(TEXT, "cho", self.models))

    @unittest.skipUnless(zstd_available(), "no zstd")
    def test_encoded_file_round_trips(self):
        recs = [damage_parity.record("d1", TEXT * 3, ("eng",), self.models)]
        lines = damage_parity.zstd_decompress(damage_parity.encode("test", recs)).decode().splitlines()
        head = json.loads(lines[0])["header"]
        self.assertEqual((head["pages"], head["source"]), (1, "test"))
        self.assertEqual(head["damage_v2_sha256"], damage_parity.damage_v2_sha256())
        self.assertEqual(json.loads(lines[1])["doc_id"], "d1")

    @unittest.skipUnless(zstd_available(), "no zstd")
    def test_cloud_sample_reads_curated_text(self):
        import pyarrow as pa
        import pyarrow.parquet as pq

        with tempfile.TemporaryDirectory() as root:
            ref, cur = jaocr.LocalStore(f"{root}/ref"), jaocr.LocalStore(f"{root}/cur")
            version = "pages-v20261003-1"
            ref.write("current.json", json.dumps({"reference": version}).encode())
            ref.write(f"{version}/batches.json", json.dumps([
                {"batch": "batch_a", "curated": {"version": 1, "parts": ["parts/a.parquet"]}}]).encode())
            buf = io.BytesIO()
            pq.write_table(pa.table({"doc_id": ["p1", "p2"], "text": [TEXT * 3, TEXT * 2]}), buf)
            cur.write("parts/a.parquet", buf.getvalue())
            out = io.StringIO()
            w = csv.writer(out, lineterminator="\n")
            w.writerow(["doc_id", "batch", "title_languages", "language", "decision", "script"])
            w.writerow(["p1", "batch_a_ver01", "eng ger", "eng", "detected", "latin"])
            w.writerow(["p2", "batch_a_ver01", "eng", "eng", "detected", "latin"])
            w.writerow(["p3", "batch_a_ver01", "eng", "no_text", "no_text", ""])
            cur.write(f"audit/ocr-quality-v2-{version}-10pct-pages.csv.gz", gzip.compress(out.getvalue().encode()))
            path = damage_parity.damage_parity(ref, cur, pages=5, audit_version=version)
            lines = damage_parity.zstd_decompress(cur.read(path)).decode().splitlines()
            self.assertEqual(json.loads(lines[0])["header"]["pages"], 2)
            got = {json.loads(x)["doc_id"]: json.loads(x)["languages"] for x in lines[1:]}
            self.assertEqual(got, {"p1": ["eng", "ger"], "p2": ["eng"]})
            with self.assertRaises(ValueError):
                damage_parity.damage_parity(ref, cur, pages=0, audit_version=version)
            # With the audit version's snapshot gone, the current one's, if its name is safe.
            ref2 = jaocr.LocalStore(f"{root}/ref2")
            ref2.write("current.json", json.dumps({"reference": "../escape"}).encode())
            with self.assertRaisesRegex(ValueError, "unsafe reference version"):
                damage_parity.damage_parity(ref2, cur, pages=5, audit_version=version)


if __name__ == "__main__":
    unittest.main()
