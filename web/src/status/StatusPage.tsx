import { useEffect, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { api, API_BASE } from "../api/client";
import type { Backfill, HourBin, Indexing, Status, TitlesPipeline, Versions } from "../api/types";
import { TITLES } from "../route";
import { count, health, relative, span, when } from "./format";
import { STATE_LABEL, headline, rightNow, steps, localDate } from "./now";
import { TERMS } from "./terms";
import { Brand } from "../components/Brand";
import { InTheIndex, Languages, OurOcr, Places, TheDatasets, VersionsSection, lastPublished } from "./Datasets";
import { Bar, Stats, Table, Time, Unavailable } from "./parts";

const REFRESH_MS = 30_000;

/**
 * `/datasets`: what the site searches, where its text comes from, and what is
 * being added now, for anyone (no sign-in). It reads `/v1/status` and
 * `/v1/versions` (07 §7.10).
 */
export default function DatasetsPage() {
  const status = useQuery({
    queryKey: ["status"],
    queryFn: ({ signal }) => api.status(signal),
    refetchInterval: REFRESH_MS,
    staleTime: 0,
  });
  const versions = useQuery({
    queryKey: ["versions"],
    queryFn: ({ signal }) => api.versions(signal),
    refetchInterval: REFRESH_MS,
    staleTime: 0,
  });
  // Relative times ("5 min ago") keep moving between refreshes.
  const [tick, setTick] = useState(() => Date.now());
  useEffect(() => {
    const timer = setInterval(() => setTick(Date.now()), 15_000);
    return () => clearInterval(timer);
  }, []);
  const now = Math.max(tick, status.dataUpdatedAt);
  useEffect(() => {
    document.title = TITLES.datasets;
  }, []);

  const s = status.data;
  return (
    <div className="app status-page">
      <header className="topbar">
        <Brand />
      </header>
      <main className="status">
        <h1>Dataset details</h1>
        <p className="status-intro">
          What the site searches, where its text comes from, and what is being added. The numbers are those of the
          search index the site uses now, and the page refreshes every 30 seconds.
        </p>
        {status.error && (
          <p className="notice notice--error" role="alert">
            The details could not be loaded{s ? "; showing the last copy" : ""}. They are retried every 30 seconds.
          </p>
        )}
        {!s && !status.error && <p role="status">Loading…</p>}
        {s?.published.synthetic && (
          <p className="notice notice--demo" role="note">
            Demo data: this server serves a small synthetic set of pages.
          </p>
        )}
        {s && <InTheIndex s={s} />}
        {s && <TheDatasets s={s} now={now} />}
        {s && <OurOcr s={s} now={now} />}
        {s && <Languages s={s} />}
        {s && <Places s={s} />}
        {s && <InProgress s={s} now={now} versions={versions.data} />}
        {s && <VersionsSection versions={versions.data} error={versions.isError} now={now} />}
        {s && <Technical s={s} now={now} />}
      </main>
      <footer className="credits">
        <a href="/">Search</a> · Newspaper pages from{" "}
        <a href="https://chroniclingamerica.loc.gov/" target="_blank" rel="noopener noreferrer">
          Chronicling America
        </a>{" "}
        (
        <a href="https://www.loc.gov/ndnp/" target="_blank" rel="noopener noreferrer">
          NEH and Library of Congress
        </a>
        ). <a href="/privacy">Privacy</a>
      </footer>
    </div>
  );
}

/** 6. In progress: what the pipeline is doing now, how far it has got, and the last update. */
function InProgress({ s, now, versions }: { s: Status; now: number; versions: Versions | undefined }) {
  const last = lastPublished(versions);
  return (
    <section aria-labelledby="in-progress" className="status-section">
      <h2 id="in-progress">In progress</h2>
      <RightNow s={s} now={now} />
      {last && <p>{last}</p>}
      <Headline s={s} />
      <Steps s={s} now={now} />
    </section>
  );
}

/** "Right now": the pipeline's current activity in one plain sentence. */
function RightNow({ s, now }: { s: Status; now: number }) {
  const line = rightNow(s, now);
  return (
    <section aria-labelledby="right-now" className="status-subsection status-now">
      <h3 id="right-now">Right now</h3>
      <p className="status-now__line">{line.text}</p>
      {line.progress && <Bar value={line.progress.done} max={line.progress.total} label={line.progress.label} />}
      {line.notes.map((n) => (
        <p key={n} className="status-now__note">
          {n}
        </p>
      ))}
      <p className="status-meta">
        Checked <time dateTime={s.generated_at}>{relative(s.generated_at, now)}</time>
        {s.pipeline.read_at && s.stale && (
          <>
            {" "}
            · <span className="badge badge--problem">Out of date</span>: the pipeline couldn&apos;t be read, so this
            shows what it was doing <time dateTime={s.pipeline.read_at}>{relative(s.pipeline.read_at, now)}</time>
          </>
        )}{" "}
        · this page refreshes every 30 s
      </p>
    </section>
  );
}

/** "Searchable now: X of Y pages (Z%)". */
function Headline({ s }: { s: Status }) {
  const h = headline(s);
  return (
    <section aria-labelledby="searchable-now" className="status-subsection status-headline">
      <h3 id="searchable-now" className="status-headline__text">
        {h.text}
      </h3>
      {h.share !== null && (
        <Bar
          // Copies against copies: the published pages plus the duplicate copies they left out.
          value={s.published.pages + (s.published.duplicate_pages ?? 0)}
          max={s.backfill.available ? s.backfill.pages : s.published.pages}
          label={h.text}
        />
      )}
      <p>
        {h.sub} {h.share !== null && h.share < 1 && "The rest are downloaded and on their way through the steps below."}
      </p>
    </section>
  );
}

/** The four steps a page goes through, as an ordered list. */
function Steps({ s, now }: { s: Status; now: number }) {
  const list = steps(s, now);
  return (
    <section aria-labelledby="steps" className="status-subsection">
      <h3 id="steps">How pages get onto the map</h3>
      <ol className="steps">
        {list.map((st, i) => {
          const current = st.state === "active" || st.state === "paused";
          return (
            <li
              key={st.key}
              className={`step step--${st.state}${current ? " step--current" : ""}`}
              aria-current={current ? "step" : undefined}
            >
              <span className="step__marker" aria-hidden="true">
                {st.state === "done" ? "✓" : i + 1}
              </span>
              <div className="step__body">
                <h4 className="step__title">
                  <span className="visually-hidden">Step {i + 1}: </span>
                  {st.title}
                </h4>
                <p className="step__state">
                  <span className={`step-badge step-badge--${st.state}`}>{STATE_LABEL[st.state]}</span>{" "}
                  <span className="step__detail">{st.detail}</span>
                </p>
                <p className="step__explain">{st.explain}</p>
              </div>
            </li>
          );
        })}
      </ol>
    </section>
  );
}

/** Operator details, collapsed: the pipeline's own numbers and terms. */
function Technical({ s, now }: { s: Status; now: number }) {
  const h = health(s, now);
  return (
    <section aria-labelledby="technical" className="status-section">
      <h2 id="technical">Technical details</h2>
      <p>For the people who run the site: the pipeline&apos;s own numbers, in its own terms.</p>
      <details className="status-technical">
        <summary>Show the pipeline&apos;s own numbers and terms</summary>
        <p className="status-note">
          These use the pipeline&apos;s internal names; the terms are explained first. Summary: {h.parts.join(" · ")}.
        </p>
        {s.error && (
          <p className="notice notice--error" role="note">
            The last attempt to read the pipeline state failed: {s.error}
          </p>
        )}
        <h3>Terms</h3>
        <dl className="terms">
          {TERMS.map(([term, meaning]) => (
            <div key={term}>
              <dt>{term}</dt>
              <dd>{meaning}</dd>
            </div>
          ))}
        </dl>
        <ActivityDetails s={s} now={now} />
        <BackfillSection s={s} now={now} />
        <IndexingSection s={s} now={now} />
        <TitlesSection s={s} />
        <h3>Raw response</h3>
        <p>
          The page reads <a href={`${API_BASE}/v1/status`}>/v1/status</a> (JSON, schema {s.schema}). The API reads the
          pipeline state at most once a minute.
        </p>
        <details>
          <summary>Raw response</summary>
          <pre className="status-json" tabIndex={0} aria-label="Raw /v1/status response">
            {JSON.stringify(s, null, 2)}
          </pre>
        </details>
      </details>
    </section>
  );
}

function ActivityDetails({ s, now }: { s: Status; now: number }) {
  const a = s.activity;
  return (
    <>
      <h3>Current activity</h3>
      {!a ? (
        <Unavailable reason="This API doesn't report its activity." />
      ) : !a.available ? (
        <Unavailable reason={a.reason} />
      ) : (
        <>
          <Stats
            items={[
              ["Step", a.now],
              [
                "Reported by",
                a.source === "job"
                  ? "the ingest job"
                  : a.source === "inferred"
                    ? "inferred from locks, leases and caches"
                    : "nothing running",
              ],
              ["Execution", a.run ?? "–"],
              ["Execution started", <Time key="r" iso={a.run_started_at} now={now} />],
              ["Step started", <Time key="s" iso={a.since} now={now} />],
              ["Last report", <Time key="p" iso={a.reported_at} now={now} />],
              ["Progress", a.done !== null && a.total !== null ? `${count(a.done)} of ${count(a.total)}` : "–"],
              ["Estimated finish", <Time key="e" iso={a.eta} now={now} />],
              ["Paused until", <Time key="u" iso={a.paused_until} now={now} />],
              [
                "Merges",
                a.merge
                  ? `${a.merge.step}: ${count(a.merge.splits)} splits, ${a.merge.merges_running} running, ${a.merge.merges_queued} queued`
                  : "–",
              ],
              ["Next scheduled run", a.next_run ? localDate(a.next_run) : "none"],
            ]}
          />
          {a.last && (
            <p>
              <strong>Last run:</strong> {a.last.outcome.replace("_", " ")} <Time iso={a.last.ended_at} now={now} />
              {a.last.index_version && (
                <>
                  {" "}
                  (<code>{a.last.index_version}</code>)
                </>
              )}
              {a.last.error && (
                <>
                  : <ErrorText text={a.last.error} />
                </>
              )}
            </p>
          )}
        </>
      )}
    </>
  );
}

function BackfillSection({ s, now }: { s: Status; now: number }) {
  const b = s.backfill;
  return (
    <section aria-labelledby="backfill" className="status-subsection">
      <h3 id="backfill">Downloads (backfill: curating batches)</h3>
      {!b.available ? <Unavailable reason={b.reason} /> : <BackfillBody b={b} now={now} />}
    </section>
  );
}

function BackfillBody({ b, now }: { b: Backfill; now: number }) {
  const t = b.throughput;
  const label = `${count(b.by_status.curated)} of ${count(b.total)} batches curated (${b.percent}%)`;
  return (
    <>
      <p className="status-lead">{label}</p>
      <Bar value={b.by_status.curated} max={b.total} label={label} />
      <Stats
        items={[
          ["Queued", count(b.by_status.queued)],
          ["In progress", count(b.in_progress)],
          ["Curated", count(b.by_status.curated)],
          ["Failed", count(b.by_status.failed)],
          ["Retrying", count(b.retrying)],
          ["Stopped workers' leases", count(b.stale_leases)],
          ["Pages curated", count(b.pages)],
          ["Pages with text", count(b.ok_pages)],
          [`Rate (last ${t.rate_window_hours} h)`, `${t.rate_per_hour} batches/h`],
          [
            "Estimated finish",
            t.eta ? <Time iso={t.eta} now={now} /> : t.remaining ? "unknown (no recent rate)" : "done",
          ],
          ["Newer versions waiting", count(b.newer_versions_pending)],
        ]}
      />
      <p>
        <strong>LoC downloads:</strong>{" "}
        {b.loc.throttled && b.loc.blocked_until ? (
          <>
            <span className="badge badge--attention">Throttled</span> LoC refused downloads; every worker waits until{" "}
            {when(b.loc.blocked_until)} (
            <Time iso={b.loc.blocked_until} now={now} />
            ).
          </>
        ) : b.loc.next_slot ? (
          <>
            paced across all workers (LoC allows 10 bulk downloads per 10 minutes); next slot{" "}
            <Time iso={b.loc.next_slot} now={now} />.
          </>
        ) : (
          "no download is waiting for a slot."
        )}
      </p>
      <h4>Batches curated per hour, last 48 hours</h4>
      <Throughput hours={t.hours} />
      <Table
        caption="Batches in progress"
        head={["Batch", "Worker", "Claimed", "Lease ends", "Attempt"]}
        empty="No batch is being curated right now."
        rows={b.in_progress_batches.map((a) => [
          <code key="b">{`${a.batch}_ver${String(a.version).padStart(2, "0")}`}</code>,
          a.worker ?? "–",
          <Time key="s" iso={a.since} now={now} />,
          a.lease_expired ? (
            <span key="l">
              <span className="badge badge--attention">Expired</span> <Time iso={a.lease_until} now={now} />
            </span>
          ) : (
            <Time key="l" iso={a.lease_until} now={now} />
          ),
          a.attempts,
        ])}
      />
      <Table
        caption={`Failed batches${b.by_status.failed > b.failed_batches.length ? ` (latest ${b.failed_batches.length})` : ""}`}
        head={["Batch", "Attempts", "Last error", "When"]}
        empty="No failed batches."
        rows={b.failed_batches.map((f) => [
          <code key="b">{`${f.batch}_ver${String(f.version).padStart(2, "0")}`}</code>,
          f.attempts,
          <ErrorText key="e" text={f.error} />,
          <Time key="w" iso={f.updated_at} now={now} />,
        ])}
      />
      <Table
        caption="Recently curated"
        head={["Batch", "Pages", "Curated"]}
        empty="Nothing curated yet."
        rows={b.recent.map((r) => [
          <code key="b">{`${r.batch}_ver${String(r.version).padStart(2, "0")}`}</code>,
          count(r.pages),
          <Time key="c" iso={r.curated_at} now={now} />,
        ])}
      />
    </>
  );
}

const W = 960;
const H = 120;

/** Batches curated per hour, as bars; the table below carries the same numbers. */
function Throughput({ hours }: { hours: HourBin[] }) {
  const max = Math.max(1, ...hours.map((h) => h.batches));
  const total = hours.reduce((a, h) => a + h.batches, 0);
  const peak = hours.reduce((p, h) => (h.batches > p.batches ? h : p), hours[0] ?? { start: "", batches: 0, pages: 0 });
  const bw = W / Math.max(hours.length, 1);
  const summary =
    total === 0
      ? "No batches curated in the last 48 hours."
      : `${count(total)} batches curated in the last 48 hours; the most in one hour was ${peak.batches}, starting ${when(peak.start)}.`;
  return (
    <figure className="throughput">
      <div className="throughput__plot">
        <span className="throughput__max" aria-hidden="true">
          {max}
        </span>
        <svg viewBox={`0 0 ${W} ${H}`} preserveAspectRatio="none" role="img" aria-label={summary}>
          <line className="throughput__base" x1={0} x2={W} y1={H - 0.5} y2={H - 0.5} />
          {hours.map((h, i) => {
            const bh = h.batches > 0 ? Math.max(2, (h.batches / max) * (H - 4)) : 0;
            return (
              <rect
                key={h.start}
                className="throughput__bar"
                x={i * bw + 1}
                y={H - bh}
                width={Math.max(bw - 2, 1)}
                height={bh}
              >
                <title>{`${when(h.start)}: ${h.batches} ${h.batches === 1 ? "batch" : "batches"}, ${count(h.pages)} pages`}</title>
              </rect>
            );
          })}
        </svg>
      </div>
      <figcaption className="throughput__axis">
        <span>48 h ago</span>
        <span>{summary}</span>
        <span>now</span>
      </figcaption>
      <details>
        <summary>Hourly numbers</summary>
        <Table
          caption="Batches curated per hour"
          head={["Hour starting", "Batches", "Pages"]}
          empty="No data."
          rows={hours.map((h) => [when(h.start), h.batches, count(h.pages)])}
        />
      </details>
    </figure>
  );
}

function IndexingSection({ s, now }: { s: Status; now: number }) {
  const p = s.published;
  const i = s.indexing;
  const deltaLabel = `${p.deltas} of ${p.max_deltas} deltas used`;
  return (
    <section aria-labelledby="indexing" className="status-subsection">
      <h3 id="indexing">Index builds (releases)</h3>
      <h4>Published version</h4>
      <Stats
        items={[
          ["Version", <code key="v">{p.index_version}</code>],
          ["Published", <Time key="p" iso={p.published_at} now={now} />],
          ["Pages", count(p.pages)],
          ["Newspapers", count(p.titles)],
          ["Places", count(p.places)],
          ["Dates", `${p.bounds.from} to ${p.bounds.to}`],
          ["Batches", p.batches === null ? "not recorded" : count(p.batches)],
        ]}
      />
      <p>
        <strong>Index set:</strong> base <code>{p.indexes[0]}</code>
        {p.deltas > 0 && (
          <>
            {" "}
            + {p.deltas} {p.deltas === 1 ? "delta" : "deltas"} (
            {p.indexes.slice(1).map((d, k) => (
              <span key={d}>
                {k > 0 && ", "}
                <code>{d}</code>
              </span>
            ))}
            )
          </>
        )}
        .
      </p>
      <p>
        {deltaLabel}.{" "}
        {p.next_release_full
          ? "The next release rebuilds everything into a new base index."
          : `Each release adds a delta; after ${p.max_deltas}, the next release rebuilds a single base index.`}
      </p>
      <Bar value={p.deltas} max={p.max_deltas} label={deltaLabel} />
      {!i.available ? <Unavailable reason={i.reason} /> : <IndexingBody i={i} now={now} />}
    </section>
  );
}

function IndexingBody({ i, now }: { i: Indexing; now: number }) {
  const r = i.release;
  return (
    <>
      <h4>Build in progress</h4>
      {r ? (
        <>
          <p className="status-lead">
            Building <code>{r.index_version}</code>: {count(r.docs_sent)} of {count(r.docs_expected)} pages sent (
            {r.percent}%), {count(Math.round(r.mb_sent))} MB. Updated <Time iso={r.updated_at} now={now} />.
          </p>
          <Bar value={r.docs_sent} max={r.docs_expected} label={`Release ${r.percent}% done`} />
        </>
      ) : (
        <p>
          {i.writer.held
            ? `The index writer is held (worker ${i.writer.holder ?? "?"}) but no build progress has been recorded yet.`
            : "No release is running."}
        </p>
      )}
      <Stats
        items={[
          ["Pipeline's current version", i.current_version ? <code key="c">{i.current_version}</code> : "–"],
          [
            "Writer lock",
            i.writer.held ? (
              <span key="w">
                held by {i.writer.holder}, until <Time iso={i.writer.until} now={now} />
              </span>
            ) : (
              "free"
            ),
          ],
          ["Failed runs since the last publish", count(i.failed_since_last_publish ?? i.failed_runs)],
          ["Failed runs (recent history)", count(i.failed_runs)],
        ]}
      />
      <Table
        caption="Recent index runs"
        head={["Version", "Kind", "Status", "Batches", "Pages indexed", "Started", "Took", "Error"]}
        empty="No index runs recorded."
        rows={i.runs.map((run) => [
          <code key="v">{run.index_version}</code>,
          run.full ? "full" : "delta",
          <RunBadge key="s" status={run.status} />,
          run.batches === null ? "–" : count(run.batches),
          run.docs
            ? `${count(run.docs)}${run.duplicate_pages ? ` (${count(run.duplicate_pages)} duplicates left out)` : ""}`
            : "–",
          <Time key="t" iso={run.started_at} now={now} />,
          run.duration_secs === null ? "–" : span(run.duration_secs * 1000),
          <ErrorText key="e" text={run.error} />,
        ])}
      />
    </>
  );
}

/** An error in a table cell: short ones in full, long ones cut to a line
 * that expands on tap, so one long message can't make its row page-tall on
 * a narrow screen. */
export function ErrorText({ text }: { text: string | null | undefined }) {
  if (!text) return <>–</>;
  if (text.length <= ERROR_PREVIEW) return <span className="status-error">{text}</span>;
  return (
    <details className="status-error">
      <summary>{`${text.slice(0, ERROR_PREVIEW).trimEnd()}…`}</summary>
      {text}
    </details>
  );
}

const ERROR_PREVIEW = 60;

function RunBadge({ status }: { status: string }) {
  const cls =
    status === "failed"
      ? "badge--problem"
      : status === "building"
        ? "badge--attention"
        : status === "stopped"
          ? "badge--muted"
          : "badge--ok";
  return <span className={`badge ${cls}`}>{status}</span>;
}

function TitlesSection({ s }: { s: Status }) {
  const t = s.titles;
  return (
    <section aria-labelledby="titles" className="status-subsection">
      <h3 id="titles">Newspaper catalog (titles-sync)</h3>
      <Stats
        items={[
          ["Newspapers in the catalog", t.catalog.available ? count(t.catalog.titles) : "not available"],
          ["Places in the catalog", t.catalog.available ? count(t.catalog.places) : "not available"],
          ["Newspapers published", count(t.published_titles)],
          ["Places published", count(t.published_places)],
        ]}
      />
      {!t.pipeline.available ? <Unavailable reason={t.pipeline.reason} /> : <TitlesBody t={t.pipeline} />}
    </section>
  );
}

function TitlesBody({ t }: { t: TitlesPipeline }) {
  const opt = (x: number | null) => (x === null ? "unknown" : count(x));
  return (
    <>
      <Stats
        items={[
          ["Newspapers in curated batches", count(t.curated_titles)],
          ["Waiting for titles-sync", opt(t.awaiting_sync)],
          ["Batches held back for missing titles", opt(t.batches_waiting_for_titles)],
          ["Curated batches not yet published", opt(t.unpublished_batches)],
          ["Ready for the next release", opt(t.ready_for_release)],
          ["Re-curated, waiting for a full release", opt(t.recurated_awaiting_full)],
        ]}
      />
      <p className="status-note">
        A release only takes batches whose newspapers are all in the catalog; titles-sync adds them from LoC.
      </p>
    </>
  );
}
