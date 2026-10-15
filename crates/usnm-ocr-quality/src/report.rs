//! The per-page record: what one page's readers look like against each
//! other, with the scan's features. [`analyse`] runs the whole
//! computation; the record's floats are rounded to six decimals so two
//! hosts' bytes compare equal.
use crate::{
    align,
    confusion::{self, Confusions},
    consensus, features,
    findability::{self, Findability, Places},
    hat,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
pub use usnm_ocr_contract::quality::{
    round6, round6_opt, ConfusionPair, HatReport, ImageFeatures, LayoutFeatures, Legibility,
    PageFeatures, PairMetrics, ReaderReport,
};

/// The record's schema version.
pub const VERSION: u32 = 1;

/// The note every estimate carries.
pub const LOW_BOUND_NOTE: &str = "three-cornered hat over pairwise disagreement: correlated reader failures bias the estimate low; calibration on public ground truth measures the gap";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PageReport {
    pub hat: HatReport,
    pub legibility: Legibility,
    pub page: PageFeatures,
    pub readers: Vec<ReaderReport>,
    pub v: u32,
}

/// One reader's text as given to [`analyse`].
#[derive(Debug, Clone, PartialEq)]
pub struct ReaderInput {
    pub name: String,
    pub source: String,
    pub text: String,
}

/// Everything [`analyse`] needs. The image and ALTO are optional so the
/// text part runs without them (calibration crops have neither).
#[derive(Debug, Clone)]
pub struct PageInputs<'a> {
    pub image: Option<&'a image::GrayImage>,
    pub alto: Option<&'a usnm_alto::Page>,
    pub readers: Vec<ReaderInput>,
    pub function_words: BTreeSet<String>,
    pub places: &'a Places,
}

/// The record and, separately, each reader's confusion pairs (written to
/// their own file).
#[derive(Debug, Clone, PartialEq)]
pub struct Analysis {
    pub report: PageReport,
    pub confusions: BTreeMap<String, Vec<ConfusionPair>>,
}

/// The whole computation for one page.
pub fn analyse(inputs: &PageInputs) -> Analysis {
    let n = inputs.readers.len();
    let texts: Vec<&str> = inputs.readers.iter().map(|r| r.text.as_str()).collect();
    let mut pair_table: BTreeMap<(usize, usize), PairMetrics> = BTreeMap::new();
    let mut hat_pairs = Vec::new();
    for i in 0..n {
        for j in i + 1..n {
            let p = align::pair(texts[i], texts[j]);
            if p.chars_a + p.chars_b > 0 {
                hat_pairs.push((i, j, p.cer_sym));
            }
            pair_table.insert((i, j), p);
        }
    }
    let h = hat::solve(n, &hat_pairs);
    let tokens: Vec<Vec<String>> = texts.iter().map(|t| consensus::tokens(t)).collect();
    let cons = consensus::build(&tokens);
    let folded: Vec<Vec<String>> = tokens
        .iter()
        .map(|ts| ts.iter().map(|t| consensus::fold(t)).collect())
        .collect();
    let mut confusions: BTreeMap<String, Vec<ConfusionPair>> = BTreeMap::new();
    let mut per_reader: Vec<Confusions> = (0..n).map(|_| Confusions::default()).collect();
    for (r, reference, reader) in &cons.disagreements {
        per_reader[*r].add(reference, reader);
    }
    let mut readers = Vec::new();
    for (i, r) in inputs.readers.iter().enumerate() {
        let f: Findability = findability::of(
            &cons.sequence,
            &folded[i],
            &inputs.function_words,
            inputs.places,
        );
        let mut pairs = BTreeMap::new();
        for (j, other) in inputs.readers.iter().enumerate() {
            if i == j {
                continue;
            }
            let p = if i < j {
                pair_table[&(i, j)].clone()
            } else {
                pair_table[&(j, i)].flipped()
            };
            pairs.insert(other.name.clone(), round_pair(p));
        }
        let cov = &cons.coverage[i];
        readers.push(ReaderReport {
            chars: usnm_ocr_survey::text::normalize(&r.text, false)
                .chars()
                .count(),
            completeness: round6_opt(cov.completeness),
            content_found: f.content_found,
            content_total: f.content_total,
            est_cer: h.determined.then(|| round6(h.estimates[i])),
            est_cer_low_bound_note: LOW_BOUND_NOTE.to_owned(),
            est_cer_raw: h.determined.then(|| round6(h.raw[i])),
            findable_content: round6_opt(f.content),
            findable_places: round6_opt(f.places),
            name: r.name.clone(),
            pairs,
            places_found: f.places_found,
            places_total: f.places_total,
            precision: round6_opt(cov.precision),
            source: r.source.clone(),
            words: tokens[i].len(),
        });
        confusions.insert(r.name.clone(), per_reader[i].top(confusion::TOP));
    }
    let ceiling = h
        .determined
        .then(|| h.estimates.iter().copied().fold(f64::INFINITY, f64::min))
        .filter(|m| m.is_finite())
        .map(|m| round6((1.0 - m).clamp(0.0, 1.0)));
    Analysis {
        report: PageReport {
            hat: HatReport {
                determined: h.determined,
                n_readers: n,
                pairs: h.pairs,
                residual: h.determined.then(|| round6(h.residual)),
            },
            legibility: Legibility {
                basis: "page-hat".into(),
                ceiling_est: ceiling,
                consensus_words: cons.words.len(),
            },
            page: PageFeatures {
                image: inputs
                    .image
                    .map(|img| round_image(features::image_features(img))),
                layout: inputs
                    .alto
                    .map(|p| round_layout(features::layout_features(p))),
            },
            readers,
            v: VERSION,
        },
        confusions,
    }
}

