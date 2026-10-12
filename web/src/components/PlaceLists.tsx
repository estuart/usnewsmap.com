import { useId, useState, type ReactNode } from "react";
import { dateFromDay, formatDate } from "../lib/time";
import { LIST_LENGTH, MOST_SHOWN, ShownList, type ListLengths } from "./ListLength";
import type { DaysResponse } from "../api/types";

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

/** Places with enough matching pages for a median, and a median to rank by. */
function eligiblePlaces<R extends ListRow>(rows: R[]): R[] {
  return rows.filter((r) => r.value >= MIN_MEDIAN_PAGES && r.when !== undefined && !Number.isNaN(r.when));
}

/**
 * Splits places into the earlier half (one more when odd), earliest first,
 * and the later half, latest first, by `at`: a pair compared by its first
 * number, then its second. Ties go to the place with more pages, then by
 * name. No place is in both, however many each list shows, even when places
 * share a median.
 */
function halves<R extends ListRow>(
  usable: R[],
  at: (r: R) => readonly [number, number],
): { earliest: R[]; latest: R[]; eligible: number } {
  const by = (dir: 1 | -1) => (a: R, b: R) => {
    const [a0, a1] = at(a);
    const [b0, b1] = at(b);
    return dir * (a0 - b0 || a1 - b1) || b.value - a.value || a.name.localeCompare(b.name);
  };
  const ascending = [...usable].sort(by(1));
  const n = Math.ceil(ascending.length / 2);
  return { earliest: ascending.slice(0, n), latest: ascending.slice(n).sort(by(-1)), eligible: usable.length };
}

/**
 * The places whose matching pages fall earliest and latest, by median date,
 * among those with at least MIN_MEDIAN_PAGES pages: every one of them, the
 * earlier half in `earliest` and the later half in `latest`, so none is in
 * both. The lists choose how many to show.
 */
export function medianExtremes<R extends ListRow>(rows: R[]): { earliest: R[]; latest: R[]; eligible: number } {
  return halves(eligiblePlaces(rows), (r) => [0, r.when!]);
}

/** The most places one /v1/days request takes (`MAX_PLACES` in the API). */
export const DAYS_PER_REQUEST = 20;

/** The two Median date lists, as `useListLengths` names them. */
export type MedianListName = "earliest" | "latest";

/**
 * The places that could be among those the Earliest and Latest lists show
 * once medians are known to the day: at each end, those ranked by bucket up
 * to the last one shown, and every place sharing that one's bucket. Each list
 * holds half the places, so with few places this is all of them.
 *
 * Null when that is more than one request's worth (20 places) for every five
 * places the two lists show between them: 20 while each shows five, as
 * before the lists could grow, up to 100 with 25 in each. An incomplete set
 * could leave out the true earliest, so the lists keep the bucket instead.
 */
export function medianCandidates(
  rows: ListRow[],
  shown: ListLengths<MedianListName> = { earliest: LIST_LENGTH, latest: LIST_LENGTH },
): string[] | null {
  const usable = eligiblePlaces(rows);
  const half = Math.ceil(usable.length / 2);
  const end = (dir: 1 | -1, n: number) => {
    const sorted = [...usable].sort((a, b) => dir * (a.when! - b.when!));
    const edge = sorted[Math.min(n, sorted.length) - 1];
    return edge ? sorted.filter((r) => dir * (r.when! - edge.when!) <= 0) : [];
  };
  const ends = [...end(1, Math.min(shown.earliest, half)), ...end(-1, Math.min(shown.latest, usable.length - half))];
  const ids = [...new Set(ends.map((r) => r.id))].sort();
  const budget = (DAYS_PER_REQUEST * (shown.earliest + shown.latest)) / (2 * LIST_LENGTH);
  return ids.length > budget ? null : ids;
}

/** One place's matching pages per day, as `/v1/days` gives them. */
export type PlaceDays = DaysResponse["places"][number];

/**
 * The pages per day of `ids`: from `known` (a search's answers so far) where
 * it has them, asking `ask` for the rest at most DAYS_PER_REQUEST at a time,
 * one request after another. Answers are added to `known`, a place the API
 * left out as one with no days, so a longer list, or the same places at a
 * new date, asks only for places not asked for before.
 */
export async function daysFor(
  ids: string[],
  known: Map<string, PlaceDays>,
  ask: (ids: string[]) => Promise<DaysResponse>,
): Promise<PlaceDays[]> {
  const missing = ids.filter((id) => !known.has(id));
  for (let i = 0; i < missing.length; i += DAYS_PER_REQUEST) {
    const part = missing.slice(i, i + DAYS_PER_REQUEST);
    const answer = await ask(part);
    const got = new Map(answer.places.map((p) => [p.id, p]));
    for (const id of part) known.set(id, got.get(id) ?? { id, days: [], hits: [] });
  }
  return ids.map((id) => known.get(id)!);
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
  /** What the lists can show, to tell one ranking from the next. */
  key: string;
}

/**
 * The two Median date lists, every eligible place in one or the other, by
 * exact day where `exact` has it, else by bucket.
 *
 * Exact days come for the places at each end (`medianCandidates`): ranked by
 * bucket, a run from the earliest and a run from the latest, each made of
 * whole buckets. Those runs are ranked by day, and the places between them
 * keep their bucket's order: every day in a run's buckets comes before (or,
 * at the latest end, after) every bucket between, so the order holds across
 * the seams. With days for every place, all are ranked by day.
 */
