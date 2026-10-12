//! The checks on node output (12 §12.5 Step 8), cheapest first. Each
//! failure is a fixed reason code that holds none of the content.

use std::collections::HashMap;

use chrono::NaiveDate;
use sha2::{Digest, Sha256};
use usnm_ocr_contract::message::{decode_dead_letter, DeadLetter};
use usnm_overlay::NodeRow;

use super::ledger::Ledger;

/// The largest part, runs record and ALTO file read (check 1).
pub const MAX_PART_BYTES: u64 = 1024 * 1024;
pub const MAX_ALTO_BYTES: u64 = 32 * 1024 * 1024;
/// The longest text kept (check 4).
pub const MAX_TEXT_BYTES: usize = 512 * 1024;
/// The longest `ocr_engine` kept.
pub const MAX_ENGINE_CHARS: usize = 512;

/// `ocr_source` values of the engines 13 lists (`usnm-<family>`, 13
/// §13.8.1). The engine list in `ocr/engines/engines.toml` (13 §13.7.2)
/// replaces this when it lands.
pub const ENGINE_SOURCES: [&str; 3] = ["usnm-tesseract", "usnm-kraken", "usnm-doctr"];

/// Where scans come from: tile.loc.gov's IIIF image server (the page's
/// IIIF base, which the worker writes as `image_url`) and its storage
/// service (a batch's original JP2, which the fetcher reads since #368).
pub const IMAGE_PREFIX: &str = "https://tile.loc.gov/";

/// Check 5's test of `image_url`: a short URL on tile.loc.gov with no
/// spaces or control characters.
pub fn image_url_ok(url: &str) -> bool {
    url.starts_with(IMAGE_PREFIX)
        && url.len() <= 512
        && !url.chars().any(|c| c.is_control() || c.is_whitespace())
}

/// Why a part or dead letter was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reject {
    /// Check 1: the name isn't the worker's pattern.
    Name,
    PartTooLarge,
    AltoMissing,
    AltoTooLarge,
    /// Check 2.
    PartUnreadable,
    Rows,
    IssueMismatch,
    /// Check 3.
    NotRequested,
    BatchMismatch,
    ReasonMismatch,
    /// Check 4.
    TextNull,
    TextTooLarge,
    TextControl,
    TextMismatch,
    /// Check 5.
    Engine,
    EngineTag,
    Reason,
    BatchUnpublished,
    TitleUnknown,
    TitleJapanese,
    ImageUrl,
    /// Check 6.
    Alto(usnm_alto::AltoError),
    /// A dead letter that fails its schema.
    DeadLetter(&'static str),
}

impl Reject {
    pub fn code(self) -> String {
        let s = match self {
            Reject::Name => "name",
            Reject::PartTooLarge => "part_too_large",
            Reject::AltoMissing => "alto_missing",
            Reject::AltoTooLarge => "alto_too_large",
            Reject::PartUnreadable => "part_unreadable",
            Reject::Rows => "rows",
            Reject::IssueMismatch => "issue_mismatch",
            Reject::NotRequested => "not_requested",
            Reject::BatchMismatch => "batch_mismatch",
            Reject::ReasonMismatch => "reason_mismatch",
            Reject::TextNull => "text_null",
            Reject::TextTooLarge => "text_too_large",
            Reject::TextControl => "text_control",
            Reject::TextMismatch => "text_mismatch",
            Reject::Engine => "engine",
            Reject::EngineTag => "engine_tag",
            Reject::Reason => "reason",
            Reject::BatchUnpublished => "batch_unpublished",
            Reject::TitleUnknown => "title_unknown",
            Reject::TitleJapanese => "title_japanese",
            Reject::ImageUrl => "image_url",
            Reject::Alto(e) => e.code(),
            Reject::DeadLetter(why) => {
                // DeadLetterError's texts, as codes.
                let code: String = why
                    .chars()
                    .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                    .collect();
                return format!("deadletter_{code}");
            }
        };
        s.to_owned()
    }
}

/// A part's name: `pages/<lccn>_<YYYY-MM-DD>_ed-<n>.<node>.<stamp>.parquet`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartName {
    pub lccn: String,
    pub date: NaiveDate,
    pub edition: u16,
    pub node: String,
    /// `YYYYMMDDTHHMMSSffffffZ`.
    pub stamp: String,
}

