import { useInfiniteQuery } from "@tanstack/react-query";
import { ApiError, type SearchParams } from "../api/client";
import type { HitItem, HitSort } from "../api/types";
import { CONGRESS_SITES, hitTitle } from "../lib/congress";
import { AMERICAN_STORIES_BADGE, americanStoriesNote } from "../lib/matchSource";
import { snippetSegments } from "../lib/snippet";
import { formatDate } from "../lib/time";
import { placeHitsQuery } from "./placeHits";
import { skewSentence, type SkewInfo } from "../lib/skewText";

interface Props {
  params: SearchParams;
  version: string;
  placeId: string;
  placeName: string;
  sort: HitSort;
  onSort: (sort: HitSort) => void;
  windowHits: number;
  /** The place's relative rate in the current window (relative-rate view). */
  note?: SkewInfo;
  /** Synthetic fixtures: their LCCNs are invented, so LoC has no such pages. */
  synthetic: boolean;
  onClose: () => void;
}

const SORT_LABELS: Record<HitSort, string> = {
  oldest: "Oldest first",
  newest: "Newest first",
  relevant: "Most mentions",
};

/** Why a page is marked "Our OCR" (#139). */
const OCR_NOTE =
  "The Library of Congress has no searchable text for this page. We read it ourselves with NDLOCR-Lite, text-recognition software from Japan's National Diet Library. Expect some misread characters.";

/** Place drill-down (F-03): pages by date or by mentions, with snippets and LoC links. */
export function PlacePanel({
  params,
  version,
  placeId,
  placeName,
  sort,
  onSort,
  windowHits,
  note,
  synthetic,
  onClose,
}: Props) {
  // The same query as the prefetch after a search (#265): a prefetched list shows at once.
  const query = useInfiniteQuery(placeHitsQuery(params, version, placeId, sort));
  const first = query.data?.pages[0];
  const items = query.data?.pages.flatMap((p) => p.items) ?? [];
  return (
    <aside className="panel" aria-label={`Pages from ${placeName}`}>
      <header className="panel__head">
        <h2>{first?.place ? `${first.place.name}, ${first.place.state}` : placeName}</h2>
        <button type="button" className="button button--quiet" onClick={onClose} aria-label="Close">
          ✕
        </button>
      </header>
      {note && <p className="panel__skew">{skewSentence(placeName, note)}</p>}
      <p className="panel__summary">
        {first ? `${first.total.toLocaleString()} pages in this search` : "Loading…"}
        {first?.days !== undefined && ` on ${first.days.toLocaleString()} ${first.days === 1 ? "day" : "days"}`}
        {first && windowHits !== first.total ? ` · ${windowHits.toLocaleString()} up to the current date` : ""}
      </p>
      <div className="segmented" role="group" aria-label="Order of pages">
        {(["oldest", "newest", "relevant"] as const).map((s) => (
          <button key={s} type="button" aria-pressed={sort === s} onClick={() => onSort(s)}>
            {SORT_LABELS[s]}
          </button>
        ))}
      </div>
      {query.error && (
        <p role="alert" className="notice notice--error">
          {query.error instanceof ApiError
            ? (query.error.problem.hint ?? query.error.message)
            : "Could not load pages."}
        </p>
      )}
      <ol className="hits">
        {items.map((h) => (
          <Hit key={h.doc_id} h={h} synthetic={synthetic} />
        ))}
      </ol>
      {query.hasNextPage && (
        <button
          type="button"
          className="button"
          disabled={query.isFetchingNextPage}
          onClick={() => void query.fetchNextPage()}
        >
          {query.isFetchingNextPage ? "Loading…" : "Load more"}
        </button>
      )}
    </aside>
  );
}

/** One page in the list: date, newspaper (or Congress document), snippets and the viewer link. */
export function Hit({ h, synthetic }: { h: HitItem; synthetic: boolean }) {
  const asNote = americanStoriesNote(h);
  // A Congress page opens in its series' viewer, not LoC's, and the badge's
  // note is about LoC's newspapers (#229).
  const site = h.congress ? (CONGRESS_SITES[h.congress.series] ?? "its source") : null;
  return (
    <li className="hit">
      <div className="hit__meta">
        <time dateTime={h.date}>{formatDate(h.date)}</time> · {hitTitle(h)} · page {h.seq}
        {h.front_page ? " (front page)" : ""}
        {h.ocr && !site && (
          <>
            {" "}
            <span className="badge badge--ocr" title={OCR_NOTE}>
              Our OCR
              {/* The title isn't announced reliably or shown on touch: say it in text too. */}
              <span className="visually-hidden">. {OCR_NOTE}</span>
            </span>
          </>
        )}
        {asNote && (
          <>
            {" "}
            <span className="badge badge--ocr" title={asNote}>
              {AMERICAN_STORIES_BADGE}
              <span className="visually-hidden">. {asNote}</span>
            </span>
          </>
        )}
      </div>
      {h.snippets.map((s, i) => (
        <p key={i} className="hit__snippet">
          {snippetSegments(s).map((seg, j) => (seg.mark ? <mark key={j}>{seg.text}</mark> : seg.text))}
        </p>
      ))}
      {synthetic ? (
        <p className="hit__demo">Demo page: not a real {site ? "Congress" : "Library of Congress"} page.</p>
      ) : (
        h.links.viewer && (
          <a href={h.links.viewer} target="_blank" rel="noopener noreferrer">
            {site
              ? `View page at ${site}`
              : h.ocr
                ? "View the page image at the Library of Congress"
                : "View page at the Library of Congress"}
          </a>
        )
      )}
    </li>
  );
}
