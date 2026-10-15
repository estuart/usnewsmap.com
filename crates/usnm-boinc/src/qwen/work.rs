//! One Qwen unit inside BOINC's job container. No network beyond the
//! loopback to llama-server, which this process starts.
//!
//! The unit always ends with its three output files written, whatever
//! happened:
//!
//! 1. Diagnostics as the Surya worker writes them: selected fields of
//!    `init_data.xml`, the container runtime, the GPU the container sees.
//! 2. Unless the unit is broken (input checksum, an engine that differs
//!    from the job), the model for the host is chosen (8B on a GPU with
//!    [`super::GPU_MB_FOR_8B`] MB free, else 2B), llama-server is started
//!    as a child on 127.0.0.1 with a free port, and each crop is cut from
//!    the page as a PNG and sent as a data URL in an OpenAI-style chat
//!    completion with the fixed prompt at temperature 0, within the unit's
//!    deadline.
//! 3. `readings.json`, `result.json` and `diag.txt`.
//!
//! llama-server runs in its own process group; its log goes to a scratch
//! directory and its device lines, version and the layers offloaded into
//! the readings' `diag`. While it runs, a thread samples its RSS and CPU
//! time every 0.5 s and the GPU's memory every 2 s, as the Surya worker
//! does. The requests go over a plain HTTP/1.1 client on the loopback
//! (`http`): no HTTP library, nothing leaves the container.
use super::{
    choose_model, outputs, DIAG_FILE, ENV_LLAMA_IMAGE, ENV_WEIGHTS, JOB_FILE, MODEL_DIR, PAGE_FILE,
    READINGS_FILE, RESULT_FILE,
};
use crate::surya::work::{
    boinc_info, container_info, gpu_info, gpu_mem_used, kill_all, llama_device, others, quick,
    reap, runner_stats, tail, Diag, TICKS,
};
use crate::{atomic, read};
use anyhow::{ensure, Context};
use image::GenericImageView as _;
use std::{
    collections::BTreeMap,
    fmt::Write as _,
    os::unix::process::CommandExt as _,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use usnm_ocr_contract::{
    boinc::sha256,
    quality::MAX_SCAN,
    qwen::{
        Crop, Engine, Job, Model, Outcome, Reading, Readings, ResultManifest, MAX_CROP_TEXT,
        MAX_ERROR, MAX_JOB_JSON, PROMPT, PROMPT_VERSION, VERSION,
    },
    surya::Host,
};

/// Checks the unit before anything runs: the job, the page and the engine.
pub fn prepare(work: &Path) -> anyhow::Result<(Job, Vec<u8>)> {
    let job: Job = serde_json::from_slice(&read(&work.join(JOB_FILE), MAX_JOB_JSON)?)?;
    job.validate()?;
    let page = read(&work.join(PAGE_FILE), MAX_SCAN)?;
    ensure!(job.input.matches(&page), "input page checksum mismatch");
    for (var, want) in [
        (ENV_LLAMA_IMAGE, &job.engine.llama),
        (ENV_WEIGHTS, &job.engine.weights),
    ] {
        let have = std::env::var(var).with_context(|| format!("the image has no {var}"))?;
        ensure!(
            &have == want,
            "the unit was staged for {var}={want}, this image has {have}"
        );
    }
    Ok((job, page))
}

/// A crop of the page as PNG bytes, for the data URL.
pub fn crop_png(page: &image::DynamicImage, crop: &Crop) -> anyhow::Result<Vec<u8>> {
    let [x0, y0, x1, y1] = crop.rect;
    let (w, h) = page.dimensions();
    ensure!(
        x0 < x1 && y0 < y1 && x1 <= w && y1 <= h,
        "crop outside the page"
    );
    let cut = page.crop_imm(x0, y0, x1 - x0, y1 - y0).to_luma8();
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageLuma8(cut)
        .write_to(&mut out, image::ImageFormat::Png)
        .context("encoding crop")?;
    Ok(out.into_inner())
}

/// The chat completion request body for one crop.
pub fn request_body(png: &[u8], max_tokens: u32) -> serde_json::Value {
    use base64::Engine as _;
    let data = base64::engine::general_purpose::STANDARD.encode(png);
    serde_json::json!({
        "max_tokens": max_tokens,
        "messages": [{
            "role": "user",
            "content": [
                {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{data}")}},
                {"type": "text", "text": PROMPT}
            ]
        }],
        "temperature": 0
    })
}

/// llama.cpp's server binary in the image (the CUDA server image keeps
/// it and its libraries under `/app`).
pub const LLAMA_SERVER: &str = "/app/llama-server";
/// Context for one crop: a 1,600 by 420 px crop is about 650 image
/// tokens at Qwen3-VL's 32 px per token, plus the prompt and the answer.
pub const CTX_SIZE: u32 = 4096;
/// How long the server may take to accept a connection.
const CONNECT: Duration = Duration::from_secs(5);
/// The sampler's period for RSS and CPU time, and for GPU memory.
const SAMPLE_EVERY: Duration = Duration::from_millis(500);
const GPU_SAMPLE_EVERY: Duration = Duration::from_secs(2);
/// A response larger than this is not one of ours.
const MAX_RESPONSE: u64 = 4 << 20;

/// The marker error for a request the deadline cut.
#[derive(Debug)]
pub struct TimedOut;
impl std::fmt::Display for TimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the deadline passed while waiting for llama-server")
    }
}
impl std::error::Error for TimedOut {}

