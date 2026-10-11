import type { Status, VersionItem, YearPages } from "../api/types";
import { count } from "./format";

/** One decade's pages, for the coverage chart. */
export interface DecadePages {
  /** The decade's first year, e.g. 1890. */
  decade: number;
  pages: number;
}

/**
 * Pages by decade, oldest first, from pages by year. Decades between the
 * first and last with no pages are kept, at zero, so the chart's gaps show.
 */
export function byDecade(years: YearPages[]): DecadePages[] {
  const sums = new Map<number, number>();
  for (const y of years) {
    const d = Math.floor(y.year / 10) * 10;
    sums.set(d, (sums.get(d) ?? 0) + y.pages);
  }
  if (sums.size === 0) return [];
  const decades = [...sums.keys()];
  const first = Math.min(...decades);
  const last = Math.max(...decades);
  const out: DecadePages[] = [];
  for (let d = first; d <= last; d += 10) out.push({ decade: d, pages: sums.get(d) ?? 0 });
  return out;
}

/**
 * Why our OCR read a page, in words: the Library of Congress text it had
 * (`loc_text` in the snapshot's `ja_latin.json`, 04 §4.8).
 */
export const REASON_LABEL: Record<string, string> = {
  missing: "No text from the Library of Congress",
  empty: "The Library of Congress text is empty",
  short: "The Library of Congress text is too short",
  garbled: "The Library of Congress text is garbled",
  mixed: "The Library of Congress text is partly readable",
};

export function reasonLabel(reason: string): string {
  return REASON_LABEL[reason] ?? reason;
}

/** What each build feature (`build.features`, #161) means for a reader. */
const FEATURE_LABEL: Record<string, string> = {
  american_stories: "American Stories' text",
  ja_latin: "the Latin-script text of our Japanese OCR",
  ocr_local: "our own OCR in place of weak Library of Congress text",
  common_grams: "word pairs for phrase searches",
  ja_fold: "folding of old Japanese character forms",
  decades: "the index split by decade",
  text_layout: "one searched field for both texts",
};

const featureLabel = (key: string) => FEATURE_LABEL[key] ?? key;

const on = (v: number | null | undefined) => v !== undefined && v !== null;

/**
 * What a version's build changed from the version before it: features it
 * added or dropped, and feature versions that changed. Empty when either has
 * no build record.
 */
export function featureChanges(v: VersionItem, before: VersionItem | undefined): string[] {
  const now = v.build?.features;
  const then = before?.build?.features;
  if (!now || !then) return [];
  const keys = [...new Set([...Object.keys(now), ...Object.keys(then)])].sort();
  const out: string[] = [];
  for (const k of keys) {
    const a = then[k];
    const b = now[k];
    if (!on(a) && on(b)) out.push(`Added ${featureLabel(k)}`);
    else if (on(a) && !on(b)) out.push(`Dropped ${featureLabel(k)}`);
    else if (on(a) && on(b) && a !== b) out.push(`New version of ${featureLabel(k)} (${String(b)})`);
  }
  return out;
}

/** Published versions, newest first, each with the published one before it. */
export function publishedVersions(all: VersionItem[]): { v: VersionItem; before: VersionItem | undefined }[] {
  const published = all
    .filter((v) => v.status === "published" && v.published_at)
    .sort((a, b) => Date.parse(b.published_at!) - Date.parse(a.published_at!));
  return published.map((v, i) => ({ v, before: published[i + 1] }));
}

/** The change in pages from the version before, signed: "+1,234", "−56", "no change". */
export function pagesChange(v: VersionItem, before: VersionItem | undefined): string {
  if (!before) return "–";
  const d = v.pages - before.pages;
  if (d === 0) return "no change";
  return d > 0 ? `+${count(d)}` : `−${count(-d)}`;
}

const day = new Intl.DateTimeFormat("en-US", { month: "short", day: "numeric", year: "numeric" });

/** "Oct 2, 2026": a date in the visitor's time zone. */
export function dayDate(iso: string): string {
  const t = Date.parse(iso);
  return Number.isNaN(t) ? iso : day.format(t);
}

/** States and territories with pages, when the API lists them. */
export function statesWithPages(s: Status): number | null {
  const rows = s.published.by_state;
  return rows ? rows.filter((r) => r.pages > 0).length : null;
}
