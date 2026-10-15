//! Sequence alignment with a backtrace, and the pairwise metrics between two
//! readings of one page.
//!
//! [`align`] gives the edit script between two sequences (unit-cost insert,
//! delete and substitute, the same distance as `distance::levenshtein`) in
//! linear space: Hirschberg's divide and conquer over the last-row
//! computation, with a full table only below [`SMALL`] cells. Ties are broken
//! in a fixed order (diagonal, then deletion, then insertion), so the script
//! is a function of the inputs alone.
//!
//! [`pair`] compares two texts after the gold set's normalisation
//! (`usnm_ocr_survey::text::normalize`): character edits by the bit-parallel
//! distance (no backtrace needed for a count), word edits over
//! whitespace-split words, and a word-bag Dice over lower-case alphanumeric
//! tokens.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use usnm_ocr_survey::{distance, text};

/// Cells of the full table below which the backtrace is taken from it
/// directly (the recursion's base case).
pub const SMALL: usize = 1 << 18;

/// One step of an edit script from `a` to `b`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    /// `a[i] == b[j]`.
    Match,
    /// `a[i]` replaced by `b[j]`.
    Sub,
    /// `b[j]` has no counterpart in `a`.
    Ins,
    /// `a[i]` has no counterpart in `b`.
    Del,
}

/// The edit script from `a` to `b`, with the fewest non-match steps.
pub fn align<T: PartialEq>(a: &[T], b: &[T]) -> Vec<Op> {
    let mut out = Vec::with_capacity(a.len().max(b.len()));
    rec(a, b, &mut out);
    out
}

/// The non-match steps of a script: the edit distance.
pub fn edits(ops: &[Op]) -> usize {
    ops.iter().filter(|o| **o != Op::Match).count()
}

fn rec<T: PartialEq>(a: &[T], b: &[T], out: &mut Vec<Op>) {
    if a.is_empty() {
        out.extend(std::iter::repeat_n(Op::Ins, b.len()));
        return;
    }
    if b.is_empty() {
        out.extend(std::iter::repeat_n(Op::Del, a.len()));
        return;
    }
    if a.len() == 1 || b.len() == 1 || a.len().saturating_mul(b.len()) <= SMALL {
        full(a, b, out);
        return;
    }
    let mid = a.len() / 2;
    let split = {
        let left = last_row(&a[..mid], b, false);
        let right = last_row(&a[mid..], b, true);
        let n = b.len();
        let mut best = (u32::MAX, 0);
        for (j, l) in left.iter().enumerate() {
            let c = l + right[n - j];
            if c < best.0 {
                best = (c, j);
            }
        }
        best.1
    };
    rec(&a[..mid], &b[..split], out);
    rec(&a[mid..], &b[split..], out);
}

