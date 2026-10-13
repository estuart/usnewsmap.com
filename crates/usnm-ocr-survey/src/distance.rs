//! Levenshtein distance (unit-cost insert, delete, substitute), the same number as gold.py's
//! `edit_distance`, computed with Myers' bit-parallel algorithm in Hyyrö's multi-word form:
//! O(⌈m/64⌉·n) word operations for sequences of length m ≤ n, against gold.py's O(m·n) Python loop.

use std::collections::HashMap;
use std::hash::Hash;

const W: usize = 64;

/// One 64-row block of one column (edlib's `calculateBlock`). `hin` is the horizontal delta
/// entering the block's top row (-1, 0 or +1); returns the delta leaving the row selected by `out_bit`.
#[inline]
fn block(pv: &mut u64, mv: &mut u64, mut eq: u64, hin: i32, out_bit: u64) -> i32 {
    let hin_neg = u64::from(hin < 0);
    let xv = eq | *mv;
    eq |= hin_neg;
    let xh = (((eq & *pv).wrapping_add(*pv)) ^ *pv) | eq;
    let mut ph = *mv | !(xh | *pv);
    let mut mh = *pv & xh;
    let hout = i32::from(ph & out_bit != 0) - i32::from(mh & out_bit != 0);
    ph <<= 1;
    mh <<= 1;
    mh |= hin_neg;
    ph |= u64::from(hin > 0);
    *pv = mh | !(xv | ph);
    *mv = ph & xv;
    hout
}

/// The edit distance between two sequences.
pub fn levenshtein<T: Eq + Hash + Copy>(a: &[T], b: &[T]) -> usize {
    // The shorter one is the pattern (the bit vectors), the longer one is scanned.
    let (p, t) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let m = p.len();
    if m == 0 {
        return t.len();
    }
    let blocks = m.div_ceil(W);
    let mut peq: HashMap<T, Vec<u64>> = HashMap::new();
    for (i, &c) in p.iter().enumerate() {
        peq.entry(c).or_insert_with(|| vec![0; blocks])[i / W] |= 1 << (i % W);
    }
    let zero = vec![0u64; blocks];
    let last_bit = 1u64 << ((m - 1) % W);
    let mut pv = vec![!0u64; blocks];
    let mut mv = vec![0u64; blocks];
    let mut score = m as i64;
    for c in t {
        let eq = peq.get(c).unwrap_or(&zero);
        let mut h = 1;
        for k in 0..blocks {
            let out_bit = if k + 1 == blocks {
                last_bit
            } else {
                1 << (W - 1)
            };
            h = block(&mut pv[k], &mut mv[k], eq[k], h, out_bit);
        }
        score += i64::from(h);
    }
    usize::try_from(score).unwrap_or(0)
}

/// The edit distance between two strings, over Unicode scalar values (Python's `str` indexing).
pub fn chars(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    levenshtein(&a, &b)
}

/// The edit distance between two word lists; equal words are equal symbols.
pub fn words<'a>(a: &[&'a str], b: &[&'a str]) -> usize {
    let mut ids: HashMap<&'a str, u32> = HashMap::new();
    let mut id = |w: &'a str| {
        let n = u32::try_from(ids.len()).unwrap_or(u32::MAX);
        *ids.entry(w).or_insert(n)
    };
    let a: Vec<u32> = a.iter().map(|w| id(w)).collect();
    let b: Vec<u32> = b.iter().map(|w| id(w)).collect();
    levenshtein(&a, &b)
}

/// gold.py's quadratic loop, for tests.
#[doc(hidden)]
pub fn reference<T: Eq>(a: &[T], b: &[T]) -> usize {
    let (a, b) = if a.len() < b.len() { (b, a) } else { (a, b) };
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, x) in a.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, y) in b.iter().enumerate() {
            cur.push(
                (prev[j + 1] + 1)
                    .min(cur[j] + 1)
                    .min(prev[j] + usize::from(x != y)),
            );
        }
        prev = cur;
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small seeded generator, so the test needs no dependency.
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

    #[test]
    fn known_values() {
        assert_eq!(chars("", ""), 0);
        assert_eq!(chars("abc", ""), 3);
        assert_eq!(chars("", "abc"), 3);
        assert_eq!(chars("kitten", "sitting"), 3);
        assert_eq!(chars("flaw", "lawn"), 2);
        assert_eq!(chars("é", "e"), 1);
        assert_eq!(words(&["a", "b", "c"], &["a", "c"]), 1);
    }

    #[test]
    fn matches_the_quadratic_loop() {
        let mut r = Lcg(457);
        for round in 0..3000 {
            // Lengths across the 64-row block edges, alphabets from 2 to 40 symbols.
            let la = r.below(if round % 10 == 0 { 300 } else { 140 }) as usize;
            let lb = r.below(if round % 10 == 0 { 300 } else { 140 }) as usize;
            let k = 2 + r.below(39);
            let a: Vec<u64> = (0..la).map(|_| r.below(k)).collect();
            let mut b: Vec<u64> = if round % 2 == 0 {
                (0..lb).map(|_| r.below(k)).collect()
            } else {
                a.clone()
            };
            if round % 2 == 1 {
                for _ in 0..r.below(20) {
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
            assert_eq!(levenshtein(&a, &b), reference(&a, &b), "{a:?} {b:?}");
        }
    }
}
