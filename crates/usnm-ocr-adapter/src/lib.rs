//! The adapter side of the engine contract `usnm-ocr-engine/1` (ADR-0016,
//! 13 §13.4), for adapters written in Rust: an engine reads one image, and
//! [`serve`] does the rest. It sends `hello`, takes one `page` at a time
//! from stdin, refuses profiles and languages the image doesn't have,
//! writes the engine's output into `/out/<run_id>/` as regular files, and
//! answers with one `result` per page on stdout.
//!
//! An engine that returns text only gets one page-sized block in
//! `boxes.json` (granularity `page`), and the worker derives the stored
//! ALTO and text from it. Adapters never tile, merge tiles or strip
//! markup: the worker does.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use usnm_alto::boxes::{BoxBlock, Boxes, Frame, ReadingOrder, Units, VERSION};
use usnm_alto::{BlockKind, Granularity, Markup};
use usnm_ocr_contract::engine::{
    decode_request, encode, FromAdapter, Hello, Layout, LayoutFile, PageRequest, PageResult,
    ResultError, Status, TextFile, ToAdapter,
};

/// What an engine read on a page.
#[derive(Debug, Clone, PartialEq)]
pub enum Reading {
    /// Plain text only, with its markup.
    Text { text: String, markup: Markup },
    /// Blocks, lines or words with boxes.
    Boxes { boxes: Boxes, markup: Markup },
}

/// Why an engine didn't read a page; never page text.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ReadError {
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("bad input: {0}")]
    BadInput(String),
    #[error("{kind}: {message}")]
    Failed {
        kind: String,
        message: String,
        /// The engine is in a bad state: the worker should restart the
        /// instance.
        restart: bool,
    },
    /// The deadline passed, and everything started for the page is
    /// stopped.
    #[error("timeout")]
    Timeout,
}

/// An engine behind the adapter.
pub trait Reader {
    /// Reads the image at `image` (the request's path, as the adapter
    /// sees it) for `req`, stopping by `deadline`.
    fn read(
        &mut self,
        req: &PageRequest,
        image: &Path,
        deadline: Instant,
    ) -> Result<Reading, ReadError>;
}

/// Where the request's `/in` and `/out` are for this process: `/` in a
/// container, or another directory for a test.
#[derive(Debug, Clone)]
pub struct Root(pub PathBuf);

impl Root {
    fn map(&self, path: &str) -> PathBuf {
        self.0.join(path.trim_start_matches('/'))
    }
}

/// The one page-sized text block of a text-only reading, in the image's
/// pixels.
pub fn page_boxes(text: &str, width: u32, height: u32) -> Boxes {
    let (w, h) = (f64::from(width), f64::from(height));
    Boxes {
        v: VERSION,
        units: Units::Px,
        frame: Frame {
            width: w,
            height: h,
        },
        granularity: Granularity::Page,
        reading_order: ReadingOrder::Engine,
        blocks: vec![BoxBlock {
            bbox: [0.0, 0.0, w, h],
            kind: BlockKind::Text,
            order: None,
            dir: None,
            lang: None,
            label: None,
            conf: None,
            text: Some(text.to_owned()),
            lines: Vec::new(),
            polygon: None,
        }],
    }
}

fn failure(
    req: &PageRequest,
    status: Status,
    kind: &str,
    message: &str,
    restart: bool,
) -> PageResult {
    // At most 2 KiB (13 §13.4.4), cut on a character boundary.
    let mut m = message.to_owned();
    while m.len() > usnm_ocr_contract::engine::MAX_ERROR_MESSAGE {
        m.pop();
    }
    PageResult {
        engine_stats: Default::default(),
        error: Some(ResultError {
            kind: kind.to_owned(),
            message: m,
        }),
        layout: None,
        markup: Markup::None,
        partial: false,
        restart,
        run_id: req.run_id.clone(),
        status,
        text: None,
        warnings: Vec::new(),
    }
}

/// The engine's own text from its boxes, at the finest level that holds
/// text: words joined by spaces, a line per line, a blank line between
/// blocks.
pub fn boxes_text(boxes: &Boxes) -> String {
    let blocks: Vec<String> = boxes
        .blocks
        .iter()
        .filter_map(|b| {
            let lines: Vec<String> = b
                .lines
                .iter()
                .map(|l| {
                    if l.words.is_empty() {
                        l.text.clone().unwrap_or_default()
                    } else {
                        l.words
                            .iter()
                            .map(|w| w.text.as_str())
                            .collect::<Vec<_>>()
                            .join(" ")
                    }
                })
                .filter(|t| !t.is_empty())
                .collect();
            let t = if lines.is_empty() {
                b.text.clone().unwrap_or_default()
            } else {
                lines.join("\n")
            };
            (!t.is_empty()).then_some(t)
        })
        .collect();
    blocks.join("\n\n")
}

