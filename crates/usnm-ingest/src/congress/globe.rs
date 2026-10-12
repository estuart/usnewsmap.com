//! The Congressional Globe from UNT's page OCR (#229 design §7, phase 1):
//! `usnm-ingest congress-globe`.
//!
//! 1. **List.** OAI-PMH `ListRecords` on the `CGLOB` collection in UNT's
//!    `untl` format (112 items, all in one response today; resumption
//!    tokens are followed): each item's id, titles and dates.
//! 2. **De-duplicate sessions.** Two items hold the same pages:
//!    `metadc30905` and `metadc30907`, the 24th Congress's first session and
//!    its appendix in the 25 cm set, which `metadc29282` (the 30 cm set)
//!    holds in one item. They are left out ([`SUPERSEDED`]). The five other
//!    pairs of items with one sort title are different parts of a session
//!    catalogued under one title (their page extents differ), so they stay.
//! 3. **Fetch.** Each remaining item's IIIF manifest (its pages), then each
//!    page's OCR (`/ark:/67531/{id}/m1/{seq}/annotations/ocr/`), one request
//!    at a time at UNT's `Crawl-delay: 3` ([`Http`], shared with the Serial
//!    Set in [`super::fetch`]), backing off on 429 and
//!    5xx. With a raw store (`USNM_RAW_URL`, the archive), every response is
//!    kept there zstd-compressed under `congress/unt/`, and read back
//!    instead of UNT when it is there, so a stopped item resumes where it
//!    stopped.
//! 4. **Read.** Each page's lines, its day and chamber headings, whether
//!    the appendix has started, its printed page and the members it names
//!    ([`super::parse`]), dated as design §5 says ([`Dating`]).
//! 5. **Write.** One curated attempt per item (`congress/globe/{id}/…`,
//!    [`super::curated`]) once every page is read; an item stopped by the
//!    deadline writes nothing and is fetched again by the next run.
//!
//! An item with a finished attempt is skipped unless `--refetch`.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context};
use chrono::{DateTime, NaiveDate, Utc};
use quick_xml::events::Event;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use usnm_store::ObjectStore;

use super::curated::{self, CuratedPage};
use super::fetch::cached;
#[cfg(test)]
use super::fetch::{retry_wait, MAX_RETRY_WAIT};
pub use super::fetch::{Fetch, Http};
use super::parse::{self, Chamber};
use super::StateRef;

/// UNT Digital Library: the links on hits, and where the fetch goes by default.
pub const UNT: &str = "https://digital.library.unt.edu";

/// The Congressional Globe collection.
pub const SET: &str = "collection:CGLOB";

/// UNT's ARK prefix.
const ARK: &str = "ark:/67531";

/// UNT's robots.txt asks for 3 seconds between requests.
pub const CRAWL_DELAY: Duration = Duration::from_secs(3);

/// Who is asking, and why, on every request.
pub const USER_AGENT: &str = concat!(
    "usnm-ingest/",
    env!("CARGO_PKG_VERSION"),
    " (+https://usnewsmap.com; Congressional Globe text for search, #229)"
);

pub const SERIES: &str = "globe";
pub const SOURCE: &str = "unt";

/// Items whose pages another item holds (step 2), with the item that holds
/// them and why.
pub const SUPERSEDED: &[(&str, &str, &str)] = &[
    (
        "metadc30905",
        "metadc29282",
        "24th Congress, 1st session (xliv, 616 p.): the first part of metadc29282 \
         (xliv, 616, ix, 769 p.), which also holds the appendix",
    ),
    (
        "metadc30907",
        "metadc29282",
        "24th Congress, 1st session, appendix (ix, 786 p.): the second part of metadc29282",
    ),
];

/// The Cosmos lock that keeps two fetches from running at once.
/// An item id in the `ops` container, so no `/` (like `quickwit-writer`).
pub const LOCK: &str = "congress-globe";

/// One Globe item, from its OAI record.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Item {
    /// `metadc29274`.
    pub id: String,
    /// The OAI header says the record is deleted.
    pub deleted: bool,
    /// `title qualifier="officialtitle"`.
    pub title: String,
    /// `title qualifier="sorttitle"`: `Congressional Globe, Congress 25, Session 2`.
    pub sort_title: String,
    pub congress: Option<u16>,
    pub session: Option<String>,
    /// `Part 2` in the sort title.
    pub part: Option<u16>,
    /// The whole item is appendix (`Session 1x`, `App.`, or an official
    /// title that starts or ends with "Appendix").
    pub appendix: bool,
    /// `coverage qualifier="date"`: the session's first and last days.
    pub coverage: Option<(NaiveDate, NaiveDate)>,
    /// `description qualifier="physical"`: `512, 641, 6 p. ; 30 cm.`
    pub extent: String,
    pub datestamp: String,
}

impl Item {
    /// The item's place in the Globe, for the order of same-day hits:
    /// Congress, session, appendix, part.
    pub fn ordinal(&self) -> u32 {
        let session: u32 = self
            .session
            .as_deref()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        (u32::from(self.congress.unwrap_or(0)) << 12)
            | ((session & 0xF) << 8)
            | (u32::from(self.appendix) << 7)
            | u32::from(self.part.unwrap_or(0).min(127))
    }

    /// Read the sort title's Congress, session, part and appendix.
    fn read_sort_title(&mut self) {
        let t = self.sort_title.clone();
        let words: Vec<&str> = t
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|w| !w.is_empty())
            .collect();
        for (i, w) in words.iter().enumerate() {
            let next = words.get(i + 1).copied().unwrap_or_default();
            match *w {
                "Congress" => self.congress = next.parse().ok(),
                "Session" => {
                    let digits: String = next.chars().take_while(char::is_ascii_digit).collect();
                    if next.ends_with('x') {
                        self.appendix = true;
                    }
                    self.session = (!digits.is_empty()).then_some(digits);
                }
                "Part" => self.part = next.parse().ok(),
                "App." | "App" => self.appendix = true,
                _ => {}
            }
        }
        let official = self.title.trim().to_lowercase();
        if official.starts_with("appendix") || official.ends_with("appendix") {
            self.appendix = true;
        }
    }
}

