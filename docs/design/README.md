# US News Map Revival: Design Document

**Status:** Living design record, kept current with the code · **First draft:** September 2026 · **Scope:** Architecture and design for bringing back [usnewsmap.com](https://usnewsmap.com) on Azure with a modern stack.

US News Map (2015–2016, Georgia Tech Research Institute and eHistory.org at the University of Georgia) let anyone search the Library of Congress's _Chronicling America_ newspaper collection and watch where and when a word or phrase appeared across the United States. You typed a term, a map lit up with cities whose newspapers printed it, and a playback control showed coverage spreading over time. The site won a prize in the NEH _Chronicling America Data Challenge_, and the Washington Post, UGA/Newswise, the Library of Virginia and genealogy societies all wrote about it. It later went offline because no organization was left to support it.

This document set evaluates the legacy system ([`tgoodyear/usnewsmap`](https://github.com/tgoodyear/usnewsmap)) and proposes a replacement that is cheaper to run, easier to operate and faster, with more features. It is written to be implementable by a small team or a single maintainer.

## Reading order

| #   | Document                                                             | What it covers                                                                                                                                                       |
| --- | -------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 01  | [Background & legacy analysis](01-background-and-legacy-analysis.md) | Project history, press coverage, a walkthrough of the legacy code, why the system failed, security findings                                                          |
| 02  | [Product requirements](02-product-requirements.md)                   | Personas and use cases from the press coverage, the legacy feature list, new features, non-functional requirements                                                   |
| 03  | [Architecture overview](03-architecture-overview.md)                 | Target architecture, components, request and data flows, the main architectural decisions                                                                            |
| 04  | [Data sources & ingestion](04-data-sources-and-ingestion.md)         | Library of Congress data sources after the 2025 migration, the ingest pipeline, data lake layout, geocoding, baselines, re-indexing                                  |
| 05  | [Search & document storage](05-search-and-storage.md)                | Evaluation of Azure AI Search, Quickwit, Cosmos DB, Elastic and PostgreSQL; sizing; index schemas; query semantics; aggregation strategy                             |
| 06  | [API design (Rust)](06-api-design.md)                                | Whether Rust is the right choice, API contract, crate layout, caching, guardrails                                                                                    |
| 07  | [Frontend design](07-frontend-design.md)                             | SPA stack, map rendering, client-side playback, timeline, accessibility, embed mode, wireframes                                                                      |
| 08  | [Azure infrastructure & delivery](08-azure-infrastructure.md)        | Resource inventory, identity, networking, IaC, environments, CI/CD                                                                                                   |
| 09  | [Operations, security & cost](09-operations-security-cost.md)        | Observability, SLOs, privacy, threat model, cost model, backup/DR                                                                                                    |
| 10  | [Roadmap, risks & open questions](10-roadmap-and-risks.md)           | Phased delivery plan, validation spikes, risk register, decisions needed from the maintainer                                                                         |
| 11  | [Geographic skew of search terms](11-term-geographic-skew.md)        | Proposal: comparing places with what their digitized pages predict (time-standardized lift with empirical-Bayes shrinkage), with measurements on the published index |
| 12  | [On-prem OCR pipeline](12-onprem-ocr-pipeline.md)                    | Re-reading badly OCR'd pages on local machines (#296): what is built, the flows from queue to search, languages, next pull requests                                  |
| 13  | [OCR engines](13-ocr-engines.md)                                     | The contract between the on-prem OCR worker and its engines, how engines run and are routed, provenance, the survey's use of the same images, and an engine catalog  |
| ADR | [Architecture decision records](adr/)                                | Short records of each major decision and the alternatives rejected                                                                                                   |

The [technical notes](../notes/README.md) are dated write-ups of the work, with its measurements; they say what was true when, where these documents say how the system stands now.

## Executive summary

**What failed before.** The legacy system was a Python 2 / Flask API in front of a self-managed Solr cluster on university-internal hosts. It kept per-user session state in MongoDB and needed hand-run Python scripts to ingest data. Every part depended on one institution's servers and on individual people. When that support went away, nothing could keep it running. It also showed only a **random sample of 500 hits per request**, so the map was a sample rather than a full count. See [01](01-background-and-legacy-analysis.md).

**What we will build.**

```
Browser (React + MapLibre/deck.gl SPA)  ◄── the API app serves the site: usnewsmap.com, TLS
   │  GET /v1/...  same origin; also https://api.usnewsmap.com/v1/...  (cacheable, canonical URLs)
   ▼
One Azure Container App, one replica, two containers:
   Rust API (axum) ── localhost ──► Quickwit searcher        ── in-memory reference data
   ▼                                   ▼
Azure Blob Storage: search index (Hot) · curated Parquet corpus (Cool) · reference data · response cache
   ▲
Ingest: backfill and full rebuilds on ACI Spot containers (preview); weekly increments on Container Apps Jobs
        ◄── LoC Chronicling America bulk OCR + loc.gov API
```

**Main decisions** (details and alternatives are in the ADRs):

1. **The server returns complete aggregates instead of a sample of hits.** The API returns counts for every matching page, grouped by place and time bucket, in one cacheable response. Playback, cumulative and trailing-window views are then computed in the browser, with no per-user server state ([ADR-0003](adr/0003-stateless-aggregate-first-api.md)).
2. **Azure Blob Storage is the system of record.** The corpus is stored as curated Parquet on Blob Storage, and every search index can be rebuilt from it ([ADR-0002](adr/0002-blob-data-lake-system-of-record.md)).
3. **Search engine: Quickwit on Azure Container Apps with its index on Blob Storage is recommended. Azure AI Search is the fully managed alternative behind the same interface.** AI Search is the more "pure PaaS" option, but for a corpus of about 23 million pages and roughly 0.6–1 TB of text it costs about **$2,800–5,600 per month**. The recommended lean deployment, including private networking, costs about **$50–75 per month** in total (typical ~$72). Quickwit also supports nested _place × time_ aggregations in its generally available API; AI Search offers them only in preview. The maintainer makes the final call based on cost ([ADR-0001](adr/0001-search-engine.md)).
4. **The API is written in Rust (axum + tokio).** The API is I/O-bound and runs on scale-to-zero, per-second-billed compute, so Rust's fast cold start, small memory footprint and predictable tail latency lower cost directly. As of May 2026 the Azure SDK for Rust has GA releases for Identity, Blob Storage and Queues. The one gap, AI Search, has no Rust SDK; we would call its REST API directly, which is a small amount of code ([ADR-0004](adr/0004-rust-api.md)).
5. **Cost and operations are hard requirements.** The target is **under $80 per month**; the lean profile runs about **$72 in a typical month**, including private networking and a private image registry. To get there:
   - there is no Front Door; the API app serves the site from its own image ([ADR-0010](adr/0010-site-served-by-the-api.md));
   - the API and search engine share one small Container App in a VNet-integrated environment, reaching Blob Storage and Cosmos DB only through **private endpoints** with **managed identities** ([ADR-0008](adr/0008-private-networking.md));
   - the corpus sits in cool storage;
   - logs stay inside the free tier;
   - images live in GitHub Container Registry.

   There are **no IaaS virtual machines**. Heavy one-off work (the initial backfill and full re-indexes) runs on **Azure Container Instances Spot containers** (preview, up to 70% cheaper), which exist only while that work runs. If Spot capacity isn't available, the fallback is Azure Batch with Spot nodes that scale to zero. All infrastructure is in Bicep, with GitHub Actions using OIDC. The earlier "growth" profile (Front Door, a larger search node, zone-redundant storage; about $180–350 per month) remains documented as an upgrade path. A single maintainer can run it ([ADR-0005](adr/0005-sustainability-constraints.md), [ADR-0006](adr/0006-lean-hosting-profile.md)).

**What users get beyond the original:** accurate full counts, not a 500-hit sample; shareable URLs; comparison of several terms; phrase, all-words, any-word, proximity and fuzzy search to cope with OCR errors; filters by state, title, language and front page only; a _first appearance_ layer for studying how stories spread; keyword-in-context snippets; a coverage layer that separates "no mention" from "no digitized papers"; CSV/GeoJSON export; an embeddable mode; accessible table views; and a documented public API for researchers. See [02](02-product-requirements.md).

## Sources consulted

The legacy source repository, [`tgoodyear/usnewsmap`](https://github.com/tgoodyear/usnewsmap), was read in full. For press and context, some sources could not be fetched directly from the build environment; for those we used search-engine summaries and syndicated copies:

- Newswise / UGA Today, _What going viral looked like 120 years ago_ (the "Cross of Gold" study)
- Washington Post, _The secret, pre-Internet history of "viral" memes_ (Caitlin Dewey, 17 March 2016)
- Library of Virginia, _The UncommonWealth_: _US News Map: Newspaper Coverage over Space and Time_ (20 April 2016)
- Rock Island County Illinois Genealogical Society (RICIGS): _Search newspapers with usnewsmap.com_
- Reddit r/history launch thread (March 2016)
- UGA Franklin College, _Saunt's USNewsMap wins NEH prize_; NEH press release of 25 July 2016; H-Net launch announcement; Jason A. Heppler's review in the _Journal of American History_ 104:1 (June 2017)
- Library of Congress: the Chronicling America migration to loc.gov (4 August 2025), the loc.gov JSON API tutorials, and the Chronicling America Datasets portal and NDNP-Open-OCR reprocessing announcement (The Signal, August 2026)
- Microsoft: Azure AI Search service limits (MicrosoftDocs/azure-ai-docs), the Azure SDK for Rust GA announcement (May 2026), Cosmos DB full-text search GA, and AI Search hierarchical facets (preview)
