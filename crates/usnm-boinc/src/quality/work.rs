//! One quality unit inside BOINC's job container. No network.
//!
//! [`compute`] is the whole computation as a library call: the page's
//! features, the readers' pairwise metrics, the hat, the consensus,
//! findability and confusion pairs, with timings. [`run`] wraps it in the
//! slot: diagnostics (the BOINC client version and plan class from
//! `init_data.xml`, the container runtime), the job's checks, and the three
//! output files, written whatever happened, so a host that could not run
//! the unit still returns why.
use super::{
    outputs, parse_alto, ALTO_FILE, CONFUSIONS_FILE, DIAG_FILE, JOB_FILE, PAGE_FILE, PLACES_FILE,
    RESULT_FILE, TEXTS_FILE,
};
use crate::surya::work::{boinc_info, container_info, tail, Diag};
use crate::{atomic, read};
use anyhow::{ensure, Context};
use std::{
    collections::BTreeSet,
    path::Path,
    time::{Duration, Instant},
};
use usnm_ocr_contract::{
    boinc::sha256,
    quality::{
        Engine, Host, Job, Outcome, ResultManifest, Texts, Timings, MAX_ALTO, MAX_ERROR,
        MAX_JOB_JSON, MAX_PLACES, MAX_SCAN, MAX_TEXTS,
    },
};
use usnm_ocr_quality::{
    features,
    findability::Places,
    report::{self, PageFeatures, PageInputs, ReaderInput},
};

/// What one unit holds, in memory.
#[derive(Debug, Clone)]
pub struct Inputs<'a> {
    pub job: &'a Job,
    /// `page.jpg`.
    pub page: &'a [u8],
    /// `loc.xml`.
    pub alto: &'a [u8],
    pub texts: &'a Texts,
    /// `places.txt`.
    pub places: &'a str,
}

/// What [`compute`] gives: the result (host and time of completion left
/// for the caller) and the confusion pairs as the bytes of
/// `confusions.json`.
#[derive(Debug, Clone, PartialEq)]
pub struct Computed {
    pub result: ResultManifest,
    pub confusions: Vec<u8>,
}

fn ms(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// How long the computation may run before the unit returns `timeout`.
/// The computation is a few seconds on a page of ordinary size (UNVERIFIED
/// on volunteers' hosts); the bound is far above it so a slow host still
/// finishes.
pub const DEADLINE: Duration = Duration::from_secs(1800);

/// The process's peak resident set in KiB (`VmHWM`), where /proc has it.
pub(crate) fn peak_rss_kib() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    s.lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.split_whitespace().next()?.parse().ok())
}

/// The process's CPU time (user plus system) in milliseconds from
/// `/proc/self/stat`, at the kernel's usual 100 ticks a second.
pub(crate) fn cpu_ms() -> Option<u64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    let (_, rest) = stat.rsplit_once(')')?;
    let f: Vec<&str> = rest.split_whitespace().collect();
    let ticks = f.get(11)?.parse::<u64>().ok()? + f.get(12)?.parse::<u64>().ok()?;
    Some(ticks * 10)
}

/// The computation for one unit: deterministic, single-threaded.
pub fn compute(inputs: &Inputs) -> anyhow::Result<Computed> {
    let started = Instant::now();
    let job = inputs.job;
    job.validate()?;
    inputs.texts.validate(job)?;
    let t = Instant::now();
    let image = image::load_from_memory(inputs.page)
        .context("decoding page.jpg")?
        .to_luma8();
    let (w, h) = image.dimensions();
    let alto = parse_alto(inputs.alto, f64::from(w), f64::from(h))?;
    let decode_ms = ms(t);
    let t = Instant::now();
    let page = PageFeatures {
        image: Some(report::round_image(features::image_features(&image))),
        layout: Some(report::round_layout(features::layout_features(&alto))),
    };
    let features_ms = ms(t);
    let t = Instant::now();
    let places = Places::parse(inputs.places);
    let readers: Vec<ReaderInput> = job
        .readers
        .iter()
        .map(|spec| {
            let text = inputs
                .texts
                .readers
                .iter()
                .find(|r| r.name == spec.name)
                .map(|r| r.text.clone())
                .unwrap_or_default();
            ReaderInput {
                name: spec.name.clone(),
                source: spec.source.clone(),
                text,
            }
        })
        .collect();
    let function_words: BTreeSet<String> = job
        .function_words
        .iter()
        .map(|w| usnm_ocr_quality::consensus::fold(w))
        .collect();
    let analysis = report::analyse(&PageInputs {
        image: None,
        alto: None,
        readers,
        function_words,
        places: &places,
    });
    let text_ms = ms(t);
    let confusions = serde_json::to_vec_pretty(&analysis.confusions)?;
    let r = analysis.report;
    Ok(Computed {
        result: ResultManifest {
            v: usnm_ocr_contract::quality::VERSION,
            job_id: job.id.clone(),
            completed_at: String::new(),
            confusions_bytes: confusions.len() as u64,
            confusions_sha256: sha256(&confusions),
            diag_bytes: 0,
            diag_sha256: sha256(b""),
            engine: job.engine.clone(),
            hat: Some(r.hat),
            host: Host::default(),
            legibility: Some(r.legibility),
            outcome: Outcome::Ok,
            page: Some(page),
            readers: r.readers,
            timings: Timings {
                cpu_ms: None,
                decode_ms,
                features_ms,
                peak_rss_kib: None,
                text_ms,
                total_ms: ms(started),
            },
            unit_error: None,
        },
        confusions,
    })
}

