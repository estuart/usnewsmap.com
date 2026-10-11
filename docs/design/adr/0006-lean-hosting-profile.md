# ADR-0006: Lean hosting profile (under $80/month)

- **Status:** Accepted
- **Date:** 2026-09
- **Supersedes:** the $500/month ceiling originally set in ADR-0005

## Context

The maintainer set a target of **under $80/month**. The first design (Front Door, a separate API app, a 4 vCPU / 8 GiB always-on searcher, zone-redundant storage, ACR) came to about $345 in a typical month. The maintainer also asked whether Front Door is needed, and allowed Spot/batch compute and preview features.

## Decision

1. **No Front Door.** _Amended by [ADR-0010](0010-site-served-by-the-api.md): the API app now serves the site from its image, because Static Web Apps can only deploy with a shared token._ **Azure Static Web Apps (Free)** hosts the SPA at `usnewsmap.com`, with free managed TLS and global static content distribution. The API is served from Container Apps at **`api.usnewsmap.com`**, with a free managed certificate. CORS allows the site origin only, for GET requests without credentials.
   - _Option:_ SWA **Standard** ($9/mo) can link the Container App so the API is same-origin under `/api/*`, and adds an SLA. The API mounts its routes at both `/v1/*` and `/api/v1/*` so either setup works without code changes.
2. **One Container App, one replica, two containers:** the Rust API (0.25 vCPU / 0.5 GiB) and Quickwit (1.0 vCPU / 2.0 GiB, **searcher only, metastore read-only**), talking over localhost. Min 1, max 2 replicas. Quickwit has no ingress of its own. Only the `index` job writes the metastore, one at a time ([08 §8.4.1](../08-azure-infrastructure.md#841-quickwit-metastore-one-writer-many-readers)).
3. **Persistent response cache on Blob Storage** (`cache/{index_version}/{hash}.json.zst`). Expensive aggregates are computed once per index version and survive restarts. This replaces most of what Front Door's edge cache did.
4. **Storage tiers:**
   - Hot LRS for the index and reference data;
   - **Cool** LRS for curated Parquet, which is read only during rebuilds;
   - raw LoC archives are **not retained**, because LoC is the source of record and checksums are kept in the Cosmos batch state.
     _Note (October 2026):_ an archival storage account (`infra/archive/`, its own stack outside every environment's) can keep everything downloaded from outside Azure, LoC's batch archives and lists at the Cold tier, for environments that set `USNM_ARCHIVE_ACCOUNT` (off by default). Curation then reads a kept archive instead of LoC, so re-curation, benchmarks (#239) and other environments never download it twice. The decision for production is unchanged ([operations](../../operations.md#archival-storage)).
5. **Free tiers:**
   - GitHub Container Registry instead of ACR;
   - Log Analytics: dev is kept within the free 5 GB/month by a 150 MB daily cap. Prod's normal days (about 53 MB) also fit the free allowance, but since October 2026 its cap is 1 GB a day, so a bulk job can't stop all logging; that cap is a burst-cost ceiling (about 30 GB in a month at the cap), not a guarantee of the free tier. The blob write logs, which the bulk jobs multiply, are on the Auxiliary plan ($0.15/GB), which the cap doesn't apply to (08 §8.1.1);
   - no Key Vault until a secret exists;
     _Note (October 2026):_ the first one exists: the Red Hat activation key the OCR nodes register with (#296). It lives in the OCR nodes' Key Vault, `usnmocrkv{env}{suffix}` (08 §8.3.1), which only the operator reads; #295 plans a vault for the environment's private settings;
   - the Container Apps free monthly grant covers the jobs.
6. **Offline compute on Spot:**
   - the initial backfill and full re-indexes run as **ACI Spot container groups** (preview, East US 2), launched in parallel and claiming batches from Cosmos with leases;
     _Amended September 2026 (after spike S-1):_ the backfill runs as a Container Apps Job with parallel replicas inside the VNet instead. It costs about the same (~$15–20 for ~175 worker-hours of CPU; ~$20–30 once downloads are paced to LoC's limit, since workers are billed while they wait, 04 §4.4) and needs no launcher or public-access window. ACI Spot stays the documented alternative (08 §8.4);
   - the weekly incremental ingest stays on Container Apps Jobs, inside the free grant;
   - Azure Batch with Spot nodes that scale to zero is the documented fallback.
7. **Region: East US 2.** ACI Spot is available there, and so are the higher-capacity AI Search partitions for the growth profile.

## Trade-offs accepted

| Given up                                        | Effect                                                                              | Mitigation                                                                                                                                                    |
| ----------------------------------------------- | ----------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Edge caching of API responses                   | Repeated popular searches hit the container instead of a CDN                        | Browser caching; in-process moka cache; Blob persistent cache; pre-warming                                                                                    |
| WAF / edge rate limiting                        | Abuse reaches the app                                                               | App-level token-bucket rate limit; **max 2 replicas caps the bill**, so abuse degrades speed rather than increasing cost                                      |
| A large searcher (4 vCPU / 8 GiB)               | High-frequency terms (such as `railroad` across all years) may take 5–15 s uncached | Relaxed SLO for that class; Blob cache; UI nudges toward narrower date ranges. Spike S-2 measures it. The first lever to pull is 2 vCPU / 4 GiB (+~$20–30/mo) |
| Redundancy (ZRS, 2 replicas)                    | Availability target 99.0%, no SLA                                                   | Stateless app restarts in seconds; index rebuildable; the growth profile restores these                                                                       |
| Raw archive retention                           | Re-fetch from LoC is needed if curated data is lost                                 | Blob versioning and soft delete on curated data; checksums in the Cosmos batch state                                                                          |
| SWA Free has no SLA; 100 GB/month bandwidth cap | Site stops serving if the cap is exceeded                                           | The SPA bundle is small; tiles are served from Blob, not SWA; upgrade to Standard ($9) if traffic approaches the cap                                          |

## Consequences

- Typical cost is **about $72 per month** with [ADR-0008](0008-private-networking.md) private networking (~$17) and the private registry (~$5) (see [09 §9.5](../09-operations-security-cost.md#95-cost-model-monthly-usd-list-prices-confirm-with-the-azure-pricing-calculator)).
- Azure AI Search, Elastic Cloud and Front Door are outside this budget. They move to the **growth profile**, enabled by configuration if the budget allows.
- ACI Spot is a preview service with no SLA. Backfills are resumable and idempotent, so an eviction only delays the job.
