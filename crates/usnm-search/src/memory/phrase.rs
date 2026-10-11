//! Phrase and NEAR matching as Quickwit 0.9.1 runs it (#243): a port of
//! tantivy's `PhraseScorer` (revision 057458b,
//! `src/query/phrase_query/phrase_scorer.rs`), so the memory backend
//! matches the engine's words out of order and its 3+-word quirks exactly.
//!
//! Each term's positions come shifted by how many terms follow it in the
//! phrase, so an in-order match lines up on one position, and two terms
//! match when their shifted positions are at most `slop` apart, either way:
//! a swap costs 2. With three or more terms, the middle ones carry the slop
//! spent so far; the last one is checked against the whole slop when the
//! engine only asks whether a page matches (scoring off: counts, cubes and
//! hits by date), and charged the carried slop when it scores the page
//! (hits by relevance). See 05 §5.6.

use std::cmp::Ordering;

/// Whether the engine scores the search. It decides how the last term of a
/// phrase of three or more words with slop is matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scoring {
    /// Counts, aggregations and hits sorted by date: does the page match?
    Off,
    /// Hits by relevance (`_score`): how often does the phrase occur?
    On,
}

/// `PhraseScorer::phrase_match` and `phrase_count` for one text: given each
/// term's positions in it, already shifted (`positions_with_offset`), the
/// phrase's count there with scoring on, or 1 when it matches with scoring
/// off. 0 means no match.
pub fn phrase_count(terms: &[Vec<u32>], slop: u32, scoring: Scoring) -> u32 {
    let n = terms.len();
    // The intersection docset: every term on the page.
    if n < 2 || terms.iter().any(Vec::is_empty) {
        return 0;
    }
    // `compute_phrase_match`.
    let mut left = terms[0].clone();
    let mut left_slops: Vec<u8> = Vec::new();
    for right in &terms[1..n - 1] {
        if slop > 0 {
            intersection_count_with_carrying_slop(&mut left, &mut left_slops, right, slop, true);
        } else {
            intersection(&mut left, right);
        }
        if left.is_empty() {
            return 0;
        }
    }
    let right = &terms[n - 1];
    match scoring {
        // `phrase_exists`.
        Scoring::Off => {
            let exists = if slop > 0 {
                intersection_exists_with_slop(&left, right, slop)
            } else {
                intersection_exists(&left, right)
            };
            u32::from(exists)
        }
        // `compute_phrase_count`.
        Scoring::On => {
            if slop > 0 {
                if n > 2 {
                    intersection_count_with_carrying_slop(
                        &mut left,
                        &mut left_slops,
                        right,
                        slop,
                        false,
                    )
                } else {
                    intersection_count_with_slop(&mut left, right, slop, false) as u32
                }
            } else {
                intersection_count(&left, right) as u32
            }
        }
    }
}

/// Whether the two sorted arrays share an element.
fn intersection_exists(left: &[u32], right: &[u32]) -> bool {
    let (mut l, mut r) = (0, 0);
    while l < left.len() && r < right.len() {
        match left[l].cmp(&right[r]) {
            Ordering::Less => l += 1,
            Ordering::Equal => return true,
            Ordering::Greater => r += 1,
        }
    }
    false
}

/// How many elements the two sorted arrays share.
fn intersection_count(left: &[u32], right: &[u32]) -> usize {
    let (mut l, mut r, mut count) = (0, 0, 0);
    while l < left.len() && r < right.len() {
        match left[l].cmp(&right[r]) {
            Ordering::Less => l += 1,
            Ordering::Equal => {
                count += 1;
                l += 1;
                r += 1;
            }
            Ordering::Greater => r += 1,
        }
    }
    count
}

