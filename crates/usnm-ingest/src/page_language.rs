//! Page languages at release time (05 §5.5.8, #289): each main-index
//! document's own languages, found from its words by
//! `usnm_core::page_language`, and the pages that moves to another set of
//! languages than its title's, which the page-language baselines count.
//!
//! The baselines come from each batch's `counts.json`, which has every page,
//! including those without text; the page language needs the text, which
//! only the documents have. So the release starts from the title-language
//! counts and moves each written page whose set differs from its title's
//! ([`Moves`]): one page off `(place, day, title set)`, one onto `(place,
//! day, page set)`. Pages without a document keep their title's set, which
//! is what the rules give them. The snapshot records the moves
//! ([`PAGE_LANGUAGES_FILE`]) so a delta, which counts every batch again,
//! can carry them forward.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use usnm_core::page_language::{self as pl, set_key, set_languages};
use usnm_core::time::day_number;

use crate::catalog::{Catalog, Title};
use crate::dedup::Hidden;

pub use usnm_state::state::{PAGE_LANGUAGES_FILE, PAGE_LANGUAGE_BASELINES_FILE};

/// Documents a thread detects at least, so a small part isn't split up.
const MIN_PER_THREAD: usize = 64;
/// Threads at most: the writer node indexes on the same machine.
const MAX_THREADS: usize = 4;

/// A title's languages as the language baselines count them: sorted,
/// without repeats or empty codes; `None` when it lists none.
pub fn title_set(title: &Title) -> Option<Vec<String>> {
    let set: BTreeSet<&str> = title
        .languages
        .iter()
        .map(String::as_str)
        .filter(|l| !l.is_empty())
        .collect();
    (!set.is_empty()).then(|| set.into_iter().map(str::to_owned).collect())
}

