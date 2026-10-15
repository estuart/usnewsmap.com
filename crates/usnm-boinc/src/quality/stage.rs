//! Fetch each page's JPEG and ALTO from LoC (or the scan archive), join the
//! texts exported for it, and write its unit.
//!
//! Scans are fetched through [`crate::scans::Sources`]: the scan archive
//! first when configured, then LoC's batch route (BagIt `manifest-md5.txt`
//! and issue METS, never `www.loc.gov`'s item API) for the page's IIIF
//! JPEG and the storage service for its ALTO next to the JP2. When the
//! hand-off container's `scans/` holds the page (`--scans-dir`), it is read
//! from there and LoC is not asked.
//!
//! `texts.json` comes from `--texts-dir/<doc_id>.json`, written by the
//! hand-off job's `texts-export` step from the curated store (American
//! Stories' text and regions, our OCR where we have it); LoC's text is
//! added here from the ALTO, so every reader is in one file. An
//! `<doc_id>.ours.xml` next to it (our ALTO) is kept in the unit directory
//! as `ours.xml` for `collect`; it is never sent to hosts. `places.txt` is
//! the campaign's, the same file for every unit; the function words are
//! the page's language's, from `--function-words-dir/<lang>.txt`.
use super::{
    check_no_hold, units, PageSpec, ALTO_FILE, JOB_FILE, PAGE_FILE, PLACES_FILE, TEXTS_FILE,
};
use crate::{assess::SampleRow, atomic, read};
use anyhow::{ensure, Context};
use arrow_array::{Array, Int32Array, LargeStringArray, RecordBatch, StringArray};
use bytes::Bytes;
use chrono::Datelike;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    time::Duration,
};
use usnm_core::ids::PageKey;
use usnm_ocr_contract::{
    boinc::is_sha256,
    quality::{
        Engine, Input, Inputs, Job, PageSize, ReaderSpec, Region, TextReader, Texts, MAX_ALTO,
        MAX_PLACES, MAX_SCAN, MAX_TEXT, MAX_TEXTS, VERSION,
    },
};
use usnm_ocr_worker::loc::FetchError;

pub const MIN_GAP: Duration = Duration::from_secs(5);
/// Our ALTO in the unit directory (server side only).
pub const OURS_ALTO_FILE: &str = "ours.xml";
/// The sample row the page came from, when staged from a sample.
pub const SAMPLE_FILE: &str = "sample.json";
/// The LoC reader's name and source in `texts.json`.
pub const LOC_READER: &str = "loc";
pub const LOC_SOURCE: &str = "loc-alto";

#[derive(Debug, Clone)]
pub struct Options {
    /// The pages, as `group:lang:doc_id:batch[+batch...]`.
    pub pages: Vec<PageSpec>,
    /// Or a sample (`quality sample`): its rows become pages of `group`.
    pub sample: Option<PathBuf>,
    pub group: String,
    pub root: PathBuf,
    pub engine: Engine,
    /// The hand-off job's `texts/` export, one `<doc_id>.json` per page.
    pub texts_dir: PathBuf,
    /// The hand-off container's `scans/`, when the pages were fetched by
    /// the node: `<doc_id>.jpg` and `<doc_id>.xml`.
    pub scans_dir: Option<PathBuf>,
    /// The campaign's place names: a `.txt` (one per line) or the
    /// catalog's `places.json`.
    pub places: PathBuf,
    /// `<lang>.txt` per language, one word per line.
    pub function_words_dir: PathBuf,
    /// The readers every unit compares, in report order; a page whose
    /// texts lack one is staged without it.
    pub readers: Vec<String>,
    pub crop_seed: u64,
    pub k_crops: u32,
    pub gap: Duration,
    pub max_pages: usize,
    pub scan_archive: Option<String>,
    pub batch_cache: Option<PathBuf>,
}

#[derive(Debug, Default)]
pub struct Summary {
    pub staged: usize,
    pub missing: usize,
    pub already: usize,
    /// Pages whose export has no text but LoC's.
    pub loc_only: usize,
}

/// The pages to stage: `--page` specs, plus the sample's rows as pages of
/// the run's group, with the row kept for the unit's `sample.json`.
pub fn pages_of(opts: &Options) -> anyhow::Result<Vec<(PageSpec, Option<SampleRow>)>> {
    let mut out: Vec<(PageSpec, Option<SampleRow>)> =
        opts.pages.iter().cloned().map(|p| (p, None)).collect();
    if let Some(sample) = &opts.sample {
        crate::surya::check_word(&opts.group, "group")?;
        for row in crate::assess::read_sample(sample)? {
            crate::assess::check_doc_id(&row.doc_id)?;
            crate::surya::check_word(&row.lang, "language")?;
            out.push((
                PageSpec {
                    batches: vec![(row.batch.clone(), row.batch_version)],
                    doc_id: row.doc_id.clone(),
                    group: opts.group.clone(),
                    lang: row.lang.clone(),
                },
                Some(row),
            ));
        }
    }
    ensure!(!out.is_empty(), "no pages: give --page or --sample");
    Ok(out)
}

/// Stages up to `max_pages` units; stops at the first LoC refusal.
pub async fn run(opts: &Options) -> anyhow::Result<Summary> {
    ensure!(opts.gap >= MIN_GAP, "--gap-secs must be at least 5");
    let src = crate::scans::Sources::new(
        crate::scans::loc_client(opts.gap, opts.batch_cache.clone())?,
        crate::scans::archive(opts.scan_archive.as_deref())?,
        MAX_SCAN,
    );
    run_with(opts, &src).await
}

