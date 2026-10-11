# 05: Search & Document Storage

This is the most consequential decision in the design. The legacy system's single Solr core did two jobs: it was the document store and it was the aggregation engine. Here we separate **storage** (Azure Blob Storage, see [04](04-data-sources-and-ingestion.md)) from **retrieval** (a full-text engine whose index can be rebuilt from the lake), and evaluate the retrieval options.

## 5.1 Workload characterization

| Query            | Description                                                                                                                          | Share of traffic | Shape                                                                                                                             |
| ---------------- | ------------------------------------------------------------------------------------------------------------------------------------ | ---------------- | --------------------------------------------------------------------------------------------------------------------------------- |
| **Q1 Aggregate** | Text query + date range (+ filters) → total hits; counts per place × time bucket; counts per time bucket; first match date per place | ~70%             | Full-text match, then **nested aggregation** over _all_ matches: `terms(place_id, size≤5,000) → histogram(bucket)` and `min(day)` |
| **Q2 Hits**      | Same query restricted to one place (or title), sorted by date, paginated 50 at a time, with highlighted snippets                     | ~25%             | Top-N by a sort field + snippet generation                                                                                        |
| **Q3 Lookup**    | Page metadata by `doc_id`                                                                                                            | ~5%              | Point read (usually served from Parquet or reference data, not the engine)                                                        |
| **Ingest**       | Append-mostly; periodic batch replacement (OCR reprocessing); rare full rebuilds                                                     | Offline          | Bulk                                                                                                                              |

**Characteristics that drive the choice:**

- **Relevance ranking barely matters.** Results are sorted by **date**, and the map needs **counts**. What matters is how fast the engine can _aggregate over millions of matches_, not BM25 quality.
- **Phrase and proximity are essential.** The index must store term **positions**.
- **Queries are deterministic and data changes weekly at most**, so caching by URL is extremely effective.
- **Traffic is low with spikes.** A fixed-cost engine sized for spikes is wasted most of the month.
- **Corpus size:** ~23M pages; **~450–800 GB raw text**; the index with positions is expected to be **0.5–1.2× raw text** depending on the engine. We budget **~1 TB**.

## 5.2 Requirements matrix

| #   | Requirement                                                                                                                 | Weight |
| --- | --------------------------------------------------------------------------------------------------------------------------- | ------ |
| R1  | Phrase and proximity search with positions over ~1 TB                                                                       | Must   |
| R2  | Nested aggregation _place × time bucket_ over all matches, GA (not preview)                                                 | Must   |
| R3  | Date range filtering that prunes efficiently                                                                                | Must   |
| R4  | Sort by date + paginate + highlight snippets                                                                                | Must   |
| R5  | Fuzzy term matching (OCR tolerance)                                                                                         | Should |
| R6  | Steady-state cost that fits the **< $80/mo total** budget (search compute + index storage ideally ≤ $50)                    | Must   |
| R7  | **No IaaS VMs** (no VMs, VM Scale Sets, AKS or Batch pools); managed or containerized on Azure PaaS; low operational effort | Must   |
| R8  | Re-index from the lake in ≤ 24 h                                                                                            | Should |
| R9  | Azure-native support and SLA                                                                                                | Nice   |
| R10 | Path to vector/semantic search                                                                                              | Nice   |

## 5.3 Options evaluated

### Option A: Azure AI Search (fully managed PaaS)

- **Fit:** Mature managed service with full Lucene syntax (phrase, proximity `"a b"~5`, fuzzy `term~1`, boolean), highlighting, facets, a native Blob indexer, vector and semantic search. SLA with 2 or more replicas. Managed-identity RBAC.
- **Capacity:** For services created after May 2024, partition storage is **S1 160 GB, S2 512 GB, S3 1 TB, L1 2 TB, L2 4 TB**. A ~1 TB index fits in **1 × L1** or **1–2 × S3**.
- **Aggregations (R2):** GA facets give counts per field (`place_id,count:5000`) and date interval facets (`date,interval:year`). They do **not** nest. **Hierarchical facets** (`place_id > year`) and facet metrics exist only in **preview** APIs (2025-03-01-preview and later), with no GA timeline and no SLA. GA-only workarounds:
  1. a precomputed composite facet field `place_year` (`"P00412|1896"`) and `place_month`, requested with large `count` values. This works, but bloats the index, and large facet counts are slow;
  2. fan out one facet query per time bucket (up to about 200 queries for yearly buckets), which is too expensive;
  3. accept the preview API for this one call.
- **Cost (list price, pay-as-you-go, East US; confirm in the Azure pricing calculator):**

  | Config                                | Storage | Approx. monthly |
  | ------------------------------------- | ------- | --------------- |
  | L1 × 1 partition × 1 replica (no SLA) | 2 TB    | **~$2,800**     |
  | L1 × 1 × 2 replicas (read SLA)        | 2 TB    | **~$5,600**     |
  | S3 × 1 × 1 (tight fit at 1 TB)        | 1 TB    | ~$1,960         |
  | S3 × 2 × 1                            | 2 TB    | ~$3,900         |

  Storage-optimized (L) tiers also have **higher query latency** by design, which matters for large aggregations.

- **Preview features are acceptable** to the maintainer, so the preview hierarchical facets would satisfy R2. Cost is the blocker: the cheapest tier that fits the index (S3 or L1) is about $2–2.8k/month, **25–35× the lean budget**. The preview **Serverless Developer** tier caps an index at 1 GB, so it can't hold the corpus.
- **Verdict:** Functionally strong and the least operations work, but far outside the budget. **This is the recommended alternative if the budget allows** (about $35–70k per year), or if semantic search (R10) becomes a priority.

### Option B: Quickwit on Azure Container Apps, index on Azure Blob ✅ _recommended_

