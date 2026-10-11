//! End to end on the synthetic corpus: turn the fixture pages back into two
//! LoC-style batch archives, run enqueue → curate → release twice (a base,
//! then a delta), and check the pipeline reproduces the checked-in fixtures
//! exactly: both indexes, the baselines, titles, places and pages per title.
//! Then load the result the way the API does.

use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{NaiveDate, TimeZone, Utc};
use serde_json::Value;
use usnm_core::text::Analyzer;
use usnm_core::time::date_from_day;
use usnm_ingest::docs::MemoryDocs;
use usnm_ingest::release::{Published, Release};
use usnm_ingest::sink::JsonlSink;
use usnm_ingest::source::{self, ListedBatch};
use usnm_ingest::state::{BatchStatus, State};
use usnm_ingest::worker::Worker;
use usnm_store::{LocalStore, ObjectStore};

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/data")
}

fn read_jsonl(path: &Path) -> Vec<Value> {
    std::io::BufReader::new(std::fs::File::open(path).unwrap())
        .lines()
        .map(|l| serde_json::from_str(&l.unwrap()).unwrap())
        .collect()
}

/// Documents keyed by id, without the provenance field the fixtures lack.
fn by_id(docs: Vec<Value>) -> BTreeMap<String, Value> {
    docs.into_iter()
        .map(|mut d| {
            d.as_object_mut().unwrap().remove("batch");
            (d["doc_id"].as_str().unwrap().to_owned(), d)
        })
        .collect()
}

/// A fixture index as it is, American Stories' text included (05 §5.5.4).
fn fixture(index_id: &str) -> BTreeMap<String, Value> {
    by_id(read_jsonl(
        &fixtures().join(format!("indexes/{index_id}.jsonl")),
    ))
}

/// A fixture index as the release builds it from LoC's text alone, without
/// `--american-stories`: without American Stories' text (`text_as`) and
/// the pages only it has text for (#218, 05 §5.5.4).
fn loc_fixture(index_id: &str) -> BTreeMap<String, Value> {
    let docs = read_jsonl(&fixtures().join(format!("indexes/{index_id}.jsonl")));
    by_id(
        docs.into_iter()
            .filter(|d| d["text"].as_str() != Some(""))
            .map(|mut d| {
                d.as_object_mut().unwrap().remove("text_as");
                d
            })
            .collect(),
    )
}

#[path = "../../../fixtures/decades.rs"]
#[allow(dead_code)]
mod decades;

/// `pages` moved over several decades as `fixtures/decades.rs` moves the
/// fixture indexes' (05 §5.5.5): the 1830s, 1840s, 1860s and 1890s.
fn over_decades(pages: &[&Page]) -> Vec<Page> {
    use chrono::Datelike;
    pages
        .iter()
        .map(|p| Page {
            lccn: p.lccn.clone(),
            date: p
                .date
                .with_year(p.date.year() - decades::years_back(&p.lccn))
                .unwrap(),
            seq: p.seq,
            text: p.text.clone(),
        })
        .collect()
}

struct Page {
    lccn: String,
    date: NaiveDate,
    seq: u16,
    text: String,
}

/// Every fixture page, including the empty ones (which are in the baselines
/// but not the indexes).
fn fixture_pages() -> Vec<Page> {
    let dir = fixtures();
    let titles: Vec<Value> =
        serde_json::from_slice(&std::fs::read(dir.join("fixture-v1/titles.json")).unwrap())
            .unwrap();
    let lccn_of: BTreeMap<String, String> = titles
        .iter()
        .map(|t| {
            (
                t["place_id"].as_str().unwrap().into(),
                t["lccn"].as_str().unwrap().into(),
            )
        })
        .collect();
    let mut text: BTreeMap<String, String> = BTreeMap::new();
    for f in ["pages-base-fixture", "pages-delta-fixture-1"] {
        for d in read_jsonl(&dir.join(format!("indexes/{f}.jsonl"))) {
            text.insert(
                d["doc_id"].as_str().unwrap().into(),
                d["text"].as_str().unwrap().into(),
            );
        }
    }
    let baselines: BTreeMap<String, Vec<(u32, u32)>> =
        serde_json::from_slice(&std::fs::read(dir.join("fixture-v1/baselines.json")).unwrap())
            .unwrap();
    let mut pages = Vec::new();
    for (place, series) in baselines {
        let lccn = &lccn_of[&place];
        for (day, n) in series {
            let date = date_from_day(day);
            for seq in 1..=u16::try_from(n).unwrap() {
                let id = format!("{lccn}_{date}_ed-1_seq-{seq}");
                pages.push(Page {
                    lccn: lccn.clone(),
                    date,
                    seq,
                    text: text.get(&id).cloned().unwrap_or_default(),
                });
            }
        }
    }
    pages
}

/// A batch archive in one of the two path layouts, compressed.
fn write_archive(path: &Path, pages: &[&Page], historical_layout: bool, gzip: bool) {
    let mut tar = tar::Builder::new(Vec::new());
    for p in pages {
        let name = if historical_layout {
            format!(
                "{}/{}/ed-1/seq-{}/ocr.txt",
                p.lccn,
                p.date.format("%Y/%m/%d"),
                p.seq
            )
        } else {
            format!("batch/{}/{}/ed-1/seq-{}/ocr.txt", p.lccn, p.date, p.seq)
        };
        let mut h = tar::Header::new_gnu();
        h.set_size(p.text.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        tar.append_data(&mut h, &name, p.text.as_bytes()).unwrap();
        // Other files in the batch are skipped.
        let xml = name.replace("ocr.txt", "ocr.xml");
        let mut h = tar::Header::new_gnu();
        h.set_size(7);
        h.set_cksum();
        tar.append_data(&mut h, &xml, &b"<alto/>"[..]).unwrap();
    }
    let raw = tar.into_inner().unwrap();
    let data = if gzip {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&raw).unwrap();
        gz.finish().unwrap()
    } else {
        let mut bz = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::fast());
        bz.write_all(&raw).unwrap();
        bz.finish().unwrap()
    };
    std::fs::write(path, data).unwrap();
}

struct Env {
    _dir: tempfile::TempDir,
    root: PathBuf,
    state: State,
    curated: Arc<dyn ObjectStore>,
    reference: Arc<LocalStore>,
}

async fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_owned();
    let reference = Arc::new(LocalStore::new(root.join("reference")));
    // The catalog: the fixture titles and places.
    for (from, to) in [
        ("titles.json", "catalog/titles.json"),
        ("places.json", "catalog/places.json"),
    ] {
        let bytes = std::fs::read(fixtures().join("fixture-v1").join(from)).unwrap();
        reference.put(to, bytes, "application/json").await.unwrap();
    }
    Env {
        state: State::new(Arc::new(MemoryDocs::default())),
        curated: Arc::new(LocalStore::new(root.join("curated"))),
        reference,
        root,
        _dir: dir,
    }
}

impl Env {
    fn worker(&self, owner: &str) -> Worker {
        Worker {
            state: self.state.clone(),
            curated: self.curated.clone(),
            owner: owner.into(),
            lease: chrono::Duration::hours(2),
            fetch_interval: None,
            batch_limit: usnm_ingest::worker::BATCH_LIMIT,
            deadline: None,
            raw: None,
        }
    }

    async fn release(&self, day: u32, full: bool) -> Option<Published> {
        self.release_with(day, full, false).await.unwrap()
    }

    /// A release with or without `--partition-decade` (05 §5.5.5).
    async fn release_by_decade(&self, day: u32, full: bool, partition_decade: bool) -> Published {
        let r = Release {
            partition_decade,
            ..self.releaser(day, full)
        };
        let mut sink = JsonlSink::new(self.root.join("reference/indexes"));
        r.run(&mut sink).await.unwrap().unwrap()
    }

    fn releaser(&self, day: u32, full: bool) -> Release {
        Release {
            state: self.state.clone(),
            curated: self.curated.clone(),
            reference: self.reference.clone(),
            owner: "releaser".into(),
            full,
            synthetic: true,
            now: Utc.with_ymd_and_hms(2026, 10, day, 3, 0, 0).unwrap(),
            titles_left: None,
            american_stories: false,
            ja_latin: false,
            partition_decade: false,
        }
    }

    /// A release with or without American Stories' text (`--american-stories`).
    async fn release_with(
        &self,
        day: u32,
        full: bool,
        american_stories: bool,
    ) -> anyhow::Result<Option<Published>> {
        self.release_opts(day, full, american_stories, false).await
    }

    /// A release with or without our OCR's Latin text (`--ja-latin`, #203).
    async fn release_latin(&self, day: u32, full: bool, ja_latin: bool) -> Option<Published> {
        self.release_opts(day, full, false, ja_latin).await.unwrap()
    }

    async fn release_opts(
        &self,
        day: u32,
        full: bool,
        american_stories: bool,
        ja_latin: bool,
    ) -> anyhow::Result<Option<Published>> {
        let r = Release {
            state: self.state.clone(),
            curated: self.curated.clone(),
            reference: self.reference.clone(),
            owner: "releaser".into(),
            full,
            synthetic: true,
            now: Utc.with_ymd_and_hms(2026, 10, day, 3, 0, 0).unwrap(),
            titles_left: None,
            american_stories,
            ja_latin,
            partition_decade: false,
        };
        let mut sink = JsonlSink::new(self.root.join("reference/indexes"));
        r.run(&mut sink).await
    }

    /// An index's documents. A main index's are each checked for their
    /// common-word pairs (`text_cg`, 05 §5.5.3, and `text_as_cg` for
    /// American Stories' text, 05 §5.5.4) and returned without them, as the
    /// fixtures hold; the Japanese pages' index has none.
    fn index(&self, id: &str) -> BTreeMap<String, Value> {
        self.index_as(id, Analyzer::LATEST)
    }

    /// [`Env::index`] for an index whose pairs `analyzer` folded (#168).
    fn index_as(&self, id: &str, analyzer: Analyzer) -> BTreeMap<String, Value> {
        let mut docs = by_id(read_jsonl(
            &self.root.join(format!("reference/indexes/{id}.jsonl")),
        ));
        if id.starts_with("pages-ja-") {
            assert!(docs.values().all(|d| d.get("text_cg").is_none()), "{id}");
            return docs;
        }
        for (doc_id, doc) in &mut docs {
            let pairs = doc
                .as_object_mut()
                .unwrap()
                .remove("text_cg")
                .unwrap_or_else(|| panic!("{doc_id} has no text_cg"));
            let want = doc["text"]
                .as_str()
                .map(|t| usnm_core::common_grams::index_text(t, analyzer));
            assert_eq!(pairs.as_str().map(str::to_owned), want, "{doc_id}");
            let as_pairs = doc.as_object_mut().unwrap().remove("text_as_cg");
            let want = doc["text_as"]
                .as_str()
                .map(|t| usnm_core::common_grams::index_text(t, analyzer));
            assert_eq!(
                as_pairs.as_ref().and_then(Value::as_str).map(str::to_owned),
                want,
                "{doc_id}"
            );
        }
        docs
    }

    async fn reference_json(&self, path: &str) -> Value {
        serde_json::from_slice(&self.reference.get(path).await.unwrap().unwrap()).unwrap()
    }
}

/// An index run item as stored, untyped.
async fn raw_run(e: &Env, version: &str) -> Value {
    e.state
        .docs
        .get("index_runs", version, version)
        .await
        .unwrap()
        .unwrap()
        .doc
}

fn listed(name: &str, path: &Path, sha256: Option<String>) -> ListedBatch {
    ListedBatch {
        name: name.into(),
        url: path.to_str().unwrap().into(),
        sha256,
        ocr_source: None,
        lccns: vec![],
    }
}

fn sha256_file(path: &Path) -> String {
    use sha2::Digest;
    source::hex(&sha2::Sha256::digest(std::fs::read(path).unwrap()))
}

#[tokio::test]
async fn a_version_without_these_common_word_pairs_is_rebuilt_in_full() {
    let e = env().await;
    let pages = fixture_pages();
    let split = NaiveDate::from_ymd_opt(1897, 7, 1).unwrap();
    let (early, late): (Vec<&Page>, Vec<&Page>) = pages.iter().partition(|p| p.date < split);
    let base_archive = e.root.join("batch_fx_early_ver01.tar.gz");
    let delta_archive = e.root.join("batch_fx_late_ver01.tar.bz2");
    write_archive(&base_archive, &early, true, true);
    write_archive(&delta_archive, &late, false, false);
    let list = [listed("batch_fx_early_ver01", &base_archive, None)];
    source::enqueue(&e.state, &list).await.unwrap();
    e.worker("w1").run(None).await.unwrap();
    assert!(e.release(1, false).await.unwrap().full);
    // The published version names the pairs its indexes hold (05 §5.5.3).
    let mut pointer = e.reference_json("current.json").await;
    assert_eq!(pointer["common_grams"], usnm_core::common_grams::VERSION);

    // A version published before the pairs (or with another list): the
    // next release rebuilds every index rather than add a delta to it.
    pointer.as_object_mut().unwrap().remove("common_grams");
    e.reference
        .put(
            "current.json",
            serde_json::to_vec(&pointer).unwrap(),
            "application/json",
        )
        .await
        .unwrap();
    let list = [listed("batch_fx_late_ver01", &delta_archive, None)];
    source::enqueue(&e.state, &list).await.unwrap();
    e.worker("w2").run(None).await.unwrap();
    let p2 = e.release(8, false).await.unwrap();
    assert!(
        p2.full,
        "a delta would mix indexes with and without the pairs"
    );
    assert_eq!(p2.indexes.len(), 1);
    let mut pointer = e.reference_json("current.json").await;
    assert_eq!(pointer["common_grams"], usnm_core::common_grams::VERSION);

    // Pairs from a newer release than this code knows: rebuilt too.
    pointer["common_grams"] = (usnm_core::common_grams::VERSION + 1).into();
    e.reference
        .put(
            "current.json",
            serde_json::to_vec(&pointer).unwrap(),
            "application/json",
        )
        .await
        .unwrap();
    let p3 = e.release(15, false).await.unwrap();
    assert!(p3.full, "pairs this release doesn't know");
    let pointer = e.reference_json("current.json").await;
    assert_eq!(pointer["common_grams"], usnm_core::common_grams::VERSION);
}

