//! Our own OCR of the Japanese pages LoC has no text for (04 §4.8, #139).
//!
//! The OCR job (`ja-ocr/jaocr.py`) writes `ocr-ja/pages/<issue>[.<n>].parquet`
//! to the curated store, one row per page, keyed by `doc_id`. A release reads
//! every part, keeps the newest row for each page (by `ocred_at`, then part
//! path), and keeps the pages of batches in the version whose titles are in
//! the catalog. It indexes them in their own small index (`pages-ja-…`,
//! `infra/quickwit/pages-ja-index.yaml`), rebuilt at every release, and adds
//! the pages curation never had (`loc_text = missing`: no `ocr.txt` in LoC's
//! archive) to the version's baselines. Pages curation did have (empty,
//! short, garbled or mixed LoC text) are already counted there.
//!
//! With `--ja-latin` (#203), the missing pages also get a main-index document
//! holding the Latin-script text our OCR read on them ([`latin_text`]: the
//! English ads, mastheads and sections), so English searches reach them.
//! Each page is counted once: the baselines have it already, and a query
//! searches either the main indexes or the Japanese one, never both.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Context;
use bytes::Bytes;
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use usnm_core::ids::PageKey;
use usnm_core::ja;
use usnm_core::text::Analyzer;
use usnm_core::time::{day_number, ym_number};
use usnm_store::ObjectStore;

use crate::catalog::{Catalog, Place, Title};
use crate::source::hex;

/// Where the OCR job writes its parts in the curated store (a directory:
/// stores list it without the trailing slash).
pub const PREFIX: &str = "ocr-ja/pages";

/// The Japanese pages' index mapping.
pub const JA_TEMPLATE: &str = include_str!("../../../infra/quickwit/pages-ja-index.yaml");

/// The snapshot file recording which overlay parts a version was built from.
pub const OCR_JA_FILE: &str = "ocr_ja.json";

/// One page of our Japanese OCR.
#[derive(Debug, Clone, PartialEq)]
pub struct JaPage {
    pub key: PageKey,
    pub batch: String,
    /// Why LoC's text wasn't used: `missing`, `empty`, `short`, `garbled`
    /// or `mixed` (word-like enough to keep, but likely a page with a
    /// Japanese column LoC read as Latin, #204).
    pub loc_text: String,
    pub ok: bool,
    pub printed: Option<String>,
    pub ocr_source: String,
    pub ocr_engine: String,
    ocred_at: i64,
    part: String,
}

impl JaPage {
    /// Whether the page goes into the Japanese index: our OCR found text,
    /// and on a `mixed` page (one LoC read as mostly words) some Japanese.
    /// Such a page is in the main index with LoC's text already; when our
    /// OCR finds no Japanese on it, it is an English page that LoC read
    /// badly, not a Japanese one.
    pub fn indexable(&self) -> bool {
        let Some(printed) = self.printed.as_deref() else {
            return false;
        };
        self.ok && (self.loc_text != "mixed" || japanese_chars(printed) >= MIN_MIXED_JA_CHARS)
    }

    /// The Latin-script text our OCR read on this page (#203), for its
    /// main-index document, or `None` with less than
    /// [`usnm_core::text::MIN_PAGE_CHARS`] of it, the bar LoC's own text has
    /// to clear (04 §4.5). A page curation never had gets a document with
    /// it; a page curation had gets it in place of LoC's text when it reads
    /// more words ([`better_than_locs`]).
    pub fn latin(&self) -> Option<String> {
        if !self.ok {
            return None;
        }
        let text = latin_text(self.printed.as_deref()?);
        (usnm_core::text::text_status(&text) == usnm_core::text::TextStatus::Ok).then_some(text)
    }

