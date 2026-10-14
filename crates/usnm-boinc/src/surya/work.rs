//! One Surya unit inside BOINC's job container. No network.
//!
//! The unit always ends with its five output files written, whatever
//! happened, so a host that can't run Surya still returns why:
//!
//! 1. Diagnostics: the BOINC client version, plan class and GPU it was given
//!    (selected fields of `init_data.xml`), the container runtime, the device
//!    nodes and CUDA variables the container sees, nvidia-smi's output or its
//!    error, and Python's torch/CUDA check.
//! 2. Unless the unit is broken (input checksum, an engine that differs from
//!    the job), each mode runs in its own process group with its own deadline:
//!    the GPU modes when nvidia-smi sees a GPU, the CPU modes otherwise, and
//!    none (`no_gpu`) when the GPU version was sent but no GPU is visible.
//!    While it runs, the group's RSS and CPU time are sampled every 0.5 s and
//!    the GPU's memory every 2 s.
//! 3. `result.json` (outcomes, times, memory, the device llama-server used)
//!    and `diag.txt` (bounded to [`MAX_DIAG`]).
use super::{outputs, DIAG_FILE, JOB_FILE, PAGE_FILE, RESULT_FILE};
use crate::{atomic, read};
use anyhow::{ensure, Context};
use std::{
    collections::BTreeMap,
    fmt::Write as _,
    os::unix::process::CommandExt as _,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use usnm_ocr_contract::{
    boinc::sha256,
    surya::{
        Engine, Host, InputMode, Job, Outcome, ResultManifest, Run, MAX_DIAG, MAX_ERROR, MAX_JSON,
        MAX_SCAN, MAX_TEXT, VERSION,
    },
};

/// The Python runner inside the image (`ocr/boinc/surya/run.py`).
const RUNNER: &str = "/opt/surya/run.py";
/// The engine the image was built with, from its environment.
const ENV_IMAGE_LLAMA: &str = "USNM_SURYA_LLAMA_IMAGE";
const ENV_VERSION: &str = "USNM_SURYA_VERSION";
const ENV_WEIGHTS: &str = "USNM_SURYA_WEIGHTS";
/// x86-64 Linux: 4 KiB pages, 100 clock ticks a second.
const PAGE_KIB: u64 = 4;
const TICKS: f64 = 100.0;

/// The diagnostics text, kept under [`MAX_DIAG`]: each section is cut to its
/// own cap and the whole to the limit, with a note where it was cut.
pub struct Diag(String);
impl Diag {
    fn new() -> Self {
        Self(String::new())
    }
    fn section(&mut self, title: &str, body: &str, cap: usize) {
        let _ = writeln!(self.0, "== {title}");
        let body = body.trim_end();
        if body.len() > cap {
            let mut end = cap;
            while !body.is_char_boundary(end) {
                end -= 1;
            }
            let _ = writeln!(
                self.0,
                "{}\n[cut at {cap} of {} bytes]",
                &body[..end],
                body.len()
            );
        } else if !body.is_empty() {
            let _ = writeln!(self.0, "{body}");
        }
    }
    fn finish(mut self) -> Vec<u8> {
        let limit = MAX_DIAG - 64;
        if self.0.len() > limit {
            let mut end = limit;
            while !self.0.is_char_boundary(end) {
                end -= 1;
            }
            self.0.truncate(end);
            self.0.push_str("\n[diagnostics cut at the size limit]\n");
        }
        self.0.into_bytes()
    }
}

/// The last `n` bytes of `s`, from a character boundary.
fn tail(s: &str, n: usize) -> &str {
    let mut i = s.len().saturating_sub(n);
    while !s.is_char_boundary(i) {
        i += 1;
    }
    &s[i..]
}

/// Runs a short command with a deadline; its stdout and stderr, or why it
/// didn't run.
fn quick(program: &str, args: &[&str], deadline: Duration) -> Result<(bool, String), String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{program}: {e}"))?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() > deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{program}: no answer in {}s", deadline.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(format!("{program}: {e}")),
        }
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("{program}: {e}"))?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    let err = String::from_utf8_lossy(&out.stderr);
    if !err.trim().is_empty() {
        text.push_str("\n[stderr]\n");
        text.push_str(&err);
    }
    Ok((out.status.success(), text))
}

