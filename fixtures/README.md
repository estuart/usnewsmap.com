# Fixture corpus (synthetic)

**Everything in `data/` is synthetic.** Place and title names are labelled "Fixture", LCCNs use the unassigned `sn99…` range, and the page text is templated. It exists only for local development and tests, and it must never be presented as real Chronicling America data.

It mimics the shapes the real pipeline will produce ([04](../docs/design/04-data-sources-and-ingestion.md)):

- `current.json`: the published version, naming its sealed index set and reference snapshot
- `indexes/{index_id}.jsonl`: one engine document per page (a base index and one delta index)
- `fixture-v1/`: the reference snapshot (`places.json`, `titles.json`, `baselines.json`, `language_baselines.json`, `title_pages.json`, `title_dates.json`, and the `manifest.json` that checksums them)

Regenerate with `python3 fixtures/generate.py` (deterministic).

`skew-vectors.json` is separate: inputs and expected scores for the relative-rate scoring ([doc 11](../docs/design/11-term-geographic-skew.md)), shared by `crates/usnm-core/tests/skew_vectors.rs` and `web/src/engine/skew.test.ts`. Its synthetic cases are generated; `cross_of_gold` and `klondike` are reduced from public API responses for that doc (counts per place index and bucket, no names). Regenerate with `UPDATE_SKEW_VECTORS=1 cargo test -p usnm-core --test skew_vectors` (add `SKEW_RECORDED_DIR` to re-read the recorded searches).

`page-language/` is separate too, and not synthetic: real text of known language, from Wikipedia (CC BY-SA 4.0) and Wikisource (public domain in the US), for the page-language detector's ground-truth test (`crates/usnm-core/tests/page_language_samples.rs`, [05 §5.5.8](../docs/design/05-search-and-storage.md)). Its [README](page-language/README.md) has the sources, their licences and how to regenerate it.

## Recorded Congress responses (`congress/unt/`)

`congress/unt/` is **not** synthetic: it holds responses recorded from the UNT Digital Library on 11 October 2026 for the Congressional Globe fetch (`usnm_ingest::congress::globe`, #229), hand-trimmed so the tests never touch the network. The Globe is a US government publication in the public domain; UNT's metadata describes it.

- `oai-cglob-1.xml`, `oai-cglob-2.xml`: one OAI-PMH `ListRecords` response for `set=collection:CGLOB` (`metadataPrefix=untl`), cut to four of its 112 records (metadc29274, and the 24th Congress's metadc29282, metadc30905 and metadc30907) and split in two after the second record. The real response is a single page; the resumption token `fixture/page2` was added to exercise paging.
- `metadc29274-manifest.json`, `metadc30767-manifest.json`: IIIF manifests with their metadata cut to the first few entries and their canvases to the pages below (metadc29274 keeps canvas 2, which has no OCR file here, so the fixture server answers 404 for it).
- `metadc29274-m1-{seq}-ocr.json`, `metadc30858-m1-100-ocr.json`: per-page OCR annotation lists (`/ark:/67531/{id}/m1/{seq}/annotations/ocr/`), each cut to a few lines of words with their boxes as recorded: the title page (1), the index (3), a Senate page ending in the House's day heading (300), the next page (301), the appendix title page (541), an appendix page (800), and a later volume's running head (metadc30858, 100).

## Recorded Congress responses (`congress/govinfo/`)

`congress/govinfo/` is **not** synthetic either: it holds responses recorded from GovInfo on 11 October 2026 for the Serial Set fetch (`usnm_ingest::congress::serialset`, #229 phase 2), hand-trimmed so the tests never touch the network. The Serial Set is a US government publication in the public domain. No PDF is committed: the tests stand in a placeholder for each PDF and read the page texts below in place of `pdftotext`, and the one test of the real `pdftotext` (skipped where it isn't installed) writes a tiny PDF of its own.

- `SERIALSET_sitemap_index.xml`: the sitemap index (`/sitemap/SERIALSET_sitemap_index.xml`), cut to two of its 182 yearly sitemaps, 1820 and 1990, with their `lastmod`.
- `SERIALSET_1820_sitemap.xml`, `SERIALSET_1990_sitemap.xml`: those years' sitemaps, cut to seven of their 197 and 837 packages: the volume packages of serials 25, 33 and 13993, serial 25's first document (which has no MODS file here, so the fixture server answers 404 for it), serial 33's documents 9 and 20 (H. Doc. 16-42 and 16-53), and serial 13993's document 21 (H. Doc. 101-183).
- `{package}-mods.xml`: the MODS of those three documents (`/metadata/pkg/{id}/mods.xml`), as recorded.
- `{package}-pages.txt`: what `pdftotext -enc UTF-8` printed for the first three pages of each document's PDF, a form feed after each page, each page cut to its first 16 to 30 lines. They keep GovInfo's `¬` line-end hyphens, a page with one stray letter (16-42's page 2) and a blank page (16-53's page 2).
