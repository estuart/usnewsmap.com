//! The user query language (06 §6.4).
//!
//! ```text
//! query     := or_expr
//! or_expr   := and_expr ( "OR" and_expr )*
//! and_expr  := unary ( ["AND"] unary )*
//! unary     := ("-" | "NOT") primary | primary
//! primary   := PHRASE [ "~" INT ]
//!            | TERM [ "~" ("1" | "2") | "*" ]
//!            | WILDCARD
//!            | "(" query ")"
//! ```
//!
//! A `WILDCARD` is a word with `?` (exactly one character) or `*` (any run)
//! inside it, after at least [`MIN_PREFIX_CHARS`] letters (#124).
//!
//! Only the parsed AST ever reaches a search backend; raw user syntax is never
//! forwarded, which removes the legacy Solr injection risk structurally.

use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::ja::{self, has_ja, tokenize, MAX_JA_RUN_CHARS};
use crate::text::{fold, Analyzer, MAX_TOKEN_CHARS};

pub const MAX_QUERY_CHARS: usize = 256;
pub const MAX_TERMS: usize = 12;
pub const MAX_OR_BRANCHES: usize = 4;
/// Letters a word needs before a trailing `*` (`influ*`) or before its first
/// wildcard (`presi?ent`, #124). Either search walks every indexed word that
/// starts with them, in every split: on the full index three letters cost
/// 39 s (`inf*`) to over 120 s (`con*`) of searcher time per cold search
/// (06 §6.4).
pub const MIN_PREFIX_CHARS: usize = 5;
pub const MAX_FUZZY: u8 = 2;
pub const MAX_SLOP: u8 = 20;
const MAX_DEPTH: usize = 8;

#[derive(Debug, Clone, Error, PartialEq, Eq, Serialize)]
#[error("{message}")]
pub struct QueryError {
    pub message: String,
    /// Character offset in the input where the problem was found.
    pub position: Option<usize>,
}

impl QueryError {
    fn at(message: impl Into<String>, position: usize) -> Self {
        Self {
            message: message.into(),
            position: Some(position),
        }
    }
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            position: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Term {
    /// Folded (lowercase, ASCII) token.
    pub text: String,
    /// Edit distance for OCR-tolerant matching (0 = exact).
    pub fuzzy: u8,
    pub prefix: bool,
    /// `text` is a pattern (#124): `?` stands for exactly one character and
    /// `*` for any run of them, after at least [`MIN_PREFIX_CHARS`] letters.
    /// A word with only a trailing `*` is a `prefix` instead.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub wildcard: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum Node {
    Term(Term),
    Phrase { terms: Vec<String>, slop: u8 },
    And(Vec<Node>),
    Or(Vec<Node>),
    Not(Box<Node>),
}

/// How a plain word list is combined (06 §6.3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Phrase,
    All,
    Any,
    Near,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Phrase => "phrase",
            Self::All => "all",
            Self::Any => "any",
            Self::Near => "near",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "phrase" => Some(Self::Phrase),
            "all" => Some(Self::All),
            "any" => Some(Self::Any),
            "near" => Some(Self::Near),
            _ => None,
        }
    }
}

/// Build the AST for a request: `q` in the query language, optionally combined
/// by `mode` when `q` is a plain word list, with `fuzzy` applied to exact terms,
/// its words folded by `analyzer`: the one the searched index was built with.
pub fn build(
    q: &str,
    mode: Option<Mode>,
    near: u8,
    fuzzy: u8,
    analyzer: Analyzer,
) -> Result<Node, QueryError> {
    let q = q.trim();
    if q.is_empty() {
        return Err(QueryError::new("query is empty"));
    }
    if q.chars().count() > MAX_QUERY_CHARS {
        return Err(QueryError::new(format!(
            "query is longer than {MAX_QUERY_CHARS} characters"
        )));
    }
    if fuzzy > MAX_FUZZY {
        return Err(QueryError::new(format!("fuzzy must be 0–{MAX_FUZZY}")));
    }
    let plain = is_plain(q);
    let node = match mode {
        None | Some(Mode::All) => parse_with(q, analyzer)?,
        Some(m) if !plain => {
            return Err(QueryError::new(format!(
                "mode={} needs a plain list of words; use mode=all with query syntax",
                m.as_str()
            )))
        }
        Some(Mode::Phrase) => phrase(tokenize(q, analyzer), 0)?,
        Some(Mode::Near) => {
            if near == 0 || near > MAX_SLOP {
                return Err(QueryError::new(format!("near must be 1–{MAX_SLOP}")));
            }
            phrase(tokenize(q, analyzer), near)?
        }
        Some(Mode::Any) => {
            // A Japanese word is a phrase of characters, not one alternative per character.
            let mut terms: Vec<Node> = Vec::new();
            for w in q.split_whitespace() {
                if has_ja(w) {
                    terms.push(word(w, 0, 0, analyzer)?);
                } else {
                    terms.extend(tokenize(w, analyzer).into_iter().map(exact));
                }
            }
            match terms.len() {
                0 => return Err(QueryError::new("query has no searchable words")),
                1 => terms.into_iter().next().expect("one term"),
                _ => Node::Or(terms),
            }
        }
    };
    let node = apply_fuzzy(normalize(node), fuzzy);
    validate(&node)?;
    Ok(node)
}

/// Parse the query language with the latest analyzer
/// ([`Analyzer::LATEST`]), for tests and tools. A search parses with the
/// analyzer of the index it searches ([`build`], [`parse_with`]).
pub fn parse(input: &str) -> Result<Node, QueryError> {
    parse_with(input, Analyzer::LATEST)
}

/// Parse the query language, folding words with `analyzer`.
pub fn parse_with(input: &str, analyzer: Analyzer) -> Result<Node, QueryError> {
    let tokens = lex(input)?;
    let mut p = Parser {
        tokens,
        pos: 0,
        depth: 0,
        input_len: input.chars().count(),
        analyzer,
    };
    let node = p.or_expr()?;
    if let Some(t) = p.peek() {
        return Err(QueryError::at("unexpected input", t.pos));
    }
    let node = normalize(node);
    validate(&node)?;
    Ok(node)
}

fn is_plain(q: &str) -> bool {
    !q.chars()
        .any(|c| matches!(c, '"' | '(' | ')' | '~' | '*' | ':'))
        && !q
            .split_whitespace()
            .any(|w| w == "AND" || w == "OR" || w == "NOT" || w.starts_with('-') || has_wildcard(w))
}