/// The first `<tag>value</tag>` in `xml`, for a plain tag.
fn tag<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&format!("</{name}>"))? + start;
    Some(xml[start..end].trim())
}

/// The fields of BOINC's `init_data.xml` this unit reports. Only these:
/// the file also holds the account's authenticator and the host's name.
const INIT_FIELDS: [&str; 10] = [
    "major_version",
    "minor_version",
    "release",
    "app_version",
    "plan_class",
    "gpu_type",
    "gpu_device_num",
    "gpu_opencl_dev_index",
    "ncpus",
    "rsc_memory_bound",
];

fn boinc_info(work: &Path, host: &mut Host, diag: &mut Diag) {
    let Ok(xml) = std::fs::read_to_string(work.join("init_data.xml")) else {
        diag.section("boinc", "no init_data.xml in the slot", 256);
        return;
    };
    let mut body = String::new();
    for f in INIT_FIELDS {
        if let Some(v) = tag(&xml, f) {
            // Numbers and short names only.
            if v.len() <= 32
                && v.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            {
                let _ = writeln!(body, "{f}: {v}");
            }
        }
    }
    if let (Some(a), Some(b), Some(c)) = (
        tag(&xml, "major_version"),
        tag(&xml, "minor_version"),
        tag(&xml, "release"),
    ) {
        host.boinc_client = Some(format!("{a}.{b}.{c}")).filter(|s| s.len() <= 32);
    }
    host.plan_class = tag(&xml, "plan_class")
        .filter(|s| s.len() <= 32)
        .map(str::to_owned);
    host.gpu_type = tag(&xml, "gpu_type")
        .filter(|s| s.len() <= 32)
        .map(str::to_owned);
    host.gpu_device_num = tag(&xml, "gpu_device_num").and_then(|s| s.parse().ok());
    host.ncpus = tag(&xml, "ncpus").and_then(|s| s.parse().ok());
    diag.section("boinc (init_data.xml, selected fields)", &body, 2048);
}

fn container_info(host: &mut Host, diag: &mut Diag) {
    let mut body = String::new();
    // Podman writes the engine and whether it is rootless here; Docker
    // leaves an empty /.dockerenv.
    if let Ok(env) = std::fs::read_to_string("/run/.containerenv") {
        for l in env.lines() {
            if let Some(v) = l.strip_prefix("engine=") {
                let v = v.trim_matches('"');
                host.runtime = Some(v.chars().take(32).collect());
                let _ = writeln!(body, "engine: {v}");
            }
            if let Some(v) = l.strip_prefix("rootless=") {
                host.rootless = Some(v.trim() == "1");
                let _ = writeln!(body, "rootless: {v}");
            }
        }
    } else if Path::new("/.dockerenv").exists() {
        host.runtime = Some("docker".into());
        body.push_str("engine: docker (/.dockerenv)\n");
    } else {
        body.push_str("engine: not recognised\n");
    }
    host.cpu_threads = std::thread::available_parallelism()
        .ok()
        .and_then(|n| u32::try_from(n.get()).ok());
    let _ = writeln!(body, "available parallelism: {:?}", host.cpu_threads);
    for (name, path) in [
        ("cgroup memory.max", "/sys/fs/cgroup/memory.max"),
        ("cgroup memory.swap.max", "/sys/fs/cgroup/memory.swap.max"),
        ("cgroup cpu.max", "/sys/fs/cgroup/cpu.max"),
        ("cgroup pids.max", "/sys/fs/cgroup/pids.max"),
    ] {
        let v = std::fs::read_to_string(path).unwrap_or_else(|e| format!("({e})"));
        let _ = writeln!(body, "{name}: {}", v.trim());
    }
    if let Ok(m) = std::fs::read_to_string("/proc/meminfo") {
        for l in m
            .lines()
            .filter(|l| l.starts_with("MemTotal") || l.starts_with("MemAvailable"))
        {
            let _ = writeln!(body, "{l}");
        }
    }
    let _ = writeln!(
        body,
        "uid {} gid {}",
        rustix::process::getuid().as_raw(),
        rustix::process::getgid().as_raw()
    );
    diag.section("container", &body, 2048);
}