/// The items of one `ListRecords` response, and its resumption token.
pub fn parse_records(xml: &[u8]) -> anyhow::Result<(Vec<Item>, Option<String>)> {
    let mut reader = quick_xml::Reader::from_reader(xml);
    let mut buf = Vec::new();
    let mut items = Vec::new();
    let mut item: Option<Item> = None;
    // The element whose text is being read, with its qualifier.
    let mut path: Vec<String> = Vec::new();
    let mut qualifier: Option<String> = None;
    let mut text = String::new();
    let mut token: Option<String> = None;
    let mut error: Option<String> = None;
    // A 200 that isn't a listing (a maintenance page, say) is no empty set.
    let mut listing = false;
    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Start(e) => {
                let name = e.local_name().as_ref().to_owned();
                qualifier = None;
                if name == "ListRecords" {
                    listing = true;
                }
                if name == "record" {
                    item = Some(Item::default());
                }
                for a in e.attributes().with_checks(true) {
                    let a = a?;
                    let value = || a.normalized_value(quick_xml::XmlVersion::Implicit1_0);
                    match a.key.local_name().as_ref() {
                        "qualifier" => qualifier = Some(value()?.into_owned()),
                        "status" if name == "header" && value()? == "deleted" => {
                            if let Some(i) = item.as_mut() {
                                i.deleted = true;
                            }
                        }
                        "code" if name == "error" => error = Some(value()?.into_owned()),
                        _ => {}
                    }
                }
                path.push(name);
                text.clear();
            }
            Event::Empty(e) => match e.local_name().as_ref() {
                "resumptionToken" => token = None,
                "ListRecords" => listing = true,
                // `<error code="badResumptionToken"/>`: an error all the same.
                "error" => {
                    let mut code = String::from("unknown");
                    for a in e.attributes().with_checks(true) {
                        let a = a?;
                        if a.key.local_name().as_ref() == "code" {
                            code = a
                                .normalized_value(quick_xml::XmlVersion::Implicit1_0)?
                                .into_owned();
                        }
                    }
                    error = Some(code);
                }
                _ => {}
            },
            Event::Text(t) => text.push_str(&t.into_inner()),
            Event::GeneralRef(r) => match &*r {
                "amp" => text.push('&'),
                "lt" => text.push('<'),
                "gt" => text.push('>'),
                "apos" => text.push('\''),
                "quot" => text.push('"'),
                _ => {
                    if let Some(c) = r.resolve_char_ref()? {
                        text.push(c);
                    }
                }
            },
            Event::End(e) => {
                let name = e.local_name().as_ref().to_owned();
                let value = text.trim().to_owned();
                if name == "resumptionToken" {
                    token = (!value.is_empty()).then(|| value.clone());
                }
                if let Some(i) = item.as_mut() {
                    match (name.as_str(), qualifier.as_deref()) {
                        ("title", Some("officialtitle")) if i.title.is_empty() => i.title = value,
                        ("title", Some("sorttitle")) => i.sort_title = value,
                        ("meta", Some("meta-id")) => i.id = value,
                        ("description", Some("physical")) => i.extent = value,
                        ("coverage", Some("date")) => {
                            i.coverage = value
                                .split_once('/')
                                .and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?)));
                        }
                        ("datestamp", _) => i.datestamp = value,
                        // `info:ark/67531/metadc29274`, until `meta-id` says.
                        ("identifier", None)
                            if i.id.is_empty() && path.iter().any(|p| p == "header") =>
                        {
                            i.id = value.rsplit('/').next().unwrap_or_default().to_owned();
                        }
                        _ => {}
                    }
                }
                if name == "record" {
                    if let Some(mut i) = item.take() {
                        i.read_sort_title();
                        items.push(i);
                    }
                }
                path.pop();
                qualifier = None;
                text.clear();
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    match error.as_deref() {
        // An empty set is an answer, not a failure.
        Some("noRecordsMatch") => Ok((items, token)),
        None if listing => Ok((items, token)),
        None => bail!("not an OAI-PMH `ListRecords` response"),
        Some(code) => bail!("OAI-PMH error `{code}`"),
    }
}

/// What the fetch does with the listed items (step 2).
#[derive(Debug, Default)]
pub struct Plan {
    pub fetch: Vec<Item>,
    /// (id, the item that holds its pages).
    pub superseded: Vec<(String, String)>,
    pub deleted: Vec<String>,
    /// Items without the dates a page needs when it has no heading.
    pub undated: Vec<String>,
    /// Items whose id can't be a segment of an object path.
    pub bad_ids: Vec<String>,
}

/// Leave out deleted and superseded items, and items without dates. Warns
/// about kept items that share a Congress, session, part and appendix,
/// which a new item from UNT could add.
pub fn plan(items: Vec<Item>) -> Plan {
    let mut p = Plan::default();
    for i in items {
        if i.deleted {
            p.deleted.push(i.id);
        } else if !usnm_store::is_safe_segment(&i.id) {
            tracing::warn!(item = %i.id, "a Globe item id that can't be an object path; left out");
            p.bad_ids.push(i.id);
        } else if let Some((_, by, _)) = SUPERSEDED.iter().find(|(id, _, _)| *id == i.id) {
            p.superseded.push((i.id, (*by).to_owned()));
        } else if i.coverage.is_none() || i.id.is_empty() {
            p.undated.push(i.id);
        } else {
            p.fetch.push(i);
        }
    }
    p.fetch
        .sort_by(|a, b| (a.ordinal(), &a.id).cmp(&(b.ordinal(), &b.id)));
    let mut seen: BTreeMap<u32, (&str, &str)> = BTreeMap::new();
    for i in &p.fetch {
        if let Some((other, extent)) = seen.insert(i.ordinal(), (&i.id, &i.extent)) {
            if *extent == i.extent {
                tracing::warn!(
                    item = %i.id, other, extent = %i.extent,
                    "two Globe items with one session, part and extent: check for duplicate pages"
                );
            }
        }
    }
    p
}

/// One page of an item, from its IIIF manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Canvas {
    pub seq: u32,
    pub label: Option<String>,
}

#[derive(Deserialize)]
struct Manifest {
    #[serde(default)]
    sequences: Vec<Sequence>,
}

#[derive(Deserialize)]
struct Sequence {
    #[serde(default)]
    canvases: Vec<ManifestCanvas>,
}

#[derive(Deserialize)]
struct ManifestCanvas {
    #[serde(rename = "@id")]
    id: String,
    #[serde(default)]
    label: Option<String>,
}

/// The pages of an item's IIIF manifest, by the `m1/{seq}` in each canvas id.
pub fn parse_manifest(json: &[u8]) -> anyhow::Result<Vec<Canvas>> {
    let m: Manifest = serde_json::from_slice(json)?;
    let mut out = Vec::new();
    for c in m.sequences.into_iter().flat_map(|s| s.canvases) {
        let seq =
            c.id.split('/')
                .skip_while(|s| *s != "m1")
                .nth(1)
                .and_then(|s| s.parse::<u32>().ok())
                .with_context(|| format!("canvas `{}` has no page number", c.id))?;
        out.push(Canvas {
            seq,
            label: c.label,
        });
    }
    // A manifest with no pages would curate an empty attempt and mark the
    // item finished; it is a bad answer, so the item is tried again.
    anyhow::ensure!(!out.is_empty(), "the manifest lists no pages");
    Ok(out)
}

/// The facts design §5 gives a page: its date and where it came from, its
/// chamber, and whether it is in the appendix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageFacts {
    pub date: NaiveDate,
    pub date_from: &'static str,
    pub chamber: Option<Chamber>,
    pub appendix: bool,
}

