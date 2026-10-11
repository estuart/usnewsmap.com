//! Where a searcher's CPU went, from the flame graph Quickwit's own
//! profiler draws (`GET /api/developer/pprof/start`, then `/flamegraph`;
//! the `pprof` feature of its release builds, #251).
//!
//! The SVG is inferno's: one `<g>` per frame, its `<title>` the function and
//! its samples (`name (123 samples, 4.56%)`), its `<rect>` with `y` (one row
//! per stack depth, the root `all` lowest on the page, so largest) and
//! `fg:x`/`fg:w` (the frame's first sample and its sample count). Quickwit
//! puts each sample's thread name at the bottom of its stack, with trailing
//! numbers removed, so the row above `all` holds the threads:
//! `main_runtime_thread` (downloads, TLS, split opening), `quickwit-search`
//! (the search pool) and others. A frame's self samples are its own less
//! those of the frames on the row above it within its span.

use std::collections::BTreeMap;

use serde::Serialize;

/// One frame of the graph.
#[derive(Debug, Clone, PartialEq)]
struct Frame {
    name: String,
    y: f64,
    x: u64,
    w: u64,
}

/// Samples by thread, and the functions that used the most samples
/// themselves (not in what they called) in the main runtime and the search
/// pool.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Summary {
    pub total_samples: u64,
    pub threads: BTreeMap<String, u64>,
    pub main_runtime_top: Vec<(String, u64)>,
    pub search_pool_top: Vec<(String, u64)>,
}

/// The main runtime's thread name (`main_runtime_thread`,
/// `quickwit-cli/src/main.rs`) as the graph has it: Linux keeps 15
/// characters of a thread's name, so `main_runtime_th`. Matched as a prefix.
pub const MAIN_RUNTIME: &str = "main_runtime_th";
/// The search pool's (`quickwit-search-{n}`, `quickwit-common/src/thread_pool`;
/// the profiler drops the number).
pub const SEARCH_POOL: &str = "quickwit-search";

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let key = format!(" {name}=\"");
    let start = tag.find(&key)? + key.len();
    let end = tag[start..].find('"')? + start;
    Some(&tag[start..end])
}

fn frames(svg: &str) -> Vec<Frame> {
    let mut out = Vec::new();
    let mut rest = svg;
    while let Some(i) = rest.find("<title>") {
        rest = &rest[i + "<title>".len()..];
        let Some(j) = rest.find("</title>") else {
            break;
        };
        let title = unescape(&rest[..j]);
        rest = &rest[j..];
        let Some(k) = rest.find("<rect") else { break };
        let tag_end = rest[k..].find('>').map_or(rest.len(), |e| k + e);
        let tag = &rest[k..tag_end];
        // `name (123 samples, 4.56%)`; a name can hold parentheses itself.
        let name = title
            .rfind(" (")
            .map_or(title.as_str(), |p| &title[..p])
            .to_owned();
        let parsed = (
            attr(tag, "y").and_then(|v| v.parse::<f64>().ok()),
            attr(tag, "fg:x").and_then(|v| v.parse::<u64>().ok()),
            attr(tag, "fg:w").and_then(|v| v.parse::<u64>().ok()),
        );
        if let (Some(y), Some(x), Some(w)) = parsed {
            out.push(Frame { name, y, x, w });
        }
        rest = &rest[tag_end..];
    }
    out
}

/// The top `n` functions by self samples within `thread`'s span.
fn top_self(frames: &[Frame], rows: &[f64], thread: &Frame, n: usize) -> Vec<(String, u64)> {
    let row_of = |y: f64| rows.iter().position(|r| (r - y).abs() < 0.5);
    let in_span = |f: &Frame| f.x >= thread.x && f.x + f.w <= thread.x + thread.w;
    let mut by_name: BTreeMap<String, u64> = BTreeMap::new();
    for f in frames.iter().filter(|f| in_span(f)) {
        let Some(r) = row_of(f.y) else { continue };
        if r < row_of(thread.y).unwrap_or(0) {
            continue;
        }
        let children: u64 = frames
            .iter()
            .filter(|c| row_of(c.y) == Some(r + 1) && c.x >= f.x && c.x + c.w <= f.x + f.w)
            .map(|c| c.w)
            .sum();
        let own = f.w.saturating_sub(children);
        if own > 0 {
            *by_name.entry(f.name.clone()).or_default() += own;
        }
    }
    let mut top: Vec<(String, u64)> = by_name.into_iter().collect();
    top.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    top.truncate(n);
    top
}

