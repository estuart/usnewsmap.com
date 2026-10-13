//! The pipeline diagrams (#296): one SVG per data set, drawn by hand in
//! `src/diagrams/*.svg` and embedded here. They are static; only the state
//! of each step (the bar on a box, its state word and a live line in a few
//! boxes) comes from the [`View`], through the `@@cls:<step>@@`,
//! `@@lbl:<step>@@` and `@@txt:<step>@@` markers in the files.
//!
//! The SVG files are repo content and trusted. Every live value that goes
//! into one passes through [`esc`].

use std::collections::BTreeMap;

use crate::html::{esc, int};
use crate::view::View;

/// One data set's diagram, with the four questions it answers.
pub struct Diagram {
    pub id: &'static str,
    pub heading: &'static str,
    pub svg: &'static str,
    /// Source, needs OCR, where it is stored, what makes it searchable.
    pub answers: [(&'static str, &'static str); 4],
}

pub const LEGEND: &str = include_str!("diagrams/legend.svg");

pub const DIAGRAMS: [Diagram; 4] = [
    Diagram {
        id: "loc-kept",
        heading: "LoC pages whose OCR text is kept",
        svg: include_str!("diagrams/loc-kept.svg"),
        answers: [
            (
                "Comes from",
                "Chronicling America batches on loc.gov, read once by the ingest job in Azure, paced.",
            ),
            (
                "Needs OCR",
                "No: LoC's own text is searched. Pages the damage pass scores above 0.25 are the exception \
                 and follow the next two diagrams.",
            ),
            (
                "Stored",
                "Azure only: curated/ (Parquet), then the search index in qw-index and reference/. \
                 Nothing on the home network.",
            ),
            (
                "Searchable when",
                "Once a release has published the batch. No further step.",
            ),
        ],
    },
    Diagram {
        id: "loc-keep",
        heading: "LoC keep set, read again on the nodes",
        svg: include_str!("diagrams/loc-keep.svg"),
        answers: [
            (
                "Comes from",
                "LoC's tile.loc.gov storage service: the batch's JP2 for each page. Only a page the batch \
                 route cannot place goes through the loc.gov item API and the IIIF JPEG. The scan fetcher \
                 fetches each scan once, paced, and holds when LoC asks it to. The keep set is every page \
                 not detected as English, plus the worst English batches.",
            ),
            (
                "Needs OCR",
                "Yes. Queue ocr-bulk (ocr-priority is for pages users find, not built). OCR nodes 3 and 4 \
                 read them with Tesseract in a container per page, using the scan from the scan server.",
            ),
            (
                "Stored",
                "Home: manifests/ and keep/ on the scan store (scans are kept for good); the nodes keep \
                 scratch space for the page in hand. Azure: ocr-out (deleted after 30 days), then \
                 curated/ocr-local/.",
            ),
            (
                "Searchable when",
                "ocr-sync has copied the output to curated/ocr-local/, and the full rebuild (#283) runs \
                 with USNM_OCR_LOCAL set. Until then LoC's text stays searchable.",
            ),
        ],
    },
    Diagram {
        id: "loc-buffer",
        heading: "LoC buffer set, English pages read again",
        svg: include_str!("diagrams/loc-buffer.svg"),
        answers: [
            (
                "Comes from",
                "The same route as the keep set. These are the other badly damaged English pages, fetched \
                 after the keep set.",
            ),
            (
                "Needs OCR",
                "Yes, but only the pages the seed plan queues (see What's next). Same queue, nodes and \
                 engine as the keep set.",
            ),
            (
                "Stored",
                "Home: buffer/ on the scan store, a rolling buffer. The fetcher waits while it is at its \
                 cap, and the worker deletes a scan once that page's output is uploaded. Azure as for the \
                 keep set.",
            ),
            (
                "Searchable when",
                "As for the keep set: ocr-sync, then the full rebuild (#283).",
            ),
        ],
    },
    Diagram {
        id: "serial-set",
        heading: "Serial Set from GovInfo",
        svg: include_str!("diagrams/serial-set.svg"),
        answers: [
            (
                "Comes from",
                "GovInfo's sitemaps, MODS and PDFs, public and without a key. usnm-sset runs on OCR node 3, \
                 one request at a time, paced. Run again to pick up new or changed volumes.",
            ),
            (
                "Needs OCR",
                "No. pdftotext reads the text layer of each PDF. The OCR queues and engines are not used.",
            ),
            (
                "Stored",
                "Node 3: a raw store of MODS and page texts (the PDFs are deleted after reading). Azure: \
                 curated/congress/serialset/.",
            ),
            (
                "Searchable when",
                "A release run with --congress (USNM_CONGRESS, off by default) builds the Congress index. \
                 It does not wait for the #283 rebuild.",
            ),
        ],
    },
];

