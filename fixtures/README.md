# Fixture corpus (synthetic)

**Everything in `data/` is synthetic.** Place and title names are labelled "Fixture", LCCNs use the unassigned `sn99…` range, and the page text is templated. It exists only for local development and tests, and it must never be presented as real Chronicling America data.

It mimics the shapes the real pipeline will produce ([04](../docs/design/04-data-sources-and-ingestion.md)):

- `current.json`: the published version, naming its sealed index set and reference snapshot
- `indexes/{index_id}.jsonl`: one engine document per page (a base index and one delta index)
- `fixture-v1/`: the reference snapshot (`places.json`, `titles.json`, `baselines.json`, `language_baselines.json`, `title_pages.json`, and the `manifest.json` that checksums them)

Regenerate with `python3 fixtures/generate.py` (deterministic).

`skew-vectors.json` is separate: inputs and expected scores for the relative-rate scoring ([doc 11](../docs/design/11-term-geographic-skew.md)), shared by `crates/usnm-core/tests/skew_vectors.rs` and `web/src/engine/skew.test.ts`. Its synthetic cases are generated; `cross_of_gold` and `klondike` are reduced from public API responses for that doc (counts per place index and bucket, no names). Regenerate with `UPDATE_SKEW_VECTORS=1 cargo test -p usnm-core --test skew_vectors` (add `SKEW_RECORDED_DIR` to re-read the recorded searches).

## Recorded Congress responses (`congress/unt/`)

`congress/unt/` is **not** synthetic: it holds responses recorded from the UNT Digital Library on 11 October 2026 for the Congressional Globe fetch (`usnm_ingest::congress::globe`, #229), hand-trimmed so the tests never touch the network. The Globe is a US government publication in the public domain; UNT's metadata describes it.

- `oai-cglob-1.xml`, `oai-cglob-2.xml`: one OAI-PMH `ListRecords` response for `set=collection:CGLOB` (`metadataPrefix=untl`), cut to four of its 112 records (metadc29274, and the 24th Congress's metadc29282, metadc30905 and metadc30907) and split in two after the second record. The real response is a single page; the resumption token `fixture/page2` was added to exercise paging.
- `metadc29274-manifest.json`, `metadc30767-manifest.json`: IIIF manifests with their metadata cut to the first few entries and their canvases to the pages below (metadc29274 keeps canvas 2, which has no OCR file here, so the fixture server answers 404 for it).
- `metadc29274-m1-{seq}-ocr.json`, `metadc30858-m1-100-ocr.json`: per-page OCR annotation lists (`/ark:/67531/{id}/m1/{seq}/annotations/ocr/`), each cut to a few lines of words with their boxes as recorded: the title page (1), the index (3), a Senate page ending in the House's day heading (300), the next page (301), the appendix title page (541), an appendix page (800), and a later volume's running head (metadc30858, 100).
