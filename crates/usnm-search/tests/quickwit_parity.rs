//! Spike S-2: the Quickwit backend must return exactly what the in-memory
//! reference backend returns on the fixture corpus.
//!
//! Runs only when `QUICKWIT_URL` points at a Quickwit 0.9 node that has the
//! fixture indexes loaded (`scripts/quickwit-fixtures.sh` does both); otherwise
//! every test returns early. CI runs it in the `quickwit` job.

use std::io::BufRead;
use std::path::PathBuf;
use std::time::Duration;

use chrono::NaiveDate;
use usnm_core::params::Filters;
use usnm_core::query::{parse, parse_with, Mode, Node, QueryError};
use usnm_core::text::Analyzer;
use usnm_core::time::{BucketSpec, BucketUnit};
use usnm_search::memory::MemoryBackend;
use usnm_search::quickwit::QuickwitBackend;
use usnm_search::{
    CubeCell, HitSort, HitsQuery, IndexSet, PageDoc, SearchBackend, SearchError, Summary,
};

const INDEXES: [&str; 2] = ["pages-base-fixture", "pages-delta-fixture-1"];
/// The same pages with their common-word pairs at version 1
/// (`Analyzer::V1`), as the indexes built before #168 have them; the ones
/// above have the latest (`scripts/quickwit-fixtures.sh`).
const INDEXES_V1: [&str; 2] = ["pages-base-fixture-cg1", "pages-delta-fixture-1-cg1"];
/// The Japanese pages (#139), searched on their own.
const JA_INDEX: &str = "pages-ja-fixture";
/// The Congress pages (#229), their own index set too.
const CONGRESS_INDEX: &str = "congress-fixture";

fn quickwit() -> Option<QuickwitBackend> {
    let url = std::env::var("QUICKWIT_URL")
        .ok()
        .filter(|u| !u.is_empty())?;
    Some(QuickwitBackend::new(&url, Duration::from_secs(30)).expect("client"))
}

fn memory() -> MemoryBackend {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/data/indexes");
    let mut m = MemoryBackend::new();
    let files = INDEXES
        .iter()
        .chain(&INDEXES)
        .chain([&JA_INDEX, &CONGRESS_INDEX]);
    let ids = INDEXES
        .iter()
        .chain(&INDEXES_V1)
        .chain([&JA_INDEX, &CONGRESS_INDEX]);
    for (file, id) in files.zip(ids) {
        let file = std::fs::File::open(dir.join(format!("{file}.jsonl"))).expect("fixture");
        let docs: Vec<PageDoc> = std::io::BufReader::new(file)
            .lines()
            .map(|l| serde_json::from_str(&l.expect("line")).expect("doc"))
            .collect();
        m.add_index(id, docs);
    }
    m
}

/// [`usnm_core::query::build`] with the latest analyzer.
fn build(q: &str, mode: Option<Mode>, near: u8, fuzzy: u8) -> Result<Node, QueryError> {
    usnm_core::query::build(q, mode, near, fuzzy, Analyzer::LATEST)
}

fn d(s: &str) -> NaiveDate {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
}

fn filters(from: &str, to: &str) -> Filters {
    Filters {
        from: d(from),
        to: d(to),
        states: vec![],
        lccns: vec![],
        langs: vec![],
        front_only: false,
        series: Vec::new(),
    }
}

