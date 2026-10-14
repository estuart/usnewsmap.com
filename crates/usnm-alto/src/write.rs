//! The stored ALTO profile (13 §13.8.2): ALTO 3.1 in pixels of the IIIF
//! full image, written from parsed values, never copied from an engine's
//! XML.

use std::fmt::Write as _;

use quick_xml::escape::escape;

use crate::model::{BlockKind, Granularity, Page, Rect, Word};

pub const NAMESPACE: &str = "http://www.loc.gov/standards/alto/ns-v3#";
pub const SCHEMA_LOCATION: &str = "http://www.loc.gov/standards/alto/v3/alto-3-1.xsd";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Software {
    pub creator: String,
    pub name: String,
    pub version: String,
}

/// One `ocrProcessingStep` or `postProcessingStep`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Step {
    /// `processingDateTime`, an `xsd:dateTime` such as
    /// `2026-10-20T14:04:31Z`.
    pub date_time: Option<String>,
    /// `processingStepSettings`, written as `key=value` lines in this order.
    pub settings: Vec<(String, String)>,
    pub software: Software,
}

/// The stored ALTO's `Description`, apart from its unit (always `pixel`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Description {
    /// `sourceImageInformation/fileName`: the URL of the image read.
    pub file_name: String,
    /// `fileIdentifier`s after it, as (`fileIdentifierLocation`, value):
    /// the sha256 of the bytes read and, for a batch's JP2, its bag md5.
    pub file_identifiers: Vec<(String, String)>,
    pub ocr: Step,
    pub post: Option<Step>,
}

/// Text safe in XML 1.0: control characters other than tab, newline and
/// carriage return, and U+FFFE and U+FFFF, can't appear even escaped.
fn clean(s: &str) -> String {
    s.chars()
        .filter(|&c| {
            (c >= ' ' || matches!(c, '\t' | '\n' | '\r')) && c != '\u{FFFE}' && c != '\u{FFFF}'
        })
        .collect()
}

fn esc(s: &str) -> String {
    escape(clean(s).as_str()).into_owned()
}

/// A position in whole pixels.
fn px(x: f64) -> i64 {
    x.round() as i64
}

fn pos(r: &Rect) -> String {
    let (x0, y0) = (px(r.x0), px(r.y0));
    format!(
        "HPOS=\"{x0}\" VPOS=\"{y0}\" WIDTH=\"{}\" HEIGHT=\"{}\"",
        (px(r.x1) - x0).max(0),
        (px(r.y1) - y0).max(0)
    )
}

/// A confidence with at most four decimals and no trailing zeros.
fn conf(c: f64) -> String {
    let s = format!("{:.4}", c.clamp(0.0, 1.0));
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() {
        "0".into()
    } else {
        s.to_owned()
    }
}

fn step(out: &mut String, tag: &str, s: &Step) {
    let _ = writeln!(out, "      <{tag}>");
    if let Some(d) = &s.date_time {
        let _ = writeln!(
            out,
            "        <processingDateTime>{}</processingDateTime>",
            esc(d)
        );
    }
    if !s.settings.is_empty() {
        let lines: Vec<String> = s.settings.iter().map(|(k, v)| format!("{k}={v}")).collect();
        let _ = writeln!(
            out,
            "        <processingStepSettings>{}</processingStepSettings>",
            esc(&lines.join("\n"))
        );
    }
    let sw = &s.software;
    out.push_str("        <processingSoftware>\n");
    for (t, v) in [
        ("softwareCreator", &sw.creator),
        ("softwareName", &sw.name),
        ("softwareVersion", &sw.version),
    ] {
        if !v.is_empty() {
            let _ = writeln!(out, "          <{t}>{}</{t}>", esc(v));
        }
    }
    out.push_str("        </processingSoftware>\n");
    let _ = writeln!(out, "      </{tag}>");
}

