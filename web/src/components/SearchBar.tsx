import { useId, useRef, useState, type FormEvent, type KeyboardEvent } from "react";
import { flushSync } from "react-dom";
import type { Meta, Mode, TitleEntry } from "../api/types";
import { parseDateEntry, showDate } from "../lib/dateEntry";
import { hasJapanese } from "../lib/japanese";
import { DEFAULTS, type ViewState } from "../state/url";
import { DateField } from "./DateField";
import { LanguageFilter, languageChoices } from "./LanguageFilter";
import { NewspaperFilter } from "./NewspaperFilter";

interface Props {
  view: ViewState;
  meta: Meta | undefined;
  onSearch: (patch: Partial<ViewState>) => void;
  /** A search is running: the button shows a spinner (its width doesn't change). */
  busy?: boolean;
  /** The version's newspapers (`/v1/titles`), once loaded. */
  titles?: readonly TitleEntry[];
  titlesFailed?: boolean;
  /** Asks for the newspapers' list: it loads when the newspaper filter is first opened. */
  onWantTitles?: () => void;
  /** Lists a newspaper's pages without a search word (#274), when `meta.browse_pages` says the API can. */
  onBrowsePaper?: (lccn: string, returnFocus: () => void) => void;
}

const MODES: { value: Mode; label: string }[] = [
  { value: "phrase", label: "Exact phrase" },
  { value: "all", label: "All words" },
  { value: "any", label: "Any word" },
  { value: "near", label: "Words near each other" },
];

/** How many search options differ from the defaults (shown on the toggle). */
export function activeOptions(
  view: Pick<ViewState, "mode" | "from" | "to" | "state" | "lang"> & Partial<Pick<ViewState, "source" | "lccn">>,
): number {
  // A Congress search hides the state, language and newspaper filters and
  // drops them on submit (#229), so they don't count.
  const congress = (view.source ?? DEFAULTS.source) === "congress";
  return [
    congress,
    view.mode !== DEFAULTS.mode,
    view.from !== DEFAULTS.from,
    view.to !== DEFAULTS.to,
    !congress && view.state.length > 0,
    !congress && view.lang.length > 0,
    !congress && (view.lccn?.length ?? 0) > 0,
  ].filter(Boolean).length;
}

/**
 * What a query in Japanese script searches (#139): only the Japanese pages we
 * read ourselves, or nothing yet on a version without them, or nothing in
 * Congress pages (#229). Null otherwise.
 */
export function japaneseHint(
  q: string,
  meta: Pick<Meta, "ja"> | undefined,
  source: ViewState["source"] = "news",
): string | null {
  // Before /v1/meta loads, it isn't known yet whether Japanese search is there.
  if (!hasJapanese(q) || !meta) return null;
  // The API refuses a Japanese query in Congress pages, which are all English.
  if (source === "congress") return "Japanese text can't be searched in Congress pages.";
  const ja = meta.ja;
  if (!ja) return "Searching Japanese text isn't available yet. It arrives with the next update.";
  return `Japanese searches cover only the ${ja.pages.toLocaleString("en-US")} Japanese-language pages we read ourselves. The Library of Congress has no searchable text for them.`;
}

/** Identity of the search in the URL; remount the form when it changes. */
export function searchKey(view: ViewState): string {
  return [
    view.q,
    view.mode,
    view.near,
    view.from,
    view.to,
    view.state.join(),
    view.lang.join(),
    view.lccn.join(),
    view.source,
    view.series.join(),
  ].join("|");
}

/**
 * The dates a search can cover: the newspapers' bounds, or for a Congress
 * search the span of the Congress series (#229), which runs past the
 * newspapers at both ends.
 */
export function searchBounds(
  meta: Pick<Meta, "bounds" | "congress"> | undefined,
  source: ViewState["source"],
): { from: string; to: string } | undefined {
  const series = meta?.congress?.series ?? [];
  if (source === "congress" && series.length > 0) {
    return {
      from: series.map((s) => s.from).reduce((a, b) => (a < b ? a : b)),
      to: series.map((s) => s.to).reduce((a, b) => (a > b ? a : b)),
    };
  }
  const lo = meta?.bounds.from;
  const hi = meta?.bounds.to;
  return lo && hi ? { from: lo, to: hi } : undefined;
}