/// A delta on a version built with an older analyzer folds its words as
/// that version's indexes do (#168): its pairs are at the published
/// version's, which the pointer and the build record keep naming, so every
/// index of the version tokenizes alike. A full rebuild moves the version
/// to the latest.
#[tokio::test]
async fn a_delta_folds_its_words_as_the_published_version_does() {
    let e = env().await;
    let pages = fixture_pages();
    // The market reports with fractions are on pages of March 1896.
    let split = NaiveDate::from_ymd_opt(1896, 3, 1).unwrap();
    let (early, late): (Vec<&Page>, Vec<&Page>) = pages.iter().partition(|p| p.date < split);
    let base_archive = e.root.join("batch_fx_early_ver01.tar.gz");
    let delta_archive = e.root.join("batch_fx_late_ver01.tar.bz2");
    write_archive(&base_archive, &early, true, true);
    write_archive(&delta_archive, &late, false, false);
    let list = [listed("batch_fx_early_ver01", &base_archive, None)];
    source::enqueue(&e.state, &list).await.unwrap();
    e.worker("w1").run(None).await.unwrap();
    assert!(e.release(1, false).await.unwrap().full);

    // As if the base had been built before #168, with version 1.
    let mut pointer = e.reference_json("current.json").await;
    pointer["common_grams"] = 1.into();
    e.reference
        .put(
            "current.json",
            serde_json::to_vec(&pointer).unwrap(),
            "application/json",
        )
        .await
        .unwrap();
    let list = [listed("batch_fx_late_ver01", &delta_archive, None)];
    source::enqueue(&e.state, &list).await.unwrap();
    e.worker("w2").run(None).await.unwrap();
    let p2 = e.release(8, false).await.unwrap();
    assert!(
        !p2.full,
        "a version with pairs this release knows takes a delta"
    );
    let delta = &p2.indexes[1];
    // Every document's pairs are version 1's (`Env::index_as` checks).
    let docs = e.index_as(delta, Analyzer::V1);
    assert_eq!(
        docs.len(),
        loc_fixture("pages-delta-fixture-1").len() + {
            let base = loc_fixture("pages-base-fixture");
            base.values()
                .filter(|d| d["date"].as_str().unwrap() >= "1896-03-01")
                .count()
        }
    );
    let raw = by_id(read_jsonl(
        &e.root.join(format!("reference/indexes/{delta}.jsonl")),
    ));
    let market = &raw["sn99000001_1896-03-07_ed-1_seq-2"];
    assert!(market["text"].as_str().unwrap().contains("½ higher at 61¼"));
    let pairs = market["text_cg"].as_str().unwrap();
    assert!(
        pairs.contains("1\u{2044}2 higher at_611\u{2044}4 611\u{2044}4") && !pairs.contains('½'),
        "{pairs}"
    );
    let pointer = e.reference_json("current.json").await;
    assert_eq!(pointer["common_grams"], 1);
    let manifest = e
        .reference_json(&format!("{}/manifest.json", p2.index_version))
        .await;
    assert_eq!(manifest["build"]["features"]["common_grams"], 1);
    assert_eq!(manifest["build"]["features"]["ja_fold"], 1);

    // A full rebuild builds every index with the latest analyzer.
    let p3 = e.release(15, true).await.unwrap();
    assert!(p3.full);
    let pointer = e.reference_json("current.json").await;
    assert_eq!(pointer["common_grams"], usnm_core::common_grams::VERSION);
    let docs = e.index(&p3.indexes[0]);
    assert!(docs.contains_key("sn99000001_1896-03-07_ed-1_seq-2"));
    let raw = by_id(read_jsonl(
        &e.root
            .join(format!("reference/indexes/{}.jsonl", p3.indexes[0])),
    ));
    let pairs = raw["sn99000001_1896-03-07_ed-1_seq-2"]["text_cg"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(pairs.contains("½ higher at_61¼ 61¼"), "{pairs}");
}

#[tokio::test]
async fn reproduces_the_fixture_corpus_as_a_base_and_a_delta() {
    let e = env().await;
    let pages = fixture_pages();
    let split = NaiveDate::from_ymd_opt(1897, 7, 1).unwrap();
    let (early, late): (Vec<&Page>, Vec<&Page>) = pages.iter().partition(|p| p.date < split);
    let base_archive = e.root.join("batch_fx_early_ver01.tar.gz");
    let delta_archive = e.root.join("batch_fx_late_ver01.tar.bz2");
    write_archive(&base_archive, &early, true, true);
    write_archive(&delta_archive, &late, false, false);

    // Week 1: one batch → a base index.
    let list = [listed(
        "batch_fx_early_ver01",
        &base_archive,
        Some(sha256_file(&base_archive)),
    )];
    assert_eq!(source::enqueue(&e.state, &list).await.unwrap().new, 1);
    assert_eq!(e.worker("w1").run(None).await.unwrap(), 1);
    let p1 = e.release(1, false).await.unwrap();
    assert!(p1.full, "the first release is a base");
    assert_eq!(p1.indexes, ["pages-base-20261001-1"]);

    let want_base = loc_fixture("pages-base-fixture");
    assert_eq!(e.index("pages-base-20261001-1"), want_base);
    assert_eq!(p1.docs, want_base.len() as u64);

    // Nothing new (a weekly run when LoC published nothing): the listing
    // queues nothing, the worker finds nothing to claim, and nothing is released.
    let again = source::enqueue(&e.state, &list).await.unwrap();
    assert_eq!((again.new, again.unchanged), (0, 1));
    assert_eq!(e.worker("w1").run(None).await.unwrap(), 0);
    assert!(e.release(2, false).await.is_none());

    // Week 2: the second batch → a delta on top of the base.
    let list = [listed("batch_fx_late_ver01", &delta_archive, None)];
    assert_eq!(source::enqueue(&e.state, &list).await.unwrap().new, 1);
    assert_eq!(e.worker("w2").run(None).await.unwrap(), 1);
    let p2 = e.release(8, false).await.unwrap();
    assert!(!p2.full);
    assert_eq!(
        p2.indexes,
        ["pages-base-20261001-1", "pages-delta-20261008-1"]
    );
    let want_delta = loc_fixture("pages-delta-fixture-1");
    assert_eq!(e.index("pages-delta-20261008-1"), want_delta);

    // The reference snapshot matches the fixture's, file for file.
    let v = &p2.index_version;
    for f in [
        "baselines.json",
        "language_baselines.json",
        "titles.json",
        "places.json",
        "title_pages.json",
    ] {
        let want: Value =
            serde_json::from_slice(&std::fs::read(fixtures().join("fixture-v1").join(f)).unwrap())
                .unwrap();
        assert_eq!(e.reference_json(&format!("{v}/{f}")).await, want, "{f}");
    }
    let current = e.reference_json("current.json").await;
    assert_eq!(current["index_version"], v.as_str());
    assert_eq!(current["previous_version"], p1.index_version.as_str());
    assert_eq!(current["backend"], "memory");
    assert_eq!(current["bounds"]["from"], "1895-01-05");
    assert_eq!(current["synthetic"], true);
    // Stamped when the pointer was written, not when the run started (#119):
    // the same instant the run records as its publish.
    assert_ne!(current["published_at"], "2026-10-08T03:00:00Z");
    let run_published: chrono::DateTime<Utc> =
        serde_json::from_value(raw_run(&e, v).await["published_at"].clone()).unwrap();
    assert_eq!(
        current["published_at"],
        run_published.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    );

    // The run item points at the batch list in the snapshot instead of
    // carrying it, and the manifest checksums the list like its other files.
    let item = raw_run(&e, v).await;
    assert!(item.get("batches").is_none(), "{item}");
    assert_eq!(item["batch_count"], 2);
    assert_eq!(item["batch_list"], format!("{v}/batches.json"));
    let listed = e.reference_json(&format!("{v}/batches.json")).await;
    assert_eq!(
        listed
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["batch"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["batch_fx_early", "batch_fx_late"]
    );
    let manifest = e.reference_json(&format!("{v}/manifest.json")).await;
    assert!(manifest["files"]
        .as_array()
        .unwrap()
        .iter()
        .any(|f| f["path"] == "batches.json"));
    // What built it (#161): the manifest has the templates in full, the run
    // item their checksums only.
    let build = &manifest["build"];
    assert_eq!(
        build["features"]["common_grams"],
        usnm_core::common_grams::VERSION
    );
    assert_eq!(
        build["templates"]["pages"]["yaml"],
        usnm_ingest::sink::INDEX_TEMPLATE
    );
    assert_eq!(
        item["build"]["templates"]["pages"]["sha256"],
        build["templates"]["pages"]["sha256"]
    );
    assert!(item["build"]["templates"]["pages"].get("yaml").is_none());
    assert_eq!(item["build"]["full"], build["full"]);
    // The memory sink has no engine to name.
    assert_eq!(build["engine"], serde_json::Value::Null);

    // The API loads it: checksums, manifest and version pairing all hold.
    let refdata = usnm_api::refdata::RefData::load(e.reference.as_ref())
        .await
        .unwrap();
    assert_eq!(refdata.version(), v);
    assert_eq!(refdata.places.len(), 6);
    // Pages per title count the same pages as the baselines.
    let title_pages = refdata.title_pages.as_ref().unwrap();
    assert_eq!(title_pages.len(), 6);
    assert_eq!(title_pages.values().sum::<u64>(), refdata.pages);

    // A full release (compaction) rebuilds one base with every page.
    let p3 = e.release(15, true).await.unwrap();
    assert_eq!(p3.indexes, ["pages-base-20261015-1"]);
    let mut both = want_base.clone();
    both.extend(want_delta.clone());
    assert_eq!(e.index("pages-base-20261015-1"), both);

    // Published indexes and snapshots are never rewritten.
    assert!(e
        .reference
        .get(&format!("{}/manifest.json", p1.index_version))
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn failed_and_leased_batches_are_retried_not_lost() {
    let e = env().await;
    let pages = fixture_pages();
    let few: Vec<&Page> = pages.iter().take(20).collect();
    let archive = e.root.join("batch_fx_small_ver01.tar.gz");
    write_archive(&archive, &few, true, true);

    // A wrong checksum fails the attempt and re-queues the batch.
    let bad = [listed(
        "batch_fx_small_ver01",
        &archive,
        Some("0".repeat(64)),
    )];
    source::enqueue(&e.state, &bad).await.unwrap();
    assert_eq!(e.worker("w1").run(None).await.unwrap(), 0);
    let (b, _) = e.state.batch("batch_fx_small").await.unwrap().unwrap();
    assert_eq!((b.status, b.attempts), (BatchStatus::Queued, 1));
    assert!(b.last_error.as_deref().unwrap().contains("sha256"));
    assert!(b.curated.is_none() && b.lease.is_none());

    // A claimed batch is invisible to other workers until its lease expires.
    let mut b2 = b.clone();
    b2.source_sha256 = None;
    let (_, etag) = e.state.batch("batch_fx_small").await.unwrap().unwrap();
    e.state.replace_batch(&b2, &etag).await.unwrap().unwrap();
    let w1 = e.worker("w1");
    let (claimed, _) = w1.claim().await.unwrap().unwrap();
    assert!(e.worker("w2").claim().await.unwrap().is_none());

    // A worker that lost its lease can't commit.
    let c = w1.curate(&claimed).await.unwrap();
    assert!(e
        .worker("w2")
        .commit("batch_fx_small", c.clone())
        .await
        .is_err());
    w1.commit("batch_fx_small", c).await.unwrap();
    let (b, _) = e.state.batch("batch_fx_small").await.unwrap().unwrap();
    assert_eq!(b.status, BatchStatus::Curated);
    assert_eq!(b.curated.unwrap().pages, 20);

    // A second release while the writer lock is held elsewhere is refused.
    e.state
        .lock(
            usnm_ingest::release::WRITER_LOCK,
            "someone-else",
            chrono::Duration::hours(1),
        )
        .await
        .unwrap();
    let r = Release {
        state: e.state.clone(),
        curated: e.curated.clone(),
        reference: e.reference.clone(),
        owner: "releaser".into(),
        full: false,
        synthetic: true,
        now: Utc::now(),
        titles_left: None,
        american_stories: false,
        ja_latin: false,
        partition_decade: false,
    };
    let mut sink = JsonlSink::new(e.root.join("idx"));
    assert!(r.run(&mut sink).await.is_err());
}

/// The same two releases into a real Quickwit writer node run by the
/// pipeline (as the Azure job does). Runs when `QUICKWIT_BIN` is set; CI's
/// `quickwit` job sets it.
#[tokio::test]
async fn releases_into_a_quickwit_writer_node() {
    let Some(bin) = std::env::var_os("QUICKWIT_BIN").filter(|b| !b.is_empty()) else {
        eprintln!("QUICKWIT_BIN not set; skipping");
        return;
    };
    use usnm_ingest::sink::{QuickwitNode, QuickwitSink};
    let e = env().await;
    let pages = fixture_pages();
    let split = NaiveDate::from_ymd_opt(1897, 7, 1).unwrap();
    let (early, late): (Vec<&Page>, Vec<&Page>) = pages.iter().partition(|p| p.date < split);
    let a = e.root.join("batch_fx_early_ver01.tar.gz");
    let b = e.root.join("batch_fx_late_ver01.tar.gz");
    write_archive(&a, &early, true, true);
    write_archive(&b, &late, false, true);

    let qw = e.root.join("qw");
    std::fs::create_dir_all(&qw).unwrap();
    let meta = format!("file://{}/meta", qw.display());
    let root = format!("file://{}/indexes", qw.display());
    let node = QuickwitNode::start(Path::new(&bin), &qw, 7390, &meta, &root)
        .await
        .unwrap();

    let mut published = Vec::new();
    for (day, name, path) in [
        (1, "batch_fx_early_ver01", &a),
        (8, "batch_fx_late_ver01", &b),
    ] {
        source::enqueue(&e.state, &[listed(name, path, None)])
            .await
            .unwrap();
        e.worker("w").run(None).await.unwrap();
        let r = Release {
            state: e.state.clone(),
            curated: e.curated.clone(),
            reference: e.reference.clone(),
            owner: "releaser".into(),
            full: false,
            synthetic: true,
            now: Utc.with_ymd_and_hms(2026, 10, day, 3, 0, 0).unwrap(),
            titles_left: None,
            american_stories: false,
            ja_latin: false,
            partition_decade: false,
        };
        let mut sink = QuickwitSink::new(&node.url, &root)
            .unwrap()
            .watching(&node)
            .merges(quick_merges());
        published.push(r.run(&mut sink).await.unwrap().unwrap());
    }
    let p = published.last().unwrap();
    assert_eq!(
        p.indexes,
        ["pages-base-20261001-1", "pages-delta-20261008-1"]
    );
    let current = e.reference_json("current.json").await;
    assert_eq!(current["backend"], "quickwit");

    // The manifest reports the splits of both indexes, merged and closed.
    let manifest = e.reference_json("pages-v20261008-1/manifest.json").await;
    let layout = manifest["indexes"].as_array().unwrap();
    assert_eq!(layout.len(), 2);
    for (l, id) in layout.iter().zip(&p.indexes) {
        assert_eq!(l["index_id"], id.as_str());
        let splits = l["splits"].as_u64().unwrap();
        assert!((1..=7).contains(&splits), "{l}");
        assert!(l["bytes"].as_u64().unwrap() > 0, "{l}");
        // Every split's footer, from the metastore (#125): part of the file.
        let footers = l["footer_bytes"].as_u64().unwrap();
        let largest = l["largest_footer_bytes"].as_u64().unwrap();
        assert!(largest > 0 && largest <= footers, "{l}");
        assert!(footers < l["bytes"].as_u64().unwrap(), "{l}");
    }
    let docs: u64 = layout.iter().map(|l| l["docs"].as_u64().unwrap()).sum();
    let http = reqwest::Client::new();
    let base: Value = http
        .get(format!("{}/api/v1/indexes/{}", node.url, p.indexes[0]))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let source = base["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["source_id"] == "_ingest-source")
        .unwrap();
    assert_eq!(
        source["enabled"], false,
        "a sealed index takes no more writes"
    );

    // Every fixture document is searchable across base + delta, and a
    // phrase matches exactly as often as in the fixture corpus.
    let count = |q: &'static str| {
        let url = format!("{}/api/v1/{}/search", node.url, p.indexes.join(","));
        let http = http.clone();
        async move {
            let v: Value = http
                .post(url)
                .json(&serde_json::json!({"query": q, "max_hits": 0}))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            v["num_hits"].as_u64().unwrap()
        }
    };
    let fixture_docs: Vec<Value> = ["pages-base-fixture", "pages-delta-fixture-1"]
        .iter()
        .flat_map(|f| loc_fixture(f).into_values())
        .collect();
    assert_eq!(count("*").await, fixture_docs.len() as u64);
    assert_eq!(docs, fixture_docs.len() as u64);
    let phrase = fixture_docs
        .iter()
        .filter(|d| d["text"].as_str().unwrap().contains("cross of gold"))
        .count() as u64;
    assert_eq!(count("text:\"cross of gold\"").await, phrase);
    node.stop().await.unwrap();
}

/// Merge waits for tests: poll often, settle quickly.
fn quick_merges() -> usnm_ingest::merges::MergeWait {
    usnm_ingest::merges::MergeWait {
        poll: std::time::Duration::from_millis(500),
        stable_polls: 3,
        timeout: std::time::Duration::from_secs(120),
        finalize_grace: std::time::Duration::from_secs(5),
        // Over Quickwit's 30 s actor heartbeat, so a pipeline still waiting
        // to be stopped doesn't count as stalled.
        finalize_stall: std::time::Duration::from_secs(60),
        report: Default::default(),
    }
}

/// A real Quickwit writer merges an index cut into many small splits down
/// to one before the release may publish it: the planner leaves splits
/// below its merge factor alone, and closing the index merges them.
/// Runs when `QUICKWIT_BIN` is set.
#[tokio::test]
async fn a_writer_merges_small_splits_before_the_index_is_sealed() {
    let Some(bin) = std::env::var_os("QUICKWIT_BIN").filter(|b| !b.is_empty()) else {
        eprintln!("QUICKWIT_BIN not set; skipping");
        return;
    };
    use usnm_ingest::merges::{self, Node};
    use usnm_ingest::sink::{QuickwitNode, INDEX_TEMPLATE};
    let dir = tempfile::tempdir().unwrap();
    let qw = dir.path().join("qw");
    std::fs::create_dir_all(&qw).unwrap();
    // A previous run's data is cleared when the node starts.
    std::fs::create_dir_all(qw.join("qwdata/wal")).unwrap();
    std::fs::write(qw.join("qwdata/wal/stale"), b"x").unwrap();
    let meta = format!("file://{}/meta", qw.display());
    let root = format!("file://{}/indexes", qw.display());
    let node = QuickwitNode::start(Path::new(&bin), &qw, 7395, &meta, &root)
        .await
        .unwrap();
    assert!(!qw.join("qwdata/wal/stale").exists());

    let http = reqwest::Client::new();
    let id = "pages-delta-20261015-1";
    let config = INDEX_TEMPLATE
        .replace("${INDEX_ID}", id)
        .replace("${INDEX_URI}", &format!("{root}/{id}"));
    http.post(format!("{}/api/v1/indexes", node.url))
        .header("content-type", "application/yaml")
        .body(config)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    // Six forced commits: six splits, under the merge factor of 10.
    let docs: Vec<Value> = ["pages-base-fixture", "pages-delta-fixture-1"]
        .iter()
        .flat_map(|f| read_jsonl(&fixtures().join(format!("indexes/{f}.jsonl"))))
        .collect();
    for chunk in docs.chunks(docs.len().div_ceil(6)) {
        let body: String = chunk.iter().map(|d| format!("{d}\n")).collect();
        let v: Value = http
            .post(format!("{}/api/v1/{id}/ingest?commit=force", node.url))
            .body(body)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(v["num_rejected_docs"], 0);
    }
    let n = Node {
        http: &http,
        base: &node.url,
    };
    assert_eq!(n.splits(id).await.unwrap().len(), 6);
    let layout = merges::seal(
        &n,
        id,
        docs.len() as u64,
        Some(&node.events()),
        &quick_merges(),
        None,
    )
    .await
    .unwrap();
    assert_eq!((layout.splits, layout.docs), (1, docs.len() as u64));

    // When the final merges stop (issue #84), the release reopens the index
    // to run them again: the writer rejects the reopening document, starts a
    // new merge pipeline, and runs its final merges once the index is closed
    // again. Nothing changes in a merged index, and it ends closed.
    let events = node.events();
    let spawns = events.spawns(id);
    merges::rerun_final_merges(&n, id, &events, &quick_merges(), deadline(120))
        .await
        .unwrap();
    assert_eq!(events.spawns(id), spawns + 1);
    let until = deadline(120);
    while events.final_merges(id) != merges::FinalMerges::Done {
        assert!(
            tokio::time::Instant::now() < until,
            "{:?}",
            events.final_merges(id)
        );
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    let splits = n.splits(id).await.unwrap();
    assert_eq!(splits.len(), 1);
    assert_eq!(splits[0].docs, docs.len() as u64);
    let index: Value = http
        .get(format!("{}/api/v1/indexes/{id}", node.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let source = index["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["source_id"] == "_ingest-source")
        .unwrap();
    assert_eq!(source["enabled"], false);
    node.stop().await.unwrap();
}

fn deadline(secs: u64) -> tokio::time::Instant {
    tokio::time::Instant::now() + std::time::Duration::from_secs(secs)
}

/// A batch can hold pages but no text (every page's OCR empty or failed):
/// the release then commits nothing. A real writer starts no ingest-source
/// pipeline for an index that was never written, so there is no "merge
/// pipeline completed" line to wait for; the empty index seals anyway.
/// Runs when `QUICKWIT_BIN` is set.
#[tokio::test]
async fn a_writer_seals_an_index_that_received_no_documents() {
    let Some(bin) = std::env::var_os("QUICKWIT_BIN").filter(|b| !b.is_empty()) else {
        eprintln!("QUICKWIT_BIN not set; skipping");
        return;
    };
    use usnm_ingest::sink::{IndexSink, QuickwitNode, QuickwitSink};
    let dir = tempfile::tempdir().unwrap();
    let qw = dir.path().join("qw");
    std::fs::create_dir_all(&qw).unwrap();
    let meta = format!("file://{}/meta", qw.display());
    let root = format!("file://{}/indexes", qw.display());
    let node = QuickwitNode::start(Path::new(&bin), &qw, 7397, &meta, &root)
        .await
        .unwrap();
    let id = "pages-delta-20261022-1";
    let mut sink = QuickwitSink::new(&node.url, &root)
        .unwrap()
        .watching(&node)
        .merges(usnm_ingest::merges::MergeWait {
            timeout: std::time::Duration::from_secs(45),
            ..quick_merges()
        });
    sink.create(id, usnm_ingest::sink::Decades::Off)
        .await
        .unwrap();
    sink.finish(0).await.unwrap();
    let layout = sink.layout(&[id.to_owned()]).await.unwrap();
    assert_eq!((layout[0].splits, layout[0].docs), (0, 0));
    node.stop().await.unwrap();
}

/// A real LoC bulk OCR archive (see `tests/data/README.md`): the layout,
/// compression and checksum as LoC publishes them.
#[tokio::test]
async fn curates_a_real_loc_archive() {
    let e = env().await;
    let archive =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/dlc_zurich_ver04.tar.bz2");
    let list = source::parse_list(
        format!(
            r#"{{"datasets": [{{"batch": "dlc_zurich_ver04", "url": "{}", "page_count": 4,
            "sha256": "ad36f7bb2ef915867460ac2b3c70f9aaa33709a9a99f554a49a9dc4fc35f3ba9"}}]}}"#,
            archive.display()
        )
        .as_bytes(),
    )
    .unwrap();
    source::enqueue(&e.state, &list).await.unwrap();
    assert_eq!(e.worker("w").run(None).await.unwrap(), 1);
    let (b, _) = e.state.batch("dlc_zurich").await.unwrap().unwrap();
    let c = b.curated.unwrap();
    assert_eq!(
        (c.version, c.pages, c.lccns.as_slice()),
        (4, 4, &["sn85042252".to_owned()][..])
    );
    assert_eq!(
        (c.first.as_str(), c.last.as_str()),
        ("1865-08-10", "1865-08-10")
    );
    let mut rows = Vec::new();
    let part = e.curated.get(&c.parts[0]).await.unwrap().unwrap();
    usnm_ingest::curated::read_part(part.into(), true, |r| {
        rows.push(r);
        Ok(())
    })
    .unwrap();
    let mut seqs: Vec<u16> = rows.iter().map(|r| r.key.seq).collect();
    seqs.sort_unstable();
    assert_eq!(seqs, [1, 2, 3, 4]);
    assert!(rows
        .iter()
        .all(|r| r.text.as_ref().is_some_and(|t| t.len() > 500)));
}

/// A release that crashed after writing `current.json` but before recording
/// the publish in Cosmos: the next release treats `current.json` as the
/// truth, repairs the state and builds on the version that is live.
#[tokio::test]
async fn recovers_from_a_crash_between_publish_and_bookkeeping() {
    use usnm_ingest::state::RunStatus;
    let e = env().await;
    let pages = fixture_pages();
    let split = NaiveDate::from_ymd_opt(1897, 7, 1).unwrap();
    let (early, late): (Vec<&Page>, Vec<&Page>) = pages.iter().partition(|p| p.date < split);
    let a = e.root.join("batch_fx_early_ver01.tar.gz");
    let b = e.root.join("batch_fx_late_ver01.tar.gz");
    write_archive(&a, &early, true, true);
    write_archive(&b, &late, false, true);
    source::enqueue(&e.state, &[listed("batch_fx_early_ver01", &a, None)])
        .await
        .unwrap();
    e.worker("w").run(None).await.unwrap();
    let p1 = e.release(1, false).await.unwrap();
    // A publish well before the restart, so the restart's time can't match it.
    let mut pointer = e.reference_json("current.json").await;
    pointer["published_at"] = "2026-10-01T04:00:00Z".into();
    e.reference
        .put(
            "current.json",
            serde_json::to_vec(&pointer).unwrap(),
            "application/json",
        )
        .await
        .unwrap();

    // Undo the bookkeeping, as if the process died right after current.json.
    let (mut run, etag) = e.state.run(&p1.index_version).await.unwrap().unwrap();
    run.status = RunStatus::Building;
    run.published_at = None;
    e.state.update_run(&run, &etag).await.unwrap();
    e.state
        .set_current_version("pages-v20260901-1")
        .await
        .unwrap();

    source::enqueue(&e.state, &[listed("batch_fx_late_ver01", &b, None)])
        .await
        .unwrap();
    e.worker("w").run(None).await.unwrap();
    let p2 = e.release(8, false).await.unwrap();
    assert_eq!(
        p2.indexes,
        ["pages-base-20261001-1", "pages-delta-20261008-1"]
    );
    assert_eq!(
        e.state
            .run(&p1.index_version)
            .await
            .unwrap()
            .unwrap()
            .0
            .status,
        RunStatus::Published
    );
    // The repaired run takes current.json's time, not the restart's.
    let repaired = e.state.run(&p1.index_version).await.unwrap().unwrap().0;
    assert_eq!(
        repaired
            .published_at
            .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
        Some("2026-10-01T04:00:00Z".to_owned())
    );
    assert_eq!(
        e.state.current_version().await.unwrap().as_deref(),
        Some(p2.index_version.as_str())
    );
}

/// Workers that die mid-batch never record a failure; the attempt cap still
/// stops the batch being retried forever.
#[tokio::test]
async fn batches_abandoned_by_crashed_workers_fail_at_the_attempt_cap() {
    let e = env().await;
    let a = e.root.join("batch_fx_x_ver01.tar.gz");
    write_archive(
        &a,
        &fixture_pages().iter().take(3).collect::<Vec<_>>(),
        true,
        true,
    );
    source::enqueue(&e.state, &[listed("batch_fx_x_ver01", &a, None)])
        .await
        .unwrap();
    for _ in 0..usnm_ingest::worker::MAX_ATTEMPTS {
        let w = e.worker("crashy");
        let (mut b, _) = w.claim().await.unwrap().unwrap();
        // Crash: the lease simply runs out.
        let (_, etag) = e.state.batch(&b.batch).await.unwrap().unwrap();
        b.lease.as_mut().unwrap().until = Utc::now() - chrono::Duration::seconds(1);
        e.state.replace_batch(&b, &etag).await.unwrap().unwrap();
    }
    assert!(e.worker("w").claim().await.unwrap().is_none());
    let (b, _) = e.state.batch("batch_fx_x").await.unwrap().unwrap();
    assert_eq!(b.status, BatchStatus::Failed);
    assert!(b.last_error.unwrap().contains("abandoned"));
}

/// Records documents and claims to be Quickwit: stands in for a backend switch.
struct OtherBackend(u64);

#[async_trait::async_trait]
impl usnm_ingest::sink::IndexSink for OtherBackend {
    async fn create(
        &mut self,
        _index_id: &str,
        _: usnm_ingest::sink::Decades,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    async fn add(&mut self, _doc: &Value) -> anyhow::Result<()> {
        self.0 += 1;
        Ok(())
    }
    async fn finish(&mut self, expected: u64) -> anyhow::Result<()> {
        anyhow::ensure!(self.0 == expected);
        Ok(())
    }
    fn backend(&self) -> &'static str {
        "quickwit"
    }
}

/// Deltas only stack on indexes in the same engine: switching backend
/// rebuilds everything into a new base there.
#[tokio::test]
async fn a_backend_switch_forces_a_full_release() {
    let e = env().await;
    let pages = fixture_pages();
    let split = NaiveDate::from_ymd_opt(1897, 7, 1).unwrap();
    let (early, late): (Vec<&Page>, Vec<&Page>) = pages.iter().partition(|p| p.date < split);
    let a = e.root.join("batch_fx_early_ver01.tar.gz");
    let b = e.root.join("batch_fx_late_ver01.tar.gz");
    write_archive(&a, &early, true, true);
    write_archive(&b, &late, false, true);
    source::enqueue(&e.state, &[listed("batch_fx_early_ver01", &a, None)])
        .await
        .unwrap();
    e.worker("w").run(None).await.unwrap();
    e.release(1, false).await.unwrap(); // memory
    source::enqueue(&e.state, &[listed("batch_fx_late_ver01", &b, None)])
        .await
        .unwrap();
    e.worker("w").run(None).await.unwrap();
    let r = Release {
        state: e.state.clone(),
        curated: e.curated.clone(),
        reference: e.reference.clone(),
        owner: "releaser".into(),
        full: false,
        synthetic: true,
        now: Utc.with_ymd_and_hms(2026, 10, 8, 3, 0, 0).unwrap(),
        titles_left: None,
        american_stories: false,
        ja_latin: false,
        partition_decade: false,
    };
    let mut sink = OtherBackend(0);
    let p = r.run(&mut sink).await.unwrap().unwrap();
    assert!(p.full);
    assert_eq!(p.indexes, ["pages-base-20261008-1"]);
    assert_eq!(sink.0, p.docs);
    assert_eq!(
        e.reference_json("current.json").await["backend"],
        "quickwit"
    );
}

/// A release that no longer holds the writer lock stops without publishing.
#[tokio::test]
async fn a_release_that_loses_the_writer_lock_does_not_publish() {
    let e = env().await;
    let a = e.root.join("batch_fx_x_ver01.tar.gz");
    write_archive(
        &a,
        &fixture_pages().iter().take(40).collect::<Vec<_>>(),
        true,
        true,
    );
    source::enqueue(&e.state, &[listed("batch_fx_x_ver01", &a, None)])
        .await
        .unwrap();
    e.worker("w").run(None).await.unwrap();
    let r = Release {
        state: e.state.clone(),
        curated: e.curated.clone(),
        reference: e.reference.clone(),
        owner: "releaser".into(),
        full: false,
        synthetic: true,
        now: Utc::now(),
        titles_left: None,
        american_stories: false,
        ja_latin: false,
        partition_decade: false,
    };
    let lease = r.lock().await.unwrap();
    // Another writer takes the lock over (e.g. after this one stalled).
    let taken = serde_json::json!({
        "id": "quickwit-writer", "kind": "quickwit-writer", "owner": "someone-else",
        "until": Utc::now() + chrono::Duration::hours(1),
    });
    e.state
        .docs
        .upsert("ops", "quickwit-writer", &taken)
        .await
        .unwrap();
    let mut sink = JsonlSink::new(e.root.join("idx"));
    assert!(r.run_held(&mut sink, &lease).await.is_err());
    assert!(e.reference.get("current.json").await.unwrap().is_none());
}

/// A batch whose title isn't in the catalog yet waits for a later release;
/// the batches that are ready publish without it.
#[tokio::test]
async fn batches_with_uncatalogued_titles_wait_for_a_later_release() {
    let e = env().await;
    let pages = fixture_pages();
    let ready: Vec<&Page> = pages
        .iter()
        .filter(|p| !p.text.is_empty())
        .take(20)
        .collect();
    let unknown = Page {
        lccn: "sn99999999".into(),
        date: ready[0].date,
        seq: 1,
        text: "armistice".into(),
    };
    let a = e.root.join("batch_fx_ready_ver01.tar.gz");
    let b = e.root.join("batch_fx_new_ver01.tar.gz");
    write_archive(&a, &ready, true, true);
    write_archive(&b, &[&unknown], true, true);
    source::enqueue(
        &e.state,
        &[
            listed("batch_fx_ready_ver01", &a, None),
            listed("batch_fx_new_ver01", &b, None),
        ],
    )
    .await
    .unwrap();
    e.worker("w").run(None).await.unwrap();
    let p = e.release(1, false).await.unwrap();
    assert_eq!(p.pages, 20);
    // Nothing is ready to add until the title is catalogued.
    assert!(e.release(2, false).await.is_none());
}

/// With titles-sync unfinished, a full base, asked for or forced (there is
/// no published version yet), is refused; a delta goes ahead.
#[tokio::test]
async fn an_unfinished_titles_sync_holds_back_a_full_release_only() {
    let e = env().await;
    let pages = fixture_pages();
    let some: Vec<&Page> = pages
        .iter()
        .filter(|p| !p.text.is_empty())
        .take(20)
        .collect();
    let a = e.root.join("batch_fx_some_ver01.tar.gz");
    write_archive(&a, &some, true, true);
    source::enqueue(&e.state, &[listed("batch_fx_some_ver01", &a, None)])
        .await
        .unwrap();
    e.worker("w").run(None).await.unwrap();
    let release = |full: bool, titles_left: Option<String>| Release {
        state: e.state.clone(),
        curated: e.curated.clone(),
        reference: e.reference.clone(),
        owner: "releaser".into(),
        full,
        synthetic: true,
        now: Utc.with_ymd_and_hms(2026, 10, 1, 3, 0, 0).unwrap(),
        titles_left,
        american_stories: false,
        ja_latin: false,
        partition_decade: false,
    };
    let why = Some("LoC rate limited titles-sync with 5 of 9 titles left".to_owned());
    let mut sink = JsonlSink::new(e.root.join("reference/indexes"));
    // Not asked for, but forced: nothing is published yet.
    let err = release(false, why.clone())
        .run(&mut sink)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("nothing was released"), "{err}");
    assert!(release(false, None).run(&mut sink).await.unwrap().is_some());
    let err = release(true, why.clone())
        .run(&mut sink)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("5 of 9 titles left"), "{err}");
    // A delta goes ahead (here with nothing new to add).
    assert!(release(false, why).run(&mut sink).await.unwrap().is_none());
}

/// A published title the catalog puts in another place (a merge) stays
/// where it was published until a full release, and a corrected point
/// rides a delta with the published place's id (04 §4.6).
#[tokio::test]
async fn a_title_moved_to_another_place_waits_for_a_full_release() {
    let e = env().await;
    let pages = fixture_pages();
    let split = NaiveDate::from_ymd_opt(1897, 7, 1).unwrap();
    let (early, late): (Vec<&Page>, Vec<&Page>) = pages.iter().partition(|p| p.date < split);
    let (late_a, late_b) = late.split_at(late.len() / 2);
    let archives: Vec<(String, PathBuf)> = [("early", &early[..]), ("a", late_a), ("b", late_b)]
        .iter()
        .map(|(name, pages)| {
            let name = format!("batch_fx_{name}_ver01");
            let path = e.root.join(format!("{name}.tar.gz"));
            write_archive(&path, pages, true, true);
            (name, path)
        })
        .collect();
    let add = |i: usize| {
        let (name, path) = archives[i].clone();
        let e = &e;
        async move {
            source::enqueue(&e.state, &[listed(&name, &path, None)])
                .await
                .unwrap();
            e.worker("w").run(None).await.unwrap();
        }
    };
    let edit = |f: Box<dyn Fn(&mut Value)>, path: &'static str| {
        let e = &e;
        async move {
            let mut v = e.reference_json(path).await;
            f(&mut v);
            e.reference
                .put(path, serde_json::to_vec(&v).unwrap(), "application/json")
                .await
                .unwrap();
        }
    };
    add(0).await;
    let p1 = e.release(1, false).await.unwrap();

    // A corrected point for a published place: a delta, whose snapshot has
    // the new point under the same id.
    edit(
        Box::new(|v| v[0]["lat"] = serde_json::json!(41.5)),
        "catalog/places.json",
    )
    .await;
    add(1).await;
    let p2 = e.release(2, false).await.unwrap();
    assert!(!p2.full);
    assert_ne!(p2.index_version, p1.index_version);
    let places = e
        .reference_json(&format!("{}/places.json", p2.index_version))
        .await;
    let first = places
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "P00001")
        .unwrap();
    assert_eq!(first["lat"], 41.5);

    // A published title moved to another place: the delta keeps it where
    // it was published, so its new pages join its published ones.
    let moved = late_b
        .iter()
        .find(|p| !p.text.is_empty())
        .unwrap()
        .lccn
        .clone();
    let catalog = e.reference_json("catalog/titles.json").await;
    let at = catalog
        .as_array()
        .unwrap()
        .iter()
        .position(|t| t["lccn"] == moved.as_str())
        .unwrap();
    let was = catalog[at]["place_id"].as_str().unwrap().to_owned();
    let to = if was == "P00001" { "P00002" } else { "P00001" };
    edit(
        Box::new(move |v| v[at]["place_id"] = serde_json::json!(to)),
        "catalog/titles.json",
    )
    .await;
    add(2).await;
    let p3 = e.release(3, false).await.unwrap();
    assert!(!p3.full, "no full base on its own (#172)");
    let place_in = |titles: &Value| {
        titles
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["lccn"] == moved.as_str())
            .unwrap()["place_id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let titles = e
        .reference_json(&format!("{}/titles.json", p3.index_version))
        .await;
    assert_eq!(place_in(&titles), was, "kept where it was published");
    let delta = e.index(p3.indexes.last().unwrap());
    let of_title: Vec<&Value> = delta
        .values()
        .filter(|d| d["lccn"] == moved.as_str())
        .collect();
    assert!(!of_title.is_empty());
    assert!(of_title.iter().all(|d| d["place_id"] == was.as_str()));

    // A full release moves it everywhere.
    let p4 = e.release(4, true).await.unwrap();
    assert!(p4.full);
    let titles = e
        .reference_json(&format!("{}/titles.json", p4.index_version))
        .await;
    assert_eq!(place_in(&titles), to);
    let docs = e.index(&p4.indexes[0]);
    let of_title: Vec<&Value> = docs
        .values()
        .filter(|d| d["lccn"] == moved.as_str())
        .collect();
    assert!(!of_title.is_empty());
    assert!(of_title.iter().all(|d| d["place_id"] == to));
}

