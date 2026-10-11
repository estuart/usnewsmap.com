//! Text normalization shared by ingest (04 §4.5) and the query parser, so the
//! index and queries agree. The `usnm_text` analyzer (05 §5.5) splits words
//! at what isn't a letter or digit, then lowercases and ASCII-folds them.
//!
//! Two rules drop over-long OCR runs. Quickwit 0.9.1's `remove_long` filter,
//! on `text` and `text_as` (layout 1, 05 §5.5.1), drops a word of
//! [`REMOVE_LONG_BYTES`] or more before folding and keeps its position
//! ([`index_tokens`], #286), so words of 41 to 254 bytes are indexed there.
//! Our own code drops words over [`MAX_TOKEN_CHARS`] after folding: queries
//! ([`tokenize`]), the common-word pairs (`text_cg`), the one searched field
//! of layout 2 (`text_all`, 05 §5.5.7) and Japanese pages. Snippets read a
//! page's words as the layout's searched field holds them.

use serde::{Deserialize, Serialize};
use unicode_normalization::char::is_combining_mark;

use crate::ja::is_ja;
use unicode_normalization::UnicodeNormalization;

/// Folded words longer than this are OCR garbage: the query parser takes
/// none, and the fields our code writes (`text_cg`, `text_all`, Japanese
/// pages) drop them. Quickwit's `usnm_text` keeps them in `text` and
/// `text_as` up to [`REMOVE_LONG_BYTES`].
pub const MAX_TOKEN_CHARS: usize = 40;

/// Quickwit 0.9.1's `remove_long` filter in `usnm_text` drops a token of this
/// many bytes or more, measured as split, before lowercasing and ASCII
/// folding (tantivy's default length, which Quickwit's tokenizer config
/// can't change). The dropped word keeps its position (#286).
pub const REMOVE_LONG_BYTES: usize = 255;

/// Pages whose normalized text is shorter than this are `short`.
pub const MIN_PAGE_CHARS: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TextStatus {
    Ok,
    Short,
    Empty,
}

/// Normalize raw OCR text for storage (04 §4.5). Case is preserved.
pub fn normalize_ocr(raw: &str) -> String {
    let nfc: String = raw
        .nfc()
        .filter_map(|c| match c {
            'ſ' => Some('s'),
            '\n' | '\t' | ' ' => Some(c),
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect();
    let expanded = expand_ligatures(&nfc);
    let joined = rejoin_hyphenation(&expanded);
    collapse_whitespace(&joined)
}

pub fn text_status(normalized: &str) -> TextStatus {
    let n = normalized.chars().filter(|c| !c.is_whitespace()).count();
    if n == 0 {
        TextStatus::Empty
    } else if normalized.trim().chars().count() < MIN_PAGE_CHARS {
        TextStatus::Short
    } else {
        TextStatus::Ok
    }
}

fn expand_ligatures(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            'ﬀ' => out.push_str("ff"),
            'ﬁ' => out.push_str("fi"),
            'ﬂ' => out.push_str("fl"),
            'ﬃ' => out.push_str("ffi"),
            'ﬄ' => out.push_str("ffl"),
            'ﬅ' | 'ﬆ' => out.push_str("st"),
            c => out.push(c),
        }
    }
    out
}