/// The two sorted arrays' intersection, into `left`.
fn intersection(left: &mut Vec<u32>, right: &[u32]) {
    let (mut l, mut r, mut count) = (0, 0, 0);
    while l < left.len() && r < right.len() {
        match left[l].cmp(&right[r]) {
            Ordering::Less => l += 1,
            Ordering::Equal => {
                left[count] = left[l];
                count += 1;
                l += 1;
                r += 1;
            }
            Ordering::Greater => r += 1,
        }
    }
    left.truncate(count);
}

/// How many pairs of positions are at most `slop` apart, either way, each
/// position used once; with `update_left`, the right positions matched go
/// into `left`.
fn intersection_count_with_slop(
    left: &mut Vec<u32>,
    right: &[u32],
    slop: u32,
    update_left: bool,
) -> usize {
    let (mut l, mut r, mut count) = (0, 0, 0);
    let left_len = left.len();
    while l < left_len && r < right.len() {
        let (left_val, right_val) = (left[l], right[r]);
        if left_val.abs_diff(right_val) <= slop {
            // There could be a better match.
            while l + 1 < left_len {
                if left[l + 1] > right_val {
                    break;
                }
                l += 1;
            }
            if update_left {
                left[count] = right_val;
            }
            count += 1;
            l += 1;
            r += 1;
        } else if left_val < right_val {
            l += 1;
        } else {
            r += 1;
        }
    }
    if update_left {
        left.truncate(count);
    }
    count
}

/// Whether any pair of positions is at most `slop` apart, either way.
fn intersection_exists_with_slop(left: &[u32], right: &[u32], slop: u32) -> bool {
    let (mut l, mut r) = (0, 0);
    while l < left.len() && r < right.len() {
        let (left_val, right_val) = (left[l], right[r]);
        if left_val.abs_diff(right_val) <= slop {
            return true;
        } else if left_val < right_val {
            l += 1;
        } else {
            r += 1;
        }
    }
    false
}

/// tantivy's intersection for three or more terms, which carries the slop
/// spent so far (`left_slops`, empty meaning none yet) and counts a pair at
/// most `max_slop` apart with it. With `update_left`, `left` and
/// `left_slops` become the positions either side of each match, and those
/// past the last that are within the slop, each with the slop spent. Its
/// quirks (it may count a position twice) are the engine's, kept.
fn intersection_count_with_carrying_slop(
    left_positions: &mut Vec<u32>,
    left_slops: &mut Vec<u8>,
    right_positions: &[u32],
    max_slop: u32,
    update_left: bool,
) -> u32 {
    let mut left_index = 0;
    let mut right_index = 0;
    let mut count = 0;
    if left_positions.is_empty() || right_positions.is_empty() {
        if update_left {
            left_positions.clear();
            left_slops.clear();
        }
        return 0;
    }
    let mut positions_buffer: Vec<u32> = Vec::new();
    let mut slops_buffer: Vec<u8> = Vec::new();
    let add_val = |val: (u8, u32), new_left: &mut Vec<u32>, new_slops: &mut Vec<u8>| {
        if update_left {
            if new_left.last() == Some(&val.1) {
                let last_slop = new_slops.last_mut().expect("a slop per position");
                *last_slop = (*last_slop).min(val.0);
            } else {
                new_left.push(val.1);
                new_slops.push(val.0);
            }
        }
    };
    loop {
        let left_val = left_positions[left_index];
        let slop_so_far = left_slops.get(left_index).copied().unwrap_or(0);
        let right_val = right_positions[right_index];
        let distance = u32::from(slop_so_far) + left_val.abs_diff(right_val);
        if distance <= max_slop {
            let (smaller_val, larger_val, mut smaller_val_idx, smaller_val_positions) =
                if left_val < right_val {
                    (left_val, right_val, left_index, left_positions.as_slice())
                } else {
                    (right_val, left_val, right_index, right_positions)
                };
            let mut new_slop = distance;
            add_val(
                (new_slop as u8, smaller_val),
                &mut positions_buffer,
                &mut slops_buffer,
            );
            while smaller_val_idx + 1 < smaller_val_positions.len() {
                // There could be a better match.
                let next_val = smaller_val_positions[smaller_val_idx + 1];
                if next_val > larger_val {
                    break;
                }
                let distance = next_val.abs_diff(larger_val);
                smaller_val_idx += 1;
                new_slop = u32::from(slop_so_far) + distance;
                add_val(
                    (new_slop as u8, next_val),
                    &mut positions_buffer,
                    &mut slops_buffer,
                );
            }
            add_val(
                (new_slop as u8, larger_val),
                &mut positions_buffer,
                &mut slops_buffer,
            );
            count += 1;
            left_index += 1;
            right_index += 1;
        } else if left_val < right_val {
            left_index += 1;
        } else {
            right_index += 1;
        }

        if left_index >= left_positions.len() || right_index >= right_positions.len() {
            // Finish the rest.
            if left_index >= left_positions.len() {
                let left_val = *left_positions.last().expect("not empty");
                let slop_so_far = *left_slops.last().unwrap_or(&0);
                for &right_val in &right_positions[right_index..] {
                    let new_slop = left_val.abs_diff(right_val) + u32::from(slop_so_far);
                    if new_slop <= max_slop {
                        add_val(
                            (new_slop as u8, right_val),
                            &mut positions_buffer,
                            &mut slops_buffer,
                        );
                    }
                }
            } else {
                let right_val = *right_positions.last().expect("not empty");
                for (left_idx, &left_val) in left_positions.iter().enumerate().skip(left_index) {
                    let slop_so_far = left_slops.get(left_idx).copied().unwrap_or(0);
                    let new_slop = left_val.abs_diff(right_val) + u32::from(slop_so_far);
                    if new_slop <= max_slop {
                        add_val(
                            (new_slop as u8, left_val),
                            &mut positions_buffer,
                            &mut slops_buffer,
                        );
                    }
                }
            }
            break;
        }
    }
    if update_left {
        *left_positions = positions_buffer;
        *left_slops = slops_buffer;
    }
    count
}

