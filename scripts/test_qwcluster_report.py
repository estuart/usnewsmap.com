"""Tests for scripts/qwcluster-report.py (python3 -m unittest scripts/test_qwcluster_report.py).

The fixtures in testdata/qwcluster/ are `qwcluster report` lines from a
local run of usnm-qwcluster load and bench on a two-node cluster, trimmed."""

import importlib.util
import json
import os
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("report", os.path.join(HERE, "qwcluster-report.py"))
report = importlib.util.module_from_spec(spec)
spec.loader.exec_module(report)


def fixture(name):
    with open(os.path.join(HERE, "testdata", "qwcluster", name)) as f:
        return json.load(f)


PRICES = {"vcpu_active": 0.000024, "vcpu_idle": 0.000003, "gib_active": 0.000003,
          "gib_idle": 0.000003, "blob_gb_month": 0.0184}


class ReportTest(unittest.TestCase):
    def test_reads_query_rows(self):
        v = {"tables": [{"columns": [{"name": "Kind"}, {"name": "Report"}],
                         "rows": [["load", "{\"a\": 1}"], ["bench", "{cut short"]]}]}
        rows = report.rows_of(v)
        self.assertEqual(rows[0], {"Kind": "load", "Report": "{\"a\": 1}"})
        self.assertEqual(report.parse_reports(rows), [("load", {"a": 1})])

    def test_spans(self):
        self.assertEqual(report.span_of("2d"), "P2D")
        self.assertEqual(report.span_of("6h"), "PT6H")
        with self.assertRaises(SystemExit):
            report.span_of("2w")

    def test_the_sidecar_costs_what_238_says(self):
        # #238: one 2 vCPU / 4 GiB sidecar, all idle, $47.30 a month.
        idle = report.replica_cost(PRICES, 2, 4, 0, report.SECONDS_PER_MONTH)
        self.assertAlmostEqual(idle, 47.30, places=2)
        self.assertAlmostEqual(report.steady_state(PRICES, 3, 2), 3 * idle, places=6)
        # Active seconds cost more for vCPU only.
        self.assertGreater(report.steady_state(PRICES, 1, 2, 0.9), idle)

    def test_picks_list_prices(self):
        aca = [{"meterName": n, "retailPrice": v, "tierMinimumUnits": 0} for n, v in [
            ("Standard vCPU Active Usage", 2.4e-05), ("Standard vCPU Idle Usage", 3e-06),
            ("Standard Memory Active Usage", 3e-06), ("Standard Memory Idle Usage", 3e-06)]]
        blob = [{"meterName": "Hot LRS Data Stored", "retailPrice": 0.0184, "tierMinimumUnits": 0},
                {"meterName": "Hot LRS Data Stored", "retailPrice": 0.0177, "tierMinimumUnits": 51200}]
        self.assertEqual(report.pick_prices(aca, blob), PRICES)
        with self.assertRaises(SystemExit):
            report.pick_prices([], blob)

    def test_prices_retry_a_refusal_then_give_up_quietly(self):
        import io
        import urllib.error

        class Resp(io.BytesIO):
            def __enter__(self):
                return self

            def __exit__(self, *a):
                return False

        def refusal(after=None):
            return urllib.error.HTTPError("u", 429, "Too Many Requests", {"Retry-After": after} if after else {}, None)

        calls, waits = [], []
        def opener(url, timeout):
            calls.append(url)
            if len(calls) < 3:
                raise refusal("5" if len(calls) == 1 else None)
            return Resp(b'{"Items": [1]}')
        self.assertEqual(report.fetch_json("u", sleep=waits.append, opener=opener), {"Items": [1]})
        self.assertEqual(waits, [5.0, 4])
        # Not a refusal: no retry.
        def gone(url, timeout):
            raise urllib.error.HTTPError("u", 404, "Not Found", {}, None)
        with self.assertRaises(urllib.error.HTTPError):
            report.fetch_json("u", sleep=waits.append, opener=gone)
        # Refused every time: prices() gives None and the report leaves costs out.
        def always(url):
            raise refusal()
        self.assertIsNone(report.prices("eastus2", fetch=always))

    def test_summarizes_metrics_per_minute(self):
        def series(name, agg, values):
            return {"name": {"value": name}, "timeseries": [{"data": [
                {"timeStamp": f"2026-10-09T00:0{i}:00Z", agg: x} for i, x in enumerate(values)]}]}
        v = {"value": [
            series("UsageNanoCores", "average", [5e6, 1.5e9, 2e6]),
            series("WorkingSetBytes", "maximum", [2**30, 2 * 2**30, 2**30]),
            series("RxBytes", "total", [6000, 600000, 120000]),
            series("TxBytes", "total", [6000, 600000, 0]),
        ]}
        m = report.summarize_metrics(v)
        self.assertEqual(m["minutes"], 3)
        self.assertAlmostEqual(m["cpu_avg"], (0.005 + 1.5 + 0.002) / 3)
        self.assertEqual(m["mem_max_mib"], 2048)
        # Minute 1 is busy; minute 3 is quiet on CPU but over 1,000 bytes/s.
        self.assertEqual(m["idle_minutes"], 1)
        # Without network values a quiet minute isn't counted idle.
        v["value"] = [x for x in v["value"] if x["name"]["value"] != "TxBytes"]
        self.assertEqual(report.summarize_metrics(v)["idle_minutes"], 0)

    def test_tables_from_real_reports(self):
        load = json.loads(fixture("load.json")["Report"])
        bench = json.loads(fixture("bench.json")["Report"])
        rows = report.load_rows([load])
        self.assertEqual(rows[0][0], "s2ix")
        self.assertEqual(rows[0][1], 2)
        self.assertIn("qw-0", rows[0][-1])
        b = report.bench_rows([bench])
        self.assertEqual([r[3] for r in b], ["first", "warm", "c1", "c2", "c4", "c10"])
        self.assertTrue(all(r[-1] == "yes" for r in b))
        md = report.table(["a", "b"], [[1, 2]])
        self.assertEqual(md.splitlines(), ["| a | b |", "|---|---|", "| 1 | 2 |"])
        self.assertGreater(report.secs_between(load["started_at"], load["sealed_at"]), 0)

    def test_compares_quickwit_versions(self):
        def bench(label, version, median, busy_ms, gb):
            node = {"main_busy_ms": busy_ms, "main_threads": 4.0, "download_bytes": gb * 1e9,
                    "leaf_splits": 58.0}
            return {"label": label, "indexes": ["s1ixb"], "american_stories": False,
                    "quickwit": {"version": version, "commit": "x", "num_cpus": 4},
                    "members": [{"node_id": "qws-0"}],
                    "passes": [{"name": "c1", "concurrency": 1, "median_secs": median, "p90_secs": median,
                                "wall_secs": 20.0, "failed": 0, "searches": [{}] * 10,
                                "nodes": {"qws-0": node},
                                "sampled": {"qws-0": {"samples": 10, "search_ongoing_max": 4.0,
                                                      "search_pending_max": 2.0, "samples_pending": 5}}}]}
        benches = [bench("v091-r1", "0.9.1", 2.0, 40000, 2.0), bench("n2509-r1", "0.8.0-nightly", 1.5, 20000, 2.0),
                   bench("v091-r2", "0.9.1", 2.2, 44000, 2.0)]
        rows = report.compare_rows(benches)
        self.assertEqual(len(rows), 3)
        r = rows[0]
        self.assertEqual(r[:2], ["v091-r1", "0.9.1"])
        # 40 s busy over 10 searches; 40 s of 4 threads × 20 s; 0.2 GB a search; 20 s a GB.
        self.assertEqual(r[9:13], ["4.00", "50", "0.200", "20.0"])
        self.assertEqual(r[13:], ["4", "2", "50"])
        summary = report.compare_summary(benches)
        by_version = {row[2]: row for row in summary}
        self.assertEqual(by_version["0.9.1"][3], 2)
        self.assertEqual(by_version["0.9.1"][5], "1.00")
        # The nightly: 1.5 s against 2.1, half the busy seconds per GB.
        self.assertEqual(by_version["0.8.0-nightly"][5], "0.71")
        self.assertEqual(by_version["0.8.0-nightly"][9], "0.48")
        # The correctness gate: the same pages for the same searches.
        for b, pages in zip(benches, (10, 10, 10)):
            b["passes"][0]["shift_days"] = 1
            b["passes"][0]["searches"] = [{"name": "radio (month)", "pages": pages, "error": None}]
        self.assertEqual(report.hit_mismatches(benches), [])
        benches[1]["passes"][0]["searches"][0]["pages"] = 11
        bad = report.hit_mismatches(benches)
        self.assertEqual(len(bad), 1)
        self.assertEqual(bad[0][2], "radio (month)")
        self.assertIn("0.8.0-nightly: [11]", bad[0][3])
        # Reports from before the counters have no comparison rows.
        old = json.loads(fixture("bench.json")["Report"])
        self.assertEqual(report.compare_rows([old]), [])
        self.assertEqual(report.app_of("qws-1"), "ca-usnm-qws-1")
        self.assertEqual(report.app_of("qw-0"), "ca-usnm-qw-0")

    def test_local_disk_tables(self):
        stats = {"num_hits": 10, "localexec_num_splits": 58, "leaf_wall_time_microsecs": 2_000_000,
                 "splits": {"warmup_microsecs": 4_000_000, "wait_for_cpu_pool_microsecs": 0,
                            "cpu_search_microsecs": 1_000_000, "wait_for_search_permit_microsecs": 500_000,
                            "download_num_bytes": 2e9}}
        b = {"label": "loc-cache-r2", "indexes": ["s1ixb"], "quickwit": {"version": "0.9.1"},
             "split_cache": {"target_splits": 58, "splits": 58, "bytes": 7.5e9, "secs": 300.0, "complete": True},
             "probe": {"shift_days": 9, "sum": stats, "searches": [{"name": "a", "stats": stats}]},
             "profile": {"summary": {"total_samples": 1000,
                                     "threads": {"main_runtime_th": 600, "quickwit-search": 300},
                                     "main_runtime_top": [["rustls::read", 300], ["memcpy", 60]]}},
             "passes": [{"name": "c1", "nodes": {"qwl-0": {"split_cache_hits": 900, "split_cache_misses": 3}}}]}
        p = report.probe_rows([b])[0]
        self.assertEqual(p[2:], [1, "58", "4.00", "0.00", "1.00", "0.50", "2.000", "2.0", "2.00"])
        f = report.profile_rows([b])[0]
        self.assertEqual(f[1:5], [1000, "60", "30", "10"])
        self.assertEqual(f[5], "rustls::read 50%, memcpy 10%")
        c = report.cache_rows([b])[0]
        self.assertEqual(c, ["loc-cache-r2", "c1", "58/58", "7.5", "300", "900", "3"])
        self.assertEqual(report.probe_rows([{"label": "x", "passes": []}]), [])

    def v091_bench(self, label="cmp-v091-r1"):
        """A 0.9.1 bench with a probe, as the job logs it since #251 split its
        lines: the root's version with its tag's `v`."""
        node = {"main_busy_ms": 40000.0, "main_threads": 4.0, "download_bytes": 2e9, "leaf_splits": 58.0}
        passes = [{"name": n, "concurrency": 1, "median_secs": 2.0, "p90_secs": 2.5, "wall_secs": 20.0,
                   "failed": 0, "shift_days": i, "started_at": "2026-10-10T00:35:31Z",
                   "searches": [{"name": "radio (month)", "pages": 10, "error": None}] * 10,
                   "nodes": {"qws-0": node}, "sampled": {}} for i, n in enumerate(["first", "c1"])]
        probe = {"shift_days": 6, "searches": [{"name": "radio (month)", "secs": 1.0, "stats": {}}],
                 "sum": {"num_hits": 10, "localexec_num_splits": 58, "leaf_wall_time_microsecs": 1,
                         "splits": {"warmup_microsecs": 1, "wait_for_cpu_pool_microsecs": 0,
                                    "cpu_search_microsecs": 1, "wait_for_search_permit_microsecs": 0,
                                    "download_num_bytes": 1}}}
        full = {"label": label, "indexes": ["s1ixb"], "american_stories": False,
                "quickwit": {"version": "v0.9.1", "commit": "962685f", "num_cpus": 4},
                "members": [{"node_id": "qws-0", "services": ["searcher", "metastore"]}], "searchers": 1,
                "started_at": "2026-10-10T00:35:31Z", "ended_at": "2026-10-10T00:37:59Z",
                "passes": passes, "probe": probe, "profile": None, "split_cache": None, "variant": None}
        return full

    def nightly_bench(self, label="cmp-n2509-r1"):
        b = self.v091_bench(label)
        b["quickwit"] = {"version": "0.8.0-nightly", "commit": "eec5bbc", "num_cpus": 4}
        b["probe"] = None
        for p in b["passes"]:
            p["nodes"]["qws-0"] = dict(p["nodes"]["qws-0"], main_busy_ms=120000.0)
            p["median_secs"] = 6.0
        return b

    def test_a_tagged_version_with_a_probe_is_compared(self):
        # As the job logs them now: in parts (crates/usnm-ingest/src/cluster/bench.rs, log_parts).
        def parts(full):
            head = {k: v for k, v in full.items() if k not in ("passes", "probe", "profile")}
            head["passes_logged"] = len(full["passes"])
            out = [("bench", head)]
            out += [("bench_pass", {"label": full["label"], "n": n, "pass": p}) for n, p in enumerate(full["passes"])]
            if full.get("probe"):
                out.append(("bench_probe", {"label": full["label"], "probe": full["probe"]}))
            return out
        reports = report.assemble(parts(self.v091_bench()) + parts(self.nightly_bench()))
        benches = [r for k, r in reports if k == "bench"]
        self.assertEqual([len(b["passes"]) for b in benches], [2, 2])
        self.assertEqual(benches[0]["probe"]["sum"]["localexec_num_splits"], 58)
        self.assertEqual(report.version_of(benches[0]), "0.9.1")
        rows = report.compare_rows(benches)
        self.assertEqual(sorted({r[1] for r in rows}), ["0.8.0-nightly", "0.9.1"])
        by = {(row[1], row[2]): row for row in report.compare_summary(benches)}
        self.assertEqual(by[("c1", "0.9.1")][5], "1.00")
        self.assertEqual(by[("c1", "0.8.0-nightly")][5], "3.00")
        self.assertEqual(by[("c1", "0.8.0-nightly")][7], "3.00")
        self.assertEqual(report.hit_mismatches(benches), [])
        self.assertEqual(len(report.probe_rows(benches)), 1)
        # A local-disk run is grouped by the mode its root was found in.
        b = dict(benches[0], variant="nfs", mode="blob")
        self.assertEqual(report.compare_rows([b])[0][1], "0.9.1 blob")

    def test_salvages_a_line_cut_short(self):
        # October 2026: one line held a whole 0.9.1 run with its probe, and
        # Container Apps kept its first 16,346 characters.
        full = self.v091_bench()
        full["probe"]["searches"] = full["probe"]["searches"] * 600
        line = json.dumps({"timestamp": "t", "level": "INFO", "fields": {
            "message": "qwcluster report", "kind": "bench",
            "report": json.dumps(full, sort_keys=True, separators=(",", ":"))}}, separators=(",", ":"))
        raw = line[:16346]
        with self.assertRaises(ValueError):
            json.loads(raw)
        kind, r = report.salvage(raw)
        self.assertEqual(kind, "bench")
        self.assertTrue(r["truncated"])
        self.assertEqual([p["name"] for p in r["passes"]], ["first", "c1"])
        self.assertNotIn("quickwit", r)
        # Its build from the run's `bench root` line, its start from its first pass.
        root = {"label": "cmp-v091-r1", "quickwit":
                'Some(QuickwitBuild { version: Some("v0.9.1"), commit: Some("962685f"), num_cpus: Some(4) })'}
        rows = [{"Kind": "", "Report": "", "Raw": raw},
                {"Kind": "bench_root", "Report": json.dumps(root), "Raw": ""}]
        (k, b), = report.assemble(report.parse_reports(rows))
        self.assertEqual(b["quickwit"], {"version": "v0.9.1", "commit": "962685f", "num_cpus": 4})
        self.assertEqual(b["started_at"], "2026-10-10T00:35:31Z")
        self.assertEqual(b["searchers"], 1)
        self.assertEqual(len(report.compare_rows([b])), 2)
        # The same from a captured log stream.
        root_line = json.dumps({"fields": {"message": "bench root", **root}})
        got = dict(report.reports_from_lines(["2026-10-10T00:37:59Z " + raw, root_line]))
        self.assertTrue(got[("bench", "cmp-v091-r1")]["truncated"])
        (k, b2), = report.assemble([(k, r) for (k, _), r in got.items()])
        self.assertEqual(report.version_of(b2), "0.9.1")
        # A dump logs the stored report again, whole: it replaces the salvaged one.
        whole = dict(full, probe=self.v091_bench()["probe"])
        head = {k: v for k, v in whole.items() if k not in ("passes", "probe", "profile")}
        head["passes_logged"] = 2
        dumped = [("bench", head)] + [("bench_pass", {"label": "cmp-v091-r1", "n": n, "pass": p})
                                      for n, p in enumerate(whole["passes"])]
        dumped.append(("bench_probe", {"label": "cmp-v091-r1", "probe": whole["probe"]}))
        both = report.assemble(report.parse_reports(rows) + dumped)
        self.assertEqual(len(both), 1)
        self.assertFalse(both[0][1].get("truncated"))
        self.assertEqual(len(report.probe_rows([both[0][1]])), 1)
        # Cut inside the passes: nothing to salvage.
        self.assertIsNone(report.salvage(line[:600]))
        self.assertIsNone(report.salvage("not a report"))

    def test_reads_reports_from_a_log_stream(self):
        load = fixture("load.json")["Report"]
        line = json.dumps({"timestamp": "t", "level": "INFO",
                           "fields": {"message": "qwcluster report", "kind": "load", "report": load}})
        lines = [
            "noise",
            line,
            "2026-10-09T10:00:00Z " + json.dumps({"Log": line}),
            json.dumps({"fields": {"message": "cluster load progress"}}),
            json.dumps({"fields": {"message": "qwcluster report", "kind": "bench", "report": "{bad"}}),
        ]
        got = report.reports_from_lines(lines)
        self.assertEqual(len(got), 1)
        (kind, name), r = got[0]
        self.assertEqual((kind, name), ("load", "s2ix"))
        self.assertEqual(r["docs"], json.loads(load)["docs"])


if __name__ == "__main__":
    unittest.main()