/// What the container sees of NVIDIA GPUs. Returns the index to run on:
/// BOINC's `gpu_device_num` when the container sees that GPU (the wrapper
/// passes all of them), else the first; none when nvidia-smi sees no GPU.
fn gpu_info(host: &mut Host, diag: &mut Diag) -> Option<u32> {
    let mut body = String::new();
    let mut devices: Vec<String> = std::fs::read_dir("/dev")
        .map(|d| {
            d.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with("nvidia") || n == "dri" || n == "kfd")
                .collect()
        })
        .unwrap_or_default();
    devices.sort();
    let _ = writeln!(
        body,
        "/dev: {}",
        if devices.is_empty() {
            "no nvidia, dri or kfd nodes".into()
        } else {
            devices.join(" ")
        }
    );
    for v in [
        "NVIDIA_VISIBLE_DEVICES",
        "NVIDIA_DRIVER_CAPABILITIES",
        "CUDA_VISIBLE_DEVICES",
        "NVIDIA_DISABLE_REQUIRE",
        "CUDA_VERSION",
    ] {
        let _ = writeln!(
            body,
            "{v}={}",
            std::env::var(v).unwrap_or_else(|_| "(unset)".into())
        );
    }
    let driver = std::fs::read_to_string("/proc/driver/nvidia/version")
        .map(|s| s.lines().next().unwrap_or("").to_owned())
        .unwrap_or_else(|e| format!("({e})"));
    let _ = writeln!(body, "/proc/driver/nvidia/version: {driver}");
    for lib in [
        "/usr/lib/x86_64-linux-gnu/libcuda.so.1",
        "/usr/lib64/libcuda.so.1",
        "/usr/local/nvidia/lib64/libcuda.so.1",
    ] {
        if Path::new(lib).exists() {
            let _ = writeln!(body, "libcuda: {lib}");
        }
    }
    diag.section("gpu devices", &body, 4096);
    let mut chosen = None;
    match quick(
        "nvidia-smi",
        &[
            "--query-gpu=name,driver_version,memory.total,compute_cap",
            "--format=csv,noheader,nounits",
        ],
        Duration::from_secs(30),
    ) {
        Ok((true, out)) => {
            let gpus: Vec<&str> = out.lines().filter(|l| l.contains(',')).collect();
            let index = host
                .gpu_device_num
                .and_then(|n| u32::try_from(n).ok())
                .filter(|n| (*n as usize) < gpus.len())
                .unwrap_or(0);
            let line = gpus.get(index as usize).copied().unwrap_or("");
            let f: Vec<&str> = line.split(',').map(str::trim).collect();
            if f.len() == 4 && !f[0].is_empty() {
                chosen = Some(index);
                host.gpu = Some(f[0].chars().take(64).collect());
                host.gpu_driver = Some(f[1].chars().take(32).collect());
                host.gpu_mem_total_mib = f[2].parse().ok();
                host.gpu_compute_cap = Some(f[3].chars().take(8).collect());
            }
            diag.section("nvidia-smi --query-gpu", &out, 2048);
            if let Ok((_, full)) = quick("nvidia-smi", &[], Duration::from_secs(30)) {
                if let Some(c) = full.split("CUDA Version:").nth(1) {
                    host.cuda_version = c
                        .split_whitespace()
                        .next()
                        .map(|s| s.chars().take(16).collect());
                }
                diag.section("nvidia-smi", &full, 6144);
            }
        }
        Ok((false, out)) => diag.section("nvidia-smi (failed)", &out, 4096),
        Err(e) => diag.section("nvidia-smi (did not run)", &e, 1024),
    }
    if let Some(i) = chosen {
        diag.section(
            "gpu used",
            &format!(
                "index {i} of the GPUs nvidia-smi lists (BOINC's gpu_device_num {:?})",
                host.gpu_device_num
            ),
            256,
        );
    }
    chosen
}

