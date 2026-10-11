# 11: Geographic Skew of Search Terms

**Status:** Proposal for review; phase 1 built, and the per-language baselines from phase 2 (§11.14) · **Date:** September 2026 · **Code:** `crates/usnm-core/src/skew.rs` (scoring and its tests), `crates/usnm-core/examples/term_skew.rs` (the evidence below), `web/src/engine/skew.ts` and `skewModel.ts` (the browser port), `fixtures/skew-vectors.json` (shared test vectors)

## 11.1 The question

The maintainer asked: can the map show where a term is covered disproportionately in one place compared with others? A city with several newspapers will have more hits for any term than a town with one, so raw counts mostly show where the newspapers are.

When this was written, the site showed two things (07 §7.4):

- **Circle size** was the number of pages with a match. Big places get big circles for every search.
- **"Share of pages published"** (`norm=rel`) coloured each place by `hits / pages published` in the window, scaled to the largest value on the map. This already divided by corpus volume, but §11.5.4 shows that the largest value is usually a place with a handful of pages, so almost every other place ended up in the bottom half of the colour scale. (This view was removed in October 2026; old `norm=rel` links open on Pages. See §11.14.)

This document compares the ways to answer the question, recommends one, and backs it with measurements on the published index and with simulations.

## 11.2 Available data

Per page, the index holds the title (`lccn`), the place the title is catalogued under, the date, edition, sequence and language. `/v1/aggregate` returns pages with a match per place and time bucket (the cube) plus the national series (06 §6.3.3).

Each reference snapshot also holds `baselines.json`, the pages published per place and day, built at release from the per-batch counts of every curated page, including pages whose OCR text is empty (04 §4.5, `write_snapshot` in `crates/usnm-ingest/src/release.rs`). `/v1/coverage` serves it as a place by bucket cube from memory, and the site already fetches it with every search. So the denominator for "hits per page" exists for every place, bucket and version, with no new ingest work.

What is not in the snapshot: pages per title per day (the per-batch counts files have them, but the snapshot sums them per place) and population. Snapshots written since the per-language baselines (§11.14) also hold `language_baselines.json`, the same pages split by the languages their title lists; older ones don't. A Japanese query (#139) searches only the Japanese pages of titles that list Japanese, so without a `lang` filter its baseline is `lang=jpn`: those titles' pages, including the Japanese pages LoC shipped without text, which the release adds to the baselines once we've OCR'd them (04 §4.8). Likewise an English query (#237): without a `lang` filter, when its words are English by `usnm_core::query_language`'s word lists, its baseline is the pages of titles that list English, while its hits stay unfiltered (06 §6.3.3).

The published version at the time of writing, `pages-v20260929-4`:

|              |                                            |
| ------------ | ------------------------------------------ |
| Pages        | 6,557,925                                  |
| Places       | 398, in 42 states and territories          |
| Titles       | 1,217 (195 places have exactly one)        |
| Batches      | 882                                        |
| No pages yet | HI, ME, MT, NV, NM, ND, RI, SD, VT, PR, VI |

At 13:30 UTC on 30 September 2026, `/v1/status` reported the backfill had curated 66.0% of LoC's 2,997 batches: 15,788,248 pages, of which 15,766,980 have usable text. The 0.13% without text are in the denominator but can't match. That makes no measurable difference overall, though a place whose pages were mostly blank would read as under-represented.

## 11.3 Candidate measures

| Measure                                                                           | Denominator                             | Available now                                             | Main problem                                                                                              |
| --------------------------------------------------------------------------------- | --------------------------------------- | --------------------------------------------------------- | --------------------------------------------------------------------------------------------------------- |
| Raw hits                                                                          | none                                    | yes                                                       | Tracks corpus volume (§11.5.2)                                                                            |
| Hits per newspaper title                                                          | titles active in the place and window   | no (titles per place are known only for the whole corpus) | Titles differ by thousands of times in digitized pages (§11.5.3)                                          |
| Hits per page ("share of pages")                                                  | pages published in the place and window | yes                                                       | Tiny places take the extremes; mixes years with different national rates (§11.5.4, §11.5.5)               |
| Hits per capita                                                                   | decennial population, interpolated      | no                                                        | Measures digitization as much as interest; population data is hard to get for small places (§11.3.1)      |
| Significance only (z-score, log-likelihood G²)                                    | expected hits                           | yes                                                       | Ranks by sample size, so the biggest places come first again (§11.5.6)                                    |
| **Lift against other places, standardized by time, with shrinkage** (recommended) | expected hits from pages per bucket     | yes                                                       | Needs a dispersion correction for reprinted items (§11.5.7); language mix is still a confounder (§11.5.9) |

### 11.3.1 Per capita rates

Per capita answers a different question ("how much did residents read about this?"), and in this corpus it would mostly measure how much of each place LoC has digitized. The top 10 places hold 60.7% of all pages, and pages per title range from 4 at the 10th percentile of places to 8,728 at the 90th (§11.5.1). A city with one digitized title and a town with ten would get per-capita rates that reflect the digitization program, not the press.

Sourcing is also expensive. The Census Bureau's historical series of city populations covers only the 100 largest urban places in each census from 1790 to 1990 (Gibson 1998). The median place in the index has 1,764 pages and is usually a small town that isn't in that series. County totals exist for every decennial census through IPUMS NHGIS, but a newspaper's readership doesn't follow its town's or county's boundaries, county boundaries changed over the period, and places would still need matching to historical counties. We recommend deferring per capita until there is a county layer (§11.10, phase 3).

## 11.4 Recommendation

Score each place by how much more or less often its pages matched than the other places' pages in the same time buckets, pull scores with little evidence behind them toward the typical place, and show the result on a diverging scale around 1.

### 11.4.1 The score

For a search with national hits `H_b` and national pages `N_b` in bucket `b` (both in the aggregate response), and a place with `P_pb` pages (the coverage cube) and `O_pb` hits (the aggregate cube) in bucket `b`:

1. **Expected hits, against the others, standardized by time.** `E_p = sum over b of P_pb * (H_b - O_pb) / (N_b - P_pb)`. This is indirect standardization, as in a standardized mortality ratio: the place's own pages, each weighted by how often the other places' pages matched in that month or year. It accounts for the number of newspapers, how much of each was digitized, and when. Leaving the place out of its own reference changes the result most for the biggest places: Washington has 30% of all pages. Buckets where the place published every page have no reference and are left out of both `O_p` and `E_p`. In a bucket where the others have no hits but the place has some, the others are credited with half a hit (a continuity correction, as in a Jeffreys prior); a reference rate of 0 would give the place's hits no expected count at all. In that case the direction is clear but the size of the lift is set by the correction, and the colour scale stops at 8 times (§11.6).
2. **Lift.** `O_p / E_p`. 1 means "as often as the others", 2 means twice as often.
3. **Dispersion.** Hits are pages, and a newspaper can print the same item in issue after issue, so counts vary more than independent pages would. `phi` is measured per search: each place's cells are merged into calendar years (calendar months for windows under three years), then consecutive cells into chunks that each expect at least 5 hits at the place's own lift; the place's dispersion is the Pearson chi-square of its chunks, with binomial variance, per degree of freedom; `phi` is the median over places with at least 3 chunks, and at least 1. The fixed calendar resolution makes `phi` the same whichever of day, month or year buckets the search uses (§11.5.7). Week buckets are the exception: a week that crosses a month or year boundary is counted in the period of its first day, because the cells only carry weekly totals, so `phi` for a weekly search can differ slightly from the same search by day. A place whose interest rises and falls over the window also adds to `phi`, so it errs toward wider intervals. When no place expects enough hits (small searches), `phi` is 4, about the middle of the measured values (2.3 to 11.6, §11.5.7). Dividing `O_p` and `E_p` by `phi` treats a place as having `1/phi` as much independent evidence.
4. **Shrinkage (empirical Bayes).** The lift gets a gamma prior with mean `mu` and shape `alpha`, both fitted per search by maximizing the negative binomial marginal likelihood of all places' counts. `mu` is fitted because the typical place need not match the page-weighted reference: for yellow fever it is 0.85, for Klondike 1.40. The posterior for each place is `Gamma(a + O_p/phi, rate a/mu + E_p/phi)`, where `a` is `alpha` widened by the uncertainty in the fitted `mu`: with `v` the sampling variance of `ln mu` (the inverse of its Fisher information, a sum over places of `alpha m/(alpha + m)` with `m` the place's mean count over `phi`), `1/a = 1/alpha + v (1 + 1/alpha)`. Without it, a search with few hits where the places don't differ fits `alpha` at its bound and puts every place at the fitted `mu` with a very narrow interval, so a `mu` of 0.87 from 17 hits flagged every place as clearly below the others (found while building phase 1; the unit test `few_hits_and_no_difference_flags_nothing`). For the seven searches `a` is 0.7% to 1.8% below `alpha`, except cross of gold (8.66 against 10.13). This is the Poisson-gamma model used in disease mapping (Clayton and Kaldor 1987); DuMouchel (1999) uses a mixture of two gammas for the same purpose, which is a possible refinement (§11.10).
5. **What is shown.** The posterior median as the place's estimate (the colour scale is logarithmic, and the median of a skewed posterior sits better on it than the mean); the 90% central credible interval for "clearly above 1", "clearly below 1" or "can't tell"; and lists of places sorted by the interval's lower bound (most clearly over-represented) and upper bound (most clearly under-represented). Ranking by a lower credible bound follows DuMouchel's EB05. The posterior probability that the lift is above 1 is available for tooltips.

A village with one page and one hit has a raw lift near 20, but its estimate stays near the prior and its interval includes 1 (§11.5.6), so it is drawn as "can't tell". How far a place moves depends on `alpha`: when places differ a lot (boll weevil, `alpha` 0.52) the prior is wide and a single hit moves the estimate further. That is why the interval, not a page-count cut-off, decides what is drawn prominently.

**States** use the same steps with the state as the unit: each state's cells are summed per bucket and compared with the other states' pages in that bucket, so a term common across a whole state isn't part of its own reference. They use the larger of the place-level `phi` and their own, measured the same way on the state cells, and their own `alpha` and `mu` fitted across states. A state's hits vary more than a place's: on the seven searches the states' own `phi` was 2 to 4 times the places' (7.6 against 3.08 for yellow fever, 41.4 against 11.62 for baking powder), and the places' value made state intervals too narrow. A state's score is dominated by its biggest cities, and the District of Columbia, with 30% of pages, is compared with everything else.

### 11.4.2 Comparison set

The comparison is with the other **published** places in the same buckets, within the search's `state` filter. With `state=GA,SC`, a place is compared with the other Georgia and South Carolina places; the legend states how many places and states that is, and the view is off when fewer than 10 places have pages in the window: with fewer, the typical place and the spread between places come from a handful of points, and the rates mostly compare a few newspapers ("guerra civil" in Spanish papers, 1935–1940, has 8 places with pages and one paper with 71% of the matches). The end-to-end tests build the site with `VITE_SKEW_MIN_PLACES=5`, since their fixture corpus has 6 places. With `lccn` or `front` filters the baselines are not exact (the API returns `baseline: null`), so the score is unavailable. With a `lang` filter the baselines count only the pages of titles that list one of the languages, so the score compares those pages with each other place's pages in the same languages (§11.14); a version published before per-language counts has none and the score is unavailable there too.

