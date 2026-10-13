//! One assessment unit inside BOINC's job container. No network.
//!
//! Each format is read once, with the arguments the production worker
//! uses (`<image> page -l <profile> --psm 3 --dpi 300 txt alto`, one
//! thread), by the Tesseract the worker image was built on. The engine's
//! exit is recorded, whatever it is: only a broken unit (bad checksum, a
//! model or engine that differs from the job) is an error.
use super::{JOB_FILE, RESULT_FILE};
use crate::{atomic, read};
use anyhow::{ensure, Context};
use std::{
    io::Read as _,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use usnm_ocr_contract::{
    assess::{Format, Job, Outcome, ResultManifest, Run, MAX_JSON, MAX_SCAN, MAX_TEXT, VERSION},
    boinc::sha256,
};
use usnm_ocr_worker::engine::{self, Runner, Tesseract, TesseractOptions};

const OUTPUTS: [&str; 4] = [
    "text-jp2.txt",
    "text-jpeg.txt",
    RESULT_FILE,
    "fraction_done",
];

pub async fn run(work: &Path) -> anyhow::Result<()> {
    for name in OUTPUTS {
        let p = work.join(name);
        if p.exists() {
            std::fs::remove_file(p)?;
        }
    }
    let job: Job = serde_json::from_slice(&read(&work.join(JOB_FILE), MAX_JSON)?)?;
    job.validate()?;
    for (f, i) in &job.inputs {
        let b = read(&work.join(f.input()), MAX_SCAN)?;
        ensure!(
            b.len() as u64 == i.bytes && sha256(&b) == i.sha256,
            "input {f:?} checksum mismatch"
        );
    }
    let base = std::env::var("USNM_TESSERACT_IMAGE")
        .context("the worker image does not name its Tesseract image")?;
    ensure!(
        base == job.engine_base,
        "the unit was staged for engine {}, this image is built on {base}",
        job.engine_base
    );
    crate::worker::check_models(&job.engine)?;
    let tesseract = Tesseract::detect(TesseractOptions {
        runner: Runner::Host("tesseract".into()),
        psm: job.engine.psm,
        dpi: job.engine.dpi.into(),
        threads: 1,
        timeout: Duration::from_secs(job.engine.timeout_secs.into()),
    })
    .await?;
    ensure!(
        tesseract.version() == job.engine.version
            && engine::route(&job.language, tesseract.installed()).as_deref()
                == Some(&job.engine.profile),
        "engine version or route differs from the job"
    );
    let mut runs = Vec::new();
    for f in [Format::Jp2, Format::Jpeg] {
        if !job.inputs.contains_key(&f) {
            continue;
        }
        let scratch = tempfile::tempdir()?;
        let (run, text) = ocr(
            Path::new("tesseract"),
            &work.join(f.input()),
            f,
            &job,
            scratch.path(),
        )?;
        atomic(&work.join(f.text()), &text)?;
        runs.push(run);
    }
    // A crash unit returns an empty JPEG text so every result has the same files.
    if !job.inputs.contains_key(&Format::Jpeg) {
        atomic(&work.join(Format::Jpeg.text()), b"")?;
    }
    let result = ResultManifest {
        v: VERSION,
        job_id: job.id.clone(),
        mode: job.mode,
        engine: job.engine.clone(),
        runs,
        completed_at: chrono::Utc::now().to_rfc3339(),
    };
    atomic(
        &work.join(RESULT_FILE),
        &serde_json::to_vec_pretty(&result)?,
    )?;
    atomic(&work.join("fraction_done"), b"1.0\n")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for name in OUTPUTS {
            std::fs::set_permissions(work.join(name), std::fs::Permissions::from_mode(0o644))?;
        }
    }
    Ok(())
}

/// The engine process's VmHWM, if /proc has it.
fn vm_hwm_kib(pid: u32) -> Option<u64> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    s.lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.split_whitespace().next()?.parse().ok())
}

