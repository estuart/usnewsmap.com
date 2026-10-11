// The URL is the source of truth for the view (07 §7.5): every state is a
// permalink, and only non-default values are written.

import { useCallback, useSyncExternalStore } from "react";
import type { SearchParams } from "../api/client";
import type { BucketUnit, HitSort, Mode } from "../api/types";
import { MAX_ZOOM, MIN_ZOOM } from "../lib/mapLimits";

export type Layer = "points" | "heat";
/**
 * What the map measures: pages with a match (`raw`), the relative rate
 * against other places (`skew`, doc 11), or when each place's matches
 * fell, by their median date (`when`, #127). The older share of pages
 * published (`norm=rel`) was removed; its permalinks open on Pages.
 */
export type Norm = "raw" | "skew" | "when";
export type Source = "news" | "congress";

/** The Congress series the API knows (`CONGRESS_SERIES` in usnm_core::params). */
export const CONGRESS_SERIES = ["annals", "globe", "register", "serialset"] as const;

/** Congress series from a URL: known names only, sorted, no repeats. */
export function parseSeries(v: string | null): string[] {
  const known: readonly string[] = CONGRESS_SERIES;
  return [...new Set((v ?? "").split(",").map((x) => x.trim().toLowerCase()))].filter((x) => known.includes(x)).sort();
}
export type Tab = "map" | "table";

export interface ViewState {
  q: string;
  mode: Mode;
  near: number;
  from: string;
  to: string;
  bucket: BucketUnit | "auto";
  state: string[];
  /** Newspaper languages (catalog codes such as "ger"), lowercase, sorted, no repeats (07 §7.9). */
  lang: string[];
  /** Newspapers (LCCNs), lowercase, sorted, no repeats: the API's `lccn` filter (#121). */
  lccn: string[];
  /** The corpus searched (#229): the newspapers, or the debates and documents of Congress. */
  source: Source;
  /** Congress series (`globe`, `serialset`, ...), sorted, no repeats; only with `source` congress. */
  series: string[];
  /** Scrubber position (ISO date); empty = end of range. */
  t: string;
  /** Trailing window in buckets; null = cumulative. */
  win: number | null;
  layer: Layer;
  norm: Norm;
  place: string;
  /** Order of the selected place's pages. */
  sort: HitSort;
  tab: Tab;
  /** Map viewport: zoom and center (lon, lat). */
  z: number | null;
  c: [number, number] | null;
}

export const DEFAULTS: ViewState = {
  q: "",
  mode: "phrase",
  near: 5,
  from: "",
  to: "",
  bucket: "auto",
  state: [],
  lang: [],
  lccn: [],
  source: "news",
  series: [],
  t: "",
  win: null,
  layer: "points",
  norm: "raw",
  place: "",
  sort: "oldest",
  tab: "map",
  z: null,
  c: null,
};

const MODES: Mode[] = ["phrase", "all", "any", "near"];
const BUCKETS = ["auto", "year", "month", "week", "day"] as const;
const ISO = /^\d{4}-\d{2}-\d{2}$/;

/** A real calendar date in YYYY-MM-DD form (rejects e.g. 1896-02-31). */
export function isIsoDate(v: string): boolean {
  if (!ISO.test(v)) return false;
  const [y, m, d] = v.split("-").map(Number) as [number, number, number];
  const date = new Date(Date.UTC(y, m - 1, d));
  return date.getUTCFullYear() === y && date.getUTCMonth() === m - 1 && date.getUTCDate() === d;
}

/**
 * Language codes as the API's canonical form has them: three letters,
 * lowercase, sorted, without repeats. Anything else is dropped.
 */
export function parseLangs(v: string | null): string[] {
  const codes = (v ?? "")
    .split(",")
    .map((x) => x.trim().toLowerCase())
    .filter((x) => /^[a-z]{3}$/.test(x));
  return [...new Set(codes)].sort().slice(0, 60);
}

/**
 * LCCNs as the API accepts them: lowercase letters and digits, sorted,
 * without repeats, at most 60 (the API's limit for a list parameter).
 */
export function parseLccns(v: string | null): string[] {
  const codes = (v ?? "")
    .split(",")
    .map((x) => x.trim().toLowerCase())
    .filter((x) => /^[a-z0-9]{1,16}$/.test(x));
  return [...new Set(codes)].sort().slice(0, 60);
}

/**
 * The relative rate needs pages published as its baseline, which aren't
 * kept per newspaper (06 §6.3.3): under a newspaper filter it is Pages,
 * whether the view came from a click or a shared link.
 */
export function supportedNorm(norm: Norm, lccn: readonly string[], source: Source = "news"): Norm {
  // Congress pages have no baselines yet (#229), so no relative rate either.
  return norm === "skew" && (lccn.length > 0 || source !== "news") ? "raw" : norm;
}

function oneOf<T extends string>(v: string | null, allowed: readonly T[], d: T): T {
  return v !== null && (allowed as readonly string[]).includes(v) ? (v as T) : d;
}

function int(v: string | null, min: number, max: number): number | null {
  if (v === null || !/^\d+$/.test(v)) return null;
  const n = Number(v);
  return n >= min && n <= max ? n : null;
}

