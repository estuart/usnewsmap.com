# ADR-0018: Scan archive: Cold-tier blobs, read through a small API

- **Status:** Proposed
- **Date:** 2026-10 (maintainer's decision of 13 October to put the scans in Azure)
- **Extends:** ADR-0015 (on-prem OCR workers, decision 9, scans from a local cache) and ADR-0009 (Entra identity only)
- **Issue:** #442

## Context

The keep-set scans exist in one place: the ZFS dataset `tank/ocr/scans/keep/` on the Proxmox host that runs the fetcher. The buffer set is deleted once its page is OCRed (12 §12.2.4). Two things are wanted:

1. An off-site copy of the keep set. LoC's storage service is the only other copy, and the fetcher spent days getting each page from it within the source's pacing limits.
2. A source for the contributor-compute (BOINC) pilot (#441) that we control, so the success rate of a fetch does not depend on the source's availability and we do not send many volunteers to LoC.

The maintainer's idea was the `$web` container of a storage account (static website hosting) with a small API in front.

### Measurements

Taken read-only on 13 October 2026, on the host that holds the scans. Nothing was copied off it.

| Item                      | Keep set (`keep/`)                                                        | Buffer set (`buffer/`) |
| ------------------------- | ------------------------------------------------------------------------- | ---------------------- |
| JP2 files                 | 37,727                                                                    | 0                      |
| Full-size IIIF JPEG files | 5,925 (fetched before the batch route)                                    | 0                      |
| Sidecar `.json` files     | 43,653 (one per scan)                                                     | 0                      |
| Bytes, apparent           | 226,186,742,458 (226.2 GB, 210.7 GiB)                                     | 0                      |
| JP2 bytes                 | 191,914,935,322 (mean 5.09 MB, median 4.64 MB, p95 8.24 MB, max 12.66 MB) | 0                      |
| JPEG bytes                | 34,255,462,523 (median 5.03 MB, p95 10.48 MB)                             | 0                      |
| Space used on disk (ZFS)  | 213 GiB used for 234 GiB logical: lz4 ratio 1.10x                         | none                   |
| Dataset free space        | 2.79 TiB of the dataset's 3.0 TiB; pool 4.31 TiB free of 4.53 TiB         | the same pool          |

JP2 does not compress, so the archive holds about as many bytes as the dataset's apparent size. The fetcher was still running, at about 1,650 pages an hour: its status file says 43,715 of 224,848 keep pages finished and `buffer_pages` 0, `buffer_cap` 1 TB.

**Projection for the full keep set.** The fetcher's status file lists `keep_pages: 224848` for the manifest `pages-v20261010-2` (the keep set of the prefetch manifest, 12 §12.2.4). At the 5.18 MB per page observed so far (226.3 GB over 43,715 pages) that is 1.16 TB; at the JP2 mean, 1.14 TB. This is a projection: the fetcher takes German and the worst English batches first, so later groups can differ in size (UNVERIFIED).

### Prices

Pulled from <https://prices.azure.com/api/retail/prices> on 13 October 2026, region eastus, pay-as-you-go list price, General Block Blob v2, LRS.

| Meter                                                               | Price                                                                                                                                                 |
| ------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------- |
| Cold data stored                                                    | $0.0036 per GB-month                                                                                                                                  |
| Cold write operations                                               | $0.18 per 10,000                                                                                                                                      |
| Cold read operations                                                | $0.10 per 10,000                                                                                                                                      |
| Cold data retrieval                                                 | $0.03 per GB                                                                                                                                          |
| Cold early delete                                                   | $0.0036 per GB (the minimum is 90 days, prorated; Learn)                                                                                              |
| Cold data write                                                     | $0                                                                                                                                                    |
| Hot data stored, for comparison                                     | $0.0208 per GB-month (the Blob Storage product lists $0.0184), reads $0.004 per 10,000, no retrieval fee                                              |
| Cool data stored, for comparison                                    | $0.0152 per GB-month, reads $0.01 per 10,000, retrieval $0.01 per GB                                                                                  |
| Internet egress (routing preference Internet)                       | first 100 GB a month $0, then $0.08 per GB to 10 TB                                                                                                   |
| Egress within the region, from storage to an app in the same region | not found in the API as a charged meter: UNVERIFIED; Learn says only transfers out of a region are billed                                             |
| Front Door Standard                                                 | $35 base fee a month plus per-request and transfer meters; the transfer-out meters for the zones listed in the API are not unambiguous, so UNVERIFIED |
| Azure CDN from Microsoft, Standard                                  | $0.081 per GB in zone 1, $0.60 per million requests                                                                                                   |

The first 100 GB of egress a month is free. If other Azure traffic already uses it, the figures below are higher by up to $8 (UNVERIFIED how much is used).

### Platform facts, from Microsoft Learn

- Static website hosting: "Files in the **$web** container are case-sensitive, served through anonymous access requests and are available only through read operations." "A static website only supports anonymous read access for files in the **$web** container." "AuthN and AuthZ are not supported." Headers cannot be set without Azure CDN, and HTTPS on a custom domain needs Azure CDN. CORS is not supported. The storage firewall does apply to the static website endpoint ([static website hosting](https://learn.microsoft.com/en-us/azure/storage/blobs/storage-blob-static-website)).
- Default access tier: "The default access tier setting can be set to either hot, cool or cold." The brief for this work assumed Hot and Cool only; the current page says Cold is allowed, and Archive is not. A blob's tier can also be set when it is uploaded. Moving a blob to a cooler tier later is billed as a write on the destination tier, and "Early deletion charges also occur if the entire object is rewritten through any operation (i.e. Put Blob, Put Block List, or Copy Blob) within the specified time window" ([access tiers](https://learn.microsoft.com/en-us/azure/storage/blobs/access-tiers-overview)). So a Hot upload followed by a tier change is a write twice and risks early-delete charges. Cold at upload, and as the account default, avoids both.
- BOINC does not authenticate the downloading host to the download server: its code signing covers app version files, and "the client verifies that the file matches the signature" ([BOINC code signing](https://github.com/BOINC/boinc/wiki/Code-signing); the page says nothing of per-host download auth, so a download server that checks the caller is not a BOINC feature, UNVERIFIED beyond that). Whether the client tries a file's several `<url>` entries in order or at random is UNVERIFIED; the design below does not rely on it.

### Costs now

Everything below is added to the existing Azure costs. Nothing here displaces an existing cost.

## Decision

1. **A dedicated storage account, `usnmscansst{env}{sfx}`** (ADR-0017: function word `scans`, type `st`; `usnmscansstprodffapzy` in prod), not the data lake. The lake is private-endpoint only, has versioning and soft delete for curated data, and is read by Quickwit. The scans have a different access pattern and no use for either. The account:
   - StorageV2, Standard_LRS, **default access tier Cold**, shared key off, `defaultToOAuthAuthentication`, public blob access off, TLS 1.2, `allowCrossTenantReplication` off.
   - Public network access on, firewall default Deny, no trusted-service bypass, allowing only the node IPs (`USNM_OCR_NODE_IPS`) and the Container Apps subnet through its service endpoint: the same shape as the OCR exchange account, and exempt from the public-network policy by resource id the same way.
   - **Blob versioning off.** Soft delete off for blobs. A scan is a copy of LoC's file and can be fetched again, and versions or soft-deleted bytes would be billed at Cold rates for nothing. Container soft delete stays on for 7 days so a deleted container can be restored.
   - One container, `scans`, private. No `$web`.
2. **Blob names mirror the local layout:** `<set>/<batch>/<lccn>/<date>/<edition>/<seq>.<jp2|jpg>`. The sidecar's fields are the blob's metadata (`doc_id`, `batch`, `batch_version`, `loc_url`, `iiif_base`, `resolution`, `fetched_at`, `bytes`, `sha256`, `format`, `md5`), not a second blob: it saves one write per scan (224,848 for the full keep set) and keeps a scan and its provenance in one object. Metadata keys use underscores because Azure metadata names must be valid identifiers. `Content-Type` is `image/jp2` or `image/jpeg`, and `Cache-Control` is `public, max-age=31536000, immutable`.
3. **Write once, by role.** The uploader is the OCR nodes' Entra group (the same group as the other node roles; pve-big's Arc identity is added to it). It gets a custom role, `usnm-scans-writer-role-{env}`: read and create blobs in `scans`, no delete, like `usnm-ocr-node-writer`. Uploads use `If-None-Match: *`, so an existing blob is never overwritten. The deployment stack's deny settings protect the account from deletion. A time-based immutability policy is not used: a locked policy cannot be undone, the bytes are re-fetchable, and the role already stops the uploader from deleting. Re-evaluate if a second writer is ever added (open question 4).
4. **Integrity.** The uploader checks the local file against its sidecar (size and sha256) before it uploads. It sends `Content-MD5`, which the service checks against the received bytes and rejects on a mismatch, and stores the same value as the blob's content MD5. `--verify` (which ignores the state file) then reads the blob's properties and compares size, content MD5, tier and the stored `sha256` metadata with the local file. It does not download the blob: a download would be billed as Cold retrieval, $0.03 per GB, so about $35 for the whole keep set.
5. **The `$web` verdict: no.** See the comparison below. The scans are public domain (UNVERIFIED in this ADR), so confidentiality does not decide it. Cost, control and fit do:
   - The static website endpoint is anonymous and cannot be fronted by authentication. Anyone with a page's URL, or a crawler walking the set, is served at our cost: a full pass over the keep set is 1.16 TB read, $35 of Cold retrieval plus about $85 of egress, so about $120, repeatable at will.
   - No per-client or global budget, no rate limit, no 429, and no response headers of our own without Azure CDN. The only brake is the IP firewall, which would also stop the volunteers.
   - Custom-domain HTTPS and header control need Azure CDN or Front Door. Front Door Standard alone has a $35 base fee a month, to cache objects that BOINC reads about once each.
   - It is not needed: a private container read through our own API does everything `$web` does, with a budget.
6. **Read path: a small API that streams the blob with its managed identity** (option B below). It is a plain Rust axum service on the existing Container Apps environment, its own app, `usnm-scans-ca-{env}`, with its own identity holding Storage Blob Data Reader on the `scans` container. It is not part of the search API: scan bytes would compete with search for the replica, and search latency comes first (ADR-0006). `GET /v1/scans/<set>/<batch>/<lccn>/<date>/<edition>/<seq>.<ext>` streams the blob, with `ETag`, `Content-Length`, the stored MD5 in a `Content-MD5` header, `Cache-Control` as stored, and ranges. A miss is 404 and an over-budget caller 429 with `Retry-After`, so the client goes to the source as the design says. The API holds no database. The API is a later pull request, not part of this one.
7. **Who may read: the bridge by identity, and later clients by signed ticket.** In the pilot of #441 the volunteer's container runs with `--network=none` and one page per task: the central `usnm-boinc` bridge stages each page's scan (the `scan` of a job: url, sha256, bytes, md5) before it creates the task. The bridge reads the archive with its own Entra identity (Storage Blob Data Reader on `scans`), so the pilot needs neither the API nor a ticket, and the volunteer is never given a credential. A later client that fetches for itself, which needs a network, reads through the API with a signed per-task ticket, checked with a public key. A BOINC task already reaches only a host the scheduler authenticated, and the ticket is added to the task's input files, which never include an account key:
   - Fields: task (workunit) name, app name and version, the page ids or blob names the task may read, `issued_at` and `expires_at` (a little past the task's `delay_bound`), a key id. Ed25519 signature over a canonical JSON encoding with sorted keys.
   - The API holds only the public key (or keys, by key id, from its configuration) and keeps no per-volunteer secret. It checks the signature, the expiry, and that the requested blob is in the ticket. The private key is in Key Vault, read by the work generator through its managed identity. It is never in the repository and never on a volunteer machine.
   - Limits, per ticket, in memory: requests and bytes of at most the ticket's page count times a small multiple (3 by default, for retries and replicas on one ticket); per IP; and a daily byte limit. Over any of them: 429 with `Retry-After`. The counters are in memory, so each replica counts alone and a restart or scale-to-zero resets them: the daily limit is a brake on one replica, not a cap on the account's cost. The ticket expiry and a low maximum replica count in the app's configuration bound the damage; a limit that holds across replicas and restarts needs a durable shared counter (a table row per day, say), which this design does not include and which is open question 2.
   - A ticket is shared by all replicas of a workunit and works for whoever holds it until it expires. Keep expiry close to the deadline, and keep the caps small. The in-memory limits reduce misuse; they do not guarantee a cost.
   - Compared with the alternatives: **(a)** presenting the volunteer's account key makes the API read every volunteer's authenticator in transit and ties it to the BOINC database: rejected. **(b)** A ticket per result or host needs a scheduler plugin or a transitioner hook: a future option if per-workunit tickets are abused. **(c)** BOINC's own download server with project-signed files: BOINC signs app files and does not authenticate the downloading host (above), so it gives no per-client limit: rejected as the access control.
8. **Fetch order.** The bridge, and any later client with a ticket, reads the archive first and checks the bytes against the job's sha256 and md5. On a 404, 429, 5xx, timeout or checksum mismatch it falls back to the source under the existing low rate. This replaces a staging account with SAS URLs (ADR-0009: no SAS): the archive is the staging copy.
9. **The OCR nodes' flow does not change.** They keep reading `serve-scans` on the LAN. Nothing reads from Azure on that path.
10. **Uploads run on the host that holds the scans** (`usnm-ocr-worker archive-scans`), from a systemd one-shot or timer, never through a laptop. It signs in with the Arc identity of the host and uploads a keep scan once its sidecar exists.
11. **Only the keep set is archived.** The buffer set is a rolling cache that is deleted within hours of OCR. Archiving it would charge the 90-day minimum for each page: at the fetcher's current 1,650 pages an hour that is 1.2 million pages and 6.1 TB a month, about $65 of early-delete charges and $22 of writes, to keep files that are gone in a day. `--set buffer` exists in the tool for a test, and the service-side lifecycle rule below stays off for it.

### The options compared

| Option                                                   | Auth                                | Cost to us                                                                                                   | Control                                                              | Verdict                 |
| -------------------------------------------------------- | ----------------------------------- | ------------------------------------------------------------------------------------------------------------ | -------------------------------------------------------------------- | ----------------------- |
| (a) `$web` with Azure CDN or Front Door                  | none (anonymous only, documented)   | Front Door $35 a month base, or CDN $0.081 per GB plus requests, plus the egress and retrieval of every miss | none: no budget, no rate limit, no 429                               | reject                  |
| (b) Private container, API streams with its identity     | signed ticket, public-key check     | one small app (below), plus retrieval and egress, slowed by the per-replica daily limit                      | per ticket, per IP, global; 404 and 429 give the client its fallback | **recommended**         |
| (c) Private container, API returns a user-delegation SAS | same ticket, then a SAS to the blob | no API egress, but a SAS is a bearer URL anyone can pass on until it expires                                 | weak after the SAS is issued; breaks the standing rule against SAS   | reject unless (b) fails |
| (d) BOINC's data server pointed at the account           | none per host                       | a proxy that is (b) anyway                                                                                   | none                                                                 | reject                  |

Cost of (b)'s API: the app is a Container App on the existing environment. At 0.25 vCPU and 0.5 GiB with a minimum of one replica, idle cost is about $6 a month at idle rates of $0.000003 per vCPU-second and per GiB-second (taken from the contributor-compute design, UNVERIFIED against the price list); scaled to zero it is about $0 and the first request after a quiet period waits for a start, which the client treats as a slow response and retries. Streaming a page takes a few seconds of active time: about $0.00002 a page, so $2 per 100,000 pages (an estimate, UNVERIFIED until measured). The environment's free monthly grant (180,000 vCPU-seconds) is not subtracted.

## Cost

Storage per month, at Cold list prices, GB meaning 10^9 bytes (Azure may bill GiB: the figures are about 7% high if so, UNVERIFIED).

| Case                                        | Size     | Cold per month | Hot per month |
| ------------------------------------------- | -------- | -------------- | ------------- |
| Keep set today (43,653 scans)               | 226.2 GB | $0.81          | $4.71         |
| Full keep set (224,848 pages, projected)    | 1,164 GB | $4.19          | $24.21        |
| Full keep set plus a full 1 TB buffer, held | 2,164 GB | $7.79          | $45.01        |

The last row is a ceiling that is not recommended; the real buffer cost is the churn, in decision 11.

One-time, for the keep set: 224,848 Put Blob writes at $0.18 per 10,000 is $4.05. Upload bytes are free. A full `--verify` makes one properties read per blob (224,848 reads at $0.10 per 10,000 is $2.25 per pass; UNVERIFIED which meter Get Blob Properties uses) and reads and hashes every local file. The daily upload is incremental instead: the uploader keeps a state file of the scans it has archived, with each one's size, nanosecond modification time, inode and change time (so a same-sized replacement is noticed), and skips a scan that is listed and unchanged without reading it or sending a request, so a day with nothing new makes no requests. A new or changed scan is checked and looked up as before. `--verify` ignores the state file and runs as its own monthly unit. The upload and verify units share one lock file, so a verify never runs alongside an upload and reports scans not yet uploaded as missing. An existing blob counts as present only if its stored MD5 is there and equal to the local file's; only then is a scan recorded in the state file. Early deletion does not arise: nothing is deleted or rewritten.

Reads, per page of 5.09 MB (the JP2 mean): Cold retrieval $0.00015, one read operation $0.00001, internet egress $0.00041 once the free 100 GB is used. Per month:

| Pages read a month | Data     | Retrieval | Read operations | Egress | Total   |
| ------------------ | -------- | --------- | --------------- | ------ | ------- |
| 20,000             | 102 GB   | $3.05     | $0.20           | $0.14  | $3.39   |
| 100,000            | 509 GB   | $15.26    | $1.00           | $32.70 | $48.96  |
| 200,000            | 1,017 GB | $30.52    | $2.00           | $73.39 | $105.91 |

These include no API compute and take the whole 100 GB free allowance as unused. Cold costs more than Hot to read and less to hold: counting retrieval fees only, it is cheaper than Hot as long as less than about 57% of the set (49% against the $0.0184 Hot meter) is read in a month, and cheaper than Cool below about 58%. Counting read operations too, the figures are about 54% (Hot) and 53% (Cool). The pilot reads 20,000 pages a month, 9% of the set. The BOINC plan reading under a fifth of the set a month is UNVERIFIED, so Cold is the right tier only if that holds.

**What it adds to the bill.** With the keep set complete and BOINC idle: about $4 a month for storage, no more. Each 100,000 pages read adds about $49, so the pilot's 20,000 pages add about $3. The API's daily byte limit slows read cost per replica but does not cap it (decision 7); the ticket expiry and a low maximum replica count bound it.

Cost of keeping the data on the Proxmox host: nothing new. The archive is a second copy, not a replacement. The local copy stays, because the nodes read it for free.

## Consequences

- One new storage account and role, a few diagnostic settings, and (later) one container app and its identity.
- Uploading 1.16 TB takes about 26 hours at 100 Mbps and about 52 hours at 50 Mbps. The home uplink speed is not measured (UNVERIFIED). The uploader resumes and skips blobs that match.
- #441 stages from the scan server or LoC today. Reading the archive first is a change to the `usnm-boinc` export and submit steps (a job's `scan.url` can stay the LoC URL as provenance; the bytes come from the archive). The server infrastructure being reworked in #439 gets nothing from this ADR but a Storage Blob Data Reader grant for the bridge's identity.
- The scan server on the LAN stays as it is.

## Alternatives rejected

- **`$web`**: above.
- **Hot or Cool tier**: more expensive to hold; Cold is cheaper unless over half the set is read each month.
- **Archive tier ($0.00099 per GB-month)**: needs rehydration of up to 15 hours, so BOINC cannot read from it.
- **SAS from the API (option c)**: a bearer URL we cannot take back, and the rule is no SAS.
- **The data lake account**: private endpoint only, with versioning and soft delete that this data does not need.
- **Immutability policy**: see decision 3.
- **Archiving the buffer set**: decision 11.

## Open questions

1. Where does the ticket signing key live, once clients fetch for themselves? A Key Vault read by whatever signs tasks. The archive API only needs the public key.
2. The daily byte limit: a number the maintainer sets. A first proposal is 20 GB a day per replica (about 4,000 pages). Whether to add a durable shared counter so the limit holds across replicas and restarts is part of this question.
3. Does the fetch-and-ocr client tolerate a `Retry-After` of minutes, or does it go straight to the source? The design says to the source at a low rate.
4. Should the uploader's identity be a separate Entra group from the OCR nodes? pve-big's Arc identity added to the nodes' group also gets the queue roles; a second group would be narrower. It costs one `group.sh` run.
5. What does the free 100 GB of egress already cover on the subscription?
6. How fast is the uplink from the host? It decides whether the first upload is a night or a weekend.
7. Does a later network-enabled client get a ticket per task, or does the bridge keep staging every page? The pilot stages, so this can wait.
8. Should the keep set's `.jpg` files (5,925 IIIF JPEGs fetched before the batch route) be re-fetched as JP2s and the JPEGs left out of the archive? They are archived as they are for now.