/// Sends one HTTP/1.1 request to the loopback server and returns the
/// status and the body. `Connection: close` makes the server end the
/// response, so the body is everything up to the close; a chunked body
/// is joined. `timeout` bounds each read: the server answers a completion
/// in one piece once it is done, so it is the time the whole request may
/// take.
pub fn http(
    port: u16,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
    timeout: Duration,
) -> anyhow::Result<(u16, Vec<u8>)> {
    use std::io::{Read as _, Write as _};
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let mut s = std::net::TcpStream::connect_timeout(&addr, CONNECT)
        .with_context(|| format!("connecting to 127.0.0.1:{port}"))?;
    s.set_read_timeout(Some(timeout.max(Duration::from_secs(1))))?;
    s.set_write_timeout(Some(Duration::from_secs(60)))?;
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nAccept: application/json\r\n"
    );
    if let Some(b) = body {
        let _ = write!(
            req,
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            b.len()
        );
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes())?;
    if let Some(b) = body {
        s.write_all(b)?;
    }
    let mut raw = Vec::new();
    if let Err(e) = s.take(MAX_RESPONSE).read_to_end(&mut raw) {
        return Err(
            if matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) {
                anyhow::Error::new(TimedOut)
            } else {
                anyhow::Error::new(e).context("reading the response")
            },
        );
    }
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .context("no response header")?;
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let status: u16 = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .context("no status line")?;
    let chunked = head.lines().any(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("transfer-encoding:") && l.contains("chunked")
    });
    let body = raw[split + 4..].to_vec();
    Ok((status, if chunked { dechunk(&body)? } else { body }))
}

/// Joins a chunked transfer encoding.
fn dechunk(mut b: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let nl = b
            .windows(2)
            .position(|w| w == b"\r\n")
            .context("chunk size missing")?;
        let size_text = std::str::from_utf8(&b[..nl])?;
        let size = usize::from_str_radix(size_text.split(';').next().unwrap_or("").trim(), 16)
            .context("chunk size")?;
        b = &b[nl + 2..];
        if size == 0 {
            return Ok(out);
        }
        ensure!(b.len() >= size + 2, "chunk cut short");
        out.extend_from_slice(&b[..size]);
        b = &b[size + 2..];
    }
}

/// A free loopback port: bound and released, for llama-server to take.
fn free_port() -> anyhow::Result<u16> {
    let l = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
    Ok(l.local_addr()?.port())
}

/// What the sampler saw of llama-server while it ran.
#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub peak_rss_kib: u64,
    pub cpu_ticks: BTreeMap<i32, u64>,
    pub gpu_mem_peak_mib: Option<u64>,
}
impl Stats {
    pub fn cpu_secs(&self) -> f64 {
        self.cpu_ticks.values().sum::<u64>() as f64 / TICKS
    }
}

