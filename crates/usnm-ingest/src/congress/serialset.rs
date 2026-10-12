//! The Congressional Serial Set from GovInfo (#229 design §7, phase 2):
//! `usnm-ingest congress-serialset`. No API key: the sitemaps, MODS and PDFs
//! are public URLs.
//!
//! 1. **List.** The sitemap index (`/sitemap/SERIALSET_sitemap_index.xml`)
//!    names one sitemap per year (182 of them, 1817 to 1998); each lists
//!    package ids with a `lastmod`. A volume package (`SERIALSET-00033_00_00`)
//!    is the whole bound volume and duplicates its documents, so only the
//!    document packages (`SERIALSET-00033_00_00-009-0042-0000`) are fetched,
//!    grouped by their volume, the unit a curated attempt holds. Every
//!    yearly sitemap is read, even with `--years`, since a volume's packages
//!    can be listed in more than one: `--years` picks the volumes with a
//!    package in those years, and each is still made of its packages in
//!    every sitemap, so a run with `--years` writes the same attempt a run
//!    without it would.
//! 2. **Decide.** A volume is fetched when it has no finished or empty
//!    attempt, or when its packages and their `lastmod`s differ from those
//!    the newer of them read ([`Volume::fingerprint`], recorded as
//!    `Counts::source_packages` and `curated::Empty::source_packages`): a
//!    document GPO changed, added or dropped, whatever its `lastmod`. An
//!    attempt written before the fingerprint was recorded has none and is
//!    fetched again (from the raw store, when it holds the volume).
//!    `--refetch` fetches finished and empty volumes again.
//! 3. **Fetch.** Per document, its MODS (`/metadata/pkg/{id}/mods.xml`) and
//!    its PDF (`/content/pkg/{id}/pdf/{id}.pdf`), one request at a time,
//!    about a second apart ([`super::fetch::Http`]), backing off on 429 and
//!    5xx. The PDF is streamed to a scratch file, read by `pdftotext`
//!    ([`PdfText`]), and deleted: about 4 TB in all, never kept. With a raw
//!    store (`USNM_RAW_URL`, the archive), the MODS and each document's page
//!    texts (`pages.json`) are kept under `congress/govinfo/`, keyed by the
//!    package's `lastmod`, and read back instead of GovInfo, so a stopped
//!    volume resumes without downloading its PDFs again.
//! 4. **Read.** Each page's text, with `¬` at a line end read as a hyphen
//!    ([`page_text`]); the document's date from MODS `dateIssued` with its
//!    precision ([`date_of`]); its citation, chamber, class, number and serial
//!    number; the MODS `geographic` subjects as places, and the states they
//!    or the title name ([`states_of`]).
//! 5. **Write.** One curated attempt per volume
//!    (`congress/serialset/{volume}/…`, [`super::curated`]) once every
//!    document is read or left out for good: undated, or with no PDF where
//!    its MODS names none. A volume with a document GovInfo answered 404
//!    for (its MODS, or a PDF its MODS names) or that couldn't be read is
//!    `incomplete`: nothing is written, the 404 isn't kept, and the next
//!    run fetches it again, since a finished attempt records the volume's
//!    packages and would not be fetched again. A volume stopped by
//!    the deadline also writes nothing and is fetched again by the next run.
//!    A volume all of whose documents are left out for good gets an empty
//!    attempt (`empty.json`, [`curated::Empty`]) with its packages and what
//!    was left out, and no pages: later runs skip it like a finished one,
//!    and the release leaves out any earlier attempt of it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context};
use chrono::{DateTime, NaiveDate, Utc};
use quick_xml::events::{BytesStart, Event};
use serde::Serialize;
use sha2::{Digest, Sha256};
use usnm_store::ObjectStore;

use super::curated::{self, CuratedPage};
use super::fetch::{self, cached_found, Fetch};
use super::StateRef;

/// GovInfo: the links on hits, and where the fetch goes by default.
pub const GOVINFO: &str = "https://www.govinfo.gov";

/// About one request a second (#229 design §7).
pub const DELAY: Duration = Duration::from_secs(1);

/// Who is asking, and why, on every request.
pub const USER_AGENT: &str = concat!(
    "usnm-ingest/",
    env!("CARGO_PKG_VERSION"),
    " (+https://usnewsmap.com; Congressional Serial Set text for search, #229)"
);

pub const SERIES: &str = "serialset";
pub const SOURCE: &str = "govinfo";

/// The Cosmos lock that keeps two fetches from running at once: an item id
/// in the `ops` container, so no `/`.
pub const LOCK: &str = "congress-serialset";

/// The sitemap index, under the base URL.
const INDEX_PATH: &str = "/sitemap/SERIALSET_sitemap_index.xml";

const PREFIX: &str = "SERIALSET-";

/// A Serial Set package id: a volume (`SERIALSET-00033_00_00`) or a
/// document in it (`SERIALSET-00033_00_00-009-0042-0000`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct PackageId {
    pub id: String,
    /// The volume, GovInfo's `bagId`: `00033_00_00`.
    pub volume: String,
    /// The serial number: `33`.
    pub serial: u32,
    /// What follows the volume in a document's id: `009-0042-0000`.
    pub document: Option<String>,
}

impl PackageId {
    /// The id's parts, or `None` for an id that isn't a Serial Set package
    /// or can't be a segment of an object path.
    pub fn parse(id: &str) -> Option<Self> {
        if !usnm_store::is_safe_segment(id) {
            return None;
        }
        let rest = id.strip_prefix(PREFIX)?;
        let (volume, document) = match rest.split_once('-') {
            Some((v, d)) => (v, Some(d)),
            None => (rest, None),
        };
        let parts: Vec<&str> = volume.split('_').collect();
        let digits = |s: &str, n: usize| s.len() == n && s.bytes().all(|b| b.is_ascii_digit());
        if parts.len() != 3 || !digits(parts[0], 5) || !digits(parts[1], 2) || !digits(parts[2], 2)
        {
            return None;
        }
        if document.is_some_and(|d| {
            d.is_empty() || !d.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        }) {
            return None;
        }
        Some(Self {
            id: id.to_owned(),
            volume: volume.to_owned(),
            serial: parts[0].parse().ok()?,
            document: document.map(str::to_owned),
        })
    }

    /// A document's place in its volume: the first number of its suffix
    /// (`009` in `009-0042-0000`).
    pub fn sequence(&self) -> Option<u32> {
        self.document.as_deref()?.split('-').next()?.parse().ok()
    }
}

/// A Serial Set document's place among all of them, for the order of
/// same-day hits: its serial number, then its place in the volume.
pub fn ordinal(serial: u32, sequence: Option<u32>) -> u32 {
    (serial.min(0xF_FFFF) << 12) | sequence.unwrap_or(0).min(0xFFF)
}

/// One `<sitemap>` or `<url>` of a sitemap: where, and when it last changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub loc: String,
    pub lastmod: Option<String>,
}

/// Walk an XML document, calling `f` with each element's path (local names,
/// root first), its attributes and its text when it closes. The root's name.
fn walk(
    xml: &[u8],
    mut f: impl FnMut(&[String], &[(String, String)], &str),
) -> anyhow::Result<Option<String>> {
    fn attrs(e: &BytesStart<'_>) -> anyhow::Result<Vec<(String, String)>> {
        let mut out = Vec::new();
        for a in e.attributes().with_checks(true) {
            let a = a?;
            out.push((
                a.key.local_name().as_ref().to_owned(),
                a.normalized_value(quick_xml::XmlVersion::Implicit1_0)?
                    .into_owned(),
            ));
        }
        Ok(out)
    }
    let mut reader = quick_xml::Reader::from_reader(xml);
    let mut buf = Vec::new();
    let mut path: Vec<String> = Vec::new();
    let mut stack: Vec<(Vec<(String, String)>, String)> = Vec::new();
    let mut root = None;
    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Start(e) => {
                let name = e.local_name().as_ref().to_owned();
                root.get_or_insert_with(|| name.clone());
                stack.push((attrs(&e)?, String::new()));
                path.push(name);
            }
            Event::Empty(e) => {
                let name = e.local_name().as_ref().to_owned();
                root.get_or_insert_with(|| name.clone());
                let a = attrs(&e)?;
                path.push(name);
                f(&path, &a, "");
                path.pop();
            }
            Event::Text(t) => {
                if let Some((_, text)) = stack.last_mut() {
                    text.push_str(&t.into_inner());
                }
            }
            Event::CData(t) => {
                if let Some((_, text)) = stack.last_mut() {
                    text.push_str(&t.into_inner());
                }
            }
            Event::GeneralRef(r) => {
                if let Some((_, text)) = stack.last_mut() {
                    match &*r {
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
                    }
                }
            }
            Event::End(_) => {
                let (a, text) = stack.pop().context("an end tag with no start")?;
                f(&path, &a, text.trim());
                path.pop();
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(root)
}

/// The entries of a sitemap index (`root` `sitemapindex`, entries `sitemap`)
/// or a sitemap (`urlset`, `url`). Anything else (a maintenance page served
/// with a 200, say) is refused.
fn parse_sitemap(xml: &[u8], root: &str, entry: &str) -> anyhow::Result<Vec<Entry>> {
    let mut out = Vec::new();
    let mut loc = None;
    let mut lastmod = None;
    let got = walk(xml, |path, _, text| {
        let name = path.last().map(String::as_str).unwrap_or_default();
        let parent = path.len().checked_sub(2).map(|i| path[i].as_str());
        match (name, parent) {
            ("loc", Some(p)) if p == entry => loc = Some(text.to_owned()),
            ("lastmod", Some(p)) if p == entry => {
                lastmod = (!text.is_empty()).then(|| text.to_owned());
            }
            (n, _) if n == entry => {
                if let Some(l) = loc.take().filter(|l| !l.is_empty()) {
                    out.push(Entry {
                        loc: l,
                        lastmod: lastmod.take(),
                    });
                }
                lastmod = None;
            }
            _ => {}
        }
    })?;
    anyhow::ensure!(
        got.as_deref() == Some(root),
        "not a sitemap: the root is {got:?}, not `{root}`"
    );
    Ok(out)
}

/// The yearly sitemaps the index names.
pub fn parse_index(xml: &[u8]) -> anyhow::Result<Vec<Entry>> {
    parse_sitemap(xml, "sitemapindex", "sitemap")
}

/// The packages a yearly sitemap lists (`…/app/details/{id}`).
pub fn parse_urlset(xml: &[u8]) -> anyhow::Result<Vec<Entry>> {
    parse_sitemap(xml, "urlset", "url")
}

/// `SERIALSET_1820_sitemap.xml` → 1820.
pub fn sitemap_year(loc: &str) -> Option<u16> {
    let name = loc.rsplit('/').next()?;
    name.strip_prefix("SERIALSET_")?
        .strip_suffix("_sitemap.xml")?
        .parse()
        .ok()
}

/// A URL's path and query, under `base` (the index names `www.govinfo.gov`;
/// a test or a mirror is elsewhere).
fn rebase(base: &str, loc: &str) -> String {
    let path = match loc.split_once("://") {
        Some((_, rest)) => rest.find('/').map_or("/", |i| &rest[i..]),
        None => loc,
    };
    format!("{base}{path}")
}

/// What MODS says about a document (the fields GovInfo puts in its
/// `extension` first, the catalog's after).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Mods {
    pub title: Option<String>,
    /// `dateIssued` as given: GovInfo's `extension` copy, else the
    /// `w3cdtf` one, else any.
    pub date_issued: Option<String>,
    pub session_year: Option<String>,
    pub doc_class: Option<String>,
    pub document_number: Option<String>,
    pub serial: Option<String>,
    pub sequence: Option<String>,
    pub congress: Option<u16>,
    pub session: Option<String>,
    /// `HOUSE`, `SENATE`.
    pub chamber: Option<String>,
    pub volume: Option<String>,
    /// `H. Doc. 16-42`.
    pub citation: Option<String>,
    /// `12 p.`
    pub extent: Option<String>,
    pub serial_title: Option<String>,
    /// `subject/geographic`, each once, in order.
    pub geographic: Vec<String>,
    /// Whether `location` names a `PDF rendition`: a PDF GovInfo should have.
    pub pdf_rendition: bool,
}

