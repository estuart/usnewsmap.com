//! Text normalization, the same as `normalize` in scripts/ocr-survey/gold.py.
//!
//! Applied to the transcription and to every hypothesis before comparing:
//!  1. Unicode NFC. Nothing else is folded: case, punctuation, diacritics, long s and ligatures all count.
//!  2. A hyphen at a line's end followed by a line break is removed and the line joined.
//!  3. Line breaks and runs of whitespace become one space; the ends are stripped.
//!
//! In the transcription only: `[?x]` (a guessed character or characters) counts as x, and `[?]`
//! (an unreadable character) is dropped.
//!
//! Characters are Unicode scalar values, as Python's `str` counts them, and whitespace is what
//! Python's `str.isspace` (and so `re`'s `\s`, `str.split` and `str.strip`) accepts.

use unicode_normalization::UnicodeNormalization;

/// An unreadable character in a transcription.
pub const UNKNOWN: &str = "[?]";
/// A skipped span; validation rejects crops that carry it.
pub const SKIPPED: &str = "[...]";

/// Python's `str.isspace`: Rust's White_Space plus the four information separators U+001C..U+001F.
pub fn is_py_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n'
            | '\u{0b}'
            | '\u{0c}'
            | '\r'
            | '\u{1c}'..='\u{1f}'
            | ' '
            | '\u{85}'
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
    )
}

/// Python's `str.strip()` with no argument.
pub fn py_strip(s: &str) -> &str {
    s.trim_matches(is_py_space)
}

/// Python's `str.split()` with no argument.
pub fn py_split(s: &str) -> Vec<&str> {
    s.split(is_py_space).filter(|w| !w.is_empty()).collect()
}

/// One match of gold.py's `GUESS` pattern, `\[\?([^\[\]\n]+)\]`, at the start of `s`: the guessed
/// text and the length of the whole match in bytes.
fn guess_at(s: &str) -> Option<(&str, usize)> {
    let rest = s.strip_prefix("[?")?;
    let end = rest.find(['[', ']', '\n'])?;
    if end == 0 || !rest[end..].starts_with(']') {
        return None;
    }
    Some((&rest[..end], 2 + end + 1))
}

/// A piece of text split around `GUESS` matches: plain text, or a match's guessed text.
enum Piece<'a> {
    Text(&'a str),
    Guess(&'a str),
}

/// The non-overlapping `GUESS` matches, left to right, and the text between them.
fn pieces(s: &str) -> Vec<Piece<'_>> {
    let mut out = Vec::new();
    let mut i = 0;
    let mut start = 0;
    while i < s.len() {
        if let Some((g, len)) = guess_at(&s[i..]) {
            out.push(Piece::Text(&s[start..i]));
            out.push(Piece::Guess(g));
            i += len;
            start = i;
        } else {
            i += s[i..].chars().next().map_or(1, char::len_utf8);
        }
    }
    out.push(Piece::Text(&s[start..]));
    out
}

/// `GUESS.sub(lambda m: m.group(1), s)`.
pub fn resolve_guesses(s: &str) -> String {
    pieces(s)
        .into_iter()
        .map(|p| match p {
            Piece::Text(t) | Piece::Guess(t) => t,
        })
        .collect()
}

/// `GUESS.sub("", s)`.
pub fn drop_guesses(s: &str) -> String {
    pieces(s)
        .into_iter()
        .filter_map(|p| match p {
            Piece::Text(t) => Some(t),
            Piece::Guess(_) => None,
        })
        .collect()
}

/// `GUESS.findall(s)`, as the guessed texts.
pub fn find_guesses(s: &str) -> Vec<&str> {
    pieces(s)
        .into_iter()
        .filter_map(|p| match p {
            Piece::Guess(g) => Some(g),
            Piece::Text(_) => None,
        })
        .collect()
}

/// `re.sub(r"-[ \t]*\r?\n[ \t]*", "", s)`.
fn join_hyphens(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut start = 0;
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'-' {
            let mut j = i + 1;
            while j < b.len() && matches!(b[j], b' ' | b'\t') {
                j += 1;
            }
            if j < b.len() && b[j] == b'\r' {
                j += 1;
            }
            if j < b.len() && b[j] == b'\n' {
                j += 1;
                while j < b.len() && matches!(b[j], b' ' | b'\t') {
                    j += 1;
                }
                out.push_str(&s[start..i]);
                i = j;
                start = j;
                continue;
            }
        }
        i += 1;
    }
    out.push_str(&s[start..]);
    out
}

/// `re.sub(r"\s+", " ", s).strip()`.
fn squash_spaces(s: &str) -> String {
    py_split(s).join(" ")
}

/// gold.py's `normalize(text, truth)`.
pub fn normalize(text: &str, truth: bool) -> String {
    let mut t: String = text.nfc().collect();
    if truth {
        t = resolve_guesses(&t).replace(UNKNOWN, "");
    }
    squash_spaces(&join_hyphens(&t))
}

/// gold.py's `unknown_share`: the share of the resolved text's characters that are unknown or
/// guessed; `[?abc]` counts 3.
pub fn unknown_share(text: &str) -> f64 {
    let guessed: usize = find_guesses(text).iter().map(|g| g.chars().count()).sum();
    let unknown = drop_guesses(text).matches(UNKNOWN).count();
    let resolved = resolve_guesses(text)
        .replace(UNKNOWN, "?")
        .replace('\n', "");
    let n = resolved.chars().count();
    (unknown + guessed) as f64 / (if n == 0 { 1 } else { n }) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_like_gold_py() {
        assert_eq!(normalize("  a  b\n\tc ", false), "a b c");
        assert_eq!(normalize("Clare-\nmont", false), "Claremont");
        assert_eq!(normalize("Clare- \r\n  mont", false), "Claremont");
        assert_eq!(normalize("Clare-x\nmont", false), "Clare-x mont");
        assert_eq!(normalize("--\nx", false), "-x");
        assert_eq!(normalize("e\u{301}", false), "\u{e9}");
        assert_eq!(normalize("a[?b]c[?]d", true), "abcd");
        assert_eq!(normalize("a[?b]c[?]d", false), "a[?b]c[?]d");
        assert_eq!(normalize("[?[?x]]", true), "[?x]");
        assert_eq!(
            normalize("a\u{1c}b\u{a0}c\u{200b}d", false),
            "a b c\u{200b}d"
        );
    }

    #[test]
    fn unknown_share_counts_guesses_and_unknowns() {
        assert_eq!(unknown_share(""), 0.0);
        assert!((unknown_share("ab[?cd][?]") - 3.0 / 5.0).abs() < 1e-12);
    }
}