pub fn round_pair(p: PairMetrics) -> PairMetrics {
    PairMetrics {
        cer_a: round6(p.cer_a),
        cer_b: round6(p.cer_b),
        cer_sym: round6(p.cer_sym),
        dice: round6(p.dice),
        wer_a: round6(p.wer_a),
        wer_b: round6(p.wer_b),
        ..p
    }
}

pub fn round_image(f: ImageFeatures) -> ImageFeatures {
    ImageFeatures {
        bleed_through: round6(f.bleed_through),
        blur: round6(f.blur),
        contrast: round6(f.contrast),
        ink_density: round6(f.ink_density),
        noise: round6(f.noise),
        skew_deg: round6(f.skew_deg),
        ..f
    }
}

pub fn round_layout(f: LayoutFeatures) -> LayoutFeatures {
    LayoutFeatures {
        line_height_px: round6_opt(f.line_height_px),
        mean_word_conf: round6_opt(f.mean_word_conf),
        ..f
    }
}

/// The record's JSON Schema.
pub fn schema() -> schemars::Schema {
    schemars::schema_for!(PageReport)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs<'a>(places: &'a Places, texts: &[(&str, &str)]) -> PageInputs<'a> {
        PageInputs {
            image: None,
            alto: None,
            readers: texts
                .iter()
                .map(|(n, t)| ReaderInput {
                    name: n.to_string(),
                    source: format!("test:{n}"),
                    text: t.to_string(),
                })
                .collect(),
            function_words: ["with", "from"].iter().map(|s| s.to_string()).collect(),
            places,
        }
    }

    #[test]
    fn three_readers_give_a_record() {
        let places = Places::parse("Albany\n");
        let truth = "The mayor spoke at Albany with the council about bridges and roads";
        let a = analyse(&inputs(
            &places,
            &[
                ("loc", truth),
                (
                    "as",
                    "The mayor spoke at Albany with the counci1 about bridges and roads",
                ),
                (
                    "ours",
                    "The rnayor spoke at Albany with the council about bridges",
                ),
            ],
        ));
        let r = &a.report;
        assert_eq!(r.v, VERSION);
        assert!(r.hat.determined);
        assert_eq!(r.readers.len(), 3);
        assert_eq!(r.readers[0].name, "loc");
        assert_eq!(r.readers[0].est_cer, Some(0.0));
        assert!(r.readers[2].est_cer.unwrap() > 0.0);
        assert_eq!(r.readers[0].completeness, Some(1.0));
        assert!(r.readers[2].completeness.unwrap() < 1.0);
        assert_eq!(r.readers[0].findable_places, Some(1.0));
        assert_eq!(r.readers[0].pairs.len(), 2);
        assert_eq!(
            r.readers[0].pairs["as"].cer_b,
            r.readers[1].pairs["loc"].cer_a
        );
        assert!(r.legibility.ceiling_est.unwrap() > 0.9);
        assert_eq!(r.page.image, None);
        assert_eq!(a.confusions["ours"][0].from, "m");
        assert_eq!(a.confusions["ours"][0].to, "rn");
        assert!(a.confusions["ours"].contains(&ConfusionPair {
            count: 1,
            from: "and".into(),
            kind: confusion::Kind::Del,
            to: String::new(),
        }));
        assert!(a.confusions["loc"].is_empty());
        // Rounded: serialising twice through JSON gives the same bytes.
        let once = serde_json::to_string(r).unwrap();
        let back: PageReport = serde_json::from_str(&once).unwrap();
        assert_eq!(serde_json::to_string(&back).unwrap(), once);
    }

    #[test]
    fn two_readers_are_flagged() {
        let places = Places::parse("");
        let a = analyse(&inputs(&places, &[("loc", "a b c"), ("as", "a b d")]));
        assert!(!a.report.hat.determined);
        assert_eq!(a.report.readers[0].est_cer, None);
        assert_eq!(a.report.legibility.ceiling_est, None);
        let empty = analyse(&inputs(&places, &[("loc", ""), ("as", ""), ("ours", "")]));
        assert!(!empty.report.hat.determined);
        assert_eq!(empty.report.readers[0].est_cer, None);
        assert_eq!(empty.report.legibility.ceiling_est, None);
        assert_eq!(empty.report.readers[0].pairs["as"].cer_sym, 0.0);
        assert!(analyse(&inputs(&places, &[])).report.readers.is_empty());
    }

    #[test]
    fn rounding_and_schema() {
        assert_eq!(round6(0.12345678), 0.123457);
        assert_eq!(round6(f64::NAN), 0.0);
        let s = serde_json::to_value(schema()).unwrap();
        assert!(s["properties"]["readers"].is_object());
    }

    #[test]
    fn unconfirmed_tokens_do_not_invent_place_names() {
        let places = Places::parse("New York\n");
        let a = analyse(&inputs(
            &places,
            &[
                ("loc", "New damaged York"),
                ("as", "New smudged York"),
                ("ours", "New unreadable York"),
            ],
        ));
        for r in &a.report.readers {
            assert_eq!(r.places_total, 0);
            assert_eq!(r.places_found, 0);
            assert_eq!(r.findable_places, None);
        }
    }

    #[test]
    fn findability_includes_consensus_words_absent_from_pivot() {
        let places = Places::parse("Albany\nBoston\nAlbany Boston\n");
        let a = analyse(&inputs(
            &places,
            &[
                ("r1", "Albany"),
                ("r2", "Albany"),
                ("r3", "Boston"),
                ("r4", "Boston"),
            ],
        ));
        for r in &a.report.readers {
            assert_eq!(r.content_total, 2);
            assert_eq!(r.content_found, 1);
            assert_eq!(r.findable_content, Some(0.5));
            assert_eq!(r.places_total, 2);
            assert_eq!(r.places_found, 1);
            assert_eq!(r.findable_places, Some(0.5));
        }
    }
}
