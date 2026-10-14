//! Golden pages for the stored ALTO writer (13 §13.11 step 3): engine
//! output in, stored ALTO 3.1 and reading-order text out. CI checks every
//! file in `tests/golden/` against `xsd/alto-3-1.xsd` with xmllint.
//!
//! `UPDATE_GOLDEN=1 cargo test -p usnm-alto --test golden` rewrites the
//! golden files after a deliberate change.

use std::path::{Path, PathBuf};

use usnm_alto::boxes::{self, BoxLimits, Convert};
use usnm_alto::{
    parse, text, write, Description, Dir, Granularity, Markup, Page, ParseOptions, Software, Step,
};

fn dir(sub: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join(sub)
}

fn data(name: &str) -> Vec<u8> {
    std::fs::read(dir("data").join(name)).unwrap()
}

fn desc() -> Description {
    Description {
        file_name: "https://tile.loc.gov/image-services/iiif/service:ndnp:x:batch_x:data:0001/full/full/0/default.jpg".into(),
        file_identifiers: vec![(
            "sha256".into(),
            "0207a56216a746f2d137d019ff44c49c83d4f403efde9b1eddba67ab6ebe326b".into(),
        )],
        ocr: Step {
            date_time: Some("2026-10-20T14:04:31Z".into()),
            settings: vec![
                ("profile".into(), "eng".into()),
                ("granularity".into(), "word".into()),
                ("dpi".into(), "300 (assumed)".into()),
            ],
            software: Software {
                creator: "tesseract-ocr".into(),
                name: "tesseract".into(),
                version: "5.5.1".into(),
            },
        },
        post: Some(Step {
            date_time: None,
            settings: vec![("text".into(), "usnm-alto reading order".into())],
            software: Software {
                creator: "usnewsmap.com".into(),
                name: "usnm-ocr-worker".into(),
                version: "0.1.0".into(),
            },
        }),
    }
}

/// Writes the page's stored ALTO and text, and compares them with the
/// golden files (or rewrites those).
fn golden(name: &str, page: &Page, language: &str) -> String {
    let xml = write(page, &desc(), language);
    let txt = text(page);
    let (gx, gt) = (
        dir("golden").join(format!("{name}.xml")),
        dir("golden").join(format!("{name}.txt")),
    );
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(dir("golden")).unwrap();
        std::fs::write(&gx, &xml).unwrap();
        std::fs::write(&gt, format!("{txt}\n")).unwrap();
    }
    assert_eq!(xml, std::fs::read_to_string(&gx).unwrap(), "{name}.xml");
    assert_eq!(
        format!("{txt}\n"),
        std::fs::read_to_string(&gt).unwrap(),
        "{name}.txt"
    );
    // The stored ALTO reads back as the same page and text.
    usnm_alto::check(xml.as_bytes(), &usnm_alto::Limits::default()).unwrap();
    let again = parse(xml.as_bytes(), &ParseOptions::default()).unwrap();
    assert_eq!(text(&again), txt, "{name}: text from the stored ALTO");
    assert_eq!(
        (again.width, again.height),
        (page.width.round(), page.height.round())
    );
    xmllint(&gx);
    xml
}

/// Validates against ALTO 3.1 when xmllint is installed (CI runs it on
/// every golden file regardless).
fn xmllint(path: &Path) {
    let xsd = Path::new(env!("CARGO_MANIFEST_DIR")).join("xsd/alto-3-1.xsd");
    let out = std::process::Command::new("xmllint")
        .args(["--noout", "--schema"])
        .arg(&xsd)
        .arg(path)
        .output();
    match out {
        Ok(o) => assert!(
            o.status.success(),
            "{} isn't valid ALTO 3.1: {}",
            path.display(),
            String::from_utf8_lossy(&o.stderr)
        ),
        Err(_) => eprintln!("xmllint not installed; skipping the schema check"),
    }
}

/// Non-empty lines, without the form feed Tesseract ends its text with.
fn lines(s: &str) -> Vec<String> {
    s.lines()
        .map(|l| {
            l.trim_matches(|c: char| c.is_whitespace() || c == '\u{c}')
                .to_owned()
        })
        .filter(|l| !l.is_empty())
        .collect()
}

#[test]
fn tesseract_alto() {
    let page = parse(&data("tesseract-5.5.xml"), &ParseOptions::default()).unwrap();
    assert_eq!(page.granularity, Granularity::Word);
    let xml = golden("tesseract-5.5", &page, "eng");
    // The same words in the same lines as Tesseract's own text output,
    // which main stores today.
    let txt = String::from_utf8(data("tesseract-5.5.txt")).unwrap();
    assert_eq!(lines(&text(&page)), lines(&txt));
    // Every word keeps its box and confidence.
    let again = parse(xml.as_bytes(), &ParseOptions::default()).unwrap();
    let words = |p: &Page| -> Vec<(String, [i64; 4], Option<f64>)> {
        p.blocks
            .iter()
            .flat_map(|b| &b.lines)
            .flat_map(|l| &l.words)
            .map(|w| {
                let r = w.rect;
                (
                    w.text.clone(),
                    [r.x0, r.y0, r.x1, r.y1].map(|v| v.round() as i64),
                    w.conf,
                )
            })
            .collect()
    };
    assert_eq!(words(&again), words(&page));
}

#[test]
fn ja_ocr_alto_in_inch1200() {
    let opts = ParseOptions {
        dpi: Some(300.0),
        ..ParseOptions::default()
    };
    let page = parse(&data("ja-ocr.xml"), &opts).unwrap();
    assert_eq!((page.width, page.height), (2400.0, 4000.0));
    golden("ja-ocr", &page, "jpn");
}

#[test]
fn word_boxes_in_detection_order() {
    let b = boxes::parse(&data("boxes-word.json"), &BoxLimits::default()).unwrap();
    let c = Convert {
        granularity: Granularity::Word,
        full: (2000.0, 3000.0),
        dir: Dir::Ltr,
        markup: Markup::None,
    };
    let page = boxes::to_page(&b, &c).unwrap();
    let xml = golden("boxes-word", &page, "eng");
    assert!(xml.contains("SUBS_TYPE=\"HypPart1\" SUBS_CONTENT=\"council\""));
    assert_eq!(
        text(&page),
        "EVENING GAZETTE\n\nThe coun-\ncil met.\n\nWheat rose\nagain."
    );
}

#[test]
fn block_text_with_html() {
    let b = boxes::parse(&data("boxes-block.json"), &BoxLimits::default()).unwrap();
    let c = Convert {
        granularity: Granularity::Block,
        full: (2000.0, 3000.0),
        dir: Dir::Ltr,
        markup: Markup::Html,
    };
    let page = boxes::to_page(&b, &c).unwrap();
    golden("boxes-block", &page, "deu");
    assert!(
        !text(&page).contains("ship"),
        "text on an illustration isn't kept"
    );
}

#[test]
fn text_only_page() {
    let t = String::from_utf8(data("text-only.txt")).unwrap();
    let page = Page::text_only(2000.0, 3000.0, &t);
    assert_eq!(page.granularity, Granularity::Page);
    golden("text-only", &page, "eng");
}
