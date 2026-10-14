//! `boxes.json` (`usnm-boxes/1`, 13 §13.4.5): the neutral layout an engine
//! without ALTO writes, its bounded reader, and its conversion to a
//! [`Page`] in pixels of the full image.

use std::cell::RefCell;

use serde::de::{self, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use crate::markup::{self, Markup};
use crate::model::{Block, BlockKind, Dir, Granularity, Line, Page, Rect, Word};
use crate::order;

/// `boxes.json`'s version.
pub const VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Units {
    /// Pixels of `frame`.
    Px,
    /// 0 to 1 of `frame` (docTR).
    Rel,
    /// 0 to 1000 of `frame` (Qwen3-VL, LightOnOCR, Chandra, HunyuanOCR).
    Norm1000,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReadingOrder {
    /// Array order, or `order` on blocks where given.
    Engine,
    /// Detection or tile order: the reader orders blocks with an XY cut.
    None,
}

/// The image the coordinates refer to.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Frame {
    pub width: f64,
    pub height: f64,
}

/// `[x, y]` points of a polygon or baseline, counted as they are read.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Points(pub Vec<[f64; 2]>);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Boxes {
    pub v: u32,
    pub units: Units,
    pub frame: Frame,
    pub granularity: Granularity,
    pub reading_order: ReadingOrder,
    pub blocks: Vec<BoxBlock>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RawBlock")]
pub struct BoxBlock {
    pub bbox: [f64; 4],
    pub kind: BlockKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dir: Option<Dir>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conf: Option<f64>,
    /// The block's text, for a block- or page-level engine.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub lines: Vec<BoxLine>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub polygon: Option<Points>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RawLine")]
pub struct BoxLine {
    pub bbox: [f64; 4],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conf: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dir: Option<Dir>,
    #[serde(default)]
    pub hyphen: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub words: Vec<BoxWord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub polygon: Option<Points>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline: Option<Points>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RawWord")]
pub struct BoxWord {
    pub bbox: [f64; 4],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conf: Option<f64>,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub polygon: Option<Points>,
}

#[derive(Deserialize)]
struct RawBlock {
    bbox: [f64; 4],
    kind: BlockKind,
    #[serde(default)]
    order: Option<u32>,
    #[serde(default)]
    dir: Option<Dir>,
    #[serde(default)]
    lang: Option<String>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    conf: Option<f64>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    lines: Vec<BoxLine>,
    #[serde(default)]
    polygon: Option<Points>,
}

#[derive(Deserialize)]
struct RawLine {
    bbox: [f64; 4],
    #[serde(default)]
    conf: Option<f64>,
    #[serde(default)]
    dir: Option<Dir>,
    #[serde(default)]
    hyphen: bool,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    words: Vec<BoxWord>,
    #[serde(default)]
    polygon: Option<Points>,
    #[serde(default)]
    baseline: Option<Points>,
}

#[derive(Deserialize)]
struct RawWord {
    bbox: [f64; 4],
    #[serde(default)]
    conf: Option<f64>,
    text: String,
    #[serde(default)]
    polygon: Option<Points>,
}

/// What [`parse`] allows (13 §13.4.7 step 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoxLimits {
    pub max_bytes: usize,
    pub max_blocks: usize,
    pub max_lines: usize,
    pub max_words: usize,
    /// Bytes of text in one word, line or block.
    pub max_text: usize,
    pub max_points_per_shape: usize,
    pub max_points: usize,
}

impl Default for BoxLimits {
    fn default() -> Self {
        Self {
            max_bytes: 64 << 20,
            max_blocks: 20_000,
            max_lines: 200_000,
            max_words: 500_000,
            max_text: 1024,
            max_points_per_shape: 1000,
            max_points: 2_000_000,
        }
    }
}

#[derive(Debug, Default)]
struct Counts {
    limits: Option<BoxLimits>,
    blocks: usize,
    lines: usize,
    words: usize,
    points: usize,
}

thread_local! {
    static COUNTS: RefCell<Counts> = RefCell::new(Counts::default());
}

/// Counts one more of something while a file is read, and fails once a
/// limit is passed, so a small file that expands into millions of boxes
/// stops before it is built.
fn bump(what: &str, n: usize) -> Result<(), String> {
    COUNTS.with(|c| {
        let mut c = c.borrow_mut();
        let Some(l) = c.limits else {
            return Ok(());
        };
        let (count, max) = match what {
            "block" => (&mut c.blocks, l.max_blocks),
            "line" => (&mut c.lines, l.max_lines),
            "word" => (&mut c.words, l.max_words),
            _ => (&mut c.points, l.max_points),
        };
        *count += n;
        if *count > max {
            return Err(format!("limit: more than {max} {what}s"));
        }
        Ok(())
    })
}

fn check_text(t: Option<&str>) -> Result<(), String> {
    let max = COUNTS.with(|c| c.borrow().limits.map(|l| l.max_text));
    match (t, max) {
        (Some(t), Some(max)) if t.len() > max => Err(format!("limit: a text over {max} bytes")),
        _ => Ok(()),
    }
}

impl TryFrom<RawWord> for BoxWord {
    type Error = String;
    fn try_from(r: RawWord) -> Result<Self, String> {
        bump("word", 1)?;
        check_text(Some(&r.text))?;
        Ok(BoxWord {
            bbox: r.bbox,
            conf: r.conf,
            text: r.text,
            polygon: r.polygon,
        })
    }
}

impl TryFrom<RawLine> for BoxLine {
    type Error = String;
    fn try_from(r: RawLine) -> Result<Self, String> {
        bump("line", 1)?;
        check_text(r.text.as_deref())?;
        Ok(BoxLine {
            bbox: r.bbox,
            conf: r.conf,
            dir: r.dir,
            hyphen: r.hyphen,
            text: r.text,
            words: r.words,
            polygon: r.polygon,
            baseline: r.baseline,
        })
    }
}

impl TryFrom<RawBlock> for BoxBlock {
    type Error = String;
    fn try_from(r: RawBlock) -> Result<Self, String> {
        bump("block", 1)?;
        check_text(r.text.as_deref())?;
        check_text(r.lang.as_deref())?;
        check_text(r.label.as_deref())?;
        Ok(BoxBlock {
            bbox: r.bbox,
            kind: r.kind,
            order: r.order,
            dir: r.dir,
            lang: r.lang,
            label: r.label,
            conf: r.conf,
            text: r.text,
            lines: r.lines,
            polygon: r.polygon,
        })
    }
}

impl<'de> Deserialize<'de> for Points {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Points;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a list of [x, y] points")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Points, A::Error> {
                let per_shape = COUNTS
                    .with(|c| c.borrow().limits.map(|l| l.max_points_per_shape))
                    .unwrap_or(usize::MAX);
                let mut out = Vec::new();
                while let Some(p) = seq.next_element::<[f64; 2]>()? {
                    if out.len() >= per_shape {
                        return Err(de::Error::custom(format!(
                            "limit: more than {per_shape} points in a shape"
                        )));
                    }
                    bump("point", 1).map_err(de::Error::custom)?;
                    out.push(p);
                }
                Ok(Points(out))
            }
        }
        d.deserialize_seq(V)
    }
}

