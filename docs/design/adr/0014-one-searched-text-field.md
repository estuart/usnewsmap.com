# ADR-0014: One searched text field for LoC's and American Stories' text

- **Status:** Accepted (built behind a setting; the full rebuild that turns it on waits for #283's measurements)
- **Date:** 2026-10
- **Amends:** 05 §5.5.4 (American Stories' text in fields of its own)

## Context

Since American Stories' text (#218), the main index has four positional text fields: LoC's `text` and its common-word pairs `text_cg`, and American Stories' `text_as` and `text_as_cg`. Each term of a query is looked up in both texts' fields, so a cold search warms two term dictionaries and two posting lists per word in every split. On 9 October, on production, switching American Stories' fields off halved a cold search's backend time (mean 15.8 s against 32.3 s) and took the median from 21.4 s to 13.1 s (#251). The patched Quickwit (ADR-0013) cut cold searches to a 3.5 to 3.8 s median, but common-word phrases over the whole range still took 14 to 45 s, and their cost is per field.

Two more facts shaped the design. Quickwit 0.9.1's `remove_long` filter in our `usnm_text` tokenizer drops tokens of 255 bytes or more, not the 40 characters `usnm_core` assumes (#286). And the aggregate's `total.american_stories_only` is a count of `(both texts) AND NOT (LoC's text alone) AND (American Stories' alone)`, which needs LoC's text indexed on its own.

## Decision

- **A version can search both texts in one field** (`text_layout: 2` in `current.json`, 05 §5.5.7). `text_all` holds LoC's words, 32 positions no query can match, then American Stories' words; `text_all_cg` holds each text's common-word pairs the same way. `text` and `text_as` are stored for snippets and `matched_in`, not indexed. A query looks each word up once.
- **The release writes the tokens**, one per word as the version's analyzer folds them, with `_` where a word is dropped and in the gap, and the fields use Quickwit's `whitespace` tokenizer, as the pairs field already does. A gap of dropped filler words would cost at least 255 bytes a position, and a shorter filler word would be indexed and reachable by a prefix query.
- **`total.american_stories_only` is not counted on such a version.** Hits keep `matched_in` and `snippet_source`, which the API works out from the stored texts.
- **`USNM_AMERICAN_STORIES_SEARCH=false` has no effect on such a version**: the API logs a warning and searches both texts.
- **The API serves both layouts**, and refuses to load a layout it doesn't know. **Only a full base changes the layout** (`--single-text-field`, `USNM_SINGLE_TEXT_FIELD`, off by default); a delta follows the published version, as for the analyzer (#168, #278) and the decades (#123).

## Alternatives

| Option                                                          | Why not                                                                                                                                                          |
| --------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Index only where American Stories differs from LoC              | Measured on a 10% sample: the lossless delta (2 words of context) keeps 80 to 91% of American Stories' tokens, so it saves 5 to 20% of its postings (#251, #253) |
| `text_all` analyzed by `usnm_text`, gap of dropped filler words | At least 255 bytes per gap position (about 8 KB a page with both texts); a shorter filler is indexed and a 5-letter prefix query reaches it                      |
| Quickwit's `concatenate` field or an array of the two texts     | Tantivy leaves a gap of one position between values, so a NEAR search could span the texts                                                                       |
| Keep `text` indexed for the American Stories-only count         | Two texts' postings in the index again, and the count request reads them on every computed aggregate                                                             |
| A field of the words only American Stories has, for the count   | Exact for words and `AND` only; wrong for phrases, NEAR, prefixes and `NOT`                                                                                      |

## Consequences

- **Fewer lookups per search**: one term dictionary and posting list per word instead of two; three lookups for `"cross of gold"` instead of six; one request fewer per computed aggregate. The posting lists are longer than LoC's alone, so the expected cold-search cost is near, not at, the "American Stories off" timing. To be measured on the dev 1% set before the rebuild (#283).
- **The index should be no larger**: the same words with positions in one dictionary, and OCR runs of 41 to 254 bytes become `_` instead of a term each.
- **The indexed words are `usnm_core`'s**, the ones the query parser, the memory backend and the snippets use, rather than Quickwit's analysis of the raw text. Quickwit no longer analyzes queries a second time on these fields.
- **The site loses the page-count note** about pages only American Stories' text matches; each hit keeps its badge.
- **American Stories' text can't be switched off at query time** on such a version. Rolling `current.json` back to a version with a field per text, or a full rebuild without `--american-stories`, does it. A release without `--american-stories` on such a version builds a full base.
- **CI checks both layouts**: the parity tests load the fixture indexes a second time with the new mapping and run every query, filter and bucket against the reference on both.
