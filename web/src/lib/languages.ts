// Title languages for the relative-rate view (doc 11, 11.14) and the
// language filter's names (07 §7.9). An English
// search term rarely matches pages printed in another language, so a place
// with such papers can read low; the view says which languages its papers
// are in. Codes and names follow the catalog's table (crates/usnm-core/src/
// names.rs once #66 merges; crates/usnm-ingest/src/titles.rs before it).

const NAMES: Record<string, string> = {
  eng: "English",
  ger: "German",
  spa: "Spanish",
  fre: "French",
  ita: "Italian",
  pol: "Polish",
  cze: "Czech",
  slo: "Slovak",
  slv: "Slovenian",
  lit: "Lithuanian",
  swe: "Swedish",
  nor: "Norwegian",
  dan: "Danish",
  dut: "Dutch",
  fin: "Finnish",
  hun: "Hungarian",
  yid: "Yiddish",
  heb: "Hebrew",
  chi: "Chinese",
  jpn: "Japanese",
  por: "Portuguese",
  rus: "Russian",
  ukr: "Ukrainian",
  gre: "Greek",
  rum: "Romanian",
  hrv: "Croatian",
  srp: "Serbian",
  ara: "Arabic",
  arm: "Armenian",
  wel: "Welsh",
  ice: "Icelandic",
  lat: "Latin",
  haw: "Hawaiian",
  chr: "Cherokee",
  cho: "Choctaw",
  dak: "Dakota",
  oji: "Ojibwa",
  baq: "Basque",
  gle: "Irish",
  tgl: "Tagalog",
  kor: "Korean",
  est: "Estonian",
  lav: "Latvian",
  bul: "Bulgarian",
  alb: "Albanian",
  mus: "Creek",
};

/**
 * The name for a catalog code. `fallback` (such as the API's name for it) is
 * used for a code this table doesn't list.
 */
export function languageName(code: string, fallback?: string): string {
  const known = NAMES[code];
  if (known) return known;
  if (fallback) return fallback;
  // Unlisted languages arrive as the catalog's lowercase name, which may
  // have several words ("pennsylvania german"): title-case each word.
  return code.replace(/(^|[\s-])(\p{L})/gu, (_, sep: string, ch: string) => sep + ch.toUpperCase());
}

/**
 * "Papers in German", "Papers in Serbian and English", or null when every
 * listed language is English (or none is listed). Other languages first,
 * English last.
 */
export function languageLabel(codes: readonly string[] | undefined): string | null {
  const list = [...new Set(codes ?? [])];
  if (!list.some((c) => c !== "eng")) return null;
  const names = list
    .filter((c) => c !== "eng")
    .map((c) => languageName(c))
    .sort((a, b) => a.localeCompare(b));
  if (list.includes("eng")) names.push("English");
  const joined = names.length === 1 ? names[0]! : `${names.slice(0, -1).join(", ")} and ${names[names.length - 1]}`;
  return `Papers in ${joined}`;
}

const percent = (n: number, of: number) => {
  const p = (100 * n) / of;
  return p > 0 && p < 1 ? "under 1%" : `${Math.round(p)}%`;
};

/**
 * "Of its 5 papers, 3 are in French (60%), 1 in English (20%) and 1 in German
 * (20%)." for the info button beside a language label, or null when the API
 * doesn't send the counts. Most papers first. A paper in several languages
 * counts in each, so the shares can add up to more than 100%.
 */
export function languageCounts(counts: Record<string, number> | undefined, titles: number): string | null {
  const entries = Object.entries(counts ?? {})
    .filter(([, n]) => n > 0)
    .map(([code, n]) => ({ name: languageName(code), n }))
    .sort((a, b) => b.n - a.n || a.name.localeCompare(b.name));
  if (entries.length === 0 || titles <= 0) return null;
  const join = (xs: string[]) => (xs.length === 1 ? xs[0]! : `${xs.slice(0, -1).join(", ")} and ${xs[xs.length - 1]}`);
  if (titles === 1) return `Its one paper is in ${join(entries.map((e) => e.name))}.`;
  const parts = entries.map(
    ({ name, n }, i) =>
      `${n.toLocaleString("en-US")}${i === 0 ? (n === 1 ? " is" : " are") : ""} in ${name} (${percent(n, titles)})`,
  );
  const sentence = `Of its ${titles.toLocaleString("en-US")} papers, ${join(parts)}.`;
  const listed = entries.reduce((a, e) => a + e.n, 0);
  return listed > titles ? `${sentence} A paper in more than one language counts in each.` : sentence;
}

/**
 * "Matches in 312 newspapers: English 96%, German 3% and Spanish 1%." under a
 * search's summary (#121), or null with fewer than two languages. Each share
 * is the matching pages of papers in that language against all matching
 * pages; a page of a paper in several languages counts in each, so the
 * shares can add up to more than 100%. At most five languages, then "N more".
 * Counted by page language (`by: "page"`, #289): "Matches in 12 newspapers:
 * pages in English 90% and German 12%.", a page in two languages in each.
 */
export function languageMix(
  langs: { code: string[]; hits: number[]; by?: "page" } | undefined,
  totalHits: number,
  papers: number | undefined,
): string | null {
  if (!langs || langs.code.length < 2 || totalHits <= 0) return null;
  const shown = langs.code.slice(0, 5).map((c, i) => `${languageName(c)} ${percent(langs.hits[i] ?? 0, totalHits)}`);
  const more = langs.code.length - shown.length;
  if (more > 0) shown.push(`${more} more`);
  const list = shown.length === 1 ? shown[0]! : `${shown.slice(0, -1).join(", ")} and ${shown[shown.length - 1]}`;
  const where =
    papers === undefined
      ? "Matches"
      : `Matches in ${papers.toLocaleString("en-US")} ${papers === 1 ? "newspaper" : "newspapers"}`;
  return `${where}: ${langs.by === "page" ? "pages in " : ""}${list}.`;
}
