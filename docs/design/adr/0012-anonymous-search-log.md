# ADR-0012: Keep an anonymous search log indefinitely

- **Status:** Accepted
- **Date:** 2026-09
- **Amends:** 09 §9.4.2, which kept no search text beyond daily aggregates with k ≥ 5

## Context

The maintainer wants to know what people search for, over years: "We should be storing the search terms forever. Don't need to store them alongside any identifying information though." Until now search text reached storage only by accident (the Quickwit sidecar's info log, 30 days, fixed in PR #58), and the design promised daily aggregates with k ≥ 5 that were never built.

Two things make a naive log identifying on a small site:

- **Time.** `AppPageViews` (kept 90 days) has each page view's exact time and an approximate location from the client address. On a site with a few dozen visitors a day, a search stored with its minute, or even its hour, can often be matched to the one page view from that city in that window.
- **Order.** Records written as they happen keep their time in their position in the file, and the storage account's write logs (`StorageBlobLogs`, 30 days) time each write.

## Decision

- **What is kept.** One JSON line per search: the words as submitted (trimmed, whitespace folded, at most 256 characters), the canonical query, the match mode, `near` and `fuzzy`, the date range, state, title and language filters, front pages only, the bucket, the number of pages matched, the index version, the UTC day, the source (`api` or `import`) and a format number. Nothing else: no IP address, user agent, referrer, cookie, request, session or user id, location, or time of day. Schema: `Record` in `crates/usnm-api/src/searchlog.rs`.
- **Day, not hour.** An hour holds a handful of visitors from a handful of places, so an hour and a place often point to one page view. A day holds all of that day's visitors, and the only day on which a search could be tied to one page view is a day with one visitor.
- **What counts as a search.** The API records a search when it answers `/v1/aggregate` with a 200, from a cache or not. The site requests that endpoint once per search; coverage and hits requests are not counted. Not recorded: the cache warm-up (it calls the handler code directly), `DNT: 1` or `Sec-GPC: 1`, crawler, script and headless user agents (the page-view classifier from 06 §6.3.7), and requests not from the site's own pages (`Origin` other than `https://{site}`, `Sec-Fetch-Site` other than `same-origin`, or neither header).
- **Order is removed.** Each replica batches records in memory and writes each day's batch, shuffled, as its own blob, `searches/staging/{day}/{random id}.jsonl`, create-only, every 5 minutes and at shutdown. A failed write is retried under the same name, so a write whose response was lost isn't stored twice, and records not staged 45 minutes after their day ends are dropped. An hour after a UTC day ends, a replica lists that day's batches, shuffles all of their lines together and writes them once, create-only, to `searches/days/{day}.jsonl`. A lifecycle rule deletes staged batches 7 days after they're written. Blob soft delete (3 days since 12 October 2026, account-wide; 14 before, #245) keeps a deleted batch recoverable for three more days, so a staged batch exists for up to about 11 days. The permanent file says nothing about when in the day a search ran.
- **Never slows a search.** The handler queues the record without waiting (a bounded queue of 1,024); a full queue drops it and counts it. A failed write keeps the batch for the next try, up to 20,000 records. Losing a batch in a crash is accepted. Errors are logged with the blob path and a count, never a record.
- **Storage.** The `searches` container on the private data account (ADR-0008, ADR-0009), with no expiry rule on `days/`. The API's identity has a custom role on that container only: list, read and create blobs, no delete. RBAC can't make blobs write-once, so the API relies on create-only writes (`If-None-Match: *`) for every blob. Operators listed in `USNM_SEARCH_LOG_READERS` get Storage Blob Data Reader on the container.
- **Reading.** `scripts/searches.sh --env <env> [--days N]` downloads the files with the operator's Entra sign-in and prints searches per day, top queries and queries that found nothing. Nothing public shows raw queries. Anything ever published from this log must be k-anonymized with k ≥ 5: only queries searched at least five times, and no rare combination of query and filters.
- **Before the log.** The searches the Quickwit sidecar logged before this existed were not imported; the log starts when the API began writing it.

## Alternatives

| Option                                          | Why not                                                                                                                                                      |
| ----------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Hour buckets                                    | Joinable with `AppPageViews` time and location at this site's traffic                                                                                        |
| Append straight to the permanent day file       | The file order and the storage write logs would time each batch to within the flush interval, and retrying an append whose response was lost stores it twice |
| One staging append blob per day                 | Retries duplicate records, and the day file could be written while another replica is still appending                                                        |
| Hold a whole day in memory and write it once    | Every restart, deploy and scale-in would flush early anyway, and a crash would lose up to a day                                                              |
| Daily aggregates with k ≥ 5 only (the old plan) | Rare searches, the ones that show what people look for, would never be kept                                                                                  |
| Application Insights custom events              | The same store as page views, with their timestamps, and 90-day retention                                                                                    |

## Consequences

- Search text is now kept indefinitely. The privacy page says so (`/privacy`), and that searches sent with Do Not Track or Global Privacy Control are not recorded.
- For up to about 11 days (7 days, a day as a previous version, then 3 days of soft delete; about 22 days before soft delete was cut from 14 days on 12 October 2026, #245), the staged batches keep their creation times, and for 30 days the storage write logs time each write. An operator who can read both, and `AppPageViews`, could place a search within a 5-minute window during that time. The permanent record carries only the day.
- A search repeated in a browser within its cache lifetime isn't sent again, so it isn't counted again.
- Changing a bucket or a filter is a new request to `/v1/aggregate`, so it is counted as a new search.
- A search slower than a visitor waits is answered `202` until it is ready and the browser asks again (06 §6.3.5). Only the 200 is recorded, so it counts once; if the visitor leaves before then, it isn't recorded.
- The cache warm-up reads the day files to run the most frequent recent searches again (06 §6.5). It logs them by rank, never by text. As with a visitor's search, a slow one's response, which includes the canonical query, is written to the Blob cache for that index version.
- Browsers that send neither `Origin` nor `Sec-Fetch-Site` on a same-origin fetch (Safari before 16.4) are not recorded.
- Removing a query later (someone searched their own details) means an operator grants themselves Storage Blob Data Contributor on the container and rewrites that day's file; the API can't delete.