/// The unit's files, read and checked against the job.
pub struct Loaded {
    pub job: Job,
    pub page: Vec<u8>,
    pub alto: Vec<u8>,
    pub texts: Texts,
    pub places: String,
}

/// Checks the unit before anything runs: the job and every input's checksum.
pub fn prepare(work: &Path) -> anyhow::Result<Loaded> {
    let job: Job = serde_json::from_slice(&read(&work.join(JOB_FILE), MAX_JOB_JSON)?)?;
    job.validate()?;
    let page = read(&work.join(PAGE_FILE), MAX_SCAN)?;
    ensure!(
        job.inputs.page.matches(&page),
        "input page checksum mismatch"
    );
    let alto = read(&work.join(ALTO_FILE), MAX_ALTO)?;
    ensure!(
        job.inputs.alto.matches(&alto),
        "input ALTO checksum mismatch"
    );
    let texts_bytes = read(&work.join(TEXTS_FILE), MAX_TEXTS)?;
    ensure!(
        job.inputs.texts.matches(&texts_bytes),
        "input texts checksum mismatch"
    );
    let texts: Texts = serde_json::from_slice(&texts_bytes).context("texts.json")?;
    texts.validate(&job)?;
    let places_bytes = read(&work.join(PLACES_FILE), MAX_PLACES)?;
    ensure!(
        job.inputs.places.matches(&places_bytes),
        "input places checksum mismatch"
    );
    let places = String::from_utf8(places_bytes).context("places.txt is not UTF-8")?;
    Ok(Loaded {
        job,
        page,
        alto,
        texts,
        places,
    })
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

/// A result with no record: a unit error, or a computation that failed.
fn empty(
    job_id: String,
    engine: Engine,
    outcome: Outcome,
    error: Option<String>,
) -> ResultManifest {
    ResultManifest {
        v: usnm_ocr_contract::quality::VERSION,
        job_id,
        completed_at: String::new(),
        confusions_bytes: 0,
        confusions_sha256: String::new(),
        diag_bytes: 0,
        diag_sha256: String::new(),
        engine,
        hat: None,
        host: Host::default(),
        legibility: None,
        outcome,
        page: None,
        readers: vec![],
        timings: Timings::default(),
        unit_error: error,
    }
}

/// Runs [`compute`] on its own thread and waits at most `deadline`. A
/// computation that overruns is left to finish on its own (the process
/// exits once the outputs are written); its result is not used. On error
/// the loaded job comes back so the result can name it.
fn compute_with_deadline(
    l: Loaded,
    deadline: Duration,
) -> Result<Computed, Box<(Job, anyhow::Error)>> {
    let job = l.job.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("quality-compute".into())
        .stack_size(64 << 20)
        .spawn(move || {
            let r = compute(&Inputs {
                job: &l.job,
                page: &l.page,
                alto: &l.alto,
                texts: &l.texts,
                places: &l.places,
            });
            let _ = tx.send(r);
        })
        .map_err(|e| {
            Box::new((
                job.clone(),
                anyhow::Error::from(e).context("starting the computation"),
            ))
        })?;
    match rx.recv_timeout(deadline) {
        Ok(Ok(c)) => Ok(c),
        Ok(Err(e)) => Err(Box::new((job, e))),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(Box::new((
            job,
            anyhow::anyhow!("computation exceeded {} s", deadline.as_secs()),
        ))),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(Box::new((
            job,
            anyhow::anyhow!("computation thread ended without a result"),
        ))),
    }
}

fn run_blocking(work: &Path) -> anyhow::Result<()> {
    let mut diag = Diag::new();
    let mut surya_host = usnm_ocr_contract::surya::Host::default();
    boinc_info(work, &mut surya_host, &mut diag);
    container_info(&mut surya_host, &mut diag);
    let host = Host {
        boinc_client: surya_host.boinc_client,
        cpu_threads: surya_host.cpu_threads,
        ncpus: surya_host.ncpus,
        plan_class: surya_host.plan_class,
        rootless: surya_host.rootless,
        runtime: surya_host.runtime,
    };
    let (mut result, confusions) = match prepare(work) {
        Ok(l) => {
            diag.section(
                "unit",
                &format!(
                    "job {} doc {} group {} language {}: readers {:?}",
                    l.job.id,
                    l.job.doc_id,
                    l.job.group,
                    l.job.language,
                    l.job
                        .readers
                        .iter()
                        .map(|r| r.name.as_str())
                        .collect::<Vec<_>>()
                ),
                1024,
            );
            let started = Instant::now();
            match compute_with_deadline(l, DEADLINE) {
                Ok(c) => {
                    diag.section("timings", &format!("{:?}", c.result.timings), 512);
                    (c.result, c.confusions)
                }
                Err(boxed) => {
                    let (job, e) = *boxed;
                    let outcome = if started.elapsed() >= DEADLINE {
                        Outcome::Timeout
                    } else {
                        Outcome::Failed
                    };
                    diag.section("computation failed", &format!("{e:#}"), 2048);
                    let mut r = empty(job.id.clone(), job.engine.clone(), outcome, None);
                    r.timings.total_ms = ms(started);
                    (r, b"{}".to_vec())
                }
            }
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
            let engine = job
                .and_then(|j| serde_json::from_value::<Engine>(j.get("engine")?.clone()).ok())
                .unwrap_or_else(|| Engine {
                    image: format!("sha256:{}", "0".repeat(64)),
                });
            (
                empty(
                    id,
                    engine,
                    Outcome::UnitError,
                    Some(tail(&msg, MAX_ERROR).to_owned()),
                ),
                b"{}".to_vec(),
            )
        }
    };
    result.timings.cpu_ms = cpu_ms();
    result.timings.peak_rss_kib = peak_rss_kib();
    diag.section(
        "process",
        &format!(
            "cpu {:?} ms, peak rss {:?} KiB, deadline {} s",
            result.timings.cpu_ms,
            result.timings.peak_rss_kib,
            DEADLINE.as_secs()
        ),
        256,
    );
    let diag = diag.finish();
    result.host = host;
    result.completed_at = chrono::Utc::now().to_rfc3339();
    result.confusions_bytes = confusions.len() as u64;
    result.confusions_sha256 = sha256(&confusions);
    result.diag_bytes = diag.len() as u64;
    result.diag_sha256 = sha256(&diag);
    atomic(&work.join(CONFUSIONS_FILE), &confusions)?;
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
    use usnm_ocr_contract::quality::{PageSize, TextReader};

    /// A light page with dark text-like rows, as a JPEG.
    pub(crate) fn synthetic_jpeg(w: u32, h: u32) -> Vec<u8> {
        let mut img = image::GrayImage::from_pixel(w, h, image::Luma([235]));
        let mut row = 60;
        while row + 6 < h - 60 {
            for x in 40..w - 40 {
                if (x / 10) % 10 == 9 {
                    continue;
                }
                for dy in 0..6 {
                    img.put_pixel(x, row + dy, image::Luma([30]));
                }
            }
            row += 24;
        }
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageLuma8(img)
            .write_to(&mut out, image::ImageFormat::Jpeg)
            .unwrap();
        out.into_inner()
    }

    pub(crate) const TRUTH: &str =
        "The mayor of Albany spoke with the council about bridges and roads across the river";

    /// A synthetic unit: page, ALTO with the truth's words in two lines,
    /// three readers (one exact, two corrupted), two place names.
    pub(crate) fn unit(dir: &Path) -> Job {
        let page = synthetic_jpeg(1000, 1400);
        let words: Vec<&str> = TRUTH.split(' ').collect();
        fn line<'a>(ws: &[&'a str], y: u32) -> Vec<(u32, u32, u32, u32, &'a str, f64)> {
            ws.iter()
                .enumerate()
                .map(|(i, w)| {
                    (
                        50 + i as u32 * 95,
                        y,
                        50 + i as u32 * 95 + 85,
                        y + 30,
                        *w,
                        0.9,
                    )
                })
                .collect()
        }
        let alto = super::super::alto_xml(
            1000,
            1400,
            &[vec![line(&words[..8], 60), line(&words[8..], 100)]],
        );
        let texts = Texts {
            page: Some(PageSize {
                height: 1400,
                width: 1000,
            }),
            readers: vec![
                TextReader {
                    alto: false,
                    name: "loc".into(),
                    regions: vec![],
                    source: "loc-alto".into(),
                    text: TRUTH.into(),
                },
                TextReader {
                    alto: false,
                    name: "as".into(),
                    regions: vec![],
                    source: "american-stories".into(),
                    text: "The mayor of Albany spoke with the counci1 about bridges and roads across the rivor".into(),
                },
                TextReader {
                    alto: false,
                    name: "ours-tesseract".into(),
                    regions: vec![],
                    source: "ocr-out:tesseract".into(),
                    text: "The rnayor of Albany spoke with the council about bridges and roads".into(),
                },
            ],
        };
        super::super::stage::local_unit(&super::super::stage::LocalUnit {
            page: &page,
            alto: alto.as_bytes(),
            texts: &texts,
            places: "Albany\nTroy\n",
            dir,
            engine: Engine {
                image: format!("sha256:{}", "a".repeat(64)),
            },
            function_words: vec!["with".into(), "the".into(), "about".into()],
            crop_seed: 522,
            k_crops: 4,
        })
        .unwrap()
    }

    #[test]
    fn compute_on_a_synthetic_page() {
        let tmp = tempfile::tempdir().unwrap();
        let job = unit(tmp.path());
        let l = prepare(tmp.path()).unwrap();
        assert_eq!(l.job, job);
        let c = compute(&Inputs {
            job: &l.job,
            page: &l.page,
            alto: &l.alto,
            texts: &l.texts,
            places: &l.places,
        })
        .unwrap();
        let r = &c.result;
        assert_eq!(r.outcome, Outcome::Ok);
        assert_eq!(r.readers.len(), 3);
        assert!(r.hat.as_ref().unwrap().determined);
        // The exact reader's estimate is near zero, not zero: the two
        // corrupted readers' errors overlap in the dropped tail.
        let loc = r.readers[0].est_cer.unwrap();
        assert!(loc < 0.02, "{loc}");
        assert!(r.readers[1].est_cer.unwrap() > loc);
        assert!(r.readers[2].est_cer.unwrap() > loc);
        assert!(r.readers[2].completeness.unwrap() < 1.0);
        assert_eq!(r.readers[0].findable_places, Some(1.0));
        let page = r.page.as_ref().unwrap();
        let img = page.image.as_ref().unwrap();
        assert_eq!((img.width, img.height), (1000, 1400));
        assert!(img.skew_deg.abs() <= 0.1);
        let layout = page.layout.as_ref().unwrap();
        assert_eq!((layout.blocks, layout.lines, layout.words), (1, 2, 15));
        assert_eq!(layout.columns, 1);
        let confusions: std::collections::BTreeMap<String, serde_json::Value> =
            serde_json::from_slice(&c.confusions).unwrap();
        assert_eq!(confusions.len(), 3);
        // The same inputs give the same bytes.
        let again = compute(&Inputs {
            job: &l.job,
            page: &l.page,
            alto: &l.alto,
            texts: &l.texts,
            places: &l.places,
        })
        .unwrap();
        assert_eq!(again.result.canonical(), r.canonical());
        assert_eq!(again.confusions, c.confusions);
    }

    #[tokio::test]
    async fn unit_writes_all_outputs_and_validates() {
        let tmp = tempfile::tempdir().unwrap();
        unit(tmp.path()).validate().unwrap();
        run(tmp.path()).await.unwrap();
        let files: Vec<PathBuf> = outputs().iter().map(|n| tmp.path().join(n)).collect();
        let l = crate::quality::validate::load(&files).unwrap();
        assert_eq!(l.result.outcome, Outcome::Ok);
        assert!(tmp.path().join("fraction_done").exists());
        // A broken page still returns a result, with the reason.
        std::fs::write(tmp.path().join(PAGE_FILE), b"not the page").unwrap();
        run(tmp.path()).await.unwrap();
        let l = crate::quality::validate::load(&files).unwrap();
        assert_eq!(l.result.outcome, Outcome::UnitError);
        assert!(l.result.unit_error.unwrap().contains("checksum"));
    }
}
