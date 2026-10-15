//! Confusion pairs: what a reader prints for what the consensus (or the
//! vision-language reading) has, counted from the character alignment of
//! each disagreeing word pair. Consecutive non-matching steps are merged
//! into one pair, so `rn` read as `m` is one substitution, not a
//! substitution and a deletion.
use crate::align::{self, Op};
use std::collections::BTreeMap;
pub use usnm_ocr_contract::quality::{ConfusionKind as Kind, ConfusionPair};

/// How many pairs a reader's list keeps.
pub const TOP: usize = 50;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Confusions(BTreeMap<(Kind, String, String), usize>);

impl Confusions {
    /// Counts the pairs in `reader` against `reference`, one word each.
    pub fn add(&mut self, reference: &str, reader: &str) {
        let r: Vec<char> = reference.chars().collect();
        let h: Vec<char> = reader.chars().collect();
        let ops = align::align(&r, &h);
        let (mut i, mut j) = (0, 0);
        let mut from = String::new();
        let mut to = String::new();
        let mut flush = |from: &mut String, to: &mut String| {
            if from.is_empty() && to.is_empty() {
                return;
            }
            let kind = match (from.is_empty(), to.is_empty()) {
                (true, _) => Kind::Ins,
                (_, true) => Kind::Del,
                _ => Kind::Sub,
            };
            *self
                .0
                .entry((kind, std::mem::take(from), std::mem::take(to)))
                .or_insert(0) += 1;
        };
        for op in ops {
            match op {
                Op::Match => {
                    flush(&mut from, &mut to);
                    i += 1;
                    j += 1;
                }
                Op::Sub => {
                    from.push(r[i]);
                    to.push(h[j]);
                    i += 1;
                    j += 1;
                }
                Op::Ins => {
                    to.push(h[j]);
                    j += 1;
                }
                Op::Del => {
                    from.push(r[i]);
                    i += 1;
                }
            }
        }
        flush(&mut from, &mut to);
    }

    /// The `n` most frequent pairs, by count then key.
    pub fn top(&self, n: usize) -> Vec<ConfusionPair> {
        let mut v: Vec<ConfusionPair> = self
            .0
            .iter()
            .map(|((kind, from, to), count)| ConfusionPair {
                count: *count,
                from: from.clone(),
                kind: *kind,
                to: to.clone(),
            })
            .collect();
        v.sort_by(|a, b| {
            b.count
                .cmp(&a.count)
                .then(a.kind.cmp(&b.kind))
                .then(a.from.cmp(&b.from))
                .then(a.to.cmp(&b.to))
        });
        v.truncate(n);
        v
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_runs_and_counts() {
        let mut c = Confusions::default();
        c.add("modern", "rnodern");
        c.add("modern", "rnodern");
        c.add("Claremont", "Claremon");
        c.add("fox", "fox");
        c.add("tale", "talle");
        let top = c.top(10);
        assert_eq!(top[0].count, 2);
        assert_eq!(
            (top[0].kind, top[0].from.as_str(), top[0].to.as_str()),
            (Kind::Sub, "m", "rn")
        );
        assert!(top.contains(&ConfusionPair {
            count: 1,
            from: "t".into(),
            kind: Kind::Del,
            to: String::new()
        }));
        assert!(top.contains(&ConfusionPair {
            count: 1,
            from: String::new(),
            kind: Kind::Ins,
            to: "l".into()
        }));
        assert_eq!(top.len(), 3);
        assert_eq!(c.top(1).len(), 1);
    }
}
