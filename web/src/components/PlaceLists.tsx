import { useId, useState, type ReactNode } from "react";
import { dateFromDay, formatDate } from "../lib/time";
import { LIST_LENGTH, ShownList, type ListLengths } from "./ListLength";

/** The Median date lists leave out places with fewer matching pages than this: one page has a date, not a median. */
export const MIN_MEDIAN_PAGES = 5;
const STORAGE_KEY = "usnm.lists";

/** A place on the map in the current window, as the lists need it. */
export interface ListRow {
  id: string;
  name: string;
  state: string;
  /** Matching pages in the playback window: up to the current date, or the trailing window ending there. */
  value: number;
  /** The median date's position, 0..1 across the search; NaN when none. */
  when?: number;
  whenLabel?: string;
}

export interface StateRow {
  state: string;
  pages: number;
  places: number;
}

/** The places with the most matching pages, most first; ties by name. All of them: the list chooses how many to show. */
export function mostPages<R extends ListRow>(rows: R[]): R[] {
  return rows.filter((r) => r.value > 0).sort((a, b) => b.value - a.value || a.name.localeCompare(b.name));
}

/** The states with the most matching pages: each place counts toward its own state. All of them, as `mostPages`. */
export function statePages(rows: ListRow[]): StateRow[] {
  const by = new Map<string, StateRow>();
  for (const r of rows) {
    if (r.value <= 0 || !r.state) continue;
    const s = by.get(r.state) ?? { state: r.state, pages: 0, places: 0 };
    s.pages += r.value;
    s.places += 1;
    by.set(r.state, s);
  }
  return [...by.values()].sort((a, b) => b.pages - a.pages || a.state.localeCompare(b.state));
}

/**
 * The places whose matching pages fall earliest and latest, by median date,
 * among those with at least MIN_MEDIAN_PAGES pages. Ties go to the place
 * with more pages.
 */
export function medianExtremes<R extends ListRow>(rows: R[]): { earliest: R[]; latest: R[]; eligible: number } {
  const usable = rows.filter((r) => r.value >= MIN_MEDIAN_PAGES && r.when !== undefined && !Number.isNaN(r.when));
  const by = (dir: 1 | -1) => (a: R, b: R) =>
    dir * (a.when! - b.when!) || b.value - a.value || a.name.localeCompare(b.name);
  // With fewer than two lists' worth, split them so no place is in both,
  // even when places share a median.
  const n = Math.min(LIST_LENGTH, Math.ceil(usable.length / 2));
  const earliest = [...usable].sort(by(1)).slice(0, n);
  const taken = new Set(earliest.map((r) => r.id));
  return {
    earliest,
    latest: usable
      .filter((r) => !taken.has(r.id))
      .sort(by(-1))
      .slice(0, LIST_LENGTH),
    eligible: usable.length,
  };
}

/** The most places one /v1/days request takes. */
const MAX_CANDIDATES = 20;

/**
 * The places that could be among the five earliest or latest once medians
 * are known to the day: those ranked by bucket up to the fifth, and every
 * place sharing the fifth's bucket. Empty when that is more than one
 * request takes: an incomplete set could leave out the true earliest, so the
 * lists keep the bucket instead.
 */
export function medianCandidates(rows: ListRow[]): string[] {
  const usable = rows.filter((r) => r.value >= MIN_MEDIAN_PAGES && r.when !== undefined && !Number.isNaN(r.when));
  const end = (dir: 1 | -1) => {
    const sorted = [...usable].sort((a, b) => dir * (a.when! - b.when!));
    const edge = sorted[Math.min(LIST_LENGTH, sorted.length) - 1];
    return edge ? sorted.filter((r) => dir * (r.when! - edge.when!) <= 0) : [];
  };
  const ids = [...new Set([...end(1), ...end(-1)].map((r) => r.id))].sort();
  return ids.length > MAX_CANDIDATES ? [] : ids;
}

/**
 * The exact median day of a place's matching pages from day `lo` to day `hi`
 * (inclusive): the first day by which at least half had been printed, as the
 * bucket median is (`windowQuantile`). NaN with none.
 */
