//! The page: one server-rendered HTML document that refreshes itself.
//! Every value from outside goes through [`esc`].

use std::fmt::Write as _;

use chrono::{DateTime, Utc};

use crate::view::{NextRow, View};

/// HTML-escape text for an element or a quoted attribute.
pub fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            c => o.push(c),
        }
    }
    o
}

/// Bytes in decimal units.
pub fn bytes(n: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1000.0 && u < units.len() - 1 {
        v /= 1000.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else if v >= 100.0 {
        format!("{v:.0} {}", units[u])
    } else {
        format!("{v:.1} {}", units[u])
    }
}

/// A span of time: `2d 4h`, `3h 12m`, `5m 3s`, `40s`.
pub fn span(secs: u64) -> String {
    let (d, h, m, s) = (
        secs / 86400,
        secs % 86400 / 3600,
        secs % 3600 / 60,
        secs % 60,
    );
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

pub fn int(n: u64) -> String {
    let s = n.to_string();
    let mut o = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            o.push(',');
        }
        o.push(c);
    }
    o
}

fn pct(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        (part as f64 / whole as f64 * 100.0).min(100.0)
    }
}

/// A time, in the reader's zone once the page's script runs, UTC before.
fn time(t: DateTime<Utc>) -> String {
    let iso = t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    format!(
        "<time datetime=\"{iso}\">{}</time>",
        t.format("%Y-%m-%d %H:%M:%S UTC")
    )
}

/// [`time`] for an RFC 3339 string; the string itself if it isn't one.
fn time_str(s: &str) -> String {
    match DateTime::parse_from_rfc3339(s) {
        Ok(t) => time(t.with_timezone(&Utc)),
        Err(_) => esc(s),
    }
}

fn ago(now: DateTime<Utc>, t: DateTime<Utc>) -> String {
    let s = (now - t).num_seconds().max(0) as u64;
    format!("{} ago", span(s))
}

/// A progress track with its label beside it.
fn meter(part: u64, whole: u64, label: &str) -> String {
    let p = pct(part, whole);
    format!(
        "<span class=\"meter\" role=\"meter\" aria-valuemin=\"0\" aria-valuemax=\"100\" aria-valuenow=\"{p:.1}\"><span style=\"width:{p:.2}%\"></span></span> <span class=\"num\">{}</span>",
        esc(label)
    )
}

/// A state with its icon and word, never color alone.
fn badge(kind: &str, text: &str) -> String {
    let icon = match kind {
        "good" => "&#10003;",
        "warning" => "!",
        "critical" => "&#10007;",
        _ => "&#8226;",
    };
    format!(
        "<span class=\"badge {kind}\"><span aria-hidden=\"true\">{icon}</span> {}</span>",
        esc(text)
    )
}

const STYLE: &str = r#"
:root { --surface:#fcfcfb; --plane:#f9f9f7; --ink:#0b0b0b; --ink2:#52514e; --muted:#898781;
  --grid:#e1e0d9; --track:#e1e0d9; --fill:#2a78d6; --good:#0ca30c; --warning:#fab219; --critical:#d03b3b;
  --ring:rgba(11,11,11,0.10); }