/// A step's state: the CSS token for its bar and the word shown.
#[derive(Debug, Clone, PartialEq)]
pub struct StepState {
    pub class: &'static str,
    pub label: &'static str,
    /// A live line some boxes show; empty for the rest.
    pub text: String,
}

impl StepState {
    fn new(class: &'static str) -> StepState {
        let label = match class {
            "done" => "done",
            "running" => "running",
            "waiting" => "waiting",
            "not-built" => "not built",
            "failed" => "failed",
            _ => "not tracked here",
        };
        StepState {
            class,
            label,
            text: String::new(),
        }
    }
    fn with(class: &'static str, text: String) -> StepState {
        StepState {
            text,
            ..StepState::new(class)
        }
    }
}

/// The state of every step the dashboard can say something about. A step
/// not in the map is drawn as "not tracked here": the dashboard is on the
/// LAN and reads nothing in Azure (12 §12.2.5).
pub fn steps(v: &View) -> BTreeMap<&'static str, StepState> {
    let mut m = BTreeMap::new();

    // Nothing in Azure is read here, so the ingest, the release, the
    // published index, the rebuild and the index with our text are left
    // out and draw as "not tracked here".
    // Step 12: the API queuing pages users find.
    m.insert("users", StepState::new("not-built"));

    // The manifest is on the scan store, so the damage pass that fed it
    // and the copy that put it there are done.
    let plan = v.fetch.plan_error.is_none() && !v.fetch.sets.is_empty();
    let manifest = StepState::new(if plan { "done" } else { "waiting" });
    m.insert("manifest", manifest.clone());
    m.insert("pull", manifest);
    if plan {
        m.insert("damage", StepState::new("done"));
    }

    // The fetcher takes the keep set first, then the buffer set, so the
    // run's state belongs to the first set that isn't complete; a set
    // behind it is waiting. A finished set is done whatever the run says.
    let run = v.fetch.status.as_ref().map(|s| s.state.as_str());
    let mut active_taken = false;
    for (set, fetch_key, store_key) in [
        ("keep", "fetch-keep", "store-keep"),
        ("buffer", "fetch-buffer", "store-buffer"),
    ] {
        let Some(s) = v.fetch.sets.iter().find(|s| s.name == set) else {
            continue;
        };
        let complete = s.fetched >= s.total;
        let (fetch_class, store_class) = if complete {
            ("done", "done")
        } else if active_taken {
            ("waiting", "waiting")
        } else {
            active_taken = true;
            match run {
                Some("running") => ("running", "running"),
                Some("failed") => ("failed", "waiting"),
                Some("done" | "waiting" | "cooling" | "budget" | "stopped") => {
                    ("waiting", "waiting")
                }
                _ => ("none", "waiting"),
            }
        };
        m.insert(
            fetch_key,
            StepState::with(
                fetch_class,
                format!("{} of {} pages", int(s.fetched), int(s.total)),
            ),
        );
        m.insert(store_key, StepState::new(store_class));
    }

    // Queues, from the nodes' look.
    if let (Some(q), false) = (&v.azure.queues, v.azure.queues_stale) {
        let n = |name: &str| q.counts.get(name).copied().unwrap_or(0);
        let (bulk, prio) = (n("ocr-bulk"), n("ocr-priority"));
        m.insert(
            "queues",
            StepState::with(
                if bulk + prio > 0 {
                    "running"
                } else {
                    "waiting"
                },
                format!("ocr-bulk {}, ocr-priority {}", int(bulk), int(prio)),
            ),
        );
    }

    // The nodes, and what they have uploaded.
    let working = v.nodes.iter().any(|n| n.state == "working");
    let up = v.nodes.iter().any(|n| n.ok);
    if up {
        m.insert(
            "nodes",
            StepState::with(
                if working { "running" } else { "waiting" },
                format!("{} pages OCRed so far", int(v.ocred.pages)),
            ),
        );
        m.insert(
            "out",
            StepState::new(if working {
                "running"
            } else if v.ocred.pages > 0 {
                "done"
            } else {
                "waiting"
            }),
        );
    }

    // The Serial Set, from the status its node pushes.
    if let Some(x) = v.serial_set.iter().find(|x| !x.stale) {
        let st = &x.status;
        let class = match x.state.as_str() {
            "finished" => "done",
            "running" => "running",
            _ => "waiting",
        };
        m.insert(
            "sset-fetch",
            StepState::with(
                class,
                format!("{} of {} volumes", int(x.done), int(st.to_fetch)),
            ),
        );
        m.insert(
            "sset-curated",
            StepState::with(
                if st.curated == 0 { "waiting" } else { class },
                format!("{} volumes, {} pages", int(st.curated), int(st.pages)),
            ),
        );
    }
    m
}

