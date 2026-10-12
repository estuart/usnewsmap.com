//! End-to-end tests of the API against the synthetic fixture corpus.

use std::collections::{BTreeMap, HashMap};
use std::io::BufRead;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;
use usnm_api::config::Config;
use usnm_api::refdata::RefData;
use usnm_api::{app, reload_if_changed, AppState, Engine, Loader};
use usnm_core::query::parse_with;
use usnm_core::text::{tokenize, Analyzer};
use usnm_core::time::{date_from_day, day_number};
use usnm_search::memory::{eval, MemoryBackend};
use usnm_search::PageDoc;
use usnm_store::LocalStore;

fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/data")
}

fn load_docs(index_id: &str) -> Vec<PageDoc> {
    let path = data_dir().join("indexes").join(format!("{index_id}.jsonl"));
    std::io::BufReader::new(std::fs::File::open(path).unwrap())
        .lines()
        .map(|l| serde_json::from_str(&l.unwrap()).unwrap())
        .collect()
}

fn config() -> Config {
    let mut c = Config::from_lookup(|_| None).unwrap();
    c.data_dir = data_dir();
    c.reference_url = data_dir().display().to_string();
    c.cache_bytes = 16 * 1024 * 1024;
    // Tests without a client address share one bucket; rate limiting has its own test.
    c.rate_limit = None;
    c
}

async fn refdata() -> RefData {
    RefData::load(&LocalStore::new(data_dir())).await.unwrap()
}

fn fixture_backend() -> MemoryBackend {
    let mut backend = MemoryBackend::new();
    for id in ["pages-base-fixture", "pages-delta-fixture-1"] {
        backend.add_index(id, load_docs(id));
    }
    backend
}

async fn state_with_cells(max_cells: usize) -> Arc<AppState> {
    let mut cfg = config();
    cfg.max_cells = max_cells;
    Arc::new(AppState::new(
        cfg,
        Arc::new(fixture_backend()),
        refdata().await,
    ))
}

async fn state_with(indexes: Option<Vec<String>>) -> Arc<AppState> {
    let backend = fixture_backend();
    let mut refdata = refdata().await;
    if let Some(ids) = indexes {
        refdata.current.indexes = ids;
    }
    Arc::new(AppState::new(config(), Arc::new(backend), refdata))
}

async fn get(state: &Arc<AppState>, uri: &str) -> (StatusCode, axum::http::HeaderMap, Value) {
    let resp = app(state.clone())
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, headers, body)
}

fn header_str(h: &axum::http::HeaderMap, name: header::HeaderName) -> &str {
    h.get(name).and_then(|v| v.to_str().ok()).unwrap_or("")
}

/// The fixture version's analyzer: its `current.json` has no
/// `common_grams`, as versions built before analyzer versions (#168).
const FIXTURE: Analyzer = Analyzer::V1;

/// [`usnm_core::query::parse`] as the API parses for the fixture version.
fn parse(q: &str) -> Result<usnm_core::query::Node, usnm_core::query::QueryError> {
    parse_with(q, FIXTURE)
}

/// Independent brute-force count over the fixture files.
fn oracle_count(q: &str, from: &str, to: &str, indexes: &[&str]) -> u64 {
    let node = parse(q).unwrap();
    let (f, t) = (
        day_number(chrono::NaiveDate::parse_from_str(from, "%Y-%m-%d").unwrap()),
        day_number(chrono::NaiveDate::parse_from_str(to, "%Y-%m-%d").unwrap()),
    );
    indexes
        .iter()
        .flat_map(|i| load_docs(i))
        .filter(|d| d.day >= f && d.day <= t && eval(&node, &tokenize(&d.text, FIXTURE)))
        .count() as u64
}

/// A version laid out by decade (05 §5.5.5) names its decades for searches
/// to skip the other decades' splits: only at the API's own decade version,
/// over the decades of the version's bounds.
#[tokio::test]
async fn searches_name_their_decades_only_on_a_version_laid_out_with_them() {
    let mut rd = refdata().await;
    assert_eq!(rd.index_set().decades(), None);
    rd.current.decades = Some(usnm_core::decade::VERSION + 1);
    assert_eq!(rd.index_set().decades(), None);
    rd.current.decades = Some(usnm_core::decade::VERSION);
    // The fixtures run from 1895 to 1897.
    assert_eq!(rd.index_set().decades(), Some(&(1890..=1890)));
    // `current.json` written without the key reads as off, and the key
    // round-trips.
    let v = serde_json::to_value(&rd.current).unwrap();
    assert_eq!(v["decades"], usnm_core::decade::VERSION);
    // Searches return the same pages either way.
    let on = Arc::new(AppState::new(config(), Arc::new(fixture_backend()), rd));
    let off = state_with(None).await;
    for q in [
        "/v1/aggregate?q=gold&from=1896-01-01&to=1896-06-30",
        "/v1/aggregate?q=gold",
    ] {
        let (status, _, a) = get(&on, q).await;
        assert_eq!(status, StatusCode::OK, "{a}");
        let (_, _, b) = get(&off, q).await;
        assert_eq!(a["total"], b["total"], "{q}");
    }
}

/// Queries are folded with the analyzer the served version was built with
/// (#168): on a version with common-word pairs 2, `½` is the word `½`, as
/// Quickwit's `text` has it; on one with pairs 1, or none, it is `1⁄2`, as
/// those versions' pairs have it. Phrases search the pairs on either, and
/// the Japanese pages fold as their own version says.
#[tokio::test]
async fn queries_fold_as_the_served_version_was_built() {
    use usnm_api::refdata::JaIndexes;
    use usnm_core::params::RawParams;
    use usnm_core::text::Analyzers;
    let canonical = |rd: &RefData, q: &str| {
        let raw = RawParams::parse(&format!("q={q}")).unwrap();
        rd.search_request(&raw).unwrap().query.to_string()
    };
    let mut rd = refdata().await;
    // The fixture version predates the pairs: version 1, phrases in `text`.
    assert_eq!(rd.analyzers(), Analyzers::all(Analyzer::V1));
    assert!(!rd.index_set().common_grams());
    assert_eq!(canonical(&rd, "%C2%BD"), "1\u{2044}2");
    rd.current.common_grams = Some(1);
    assert!(rd.index_set().common_grams());
    assert_eq!(rd.index_set().analyzer(), Analyzer::V1);
    assert_eq!(canonical(&rd, "%C2%BD"), "1\u{2044}2");
    let v1 = rd.current.clone();
    rd.current.common_grams = Some(2);
    assert!(rd.index_set().common_grams());
    assert_eq!(rd.index_set().analyzer(), Analyzer::V2);
    assert_eq!(canonical(&rd, "%C2%BD"), "½");
    let v2 = rd.current.clone();
    // Pairs this API doesn't know: phrases stay in `text`.
    rd.current.common_grams = Some(usnm_core::common_grams::VERSION + 1);
    assert!(!rd.index_set().common_grams());
    assert_eq!(rd.index_set().analyzer(), Analyzer::LATEST);
    // The Japanese pages fold as `ja.fold` says.
    rd.current = v1.clone();
    rd.current.ja = Some(JaIndexes {
        indexes: vec!["pages-ja-fixture".into()],
        fold: 1,
        pages: 52,
    });
    assert_eq!(rd.ja_index_set().unwrap().analyzer(), Analyzer::V1);
    rd.current.ja.as_mut().unwrap().fold = 2;
    assert_eq!(rd.analyzers().ja, Analyzer::V2);
    assert_eq!(rd.ja_index_set().unwrap().analyzer(), Analyzer::V2);

    // Through the API, on the memory backend, which reads pages and
    // queries as Quickwit does: `½` finds the four market reports that print
    // it with version 2, and with version 1 (`1⁄2`, the phrase "1 2" to the
    // engine) the four that print `1/2`, as before #168. `1/2` finds those
    // on either.
    for (current, half) in [(v1, "1\u{2044}2"), (v2, "½")] {
        let mut rd = refdata().await;
        rd.current = current;
        let s = Arc::new(AppState::new(config(), Arc::new(fixture_backend()), rd));
        for (q, want) in [("%C2%BD", 4), ("1%2F2", 4)] {
            let (status, _, body) = get(&s, &format!("/v1/aggregate?q={q}")).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["total"]["hits"], want, "{q} {half}");
        }
        let (_, _, body) = get(&s, "/v1/aggregate?q=%C2%BD").await;
        assert_eq!(body["query"]["ast"], half, "{body}");
    }
}

/// American Stories' text (05 §5.5.4) is searched only for a version built
/// with it, and a hit says when its snippets come from it.
#[tokio::test]
async fn american_stories_text_is_searched_only_for_a_version_built_with_it() {
    let off = state_with(None).await;
    let mut rd = refdata().await;
    rd.current.american_stories = Some(usnm_core::american_stories::VERSION);
    let on = Arc::new(AppState::new(config(), Arc::new(fixture_backend()), rd));
    let hits = |s: &Arc<AppState>, q: &'static str| {
        let s = s.clone();
        async move {
            let (status, _, body) = get(&s, &format!("/v1/aggregate?q={q}")).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            body["total"]["hits"].as_u64().unwrap()
        }
    };
    assert_eq!(hits(&off, "bimetallism").await, 0);
    assert!(hits(&on, "bimetallism").await > 0);
    assert!(hits(&on, "gold").await > hits(&off, "gold").await);

    let (status, _, body) = get(&on, "/v1/hits?q=bimetallism&place=P00001").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body["items"].as_array().unwrap();
    assert!(!items.is_empty());
    for item in items {
        assert_eq!(item["snippet_source"], "american_stories", "{item}");
        assert!(item["snippets"][0].as_str().unwrap().contains("<mark>"));
    }
    // Snippets from LoC's text say nothing about their source, and without
    // the flag no snippet comes from American Stories' text.
    for s in [&on, &off] {
        let (_, _, body) = get(s, "/v1/hits?q=council&place=P00001&limit=50").await;
        let items = body["items"].as_array().unwrap();
        assert!(items.iter().any(|i| i.get("snippet_source").is_none()));
    }
    let (_, _, body) = get(&off, "/v1/hits?q=council&place=P00001&limit=50").await;
    let items = body["items"].as_array().unwrap();
    assert!(items.iter().all(|i| i.get("snippet_source").is_none()));
}

/// With American Stories' text searched, each hit says which texts the
/// query matches, and the aggregate counts the pages only that text
/// matches; without it, neither key is there (05 §5.5.4).
#[tokio::test]
async fn hits_say_which_texts_match_and_the_aggregate_counts_american_stories_only() {
    let off = state_with(None).await;
    let mut rd = refdata().await;
    rd.current.american_stories = Some(usnm_core::american_stories::VERSION);
    let on = Arc::new(AppState::new(config(), Arc::new(fixture_backend()), rd));

    // Every page of a place, and the aggregate's count for the same search.
    let (status, _, agg) = get(&on, "/v1/aggregate?q=gold").await;
    assert_eq!(status, StatusCode::OK, "{agg}");
    let only = agg["total"]["american_stories_only"].as_u64().unwrap();
    let total = agg["total"]["hits"].as_u64().unwrap();
    assert!(0 < only && only < total, "{only} of {total}");
    for edge in ["first", "last"] {
        assert!(agg["total"][edge]["matched_in"].is_array(), "{edge}");
    }
    let mut counted = BTreeMap::<String, u64>::new();
    for place in agg["places"]["id"].as_array().unwrap() {
        let mut cursor = String::new();
        loop {
            let uri = format!(
                "/v1/hits?q=gold&place={}&limit=50{cursor}",
                place.as_str().unwrap()
            );
            let (status, _, body) = get(&on, &uri).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            for item in body["items"].as_array().unwrap() {
                let texts: Vec<&str> = item["matched_in"]
                    .as_array()
                    .unwrap_or_else(|| panic!("{item}"))
                    .iter()
                    .map(|t| t.as_str().unwrap())
                    .collect();
                *counted.entry(texts.join("+")).or_default() += 1;
                // Snippets from American Stories' text only when LoC's has no match.
                if item["snippet_source"] == "american_stories" {
                    assert_eq!(texts, ["american_stories"], "{item}");
                }
            }
            match body["next_cursor"].as_str() {
                Some(c) => cursor = format!("&cursor={c}"),
                None => break,
            }
        }
    }
    assert_eq!(counted.values().sum::<u64>(), total);
    assert_eq!(counted.get("american_stories"), Some(&only));
    assert!(counted.get("loc").is_some_and(|n| *n > 0));
    assert!(counted.get("loc+american_stories").is_some_and(|n| *n > 0));

    // A word only American Stories' text has: every page.
    let (_, _, agg) = get(&on, "/v1/aggregate?q=bimetallism").await;
    assert_eq!(agg["total"]["american_stories_only"], agg["total"]["hits"]);

    // Off: the keys aren't there.
    let (_, _, agg) = get(&off, "/v1/aggregate?q=gold").await;
    assert!(agg["total"].get("american_stories_only").is_none());
    assert!(agg["total"]["first"].get("matched_in").is_none());
    let (_, _, body) = get(&off, "/v1/hits?q=gold&place=P00001&limit=50").await;
    let items = body["items"].as_array().unwrap();
    assert!(!items.is_empty());
    assert!(items.iter().all(|i| i.get("matched_in").is_none()));
}

/// A state on a version built with American Stories' text, searching it or
/// not (`USNM_AMERICAN_STORIES_SEARCH`).
async fn american_stories_state(search: bool) -> Arc<AppState> {
    let mut rd = refdata().await;
    rd.current.american_stories = Some(usnm_core::american_stories::VERSION);
    let mut cfg = config();
    cfg.american_stories_search = search;
    Arc::new(AppState::new(cfg, Arc::new(fixture_backend()), rd))
}

/// The search endpoints, and the reference-data ones a search links to.
const AMERICAN_STORIES_URIS: [&str; 6] = [
    "/v1/aggregate?q=gold",
    "/v1/aggregate?q=bimetallism",
    "/v1/aggregate?q=gold+-bryan&from=1896-01-01&to=1896-12-31",
    "/v1/hits?q=gold&place=P00001&limit=50",
    "/v1/hits?q=council&place=P00001&limit=50",
    "/v1/days?q=gold&place=P00001,P00006",
];