fn queries() -> Vec<(&'static str, Node)> {
    vec![
        ("term", parse("gold").unwrap()),
        ("phrase", parse(r#""cross of gold""#).unwrap()),
        (
            "phrase with stop word",
            parse(r#""board of health""#).unwrap(),
        ),
        ("sloppy phrase", parse(r#""fever port"~4"#).unwrap()),
        ("and", parse("free silver").unwrap()),
        ("or", parse("(yellow OR orator)").unwrap()),
        ("not", parse("gold -standard").unwrap()),
        ("prefix", parse("silve*").unwrap()),
        // Wildcards inside words (#124).
        ("wildcard ?", parse("convent?on").unwrap()),
        ("wildcard *", parse("conve*tion").unwrap()),
        ("wildcards and", parse("railr*ad weath?r -cotton").unwrap()),
        ("wildcard no match", parse("golde?s").unwrap()),
        (
            "mode any",
            build("bankers orator", Some(Mode::Any), 0, 0).unwrap(),
        ),
        (
            "mode near",
            build("speaker convention", Some(Mode::Near), 3, 0).unwrap(),
        ),
        ("no match", parse("zyzzyva").unwrap()),
        // American Stories' text (05 §5.5.4): words only in its headlines,
        // words each OCR misread where the other didn't, and a query whose
        // words are in different texts of one page.
        ("only in american stories", parse("bimetallism").unwrap()),
        (
            "phrase only in american stories",
            parse(r#""boy orator""#).unwrap(),
        ),
        (
            "near only in american stories",
            build("bryan chicago", Some(Mode::Near), 2, 0).unwrap(),
        ),
        ("not in american stories", parse("gold -bryan").unwrap()),
        ("misread by loc", parse("goid").unwrap()),
        ("misread by american stories", parse("golcl").unwrap()),
        ("prefix in american stories", parse("bimet*").unwrap()),
        ("across texts", parse("bryan speaker").unwrap()),
        // Fractions (#168): `½` and `61¼` are each one word, as the analyzer
        // has them, and `1/2` is two.
        ("vulgar fraction", parse("½").unwrap()),
        ("fraction in a number", parse("61¼").unwrap()),
        (
            "phrase with a fraction and a common word",
            parse(r#""½ higher at 61¼""#).unwrap(),
        ),
        (
            "near with a fraction",
            build("wheat ½", Some(Mode::Near), 2, 0).unwrap(),
        ),
        ("slash between digits", parse("1/2").unwrap()),
        // Characters that fold to Japanese ones stay themselves (#241), as
        // the analyzer has them: `b㊁` is one word.
        ("circled ideograph", parse("㊀").unwrap()),
        ("circled ideograph in a word", parse("b㊁").unwrap()),
    ]
}

/// Queries parsed with analyzer version 1, as the API parses them on an
/// index built with it (#168): `½` folds to `1⁄2`, which Quickwit analyzes
/// again into the phrase "1 2", as the memory backend does. `㊀` folds to
/// `一` and `b㊁` to `b二`, which the analyzer has on no page (#241).
#[tokio::test]
async fn version_1_queries_match_the_reference_backend() {
    let Some(qw) = quickwit() else {
        eprintln!("QUICKWIT_URL not set; skipping");
        return;
    };
    let mem = memory();
    let v1 = Analyzer::V1;
    let set = IndexSet::new(INDEXES_V1.iter().map(|s| (*s).to_owned()).collect()).with_analyzer(v1);
    let f = filters("1895-01-01", "1897-12-31");
    let spec = BucketSpec::new(BucketUnit::Month, f.from, f.to);
    let mut found = 0;
    for q in [
        "½",
        "61¼",
        "1/2",
        "wheat ½",
        r#""closed ½ lower""#,
        "gold",
        "㊀",
        "b㊁",
    ] {
        let node = parse_with(q, v1).unwrap();
        let want = sorted(mem.summary(&set, &node, &f, &spec).await.unwrap());
        let got = sorted(qw.summary(&set, &node, &f, &spec).await.expect(q));
        assert_eq!(got, want, "summary: {q}");
        found += usize::from(want.total_hits > 0);
        let page = HitsQuery {
            limit: 20,
            ..HitsQuery::default()
        };
        let want = mem.hits(&set, &node, &f, &page).await.unwrap();
        let got = qw.hits(&set, &node, &f, &page).await.expect(q);
        let ids = |h: &usnm_search::HitsPage| -> Vec<String> {
            h.hits.iter().map(|h| h.doc_id.clone()).collect()
        };
        assert_eq!(ids(&got), ids(&want), "hits: {q}");
        assert_eq!(
            got.hits.iter().map(|h| &h.snippets).collect::<Vec<_>>(),
            want.hits.iter().map(|h| &h.snippets).collect::<Vec<_>>(),
            "snippets: {q}"
        );
    }
    assert!(found >= 3, "only {found} queries matched the fixtures");
}

/// The main indexes searched in LoC's text alone, and in both texts
/// (05 §5.5.4).
fn sets() -> Vec<(&'static str, IndexSet)> {
    let loc = IndexSet::new(INDEXES.iter().map(|s| (*s).to_owned()).collect());
    let both = loc.clone().with_american_stories(true);
    vec![("loc", loc), ("both texts", both)]
}

fn filter_cases() -> Vec<(&'static str, Filters)> {
    let all = filters("1895-01-01", "1897-12-31");
    let mut states = all.clone();
    states.states = vec!["GA".into(), "SC".into()];
    let mut lccn = all.clone();
    lccn.lccns = vec!["sn99000002".into()];
    let mut front = all.clone();
    front.front_only = true;
    let mut lang = all.clone();
    lang.langs = vec!["eng".into()];
    vec![
        ("all", all),
        ("1896 H2", filters("1896-06-01", "1896-12-31")),
        ("states", states),
        ("lccn", lccn),
        ("front pages", front),
        ("language", lang),
    ]
}

fn specs(f: &Filters) -> Vec<BucketSpec> {
    let mut out = vec![
        BucketSpec::new(BucketUnit::Year, f.from, f.to),
        BucketSpec::new(BucketUnit::Month, f.from, f.to),
        BucketSpec::new(BucketUnit::Week, f.from, f.to),
    ];
    // Day buckets over a short window, with a start that isn't a week boundary.
    out.push(BucketSpec::new(
        BucketUnit::Day,
        d("1896-07-03"),
        d("1896-09-17"),
    ));
    out
}

fn sorted(mut s: Summary) -> Summary {
    s.places.sort_by(|a, b| a.place_id.cmp(&b.place_id));
    s
}

fn sorted_cells(mut c: Vec<CubeCell>) -> Vec<CubeCell> {
    c.sort_by(|a, b| (&a.place_id, a.bucket).cmp(&(&b.place_id, b.bucket)));
    c
}

#[tokio::test]
async fn summaries_and_cubes_match_the_reference_backend() {
    let Some(qw) = quickwit() else {
        eprintln!("QUICKWIT_URL not set; skipping");
        return;
    };
    let mem = memory();
    let mut checked = 0;
    for ((sname, set), (qname, q)) in sets()
        .into_iter()
        .flat_map(|s| queries().into_iter().map(move |q| (s.clone(), q)))
    {
        let set = &set;
        for (fname, f) in filter_cases() {
            for spec in specs(&f) {
                let mut f = f.clone();
                // Day specs use their own window.
                f.from = spec.from;
                f.to = spec.to;
                let ctx = format!("{sname} / {qname} / {fname} / {:?}", spec.unit);
                let want = sorted(mem.summary(set, &q, &f, &spec).await.unwrap());
                let got = sorted(qw.summary(set, &q, &f, &spec).await.expect(&ctx));
                assert_eq!(got, want, "summary: {ctx}");

                let all: Vec<u8> = (0..8).collect();
                let want = sorted_cells(mem.cube(set, &q, &f, &spec, &all).await.unwrap());
                let got = sorted_cells(qw.cube(set, &q, &f, &spec, &all).await.expect(&ctx));
                assert_eq!(got, want, "cube: {ctx}");

                // Sharded cubes must partition the full cube exactly.
                let mut parts = Vec::new();
                for shards in [vec![0u8, 2, 4, 6], vec![1, 3, 5, 7]] {
                    parts.extend(qw.cube(set, &q, &f, &spec, &shards).await.expect(&ctx));
                }
                assert_eq!(sorted_cells(parts), want, "sharded cube: {ctx}");

                // A cube of some places (`/v1/days`) is the full cube's cells
                // for those places, one with no pages included.
                let places = vec![
                    "P00002".to_owned(),
                    "P00005".to_owned(),
                    "P99999".to_owned(),
                ];
                let want: Vec<CubeCell> = want
                    .iter()
                    .filter(|c| places.contains(&c.place_id))
                    .cloned()
                    .collect();
                let got = qw
                    .place_cube(set, &q, &f, &spec, &places)
                    .await
                    .expect(&ctx);
                assert_eq!(sorted_cells(got), want, "place cube: {ctx}");
                let mem_got = mem.place_cube(set, &q, &f, &spec, &places).await.unwrap();
                assert_eq!(sorted_cells(mem_got), want, "memory place cube: {ctx}");
                checked += 1;
            }
        }
    }
    assert!(checked > 100, "only {checked} cases ran");
}

/// The fixtures exercise American Stories' text: some pages match only
/// there, and only when the index set searches it.
#[tokio::test]
async fn american_stories_text_adds_pages_only_when_searched() {
    let mem = memory();
    let f = filters("1895-01-01", "1897-12-31");
    let spec = BucketSpec::new(BucketUnit::Year, f.from, f.to);
    let total = |set: IndexSet, q: &str| {
        let (mem, f, spec, q) = (&mem, &f, &spec, parse(q).unwrap());
        async move { mem.summary(&set, &q, f, spec).await.unwrap().total_hits }
    };
    let [(_, loc), (_, both)] = <[_; 2]>::try_from(sets()).ok().unwrap();
    // Only American Stories has these: its headlines, its misreading.
    for q in ["bimetallism", r#""boy orator""#, "bryan", "golcl"] {
        assert_eq!(total(loc.clone(), q).await, 0, "{q}");
        assert!(total(both.clone(), q).await > 0, "{q}");
    }
    // It reads some words LoC misread.
    for q in ["gold", "silver"] {
        assert!(
            total(both.clone(), q).await > total(loc.clone(), q).await,
            "{q}"
        );
    }
    // LoC's misreading: no more pages.
    assert!(total(loc.clone(), "goid").await > 0);
    assert_eq!(
        total(both.clone(), "goid").await,
        total(loc.clone(), "goid").await
    );
    assert!(total(both.clone(), "gold -bryan").await < total(both.clone(), "gold").await);
}

/// The aggregate's count of pages only American Stories' text matches
/// (05 §5.5.4) is the reference's.
#[tokio::test]
async fn american_stories_only_counts_match_the_reference_backend() {
    let Some(qw) = quickwit() else {
        eprintln!("QUICKWIT_URL not set; skipping");
        return;
    };
    let mem = memory();
    let [_, (_, both)] = <[_; 2]>::try_from(sets()).ok().unwrap();
    let mut found = 0;
    for (qname, q) in queries() {
        for (fname, f) in filter_cases() {
            let ctx = format!("{qname} / {fname}");
            let want = mem.american_stories_only(&both, &q, &f).await.unwrap();
            let got = qw.american_stories_only(&both, &q, &f).await.expect(&ctx);
            assert_eq!(got, want, "{ctx}");
            found += got;
        }
    }
    assert!(found > 0, "no page matched only in American Stories' text");
}

/// The count is the number of hits whose `matched_in` is American Stories'
/// text alone, for every query shape, `NOT` and words from both texts
/// included.
#[tokio::test]
async fn american_stories_only_counts_the_hits_matched_only_there() {
    let mem = memory();
    let [_, (_, both)] = <[_; 2]>::try_from(sets()).ok().unwrap();
    let all = HitsQuery {
        limit: 10_000,
        ..HitsQuery::default()
    };
    let mut found = 0;
    for (qname, q) in queries() {
        for (fname, f) in filter_cases() {
            let hits = mem.hits(&both, &q, &f, &all).await.unwrap();
            assert_eq!(hits.hits.len() as u64, hits.total);
            let only = hits
                .hits
                .iter()
                .filter(|h| h.matched_in.as_deref() == Some(&["american_stories"][..]))
                .count() as u64;
            let counted = mem.american_stories_only(&both, &q, &f).await.unwrap();
            assert_eq!(counted, only, "{qname} / {fname}");
            found += only;
        }
    }
    assert!(found > 0, "no page matched only in American Stories' text");
}

/// Quickwit 0.9.1's own snippets mark nothing for a wildcard (#124): it
/// returns no fragment for `text:convent?on`. The hits' snippets don't come
/// from Quickwit (#126), so the OCR variant a wildcard finds is still marked.
#[tokio::test]
async fn wildcard_hits_mark_the_words_they_match() {
    let Some(qw) = quickwit() else {
        eprintln!("QUICKWIT_URL not set; skipping");
        return;
    };
    let f = filters("1895-01-01", "1897-12-31");
    let page = HitsQuery {
        limit: 20,
        ..Default::default()
    };
    // Only the pages with the misread `conventlon`, in LoC's text (American
    // Stories' text has `convention` on them, which `-convention` excludes).
    let (_, set) = sets().remove(0);
    let q = parse("convent?on -convention").unwrap();
    let hits = qw.hits(&set, &q, &f, &page).await.expect("hits");
    assert!(hits.total > 0);
    for h in &hits.hits {
        assert!(
            h.snippets
                .iter()
                .any(|s| s.to_lowercase().contains("<mark>conventlon</mark>")),
            "{:?}",
            h.snippets
        );
    }
}

#[tokio::test]
async fn hits_pages_match_the_reference_backend() {
    let Some(qw) = quickwit() else {
        eprintln!("QUICKWIT_URL not set; skipping");
        return;
    };
    let mem = memory();
    let f = filters("1895-01-01", "1897-12-31");
    let mut from_american_stories = 0;
    for ((sname, set), (qname, q)) in sets()
        .into_iter()
        .flat_map(|s| queries().into_iter().map(move |q| (s.clone(), q)))
    {
        let set = &set;
        for selector in [
            HitsQuery {
                place_id: Some("P00001".into()),
                ..Default::default()
            },
            HitsQuery {
                lccn: Some("sn99000004".into()),
                ..Default::default()
            },
            HitsQuery {
                place_id: Some("P00001".into()),
                sort: HitSort::Newest,
                ..Default::default()
            },
            // Every match: how the aggregate finds the first and last page.
            HitsQuery::default(),
            HitsQuery {
                sort: HitSort::Newest,
                ..Default::default()
            },
        ] {
            for offset in [0, 5, 30] {
                let page = HitsQuery {
                    offset,
                    limit: 7,
                    days: offset == 0,
                    ..selector.clone()
                };
                let ctx = format!("{sname} / {qname} / {selector:?} / offset {offset}");
                let want = mem.hits(set, &q, &f, &page).await.unwrap();
                let got = qw.hits(set, &q, &f, &page).await.expect(&ctx);
                assert_eq!(got.total, want.total, "total: {ctx}");
                // Distinct days (#127), on the first page only. Quickwit's is a
                // HyperLogLog estimate, exact at the fixtures' size.
                assert_eq!(got.days, want.days, "days: {ctx}");
                assert_eq!(got.days.is_some(), offset == 0, "days: {ctx}");
                let key = |h: &usnm_search::Hit| {
                    (
                        h.doc_id.clone(),
                        h.day,
                        h.lccn.clone(),
                        h.place_id.clone(),
                        h.edition,
                        h.seq,
                        h.front_page,
                        // Both build snippets from the page text (#126),
                        // or American Stories' when only it matches.
                        h.snippets.clone(),
                        h.snippet_source,
                        // Which texts match, from the same stored texts.
                        h.matched_in.clone(),
                    )
                };
                assert_eq!(
                    got.hits.iter().map(key).collect::<Vec<_>>(),
                    want.hits.iter().map(key).collect::<Vec<_>>(),
                    "hits: {ctx}"
                );
                // Snippets: highlighted and safe (text escaped, only <mark> tags).
                for h in &got.hits {
                    for s in &h.snippets {
                        let stripped = s.replace("<mark>", "").replace("</mark>", "");
                        assert!(
                            !stripped.contains('<') && !stripped.contains('>'),
                            "unescaped snippet: {s}"
                        );
                    }
                }
                from_american_stories += got
                    .hits
                    .iter()
                    .filter(|h| h.snippet_source.is_some())
                    .count();
                assert!(
                    got.hits
                        .iter()
                        .all(|h| h.matched_in.is_some() == set.american_stories()),
                    "matched_in: {ctx}"
                );
                if qname == "phrase" && !got.hits.is_empty() {
                    assert!(got
                        .hits
                        .iter()
                        .any(|h| h.snippets.iter().any(|s| s.contains("<mark>"))));
                }
            }
        }
    }
    assert!(
        from_american_stories > 0,
        "no snippets from American Stories' text"
    );
}

/// "Most mentions first" (#126): Quickwit's score weighs rare words per
/// split, so its order isn't the reference's; the pages listed are the same.
#[tokio::test]
async fn relevant_hits_list_the_same_pages() {
    let Some(qw) = quickwit() else {
        eprintln!("QUICKWIT_URL not set; skipping");
        return;
    };
    let mem = memory();
    let f = filters("1895-01-01", "1897-12-31");
    for ((sname, set), (qname, q)) in sets()
        .into_iter()
        .flat_map(|s| queries().into_iter().map(move |q| (s.clone(), q)))
    {
        let set = &set;
        let mut pages = Vec::new();
        for backend in [&mem as &dyn SearchBackend, &qw] {
            let mut all = Vec::new();
            let mut offset = 0;
            loop {
                let page = HitsQuery {
                    sort: HitSort::Relevant,
                    offset,
                    limit: 500,
                    ..Default::default()
                };
                let got = backend.hits(set, &q, &f, &page).await.expect(qname);
                let n = got.hits.len();
                all.extend(got.hits.into_iter().map(|h| (h.doc_id, h.snippets)));
                offset += n;
                if n == 0 || offset as u64 >= got.total {
                    break;
                }
            }
            all.sort();
            pages.push(all);
        }
        assert_eq!(pages[1], pages[0], "relevant: {sname} / {qname}");
    }
}

#[tokio::test]
async fn fuzzy_terms_match_the_reference_backend_or_are_refused() {
    let Some(qw) = quickwit() else {
        eprintln!("QUICKWIT_URL not set; skipping");
        return;
    };
    let mem = memory();
    let set = IndexSet::new(INDEXES.iter().map(|s| (*s).to_owned()).collect());
    let f = filters("1895-01-01", "1897-12-31");
    let spec = BucketSpec::new(BucketUnit::Month, f.from, f.to);
    // OCR-style misspellings of words in the corpus.
    for (q, fuzzy) in [("silvr", 1), ("fevre", 2), ("convention", 1)] {
        let node = build(q, Some(Mode::All), 0, fuzzy).unwrap();
        match qw.summary(&set, &node, &f, &spec).await {
            Ok(got) => {
                assert!(
                    qw.capabilities().fuzzy,
                    "fuzzy query ran but capability is off"
                );
                let want = mem.summary(&set, &node, &f, &spec).await.unwrap();
                assert_eq!(sorted(got), sorted(want), "fuzzy {q}~{fuzzy}");
            }
            Err(SearchError::Unsupported(_)) => assert!(!qw.capabilities().fuzzy),
            Err(e) => panic!("fuzzy {q}~{fuzzy}: {e}"),
        }
    }
}

#[tokio::test]
async fn prepare_accepts_published_indexes_and_refuses_missing_ones() {
    let Some(qw) = quickwit() else {
        eprintln!("QUICKWIT_URL not set; skipping");
        return;
    };
    let set = IndexSet::new(INDEXES.iter().map(|s| (*s).to_owned()).collect());
    qw.prepare(&set).await.expect("fixture indexes");
    let missing = IndexSet::new(vec![
        "pages-base-fixture".into(),
        "pages-delta-missing".into(),
    ]);
    assert!(matches!(
        qw.prepare(&missing).await,
        Err(SearchError::Backend(_))
    ));
}

/// Japanese words, phrases, folding, mixed Latin, NEAR and filters on the
/// Japanese index (#139): counts, series, hits and snippets as the memory
/// backend has them. Also checks that Quickwit's query grammar takes bare
/// CJK terms and phrases, and the `whitespace` tokenizer keeps positions.
#[tokio::test]
async fn japanese_searches_match_the_reference_backend() {
    let Some(qw) = quickwit() else {
        eprintln!("QUICKWIT_URL not set; skipping");
        return;
    };
    let mem = memory();
    let set = IndexSet::new(vec![JA_INDEX.to_owned()]);
    let all = filters("1895-01-01", "1897-12-31");
    let mut jpn = filters("1896-01-01", "1896-12-31");
    jpn.langs = vec!["jpn".into()];
    let mut ca = filters("1895-01-01", "1897-12-31");
    ca.states = vec!["CA".into()];
    let queries: Vec<(&str, Node)> = vec![
        ("char", parse("年").unwrap()),
        ("word", parse("戦争").unwrap()),
        ("old form", parse("戰爭").unwrap()),
        ("small kana", parse("がつこう").unwrap()),
        ("katakana", parse("ニュース").unwrap()),
        ("phrase", parse("\"米国と日本\"").unwrap()),
        ("and", parse("東京 選挙").unwrap()),
        ("or", parse("戦争 OR 選挙").unwrap()),
        ("not", parse("日本 -戦争").unwrap()),
        ("mixed latin", parse("denver 日本").unwrap()),
        ("latin wildcard", parse("denve? 日本").unwrap()),
        ("punctuation", parse("東京、平和").unwrap()),
        ("near", build("米国 日本", Some(Mode::Near), 3, 0).unwrap()),
        ("any", build("戦争 選挙", Some(Mode::Any), 0, 0).unwrap()),
    ];
    let mut nonzero = 0;
    for (qname, q) in &queries {
        for (fname, f) in [("all", &all), ("jpn 1896", &jpn), ("CA", &ca)] {
            for spec in specs(f) {
                let mut f = f.clone();
                // Day specs use their own window.
                f.from = spec.from;
                f.to = spec.to;
                let ctx = format!("{qname} / {fname} / {:?}", spec.unit);
                let want = sorted(mem.summary(&set, q, &f, &spec).await.unwrap());
                let got = sorted(qw.summary(&set, q, &f, &spec).await.expect(&ctx));
                assert_eq!(got, want, "summary: {ctx}");
                nonzero += usize::from(want.total_hits > 0);
                let all: Vec<u8> = (0..8).collect();
                let want = sorted_cells(mem.cube(&set, q, &f, &spec, &all).await.unwrap());
                let got = sorted_cells(qw.cube(&set, q, &f, &spec, &all).await.expect(&ctx));
                assert_eq!(got, want, "cube: {ctx}");
            }
            let page = HitsQuery {
                limit: 10,
                ..Default::default()
            };
            let want = mem.hits(&set, q, f, &page).await.unwrap();
            let got = qw.hits(&set, q, f, &page).await.expect(qname);
            assert_eq!(got.total, want.total, "hits total: {qname} / {fname}");
            let key = |h: &usnm_search::Hit| {
                let ocr = (h.ocr_source.clone(), h.ocr_engine.clone());
                (h.doc_id.clone(), h.snippets.clone(), ocr)
            };
            assert_eq!(
                got.hits.iter().map(key).collect::<Vec<_>>(),
                want.hits.iter().map(key).collect::<Vec<_>>(),
                "hits: {qname} / {fname}"
            );
        }
    }
    assert!(
        nonzero > 20,
        "only {nonzero} Japanese cases matched anything"
    );
}

/// Phrases holding common words search `text_cg` (05 §5.5.3) when the
/// indexes have it: the counts, cubes, edges and hits must be exactly the
/// exact phrase's, which the reference backend finds word by word. For each
/// analyzer version the API supports: the indexes built with version 1 have
/// its pairs, and queries on them are parsed with it (#168).
#[tokio::test]
async fn phrases_through_common_word_pairs_match_the_reference_backend() {
    let Some(qw) = quickwit() else {
        eprintln!("QUICKWIT_URL not set; skipping");
        return;
    };
    let mem = memory();
    for (analyzer, indexes) in [(Analyzer::V2, INDEXES), (Analyzer::V1, INDEXES_V1)] {
        let ids: Vec<String> = indexes.iter().map(|s| (*s).to_owned()).collect();
        let mut matched = 0;
        // LoC's text alone, then both texts through both texts' pairs (05 §5.5.4).
        for american_stories in [false, true] {
            let set = IndexSet::new(ids.clone())
                .with_analyzer(analyzer)
                .with_american_stories(american_stories);
            let grams = set.clone().with_common_grams(true);
            matched += common_word_phrases(&qw, &mem, &set, &grams).await;
        }
        assert!(
            matched >= 10,
            "only {matched} phrases matched the fixtures ({analyzer:?})"
        );
    }
}

/// How many of the phrases matched: Quickwit through the pairs (`grams`)
/// against the memory backend on the same texts (`set`), parsed with the
/// sets' analyzer.
async fn common_word_phrases(
    qw: &QuickwitBackend,
    mem: &MemoryBackend,
    set: &IndexSet,
    grams: &IndexSet,
) -> usize {
    let phrases = [
        r#""cross of gold""#,
        r#""the friends of free""#,
        r#""of the railroad""#,
        r#""friends of free silver""#,
        r#""of the the cotton""#,
        r#""the cross of gold""#,
        r#""of the gold of""#,
        r#""cross of the gold""#,
        r#""cross of gold" -silver"#,
        r#""cross of gold" OR "the friends of free""#,
        // Fractions are one word in the pairs as in `text` from version 2
        // (#168). Version 1 folds `½` to `1⁄2` in the pairs and the query,
        // and `text` reads that as the phrase "1 2", as the memory backend
        // does.
        r#""½ higher at 61¼""#,
        r#""at 61¼ and oats""#,
        r#""1/2 lower at 20¾""#,
        // So are `㊀` and `B㊁` from version 2 (#241). Version 1 has `一` and
        // `b二` in the pairs, but `text` has neither.
        r#""lots ㊀ and b㊁""#,
        // In American Stories' headlines only.
        r#""orator of the platte""#,
        r#""the boy orator""#,
        r#""quarantine at the port""#,
    ];
    let f = filters("1895-01-01", "1897-12-31");
    let spec = BucketSpec::new(BucketUnit::Month, f.from, f.to);
    let all: Vec<u8> = (0..8).collect();
    let mut matched = 0;
    for p in phrases {
        let q = parse_with(p, set.analyzer()).unwrap();
        let ctx = format!(
            "{p} / american stories {} / {:?}",
            set.american_stories(),
            set.analyzer()
        );
        let want = sorted(mem.summary(set, &q, &f, &spec).await.unwrap());
        let got = sorted(qw.summary(grams, &q, &f, &spec).await.expect(&ctx));
        assert_eq!(got, want, "summary: {ctx}");
        if want.total_hits > 0 {
            matched += 1;
        }
        let want = sorted_cells(mem.cube(set, &q, &f, &spec, &all).await.unwrap());
        let got = sorted_cells(qw.cube(grams, &q, &f, &spec, &all).await.expect(&ctx));
        assert_eq!(got, want, "cube: {ctx}");
        // The pages only American Stories' text matches, through the pairs.
        if set.american_stories() {
            let want = mem.american_stories_only(set, &q, &f).await.unwrap();
            let got = qw.american_stories_only(grams, &q, &f).await.expect(&ctx);
            assert_eq!(got, want, "american stories only: {ctx}");
        }
        for sort in [HitSort::Oldest, HitSort::Newest] {
            let page = HitsQuery {
                sort,
                limit: 20,
                ..HitsQuery::default()
            };
            let want = mem.hits(set, &q, &f, &page).await.unwrap();
            let got = qw.hits(grams, &q, &f, &page).await.expect(&ctx);
            assert_eq!(got.total, want.total, "hits total: {ctx}");
            let ids = |h: &usnm_search::HitsPage| -> Vec<String> {
                h.hits.iter().map(|h| h.doc_id.clone()).collect()
            };
            assert_eq!(ids(&got), ids(&want), "hits: {ctx}");
            // The snippets still mark the phrase's words in `text`, or in
            // `text_as` when only it has the phrase.
            assert_eq!(
                got.hits.iter().map(|h| &h.snippets).collect::<Vec<_>>(),
                want.hits.iter().map(|h| &h.snippets).collect::<Vec<_>>(),
                "snippets: {ctx}"
            );
            assert_eq!(
                got.hits.iter().map(|h| &h.matched_in).collect::<Vec<_>>(),
                want.hits.iter().map(|h| &h.matched_in).collect::<Vec<_>>(),
                "matched_in: {ctx}"
            );
            if want.total > 0 {
                assert!(
                    got.hits
                        .iter()
                        .any(|h| h.snippets.iter().any(|s| s.contains("<mark>"))),
                    "no highlights: {ctx}"
                );
            }
        }
    }
    matched
}

/// The searcher's metrics have the names and labels the API reads (#125,
/// #251): after a search over both indexes, each split's footer is cached,
/// the search pool has run, and the main runtime has threads.
#[tokio::test]
async fn searcher_metrics_report_the_split_footers_and_the_search_pool() {
    let Some(qw) = quickwit() else {
        eprintln!("QUICKWIT_URL not set; skipping");
        return;
    };
    use usnm_search::cache_metrics::Cache;
    use usnm_search::thread_metrics::{Pool, Runtime};
    let set = IndexSet::new(INDEXES.iter().map(|s| (*s).to_owned()).collect());
    let f = filters("1890-01-01", "1899-12-31");
    let spec = BucketSpec::new(BucketUnit::Year, f.from, f.to);
    qw.summary(&set, &parse("gold").unwrap(), &f, &spec)
        .await
        .unwrap();
    let metrics = qw.searcher_metrics().await.unwrap();
    let report = &metrics.caches;
    let footers = report[&Cache::SplitFooter];
    // At least one split per index, each footer cached once.
    assert!(footers.items >= 2, "{report:?}");
    assert!(
        footers.bytes > 0 && footers.misses >= footers.items,
        "{report:?}"
    );
    // No assertion on evictions: the shared searcher's counter covers its whole
    // life, and Quickwit 0.9.1 also counts replacing a footer as an eviction.
    assert!(report.contains_key(&Cache::FastField), "{report:?}");
    // The search is over, so the pool's numbers are whatever other tests
    // run now; only that it is there.
    let threads = &metrics.threads;
    assert!(threads.pools.contains_key(&Pool::Search), "{threads:?}");
    assert!(
        threads
            .runtimes
            .get(&Runtime::Main)
            .is_some_and(|m| m.threads >= 1),
        "{threads:?}"
    );
}

// ------------------------------------------------- decade partitions (#123)

#[path = "../../../fixtures/decades.rs"]
mod decades;

/// The fixture base and delta laid out by decade (05 §5.5.5), their pages
/// moved over the 1830s, 1840s, 1860s and 1890s (`fixtures/decades.rs`):
/// the base partitioned by decade, the delta with decade tags, as a release
/// builds them (scripts/quickwit-fixtures.sh loads them).
const DECADE_INDEXES: [&str; 2] = [
    "pages-base-fixture-decades",
    "pages-delta-fixture-1-decades",
];

/// The decades the moved pages span.
const DECADE_SPAN: std::ops::RangeInclusive<u16> = 1830..=1890;

/// The reference: the same moved pages, searched without the decade clause.
fn decade_memory() -> MemoryBackend {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/data/indexes");
    let mut m = MemoryBackend::new();
    for (from, to) in INDEXES.iter().zip(DECADE_INDEXES) {
        let file = std::fs::File::open(dir.join(format!("{from}.jsonl"))).expect("fixture");
        let docs: Vec<PageDoc> = std::io::BufReader::new(file)
            .lines()
            .map(|l| {
                let mut doc: serde_json::Value = serde_json::from_str(&l.expect("line")).unwrap();
                decades::shift(&mut doc);
                serde_json::from_value(doc).expect("doc")
            })
            .collect();
        m.add_index(to, docs);
    }
    m
}

/// The laid-out indexes, in LoC's text and in both texts, naming their
/// decades as the API does for a version with `decades`.
fn decade_sets() -> Vec<(&'static str, IndexSet)> {
    let loc = IndexSet::new(DECADE_INDEXES.iter().map(|s| (*s).to_owned()).collect())
        .with_decades(Some(DECADE_SPAN));
    let both = loc.clone().with_american_stories(true);
    vec![("loc", loc), ("both texts", both)]
}

/// Date ranges over one decade, several, all of them, and none.
fn decade_filter_cases() -> Vec<(&'static str, Filters)> {
    let mut states = filters("1835-01-01", "1869-12-31");
    states.states = vec!["GA".into(), "SC".into(), "IL".into()];
    vec![
        ("1868", filters("1868-01-01", "1868-12-31")),
        ("1839 to 1841", filters("1839-06-01", "1841-06-30")),
        ("1860 to 1899", filters("1860-01-01", "1899-12-31")),
        ("1835 to 1869 in three states", states),
        ("every decade", filters("1830-01-01", "1897-12-31")),
        ("no pages", filters("1900-01-01", "1910-12-31")),
    ]
}

/// A search over some of a version's decades names them, and Quickwit
/// returns exactly the reference's pages: the decade clause only skips
/// splits, the `day` filter decides.
#[tokio::test]
async fn searches_by_decade_match_the_reference_backend() {
    let Some(qw) = quickwit() else {
        eprintln!("QUICKWIT_URL not set; skipping");
        return;
    };
    let mem = decade_memory();
    let mut named = 0;
    let mut matched = 0;
    for ((sname, set), (qname, q)) in decade_sets()
        .into_iter()
        .flat_map(|s| queries().into_iter().map(move |q| (s.clone(), q)))
    {
        let set = &set;
        for (fname, f) in decade_filter_cases() {
            let ctx = format!("{sname} / {qname} / {fname}");
            let clause = usnm_search::quickwit::decade_clause(&f, set);
            assert_eq!(
                clause.is_none(),
                fname == "every decade",
                "{ctx}: {clause:?}"
            );
            named += u32::from(clause.is_some());
            let spec = BucketSpec::new(BucketUnit::Month, f.from, f.to);
            let want = sorted(mem.summary(set, &q, &f, &spec).await.unwrap());
            let got = sorted(qw.summary(set, &q, &f, &spec).await.expect(&ctx));
            // Days with a match are a HyperLogLog estimate in Quickwit,
            // merged over more splits here than in the other tests: within
            // 2%, not exact.
            assert!(
                close(got.days, want.days),
                "days: {ctx}: {} {}",
                got.days,
                want.days
            );
            assert_eq!(
                Summary { days: 0, ..got },
                Summary {
                    days: 0,
                    ..want.clone()
                },
                "summary: {ctx}"
            );
            matched += want.total_hits;

            let all: Vec<u8> = (0..8).collect();
            let want = sorted_cells(mem.cube(set, &q, &f, &spec, &all).await.unwrap());
            let got = sorted_cells(qw.cube(set, &q, &f, &spec, &all).await.expect(&ctx));
            assert_eq!(got, want, "cube: {ctx}");

            for sort in [HitSort::Oldest, HitSort::Newest] {
                let page = HitsQuery {
                    limit: 9,
                    sort,
                    days: true,
                    ..Default::default()
                };
                let want = mem.hits(set, &q, &f, &page).await.unwrap();
                let got = qw.hits(set, &q, &f, &page).await.expect(&ctx);
                assert_eq!(got.total, want.total, "hits total: {ctx}");
                assert!(close(got.days.unwrap(), want.days.unwrap()), "days: {ctx}");
                let ids = |p: &usnm_search::HitsPage| {
                    p.hits.iter().map(|h| h.doc_id.clone()).collect::<Vec<_>>()
                };
                assert_eq!(ids(&got), ids(&want), "hits: {ctx}");
            }
            if set.american_stories() {
                let want = mem.american_stories_only(set, &q, &f).await.unwrap();
                let got = qw.american_stories_only(set, &q, &f).await.expect(&ctx);
                assert_eq!(got, want, "american stories only: {ctx}");
            }
        }
    }
    assert!(named > 100, "only {named} searches named their decades");
    assert!(matched > 0, "no search matched a page");
}

/// A HyperLogLog estimate of `want`: within 2% (at least 1).
fn close(got: u64, want: u64) -> bool {
    got.abs_diff(want) <= (want / 50).max(1)
}

/// The searcher's count of splits a root search targets, after pruning
/// (`quickwit_search_root_search_targeted_splits`), for the one search `f`
/// makes. Other tests search the same node at the same time, so it retries,
/// for up to two minutes, until no other search was counted in between.
async fn targeted_splits<F, Fut>(url: &str, f: F) -> u64
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let http = reqwest::Client::new();
    let read = || {
        let http = http.clone();
        async move {
            let text = http
                .get(format!("{url}/metrics"))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap();
            let value = |name: &str| -> f64 {
                text.lines()
                    .find_map(|l| l.strip_prefix(&format!("{name}{{status=\"success\"}} ")))
                    .map_or(0.0, |v| v.trim().parse().unwrap())
            };
            (
                value("quickwit_search_root_search_targeted_splits_count"),
                value("quickwit_search_root_search_targeted_splits_sum"),
            )
        }
    };
    for _ in 0..600 {
        let (count, sum) = read().await;
        f().await;
        let (count2, sum2) = read().await;
        if count2 - count == 1.0 {
            return (sum2 - sum) as u64;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("other searches kept running between the metric reads");
}

/// Each split of the partitioned base holds one decade, the delta's split
/// lists its decades, and a search names only the splits of its decades:
/// the pruning #123 is for.
#[tokio::test]
async fn searches_by_decade_open_only_their_decades_splits() {
    let Some(qw) = quickwit() else {
        eprintln!("QUICKWIT_URL not set; skipping");
        return;
    };
    let url = std::env::var("QUICKWIT_URL").unwrap();
    let url = url.trim_end_matches('/');
    let http = reqwest::Client::new();
    // Each index's splits, as the decades in their tags.
    let mut splits: Vec<(&str, Vec<u16>)> = Vec::new();
    for id in DECADE_INDEXES {
        let v: serde_json::Value = http
            .get(format!(
                "{url}/api/v1/indexes/{id}/splits?split_states=Published"
            ))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        for s in v["splits"].as_array().unwrap() {
            let tags: Vec<&str> = s["tags"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t.as_str().unwrap())
                .collect();
            assert!(tags.contains(&"decade!"), "{id}: {tags:?}");
            let decades: Vec<u16> = tags
                .iter()
                .filter_map(|t| t.strip_prefix("decade:")?.parse().ok())
                .collect();
            splits.push((id, decades));
        }
    }
    let base: Vec<&Vec<u16>> = splits
        .iter()
        .filter(|(id, _)| *id == DECADE_INDEXES[0])
        .map(|(_, d)| d)
        .collect();
    let mut base_decades: Vec<u16> = base.iter().flat_map(|d| d.iter().copied()).collect();
    base_decades.sort_unstable();
    assert!(
        base.iter().all(|d| d.len() == 1),
        "a partitioned split holds one decade: {base:?}"
    );
    assert_eq!(base_decades, [1830, 1840, 1860, 1890]);
    assert!(
        splits
            .iter()
            .any(|(id, d)| *id == DECADE_INDEXES[1] && d.len() > 1),
        "the delta's split is tagged with its decades, not partitioned: {splits:?}"
    );

    let [(_, set), _] = <[_; 2]>::try_from(decade_sets()).ok().unwrap();
    let plain = IndexSet::new(set.ids().to_vec());
    let q = parse("gold").unwrap();
    let opened = |set: IndexSet, f: Filters| {
        let qw = &qw;
        let q = &q;
        async move {
            targeted_splits(url, || {
                let (set, f) = (set.clone(), f.clone());
                async move {
                    let spec = BucketSpec::new(BucketUnit::Year, f.from, f.to);
                    qw.summary(&set, q, &f, &spec).await.unwrap();
                }
            })
            .await
        }
    };
    let holding = |decade: u16| splits.iter().filter(|(_, d)| d.contains(&decade)).count() as u64;
    let f1868 = filters("1868-01-01", "1868-12-31");
    assert_eq!(opened(set.clone(), f1868.clone()).await, holding(1860));
    assert_eq!(opened(plain.clone(), f1868).await, splits.len() as u64);
    assert!(holding(1860) < splits.len() as u64);
    // Two decades: the splits holding either.
    let either = splits
        .iter()
        .filter(|(_, d)| d.contains(&1830) || d.contains(&1840))
        .count() as u64;
    assert_eq!(
        opened(set.clone(), filters("1839-06-01", "1840-06-30")).await,
        either
    );
    // Every decade: no clause, every split.
    assert_eq!(
        opened(set.clone(), filters("1830-01-01", "1897-12-31")).await,
        splits.len() as u64
    );
    // A range with no pages: none.
    assert_eq!(opened(set, filters("1900-01-01", "1910-12-31")).await, 0);
}

#[tokio::test]
async fn congress_searches_match_the_reference_backend() {
    let Some(qw) = quickwit() else {
        eprintln!("QUICKWIT_URL not set; skipping");
        return;
    };
    let mem = memory();
    let plain = IndexSet::new(vec![CONGRESS_INDEX.to_owned()]);
    // As the API searches it: phrases with common words go to `text_cg`.
    let grams = plain.clone().with_common_grams(true);
    let all = filters("1789-01-01", "1994-12-31");
    let mut globe = all.clone();
    globe.series = vec!["globe".into()];
    let mut two = filters("1820-01-01", "1850-12-31");
    two.series = vec!["annals".into(), "register".into()];
    let queries: Vec<(&str, Node)> = vec![
        ("word", parse("tariff").unwrap()),
        ("phrase", parse("\"fugitive slave\"").unwrap()),
        (
            "common-word phrase",
            parse("\"admission of california\"").unwrap(),
        ),
        ("and", parse("gold silver").unwrap()),
        ("or", parse("missouri OR nullification").unwrap()),
        ("not", parse("senate -california").unwrap()),
        ("prefix", parse("nullif*").unwrap()),
        (
            "near",
            build("free standard", Some(Mode::Near), 4, 0).unwrap(),
        ),
    ];
    let mut nonzero = 0;
    for (sname, set) in [("plain", &plain), ("pairs", &grams)] {
        for (qname, q) in &queries {
            for (fname, f) in [("all", &all), ("globe", &globe), ("annals+register", &two)] {
                for spec in specs(f) {
                    let mut f = f.clone();
                    f.from = spec.from;
                    f.to = spec.to;
                    let ctx = format!("{sname} / {qname} / {fname} / {:?}", spec.unit);
                    let want = sorted(mem.summary(set, q, &f, &spec).await.unwrap());
                    let got = sorted(qw.summary(set, q, &f, &spec).await.expect(&ctx));
                    assert_eq!(got, want, "summary: {ctx}");
                    nonzero += usize::from(want.total_hits > 0);
                    let shards: Vec<u8> = (0..8).collect();
                    let want = sorted_cells(mem.cube(set, q, &f, &spec, &shards).await.unwrap());
                    let got = sorted_cells(qw.cube(set, q, &f, &spec, &shards).await.expect(&ctx));
                    assert_eq!(got, want, "cube: {ctx}");
                }
                let page = HitsQuery {
                    limit: 10,
                    ..Default::default()
                };
                let ctx = format!("{sname} / {qname} / {fname}");
                let want = mem.hits(set, q, f, &page).await.unwrap();
                let got = qw.hits(set, q, f, &page).await.expect(&ctx);
                assert_eq!(got.total, want.total, "hits total: {ctx}");
                let key = |h: &usnm_search::Hit| {
                    (
                        h.doc_id.clone(),
                        h.snippets.clone(),
                        h.congress.clone(),
                        h.front_page,
                        h.seq,
                        h.place_id.clone(),
                    )
                };
                assert_eq!(
                    got.hits.iter().map(key).collect::<Vec<_>>(),
                    want.hits.iter().map(key).collect::<Vec<_>>(),
                    "hits: {ctx}"
                );
            }
        }
    }
    assert!(nonzero > 20, "only {nonzero} Congress searches matched");
}
