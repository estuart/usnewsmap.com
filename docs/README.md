# Documentation

## Guides

| Guide                                | For                                                                                                                                 |
| ------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------- |
| [Development](development.md)        | Running and testing locally: the API and the site on the synthetic corpus, Quickwit, the ingest pipeline, the local Azure stand-ins |
| [Configuration](configuration.md)    | The API's environment variables                                                                                                     |
| [Operations](operations.md)          | Running the ingest pipeline in Azure: the backfill, releases, full rebuilds, pruning old index versions, logs and the search log    |
| [Infrastructure](../infra/README.md) | What the Bicep deploys, standing an environment up, settings, DNS, the container registry                                           |
| [Web app](../web/README.md)          | The front end: scripts, build settings, how it works                                                                                |
| [Fixtures](../fixtures/README.md)    | The synthetic corpus the tests and local development use                                                                            |
| [Contributing](../CONTRIBUTING.md)   | The checks to run and the repository's rules                                                                                        |

## Design

The [design documents](design/README.md) (01–11) record the background, requirements and architecture, and the reasoning behind each part of the system; the [architecture decision records](design/adr/README.md) record each major decision and the alternatives rejected. References such as "05 §5.5.1" in the code and docs point into them.

## Notes

The [technical notes](notes/README.md) are dated write-ups of work on the index, the OCR and the corpus's coverage: what was tried, what was measured, what was decided and what is left. The design documents say how the system stands now; a note says what was true when.