/// Read a package's MODS. A response whose root isn't `mods` is refused.
pub fn parse_mods(xml: &[u8]) -> anyhow::Result<Mods> {
    let mut m = Mods::default();
    let (mut ext_date, mut w3c_date, mut any_date) = (None, None, None);
    let root = walk(xml, |path, attrs, text| {
        let name = path.last().map(String::as_str).unwrap_or_default();
        let parent = path
            .len()
            .checked_sub(2)
            .map(|i| path[i].as_str())
            .unwrap_or_default();
        let value = || (!text.is_empty()).then(|| text.to_owned());
        let set = |slot: &mut Option<String>| {
            if slot.is_none() {
                *slot = value();
            }
        };
        match (parent, name) {
            ("titleInfo", "title") if path.len() == 3 => set(&mut m.title),
            ("extension", "dateIssued") => set(&mut ext_date),
            ("originInfo", "dateIssued") => {
                if attrs.iter().any(|(k, v)| k == "encoding" && v == "w3cdtf") {
                    set(&mut w3c_date);
                }
                set(&mut any_date);
            }
            ("extension", "sessionYear") => set(&mut m.session_year),
            ("extension", "docClass") => set(&mut m.doc_class),
            ("extension", "documentNumber") => set(&mut m.document_number),
            ("extension", "serialSetNumber") => set(&mut m.serial),
            ("extension", "sequenceNumber") => set(&mut m.sequence),
            ("extension", "congress") if m.congress.is_none() => {
                m.congress = text.parse().ok();
            }
            ("extension", "session") => set(&mut m.session),
            ("extension", "chamber") => set(&mut m.chamber),
            ("extension", "volume") => set(&mut m.volume),
            ("extension", "citation") => set(&mut m.citation),
            ("extension", "serialTitle") => set(&mut m.serial_title),
            ("physicalDescription", "extent") => set(&mut m.extent),
            ("location", "url")
                if path.len() == 3
                    && attrs
                        .iter()
                        .any(|(k, v)| k == "displayLabel" && v == "PDF rendition") =>
            {
                m.pdf_rendition = true;
            }
            (_, "geographic")
                if path.iter().any(|p| p == "subject")
                    && !text.is_empty()
                    && !m.geographic.iter().any(|g| g == text) =>
            {
                m.geographic.push(text.to_owned());
            }
            _ => {}
        }
    })?;
    anyhow::ensure!(
        root.as_deref() == Some("mods"),
        "not a MODS record: the root is {root:?}"
    );
    m.date_issued = ext_date.or(w3c_date).or(any_date);
    Ok(m)
}

/// A MODS date and how precise it is: `1820-01-13` (`day`), `1820-01`
/// (`month`, on its first day), `1820` or `1990.` (`year`, on 1 January).
pub fn read_date(s: &str) -> Option<(NaiveDate, &'static str)> {
    let t = s.trim().trim_end_matches('.');
    let parts: Vec<&str> = t.split('-').collect();
    let num = |p: &str, n: usize| {
        (p.len() == n && p.bytes().all(|b| b.is_ascii_digit()))
            .then(|| p.parse::<u32>().ok())
            .flatten()
    };
    let year = i32::try_from(num(parts.first()?, 4)?).ok()?;
    if !(1789..=2100).contains(&year) {
        return None;
    }
    match parts.as_slice() {
        [_] => Some((NaiveDate::from_ymd_opt(year, 1, 1)?, "year")),
        [_, m] => Some((NaiveDate::from_ymd_opt(year, num(m, 2)?, 1)?, "month")),
        [_, m, d] => Some((
            NaiveDate::from_ymd_opt(year, num(m, 2)?, num(d, 2)?)?,
            "day",
        )),
        _ => None,
    }
}

/// A document's date (#229 design §5): its date, precision (`day`, `month`,
/// `year`) and where it came from (`date_issued`, or `session_year` when
/// MODS has no usable `dateIssued`). `None`: undated, left out.
pub fn date_of(m: &Mods) -> Option<(NaiveDate, &'static str, &'static str)> {
    if let Some((d, p)) = m.date_issued.as_deref().and_then(read_date) {
        return Some((d, p, "date_issued"));
    }
    let (d, _) = read_date(m.session_year.as_deref()?)?;
    Some((d, "year", "session_year"))
}

/// The state a MODS `geographic` subject names: `Texas`, `Missoula,
/// Montana`, `Gonzales County (Tex.)`, `Washington (State)`,
/// `Texas--Gonzales County`, `Washington (D.C.)`.
pub fn geographic_state(place: &str) -> Option<&'static usnm_core::names::State> {
    use usnm_core::names::{state_by_name, STATES};
    let place = place.trim().trim_end_matches('.');
    if let Some(s) = state_by_name(place) {
        return Some(s);
    }
    if let Some((before, rest)) = place.split_once('(') {
        let inner = rest.trim_end_matches(')').trim();
        if inner.eq_ignore_ascii_case("state") {
            return state_by_name(before);
        }
        if let Some(s) = state_by_name(inner) {
            return Some(s);
        }
        let dotted = |a: &str| {
            a.trim_end_matches('.')
                .eq_ignore_ascii_case(inner.trim_end_matches('.'))
        };
        if let Some(s) = STATES.iter().find(|s| s.abbrevs.iter().any(|a| dotted(a))) {
            return Some(s);
        }
    }
    if let Some((first, _)) = place.split_once("--") {
        if let Some(s) = state_by_name(first) {
            return Some(s);
        }
    }
    let (_, last) = place.rsplit_once(',')?;
    state_by_name(last)
}

/// The states a title names in full (`District of Columbia`, `Texas`), each
/// once, in order. `Washington` counts only as `Washington Territory` or
/// `State of Washington`, since it is the capital far more often. A name
/// inside a longer one (`Virginia` in `West Virginia`) isn't counted twice.
pub fn title_states(title: &str) -> Vec<(&'static str, &'static str)> {
    let words: Vec<String> = title
        .split(|c: char| !c.is_alphabetic())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect();
    let mut names: Vec<&'static usnm_core::names::State> =
        usnm_core::names::STATES.iter().collect();
    // Longest names first, so `West Virginia` claims its words before `Virginia`.
    names.sort_by_key(|s| std::cmp::Reverse(s.name.split(' ').count()));
    let mut used = vec![false; words.len()];
    let mut found: Vec<(usize, &'static str, &'static str)> = Vec::new();
    for s in names {
        let name: Vec<String> = s.name.split(' ').map(str::to_lowercase).collect();
        let n = name.len();
        for i in 0..words.len().saturating_sub(n - 1) {
            if used[i..i + n].iter().any(|u| *u) || words[i..i + n] != name[..] {
                continue;
            }
            if s.code == "WA" {
                let territory = words.get(i + 1).is_some_and(|w| w == "territory");
                let state_of = i >= 2 && words[i - 2] == "state" && words[i - 1] == "of";
                if !territory && !state_of {
                    continue;
                }
            }
            used[i..i + n].iter_mut().for_each(|u| *u = true);
            if !found.iter().any(|(_, c, _)| *c == s.code) {
                found.push((i, s.code, s.name));
            }
        }
    }
    found.sort_unstable();
    found.into_iter().map(|(_, c, n)| (c, n)).collect()
}

/// The states a document names (#229 design §6): its MODS `geographic`
/// subjects (`mods_geographic`, a cataloger's), then its title (`title`, a
/// heuristic), each state and way once.
pub fn states_of(m: &Mods) -> Vec<StateRef> {
    let mut out: Vec<StateRef> = Vec::new();
    let mut add = |state: &str, via: &str, value: &str| {
        if !out.iter().any(|s| s.state == state && s.via == via) {
            out.push(StateRef {
                state: state.to_owned(),
                via: via.to_owned(),
                value: Some(value.to_owned()),
            });
        }
    };
    for g in &m.geographic {
        if let Some(s) = geographic_state(g) {
            add(s.code, "mods_geographic", g);
        }
    }
    for (code, name) in title_states(m.title.as_deref().unwrap_or_default()) {
        add(code, "title", name);
    }
    out
}

/// One page's text: GovInfo's text layer marks a hyphen at a line end with
/// `¬` (`com¬` / `mercial`), read here as `-` so normalizing (04 §4.5)
/// joins the word.
pub fn page_text(raw: &str) -> String {
    let lines: Vec<String> = raw
        .split('\n')
        .map(|l| {
            let t = l.trim_end();
            match t.strip_suffix('¬') {
                Some(head) => format!("{head}-"),
                None => l.to_owned(),
            }
        })
        .collect();
    usnm_core::text::normalize_ocr(&lines.join("\n"))
}

/// The pages `pdftotext` wrote: it ends each page with a form feed.
pub fn split_pages(text: &str) -> Vec<String> {
    let mut pages: Vec<String> = text.split('\x0c').map(str::to_owned).collect();
    if pages.last().is_some_and(String::is_empty) {
        pages.pop();
    }
    pages
}

/// Reads a PDF's text, a string per page in order.
#[async_trait::async_trait]
pub trait PdfText: Send + Sync {
    async fn pages(&self, pdf: &Path) -> anyhow::Result<Vec<String>>;
}

/// Poppler's `pdftotext` (the ingest image's poppler-utils): the text layer
/// in reading order, one form feed after each page, split there.
pub struct Pdftotext {
    pub bin: PathBuf,
    /// A PDF that takes longer is given up on (and counted unreadable).
    pub timeout: Duration,
}

impl Pdftotext {
    pub fn new(bin: impl Into<PathBuf>) -> Self {
        Self {
            bin: bin.into(),
            timeout: Duration::from_secs(900),
        }
    }
}

#[async_trait::async_trait]
impl PdfText for Pdftotext {
    async fn pages(&self, pdf: &Path) -> anyhow::Result<Vec<String>> {
        let mut cmd = tokio::process::Command::new(&self.bin);
        cmd.arg("-enc")
            .arg("UTF-8")
            .arg(pdf)
            .arg("-")
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        let out = tokio::time::timeout(self.timeout, cmd.output())
            .await
            .with_context(|| format!("pdftotext ran past {:?}", self.timeout))?
            .with_context(|| format!("running `{}`", self.bin.display()))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            bail!(
                "pdftotext failed ({}): {}",
                out.status,
                err.trim().chars().take(500).collect::<String>()
            );
        }
        Ok(split_pages(&String::from_utf8_lossy(&out.stdout)))
    }
}

