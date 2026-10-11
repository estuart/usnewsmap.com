import { useState } from "react";
import type { AggregateResponse } from "../api/types";
import { locTitleUrl } from "./NewspaperFilter";

/** Rows shown before "Show all". */
const FIRST = 25;

export interface PaperRow {
  lccn: string;
  title: string;
  place: string;
  hits: number;
}

/** The aggregate's newspapers as rows, with place names joined. */
export function paperRows(papers: AggregateResponse["papers"], placeName: (id: string) => string): PaperRow[] {
  if (!papers) return [];
  return papers.lccn.map((lccn, i) => {
    const place = papers.place_id[i];
    return {
      lccn,
      title: papers.title[i] ?? lccn,
      place: place ? placeName(place) : "",
      hits: papers.hits[i] ?? 0,
    };
  });
}

/** RFC 4180 CSV of the listed newspapers. */
export function papersCsv(rows: PaperRow[]): string {
  const cell = (v: string | number) => {
    const s = String(v);
    return /[",\n\r]/.test(s) ? `"${s.replace(/"/g, '""')}"` : s;
  };
  const lines = [["lccn", "newspaper", "place", "matching_pages"].join(",")];
  for (const r of rows) lines.push([r.lccn, r.title, r.place, r.hits].map(cell).join(","));
  return `${lines.join("\r\n")}\r\n`;
}

interface Props {
  rows: PaperRow[];
  /** Every newspaper with a match; more than `rows` when the API capped the list. */
  total: number;
  /** Limit the search to one newspaper (the `lccn` filter). */
  onOnly: (lccn: string) => void;
  filename: string;
}

/** The newspapers carrying the matches, most first (#121). */
export function NewspaperTable({ rows, total, onOnly, filename }: Props) {
  const [all, setAll] = useState(false);
  const shown = all ? rows : rows.slice(0, FIRST);
  return (
    <section className="table-wrap newspapers" aria-labelledby="newspapers-heading">
      <h2 id="newspapers-heading">Newspapers</h2>
      <p className="newspapers__note">
        {total.toLocaleString("en-US")} {total === 1 ? "newspaper has" : "newspapers have"} matching pages between the
        search&apos;s dates
        {rows.length < total && `; the ${rows.length.toLocaleString("en-US")} with the most are listed`}.
      </p>
      <table className="papers">
        <thead>
          <tr>
            <th scope="col">Newspaper</th>
            <th scope="col">Place</th>
            <th scope="col" className="num">
              Matching pages
            </th>
            <th scope="col">
              <span className="visually-hidden">Filter</span>
            </th>
          </tr>
        </thead>
        <tbody>
          {shown.map((r) => (
            <tr key={r.lccn}>
              <th scope="row">{r.title}</th>
              <td>{r.place}</td>
              <td className="num">{r.hits.toLocaleString("en-US")}</td>
              <td className="papers__actions">
                <button type="button" className="button button--quiet" onClick={() => onOnly(r.lccn)}>
                  Only this newspaper<span className="visually-hidden">: {r.title}</span>
                </button>
                {/* Its issues by date, which a search here can't list without a word (07 §7.13). */}
                <a href={locTitleUrl(r.lccn)} target="_blank" rel="noopener noreferrer">
                  Issues <span className="visually-hidden">of {r.title}</span> at loc.gov
                </a>
              </td>
            </tr>
          ))}
        </tbody>
      </table>
      <div className="newspapers__actions">
        {rows.length > FIRST && (
          <button type="button" className="button" onClick={() => setAll(!all)} aria-expanded={all}>
            {all ? `Show the first ${FIRST}` : `Show all ${rows.length.toLocaleString("en-US")}`}
          </button>
        )}
        <button
          type="button"
          className="button"
          onClick={() => {
            const url = URL.createObjectURL(new Blob([papersCsv(rows)], { type: "text/csv" }));
            const a = document.createElement("a");
            a.href = url;
            a.download = filename;
            a.click();
            setTimeout(() => URL.revokeObjectURL(url), 1000);
          }}
        >
          Download newspapers CSV
        </button>
      </div>
    </section>
  );
}
