//! The consensus reading: word occurrences at least two readers agree on.
//!
//! Every pair of readers is aligned at word level (case-folded tokens). A
//! match links the two occurrences; the links' connected components are the
//! page's words as the readers saw them, and a component with members from
//! at least two readers is a consensus word. A reader's completeness is the
//! share of consensus words it has; its precision the share of its own
//! tokens that are consensus words. Where a reader's token is aligned as a
//! substitution against a consensus occurrence it does not share, the pair
//! is kept for the confusion counts.
use crate::align::{self, Op};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use unicode_normalization::UnicodeNormalization;
use usnm_ocr_survey::text;

/// A reader's text as tokens: the gold normalisation (NFC, hyphenated line
/// breaks joined, whitespace squashed), split on whitespace, each token
/// stripped of leading and trailing punctuation; empty tokens dropped. The
/// surface form is kept; matching folds case.
pub fn tokens(s: &str) -> Vec<String> {
    let n = text::normalize(s, false);
    text::py_split(&n)
        .into_iter()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|w| !w.is_empty())
        .map(|w| w.nfc().collect())
        .collect()
}

/// A token as compared: lower case.
pub fn fold(token: &str) -> String {
    token.to_lowercase()
}

/// One word the readers agree on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Word {
    /// The folded token.
    pub token: String,
    /// `(reader, position)` of each occurrence, sorted.
    pub members: Vec<(usize, usize)>,
}

/// How much of the consensus one reader has.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Coverage {
    /// The reader's tokens.
    pub tokens: usize,
    /// Its tokens that are consensus occurrences.
    pub in_consensus: usize,
    /// Consensus words it has at least one occurrence of.
    pub words: usize,
    /// `words / consensus words`; none when there is no consensus.
    pub completeness: Option<f64>,
    /// `in_consensus / tokens`; none for an empty reader.
    pub precision: Option<f64>,
}

/// A reader's token aligned as a substitution against a consensus
/// occurrence it does not share: `(reader, consensus surface, reader surface)`.
pub type Disagreement = (usize, String, String);

#[derive(Debug, Clone, PartialEq)]
pub struct Consensus {
    pub words: Vec<Word>,
    pub coverage: Vec<Coverage>,
    /// The consensus words in reading order: the pivot reader's supported
    /// runs first, then any consensus words or supported adjacent phrases
    /// present only in other readers, with `""` separating non-adjacent runs.
    pub sequence: Vec<String>,
    pub disagreements: Vec<Disagreement>,
}

struct Dsu {
    parent: Vec<usize>,
    members: Vec<Vec<(usize, usize)>>,
}
impl Dsu {
    fn new(total: usize, reader_of: impl Fn(usize) -> (usize, usize)) -> Self {
        Self {
            parent: (0..total).collect(),
            members: (0..total).map(|occ| vec![reader_of(occ)]).collect(),
        }
    }
    fn find(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            self.parent[x] = self.parent[self.parent[x]];
            x = self.parent[x];
        }
        x
    }
    fn union_if_disjoint_readers(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra == rb {
            return;
        }
        if self.members[ra]
            .iter()
            .any(|(r1, _)| self.members[rb].iter().any(|(r2, _)| r1 == r2))
        {
            return;
        }
        // The lower index is the root, so roots are a function of the data.
        let (lo, hi) = if ra < rb { (ra, rb) } else { (rb, ra) };
        self.parent[hi] = lo;
        let hi_members = std::mem::take(&mut self.members[hi]);
        self.members[lo].extend(hi_members);
        self.members[lo].sort_unstable();
    }
}