/// How far a heading's date may fall outside the item's dates.
const DATE_SLACK_DAYS: i64 = 7;
/// How far back from the day before a debates heading may go: further back
/// is more likely a misread digit than a session out of order.
const BACKWARDS_DAYS: i64 = 3;

/// Dates and chambers through an item's pages, in order (design §5).
#[derive(Debug, Clone)]
pub struct Dating {
    first: NaiveDate,
    last: NaiveDate,
    appendix: bool,
    carried: Option<NaiveDate>,
    chamber: Option<Chamber>,
}

impl Dating {
    pub fn new(item: &Item) -> anyhow::Result<Self> {
        let (first, last) = item
            .coverage
            .with_context(|| format!("item `{}` has no dates", item.id))?;
        Ok(Self {
            first,
            last,
            appendix: item.appendix,
            carried: None,
            chamber: None,
        })
    }

    /// The next page's facts.
    ///
    /// - The appendix starts at its title page and runs to the item's end;
    ///   the date and chamber carried so far are dropped there.
    /// - A page takes the date of its first heading; a page without one,
    ///   the last one before it (`carried`); a page before any, the
    ///   session's first day, or in the appendix its last (`session`).
    /// - The chamber likewise, without a fallback.
    pub fn page(&mut self, lines: &[String]) -> PageFacts {
        if !self.appendix && parse::starts_appendix(lines) {
            self.appendix = true;
            self.carried = None;
            self.chamber = None;
        }
        let low = self.first - chrono::Duration::days(DATE_SLACK_DAYS);
        let high = self.last + chrono::Duration::days(DATE_SLACK_DAYS);
        let mut first_date = None;
        let mut first_chamber = None;
        for h in lines.iter().filter_map(|l| parse::heading(l)) {
            if let Some(d) = h.date {
                let backwards = !self.appendix
                    && self
                        .carried
                        .is_some_and(|c| d < c - chrono::Duration::days(BACKWARDS_DAYS));
                if d >= low && d <= high && !backwards {
                    first_date.get_or_insert(d);
                    self.carried = Some(d);
                }
            }
            if let Some(c) = h.chamber {
                first_chamber.get_or_insert(c);
                self.chamber = Some(c);
            }
        }
        let carried_before = first_date.is_none().then_some(self.carried).flatten();
        let (date, date_from) = match (first_date, carried_before) {
            (Some(d), _) if self.appendix => (d, "appendix_heading"),
            (Some(d), _) => (d, "heading"),
            (None, Some(d)) => (d, "carried"),
            (None, None) if self.appendix => (self.last, "session"),
            (None, None) => (self.first, "session"),
        };
        PageFacts {
            date,
            date_from,
            chamber: first_chamber.or(self.chamber),
            appendix: self.appendix,
        }
    }
}

/// `25th`, `2nd`.
fn ordinal(n: &str) -> String {
    let v: u32 = n.parse().unwrap_or(0);
    let suffix = match (v % 10, v % 100) {
        (1, x) if x != 11 => "st",
        (2, x) if x != 12 => "nd",
        (3, x) if x != 13 => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

/// `Cong. Globe, 25th Cong., 2nd Sess. 300` (`Cong. Globe App., …` in the
/// appendix), the page left off when it isn't known.
fn citation(item: &Item, appendix: bool, page: Option<&str>) -> Option<String> {
    let congress = item.congress?;
    let session = item.session.as_deref()?;
    let app = if appendix { " App." } else { "" };
    let mut c = format!(
        "Cong. Globe{app}, {} Cong., {} Sess.",
        ordinal(&congress.to_string()),
        ordinal(session)
    );
    if let Some(p) = page {
        c.push(' ');
        c.push_str(p);
    }
    Some(c)
}

/// The curated row of one page.
pub fn page_row(
    item: &Item,
    canvas: &Canvas,
    lines: &[String],
    facts: &PageFacts,
    fetched_at: DateTime<Utc>,
) -> CuratedPage {
    let text = usnm_core::text::normalize_ocr(&lines.join("\n"));
    let status = usnm_core::text::text_status(&text);
    let ok = status == usnm_core::text::TextStatus::Ok;
    let speakers = if ok {
        parse::speakers(&text)
    } else {
        Vec::new()
    };
    let states = speakers
        .iter()
        .filter_map(|s| {
            Some(StateRef {
                state: s.state.clone()?,
                via: "speaker".into(),
                value: Some(s.as_printed.clone()),
            })
        })
        .collect();
    let printed = parse::printed_page(canvas.label.as_deref(), lines);
    CuratedPage {
        doc_id: format!("unt-{}_p{}", item.id, canvas.seq),
        source: SOURCE.into(),
        series: SERIES.into(),
        unit_id: item.id.clone(),
        unit_ordinal: item.ordinal(),
        package_id: Some(item.id.clone()),
        seq: canvas.seq,
        citation: citation(item, facts.appendix, printed.as_deref()),
        printed_page: printed,
        appendix: facts.appendix,
        date: facts.date,
        date_precision: "day".into(),
        date_from: facts.date_from.into(),
        congress: item.congress,
        session: item.session.clone(),
        chamber: facts.chamber.map(|c| c.as_str().to_owned()),
        doc_class: None,
        doc_number: None,
        serial: None,
        title: (!item.title.is_empty()).then(|| item.title.clone()),
        text_status: match status {
            usnm_core::text::TextStatus::Ok => "ok",
            usnm_core::text::TextStatus::Short => "short",
            usnm_core::text::TextStatus::Empty => "empty",
        }
        .into(),
        text_chars: i32::try_from(text.chars().count()).unwrap_or(i32::MAX),
        text_sha256: Sha256::digest(text.as_bytes()).into(),
        text: ok.then_some(text),
        ocr_source: SOURCE.into(),
        ocr_engine: None,
        link: Some(format!("{UNT}/{ARK}/{}/m1/{}/", item.id, canvas.seq)),
        states,
        places: Vec::new(),
        speakers,
        fetched_at,
    }
}

/// The URLs of one item's manifest and page OCR, under `base`.
fn manifest_url(base: &str, id: &str) -> String {
    format!("{base}/{ARK}/{id}/manifest/")
}

fn ocr_url(base: &str, id: &str, seq: u32) -> String {
    format!("{base}/{ARK}/{id}/m1/{seq}/annotations/ocr/")
}

/// Where a response is kept in the raw store.
fn raw_manifest(id: &str) -> String {
    format!("congress/unt/{id}/manifest.json.zst")
}

fn raw_ocr(id: &str, seq: u32) -> String {
    format!("congress/unt/{id}/ocr/{seq:05}.json.zst")
}

/// What the command is asked to do.
#[derive(Debug, Clone)]
pub struct Options {
    /// Where to fetch from: UNT, or a copy of its responses (tests).
    pub base: String,
    pub set: String,
    /// Only these items (`metadc29274`); empty: every item.
    pub items: Vec<String>,
    /// Fetch items that already have a finished attempt again.
    pub refetch: bool,
    /// Stop starting items, and stop an item between pages, at this time.
    pub deadline: Option<tokio::time::Instant>,
    /// Stop after this many items.
    pub max_items: Option<usize>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            base: UNT.into(),
            set: SET.into(),
            items: Vec::new(),
            refetch: false,
            deadline: None,
            max_items: None,
        }
    }
}

