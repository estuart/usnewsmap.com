//! A page's tokens and word tokens, as `ja-ocr/damage_v2.py` reads them
//! (`text_words`, `token_info`, `garbage`), with Python's string semantics.
//!
//! Python and Rust disagree on several character classes, so this module
//! spells Python's out (12 §12.5 Step 5, "known traps"):
//!
//! - `str.isalpha` is the general categories `L*`, where Rust's
//!   `char::is_alphabetic` also takes combining marks (`Other_Alphabetic`).
//! - `str.isalnum` and the `re` module's `\w` add the numbers (`N*`);
//!   `\d` is `Nd`.
//! - `str.split()` and `\s` split on `\x1c` to `\x1f` as well, which
//!   `char::is_whitespace` doesn't.
//! - `str.casefold` is full case folding (ß to "ss"), where
//!   `to_lowercase` keeps ß.

use caseless::default_case_fold_str;
use unicode_general_category::{get_general_category, GeneralCategory as G};
use unicode_normalization::char::canonical_combining_class;
use unicode_normalization::UnicodeNormalization;

/// Python's `str.isalpha` for one character: a letter.
pub(crate) fn is_alpha(c: char) -> bool {
    if c.is_ascii() {
        return c.is_ascii_alphabetic();
    }
    matches!(
        get_general_category(c),
        G::UppercaseLetter
            | G::LowercaseLetter
            | G::TitlecaseLetter
            | G::ModifierLetter
            | G::OtherLetter
    )
}

/// Python's `str.isalnum` for one character: a letter or a number.
pub(crate) fn is_alnum(c: char) -> bool {
    if c.is_ascii() {
        return c.is_ascii_alphanumeric();
    }
    is_alpha(c)
        || matches!(
            get_general_category(c),
            G::DecimalNumber | G::LetterNumber | G::OtherNumber
        )
}

/// Python's `\d`: a decimal digit.
fn is_decimal(c: char) -> bool {
    if c.is_ascii() {
        return c.is_ascii_digit();
    }
    get_general_category(c) == G::DecimalNumber
}

/// Python's `\w`.
fn is_word(c: char) -> bool {
    c == '_' || is_alnum(c)
}

/// Python's `str.isspace` and `\s`.
pub(crate) fn is_space(c: char) -> bool {
    c.is_whitespace() || ('\x1c'..='\x1f').contains(&c)
}

fn is_title(c: char) -> bool {
    get_general_category(c) == G::TitlecaseLetter
}

/// Python's `str.isalpha` for a string: not empty, every character a letter.
pub(crate) fn str_is_alpha(s: &str) -> bool {
    !s.is_empty() && s.chars().all(is_alpha)
}

/// Python's `str.islower` for one character.
fn char_is_lower(c: char) -> bool {
    c.is_lowercase() && !c.is_uppercase() && !is_title(c)
}

/// Python's `str.isupper` for one character.
fn char_is_upper(c: char) -> bool {
    c.is_uppercase() && !c.is_lowercase() && !is_title(c)
}

/// Python's `str.islower`: no upper- or titlecase character, and at least one lowercase one.
fn str_is_lower(s: &str) -> bool {
    let mut cased = false;
    for c in s.chars() {
        if c.is_uppercase() || is_title(c) {
            return false;
        }
        cased |= c.is_lowercase();
    }
    cased
}

/// Python's `str.isupper`.
fn str_is_upper(s: &str) -> bool {
    let mut cased = false;
    for c in s.chars() {
        if c.is_lowercase() || is_title(c) {
            return false;
        }
        cased |= c.is_uppercase();
    }
    cased
}

/// Python's `str.casefold`: full case folding.
pub(crate) fn casefold(s: &str) -> String {
    if s.is_ascii() {
        return s.to_ascii_lowercase();
    }
    default_case_fold_str(s)
}

/// Latin script: below U+0250, or Latin Extended Additional.
pub(crate) fn latin_char(c: char) -> bool {
    let o = c as u32;
    o < 0x250 || (0x1E00..=0x1EFF).contains(&o)
}

/// `latin`: every character in Latin script.
fn latin(word: &str) -> bool {
    word.chars().all(latin_char)
}

const VOWELS: &str = "aeiouyàáâãäåæèéêëìíîïòóôõöøùúûüýÿāăąēĕėęěīĭįıōŏőœūŭůűų";

/// `mixed_case`: a capital inside a lower-case word ("tHe", "ThE",
/// "HeLLoS"; not "McKay", "MacDonald", "USA").
fn mixed_case(word: &str) -> bool {
    let chars: Vec<char> = word.chars().collect();
    if str_is_lower(word) || str_is_upper(word) || chars.len() < 2 {
        return false;
    }
    if char_is_lower(chars[0]) {
        return true;
    }
    let rises = chars
        .windows(2)
        .filter(|p| char_is_lower(p[0]) && char_is_upper(p[1]))
        .count();
    let n = chars.len();
    rises >= 2 || (rises == 1 && char_is_upper(chars[n - 1]) && char_is_lower(chars[n - 2]))
}

