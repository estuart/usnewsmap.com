// The search form's newspaper filter (07 §7.13): a button that opens a box
// to find newspapers by title or place, and the papers chosen. The list
// comes from `/v1/titles` once per version and is filtered here. A choice
// applies with the rest of the form when the visitor presses Search.

import { useEffect, useRef, useState, type KeyboardEvent } from "react";
import type { TitleEntry } from "../api/types";
import { languageName } from "../lib/languages";

/** Most matches listed at once; typing more narrows them. */
export const SHOWN = 50;

/** Most newspapers one search can name (the API's `lccn` limit). */
export const MAX_PAPERS = 60;

/** Lowercase, without accents or punctuation: "Nueva-Orleáns" is "nueva orleans". */
export function fold(s: string): string {
  return s
    .normalize("NFD")
    .replace(/[̀-ͯ]/g, "")
    .toLowerCase()
    .replace(/[^\p{L}\p{N}]+/gu, " ")
    .trim();
}

/**
 * The newspapers matching what was typed, best first, at most `limit`, and
 * how many match in all. Every word typed must begin a word of the title,
 * the place, the state's code or the LCCN. Titles that begin with the text
 * come first, then titles that hold every word, then the rest (a match on
 * the place); within each, most pages first.
 */
export function matchTitles(
  titles: readonly TitleEntry[],
  text: string,
  limit = SHOWN,
): { shown: TitleEntry[]; total: number } {
  const q = fold(text);
  if (!q) return { shown: [], total: 0 };
  const words = q.split(" ");
  const ranked: { t: TitleEntry; rank: number }[] = [];
  for (const t of titles) {
    const title = ` ${fold(t.title)}`;
    const all = `${title} ${fold(t.place ?? "")} ${t.state.toLowerCase()} ${t.lccn}`;
    if (!words.every((w) => all.includes(` ${w}`))) continue;
    const rank =
      title.startsWith(` ${q}`) || title.startsWith(` the ${q}`)
        ? 0
        : words.every((w) => title.includes(` ${w}`))
          ? 1
          : 2;
    ranked.push({ t, rank });
  }
  ranked.sort((a, b) => a.rank - b.rank || (b.t.pages ?? 0) - (a.t.pages ?? 0) || a.t.title.localeCompare(b.t.title));
  return { shown: ranked.slice(0, limit).map((r) => r.t), total: ranked.length };
}

/**
 * The years of publication in LoC's record (the paper's whole run): "published 1883–1922",
 * "published from 1896" (still published when catalogued), "published until 1922",
 * "published 1898", or "" when unknown.
 */
export function published(t: Pick<TitleEntry, "first_year" | "last_year">): string {
  if (t.first_year === null) return t.last_year === null ? "" : `published until ${t.last_year}`;
  if (t.last_year === null) return `published from ${t.first_year}`;
  return t.first_year === t.last_year ? `published ${t.first_year}` : `published ${t.first_year}–${t.last_year}`;
}

/**
 * The paper's pages on this site: "12,345 pages from 1885 to 1910" (the years of its first
 * and last pages in the version), "1 page from 1898", "12,345 pages" when the version
 * doesn't record the dates, or "" when it counts neither.
 */
export function pagesDetail(t: Pick<TitleEntry, "pages" | "first_page_date" | "last_page_date">): string {
  const count = t.pages === null ? "pages" : `${t.pages.toLocaleString("en-US")} ${t.pages === 1 ? "page" : "pages"}`;
  const first = t.first_page_date?.slice(0, 4);
  const last = t.last_page_date?.slice(0, 4);
  if (!first || !last) return t.pages === null ? "" : count;
  return first === last ? `${count} from ${first}` : `${count} from ${first} to ${last}`;
}

/**
 * "Omaha, NE · published 1883–1922 · English, German · 12,345 pages from 1885 to 1910":
 * what tells papers of one name apart.
 */
export function titleDetail(t: TitleEntry): string {
  return [
    t.place ? `${t.place}, ${t.state}` : t.state,
    published(t),
    t.languages.map((c) => languageName(c)).join(", "),
    pagesDetail(t),
  ]
    .filter(Boolean)
    .join(" · ");
}

/** The newspaper's page at the Library of Congress, where its issues can be browsed by date. */
export function locTitleUrl(lccn: string): string {
  return `https://www.loc.gov/item/${encodeURIComponent(lccn)}/`;
}

/** "Newspapers: any", "Newspapers: The Omaha Bee", "Newspapers: 3 chosen". */
export function newspaperSummary(chosen: readonly string[], nameOf: (lccn: string) => string): string {
  if (chosen.length === 0) return "Newspapers: any";
  if (chosen.length === 1) return `Newspapers: ${nameOf(chosen[0]!)}`;
  return `Newspapers: ${chosen.length} chosen`;
}