/// Stages up to `max_pages` units using `src`; stops at the first LoC refusal.
pub async fn run_with(opts: &Options, src: &crate::scans::Sources) -> anyhow::Result<Summary> {
    ensure!(
        opts.engine
            .image
            .strip_prefix("sha256:")
            .is_some_and(is_sha256),
        "--engine-image must be the worker image's full sha256 digest"
    );
    ensure!(!opts.readers.is_empty(), "--readers must name at least one");
    check_no_hold(&opts.root)?;
    let pages = pages_of(opts)?;
    let dir = units(&opts.root);
    std::fs::create_dir_all(&dir)?;
    let places = read_places(&opts.places)?;
    ensure!(places.len() <= MAX_PLACES, "places over size limit");
    let mut function_words: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut summary = Summary::default();
    for (p, row) in &pages {
        if summary.staged + summary.missing >= opts.max_pages {
            break;
        }
        crate::surya::check_word(&p.group, "group")?;
        crate::surya::check_word(&p.lang, "language")?;
        let unit = dir.join(&p.doc_id);
        if unit.join(JOB_FILE).exists() || unit.join("missing").exists() {
            summary.already += 1;
            continue;
        }
        if !function_words.contains_key(&p.lang) {
            let path = opts.function_words_dir.join(format!("{}.txt", p.lang));
            ensure!(
                path.exists(),
                "no function words for {}: {} (usnm-boinc quality function-words --lang {} --out-dir {})",
                p.lang,
                path.display(),
                p.lang,
                opts.function_words_dir.display()
            );
            function_words.insert(p.lang.clone(), read_words(&path)?);
        }
        std::fs::create_dir_all(&unit)?;
        if let Some(row) = row {
            atomic(&unit.join(SAMPLE_FILE), &serde_json::to_vec(row)?)?;
        }
        let fw = function_words[&p.lang].clone();
        match page(src, p, &unit, opts, &places, fw).await {
            Ok(Some(loc_only)) => {
                summary.staged += 1;
                if loc_only {
                    summary.loc_only += 1;
                }
            }
            Ok(None) => summary.missing += 1,
            Err(e) => {
                atomic(
                    &crate::assess::hold_path(&opts.root),
                    format!("{}: {e:#}\n", p.doc_id).as_bytes(),
                )?;
                return Err(e.context(format!("stopped at {}", p.doc_id)));
            }
        }
    }
    Ok(summary)
}

fn missing(unit: &Path, why: &str) -> anyhow::Result<Option<bool>> {
    atomic(&unit.join("missing"), format!("{why}\n").as_bytes())?;
    Ok(None)
}

/// The page's JPEG and ALTO with their sources.
struct Scan {
    page: Vec<u8>,
    page_source: String,
    alto: Vec<u8>,
    alto_source: String,
}

/// The ALTO's URL on the storage service for a page's IIIF base: the base
/// is `service:<path with : for />` under the IIIF root, and the ALTO sits
/// next to the JP2 at `<path>.xml`.
pub fn alto_url_of(iiif_base: &str) -> Option<String> {
    alto_url_of_roots(
        usnm_ocr_worker::loc::IIIF_ROOT,
        usnm_ocr_worker::loc::STORAGE_SERVICE_ROOT,
        iiif_base,
    )
}

pub fn alto_url_of_roots(iiif_root: &str, storage_root: &str, iiif_base: &str) -> Option<String> {
    let path = iiif_base
        .strip_prefix(iiif_root)?
        .strip_prefix("service:")?;
    if path.is_empty() || path.contains("..") {
        return None;
    }
    Some(format!(
        "{}{}.xml",
        storage_root.strip_suffix("ndnp/").unwrap_or(storage_root),
        path.replace(':', "/")
    ))
}

/// The hand-off container's copy of the page, when it has both files.
fn from_scans(scans: &Path, doc_id: &str) -> anyhow::Result<Option<Scan>> {
    let jpg = scans.join(format!("{doc_id}.jpg"));
    let xml = scans.join(format!("{doc_id}.xml"));
    if !jpg.exists() || !xml.exists() {
        return Ok(None);
    }
    Ok(Some(Scan {
        page: read(&jpg, MAX_SCAN)?,
        page_source: format!("handoff:scans/{doc_id}.jpg"),
        alto: read(&xml, MAX_ALTO)?,
        alto_source: format!("handoff:scans/{doc_id}.xml"),
    }))
}

/// The page from the scan archive or LoC's batch route. `Ok(Err(why))` is a
/// page LoC does not have.
async fn from_loc(
    src: &crate::scans::Sources,
    key: &PageKey,
    batches: &[(String, u16)],
) -> anyhow::Result<Result<Scan, String>> {
    let jpg = match src.jpeg(key, batches).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            return Ok(Err(
                "not in the scan archive, and none of its batches places it on tile.loc.gov".into(),
            ))
        }
        Err(FetchError::Permanent(m)) => return Ok(Err(m)),
        Err(e) => return Err(anyhow::anyhow!("{e}")),
    };
    ensure!(jpg.bytes.len() <= MAX_SCAN, "JPEG over size limit");
    let alto_url = alto_url_of_roots(&src.iiif_root, &src.storage_root, &jpg.iiif_base)
        .context("the IIIF base has no storage path")?;
    let alto = match src.loc.storage_file(&alto_url, &key.lccn, MAX_ALTO).await {
        Ok(Some(b)) => b,
        Ok(None) => return Ok(Err(format!("no ALTO at {alto_url}"))),
        Err(FetchError::Permanent(m)) => return Ok(Err(m)),
        Err(e) => return Err(anyhow::anyhow!("{e}")),
    };
    Ok(Ok(Scan {
        page: jpg.bytes.to_vec(),
        page_source: jpg.url,
        alto: alto.to_vec(),
        alto_source: alto_url,
    }))
}

