//! OCR engines. Tesseract to start; the engine survey (#296) may add others
//! behind the same trait.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context};
use async_trait::async_trait;

/// What an engine read on a page.
#[derive(Debug)]
pub struct EngineOutput {
    /// Plain text, as the engine wrote it (not normalized).
    pub text: String,
    /// ALTO XML.
    pub alto: Vec<u8>,
    /// Engine, version and models, e.g. `tesseract-5.5.1/deu_latf+deu`.
    pub engine: String,
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// No model for the language on this node: retrying here won't help.
    #[error("no model for language `{0}` on this node")]
    Unsupported(String),
    #[error(transparent)]
    Failed(#[from] anyhow::Error),
}

#[async_trait]
pub trait Engine: Send + Sync + std::fmt::Debug {
    /// `ocr_source` for this engine's rows, e.g. `usnm-tesseract`.
    fn source(&self) -> &str;
    /// Read `image` (a file) for a page in `language` (ISO 639-3), using
    /// `work` for scratch files.
    async fn ocr(
        &self,
        image: &Path,
        language: &str,
        work: &Path,
    ) -> Result<EngineOutput, EngineError>;
}

/// Tesseract models to try for a language, best first. A `+` joins models
/// read together; a candidate is used only when all its models are installed.
/// German: `deu_latf` (Fraktur, tessdata 4.1+) with `deu` for pages that mix
/// in roman type, else the older `frk`, else `deu`.
pub fn candidates(language: &str) -> Vec<String> {
    let list: &[&str] = match language {
        "deu" => &["deu_latf+deu", "frk+deu", "deu"],
        // Serbian papers print Cyrillic and Latin.
        "srp" => &["srp+srp_latn", "srp"],
        "hrv" | "bos" => &["hrv", "srp_latn"],
        "nob" | "nno" | "nor" => &["nor"],
        "zho" => &["chi_tra", "chi_sim"],
        "yid" => &["yid", "heb"],
        "ell" | "grc" => &["ell", "grc"],
        other => return vec![other.to_owned()],
    };
    list.iter().map(|s| (*s).to_owned()).collect()
}

/// The first candidate whose models are all in `installed`.
pub fn route(language: &str, installed: &BTreeSet<String>) -> Option<String> {
    candidates(language)
        .into_iter()
        .find(|c| c.split('+').all(|m| installed.contains(m)))
}

#[derive(Debug, Clone)]
pub struct Tesseract {
    bin: PathBuf,
    version: String,
    installed: BTreeSet<String>,
    psm: u8,
    dpi: u32,
    threads: u32,
    timeout: Duration,
}

/// `tesseract 5.5.1` → `5.5.1`, from `tesseract --version`'s first line
/// (stdout in 5.x, stderr in older releases).
fn parse_version(out: &str) -> Option<String> {
    let line = out
        .lines()
        .find(|l| l.trim_start().starts_with("tesseract "))?;
    let v = line.trim().strip_prefix("tesseract ")?.trim();
    let v = v.strip_prefix('v').unwrap_or(v);
    (!v.is_empty()
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-')))
    .then(|| v.to_owned())
}

/// The model names in `tesseract --list-langs` output.
fn parse_langs(out: &str) -> BTreeSet<String> {
    out.lines()
        .skip_while(|l| !l.starts_with("List of available languages"))
        .skip(1)
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.contains(' '))
        .map(str::to_owned)
        .collect()
}

#[derive(Debug, Clone)]
pub struct TesseractOptions {
    pub bin: PathBuf,
    /// Page segmentation mode (3: automatic, no orientation detection).
    pub psm: u8,
    /// Resolution to assume: IIIF JPEGs carry none, and Tesseract guesses 70.
    pub dpi: u32,
    /// OpenMP threads per page (`OMP_THREAD_LIMIT`): 1 when pages run in parallel.
    pub threads: u32,
    pub timeout: Duration,
}

impl Default for TesseractOptions {
    fn default() -> Self {
        Self {
            bin: "tesseract".into(),
            psm: 3,
            dpi: 300,
            threads: 1,
            timeout: Duration::from_secs(600),
        }
    }
}

async fn run(bin: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = tokio::process::Command::new(bin)
        .args(args)
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("running {}", bin.display()))?;
    if !out.status.success() {
        bail!("{} {:?} exited with {}", bin.display(), args, out.status);
    }
    Ok(format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    ))
}

impl Tesseract {
    /// Find Tesseract's version and installed models (once, at startup).
    pub async fn detect(opts: TesseractOptions) -> anyhow::Result<Self> {
        let version = parse_version(&run(&opts.bin, &["--version"]).await?)
            .context("can't read `tesseract --version`")?;
        let installed = parse_langs(&run(&opts.bin, &["--list-langs"]).await?);
        Ok(Self {
            bin: opts.bin,
            version,
            installed,
            psm: opts.psm,
            dpi: opts.dpi,
            threads: opts.threads.max(1),
            timeout: opts.timeout,
        })
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn installed(&self) -> &BTreeSet<String> {
        &self.installed
    }

    /// The `ocr_engine` value for a language, if it has a model here.
    pub fn engine_for(&self, language: &str) -> Option<String> {
        route(language, &self.installed).map(|m| format!("tesseract-{}/{m}", self.version))
    }
}

/// Point ALTO's `sourceImageInformation/fileName` at the scan's URL rather
/// than the scratch file it was read from.
pub fn set_alto_file_name(alto: &str, name: &str) -> String {
    let escaped = name
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    match (alto.find("<fileName>"), alto.find("</fileName>")) {
        (Some(a), Some(b)) if a < b => {
            format!("{}<fileName>{escaped}{}", &alto[..a], &alto[b..])
        }
        _ => alto.to_owned(),
    }
}

#[async_trait]
impl Engine for Tesseract {
    fn source(&self) -> &str {
        "usnm-tesseract"
    }