/// The edit distance between `a` and every prefix of `b` (both reversed
/// when `rev`), as the last row of the table.
fn last_row<T: PartialEq>(a: &[T], b: &[T], rev: bool) -> Vec<u32> {
    fn at<T>(s: &[T], i: usize, rev: bool) -> &T {
        if rev {
            &s[s.len() - 1 - i]
        } else {
            &s[i]
        }
    }
    let n = b.len();
    let mut prev: Vec<u32> = (0..=n as u32).collect();
    let mut cur = vec![0u32; n + 1];
    for i in 0..a.len() {
        cur[0] = i as u32 + 1;
        let x = at(a, i, rev);
        for j in 1..=n {
            let sub = prev[j - 1] + u32::from(x != at(b, j - 1, rev));
            cur[j] = sub.min(prev[j] + 1).min(cur[j - 1] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev
}

/// The full table and its backtrace, for small inputs.
fn full<T: PartialEq>(a: &[T], b: &[T], out: &mut Vec<Op>) {
    let (m, n) = (a.len(), b.len());
    let w = n + 1;
    let mut cost = vec![0u32; (m + 1) * w];
    for j in 0..=n {
        cost[m * w + j] = (n - j) as u32;
    }
    for i in (0..m).rev() {
        cost[i * w + n] = (m - i) as u32;
        for j in (0..n).rev() {
            let sub = cost[(i + 1) * w + j + 1] + u32::from(a[i] != b[j]);
            let del = cost[(i + 1) * w + j] + 1;
            let ins = cost[i * w + j + 1] + 1;
            cost[i * w + j] = sub.min(del).min(ins);
        }
    }
    let (mut i, mut j) = (0, 0);
    while i < m || j < n {
        if i < m && j < n {
            let same = a[i] == b[j];
            if cost[(i + 1) * w + j + 1] + u32::from(!same) == cost[i * w + j] {
                out.push(if same { Op::Match } else { Op::Sub });
                i += 1;
                j += 1;
                continue;
            }
        }
        if i < m && cost[(i + 1) * w + j] + 1 == cost[i * w + j] {
            out.push(Op::Del);
            i += 1;
            continue;
        }
        out.push(Op::Ins);
        j += 1;
    }
}

/// The edit script between two strings, over Unicode scalar values.
pub fn chars(a: &str, b: &str) -> Vec<Op> {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    align(&a, &b)
}

/// The edit script between two word lists.
pub fn words(a: &[&str], b: &[&str]) -> Vec<Op> {
    align(a, b)
}

/// Words as the assessment's validator counts them for Dice: lower case,
/// runs of letters and digits.
pub fn bag(text: &str) -> BTreeMap<String, usize> {
    let mut out = BTreeMap::new();
    for w in text
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
    {
        *out.entry(w.to_owned()).or_insert(0) += 1;
    }
    out
}

/// Word-bag Dice: 2 * shared words / (words of both), 1.0 for two empty
/// texts.
pub fn dice(a: &str, b: &str) -> f64 {
    let (wa, wb) = (bag(a), bag(b));
    let total: usize = wa.values().sum::<usize>() + wb.values().sum::<usize>();
    if total == 0 {
        return 1.0;
    }
    let shared: usize = wa
        .iter()
        .map(|(w, n)| (*n).min(*wb.get(w).unwrap_or(&0)))
        .sum();
    2.0 * shared as f64 / total as f64
}

/// A rate `edits / reference`; 0 when both are empty, 1 when only the
/// reference is.
pub fn rate(edits: usize, reference: usize, hypothesis: usize) -> f64 {
    if reference == 0 {
        if hypothesis == 0 {
            0.0
        } else {
            1.0
        }
    } else {
        edits as f64 / reference as f64
    }
}

pub use usnm_ocr_contract::quality::PairMetrics;

/// The pairwise metrics of two texts, after normalisation.
pub fn pair(a: &str, b: &str) -> PairMetrics {
    let (na, nb) = (text::normalize(a, false), text::normalize(b, false));
    let (chars_a, chars_b) = (na.chars().count(), nb.chars().count());
    let char_edits = distance::chars(&na, &nb);
    let (wa, wb) = (text::py_split(&na), text::py_split(&nb));
    let word_edits = distance::words(&wa, &wb);
    let mean = (chars_a + chars_b) as f64 / 2.0;
    PairMetrics {
        cer_a: rate(char_edits, chars_b, chars_a),
        cer_b: rate(char_edits, chars_a, chars_b),
        cer_sym: if mean == 0.0 {
            0.0
        } else {
            char_edits as f64 / mean
        },
        char_edits,
        chars_a,
        chars_b,
        dice: dice(&na, &nb),
        wer_a: rate(word_edits, wb.len(), wa.len()),
        wer_b: rate(word_edits, wa.len(), wb.len()),
        word_edits,
        words_a: wa.len(),
        words_b: wb.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// Replays a script: `a` edited by `ops` must give `b`.
    fn replay(a: &[u64], b: &[u64], ops: &[Op]) -> bool {
        let (mut i, mut j) = (0, 0);
        for op in ops {
            match op {
                Op::Match => {
                    if i >= a.len() || j >= b.len() || a[i] != b[j] {
                        return false;
                    }
                    i += 1;
                    j += 1;
                }
                Op::Sub => {
                    if i >= a.len() || j >= b.len() || a[i] == b[j] {
                        return false;
                    }
                    i += 1;
                    j += 1;
                }
                Op::Ins => {
                    if j >= b.len() {
                        return false;
                    }
                    j += 1;
                }
                Op::Del => {
                    if i >= a.len() {
                        return false;
                    }
                    i += 1;
                }
            }
        }
        i == a.len() && j == b.len()
    }

    #[test]
    fn known_scripts() {
        assert_eq!(chars("", ""), vec![]);
        assert_eq!(chars("abc", ""), vec![Op::Del; 3]);
        assert_eq!(chars("", "ab"), vec![Op::Ins; 2]);
        assert_eq!(
            chars("kitten", "sitting"),
            vec![
                Op::Sub,
                Op::Match,
                Op::Match,
                Op::Match,
                Op::Sub,
                Op::Match,
                Op::Ins
            ]
        );
        assert_eq!(edits(&words(&["a", "b", "c"], &["a", "c"])), 1);
    }

    /// The script's edits equal the bit-parallel distance, and the script
    /// replays `a` into `b`, on random and on edited sequences, across the
    /// full-table and the divide-and-conquer paths.
    #[test]
    fn matches_levenshtein_and_replays() {
        let mut r = Lcg(522);
        for round in 0..400 {
            let big = round % 20 == 0;
            let la = r.below(if big { 1200 } else { 120 }) as usize;
            let lb = r.below(if big { 1200 } else { 120 }) as usize;
            let k = 2 + r.below(30);
            let a: Vec<u64> = (0..la).map(|_| r.below(k)).collect();
            let mut b: Vec<u64> = if round % 2 == 0 {
                (0..lb).map(|_| r.below(k)).collect()
            } else {
                a.clone()
            };
            if round % 2 == 1 {
                for _ in 0..r.below(30) {
                    let at = r.below(b.len() as u64 + 1) as usize;
                    match r.below(3) {
                        0 => b.insert(at, r.below(k)),
                        1 if at < b.len() => {
                            b.remove(at);
                        }
                        _ if at < b.len() => b[at] = r.below(k),
                        _ => {}
                    }
                }
            }
            let ops = align(&a, &b);
            assert_eq!(edits(&ops), distance::levenshtein(&a, &b), "{a:?} {b:?}");
            assert!(replay(&a, &b, &ops), "{a:?} {b:?} {ops:?}");
        }
    }

    #[test]
    fn divide_and_conquer_is_exercised() {
        // Over SMALL cells, so the recursion splits at least once.
        let a: Vec<u64> = (0..600).map(|i| i % 7).collect();
        let b: Vec<u64> = (0..600).map(|i| (i + 3) % 7).collect();
        assert!(a.len() * b.len() > SMALL);
        let ops = align(&a, &b);
        assert_eq!(edits(&ops), distance::levenshtein(&a, &b));
        assert!(replay(&a, &b, &ops));
    }

    #[test]
    fn pair_metrics() {
        let p = pair("one two three four", "one two three five");
        assert_eq!(p.words_a, 4);
        assert_eq!(p.word_edits, 1);
        assert_eq!(p.dice, 0.75);
        assert_eq!(p.char_edits, 3);
        assert!((p.cer_a - 3.0 / 18.0).abs() < 1e-12);
        assert_eq!(p.flipped().cer_b, p.cer_a);
        let e = pair("", "abc");
        assert_eq!(e.cer_a, 1.0);
        assert_eq!(e.cer_b, 1.0);
        assert_eq!(pair("", "").cer_a, 0.0);
        assert_eq!(pair("", "").dice, 1.0);
        // Normalisation joins hyphenated line breaks before comparing.
        assert_eq!(pair("Clare-\nmont", "Claremont").char_edits, 0);
    }
}
