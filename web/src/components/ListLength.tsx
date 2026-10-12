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
 * buttons. While `ranking` is set the entries' order may still change (the
 * Median date lists before the days of the places just added arrive), so
 * focus waits for it to settle before moving to an added entry.
 */
export function ShownList<R>({
  heading,
  empty,
  items,
  itemKey,
  render,
  limit = Infinity,
  onLimit,
  ranking = false,
}: {
  heading: string;
  empty: string;
  items: R[];
  itemKey: (r: R) => string;
  /** One entry's content, inside its `<li>`. */
  render: (r: R) => ReactNode;
  limit?: number;
  onLimit?: (n: number) => void;
  /** The order of `items` is not final yet: hold focus moves to an entry until it is. */
  ranking?: boolean;
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
  // fewer", which goes away, the "Show more" button. While `ranking`, the
  // added entries may still move, and one moved past the end would unmount
  // with focus on it, so the move to an entry waits, focus staying on "Show
  // more", and then goes to whichever entry is first of the added ones in
  // the final order. `waited` notes that it waited: by then focus may have
  // gone elsewhere on purpose, and is only moved if it is still on "Show
  // more", or nowhere because that button went away with the last entries.
  const focusNext = useRef<{ to: number | "more"; waited: boolean } | null>(null);

  useEffect(() => {
    const next = focusNext.current;
    if (next === null) return;
    if (next.to === "more") {
      focusNext.current = null;
      moreButton.current?.focus();
      return;
    }
    if (ranking) {
      next.waited = true;
      return;
    }
    focusNext.current = null;
    const active = document.activeElement;
    if (next.waited && active && active !== document.body && active !== moreButton.current) return;
    const entries = list.current?.children;
    const item = (entries?.[next.to] ?? entries?.[entries.length - 1]) as HTMLElement | undefined;
    const target = item?.querySelector("button") ?? item;
    if (target === item && item) item.tabIndex = -1;
    target?.focus();
  }, [limit, ranking]);

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
                focusNext.current = { to: visible.length, waited: false };
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
                focusNext.current = { to: "more", waited: false };
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
