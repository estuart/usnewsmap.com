//! Evidence for docs/design/11-term-geographic-skew.md: scores saved API
//! responses with `usnm_core::skew` and prints the tables the doc quotes.
//!
//! ```sh
//! cargo run -p usnm-core --example term_skew -- --dir DIR --names NAME,...
//! ```
//!
//! `DIR` holds `places.json` (`/v1/places`), `cov_year.json` (`/v1/coverage`
//! by year over the whole corpus) and, for each `NAME`, `agg_NAME.json`
//! (`/v1/aggregate`) and `cov_NAME.json` (the coverage its `baseline_ref`
//! names). The doc lists the exact requests.

use std::collections::HashMap;
use std::path::Path;

use serde_json::Value;
use usnm_core::skew::{dispersion_for, place_counts, score_all, Cell, Counts, Prior, Score};
use usnm_core::time::{BucketSpec, BucketUnit};

fn load(dir: &Path, name: &str) -> Value {
    let path = dir.join(name);
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_slice(&bytes).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn u64s(v: &Value) -> Vec<u64> {
    v.as_array()
        .expect("array")
        .iter()
        .map(|x| x.as_u64().expect("u64"))
        .collect()
}

fn strs(v: &Value) -> Vec<String> {
    v.as_array()
        .expect("array")
        .iter()
        .map(|x| x.as_str().expect("str").to_owned())
        .collect()
}

/// Ranks with ties averaged.
fn ranks(x: &[f64]) -> Vec<f64> {
    let mut idx: Vec<usize> = (0..x.len()).collect();
    idx.sort_by(|&a, &b| x[a].total_cmp(&x[b]));
    let mut r = vec![0.0; x.len()];
    let mut i = 0;
    while i < idx.len() {
        let mut j = i;
        while j + 1 < idx.len() && x[idx[j + 1]] == x[idx[i]] {
            j += 1;
        }
        let avg = (i + j) as f64 / 2.0 + 1.0;
        for k in &idx[i..=j] {
            r[*k] = avg;
        }
        i = j + 1;
    }
    r
}

fn pearson(x: &[f64], y: &[f64]) -> f64 {
    let n = x.len() as f64;
    let (mx, my) = (x.iter().sum::<f64>() / n, y.iter().sum::<f64>() / n);
    let (mut sxy, mut sxx, mut syy) = (0.0, 0.0, 0.0);
    for (a, b) in x.iter().zip(y) {
        sxy += (a - mx) * (b - my);
        sxx += (a - mx).powi(2);
        syy += (b - my).powi(2);
    }
    sxy / (sxx * syy).sqrt()
}

fn spearman(x: &[f64], y: &[f64]) -> f64 {
    pearson(&ranks(x), &ranks(y))
}

fn median(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(f64::total_cmp);
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

struct Place {
    name: String,
    state: String,
    titles: u64,
}

fn places(dir: &Path) -> HashMap<String, Place> {
    let v = load(dir, "places.json");
    v["features"]
        .as_array()
        .expect("features")
        .iter()
        .map(|f| {
            let p = &f["properties"];
            (
                f["id"].as_str().expect("id").to_owned(),
                Place {
                    name: p["name"].as_str().unwrap_or("?").to_owned(),
                    state: p["state"].as_str().unwrap_or("?").to_owned(),
                    titles: p["titles"].as_u64().unwrap_or(0),
                },
            )
        })
        .collect()
}

fn corpus(dir: &Path, info: &HashMap<String, Place>) {
    let cov = load(dir, "cov_year.json");
    let ids = strs(&cov["places"]);
    let (p, b, h) = (
        u64s(&cov["pages"]["p"]),
        u64s(&cov["pages"]["b"]),
        u64s(&cov["pages"]["h"]),
    );
    let years = cov["count"].as_u64().expect("count") as usize;
    let mut pages = vec![0u64; ids.len()];
    let mut per_year_places = vec![0usize; years];
    let mut per_year_states: Vec<std::collections::BTreeSet<&str>> =
        vec![Default::default(); years];
    for i in 0..h.len() {
        pages[p[i] as usize] += h[i];
        per_year_places[b[i] as usize] += 1;
        per_year_states[b[i] as usize].insert(info[&ids[p[i] as usize]].state.as_str());
    }
    let total: u64 = pages.iter().sum();
    let mut sorted = pages.clone();
    sorted.sort_unstable_by(|a, b| b.cmp(a));
    let top10: u64 = sorted[..10].iter().sum();
    println!(
        "## Corpus ({})",
        cov["index_version"].as_str().unwrap_or("?")
    );
    println!(
        "places {}  pages {}  median pages/place {:.0}  p90 {}  max {}  top 10 places hold {:.1}% of pages",
        ids.len(),
        total,
        median(sorted.iter().map(|&x| x as f64).collect()),
        sorted[sorted.len() / 10],
        sorted[0],
        100.0 * top10 as f64 / total as f64
    );
    let titles: Vec<f64> = ids.iter().map(|id| info[id].titles as f64).collect();
    let pagesf: Vec<f64> = pages.iter().map(|&x| x as f64).collect();
    let ppt: Vec<f64> = pagesf.iter().zip(&titles).map(|(p, t)| p / t).collect();
    let mut ppt_sorted = ppt.clone();
    ppt_sorted.sort_by(f64::total_cmp);
    let q = |f: f64| ppt_sorted[((ppt_sorted.len() - 1) as f64 * f) as usize];
    println!(
        "pages per title, by place: p10 {:.0}  median {:.0}  p90 {:.0}  min {:.0}  max {:.0}  (p90/p10 = {:.0}x)",
        q(0.1),
        q(0.5),
        q(0.9),
        ppt_sorted[0],
        ppt_sorted[ppt_sorted.len() - 1],
        q(0.9) / q(0.1)
    );
    println!(
        "spearman(pages, titles) across places = {:.3}  places with one title {}",
        spearman(&pagesf, &titles),
        titles.iter().filter(|&&t| t == 1.0).count()
    );
    let states: std::collections::BTreeSet<&str> =
        ids.iter().map(|id| info[id].state.as_str()).collect();
    println!("states and territories with pages {}", states.len());
    let thin: Vec<usize> = (0..years).filter(|&y| per_year_places[y] > 0).collect();
    let few = thin
        .iter()
        .filter(|&&y| per_year_states[y].len() < 5)
        .count();
    let first_wide = thin
        .iter()
        .find(|&&y| per_year_states[y].len() >= 5)
        .copied()
        .unwrap_or(0);
    let from_year: u64 = cov["from"].as_str().expect("from")[..4]
        .parse()
        .expect("year");
    println!(
        "years with pages {}  of which with pages from fewer than 5 states {}  first year with 5+ states {}",
        thin.len(),
        few,
        from_year + first_wide as u64
    );
    println!();
}

struct Row<'a> {
    id: &'a str,
    pages: u64,
    score: Score,
}

struct Scored {
    phi: f64,
    phi_places: usize,
    prior: Prior,
    scores: Vec<Score>,
}

fn score(
    spec: &BucketSpec,
    hits: &[u64],
    pages: &[u64],
    cells: &[Cell],
    places: usize,
    phi: Option<f64>,
) -> Scored {
    let counts = place_counts(hits, pages, cells, places);
    let (d, measured) = dispersion_for(spec, hits, pages, cells);
    let phi_used = phi.unwrap_or(measured);
    let (prior, scores) = score_all(&counts, phi_used, 0.9);
    Scored {
        phi: phi_used,
        phi_places: d.places,
        prior,
        scores,
    }
}

fn spec_of(agg: &Value) -> BucketSpec {
    let date = |k: &str| {
        chrono::NaiveDate::parse_from_str(agg["bucket"][k].as_str().expect("date"), "%Y-%m-%d")
            .expect("date")
    };
    let unit = BucketUnit::parse(agg["bucket"]["unit"].as_str().expect("unit")).expect("unit");
    BucketSpec::new(unit, date("from"), date("to"))
}

fn flags(scores: &[Score]) -> (usize, usize) {
    (
        scores.iter().filter(|s| s.direction() == 1).count(),
        scores.iter().filter(|s| s.direction() == -1).count(),
    )
}

fn changed(a: &[Score], b: &[Score]) -> usize {
    a.iter()
        .zip(b)
        .filter(|(x, y)| x.direction() != y.direction())
        .count()
}

/// Month buckets merged into calendar years (the same data at a coarser unit).
fn to_years(
    agg: &Value,
    hits: &[u64],
    pages: &[u64],
    cells: &[Cell],
) -> (Vec<u64>, Vec<u64>, Vec<Cell>) {
    let from = agg["bucket"]["from"].as_str().expect("from");
    let month0: usize = from[5..7].parse::<usize>().expect("month") - 1;
    let year = |b: usize| (month0 + b) / 12;
    let n = year(hits.len() - 1) + 1;
    let (mut h, mut p) = (vec![0u64; n], vec![0u64; n]);
    for (b, (&x, &y)) in hits.iter().zip(pages).enumerate() {
        h[year(b)] += x;
        p[year(b)] += y;
    }
    let mut merged: std::collections::BTreeMap<(usize, usize), (u64, u64)> = Default::default();
    for c in cells {
        let e = merged.entry((c.place, year(c.bucket))).or_default();
        e.0 += c.pages;
        e.1 += c.hits;
    }
    let cells = merged
        .into_iter()
        .map(|((place, bucket), (pages, hits))| Cell {
            place,
            bucket,
            pages,
            hits,
        })
        .collect();
    (h, p, cells)
}

fn term(dir: &Path, name: &str, info: &HashMap<String, Place>) {
    let agg = load(dir, &format!("agg_{name}.json"));
    let cov = load(dir, &format!("cov_{name}.json"));
    assert_eq!(agg["bucket"]["count"], cov["count"], "bucket counts differ");
    let buckets = cov["count"].as_u64().expect("count") as usize;
    let nat_hits = u64s(&agg["series"]["hits"]);
    let nat_pages = u64s(&agg["series"]["baseline"]);
    let total_hits: u64 = nat_hits.iter().sum();
    let total_pages: u64 = nat_pages.iter().sum();

    // Places in coverage order; hits per place and bucket from the cube.
    let ids = strs(&cov["places"]);
    let index: HashMap<&str, usize> = ids
        .iter()
        .enumerate()
        .map(|(i, s)| (s.as_str(), i))
        .collect();
    let agg_ids = strs(&agg["places"]["id"]);
    let (qp, qb, qh) = (
        u64s(&agg["cube"]["p"]),
        u64s(&agg["cube"]["b"]),
        u64s(&agg["cube"]["h"]),
    );
    let mut cube: HashMap<(usize, usize), u64> = HashMap::new();
    for i in 0..qh.len() {
        let id = agg_ids[qp[i] as usize].as_str();
        let place = *index
            .get(id)
            .unwrap_or_else(|| panic!("{id} has hits but no pages"));
        *cube.entry((place, qb[i] as usize)).or_default() += qh[i];
    }
    let (cp, cb, ch) = (
        u64s(&cov["pages"]["p"]),
        u64s(&cov["pages"]["b"]),
        u64s(&cov["pages"]["h"]),
    );
    let cells: Vec<Cell> = (0..ch.len())
        .map(|i| {
            let (place, bucket) = (cp[i] as usize, cb[i] as usize);
            Cell {
                place,
                bucket,
                pages: ch[i],
                hits: cube.remove(&(place, bucket)).unwrap_or(0),
            }
        })
        .collect();
    assert!(cube.is_empty(), "hits in cells without pages");
    // The national series is the sum of the places, bucket by bucket.
    let (mut sum_hits, mut sum_pages) = (vec![0u64; buckets], vec![0u64; buckets]);
    for c in &cells {
        sum_hits[c.bucket] += c.hits;
        sum_pages[c.bucket] += c.pages;
    }
    assert_eq!(
        sum_hits, nat_hits,
        "national hits differ from the places' sum"
    );
    assert_eq!(
        sum_pages, nat_pages,
        "national pages differ from the coverage sum"
    );
    let mut pages = vec![0u64; ids.len()];
    let mut observed_all = vec![0u64; ids.len()];
    for c in &cells {
        pages[c.place] += c.pages;
        observed_all[c.place] += c.hits;
    }

    let started = std::time::Instant::now();
    let spec = spec_of(&agg);
    let main = score(&spec, &nat_hits, &nat_pages, &cells, ids.len(), None);
    let scoring = started.elapsed();
    let scores = &main.scores;
    let rows: Vec<Row> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| Row {
            id,
            pages: pages[i],
            score: scores[i],
        })
        .collect();

    println!(
        "## {name}: {}  {} to {} by {} ({} buckets)",
        agg["query"]["ast"].as_str().unwrap_or("?"),
        agg["bucket"]["from"].as_str().unwrap_or("?"),
        agg["bucket"]["to"].as_str().unwrap_or("?"),
        agg["bucket"]["unit"].as_str().unwrap_or("?"),
        buckets
    );
    println!(
        "hits {total_hits}  pages {total_pages}  places with pages {}  with hits {}  coverage cells {}  scoring {} us",
        ids.len(),
        agg_ids.len(),
        cells.len(),
        scoring.as_micros()
    );

    // 1. Raw counts track corpus volume.
    let o: Vec<f64> = observed_all.iter().map(|&x| x as f64).collect();
    let n: Vec<f64> = pages.iter().map(|&x| x as f64).collect();
    let t: Vec<f64> = ids.iter().map(|id| info[id].titles as f64).collect();
    let with_hits: Vec<usize> = (0..ids.len()).filter(|&i| observed_all[i] > 0).collect();
    let lg = |v: &[f64]| with_hits.iter().map(|&i| v[i].ln()).collect::<Vec<f64>>();
    let r_pages = pearson(&lg(&o), &lg(&n));
    let r_titles = pearson(&lg(&o), &lg(&t));
    println!(
        "raw hits vs pages: spearman {:.3} (all places)  R^2 of log-log {:.3} (places with hits)",
        spearman(&o, &n),
        r_pages * r_pages
    );
    println!(
        "raw hits vs titles (whole corpus): spearman {:.3}  R^2 of log-log {:.3}",
        spearman(&o, &t),
        r_titles * r_titles
    );

    // 2. What the site's "share of pages published" colouring does today:
    //    colour = rel / max(rel) over places with hits.
    let rel: Vec<f64> = (0..ids.len())
        .map(|i| if pages[i] > 0 { o[i] / n[i] } else { 0.0 })
        .collect();
    let max_rel = with_hits.iter().map(|&i| rel[i]).fold(0.0, f64::max);
    let top_half = with_hits
        .iter()
        .filter(|&&i| rel[i] >= 0.5 * max_rel)
        .count();
    let argmax = with_hits
        .iter()
        .copied()
        .max_by(|&a, &b| rel[a].total_cmp(&rel[b]))
        .expect("a place with hits");
    println!(
        "share-of-pages colour today: max {:.4} at {} ({} pages, {} hits); {} of {} places with hits are in the top half of the scale",
        max_rel,
        label(info, rows[argmax].id),
        pages[argmax],
        observed_all[argmax],
        top_half,
        with_hits.len()
    );

    // 3. The model.
    let poisson = score(&spec, &nat_hits, &nat_pages, &cells, ids.len(), Some(1.0));
    let (po, pu) = flags(&poisson.scores);
    let (over, under) = flags(scores);
    println!(
        "dispersion phi {:.2} from {} places{}",
        main.phi,
        main.phi_places,
        if main.phi_places == 0 {
            " (none measurable: fallback)"
        } else {
            ""
        }
    );
    println!(
        "prior: alpha {:.3}  mean {:.3}  (prior sd of lift {:.2})  scored with shape {:.3}",
        main.prior.alpha,
        main.prior.mean,
        main.prior.mean / main.prior.alpha.sqrt(),
        main.prior.shape()
    );
    println!(
        "90% interval above 1 / below 1 / includes 1: {over} / {under} / {}  (phi = 1: {po} / {pu})",
        scores.len() - over - under
    );
    if agg["bucket"]["unit"] == "month" {
        let (yh, yp, ycells) = to_years(&agg, &nat_hits, &nat_pages, &cells);
        let yspec = BucketSpec::new(BucketUnit::Year, spec.from, spec.to);
        let yearly = score(&yspec, &yh, &yp, &ycells, ids.len(), None);
        let (yo, yu) = flags(&yearly.scores);
        println!(
            "same search by year: phi {:.2} from {} places, above/below {yo} / {yu}, {} places change flag",
            yearly.phi,
            yearly.phi_places,
            changed(scores, &yearly.scores)
        );
    }

    let estimate: Vec<f64> = scores.iter().map(|s| s.estimate).collect();
    let lower: Vec<f64> = scores.iter().map(|s| s.lower).collect();
    let top10 = |v: &[f64]| {
        let mut idx: Vec<usize> = (0..v.len()).collect();
        idx.sort_by(|&a, &b| v[b].total_cmp(&v[a]));
        idx.truncate(10);
        idx
    };
    let biggest = top10(&n);
    let overlap = |v: &[f64]| top10(v).iter().filter(|i| biggest.contains(i)).count();
    println!(
        "top 10 that are also among the 10 places with most pages: raw hits {}  share of pages {}  estimate {}  lower bound {}",
        overlap(&o),
        overlap(&rel),
        overlap(&estimate),
        overlap(&lower)
    );
    let median_pages = median(n.clone());
    let mut by_rel: Vec<usize> = with_hits.clone();
    by_rel.sort_by(|&a, &b| rel[b].total_cmp(&rel[a]));
    let small = by_rel
        .iter()
        .take(10)
        .filter(|&&i| n[i] < median_pages)
        .count();
    println!(
        "top 10 by share of pages: {small} have fewer pages than the median place ({median_pages:.0})"
    );

    // 4. Alternatives to step 1: a plain share of all pages in the window
    //    (no time), and the national rate including the place itself.
    let naive: Vec<Counts> = (0..ids.len())
        .map(|i| Counts {
            observed: observed_all[i],
            expected: total_hits as f64 * n[i] / total_pages as f64,
        })
        .collect();
    let (_, naive_scores) = score_all(&naive, main.phi, 0.9);
    let mut worst: Option<(usize, f64)> = None;
    for (i, (s, nv)) in scores.iter().zip(&naive).enumerate() {
        if s.expected >= 5.0 && nv.expected > 0.0 {
            let r = s.expected / nv.expected;
            if worst.is_none_or(|(_, w)| r.ln().abs() > w.ln().abs()) {
                worst = Some((i, r));
            }
        }
    }
    if let Some((i, r)) = worst {
        println!(
            "without time standardization: {} places change flag; largest gap {} expected {:.1} with time vs {:.1} without ({:.2}x)",
            changed(scores, &naive_scores),
            label(info, rows[i].id),
            scores[i].expected,
            naive[i].expected,
            r
        );
    }
    let inclusive: Vec<Counts> = {
        let mut e = vec![0.0; ids.len()];
        for c in &cells {
            e[c.place] += c.pages as f64 * nat_hits[c.bucket] as f64 / nat_pages[c.bucket] as f64;
        }
        (0..ids.len())
            .map(|i| Counts {
                observed: observed_all[i],
                expected: e[i],
            })
            .collect()
    };
    let (_, inclusive_scores) = score_all(&inclusive, main.phi, 0.9);
    let big = biggest[0];
    println!(
        "with the place in its own reference rate: {} places change flag; {} raw lift {:.3} vs {:.3} against the others",
        changed(scores, &inclusive_scores),
        label(info, rows[big].id),
        inclusive[big].observed as f64 / inclusive[big].expected,
        scores[big].lift.unwrap_or(f64::NAN)
    );

    let print = |title: &str, list: &[&Row]| {
        println!("{title}");
        println!(
            "  place | pages | hits | expected | raw lift | estimate | 90% interval | P(above 1)"
        );
        for r in list {
            let s = r.score;
            println!(
                "  {} | {} | {} | {:.1} | {} | {:.2} | {:.2} to {:.2} | {:.2}",
                label(info, r.id),
                r.pages,
                s.observed,
                s.expected,
                s.lift.map_or("-".into(), |l| format!("{l:.2}")),
                s.estimate,
                s.lower,
                s.upper,
                s.above
            );
        }
    };
    let mut by_hits: Vec<&Row> = rows.iter().collect();
    by_hits.sort_by_key(|r| std::cmp::Reverse(r.score.observed));
    print("top 5 by raw hits:", &by_hits[..5.min(by_hits.len())]);
    let mut by_raw: Vec<&Row> = rows.iter().filter(|r| r.score.observed > 0).collect();
    by_raw.sort_by(|a, b| b.score.lift.partial_cmp(&a.score.lift).expect("finite"));
    print("top 5 by raw lift:", &by_raw[..5.min(by_raw.len())]);
    let mut by_lower: Vec<&Row> = rows.iter().collect();
    by_lower.sort_by(|a, b| b.score.lower.total_cmp(&a.score.lower));
    print(
        "top 8 by lower bound (most clearly over-represented):",
        &by_lower[..8.min(by_lower.len())],
    );
    let mut by_upper: Vec<&Row> = rows.iter().filter(|r| r.score.expected > 0.0).collect();
    by_upper.sort_by(|a, b| a.score.upper.total_cmp(&b.score.upper));
    print(
        "top 5 by upper bound (most clearly under-represented):",
        &by_upper[..5.min(by_upper.len())],
    );
    println!();
}

