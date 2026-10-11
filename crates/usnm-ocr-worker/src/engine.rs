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
    /// Whether this node can read pages in `language` (ISO 639-3). The
    /// worker asks before it fetches a scan, so a page with no model here
    /// costs no image request (12 Step 7). The engine contract (ADR-0016,
    /// #307) answers it from routing.
    fn can_read(&self, language: &str) -> bool;
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

/// Where Tesseract runs: a binary on the host, or a pinned container image
/// run with podman (ADR-0015: EL9 has no Tesseract 5, so the node's comes as
/// an image). The engine contract (ADR-0016, #307) replaces this with a
/// long-lived instance per engine; until then each page is one container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Runner {
    Host(PathBuf),
    Podman {
        /// The podman binary.
        podman: PathBuf,
        /// `--url` for `podman --remote` (a Podman API socket), or none for
        /// podman run by the worker's own user.
        url: Option<String>,
        /// `sha256:<id>` or `<name>@sha256:<digest>`: never a mutable tag.
        image: String,
    },
}

/// A container image reference that names its content: an image id
/// (`sha256:` and 64 hex digits) or a name pinned by digest
/// (`<name>@sha256:<digest>`). Tags can move, so they are refused.
pub fn check_image_ref(image: &str) -> anyhow::Result<()> {
    let hex64 =
        |s: &str| s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    let ok = match image.split_once("@sha256:") {
        Some((name, digest)) => {
            hex64(digest)
                && !name.is_empty()
                && name.bytes().all(|b| {
                    b.is_ascii_lowercase()
                        || b.is_ascii_digit()
                        || matches!(b, b'.' | b'/' | b'-' | b'_' | b':')
                })
        }
        None => image.strip_prefix("sha256:").is_some_and(hex64),
    };
    if !ok {
        bail!("Tesseract image `{image}` must be `sha256:<id>` or `<name>@sha256:<digest>`, not a tag");
    }
    Ok(())
}

/// Removes a page's container if the page is cancelled or times out while
/// it runs: killing the podman client leaves the container running.
struct ContainerGuard {
    podman: PathBuf,
    url: Option<String>,
    name: String,
    armed: bool,
}

impl Drop for ContainerGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut cmd = tokio::process::Command::new(&self.podman);
        if let Some(url) = &self.url {
            cmd.args(["--remote", "--url", url]);
        }
        cmd.args(["rm", "--force", "--time", "0", &self.name])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let name = self.name.clone();
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                // `podman rm` can fail while the container is still
                // stopping; try a few times, and say so if it never goes.
                for attempt in 1..=3u32 {
                    match cmd.status().await {
                        Ok(s) if s.success() => return,
                        Ok(s) => tracing::warn!(container = %name, attempt, status = %s, "podman rm failed"),
                        Err(e) => tracing::warn!(container = %name, attempt, error = %e, "podman rm didn't run"),
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(5 * u64::from(attempt))).await;
                }
                tracing::error!(container = %name, "couldn't remove a stopped page's container; remove it by hand");
            });
        }
    }
}

