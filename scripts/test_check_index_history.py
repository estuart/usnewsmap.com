"""scripts/check-index-history.py: the schema and the checks it can't express."""

import copy
import importlib.util
import json
import tempfile
import unittest
import unittest.mock
from pathlib import Path

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("check", HERE / "check-index-history.py")
check = importlib.util.module_from_spec(spec)
spec.loader.exec_module(check)

ENTRY = {
    "commit": "d8b9c3041b8eb7871f922e383a445874dd9f5832",
    "engine": "Quickwit 0.9.1",
    "execution": "caj-usnm-ingest-prod-8dmuvwz",
    "features": {"common_grams": 1, "ja_fold": None},
    "full": True,
    "image": {
        "digest": "sha256:" + "8d" * 32,
        "pushed_at": "2026-09-28T18:33:50Z",
        "tag": "usnewsmap-ingest:main",
    },
    "ingest": "0.1.0",
    "reconstructed": "Reconstructed: from git.",
    "run_started_at": "2026-09-28T22:36:26.099785764Z",
    "templates": {"pages": {"sha256": "ec" * 32}, "pages-ja": {"sha256": "03" * 32}},
}
GOOD = {"pages-v20260928-1": copy.deepcopy(ENTRY), "pages-v20260928-2": copy.deepcopy(ENTRY)}


class Check(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.validate = staticmethod(check.fastjsonschema.compile(json.loads(check.SCHEMA.read_text())))

    def problems_in(self, text):
        with tempfile.TemporaryDirectory() as d:
            path = Path(d) / "index-history.json"
            path.write_text(text)
            return check.problems(path, self.validate)

    def problems(self, doc):
        return self.problems_in(json.dumps(doc, indent=2, sort_keys=True))

    def broken(self, change):
        doc = copy.deepcopy(GOOD)
        change(doc["pages-v20260928-1"])
        return self.problems(doc)

    def test_a_good_history_passes(self):
        self.assertEqual(self.problems(GOOD), [])

    def test_the_committed_history_passes(self):
        self.assertEqual(check.problems(check.HISTORY, self.validate), [])

    def test_nulls_the_reconstruction_writes_for_what_a_commit_lacks(self):
        def nulls(e):
            e.update(engine=None, ingest=None)
            e["features"]["ja_fold"] = None
            e["templates"]["pages"]["sha256"] = None
        self.assertEqual(self.broken(nulls), [])

    def test_schema_violations(self):
        for why, change in [
            ("unknown field", lambda e: e.update(note="x")),
            ("missing field", lambda e: e.pop("execution")),
            ("short commit", lambda e: e.update(commit="d8b9c30")),
            ("engine not Quickwit", lambda e: e.update(engine="0.9.1")),
            ("full not a boolean", lambda e: e.update(full="true")),
            ("unknown feature", lambda e: e["features"].update(stemming=1)),
            ("feature not a number", lambda e: e["features"].update(common_grams="1")),
            ("missing feature", lambda e: e["features"].pop("ja_fold")),
            ("another image tag", lambda e: e["image"].update(tag="usnewsmap-ingest:abc")),
            ("bad digest", lambda e: e["image"].update(digest="8d" * 32)),
            ("push time with fractions", lambda e: e["image"].update(pushed_at="2026-09-28T18:33:50.1Z")),
            ("unknown image field", lambda e: e["image"].update(size=1)),
            ("bad ingest version", lambda e: e.update(ingest="v0.1")),
            ("no note", lambda e: e.update(reconstructed="")),
            ("start time without zone", lambda e: e.update(run_started_at="2026-09-28T22:36:26")),
            ("no pages template", lambda e: e["templates"].pop("pages")),
            ("unknown template", lambda e: e["templates"].update(places={"sha256": "ec" * 32})),
            ("bad template hash", lambda e: e["templates"]["pages"].update(sha256="EC" * 32)),
        ]:
            got = self.broken(change)
            self.assertEqual(len(got), 1, (why, got))
            self.assertTrue(got[0].startswith("schema: "), (why, got))

    def test_version_names(self):
        for name in ["v20260928-1", "pages-v2026-09-28", "pages-ja-v20260928-1"]:
            got = self.problems({name: ENTRY})
            self.assertTrue(got and got[0].startswith("schema: "), (name, got))
        self.assertTrue(self.problems({})[0].startswith("schema: "))

    def test_keys_must_be_sorted_at_every_level(self):
        text = json.dumps(GOOD, indent=2, sort_keys=True)
        self.assertEqual(self.problems_in(text), [])
        swapped = json.dumps({"pages-v20260928-2": ENTRY, "pages-v20260928-1": ENTRY}, sort_keys=False)
        self.assertEqual(self.problems_in(swapped),
                         ["object with 'pages-v20260928-2': keys out of order: pages-v20260928-2, pages-v20260928-1"])
        entry = {"full": True, **{k: v for k, v in ENTRY.items() if k != "full"}}
        got = self.problems_in(json.dumps({"pages-v20260928-1": entry}))
        self.assertEqual(len(got), 1, got)
        self.assertTrue(got[0].startswith("object with 'full': keys out of order"), got)
        image = {"tag": "usnewsmap-ingest:main", "digest": ENTRY["image"]["digest"],
                 "pushed_at": ENTRY["image"]["pushed_at"]}
        got = self.problems_in(json.dumps({"pages-v20260928-1": {**ENTRY, "image": image}}))
        self.assertEqual(got, ["object with 'tag': keys out of order: tag, digest, pushed_at"])

    def test_repeated_keys(self):
        one = json.dumps(ENTRY, sort_keys=True)
        got = self.problems_in(f'{{"pages-v20260928-1": {one}, "pages-v20260928-1": {one}}}')
        self.assertEqual(got, ["object with 'pages-v20260928-1': repeated keys"])

    def test_the_image_comes_before_the_run(self):
        got = self.broken(lambda e: e["image"].update(pushed_at="2026-09-29T00:00:00Z"))
        self.assertEqual(got, ["pages-v20260928-1: image pushed at 2026-09-29T00:00:00Z, after the run started "
                               "at 2026-09-28T22:36:26.099785764Z"])

    def test_a_time_the_calendar_doesnt_have(self):
        got = self.broken(lambda e: e["image"].update(pushed_at="2026-99-99T00:00:00Z"))
        self.assertEqual(len(got), 1, got)
        self.assertTrue(got[0].startswith("pages-v20260928-1: not a real time: "), got)
        got = self.broken(lambda e: e.update(run_started_at="2026-02-30T22:36:26.099785764Z"))
        self.assertEqual(len(got), 1, got)
        self.assertTrue(got[0].startswith("pages-v20260928-1: not a real time: "), got)

    def test_main_checks_the_given_file(self):
        with tempfile.TemporaryDirectory() as d:
            path = Path(d) / "h.json"
            path.write_text(json.dumps({"pages-v20260928-1": {**ENTRY, "note": 1}}, sort_keys=True))
            with unittest.mock.patch.object(check.sys, "argv", ["check", "--path", str(path)]):
                self.assertEqual(check.main(), 1)
            path.write_text(json.dumps(GOOD, sort_keys=True))
            with unittest.mock.patch.object(check.sys, "argv", ["check", "--path", str(path)]):
                self.assertEqual(check.main(), 0)

    def test_not_json(self):
        self.assertTrue(self.problems_in("{")[0].startswith("not JSON"))


if __name__ == "__main__":
    unittest.main()