/// `TRIPLE`: the same letter (a word character that isn't a digit or `_`)
/// three times in a row.
fn has_triple(s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    chars
        .windows(3)
        .any(|w| w[0] == w[1] && w[1] == w[2] && is_alnum(w[0]) && !is_decimal(w[0]))
}

/// `ROMAN`: a Roman numeral in lower case.
fn is_roman(s: &str) -> bool {
    const G1: &[&str] = &["", "m", "mm", "mmm", "mmmm"];
    const G2: &[&str] = &["cm", "cd", "", "c", "cc", "ccc", "d", "dc", "dcc", "dccc"];
    const G3: &[&str] = &["xc", "xl", "", "x", "xx", "xxx", "l", "lx", "lxx", "lxxx"];
    const G4: &[&str] = &["ix", "iv", "", "i", "ii", "iii", "v", "vi", "vii", "viii"];
    if s.is_empty() || !s.chars().all(|c| "mdclxvi".contains(c)) {
        return false;
    }
    G1.iter().any(|a| {
        s.strip_prefix(a).is_some_and(|s| {
            G2.iter().any(|b| {
                s.strip_prefix(b).is_some_and(|s| {
                    G3.iter()
                        .any(|c| s.strip_prefix(c).is_some_and(|s| G4.contains(&s)))
                })
            })
        })
    })
}

/// `CORE`: a token from its first to its last letter or digit.
fn core(token: &str) -> Option<&str> {
    let start = token.char_indices().find(|&(_, c)| is_alnum(c))?.0;
    let (last, c) = token.char_indices().rev().find(|&(_, c)| is_alnum(c))?;
    Some(&token[start..last + c.len_utf8()])
}

/// `garbage`: rmgarbage-style rules (Taghva, Nartker, Condit and Borsack
/// 2001) on a whitespace token. `syllabic`: the page's language has words
/// with no vowel (Czech "smrt").
fn garbage(token: &str, core: Option<&str>, syllabic: bool) -> bool {
    let n = token.chars().count();
    if n > 40 {
        return true;
    }
    if n >= 2 && token.chars().filter(|&c| !is_alnum(c)).count() * 2 > n {
        return true;
    }
    let Some(core) = core else {
        return false;
    };
    let folded = casefold(core);
    if has_triple(&folded) && !is_roman(&folded) {
        return true;
    }
    if str_is_alpha(core) {
        if core.chars().count() >= 4 && latin(core) {
            let len = folded.chars().count();
            let vowels = folded.chars().filter(|&c| VOWELS.contains(c)).count();
            if vowels == len || (vowels == 0 && !syllabic) {
                return true;
            }
        }
        if mixed_case(core) {
            return true;
        }
    }
    false
}

/// Yiddish ligatures as their two letters.
fn ligatures(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\u{05f0}' => out.push_str("\u{05d5}\u{05d5}"),
            '\u{05f1}' => out.push_str("\u{05d5}\u{05d9}"),
            '\u{05f2}' => out.push_str("\u{05d9}\u{05d9}"),
            c => out.push(c),
        }
    }
    out
}

/// What `token_info` finds in a whitespace token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TokenInfo {
    /// The token's word, case-folded, if it has one.
    pub word: Option<String>,
    /// rmgarbage flags it.
    pub garbage: bool,
    /// It flags it in a language whose words can lack vowels.
    pub garbage_syllabic: bool,
}

/// `token_info`.
pub(crate) fn token_info(token: &str) -> TokenInfo {
    let core = core(token);
    let g = garbage(token, core, false);
    let g_syllabic = g && garbage(token, core, true);
    let word = core.and_then(|core| {
        // Points and accents as combining marks: "פֿאר", "é".
        let core = if str_is_alpha(core) {
            ligatures(core)
        } else {
            let composed: String = core
                .nfc()
                .filter(|&c| canonical_combining_class(c) == 0)
                .collect();
            ligatures(&composed)
        };
        (core.chars().count() >= 2 && str_is_alpha(&core)).then(|| casefold(&core))
    });
    TokenInfo {
        word,
        garbage: g,
        garbage_syllabic: g_syllabic,
    }
}