    async fn ocr(
        &self,
        image: &Path,
        language: &str,
        work: &Path,
    ) -> Result<EngineOutput, EngineError> {
        let models = route(language, &self.installed)
            .ok_or_else(|| EngineError::Unsupported(language.to_owned()))?;
        let base = work.join("page");
        // `txt` and `alto` are Tesseract's config files for the two outputs
        // (page.txt, page.xml), written in one pass.
        let child = tokio::process::Command::new(&self.bin)
            .arg(image)
            .arg(&base)
            .args(["-l", &models, "--psm", &self.psm.to_string()])
            .args(["--dpi", &self.dpi.to_string()])
            .args(["txt", "alto"])
            .env("OMP_THREAD_LIMIT", self.threads.to_string())
            .kill_on_drop(true)
            .output();
        let out = tokio::time::timeout(self.timeout, child)
            .await
            .map_err(|_| anyhow::anyhow!("tesseract took over {:?}", self.timeout))?
            .context("running tesseract")?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            let tail: String = err
                .chars()
                .rev()
                .take(400)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            return Err(anyhow::anyhow!("tesseract exited with {}: {tail}", out.status).into());
        }
        let text = tokio::fs::read_to_string(base.with_extension("txt"))
            .await
            .context("tesseract wrote no text")?;
        let alto = tokio::fs::read(base.with_extension("xml"))
            .await
            .context("tesseract wrote no ALTO")?;
        Ok(EngineOutput {
            text,
            alto,
            engine: format!("tesseract-{}/{models}", self.version),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_german_to_fraktur_when_installed() {
        let set = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<BTreeSet<_>>();
        assert_eq!(
            route("deu", &set(&["deu", "deu_latf", "eng"])).as_deref(),
            Some("deu_latf+deu")
        );
        assert_eq!(
            route("deu", &set(&["deu", "frk"])).as_deref(),
            Some("frk+deu")
        );
        assert_eq!(route("deu", &set(&["deu"])).as_deref(), Some("deu"));
        assert_eq!(route("deu", &set(&["eng"])), None);
        assert_eq!(route("srp", &set(&["srp"])).as_deref(), Some("srp"));
        assert_eq!(route("ces", &set(&["ces"])).as_deref(), Some("ces"));
        assert_eq!(route("eng", &set(&["eng", "osd"])).as_deref(), Some("eng"));
    }

    #[test]
    fn reads_version_and_languages() {
        assert_eq!(
            parse_version("tesseract 5.5.1\n leptonica-1.87.0\n").as_deref(),
            Some("5.5.1")
        );
        assert_eq!(
            parse_version("tesseract v5.3.0.20221214\n").as_deref(),
            Some("5.3.0.20221214")
        );
        assert_eq!(parse_version("nope"), None);
        let langs = parse_langs(
            "List of available languages in \"/usr/share/tesseract/tessdata/\" (3):\neng\nosd\ndeu_latf\n",
        );
        assert_eq!(
            langs.into_iter().collect::<Vec<_>>(),
            ["deu_latf", "eng", "osd"]
        );
    }

    #[test]
    fn alto_names_the_scan() {
        let alto =
            "<sourceImageInformation><fileName>/tmp/x/page.jpg</fileName></sourceImageInformation>";
        assert_eq!(
            set_alto_file_name(alto, "https://tile.loc.gov/a?b&c"),
            "<sourceImageInformation><fileName>https://tile.loc.gov/a?b&amp;c</fileName></sourceImageInformation>"
        );
        assert_eq!(set_alto_file_name("<alto/>", "x"), "<alto/>");
    }

    /// Reads a rendered line with the real Tesseract, when it is installed
    /// with English (skipped otherwise).
    #[tokio::test]
    async fn real_tesseract_reads_text_and_alto() {
        let Ok(t) = Tesseract::detect(TesseractOptions::default()).await else {
            eprintln!("tesseract not installed; skipping");
            return;
        };
        if t.engine_for("eng").is_none() {
            eprintln!("tesseract has no eng model; skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("line.pgm");
        std::fs::write(&img, crate::testimg::text_image("HELLO WORLD")).unwrap();
        let out = t.ocr(&img, "eng", dir.path()).await.unwrap();
        assert!(out.engine.starts_with("tesseract-"), "{}", out.engine);
        assert!(out.engine.ends_with("/eng"));
        assert!(out.text.contains("HELLO"), "read {:?}", out.text);
        assert!(String::from_utf8_lossy(&out.alto).contains("<alto"));
        assert!(matches!(
            t.ocr(&img, "zzz", dir.path()).await,
            Err(EngineError::Unsupported(_))
        ));
    }
}