export function parseView(search: string): ViewState {
  const s = new URLSearchParams(search);
  const date = (k: string) => {
    const v = s.get(k) ?? "";
    return isIsoDate(v) ? v : "";
  };
  // Number("") is 0, so blank values are rejected before converting.
  const num = (v: string | null | undefined) => (v === null || v === undefined || v.trim() === "" ? NaN : Number(v));
  const z = num(s.get("z"));
  const c = (s.get("c") ?? "").split(",").map(num);
  // A Congress search has no newspapers to filter by, and its form hides the
  // state and language filters, so a permalink's would hide every page (#229).
  const congress = s.get("source") === "congress";
  return {
    q: (s.get("q") ?? "").slice(0, 256),
    mode: oneOf(s.get("mode"), MODES, DEFAULTS.mode),
    near: int(s.get("near"), 1, 20) ?? DEFAULTS.near,
    from: date("from"),
    to: date("to"),
    bucket: oneOf(s.get("bucket"), BUCKETS, DEFAULTS.bucket),
    state: congress
      ? []
      : (s.get("state") ?? "")
          .split(",")
          .map((x) => x.trim().toUpperCase())
          .filter((x) => /^[A-Z]{2}$/.test(x)),
    lang: congress ? [] : parseLangs(s.get("lang")),
    lccn: congress ? [] : parseLccns(s.get("lccn")),
    source: oneOf(s.get("source"), ["news", "congress"] as const, DEFAULTS.source),
    series: congress ? parseSeries(s.get("series")) : [],
    t: date("t"),
    win: s.get("win") === "cum" || s.get("win") === null ? null : int(s.get("win"), 1, 1000),
    layer: oneOf(s.get("layer"), ["points", "heat"] as const, DEFAULTS.layer),
    norm: supportedNorm(
      oneOf(s.get("norm"), ["raw", "skew", "when"] as const, DEFAULTS.norm),
      parseLccns(s.get("lccn")),
      oneOf(s.get("source"), ["news", "congress"] as const, DEFAULTS.source),
    ),
    place: /^[A-Za-z0-9_-]{1,32}$/.test(s.get("place") ?? "") ? (s.get("place") as string) : "",
    sort: oneOf(s.get("sort"), ["oldest", "newest", "relevant"] as const, DEFAULTS.sort),
    tab: oneOf(s.get("tab"), ["map", "table"] as const, DEFAULTS.tab),
    z: Number.isFinite(z) && z >= MIN_ZOOM && z <= MAX_ZOOM ? z : null,
    c:
      c.length === 2 && c.every(Number.isFinite) && Math.abs(c[0]!) <= 180 && Math.abs(c[1]!) <= 90
        ? [c[0]!, c[1]!]
        : null,
  };
}

export function serializeView(v: ViewState): string {
  const s = new URLSearchParams();
  const put = (k: string, val: string, d: string) => {
    if (val !== d) s.set(k, val);
  };
  put("q", v.q, DEFAULTS.q);
  put("mode", v.mode, DEFAULTS.mode);
  if (v.mode === "near") put("near", String(v.near), String(DEFAULTS.near));
  put("from", v.from, "");
  put("to", v.to, "");
  put("bucket", v.bucket, DEFAULTS.bucket);
  put("state", v.source === "congress" ? "" : v.state.join(","), "");
  put("lang", v.source === "congress" ? "" : parseLangs(v.lang.join(",")).join(","), "");
  put("lccn", v.source === "congress" ? "" : parseLccns(v.lccn.join(",")).join(","), "");
  put("source", v.source, DEFAULTS.source);
  if (v.source === "congress") put("series", parseSeries(v.series.join(",")).join(","), "");
  put("t", v.t, "");
  if (v.win !== null) s.set("win", String(v.win));
  put("layer", v.layer, DEFAULTS.layer);
  put("norm", supportedNorm(v.norm, v.lccn, v.source), DEFAULTS.norm);
  put("place", v.place, "");
  put("sort", v.sort, DEFAULTS.sort);
  put("tab", v.tab, DEFAULTS.tab);
  if (v.z !== null) s.set("z", v.z.toFixed(2));
  if (v.c) s.set("c", `${v.c[0].toFixed(3)},${v.c[1].toFixed(3)}`);
  const out = s.toString();
  return out ? `?${out}` : "";
}

/** The search a view asks the API for (the rest of the view is display state). */
export function searchParams(v: ViewState): SearchParams {
  return {
    q: v.q,
    mode: v.mode,
    near: v.near,
    from: v.from,
    to: v.to,
    bucket: v.bucket,
    // Newspaper filters only: a Congress search sends none (#229).
    state: v.source === "congress" ? [] : v.state,
    lang: v.source === "congress" ? [] : v.lang,
    lccn: v.source === "congress" ? [] : v.lccn,
    // Only a Congress search names its source, so a newspaper search's
    // parameters, and the query keys built from them, are as before.
    ...(v.source === "congress" ? { source: v.source, series: v.series } : {}),
  };
}

function subscribe(cb: () => void): () => void {
  window.addEventListener("popstate", cb);
  return () => window.removeEventListener("popstate", cb);
}

const getSearch = () => window.location.search;

/**
 * The view state and a setter. `push` adds a history entry (new searches);
 * everything else (scrubbing, panning) replaces the current one.
 */
export function useView(): [ViewState, (patch: Partial<ViewState>, push?: boolean) => void] {
  const search = useSyncExternalStore(subscribe, getSearch, getSearch);
  const view = parseView(search);
  const set = useCallback((patch: Partial<ViewState>, push = false) => {
    const next = { ...parseView(window.location.search), ...patch };
    const url = `${window.location.pathname}${serializeView(next)}`;
    if (push) window.history.pushState(null, "", url);
    else window.history.replaceState(null, "", url);
    window.dispatchEvent(new PopStateEvent("popstate"));
  }, []);
  return [view, set];
}