/// What a run did, printed as JSON at the end.
#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub listed: usize,
    pub superseded: Vec<(String, String)>,
    pub deleted: Vec<String>,
    pub undated: Vec<String>,
    /// Items left out for an id that can't be an object path.
    pub bad_ids: Vec<String>,
    /// Items with a finished attempt already, not fetched again.
    pub already: usize,
    /// Items written, with their attempt and pages.
    pub written: Vec<(String, u32, u64)>,
    pub pages: u64,
    /// Pages UNT has no OCR for (404).
    pub pages_without_ocr: u64,
    /// Items left for the next run: the deadline, `--max-items` or a lost lock.
    pub left: usize,
}

/// List the collection, keeping each response in the raw store.
async fn list(
    fetch: &dyn Fetch,
    raw: Option<&dyn ObjectStore>,
    opts: &Options,
    now: DateTime<Utc>,
) -> anyhow::Result<Vec<Item>> {
    let mut url = format!(
        "{}/oai/?verb=ListRecords&metadataPrefix=untl&set={}",
        opts.base, opts.set
    );
    let mut items = Vec::new();
    for page in 0..1000 {
        let body = fetch
            .get(&url)
            .await?
            .with_context(|| format!("`{url}` answered 404"))?;
        if let Some(raw) = raw {
            let path = format!(
                "congress/unt/oai/{}-{page:03}.xml.zst",
                now.format("%Y%m%dT%H%M%SZ")
            );
            raw.put_new(&path, zstd::encode_all(&body[..], 9)?, "application/zstd")
                .await?;
        }
        let (mut got, token) = parse_records(&body).with_context(|| url.clone())?;
        items.append(&mut got);
        match token {
            Some(t) => {
                url = format!(
                    "{}/oai/?verb=ListRecords&resumptionToken={}",
                    opts.base,
                    form_urlencoded_byte(&t)
                );
            }
            None => return Ok(items),
        }
    }
    bail!("OAI-PMH listing ran past 1,000 pages")
}