/// The graph's summary; `None` when the SVG has no frames we can read.
pub fn summarize(svg: &str, n: usize) -> Option<Summary> {
    let frames = frames(svg);
    // Rows from the root up: largest `y` first.
    let mut rows: Vec<f64> = frames.iter().map(|f| f.y).collect();
    rows.sort_by(|a, b| b.total_cmp(a));
    rows.dedup_by(|a, b| (*a - *b).abs() < 0.5);
    let root_row = *rows.first()?;
    let total_samples = frames
        .iter()
        .filter(|f| (f.y - root_row).abs() < 0.5)
        .map(|f| f.w)
        .sum();
    let thread_row = *rows.get(1)?;
    let threads_frames: Vec<&Frame> = frames
        .iter()
        .filter(|f| (f.y - thread_row).abs() < 0.5)
        .collect();
    let mut threads: BTreeMap<String, u64> = BTreeMap::new();
    for f in &threads_frames {
        *threads.entry(f.name.clone()).or_default() += f.w;
    }
    let top_of = |name: &str| {
        let mut all: BTreeMap<String, u64> = BTreeMap::new();
        for t in threads_frames.iter().filter(|f| f.name.starts_with(name)) {
            for (k, v) in top_self(&frames, &rows, t, n * 4) {
                *all.entry(k).or_default() += v;
            }
        }
        let mut v: Vec<(String, u64)> = all.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v.truncate(n);
        v
    };
    Some(Summary {
        total_samples,
        main_runtime_top: top_of(MAIN_RUNTIME),
        search_pool_top: top_of(SEARCH_POOL),
        threads,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(name: &str, y: u32, x: u64, w: u64) -> String {
        format!(
            r#"<g><title>{name} ({w} samples, 1.00%)</title><rect x="0%" y="{y}" width="1%" height="15" fill="rgb(1,2,3)" fg:x="{x}" fg:w="{w}"/><text x="1" y="{}">x</text></g>"#,
            y + 10
        )
    }

    #[test]
    fn splits_samples_by_thread_and_function() {
        // all (100) > main_runtime_thread (60) > tokio (60) > rustls (25), hyper (20)
        //           > quickwit-search (40) > tantivy::search (30) > memcpy (30)
        let svg = [
            "<svg>".to_owned(),
            g("all", 100, 0, 100),
            g("main_runtime_th", 84, 0, 60),
            g("tokio::runtime::task", 68, 0, 60),
            g("rustls::conn::read&lt;T&gt;", 52, 0, 25),
            g("hyper::proto (h1)", 52, 25, 20),
            g("quickwit-search", 84, 60, 40),
            g("tantivy::search", 68, 60, 30),
            g("memcpy", 52, 60, 30),
            "</svg>".to_owned(),
        ]
        .concat();
        let s = summarize(&svg, 3).unwrap();
        assert_eq!(s.total_samples, 100);
        assert_eq!(s.threads["main_runtime_th"], 60);
        assert_eq!(s.threads["quickwit-search"], 40);
        assert_eq!(
            s.main_runtime_top,
            [
                ("rustls::conn::read<T>".to_owned(), 25),
                ("hyper::proto (h1)".to_owned(), 20),
                ("tokio::runtime::task".to_owned(), 15),
            ]
        );
        // The pool's own 10 samples sit in the thread frame.
        assert_eq!(
            s.search_pool_top,
            [
                ("memcpy".to_owned(), 30),
                ("quickwit-search".to_owned(), 10)
            ]
        );
        assert_eq!(summarize("<svg></svg>", 3), None);
    }
}
