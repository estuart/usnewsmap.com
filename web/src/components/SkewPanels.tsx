import { useId, useState } from "react";
import { formatTimes } from "../lib/skewScale";
import { formatExpected, formatRange, type SkewInfo } from "../lib/skewText";
import { dateFromDay } from "../lib/time";
import { ShownList, type ListLengths } from "./ListLength";
import { ListsPanel } from "./PlaceLists";

export interface SkewRow {
  id: string;
  name: string;
  state: string;
  skew: SkewInfo;
  /** The place's first and last matching day in the whole search; negative when unknown. */
  firstDay?: number;
  lastDay?: number;
}

/**
 * The places that differ most clearly (doc 11, 11.6): above 1 sorted by the
 * interval's lower bound, below 1 by its upper bound. Only places whose
 * interval excludes 1, all of them: the lists choose how many to show.
 * Papers in other languages are named, not left out.
 */
export function clearest(rows: SkewRow[]): { above: SkewRow[]; below: SkewRow[] } {
  const usable = rows.filter((r) => r.skew.dir !== 0);
  return {
    above: usable.filter((r) => r.skew.dir === 1).sort((a, b) => b.skew.lower - a.skew.lower),
    below: usable.filter((r) => r.skew.dir === -1).sort((a, b) => a.skew.upper - b.skew.upper),
  };
}

/** The two Clearest differences lists, as `useListLengths` names them. */
export type SkewListName = "above" | "below";

interface ListsProps {
  rows: SkewRow[];
  onSelect: (id: string) => void;
  /** How many places each list shows, from `useListLengths`. */
  shown: ListLengths<SkewListName>;
  onShown: (list: SkewListName, n: number) => void;
}

/** The side panel when no place is selected. */
export function SkewLists({ rows, onSelect, shown, onShown }: ListsProps) {
  const { above, below } = clearest(rows);
  const item = (r: SkewRow) => (
    <>
      <button type="button" className="link-button" onClick={() => onSelect(r.id)}>
        {r.name}, {r.state}
      </button>{" "}
      <span className="skew-list__value">
        {formatTimes(r.skew.estimate)} ({formatRange(r.skew)})
      </span>
      {r.skew.languages && <LanguageNote label={r.skew.languages} counts={r.skew.languageCounts} />}
    </>
  );
  return (
    <ListsPanel title="Clearest differences" label="Places that differ most clearly" className="skew-lists">
      <ShownList
        heading="Most clearly above 1×"
        empty="No place is clearly above 1× in this window."
        items={above}
        itemKey={(r) => r.id}
        render={item}
        limit={shown.above}
        onLimit={(n) => onShown("above", n)}
      />
      <ShownList
        heading="Most clearly below 1×"
        empty="No place is clearly below 1× in this window."
        items={below}
        itemKey={(r) => r.id}
        render={item}
        limit={shown.below}
        onLimit={(n) => onShown("below", n)}
      />
      <p className="skew-list__note">Ranked by the end of each place's 90% range nearest 1×.</p>
    </ListsPanel>
  );
}

/** What a "Papers in …" label means, for the info button beside it. */
export const LANGUAGE_EXPLAINER =
  "These are the languages this place's papers are catalogued in, across the whole collection. " +
  "They don't say which language the matched pages are in. " +
  "An English search term rarely matches pages printed in another language, so a place with such papers can read low. " +
  "To compare only pages from English-language papers, choose English under newspaper languages.";

/**
 * A place's language label with a small info button that shows how many of
 * its papers are in each language and what the label means. A tap (not
 * hover), so it works on phones.
 */
export function LanguageNote({ label, counts }: { label: string; counts?: string | null }) {
  const [open, setOpen] = useState(false);
  const id = useId();
  return (
    <span className="skew-list__lang">
      <span>{label}</span>
      <button
        type="button"
        className="info-button"
        aria-label="What this means"
        aria-expanded={open}
        aria-controls={id}
        onClick={() => setOpen((o) => !o)}
        onKeyDown={(e) => {
          if (e.key === "Escape") setOpen(false);
        }}
      >
        i
      </button>
      <span id={id} role="note" className="info-tip" hidden={!open}>
        {counts && <span className="info-tip__counts">{counts}</span>}
        {LANGUAGE_EXPLAINER}
      </span>
    </span>
  );
}

interface StateTableProps {
  rows: { state: string; skew: SkewInfo }[];
}

/** The same score per state, each compared with the other states. */
export function StateTable({ rows }: StateTableProps) {
  const sorted = [...rows].filter((r) => r.skew.pages > 0).sort((a, b) => b.skew.estimate - a.skew.estimate);
  return (
    <div className="table-wrap">
      <table className="places">
        <caption>By state, each compared with the other states</caption>
        <thead>
          <tr>
            <th scope="col">State</th>
            <th scope="col" className="num">
              Pages published
            </th>
            <th scope="col" className="num">
              Matched
            </th>
            <th scope="col" className="num">
              Expected
            </th>
            <th scope="col" className="num">
              Relative rate
            </th>
            <th scope="col" className="num">
              90% range
            </th>
          </tr>
        </thead>
        <tbody>
          {sorted.map((r) => (
            <tr key={r.state}>
              <th scope="row">{r.state}</th>
              <td className="num">{r.skew.pages.toLocaleString()}</td>
              <td className="num">{r.skew.observed.toLocaleString()}</td>
              <td className="num">{formatExpected(r.skew.expected)}</td>
              <td className="num">{formatTimes(r.skew.estimate)}</td>
              <td className="num">
                {formatRange(r.skew)}
                {r.skew.dir === 0 ? " (not enough pages to tell)" : ""}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

/** Per-place CSV for the window (doc 11, 11.6). */
export function skewCsv(rows: SkewRow[]): string {
  const q = (s: string) => (/[",\n]/.test(s) ? `"${s.replace(/"/g, '""')}"` : s);
  const n = (x: number) => (Number.isFinite(x) ? String(Number(x.toPrecision(6))) : "");
  const day = (d: number | undefined) => (d !== undefined && d >= 0 ? dateFromDay(d) : "");
  const lines = ["place_id,name,state,pages,hits,expected,estimate,lower,upper,languages,first_seen,last_seen"];
  for (const r of rows) {
    const s = r.skew;
    lines.push(
      [
        q(r.id),
        q(r.name),
        q(r.state),
        s.pages,
        s.observed,
        n(s.expected),
        n(s.estimate),
        n(s.lower),
        n(s.upper),
        q(s.languages ?? ""),
        day(r.firstDay),
        day(r.lastDay),
      ].join(","),
    );
  }
  return `${lines.join("\n")}\n`;
}

export function DownloadCsv({ rows, filename }: { rows: SkewRow[]; filename: string }) {
  return (
    <button
      type="button"
      className="button"
      onClick={() => {
        const url = URL.createObjectURL(new Blob([skewCsv(rows)], { type: "text/csv" }));
        const a = document.createElement("a");
        a.href = url;
        a.download = filename;
        a.click();
        setTimeout(() => URL.revokeObjectURL(url), 1000);
      }}
    >
      Download CSV
    </button>
  );
}