impl Runner {
    /// The command for `tesseract <args>`, with `work` (when given) shared
    /// as `/work`, and the paths in `args` already relative to it. The guard
    /// removes the container if the command is dropped before it ends.
    fn command(
        &self,
        threads: u32,
        work: Option<&Path>,
    ) -> (tokio::process::Command, Option<ContainerGuard>) {
        match self {
            Runner::Host(bin) => {
                let mut cmd = tokio::process::Command::new(bin);
                cmd.env("OMP_THREAD_LIMIT", threads.to_string());
                if let Some(w) = work {
                    cmd.current_dir(w);
                }
                (cmd, None)
            }
            Runner::Podman { podman, url, image } => {
                let name = format!("usnm-tesseract-{}", crate::queue::random_token());
                let mut cmd = tokio::process::Command::new(podman);
                if let Some(url) = url {
                    cmd.args(["--remote", "--url", url]);
                }
                // No network, a read-only root, no capabilities and no
                // privilege gain: the engine only reads the scan in /work
                // and writes its output there.
                cmd.args([
                    "run",
                    "--rm",
                    "--pull=never",
                    "--network=none",
                    "--read-only",
                ])
                .args(["--cap-drop=all", "--security-opt=no-new-privileges"])
                .args(["--userns=keep-id", "--name", &name])
                .args(["--env", &format!("OMP_THREAD_LIMIT={threads}")])
                .args(["--entrypoint", "tesseract"]);
                if let Some(w) = work {
                    // :Z gives the page's private scratch directory the
                    // container's SELinux label. The engine runs as the
                    // directory's owner (the worker's user, mapped to itself
                    // by keep-id), not the image's user, so it can write its
                    // output there.
                    cmd.args(["--volume", &format!("{}:/work:Z", w.display())])
                        .args(["--workdir", "/work"]);
                    #[cfg(unix)]
                    if let Ok(meta) = std::fs::metadata(w) {
                        use std::os::unix::fs::MetadataExt;
                        cmd.args(["--user", &format!("{}:{}", meta.uid(), meta.gid())]);
                    }
                }
                cmd.arg(image);
                let guard = ContainerGuard {
                    podman: podman.clone(),
                    url: url.clone(),
                    name,
                    armed: true,
                };
                (cmd, Some(guard))
            }
        }
    }

    fn describe(&self) -> String {
        match self {
            Runner::Host(bin) => bin.display().to_string(),
            Runner::Podman { image, .. } => format!("podman image {image}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Tesseract {
    runner: Runner,
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
    pub runner: Runner,
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
            runner: Runner::Host("tesseract".into()),
            psm: 3,
            dpi: 300,
            threads: 1,
            timeout: Duration::from_secs(600),
        }
    }
}

async fn run(runner: &Runner, args: &[&str]) -> anyhow::Result<String> {
    let (mut cmd, mut guard) = runner.command(1, None);
    let out = cmd
        .args(args)
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("running tesseract ({})", runner.describe()))?;
    if let Some(g) = guard.as_mut() {
        g.armed = false;
    }
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!(
            "tesseract {:?} ({}) exited with {}: {}",
            args,
            runner.describe(),
            out.status,
            tail(&err, 400)
        );
    }
    Ok(format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    ))
}

/// The last `n` characters of `s`.
fn tail(s: &str, n: usize) -> String {
    let skip = s.chars().count().saturating_sub(n);
    s.chars().skip(skip).collect()
}