/// `indus-\ntry` → `industry`, only when both sides are alphabetic.
fn rejoin_hyphenation(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '-' && i > 0 && chars[i - 1].is_alphabetic() {
            let mut j = i + 1;
            while j < chars.len() && (chars[j] == ' ' || chars[j] == '\t') {
                j += 1;
            }
            if j < chars.len() && chars[j] == '\n' {
                let mut k = j + 1;
                while k < chars.len() && chars[k].is_whitespace() {
                    k += 1;
                }
                if k < chars.len() && chars[k].is_alphabetic() {
                    i = k;
                    continue;
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Collapse runs of spaces/tabs; keep single line breaks as paragraph markers.
fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending_space = false;
    let mut pending_newline = false;
    for c in s.chars() {
        if c == '\n' {
            pending_newline = true;
            pending_space = false;
        } else if c.is_whitespace() {
            if !pending_newline {
                pending_space = true;
            }
        } else {
            if !out.is_empty() {
                if pending_newline {
                    out.push('\n');
                } else if pending_space {
                    out.push(' ');
                }
            }
            pending_space = false;
            pending_newline = false;
            out.push(c);
        }
    }
    out
}

/// A version of the text analysis this code does on its side of the index:
/// the folding of words ([`fold`], [`tokenize`]), which the query parser, the
/// memory backend, snippets, the common-word pairs (`text_cg`, 05 §5.5.3) and
/// the Japanese pages' index (`crate::ja`) all use. An index is built with
/// one, and a published version records it (`current.json`'s
/// `common_grams` and `ja.fold`, [`crate::common_grams::analyzer`] and
/// [`crate::ja::analyzer`]); the API parses queries for it with the same one,
/// so a version keeps searching the way it was built until a full rebuild
/// switches it to [`Analyzer::LATEST`] (05 §5.5.3).
///
/// Quickwit's `usnm_text` analyzer on `text` is the same in every version:
/// only our side changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Analyzer {
    /// Every character folds through its compatibility decomposition, so
    /// `½` is `1⁄2` (common-word pairs 1, Japanese fold 1).
    V1,
    /// A character whose decomposition holds a separator stays itself
    /// (`½`, `61¼`), as the `usnm_text` analyzer has it, and `Ŀ` folds to
    /// `l` (#168); so does one that isn't Japanese but decomposes into a
    /// Japanese character (`㊀`, #241). Common-word pairs 2, Japanese fold 2.
    V2,
}

impl Analyzer {
    /// What a full rebuild builds with.
    pub const LATEST: Self = Self::V2;
    /// Every version this code can build and search, oldest first.
    pub const ALL: [Self; 2] = [Self::V1, Self::V2];
}

/// The version whose folding is what Quickwit's `usnm_text` analyzer makes
/// of a text, in every index version: pages' `text` and `text_as`
/// ([`index_tokens`]), and the query terms Quickwit analyzes again (#168).
pub const USNM_TEXT: Analyzer = Analyzer::V2;

impl Default for Analyzer {
    /// [`Analyzer::LATEST`].
    fn default() -> Self {
        Self::LATEST
    }
}

/// The analyzers a published version's indexes were built with: its main
/// indexes' (`current.json`'s `common_grams`) and its Japanese pages'
/// (`ja.fold`). A release builds both with one, so they differ only in a
/// version that was put together otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Analyzers {
    pub main: Analyzer,
    pub ja: Analyzer,
}

impl Analyzers {
    /// Both indexes built with `analyzer`.
    pub fn all(analyzer: Analyzer) -> Self {
        Self {
            main: analyzer,
            ja: analyzer,
        }
    }
}

/// Lowercase + ASCII-fold one token (`Ñoño` → `nono`, `ſ` → `s`, `Æsop` → `aesop`).
///
/// Decomposable letters lose their diacritics via NFKD; letters with no
/// decomposition (`æ œ ø ł ß đ ð þ ı`) are transliterated the way Lucene's
/// ASCII folding filter does.
///
/// From [`Analyzer::V2`], a character whose decomposition holds a separator
/// or a Japanese character stays itself, lowercased (see [`keeps_form`]):
/// `½` is the token `½`, not `1⁄2`, and `a㊀` is `a㊀`, not `a一`. So an
/// alphanumeric token folds to an alphanumeric token, which tokenizes as
/// itself, here and on a Japanese page (#168, #241). [`Analyzer::V1`]
/// decomposes it.
pub fn fold(token: &str, analyzer: Analyzer) -> String {
    let mut out = String::with_capacity(token.len());
    match analyzer {
        Analyzer::V1 => push_folded(token.nfkd(), &mut out),
        Analyzer::V2 => {
            for c in token.chars() {
                if c.is_ascii() {
                    out.push(c.to_ascii_lowercase());
                } else if matches!(c, 'Ŀ' | 'ŀ') {
                    // `L·`: ASCII folding drops the middle dot.
                    out.push('l');
                } else if keeps_form(c) {
                    out.extend(c.to_lowercase());
                } else {
                    push_folded(c.nfkd(), &mut out);
                }
            }
        }
    }
    out
}

/// Whether `c` folds as itself rather than through its compatibility
/// decomposition, from [`Analyzer::V2`]: an alphanumeric character that
/// decomposes into a separator as well, such as the vulgar fractions (`½` is
/// `1⁄2`, with U+2044 FRACTION SLASH), `⒈` (`1.`), `⑴` (`(1)`) or `ﷺ` (four
/// Arabic words). Decomposed, such a character would be several words where
/// the `usnm_text` analyzer, which splits text before it folds, has one token
/// (`½` stays `½`): a query for it would miss the pages that have it, and its
/// canonical form would reparse as a phrase (#168). Kept, the query term goes
/// through the same analyzer as the page.
///
/// The same goes for a character that isn't Japanese but decomposes into a
/// Japanese one: the Hangzhou numerals `〸 〹 〺`, the Kanbun marks `㆒`–`㆕`
/// and the circled ideographs `㊀`–`㊉`. `usnm_text` keeps them (`a㊀` is one
/// token, `㊀` another), where `a一` would be two words to the query parser
/// and to a Japanese page's tokens, one Latin and one Japanese ([`crate::ja`]),
/// so `"a㊀ b"` would reparse as a different search (#241).
fn keeps_form(c: char) -> bool {
    let ja = is_ja(c);
    c.is_alphanumeric()
        && c.nfkd()
            .any(|d| (!is_combining_mark(d) && !d.is_alphanumeric()) || (!ja && is_ja(d)))
}

/// Append compatibility-decomposed characters, without their combining
/// marks, lowercased and transliterated.
fn push_folded(decomposed: impl Iterator<Item = char>, out: &mut String) {
    for c in decomposed
        .filter(|c| !is_combining_mark(*c))
        .flat_map(char::to_lowercase)
    {
        match c {
            'ſ' => out.push('s'),
            'æ' => out.push_str("ae"),
            'œ' => out.push_str("oe"),
            'ø' => out.push('o'),
            'ł' => out.push('l'),
            'ß' => out.push_str("ss"),
            'đ' | 'ð' => out.push('d'),
            'þ' => out.push_str("th"),
            'ı' => out.push('i'),
            'ŋ' => out.push_str("ng"),
            'ħ' => out.push('h'),
            c => out.push(c),
        }
    }
}

/// Split into folded tokens as the `usnm_text` analyzer does (with
/// [`Analyzer::V1`], except that it decomposes `½` and the like), dropping
/// words over [`MAX_TOKEN_CHARS`] and their positions: the words of a query,
/// which never has a longer one. A page's words as Quickwit indexes them in
/// `text` and `text_as` are [`index_tokens`].
pub fn tokenize(text: &str, analyzer: Analyzer) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| fold(t, analyzer))
        .filter(|t| t.chars().count() <= MAX_TOKEN_CHARS)
        .collect()
}

