import type { ReactNode } from "react";
import { relative, when } from "./format";

/** The dataset details page's small building blocks. */

export function Unavailable({ reason }: { reason: string }) {
  return (
    <p className="notice" role="note">
      Not available. {reason}
    </p>
  );
}

export function Bar({ value, max, label }: { value: number; max: number; label: string }) {
  const pct = max > 0 ? Math.min(100, (100 * value) / max) : 0;
  return (
    <div
      className="progress"
      role="progressbar"
      aria-valuemin={0}
      aria-valuemax={max}
      aria-valuenow={value}
      aria-valuetext={label}
      aria-label={label}
    >
      <div className="progress__fill" style={{ width: `${pct}%` }} />
    </div>
  );
}

export function Stats({ items }: { items: [string, ReactNode][] }) {
  return (
    <dl className="stats">
      {items.map(([k, v]) => (
        <div key={k}>
          <dt>{k}</dt>
          <dd>{v}</dd>
        </div>
      ))}
    </dl>
  );
}

export function Table({
  caption,
  head,
  rows,
  empty,
  numeric = [],
}: {
  caption: string;
  head: string[];
  rows: ReactNode[][];
  empty: string;
  /** Columns (by index) that hold numbers, aligned to the right. */
  numeric?: number[];
}) {
  if (rows.length === 0) return <p className="status-empty">{empty}</p>;
  const cls = (j: number) => (numeric.includes(j) ? "num" : undefined);
  return (
    // Focusable so keyboard users can scroll a wide table on a narrow screen.
    <div className="table-scroll" tabIndex={0} role="region" aria-label={caption}>
      <table className="places status-table">
        <caption>{caption}</caption>
        <thead>
          <tr>
            {head.map((h, j) => (
              <th key={h} scope="col" className={cls(j)}>
                {h}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.map((r, i) => (
            <tr key={i}>
              {r.map((c, j) => (
                <td key={j} className={cls(j)}>
                  {c}
                </td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

export function Time({ iso, now }: { iso: string | null; now: number }) {
  if (!iso) return <>–</>;
  return (
    <time dateTime={iso} title={when(iso)}>
      {relative(iso, now)}
    </time>
  );
}

/** A link that opens another site in a new tab. */
export function Ext({ href, children }: { href: string; children: ReactNode }) {
  return (
    <a href={href} target="_blank" rel="noopener noreferrer">
      {children}
    </a>
  );
}