/// A running llama-server: its port, the child, and what it reported.
/// Dropping it kills the server (and, as pid 1, everything else) and
/// stops the sampler.
pub struct Server {
    pub port: u16,
    pub model: String,
    pub device: String,
    pub load_ms: u64,
    pub diag: BTreeMap<String, String>,
    pub stats: Arc<Mutex<Stats>>,
    pub gpu_mem_before_mib: Option<u64>,
    child: Option<std::process::Child>,
    pgid: i32,
    log: PathBuf,
    stop: Arc<AtomicBool>,
    sampler: Option<std::thread::JoinHandle<()>>,
    _scratch: tempfile::TempDir,
}
impl Server {
    /// The server's log so far.
    pub fn log_text(&self) -> String {
        std::fs::read(&self.log)
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default()
    }
    pub fn stats(&self) -> Stats {
        self.stats.lock().map(|s| s.clone()).unwrap_or_default()
    }
    /// A handle to a server this process did not start (a test's fake,
    /// or one already running): nothing is killed when it is dropped.
    #[cfg(test)]
    pub(crate) fn detached(port: u16) -> Self {
        Server {
            port,
            model: "detached".into(),
            device: "none".into(),
            load_ms: 0,
            diag: BTreeMap::new(),
            stats: Arc::new(Mutex::new(Stats::default())),
            gpu_mem_before_mib: None,
            child: None,
            pgid: 0,
            log: PathBuf::new(),
            stop: Arc::new(AtomicBool::new(false)),
            sampler: None,
            _scratch: tempfile::tempdir().expect("tempdir"),
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let Some(child) = self.child.as_mut() else {
            return;
        };
        kill_all(self.pgid);
        let _ = child.wait();
        let gone = Instant::now();
        while !others(self.pgid).is_empty() && gone.elapsed() < Duration::from_secs(15) {
            reap();
            std::thread::sleep(Duration::from_millis(100));
        }
        if let Some(h) = self.sampler.take() {
            let _ = h.join();
        }
    }
}

/// The model's files under [`MODEL_DIR`], checked to exist.
fn model_paths(model: &Model) -> anyhow::Result<(PathBuf, PathBuf)> {
    let dir = Path::new(MODEL_DIR);
    let m = dir.join(&model.file);
    let p = dir.join(&model.mmproj);
    for f in [&m, &p] {
        ensure!(f.is_file(), "model file missing: {}", f.display());
    }
    Ok((m, p))
}

/// Starts llama-server as a child process on 127.0.0.1 with a free port
/// (`-m`, `--mmproj`, `-ngl 99` on a GPU else 0, `--threads`, a context
/// large enough for one crop, `--temp 0`) and waits for `/health`.
pub fn serve(
    model: &Model,
    gpu: Option<u32>,
    threads: u32,
    deadline: Duration,
) -> anyhow::Result<Server> {
    let (model_path, mmproj_path) = model_paths(model)?;
    let scratch = tempfile::tempdir().context("no scratch directory")?;
    let log = scratch.path().join("llama-server.log");
    let port = free_port()?;
    let gpu_before = gpu.and_then(gpu_mem_used);
    let started = Instant::now();
    let mut cmd = Command::new(LLAMA_SERVER);
    if let Some(i) = gpu {
        cmd.env("CUDA_DEVICE_ORDER", "PCI_BUS_ID")
            .env("CUDA_VISIBLE_DEVICES", i.to_string());
    }
    let log_file = std::fs::File::create(&log)?;
    let child = cmd
        .args(["-m", &model_path.to_string_lossy()])
        .args(["--mmproj", &mmproj_path.to_string_lossy()])
        .args(["--host", "127.0.0.1", "--port", &port.to_string()])
        .args(["-ngl", if gpu.is_some() { "99" } else { "0" }])
        .args(["--threads", &threads.to_string()])
        .args(["--ctx-size", &CTX_SIZE.to_string()])
        .args(["--temp", "0", "--parallel", "1", "--no-webui"])
        .env("HOME", scratch.path())
        .stdin(Stdio::null())
        .stdout(log_file.try_clone()?)
        .stderr(log_file)
        .process_group(0)
        .spawn()
        .with_context(|| format!("starting {LLAMA_SERVER}"))?;
    let pgid = i32::try_from(child.id()).context("pid")?;
    let stats = Arc::new(Mutex::new(Stats::default()));
    let stop = Arc::new(AtomicBool::new(false));
    let sampler = {
        let (stats, stop) = (stats.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut last_gpu = Instant::now() - GPU_SAMPLE_EVERY;
            while !stop.load(Ordering::Relaxed) {
                if let Ok(mut s) = stats.lock() {
                    let rss = runner_stats(pgid, &mut s.cpu_ticks);
                    s.peak_rss_kib = s.peak_rss_kib.max(rss);
                    if let Some(i) = gpu.filter(|_| last_gpu.elapsed() >= GPU_SAMPLE_EVERY) {
                        last_gpu = Instant::now();
                        if let Some(u) = gpu_mem_used(i) {
                            s.gpu_mem_peak_mib = Some(s.gpu_mem_peak_mib.map_or(u, |p| p.max(u)));
                        }
                    }
                }
                std::thread::sleep(SAMPLE_EVERY);
            }
        })
    };
    let mut server = Server {
        port,
        model: model.name.clone(),
        device: "none".into(),
        load_ms: 0,
        diag: BTreeMap::new(),
        stats,
        gpu_mem_before_mib: gpu_before,
        child: Some(child),
        pgid,
        log,
        stop,
        sampler: Some(sampler),
        _scratch: scratch,
    };
    // Wait for the model to load: /health answers 503 until it has.
    loop {
        if let Some(status) = server
            .child
            .as_mut()
            .and_then(|c| c.try_wait().ok().flatten())
        {
            anyhow::bail!(
                "llama-server exited while loading ({status}):\n{}",
                tail(&server.log_text(), 2048)
            );
        }
        if started.elapsed() >= deadline {
            anyhow::bail!(
                "llama-server not healthy within {} s:\n{}",
                deadline.as_secs(),
                tail(&server.log_text(), 2048)
            );
        }
        if let Ok((200, _)) = http(port, "GET", "/health", None, Duration::from_secs(5)) {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    server.load_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let log_text = server.log_text();
    let (device, layers, _) = llama_device(&log_text);
    server.device = if gpu.is_some() { device } else { "cpu".into() };
    let version = log_text
        .lines()
        .find(|l| l.starts_with("build:"))
        .unwrap_or("")
        .chars()
        .take(256)
        .collect::<String>();
    server.diag.insert("ctx_size".into(), CTX_SIZE.to_string());
    server
        .diag
        .insert("gpu_layers".into(), layers.unwrap_or_default());
    server.diag.insert("model_file".into(), model.file.clone());
    server.diag.insert("threads".into(), threads.to_string());
    server.diag.insert("version".into(), version);
    Ok(server)
}

/// A reading's text as the contract allows it: no control characters but
/// newline and tab, trimmed, at most [`MAX_CROP_TEXT`] bytes.
pub fn clean_text(s: &str) -> String {
    let mut t: String = s
        .chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect();
    let trimmed = t.trim().len();
    let start = t.len() - t.trim_start().len();
    t = t[start..start + trimmed].to_owned();
    if t.len() > MAX_CROP_TEXT {
        let mut end = MAX_CROP_TEXT;
        while !t.is_char_boundary(end) {
            end -= 1;
        }
        t.truncate(end);
    }
    t
}

/// Sends one crop and returns the reading with its time and token count.
/// `remaining` is what is left of the unit's deadline.
pub fn transcribe(
    server: &Server,
    crop: &Crop,
    png: &[u8],
    max_tokens: u32,
    remaining: Duration,
) -> anyhow::Result<Reading> {
    let body = serde_json::to_vec(&request_body(png, max_tokens))?;
    let started = Instant::now();
    let (status, answer) = http(
        server.port,
        "POST",
        "/v1/chat/completions",
        Some(&body),
        remaining,
    )
    .with_context(|| format!("crop {}", crop.id))?;
    let ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    ensure!(
        status == 200,
        "crop {}: llama-server answered {status}: {}",
        crop.id,
        tail(&String::from_utf8_lossy(&answer), 512)
    );
    let v: serde_json::Value = serde_json::from_slice(&answer).context("completion JSON")?;
    let text = v["choices"][0]["message"]["content"]
        .as_str()
        .context("no choices[0].message.content")?;
    let tokens = v["usage"]["completion_tokens"]
        .as_u64()
        .and_then(|t| u32::try_from(t).ok())
        .unwrap_or(0);
    Ok(Reading {
        id: crop.id.clone(),
        ms,
        text: clean_text(text),
        tokens,
    })
}

/// What `--probe` prints and the diagnostics record: llama-server's
/// version line and the model files the image holds, without loading one.
pub fn probe() -> String {
    let mut out = String::new();
    match quick(LLAMA_SERVER, &["--version"], Duration::from_secs(30)) {
        Ok((_, text)) => {
            let _ = writeln!(out, "{}", text.trim());
        }
        Err(e) => {
            let _ = writeln!(out, "{e}");
        }
    }
    let _ = writeln!(out, "models in {MODEL_DIR}:");
    let mut files: Vec<(String, u64)> = std::fs::read_dir(MODEL_DIR)
        .map(|d| {
            d.filter_map(|e| e.ok())
                .map(|e| {
                    (
                        e.file_name().to_string_lossy().into_owned(),
                        e.metadata().map(|m| m.len()).unwrap_or(0),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    if files.is_empty() {
        out.push_str("  none\n");
    }
    for (name, bytes) in files {
        let _ = writeln!(out, "  {name} {bytes}");
    }
    out
}

pub async fn run(work: &Path) -> anyhow::Result<()> {
    for name in outputs().into_iter().chain(["fraction_done"]) {
        let p = work.join(name);
        if p.exists() {
            std::fs::remove_file(p)?;
        }
    }
    let work = work.to_owned();
    tokio::task::spawn_blocking(move || run_blocking(&work)).await?
}

struct Done {
    outcome: Outcome,
    device: String,
    model: Option<String>,
    readings: Vec<Reading>,
    diag: BTreeMap<String, String>,
    load_ms: u64,
    engine_ms: u64,
    peak_rss_kib: Option<u64>,
    gpu_mem_peak_mib: Option<u64>,
}
impl Done {
    fn none(outcome: Outcome) -> Self {
        Done {
            outcome,
            device: "none".into(),
            model: None,
            readings: vec![],
            diag: BTreeMap::new(),
            load_ms: 0,
            engine_ms: 0,
            peak_rss_kib: None,
            gpu_mem_peak_mib: None,
        }
    }
}

/// The GPU's free memory in MB, for the model choice.
fn gpu_free_mb(gpu: Option<u32>) -> Option<u64> {
    let i = gpu?;
    let (ok, out) = quick(
        "nvidia-smi",
        &[
            "-i",
            &i.to_string(),
            "--query-gpu=memory.free",
            "--format=csv,noheader,nounits",
        ],
        Duration::from_secs(10),
    )
    .ok()?;
    ok.then(|| out.lines().next()?.trim().parse::<u64>().ok())
        .flatten()
}

/// Reads every crop within the deadline; sets the outcome.
fn read_crops(
    job: &Job,
    page: &image::DynamicImage,
    server: &Server,
    started: Instant,
    deadline: Duration,
    diag: &mut Diag,
    done: &mut Done,
) {
    let first = Instant::now();
    for crop in &job.crops {
        let remaining = deadline.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            done.outcome = Outcome::Timeout;
            diag.section(
                "deadline",
                &format!("after {} crops", done.readings.len()),
                256,
            );
            break;
        }
        let png = match crop_png(page, crop) {
            Ok(p) => p,
            Err(e) => {
                diag.section(&format!("crop {}", crop.id), &format!("{e:#}"), 512);
                break;
            }
        };
        match transcribe(server, crop, &png, job.max_tokens(crop), remaining) {
            Ok(r) => done.readings.push(r),
            Err(e) => {
                if e.root_cause().is::<TimedOut>() {
                    done.outcome = Outcome::Timeout;
                }
                diag.section(&format!("crop {}", crop.id), &format!("{e:#}"), 2048);
                break;
            }
        }
    }
    done.engine_ms = u64::try_from(first.elapsed().as_millis()).unwrap_or(u64::MAX);
    if done.readings.len() == job.crops.len() {
        done.outcome = Outcome::Ok;
    }
}

fn read_unit(job: &Job, page: &[u8], gpu: Option<u32>, threads: u32, diag: &mut Diag) -> Done {
    let mut done = Done::none(Outcome::Failed);
    let gpu_free = gpu_free_mb(gpu);
    let Some(model) = choose_model(&job.engine.models, gpu_free) else {
        diag.section("model", "the job names no model", 256);
        return done;
    };
    diag.section(
        "model",
        &format!(
            "{} (gpu free {gpu_free:?} MB, threads {threads})",
            model.name
        ),
        256,
    );
    let deadline = Duration::from_secs(
        if gpu.is_some() {
            job.gpu_timeout_secs
        } else {
            job.cpu_timeout_secs
        }
        .into(),
    );
    let started = Instant::now();
    let page = match image::load_from_memory(page) {
        Ok(p) => p,
        Err(e) => {
            diag.section("page", &format!("decoding page.jpg: {e}"), 512);
            return done;
        }
    };
    let server = match serve(model, gpu, threads, deadline) {
        Ok(s) => s,
        Err(e) => {
            diag.section("llama-server", &format!("{e:#}"), 4096);
            return done;
        }
    };
    done.model = Some(server.model.clone());
    done.device = server.device.clone();
    done.load_ms = server.load_ms;
    done.diag = server.diag.clone();
    read_crops(job, &page, &server, started, deadline, diag, &mut done);
    let stats = server.stats();
    let log_text = server.log_text();
    let gpu_before = server.gpu_mem_before_mib;
    drop(server);
    done.peak_rss_kib = Some(stats.peak_rss_kib);
    done.gpu_mem_peak_mib = stats
        .gpu_mem_peak_mib
        .map(|p| p.saturating_sub(gpu_before.unwrap_or(0)));
    let (_, _, device_lines) = llama_device(&log_text);
    diag.section(
        "llama-server",
        &format!(
            "outcome {:?}, device {}, load {} ms, read {} of {} crops in {} ms, cpu {:.0} s, peak rss {} KiB, gpu memory peak {:?} MiB, threads {threads}",
            done.outcome,
            done.device,
            done.load_ms,
            done.readings.len(),
            job.crops.len(),
            done.engine_ms,
            stats.cpu_secs(),
            stats.peak_rss_kib,
            stats.gpu_mem_peak_mib
        ),
        1024,
    );
    diag.section("llama-server device lines", &device_lines, 4096);
    diag.section("llama-server log, last lines", tail(&log_text, 3072), 3072);
    done
}

fn run_blocking(work: &Path) -> anyhow::Result<()> {
    let mut diag = Diag::new();
    let mut host = Host::default();
    boinc_info(work, &mut host, &mut diag);
    container_info(&mut host, &mut diag);
    let gpu = gpu_info(&mut host, &mut diag);
    diag.section("llama-server probe", &probe(), 4096);
    let gpu_version = host
        .plan_class
        .as_deref()
        .is_some_and(|p| p.contains("cuda"))
        || host.gpu_type.as_deref() == Some("nvidia");
    let threads = if gpu.is_some() {
        2
    } else {
        host.ncpus
            .map(|n| n.round().max(1.0) as u32)
            .or(host.cpu_threads)
            .unwrap_or(1)
    };
    let (job_id, engine, crops, unit_error, done) = match prepare(work) {
        Ok((job, page)) => {
            diag.section(
                "unit",
                &format!(
                    "job {} doc {} group {} language {}: {} crops, GPU version {gpu_version}, GPU visible {}",
                    job.id, job.doc_id, job.group, job.language, job.crops.len(), gpu.is_some()
                ),
                1024,
            );
            let done = if gpu_version && gpu.is_none() {
                Done::none(Outcome::NoGpu)
            } else {
                read_unit(&job, &page, gpu, threads, &mut diag)
            };
            (job.id, job.engine, job.crops.len() as u32, None, done)
        }
        Err(e) => {
            let msg = format!("{e:#}");
            diag.section("unit error", &msg, 1024);
            let job: Option<serde_json::Value> = std::fs::read(work.join(JOB_FILE))
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok());
            let id = job
                .as_ref()
                .and_then(|j| j.get("id")?.as_str())
                .filter(|s| usnm_ocr_contract::boinc::is_sha256(s))
                .unwrap_or(&"0".repeat(64))
                .to_owned();
            let crops = job
                .as_ref()
                .and_then(|j| j.get("crops")?.as_array())
                .map_or(0, |c| c.len() as u32);
            let engine = job
                .and_then(|j| serde_json::from_value::<Engine>(j.get("engine")?.clone()).ok())
                .unwrap_or_else(|| Engine {
                    image: format!("sha256:{}", "0".repeat(64)),
                    llama: format!("sha256:{}", "0".repeat(64)),
                    models: vec![],
                    weights: format!("sha256:{}", "0".repeat(64)),
                });
            (
                id,
                engine,
                crops,
                Some(tail(&msg, MAX_ERROR).to_owned()),
                Done::none(Outcome::UnitError),
            )
        }
    };
    let readings = Readings {
        v: VERSION,
        job_id: job_id.clone(),
        device: done.device.clone(),
        diag: done.diag,
        model: done.model.clone(),
        outcome: done.outcome,
        prompt_version: PROMPT_VERSION,
        readings: done.readings,
    };
    let readings_bytes = serde_json::to_vec_pretty(&readings)?;
    let diag = diag.finish();
    let result = ResultManifest {
        v: VERSION,
        job_id,
        completed_at: chrono::Utc::now().to_rfc3339(),
        crops,
        device: done.device,
        diag_bytes: diag.len() as u64,
        diag_sha256: sha256(&diag),
        engine,
        engine_ms: done.engine_ms,
        gpu_mem_peak_mib: done.gpu_mem_peak_mib,
        host,
        load_ms: done.load_ms,
        model: done.model,
        outcome: done.outcome,
        peak_rss_kib: done.peak_rss_kib,
        read: readings.readings.len() as u32,
        readings_bytes: readings_bytes.len() as u64,
        readings_sha256: sha256(&readings_bytes),
        unit_error,
    };
    atomic(&work.join(READINGS_FILE), &readings_bytes)?;
    atomic(&work.join(DIAG_FILE), &diag)?;
    atomic(
        &work.join(RESULT_FILE),
        &serde_json::to_vec_pretty(&result)?,
    )?;
    atomic(&work.join("fraction_done"), b"1.0\n")?;
    {
        use std::os::unix::fs::PermissionsExt as _;
        for name in outputs().into_iter().chain(["fraction_done"]) {
            std::fs::set_permissions(work.join(name), std::fs::Permissions::from_mode(0o644))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn crops_become_pngs_and_requests() {
        let jpeg = crate::quality::work::tests::synthetic_jpeg(400, 300);
        let page = image::load_from_memory(&jpeg).unwrap();
        let crop = Crop {
            rect: [10, 10, 210, 110],
            id: "c01".into(),
            loc_text: String::new(),
            stratum: "wc-low/top".into(),
        };
        let png = crop_png(&page, &crop).unwrap();
        let back = image::load_from_memory(&png).unwrap();
        assert_eq!(back.dimensions(), (200, 100));
        let body = request_body(&png, 64);
        assert_eq!(body["temperature"], 0);
        assert_eq!(body["max_tokens"], 64);
        assert!(body["messages"][0]["content"][0]["image_url"]["url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,"));
        let outside = Crop {
            rect: [10, 10, 500, 110],
            ..crop
        };
        assert!(crop_png(&page, &outside).is_err());
    }

    /// Without the image's environment the unit is broken; it still
    /// writes all three outputs and they pass the validator's check.
    #[tokio::test]
    async fn broken_unit_still_returns_a_result() {
        let tmp = tempfile::tempdir().unwrap();
        let unit = tmp.path().join("q");
        crate::quality::work::tests::unit(&unit);
        let dir = tmp.path().join("u");
        let j = super::super::stage::local_unit(
            &unit.join("page.jpg"),
            &unit.join("loc.xml"),
            &dir,
            super::super::stage::tests::engine(),
            1,
            4,
            600,
            4,
        )
        .unwrap();
        run(&dir).await.unwrap();
        let files: Vec<PathBuf> = outputs().iter().map(|n| dir.join(n)).collect();
        let l = crate::qwen::validate::load(&files).unwrap();
        assert_eq!(l.result.job_id, j.id);
        assert_eq!(l.result.outcome, Outcome::UnitError);
        assert_eq!(l.result.crops, 2);
        assert!(l.result.unit_error.unwrap().contains(ENV_LLAMA_IMAGE));
    }

    /// A one-shot HTTP server on the loopback that answers one request
    /// with `response` and returns what it read.
    fn fake_server(response: &'static str) -> (u16, std::thread::JoinHandle<Vec<u8>>) {
        use std::io::{Read as _, Write as _};
        let l = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = l.local_addr().unwrap().port();
        let h = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut got = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = s.read(&mut buf).unwrap();
                got.extend_from_slice(&buf[..n]);
                if let Some(i) = got.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&got[..i]).to_ascii_lowercase();
                    let len: usize = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .map_or(0, |v| v.trim().parse().unwrap());
                    if got.len() >= i + 4 + len {
                        break;
                    }
                }
            }
            s.write_all(response.as_bytes()).unwrap();
            got
        });
        (port, h)
    }

    #[test]
    fn loopback_client_reads_plain_and_chunked_bodies() {
        let (port, h) = fake_server(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 13\r\n\r\n{\"status\":1}\n",
        );
        let (status, body) = http(port, "GET", "/health", None, Duration::from_secs(5)).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"{\"status\":1}\n");
        let got = String::from_utf8(h.join().unwrap()).unwrap();
        assert!(got.starts_with("GET /health HTTP/1.1\r\n"));
        assert!(got.contains("Connection: close\r\n"));

        let (port, h) = fake_server(
            "HTTP/1.1 503 Service Unavailable\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6;ext=1\r\n world\r\n0\r\n\r\n",
        );
        let (status, body) = http(
            port,
            "POST",
            "/v1/chat/completions",
            Some(b"{\"a\":1}"),
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(status, 503);
        assert_eq!(body, b"hello world");
        let got = String::from_utf8(h.join().unwrap()).unwrap();
        assert!(got.contains("Content-Length: 7\r\n"));
        assert!(got.ends_with("\r\n\r\n{\"a\":1}"));
    }

    #[test]
    fn a_silent_server_is_a_timeout() {
        let l = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = l.local_addr().unwrap().port();
        let h = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            std::thread::sleep(Duration::from_millis(1500));
            drop(s);
        });
        let e = http(port, "GET", "/health", None, Duration::from_millis(100)).unwrap_err();
        assert!(e.root_cause().is::<TimedOut>(), "{e:#}");
        h.join().unwrap();
        let e = http(1, "GET", "/health", None, Duration::from_secs(1)).unwrap_err();
        assert!(!e.root_cause().is::<TimedOut>());
    }

    #[test]
    fn transcribe_parses_a_completion() {
        let (port, h) = fake_server(
            "HTTP/1.1 200 OK\r\nContent-Length: 142\r\n\r\n{\"choices\":[{\"message\":{\"role\":\"assistant\",\"content\":\" HELLO\\u0007 WORLD\\n \"}}],\"usage\":{\"completion_tokens\":4,\"prompt_tokens\":300},\"model\":\"x\"}",
        );
        let server = Server::detached(port);
        let crop = Crop {
            rect: [0, 0, 10, 10],
            id: "c01".into(),
            loc_text: String::new(),
            stratum: "s".into(),
        };
        let r = transcribe(&server, &crop, b"png", 32, Duration::from_secs(5)).unwrap();
        assert_eq!(r.text, "HELLO WORLD");
        assert_eq!(r.tokens, 4);
        assert_eq!(r.id, "c01");
        let got = String::from_utf8(h.join().unwrap()).unwrap();
        assert!(got.contains("\"max_tokens\":32"));
        assert!(got.contains(PROMPT));
    }

    #[test]
    fn text_is_cleaned_to_the_contract() {
        assert_eq!(clean_text("  a\tb\u{0}c\r\nd \n"), "a\tbc\nd");
        let long = "é".repeat(MAX_CROP_TEXT);
        let c = clean_text(&long);
        assert!(c.len() <= MAX_CROP_TEXT && c.chars().all(|ch| ch == 'é'));
        assert_eq!(dechunk(b"3\r\nabc\r\n0\r\n\r\n").unwrap(), b"abc");
        assert!(dechunk(b"5\r\nab").is_err());
    }
}