/// The boxes of a diagram with a live state, as plain text for screen
/// readers: each box's title and what it says.
pub fn summary(svg: &str, steps: &BTreeMap<&'static str, StepState>) -> String {
    let mut out = String::new();
    let mut rest = svg;
    while let Some(i) = rest.find("data-state=\"@@cls:") {
        let after = &rest[i + "data-state=\"@@cls:".len()..];
        let Some(j) = after.find("@@") else { break };
        let key = &after[..j];
        rest = &after[j..];
        let title = rest
            .find("class=\"ttl\"")
            .and_then(|t| rest[t..].split_once('>'))
            .and_then(|(_, r)| r.split_once("</text>"))
            .map(|(t, _)| t.trim_start_matches(|c: char| c.is_ascii_digit() || c == ' '));
        let Some(title) = title else { continue };
        let (label, text) = steps
            .get(key)
            .map_or(("not tracked here", ""), |s| (s.label, s.text.as_str()));
        out.push_str(&format!("<li>{}: {}", title, esc(label)));
        if !text.is_empty() {
            out.push_str(&format!(" ({})", esc(text)));
        }
        out.push_str("</li>");
    }
    out
}

/// A diagram with its markers filled in from `steps`. A marker for a step
/// not in the map reads "not tracked here"; none is left in the output.
pub fn fill(svg: &str, steps: &BTreeMap<&'static str, StepState>) -> String {
    let mut out = String::with_capacity(svg.len() + 512);
    let mut rest = svg;
    while let Some(i) = rest.find("@@") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        let Some(j) = after.find("@@") else {
            out.push_str("@@");
            rest = after;
            continue;
        };
        let (kind, step) = after[..j].split_once(':').unwrap_or(("", ""));
        let state = steps.get(step);
        match kind {
            "cls" => out.push_str(state.map_or("none", |s| s.class)),
            "lbl" => out.push_str(&esc(state.map_or(StepState::new("none").label, |s| s.label))),
            "txt" => out.push_str(&esc(state.map_or("", |s| s.text.as_str()))),
            _ => {}
        }
        rest = &after[j + 2..];
    }
    out.push_str(rest);
    out
}