fn label(info: &HashMap<String, Place>, id: &str) -> String {
    info.get(id)
        .map_or_else(|| id.to_owned(), |p| format!("{}, {}", p.name, p.state))
}

fn main() {
    let usage = || -> ! {
        eprintln!("usage: term_skew --dir DIR --names NAME,...");
        std::process::exit(2);
    };
    let mut args = std::env::args().skip(1).peekable();
    let (mut dir, mut names) = (None, Vec::new());
    // The positional form (`term_skew DIR NAME...`) works for one release,
    // with a warning (#277).
    if let Some(d) = args.next_if(|a| !a.starts_with('-')) {
        eprintln!(
            "warning: `term_skew DIR NAME...` is deprecated; use --dir DIR --names NAME,... (#277)"
        );
        dir = Some(d);
        names.extend(args.by_ref());
    }
    while let Some(arg) = args.next() {
        match (arg.as_str(), args.next()) {
            ("--dir", Some(d)) => dir = Some(d),
            ("--names", Some(n)) => names.extend(n.split(',').map(str::to_owned)),
            _ => usage(),
        }
    }
    let Some(dir) = dir else { usage() };
    let dir = Path::new(&dir);
    let info = places(dir);
    corpus(dir, &info);
    for name in &names {
        term(dir, name, &info);
    }
    timing();
}

/// Scoring cost when places don't differ (the prior's alpha at its bound,
/// so every posterior shape is large): 3,000 places, about the size of the
/// full corpus.
fn timing() {
    let counts: Vec<Counts> = (0..3_000)
        .map(|i| {
            let e = 1.0 + (i % 997) as f64 * 37.0;
            Counts {
                observed: e.round() as u64,
                expected: e,
            }
        })
        .collect();
    let started = std::time::Instant::now();
    let (prior, _) = score_all(&counts, 1.0, 0.9);
    println!(
        "## Timing\n3,000 places with no difference between them: alpha {:.0}, scoring {} ms",
        prior.alpha,
        started.elapsed().as_millis()
    );
}