/// Python's view: torch and its CUDA check, surya's version and settings.
fn python_probe(host: &mut Host, diag: &mut Diag) {
    match quick("python3", &[RUNNER, "--probe"], Duration::from_secs(180)) {
        Ok((ok, out)) => {
            if let Some(line) = out.lines().find(|l| l.starts_with('{')) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                    host.torch_cuda = v.get("torch_cuda").and_then(|b| b.as_bool());
                }
            }
            diag.section(
                if ok {
                    "python probe"
                } else {
                    "python probe (failed)"
                },
                &out,
                4096,
            );
        }
        Err(e) => diag.section("python probe (did not run)", &e, 1024),
    }
}

/// Checks the unit before anything runs: the job, the page and the engine.
fn prepare(work: &Path) -> anyhow::Result<Job> {
    let job: Job = serde_json::from_slice(&read(&work.join(JOB_FILE), MAX_JSON)?)?;
    job.validate()?;
    let page = read(&work.join(PAGE_FILE), MAX_SCAN)?;
    ensure!(
        page.len() as u64 == job.input.bytes && sha256(&page) == job.input.sha256,
        "input page checksum mismatch"
    );
    for (var, want) in [
        (ENV_IMAGE_LLAMA, &job.engine.llama),
        (ENV_VERSION, &job.engine.surya),
        (ENV_WEIGHTS, &job.engine.weights),
    ] {
        let have = std::env::var(var).with_context(|| format!("the image has no {var}"))?;
        ensure!(
            &have == want,
            "the unit was staged for {var}={want}, this image has {have}"
        );
    }
    Ok(job)
}