/// `Some(loc_only)` when staged, `None` when LoC lacks the page.
async fn page(
    src: &crate::scans::Sources,
    p: &PageSpec,
    unit: &Path,
    opts: &Options,
    places: &str,
    function_words: Vec<String>,
) -> anyhow::Result<Option<bool>> {
    let key = PageKey::from_doc_id(&p.doc_id)?;
    let scan = match opts
        .scans_dir
        .as_deref()
        .map(|d| from_scans(d, &p.doc_id))
        .transpose()?
        .flatten()
    {
        Some(s) => s,
        None => match from_loc(src, &key, &p.batches).await? {
            Ok(s) => s,
            Err(why) => return missing(unit, &why),
        },
    };
    // The export, with LoC's text added from the ALTO.
    let export_path = opts.texts_dir.join(format!("{}.json", p.doc_id));
    let exported: Texts = if export_path.exists() {
        serde_json::from_slice(&read(&export_path, MAX_TEXTS)?)
            .with_context(|| format!("reading {}", export_path.display()))?
    } else {
        Texts {
            page: None,
            readers: vec![],
        }
    };
    let (width, height) = image::load_from_memory(&scan.page)
        .context("decoding page.jpg")?
        .to_luma8()
        .dimensions();
    let parsed = super::parse_alto(&scan.alto, f64::from(width), f64::from(height))?;
    let texts = join_texts(
        exported,
        &usnm_alto::text(&parsed),
        &opts.readers,
        width,
        height,
    );
    let loc_only = texts.readers.len() == 1;
    let ours = opts.texts_dir.join(format!("{}.ours.xml", p.doc_id));
    let job = write_unit(&WriteUnit {
        dir: unit,
        doc_id: &p.doc_id,
        group: &p.group,
        language: &p.lang,
        page: &scan.page,
        page_source: &scan.page_source,
        alto: &scan.alto,
        alto_source: &scan.alto_source,
        texts: &texts,
        texts_source: &format!("handoff:texts/{}.json", p.doc_id),
        places,
        places_source: "campaign:places.txt",
        engine: opts.engine.clone(),
        function_words,
        crop_seed: opts.crop_seed,
        k_crops: opts.k_crops,
    })?;
    if ours.exists() {
        atomic(&unit.join(OURS_ALTO_FILE), &read(&ours, MAX_ALTO)?)?;
    }
    debug_assert_eq!(job.doc_id, p.doc_id);
    Ok(Some(loc_only))
}

/// The unit's `texts.json`: LoC's text first, then the exported readers,
/// in `order`; readers not in `order`, and names in `order` the export
/// lacks, are left out. The page size is the export's (American Stories'
/// regions are in its scan's pixels) or the JPEG's.
pub fn join_texts(exported: Texts, loc_text: &str, order: &[String], w: u32, h: u32) -> Texts {
    let mut all = vec![TextReader {
        alto: false,
        name: LOC_READER.into(),
        regions: vec![],
        source: LOC_SOURCE.into(),
        text: loc_text.to_owned(),
    }];
    all.extend(exported.readers);
    let readers = order
        .iter()
        .filter_map(|name| all.iter().find(|r| &r.name == name).cloned())
        .collect();
    Texts {
        page: exported.page.or(Some(PageSize {
            height: h,
            width: w,
        })),
        readers,
    }
}

/// Everything a unit directory is written from.
pub struct WriteUnit<'a> {
    pub dir: &'a Path,
    pub doc_id: &'a str,
    pub group: &'a str,
    pub language: &'a str,
    pub page: &'a [u8],
    pub page_source: &'a str,
    pub alto: &'a [u8],
    pub alto_source: &'a str,
    pub texts: &'a Texts,
    pub texts_source: &'a str,
    pub places: &'a str,
    pub places_source: &'a str,
    pub engine: Engine,
    pub function_words: Vec<String>,
    pub crop_seed: u64,
    pub k_crops: u32,
}

/// Writes the five input files of a unit into `dir`, the job last, and
/// returns the job.
pub fn write_unit(u: &WriteUnit) -> anyhow::Result<Job> {
    ensure!(u.page.len() <= MAX_SCAN, "page over size limit");
    ensure!(u.alto.len() <= MAX_ALTO, "ALTO over size limit");
    ensure!(u.places.len() <= MAX_PLACES, "places over size limit");
    ensure!(
        !u.texts.readers.is_empty(),
        "a unit needs at least one text"
    );
    let texts_bytes = serde_json::to_vec_pretty(u.texts)?;
    ensure!(texts_bytes.len() <= MAX_TEXTS, "texts over size limit");
    std::fs::create_dir_all(u.dir)?;
    let readers: Vec<ReaderSpec> = u
        .texts
        .readers
        .iter()
        .map(|r| ReaderSpec {
            name: r.name.clone(),
            source: r.source.clone(),
        })
        .collect();
    let mut job = Job {
        v: VERSION,
        id: String::new(),
        crop_seed: u.crop_seed,
        doc_id: u.doc_id.to_owned(),
        engine: u.engine.clone(),
        function_words: u.function_words.clone(),
        group: u.group.to_owned(),
        inputs: Inputs {
            alto: Input::of(u.alto, u.alto_source),
            page: Input::of(u.page, u.page_source),
            places: Input::of(u.places.as_bytes(), u.places_source),
            texts: Input::of(&texts_bytes, u.texts_source),
        },
        k_crops: u.k_crops,
        language: u.language.to_owned(),
        readers,
    };
    job.id = job.identity()?;
    job.validate().context("staged job")?;
    u.texts.validate(&job)?;
    atomic(&u.dir.join(PAGE_FILE), u.page)?;
    atomic(&u.dir.join(ALTO_FILE), u.alto)?;
    atomic(&u.dir.join(TEXTS_FILE), &texts_bytes)?;
    atomic(&u.dir.join(PLACES_FILE), u.places.as_bytes())?;
    atomic(&u.dir.join(JOB_FILE), &serde_json::to_vec_pretty(&job)?)?;
    Ok(job)
}

/// A unit from local files, for tests and for checking a host or the
/// image without LoC or BOINC.
#[derive(Debug, Clone)]
pub struct LocalUnit<'a> {
    pub page: &'a [u8],
    pub alto: &'a [u8],
    pub texts: &'a Texts,
    pub places: &'a str,
    pub dir: &'a Path,
    pub engine: Engine,
    pub function_words: Vec<String>,
    pub crop_seed: u64,
    pub k_crops: u32,
}

/// Writes the five input files of a unit into `dir` and returns its job.
/// The sources are placeholders; `doc_id`, `group` and `language` are
/// `local`, `local` and `und`.
pub fn local_unit(u: &LocalUnit) -> anyhow::Result<Job> {
    write_unit(&WriteUnit {
        dir: u.dir,
        doc_id: "local",
        group: "local",
        language: "und",
        page: u.page,
        page_source: "local:page.jpg",
        alto: u.alto,
        alto_source: "local:loc.xml",
        texts: u.texts,
        texts_source: "local:texts.json",
        places: u.places,
        places_source: "local:places.txt",
        engine: u.engine.clone(),
        function_words: u.function_words.clone(),
        crop_seed: u.crop_seed,
        k_crops: u.k_crops,
    })
    .context("local unit")
}