/// The pipeline state with writes of the release's progress item failing,
/// as when Cosmos throttles or drops one request.
struct NoProgress(MemoryDocs, Arc<std::sync::atomic::AtomicUsize>);

#[async_trait::async_trait]
impl usnm_ingest::docs::DocStore for NoProgress {
    async fn get(
        &self,
        c: &str,
        pk: &str,
        id: &str,
    ) -> anyhow::Result<Option<usnm_ingest::docs::Versioned>> {
        self.0.get(c, pk, id).await
    }
    async fn create(&self, c: &str, pk: &str, doc: &Value) -> anyhow::Result<Option<String>> {
        self.0.create(c, pk, doc).await
    }
    async fn replace(
        &self,
        c: &str,
        pk: &str,
        doc: &Value,
        etag: &str,
    ) -> anyhow::Result<Option<String>> {
        self.0.replace(c, pk, doc, etag).await
    }
    async fn upsert(&self, c: &str, pk: &str, doc: &Value) -> anyhow::Result<()> {
        if pk == "release-progress" {
            self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            anyhow::bail!("Cosmos upsert in `ops` returned 503 Service Unavailable");
        }
        self.0.upsert(c, pk, doc).await
    }
    async fn list(
        &self,
        c: &str,
        field: &str,
        values: &[&str],
    ) -> anyhow::Result<Vec<usnm_ingest::docs::Versioned>> {
        self.0.list(c, field, values).await
    }
}

/// Recording progress for the status page never fails a release.
#[tokio::test]
async fn a_release_publishes_when_its_progress_cannot_be_recorded() {
    let mut e = env().await;
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    e.state = State::new(Arc::new(NoProgress(
        MemoryDocs::default(),
        attempts.clone(),
    )));
    let a = e.root.join("batch_fx_p_ver01.tar.gz");
    write_archive(
        &a,
        &fixture_pages().iter().take(40).collect::<Vec<_>>(),
        true,
        true,
    );
    source::enqueue(&e.state, &[listed("batch_fx_p_ver01", &a, None)])
        .await
        .unwrap();
    e.worker("w").run(None).await.unwrap();
    // The commit records when the batch was curated.
    let (b, _) = e.state.batch("batch_fx_p").await.unwrap().unwrap();
    assert!(b.curated_at.is_some_and(|t| t == b.updated_at));
    let p = e.release(1, true).await.unwrap();
    assert!(p.docs > 0);
    assert!(e.reference.get("current.json").await.unwrap().is_some());
    // The release did try (the first report is immediate).
    assert!(attempts.load(std::sync::atomic::Ordering::SeqCst) > 0);
}

/// Curate one small real batch, then add `n` curated batches that copy its
/// parts and counts to paths as long as production's (a job replica's
/// owner id in every attempt path) and list every fixture title.
async fn synthetic_batches(e: &Env, n: usize) -> Vec<String> {
    let pages = fixture_pages();
    let mut one_per_title: BTreeMap<&str, &Page> = BTreeMap::new();
    for p in pages.iter().filter(|p| !p.text.is_empty()) {
        one_per_title.entry(p.lccn.as_str()).or_insert(p);
    }
    let seed: Vec<&Page> = one_per_title.into_values().collect();
    let a = e.root.join("batch_fx_seed_ver01.tar.gz");
    write_archive(&a, &seed, true, true);
    source::enqueue(&e.state, &[listed("batch_fx_seed_ver01", &a, None)])
        .await
        .unwrap();
    e.worker("w").run(None).await.unwrap();
    let (template, _) = e.state.batch("batch_fx_seed").await.unwrap().unwrap();
    let c = template.curated.clone().unwrap();
    let part = e.curated.get(&c.parts[0]).await.unwrap().unwrap();
    let counts = e.curated.get(&c.counts).await.unwrap().unwrap();
    let mut names = Vec::new();
    for i in 0..n {
        let name = format!("xx_synthetic{i:05}");
        let prefix = format!(
            "pages/{name}/v01/20260929T101112123456Z-caj-usnm-backfill-4f7x2-kq9zd-1-0badf00d-a1"
        );
        let mut curated = c.clone();
        curated.parts = Vec::new();
        for k in 0..2 {
            let path = format!("{prefix}/part-{k:04}.parquet");
            e.curated
                .put(&path, part.clone(), "application/octet-stream")
                .await
                .unwrap();
            curated.parts.push(path);
        }
        curated.counts = format!("{prefix}/counts.json");
        e.curated
            .put(&curated.counts, counts.clone(), "application/json")
            .await
            .unwrap();
        let mut b = template.clone();
        b.id = name.clone();
        b.batch = name.clone();
        b.curated = Some(curated);
        assert!(e.state.create_batch(&b).await.unwrap());
        names.push(name);
    }
    names
}

