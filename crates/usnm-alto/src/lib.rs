//! ALTO from OCR nodes (#296, 13 §13.2): a bounded reader.
//!
//! An OCR node is outside Azure and its output is untrusted, so the pickup
//! (`usnm-ingest ocr-sync`, 12 §12.5 Step 8 check 6) reads each ALTO file
//! with [`check`] before copying it into the lake: UTF-8, well-formed XML
//! with no DOCTYPE (so no entity can be declared or expanded) and no
//! processing instruction but the declaration, one root element, `alto` in
//! an ALTO v2, v3 or v4 namespace, within depth, element and attribute
//! limits, with a `Page` that has a positive `WIDTH` and `HEIGHT`. The
//! reader streams and keeps only the element stack, so memory stays at the
//! input's size plus the limits whatever the input holds.
//!
//! The rest of 13's `usnm-alto` (writing the stored profile, boxes to ALTO,
//! tile merging, text in reading order) comes with the engine host.

use quick_xml::events::{BytesStart, Event};
use quick_xml::name::{Namespace, ResolveResult};
use quick_xml::{NsReader, XmlVersion};

/// ALTO namespaces this reader accepts, oldest first.
pub const NAMESPACES: [(&str, AltoVersion); 3] = [
    ("http://www.loc.gov/standards/alto/ns-v2#", AltoVersion::V2),
    ("http://www.loc.gov/standards/alto/ns-v3#", AltoVersion::V3),
    ("http://www.loc.gov/standards/alto/ns-v4#", AltoVersion::V4),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AltoVersion {
    V2,
    V3,
    V4,
}

/// What [`check`] allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_bytes: usize,
    /// Element nesting, the root at depth 1.
    pub max_depth: usize,
    pub max_elements: usize,
    pub max_attributes: usize,
}

impl Default for Limits {
    /// 32 MiB (12 Step 8 check 1); a newspaper page's ALTO nests about 8
    /// deep and holds some 10,000 to 100,000 elements.
    fn default() -> Self {
        Self {
            max_bytes: 32 * 1024 * 1024,
            max_depth: 32,
            max_elements: 2_000_000,
            max_attributes: 32,
        }
    }
}

/// What a valid file says about its page.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AltoPage {
    pub version: AltoVersion,
    pub width: f64,
    pub height: f64,
    pub elements: usize,
}

/// Why a file was refused. Each kind is a fixed reason code that carries
/// none of the file's content, so it can go in logs and quarantine records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AltoError {
    #[error("over the size limit")]
    TooLarge,
    #[error("not UTF-8")]
    NotUtf8,
    #[error("not well-formed XML")]
    Malformed,
    #[error("has a DOCTYPE")]
    Doctype,
    #[error("has a processing instruction")]
    ProcessingInstruction,
    #[error("nested too deeply")]
    TooDeep,
    #[error("too many elements")]
    TooManyElements,
    #[error("an element has too many attributes")]
    TooManyAttributes,
    #[error("the root isn't alto in an ALTO namespace")]
    NotAlto,
    #[error("more than one root element, or text outside it")]
    OutsideRoot,
    #[error("no Page with a positive WIDTH and HEIGHT")]
    NoPage,
}

impl AltoError {
    /// A short code for records and metrics.
    pub fn code(self) -> &'static str {
        match self {
            AltoError::TooLarge => "alto_too_large",
            AltoError::NotUtf8 => "alto_not_utf8",
            AltoError::Malformed => "alto_malformed",
            AltoError::Doctype => "alto_doctype",
            AltoError::ProcessingInstruction => "alto_pi",
            AltoError::TooDeep => "alto_too_deep",
            AltoError::TooManyElements => "alto_too_many_elements",
            AltoError::TooManyAttributes => "alto_too_many_attributes",
            AltoError::NotAlto => "alto_not_alto",
            AltoError::OutsideRoot => "alto_outside_root",
            AltoError::NoPage => "alto_no_page",
        }
    }
}

fn alto_version(ns: &ResolveResult<'_>) -> Option<AltoVersion> {
    match ns {
        ResolveResult::Bound(Namespace(uri)) => {
            NAMESPACES.iter().find(|(n, _)| n == uri).map(|(_, v)| *v)
        }
        _ => None,
    }
}

/// A positive, finite number.
fn dimension(e: &BytesStart<'_>, name: &str) -> Result<Option<f64>, AltoError> {
    for a in e.attributes().with_checks(true) {
        let a = a.map_err(|_| AltoError::Malformed)?;
        // The unprefixed attribute only: `x:WIDTH` is someone else's.
        if a.key.as_ref() == name {
            return Ok(a
                .value
                .trim()
                .parse::<f64>()
                .ok()
                .filter(|x| x.is_finite() && *x > 0.0));
        }
    }
    Ok(None)
}

