//! Make gold units with stock `create_work`, one result each.
//!
//! Each unit's inputs, in input template order: its job (`gold.json`), the bundle tar (staged
//! once in the download tree, sticky), and every original file as a `--remote_file`: the BOINC
//! client downloads it from the publisher's URL and checks its size and md5, and keeps it as a
//! sticky file, so a host fetches it once for all the units it runs. The project server stores
//! none of the originals.
//!
//! Write-ahead, as for the assessment: `submitting` before `create_work`, `submitted` after. A
//! unit left at `submitting` is skipped until a person checks the server for its workunit name
//! (`usnm_gold_<job id>`) and either removes the marker (absent) or renames it to `submitted`.
use super::job::{Engine, File, Job, JOB_FILE, MAX_JSON, VERSION};
use super::{prepare, validate};
use crate::{atomic, read};
use anyhow::{ensure, Context};
use std::path::{Path, PathBuf};
use tokio::process::Command;
use usnm_ocr_contract::boinc::is_sha256;

pub const APP: &str = "usnm_gold";
pub const RESULT_TEMPLATE: &str = "templates/usnm_gold_out";

#[derive(Debug, Clone)]
pub struct Options {
    pub root: PathBuf,
    pub project: PathBuf,
    pub units: u32,
    pub engine: Engine,
}

#[derive(Debug, Default)]
pub struct Summary {
    pub submitted: usize,
    pub already: usize,
    pub uncertain: usize,
}

/// The bundle tar's physical name: one per bundle, shared by every unit.
pub fn bundle_physical(p: &prepare::Prepared) -> String {
    format!(
        "usnm_gold_v{}_{}.tar",
        p.bundle_version,
        &p.bundle.sha256[..16]
    )
}

/// The input template for a prepared bundle: the job, then the sticky tar and originals.
pub fn input_template(p: &prepare::Prepared) -> String {
    let mut infos = String::from("  <file_info><number>0</number></file_info>\n");
    let mut refs = format!(
        "    <file_ref><file_number>0</file_number><open_name>{JOB_FILE}</open_name><copy_file/></file_ref>\n"
    );
    let names = std::iter::once(&p.bundle.name).chain(p.remotes.iter().map(|r| &r.name));
    for (i, name) in names.enumerate() {
        let n = i + 1;
        // Kept on the host for later units, and never deleted from the server by file_deleter
        // (later units of the campaign still name the tar).
        infos.push_str(&format!(
            "  <file_info><number>{n}</number><sticky/><no_delete/></file_info>\n"
        ));
        refs.push_str(&format!(
            "    <file_ref><file_number>{n}</file_number><open_name>{name}</open_name><copy_file/></file_ref>\n"
        ));
    }
    format!("<input_template>\n{infos}  <workunit>\n{refs}  </workunit>\n</input_template>\n")
}

pub fn template_name(p: &prepare::Prepared) -> String {
    format!(
        "templates/usnm_gold_v{}_{}_in",
        p.bundle_version,
        &p.manifest_sha256[..16]
    )
}

pub fn job(p: &prepare::Prepared, engine: &Engine, unit: u32) -> anyhow::Result<Job> {
    let file = |s: &prepare::Staged| File {
        bytes: s.bytes,
        name: s.name.clone(),
        sha256: s.sha256.clone(),
    };
    let mut j = Job {
        bundle: file(&p.bundle),
        bundle_version: p.bundle_version,
        engine: engine.clone(),
        id: String::new(),
        manifest_sha256: p.manifest_sha256.clone(),
        remotes: p.remotes.iter().map(file).collect(),
        unit,
        v: VERSION,
    };
    j.id = j.identity()?;
    j.validate()?;
    ensure!(
        serde_json::to_vec_pretty(&j)?.len() <= MAX_JSON,
        "the job is over {MAX_JSON} bytes: the bundle names too many original files"
    );
    Ok(j)
}