/// The runner's processes. Surya starts llama-server in a session of its
/// own, outside the runner's process group, so when this process is pid 1
/// (the container's own pid namespace, which holds only this unit) they are
/// every other pid; otherwise, to be safe outside a container, only the
/// runner's process group.
fn others(pgid: i32) -> Vec<i32> {
    let me = i32::try_from(std::process::id()).unwrap_or(-1);
    let pids = std::fs::read_dir("/proc")
        .map(|d| {
            d.filter_map(|e| e.ok()?.file_name().to_str()?.parse::<i32>().ok())
                .filter(|p| *p != me)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if me == 1 {
        return pids;
    }
    pids.into_iter()
        .filter(|p| {
            std::fs::read_to_string(format!("/proc/{p}/stat"))
                .ok()
                .and_then(|s| {
                    s.rsplit_once(')')?
                        .1
                        .split_whitespace()
                        .nth(2)?
                        .parse()
                        .ok()
                })
                == Some(pgid)
        })
        .collect()
}

/// As pid 1, collects orphans that exited (llama-server once its parent is
/// gone), so they leave /proc.
fn reap() {
    if std::process::id() == 1 {
        while let Ok(Some(_)) = rustix::process::wait(rustix::process::WaitOptions::NOHANG) {}
    }
}

/// Kills the runner's process group and, in a container, every other
/// process (llama-server among them).
fn kill_all(pgid: i32) {
    if let Some(p) = rustix::process::Pid::from_raw(pgid) {
        let _ = rustix::process::kill_process_group(p, rustix::process::Signal::KILL);
    }
    for pid in others(pgid) {
        if let Some(p) = rustix::process::Pid::from_raw(pid) {
            let _ = rustix::process::kill_process(p, rustix::process::Signal::KILL);
        }
    }
}

/// The summed RSS (KiB) of the runner's processes ([`others`]), and their
/// CPU ticks per process, from /proc.
fn runner_stats(pgid: i32, cpu: &mut BTreeMap<i32, u64>) -> u64 {
    let mut rss = 0;
    for pid in others(pgid) {
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        // After the command name: state ppid pgrp ... utime(11) stime(12) ... rss(21)
        let Some((_, rest)) = stat.rsplit_once(')') else {
            continue;
        };
        let f: Vec<&str> = rest.split_whitespace().collect();
        if f.len() < 22 {
            continue;
        }
        let ticks = f[11].parse::<u64>().unwrap_or(0) + f[12].parse::<u64>().unwrap_or(0);
        let seen = cpu.entry(pid).or_insert(0);
        *seen = (*seen).max(ticks);
        rss += f[21].parse::<u64>().unwrap_or(0) * PAGE_KIB;
    }
    rss
}

fn gpu_mem_used(index: u32) -> Option<u64> {
    let (ok, out) = quick(
        "nvidia-smi",
        &[
            "-i",
            &index.to_string(),
            "--query-gpu=memory.used",
            "--format=csv,noheader,nounits",
        ],
        Duration::from_secs(10),
    )
    .ok()?;
    ok.then(|| out.lines().next()?.trim().parse().ok())
        .flatten()
}

/// llama-server's log lines that say which device it used, and the
/// "offloaded N/M layers to GPU" count.
fn llama_device(log: &str) -> (String, Option<String>, String) {
    let mut lines = String::new();
    let mut offload = None;
    let mut cuda = false;
    for l in log.lines() {
        let low = l.to_ascii_lowercase();
        if low.contains("offloaded") && low.contains("layers to gpu") {
            offload = l
                .split("offloaded")
                .nth(1)
                .map(|s| s.trim().chars().take(64).collect::<String>());
            if let Some(o) = &offload {
                cuda = !o.starts_with("0/");
            }
        }
        let shown = [
            "cuda",
            "offload",
            "ggml_backend",
            "error",
            "failed",
            "device",
            "gpu",
            "vram",
        ]
        .iter()
        .any(|k| low.contains(k));
        if shown && lines.len() < 4096 {
            lines.push_str(l);
            lines.push('\n');
        }
    }
    (if cuda { "cuda" } else { "cpu" }.into(), offload, lines)
}

struct ModeRun {
    run: Run,
    text: Vec<u8>,
}

#[allow(clippy::too_many_arguments)]
fn run_mode(
    work: &Path,
    mode: InputMode,
    deadline: Duration,
    threads: u32,
    gpu: Option<u32>,
    home: &Path,
    diag: &mut Diag,
) -> anyhow::Result<ModeRun> {
    let scratch = tempfile::tempdir()?;
    let out = scratch.path().join("text.txt");
    let stats = scratch.path().join("stats.json");
    let stderr_path = scratch.path().join("stderr.txt");
    let log = home.join(".cache/datalab/surya/llamacpp_server.log");
    let _ = std::fs::remove_file(&log);
    let gpu_before = gpu.and_then(gpu_mem_used);
    let mut cmd = Command::new("python3");
    if let Some(i) = gpu {
        // Only BOINC's GPU, as nvidia-smi numbers them (llama.cpp's CUDA
        // order is the same with CUDA_DEVICE_ORDER=PCI_BUS_ID).
        cmd.env("CUDA_DEVICE_ORDER", "PCI_BUS_ID")
            .env("CUDA_VISIBLE_DEVICES", i.to_string());
    }
    let mut child = cmd
        .arg(RUNNER)
        .args(["--image", &work.join(PAGE_FILE).to_string_lossy()])
        .args(["--mode", mode.name()])
        .args(["--out", &out.to_string_lossy()])
        .args(["--stats", &stats.to_string_lossy()])
        .env("HOME", home)
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("LLAMA_CPP_EXTRA_ARGS", format!("--threads {threads}"))
        .env("OMP_NUM_THREADS", threads.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&stderr_path)?)
        .process_group(0)
        .spawn()
        .context("starting python3")?;
    let pgid = i32::try_from(child.id()).context("pid")?;
    let kill_group = || kill_all(pgid);
    let started = Instant::now();
    let mut cpu = BTreeMap::new();
    let mut peak_rss = 0u64;
    let mut gpu_peak = None::<u64>;
    let mut last_gpu = Instant::now() - Duration::from_secs(10);
    let mut timed_out = false;
    let waited: std::io::Result<std::process::ExitStatus> = loop {
        peak_rss = peak_rss.max(runner_stats(pgid, &mut cpu));
        if let Some(i) = gpu.filter(|_| last_gpu.elapsed() >= Duration::from_secs(2)) {
            last_gpu = Instant::now();
            if let Some(u) = gpu_mem_used(i) {
                gpu_peak = Some(gpu_peak.map_or(u, |p| p.max(u)));
            }
        }
        match child.try_wait() {
            Ok(Some(s)) => break Ok(s),
            Ok(None) => {}
            Err(e) => break Err(e),
        }
        if started.elapsed() >= deadline {
            timed_out = true;
            kill_group();
            break child.wait();
        }
        std::thread::sleep(Duration::from_millis(500));
    };
    // Whatever is left (llama-server, when Python died first), also when
    // waiting failed; the next mode starts only once it has gone.
    kill_group();
    let gone = Instant::now();
    while !others(pgid).is_empty() && gone.elapsed() < Duration::from_secs(15) {
        reap();
        std::thread::sleep(Duration::from_millis(100));
    }
    let status = match waited {
        Ok(s) => s,
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e).context("waiting for python3");
        }
    };
    let engine_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let code = status.code();
    let signal = std::os::unix::process::ExitStatusExt::signal(&status);
    let outcome = Outcome::of(code, timed_out);
    let text = if outcome == Outcome::Ok {
        let t = read(&out, MAX_TEXT)
            .context("surya exited 0 but its text is missing or over the limit");
        match t {
            Ok(t) if std::str::from_utf8(&t).is_ok() => Some(t),
            _ => None,
        }
    } else {
        None
    };
    // Exit 0 without usable text is a failed run, not a broken unit.
    let (outcome, code, text) = match (outcome, text) {
        (Outcome::Ok, Some(t)) => (Outcome::Ok, code, t),
        (Outcome::Ok, None) => (Outcome::Failed, Some(-1), Vec::new()),
        (o, _) => (o, if timed_out { None } else { code }, Vec::new()),
    };
    let tiles = std::fs::read(&stats)
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v.get("tiles")?.as_u64())
        .and_then(|n| u32::try_from(n).ok());
    let log_text = std::fs::read(&log)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let (device, gpu_layers, device_lines) = llama_device(&log_text);
    let stderr = std::fs::read(&stderr_path)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let run = Run {
        code,
        cpu_secs: Some(cpu.values().sum::<u64>() as f64 / TICKS),
        device: if log_text.is_empty() {
            "none".into()
        } else {
            device
        },
        engine_ms,
        gpu_mem_peak_mib: gpu_peak.map(|p| p.saturating_sub(gpu_before.unwrap_or(0))),
        gpu_layers,
        mode,
        outcome,
        peak_rss_kib: Some(peak_rss),
        signal: if timed_out { None } else { signal },
        text_bytes: text.len() as u64,
        text_sha256: sha256(&text),
        tiles,
    };
    diag.section(
        &format!("mode {}", mode.name()),
        &format!(
            "outcome {:?}, code {:?}, signal {:?}, {} ms, cpu {:.0} s, peak rss {} KiB, gpu memory before {:?} MiB, peak {:?} MiB, threads {threads}",
            run.outcome, run.code, run.signal, run.engine_ms, run.cpu_secs.unwrap_or(0.0), peak_rss, gpu_before, gpu_peak
        ),
        1024,
    );
    diag.section(
        &format!("mode {}: llama-server device lines", mode.name()),
        &device_lines,
        4096,
    );
    diag.section(
        &format!("mode {}: llama-server log, last lines", mode.name()),
        tail(&log_text, 1536),
        1536,
    );
    diag.section(
        &format!("mode {}: surya stderr, last lines", mode.name()),
        tail(&stderr, 3072),
        3072,
    );
    Ok(ModeRun { run, text })
}

