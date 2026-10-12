// One newspaper's pages without a search word (07 §7.13, #274): a list by
// date, from `/v1/hits?lccn=…` with no `q`, each page with the opening of
// its text and its link at the Library of Congress. Shown only when
// `/v1/meta` says the API lists pages this way (`browse_pages`).

import { useEffect, useRef } from "react";
import { infiniteQueryOptions, useInfiniteQuery } from "@tanstack/react-query";
import { api, ApiError } from "../api/client";
import { Hit } from "./PlacePanel";

export type PaperSort = "oldest" | "newest";

/** The list as the panel loads it, a page of 20 at a time. */
export function paperPagesQuery(lccn: string, version: string, sort: PaperSort) {
  return infiniteQueryOptions({
    queryKey: ["paper-pages", version, lccn, sort],
    queryFn: ({ pageParam, signal }) => api.titlePages(lccn, version, sort, pageParam, signal),
    initialPageParam: null as string | null,
    getNextPageParam: (last) => last.next_cursor,
  });
}

/** "12,345 pages on 3,210 days", or "1 page on 1 day". */
export function paperPagesSummary(total: number, days: number | undefined): string {
  const pages = `${total.toLocaleString("en-US")} ${total === 1 ? "page" : "pages"}`;
  if (days === undefined) return pages;
  return `${pages} on ${days.toLocaleString("en-US")} ${days === 1 ? "day" : "days"}`;
}

interface Props {
  lccn: string;
  /** The paper's name from the newspapers' list, until the API's answer names it. */
  name: string;
  version: string;
  sort: PaperSort;
  onSort: (sort: PaperSort) => void;
  /** Synthetic fixtures: their LCCNs are invented, so LoC has no such pages. */
  synthetic: boolean;
  onClose: () => void;
}

export function PaperPages({ lccn, name, version, sort, onSort, synthetic, onClose }: Props) {
  const query = useInfiniteQuery(paperPagesQuery(lccn, version, sort));
  const first = query.data?.pages[0];
  const items = query.data?.pages.flatMap((p) => p.items) ?? [];
  const title = first?.title?.name ?? name;
  const heading = useRef<HTMLHeadingElement>(null);
  // Opening the list moves focus to it, so a keyboard or screen-reader user lands on it.
  useEffect(() => heading.current?.focus(), [lccn]);
  return (
    <section
      className="paper-pages"
      aria-labelledby="paper-pages-title"
      onKeyDown={(e) => {
        if (e.key === "Escape") onClose();
      }}
    >
      <header className="panel__head">
        <h2 id="paper-pages-title" ref={heading} tabIndex={-1}>
          Pages of {title}
        </h2>
        <button type="button" className="button button--quiet" onClick={onClose} aria-label="Close the pages">
          ✕
        </button>
      </header>
      <p className="panel__summary">
        {first ? paperPagesSummary(first.total, first.days) : "Loading…"}
        {" · every page, with no search word"}
      </p>
      <div className="segmented" role="group" aria-label="Order of pages">
        {(["oldest", "newest"] as const).map((s) => (
          <button key={s} type="button" aria-pressed={sort === s} onClick={() => onSort(s)}>
            {s === "oldest" ? "Oldest first" : "Newest first"}
          </button>
        ))}
      </div>
      {query.error && (
        <p role="alert" className="notice notice--error">
          {query.error instanceof ApiError
            ? (query.error.problem.hint ?? query.error.message)
            : "Could not load the pages."}
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
    </section>
  );
}
