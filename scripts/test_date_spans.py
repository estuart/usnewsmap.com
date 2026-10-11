"""scripts/lib/date-spans.jq: searches.sh's date-span tables (#123), on invented records."""

import json
import subprocess
import unittest
from datetime import date
from pathlib import Path

JQ = Path(__file__).resolve().parent / "lib" / "date-spans.jq"


def rec(start, end, corpus=None, version="pages-v1", query="invented words"):
    r = {"v": 1, "day": "2026-10-01", "source": "api", "q": query, "query": query,
         "mode": None, "near": None, "fuzzy": 0, "from": start, "to": end, "state": [],
         "lccn": [], "lang": [], "front": False, "bucket": "year", "pages": 1,
         "index_version": version}
    if corpus:
        r["corpus"] = corpus
    return r


def run(records, by, program=None, bounds=None):
    args = ["jq", "-r", "--arg", "by", by]
    if bounds is not None:
        args += ["--argjson", "bounds", json.dumps(bounds)]
    args += [program] if program else ["-f", str(JQ)]
    out = subprocess.run(args, input=json.dumps(records), capture_output=True, text=True, check=True)
    return out.stdout


def table(records, by, bounds=None):
    rows = [line.split("\t") for line in run(records, by, bounds=bounds).splitlines()]
    return rows[0], {k: (int(n), share) for k, n, share in rows[1:]}


# Newspapers in pages-v1 span 1736-01-01 to 1963-12-31, Congress 1774-01-01
# to 1875-12-31, and pages-v2 1860-01-01 to 1875-12-31: each group's whole
# range is its earliest `from` and latest `to`.
RECORDS = [
    rec("1736-01-01", "1963-12-31"),  # whole range (the default)
    rec("1736-01-01", "1963-12-31"),
    rec("1896-01-01", "1896-12-31"),  # 366 days, 1 decade
    rec("1896-03-01", "1896-03-31"),  # 31 days, 1 decade
    rec("1890-01-01", "1899-12-31"),  # 3,652 days, 1 decade
    rec("1895-06-01", "1905-06-02"),  # 3,654 days, 2 decades
    rec("1736-01-01", "1835-01-01"),  # 1820 (with everything before) and 1830
    rec("1800-01-01", "1963-12-31"),  # 1820 to 1960: 15 partitions
    rec("1774-01-01", "1875-12-31", corpus="congress"),  # Congress's whole range
    rec("1774-01-01", "1775-03-01", corpus="congress"),  # 425 days, 1 partition (pre-1820)
    rec("1860-01-01", "1875-12-31", version="pages-v2"),  # pages-v2's whole range
    rec("1870-01-01", "1875-12-31", version="pages-v2"),  # 2,191 days, 1 decade
]


class DateSpans(unittest.TestCase):
    def test_by_span(self):
        header, got = table(RECORDS, "span")
        self.assertEqual(header, ["span", "searches", "share"])
        self.assertEqual(got, {
            "whole range": (4, "33.3%"),
            "over 10 years": (3, "25%"),
            "1-10 years": (3, "25%"),
            "a year or less": (2, "16.7%"),
        })

    def test_by_decades_touched(self):
        header, got = table(RECORDS, "decades")
        self.assertEqual(header, ["decades", "searches", "share"])
        self.assertEqual(got, {
            "whole range": (4, "33.3%"),
            "1": (5, "41.7%"),
            "2": (2, "16.7%"),
            "3": (0, "0%"),
            "4-5": (0, "0%"),
            "6-10": (0, "0%"),
            "11 or more": (1, "8.3%"),
        })

    def test_partitions_as_usnm_core_decade(self):
        # The cases of usnm_core::decade::between's tests, plus decade edges.
        cases = [
            ("1896-01-01", "1896-12-31", "1"),
            ("1895-06-01", "1905-06-01", "2"),
            ("1700-01-01", "1835-01-01", "2"),
            ("1700-01-01", "1819-12-31", "1"),
            ("1819-12-31", "1820-01-01", "1"),
            ("1829-12-31", "1830-01-01", "2"),
            ("1860-01-01", "1899-12-31", "4-5"),
            ("1850-01-01", "1910-01-01", "6-10"),
        ]
        # A wider record per group sets its whole range, so none of these is it.
        for start, end, want in cases:
            with self.subTest(start=start, end=end):
                _, got = table([rec("1600-01-01", "1999-12-31"), rec(start, end)], "decades")
                self.assertEqual(got["whole range"][0], 1)
                self.assertEqual(got[want][0], 1)

    def test_day_number(self):
        program = JQ.read_text().split("\ndef partition")[0] + "\nmap(day_number)"
        dates = ["1970-01-01", "1969-12-31", "2000-03-01", "1896-12-31", "1736-01-01",
                 "1600-02-29", "0400-02-29"]
        want = [(date.fromisoformat(d) - date(1970, 1, 1)).days for d in dates]
        self.assertEqual(json.loads(run(dates, "span", program)), want)

    def test_aggregates_only(self):
        recs = [rec("1896-01-01", "1896-12-31", query="invented-term-xyzzy")]
        for by in ("span", "decades"):
            self.assertNotIn("xyzzy", run(recs, by))

    def test_whole_range_from_the_served_bounds(self):
        # Every search in the window is for 1896: none is the whole range,
        # which the widest-search fallback would have claimed.
        bounds = {"news": {"from": "1736-01-01", "to": "1963-12-31"},
                  "congress": {"from": "1774-01-01", "to": "1994-12-31"}}
        narrow = [rec("1896-01-01", "1896-12-31"), rec("1896-03-01", "1896-03-31")]
        self.assertEqual(table(narrow, "span", bounds)[1]["whole range"], (0, "0%"))
        self.assertEqual(table(narrow, "span")[1]["whole range"], (1, "50%"))
        # An older version's wider default still covers the bounds; Congress
        # uses its own, and a corpus with no bounds falls back.
        records = [
            rec("1700-01-01", "1963-12-31", version="pages-v0"),
            rec("1736-01-01", "1963-12-31"),
            rec("1774-01-01", "1994-12-31", corpus="congress"),
            rec("1774-01-01", "1875-12-31", corpus="congress"),
            rec("1900-01-01", "1910-12-31", corpus="other"),
        ]
        got = table(records, "span", bounds)[1]
        self.assertEqual(got["whole range"], (4, "80%"))
        self.assertEqual(got["over 10 years"], (1, "20%"))

    def test_empty(self):
        _, got = table([], "span")
        self.assertEqual(got["whole range"], (0, "-"))


if __name__ == "__main__":
    unittest.main()