### 11.4.3 Implementation

**In the browser.** It already has every input: the aggregate's cube and national series, and the coverage cube. The fit is a two-parameter search over at most a few thousand places, and the rest is arithmetic per place. Computing it there needs no new endpoint, costs the search backend nothing, and leaves the aggregate's cache format and its role in the search log (ADR-0012) untouched.

`usnm_core::skew` is the reference implementation: `place_counts`, `dispersion_for`, `Prior::fit`, `Prior::score`, `score_all`, and `score_search` and `score_groups` for a whole search and its states, with the tests in §11.5.8. The web version is a port of it (`web/src/engine/skew.ts`, about 450 lines of TypeScript including the gamma functions). `fixtures/skew-vectors.json` holds inputs and the scores the Rust gives them, with the tolerances both sides allow; `cargo test` (`crates/usnm-core/tests/skew_vectors.rs`, which rewrites the file when `UPDATE_SKEW_VECTORS` is set) and the web unit tests (`web/src/engine/skew.test.ts`) both check it, so the two can't drift (§11.14).

A public `/v1/skew` endpoint for researchers can follow later (phase 2). It would reuse the aggregate's cache key, but the review in §11.12 found details to handle: the cache holds serialized bytes, so the handler must parse the aggregate body; on a cache miss it would run a search that the search log doesn't count, so ADR-0012 would have to name it; a coarsened aggregate changes the bucket; the model needs a version in the cache key so a change to the scoring isn't hidden by cached responses; and scoring should run off the async workers.

### 11.4.4 Fallbacks

- If `alpha` reaches its upper bound (no detectable difference between places), every estimate is close to the prior mean and the map says "can't tell" everywhere. That is the correct answer when places don't differ (the null simulation in §11.5.8). That depends on the scoring shape allowing for the uncertainty in the fitted mean (§11.4.1 step 4); otherwise a mean fitted from few hits that lands away from 1 flags every place.
- If the coverage cube can't be fetched, the site keeps the raw view, as the relative view does today.
- The existing "share of pages" view stays. A small fix is worth making independently: scale its colour to a high percentile instead of the maximum (§11.5.4). (It was later removed; see §11.14.)

## 11.5 Evidence

### 11.5.1 Method

Read-only requests to the public API, pinned to `v=pages-v20260929-4`, one at a time with pauses. The script below makes 16 requests; the runs behind this document also made two retries, a few `/v1/hits` requests (two of them saved for §11.5.9) and `/v1/meta` and `/v1/status` requests (the status response saved for §11.2). Each aggregate response records the backend time it took when computed: 16 to 22 seconds for the three example searches, which were served from the cache, and 3.3 to 6.9 seconds for the other four. Two of those four timed out (503) on the first try and succeeded on one retry.

```sh
V=pages-v20260929-4; API=https://usnewsmap.com/v1; D=/tmp/termskew; mkdir -p $D
curl -s "$API/places?v=$V" -o $D/places.json
curl -s "$API/coverage?bucket=year&from=1751-05-09&to=1963-12-31&v=$V" -o $D/cov_year.json
while read -r name qs; do
  curl -s "$API/aggregate?$qs&v=$V" -o $D/agg_$name.json
  curl -s "https://usnewsmap.com$(jq -r .cube.baseline_ref $D/agg_$name.json)" -o $D/cov_$name.json
  sleep 10
done <<'EOF'
yellow_fever q=%22yellow+fever%22&bucket=month
free_silver q=%22free+silver%22&bucket=month
cross_of_gold q=%22cross+of+gold%22&from=1896-06-01&to=1896-12-31&bucket=week
boll_weevil q=%22boll+weevil%22&from=1895-01-01&to=1930-12-31&bucket=year
klondike q=klondike&from=1896-01-01&to=1901-12-31&bucket=month
mormon q=mormon&from=1850-01-01&to=1930-12-31&bucket=year
baking_powder q=%22baking+powder%22&from=1880-01-01&to=1920-12-31&bucket=year
EOF
cargo run -q --release -p usnm-core --example term_skew -- --dir $D \
  --names yellow_fever,free_silver,cross_of_gold,boll_weevil,klondike,mormon,baking_powder
```

A request pinned to an older version is redirected once a newer one is published, so rerunning this later measures the newer version. The full output is in §11.13. The corpus figures:

```
places 398  pages 6557925  median pages/place 1764  p90 18728  max 1990131  top 10 places hold 60.7% of pages
pages per title, by place: p10 4  median 805  p90 8728  min 1  max 67594  (p90/p10 = 2182x)
spearman(pages, titles) across places = 0.437  places with one title 195
states and territories with pages 42
years with pages 198  of which with pages from fewer than 5 states 60  first year with 5+ states 1776
```

The harness also checks that the inputs agree: in every bucket, the national hits equal the sum of the places' hits, the national pages equal the sum of the coverage cells, and no hits fall in a cell without pages.

### 11.5.2 Raw counts and corpus volume

Spearman correlation between a place's hits and its pages in the window, over every place with pages, and how many of the 10 places with the most hits are also among the 10 with the most pages:

| Search            | Window                         | Places with pages | Spearman(hits, pages) | Top 10 by hits that are top 10 by pages |
| ----------------- | ------------------------------ | ----------------- | --------------------- | --------------------------------------- |
| `"yellow fever"`  | whole corpus, by month         | 398               | 0.825                 | 7                                       |
| `"free silver"`   | whole corpus, by month         | 398               | 0.736                 | 7                                       |
| `"cross of gold"` | June to December 1896, by week | 58                | 0.597                 | 9                                       |
| `"boll weevil"`   | 1895 to 1930, by year          | 262               | 0.759                 | 4                                       |
| `klondike`        | 1896 to 1901, by month         | 163               | 0.883                 | 8                                       |
| `mormon`          | 1850 to 1930, by year          | 336               | 0.862                 | 6                                       |
| `"baking powder"` | 1880 to 1920, by year          | 282               | 0.916                 | 8                                       |

For most searches the raw map is largely a map of where the pages are. New York and Washington are in the top 5 by hits for all seven searches.

### 11.5.3 Titles as a denominator

Across places, pages per title range from 4 (10th percentile) to 8,728 (90th percentile), and the rank correlation between a place's pages and its titles is only 0.437. How much of a title LoC has digitized varies far more than how many titles a place has. Rank correlation of hits with pages and with titles over all places with pages, and the share of the variance in log hits explained by each over places with hits:

| Search        | Spearman with pages | Spearman with titles | R² with pages | R² with titles |
| ------------- | ------------------- | -------------------- | ------------- | -------------- |
| yellow fever  | 0.825               | 0.381                | 0.561         | 0.217          |
| free silver   | 0.736               | 0.341                | 0.409         | 0.126          |
| cross of gold | 0.597               | 0.555                | 0.660         | 0.394          |
| boll weevil   | 0.759               | 0.165                | 0.326         | 0.059          |
| klondike      | 0.883               | 0.156                | 0.779         | 0.030          |
| mormon        | 0.862               | 0.359                | 0.640         | 0.146          |
| baking powder | 0.916               | 0.302                | 0.767         | 0.085          |

Two caveats weaken the titles column: titles are counted over the whole corpus, not the window, and 195 of the 398 places have exactly one title, so the title count varies little. The two whole-corpus searches (yellow fever, free silver) avoid the first caveat and show the same gap. Pages track hits much more closely, and they are the unit hits are counted in.

### 11.5.4 Share of pages and small places

The site's relative view colours by `rel / max(rel)`. The place with the largest share, and how many places with hits land in the top half of the colour scale:

| Search        | Largest share | Place                | Its pages | Its hits | Places in top half |
| ------------- | ------------- | -------------------- | --------- | -------- | ------------------ |
| yellow fever  | 1.000         | Little Rock Ark., AR | 1         | 1        | 2 of 265           |
| free silver   | 0.500         | Laurel, DE           | 8         | 4        | 6 of 204           |
| cross of gold | 0.250         | Laurel, DE           | 4         | 1        | 1 of 32            |
| boll weevil   | 0.125         | Columbia, SC         | 8         | 1        | 1 of 164           |
| klondike      | 0.750         | Batesville, AR       | 4         | 3        | 6 of 120           |
| mormon        | 0.545         | Mariposa, CA         | 209       | 114      | 2 of 261           |
| baking powder | 0.263         | Bessemer, CO         | 392       | 103      | 5 of 209           |

Of the 10 places with the largest share of pages, 4 to 10 (depending on the search) have fewer pages than the median place in the window.

### 11.5.5 Time standardization and the leave-one-out reference

The alternative to §11.4.1 step 1 is `E = total hits * place pages / total pages`, which ignores when the place's pages were printed. With `phi` unchanged and the prior refitted, the number of places whose flag (above, below, can't tell) changes, and the largest gap between the two expectations:

| Search        | Flag changes | Largest gap                                       |
| ------------- | ------------ | ------------------------------------------------- |
| yellow fever  | 162 of 398   | Miami, FL: 18.0 expected with time, 280.8 without |
| free silver   | 165 of 398   | Chicago, IL: 9.8 with, 280.1 without              |
| cross of gold | 1 of 58      | (one half-year window)                            |
| boll weevil   | 22 of 262    | Deland, FL: 7.0 with, 28.2 without                |
| klondike      | 17 of 163    | Milford, DE: 5.1 with, 18.6 without               |
| mormon        | 76 of 336    | Mariposa, CA: 19.5 with, 2.7 without              |
| baking powder | 44 of 282    | Hardy, AR: 17.3 with, 4.8 without                 |

Over long windows a term's national rate moves a lot, and places are digitized for different decades. Chicago's pages mostly fall outside the years when "free silver" was frequent, so a plain share of pages would expect 29 times as many hits there (280.1 against 9.8) and understate Chicago's lift by the same factor.

Leaving each place out of its own reference changes fewer flags (0 or 1 per search) but moves the biggest places: Washington's raw lift for boll weevil is 0.360 against a national rate that includes Washington and 0.284 against the others; for yellow fever, 1.093 and 1.116. The unit tests `compares_each_place_with_the_others_in_the_same_buckets` and `a_big_place_is_not_compared_with_itself` check both effects on small examples.

The bucket unit still matters a little, because it sets how finely time is standardized: scoring the three monthly searches by year instead changes the flags of 5 places (yellow fever), 3 (free silver) and 2 (Klondike), with `phi` identical.

### 11.5.6 Shrinkage and ranking

Yellow fever, whole corpus, by month (`phi` 3.08 from 144 places, `alpha` 2.321, `mu` 0.846, scored with shape 2.305):

Top 5 by raw hits:

| Place               | Pages     | Hits   | Expected | Raw lift | Estimate | 90% interval |
| ------------------- | --------- | ------ | -------- | -------- | -------- | ------------ |
| New-York, NY        | 699,264   | 15,124 | 13,236.0 | 1.14     | 1.14     | 1.12 to 1.17 |
| Washington, DC      | 1,990,131 | 12,658 | 11,337.8 | 1.12     | 1.12     | 1.09 to 1.15 |
| Wilmington, DE      | 192,859   | 3,413  | 3,373.4  | 1.01     | 1.01     | 0.96 to 1.06 |
| Birmingham, AL      | 164,346   | 3,316  | 1,207.4  | 2.75     | 2.73     | 2.60 to 2.87 |
| Washington City, DC | 118,302   | 3,114  | 3,751.7  | 0.83     | 0.83     | 0.79 to 0.87 |