/// Runs `bin` (Tesseract) on `image` with the production arguments and
/// returns the run's record and its text (empty unless it exited 0).
pub fn ocr(
    bin: &Path,
    image: &Path,
    format: Format,
    job: &Job,
    scratch: &Path,
) -> anyhow::Result<(Run, Vec<u8>)> {
    let deadline = Duration::from_secs(job.engine.timeout_secs.into());
    let mut child = Command::new(bin)
        .arg(image)
        .arg(scratch.join("page"))
        .args(["-l", &job.engine.profile])
        .args(["--psm", &job.engine.psm.to_string()])
        .args(["--dpi", &job.engine.dpi.to_string()])
        .args(["txt", "alto"])
        .env("OMP_THREAD_LIMIT", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting tesseract")?;
    let mut stderr = child.stderr.take().context("no stderr")?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.by_ref().take(1 << 20).read_to_end(&mut buf);
        buf
    });
    let started = Instant::now();
    let mut peak = None;
    let mut timed_out = false;
    let status = loop {
        if let Some(hwm) = vm_hwm_kib(child.id()) {
            peak = Some(peak.map_or(hwm, |p: u64| p.max(hwm)));
        }
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if started.elapsed() >= deadline {
            timed_out = true;
            let _ = child.kill();
            break child.wait()?;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let engine_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let err = reader.join().unwrap_or_default();
    #[cfg(unix)]
    let signal = std::os::unix::process::ExitStatusExt::signal(&status);
    #[cfg(not(unix))]
    let signal = None;
    let code = status.code();
    let outcome = Outcome::of(code, signal, timed_out);
    let text = if outcome == Outcome::Ok {
        // Exit 0 without a readable text is a broken engine, not a result.
        let t = read(&scratch.join("page.txt"), MAX_TEXT)
            .context("tesseract exited 0 but its text is missing or over the limit")?;
        std::str::from_utf8(&t).context("tesseract's text is not UTF-8")?;
        t
    } else {
        Vec::new()
    };
    let stderr_tail = (outcome != Outcome::Ok).then(|| {
        let s = String::from_utf8_lossy(&err);
        let start = s.len().saturating_sub(400);
        let mut i = start;
        while !s.is_char_boundary(i) {
            i += 1;
        }
        s[i..].to_owned()
    });
    Ok((
        Run {
            format,
            outcome,
            code: if timed_out { None } else { code },
            signal: if timed_out { None } else { signal },
            engine_ms,
            peak_rss_kib: peak,
            text_sha256: sha256(&text),
            text_bytes: text.len() as u64,
            stderr_tail,
        },
        text,
    ))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use usnm_ocr_contract::assess::{Input, Mode, Stratum};

    pub(crate) fn job(mode: Mode) -> Job {
        let mut inputs = BTreeMap::from([(
            Format::Jp2,
            Input {
                url: "https://tile.loc.gov/x.jp2".into(),
                sha256: "c".repeat(64),
                bytes: 10,
            },
        )]);
        if mode == Mode::Ocr {
            inputs.insert(
                Format::Jpeg,
                Input {
                    url: "https://tile.loc.gov/x.jpg".into(),
                    sha256: "d".repeat(64),
                    bytes: 10,
                },
            );
        }
        let mut j = Job {
            v: VERSION,
            id: String::new(),
            mode,
            doc_id: "sn00000001_1900-01-01_ed-1_seq-1".into(),
            batch: "test_batch".into(),
            batch_version: 1,
            language: "eng".into(),
            stratum: Stratum {
                crash_weight: 1.0,
                damage: "low".into(),
                decade: 1900,
                lang: "eng".into(),
                pages: 1,
                prefix: "test".into(),
                weight: 1.0,
            },
            inputs,
            engine: usnm_ocr_contract::boinc::Engine {
                image: format!("sha256:{}", "a".repeat(64)),
                version: "5.5.0".into(),
                profile: "eng".into(),
                models: "sha256:0123456789ab".into(),
                psm: 3,
                dpi: 300,
                timeout_secs: 2,
            },
            engine_base: format!("sha256:{}", "f".repeat(64)),
        };
        j.id = j.identity().unwrap();
        j
    }

    /// A stand-in engine script: the arguments decide how it ends.
    #[cfg(unix)]
    fn fake(dir: &Path, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join("tesseract");
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    #[cfg(unix)]
    #[test]
    fn records_ok_crash_and_timeout() {
        let tmp = tempfile::tempdir().unwrap();
        let j = job(Mode::Ocr);
        let img = tmp.path().join("page.jp2");
        std::fs::write(&img, b"x").unwrap();

        let ok = fake(tmp.path(), r#"printf 'hello world\n' > "$2.txt""#);
        let (r, t) = ocr(&ok, &img, Format::Jp2, &j, tmp.path()).unwrap();
        assert_eq!(r.outcome, Outcome::Ok);
        assert_eq!(t, b"hello world\n");

        let crash = fake(
            tmp.path(),
            "echo 'Floating point exception' >&2; kill -FPE $$",
        );
        let (r, t) = ocr(&crash, &img, Format::Jp2, &j, tmp.path()).unwrap();
        assert_eq!(r.outcome, Outcome::Crash136);
        assert!(t.is_empty());
        assert!(r.stderr_tail.unwrap().contains("Floating"));

        let rc136 = fake(tmp.path(), "exit 136");
        let (r, _) = ocr(&rc136, &img, Format::Jp2, &j, tmp.path()).unwrap();
        assert_eq!(r.outcome, Outcome::Crash136);

        let slow = fake(tmp.path(), "exec sleep 30");
        let (r, _) = ocr(&slow, &img, Format::Jp2, &j, tmp.path()).unwrap();
        assert_eq!(r.outcome, Outcome::Timeout);
        assert!(r.engine_ms < 10_000);
    }
}