/// Why a `boxes.json` was refused. `Limit` is the worker's
/// `engine_protocol`; the others are `convert` (13 §13.4.6).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum BoxesError {
    #[error("boxes.json breaks a limit: {0}")]
    Limit(String),
    #[error("boxes.json doesn't parse: {0}")]
    Parse(String),
    #[error("boxes.json is version {0}, not {VERSION}")]
    Version(u32),
    #[error("boxes.json declares granularity {found:?}, the engine {expected:?}")]
    Granularity {
        found: Granularity,
        expected: Granularity,
    },
    #[error("boxes.json has a bad frame or box: {0}")]
    Geometry(String),
}

impl BoxesError {
    /// Whether the worker treats this as `engine_protocol` (a limit or the
    /// declared granularity broken) rather than `convert`.
    pub fn is_protocol(&self) -> bool {
        matches!(self, BoxesError::Limit(_) | BoxesError::Granularity { .. })
    }
}

/// Reads `boxes.json` within `limits`.
pub fn parse(json: &[u8], limits: &BoxLimits) -> Result<Boxes, BoxesError> {
    if json.len() > limits.max_bytes {
        return Err(BoxesError::Limit(format!(
            "{} bytes, over {}",
            json.len(),
            limits.max_bytes
        )));
    }
    COUNTS.with(|c| {
        *c.borrow_mut() = Counts {
            limits: Some(*limits),
            ..Counts::default()
        }
    });
    // serde_json checks UTF-8 and caps nesting at 128.
    let out = serde_json::from_slice::<Boxes>(json);
    COUNTS.with(|c| *c.borrow_mut() = Counts::default());
    out.map_err(|e| {
        let msg = e.to_string();
        if msg.starts_with("limit: ") || msg.contains("recursion limit") {
            BoxesError::Limit(msg)
        } else {
            BoxesError::Parse(msg)
        }
    })
}

