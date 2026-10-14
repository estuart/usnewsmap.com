//! One page inside BOINC's job container. No Azure or LoC operations.
use crate::{atomic, read};
use anyhow::{ensure, Context};
use std::{
    path::Path,
    time::{Duration, Instant},
};
use usnm_ocr_contract::boinc::{self, Job, ResultManifest};
use usnm_ocr_worker::engine::{self, Engine, Runner, Tesseract, TesseractOptions};

pub async fn run(work: &Path) -> anyhow::Result<()> {
    let work = work.canonicalize()?;
    if work.join(crate::assess::JOB_FILE).exists() {
        return crate::assess::work::run(&work).await;
    }
    if work.join(crate::gold::job::JOB_FILE).exists() {
        return crate::gold::work::run(&work).await;
    }
    for name in ["result.json", "text.txt", "layout.xml", "fraction_done"] {
        let p = work.join(name);
        if p.exists() {
            std::fs::remove_file(p)?;
        }
    }
    let job: Job = serde_json::from_slice(&read(&work.join("request.json"), boinc::MAX_JSON)?)?;
    job.validate()?;
    let image = read(&work.join("page"), boinc::MAX_SCAN)?;
    ensure!(
        image.len() as u64 == job.scan.bytes && boinc::sha256(&image) == job.scan.sha256,
        "input scan checksum mismatch"
    );
    check_models(&job.engine)?;
    let engine = Tesseract::detect(TesseractOptions {
        runner: Runner::Host("tesseract".into()),
        psm: job.engine.psm,
        dpi: job.engine.dpi.into(),
        threads: 1,
        timeout: Duration::from_secs(job.engine.timeout_secs.into()),
    })
    .await?;
    ensure!(
        engine.version() == job.engine.version
            && engine::route(&job.message.language, engine.installed()).as_deref()
                == Some(&job.engine.profile),
        "engine version/route differs from job"
    );
    let started = Instant::now();
    let scratch = tempfile::tempdir()?;
    let output = engine
        .ocr(&work.join("page"), &job.message.language, scratch.path())
        .await?;
    ensure!(
        output.text.len() <= boinc::MAX_TEXT && output.alto.len() <= boinc::MAX_ALTO,
        "result over limit"
    );
    usnm_alto::check(&output.alto, &usnm_alto::Limits::default())?;
    let result = ResultManifest {
        v: boinc::VERSION,
        job_id: job.id,
        scan_sha256: job.scan.sha256,
        engine: job.engine,
        text_sha256: boinc::sha256(output.text.as_bytes()),
        alto_sha256: boinc::sha256(&output.alto),
        elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        completed_at: chrono::Utc::now().to_rfc3339(),
    };
    atomic(&work.join("text.txt"), output.text.as_bytes())?;
    atomic(&work.join("layout.xml"), &output.alto)?;
    atomic(
        &work.join("result.json"),
        &serde_json::to_vec_pretty(&result)?,
    )?;
    atomic(&work.join("fraction_done"), b"1.0\n")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for name in ["text.txt", "layout.xml", "result.json", "fraction_done"] {
            std::fs::set_permissions(work.join(name), std::fs::Permissions::from_mode(0o644))?;
        }
    }
    Ok(())
}

/// Checks the installed models of `engine.profile` against the lock and
/// the job's profile digest.
pub fn check_models(spec: &usnm_ocr_contract::boinc::Engine) -> anyhow::Result<()> {
    let models = std::env::var_os("TESSDATA_PREFIX")
        .context("TESSDATA_PREFIX must point to the locked image's models")?;
    for name in spec.profile.split('+') {
        let physical = format!("tessdata/{name}.traineddata");
        let expected = engine::MODELS_LOCK
            .lines()
            .filter(|s| !s.starts_with('#'))
            .find_map(|s| {
                let fields: Vec<_> = s.split_whitespace().collect();
                if fields.len() == 3 && fields[0] == physical {
                    Some(fields[2])
                } else {
                    None
                }
            })
            .context("model not in lock")?;
        ensure!(
            boinc::sha256(&read(
                &Path::new(&models).join(format!("{name}.traineddata")),
                64 << 20
            )?) == expected,
            "installed model checksum mismatch"
        );
    }
    ensure!(
        engine::models_digest(engine::MODELS_LOCK, &spec.profile)
            .context("model digest")?
            .to_string()
            == spec.models,
        "profile digest mismatch"
    );
    Ok(())
}
