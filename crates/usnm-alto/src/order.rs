//! Reading order (13 §13.4.5): a column-aware XY cut on block boxes for
//! engines that give none, and the order of lines and words by direction.

use crate::model::{Dir, Line, Page, Rect, Word};

/// The order to read `rects` in, as indices: a recursive XY cut that tries
/// a cut between columns first, so the lines of a 6 to 8 column page aren't
/// read across the columns, then a cut between rows (a headline over its
/// columns). Columns run left to right, or right to left for `rtl` and
/// `ttb-rl`; rows run top to bottom. Boxes that no cut separates are read
/// top to bottom, then along the direction.
pub fn xy_cut(rects: &[Rect], dir: Dir) -> Vec<usize> {
    let mut out = Vec::with_capacity(rects.len());
    let ids: Vec<usize> = (0..rects.len()).collect();
    cut(rects, ids, dir, &mut out);
    out
}

fn cut(rects: &[Rect], ids: Vec<usize>, dir: Dir, out: &mut Vec<usize>) {
    if ids.len() <= 1 {
        out.extend(ids);
        return;
    }
    let mut cols = split(rects, &ids, |r| (r.x0, r.x1));
    if cols.len() > 1 {
        if dir != Dir::Ltr {
            cols.reverse();
        }
        for g in cols {
            cut(rects, g, dir, out);
        }
        return;
    }
    let rows = split(rects, &ids, |r| (r.y0, r.y1));
    if rows.len() > 1 {
        for g in rows {
            cut(rects, g, dir, out);
        }
        return;
    }
    let mut ids = ids;
    ids.sort_by(|&a, &b| {
        let (ra, rb) = (&rects[a], &rects[b]);
        let along = match dir {
            Dir::Ltr => ra.x0.total_cmp(&rb.x0),
            Dir::Rtl | Dir::TtbRl => rb.x1.total_cmp(&ra.x1),
        };
        ra.y0.total_cmp(&rb.y0).then(along)
    });
    out.extend(ids);
}

/// `ids` in groups separated by gaps in their projection on one axis, in
/// increasing order along it.
fn split(rects: &[Rect], ids: &[usize], span: impl Fn(&Rect) -> (f64, f64)) -> Vec<Vec<usize>> {
    let mut sorted = ids.to_vec();
    sorted.sort_by(|&a, &b| span(&rects[a]).0.total_cmp(&span(&rects[b]).0));
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut end = f64::NEG_INFINITY;
    for i in sorted {
        let (s, e) = span(&rects[i]);
        match groups.last_mut() {
            Some(g) if s < end => g.push(i),
            _ => groups.push(vec![i]),
        }
        end = end.max(e);
    }
    groups
}

/// Words along the line's direction.
pub fn sort_words(words: &mut [Word], dir: Dir) {
    match dir {
        Dir::Ltr => words.sort_by(|a, b| a.rect.x0.total_cmp(&b.rect.x0)),
        Dir::Rtl => words.sort_by(|a, b| b.rect.x1.total_cmp(&a.rect.x1)),
        Dir::TtbRl => words.sort_by(|a, b| a.rect.y0.total_cmp(&b.rect.y0)),
    }
}

/// Lines across the block: top to bottom, or right to left for vertical
/// lines.
pub fn sort_lines(lines: &mut [Line], dir: Dir) {
    match dir {
        Dir::Ltr | Dir::Rtl => lines.sort_by(|a, b| a.rect.y0.total_cmp(&b.rect.y0)),
        Dir::TtbRl => lines.sort_by(|a, b| b.rect.x1.total_cmp(&a.rect.x1)),
    }
}

/// Puts `page`'s blocks in XY-cut order.
pub fn order_blocks(page: &mut Page, dir: Dir) {
    let rects: Vec<Rect> = page.blocks.iter().map(|b| b.rect).collect();
    let order = xy_cut(&rects, dir);
    let mut old: Vec<Option<crate::model::Block>> = page.blocks.drain(..).map(Some).collect();
    page.blocks = order.into_iter().filter_map(|i| old[i].take()).collect();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x0: f64, y0: f64, x1: f64, y1: f64) -> Rect {
        Rect::new(x0, y0, x1, y1)
    }

    #[test]
    fn reads_columns_down_then_across() {
        // A masthead, then three columns of two blocks each, given in
        // detection order (row by row).
        let rects = [
            r(0.0, 0.0, 300.0, 50.0),
            r(0.0, 100.0, 90.0, 200.0),
            r(100.0, 100.0, 190.0, 180.0),
            r(200.0, 100.0, 300.0, 150.0),
            r(0.0, 210.0, 90.0, 300.0),
            r(100.0, 190.0, 190.0, 300.0),
            r(200.0, 160.0, 300.0, 300.0),
        ];
        assert_eq!(xy_cut(&rects, Dir::Ltr), vec![0, 1, 4, 2, 5, 3, 6]);
        assert_eq!(xy_cut(&rects, Dir::Rtl), vec![0, 3, 6, 2, 5, 1, 4]);
        assert_eq!(xy_cut(&rects, Dir::TtbRl), vec![0, 3, 6, 2, 5, 1, 4]);
    }

    #[test]
    fn a_headline_across_two_columns() {
        let rects = [
            r(0.0, 0.0, 90.0, 100.0),
            r(100.0, 0.0, 190.0, 100.0),
            r(0.0, 110.0, 190.0, 130.0),
            r(0.0, 140.0, 90.0, 300.0),
            r(100.0, 140.0, 190.0, 300.0),
            r(200.0, 0.0, 300.0, 300.0),
        ];
        assert_eq!(xy_cut(&rects, Dir::Ltr), vec![0, 1, 2, 3, 4, 5]);
        // Overlapping boxes: top to bottom.
        let rects = [r(0.0, 50.0, 100.0, 90.0), r(10.0, 0.0, 90.0, 60.0)];
        assert_eq!(xy_cut(&rects, Dir::Ltr), vec![1, 0]);
        assert!(xy_cut(&[], Dir::Ltr).is_empty());
    }
}
