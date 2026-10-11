import { useEffect, useId, useRef, useState, type ReactNode } from "react";

/** How many places each list shows at first, and how many more each "Show more" adds. */
export const LIST_LENGTH = 5;
/**
 * The most places a list shows, so an expanded list doesn't push the rest of
 * the page off a phone's screen. The table has every place.
 */
export const MOST_SHOWN = 25;

/** How many entries each of the named lists shows. */
export type ListLengths<K extends string> = Record<K, number>;

const firstFew = <K extends string>(names: readonly K[]) =>
  Object.fromEntries(names.map((n) => [n, LIST_LENGTH])) as ListLengths<K>;

/**
 * How many entries each of the side panel's lists shows, kept above the lists
 * so they keep it when they move (phones put them after the playback
 * controls, so a rotation remounts them). All go back to the first few when
 * `resetKey` changes: a new search, window or measure.
 */
export function useListLengths<K extends string>(
  resetKey: string,
  names: readonly K[],
): [ListLengths<K>, (list: K, n: number) => void] {
  const [state, setState] = useState(() => ({ key: resetKey, lengths: firstFew(names) }));
  let current = state;
  if (state.key !== resetKey) {
    current = { key: resetKey, lengths: firstFew(names) };
    setState(current);
  }
  return [current.lengths, (list, n) => setState((s) => ({ ...s, lengths: { ...s.lengths, [list]: n } }))];
}

/**
 * One list in the side panel: its heading, its first `limit` entries, how
 * many there are in all, and buttons to show the next ones or go back to the
 * first few. Without `onLimit` it shows every entry it is given, with no
 * buttons.
 */
export function ShownList<R>({
  heading,
  empty,
  items,
  itemKey,
  render,
  limit = Infinity,
  onLimit,
}: {
  heading: string;
  empty: string;
  items: R[];
  itemKey: (r: R) => string;
  /** One entry's content, inside its `<li>`. */
  render: (r: R) => ReactNode;
  limit?: number;
  onLimit?: (n: number) => void;
}) {
  const id = useId();
  const listId = `${id}-list`;
  const headingId = `${id}-heading`;
  const countId = `${id}-count`;
  const cap = onLimit ? MOST_SHOWN : Infinity;
  const visible = items.slice(0, Math.min(limit, cap));
  const more = !!onLimit && items.length > visible.length && visible.length < cap;
  // Showing more than the first few: what aria-expanded says on both buttons.
  const expanded = !!onLimit && visible.length > LIST_LENGTH;
  const list = useRef<HTMLOListElement>(null);
  const moreButton = useRef<HTMLButtonElement>(null);
  // Where focus goes once the list has changed: after "Show more", the first
  // entry it added (its button, or the entry itself when it has none), so
  // keyboard and screen reader users land on the new rows; after "Show
  // fewer", which goes away, the "Show more" button.
  const focusNext = useRef<number | "more" | null>(null);

  useEffect(() => {
    const next = focusNext.current;
    if (next === null) return;
    focusNext.current = null;
    if (next === "more") {
      moreButton.current?.focus();
      return;
    }
    const item = list.current?.children[next] as HTMLElement | undefined;
    const target = item?.querySelector("button") ?? item;
    if (target === item && item) item.tabIndex = -1;
    target?.focus();
  }, [limit]);

  let count = "";
  if (onLimit && items.length > LIST_LENGTH) {
    count =
      visible.length === items.length
        ? `Showing all ${items.length.toLocaleString("en-US")}.`
        : `Showing ${visible.length} of ${items.length.toLocaleString("en-US")}.`;
    if (visible.length === MOST_SHOWN && items.length > MOST_SHOWN) count += " The table lists every place.";
  }

  return (
    <section className="skew-list">
      <h3 id={headingId}>{heading}</h3>
      {items.length === 0 ? (
        <p className="skew-list__empty">{empty}</p>
      ) : (
        <ol id={listId} ref={list}>
          {visible.map((r) => (
            <li key={itemKey(r)}>{render(r)}</li>
          ))}
        </ol>
      )}
      {count && (
        <p className="skew-list__count" id={countId}>
          {count}
        </p>
      )}
      {onLimit && (more || expanded) && (
        <p className="skew-list__more">
          {more && (
            <button
              type="button"
              className="link-button"
              ref={moreButton}
              aria-expanded={expanded}
              aria-controls={listId}
              aria-describedby={`${headingId} ${countId}`}
              onClick={() => {
                focusNext.current = visible.length;
                onLimit(Math.min(visible.length + LIST_LENGTH, MOST_SHOWN));
              }}
            >
              Show more
            </button>
          )}
          {expanded && (
            <button
              type="button"
              className="link-button"
              aria-expanded={true}
              aria-controls={listId}
              aria-describedby={`${headingId} ${countId}`}
              onClick={() => {
                focusNext.current = "more";
                onLimit(LIST_LENGTH);
              }}
            >
              Show fewer
            </button>
          )}
        </p>
      )}
    </section>
  );
}
