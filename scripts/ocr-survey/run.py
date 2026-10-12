#!/usr/bin/env python3
"""Run the survey's engines on the sampled pages, one container at a time (#296).

    run.py --sample /srv/ocr-survey/sample --work /srv/ocr-survey/work --engines tesseract,doctr

For each engine it writes WORK/jobs/<engine>.jsonl from the manifest and
the plan below, then runs the engine's container (ocr-survey/<engine>, built
from containers/<engine>/Containerfile) on them. The container measures
each page (engine_runner.py) and appends to WORK/results/<engine>.jsonl;
text and ALTO go to WORK/out/<engine>/. Only one engine runs at a time, so
the timings don't compete for the node's 4 threads. Rerunning skips jobs
that have a result.

--parallel N splits an engine's jobs over N containers that run at once
(Tesseract: one thread each), each writing WORK/results/<engine>.<i>.jsonl.
Its seconds per page are then measured under load (the result's
`concurrency`). For clean seconds per page, run a subset alone with
--parallel 1 --pages-per-group K into a separate --work directory.

--throughput N runs N Tesseract containers at once on the same pages (the
first --throughput-pages German pages with deu_latf, OMP_THREAD_LIMIT=1
each) and records the wall time of the whole set, for pages per day.
"""

from __future__ import annotations

import argparse
import json
import shutil
import subprocess
import time
from pathlib import Path

# engine -> group -> [(variant, scale)]; pages per group (None = all), timeout per page (seconds)
PLAN = {
    "tesseract": ({
        "german": [("deu", 1.0), ("deu_latf", 1.0), ("frak2021", 1.0), ("deu_latf", 0.5)],
        "worst-english": [("eng", 1.0), ("eng", 0.5)],
        "control": [("eng", 1.0), ("eng", 0.5)],
        "serbian": [("srp", 1.0), ("srp_latn", 1.0), ("srp", 0.5)],
        "czech": [("ces", 1.0), ("ces", 0.5)],
        "russian": [("rus", 1.0)],
        "polish": [("pol", 1.0)],
    }, None, 1800),
    "doctr": ({
        g: [("db_resnet50+parseq-multilingual", 1.0)]
        for g in ("worst-english", "control", "german", "czech", "polish")
    }, None, 1800),
    "ndnp-open-ocr": ({
        "worst-english": [("segmentation", 1.0)],
        "control": [("segmentation", 1.0)],
    }, None, 1800),
    "americanstories": ({
        "worst-english": [("word-level", 1.0)],
        "control": [("word-level", 1.0)],
    }, None, 1800),
    "surya": ({
        g: [("surya-ocr-2-tiles", 1.0)] for g in ("worst-english", "serbian", "russian", "german")
    }, 2, 3600),
}


def jobs_for(engine: str, manifest: list[dict], pages_per_group: int | None = None) -> list[dict]:
    plan, per_group, timeout = PLAN[engine]
    if pages_per_group is not None:
        per_group = pages_per_group if per_group is None else min(per_group, pages_per_group)
    out, seen = [], {}
    for r in manifest:
        if r["group"] not in plan or not r.get("in_sample", r["group"] != "control"):
            continue  # control candidates wait for score.py --select-control
        seen[r["group"]] = seen.get(r["group"], 0) + 1
        if per_group is not None and seen[r["group"]] > per_group:
            continue
        for variant, scale in plan[r["group"]]:
            out.append({
                "job_id": f"{r['page_id']}__{variant}@{scale:g}", "page_id": r["page_id"], "group": r["group"],
                "image": f"/data/images/{r['page_id']}.jpg", "variant": variant, "scale": scale,
                "timeout": timeout,
            })
    return out


ENV: list[str] = []  # --env: extra environment for the engine containers


def podman(engine: str, sample: Path, work: Path, jobs: Path, results: Path, out: Path, extra=(),
           memory: str = "7g") -> list[str]:
    extra = [*extra, *(x for e in ENV for x in ("-e", e))]
    return ["podman", "run", "--rm", "--memory", memory, "--network", "none", *extra,
            "-v", f"{sample}:/data:ro,z", "-v", f"{work}:/work:z", f"ocr-survey/{engine}",
            "--jobs", f"/work/{jobs.relative_to(work)}", "--results", f"/work/{results.relative_to(work)}",
            "--out", f"/work/{out.relative_to(work)}"]


def load_manifest(sample: Path) -> list[dict]:
    p = sample / "manifest-final.jsonl"
    if not p.exists():
        p = sample / "manifest.jsonl"
    return [json.loads(x) for x in p.read_text().splitlines() if x.strip()]


