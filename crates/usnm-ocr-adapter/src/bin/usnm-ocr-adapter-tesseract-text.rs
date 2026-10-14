//! A text-only engine adapter: the Tesseract CLI's plain text, one call per
//! page, as a page-sized block in `boxes.json` (layout `boxes`,
//! granularity `page`). It exists to run the worker's path for engines
//! that return text without ALTO (vision-language models, for one) with
//! an engine every node already has. The production Tesseract adapter
//! (13 §13.11 step 5) returns ALTO instead.
//!
//! `usnm-ocr-adapter-tesseract-text --contract 1 --threads 1`

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use clap::Parser;
use usnm_alto::{Granularity, Markup};
use usnm_ocr_adapter::{serve, sha256_file, ReadError, Reader, Reading, Root};
use usnm_ocr_contract::engine::{
    Adapter, Cancel, EngineInfo, Hello, Layout, Model, ModelsLoaded, PageRequest, Profile, CONTRACT,
};

#[derive(Parser)]
#[command(about = "Text-only Tesseract adapter for the engine contract (usnm-ocr-engine/1)")]
struct Args {
    /// The contract version the worker speaks.
    #[arg(long)]
    contract: u32,
    /// OpenMP threads per page.
    #[arg(long, default_value_t = 1)]
    threads: u32,
    /// The Tesseract binary.
    #[arg(long, default_value = "tesseract")]
    tesseract_bin: PathBuf,
    /// Where the requests' `/in` and `/out` are: `/` in a container.
    #[arg(long, default_value = "/")]
    root: PathBuf,
}

struct Tesseract {
    bin: PathBuf,
    threads: u32,
}

/// `tesseract 5.5.1` from `tesseract --version`.
fn version(bin: &Path) -> anyhow::Result<String> {
    let out = Command::new(bin)
        .arg("--version")
        .stdin(Stdio::null())
        .output()?;
    let all = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    all.lines()
        .find_map(|l| l.trim().strip_prefix("tesseract "))
        .map(|v| v.trim().trim_start_matches('v').to_owned())
        .filter(|v| !v.is_empty())
        .context("no version from tesseract --version")
}

/// The tessdata directory and the models in it, from `--list-langs`.
fn models(bin: &Path) -> anyhow::Result<(PathBuf, Vec<String>)> {
    let out = Command::new(bin)
        .arg("--list-langs")
        .stdin(Stdio::null())
        .output()?;
    let all = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let mut lines = all
        .lines()
        .skip_while(|l| !l.starts_with("List of available languages"));
    let head = lines
        .next()
        .context("no model list from tesseract --list-langs")?;
    let dir = head
        .split('"')
        .nth(1)
        .context("no tessdata directory in the model list")?;
    let names = lines
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.contains(' ') && *l != "osd")
        .map(str::to_owned)
        .collect();
    Ok((PathBuf::from(dir), names))
}

fn hello(bin: &Path) -> anyhow::Result<Hello> {
    let version = version(bin)?;
    let (dir, names) = models(bin)?;
    let mut models = Vec::new();
    let mut profiles = Vec::new();
    for name in names {
        let file = format!("{name}.traineddata");
        let sha256 = sha256_file(&dir.join(&file)).with_context(|| format!("hashing {file}"))?;
        let model = format!("tessdata/{file}");
        models.push(Model {
            name: model.clone(),
            sha256,
        });
        // A model named for its language (`deu`, `deu_latf`) reads it.
        let lang: String = name.chars().take_while(char::is_ascii_lowercase).collect();
        if lang.len() == 3 {
            profiles.push(Profile {
                id: name,
                languages: vec![lang],
                models: vec![model],
                tunable: vec!["psm".into()],
            });
        }
    }
    Ok(Hello {
        adapter: Adapter {
            lang: "rust".into(),
            name: "usnm-ocr-adapter-tesseract-text".into(),
            version: env!("CARGO_PKG_VERSION").into(),
        },
        cancel: Cancel::Adapter,
        contract: CONTRACT,
        deterministic: true,
        engine: EngineInfo {
            components: None,
            family: "tesseract".into(),
            id: "tesseract-text".into(),
            version,
        },
        granularity: Granularity::Page,
        layout: Layout::Boxes,
        models,
        models_loaded: ModelsLoaded::PerPage,
        profiles,
    })
}

impl Reader for Tesseract {
    fn read(
        &mut self,
        req: &PageRequest,
        image: &Path,
        deadline: Instant,
    ) -> Result<Reading, ReadError> {
        let psm = req.options.get("psm").and_then(|v| v.as_u64()).unwrap_or(3);
        let mut cmd = Command::new(&self.bin);
        cmd.arg(image)
            .arg("stdout")
            .args(["-l", &req.profile, "--psm", &psm.to_string()])
            .args(["--dpi", &req.image.dpi.to_string()])
            .env("OMP_THREAD_LIMIT", self.threads.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // Its own process group, so a timeout stops everything it started.
            cmd.process_group(0);
        }
        eprintln!("run {}: {:?}", req.run_id, cmd);
        let failed = |message: String| ReadError::Failed {
            kind: "engine".into(),
            message,
            restart: false,
        };
        let mut child = cmd
            .spawn()
            .map_err(|e| failed(format!("starting tesseract: {e}")))?;
        let mut stdout = child.stdout.take().expect("piped");
        let mut stderr = child.stderr.take().expect("piped");
        let out = std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = stdout.read_to_end(&mut b);
            b
        });
        let err = std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = stderr.read_to_end(&mut b);
            b
        });
        // Stops the whole process group, so nothing it started keeps
        // working into the next page (13 §13.6.5), then reaps the child.
        let stop = |child: &mut std::process::Child| {
            #[cfg(unix)]
            if let Some(pid) = rustix::process::Pid::from_raw(child.id() as i32) {
                let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
            }
            let _ = child.kill();
            let _ = child.wait();
        };
        let status = loop {
            match child.try_wait() {
                Ok(Some(s)) => break s,
                Ok(None) if Instant::now() >= deadline => {
                    stop(&mut child);
                    return Err(ReadError::Timeout);
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(e) => {
                    stop(&mut child);
                    return Err(failed(format!("waiting for tesseract: {e}")));
                }
            }
        };
        let text = out.join().unwrap_or_default();
        let log = String::from_utf8_lossy(&err.join().unwrap_or_default()).into_owned();
        if !status.success() {
            let tail: String = log
                .chars()
                .rev()
                .take(400)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            return Err(failed(format!("tesseract exited with {status}: {tail}")));
        }
        let text =
            String::from_utf8(text).map_err(|_| failed("tesseract's text isn't UTF-8".into()))?;
        Ok(Reading::Text {
            text,
            markup: Markup::None,
        })
    }
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    if args.contract != CONTRACT {
        bail!(
            "this adapter speaks contract {CONTRACT}, not {}",
            args.contract
        );
    }
    let hello = hello(&args.tesseract_bin)?;
    let mut reader = Tesseract {
        bin: args.tesseract_bin,
        threads: args.threads.max(1),
    };
    let stdin = std::io::stdin().lock();
    let stdout = std::io::stdout().lock();
    serve(&hello, &mut reader, &Root(args.root), stdin, stdout)
}
