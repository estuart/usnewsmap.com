//! How the main index lays out a page's texts for searching (05 §5.5.7,
//! #283): a field for each text, or one field for both.
//!
//! [`TextLayout::Separate`] is the layout of every version before #283:
//! LoC's text in `text` and its common-word pairs in `text_cg`, and, with
//! American Stories' text (05 §5.5.4), `text_as` and `text_as_cg`. A query
//! looks each word up in each text's field.
//!
//! [`TextLayout::Single`] searches one field, [`FIELD`], and one pairs
//! field, [`PAIRS_FIELD`]: LoC's words, then [`GAP_POSITIONS`] positions that no query
//! can match, then American Stories' words. `text` and `text_as` are kept,
//! stored but not indexed, for snippets and `matched_in`. A query then looks
//! each word up once. The release writes both fields' tokens
//! ([`index_fields`]), one per word as the version's analyzer has it, and
//! the fields' tokenizer is `whitespace`, as for the pairs (05 §5.5.3).
//!
//! The layout is part of the index: `current.json` names it
//! (`text_layout`, absent for [`TextLayout::Separate`]), every main index
//! of a version has the same one, and only a full rebuild changes it. The
//! API searches each version as its layout says.

use crate::common_grams;
use crate::text::Analyzer;

/// The text fields of a version's main indexes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum TextLayout {
    /// A searched field per text: `text` and `text_cg`, `text_as` and
    /// `text_as_cg`.
    #[default]
    Separate,
    /// One searched field for both texts, [`FIELD`] and [`PAIRS_FIELD`].
    Single,
}

impl TextLayout {
    /// The layout a full rebuild with `--single-text-field` builds.
    pub const LATEST: Self = Self::Single;

    /// The number `current.json` records as `text_layout`.
    pub fn version(self) -> u32 {
        match self {
            Self::Separate => 1,
            Self::Single => 2,
        }
    }

    /// The layout `current.json`'s `text_layout` names: [`Self::Separate`]
    /// when it has none, `None` for a number this code doesn't know (a
    /// newer one).
    pub fn from_version(version: Option<u32>) -> Option<Self> {
        match version {
            None | Some(1) => Some(Self::Separate),
            Some(2) => Some(Self::Single),
            Some(_) => None,
        }
    }
}

/// The one searched field of [`TextLayout::Single`].
pub const FIELD: &str = "text_all";

/// Its common-word pairs (05 §5.5.3).
pub const PAIRS_FIELD: &str = "text_all_cg";

/// Positions between LoC's words and American Stories' in [`FIELD`] and
/// [`PAIRS_FIELD`], each a [`common_grams::GAP`], so no phrase or NEAR
/// search matches across the two texts. Quickwit 0.9.1 matched no phrase of
/// up to 12 words with slop 20 across 32, in order or reversed (it counts a
/// swap against the slop, #243); a two-word phrase needs slop 32.
pub const GAP_POSITIONS: usize = 32;

const _: () = assert!(GAP_POSITIONS > crate::query::MAX_SLOP as usize + 1);

/// [`FIELD`] and [`PAIRS_FIELD`] for a page with LoC's `text` (empty when
/// it has none) and American Stories' `text_as`, folded by `analyzer`: each
/// text's words ([`common_grams::index_words`]) and pairs
/// ([`common_grams::index_text`], made per text, so the last word of one is
/// never paired with the first of the other), with [`GAP_POSITIONS`] between them
/// when both have words.
pub fn index_fields(text: &str, text_as: Option<&str>, analyzer: Analyzer) -> (String, String) {
    let mut words = Vec::new();
    let mut pairs = Vec::new();
    for t in std::iter::once(text).chain(text_as) {
        let w = common_grams::index_words(t, analyzer);
        if !w.is_empty() {
            words.push(w);
            pairs.push(common_grams::index_text(t, analyzer));
        }
    }
    let gap = format!(" {} ", vec![common_grams::GAP; GAP_POSITIONS].join(" "));
    (words.join(&gap), pairs.join(&gap))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_round_trip_and_unknown_ones_are_refused() {
        for l in [TextLayout::Separate, TextLayout::Single] {
            assert_eq!(TextLayout::from_version(Some(l.version())), Some(l));
        }
        assert_eq!(TextLayout::from_version(None), Some(TextLayout::Separate));
        assert_eq!(TextLayout::from_version(Some(3)), None);
        assert_eq!(TextLayout::from_version(Some(0)), None);
        assert_eq!(TextLayout::default(), TextLayout::Separate);
    }

    #[test]
    fn both_texts_with_a_gap_between_them() {
        let a = Analyzer::LATEST;
        let (words, pairs) = index_fields("A cross of Gold, the", Some("BRYAN: of the people"), a);
        let gap = vec!["_"; GAP_POSITIONS].join(" ");
        assert_eq!(
            words,
            format!("a cross of gold the {gap} bryan of the people")
        );
        // The last word of LoC's text pairs with nothing from the other.
        assert_eq!(
            pairs,
            format!("a_cross cross of_gold gold the_ {gap} bryan of_the the_people people")
        );
        // Same positions in both fields.
        assert_eq!(words.split(' ').count(), pairs.split(' ').count());
    }

    #[test]
    fn one_text_has_no_gap() {
        let a = Analyzer::LATEST;
        assert_eq!(
            index_fields("Cross of gold", None, a),
            ("cross of gold".into(), "cross of_gold gold".into())
        );
        // A page only American Stories has text for (LoC's empty or short).
        assert_eq!(
            index_fields("", Some("Cross of gold"), a),
            ("cross of gold".into(), "cross of_gold gold".into())
        );
        assert_eq!(
            index_fields(" -- ", Some("gold"), a),
            ("gold".into(), "gold".into())
        );
        assert_eq!(index_fields("", None, a), (String::new(), String::new()));
    }

    #[test]
    fn dropped_words_keep_their_positions() {
        let long = "x".repeat(41);
        let (words, pairs) = index_fields(&format!("gold {long} of"), None, Analyzer::LATEST);
        assert_eq!(words, "gold _ of");
        assert_eq!(pairs, "gold _ of_");
    }
}
