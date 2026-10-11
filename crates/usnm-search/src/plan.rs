//! Aggregate query planning (05 §5.7): one summary call, then a cube call that
//! is sharded by `place_shard` when it would exceed the engine's bucket limit.
//! The cube is only issued when its upper bound (places with hits × buckets)
//! fits the caller's cell budget, so an oversized request never reaches the
//! engine's aggregation limit. Alongside the cube, two one-hit queries find
//! the earliest and latest matching page, and when the search covers American
//! Stories' text, a count of the pages only that text matches (05 §5.5.4).
//!
//! [`days`] plans `/v1/days`: matching pages per day for a few places.

use std::collections::HashMap;

use futures::stream::{self, StreamExt, TryStreamExt};
use usnm_core::cube::{shard_count, shards_for, PLACE_SHARDS, SHARD_TARGET_BUCKETS};
use usnm_core::params::Filters;
use usnm_core::query::Node;
use usnm_core::time::{day_number, BucketSpec, BucketUnit};

use crate::{CubeCell, Hit, HitSort, HitsQuery, IndexSet, SearchBackend, SearchError, Summary};

/// Cube sub-queries run at most this many at a time.
pub const SHARD_CONCURRENCY: usize = 2;

/// Largest cube one request can compute: every shard at its bucket target.
pub const MAX_SHARDED_BUCKETS: usize = PLACE_SHARDS as usize * SHARD_TARGET_BUCKETS;

pub struct Aggregate {
    pub summary: Summary,
    pub cells: Vec<CubeCell>,
    /// Number of cube calls made (1 = unsharded).
    pub cube_calls: u8,
    /// The earliest and latest matching page; `None` when nothing matches.
    pub first: Option<Hit>,
    pub last: Option<Hit>,
    /// When the search covers American Stories' text: how many of the
    /// matching pages match in it but not in LoC's text (05 §5.5.4).
    pub american_stories_only: Option<u64>,
}

pub enum Planned {
    Complete(Box<Aggregate>),
    /// The cube's upper bound exceeds the budget, so no cube query was issued;
    /// the caller should retry with coarser buckets or reject the request.
    TooManyCells {
        upper_bound: usize,
    },
}

pub async fn aggregate(
    backend: &dyn SearchBackend,
    indexes: &IndexSet,
    query: &Node,
    filters: &Filters,
    spec: &BucketSpec,
    max_cells: usize,
) -> Result<Planned, SearchError> {
    let summary = backend.summary(indexes, query, filters, spec).await?;
    if summary.places.is_empty() {
        return Ok(Planned::Complete(Box::new(Aggregate {
            summary,
            cells: Vec::new(),
            cube_calls: 0,
            first: None,
            last: None,
            american_stories_only: indexes.american_stories().then_some(0),
        })));
    }
    let upper_bound = summary.places.len().saturating_mul(spec.len());
    if upper_bound > max_cells.min(MAX_SHARDED_BUCKETS) {
        return Ok(Planned::TooManyCells { upper_bound });
    }
    let n = shard_count(summary.places.len(), spec.len());
    let cube = stream::iter(0..n)
        .map(|k| {
            let shards = shards_for(k, n);
            async move { backend.cube(indexes, query, filters, spec, &shards).await }
        })
        .buffer_unordered(SHARD_CONCURRENCY)
        .try_collect::<Vec<Vec<CubeCell>>>();
    let edges = futures::future::try_join(
        edge(backend, indexes, query, filters, HitSort::Oldest),
        edge(backend, indexes, query, filters, HitSort::Newest),
    );
    // One count-only query.
    let american_stories_only = async {
        if !indexes.american_stories() {
            return Ok(None);
        }
        backend
            .american_stories_only(indexes, query, filters)
            .await
            .map(Some)
    };
    let (parts, (first, last), american_stories_only) =
        futures::future::try_join3(cube, edges, american_stories_only).await?;
    Ok(Planned::Complete(Box::new(Aggregate {
        summary,
        cells: parts.into_iter().flatten().collect(),
        cube_calls: n,
        first,
        last,
        american_stories_only,
    })))
}

/// The first hit in `sort` order: the earliest or latest matching page.
async fn edge(
    backend: &dyn SearchBackend,
    indexes: &IndexSet,
    query: &Node,
    filters: &Filters,
    sort: HitSort,
) -> Result<Option<Hit>, SearchError> {
    let page = HitsQuery {
        sort,
        limit: 1,
        ..HitsQuery::default()
    };
    let hits = backend.hits(indexes, query, filters, &page).await?;
    Ok(hits.hits.into_iter().next())
}