fn exact(text: String) -> Node {
    Node::Term(Term {
        text,
        fuzzy: 0,
        prefix: false,
        wildcard: false,
    })
}

fn phrase(terms: Vec<String>, slop: u8) -> Result<Node, QueryError> {
    match terms.len() {
        0 => Err(QueryError::new("query has no searchable words")),
        1 if slop == 0 => Ok(exact(terms.into_iter().next().expect("one term"))),
        _ => Ok(Node::Phrase { terms, slop }),
    }
}

// ---------------------------------------------------------------- lexer

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    LParen,
    RParen,
    Minus,
    And,
    Or,
    Not,
    Phrase {
        text: String,
        slop: u8,
    },
    /// A word as typed, with any `*` and `?` in it.
    Word {
        text: String,
        fuzzy: u8,
    },
}

#[derive(Debug, Clone)]
struct Spanned {
    tok: Tok,
    pos: usize,
}

fn lex(input: &str) -> Result<Vec<Spanned>, QueryError> {
    let chars: Vec<char> = input.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let pos = i;
        match c {
            c if c.is_whitespace() => i += 1,
            '(' => {
                out.push(Spanned {
                    tok: Tok::LParen,
                    pos,
                });
                i += 1;
            }
            ')' => {
                out.push(Spanned {
                    tok: Tok::RParen,
                    pos,
                });
                i += 1;
            }
            '-' if i + 1 < chars.len() && !chars[i + 1].is_whitespace() => {
                out.push(Spanned {
                    tok: Tok::Minus,
                    pos,
                });
                i += 1;
            }
            '"' => {
                let start = i + 1;
                let end = chars[start..]
                    .iter()
                    .position(|&c| c == '"')
                    .map(|n| start + n)
                    .ok_or_else(|| QueryError::at("unterminated quote", pos))?;
                let text: String = chars[start..end].iter().collect();
                i = end + 1;
                let (slop, next) = read_tilde(&chars, i, MAX_SLOP, "phrase slop")?;
                i = next;
                out.push(Spanned {
                    tok: Tok::Phrase { text, slop },
                    pos,
                });
            }
            _ => {
                let start = i;
                while i < chars.len()
                    && !chars[i].is_whitespace()
                    && !matches!(chars[i], '(' | ')' | '"' | '~')
                {
                    if chars[i] == ':' {
                        return Err(QueryError::at("field syntax (':') is not supported", i));
                    }
                    i += 1;
                }
                let text: String = chars[start..i].iter().collect();
                if text.is_empty() {
                    return Err(QueryError::at("'~' must follow a word or phrase", i));
                }
                let tok = match text.as_str() {
                    "AND" => Tok::And,
                    "OR" => Tok::Or,
                    "NOT" => Tok::Not,
                    _ => {
                        if has_wildcard(&text) && i < chars.len() && chars[i] == '~' {
                            return Err(QueryError::at(NOT_BOTH, i));
                        }
                        let (fuzzy, next) = read_tilde(&chars, i, MAX_FUZZY, "fuzzy distance")?;
                        if next < chars.len() && matches!(chars[next], '*' | '?' | '~') {
                            return Err(QueryError::at(NOT_BOTH, next));
                        }
                        i = next;
                        Tok::Word { text, fuzzy }
                    }
                };
                out.push(Spanned { tok, pos: start });
            }
        }
    }
    Ok(out)
}

const NOT_BOTH: &str = "a term can be fuzzy or have wildcards (* or ?), not both";

/// Read an optional `~N` at `i`. Returns (N, next index).
fn read_tilde(chars: &[char], i: usize, max: u8, what: &str) -> Result<(u8, usize), QueryError> {
    if i >= chars.len() || chars[i] != '~' {
        return Ok((0, i));
    }
    let start = i + 1;
    let mut end = start;
    while end < chars.len() && chars[end].is_ascii_digit() {
        end += 1;
    }
    let digits: String = chars[start..end].iter().collect();
    let n: u8 = digits
        .parse()
        .ok()
        .filter(|n| (1..=max).contains(n))
        .ok_or_else(|| QueryError::at(format!("{what} must be 1–{max}"), i))?;
    Ok((n, end))
}

// ---------------------------------------------------------------- parser

struct Parser {
    tokens: Vec<Spanned>,
    pos: usize,
    depth: usize,
    /// Input length in characters, for "expected more input" errors.
    input_len: usize,
    analyzer: Analyzer,
}

impl Parser {
    fn peek(&self) -> Option<&Spanned> {
        self.tokens.get(self.pos)
    }

    fn end_pos(&self) -> usize {
        self.input_len
    }

    fn or_expr(&mut self) -> Result<Node, QueryError> {
        let mut branches = vec![self.and_expr()?];
        while matches!(self.peek().map(|t| &t.tok), Some(Tok::Or)) {
            self.pos += 1;
            branches.push(self.and_expr()?);
        }
        Ok(if branches.len() == 1 {
            branches.pop().expect("one")
        } else {
            Node::Or(branches)
        })
    }

    fn and_expr(&mut self) -> Result<Node, QueryError> {
        let mut parts = vec![self.unary()?];
        loop {
            match self.peek().map(|t| &t.tok) {
                Some(Tok::And) => {
                    self.pos += 1;
                    parts.push(self.unary()?);
                }
                Some(Tok::Or | Tok::RParen) | None => break,
                Some(_) => parts.push(self.unary()?),
            }
        }
        Ok(if parts.len() == 1 {
            parts.pop().expect("one")
        } else {
            Node::And(parts)
        })
    }