    /// A page curation never saw: not in any `counts.json`, so not in the baselines.
    pub fn missing_from_curation(&self) -> bool {
        self.loc_text == "missing"
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PartRef {
    pub path: String,
    pub sha256: String,
    pub bytes: usize,
}

/// The overlay as a version uses it.
#[derive(Debug, Default)]
pub struct Overlay {
    /// The pages kept, sorted by doc_id.
    pub pages: Vec<JaPage>,
    /// Every part read, sorted by path.
    pub parts: Vec<PartRef>,
    /// Pages left out: their batch isn't in the version, or their title isn't catalogued.
    pub skipped: u64,
}

/// Read every overlay part and keep the newest row per page, for the batches in
/// `batches` (base names, as in `RunBatch::batch`) whose titles are catalogued.
pub async fn load(
    store: &dyn ObjectStore,
    batches: &BTreeSet<&str>,
    catalog: &Catalog,
) -> anyhow::Result<Overlay> {
    let mut paths: Vec<String> = store
        .list(PREFIX)
        .await?
        .into_iter()
        .filter(|p| p.starts_with(&format!("{PREFIX}/")) && p.ends_with(".parquet"))
        .collect();
    paths.sort();
    let mut newest: BTreeMap<String, JaPage> = BTreeMap::new();
    // Every page seen, so pages with no copy in the version count as skipped.
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut parts = Vec::with_capacity(paths.len());
    for path in paths {
        let bytes = store
            .get(&path)
            .await?
            .with_context(|| format!("overlay part `{path}` vanished"))?;
        parts.push(PartRef {
            path: path.clone(),
            sha256: hex(&Sha256::digest(&bytes)),
            bytes: bytes.len(),
        });
        for page in read_part(Bytes::from(bytes), &path).with_context(|| path.clone())? {
            let doc_id = page.key.doc_id();
            seen.insert(doc_id.clone());
            // Only copies from the version's batches, with catalogued titles,
            // compete: a newer copy from elsewhere mustn't hide one that counts.
            if !batches.contains(page.batch.as_str()) || catalog.title(&page.key.lccn).is_none() {
                continue;
            }
            let newer = newest
                .get(&doc_id)
                .is_none_or(|old| (page.ocred_at, &page.part) > (old.ocred_at, &old.part));
            if newer {
                newest.insert(doc_id, page);
            }
        }
    }
    Ok(Overlay {
        skipped: (seen.len() - newest.len()) as u64,
        pages: newest.into_values().collect(),
        parts,
    })
}

/// The rows of one overlay part (the schema `jaocr.py write_part` writes,
/// [`usnm_overlay::schema`]).
pub fn read_part(bytes: Bytes, path: &str) -> anyhow::Result<Vec<JaPage>> {
    Ok(usnm_overlay::read_part(bytes)?
        .into_iter()
        .map(|p| JaPage {
            key: p.key,
            batch: p.batch,
            loc_text: p.loc_text,
            ok: p.text_status == "ok",
            printed: p.text,
            ocr_source: p.ocr_source,
            ocr_engine: p.ocr_engine,
            ocred_at: p.ocred_at,
            part: path.to_owned(),
        })
        .collect())
}

/// The engine document for a Japanese page: the fields of the main index's
/// `page_doc`, with `text` the tokens of `printed` folded by `analyzer`.
pub fn ja_doc(page: &JaPage, title: &Title, place: &Place, analyzer: Analyzer) -> Value {
    let k = &page.key;
    let printed = page.printed.as_deref().unwrap_or_default();
    json!({
        "doc_id": k.doc_id(),
        "day": day_number(k.date),
        "ym": ym_number(k.date),
        "year": chrono::Datelike::year(&k.date),
        "place_id": place.id,
        "place_shard": place.ordinal % 8,
        "lccn": k.lccn,
        "state": title.state,
        "language": title.languages,
        "front_page": k.seq == 1,
        "edition": k.edition,
        "seq": k.seq,
        "sort_key": (u64::from(title.ordinal) << 32) | (u64::from(k.edition) << 16) | u64::from(k.seq),
        "date": k.date.to_string(),
        "batch": page.batch,
        "text": ja::index_text(printed, analyzer),
        "printed": printed,
        "ocr_source": page.ocr_source,
        "ocr_engine": page.ocr_engine,
    })
}

/// The rule [`latin_text`] reads the Latin-script text by, recorded in a
/// version's build record when it writes such text (`--ja-latin`).
pub const LATIN_VERSION: u32 = 1;

/// Japanese characters a `mixed` page's OCR needs to join the Japanese index:
/// a short line of Japanese. NDLOCR-Lite reading an English page puts out a
/// few Japanese-looking characters for specks and rules (一, ー, ロ): at most
/// 13 on 42 sampled English pages, against 1,433 or more on the mixed ones
/// (04 §4.8).
pub const MIN_MIXED_JA_CHARS: usize = 20;

/// The Japanese characters (kana, Han) in `text`.
pub fn japanese_chars(text: &str) -> usize {
    text.chars().filter(|c| ja::is_ja(*c)).count()
}

/// A character of Latin-script text: ASCII (after folding full-width forms,
/// [`fold_fullwidth`]), Latin letters with diacritics, and the general
/// punctuation English print uses (dashes, curly quotes).
fn is_latin_run_char(c: char) -> bool {
    c.is_ascii() || matches!(c, '\u{00A0}'..='\u{024F}' | '\u{2010}'..='\u{205E}')
}

/// Full-width ASCII forms (`Ｄｅｎｖｅｒ`, which a Japanese engine may put out for
/// Latin set in Japanese type) as ASCII.
fn fold_fullwidth(c: char) -> char {
    match c {
        '\u{FF01}'..='\u{FF5E}' => char::from_u32(c as u32 - 0xFEE0).unwrap_or(c),
        '\u{3000}' => ' ',
        c => c,
    }
}

/// The Latin-script text in our OCR of a page (#203): on each line, the runs
/// between Japanese characters and Japanese punctuation that hold at least
/// two Latin letters (`Moritz Drug Co`, `2001 Larimer St., Denver`), joined
/// by spaces, a line per line, then normalized as LoC's text is (04 §4.5).
/// Numbers alone in Japanese text (`1945年`) and stray letters are left out.
pub fn latin_text(printed: &str) -> String {
    let mut out = String::new();
    for line in printed.lines() {
        let mut kept: Vec<String> = Vec::new();
        let mut run = String::new();
        let mut flush = |run: &mut String| {
            let letters = run.chars().filter(|c| c.is_alphabetic()).count();
            let trimmed = run.trim();
            if letters >= 2 {
                kept.push(trimmed.to_owned());
            }
            run.clear();
        };
        for c in line.chars().map(fold_fullwidth) {
            if is_latin_run_char(c) && !c.is_control() {
                run.push(c);
            } else {
                flush(&mut run);
            }
        }
        flush(&mut run);
        if !kept.is_empty() {
            out.push_str(&kept.join(" "));
            out.push('\n');
        }
    }
    usnm_core::text::normalize_ocr(&out)
}

/// Tokens that look like words, by `ja-ocr/jaocr.py`'s rule (`WORDLIKE`,
/// the test that picks pages for our OCR): after trimming `.,;:!?'"()`, a
/// lower-case word of 2 letters or more, optionally capitalized, or 3 to 15
/// capitals. OCR noise mixes case and letters with digits and symbols.
pub fn wordlike_tokens(text: &str) -> usize {
    text.split_whitespace()
        .map(|w| w.trim_matches(|c| ".,;:!?'\"()".contains(c)))
        .filter(|w| {
            let n = w.chars().count();
            let lower = |s: &str| s.len() >= 2 && s.chars().all(|c| c.is_ascii_lowercase());
            let rest = w
                .strip_prefix(|c: char| c.is_ascii_uppercase())
                .unwrap_or(w);
            lower(rest) || ((3..=15).contains(&n) && w.chars().all(|c| c.is_ascii_uppercase()))
        })
        .count()
}

/// Whether our Latin text should stand in for LoC's on a page curation had
/// (#203): it holds more word-like tokens. On the English pages LoC read
/// badly (mimeographed bulletins, `garbled` and the lower `mixed` bands),
/// NDLOCR-Lite read 1,245 common English words on four sampled pages where
/// LoC's text had 339 (04 §4.8).
pub fn better_than_locs(ours: &str, locs: Option<&str>) -> bool {
    wordlike_tokens(ours) > wordlike_tokens(locs.unwrap_or_default())
}

/// The main-index document of a page curation never had (#203): the main
/// index's fields ([`crate::release::main_doc`]) with `latin` as its text.
/// Written in the release's `format`, as every main-index document of the
/// version is (its decade partition, 05 §5.5.5, and its analyzer, #168).
pub fn latin_doc(
    page: &JaPage,
    latin: &str,
    title: &Title,
    place: &Place,
    format: crate::release::DocFormat,
) -> Value {
    crate::release::main_doc(&page.key, &page.batch, latin, title, place, None, format)
}

/// What our OCR found on a page: `japanese` (at least 20 characters, half of
/// them or more Japanese), `near_blank` (under 20 characters: a blank page,
/// a picture, a masthead) or `mostly_latin` (the rest). For the snapshot's
/// record of what the overlay holds.
pub fn kind(printed: Option<&str>) -> &'static str {
    let printed = printed.unwrap_or_default();
    let chars = printed.chars().filter(|c| !c.is_whitespace()).count();
    let ja = printed.chars().filter(|c| ja::is_ja(*c)).count();
    if chars < usnm_core::text::MIN_PAGE_CHARS {
        "near_blank"
    } else if ja * 2 >= chars {
        "japanese"
    } else {
        "mostly_latin"
    }
}

/// How many pages of each [`kind`], overall and by why LoC's text wasn't used.
pub fn kinds(pages: &[JaPage]) -> Value {
    let mut all: BTreeMap<&str, u64> = BTreeMap::new();
    let mut by_loc_text: BTreeMap<&str, BTreeMap<&str, u64>> = BTreeMap::new();
    for p in pages {
        let k = kind(p.printed.as_deref());
        *all.entry(k).or_default() += 1;
        *by_loc_text
            .entry(p.loc_text.as_str())
            .or_default()
            .entry(k)
            .or_default() += 1;
    }
    json!({ "all": all, "by_loc_text": by_loc_text })
}

/// The Japanese index for a version: `pages-v20261005-1` → `pages-ja-20261005-1`.
pub fn index_id(version: &str) -> String {
    format!(
        "pages-ja-{}",
        version.strip_prefix("pages-v").unwrap_or(version)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_what_the_ocr_found() {
        assert_eq!(
            kind(Some(
                "去年の大記事は何?やはり西歐大侵略戰。米國通信社の面白い調査"
            )),
            "japanese"
        );
        assert_eq!(
            kind(Some("NEW YEAR'S EDITION The Rocky Shimpo DENVER COLORADO")),
            "mostly_latin"
        );
        assert_eq!(kind(Some("  ﾉ 1 \n")), "near_blank");
        assert_eq!(kind(None), "near_blank");
    }

    #[test]
    fn keeps_the_latin_runs_of_a_page() {
        // Lines from NDLOCR-Lite's reading of a Colorado Times page (1945-03-29, seq 4).
        let printed = "ハート山通信 雨宮一聲\n\
            Moritz Drug Co\n\
            2001 Larimer St., Denver\n\
            比較して、 ) (一二\n\
            一月上旬ハ千七百臺であつ\n\
            1946年 パウエル市 Powell 開拓局\n\
            Ｄｅｎｖｅｒ　２\n\
            UMEYA COMPANY\n\
            x";
        assert_eq!(
            latin_text(printed),
            "Moritz Drug Co\n2001 Larimer St., Denver\nPowell\nDenver 2\nUMEYA COMPANY"
        );
        assert_eq!(latin_text("去年の大記事は何?やはり西歐大侵略戰。1945"), "");
    }

    fn page(loc_text: &str, printed: &str) -> JaPage {
        JaPage {
            key: PageKey::new(
                "sn83025518",
                chrono::NaiveDate::from_ymd_opt(1945, 3, 29).unwrap(),
                1,
                4,
            )
            .unwrap(),
            batch: "batch_dlc_dupontcircle".into(),
            loc_text: loc_text.into(),
            ok: true,
            printed: Some(printed.into()),
            ocr_source: "usnm-ndlocr-lite".into(),
            ocr_engine: "ndlocr-lite test".into(),
            ocred_at: 0,
            part: "p".into(),
        }
    }

    #[test]
    fn latin_text_needs_twenty_characters() {
        let ads = "Moritz Drug Co\n2001 Larimer St., Denver\n日本語の記事";
        for loc_text in ["missing", "garbled", "short", "empty", "mixed"] {
            assert_eq!(
                page(loc_text, ads).latin().as_deref(),
                Some("Moritz Drug Co\n2001 Larimer St., Denver"),
                "{loc_text}"
            );
        }
        // Under 20 characters, like LoC's `short` pages: no document.
        assert_eq!(page("missing", "ROCKY SHIMPO\n日本").latin(), None);
        let mut blank = page("missing", ads);
        blank.ok = false;
        assert_eq!(blank.latin(), None);
    }

    #[test]
    fn counts_word_like_tokens_as_the_job_does() {
        // jaocr.py WORDLIKE: "The", "relocation", "WAR", "Denver," and "(news)".
        assert_eq!(
            wordlike_tokens("The relocation WAR Denver, (news) tbE aB 1945 x Ab ABCDEFGHIJKLMNOP"),
            5
        );
        let locs = "Br H rJjr Jjw-Ini l iiiTTTTiwtiawyM d j.. s,y fr r vtT the";
        assert!(better_than_locs(
            "The relocation center held a meeting",
            Some(locs)
        ));
        assert!(!better_than_locs(
            "THE OUTPOST",
            Some("The relocation center held a meeting")
        ));
        assert!(better_than_locs("THE OUTPOST", None));
        assert!(!better_than_locs("1945 ー", None));
    }

    #[test]
    fn a_mixed_page_joins_the_japanese_index_only_with_japanese_on_it() {
        let english = "THE ROHWER OUTPOST Saturday, March 24, 1945 一 ロ ー";
        assert!(!page("mixed", english).indexable());
        assert!(
            page("garbled", english).indexable(),
            "unchanged for the others"
        );
        let mixed = format!(
            "{english}\n{}",
            "去年の大記事は何?やはり西歐大侵略戰。米國通信社の面白い調査"
        );
        assert!(page("mixed", &mixed).indexable());
    }

    #[test]
    fn names_the_index_after_the_version() {
        assert_eq!(index_id("pages-v20261005-1"), "pages-ja-20261005-1");
    }
}