def throughput(sample: Path, work: Path, n: int, pages: int) -> None:
    manifest = [r for r in load_manifest(sample) if r["group"] == "german"][:pages]
    d = work / "throughput" / f"x{n}"
    if d.exists():  # results left from an earlier run would be skipped as done and count as new work
        shutil.rmtree(d)
    d.mkdir(parents=True)
    shards = [manifest[i::n] for i in range(n)]
    procs, t0 = [], time.time()
    for i, shard in enumerate(shards):
        jobs = d / f"jobs-{i}.jsonl"
        jobs.write_text("".join(json.dumps({
            "job_id": f"{r['page_id']}__deu_latf@1", "page_id": r["page_id"], "image": f"/data/images/{r['page_id']}.jpg",
            "variant": "deu_latf", "scale": 1.0, "timeout": 3600}) + "\n" for r in shard))
        cmd = podman("tesseract", sample, work, jobs, d / f"results-{i}.jsonl", d / "out")
        procs.append(subprocess.Popen(cmd))
    codes = [p.wait() for p in procs]
    wall = time.time() - t0
    if any(codes):
        raise SystemExit(f"throughput x{n}: a container failed (exit codes {codes}); no row written")
    results = [json.loads(x) for i in range(n) if (d / f"results-{i}.jsonl").exists()
               for x in (d / f"results-{i}.jsonl").read_text().splitlines() if x.strip()]
    ok = sum(r["status"] == "ok" for r in results)
    if ok != len(manifest):
        raise SystemExit(f"throughput x{n}: {ok} of {len(manifest)} pages ok; no row written")
    row = {"containers": n, "pages": ok, "wall_seconds": round(wall, 1),
           "pages_per_day": round(ok / wall * 86400), "engine": "tesseract", "variant": "deu_latf",
           "threads_per_container": 1, "finished": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())}
    with (work / "results" / "throughput.jsonl").open("a") as f:
        f.write(json.dumps(row) + "\n")
    print(row, flush=True)


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--sample", type=Path, required=True)
    ap.add_argument("--work", type=Path, required=True)
    ap.add_argument("--engines", default="tesseract,doctr,ndnp-open-ocr,americanstories,surya")
    ap.add_argument("--throughput", type=int, default=0, help="containers at once for the throughput test")
    ap.add_argument("--throughput-pages", type=int, default=8)
    ap.add_argument("--parallel", type=int, default=1, help="containers at once for an engine's jobs")
    ap.add_argument("--pages-per-group", type=int, default=None, help="only the first K pages of each group")
    ap.add_argument("--env", action="append", default=[],
                    help="KEY=VALUE for the engine containers, e.g. OMP_NUM_THREADS=4 (repeatable)")
    ap.add_argument("--memory", default=None, help="memory limit per container (default: 7g over --parallel)")
    a = ap.parse_args()
    ENV.extend(a.env)
    for sub in ("jobs", "results", "out"):
        (a.work / sub).mkdir(parents=True, exist_ok=True)
    if a.throughput:
        throughput(a.sample, a.work, a.throughput, a.throughput_pages)
        return
    manifest = load_manifest(a.sample)
    failed = []
    for engine in filter(None, a.engines.split(",")):
        done = set()
        for res in (a.work / "results").glob(f"{engine}*.jsonl"):
            done |= {json.loads(x)["job_id"] for x in res.read_text().splitlines() if x.strip()}
        jobs = [dict(j, concurrency=a.parallel) for j in jobs_for(engine, manifest, a.pages_per_group)
                if j["job_id"] not in done]
        out = a.work / "out" / engine
        out.mkdir(parents=True, exist_ok=True)
        print(f"{engine}: {len(jobs)} jobs to run, {len(done)} done", flush=True)
        if a.parallel == 1:
            jp = a.work / "jobs" / f"{engine}.jsonl"
            jp.write_text("".join(json.dumps(j) + "\n" for j in jobs))
            rc = subprocess.run(podman(engine, a.sample, a.work, jp, a.work / "results" / f"{engine}.jsonl", out,
                                       memory=a.memory or "7g")).returncode
            if rc:
                failed.append(f"{engine}: exit {rc}")
            continue
        procs = []
        for i in range(a.parallel):
            jp = a.work / "jobs" / f"{engine}.{i}.jsonl"
            jp.write_text("".join(json.dumps(j) + "\n" for j in jobs[i::a.parallel]))
            procs.append(subprocess.Popen(podman(engine, a.sample, a.work, jp,
                                                 a.work / "results" / f"{engine}.{i}.jsonl", out,
                                                 memory=a.memory or f"{max(1, 7 // a.parallel)}g")))
        codes = [p.wait() for p in procs]
        if any(codes):
            failed.append(f"{engine}: exit codes {codes}")
    if failed:  # the other engines still ran; rerunning picks up where each stopped
        raise SystemExit("container failures: " + "; ".join(failed))


if __name__ == "__main__":
    main()
