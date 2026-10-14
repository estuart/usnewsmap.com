//! Merging tiles (13 §13.4.3): a profile may have the worker cut a page
//! into overlapping tiles, each read as its own page. Each tile's boxes
//! move to page pixels, and a box is kept only when its centre falls in
//! the tile's core: the tile less half the overlap on each inner edge, as
//! the survey's `inside()` does. Adapters never tile or merge tiles.

use crate::model::{Block, Dir, Granularity, Page, Rect};
use crate::order;

/// Where a tile sits in the page, in page pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tile {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Tile {
    /// The tile less half of `overlap` on each edge that isn't the page's.
    pub fn core(&self, page: (f64, f64), overlap: f64) -> Rect {
        let h = overlap / 2.0;
        let x1 = self.x + self.width;
        let y1 = self.y + self.height;
        Rect::new(
            if self.x > 0.0 { self.x + h } else { self.x },
            if self.y > 0.0 { self.y + h } else { self.y },
            if x1 < page.0 { x1 - h } else { x1 },
            if y1 < page.1 { y1 - h } else { y1 },
        )
    }
}

fn inside(core: &Rect, r: &Rect) -> bool {
    let (cx, cy) = r.centre();
    core.x0 <= cx && cx < core.x1 && core.y0 <= cy && cy < core.y1
}

/// One page from its tiles' pages (each in its tile's own pixels, as read).
/// All tiles must share a granularity. Elements are kept at that level by
/// their centre; lines and blocks shrink to what they keep. The merged
/// blocks are put in XY-cut order in `dir`, since tile order isn't reading
/// order.
pub fn merge(page: (f64, f64), overlap: f64, tiles: Vec<(Tile, Page)>, dir: Dir) -> Page {
    let granularity = tiles.first().map_or(Granularity::Word, |t| t.1.granularity);
    let mut blocks: Vec<Block> = Vec::new();
    for (tile, mut p) in tiles {
        p.rescale(tile.width, tile.height);
        let core = tile.core(page, overlap);
        for mut b in p.blocks {
            let shift = |r: Rect| r.map(1.0, 1.0, tile.x, tile.y);
            b.rect = shift(b.rect);
            for l in &mut b.lines {
                l.rect = shift(l.rect);
                for w in &mut l.words {
                    w.rect = shift(w.rect);
                }
            }
            if !b.kind.is_text() || granularity >= Granularity::Block {
                if inside(&core, &b.rect) {
                    blocks.push(b);
                }
                continue;
            }
            if granularity == Granularity::Line {
                b.lines.retain(|l| inside(&core, &l.rect));
            } else {
                for l in &mut b.lines {
                    let n = l.words.len();
                    let last_kept = l.words.last().is_some_and(|w| inside(&core, &w.rect));
                    l.words.retain(|w| inside(&core, &w.rect));
                    if l.words.len() < n {
                        if let Some(r) = Rect::union_all(l.words.iter().map(|w| &w.rect)) {
                            l.rect = r;
                        }
                    }
                    // The hyphen belongs to the line's last word.
                    if !last_kept {
                        l.hyphen = None;
                    }
                }
                b.lines.retain(|l| !l.words.is_empty());
            }
            if let Some(r) = Rect::union_all(b.lines.iter().map(|l| &l.rect)) {
                b.rect = r;
                blocks.push(b);
            }
        }
    }
    let mut out = Page {
        width: page.0,
        height: page.1,
        granularity,
        blocks,
    };
    order::order_blocks(&mut out, dir);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{text, BlockKind, Line, Word};

    fn word(x0: f64, text: &str) -> Word {
        Word {
            rect: Rect::new(x0, 10.0, x0 + 40.0, 30.0),
            text: text.into(),
            conf: None,
        }
    }

    fn tile_page(words: Vec<Word>) -> Page {
        let rect = Rect::union_all(words.iter().map(|w| &w.rect)).unwrap();
        Page {
            width: 200.0,
            height: 100.0,
            granularity: Granularity::Word,
            blocks: vec![Block {
                kind: BlockKind::Text,
                rect,
                lang: None,
                lines: vec![Line {
                    rect,
                    words,
                    hyphen: None,
                }],
            }],
        }
    }

    #[test]
    fn keeps_each_word_once() {
        // A 360 px wide page in two 200 px tiles overlapping by 40 px:
        // cores [0, 180) and [180, 360).
        let a = Tile {
            x: 0.0,
            y: 0.0,
            width: 200.0,
            height: 100.0,
        };
        let b = Tile {
            x: 160.0,
            y: 0.0,
            width: 200.0,
            height: 100.0,
        };
        assert_eq!(
            a.core((360.0, 100.0), 40.0),
            Rect::new(0.0, 0.0, 180.0, 100.0)
        );
        // "three" is in both tiles: centre 170 in tile a, 10 + 160 in b.
        let p = merge(
            (360.0, 100.0),
            40.0,
            vec![
                (
                    a,
                    tile_page(vec![
                        word(0.0, "one"),
                        word(70.0, "two"),
                        word(150.0, "three"),
                    ]),
                ),
                (
                    b,
                    tile_page(vec![
                        word(-10.0, "three"),
                        word(60.0, "four"),
                        word(140.0, "five"),
                    ]),
                ),
            ],
            Dir::Ltr,
        );
        assert_eq!(text(&p), "one two three\n\nfour five");
        assert_eq!(p.blocks[1].rect.x0, 220.0);
    }

    #[test]
    fn a_hyphen_stays_with_its_word() {
        let a = Tile {
            x: 0.0,
            y: 0.0,
            width: 200.0,
            height: 100.0,
        };
        let mut hyphenated = tile_page(vec![word(0.0, "one"), word(150.0, "thr")]);
        hyphenated.blocks[0].lines[0].hyphen = Some("-".into());
        // "thr" (centre 170) is outside the core [0, 140): its hyphen goes too.
        let p = merge(
            (360.0, 100.0),
            120.0,
            vec![(a, hyphenated.clone())],
            Dir::Ltr,
        );
        assert_eq!(text(&p), "one");
        let p = merge((200.0, 100.0), 120.0, vec![(a, hyphenated)], Dir::Ltr);
        assert_eq!(text(&p), "one thr-");
    }
}