export function medianLists(rows: ListRow[], exact: Map<string, number> | null | undefined): Ranked {
  const usable = eligiblePlaces(rows);
  let ranked: { earliest: ListRow[]; latest: ListRow[]; eligible: number };
  if (!exact) {
    ranked = medianExtremes(usable);
  } else {
    const known = (r: ListRow) => exact.has(r.id) && !Number.isNaN(exact.get(r.id)!);
    const byBucket = [...usable].sort((a, b) => a.when! - b.when!);
    // How many places from the start of `list` have exact days, cut back to
    // whole buckets: a place without one, sharing the last one's bucket, could
    // fall on an earlier day.
    const run = (list: ListRow[]) => {
      let k = 0;
      while (k < list.length && known(list[k]!)) k++;
      if (k < list.length) while (k > 0 && list[k - 1]!.when === list[k]!.when) k--;
      return k;
    };
    const head = run(byBucket);
    const tail = head === byBucket.length ? 0 : run([...byBucket].reverse());
    // 0: the earliest run, by day; 1: between the runs, by bucket; 2: the latest run, by day.
    const group = new Map<string, number>();
    const placed = byBucket.map((r, i) => {
      const g = i < head ? 0 : i >= byBucket.length - tail ? 2 : 1;
      group.set(r.id, g);
      if (g === 1) return r;
      const day = exact.get(r.id)!;
      return { ...r, when: day, whenLabel: formatDate(dateFromDay(day)) };
    });
    ranked = halves(placed, (r) => [group.get(r.id)!, r.when!]);
  }
  const { earliest, latest, eligible } = ranked;
  // Only what can show changes the lists: the first MOST_SHOWN of each, and how many places there are.
  const key = JSON.stringify([
    eligible,
    ...[...earliest.slice(0, MOST_SHOWN), ...latest.slice(0, MOST_SHOWN)].map((r) => [r.id, r.whenLabel, r.value]),
  ]);
  return { earliest, latest, eligible, key };
}

/**
 * Where the Median date lists stand: `exact` (ranked by day), `updating`
 * (the last lists, exact or by bucket, while the next set loads), `loading`
 * (no answer for this search yet and no lists shown: nothing is listed), or `bucket` (ranked by the
 * search's bucket for good: a search by day, more tied places than the
 * lists' lengths allow requests for, or the day counts failed).
 */
export type MedianStatus = "exact" | "updating" | "loading" | "bucket";

export function WhenLists({
  rows,
  onSelect,
  trailing,
  exact,
  status = exact ? "exact" : "bucket",
  shown,
  onShown,
  tied,
}: ListsProps & {
  /** Exact median day numbers for the places that could be listed (`exact` and `updating`). */
  exact?: Map<string, number> | null;
  status?: MedianStatus;
  /** How many places each list shows, from `useListLengths`. */
  shown: ListLengths<MedianListName>;
  onShown: (list: MedianListName, n: number) => void;
  /**
   * The bucket ("month", "year") the lists rank by because too many places
   * share one to ask for their days; absent otherwise.
   */
  tied?: string;
}) {
  // The lists last shown from exact days, and how long the lists were then.
  // While the next set loads ("updating") they stay exactly as they were, not
  // recomputed against the new window from the old answer, until the fresh
  // answer replaces them. After lists by bucket (or none) `ranked` is null:
  // an "updating" then shows them by bucket, not exact lists from before
  // them, maybe another search's.
  const lengths = `${shown.earliest},${shown.latest}`;
  const [last, setLast] = useState<{ ranked: Ranked | null; lengths: string } | null>(null);
  const current = status === "updating" && last?.ranked ? null : medianLists(rows, status === "bucket" ? null : exact);
  if (status === "exact" && current && (current.key !== last?.ranked?.key || lengths !== last.lengths))
    setLast({ ranked: current, lengths });
  if ((status === "bucket" || status === "loading") && (last?.ranked || last?.lengths !== lengths))
    setLast({ ranked: null, lengths });
  if (status === "loading") {
    return (
      <ListsPanel title="Earliest and latest" label="Places with the earliest and latest median dates">
        <p className="lists__loading" role="status">
          Finding each place&apos;s median day…
        </p>
      </ListsPanel>
    );
  }
  const { earliest, latest, eligible } = (status === "updating" && last?.ranked) || current!;
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
  // A list grew (or shrank) since the last lists: the days of the places it lists are on their way.
  const note =
    status !== "updating"
      ? ""
      : last && last.lengths !== lengths
        ? "Finding the listed places' median days…"
        : "Updating for the new date…";
  return (
    <ListsPanel title="Earliest and latest" label="Places with the earliest and latest median dates">
      <div className={status === "updating" ? "lists--updating" : undefined} aria-busy={status === "updating"}>
        <ShownList
          heading="Earliest median date"
          items={earliest}
          empty={empty}
          itemKey={(r) => r.id}
          render={item}
          limit={shown.earliest}
          onLimit={(n) => onShown("earliest", n)}
          ranking={status === "updating"}
        />
        <ShownList
          heading="Latest median date"
          items={latest}
          empty={empty}
          itemKey={(r) => r.id}
          render={item}
          limit={shown.latest}
          onLimit={(n) => onShown("latest", n)}
          ranking={status === "updating"}
        />
      </div>
      <p className="skew-list__note" role="status">
        {note}
      </p>
      {tied && status === "bucket" && (
        <p className="skew-list__note">
          Too many places share a median {tied} to look up each one&apos;s day, so these lists go by {tied}.
        </p>
      )}
      <p className="skew-list__note">
        A place&apos;s median date is when half of its matching pages {scope(trailing)} had been printed. Only places
        with at least {MIN_MEDIAN_PAGES} matching pages are listed: the earlier half under Earliest, the later half
        under Latest.
      </p>
    </ListsPanel>
  );
}