/// One place's matching pages per day: `days` ascending (day numbers, as in
/// the index), `hits[i]` pages on `days[i]`, only days with a match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaceDays {
    pub place_id: String,
    pub days: Vec<u32>,
    pub hits: Vec<u32>,
}

pub enum PlannedDays {
    /// One entry per requested place, in the order requested.
    Complete(Vec<PlaceDays>),
    /// More (place, day) cells than the budget could hold, so no day query
    /// was issued.
    TooManyCells { upper_bound: usize },
}

/// Matching pages per day for `places` (distinct, validated ids). A cube by
/// year comes first: each place-year can hold at most as many day cells as
/// it has pages, and as the year has days in the range, which bounds the
/// day cube. Over `max_cells` nothing more is asked. Otherwise the places
/// are grouped into day cubes of at most [`SHARD_TARGET_BUCKETS`] cells
/// each (a place over that alone gets its own), run two at a time.
pub async fn days(
    backend: &dyn SearchBackend,
    indexes: &IndexSet,
    query: &Node,
    filters: &Filters,
    places: &[String],
    max_cells: usize,
) -> Result<PlannedDays, SearchError> {
    let (from, to) = (filters.from, filters.to);
    let years = BucketSpec::new(BucketUnit::Year, from, to);
    let mut bound: HashMap<&str, usize> = HashMap::new();
    for c in backend
        .place_cube(indexes, query, filters, &years, places)
        .await?
    {
        let i = c.bucket as usize;
        let start = years.bucket_start(i);
        let end = if i + 1 < years.len() {
            years.bucket_start(i + 1).pred_opt().unwrap_or(to)
        } else {
            to
        };
        let days_in_range = (end - start).num_days().max(0) as usize + 1;
        if let Some(id) = places.iter().find(|p| **p == c.place_id) {
            *bound.entry(id.as_str()).or_default() += (c.hits as usize).min(days_in_range);
        }
    }
    let upper_bound: usize = bound.values().sum();
    if upper_bound > max_cells {
        return Ok(PlannedDays::TooManyCells { upper_bound });
    }
    // Places in the order asked, packed into calls of at most the target.
    let mut groups: Vec<(Vec<String>, usize)> = Vec::new();
    for p in places {
        let Some(&n) = bound.get(p.as_str()).filter(|n| **n > 0) else {
            continue;
        };
        match groups.last_mut() {
            Some((ids, cells)) if *cells + n <= SHARD_TARGET_BUCKETS => {
                ids.push(p.clone());
                *cells += n;
            }
            _ => groups.push((vec![p.clone()], n)),
        }
    }
    let spec = BucketSpec::new(BucketUnit::Day, from, to);
    let parts = stream::iter(groups)
        .map(|(ids, _)| {
            let spec = &spec;
            async move {
                backend
                    .place_cube(indexes, query, filters, spec, &ids)
                    .await
            }
        })
        .buffer_unordered(SHARD_CONCURRENCY)
        .try_collect::<Vec<Vec<CubeCell>>>()
        .await?;
    let origin = day_number(from);
    let mut by_place: HashMap<String, Vec<(u32, u32)>> = HashMap::new();
    for c in parts.into_iter().flatten() {
        by_place
            .entry(c.place_id)
            .or_default()
            .push((origin + c.bucket, c.hits));
    }
    Ok(PlannedDays::Complete(
        places
            .iter()
            .map(|p| {
                // Each place is in one call, so each day comes once.
                let mut cells = by_place.remove(p).unwrap_or_default();
                cells.sort_unstable();
                PlaceDays {
                    place_id: p.clone(),
                    days: cells.iter().map(|c| c.0).collect(),
                    hits: cells.iter().map(|c| c.1).collect(),
                }
            })
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryBackend;
    use crate::PageDoc;
    use usnm_core::time::date_from_day;

    fn doc(i: u32, day: u32, place: u8) -> PageDoc {
        PageDoc {
            printed: None,
            ocr_source: None,
            ocr_engine: None,
            text_as: None,
            congress: Default::default(),
            doc_id: format!("sn99{i:06}_x_ed-1_seq-1"),
            day,
            ym: 0,
            year: 0,
            place_id: format!("P{place:05}"),
            place_shard: place % 8,
            lccn: format!("sn99{place:06}"),
            state: "GA".into(),
            language: vec!["eng".into()],
            front_page: true,
            edition: 1,
            seq: 1,
            sort_key: 0,
            batch: "b".into(),
            text: if i.is_multiple_of(3) {
                "silver"
            } else {
                "gold"
            }
            .into(),
        }
    }

    fn filters(from: u32, to: u32) -> Filters {
        Filters {
            from: date_from_day(from),
            to: date_from_day(to),
            states: vec![],
            lccns: vec![],
            langs: vec![],
            front_only: false,
            series: Vec::new(),
        }
    }

    /// Pages on days 71,000–71,799 in places 1–4, several a day.
    fn backend() -> MemoryBackend {
        let mut b = MemoryBackend::new();
        b.add_index(
            "i",
            (0..2_400).map(|i| doc(i, 71_000 + (i * 7) % 800, (i % 4) as u8 + 1)),
        );
        b
    }

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    /// With American Stories' text searched, the aggregate also counts the
    /// pages only that text matches; without it, it asks nothing more.
    #[tokio::test]
    async fn aggregate_counts_pages_only_american_stories_text_matches() {
        let mut b = MemoryBackend::new();
        b.add_index(
            "i",
            (0..30).map(|i| {
                let mut d = doc(i, 71_000 + i, (i % 3) as u8 + 1);
                // Every other page: American Stories reads `gold` where LoC has `goid`.
                if i % 2 == 0 {
                    d.text = "goid".into();
                    d.text_as = Some("gold".into());
                }
                d
            }),
        );
        let q = usnm_core::query::parse("gold").unwrap();
        let f = filters(71_000, 71_100);
        let spec = BucketSpec::new(BucketUnit::Year, f.from, f.to);
        let run = |set: IndexSet, q: Node| {
            let (b, f, spec) = (&b, &f, &spec);
            async move {
                match aggregate(b, &set, &q, f, spec, 700_000).await.unwrap() {
                    Planned::Complete(a) => a,
                    Planned::TooManyCells { .. } => panic!("over budget"),
                }
            }
        };
        let off = IndexSet::new(vec!["i".into()]);
        let on = off.clone().with_american_stories(true);
        let a = run(on.clone(), q.clone()).await;
        // Odd pages not divisible by 3 have `gold` in LoC's text: 10 of them;
        // the 15 even pages have it in American Stories' only.
        assert_eq!(
            (a.summary.total_hits, a.american_stories_only),
            (25, Some(15))
        );
        let a = run(off, q.clone()).await;
        assert_eq!((a.summary.total_hits, a.american_stories_only), (10, None));
        // Nothing matches: nothing found only in American Stories' text.
        let none = usnm_core::query::parse("zyzzyva").unwrap();
        assert_eq!(run(on, none).await.american_stories_only, Some(0));
    }

    #[tokio::test]
    async fn days_match_brute_force_in_the_order_asked() {
        let b = backend();
        let idx = IndexSet::new(vec!["i".into()]);
        let q = usnm_core::query::parse("gold").unwrap();
        let f = filters(71_100, 71_650);
        let asked = ids(&["P00003", "P00009", "P00001"]);
        let PlannedDays::Complete(out) = days(&b, &idx, &q, &f, &asked, 700_000).await.unwrap()
        else {
            panic!("over budget")
        };
        assert_eq!(
            out.iter().map(|p| p.place_id.as_str()).collect::<Vec<_>>(),
            ["P00003", "P00009", "P00001"]
        );
        // A place with no matches has empty lists.
        assert!(out[1].days.is_empty() && out[1].hits.is_empty());
        for p in [&out[0], &out[2]] {
            let mut expected: std::collections::BTreeMap<u32, u32> = Default::default();
            for i in 0..2_400u32 {
                let d = doc(i, 71_000 + (i * 7) % 800, (i % 4) as u8 + 1);
                if d.place_id == p.place_id
                    && d.text == "gold"
                    && (71_100..=71_650).contains(&d.day)
                {
                    *expected.entry(d.day).or_default() += 1;
                }
            }
            assert!(!expected.is_empty());
            assert_eq!(p.days, expected.keys().copied().collect::<Vec<_>>());
            assert_eq!(p.hits, expected.values().copied().collect::<Vec<_>>());
        }
    }

    #[tokio::test]
    async fn too_many_day_cells_are_refused_before_the_day_query() {
        let b = backend();
        let idx = IndexSet::new(vec!["i".into()]);
        let q = usnm_core::query::parse("gold").unwrap();
        let f = filters(71_000, 71_799);
        let asked = ids(&["P00001", "P00002"]);
        // Each place has 400 matching pages on 200 days. No year in range
        // has fewer days than pages, so the bound is the pages: 800.
        match days(&b, &idx, &q, &f, &asked, 799).await.unwrap() {
            PlannedDays::TooManyCells { upper_bound } => assert_eq!(upper_bound, 800),
            PlannedDays::Complete(_) => panic!("within budget"),
        }
        let PlannedDays::Complete(out) = days(&b, &idx, &q, &f, &asked, 800).await.unwrap() else {
            panic!("over budget")
        };
        assert_eq!(out[0].days.len(), 200);
        assert_eq!(out[0].hits.iter().sum::<u32>(), 400);
        // Over one day, P00001's two pages there make one cell, not two.
        let one_day = filters(71_000, 71_000);
        match days(&b, &idx, &q, &one_day, &asked, 0).await.unwrap() {
            PlannedDays::TooManyCells { upper_bound } => assert_eq!(upper_bound, 1),
            PlannedDays::Complete(_) => panic!("within budget"),
        }
    }

    /// Places with synthetic yearly counts big enough to need several day
    /// calls; records which places each day call asked for.
    struct Packing {
        calls: std::sync::Mutex<Vec<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl SearchBackend for Packing {
        fn capabilities(&self) -> crate::Capabilities {
            crate::Capabilities {
                fuzzy: false,
                max_slop: 0,
                nested_aggregations: true,
            }
        }
        async fn summary(
            &self,
            _: &IndexSet,
            _: &Node,
            _: &Filters,
            _: &BucketSpec,
        ) -> Result<Summary, SearchError> {
            unreachable!("days asks for no summary")
        }
        async fn cube(
            &self,
            _: &IndexSet,
            _: &Node,
            _: &Filters,
            _: &BucketSpec,
            _: &[u8],
        ) -> Result<Vec<CubeCell>, SearchError> {
            unreachable!("days asks for no sharded cube")
        }
        async fn place_cube(
            &self,
            _: &IndexSet,
            _: &Node,
            _: &Filters,
            spec: &BucketSpec,
            places: &[String],
        ) -> Result<Vec<CubeCell>, SearchError> {
            let cell = |p: &str, bucket: u32, hits: u32| CubeCell {
                place_id: p.to_owned(),
                bucket,
                hits,
            };
            if spec.unit == BucketUnit::Year {
                // A, B and C have pages every day of every year (about
                // 110,000 day cells each); D has 10 in one year; E none.
                let mut cells = Vec::new();
                for p in places {
                    match p.as_str() {
                        "A" | "B" | "C" => {
                            cells.extend((0..spec.len() as u32).map(|y| cell(p, y, 1_000)))
                        }
                        "D" => cells.push(cell(p, 3, 10)),
                        _ => {}
                    }
                }
                return Ok(cells);
            }
            self.calls.lock().unwrap().push(places.to_vec());
            // Out of day order, to check the merge sorts each place's days.
            Ok(places
                .iter()
                .flat_map(|p| [cell(p, 2, 5), cell(p, 0, 1)])
                .collect())
        }
        async fn hits(
            &self,
            _: &IndexSet,
            _: &Node,
            _: &Filters,
            _: &HitsQuery,
        ) -> Result<crate::HitsPage, SearchError> {
            unreachable!("days asks for no hits")
        }
        async fn health(&self) -> Result<(), SearchError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn days_pack_big_places_into_separate_calls_and_keep_the_order_asked() {
        let b = Packing {
            calls: std::sync::Mutex::new(Vec::new()),
        };
        let idx = IndexSet::new(vec!["i".into()]);
        let q = usnm_core::query::parse("gold").unwrap();
        let from =
            usnm_core::time::day_number(chrono::NaiveDate::from_ymd_opt(1700, 1, 1).unwrap());
        let to =
            usnm_core::time::day_number(chrono::NaiveDate::from_ymd_opt(2000, 12, 31).unwrap());
        let f = filters(from, to);
        let asked = ids(&["A", "E", "B", "C", "D"]);
        let PlannedDays::Complete(days) = days(&b, &idx, &q, &f, &asked, 1_000_000).await.unwrap()
        else {
            panic!("within the budget")
        };
        // Over 150,000 cells together, so A, B and C each get a call; D
        // (10 cells) joins C's; E has no pages and is never asked for.
        let mut calls = b.calls.lock().unwrap().clone();
        calls.sort();
        assert_eq!(calls, vec![ids(&["A"]), ids(&["B"]), ids(&["C", "D"])]);
        let order: Vec<&str> = days.iter().map(|d| d.place_id.as_str()).collect();
        assert_eq!(order, ["A", "E", "B", "C", "D"]);
        for d in &days {
            if d.place_id == "E" {
                assert!(d.days.is_empty() && d.hits.is_empty());
            } else {
                assert_eq!(d.days, [from, from + 2], "{}", d.place_id);
                assert_eq!(d.hits, [1, 5], "{}", d.place_id);
            }
        }
    }
}