/**
 * The papers a search is limited to, for the line over its results: every
 * name up to three; from four on, two names and how many more, so it never
 * reads "and 1 more" where the name would fit.
 */
export function paperFilterNames(
  lccns: readonly string[],
  nameOf: (lccn: string) => string,
): { names: string[]; more: number } {
  const names = lccns.slice(0, lccns.length > 3 ? 2 : 3).map(nameOf);
  return { names, more: lccns.length - names.length };
}

interface Props {
  id: string;
  /** The version's newspapers; undefined while they load. */
  titles: readonly TitleEntry[] | undefined;
  /** The list couldn't be loaded. */
  failed?: boolean;
  /** Called when the visitor first opens the filter, so the list loads only when wanted. */
  onWant?: () => void;
  /** Chosen LCCNs, sorted. */
  value: string[];
  onChange: (lccns: string[]) => void;
  /**
   * Lists a chosen paper's pages without a search word (#274). Given only
   * when the API lists pages that way (`/v1/meta`'s `browse_pages`); each
   * chosen paper then has "Browse pages". `returnFocus` puts focus back
   * where the list was opened from, for when it closes.
   */
  onBrowse?: (lccn: string, returnFocus: () => void) => void;
}

/**
 * Focus back on the "Browse pages" button `from` while it can take focus,
 * else on the filter's trigger (the panel holding `from` is closed, or the
 * paper is no longer chosen), so closing the pages never leaves focus on
 * the page's body.
 */
function focusOpener(from: HTMLElement, trigger: HTMLElement | null) {
  const usable = (el: HTMLElement | null) => !!el && el.isConnected && !el.closest("[hidden]");
  const to = usable(from)
    ? from
    : usable(trigger)
      ? trigger
      : // The search form was remounted: the trigger is a new element.
        document.querySelector<HTMLElement>(".papers-filter__button");
  to?.focus();
}

