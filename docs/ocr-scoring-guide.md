# Scoring an OCR engine against public ground truth

This guide is for people who run their own OCR engine and want its character error rate (CER) and word error rate (WER) on historical newspaper text. You do not transcribe anything. `scripts/ocr-survey/gold.py` downloads public datasets whose text was transcribed and checked by people, cuts them into crops, and scores your engine's text for those crops against the published transcription.

The design, the source policy and the dataset survey are in [ocr-gold-set.md](ocr-gold-set.md). This guide covers the steps. Where it says UNVERIFIED, neither the code nor that document confirms the point.

A faster scorer written in Rust is in progress in [#457](https://github.com/tgoodyear/usnewsmap.com/issues/457). When it lands it replaces `gold.py score`, and the scoring commands in this guide will change. Fetching, converting and the hypothesis format stay as described here.

## Prerequisites

- **A checkout of this repository.** `gold.py` imports `locfetch.py`, `crops.py` and `altotext.py` beside it, and `locfetch.py` imports `ja-ocr/jaocr.py`, so run it from the checkout rather than copying the script out.
- **Python 3.9 or later.** The scripts use only the standard library apart from Pillow, and the newest feature in `gold.py` is `str.removesuffix` (3.9). It was run here on 3.14; 3.9 itself is UNVERIFIED.
- **Pillow**, for `public-convert`, which opens the page images and cuts the crops (`pip install Pillow`). `validate` and `score` don't need it.
- **bsdtar or 7-Zip**, for BLN600 only. Its zip is AES-encrypted and Python's `zipfile` can't open it, so `public-fetch` calls `bsdtar` (libarchive) or `7z`/`7zz`, and stops with an error if neither is on the `PATH`.
- **Network access** to Zenodo, figshare, GitHub, the UB Mannheim image server, Hugging Face and loc.gov, depending on the dataset.
- **Disk.** Sizes of the downloads, from [ocr-gold-set.md](ocr-gold-set.md); the extracted files need about as much again:

  | Dataset                         | Download                                                                                                                   |
  | ------------------------------- | -------------------------------------------------------------------------------------------------------------------------- |
  | `bln600`                        | 68.1 MB zip                                                                                                                |
  | `overproof-ca`                  | HIPE text files, then one ALTO file and one full-size page image per LoC page (`--pages`, 20 by default); total UNVERIFIED |
  | `reichsanzeiger`                | 26.1 MB zip, then `--pages` page JPEGs (20 by default, 101 at most) from Mannheim; image size UNVERIFIED                   |
  | `austrian`                      | 183 MB (both zips: this dataset defaults to `--subset all`)                                                                |
  | `newseye-bnf`, `-nlf-fi`, `-sv` | 16 to 122 MB for the validation zip (the default); the Finnish training zip is 5.4 GB                                      |
  | `nzz`                           | 472.6 MB zip                                                                                                               |

- **LoC pacing, for `overproof-ca`.** The page images and ALTO come from loc.gov at run time through `locfetch.py`, which waits 3.5 seconds between loc.gov API requests and 1 second between image-server requests. A 429 waits for its `Retry-After` (or a minute, longer on each retry), and a 403 stops the run, because it is usually a challenge page and the script doesn't work around one. Don't lower the gaps.

## Choosing datasets

Pick the sets that match the language and script your engine is meant for. Languages are the `lang` codes `gold.py` writes; eras and licences are from [ocr-gold-set.md](ocr-gold-set.md).

| `--dataset`       | Language, script                        | Era                                     | Licence                                                                                                | Use             |
| ----------------- | --------------------------------------- | --------------------------------------- | ------------------------------------------------------------------------------------------------------ | --------------- |
| `bln600`          | English (`eng`), roman                  | 1800 to 1900, London newspapers         | CC BY-NC-ND 4.0                                                                                        | Scoring         |
| `overproof-ca`    | English (`eng`), roman                  | 1871 to 1922, 5 US titles               | CC BY-NC-SA 4.0 (HIPE copy); the newspaper text and LoC's images are public domain                     | Scoring         |
| `reichsanzeiger`  | German (`deu`), Fraktur and roman       | 1820 to 1939, a government gazette      | CC0-1.0 (GitHub) and CC-BY-4.0 (Zenodo); images Public Domain Mark 1.0. Follow CC-BY-4.0               | Scoring         |
| `austrian`        | German (`deu`), Fraktur                 | 1864 to 1911                            | CC-BY-4.0                                                                                              | Scoring         |
| `nzz`             | German (`deu`), Fraktur, line level     | 1780 to 1947, Swiss daily               | CC BY-NC 4.0 (the archive's LICENSE.txt; the Zenodo record says CC-BY-4.0, and the stricter one rules) | Scoring         |
| `newseye-bnf`     | French (`fra`)                          | UNVERIFIED                              | CC-BY-4.0                                                                                              | Scoring         |
| `newseye-nlf-fi`  | Finnish (`fin`)                         | UNVERIFIED                              | CC-BY-4.0                                                                                              | Scoring         |
| `newseye-nlf-sv`  | Swedish (`swe`)                         | UNVERIFIED                              | CC-BY-4.0                                                                                              | Scoring         |
| `overproof-smh`   | English (`eng`), text only, no images   | Sydney Morning Herald; years UNVERIFIED | CC BY-NC-SA 4.0 (HIPE copy); Trove image licence UNVERIFIED                                            | Comparison only |
| `enp-berlin`      | German (`deu`), Fraktur                 | UNVERIFIED                              | CC0-1.0                                                                                                | Comparison only |
| `americanstories` | English (`eng`)                         | 19th and 20th century; years UNVERIFIED | Apache-2.0 (Hugging Face metadata only)                                                                | Comparison only |
| `ndl-pdm`         | Japanese (`jpn`), books, not newspapers | 1870 to the 1940s                       | Public Domain Mark 1.0                                                                                 | Comparison only |

"Comparison only" sets fail the [source policy](ocr-gold-set.md#source-policy): their text isn't clean enough or its provenance isn't documented. `public-convert` refuses them unless you pass `--not-for-scoring`, and their rows then carry `for_scoring: false`. Don't quote their numbers as accuracy. There is no Japanese scoring set yet.

## Fetch and convert

`public-fetch` downloads a dataset into `--src` and won't run without `--confirm-download`. It writes `SOURCE.json` (licence, attribution and URLs) beside the download. A file already downloaded is not fetched again. `public-convert` reads `--src` and writes the scoring layout to `--out`.

The commands below assume the repository root as the working directory, with `DATA` and `OUT` standing for directories of your choice. The counts are the ones used in the real runs of 13 October 2026, listed in [ocr-gold-set.md](ocr-gold-set.md#fetch-and-convert); `--count` defaults to 60 and `--per-page` to 3.

```sh
# BLN600: 600 excerpts; the zip's sha256 is checked before extracting
scripts/ocr-survey/gold.py public-fetch --dataset bln600 --src DATA/bln600 --confirm-download
scripts/ocr-survey/gold.py public-convert --dataset bln600 --src DATA/bln600 --out OUT/bln600 --count 600 --per-page 1

# Overproof dataset 3 (Chronicling America): --pages LoC pages, fetched with LoC pacing
scripts/ocr-survey/gold.py public-fetch --dataset overproof-ca --src DATA/overproof-ca --confirm-download --pages 20
scripts/ocr-survey/gold.py public-convert --dataset overproof-ca --src DATA/overproof-ca --out OUT/overproof-ca --count 600 --per-page 50

# Reichsanzeiger-GT: --pages page images from Mannheim, one second apart
scripts/ocr-survey/gold.py public-fetch --dataset reichsanzeiger --src DATA/reichsanzeiger --confirm-download --pages 20
scripts/ocr-survey/gold.py public-convert --dataset reichsanzeiger --src DATA/reichsanzeiger --out OUT/reichsanzeiger --count 60

# Austrian newspapers (both zips by default)
scripts/ocr-survey/gold.py public-fetch --dataset austrian --src DATA/austrian --confirm-download
scripts/ocr-survey/gold.py public-convert --dataset austrian --src DATA/austrian --out OUT/austrian --count 60

# NZZ black-letter, line level
scripts/ocr-survey/gold.py public-fetch --dataset nzz --src DATA/nzz --confirm-download
scripts/ocr-survey/gold.py public-convert --dataset nzz --src DATA/nzz --out OUT/nzz --count 60

# NewsEye French, Finnish, Swedish: --subset validation (default), training or all
scripts/ocr-survey/gold.py public-fetch --dataset newseye-bnf --src DATA/newseye-bnf --confirm-download --subset validation
scripts/ocr-survey/gold.py public-convert --dataset newseye-bnf --src DATA/newseye-bnf --out OUT/newseye-bnf --count 60
# newseye-nlf-fi and newseye-nlf-sv take the same two commands
```

Other `public-convert` flags: `--seed` (default 296) changes which regions are picked; `--min-chars` and `--max-chars` override the length window (200 to 2,500 normalized characters for most sets, 200 to 8,000 for BLN600, 20 to 300 for NZZ lines). The pick takes one region from each page before a second from any page, up to `--per-page` per page. With the same download, seed and flags, the pick is the same, so two people can score on the same crops.

Not yet run on the real files: the `reichsanzeiger` and `austrian` conversions (see [What was tested](ocr-gold-set.md#fetch-and-convert)). If one of them fails or writes no crops, open an issue with the error.

## Running your engine

`public-convert` writes this to `--out`:

```text
OUT/bln600/
  crops.jsonl     one row per crop, with the published transcription
  manifest.jsonl  one row per crop: source page, box, checksums, source URL and licence
  images/         one image per crop, named <crop_id>.jpg (BLN600: the dataset's own file, .jpg or .tif)
  LICENSES/       SOURCE.json and the dataset's own licence and readme files
  match.json      overproof-ca only: how many lines matched today's LoC ALTO
```

Each `crops.jsonl` row has `crop_id` (such as `pub-bln600-001`), `group` (`pub-bln600`), `lang`, `image` (the path relative to `--out`, `images/pub-bln600-001.jpg`), `transcription`, `truth_source` and `for_scoring`. The source fields (`source_dataset`, `source_url`, `source_license`, `source_attribution`) are in `manifest.jsonl`, keyed by the same `crop_id`. Crop images are grayscale JPEG at quality 85, except BLN600's, which are the dataset's own files byte for byte. Overproof dataset 2 rows have `image: null` and nothing for an engine to read.

Run your engine on each `image` and write one JSON object per line:

```json
{"crop_id": "pub-bln600-001", "text": "The text your engine read,\nwith its line breaks.\n"}
```

Both keys are required. A crop with no row in your file is left out of your engine's score, and the `crops` column then shows fewer crops than the set has (see [Comparing engines](#comparing-engines)). Rows whose `crop_id` isn't in `crops.jsonl` are ignored.

What to normalize, and what to leave alone. `score` applies the same normalization to your text and to the transcription: Unicode NFC, a hyphen at the end of a line joined to the next line, and every run of whitespace and line breaks turned into one space. So:

- **Send the raw reading order and line breaks.** Don't join hyphenated words yourself and don't reflow lines; the scorer does both.
- **Don't fold anything else.** Case, punctuation, diacritics, long s (ſ) and ligatures all count. Don't lowercase, strip punctuation, transliterate or spell-correct, or you'll be charged for each change.
- **Don't add anything.** No confidence scores, markup, page headers or `[?]`-style markers in `text`; they count as characters. The `[?]` and `[?x]` markers are read only in transcriptions.
- **Use UTF-8.** Write the file in UTF-8; `ensure_ascii=False` in Python keeps it readable, and escaped `\u` sequences decode to the same text.

A minimal example with Tesseract. It reads `crops.jsonl`, runs `tesseract` on each image with the language you name, and writes the hypothesis file. Save it anywhere outside the repository, for example as `run_tesseract.py`:

```python
#!/usr/bin/env python3
"""Runs Tesseract over a converted gold-set directory and writes a hypothesis file for gold.py score.

    run_tesseract.py --crops OUT/bln600/crops.jsonl --lang eng --out tess.jsonl
"""

import argparse
import json
import subprocess
import sys
from pathlib import Path


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--crops", type=Path, required=True, help="crops.jsonl written by gold.py public-convert")
    ap.add_argument("--lang", default="eng", help="Tesseract -l value, e.g. eng, deu, frk, fra")
    ap.add_argument("--psm", default="6", help="Tesseract page segmentation mode; 6 is one block of text")
    ap.add_argument("--out", type=Path, required=True, help="hypothesis JSONL to write")
    a = ap.parse_args()

    base = a.crops.parent
    done = 0
    with a.out.open("w", encoding="utf-8") as out:
        for line in a.crops.read_text(encoding="utf-8").splitlines():
            if not line.strip():
                continue
            row = json.loads(line)
            if not row.get("image"):  # text-only rows have no image to read
                continue
            image = base / row["image"]
            r = subprocess.run(["tesseract", str(image), "stdout", "-l", a.lang, "--psm", a.psm],
                               capture_output=True, text=True, encoding="utf-8")
            if r.returncode != 0:
                print(f"tesseract failed on {row['crop_id']}: {r.stderr.strip()}", file=sys.stderr)
                continue
            # The raw text, line breaks and all: gold.py score normalizes both sides itself.
            out.write(json.dumps({"crop_id": row["crop_id"], "text": r.stdout}, ensure_ascii=False) + "\n")
            done += 1
    print(f"{done} crops read; wrote {a.out}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
```

```sh
python3 run_tesseract.py --crops OUT/bln600/crops.jsonl --lang eng --out OUT/bln600/tesseract.jsonl
```

Tested with Tesseract 5.5.1 on a synthetic set of two rendered English crops in the `public-convert` layout: it wrote both rows, and `validate` and `score` ran on them. It has not been run on a downloaded dataset. The `--lang` value has to be a language pack your Tesseract has installed (`tesseract --list-langs`). Page segmentation mode 6 suits a block of lines; a single NZZ line may read better with `--psm 7`, which is UNVERIFIED.

## Validate and score

```sh
scripts/ocr-survey/gold.py validate --crops OUT/bln600/crops.jsonl --manifest OUT/bln600/manifest.jsonl --images OUT/bln600/images
scripts/ocr-survey/gold.py score --crops OUT/bln600/crops.jsonl --hyp tesseract-5.5=OUT/bln600/tesseract.jsonl --out OUT/bln600/scores
```

`validate` checks the transcriptions and that each image is present. It prints each error and warning, then a count, and exits with status 1 on any error. `crop_sha256` in the manifest is the checksum of the bytes `public-convert` wrote, so a warning that an image differs from it means the file changed after conversion; convert again. The real runs listed in [ocr-gold-set.md](ocr-gold-set.md#fetch-and-convert) validated with 0 errors.

`score` takes one `--hyp NAME=FILE.jsonl` per engine; repeat the flag to score several engines in one run. `NAME` is your label for the engine and its settings, and it's what the output shows, so put the version and profile in it (`tesseract-5.5-psm6`). `score` leaves out crops that fail validation and prints how many it used on stderr. It prints the summary as a Markdown table and, with `--out`, writes two CSV files.

`cer-summary.csv`, one row per engine and group, plus a row with group `all`:

| Column      | Meaning                                                                                                 |
| ----------- | ------------------------------------------------------------------------------------------------------- |
| `engine`    | The `NAME` from `--hyp`                                                                                 |
| `group`     | The dataset's group (`pub-bln600`), or `all` for every crop scored in this run                          |
| `crops`     | Crops that had both a usable transcription and a row in your file                                       |
| `ref_chars` | Characters of the normalized transcriptions of those crops                                              |
| `cer`       | Character edits (insertions, deletions, substitutions) summed over the crops, divided by `ref_chars`    |
| `wer`       | The same over whitespace-separated words: word edits summed, divided by the words in the transcriptions |

`cer-by-crop.csv`, one row per engine and crop:

| Column       | Meaning                                                     |
| ------------ | ----------------------------------------------------------- |
| `engine`     | The `NAME` from `--hyp`                                     |
| `crop_id`    | The crop                                                    |
| `group`      | The dataset's group                                         |
| `ref_chars`  | Characters in the normalized transcription                  |
| `char_edits` | Character edit distance between your normalized text and it |
| `ref_words`  | Words in the normalized transcription                       |
| `word_edits` | Word edit distance                                          |

CER and WER are rounded to 4 decimal places. Because edits and lengths are summed before dividing, a long crop weighs more than a short one. Either can exceed 1 when your text is much longer than the transcription.

The edit distance is pure Python. BLN600 crops run up to 7,000 characters, so scoring all 600 takes a long time; the Rust scorer from #457 is meant to fix that.

## Reading results

**Per group.** Each dataset is its own group, so read CER and WER per group. The `all` row pools whatever sets you passed in one `--crops` file; since each `public-convert` output is scored with its own `crops.jsonl`, `all` normally equals the single group. Don't average groups from different runs into one number: the sets differ in language, era, layout and transcription convention.

**Transcription conventions.** The scorer folds nothing but the line-end `-`, so an engine is charged wherever its output convention differs from the dataset's. Read each number as "against this dataset's transcription convention":

- Reichsanzeiger and Austrian text marks a word broken across lines with the double oblique hyphen `⸗`, which the scorer does not join, and keeps long s and other old forms. An engine that prints `-` or `s` is charged for each.
- NZZ marks a line-end hyphen with `¬` and transcribes long s as `s`. Its guidelines also turn "sz" into ß and spell I/J by modern rules, so an engine that prints `ſ` is charged on NZZ and one that prints `s` is charged on Reichsanzeiger.
- Overproof's `¬` is turned into `-` on conversion, so it joins as a normal line-end hyphen.
- The NDL text (comparison only) is transcribed as seen, old character forms included, and has no spaces, so WER means nothing for `jpn`. Use CER.
- A Reichsanzeiger region can be mostly figures and tables; the length window doesn't filter those out.

**Comparing engines.** Compare engines with each other on the same crops: the same dataset, the same `public-convert` flags and seed, and the same `crops.jsonl`. Check that `crops` and `ref_chars` match between the engines' rows. If one engine has fewer crops, it skipped some or your file lacks rows, and its CER covers different text. The easiest way to keep them comparable is one `score` run with a `--hyp` per engine.

**Accuracy and agreement are different numbers.** `gold.py agree` compares engines with each other on this project's own 60 crops, which have no transcription. That number is agreement, not accuracy: two engines can agree and both be wrong. The public sets give accuracy against a human transcription. Report them in separate tables with those labels, and never put a public-set CER and an agreement figure in the same column. The `pub-` prefix on every public group keeps the rows apart.

**Sample size.** The NewsEye validation sets gave 24 (French), 18 (Finnish) and 10 (Swedish) crops in the real runs. Small sets swing with a few crops; quote `crops` and `ref_chars` with every CER.

## Licences and attribution

Each output directory has a `LICENSES/` folder with `SOURCE.json` (dataset, URL, licence, attribution, and for BLN600 a note on the No-Derivatives terms) and the dataset's own licence and readme files where it has them. Every row in `manifest.jsonl` carries `source_dataset`, `source_url`, `source_license` and `source_attribution`. When you publish results, cite each dataset you scored on from those fields, and say which subset, `--count`, `--per-page` and `--seed` you used so others can rebuild the same crops.

- **Attribution (BY).** Every scoring set except Reichsanzeiger's GitHub copy carries a BY term. Credit the source named in `SOURCE.json`. Its `attribution` (and the manifest's `source_attribution`) is null for Reichsanzeiger and Austrian, because `gold.py` has no attribution text for them yet, so use the credits that follow. From [ocr-gold-set.md](ocr-gold-set.md#source-policy): UB Mannheim for Reichsanzeiger-GT; ONB, NewsEye and READ for the Austrian set; Callum Booth, Alan Thomas and Robert Gaizauskas for BLN600.
- **NonCommercial (NC): BLN600, NZZ, Overproof.** Fine for research, which is how [ocr-gold-set.md](ocr-gold-set.md#source-policy) uses them. Commercial use of the data needs the rights holder's terms, which this guide doesn't cover.
- **ShareAlike (SA): Overproof (HIPE copy).** If you redistribute the converted text or an adaptation of it, it goes out under CC BY-NC-SA 4.0. The HIPE licence applies to the structured format; the newspaper text and LoC's images are public domain.
- **NoDerivatives (ND): BLN600.** Don't share crops, re-encoded images or other derived files of BLN600. `public-convert` copies BLN600's images unchanged for that reason. To let someone reproduce your run, give them the dataset's URL and your flags, and let them fetch it themselves.

Scores, tables and the per-crop CSVs hold numbers and crop IDs, not the dataset's text or images, so this project reads them as publishable with the attribution above (an inference, not legal advice). Your hypothesis files contain your engine's text of the crops; whether publishing those counts as a derivative under ND or SA terms is UNVERIFIED, so keep BLN600 hypotheses private to be safe.

Nothing from these datasets is committed to this repository. Keep `DATA` and `OUT` outside it, or out of version control.