fn str_array(v: &Value) -> Vec<&str> {
    v.as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

/// A document's page languages, from its own fields: its title's languages
/// (`language`, in catalog order) and its text (`text`, LoC's or ours; on a
/// page with only American Stories' text, `text_as`).
pub fn languages_of(doc: &Value) -> Vec<String> {
    let title: Vec<&str> = str_array(&doc["language"])
        .into_iter()
        .filter(|l| !l.is_empty())
        .collect();
    let text = doc["text"]
        .as_str()
        .filter(|t| !t.is_empty())
        .or_else(|| doc["text_as"].as_str().filter(|t| !t.is_empty()));
    let detection = text.map(|t| pl::detect(t, &title));
    pl::page_languages(detection.as_ref(), &title)
}

/// Add `page_language` and `page_language_set` to one document.
pub fn annotate_one(doc: &mut Value) {
    let langs = languages_of(doc);
    doc[pl::SET_FIELD] = set_key(&langs).into();
    doc[pl::FIELD] = langs.into();
}

/// Add `page_language` and `page_language_set` to every document, on a few
/// threads: detection is about 0.5 µs a word (05 §5.5.8), the cost of a
/// long page being written.
pub fn annotate(docs: &mut [Value]) {
    let threads = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(MAX_THREADS)
        .min(docs.len() / MIN_PER_THREAD)
        .max(1);
    if threads == 1 {
        docs.iter_mut().for_each(annotate_one);
        return;
    }
    let chunk = docs.len().div_ceil(threads);
    std::thread::scope(|s| {
        for part in docs.chunks_mut(chunk) {
            s.spawn(|| part.iter_mut().for_each(annotate_one));
        }
    });
}

/// Pages per place and day for each set of languages: `{languages: {place:
/// {day: pages}}}`, what the language baselines are written from.
pub type BySet = BTreeMap<Vec<String>, BTreeMap<String, BTreeMap<u32, u32>>>;

/// A title's moved pages in one batch: `{page set: {date: [[edition, seq]]}}`.
pub type TitleMoves = BTreeMap<String, BTreeMap<String, Vec<[u16; 2]>>>;

/// The pages a version's main indexes hold in a set of page languages other
/// than their title's: `{batch: {lccn: {page set: {date: [[edition, seq]]}}}}`
/// (the copy's batch, as `duplicates.json` names it). [`PAGE_LANGUAGES_FILE`].
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Moves {
    /// `usnm_core::page_language::VERSION`.
    pub version: u32,
    pub batches: BTreeMap<String, BTreeMap<String, TitleMoves>>,
}

impl Moves {
    pub fn new() -> Self {
        Moves {
            version: pl::VERSION,
            batches: BTreeMap::new(),
        }
    }

    /// Record a written document ([`annotate`]d) if its page languages
    /// differ from its title's.
    pub fn record(&mut self, doc: &Value) -> anyhow::Result<()> {
        let page = str_array(&doc[pl::FIELD]);
        let title: BTreeSet<&str> = str_array(&doc["language"])
            .into_iter()
            .filter(|l| !l.is_empty())
            .collect();
        if page.iter().copied().eq(title.iter().copied()) {
            return Ok(());
        }
        let field = |name: &str| {
            doc[name]
                .as_str()
                .with_context(|| format!("a document without `{name}`: {}", doc["doc_id"]))
        };
        let number = |name: &str| {
            doc[name]
                .as_u64()
                .and_then(|n| u16::try_from(n).ok())
                .with_context(|| format!("a document without `{name}`: {}", doc["doc_id"]))
        };
        self.batches
            .entry(field("batch")?.to_owned())
            .or_default()
            .entry(field("lccn")?.to_owned())
            .or_default()
            .entry(set_key(&page))
            .or_default()
            .entry(field("date")?.to_owned())
            .or_default()
            .push([number("edition")?, number("seq")?]);
        Ok(())
    }

    /// Drop the moves of copies the version hides (`duplicates.json`).
    pub fn drop_hidden(&mut self, hidden: &[Hidden]) {
        for h in hidden {
            let Ok(key) = usnm_core::ids::PageKey::from_doc_id(&h.doc_id) else {
                continue;
            };
            let Some(titles) = self.batches.get_mut(&h.batch) else {
                continue;
            };
            let Some(sets) = titles.get_mut(&key.lccn) else {
                continue;
            };
            let date = key.date.to_string();
            for dates in sets.values_mut() {
                if let Some(pages) = dates.get_mut(&date) {
                    pages.retain(|p| *p != [key.edition, key.seq]);
                    if pages.is_empty() {
                        dates.remove(&date);
                    }
                }
            }
            sets.retain(|_, dates| !dates.is_empty());
            if sets.is_empty() {
                titles.remove(&key.lccn);
            }
            if titles.is_empty() {
                self.batches.remove(&h.batch);
            }
        }
    }

    /// Add `other`'s moves (a delta's own documents' to the published
    /// version's).
    pub fn extend(&mut self, other: Moves) {
        for (batch, titles) in other.batches {
            for (lccn, sets) in titles {
                for (set, dates) in sets {
                    for (date, pages) in dates {
                        let to = self
                            .batches
                            .entry(batch.clone())
                            .or_default()
                            .entry(lccn.clone())
                            .or_default()
                            .entry(set.clone())
                            .or_default()
                            .entry(date)
                            .or_default();
                        to.extend(pages);
                        to.sort_unstable();
                        to.dedup();
                    }
                }
            }
        }
    }

    /// Each moved page's set of page languages, by doc id
    /// (`lccn_date_ed-N_seq-M`), whichever batch's copy moved.
    pub fn sets_by_page(&self) -> std::collections::HashMap<String, &str> {
        let mut out = std::collections::HashMap::new();
        for titles in self.batches.values() {
            for (lccn, sets) in titles {
                for (set, dates) in sets {
                    for (date, pages) in dates {
                        for [ed, seq] in pages {
                            out.insert(format!("{lccn}_{date}_ed-{ed}_seq-{seq}"), set.as_str());
                        }
                    }
                }
            }
        }
        out
    }

    /// Give a page not in the main indexes' documents (the Japanese index's,
    /// 05 §5.5.8) the languages the page-language baselines count it in: its
    /// moved set if a main-index document of it moved, else its title's.
    /// `by_page` is [`Moves::sets_by_page`].
    pub fn annotate_as_counted(
        doc: &mut Value,
        title: &Title,
        by_page: &std::collections::HashMap<String, &str>,
    ) {
        let key = match doc["doc_id"].as_str().and_then(|id| by_page.get(id)) {
            Some(set) => (*set).to_owned(),
            None => set_key(&title_set(title).unwrap_or_default()),
        };
        let langs: Vec<String> = set_languages(&key).map(str::to_owned).collect();
        doc[pl::SET_FIELD] = key.into();
        doc[pl::FIELD] = langs.into();
    }

    /// Pages that move.
    pub fn pages(&self) -> u64 {
        self.batches
            .values()
            .flat_map(BTreeMap::values)
            .flat_map(BTreeMap::values)
            .flat_map(BTreeMap::values)
            .map(|p| p.len() as u64)
            .sum()
    }

    /// The page-language counts: `by_language` (the title-language counts,
    /// `{title set: {place: {day: pages}}}`) with every move applied.
    pub fn apply(&self, by_language: &BySet, catalog: &Catalog) -> anyhow::Result<BySet> {
        let mut out = by_language.clone();
        for titles in self.batches.values() {
            for (lccn, sets) in titles {
                let title = catalog.title(lccn).with_context(|| {
                    format!("a moved page of `{lccn}`, which the catalog lacks")
                })?;
                let from = title_set(title);
                for (set, dates) in sets {
                    let to: Vec<String> = set_languages(set).map(str::to_owned).collect();
                    if from.as_ref().map_or(to.is_empty(), |f| *f == to) {
                        continue;
                    }
                    for (date, pages) in dates {
                        let day = day_number(
                            date.parse()
                                .with_context(|| format!("a moved page's date `{date}`"))?,
                        );
                        let n = pages.len() as u32;
                        if let Some(from) = &from {
                            let count = out
                                .get_mut(from)
                                .and_then(|places| places.get_mut(&title.place_id))
                                .and_then(|days| days.get_mut(&day))
                                .filter(|c| **c >= n);
                            let Some(count) = count else {
                                bail!(
                                    "{n} pages of `{lccn}` on {date} move from {from:?}, which \
                                     counts fewer"
                                );
                            };
                            *count -= n;
                        }
                        if !to.is_empty() {
                            *out.entry(to.clone())
                                .or_default()
                                .entry(title.place_id.clone())
                                .or_default()
                                .entry(day)
                                .or_default() += n;
                        }
                    }
                }
            }
        }
        for places in out.values_mut() {
            for days in places.values_mut() {
                days.retain(|_, n| *n > 0);
            }
            places.retain(|_, days| !days.is_empty());
        }
        out.retain(|_, places| !places.is_empty());
        Ok(out)
    }

    /// What the release logs and the manifest records of them.
    pub fn summary(&self) -> Value {
        json!({ "version": self.version, "pages": self.pages() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(batch: &str, date: &str, seq: u64, langs: &[&str], text: &str) -> Value {
        json!({
            "doc_id": format!("sn12345678_{date}_ed-1_seq-{seq}"),
            "batch": batch,
            "lccn": "sn12345678",
            "date": date,
            "edition": 1,
            "seq": seq,
            "language": langs,
            "text": text,
        })
    }

    const GERMAN: &str = "Der Stadtrath hat am Dienstag beschlossen, dass die neue Schule auf dem \
        Grundstück gebaut werden soll, welches die Behörde im vorigen Jahre gekauft hat. Es ist \
        noch nicht bekannt, wann die Arbeit beginnen wird, aber alle sind der Meinung, dass es \
        eine gute Sache für die Stadt und für ihre Bürger ist.";
    const ENGLISH: &str = "The council of the city met on Tuesday evening at the court house, \
        and after a long debate it was voted that the new school should be built on the lot \
        which the board had bought last year. The mayor said that he would not sign the bill.";

    #[test]
    fn documents_get_their_page_languages() {
        let mut docs = vec![
            doc("b1", "1890-01-01", 1, &["eng", "ger"], GERMAN),
            doc("b1", "1890-01-01", 2, &["eng", "ger"], "short"),
            doc("b1", "1890-01-01", 3, &["dak", "eng"], ENGLISH),
            doc("b1", "1890-01-01", 4, &[], ENGLISH),
            doc("b1", "1890-01-01", 5, &[], "short"),
        ];
        // Only American Stories' text: detected on that.
        let mut as_only = doc("b1", "1890-01-01", 6, &["eng", "ger"], "");
        as_only["text_as"] = GERMAN.into();
        docs.push(as_only);
        annotate(&mut docs);
        let got: Vec<(Value, Value)> = docs
            .iter()
            .map(|d| (d[pl::FIELD].clone(), d[pl::SET_FIELD].clone()))
            .collect();
        assert_eq!(
            got,
            [
                (json!(["ger"]), json!("ger")),
                (json!(["eng", "ger"]), json!("eng_ger")),
                (json!(["dak", "eng"]), json!("dak_eng")),
                (json!(["eng"]), json!("eng")),
                (json!([]), json!("none")),
                (json!(["ger"]), json!("ger")),
            ]
        );
    }

    #[test]
    fn many_documents_are_detected_on_threads_alike() {
        let mut many: Vec<Value> = (1..=300)
            .map(|i| {
                let text = if i % 3 == 0 { GERMAN } else { ENGLISH };
                doc("b1", "1890-01-01", i, &["eng", "ger"], text)
            })
            .collect();
        let mut one_by_one = many.clone();
        annotate(&mut many);
        one_by_one.iter_mut().for_each(annotate_one);
        assert_eq!(many, one_by_one);
    }

    fn annotated(batch: &str, date: &str, seq: u64, langs: &[&str], text: &str) -> Value {
        let mut d = doc(batch, date, seq, langs, text);
        annotate_one(&mut d);
        d
    }

    #[test]
    fn only_pages_whose_languages_differ_from_their_title_move() {
        let mut m = Moves::new();
        for d in [
            annotated("b1", "1890-01-01", 1, &["ger", "eng"], GERMAN),
            annotated("b1", "1890-01-01", 2, &["ger", "eng"], "short"),
            annotated("b1", "1890-01-02", 1, &["ger"], GERMAN),
            annotated("b1", "1890-01-02", 2, &["ger"], ENGLISH),
            annotated("b2", "1890-01-03", 1, &[], ENGLISH),
        ] {
            m.record(&d).unwrap();
        }
        assert_eq!(m.pages(), 3);
        assert_eq!(
            serde_json::to_value(&m).unwrap(),
            json!({"version": pl::VERSION, "batches": {
                "b1": {"sn12345678": {
                    "ger": {"1890-01-01": [[1, 1]]},
                    "eng": {"1890-01-02": [[1, 2]]},
                }},
                "b2": {"sn12345678": {"eng": {"1890-01-03": [[1, 1]]}}},
            }})
        );
        let back: Moves = serde_json::from_value(serde_json::to_value(&m).unwrap()).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn hidden_copies_lose_their_moves_and_a_delta_adds_its_own() {
        let mut published = Moves::new();
        published
            .record(&annotated("b1", "1890-01-01", 1, &["ger", "eng"], GERMAN))
            .unwrap();
        published
            .record(&annotated("b1", "1890-01-01", 2, &["ger", "eng"], GERMAN))
            .unwrap();
        // The delta's batch b2 has a copy of seq 1 that wins: b1's is hidden.
        published.drop_hidden(&[
            Hidden {
                doc_id: "sn12345678_1890-01-01_ed-1_seq-1".into(),
                batch: "b1".into(),
            },
            // Another batch's copy of the same page: not b1's, kept.
            Hidden {
                doc_id: "sn12345678_1890-01-01_ed-1_seq-2".into(),
                batch: "b9".into(),
            },
        ]);
        assert_eq!(published.pages(), 1);
        let mut delta = Moves::new();
        delta
            .record(&annotated("b2", "1890-01-01", 1, &["ger", "eng"], GERMAN))
            .unwrap();
        published.extend(delta.clone());
        published.extend(delta);
        assert_eq!(published.pages(), 2, "a move counts once");
        published.drop_hidden(&[
            Hidden {
                doc_id: "sn12345678_1890-01-01_ed-1_seq-2".into(),
                batch: "b1".into(),
            },
            Hidden {
                doc_id: "sn12345678_1890-01-01_ed-1_seq-1".into(),
                batch: "b2".into(),
            },
        ]);
        assert_eq!(published, Moves::new());
    }

    fn catalog() -> Catalog {
        let title = |lccn: &str, ordinal: u32, langs: &[&str]| {
            serde_json::from_value::<Title>(json!({
                "lccn": lccn, "name": lccn, "ordinal": ordinal, "place_id": "p1", "state": "Ohio",
                "languages": langs,
            }))
            .unwrap()
        };
        let place: crate::catalog::Place = serde_json::from_value(json!({
            "id": "p1", "ordinal": 0, "name": "Cincinnati", "state": "Ohio",
            "lat": 39.1, "lon": -84.5, "precision": "city",
        }))
        .unwrap();
        Catalog::new(
            vec![
                title("sn12345678", 1, &["ger", "eng"]),
                title("sn00000001", 2, &[]),
            ],
            vec![place],
        )
        .unwrap()
    }

    #[test]
    fn moves_shift_pages_between_sets() {
        let catalog = catalog();
        let day = |d: &str| day_number(d.parse().unwrap());
        let mut by_language = BTreeMap::new();
        by_language.insert(
            vec!["eng".to_owned(), "ger".to_owned()],
            BTreeMap::from([(
                "p1".to_owned(),
                BTreeMap::from([(day("1890-01-01"), 3), (day("1890-01-02"), 1)]),
            )]),
        );
        let mut m = Moves::new();
        for d in [
            annotated("b1", "1890-01-01", 1, &["ger", "eng"], GERMAN),
            annotated("b1", "1890-01-01", 2, &["ger", "eng"], ENGLISH),
            annotated("b1", "1890-01-02", 1, &["ger", "eng"], GERMAN),
        ] {
            m.record(&d).unwrap();
        }
        let mut no_language = doc("b1", "1890-01-02", 3, &[], ENGLISH);
        no_language["lccn"] = "sn00000001".into();
        annotate_one(&mut no_language);
        m.record(&no_language).unwrap();
        let got = m.apply(&by_language, &catalog).unwrap();
        let set = |s: &[&str]| s.iter().map(|l| (*l).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            got[&set(&["eng", "ger"])]["p1"],
            BTreeMap::from([(day("1890-01-01"), 1)])
        );
        assert_eq!(
            got[&set(&["ger"])]["p1"],
            BTreeMap::from([(day("1890-01-01"), 1), (day("1890-01-02"), 1)])
        );
        // A title with no languages: its page joins English, from nowhere.
        assert_eq!(
            got[&set(&["eng"])]["p1"],
            BTreeMap::from([(day("1890-01-01"), 1), (day("1890-01-02"), 1)])
        );
        // Every page is counted once, before and after.
        let total = |b: &BySet| -> u32 {
            b.values()
                .flat_map(BTreeMap::values)
                .flat_map(BTreeMap::values)
                .sum()
        };
        assert_eq!(total(&got), total(&by_language) + 1);
        // More moves than the counts hold: an error, not a negative count.
        let mut too_many = m.clone();
        too_many
            .record(&annotated("b1", "1890-01-02", 2, &["ger", "eng"], GERMAN))
            .unwrap();
        assert!(too_many.apply(&by_language, &catalog).is_err());
    }
}
