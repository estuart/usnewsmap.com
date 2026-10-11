# Word lists in usnm-core

Data files compiled into `usnm-core`. The crate's code is MIT; these files keep the licences of the data they come from, and `Cargo.toml` names both.

| Files                                                    | Used by                                              | Built by                                           | Source                                   | Licence      |
| -------------------------------------------------------- | ---------------------------------------------------- | -------------------------------------------------- | ---------------------------------------- | ------------ |
| `english-words.txt`, `english-shared-words.txt`          | `usnm_core::query_language` (06 §6.3.3, #237)        | `scripts/build-english-words.py`                   | wordfreq 3.1.1                           | CC BY-SA 4.0 |
| `damage-words/<code>.txt`, except `haw.txt` and `yi.txt` | `usnm_core::lang`, feature `damage` (12 §12.5, #296) | `scripts/build-damage-words.py`                    | wordfreq 3.1.1, `top_n_list(code, 5000)` | CC BY-SA 4.0 |
| `damage-words/haw.txt`                                   | the same                                             | copied from `ja-ocr/wordlists/` by the same script | Hawaiian Corpus Project frequency list   | CC0          |
| `damage-words/yi.txt`                                    | the same                                             | copied from `ja-ocr/wordlists/` by the same script | Yiddish Wikipedia and Wikisource         | CC BY-SA 4.0 |

`ja-ocr/wordlists/README.md` describes how the Hawaiian and Yiddish lists were built.

## The damage word lists

One file per word list code in `usnm_core::lang` (wordfreq's codes: `sh` serves Croatian and Serbian, `nb` Norwegian, `fil` Tagalog). Each holds the language's 5,000 most frequent words, most frequent first, one per line, exactly as wordfreq 3.1.1's `top_n_list` returns them, the call `ja-ocr/quality.py` makes. Order matters: the metric takes its function words and head words from the front of the list.

Rebuild or check them with wordfreq 3.1.1 installed (`pip install wordfreq==3.1.1`):

```sh
scripts/build-damage-words.py --check
scripts/build-damage-words.py --out-dir crates/usnm-core/data/damage-words
```

Nothing in the pipeline runs the script; the files are committed data.

## wordfreq

[wordfreq](https://github.com/rspeer/wordfreq) 3.1.1, by Robyn Speer. Its code is Apache-2.0 and its data CC BY-SA 4.0 (<https://creativecommons.org/licenses/by-sa/4.0/>), so lists exported from it are shared under CC BY-SA 4.0. Its data draws on:

- Google Books Ngrams (<http://books.google.com/ngrams>) and Google Books Syntactic Ngrams, which may be used for any purpose with acknowledgement of the Google Books Ngram Viewer;
- the Leeds Internet Corpus (University of Leeds Centre for Translation Studies), Wikipedia and ParaCrawl, under Creative Commons licences;
- OPUS OpenSubtitles 2018, from the OpenSubtitles project, used with attribution to OpenSubtitles;
- the SUBTLEX lists (SUBTLEX-US, -UK, -CH, -DE and -NL) by Marc Brysbaert et al., freely available data that must be credited to the SUBTLEX authors;
- word counts from the Twitter streaming API, and other web and news text.

Cite wordfreq as: Robyn Speer (2022). rspeer/wordfreq: v3.0 (v3.0.2). Zenodo. <https://doi.org/10.5281/zenodo.7199437>.