Top 5 by raw lift:

| Place                | Pages | Hits | Expected | Raw lift | Estimate | 90% interval |
| -------------------- | ----- | ---- | -------- | -------- | -------- | ------------ |
| Little Rock Ark., AR | 1     | 1    | 0.0      | 21.68    | 0.84     | 0.23 to 2.09 |
| Langston City, OK    | 4     | 1    | 0.1      | 19.97    | 0.84     | 0.23 to 2.09 |
| Littleton, NC        | 4     | 1    | 0.1      | 15.81    | 0.84     | 0.23 to 2.09 |
| Hydaburg, AK         | 56    | 1    | 0.1      | 9.10     | 0.83     | 0.23 to 2.08 |
| Stamford, CT         | 8     | 1    | 0.1      | 7.25     | 0.83     | 0.23 to 2.07 |

Top 8 by the lower bound of the interval:

| Place          | Pages   | Hits  | Expected | Raw lift | Estimate | 90% interval |
| -------------- | ------- | ----- | -------- | -------- | -------- | ------------ |
| Mobile, AL     | 1,351   | 336   | 47.3     | 7.10     | 6.14     | 5.23 to 7.15 |
| Key West, FL   | 51,153  | 204   | 38.9     | 5.25     | 4.45     | 3.62 to 5.39 |
| Montgomery, AL | 135,189 | 2,058 | 646.8    | 3.18     | 3.15     | 2.95 to 3.35 |
| Birmingham, AL | 164,346 | 3,316 | 1,207.4  | 2.75     | 2.73     | 2.60 to 2.87 |
| Huntsville, AL | 2,796   | 201   | 73.7     | 2.73     | 2.52     | 2.05 to 3.06 |
| Monticello, AR | 4,283   | 132   | 48.3     | 2.73     | 2.43     | 1.89 to 3.08 |
| Marysville, CA | 3,999   | 298   | 155.4    | 1.92     | 1.86     | 1.57 to 2.18 |
| Batesville, AR | 18,005  | 265   | 136.6    | 1.94     | 1.87     | 1.56 to 2.22 |

The one-hit places fall back to the prior (about 0.84) with intervals that include 1, so the map draws them as "can't tell". The places the lower bound ranks first have tens to thousands of hits each. Places with many pages still appear when their lift is large (Montgomery, Birmingham), because the ranking is by how clearly a place differs. An earlier run without the zero-reference correction put Philadelphia third, with 41 hits against 2.0 expected, because some of its hits fell in months when no other place's pages matched, and those hits had no expected count to be compared with; with the correction it drops out of the list.

Klondike, 1896 to 1901, by month (`phi` 3.94, `alpha` 1.229, `mu` 1.401, scored with shape 1.207), top 8 by lower bound:

| Place              | Pages | Hits  | Expected | Estimate | 90% interval   |
| ------------------ | ----- | ----- | -------- | -------- | -------------- |
| Skagway, AK        | 1,900 | 1,109 | 46.0     | 22.52    | 20.39 to 24.80 |
| Douglas City, AK   | 668   | 249   | 22.4     | 9.79     | 7.92 to 11.95  |
| Fort Wrangel, AK   | 476   | 209   | 31.0     | 6.17     | 4.89 to 7.66   |
| Skaguay Alaska, AK | 54    | 36    | 4.8      | 4.82     | 2.73 to 7.76   |
| Arizona City, AZ   | 1,188 | 141   | 48.7     | 2.78     | 2.09 to 3.60   |
| Mineral Park, AZ   | 1,561 | 168   | 70.1     | 2.33     | 1.80 to 2.96   |
| Salisbury, CT      | 1,297 | 147   | 64.3     | 2.22     | 1.68 to 2.87   |
| Elbert, CO         | 463   | 66    | 26.3     | 2.34     | 1.54 to 3.38   |

By raw hits the Klondike top 4 are San Francisco, Washington, New York and Los Angeles; Skagway is fifth. For boll weevil (1895 to 1930) the top 8 by lower bound are Montgomery and Birmingham, Alabama, and six Arkansas places (Magnolia, AR: 144 hits where 12.9 were expected), while Washington (1,867 hits, lift 0.28) and New York (527 hits, lift 0.28) are third and fifth by raw hits.

How many of each ranking's top 10 are among the 10 places with the most pages:

| Search        | Raw hits | Share of pages | Estimate | Lower bound |
| ------------- | -------- | -------------- | -------- | ----------- |
| yellow fever  | 7        | 0              | 2        | 2           |
| free silver   | 7        | 0              | 1        | 2           |
| cross of gold | 9        | 0              | 5        | 5           |
| boll weevil   | 4        | 1              | 2        | 2           |
| klondike      | 8        | 0              | 0        | 1           |
| mormon        | 6        | 0              | 0        | 0           |
| baking powder | 8        | 0              | 0        | 0           |

Share of pages removes the big places from the top, but only by putting the one-page places there. The lower bound removes both. For "cross of gold" (263 hits in half a year) only one place's interval excludes 1 (Washington, DC: 30 hits where 49.4 were expected, 0.49 to 0.99): only the larger places have enough hits to say anything, and not enough to say it clearly.

Ranking by significance alone (a z-score) brings the big places back: in the unit test `ranks_by_size_of_skew_not_by_size_of_place`, 10 big places at 1.15 times the reference rate fill the z-score top 10, while the lower bound's top 10 are the 10 mid-size places at 3 times.

### 11.5.7 Reprinted items (over-dispersion)

`phi` per search, and the places flagged above and below 1 with and without it:

| Search        | phi (places measured) | Above / below with phi = 1 | With the measured phi |
| ------------- | --------------------- | -------------------------- | --------------------- |
| yellow fever  | 3.08 (144)            | 42 / 138                   | 23 / 91               |
| free silver   | 4.65 (94)             | 48 / 70                    | 27 / 32               |
| cross of gold | 2.33 (5)              | 4 / 6                      | 0 / 1                 |
| boll weevil   | 4.85 (76)             | 41 / 130                   | 34 / 104              |
| klondike      | 3.94 (67)             | 38 / 44                    | 28 / 28               |
| mormon        | 3.29 (153)            | 67 / 125                   | 53 / 86               |
| baking powder | 11.62 (155)           | 102 / 88                   | 63 / 33               |

Every search is over-dispersed. We expect repeated advertisements to be part of it for "baking powder", the highest, but these counts can't separate reprinting from a place's own changes in interest over the window; either way, treating pages as independent would overstate the evidence. The first version of this proposal measured `phi` on the search's own buckets, and the internal review showed it then depended on the bucket unit (for yellow fever, 1.30 by month in our run and 2.29 by year in the reviewer's). Measuring on calendar years removes that: the same three searches by year give identical `phi`, and the unit test `dispersion_for_ignores_the_bucket_unit` checks it.

In the simulations (§11.5.8), `phi` comes out at 3.78 with 240 fine buckets and 4.04 with 20 coarse ones for items that run 4 pages at a time with interest drifting 30% over the window; and when no place differs from the others but items run 6 pages at a time, the plain Poisson model flags 124 of 300 places and the corrected model flags none (`phi` measured 5.52).

### 11.5.8 Simulations (unit tests)

`cargo test -p usnm-core skew` runs these, with a fixed-seed generator. The first three test the model on data it assumes; the next three on a prior of the wrong shape, only 42 units, and reprinted items.

