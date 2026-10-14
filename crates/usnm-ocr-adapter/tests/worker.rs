//! The text-only adapter end to end through the worker's contract engine:
//! long-lived instances over JSON lines, `boxes.json` at page granularity,
//! and the stored ALTO and text the worker derives from it. A stand-in
//! `tesseract` script keeps the test independent of the host's models; a
//! second test uses the real Tesseract when it is installed with English.

use std::path::{Path, PathBuf};
use std::time::Duration;

use usnm_ocr_worker::contract::{ContractEngine, ContractOptions, Launch};
use usnm_ocr_worker::engine::{Engine, EngineError};
use usnm_ocr_worker::stored::{self, Source};

const ADAPTER: &str = env!("CARGO_BIN_EXE_usnm-ocr-adapter-tesseract-text");

/// A `tesseract` that answers `--version` and `--list-langs`, and "reads"
/// an image by what it holds: `crash` exits 1, `slow` sleeps.
fn stand_in(dir: &Path) -> PathBuf {
    let tessdata = dir.join("tessdata");
    std::fs::create_dir_all(&tessdata).unwrap();
    std::fs::write(tessdata.join("eng.traineddata"), b"model").unwrap();
    let script = dir.join("tesseract");
    std::fs::write(
        &script,
        format!(
            r#"#!/bin/sh
case "$1" in
  --version) echo "tesseract 5.5.9"; exit 0 ;;
  --list-langs) echo "List of available languages in \"{}/\" (1):"; echo eng; exit 0 ;;
esac
case "$(cat "$1")" in
  *crash) echo "Error in pixReadStream" >&2; exit 1 ;;
  *slow) sleep 5 ;;
  *long) i=0; while [ $i -lt 400 ]; do printf 'word%03d ' $i; i=$((i+1)); done; echo; exit 0 ;;
esac
printf 'THE EVENING GAZETTE\nThe council met on Mon-\nday.\n\n\f'
"#,
            tessdata.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    script
}