/// A version's batch list grows with the corpus; its Cosmos run item must
/// not (Cosmos DB refuses items over 2 MB). The list lives in the version's
/// reference snapshot, and the next incremental release reads it from there.
#[tokio::test]
async fn run_items_stay_small_however_many_batches_a_version_has() {
    const N: usize = 3_000;
    let e = env().await;
    synthetic_batches(&e, N).await;
    let p1 = e.release(1, true).await.unwrap();
    let v1 = &p1.index_version;
    let item = serde_json::to_vec(&raw_run(&e, v1).await).unwrap().len();
    let list = e
        .reference
        .get(&format!("{v1}/batches.json"))
        .await
        .unwrap()
        .unwrap()
        .len();
    eprintln!(
        "{} batches: run item {item} bytes, batch list {list} bytes ({} per batch)",
        N + 1,
        list / (N + 1)
    );
    assert!(item < 64 * 1024, "run item is {item} bytes");
    // Inline, the list alone would be most of the way to the limit.
    assert!(list > 1_000_000, "batch list is {list} bytes");
    let (run, _) = e.state.run(v1).await.unwrap().unwrap();
    assert_eq!(run.batch_count, Some(N as u64 + 1));
    assert!(run.batches.is_none());

    // One more batch: the next release is a delta with only that batch.
    let pages = fixture_pages();
    let late: Vec<&Page> = pages
        .iter()
        .filter(|p| !p.text.is_empty())
        .rev()
        .take(5)
        .collect();
    let a = e.root.join("batch_fx_late_ver01.tar.gz");
    write_archive(&a, &late, true, true);
    source::enqueue(&e.state, &[listed("batch_fx_late_ver01", &a, None)])
        .await
        .unwrap();
    e.worker("w").run(None).await.unwrap();
    let p2 = e.release(8, false).await.unwrap();
    assert!(!p2.full);
    assert_eq!(p2.indexes.len(), 2);
    assert_eq!(p2.docs, 5);
    let v2 = &p2.index_version;
    let listed2 = e.reference_json(&format!("{v2}/batches.json")).await;
    assert_eq!(listed2.as_array().unwrap().len(), N + 2);
    assert_eq!(raw_run(&e, v2).await["batch_count"], N + 2);
    assert!(serde_json::to_vec(&raw_run(&e, v2).await).unwrap().len() < 64 * 1024);
    // Nothing new since: no release.
    assert!(e.release(9, false).await.is_none());
}

/// Runs published before the batch list moved to the snapshot carry it
/// inline (and their snapshots have no `batches.json`). The next release
/// reads the inline list and writes the new format.
#[tokio::test]
async fn an_incremental_release_builds_on_a_run_with_an_inline_batch_list() {
    let e = env().await;
    let pages = fixture_pages();
    let split = NaiveDate::from_ymd_opt(1897, 7, 1).unwrap();
    let (early, late): (Vec<&Page>, Vec<&Page>) = pages.iter().partition(|p| p.date < split);
    let a = e.root.join("batch_fx_early_ver01.tar.gz");
    let b = e.root.join("batch_fx_late_ver01.tar.gz");
    write_archive(&a, &early, true, true);
    write_archive(&b, &late, false, true);
    source::enqueue(&e.state, &[listed("batch_fx_early_ver01", &a, None)])
        .await
        .unwrap();
    e.worker("w").run(None).await.unwrap();
    let p1 = e.release(1, false).await.unwrap();
    let v1 = &p1.index_version;

    // Rewrite the published run the way older releases stored it.
    let list = e.reference_json(&format!("{v1}/batches.json")).await;
    let mut old = raw_run(&e, v1).await;
    let doc = old.as_object_mut().unwrap();
    doc.remove("batch_count");
    doc.remove("batch_list");
    doc.insert("batches".into(), list);
    e.state.docs.upsert("index_runs", v1, &old).await.unwrap();
    std::fs::remove_file(e.root.join(format!("reference/{v1}/batches.json"))).unwrap();
    let (run, _) = e.state.run(v1).await.unwrap().unwrap();
    assert_eq!(run.batches.as_ref().map(Vec::len), Some(1));

    source::enqueue(&e.state, &[listed("batch_fx_late_ver01", &b, None)])
        .await
        .unwrap();
    e.worker("w").run(None).await.unwrap();
    let p2 = e.release(8, false).await.unwrap();
    assert!(!p2.full);
    let want_delta = loc_fixture("pages-delta-fixture-1");
    assert_eq!(e.index(&p2.indexes[1]), want_delta);
    let item = raw_run(&e, &p2.index_version).await;
    assert!(item.get("batches").is_none());
    assert_eq!(item["batch_count"], 2);
    let list = e
        .reference_json(&format!("{}/batches.json", p2.index_version))
        .await;
    assert_eq!(list.as_array().unwrap().len(), 2);
    // The older item keeps its inline list.
    assert!(raw_run(&e, v1).await["batches"].is_array());
}

/// A batch list that doesn't match its manifest entry stops the release.
#[tokio::test]
async fn a_batch_list_that_does_not_match_its_manifest_stops_the_release() {
    let e = env().await;
    let pages = fixture_pages();
    let split = NaiveDate::from_ymd_opt(1897, 7, 1).unwrap();
    let (early, late): (Vec<&Page>, Vec<&Page>) = pages.iter().partition(|p| p.date < split);
    let a = e.root.join("batch_fx_early_ver01.tar.gz");
    let b = e.root.join("batch_fx_late_ver01.tar.gz");
    write_archive(&a, &early, true, true);
    write_archive(&b, &late, false, true);
    source::enqueue(&e.state, &[listed("batch_fx_early_ver01", &a, None)])
        .await
        .unwrap();
    e.worker("w").run(None).await.unwrap();
    let p1 = e.release(1, false).await.unwrap();
    // Emptied, the published list would make every batch look new.
    e.reference
        .put(
            &format!("{}/batches.json", p1.index_version),
            b"[]".to_vec(),
            "application/json",
        )
        .await
        .unwrap();
    source::enqueue(&e.state, &[listed("batch_fx_late_ver01", &b, None)])
        .await
        .unwrap();
    e.worker("w").run(None).await.unwrap();
    let r = Release {
        state: e.state.clone(),
        curated: e.curated.clone(),
        reference: e.reference.clone(),
        owner: "releaser".into(),
        full: false,
        synthetic: true,
        now: Utc.with_ymd_and_hms(2026, 10, 8, 3, 0, 0).unwrap(),
        titles_left: None,
        american_stories: false,
        ja_latin: false,
        partition_decade: false,
    };
    let mut sink = JsonlSink::new(e.root.join("idx"));
    let err = format!("{:#}", r.run(&mut sink).await.unwrap_err());
    assert!(err.contains("does not match its manifest entry"), "{err}");
}

/// A sink whose final check fails, as a writer that died mid-merge does.
struct FailingSink;

#[async_trait::async_trait]
impl usnm_ingest::sink::IndexSink for FailingSink {
    async fn create(
        &mut self,
        _index_id: &str,
        _: usnm_ingest::sink::Decades,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    async fn add(&mut self, _doc: &Value) -> anyhow::Result<()> {
        Ok(())
    }
    async fn finish(&mut self, _expected: u64) -> anyhow::Result<()> {
        anyhow::bail!("the writer exited")
    }
    fn backend(&self) -> &'static str {
        "memory"
    }
}

/// A release that fails records the run as failed, with when it failed, for the status page.
#[tokio::test]
async fn a_failed_release_records_when_it_failed() {
    let e = env().await;
    let a = e.root.join("batch_fx_fail_ver01.tar.gz");
    write_archive(
        &a,
        &fixture_pages().iter().take(20).collect::<Vec<_>>(),
        true,
        true,
    );
    source::enqueue(&e.state, &[listed("batch_fx_fail_ver01", &a, None)])
        .await
        .unwrap();
    e.worker("w").run(None).await.unwrap();
    let now = Utc.with_ymd_and_hms(2026, 10, 9, 3, 0, 0).unwrap();
    let r = Release {
        state: e.state.clone(),
        curated: e.curated.clone(),
        reference: e.reference.clone(),
        owner: "releaser".into(),
        full: true,
        synthetic: true,
        now,
        titles_left: None,
        american_stories: false,
        ja_latin: false,
        partition_decade: false,
    };
    let before = Utc::now();
    let err = r.run(&mut FailingSink).await.unwrap_err();
    assert!(format!("{err:#}").contains("the writer exited"));
    let (run, _) = e
        .state
        .run("pages-v20261009-1")
        .await
        .unwrap()
        .expect("the run is recorded");
    assert_eq!(run.status, usnm_state::state::RunStatus::Failed);
    let failed_at = run.failed_at.expect("failed_at is recorded");
    assert!(
        failed_at >= before,
        "failed_at is when it failed, not when it started"
    );
    assert!(run.last_error.unwrap().contains("the writer exited"));
}

/// Enqueue and curate `list`.
async fn curate(e: &Env, list: Vec<ListedBatch>) {
    let n = list.len();
    assert_eq!(source::enqueue(&e.state, &list).await.unwrap().new, n);
    assert_eq!(e.worker("w").run(None).await.unwrap(), n);
}

/// Pages that ship in two batches are indexed and counted once (04 §4.7).
/// Of a page's copies the one kept has text, then comes from the batch whose
/// name sorts first. A delta can't drop a copy a published index holds, so
/// the snapshot lists it for searches to hide.
#[tokio::test]
async fn pages_in_two_batches_are_kept_once() {
    let e = env().await;
    let pages = fixture_pages();
    let split = NaiveDate::from_ymd_opt(1897, 7, 1).unwrap();
    let (early, late): (Vec<&Page>, Vec<&Page>) = pages.iter().partition(|p| p.date < split);
    let archive = |name: &str, pages: &[&Page]| {
        let path = e.root.join(format!("{name}.tar.gz"));
        write_archive(&path, pages, false, true);
        listed(name, &path, Some(sha256_file(&path)))
    };

    // Week 1, a base: the early pages, and 25 of them again in a batch whose
    // name sorts later, so the early batch's copies are kept.
    curate(
        &e,
        vec![
            archive("batch_fx_early_ver01", &early),
            archive("batch_zz_ver01", &early[..25]),
        ],
    )
    .await;
    // `usnm-ingest duplicates` measures them before any release.
    let report = usnm_ingest::dedup::report(&e.state, e.curated.as_ref())
        .await
        .unwrap();
    assert_eq!(report["duplicate_pages"], 25);
    assert_eq!(report["distinct_pages"], early.len());
    assert_eq!(
        report["pairs"],
        serde_json::json!([{"kept": "batch_fx_early", "other": "batch_zz", "pages": 25}])
    );
    let p1 = e.release(1, false).await.unwrap();
    assert!(p1.full);
    let want_base = loc_fixture("pages-base-fixture");
    let base = e.index(&p1.indexes[0]);
    assert_eq!(base, want_base, "each page once");
    assert_eq!(p1.docs, want_base.len() as u64);
    assert_eq!(p1.pages, early.len() as u64);
    let v1 = &p1.index_version;
    assert_eq!(raw_run(&e, v1).await["duplicate_pages"], 25);
    let manifest = e.reference_json(&format!("{v1}/manifest.json")).await;
    assert_eq!(manifest["duplicate_pages"], 25);
    // A base indexes one copy of everything, so nothing needs hiding.
    assert_eq!(
        e.reference_json(&format!("{v1}/duplicates.json")).await,
        serde_json::json!([])
    );
    let title_pages: BTreeMap<String, u64> =
        serde_json::from_value(e.reference_json(&format!("{v1}/title_pages.json")).await).unwrap();
    assert_eq!(title_pages.values().sum::<u64>(), early.len() as u64);

    // Week 2, a delta: the late pages, and a batch whose name sorts first
    // with 10 more early pages (already published) and 10 of the late ones.
    let again_early: Vec<&Page> = early[30..40].to_vec();
    let mut aa = again_early.clone();
    aa.extend(&late[..10]);
    curate(
        &e,
        vec![
            archive("batch_aa_ver01", &aa),
            archive("batch_fx_late_ver01", &late),
        ],
    )
    .await;
    let p2 = e.release(8, false).await.unwrap();
    assert!(!p2.full);
    assert_eq!(p2.pages, pages.len() as u64);
    let v2 = &p2.index_version;
    assert_eq!(raw_run(&e, v2).await["duplicate_pages"], 25 + 10 + 10);
    // The delta has every late page once, plus batch_aa's copies of the 10
    // early pages: they win, and the base's copies are hidden.
    let want_delta = loc_fixture("pages-delta-fixture-1");
    let delta = e.index(p2.indexes.last().unwrap());
    let early_ids: Vec<String> = again_early
        .iter()
        .map(|p| format!("{}_{}_ed-1_seq-{}", p.lccn, p.date, p.seq))
        .filter(|id| want_base.contains_key(id))
        .collect();
    assert!(!early_ids.is_empty());
    let mut want = want_delta.clone();
    want.extend(
        early_ids
            .iter()
            .map(|id| (id.clone(), want_base[id].clone())),
    );
    assert_eq!(delta, want);
    let hidden = e.reference_json(&format!("{v2}/duplicates.json")).await;
    let want_hidden: Vec<Value> = early_ids
        .iter()
        .map(|id| serde_json::json!({"doc_id": id, "batch": "batch_fx_early"}))
        .collect();
    let mut got: Vec<Value> = hidden.as_array().unwrap().clone();
    got.sort_by_key(|h| h["doc_id"].as_str().unwrap().to_owned());
    let mut want_hidden = want_hidden;
    want_hidden.sort_by_key(|h| h["doc_id"].as_str().unwrap().to_owned());
    assert_eq!(got, want_hidden);
    // Every page counts once: the snapshot matches the fixture's.
    for f in [
        "baselines.json",
        "title_pages.json",
        "language_baselines.json",
    ] {
        let want: Value =
            serde_json::from_slice(&std::fs::read(fixtures().join("fixture-v1").join(f)).unwrap())
                .unwrap();
        assert_eq!(e.reference_json(&format!("{v2}/{f}")).await, want, "{f}");
    }

    // The API loads the copies to hide with the snapshot.
    let refdata = usnm_api::refdata::RefData::load(e.reference.as_ref())
        .await
        .unwrap();
    assert_eq!(refdata.duplicate_pages, Some(45));
    assert_eq!(refdata.hidden.len(), early_ids.len());
    assert_eq!(refdata.pages, pages.len() as u64);
    assert!(refdata.index_set().hides(&early_ids[0], "batch_fx_early"));

    // A full release keeps the same copies and needs to hide none.
    let p3 = e.release(15, true).await.unwrap();
    let mut both = want_base.clone();
    both.extend(want_delta);
    assert_eq!(e.index(&p3.indexes[0]), both);
    let v3 = &p3.index_version;
    assert_eq!(
        e.reference_json(&format!("{v3}/duplicates.json")).await,
        serde_json::json!([])
    );
    let kept: BTreeMap<String, String> = read_jsonl(
        &e.root
            .join(format!("reference/indexes/{}.jsonl", p3.indexes[0])),
    )
    .into_iter()
    .map(|d| {
        (
            d["doc_id"].as_str().unwrap().to_owned(),
            d["batch"].as_str().unwrap().to_owned(),
        )
    })
    .collect();
    assert!(early_ids.iter().all(|id| kept[id] == "batch_aa"));
}

/// A version released before duplicates were handled indexed every copy, so
/// the first delta on top of it hides every copy it doesn't keep.
#[tokio::test]
async fn a_delta_on_an_older_version_hides_every_extra_copy() {
    let e = env().await;
    let pages = fixture_pages();
    let split = NaiveDate::from_ymd_opt(1897, 7, 1).unwrap();
    let (early, late): (Vec<&Page>, Vec<&Page>) = pages.iter().partition(|p| p.date < split);
    let archive = |name: &str, pages: &[&Page]| {
        let path = e.root.join(format!("{name}.tar.gz"));
        write_archive(&path, pages, false, true);
        listed(name, &path, Some(sha256_file(&path)))
    };
    curate(
        &e,
        vec![
            archive("batch_fx_early_ver01", &early),
            archive("batch_zz_ver01", &early[..25]),
        ],
    )
    .await;
    let p1 = e.release(1, false).await.unwrap();
    // As an older release wrote it: no list of hidden copies.
    std::fs::remove_file(
        e.root
            .join(format!("reference/{}/duplicates.json", p1.index_version)),
    )
    .unwrap();

    curate(&e, vec![archive("batch_fx_late_ver01", &late)]).await;
    let p2 = e.release(8, false).await.unwrap();
    let hidden = e
        .reference_json(&format!("{}/duplicates.json", p2.index_version))
        .await;
    let base = e.index(&p1.indexes[0]);
    let want: Vec<Value> = early[..25]
        .iter()
        .map(|p| format!("{}_{}_ed-1_seq-{}", p.lccn, p.date, p.seq))
        .filter(|id| base.contains_key(id))
        .map(|id| serde_json::json!({"doc_id": id, "batch": "batch_zz"}))
        .collect();
    assert!(!want.is_empty());
    assert_eq!(hidden, Value::Array(want));
}