/// What [`to_page`] needs besides the boxes.
#[derive(Debug, Clone, Copy)]
pub struct Convert {
    /// The granularity the engine's `hello` declared.
    pub granularity: Granularity,
    /// The size of the full image the stored ALTO describes (13 §13.8.2).
    pub full: (f64, f64),
    /// The page's direction when a block or line gives none
    /// ([`crate::default_dir`]).
    pub dir: Dir,
    pub markup: Markup,
}

/// How far outside the frame a box may reach before it is refused; boxes
/// within it are clamped.
pub const CLAMP_PX: f64 = 2.0;

/// The boxes as a page in pixels of the full image, in reading order, with
/// markup stripped and text at the finest level present.
pub fn to_page(b: &Boxes, c: &Convert) -> Result<Page, BoxesError> {
    if b.v != VERSION {
        return Err(BoxesError::Version(b.v));
    }
    if b.granularity != c.granularity {
        return Err(BoxesError::Granularity {
            found: b.granularity,
            expected: c.granularity,
        });
    }
    let (fw, fh) = (b.frame.width, b.frame.height);
    let (gw, gh) = c.full;
    let positive = |x: f64| x.is_finite() && x > 0.0;
    if !(positive(fw) && positive(fh) && positive(gw) && positive(gh)) {
        return Err(BoxesError::Geometry("frame or image size".into()));
    }
    let unit = match b.units {
        Units::Px => (1.0, 1.0),
        Units::Rel => (fw, fh),
        Units::Norm1000 => (fw / 1000.0, fh / 1000.0),
    };
    // Units to frame pixels, clamped, then to the full image's pixels.
    let rect = |bb: &[f64; 4]| -> Result<Rect, BoxesError> {
        let [x0, y0, x1, y1] = *bb;
        let r = Rect::new(x0 * unit.0, y0 * unit.1, x1 * unit.0, y1 * unit.1);
        let ok = [r.x0, r.y0, r.x1, r.y1].iter().all(|v| v.is_finite())
            && r.x1 > r.x0
            && r.y1 > r.y0
            && r.x0 >= -CLAMP_PX
            && r.y0 >= -CLAMP_PX
            && r.x1 <= fw + CLAMP_PX
            && r.y1 <= fh + CLAMP_PX;
        if !ok {
            return Err(BoxesError::Geometry(format!(
                "box {bb:?} in a {fw}x{fh} frame"
            )));
        }
        let r = Rect::new(r.x0.max(0.0), r.y0.max(0.0), r.x1.min(fw), r.y1.min(fh));
        if r.x1 <= r.x0 || r.y1 <= r.y0 {
            return Err(BoxesError::Geometry(format!(
                "box {bb:?} has no area in the frame"
            )));
        }
        Ok(r.map(gw / fw, gh / fh, 0.0, 0.0))
    };
    let g = c.granularity;
    let mut blocks = Vec::with_capacity(b.blocks.len());
    for (i, bb) in b.blocks.iter().enumerate() {
        let brect = rect(&bb.bbox)?;
        let dir = bb.dir.unwrap_or(c.dir);
        if !bb.kind.is_text() {
            // Text on an illustration is never indexed.
            blocks.push((
                i,
                bb.order,
                Block {
                    kind: bb.kind,
                    rect: brect,
                    lang: None,
                    lines: Vec::new(),
                },
            ));
            continue;
        }
        let has_lines = bb.lines.iter().any(|l| {
            l.text.as_deref().is_some_and(|t| !t.trim().is_empty())
                || l.words.iter().any(|w| !w.text.trim().is_empty())
        });
        let block = if has_lines {
            let mut lines = Vec::new();
            for bl in &bb.lines {
                let lrect = if g <= Granularity::Line {
                    rect(&bl.bbox)?
                } else {
                    brect
                };
                let ldir = bl.dir.unwrap_or(dir);
                let mut words = Vec::new();
                if bl.words.iter().any(|w| !w.text.trim().is_empty()) {
                    for bw in &bl.words {
                        let wrect = if g == Granularity::Word {
                            rect(&bw.bbox)?
                        } else {
                            lrect
                        };
                        let t = markup::strip(&bw.text, c.markup);
                        // A word whose markup held a space is still one box.
                        for part in t.split_whitespace() {
                            words.push(Word {
                                rect: wrect,
                                text: part.to_owned(),
                                conf: bw.conf.map(|x| x.clamp(0.0, 1.0)),
                            });
                        }
                    }
                } else if let Some(t) = &bl.text {
                    let t = markup::strip(t, c.markup);
                    words = split_words(&t, lrect, bl.conf);
                }
                if g == Granularity::Word {
                    order::sort_words(&mut words, ldir);
                }
                if words.is_empty() {
                    continue;
                }
                let hyphen = bl.hyphen.then(|| take_hyphen(&mut words));
                lines.push(Line {
                    rect: lrect,
                    words,
                    hyphen,
                });
            }
            if g <= Granularity::Line {
                order::sort_lines(&mut lines, dir);
            }
            (!lines.is_empty()).then(|| Block {
                kind: bb.kind,
                rect: brect,
                lang: bb.lang.clone(),
                lines,
            })
        } else {
            let t = markup::strip(bb.text.as_deref().unwrap_or(""), c.markup);
            text_block(bb.kind, brect, bb.lang.clone(), &t, bb.conf)
        };
        if let Some(block) = block {
            blocks.push((i, bb.order, block));
        }
    }
    let mut page = Page {
        width: gw,
        height: gh,
        granularity: g,
        blocks: Vec::new(),
    };
    match b.reading_order {
        ReadingOrder::Engine => {
            // `order` where given, else array order; blocks without one go
            // after those with one.
            blocks.sort_by_key(|(i, o, _)| (o.is_none(), o.unwrap_or(0), *i));
            page.blocks = blocks.into_iter().map(|(_, _, b)| b).collect();
        }
        ReadingOrder::None => {
            page.blocks = blocks.into_iter().map(|(_, _, b)| b).collect();
            order::order_blocks(&mut page, c.dir);
        }
    }
    Ok(page)
}