async fn engine(tesseract: &Path, scratch: &Path, page_timeout: Duration) -> ContractEngine {
    ContractEngine::start(ContractOptions {
        launch: Launch::Local {
            program: ADAPTER.into(),
            args: vec![format!("--tesseract-bin={}", tesseract.display())],
            scratch: scratch.to_owned(),
        },
        instances: 1,
        threads: 1,
        start_timeout: Duration::from_secs(30),
        page_timeout,
        max_jobs: 100,
        dpi: 300,
        node: "test1".into(),
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn a_text_only_engine_through_long_lived_instances() {
    let dir = tempfile::tempdir().unwrap();
    let tesseract = stand_in(dir.path());
    let scratch = dir.path().join("scratch");
    std::fs::create_dir(&scratch).unwrap();
    let e = engine(&tesseract, &scratch, Duration::from_secs(2)).await;
    assert_eq!(e.source(), "usnm-tesseract");
    assert!(e.can_read("eng") && !e.can_read("deu"));
    let image = dir.path().join("page.pgm");
    std::fs::write(&image, usnm_ocr_worker::testimg::text_image("HELLO")).unwrap();
    let work = dir.path();

    for _ in 0..3 {
        let mut out = e.ocr(&image, "eng", work).await.unwrap();
        assert!(
            out.engine
                .starts_with("tesseract/5.5.9 profile=eng image=local models=sha256:"),
            "{}",
            out.engine
        );
        assert!(out.engine.ends_with(" contract=1"), "{}", out.engine);
        let page = out.page.as_ref().expect("a page from boxes.json");
        assert_eq!(page.granularity, usnm_alto::Granularity::Page);
        assert_eq!((page.width, page.height), (260.0, 122.0));
        let s = stored::from_output(
            &mut out,
            "eng",
            300,
            &Source {
                url: "https://tile.loc.gov/x/full/full/0/default.jpg",
                sha256: "ab",
                md5: None,
            },
            "0.1.0",
            chrono::Utc::now(),
        )
        .unwrap();
        assert_eq!(s.text, "THE EVENING GAZETTE\nThe council met on Monday.");
        usnm_alto::check(s.alto.as_bytes(), &usnm_alto::Limits::default()).unwrap();
        assert!(s.alto.contains("granularity=page"));
        // One page-sized block: every line and word has the page's box.
        assert!(s.alto.contains("<TextBlock ID=\"TB1\" HPOS=\"0\" VPOS=\"0\" WIDTH=\"260\" HEIGHT=\"122\" LANG=\"eng\">"));
    }
    // A page's text is one block, well over 1 KiB.
    let long = dir.path().join("long.pgm");
    std::fs::write(&long, b"P5 4 4 255 long").unwrap();
    let out = e.ocr(&long, "eng", work).await.unwrap();
    let words = out.page.as_ref().unwrap().blocks[0].lines[0].words.len();
    assert_eq!(words, 400);
    // A language no profile reads is refused before any engine call.
    assert!(matches!(
        e.ocr(&image, "deu", work).await,
        Err(EngineError::Unsupported(_))
    ));
    // An engine failure fails the page, and the next page still reads.
    // (The stand-in reads the file's content; a PGM header with "crash"
    // after it still has a size the worker can read.)
    let crash_img = dir.path().join("crash2.pgm");
    std::fs::write(&crash_img, b"P5 4 4 255 crash").unwrap();
    let err = e.ocr(&crash_img, "eng", work).await.unwrap_err();
    assert!(format!("{err:#}").contains("engine failed"), "{err:#}");
    assert!(e.ocr(&image, "eng", work).await.is_ok());
    // The adapter stops a page at its deadline and answers timeout.
    let slow = dir.path().join("slow.pgm");
    std::fs::write(&slow, b"P5 4 4 255 slow").unwrap();
    let err = e.ocr(&slow, "eng", work).await.unwrap_err();
    assert!(format!("{err:#}").contains("engine timeout"), "{err:#}");
    assert!(e.ocr(&image, "eng", work).await.is_ok());
    // Nothing is left in the instance's directories between pages.
    let left: Vec<_> = walk(&scratch).into_iter().filter(|p| p.is_file()).collect();
    assert!(left.is_empty(), "{left:?}");
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        }
        out.push(p);
    }
    out
}

/// With the real Tesseract, when it is installed with English.
#[tokio::test]
async fn the_real_tesseract_as_a_text_only_engine() {
    let has_eng = std::process::Command::new("tesseract")
        .arg("--list-langs")
        .output()
        .is_ok_and(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .any(|l| l.trim() == "eng")
        });
    if !has_eng {
        eprintln!("tesseract with eng isn't installed; skipping");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let e = engine(Path::new("tesseract"), dir.path(), Duration::from_secs(60)).await;
    let image = dir.path().join("line.pgm");
    std::fs::write(&image, usnm_ocr_worker::testimg::text_image("HELLO")).unwrap();
    let mut out = e.ocr(&image, "eng", dir.path()).await.unwrap();
    let s = stored::from_output(
        &mut out,
        "eng",
        300,
        &Source {
            url: "line.pgm",
            sha256: "",
            md5: None,
        },
        "0.1.0",
        chrono::Utc::now(),
    )
    .unwrap();
    assert!(s.text.contains("HELLO"), "read {:?}", s.text);
}

/// An adapter that exits in the middle of a page: the page fails, and the
/// next page gets a new instance.
#[tokio::test]
async fn an_instance_that_exits_is_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let hello = serde_json::json!({
        "adapter": {"lang": "rust", "name": "crash", "version": "0"},
        "cancel": "kill", "contract": 1, "deterministic": true,
        "engine": {"family": "tesseract", "id": "crash", "version": "1"},
        "granularity": "page", "layout": "boxes", "models": [], "models_loaded": "at_start",
        "profiles": [{"id": "eng", "languages": ["eng"], "models": [], "tunable": []}],
        "type": "hello"
    });
    let script = dir.path().join("adapter");
    std::fs::write(
        &script,
        format!("#!/bin/sh\necho '{hello}'\nread line\necho 'started on a page' >&2\nexit 3\n"),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let e = ContractEngine::start(ContractOptions {
        launch: Launch::Local {
            program: script,
            args: Vec::new(),
            scratch: dir.path().to_owned(),
        },
        instances: 1,
        threads: 1,
        start_timeout: Duration::from_secs(10),
        page_timeout: Duration::from_secs(10),
        max_jobs: 100,
        dpi: 300,
        node: "test1".into(),
    })
    .await
    .unwrap();
    let image = dir.path().join("page.pgm");
    std::fs::write(&image, usnm_ocr_worker::testimg::text_image("HELLO")).unwrap();
    for _ in 0..2 {
        let err = e.ocr(&image, "eng", dir.path()).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("the engine instance exited"), "{msg}");
    }
}