/// The joined word for each word that is half of a hyphenated pair
/// (`SUBS_TYPE` and `SUBS_CONTENT`), indexed by line then word.
fn hyphen_parts(lines: &[&crate::model::Line]) -> Vec<Vec<Option<(&'static str, String)>>> {
    let mut parts: Vec<Vec<Option<(&'static str, String)>>> =
        lines.iter().map(|l| vec![None; l.words.len()]).collect();
    for i in 0..lines.len().saturating_sub(1) {
        let (a, b) = (&lines[i], &lines[i + 1]);
        let (Some(_), Some(last), Some(first)) = (&a.hyphen, a.words.last(), b.words.first())
        else {
            continue;
        };
        if last.text.is_empty() || first.text.is_empty() {
            continue;
        }
        let joined = format!("{}{}", last.text, first.text);
        let (la, lb) = (a.words.len() - 1, 0);
        parts[i + 1][lb] = Some(("HypPart2", joined.clone()));
        parts[i][la] = Some(("HypPart1", joined));
    }
    parts
}

/// The stored ALTO for `page`: LANG on each TextBlock is the block's
/// language or else `language` (ISO 639-3). Positions are whole pixels.
/// Geometry finer than `page.granularity` is the parent's box, as the
/// reading gave it; the settings say which granularity that is.
pub fn write(page: &Page, desc: &Description, language: &str) -> String {
    let mut out = String::with_capacity(4096 + page.blocks.len() * 512);
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    let _ = writeln!(
        out,
        "<alto xmlns=\"{NAMESPACE}\" xmlns:xlink=\"http://www.w3.org/1999/xlink\" xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" xsi:schemaLocation=\"{NAMESPACE} {SCHEMA_LOCATION}\">"
    );
    out.push_str("  <Description>\n    <MeasurementUnit>pixel</MeasurementUnit>\n");
    out.push_str("    <sourceImageInformation>\n");
    let _ = writeln!(out, "      <fileName>{}</fileName>", esc(&desc.file_name));
    for (loc, v) in &desc.file_identifiers {
        let _ = writeln!(
            out,
            "      <fileIdentifier fileIdentifierLocation=\"{}\">{}</fileIdentifier>",
            esc(loc),
            esc(v)
        );
    }
    out.push_str("    </sourceImageInformation>\n");
    out.push_str("    <OCRProcessing ID=\"OCR_0\">\n");
    step(&mut out, "ocrProcessingStep", &desc.ocr);
    if let Some(p) = &desc.post {
        step(&mut out, "postProcessingStep", p);
    }
    out.push_str("    </OCRProcessing>\n  </Description>\n  <Layout>\n");
    let full = Rect::new(0.0, 0.0, page.width, page.height);
    let (w, h) = (px(page.width), px(page.height));
    let _ = writeln!(
        out,
        "    <Page ID=\"P1\" PHYSICAL_IMG_NR=\"1\" WIDTH=\"{w}\" HEIGHT=\"{h}\">"
    );
    let _ = writeln!(out, "      <PrintSpace {}>", pos(&full));
    let (mut nb, mut nl, mut ns) = (0usize, 0usize, 0usize);
    let lang = esc(language);
    for b in &page.blocks {
        nb += 1;
        match b.kind {
            BlockKind::Illustration => {
                let _ = writeln!(
                    out,
                    "        <Illustration ID=\"IL{nb}\" {}/>",
                    pos(&b.rect)
                );
                continue;
            }
            BlockKind::Separator => {
                let _ = writeln!(
                    out,
                    "        <GraphicalElement ID=\"GE{nb}\" {}/>",
                    pos(&b.rect)
                );
                continue;
            }
            _ => {}
        }
        let owned = crate::model::stored_lines(b);
        let lines: Vec<&crate::model::Line> = owned.iter().collect();
        if lines.is_empty() {
            continue;
        }
        let block_lang = b.lang.as_deref().map(esc).unwrap_or_else(|| lang.clone());
        let _ = writeln!(
            out,
            "        <TextBlock ID=\"TB{nb}\" {} LANG=\"{block_lang}\">",
            pos(&b.rect)
        );
        let parts = hyphen_parts(&lines);
        for (li, l) in lines.iter().enumerate() {
            nl += 1;
            let _ = writeln!(out, "          <TextLine ID=\"TL{nl}\" {}>", pos(&l.rect));
            for (wi, wd) in l.words.iter().enumerate() {
                ns += 1;
                if wi > 0 {
                    out.push_str(&space(&l.words[wi - 1], wd, page.granularity));
                }
                out.push_str("            <String ID=\"ST");
                let _ = write!(out, "{ns}\" {}", pos(&wd.rect));
                if let Some(c) = wd.conf {
                    let _ = write!(out, " WC=\"{}\"", conf(c));
                }
                if let Some((t, s)) = &parts[li][wi] {
                    let _ = write!(out, " SUBS_TYPE=\"{t}\" SUBS_CONTENT=\"{}\"", esc(s));
                }
                let _ = writeln!(out, " CONTENT=\"{}\"/>", esc(&wd.text));
            }
            if let Some(hy) = &l.hyphen {
                let _ = writeln!(out, "            <HYP CONTENT=\"{}\"/>", esc(hy));
            }
            out.push_str("          </TextLine>\n");
        }
        out.push_str("        </TextBlock>\n");
    }
    out.push_str("      </PrintSpace>\n    </Page>\n  </Layout>\n</alto>\n");
    out
}

/// The `SP` between two words: with its position between them when words
/// have their own boxes and there is a gap.
fn space(prev: &Word, next: &Word, g: Granularity) -> String {
    let (a, b) = (px(prev.rect.x1), px(next.rect.x0));
    if g == Granularity::Word && b > a {
        format!(
            "            <SP HPOS=\"{a}\" VPOS=\"{}\" WIDTH=\"{}\"/>\n",
            px(prev.rect.y0),
            b - a
        )
    } else {
        "            <SP/>\n".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers() {
        assert_eq!(conf(0.96), "0.96");
        assert_eq!(conf(1.0), "1");
        assert_eq!(conf(0.0), "0");
        assert_eq!(conf(0.123456), "0.1235");
        assert_eq!(
            pos(&Rect::new(1.4, 2.6, 10.5, 3.0)),
            "HPOS=\"1\" VPOS=\"3\" WIDTH=\"10\" HEIGHT=\"0\""
        );
        assert_eq!(clean("a\u{1}b\tc\u{FFFF}"), "ab\tc");
    }
}