/// Words of `text`, each with the box `rect` (the box of the line or block
/// they came from).
fn split_words(text: &str, rect: Rect, conf: Option<f64>) -> Vec<Word> {
    text.split_whitespace()
        .map(|w| Word {
            rect,
            text: w.to_owned(),
            conf: conf.map(|x| x.clamp(0.0, 1.0)),
        })
        .collect()
}

/// A text block from text alone: a line per line of `text`, every line and
/// word with the block's box. None when the text is empty.
pub(crate) fn text_block(
    kind: BlockKind,
    rect: Rect,
    lang: Option<String>,
    text: &str,
    conf: Option<f64>,
) -> Option<Block> {
    let lines: Vec<Line> = text
        .lines()
        .map(|l| split_words(l, rect, conf))
        .filter(|w| !w.is_empty())
        .map(|words| Line {
            rect,
            words,
            hyphen: None,
        })
        .collect();
    (!lines.is_empty()).then_some(Block {
        kind,
        rect,
        lang,
        lines,
    })
}

const HYPHENS: [char; 5] = ['-', '\u{00AC}', '\u{2010}', '\u{2E17}', '\u{00AD}'];

/// The hyphen at the end of a hyphenated line's last word, moved off the
/// word (ALTO's `HYP`); `-` when the engine dropped it.
fn take_hyphen(words: &mut [Word]) -> String {
    if let Some(last) = words.last_mut() {
        if let Some(c) = last.text.chars().last().filter(|c| HYPHENS.contains(c)) {
            if last.text.chars().count() > 1 {
                last.text.pop();
                return if c == '\u{00AD}' {
                    "-".into()
                } else {
                    c.to_string()
                };
            }
        }
    }
    "-".into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::text;

    const WORDS: &str = r#"{
      "blocks": [
        {"bbox": [120, 340, 1610, 2200], "conf": 0.9, "dir": "ltr", "kind": "text", "label": "article", "lang": "deu", "lines": [
          {"bbox": [120, 340, 1610, 395], "conf": 0.87, "hyphen": true, "text": "Deutsche Zei-", "words": [
            {"bbox": [500, 342, 700, 395], "conf": 0.91, "text": "Zei-"},
            {"bbox": [120, 342, 410, 395], "conf": 0.91, "text": "Deutsche"}
          ]},
          {"bbox": [120, 400, 1610, 455], "words": [{"bbox": [120, 400, 400, 455], "text": "tung"}]}
        ], "order": 1},
        {"bbox": [1700, 300, 3400, 1900], "kind": "illustration", "order": 0, "text": "a picture of a ship"}
      ],
      "frame": {"height": 7208, "width": 5412},
      "granularity": "word",
      "reading_order": "engine",
      "units": "px",
      "unknown_field": true,
      "v": 1
    }"#;

    fn conv(g: Granularity) -> Convert {
        Convert {
            granularity: g,
            full: (5412.0, 7208.0),
            dir: Dir::Ltr,
            markup: Markup::None,
        }
    }

    #[test]
    fn word_boxes_become_a_page() {
        let b = parse(WORDS.as_bytes(), &BoxLimits::default()).unwrap();
        let p = to_page(&b, &conv(Granularity::Word)).unwrap();
        assert_eq!(p.blocks[0].kind, BlockKind::Illustration);
        assert_eq!(p.blocks[1].lang.as_deref(), Some("deu"));
        let l = &p.blocks[1].lines[0];
        assert_eq!(l.words[0].text, "Deutsche", "words in direction order");
        assert_eq!(l.words[1].text, "Zei");
        assert_eq!(l.hyphen.as_deref(), Some("-"));
        assert_eq!(text(&p), "Deutsche Zei-\ntung");
        // The declared granularity must match the engine's.
        assert!(matches!(
            to_page(&b, &conv(Granularity::Line)),
            Err(BoxesError::Granularity { .. })
        ));
    }

    #[test]
    fn units_scale_to_the_full_image() {
        let json = r#"{"v":1,"units":"rel","frame":{"width":1000,"height":500},"granularity":"line","reading_order":"engine",
          "blocks":[{"bbox":[0.1,0.1,0.5,0.5],"kind":"text","lines":[{"bbox":[0.1,0.1,0.5,0.2],"text":"**Hello** world","words":[]}]}]}"#;
        let b = parse(json.as_bytes(), &BoxLimits::default()).unwrap();
        let c = Convert {
            full: (2000.0, 1000.0),
            markup: Markup::Markdown,
            ..conv(Granularity::Line)
        };
        let p = to_page(&b, &c).unwrap();
        let l = &p.blocks[0].lines[0];
        assert_eq!(l.rect, Rect::new(200.0, 100.0, 1000.0, 200.0));
        assert_eq!(l.words[0].rect, l.rect, "words inherit the line's box");
        assert_eq!(text(&p), "Hello world");
        let n = json
            .replace("\"rel\"", "\"norm1000\"")
            .replace("0.1,0.1,0.5,0.5", "100,100,500,500")
            .replace("0.1,0.1,0.5,0.2", "100,100,500,200");
        let p2 = to_page(&parse(n.as_bytes(), &BoxLimits::default()).unwrap(), &c).unwrap();
        assert_eq!(p2.blocks[0].lines[0].rect, l.rect);
    }

    #[test]
    fn block_text_and_page_text() {
        let json = r#"{"v":1,"units":"px","frame":{"width":100,"height":100},"granularity":"page","reading_order":"engine",
          "blocks":[{"bbox":[0,0,100,100],"kind":"text","text":"<p>Erste Zeile</p>Zweite  Zeile"}]}"#;
        let b = parse(json.as_bytes(), &BoxLimits::default()).unwrap();
        let c = Convert {
            full: (100.0, 100.0),
            markup: Markup::Html,
            ..conv(Granularity::Page)
        };
        let p = to_page(&b, &c).unwrap();
        assert_eq!(text(&p), "Erste Zeile\nZweite Zeile");
        assert_eq!(p, {
            let mut t = Page::text_only(100.0, 100.0, "Erste Zeile\nZweite Zeile");
            t.granularity = Granularity::Page;
            t
        });
    }

    #[test]
    fn detection_order_is_cut_into_columns() {
        let json = r#"{"v":1,"units":"px","frame":{"width":300,"height":300},"granularity":"block","reading_order":"none",
          "blocks":[{"bbox":[150,0,300,100],"kind":"text","text":"c"},{"bbox":[0,150,140,300],"kind":"text","text":"b"},
                    {"bbox":[0,0,140,100],"kind":"text","text":"a"},{"bbox":[150,150,300,300],"kind":"text","text":"d"}]}"#;
        let b = parse(json.as_bytes(), &BoxLimits::default()).unwrap();
        let c = Convert {
            full: (300.0, 300.0),
            ..conv(Granularity::Block)
        };
        assert_eq!(text(&to_page(&b, &c).unwrap()), "a\n\nb\n\nc\n\nd");
        let rtl = Convert { dir: Dir::Rtl, ..c };
        assert_eq!(text(&to_page(&b, &rtl).unwrap()), "c\n\nd\n\na\n\nb");
    }

    #[test]
    fn refuses_bad_boxes() {
        let base = r#"{"v":1,"units":"px","frame":{"width":100,"height":100},"granularity":"block","reading_order":"engine","blocks":[{"bbox":BOX,"kind":"text","text":"x"}]}"#;
        let c = Convert {
            full: (100.0, 100.0),
            ..conv(Granularity::Block)
        };
        let page = |bbox: &str| {
            let j = base.replace("BOX", bbox);
            to_page(&parse(j.as_bytes(), &BoxLimits::default()).unwrap(), &c)
        };
        // Within 2 px: clamped.
        assert_eq!(
            page("[-2,0,102,50]").unwrap().blocks[0].rect,
            Rect::new(0.0, 0.0, 100.0, 50.0)
        );
        assert!(matches!(page("[-3,0,50,50]"), Err(BoxesError::Geometry(_))));
        assert!(matches!(
            page("[10,10,10,50]"),
            Err(BoxesError::Geometry(_))
        ));
        assert!(matches!(page("[50,0,10,50]"), Err(BoxesError::Geometry(_))));
        assert!(matches!(
            parse(
                base.replace("BOX", "[1,2]").as_bytes(),
                &BoxLimits::default()
            ),
            Err(BoxesError::Parse(_))
        ));
        let v2 = base
            .replace("BOX", "[0,0,1,1]")
            .replace("\"v\":1", "\"v\":2");
        assert!(matches!(
            to_page(&parse(v2.as_bytes(), &BoxLimits::default()).unwrap(), &c),
            Err(BoxesError::Version(2))
        ));
    }

    #[test]
    fn limits_stop_the_read() {
        let words: Vec<String> = (0..20)
            .map(|i| format!(r#"{{"bbox":[{i},0,{},1],"text":"w"}}"#, i + 1))
            .collect();
        let json = format!(
            r#"{{"v":1,"units":"px","frame":{{"width":100,"height":100}},"granularity":"word","reading_order":"engine","blocks":[{{"bbox":[0,0,100,100],"kind":"text","lines":[{{"bbox":[0,0,100,1],"words":[{}],"polygon":[[0,0],[1,1],[2,2]]}}]}}]}}"#,
            words.join(",")
        );
        let ok = parse(json.as_bytes(), &BoxLimits::default()).unwrap();
        assert_eq!(ok.blocks[0].lines[0].words.len(), 20);
        let l = |f: fn(&mut BoxLimits)| {
            let mut lim = BoxLimits::default();
            f(&mut lim);
            parse(json.as_bytes(), &lim)
        };
        assert!(matches!(l(|x| x.max_words = 19), Err(BoxesError::Limit(_))));
        assert!(matches!(
            l(|x| x.max_points_per_shape = 2),
            Err(BoxesError::Limit(_))
        ));
        assert!(matches!(l(|x| x.max_points = 2), Err(BoxesError::Limit(_))));
        assert!(matches!(l(|x| x.max_bytes = 10), Err(BoxesError::Limit(_))));
        assert!(matches!(
            parse(
                json.replace("\"w\"", "\"wwww\"").as_bytes(),
                &BoxLimits {
                    max_text: 3,
                    ..BoxLimits::default()
                }
            ),
            Err(BoxesError::Limit(_))
        ));
        let deep = format!("{}{}", "[".repeat(200), "]".repeat(200));
        assert!(parse(deep.as_bytes(), &BoxLimits::default()).is_err());
        assert!(parse(&[0xff, b'{'], &BoxLimits::default()).is_err());
        // A boxes.json round-trips through its own types.
        let again = serde_json::to_vec(&ok).unwrap();
        assert_eq!(parse(&again, &BoxLimits::default()).unwrap(), ok);
    }
}