fn is_node(s: &str) -> bool {
    (1..=32).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn is_stamp(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 22
        && b[..8].iter().all(u8::is_ascii_digit)
        && b[8] == b'T'
        && b[9..21].iter().all(u8::is_ascii_digit)
        && b[21] == b'Z'
}

/// Parse a part's path as the worker names it (`output_paths`).
pub fn parse_part_name(path: &str) -> Option<PartName> {
    let name = path.strip_prefix("pages/")?.strip_suffix(".parquet")?;
    let mut it = name.splitn(3, '.');
    let (issue, node, stamp) = (it.next()?, it.next()?, it.next()?);
    if !is_node(node) || !is_stamp(stamp) {
        return None;
    }
    // `<lccn>_<date>_ed-<n>`; an LCCN has no `_`.
    let (lccn, rest) = issue.split_once('_')?;
    let (date, ed) = rest.split_once("_ed-")?;
    let date = NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    if date.format("%Y-%m-%d").to_string() != rest.split_once("_ed-")?.0 {
        return None;
    }
    let edition: u16 = ed.parse().ok()?;
    if edition.to_string() != ed || edition == 0 {
        return None;
    }
    // The canonical page key form for its lccn.
    usnm_core::ids::PageKey::new(lccn, date, edition, 1).ok()?;
    Some(PartName {
        lccn: lccn.to_owned(),
        date,
        edition,
        node: node.to_owned(),
        stamp: stamp.to_owned(),
    })
}

/// The ALTO path in `ocr-out` that goes with a part's row.
pub fn source_alto_path(doc_id: &str, name: &PartName) -> String {
    format!("alto/{doc_id}.{}.{}.xml", name.node, name.stamp)
}

/// What the checks need besides the part.
#[derive(Debug, Clone, Default)]
pub struct Context {
    /// The published version's base batch names and their curated versions.
    pub published: HashMap<String, u16>,
    /// Catalog languages (MARC codes) by lccn.
    pub titles: HashMap<String, Vec<String>>,
    pub ledger: Ledger,
}

impl Context {
    pub fn title_languages(&self, lccn: &str) -> Vec<&str> {
        self.titles
            .get(lccn)
            .map(|l| l.iter().map(String::as_str).collect())
            .unwrap_or_default()
    }
}

/// Text with no control characters but newline and tab.
fn clean_text(text: &str) -> bool {
    !text
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t')
}

/// An `ocr_engine` value we keep: short, printable, and not from an image
/// the engine lock doesn't pin (13 §13.8.1).
fn engine_tag_ok(tag: &str) -> bool {
    !tag.is_empty()
        && tag.chars().count() <= MAX_ENGINE_CHARS
        && !tag.chars().any(char::is_control)
        && !tag.split(' ').any(|f| f == "image=local")
}

/// Checks 2 to 5 on a part's bytes (its name and size passed check 1).
/// The ALTO's existence and checks 6 and 7 are the caller's.
pub fn check_part(name: &PartName, bytes: Vec<u8>, ctx: &Context) -> Result<NodeRow, Reject> {
    // 2: the strict reader, one row, the row's issue is the name's.
    let rows = usnm_overlay::read_node_part(bytes::Bytes::from(bytes))
        .map_err(|_| Reject::PartUnreadable)?;
    let [row] = <[NodeRow; 1]>::try_from(rows).map_err(|_| Reject::Rows)?;
    let k = &row.key;
    if k.lccn != name.lccn || k.date != name.date || k.edition != name.edition {
        return Err(Reject::IssueMismatch);
    }
    let doc_id = k.doc_id();
    // 3: asked for, with the same batch and reason.
    let queued = ctx
        .ledger
        .outstanding(&doc_id)
        .ok_or(Reject::NotRequested)?;
    if queued.batch != row.batch {
        return Err(Reject::BatchMismatch);
    }
    if queued.reason != row.loc_text {
        return Err(Reject::ReasonMismatch);
    }
    // 4: the text is what its hash and length say, and plain.
    let text = row.text.as_deref().ok_or(Reject::TextNull)?;
    if text.len() > MAX_TEXT_BYTES {
        return Err(Reject::TextTooLarge);
    }
    if !clean_text(text) {
        return Err(Reject::TextControl);
    }
    let chars = i32::try_from(text.chars().count()).unwrap_or(i32::MAX);
    let status = usnm_overlay::status_str(usnm_core::text::text_status(text));
    if Sha256::digest(text.as_bytes()).as_slice() != row.text_sha256
        || chars != row.text_chars
        || status != row.text_status
    {
        return Err(Reject::TextMismatch);
    }
    // 5: provenance, batch, title and image.
    if !ENGINE_SOURCES.contains(&row.ocr_source.as_str()) {
        return Err(Reject::Engine);
    }
    if !engine_tag_ok(&row.ocr_engine) {
        return Err(Reject::EngineTag);
    }
    if !matches!(row.loc_text.as_str(), "bulk" | "user") {
        return Err(Reject::Reason);
    }
    if !ctx.published.contains_key(&row.batch) {
        return Err(Reject::BatchUnpublished);
    }
    let languages = ctx.titles.get(&k.lccn).ok_or(Reject::TitleUnknown)?;
    if languages.iter().any(|l| l == "jpn") {
        return Err(Reject::TitleJapanese);
    }
    if !image_url_ok(&row.image_url) {
        return Err(Reject::ImageUrl);
    }
    Ok(row)
}

/// Check 6: the ALTO, with `usnm-alto`'s bounded reader.
pub fn check_alto(bytes: &[u8]) -> Result<usnm_alto::AltoPage, Reject> {
    let limits = usnm_alto::Limits {
        max_bytes: MAX_ALTO_BYTES as usize,
        ..usnm_alto::Limits::default()
    };
    usnm_alto::check(bytes, &limits).map_err(Reject::Alto)
}

/// A dead letter's text, checked (v1 or v2, unknown fields refused, every
/// field in its format; #338's rules).
pub fn check_dead_letter(text: &str) -> Result<DeadLetter, Reject> {
    decode_dead_letter(text).map_err(|e| Reject::DeadLetter(e.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_urls_are_on_tile_loc_gov() {
        for ok in [
            "https://tile.loc.gov/image-services/iiif/service:ndnp:mdu:batch_mdu_garlic_ver02:data:sn83045081:0029602241A:1904072901:0812",
            "https://tile.loc.gov/storage-services/service/ndnp/mdu/batch_mdu_garlic_ver02/data/sn83045081/0029602241A/1904072901/0812.jp2",
        ] {
            assert!(image_url_ok(ok), "{ok}");
        }
        for bad in [
            "https://evil.example/x",
            "https://tile.loc.gov.evil.example/x",
            "http://tile.loc.gov/x",
            "https://tile.loc.gov/a b",
        ] {
            assert!(!image_url_ok(bad), "{bad}");
        }
    }
}
