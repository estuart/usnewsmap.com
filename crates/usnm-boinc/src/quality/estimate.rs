//! The same computation as the unit, offline, without BOINC.
//!
//! Two layouts are read:
//!
//! - Per-page directories: `<dir>/<page>/{page.jpg, loc.xml, texts.json}`
//!   with optional `ours.xml` and `{qwen.json, readings.json}` (the
//!   `usnm_qwen` app's job and output for the page).
//! - The engine survey's: `<dir>/sample/images/<page>.jpg`,
//!   `sample/alto/<page>.xml`, `sample/loctext/<page>.txt` (LoC's text,
//!   reader `loc`) and one `--reader NAME=PATTERN` per engine, the pattern
//!   a path with one `*` that stands for the page name (for instance
//!   `out/tesseract/*.txt`). A page is read when its image, ALTO and LoC
//!   text exist; a reader whose file is missing for a page is left out of
//!   that page. Optional readings: `readings/<page>.json` alongside its
//!   originating job `readings/<page>.qwen.json` (or `qwen/<page>.json`).
//!
//! The campaign's `places.txt` and function words apply to every page.
//! Writes `<out>/<page>/{result.json, confusions.json}` and, where the page
//! has readings, `crops.jsonl` (the rows of `crops.parquet`, one per line),
//! plus `<out>/pages.jsonl` and `<out>/crops.jsonl` over every page.
use super::{
    collect::{self, CropInputs, OURS_FILE},
    parse_alto,
    stage::read_words,
    work, ALTO_FILE, CONFUSIONS_FILE, PAGE_FILE, RESULT_FILE, TEXTS_FILE,
};
use crate::{atomic, read};
use anyhow::{ensure, Context};
use image::GenericImageView as _;
use std::{
    collections::BTreeSet,
    fmt::Write as _,
    path::{Path, PathBuf},
};
use usnm_ocr_contract::{
    quality::{Engine, TextReader, Texts, MAX_ALTO, MAX_SCAN, MAX_TEXT, MAX_TEXTS},
    qwen::{
        Job as QwenJob, Readings, MAX_JOB_JSON as MAX_QWEN_JOB_JSON, MAX_READINGS,
        VERSION as QWEN_VERSION,
    },
};

pub const READINGS_FILE: &str = "readings.json";

#[derive(Debug, Clone)]
pub struct Options {
    pub dir: PathBuf,
    pub out: PathBuf,
    pub places: PathBuf,
    pub function_words: PathBuf,
    pub crop_seed: u64,
    pub k_crops: u32,
    /// `NAME=PATTERN` for the survey layout; empty for per-page directories.
    pub readers: Vec<String>,
}

#[derive(Debug, Default, PartialEq)]
pub struct Summary {
    pub pages: usize,
    pub with_readings: usize,
    pub failed: usize,
    pub crop_rows: usize,
}

/// One page's inputs, from either layout.
struct PageFiles {
    name: String,
    page: PathBuf,
    alto: PathBuf,
    /// A `texts.json`, or the reader texts to assemble one from.
    texts: Result<PathBuf, Vec<(String, String, PathBuf)>>,
    ours: Option<PathBuf>,
    readings: Option<PathBuf>,
    qwen_job: PathBuf,
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// `NAME=PATTERN` with the `*` replaced by the page name.
fn reader_path(spec: &str, page: &str) -> anyhow::Result<(String, PathBuf)> {
    let (name, pattern) = spec
        .split_once('=')
        .filter(|(n, p)| !n.is_empty() && p.contains('*'))
        .with_context(|| format!("--reader wants NAME=PATTERN with one *, got {spec:?}"))?;
    usnm_ocr_contract::quality::check_name(name, "reader name")?;
    Ok((
        name.to_owned(),
        PathBuf::from(pattern.replacen('*', page, 1)),
    ))
}

fn per_page_dirs(dir: &Path) -> anyhow::Result<Vec<PageFiles>> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {dir:?}"))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.join(TEXTS_FILE).exists())
        .collect();
    dirs.sort();
    Ok(dirs
        .into_iter()
        .map(|p| PageFiles {
            name: file_name(&p),
            page: p.join(PAGE_FILE),
            alto: p.join(ALTO_FILE),
            texts: Ok(p.join(TEXTS_FILE)),
            ours: Some(p.join(OURS_FILE)).filter(|o| o.exists()),
            readings: Some(p.join(READINGS_FILE)).filter(|r| r.exists()),
            qwen_job: p.join(crate::qwen::JOB_FILE),
        })
        .collect())
}