pub fn create_work_args(p: &prepare::Prepared, job: &Job) -> anyhow::Result<Vec<String>> {
    let mut args: Vec<String> = [
        "--appname",
        APP,
        "--wu_name",
        &job.workunit(),
        "--wu_template",
        &template_name(p),
        "--result_template",
        RESULT_TEMPLATE,
        // One result per unit: each result measures the host that ran it, so results of one
        // unit are not expected to agree, and the validator checks each against the bundle.
        "--min_quorum",
        "1",
        "--target_nresults",
        "1",
        "--max_error_results",
        "3",
        "--max_total_results",
        "4",
        "--max_success_results",
        "1",
        "--rsc_fpops_est",
        "5e12",
        "--rsc_fpops_bound",
        "1e15",
        "--rsc_memory_bound",
        "1610612736",
        "--rsc_disk_bound",
        "2147483648",
        "--delay_bound",
        "259200",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.push(format!("{}_gold_json", job.id));
    args.push(bundle_physical(p));
    for r in &p.remotes {
        let url = r.url.as_deref().context("original file without a URL")?;
        ensure!(
            url.starts_with("https://") && !url.chars().any(|c| c.is_whitespace()),
            "bad URL {url}"
        );
        ensure!(r.md5.len() == 32, "bad md5 for {}", r.name);
        args.extend([
            "--remote_file".to_string(),
            url.to_string(),
            r.bytes.to_string(),
            r.md5.clone(),
        ]);
    }
    Ok(args)
}

async fn hier_path(project: &Path, physical: &str) -> anyhow::Result<PathBuf> {
    let output = Command::new(project.join("bin/dir_hier_path"))
        .arg(physical)
        .current_dir(project)
        .output()
        .await?;
    ensure!(output.status.success(), "dir_hier_path failed");
    let path = String::from_utf8(output.stdout)?;
    let path = Path::new(path.trim());
    ensure!(!path.as_os_str().is_empty(), "empty staging path");
    Ok(if path.is_absolute() {
        path.to_owned()
    } else {
        project.join(path)
    })
}

fn publish(path: &Path, from: &Path) -> anyhow::Result<()> {
    let parent = path.parent().context("no parent")?;
    crate::public_dirs(parent)?;
    let tmp = path.with_extension("part");
    std::fs::copy(from, &tmp)?;
    #[cfg(unix)]
    {
        // Apache serves the download tree through the BOINC group.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o664))?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn units_dir(root: &Path) -> PathBuf {
    root.join("units")
}

pub async fn run(opts: &Options) -> anyhow::Result<Summary> {
    ensure!(
        (1..=1000).contains(&opts.units),
        "--units must be 1 to 1000"
    );
    ensure!(
        opts.engine
            .image
            .strip_prefix("sha256:")
            .is_some_and(is_sha256),
        "--engine-image must be the worker image's full sha256 digest"
    );
    let project = opts.project.canonicalize()?;
    let p = prepare::read(&opts.root)?;
    let (bytes, _, sha) = prepare::md5_sha256(&opts.root.join(prepare::BUNDLE_COPY))?;
    ensure!(
        bytes == p.bundle.bytes && sha == p.bundle.sha256,
        "bundle.tar changed since prepare"
    );
    // The template, the shared tar and the validator's expectations, before any unit.
    atomic(
        &project.join(template_name(&p)),
        input_template(&p).as_bytes(),
    )?;
    let tar = hier_path(&project, &bundle_physical(&p)).await?;
    let current = tar.is_file()
        && prepare::md5_sha256(&tar)
            .is_ok_and(|(b, _, s)| b == p.bundle.bytes && s == p.bundle.sha256);
    if !current {
        publish(&tar, &opts.root.join(prepare::BUNDLE_COPY))?;
    }
    let mut summary = Summary::default();
    let mut jobs = Vec::new();
    for unit in 0..opts.units {
        jobs.push(job(&p, &opts.engine, unit)?);
    }
    validate::add_jobs(&validate::expected_dir(&project), &p, &jobs)?;
    for j in jobs {
        let dir = units_dir(&opts.root).join(format!("{:04}", j.unit));
        let path = dir.join(JOB_FILE);
        if path.exists() {
            let old: Job = serde_json::from_slice(&read(&path, MAX_JSON)?)?;
            ensure!(
                old == j,
                "{} was made for another engine or bundle; use a fresh campaign root",
                dir.display()
            );
        } else {
            atomic(&path, &serde_json::to_vec_pretty(&j)?)?;
        }
        if dir.join("submitted").exists() {
            summary.already += 1;
            continue;
        }
        if dir.join("submitting").exists() {
            summary.uncertain += 1;
            continue;
        }
        atomic(&dir.join("submitting"), j.workunit().as_bytes())?;
        publish(
            &hier_path(&project, &format!("{}_gold_json", j.id)).await?,
            &path,
        )?;
        let status = Command::new(project.join("bin/create_work"))
            .current_dir(&project)
            .args(create_work_args(&p, &j)?)
            .status()
            .await?;
        ensure!(
            status.success(),
            "create_work failed for {}; {} is uncertain until checked",
            j.workunit(),
            dir.display()
        );
        std::fs::rename(dir.join("submitting"), dir.join("submitted"))?;
        summary.submitted += 1;
    }
    Ok(summary)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::BTreeMap;

    pub(crate) fn prepared() -> prepare::Prepared {
        prepare::Prepared {
            bundle: prepare::Staged {
                bytes: 100,
                md5: "0".repeat(32),
                name: "bundle.tar".into(),
                sha256: "1".repeat(64),
                url: None,
            },
            bundle_version: 1,
            crops: BTreeMap::from([
                (
                    "a-1".to_string(),
                    (
                        "bln600".to_string(),
                        prepare::Expected {
                            ref_chars: 100,
                            ref_words: 20,
                        },
                    ),
                ),
                (
                    "b-1".to_string(),
                    (
                        "nzz".to_string(),
                        prepare::Expected {
                            ref_chars: 50,
                            ref_words: 10,
                        },
                    ),
                ),
            ]),
            manifest_sha256: "2".repeat(64),
            remotes: vec![prepare::Staged {
                bytes: 68,
                md5: "3".repeat(32),
                name: format!("{}.zip", "4".repeat(64)),
                sha256: "4".repeat(64),
                url: Some("https://ndownloader.figshare.com/files/1".into()),
            }],
        }
    }

    pub(crate) fn engine() -> Engine {
        Engine {
            base: format!("docker.io/t@sha256:{}", "5".repeat(64)),
            dpi: 300,
            image: format!("sha256:{}", "6".repeat(64)),
            psm: 3,
            timeout_secs: 300,
            version: "5.5.0".into(),
        }
    }

    #[test]
    fn template_and_args_list_the_same_files() {
        let p = prepared();
        let j = job(&p, &engine(), 0).unwrap();
        let t = input_template(&p);
        assert_eq!(t.matches("<file_info>").count(), 3);
        assert_eq!(t.matches("<sticky/>").count(), 2);
        assert!(t.contains(&format!("<open_name>{}</open_name>", p.remotes[0].name)));
        let a = create_work_args(&p, &j).unwrap();
        let i = a.iter().position(|s| s == "--remote_file").unwrap();
        assert_eq!(
            &a[i..],
            [
                "--remote_file",
                "https://ndownloader.figshare.com/files/1",
                "68",
                &"3".repeat(32)
            ]
        );
        assert_eq!(a[i - 1], bundle_physical(&p));
        assert_eq!(a[i - 2], format!("{}_gold_json", j.id));
        assert_ne!(job(&p, &engine(), 1).unwrap().id, j.id);
    }
}
