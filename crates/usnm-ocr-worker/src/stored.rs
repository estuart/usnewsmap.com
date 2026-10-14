//! The page's stored outputs from an engine's reading (ADR-0016 decision
//! 5, 13 §13.4.7 steps 2 to 4): the engine's ALTO is parsed, the stored
//! ALTO 3.1 is written from the parsed values (`usnm_alto::write`), and the
//! page text is derived from that page in reading order, then normalized.
//! The text and the ALTO always hold the same words, and no engine XML is
//! stored.

use chrono::{DateTime, SecondsFormat, Utc};
use usnm_alto::{Description, Page, ParseOptions, Software, Step};
use usnm_core::ocr_engine::EngineTag;

/// This worker as the stored ALTO names it: the crate version, plus the
/// commit CI built it from (`USNM_GIT_SHA`), e.g. `0.1.0+1dcc5c3`.
pub fn worker_version() -> String {
    let v = env!("CARGO_PKG_VERSION");
    match option_env!("USNM_GIT_SHA").filter(|s| !s.is_empty()) {
        Some(sha) => format!("{v}+{}", &sha[..sha.len().min(7)]),
        None => v.to_owned(),
    }
}

/// The file the engine read.
#[derive(Debug, Clone, Copy)]
pub struct Source<'a> {
    /// The scan's URL: the IIIF full image, or a batch's JP2.
    pub url: &'a str,
    /// The sha256 of the bytes read.
    pub sha256: &'a str,
    /// A batch JP2's md5 from its bag manifest, which the bytes matched.
    pub md5: Option<&'a str>,
}

/// What the worker uploads for a page.
#[derive(Debug, Clone, PartialEq)]
pub struct Stored {
    /// The stored ALTO 3.1.
    pub alto: String,
    /// The page text, normalized (`normalize_ocr`).
    pub text: String,
    pub page: Page,
}