/// Check one element's start: its attributes are well-formed and few enough.
fn attributes(e: &BytesStart<'_>, limits: &Limits) -> Result<(), AltoError> {
    let mut n = 0;
    for a in e.attributes().with_checks(true) {
        let a = a.map_err(|_| AltoError::Malformed)?;
        // Entity references other than XML's five fail here, since there is
        // no DTD to declare them.
        a.normalized_value(XmlVersion::Implicit1_0)
            .map_err(|_| AltoError::Malformed)?;
        n += 1;
        if n > limits.max_attributes {
            return Err(AltoError::TooManyAttributes);
        }
    }
    Ok(())
}

/// Read `xml` as ALTO within `limits`.
pub fn check(xml: &[u8], limits: &Limits) -> Result<AltoPage, AltoError> {
    if xml.len() > limits.max_bytes {
        return Err(AltoError::TooLarge);
    }
    let text = std::str::from_utf8(xml).map_err(|_| AltoError::NotUtf8)?;
    let mut reader = NsReader::from_str(text);
    {
        let c = reader.config_mut();
        c.check_end_names = true;
        c.check_comments = true;
        c.expand_empty_elements = false;
        c.trim_text(false);
    }
    let mut depth = 0usize;
    let mut elements = 0usize;
    let mut root: Option<AltoVersion> = None;
    let mut root_closed = false;
    let mut page: Option<(f64, f64)> = None;
    let mut first = true;
    loop {
        let (ns, event) = reader
            .read_resolved_event()
            .map_err(|_| AltoError::Malformed)?;
        let start = match &event {
            Event::Start(e) | Event::Empty(e) => Some(e.clone()),
            _ => None,
        };
        if let Some(e) = start {
            if root_closed {
                return Err(AltoError::OutsideRoot);
            }
            elements += 1;
            if elements > limits.max_elements {
                return Err(AltoError::TooManyElements);
            }
            attributes(&e, limits)?;
            let version = alto_version(&ns);
            if depth == 0 {
                if e.local_name().as_ref() != "alto" || version.is_none() {
                    return Err(AltoError::NotAlto);
                }
                root = version;
            }
            if e.local_name().as_ref() == "Page" && version == root && page.is_none() {
                if let (Some(w), Some(h)) = (dimension(&e, "WIDTH")?, dimension(&e, "HEIGHT")?) {
                    page = Some((w, h));
                }
            }
            // An empty element sits one level down too.
            if depth + 1 > limits.max_depth {
                return Err(AltoError::TooDeep);
            }
            if matches!(event, Event::Start(_)) {
                depth += 1;
            } else if depth == 0 {
                root_closed = true;
            }
        }
        match event {
            Event::End(_) => {
                depth = depth.checked_sub(1).ok_or(AltoError::Malformed)?;
                if depth == 0 {
                    root_closed = true;
                }
            }
            Event::DocType(_) => return Err(AltoError::Doctype),
            Event::PI(_) => return Err(AltoError::ProcessingInstruction),
            Event::Decl(_) if !first => return Err(AltoError::Malformed),
            Event::Text(t) if depth == 0 => {
                if !t.trim().is_empty() {
                    return Err(AltoError::OutsideRoot);
                }
            }
            Event::GeneralRef(r) => {
                if depth == 0 {
                    return Err(AltoError::OutsideRoot);
                }
                // XML's own five or a character reference: there is no DTD
                // to declare any other.
                let known = matches!(&*r, "lt" | "gt" | "amp" | "apos" | "quot")
                    || r.resolve_char_ref().ok().flatten().is_some();
                if !known {
                    return Err(AltoError::Malformed);
                }
            }
            Event::CData(_) if depth == 0 => return Err(AltoError::OutsideRoot),
            Event::Eof => break,
            _ => {}
        }
        first = false;
    }
    if depth != 0 {
        return Err(AltoError::Malformed);
    }
    let version = root.ok_or(AltoError::NotAlto)?;
    let (width, height) = page.ok_or(AltoError::NoPage)?;
    Ok(AltoPage {
        version,
        width,
        height,
        elements,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const V3: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<alto xmlns="http://www.loc.gov/standards/alto/ns-v3#" xmlns:xlink="http://www.w3.org/1999/xlink">
  <Description><MeasurementUnit>pixel</MeasurementUnit>
    <sourceImageInformation><fileName>https://tile.loc.gov/a?b&amp;c</fileName></sourceImageInformation>
  </Description>
  <Layout><Page ID="p1" WIDTH="5000" HEIGHT="7000" PHYSICAL_IMG_NR="1">
    <PrintSpace><TextBlock ID="b1"><TextLine ID="l1">
      <String CONTENT="Zeitung" HPOS="1" VPOS="2" WIDTH="3" HEIGHT="4"/><SP/>
      <String CONTENT="&#228;&lt;" />
    </TextLine></TextBlock></PrintSpace>
  </Page></Layout>
</alto>
"#;

    fn ok(xml: &str) -> AltoPage {
        check(xml.as_bytes(), &Limits::default()).unwrap()
    }

    fn err(xml: &str) -> AltoError {
        check(xml.as_bytes(), &Limits::default()).unwrap_err()
    }

    #[test]
    fn reads_alto_v2_v3_and_v4() {
        let p = ok(V3);
        assert_eq!(p.version, AltoVersion::V3);
        assert_eq!((p.width, p.height), (5000.0, 7000.0));
        assert!(p.elements > 10);
        assert_eq!(ok(&V3.replace("ns-v3#", "ns-v2#")).version, AltoVersion::V2);
        assert_eq!(ok(&V3.replace("ns-v3#", "ns-v4#")).version, AltoVersion::V4);
        // A prefixed namespace is the same document.
        assert_eq!(
            err(&V3.replace("WIDTH=\"5000\"", "xmlns:x=\"urn:x\" x:WIDTH=\"5000\"")),
            AltoError::NoPage
        );
        let prefixed = r#"<a:alto xmlns:a="http://www.loc.gov/standards/alto/ns-v4#"><a:Layout><a:Page WIDTH="1.5" HEIGHT="2"/></a:Layout></a:alto>"#;
        assert_eq!(ok(prefixed).version, AltoVersion::V4);
    }

    #[test]
    fn refuses_what_isnt_bounded_alto() {
        let doctype = V3.replace(
            "<alto ",
            "<!DOCTYPE alto [<!ENTITY a \"aaaaaaaaaa\"><!ENTITY b \"&a;&a;&a;&a;\">]>\n<alto ",
        );
        assert_eq!(err(&doctype), AltoError::Doctype);
        assert_eq!(
            err(&V3.replace("Zeitung", "&b;")),
            AltoError::Malformed,
            "an undeclared entity"
        );
        assert_eq!(
            err(&V3.replace("<Layout>", "<?php x ?><Layout>")),
            AltoError::ProcessingInstruction
        );
        assert_eq!(err(&V3.replace("ns-v3#", "ns-v9#")), AltoError::NotAlto);
        assert_eq!(
            err(&V3.replace("<alto ", "<page ").replace("</alto>", "</page>")),
            AltoError::NotAlto
        );
        assert_eq!(err(&V3.replace("</TextLine>", "")), AltoError::Malformed);
        assert_eq!(err(&format!("{V3}<alto/>")), AltoError::OutsideRoot);
        assert_eq!(err(&format!("{V3}junk")), AltoError::OutsideRoot);
        assert_eq!(err(&V3.replace("WIDTH=\"5000\"", "")), AltoError::NoPage);
        assert_eq!(
            err(&V3.replace("WIDTH=\"5000\"", "WIDTH=\"-1\"")),
            AltoError::NoPage
        );
        assert_eq!(
            err(&V3.replace("WIDTH=\"5000\"", "WIDTH=\"NaN\"")),
            AltoError::NoPage
        );
        assert_eq!(
            err(&V3.replace("ID=\"p1\"", "ID=\"p1\" ID=\"p2\"")),
            AltoError::Malformed,
            "a duplicate attribute"
        );
        assert_eq!(
            check(&[0xff, 0xfe, b'<'], &Limits::default()),
            Err(AltoError::NotUtf8)
        );
        assert_eq!(err(""), AltoError::NotAlto);
        let deep = format!(
            "<alto xmlns=\"http://www.loc.gov/standards/alto/ns-v3#\">{}{}</alto>",
            "<x>".repeat(40),
            "</x>".repeat(40)
        );
        assert_eq!(err(&deep), AltoError::TooDeep);
        // An empty element one level past the limit.
        let edge = format!(
            "<alto xmlns=\"http://www.loc.gov/standards/alto/ns-v3#\">{}<Page WIDTH=\"1\" HEIGHT=\"1\"/>{}</alto>",
            "<x>".repeat(31),
            "</x>".repeat(31)
        );
        assert_eq!(err(&edge), AltoError::TooDeep);
        let small = Limits {
            max_elements: 5,
            ..Limits::default()
        };
        assert_eq!(
            check(V3.as_bytes(), &small),
            Err(AltoError::TooManyElements)
        );
        let small = Limits {
            max_bytes: 10,
            ..Limits::default()
        };
        assert_eq!(check(V3.as_bytes(), &small), Err(AltoError::TooLarge));
        let small = Limits {
            max_attributes: 2,
            ..Limits::default()
        };
        assert_eq!(
            check(V3.as_bytes(), &small),
            Err(AltoError::TooManyAttributes)
        );
    }
}