- **What:** Quickwit is a Rust search engine built on **Tantivy**, relicensed to **Apache-2.0** after Datadog acquired it in January 2025. It **decouples compute from storage**: index _splits_ live on **Azure Blob Storage**, and stateless searchers read them directly, using hotcache footers and a local split cache.
- **Fit:** Elasticsearch-compatible aggregations (**terms → histogram/date_histogram nesting, min/max, cardinality** are GA). Phrase queries with slop (positions have to be enabled per field). Boolean queries, snippets and sorting on fast fields. Time-partitioned splits prune date-range queries well, and publication date is a natural timestamp. Ingest API, delete tasks, and a file-backed metastore on Blob (no database needed). The file-backed metastore allows **one writer only**; serving searchers open it read-only and poll for changes ([08 §8.4.1](08-azure-infrastructure.md#841-quickwit-metastore-one-writer-many-readers)).
- **Azure PaaS usage:** Container Apps (managed, KEDA autoscaling, managed identity, managed OTel agent), Blob Storage (Hot), and Container Apps Jobs for indexing. There are no VMs and no Kubernetes to operate.
- **Cost:** Index on Blob Hot at about **$20 per TB-month**. The lean profile runs Quickwit as a sidecar next to the API in one always-warm Container Apps replica. It started at 1 vCPU / 2 GiB (roughly **$15–45 per month** at idle rates) moved to 2 vCPU / 4 GiB in October 2026 so that uncached searches across the full date range finish sooner, and to 3.75 vCPU / 7.5 GiB (about $95 a month idle for the replica) on 9 October 2026, when the American Stories index made searches CPU-bound (#251). The growth profile uses a 4 vCPU / 8 GiB searcher at about $100–300. Ingest compute is paid only while jobs run, on Spot for the backfill.
- **Risks:**
  1. Quickwit is tuned for logs and traces. Documents averaging 20–35 KB and very high-frequency terms need benchmarking (Spike S-2).
  2. Pre-1970 timestamps need validation. The mitigation is integer bucket fields (`day`, `ym`, `year`), which the design uses anyway (§5.5).
  3. Project cadence after the acquisition. The mitigation is that it's Apache-2.0 and Tantivy is independently maintained; the `SearchBackend` trait keeps it swappable.
  4. Fuzzy matching through the Quickwit query DSL needs validation.
  5. Container Apps ephemeral disk limits the size of the split cache. Mitigation: a Dedicated workload profile if the cache misses hurt latency.
- **Verdict:** Meets R1–R4, R6 and R7 and is Rust-native. **Recommended**, subject to Spike S-2.

### Option C: Azure Cosmos DB for NoSQL with full-text search

- Full-text search and BM25 (keyword and phrase) are **GA**, with hybrid vector search. Excellent for operational data.
- **No good fit for R2.** Aggregating millions of matching documents with `GROUP BY` costs request units that grow with the number of matches, and multi-partition aggregation over roughly 1 TB would be slow and expensive. Phrase search on 30 KB OCR bodies at this scale is untested.
- **Verdict:** Not the search backend. **Reserved for future user features** (saved searches, collections) as Cosmos serverless.

### Option D: Elastic Cloud on Azure (Azure Native ISV Service)

- Has every feature we need (nested aggregations, highlighting, fuzzy, vectors). **Searchable snapshots on Azure Blob** (frozen tier) could make ~1 TB cheap to store, but frozen-tier aggregations over millions of documents take multiple seconds. A hot tier of ~1 TB runs about **$1,500–3,000 per month**.
- **Verdict:** A credible fallback if Quickwit fails S-2 and AI Search is unaffordable. It is a fully managed SaaS offering billed through the Azure Marketplace; Elastic runs the infrastructure and we manage no VMs. A self-hosted Elasticsearch/OpenSearch cluster on VMs is **not** an option.

### Option E: Azure Database for PostgreSQL Flexible Server (tsvector / GIN)

- Handles phrase search (`<->`) and SQL aggregation well at small scale. However, a **`tsvector` records at most 16,383 positions**, and word positions above that are clamped. Dense broadsheet pages exceed that, so **phrase matches silently fail** on the ends of long pages. Also, `ts_headline` over 30 KB pages is slow, and GIN over ~1 TB needs a large, always-on server (~$1k+/mo).
- **Verdict:** Rejected (fails R1 at our document sizes).

### Option F: Embedded Tantivy inside the Rust API

- Fastest and cheapest per query (in-process, no network hop). But it needs a **~1 TB local index on a persistent disk**, which Container Apps doesn't provide at that size. It would mean VMs or AKS, which violates R7.
- **Verdict:** Rejected for the hosted system, because it would require IaaS VMs. It remains useful **outside the cloud architecture** as an offline research build: a single binary plus an index on a researcher's own machine.

### Scoring summary

|                         | R1  | R2              | R3  | R4  | R5          | R6 cost    | R7 ops | R9     | R10 | Result                     |
| ----------------------- | --- | --------------- | --- | --- | ----------- | ---------- | ------ | ------ | --- | -------------------------- |
| A AI Search             | ✅  | ✅ (preview OK) | ✅  | ✅  | ✅          | ❌ $2–5.6k | ✅✅   | ✅     | ✅  | Growth-profile alternative |
| **B Quickwit/ACA/Blob** | ✅  | ✅              | ✅  | ✅  | ⚠️ validate | ✅ ~$25–65 | ✅     | ⚠️ OSS | ⚠️  | **Recommended**            |
| C Cosmos DB             | ⚠️  | ❌              | ✅  | ⚠️  | ⚠️          | ⚠️         | ✅✅   | ✅     | ✅  | Future user data           |
| D Elastic on Azure      | ✅  | ✅              | ✅  | ✅  | ✅          | ❌/⚠️      | ✅     | ✅     | ✅  | Fallback                   |
| E PostgreSQL            | ❌  | ✅              | ✅  | ⚠️  | ⚠️          | ⚠️         | ✅     | ✅     | ✅  | Rejected                   |
| F Embedded Tantivy      | ✅  | ✅              | ✅  | ✅  | ✅          | ✅         | ❌     | ❌     | ⚠️  | Offline only               |

**Decision gate (Spike S-2):** index a **1M-page sample** into Quickwit running at the lean size (1 vCPU / 2 GiB sidecar), and repeat at 2 vCPU / 4 GiB. Run the benchmark query set (§5.8) and project the results to 23M pages. **Accept the lean size if** p95 for Q1 is ≤ 2 s on typical queries and ≤ 15 s on the high-frequency set, and the correctness checks pass. **Otherwise** move to 2 vCPU / 4 GiB (still under $80), and if that fails, revisit the budget. A comparison run on AI Search S1 is optional; it is informative, but unaffordable at full scale.

## 5.4 Backend abstraction

The Rust API depends on a narrow trait, so the engine choice is reversible:

```rust
#[async_trait]
pub trait SearchBackend: Send + Sync {
    async fn aggregate(&self, q: &CompiledQuery, spec: &AggSpec) -> Result<AggResult, SearchError>;
    async fn hits(&self, q: &CompiledQuery, page: &HitPage) -> Result<HitsResult, SearchError>;
    async fn health(&self) -> Result<BackendHealth, SearchError>;
    fn capabilities(&self) -> Capabilities; // fuzzy, proximity max slop, nested aggs, max buckets
}
```

`CompiledQuery` is produced from the backend-neutral AST (see [06](06-api-design.md)) by a per-backend **translator**. Both translators are covered by the same golden-file tests.

## 5.5 Index schema

The same logical fields exist in both engines. **Integer bucket fields** are used for all time aggregation. This avoids calendar edge cases before 1970 and makes week, month and year bucketing exact and engine-independent.

| Field            | Type             | Indexed                   | Fast/Facet/Sort | Stored            | Purpose                                                                                                                                              |
| ---------------- | ---------------- | ------------------------- | --------------- | ----------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------- |
| `doc_id`         | keyword          | key                       | ✅              | ✅                | Identity                                                                                                                                             |
| `text`           | text (positions) | ✅ analyzer `usnm_text`   | –               | ✅ (for snippets) | Search. LoC's text, or with `--ja-latin` the Latin-script text of our Japanese OCR on the pages the snapshot's `ja_latin.json` lists (04 §4.8, #203) |
| `text_cg`        | text (positions) | ✅ tokenizer `whitespace` | –               | –                 | Phrases holding common words (§5.5.3)                                                                                                                |
| `text_as`        | text (positions) | ✅ analyzer `usnm_text`   | –               | ✅ (for snippets) | American Stories' text of the page, searched with `text` (§5.5.4)                                                                                    |
| `text_as_cg`     | text (positions) | ✅ tokenizer `whitespace` | –               | –                 | `text_as`'s common-word pairs (§5.5.3, §5.5.4)                                                                                                       |
| `text_all`       | text (positions) | ✅ tokenizer `whitespace` | –               | –                 | Only with one searched field (§5.5.7): both texts' words, with a gap between them                                                                    |
| `text_all_cg`    | text (positions) | ✅ tokenizer `whitespace` | –               | –                 | Only with one searched field: its common-word pairs (§5.5.3, §5.5.7)                                                                                 |
| `date`           | date             | ✅                        | ✅              | ✅                | Quickwit timestamp field (pre-1970 confirmed in S-2)                                                                                                 |
| `day`            | u32              | ✅                        | ✅ sort         | ✅                | Days since 1700-01-01; day/week buckets; range filter; hit order                                                                                     |
| `sort_key`       | u64              | –                         | ✅ sort         | –                 | `title ordinal << 32 \| edition << 16 \| seq`: stable hit order within a day (Quickwit can't sort on text, §5.5.1)                                   |
| `ym`             | u32              | ✅                        | ✅              | –                 | `year*12 + (month-1)`; month buckets                                                                                                                 |
| `year`           | u16              | ✅                        | ✅              | –                 | Year buckets                                                                                                                                         |
| `decade`         | u16              | ✅                        | – (split tag)   | –                 | Only in versions laid out by decade (§5.5.5): the split partition, and the tag a date-limited search prunes splits on                                |
| `place_id`       | keyword          | ✅                        | ✅ facet        | ✅                | Map aggregation                                                                                                                                      |
| `place_shard`    | u8               | ✅                        | ✅              | –                 | `place ordinal mod 8`, used to shard large cube aggregations (§5.7)                                                                                  |
| `lccn`           | keyword          | ✅                        | ✅              | ✅                | Title filter and hits                                                                                                                                |
| `state`          | keyword          | ✅                        | ✅              | –                 | Filter and choropleth                                                                                                                                |
| `language`       | keyword[]        | ✅                        | ✅              | –                 | Filter                                                                                                                                               |
| `front_page`     | bool             | ✅                        | ✅              | –                 | Filter                                                                                                                                               |
| `edition`, `seq` | u16              | –                         | –               | ✅                | Link building                                                                                                                                        |
| `batch`          | keyword          | ✅                        | –               | –                 | Delete-by-batch on reprocessing                                                                                                                      |

A version with one searched field (§5.5.7) has `text_all` and `text_all_cg` instead of `text_cg` and `text_as_cg`, and keeps `text` and `text_as` stored but not indexed.

**Analyzer `usnm_text`:** a Unicode word tokenizer, then lowercase, then ASCII folding, then removal of tokens longer than 40 characters (OCR garbage). **No stemming and no stop words.** Historical exactness matters, and stop words are needed for phrases such as "cross of gold". Words are split before they are folded, so `½` is the one token `½` (ASCII folding has no entry for it), and `1/2` is `1` and `2`. ASCII folding has no entry for `㊀` either, so `㊀` and `a㊀` are each one token. `usnm_core::text::tokenize` does the same for the memory backend, the common-word pairs and queries with analyzer version 2 (#168, #241). Indexes built with version 1 have `½` as `1⁄2` in their pairs, and queries on them fold it so too (§5.5.3).

### 5.5.1 Quickwit index config (validated in S-2)

The config lives in [`infra/quickwit/pages-index.yaml`](../../infra/quickwit/pages-index.yaml). It is a template: the tooling substitutes `${INDEX_ID}` (`pages-base-20261001`, `pages-delta-20261008-1`, …) and `${INDEX_URI}` (`azure://qw-index/{id}`), so every base and delta shares one doc mapping. It has been checked against **Quickwit 0.9.1** (the pinned version) on the synthetic fixtures. Production runs our build of 0.9.1 ([ADR-0013](adr/0013-quickwit-from-our-fork.md)): one commit that pools the Azure Blob client's connections, with the same index format, queries and aggregations, so the findings below hold for it. CI checks them on upstream's 0.9.1 release, on local files:

```yaml
doc_mapping:
  mode: strict
  timestamp_field: date
  tokenizers:
    - { name: usnm_text, type: simple, filters: [remove_long, lower_caser, ascii_folding] }
  field_mappings:
    - { name: doc_id,     type: text,     tokenizer: raw, fast: true, stored: true }
    - { name: text,       type: text,     tokenizer: usnm_text, record: position, stored: true, fieldnorms: false }
    - { name: text_as,    type: text,     tokenizer: usnm_text, record: position, stored: true, fieldnorms: false }   # §5.5.4
    # text_cg and text_as_cg: tokenizer whitespace, positions, not stored (§5.5.3)
    - { name: date,       type: datetime, input_formats: ["%Y-%m-%d"], output_format: "%Y-%m-%d", fast: true, stored: true }
    - { name: day,        type: u64,      fast: true, indexed: true, stored: true }
    - { name: sort_key,   type: u64,      fast: true, indexed: false }   # hit order within a day
    # ym, year, place_id, place_shard, lccn, state, language, front_page, edition, seq, batch as in §5.5
indexing_settings:
  commit_timeout_secs: 30           # batch ingestion: cut splits at the 1 GiB heap or 30 s, not every 10 s
  docstore_blocksize: 131072        # 128 KB blocks (about 5 pages, not 40): a cold page of hits about 3x faster, index about 8% larger
  split_num_docs_target: 30000      # splits stop merging at 30,000 pages (at most about 2.4 GB with text_cg and text_as): the writer's memory caps them
  merge_policy:
    type: limit_merge
    merge_factor: 10
    max_merge_factor: 12
    max_merge_ops: 4
    maturation_period: 7d
    max_finalize_merge_operations: 5
  resources: { heap_size: 1GiB }
```

A full base built with `--single-text-field` replaces the four text fields with one searched field for both texts (§5.5.7, `crates/usnm-ingest/src/sink.rs`, `text_fields`); everything else is the same.

**What spike S-2 settled, and how it's checked.** CI's `quickwit` job loads the fixture base and delta into Quickwit 0.9.1, stops the writer, and serves them from a read-only polling searcher configured like production. `crates/usnm-search/tests/quickwit_parity.rs` then compares the Quickwit backend with the in-memory reference backend. It covers 15 query shapes × 6 filter sets × 4 bucket units: summaries, full cubes, place-shard partitions of the cube, hit pages and snippets, on both text layouts (§5.5.7).

| Item                   | Finding on 0.9.1                                                                                                                                                                                                                                                                                                                                                                                                                                                                | Consequence                                                                                                                                                                                             |
| ---------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Tokenizer              | `type: simple` plus `remove_long`, `lower_caser` and `ascii_folding` filters is accepted as written                                                                                                                                                                                                                                                                                                                                                                             | As §5.5                                                                                                                                                                                                 |
| Pre-1970 dates         | `datetime` with `%Y-%m-%d` input stores and returns 1890s dates correctly, and `date` works as the timestamp field. But every timestamp range on it (`start_timestamp`/`end_timestamp`, `date:[…]`, Elasticsearch `range`) matches **no page before April 1972**, with no error: Quickwit rewrites the range with bounds in nanoseconds, which it only reads for 1972 to 2242 (#158, October 2026)                                                                              | Buckets and date filters use the integer fields (`day`, `ym`, `year`). **Never filter on `date`**, and never send timestamps                                                                            |
| Multi-index search     | One request over base + delta returns exact counts and aggregations                                                                                                                                                                                                                                                                                                                                                                                                             | Versions are explicit index lists (08 §8.4.1)                                                                                                                                                           |
| Nested aggregations    | `terms(place_id) > histogram(day / ym / year)` and the `place_shard` split match the reference exactly                                                                                                                                                                                                                                                                                                                                                                          | §5.7 as designed                                                                                                                                                                                        |
| Sorting                | Quickwit **can't sort on text fields**, so `doc_id` can't be the tiebreak. A leading `-` in `sort_by` means **ascending**                                                                                                                                                                                                                                                                                                                                                       | Hits sort by `-day,-sort_key` (oldest first) or `day,sort_key` (newest first). `sort_key` = `title ordinal << 32 \| edition << 16 \| seq`, a numeric stand-in for (title, edition, page)                |
| Snippets               | `snippet_fields` is a comma-separated string, not an array. Fragments come back HTML-escaped with `<b>` highlights, one short fragment per field                                                                                                                                                                                                                                                                                                                                | Since #126 the API builds snippets from the stored `text` instead (06 §6.3.4), and the parity test compares them with the memory backend's exactly                                                      |
| Score sort             | `sort_by: _score` works with `fieldnorms: false`: on the fixtures it orders pages by how often they mention the word, with one inversion where the two indexes meet (rare words are weighted per split). `_score,-day` breaks ties oldest first; a third field (`-sort_key`) is refused ("sort by field must be up to 2 fields")                                                                                                                                                | `sort=relevant` on `/v1/hits` (#126)                                                                                                                                                                    |
| Phrases, slop, prefix  | Exact phrases (stop words included), `"a b"~n` and `word*` match the reference                                                                                                                                                                                                                                                                                                                                                                                                  | As §5.6                                                                                                                                                                                                 |
| Wildcards inside words | An unquoted word with `?` or `*` is a wildcard query matched against the index's words: `text:convent?on` and `text:conve*tion` match the reference. Quickwit's own snippets have no fragment at all for a wildcard or prefix query (`text:convent?on`, `text:silve*`; checked on 0.9.1, October 2026): it highlights only the terms a query lists, and wildcard and prefix queries list none. In a phrase the wildcards don't work (`text:"con*tion speaker"` matches nothing) | Allowed after at least 5 letters, as for a prefix, and refused inside quotes (06 §6.4, #124). The API marks the matched words in its own snippets, which the parity test checks on the Quickwit backend |
| **Fuzzy terms**        | **Not supported.** `term~1` parses but silently matches nothing, and the Elasticsearch-compatible API has no fuzzy query either                                                                                                                                                                                                                                                                                                                                                 | The Quickwit backend reports `fuzzy: false` and returns 422 for fuzzy queries rather than wrong counts. F-21 needs another approach; see [10 R-15](10-roadmap-and-risks.md#103-risk-register)           |

Still open in S-2: the 1M-page benchmark (§5.8, latency and memory at the sidecar size, split cache) and managed-identity Blob auth against a real account (08 §8.2).

**Splits and merges.** A cold search opens every split of every index it names, with several Blob round trips per split (the footer, then term dictionary, postings and fast-field ranges), so its fixed cost grows with the number of splits, whatever the match count. The first prod releases (September 2026) committed every 10 s, so the writer cut a split about every 10 s while it ingested; each release published about 30 s after ingest stopped and the job exited, so background merges never caught up. Two deltas also filled the writer's disk ("No space left on device"), which killed indexers and failed merges. Searches against them had a fixed cost of about 4.5 to 5 s while the 1 vCPU searcher peaked at 65% CPU.

The settings above, and the release, now make each index splits of about 33,000 pages, not thousands of small ones:

- Splits are cut when the indexing heap fills or after 30 s, then merged 10 at a time (`limit_merge`). A merge stops taking splits once it holds `split_num_docs_target` pages (100,000 at first), and a split that size stops merging, so merged splits held a little over 100,000 pages and never 200,000. In the October 2026 full release, the first round of merges left 7.8M pages in 70 splits, about 110,000 pages (about 2.6 GB at about 23 KB of index per page) each. The whole corpus (23.8M pages) was about 220 splits. The common-word pairs (§5.5.3) make a page about 37 KB of index rather than 23, so the target became 60,000 pages: a merged split was about 2.4 GB again, at most 4.4 GB, and the corpus about 360 splits. American Stories' text and its pairs (§5.5.4) make a page it covers about 74 KB, so the target is now 30,000 pages: at most about 2.4 GB and 4.4 GB again, and the corpus about 800 splits. Step 4 of #218 measures the index size and the cold-search cost of the extra splits before a full rebuild.
- **Why not bigger splits: the writer's memory.** Quickwit 0.9.1 uploads a split to Blob in 100 MB blocks, up to 100 at once, and reads each block into memory first. So a split holds min(its size, 10 GB) of the writer's memory while it uploads, on top of what indexing uses. The ingest container has 7.5 GiB. The first target, a million pages per split (about 23 GB), lost the writer in the October 2026 full rebuild: its first million-page merge finished about 17:42 UTC, the writer went from 1.9 GB to 7.0 GB within 30 s, and by the release's next poll, 14 s later, it was gone without logging an error, which is how the kernel's out-of-memory killer leaves a process. The same release's 110,000-page splits, merged and uploaded, peaked the writer at 3.2 GB while it only merged and at 5.0 GB while it also indexed (08 §8.4). The cost is cold searches: each one opens every split. Measured in prod in October 2026, a cold search for a word on no page took about 0.5 s of backend time over the 220 splits (1.4 s with the split footers not yet cached), about 2.5 to 6.5 ms a split, so the 360 splits of 60,000 pages add about 0.3 to 0.9 s and the about 800 of 30,000 pages about 2 to 5 s (step 4 of #218 measures it), against 4 s for `gold` alone over 1896 and 41 s for the phrase "cross of gold" before the pairs. Splits are too small once that fixed cost is a large part of a typical search: 10,000-page splits would be about 2,400 splits and 6 to 15 s, the September 2026 problem (R-17). Splitting by decade (issue #123, §5.5.5) cuts the number a date-limited search opens instead. Bigger splits need either a writer with more memory (a Dedicated workload profile, which costs at least $73 a month in management fees, 08 §8.4) or a Quickwit build that uploads fewer blocks at once.
- The release waits for the merges before it publishes (`crates/usnm-ingest/src/merges.rs`): first until the merge planner has nothing running or queued and the split list is stable, then it disables the index's ingest source, which shuts its merge pipeline down after up to 5 final merges of the small splits that are left. That also seals the index: later writer runs start no pipelines for it. Quickwit 0.9.1 doesn't restart a merge pipeline that fails after it was told to stop, and its uploader occasionally fails a merge on a race with its own scratch folder ("No such file or directory"; issue #84). When the writer reports that, or stays idle for 10 minutes without finishing, the release runs the final merges again: it re-enables the source, sends one document the strict mapping rejects (which gives a source whose idle shards were deleted a new, empty shard, so Quickwit starts its pipelines), waits for the new merge pipeline and disables the source again. It tries three times. The wait is bounded (4 hours by default, `USNM_MERGE_TIMEOUT_SECS`), and the release fails rather than publish if it runs out, if the writer exits (it names the signal, and says when the container's cgroup counted an out-of-memory kill), if it reports a full disk or a failed merge, if the splits don't hold exactly the documents sent, or if there are more splits than the policy leaves (`docs / 30,000 + 5 + 2`, per decade for a base laid out by decade, §5.5.5).
- Each release logs an `index layout` line per index of the new version (splits, documents, size, smallest and largest split, and the splits' footers: `footer_bytes` in all and `largest_footer_bytes`, from `footer_offsets` in the split metadata) and records the same in the version's `manifest.json` under `indexes`, so the result is visible without access to the index storage. The footer fields are optional: manifests from before #125 don't have them, and a split whose metadata gives no footer leaves them out. The searcher's split footer cache must hold the footers' total for cold searches not to fetch a footer twice (08 §8.4).

Indexes built before this keep their splits: they are older than any maturation period, so no merge policy touches them again. A full release rebuilds them as one new base ([operations](../operations.md#full-rebuild)).

### 5.5.2 Azure AI Search index (sketch)

```json
{
  "name": "pages-v20261001-1",
  "fields": [
    { "name": "doc_id", "type": "Edm.String", "key": true, "filterable": true },
    { "name": "text", "type": "Edm.String", "searchable": true, "analyzer": "usnm_text", "retrievable": true },
    { "name": "date", "type": "Edm.DateTimeOffset", "filterable": true, "sortable": true, "facetable": true },
    { "name": "day",  "type": "Edm.Int32", "filterable": true, "sortable": true },
    { "name": "ym",   "type": "Edm.Int32", "filterable": true, "facetable": true },
    { "name": "year", "type": "Edm.Int32", "filterable": true, "facetable": true },
    { "name": "place_id", "type": "Edm.String", "filterable": true, "facetable": true },
    { "name": "place_year",  "type": "Edm.String", "facetable": true, "retrievable": false },
    { "name": "place_ym",    "type": "Edm.String", "facetable": true, "retrievable": false },
    { "name": "lccn", "type": "Edm.String", "filterable": true, "facetable": true },
    { "name": "state", "type": "Edm.String", "filterable": true, "facetable": true },
    { "name": "language", "type": "Collection(Edm.String)", "filterable": true, "facetable": true },
    { "name": "front_page", "type": "Edm.Boolean", "filterable": true },
    { "name": "edition", "type": "Edm.Int32" },
    { "name": "seq", "type": "Edm.Int32" },
    { "name": "batch", "type": "Edm.String", "filterable": true }
  ],
  "analyzers": [{
    "name": "usnm_text",
    "@odata.type": "#Microsoft.Azure.Search.CustomAnalyzer",
    "tokenizer": "standard_v2",
    "tokenFilters": ["lowercase", "asciifolding", "usnm_len"]
  }],
  "tokenFilters": [{ "name": "usnm_len", "@odata.type": "#Microsoft.Azure.Search.LengthTokenFilter", "min": 1, "max": 40 }]
}
```

`place_year` and `place_ym` are the GA workaround for R2 (composite facets). With the preview API, `facets=["place_id > (year)"]` replaces them.

### 5.5.3 Common-word pairs (`text_cg`)

A phrase reads the positions of every word in it, and the commonest words are on almost every page many times over. Over 1896 the phrase "cross of gold" took 41 s, `cross gold` within 2 words 9 s, and the phrase "yellow fever" 8 s (06 §6.5). So each page is indexed a second time, in `text_cg`, with each common word joined to the word after it: "Upon a cross of gold" becomes `upon_a a_cross cross of_gold gold`.

- **Same positions as `text`.** One token per word the `usnm_text` analyzer sees, including a `_` where it drops one (over 40 characters). The release writes the tokens (`usnm_core::common_grams::index_text`) and the field's `whitespace` tokenizer indexes them as written.
- **Exact matches.** A phrase of two or more words with a common word that isn't last maps word for word: a common word to `word_next`, any other to itself (`cross of_gold gold`). It matches at a position in `text_cg` exactly when the phrase's words are at that position in `text`, because each common word's token also checks the word after it. A phrase ending in a common word, one without any, and phrases with slop stay in `text`. Unit tests check the equivalence over every phrase of up to 4 words from a small vocabulary; the Quickwit parity tests (`phrases_through_common_word_pairs_match_the_reference_backend`) check counts, cubes, hits and their order on the fixtures.
- **Snippets.** The query also requires each phrase word that isn't common in `text` (`text:cross AND text:gold`): no page changes, and the snippets on `text` get their highlights. The common words in the phrase aren't highlighted.
- **The list** (`usnm_core::common_grams::WORDS`, 89 words) is the function words on about half of all pages or more, measured over a week of 1896, and `mr`. Frequent content words (`new`, `time`) are left out. The list is part of the index: changing it, or how words are folded, makes a new version (`common_grams::VERSION` is the latest).
- **Versions.** `current.json` names the `common_grams` version its indexes were built with, and so the analyzer that folded their words (`usnm_core::text::Analyzer`). Version 1 folds every character through its compatibility form, so `½` is `1⁄2`; version 2 keeps a character whose compatibility form holds a separator as one word, as `usnm_text` has it (`½`, `61¼`; #168), and a character that isn't Japanese but whose compatibility form is (`㊀` is `一`) as itself, so that `a㊀` is one word on a Japanese page too, as in `text` (#241). The Japanese pages' index is folded with the same analyzer and records it as `ja.fold`. The API knows every version up to its own: on any of them it searches `text_cg`, and parses queries and maps phrases to pairs with that version's analyzer, so it searches each version as it was built. Quickwit's `text` is `usnm_text`'s in every version and analyzes a query's words again, so on a version 1 index `½` (the term `1⁄2`) finds the pages with `1/2`, as before #168; the memory backend and the snippets read pages and queries the same way. A version without pairs (absent: analyzer 1), or with a version newer than the API, keeps phrases in `text`. Response caches are keyed by the index version, so the two never share an entry.
- **One searched field.** On a version with one field for both texts (§5.5.7) the pairs are in `text_all_cg`, made separately for each text, so the last word of one is never paired with the first of the other, with the same gap between them as in `text_all`. A phrase maps to pairs as above, and its other words are required in `text_all`.
- **Rollout.** A delta is built with the published version's analyzer, so every index of a version tokenizes alike and `current.json` keeps the version. Only a full base moves to the latest: `USNM_INGEST_FULL`, or the release's own triggers (another backend, American Stories' text, too many deltas). A release builds a full base for the pairs only when the published version has none, or a version newer than the release knows. So a change to the analyzer ships inert, and takes effect with the next deliberate full rebuild (operations.md, Full rebuild).
- **Cost.** `text_cg` holds a position for every word, like `text`. On 90,745 real pages (11 LoC batches, October 2026) it made the index 1.6× as big (4.8 GB against 3.0 GB) and indexing 1.9× as long; over the corpus that is about +0.35 TB on Blob Hot, about +$7 a month. Japanese pages (#139) have their own index and no pairs.
- **Measured gain.** On the same pages, cold (OS cache dropped) on 2 CPUs, the searcher read 11–31× fewer bytes for phrases with a common word ("cross of gold" 17.3 MB → 0.6 MB) and took 2–6.5× less time, with the same hits.

### 5.5.4 American Stories' text (`text_as`, #218)

American Stories (Dell et al., CC BY 4.0) re-OCRed many of the same scans with a layout model. Matching a term in either text finds more pages: +10% to +26% per term in 1865 and +2% to +12% in 1925 (#205). So a page in the main index carries a second text.

- **One document per page, two texts.** `text_as` holds American Stories' text for the page (headlines, bylines and articles, in reading order), analyzed as `text` is, with positions, and stored. `text_as_cg` holds its common-word pairs, made as `text_cg` is (§5.5.3). LoC's `text` and `text_cg` don't change. Japanese pages (#139) have their own index and no second text.
- **Either text, counted once.** Each term or phrase of a query becomes `(text:… OR text_as:…)`; an exact phrase through the pairs becomes `((text_cg:… AND text:…) OR (text_as_cg:… AND text_as:…))`. A phrase has to sit within one text, so it can't span the two. `AND`, `OR` and `NOT` combine the leaves as before, so `NOT x` leaves out a page with `x` in either text, and `a b` matches a page with `a` in one text and `b` in the other. A page is one document, so it counts once however many texts it matches in: hits, cubes, medians and first and last pages count documents. The memory backend, the reference for the parity tests, evaluates each leaf against both token streams the same way. A version with one searched field (§5.5.7) gives the same pages with one leaf per term or phrase.
- **Behind a flag.** `current.json` records `american_stories: <version>`, and the API searches `text_as` only when that is `usnm_core::american_stories::VERSION` (`IndexSet::american_stories`). Without it, every query is exactly LoC's, so versions built without the fields (which a strict Quickwit mapping would refuse to query) behave as before. The API setting `USNM_AMERICAN_STORIES_SEARCH=false` switches the fields off without a rebuild or touching `current.json` (`RefData::searches_american_stories`): searches, snippets and counts are then LoC's alone, and their response-cache keys end in `|american_stories=off`, so a response computed in one state never serves the other (with it on, the keys are the same as before the setting; Japanese queries, which search the Japanese pages and never this text, keep theirs either way). It is how cold searches are timed with and without the fields on production (#251), and the off switch if they cost too much. `/v1/meta` says which applies (`american_stories`). On a version with one searched field (§5.5.7) the setting has no effect: the texts share the field, so the API logs a warning and searches both.
- **Snippets.** LoC's text first. When the query marks nothing there and the search covers American Stories' text, the snippets come from `text_as`, and the hit says so (`snippet_source: "american_stories"` on `/v1/hits` items, response format 5).
- **Which text matched.** With the flag on, each hit also lists the texts the whole query matches on its own (`matched_in`: `["loc"]`, `["american_stories"]` or both; response format 6), worked out in the API from the two texts the hit already carries for its snippets, with the memory backend's evaluator (`usnm_search::snippet::matched_in`): no extra engine call. A page found only by taking a word from each text (`a b`, `a` in one and `b` in the other) lists both. Japanese pages carry no `matched_in`.
- **Pages only American Stories' text finds.** With the flag on, and the text in fields of its own, `/v1/aggregate` adds `total.american_stories_only`: the matching pages whose `matched_in` would be `["american_stories"]`. It is one more count-only request (`max_hits: 0`, no aggregations), made with the cube and the first and last page, with the query rendered three ways: `(both texts) AND NOT (LoC's text alone) AND (American Stories' text alone)`, plus the same filters. Subtracting a count of LoC's text alone from `total.hits` would be wrong under `NOT`: `gold -bryan` in LoC's text alone finds pages whose American Stories text has `bryan`, which the search of both texts leaves out. On the fixtures, `gold -bryan` finds 372 pages in both texts and 379 in LoC's alone, so subtracting gives −7; 15 match only in American Stories' text. The count is part of the aggregate response, so the response caches and the warm-up (06 §6.6) hold it too, and a cached search costs nothing more. On the fixtures it takes about 1 ms, the same as any count there; on the full index it should read the same posting lists the summary request has just read (not measured). A version with one searched field (§5.5.7) can't count it, since LoC's text isn't indexed on its own there: the key is absent, as on a version without American Stories' text, and the hits keep `matched_in`.
- **Sizing.** With its pairs, `text_as` roughly doubles a covered page's index, to about 74 KB, so `split_num_docs_target` is 30,000 (§5.5.1).
- **Pages with only American Stories' text.** A page LoC has no usable text for (`empty` or `short`) is indexed when American Stories has text for it, with an empty `text`. The baselines already count it, so rates don't change their denominators.
- **Written by the release** with `--american-stories` (`USNM_AMERICAN_STORIES`, off by default; 04 §4.9): the per-batch loader, the pages above, and `american_stories` in `current.json` only when every index of the version has the text, so the first release with it (or with a new `VERSION`) builds a full base. The full rebuild itself waits for step 4 of #218 (the measurement) and a go-ahead ([operations](../operations.md#american-stories-text)).

### 5.5.5 Decade partitions (`decade`, #123)

A cold search opens every split of every index it names, even one limited to a single year: the `day` filter is exact but prunes no split, and splits span decades because the release sends pages in archive order (batch, then title, then date). With about 800 splits at the 30,000-page target (§5.5.4), a search over 1896 opens them all. Laying the base out by decade lets it open the 1890s' splits and the deltas that hold 1890s pages: about 8× fewer for a single year at production's page counts, the same for a full-range search.

- **Behind a setting, at a full rebuild.** `--partition-decade` (`USNM_PARTITION_DECADE`, off by default; [operations](../operations.md#full-rebuild)) lays a full base out by decade. Off, the documents, the index config and `current.json` are exactly as before. A delta follows the published version, whatever the setting says (below).
- **The field.** Each page gets `decade` (u64, indexed, not stored or fast): the decade of its date (`usnm_core::decade::of_year`), with every page before 1820 in 1820. Those ten early decades hold about 190,000 pages in all, fewer than one split of the 1820s, and a split each would add nine to every full-range search. That leaves 15 partitions, 1820s to 1960s.
- **The base.** The index config adds `tag_fields: [decade]`, `partition_key: decade` and `max_num_partitions: 32` to the doc mapping (`Decades::Partitioned` in `crates/usnm-ingest/src/sink.rs`, applied to `pages-index.yaml` when the index is created; the manifest records the config as applied). Quickwit 0.9.1 keeps each partition's pages in their own splits and merges only splits of one partition, and records each split's decade as a tag (`decade:1890`). The indexing settings don't change.
- **Deltas: tagged, not partitioned.** A delta on a version laid out by decade gets the field and `tag_fields: [decade]`, but no `partition_key` (`Decades::Tagged`). A weekly delta is a split or a few; partitioning would cut each into one per decade, and every full-range search, the site's default, opens all of them. With tags, a delta split lists every decade it holds (`decade:1840`, `decade:1860`, …) and a search skips it when it holds none of the search's. A delta on a version without the field has none either, so every main index of a version has the field or none does.
- **The search.** When `current.json` has `decades` at the API's `usnm_core::decade::VERSION`, the Quickwit backend adds `decade:IN [1890 1900]` (the decades of the search's range) next to the `day` filter (`IndexSet::with_decades`, `quickwit::decade_clause`). Quickwit turns the term set into a filter on split tags before it opens any split; a split without the `decade!` tag (an index without the tag field) always passes. The `day` filter stays the exact one, so the clause changes no count. It's left out when the range covers every decade of the version's bounds: it would skip nothing and cost each split a posting list. It is never a timestamp or a `date:` range (§5.5.1, #158).
- **Writing the base a decade at a time.** Quickwit cuts one split per partition at each commit, and a commit takes the pages that arrived since the last. In archive order a 22,000-page window spans 3 to 6 decades, so each commit would cut several small splits that all need merging: measured below, 5× the splits and 1.8× the pages merged, on a writer already bound by its CPU (#233). So the release sends a partitioned base's pages a decade at a time (`crates/usnm-ingest/src/decade_order.rs`): each page goes to its decade's file on the writer's scratch share (zstd JSON lines), a decade's file is sent as soon as it holds `split_num_docs_target` pages, and the rest go out at the end, oldest decade first. A commit then holds one or two decades instead of several. The share holds at most 15 × 30,000 pages that way, a few GB compressed, never the whole corpus (about 1.3 TB of JSON, which is why spilling everything first, #123's first design, doesn't fit). Each curated part is still read once: re-reading parts by decade would read about 2.3× the pages (#123). A `sent a decade's pages` line logs each run, and `sent the pages a decade at a time` the totals and the most held on disk.
- **The merge check.** Merges and final merges run per partition, so each partition can keep its own small splits. The release's ceiling is the sum over partitions of `docs / 30,000 + 5 + 2` (`merges::max_partitioned_splits`, by the splits' `partition_id`); for an index without partitions that is the same bound as before. The `index layout` line and the manifest's `indexes` add `splits_by_decade` for a partitioned index.
- **Rollout.** `current.json` records `decades: 1`, and so does the build record's `features`. The API names decades only at its own version, so an API with another bucketing (a new `VERSION`) prunes nothing on older versions rather than skip splits it needs, and dropping the key from `current.json` turns the clause off without a rebuild.
- **Measured locally** (Quickwit 0.9.1 on 8 cores, `crates/usnm-ingest/tests/decade_measure.rs`): 3,000,000 synthetic pages in archive order (batches of titles, each title's pages by date, decades weighted like production), about 1.7 KB of text each, with a 2 s commit timeout so that, as in production (120 s, about 23,000 pages), the writer cuts splits before the 30,000-page target:

  |                                          | No decades   | Partitioned, archive order | Partitioned, a decade at a time |
  | ---------------------------------------- | ------------ | -------------------------- | ------------------------------- |
  | Writer's peak memory                     | 1,058 MiB    | 1,931 MiB                  | 1,514 MiB                       |
  | Splits cut before merging (median pages) | 288 (10,039) | 1,538 (1,714)              | 492 (5,022)                     |
  | Merges (pages merged)                    | 85 (3.0M)    | 213 (5.3M)                 | 105 (3.0M)                      |
  | Splits when sealed                       | 86           | 87                         | 111                             |
  | Sending + merge wait                     | 324 s + 34 s | 417 s + 384 s              | 452 s + 78 s                    |

  Partitioning in archive order cuts each commit into a split per decade: 5× the splits, 1.8× the pages merged, 11× the merge wait and 1.8× the writer's memory (all partitions of a commit share the one indexing heap, but each has its own split to build, package and upload). A decade at a time keeps the merges to one pass over the pages, like today, and the memory at 1.4×. It leaves about 1.5 more splits per decade than no decades (merges join only splits of one decade): at production's scale about 20 more than the about 800 a full-range search opens, against about 8× fewer for a year's search. The files held at most 304,000 pages (378 MB compressed). Sending was slower here because the test's generator and the order's files share the 8 cores with the writer, which indexes these small pages about 50× faster than production's: in production the writer's ingest queue (about 2 minutes of pages) keeps it busy while the release fills the next decade's file. Measure the rate on the first partitioned rebuild (`release progress`) against the 48 h replica timeout.

- **Checked in CI.** The parity tests (`searches_by_decade_match_the_reference_backend`, `searches_by_decade_open_only_their_decades_splits`) check that searches naming their decades return the reference's pages and target only those decades' splits, on the fixtures moved over four decades (`fixtures/decades.rs`), and the pipeline test `releases_a_base_laid_out_by_decade_into_a_quickwit_writer_node` builds a partitioned base and a tagged delta on a real writer.

### 5.5.6 Congress pages (`congress-*`, #229)

The Congressional Globe, the Serial Set, the Annals of Congress and the Register of Debates go in an index set of their own, `infra/quickwit/congress-index.yaml`, named in `current.json` under `congress`. Newspaper searches never open it (the reasons are in #229's design: split count, split size, and releases that don't wait on each other).

- **Same shared fields.** The template keeps every field the shared search code reads (`day`, `ym`, `year`, `place_id`, `place_shard`, `state`, `language`, `front_page`, `lccn`, `sort_key`, `text`, `text_cg`), so the same aggregations, filters and hits serve both sets. It drops the American Stories fields and adds `series` (filtered with `series:IN [...]`), the stored citation fields (`title`, `citation`, `congress`, `session`, `chamber`, `printed_page`, `package_id`, `link`) and the state data kept for later maps (`states`, `state_via`, `speakers`: indexed keyword arrays, not fast or stored).
- **One engine document per page**, as for newspapers (a report or a debates volume is many pages; grouping them in hits comes later), at the reserved place `PC001` (Washington, DC), with `state` `DC`, `language` `eng`, an empty `lccn` and `front_page` false. `usnm_ingest::congress::congress_doc` builds it from a curated page.
- **Splits of about 500,000 pages.** A Congress page has a tenth of a newspaper page's text or less, so the set is about 20 splits.
- **One analyzer.** A query is parsed once, with the main indexes' analyzer, so `congress.common_grams` must equal the main indexes' `common_grams`; otherwise the API refuses Congress searches. A full rebuild that changes the analyzer rebuilds the Congress set too.
- **Checked.** `congress_searches_match_the_reference_backend` compares Quickwit with the reference backend on the synthetic Congress fixture (`fixtures/data/indexes/congress-fixture.jsonl`), with and without common-word pairs, across series filters.

### 5.5.7 One searched text field (`text_all`, #283)

Each term of a query is looked up in both texts' fields (§5.5.4): two term dictionaries and two posting lists per word in every split, and four fields for a phrase with a common word. With American Stories' fields switched off on production, a cold search did half the backend work (#251). So a version can search both texts in one field instead ([ADR-0014](adr/0014-one-searched-text-field.md)).

- **The fields.** `text_all` holds LoC's words, then 32 positions no query can match, then American Stories' words; `text_all_cg` holds each text's common-word pairs (§5.5.3) the same way. A page with one text has no gap. `text` and `text_as` are stored for snippets and `matched_in`, not indexed, and `text_cg` and `text_as_cg` are gone. `default_search_fields` is `[text_all]`. The names are new, so a query rendered for one layout and sent to an index of the other fails with a 400 rather than miscounting.
- **The release writes the tokens** (`usnm_core::text_layout::index_fields`): one per word as the version's analyzer folds it, `_` where the analyzer drops one and in the gap, and the fields' tokenizer is `whitespace`, as for the pairs. Quickwit 0.9.1's `remove_long` filter in `usnm_text` drops tokens of 255 bytes or more, not 40 characters (#286), so a gap of dropped filler words would cost at least 255 bytes a position, and a shorter filler word would be indexed and reachable by a prefix query. `_` is not a word, so no query produces it. The indexed words are then exactly the ones the query parser, the memory backend and the snippets use.
- **The gap.** 32 positions (`GAP_POSITIONS`, more than the largest slop, 20). On Quickwit 0.9.1, no phrase of up to 12 words with slop 20 matched across it, in order or reversed (Quickwit counts a swap against the slop, #243), and a two-word phrase needed slop 32. Tantivy's own gap between the values of an array field is one position, which is why the texts are one string.
- **Queries.** The AST renders as in §5.5.4 with one field set: `gold` is `text_all:gold`, `"cross of gold"` is `(text_all_cg:"cross of_gold gold" AND text_all:cross AND text_all:gold)`, and `NOT x` is `NOT text_all:x`, which still leaves out a page with `x` in either text (`quickwit::query_for`). Prefixes and wildcards walk one term dictionary, with the same 5-letter minimum. Fuzzy terms are still refused. The Japanese index keeps its own mapping.
- **What the API works out from the stored texts.** `matched_in` and `snippet_source` are computed from `text` and `text_as` on each hit, as before. `total.american_stories_only` is absent (§5.5.4), and the aggregate makes one request fewer. `USNM_AMERICAN_STORIES_SEARCH=false` has no effect.
- **Behind a flag.** `current.json` records `text_layout: 2` (absent: a field per text), and so does the manifest's `build.features`. The API searches each version as its layout says (`IndexSet::text_layout`) and refuses to load a layout it doesn't know, so the loader keeps serving the previous version. `/v1/meta` says which applies (`text_layout`).
- **Rollout.** `--single-text-field` (`USNM_SINGLE_TEXT_FIELD`, off by default; [operations](../operations.md#full-rebuild)) lays a full base out this way. A delta follows the published version, whatever the setting says, so every main index of a version has the same fields. A published layout the release doesn't know builds a full base, and so does a release without `--american-stories` on a version with one field and American Stories' text: the text can't be left out of a field it shares with LoC's, the way a delta leaves `text_as` out.
- **Size and cost.** The same words are indexed with positions, in one dictionary: a word in both texts of a page is one posting with more positions, and OCR runs of 41 to 254 bytes become `_` rather than a term each, so the index should be no larger than with four fields. A search looks each word up once; the posting lists are longer than LoC's alone, so a cold search should land near, not at, the "fields off" timing of #251. Both to be measured on the dev 1% set before the rebuild (#283).
- **Checked in CI.** The parity tests load the fixture base and delta a second time with this mapping (`scripts/quickwit-fixtures.sh`, `single_field_fixture`) and run every query, filter, bucket unit, hit page and phrase through the pairs on both layouts; `one_field_keeps_phrases_within_one_text` checks that a phrase and a NEAR search at slop 20 don't span the texts. The pipeline test `releases_one_text_field_into_a_quickwit_writer_node` builds a base and a delta on a real writer under the strict mapping.

## 5.6 Query semantics

The user-facing syntax is simple and matches LoC's modes. The API parses it into an AST (see [06](06-api-design.md)), then translates:

| User intent    | UI control / syntax                          | AST                               | Quickwit                               | AI Search (`queryType=full`) |
| -------------- | -------------------------------------------- | --------------------------------- | -------------------------------------- | ---------------------------- |
| Exact phrase   | mode = phrase, or `"cross of gold"`          | `Phrase([cross,of,gold], slop 0)` | `text:"cross of gold"`                 | `text:"cross of gold"`       |
| All words      | mode = all                                   | `And[Term…]`                      | `text:a AND text:b`                    | `+a +b`                      |
| Any word       | mode = any                                   | `Or[Term…]`                       | `text:a OR text:b`                     | `a b` (searchMode any)       |
| Near           | mode = near, n = 5                           | `Phrase(slop 5, unordered)`       | `text:"a b"~5`                         | `"a b"~5`                    |
| Exclude        | `-word`                                      | `Not(Term)`                       | `-text:word`                           | `-word`                      |
| Fuzzy (OCR)    | toggle "OCR-tolerant"                        | `Fuzzy(term, d=1 or 2)`           | not supported in 0.9 (§5.5.1); refused | `term~1`                     |
| Prefix         | `word*` (≥ 5 letters)                        | `Prefix`                          | `text:word*`                           | `word*`                      |
| Wildcard (OCR) | `presi?ent`, `washi*ton` (≥ 5 letters first) | `Term` with `wildcard`            | `text:presi?ent`                       | `presi?ent`                  |

Filters (`from`, `to`, `state`, `lccn`, `language`, `front`) compile to range and term filters on `day`, `state`, etc. They are never free text.

## 5.7 Aggregation strategy

**Bucket choice.** The API chooses the finest bucket that keeps the result cube bounded, unless the client pins one:

| Date span          | Default bucket | Max buckets |
| ------------------ | -------------- | ----------- |
| > 30 years         | year           | ~210        |
| 3–30 years         | month          | 360         |
| 4 months – 3 years | week           | 157         |
| ≤ 4 months         | day            | 122         |

**Queries issued for Q1** (Quickwit; AI Search in the growth profile issues the equivalent facet requests). Every query, the hits queries included, also excludes the copies of duplicated pages the version's `duplicates.json` lists (04 §4.7), one clause per batch: `NOT (batch:{batch} AND doc_id:IN [{ids}])`. The list is empty after a full release.

1. **Summary query** (always one call): `histogram(bucket_field)` for the national series, `min(day)` and `max(day)` for the first and last matching day, plus `terms(place_id, size = 5,000) → min(day), max(day)` for the first and last appearance per place. This also yields **P**, the number of places with at least one hit. It needs at most ~3,000 + 360 buckets.
2. **Cube query:** `terms(place_id) → histogram(bucket_field, interval)` gives the sparse cube `[place][bucket] = pages`. Its upper bound is **P × B** buckets (B = number of time buckets).
   - If P × B ≤ **150,000**, it's a single call.
   - Otherwise the API **shards** it by `place_shard`, a fast field holding `place ordinal mod 8` that is written at index time. It issues ⌈P × B / 150,000⌉ sub-queries (at most 8), two at a time, each filtered to its shard, and merges the results. The worst case (~633k cells) takes 5 sub-queries.
3. **First and last page** (when anything matches): two hits queries with `max_hits = 1`, sorted oldest and newest first, run alongside the cube. They name the page behind the first and last day, so a misleading first mention (an OCR misread) can be checked.
4. `max_hits = 0` on every other call, so no documents are returned.

**Engine limits, set explicitly.** Quickwit caps nested aggregations with `searcher.aggregation_bucket_limit` (**default 65,000**) and `searcher.aggregation_memory_limit` (default 500 MB). The sidecar config sets **`aggregation_bucket_limit: 200000`** and **`aggregation_memory_limit: 768MB`**, which fits the 4 GiB sidecar, so a 150k-bucket shard has headroom. S-2 benchmarks memory and latency at exactly these settings. If memory is tight, the shard target drops (e.g. to 60k buckets, ≤ 11 sub-queries) rather than raising the limits. Typical queries therefore take four calls (summary, one cube and the two first/last page queries), and the largest take up to 1 + 8 + 2.

**Searcher caches.** The sidecar also keeps split footers, fast-field data and per-split partial and predicate results in memory (`infra/quickwit/searcher.yaml`, 08 §8.4). The cube and summary queries of a repeated or similar search reuse them, so only the first search after a start or a publish pays for every split's footer. The API reports the caches' size, hits, misses and evictions (`api.searcher_cache_*`, 08 §8.1.2); the split footer cache is sized to hold every footer of the serving version.

**Normalization** happens in the API from `reference/{index_version}/baselines_place_day` (pages _published_ per place and day, rolled up to any bucket in memory):
`rel[place][bucket] = hits / baseline`, and nationally `rel[bucket] = Σhits / Σbaseline`. This replaces the legacy `globalFreq` table, stays correct as the corpus grows, and gives honest per-place rates.

**High-frequency guardrails.** Terms such as `the` match almost every page. Protections:

- (a) The response cache and CDN absorb repeats.
- (b) A visitor's request waits at most 10 s for a search (12 s when there is a persistent cache). The API answers a slower search with `202 Accepted` and keeps computing it for up to 2 minutes while the browser asks again (06 §6.3.5); past that the API returns `503` with a hint to narrow the date range or filters, and the timeout is not cached.
- (c) The UI discourages queries made only of stop words (warns before sending).
- (d) Example searches are pre-warmed after each index-version bump.

## 5.8 Benchmark query set (Spike S-2)

| Class               | Examples                                      | Expected hits (23M corpus) |
| ------------------- | --------------------------------------------- | -------------------------- |
| Rare phrase         | `"cross of gold"` 1896; `"yellow jack"` 1878  | 10³–10⁴                    |
| Mid-frequency term  | `scalawag`, `miscegenation`, `influenza` 1918 | 10⁴–10⁵                    |
| High-frequency term | `railroad`, `lincoln`, `war`                  | 10⁶–10⁷                    |
| Proximity           | `"gold silver"~5`                             | 10⁵                        |
| Filtered            | `influenza` state=GA front=true               | 10³                        |
| Pathological        | `the`, `a*` (rejected), 6-term OR             | –                          |

Metrics: p50, p95 and p99 latency (cold and warm); index size ÷ raw text; ingest pages/s; cost per 1,000 queries; count correctness against a DataFusion brute-force scan of the curated Parquet sample (must match exactly).

## 5.9 Document storage and document state

The design separates **document content** from **document state**:

- **Content** is the page text and immutable metadata: large, written once per batch version, and read in bulk.
- **State** is small and mutable: where each title, batch and page is in the pipeline, its errors, versions, index membership, geocoding review status, and coverage summaries. It changes often, needs atomic updates, and people ask questions of it ("which titles failed curation?", "which batches aren't in index v7?").

| Data                                                                 | Store                                                                                                                                       | Why                                                                                                                                                                     |
| -------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Page text + immutable metadata** (the corpus)                      | **Curated Parquet on Blob Storage (flat namespace), Cool tier** (~150–300 GB compressed; immutable attempt paths; versioning + soft delete) | The system of record for content ([ADR-0002](adr/0002-blob-data-lake-system-of-record.md)). Read in bulk for rebuilds, baselines and offline research. About $2–3/month |
| **Document state** (per LCCN, per batch, per issue, per index run)   | **Azure Cosmos DB for NoSQL, free tier**                                                                                                    | Queryable, atomic partial updates (patch), optimistic concurrency, change feed. **$0** within the free tier ([ADR-0007](adr/0007-cosmos-document-state.md))             |
| **Serving copy of text** (snippets) and **stored fields**            | Inside the search index (Quickwit docstore on Blob Hot)                                                                                     | Hits and snippets come back in the same engine call                                                                                                                     |
| **Reference data for the API** (titles, places, baselines, coverage) | Blob Hot, Parquet + zstd JSON, loaded into API memory. Built by the `stats` job from Parquet and the Cosmos `titles` container              | The API's read path has no database dependency, so the site keeps serving if Cosmos is unavailable                                                                      |
| **Response cache**                                                   | Blob Hot `cache/{index_version}/`                                                                                                           | Survives restarts                                                                                                                                                       |
| **User data** (future: saved searches, collections)                  | Same Cosmos account (new containers)                                                                                                        | Reuses the existing account and identity                                                                                                                                |

### 5.9.1 Cosmos DB state model

One account (**free tier**, provisioned throughput, NoSQL API, `disableLocalAuth: true`, Entra ID data-plane RBAC), one database `usnm` with **shared throughput of 1,000 RU/s** (the free-tier allowance), and these containers:

| Container    | Partition key    | Item (one per…)               | Approx. count | Key fields                                                                                                                                                                                                                                    |
| ------------ | ---------------- | ----------------------------- | ------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `titles`     | `/lccn`          | newspaper title               | ~4–5k         | name, place history (date-ranged), `place_id`, geocode `{method, precision, review_status, override}`, languages, first/last issue, page counts by `ocr_source`, `status ∈ {discovered, curated, indexed, error}`, `last_error`, `updated_at` |
| `batches`    | `/batch`         | LoC batch                     | ~3k           | `versions_seen[]`, `current_version`, `status ∈ {discovered, queued, downloading, curated, indexed, failed}`, `attempts`, `source_sha256`, `pages`, `lccns[]`, `curated_path`, `indexed_in[]`, `lease {owner, until}`                         |
| `issues`     | `/lccn`          | issue (lccn + date + edition) | ~3–4M         | page count, empty-OCR pages, `batch`, `batch_version`, `ocr_source`, `indexed_in` (index version). Enables "what's missing for this title?" checks                                                                                            |
| `index_runs` | `/index_version` | index build                   | tens          | `status`, `full`, `indexes`, `new_index`, doc and page counts, `batch_count`, `batch_list` (the path of `reference/{index_version}/batches.json`), `started_at`, `published_at`, `previous_version`, `last_error`                             |
| `ops`        | `/kind`          | misc                          | few           | `current` pointer (mirrors `reference/current.json`), locks, the LoC download pacer, the running release's progress (`release-progress`), schedules                                                                                           |

**Index run items stay small.** A version's batch list carries every batch's curation record, about 500–650 bytes each, so ~3,000 batches come to ~1.5–2 MB, and Cosmos DB refuses items over 2 MB. The list is in the version's reference snapshot (`batches.json`, immutable and checksummed in the manifest like the rest), and the run item has only `batch_count` and `batch_list`, a few hundred bytes whatever the corpus size. Runs written before this carry the list inline as `batches`; the release reads either form, and the status page reads `batch_count` or the inline list's length, never the list.

**Why issues and not pages?** Page-level state (23M items, ~12 GB) would fit the free tier's 25 GB, but writing it costs about 140M RU: roughly 38 hours of the whole free throughput. It would also duplicate what the Parquet files already record. Issue-level state (~3–4M items, ~2 GB) answers the practical questions (coverage gaps, reprocessing status) at about a sixth of the write cost. Page-level detail is always available by scanning Parquet.

**Usage patterns:**

- **Work claiming.** Batch workers (Container Apps Jobs or ACI Spot groups) take a batch with a conditional **patch** on the batch item (`lease.until < now` → set the owner, `status=downloading`), using its ETag. This also makes Cosmos the work queue: `queued` batches are claimed in order, and an expired lease makes a batch claimable again. No Storage Queue is needed, which saves a private endpoint.
- **Commit order.** Cosmos transactions don't span containers, so a worker writes dependent items first (issue upserts, which are idempotent) and makes the batch's `status=curated` patch the **last** write, conditional on its lease. A crash at any point leaves the batch un-curated and retryable, never marked done with incomplete issue state.
- **Progress and retries.** Every state transition is one patch (~10 RU). Failures record `last_error` and `attempts`, and a failed batch can be re-queued with a single query plus patch.
- **Change feed.** The `index` job reads the `batches` change feed (pull model) to find batches that became `curated` since its last continuation token. This is a natural driver for incremental indexing. The Rust SDK added change-feed pull in 0.37.
- **Operator queries.** `SELECT * FROM b WHERE b.status = 'failed'`, "titles with county-precision geocodes awaiting review", "issues not in the current index". These run from the Data Explorer or a tiny admin CLI subcommand.

**Throughput budget.**

- A backfill writes ~3k batch transitions × ~5 plus ~3.5M issue upserts at ~8 RU each, about **30M RU in total**. At a throttled ~600 RU/s that takes ~14 hours, well within the ~4-day download-bound backfill.
- Weekly increments use a few thousand RU.
- The SDK retries 429s, and workers keep a client-side RU budget so the free throughput isn't exceeded.

**Cost:** **$0** within the free tier. Beyond it: ~$0.25/GB-month plus provisioned RU/s. If the free tier is already used in the subscription, **serverless** costs roughly $1–3/month at this volume.

**Client:** the pipeline talks to the Cosmos **REST API** directly (`crates/usnm-ingest/src/cosmos.rs`) with Entra ID tokens: point read, create, **replace with `If-Match`** (for claims, commits and locks), upsert and a one-field query, retrying 429s. That is the whole surface the pipeline needs, and it avoids depending on the beta Rust SDK. Finding newly curated batches uses a status query rather than the change feed; at ~3k batch items the query costs little.

**Resilience:** the API doesn't read Cosmos on the search path. Only the status page (`/v1/status`, [06 §6.3.6](06-api-design.md#636-get-v1status)) reads it, read-only, at most once a minute per API replica and three queries at a time. If Cosmos is throttled or down, the site keeps serving, the status page shows its last reading as stale, and only the pipeline pauses. Continuous backup (7-day, free tier) covers mistakes, and state can be rebuilt from Parquet plus the LoC batch list if ever lost.