fn survey_dirs(dir: &Path, readers: &[String]) -> anyhow::Result<Vec<PageFiles>> {
    let images = dir.join("sample/images");
    let mut pages: Vec<PathBuf> = std::fs::read_dir(&images)
        .with_context(|| format!("reading {images:?}"))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "jpg"))
        .collect();
    pages.sort();
    let mut out = Vec::new();
    for image in pages {
        let name = image
            .file_stem()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let alto = dir.join("sample/alto").join(format!("{name}.xml"));
        let loctext = dir.join("sample/loctext").join(format!("{name}.txt"));
        if !alto.exists() || !loctext.exists() {
            continue;
        }
        let mut texts = vec![("loc".to_owned(), "loc-alto".to_owned(), loctext)];
        for spec in readers {
            let (rname, path) = reader_path(spec, &name)?;
            let path = if path.is_absolute() {
                path
            } else {
                dir.join(path)
            };
            if path.exists() {
                texts.push((rname.clone(), format!("survey:{rname}"), path));
            }
        }
        let readings = dir.join("readings").join(format!("{name}.json"));
        let qwen_sidecar = dir.join("readings").join(format!("{name}.qwen.json"));
        let qwen_job = if qwen_sidecar.exists() {
            qwen_sidecar
        } else {
            dir.join("qwen").join(format!("{name}.json"))
        };
        out.push(PageFiles {
            name,
            page: image,
            alto,
            texts: Err(texts),
            ours: None,
            readings: Some(readings).filter(|r| r.exists()),
            qwen_job,
        });
    }
    Ok(out)
}

fn load_texts(files: &PageFiles) -> anyhow::Result<Texts> {
    match &files.texts {
        Ok(p) => Ok(serde_json::from_slice(&read(p, MAX_TEXTS)?)?),
        Err(list) => Ok(Texts {
            page: None,
            readers: list
                .iter()
                .map(|(name, source, path)| {
                    Ok(TextReader {
                        alto: false,
                        name: name.clone(),
                        regions: vec![],
                        source: source.clone(),
                        text: String::from_utf8(read(path, MAX_TEXT)?)
                            .with_context(|| format!("{path:?} is not UTF-8"))?,
                    })
                })
                .collect::<anyhow::Result<_>>()?,
        }),
    }
}

