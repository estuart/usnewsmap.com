//! Congress pages (#229): the Congressional Globe, the Serial Set, the
//! Annals of Congress and the Register of Debates, one engine document per
//! page in their own index set (`infra/quickwit/congress-index.yaml`),
//! named in `current.json` under `congress`.
//!
//! Phase 0 holds the shapes: the curated page a fetch job writes
//! ([`CongressPage`]) and the engine document a release builds from it
//! ([`congress_doc`]). The fetch jobs and the release step that builds the
//! index come with the first source (the Globe, from UNT).

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use usnm_core::text::Analyzer;
use usnm_core::time::{day_number, ym_number};

/// The Congress index template, as the release would apply it.
pub const TEMPLATE: &str = include_str!("../../../infra/quickwit/congress-index.yaml");

/// The reserved place every Congress page sits at for now (#229 design §3):
/// Washington, DC, apart from DC's newspapers' place. The API's
/// `refdata::CONGRESS_PLACE` is the same id.
pub const PLACE_ID: &str = "PC001";

/// The series a page can come from, in the order hits on one day list them.
pub const SERIES: &[&str] = &["annals", "register", "globe", "serialset"];

/// One state a page names, and how we know it (#229 design §6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateRef {
    /// USPS code.
    pub state: String,
    /// `mods_geographic`, `title`, `text_gazetteer` or `speaker`.
    pub via: String,
}

/// One curated Congress page: the fields the release indexes. The curated
/// Parquet keeps more (raw MODS places, speaker detail); this is the part
/// the engine document needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CongressPage {
    /// `{package_id}_p{seq}` (GovInfo), `unt-{metadc}_p{seq}` (UNT),
    /// `lcg-{series}-{volume}_p{seq}` (Congress.gov): letters, digits, `_`
    /// and `-` only.
    pub doc_id: String,
    /// One of [`SERIES`].
    pub series: String,
    /// The page's place among its series' bound units (serial number,
    /// UNT item, volume), for a stable order of hits on one day.
    pub unit_ordinal: u32,
    /// The page's place in its PDF or item.
    pub seq: u32,
    /// The day the page is dated to: the day heading it falls under
    /// (debates) or MODS `dateIssued` (Serial Set).
    pub date: chrono::NaiveDate,
    pub congress: Option<u16>,
    pub session: Option<String>,
    pub chamber: Option<String>,
    pub title: Option<String>,
    pub citation: Option<String>,
    pub printed_page: Option<String>,
    pub package_id: Option<String>,
    pub link: Option<String>,
    /// Normalized text (04 §4.5). Empty or short pages still count in the
    /// baselines but aren't indexed, as for newspapers.
    pub text: String,
    /// Who made the text: `govinfo`, `unt`, or our OCR (`usnm-tesseract`).
    pub ocr_source: Option<String>,
    pub ocr_engine: Option<String>,
    #[serde(default)]
    pub states: Vec<StateRef>,
    /// Bioguide ids of the members who speak on the page (debates).
    #[serde(default)]
    pub speakers: Vec<String>,
}

/// The Congress index id for an index version: `pages-v20261010-1` →
/// `congress-20261010-1`.
pub fn index_id(version: &str) -> String {
    format!(
        "congress-{}",
        version.strip_prefix("pages-v").unwrap_or(version)
    )
}

/// Same-day order for hits: series, then bound unit, then page.
pub fn sort_key(page: &CongressPage) -> u64 {
    let series = SERIES
        .iter()
        .position(|s| *s == page.series)
        .unwrap_or(SERIES.len()) as u64;
    (series << 56)
        | (u64::from(page.unit_ordinal & 0xF_FFFF) << 24)
        | u64::from(page.seq & 0xFF_FFFF)
}