export function exactMedian(days: number[], hits: number[], lo: number, hi: number): number {
  let total = 0;
  for (let i = 0; i < days.length; i++) if (days[i]! >= lo && days[i]! <= hi) total += hits[i]!;
  if (total <= 0) return Number.NaN;
  let seen = 0;
  for (let i = 0; i < days.length; i++) {
    if (days[i]! < lo || days[i]! > hi) continue;
    seen += hits[i]!;
    if (seen >= total / 2) return days[i]!;
  }
  return Number.NaN;
}

function readOpen(): boolean {
  try {
    return window.localStorage.getItem(STORAGE_KEY) !== "hidden";
  } catch {
    return true;
  }
}

/** Whether the place lists are shown; remembered across visits on this browser. */
export function useListsOpen(): [boolean, (open: boolean) => void] {
  const [open, setOpen] = useState(readOpen);
  return [
    open,
    (next: boolean) => {
      setOpen(next);
      try {
        if (next) window.localStorage.removeItem(STORAGE_KEY);
        else window.localStorage.setItem(STORAGE_KEY, "hidden");
      } catch {
        // Private browsing without storage: it still toggles for this visit.
      }
    },
  ];
}

/**
 * The side panel when no place is selected, in every measure. It can be
 * hidden to give the map the room; the choice is kept for the next visit.
 */
export function ListsPanel({
  title,
  label,
  className,
  children,
}: {
  title: string;
  /** The panel's accessible name. */
  label: string;
  className?: string;
  children: ReactNode;
}) {
  const [open, setOpen] = useListsOpen();
  const body = useId();
  if (!open) {
    return (
      <aside className="panel panel--collapsed" aria-label={label}>
        <button
          type="button"
          className="button button--quiet panel__toggle"
          aria-expanded={false}
          onClick={() => setOpen(true)}
        >
          Show {title.toLowerCase()}
        </button>
      </aside>
    );
  }
  return (
    <aside className={`panel lists ${className ?? ""}`.trim()} aria-label={label}>
      <header className="panel__head">
        <h2>{title}</h2>
        <button
          type="button"
          className="button button--quiet panel__toggle"
          aria-expanded={true}
          aria-controls={body}
          onClick={() => setOpen(false)}
        >
          Hide
        </button>
      </header>
      <div id={body}>{children}</div>
    </aside>
  );
}

const pages = (n: number) => `${n.toLocaleString("en-US")} ${n === 1 ? "page" : "pages"}`;

function share(part: number, total: number): string {
  if (total <= 0) return "";
  const p = (100 * part) / total;
  return p < 0.1 ? "under 0.1%" : `${p.toFixed(1)}%`;
}

function PlaceButton({ row, onSelect }: { row: ListRow; onSelect: (id: string) => void }) {
  return (
    <button type="button" className="link-button" onClick={() => onSelect(row.id)}>
      {row.name}, {row.state}
    </button>
  );
}

/** Where the counts come from: everything up to the scrubber's date, or a trailing window ending there. */
const scope = (trailing: boolean) => (trailing ? "in the playback window" : "up to the current date");

interface ListsProps {
  rows: ListRow[];
  onSelect: (id: string) => void;
  /** A "Last N" window rather than everything up to the current date. */
  trailing: boolean;
}

/** The two Pages lists, as `useListLengths` names them. */
export type PagesListName = "places" | "states";

/** Pages: where the matching pages are, by place and by state. */
export function PagesLists({
  rows,
  onSelect,
  trailing,
  shown,
  onShown,
}: ListsProps & {
  /** How many places and states the lists show, from `useListLengths`. */
  shown: ListLengths<PagesListName>;
  onShown: (list: PagesListName, n: number) => void;
}) {
  const total = rows.reduce((a, r) => a + Math.max(r.value, 0), 0);
  return (
    <ListsPanel title="Most pages" label="Places and states with the most pages">
      <ShownList
        heading="Places"
        items={mostPages(rows)}
        empty={`No matching pages ${scope(trailing)}.`}
        itemKey={(r) => r.id}
        render={(r) => (
          <>
            <PlaceButton row={r} onSelect={onSelect} />{" "}
            <span className="skew-list__value">
              {pages(r.value)} ({share(r.value, total)})
            </span>
          </>
        )}
        limit={shown.places}
        onLimit={(n) => onShown("places", n)}
      />
      <ShownList
        heading="States"
        items={statePages(rows)}
        empty={`No matching pages ${scope(trailing)}.`}
        itemKey={(s) => s.state}
        render={(s) => (
          <>
            {s.state}{" "}
            <span className="skew-list__value">
              {pages(s.pages)} ({share(s.pages, total)}) in {s.places.toLocaleString("en-US")}{" "}
              {s.places === 1 ? "place" : "places"}
            </span>
          </>
        )}
        limit={shown.states}
        onLimit={(n) => onShown("states", n)}
      />
      <p className="skew-list__note">Matching pages {scope(trailing)}; shares are of all of them.</p>
    </ListsPanel>
  );
}