/// With `USNM_AMERICAN_STORIES_SEARCH=false`, a version built with American
/// Stories' text answers exactly as if `current.json` had no
/// `american_stories`: LoC's text alone, no `matched_in`, no
/// `snippet_source`, no `american_stories_only` (05 §5.5.4).
#[tokio::test]
async fn the_setting_switches_american_stories_text_off() {
    let switched = american_stories_state(false).await;
    let on = american_stories_state(true).await;
    let loc = state_with(None).await;
    let strip = |mut v: Value| {
        if let Some(o) = v.as_object_mut() {
            o.remove("timing_ms");
        }
        v
    };
    for uri in AMERICAN_STORIES_URIS {
        let (status, _, off) = get(&switched, uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {off}");
        let (_, _, without) = get(&loc, uri).await;
        assert_eq!(strip(off.clone()), strip(without), "{uri}");
        let (_, _, with) = get(&on, uri).await;
        assert_ne!(strip(off), strip(with), "{uri}");
    }
    let (_, _, agg) = get(&switched, "/v1/aggregate?q=bimetallism").await;
    assert_eq!(agg["total"]["hits"], 0);
    let (_, _, agg) = get(&switched, "/v1/aggregate?q=gold").await;
    assert!(agg["total"].get("american_stories_only").is_none());
    assert!(agg["total"]["first"].get("matched_in").is_none());

    // `/v1/meta` says whether searches cover the text.
    for (s, searched) in [(&switched, false), (&on, true), (&loc, false)] {
        let (_, _, meta) = get(s, "/v1/meta").await;
        assert_eq!(meta["american_stories"], searched);
    }
}

/// The keys of every response cached in process, after the cache settles.
async fn cached_keys(state: &Arc<AppState>) -> std::collections::BTreeSet<String> {
    state.cache.run_pending_tasks().await;
    state.cache.iter().map(|(k, _)| (*k).clone()).collect()
}

/// Search responses computed with American Stories' text switched off are
/// cached under keys of their own, so neither state serves the other's;
/// with it on the keys are as before the setting, and reference-data
/// responses (places, titles, coverage) share their keys.
#[tokio::test]
async fn search_cache_keys_mark_american_stories_switched_off() {
    let (on, off) = (
        american_stories_state(true).await,
        american_stories_state(false).await,
    );
    let (_, _, agg) = get(&on, "/v1/aggregate?q=gold").await;
    let coverage = agg["cube"]["baseline_ref"].as_str().unwrap().to_owned();
    let mut uris: Vec<&str> = AMERICAN_STORIES_URIS.to_vec();
    uris.extend(["/v1/places", "/v1/titles", coverage.as_str()]);
    for s in [&on, &off] {
        for uri in &uris {
            let (status, _, body) = get(s, uri).await;
            assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        }
    }
    let on_keys = cached_keys(&on).await;
    let off_keys = cached_keys(&off).await;
    // On: `{index_version}|{endpoint}|{canonical}`, unchanged.
    let canonical = agg["query"]["canonical"].as_str().unwrap();
    assert!(on_keys.contains(&format!("fixture-v1|aggregate|{canonical}")));
    assert!(on_keys.iter().all(|k| !k.contains("american_stories")));
    assert!(on_keys.iter().any(|k| k.contains("|places|")));
    assert!(on_keys.iter().any(|k| k.contains("|titles|")));
    assert!(on_keys.iter().any(|k| k.contains("|coverage|")));
    // Off: the same keys, the searches' marked.
    let expected: std::collections::BTreeSet<String> = on_keys
        .iter()
        .map(|k| {
            if k.contains("|places|") || k.contains("|titles|") || k.contains("|coverage|") {
                k.clone()
            } else {
                format!("{k}{}", usnm_api::refdata::AMERICAN_STORIES_OFF_KEY)
            }
        })
        .collect();
    assert_eq!(off_keys, expected);
    for endpoint in ["aggregate", "hits", "days"] {
        assert!(
            off_keys
                .iter()
                .any(|k| k.contains(&format!("|{endpoint}|"))
                    && k.ends_with("|american_stories=off")),
            "{endpoint}"
        );
    }

    // Japanese queries search the Japanese pages, which have no American
    // Stories text: their keys stay as they were.
    let mut cfg = config();
    cfg.american_stories_search = false;
    let ja = ja_state_with(cfg, true).await;
    for uri in ["/v1/aggregate?q=%E6%88%A6%E4%BA%89", "/v1/aggregate?q=gold"] {
        let (status, _, body) = get(&ja, uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
    }
    let ja_keys: Vec<String> = cached_keys(&ja)
        .await
        .into_iter()
        .filter(|k| k.contains("|aggregate|"))
        .collect();
    assert_eq!(ja_keys.len(), 2, "{ja_keys:?}");
    for k in &ja_keys {
        let japanese = k.contains("%E6%88%A6");
        assert_eq!(k.ends_with("|american_stories=off"), !japanese, "{k}");
    }

    // On a version without the text the setting changes nothing, and
    // neither do the keys.
    let mut cfg = config();
    cfg.american_stories_search = false;
    let plain = Arc::new(AppState::new(
        cfg,
        Arc::new(fixture_backend()),
        refdata().await,
    ));
    let (_, _, _) = get(&plain, "/v1/aggregate?q=gold").await;
    assert!(cached_keys(&plain)
        .await
        .contains(&format!("fixture-v1|aggregate|{canonical}")));
}

/// A state on a version whose main indexes search both texts in one field
/// (`text_layout` 2, 05 §5.5.7), with `USNM_AMERICAN_STORIES_SEARCH` as
/// `search` says.
async fn single_field_state(search: bool) -> Arc<AppState> {
    let mut rd = refdata().await;
    rd.current.american_stories = Some(usnm_core::american_stories::VERSION);
    rd.current.text_layout = Some(usnm_core::text_layout::TextLayout::Single.version());
    let mut cfg = config();
    cfg.american_stories_search = search;
    Arc::new(AppState::new(cfg, Arc::new(fixture_backend()), rd))
}

/// One field for both texts answers as a field per text with both
/// searched: the same pages, snippets and `matched_in`, but no
/// `total.american_stories_only`, which needs LoC's text on its own. The
/// setting can't leave American Stories' text out, so it changes nothing,
/// cache keys included.
#[tokio::test]
async fn one_text_field_answers_as_both_texts_without_the_american_stories_only_count() {
    let single = single_field_state(true).await;
    let switched = single_field_state(false).await;
    let on = american_stories_state(true).await;
    let strip = |mut v: Value| {
        if let Some(o) = v.as_object_mut() {
            o.remove("timing_ms");
            if let Some(t) = o.get_mut("total").and_then(Value::as_object_mut) {
                t.remove("american_stories_only");
            }
        }
        v
    };
    for uri in AMERICAN_STORIES_URIS {
        let (status, _, got) = get(&single, uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {got}");
        if uri.starts_with("/v1/aggregate") {
            assert!(got["total"].get("american_stories_only").is_none(), "{uri}");
        }
        let (_, _, want) = get(&on, uri).await;
        assert_eq!(strip(got.clone()), strip(want), "{uri}");
        let (_, _, off) = get(&switched, uri).await;
        assert_eq!(strip(off), strip(got), "{uri}");
    }
    let (_, _, agg) = get(&single, "/v1/aggregate?q=bimetallism").await;
    assert!(agg["total"]["hits"].as_u64() > Some(0));
    assert_eq!(
        agg["total"]["first"]["matched_in"],
        json!(["american_stories"])
    );
    for s in [&single, &switched] {
        let (_, _, meta) = get(s, "/v1/meta").await;
        assert_eq!(meta["american_stories"], true);
        assert_eq!(meta["text_layout"], 2);
        assert!(cached_keys(s)
            .await
            .iter()
            .all(|k| !k.contains("american_stories")));
    }
    let (_, _, meta) = get(&on, "/v1/meta").await;
    assert_eq!(meta["text_layout"], 1);
}

/// A version with a text layout newer than the API's isn't loaded: its
/// fields may not be the ones a search would name.
#[tokio::test]
async fn a_text_layout_this_api_does_not_know_is_not_loaded() {
    let store = LocalStore::new(data_dir());
    let mut current = refdata().await.current;
    current.common_grams = Some(usnm_core::common_grams::VERSION);
    current.american_stories = Some(usnm_core::american_stories::VERSION);
    for known in [None, Some(1), Some(2)] {
        current.text_layout = known;
        assert!(
            RefData::load_for(&store, current.clone()).await.is_ok(),
            "{known:?}"
        );
    }
    let mut newer = current.clone();
    newer.text_layout = Some(usnm_core::text_layout::TextLayout::LATEST.version() + 1);
    let err = RefData::load_for(&store, newer).await.unwrap_err();
    assert!(err.contains("text_layout"), "{err}");
    // One field for both texts holds words folded by the version's
    // analyzer, and American Stories' text of its version: both must be
    // ones the API knows, or queries would silently miss.
    for (grams, stories) in [
        (None, Some(usnm_core::american_stories::VERSION)),
        (Some(usnm_core::common_grams::VERSION + 1), None),
        (
            Some(usnm_core::common_grams::VERSION),
            Some(usnm_core::american_stories::VERSION + 1),
        ),
    ] {
        let mut c = current.clone();
        c.text_layout = Some(2);
        c.common_grams = grams;
        c.american_stories = stories;
        let err = RefData::load_for(&store, c.clone()).await.unwrap_err();
        assert!(err.contains("text_layout 2"), "{err}");
        // A field per text still loads, as before.
        c.text_layout = None;
        assert!(RefData::load_for(&store, c).await.is_ok());
    }
}

/// The persistent cache too: a response persisted with American Stories'
/// text on is never read with it off, and the other way round.
#[tokio::test]
async fn persisted_responses_stay_with_their_american_stories_state() {
    let dir = std::env::temp_dir().join(format!("usnm-respcache-as-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store: Arc<LocalStore> = Arc::new(LocalStore::new(&dir));
    let state = |search: bool| {
        let store = store.clone();
        async move {
            let mut rd = refdata().await;
            rd.current.american_stories = Some(usnm_core::american_stories::VERSION);
            let mut cfg = config();
            cfg.persist_after = Duration::ZERO;
            cfg.american_stories_search = search;
            Arc::new(AppState::new(cfg, Arc::new(fixture_backend()), rd).with_response_store(store))
        }
    };
    let wait_for = |n: usize| {
        let dir = dir.clone();
        async move {
            for _ in 0..100 {
                if persisted_files(&dir).len() == n {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            assert_eq!(persisted_files(&dir).len(), n);
        }
    };
    let uri = "/v1/aggregate?q=gold&v=fixture-v1";
    let (status, _, body) = get(&state(true).await, uri).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["total"]["american_stories_only"].is_u64());
    wait_for(1).await;
    // Off, with nothing in process: computed, not read, and persisted apart.
    let (status, _, body) = get(&state(false).await, uri).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body["total"].get("american_stories_only").is_none(),
        "{body}"
    );
    wait_for(2).await;
    // Each state reads its own entry back, with an empty index.
    for (search, only) in [(true, true), (false, false)] {
        let mut rd = refdata().await;
        rd.current.american_stories = Some(usnm_core::american_stories::VERSION);
        let mut cfg = config();
        cfg.american_stories_search = search;
        let restarted = Arc::new(
            AppState::new(cfg, Arc::new(MemoryBackend::new()), rd)
                .with_response_store(store.clone()),
        );
        let (status, _, body) = get(&restarted, uri).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["total"]["american_stories_only"].is_u64(), only);
        assert!(body["total"]["hits"].as_u64().unwrap() > 0);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The newspapers of the version (06 §6.3.11): every title in the snapshot,
/// by LCCN, with its place, years of publication, pages and languages,
/// cached and versioned like `/v1/places`.
#[tokio::test]
async fn titles_list_every_newspaper_versioned_like_places() {
    let s = state_with(None).await;
    let (status, headers, body) = get(&s, "/v1/titles").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["index_version"], "fixture-v1");
    let titles = body["titles"].as_array().unwrap();
    let lccns: Vec<&str> = titles.iter().map(|t| t["lccn"].as_str().unwrap()).collect();
    assert_eq!(
        lccns,
        [
            "sn99000001",
            "sn99000002",
            "sn99000003",
            "sn99000004",
            "sn99000005",
            "sn99000006"
        ]
    );
    assert_eq!(
        titles[1],
        json!({
            "lccn": "sn99000002",
            "title": "The Fixture Gazette 2",
            "place_id": "P00002",
            "place": "Fixture City B (New York area)",
            "state": "NY",
            "first_year": 1892,
            "last_year": 1897,
            "pages": 312,
            "first_page_date": "1895-01-05",
            "last_page_date": "1897-12-25",
            "languages": ["eng", "ger"],
        })
    );
    // The dates of its pages are the version's, apart from the years of
    // publication: every fixture title has pages from 1895 to 1897.
    assert_eq!(titles[0]["first_year"], 1891);
    assert_eq!(titles[0]["first_page_date"], "1895-01-05");
    // An open-ended run ("1896-current") has no last year.
    assert_eq!(titles[5]["first_year"], 1896);
    assert!(titles[5]["last_year"].is_null());
    // Every page of the version belongs to one title.
    let pages: u64 = titles.iter().map(|t| t["pages"].as_u64().unwrap()).sum();
    let (_, _, meta) = get(&s, "/v1/meta").await;
    assert_eq!(meta["pages"], pages);
    assert_eq!(meta["titles"], titles.len());

    // Unversioned: short cache, Content-Location pins the version.
    assert_eq!(
        header_str(&headers, header::CACHE_CONTROL),
        "public, max-age=300"
    );
    assert_eq!(
        header_str(&headers, header::CONTENT_LOCATION),
        "/v1/titles?v=fixture-v1"
    );
    // Pinned: a day, with an ETag.
    let (status, headers, pinned) = get(&s, "/v1/titles?v=fixture-v1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(pinned, body);
    assert_eq!(
        header_str(&headers, header::CACHE_CONTROL),
        "public, max-age=86400"
    );
    assert!(header_str(&headers, header::ETAG).starts_with("\"fixture-v1:"));
    // Another version: redirected to the serving one, never cached.
    let (status, headers, _) = get(&s, "/v1/titles?v=old-version").await;
    assert_eq!(status, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(header_str(&headers, header::CACHE_CONTROL), "no-store");
    assert_eq!(
        header_str(&headers, header::LOCATION),
        "/v1/titles?v=fixture-v1"
    );
    // It takes no other parameter.
    let (status, _, body) = get(&s, "/v1/titles?q=gold").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["type"], "/errors/bad-parameter");
}

/// One newspaper: the list's entry with its page at the Library of
/// Congress; 404 for a title the version doesn't have, after a stale
/// version is redirected.
#[tokio::test]
async fn one_title_with_its_link_or_not_found() {
    let s = state_with(None).await;
    let (_, _, list) = get(&s, "/v1/titles").await;
    let (status, headers, body) = get(&s, "/v1/titles/sn99000003").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let mut want = list["titles"][2].clone();
    want["index_version"] = json!("fixture-v1");
    want["links"] = json!({"loc": "https://www.loc.gov/item/sn99000003/"});
    assert_eq!(body, want);
    assert_eq!(body["languages"], json!(["spa"]));
    assert_eq!(
        header_str(&headers, header::CONTENT_LOCATION),
        "/v1/titles/sn99000003?v=fixture-v1"
    );
    let (status, headers, _) = get(&s, "/v1/titles/sn99000003?v=fixture-v1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        header_str(&headers, header::CACHE_CONTROL),
        "public, max-age=86400"
    );
    assert!(header_str(&headers, header::ETAG).starts_with("\"fixture-v1:"));
    // A stale version is redirected first, even for a title this one lacks.
    for lccn in ["sn99000003", "sn12345678"] {
        let (status, headers, _) = get(&s, &format!("/v1/titles/{lccn}?v=old-version")).await;
        assert_eq!(status, StatusCode::TEMPORARY_REDIRECT, "{lccn}");
        assert_eq!(header_str(&headers, header::CACHE_CONTROL), "no-store");
        assert_eq!(
            header_str(&headers, header::LOCATION),
            format!("/v1/titles/{lccn}?v=fixture-v1")
        );
    }
    // Unknown, or not an LCCN at all: not found, as problem details.
    for path in [
        "/v1/titles/sn12345678",
        "/v1/titles/sn12345678?v=fixture-v1",
        "/v1/titles/SN99000003",
        "/v1/titles/sn99000003x1234567",
        "/v1/titles/sn%2099000003",
    ] {
        let (status, headers, body) = get(&s, path).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        assert_eq!(
            header_str(&headers, header::CONTENT_TYPE),
            "application/problem+json"
        );
        assert_eq!(body["type"], "/errors/not-found", "{path}");
    }
    let (status, _, _) = get(&s, "/v1/titles/sn99000003?q=gold").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// A snapshot without `title_pages.json` (published before releases wrote
/// it) still lists its titles, with their pages unknown.
#[tokio::test]
async fn titles_without_pages_per_title() {
    let mut rd = refdata().await;
    rd.title_pages = None;
    let s = Arc::new(AppState::new(config(), Arc::new(fixture_backend()), rd));
    let (status, _, body) = get(&s, "/v1/titles").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let titles = body["titles"].as_array().unwrap();
    assert_eq!(titles.len(), 6);
    assert!(titles.iter().all(|t| t["pages"].is_null()));
    let (_, _, one) = get(&s, "/v1/titles/sn99000001").await;
    assert!(one["pages"].is_null());
}

#[tokio::test]
async fn health_and_meta() {
    let s = state_with(None).await;
    let (status, _, _) = get(&s, "/healthz").await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, meta) = get(&s, "/v1/meta").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(meta["index_version"], "fixture-v1");
    assert_eq!(meta["synthetic"], true);
    assert_eq!(meta["places"], 6);
    assert_eq!(meta["limits"]["min_prefix_chars"], 5);
    assert_eq!(meta["limits"]["max_at_least_items"], 6);
    assert_eq!(meta["limits"]["max_ocr_variants"], 32);
    // Every page in the version: the sum of the baselines.
    let baselines: HashMap<String, Vec<(u32, u32)>> = serde_json::from_slice(
        &std::fs::read(data_dir().join("fixture-v1/baselines.json")).unwrap(),
    )
    .unwrap();
    let pages: u64 = baselines
        .values()
        .flatten()
        .map(|&(_, n)| u64::from(n))
        .sum();
    assert!(pages > 0);
    assert_eq!(meta["pages"], pages);
    // The language filter's choices: most pages first; the title in English
    // and German counts in both.
    assert_eq!(
        meta["languages"],
        json!([
            {"code": "eng", "name": "English", "titles": 4, "pages": 1248},
            {"code": "ger", "name": "German", "titles": 2, "pages": 624},
            {"code": "spa", "name": "Spanish", "titles": 1, "pages": 312},
        ])
    );
}

/// `lang` keeps the pages of titles that list any of the given languages; a
/// title in two languages is found by either.
#[tokio::test]
async fn lang_filters_by_any_title_language() {
    let s = state_with(None).await;
    let places = |lang: &str| {
        let s = &s;
        let lang = lang.to_owned();
        async move {
            let uri = format!("/v1/aggregate?q=%22cross+of+gold%22&lang={lang}");
            let (status, _, body) = get(s, &uri).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let ids: Vec<String> = body["places"]["id"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_owned())
                .collect();
            (ids, body)
        }
    };
    let (ger, body) = places("ger").await;
    assert_eq!(ger, ["P00002", "P00006"]);
    assert!(body["query"]["canonical"]
        .as_str()
        .unwrap()
        .contains("lang=ger&"));
    let (eng, _) = places("eng").await;
    assert_eq!(eng, ["P00001", "P00002", "P00004", "P00005"]);
    // Any of the languages, in canonical (sorted) order.
    let (both, body) = places("SPA,ger").await;
    assert_eq!(both, ["P00002", "P00003", "P00006"]);
    assert!(body["query"]["canonical"]
        .as_str()
        .unwrap()
        .contains("lang=ger%2Cspa&"));
    let (none, body) = places("fre").await;
    assert!(none.is_empty());
    assert_eq!(body["total"]["hits"], 0);
}

/// Under a language filter the baseline is the pages of the titles that list
/// any of the languages, each page once: the title in English and German is in
/// both counts but not twice in their union. Every fixture title has 312 pages.
#[tokio::test]
async fn lang_filter_keeps_exact_baselines() {
    let s = state_with(None).await;
    let baseline = |lang: &'static str, extra: &'static str| {
        let s = &s;
        async move {
            let uri = format!("/v1/aggregate?q=gold&lang={lang}{extra}");
            let (status, _, body) = get(s, &uri).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let series: u64 = body["series"]["baseline"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_u64().unwrap())
                .sum();
            assert_eq!(body["total"]["baseline_pages"], series);
            (series, body)
        }
    };
    assert_eq!(baseline("eng", "").await.0, 4 * 312);
    // P00002 (English and German) and P00006 (German).
    assert_eq!(baseline("ger", "").await.0, 2 * 312);
    assert_eq!(baseline("spa", "").await.0, 312);
    // Together: P00006 added to the four English titles, P00002 once.
    assert_eq!(baseline("eng,ger", "").await.0, 5 * 312);
    // Lowercased, de-duplicated and ordered like the search itself.
    assert_eq!(baseline("GER,eng,ger", "").await.0, 5 * 312);
    assert_eq!(baseline("fre", "").await.0, 0);
    // With a state filter: only that state's places (P00002 is in New York).
    assert_eq!(baseline("ger", "&state=NY").await.0, 312);

    // The link names the same scope, and the coverage cube sums to the baseline.
    let (pages, body) = baseline("ger", "").await;
    let link = body["cube"]["baseline_ref"].as_str().unwrap().to_owned();
    assert!(link.contains("lang=ger"), "{link}");
    let (status, _, cov) = get(&s, &link).await;
    assert_eq!(status, StatusCode::OK, "{cov}");
    let covered: u64 = cov["pages"]["h"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap())
        .sum();
    assert_eq!(covered, pages);
    assert_eq!(cov["places"], json!(["P00002", "P00006"]));
    // Without the filter it is every page.
    let (status, _, all) = get(&s, "/v1/coverage?bucket=year").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(all["places"].as_array().unwrap().len(), 6);
    // Other filters still have no exact baseline, with or without a language.
    let (_, _, body) = get(&s, "/v1/aggregate?q=gold&lang=ger&front=true").await;
    assert!(body["series"]["baseline"].is_null());
    assert!(body["cube"]["baseline_ref"].is_null());
    let (_, _, body) = get(&s, "/v1/aggregate?q=gold&lang=ger&lccn=sn99000002").await;
    assert!(body["series"]["baseline"].is_null());
}

/// Without `lang`, a query whose words are English is compared with the pages
/// of titles that list English (#237), while its hits still count every
/// matching page: the fixture's German (P00006) and Spanish (P00003) titles
/// print the same English text, so they match but aren't in the baseline.
#[tokio::test]
async fn an_english_query_is_compared_with_english_pages() {
    let s = state_with(None).await;
    let (status, _, body) = get(&s, "/v1/aggregate?q=gold").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["baseline"],
        json!({"languages": ["eng"], "why": "query_language"})
    );
    // P00001, P00002 (English and German), P00004 and P00005.
    assert_eq!(body["total"]["baseline_pages"], 4 * 312);
    let series: u64 = body["series"]["baseline"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap())
        .sum();
    assert_eq!(series, 4 * 312);
    // The canonical request has no `lang`: the choice follows from the query.
    assert!(!body["query"]["canonical"]
        .as_str()
        .unwrap()
        .contains("lang="));
    let link = body["cube"]["baseline_ref"].as_str().unwrap().to_owned();
    assert!(link.contains("lang=eng"), "{link}");
    let (status, _, cov) = get(&s, &link).await;
    assert_eq!(status, StatusCode::OK, "{cov}");
    assert_eq!(
        cov["places"],
        json!(["P00001", "P00002", "P00004", "P00005"])
    );
    // The hits are not filtered: the German and Spanish titles' matches count.
    let ids = body["places"]["id"].as_array().unwrap();
    assert!(ids.contains(&json!("P00003")) && ids.contains(&json!("P00006")));
    let (_, _, filtered) = get(&s, "/v1/aggregate?q=gold&lang=eng").await;
    assert!(
        body["total"]["hits"].as_u64().unwrap() > filtered["total"]["hits"].as_u64().unwrap(),
        "{body}"
    );
    // A phrase of English words too.
    let (_, _, body) = get(&s, "/v1/aggregate?q=%22cross+of+gold%22").await;
    assert_eq!(body["baseline"]["languages"], json!(["eng"]));
}

/// An explicit `lang` decides the baseline, whatever the query's words.
#[tokio::test]
async fn a_language_filter_still_decides_the_baseline() {
    let s = state_with(None).await;
    let (status, _, body) = get(&s, "/v1/aggregate?q=gold&lang=ger").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["baseline"],
        json!({"languages": ["ger"], "why": "filter"})
    );
    assert_eq!(body["total"]["baseline_pages"], 2 * 312);
    let (_, _, body) = get(&s, "/v1/aggregate?q=convention&lang=eng,spa").await;
    assert_eq!(
        body["baseline"],
        json!({"languages": ["eng", "spa"], "why": "filter"})
    );
    assert_eq!(body["total"]["baseline_pages"], 5 * 312);
}

/// A query the word lists can't call English keeps every page as its
/// baseline: "convention" is as common in French, and a prefix matches
/// words the lists can't vouch for.
#[tokio::test]
async fn a_query_of_uncertain_language_keeps_every_page() {
    let s = state_with(None).await;
    for q in ["convention", "silve*", "convention+OR+gold"] {
        let (status, _, body) = get(&s, &format!("/v1/aggregate?q={q}")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body["total"]["hits"].as_u64().unwrap() > 0, "{q}");
        assert_eq!(
            body["baseline"],
            json!({"languages": [], "why": "all"}),
            "{q}"
        );
        assert_eq!(body["total"]["baseline_pages"], 6 * 312, "{q}");
        let link = body["cube"]["baseline_ref"].as_str().unwrap();
        assert!(!link.contains("lang="), "{link}");
    }
    // Filters without exact baselines have no baseline to describe.
    let (_, _, body) = get(&s, "/v1/aggregate?q=gold&front=true").await;
    assert!(body["baseline"].is_null());
    assert!(body["series"]["baseline"].is_null());
}

/// A version published before baselines were kept per language serves the
/// language filter without them, as it always did.
#[tokio::test]
async fn lang_filter_on_an_older_snapshot_has_no_baseline() {
    let mut refdata = refdata().await;
    refdata.language_baselines = None;
    let s = Arc::new(AppState::new(
        config(),
        Arc::new(fixture_backend()),
        refdata,
    ));
    let (status, _, body) = get(&s, "/v1/aggregate?q=gold&lang=ger").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["series"]["baseline"].is_null());
    assert!(body["cube"]["baseline_ref"].is_null());
    assert!(body["total"]["baseline_pages"].is_null());
    let (status, _, _) = get(&s, "/v1/coverage?lang=ger").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    // A request for another version is redirected, not rejected.
    let (status, headers, _) = get(&s, "/v1/coverage?lang=ger&v=pages-v-other").await;
    assert_eq!(status, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(header_str(&headers, header::CACHE_CONTROL), "no-store");
    // Searches without a language filter are unaffected: an English query
    // keeps every page as its baseline, as before #237.
    let (_, _, body) = get(&s, "/v1/aggregate?q=gold").await;
    let link = body["cube"]["baseline_ref"].as_str().unwrap();
    assert!(!link.contains("lang="), "{link}");
    assert_eq!(body["baseline"], json!({"languages": [], "why": "all"}));
    assert_eq!(body["total"]["baseline_pages"], 6 * 312);
}

#[tokio::test]
async fn aggregate_counts_match_brute_force() {
    let s = state_with(None).await;
    let uri = "/v1/aggregate?q=%22cross+of+gold%22&from=1896-06-01&to=1896-12-31";
    let (status, headers, body) = get(&s, uri).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let expected = oracle_count(
        r#""cross of gold""#,
        "1896-06-01",
        "1896-12-31",
        &["pages-base-fixture", "pages-delta-fixture-1"],
    );
    assert!(expected > 0);
    assert_eq!(body["total"]["hits"], expected);
    assert_eq!(body["bucket"]["unit"], "week");
    let series_sum: u64 = body["series"]["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap())
        .sum();
    let cube_sum: u64 = body["cube"]["h"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap())
        .sum();
    let places_sum: u64 = body["places"]["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap())
        .sum();
    assert_eq!(series_sum, expected);
    assert_eq!(cube_sum, expected);
    assert_eq!(places_sum, expected);
    assert!(
        body["series"]["baseline"].as_array().unwrap().len()
            == body["bucket"]["count"].as_u64().unwrap() as usize
    );

    // The synthetic speech spreads east before west: P00001 first, P00006 last.
    let ids: Vec<&str> = body["places"]["id"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    let first: Vec<u64> = body["places"]["first_day"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap())
        .collect();
    let fd = |id: &str| first[ids.iter().position(|x| *x == id).unwrap()];
    assert!(fd("P00001") < fd("P00003") && fd("P00003") < fd("P00006"));

    // Unversioned: short cache, Content-Location pins the version.
    assert_eq!(
        header_str(&headers, header::CACHE_CONTROL),
        "public, max-age=300"
    );
    assert!(header_str(&headers, header::CONTENT_LOCATION).ends_with("&v=fixture-v1"));
}

#[tokio::test]
async fn version_pinning_and_redirects() {
    let s = state_with(None).await;
    let (status, headers, _) = get(&s, "/v1/aggregate?q=gold&v=fixture-v1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        header_str(&headers, header::CACHE_CONTROL),
        "public, max-age=86400"
    );
    assert!(header_str(&headers, header::ETAG).starts_with("\"fixture-v1:"));

    let (status, headers, _) = get(&s, "/v1/aggregate?q=Gold&v=old-version").await;
    assert_eq!(status, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(header_str(&headers, header::CACHE_CONTROL), "no-store");
    let location = header_str(&headers, header::LOCATION);
    assert!(
        location.starts_with("/v1/aggregate?bucket=week&from=1895-01-01&q=gold&to=1897-12-31"),
        "{location}"
    );
    assert!(location.ends_with("&v=fixture-v1"));
}

#[tokio::test]
async fn index_set_isolation() {
    // A version naming only the base index must not see pages in the delta.
    let full = state_with(None).await;
    let base_only = state_with(Some(vec!["pages-base-fixture".into()])).await;
    let uri = "/v1/aggregate?q=fever&from=1895-01-01&to=1897-12-31";
    let (_, _, a) = get(&full, uri).await;
    let (_, _, b) = get(&base_only, uri).await;
    assert_eq!(
        a["total"]["hits"],
        oracle_count(
            "fever",
            "1895-01-01",
            "1897-12-31",
            &["pages-base-fixture", "pages-delta-fixture-1"]
        )
    );
    assert_eq!(
        b["total"]["hits"],
        oracle_count("fever", "1895-01-01", "1897-12-31", &["pages-base-fixture"])
    );
    assert!(a["total"]["hits"].as_u64() > b["total"]["hits"].as_u64());
}

#[tokio::test]
async fn problems_for_bad_requests() {
    let s = state_with(None).await;
    let (status, headers, body) = get(&s, "/v1/aggregate?q=text:gold").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        header_str(&headers, header::CONTENT_TYPE),
        "application/problem+json"
    );
    assert_eq!(body["type"], "/errors/query-syntax");
    assert_eq!(body["position"], 4);
    // A prefix needs 5 letters: a shorter one would walk too many words.
    let (status, headers, body) = get(&s, "/v1/aggregate?q=gold+silv*").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        header_str(&headers, header::CONTENT_TYPE),
        "application/problem+json"
    );
    assert_eq!(body["type"], "/errors/query-syntax");
    assert_eq!(body["detail"], "prefix searches need at least 5 letters");
    assert_eq!(body["position"], 5);
    let (status, _, body) = get(&s, "/v1/aggregate?q=gold+silve*").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _, body) = get(&s, "/v1/aggregate?q=gold&cachebust=1").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["type"], "/errors/bad-parameter");
    let (status, _, _) = get(&s, "/v1/hits?q=gold").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _, _) = get(&s, "/v1/places?q=gold").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _, _) = get(&s, "/v1/hits?q=gold&place=P99999").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Wildcards inside words (#124): counted like the reference, one cache key
/// per spelling, marked in snippets, and refused with a 400 where they
/// can't work.
#[tokio::test]
async fn wildcard_words_count_mark_and_refuse() {
    let s = state_with(None).await;
    let all = ["pages-base-fixture", "pages-delta-fixture-1"];
    let (status, headers, body) = get(
        &s,
        "/v1/aggregate?q=CONVE*%3FTION&from=1895-01-01&to=1897-12-31",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let expected = oracle_count("conve?*tion", "1895-01-01", "1897-12-31", &all);
    assert!(expected > 0);
    assert_eq!(body["total"]["hits"], expected);
    assert_eq!(
        expected,
        oracle_count("convention", "1895-01-01", "1897-12-31", &all)
    );
    assert!(
        header_str(&headers, header::CONTENT_LOCATION).contains("&q=conve%3F*tion&"),
        "{}",
        header_str(&headers, header::CONTENT_LOCATION)
    );

    let (status, _, page) = get(&s, "/v1/hits?q=silve%2Ar&place=P00001&limit=3").await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let snippet = page["items"][0]["snippets"][0].as_str().unwrap();
    assert!(
        snippet.to_lowercase().contains("<mark>silver</mark>"),
        "{snippet}"
    );

    for (q, detail) in [
        ("pres%3Fdent", "wildcards must follow at least 5 letters"),
        ("%2Agold", "wildcards must follow at least 5 letters"),
        ("%22presi%3Fent+lincoln%22", "inside quotes"),
        ("presi%3Fent~1", "not both"),
    ] {
        let (status, _, body) = get(&s, &format!("/v1/aggregate?q={q}")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{q}");
        assert_eq!(body["type"], "/errors/query-syntax", "{q}");
        assert!(
            body["detail"].as_str().unwrap().contains(detail),
            "{q}: {body}"
        );
        assert!(body["hint"].is_string(), "{q}");
    }
    // A question mark that ends a word is still punctuation.
    let (status, _, body) = get(&s, "/v1/aggregate?q=convention%3F&mode=phrase").await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// `word~ocr` and `N OF (...)` (#124): counted, in canonical form, and
/// refused with a syntax error where they can't work.
#[tokio::test]
async fn ocr_spellings_and_at_least_n_count_and_refuse() {
    let s = state_with(None).await;
    let all = ["pages-base-fixture", "pages-delta-fixture-1"];
    let (from, to) = ("1895-01-01", "1897-12-31");
    for (q, node, canonical) in [
        ("Gold~OCR", "gold~ocr", "gold%7Eocr"),
        (
            "2+OF+(silver+gold+bryan)",
            "2 OF (bryan gold silver)",
            "2+OF+%28bryan+gold+silver%29",
        ),
    ] {
        let (status, headers, body) =
            get(&s, &format!("/v1/aggregate?q={q}&from={from}&to={to}")).await;
        assert_eq!(status, StatusCode::OK, "{q}: {body}");
        let expected = oracle_count(node, from, to, &all);
        assert!(expected > 0, "{q}");
        assert_eq!(body["total"]["hits"], expected, "{q}");
        assert!(
            header_str(&headers, header::CONTENT_LOCATION).contains(&format!("&q={canonical}&")),
            "{q}: {}",
            header_str(&headers, header::CONTENT_LOCATION)
        );
    }
    assert!(oracle_count("gold~ocr", from, to, &all) > oracle_count("gold", from, to, &all));
    for (q, detail) in [
        ("gold~ocr*", "not both"),
        ("%22cross+of+gold%22~ocr", "not a phrase"),
        // The fixture's analyzer (V1) folds `ﷺ` to four words and `½` to
        // `1⁄2`: not one index token, so not one term-set member.
        ("%EF%B7%BA~ocr", "single word of letters and digits"),
        ("%C2%BD~ocr", "single word of letters and digits"),
        ("2+OF+(gold+silver)", "use AND"),
        ("2+OF+(gold+OR+silver+bryan)", "takes words and phrases"),
        ("2+OF+(gold-silver+bryan+coin)", "quote it as a phrase"),
        ("9+OF+(gold+silver+bryan)", "N from 2 to 5"),
    ] {
        let (status, _, body) = get(&s, &format!("/v1/aggregate?q={q}")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{q}");
        assert_eq!(body["type"], "/errors/query-syntax", "{q}");
        assert!(
            body["detail"].as_str().unwrap().contains(detail),
            "{q}: {body}"
        );
    }
}

#[tokio::test]
async fn hits_are_sorted_marked_linked_and_paginated() {
    let s = state_with(None).await;
    let base = "/v1/hits?q=%22cross+of+gold%22&from=1896-01-01&to=1896-12-31&place=P00001&limit=5";
    let (status, _, page1) = get(&s, base).await;
    assert_eq!(status, StatusCode::OK, "{page1}");
    let items = page1["items"].as_array().unwrap();
    assert_eq!(items.len(), 5);
    let dates: Vec<&str> = items.iter().map(|i| i["date"].as_str().unwrap()).collect();
    let mut sorted = dates.clone();
    sorted.sort_unstable();
    assert_eq!(dates, sorted);
    let first = &items[0];
    assert!(first["snippets"][0].as_str().unwrap().contains("<mark>"));
    assert!(first["links"]["viewer"]
        .as_str()
        .unwrap()
        .starts_with("https://www.loc.gov/resource/sn99000001/1896-"));
    assert!(first["links"]["viewer"]
        .as_str()
        .unwrap()
        .ends_with("&q=cross+of+gold"));
    assert_eq!(page1["place"]["id"], "P00001");

    let cursor = page1["next_cursor"].as_str().unwrap();
    let (_, _, page2) = get(&s, &format!("{base}&cursor={cursor}")).await;
    let second_first = page2["items"][0]["doc_id"].as_str().unwrap();
    assert!(items.iter().all(|i| i["doc_id"] != second_first));
}

/// A page whose main-index document holds our OCR's Latin text (#203,
/// the snapshot's `ja_latin.json`) is marked as our OCR in hits, and its
/// viewer link has no highlight; the other hits are LoC's as before.
#[tokio::test]
async fn hits_on_our_latin_text_are_marked_as_our_ocr() {
    use sha2::Digest;
    let base = "/v1/hits?q=gold&place=P00001&limit=5";
    let (_, _, before) = get(&state_with(None).await, base).await;
    let ours = before["items"][0]["doc_id"].as_str().unwrap().to_owned();
    let theirs = before["items"][1]["doc_id"].as_str().unwrap().to_owned();

    // The fixture snapshot with `ja_latin.json` listing the first hit.
    let dir = temp_data_dir("ja-latin");
    let body = serde_json::to_vec(&json!({"pages": {&ours: {
        "batch": "batch_fx", "loc_text": "missing", "ocr_source": "usnm-ndlocr-lite",
        "ocr_engine": "ndlocr-lite 636d1cf",
    }}}))
    .unwrap();
    std::fs::write(dir.join("fixture-v1/ja_latin.json"), &body).unwrap();
    let manifest = dir.join("fixture-v1/manifest.json");
    let mut m: Value = serde_json::from_str(&std::fs::read_to_string(&manifest).unwrap()).unwrap();
    let digest: String = sha2::Sha256::digest(&body)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    m["files"].as_array_mut().unwrap().push(json!({
        "path": "ja_latin.json", "sha256": digest, "bytes": body.len(),
    }));
    std::fs::write(&manifest, m.to_string()).unwrap();
    let rd = RefData::load(&LocalStore::new(&dir)).await.unwrap();
    assert_eq!(rd.ja_latin.len(), 1);
    let s = Arc::new(AppState::new(config(), Arc::new(fixture_backend()), rd));

    let (status, _, after) = get(&s, base).await;
    assert_eq!(status, StatusCode::OK, "{after}");
    assert_eq!(
        after["total"], before["total"],
        "the same pages, counted the same"
    );
    let item = |id: &str| {
        after["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["doc_id"] == id)
            .unwrap()
            .clone()
    };
    let it = item(&ours);
    assert_eq!(
        it["ocr"],
        json!({"source": "usnm-ndlocr-lite", "engine": "ndlocr-lite 636d1cf"})
    );
    assert!(!it["links"]["viewer"].as_str().unwrap().contains("&q="));
    let it = item(&theirs);
    assert!(it.get("ocr").is_none());
    assert!(it["links"]["viewer"].as_str().unwrap().ends_with("&q=gold"));

    // When the version hides our copy (another batch's copy of the page
    // wins), its hits are that copy's, not ours.
    let hidden = serde_json::to_vec(&json!([{"doc_id": &ours, "batch": "batch_fx"}])).unwrap();
    std::fs::write(dir.join("fixture-v1/duplicates.json"), &hidden).unwrap();
    let digest: String = sha2::Sha256::digest(&hidden)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let files = m["files"].as_array_mut().unwrap();
    files.retain(|f| f["path"] != "duplicates.json");
    files.push(json!({"path": "duplicates.json", "sha256": digest, "bytes": hidden.len()}));
    std::fs::write(&manifest, m.to_string()).unwrap();
    let rd = RefData::load(&LocalStore::new(&dir)).await.unwrap();
    assert!(rd.ja_latin.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A page whose main-index document holds our own OCR in place of LoC's
/// text (12 §12.5 Step 9, the snapshot's `ocr_local_pages.bin`) is marked
/// as our OCR in hits, with its engine and a viewer link without
/// highlights; the other hits are LoC's as before. A copy the version hides
/// isn't marked.
#[tokio::test]
async fn hits_on_our_ocr_in_place_of_locs_are_marked_as_our_ocr() {
    use sha2::Digest;
    use usnm_state::ocr_local::{Builder, Engine, OCR_LOCAL_PAGES_FILE};
    let base = "/v1/hits?q=gold&place=P00001&limit=5";
    let (_, _, before) = get(&state_with(None).await, base).await;
    let ours = before["items"][0]["doc_id"].as_str().unwrap().to_owned();
    let theirs = before["items"][1]["doc_id"].as_str().unwrap().to_owned();

    let dir = temp_data_dir("ocr-local");
    let mut pages = Builder::default();
    pages
        .add(
            &ours,
            Engine {
                source: "usnm-tesseract".into(),
                engine: "tesseract/5.5.1 eng".into(),
            },
            1,
        )
        .unwrap();
    let body = pages.finish().encode().unwrap();
    std::fs::write(dir.join("fixture-v1").join(OCR_LOCAL_PAGES_FILE), &body).unwrap();
    let manifest = dir.join("fixture-v1/manifest.json");
    let mut m: Value = serde_json::from_str(&std::fs::read_to_string(&manifest).unwrap()).unwrap();
    let digest = |b: &[u8]| -> String {
        sha2::Sha256::digest(b)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    };
    m["files"].as_array_mut().unwrap().push(json!({
        "path": OCR_LOCAL_PAGES_FILE, "sha256": digest(&body), "bytes": body.len(),
    }));
    // The file names batches by their place in the version's list.
    m["built_from"] = json!({"batches": [
        {"batch": "batch_fx_early", "version": 1, "counts": "x"},
        {"batch": "batch_fx_late", "version": 1, "counts": "y"},
    ], "american_stories": {
        "version": 1, "years": 2, "parts": 5, "bytes": 100, "docs": 40, "only_american_stories": 3,
    }});
    std::fs::write(&manifest, m.to_string()).unwrap();
    let rd = RefData::load(&LocalStore::new(&dir)).await.unwrap();
    assert_eq!(rd.ocr_local.len(), 1);
    // The manifest's American Stories summary is kept for the status page.
    assert_eq!(
        rd.american_stories_built,
        Some(usnm_api::refdata::AmericanStoriesBuilt {
            years: 2,
            docs: 40,
            only_american_stories: 3,
        })
    );
    let s = Arc::new(AppState::new(config(), Arc::new(fixture_backend()), rd));

    let (status, _, after) = get(&s, base).await;
    assert_eq!(status, StatusCode::OK, "{after}");
    assert_eq!(after["total"], before["total"]);
    let item = |id: &str| {
        after["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["doc_id"] == id)
            .unwrap()
            .clone()
    };
    let it = item(&ours);
    assert_eq!(
        it["ocr"],
        json!({"source": "usnm-tesseract", "engine": "tesseract/5.5.1 eng"})
    );
    assert!(!it["links"]["viewer"].as_str().unwrap().contains("&q="));
    let it = item(&theirs);
    assert!(it.get("ocr").is_none());
    assert!(it["links"]["viewer"].as_str().unwrap().ends_with("&q=gold"));

    // Another batch's copy hidden changes nothing; ours hidden drops it.
    for (batch, kept) in [("batch_fx_early", 1), ("batch_fx_late", 0)] {
        let hidden = serde_json::to_vec(&json!([{"doc_id": &ours, "batch": batch}])).unwrap();
        std::fs::write(dir.join("fixture-v1/duplicates.json"), &hidden).unwrap();
        let files = m["files"].as_array_mut().unwrap();
        files.retain(|f| f["path"] != "duplicates.json");
        files.push(json!({
            "path": "duplicates.json", "sha256": digest(&hidden), "bytes": hidden.len(),
        }));
        std::fs::write(&manifest, m.to_string()).unwrap();
        let rd = RefData::load(&LocalStore::new(&dir)).await.unwrap();
        assert_eq!(rd.ocr_local.len(), kept, "{batch}");
    }

    // A file naming a batch the version doesn't have is refused.
    m["built_from"] = json!({"batches": [{"batch": "batch_fx_early", "version": 1}]});
    std::fs::write(&manifest, m.to_string()).unwrap();
    assert!(RefData::load(&LocalStore::new(&dir)).await.is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Matching pages per day for some places, against a brute-force pass over
/// the fixture files and the aggregate's per-place totals.
#[tokio::test]
async fn days_per_place_match_brute_force() {
    let s = state_with(None).await;
    let search = "q=%22cross+of+gold%22&from=1896-01-01&to=1896-12-31";
    let (status, headers, body) = get(&s, &format!("/v1/days?{search}&place=P00006,P00001")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let places = body["places"].as_array().unwrap();
    // In the order asked.
    assert_eq!(places.len(), 2);
    assert_eq!(places[0]["id"], "P00006");
    assert_eq!(places[1]["id"], "P00001");

    let node = parse(r#""cross of gold""#).unwrap();
    let (from, to) = (
        day_number(chrono::NaiveDate::from_ymd_opt(1896, 1, 1).unwrap()),
        day_number(chrono::NaiveDate::from_ymd_opt(1896, 12, 31).unwrap()),
    );
    let (_, _, agg) = get(&s, &format!("/v1/aggregate?{search}")).await;
    let agg_ids: Vec<&str> = agg["places"]["id"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    for p in places {
        let id = p["id"].as_str().unwrap();
        let mut expected: BTreeMap<u32, u64> = BTreeMap::new();
        for d in ["pages-base-fixture", "pages-delta-fixture-1"]
            .iter()
            .flat_map(|i| load_docs(i))
            .filter(|d| d.place_id == id && d.day >= from && d.day <= to)
            .filter(|d| eval(&node, &tokenize(&d.text, FIXTURE)))
        {
            *expected.entry(d.day).or_default() += 1;
        }
        assert!(!expected.is_empty(), "{id}");
        let days: Vec<u32> = p["days"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_u64().unwrap() as u32)
            .collect();
        let hits: Vec<u64> = p["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_u64().unwrap())
            .collect();
        assert_eq!(days, expected.keys().copied().collect::<Vec<_>>(), "{id}");
        assert_eq!(hits, expected.values().copied().collect::<Vec<_>>(), "{id}");
        // Same day numbers and totals as the aggregate's.
        let i = agg_ids.iter().position(|x| *x == id).unwrap();
        assert_eq!(agg["places"]["first_day"][i], days[0]);
        assert_eq!(agg["places"]["last_day"][i], *days.last().unwrap());
        assert_eq!(agg["places"]["hits"][i], hits.iter().sum::<u64>());
    }

    // Unversioned: short cache, Content-Location pins the version, by day
    // whatever `bucket` said, with the places in the order asked.
    assert_eq!(
        header_str(&headers, header::CACHE_CONTROL),
        "public, max-age=300"
    );
    let location = header_str(&headers, header::CONTENT_LOCATION);
    assert!(
        location.starts_with("/v1/days?bucket=day&from=1896-01-01&q="),
        "{location}"
    );
    assert!(
        location.ends_with("&place=P00006%2CP00001&v=fixture-v1"),
        "{location}"
    );
    let (_, h2, again) = get(
        &s,
        &format!("/v1/days?{search}&bucket=week&place=P00006,P00001"),
    )
    .await;
    assert_eq!(again, body);
    assert_eq!(header_str(&h2, header::CONTENT_LOCATION), location);

    // Pinned: cached for a day; another version redirects to this one.
    let (status, headers, _) =
        get(&s, &format!("/v1/days?{search}&place=P00001&v=fixture-v1")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        header_str(&headers, header::CACHE_CONTROL),
        "public, max-age=86400"
    );
    let (status, headers, _) = get(&s, &format!("/v1/days?{search}&place=P00001&v=old")).await;
    assert_eq!(status, StatusCode::TEMPORARY_REDIRECT);
    let location = header_str(&headers, header::LOCATION);
    assert!(location.starts_with("/v1/days?bucket=day&"), "{location}");
    assert!(
        location.ends_with("&place=P00001&v=fixture-v1"),
        "{location}"
    );

    // A place without matches is listed with empty lists.
    let (status, _, body) = get(
        &s,
        &format!("/v1/days?{search}&lccn=sn99000001&place=P00002,P00001"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["places"][0],
        json!({"id": "P00002", "days": [], "hits": []})
    );
    assert!(!body["places"][1]["days"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn days_problems_for_bad_requests() {
    let s = state_with(None).await;
    let many: Vec<String> = (1..=21).map(|i| format!("P{i:05}")).collect();
    for (query, status, kind) in [
        ("q=gold", StatusCode::BAD_REQUEST, "/errors/bad-parameter"),
        (
            "q=gold&place=",
            StatusCode::BAD_REQUEST,
            "/errors/bad-parameter",
        ),
        (
            "q=gold&place=P00001,P00002,P00001",
            StatusCode::BAD_REQUEST,
            "/errors/bad-parameter",
        ),
        (
            &format!("q=gold&place={}", many.join(",")),
            StatusCode::BAD_REQUEST,
            "/errors/bad-parameter",
        ),
        (
            "q=gold&place=P00001&limit=5",
            StatusCode::BAD_REQUEST,
            "/errors/bad-parameter",
        ),
        (
            "q=gold&place=P00001&bucket=decade",
            StatusCode::BAD_REQUEST,
            "/errors/bad-parameter",
        ),
        (
            "place=P00001",
            StatusCode::BAD_REQUEST,
            "/errors/bad-parameter",
        ),
        (
            "q=text:gold&place=P00001",
            StatusCode::BAD_REQUEST,
            "/errors/query-syntax",
        ),
        (
            "q=gold&place=P99999",
            StatusCode::NOT_FOUND,
            "/errors/not-found",
        ),
        (
            "q=gold&place=P00001,P%20OR%20x",
            StatusCode::NOT_FOUND,
            "/errors/not-found",
        ),
    ] {
        let (got, headers, body) = get(&s, &format!("/v1/days?{query}")).await;
        assert_eq!(got, status, "{query}: {body}");
        assert_eq!(body["type"], kind, "{query}");
        assert_eq!(
            header_str(&headers, header::CONTENT_TYPE),
            "application/problem+json"
        );
    }
    // More day cells than the budget → 422, and nothing asked by day.
    let tiny = state_with_cells(5).await;
    let (status, _, body) = get(
        &tiny,
        "/v1/days?q=gold&from=1896-01-01&to=1896-12-31&place=P00001",
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["type"], "/errors/query-too-broad");
    assert!(body["detail"].as_str().unwrap().contains("days"), "{body}");
}

/// Newspapers and languages (#121) against a brute-force pass over the
/// fixture files: counts, order (most first, ties by key), the catalog's
/// names and places, and languages counted once per language a paper lists.
#[tokio::test]
async fn newspapers_and_languages_match_brute_force() {
    let s = state_with(None).await;
    let (status, _, body) = get(&s, "/v1/aggregate?q=gold&from=1895-01-01&to=1897-12-31").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let node = parse("gold").unwrap();
    let (f, t) = (
        day_number(chrono::NaiveDate::from_ymd_opt(1895, 1, 1).unwrap()),
        day_number(chrono::NaiveDate::from_ymd_opt(1897, 12, 31).unwrap()),
    );
    let mut papers: BTreeMap<String, u64> = BTreeMap::new();
    let mut langs: BTreeMap<String, u64> = BTreeMap::new();
    for d in ["pages-base-fixture", "pages-delta-fixture-1"]
        .iter()
        .flat_map(|i| load_docs(i))
        .filter(|d| d.day >= f && d.day <= t && eval(&node, &tokenize(&d.text, FIXTURE)))
    {
        *papers.entry(d.lccn.clone()).or_default() += 1;
        for l in d.language {
            *langs.entry(l).or_default() += 1;
        }
    }
    let ranked = |m: BTreeMap<String, u64>| {
        let mut v: Vec<(String, u64)> = m.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v
    };
    let column = |v: &Value| -> Vec<Value> { v.as_array().unwrap().clone() };
    let got_papers: Vec<(String, u64)> = column(&body["papers"]["lccn"])
        .iter()
        .zip(column(&body["papers"]["hits"]))
        .map(|(l, h)| (l.as_str().unwrap().to_owned(), h.as_u64().unwrap()))
        .collect();
    let want_papers = ranked(papers);
    assert!(want_papers.len() > 1);
    assert_eq!(got_papers, want_papers);
    assert_eq!(body["total"]["papers"], want_papers.len());
    let got_langs: Vec<(String, u64)> = column(&body["languages"]["code"])
        .iter()
        .zip(column(&body["languages"]["hits"]))
        .map(|(l, h)| (l.as_str().unwrap().to_owned(), h.as_u64().unwrap()))
        .collect();
    let want_langs = ranked(langs);
    // Multilingual papers count in each of their languages.
    assert!(want_langs.iter().map(|l| l.1).sum::<u64>() > body["total"]["hits"].as_u64().unwrap());
    assert_eq!(got_langs, want_langs);
    // Names and places come from the catalog.
    let titles: Vec<Value> =
        serde_json::from_slice(&std::fs::read(data_dir().join("fixture-v1/titles.json")).unwrap())
            .unwrap();
    for (i, lccn) in column(&body["papers"]["lccn"]).iter().enumerate() {
        let t = titles.iter().find(|t| t["lccn"] == *lccn).unwrap();
        assert_eq!(body["papers"]["title"][i], t["name"]);
        assert_eq!(body["papers"]["place_id"][i], t["place_id"]);
    }
}

/// Distinct days (#127) against a brute-force pass: the whole search on
/// `/v1/aggregate`, one place on the first page of `/v1/hits` (not later ones).
#[tokio::test]
async fn distinct_days_match_brute_force() {
    let s = state_with(None).await;
    let node = parse("gold").unwrap();
    let (f, t) = (
        day_number(chrono::NaiveDate::from_ymd_opt(1895, 1, 1).unwrap()),
        day_number(chrono::NaiveDate::from_ymd_opt(1897, 12, 31).unwrap()),
    );
    let docs: Vec<PageDoc> = ["pages-base-fixture", "pages-delta-fixture-1"]
        .iter()
        .flat_map(|i| load_docs(i))
        .filter(|d| d.day >= f && d.day <= t && eval(&node, &tokenize(&d.text, FIXTURE)))
        .collect();
    let days = |place: Option<&str>| {
        docs.iter()
            .filter(|d| place.is_none_or(|p| d.place_id == p))
            .map(|d| d.day)
            .collect::<std::collections::BTreeSet<_>>()
            .len()
    };
    let (_, _, body) = get(&s, "/v1/aggregate?q=gold&from=1895-01-01&to=1897-12-31").await;
    assert!(days(None) > 1);
    assert_eq!(body["total"]["days"], days(None));
    let base = "/v1/hits?q=gold&from=1895-01-01&to=1897-12-31&place=P00001&limit=5";
    let (status, _, first) = get(&s, base).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["days"], days(Some("P00001")));
    let cursor = first["next_cursor"].as_str().unwrap();
    let (_, _, next) = get(&s, &format!("{base}&cursor={cursor}")).await;
    assert!(next.get("days").is_none(), "{next}");
}

/// The newest-first list is the oldest-first list reversed, including the
/// order of pages on the same day. `sort=oldest` is the default and shares its
/// canonical URL; anything else is refused.
#[tokio::test]
async fn hits_sort_oldest_or_newest() {
    let s = state_with(None).await;
    let base = "/v1/hits?q=gold&from=1895-01-01&to=1897-12-31&place=P00001";
    // Every page of the list, following the cursor.
    let all = |sort: &'static str| {
        let s = &s;
        async move {
            let mut ids = Vec::new();
            let mut cursor: Option<String> = None;
            loop {
                let mut uri = format!("{base}{sort}");
                if let Some(c) = &cursor {
                    uri.push_str(&format!("&cursor={c}"));
                }
                let (status, _, body) = get(s, &uri).await;
                assert_eq!(status, StatusCode::OK, "{body}");
                for i in body["items"].as_array().unwrap() {
                    ids.push(i["doc_id"].as_str().unwrap().to_owned());
                }
                match body["next_cursor"].as_str() {
                    Some(c) => cursor = Some(c.to_owned()),
                    None => break ids,
                }
            }
        }
    };
    let oldest = all("").await;
    assert!(oldest.len() > 1);
    assert_eq!(all("&sort=oldest").await, oldest);
    let mut reversed = oldest.clone();
    reversed.reverse();
    assert_eq!(all("&sort=newest").await, reversed);
    // Most mentions first (#126): the same pages in another order.
    let mut relevant = all("&sort=relevant").await;
    relevant.sort();
    let mut sorted = oldest.clone();
    sorted.sort();
    assert_eq!(relevant, sorted);

    let (_, plain, _) = get(&s, base).await;
    let (_, explicit, _) = get(&s, &format!("{base}&sort=oldest")).await;
    let (_, newest, _) = get(&s, &format!("{base}&sort=newest")).await;
    assert_eq!(
        header_str(&explicit, header::CONTENT_LOCATION),
        header_str(&plain, header::CONTENT_LOCATION)
    );
    assert!(header_str(&newest, header::CONTENT_LOCATION).contains("&sort=newest"));
    let (_, relevant, _) = get(&s, &format!("{base}&sort=relevant")).await;
    assert!(header_str(&relevant, header::CONTENT_LOCATION).contains("&sort=relevant"));

    let (status, _, body) = get(&s, &format!("{base}&sort=sideways")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["type"], "/errors/bad-parameter");
}

/// National and per-place first and last days, and the first and last pages,
/// against a brute-force pass over the fixture files.
#[tokio::test]
async fn first_and_last_mentions_match_brute_force() {
    let s = state_with(None).await;
    let docs: Vec<PageDoc> = ["pages-base-fixture", "pages-delta-fixture-1"]
        .iter()
        .flat_map(|i| load_docs(i))
        .collect();
    let day = |iso: &str| day_number(chrono::NaiveDate::parse_from_str(iso, "%Y-%m-%d").unwrap());
    for (q, qs, from, to) in [
        ("gold", "gold", "1895-01-01", "1897-12-31"),
        (
            r#""cross of gold""#,
            "%22cross+of+gold%22",
            "1896-06-01",
            "1896-12-31",
        ),
        ("fever", "fever", "1896-07-01", "1896-09-30"),
        ("zyzzyva", "zyzzyva", "1895-01-01", "1897-12-31"),
    ] {
        let node = parse(q).unwrap();
        let (f, t) = (day(from), day(to));
        let matching: Vec<&PageDoc> = docs
            .iter()
            .filter(|d| d.day >= f && d.day <= t && eval(&node, &tokenize(&d.text, FIXTURE)))
            .collect();
        let first = matching.iter().copied().min_by_key(|d| (d.day, d.sort_key));
        let last = matching.iter().copied().max_by_key(|d| (d.day, d.sort_key));
        let mut places: HashMap<&str, (u32, u32)> = HashMap::new();
        for d in &matching {
            let e = places.entry(&d.place_id).or_insert((d.day, d.day));
            e.0 = e.0.min(d.day);
            e.1 = e.1.max(d.day);
        }

        let uri = format!("/v1/aggregate?q={qs}&from={from}&to={to}");
        let (status, _, body) = get(&s, &uri).await;
        assert_eq!(status, StatusCode::OK, "{q}: {body}");
        let total = &body["total"];
        assert_eq!(total["first_day"], json!(first.map(|d| d.day)), "{q}");
        assert_eq!(total["last_day"], json!(last.map(|d| d.day)), "{q}");
        assert_eq!(
            total["first"]["doc_id"],
            json!(first.map(|d| &d.doc_id)),
            "{q}"
        );
        assert_eq!(
            total["last"]["doc_id"],
            json!(last.map(|d| &d.doc_id)),
            "{q}"
        );
        if let Some(d) = first {
            assert_eq!(total["first"]["place_id"], d.place_id.as_str());
            assert_eq!(total["first"]["date"], date_from_day(d.day).to_string());
            assert!(total["first"]["links"]["viewer"].is_string());
        }

        let column =
            |name: &str| -> Vec<Value> { body["places"][name].as_array().unwrap().clone() };
        let ids = column("id");
        assert_eq!(ids.len(), places.len(), "{q}");
        for (i, id) in ids.iter().enumerate() {
            let (lo, hi) = places[id.as_str().unwrap()];
            assert_eq!(column("first_day")[i], lo, "{q} {id}");
            assert_eq!(column("last_day")[i], hi, "{q} {id}");
        }
    }
}

#[tokio::test]
async fn hits_by_title_uses_lccn() {
    let s = state_with(None).await;
    let (status, _, body) = get(&s, "/v1/hits?q=gold&lccn=sn99000002&limit=3").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["title"]["lccn"], "sn99000002");
    assert!(body["items"]
        .as_array()
        .unwrap()
        .iter()
        .all(|i| i["lccn"] == "sn99000002"));
    let (status, _, _) = get(&s, "/v1/hits?q=gold&lccn=sn99000001,sn99000002").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _, _) = get(&s, "/v1/hits?q=gold&lccn=sn99999999").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // With `place`, `lccn` is just a filter.
    let (status, _, body) = get(&s, "/v1/hits?q=gold&place=P00001&lccn=sn99000002").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 0);
}

async fn browsing_state(on: bool) -> Arc<AppState> {
    let mut cfg = config();
    cfg.browse_pages = on;
    Arc::new(AppState::new(
        cfg,
        Arc::new(fixture_backend()),
        refdata().await,
    ))
}

/// A title's pages without a query (#274): only with `USNM_BROWSE_PAGES`,
/// which `/v1/meta` reports; every page of the one title, by date, each
/// with its opening and an unhighlighted viewer link.
#[tokio::test]
async fn a_titles_pages_are_listed_without_a_query_only_when_switched_on() {
    let off = state_with(None).await;
    let (_, _, meta) = get(&off, "/v1/meta").await;
    assert_eq!(meta["browse_pages"], false);
    let (status, _, body) = get(&off, "/v1/hits?lccn=sn99000002").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body["detail"].as_str().unwrap().contains("`q`"), "{body}");

    let s = browsing_state(true).await;
    let (_, _, meta) = get(&s, "/v1/meta").await;
    assert_eq!(meta["browse_pages"], true);
    let mut days: Vec<u32> = ["pages-base-fixture", "pages-delta-fixture-1"]
        .iter()
        .flat_map(|i| load_docs(i))
        .filter(|d| d.lccn == "sn99000002")
        .map(|d| d.day)
        .collect();
    days.sort_unstable();
    let (status, _, body) = get(&s, "/v1/hits?lccn=sn99000002&limit=5").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], days.len());
    assert_eq!(body["title"]["lccn"], "sn99000002");
    assert_eq!(body["place"], Value::Null);
    assert!(body["days"].as_u64().unwrap() > 0);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 5);
    let first = date_from_day(days[0]).to_string();
    assert_eq!(items[0]["date"], first.as_str());
    for i in items {
        assert_eq!(i["lccn"], "sn99000002");
        let snippets = i["snippets"].as_array().unwrap();
        assert_eq!(snippets.len(), 1, "{i}");
        assert!(!snippets[0].as_str().unwrap().contains("<mark>"), "{i}");
        assert!(i.get("matched_in").is_none());
        let viewer = i["links"]["viewer"].as_str().unwrap();
        assert!(!viewer.contains("&q="), "{viewer}");
    }
    // The next page carries on from the cursor, and newest first ends where
    // oldest first began.
    let cursor = body["next_cursor"].as_str().unwrap();
    let (status, _, next) = get(
        &s,
        &format!("/v1/hits?lccn=sn99000002&limit=5&cursor={cursor}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(next.get("days").is_none());
    let last = date_from_day(*days.last().unwrap()).to_string();
    let (_, _, newest) = get(&s, "/v1/hits?lccn=sn99000002&limit=1&sort=newest").await;
    assert_eq!(newest["items"][0]["date"], last.as_str());
    // Dates and the other filters narrow the list.
    let (_, _, narrowed) = get(
        &s,
        "/v1/hits?lccn=sn99000002&from=1896-01-01&to=1896-12-31&front=true&limit=50",
    )
    .await;
    let n = narrowed["total"].as_u64().unwrap();
    assert!(n > 0 && n < days.len() as u64, "{n}");
    assert!(narrowed["items"]
        .as_array()
        .unwrap()
        .iter()
        .all(|i| i["front_page"] == true && i["date"].as_str().unwrap().starts_with("1896")));

    // One title, no place, no relevance, nothing that needs a query.
    for uri in [
        "/v1/hits",
        "/v1/hits?lccn=sn99000001,sn99000002",
        "/v1/hits?lccn=sn99000002&place=P00002",
        "/v1/hits?place=P00002",
        "/v1/hits?lccn=sn99000002&sort=relevant",
        "/v1/hits?lccn=sn99000002&mode=all",
        "/v1/hits?lccn=sn99000002&bucket=year",
        "/v1/hits?lccn=sn99000002&source=congress",
    ] {
        let (status, _, body) = get(&s, uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}: {body}");
    }
    let (status, _, _) = get(&s, "/v1/hits?lccn=sn99999999").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = get(&s, "/v1/hits?lccn=sn99999999&v=fixture-v1").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // A stale `v` is redirected before the title is looked up: a title of
    // an older version may be gone from this one, and the client learns of
    // the new version from the redirect, not from a 404 at the old URL.
    let (status, headers, body) = get(&s, "/v1/hits?lccn=sn99999999&v=old-version").await;
    assert_eq!(status, StatusCode::TEMPORARY_REDIRECT, "{body}");
    assert_eq!(header_str(&headers, header::CACHE_CONTROL), "no-store");
    let location = header_str(&headers, header::LOCATION);
    assert!(
        location.contains("lccn=sn99999999") && location.ends_with("&v=fixture-v1"),
        "{location}"
    );
    // A search with `q` answers as before, under a key of its own.
    let (status, _, searched) = get(&s, "/v1/hits?q=gold&lccn=sn99000002&limit=5").await;
    assert_eq!(status, StatusCode::OK);
    assert!(searched["total"].as_u64().unwrap() < days.len() as u64);
    // A stale `v` is redirected to the current version's canonical URL.
    let resp = app(s.clone())
        .oneshot(
            Request::builder()
                .uri("/v1/hits?lccn=sn99000002&v=old-version")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::TEMPORARY_REDIRECT);
    let location = header_str(resp.headers(), header::LOCATION);
    assert!(
        location.contains("lccn=sn99000002") && !location.contains("q="),
        "{location}"
    );
}

/// A title's pages without a query go on to the last one however deep
/// (#274): their cursor is the last page's key, not an offset, so the
/// offset cap that stops a search's "next" doesn't apply. With the cap
/// lowered to a few pages, walking the cursors lists every page of the
/// title once, in order, same-day pages included, in both orders, while a
/// search of the same title stops at the cap.
#[tokio::test]
async fn browsing_a_titles_pages_reaches_the_last_page_past_the_offset_cap() {
    let mut cfg = config();
    cfg.browse_pages = true;
    cfg.max_offset = 4;
    let s = Arc::new(AppState::new(
        cfg,
        Arc::new(fixture_backend()),
        refdata().await,
    ));
    let mut docs: Vec<PageDoc> = ["pages-base-fixture", "pages-delta-fixture-1"]
        .iter()
        .flat_map(|i| load_docs(i))
        .filter(|d| d.lccn == "sn99000002")
        .collect();
    docs.sort_by_key(|d| (d.day, d.sort_key));
    let oldest: Vec<String> = docs.iter().map(|d| d.doc_id.clone()).collect();
    assert!(oldest.len() > 4 + 3, "too few pages to pass the cap");
    assert!(
        docs.windows(2).any(|w| w[0].day == w[1].day),
        "no two pages on one day"
    );
    let mut newest = oldest.clone();
    newest.reverse();
    for (sort, want) in [("oldest", &oldest), ("newest", &newest)] {
        let mut walked: Vec<String> = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..=want.len() {
            let uri = match &cursor {
                None => format!("/v1/hits?lccn=sn99000002&limit=3&sort={sort}"),
                Some(c) => format!("/v1/hits?lccn=sn99000002&limit=3&sort={sort}&cursor={c}"),
            };
            let (status, _, body) = get(&s, &uri).await;
            assert_eq!(status, StatusCode::OK, "{uri}: {body}");
            // The pages from this one on, as the backend counts them.
            assert_eq!(body["total"], want.len() - walked.len(), "{uri}");
            walked.extend(
                body["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|i| i["doc_id"].as_str().unwrap().to_owned()),
            );
            match body["next_cursor"].as_str() {
                Some(c) => cursor = Some(c.to_owned()),
                None => break,
            }
        }
        assert_eq!(&walked, want, "{sort}");
    }

    // A search of the same title stops at the cap.
    let (_, _, searched) = get(&s, "/v1/hits?q=the&lccn=sn99000002&limit=3").await;
    assert!(
        searched["total"].as_u64().unwrap() > 6,
        "{}",
        searched["total"]
    );
    let search_cursor = searched["next_cursor"].as_str().unwrap().to_owned();
    let (_, _, deeper) = get(
        &s,
        &format!("/v1/hits?q=the&lccn=sn99000002&limit=3&cursor={search_cursor}"),
    )
    .await;
    assert_eq!(deeper["next_cursor"], Value::Null, "{deeper}");

    // A cursor that isn't this list's, in this order, is refused.
    let (_, _, first) = get(&s, "/v1/hits?lccn=sn99000002&limit=3").await;
    let browse_cursor = first["next_cursor"].as_str().unwrap().to_owned();
    // A cursor carries no count a client could set: one with `n` is refused.
    let decoded: Value = serde_json::from_slice(
        &base64::Engine::decode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            &browse_cursor,
        )
        .unwrap(),
    )
    .unwrap();
    let mut forged = decoded.clone();
    forged["n"] = json!(u64::MAX);
    let forged = base64::Engine::encode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        serde_json::to_vec(&forged).unwrap(),
    );
    for uri in [
        format!("/v1/hits?lccn=sn99000002&cursor={forged}"),
        "/v1/hits?lccn=sn99000002&cursor=not-a-cursor".to_owned(),
        format!("/v1/hits?lccn=sn99000002&cursor={search_cursor}"),
        format!("/v1/hits?lccn=sn99000002&sort=newest&cursor={browse_cursor}"),
        format!("/v1/hits?q=the&lccn=sn99000002&cursor={browse_cursor}"),
    ] {
        let (status, _, body) = get(&s, &uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}: {body}");
    }
}

#[tokio::test]
async fn coverage_matches_aggregate_baseline() {
    let s = state_with(None).await;
    let (_, _, agg) = get(
        &s,
        "/v1/aggregate?q=gold&from=1896-01-01&to=1896-12-31&bucket=month",
    )
    .await;
    let baseline_ref = agg["cube"]["baseline_ref"].as_str().unwrap().to_owned();
    assert!(baseline_ref.ends_with("&v=fixture-v1"));
    let (status, _, cov) = get(&s, &baseline_ref).await;
    assert_eq!(status, StatusCode::OK, "{cov}");
    let pages: u64 = cov["pages"]["h"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap())
        .sum();
    assert_eq!(Some(pages), agg["total"]["baseline_pages"].as_u64());
    // Baselines include empty-OCR pages, so they exceed the indexed page count.
    assert!(pages > 0);
}

#[tokio::test]
async fn front_filter_disables_inexact_baseline_and_api_alias_works() {
    let s = state_with(None).await;
    let (status, _, body) = get(&s, "/api/v1/aggregate?q=gold&front=true").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["series"]["baseline"].is_null());
    assert!(body["cube"]["baseline_ref"].is_null());
    // Links built on the /api/v1 mount stay on it.
    let (_, _, body) = get(&s, "/api/v1/aggregate?q=gold").await;
    let link = body["cube"]["baseline_ref"].as_str().unwrap().to_owned();
    assert!(link.starts_with("/api/v1/coverage?"), "{link}");
    let (status, _, _) = get(&s, &link).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn coarsens_before_querying_an_oversized_cube() {
    // 6 places × 366 days and × 53 weeks exceed 240 cells; 6 × 12 months fit.
    let s = state_with_cells(240).await;
    let (status, _, body) = get(
        &s,
        "/v1/aggregate?q=gold&from=1896-01-01&to=1896-12-31&bucket=day",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["bucket"]["unit"], "month");
    assert_eq!(body["coarsened"], true);
    let cube_sum: u64 = body["cube"]["h"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap())
        .sum();
    assert_eq!(cube_sum, body["total"]["hits"].as_u64().unwrap());
    // Even by year the cube would exceed the budget → 422, not a backend failure.
    let tiny = state_with_cells(5).await;
    let (status, _, body) = get(&tiny, "/v1/aggregate?q=gold&from=1896-01-01&to=1896-12-31").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["type"], "/errors/query-too-broad");
}

#[tokio::test]
async fn places_geojson_is_version_pinned() {
    let s = state_with(None).await;
    let (status, headers, _) = get(&s, "/v1/places?v=old").await;
    assert_eq!(status, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(
        header_str(&headers, header::LOCATION),
        "/v1/places?v=fixture-v1"
    );
    let (status, headers, _) = get(&s, "/v1/places?v=fixture-v1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        header_str(&headers, header::CACHE_CONTROL),
        "public, max-age=86400"
    );
    let (_, headers, body) = get(&s, "/v1/places").await;
    assert_eq!(
        header_str(&headers, header::CONTENT_LOCATION),
        "/v1/places?v=fixture-v1"
    );
    assert_eq!(body["type"], "FeatureCollection");
    assert_eq!(body["features"].as_array().unwrap().len(), 6);
    // Each place lists its titles' languages.
    let langs: Vec<(&str, &Value)> = body["features"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| (f["id"].as_str().unwrap(), &f["properties"]["languages"]))
        .collect();
    assert_eq!(
        langs,
        [
            ("P00001", &json!(["eng"])),
            ("P00002", &json!(["eng", "ger"])),
            ("P00003", &json!(["spa"])),
            ("P00004", &json!(["eng"])),
            ("P00005", &json!(["eng"])),
            ("P00006", &json!(["ger"])),
        ]
    );
    // And how many of its titles list each.
    assert_eq!(
        body["features"][1]["properties"]["language_titles"],
        json!({"eng": 1, "ger": 1})
    );
    assert_eq!(body["features"][1]["properties"]["titles"], 1);
    assert_eq!(body["features"][0]["geometry"]["type"], "Point");
}

/// Copy the fixture data dir to a fresh temp dir the test can modify.
fn temp_data_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("usnm-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for sub in ["indexes", "fixture-v1"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
        for entry in std::fs::read_dir(data_dir().join(sub)).unwrap() {
            let path = entry.unwrap().path();
            std::fs::copy(&path, dir.join(sub).join(path.file_name().unwrap())).unwrap();
        }
    }
    std::fs::copy(data_dir().join("current.json"), dir.join("current.json")).unwrap();
    dir
}

/// Copy a reference snapshot under a new version, as the stats job would publish it.
fn snapshot_copy(dir: &std::path::Path, from: &str, to: &str) {
    std::fs::create_dir_all(dir.join(to)).unwrap();
    for entry in std::fs::read_dir(dir.join(from)).unwrap() {
        let path = entry.unwrap().path();
        std::fs::copy(&path, dir.join(to).join(path.file_name().unwrap())).unwrap();
    }
    let manifest = dir.join(to).join("manifest.json");
    let mut m: Value = serde_json::from_str(&std::fs::read_to_string(&manifest).unwrap()).unwrap();
    m["index_version"] = to.into();
    std::fs::write(manifest, m.to_string()).unwrap();
}

#[tokio::test]
async fn hot_reload_swaps_reference_data_and_backend_together() {
    let dir = temp_data_dir("reload");
    let loader = Loader {
        reference: Arc::new(LocalStore::new(&dir)),
        engine: Engine::Memory {
            indexes_dir: dir.join("indexes"),
        },
        congress_search: true,
    };
    let snapshot = loader.snapshot().await.unwrap();
    let s = Arc::new(AppState::with_loader(config(), snapshot, Some(loader)));
    assert!(
        !reload_if_changed(&s).await.unwrap(),
        "unchanged version must not reload"
    );

    // Publish a new version whose index set adds a delta the old snapshot never loaded.
    std::fs::copy(
        dir.join("indexes/pages-delta-fixture-1.jsonl"),
        dir.join("indexes/pages-delta-fixture-2.jsonl"),
    )
    .unwrap();
    let mut current: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("current.json")).unwrap()).unwrap();
    current["index_version"] = "fixture-v2".into();
    current["indexes"] = serde_json::json!(["pages-base-fixture", "pages-delta-fixture-2"]);

    // A version whose reference snapshot is still v1's is refused.
    std::fs::write(dir.join("current.json"), current.to_string()).unwrap();
    let err = reload_if_changed(&s).await.unwrap_err();
    assert!(err.contains("is for `fixture-v1`"), "{err}");
    assert_eq!(s.snapshot.load().refdata.version(), "fixture-v1");

    // With its own snapshot, it publishes.
    snapshot_copy(&dir, "fixture-v1", "fixture-v2");
    current["reference"] = "fixture-v2".into();
    std::fs::write(dir.join("current.json"), current.to_string()).unwrap();

    assert!(reload_if_changed(&s).await.unwrap());
    let (_, _, meta) = get(&s, "/v1/meta").await;
    assert_eq!(meta["index_version"], "fixture-v2");
    let (status, _, body) = get(&s, "/v1/aggregate?q=fever").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["index_version"], "fixture-v2");
    assert!(body["total"]["hits"].as_u64().unwrap() > 0);
    let _ = std::fs::remove_dir_all(&dir);
}

fn persisted_files(dir: &std::path::Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir.join("fixture-v1/f7")) else {
        return Vec::new();
    };
    entries
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "zst"))
        .collect()
}

#[tokio::test]
async fn slow_responses_persist_and_survive_a_restart() {
    let dir = std::env::temp_dir().join(format!("usnm-respcache-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = Arc::new(LocalStore::new(&dir));
    let mut cfg = config();
    cfg.persist_after = Duration::ZERO;
    let first = Arc::new(
        AppState::new(cfg.clone(), Arc::new(fixture_backend()), refdata().await)
            .with_response_store(store.clone()),
    );
    let uri = "/v1/aggregate?q=fever&v=fixture-v1";
    let (status, _, body) = get(&first, uri).await;
    assert_eq!(status, StatusCode::OK);
    let mut files = Vec::new();
    for _ in 0..100 {
        files = persisted_files(&dir);
        if !files.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(files.len(), 1, "one entry under {{version}}/f7/");
    let name = files[0].file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        !name.contains("fever"),
        "search text must not appear in paths"
    );

    // A fresh replica with an empty in-process cache and an empty index still
    // answers from the persisted entry: the body is identical.
    let restarted = Arc::new(
        AppState::new(cfg, Arc::new(MemoryBackend::new()), refdata().await)
            .with_response_store(store),
    );
    let (status, headers, again) = get(&restarted, uri).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again, body);
    assert!(header_str(&headers, header::ETAG).starts_with("\"fixture-v1:"));

    // An entry that decompresses but isn't JSON is ignored and recomputed.
    std::fs::write(
        &files[0],
        zstd::encode_all(&b"{\"truncated"[..], 3).unwrap(),
    )
    .unwrap();
    let recomputing = Arc::new(
        AppState::new(config(), Arc::new(fixture_backend()), refdata().await)
            .with_response_store(Arc::new(LocalStore::new(&dir))),
    );
    let (status, _, mut fresh) = get(&recomputing, uri).await;
    assert_eq!(status, StatusCode::OK);
    // Recomputed, so only its timing may differ from the original.
    let mut original = body.clone();
    for v in [&mut fresh, &mut original] {
        v.as_object_mut().unwrap().remove("timing_ms");
    }
    // Serving the invalid entry would have parsed as `null` here.
    assert_eq!(fresh, original);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn fast_responses_are_not_persisted() {
    let dir = std::env::temp_dir().join(format!("usnm-respfast-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut cfg = config();
    cfg.persist_after = Duration::from_secs(3600);
    let s = Arc::new(
        AppState::new(cfg, Arc::new(fixture_backend()), refdata().await)
            .with_response_store(Arc::new(LocalStore::new(&dir))),
    );
    let (status, _, _) = get(&s, "/v1/aggregate?q=fever&v=fixture-v1").await;
    assert_eq!(status, StatusCode::OK);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(persisted_files(&dir).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

async fn get_from(
    state: &Arc<AppState>,
    uri: &str,
    xff: &str,
) -> (StatusCode, axum::http::HeaderMap) {
    let resp = app(state.clone())
        .oneshot(
            Request::builder()
                .uri(uri)
                .header("x-forwarded-for", xff)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    (resp.status(), resp.headers().clone())
}

#[tokio::test]
async fn clients_over_their_rate_get_429() {
    let mut cfg = config();
    cfg.rate_limit = Some(usnm_api::config::RateLimit {
        per_minute: 1.try_into().unwrap(),
        burst: 2.try_into().unwrap(),
    });
    let s = Arc::new(AppState::new(
        cfg,
        Arc::new(fixture_backend()),
        refdata().await,
    ));
    for _ in 0..2 {
        assert_eq!(
            get_from(&s, "/v1/meta", "203.0.113.7").await.0,
            StatusCode::OK
        );
    }
    let (status, headers) = get_from(&s, "/api/v1/meta", "198.51.100.1, 203.0.113.7").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        header_str(&headers, header::CONTENT_TYPE),
        "application/problem+json"
    );
    assert_eq!(header_str(&headers, header::CACHE_CONTROL), "no-store");
    let retry: u64 = header_str(&headers, header::RETRY_AFTER).parse().unwrap();
    assert!((1..=60).contains(&retry));
    // Other clients and health probes are unaffected.
    assert_eq!(
        get_from(&s, "/v1/meta", "203.0.113.8").await.0,
        StatusCode::OK
    );
    assert_eq!(
        get_from(&s, "/healthz", "203.0.113.7").await.0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn reference_files_must_match_the_manifest() {
    let dir = temp_data_dir("manifest");
    let store = LocalStore::new(&dir);
    assert!(RefData::load(&store).await.is_ok());

    let places = dir.join("fixture-v1/places.json");
    let mut text = std::fs::read_to_string(&places).unwrap();
    text = text.replacen("Fixture", "Fixturf", 1);
    std::fs::write(&places, text).unwrap();
    let err = RefData::load(&store).await.unwrap_err();
    assert!(err.contains("manifest"), "{err}");

    // Ids from current.json become object paths, so they must be plain segments.
    let mut current: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("current.json")).unwrap()).unwrap();
    current["reference"] = "../fixture-v1".into();
    std::fs::write(dir.join("current.json"), current.to_string()).unwrap();
    let err = RefData::load(&store).await.unwrap_err();
    assert!(err.contains("not a valid id"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A built site: an index, a hashed asset with a precompressed copy, a favicon.
/// Each test names its own, since tests run in parallel.
fn site_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("usnm-site-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("assets")).unwrap();
    std::fs::write(dir.join("index.html"), "<!doctype html><div id=root></div>").unwrap();
    std::fs::write(dir.join("assets/index-abc.js"), "console.log(1)").unwrap();
    std::fs::write(dir.join("assets/index-abc.js.gz"), b"gzipped").unwrap();
    std::fs::write(dir.join("favicon.svg"), "<svg/>").unwrap();
    dir
}

fn site_dir_of(s: &AppState) -> PathBuf {
    s.config.site_dir.clone().unwrap()
}

async fn get_site(
    state: &Arc<AppState>,
    uri: &str,
    encoding: Option<&str>,
) -> (StatusCode, axum::http::HeaderMap, String) {
    let mut req = Request::builder().uri(uri);
    if let Some(e) = encoding {
        req = req.header(header::ACCEPT_ENCODING, e);
    }
    let resp = app(state.clone())
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let (status, headers) = (resp.status(), resp.headers().clone());
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

#[tokio::test]
async fn serves_the_site_alongside_the_api() {
    let mut cfg = config();
    cfg.site_dir = Some(site_dir("serve"));
    let s = Arc::new(AppState::new(
        cfg,
        Arc::new(fixture_backend()),
        refdata().await,
    ));

    // The app shell, revalidated on every load, with the security headers.
    let (status, h, body) = get_site(&s, "/", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("id=root"));
    assert_eq!(header_str(&h, header::CACHE_CONTROL), "no-cache");
    assert!(header_str(&h, header::CONTENT_SECURITY_POLICY).contains("connect-src 'self'"));
    assert_eq!(header_str(&h, header::X_CONTENT_TYPE_OPTIONS), "nosniff");

    // The app's pages get the shell too, so a reload keeps working. The file
    // itself is served with or without a trailing slash (web/src/route.ts).
    for uri in ["/datasets", "/datasets/", "/index.html", "/index.html/"] {
        let (status, h, body) = get_site(&s, uri, None).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert!(body.contains("id=root"), "{uri}");
        assert!(header_str(&h, header::CONTENT_TYPE).starts_with("text/html"));
        assert_eq!(header_str(&h, header::CACHE_CONTROL), "no-cache");
    }

    // The dataset details page's old path redirects, keeping the query (#294).
    for (uri, location) in [
        ("/status", "/datasets"),
        ("/status/", "/datasets"),
        ("/status?x=1", "/datasets?x=1"),
    ] {
        let (status, h, _) = get_site(&s, uri, None).await;
        assert_eq!(status, StatusCode::MOVED_PERMANENTLY, "{uri}");
        assert_eq!(header_str(&h, header::LOCATION), location, "{uri}");
    }

    // Other paths get the shell, where the app shows a not-found page, with a
    // 404; so do paths with a segment too long to be a file name, which the
    // filesystem would refuse (a 500 from the file service).
    let long = format!("/{}", "a".repeat(300));
    let long_nested = format!("/assets/{}/x", "b".repeat(256));
    for uri in [
        "/search/gold",
        "/does-not-exist",
        "/statuses",
        long.as_str(),
    ] {
        let (status, h, body) = get_site(&s, uri, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
        assert!(body.contains("id=root"), "{uri}");
        assert!(header_str(&h, header::CONTENT_TYPE).starts_with("text/html"));
        assert_eq!(header_str(&h, header::CACHE_CONTROL), "no-cache", "{uri}");
    }
    let (status, _, _) = get_site(&s, &long_nested, None).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a long name under /assets/ is a plain 404"
    );
    // The limit is on the decoded name, as the file service decodes it: a
    // 100-byte file name sent as 300 bytes of escapes is still served.
    let name = format!("{}.txt", "d".repeat(96));
    std::fs::write(site_dir_of(&s).join(&name), "found").unwrap();
    let escaped: String = name.bytes().map(|b| format!("%{b:02X}")).collect();
    assert!(escaped.len() > 255);
    let (status, _, body) = get_site(&s, &format!("/{escaped}"), None).await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, "found"));
    // 253 bytes fits NAME_MAX, but `ServeDir` would also probe the 256-byte
    // `.gz` name for a client that accepts gzip.
    let (status, _, _) = get_site(&s, &format!("/{}.js", "e".repeat(250)), Some("gzip")).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a name that can't take .gz is a plain 404"
    );
    let (status, _, _) = get_site(&s, &format!("/{}.js", "c".repeat(400)), None).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a long file name is a plain 404"
    );

    // HEAD on an app route: GET's headers and status, no body.
    let resp = app(s.clone())
        .oneshot(
            Request::builder()
                .method("HEAD")
                .uri("/datasets")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let len = std::fs::metadata(s.config.site_dir.as_ref().unwrap().join("index.html"))
        .unwrap()
        .len();
    assert_eq!(
        header_str(resp.headers(), header::CONTENT_LENGTH),
        len.to_string()
    );
    assert!(header_str(resp.headers(), header::CONTENT_TYPE).starts_with("text/html"));
    assert!(resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .is_empty());

    // Hashed assets: cached for a year, the precompressed copy when accepted.
    let (status, h, body) = get_site(&s, "/assets/index-abc.js", Some("gzip")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "gzipped");
    assert_eq!(header_str(&h, header::CONTENT_ENCODING), "gzip");
    assert_eq!(
        header_str(&h, header::CACHE_CONTROL),
        "public, max-age=31536000, immutable"
    );
    let (_, h, body) = get_site(&s, "/assets/index-abc.js", None).await;
    assert_eq!(body, "console.log(1)");
    assert_eq!(header_str(&h, header::CONTENT_ENCODING), "");

    let (status, h, _) = get_site(&s, "/favicon.svg", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        header_str(&h, header::CACHE_CONTROL),
        "public, max-age=3600"
    );

    // A file path with a trailing slash is a missing file, not an app route.
    let (status, h, body) = get_site(&s, "/favicon.svg/", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(!body.contains("id=root"));
    assert!(!header_str(&h, header::CONTENT_TYPE).starts_with("text/html"));
    for uri in ["/favicon.svg/", "/assets/missing.js"] {
        let resp = app(s.clone())
            .oneshot(
                Request::builder()
                    .method("HEAD")
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{uri}");
        assert!(
            resp.into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .is_empty(),
            "{uri}"
        );
    }

    let resp = app(s.clone())
        .oneshot(
            Request::builder()
                .method("HEAD")
                .uri("/does-not-exist")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert!(header_str(resp.headers(), header::CONTENT_TYPE).starts_with("text/html"));
    assert_eq!(
        header_str(resp.headers(), header::CACHE_CONTROL),
        "no-cache"
    );

    // Missing files are 404s, not the shell.
    for uri in ["/assets/gone-123.js", "/gone.txt"] {
        let (status, h, _) = get_site(&s, uri, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
        assert_eq!(header_str(&h, header::CACHE_CONTROL), "", "{uri}");
    }

    // The API is unchanged, and its unknown paths stay problem details.
    let (status, _, body) = get(&s, "/v1/meta").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["index_version"].is_string());
    let (status, h, _) = get(&s, "/v1/nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        header_str(&h, header::CONTENT_TYPE),
        "application/problem+json"
    );
    let (status, _, _) = get(&s, "/healthz").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn search_permalinks_and_dataset_details_are_noindex() {
    let mut cfg = config();
    cfg.site_dir = Some(site_dir("noindex"));
    let s = Arc::new(AppState::new(
        cfg,
        Arc::new(fixture_backend()),
        refdata().await,
    ));
    let robots = header::HeaderName::from_static("x-robots-tag");
    for uri in [
        "/?q=x",
        "/?q=%22cross+of+gold%22&from=1896-06-01",
        "/datasets",
    ] {
        let (status, h, _) = get_site(&s, uri, None).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(header_str(&h, robots.clone()), "noindex", "{uri}");
    }
    // The old path's redirect too.
    let (status, h, _) = get_site(&s, "/status", None).await;
    assert_eq!(status, StatusCode::MOVED_PERMANENTLY);
    assert_eq!(header_str(&h, robots.clone()), "noindex");
    for uri in ["/", "/?t=1896-07-01", "/favicon.svg"] {
        let (status, h, _) = get_site(&s, uri, None).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(header_str(&h, robots.clone()), "", "{uri}");
    }
}

async fn get_host(state: &Arc<AppState>, host: &str, uri: &str) -> axum::response::Response {
    app(state.clone())
        .oneshot(
            Request::builder()
                .uri(uri)
                .header(header::HOST, host)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn www_redirects_to_the_apex() {
    let mut cfg = config();
    cfg.site_dir = Some(site_dir("www"));
    let s = Arc::new(AppState::new(
        cfg,
        Arc::new(fixture_backend()),
        refdata().await,
    ));
    let cases = [
        ("www.usnewsmap.com", "/", "https://usnewsmap.com/"),
        (
            "WWW.usnewsmap.com:443",
            "/?q=%22cross+of+gold%22&from=1896-06-01",
            "https://usnewsmap.com/?q=%22cross+of+gold%22&from=1896-06-01",
        ),
        (
            "www.usnewsmap.com",
            "/status",
            "https://usnewsmap.com/status",
        ),
        (
            "www.usnewsmap.com",
            "/v1/meta",
            "https://usnewsmap.com/v1/meta",
        ),
    ];
    for (host, uri, location) in cases {
        let resp = get_host(&s, host, uri).await;
        assert_eq!(resp.status(), StatusCode::MOVED_PERMANENTLY, "{host}{uri}");
        assert_eq!(header_str(resp.headers(), header::LOCATION), location);
        assert_eq!(
            header_str(resp.headers(), header::X_CONTENT_TYPE_OPTIONS),
            "nosniff"
        );
        assert!(header_str(resp.headers(), header::CONTENT_SECURITY_POLICY).contains("default-src"));
    }
    // HTTP/2 puts the host in the URI.
    let resp = get_site(&s, "https://www.usnewsmap.com/status?x=1", None).await;
    assert_eq!(resp.0, StatusCode::MOVED_PERMANENTLY);
    assert_eq!(
        header_str(&resp.1, header::LOCATION),
        "https://usnewsmap.com/status?x=1"
    );
    // Other hosts are served as they are.
    for host in [
        "usnewsmap.com",
        "api.usnewsmap.com",
        "www.example.com",
        "127.0.0.1:8080",
    ] {
        let resp = get_host(&s, host, "/").await;
        assert_eq!(resp.status(), StatusCode::OK, "{host}");
    }
}

#[tokio::test]
async fn without_a_site_only_the_api_answers() {
    let s = state_with(None).await;
    let (status, _, _) = get_site(&s, "/", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = get_site(&s, "/v1/meta", None).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn status_without_pipeline_state() {
    let state = state_with(None).await;
    for uri in ["/v1/status", "/api/v1/status"] {
        let (status, headers, body) = get(&state, uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(
            header_str(&headers, header::CACHE_CONTROL),
            "public, max-age=30"
        );
        assert_eq!(
            header_str(&headers, header::CONTENT_TYPE),
            "application/json"
        );
        assert_eq!(body["schema"], 1);
        assert_eq!(body["published"]["index_version"], "fixture-v1");
        assert_eq!(body["published"]["indexes"].as_array().unwrap().len(), 2);
        assert_eq!(body["backfill"]["available"], false);
        assert_eq!(body["indexing"]["available"], false);
    }
}

#[tokio::test]
async fn versions_without_pipeline_state() {
    let state = state_with(None).await;
    for uri in ["/v1/versions", "/api/v1/versions"] {
        let (status, headers, body) = get(&state, uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(
            header_str(&headers, header::CACHE_CONTROL),
            "public, max-age=30"
        );
        assert_eq!(body["schema"], 1);
        assert_eq!(body["available"], false);
        assert_eq!(body["serving"], "fixture-v1");
        assert_eq!(body["versions"], json!([]));
    }
}

#[tokio::test]
async fn status_has_pages_by_state_and_language() {
    let state = state_with(None).await;
    let (_, _, body) = get(&state, "/v1/status").await;
    let p = &body["published"];
    // Six fixture places, one per state, 312 pages each: ties go by name.
    let states = p["by_state"].as_array().unwrap();
    assert_eq!(states.len(), 6);
    assert_eq!(
        states[0],
        json!({"state": "CA", "name": "California", "places": 1, "titles": 1,
               "pages": 312, "percent": 16.7})
    );
    let names: Vec<&str> = states.iter().map(|r| r["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        [
            "California",
            "Georgia",
            "Illinois",
            "Nebraska",
            "New York",
            "South Carolina"
        ]
    );
    let total: u64 = states.iter().map(|r| r["pages"].as_u64().unwrap()).sum();
    assert_eq!(total, p["pages"].as_u64().unwrap());
    assert_eq!(
        p["by_language"],
        json!({"pages_known": true, "multilingual_titles": 1, "multilingual_pages": 312,
               "rows": [{"code": "eng", "name": "English", "titles": 4, "pages": 1248,
                         "percent": 66.7},
                        {"code": "ger", "name": "German", "titles": 2, "pages": 624,
                         "percent": 33.3},
                        {"code": "spa", "name": "Spanish", "titles": 1, "pages": 312,
                         "percent": 16.7}]})
    );
    // Pages by year cover the version's dates and add up to its pages.
    let years = p["by_year"].as_array().unwrap();
    let year = |r: &Value| r["year"].as_i64().unwrap();
    assert_eq!(
        years.iter().map(year).collect::<Vec<_>>(),
        [1895, 1896, 1897]
    );
    let total: u64 = years.iter().map(|r| r["pages"].as_u64().unwrap()).sum();
    assert_eq!(total, p["pages"].as_u64().unwrap());
    // The fixtures search LoC's text alone.
    assert_eq!(
        p["sources"],
        json!({"american_stories": null, "ja_latin": null, "ocr_local": null})
    );
}

#[tokio::test]
async fn a_snapshot_without_pages_per_title_still_loads() {
    let dir = temp_data_dir("no-title-pages");
    let store = LocalStore::new(&dir);
    // A snapshot from before releases wrote the file: not in the manifest.
    std::fs::remove_file(dir.join("fixture-v1/title_pages.json")).unwrap();
    let manifest = dir.join("fixture-v1/manifest.json");
    let mut m: Value = serde_json::from_str(&std::fs::read_to_string(&manifest).unwrap()).unwrap();
    m["files"]
        .as_array_mut()
        .unwrap()
        .retain(|f| f["path"] != "title_pages.json");
    std::fs::write(&manifest, m.to_string()).unwrap();
    let rd = RefData::load(&store).await.unwrap();
    assert!(rd.title_pages.is_none());
    let lang = usnm_api::status::assemble::by_language(&rd);
    assert!(!lang.pages_known);
    assert_eq!((lang.rows[0].titles, lang.rows[0].pages), (4, None));
    assert_eq!(usnm_api::status::assemble::by_state(&rd).len(), 6);
    let _ = std::fs::remove_dir_all(&dir);

    // Listed in the manifest but altered: the version doesn't load.
    let dir = temp_data_dir("bad-title-pages");
    let store = LocalStore::new(&dir);
    let path = dir.join("fixture-v1/title_pages.json");
    let text = std::fs::read_to_string(&path)
        .unwrap()
        .replacen("312", "313", 1);
    std::fs::write(&path, text).unwrap();
    let err = RefData::load(&store).await.unwrap_err();
    assert!(err.contains("title_pages.json does not match"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A snapshot from before releases recorded each title's page dates still
/// loads, and its titles list them as `null`; one whose file doesn't match
/// its manifest doesn't load.
#[tokio::test]
async fn a_snapshot_without_title_dates_still_loads() {
    let dir = temp_data_dir("no-title-dates");
    let store = LocalStore::new(&dir);
    std::fs::remove_file(dir.join("fixture-v1/title_dates.json")).unwrap();
    let manifest = dir.join("fixture-v1/manifest.json");
    let mut m: Value = serde_json::from_str(&std::fs::read_to_string(&manifest).unwrap()).unwrap();
    m["files"]
        .as_array_mut()
        .unwrap()
        .retain(|f| f["path"] != "title_dates.json");
    std::fs::write(&manifest, m.to_string()).unwrap();
    let rd = RefData::load(&store).await.unwrap();
    assert!(rd.title_dates.is_none());
    assert!(rd.title_pages.is_some());
    let s = Arc::new(AppState::new(config(), Arc::new(fixture_backend()), rd));
    let (status, _, body) = get(&s, "/v1/titles").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let titles = body["titles"].as_array().unwrap();
    assert_eq!(titles.len(), 6);
    assert!(titles
        .iter()
        .all(|t| t["first_page_date"].is_null() && t["last_page_date"].is_null()));
    assert!(titles.iter().all(|t| t["pages"].is_u64()));
    let (_, _, one) = get(&s, "/v1/titles/sn99000001").await;
    assert!(one["first_page_date"].is_null() && one["last_page_date"].is_null());
    let _ = std::fs::remove_dir_all(&dir);

    // Listed in the manifest but altered: the version doesn't load.
    let dir = temp_data_dir("bad-title-dates");
    let store = LocalStore::new(&dir);
    let path = dir.join("fixture-v1/title_dates.json");
    let text = std::fs::read_to_string(&path)
        .unwrap()
        .replacen("1895-01-05", "1895-01-06", 1);
    std::fs::write(&path, text).unwrap();
    let err = RefData::load(&store).await.unwrap_err();
    assert!(err.contains("title_dates.json does not match"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn api_responses_are_compressed_and_cors_is_limited_to_allowed_origins() {
    let mut cfg = config();
    cfg.allowed_origins = vec!["https://usnewsmap.com".to_owned()];
    let s = Arc::new(AppState::new(
        cfg,
        Arc::new(fixture_backend()),
        refdata().await,
    ));
    let send = |origin: &'static str, encoding: Option<&'static str>| {
        let s = s.clone();
        async move {
            let mut req = Request::builder()
                .uri("/v1/meta")
                .header(header::ORIGIN, origin);
            if let Some(e) = encoding {
                req = req.header(header::ACCEPT_ENCODING, e);
            }
            app(s)
                .oneshot(req.body(Body::empty()).unwrap())
                .await
                .unwrap()
        }
    };
    let vary = |h: &axum::http::HeaderMap| {
        h.get_all(header::VARY)
            .iter()
            .map(|v| v.to_str().unwrap().to_ascii_lowercase())
            .collect::<Vec<_>>()
            .join(", ")
    };

    // Gzip when accepted, with the CORS header for an allowed origin.
    let resp = send("https://usnewsmap.com", Some("gzip")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let h = resp.headers().clone();
    assert_eq!(header_str(&h, header::CONTENT_ENCODING), "gzip");
    assert_eq!(
        header_str(&h, header::ACCESS_CONTROL_ALLOW_ORIGIN),
        "https://usnewsmap.com"
    );
    assert!(vary(&h).contains("accept-encoding"), "{}", vary(&h));
    assert!(vary(&h).contains("origin"), "{}", vary(&h));
    let gzipped = resp.into_body().collect().await.unwrap().to_bytes();
    let mut gz = flate2::read::GzDecoder::new(&gzipped[..]);
    let mut json = String::new();
    std::io::Read::read_to_string(&mut gz, &mut json).unwrap();
    let meta: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(meta["index_version"], "fixture-v1");

    // Identity when compression isn't asked for, or is refused in favour of identity.
    for encoding in [None, Some("identity"), Some("gzip;q=0")] {
        let resp = send("https://usnewsmap.com", encoding).await;
        assert_eq!(resp.status(), StatusCode::OK, "{encoding:?}");
        assert_eq!(
            header_str(resp.headers(), header::CONTENT_ENCODING),
            "",
            "{encoding:?}"
        );
    }

    // Another origin gets the response but no CORS grant.
    let resp = send("https://elsewhere.example", None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        header_str(resp.headers(), header::ACCESS_CONTROL_ALLOW_ORIGIN),
        ""
    );
}

/// The fixture state with the Japanese pages' index (#139) published, and its
/// synthetic title in the catalog.
async fn ja_state() -> Arc<AppState> {
    ja_state_with(config(), false).await
}

/// [`ja_state`] with `cfg`, on a version built with American Stories' text
/// or not.
async fn ja_state_with(cfg: Config, american_stories: bool) -> Arc<AppState> {
    let mut backend = fixture_backend();
    backend.add_index("pages-ja-fixture", load_docs("pages-ja-fixture"));
    let mut refdata = refdata().await;
    refdata.current.ja = Some(usnm_api::refdata::JaIndexes {
        indexes: vec!["pages-ja-fixture".into()],
        fold: usnm_core::ja::fold_version(FIXTURE),
        pages: 52,
    });
    if american_stories {
        refdata.current.american_stories = Some(usnm_core::american_stories::VERSION);
    }
    refdata.titles.insert(
        "sn99000901".into(),
        serde_json::from_value(json!({
            "lccn": "sn99000901", "name": "Fixture Shimpo (Japanese)",
            "place_id": "P00003", "state": "CA", "languages": ["eng", "jpn"],
        }))
        .unwrap(),
    );
    Arc::new(AppState::new(cfg, Arc::new(backend), refdata))
}

#[tokio::test]
async fn japanese_queries_search_the_japanese_pages() {
    let s = ja_state().await;
    let docs = load_docs("pages-ja-fixture");
    let printed_has = |w: &str| {
        let q = vec![usnm_core::ja::tokenize(w, FIXTURE)];
        docs.iter()
            .filter(|d| !usnm_core::ja::find(d.printed.as_deref().unwrap(), &q, FIXTURE).is_empty())
            .count() as u64
    };
    let war = printed_has("戰爭");
    assert!(war > 0);
    // Modern and printed forms are one search, on the Japanese pages only.
    for q in ["%E6%88%A6%E4%BA%89", "%E6%88%B0%E7%88%AD"] {
        let (status, _, body) = get(&s, &format!("/v1/aggregate?q={q}")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["total"]["hits"], war, "{body}");
        assert_eq!(body["places"]["id"], json!(["P00003"]));
        // The relative rate compares with pages of titles that list Japanese.
        let r = body["cube"]["baseline_ref"].as_str().unwrap();
        assert!(r.contains("lang=jpn"), "{r}");
        assert_eq!(
            body["baseline"],
            json!({"languages": ["jpn"], "why": "query_language"})
        );
    }
    // Hits: our OCR is marked, snippets show the printed form, and the LoC
    // viewer link has no highlight (LoC has no text for these pages).
    let (status, _, body) = get(&s, "/v1/meta").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ja"]["indexes"], json!(["pages-ja-fixture"]));
    assert_eq!(body["ja"]["pages"], 52);
    let (status, _, body) = get(&s, "/v1/hits?q=%E6%88%A6%E4%BA%89&place=P00003").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body["items"].as_array().unwrap();
    assert_eq!(body["total"], war);
    assert_eq!(
        items.len() as u64,
        war.min(50),
        "a first page of the matches"
    );
    for it in items {
        assert_eq!(it["ocr"]["source"], "usnm-ndlocr-lite");
        assert_eq!(it["ocr"]["engine"], "ndlocr-lite 636d1cf");
        assert_eq!(it["title"], "Fixture Shimpo (Japanese)");
        assert!(it["snippets"][0]
            .as_str()
            .unwrap()
            .contains("<mark>戰爭</mark>"));
        assert!(!it["links"]["viewer"].as_str().unwrap().contains("&q="));
    }
    // A Latin query is untouched: the main indexes, LoC's text, highlighted viewer links.
    let (_, _, body) = get(&s, "/v1/hits?q=gold&place=P00001").await;
    let it = &body["items"][0];
    assert!(it.get("ocr").is_none());
    assert!(it["links"]["viewer"].as_str().unwrap().contains("&q=gold"));
}

#[tokio::test]
async fn a_japanese_query_on_a_version_without_japanese_pages_is_refused() {
    let s = state_with(None).await;
    let (status, _, body) = get(&s, "/v1/aggregate?q=%E6%97%A5%E6%9C%AC").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (status, _, _) = get(&s, "/v1/hits?q=%E6%97%A5%E6%9C%AC&place=P00003").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    // Latin searches work as before.
    let (status, _, _) = get(&s, "/v1/aggregate?q=gold").await;
    assert_eq!(status, StatusCode::OK);
}

/// `current.json`'s Japanese index ids are checked like the main ones (#139).
#[tokio::test]
async fn current_json_japanese_index_ids_are_validated() {
    use usnm_store::ObjectStore as _;
    let dir = tempfile::tempdir().unwrap();
    let store = LocalStore::new(dir.path());
    let base = json!({
        "index_version": "v1", "indexes": ["pages-base-1"], "reference": "v1",
        "bounds": {"from": "1895-01-01", "to": "1897-12-31"}, "published_at": "2026-10-05T00:00:00Z",
    });
    let put = |v: Value| {
        let store = &store;
        async move {
            store
                .put(
                    "current.json",
                    serde_json::to_vec(&v).unwrap(),
                    "application/json",
                )
                .await
                .unwrap();
        }
    };
    let mut ok = base.clone();
    ok["ja"] = json!({"indexes": ["pages-ja-1"], "fold": 1, "pages": 3});
    put(ok).await;
    assert!(usnm_api::refdata::read_current(&store).await.is_ok());
    for bad in [json!(["../escape"]), json!(["a/b"]), json!([])] {
        let mut v = base.clone();
        v["ja"] = json!({"indexes": bad, "fold": 1});
        put(v).await;
        assert!(
            usnm_api::refdata::read_current(&store).await.is_err(),
            "{bad}"
        );
    }
}

// ---- Congress pages (#229, phase 0) ----

const CONGRESS: &str = "congress-fixture";

/// The fixture version with the Congress fixture index named in its pointer.
async fn congress_state_with(cfg: Config) -> Arc<AppState> {
    let mut backend = fixture_backend();
    backend.add_index(CONGRESS, load_docs(CONGRESS));
    Arc::new(AppState::new(
        cfg,
        Arc::new(backend),
        congress_refdata().await,
    ))
}

/// The fixture version's reference data, its pointer naming the Congress
/// fixture index and its series.
async fn congress_refdata() -> RefData {
    let mut refdata = refdata().await;
    let d = |s: &str| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
    let series = |name: &str, pages: u64, from: &str, to: &str| usnm_api::refdata::CongressSeries {
        series: name.into(),
        pages,
        from: d(from),
        to: d(to),
    };
    refdata.current.congress = Some(usnm_api::refdata::CongressIndexes {
        indexes: vec![CONGRESS.into()],
        common_grams: refdata.current.common_grams,
        series: vec![
            series("annals", 20, "1820-02-01", "1820-03-29"),
            series("register", 21, "1832-12-10", "1833-02-27"),
            series("globe", 78, "1850-01-07", "1850-09-30"),
            series("serialset", 72, "1896-02-03", "1897-11-29"),
        ],
    });
    refdata
}

async fn congress_state() -> Arc<AppState> {
    congress_state_with(config()).await
}

/// Congress fixture pages whose text holds `phrase`, by brute force.
fn congress_oracle(phrase: &str, from: &str, to: &str, series: Option<&str>) -> u64 {
    let needle = format!(" {phrase} ");
    load_docs(CONGRESS)
        .iter()
        .filter(|d| {
            let date = usnm_core::time::date_from_day(d.day).to_string();
            date.as_str() >= from
                && date.as_str() <= to
                && series.is_none_or(|s| d.congress.series.as_deref() == Some(s))
                && format!(" {} ", d.text).contains(&needle)
        })
        .count() as u64
}

#[tokio::test]
async fn congress_searches_count_congress_pages_at_their_own_place() {
    let s = congress_state().await;
    // 1850 is before the fixture newspapers' first year (1895): a Congress
    // search is clamped to the Congress series' dates, not the newspapers'.
    let (st, _, body) = get(
        &s,
        "/v1/aggregate?q=%22fugitive+slave%22&from=1850-01-01&to=1850-12-31&bucket=month&source=congress",
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let want = congress_oracle("fugitive slave", "1850-01-01", "1850-12-31", None);
    assert!(want > 0);
    assert_eq!(body["total"]["hits"], want);
    assert_eq!(body["places"]["id"], json!(["PC001"]));
    assert_eq!(body["bucket"]["from"], "1850-01-01");
    // No Congress baselines yet, and no newspapers.
    assert!(body["series"]["baseline"].is_null());
    assert!(body["baseline"].is_null());
    assert_eq!(body["total"]["papers"], 0);
    assert_eq!(body["papers"]["lccn"], json!([]));
    assert!(body["query"]["canonical"]
        .as_str()
        .unwrap()
        .ends_with("&source=congress&to=1850-12-31"));
}

#[tokio::test]
async fn the_series_filter_counts_one_series() {
    let s = congress_state().await;
    let q = "/v1/aggregate?q=gold&mode=phrase&from=1789-01-01&to=1994-12-31&bucket=year&source=congress";
    let (st, _, all) = get(&s, q).await;
    assert_eq!(st, StatusCode::OK, "{all}");
    let (_, _, serial) = get(&s, &format!("{q}&series=serialset")).await;
    let (_, _, globe) = get(&s, &format!("{q}&series=globe")).await;
    let want = |series| congress_oracle("gold", "1789-01-01", "1994-12-31", series);
    assert_eq!(all["total"]["hits"], want(None));
    assert_eq!(serial["total"]["hits"], want(Some("serialset")));
    assert_eq!(globe["total"]["hits"], want(Some("globe")));
    assert!(want(Some("serialset")) > 0);
}

#[tokio::test]
async fn newspaper_searches_never_see_congress_pages() {
    let s = congress_state().await;
    let (st, _, body) = get(
        &s,
        "/v1/aggregate?q=%22free+silver%22&from=1896-01-01&to=1896-12-31&bucket=month",
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(
        body["total"]["hits"],
        oracle_count(
            "\"free silver\"",
            "1896-01-01",
            "1896-12-31",
            &["pages-base-fixture", "pages-delta-fixture-1"]
        )
    );
    assert!(!body["places"]["id"]
        .as_array()
        .unwrap()
        .contains(&json!("PC001")));
    assert!(!body["query"]["canonical"]
        .as_str()
        .unwrap()
        .contains("source"));
    // `source=news` is the same search, under the same cache key.
    let (_, _, explicit) = get(
        &s,
        "/v1/aggregate?q=%22free+silver%22&from=1896-01-01&to=1896-12-31&bucket=month&source=news",
    )
    .await;
    assert_eq!(explicit["query"]["canonical"], body["query"]["canonical"]);
}

#[tokio::test]
async fn congress_hits_carry_their_series_citation_and_link() {
    let s = congress_state().await;
    let (st, _, body) = get(
        &s,
        "/v1/hits?q=%22fugitive+slave%22&from=1850-01-01&to=1850-12-31&source=congress&place=PC001&limit=5",
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    // The Congress place isn't in the newspapers' catalog, but is echoed.
    assert_eq!(
        body["place"],
        json!({"id": "PC001", "name": "Congress", "state": "DC"})
    );
    let items = body["items"].as_array().unwrap();
    assert!(!items.is_empty());
    for it in items {
        assert_eq!(it["place_id"], "PC001");
        assert_eq!(it["front_page"], false);
        assert_eq!(it["congress"]["series"], "globe");
        assert_eq!(it["congress"]["congress"], 31);
        assert!(it["title"].as_str().unwrap().starts_with("Fixture Globe"));
        assert!(it["links"]["viewer"]
            .as_str()
            .unwrap()
            .starts_with("https://example.org/fixture/congress/globe/"));
        assert!(it["snippets"][0].as_str().unwrap().contains("<mark>"));
        // Who made the text goes with the citation, not in `ocr`, which
        // the site shows as our Japanese OCR.
        assert!(it.get("ocr").is_none(), "{it}");
        assert_eq!(it["congress"]["text_source"], "fixture");
        assert!(it["congress"].get("text_engine").is_none(), "{it}");
    }
    // A newspaper search still refuses the Congress place.
    let (st, _, _) = get(&s, "/v1/hits?q=gold&place=PC001").await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn congress_searches_answer_422_where_they_cannot_run() {
    let unsupported = |st: StatusCode, body: &Value| {
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(body["type"], "/errors/unsupported");
    };
    let s = congress_state().await;
    // Both at once needs two result blocks, which come in phase 1.
    let (st, _, body) = get(&s, "/v1/aggregate?q=gold&source=all").await;
    unsupported(st, &body);
    // Congress pages have no Japanese.
    let (st, _, body) = get(&s, "/v1/aggregate?q=%E6%97%A5%E6%9C%AC&source=congress").await;
    unsupported(st, &body);
    // A version without a Congress set.
    let plain = state_with(None).await;
    let (st, _, body) = get(&plain, "/v1/aggregate?q=gold&source=congress").await;
    unsupported(st, &body);
    // Switched off by the setting.
    let mut cfg = config();
    cfg.congress_search = false;
    let off = congress_state_with(cfg).await;
    let (st, _, body) = get(&off, "/v1/aggregate?q=gold&source=congress").await;
    unsupported(st, &body);
    let (_, _, meta) = get(&off, "/v1/meta").await;
    assert!(meta["congress"].is_null());
    // Newspaper-only filters are refused under Congress.
    let (st, _, _) = get(&s, "/v1/aggregate?q=gold&source=congress&front=true").await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

/// A stale `v` is redirected before a Congress search is refused, so a page
/// pinned to a version with Congress pages learns that the version changed
/// even when the new one can't search them.
#[tokio::test]
async fn a_stale_version_is_redirected_before_congress_is_refused() {
    let plain = state_with(None).await;
    let q = "q=gold&from=1895-01-01&to=1897-12-31&source=congress";
    for path in [
        format!("/v1/aggregate?{q}"),
        format!("/v1/hits?{q}&place=PC001"),
        format!("/v1/days?{q}&place=PC001"),
    ] {
        let (st, headers, body) = get(&plain, &format!("{path}&v=old-version")).await;
        assert_eq!(st, StatusCode::TEMPORARY_REDIRECT, "{path}: {body}");
        let location = header_str(&headers, header::LOCATION);
        assert!(location.contains("&source=congress"), "{location}");
        assert!(location.ends_with("&v=fixture-v1"), "{location}");
        // The serving version refuses it, pinned to it or not.
        for uri in [path.clone(), format!("{path}&v=fixture-v1")] {
            let (st, _, body) = get(&plain, &uri).await;
            assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{uri}: {body}");
        }
    }
}

#[tokio::test]
async fn current_json_congress_series_are_validated() {
    use usnm_store::ObjectStore as _;
    let dir = tempfile::tempdir().unwrap();
    let store = LocalStore::new(dir.path());
    let base = json!({
        "index_version": "v1", "indexes": ["pages-base-1"], "reference": "v1",
        "bounds": {"from": "1895-01-01", "to": "1897-12-31"}, "published_at": "2026-10-05T00:00:00Z",
    });
    let with_series = |series: Value, indexes: Value| {
        let mut v = base.clone();
        v["congress"] = json!({"indexes": indexes, "series": [series]});
        v
    };
    let globe =
        |from: &str, to: &str| json!({"series": "globe", "pages": 3, "from": from, "to": to});
    let read = |v: Value| {
        let store = &store;
        async move {
            store
                .put(
                    "current.json",
                    serde_json::to_vec(&v).unwrap(),
                    "application/json",
                )
                .await
                .unwrap();
            usnm_api::refdata::read_current(store).await
        }
    };
    let ok = globe("1833-12-02", "1873-03-03");
    assert!(read(with_series(ok.clone(), json!(["congress-1"])))
        .await
        .is_ok());
    // One day is a span.
    let day = globe("1850-01-01", "1850-01-01");
    assert!(read(with_series(day, json!(["congress-1"]))).await.is_ok());
    // Requests clamp their dates to a series' span, so it can't run backwards.
    let backwards = globe("1873-03-03", "1833-12-02");
    let err = read(with_series(backwards, json!(["congress-1"])))
        .await
        .unwrap_err();
    assert!(err.contains("ends before it starts"), "{err}");
    let mut unknown = ok.clone();
    unknown["series"] = json!("hansard");
    let err = read(with_series(unknown, json!(["congress-1"])))
        .await
        .unwrap_err();
    assert!(err.contains("unknown Congress series"), "{err}");
    for bad in [json!([]), json!(["../escape"])] {
        assert!(
            read(with_series(ok.clone(), bad.clone())).await.is_err(),
            "{bad}"
        );
    }
}

#[tokio::test]
async fn meta_lists_congress_series_and_its_place() {
    let s = congress_state().await;
    let (st, _, meta) = get(&s, "/v1/meta").await;
    assert_eq!(st, StatusCode::OK);
    let series: Vec<&str> = meta["congress"]["series"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["series"].as_str().unwrap())
        .collect();
    assert_eq!(series, ["annals", "register", "globe", "serialset"]);
    assert_eq!(meta["congress"]["place"]["id"], "PC001");
    assert_eq!(meta["congress"]["place"]["name"], "Congress");
    // A version without Congress pages says so.
    let (_, _, plain) = get(&state_with(None).await, "/v1/meta").await;
    assert!(plain["congress"].is_null());
}

#[tokio::test]
async fn days_take_the_congress_place_for_congress_searches_only() {
    let s = congress_state().await;
    let (st, _, body) = get(
        &s,
        "/v1/days?q=%22fugitive+slave%22&from=1850-01-01&to=1850-12-31&source=congress&place=PC001",
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["places"][0]["id"], "PC001");
    let hits: u64 = body["places"][0]["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h.as_u64().unwrap())
        .sum();
    let want = congress_oracle("fugitive slave", "1850-01-01", "1850-12-31", None);
    assert!(want > 0);
    assert_eq!(hits, want);
    // A newspaper search doesn't know the place.
    let (st, _, _) = get(&s, "/v1/days?q=gold&place=PC001").await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, _, _) = get(&s, "/v1/days?q=gold&place=PC001&source=news").await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

/// Switching Congress search off refuses Congress searches before any
/// cache is asked, so responses cached while it was on aren't served.
#[tokio::test]
async fn congress_responses_cached_before_the_switch_goes_off_are_refused() {
    let s = congress_state().await;
    let range = "q=%22fugitive+slave%22&from=1850-01-01&to=1850-12-31&source=congress";
    let uris = [
        format!("/v1/aggregate?{range}"),
        format!("/v1/hits?{range}&place=PC001"),
        format!("/v1/days?{range}&place=PC001"),
    ];
    let mut keys = Vec::new();
    for uri in &uris {
        let (st, _, body) = get(&s, uri).await;
        assert_eq!(st, StatusCode::OK, "{uri}: {body}");
        if let Some(canonical) = body["query"]["canonical"].as_str() {
            keys.push(format!("fixture-v1|aggregate|{canonical}"));
        }
    }
    assert!(s.cache.contains_key(&keys[0]), "{keys:?}");
    // The same version, switched off.
    let serving = s.snapshot.load_full();
    let mut refdata = congress_refdata().await;
    refdata.congress_search = false;
    s.snapshot.store(Arc::new(usnm_api::Snapshot {
        refdata,
        backend: serving.backend.clone(),
        congress_dropped: false,
    }));
    for uri in &uris {
        let (st, _, body) = get(&s, uri).await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{uri}: {body}");
        assert_eq!(body["type"], "/errors/unsupported");
    }
    // Still cached: only refused.
    assert!(s.cache.contains_key(&keys[0]));
    // Newspaper searches answer as before.
    let (st, _, _) = get(&s, "/v1/aggregate?q=fever").await;
    assert_eq!(st, StatusCode::OK);
}

/// Without series a Congress set has no dates of its own, so a Congress
/// search would be clamped to the newspapers': refused instead.
#[tokio::test]
async fn a_congress_set_without_series_is_not_searched() {
    let mut backend = fixture_backend();
    backend.add_index(CONGRESS, load_docs(CONGRESS));
    let mut refdata = congress_refdata().await;
    if let Some(c) = refdata.current.congress.as_mut() {
        c.series.clear();
    }
    let s = Arc::new(AppState::new(config(), Arc::new(backend), refdata));
    let (st, _, body) = get(
        &s,
        "/v1/aggregate?q=%22fugitive+slave%22&from=1850-01-01&to=1850-12-31&source=congress",
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["type"], "/errors/unsupported");
    let (_, _, meta) = get(&s, "/v1/meta").await;
    assert!(meta["congress"].is_null());
}

/// A state loaded from `dir` as the API loads one, with Congress search on
/// or off.
async fn loaded_state(dir: &std::path::Path, congress_search: bool) -> Arc<AppState> {
    let loader = Loader {
        reference: Arc::new(LocalStore::new(dir)),
        engine: Engine::Memory {
            indexes_dir: dir.join("indexes"),
        },
        congress_search,
    };
    let snapshot = loader.snapshot().await.unwrap();
    let mut cfg = config();
    cfg.congress_search = congress_search;
    Arc::new(AppState::with_loader(cfg, snapshot, None))
}

/// A Congress index that can't be loaded only takes the Congress pages out
/// of a version: its newspapers still publish. Switched off, the Congress
/// index isn't read at all.
#[tokio::test]
async fn a_broken_congress_index_never_holds_back_the_newspapers() {
    let dir = temp_data_dir("congress-load");
    let pointer = dir.join("current.json");
    let mut current: Value =
        serde_json::from_str(&std::fs::read_to_string(&pointer).unwrap()).unwrap();
    current["congress"] = json!({
        "indexes": ["congress-missing"],
        "series": [{"series": "globe", "pages": 78, "from": "1850-01-07", "to": "1850-09-30"}],
    });
    std::fs::write(&pointer, current.to_string()).unwrap();
    let congress =
        "/v1/aggregate?q=%22fugitive+slave%22&from=1850-01-01&to=1850-12-31&source=congress";
    for on in [true, false] {
        let s = loaded_state(&dir, on).await;
        let (st, _, body) = get(&s, "/v1/aggregate?q=fever").await;
        assert_eq!(st, StatusCode::OK, "{on}: {body}");
        assert!(body["total"]["hits"].as_u64().unwrap() > 0);
        let (st, _, body) = get(&s, congress).await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{on}: {body}");
        let (_, _, meta) = get(&s, "/v1/meta").await;
        assert!(meta["congress"].is_null(), "{on}");
    }

    // With its index there, the version searches its Congress pages too.
    current["congress"]["indexes"] = json!([CONGRESS]);
    std::fs::write(&pointer, current.to_string()).unwrap();
    let s = loaded_state(&dir, true).await;
    let (st, _, body) = get(&s, congress).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(
        body["total"]["hits"],
        congress_oracle("fugitive slave", "1850-01-01", "1850-12-31", None)
    );
    // Switched off, the index was never loaded.
    let s = loaded_state(&dir, false).await;
    let filters = usnm_core::params::Filters {
        from: chrono::NaiveDate::from_ymd_opt(1789, 1, 1).unwrap(),
        to: chrono::NaiveDate::from_ymd_opt(1994, 12, 31).unwrap(),
        states: vec![],
        lccns: vec![],
        langs: vec![],
        front_only: false,
        series: vec![],
    };
    let page = usnm_search::HitsQuery {
        limit: 1,
        ..Default::default()
    };
    let read = s
        .snapshot
        .load_full()
        .backend
        .hits(
            &usnm_search::IndexSet::new(vec![CONGRESS.into()]),
            &parse("slave").unwrap(),
            &filters,
            &page,
        )
        .await;
    assert!(read.is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A Congress set dropped at load comes back at the next refresh of the same
/// version once it can be loaded, without republishing anything else; until
/// then the refresh changes nothing (#229).
#[tokio::test]
async fn a_dropped_congress_set_comes_back_at_the_next_refresh() {
    let dir = temp_data_dir("congress-retry");
    let pointer = dir.join("current.json");
    let mut current: Value =
        serde_json::from_str(&std::fs::read_to_string(&pointer).unwrap()).unwrap();
    current["congress"] = json!({
        "indexes": ["congress-late"],
        "series": [{"series": "globe", "pages": 78, "from": "1850-01-07", "to": "1850-09-30"}],
    });
    std::fs::write(&pointer, current.to_string()).unwrap();
    let loader = || Loader {
        reference: Arc::new(LocalStore::new(&dir)),
        engine: Engine::Memory {
            indexes_dir: dir.join("indexes"),
        },
        congress_search: true,
    };
    let snapshot = loader().snapshot().await.unwrap();
    assert!(snapshot.congress_dropped);
    let s = Arc::new(AppState::with_loader(config(), snapshot, Some(loader())));
    let congress =
        "/v1/aggregate?q=%22fugitive+slave%22&from=1850-01-01&to=1850-12-31&source=congress";

    // Still missing: the refresh keeps the snapshot as it is.
    let before = Arc::as_ptr(&s.snapshot.load_full());
    assert!(!reload_if_changed(&s).await.unwrap());
    assert_eq!(Arc::as_ptr(&s.snapshot.load_full()), before);
    let (st, _, _) = get(&s, congress).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);

    // The index arrives: the next refresh brings the Congress pages back.
    std::fs::copy(
        dir.join("indexes").join(format!("{CONGRESS}.jsonl")),
        dir.join("indexes").join("congress-late.jsonl"),
    )
    .unwrap();
    assert!(reload_if_changed(&s).await.unwrap());
    assert!(!s.snapshot.load_full().congress_dropped);
    let (st, _, body) = get(&s, congress).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(
        body["total"]["hits"],
        congress_oracle("fugitive slave", "1850-01-01", "1850-12-31", None)
    );
    let (_, _, meta) = get(&s, "/v1/meta").await;
    assert_eq!(meta["congress"]["place"]["id"], "PC001");
    // Nothing left to bring back: the next refresh is a no-op again.
    assert!(!reload_if_changed(&s).await.unwrap());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A same-version `current.json` whose `page_language` changed isn't
/// loaded, not even to bring back a dropped Congress set: the caches key on
/// the version, so the language mode is fixed for it (#289).
#[tokio::test]
async fn a_changed_page_language_under_the_same_version_is_not_loaded() {
    let dir = temp_data_dir("congress-retry-page-language");
    let pointer = dir.join("current.json");
    let mut current: Value =
        serde_json::from_str(&std::fs::read_to_string(&pointer).unwrap()).unwrap();
    current["congress"] = json!({
        "indexes": ["congress-late"],
        "series": [{"series": "globe", "pages": 78, "from": "1850-01-07", "to": "1850-09-30"}],
    });
    std::fs::write(&pointer, current.to_string()).unwrap();
    let loader = || Loader {
        reference: Arc::new(LocalStore::new(&dir)),
        engine: Engine::Memory {
            indexes_dir: dir.join("indexes"),
        },
        congress_search: true,
    };
    let snapshot = loader().snapshot().await.unwrap();
    assert!(snapshot.congress_dropped);
    assert_eq!(snapshot.refdata.current.page_language, None);
    let s = Arc::new(AppState::with_loader(config(), snapshot, Some(loader())));
    std::fs::copy(
        dir.join("indexes").join(format!("{CONGRESS}.jsonl")),
        dir.join("indexes").join("congress-late.jsonl"),
    )
    .unwrap();

    // The Congress set could come back, but page_language changed: no reload.
    let mut changed = current.clone();
    changed["page_language"] = json!(1);
    std::fs::write(&pointer, changed.to_string()).unwrap();
    let before = Arc::as_ptr(&s.snapshot.load_full());
    assert!(!reload_if_changed(&s).await.unwrap());
    assert_eq!(Arc::as_ptr(&s.snapshot.load_full()), before);
    assert!(s.snapshot.load_full().congress_dropped);

    // The pointer restored: the Congress set comes back, in title language.
    std::fs::write(&pointer, current.to_string()).unwrap();
    assert!(reload_if_changed(&s).await.unwrap());
    let now = s.snapshot.load_full();
    assert!(!now.congress_dropped);
    assert_eq!(now.refdata.current.page_language, None);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- Page languages (#289) ----

#[path = "../../../fixtures/page_language.rs"]
mod page_language;

/// The fixture docs with their page languages (`fixtures/page_language.rs`).
fn page_language_docs(index_id: &str) -> Vec<PageDoc> {
    let path = data_dir().join("indexes").join(format!("{index_id}.jsonl"));
    std::io::BufReader::new(std::fs::File::open(path).unwrap())
        .lines()
        .map(|l| {
            let mut doc: Value = serde_json::from_str(&l.unwrap()).unwrap();
            page_language::add(&mut doc);
            serde_json::from_value(doc).unwrap()
        })
        .collect()
}

/// The fixture version with page languages: its documents have them, its
/// `current.json` says `page_language: version`, and its snapshot has
/// `page_language_baselines.json`, counted from the documents (each page
/// once, by its own languages).
async fn page_language_state(version: u32, side: Option<&str>) -> (Arc<AppState>, Vec<PageDoc>) {
    use sha2::Digest;
    let mut backend = MemoryBackend::new();
    let mut docs = Vec::new();
    for id in ["pages-base-fixture", "pages-delta-fixture-1"] {
        let d = page_language_docs(id);
        docs.extend(d.clone());
        backend.add_index(id, d);
    }
    let mut sets: BTreeMap<Vec<String>, BTreeMap<String, BTreeMap<u32, u32>>> = BTreeMap::new();
    for d in &docs {
        *sets
            .entry(d.page_language.clone())
            .or_default()
            .entry(d.place_id.clone())
            .or_default()
            .entry(d.day)
            .or_default() += 1;
    }
    let body = serde_json::to_vec(&json!({"sets": sets
        .into_iter()
        .map(|(languages, places)| json!({
            "languages": languages,
            "baselines": places
                .into_iter()
                .map(|(p, days)| (p, days.into_iter().collect::<Vec<_>>()))
                .collect::<BTreeMap<_, _>>(),
        }))
        .collect::<Vec<_>>()}))
    .unwrap();
    let dir = temp_data_dir(&format!(
        "page-language-{version}-{}",
        side.unwrap_or("default")
    ));
    std::fs::write(dir.join("fixture-v1/page_language_baselines.json"), &body).unwrap();
    let manifest = dir.join("fixture-v1/manifest.json");
    let mut m: Value = serde_json::from_str(&std::fs::read_to_string(&manifest).unwrap()).unwrap();
    let digest: String = sha2::Sha256::digest(&body)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    m["files"].as_array_mut().unwrap().push(json!({
        "path": "page_language_baselines.json", "sha256": digest, "bytes": body.len(),
    }));
    std::fs::write(&manifest, m.to_string()).unwrap();
    let current = dir.join("current.json");
    let mut c: Value = serde_json::from_str(&std::fs::read_to_string(&current).unwrap()).unwrap();
    c["page_language"] = version.into();
    std::fs::write(&current, c.to_string()).unwrap();
    let rd = RefData::load(&LocalStore::new(&dir)).await.unwrap();
    let mut cfg = config();
    if let Some(side) = side {
        cfg.page_language_filter = match side {
            "match" => usnm_search::PageLanguageSide::Match,
            _ => usnm_search::PageLanguageSide::Smaller,
        };
    }
    (Arc::new(AppState::new(cfg, Arc::new(backend), rd)), docs)
}

/// On a version with page languages at the API's version, `lang=` keeps
/// the pages written in any of the languages, a page in two found by
/// either; the baselines count pages by theirs, and the counts by language
/// are by page language. `/v1/meta` says so.
#[tokio::test]
async fn lang_filters_by_page_language_on_a_version_with_them() {
    let (s, docs) = page_language_state(usnm_core::page_language::VERSION, None).await;
    let (status, _, meta) = get(&s, "/v1/meta").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(meta["language_filter"], "page");
    let pages_in = |l: &str| {
        docs.iter()
            .filter(|d| d.page_language.iter().any(|x| x == l))
            .count()
    };
    let langs = meta["languages"].as_array().unwrap();
    for l in langs {
        let code = l["code"].as_str().unwrap();
        assert_eq!(l["pages"], pages_in(code) as u64, "{code}");
    }
    // Most pages first: English, then German.
    assert_eq!(langs[0]["code"], "eng");
    assert_eq!(langs[1]["code"], "ger");

    let q = "gold";
    let node = parse(q).unwrap();
    let matching = |l: &[&str]| -> Vec<&PageDoc> {
        docs.iter()
            .filter(|d| {
                d.page_language.iter().any(|x| l.contains(&x.as_str()))
                    && eval(&node, &tokenize(&d.text, FIXTURE))
            })
            .collect()
    };
    for lang in ["eng", "ger", "spa", "fre", "ger,spa"] {
        let (status, _, body) = get(&s, &format!("/v1/aggregate?q={q}&lang={lang}")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let l: Vec<&str> = lang.split(',').collect();
        let want = matching(&l);
        assert_eq!(body["total"]["hits"], want.len() as u64, "{lang}");
        assert_eq!(body["languages"]["by"], "page", "{lang}");
        // A matching page counts in each of its own languages.
        let mut by_lang: BTreeMap<String, u64> = BTreeMap::new();
        for d in &want {
            for x in &d.page_language {
                *by_lang.entry(x.clone()).or_default() += 1;
            }
        }
        let got: BTreeMap<String, u64> = body["languages"]["code"]
            .as_array()
            .unwrap()
            .iter()
            .zip(body["languages"]["hits"].as_array().unwrap())
            .map(|(c, h)| (c.as_str().unwrap().to_owned(), h.as_u64().unwrap()))
            .collect();
        assert_eq!(got, by_lang, "{lang}");
        // The baseline: the pages written in the languages, each once.
        let baseline = docs
            .iter()
            .filter(|d| d.page_language.iter().any(|x| l.contains(&x.as_str())))
            .count() as u64;
        assert_eq!(body["total"]["baseline_pages"], baseline, "{lang}");
    }
    // Without `lang`, an English query's baseline is the English pages (#237).
    let (_, _, body) = get(&s, "/v1/aggregate?q=gold").await;
    assert_eq!(body["baseline"]["languages"], json!(["eng"]));
    assert_eq!(body["total"]["baseline_pages"], pages_in("eng") as u64);
}

/// Rendered as the complement or as the match side, a page-language filter
/// finds the same pages.
#[tokio::test]
async fn both_sides_of_a_page_language_filter_answer_alike() {
    let (smaller, _) =
        page_language_state(usnm_core::page_language::VERSION, Some("smaller")).await;
    let (matching, _) = page_language_state(usnm_core::page_language::VERSION, Some("match")).await;
    let eng = vec!["eng".to_owned()];
    let side = |s: &Arc<AppState>| s.snapshot.load().refdata.index_set().language_match(&eng);
    assert!(matches!(
        side(&smaller),
        Some(usnm_search::LanguageMatch::NotPageSets(_))
    ));
    assert_eq!(
        side(&matching),
        Some(usnm_search::LanguageMatch::Page(eng.clone()))
    );
    for uri in [
        "/v1/aggregate?q=gold&lang=eng",
        "/v1/aggregate?q=railroad&lang=eng,ger",
        "/v1/aggregate?q=cotton&lang=ger",
        "/v1/hits?q=gold&lang=eng&limit=20",
    ] {
        let (_, _, mut a) = get(&smaller, uri).await;
        let (_, _, mut b) = get(&matching, uri).await;
        for body in [&mut a, &mut b] {
            if let Some(o) = body.as_object_mut() {
                o.remove("timing_ms");
            }
        }
        assert_eq!(a, b, "{uri}");
    }
}

/// The Japanese index set uses page languages too on such a version, so a
/// Japanese search's `lang=`, counts and baselines agree (its pages carry
/// the languages the baselines count them in); never on a title-language
/// version.
#[tokio::test]
async fn the_japanese_index_set_follows_the_version_s_language_mode() {
    let ja =
        json!({"indexes": ["pages-ja-fixture"], "fold": usnm_core::ja::FOLD_VERSION, "pages": 1});
    let mut rd = refdata().await;
    rd.current.ja = serde_json::from_value(ja.clone()).unwrap();
    assert!(rd.ja_index_set().unwrap().page_languages().is_none());
    // The page-language version's snapshot, its pointer naming a Japanese index.
    let _ = page_language_state(usnm_core::page_language::VERSION, Some("ja")).await;
    let dir = std::env::temp_dir().join(format!(
        "usnm-page-language-{}-ja-{}",
        usnm_core::page_language::VERSION,
        std::process::id()
    ));
    let current = dir.join("current.json");
    let mut c: Value = serde_json::from_str(&std::fs::read_to_string(&current).unwrap()).unwrap();
    c["ja"] = ja;
    std::fs::write(&current, c.to_string()).unwrap();
    let rd = RefData::load(&LocalStore::new(&dir)).await.unwrap();
    let set = rd.ja_index_set().unwrap();
    assert!(set.page_languages().is_some());
    assert_eq!(set.language_field(), "page_language");
    let ger = vec!["ger".to_owned()];
    assert_eq!(
        set.language_match(&ger),
        Some(usnm_search::LanguageMatch::Page(ger.clone()))
    );
}

/// Page languages of another version than the API's are left alone:
/// `lang=` means title language, as before the setting.
#[tokio::test]
async fn page_languages_of_another_version_are_not_used() {
    let (s, _) = page_language_state(usnm_core::page_language::VERSION + 1, None).await;
    let (_, _, meta) = get(&s, "/v1/meta").await;
    assert_eq!(meta["language_filter"], "title");
    let (status, _, body) = get(&s, "/v1/aggregate?q=gold&lang=ger").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["languages"].get("by").is_none(), "{body}");
    // The title-language baseline: P00002 and P00006.
    assert_eq!(body["total"]["baseline_pages"], 2 * 312);
    // And the fixture version itself: title language.
    let (_, _, meta) = get(&state_with(None).await, "/v1/meta").await;
    assert_eq!(meta["language_filter"], "title");
}