#[cfg(test)]
mod tests {
    //! tantivy's own cases for these functions, and the engine's slop.
    use super::*;

    fn intersection_aux(left: &[u32], right: &[u32], expected: &[u32], slop: u32) {
        let mut left_vec = left.to_vec();
        if slop == 0 {
            assert_eq!(intersection_count(&left_vec, right), expected.len());
            intersection(&mut left_vec, right);
        } else {
            intersection_count_with_slop(&mut left_vec, right, slop, true);
        }
        assert_eq!(left_vec, expected);
    }

    fn intersection_sym(left: &[u32], right: &[u32], expected: &[u32]) {
        intersection_aux(left, right, expected, 0);
        intersection_aux(right, left, expected, 0);
    }

    #[test]
    fn intersects() {
        intersection_sym(&[1], &[1], &[1]);
        intersection_sym(&[1], &[2], &[]);
        intersection_sym(&[], &[2], &[]);
        intersection_sym(&[5, 7], &[1, 5, 10, 12], &[5]);
        intersection_sym(&[1, 5, 6, 9, 10, 12], &[6, 8, 9, 12], &[6, 9, 12]);
        assert!(intersection_exists(&[5, 7], &[1, 5, 10, 12]));
        assert!(!intersection_exists(&[1, 3], &[2, 4]));
    }

    #[test]
    fn slop() {
        intersection_aux(&[1], &[2], &[2], 1);
        intersection_aux(&[1], &[3], &[], 1);
        intersection_aux(&[1], &[3], &[3], 2);
        intersection_aux(&[], &[2], &[], 100000);
        intersection_aux(&[5, 7, 11], &[1, 5, 10, 12], &[5, 10], 1);
        intersection_aux(&[1, 5, 6, 9, 10, 12], &[6, 8, 9, 12], &[6, 8, 9, 12], 1);
        intersection_aux(&[1, 5, 6, 9, 10, 12], &[6, 8, 9, 12], &[6, 8, 9, 12], 10);
        intersection_aux(&[1, 3, 5], &[2, 4, 6], &[2, 4, 6], 1);
        intersection_aux(&[1, 3, 5], &[2, 4, 6], &[], 0);
        // Either way: the right position may come first.
        intersection_aux(&[3], &[1], &[1], 2);
        intersection_aux(&[3], &[1], &[], 1);
        assert!(intersection_exists_with_slop(&[3], &[1], 2));
        assert!(!intersection_exists_with_slop(&[3], &[1], 1));
    }