/// Searches on Quickwit hide the copies a delta couldn't drop: the base
/// holds one copy of a page and the delta the copy that wins, and every
/// count and hit list sees the page once. Runs when `QUICKWIT_BIN` is set.
#[tokio::test]
async fn quickwit_searches_hide_the_copies_a_delta_could_not_drop() {
    let Some(bin) = std::env::var_os("QUICKWIT_BIN").filter(|b| !b.is_empty()) else {
        eprintln!("QUICKWIT_BIN not set; skipping");
        return;
    };
    use usnm_core::params::Filters;
    use usnm_core::query::parse;
    use usnm_core::time::{BucketSpec, BucketUnit};
    use usnm_ingest::sink::{QuickwitNode, QuickwitSink};
    use usnm_search::quickwit::QuickwitBackend;
    use usnm_search::{HitsQuery, IndexSet, SearchBackend};

    let e = env().await;
    let pages = fixture_pages();
    let split = NaiveDate::from_ymd_opt(1897, 7, 1).unwrap();
    let early: Vec<&Page> = pages.iter().filter(|p| p.date < split).collect();
    let again: Vec<&Page> = early[30..40].to_vec();
    let archive = |name: &str, pages: &[&Page]| {
        let path = e.root.join(format!("{name}.tar.gz"));
        write_archive(&path, pages, false, true);
        listed(name, &path, None)
    };
    let qw = e.root.join("qw");
    std::fs::create_dir_all(&qw).unwrap();
    let meta = format!("file://{}/meta", qw.display());
    let root = format!("file://{}/indexes", qw.display());
    let node = QuickwitNode::start(Path::new(&bin), &qw, 7393, &meta, &root)
        .await
        .unwrap();
    let mut published = Vec::new();
    for (day, batch) in [
        (1, archive("batch_fx_early_ver01", &early)),
        (8, archive("batch_aa_ver01", &again)),
    ] {
        curate(&e, vec![batch]).await;
        let r = Release {
            state: e.state.clone(),
            curated: e.curated.clone(),
            reference: e.reference.clone(),
            owner: "releaser".into(),
            full: false,
            synthetic: true,
            now: Utc.with_ymd_and_hms(2026, 10, day, 3, 0, 0).unwrap(),
            titles_left: None,
            american_stories: false,
            ja_latin: false,
            partition_decade: false,
        };
        let mut sink = QuickwitSink::new(&node.url, &root)
            .unwrap()
            .watching(&node)
            .merges(quick_merges());
        published.push(r.run(&mut sink).await.unwrap().unwrap());
    }
    let p = published.last().unwrap();
    let hidden = e
        .reference_json(&format!("{}/duplicates.json", p.index_version))
        .await;
    let hidden: Vec<(String, String)> = hidden
        .as_array()
        .unwrap()
        .iter()
        .map(|h| {
            (
                h["doc_id"].as_str().unwrap().to_owned(),
                h["batch"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert!(!hidden.is_empty());

    let backend = QuickwitBackend::new(&node.url, std::time::Duration::from_secs(30)).unwrap();
    let both = IndexSet::new(p.indexes.clone());
    let deduped = both.clone().hiding(hidden.clone());
    let from = NaiveDate::from_ymd_opt(1895, 1, 1).unwrap();
    let to = NaiveDate::from_ymd_opt(1897, 6, 30).unwrap();
    let f = Filters {
        from,
        to,
        states: vec![],
        lccns: vec![],
        langs: vec![],
        front_only: false,
        series: Vec::new(),
    };
    let spec = BucketSpec::new(BucketUnit::Year, from, to);
    let every = parse("the OR a OR of OR and").unwrap();
    let with = backend.summary(&both, &every, &f, &spec).await.unwrap();
    let without = backend.summary(&deduped, &every, &f, &spec).await.unwrap();
    assert_eq!(with.total_hits - without.total_hits, hidden.len() as u64);
    let indexed: u64 = ["pages-base-fixture"]
        .iter()
        .map(|f| read_jsonl(&fixtures().join(format!("indexes/{f}.jsonl"))).len() as u64)
        .sum();
    assert_eq!(
        p.docs,
        hidden.len() as u64,
        "the delta holds the winning copies"
    );
    assert!(without.total_hits <= indexed);

    // A hidden page's hit list shows it once, from the batch that won.
    let (doc_id, _) = &hidden[0];
    let key = usnm_core::ids::PageKey::from_doc_id(doc_id).unwrap();
    let day = Filters {
        from: key.date,
        to: key.date,
        lccns: vec![key.lccn.clone()],
        ..f.clone()
    };
    let page = HitsQuery {
        lccn: Some(key.lccn.clone()),
        limit: 50,
        ..HitsQuery::default()
    };
    let ids = |set: &IndexSet| {
        let (backend, day, page, every) = (&backend, &day, &page, &every);
        let set = set.clone();
        async move {
            backend
                .hits(&set, every, day, page)
                .await
                .unwrap()
                .hits
                .into_iter()
                .filter(|h| h.doc_id == *doc_id)
                .count()
        }
    };
    assert_eq!(ids(&both).await, 2);
    assert_eq!(ids(&deduped).await, 1);
    node.stop().await.unwrap();
}

/// One row of a Japanese OCR overlay part, as `ja-ocr/jaocr.py` writes it.
struct JaRow<'a> {
    lccn: &'a str,
    date: NaiveDate,
    seq: u16,
    batch: &'a str,
    loc_text: &'a str,
    text: &'a str,
    ocred_at: i64,
}

async fn put_overlay_part(e: &Env, path: &str, rows: &[JaRow<'_>]) {
    use arrow_array::{
        ArrayRef, Date32Array, FixedSizeBinaryArray, Int16Array, Int32Array, LargeStringArray,
        RecordBatch, StringArray, TimestampMicrosecondArray,
    };
    let s = |v: Vec<String>| Arc::new(StringArray::from(v)) as ArrayRef;
    let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
    let key = |r: &JaRow| format!("{}/{}/ed-1/seq-{}", r.lccn, r.date, r.seq);
    let cols: Vec<(&str, ArrayRef)> = vec![
        (
            "doc_id",
            s(rows.iter().map(|r| key(r).replace('/', "_")).collect()),
        ),
        ("page_key", s(rows.iter().map(key).collect())),
        ("lccn", s(rows.iter().map(|r| r.lccn.to_owned()).collect())),
        (
            "date",
            Arc::new(Date32Array::from(
                rows.iter()
                    .map(|r| (r.date - epoch).num_days() as i32)
                    .collect::<Vec<_>>(),
            )),
        ),
        (
            "edition",
            Arc::new(Int16Array::from(vec![1i16; rows.len()])),
        ),
        (
            "seq",
            Arc::new(Int16Array::from(
                rows.iter().map(|r| r.seq as i16).collect::<Vec<_>>(),
            )),
        ),
        (
            "batch",
            s(rows.iter().map(|r| r.batch.to_owned()).collect()),
        ),
        ("ocr_source", s(vec!["usnm-ndlocr-lite".into(); rows.len()])),
        ("ocr_engine", s(vec!["ndlocr-lite test".into(); rows.len()])),
        (
            "loc_text",
            s(rows.iter().map(|r| r.loc_text.to_owned()).collect()),
        ),
        ("text_status", s(vec!["ok".into(); rows.len()])),
        (
            "text",
            Arc::new(LargeStringArray::from(
                rows.iter().map(|r| r.text).collect::<Vec<_>>(),
            )),
        ),
        (
            "text_chars",
            Arc::new(Int32Array::from(
                rows.iter()
                    .map(|r| r.text.chars().count() as i32)
                    .collect::<Vec<_>>(),
            )),
        ),
        (
            "text_sha256",
            Arc::new(FixedSizeBinaryArray::try_from_iter(rows.iter().map(|_| [0u8; 32])).unwrap()),
        ),
        (
            "image_url",
            s(vec!["https://tile.loc.gov/x".into(); rows.len()]),
        ),
        (
            "ocred_at",
            Arc::new(
                TimestampMicrosecondArray::from(
                    rows.iter().map(|r| r.ocred_at).collect::<Vec<_>>(),
                )
                .with_timezone("UTC"),
            ),
        ),
    ];
    let batch = RecordBatch::try_from_iter(cols).unwrap();
    let mut buf = Vec::new();
    let mut w = parquet::arrow::ArrowWriter::try_new(&mut buf, batch.schema(), None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
    e.curated
        .put(path, buf, "application/octet-stream")
        .await
        .unwrap();
}

/// Our Japanese OCR (#139): a release indexes the overlay's pages in their own
/// index, newest copy of each page, only for batches in the version, and adds
/// the pages curation never had to the baselines (not the ones it had).
#[tokio::test]
async fn a_release_indexes_the_japanese_ocr_and_counts_missing_pages() {
    let e = env().await;
    let pages = fixture_pages();
    let path = e.root.join("batch_fx_ja_ver01.tar.gz");
    let all: Vec<&Page> = pages.iter().collect();
    write_archive(&path, &all, false, true);
    curate(
        &e,
        vec![listed("batch_fx_ja_ver01", &path, Some(sha256_file(&path)))],
    )
    .await;

    let p0 = pages.iter().find(|p| !p.text.is_empty()).unwrap();
    // Another page of the same title and day that LoC gave text, in the same batch.
    let p1 = pages
        .iter()
        .find(|p| p.lccn == p0.lccn && p.date == p0.date && p.seq != p0.seq && !p.text.is_empty())
        .expect("two pages with text on one day");
    let same_day = pages
        .iter()
        .filter(|p| p.lccn == p0.lccn && p.date == p0.date)
        .count();
    let row = |seq, batch, loc_text, text, at| JaRow {
        lccn: &p0.lccn,
        date: p0.date,
        seq,
        batch,
        loc_text,
        text,
        ocred_at: at,
    };
    put_overlay_part(
        &e,
        "ocr-ja/pages/a.parquet",
        &[
            row(90, "batch_fx_ja", "missing", "古い読み", 1),
            row(p0.seq, "batch_fx_ja", "garbled", "日本", 1),
            row(91, "batch_elsewhere", "missing", "東京", 1),
            // In the version's batch; a newer copy below is from a batch outside it.
            row(92, "batch_fx_ja", "missing", "大阪", 1),
            // Marked missing (from some archive), but curation has it with text: counted already.
            row(p1.seq, "batch_fx_ja", "missing", "平和", 1),
        ],
    )
    .await;
    put_overlay_part(
        &e,
        "ocr-ja/pages/a.2.parquet",
        &[
            row(90, "batch_fx_ja", "missing", "米國と日本の戰爭", 2),
            row(92, "batch_elsewhere", "missing", "外", 5),
        ],
    )
    .await;

    let published = e.release(5, true).await.expect("published");
    let current = e.reference_json("current.json").await;
    let ja_id = format!(
        "pages-ja-{}",
        published.index_version.trim_start_matches("pages-v")
    );
    assert_eq!(current["ja"]["indexes"], serde_json::json!([ja_id]));
    assert_eq!(current["ja"]["pages"], 4);
    assert_eq!(current["ja"]["fold"], usnm_core::ja::FOLD_VERSION);

    let ja = e.index(&ja_id);
    assert_eq!(ja.len(), 4, "{:?}", ja.keys());
    // The out-of-version copy didn't hide the in-version one.
    assert_eq!(
        ja[&format!("{}_{}_ed-1_seq-92", p0.lccn, p0.date)]["printed"],
        "大阪"
    );
    let missing = &ja[&format!("{}_{}_ed-1_seq-90", p0.lccn, p0.date)];
    // The newest copy wins, and its text is the folded tokens of the printed text.
    assert_eq!(missing["printed"], "米國と日本の戰爭");
    assert_eq!(
        missing["text"],
        usnm_core::ja::index_text("米國と日本の戰爭", Analyzer::LATEST)
    );
    assert_eq!(missing["ocr_engine"], "ndlocr-lite test");

    // The missing pages join the baselines once; the garbled one, and the one
    // curation had in spite of its label, were counted already.
    let v = &published.index_version;
    let titles: Vec<Value> =
        serde_json::from_value(e.reference_json(&format!("{v}/titles.json")).await).unwrap();
    let place = titles
        .iter()
        .find(|t| t["lccn"] == p0.lccn.as_str())
        .unwrap()["place_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let baselines = e.reference_json(&format!("{v}/baselines.json")).await;
    let day = usnm_core::time::day_number(p0.date);
    let at_day = baselines[&place]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d[0] == day)
        .unwrap()[1]
        .as_u64()
        .unwrap();
    assert_eq!(at_day, same_day as u64 + 2);
    let title_pages = e.reference_json(&format!("{v}/title_pages.json")).await;
    let fixture_title_pages: Value = serde_json::from_slice(
        &std::fs::read(fixtures().join("fixture-v1/title_pages.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        title_pages[&p0.lccn],
        fixture_title_pages[&p0.lccn].as_u64().unwrap() + 2
    );
    // The release's page total is the snapshot's.
    let snapshot_total: u64 = title_pages
        .as_object()
        .unwrap()
        .values()
        .map(|v| v.as_u64().unwrap())
        .sum();
    assert_eq!(published.pages, snapshot_total);

    let record = e.reference_json(&format!("{v}/ocr_ja.json")).await;
    assert_eq!(record["added_to_baselines"], 2);
    // The four pages kept have short test texts: all near-blank (no other kind is listed).
    assert_eq!(record["kinds"]["all"]["near_blank"], 4);
    assert!(record["kinds"]["all"].get("japanese").is_none());
    assert_eq!(record["kinds"]["by_loc_text"]["garbled"]["near_blank"], 1);
    assert_eq!(record["skipped"], 1);
    assert_eq!(record["parts"].as_array().unwrap().len(), 2);
    let manifest = e.reference_json(&format!("{v}/manifest.json")).await;
    assert!(manifest["files"]
        .as_array()
        .unwrap()
        .iter()
        .any(|f| f["path"] == "ocr_ja.json"));
    assert_eq!(
        manifest["built_from"]["ocr_ja"].as_array().unwrap().len(),
        2
    );
}

/// New Japanese OCR with no new batches (#139): an incremental release
/// publishes it on the same main indexes, with a new Japanese index and
/// snapshot; with neither new batches nor new OCR, nothing is released.
#[tokio::test]
async fn new_japanese_ocr_alone_is_released_on_the_same_indexes() {
    let e = env().await;
    let pages = fixture_pages();
    let path = e.root.join("batch_fx_ja_ver01.tar.gz");
    let all: Vec<&Page> = pages.iter().collect();
    write_archive(&path, &all, false, true);
    curate(
        &e,
        vec![listed("batch_fx_ja_ver01", &path, Some(sha256_file(&path)))],
    )
    .await;
    let p0 = pages.iter().find(|p| !p.text.is_empty()).unwrap();
    let row = |seq, text| JaRow {
        lccn: &p0.lccn,
        date: p0.date,
        seq,
        batch: "batch_fx_ja",
        loc_text: "missing",
        text,
        ocred_at: 1,
    };

    // A base with the first OCR part.
    put_overlay_part(&e, "ocr-ja/pages/a.parquet", &[row(90, "米國と日本の戰爭")]).await;
    let v1 = e.release(5, true).await.expect("base");
    // No new batches, no new OCR: nothing to release.
    assert!(e.release(6, false).await.is_none());

    // New OCR arrives: released on the same main indexes.
    put_overlay_part(&e, "ocr-ja/pages/b.parquet", &[row(91, "東京の新聞")]).await;
    let v2 = e.release(6, false).await.expect("overlay-only release");
    assert!(!v2.full);
    assert_eq!(v2.indexes, v1.indexes, "no new main index");
    assert_eq!(v2.docs, 0);
    assert_eq!(v2.pages, v1.pages + 1);
    let current = e.reference_json("current.json").await;
    assert_eq!(current["index_version"], v2.index_version.as_str());
    assert_eq!(current["indexes"], serde_json::json!(v1.indexes));
    let ja_id = format!(
        "pages-ja-{}",
        v2.index_version.trim_start_matches("pages-v")
    );
    assert_eq!(current["ja"]["indexes"], serde_json::json!([ja_id]));
    assert_eq!(e.index(&ja_id).len(), 2);
    let run = raw_run(&e, &v2.index_version).await;
    assert_eq!(run["new_index"], ja_id.as_str());
    // Its build record lists only the template it applied (#161).
    assert!(run["build"]["templates"].get("pages").is_none(), "{run}");
    assert!(run["build"]["templates"]["pages-ja"]["sha256"].is_string());
    let manifest = e
        .reference_json(&format!("{}/manifest.json", v2.index_version))
        .await;
    assert_eq!(
        manifest["build"]["templates"]["pages-ja"]["yaml"],
        usnm_ingest::ocr_ja::JA_TEMPLATE
    );
    assert!(manifest["build"]["templates"].get("pages").is_none());
    let record = e
        .reference_json(&format!("{}/ocr_ja.json", v2.index_version))
        .await;
    assert_eq!(record["added_to_baselines"], 2);

    // And again nothing, until something changes.
    assert!(e.release(7, false).await.is_none());
}

/// OCR that changed without a page to index doesn't make an overlay-only
/// release: the run would name a Japanese index it never wrote.
#[tokio::test]
async fn japanese_ocr_with_nothing_to_index_waits() {
    let e = env().await;
    let pages = fixture_pages();
    let path = e.root.join("batch_fx_ja_ver01.tar.gz");
    let all: Vec<&Page> = pages.iter().collect();
    write_archive(&path, &all, false, true);
    curate(
        &e,
        vec![listed("batch_fx_ja_ver01", &path, Some(sha256_file(&path)))],
    )
    .await;
    let p0 = pages.iter().find(|p| !p.text.is_empty()).unwrap();
    e.release(5, true).await.expect("base");
    // A part whose only page is from a batch outside the version: changed, nothing to index.
    put_overlay_part(
        &e,
        "ocr-ja/pages/x.parquet",
        &[JaRow {
            lccn: &p0.lccn,
            date: p0.date,
            seq: 90,
            batch: "batch_elsewhere",
            loc_text: "missing",
            text: "東京",
            ocred_at: 1,
        }],
    )
    .await;
    assert!(e.release(6, false).await.is_none());
}

/// Our OCR's Latin text on the pages LoC ships without text (#203,
/// `--ja-latin`): the release's new main index takes the pages no index of
/// the version holds yet, each counted once (the baselines had them since
/// #139). A delta keeps the earlier ones, and hides one a new batch now has
/// from LoC; with no new batch, new Latin text of ours gets a delta of its
/// own, and OCR with no Latin text gets an overlay-only release; a full
/// release without the setting leaves them out. A page curation had takes our text in place of
/// LoC's where ours reads more words (here, an empty one).
#[tokio::test]
async fn our_ocr_latin_text_reaches_the_main_index_once() {
    let e = env().await;
    let (early, late, early_pages, late_pages) = curate_early_and_late(&e).await;
    curate(&e, early).await;
    let pages = fixture_pages();
    let split = NaiveDate::from_ymd_opt(1897, 7, 1).unwrap();
    let p0 = pages
        .iter()
        .find(|p| p.date < split && !p.text.is_empty())
        .unwrap();
    let p1 = pages
        .iter()
        .find(|p| p.lccn == p0.lccn && p.date == p0.date && p.seq != p0.seq && !p.text.is_empty())
        .expect("two pages with text on one day");
    // An early page LoC has no text for, which curation has (an `empty` page).
    let blank = pages
        .iter()
        .find(|p| p.date < split && p.text.is_empty())
        .expect("an empty early page");
    // A late page with LoC's text, which our OCR also read as a page missing from the early batch.
    let late_page = pages
        .iter()
        .find(|p| p.date >= split && !p.text.is_empty())
        .unwrap();
    let id = |p: &Page| format!("{}_{}_ed-1_seq-{}", p.lccn, p.date, p.seq);
    let at = |seq: u16| format!("{}_{}_ed-1_seq-{seq}", p0.lccn, p0.date);
    let row = |lccn, date, seq, loc_text, text| JaRow {
        lccn,
        date,
        seq,
        batch: "batch_fx_early",
        loc_text,
        text,
        ocred_at: 1,
    };
    let ads = "Moritz Drug Co\n2001 Larimer St., Denver\n米國と日本の戰爭";
    put_overlay_part(
        &e,
        "ocr-ja/pages/a.parquet",
        &[
            row(&p0.lccn, p0.date, 90, "missing", ads),
            // Japanese only: no main-index document.
            row(&p0.lccn, p0.date, 91, "missing", "東京の新聞"),
            // Curation has these from LoC: no document of ours.
            row(
                &p0.lccn,
                p0.date,
                p1.seq,
                "missing",
                "Larimer Street, Denver, Colorado",
            ),
            row(
                &p0.lccn,
                p0.date,
                p0.seq,
                "garbled",
                "Larimer Street, Denver, Colorado",
            ),
            row(
                &late_page.lccn,
                late_page.date,
                late_page.seq,
                "missing",
                "Sunshine Grocery, Denver",
            ),
            // Ours reads words where LoC has none: its document takes our text.
            row(
                &blank.lccn,
                blank.date,
                blank.seq,
                "empty",
                "The relocation center held a meeting",
            ),
        ],
    )
    .await;

    // A base with the setting: the two pages curation doesn't have get our
    // Latin text, and so does the empty one, in its own document.
    let v1 = e.release_latin(5, true, true).await.expect("base");
    let base = e.index(&v1.indexes[0]);
    let early_docs = early_pages
        - pages
            .iter()
            .filter(|p| p.date < split && p.text.is_empty())
            .count();
    assert_eq!(base.len(), early_docs + 3, "{:?}", v1);
    assert_eq!(
        base[&id(blank)]["text"],
        "The relocation center held a meeting"
    );
    let doc = &base[&at(90)];
    assert_eq!(doc["text"], "Moritz Drug Co\n2001 Larimer St., Denver");
    // Its batch is the one whose archive had no text for it (hidden copies name it).
    let raw = read_jsonl(
        &e.root
            .join(format!("reference/indexes/{}.jsonl", v1.indexes[0])),
    );
    let raw = raw.iter().find(|d| d["doc_id"] == at(90).as_str()).unwrap();
    assert_eq!(raw["batch"], "batch_fx_early");
    assert_eq!(base[&id(late_page)]["text"], "Sunshine Grocery, Denver");
    assert!(!base.contains_key(&at(91)));
    // LoC's copies, untouched.
    assert_eq!(base[&id(p1)]["text"], p1.text.as_str());
    assert_eq!(base[&id(p0)]["text"], p0.text.as_str());
    let v = &v1.index_version;
    let latin = e.reference_json(&format!("{v}/ja_latin.json")).await;
    assert_eq!(
        latin["pages"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        {
            let mut want = vec![at(90), id(late_page), id(blank)];
            want.sort();
            want
        }
        .iter()
        .collect::<Vec<_>>()
    );
    assert_eq!(latin["pages"][at(90)]["ocr_engine"], "ndlocr-lite test");
    assert_eq!(latin["pages"][at(90)]["loc_text"], "missing");
    assert_eq!(latin["pages"][id(blank)]["loc_text"], "empty");
    let record = e.reference_json(&format!("{v}/ocr_ja.json")).await;
    assert_eq!(
        record["latin"],
        serde_json::json!({"added": 3, "pages": 3, "hidden": 0, "rule": usnm_ingest::ocr_ja::LATIN_VERSION})
    );
    // Counted once each: the three pages curation never had (90, 91 and the
    // late one, in no counts.json of this version).
    assert_eq!(record["added_to_baselines"], 3);
    assert_eq!(v1.pages, early_pages as u64 + 3);
    let manifest = e.reference_json(&format!("{v}/manifest.json")).await;
    assert_eq!(
        manifest["build"]["features"]["ja_latin"],
        usnm_ingest::ocr_ja::LATIN_VERSION
    );

    // New OCR alone, with Latin text on a page no index holds: a delta of
    // just that page, though no batch is new, and the base kept.
    put_overlay_part(
        &e,
        "ocr-ja/pages/b.parquet",
        &[row(
            &p0.lccn,
            p0.date,
            92,
            "missing",
            "SUNSHINE GROCERY 1851 Larimer Street",
        )],
    )
    .await;
    let v2 = e.release_latin(6, false, true).await.expect("Latin delta");
    assert!(!v2.full);
    assert_eq!(v2.indexes.len(), 2);
    assert_eq!(v2.indexes[0], v1.indexes[0]);
    let delta = e.index(&v2.indexes[1]);
    assert_eq!(delta.keys().collect::<Vec<_>>(), vec![&at(92)]);
    assert_eq!(
        delta[&at(92)]["text"],
        "SUNSHINE GROCERY 1851 Larimer Street"
    );
    assert_eq!(v2.docs, 1);
    assert_eq!(v2.pages, v1.pages + 1);
    let v = &v2.index_version;
    let latin2 = e.reference_json(&format!("{v}/ja_latin.json")).await;
    assert_eq!(latin2["pages"].as_object().unwrap().len(), 4);
    assert_eq!(latin2["pages"][at(92)]["loc_text"], "missing");
    let record = e.reference_json(&format!("{v}/ocr_ja.json")).await;
    assert_eq!(
        record["latin"],
        serde_json::json!({"added": 1, "pages": 4, "hidden": 0, "rule": usnm_ingest::ocr_ja::LATIN_VERSION})
    );
    let hidden = e.reference_json(&format!("{v}/duplicates.json")).await;
    assert_eq!(hidden, serde_json::json!([]));
    let current = e.reference_json("current.json").await;
    assert_eq!(current["index_version"], v.as_str());
    assert_eq!(
        current["indexes"],
        serde_json::to_value(&v2.indexes).unwrap()
    );
    assert!(current["ja"]["indexes"][0].is_string());
    let manifest = e.reference_json(&format!("{v}/manifest.json")).await;
    assert!(manifest["build"]["templates"]["pages"].is_object());
    assert!(manifest["build"]["templates"]["pages-ja"].is_object());
    // Nothing new since: nothing to release.
    assert!(e.release_latin(7, false, true).await.is_none());

    // OCR with no Latin text: a new Japanese index, no main index, the same Latin pages.
    put_overlay_part(
        &e,
        "ocr-ja/pages/c.parquet",
        &[row(&p0.lccn, p0.date, 93, "missing", "米國の新聞")],
    )
    .await;
    let v3 = e.release_latin(8, false, true).await.expect("overlay-only");
    assert_eq!(v3.indexes, v2.indexes);
    assert_eq!(v3.pages, v2.pages + 1);
    let latin3 = e
        .reference_json(&format!("{}/ja_latin.json", v3.index_version))
        .await;
    assert_eq!(latin3, latin2);

    // A delta with the late batch: the late page now has LoC's text, so our
    // copy in the base is hidden and it counts once. Pages 90 and 92 are in
    // the indexes the version keeps.
    curate(&e, late).await;
    let v4 = e.release_latin(9, false, true).await.expect("delta");
    assert!(!v4.full);
    assert_eq!(v4.indexes.len(), 3);
    let delta = e.index(&v4.indexes[2]);
    assert_eq!(delta[&id(late_page)]["text"], late_page.text.as_str());
    assert!(!delta.contains_key(&at(90)), "already in the base");
    assert!(!delta.contains_key(&at(92)), "already in the first delta");
    let v = &v4.index_version;
    let latin4 = e.reference_json(&format!("{v}/ja_latin.json")).await;
    assert_eq!(latin4["pages"].as_object().unwrap().len(), 4);
    let hidden = e.reference_json(&format!("{v}/duplicates.json")).await;
    assert_eq!(
        hidden,
        serde_json::json!([{"doc_id": id(late_page), "batch": "batch_fx_early"}])
    );
    let record = e.reference_json(&format!("{v}/ocr_ja.json")).await;
    assert_eq!(
        record["latin"],
        serde_json::json!({"added": 0, "pages": 4, "hidden": 1, "rule": usnm_ingest::ocr_ja::LATIN_VERSION})
    );
    // Every fixture page, and 90 to 93 once each.
    assert_eq!(v4.pages, (early_pages + late_pages) as u64 + 4);
    let title_pages = e.reference_json(&format!("{v}/title_pages.json")).await;
    let total: u64 = title_pages
        .as_object()
        .unwrap()
        .values()
        .map(|v| v.as_u64().unwrap())
        .sum();
    assert_eq!(total, v4.pages);

    // A full release without the setting: LoC's text only, and no record.
    let v5 = e.release_latin(10, true, false).await.expect("full");
    let docs: usize = v5.indexes.iter().map(|i| e.index(i).len()).sum();
    let fixture_docs = pages.iter().filter(|p| !p.text.is_empty()).count();
    assert_eq!(docs, fixture_docs);
    assert_eq!(v5.pages, v4.pages);
    assert!(e
        .reference
        .get(&format!("{}/ja_latin.json", v5.index_version))
        .await
        .unwrap()
        .is_none());
    let hidden = e
        .reference_json(&format!("{}/duplicates.json", v5.index_version))
        .await;
    assert_eq!(hidden, serde_json::json!([]));
}

/// The setting turned on with no new batch and no new OCR (#203): the
/// pages LoC ships without text that our OCR read Latin text on get a
/// delta of just those pages, rather than waiting for LoC's next batch.
/// Our text in place of LoC's (an empty page here) waits for a release
/// that indexes the page's batch. Once published, nothing is left to
/// release, and a run finds that out without reading the curated parts,
/// though a page our OCR read is left out for a copy of LoC's.
#[tokio::test]
async fn our_latin_text_gets_a_delta_without_a_new_batch() {
    let e = env().await;
    let (early, _, early_pages, _) = curate_early_and_late(&e).await;
    curate(&e, early).await;
    let pages = fixture_pages();
    let split = NaiveDate::from_ymd_opt(1897, 7, 1).unwrap();
    let blank = pages
        .iter()
        .find(|p| p.date < split && p.text.is_empty())
        .expect("an empty early page");
    let row = |seq, loc_text, text| JaRow {
        lccn: &blank.lccn,
        date: blank.date,
        seq,
        batch: "batch_fx_early",
        loc_text,
        text,
        ocred_at: 1,
    };
    // A page curation has with LoC's text, that our OCR read as missing
    // from the batch's archive: no document of ours, ever.
    let with_text = pages
        .iter()
        .find(|p| p.date < split && !p.text.is_empty())
        .unwrap();
    put_overlay_part(
        &e,
        "ocr-ja/pages/a.parquet",
        &[
            row(
                90,
                "missing",
                "Moritz Drug Co, 2001 Larimer Street, eggs ½ dozen",
            ),
            row(91, "missing", "東京の新聞"),
            row(blank.seq, "empty", "The relocation center held a meeting"),
            JaRow {
                lccn: &with_text.lccn,
                date: with_text.date,
                seq: with_text.seq,
                batch: "batch_fx_early",
                loc_text: "missing",
                text: "Larimer Street, Denver, Colorado",
                ocred_at: 1,
            },
        ],
    )
    .await;
    let id = |seq: u16| format!("{}_{}_ed-1_seq-{seq}", blank.lccn, blank.date);
    let v1 = e.release_latin(5, true, false).await.expect("base");
    assert!(!e.index(&v1.indexes[0]).contains_key(&id(90)));
    assert!(e.release_latin(6, false, false).await.is_none());
    // As if the base had been built before #168, with analyzer version 1.
    let mut pointer = e.reference_json("current.json").await;
    pointer["common_grams"] = 1.into();
    e.reference
        .put(
            "current.json",
            serde_json::to_vec(&pointer).unwrap(),
            "application/json",
        )
        .await
        .unwrap();

    let v2 = e.release_latin(7, false, true).await.expect("Latin delta");
    assert!(!v2.full);
    assert_eq!(v2.indexes.len(), 2);
    assert_eq!(v2.indexes[0], v1.indexes[0]);
    // Its pairs are version 1's, like the base's (`Env::index_as` checks).
    let delta = e.index_as(&v2.indexes[1], Analyzer::V1);
    assert_eq!(delta.keys().collect::<Vec<_>>(), vec![&id(90)]);
    let text = "Moritz Drug Co, 2001 Larimer Street, eggs ½ dozen";
    assert_eq!(delta[&id(90)]["text"], text);
    assert_ne!(
        usnm_core::common_grams::index_text(text, Analyzer::V1),
        usnm_core::common_grams::index_text(text, Analyzer::LATEST),
        "a text the two analyzers fold apart"
    );
    assert_eq!(e.reference_json("current.json").await["common_grams"], 1);
    assert_eq!(v2.docs, 1);
    // Pages 90 and 91 were counted since the base.
    assert_eq!(v2.pages, v1.pages);
    assert_eq!(v1.pages, early_pages as u64 + 2);
    let v = &v2.index_version;
    let latin = e.reference_json(&format!("{v}/ja_latin.json")).await;
    assert_eq!(
        latin["pages"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        vec![&id(90)]
    );
    let record = e.reference_json(&format!("{v}/ocr_ja.json")).await;
    assert_eq!(
        record["latin"],
        serde_json::json!({"added": 1, "pages": 1, "hidden": 0, "rule": usnm_ingest::ocr_ja::LATIN_VERSION})
    );
    let manifest = e.reference_json(&format!("{v}/manifest.json")).await;
    assert_eq!(
        manifest["build"]["features"]["ja_latin"],
        usnm_ingest::ocr_ja::LATIN_VERSION
    );
    assert_eq!(manifest["build"]["features"]["common_grams"], 1);
    assert_eq!(manifest["build"]["features"]["ja_fold"], 1);
    // With the curated parts gone, a run that read them would fail.
    let parts = e.root.join("curated/pages");
    let moved = e.root.join("parts-away");
    std::fs::rename(&parts, &moved).unwrap();
    assert!(e.release_latin(8, false, true).await.is_none());
    std::fs::rename(&moved, &parts).unwrap();
}

/// The setting turned on where every page our OCR read Latin text on is
/// left out for a copy of LoC's (#203): one overlay-only release records
/// that the overlay was weighed, and later runs release nothing without
/// reading the curated parts.
#[tokio::test]
async fn latin_text_with_nothing_to_add_is_weighed_once() {
    let e = env().await;
    let (early, _, _, _) = curate_early_and_late(&e).await;
    curate(&e, early).await;
    let pages = fixture_pages();
    let split = NaiveDate::from_ymd_opt(1897, 7, 1).unwrap();
    let with_text = pages
        .iter()
        .find(|p| p.date < split && !p.text.is_empty())
        .unwrap();
    let row = |seq, text| JaRow {
        lccn: &with_text.lccn,
        date: with_text.date,
        seq,
        batch: "batch_fx_early",
        loc_text: "missing",
        text,
        ocred_at: 1,
    };
    put_overlay_part(
        &e,
        "ocr-ja/pages/a.parquet",
        &[
            row(with_text.seq, "Larimer Street, Denver, Colorado"),
            row(91, "東京の新聞"),
        ],
    )
    .await;
    let v1 = e.release_latin(5, true, false).await.expect("base");
    let v2 = e.release_latin(6, false, true).await.expect("overlay-only");
    assert_eq!(v2.indexes, v1.indexes);
    assert_eq!(v2.pages, v1.pages);
    let v = &v2.index_version;
    let record = e.reference_json(&format!("{v}/ocr_ja.json")).await;
    assert_eq!(
        record["latin"],
        serde_json::json!({"added": 0, "pages": 0, "hidden": 0, "rule": usnm_ingest::ocr_ja::LATIN_VERSION})
    );
    assert!(e
        .reference
        .get(&format!("{v}/ja_latin.json"))
        .await
        .unwrap()
        .is_none());
    // With the curated parts gone, a run that read them would fail.
    let parts = e.root.join("curated/pages");
    let moved = e.root.join("parts-away");
    std::fs::rename(&parts, &moved).unwrap();
    assert!(e.release_latin(7, false, true).await.is_none());
    std::fs::rename(&moved, &parts).unwrap();
}

/// A page LoC had no text for, indexed with ours in its own document
/// (#203), that a later batch ships with LoC's text: the later copy wins,
/// and the snapshot hides ours, so the page counts once.
#[tokio::test]
async fn a_copy_with_our_text_that_loses_to_locs_is_hidden() {
    let e = env().await;
    let (early, _, early_pages, _) = curate_early_and_late(&e).await;
    curate(&e, early).await;
    let pages = fixture_pages();
    let split = NaiveDate::from_ymd_opt(1897, 7, 1).unwrap();
    let blank = pages
        .iter()
        .find(|p| p.date < split && p.text.is_empty())
        .expect("an empty early page");
    let id = format!("{}_{}_ed-1_seq-{}", blank.lccn, blank.date, blank.seq);
    put_overlay_part(
        &e,
        "ocr-ja/pages/a.parquet",
        &[JaRow {
            lccn: &blank.lccn,
            date: blank.date,
            seq: blank.seq,
            batch: "batch_fx_early",
            loc_text: "empty",
            text: "The relocation center held a meeting",
            ocred_at: 1,
        }],
    )
    .await;
    let v1 = e.release_latin(5, true, true).await.expect("base");
    assert_eq!(
        e.index(&v1.indexes[0])[&id]["text"],
        "The relocation center held a meeting"
    );
    assert_eq!(v1.pages, early_pages as u64);

    // A later batch has the page with LoC's text.
    let again = Page {
        lccn: blank.lccn.clone(),
        date: blank.date,
        seq: blank.seq,
        text: "The council met on Tuesday and voted for the new school.".into(),
    };
    let path = e.root.join("batch_fx_again_ver01.tar.gz");
    write_archive(&path, &[&again], false, true);
    curate(&e, vec![listed("batch_fx_again_ver01", &path, None)]).await;
    let v2 = e.release_latin(6, false, true).await.expect("delta");
    assert_eq!(e.index(&v2.indexes[1])[&id]["text"], again.text.as_str());
    let hidden = e
        .reference_json(&format!("{}/duplicates.json", v2.index_version))
        .await;
    assert_eq!(
        hidden,
        serde_json::json!([{"doc_id": id, "batch": "batch_fx_early"}])
    );
    assert_eq!(v2.pages, early_pages as u64, "the page counts once");
}

/// A later batch that ships a page with no text of LoC's doesn't take over
/// from our copy (#203): neither a page our OCR read because LoC's archive
/// had no text for it, nor an empty page given our text, is hidden, and the
/// later copy gets no document, so each page is still searchable and counts
/// once.
#[tokio::test]
async fn a_copy_without_locs_text_does_not_hide_ours() {
    let e = env().await;
    let (early, _, early_pages, _) = curate_early_and_late(&e).await;
    curate(&e, early).await;
    let pages = fixture_pages();
    let split = NaiveDate::from_ymd_opt(1897, 7, 1).unwrap();
    let blank = pages
        .iter()
        .find(|p| p.date < split && p.text.is_empty())
        .expect("an empty early page");
    let row = |seq, loc_text, text| JaRow {
        lccn: &blank.lccn,
        date: blank.date,
        seq,
        batch: "batch_fx_early",
        loc_text,
        text,
        ocred_at: 1,
    };
    put_overlay_part(
        &e,
        "ocr-ja/pages/a.parquet",
        &[
            row(90, "missing", "Moritz Drug Co, 2001 Larimer Street"),
            row(blank.seq, "empty", "The relocation center held a meeting"),
        ],
    )
    .await;
    let v1 = e.release_latin(5, true, true).await.expect("base");
    let id = |seq: u16| format!("{}_{}_ed-1_seq-{seq}", blank.lccn, blank.date);
    let base = e.index(&v1.indexes[0]);
    assert!(base.contains_key(&id(90)) && base.contains_key(&id(blank.seq)));
    assert_eq!(v1.pages, early_pages as u64 + 1);

    // A batch whose name sorts first ships both pages, empty: its copy of
    // the empty page wins the plan's tie, but has nothing to search.
    let empty = |seq| Page {
        lccn: blank.lccn.clone(),
        date: blank.date,
        seq,
        text: String::new(),
    };
    let (p90, pb) = (empty(90), empty(blank.seq));
    let path = e.root.join("batch_fx_again_ver01.tar.gz");
    write_archive(&path, &[&p90, &pb], false, true);
    curate(&e, vec![listed("batch_fx_again_ver01", &path, None)]).await;
    let v2 = e.release_latin(6, false, true).await.expect("delta");
    let delta = e.index(&v2.indexes[1]);
    assert!(!delta.contains_key(&id(90)) && !delta.contains_key(&id(blank.seq)));
    let hidden = e
        .reference_json(&format!("{}/duplicates.json", v2.index_version))
        .await;
    assert_eq!(hidden, serde_json::json!([]));
    let record = e
        .reference_json(&format!("{}/ocr_ja.json", v2.index_version))
        .await;
    assert_eq!(record["latin"]["hidden"], 0);
    assert_eq!(record["latin"]["pages"], 2);
    // Page 90 is counted now from the new batch's counts, not ours; the
    // empty page twice over is one page.
    assert_eq!(v2.pages, early_pages as u64 + 1, "each page counts once");
}

/// Our OCR's Latin text in a version laid out by decade (#203 with #123):
/// its documents carry the `decade` field like LoC's, in the partitioned
/// base and in a tagged delta, so a date-limited search that skips the
/// other decades' splits still finds them.
#[tokio::test]
async fn our_latin_text_follows_the_decade_layout() {
    let e = env().await;
    let (early, late, _, _) = curate_early_and_late(&e).await;
    curate(&e, early).await;
    let pages = fixture_pages();
    let split = NaiveDate::from_ymd_opt(1897, 7, 1).unwrap();
    let blank = pages
        .iter()
        .find(|p| p.date < split && p.text.is_empty())
        .expect("an empty early page");
    let row = |seq, loc_text, text| JaRow {
        lccn: &blank.lccn,
        date: blank.date,
        seq,
        batch: "batch_fx_early",
        loc_text,
        text,
        ocred_at: 1,
    };
    put_overlay_part(
        &e,
        "ocr-ja/pages/a.parquet",
        &[
            row(90, "missing", "Moritz Drug Co, 2001 Larimer Street"),
            row(blank.seq, "empty", "The relocation center held a meeting"),
        ],
    )
    .await;
    let release = |day, full| Release {
        partition_decade: true,
        ja_latin: true,
        ..e.releaser(day, full)
    };
    let mut sink = JsonlSink::new(e.root.join("reference/indexes"));
    let v1 = release(5, true).run(&mut sink).await.unwrap().unwrap();
    assert_eq!(
        e.reference_json("current.json").await["decades"],
        usnm_core::decade::VERSION
    );
    let decade = usnm_core::decade::of_date(blank.date);
    let id = |seq: u16| format!("{}_{}_ed-1_seq-{seq}", blank.lccn, blank.date);
    let base = e.index(&v1.indexes[0]);
    assert!(base.values().all(|d| d.get("decade").is_some()));
    assert_eq!(base[&id(90)]["decade"], decade);
    assert_eq!(base[&id(90)]["text"], "Moritz Drug Co, 2001 Larimer Street");
    assert_eq!(base[&id(blank.seq)]["decade"], decade);

    // A delta on it is tagged: its new page of ours has the field too.
    put_overlay_part(
        &e,
        "ocr-ja/pages/b.parquet",
        &[row(92, "missing", "SUNSHINE GROCERY 1851 Larimer Street")],
    )
    .await;
    curate(&e, late).await;
    let v2 = release(6, false).run(&mut sink).await.unwrap().unwrap();
    assert!(!v2.full);
    let delta = e.index(&v2.indexes[1]);
    assert!(delta.values().all(|d| d.get("decade").is_some()));
    assert_eq!(delta[&id(92)]["decade"], decade);

    // So is a delta of only our Latin text, with no new batch.
    put_overlay_part(
        &e,
        "ocr-ja/pages/c.parquet",
        &[row(93, "missing", "Pacific Citizen, 1945 Larimer Street")],
    )
    .await;
    let v3 = release(7, false).run(&mut sink).await.unwrap().unwrap();
    assert_eq!(v3.indexes.len(), 3);
    let delta = e.index(&v3.indexes[2]);
    assert_eq!(delta.keys().collect::<Vec<_>>(), vec![&id(93)]);
    assert_eq!(delta[&id(93)]["decade"], decade);
    assert_eq!(
        e.reference_json("current.json").await["decades"],
        usnm_core::decade::VERSION
    );
}

/// Copies without LoC's text don't cost a page our text (#203, review of
/// #250): a page LoC's archive had no text for in one batch and an empty
/// copy in another keeps our document (and the empty copy gets none), and
/// an empty page with copies in two batches gets our text on the copy the
/// version keeps, whichever batch's copy we read. One document each.
#[tokio::test]
async fn copies_without_locs_text_keep_our_text_once() {
    let e = env().await;
    let (early, _, early_pages, _) = curate_early_and_late(&e).await;
    let pages = fixture_pages();
    let split = NaiveDate::from_ymd_opt(1897, 7, 1).unwrap();
    let blank = pages
        .iter()
        .find(|p| p.date < split && p.text.is_empty())
        .expect("an empty early page");
    // Another batch, sorting first, with both pages and no text for either.
    let empty = |seq| Page {
        lccn: blank.lccn.clone(),
        date: blank.date,
        seq,
        text: String::new(),
    };
    let (p90, pb) = (empty(90), empty(blank.seq));
    let path = e.root.join("batch_fx_again_ver01.tar.gz");
    write_archive(&path, &[&p90, &pb], false, true);
    let mut both = early;
    both.push(listed("batch_fx_again_ver01", &path, None));
    curate(&e, both).await;
    let row = |seq, loc_text, text| JaRow {
        lccn: &blank.lccn,
        date: blank.date,
        seq,
        batch: "batch_fx_early",
        loc_text,
        text,
        ocred_at: 1,
    };
    put_overlay_part(
        &e,
        "ocr-ja/pages/a.parquet",
        &[
            row(90, "missing", "Moritz Drug Co, 2001 Larimer Street"),
            // We read the early batch's copy; the plan keeps batch_fx_again's.
            row(blank.seq, "empty", "The relocation center held a meeting"),
        ],
    )
    .await;
    let v = e.release_latin(5, true, true).await.expect("base");
    let id = |seq: u16| format!("{}_{}_ed-1_seq-{seq}", blank.lccn, blank.date);
    let raw = read_jsonl(
        &e.root
            .join(format!("reference/indexes/{}.jsonl", v.indexes[0])),
    );
    let docs = |seq| {
        raw.iter()
            .filter(|d| d["doc_id"] == id(seq).as_str())
            .collect::<Vec<_>>()
    };
    let d90 = docs(90);
    assert_eq!(d90.len(), 1, "one document for page 90");
    assert_eq!(d90[0]["text"], "Moritz Drug Co, 2001 Larimer Street");
    assert_eq!(d90[0]["batch"], "batch_fx_early");
    let db = docs(blank.seq);
    assert_eq!(db.len(), 1, "one document for the empty page");
    assert_eq!(db[0]["text"], "The relocation center held a meeting");
    assert_eq!(db[0]["batch"], "batch_fx_again");
    let latin = e
        .reference_json(&format!("{}/ja_latin.json", v.index_version))
        .await;
    assert_eq!(latin["pages"][id(blank.seq)]["batch"], "batch_fx_again");
    assert_eq!(latin["pages"][id(90)]["batch"], "batch_fx_early");
    let hidden = e
        .reference_json(&format!("{}/duplicates.json", v.index_version))
        .await;
    assert_eq!(hidden, serde_json::json!([]));
    // The early pages, and page 90 from the other batch's counts; the empty
    // page's two copies are one page.
    assert_eq!(v.pages, early_pages as u64 + 1);
}

/// American Stories' text for the fixture pages that have some (their
/// `text_as`), as `american_stories_write.py` writes it: a part per title
/// and year, then each year's marker. Plus a page no batch has, and a
/// blank text for a page with no text of LoC's either: neither is indexed.
/// The number of parts.
async fn put_american_stories(e: &Env) -> usize {
    use chrono::Datelike;
    type Rows = Vec<(String, String, NaiveDate, String)>;
    let mut parts: BTreeMap<(String, i32), Rows> = BTreeMap::new();
    let mut indexed = std::collections::BTreeSet::new();
    for f in ["pages-base-fixture", "pages-delta-fixture-1"] {
        for d in read_jsonl(&fixtures().join(format!("indexes/{f}.jsonl"))) {
            let id = d["doc_id"].as_str().unwrap().to_owned();
            indexed.insert(id.clone());
            let Some(text) = d["text_as"].as_str() else {
                continue;
            };
            let lccn = d["lccn"].as_str().unwrap().to_owned();
            let date = NaiveDate::parse_from_str(d["date"].as_str().unwrap(), "%Y-%m-%d").unwrap();
            parts.entry((lccn.clone(), date.year())).or_default().push((
                id,
                lccn,
                date,
                text.to_owned(),
            ));
        }
    }
    let blank = fixture_pages()
        .into_iter()
        .find(|p| !indexed.contains(&format!("{}_{}_ed-1_seq-{}", p.lccn, p.date, p.seq)))
        .expect("a page with no text at all");
    let rows = parts
        .entry((blank.lccn.clone(), blank.date.year()))
        .or_default();
    rows.push((
        format!("{}_{}_ed-1_seq-{}", blank.lccn, blank.date, blank.seq),
        blank.lccn.clone(),
        blank.date,
        " \n ".into(),
    ));
    rows.push((
        format!("{}_{}_ed-1_seq-9", blank.lccn, blank.date),
        blank.lccn.clone(),
        blank.date,
        "A page no batch has".into(),
    ));
    let mut years = std::collections::BTreeSet::new();
    for ((lccn, year), rows) in &parts {
        let rows: Vec<(&str, &str, NaiveDate, &str)> = rows
            .iter()
            .map(|(id, l, d, t)| (id.as_str(), l.as_str(), *d, t.as_str()))
            .collect();
        e.curated
            .put(
                &format!("american-stories/pages/{lccn}/{year}-000.parquet"),
                usnm_ingest::american_stories::encode_part(&rows).unwrap(),
                "application/octet-stream",
            )
            .await
            .unwrap();
        years.insert(*year);
    }
    for year in years {
        e.curated
            .put(
                &format!("american-stories/years/{year}.json"),
                b"{}".to_vec(),
                "application/json",
            )
            .await
            .unwrap();
    }
    parts.len()
}

/// Curate the fixture pages as two batches: early (before July 1897) and late.
async fn curate_early_and_late(e: &Env) -> (Vec<ListedBatch>, Vec<ListedBatch>, usize, usize) {
    let pages = fixture_pages();
    let split = NaiveDate::from_ymd_opt(1897, 7, 1).unwrap();
    let (early, late): (Vec<&Page>, Vec<&Page>) = pages.iter().partition(|p| p.date < split);
    let a = e.root.join("batch_fx_early_ver01.tar.gz");
    let b = e.root.join("batch_fx_late_ver01.tar.gz");
    write_archive(&a, &early, true, true);
    write_archive(&b, &late, false, true);
    (
        vec![listed("batch_fx_early_ver01", &a, None)],
        vec![listed("batch_fx_late_ver01", &b, None)],
        early.len(),
        late.len(),
    )
}

/// With `--american-stories` (#218, 05 §5.5.4) the release reproduces the
/// fixtures in full: `text_as` and its pairs exactly on the pages American
/// Stories covers, and the pages only it has text for indexed with an empty
/// `text`. The version says so in `current.json` and its build record, and
/// records the parts it read. Without the setting, the next release is as
/// before.
#[tokio::test]
async fn american_stories_text_is_indexed_beside_locs() {
    let e = env().await;
    let (early, late, early_pages, late_pages) = curate_early_and_late(&e).await;
    put_american_stories(&e).await;
    curate(&e, early).await;
    let p1 = e.release_with(1, false, true).await.unwrap().unwrap();
    assert!(p1.full);
    let want_base = fixture("pages-base-fixture");
    assert_eq!(e.index(&p1.indexes[0]), want_base);
    assert_eq!(p1.docs, want_base.len() as u64);
    assert_eq!(
        p1.pages, early_pages as u64,
        "the baselines count every page already"
    );
    let only = want_base.values().filter(|d| d["text"] == "").count();
    let with_as = want_base
        .values()
        .filter(|d| d.get("text_as").is_some())
        .count();
    assert!(only > 0 && with_as > only, "{only} {with_as}");
    assert_eq!(
        loc_fixture("pages-base-fixture").len() + only,
        want_base.len()
    );

    let v1 = &p1.index_version;
    let current = e.reference_json("current.json").await;
    assert_eq!(
        current["american_stories"],
        usnm_core::american_stories::VERSION
    );
    let manifest = e.reference_json(&format!("{v1}/manifest.json")).await;
    let built = &manifest["built_from"]["american_stories"];
    assert_eq!(built["docs"], with_as);
    assert_eq!(built["only_american_stories"], only);
    assert_eq!(built["version"], usnm_core::american_stories::VERSION);
    assert_eq!(
        manifest["build"]["features"]["american_stories"],
        usnm_core::american_stories::VERSION
    );
    assert!(manifest["files"]
        .as_array()
        .unwrap()
        .iter()
        .any(|f| f["path"] == "american_stories.json"));
    let record = e
        .reference_json(&format!("{v1}/american_stories.json"))
        .await;
    let parts = record["parts"].as_array().unwrap();
    assert_eq!(built["parts"], parts.len());
    assert!(!parts.is_empty());
    assert!(parts
        .iter()
        .all(|p| p["sha256"].as_str().unwrap().len() == 64 && p["bytes"].as_u64() > Some(0)));
    assert_eq!(record["docs"], with_as);

    // A delta keeps writing the text.
    curate(&e, late).await;
    let p2 = e.release_with(8, false, true).await.unwrap().unwrap();
    assert!(!p2.full);
    assert_eq!(
        e.index(p2.indexes.last().unwrap()),
        fixture("pages-delta-fixture-1")
    );
    assert_eq!(p2.pages, (early_pages + late_pages) as u64);
    let v2 = &p2.index_version;
    let current = e.reference_json("current.json").await;
    assert_eq!(
        current["american_stories"],
        usnm_core::american_stories::VERSION
    );
    // The snapshot is the fixture's: no page counts twice.
    for f in [
        "baselines.json",
        "title_pages.json",
        "language_baselines.json",
    ] {
        let want: Value =
            serde_json::from_slice(&std::fs::read(fixtures().join("fixture-v1").join(f)).unwrap())
                .unwrap();
        assert_eq!(e.reference_json(&format!("{v2}/{f}")).await, want, "{f}");
    }
    // The API searches the text.
    let refdata = usnm_api::refdata::RefData::load(e.reference.as_ref())
        .await
        .unwrap();
    assert!(refdata.index_set().american_stories());

    // Without the setting, a full release is LoC's text alone again, and
    // the version doesn't claim the text.
    let p3 = e.release_with(15, true, false).await.unwrap().unwrap();
    let mut both = loc_fixture("pages-base-fixture");
    both.extend(loc_fixture("pages-delta-fixture-1"));
    assert_eq!(e.index(&p3.indexes[0]), both);
    let current = e.reference_json("current.json").await;
    assert!(current.get("american_stories").is_none(), "{current}");
    let v3 = &p3.index_version;
    let manifest = e.reference_json(&format!("{v3}/manifest.json")).await;
    assert!(manifest["built_from"].get("american_stories").is_none());
    assert!(manifest["build"]["features"]
        .get("american_stories")
        .is_none());
    assert!(e
        .reference
        .get(&format!("{v3}/american_stories.json"))
        .await
        .unwrap()
        .is_none());
    let refdata = usnm_api::refdata::RefData::load(e.reference.as_ref())
        .await
        .unwrap();
    assert!(!refdata.index_set().american_stories());
}

/// Turning the setting on for a version built without it rebuilds every
/// index, so the version can say all of them have the text. Until then,
/// text in the curated store changes nothing: the documents are LoC's.
#[tokio::test]
async fn a_version_without_american_stories_is_rebuilt_in_full_when_it_is_turned_on() {
    let e = env().await;
    let (early, late, ..) = curate_early_and_late(&e).await;
    put_american_stories(&e).await;
    curate(&e, early).await;
    let p1 = e.release(1, false).await.unwrap();
    assert_eq!(
        e.index(&p1.indexes[0]),
        loc_fixture("pages-base-fixture"),
        "off: the text in the store is not read"
    );
    let current = e.reference_json("current.json").await;
    assert!(current.get("american_stories").is_none(), "{current}");

    curate(&e, late).await;
    let p2 = e.release_with(8, false, true).await.unwrap().unwrap();
    assert!(
        p2.full,
        "a delta would mix indexes with and without the text"
    );
    assert_eq!(p2.indexes.len(), 1);
    let mut both = fixture("pages-base-fixture");
    both.extend(fixture("pages-delta-fixture-1"));
    assert_eq!(e.index(&p2.indexes[0]), both);
    let current = e.reference_json("current.json").await;
    assert_eq!(
        current["american_stories"],
        usnm_core::american_stories::VERSION
    );

    // A published version with another version of the text is rebuilt too.
    let mut pointer = current;
    pointer["american_stories"] = serde_json::json!(usnm_core::american_stories::VERSION + 1);
    e.reference
        .put(
            "current.json",
            serde_json::to_vec(&pointer).unwrap(),
            "application/json",
        )
        .await
        .unwrap();
    let pages = fixture_pages();
    let extra: Vec<&Page> = pages.iter().take(3).collect();
    let path = e.root.join("batch_zz_ver01.tar.gz");
    write_archive(&path, &extra, false, true);
    curate(&e, vec![listed("batch_zz_ver01", &path, None)]).await;
    let p3 = e.release_with(9, false, true).await.unwrap().unwrap();
    assert!(p3.full);
}

/// With the setting on and no American Stories text written yet (no
/// finished year), nothing is released: the version would claim text it
/// doesn't have.
#[tokio::test]
async fn the_setting_needs_american_stories_text_in_the_store() {
    let e = env().await;
    let (early, ..) = curate_early_and_late(&e).await;
    curate(&e, early).await;
    // A part of a year the writer hasn't finished doesn't count.
    e.curated
        .put(
            "american-stories/pages/sn99000001/1895-000.parquet",
            usnm_ingest::american_stories::encode_part(&[]).unwrap(),
            "application/octet-stream",
        )
        .await
        .unwrap();
    let err = e.release_with(1, false, true).await.unwrap_err();
    assert!(
        format!("{err:#}").contains("american-stories-write"),
        "{err:#}"
    );
    assert!(e.reference.get("current.json").await.unwrap().is_none());
    // Without the setting the same release goes ahead.
    assert!(e.release(1, false).await.unwrap().full);
}

/// A page only American Stories has text for is a document, so a copy of
/// it in a later batch that wins (its name sorts first) hides the published
/// one, as a copy with LoC's text would (04 §4.7).
#[tokio::test]
async fn a_page_with_only_american_stories_text_is_kept_once() {
    let e = env().await;
    let (early, ..) = curate_early_and_late(&e).await;
    put_american_stories(&e).await;
    curate(&e, early).await;
    let p1 = e.release_with(1, false, true).await.unwrap().unwrap();
    let want_base = fixture("pages-base-fixture");
    let pages = fixture_pages();
    let id = |p: &Page| format!("{}_{}_ed-1_seq-{}", p.lccn, p.date, p.seq);
    let only: Vec<&Page> = pages
        .iter()
        .filter(|p| want_base.get(&id(p)).is_some_and(|d| d["text"] == ""))
        .take(2)
        .collect();
    assert_eq!(only.len(), 2);

    let path = e.root.join("batch_aa_ver01.tar.gz");
    write_archive(&path, &only, false, true);
    curate(&e, vec![listed("batch_aa_ver01", &path, None)]).await;
    let p2 = e.release_with(8, false, true).await.unwrap().unwrap();
    assert!(!p2.full);
    assert_eq!(p2.pages, p1.pages, "each page counts once");
    let delta = e.index(p2.indexes.last().unwrap());
    let want: BTreeMap<String, Value> = only
        .iter()
        .map(|p| (id(p), want_base[&id(p)].clone()))
        .collect();
    assert_eq!(delta, want);
    let v2 = &p2.index_version;
    let mut hidden: Vec<Value> = e
        .reference_json(&format!("{v2}/duplicates.json"))
        .await
        .as_array()
        .unwrap()
        .clone();
    hidden.sort_by_key(|h| h["doc_id"].as_str().unwrap().to_owned());
    let mut want_hidden: Vec<Value> = only
        .iter()
        .map(|p| serde_json::json!({"doc_id": id(p), "batch": "batch_fx_early"}))
        .collect();
    want_hidden.sort_by_key(|h| h["doc_id"].as_str().unwrap().to_owned());
    assert_eq!(hidden, want_hidden);

    // A full release keeps batch_aa's copies and hides nothing.
    let p3 = e.release_with(15, true, true).await.unwrap().unwrap();
    assert_eq!(e.index(&p3.indexes[0]), want_base);
    assert_eq!(
        e.reference_json(&format!("{}/duplicates.json", p3.index_version))
            .await,
        serde_json::json!([])
    );
}

/// A release of new Japanese OCR alone keeps the published main indexes,
/// and with them their American Stories text: `current.json` still says so.
#[tokio::test]
async fn a_japanese_ocr_release_keeps_american_stories() {
    let e = env().await;
    let pages = fixture_pages();
    let path = e.root.join("batch_fx_ja_ver01.tar.gz");
    let all: Vec<&Page> = pages.iter().collect();
    write_archive(&path, &all, false, true);
    curate(
        &e,
        vec![listed("batch_fx_ja_ver01", &path, Some(sha256_file(&path)))],
    )
    .await;
    put_american_stories(&e).await;
    let p0 = pages.iter().find(|p| !p.text.is_empty()).unwrap();
    let row = |seq, text| JaRow {
        lccn: &p0.lccn,
        date: p0.date,
        seq,
        batch: "batch_fx_ja",
        loc_text: "missing",
        text,
        ocred_at: 1,
    };
    put_overlay_part(&e, "ocr-ja/pages/a.parquet", &[row(90, "米國と日本の戰爭")]).await;
    let v1 = e.release_with(5, true, true).await.unwrap().unwrap();
    put_overlay_part(&e, "ocr-ja/pages/b.parquet", &[row(91, "東京の新聞")]).await;
    let v2 = e.release_with(6, false, true).await.unwrap().unwrap();
    assert!(!v2.full);
    assert_eq!(v2.indexes, v1.indexes, "no new main index");
    let current = e.reference_json("current.json").await;
    assert_eq!(current["index_version"], v2.index_version.as_str());
    assert_eq!(
        current["american_stories"],
        usnm_core::american_stories::VERSION
    );
    // It built no main index, so it read no American Stories text.
    let manifest = e
        .reference_json(&format!("{}/manifest.json", v2.index_version))
        .await;
    assert!(manifest["built_from"].get("american_stories").is_none());
}

/// The documents with American Stories' text go into a real Quickwit
/// writer under the strict mapping, and both texts are searchable. Runs
/// when `QUICKWIT_BIN` is set.
#[tokio::test]
async fn releases_american_stories_into_a_quickwit_writer_node() {
    let Some(bin) = std::env::var_os("QUICKWIT_BIN").filter(|b| !b.is_empty()) else {
        eprintln!("QUICKWIT_BIN not set; skipping");
        return;
    };
    use usnm_ingest::sink::{QuickwitNode, QuickwitSink};
    let e = env().await;
    let (early, late, ..) = curate_early_and_late(&e).await;
    put_american_stories(&e).await;
    let qw = e.root.join("qw");
    std::fs::create_dir_all(&qw).unwrap();
    let meta = format!("file://{}/meta", qw.display());
    let root = format!("file://{}/indexes", qw.display());
    let node = QuickwitNode::start(Path::new(&bin), &qw, 7401, &meta, &root)
        .await
        .unwrap();
    let mut published = Vec::new();
    for (day, list) in [(1, early), (8, late)] {
        curate(&e, list).await;
        let r = Release {
            state: e.state.clone(),
            curated: e.curated.clone(),
            reference: e.reference.clone(),
            owner: "releaser".into(),
            full: false,
            synthetic: true,
            now: Utc.with_ymd_and_hms(2026, 10, day, 3, 0, 0).unwrap(),
            titles_left: None,
            american_stories: true,
            ja_latin: false,
            partition_decade: false,
        };
        let mut sink = QuickwitSink::new(&node.url, &root)
            .unwrap()
            .watching(&node)
            .merges(quick_merges());
        published.push(r.run(&mut sink).await.unwrap().unwrap());
    }
    let p = published.last().unwrap();
    assert_eq!(p.indexes.len(), 2);
    let current = e.reference_json("current.json").await;
    assert_eq!(
        current["american_stories"],
        usnm_core::american_stories::VERSION
    );
    let http = reqwest::Client::new();
    let count = |q: String| {
        let url = format!("{}/api/v1/{}/search", node.url, p.indexes.join(","));
        let http = http.clone();
        async move {
            let v: Value = http
                .post(url)
                .json(&serde_json::json!({"query": q, "max_hits": 0}))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            v["num_hits"].as_u64().unwrap_or_else(|| panic!("{v}"))
        }
    };
    let docs: Vec<Value> = ["pages-base-fixture", "pages-delta-fixture-1"]
        .iter()
        .flat_map(|f| fixture(f).into_values())
        .collect();
    assert_eq!(count("*".into()).await, docs.len() as u64);
    let has = |field: &str, phrase: &str| {
        docs.iter()
            .filter(|d| d[field].as_str().is_some_and(|t| t.contains(phrase)))
            .count() as u64
    };
    let want = has("text_as", "cross of gold");
    assert!(want > 0);
    assert_eq!(count("text_as:\"cross of gold\"".into()).await, want);
    // The pairs field, as the API queries a phrase with a common word.
    let pairs = usnm_core::common_grams::index_text("cross of gold", Analyzer::LATEST);
    assert_eq!(
        count(format!(
            "text_as_cg:\"{pairs}\" AND text_as:\"cross of gold\""
        ))
        .await,
        want
    );
    node.stop().await.unwrap();
}

/// A full base built with `--partition-decade` (05 §5.5.5): every page has
/// its decade, the pages are written a decade at a time, and the version
/// says so. Deltas follow the published version whatever the setting says,
/// and a full base without it lays nothing out by decade.
#[tokio::test]
async fn a_base_laid_out_by_decade_and_the_releases_after_it() {
    let e = env().await;
    let pages = fixture_pages();
    let d = |s: &str| NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
    let group = |from: &str, to: &str| -> Vec<Page> {
        let g: Vec<&Page> = pages
            .iter()
            .filter(|p| p.date >= d(from) && p.date < d(to))
            .collect();
        over_decades(&g)
    };
    let batches = [
        ("batch_fx_a_ver01", group("1895-01-01", "1896-07-01")),
        ("batch_fx_b_ver01", group("1896-07-01", "1897-07-01")),
        ("batch_fx_c_ver01", group("1897-07-01", "1898-01-01")),
    ];
    let curate = |i: usize| {
        let (name, pages) = &batches[i];
        let path = e.root.join(format!("{name}.tar.gz"));
        write_archive(&path, &pages.iter().collect::<Vec<_>>(), false, true);
        let e = &e;
        let name = *name;
        async move {
            source::enqueue(&e.state, &[listed(name, &path, None)])
                .await
                .unwrap();
            e.worker("w").run(None).await.unwrap();
        }
    };
    let decade_of = |doc: &Value| usnm_core::decade::of_date(d(doc["date"].as_str().unwrap()));

    // 1. A partitioned base.
    curate(0).await;
    let p1 = e.release_by_decade(1, true, true).await;
    assert!(p1.full);
    let pointer = e.reference_json("current.json").await;
    assert_eq!(pointer["decades"], usnm_core::decade::VERSION);
    let manifest = e
        .reference_json(&format!("{}/manifest.json", p1.index_version))
        .await;
    assert_eq!(
        manifest["build"]["features"]["decades"],
        usnm_core::decade::VERSION
    );
    let yaml = manifest["build"]["templates"]["pages"]["yaml"]
        .as_str()
        .unwrap();
    assert!(yaml.contains("partition_key: decade"), "{yaml}");
    let base = &p1.indexes[0];
    let docs = read_jsonl(&e.root.join(format!("reference/indexes/{base}.jsonl")));
    assert_eq!(docs.len() as u64, p1.docs);
    for doc in &docs {
        assert_eq!(
            doc["decade"].as_u64(),
            Some(u64::from(decade_of(doc))),
            "{doc}"
        );
    }
    // Written a decade at a time: under the split target, each decade's
    // pages wait for the end and go out oldest decade first.
    let written: Vec<u16> = docs.iter().map(decade_of).collect();
    assert!(written.windows(2).all(|w| w[0] <= w[1]), "{written:?}");
    let mut seen = written.clone();
    seen.dedup();
    assert_eq!(seen, [1830, 1840, 1860, 1890]);
    // The order's files are gone.
    assert!(!e
        .root
        .join("reference/indexes")
        .join(usnm_ingest::decade_order::SPILL_DIR)
        .exists());

    // 2. A delta follows the version, the setting off: the field, and its
    // splits would be tagged rather than partitioned.
    curate(1).await;
    let p2 = e.release_by_decade(8, false, false).await;
    assert!(!p2.full);
    assert_eq!(
        e.reference_json("current.json").await["decades"],
        usnm_core::decade::VERSION
    );
    let manifest = e
        .reference_json(&format!("{}/manifest.json", p2.index_version))
        .await;
    let yaml = manifest["build"]["templates"]["pages"]["yaml"]
        .as_str()
        .unwrap();
    assert!(
        yaml.contains("tag_fields: [decade]") && !yaml.contains("partition_key"),
        "{yaml}"
    );
    let delta = read_jsonl(
        &e.root
            .join(format!("reference/indexes/{}.jsonl", p2.indexes[1])),
    );
    assert!(!delta.is_empty());
    for doc in &delta {
        assert_eq!(
            doc["decade"].as_u64(),
            Some(u64::from(decade_of(doc))),
            "{doc}"
        );
    }

    // 3. A full base without the setting: no decades anywhere.
    let p3 = e.release_by_decade(15, true, false).await;
    assert!(p3.full);
    let pointer = e.reference_json("current.json").await;
    assert!(pointer.get("decades").is_none(), "{pointer}");
    let manifest = e
        .reference_json(&format!("{}/manifest.json", p3.index_version))
        .await;
    assert!(manifest["build"]["features"].get("decades").is_none());
    let rebuilt = read_jsonl(
        &e.root
            .join(format!("reference/indexes/{}.jsonl", p3.indexes[0])),
    );
    assert!(rebuilt.iter().all(|doc| doc.get("decade").is_none()));
    // The partitioned base's pages are this base's, apart from the field.
    let rebuilt: BTreeMap<&str, &Value> = rebuilt
        .iter()
        .map(|doc| (doc["doc_id"].as_str().unwrap(), doc))
        .collect();
    for mut doc in docs {
        doc.as_object_mut().unwrap().remove("decade");
        assert_eq!(rebuilt[doc["doc_id"].as_str().unwrap()], &doc);
    }

    // 4. The setting on, but a delta: it follows the version, so no decades
    // until the next full base.
    curate(2).await;
    let p4 = e.release_by_decade(22, false, true).await;
    assert!(!p4.full);
    assert!(e
        .reference_json("current.json")
        .await
        .get("decades")
        .is_none());
    let delta = read_jsonl(
        &e.root
            .join(format!("reference/indexes/{}.jsonl", p4.indexes[1])),
    );
    assert!(!delta.is_empty() && delta.iter().all(|doc| doc.get("decade").is_none()));
}

/// A base built with `--partition-decade` on a real Quickwit writer (05
/// §5.5.5): each split holds one decade, the release checks the merges per
/// decade and records the splits of each, and a delta on it tags its split
/// with its decades. Searches that name their decades get the same pages
/// as without, from fewer splits. Runs when `QUICKWIT_BIN` is set.
#[tokio::test]
async fn releases_a_base_laid_out_by_decade_into_a_quickwit_writer_node() {
    let Some(bin) = std::env::var_os("QUICKWIT_BIN").filter(|b| !b.is_empty()) else {
        eprintln!("QUICKWIT_BIN not set; skipping");
        return;
    };
    use usnm_core::params::Filters;
    use usnm_core::query::parse;
    use usnm_core::time::{BucketSpec, BucketUnit};
    use usnm_ingest::sink::{QuickwitNode, QuickwitSink};
    use usnm_search::quickwit::QuickwitBackend;
    use usnm_search::{IndexSet, SearchBackend};

    let e = env().await;
    let pages = fixture_pages();
    let split = NaiveDate::from_ymd_opt(1897, 7, 1).unwrap();
    let (early, late): (Vec<&Page>, Vec<&Page>) = pages.iter().partition(|p| p.date < split);
    let (early, late) = (over_decades(&early), over_decades(&late));
    let archive = |name: &str, pages: &[Page]| {
        let path = e.root.join(format!("{name}.tar.gz"));
        write_archive(&path, &pages.iter().collect::<Vec<_>>(), false, true);
        listed(name, &path, None)
    };
    let qw = e.root.join("qw");
    std::fs::create_dir_all(&qw).unwrap();
    let meta = format!("file://{}/meta", qw.display());
    let root = format!("file://{}/indexes", qw.display());
    let node = QuickwitNode::start(Path::new(&bin), &qw, 7403, &meta, &root)
        .await
        .unwrap();
    let mut published = Vec::new();
    for (day, full, batch) in [
        (1, true, archive("batch_fx_early_ver01", &early)),
        (8, false, archive("batch_fx_late_ver01", &late)),
    ] {
        curate(&e, vec![batch]).await;
        let r = Release {
            partition_decade: true,
            ..e.releaser(day, full)
        };
        let mut sink = QuickwitSink::new(&node.url, &root)
            .unwrap()
            .watching(&node)
            .merges(quick_merges());
        published.push(r.run(&mut sink).await.unwrap().unwrap());
    }
    let p = published.last().unwrap();
    assert_eq!(
        e.reference_json("current.json").await["decades"],
        usnm_core::decade::VERSION
    );
    // The base: one decade a split, counted per decade in the manifest.
    let manifest = e
        .reference_json(&format!("{}/manifest.json", p.index_version))
        .await;
    let layout = manifest["indexes"].as_array().unwrap();
    assert_eq!(
        layout[0]["splits_by_decade"],
        serde_json::json!({"1830": 1, "1840": 1, "1860": 1, "1890": 1}),
        "{}",
        layout[0]
    );
    // The delta: tagged, not partitioned, so one split with every decade.
    assert_eq!(layout[1]["splits"], 1, "{}", layout[1]);
    assert!(layout[1].get("splits_by_decade").is_none(), "{}", layout[1]);
    let http = reqwest::Client::new();
    let n = usnm_ingest::merges::Node {
        http: &http,
        base: &node.url,
    };
    let delta = n.splits(&p.indexes[1]).await.unwrap();
    assert_eq!(delta[0].decades, [1840, 1860, 1890]);
    assert_eq!(delta[0].partition_id, 0);

    // Searches name their decades as the API does, and find the same pages.
    let backend = QuickwitBackend::new(&node.url, std::time::Duration::from_secs(30)).unwrap();
    let plain = IndexSet::new(p.indexes.clone());
    let by_decade = plain.clone().with_decades(Some(1830..=1890));
    let q = parse("gold").unwrap();
    let mut found = 0;
    for (from, to) in [
        ("1868-01-01", "1868-12-31"),
        ("1839-01-01", "1841-12-31"),
        ("1860-01-01", "1899-12-31"),
        ("1830-01-01", "1897-12-31"),
    ] {
        let f = Filters {
            from: NaiveDate::parse_from_str(from, "%Y-%m-%d").unwrap(),
            to: NaiveDate::parse_from_str(to, "%Y-%m-%d").unwrap(),
            states: vec![],
            lccns: vec![],
            langs: vec![],
            front_only: false,
            series: Vec::new(),
        };
        let spec = BucketSpec::new(BucketUnit::Month, f.from, f.to);
        let a = backend.summary(&by_decade, &q, &f, &spec).await.unwrap();
        let b = backend.summary(&plain, &q, &f, &spec).await.unwrap();
        assert_eq!(a, b, "{from} to {to}");
        found += a.total_hits;
    }
    assert!(found > 0);
    // The splits a search for 1868 needs: the base's 1860s split and the
    // delta's, of the five.
    let mut targeted = 0;
    for id in &p.indexes {
        targeted += n
            .splits(id)
            .await
            .unwrap()
            .iter()
            .filter(|s| s.decades.contains(&1860))
            .count();
    }
    assert_eq!(targeted, 2);
    node.stop().await.unwrap();
}

/// With a raw store (`USNM_RETAIN_RAW`), curation keeps each archive byte for
/// byte with a manifest, and a batch queued again (`enqueue --force`) is
/// curated from that copy, the source gone, to the same pages.
#[tokio::test]
async fn retained_archives_are_kept_and_read_back() {
    let e = env().await;
    let pages = fixture_pages();
    let all: Vec<&Page> = pages.iter().collect();
    let archive = e.root.join("batch_fx_raw_ver01.tar.bz2");
    write_archive(&archive, &all, false, false);
    let original = std::fs::read(&archive).unwrap();
    let sha = sha256_file(&archive);
    let raw: Arc<dyn ObjectStore> = Arc::new(LocalStore::new(e.root.join("raw")));
    let worker = |owner: &str| usnm_ingest::worker::Worker {
        raw: Some(raw.clone()),
        ..e.worker(owner)
    };
    let list = [listed("batch_fx_raw_ver01", &archive, Some(sha.clone()))];
    source::enqueue(&e.state, &list).await.unwrap();
    assert_eq!(worker("w1").run(None).await.unwrap(), 1);
    let first = e.state.batch("batch_fx_raw").await.unwrap().unwrap().0;
    let kept = raw
        .get("batch_fx_raw_ver01/batch_fx_raw_ver01.tar.bz2")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(kept, original, "byte for byte");
    let m: Value = serde_json::from_slice(
        &raw.get("batch_fx_raw_ver01/manifest.json")
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(m["sha256"], sha.as_str());
    assert_eq!(m["bytes"], original.len() as u64);
    assert_eq!(m["source_url"], archive.to_str().unwrap());

    // Queued again and curated with the source gone: from the retained copy.
    let report = source::requeue(&e.state, &list).await.unwrap();
    assert_eq!(report.requeued, 1);
    // Without --force the curated batch stays as it is.
    assert_eq!(source::enqueue(&e.state, &list).await.unwrap().unchanged, 1);
    std::fs::remove_file(&archive).unwrap();
    assert_eq!(worker("w2").run(None).await.unwrap(), 1);
    let second = e.state.batch("batch_fx_raw").await.unwrap().unwrap().0;
    let (a, b) = (first.curated.unwrap(), second.curated.unwrap());
    assert_ne!(a.parts, b.parts, "a new attempt");
    assert_eq!(
        (a.pages, a.ok_pages, a.source_sha256),
        (b.pages, b.ok_pages, b.source_sha256)
    );
}
