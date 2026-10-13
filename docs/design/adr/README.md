# Architecture Decision Records

We use the [MADR](https://adr.github.io/madr/)-style lightweight format. Each record lists its status, the context, the decision, the alternatives we rejected, and the consequences.

| ADR                                             | Title                                                                                                                  | Status                                  |
| ----------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------- | --------------------------------------- |
| [0001](0001-search-engine.md)                   | Search engine: Quickwit on Container Apps + Blob, with Azure AI Search as the managed alternative                      | Proposed; final after Spike S-2         |
| [0002](0002-blob-data-lake-system-of-record.md) | Azure Blob Storage curated Parquet lake as the system of record                                                        | Accepted                                |
| [0003](0003-stateless-aggregate-first-api.md)   | Stateless, aggregate-first, cacheable API; playback in the browser                                                     | Accepted                                |
| [0004](0004-rust-api.md)                        | Rust (axum) for the API and ingest                                                                                     | Accepted                                |
| [0005](0005-sustainability-constraints.md)      | Sustainability constraints: cost ceiling, no VMs, IaC, two admins                                                      | Accepted (ceiling amended by 0006)      |
| [0006](0006-lean-hosting-profile.md)            | Lean hosting profile under $80/month: no Front Door, SWA Free, single container app, Spot ingest                       | Accepted (site hosting amended by 0010) |
| [0007](0007-cosmos-document-state.md)           | Cosmos DB (free tier) for document state (per LCCN, batch, issue, index run)                                           | Accepted                                |
| [0008](0008-private-networking.md)              | Private networking: VNet-integrated env, private endpoints for Blob + Cosmos, guarded public window for Spot backfills | Accepted                                |
| [0009](0009-entra-identity-only.md)             | Entra identity only: RBAC for every caller, local (key) auth off everywhere, no shared keys or SAS                     | Accepted                                |
| [0010](0010-site-served-by-the-api.md)          | The API app serves the site: same origin, Entra-only deploys, no Static Web App                                        | Accepted                                |
| [0011](0011-deployment-stacks.md)               | One deployment stack per environment (delete on removal, deny deletes), without azd                                    | Accepted                                |
| [0012](0012-anonymous-search-log.md)            | Keep an anonymous search log indefinitely: day buckets, shuffled day files, no identifiers                             | Accepted                                |
| [0013](0013-quickwit-from-our-fork.md)          | We build Quickwit from our fork: v0.9.1 plus a pooled Azure Blob client                                                | Accepted                                |
| [0014](0014-one-searched-text-field.md)         | One searched text field for LoC's and American Stories' text, behind a version flag                                    | Accepted (behind a setting)             |
| [0015](0015-onprem-ocr-workers.md)              | On-prem OCR workers: Azure Arc identities, an exchange account with a firewall, Storage queues                         | Proposed                                |
| [0016](0016-pluggable-ocr-engines.md)           | Pluggable OCR engines: a versioned contract, one pinned container per engine, Rust around it                           | Proposed                                |
| [0017](0017-resource-naming.md)                 | Resource naming: usnm-{function}-{type}-{env}, adopted at natural points, stateful prod names kept until a rebuild     | Proposed                                |
| [0018](0018-scan-archive.md)                    | Scan archive: Cold-tier blobs, read through a small API with signed tickets                                            | Proposed                                |
| [0019](0019-boinc-server-hosting.md)            | Hosting for the stock BOINC server that #441's pilot submits to: Container App, MySQL, Azure Files, a bridge job       | Proposed                                |
