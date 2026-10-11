import type { ReactNode } from "react";
import type { OcrJa, Sources, Status, Versions, YearPages } from "../api/types";
import { languageName } from "../lib/languages";
import { count, when } from "./format";
import { auditRate, lastUpdate, localDate, longDate, ocrAudit, ocrExperiment } from "./now";
import { LanguagesSection, SortableTable, StatesSection, type Column } from "./PagesTables";
import { Bar, Ext, Table, Time, Unavailable } from "./parts";
import {
  byDecade,
  dayDate,
  featureChanges,
  pagesChange,
  publishedVersions,
  reasonLabel,
  statesWithPages,
  type DecadePages,
} from "./datasets";

/**
 * The sections of the dataset details page that say what the site searches
 * (07 §7.10): what's in the index, each dataset, the pages we read
 * ourselves, languages, places and versions. Every number comes from
 * `/v1/status` or `/v1/versions`; the rest is fixed text.
 */

const REPO = "https://github.com/tgoodyear/usnewsmap.com";
const LANGUAGE_REPORT = `${REPO}/tree/main/docs/reports/2026-10-language-analysis`;
const OVERRIDES = `${REPO}/tree/main/catalog/overrides`;
const issue = (n: number) => `${REPO}/issues/${n}`;

const plural = (n: number, one: string, many: string) => `${count(n)} ${n === 1 ? one : many}`;

/** 1. What's in the index: the totals, then pages by decade. */
export function InTheIndex({ s }: { s: Status }) {
  const p = s.published;
  const states = statesWithPages(s);
  const years = p.by_year;
  return (
    <section aria-labelledby="in-the-index" className="status-section">
      <h2 id="in-the-index">What&apos;s in the index</h2>
      <p className="status-lead">
        The site searches {plural(p.pages, "newspaper page", "newspaper pages")} from{" "}
        {plural(p.titles, "newspaper", "newspapers")} published in {plural(p.places, "place", "places")}
        {states !== null && <> in {plural(states, "state or territory", "states and territories")}</>}, dated{" "}
        {longDate(p.bounds.from)} to {longDate(p.bounds.to)}
        {years && years.length > 0 && <>, with pages from {plural(years.length, "year", "different years")}</>}.
      </p>
      <p className="status-meta">
        This is the version of the search index that went live {localDate(lastUpdate(s))} (
        <code>{p.index_version}</code>).
      </p>
      {years && years.length > 0 && <Coverage years={years} />}
    </section>
  );
}

const W = 960;
const H = 140;

/** Pages by decade as bars; the table under it has the same numbers. */
function Coverage({ years }: { years: YearPages[] }) {
  const decades = byDecade(years);
  const max = Math.max(1, ...decades.map((d) => d.pages));
  const peak = decades.reduce<DecadePages>((a, d) => (d.pages > a.pages ? d : a), decades[0]!);
  const bw = W / Math.max(decades.length, 1);
  const first = decades[0]!.decade;
  const last = decades[decades.length - 1]!.decade;
  const summary =
    first === last
      ? `Pages by decade: all ${count(peak.pages)} are from the ${first}s.`
      : `Pages by decade, ${first}s to ${last}s; the most are from the ${peak.decade}s, ${count(peak.pages)} pages.`;
  return (
    <section aria-labelledby="coverage" className="status-subsection">
      <h3 id="coverage">Coverage over time</h3>
      <figure className="throughput coverage">
        <div className="throughput__plot">
          <span className="throughput__max" aria-hidden="true">
            {count(max)}
          </span>
          <svg viewBox={`0 0 ${W} ${H}`} preserveAspectRatio="none" role="img" aria-label={summary}>
            <line className="throughput__base" x1={0} x2={W} y1={H - 0.5} y2={H - 0.5} />
            {decades.map((d, i) => {
              const bh = d.pages > 0 ? Math.max(2, (d.pages / max) * (H - 4)) : 0;
              return (
                <rect
                  key={d.decade}
                  className="throughput__bar"
                  x={i * bw + 1}
                  y={H - bh}
                  width={Math.max(bw - 2, 1)}
                  height={bh}
                >
                  <title>{`${d.decade}s: ${count(d.pages)} pages`}</title>
                </rect>
              );
            })}
          </svg>
        </div>
        <figcaption className="throughput__axis">
          <span>{first}s</span>
          <span>{summary}</span>
          <span>{last}s</span>
        </figcaption>
      </figure>
      <details className="status-table">
        <summary>Show the table of pages by decade</summary>
        <Table
          caption="Pages by decade"
          head={["Decade", "Pages"]}
          numeric={[1]}
          empty="No pages."
          rows={decades.map((d) => [`${d.decade}s`, count(d.pages)])}
        />
      </details>
    </section>
  );
}