| Test                                                                        | Setup                                                                                          | Result                                                                                                                                                      |
| --------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `no_difference_between_places_means_no_skew`                                | 400 places, heavy-tailed exposure (median 20 expected hits), every lift 1                      | `alpha` about 31,000, `mu` 0.998; 0 places flagged; estimates between 0.996 and 1.003, while the highest raw lift is 4.4 at a place with 0.46 expected hits |
| `recovers_the_spread_between_places_and_calibrates_intervals`               | 20 runs of 400 places, lifts from `Gamma(4, 4)`                                                | Fitted `alpha` 3.4 to 5.3 (median 3.9, true 4); 90% intervals contain the true lift for 90.4% of 8,000 places                                               |
| `one_page_one_hit_does_not_top_the_map`                                     | a village (1 hit, 0.01 expected) and a city (2,000 hits, 1,000 expected)                       | Village: raw lift 100, estimate 1.39, interval 0.72 to 2.38; city: estimate 2.00, interval 1.92 to 2.07; only the city is flagged                           |
| `hits_only_here_still_have_an_expected_count`                               | the term appears only in one place (5 hits on 100 pages; 10,000 other pages with none)         | Both places get a finite expected count; the first is flagged above 1 and the other below                                                                   |
| `stays_calibrated_when_the_prior_is_wrong`                                  | lifts not gamma: 85% log-normal with mean 1.3, 15% at 0.05 (like a paper in another language)  | Coverage 88.5%; fitted `mu` 1.02 to 1.17 (the true mean lift is 1.11)                                                                                       |
| `intervals_with_few_units`                                                  | 42 units, as in the states layer, 200 runs                                                     | Coverage 89.8%                                                                                                                                              |
| `reprinting_with_real_differences_stays_calibrated_once_dispersion_is_used` | `Gamma(4, 4)` lifts, items in runs of 6, 10 runs of 300 places                                 | Coverage 53.9% with `phi = 1`, 89.6% with `phi = 6`                                                                                                         |
| `dispersion_measures_reprinting_whatever_the_bucket`                        | 300 places; runs of 4 and 30% drift in interest, 240 or 20 buckets                             | `phi` 1.00 with independent pages; 3.78 fine, 4.04 coarse                                                                                                   |
| `dispersion_for_ignores_the_bucket_unit`                                    | the same data by month and by year                                                             | identical `phi`                                                                                                                                             |
| `reprinting_alone_is_not_skew_once_dispersion_is_used`                      | no differences, runs of 6                                                                      | `phi` 5.52; 124 flagged with `phi = 1`, 0 with it                                                                                                           |
| `few_hits_and_no_difference_flags_nothing`                                  | 12 places at the reference rate with 17 hits; `mu` fitted 0.87 by chance, `alpha` at its bound | none flagged (all 12 without the mean's uncertainty, §11.4.1 step 4)                                                                                        |
| `ranks_by_size_of_skew_not_by_size_of_place`                                | 10 big places at 1.15x, 10 mid-size at 3x, 280 at 1x                                           | z-score top 10: all 10 big places; lower-bound top 10: all 10 at 3x                                                                                         |

The special functions are checked against known values: `ln_gamma`; the regularized incomplete gamma function at chi-square 95th percentiles for 1 to 1,000 degrees of freedom; its inverse by round trip for shapes from 0.01 to 9,999; the Wilson and Hilferty approximation used above shape 10,000 against the asymptotic median of large gammas; and the normal quantile.

### 11.5.9 Other findings

- **Language.** San Diego, CA has 0 hits for "yellow fever" where 130.6 were expected, and it is among the five most under-represented places for five of the seven searches. Its one title is the _Süd California Deutsche Zeitung_; Pittsburg, PA, among the five most under-represented for three searches, is the _Amerikanski Srbobran_ (`/v1/hits`). By their names these are German- and Serbian-language papers (the snapshot's title records list languages; we did not check them). English search terms rarely match such pages, so these places read as under-represented for any English term. Phase 1 names the languages of a place's papers wherever any of them is not English (tooltip, lists, table and CSV) and scores every place the same way; phase 2 adds per-language baselines. Many titles list English as well as another language (the maintainer reports that LoC lists the _Amerikanski Srbobran_ as Serbian and English), so a rule that only marked places whose titles are all non-English would miss the cases it was meant for.
- **Duplicate places.** "Washington, DC" and "Washington City, DC", "Skagway, AK" and "Skaguay Alaska, AK", "Little Rock, AR" and "Little Rock Ark., AR" are separate places in the catalog. Each is scored on its own pages, so the scores are correct but split; the fix belongs in the place overrides (spike S-3, 10 §10.2).
- **Thin early years.** 60 of the 198 years with pages have pages from fewer than 5 states. For searches in those years "the others" are a handful of places.

## 11.6 Web

- A two-way toggle, **"Pages"** and **"Relative rate"** (`norm=skew`), replacing the colour menu. The older "Share of pages published" (`norm=rel`) was removed; its permalinks open on Pages (§11.14).
- **Colour:** the estimate on a diverging, colour-blind-safe scale in log2, centred on 1, clamped at 1/8 and 8, with legend ticks at 1/8, 1/4, 1/2, 1, 2, 4 and 8 times. Places whose interval includes 1 are drawn at reduced opacity. County- and state-precision places, drawn as hollow rings today, get the colour on the ring. The heat layer is not offered in this view.
- **Which places, and how big:** every place with pages in the window, including those with no hits (the under-represented list is mostly places with 0 hits). Circle area follows expected hits, the amount of evidence behind each colour, with a visible minimum size so a place with pages but almost no expected hits (a few pages, or pages only from months when the term was absent elsewhere) is still drawn, as "can't tell" unless its interval excludes 1. Circles change size when switching into this view; the alternative, area by hits, would hide the places with 0 hits that fill the under-represented list.
- **Tooltip and table:** "Mobile, AL: 336 pages matched where 47 were expected from its 1,351 pages. About 6.1× the rate of the other places (5.2 to 7.2×)." The place table gains Expected and Relative rate columns and the 90% range, and the panel gets two short lists, most clearly above and most clearly below, sorted by the interval bounds, only places whose interval excludes 1. The UI does not use the word "significant".
- **Legend:** the title, the colour bar and its ticks. The explanation, "Matches per page compared with the other 397 places with pages in this window, in 42 states, over the same months. 1× is the same rate.", and what faded circles and circle area mean, are behind the legend's info button, which opens on a tap and closes with Escape or a tap elsewhere. The Pages and Median date legends work the same way.
- **Playback:** at load, build an expected-hits cube (each coverage cell's pages times the other places' rate in its bucket) and prefix-sum it like the page sums, so each frame is O(places). `alpha`, `mu` and `phi` stay fixed at the full-window values so colours don't jump because of a refit; for short trailing windows the intervals are then only a guide, since the spread between places can differ from the full window's.
- **States layer:** the same score per state, replacing "hits per 1,000 pages" in 07 §7.4. The state choropleth isn't built yet, so phase 1 shows the state scores as a table under the place table.
- **Export:** a separate per-place CSV for the window (`place_id,name,state,pages,hits,expected,estimate,lower,upper,languages`, the last empty or a label such as "Papers in Serbian and English"), because the existing export (06 §6.3.2) is one row per place and bucket. `hits` counts the pages that matched in buckets with other places to compare with, as the score does.

## 11.7 Performance and cost

- **Search backend:** no new queries. The inputs are the aggregate and coverage responses the site already requests.
- **CPU:** scoring the seven searches (the fit, dispersion and every place's interval) took 0.8 to 20 ms each in the Rust release build on an Apple silicon laptop; the whole-corpus monthly searches are the slowest. A synthetic worst case, 3,000 places (about the full corpus, 06 §6.1) with no difference between them so every posterior shape is large, took 48 ms. In the browser port, run in Node 22 (which uses V8, Chrome's JavaScript engine) on the same laptop, fitting a search takes 3 to 31 ms for the seven searches (yellow fever is the slowest) and runs in a web worker; scoring a playback frame takes 0.3 to 2.3 ms. Synthetic cases with 3,000 places: 197 ms to fit 2,552 months at about the corpus's density (511,000 cells), with 10.5 ms per frame; 103 ms for 240 months with 40% of cells filled (297,000 cells), with 8.9 ms per frame.
- **Memory:** running totals of observed hits, expected hits and pages, stored once per coverage cell rather than per place and bucket (each frame looks them up by binary search): about 28 bytes a cell, 1.7 MB for yellow fever and 14 MB for the 511,000-cell synthetic case. A dense places × buckets layout would have taken 24 MB and about 184 MB.
- **Azure cost:** none. No new resources, storage, endpoint or ingest steps.

## 11.8 The backfill

Scores are relative to what is published. A permalink or citation records the index version its numbers came from, but once a newer version is published a request for the old one is redirected to the new one (06 §6.5), so revisiting the link shows the new version's scores. They can differ: when states that have no pages today (for example Maine, Montana or New Mexico) are published, "the others" change, and so do the reference rates behind every expected count. The legend states what the comparison covered and which version it came from, and the citation includes the index version (07 §7.5), so a reader can tell when numbers were measured on an earlier version. No recomputation job is needed: baselines are rebuilt with every release (04 §4.4).

## 11.9 Search log and privacy

Nothing changes. The score is computed in the browser from responses the site already fetches, so no new request carries the query and nothing new is logged (ADR-0012, 09 §9.4).

## 11.10 Limits and plan

**Limits that remain after phase 1:**

- **Language** (§11.5.9). Without a language filter, places with mostly non-English pages read as under-represented for English terms; the view only names the languages of their papers. Since #237 an English query is compared with English-language titles' pages instead (§11.14), so this is left to queries whose language the word lists can't tell. Under a language filter the baselines count only those languages' pages, so a German search compares German pages with German pages. A title in several languages is in every language it lists, so `lang=eng` compares a New York paper's English and German pages as one stream, and the page-level language is the paper's, not the page's.
- **The reference rate is treated as exact.** In buckets where the other places have few pages or few hits, it isn't, and a place can get a large lift from a handful of hits. The zero-reference correction (§11.4.1) handles the extreme case; the legend's place count helps; a later version could add the reference's own uncertainty.
- **OCR quality** varies by title and era. A title with poor OCR matches fewer pages for every term and will read as under-represented. A words-per-page denominator (the curated corpus has `word_count`) would partly correct it, and needs a change to the snapshot.
- **Pages differ in size.** 04 §4.3.1 estimates that later eight-column broadsheet pages carry two to five times the text of an 1879 small-town page, so they are more likely to contain any term. Same fix as above; not measured here.
- **Place is the title's place of publication.** A paper's readers and news sources lie beyond its town. The score measures where the pages were printed.
- **`phi` is one number per search** and errs wide; a place with one heavily repeated advertisement can still be flagged. Phase 3's per-title check addresses it.
- **Many places at once.** With a few hundred places and 90% intervals, the lists will include some places that don't really differ, more so when `alpha` is small. The lists sort by the interval bounds, so the clearest cases come first; the intervals are a guide to what to show prominently, not a hypothesis test.
- **Plug-in prior.** The intervals ignore uncertainty in `alpha`, `mu` and `phi`. With 42 units (states) the simulation still covered 89.8%.

**Phases:**

1. **Now (small; built, §11.14).** `usnm_core::skew`; the browser port with shared test vectors; the `norm=skew` view with its lists, legend and states layer; naming the languages of places whose papers aren't all in English; the per-place CSV. No ingest or infrastructure change; the API's one change is a `languages` list on each place in `/v1/places`.
2. **Language, places and API.** Baselines per place, language and day in the snapshot, so `lang` filters keep exact baselines and the view can compare German pages with German pages (built, §11.14). Merge duplicate places through the catalog overrides. Optionally, words instead of pages as the denominator; a two-gamma prior (DuMouchel 1999) if the single gamma fits poorly; and the public `/v1/skew` endpoint (§11.4.3).
3. **Optional research features.** Per-title counts ("titles that printed it" out of titles publishing), a terms aggregation on `lccn` that costs one more engine call per search, as a check that reprints don't inflate. Per capita at county level from IPUMS NHGIS, if a county layer is added.

## 11.11 References

- Clayton, D. and Kaldor, J. (1987). Empirical Bayes estimates of age-standardized relative risks for use in disease mapping. _Biometrics_ 43(3): 671 to 681.
- DuMouchel, W. (1999). Bayesian data mining in large frequency tables, with an application to the FDA spontaneous reporting system. _The American Statistician_ 53(3): 177 to 190.
- Dunning, T. (1993). Accurate methods for the statistics of surprise and coincidence. _Computational Linguistics_ 19(1): 61 to 74. (The G² statistic in §11.3.)
- Gibson, C. (1998). _Population of the 100 Largest Cities and Other Urban Places in the United States: 1790 to 1990._ U.S. Census Bureau, Population Division Working Paper No. 27.
- IPUMS NHGIS, National Historical Geographic Information System, <https://www.nhgis.org/>.
- Wilson, E. B. and Hilferty, M. M. (1931). The distribution of chi-square. _Proceedings of the National Academy of Sciences_ 17(12): 684 to 688.
- Acklam, P. J. An algorithm for computing the inverse normal cumulative distribution function (the rational approximation in `normal_quantile`).

## 11.12 Review notes

An internal review (a separate reviewer asked for an adversarial reading of the statistics, the data assumptions and the engineering plan, with its own re-implementation of the model) raised the points below before any external review. The reviewer reproduced the first version's flag counts exactly and confirmed the data consistency, the reference citations and most of the evidence figures.

| Raised                                                                                                                                                                                                                                                                                                                                        | Resolution                                                                                                                                                                                                                                                         |
| --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `phi` measured on the search's own buckets depended on the bucket unit (yellow fever 1.30 by month, 2.29 by year), so flags changed with the bucket menu; for "cross of gold" it silently fell back to 1                                                                                                                                      | `dispersion_for` measures on calendar years (months for short windows) with chunks of at least 5 expected hits and binomial variance, reports how many places it used, and falls back to 4, not 1. New tests for bucket invariance and drifting interest (§11.5.7) |
| Each place was part of the rate it was compared against (Washington holds 30% of pages)                                                                                                                                                                                                                                                       | Reference rates leave the place out; buckets where it is the only publisher are dropped (§11.4.1 step 1, §11.5.5)                                                                                                                                                  |
| The prior mean fixed at 1 was rejected by the data (the typical place differs from the page-weighted reference)                                                                                                                                                                                                                               | The prior mean is fitted with `alpha` (yellow fever 0.85, Klondike 1.40); new test with a non-gamma, mean-1.3 population and a spike near 0 (§11.5.8)                                                                                                              |
| A single gamma fits poorly where non-English places pile up at 0; the posterior mean is a poor colour on a log scale when `alpha` is small; "no cut-off is needed" was overstated                                                                                                                                                             | Colour by posterior median; the interval decides prominence, stated with the boll weevil case; non-English places marked and left out of the fit in phase 1 (as built, phase 1 names their languages instead, §11.14); two-gamma prior listed for phase 2          |
| Places with 0 hits weren't drawn, yet they fill the under-represented list                                                                                                                                                                                                                                                                    | In this view every place with pages is drawn, sized by expected hits (§11.6)                                                                                                                                                                                       |
| The `/v1/skew` cache plan understated parsing, cache misses outside the search log, coarsening and model versioning                                                                                                                                                                                                                           | Phase 1 computes in the browser from responses it already has; the endpoint moves to phase 2 with those items listed (§11.4.3)                                                                                                                                     |
| Unsupported or wrong claims: advertisements as the cause of "baking powder" dispersion; San Diego's list count; the boll weevil list; a median; a timing figure from an earlier run; unsaved status and hits responses                                                                                                                        | Corrected or reworded against the final output; the advertisement point is stated as an expectation; status and hits responses saved with the other inputs                                                                                                         |
| `gamma_quantile` was slow for large shapes (the reviewer measured about 190 ms for 400 places at `alpha`'s bound) and inaccurate above shape 5 million                                                                                                                                                                                        | Newton's method from a Wilson and Hilferty start, the approximation alone above shape 10,000, and an iteration limit that grows with the shape; 3,000 places at the bound now take 48 ms; new accuracy tests (§11.7)                                               |
| The simulations only tested the model's own assumptions                                                                                                                                                                                                                                                                                       | Added a misspecified prior, 42 units, and real differences combined with reprinting (§11.5.8)                                                                                                                                                                      |
| Minor: Poisson variance for pages, multiple comparisons, plug-in intervals, per-frame cost, states' `phi`, a mislabelled ranking, refitting the prior for the naive comparison, the titles R², the 0.13%, CSV shape, response naming, cross-references, rounding, one median definition, the menu label, ring-drawn places and the heat layer | Binomial variance in `dispersion`; limits added (§11.10); expected-hits prefix sums; states use the place `phi`; the naive comparison refits the prior; Spearman added and the titles conclusion softened; per-place CSV; the rest corrected in place              |

The first Copilot review raised four more points:

| Raised                                                                                                                     | Resolution                                                                                                                                                                  |
| -------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A place with hits in a bucket where the others have none got no expected count for them, and a confident, extreme estimate | Continuity correction in `reference_rate`, with a test (§11.4.1 step 1). It changed the yellow fever results: Philadelphia (41 hits, 2.0 expected) left the top of the list |
| Summing place-level expected hits for a state keeps the state's other places in its reference                              | States are scored as units, each compared with the other states (§11.4.1)                                                                                                   |
| A place with pages but no expected hits would get a zero-size circle                                                       | A visible minimum size (§11.6)                                                                                                                                              |
| A permalink doesn't keep its numbers after a newer version is published, because old versions are redirected               | Reworded: the link and citation record the version; revisiting shows the new version's scores (§11.8)                                                                       |

The second Copilot review raised two:

| Raised                                                                                                                                         | Resolution                                                                                                                                                      |
| ---------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `phi` took the upper of the two middle values for an even number of places, not the median                                                     | The median now averages the middle pair, with a test. `phi` moved from 4.67 to 4.65 (free silver) and 5.03 to 4.85 (boll weevil); tables updated                |
| A week that crosses a month or year boundary goes wholly to the earlier period, so weekly searches aren't fully independent of the bucket unit | Documented in `dispersion_for` and §11.4.1 step 3: exact for day, month and year buckets, slightly different for weeks, which can't be split from weekly totals |

The third Copilot review raised one:

| Raised                                                                                                   | Resolution                                                                                                                                                                                            |
| -------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| The date range is inclusive, so a window of exactly three years was measured by month instead of by year | The comparison counts the last day, and a test checks that exactly three years gives the same result by month and by year. None of the seven searches is near the boundary; their output is unchanged |

Building phase 1, and its internal review, raised these:

| Raised                                                                                                                                                                                                                                                                       | Resolution                                                                                                                                                                                                                                            |
| ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| States were scored with the places' `phi`, though their own was 2 to 4 times larger                                                                                                                                                                                          | States use the larger of the two, in the Rust and the port, with a test and shared vectors (§11.4.1, "States")                                                                                                                                        |
| A place with no pages in a frame, or nothing expected, was scored at the prior alone and could be listed as "clearly" different when the prior's range excluded 1                                                                                                            | Such a place is shown as "can't tell", and places with no pages in the frame are left out of the lists, table, export and announcement (it is still drawn at size 0)                                                                                  |
| Prefix sums for every place and bucket would take about 184 MB at 3,000 places by month                                                                                                                                                                                      | Running totals are kept per cell (§11.7)                                                                                                                                                                                                              |
| The maintainer asked for papers in other languages to be named, not handled differently                                                                                                                                                                                      | Every place is fitted, scored and drawn the same way; the tooltip, lists, table and CSV name the languages where any title isn't in English (§11.5.9)                                                                                                 |
| With few hits and no real difference between places, `alpha` reaches its bound and every place is scored at the fitted mean: in a simulated daily search (12 places, 17 hits, `mu` 0.87 by chance) every place's interval was 0.872 to 0.875, so all 12 were "clearly below" | The scoring shape adds the uncertainty in the fitted mean (§11.4.1 step 4), with a test. Estimates and bounds for the seven searches move by less than 0.1; for cross of gold Washington, DC is now flagged below (0.49 to 0.99, before 0.50 to 1.00) |

## 11.13 Appendix: full output

<details>
<summary><code>term_skew</code> output for the seven searches</summary>

```
## Corpus (pages-v20260929-4)
places 398  pages 6557925  median pages/place 1764  p90 18728  max 1990131  top 10 places hold 60.7% of pages
pages per title, by place: p10 4  median 805  p90 8728  min 1  max 67594  (p90/p10 = 2182x)
spearman(pages, titles) across places = 0.437  places with one title 195
states and territories with pages 42
years with pages 198  of which with pages from fewer than 5 states 60  first year with 5+ states 1776

## yellow_fever: "yellow fever"  1751-05-09 to 1963-12-31 by month (2552 buckets)
hits 66769  pages 6557925  places with pages 398  with hits 265  coverage cells 61770  scoring 11828 us
raw hits vs pages: spearman 0.825 (all places)  R^2 of log-log 0.561 (places with hits)
raw hits vs titles (whole corpus): spearman 0.381  R^2 of log-log 0.217
share-of-pages colour today: max 1.0000 at Little Rock Ark., AR (1 pages, 1 hits); 2 of 265 places with hits are in the top half of the scale
dispersion phi 3.08 from 144 places
prior: alpha 2.321  mean 0.846  (prior sd of lift 0.56)  scored with shape 2.305
90% interval above 1 / below 1 / includes 1: 23 / 91 / 284  (phi = 1: 42 / 138)
same search by year: phi 3.08 from 144 places, above/below 24 / 87, 5 places change flag
top 10 that are also among the 10 places with most pages: raw hits 7  share of pages 0  estimate 2  lower bound 2
top 10 by share of pages: 9 have fewer pages than the median place (1764)
without time standardization: 162 places change flag; largest gap Miami, FL expected 18.0 with time vs 280.8 without (0.06x)
with the place in its own reference rate: 1 places change flag; Washington, DC raw lift 1.093 vs 1.116 against the others
top 5 by raw hits:
  place | pages | hits | expected | raw lift | estimate | 90% interval | P(above 1)
  New-York, NY | 699264 | 15124 | 13236.0 | 1.14 | 1.14 | 1.12 to 1.17 | 1.00
  Washington, DC | 1990131 | 12658 | 11337.8 | 1.12 | 1.12 | 1.09 to 1.15 | 1.00
  Wilmington, DE | 192859 | 3413 | 3373.4 | 1.01 | 1.01 | 0.96 to 1.06 | 0.64
  Birmingham, AL | 164346 | 3316 | 1207.4 | 2.75 | 2.73 | 2.60 to 2.87 | 1.00
  Washington City, DC | 118302 | 3114 | 3751.7 | 0.83 | 0.83 | 0.79 to 0.87 | 0.00
top 5 by raw lift:
  place | pages | hits | expected | raw lift | estimate | 90% interval | P(above 1)
  Little Rock Ark., AR | 1 | 1 | 0.0 | 21.68 | 0.84 | 0.23 to 2.09 | 0.39
  Langston City, OK | 4 | 1 | 0.1 | 19.97 | 0.84 | 0.23 to 2.09 | 0.39
  Littleton, NC | 4 | 1 | 0.1 | 15.81 | 0.84 | 0.23 to 2.09 | 0.39
  Hydaburg, AK | 56 | 1 | 0.1 | 9.10 | 0.83 | 0.23 to 2.08 | 0.39
  Stamford, CT | 8 | 1 | 0.1 | 7.25 | 0.83 | 0.23 to 2.07 | 0.39
top 8 by lower bound (most clearly over-represented):
  place | pages | hits | expected | raw lift | estimate | 90% interval | P(above 1)
  Mobile, AL | 1351 | 336 | 47.3 | 7.10 | 6.14 | 5.23 to 7.15 | 1.00
  Key West, FL | 51153 | 204 | 38.9 | 5.25 | 4.45 | 3.62 to 5.39 | 1.00
  Montgomery, AL | 135189 | 2058 | 646.8 | 3.18 | 3.15 | 2.95 to 3.35 | 1.00
  Birmingham, AL | 164346 | 3316 | 1207.4 | 2.75 | 2.73 | 2.60 to 2.87 | 1.00
  Huntsville, AL | 2796 | 201 | 73.7 | 2.73 | 2.52 | 2.05 to 3.06 | 1.00
  Monticello, AR | 4283 | 132 | 48.3 | 2.73 | 2.43 | 1.89 to 3.08 | 1.00
  Marysville, CA | 3999 | 298 | 155.4 | 1.92 | 1.86 | 1.57 to 2.18 | 1.00
  Batesville, AR | 18005 | 265 | 136.6 | 1.94 | 1.87 | 1.56 to 2.22 | 1.00
top 5 by upper bound (most clearly under-represented):
  place | pages | hits | expected | raw lift | estimate | 90% interval | P(above 1)
  San Diego, CA | 8964 | 0 | 130.6 | 0.00 | 0.04 | 0.01 to 0.12 | 0.00
  Pittsburg, PA | 48710 | 3 | 68.2 | 0.04 | 0.12 | 0.04 to 0.27 | 0.00
  Flagstaff, AZ | 18728 | 53 | 218.8 | 0.24 | 0.26 | 0.17 to 0.37 | 0.00
  Skagway, AK | 24298 | 45 | 187.4 | 0.24 | 0.26 | 0.17 to 0.38 | 0.00
  Tampa, FL | 29059 | 1 | 36.9 | 0.03 | 0.16 | 0.04 to 0.39 | 0.00

## free_silver: "free silver"  1751-05-09 to 1963-12-31 by month (2552 buckets)
hits 34389  pages 6557925  places with pages 398  with hits 204  coverage cells 61770  scoring 8862 us
raw hits vs pages: spearman 0.736 (all places)  R^2 of log-log 0.409 (places with hits)
raw hits vs titles (whole corpus): spearman 0.341  R^2 of log-log 0.126
share-of-pages colour today: max 0.5000 at Laurel, DE (8 pages, 4 hits); 6 of 204 places with hits are in the top half of the scale
dispersion phi 4.65 from 94 places
prior: alpha 2.413  mean 0.982  (prior sd of lift 0.63)  scored with shape 2.385
90% interval above 1 / below 1 / includes 1: 27 / 32 / 339  (phi = 1: 48 / 70)
same search by year: phi 4.65 from 94 places, above/below 28 / 32, 3 places change flag
top 10 that are also among the 10 places with most pages: raw hits 7  share of pages 0  estimate 1  lower bound 2
top 10 by share of pages: 9 have fewer pages than the median place (1764)
without time standardization: 165 places change flag; largest gap Chicago, IL expected 9.8 with time vs 280.1 without (0.04x)
with the place in its own reference rate: 1 places change flag; Washington, DC raw lift 1.054 vs 1.069 against the others
top 5 by raw hits:
  place | pages | hits | expected | raw lift | estimate | 90% interval | P(above 1)
  Washington, DC | 1990131 | 6595 | 6167.6 | 1.07 | 1.07 | 1.02 to 1.12 | 0.99
  New-York, NY | 699264 | 5579 | 5007.5 | 1.11 | 1.11 | 1.06 to 1.17 | 1.00
  San Francisco, CA | 208359 | 2556 | 3173.3 | 0.81 | 0.81 | 0.75 to 0.86 | 0.00
  Wilmington, DE | 192859 | 2082 | 1661.3 | 1.25 | 1.25 | 1.16 to 1.35 | 1.00
  Birmingham, AL | 164346 | 1975 | 1325.9 | 1.49 | 1.48 | 1.37 to 1.61 | 1.00
top 5 by raw lift:
  place | pages | hits | expected | raw lift | estimate | 90% interval | P(above 1)
  Pinal City, AZ | 1129 | 1 | 0.1 | 15.62 | 0.93 | 0.25 to 2.33 | 0.46
  Columbus, OH | 8 | 1 | 0.1 | 13.18 | 0.93 | 0.25 to 2.33 | 0.46
  Seattle, WA | 8 | 2 | 0.2 | 9.97 | 1.01 | 0.29 to 2.44 | 0.50
  Augusta, GA | 80 | 1 | 0.1 | 9.52 | 0.93 | 0.25 to 2.32 | 0.45
  Parsons, KS | 4 | 1 | 0.1 | 9.52 | 0.93 | 0.25 to 2.32 | 0.45
top 8 by lower bound (most clearly over-represented):
  place | pages | hits | expected | raw lift | estimate | 90% interval | P(above 1)
  Florence, AZ | 1917 | 184 | 26.8 | 6.87 | 5.08 | 3.90 to 6.49 | 1.00
  Montgomery, AL | 135189 | 609 | 230.5 | 2.64 | 2.56 | 2.21 to 2.94 | 1.00
  Mineral Park, AZ | 9111 | 295 | 136.9 | 2.16 | 2.06 | 1.67 to 2.50 | 1.00
  Leavenworth, KS | 1261 | 96 | 40.5 | 2.37 | 2.04 | 1.41 to 2.82 | 1.00
  Smyrna, DE | 36878 | 252 | 136.4 | 1.85 | 1.77 | 1.41 to 2.19 | 1.00
  Prescott, AZ | 14966 | 246 | 132.9 | 1.85 | 1.77 | 1.41 to 2.19 | 1.00
  Birmingham, AL | 164346 | 1975 | 1325.9 | 1.49 | 1.48 | 1.37 to 1.61 | 1.00
  Topeka, KS | 7194 | 426 | 261.3 | 1.63 | 1.60 | 1.34 to 1.88 | 1.00
top 5 by upper bound (most clearly under-represented):
  place | pages | hits | expected | raw lift | estimate | 90% interval | P(above 1)
  Deland, FL | 9665 | 10 | 275.0 | 0.04 | 0.07 | 0.03 to 0.14 | 0.00
  San Diego, CA | 8964 | 0 | 111.6 | 0.00 | 0.08 | 0.02 to 0.20 | 0.00
  Little Rock, AR | 58416 | 29 | 287.4 | 0.10 | 0.13 | 0.07 to 0.22 | 0.00
  Newtown, CT | 11767 | 19 | 218.3 | 0.09 | 0.12 | 0.06 to 0.23 | 0.00
  Ocala, FL | 15152 | 79 | 336.1 | 0.24 | 0.25 | 0.17 to 0.36 | 0.00

## cross_of_gold: "cross of gold"  1896-06-01 to 1896-12-31 by week (31 buckets)
hits 263  pages 39423  places with pages 58  with hits 32  coverage cells 1481  scoring 809 us
raw hits vs pages: spearman 0.597 (all places)  R^2 of log-log 0.660 (places with hits)
raw hits vs titles (whole corpus): spearman 0.555  R^2 of log-log 0.394
share-of-pages colour today: max 0.2500 at Laurel, DE (4 pages, 1 hits); 1 of 32 places with hits are in the top half of the scale
dispersion phi 2.33 from 5 places
prior: alpha 10.129  mean 0.995  (prior sd of lift 0.31)  scored with shape 8.663
90% interval above 1 / below 1 / includes 1: 0 / 1 / 57  (phi = 1: 4 / 6)
top 10 that are also among the 10 places with most pages: raw hits 9  share of pages 0  estimate 5  lower bound 5
top 10 by share of pages: 7 have fewer pages than the median place (128)
without time standardization: 1 places change flag; largest gap Tombstone, AZ expected 5.7 with time vs 4.8 without (1.18x)
with the place in its own reference rate: 1 places change flag; New-York, NY raw lift 1.219 vs 1.276 against the others
top 5 by raw hits:
  place | pages | hits | expected | raw lift | estimate | 90% interval | P(above 1)
  New-York, NY | 7417 | 58 | 45.4 | 1.28 | 1.18 | 0.87 to 1.55 | 0.82
  San Francisco, CA | 3641 | 31 | 23.9 | 1.30 | 1.14 | 0.78 to 1.59 | 0.73
  Washington, DC | 7011 | 30 | 49.4 | 0.61 | 0.71 | 0.49 to 0.99 | 0.05
  Los Angeles, CA | 2440 | 27 | 15.7 | 1.72 | 1.29 | 0.87 to 1.82 | 0.86
  Birmingham, AL | 1638 | 23 | 10.2 | 2.25 | 1.39 | 0.92 to 2.00 | 0.91
top 5 by raw lift:
  place | pages | hits | expected | raw lift | estimate | 90% interval | P(above 1)
  Laurel, DE | 4 | 1 | 0.0 | 62.55 | 1.01 | 0.55 to 1.67 | 0.51
  Prescott, AZ | 118 | 5 | 0.7 | 7.00 | 1.16 | 0.67 to 1.85 | 0.68
  Wichita, KS | 90 | 3 | 0.7 | 4.28 | 1.07 | 0.60 to 1.74 | 0.58
  Cleveland, OH | 134 | 3 | 0.9 | 3.19 | 1.06 | 0.59 to 1.72 | 0.57
  Meeker, CO | 116 | 2 | 0.8 | 2.53 | 1.02 | 0.56 to 1.67 | 0.52
top 8 by lower bound (most clearly over-represented):
  place | pages | hits | expected | raw lift | estimate | 90% interval | P(above 1)
  Birmingham, AL | 1638 | 23 | 10.2 | 2.25 | 1.39 | 0.92 to 2.00 | 0.91
  New-York, NY | 7417 | 58 | 45.4 | 1.28 | 1.18 | 0.87 to 1.55 | 0.82
  Los Angeles, CA | 2440 | 27 | 15.7 | 1.72 | 1.29 | 0.87 to 1.82 | 0.86
  San Francisco, CA | 3641 | 31 | 23.9 | 1.30 | 1.14 | 0.78 to 1.59 | 0.73
  New Haven, CT | 1500 | 15 | 9.7 | 1.55 | 1.15 | 0.73 to 1.71 | 0.70
  Prescott, AZ | 118 | 5 | 0.7 | 7.00 | 1.16 | 0.67 to 1.85 | 0.68
  Texarkana, AR | 612 | 6 | 3.9 | 1.53 | 1.05 | 0.61 to 1.66 | 0.56
  Flagstaff, AZ | 224 | 4 | 1.7 | 2.30 | 1.06 | 0.60 to 1.71 | 0.58
top 5 by upper bound (most clearly under-represented):
  place | pages | hits | expected | raw lift | estimate | 90% interval | P(above 1)
  Washington, DC | 7011 | 30 | 49.4 | 0.61 | 0.71 | 0.49 to 0.99 | 0.05
  Waterbury, CT | 1443 | 4 | 9.9 | 0.40 | 0.77 | 0.44 to 1.25 | 0.20
  Tombstone, AZ | 726 | 1 | 5.7 | 0.17 | 0.78 | 0.43 to 1.30 | 0.23
  Phoenix, AZ | 1460 | 6 | 10.1 | 0.59 | 0.84 | 0.49 to 1.32 | 0.27
  Ocala, FL | 656 | 0 | 4.2 | 0.00 | 0.79 | 0.42 to 1.33 | 0.24

## boll_weevil: "boll weevil"  1895-01-01 to 1930-12-31 by year (36 buckets)
hits 21782  pages 3699444  places with pages 262  with hits 164  coverage cells 3326  scoring 4016 us
raw hits vs pages: spearman 0.759 (all places)  R^2 of log-log 0.326 (places with hits)
raw hits vs titles (whole corpus): spearman 0.165  R^2 of log-log 0.059
share-of-pages colour today: max 0.1250 at Columbia, SC (8 pages, 1 hits); 1 of 164 places with hits are in the top half of the scale
dispersion phi 4.85 from 76 places
prior: alpha 0.519  mean 1.087  (prior sd of lift 1.51)  scored with shape 0.510
90% interval above 1 / below 1 / includes 1: 34 / 104 / 124  (phi = 1: 41 / 130)
top 10 that are also among the 10 places with most pages: raw hits 4  share of pages 1  estimate 2  lower bound 2
top 10 by share of pages: 4 have fewer pages than the median place (3417)
without time standardization: 22 places change flag; largest gap Deland, FL expected 7.0 with time vs 28.2 without (0.25x)
with the place in its own reference rate: 0 places change flag; Washington, DC raw lift 0.360 vs 0.284 against the others
top 5 by raw hits:
  place | pages | hits | expected | raw lift | estimate | 90% interval | P(above 1)
  Montgomery, AL | 135189 | 6709 | 706.8 | 9.49 | 9.46 | 9.05 to 9.89 | 1.00
  Birmingham, AL | 162914 | 4396 | 903.8 | 4.86 | 4.85 | 4.59 to 5.12 | 1.00
  Washington, DC | 841532 | 1867 | 6580.1 | 0.28 | 0.28 | 0.26 to 0.31 | 0.00
  Pine Bluff, AR | 64596 | 792 | 315.5 | 2.51 | 2.49 | 2.19 to 2.83 | 1.00
  New-York, NY | 325934 | 527 | 1871.2 | 0.28 | 0.28 | 0.24 to 0.33 | 0.00
top 5 by raw lift:
  place | pages | hits | expected | raw lift | estimate | 90% interval | P(above 1)
  Magnolia, AR | 2392 | 144 | 12.9 | 11.18 | 9.55 | 6.96 to 12.72 | 1.00
  Montgomery, AL | 135189 | 6709 | 706.8 | 9.49 | 9.46 | 9.05 to 9.89 | 1.00
  Pulaski Heights, AR | 2534 | 139 | 17.8 | 7.83 | 6.98 | 5.06 to 9.34 | 1.00
  Marianna, AR | 6356 | 342 | 43.9 | 7.79 | 7.42 | 6.06 to 8.97 | 1.00
  Osceola, AR | 4955 | 220 | 30.0 | 7.34 | 6.85 | 5.31 to 8.66 | 1.00
top 8 by lower bound (most clearly over-represented):
  place | pages | hits | expected | raw lift | estimate | 90% interval | P(above 1)
  Montgomery, AL | 135189 | 6709 | 706.8 | 9.49 | 9.46 | 9.05 to 9.89 | 1.00
  Magnolia, AR | 2392 | 144 | 12.9 | 11.18 | 9.55 | 6.96 to 12.72 | 1.00
  Marianna, AR | 6356 | 342 | 43.9 | 7.79 | 7.42 | 6.06 to 8.97 | 1.00
  Osceola, AR | 4955 | 220 | 30.0 | 7.34 | 6.85 | 5.31 to 8.66 | 1.00
  Pulaski Heights, AR | 2534 | 139 | 17.8 | 7.83 | 6.98 | 5.06 to 9.34 | 1.00
  Monticello, AR | 4223 | 163 | 22.3 | 7.32 | 6.68 | 4.96 to 8.75 | 1.00
  Ashdown, AR | 3419 | 174 | 26.1 | 6.67 | 6.17 | 4.63 to 8.01 | 1.00
  Birmingham, AL | 162914 | 4396 | 903.8 | 4.86 | 4.85 | 4.59 to 5.12 | 1.00
top 5 by upper bound (most clearly under-represented):
  place | pages | hits | expected | raw lift | estimate | 90% interval | P(above 1)
  Skagway, AK | 24298 | 0 | 145.8 | 0.00 | 0.01 | 0.00 to 0.06 | 0.00
  Pittsburg, PA | 16100 | 0 | 109.3 | 0.00 | 0.01 | 0.00 to 0.08 | 0.00
  Tucson, AZ | 20181 | 2 | 121.8 | 0.02 | 0.02 | 0.00 to 0.11 | 0.00
  Nome, AK | 22164 | 6 | 158.6 | 0.04 | 0.04 | 0.01 to 0.13 | 0.00
  Valdez, AK | 9457 | 0 | 60.0 | 0.00 | 0.02 | 0.00 to 0.15 | 0.00

## klondike: klondike  1896-01-01 to 1901-12-31 by month (72 buckets)
hits 22132  pages 437180  places with pages 163  with hits 120  coverage cells 5107  scoring 2514 us
raw hits vs pages: spearman 0.883 (all places)  R^2 of log-log 0.779 (places with hits)
raw hits vs titles (whole corpus): spearman 0.156  R^2 of log-log 0.030
share-of-pages colour today: max 0.7500 at Batesville, AR (4 pages, 3 hits); 6 of 120 places with hits are in the top half of the scale
dispersion phi 3.94 from 67 places
prior: alpha 1.229  mean 1.401  (prior sd of lift 1.26)  scored with shape 1.207
90% interval above 1 / below 1 / includes 1: 28 / 28 / 107  (phi = 1: 38 / 44)
same search by year: phi 3.94 from 67 places, above/below 28 / 28, 2 places change flag
top 10 that are also among the 10 places with most pages: raw hits 8  share of pages 0  estimate 0  lower bound 1
top 10 by share of pages: 7 have fewer pages than the median place (463)
without time standardization: 17 places change flag; largest gap Milford, DE expected 5.1 with time vs 18.6 without (0.27x)
with the place in its own reference rate: 0 places change flag; Washington, DC raw lift 0.875 vs 0.847 against the others
top 5 by raw hits:
  place | pages | hits | expected | raw lift | estimate | 90% interval | P(above 1)
  San Francisco, CA | 39426 | 3157 | 2007.6 | 1.57 | 1.57 | 1.48 to 1.66 | 1.00
  Washington, DC | 78127 | 3144 | 3710.3 | 0.85 | 0.85 | 0.80 to 0.90 | 0.00
  New-York, NY | 67799 | 2838 | 3672.9 | 0.77 | 0.77 | 0.73 to 0.82 | 0.00
  Los Angeles, CA | 13577 | 1344 | 889.9 | 1.51 | 1.51 | 1.38 to 1.65 | 1.00
  Skagway, AK | 1900 | 1109 | 46.0 | 24.11 | 22.52 | 20.39 to 24.80 | 1.00
top 5 by raw lift:
  place | pages | hits | expected | raw lift | estimate | 90% interval | P(above 1)
  Skagway, AK | 1900 | 1109 | 46.0 | 24.11 | 22.52 | 20.39 to 24.80 | 1.00
  Douglas City, AK | 668 | 249 | 22.4 | 11.13 | 9.79 | 7.92 to 11.95 | 1.00
  Eagle City, AK | 2 | 1 | 0.1 | 9.60 | 1.29 | 0.18 to 4.32 | 0.61
  Seattle, WA | 8 | 3 | 0.4 | 8.38 | 1.73 | 0.36 to 4.93 | 0.74
  Skaguay Alaska, AK | 54 | 36 | 4.8 | 7.51 | 4.82 | 2.73 to 7.76 | 1.00
top 8 by lower bound (most clearly over-represented):
  place | pages | hits | expected | raw lift | estimate | 90% interval | P(above 1)
  Skagway, AK | 1900 | 1109 | 46.0 | 24.11 | 22.52 | 20.39 to 24.80 | 1.00
  Douglas City, AK | 668 | 249 | 22.4 | 11.13 | 9.79 | 7.92 to 11.95 | 1.00
  Fort Wrangel, AK | 476 | 209 | 31.0 | 6.74 | 6.17 | 4.89 to 7.66 | 1.00
  Skaguay Alaska, AK | 54 | 36 | 4.8 | 7.51 | 4.82 | 2.73 to 7.76 | 1.00
  Arizona City, AZ | 1188 | 141 | 48.7 | 2.90 | 2.78 | 2.09 to 3.60 | 1.00
  Mineral Park, AZ | 1561 | 168 | 70.1 | 2.40 | 2.33 | 1.80 to 2.96 | 1.00
  Salisbury, CT | 1297 | 147 | 64.3 | 2.29 | 2.22 | 1.68 to 2.87 | 1.00
  Elbert, CO | 463 | 66 | 26.3 | 2.51 | 2.34 | 1.54 to 3.38 | 1.00
top 5 by upper bound (most clearly under-represented):
  place | pages | hits | expected | raw lift | estimate | 90% interval | P(above 1)
  San Diego, CA | 1242 | 0 | 60.4 | 0.00 | 0.06 | 0.01 to 0.21 | 0.00
  Phoenix, AZ | 13908 | 160 | 771.3 | 0.21 | 0.21 | 0.16 to 0.27 | 0.00
  Oklahoma City, OK | 1082 | 0 | 37.2 | 0.00 | 0.09 | 0.01 to 0.33 | 0.00
  Arizola, AZ | 3081 | 33 | 155.7 | 0.21 | 0.23 | 0.13 to 0.38 | 0.00
  Florence, CO | 3622 | 42 | 168.5 | 0.25 | 0.26 | 0.16 to 0.41 | 0.00

## mormon: mormon  1850-01-01 to 1930-12-31 by year (81 buckets)
hits 59718  pages 4623735  places with pages 336  with hits 261  coverage cells 4773  scoring 4355 us
raw hits vs pages: spearman 0.862 (all places)  R^2 of log-log 0.640 (places with hits)
raw hits vs titles (whole corpus): spearman 0.359  R^2 of log-log 0.146
share-of-pages colour today: max 0.5455 at Mariposa, CA (209 pages, 114 hits); 2 of 261 places with hits are in the top half of the scale
dispersion phi 3.29 from 153 places
prior: alpha 1.407  mean 1.172  (prior sd of lift 0.99)  scored with shape 1.396
90% interval above 1 / below 1 / includes 1: 53 / 86 / 197  (phi = 1: 67 / 125)
top 10 that are also among the 10 places with most pages: raw hits 6  share of pages 0  estimate 0  lower bound 0
top 10 by share of pages: 10 have fewer pages than the median place (2124)
without time standardization: 76 places change flag; largest gap Mariposa, CA expected 19.5 with time vs 2.7 without (7.21x)
with the place in its own reference rate: 1 places change flag; Washington, DC raw lift 0.790 vs 0.736 against the others
top 5 by raw hits:
  place | pages | hits | expected | raw lift | estimate | 90% interval | P(above 1)
  New-York, NY | 583771 | 9582 | 11910.7 | 0.80 | 0.80 | 0.78 to 0.83 | 0.00
  Washington, DC | 989067 | 8813 | 11976.5 | 0.74 | 0.74 | 0.71 to 0.76 | 0.00
  Phoenix, AZ | 121420 | 2953 | 1113.8 | 2.65 | 2.65 | 2.50 to 2.79 | 1.00
  San Francisco, CA | 208359 | 2778 | 2325.5 | 1.19 | 1.19 | 1.13 to 1.26 | 1.00
  Chicago, IL | 53408 | 2279 | 1377.2 | 1.65 | 1.65 | 1.55 to 1.76 | 1.00
top 5 by raw lift:
  place | pages | hits | expected | raw lift | estimate | 90% interval | P(above 1)
  Parsons, KS | 4 | 2 | 0.1 | 31.47 | 1.39 | 0.29 to 3.92 | 0.66
  Xenia, OH | 4 | 1 | 0.1 | 15.72 | 1.14 | 0.20 to 3.51 | 0.56
  Corsicana, TX | 4 | 1 | 0.1 | 15.72 | 1.14 | 0.20 to 3.51 | 0.56
  Macon, GA | 4 | 1 | 0.1 | 15.72 | 1.14 | 0.20 to 3.51 | 0.56
  Snowflake, AZ | 2398 | 151 | 12.7 | 11.89 | 9.29 | 7.24 to 11.70 | 1.00
top 8 by lower bound (most clearly over-represented):
  place | pages | hits | expected | raw lift | estimate | 90% interval | P(above 1)
  Flagstaff, AZ | 18728 | 1880 | 167.7 | 11.21 | 10.97 | 10.24 to 11.75 | 1.00
  Safford, AZ | 5635 | 518 | 51.7 | 10.02 | 9.37 | 8.20 to 10.65 | 1.00
  Snowflake, AZ | 2398 | 151 | 12.7 | 11.89 | 9.29 | 7.24 to 11.70 | 1.00
  Peach Springs, AZ | 1762 | 389 | 65.9 | 5.90 | 5.62 | 4.82 to 6.51 | 1.00
  Mariposa, CA | 209 | 114 | 19.5 | 5.86 | 5.03 | 3.77 to 6.54 | 1.00
  St. Johns, AZ | 10869 | 482 | 125.6 | 3.84 | 3.75 | 3.26 to 4.28 | 1.00
  Douglas, AZ | 25145 | 477 | 125.8 | 3.79 | 3.71 | 3.22 to 4.23 | 1.00
  Globe, AZ | 7554 | 201 | 51.6 | 3.89 | 3.68 | 2.97 to 4.51 | 1.00
top 5 by upper bound (most clearly under-represented):
  place | pages | hits | expected | raw lift | estimate | 90% interval | P(above 1)
  San Diego, CA | 8964 | 1 | 123.2 | 0.01 | 0.04 | 0.01 to 0.11 | 0.00
  Pittsburg, PA | 16100 | 0 | 90.6 | 0.00 | 0.04 | 0.01 to 0.13 | 0.00
  Deland, FL | 9665 | 11 | 215.2 | 0.05 | 0.07 | 0.03 to 0.13 | 0.00
  Texarkana, AR | 9569 | 16 | 129.3 | 0.12 | 0.15 | 0.07 to 0.27 | 0.00
  Newtown, CT | 11767 | 44 | 183.9 | 0.24 | 0.25 | 0.16 to 0.38 | 0.00

## baking_powder: "baking powder"  1880-01-01 to 1920-12-31 by year (41 buckets)
hits 111479  pages 3190497  places with pages 282  with hits 209  coverage cells 3466  scoring 5175 us
raw hits vs pages: spearman 0.916 (all places)  R^2 of log-log 0.767 (places with hits)
raw hits vs titles (whole corpus): spearman 0.302  R^2 of log-log 0.085
share-of-pages colour today: max 0.2628 at Bessemer, CO (392 pages, 103 hits); 5 of 209 places with hits are in the top half of the scale
dispersion phi 11.62 from 155 places
prior: alpha 2.546  mean 1.204  (prior sd of lift 0.75)  scored with shape 2.524
90% interval above 1 / below 1 / includes 1: 63 / 33 / 186  (phi = 1: 102 / 88)
top 10 that are also among the 10 places with most pages: raw hits 8  share of pages 0  estimate 0  lower bound 0
top 10 by share of pages: 8 have fewer pages than the median place (2041)
without time standardization: 44 places change flag; largest gap Hardy, AR expected 17.3 with time vs 4.8 without (3.65x)
with the place in its own reference rate: 0 places change flag; Washington, DC raw lift 0.625 vs 0.581 against the others
top 5 by raw hits:
  place | pages | hits | expected | raw lift | estimate | 90% interval | P(above 1)
  Washington, DC | 562296 | 11954 | 20573.7 | 0.58 | 0.58 | 0.55 to 0.61 | 0.00
  New Haven, CT | 108487 | 7219 | 3982.2 | 1.81 | 1.81 | 1.69 to 1.93 | 1.00
  New-York, NY | 322129 | 6209 | 13541.9 | 0.46 | 0.46 | 0.43 to 0.49 | 0.00
  Los Angeles, CA | 97955 | 4919 | 4813.2 | 1.02 | 1.02 | 0.94 to 1.11 | 0.67
  Sacramento, CA | 39799 | 4782 | 2323.0 | 2.06 | 2.05 | 1.89 to 2.22 | 1.00
top 5 by raw lift:
  place | pages | hits | expected | raw lift | estimate | 90% interval | P(above 1)
  Laurel, DE | 8 | 2 | 0.3 | 6.78 | 1.12 | 0.31 to 2.75 | 0.57
  Wilmington, NC | 4 | 1 | 0.1 | 6.74 | 1.08 | 0.30 to 2.70 | 0.55
  Oakland, CA | 104 | 12 | 2.6 | 4.57 | 1.39 | 0.48 to 3.06 | 0.71
  Portland, OR | 416 | 46 | 11.2 | 4.10 | 2.01 | 0.96 to 3.64 | 0.94
  Williams, AZ | 5122 | 474 | 132.1 | 3.59 | 3.19 | 2.46 to 4.06 | 1.00
top 8 by lower bound (most clearly over-represented):
  place | pages | hits | expected | raw lift | estimate | 90% interval | P(above 1)
  Williams, AZ | 5122 | 474 | 132.1 | 3.59 | 3.19 | 2.46 to 4.06 | 1.00
  Elbert, CO | 7989 | 682 | 212.8 | 3.20 | 2.98 | 2.40 to 3.66 | 1.00
  Irwin, CO | 4626 | 430 | 124.5 | 3.45 | 3.06 | 2.32 to 3.93 | 1.00
  Putnam, CT | 11934 | 832 | 283.1 | 2.94 | 2.79 | 2.29 to 3.36 | 1.00
  Willcox, AZ | 6717 | 738 | 247.0 | 2.99 | 2.81 | 2.28 to 3.42 | 1.00
  Waterbury, CT | 46475 | 4500 | 2065.8 | 2.18 | 2.17 | 1.99 to 2.35 | 1.00
  Cañon City, CO | 6022 | 486 | 178.5 | 2.72 | 2.52 | 1.95 to 3.20 | 1.00
  Sacramento, CA | 39799 | 4782 | 2323.0 | 2.06 | 2.05 | 1.89 to 2.22 | 1.00
top 5 by upper bound (most clearly under-represented):
  place | pages | hits | expected | raw lift | estimate | 90% interval | P(above 1)
  San Diego, CA | 8152 | 0 | 362.2 | 0.00 | 0.07 | 0.02 to 0.17 | 0.00
  Cordova, AK | 10564 | 21 | 225.8 | 0.09 | 0.19 | 0.07 to 0.38 | 0.00
  Indianapolis, IN | 12655 | 140 | 584.3 | 0.24 | 0.27 | 0.17 to 0.41 | 0.00
  Skagway, AK | 23056 | 155 | 628.4 | 0.25 | 0.28 | 0.18 to 0.41 | 0.00
  Washington City, DC | 16745 | 227 | 817.3 | 0.28 | 0.30 | 0.21 to 0.42 | 0.00

## Timing
3,000 places with no difference between them: alpha 1000000, scoring 49 ms
```

</details>

## 11.14 Phase 1 as built

- **Labels.** The toggle reads "Pages" and "Relative rate". "Pages" is the word the site already uses for raw counts (summary, tooltip, table, legend). "Relative rate" is short enough for a two-button toggle on a phone, which "Compared with other places" is not; the legend line says what it is compared with, and an info button explains the shrinkage and the faded circles.
- **The older share of pages.** Removed in October 2026. Phase 1 kept it for `norm=rel` permalinks, with a third button shown only while it was selected; now those links open on Pages and the toggle has two buttons. The place table keeps its "Share of pages published" column.
- **Languages.** `/v1/places` lists the languages of each place's titles. Every place is fitted, scored and drawn the same way. Where any of a place's titles isn't in English, its tooltip, list entry, table row and CSV row say so ("Papers in Serbian and English"), so a reader can see why it may read low for an English term. In the lists, an info button beside the label says how many of the place's titles are in each language and what share of its titles that is (from `language_titles` in `/v1/places`; a title in several languages counts in each), explains that these are the languages the titles are catalogued in across the collection, not the language of the matched pages, and points to the language filter for a like-with-like comparison.
- **Per-language baselines** (issue 86). The release writes `language_baselines.json` next to `baselines.json`: the same pages per place and day, split into one set per distinct list of languages a title has (`["eng"]`, `["eng","ger"]`, …). A page is in one set, so for `lang=eng,ger` the API sums every set that has English or German and no page is counted twice; a title in English and German is in both `lang=eng` and `lang=ger` but once in their union. `/v1/aggregate` returns `series.baseline`, `total.baseline_pages` and `cube.baseline_ref` for that scope, and `/v1/coverage` takes `lang` (06 §6.3.3). The site then scores the relative rate the same way as without a filter, shows the place table's "Share of pages published" column and offers the CSV. It leaves out the "Papers in X and English" line, since the baseline already compares like with like. A language's places can be few: German reaches 2 of the 6 places in the fixtures, and below the minimum (10 places with pages on the site, 5 in the end-to-end build) the view says so. Titles that list no language have no set, as no filter matches them.
- **The query's language** (#237). Four of the five places most clearly below 1× for "Korea", 1945 to 1956, had papers in other languages (Yiddish, French, Croatian, and one in Spanish, Italian and English): an English word can't match "Corée" or "Koreja". Without a `lang` filter, the API now compares a query whose words are English with the pages of titles that list English, using the per-language baselines above, and says so in the aggregate's `baseline` (`{"languages": ["eng"], "why": "query_language"}`). The rule (06 §6.3.3, `crates/usnm-core/src/query_language.rs`): each side of any `OR` has a word at least as frequent in English as in each of the corpus's other Latin-script languages (wordfreq, folded as the index folds), and its other words are English words no such language uses more than 10 times as often, or digits. A query it can't place keeps every page, as before. The hits are not filtered: `lang=eng` makes cold searches about 40% slower and prunes nothing, so an English search's matches still include the few in titles catalogued in other languages, mostly bilingual papers, and the relative rate's info button says so. Mixed places are compared on their English titles' pages, so a bilingual paper's other-language matches count against its English pages only, and a place whose papers are all in other languages drops out of the view. The examples that set a language keep it. Page language from the OCR audit (#283) can replace the titles' languages after a rebuild that records it.
- **Where it runs.** The fit runs in a web worker (`web/src/engine/skew.worker.ts`), falling back to the main thread where workers aren't available; frames are scored on the main thread from prefix sums (§11.7). Each new search shows pages with a notice while its fit runs; a refit of the same search (same index version, query and buckets) keeps the previous colours until it is ready, and switching to Pages and back doesn't refit. When the coverage cube is missing or doesn't match the search's buckets, the map shows pages with a notice. A layer=heat permalink shows points in this view. With fewer than 10 places with pages between the search's dates (5 in the end-to-end build, §11.4.2) it says so and shows pages.
- **Shared vectors.** Ten generated cases (lone publishers, the zero-reference correction, reprints and drift by month, half a year by week, few hits by day with the fallback `phi`, no difference with `alpha` at its bound, both sides of the three-year boundary including from 29 February, places left out of the fit, states with their own `phi`) and two recorded searches (cross of gold and Klondike, reduced to counts), each with playback frames for places and states, plus the special functions. Tolerances (relative unless noted): counts 1e-12, `phi` 1e-10, `1/alpha`, `mu` and the shape 1e-5, estimates and bounds 1e-5, P(above 1) 1e-6 absolute, special functions 1e-12. The largest differences measured between the port and the Rust were about 1e-7 for place bounds, 1e-6 for one Klondike state's lower bound (33 states, a flatter fit) and 6e-8 for `1/alpha`; `phi` and the counts were identical.
- **Not built yet:** the state choropleth (state scores are a table), a citation in the share dialog (the dialog only copies the link; the legend names the index version).