    #[test]
    fn merge_slop() {
        let merge = |left: &[u32], right: &[u32], expected: &[u32], slop| {
            let mut left = left.to_vec();
            intersection_count_with_slop(&mut left, right, slop, true);
            assert_eq!(left, expected);
        };
        merge(&[1, 2], &[1], &[1], 1);
        merge(&[3], &[4], &[4], 2);
        merge(&[1, 5, 6, 9, 10, 12], &[6, 8, 9, 12], &[6, 8, 9, 12], 10);
    }

    fn carry_slop_aux(terms: &[&[u32]], expected: &[(u8, u32)], slop: u32, expected_count: u32) {
        let mut left = terms[0].to_vec();
        let mut slops = vec![0; left.len()];
        let mut count = 0;
        for right in &terms[1..] {
            count = intersection_count_with_carrying_slop(&mut left, &mut slops, right, slop, true);
        }
        let out: Vec<(u8, u32)> = slops.into_iter().zip(left).collect();
        assert_eq!(out, expected);
        assert_eq!(count, expected_count);
    }

    #[test]
    fn carry_slop_intersection() {
        carry_slop_aux(&[&[1], &[]], &[], 1, 0);
        carry_slop_aux(&[&[1], &[2]], &[(1, 1), (1, 2)], 1, 1);
        carry_slop_aux(&[&[1], &[3]], &[], 1, 0);
        // The order may still matter.
        carry_slop_aux(&[&[1], &[2], &[2]], &[(1, 2)], 1, 1);
        carry_slop_aux(&[&[2], &[1], &[2]], &[(1, 2)], 1, 1);
        carry_slop_aux(&[&[2], &[2], &[1]], &[(1, 1), (1, 2)], 1, 1);
        carry_slop_aux(&[&[2], &[2], &[1], &[2]], &[(1, 2)], 1, 1);
        carry_slop_aux(&[&[1], &[2], &[2], &[2]], &[(1, 2)], 1, 1);
        carry_slop_aux(&[&[1], &[2], &[1]], &[(1, 1)], 1, 1);
        carry_slop_aux(&[&[11], &[10, 12]], &[(1, 10), (1, 11), (1, 12)], 1, 1);
        carry_slop_aux(&[&[10, 12], &[11]], &[(1, 10), (1, 11), (1, 12)], 1, 1);
        carry_slop_aux(
            &[&[5, 7, 11], &[1, 5, 10, 12]],
            &[(0, 5), (1, 10), (1, 11), (1, 12)],
            1,
            2,
        );
    }

    /// Positions as the scorer reads them: term `i` of `n` at `p` is at
    /// `p + n - 1 - i`.
    fn shifted(doc: &[&str], phrase: &[&str]) -> Vec<Vec<u32>> {
        let n = phrase.len() as u32;
        (0u32..)
            .zip(phrase)
            .map(|(i, word)| {
                (0u32..)
                    .zip(doc)
                    .filter(|(_, w)| *w == word)
                    .map(|(p, _)| p + n - 1 - i)
                    .collect()
            })
            .collect()
    }

    fn count(doc: &str, phrase: &str, slop: u32, scoring: Scoring) -> u32 {
        let doc: Vec<&str> = doc.split(' ').collect();
        let phrase: Vec<&str> = phrase.split(' ').collect();
        phrase_count(&shifted(&doc, &phrase), slop, scoring)
    }