/// Why an engine's output couldn't be stored: the worker's `convert`.
#[derive(Debug, thiserror::Error)]
#[error("the engine's ALTO doesn't convert: {0}")]
pub struct ConvertError(#[from] pub usnm_alto::ParseError);

fn hex(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The stored outputs for a page read as `ocr_engine` (its full tag) in
/// `language` (ISO 639-3) from engine ALTO in pixels of the image read,
/// at `dpi`. `worker_version` names this worker in the ALTO.
pub fn from_engine_alto(
    engine_alto: &[u8],
    ocr_engine: &str,
    language: &str,
    dpi: u32,
    source: &Source<'_>,
    worker_version: &str,
    now: DateTime<Utc>,
) -> Result<Stored, ConvertError> {
    let page = usnm_alto::parse(engine_alto, &ParseOptions::default())?;
    Ok(store(
        page,
        ocr_engine,
        language,
        dpi,
        source,
        worker_version,
        now,
    ))
}

/// The stored outputs for a parsed page.
pub fn store(
    page: Page,
    ocr_engine: &str,
    language: &str,
    dpi: u32,
    source: &Source<'_>,
    worker_version: &str,
    now: DateTime<Utc>,
) -> Stored {
    let mut ids = Vec::new();
    if hex(source.sha256) {
        ids.push(("sha256".to_owned(), source.sha256.to_owned()));
        if let Some(m) = source.md5.filter(|m| hex(m)) {
            ids.push(("md5".to_owned(), m.to_owned()));
        }
    }
    // The engine's name and version from its tag; the whole tag goes in
    // the settings, with what it names (13 §13.8.2).
    let (family, version) = match EngineTag::parse(ocr_engine) {
        Ok(EngineTag::Tag(t)) => (t.family().to_owned(), t.version().to_owned()),
        _ => (
            ocr_engine.split('/').next().unwrap_or("").to_owned(),
            String::new(),
        ),
    };
    let creator = match family.as_str() {
        "tesseract" => "tesseract-ocr".to_owned(),
        other => other.to_owned(),
    };
    let desc = Description {
        file_name: source.url.to_owned(),
        file_identifiers: ids,
        ocr: Step {
            date_time: Some(now.to_rfc3339_opts(SecondsFormat::Secs, true)),
            settings: vec![
                ("ocr_engine".into(), ocr_engine.to_owned()),
                ("granularity".into(), page.granularity.as_str().to_owned()),
                ("dpi".into(), format!("{dpi} (assumed)")),
            ],
            software: Software {
                creator,
                name: family,
                version,
            },
        },
        post: Some(Step {
            date_time: None,
            settings: vec![
                ("source".into(), "engine ALTO, parsed".into()),
                (
                    "text".into(),
                    "usnm-alto reading order, normalize_ocr".into(),
                ),
            ],
            software: Software {
                creator: "usnewsmap.com".into(),
                name: "usnm-ocr-worker".into(),
                version: worker_version.to_owned(),
            },
        }),
    };
    let alto = usnm_alto::write(&page, &desc, language);
    let text = usnm_core::text::normalize_ocr(&usnm_alto::text(&page));
    Stored { alto, text, page }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::set_alto_source;

    const TAG: &str = "tesseract/5.5.1 profile=eng image=sha256:3f9a0c1d2e4b models=sha256:8c2d71aa09fe route=minimal@r1 contract=0";
    const URL: &str = "https://tile.loc.gov/image-services/iiif/service:ndnp:az:batch_az_agave_ver01:data:sn84024828:00211100618:1894062301:0285/full/full/0/default.jpg";
    const SHA: &str = "0207a56216a746f2d137d019ff44c49c83d4f403efde9b1eddba67ab6ebe326b";

    /// Tesseract 5.5's own ALTO and text for two pages: a crop of a real
    /// 1894 page (Mohave County Miner, from LoC's IIIF service, read with
    /// `--psm 3 --dpi 300`) and a rendered two-column page.
    fn fixtures() -> Vec<(&'static str, Vec<u8>, String)> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        [
            (
                "testdata/tesseract-agave-1894-06-23",
                dir.join("testdata/tesseract-agave-1894-06-23"),
            ),
            (
                "usnm-alto tesseract-5.5",
                dir.join("../usnm-alto/tests/data/tesseract-5.5"),
            ),
        ]
        .into_iter()
        .map(|(name, base)| {
            (
                name,
                std::fs::read(format!("{}.xml", base.display())).unwrap(),
                std::fs::read_to_string(format!("{}.txt", base.display())).unwrap(),
            )
        })
        .collect()
    }

    fn page_of(alto: &str) -> Page {
        usnm_alto::parse(alto.as_bytes(), &ParseOptions::default()).unwrap()
    }

    /// The text between `<tag>` and `</tag>`s, in order.
    fn elements(xml: &str, tag: &str) -> Vec<String> {
        let (open, close) = (format!("<{tag}"), format!("</{tag}>"));
        xml.match_indices(&open)
            .filter_map(|(i, _)| {
                let rest = &xml[i..];
                let start = rest.find('>')? + 1;
                let end = rest.find(&close)?;
                Some(rest[start..end].to_owned())
            })
            .collect()
    }

    /// Regression against main's output for Tesseract pages: main stored
    /// Tesseract's own ALTO with `fileName` replaced and a `fileIdentifier`
    /// added, and `normalize_ocr` of Tesseract's `page.txt`.
    #[test]
    fn tesseract_pages_store_what_main_stored() {
        let now: DateTime<Utc> = "2026-10-20T14:04:31Z".parse().unwrap();
        for md5 in [None, Some("cd34")] {
            for (name, xml, txt) in fixtures() {
                let source = Source {
                    url: URL,
                    sha256: SHA,
                    md5,
                };
                let new = from_engine_alto(&xml, TAG, "eng", 300, &source, "0.1.0", now).unwrap();
                let old_alto = set_alto_source(&String::from_utf8_lossy(&xml), URL, SHA, md5);
                let old_text = usnm_core::text::normalize_ocr(&txt);
                // The same text.
                assert_eq!(new.text, old_text, "{name}: text");
                // The same page: size, blocks, lines, words, boxes,
                // confidences and hyphens.
                assert_eq!(page_of(&new.alto), page_of(&old_alto), "{name}: layout");
                // The same scan named, with the same identifiers.
                for tag in ["fileName", "fileIdentifier"] {
                    assert_eq!(
                        elements(&new.alto, tag),
                        elements(&old_alto, tag),
                        "{name}: {tag}"
                    );
                }
                // The pickup takes it, as it took main's.
                usnm_ingest::ocr_local::checks::check_alto(new.alto.as_bytes()).unwrap();
                assert!(new
                    .alto
                    .contains("<softwareVersion>5.5.1</softwareVersion>"));
                assert!(new.alto.contains(&format!("ocr_engine={TAG}")));
            }
        }
    }

    #[test]
    fn engine_alto_that_doesnt_parse_is_a_convert_failure() {
        let source = Source {
            url: URL,
            sha256: SHA,
            md5: None,
        };
        let now = Utc::now();
        assert!(from_engine_alto(b"<alto/>", TAG, "eng", 300, &source, "0.1.0", now).is_err());
        assert!(from_engine_alto(b"not xml", TAG, "eng", 300, &source, "0.1.0", now).is_err());
    }
}