export function SearchBar({
  view,
  meta,
  onSearch,
  busy = false,
  titles,
  titlesFailed,
  onWantTitles,
  onBrowsePaper,
}: Props) {
  const id = useId();
  // The parent remounts this form (via `key`) when the search in the URL
  // changes, so back/forward and example cards replace the draft.
  const [draft, setDraft] = useState(view);
  const [states, setStates] = useState(view.state.join(", "));
  // The From and To boxes as typed; read into ISO dates on submit.
  const [fromText, setFromText] = useState(showDate(view.from));
  const [toText, setToText] = useState(showDate(view.to));
  const [fromError, setFromError] = useState<string | null>(null);
  const [toError, setToError] = useState<string | null>(null);
  const fromRef = useRef<HTMLInputElement>(null);
  const toRef = useRef<HTMLInputElement>(null);
  // Codes from the URL stay listed after they're unchecked, so they don't vanish.
  const languages = languageChoices(meta?.languages, [...new Set([...view.lang, ...draft.lang])]);
  // On phones the options (match mode, dates, states, languages, newspapers) sit behind a toggle so
  // the header is just the search box. They start open when a search already
  // uses one, so an active filter is never hidden. Wider screens always show
  // them (CSS hides the toggle).
  // The switch shows only once /v1/meta lists Congress pages (#229), and
  // the options count the source the form will actually search. Until
  // /v1/meta has loaded the URL's source stands, so a Congress link opened
  // cold still searches Congress and opens with its options shown.
  const congressAvailable = (meta?.congress?.series.length ?? 0) > 0;
  const searchable = (s: ViewState["source"]) => (!meta || congressAvailable ? s : "news");
  const source = searchable(draft.source);
  const bounds = searchBounds(meta, source);
  const [open, setOpen] = useState(() => activeOptions({ ...view, source: searchable(view.source) }) > 0);
  const active = activeOptions({
    ...draft,
    source,
    from: fromText.trim(),
    to: toText.trim(),
    state: states.split(/[\s,]+/).filter(Boolean),
  });

  const submit = (e: FormEvent) => {
    e.preventDefault();
    const from = parseDateEntry(fromText, "from", bounds);
    const to = parseDateEntry(toText, "to", bounds);
    if (!from.ok || !to.ok) {
      // Show the options (phones hide them) so the box with the problem can take focus.
      flushSync(() => {
        setFromError(from.ok ? null : from.error);
        setToError(to.ok ? null : to.error);
        setOpen(true);
      });
      (from.ok ? toRef : fromRef).current?.focus();
      return;
    }
    // Show the dates searched: 1827 in From reads 01/01/1827. Before
    // /v1/meta has loaded, keep the entries as typed, so they are cut to the
    // index's days once it has (see DateField).
    if (bounds) {
      setFromText(showDate(from.iso));
      setToText(showDate(to.iso));
    }
    setFromError(null);
    setToError(null);
    if (!draft.q.trim()) return;
    onSearch({
      q: draft.q.trim(),
      mode: draft.mode,
      near: draft.near,
      from: from.iso,
      to: to.iso,
      state: states
        .split(/[\s,]+/)
        .map((s) => s.toUpperCase())
        .filter((s) => /^[A-Z]{2}$/.test(s)),
      // Congress pages are all English and printed in Washington for now,
      // and belong to no newspaper: those filters apply to newspapers only.
      lang: source === "congress" ? [] : draft.lang,
      lccn: source === "congress" ? [] : draft.lccn,
      ...(source === "congress" ? { state: [] } : {}),
      source,
      series: source === "congress" ? draft.series : [],
    });
  };

  const jaHint = japaneseHint(draft.q, meta, source);
  // An input method (IME) uses Enter to confirm a word: that Enter mustn't
  // submit the search. Safari ends the composition before the keydown, so
  // keyCode 229 ("being composed") is checked too.
  const composing = useRef(false);
  const onKeyDown = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key === "Enter" && (composing.current || e.nativeEvent.isComposing || e.keyCode === 229)) {
      e.preventDefault();
    }
  };
  return (
    <form
      className={open ? "search search--open" : "search"}
      role="search"
      onSubmit={submit}
      // What the form searches: the switch's choice, or newspapers when there is no switch (#229).
      aria-label={source === "congress" ? "Search Congress" : "Search newspapers"}
    >
      <label className="visually-hidden" htmlFor={`${id}-q`}>
        Word or phrase
      </label>
      <input
        id={`${id}-q`}
        className="search__q"
        type="search"
        placeholder='Try "cross of gold"'
        value={draft.q}
        maxLength={meta?.limits.max_query_chars ?? 256}
        onChange={(e) => setDraft({ ...draft, q: e.target.value })}
        onKeyDown={onKeyDown}
        onCompositionStart={() => (composing.current = true)}
        onCompositionEnd={() => (composing.current = false)}
        aria-describedby={jaHint ? `${id}-ja` : undefined}
        autoComplete="off"
        spellCheck={false}
      />
      <button
        type="submit"
        className={busy ? "button button--primary search__go search__go--busy" : "button button--primary search__go"}
        // The visible label is hidden behind the spinner while busy: a fixed name keeps the button named.
        aria-label="Search"
        aria-busy={busy}
      >
        <span className="search__go-label">Search</span>
        <span className="search__go-spinner" aria-hidden="true">
          <span className="spinner" />
        </span>
      </button>
      {jaHint && (
        <p id={`${id}-ja`} className="search__hint" role="note">
          {jaHint}
        </p>
      )}
      <button
        type="button"
        className="button search__toggle"
        aria-expanded={open}
        aria-controls={`${id}-options`}
        onClick={() => setOpen(!open)}
      >
        Options{active > 0 ? ` (${active})` : ""}
      </button>
      <div className="search__options" id={`${id}-options`}>
        <label className="visually-hidden" htmlFor={`${id}-mode`}>
          Match
        </label>
        <select
          id={`${id}-mode`}
          value={draft.mode}
          onChange={(e) => setDraft({ ...draft, mode: e.target.value as Mode })}
        >
          {MODES.map((m) => (
            <option key={m.value} value={m.value}>
              {m.label}
            </option>
          ))}
        </select>
        {draft.mode === "near" && (
          <label className="search__near">
            within
            <input
              type="number"
              min={1}
              max={meta?.limits.max_slop ?? 20}
              value={draft.near}
              onChange={(e) => setDraft({ ...draft, near: Number(e.target.value) || 1 })}
            />
            words
          </label>
        )}
        <DateField
          id={id}
          label="From"
          edge="from"
          text={fromText}
          onText={setFromText}
          error={fromError}
          errorId={`${id}-from-error`}
          onError={setFromError}
          bounds={bounds}
          inputRef={fromRef}
        />
        <DateField
          id={id}
          label="To"
          edge="to"
          text={toText}
          onText={setToText}
          error={toError}
          errorId={`${id}-to-error`}
          onError={setToError}
          bounds={bounds}
          inputRef={toRef}
        />
        {congressAvailable && (
          <label className="search__source">
            <span>Search</span>
            <select
              value={source}
              onChange={(e) => setDraft({ ...draft, source: e.target.value as ViewState["source"] })}
            >
              <option value="news">Newspapers</option>
              <option value="congress">Congress</option>
            </select>
          </label>
        )}
        {source === "news" && (
          <label className="search__state">
            <span>States</span>
            <input
              placeholder="e.g. GA, SC"
              value={states}
              onChange={(e) => setStates(e.target.value)}
              pattern="^\s*([A-Za-z]{2}([\s,]+|$))*$"
              title="Two-letter state codes, separated by commas"
            />
          </label>
        )}
        {source === "news" && languages.length > 0 && (
          <LanguageFilter
            id={id}
            choices={languages}
            byPage={meta?.language_filter === "page"}
            value={draft.lang}
            onChange={(lang) => setDraft({ ...draft, lang })}
          />
        )}
        {source === "news" && (
          <NewspaperFilter
            id={id}
            titles={titles}
            failed={titlesFailed}
            onWant={onWantTitles}
            value={draft.lccn}
            onChange={(lccn) => setDraft({ ...draft, lccn })}
            onBrowse={meta?.browse_pages ? onBrowsePaper : undefined}
          />
        )}
      </div>
      <span id={`${id}-date-hint`} className="visually-hidden">
        A full date, a month and year, or just a year.
      </span>
      {/* Polite, so a problem found on leaving a box is read out after the next one's name. */}
      <div className="search__errors" aria-live="polite">
        {fromError && (
          <p id={`${id}-from-error`} className="search__error">
            From: {fromError}
          </p>
        )}
        {toError && (
          <p id={`${id}-to-error`} className="search__error">
            To: {toError}
          </p>
        )}
      </div>
    </form>
  );
}
