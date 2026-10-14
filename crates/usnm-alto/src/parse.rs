//! Engine ALTO (v2, v3 or v4, in `pixel`, `inch1200` or `mm10`) read into a
//! [`Page`] in pixels (13 §13.4.7 step 2). The file first passes
//! [`crate::check`]: no DOCTYPE, no entities, within depth, element and
//! attribute limits. Only values are taken from it; nothing of its XML is
//! kept but the `CONTENT` strings.

use quick_xml::events::{BytesStart, Event};
use quick_xml::name::{Namespace, ResolveResult};
use quick_xml::NsReader;

use crate::model::{Block, BlockKind, Granularity, Line, Page, Rect, Word};
use crate::{check, AltoError, Limits};

/// What [`parse`] allows besides [`Limits`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ParseOptions {
    pub limits: Limits,
    /// Bytes in one `CONTENT`.
    pub max_content: usize,
    /// `String` elements on the page.
    pub max_strings: usize,
    /// The image's dpi, needed for `inch1200` and `mm10`: the adapter's
    /// `alto_dpi`, or the dpi the worker sent.
    pub dpi: Option<f64>,
}

impl Default for ParseOptions {
    fn default() -> Self {
        Self {
            limits: Limits {
                max_bytes: 64 << 20,
                ..Limits::default()
            },
            max_content: 1024,
            max_strings: 500_000,
            dpi: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ParseError {
    #[error("ALTO refused: {0}")]
    Alto(#[from] AltoError),
    #[error("ALTO in {0} needs the image's dpi")]
    NeedsDpi(String),
    #[error("ALTO MeasurementUnit `{0}` isn't pixel, inch1200 or mm10")]
    Unit(String),
    #[error("ALTO has more than one Page")]
    Pages,
    #[error("ALTO breaks a limit: {0}")]
    Limit(&'static str),
    #[error("ALTO has a bad value: {0}")]
    Value(String),
}

/// An attribute's value by local name, unescaped.
fn attr(e: &BytesStart<'_>, name: &str) -> Result<Option<String>, ParseError> {
    for a in e.attributes().with_checks(true) {
        let a = a.map_err(|_| AltoError::Malformed)?;
        if a.key.as_ref() == name {
            let v = a
                .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .map_err(|_| AltoError::Malformed)?;
            return Ok(Some(v.into_owned()));
        }
    }
    Ok(None)
}

fn num(e: &BytesStart<'_>, name: &str) -> Result<Option<f64>, ParseError> {
    match attr(e, name)? {
        None => Ok(None),
        Some(v) => v
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|x| x.is_finite())
            .map(Some)
            .ok_or_else(|| ParseError::Value(format!("{name}={v:?}"))),
    }
}

/// `HPOS`, `VPOS`, `WIDTH`, `HEIGHT` as a box in file units, when all four
/// are there and the size isn't negative.
fn rect(e: &BytesStart<'_>) -> Result<Option<Rect>, ParseError> {
    let (h, v, w, ht) = (
        num(e, "HPOS")?,
        num(e, "VPOS")?,
        num(e, "WIDTH")?,
        num(e, "HEIGHT")?,
    );
    Ok(match (h, v, w, ht) {
        (Some(h), Some(v), Some(w), Some(ht)) if w >= 0.0 && ht >= 0.0 => {
            Some(Rect::from_alto(h, v, w, ht))
        }
        _ => None,
    })
}

#[derive(Default)]
struct OpenLine {
    rect: Option<Rect>,
    words: Vec<RawWord>,
    hyphen: Option<String>,
}

/// A String as read: its box, if it had one, its text and confidence.
type RawWord = (Option<Rect>, String, Option<f64>);
/// A TextLine as read: its box, its Strings and its hyphen.
type RawLine = (Option<Rect>, Vec<RawWord>, Option<String>);

struct OpenBlock {
    rect: Option<Rect>,
    lines: Vec<RawLine>,
}

/// Reads ALTO into a page in pixels, in document order (the engine's
/// reading order). `ComposedBlock`s are flattened into their TextBlocks.
/// LANG attributes are ignored: the stored ALTO's languages come from the
/// page (13 §13.8.2). A String, line or block without a full box inherits
/// its parent's; the page's granularity is the finest level at which every
/// element had its own.
pub fn parse(xml: &[u8], opts: &ParseOptions) -> Result<Page, ParseError> {
    let version = check(xml, &opts.limits)?.version;
    // Only elements in the document's own ALTO namespace count: ALTO
    // allows foreign XML (under `Tags/XmlData`), whose `Page` or
    // `MeasurementUnit` isn't the page's.
    let ns_uri = crate::NAMESPACES
        .iter()
        .find(|(_, v)| *v == version)
        .map(|(n, _)| *n)
        .ok_or(AltoError::NotAlto)?;
    let ours =
        |ns: &ResolveResult<'_>| matches!(ns, ResolveResult::Bound(Namespace(u)) if *u == ns_uri);
    let text = std::str::from_utf8(xml).map_err(|_| AltoError::NotUtf8)?;
    let mut reader = NsReader::from_str(text);
    reader.config_mut().trim_text(true);
    let mut unit: Option<String> = None;
    let mut in_unit = false;
    let mut page: Option<(f64, f64)> = None;
    let mut pages = 0usize;
    let mut raw: Vec<(BlockKind, Option<Rect>, Option<OpenBlock>)> = Vec::new();
    let mut block: Option<OpenBlock> = None;
    let mut line: Option<OpenLine> = None;
    let mut strings = 0usize;
    loop {
        let (ns, event) = reader
            .read_resolved_event()
            .map_err(|_| AltoError::Malformed)?;
        let empty = matches!(event, Event::Empty(_));
        let ours = ours(&ns);
        match &event {
            Event::Start(e) | Event::Empty(e) if ours => {
                let name = e.local_name();
                match name.as_ref() {
                    "MeasurementUnit" => in_unit = !empty,
                    "Page" => {
                        pages += 1;
                        if pages > 1 {
                            return Err(ParseError::Pages);
                        }
                        let (w, h) = (num(e, "WIDTH")?, num(e, "HEIGHT")?);
                        page = w.zip(h);
                    }
                    "TextBlock" => {
                        let ob = OpenBlock {
                            rect: rect(e)?,
                            lines: Vec::new(),
                        };
                        // A TextBlock inside another (not valid ALTO) ends
                        // the outer one, so no line goes to the wrong block.
                        if let Some(open) = block.take() {
                            raw.push((BlockKind::Text, open.rect, Some(open)));
                        }
                        line = None;
                        if empty {
                            raw.push((BlockKind::Text, ob.rect, Some(ob)));
                        } else {
                            block = Some(ob);
                        }
                    }
                    "TextLine" if block.is_some() => {
                        let ol = OpenLine {
                            rect: rect(e)?,
                            ..OpenLine::default()
                        };
                        if !empty {
                            line = Some(ol);
                        }
                    }
                    "String" => {
                        if let Some(l) = line.as_mut() {
                            strings += 1;
                            if strings > opts.max_strings {
                                return Err(ParseError::Limit("strings per page"));
                            }
                            let content = attr(e, "CONTENT")?.unwrap_or_default();
                            if content.len() > opts.max_content {
                                return Err(ParseError::Limit("CONTENT length"));
                            }
                            let wc = num(e, "WC")?.map(|c| c.clamp(0.0, 1.0));
                            l.words.push((rect(e)?, content, wc));
                        }
                    }
                    "HYP" => {
                        if let Some(l) = line.as_mut() {
                            let c = attr(e, "CONTENT")?.unwrap_or_else(|| "-".into());
                            if c.len() > opts.max_content {
                                return Err(ParseError::Limit("CONTENT length"));
                            }
                            l.hyphen = Some(c);
                        }
                    }
                    "Illustration" => raw.push((BlockKind::Illustration, rect(e)?, None)),
                    "GraphicalElement" => raw.push((BlockKind::Separator, rect(e)?, None)),
                    _ => {}
                }
            }
            Event::End(e) if ours => match e.local_name().as_ref() {
                "MeasurementUnit" => in_unit = false,
                "TextLine" => {
                    if let (Some(l), Some(b)) = (line.take(), block.as_mut()) {
                        b.lines.push((l.rect, l.words, l.hyphen));
                    }
                }
                "TextBlock" => {
                    if let Some(b) = block.take() {
                        raw.push((BlockKind::Text, b.rect, Some(b)));
                    }
                }
                _ => {}
            },
            Event::Text(t) if in_unit => {
                let s = t.xml_content(quick_xml::XmlVersion::Implicit1_0);
                unit = Some(s.trim().to_owned());
            }
            Event::Eof => break,
            _ => {}
        }
    }
    let unit = unit.unwrap_or_else(|| "pixel".into());
    if let Some(d) = opts.dpi.filter(|d| !(d.is_finite() && *d > 0.0)) {
        return Err(ParseError::Value(format!("dpi {d}")));
    }
    let scale = match unit.as_str() {
        "pixel" => 1.0,
        "inch1200" => opts.dpi.ok_or_else(|| ParseError::NeedsDpi(unit.clone()))? / 1200.0,
        "mm10" => opts.dpi.ok_or_else(|| ParseError::NeedsDpi(unit.clone()))? / 254.0,
        other => return Err(ParseError::Unit(other.to_owned())),
    };
    let (pw, ph) = page.ok_or(AltoError::NoPage)?;
    let page_rect = Rect::new(0.0, 0.0, pw * scale, ph * scale);
    // Boxes in pixels, clamped to the page: an engine's box past the
    // page's edge would make the stored ALTO describe pixels that aren't
    // there.
    let to_px = |r: Rect| {
        let r = r.map(scale, scale, 0.0, 0.0);
        let x0 = r.x0.clamp(0.0, page_rect.x1);
        let y0 = r.y0.clamp(0.0, page_rect.y1);
        Rect::new(
            x0,
            y0,
            r.x1.clamp(x0, page_rect.x1),
            r.y1.clamp(y0, page_rect.y1),
        )
    };
    // Whether every element at a level had its own box.
    let (mut all_words, mut all_lines, mut all_blocks) = (true, true, true);
    let mut blocks = Vec::new();
    for (kind, r, open) in raw {
        all_blocks &= r.is_some()
            || open
                .as_ref()
                .is_some_and(|b| b.lines.iter().any(|l| l.0.is_some()));
        let mut lines = Vec::new();
        for (lr, words, hyphen) in open.map(|b| b.lines).unwrap_or_default() {
            all_lines &= lr.is_some() || words.iter().any(|w| w.0.is_some());
            let mut out = Vec::new();
            for (wr, text, conf) in words {
                all_words &= wr.is_some();
                out.push((wr.map(to_px), text, conf));
            }
            let lrect = lr
                .map(to_px)
                .or_else(|| Rect::union_all(out.iter().filter_map(|w| w.0.as_ref())));
            lines.push((lrect, out, hyphen));
        }
        let brect = r
            .map(to_px)
            .or_else(|| Rect::union_all(lines.iter().filter_map(|l| l.0.as_ref())))
            .unwrap_or(page_rect);
        let lines: Vec<Line> = lines
            .into_iter()
            .map(|(lr, words, hyphen)| {
                let lrect = lr.unwrap_or(brect);
                Line {
                    rect: lrect,
                    words: words
                        .into_iter()
                        .map(|(wr, text, conf)| Word {
                            rect: wr.unwrap_or(lrect),
                            text,
                            conf,
                        })
                        .collect(),
                    hyphen,
                }
            })
            .collect();
        blocks.push(Block {
            kind,
            rect: brect,
            lang: None,
            lines,
        });
    }
    let granularity = if all_words {
        Granularity::Word
    } else if all_lines {
        Granularity::Line
    } else if all_blocks {
        Granularity::Block
    } else {
        Granularity::Page
    };
    Ok(Page {
        width: page_rect.x1,
        height: page_rect.y1,
        granularity,
        blocks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::text;

    const KRAKEN: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<alto xmlns="http://www.loc.gov/standards/alto/ns-v4#">
  <Description><MeasurementUnit>inch1200</MeasurementUnit></Description>
  <Layout><Page ID="p" WIDTH="2400" HEIGHT="3600" PHYSICAL_IMG_NR="1"><PrintSpace>
    <ComposedBlock ID="c"><TextBlock ID="b" HPOS="120" VPOS="120" WIDTH="1200" HEIGHT="400" LANG="eng">
      <TextLine ID="l" HPOS="120" VPOS="120" WIDTH="1200" HEIGHT="100">
        <Shape><Polygon POINTS="0 0 1 1"/></Shape>
        <String CONTENT="Ein&amp;" HPOS="120" VPOS="120" WIDTH="400" HEIGHT="100" WC="0.5"/><SP/>
        <String CONTENT="Wort" HPOS="600" VPOS="120" WIDTH="400" HEIGHT="100"/>
        <HYP CONTENT="¬"/>
      </TextLine>
      <TextLine ID="l2"><String CONTENT="ende"/></TextLine>
    </TextBlock></ComposedBlock>
    <Illustration ID="i" HPOS="0" VPOS="2400" WIDTH="2400" HEIGHT="1200"/>
  </PrintSpace></Page></Layout>
</alto>"#;

    #[test]
    fn reads_units_and_structure() {
        assert!(matches!(
            parse(KRAKEN.as_bytes(), &ParseOptions::default()),
            Err(ParseError::NeedsDpi(_))
        ));
        let opts = ParseOptions {
            dpi: Some(300.0),
            ..ParseOptions::default()
        };
        let p = parse(KRAKEN.as_bytes(), &opts).unwrap();
        assert_eq!((p.width, p.height), (600.0, 900.0));
        assert_eq!(
            p.granularity,
            Granularity::Block,
            "a line with no box of its own"
        );
        assert_eq!(p.blocks.len(), 2);
        let b = &p.blocks[0];
        assert_eq!(b.rect, Rect::new(30.0, 30.0, 330.0, 130.0));
        assert_eq!(b.lang, None, "LANG in engine ALTO is ignored");
        assert_eq!(b.lines[0].words[0].text, "Ein&");
        assert_eq!(b.lines[0].words[0].conf, Some(0.5));
        assert_eq!(b.lines[1].words[0].rect, b.rect, "inherits the block's box");
        assert_eq!(p.blocks[1].kind, BlockKind::Illustration);
        assert_eq!(text(&p), "Ein& Wort¬\nende");
        let mm = KRAKEN.replace("inch1200", "mm10");
        let p = parse(mm.as_bytes(), &opts).unwrap();
        assert!((p.width - 2400.0 * 300.0 / 254.0).abs() < 1e-9);
    }

    #[test]
    fn refuses_bad_alto() {
        let opts = ParseOptions {
            dpi: Some(300.0),
            ..ParseOptions::default()
        };
        let err = |x: &str, o: &ParseOptions| parse(x.as_bytes(), o).unwrap_err();
        assert_eq!(
            err(&KRAKEN.replace("inch1200", "furlong"), &opts),
            ParseError::Unit("furlong".into())
        );
        assert_eq!(
            err(&KRAKEN.replace("</PrintSpace></Page>", "</PrintSpace></Page><Page ID=\"q\" WIDTH=\"1\" HEIGHT=\"1\" PHYSICAL_IMG_NR=\"2\"/>"), &opts),
            ParseError::Pages
        );
        assert!(matches!(
            err(&KRAKEN.replace("WC=\"0.5\"", "WC=\"x\""), &opts),
            ParseError::Value(_)
        ));
        let few = ParseOptions {
            max_strings: 2,
            ..opts
        };
        assert_eq!(err(KRAKEN, &few), ParseError::Limit("strings per page"));
        let short = ParseOptions {
            max_content: 3,
            ..opts
        };
        assert_eq!(err(KRAKEN, &short), ParseError::Limit("CONTENT length"));
        assert!(matches!(
            err("<x/>", &opts),
            ParseError::Alto(AltoError::NotAlto)
        ));
        let zero = ParseOptions {
            dpi: Some(0.0),
            ..opts
        };
        assert!(matches!(err(KRAKEN, &zero), ParseError::Value(_)));
        let nan = ParseOptions {
            dpi: Some(f64::NAN),
            ..opts
        };
        assert!(matches!(err(KRAKEN, &nan), ParseError::Value(_)));
        // Foreign XML with ALTO's element names is not the page.
        let foreign = KRAKEN.replace(
            "<Layout>",
            "<Tags><OtherTag ID=\"t\"><XmlData><x:Page xmlns:x=\"urn:x\" WIDTH=\"9\" HEIGHT=\"9\"/><x:MeasurementUnit xmlns:x=\"urn:x\">pixel</x:MeasurementUnit></XmlData></OtherTag></Tags><Layout>",
        );
        assert_eq!(parse(foreign.as_bytes(), &opts).unwrap().width, 600.0);
        // Boxes past the page are clamped to it.
        let wide = KRAKEN.replace(
            "HPOS=\"600\" VPOS=\"120\" WIDTH=\"400\"",
            "HPOS=\"2000\" VPOS=\"120\" WIDTH=\"4000\"",
        );
        let p = parse(wide.as_bytes(), &opts).unwrap();
        assert_eq!(p.blocks[0].lines[0].words[1].rect.x1, 600.0);
        // A TextBlock inside another ends it.
        let nested = KRAKEN
            .replace(
                "<TextLine ID=\"l2\">",
                "<TextBlock ID=\"inner\"><TextLine ID=\"l2\">",
            )
            .replace(
                "</TextBlock></ComposedBlock>",
                "</TextBlock></TextBlock></ComposedBlock>",
            );
        let p = parse(nested.as_bytes(), &opts).unwrap();
        assert_eq!(text(&p), "Ein& Wort¬\n\nende");
    }
}