/** What one dataset is, where it's from, and how it's searched. */
function Facts({ items }: { items: [string, ReactNode][] }) {
  return (
    <dl className="dataset-facts">
      {items.map(([k, v]) => (
        <div key={k}>
          <dt>{k}</dt>
          <dd>{v}</dd>
        </div>
      ))}
    </dl>
  );
}

/** 2. The datasets, one subsection each. */
export function TheDatasets({ s, now }: { s: Status; now: number }) {
  const sources = s.published.sources;
  return (
    <section aria-labelledby="datasets" className="status-section">
      <h2 id="datasets">The datasets</h2>
      <p>
        Every page on the map is a page of Chronicling America. Searches read its text from up to three sources: the
        Library of Congress&apos;s own text, American Stories&apos; reading of the same pages, and our own reading of
        pages whose text is missing or badly read.
      </p>
      <ChroniclingAmerica s={s} />
      <AmericanStories as={sources?.american_stories ?? null} known={sources !== undefined} />
      <JapanesePages s={s} now={now} />
      <section aria-labelledby="ds-later" className="status-subsection">
        <h3 id="ds-later">More to come</h3>
        <p>
          Records of Congress (the Congressional Globe, the Annals of Congress, the Register of Debates and the Serial
          Set) are planned as a second collection, searched apart from the newspapers. They will be listed here when
          they go live (<Ext href={issue(229)}>#229</Ext>).
        </p>
      </section>
    </section>
  );
}

function ChroniclingAmerica({ s }: { s: Status }) {
  const p = s.published;
  return (
    <section aria-labelledby="ds-loc" className="status-subsection">
      <h3 id="ds-loc">Chronicling America</h3>
      <Facts
        items={[
          [
            "What it is",
            <>
              Historic American newspapers digitized by the National Digital Newspaper Program, a partnership of the
              National Endowment for the Humanities and the Library of Congress. Each page comes with text the Library
              of Congress made from its scan by optical character recognition (OCR).
            </>,
          ],
          [
            "Source",
            <>
              <Ext href="https://chroniclingamerica.loc.gov/">Chronicling America</Ext>, at the Library of Congress
            </>,
          ],
          [
            "Rights",
            <>
              The Library of Congress&apos;s{" "}
              <Ext href="https://www.loc.gov/collections/chronicling-america/about-this-collection/rights-and-access/">
                rights and access statement
              </Ext>
            </>,
          ],
          [
            "In the index",
            <>
              {plural(p.pages, "page", "pages")} from {plural(p.titles, "newspaper", "newspapers")}, dated{" "}
              {longDate(p.bounds.from)} to {longDate(p.bounds.to)}
            </>,
          ],
          [
            "How it's searched",
            <>
              Every search reads the Library of Congress&apos;s text, and each result links to the page image at the
              Library of Congress.
            </>,
          ],
        ]}
      />
    </section>
  );
}

function AmericanStories({ as, known }: { as: Sources["american_stories"]; known: boolean }) {
  return (
    <section aria-labelledby="ds-american-stories" className="status-subsection">
      <h3 id="ds-american-stories">American Stories</h3>
      <Facts
        items={[
          [
            "What it is",
            <>
              A second reading of most of Chronicling America&apos;s scans (Dell et al. 2023), made with a model that
              reads each page&apos;s layout, article by article.
            </>,
          ],
          [
            "Source",
            <>
              <Ext href="https://huggingface.co/datasets/dell-research-harvard/AmericanStories">
                American Stories on Hugging Face
              </Ext>
              ; paper: <Ext href="https://arxiv.org/abs/2308.12477">arXiv:2308.12477</Ext>
            </>,
          ],
          [
            "Licence",
            <Ext key="l" href="https://creativecommons.org/licenses/by/4.0/">
              CC BY 4.0
            </Ext>,
          ],
          ["In the index", americanStoriesPages(as, known)],
          [
            "How it's searched",
            as && !as.searched ? (
              "This server has its text switched off, so searches read the Library of Congress text alone."
            ) : (
              <>
                {as ? "A search finds" : "Once its text is in the index, a search will find"} a page when either text
                has the words. When a page matched only in American Stories&apos; text, the result says so with an
                &quot;American Stories OCR&quot; badge, and the search&apos;s page count says how many pages that is.
              </>
            ),
          ],
        ]}
      />
    </section>
  );
}

function americanStoriesPages(as: Sources["american_stories"], known: boolean): ReactNode {
  if (!known) return "This server doesn't say.";
  if (!as) return "Not yet: the search index the site uses now doesn't have its text.";
  if (as.pages === null)
    return "Its text is in the index. The count of pages with it is shown after the next full rebuild of the index.";
  return (
    <>
      {plural(as.pages, "page has", "pages have")} its text
      {as.only_pages !== null && as.only_pages > 0 && (
        <>
          , {plural(as.only_pages, "page", "pages")} of them with no Library of Congress text
          {as.searched && ", searchable only through American Stories"}
        </>
      )}
      {as.years !== null && <> ({plural(as.years, "year", "years")} of its text read)</>}
    </>
  );
}

function JapanesePages({ s, now }: { s: Status; now: number }) {
  const line = ocrExperiment(s, now);
  const o = s.ocr_ja?.available ? s.ocr_ja : null;
  const ja = s.published.ja;
  const latin = s.published.sources?.ja_latin ?? null;
  const mixed = latin?.by_reason.find((r) => r.reason === "mixed")?.pages ?? 0;
  return (
    <section aria-labelledby="ds-japanese" className="status-subsection">
      <h3 id="ds-japanese">Our OCR of the Japanese-language pages</h3>
      <Facts
        items={[
          [
            "What it is",
            <>
              The Library of Congress has no searchable text for most pages of its Japanese-language newspapers, mostly
              those of the World War II incarceration camps. We read those page images ourselves with NDLOCR-Lite,
              text-recognition software from Japan&apos;s National Diet Library.
            </>,
          ],
          [
            "Source",
            <>
              The page images at the Library of Congress, read with{" "}
              <Ext href="https://github.com/ndl-lab/ndlocr-lite">NDLOCR-Lite</Ext>
            </>,
          ],
          [
            "Rights",
            <>
              For the page images, the Library of Congress&apos;s{" "}
              <Ext href="https://www.loc.gov/collections/chronicling-america/about-this-collection/rights-and-access/">
                rights and access statement
              </Ext>
              . The text is our own reading of them with NDLOCR-Lite, which the National Diet Library releases under{" "}
              <Ext href="https://creativecommons.org/licenses/by/4.0/">CC BY 4.0</Ext>.
            </>,
          ],
          [
            "In the index",
            ja || latin ? (
              <>
                {ja ? `${plural(ja.pages, "Japanese page", "Japanese pages")} in an index of their own` : null}
                {ja && latin ? "; " : null}
                {latin
                  ? `the English and other Latin-script text we read on ${plural(latin.pages, "page", "pages")} in the main search${
                      mixed > 0
                        ? `, ${plural(mixed, "page", "pages")} of them where the Library of Congress text was partly readable`
                        : ""
                    }`
                  : null}
              </>
            ) : (
              "Not yet: they become searchable with the next update of the index."
            ),
          ],
          [
            "How it's searched",
            <>
              A search with any Japanese character searches only the Japanese pages, by character. Other searches read
              the Latin-script text we found on them along with the Library of Congress text. Results from our OCR carry
              an &quot;Our OCR&quot; badge.
            </>,
          ],
        ]}
      />
      {line && <JapaneseProgress line={line} o={o} now={now} />}
    </section>
  );
}

function JapaneseProgress({
  line,
  o,
  now,
}: {
  line: NonNullable<ReturnType<typeof ocrExperiment>>;
  o: OcrJa | null;
  now: number;
}) {
  return (
    <>
      <p className="status-ocr__line">{line.text}</p>
      {line.progress && <Bar value={line.progress.done} max={line.progress.total} label={line.progress.label} />}
      <p>{line.searchable}</p>
      {o && (
        <p className="status-meta">
          Last report <Time iso={o.updated_at} now={now} />
          {o.engine && <> · {o.engine.replace(/^(\S+ [0-9a-f]{7})[0-9a-f]+$/, "$1")}</>}
        </p>
      )}
    </>
  );
}

const REASON_COLUMNS: Column<{ reason: string; pages: number }>[] = [
  { key: "reason", label: "Why we read it", value: (r) => reasonLabel(r.reason) },
  { key: "pages", label: "Pages", numeric: true, value: (r) => r.pages, cell: (r) => count(r.pages) },
];

const TITLE_COLUMNS: Column<{ lccn: string; name: string; pages: number }>[] = [
  { key: "name", label: "Newspaper", value: (r) => r.name },
  { key: "lccn", label: "LCCN", value: (r) => r.lccn },
  { key: "pages", label: "Pages", numeric: true, value: (r) => r.pages, cell: (r) => count(r.pages) },
];

const ENGINE_COLUMNS: Column<{ source: string; engine: string; pages: number }>[] = [
  { key: "engine", label: "Software", value: (r) => r.engine },
  { key: "pages", label: "Pages", numeric: true, value: (r) => r.pages, cell: (r) => count(r.pages) },
];

/** 3. Pages we read ourselves, and why. */
export function OurOcr({ s, now }: { s: Status; now: number }) {
  const sources = s.published.sources;
  const latin = sources?.ja_latin ?? null;
  const local = sources?.ocr_local ?? null;
  const audit = ocrAudit(s, now);
  return (
    <section aria-labelledby="own-ocr" className="status-section">
      <h2 id="own-ocr">Pages we read ourselves</h2>
      <p>
        The Library of Congress&apos;s text is all a search has to go on, so a page whose text is missing or badly read
        can&apos;t be found. We read such a page again from its image: on the Japanese-language newspapers, when the
        Library of Congress has no text for it, or its text is empty, too short, garbled or only partly readable; on the
        others, when its text misreads many common words.
      </p>
      {sources === undefined ? (
        <p className="status-empty">This server doesn&apos;t say how many pages that is.</p>
      ) : !latin && !local ? (
        <p className="status-empty">The search index the site uses now has none of our own text in place of theirs.</p>
      ) : null}
      {latin && (
        <section aria-labelledby="own-ocr-japanese" className="status-subsection">
          <h3 id="own-ocr-japanese">Japanese-language newspapers</h3>
          <p>
            {plural(latin.pages, "page", "pages")} of {plural(latin.by_title.length, "newspaper", "newspapers")} are
            searched with the Latin-script text of our Japanese OCR.
          </p>
          <details className="status-table">
            <summary>Show the tables by reason and by newspaper</summary>
            <SortableTable
              caption="Pages we read, by reason"
              columns={REASON_COLUMNS}
              rows={latin.by_reason}
              rowKey={(r) => r.reason}
              initial={{ key: "pages", desc: true }}
            />
            <SortableTable
              caption="Pages we read, by newspaper"
              columns={TITLE_COLUMNS}
              rows={latin.by_title}
              rowKey={(r) => r.lccn}
              initial={{ key: "pages", desc: true }}
            />
          </details>
        </section>
      )}
      {local && (
        <section aria-labelledby="own-ocr-other" className="status-subsection">
          <h3 id="own-ocr-other">Other newspapers</h3>
          <p>
            On {plural(local.pages, "page", "pages")} our reading replaced the Library of Congress&apos;s, where ours is
            in the same language and misreads clearly fewer common words.
          </p>
          <details className="status-table">
            <summary>Show the table by OCR software</summary>
            <SortableTable
              caption="Pages with our text, by OCR software"
              columns={ENGINE_COLUMNS}
              rows={local.by_engine}
              rowKey={(r) => `${r.source} ${r.engine}`}
              initial={{ key: "pages", desc: true }}
            />
          </details>
        </section>
      )}
      {audit && <OcrAudit audit={audit} s={s} />}
    </section>
  );
}

/** The OCR quality audit: progress, then the summary by language. */
function OcrAudit({ audit, s }: { audit: NonNullable<ReturnType<typeof ocrAudit>>; s: Status }) {
  const q = s.ocr_quality?.available ? s.ocr_quality : null;
  const rows = q?.finished_at ? (q.summary?.languages ?? []) : [];

  return (
    <section aria-labelledby="ocr-audit" className="status-subsection">
      <h3 id="ocr-audit">How well the Library of Congress text reads</h3>
      <p>
        To find the pages worth reading again, we check how readable the Library of Congress&apos;s text is. For a
        sample of pages, we detect each page&apos;s language and count how often that language&apos;s 20 most common
        words are misread by one letter (&quot;tbe&quot; for &quot;the&quot;, &quot;ift&quot; for German
        &quot;ist&quot;) instead of read correctly. The table calls this the misread rate.
      </p>
      <p className="status-ocr__line">{audit.text}</p>
      {audit.progress && <Bar value={audit.progress.done} max={audit.progress.total} label={audit.progress.label} />}
      {audit.agreement && <p>{audit.agreement}</p>}
      {rows.length > 0 && (
        <Table
          caption="Misread rate by page language"
          head={["Language", "Pages sampled", "Misread rate, median page", "Pages with misread rate over 10%"]}
          numeric={[1, 2, 3]}
          empty="No languages."
          rows={rows.map((r) => [
            languageName(r.language),
            count(r.pages),
            auditRate(r.damage_rate_median),
            auditRate(r.damaged_share),
          ])}
        />
      )}
      {rows.some((r) => r.damage_rate_median === null) && (
        <p className="status-note">– means we have no word list for that language.</p>
      )}
    </section>
  );
}

/** 4. Languages: by catalog language now, by page language later. */
export function Languages({ s }: { s: Status }) {
  const l = s.published.by_language;
  return (
    <section aria-labelledby="languages" className="status-section">
      <h2 id="languages">Languages</h2>
      <p>
        Pages are counted by the languages the Library of Congress catalogs for their newspaper, so every page of a
        newspaper has its languages. The next full rebuild of the index detects each page&apos;s own language, and this
        section will count pages by it then (<Ext href={issue(283)}>#283</Ext>). The{" "}
        <Ext href={LANGUAGE_REPORT}>language report</Ext> describes each language&apos;s pages, where their text comes
        from and how well it reads.
      </p>
      {l ? <LanguagesSection data={l} /> : <p className="status-empty">This server doesn&apos;t list languages.</p>}
    </section>
  );
}

/** 5. Places: how papers are placed, the corrections and the known gaps. */
export function Places({ s }: { s: Status }) {
  const p = s.published;
  return (
    <section aria-labelledby="places" className="status-section">
      <h2 id="places">Places</h2>
      <p>
        Each newspaper is on the map at the town the Library of Congress says it was published in, and all its pages are
        counted there. The town&apos;s point comes from our own corrections first, then from the Library of
        Congress&apos;s records and the US Census Bureau&apos;s list of places; a town none of them places is put at the
        middle of its state. Towns placed only roughly, at a county or a state, have a hollow marker.
      </p>
      <p>
        Our corrections are in <Ext href={OVERRIDES}>catalog/overrides</Ext> in the site&apos;s source code. Each
        records when it was made, why, its source, and the issue or pull request where it was decided, and keeps its
        earlier values when it changes.
      </p>
      <p>
        Known gaps: a few towns couldn&apos;t be placed from the sources we checked, and some newspapers that moved are
        still shown at the town in their title rather than where most of their pages were printed, until the next full
        rebuild. The checks and their sources are on <Ext href={issue(235)}>#235</Ext>.
      </p>
      {p.by_state && <StatesSection rows={p.by_state} pages={p.pages} />}
    </section>
  );
}

/** 7. Versions: the published index versions, newest first, and what each changed. */
export function VersionsSection({
  versions,
  error,
  now,
}: {
  versions: Versions | undefined;
  error: boolean;
  now: number;
}) {
  return (
    <section aria-labelledby="versions" className="status-section">
      <h2 id="versions">Versions</h2>
      <p>
        The site searches one version of the index at a time. An update adds new pages to it; a full rebuild makes the
        whole index again, and is when changes to every page, such as a new source of text, take effect.
      </p>
      {!versions ? (
        error ? (
          <Unavailable reason="The list of versions could not be loaded." />
        ) : (
          <p role="status">Loading…</p>
        )
      ) : !versions.available ? (
        <Unavailable reason={versions.reason ?? "The list of versions isn't available on this server."} />
      ) : (
        <VersionsTable versions={versions} now={now} />
      )}
    </section>
  );
}

function VersionsTable({ versions, now }: { versions: Versions; now: number }) {
  const list = publishedVersions(versions.versions);
  return (
    <>
      <p className="status-meta">
        From the pipeline&apos;s records, read <Time iso={versions.read_at} now={now} />.
      </p>
      <Table
        caption="Published versions, newest first"
        head={["Version", "Went live", "Kind", "Pages", "Change in pages", "What changed"]}
        numeric={[3, 4]}
        empty="No version has been published yet."
        rows={list.map(({ v, before }) => {
          const changes = featureChanges(v, before);
          return [
            <span key="v">
              <code>{v.index_version}</code>
              {v.serving && <> (the site searches this one)</>}
            </span>,
            <time key="t" dateTime={v.published_at!} title={when(v.published_at!)}>
              {dayDate(v.published_at!)}
            </time>,
            v.full ? "Full rebuild" : "Update",
            count(v.pages),
            pagesChange(v, before),
            changes.length > 0 ? changes.join("; ") : "–",
          ];
        })}
      />
    </>
  );
}

/** "The last update went live …": the newest published version and what it added. */
export function lastPublished(versions: Versions | undefined): string | null {
  if (!versions?.available) return null;
  const [newest] = publishedVersions(versions.versions);
  if (!newest) return null;
  const { v, before } = newest;
  const kind = v.full ? "full rebuild" : "update";
  const change = before ? v.pages - before.pages : null;
  const added =
    change === null
      ? ""
      : change > 0
        ? `, adding ${plural(change, "page", "pages")}`
        : change < 0
          ? `, with ${plural(-change, "page", "pages")} fewer`
          : ", with the same number of pages";
  const features = featureChanges(v, before);
  return `The last ${kind}, ${v.index_version}, went live on ${dayDate(v.published_at!)}: ${plural(v.pages, "page", "pages")}${added}.${
    features.length > 0 ? ` ${features.join("; ")}.` : ""
  }`;
}