impl Tesseract {
    /// Find Tesseract's version and installed models (once, at startup).
    pub async fn detect(opts: TesseractOptions) -> anyhow::Result<Self> {
        if let Runner::Podman { image, .. } = &opts.runner {
            check_image_ref(image)?;
        }
        let version = parse_version(&run(&opts.runner, &["--version"]).await?)
            .context("can't read `tesseract --version`")?;
        let installed = parse_langs(&run(&opts.runner, &["--list-langs"]).await?);
        Ok(Self {
            runner: opts.runner,
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

    pub fn runner(&self) -> &Runner {
        &self.runner
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

    fn can_read(&self, language: &str) -> bool {
        route(language, &self.installed).is_some()
    }

    async fn ocr(
        &self,
        image: &Path,
        language: &str,
        work: &Path,
    ) -> Result<EngineOutput, EngineError> {
        let models = route(language, &self.installed)
            .ok_or_else(|| EngineError::Unsupported(language.to_owned()))?;
        // Tesseract runs in `work`, on paths relative to it, so the same
        // arguments serve the host binary and a container with `work` as
        // /work. A scan from elsewhere is copied in first.
        let input = match image.parent() {
            Some(p) if p == work => image.to_owned(),
            _ => {
                let ext = image.extension().and_then(|e| e.to_str()).unwrap_or("img");
                let copy = work.join(format!("input.{ext}"));
                tokio::fs::copy(image, &copy)
                    .await
                    .context("copying the scan into the scratch directory")?;
                copy
            }
        };
        let input_name = input
            .file_name()
            .and_then(|n| n.to_str())
            .context("scan file name isn't UTF-8")?
            .to_owned();
        let base = work.join("page");
        let (mut cmd, mut guard) = self.runner.command(self.threads, Some(work));
        // `txt` and `alto` are Tesseract's config files for the two outputs
        // (page.txt, page.xml), written in one pass.
        let child = cmd
            .args([input_name.as_str(), "page"])
            .args(["-l", &models, "--psm", &self.psm.to_string()])
            .args(["--dpi", &self.dpi.to_string()])
            .args(["txt", "alto"])
            .kill_on_drop(true)
            .output();
        let out = tokio::time::timeout(self.timeout, child)
            .await
            .map_err(|_| anyhow::anyhow!("tesseract took over {:?}", self.timeout))?
            .context("running tesseract")?;
        if let Some(g) = guard.as_mut() {
            g.armed = false;
        }
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(anyhow::anyhow!(
                "tesseract exited with {}: {}",
                out.status,
                tail(&err, 400)
            )
            .into());
        }
        // What Tesseract said, for a run that exits 0 but writes nothing.
        let said = || {
            let files: Vec<String> = std::fs::read_dir(work)
                .map(|d| {
                    d.filter_map(|e| e.ok())
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default();
            format!(
                "scratch holds {files:?}; tesseract said: {}",
                tail(&String::from_utf8_lossy(&out.stderr), 400)
            )
        };
        let text = tokio::fs::read_to_string(base.with_extension("txt"))
            .await
            .with_context(|| format!("tesseract wrote no text ({})", said()))?;
        let alto = tokio::fs::read(base.with_extension("xml"))
            .await
            .with_context(|| format!("tesseract wrote no ALTO ({})", said()))?;
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
    fn image_must_be_pinned() {
        let id = "a".repeat(64);
        assert!(check_image_ref(&format!("sha256:{id}")).is_ok());
        assert!(check_image_ref(&format!("ghcr.io/x/tesseract@sha256:{id}")).is_ok());
        assert!(check_image_ref(&format!("localhost/usnm-ocr/tesseract:5.5@sha256:{id}")).is_ok());
        assert!(check_image_ref("usnm-ocr/tesseract:latest").is_err());
        assert!(check_image_ref("sha256:abc").is_err());
        assert!(check_image_ref(&format!("sha256:{}", "A".repeat(64))).is_err());
        assert!(check_image_ref(&format!("@sha256:{id}")).is_err());
        assert!(check_image_ref(&format!("x y@sha256:{id}")).is_err());
    }

    #[test]
    fn podman_runs_the_image_without_network_or_privileges() {
        let image = format!("sha256:{}", "b".repeat(64));
        let runner = Runner::Podman {
            podman: "podman".into(),
            url: Some("unix:///run/usnm-ocr/podman.sock".into()),
            image: image.clone(),
        };
        let (cmd, guard) = runner.command(2, Some(Path::new("/tmp/w")));
        let mut guard = guard.expect("a container has a guard");
        guard.armed = false;
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let has = |a: &str| args.iter().any(|x| x == a);
        assert_eq!(
            args[..3],
            ["--remote", "--url", "unix:///run/usnm-ocr/podman.sock"]
        );
        for a in [
            "run",
            "--rm",
            "--pull=never",
            "--network=none",
            "--read-only",
            "--cap-drop=all",
            "--security-opt=no-new-privileges",
            "OMP_THREAD_LIMIT=2",
            "/tmp/w:/work:Z",
        ] {
            assert!(has(a), "{a} missing from {args:?}");
        }
        assert_eq!(args.last(), Some(&image));
        // With a real scratch directory, the engine runs as its owner.
        let dir = tempfile::tempdir().unwrap();
        let (cmd, guard) = runner.command(1, Some(dir.path()));
        guard.unwrap().armed = false;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let meta = std::fs::metadata(dir.path()).unwrap();
            let want = format!("{}:{}", meta.uid(), meta.gid());
            assert!(cmd.as_std().get_args().any(|a| a == want.as_str()));
        }
        let (cmd, guard) = Runner::Host("tesseract".into()).command(1, None);
        assert!(guard.is_none());
        assert_eq!(cmd.as_std().get_program(), "tesseract");
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