export function NewspaperFilter({ id, titles, failed = false, onWant, value, onChange, onBrowse }: Props) {
  const [open, setOpen] = useState(false);
  const [text, setText] = useState("");
  // The highlighted match (aria-activedescendant), or -1 for none.
  const [active, setActive] = useState(-1);
  const [listOpen, setListOpen] = useState(false);
  const wrap = useRef<HTMLDivElement>(null);
  const button = useRef<HTMLButtonElement>(null);
  const input = useRef<HTMLInputElement>(null);
  useEffect(() => {
    if (!open) return;
    const away = (e: PointerEvent) => {
      if (!wrap.current?.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("pointerdown", away);
    return () => document.removeEventListener("pointerdown", away);
  }, [open]);

  const byLccn = new Map((titles ?? []).map((t) => [t.lccn, t]));
  const nameOf = (lccn: string) => byLccn.get(lccn)?.title ?? lccn;
  const { shown, total } = titles ? matchTitles(titles, text) : { shown: [], total: 0 };
  const expanded = listOpen && shown.length > 0;
  const panelId = `${id}-papers`;
  const listId = `${id}-papers-list`;
  const optionId = (i: number) => `${id}-papers-${i}`;
  const full = value.length >= MAX_PAPERS;

  useEffect(() => {
    if (active >= 0) document.getElementById(optionId(active))?.scrollIntoView?.({ block: "nearest" });
  });

  const toggle = (lccn: string) => {
    if (value.includes(lccn)) onChange(value.filter((l) => l !== lccn));
    else if (!full) onChange([...value, lccn].sort());
  };
  const pick = (t: TitleEntry) => {
    if (full && !value.includes(t.lccn)) return;
    toggle(t.lccn);
    setText("");
    setListOpen(false);
    setActive(-1);
    input.current?.focus();
  };

  const onKeyDown = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      if (!shown.length) return;
      setListOpen(true);
      const step = e.key === "ArrowDown" ? 1 : -1;
      setActive((a) =>
        !expanded || a < 0 ? (step > 0 ? 0 : shown.length - 1) : (a + step + shown.length) % shown.length,
      );
    } else if (e.key === "Enter") {
      // Enter chooses the highlighted paper; it never submits the search from this box.
      e.preventDefault();
      const t = expanded && active >= 0 ? shown[active] : undefined;
      if (t) pick(t);
    } else if (e.key === "Escape") {
      if (expanded) {
        // The list closes first, then the filter on a second Escape.
        e.preventDefault();
        e.stopPropagation();
        setListOpen(false);
        setActive(-1);
      } else if (text) {
        e.preventDefault();
        e.stopPropagation();
        setText("");
      }
    }
  };

  return (
    <div
      className="papers-filter"
      ref={wrap}
      onKeyDown={(e) => {
        if (e.key === "Escape" && open) {
          e.preventDefault();
          setOpen(false);
          button.current?.focus();
        }
      }}
    >
      <button
        ref={button}
        type="button"
        className="button papers-filter__button"
        aria-expanded={open}
        aria-controls={panelId}
        onClick={() => {
          if (!open) onWant?.();
          setOpen((o) => !o);
        }}
      >
        {newspaperSummary(value, nameOf)}
      </button>
      <div
        id={panelId}
        className="papers-filter__panel"
        role="group"
        aria-labelledby={`${id}-papers-label`}
        hidden={!open}
      >
        <label id={`${id}-papers-label`} className="papers-filter__label" htmlFor={`${id}-papers-q`}>
          Newspapers
        </label>
        <p id={`${id}-papers-hint`} className="papers-filter__hint">
          Type part of a title or a place. Pages from any newspaper chosen; none for every newspaper.
        </p>
        <input
          ref={input}
          id={`${id}-papers-q`}
          type="text"
          role="combobox"
          aria-autocomplete="list"
          aria-expanded={expanded}
          aria-controls={listId}
          aria-activedescendant={expanded && active >= 0 ? optionId(active) : undefined}
          aria-describedby={`${id}-papers-hint`}
          placeholder={titles ? "e.g. Bee or Omaha" : "Loading newspapers…"}
          disabled={!titles}
          value={text}
          onChange={(e) => {
            setText(e.target.value);
            setListOpen(true);
            setActive(-1);
          }}
          onKeyDown={onKeyDown}
          autoComplete="off"
          spellCheck={false}
        />
        <ul
          id={listId}
          role="listbox"
          aria-label="Newspapers found"
          aria-multiselectable="true"
          className="papers-filter__list"
          hidden={!expanded}
        >
          {expanded &&
            shown.map((t, i) => {
              const chosen = value.includes(t.lccn);
              return (
                <li
                  key={t.lccn}
                  id={optionId(i)}
                  role="option"
                  aria-selected={chosen}
                  aria-disabled={!chosen && full ? true : undefined}
                  className={
                    i === active ? "papers-filter__option papers-filter__option--active" : "papers-filter__option"
                  }
                  // Keep focus in the box: a click chooses without blurring it.
                  onPointerDown={(e) => e.preventDefault()}
                  onClick={() => pick(t)}
                >
                  <span className="papers-filter__title">{t.title}</span>
                  <span className="papers-filter__detail">{titleDetail(t)}</span>
                </li>
              );
            })}
        </ul>
        {/* Read out as the matches change, so a screen reader knows the list is there. */}
        <p className="visually-hidden" role="status">
          {failed
            ? ""
            : text.trim() && titles
              ? total === 0
                ? "No newspapers match."
                : `${total.toLocaleString("en-US")} ${total === 1 ? "newspaper matches" : "newspapers match"}${
                    total > shown.length ? `, the first ${shown.length} listed` : ""
                  }. Use the arrow keys to choose.`
              : ""}
        </p>
        {expanded && total > shown.length && (
          <p className="papers-filter__more" aria-hidden="true">
            {total.toLocaleString("en-US")} newspapers match; type more to narrow the list.
          </p>
        )}
        {listOpen && text.trim() && titles && total === 0 && (
          <p className="papers-filter__more">No newspapers match.</p>
        )}
        {failed && (
          <p className="papers-filter__more" role="alert">
            The list of newspapers could not be loaded.
          </p>
        )}
        {full && <p className="papers-filter__more">A search can name at most {MAX_PAPERS} newspapers.</p>}
        {value.length > 0 && (
          <ul className="papers-filter__chosen" aria-label="Chosen newspapers">
            {value.map((lccn) => {
              const t = byLccn.get(lccn);
              return (
                <li key={lccn}>
                  <span className="papers-filter__title">{t?.title ?? lccn}</span>
                  {t && <span className="papers-filter__detail">{titleDetail(t)}</span>}
                  <span className="papers-filter__actions">
                    {onBrowse && (
                      <button
                        type="button"
                        className="link-button"
                        onClick={(e) => {
                          const from = e.currentTarget;
                          setOpen(false);
                          onBrowse(lccn, () => focusOpener(from, button.current));
                        }}
                      >
                        Browse pages <span className="visually-hidden">of {t?.title ?? lccn}</span>
                      </button>
                    )}
                    <a href={locTitleUrl(lccn)} target="_blank" rel="noopener noreferrer">
                      Issues <span className="visually-hidden">of {t?.title ?? lccn}</span> at loc.gov
                    </a>
                    <button
                      type="button"
                      className="link-button"
                      onClick={() => onChange(value.filter((l) => l !== lccn))}
                    >
                      Remove <span className="visually-hidden">{t?.title ?? lccn}</span>
                    </button>
                  </span>
                </li>
              );
            })}
          </ul>
        )}
        {value.length > 0 && (
          <button type="button" className="link-button" onClick={() => onChange([])}>
            Clear newspapers
          </button>
        )}
      </div>
    </div>
  );
}