/** Median date: the places whose matching pages fall earliest and latest. */
interface Ranked {
  earliest: ListRow[];
  latest: ListRow[];
  eligible: number;
  /** What the lists show, to tell one ranking from the next. */
  key: string;
}

/** The two Median date lists, by exact day when `exact` is given, else by bucket. */
export function medianLists(rows: ListRow[], exact: Map<string, number> | null | undefined): Ranked {
  const { eligible } = medianExtremes(rows);
  const ranked = exact
    ? rows
        .filter((r) => exact.has(r.id) && !Number.isNaN(exact.get(r.id)!))
        .map((r) => ({ ...r, when: exact.get(r.id)!, whenLabel: formatDate(dateFromDay(exact.get(r.id)!)) }))
    : rows;
  const { earliest, latest } = medianExtremes(ranked);
  const key = JSON.stringify([eligible, ...[...earliest, ...latest].map((r) => [r.id, r.whenLabel, r.value])]);
  return { earliest, latest, eligible, key };
}

/**
 * Where the Median date lists stand: `exact` (ranked by day), `updating`
 * (the last exact lists while the next set loads), `loading` (no exact
 * answer for this search yet: nothing is listed), or `bucket` (ranked by the
 * search's bucket for good: a search by day, more ties than one request
 * takes, or the day counts failed).
 */
export type MedianStatus = "exact" | "updating" | "loading" | "bucket";

export function WhenLists({
  rows,
  onSelect,
  trailing,
  exact,
  status = exact ? "exact" : "bucket",
}: ListsProps & {
  /** Exact median day numbers for the places that could be listed (`exact` and `updating`). */
  exact?: Map<string, number> | null;
  status?: MedianStatus;
}) {
  // The lists last shown from exact days. While the next set loads
  // ("updating") they stay exactly as they were, not recomputed against the
  // new window from the old answer, until the fresh answer replaces them.
  const [shown, setShown] = useState<Ranked | null>(null);
  const current = status === "updating" && shown ? null : medianLists(rows, status === "bucket" ? null : exact);
  if (status === "exact" && current && current.key !== shown?.key) setShown(current);
  if (status === "loading") {
    return (
      <ListsPanel title="Earliest and latest" label="Places with the earliest and latest median dates">
        <p className="lists__loading" role="status">
          Finding each place&apos;s median day…
        </p>
      </ListsPanel>
    );
  }
  const { earliest, latest, eligible } = (status === "updating" && shown) || current!;
  const empty =
    eligible === 1
      ? `Only one place has ${MIN_MEDIAN_PAGES} or more matching pages ${scope(trailing)}.`
      : `No place has ${MIN_MEDIAN_PAGES} or more matching pages ${scope(trailing)}.`;
  const item = (r: ListRow) => (
    <>
      <PlaceButton row={r} onSelect={onSelect} />{" "}
      <span className="skew-list__value">
        {r.whenLabel} · {pages(r.value)}
      </span>
    </>
  );
  return (
    <ListsPanel title="Earliest and latest" label="Places with the earliest and latest median dates">
      <div className={status === "updating" ? "lists--updating" : undefined} aria-busy={status === "updating"}>
        <ShownList heading="Earliest median date" items={earliest} empty={empty} itemKey={(r) => r.id} render={item} />
        <ShownList heading="Latest median date" items={latest} empty={empty} itemKey={(r) => r.id} render={item} />
      </div>
      <p className="skew-list__note" role="status">
        {status === "updating" ? "Updating for the new date…" : ""}
      </p>
      <p className="skew-list__note">
        A place&apos;s median date is when half of its matching pages {scope(trailing)} had been printed. Only places
        with at least {MIN_MEDIAN_PAGES} matching pages are listed.
      </p>
    </ListsPanel>
  );
}