    #[test]
    fn two_words_match_out_of_order_with_a_swap_costing_two() {
        for scoring in [Scoring::Off, Scoring::On] {
            assert_eq!(count("a b", "a b", 0, scoring), 1);
            assert_eq!(count("a b", "b a", 0, scoring), 0);
            assert_eq!(count("a b", "b a", 1, scoring), 0);
            assert_eq!(count("a b", "b a", 2, scoring), 1);
            // One word between: in order 1, swapped 3.
            assert_eq!(count("a x b", "a b", 0, scoring), 0);
            assert_eq!(count("a x b", "a b", 1, scoring), 1);
            assert_eq!(count("a x b", "b a", 2, scoring), 0);
            assert_eq!(count("a x b", "b a", 3, scoring), 1);
        }
        // The issue's case: "said the speaker to the convention".
        let doc = "said the speaker to the convention";
        assert_eq!(count(doc, "speaker convention", 3, Scoring::Off), 1);
        assert_eq!(count(doc, "convention speaker", 3, Scoring::Off), 0);
        assert_eq!(count(doc, "convention speaker", 4, Scoring::Off), 1);
        assert_eq!(count(doc, "convention speaker", 5, Scoring::Off), 1);
        // Scored, each pair counts once.
        assert_eq!(count("a b x a b", "a b", 1, Scoring::On), 2);
    }

    #[test]
    fn three_words_carry_the_slop_only_when_scored() {
        // Two words apart each time: 4 in all, over a slop of 2. The middle
        // word carries 2, and the last is checked against the whole slop
        // when the engine only asks whether the page matches.
        let doc = "a x x b x x c";
        assert_eq!(count(doc, "a b c", 2, Scoring::Off), 1);
        assert_eq!(count(doc, "a b c", 2, Scoring::On), 0);
        assert_eq!(count(doc, "a b c", 3, Scoring::On), 0);
        assert_eq!(count(doc, "a b c", 4, Scoring::On), 1);
        assert_eq!(count(doc, "a b c", 1, Scoring::Off), 0);
        // In order and close, both agree.
        for scoring in [Scoring::Off, Scoring::On] {
            assert_eq!(count("a b c", "a b c", 0, scoring), 1);
            assert_eq!(count("a b x c", "a b c", 1, scoring), 1);
            assert_eq!(count("a x b x c", "a b c", 0, scoring), 0);
        }
        // One word apart each time, over a slop of 1: matched, not scored.
        assert_eq!(count("a x b x c", "a b c", 1, Scoring::Off), 1);
        assert_eq!(count("a x b x c", "a b c", 1, Scoring::On), 0);
        assert_eq!(count("a x b x c", "a b c", 2, Scoring::On), 1);
        // Swapped: "b a c" on "a b c" spends 2 on the swap, and the scorer
        // charges the last word 1 more.
        assert_eq!(count("a b c", "b a c", 1, Scoring::Off), 0);
        assert_eq!(count("a b c", "b a c", 2, Scoring::Off), 1);
        assert_eq!(count("a b c", "b a c", 2, Scoring::On), 0);
        assert_eq!(count("a b c", "b a c", 3, Scoring::On), 1);
        // The issue's case: "Wheat closed ½ higher".
        let doc = "wheat closed ½ higher";
        for scoring in [Scoring::Off, Scoring::On] {
            assert_eq!(count(doc, "wheat higher ½", 2, scoring), 1);
            assert_eq!(count(doc, "wheat higher ½", 1, scoring), 0);
        }
    }

    #[test]
    fn every_term_must_be_on_the_page() {
        for scoring in [Scoring::Off, Scoring::On] {
            assert_eq!(count("a b", "a c", 5, scoring), 0);
            assert_eq!(count("a b", "a c b", 5, scoring), 0);
            assert_eq!(phrase_count(&[vec![1]], 1, scoring), 0);
        }
    }
}