pub fn run(opts: &Options) -> anyhow::Result<Summary> {
    let places = std::fs::read_to_string(&opts.places)
        .with_context(|| format!("reading {:?}", opts.places))?;
    let function_words = read_words(&opts.function_words)?;
    let pages = if opts.dir.join("sample/images").is_dir() {
        survey_dirs(&opts.dir, &opts.readers)?
    } else {
        anyhow::ensure!(
            opts.readers.is_empty(),
            "--reader applies to the survey layout (<dir>/sample/images)"
        );
        per_page_dirs(&opts.dir)?
    };
    let mut summary = Summary::default();
    let (mut all_pages, mut all_crops) = (String::new(), String::new());
    for files in pages {
        let name = files.name.clone();
        summary.pages += 1;
        let staged = tempfile::tempdir()?;
        let outcome = (|| -> anyhow::Result<()> {
            let texts = load_texts(&files)?;
            let job = super::stage::local_unit(&super::stage::LocalUnit {
                page: &read(&files.page, MAX_SCAN)?,
                alto: &read(&files.alto, MAX_ALTO)?,
                texts: &texts,
                places: &places,
                dir: staged.path(),
                engine: Engine {
                    image: format!("sha256:{}", "0".repeat(64)),
                },
                function_words: function_words.clone(),
                crop_seed: opts.crop_seed,
                k_crops: opts.k_crops,
            })?;
            let l = work::prepare(staged.path())?;
            let c = work::compute(&work::Inputs {
                job: &job,
                page: &l.page,
                alto: &l.alto,
                texts: &l.texts,
                places: &l.places,
            })?;
            let crops_out = if let Some(rp) = &files.readings {
                let readings: Readings = serde_json::from_slice(&read(rp, MAX_READINGS)?)
                    .with_context(|| format!("{rp:?}"))?;
                let qwen_job: QwenJob =
                    serde_json::from_slice(&read(&files.qwen_job, MAX_QWEN_JOB_JSON)?)
                        .with_context(|| {
                            format!("{:?} requires originating job {:?}", rp, files.qwen_job)
                        })?;
                qwen_job
                    .validate()
                    .with_context(|| format!("{:?}", files.qwen_job))?;
                ensure!(
                    qwen_job.input.matches(&l.page),
                    "{:?} input does not match {:?}",
                    files.qwen_job,
                    files.page
                );
                ensure!(
                    readings.v == QWEN_VERSION
                        && readings.job_id == qwen_job.id
                        && readings.prompt_version == qwen_job.prompt_version,
                    "{rp:?} does not belong to {:?}",
                    files.qwen_job
                );
                let crop_ids: BTreeSet<&str> =
                    qwen_job.crops.iter().map(|c| c.id.as_str()).collect();
                let mut seen_ids = BTreeSet::new();
                for r in &readings.readings {
                    ensure!(
                        crop_ids.contains(r.id.as_str()),
                        "{rp:?} has crop {:?} not in {:?}",
                        r.id,
                        files.qwen_job
                    );
                    ensure!(
                        seen_ids.insert(r.id.as_str()),
                        "{rp:?} repeats crop {:?}",
                        r.id
                    );
                }
                let (w, h) = image::load_from_memory(&l.page)?.dimensions();
                let loc = parse_alto(&l.alto, f64::from(w), f64::from(h))?;
                for crop in &qwen_job.crops {
                    ensure!(
                        crop.loc_text == usnm_ocr_quality::crops::text_in_box(&loc, crop.rect),
                        "{:?} crop {:?} does not match {:?}",
                        files.qwen_job,
                        crop.id,
                        files.alto
                    );
                }
                let ours = match &files.ours {
                    Some(o) => Some(parse_alto(&read(o, MAX_ALTO)?, f64::from(w), f64::from(h))?),
                    None => None,
                };
                let readers: Vec<String> = job.readers.iter().map(|r| r.name.clone()).collect();
                let rows = collect::crop_rows(&CropInputs {
                    doc_id: &name,
                    loc: &loc,
                    ours: ours.as_ref(),
                    texts: &l.texts,
                    readers: &readers,
                    crops: &qwen_job.crops,
                    readings: Some(&readings),
                });
                let mut text = String::new();
                for row in &rows {
                    let _ = writeln!(text, "{}", serde_json::to_string(row)?);
                }
                Some((text, rows.len()))
            } else {
                None
            };
            let mut job_named = job.clone();
            job_named.doc_id = name.clone();
            let mut page_text = String::new();
            for row in collect::page_rows(&job_named, &c.result, crops_out.is_some()) {
                page_text.push_str(&serde_json::to_string(&row)?);
                page_text.push('\n');
            }
            let result_bytes = serde_json::to_vec_pretty(&c.result)?;
            let out = opts.out.join(&name);
            std::fs::create_dir_all(&out)?;
            atomic(&out.join(RESULT_FILE), &result_bytes)?;
            atomic(&out.join(CONFUSIONS_FILE), &c.confusions)?;
            if let Some((crop_text, row_count)) = crops_out {
                atomic(&out.join("crops.jsonl"), crop_text.as_bytes())?;
                all_crops.push_str(&crop_text);
                summary.crop_rows += row_count;
                summary.with_readings += 1;
            } else {
                let _ = std::fs::remove_file(out.join("crops.jsonl"));
            }
            all_pages.push_str(&page_text);
            Ok(())
        })();
        if let Err(e) = outcome {
            let _ = std::fs::remove_dir_all(opts.out.join(&name));
            summary.failed += 1;
            eprintln!("{name}: {e:#}");
        }
    }
    std::fs::create_dir_all(&opts.out)?;
    atomic(&opts.out.join("pages.jsonl"), all_pages.as_bytes())?;
    atomic(&opts.out.join("crops.jsonl"), all_crops.as_bytes())?;
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(root: &Path, dir: PathBuf, readers: Vec<String>) -> Options {
        std::fs::write(root.join("places.txt"), "Albany\n").unwrap();
        std::fs::write(root.join("fw.txt"), "the\nwith\n").unwrap();
        Options {
            dir,
            out: root.join("out"),
            places: root.join("places.txt"),
            function_words: root.join("fw.txt"),
            crop_seed: 522,
            k_crops: 4,
            readers,
        }
    }

    #[test]
    fn estimates_a_directory_and_joins_readings() {
        let tmp = tempfile::tempdir().unwrap();
        let page = tmp.path().join("pages/p1");
        let job = crate::quality::work::tests::unit(&page);
        let opts = options(tmp.path(), tmp.path().join("pages"), vec![]);
        let s = run(&opts).unwrap();
        assert_eq!(
            s,
            Summary {
                pages: 1,
                with_readings: 0,
                failed: 0,
                crop_rows: 0,
            }
        );
        assert!(tmp.path().join("out/p1/result.json").exists());
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("out/pages.jsonl"))
                .unwrap()
                .lines()
                .count(),
            3
        );
        // Readings for the page's crops join into crops.jsonl via the originating qwen.json.
        let qwen_job = crate::qwen::stage::local_unit(
            &page.join(PAGE_FILE),
            &page.join(ALTO_FILE),
            &page,
            crate::qwen::stage::tests::engine(),
            999,
            4,
            600,
            4,
        )
        .unwrap();
        let readings = Readings {
            v: usnm_ocr_contract::qwen::VERSION,
            job_id: qwen_job.id.clone(),
            device: "cpu".into(),
            diag: Default::default(),
            model: None,
            outcome: usnm_ocr_contract::qwen::Outcome::Ok,
            prompt_version: 1,
            readings: qwen_job
                .crops
                .iter()
                .map(|c| usnm_ocr_contract::qwen::Reading {
                    id: c.id.clone(),
                    ms: 1,
                    text: c.loc_text.clone(),
                    tokens: 1,
                })
                .collect(),
        };
        std::fs::write(
            page.join(READINGS_FILE),
            serde_json::to_vec(&readings).unwrap(),
        )
        .unwrap();
        let s = run(&opts).unwrap();
        assert_eq!(s.with_readings, 1);
        assert_eq!(s.failed, 0);
        assert_eq!(s.crop_rows, qwen_job.crops.len() * 4);
        assert!(tmp.path().join("out/p1/crops.jsonl").exists());

        // Mismatched job_id in readings.json is rejected and clears stale outputs.
        let mut bad_readings = readings.clone();
        bad_readings.job_id = job.id;
        std::fs::write(
            page.join(READINGS_FILE),
            serde_json::to_vec(&bad_readings).unwrap(),
        )
        .unwrap();
        let s = run(&opts).unwrap();
        assert_eq!(
            s,
            Summary {
                pages: 1,
                with_readings: 0,
                failed: 1,
                crop_rows: 0,
            }
        );
        assert!(!tmp.path().join("out/p1").exists());
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("out/pages.jsonl")).unwrap(),
            ""
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("out/crops.jsonl")).unwrap(),
            ""
        );

        // Missing qwen.json alongside readings.json is rejected and writes no outputs.
        std::fs::write(
            page.join(READINGS_FILE),
            serde_json::to_vec(&readings).unwrap(),
        )
        .unwrap();
        std::fs::remove_file(page.join(crate::qwen::JOB_FILE)).unwrap();
        let s = run(&opts).unwrap();
        assert_eq!(
            s,
            Summary {
                pages: 1,
                with_readings: 0,
                failed: 1,
                crop_rows: 0,
            }
        );
        assert!(!tmp.path().join("out/p1").exists());
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("out/pages.jsonl")).unwrap(),
            ""
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("out/crops.jsonl")).unwrap(),
            ""
        );
    }

    #[test]
    fn reads_the_survey_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let unit = tmp.path().join("unit");
        crate::quality::work::tests::unit(&unit);
        let l = crate::quality::work::prepare(&unit).unwrap();
        let s = tmp.path().join("survey");
        for d in ["sample/images", "sample/alto", "sample/loctext", "out/tess"] {
            std::fs::create_dir_all(s.join(d)).unwrap();
        }
        std::fs::write(s.join("sample/images/pg.jpg"), &l.page).unwrap();
        std::fs::write(s.join("sample/alto/pg.xml"), &l.alto).unwrap();
        std::fs::write(s.join("sample/loctext/pg.txt"), &l.texts.readers[0].text).unwrap();
        std::fs::write(s.join("out/tess/pg.txt"), &l.texts.readers[2].text).unwrap();
        // A page without an ALTO is skipped.
        std::fs::write(s.join("sample/images/other.jpg"), &l.page).unwrap();
        let opts = options(
            tmp.path(),
            s.clone(),
            vec![
                "tess=out/tess/*.txt".into(),
                "absent=out/absent/*.txt".into(),
            ],
        );
        let r = run(&opts).unwrap();
        assert_eq!(r.pages, 1);
        assert_eq!(r.failed, 0);
        let result: usnm_ocr_contract::quality::ResultManifest =
            serde_json::from_slice(&std::fs::read(tmp.path().join("out/pg/result.json")).unwrap())
                .unwrap();
        let names: Vec<&str> = result.readers.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["loc", "tess"]);
        assert!(reader_path("bad", "pg").is_err());
        assert!(reader_path("x=no-star.txt", "pg").is_err());
        assert_eq!(
            reader_path("x=a/*.txt", "pg").unwrap().1,
            PathBuf::from("a/pg.txt")
        );
    }
}