/// `https://www.govinfo.gov/content/pkg/{id}/pdf/{id}.pdf#page={seq}`: the
/// page in the document's PDF.
pub fn page_link(id: &str, seq: u32) -> String {
    format!("{}#page={seq}", pdf_url(GOVINFO, id))
}

fn pdf_url(base: &str, id: &str) -> String {
    format!("{base}/content/pkg/{id}/pdf/{id}.pdf")
}

fn mods_url(base: &str, id: &str) -> String {
    format!("{base}/metadata/pkg/{id}/mods.xml")
}

/// A `lastmod` as an object path segment: `2024-11-06T03:15:00.159Z` →
/// `20241106T031500159Z`.
fn stamp(lastmod: Option<&str>) -> String {
    lastmod
        .map(|l| {
            l.chars()
                .filter(char::is_ascii_alphanumeric)
                .collect::<String>()
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

/// Where a document's responses are kept in the raw store: under its
/// volume, its id and its `lastmod`, so a changed package is fetched again.
fn raw_dir(p: &Listed) -> String {
    format!(
        "congress/govinfo/{}/{}/{}",
        p.id.volume,
        p.id.id,
        stamp(p.lastmod.as_deref())
    )
}

/// The curated rows of one document's pages, or `None` when it has no date.
pub fn document_rows(
    id: &PackageId,
    m: &Mods,
    pages: &[String],
    fetched_at: DateTime<Utc>,
) -> Option<Vec<CuratedPage>> {
    let (date, precision, from) = date_of(m)?;
    let states = states_of(m);
    let chamber = m.chamber.as_deref().map(str::to_lowercase);
    let mut rows = Vec::with_capacity(pages.len());
    for (n, raw) in pages.iter().enumerate() {
        let seq = u32::try_from(n + 1).unwrap_or(u32::MAX);
        let text = page_text(raw);
        let status = usnm_core::text::text_status(&text);
        let ok = status == usnm_core::text::TextStatus::Ok;
        rows.push(CuratedPage {
            doc_id: format!("{}_p{seq}", id.id),
            source: SOURCE.into(),
            series: SERIES.into(),
            unit_id: id.volume.clone(),
            unit_ordinal: ordinal(id.serial, id.sequence()),
            package_id: Some(id.id.clone()),
            seq,
            printed_page: None,
            appendix: false,
            date,
            date_precision: precision.into(),
            date_from: from.into(),
            congress: m.congress,
            session: m.session.clone(),
            chamber: chamber.clone(),
            doc_class: m.doc_class.clone(),
            doc_number: m.document_number.clone(),
            citation: m.citation.clone(),
            serial: m.serial.clone().or_else(|| Some(id.serial.to_string())),
            title: m.title.clone(),
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
            link: Some(page_link(&id.id, seq)),
            states: states.clone(),
            places: m.geographic.clone(),
            speakers: Vec::new(),
            fetched_at,
        });
    }
    Some(rows)
}

/// A document package a sitemap lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub id: PackageId,
    pub lastmod: Option<String>,
}

/// One volume's documents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Volume {
    /// `00033_00_00`.
    pub volume: String,
    pub serial: u32,
    pub documents: Vec<Listed>,
}

impl Volume {
    /// The newest `lastmod` of its documents.
    pub fn lastmod(&self) -> Option<String> {
        self.documents
            .iter()
            .filter_map(|d| d.lastmod.clone())
            .max()
    }

    /// Which documents it has, each with its `lastmod`: the SHA-256 (hex)
    /// of `{id}\t{lastmod}\n` per document, by id. A document added,
    /// dropped or with any other `lastmod`, earlier or later, changes it.
    pub fn fingerprint(&self) -> String {
        let mut ids: Vec<(&str, &str)> = self
            .documents
            .iter()
            .map(|d| (d.id.id.as_str(), d.lastmod.as_deref().unwrap_or_default()))
            .collect();
        ids.sort_unstable();
        let mut h = Sha256::new();
        for (id, lastmod) in ids {
            h.update(id.as_bytes());
            h.update(b"\t");
            h.update(lastmod.as_bytes());
            h.update(b"\n");
        }
        crate::source::hex(&h.finalize())
    }
}

/// The listed packages, grouped.
#[derive(Debug, Default)]
pub struct Plan {
    /// By serial number, then volume; documents by id.
    pub volumes: Vec<Volume>,
    /// Whole-volume packages, not fetched (their documents are).
    pub volume_packages: usize,
    /// Ids that aren't Serial Set packages, or can't be object paths.
    pub bad_ids: Vec<String>,
}

/// The package id a sitemap entry names (`…/app/details/{id}`).
fn entry_id(e: &Entry) -> &str {
    e.loc
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default()
}

/// Group the sitemaps' packages by volume. A package listed twice keeps its
/// later `lastmod`.
pub fn plan(entries: Vec<Entry>) -> Plan {
    let mut p = Plan::default();
    let mut docs: BTreeMap<String, BTreeMap<String, Listed>> = BTreeMap::new();
    let mut volumes_seen = BTreeSet::new();
    for e in entries {
        let id = entry_id(&e);
        let Some(pkg) = PackageId::parse(id) else {
            p.bad_ids.push(id.to_owned());
            continue;
        };
        if pkg.document.is_none() {
            volumes_seen.insert(pkg.id);
            continue;
        }
        let listed = Listed {
            id: pkg,
            lastmod: e.lastmod,
        };
        let slot = docs.entry(listed.id.volume.clone()).or_default();
        match slot.get(&listed.id.id) {
            Some(old) if old.lastmod >= listed.lastmod => {}
            _ => {
                slot.insert(listed.id.id.clone(), listed);
            }
        }
    }
    p.volume_packages = volumes_seen.len();
    p.volumes = docs
        .into_iter()
        .map(|(volume, d)| {
            let documents: Vec<Listed> = d.into_values().collect();
            Volume {
                serial: documents[0].id.serial,
                volume,
                documents,
            }
        })
        .collect();
    p.volumes
        .sort_by(|a, b| (a.serial, &a.volume).cmp(&(b.serial, &b.volume)));
    p
}

/// What the command is asked to do.
#[derive(Debug, Clone)]
pub struct Options {
    /// Where to fetch from: GovInfo, or a copy of its responses (tests).
    pub base: String,
    /// Only the volumes with a package in these years' sitemaps; empty:
    /// all. Every sitemap is still read, and a volume is made of its
    /// packages in all of them.
    pub years: Vec<u16>,
    /// Only these serial numbers' volumes; empty: every volume listed.
    pub serials: Vec<u32>,
    /// Fetch volumes that already have a finished attempt again.
    pub refetch: bool,
    /// Stop starting volumes, and stop a volume between documents, at this time.
    pub deadline: Option<tokio::time::Instant>,
    /// Stop after this many volumes.
    pub max_volumes: Option<usize>,
    /// Where PDFs are streamed to, each deleted once read.
    pub scratch: PathBuf,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            base: GOVINFO.into(),
            years: Vec::new(),
            serials: Vec::new(),
            refetch: false,
            deadline: None,
            max_volumes: None,
            scratch: std::env::temp_dir(),
        }
    }
}

/// What a run did, printed as JSON at the end.
#[derive(Debug, Default, Serialize)]
pub struct Report {
    /// Yearly sitemaps read: all of them, even with `--years`.
    pub sitemaps: usize,
    /// Packages they list.
    pub listed: usize,
    /// Whole-volume packages, not fetched.
    pub volume_packages: usize,
    pub bad_ids: Vec<String>,
    /// Volumes with documents, and their documents (after `--years` and
    /// `--serials`).
    pub volumes: usize,
    pub documents: usize,
    /// Volumes with a finished (or empty) attempt of the same packages and
    /// `lastmod`s, not fetched again.
    pub already: usize,
    /// Volumes with a finished (or empty) attempt whose packages changed
    /// since (added, dropped or another `lastmod`), or that has no record
    /// of its packages (written before it was kept).
    pub changed: usize,
    /// Volumes written, with their attempt and pages.
    pub written: Vec<(String, u32, u64)>,
    pub documents_read: u64,
    pub pages: u64,
    /// Documents by how precisely they are dated.
    pub date_precision: BTreeMap<String, u64>,
    /// Documents GovInfo answered 404 for the MODS or a PDF its MODS names,
    /// with which: their volumes are left for the next run.
    pub missing: Vec<String>,
    /// Documents whose MODS or PDF couldn't be read, with why: their volumes
    /// are left for the next run.
    pub unreadable: Vec<String>,
    /// Volumes not written because a document of theirs is in `missing` or
    /// `unreadable`, with how many: fetched again by the next run.
    pub incomplete: Vec<String>,
    /// Documents whose MODS names no PDF and that have none (404): left out
    /// for good, as they have no text.
    pub no_pdf: Vec<String>,
    /// Documents without a usable date: left out for good (until GPO
    /// changes their MODS, and with it their `lastmod`).
    pub undated: Vec<String>,
    /// Volumes all of whose documents are left out for good (`no_pdf`,
    /// `undated`): an empty attempt (`curated::Empty`) is written in place
    /// of pages, so later runs skip them until their packages change.
    pub empty_volumes: Vec<String>,
    /// Volumes left for the next run: the deadline, `--max-volumes` or a lost lock.
    pub left: usize,
}

/// Read the sitemap index and every yearly sitemap, keeping each response in
/// the raw store: the entries, and the ids of the packages listed in the
/// years `--years` names (all, without it).
async fn list(
    fetch: &dyn Fetch,
    raw: Option<&dyn ObjectStore>,
    opts: &Options,
    now: DateTime<Utc>,
    report: &mut Report,
) -> anyhow::Result<(Vec<Entry>, BTreeSet<String>)> {
    let dir = format!("congress/govinfo/sitemaps/{}", now.format("%Y%m%dT%H%M%SZ"));
    let url = format!("{}{INDEX_PATH}", opts.base);
    let body = fetch
        .get(&url)
        .await?
        .with_context(|| format!("`{url}` answered 404"))?;
    if let Some(raw) = raw {
        fetch::keep(raw, &format!("{dir}/index.xml.zst"), &body).await?;
    }
    let mut sitemaps: Vec<(u16, Entry)> = Vec::new();
    for e in parse_index(&body).with_context(|| url.clone())? {
        match sitemap_year(&e.loc) {
            // Every year, even with `--years`: a volume's packages can be in
            // more than one sitemap, and a volume read from only some of
            // them would be written as finished without the others.
            Some(y) => sitemaps.push((y, e)),
            None => {
                tracing::warn!(loc = %e.loc, "a sitemap the index names without a year; skipped")
            }
        }
    }
    let unknown: Vec<&u16> = opts
        .years
        .iter()
        .filter(|y| !sitemaps.iter().any(|(s, _)| s == *y))
        .collect();
    if !unknown.is_empty() {
        bail!("the sitemap index has no sitemap for {unknown:?}");
    }
    sitemaps.sort_by_key(|(y, _)| *y);
    let mut out = Vec::new();
    let mut selected = BTreeSet::new();
    for (year, e) in sitemaps {
        let url = rebase(&opts.base, &e.loc);
        let body = fetch
            .get(&url)
            .await?
            .with_context(|| format!("`{url}` answered 404"))?;
        if let Some(raw) = raw {
            fetch::keep(raw, &format!("{dir}/{year}.xml.zst"), &body).await?;
        }
        let mut got = parse_urlset(&body).with_context(|| url.clone())?;
        tracing::info!(year, packages = got.len(), "Serial Set sitemap read");
        report.sitemaps += 1;
        if opts.years.is_empty() || opts.years.contains(&year) {
            selected.extend(got.iter().map(|e| entry_id(e).to_owned()));
        }
        out.append(&mut got);
    }
    report.listed = out.len();
    Ok((out, selected))
}