/// A text's words as Quickwit's `usnm_text` analyzer indexes them in `text`
/// and `text_as` (layout 1, 05 §5.5.1), one per position: split as
/// [`tokenize`] splits (tantivy's `simple` tokenizer splits at what isn't
/// alphanumeric), `None` for a word of [`REMOVE_LONG_BYTES`] or more as
/// split, which `remove_long` drops but whose position still counts, and
/// otherwise the word folded, however long (#286).
pub fn index_tokens(text: &str, analyzer: Analyzer) -> Vec<Option<String>> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| (t.len() < REMOVE_LONG_BYTES).then(|| fold(t, analyzer)))
        .collect()
}

/// The vulgar fractions, U+00BC–U+00BE, U+2150–U+215F and U+2189.
#[cfg(test)]
pub(crate) fn vulgar_fractions() -> impl Iterator<Item = char> {
    ('\u{BC}'..='\u{BE}')
        .chain('\u{2150}'..='\u{215F}')
        .chain(['\u{2189}'])
}

/// The characters that aren't Japanese but fold to a Japanese one with
/// [`Analyzer::V1`] (#241): the Hangzhou numerals U+3038–U+303A, the Kanbun
/// marks U+3192–U+3195 and the circled ideographs U+3280–U+3289.
#[cfg(test)]
pub(crate) fn folding_to_japanese() -> impl Iterator<Item = char> {
    ('\u{3038}'..='\u{303A}')
        .chain('\u{3192}'..='\u{3195}')
        .chain('\u{3280}'..='\u{3289}')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ja;

    #[test]
    fn normalizes_ocr_text() {
        let raw = "The indus-\ntry of the ſtate\u{0007} is  great.\n\n\nNew  para ﬁne";
        assert_eq!(
            normalize_ocr(raw),
            "The industry of the state is great.\nNew para fine"
        );
    }

    #[test]
    fn keeps_hyphens_that_are_not_line_breaks() {
        assert_eq!(
            normalize_ocr("well-known 1896-\n97"),
            "well-known 1896-\n97"
        );
    }

    #[test]
    fn classifies_text_status() {
        assert_eq!(text_status(""), TextStatus::Empty);
        assert_eq!(text_status("  \n "), TextStatus::Empty);
        assert_eq!(text_status("tiny page"), TextStatus::Short);
        assert_eq!(
            text_status("a reasonably long page of newsprint"),
            TextStatus::Ok
        );
    }

    #[test]
    fn analyzers_are_listed_in_order() {
        // Oldest first, numbered from 0, so `a as usize` indexes `ALL`.
        for (i, a) in Analyzer::ALL.iter().enumerate() {
            assert_eq!(*a as usize, i);
        }
        assert_eq!(Analyzer::ALL.last(), Some(&Analyzer::LATEST));
    }

    #[test]
    fn tokenizes_like_the_analyzer() {
        for a in Analyzer::ALL {
            assert_eq!(
                tokenize("Crucify mankind upon a CROSS of Gold! Café—ſo", a),
                vec!["crucify", "mankind", "upon", "a", "cross", "of", "gold", "cafe", "so"]
            );
            assert_eq!(
                tokenize("Æsop Œuvre Søren Łódź Straße Þór Đakovo", a),
                vec!["aesop", "oeuvre", "soren", "lodz", "strasse", "thor", "dakovo"]
            );
            assert!(tokenize("Æsop Œuvre Søren Łódź Straße", a)
                .iter()
                .all(|t| t.is_ascii()));
            let long = "x".repeat(41);
            assert!(tokenize(&long, a).is_empty());
        }
    }

    #[test]
    fn index_tokens_drop_words_of_255_bytes_keeping_their_positions() {
        // Quickwit 0.9.1's `remove_long` measures the word as split, in
        // bytes, before folding: 254 bytes stay, 255 go, and the position
        // stays either way (#286).
        for a in Analyzer::ALL {
            let kept = "x".repeat(REMOVE_LONG_BYTES - 1);
            let dropped = "x".repeat(REMOVE_LONG_BYTES);
            let text = format!("Gold {} of {dropped}, silver", "X".repeat(41));
            assert_eq!(
                index_tokens(&text, a),
                vec![
                    Some("gold".to_owned()),
                    Some("x".repeat(41)),
                    Some("of".to_owned()),
                    None,
                    Some("silver".to_owned())
                ]
            );
            assert_eq!(index_tokens(&kept, a), vec![Some(kept.clone())]);
            // `tokenize` drops both, and their positions.
            assert_eq!(tokenize(&text, a), vec!["gold", "of", "silver"]);
            // Bytes, not characters: `é` is two bytes, folded to `e`.
            let e254 = format!("Q{}x", "é".repeat(126));
            let e255 = format!("Q{}", "é".repeat(127));
            assert_eq!((e254.len(), e255.len()), (254, 255));
            assert_eq!(
                index_tokens(&format!("{e254} {e255}"), a),
                vec![Some(format!("q{}x", "e".repeat(126))), None]
            );
            // Measured before folding, which can lengthen a word (`Æ` is `ae`).
            let ae = "Æ".repeat(127);
            assert_eq!(ae.len(), 254);
            assert_eq!(index_tokens(&ae, a), vec![Some("ae".repeat(127))]);
            // Otherwise the words are `tokenize`'s.
            let s = "Crucify mankind upon a CROSS of Gold! Café—ſo ½ 61¼ ㊀ a㊁";
            let words: Vec<String> = index_tokens(s, a).into_iter().flatten().collect();
            assert_eq!(words, tokenize(s, a));
        }
    }

    #[test]
    fn fractions_are_one_token_as_the_analyzer_has_them() {
        // Quickwit 0.9.1's `usnm_text` keeps `½` as one token (its ASCII
        // folding has no entry for it) and splits at `/`, U+2044 FRACTION
        // SLASH and U+2215 DIVISION SLASH (#168).
        let v2 = Analyzer::V2;
        assert_eq!(
            tokenize("Wheat ½ higher at 61¼; oats 1/2, 1⁄2 and 1∕2", v2),
            vec![
                "wheat", "½", "higher", "at", "61¼", "oats", "1", "2", "1", "2", "and", "1", "2"
            ]
        );
        for f in vulgar_fractions() {
            assert_eq!(fold(&f.to_string(), v2), f.to_string(), "{f:?}");
        }
        // Other characters that decompose into a separator stay whole too,
        // and `Ŀ` folds as ASCII folding has it.
        assert_eq!(tokenize("⒈ ⑴ Ŀ ŀ ﷺ", v2), vec!["⒈", "⑴", "l", "l", "ﷺ"]);
    }

    #[test]
    fn characters_that_fold_to_japanese_stay_themselves_from_version_2() {
        // Quickwit 0.9.1's `usnm_text` keeps these as they are (its ASCII
        // folding has no entry for them), and a Latin letter before one is
        // the same token: `a㊀` is one word, which matches neither `a一` nor
        // `㊀` (#241).
        let v2 = Analyzer::V2;
        assert_eq!(
            tokenize("Lot ㊀ and A㊁, 〸 ㆒ 一", v2),
            vec!["lot", "㊀", "and", "a㊁", "〸", "㆒", "一"]
        );
        let all: String = folding_to_japanese().collect();
        assert_eq!(all.chars().count(), 17);
        assert_eq!(fold(&all, v2), all);
        // Version 1 folds them to the Japanese characters.
        assert_eq!(
            tokenize("Lot ㊀ and A㊁, 〸 ㆒", Analyzer::V1),
            vec!["lot", "一", "and", "a二", "十", "一"]
        );
        // They are every character that isn't Japanese and folds to a
        // Japanese one alone; `㈠` (`(一)`) holds a separator, so version 2
        // kept it already (#168).
        for c in (0..=0x10FFFF).filter_map(char::from_u32) {
            let f1 = fold(&c.to_string(), Analyzer::V1);
            let listed = folding_to_japanese().any(|x| x == c);
            let to_ja = c.is_alphanumeric()
                && !ja::is_ja(c)
                && ja::has_ja(&f1)
                && f1.chars().all(char::is_alphanumeric);
            assert_eq!(listed, to_ja, "{c:?} folds to {f1:?}");
        }
    }

    #[test]
    fn version_1_decomposes_fractions() {
        // The folding indexes built before #168 have: `½` is `1⁄2`.
        let v1 = Analyzer::V1;
        assert_eq!(fold("½", v1), "1\u{2044}2");
        assert_eq!(fold("61¼", v1), "611\u{2044}4");
        assert_eq!(fold("Ŀ", v1), "l\u{b7}");
        assert_eq!(
            tokenize("Wheat ½ higher at 61¼; oats 1/2", v1),
            vec![
                "wheat",
                "1\u{2044}2",
                "higher",
                "at",
                "611\u{2044}4",
                "oats",
                "1",
                "2"
            ]
        );
    }

    /// `fold` as it was before analyzer versions (#168): what
    /// [`Analyzer::V1`] must keep doing, for the indexes built with it.
    fn fold_before_versions(token: &str) -> String {
        let mut out = String::new();
        for c in token
            .nfkd()
            .filter(|c| !is_combining_mark(*c))
            .flat_map(char::to_lowercase)
        {
            match c {
                'ſ' => out.push('s'),
                'æ' => out.push_str("ae"),
                'œ' => out.push_str("oe"),
                'ø' => out.push('o'),
                'ł' => out.push('l'),
                'ß' => out.push_str("ss"),
                'đ' | 'ð' => out.push('d'),
                'þ' => out.push_str("th"),
                'ı' => out.push('i'),
                'ŋ' => out.push_str("ng"),
                'ħ' => out.push('h'),
                c => out.push(c),
            }
        }
        out
    }

    #[test]
    fn version_1_folds_every_character_as_before() {
        for c in (0..=0x10FFFF).filter_map(char::from_u32) {
            let s = c.to_string();
            assert_eq!(fold(&s, Analyzer::V1), fold_before_versions(&s), "{c:?}");
        }
        for s in ["Ñoño Æsop ½ 61¼ Ŀa ﷺ ⑴", "a\u{301}\u{323}b", "가각"] {
            assert_eq!(fold(s, Analyzer::V1), fold_before_versions(s), "{s:?}");
        }
    }

    #[test]
    fn version_2_differs_only_on_the_characters_it_keeps() {
        for c in (0..=0x10FFFF).filter_map(char::from_u32) {
            let s = c.to_string();
            if !keeps_form(c) && !matches!(c, 'Ŀ' | 'ŀ') {
                assert_eq!(fold(&s, Analyzer::V2), fold(&s, Analyzer::V1), "{c:?}");
            }
        }
    }

    #[test]
    fn every_folded_word_is_one_token() {
        // With version 2, a word of one alphanumeric character folds to a
        // word that tokenizes as itself, so the query parser's canonical form
        // reparses the same. Version 1 breaks this for the characters it
        // decomposes into separators (#168), and on a Japanese page for the
        // ones it folds to Japanese characters (#241).
        let v2 = Analyzer::V2;
        for c in (0..=0x10FFFF).filter_map(char::from_u32) {
            if !c.is_alphanumeric() {
                continue;
            }
            let f = fold(&c.to_string(), v2);
            assert!(f.chars().all(char::is_alphanumeric), "{c:?} folds to {f:?}");
            assert_eq!(fold(&f, v2), f, "{c:?} folds again");
            if !f.is_empty() {
                assert_eq!(tokenize(&f, v2), vec![f.clone()], "{c:?}");
                // A Japanese page tokenizes it as one Latin word too, as the
                // query parser does (#241); a Japanese character has its
                // own folding there.
                if !ja::is_ja(c) {
                    assert_eq!(ja::tokenize(&f, v2), vec![f.clone()], "{c:?}");
                }
            }
            let f1 = fold(&c.to_string(), Analyzer::V1);
            if f1.chars().all(char::is_alphanumeric) && !f1.is_empty() {
                assert_eq!(tokenize(&f1, Analyzer::V1), vec![f1.clone()], "{c:?}");
            }
        }
    }
}