/// Builds the consensus over readers' surface tokens ([`tokens`]).
pub fn build(readers: &[Vec<String>]) -> Consensus {
    let folded: Vec<Vec<String>> = readers
        .iter()
        .map(|r| r.iter().map(|t| fold(t)).collect())
        .collect();
    let offsets: Vec<usize> = readers
        .iter()
        .scan(0, |acc, r| {
            let o = *acc;
            *acc += r.len();
            Some(o)
        })
        .collect();
    let total: usize = readers.iter().map(Vec::len).sum();
    let reader_of = |occ: usize| -> (usize, usize) {
        let r = offsets.partition_point(|o| *o <= occ) - 1;
        (r, occ - offsets[r])
    };
    let mut dsu = Dsu::new(total, reader_of);
    let mut matches: Vec<(usize, usize)> = Vec::new();
    // Substitutions and omissions per pair, resolved against the components afterwards.
    let mut subs: Vec<(usize, usize, usize, usize)> = Vec::new();
    let mut omissions: Vec<(usize, usize, usize)> = Vec::new();
    for i in 0..readers.len() {
        for j in i + 1..readers.len() {
            let ops = align::align(&folded[i], &folded[j]);
            let (mut a, mut b) = (0, 0);
            for op in ops {
                match op {
                    Op::Match => {
                        matches.push((offsets[i] + a, offsets[j] + b));
                        a += 1;
                        b += 1;
                    }
                    Op::Sub => {
                        subs.push((i, a, j, b));
                        a += 1;
                        b += 1;
                    }
                    Op::Ins => {
                        omissions.push((i, j, b));
                        b += 1;
                    }
                    Op::Del => {
                        omissions.push((j, i, a));
                        a += 1;
                    }
                }
            }
        }
    }
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); total];
    for &(u, v) in &matches {
        adj[u].push(v);
        adj[v].push(u);
    }
    for nbrs in &mut adj {
        nbrs.sort_unstable();
        nbrs.dedup();
    }
    let mut scored_edges: Vec<(usize, usize, usize)> = matches
        .iter()
        .map(|&(u, v)| {
            let support = adj[u]
                .iter()
                .filter(|w| adj[v].binary_search(w).is_ok())
                .count();
            (support, u, v)
        })
        .collect();
    scored_edges.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    for (_, u, v) in scored_edges {
        dsu.union_if_disjoint_readers(u, v);
    }
    let mut groups: BTreeMap<usize, Vec<(usize, usize)>> = BTreeMap::new();
    for occ in 0..total {
        let root = dsu.find(occ);
        groups.entry(root).or_default().push(reader_of(occ));
    }
    let mut root_of_word: BTreeMap<usize, usize> = BTreeMap::new();
    let mut words = Vec::new();
    for (root, mut members) in groups {
        members.sort();
        let distinct = members.windows(2).filter(|w| w[0].0 != w[1].0).count() + 1;
        if members.len() >= 2 && distinct >= 2 {
            let (r, p) = members[0];
            root_of_word.insert(root, words.len());
            words.push(Word {
                token: folded[r][p].clone(),
                members,
            });
        }
    }
    let mut coverage = Vec::new();
    for (r, toks) in readers.iter().enumerate() {
        let mut seen: Vec<bool> = vec![false; words.len()];
        let mut in_consensus = 0;
        for p in 0..toks.len() {
            if let Some(&w) = root_of_word.get(&dsu.find(offsets[r] + p)) {
                in_consensus += 1;
                seen[w] = true;
            }
        }
        let have = seen.iter().filter(|s| **s).count();
        coverage.push(Coverage {
            tokens: toks.len(),
            in_consensus,
            words: have,
            completeness: (!words.is_empty()).then(|| have as f64 / words.len() as f64),
            precision: (!toks.is_empty()).then(|| in_consensus as f64 / toks.len() as f64),
        });
    }
    let pivot = coverage
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.words.cmp(&b.1.words).then(b.0.cmp(&a.0)))
        .map_or(0, |(i, _)| i);
    let is_adjacent = |wa: usize, wb: usize| -> bool {
        let (mut adj_count, mut non_adj_count) = (0usize, 0usize);
        for &(r, pa) in &words[wa].members {
            if let Some(&(_, pb)) = words[wb].members.iter().find(|&&(r2, _)| r2 == r) {
                if pb == pa + 1 {
                    adj_count += 1;
                } else {
                    non_adj_count += 1;
                }
            }
        }
        adj_count >= 2 || (adj_count >= 1 && non_adj_count == 0)
    };
    let mut sequence: Vec<String> = Vec::new();
    let mut covered_words = vec![false; words.len()];
    let mut covered_adj: BTreeSet<(usize, usize)> = BTreeSet::new();
    let mut reader_order = Vec::with_capacity(readers.len());
    if !readers.is_empty() {
        reader_order.push(pivot);
        for r in 0..readers.len() {
            if r != pivot {
                reader_order.push(r);
            }
        }
    }
    for (idx, &r) in reader_order.iter().enumerate() {
        let occs: Vec<(usize, usize)> = (0..readers[r].len())
            .filter_map(|p| root_of_word.get(&dsu.find(offsets[r] + p)).map(|&w| (p, w)))
            .collect();
        let mut runs: Vec<Vec<usize>> = Vec::new();
        for &(p, w) in &occs {
            let continues = runs.last().is_some_and(|run| {
                let prev_w = *run.last().unwrap();
                let prev_p = occs[runs.iter().map(Vec::len).sum::<usize>() - 1].0;
                p == prev_p + 1 && is_adjacent(prev_w, w)
            });
            if continues {
                runs.last_mut().unwrap().push(w);
            } else {
                runs.push(vec![w]);
            }
        }
        for run in runs {
            let need = idx == 0
                || run.iter().any(|&w| !covered_words[w])
                || run
                    .windows(2)
                    .any(|pair| !covered_adj.contains(&(pair[0], pair[1])));
            if !need {
                continue;
            }
            if !sequence.is_empty() {
                sequence.push(String::new());
            }
            for &w in &run {
                covered_words[w] = true;
                sequence.push(words[w].token.clone());
            }
            for pair in run.windows(2) {
                covered_adj.insert((pair[0], pair[1]));
            }
        }
    }
    // A substitution counts against a reader when the other side's
    // occurrence is a consensus word the reader has no occurrence in; one
    // per reader and word.
    let mut disagreements = Vec::new();
    let mut taken: Vec<(usize, usize)> = Vec::new();
    let mut has: Vec<Vec<bool>> = vec![vec![false; words.len()]; readers.len()];
    for (w, word) in words.iter().enumerate() {
        for &(r, _) in &word.members {
            has[r][w] = true;
        }
    }
    for (i, a, j, b) in subs {
        for (reader, pos, other, opos) in [(i, a, j, b), (j, b, i, a)] {
            let Some(&w) = root_of_word.get(&dsu.find(offsets[other] + opos)) else {
                continue;
            };
            if has[reader][w] || taken.contains(&(reader, w)) {
                continue;
            }
            taken.push((reader, w));
            disagreements.push((
                reader,
                readers[other][opos].clone(),
                readers[reader][pos].clone(),
            ));
        }
    }
    for (reader, other, opos) in omissions {
        let Some(&w) = root_of_word.get(&dsu.find(offsets[other] + opos)) else {
            continue;
        };
        if has[reader][w] || taken.contains(&(reader, w)) {
            continue;
        }
        taken.push((reader, w));
        disagreements.push((reader, readers[other][opos].clone(), String::new()));
    }
    disagreements.sort();
    Consensus {
        words,
        coverage,
        sequence,
        disagreements,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_strip_punctuation_and_keep_case() {
        assert_eq!(
            tokens("The Mayor, of Clare-\nmont (Ohio)."),
            vec!["The", "Mayor", "of", "Claremont", "Ohio"]
        );
        assert_eq!(tokens("-- ... !"), Vec::<String>::new());
    }

    #[test]
    fn two_of_three_make_a_word() {
        let a = tokens("the quick brown fox jumps");
        let b = tokens("the quick brovn fox jumps over");
        let c = tokens("tho quick brown fax");
        let cons = build(&[a, b, c]);
        let toks: Vec<&str> = cons.words.iter().map(|w| w.token.as_str()).collect();
        assert_eq!(toks, vec!["the", "quick", "brown", "fox", "jumps"]);
        assert_eq!(cons.sequence, vec!["the", "quick", "brown", "fox", "jumps"]);
        let c0 = &cons.coverage[0];
        assert_eq!(c0.completeness, Some(1.0));
        assert_eq!(c0.precision, Some(1.0));
        let c1 = &cons.coverage[1];
        assert_eq!(c1.words, 4);
        assert!((c1.completeness.unwrap() - 0.8).abs() < 1e-12);
        assert!((c1.precision.unwrap() - 4.0 / 6.0).abs() < 1e-12);
        let c2 = &cons.coverage[2];
        assert_eq!(c2.words, 2);
        assert!(cons
            .disagreements
            .contains(&(1, "brown".into(), "brovn".into())));
        assert!(cons
            .disagreements
            .contains(&(2, "the".into(), "tho".into())));
    }

    #[test]
    fn case_folds_and_empty_reader() {
        let cons = build(&[tokens("Main Street"), tokens("main street"), tokens("")]);
        assert_eq!(cons.words.len(), 2);
        assert_eq!(cons.coverage[2].completeness, Some(0.0));
        assert_eq!(cons.coverage[2].precision, None);
        let none = build(&[tokens("abc"), tokens("xyz")]);
        assert!(none.words.is_empty());
        assert_eq!(none.coverage[0].completeness, None);
        assert!(build(&[]).words.is_empty());
    }

    #[test]
    fn repeated_token_does_not_merge_two_positions_from_one_reader() {
        let cons = build(&[tokens("the"), tokens("the the"), tokens("the x")]);
        assert_eq!(cons.words.len(), 1);
        assert_eq!(cons.words[0].token, "the");
        assert_eq!(cons.words[0].members, vec![(0, 0), (1, 0), (2, 0)]);
        assert_eq!(cons.coverage[1].tokens, 2);
        assert_eq!(cons.coverage[1].in_consensus, 1);
        assert_eq!(cons.coverage[1].words, 1);
        assert_eq!(cons.coverage[1].completeness, Some(1.0));
        assert_eq!(cons.coverage[1].precision, Some(0.5));
    }

    #[test]
    fn sequence_preserves_gaps_and_includes_non_pivot_consensus_words() {
        let gapped = build(&[
            tokens("New damaged York"),
            tokens("New smudged York"),
            tokens("New unreadable York"),
        ]);
        assert_eq!(gapped.sequence, vec!["new", "", "york"]);

        let split = build(&[
            tokens("Albany"),
            tokens("Albany"),
            tokens("Boston"),
            tokens("Boston"),
        ]);
        assert_eq!(split.sequence, vec!["albany", "", "boston"]);

        let omitted = build(&[
            tokens("Albany mayor"),
            tokens("Albany mayor"),
            tokens("Albany"),
        ]);
        assert_eq!(
            omitted.disagreements,
            vec![(2, "mayor".into(), String::new())]
        );
    }
}