/// The engine document for a Congress page: the newspaper fields the shared
/// search code reads (dates, place, state, language, `text`, `text_cg`,
/// `sort_key`), with Congress's own fields beside them.
pub fn congress_doc(page: &CongressPage, analyzer: Analyzer) -> Value {
    let mut states: Vec<&str> = page.states.iter().map(|s| s.state.as_str()).collect();
    states.sort_unstable();
    states.dedup();
    let state_via: Vec<String> = page
        .states
        .iter()
        .map(|s| format!("{}:{}", s.state, s.via))
        .collect();
    let mut doc = json!({
        "doc_id": page.doc_id,
        "day": day_number(page.date),
        "ym": ym_number(page.date),
        "year": chrono::Datelike::year(&page.date),
        "date": page.date.to_string(),
        "place_id": PLACE_ID,
        "place_shard": 0,
        // Congress pages belong to no newspaper and have no front page.
        "lccn": "",
        // Where it was printed, as for newspapers; `states` holds the states
        // a page is about.
        "state": "DC",
        "language": ["eng"],
        "front_page": false,
        "edition": 1,
        "seq": page.seq,
        "sort_key": sort_key(page),
        "text": page.text,
        "text_cg": usnm_core::common_grams::index_text(&page.text, analyzer),
        "series": page.series,
        "states": states,
        "state_via": state_via,
        "speakers": page.speakers,
    });
    let optional = [
        ("title", page.title.as_deref()),
        ("citation", page.citation.as_deref()),
        ("session", page.session.as_deref()),
        ("chamber", page.chamber.as_deref()),
        ("printed_page", page.printed_page.as_deref()),
        ("package_id", page.package_id.as_deref()),
        ("link", page.link.as_deref()),
        ("ocr_source", page.ocr_source.as_deref()),
        ("ocr_engine", page.ocr_engine.as_deref()),
    ];
    for (key, value) in optional {
        if let Some(v) = value {
            doc[key] = v.into();
        }
    }
    if let Some(c) = page.congress {
        doc["congress"] = c.into();
    }
    doc
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page() -> CongressPage {
        CongressPage {
            doc_id: "unt-metadc29274_p300".into(),
            series: "globe".into(),
            unit_ordinal: 42,
            seq: 300,
            date: chrono::NaiveDate::from_ymd_opt(1846, 7, 21).unwrap(),
            congress: Some(29),
            session: Some("1".into()),
            chamber: Some("senate".into()),
            title: Some("The Congressional Globe, 29th Congress, 1st Session".into()),
            citation: Some("Cong. Globe, 29th Cong., 1st Sess. 1112".into()),
            printed_page: Some("1112".into()),
            package_id: Some("metadc29274".into()),
            link: Some("https://digital.library.unt.edu/ark:/67531/metadc29274/m1/300/".into()),
            text: "Mr. CLAY of Alabama moved the bill on the tariff of duties".into(),
            ocr_source: Some("unt".into()),
            ocr_engine: None,
            states: vec![
                StateRef {
                    state: "AL".into(),
                    via: "speaker".into(),
                },
                StateRef {
                    state: "AL".into(),
                    via: "title".into(),
                },
            ],
            speakers: vec!["C000482".into()],
        }
    }

    /// The field names the template declares, read from its
    /// `{ name: …, … }` lines.
    fn template_fields() -> Vec<String> {
        TEMPLATE
            .lines()
            .filter_map(|l| l.trim().strip_prefix("- { name: "))
            .filter_map(|l| l.split(',').next())
            .map(|n| n.trim().to_owned())
            .collect()
    }

    #[test]
    fn every_field_written_is_in_the_strict_template() {
        let fields = template_fields();
        assert!(fields.len() > 20, "{fields:?}");
        let doc = congress_doc(&page(), Analyzer::LATEST);
        for key in doc.as_object().unwrap().keys() {
            assert!(
                fields.contains(key),
                "`{key}` is not in congress-index.yaml"
            );
        }
    }

    #[test]
    fn the_template_has_every_field_the_shared_search_code_reads() {
        // Filters, aggregations and hits read these on every index set.
        let fields = template_fields();
        for f in [
            "doc_id",
            "text",
            "text_cg",
            "date",
            "day",
            "ym",
            "year",
            "place_id",
            "place_shard",
            "lccn",
            "state",
            "language",
            "front_page",
            "edition",
            "seq",
            "sort_key",
            "series",
        ] {
            assert!(
                fields.iter().any(|x| x == f),
                "congress-index.yaml lacks `{f}`"
            );
        }
        // And it keeps the newspaper template's analyzer.
        let tokenizer = |t: &str| {
            t.lines()
                .find(|l| l.contains("{ name: text,"))
                .map(str::to_owned)
        };
        assert_eq!(
            tokenizer(TEMPLATE),
            tokenizer(include_str!("../../../infra/quickwit/pages-index.yaml"))
        );
    }

    #[test]
    fn a_page_sits_at_the_congress_place_dated_by_its_day() {
        let doc = congress_doc(&page(), Analyzer::LATEST);
        assert_eq!(doc["place_id"], PLACE_ID);
        assert_eq!(doc["state"], "DC");
        assert_eq!(doc["lccn"], "");
        assert_eq!(doc["front_page"], false);
        assert_eq!(doc["date"], "1846-07-21");
        assert_eq!(doc["year"], 1846);
        assert_eq!(doc["series"], "globe");
        assert_eq!(doc["states"], json!(["AL"]));
        assert_eq!(doc["state_via"], json!(["AL:speaker", "AL:title"]));
        assert_eq!(doc["congress"], 29);
        assert!(doc["text_cg"].as_str().unwrap().contains("of_alabama"));
    }

    #[test]
    fn absent_fields_are_left_out() {
        let mut p = page();
        p.title = None;
        p.congress = None;
        let doc = congress_doc(&p, Analyzer::LATEST);
        assert!(doc.get("title").is_none());
        assert!(doc.get("congress").is_none());
    }

    #[test]
    fn same_day_hits_order_by_series_unit_and_page() {
        let mut a = page();
        let mut b = page();
        b.seq = 301;
        assert!(sort_key(&a) < sort_key(&b));
        a.series = "serialset".into();
        assert!(sort_key(&b) < sort_key(&a));
        assert_eq!(index_id("pages-v20261010-1"), "congress-20261010-1");
    }
}