/// One document, read.
enum Got {
    Pages(Box<Mods>, Vec<String>),
    /// Its MODS names no PDF, and GovInfo has none (404): no text, for good.
    NoPdf,
    /// GovInfo answered 404 for its `mods`, or for a `pdf` its MODS names:
    /// taken as passing (the sitemap lists the package), so asked again.
    Missing(&'static str),
    /// Not read this time (MODS that isn't MODS, a PDF pdftotext can't
    /// read): asked again.
    Unreadable(String),
}

/// Fetch one document's MODS and page texts, from the raw store when it
/// holds them. A 404 is never kept, so the next run (or `--refetch`) asks
/// again.
async fn fetch_document(
    fetch: &dyn Fetch,
    pdf: &dyn PdfText,
    raw: Option<&dyn ObjectStore>,
    opts: &Options,
    doc: &Listed,
) -> anyhow::Result<Got> {
    let dir = raw_dir(doc);
    let id = &doc.id.id;
    let Some(body) = cached_found(
        fetch,
        raw,
        &format!("{dir}/mods.xml.zst"),
        &mods_url(&opts.base, id),
        |b| parse_mods(b).is_ok(),
    )
    .await?
    else {
        return Ok(Got::Missing("mods"));
    };
    let mods = match parse_mods(&body) {
        Ok(m) => m,
        Err(e) => return Ok(Got::Unreadable(format!("mods: {e:#}"))),
    };
    let pages_path = format!("{dir}/pages.json.zst");
    if let Some(raw) = raw {
        if let Some(kept) = fetch::read_kept(raw, &pages_path).await? {
            match serde_json::from_slice::<Vec<String>>(&kept) {
                Ok(p) if !p.is_empty() => return Ok(Got::Pages(Box::new(mods), p)),
                _ => {
                    tracing::warn!(path = %pages_path, "kept page texts aren't usable; reading the PDF again")
                }
            }
        }
    }
    // Streamed to scratch, read, and deleted when `file` drops.
    let file = tempfile::Builder::new()
        .prefix("serialset-")
        .suffix(".pdf")
        .tempfile_in(&opts.scratch)
        .with_context(|| format!("a scratch file in {}", opts.scratch.display()))?;
    let Some(bytes) = fetch
        .download(&pdf_url(&opts.base, id), file.path())
        .await?
    else {
        return Ok(if mods.pdf_rendition {
            Got::Missing("pdf")
        } else {
            Got::NoPdf
        });
    };
    let pages = match pdf.pages(file.path()).await {
        Ok(p) if !p.is_empty() => p,
        Ok(_) => return Ok(Got::Unreadable("pdf: no pages".into())),
        Err(e) => return Ok(Got::Unreadable(format!("pdf: {e:#}"))),
    };
    drop(file);
    tracing::debug!(id = %id, bytes, pages = pages.len(), "Serial Set PDF read");
    if let Some(raw) = raw {
        fetch::keep(raw, &pages_path, &serde_json::to_vec(&pages)?).await?;
    }
    Ok(Got::Pages(Box::new(mods), pages))
}

/// Why a volume stopped before its last document.
enum Stopped {
    Deadline,
    Lost,
}

fn past(deadline: Option<tokio::time::Instant>) -> bool {
    deadline.is_some_and(|d| tokio::time::Instant::now() >= d)
}

/// One volume's documents, read.
struct Read {
    rows: Vec<CuratedPage>,
    /// Documents not read this time (missing or unreadable): the volume
    /// isn't finished while there are any.
    unread: usize,
    /// Documents left out for good, with why (`{id}: undated`).
    left_out: Vec<String>,
}

/// Fetch and read one volume's documents.
async fn fetch_volume(
    fetch: &dyn Fetch,
    pdf: &dyn PdfText,
    raw: Option<&dyn ObjectStore>,
    opts: &Options,
    volume: &Volume,
    lost: &AtomicBool,
    report: &mut Report,
) -> anyhow::Result<Result<Read, Stopped>> {
    let mut rows = Vec::new();
    let mut unread = 0;
    let mut left_out = Vec::new();
    for doc in &volume.documents {
        if lost.load(Ordering::SeqCst) {
            return Ok(Err(Stopped::Lost));
        }
        if past(opts.deadline) {
            return Ok(Err(Stopped::Deadline));
        }
        let id = &doc.id.id;
        match fetch_document(fetch, pdf, raw, opts, doc).await? {
            Got::NoPdf => {
                tracing::warn!(id = %id, "a Serial Set document whose MODS names no PDF, and GovInfo has none; left out");
                report.no_pdf.push(id.clone());
                left_out.push(format!("{id}: no_pdf"));
            }
            Got::Missing(what) => {
                tracing::warn!(id = %id, what, "GovInfo answered 404 for a file it lists; the volume is left for the next run");
                report.missing.push(format!("{id}: {what}"));
                unread += 1;
            }
            Got::Unreadable(why) => {
                tracing::warn!(id = %id, why = %why, "a Serial Set document couldn't be read; the volume is left for the next run");
                report.unreadable.push(format!("{id}: {why}"));
                unread += 1;
            }
            Got::Pages(mods, pages) => match document_rows(&doc.id, &mods, &pages, Utc::now()) {
                Some(mut r) => {
                    if let Some(p) = r.first().map(|r| r.date_precision.clone()) {
                        *report.date_precision.entry(p).or_default() += 1;
                    }
                    report.documents_read += 1;
                    rows.append(&mut r);
                }
                None => {
                    tracing::warn!(id = %id, date = ?mods.date_issued, "a Serial Set document without a date; left out");
                    report.undated.push(id.clone());
                    left_out.push(format!("{id}: undated"));
                }
            },
        }
    }
    Ok(Ok(Read {
        rows,
        unread,
        left_out,
    }))
}

/// Run the fetch (steps 1 to 5). `lost` is set when the lock that keeps two
/// runs apart can no longer be renewed: the run stops between documents.
pub async fn run(
    fetch: &dyn Fetch,
    pdf: &dyn PdfText,
    curated: &dyn ObjectStore,
    raw: Option<&dyn ObjectStore>,
    opts: &Options,
    lost: &AtomicBool,
) -> anyhow::Result<Report> {
    let mut report = Report::default();
    let (entries, selected) = list(fetch, raw, opts, Utc::now(), &mut report).await?;
    let mut p = plan(entries);
    if !opts.years.is_empty() {
        // The volumes with a package in those years, each with its
        // packages in every sitemap.
        let volumes: BTreeSet<String> = selected
            .iter()
            .filter_map(|id| PackageId::parse(id))
            .map(|id| id.volume)
            .collect();
        p.volumes.retain(|v| volumes.contains(&v.volume));
    }
    report.volume_packages = p.volume_packages;
    report.bad_ids = p.bad_ids.clone();
    if !p.bad_ids.is_empty() {
        tracing::warn!(ids = ?p.bad_ids, "sitemap entries that aren't Serial Set packages; left out");
    }
    if !opts.serials.is_empty() {
        let unknown: Vec<&u32> = opts
            .serials
            .iter()
            .filter(|s| !p.volumes.iter().any(|v| v.serial == **s))
            .collect();
        if !unknown.is_empty() {
            bail!("no documents listed for serial numbers {unknown:?} (check --years)");
        }
        p.volumes.retain(|v| opts.serials.contains(&v.serial));
    }
    report.volumes = p.volumes.len();
    report.documents = p.volumes.iter().map(|v| v.documents.len()).sum();
    let finished: BTreeMap<String, curated::Finished> = curated::finished(curated)
        .await?
        .into_iter()
        .filter(|f| f.series == SERIES)
        .map(|f| (f.unit.clone(), f))
        .collect();
    // A volume whose documents were all left out has an empty attempt:
    // the newer of it and the finished one says which packages were read.
    let empties: BTreeMap<String, curated::Empty> = curated::empties(curated)
        .await?
        .into_iter()
        .filter(|((series, _), _)| series == SERIES)
        .map(|((_, unit), e)| (unit, e))
        .collect();
    let mut todo: Vec<&Volume> = Vec::new();
    for v in &p.volumes {
        let recorded = match (finished.get(&v.volume), empties.get(&v.volume)) {
            (Some(f), e) if e.is_none_or(|e| e.attempt < f.attempt) => {
                Some(curated::read_counts(curated, f).await?.0.source_packages)
            }
            (_, Some(e)) => Some(e.source_packages.clone()),
            (_, None) => None,
        };
        match recorded {
            None => todo.push(v),
            Some(_) if opts.refetch => todo.push(v),
            Some(Some(packages)) if packages == v.fingerprint() => report.already += 1,
            // Packages added, dropped or with another `lastmod`, earlier
            // or later; or an attempt from before the packages were
            // recorded, which can't tell: fetched again.
            Some(_) => {
                report.changed += 1;
                todo.push(v);
            }
        }
    }
    tracing::info!(
        sitemaps = report.sitemaps,
        listed = report.listed,
        volumes = report.volumes,
        documents = report.documents,
        volume_packages = report.volume_packages,
        finished = finished.len(),
        empty = empties.len(),
        changed = report.changed,
        to_fetch = todo.len(),
        "Serial Set volumes"
    );
    for (n, v) in todo.iter().enumerate() {
        if opts.max_volumes.is_some_and(|m| n >= m)
            || past(opts.deadline)
            || lost.load(Ordering::SeqCst)
        {
            report.left = todo.len() - n;
            break;
        }
        tracing::info!(volume = %v.volume, documents = v.documents.len(), "fetching a Serial Set volume");
        match fetch_volume(fetch, pdf, raw, opts, v, lost, &mut report).await? {
            // Lost during the last document: nothing to see it between documents.
            Ok(_) if lost.load(Ordering::SeqCst) => {
                report.left = todo.len() - n;
                tracing::warn!(volume = %v.volume, "the fetch lock was lost; stopping");
                break;
            }
            // A finished attempt records the volume's packages, and a
            // later run skips it: written only when every document is read
            // or left out for good.
            Ok(read) if read.unread > 0 => {
                tracing::warn!(
                    volume = %v.volume, unread = read.unread, documents = v.documents.len(),
                    "documents of the volume weren't read; nothing written, and the next run fetches it again"
                );
                report.incomplete.push(format!(
                    "{}: {} of {}",
                    v.volume,
                    read.unread,
                    v.documents.len()
                ));
            }
            // Recorded as an empty attempt, so later runs skip the volume
            // until its packages change (or `--refetch`), and the release
            // drops an earlier attempt of it.
            Ok(read) if read.rows.is_empty() => {
                let empty = curated::write_empty(
                    curated,
                    SERIES,
                    &v.volume,
                    Utc::now(),
                    v.lastmod(),
                    Some(v.fingerprint()),
                    read.left_out,
                )
                .await?;
                tracing::warn!(
                    volume = %v.volume, attempt = empty.attempt,
                    "every document of the volume is left out; an empty attempt written"
                );
                report.empty_volumes.push(v.volume.clone());
            }
            Ok(Read { rows, .. }) => {
                let counts = curated::write_attempt_from(
                    curated,
                    SERIES,
                    SOURCE,
                    &v.volume,
                    &rows,
                    Utc::now(),
                    v.lastmod(),
                    Some(v.fingerprint()),
                )
                .await?;
                tracing::info!(
                    volume = %v.volume, attempt = counts.attempt, pages = counts.pages,
                    indexable = counts.indexable, "Serial Set volume curated"
                );
                report.pages += counts.pages;
                report
                    .written
                    .push((v.volume.clone(), counts.attempt, counts.pages));
            }
            Err(why) => {
                report.left = todo.len() - n;
                match why {
                    Stopped::Deadline => {
                        tracing::info!(volume = %v.volume, "deadline reached mid-volume; the next run resumes it")
                    }
                    Stopped::Lost => {
                        tracing::warn!(volume = %v.volume, "the fetch lock was lost; stopping")
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
    fetch::locked(state, LOCK, owner, body, lost).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/congress/govinfo")
    }

    fn recorded(name: &str) -> Vec<u8> {
        std::fs::read(dir().join(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
    }

    const BASE: &str = "https://govinfo.example";

    /// GovInfo as recorded in `fixtures/congress/govinfo/`: what the fetch
    /// asks for, by URL. A PDF is a placeholder naming its document, whose
    /// page texts [`RecordedText`] reads; a package with no recorded file
    /// is a 404.
    #[derive(Default)]
    struct Recorded {
        asked: std::sync::Mutex<Vec<String>>,
        /// Answer only the sitemaps: everything else must come from the raw store.
        sitemaps_only: bool,
        /// A `lastmod` to give one package in the 1820 sitemap.
        touched: Option<(&'static str, &'static str)>,
    }

    impl Recorded {
        fn asked(&self, part: &str) -> usize {
            self.asked
                .lock()
                .unwrap()
                .iter()
                .filter(|u| u.contains(part))
                .count()
        }
    }

    #[async_trait::async_trait]
    impl Fetch for Recorded {
        async fn get(&self, url: &str) -> anyhow::Result<Option<Vec<u8>>> {
            self.asked.lock().unwrap().push(url.to_owned());
            let path = url.strip_prefix(BASE).expect("only the recorded host");
            if let Some(name) = path.strip_prefix("/sitemap/") {
                let Ok(body) = std::fs::read(dir().join(name)) else {
                    return Ok(None);
                };
                return Ok(Some(match self.touched {
                    Some((id, lastmod)) if name.contains("1820") => {
                        let s = String::from_utf8(body).unwrap();
                        let at = s.find(id).unwrap();
                        let open = at + s[at..].find("<lastmod>").unwrap() + "<lastmod>".len();
                        let close = open + s[open..].find('<').unwrap();
                        format!("{}{lastmod}{}", &s[..open], &s[close..]).into_bytes()
                    }
                    _ => body,
                }));
            }
            assert!(!self.sitemaps_only, "asked GovInfo for {url}");
            if let Some(rest) = path.strip_prefix("/metadata/pkg/") {
                let id = rest.strip_suffix("/mods.xml").expect("a MODS url");
                return Ok(std::fs::read(dir().join(format!("{id}-mods.xml"))).ok());
            }
            if let Some(rest) = path.strip_prefix("/content/pkg/") {
                let (id, file) = rest.split_once("/pdf/").expect("a PDF url");
                assert_eq!(file, format!("{id}.pdf"));
                let exists = dir().join(format!("{id}-pages.txt")).exists();
                return Ok(exists.then(|| format!("%PDF-fixture {id}").into_bytes()));
            }
            panic!("unexpected request {url}")
        }
    }

    /// `pdftotext` on a placeholder PDF: the recorded page texts of the
    /// document it names, as pdftotext wrote them (form feeds included).
    #[derive(Default)]
    struct RecordedText(std::sync::atomic::AtomicUsize);

    #[async_trait::async_trait]
    impl PdfText for RecordedText {
        async fn pages(&self, pdf: &Path) -> anyhow::Result<Vec<String>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            let body = std::fs::read_to_string(pdf)?;
            let id = body
                .strip_prefix("%PDF-fixture ")
                .context("not a placeholder PDF")?;
            Ok(split_pages(&String::from_utf8(recorded(&format!(
                "{id}-pages.txt"
            )))?))
        }
    }

    #[test]
    fn package_ids_name_their_volume_and_place() {
        let p = PackageId::parse("SERIALSET-00033_00_00-009-0042-0000").unwrap();
        assert_eq!(
            (
                p.volume.as_str(),
                p.serial,
                p.document.as_deref(),
                p.sequence()
            ),
            ("00033_00_00", 33, Some("009-0042-0000"), Some(9))
        );
        let v = PackageId::parse("SERIALSET-00033_00_00").unwrap();
        assert_eq!((v.document.as_deref(), v.sequence()), (None, None));
        // The one malformed id GovInfo lists still names its volume.
        let odd = PackageId::parse("SERIALSET-01622_00_00-001-300-0000").unwrap();
        assert_eq!((odd.serial, odd.sequence()), (1622, Some(1)));
        for bad in [
            "CRPT-118hrpt100",
            "SERIALSET-33_00_00",
            "SERIALSET-00033_00",
            "SERIALSET-00033_00_00-",
            "SERIALSET-00033_00_00-0/9",
            "../SERIALSET-00033_00_00",
        ] {
            assert!(PackageId::parse(bad).is_none(), "{bad}");
        }
        assert!(ordinal(33, Some(9)) < ordinal(33, Some(20)));
        assert!(ordinal(33, Some(4095)) < ordinal(34, Some(1)));
        assert!(ordinal(14_277, Some(300)) > ordinal(13_993, Some(21)));
    }

    #[test]
    fn the_recorded_sitemaps_list_packages_with_lastmod() {
        let index = parse_index(&recorded("SERIALSET_sitemap_index.xml")).unwrap();
        let years: Vec<Option<u16>> = index.iter().map(|e| sitemap_year(&e.loc)).collect();
        assert_eq!(years, [Some(1820), Some(1990)]);
        assert_eq!(
            index[0].lastmod.as_deref(),
            Some("2026-08-05T16:04:00.039Z")
        );
        assert_eq!(
            rebase(BASE, &index[0].loc),
            format!("{BASE}/sitemap/SERIALSET_1820_sitemap.xml")
        );
        let urls = parse_urlset(&recorded("SERIALSET_1820_sitemap.xml")).unwrap();
        assert_eq!(urls.len(), 5);
        assert_eq!(
            urls[2],
            Entry {
                loc: "https://www.govinfo.gov/app/details/SERIALSET-00033_00_00-009-0042-0000"
                    .into(),
                lastmod: Some("2024-11-06T03:15:00.159Z".into()),
            }
        );
        let p = plan(urls);
        assert_eq!(p.volume_packages, 2);
        let vols: Vec<(&str, usize)> = p
            .volumes
            .iter()
            .map(|v| (v.volume.as_str(), v.documents.len()))
            .collect();
        assert_eq!(vols, [("00025_00_00", 1), ("00033_00_00", 2)]);
        assert_eq!(
            p.volumes[1].lastmod().as_deref(),
            Some("2024-11-06T03:15:00.159Z")
        );
        // Not a sitemap: refused, so a maintenance page isn't an empty year.
        assert!(parse_urlset(b"<html><body>down</body></html>").is_err());
        assert!(parse_index(&recorded("SERIALSET_1820_sitemap.xml")).is_err());
        assert!(parse_urlset(b"<urlset/>").unwrap().is_empty());
    }

    #[test]
    fn mods_gives_dates_citations_and_places() {
        let m = parse_mods(&recorded("SERIALSET-00033_00_00-009-0042-0000-mods.xml")).unwrap();
        assert_eq!(m.date_issued.as_deref(), Some("1820-01-13"));
        assert_eq!(
            (
                m.doc_class.as_deref(),
                m.document_number.as_deref(),
                m.serial.as_deref(),
                m.sequence.as_deref(),
                m.congress,
                m.session.as_deref(),
                m.chamber.as_deref(),
                m.citation.as_deref(),
                m.extent.as_deref(),
            ),
            (
                Some("HDOC"),
                Some("42"),
                Some("33"),
                Some("9"),
                Some(16),
                Some("1"),
                Some("HOUSE"),
                Some("H. Doc. 16-42"),
                Some("12 p.")
            )
        );
        assert!(m
            .title
            .as_deref()
            .unwrap()
            .starts_with("Letter from the Secretary of the Treasury"));
        assert_eq!(m.geographic, ["Spain"]);
        assert!(m.pdf_rendition);
        let m = parse_mods(&recorded("SERIALSET-13993_00_00-021-0183-0000-mods.xml")).unwrap();
        assert_eq!(
            (m.date_issued.as_deref(), m.congress, m.session.as_deref()),
            (Some("1990-05-01"), Some(101), Some("2"))
        );
        assert!(m.geographic.is_empty());
        assert!(parse_mods(b"<html><body>error</body></html>").is_err());
    }

    #[test]
    fn dates_carry_their_precision() {
        let d = |y, m, dd| NaiveDate::from_ymd_opt(y, m, dd).unwrap();
        assert_eq!(read_date("1820-01-13"), Some((d(1820, 1, 13), "day")));
        assert_eq!(read_date("1820-01"), Some((d(1820, 1, 1), "month")));
        assert_eq!(read_date("1990."), Some((d(1990, 1, 1), "year")));
        for bad in [
            "",
            "18",
            "1820-13",
            "1820-02-30",
            "c1820",
            "1700",
            "1820-1-3",
        ] {
            assert_eq!(read_date(bad), None, "{bad}");
        }
        let mut m = Mods {
            date_issued: Some("1820".into()),
            session_year: Some("1821".into()),
            ..Mods::default()
        };
        assert_eq!(date_of(&m), Some((d(1820, 1, 1), "year", "date_issued")));
        m.date_issued = Some("undated".into());
        assert_eq!(date_of(&m), Some((d(1821, 1, 1), "year", "session_year")));
        m.session_year = None;
        assert_eq!(date_of(&m), None);
    }

    #[test]
    fn states_from_subjects_and_titles() {
        let code = |p: &str| geographic_state(p).map(|s| s.code);
        assert_eq!(code("Texas"), Some("TX"));
        assert_eq!(code("Missoula, Montana"), Some("MT"));
        assert_eq!(code("Gonzales County (Tex.)"), Some("TX"));
        assert_eq!(code("Washington (State)"), Some("WA"));
        assert_eq!(code("Washington (D.C.)"), Some("DC"));
        assert_eq!(code("Texas--Gonzales County"), Some("TX"));
        assert_eq!(code("United States"), None);
        assert_eq!(code("Spain"), None);
        assert_eq!(code("Mississippi River"), None);
        let t = |s: &str| {
            title_states(s)
                .into_iter()
                .map(|(c, _)| c)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            t("Memorial of citizens of West Virginia and of Virginia, praying relief"),
            ["WV", "VA"]
        );
        assert_eq!(
            t("Letter on the public buildings at Washington"),
            Vec::<&str>::new()
        );
        assert_eq!(
            t("Roads in Washington Territory and the District of Columbia"),
            ["WA", "DC"]
        );
        assert_eq!(t("Lands in Arkansas"), ["AR"], "not Kansas");
        let m = Mods {
            title: Some("Claims of settlers in Texas".into()),
            geographic: vec!["Gonzales County (Tex.)".into(), "Spain".into()],
            ..Mods::default()
        };
        let s = states_of(&m);
        assert_eq!(
            s.iter()
                .map(|s| (s.state.as_str(), s.via.as_str(), s.value.as_deref()))
                .collect::<Vec<_>>(),
            [
                ("TX", "mods_geographic", Some("Gonzales County (Tex.)")),
                ("TX", "title", Some("Texas"))
            ]
        );
    }

    #[test]
    fn line_end_hyphens_are_read_from_the_not_sign() {
        assert_eq!(
            page_text("information in rela¬\ntion to the illicit\ncom¬  \nmercial trade"),
            "information in relation to the illicit\ncommercial trade"
        );
        // A `¬` elsewhere is left as printed.
        assert_eq!(page_text("a ¬ b"), "a ¬ b");
        assert_eq!(split_pages("one\x0ctwo\x0c"), ["one", "two"]);
        assert_eq!(split_pages("one\x0c\x0cthree\x0c"), ["one", "", "three"]);
        assert!(split_pages("").is_empty());
    }

    fn options(scratch: &Path) -> Options {
        Options {
            base: BASE.into(),
            years: vec![1820],
            scratch: scratch.to_owned(),
            ..Options::default()
        }
    }

    #[tokio::test]
    async fn fetches_a_volume_from_recorded_responses() {
        let dir = tempfile::tempdir().unwrap();
        let curated = usnm_store::LocalStore::new(dir.path().join("curated"));
        let raw = usnm_store::LocalStore::new(dir.path().join("raw"));
        let scratch = dir.path().join("scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        let gi = Recorded::default();
        let text = RecordedText::default();
        let lost = AtomicBool::new(false);
        let opts = options(&scratch);
        let report = run(&gi, &text, &curated, Some(&raw), &opts, &lost)
            .await
            .unwrap();
        // Both sitemaps are read; `--years 1820` picks the volumes.
        assert_eq!((report.sitemaps, report.listed), (2, 7));
        assert_eq!(
            (report.volume_packages, report.volumes, report.documents),
            (3, 2, 3)
        );
        // Serial 25's one document has no recorded MODS: GovInfo's 404.
        assert_eq!(
            report.missing,
            ["SERIALSET-00025_00_00-001-0000-0000: mods"]
        );
        // A 404 for a package the sitemap lists: not finished, fetched again.
        assert_eq!(report.incomplete, ["00025_00_00: 1 of 1"]);
        assert!(report.empty_volumes.is_empty());
        assert_eq!(report.written, [("00033_00_00".to_owned(), 1, 6)]);
        assert_eq!(report.documents_read, 2);
        assert_eq!(report.date_precision["day"], 2);
        // The PDFs were deleted once read.
        assert_eq!(std::fs::read_dir(&scratch).unwrap().count(), 0);
        assert_eq!(gi.asked("/content/pkg/"), 2);
        // The index, two sitemaps, three MODS (one a 404) and two PDFs.
        assert_eq!(gi.asked.lock().unwrap().len(), 8);

        let f = curated::finished(&curated).await.unwrap();
        assert_eq!(f.len(), 1);
        let (counts, _) = curated::read_counts(&curated, &f[0]).await.unwrap();
        assert_eq!(counts.series, SERIES);
        assert_eq!(
            counts.source_lastmod.as_deref(),
            Some("2024-11-06T03:15:00.159Z")
        );
        assert_eq!(counts.text_status["ok"], 4);
        let rows = curated::read_checked(&curated, &counts.parts[0])
            .await
            .unwrap();
        let by_id: BTreeMap<&str, &CuratedPage> =
            rows.iter().map(|r| (r.doc_id.as_str(), r)).collect();
        let p3 = by_id["SERIALSET-00033_00_00-009-0042-0000_p3"];
        assert_eq!(p3.date, NaiveDate::from_ymd_opt(1820, 1, 13).unwrap());
        assert_eq!(
            (
                p3.date_precision.as_str(),
                p3.date_from.as_str(),
                p3.chamber.as_deref(),
                p3.doc_class.as_deref(),
                p3.doc_number.as_deref(),
                p3.citation.as_deref(),
                p3.serial.as_deref(),
                p3.congress,
                p3.session.as_deref(),
            ),
            (
                "day",
                "date_issued",
                Some("house"),
                Some("HDOC"),
                Some("42"),
                Some("H. Doc. 16-42"),
                Some("33"),
                Some(16),
                Some("1")
            )
        );
        assert_eq!(
            p3.link.as_deref(),
            Some(
                "https://www.govinfo.gov/content/pkg/SERIALSET-00033_00_00-009-0042-0000/pdf/\
                 SERIALSET-00033_00_00-009-0042-0000.pdf#page=3"
            )
        );
        assert_eq!(p3.unit_id, "00033_00_00");
        assert_eq!(p3.unit_ordinal, ordinal(33, Some(9)));
        assert_eq!(p3.places, ["Spain"]);
        assert!(p3.states.is_empty());
        let body = p3.text.as_deref().unwrap();
        assert!(
            body.contains("in relation to the illicit introduction"),
            "{body}"
        );
        assert!(!body.contains('¬'));
        // A page with a stray mark is short; a blank one empty: counted, not indexed.
        assert_eq!(
            by_id["SERIALSET-00033_00_00-009-0042-0000_p2"].text_status,
            "short"
        );
        let p2 = by_id["SERIALSET-00033_00_00-020-0053-0000_p2"];
        assert_eq!(
            (p2.text_status.as_str(), p2.text.as_deref()),
            ("empty", None)
        );
        assert_eq!(p2.date, NaiveDate::from_ymd_opt(1820, 1, 20).unwrap());
        assert!(by_id["SERIALSET-00033_00_00-020-0053-0000_p1"]
            .text
            .as_deref()
            .unwrap()
            .contains("ESTIMATED APPROPRIATIONS"));
        assert_eq!(counts.days.len(), 2);

        // Finished and unchanged: not fetched again.
        let again = Recorded::default();
        let r = run(&again, &text, &curated, Some(&raw), &opts, &lost)
            .await
            .unwrap();
        assert_eq!((r.already, r.changed, r.written.len()), (1, 0, 0));
        assert_eq!(again.asked("/content/pkg/"), 0);
        // Serial 25, unfinished, is asked for again: its 404 wasn't kept.
        assert_eq!(again.asked("/metadata/pkg/SERIALSET-00025"), 1);
        assert_eq!(r.incomplete, ["00025_00_00: 1 of 1"]);

        // Fetched again on request, from the raw store's copies alone.
        let kept = Recorded {
            sitemaps_only: true,
            ..Recorded::default()
        };
        let refetch = Options {
            refetch: true,
            serials: vec![33],
            ..opts.clone()
        };
        let r = run(&kept, &text, &curated, Some(&raw), &refetch, &lost)
            .await
            .unwrap();
        assert_eq!(r.written, [("00033_00_00".to_owned(), 2, 6)]);
        assert_eq!(text.0.load(Ordering::SeqCst), 2, "no PDF read again");

        // GovInfo changes one document: its volume is fetched again, that
        // document from GovInfo (a new lastmod) and the other from the raw store.
        let changed = Recorded {
            touched: Some((
                "SERIALSET-00033_00_00-020-0053-0000",
                "2026-10-01T00:00:00.000Z",
            )),
            ..Recorded::default()
        };
        let r = run(&changed, &text, &curated, Some(&raw), &opts, &lost)
            .await
            .unwrap();
        assert_eq!((r.changed, r.already), (1, 0));
        assert_eq!(r.written, [("00033_00_00".to_owned(), 3, 6)]);
        assert_eq!(changed.asked("/content/pkg/"), 1);
        let f = curated::finished(&curated).await.unwrap();
        let (c3, _) = curated::read_counts(&curated, &f[0]).await.unwrap();
        assert_eq!(
            c3.source_lastmod.as_deref(),
            Some("2026-10-01T00:00:00.000Z")
        );
    }

    #[tokio::test]
    async fn stops_at_the_deadline_and_refuses_unknown_serials() {
        let dir = tempfile::tempdir().unwrap();
        let curated = usnm_store::LocalStore::new(dir.path());
        let lost = AtomicBool::new(false);
        let past = Options {
            deadline: Some(tokio::time::Instant::now()),
            ..options(dir.path())
        };
        let text = RecordedText::default();
        let r = run(&Recorded::default(), &text, &curated, None, &past, &lost)
            .await
            .unwrap();
        assert_eq!((r.written.len(), r.left), (0, 2));
        assert!(curated::finished(&curated).await.unwrap().is_empty());
        let unknown = Options {
            serials: vec![13993],
            ..options(dir.path())
        };
        let err = run(&Recorded::default(), &text, &curated, None, &unknown, &lost)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("13993"), "{err}");
        let no_year = Options {
            years: vec![1821],
            ..options(dir.path())
        };
        assert!(
            run(&Recorded::default(), &text, &curated, None, &no_year, &lost)
                .await
                .is_err()
        );
        // --max-volumes 1 writes nothing for serial 25 (incomplete), and leaves 33.
        let one = Options {
            max_volumes: Some(1),
            ..options(dir.path())
        };
        let r = run(&Recorded::default(), &text, &curated, None, &one, &lost)
            .await
            .unwrap();
        assert_eq!((r.incomplete.len(), r.written.len(), r.left), (1, 0, 1));
        // The 1990 volume, on its own.
        let later = Options {
            years: vec![1990],
            ..options(dir.path())
        };
        let r = run(&Recorded::default(), &text, &curated, None, &later, &lost)
            .await
            .unwrap();
        assert_eq!(r.written, [("13993_00_00".to_owned(), 1, 3)]);
    }

    /// The recorded responses, losing the lock while the last PDF is asked for.
    struct LosesOnLastPdf<'a>(Recorded, &'a AtomicBool);

    #[async_trait::async_trait]
    impl Fetch for LosesOnLastPdf<'_> {
        async fn get(&self, url: &str) -> anyhow::Result<Option<Vec<u8>>> {
            if url.ends_with("0053-0000.pdf") {
                self.1.store(true, Ordering::SeqCst);
            }
            self.0.get(url).await
        }
    }

    #[tokio::test]
    async fn a_lock_lost_on_the_last_document_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let curated = usnm_store::LocalStore::new(dir.path());
        let lost = AtomicBool::new(false);
        let gi = LosesOnLastPdf(Recorded::default(), &lost);
        let opts = Options {
            serials: vec![33],
            ..options(dir.path())
        };
        let text = RecordedText::default();
        assert!(run(&gi, &text, &curated, None, &opts, &lost).await.is_err());
        assert!(lost.load(Ordering::SeqCst), "the last PDF was asked for");
        assert!(curated::finished(&curated).await.unwrap().is_empty());
    }

    /// The recorded responses, answering 404 the first time a URL with one
    /// of `fail_once` in it is asked for, and with the documents in `no_pdf`
    /// having no PDF (404), which their MODS doesn't name.
    #[derive(Default)]
    struct Flaky {
        inner: Recorded,
        fail_once: std::sync::Mutex<Vec<String>>,
        no_pdf: Vec<&'static str>,
    }

    #[async_trait::async_trait]
    impl Fetch for Flaky {
        async fn get(&self, url: &str) -> anyhow::Result<Option<Vec<u8>>> {
            let failed = {
                let mut fail = self.fail_once.lock().unwrap();
                let at = fail.iter().position(|p| url.contains(p.as_str()));
                at.map(|i| fail.remove(i)).is_some()
            };
            let no_pdf = self.no_pdf.iter().any(|id| url.contains(id));
            if failed || (no_pdf && url.ends_with(".pdf")) {
                self.inner.asked.lock().unwrap().push(url.to_owned());
                return Ok(None);
            }
            let body = self.inner.get(url).await?;
            if no_pdf && url.ends_with("mods.xml") {
                let s = String::from_utf8(body.unwrap()).unwrap();
                return Ok(Some(
                    s.replace("PDF rendition", "Other rendition").into_bytes(),
                ));
            }
            Ok(body)
        }
    }

    const D42: &str = "SERIALSET-00033_00_00-009-0042-0000";
    const D53: &str = "SERIALSET-00033_00_00-020-0053-0000";

    /// A 404 for a document the sitemap lists (its MODS, or a PDF its MODS
    /// names) leaves the volume unfinished; the next run fetches it again
    /// and finishes it, reading what was read before from the raw store.
    #[tokio::test]
    async fn a_passing_404_leaves_the_volume_for_the_next_run() {
        let dir = tempfile::tempdir().unwrap();
        let lost = AtomicBool::new(false);
        let text = RecordedText::default();
        let opts = Options {
            serials: vec![33],
            ..options(dir.path())
        };
        for (i, fail) in [format!("{D42}/mods.xml"), format!("{D53}.pdf")]
            .into_iter()
            .enumerate()
        {
            let fail = fail.as_str();
            let curated = usnm_store::LocalStore::new(dir.path().join(format!("curated-{i}")));
            let raw = usnm_store::LocalStore::new(dir.path().join(format!("raw-{i}")));
            let gi = Flaky {
                fail_once: std::sync::Mutex::new(vec![fail.to_owned()]),
                ..Flaky::default()
            };
            let r = run(&gi, &text, &curated, Some(&raw), &opts, &lost)
                .await
                .unwrap();
            assert_eq!(r.incomplete, ["00033_00_00: 1 of 2"], "{fail}");
            assert_eq!(r.missing.len(), 1, "{fail}");
            assert!(r.written.is_empty());
            assert!(
                curated::finished(&curated).await.unwrap().is_empty(),
                "{fail}"
            );
            // The next run asks again, and finishes the volume.
            let r = run(&gi, &text, &curated, Some(&raw), &opts, &lost)
                .await
                .unwrap();
            assert!(r.incomplete.is_empty() && r.missing.is_empty(), "{fail}");
            assert_eq!(r.written, [("00033_00_00".to_owned(), 1, 6)], "{fail}");
            assert_eq!(gi.inner.asked(fail), 2, "{fail}");
            // Each PDF was read once: the second run read the first's pages back.
            assert_eq!(text.0.load(Ordering::SeqCst), 2 * (i + 1), "{fail}");
        }
        // Without a raw store too.
        let curated = usnm_store::LocalStore::new(dir.path().join("curated"));
        let gi = Flaky {
            fail_once: std::sync::Mutex::new(vec!["0042-0000.pdf".into()]),
            ..Flaky::default()
        };
        let r = run(&gi, &text, &curated, None, &opts, &lost).await.unwrap();
        assert_eq!(r.incomplete, ["00033_00_00: 1 of 2"]);
        let r = run(&gi, &text, &curated, None, &opts, &lost).await.unwrap();
        assert_eq!(r.written, [("00033_00_00".to_owned(), 1, 6)]);
    }

    /// `--refetch` asks again for a MODS whose 404 an earlier run kept.
    #[tokio::test]
    async fn refetch_asks_again_for_a_kept_404() {
        let dir = tempfile::tempdir().unwrap();
        let curated = usnm_store::LocalStore::new(dir.path().join("curated"));
        let raw = usnm_store::LocalStore::new(dir.path().join("raw"));
        let lost = AtomicBool::new(false);
        let text = RecordedText::default();
        let opts = Options {
            serials: vec![33],
            ..options(dir.path())
        };
        let r = run(&Recorded::default(), &text, &curated, None, &opts, &lost)
            .await
            .unwrap();
        assert_eq!(r.written.len(), 1);
        // A 404 kept for D42's MODS, as the first version of the fetch did.
        let urls = parse_urlset(&recorded("SERIALSET_1820_sitemap.xml")).unwrap();
        let p = plan(urls);
        let d42 = p.volumes[1]
            .documents
            .iter()
            .find(|d| d.id.id == D42)
            .unwrap();
        fetch::keep(
            &raw,
            &format!("{}/mods.xml.zst", raw_dir(d42)),
            fetch::RAW_MISSING,
        )
        .await
        .unwrap();
        let gi = Recorded::default();
        let refetch = Options {
            refetch: true,
            ..opts.clone()
        };
        let r = run(&gi, &text, &curated, Some(&raw), &refetch, &lost)
            .await
            .unwrap();
        assert_eq!(gi.asked(&format!("{D42}/mods.xml")), 1);
        assert!(r.missing.is_empty() && r.incomplete.is_empty());
        assert_eq!(r.written, [("00033_00_00".to_owned(), 2, 6)]);
    }

    /// A document whose MODS names no PDF, and that has none, is left out
    /// for good: the volume is finished without it.
    #[tokio::test]
    async fn a_document_without_a_pdf_is_left_out_for_good() {
        let dir = tempfile::tempdir().unwrap();
        let curated = usnm_store::LocalStore::new(dir.path());
        let lost = AtomicBool::new(false);
        let text = RecordedText::default();
        let opts = Options {
            serials: vec![33],
            ..options(dir.path())
        };
        let gi = Flaky {
            no_pdf: vec![D53],
            ..Flaky::default()
        };
        let r = run(&gi, &text, &curated, None, &opts, &lost).await.unwrap();
        assert_eq!(r.no_pdf, [D53]);
        assert!(r.incomplete.is_empty() && r.missing.is_empty());
        assert_eq!(r.written.len(), 1);
        assert_eq!(r.documents_read, 1);
        let f = curated::finished(&curated).await.unwrap();
        let (counts, _) = curated::read_counts(&curated, &f[0]).await.unwrap();
        assert!(counts.pages < 6, "{}", counts.pages);
    }

    /// A volume all of whose documents are left out for good gets an empty
    /// attempt: the next run skips it (no MODS or PDF asked for) and the
    /// release lists nothing for it; a later `lastmod` or `--refetch` fetches
    /// it again, and an empty attempt after a finished one takes the volume
    /// out of the release.
    #[tokio::test]
    async fn an_all_left_out_volume_is_finished_empty() {
        use super::super::release::Listing;
        let dir = tempfile::tempdir().unwrap();
        let curated = usnm_store::LocalStore::new(dir.path().join("curated"));
        let lost = AtomicBool::new(false);
        let text = RecordedText::default();
        let opts = Options {
            serials: vec![33],
            ..options(dir.path())
        };
        let key = (SERIES.to_owned(), "00033_00_00".to_owned());
        let none = || Flaky {
            no_pdf: vec![D42, D53],
            ..Flaky::default()
        };
        let gi = none();
        let r = run(&gi, &text, &curated, None, &opts, &lost).await.unwrap();
        assert_eq!(r.empty_volumes, ["00033_00_00"]);
        assert_eq!(r.no_pdf, [D42, D53]);
        assert!(r.written.is_empty() && r.incomplete.is_empty());
        assert!(curated::finished(&curated).await.unwrap().is_empty());
        let e = curated::empties(&curated).await.unwrap();
        assert_eq!(e[&key].attempt, 1);
        assert_eq!(
            e[&key].source_lastmod.as_deref(),
            Some("2024-11-06T03:15:00.159Z")
        );
        assert_eq!(
            e[&key].left_out,
            [format!("{D42}: no_pdf"), format!("{D53}: no_pdf")]
        );
        let l = Listing::load(&curated).await.unwrap();
        assert!(l.units.is_empty() && l.left_out.is_empty());

        // Skipped by the next run: nothing asked for but the sitemaps.
        let again = none();
        let r = run(&again, &text, &curated, None, &opts, &lost)
            .await
            .unwrap();
        assert_eq!((r.already, r.changed), (1, 0));
        assert!(r.empty_volumes.is_empty() && r.no_pdf.is_empty());
        assert_eq!(again.inner.asked("/metadata/pkg/"), 0);
        assert_eq!(again.inner.asked("/content/pkg/"), 0);

        // `--refetch` fetches it again: another empty attempt.
        let refetch = Options {
            refetch: true,
            ..opts.clone()
        };
        let gi = none();
        let r = run(&gi, &text, &curated, None, &refetch, &lost)
            .await
            .unwrap();
        assert_eq!(r.empty_volumes, ["00033_00_00"]);
        assert_eq!(gi.inner.asked("/metadata/pkg/"), 2);
        assert_eq!(curated::empties(&curated).await.unwrap()[&key].attempt, 2);

        // A later `lastmod`: fetched again, and now with PDFs, finished.
        let touched = || Recorded {
            touched: Some((D53, "2026-10-01T00:00:00.000Z")),
            ..Recorded::default()
        };
        let gi = Flaky {
            inner: touched(),
            ..Flaky::default()
        };
        let r = run(&gi, &text, &curated, None, &opts, &lost).await.unwrap();
        assert_eq!((r.changed, r.already), (1, 0));
        assert_eq!(r.written, [("00033_00_00".to_owned(), 3, 6)]);
        let l = Listing::load(&curated).await.unwrap();
        assert_eq!(l.refs().len(), 1);
        assert_eq!(l.refs()[0].attempt, 3);

        // The finished attempt is newer than the empty one: skipped.
        let gi = Flaky {
            inner: touched(),
            ..Flaky::default()
        };
        let r = run(&gi, &text, &curated, None, &opts, &lost).await.unwrap();
        assert_eq!((r.already, r.changed), (1, 0));
        assert_eq!(gi.inner.asked("/metadata/pkg/"), 0);

        // Fetched again with no PDFs: the empty attempt after the finished
        // one takes the volume out of the release.
        let gi = Flaky {
            inner: touched(),
            no_pdf: vec![D42, D53],
            ..Flaky::default()
        };
        let r = run(&gi, &text, &curated, None, &refetch, &lost)
            .await
            .unwrap();
        assert_eq!(r.empty_volumes, ["00033_00_00"]);
        let e = curated::empties(&curated).await.unwrap();
        assert_eq!(e[&key].attempt, 4);
        assert_eq!(
            e[&key].source_lastmod.as_deref(),
            Some("2026-10-01T00:00:00.000Z")
        );
        let l = Listing::load(&curated).await.unwrap();
        assert!(l.units.is_empty());
        assert_eq!(l.left_out, ["serialset/00033_00_00"]);
        let gi = Flaky {
            inner: touched(),
            ..Flaky::default()
        };
        let r = run(&gi, &text, &curated, None, &opts, &lost).await.unwrap();
        assert_eq!((r.already, r.changed), (1, 0));
        assert_eq!(gi.inner.asked("/content/pkg/"), 0);
    }

    /// The recorded responses, with one package of the 1820 sitemap listed
    /// in the 1990 one instead: a volume whose packages are in two yearly
    /// sitemaps.
    struct Moved(Recorded, &'static str);

    #[async_trait::async_trait]
    impl Fetch for Moved {
        async fn get(&self, url: &str) -> anyhow::Result<Option<Vec<u8>>> {
            let body = self.0.get(url).await?;
            let year = |y: &str| url.ends_with(&format!("SERIALSET_{y}_sitemap.xml"));
            if !year("1820") && !year("1990") {
                return Ok(body);
            }
            let block = {
                let s = String::from_utf8(recorded("SERIALSET_1820_sitemap.xml")).unwrap();
                let at = s.find(self.1).unwrap();
                let start = s[..at].rfind("<url>").unwrap();
                let end = at + s[at..].find("</url>").unwrap() + "</url>".len();
                s[start..end].to_owned()
            };
            let s = String::from_utf8(body.unwrap()).unwrap();
            let s = if year("1820") {
                s.replace(&block, "")
            } else {
                s.replace("</urlset>", &format!("{block}\n</urlset>"))
            };
            Ok(Some(s.into_bytes()))
        }
    }

    /// `--years` picks volumes, but each is made of its packages in every
    /// sitemap: a volume listed in 1820 and 1990 is written whole by a run
    /// with `--years 1820` (or `--years 1990`), and a run of every year then
    /// skips it.
    #[tokio::test]
    async fn years_pick_volumes_made_from_every_sitemap() {
        let dir = tempfile::tempdir().unwrap();
        let lost = AtomicBool::new(false);
        let text = RecordedText::default();
        let v33 = plan(parse_urlset(&recorded("SERIALSET_1820_sitemap.xml")).unwrap())
            .volumes
            .remove(1);
        assert_eq!(v33.volume, "00033_00_00");
        for (i, year) in [1820, 1990].into_iter().enumerate() {
            let curated = usnm_store::LocalStore::new(dir.path().join(format!("curated-{i}")));
            let opts = Options {
                years: vec![year],
                serials: vec![33],
                ..options(dir.path())
            };
            let gi = Moved(Recorded::default(), D53);
            let r = run(&gi, &text, &curated, None, &opts, &lost).await.unwrap();
            assert_eq!(r.sitemaps, 2, "{year}");
            // Both documents, D53 from the 1990 sitemap.
            assert_eq!(r.documents, 2, "{year}");
            assert_eq!(r.written, [("00033_00_00".to_owned(), 1, 6)], "{year}");
            let f = curated::finished(&curated).await.unwrap();
            let (counts, _) = curated::read_counts(&curated, &f[0]).await.unwrap();
            assert_eq!(counts.source_packages, Some(v33.fingerprint()), "{year}");
            assert_eq!(counts.source_lastmod, v33.lastmod(), "{year}");

            // Every year: the same packages, so skipped.
            let all = Options {
                years: Vec::new(),
                ..opts.clone()
            };
            let gi = Moved(Recorded::default(), D53);
            let r = run(&gi, &text, &curated, None, &all, &lost).await.unwrap();
            assert_eq!((r.already, r.changed, r.written.len()), (1, 0, 0), "{year}");
            assert_eq!(gi.0.asked("/metadata/pkg/"), 0, "{year}");
        }
    }

    /// A volume is fetched again when its packages change in any way, not
    /// only when the newest `lastmod` moves later: a document's `lastmod`
    /// moved earlier, or a document added with an older one. An attempt
    /// that recorded no packages (written before they were kept) is
    /// fetched again too.
    #[tokio::test]
    async fn a_change_below_the_newest_lastmod_is_fetched_again() {
        let dir = tempfile::tempdir().unwrap();
        let curated = usnm_store::LocalStore::new(dir.path().join("curated"));
        let raw = usnm_store::LocalStore::new(dir.path().join("raw"));
        let lost = AtomicBool::new(false);
        let text = RecordedText::default();
        let opts = Options {
            serials: vec![33],
            ..options(dir.path())
        };
        let r = run(
            &Recorded::default(),
            &text,
            &curated,
            Some(&raw),
            &opts,
            &lost,
        )
        .await
        .unwrap();
        assert_eq!(r.written, [("00033_00_00".to_owned(), 1, 6)]);

        // D53's `lastmod` moves earlier, under D42's (the volume's newest,
        // which stays): fetched again, D53 from GovInfo and D42 from the
        // raw store.
        let earlier = || Recorded {
            touched: Some((D53, "2020-01-01T00:00:00.000Z")),
            ..Recorded::default()
        };
        let gi = earlier();
        let r = run(&gi, &text, &curated, Some(&raw), &opts, &lost)
            .await
            .unwrap();
        assert_eq!((r.changed, r.already), (1, 0));
        assert_eq!(r.written, [("00033_00_00".to_owned(), 2, 6)]);
        assert_eq!(gi.asked("/content/pkg/"), 1);
        let f = curated::finished(&curated).await.unwrap();
        let (c2, _) = curated::read_counts(&curated, &f[0]).await.unwrap();
        assert_eq!(
            c2.source_lastmod.as_deref(),
            Some("2024-11-06T03:15:00.159Z")
        );
        // The same again: skipped.
        let gi = earlier();
        let r = run(&gi, &text, &curated, Some(&raw), &opts, &lost)
            .await
            .unwrap();
        assert_eq!((r.already, r.changed), (1, 0));
        assert_eq!(gi.asked("/metadata/pkg/"), 0);

        // An attempt without the packages, as the first version of the
        // fetch wrote: fetched again (from the raw store).
        let rows = curated::read_checked(&curated, &c2.parts[0]).await.unwrap();
        curated::write_attempt_from(
            &curated,
            SERIES,
            SOURCE,
            "00033_00_00",
            &rows,
            Utc::now(),
            c2.source_lastmod.clone(),
            None,
        )
        .await
        .unwrap();
        let gi = earlier();
        let r = run(&gi, &text, &curated, Some(&raw), &opts, &lost)
            .await
            .unwrap();
        assert_eq!((r.changed, r.already), (1, 0));
        assert_eq!(r.written, [("00033_00_00".to_owned(), 4, 6)]);
        assert_eq!(gi.asked("/content/pkg/"), 0);

        // A document added with an older `lastmod` than the newest: the
        // newest is the same, the fingerprint isn't.
        let mut entries = parse_urlset(&recorded("SERIALSET_1820_sitemap.xml")).unwrap();
        let before = plan(entries.clone()).volumes.remove(1);
        entries.push(Entry {
            loc: "https://www.govinfo.gov/app/details/SERIALSET-00033_00_00-030-0060-0000".into(),
            lastmod: Some("2001-01-01T00:00:00.000Z".into()),
        });
        let after = plan(entries).volumes.remove(1);
        assert_eq!(after.documents.len(), 3);
        assert_eq!(before.lastmod(), after.lastmod());
        assert_ne!(before.fingerprint(), after.fingerprint());
        // Dropped, or listed in another order: as read.
        let mut reordered = after.clone();
        reordered.documents.reverse();
        assert_eq!(reordered.fingerprint(), after.fingerprint());
        reordered.documents.retain(|d| d.id.id != D42);
        assert_ne!(reordered.fingerprint(), after.fingerprint());
    }

    #[test]
    fn the_lock_name_is_a_cosmos_item_id() {
        assert!(!LOCK.contains(['/', '\\', '?', '#']));
        assert_ne!(LOCK, super::super::globe::LOCK);
    }

    /// A two-page PDF with a text layer, written by hand.
    fn tiny_pdf(pages: &[&str]) -> Vec<u8> {
        let n = pages.len();
        let mut objs: Vec<String> = vec![
            "<< /Type /Catalog /Pages 2 0 R >>".into(),
            format!(
                "<< /Type /Pages /Kids [{}] /Count {n} >>",
                (0..n)
                    .map(|i| format!("{} 0 R", 4 + 2 * i))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
        ];
        for (i, text) in pages.iter().enumerate() {
            objs.push(format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 400 200] \
                 /Resources << /Font << /F1 3 0 R >> >> /Contents {} 0 R >>",
                5 + 2 * i
            ));
            let stream = format!("BT /F1 12 Tf 20 150 Td ({text}) Tj ET");
            objs.push(format!(
                "<< /Length {} >>\nstream\n{stream}\nendstream",
                stream.len()
            ));
        }
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, o) in objs.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n{o}\nendobj\n", i + 1).as_bytes());
        }
        let xref = out.len();
        out.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes(),
        );
        for o in offsets {
            out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objs.len() + 1
            )
            .as_bytes(),
        );
        out
    }

    /// The real `pdftotext`, when it is installed (the ingest image has it;
    /// CI may not): a page per page, in order, a blank page kept.
    #[tokio::test]
    async fn pdftotext_reads_a_page_per_page() {
        let available = std::process::Command::new("pdftotext")
            .arg("-v")
            .output()
            .is_ok();
        if !available {
            eprintln!("pdftotext not installed; skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let pdf = dir.path().join("t.pdf");
        std::fs::write(
            &pdf,
            tiny_pdf(&["Letter from the Secretary", "", "of the Treasury"]),
        )
        .unwrap();
        let pages = Pdftotext::new("pdftotext").pages(&pdf).await.unwrap();
        assert_eq!(pages.len(), 3, "{pages:?}");
        assert_eq!(pages[0].trim(), "Letter from the Secretary");
        assert_eq!(pages[1].trim(), "");
        assert_eq!(pages[2].trim(), "of the Treasury");
        // Not a PDF: an error, which the fetch counts as unreadable.
        std::fs::write(&pdf, b"<html>not a pdf</html>").unwrap();
        assert!(Pdftotext::new("pdftotext").pages(&pdf).await.is_err());
        assert!(Pdftotext::new("/nonexistent/pdftotext")
            .pages(&pdf)
            .await
            .is_err());
    }
}
