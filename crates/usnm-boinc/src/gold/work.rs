//! One gold unit inside BOINC's job container. No network: the BOINC client has already
//! downloaded the bundle tar and the original files into the slot, and checked their md5s.
//!
//! The unit checks every input's size and sha256 against the job, the bundle's manifest digest
//! and version, and the engine build, then runs [`super::run`] with the slot as the cache and
//! writes `gold-result.json`: counts only, no page text and no transcription.
use super::job::{Job, BUNDLE_FILE, JOB_FILE, MAX_JSON, MAX_RESULT, RESULT_FILE};
use super::{BoincInfo, Report, RunOptions};
use crate::{atomic, read};
use anyhow::{ensure, Context};
use std::{collections::BTreeSet, path::Path};
use usnm_ocr_contract::boinc;
use usnm_ocr_worker::engine;

const OUTPUTS: [&str; 2] = [RESULT_FILE, "fraction_done"];

/// The host id the client wrote into the slot's `init_data.xml`, if it is there.
pub fn host_id(work: &Path) -> Option<u64> {
    let s = std::fs::read_to_string(work.join("init_data.xml")).ok()?;
    let start = s.find("<hostid>")? + "<hostid>".len();
    let end = start + s[start..].find("</hostid>")?;
    s[start..end].trim().parse().ok()
}

fn sha256_file(p: &Path, max: u64) -> anyhow::Result<(u64, String)> {
    use std::io::Read as _;
    let meta = std::fs::symlink_metadata(p).with_context(|| format!("{}", p.display()))?;
    ensure!(
        meta.file_type().is_file(),
        "{} is not a regular file",
        p.display()
    );
    ensure!(meta.len() <= max, "{} is over its size limit", p.display());
    let mut f = std::fs::File::open(p)?;
    let mut h = <sha2::Sha256 as sha2::Digest>::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        sha2::Digest::update(&mut h, &buf[..n]);
    }
    let d = sha2::Digest::finalize(h);
    Ok((meta.len(), d.iter().map(|b| format!("{b:02x}")).collect()))
}

/// Checks a file of the unit against the job.
pub fn check_file(dir: &Path, f: &crate::gold::job::File, max: u64) -> anyhow::Result<()> {
    let (bytes, sha) = sha256_file(&dir.join(&f.name), max)?;
    ensure!(
        bytes == f.bytes && sha == f.sha256,
        "input {} differs from the job",
        f.name
    );
    Ok(())
}

pub async fn run(work: &Path) -> anyhow::Result<()> {
    for name in OUTPUTS {
        let p = work.join(name);
        if p.exists() {
            std::fs::remove_file(p)?;
        }
    }
    let job: Job = serde_json::from_slice(&read(&work.join(JOB_FILE), MAX_JSON)?)?;
    job.validate()?;
    check_file(work, &job.bundle, crate::gold::job::MAX_BUNDLE)?;
    for r in &job.remotes {
        check_file(work, r, crate::gold::job::MAX_REMOTE)?;
    }
    let base = std::env::var("USNM_TESSERACT_IMAGE")
        .context("the worker image does not name its Tesseract image")?;
    ensure!(
        base == job.engine.base,
        "the unit was made for engine {}, this image is built on {base}",
        job.engine.base
    );
    // The container's root filesystem is read-only; /tmp is its scratch tmpfs.
    let scratch = tempfile::tempdir()?;
    let report = super::run(&RunOptions {
        bundle: work.join(BUNDLE_FILE),
        cache: work.to_path_buf(),
        work: scratch.path().to_path_buf(),
        tesseract: "tesseract".into(),
        psm: job.engine.psm,
        dpi: job.engine.dpi.into(),
        timeout_secs: job.engine.timeout_secs,
        limit: None,
    })
    .await?;
    let report = finish(&job, report, host_id(work))?;
    // Every locked model, not only the profiles that scored: a crop the engine failed on still
    // counts in the report.
    let profiles: BTreeSet<&str> = engine::MODELS_LOCK
        .lines()
        .filter(|s| !s.starts_with('#'))
        .filter_map(|s| s.split_whitespace().next())
        .filter_map(|s| s.strip_prefix("tessdata/")?.strip_suffix(".traineddata"))
        .chain(report.crops.iter().map(|c| c.profile.as_str()))
        .collect();
    for p in profiles {
        crate::worker::check_models(&boinc::Engine {
            image: job.engine.image.clone(),
            version: job.engine.version.clone(),
            profile: p.to_string(),
            models: engine::models_digest(engine::MODELS_LOCK, p)
                .context("a profile outside the model lock")?
                .to_string(),
            psm: job.engine.psm,
            dpi: job.engine.dpi,
            timeout_secs: job.engine.timeout_secs,
        })?;
    }
    let bytes = serde_json::to_vec_pretty(&report)?;
    ensure!(
        bytes.len() <= MAX_RESULT,
        "the report is over its size limit"
    );
    atomic(&work.join(RESULT_FILE), &bytes)?;
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

/// Checks the run against the job and adds the unit's identity.
pub fn finish(job: &Job, mut report: Report, host_id: Option<u64>) -> anyhow::Result<Report> {
    ensure!(
        report.bundle_version == job.bundle_version
            && report.manifest_sha256 == job.manifest_sha256,
        "the bundle's version or manifest differs from the job"
    );
    ensure!(
        report.engine.version == job.engine.version,
        "engine version {} differs from the job's {}",
        report.engine.version,
        job.engine.version
    );
    // The report leaves the host: free-form reasons (engine errors can quote stderr) become codes.
    for s in &mut report.skipped {
        s.reason = reason_code(&s.reason);
    }
    report.boinc = Some(BoincInfo {
        engine_base: job.engine.base.clone(),
        engine_image: job.engine.image.clone(),
        host_id,
        job_id: job.id.clone(),
    });
    Ok(report)
}

/// A skipped crop's reason as a code: `no-model:<lang>`, `engine-error` or `not-prepared`.
pub fn reason_code(reason: &str) -> String {
    if let Some(lang) = reason.strip_prefix("no installed model for ") {
        if super::validate::lang_ok(lang) {
            return format!("no-model:{lang}");
        }
    }
    if reason.starts_with("engine:") {
        return "engine-error".into();
    }
    "not-prepared".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_host_id() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(host_id(tmp.path()), None);
        std::fs::write(
            tmp.path().join("init_data.xml"),
            "<app_init_data>\n<hostid>4217</hostid>\n</app_init_data>\n",
        )
        .unwrap();
        assert_eq!(host_id(tmp.path()), Some(4217));
    }

    #[test]
    fn reasons_become_codes() {
        assert_eq!(reason_code("no installed model for fra"), "no-model:fra");
        assert_eq!(reason_code("engine: page text here"), "engine-error");
        assert_eq!(reason_code("bln600-archive is not staged"), "not-prepared");
    }
}