/// Reads a text file of one word per line into the job's list, folded.
pub fn read_words(path: &Path) -> anyhow::Result<Vec<String>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {path:?}"))?;
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(usnm_ocr_quality::consensus::fold)
        .collect())
}

/// The campaign's `places.txt`: from a `.txt` as it is, or from the
/// catalog's `places.json` (`usnm_ingest::catalog::Place` rows): every
/// place's name and its titles' variant names, distinct, sorted, one per
/// line.
pub fn read_places(path: &Path) -> anyhow::Result<String> {
    let bytes = read(path, MAX_PLACES).with_context(|| format!("reading {}", path.display()))?;
    if path.extension().is_some_and(|e| e == "json") {
        let places: Vec<usnm_ingest::catalog::Place> =
            serde_json::from_slice(&bytes).context("places.json")?;
        Ok(places_txt(&places))
    } else {
        let text = String::from_utf8(bytes).context("places.txt is not UTF-8")?;
        ensure!(
            text.lines().any(|l| !l.trim().is_empty()),
            "places.txt is empty"
        );
        Ok(text)
    }
}

pub fn places_txt(places: &[usnm_ingest::catalog::Place]) -> String {
    let mut names: Vec<&str> = places
        .iter()
        .flat_map(|p| {
            std::iter::once(p.name.as_str()).chain(p.variants.iter().map(|v| v.name.as_str()))
        })
        .map(str::trim)
        .filter(|n| !n.is_empty() && !n.chars().any(char::is_control))
        .collect();
    names.sort_unstable();
    names.dedup();
    let mut out = String::new();
    for n in names {
        out.push_str(n);
        out.push('\n');
    }
    out
}

