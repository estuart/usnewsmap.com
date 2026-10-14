# Technical notes

Short, dated write-ups of work on the index, the OCR and the corpus's coverage: what we tried, what we measured, what we decided and what is left. They are the project's engineering log, kept next to the code so a number can always be traced to the version, commit and script it came from.

They sit between the other two kinds of document:

- The [design documents](../design/README.md) say how the system works now. They are kept current.
- The [ADRs](../design/adr/README.md) record a decision and the alternatives rejected.
- A **note** records a piece of work at a point in time, with its measurements. Once written it isn't rewritten: a later note supersedes it and links back. When a note changes what a design document says, the design document changes in the same pull request and names the note.

## Notes

| Date       | Note                                                                                                           | Topic                | Measured on                                  |
| ---------- | -------------------------------------------------------------------------------------------------------------- | -------------------- | -------------------------------------------- |
| 2026-10-14 | [Native ARM64 BOINC worker on the Mac](2026-10-14-buda-native-arm64.md)                                        | ocr, operations      | Native ARM engine/worker; wrapper 22         |
| 2026-10-14 | [BUDA Mac tests without a project server](2026-10-14-buda-mac-offline-tests.md)                                | ocr, operations      | BOINC 8.2.15, Podman 5.5.2, wrapper 22       |
| 2026-10-14 | [BUDA evaluation: validation, worker identity and Mac support](2026-10-14-buda-evaluation.md)                  | ocr, operations      | Pinned BOINC source; Mac wrapper 22 launch   |
| 2026-10-05 | [Corpus coverage: what is searchable, what is counted, what is missing](2026-10-05-corpus-coverage.md)         | coverage             | pages-v20260929-4, pages-v20261003-1         |
| 2026-10-05 | [Japanese pages: OCR of the pages LoC ships without text](2026-10-05-japanese-ocr.md)                          | ocr                  | sample of 23 crops, 1942–45                  |
| 2026-10-05 | [Phrases with common words, and the index layout around them](2026-10-05-phrases-through-common-word-pairs.md) | indexing             | 90,745 pages (11 batches); pages-v20261003-1 |
| 2026-10-05 | [Index versions: what built each one, and comparing them](2026-10-05-index-versions-and-what-built-them.md)    | indexing, operations | pages-v20260928-1 to pages-v20261005-1       |

Topics: `indexing` (index schema, splits, the search engine), `ocr` (text we produce ourselves), `coverage` (what is in the corpus, the baselines and the no-data layer), `operations` (the pipeline, deploys, cost).

## Writing one

Copy [`template.md`](template.md) to `YYYY-MM-DD-slug.md`, fill it in, and add a row to the table above (newest first). Then:

- **Name what was measured.** Every number names the index version, the commit or the data it was measured on, and the script or command that produced it, so someone can run it again. Prefer the repository's own tooling: `scripts/compare-versions.py` for a version's search results, `/v1/status` and `/v1/versions` for the pipeline, the captures under `ops/version-snapshots/`, the Japanese OCR scoring in `scripts/ja-ocr/`.
- **Numbers go in tables**, with the environment they were taken in (the job's 2 vCPU, a laptop, cold or warm caches).
- **Point, don't repeat.** The current description of a mechanism lives in the design documents; the note tells the story and keeps the measurements, and links the section (`05 §5.5.3`) rather than copying it. Link the issue and pull request numbers.
- **Say what is left.** A note ends with what the work didn't settle and what the next measurement should be.
- **The same rules as [CONTRIBUTING](../../CONTRIBUTING.md).** No real search queries from the search log, no production logs with identifiers, no secrets. Aggregate counts from `/v1/status`, the benchmark searches (05 §5.8), the home page examples and the fixtures are all fine.

The notes are plain Markdown, rendered by GitHub. The site is served by the API from its own image ([ADR-0010](../design/adr/0010-site-served-by-the-api.md)), so publishing them on usnewsmap.com would mean building them into the web app; nothing here assumes that.

- [Stock BOINC batch pilot](2026-10-13-boinc-pilot.md): cached-page transport, native ingestion, and validation boundaries.

- [Rootless BOINC volunteer readiness on an SELinux host](2026-10-14-boinc-volunteer-preflight.md): slot labels, kernel limits, pause/restart and a startup gate.

- [Native BOINC worker builds and Mac release tooling](2026-10-14-boinc-native-arm-build.md): native Podman round trip and remaining release/client checks.
