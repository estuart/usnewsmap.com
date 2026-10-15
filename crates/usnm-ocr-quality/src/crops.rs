//! The crops the vision-language app reads: the gold set's recipe
//! (`scripts/ocr-survey/crops.py`), chosen with a seed from LoC's ALTO so
//! the server and anyone with the page agree on them.
//!
//! A candidate is a text line with at least [`MIN_WORDS`] words whose word
//! boxes span at least [`MIN_WIDTH`] px. Its crop is the line's left edge
//! and top, less a margin, to its right edge (at most [`MAX_WIDTH`] from
//! the left) plus the margin, and down to [`HEIGHT_SHARE`] of the page
//! height below the line's top. Candidates fall into six strata, terciles
//! of their block's mean word confidence by top or bottom half of the
//! page, and `k` crops are taken round-robin over the strata, each
//! stratum's candidates in a seeded shuffle.
use serde::{Deserialize, Serialize};
use usnm_alto::{Page, Rect};

pub const DEFAULT_K: usize = 12;
pub const MIN_WORDS: usize = 4;
pub const MIN_WIDTH: f64 = 600.0;
pub const MAX_WIDTH: f64 = 1600.0;
pub const HEIGHT_SHARE: f64 = 0.06;
pub const MARGIN: f64 = 15.0;
pub const STRATA: [&str; 6] = [
    "wc-low/top",
    "wc-low/bottom",
    "wc-mid/top",
    "wc-mid/bottom",
    "wc-high/top",
    "wc-high/bottom",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Crop {
    /// Index of the block in the ALTO's reading order.
    pub block: usize,
    pub id: String,
    /// Index of the line in its block.
    pub line: usize,
    /// `[x0, y0, x1, y1]` in page pixels.
    pub rect: [u32; 4],
    pub stratum: String,
}

/// SplitMix64: a small seeded generator, so the selection needs no
/// dependency and is the same everywhere.
pub struct SplitMix(pub u64);
impl SplitMix {
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next_u64() % n
        }
    }
    /// Fisher-Yates.
    pub fn shuffle<T>(&mut self, v: &mut [T]) {
        for i in (1..v.len()).rev() {
            let j = self.below(i as u64 + 1) as usize;
            v.swap(i, j);
        }
    }
}

struct Candidate {
    block: usize,
    line: usize,
    extent: Rect,
    conf: f64,
}

fn line_extent(l: &usnm_alto::Line) -> Rect {
    Rect::union_all(l.words.iter().map(|w| &w.rect)).unwrap_or(l.rect)
}

/// The crop box of a line extent on a page `w` by `h`, as crops.py cuts it.
pub fn crop_rect(extent: &Rect, w: f64, h: f64) -> [u32; 4] {
    let x0 = (extent.x0 - MARGIN).max(0.0);
    let y0 = (extent.y0 - MARGIN).max(0.0);
    let x1 = (extent.x1.min(extent.x0 + MAX_WIDTH) + MARGIN).min(w);
    let y1 = (extent.y0 + HEIGHT_SHARE * h).min(h);
    [x0 as u32, y0 as u32, x1 as u32, y1 as u32]
}

