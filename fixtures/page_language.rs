// The fixture pages with their own languages, for the tests of the
// page-language filter (05 §5.5.8, #289): the fixture's text is a handful
// of English words on every page, so the detector would leave most pages
// their title's languages. Instead each page gets languages by its title
// and a number from its day and page, as a release could find them: some
// of an English title's pages German or English and French, the English
// and German title's split between them, the Spanish title's half English.
// Included with `#[path]` by `crates/usnm-ingest/examples/page_language_fixture.rs`
// (which loads them into Quickwit for scripts/quickwit-fixtures.sh) and by
// the parity test (`crates/usnm-search/tests/quickwit_parity.rs`), so both
// backends see the same pages.

use serde_json::Value;

/// A fixture page's own languages, sorted.
pub fn languages(doc: &Value) -> Vec<String> {
    let lccn = doc["lccn"].as_str().expect("lccn");
    let n = doc["day"].as_u64().expect("day") * 7 + doc["seq"].as_u64().expect("seq");
    let langs: &[&str] = match lccn {
        "sn99000002" => match n % 3 {
            0 => &["ger"],
            1 => &["eng", "ger"],
            _ => &["eng"],
        },
        "sn99000003" if n.is_multiple_of(2) => &["eng"],
        "sn99000006" if n.is_multiple_of(5) => &["eng"],
        _ if n % 11 == 3 => &["eng", "fre"],
        _ if n % 13 == 5 => &["ger"],
        _ => return title_languages(doc),
    };
    langs.iter().map(|l| (*l).to_owned()).collect()
}

fn title_languages(doc: &Value) -> Vec<String> {
    let mut langs: Vec<String> = doc["language"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|l| l.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    langs.sort();
    langs.dedup();
    langs
}

/// `doc` (a fixture index document) with `page_language` and
/// `page_language_set`, as a release writes them.
pub fn add(doc: &mut Value) {
    let langs = languages(doc);
    doc["page_language_set"] = usnm_core::page_language::set_key(&langs).into();
    doc["page_language"] = langs.into();
}
