import type { HitItem, HitSort } from "../api/types";
import { hitTitle } from "../lib/congress";
import { formatDate } from "../lib/time";

interface Props {
  first: HitItem;
  last: HitItem | null;
  /** "Chicago, IL" for a place id. */
  placeName: (id: string) => string;
  /** The permalink that opens a page's place with its list in `sort` order, so the page is at the top. */
  hrefFor: (hit: HitItem, sort: HitSort) => string;
  onOpen: (hit: HitItem, sort: HitSort) => void;
}

/**
 * The search's first and last matching page. Each date opens the page in its
 * place's list, so a misleading first mention (an OCR misread) can be checked.
 */
export function Mentions({ first, last, placeName, hrefFor, onOpen }: Props) {
  const link = (hit: HitItem, sort: HitSort) => (
    <a
      href={hrefFor(hit, sort)}
      onClick={(e) => {
        // Let modified clicks open the permalink in a new tab.
        if (e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
        e.preventDefault();
        onOpen(hit, sort);
      }}
    >
      <time dateTime={hit.date}>{formatDate(hit.date)}</time>
    </a>
  );
  return (
    <p className="mentions">
      First mention: {link(first, "oldest")}, <cite>{hitTitle(first)}</cite>, {placeName(first.place_id)}
      {last && last.doc_id !== first.doc_id && <> · Last: {link(last, "newest")}</>}
    </p>
  );
}