/// `DEHYPHENATE`: "rail-\nroad" as "railroad", as Python's
/// `re.sub(r"(\w)-\n\s*(?=\w)", r"\1", text)`.
fn dehyphenate(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains("-\n") {
        return text.into();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if is_word(c) && chars.get(i + 1) == Some(&'-') && chars.get(i + 2) == Some(&'\n') {
            let mut j = i + 3;
            while j < chars.len() && is_space(chars[j]) {
                j += 1;
            }
            if j < chars.len() && is_word(chars[j]) {
                out.push(c);
                i = j;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out.into()
}

/// A page's whitespace tokens, read as `text_words` reads them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextWords {
    /// Whitespace tokens.
    pub tokens: usize,
    /// Word tokens, case-folded, in order.
    pub words: Vec<String>,
    /// Tokens rmgarbage flags.
    pub garbage: usize,
    /// Of those, the ones it flags in a language whose words can lack vowels.
    pub garbage_syllabic: usize,
}

/// `text_words`: a page's whitespace tokens, its word tokens (letters only,
/// at least 2, case-folded) and the tokens rmgarbage flags. A word broken
/// across lines with a hyphen is joined first.
pub fn text_words(text: &str) -> TextWords {
    // Pages repeat most of their tokens, and each other's: each thread keeps
    // what it found for up to `CACHED` distinct tokens, as `token_info`'s
    // lru_cache does in Python.
    const CACHED: usize = 1 << 18;
    thread_local! {
        static CACHE: std::cell::RefCell<std::collections::HashMap<String, TokenInfo>> =
            std::cell::RefCell::new(std::collections::HashMap::new());
    }
    let text = dehyphenate(text);
    let mut out = TextWords::default();
    CACHE.with_borrow_mut(|cache| {
        for token in text.split(is_space).filter(|t| !t.is_empty()) {
            out.tokens += 1;
            let info = match cache.get(token) {
                Some(info) => info,
                None => {
                    if cache.len() >= CACHED {
                        cache.clear();
                    }
                    cache
                        .entry(token.to_owned())
                        .or_insert_with(|| token_info(token))
                }
            };
            out.garbage += usize::from(info.garbage);
            out.garbage_syllabic += usize::from(info.garbage_syllabic);
            if let Some(w) = &info.word {
                out.words.push(w.clone());
            }
        }
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &str) -> Vec<String> {
        text_words(text).words
    }

    #[test]
    fn python_character_classes() {
        // Combining marks are alphabetic to Rust, not to Python.
        assert!('\u{05bf}'.is_alphabetic());
        assert!(!is_alpha('\u{05bf}'));
        assert!(!is_alpha('\u{0301}'));
        assert!(is_alnum('5') && is_alnum('½') && !is_alnum('_') && is_word('_'));
        for c in [
            '\x1c', '\x1d', '\x1e', '\x1f', '\u{85}', '\u{a0}', '\u{3000}',
        ] {
            assert!(is_space(c), "{c:?}");
        }
        assert!(!is_space('\u{200b}'));
        assert_eq!(casefold("Straße"), "strasse");
        assert_eq!(casefold("ΣΊΣΥΦΟΣ"), "σίσυφοσ");
        assert_eq!(casefold("ſ"), "s");
    }

    #[test]
    fn tokens_split_on_python_whitespace() {
        let t = text_words("the\x1cold\x1fman  \u{a0}ran");
        assert_eq!((t.tokens, t.words.len()), (4, 4));
    }

    #[test]
    fn words_are_letters_case_folded() {
        // "council's" holds a letter that isn't one; "a" and "Ø" are too short.
        assert_eq!(
            words("The, COUNCIL's (Met) a 1905 Ø STRASSE"),
            ["the", "met", "strasse"]
        );
    }

    #[test]
    fn hyphenated_line_breaks_are_joined() {
        assert_eq!(words("turn-\ning the well-known"), ["turning", "the"]);
        assert_eq!(words("turn-\n   ing"), ["turning"]);
        assert_eq!(words("turn-\n-ing"), ["turn", "ing"]);
    }

    #[test]
    fn combining_marks_and_ligatures() {
        assert!(words("פֿארזאמלט").contains(&"פארזאמלט".to_owned()));
        assert_eq!(words("\u{05f0}אס \u{05d5}\u{05d5}אס"), ["וואס", "וואס"]);
        for (lig, pair) in [("\u{05f0}", "וו"), ("\u{05f1}", "וי"), ("\u{05f2}", "יי")] {
            assert_eq!(words(&format!("{lig} {pair}")), [pair, pair]);
        }
        // A combining accent is dropped; a precomposed letter stays.
        assert_eq!(words("cafe\u{0301} naïve"), ["cafe", "naïve"]);
    }

    #[test]
    fn garbage_rules() {
        let g = |t: &str| token_info(t).garbage;
        for t in [
            "tHe",
            "aaaa",
            "bcdfg",
            "%%%a",
            "eeee",
            &"x".repeat(41),
            "aaa",
        ] {
            assert!(g(t), "{t}");
        }
        for t in [
            "the",
            "McKay",
            "USA",
            "MacDonald",
            "xviii",
            "xxx",
            "mmm",
            "street",
            "1905",
        ] {
            assert!(!g(t), "{t}");
        }
        // No vowel: garbage in English, not in Czech.
        let smrt = token_info("smrt");
        assert!(smrt.garbage && !smrt.garbage_syllabic);
    }

    #[test]
    fn roman_numerals() {
        for s in ["xviii", "mcmxlv", "iii", "mmmm", "cd"] {
            assert!(is_roman(s), "{s}");
        }
        for s in ["", "iiii", "vv", "xxxx", "mmmmm", "abc"] {
            assert!(!is_roman(s), "{s}");
        }
    }
}