/// Writes a regular file, mode 0644, in the page's out directory.
fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> std::io::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o644);
    }
    opts.open(dir.join(name))?.write_all(bytes)
}

/// Answers one page.
pub fn answer<R: Reader>(
    hello: &Hello,
    reader: &mut R,
    root: &Root,
    req: &PageRequest,
) -> PageResult {
    let start = Instant::now();
    let deadline = start + Duration::from_millis(req.deadline_ms);
    let Some(profile) = hello.profile(&req.profile) else {
        return failure(
            req,
            Status::Unsupported,
            "profile",
            &format!("no profile {}", req.profile),
            false,
        );
    };
    if !profile.languages.contains(&req.language) {
        return failure(
            req,
            Status::Unsupported,
            "language",
            &format!("profile {} doesn't read {}", req.profile, req.language),
            false,
        );
    }
    if let Some(k) = req.options.keys().find(|k| !profile.tunable.contains(k)) {
        return failure(
            req,
            Status::Unsupported,
            "option",
            &format!("profile {} has no option {k}", req.profile),
            false,
        );
    }
    let image = root.map(&req.image.path);
    let reading = match reader.read(req, &image, deadline) {
        Ok(r) => r,
        Err(ReadError::Unsupported(m)) => {
            return failure(req, Status::Unsupported, "unsupported", &m, false)
        }
        Err(ReadError::BadInput(m)) => {
            return failure(req, Status::BadInput, "bad_input", &m, false)
        }
        Err(ReadError::Failed {
            kind,
            message,
            restart,
        }) => return failure(req, Status::Failed, &kind, &message, restart),
        Err(ReadError::Timeout) => {
            return failure(
                req,
                Status::Timeout,
                "timeout",
                "the page passed its deadline",
                false,
            )
        }
    };
    let (boxes, text, markup) = match reading {
        Reading::Text { text, markup } => (
            page_boxes(&text, req.image.width, req.image.height),
            text,
            markup,
        ),
        Reading::Boxes { boxes, markup } => {
            let text = boxes_text(&boxes);
            (boxes, text, markup)
        }
    };
    let out = root.map(&req.out);
    let out_str = req.out.trim_end_matches('/');
    let written = serde_json::to_vec(&boxes)
        .map_err(std::io::Error::other)
        .and_then(|b| write_file(&out, "boxes.json", &b))
        .and_then(|()| write_file(&out, "page.txt", text.as_bytes()));
    if let Err(e) = written {
        return failure(
            req,
            Status::Failed,
            "output",
            &format!("writing the output: {e}"),
            false,
        );
    }
    PageResult {
        engine_stats: Default::default(),
        error: None,
        layout: Some(LayoutFile {
            alto_dpi: None,
            kind: Layout::Boxes,
            path: format!("{out_str}/boxes.json"),
        }),
        markup,
        partial: false,
        restart: false,
        run_id: req.run_id.clone(),
        status: Status::Ok,
        text: Some(TextFile {
            path: format!("{out_str}/page.txt"),
        }),
        warnings: Vec::new(),
    }
}

/// Runs the protocol until stdin closes: `hello`, then one `result` per
/// `page`. A line that isn't a contract request ends the adapter with an
/// error, as the worker treats a broken line from the adapter.
pub fn serve<R: Reader>(
    hello: &Hello,
    reader: &mut R,
    root: &Root,
    input: impl BufRead,
    mut output: impl Write,
) -> anyhow::Result<()> {
    output.write_all(&encode(&FromAdapter::Hello(hello.clone())))?;
    output.flush()?;
    for line in input.split(b'\n') {
        let line = line?;
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let ToAdapter::Page(req) = decode_request(&line)?;
        let result = answer(hello, reader, root, &req);
        output.write_all(&encode(&FromAdapter::Result(result)))?;
        output.flush()?;
    }
    Ok(())
}