/// Selects up to `k` crops from `page` (in image pixels) with `seed`.
pub fn select(page: &Page, seed: u64, k: usize) -> Vec<Crop> {
    let mut cands = Vec::new();
    for (bi, b) in page.blocks.iter().enumerate() {
        if !b.kind.is_text() {
            continue;
        }
        let confs: Vec<f64> = b
            .lines
            .iter()
            .flat_map(|l| &l.words)
            .filter_map(|w| w.conf)
            .collect();
        let conf = if confs.is_empty() {
            0.0
        } else {
            confs.iter().sum::<f64>() / confs.len() as f64
        };
        for (li, l) in b.lines.iter().enumerate() {
            if l.words.len() < MIN_WORDS {
                continue;
            }
            let extent = line_extent(l);
            if extent.width() < MIN_WIDTH {
                continue;
            }
            cands.push(Candidate {
                block: bi,
                line: li,
                extent,
                conf,
            });
        }
    }
    if cands.is_empty() || k == 0 {
        return vec![];
    }
    let mut confs: Vec<f64> = cands.iter().map(|c| c.conf).collect();
    confs.sort_by(f64::total_cmp);
    let (t1, t2) = (confs[confs.len() / 3], confs[confs.len() * 2 / 3]);
    let mut strata: Vec<Vec<usize>> = vec![vec![]; STRATA.len()];
    for (i, c) in cands.iter().enumerate() {
        let wc = if c.conf < t1 {
            0
        } else if c.conf < t2 {
            1
        } else {
            2
        };
        let half = usize::from(c.extent.centre().1 >= page.height / 2.0);
        strata[wc * 2 + half].push(i);
    }
    for (s, ids) in strata.iter_mut().enumerate() {
        let mut r = SplitMix(seed ^ ((s as u64 + 1).wrapping_mul(0xD1B54A32D192ED03)));
        r.shuffle(ids);
    }
    let mut chosen: Vec<usize> = Vec::new();
    let mut cursors = vec![0usize; STRATA.len()];
    while chosen.len() < k {
        let mut progressed = false;
        for (s, ids) in strata.iter().enumerate() {
            if chosen.len() >= k {
                break;
            }
            if let Some(&i) = ids.get(cursors[s]) {
                cursors[s] += 1;
                chosen.push(i);
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
    }
    chosen
        .into_iter()
        .enumerate()
        .map(|(n, i)| {
            let c = &cands[i];
            let wc = if c.conf < t1 {
                0
            } else if c.conf < t2 {
                1
            } else {
                2
            };
            let half = usize::from(c.extent.centre().1 >= page.height / 2.0);
            Crop {
                block: c.block,
                id: format!("c{:02}", n + 1),
                line: c.line,
                rect: crop_rect(&c.extent, page.width, page.height),
                stratum: STRATA[wc * 2 + half].to_owned(),
            }
        })
        .collect()
}

/// The words whose centre is in `rect`, as visual lines: sorted by their
/// centre's height, a new line when a word's centre is more than half a
/// median word height below the line's first word, then left to right
/// (crops.py's `lines_in_box`).
pub fn text_in_box(page: &Page, rect: [u32; 4]) -> String {
    let [x0, y0, x1, y1] = rect.map(f64::from);
    let mut ws: Vec<(f64, f64, f64, &str)> = page
        .blocks
        .iter()
        .filter(|b| b.kind.is_text())
        .flat_map(|b| &b.lines)
        .flat_map(|l| &l.words)
        .filter(|w| !w.text.trim().is_empty())
        .map(|w| {
            let (cx, cy) = w.rect.centre();
            (cx, cy, w.rect.height(), w.text.as_str())
        })
        .filter(|(cx, cy, _, _)| x0 <= *cx && *cx < x1 && y0 <= *cy && *cy < y1)
        .collect();
    if ws.is_empty() {
        return String::new();
    }
    let mut heights: Vec<f64> = ws.iter().map(|w| w.2).collect();
    heights.sort_by(f64::total_cmp);
    let tol = heights[heights.len() / 2] / 2.0;
    ws.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.total_cmp(&b.0)));
    let mut lines: Vec<Vec<(f64, f64, f64, &str)>> = Vec::new();
    for w in ws {
        match lines.last_mut() {
            Some(line) if w.1 - line[0].1 <= tol => line.push(w),
            _ => lines.push(vec![w]),
        }
    }
    lines
        .into_iter()
        .map(|mut line| {
            line.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1)));
            line.iter().map(|w| w.3).collect::<Vec<_>>().join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::tests::page_with_columns;

    #[test]
    fn deterministic_and_stratified() {
        let page = page_with_columns(3, 20);
        let a = select(&page, 522, DEFAULT_K);
        let b = select(&page, 522, DEFAULT_K);
        assert_eq!(a, b);
        assert_eq!(a.len(), DEFAULT_K);
        let c = select(&page, 523, DEFAULT_K);
        assert_ne!(a, c);
        // Three confidence levels across the columns and two halves: every
        // stratum has candidates, so twelve crops cover each twice.
        for s in STRATA {
            assert_eq!(a.iter().filter(|c| c.stratum == s).count(), 2, "{s}");
        }
        let ids: Vec<&str> = a.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids[0], "c01");
        assert_eq!(ids[11], "c12");
        for c in &a {
            let [x0, y0, x1, y1] = c.rect;
            assert!(x0 < x1 && y0 < y1);
            assert!(x1 <= 4000 && y1 <= 6000);
            assert!(u64::from(y1 - y0) <= (0.06 * 6000.0) as u64 + MARGIN as u64);
        }
        assert!(select(&page, 1, 0).is_empty());
        assert!(select(&page, 1, 100).len() <= 120);
    }

    #[test]
    fn short_or_narrow_lines_are_not_candidates() {
        let mut page = page_with_columns(1, 3);
        for b in &mut page.blocks {
            for l in &mut b.lines {
                l.words.truncate(3);
            }
        }
        assert!(select(&page, 1, 4).is_empty());
        let mut page = page_with_columns(1, 3);
        for b in &mut page.blocks {
            for l in &mut b.lines {
                for w in &mut l.words {
                    w.rect = Rect::new(100.0, w.rect.y0, 150.0, w.rect.y1);
                }
            }
        }
        assert!(select(&page, 1, 4).is_empty());
    }

    #[test]
    fn text_in_box_reads_visual_lines() {
        let page = page_with_columns(2, 4);
        let b = &page.blocks[0];
        let r = b.rect;
        let text = text_in_box(
            &page,
            [r.x0 as u32, r.y0 as u32, r.x1 as u32, (r.y0 + 75.0) as u32],
        );
        assert_eq!(
            text,
            "w0000 w0001 w0002 w0003 w0004\nw0010 w0011 w0012 w0013 w0014"
        );
        assert_eq!(text_in_box(&page, [0, 0, 10, 10]), "");
    }
}