@media (prefers-color-scheme: dark) { :root { --surface:#1a1a19; --plane:#0d0d0d; --ink:#ffffff; --ink2:#c3c2b7;
  --grid:#2c2c2a; --track:#383835; --fill:#3987e5; --ring:rgba(255,255,255,0.10); } }
* { box-sizing: border-box; }
body { margin:0; padding:16px 20px 40px; background:var(--plane); color:var(--ink);
  font: 14px/1.45 system-ui, -apple-system, "Segoe UI", sans-serif; }
h1 { font-size:20px; margin:0 0 4px; } h2 { font-size:16px; margin:0 0 10px; } h3 { font-size:14px; margin:14px 0 6px; }
.sub { color:var(--ink2); margin-bottom:16px; }
.grid { display:grid; grid-template-columns: repeat(auto-fit, minmax(min(560px, 100%), 1fr)); gap:16px; }
.grid > section { min-width:0; }
section { background:var(--surface); border:1px solid var(--ring); border-radius:8px; padding:14px 16px; overflow-x:auto; }
table { border-collapse:collapse; width:100%; font-variant-numeric: tabular-nums; }
th, td { text-align:left; padding:3px 8px 3px 0; border-bottom:1px solid var(--grid); vertical-align:top; white-space:nowrap; }
th { color:var(--ink2); font-weight:600; }
td.r, th.r { text-align:right; }
.muted, .note { color:var(--ink2); } .note { white-space:normal; font-size:13px; }
code { font-size:12.5px; overflow-wrap:anywhere; } a { color:var(--fill); }
.meter { display:inline-block; width:140px; height:8px; background:var(--track); border-radius:4px; vertical-align:middle; overflow:hidden; }
.meter > span { display:block; height:100%; background:var(--fill); border-radius:4px; }
.num { color:var(--ink2); }
.badge { display:inline-block; padding:0 6px; border-radius:4px; border:1px solid var(--ring); font-weight:600; }
.badge.good > span { color:var(--good); } .badge.warning > span { color:var(--warning); } .badge.critical > span { color:var(--critical); }
.kv td:first-child { color:var(--ink2); width:1%; }
.err { color:var(--critical); white-space:normal; overflow-wrap:anywhere; }
/* Phones: one column, no page-wide scroll. Two-column tables become
   label-over-value cards; wider tables wrap, and scroll inside their panel
   if they still don't fit. */
@media (max-width: 640px) {
  body { padding:10px 10px 32px; }
  section { padding:10px 12px; }
  .grid { gap:10px; }
  th { white-space:nowrap; padding-right:6px; }
  td { white-space:normal; overflow-wrap:break-word; padding-right:6px; }
  td code { word-break:break-all; }
  table.kv, table.kv tbody, table.kv tr, table.kv td { display:block; width:auto; }
  table.kv td:first-child { width:auto; border-bottom:none; padding:6px 0 0; font-weight:600; }
  table { width:100%; table-layout:auto; }
  table.kv td + td { padding-top:0; }
  .meter { width:84px; }
}
"#;

/// Turns each `<time>` into the reader's local time.
const SCRIPT: &str = r#"
for (const t of document.querySelectorAll('time[datetime]')) {
  const d = new Date(t.getAttribute('datetime'));
  if (!isNaN(d)) t.textContent = d.toLocaleString(undefined, {month:'short', day:'numeric', hour:'2-digit', minute:'2-digit', second:'2-digit', timeZoneName:'short'});
}
"#;

fn next_table(o: &mut String, rows: &[NextRow], with_fetched: bool) {
    if rows.is_empty() {
        o.push_str("<p class=\"muted\">None.</p>");
        return;
    }
    o.push_str("<table><tr><th>#</th><th>doc_id</th><th>Lang</th><th>Group</th>");
    if with_fetched {
        o.push_str("<th>Scan</th>");
    }
    o.push_str("<th></th></tr>");
    for (i, r) in rows.iter().enumerate() {
        let _ = write!(
            o,
            "<tr><td class=\"r\">{}</td><td><code>{}</code></td><td>{}</td><td>{}</td>",
            i + 1,
            esc(&r.doc_id),
            esc(&r.lang),
            esc(r.group)
        );
        if with_fetched {
            let _ = write!(
                o,
                "<td>{}</td>",
                if r.fetched { "on pve-big" } else { "not yet" }
            );
        }
        let _ = write!(
            o,
            "<td>{}</td></tr>",
            if r.in_flight { "in flight" } else { "" }
        );
    }
    o.push_str("</table>");
}

pub fn render(v: &View) -> String {
    let now = v.generated_at;
    let mut o = String::with_capacity(64 * 1024);
    let _ = write!(
        o,
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta http-equiv=\"refresh\" content=\"{}\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>usnm OCR, local</title><style>{STYLE}</style></head><body>\
         <h1>usnm OCR, local</h1><div class=\"sub\">The scan fetch, the nodes and the hosts on the LAN. \
         Read-only; no page text. Made {}; refreshes every {}s. \
         JSON: <a href=\"/api/view.json\">/api/view.json</a>.</div><div class=\"grid\">",
        v.refresh_secs,
        time(now),
        v.refresh_secs
    );

    // 1. Pool and hosts.
    o.push_str("<section><h2>Pool and hosts</h2>");
    if v.hosts.is_empty() {
        o.push_str("<p class=\"muted\">No host stats yet.</p>");
    }
    for h in &v.hosts {
        let _ = write!(o, "<h3>{}</h3>", esc(&h.name));
        if let Some(e) = &h.error {
            let _ = write!(o, "<p class=\"err\">{}</p>", esc(e));
        }
        if let Some(r) = h.collected_at {
            let stale = h.age_secs.unwrap_or(0) > 300;
            let _ = write!(
                o,
                "<table class=\"kv\"><tr><td>Stats</td><td>collected {} {}</td></tr>",
                ago(now, r),
                if stale {
                    badge("warning", "stale")
                } else {
                    String::new()
                }
            );
            let _ = write!(
                o,
                "<tr><td>CPU</td><td>{} of {} CPUs; load {}</td></tr>\
                 <tr><td>Memory</td><td>{}</td></tr><tr><td>Up</td><td>{}</td></tr></table>",
                meter(
                    (h.cpu_pct * 10.0) as u64,
                    1000,
                    &format!("{:.1}%", h.cpu_pct)
                ),
                h.cpus,
                esc(&h
                    .load
                    .iter()
                    .map(|l| format!("{l:.2}"))
                    .collect::<Vec<_>>()
                    .join(" / ")),
                meter(
                    h.mem_used,
                    h.mem_total,
                    &format!("{} of {}", bytes(h.mem_used), bytes(h.mem_total))
                ),
                span(h.uptime_secs)
            );
        }
        for p in &h.pools {
            let health = if p.health == "ONLINE" {
                badge("good", &p.health)
            } else {
                badge("critical", &p.health)
            };
            let _ = write!(
                o,
                "<table class=\"kv\"><tr><td>Pool <code>{}</code></td><td>{health} {}</td></tr>\
                 <tr><td>Capacity</td><td>{}</td></tr><tr><td>Fragmentation</td><td>{}%</td></tr>\
                 <tr><td>Last scan</td><td>{}</td></tr><tr><td>Errors</td><td>{}</td></tr></table>",
                esc(&p.name),
                esc(&p.status_x),
                meter(
                    p.alloc,
                    p.size,
                    &format!("{} of {} ({}%)", bytes(p.alloc), bytes(p.size), p.cap_pct)
                ),
                p.frag_pct,
                esc(&p.scan),
                esc(&p.errors)
            );
        }
        if !h.datasets.is_empty() {
            o.push_str("<table><tr><th>Dataset</th><th>Used</th><th class=\"r\">Logical</th><th class=\"r\">Ratio</th></tr>");
            for d in &h.datasets {
                let used = if d.quota > 0 {
                    meter(
                        d.used,
                        d.quota,
                        &format!("{} of {} quota", bytes(d.used), bytes(d.quota)),
                    )
                } else {
                    bytes(d.used)
                };
                let _ = write!(
                    o,
                    "<tr><td><code>{}</code></td><td>{used}</td><td class=\"r\">{}</td><td class=\"r\">{:.2}x</td></tr>",
                    esc(&d.name),
                    bytes(d.logicalused),
                    d.compressratio
                );
            }
            o.push_str("</table>");
        }
        if !h.guests.is_empty() {
            o.push_str("<h3>Guests</h3><table><tr><th>ID</th><th>Name</th><th>State</th><th class=\"r\">CPU</th><th>Memory</th><th class=\"r\">Up</th></tr>");
            for g in &h.guests {
                let st = if g.status == "running" {
                    badge("good", &g.status)
                } else {
                    badge("critical", &g.status)
                };
                let _ = write!(
                    o,
                    "<tr><td>{} {}</td><td>{}</td><td>{st}</td><td class=\"r\">{:.1}% of {}</td><td>{}</td><td class=\"r\">{}</td></tr>",
                    if g.kind == "lxc" { "CT" } else { "VM" },
                    g.vmid,
                    esc(&g.name),
                    g.cpu_pct,
                    g.cpus,
                    meter(g.mem, g.maxmem, &format!("{} of {}", bytes(g.mem), bytes(g.maxmem))),
                    span(g.uptime_secs)
                );
            }
            o.push_str("</table>");
        }
    }
    o.push_str("</section>");

    // 2. Downloads.
    let f = &v.fetch;
    o.push_str("<section><h2>Scan downloads</h2>");
    if let Some(e) = &f.status_error {
        let _ = write!(o, "<p class=\"err\">{}</p>", esc(e));
    }
    if let Some(e) = &f.plan_error {
        let _ = write!(o, "<p class=\"err\">Manifest: {}</p>", esc(e));
    }
    if let Some(s) = &f.status {
        let state = match s.state.as_str() {
            "running" | "done" => badge("good", &s.state),
            "waiting" | "budget" | "stopped" => badge("warning", &s.state),
            other => badge("critical", other),
        };
        let hold = match s.loc_hold_secs {
            Some(h) => format!(
                " {}",
                badge("warning", &format!("LoC hold, {} left", span(h)))
            ),
            None => String::new(),
        };
        let _ = write!(
            o,
            "<table class=\"kv\"><tr><td>Fetcher</td><td>{state}{hold} since {}, version {}</td></tr>\
             <tr><td>This run</td><td>{}</td></tr>\
             <tr><td>Last page</td><td><code>{}</code>, status written {}</td></tr>",
            time_str(&s.started_at),
            esc(&s.version),
            meter(
                s.finished,
                s.pages,
                &format!(
                    "{} of {} pages ({} already there, {} fetched, {} failed)",
                    int(s.finished),
                    int(s.pages),
                    int(s.already_there),
                    int(s.fetched),
                    int(s.failed_permanent + s.failed_transient)
                )
            ),
            esc(&s.last_doc_id),
            time_str(&s.updated_at)
        );
        if let Some(e) = &s.error {
            let _ = write!(
                o,
                "<tr><td>Error</td><td class=\"err\">{}</td></tr>",
                esc(e)
            );
        }
        let _ = write!(
            o,
            "<tr><td>Buffer</td><td>{}</td></tr></table>",
            meter(
                s.buffer_bytes,
                s.buffer_cap,
                &format!("{} of {} cap", bytes(s.buffer_bytes), bytes(s.buffer_cap))
            )
        );
    }
    let _ = write!(
        o,
        "<table><tr><th></th><th class=\"r\">Pages</th><th class=\"r\">Pages/h</th><th class=\"r\">MB/s</th><th class=\"r\">Data</th><th class=\"r\">LoC holds</th><th class=\"r\">Failures</th></tr>\
         <tr><td>Last hour</td><td class=\"r\">{}</td><td class=\"r\">{:.0}</td><td class=\"r\">{:.2}</td><td class=\"r\">{}</td><td class=\"r\">{}</td><td class=\"r\">{}</td></tr>\
         <tr><td>Last day</td><td class=\"r\">{}</td><td class=\"r\">{:.0}</td><td class=\"r\">{:.2}</td><td class=\"r\">{}</td><td class=\"r\">{}</td><td class=\"r\">{}</td></tr></table>",
        int(f.last_hour.pages), f.last_hour.pages_per_hour, f.last_hour.mb_per_s, bytes(f.last_hour.bytes), f.holds_last_hour, f.failures_last_hour,
        int(f.last_day.pages), f.last_day.pages_per_hour, f.last_day.mb_per_s, bytes(f.last_day.bytes), f.holds_last_day, f.failures_last_day,
    );
    if !f.holds_counted {
        o.push_str("<p class=\"note\">LoC holds are counted from the status file as it is read, so holds shorter than its 30-second rewrite can be missed.</p>");
    }
    if !f.sets.is_empty() {
        o.push_str(
            "<h3>By set</h3><table><tr><th>Set</th><th>Fetched</th><th class=\"r\">Data</th></tr>",
        );
        for s in &f.sets {
            let _ = write!(
                o,
                "<tr><td>{}</td><td>{}</td><td class=\"r\">{}</td></tr>",
                s.name,
                meter(
                    s.fetched,
                    s.total,
                    &format!("{} of {}", int(s.fetched), int(s.total))
                ),
                bytes(s.bytes)
            );
        }
        o.push_str("</table><h3>Keep set by group, in fetch order</h3><table><tr><th>Group</th><th>Fetched</th><th class=\"r\">Data</th><th class=\"r\">Done in</th></tr>");
        for g in &f.groups {
            let _ =
                write!(
                o,
                "<tr><td>{}{}</td><td>{}</td><td class=\"r\">{}</td><td class=\"r\">{}</td></tr>",
                esc(g.name),
                if g.current { " <span class=\"badge\">now</span>" } else { "" },
                meter(g.fetched, g.total, &format!("{} of {}", int(g.fetched), int(g.total))),
                bytes(g.bytes),
                match (g.eta_secs, g.total > 0 && g.fetched >= g.total) {
                    (_, true) => "done".to_owned(),
                    (Some(e), _) => span(e),
                    (None, _) => "-".to_owned(),
                }
            );
        }
        let _ = write!(
            o,
            "</table><p class=\"note\">Done in: at the last hour's rate (the last day's if the last hour had none). Keep set done in {}.</p>",
            f.keep_eta_secs.map_or_else(|| "-".to_owned(), span)
        );
    }
    let _ = write!(
        o,
        "<h3>Failures</h3><p>{} pages LoC can't serve (permanent failures).</p>",
        int(f.dead_pages as u64)
    );
    if !f.recent_failures.is_empty() {
        o.push_str("<table><tr><th>When</th><th>doc_id</th><th>Kind</th><th>Error</th></tr>");
        for e in &f.recent_failures {
            let _ = write!(
                o,
                "<tr><td>{}</td><td><code>{}</code></td><td>{}</td><td class=\"note\">{}</td></tr>",
                time(e.at),
                esc(&e.doc_id),
                esc(&e.kind),
                esc(&e.error)
            );
        }
        o.push_str("</table>");
    }
    o.push_str("</section>");

    // 3. Nodes.
    o.push_str("<section><h2>Nodes</h2>");
    if v.nodes.is_empty() {
        o.push_str("<p class=\"muted\">No nodes configured.</p>");
    }
    for n in &v.nodes {
        let state = if n.ok {
            badge("good", "reporting")
        } else {
            badge("critical", "no status")
        };
        let _ = write!(o, "<h3>{} {state}</h3>", esc(&n.name));
        if let Some(e) = &n.error {
            let _ = write!(
                o,
                "<p class=\"err\">{}: {}{}</p>",
                esc(&n.url),
                esc(e),
                n.last_ok
                    .map(|t| format!("; last answered {}", ago(now, t)))
                    .unwrap_or_default()
            );
        }
        if n.last_ok.is_none() {
            continue;
        }
        let routes = if n.routes.is_empty() {
            "none".to_owned()
        } else {
            n.routes
                .iter()
                .map(|(l, e)| format!("{} → {}", esc(l), esc(e)))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let cache = |o: &usnm_ocr_contract::status::Outcomes| {
            let all = o.from_cache + o.from_loc;
            if all == 0 {
                "-".to_owned()
            } else {
                format!(
                    "{} cache / {} LoC ({:.0}% cache)",
                    o.from_cache,
                    o.from_loc,
                    pct(o.from_cache, all)
                )
            }
        };
        let _ = write!(
            o,
            "<table class=\"kv\"><tr><td>Worker</td><td>{} (<code>{}</code>)</td></tr>\
             <tr><td>Engine</td><td>{} {}; models {}</td></tr>\
             <tr><td>Routes</td><td>{routes}</td></tr>\
             <tr><td>Concurrency</td><td>{}; {} in flight</td></tr>\
             <tr><td>Scans</td><td>{}</td></tr>{}</table>",
            esc(&n.version),
            esc(&n.node_id),
            esc(&n.engine),
            esc(&n.engine_version),
            esc(&n.models.join(", ")),
            n.concurrency,
            n.in_flight.len(),
            n.scan_cache_url.as_deref().map_or_else(
                || "from LoC only".to_owned(),
                |u| format!("scan server {} first", esc(u))
            ),
            n.loc_hold_secs
                .map(|h| format!(
                    "<tr><td>LoC</td><td>{}</td></tr>",
                    badge("warning", &format!("hold, {} left", span(h)))
                ))
                .unwrap_or_default()
        );
        let _ = write!(
            o,
            "<table><tr><th></th><th class=\"r\">Done</th><th class=\"r\">Failed</th><th class=\"r\">Abandoned</th><th class=\"r\">Dead-lettered</th><th>Scans</th></tr>\
             <tr><td>Last hour</td><td class=\"r\">{}</td><td class=\"r\">{}</td><td class=\"r\">{}</td><td class=\"r\">{}</td><td>{}</td></tr>\
             <tr><td>Last day</td><td class=\"r\">{}</td><td class=\"r\">{}</td><td class=\"r\">{}</td><td class=\"r\">{}</td><td>{}</td></tr></table>\
             <p>{:.2} pages a minute over the last hour.</p>",
            n.last_hour.done, n.last_hour.failed, n.last_hour.abandoned, n.last_hour.dead_lettered, cache(&n.last_hour),
            n.last_day.done, n.last_day.failed, n.last_day.abandoned, n.last_day.dead_lettered, cache(&n.last_day),
            n.pages_per_min
        );
        if !n.in_flight.is_empty() {
            o.push_str("<table><tr><th>In flight</th><th>Lang</th><th>Queue</th><th>Stage</th><th>Started</th></tr>");
            for p in &n.in_flight {
                let _ = write!(
                    o,
                    "<tr><td><code>{}</code></td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                    esc(&p.doc_id),
                    esc(&p.language),
                    esc(&p.queue),
                    esc(&p.stage),
                    ago(now, p.started_at)
                );
            }
            o.push_str("</table>");
        }
    }
    o.push_str("</section>");

    // 4. What's next.
    let x = &v.next;
    o.push_str("<section><h2>What's next</h2>");
    let _ = write!(
        o,
        "<h3>To OCR: keep set in fetch order, not yet OCRed ({} left)</h3>",
        int(x.keep_left_to_ocr)
    );
    next_table(&mut o, &x.to_ocr, true);
    o.push_str("<p class=\"note\">The queues decide what a node takes; this list is the keep set in the fetcher's order. \
                A page counts as OCRed once a node's journal says so.</p>");
    o.push_str("<h3>To fetch</h3>");
    next_table(&mut o, &x.to_fetch, false);
    o.push_str("<h3>Seed plan</h3>");
    match &x.seed_plan {
        Some(p) => {
            o.push_str("<table class=\"kv\">");
            for (k, val) in p {
                let _ = write!(o, "<tr><td>{}</td><td>{}</td></tr>", esc(k), esc(val));
            }
            o.push_str("</table>");
        }
        None => o.push_str("<p class=\"note\">No seed plan file. The cloud seeder's settings (USNM_OCR_SEED_*) are on the ocr-sync job in Azure.</p>"),
    }
    o.push_str("</section>");

    // 5. What's been OCRed.
    let d = &v.ocred;
    o.push_str("<section><h2>What's been OCRed</h2>");
    let _ = write!(
        o,
        "<p>{} pages OCRed by the nodes, as their journals report.</p>",
        int(d.pages)
    );
    let pairs = |o: &mut String, title: &str, rows: &[(String, u64)]| {
        if rows.is_empty() {
            return;
        }
        let _ = write!(o, "<h3>{title}</h3><table>");
        for (k, n) in rows {
            let _ = write!(
                o,
                "<tr><td>{}</td><td class=\"r\">{}</td></tr>",
                if k.is_empty() { "-".to_owned() } else { esc(k) },
                int(*n)
            );
        }
        o.push_str("</table>");
    };
    pairs(&mut o, "By language", &d.by_language);
    pairs(&mut o, "By engine", &d.by_engine);
    for l in &d.latest {
        let _ = write!(o, "<h3>Latest on {}</h3>", esc(&l.node));
        if l.rows.is_empty() {
            o.push_str("<p class=\"muted\">None yet.</p>");
            continue;
        }
        o.push_str("<table><tr><th>doc_id</th><th>Lang</th><th>Engine</th><th>Outcome</th><th>Uploaded</th><th>Scan</th><th class=\"r\">Secs</th><th>Finished</th></tr>");
        for c in &l.rows {
            let _ = write!(
                o,
                "<tr><td><code>{}</code></td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td class=\"r\">{:.0}</td><td>{}</td></tr>",
                esc(&c.doc_id),
                esc(&c.language),
                esc(&c.engine),
                match c.outcome.as_str() {
                    "done" => badge("good", "done"),
                    other => badge(
                        "warning",
                        &match &c.error_kind {
                            Some(k) => format!("{other} ({k})"),
                            None => other.to_owned(),
                        }
                    ),
                },
                if c.uploaded { "yes" } else { "no" },
                esc(&c.scan),
                c.secs,
                time(c.finished_at)
            );
        }
        o.push_str("</table>");
    }
    let _ = write!(
        o,
        "<h3>Imports</h3><p class=\"note\">{}",
        esc(v.azure.imports)
    );
    if let Some(u) = &d.workbook_url {
        let _ = write!(o, " See the <a href=\"{}\">OCR nodes workbook</a>.", esc(u));
    }
    o.push_str("</p></section>");

    // 6. Queues.
    o.push_str("<section><h2>Azure queues and dead letters</h2>");
    match (&v.azure.queues, &v.azure.queues_from) {
        (Some(q), Some(from)) => {
            let _ = write!(
                o,
                "<p>As {} saw them {}{}.</p>",
                esc(from),
                ago(now, q.checked_at),
                if v.azure.queues_stale {
                    format!(" {}", badge("warning", "stale"))
                } else {
                    String::new()
                }
            );
            if let Some(e) = &q.error {
                let _ = write!(o, "<p class=\"err\">{}</p>", esc(e));
            }
            o.push_str("<table><tr><th>Queue</th><th class=\"r\">Messages</th></tr>");
            for (name, n) in &q.counts {
                let _ = write!(
                    o,
                    "<tr><td><code>{}</code></td><td class=\"r\">{}</td></tr>",
                    esc(name),
                    int(*n)
                );
            }
            o.push_str("</table>");
            if q.dead_letters.is_empty() {
                o.push_str("<p class=\"muted\">No dead letters visible.</p>");
            } else {
                o.push_str("<h3>Dead letters, oldest first</h3><table><tr><th>doc_id</th><th>From</th><th>Cause</th><th class=\"r\">Received</th><th>Node</th><th>When</th><th>Error</th></tr>");
                for d in &q.dead_letters {
                    let cause = match &d.kind {
                        Some(k) => format!("{} ({k})", d.cause),
                        None => d.cause.clone(),
                    };
                    let _ = write!(
                        o,
                        "<tr><td><code>{}</code></td><td>{}</td><td>{}</td><td class=\"r\">{}</td><td>{}</td><td>{}</td><td class=\"note\">{}</td></tr>",
                        if d.doc_id.is_empty() { "-".to_owned() } else { esc(&d.doc_id) },
                        esc(&d.from),
                        esc(&cause),
                        d.dequeue_count,
                        esc(&d.node),
                        time_str(&d.at),
                        esc(&d.error)
                    );
                }
                o.push_str("</table>");
            }
        }
        _ => o.push_str("<p class=\"muted\">No node has reported the queues yet.</p>"),
    }
    let _ = write!(
        o,
        "<p class=\"note\">{}</p></section>",
        esc(v.azure.queues_note)
    );

    let _ = write!(o, "</div><script>{SCRIPT}</script></body></html>");
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(
            esc("<a href=\"x\">'&'</a>"),
            "&lt;a href=&quot;x&quot;&gt;&#39;&amp;&#39;&lt;/a&gt;"
        );
        assert_eq!(bytes(999), "999 B");
        assert_eq!(bytes(1_215_076_923), "1.2 GB");
        assert_eq!(bytes(3_298_534_883_328), "3.3 TB");
        assert_eq!(span(691_999), "8d 0h");
        assert_eq!(span(3_700), "1h 1m");
        assert_eq!(span(42), "42s");
        assert_eq!(int(224_848), "224,848");
        assert_eq!(int(5), "5");
        assert_eq!(pct(1, 0), 0.0);
    }
}