/// The sha256 of a file, as lowercase hex.
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use usnm_ocr_contract::engine::{
        Adapter, Cancel, DpiSource, EngineInfo, Image, ModelsLoaded, Profile, CONTRACT,
    };

    struct Fake;

    impl Reader for Fake {
        fn read(
            &mut self,
            req: &PageRequest,
            image: &Path,
            _: Instant,
        ) -> Result<Reading, ReadError> {
            let bytes = std::fs::read(image).map_err(|e| ReadError::BadInput(e.to_string()))?;
            if bytes == b"crash" {
                return Err(ReadError::Failed {
                    kind: "engine".into(),
                    message: "x".repeat(5000),
                    restart: true,
                });
            }
            Ok(Reading::Text {
                text: format!(
                    "read {} in {}",
                    String::from_utf8_lossy(&bytes),
                    req.language
                ),
                markup: Markup::None,
            })
        }
    }

    pub(crate) fn hello() -> Hello {
        Hello {
            adapter: Adapter {
                lang: "rust".into(),
                name: "fake".into(),
                version: "0".into(),
            },
            cancel: Cancel::Adapter,
            contract: CONTRACT,
            deterministic: true,
            engine: EngineInfo {
                components: None,
                family: "fake".into(),
                id: "fake-1".into(),
                version: "1".into(),
            },
            granularity: Granularity::Page,
            layout: Layout::Boxes,
            models: Vec::new(),
            models_loaded: ModelsLoaded::AtStart,
            profiles: vec![Profile {
                id: "eng".into(),
                languages: vec!["eng".into()],
                models: Vec::new(),
                tunable: vec!["psm".into()],
            }],
        }
    }

    fn request(run: &str, profile: &str, language: &str) -> String {
        let req = PageRequest {
            deadline_ms: 10_000,
            image: Image {
                dpi: 300,
                dpi_source: DpiSource::Assumed,
                height: 100,
                path: format!("/in/{run}.jpg"),
                sha256: "00".into(),
                width: 200,
            },
            language: language.into(),
            options: Default::default(),
            out: format!("/out/{run}/"),
            profile: profile.into(),
            run_id: run.into(),
            script: None,
        };
        String::from_utf8(encode(&ToAdapter::Page(req))).unwrap()
    }

    #[test]
    fn engine_text_from_the_finest_level() {
        let json = r#"{"v":1,"units":"px","frame":{"width":100,"height":100},"granularity":"word","reading_order":"engine","blocks":[
          {"bbox":[0,0,50,50],"kind":"text","lines":[{"bbox":[0,0,50,10],"words":[{"bbox":[0,0,10,10],"text":"Die"},{"bbox":[20,0,50,10],"text":"Zeitung"}]},{"bbox":[0,10,50,20],"text":"Seite zwei"}]},
          {"bbox":[50,0,100,50],"kind":"text","text":"Block"}]}"#;
        let b = usnm_alto::boxes::parse(json.as_bytes(), &Default::default()).unwrap();
        assert_eq!(boxes_text(&b), "Die Zeitung\nSeite zwei\n\nBlock");
    }

    #[test]
    fn serves_pages_over_json_lines() {
        let dir = tempfile::tempdir().unwrap();
        let root = Root(dir.path().to_owned());
        for (run, body) in [("r1", "hello"), ("r2", "crash")] {
            std::fs::create_dir_all(dir.path().join("out").join(run)).unwrap();
            std::fs::create_dir_all(dir.path().join("in")).unwrap();
            std::fs::write(dir.path().join(format!("in/{run}.jpg")), body).unwrap();
        }
        let input = [
            request("r1", "eng", "eng"),
            request("r2", "eng", "eng"),
            request("r3", "deu", "deu"),
            request("r4", "eng", "deu"),
        ]
        .concat();
        let mut out = Vec::new();
        serve(&hello(), &mut Fake, &root, input.as_bytes(), &mut out).unwrap();
        let lines: Vec<FromAdapter> = out
            .split(|b| *b == b'\n')
            .filter(|l| !l.is_empty())
            .map(|l| usnm_ocr_contract::engine::decode(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[0], FromAdapter::Hello(hello()));
        let FromAdapter::Result(ok) = &lines[1] else {
            panic!()
        };
        assert_eq!(ok.status, Status::Ok);
        assert_eq!(ok.layout.as_ref().unwrap().path, "/out/r1/boxes.json");
        let boxes = usnm_alto::boxes::parse(
            &std::fs::read(dir.path().join("out/r1/boxes.json")).unwrap(),
            &Default::default(),
        )
        .unwrap();
        assert_eq!(boxes, page_boxes("read hello in eng", 200, 100));
        let FromAdapter::Result(failed) = &lines[2] else {
            panic!()
        };
        assert_eq!((failed.status, failed.restart), (Status::Failed, true));
        assert!(failed.error.as_ref().unwrap().message.len() <= 2048);
        for l in &lines[3..] {
            let FromAdapter::Result(r) = l else { panic!() };
            assert_eq!(r.status, Status::Unsupported);
        }
        // A line that isn't a request ends the adapter.
        assert!(serve(&hello(), &mut Fake, &root, &b"garbage\n"[..], Vec::new()).is_err());
    }
}