/// `<lang>.txt` for the campaign's languages: the first `n` words of the
/// language's embedded word list (wordfreq's `top_n_list`, the one
/// `usnm-ingest ocr-damage` scores with), as `ja-ocr/quality.py` takes
/// the 100 most frequent words for its function-word share. Languages
/// with no list (`usnm_core::lang::wordlist`) are an error.
pub fn write_function_words(langs: &[String], n: usize, out_dir: &Path) -> anyhow::Result<()> {
    ensure!(n > 0, "--top must be positive");
    std::fs::create_dir_all(out_dir)?;
    for lang in langs {
        crate::surya::check_word(lang, "language")?;
        let marc = usnm_core::lang::iso3_to_marc(lang)
            .with_context(|| format!("{lang} is not an ISO 639-3 code the catalog knows"))?;
        let code = usnm_core::lang::wordlist_code(marc)
            .with_context(|| format!("{lang} has no word list"))?;
        let list = usnm_core::lang::wordlist(code)
            .with_context(|| format!("{lang}: word list {code} is not embedded"))?;
        let mut text = format!("# {lang}: the {n} most frequent words of word list {code}\n");
        for w in list
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .take(n)
        {
            text.push_str(w);
            text.push('\n');
        }
        atomic(&out_dir.join(format!("{lang}.txt")), text.as_bytes())?;
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct TextsExportOptions {
    pub curated_url: String,
    pub out_dir: PathBuf,
    pub sample: Option<PathBuf>,
    pub doc_ids: Vec<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TextsExportSummary {
    pub pages: usize,
    pub written: usize,
    pub american_stories: usize,
    pub ocr_local: usize,
    pub altos: usize,
}

#[derive(serde::Deserialize)]
struct AsArticle {
    #[serde(default)]
    bboxes: Vec<Vec<Option<i64>>>,
    #[serde(default)]
    legibility: Vec<String>,
}

fn str_col<'a>(batch: &'a RecordBatch, name: &str, row: usize) -> Option<&'a str> {
    let col = batch.column_by_name(name)?;
    if col.is_null(row) {
        return None;
    }
    if let Some(s) = col.as_any().downcast_ref::<StringArray>() {
        Some(s.value(row))
    } else if let Some(s) = col.as_any().downcast_ref::<LargeStringArray>() {
        Some(s.value(row))
    } else {
        None
    }
}

fn int_col(batch: &RecordBatch, name: &str, row: usize) -> Option<i32> {
    let col = batch.column_by_name(name)?;
    if col.is_null(row) {
        return None;
    }
    col.as_any()
        .downcast_ref::<Int32Array>()
        .map(|a| a.value(row))
}

fn parse_as_regions(articles_json: &str) -> Vec<Region> {
    let Ok(articles) = serde_json::from_str::<Vec<AsArticle>>(articles_json) else {
        return Vec::new();
    };
    let mut regions = Vec::new();
    for a in articles {
        let legibility = ["illegible", "borderline", "legible"]
            .into_iter()
            .find(|k| a.legibility.iter().any(|l| l == k))
            .map(str::to_owned);
        for b in a.bboxes {
            if b.len() < 4 {
                continue;
            }
            let (Some(x0), Some(y0), Some(x1), Some(y1)) = (b[0], b[1], b[2], b[3]) else {
                continue;
            };
            let (Ok(x0), Ok(y0), Ok(x1), Ok(y1)) = (
                u32::try_from(x0),
                u32::try_from(y0),
                u32::try_from(x1),
                u32::try_from(y1),
            ) else {
                continue;
            };
            if x0 < x1 && y0 < y1 {
                regions.push(Region {
                    rect: [x0, y0, x1, y1],
                    kind: Some("article".into()),
                    legibility: legibility.clone(),
                });
            }
        }
    }
    regions
}

fn ours_reader_name(engine: &str, source: &str) -> String {
    let low = format!("{engine} {source}").to_ascii_lowercase();
    if low.contains("surya") {
        "ours-surya".into()
    } else if low.contains("tesseract") {
        "ours-tesseract".into()
    } else {
        let slug: String = engine
            .to_ascii_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let slug = slug.trim_matches('-');
        let name = if slug.is_empty() {
            "ours".to_owned()
        } else {
            format!("ours-{slug}")
        };
        name.chars()
            .take(32)
            .collect::<String>()
            .trim_end_matches('-')
            .to_owned()
    }
}

/// Export each requested page's non-LoC texts (American Stories and `ocr-local`)
/// and our ALTO from the curated store into `opts.out_dir`.
pub async fn texts_export(opts: &TextsExportOptions) -> anyhow::Result<TextsExportSummary> {
    let mut wanted: BTreeSet<String> = BTreeSet::new();
    for doc_id in &opts.doc_ids {
        crate::assess::check_doc_id(doc_id)?;
        wanted.insert(doc_id.clone());
    }
    if let Some(sample) = &opts.sample {
        for row in crate::assess::read_sample(sample)? {
            crate::assess::check_doc_id(&row.doc_id)?;
            wanted.insert(row.doc_id);
        }
    }
    ensure!(!wanted.is_empty(), "no pages: give --sample or --doc-id");
    let mut by_lccn_year: BTreeMap<String, BTreeSet<i32>> = BTreeMap::new();
    for doc_id in &wanted {
        let key = PageKey::from_doc_id(doc_id)?;
        by_lccn_year
            .entry(key.lccn)
            .or_default()
            .insert(key.date.year());
    }

    let curated = usnm_store::open(&opts.curated_url)?;
    std::fs::create_dir_all(&opts.out_dir)?;

    let mut as_by_doc: BTreeMap<String, (Option<PageSize>, TextReader)> = BTreeMap::new();
    for (lccn, years) in &by_lccn_year {
        let prefix = format!("{}/{lccn}", usnm_ingest::american_stories::PAGES);
        let mut paths = curated.list(&prefix).await?;
        paths.sort();
        let dir_prefix = format!("{prefix}/");
        for path in paths {
            let Some(file) = path
                .strip_prefix(&dir_prefix)
                .and_then(|s| s.strip_suffix(".parquet"))
            else {
                continue;
            };
            let Some((y_str, _)) = file.split_once('-') else {
                continue;
            };
            let Ok(year) = y_str.parse::<i32>() else {
                continue;
            };
            if !years.contains(&year) {
                continue;
            }
            let Some(bytes) = curated.get(&path).await? else {
                continue;
            };
            let reader = ParquetRecordBatchReaderBuilder::try_new(Bytes::from(bytes))
                .with_context(|| format!("reading {path}"))?
                .build()
                .with_context(|| format!("opening {path}"))?;
            for batch in reader {
                let b = batch.with_context(|| format!("decoding {path}"))?;
                for i in 0..b.num_rows() {
                    let Some(doc_id) = str_col(&b, "doc_id", i) else {
                        continue;
                    };
                    if !wanted.contains(doc_id) || as_by_doc.contains_key(doc_id) {
                        continue;
                    }
                    let Some(raw_text) = str_col(&b, "text", i) else {
                        continue;
                    };
                    let text = usnm_core::text::normalize_ocr(raw_text);
                    if text.is_empty() || text.len() > MAX_TEXT {
                        continue;
                    }
                    let regions = str_col(&b, "articles", i)
                        .filter(|s| !s.is_empty())
                        .map(parse_as_regions)
                        .unwrap_or_default();
                    let page_size = match (int_col(&b, "width", i), int_col(&b, "height", i)) {
                        (Some(w), Some(h)) if w > 0 && h > 0 => Some(PageSize {
                            width: w as u32,
                            height: h as u32,
                        }),
                        _ => None,
                    };
                    as_by_doc.insert(
                        doc_id.to_owned(),
                        (
                            page_size,
                            TextReader {
                                alto: false,
                                name: "as".into(),
                                regions,
                                source: "american-stories".into(),
                                text,
                            },
                        ),
                    );
                }
            }
        }
    }

    let revoked = usnm_ingest::ocr_local::replace::revocations(curated.as_ref()).await?;
    let listing = usnm_ingest::ocr_local::replace::Listing::list(curated.as_ref()).await?;
    let loaded = listing
        .load(
            curated.as_ref(),
            by_lccn_year.keys().map(String::as_str),
            &revoked,
        )
        .await?;
    let mut ocr_candidates: BTreeMap<String, usnm_ingest::ocr_local::replace::Candidate> =
        BTreeMap::new();
    for (doc_id, mut cand) in loaded.pages {
        if !wanted.contains(&doc_id) {
            continue;
        }
        cand.text = usnm_core::text::normalize_ocr(&cand.text);
        if !cand.text.is_empty() && cand.text.len() <= MAX_TEXT {
            ocr_candidates.insert(doc_id, cand);
        }
    }

    let mut altos_by_doc: BTreeMap<String, Vec<String>> = BTreeMap::new();
    if !ocr_candidates.is_empty() {
        let alto_root = format!("{}/alto", usnm_ingest::ocr_local::ROOT);
        let alto_prefix = format!("{alto_root}/");
        let mut alto_paths = curated.list(&alto_root).await?;
        alto_paths.sort();
        for path in alto_paths {
            let Some(rest) = path.strip_prefix(&alto_prefix) else {
                continue;
            };
            if !rest.ends_with(".xml") {
                continue;
            }
            let Some((doc_id, _)) = rest.split_once('.') else {
                continue;
            };
            if ocr_candidates.contains_key(doc_id) {
                altos_by_doc
                    .entry(doc_id.to_owned())
                    .or_default()
                    .push(path);
            }
        }
    }

    let mut summary = TextsExportSummary {
        pages: wanted.len(),
        ..TextsExportSummary::default()
    };
    let mut ours_by_doc: BTreeMap<String, TextReader> = BTreeMap::new();
    for (doc_id, cand) in ocr_candidates {
        let mut has_alto = false;
        if let Some(paths) = altos_by_doc.get(&doc_id) {
            let node_needle = format!(
                "{}/alto/{doc_id}.{}.",
                usnm_ingest::ocr_local::ROOT,
                cand.row.node
            );
            let chosen = paths
                .iter()
                .rev()
                .find(|p| p.starts_with(&node_needle))
                .or_else(|| paths.last());
            if let Some(alto_path) = chosen {
                if let Some(bytes) = curated.get(alto_path).await? {
                    if !bytes.is_empty() && bytes.len() <= MAX_ALTO {
                        atomic(&opts.out_dir.join(format!("{doc_id}.ours.xml")), &bytes)?;
                        has_alto = true;
                        summary.altos += 1;
                    }
                }
            }
        }
        let name = ours_reader_name(&cand.row.ocr_engine, &cand.row.ocr_source);
        let engine = cand
            .row
            .ocr_engine
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(";");
        let source = format!("ocr-local:{engine}");
        ours_by_doc.insert(
            doc_id,
            TextReader {
                alto: has_alto,
                name,
                regions: vec![],
                source,
                text: cand.text,
            },
        );
    }

    for doc_id in &wanted {
        let mut page = None;
        let mut readers = Vec::new();
        if let Some((sz, r)) = as_by_doc.remove(doc_id) {
            page = sz;
            readers.push(r);
            summary.american_stories += 1;
        }
        if let Some(r) = ours_by_doc.remove(doc_id) {
            readers.push(r);
            summary.ocr_local += 1;
        }
        if !readers.is_empty() {
            let texts = Texts { page, readers };
            atomic(
                &opts.out_dir.join(format!("{doc_id}.json")),
                &serde_json::to_vec_pretty(&texts)?,
            )?;
            summary.written += 1;
        }
    }
    Ok(summary)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn alto_url_follows_the_iiif_base() {
        let base = "https://tile.loc.gov/image-services/iiif/service:ndnp:dlc:batch_dlc_abc_ver01:data:sn83045462:00280654021:1890050301:0001";
        assert_eq!(
            alto_url_of(base).unwrap(),
            "https://tile.loc.gov/storage-services/service/ndnp/dlc/batch_dlc_abc_ver01/data/sn83045462/00280654021/1890050301/0001.xml"
        );
        assert_eq!(
            alto_url_of_roots(
                "http://example.test/iiif/",
                "http://example.test/storage-services/service/ndnp/",
                "http://example.test/iiif/service:ndnp:xx:batch_xx_test_ver01:data:sn83045462:001:1890050301:0001"
            )
            .unwrap(),
            "http://example.test/storage-services/service/ndnp/xx/batch_xx_test_ver01/data/sn83045462/001/1890050301/0001.xml"
        );
        assert!(alto_url_of("https://example.org/x").is_none());
    }

    #[test]
    fn places_from_the_catalog_are_names_and_variants() {
        let places: Vec<usnm_ingest::catalog::Place> = serde_json::from_str(
            r#"[{"id":"a","ordinal":1,"name":"Troy","state":"NY","lat":1.0,"lon":2.0,"precision":"city",
                 "variants":[{"name":"Troy City","rule":"x_city"}]},
                {"id":"b","ordinal":2,"name":"Albany","state":"NY","lat":1.0,"lon":2.0,"precision":"city"}]"#,
        )
        .unwrap();
        assert_eq!(places_txt(&places), "Albany\nTroy\nTroy City\n");
    }

    #[test]
    fn function_words_come_from_the_embedded_lists() {
        let tmp = tempfile::tempdir().unwrap();
        write_function_words(&["eng".into(), "deu".into()], 100, tmp.path()).unwrap();
        let eng = read_words(&tmp.path().join("eng.txt")).unwrap();
        assert_eq!(eng.len(), 100);
        assert!(eng.contains(&"the".to_string()));
        let deu = read_words(&tmp.path().join("deu.txt")).unwrap();
        assert!(deu.contains(&"und".to_string()));
        assert!(write_function_words(&["xxx".into()], 100, tmp.path()).is_err());
    }

    #[test]
    fn joined_texts_put_loc_first_and_follow_the_order() {
        let exported = Texts {
            page: Some(PageSize {
                height: 20,
                width: 10,
            }),
            readers: vec![
                TextReader {
                    alto: true,
                    name: "ours-tesseract".into(),
                    regions: vec![],
                    source: "ocr-local:usnm-tesseract".into(),
                    text: "b".into(),
                },
                TextReader {
                    alto: false,
                    name: "as".into(),
                    regions: vec![],
                    source: "american-stories".into(),
                    text: "a".into(),
                },
            ],
        };
        let t = join_texts(
            exported,
            "l",
            &["loc".into(), "as".into(), "missing".into()],
            1,
            2,
        );
        let names: Vec<&str> = t.readers.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["loc", "as"]);
        assert_eq!(t.page.unwrap().width, 10);
        let t = join_texts(
            Texts {
                page: None,
                readers: vec![],
            },
            "l",
            &["loc".into()],
            1,
            2,
        );
        assert_eq!(t.page.unwrap().height, 2);
    }

    /// Stages one page from a hand-off `scans/` copy, without LoC.
    #[tokio::test]
    async fn stages_from_the_handoff_scans() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = tmp.path().join("fixture");
        let j = crate::quality::work::tests::unit(&fixture);
        let doc_id = "sn83045462_1890-05-03_ed-1_seq-1";
        let scans = tmp.path().join("scans");
        std::fs::create_dir_all(&scans).unwrap();
        std::fs::copy(fixture.join(PAGE_FILE), scans.join(format!("{doc_id}.jpg"))).unwrap();
        std::fs::copy(fixture.join(ALTO_FILE), scans.join(format!("{doc_id}.xml"))).unwrap();
        let texts_dir = tmp.path().join("texts");
        std::fs::create_dir_all(&texts_dir).unwrap();
        let fixture_texts: Texts =
            serde_json::from_slice(&std::fs::read(fixture.join(TEXTS_FILE)).unwrap()).unwrap();
        let export = Texts {
            page: fixture_texts.page,
            readers: fixture_texts
                .readers
                .iter()
                .filter(|r| r.name != "loc")
                .cloned()
                .collect(),
        };
        std::fs::write(
            texts_dir.join(format!("{doc_id}.json")),
            serde_json::to_vec(&export).unwrap(),
        )
        .unwrap();
        std::fs::write(
            texts_dir.join(format!("{doc_id}.ours.xml")),
            std::fs::read(fixture.join(ALTO_FILE)).unwrap(),
        )
        .unwrap();
        let fw = tmp.path().join("fw");
        write_function_words(&["eng".into()], 100, &fw).unwrap();
        std::fs::write(tmp.path().join("places.txt"), "Albany\n").unwrap();
        let sample = tmp.path().join("sample.jsonl");
        let row = SampleRow {
            batch: "dlc_abc".into(),
            batch_version: 1,
            crash_weight: 1.0,
            damage: "low".into(),
            damage_rate: 0.01,
            decade: 1890,
            doc_id: doc_id.into(),
            lang: "eng".into(),
            mode: usnm_ocr_contract::assess::Mode::Ocr,
            prefix: "dlc".into(),
            stratum_pages: 10,
            weight: 10.0,
            year: 1890,
        };
        std::fs::write(
            &sample,
            format!("{}\n", serde_json::to_string(&row).unwrap()),
        )
        .unwrap();
        let opts = Options {
            pages: vec![],
            sample: Some(sample),
            group: "probe".into(),
            root: tmp.path().join("root"),
            engine: j.engine.clone(),
            texts_dir,
            scans_dir: Some(scans),
            places: tmp.path().join("places.txt"),
            function_words_dir: fw,
            readers: vec!["loc".into(), "as".into(), "ours-tesseract".into()],
            crop_seed: 522,
            k_crops: 12,
            gap: MIN_GAP,
            max_pages: 10,
            scan_archive: None,
            batch_cache: None,
        };
        let s = run(&opts).await.unwrap();
        assert_eq!((s.staged, s.missing, s.already, s.loc_only), (1, 0, 0, 0));
        let unit = units(&opts.root).join(doc_id);
        let job: Job =
            serde_json::from_slice(&std::fs::read(unit.join(JOB_FILE)).unwrap()).unwrap();
        job.validate().unwrap();
        assert_eq!(job.doc_id, doc_id);
        assert_eq!(job.group, "probe");
        assert_eq!(job.language, "eng");
        assert_eq!(job.readers.len(), 3);
        assert_eq!(job.readers[0].name, "loc");
        assert_eq!(job.function_words.len(), 100);
        assert_eq!(
            job.inputs.page.source,
            format!("handoff:scans/{doc_id}.jpg")
        );
        assert!(unit.join(OURS_ALTO_FILE).exists());
        assert!(unit.join(SAMPLE_FILE).exists());
        // The unit runs as the worker would.
        let l = crate::quality::work::prepare(&unit).unwrap();
        assert_eq!(
            l.texts.readers[0].text.replace('\n', " "),
            crate::quality::work::tests::TRUTH
        );
        // A second run stages nothing new.
        let s = run(&opts).await.unwrap();
        assert_eq!((s.staged, s.already), (0, 1));
        // A page the scans lack would go to LoC; a language without a list stops first.
        let bad = Options {
            pages: vec!["probe:xxx:sn83045462_1890-05-04_ed-1_seq-1:dlc_abc_ver01"
                .parse()
                .unwrap()],
            sample: None,
            ..opts.clone()
        };
        assert!(run(&bad)
            .await
            .unwrap_err()
            .to_string()
            .contains("function words"));
    }

    #[tokio::test]
    async fn stages_from_batch_route_without_item_api() {
        use usnm_ocr_worker::{
            loc::{iiif_base, image_url, ImageSize, LocClient, LocConfig},
            standin::{Page as SPage, StandIn},
        };

        let tmp = tempfile::tempdir().unwrap();
        let fixture = tmp.path().join("fixture");
        let j = crate::quality::work::tests::unit(&fixture);
        let page_bytes = std::fs::read(fixture.join(PAGE_FILE)).unwrap();
        let alto_bytes = std::fs::read(fixture.join(ALTO_FILE)).unwrap();
        let doc_id = "sn83045462_1890-05-03_ed-1_seq-1";

        let s = StandIn::start().await;
        let batch_path = "/storage-services/service/ndnp/xx/batch_xx_test_ver01/";
        s.add_issue(
            batch_path,
            "sn83045462",
            "001",
            "1890050301",
            &[SPage {
                seq: 1,
                frame: "0042",
                jp2: b"jp2",
            }],
        );
        let jp2_url = format!(
            "{}{batch_path}data/sn83045462/001/1890050301/0042.jp2",
            s.root
        );
        let base = iiif_base(&s.iiif_root(), &jp2_url).unwrap();
        let jpg_url = image_url(&base, ImageSize::Full);
        s.put(jpg_url.strip_prefix(&s.root).unwrap(), page_bytes);
        s.put(
            &format!("{batch_path}data/sn83045462/001/1890050301/0042.xml"),
            alto_bytes,
        );

        let loc = LocClient::new(LocConfig {
            api_gap: Duration::from_millis(1),
            tile_gap: Duration::from_millis(1),
            storage_gap: Duration::from_millis(1),
            attempts: 1,
            backoff: Duration::from_millis(1),
            api_root: s.root.clone(),
            iiif_root: s.iiif_root(),
            storage_root: s.storage_root(),
            batches_root: s.batches_root(),
            item_probes: false,
            html_is_block: true,
            ..LocConfig::default()
        })
        .unwrap();
        let src = crate::scans::Sources {
            loc,
            archive: None,
            iiif_root: s.iiif_root(),
            storage_root: s.storage_root(),
            max: MAX_SCAN,
        };

        let texts_dir = tmp.path().join("texts");
        std::fs::create_dir_all(&texts_dir).unwrap();
        let fw = tmp.path().join("fw");
        write_function_words(&["eng".into()], 100, &fw).unwrap();
        std::fs::write(tmp.path().join("places.txt"), "Albany\n").unwrap();

        let opts = Options {
            pages: vec![format!("smoke:eng:{doc_id}:xx_test_ver01").parse().unwrap()],
            sample: None,
            group: "smoke".into(),
            root: tmp.path().join("root"),
            engine: j.engine.clone(),
            texts_dir,
            scans_dir: None,
            places: tmp.path().join("places.txt"),
            function_words_dir: fw,
            readers: vec!["loc".into()],
            crop_seed: 522,
            k_crops: 12,
            gap: MIN_GAP,
            max_pages: 10,
            scan_archive: None,
            batch_cache: None,
        };

        let sum = run_with(&opts, &src).await.unwrap();
        assert_eq!(
            (sum.staged, sum.missing, sum.already, sum.loc_only),
            (1, 0, 0, 1)
        );
        assert_eq!(s.hits("/item/"), 0);
        assert_eq!(s.hits("manifest-md5.txt"), 1);
        assert_eq!(s.hits("1890050301.xml"), 1);
        assert_eq!(s.hits("default.jpg"), 1);
        assert_eq!(s.hits("0042.xml"), 1);
        let unit = units(&opts.root).join(doc_id);
        let job: Job =
            serde_json::from_slice(&std::fs::read(unit.join(JOB_FILE)).unwrap()).unwrap();
        job.validate().unwrap();
    }

    #[tokio::test]
    async fn exports_american_stories_and_ocr_local_texts_from_curated() {
        use arrow_array::builder::{Int32Builder, LargeStringBuilder, StringBuilder};
        use arrow_schema::{DataType, Field, Schema};
        use parquet::arrow::ArrowWriter;
        use std::sync::Arc;
        use usnm_ingest::ocr_local::pages::{encode as encode_ocr_local, Imported, Scores};
        use usnm_overlay::OverlayRow;

        let tmp = tempfile::tempdir().unwrap();
        let curated_dir = tmp.path().join("curated");
        let curated = usnm_store::open(curated_dir.to_str().unwrap()).unwrap();
        let doc_id = "sn83045462_1890-05-03_ed-1_seq-1";

        let as_schema = Arc::new(Schema::new(vec![
            Field::new("doc_id", DataType::Utf8, false),
            Field::new("text", DataType::LargeUtf8, false),
            Field::new("articles", DataType::LargeUtf8, true),
            Field::new("width", DataType::Int32, true),
            Field::new("height", DataType::Int32, true),
        ]));
        let mut b_doc = StringBuilder::new();
        let mut b_txt = LargeStringBuilder::new();
        let mut b_art = LargeStringBuilder::new();
        let mut b_w = Int32Builder::new();
        let mut b_h = Int32Builder::new();
        b_doc.append_value(doc_id);
        b_txt.append_value("American Stories text");
        b_art.append_value(r#"[{"bboxes":[[10,20,300,400]],"legibility":["legible","unknown"]}]"#);
        b_w.append_value(1000);
        b_h.append_value(2000);
        let batch = RecordBatch::try_new(
            as_schema.clone(),
            vec![
                Arc::new(b_doc.finish()),
                Arc::new(b_txt.finish()),
                Arc::new(b_art.finish()),
                Arc::new(b_w.finish()),
                Arc::new(b_h.finish()),
            ],
        )
        .unwrap();
        let mut as_parquet = Vec::new();
        let mut w = ArrowWriter::try_new(&mut as_parquet, as_schema, None).unwrap();
        w.write(&batch).unwrap();
        w.close().unwrap();
        curated
            .put(
                "american-stories/pages/sn83045462/1890-000.parquet",
                as_parquet,
                "application/vnd.apache.parquet",
            )
            .await
            .unwrap();

        let imported = Imported {
            row: OverlayRow {
                key: PageKey::from_doc_id(doc_id).unwrap(),
                batch: "dlc_abc".into(),
                ocr_source: "usnm-tesseract".into(),
                ocr_engine: "tesseract/5.5.1 profile=eng".into(),
                loc_text: "bulk".into(),
                text: "Our Tesseract text".into(),
                image_url: "https://example.org/p".into(),
                ocred_at: chrono::DateTime::from_timestamp(1, 0).unwrap(),
            },
            batch_version: 1,
            source_part: "ocr-out/pages/node1/part.parquet".into(),
            node: "node1".into(),
            reason: "bulk".into(),
            ours: Scores {
                lang: Some("eng".into()),
                decision: Some("ok".into()),
                damage_rate: Some(0.01),
                function_share: Some(0.35),
                head: Some(100),
            },
            loc: Scores {
                lang: Some("eng".into()),
                decision: Some("damaged".into()),
                damage_rate: Some(0.25),
                function_share: Some(0.20),
                head: Some(80),
            },
        };
        let ocr_parquet = encode_ocr_local(&[imported]).unwrap();
        curated
            .put(
                "ocr-local/pages/sn83045462/2026-04-01T00-00-00Z-a1.parquet",
                ocr_parquet,
                "application/vnd.apache.parquet",
            )
            .await
            .unwrap();
        let alto_xml = b"<alto>ours</alto>";
        curated
            .put(
                &format!("ocr-local/alto/{doc_id}.node1.20260401T000000Z.xml"),
                alto_xml.to_vec(),
                "application/xml",
            )
            .await
            .unwrap();

        let out_dir = tmp.path().join("texts");
        let sum = texts_export(&TextsExportOptions {
            curated_url: curated_dir.to_str().unwrap().into(),
            out_dir: out_dir.clone(),
            sample: None,
            doc_ids: vec![doc_id.into()],
        })
        .await
        .unwrap();
        assert_eq!(
            sum,
            TextsExportSummary {
                pages: 1,
                written: 1,
                american_stories: 1,
                ocr_local: 1,
                altos: 1,
            }
        );

        let exported: Texts =
            serde_json::from_slice(&std::fs::read(out_dir.join(format!("{doc_id}.json"))).unwrap())
                .unwrap();
        assert_eq!(
            exported.page,
            Some(PageSize {
                width: 1000,
                height: 2000
            })
        );
        assert_eq!(exported.readers.len(), 2);
        assert_eq!(exported.readers[0].name, "as");
        assert_eq!(exported.readers[0].source, "american-stories");
        assert!(!exported.readers[0].alto);
        assert_eq!(exported.readers[0].text, "American Stories text");
        assert_eq!(
            exported.readers[0].regions,
            vec![Region {
                rect: [10, 20, 300, 400],
                kind: Some("article".into()),
                legibility: Some("legible".into()),
            }]
        );
        assert_eq!(exported.readers[1].name, "ours-tesseract");
        assert_eq!(
            exported.readers[1].source,
            "ocr-local:tesseract/5.5.1;profile=eng"
        );
        assert!(exported.readers[1].alto);
        assert_eq!(exported.readers[1].text, "Our Tesseract text");
        assert_eq!(
            std::fs::read(out_dir.join(format!("{doc_id}.ours.xml"))).unwrap(),
            alto_xml
        );
    }
}