/// The "Pipelines" section.
pub fn section(v: &View) -> String {
    let st = steps(v);
    let mut o = String::with_capacity(48 * 1024);
    o.push_str(
        "<section class=\"wide\" id=\"pipelines\"><h2>Pipelines</h2>\
         <p class=\"note\">One diagram for each data set: where it comes from, whether it needs OCR, where \
         each copy is stored and what still has to happen before it is searchable. The drawings are \
         fixed; the bar on each box and the italic line under it are the live state from this page's \
         own data. Steps in Azure that nothing on the LAN can read show as not tracked here.</p>",
    );
    o.push_str(LEGEND);
    for d in &DIAGRAMS {
        let _ = std::fmt::Write::write_fmt(
            &mut o,
            format_args!(
                "<h3 id=\"pipeline-{}\">{}</h3><figure style=\"margin:0\">{}</figure>\
                 <ul class=\"sr\" aria-label=\"Live state of each step\">{}</ul><table class=\"kv\">",
                d.id,
                esc(d.heading),
                fill(d.svg, &st),
                summary(d.svg, &st)
            ),
        );
        for (k, val) in d.answers {
            let _ = std::fmt::Write::write_fmt(
                &mut o,
                format_args!("<tr><td>{}</td><td>{}</td></tr>", esc(k), esc(val)),
            );
        }
        o.push_str("</table>");
    }
    o.push_str("</section>");
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::Fetch;
    use crate::hosts::Hosts;
    use crate::nodes::Nodes;
    use crate::sset::{SsetStatus, Ssets};
    use crate::view::{build, SetView, Settings, SsetView};
    use chrono::Utc;
    use quick_xml::events::Event;
    use quick_xml::Reader;

    fn empty_view() -> View {
        let dir = tempfile::tempdir().unwrap();
        let mut fetch = Fetch::new(dir.path().to_owned(), None);
        let nodes = Nodes::open(dir.path().join("state"), &[]).unwrap();
        build(
            &mut fetch,
            &nodes,
            &Hosts::new(Vec::new()),
            &Ssets::default(),
            None,
            &Settings::default(),
            Utc::now(),
        )
    }

    fn all_svgs() -> Vec<(&'static str, &'static str)> {
        let mut v = vec![("legend", LEGEND)];
        v.extend(DIAGRAMS.iter().map(|d| (d.id, d.svg)));
        v
    }

    /// Parses to the end, with matching tags; returns the element names.
    fn well_formed(xml: &str) -> Vec<String> {
        let mut r = Reader::from_str(xml);
        let mut names = Vec::new();
        loop {
            match r.read_event() {
                Ok(Event::Eof) => break,
                Ok(Event::Start(e) | Event::Empty(e)) => {
                    names.push(e.name().as_ref().to_owned());
                }
                Ok(_) => {}
                Err(e) => panic!("not well-formed: {e}"),
            }
        }
        names
    }

    #[test]
    fn each_svg_is_well_formed_and_described() {
        for (name, svg) in all_svgs() {
            let names = well_formed(svg);
            assert_eq!(names.first().map(String::as_str), Some("svg"), "{name}");
            assert!(svg.contains("viewBox="), "{name} has no viewBox");
            assert!(svg.contains("<title id="), "{name} has no title");
            assert!(svg.contains("<desc id="), "{name} has no description");
            assert!(svg.contains("role=\"img\""), "{name}");
            // Plain SVG: no scripts, external files or hard-coded fonts.
            for bad in ["<script", "<image", "xlink:href", "http://fonts", "@import"] {
                assert!(!svg.contains(bad), "{name} has {bad}");
            }
        }
    }

    #[test]
    fn filled_diagrams_are_well_formed_with_no_marker_left() {
        let st = steps(&empty_view());
        for (name, svg) in all_svgs() {
            let out = fill(svg, &st);
            assert!(!out.contains("@@"), "{name} keeps a marker");
            well_formed(&out);
        }
        // With nothing read, the steps the dashboard can't see say so.
        let out = fill(DIAGRAMS[1].svg, &st);
        assert!(out.contains("not tracked here"));
        assert!(out.contains("data-state=\"not-built\""));
    }

    #[test]
    fn state_comes_from_the_view() {
        let mut v = empty_view();
        v.fetch.sets.push(SetView {
            name: "keep",
            total: 10,
            fetched: 10,
            bytes: 0,
        });
        v.fetch.sets.push(SetView {
            name: "buffer",
            total: 1_000,
            fetched: 5,
            bytes: 0,
        });
        v.serial_set.push(SsetView {
            name: "usnm-sset".into(),
            received: Utc::now(),
            age_secs: 0,
            stale: false,
            state: "running".into(),
            status: SsetStatus {
                to_fetch: 900,
                curated: 120,
                pages: 50_000,
                ..SsetStatus::default()
            },
            done: 120,
        });
        let st = steps(&v);
        assert_eq!(st["fetch-keep"].class, "done");
        assert_eq!(st["fetch-keep"].text, "10 of 10 pages");
        assert_eq!(st["fetch-buffer"].text, "5 of 1,000 pages");
        assert_eq!(st["sset-fetch"].class, "running");
        assert_eq!(st["sset-fetch"].text, "120 of 900 volumes");
        assert_eq!(st["sset-curated"].text, "120 volumes, 50,000 pages");
        let out = fill(DIAGRAMS[3].svg, &st);
        assert!(out.contains("data-state=\"running\""));
        assert!(out.contains("120 of 900 volumes"));
    }

    #[test]
    fn live_text_is_escaped() {
        let mut st = BTreeMap::new();
        st.insert("fetch-keep", StepState::with("done", "<b>&".into()));
        let out = fill("<text>@@txt:fetch-keep@@</text>", &st);
        assert_eq!(out, "<text>&lt;b&gt;&amp;</text>");
    }

    #[test]
    fn the_page_has_every_diagram_once() {
        let html = crate::html::render(&empty_view());
        assert!(html.contains("<h2>Pipelines</h2>"));
        for d in &DIAGRAMS {
            assert_eq!(
                html.matches(&format!("<title id=\"{}-t\">", d.id)).count(),
                1
            );
            assert!(html.contains(&esc(d.heading)));
        }
        // Pipelines follow At a glance.
        assert!(html.find("At a glance") < html.find("Pipelines"));
        // Element ids are unique across the inlined SVGs.
        let mut ids: Vec<&str> = html
            .split(" id=\"")
            .skip(1)
            .filter_map(|s| s.split('"').next())
            .collect();
        let n = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), n, "duplicate element ids");
    }

    #[test]
    fn nothing_private_is_written() {
        let mut text: Vec<String> = all_svgs().iter().map(|(_, s)| s.to_string()).collect();
        for d in &DIAGRAMS {
            for (k, v) in d.answers {
                text.push(format!("{k} {v} {}", d.heading));
            }
        }
        for t in text {
            for word in t.split(|c: char| !(c.is_ascii_digit() || c == '.')) {
                let o: Vec<u8> = word
                    .split('.')
                    .map_while(|p| p.parse::<u8>().ok())
                    .collect();
                if word.split('.').count() == 4 && o.len() == 4 {
                    let private = o[0] == 10
                        || (o[0] == 172 && (16..32).contains(&o[1]))
                        || (o[0] == 192 && o[1] == 168);
                    assert!(!private, "private address {word}");
                }
            }
            for name in ["pve", "unifi", ".tjg.fyi"] {
                assert!(!t.to_lowercase().contains(name), "internal name {name}");
            }
        }
    }

    #[test]
    fn a_stale_status_and_an_unread_step_are_not_shown_as_live() {
        let mut v = empty_view();
        v.serial_set.push(SsetView {
            name: "usnm-sset".into(),
            received: Utc::now(),
            age_secs: 99_999,
            stale: true,
            state: "running".into(),
            status: SsetStatus::default(),
            done: 0,
        });
        let st = steps(&v);
        assert!(!st.contains_key("sset-fetch"));
        for k in ["rebuild", "index-ocr", "ingest", "release-loc", "index-loc"] {
            assert!(!st.contains_key(k), "{k} is not read from anywhere");
        }
    }

    #[test]
    fn an_empty_set_is_complete_and_the_run_state_goes_to_the_next() {
        let mut v = empty_view();
        for (name, total) in [("keep", 0), ("buffer", 50)] {
            v.fetch.sets.push(SetView {
                name,
                total,
                fetched: 0,
                bytes: 0,
            });
        }
        v.fetch.status = Some(crate::fetch::FetchStatus {
            state: "cooling".into(),
            ..Default::default()
        });
        let st = steps(&v);
        assert_eq!(st["fetch-keep"].class, "done");
        assert_eq!(st["fetch-buffer"].class, "waiting");
        v.fetch.status.as_mut().unwrap().state = "failed".into();
        assert_eq!(steps(&v)["fetch-buffer"].class, "failed");
    }

    #[test]
    fn screen_readers_get_the_live_state_as_text() {
        let st = steps(&empty_view());
        let s = summary(DIAGRAMS[3].svg, &st);
        assert!(s.contains("<li>"));
        assert!(s.contains("not tracked here"));
        let html = crate::html::render(&empty_view());
        assert_eq!(html.matches("class=\"sr\"").count(), DIAGRAMS.len());
    }
}
