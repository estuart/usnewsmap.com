# Fixture corpus (synthetic)

**Everything in `data/` is synthetic.** Place and title names are labelled "Fixture", LCCNs use the unassigned `sn99…` range, and the page text is templated. It exists only for local development and tests, and it must never be presented as real Chronicling America data.

It mimics the shapes the real pipeline will produce ([04](../docs/design/04-data-sources-and-ingestion.md)):

- `current.json`: the published version, naming its sealed index set and reference snapshot
- `indexes/{index_id}.jsonl`: one engine document per page (a base index and one delta index)
- `fixture-v1/`: the reference snapshot (`places.json`, `titles.json`, `baselines.json`, `language_baselines.json`, `title_pages.json`, `title_dates.json`, and the `manifest.json` that checksums them)

Regenerate with `python3 fixtures/generate.py` (deterministic).

`skew-vectors.json` is separate: inputs and expected scores for the relative-rate scoring ([doc 11](../docs/design/11-term-geographic-skew.md)), shared by `crates/usnm-core/tests/skew_vectors.rs` and `web/src/engine/skew.test.ts`. Its synthetic cases are generated; `cross_of_gold` and `klondike` are reduced from public API responses for that doc (counts per place index and bucket, no names). Regenerate with `UPDATE_SKEW_VECTORS=1 cargo test -p usnm-core --test skew_vectors` (add `SKEW_RECORDED_DIR` to re-read the recorded searches).

`page-language/` is separate too, and not synthetic: real text of known language, from Wikipedia (CC BY-SA 4.0) and Wikisource (public domain in the US), for the page-language detector's ground-truth test (`crates/usnm-core/tests/page_language_samples.rs`, [05 §5.5.8](../docs/design/05-search-and-storage.md)). Its [README](page-language/README.md) has the sources, their licences and how to regenerate it.