/// Percent-encode a resumption token for a query string.
fn form_urlencoded_byte(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Why an item stopped before its last page.
enum Stopped {
    Deadline,
    Lost,
}

/// Fetch and read one item's pages.
async fn fetch_item(
    fetch: &dyn Fetch,
    raw: Option<&dyn ObjectStore>,
    opts: &Options,
    item: &Item,
    lost: &AtomicBool,
    report: &mut Report,
) -> anyhow::Result<Result<Vec<CuratedPage>, Stopped>> {
    let manifest = cached(
        fetch,
        raw,
        &raw_manifest(&item.id),
        &manifest_url(&opts.base, &item.id),
        |b| parse_manifest(b).is_ok(),
    )
    .await?
    .with_context(|| format!("item `{}` has no manifest", item.id))?;
    let canvases = parse_manifest(&manifest).with_context(|| format!("{} manifest", item.id))?;
    let mut dating = Dating::new(item)?;
    let mut rows = Vec::with_capacity(canvases.len());
    for (n, c) in canvases.iter().enumerate() {
        if lost.load(Ordering::SeqCst) {
            return Ok(Err(Stopped::Lost));
        }
        if opts
            .deadline
            .is_some_and(|d| tokio::time::Instant::now() >= d)
        {
            return Ok(Err(Stopped::Deadline));
        }
        let body = cached(
            fetch,
            raw,
            &raw_ocr(&item.id, c.seq),
            &ocr_url(&opts.base, &item.id, c.seq),
            |b| parse::page_lines(b).is_ok(),
        )
        .await?;
        let lines = match &body {
            Some(b) => parse::page_lines(b).with_context(|| {
                format!("{} page {}: OCR isn't an annotation list", item.id, c.seq)
            })?,
            None => {
                report.pages_without_ocr += 1;
                Vec::new()
            }
        };
        let facts = dating.page(&lines);
        rows.push(page_row(item, c, &lines, &facts, Utc::now()));
        if (n + 1) % 100 == 0 {
            tracing::info!(item = %item.id, pages = n + 1, of = canvases.len(), "Globe pages read");
        }
    }
    Ok(Ok(rows))
}

/// Run the fetch (steps 1 to 5). `lost` is set when the lock that keeps two
/// runs apart can no longer be renewed: the run stops between pages.
pub async fn run(
    fetch: &dyn Fetch,
    curated: &dyn ObjectStore,
    raw: Option<&dyn ObjectStore>,
    opts: &Options,
    lost: &AtomicBool,
) -> anyhow::Result<Report> {
    let started = Utc::now();
    let items = list(fetch, raw, opts, started).await?;
    let mut report = Report {
        listed: items.len(),
        ..Report::default()
    };
    let mut p = plan(items);
    report.superseded = p.superseded.clone();
    report.deleted = p.deleted.clone();
    report.undated = p.undated.clone();
    report.bad_ids = p.bad_ids.clone();
    if !opts.items.is_empty() {
        let unknown: Vec<&String> = opts
            .items
            .iter()
            .filter(|w| !p.fetch.iter().any(|i| &&i.id == w))
            .collect();
        if !unknown.is_empty() {
            bail!("not among the items to fetch: {unknown:?} (superseded, deleted or not listed)");
        }
        p.fetch.retain(|i| opts.items.contains(&i.id));
    }
    let finished: Vec<String> = curated::finished(curated)
        .await?
        .into_iter()
        .filter(|f| f.series == SERIES)
        .map(|f| f.unit)
        .collect();
    tracing::info!(
        listed = report.listed,
        to_fetch = p.fetch.len(),
        superseded = p.superseded.len(),
        deleted = p.deleted.len(),
        undated = p.undated.len(),
        bad_ids = p.bad_ids.len(),
        finished = finished.len(),
        "Congressional Globe items"
    );
    let todo: Vec<&Item> = p
        .fetch
        .iter()
        .filter(|i| {
            let done = finished.contains(&i.id);
            // Counted as not fetched again only when it isn't.
            if done && !opts.refetch {
                report.already += 1;
            }
            opts.refetch || !done
        })
        .collect();
    for (n, item) in todo.iter().enumerate() {
        let out_of_time = opts
            .deadline
            .is_some_and(|d| tokio::time::Instant::now() >= d);
        if opts.max_items.is_some_and(|m| n >= m) || out_of_time || lost.load(Ordering::SeqCst) {
            report.left = todo.len() - n;
            break;
        }
        tracing::info!(item = %item.id, title = %item.title, "fetching a Globe item");
        match fetch_item(fetch, raw, opts, item, lost, &mut report).await? {
            // Lost during the last page: nothing to see it between pages.
            Ok(_) if lost.load(Ordering::SeqCst) => {
                report.left = todo.len() - n;
                tracing::warn!(item = %item.id, "the fetch lock was lost; stopping");
                break;
            }
            Ok(rows) => {
                let counts =
                    curated::write_attempt(curated, SERIES, SOURCE, &item.id, &rows, Utc::now())
                        .await?;
                tracing::info!(
                    item = %item.id, attempt = counts.attempt, pages = counts.pages,
                    indexable = counts.indexable, appendix = counts.appendix_pages,
                    date_from = ?counts.date_from, "Globe item curated"
                );
                report.pages += counts.pages;
                report
                    .written
                    .push((item.id.clone(), counts.attempt, counts.pages));
            }
            Err(why) => {
                report.left = todo.len() - n;
                match why {
                    Stopped::Deadline => {
                        tracing::info!(item = %item.id, "deadline reached mid-item; the next run resumes it")
                    }
                    Stopped::Lost => {
                        tracing::warn!(item = %item.id, "the fetch lock was lost; stopping")
                    }
                }
                break;
            }
        }
    }
    if lost.load(Ordering::SeqCst) {
        bail!("the `{LOCK}` lock could not be renewed; stopped without finishing");
    }
    Ok(report)
}

/// Hold [`LOCK`] while `body` runs, renewing it; `lost` is set if a renewal fails.
pub async fn locked<T>(
    state: &crate::state::State,
    owner: &str,
    body: impl std::future::Future<Output = anyhow::Result<T>>,
    lost: Arc<AtomicBool>,
) -> anyhow::Result<T> {
    super::fetch::locked(state, LOCK, owner, body, lost).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_waits_double_honour_retry_after_and_stay_under_ten_minutes() {
        let d = Duration::from_secs(3);
        assert_eq!(retry_wait(d, 0, None), Duration::from_secs(6));
        assert_eq!(retry_wait(d, 2, None), Duration::from_secs(24));
        // A short hint doesn't shorten the doubling; a longer one is honoured.
        assert_eq!(
            retry_wait(d, 1, Some(Duration::from_secs(5))),
            Duration::from_secs(12)
        );
        assert_eq!(
            retry_wait(d, 0, Some(Duration::from_secs(120))),
            Duration::from_secs(120)
        );
        // The cap holds for the doubling and for a hint over it.
        assert_eq!(retry_wait(d, 10, None), MAX_RETRY_WAIT);
        assert_eq!(
            retry_wait(d, 0, Some(Duration::from_secs(3600))),
            MAX_RETRY_WAIT
        );
        assert_eq!(
            retry_wait(d, 0, Some(Duration::from_secs(u64::MAX))),
            MAX_RETRY_WAIT
        );
    }

    fn item(sort: &str, official: &str) -> Item {
        let mut i = Item {
            id: "metadc1".into(),
            title: official.into(),
            sort_title: sort.into(),
            coverage: Some((
                NaiveDate::from_ymd_opt(1837, 12, 4).unwrap(),
                NaiveDate::from_ymd_opt(1838, 7, 18).unwrap(),
            )),
            ..Item::default()
        };
        i.read_sort_title();
        i
    }

    #[test]
    fn sort_titles_give_congress_session_part_and_appendix() {
        let i = item(
            "Congressional Globe, Congress 25, Session 2",
            "The Congressional Globe, Volume 6",
        );
        assert_eq!(
            (i.congress, i.session.as_deref(), i.part, i.appendix),
            (Some(25), Some("2"), None, false)
        );
        let i = item("Congressional Globe, Congress 24, Session 1x", "x");
        assert!(i.appendix);
        let i = item("Congressional Globe, Congress 36, Session 1, Part 2", "x");
        assert_eq!(i.part, Some(2));
        let i = item(
            "Congressional Globe, Congress 32, Session 1, Volume 25, App.",
            "x",
        );
        assert!(i.appendix);
        let i = item(
            "Congressional Globe, Congress 34, Session 1, Part 4",
            "Appendix to the Congressional Globe: Containing Speeches",
        );
        assert!(i.appendix);
        assert!(
            item("Congressional Globe, Congress 25, Session 2", "a").ordinal()
                < item("Congressional Globe, Congress 25, Session 2x", "a").ordinal()
        );
    }

    #[test]
    fn superseded_and_deleted_items_are_left_out() {
        let mut a = item("Congressional Globe, Congress 24, Session 1", "x");
        a.id = "metadc29282".into();
        let mut b = a.clone();
        b.id = "metadc30905".into();
        let mut c = a.clone();
        c.id = "metadc99999".into();
        c.deleted = true;
        let mut d = a.clone();
        d.id = "metadc88888".into();
        d.coverage = None;
        let mut e = a.clone();
        e.id = "../metadc1".into();
        let p = plan(vec![b, a, c, d, e]);
        assert_eq!(
            p.fetch.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            ["metadc29282"]
        );
        assert_eq!(
            p.superseded,
            [("metadc30905".to_owned(), "metadc29282".to_owned())]
        );
        assert_eq!(p.deleted, ["metadc99999"]);
        assert_eq!(p.undated, ["metadc88888"]);
        assert_eq!(p.bad_ids, ["../metadc1"]);
    }

    fn lines(s: &[&str]) -> Vec<String> {
        s.iter().map(|l| (*l).to_owned()).collect()
    }

    #[test]
    fn pages_are_dated_by_headings_carried_forward() {
        let mut d = Dating::new(&item("Congressional Globe, Congress 25, Session 2", "x")).unwrap();
        let day = |m, dd| NaiveDate::from_ymd_opt(1838, m, dd).unwrap();
        // Front matter: the session's first day.
        let f = d.page(&lines(&["INDEX TO THE SIXTH VOLUME"]));
        assert_eq!(
            (f.date, f.date_from),
            (NaiveDate::from_ymd_opt(1837, 12, 4).unwrap(), "session")
        );
        let f = d.page(&lines(&[
            "IN SENATE",
            "Wednesday March 28 1838",
            "text",
            "HOUSE OF REPRESENTATIVES",
            "TftuasDAY March 29 1838",
        ]));
        assert_eq!(
            (f.date, f.date_from, f.chamber),
            (day(3, 28), "heading", Some(Chamber::Senate))
        );
        let f = d.page(&lines(&["Mr CLAY of Alabama said"]));
        assert_eq!(
            (f.date, f.date_from, f.chamber),
            (day(3, 29), "carried", Some(Chamber::House))
        );
        // A misread day well before the last is ignored.
        let f = d.page(&lines(&["THURSDAY March 1 1838"]));
        assert_eq!((f.date, f.date_from), (day(3, 29), "carried"));
        // The appendix: nothing carries over; then speech headings.
        let f = d.page(&lines(&["APPENDIX", "TO THE CONGRESSIONAL GLOBE"]));
        assert!(f.appendix);
        assert_eq!(
            (f.date, f.date_from, f.chamber),
            (day(7, 18), "session", None)
        );
        let f = d.page(&lines(&[
            "SPEECH OF MR CLAY",
            "In Senate, March 3, 1838",
            "Mr President",
        ]));
        assert_eq!(
            (f.date, f.date_from, f.chamber),
            (day(3, 3), "appendix_heading", Some(Chamber::Senate))
        );
        let f = d.page(&lines(&["more of the speech"]));
        assert_eq!(
            (f.date, f.date_from, f.appendix),
            (day(3, 3), "carried", true)
        );
    }

    fn recorded(name: &str) -> Vec<u8> {
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/congress/unt");
        std::fs::read(dir.join(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
    }

    /// UNT as recorded in `fixtures/congress/unt/`: what the fetch asks for,
    /// by URL, and how many times. A page with no recorded OCR is a 404.
    #[derive(Default)]
    struct Recorded {
        asked: std::sync::Mutex<Vec<String>>,
        /// Answer only the listing: everything else must come from the raw store.
        listing_only: bool,
    }

    const BASE: &str = "https://unt.example";

    #[async_trait::async_trait]
    impl Fetch for Recorded {
        async fn get(&self, url: &str) -> anyhow::Result<Option<Vec<u8>>> {
            self.asked.lock().unwrap().push(url.to_owned());
            let path = url.strip_prefix(BASE).expect("only the recorded host");
            if path == "/oai/?verb=ListRecords&metadataPrefix=untl&set=collection:CGLOB" {
                return Ok(Some(recorded("oai-cglob-1.xml")));
            }
            if path == "/oai/?verb=ListRecords&resumptionToken=fixture%2Fpage2" {
                return Ok(Some(recorded("oai-cglob-2.xml")));
            }
            assert!(!self.listing_only, "asked UNT for {url}");
            let rest = path.strip_prefix("/ark:/67531/").expect("an ARK");
            let parts: Vec<&str> = rest.trim_end_matches('/').split('/').collect();
            match parts.as_slice() {
                [id, "manifest"] => Ok(Some(recorded(&format!("{id}-manifest.json")))),
                [id, "m1", seq, "annotations", "ocr"] => {
                    let name = format!("{id}-m1-{seq}-ocr.json");
                    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join("../../fixtures/congress/unt");
                    Ok(dir.join(&name).exists().then(|| recorded(&name)))
                }
                _ => panic!("unexpected request {url}"),
            }
        }
    }

    #[test]
    fn the_recorded_listing_pages_and_its_items() {
        let (items, token) = parse_records(&recorded("oai-cglob-1.xml")).unwrap();
        assert_eq!(token.as_deref(), Some("fixture/page2"));
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, ["metadc29274", "metadc29282"]);
        let i = &items[0];
        assert_eq!(
            i.title,
            "The Congressional Globe, Volume 6: Twenty-Fifth Congress, Second Session"
        );
        assert_eq!(
            (i.congress, i.session.as_deref(), i.appendix),
            (Some(25), Some("2"), false)
        );
        assert_eq!(
            i.coverage,
            Some((
                NaiveDate::from_ymd_opt(1837, 12, 4).unwrap(),
                NaiveDate::from_ymd_opt(1838, 7, 18).unwrap()
            ))
        );
        assert_eq!(i.extent, "512, 641, 6 p. ; 30 cm.");
        let (items, token) = parse_records(&recorded("oai-cglob-2.xml")).unwrap();
        assert_eq!(token, None);
        assert_eq!(items.len(), 2);
        assert!(items[1].appendix, "{:?}", items[1]);
        assert!(
            parse_records(b"<OAI-PMH><error code=\"badArgument\">no</error></OAI-PMH>").is_err()
        );
        assert!(
            parse_records(b"<OAI-PMH><error code=\"noRecordsMatch\"/></OAI-PMH>")
                .unwrap()
                .0
                .is_empty()
        );
        // A self-closing error is an error, not an empty last page.
        let err = parse_records(b"<OAI-PMH><error code=\"badResumptionToken\"/></OAI-PMH>")
            .unwrap_err()
            .to_string();
        assert!(err.contains("badResumptionToken"), "{err}");
        assert!(parse_records(b"<OAI-PMH><error/></OAI-PMH>").is_err());
        // A page that isn't a listing is refused; an empty listing is not.
        assert!(parse_records(b"<html><body>Down for maintenance</body></html>").is_err());
        assert!(parse_records(b"<OAI-PMH><ListRecords/></OAI-PMH>")
            .unwrap()
            .0
            .is_empty());
    }

    /// Answers every request with the same body, counting them.
    struct Same(&'static [u8], std::sync::atomic::AtomicUsize);

    #[async_trait::async_trait]
    impl Fetch for Same {
        async fn get(&self, _url: &str) -> anyhow::Result<Option<Vec<u8>>> {
            self.1.fetch_add(1, Ordering::SeqCst);
            Ok(Some(self.0.to_vec()))
        }
    }

    #[tokio::test]
    async fn a_bad_answer_is_not_kept_and_a_kept_one_is_passed_over() {
        let dir = tempfile::tempdir().unwrap();
        let raw = usnm_store::LocalStore::new(dir.path());
        let valid = |b: &[u8]| parse_manifest(b).is_ok();
        // An empty manifest: returned, not kept, so the next run asks again.
        let bad = Same(b"{}", Default::default());
        let got = cached(&bad, Some(&raw), "m.json.zst", "u", valid)
            .await
            .unwrap();
        assert_eq!(got.as_deref(), Some(&b"{}"[..]));
        assert!(!raw.exists("m.json.zst").await.unwrap());
        // One kept before the check is passed over and fetched again.
        raw.put_new(
            "m.json.zst",
            zstd::encode_all(&b"{}"[..], 9).unwrap(),
            "application/zstd",
        )
        .await
        .unwrap();
        let good = Same(recorded_static(), Default::default());
        let got = cached(&good, Some(&raw), "m.json.zst", "u", valid)
            .await
            .unwrap()
            .unwrap();
        assert!(parse_manifest(&got).is_ok());
        assert_eq!(good.1.load(Ordering::SeqCst), 1);
    }

    fn recorded_static() -> &'static [u8] {
        Box::leak(recorded("metadc29274-manifest.json").into_boxed_slice())
    }

    #[test]
    fn a_manifest_without_pages_is_refused() {
        assert!(parse_manifest(b"{}").is_err());
        assert!(parse_manifest(br#"{"sequences": []}"#).is_err());
        assert!(parse_manifest(br#"{"sequences": [{"canvases": []}]}"#).is_err());
    }

    #[test]
    fn manifests_give_pages_and_their_labels() {
        let c = parse_manifest(&recorded("metadc29274-manifest.json")).unwrap();
        let seqs: Vec<u32> = c.iter().map(|c| c.seq).collect();
        assert_eq!(seqs, [1, 2, 3, 300, 301, 541, 800]);
        assert_eq!(c[3].label.as_deref(), Some("Image 300"));
        let c = parse_manifest(&recorded("metadc30767-manifest.json")).unwrap();
        let labels: Vec<&str> = c.iter().filter_map(|c| c.label.as_deref()).collect();
        assert_eq!(
            labels[..4],
            [
                "No Page # [Sequence: 1]",
                "No Page # [Sequence: 2]",
                "No Page # [Sequence: 3]",
                "2"
            ]
        );
    }

    #[test]
    fn a_later_volume_s_running_head_gives_its_page() {
        let lines = parse::page_lines(&recorded("metadc30858-m1-100-ocr.json")).unwrap();
        assert_eq!(
            parse::printed_page(Some("Image 100"), &lines).as_deref(),
            Some("28")
        );
    }

    fn options() -> Options {
        Options {
            base: BASE.into(),
            items: vec!["metadc29274".into()],
            ..Options::default()
        }
    }

    #[tokio::test]
    async fn fetches_an_item_from_recorded_responses() {
        let dir = tempfile::tempdir().unwrap();
        let curated = usnm_store::LocalStore::new(dir.path().join("curated"));
        let raw = usnm_store::LocalStore::new(dir.path().join("raw"));
        let unt = Recorded::default();
        let lost = AtomicBool::new(false);
        let report = run(&unt, &curated, Some(&raw), &options(), &lost)
            .await
            .unwrap();
        assert_eq!(report.listed, 4);
        assert_eq!(report.superseded.len(), 2);
        assert_eq!(report.written, [("metadc29274".to_owned(), 1, 7)]);
        assert_eq!(report.pages_without_ocr, 1);
        // Two listing pages, a manifest and seven pages, nothing twice.
        assert_eq!(unt.asked.lock().unwrap().len(), 10);

        let f = curated::finished(&curated).await.unwrap();
        let (counts, _) = curated::read_counts(&curated, &f[0]).await.unwrap();
        let rows = curated::read_checked(&curated, &counts.parts[0])
            .await
            .unwrap();
        let by_seq: BTreeMap<u32, &CuratedPage> = rows.iter().map(|r| (r.seq, r)).collect();
        let day = |y, m, d| NaiveDate::from_ymd_opt(y, m, d).unwrap();
        let facts = |seq: u32| {
            let r = by_seq[&seq];
            (
                r.date,
                r.date_from.as_str(),
                r.chamber.as_deref(),
                r.appendix,
            )
        };
        assert_eq!(facts(1), (day(1837, 12, 4), "session", None, false));
        assert_eq!(by_seq[&2].text_status, "empty");
        assert_eq!(
            facts(300),
            (day(1838, 3, 29), "heading", Some("house"), false)
        );
        // Issue No. 18's masthead (April 9) heads this page, but its debates
        // are continued from No. 17: still the 29th.
        assert_eq!(
            facts(301),
            (day(1838, 3, 29), "carried", Some("house"), false)
        );
        assert_eq!(facts(541), (day(1838, 7, 18), "session", None, true));
        assert_eq!(facts(800), (day(1838, 7, 18), "session", None, true));
        let p300 = by_seq[&300];
        assert_eq!(p300.doc_id, "unt-metadc29274_p300");
        assert_eq!(
            p300.link.as_deref(),
            Some("https://digital.library.unt.edu/ark:/67531/metadc29274/m1/300/")
        );
        assert_eq!(
            p300.citation.as_deref(),
            Some("Cong. Globe, 25th Cong., 2nd Sess.")
        );
        assert!(p300
            .text
            .as_deref()
            .unwrap()
            .contains("HOUSE OP REPRESENTATIVES"));
        let named: Vec<&str> = p300
            .speakers
            .iter()
            .map(|s| s.as_printed.as_str())
            .collect();
        assert_eq!(named, ["CLAY of Alabama", "NORVELL"]);
        assert_eq!(p300.states[0].state, "AL");
        assert_eq!(by_seq[&800].printed_page.as_deref(), Some("260"));
        assert_eq!(
            by_seq[&800].citation.as_deref(),
            Some("Cong. Globe App., 25th Cong., 2nd Sess. 260")
        );
        assert_eq!(counts.text_status["empty"], 1);
        assert_eq!(counts.appendix_pages, 2);

        // Finished: not fetched again.
        let again = run(
            &Recorded::default(),
            &curated,
            Some(&raw),
            &options(),
            &lost,
        )
        .await
        .unwrap();
        assert_eq!((again.already, again.written.len()), (1, 0));
        // Fetched again on request, from the raw store's copies alone.
        let refetch = Options {
            refetch: true,
            ..options()
        };
        let cached = Recorded {
            listing_only: true,
            ..Recorded::default()
        };
        let again = run(&cached, &curated, Some(&raw), &refetch, &lost)
            .await
            .unwrap();
        assert_eq!(again.written, [("metadc29274".to_owned(), 2, 7)]);
        assert_eq!(again.pages_without_ocr, 1, "the 404 is kept as one");
        let f = curated::finished(&curated).await.unwrap();
        let (c2, _) = curated::read_counts(&curated, &f[0]).await.unwrap();
        assert_eq!(
            (c2.days.clone(), c2.text_status.clone()),
            (counts.days, counts.text_status)
        );
    }

    /// Memory docs whose replaces take ten seconds, counting those that finish.
    #[derive(Default)]
    struct SlowReplaces(crate::docs::MemoryDocs, std::sync::atomic::AtomicUsize);

    #[async_trait::async_trait]
    impl crate::docs::DocStore for SlowReplaces {
        async fn get(
            &self,
            c: &str,
            pk: &str,
            id: &str,
        ) -> anyhow::Result<Option<crate::docs::Versioned>> {
            self.0.get(c, pk, id).await
        }
        async fn create(
            &self,
            c: &str,
            pk: &str,
            doc: &serde_json::Value,
        ) -> anyhow::Result<Option<String>> {
            self.0.create(c, pk, doc).await
        }
        async fn replace(
            &self,
            c: &str,
            pk: &str,
            doc: &serde_json::Value,
            etag: &str,
        ) -> anyhow::Result<Option<String>> {
            tokio::time::sleep(Duration::from_secs(10)).await;
            let out = self.0.replace(c, pk, doc, etag).await;
            self.1.fetch_add(1, Ordering::SeqCst);
            out
        }
        async fn upsert(&self, c: &str, pk: &str, doc: &serde_json::Value) -> anyhow::Result<()> {
            self.0.upsert(c, pk, doc).await
        }
        async fn list(
            &self,
            c: &str,
            field: &str,
            values: &[&str],
        ) -> anyhow::Result<Vec<crate::docs::Versioned>> {
            self.0.list(c, field, values).await
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_lock_is_released_after_a_renewal_under_way() {
        let docs = Arc::new(SlowReplaces::default());
        let state = crate::state::State::new(docs.clone());
        let lost = Arc::new(AtomicBool::new(false));
        // The body ends while the first renewal (at 300 s) is still writing.
        let body = async {
            tokio::time::sleep(Duration::from_secs(305)).await;
            Ok(())
        };
        locked(&state, "run-1", body, lost.clone()).await.unwrap();
        assert!(!lost.load(Ordering::SeqCst));
        // The renewal finished, then the unlock: two replaces, the lock free.
        assert_eq!(docs.1.load(Ordering::SeqCst), 2);
        state
            .lock(LOCK, "run-2", chrono::Duration::minutes(30))
            .await
            .unwrap();
    }

    /// The recorded responses, losing the lock while the last page is asked for.
    struct LosesOnLastPage<'a>(Recorded, &'a AtomicBool);

    #[async_trait::async_trait]
    impl Fetch for LosesOnLastPage<'_> {
        async fn get(&self, url: &str) -> anyhow::Result<Option<Vec<u8>>> {
            if url.ends_with("/m1/800/annotations/ocr/") {
                self.1.store(true, Ordering::SeqCst);
            }
            self.0.get(url).await
        }
    }

    #[tokio::test]
    async fn a_lock_lost_on_the_last_page_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let curated = usnm_store::LocalStore::new(dir.path());
        let lost = AtomicBool::new(false);
        let unt = LosesOnLastPage(Recorded::default(), &lost);
        assert!(run(&unt, &curated, None, &options(), &lost).await.is_err());
        assert!(lost.load(Ordering::SeqCst), "the last page was asked for");
        assert!(curated::finished(&curated).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn stops_at_the_deadline_and_refuses_superseded_items() {
        let dir = tempfile::tempdir().unwrap();
        let curated = usnm_store::LocalStore::new(dir.path());
        let lost = AtomicBool::new(false);
        let past = Options {
            deadline: Some(tokio::time::Instant::now()),
            ..options()
        };
        let r = run(&Recorded::default(), &curated, None, &past, &lost)
            .await
            .unwrap();
        assert_eq!((r.written.len(), r.left), (0, 1));
        assert!(curated::finished(&curated).await.unwrap().is_empty());
        let superseded = Options {
            items: vec!["metadc30905".into()],
            ..options()
        };
        let err = run(&Recorded::default(), &curated, None, &superseded, &lost)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("metadc30905"), "{err}");
        lost.store(true, Ordering::SeqCst);
        assert!(run(&Recorded::default(), &curated, None, &options(), &lost)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn http_paces_retries_and_says_who_it_is() {
        use axum::http::{HeaderMap, StatusCode};
        let hits = Arc::new(std::sync::Mutex::new(
            Vec::<(String, tokio::time::Instant)>::new(),
        ));
        let h = hits.clone();
        let app = axum::Router::new().fallback(move |headers: HeaderMap, uri: axum::http::Uri| {
            let h = h.clone();
            async move {
                let ua = headers["user-agent"].to_str().unwrap().to_owned();
                assert_eq!(ua, USER_AGENT);
                let mut v = h.lock().unwrap();
                v.push((uri.path().to_owned(), tokio::time::Instant::now()));
                let busy =
                    uri.path() == "/busy" && v.iter().filter(|(p, _)| p == "/busy").count() == 1;
                match uri.path() {
                    "/missing" => (StatusCode::NOT_FOUND, "no"),
                    "/busy" if busy => (StatusCode::SERVICE_UNAVAILABLE, "later"),
                    "/bad" => (StatusCode::FORBIDDEN, "no"),
                    _ => (StatusCode::OK, "ok"),
                }
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let http = Http::new(Duration::from_millis(200), USER_AGENT, "UNT").unwrap();
        let base = format!("http://{addr}");
        assert_eq!(
            http.get(&format!("{base}/a")).await.unwrap().unwrap(),
            b"ok"
        );
        assert_eq!(http.get(&format!("{base}/missing")).await.unwrap(), None);
        assert_eq!(
            http.get(&format!("{base}/busy")).await.unwrap().unwrap(),
            b"ok"
        );
        assert!(http.get(&format!("{base}/bad")).await.is_err());
        let v = hits.lock().unwrap();
        assert_eq!(v.len(), 5);
        for w in v.windows(2) {
            assert!(w[1].1 - w[0].1 >= Duration::from_millis(190), "{v:?}");
        }
        // The retry waited longer than the crawl delay.
        assert!(v[3].1 - v[2].1 >= Duration::from_secs(2), "{v:?}");
    }

    /// A 503 whose `Retry-After` is a date, then a 200 whose body is cut
    /// short, then the page: both failures are retried, the first after
    /// the date.
    #[tokio::test]
    async fn http_retries_a_cut_body_and_honours_a_dated_retry_after() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(std::sync::Mutex::new(Vec::<tokio::time::Instant>::new()));
        let h = hits.clone();
        tokio::spawn(async move {
            // The three connections the fetch should need; a fourth is refused.
            for n in 0..3 {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut req = [0u8; 4096];
                let _ = sock.read(&mut req).await;
                h.lock().unwrap().push(tokio::time::Instant::now());
                let reply = match n {
                    0 => {
                        let at = Utc::now() + chrono::Duration::seconds(5);
                        format!(
                            "HTTP/1.1 503 Service Unavailable\r\nretry-after: {}\r\n\
                             content-length: 0\r\nconnection: close\r\n\r\n",
                            at.format("%a, %d %b %Y %H:%M:%S GMT")
                        )
                    }
                    1 => "HTTP/1.1 200 OK\r\ncontent-length: 10\r\nconnection: close\r\n\r\nok"
                        .to_owned(),
                    _ => "HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok"
                        .to_owned(),
                };
                sock.write_all(reply.as_bytes()).await.unwrap();
                let _ = sock.shutdown().await;
            }
        });
        let http = Http::new(Duration::from_millis(10), USER_AGENT, "UNT").unwrap();
        let got = http.get(&format!("http://{addr}/p")).await.unwrap();
        assert_eq!(got.unwrap(), b"ok");
        let v = hits.lock().unwrap();
        assert_eq!(v.len(), 3, "{v:?}");
        // The date is whole seconds, so at least 4 s away; the doubling alone is 2 s.
        assert!(v[1] - v[0] >= Duration::from_millis(3500), "{v:?}");
        // The cut body counts as an attempt: the next wait doubles again.
        assert!(v[2] - v[1] >= Duration::from_secs(4), "{v:?}");
    }

    #[test]
    fn citations_name_the_appendix_and_page() {
        let i = item("Congressional Globe, Congress 25, Session 2", "x");
        assert_eq!(
            citation(&i, false, Some("300")).unwrap(),
            "Cong. Globe, 25th Cong., 2nd Sess. 300"
        );
        assert_eq!(
            citation(&i, true, None).unwrap(),
            "Cong. Globe App., 25th Cong., 2nd Sess."
        );
        assert_eq!(ordinal("21"), "21st");
        assert_eq!(ordinal("11"), "11th");
        assert_eq!(ordinal("42"), "42nd");
        assert_eq!(ordinal("23"), "23rd");
    }
}