    fn unary(&mut self) -> Result<Node, QueryError> {
        if matches!(self.peek().map(|t| &t.tok), Some(Tok::Minus | Tok::Not)) {
            self.pos += 1;
            return Ok(Node::Not(Box::new(self.primary()?)));
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Node, QueryError> {
        let Some(t) = self.tokens.get(self.pos).cloned() else {
            return Err(QueryError::at(
                "expected a word, phrase or '('",
                self.end_pos(),
            ));
        };
        self.pos += 1;
        match t.tok {
            Tok::LParen => {
                self.depth += 1;
                if self.depth > MAX_DEPTH {
                    return Err(QueryError::at("too many nested parentheses", t.pos));
                }
                let inner = self.or_expr()?;
                self.depth -= 1;
                match self.peek() {
                    Some(Spanned {
                        tok: Tok::RParen, ..
                    }) => {
                        self.pos += 1;
                        Ok(inner)
                    }
                    _ => Err(QueryError::at("missing ')'", t.pos)),
                }
            }
            // Quickwit's phrases take no wildcards (its tokenizer would drop
            // them), so refuse them rather than search without them.
            Tok::Phrase { text, .. } if text.split_whitespace().any(has_wildcard) => Err(
                QueryError::at("wildcards (* and ?) don't work inside quotes", t.pos),
            ),
            Tok::Phrase { text, slop } => phrase(tokenize(&text, self.analyzer), slop)
                .map_err(|e| QueryError::at(e.message, t.pos)),
            Tok::Word { text, fuzzy } => word(&text, fuzzy, t.pos, self.analyzer),
            Tok::RParen => Err(QueryError::at("unexpected ')'", t.pos)),
            Tok::And | Tok::Or | Tok::Not | Tok::Minus => Err(QueryError::at(
                "operator is missing a word before or after it",
                t.pos,
            )),
        }
    }
}

fn word(text: &str, fuzzy: u8, pos: usize, analyzer: Analyzer) -> Result<Node, QueryError> {
    if has_ja(text) {
        return ja_word(text, fuzzy, has_wildcard(text), pos, analyzer);
    }
    if has_wildcard(text) {
        return wildcard_word(text, pos, analyzer);
    }
    let tokens = tokenize(text, analyzer);
    match tokens.len() {
        0 => Err(QueryError::at("word has no searchable characters", pos)),
        1 => Ok(Node::Term(Term {
            text: tokens.into_iter().next().expect("one token"),
            fuzzy,
            prefix: false,
            wildcard: false,
        })),
        // "well-known" → the phrase "well known"
        _ if fuzzy == 0 => Ok(Node::Phrase {
            terms: tokens,
            slop: 0,
        }),
        _ => Err(QueryError::at("fuzzy needs a single word", pos)),
    }
}

/// The part of a word that can hold wildcards, without the punctuation
/// around it: a `?` that ends a word ("president?") is a question mark, as
/// it was before wildcards (#124).
fn wildcard_span(word: &str) -> &str {
    word.trim_start_matches(|c: char| !c.is_alphanumeric() && !matches!(c, '*' | '?'))
        .trim_end_matches(|c: char| !c.is_alphanumeric() && c != '*')
}

/// Whether a word has a `*`, or a `?` that is a wildcard.
fn has_wildcard(word: &str) -> bool {
    wildcard_span(word).contains(['*', '?'])
}

/// A word with wildcards (#124): `?` matches exactly one character and `*`
/// any run of them, as in Quickwit's wildcard queries. The letters between
/// the wildcards are folded like any word. At least [`MIN_PREFIX_CHARS`]
/// letters must come before the first wildcard: the engine then walks only
/// the index's words that start with them, where a leading wildcard would
/// walk all of them. A run of wildcards is written as its `?`s, then one `*`
/// if it has any, so `wash*?ton` and `wash?**ton` are one query (and one
/// cache key). A word whose only wildcard is a trailing `*` is a prefix.
fn wildcard_word(text: &str, pos: usize, analyzer: Analyzer) -> Result<Node, QueryError> {
    let chars: Vec<char> = wildcard_span(text).chars().collect();
    let mut pattern = String::new();
    // Folded letters before the first wildcard.
    let mut lead: Option<usize> = None;
    let mut i = 0;
    while i < chars.len() {
        let start = i;
        let wild = matches!(chars[i], '*' | '?');
        while i < chars.len() && matches!(chars[i], '*' | '?') == wild {
            i += 1;
        }
        let run = &chars[start..i];
        if wild {
            lead.get_or_insert(pattern.chars().count());
            pattern.extend(run.iter().filter(|&&c| c == '?'));
            if run.contains(&'*') {
                pattern.push('*');
            }
        } else {
            // A letter can fold to more than letters (`½` is `1⁄2` with
            // `Analyzer::V1`).
            let folded = fold(&run.iter().collect::<String>(), analyzer);
            if !run.iter().all(|c| c.is_alphanumeric())
                || !folded.chars().all(char::is_alphanumeric)
            {
                return Err(QueryError::at(
                    "wildcards (* and ?) need a single word",
                    pos,
                ));
            }
            pattern.push_str(&folded);
        }
    }
    // No query word is longer than MAX_TOKEN_CHARS, and the fields our code
    // writes drop longer ones; `text` keeps OCR runs up to 254 bytes (#286),
    // but no pattern needs to be longer to search them.
    if pattern.chars().filter(|&c| c != '*').count() > MAX_TOKEN_CHARS {
        return Err(QueryError::at(
            format!("a word with * or ? can't be longer than {MAX_TOKEN_CHARS} letters"),
            pos,
        ));
    }
    let lead = lead.unwrap_or_default();
    let stem = pattern
        .strip_suffix('*')
        .filter(|s| lead > 0 && !s.contains(['*', '?']));
    if let Some(stem) = stem {
        if lead < MIN_PREFIX_CHARS {
            return Err(QueryError::at(
                format!("prefix searches need at least {MIN_PREFIX_CHARS} letters"),
                pos,
            ));
        }
        return Ok(Node::Term(Term {
            text: stem.to_owned(),
            fuzzy: 0,
            prefix: true,
            wildcard: false,
        }));
    }
    if lead < MIN_PREFIX_CHARS {
        return Err(QueryError::at(
            format!("wildcards must follow at least {MIN_PREFIX_CHARS} letters"),
            pos,
        ));
    }
    Ok(Node::Term(Term {
        text: pattern,
        fuzzy: 0,
        prefix: false,
        wildcard: true,
    }))
}

/// Whether `token` matches a wildcard term's pattern as a whole (#124): `?`
/// is exactly one character and `*` any run of them, as Quickwit matches a
/// wildcard query against the index's words.
pub fn wildcard_matches(pattern: &str, token: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = token.chars().collect();
    let (mut i, mut j) = (0, 0);
    // The last `*` seen and where in the token it started: on a mismatch,
    // it takes one more character and matching resumes after it.
    let mut star: Option<(usize, usize)> = None;
    while j < t.len() {
        if i < p.len() && p[i] == '*' {
            star = Some((i, j));
            i += 1;
        } else if i < p.len() && (p[i] == '?' || p[i] == t[j]) {
            i += 1;
            j += 1;
        } else if let Some((si, sj)) = star {
            star = Some((si, sj + 1));
            i = si + 1;
            j = sj + 1;
        } else {
            return false;
        }
    }
    p[i..].iter().all(|&c| c == '*')
}

/// A word with Japanese in it: each run between punctuation (、。「」 …) is a
/// phrase of its characters, and the runs are ANDed. Wildcard and fuzzy
/// matching work on whole words, which Japanese text doesn't mark, so they're
/// refused.
fn ja_word(
    text: &str,
    fuzzy: u8,
    wildcard: bool,
    pos: usize,
    analyzer: Analyzer,
) -> Result<Node, QueryError> {
    if fuzzy > 0 || wildcard {
        return Err(QueryError::at(
            "wildcard (* and ?) and fuzzy (~) searches aren't available for Japanese",
            pos,
        ));
    }
    // Voicing marks stay in the run, so a decomposed か+゙ composes to が.
    let runs: Vec<Node> = text
        .split(|c: char| !c.is_alphanumeric() && !ja::is_voicing_mark(c))
        .map(|run| tokenize(run, analyzer))
        .filter(|t| !t.is_empty())
        .map(|t| phrase(t, 0))
        .collect::<Result<_, _>>()
        .map_err(|e| QueryError::at(e.message, pos))?;
    match runs.len() {
        0 => Err(QueryError::at("word has no searchable characters", pos)),
        1 => Ok(runs.into_iter().next().expect("one run")),
        _ => Ok(Node::And(runs)),
    }
}

/// Whether a term is Japanese (one Japanese character, as the tokenizer makes them).
fn is_ja_term(t: &str) -> bool {
    t.chars().count() == 1 && t.chars().all(ja::is_ja)
}

/// The lengths of the contiguous Japanese runs in a phrase's terms.
fn ja_runs(terms: &[String]) -> Vec<usize> {
    let mut runs = Vec::new();
    let mut len = 0;
    for t in terms {
        if is_ja_term(t) {
            len += 1;
        } else if len > 0 {
            runs.push(len);
            len = 0;
        }
    }
    if len > 0 {
        runs.push(len);
    }
    runs
}

/// Whether the query has a Japanese word that isn't excluded: it then searches
/// the Japanese pages (#139).
pub fn is_japanese(node: &Node) -> bool {
    match node {
        Node::Term(t) => is_ja_term(&t.text),
        Node::Phrase { terms, .. } => terms.iter().any(|t| is_ja_term(t)),
        Node::And(c) | Node::Or(c) => c.iter().any(is_japanese),
        Node::Not(_) => false,
    }
}

fn has_any_ja(node: &Node) -> bool {
    match node {
        Node::Term(t) => is_ja_term(&t.text),
        Node::Phrase { terms, .. } => terms.iter().any(|t| is_ja_term(t)),
        Node::And(c) | Node::Or(c) => c.iter().any(has_any_ja),
        Node::Not(n) => has_any_ja(n),
    }
}

// ---------------------------------------------------------------- normalize / validate

/// Flatten nested AND/OR, drop double negation, sort children for a stable canonical form.
pub fn normalize(node: Node) -> Node {
    match node {
        Node::And(children) => flatten(children, true),
        Node::Or(children) => flatten(children, false),
        Node::Not(inner) => match normalize(*inner) {
            Node::Not(x) => *x,
            x => Node::Not(Box::new(x)),
        },
        leaf => leaf,
    }
}

fn flatten(children: Vec<Node>, is_and: bool) -> Node {
    let mut out: Vec<Node> = Vec::new();
    for c in children.into_iter().map(normalize) {
        match (c, is_and) {
            (Node::And(inner), true) | (Node::Or(inner), false) => out.extend(inner),
            (c, _) => out.push(c),
        }
    }
    out.sort_by_key(|n| n.to_string());
    out.dedup();
    match out.len() {
        1 => out.pop().expect("one"),
        _ if is_and => Node::And(out),
        _ => Node::Or(out),
    }
}

fn apply_fuzzy(node: Node, fuzzy: u8) -> Node {
    if fuzzy == 0 {
        return node;
    }
    match node {
        Node::Term(t) if !t.prefix && !t.wildcard && t.fuzzy == 0 && !is_ja_term(&t.text) => {
            Node::Term(Term { fuzzy, ..t })
        }
        Node::And(c) => Node::And(c.into_iter().map(|n| apply_fuzzy(n, fuzzy)).collect()),
        Node::Or(c) => Node::Or(c.into_iter().map(|n| apply_fuzzy(n, fuzzy)).collect()),
        Node::Not(n) => Node::Not(Box::new(apply_fuzzy(*n, fuzzy))),
        other => other,
    }
}

fn validate(node: &Node) -> Result<(), QueryError> {
    let terms = count_terms(node);
    if terms > MAX_TERMS {
        return Err(QueryError::new(format!(
            "query has {terms} words; the limit is {MAX_TERMS}"
        )));
    }
    check(node)?;
    if has_any_ja(node) && !is_japanese(node) {
        return Err(QueryError::new(
            "a Japanese word can't only be excluded; search for one too",
        ));
    }
    if !has_positive(node) {
        return Err(QueryError::new(
            "query needs at least one word that is not excluded",
        ));
    }
    Ok(())
}

fn check(node: &Node) -> Result<(), QueryError> {
    match node {
        Node::Or(c) if c.len() > MAX_OR_BRANCHES => Err(QueryError::new(format!(
            "OR has {} alternatives; the limit is {MAX_OR_BRANCHES}",
            c.len()
        ))),
        Node::Or(c) => {
            if c.iter().any(|n| matches!(n, Node::Not(_))) {
                return Err(QueryError::new(
                    "an OR alternative can't be only an exclusion",
                ));
            }
            c.iter().try_for_each(check)
        }
        Node::And(c) => c.iter().try_for_each(check),
        Node::Not(n) => check(n),
        Node::Phrase { terms, .. } => {
            let longest = ja_runs(terms).into_iter().max().unwrap_or(0);
            if longest > MAX_JA_RUN_CHARS {
                return Err(QueryError::new(format!(
                    "a Japanese phrase has {longest} characters; the limit is {MAX_JA_RUN_CHARS}"
                )));
            }
            Ok(())
        }
        Node::Term(_) => Ok(()),
    }
}

fn count_terms(node: &Node) -> usize {
    match node {
        Node::Term(_) => 1,
        // The characters of a Japanese run are one word between them.
        Node::Phrase { terms, .. } => {
            let latin = terms.iter().filter(|t| !is_ja_term(t)).count();
            latin + ja_runs(terms).len()
        }
        Node::And(c) | Node::Or(c) => c.iter().map(count_terms).sum(),
        Node::Not(n) => count_terms(n),
    }
}

fn has_positive(node: &Node) -> bool {
    match node {
        Node::Term(_) | Node::Phrase { .. } => true,
        Node::Not(_) => false,
        Node::And(c) | Node::Or(c) => c.iter().any(has_positive),
    }
}

/// Words to highlight in the LoC viewer (positive terms only).
pub fn highlight_terms(node: &Node) -> Vec<String> {
    let mut out = Vec::new();
    collect_positive(node, &mut out);
    out.dedup();
    out
}

fn collect_positive(node: &Node, out: &mut Vec<String>) {
    match node {
        Node::Term(t) => out.push(t.text.clone()),
        Node::Phrase { terms, .. } => out.extend(terms.iter().cloned()),
        Node::And(c) | Node::Or(c) => c.iter().for_each(|n| collect_positive(n, out)),
        Node::Not(_) => {}
    }
}

impl fmt::Display for Node {
    /// Canonical query-language rendering; parsing it yields the same AST.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Node::Term(t) => {
                write!(f, "{}", t.text)?;
                if t.prefix {
                    write!(f, "*")
                } else if t.fuzzy > 0 {
                    write!(f, "~{}", t.fuzzy)
                } else {
                    Ok(())
                }
            }
            Node::Phrase { terms, slop } => {
                // Japanese characters run together, as they're written (and reparse the same).
                f.write_str("\"")?;
                for (i, t) in terms.iter().enumerate() {
                    if i > 0 && !(is_ja_term(&terms[i - 1]) && is_ja_term(t)) {
                        f.write_str(" ")?;
                    }
                    f.write_str(t)?;
                }
                f.write_str("\"")?;
                if *slop > 0 {
                    write!(f, "~{slop}")?;
                }
                Ok(())
            }
            Node::And(c) => join(f, c, " AND "),
            Node::Or(c) => join(f, c, " OR "),
            Node::Not(n) => match n.as_ref() {
                Node::And(_) | Node::Or(_) => write!(f, "-({n})"),
                _ => write!(f, "-{n}"),
            },
        }
    }
}