/// A mode that read nothing: no GPU to run on, or a runner that couldn't start.
fn not_run(mode: InputMode, outcome: Outcome) -> Run {
    Run {
        code: None,
        cpu_secs: None,
        device: "none".into(),
        engine_ms: 0,
        gpu_mem_peak_mib: None,
        gpu_layers: None,
        mode,
        outcome,
        peak_rss_kib: None,
        signal: None,
        text_bytes: 0,
        text_sha256: sha256(b""),
        tiles: None,
    }
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

fn run_blocking(work: &Path) -> anyhow::Result<()> {
    let mut diag = Diag::new();
    let mut host = Host::default();
    boinc_info(work, &mut host, &mut diag);
    container_info(&mut host, &mut diag);
    let gpu = gpu_info(&mut host, &mut diag);
    let visible = gpu.is_some();
    python_probe(&mut host, &mut diag);
    let gpu_version = host
        .plan_class
        .as_deref()
        .is_some_and(|p| p.contains("cuda"))
        || host.gpu_type.as_deref() == Some("nvidia");
    let mut texts = BTreeMap::new();
    let mut runs = Vec::new();
    let (job_id, engine, unit_error) = match prepare(work) {
        Ok(job) => {
            let (modes, deadline, threads) = if visible {
                (&job.gpu_modes, job.gpu_timeout_secs, 2)
            } else {
                let n = host
                    .ncpus
                    .map(|n| n.round().max(1.0) as u32)
                    .or(host.cpu_threads)
                    .unwrap_or(1);
                (&job.cpu_modes, job.cpu_timeout_secs, n)
            };
            diag.section(
                "unit",
                &format!(
                    "job {} doc {} group {} language {}\nGPU version {gpu_version}, GPU visible {visible}: modes {:?}, deadline {deadline} s each",
                    job.id, job.doc_id, job.group, job.language, modes.iter().map(|m| m.name()).collect::<Vec<_>>()
                ),
                1024,
            );
            // HOME for Surya and llama-server's log; /tmp when no fresh directory can be made.
            let home_dir = tempfile::tempdir().ok();
            let home = home_dir
                .as_ref()
                .map_or_else(|| std::path::PathBuf::from("/tmp"), |d| d.path().to_owned());
            for &mode in modes {
                if gpu_version && !visible {
                    runs.push(not_run(mode, Outcome::NoGpu));
                    continue;
                }
                match run_mode(
                    work,
                    mode,
                    Duration::from_secs(deadline.into()),
                    threads,
                    gpu,
                    &home,
                    &mut diag,
                ) {
                    Ok(r) => {
                        texts.insert(mode, r.text);
                        runs.push(r.run);
                    }
                    // The runner could not be started or watched: a failed
                    // run, with the reason in the diagnostics.
                    Err(e) => {
                        diag.section(
                            &format!("mode {}: runner error", mode.name()),
                            &format!("{e:#}"),
                            1024,
                        );
                        runs.push(not_run(mode, Outcome::Failed));
                    }
                }
            }
            (job.id, job.engine, None)
        }
        Err(e) => {
            let msg = format!("{e:#}");
            diag.section("unit error", &msg, 1024);
            // What the job says, as far as it can be read, so the result can
            // still be matched to its unit.
            let job: Option<serde_json::Value> = std::fs::read(work.join(JOB_FILE))
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok());
            let id = job
                .as_ref()
                .and_then(|j| j.get("id")?.as_str())
                .filter(|s| usnm_ocr_contract::boinc::is_sha256(s))
                .unwrap_or(&"0".repeat(64))
                .to_owned();
            let engine = job
                .and_then(|j| serde_json::from_value::<Engine>(j.get("engine")?.clone()).ok())
                .unwrap_or_else(|| Engine {
                    image: format!("sha256:{}", "0".repeat(64)),
                    llama: format!("sha256:{}", "0".repeat(64)),
                    surya: "unknown".into(),
                    weights: format!("sha256:{}", "0".repeat(64)),
                });
            (id, engine, Some(tail(&msg, MAX_ERROR).to_owned()))
        }
    };
    let diag = diag.finish();
    let result = ResultManifest {
        v: VERSION,
        job_id,
        engine,
        host,
        unit_error,
        runs,
        diag_bytes: diag.len() as u64,
        diag_sha256: sha256(&diag),
        completed_at: chrono::Utc::now().to_rfc3339(),
    };
    for m in usnm_ocr_contract::surya::MODES {
        atomic(&work.join(m.text()), texts.get(&m).map_or(&[][..], |t| t))?;
    }
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
pub(crate) mod tests {
    use super::*;
    use std::path::PathBuf;
    use usnm_ocr_contract::surya::{Input, MODES};

    pub(crate) fn job() -> Job {
        let mut j = Job {
            v: VERSION,
            id: String::new(),
            doc_id: "sn85055101_1917-04-03_ed-1_seq-3".into(),
            group: "serbian".into(),
            language: "srp".into(),
            input: Input {
                bytes: 1,
                sha256: sha256(b"x"),
                url: "https://tile.loc.gov/x/full/full/0/default.jpg".into(),
            },
            engine: Engine {
                image: format!("sha256:{}", "a".repeat(64)),
                llama: format!("sha256:{}", "b".repeat(64)),
                surya: "0.22.1".into(),
                weights: format!("sha256:{}", "c".repeat(64)),
            },
            gpu_modes: MODES.to_vec(),
            cpu_modes: vec![InputMode::Tiled],
            gpu_timeout_secs: 1800,
            cpu_timeout_secs: 5400,
        };
        j.id = j.identity().unwrap();
        j
    }

    #[test]
    fn init_data_fields_only() {
        let xml = "<app_init_data>\n<major_version>8</major_version>\n<minor_version>2</minor_version>\n<release>13</release>\n<plan_class>docker_nvidia_cuda</plan_class>\n<authenticator>secret</authenticator>\n<gpu_type>nvidia</gpu_type>\n<gpu_device_num>0</gpu_device_num>\n<ncpus>1.000000</ncpus>\n<host_info><domain_name>myhost</domain_name></host_info></app_init_data>";
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("init_data.xml"), xml).unwrap();
        let mut host = Host::default();
        let mut diag = Diag::new();
        boinc_info(tmp.path(), &mut host, &mut diag);
        let d = String::from_utf8(diag.finish()).unwrap();
        assert!(!d.contains("secret") && !d.contains("myhost"));
        assert_eq!(host.boinc_client.as_deref(), Some("8.2.13"));
        assert_eq!(host.plan_class.as_deref(), Some("docker_nvidia_cuda"));
        assert_eq!(host.gpu_device_num, Some(0));
        assert_eq!(host.ncpus, Some(1.0));
    }

    #[test]
    fn llama_log_device() {
        let log = "ggml_cuda_init: found 1 CUDA devices:\n  Device 0: NVIDIA GeForce RTX 3060, compute capability 8.6\nload_tensors: offloaded 29/29 layers to GPU\n";
        let (d, o, lines) = llama_device(log);
        assert_eq!(d, "cuda");
        assert_eq!(o.as_deref(), Some("29/29 layers to GPU"));
        assert!(lines.contains("RTX 3060"));
        assert_eq!(
            llama_device("load_tensors: offloaded 0/29 layers to GPU\n").0,
            "cpu"
        );
        assert_eq!(llama_device("main: server is listening\n").0, "cpu");
    }

    #[test]
    fn diag_is_bounded() {
        let mut d = Diag::new();
        for i in 0..100 {
            d.section(&format!("s{i}"), &"é".repeat(5000), 4000);
        }
        let b = d.finish();
        assert!(b.len() <= MAX_DIAG);
        assert!(std::str::from_utf8(&b).is_ok());
    }

    /// A unit whose page doesn't match still writes all five outputs, with
    /// the reason as a unit error, and they pass the validator's check.
    #[tokio::test]
    async fn broken_unit_still_returns_a_result() {
        let tmp = tempfile::tempdir().unwrap();
        let j = job();
        std::fs::write(tmp.path().join(JOB_FILE), serde_json::to_vec(&j).unwrap()).unwrap();
        std::fs::write(tmp.path().join(PAGE_FILE), b"not the page").unwrap();
        run(tmp.path()).await.unwrap();
        let r: ResultManifest =
            serde_json::from_slice(&std::fs::read(tmp.path().join(RESULT_FILE)).unwrap()).unwrap();
        assert_eq!(r.job_id, j.id);
        assert!(r.unit_error.unwrap().contains("checksum"));
        let files: Vec<PathBuf> = outputs().iter().map(|n| tmp.path().join(n)).collect();
        crate::surya::validate::check(&files, None).unwrap();
    }
}