fn join(f: &mut fmt::Formatter<'_>, children: &[Node], sep: &str) -> fmt::Result {
    for (i, c) in children.iter().enumerate() {
        if i > 0 {
            f.write_str(sep)?;
        }
        match c {
            Node::And(_) | Node::Or(_) => write!(f, "({c})")?,
            _ => write!(f, "{c}")?,
        }
    }
    Ok(())
}

/// Fold a user string the same way terms are folded with `analyzer` (for
/// display/highlighting).
pub fn fold_text(s: &str, analyzer: Analyzer) -> String {
    fold(s, analyzer)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`super::build`] with the latest analyzer.
    fn build(q: &str, mode: Option<Mode>, near: u8, fuzzy: u8) -> Result<Node, QueryError> {
        super::build(q, mode, near, fuzzy, Analyzer::LATEST)
    }

    fn term(t: &str) -> Node {
        exact(t.to_owned())
    }

    fn ja(s: &str) -> Node {
        Node::Phrase {
            terms: s.chars().map(|c| c.to_string()).collect(),
            slop: 0,
        }
    }

    #[test]
    fn japanese_words_are_character_phrases() {
        let n = parse("真珠湾").unwrap();
        assert_eq!(n, ja("真珠湾"));
        assert_eq!(n.to_string(), "\"真珠湾\"");
        assert_eq!(parse(&n.to_string()).unwrap(), n);
        assert!(is_japanese(&n));
        // Old forms fold, so both spellings are one query (and one cache key).
        assert_eq!(parse("戰爭").unwrap(), parse("戦争").unwrap());
        // Decomposed (IME) and halfwidth input parse like the composed forms.
        assert_eq!(parse("か\u{3099}す").unwrap(), parse("がす").unwrap());
        assert_eq!(parse("ｶﾞｽ").unwrap(), parse("ガス").unwrap());
        // A single character is a term.
        assert_eq!(parse("年").unwrap(), term("年"));
    }

    #[test]
    fn japanese_punctuation_splits_runs() {
        assert_eq!(
            parse("東京、大阪").unwrap(),
            Node::And(vec![ja("大阪"), ja("東京")])
        );
        assert_eq!(parse("「日本」").unwrap(), ja("日本"));
    }

    #[test]
    fn mixed_japanese_and_latin() {
        let n = parse("gold 東京").unwrap();
        assert_eq!(n, Node::And(vec![ja("東京"), term("gold")]));
        assert!(is_japanese(&n));
        assert_eq!(
            parse("Rocky新報").unwrap(),
            Node::Phrase {
                terms: vec!["rocky".into(), "新".into(), "報".into()],
                slop: 0
            }
        );
        assert!(!is_japanese(&parse("gold silver").unwrap()));
    }

    #[test]
    fn japanese_in_each_mode() {
        let any = build("東京 大阪 gold", Some(Mode::Any), 0, 0).unwrap();
        assert_eq!(any, Node::Or(vec![ja("大阪"), ja("東京"), term("gold")]));
        let near = build("真珠湾 攻撃", Some(Mode::Near), 3, 0).unwrap();
        assert_eq!(
            near,
            Node::Phrase {
                terms: "真珠湾攻撃".chars().map(String::from).collect(),
                slop: 3
            }
        );
        let phrase = build("真珠湾 攻撃", Some(Mode::Phrase), 0, 0).unwrap();
        assert_eq!(phrase, ja("真珠湾攻撃"));
        // fuzzy applies to Latin words only.
        let fz = build("gold 東京", Some(Mode::All), 0, 1).unwrap();
        assert_eq!(
            fz,
            Node::And(vec![
                ja("東京"),
                Node::Term(Term {
                    text: "gold".into(),
                    fuzzy: 1,
                    prefix: false,
                    wildcard: false
                })
            ])
        );
    }

    #[test]
    fn japanese_limits() {
        // A run is one word for the word limit...
        let long = "日本".repeat(8);
        assert!(parse(&long).is_ok());
        // ...up to MAX_JA_RUN_CHARS characters.
        assert!(parse(&"日".repeat(MAX_JA_RUN_CHARS + 1))
            .unwrap_err()
            .message
            .contains("Japanese phrase"));
        // The limit is per run: two 20-character runs either side of a Latin word are fine...
        let two_runs = format!("\"{} gold {}\"", "日".repeat(20), "月".repeat(20));
        assert!(parse(&two_runs).is_ok());
        // ...and each run is a term: 日 gold 月 is three.
        assert_eq!(count_terms(&parse("\"日 gold 月\"").unwrap()), 3);
        assert!(parse("東京*")
            .unwrap_err()
            .message
            .contains("aren't available for Japanese"));
        assert!(parse("東京~1")
            .unwrap_err()
            .message
            .contains("aren't available for Japanese"));
        assert!(parse("gold -東京")
            .unwrap_err()
            .message
            .contains("can't only be excluded"));
        assert!(parse("東京 -大阪").is_ok());
    }

    #[test]
    fn parses_phrases_terms_and_operators() {
        let n = parse(r#""Cross of Gold" silver -bryan"#).unwrap();
        assert_eq!(n.to_string(), r#""cross of gold" AND -bryan AND silver"#);
        let n = parse("scalawag OR carpetbagger").unwrap();
        assert_eq!(n, Node::Or(vec![term("carpetbagger"), term("scalawag")]));
        let n = parse(r#"("yellow fever" OR "yellow jack") AND memphis"#).unwrap();
        assert_eq!(
            n.to_string(),
            r#"("yellow fever" OR "yellow jack") AND memphis"#
        );
    }

    #[test]
    fn canonical_form_round_trips_and_is_order_independent() {
        let a = parse("silver gold").unwrap();
        let b = parse("gold AND silver").unwrap();
        assert_eq!(a, b);
        let again = parse(&a.to_string()).unwrap();
        assert_eq!(again, a);
        let n = parse(r#"NOT (a1 OR b1) keep "x y"~3 influ* colr~1"#).unwrap();
        assert_eq!(parse(&n.to_string()).unwrap(), n);
    }

    fn assert_round_trips(q: &str, a: Analyzer) {
        let n = parse_with(q, a).unwrap_or_else(|e| panic!("{q:?}: {e}"));
        let canonical = n.to_string();
        let again = parse_with(&canonical, a);
        assert_eq!(again.as_ref(), Ok(&n), "{q:?} → {canonical:?} ({a:?})");
        assert_eq!(again.unwrap().to_string(), canonical, "{q:?} ({a:?})");
    }

    #[test]
    fn fractions_are_one_word_and_round_trip() {
        // `½` is one word, as the index has it, not the phrase "1 2" (#168).
        let v2 = Analyzer::V2;
        assert_eq!(parse_with("½", v2).unwrap(), term("½"));
        assert_eq!(parse_with("3½", v2).unwrap(), term("3½"));
        assert_eq!(parse_with("½", v2).unwrap().to_string(), "½");
        // A slash, fraction slash or division slash separates, as in the
        // index, with either version.
        let one_two = Node::Phrase {
            terms: vec!["1".into(), "2".into()],
            slop: 0,
        };
        for a in Analyzer::ALL {
            for q in ["1/2", "1⁄2", "1∕2", r#""1 2""#] {
                assert_eq!(parse_with(q, a).unwrap(), one_two, "{q} {a:?}");
            }
        }
        for f in crate::text::vulgar_fractions() {
            for q in [
                format!("{f}"),
                format!("3{f}"),
                format!("wheat {f} -corn"),
                format!(r#""wheat {f} higher at 61{f}""#),
                format!(r#""{f} cent"~3"#),
                format!("18461{f}*"),
                format!("({f} OR 1/2) cent"),
            ] {
                assert_round_trips(&q, v2);
            }
            for mode in [Mode::Phrase, Mode::All, Mode::Any, Mode::Near] {
                let n = super::build(&format!("wheat {f} 1/2"), Some(mode), 2, 0, v2).unwrap();
                assert_eq!(parse_with(&n.to_string(), v2).unwrap(), n, "{f} {mode:?}");
            }
        }
    }

    #[test]
    fn version_1_parses_fractions_as_before() {
        // On an index built with version 1, a query folds `½` to `1⁄2`, as
        // it did before #168: the term the index's pairs have. Its
        // canonical form reparses as the phrase "1 2" (the bug version 2
        // fixes), so `½` doesn't round-trip there.
        let v1 = Analyzer::V1;
        assert_eq!(parse_with("½", v1).unwrap(), term("1\u{2044}2"));
        assert_eq!(parse_with("3½", v1).unwrap(), term("31\u{2044}2"));
        assert_eq!(
            parse_with(r#""wheat ½ higher""#, v1).unwrap(),
            Node::Phrase {
                terms: vec!["wheat".into(), "1\u{2044}2".into(), "higher".into()],
                slop: 0,
            }
        );
        assert_ne!(parse_with("½", v1), parse_with("½", Analyzer::V2));
        // Wildcards after a fraction need a single word, which `1⁄2` isn't.
        assert!(parse_with("18461½*", v1).is_err());
        assert!(parse_with("18461½*", Analyzer::V2).is_ok());
    }

    #[test]
    fn every_alphanumeric_word_round_trips() {
        for a in Analyzer::ALL {
            for c in (0..=0x10FFFF).filter_map(char::from_u32) {
                if !c.is_alphanumeric() || parse_with(&c.to_string(), a).is_err() {
                    continue;
                }
                // Version 1 decomposes some characters into separators
                // (`½` is `1⁄2`), which reparse as several words (#168).
                let folded = fold(&c.to_string(), a);
                if !folded.chars().all(char::is_alphanumeric) {
                    assert_eq!(a, Analyzer::V1, "{c:?} folds to {folded:?}");
                    continue;
                }
                assert_round_trips(&c.to_string(), a);
                // Version 1 folds a character that isn't Japanese to a
                // Japanese one (`㊀` is `一`): inside a Latin word that is
                // the term `a一`, which reparses as `a 一` (#241). Version 2
                // keeps it, as the index does.
                if !ja::is_ja(c) && has_ja(&folded) {
                    assert_eq!(a, Analyzer::V1, "{c:?} folds to {folded:?}");
                    continue;
                }
                assert_round_trips(&format!("a{c}"), a);
                assert_round_trips(&format!("\"a{c} b\""), a);
            }
        }
    }

    #[test]
    fn characters_that_fold_to_japanese_round_trip_inside_latin_words() {
        // `㊀` and the other characters that aren't Japanese but fold to a
        // Japanese one stay themselves with version 2, as Quickwit's
        // `usnm_text` has them: `a㊀` is one word, not the Latin `a` and the
        // Japanese `一`, and it searches the main indexes (#241).
        let v2 = Analyzer::V2;
        assert_eq!(parse_with("a㊀", v2).unwrap(), term("a㊀"));
        assert_eq!(parse_with("A㊀", v2).unwrap(), term("a㊀"));
        assert_eq!(
            parse_with(r#""a㊀ b""#, v2).unwrap(),
            Node::Phrase {
                terms: vec!["a㊀".into(), "b".into()],
                slop: 0,
            }
        );
        assert_eq!(parse_with("㊀", v2).unwrap(), term("㊀"));
        assert!(!is_japanese(&parse_with("a㊀ ㊀", v2).unwrap()));
        // `a一` is still the Latin `a` and the Japanese `一`.
        assert_eq!(
            parse_with("a一", v2).unwrap(),
            Node::Phrase {
                terms: vec!["a".into(), "一".into()],
                slop: 0,
            }
        );
        for c in crate::text::folding_to_japanese() {
            for q in [
                format!("{c}"),
                format!("a{c}"),
                format!("A{c}"),
                format!(r#""a{c} b""#),
                format!(r#""a{c} b"~3"#),
                format!("{c}a{c} -b{c}"),
                format!("(a{c} OR {c}) 東京"),
                format!(r#""東京 a{c}""#),
                format!("a{c}~1"),
                format!("abcde{c}*"),
                format!("abcde{c}?x"),
            ] {
                assert_round_trips(&q, v2);
            }
            for mode in [Mode::Phrase, Mode::All, Mode::Any, Mode::Near] {
                let n = super::build(&format!("a{c} b {c}"), Some(mode), 2, 1, v2).unwrap();
                assert_eq!(parse_with(&n.to_string(), v2).unwrap(), n, "{c} {mode:?}");
            }
        }
    }

    #[test]
    fn version_1_folds_characters_to_japanese_as_before() {
        // On an index built with version 1, `㊀` folds to `一`, as it did
        // before #241: alone it is the Japanese `一`, which round-trips, and
        // inside a Latin word the term `a一`, which reparses as the Latin
        // `a` and the Japanese `一`.
        let v1 = Analyzer::V1;
        assert_eq!(parse_with("㊀", v1).unwrap(), term("一"));
        assert!(is_japanese(&parse_with("㊀", v1).unwrap()));
        assert_eq!(parse_with("a㊀", v1).unwrap(), term("a一"));
        assert_eq!(
            parse_with("a一", v1).unwrap(),
            Node::Phrase {
                terms: vec!["a".into(), "一".into()],
                slop: 0,
            }
        );
        assert_ne!(parse_with("a㊀", v1), parse_with("a㊀", Analyzer::V2));
    }

    #[test]
    fn supports_slop_fuzzy_and_prefix() {
        assert_eq!(
            parse(r#""gold silver"~5"#).unwrap(),
            Node::Phrase {
                terms: vec!["gold".into(), "silver".into()],
                slop: 5
            }
        );
        assert_eq!(
            parse("miscegenaton~2").unwrap(),
            Node::Term(Term {
                text: "miscegenaton".into(),
                fuzzy: 2,
                prefix: false,
                wildcard: false
            })
        );
        assert_eq!(
            parse("influ*").unwrap(),
            Node::Term(Term {
                text: "influ".into(),
                fuzzy: 0,
                prefix: true,
                wildcard: false
            })
        );
    }

    fn wild(p: &str) -> Node {
        Node::Term(Term {
            text: p.into(),
            fuzzy: 0,
            prefix: false,
            wildcard: true,
        })
    }

    #[test]
    fn wildcards_inside_words() {
        assert_eq!(parse("presi?ent").unwrap(), wild("presi?ent"));
        // `*` can stand for nothing, so a 6-letter word can hold one.
        assert_eq!(parse("silve*r").unwrap(), wild("silve*r"));
        assert_eq!(parse("WashI*TON").unwrap(), wild("washi*ton"));
        assert_eq!(parse("cafet?ría").unwrap(), wild("cafet?ria"));
        // A trailing `*` alone is still a prefix; with a `?` it's a pattern.
        assert_eq!(parse("influ*").unwrap().to_string(), "influ*");
        assert!(matches!(parse("influ*").unwrap(), Node::Term(t) if t.prefix && !t.wildcard));
        assert_eq!(parse("influ?nz*").unwrap(), wild("influ?nz*"));
        // Runs of wildcards have one spelling, so one cache key.
        assert_eq!(parse("washi*?ton").unwrap(), wild("washi?*ton"));
        assert_eq!(parse("washi?**ton").unwrap(), wild("washi?*ton"));
        // A `?` that ends a word is punctuation, as before.
        assert_eq!(parse("president?").unwrap(), term("president"));
        assert_eq!(parse("pres?").unwrap(), term("pres"));
        assert_eq!(parse("presi?ent?!").unwrap(), wild("presi?ent"));
        // The canonical form reparses to the same AST.
        for q in [
            "presi?ent",
            "washi*ton OR lincoln",
            "-colou?r gold",
            "abcde??f",
        ] {
            let n = parse(q).unwrap();
            assert_eq!(parse(&n.to_string()).unwrap(), n, "{q}");
        }
        // `fuzzy` leaves wildcard terms alone; modes need plain words.
        assert_eq!(
            build("presi?ent gold", None, 0, 1).unwrap().to_string(),
            "gold~1 AND presi?ent"
        );
        assert!(build("presi?ent", Some(Mode::Phrase), 0, 0).is_err());
        assert!(is_plain("who was president?"));
        assert!(!is_plain("presi?ent"));
        assert!(!is_plain("pres?dent"));
    }

    #[test]
    fn wildcards_need_five_leading_letters_and_a_single_word() {
        let err = |q: &str| parse(q).unwrap_err();
        // The same minimum as a prefix (#247), for the same reason: the
        // engine walks every indexed word that starts with those letters.
        let five = format!("wildcards must follow at least {MIN_PREFIX_CHARS} letters");
        for q in [
            "*gold",
            "?old",
            "pr?sident",
            "pres?dent",
            "wash*ton",
            "go*d",
            "*",
        ] {
            assert_eq!(err(q).message, five, "{q}");
        }
        // A word whose only wildcard is a trailing `*` is a prefix.
        assert!(err("ab*?").message.contains("prefix searches"));
        // Letters are counted after folding, like a prefix's: `Æ` is `ae`.
        assert_eq!(parse("Æsop?s").unwrap(), wild("aesop?s"));
        assert_eq!(err("ñoño?s").message, five);
        assert_eq!(parse("ñoños?a").unwrap(), wild("nonos?a"));
        assert_eq!(err("gold pres?dent").position, Some(5));
        assert_eq!(err("gold (silver OR wash*ton)").position, Some(16));
        assert!(err("o'bri?n").message.contains("single word"));
        // No indexed word is longer than MAX_TOKEN_CHARS; `?` is one
        // character and `*` can be none.
        let long = |n: usize| "x".repeat(n);
        assert!(err(&format!("{}*", long(41)))
            .message
            .contains("longer than 40"));
        assert_eq!(
            parse(&format!("{}*", long(40))).unwrap().to_string(),
            format!("{}*", long(40))
        );
        assert!(err(&format!("abcde?{}", long(35)))
            .message
            .contains("longer than 40"));
        assert_eq!(
            parse(&format!("abcde?{}", long(34))).unwrap(),
            wild(&format!("abcde?{}", long(34)))
        );
        assert_eq!(
            parse(&format!("abcde*{}*", long(35))).unwrap(),
            wild(&format!("abcde*{}*", long(35)))
        );
        assert!(err("presi?ent~1").message.contains("not both"));
        assert!(err("gold~1?").message.contains("not both"));
        assert!(err(r#""presi?ent lincoln""#)
            .message
            .contains("inside quotes"));
        assert!(err(r#""cross of gol*""#).message.contains("inside quotes"));
        // A question mark ending a word in quotes is still punctuation.
        assert!(parse(r#""who is he?""#).is_ok());
        assert!(err("東?京")
            .message
            .contains("aren't available for Japanese"));
    }

    #[test]
    fn wildcard_patterns_match_whole_words() {
        let m = wildcard_matches;
        assert!(m("pres?dent", "president"));
        assert!(!m("pres?dent", "presdent"));
        assert!(!m("pres?dent", "presidents"));
        assert!(m("wash*ton", "washington"));
        assert!(m("wash*ton", "washton"));
        assert!(!m("wash*ton", "washingtons"));
        assert!(m("wash*ton*", "washingtons"));
        assert!(m("a*b*c", "axxbyybc"));
        assert!(!m("a*b*c", "axxbyyb"));
        assert!(m("abc?*", "abcd"));
        assert!(!m("abc?*", "abc"));
        assert!(m("ab??", "abçd"));
    }

    #[test]
    fn prefixes_need_five_letters() {
        assert_eq!(MIN_PREFIX_CHARS, 5);
        for q in ["influ*", "Influ*", "influenza*", "gold silve*"] {
            assert!(parse(q).is_ok(), "{q}");
        }
        // Counted after folding, so an accented letter is one letter.
        assert!(parse("ñoños*").is_ok());
        for (q, pos) in [
            ("inf*", 0),
            ("infl*", 0),
            ("gold con*", 5),
            ("gold (silv* OR bryan)", 6),
            ("ñoño*", 0),
        ] {
            let e = parse(q).unwrap_err();
            assert_eq!(e.message, "prefix searches need at least 5 letters", "{q}");
            assert_eq!(e.position, Some(pos), "{q}");
        }
        let e = parse("gold *silver").unwrap_err();
        assert_eq!(e.message, "wildcards must follow at least 5 letters");
        assert_eq!(e.position, Some(5));
        // The canonical form keeps the prefix and parses again.
        let n = parse("Telegr* -wireless").unwrap();
        assert_eq!(n.to_string(), "-wireless AND telegr*");
        assert_eq!(parse(&n.to_string()).unwrap(), n);
    }

    #[test]
    fn hyphenated_word_becomes_phrase() {
        assert_eq!(
            parse("well-known").unwrap(),
            Node::Phrase {
                terms: vec!["well".into(), "known".into()],
                slop: 0
            }
        );
    }

    #[test]
    fn rejects_unsafe_or_invalid_queries() {
        let err = |q: &str| parse(q).unwrap_err();
        assert_eq!(err("text:gold").position, Some(4));
        assert!(err("*gold").message.contains("wildcard"));
        assert!(err("go*").message.contains("prefix"));
        assert!(err("gold~3").message.contains("fuzzy"));
        assert!(err(r#""a b"~21"#).message.contains("slop"));
        assert!(err(r#""cross of gold"#).message.contains("unterminated"));
        assert!(err("(gold").message.contains("')'"));
        assert!(err("-gold").message.contains("not excluded"));
        assert!(err("a1 OR b1 OR c1 OR d1 OR e1")
            .message
            .contains("OR has 5"));
        assert!(err("gold OR -silver").message.contains("only an exclusion"));
        let trailing = err("gold AND");
        assert!(trailing.message.contains("expected"));
        assert_eq!(trailing.position, Some(8));
        assert!(err("a1 b1 c1 d1 e1 f1 g1 h1 i1 j1 k1 l1 m1")
            .message
            .contains("limit is 12"));
        assert!(err("gold~1*").message.contains("not both"));
    }

    #[test]
    fn modes_combine_plain_word_lists() {
        assert_eq!(
            build("Cross of Gold", Some(Mode::Phrase), 0, 0)
                .unwrap()
                .to_string(),
            r#""cross of gold""#
        );
        assert_eq!(
            build("gold silver", Some(Mode::Any), 0, 0)
                .unwrap()
                .to_string(),
            "gold OR silver"
        );
        assert_eq!(
            build("gold silver", Some(Mode::Near), 5, 0)
                .unwrap()
                .to_string(),
            r#""gold silver"~5"#
        );
        assert_eq!(
            build("gold silver", None, 0, 1).unwrap().to_string(),
            "gold~1 AND silver~1"
        );
        assert!(build(r#""a b" c"#, Some(Mode::Any), 0, 0).is_err());
        assert!(build("gold silver", Some(Mode::Near), 0, 0).is_err());
        assert!(build("   ", None, 0, 0).is_err());
    }

    #[test]
    fn highlight_terms_skip_exclusions() {
        let n = parse(r#""cross of gold" -bryan"#).unwrap();
        assert_eq!(highlight_terms(&n), vec!["cross", "of", "gold"]);
    }
}
