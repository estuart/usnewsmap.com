//! Write one `usnm_qwen` unit per page: the page JPEG and the job with the
//! crops, cut from LoC's ALTO with the campaign's seed.
//!
//! The pages come from the `usnm_quality` staging root (`--quality-root`),
//! which already holds `page.jpg` and `loc.xml` for each, so LoC is not
//! asked again; the crop seed and count are read from each page's
//! `quality.json`, so both apps cut the same crops.
use super::{check_no_hold, units, JOB_FILE, PAGE_FILE};
use crate::{atomic, read};
use anyhow::{ensure, Context};
use image::GenericImageView as _;
use std::path::{Path, PathBuf};
use usnm_ocr_contract::{
    quality::{Input, Job as QualityJob, MAX_ALTO, MAX_JOB_JSON as MAX_QUALITY_JOB, MAX_SCAN},
    qwen::{Crop, Engine, Job, PROMPT_VERSION, VERSION},
};
use usnm_ocr_quality::crops;

#[derive(Debug, Clone)]
pub struct Options {
    /// The `usnm_quality` staging root whose units to cut crops from.
    pub quality_root: PathBuf,
    pub root: PathBuf,
    pub engine: Engine,
    pub gpu_timeout_secs: u32,
    pub cpu_timeout_secs: u32,
    pub max_tokens_per_1000px2: u32,
    pub max_pages: usize,
}

#[derive(Debug, Default)]
pub struct Summary {
    pub staged: usize,
    pub no_crops: usize,
    pub already: usize,
}

/// Writes one unit per staged `usnm_quality` page (those with a
/// `quality.json`), in doc id order, up to `max_pages` new ones. A page
/// whose ALTO has no line the recipe can cut gets a `no_crops` marker and
/// no unit. The quality unit's `sample.json`, when there, is copied along
/// for `collect`.
pub async fn run(opts: &Options) -> anyhow::Result<Summary> {
    super::check_digest(&opts.engine.image, "--engine-image")?;
    super::check_digest(&opts.engine.llama, "--engine-llama")?;
    super::check_digest(&opts.engine.weights, "--engine-weights")?;
    check_no_hold(&opts.root)?;
    let dir = units(&opts.root);
    std::fs::create_dir_all(&dir)?;
    let src = crate::quality::units(&opts.quality_root);
    let mut pages: Vec<PathBuf> = std::fs::read_dir(&src)
        .with_context(|| format!("reading {}", src.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.join(crate::quality::JOB_FILE).exists())
        .collect();
    pages.sort();
    let mut summary = Summary::default();
    for page in pages {
        if summary.staged + summary.no_crops >= opts.max_pages {
            break;
        }
        let name = page
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let unit = dir.join(&name);
        if unit.join(JOB_FILE).exists() || unit.join("no_crops").exists() {
            summary.already += 1;
            continue;
        }
        let quality: QualityJob = serde_json::from_slice(&read(
            &page.join(crate::quality::JOB_FILE),
            MAX_QUALITY_JOB,
        )?)
        .with_context(|| format!("{name}: quality.json"))?;
        quality.validate()?;
        let bytes = read(&page.join(crate::quality::PAGE_FILE), MAX_SCAN)?;
        ensure!(
            quality.inputs.page.matches(&bytes),
            "{name}: staged page changed"
        );
        let alto = read(&page.join(crate::quality::ALTO_FILE), MAX_ALTO)?;
        ensure!(
            quality.inputs.alto.matches(&alto),
            "{name}: staged ALTO changed"
        );
        let crops = cut(&bytes, &alto, quality.crop_seed, quality.k_crops)
            .with_context(|| format!("{name}: cutting crops"))?;
        std::fs::create_dir_all(&unit)?;
        let sample = page.join(crate::quality::stage::SAMPLE_FILE);
        if sample.exists() {
            atomic(
                &unit.join(crate::quality::stage::SAMPLE_FILE),
                &std::fs::read(&sample)?,
            )?;
        }
        if crops.is_empty() {
            atomic(
                &unit.join("no_crops"),
                b"the ALTO has no line the recipe can cut\n",
            )?;
            summary.no_crops += 1;
            continue;
        }
        atomic(&unit.join(PAGE_FILE), &bytes)?;
        let mut job = Job {
            v: VERSION,
            id: String::new(),
            cpu_timeout_secs: opts.cpu_timeout_secs,
            crops,
            doc_id: quality.doc_id.clone(),
            engine: opts.engine.clone(),
            gpu_timeout_secs: opts.gpu_timeout_secs,
            group: quality.group.clone(),
            input: quality.inputs.page.clone(),
            language: quality.language.clone(),
            max_tokens_per_1000px2: opts.max_tokens_per_1000px2,
            prompt_version: PROMPT_VERSION,
        };
        job.id = job.identity()?;
        job.validate()
            .with_context(|| format!("{name}: staged job"))?;
        atomic(&unit.join(JOB_FILE), &serde_json::to_vec_pretty(&job)?)?;
        summary.staged += 1;
    }
    Ok(summary)
}

/// The crops of a page: LoC's ALTO in the image's pixels, the recipe's
/// selection, LoC's text in each box.
pub fn cut(page: &[u8], alto: &[u8], seed: u64, k: u32) -> anyhow::Result<Vec<Crop>> {
    let (w, h) = image::load_from_memory(page)
        .context("decoding page.jpg")?
        .dimensions();
    let parsed = crate::quality::parse_alto(alto, f64::from(w), f64::from(h))?;
    Ok(crops::select(&parsed, seed, k as usize)
        .into_iter()
        .map(|c| Crop {
            rect: c.rect,
            id: c.id,
            loc_text: crops::text_in_box(&parsed, c.rect),
            stratum: c.stratum,
        })
        .collect())
}

/// Writes a unit from a local page and ALTO, for checking a host or the
/// image without LoC or BOINC: `dir` gets `page.jpg` and `qwen.json`, and
/// `usnm-boinc work --dir <dir>` then runs it.
#[allow(clippy::too_many_arguments)]
pub fn local_unit(
    image: &Path,
    alto: &Path,
    dir: &Path,
    engine: Engine,
    seed: u64,
    k: u32,
    timeout_secs: u32,
    max_tokens_per_1000px2: u32,
) -> anyhow::Result<Job> {
    let bytes = read(image, MAX_SCAN)?;
    let alto = read(alto, MAX_ALTO)?;
    let crops = cut(&bytes, &alto, seed, k)?;
    ensure!(!crops.is_empty(), "the ALTO has no line the recipe can cut");
    std::fs::create_dir_all(dir)?;
    atomic(&dir.join(PAGE_FILE), &bytes)?;
    let mut job = Job {
        v: VERSION,
        id: String::new(),
        cpu_timeout_secs: timeout_secs,
        crops,
        doc_id: "local".into(),
        engine,
        gpu_timeout_secs: timeout_secs,
        group: "local".into(),
        input: Input::of(&bytes, "local:page.jpg"),
        language: "und".into(),
        max_tokens_per_1000px2,
        prompt_version: PROMPT_VERSION,
    };
    job.id = job.identity()?;
    job.validate()?;
    atomic(&dir.join(JOB_FILE), &serde_json::to_vec_pretty(&job)?)?;
    Ok(job)
}

/// The synthetic check page: two lines of block capitals in a 5 by 7
/// pixel font scaled up, with an ALTO naming each word's box, so the
/// image's build check can read a line with nothing but what the image
/// holds (no font, no Python). Returns the JPEG and the ALTO.
pub fn synthetic_page() -> (Vec<u8>, String) {
    const SCALE: u32 = 8;
    const ADVANCE: u32 = 6 * SCALE;
    const LINES: [&str; 2] = ["HELLO NEWSPAPER WORLD NOW", "QUICK BROWN FOX JUMPS"];
    let (width, height) = (1400u32, 1400u32);
    let mut img = image::GrayImage::from_pixel(width, height, image::Luma([245]));
    let mut blocks = Vec::new();
    for (li, line) in LINES.iter().enumerate() {
        let y = 100 + li as u32 * 200;
        let mut words = Vec::new();
        let mut x = 60u32;
        for word in line.split(' ') {
            let x0 = x;
            for ch in word.chars() {
                for (r, row) in glyph(ch).iter().enumerate() {
                    for (c, cell) in row.bytes().enumerate() {
                        if cell == b'#' {
                            for dy in 0..SCALE {
                                for dx in 0..SCALE {
                                    img.put_pixel(
                                        x + c as u32 * SCALE + dx,
                                        y + r as u32 * SCALE + dy,
                                        image::Luma([20]),
                                    );
                                }
                            }
                        }
                    }
                }
                x += ADVANCE;
            }
            words.push((x0, y, x - SCALE, y + 7 * SCALE, word, 0.9));
            x += ADVANCE;
        }
        blocks.push(words);
    }
    let mut jpeg = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageLuma8(img)
        .write_to(&mut jpeg, image::ImageFormat::Jpeg)
        .expect("encoding the synthetic page");
    (
        jpeg.into_inner(),
        crate::quality::alto_xml(width, height, &[blocks]),
    )
}

/// A 5 by 7 capital, rows top to bottom; anything else is blank.
fn glyph(ch: char) -> [&'static str; 7] {
    match ch {
        'A' => [
            " ### ", "#   #", "#   #", "#####", "#   #", "#   #", "#   #",
        ],
        'B' => [
            "#### ", "#   #", "#   #", "#### ", "#   #", "#   #", "#### ",
        ],
        'C' => [
            " ####", "#    ", "#    ", "#    ", "#    ", "#    ", " ####",
        ],
        'D' => [
            "#### ", "#   #", "#   #", "#   #", "#   #", "#   #", "#### ",
        ],
        'E' => [
            "#####", "#    ", "#    ", "#### ", "#    ", "#    ", "#####",
        ],
        'F' => [
            "#####", "#    ", "#    ", "#### ", "#    ", "#    ", "#    ",
        ],
        'G' => [
            " ####", "#    ", "#    ", "# ###", "#   #", "#   #", " ####",
        ],
        'H' => [
            "#   #", "#   #", "#   #", "#####", "#   #", "#   #", "#   #",
        ],
        'I' => [
            "#####", "  #  ", "  #  ", "  #  ", "  #  ", "  #  ", "#####",
        ],
        'J' => [
            "    #", "    #", "    #", "    #", "#   #", "#   #", " ### ",
        ],
        'K' => [
            "#   #", "#  # ", "# #  ", "##   ", "# #  ", "#  # ", "#   #",
        ],
        'L' => [
            "#    ", "#    ", "#    ", "#    ", "#    ", "#    ", "#####",
        ],
        'M' => [
            "#   #", "## ##", "# # #", "# # #", "#   #", "#   #", "#   #",
        ],
        'N' => [
            "#   #", "##  #", "# # #", "#  ##", "#   #", "#   #", "#   #",
        ],
        'O' => [
            " ### ", "#   #", "#   #", "#   #", "#   #", "#   #", " ### ",
        ],
        'P' => [
            "#### ", "#   #", "#   #", "#### ", "#    ", "#    ", "#    ",
        ],
        'Q' => [
            " ### ", "#   #", "#   #", "#   #", "# # #", "#  # ", " ## #",
        ],
        'R' => [
            "#### ", "#   #", "#   #", "#### ", "# #  ", "#  # ", "#   #",
        ],
        'S' => [
            " ####", "#    ", "#    ", " ### ", "    #", "    #", "#### ",
        ],
        'T' => [
            "#####", "  #  ", "  #  ", "  #  ", "  #  ", "  #  ", "  #  ",
        ],
        'U' => [
            "#   #", "#   #", "#   #", "#   #", "#   #", "#   #", " ### ",
        ],
        'V' => [
            "#   #", "#   #", "#   #", "#   #", " # # ", " # # ", "  #  ",
        ],
        'W' => [
            "#   #", "#   #", "#   #", "# # #", "# # #", "## ##", "#   #",
        ],
        'X' => [
            "#   #", "#   #", " # # ", "  #  ", " # # ", "#   #", "#   #",
        ],
        'Y' => [
            "#   #", "#   #", " # # ", "  #  ", "  #  ", "  #  ", "  #  ",
        ],
        'Z' => [
            "#####", "    #", "   # ", "  #  ", " #   ", "#    ", "#####",
        ],
        _ => ["     "; 7],
    }
}

/// Writes a unit from the synthetic page, for the image's build check.
pub fn synthetic_unit(
    dir: &Path,
    engine: Engine,
    k: u32,
    timeout_secs: u32,
    max_tokens_per_1000px2: u32,
) -> anyhow::Result<Job> {
    let (jpeg, alto) = synthetic_page();
    std::fs::create_dir_all(dir)?;
    let image = dir.join("synthetic-page.jpg");
    let alto_path = dir.join("synthetic-loc.xml");
    atomic(&image, &jpeg)?;
    atomic(&alto_path, alto.as_bytes())?;
    let job = local_unit(
        &image,
        &alto_path,
        dir,
        engine,
        522,
        k,
        timeout_secs,
        max_tokens_per_1000px2,
    )?;
    std::fs::remove_file(image)?;
    std::fs::remove_file(alto_path)?;
    Ok(job)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn engine() -> Engine {
        Engine {
            image: format!("sha256:{}", "a".repeat(64)),
            llama: format!("sha256:{}", "b".repeat(64)),
            models: super::super::default_models(),
            weights: format!("sha256:{}", "c".repeat(64)),
        }
    }

    #[test]
    fn local_unit_cuts_crops_from_the_alto() {
        let tmp = tempfile::tempdir().unwrap();
        let unit = tmp.path().join("q");
        crate::quality::work::tests::unit(&unit);
        let j = local_unit(
            &unit.join("page.jpg"),
            &unit.join("loc.xml"),
            &tmp.path().join("u"),
            engine(),
            522,
            4,
            600,
            4,
        )
        .unwrap();
        assert_eq!(j.crops.len(), 2);
        assert!(j.crops[0].loc_text.contains("mayor") || j.crops[1].loc_text.contains("mayor"));
        assert!(tmp.path().join("u/qwen.json").exists());
        let again = cut(
            &std::fs::read(unit.join("page.jpg")).unwrap(),
            &std::fs::read(unit.join("loc.xml")).unwrap(),
            522,
            4,
        )
        .unwrap();
        assert_eq!(again, j.crops);
    }

    #[tokio::test]
    async fn stages_every_quality_unit_once() {
        let tmp = tempfile::tempdir().unwrap();
        let qroot = tmp.path().join("quality");
        let unit = crate::quality::units(&qroot).join("sn83045462_1890-05-03_ed-1_seq-1");
        crate::quality::work::tests::unit(&unit);
        std::fs::write(unit.join("sample.json"), b"{}").unwrap();
        let opts = Options {
            quality_root: qroot.clone(),
            root: tmp.path().join("qwen"),
            engine: engine(),
            gpu_timeout_secs: 1800,
            cpu_timeout_secs: 7200,
            max_tokens_per_1000px2: 4,
            max_pages: 20,
        };
        let s = run(&opts).await.unwrap();
        assert_eq!((s.staged, s.no_crops, s.already), (1, 0, 0));
        let out = units(&opts.root).join("sn83045462_1890-05-03_ed-1_seq-1");
        let job: Job = serde_json::from_slice(&std::fs::read(out.join(JOB_FILE)).unwrap()).unwrap();
        job.validate().unwrap();
        assert_eq!(job.doc_id, "local");
        assert!(!job.crops.is_empty());
        assert!(out.join("sample.json").exists());
        assert_eq!(
            std::fs::read(out.join(PAGE_FILE)).unwrap(),
            std::fs::read(unit.join("page.jpg")).unwrap()
        );
        let s = run(&opts).await.unwrap();
        assert_eq!((s.staged, s.already), (0, 1));
        // A page whose ALTO has no cuttable line gets a marker, not a unit.
        let empty = crate::quality::units(&qroot).join("sn83045462_1890-05-04_ed-1_seq-1");
        crate::quality::work::tests::unit(&empty);
        let page = std::fs::read(empty.join("page.jpg")).unwrap();
        let alto =
            crate::quality::alto_xml(1000, 1400, &[vec![vec![(10, 10, 60, 40, "one", 0.9)]]]);
        let texts: usnm_ocr_contract::quality::Texts =
            serde_json::from_slice(&std::fs::read(empty.join("texts.json")).unwrap()).unwrap();
        crate::quality::stage::local_unit(&crate::quality::stage::LocalUnit {
            page: &page,
            alto: alto.as_bytes(),
            texts: &texts,
            places: "Albany\n",
            dir: &empty,
            engine: usnm_ocr_contract::quality::Engine {
                image: format!("sha256:{}", "a".repeat(64)),
            },
            function_words: vec![],
            crop_seed: 1,
            k_crops: 4,
        })
        .unwrap();
        let s = run(&opts).await.unwrap();
        assert_eq!((s.staged, s.no_crops, s.already), (0, 1, 1));
        assert!(units(&opts.root)
            .join("sn83045462_1890-05-04_ed-1_seq-1/no_crops")
            .exists());
    }

    #[test]
    fn synthetic_page_cuts_both_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let j = synthetic_unit(tmp.path(), engine(), 2, 600, 4).unwrap();
        assert_eq!(j.crops.len(), 2);
        let mut texts: Vec<&str> = j.crops.iter().map(|c| c.loc_text.as_str()).collect();
        texts.sort();
        assert_eq!(
            texts,
            ["HELLO NEWSPAPER WORLD NOW", "QUICK BROWN FOX JUMPS"]
        );
        let page =
            image::load_from_memory(&std::fs::read(tmp.path().join(PAGE_FILE)).unwrap()).unwrap();
        for c in &j.crops {
            let png = crate::qwen::work::crop_png(&page, c).unwrap();
            let cut = image::load_from_memory(&png).unwrap().to_luma8();
            let ink = cut.pixels().filter(|p| p.0[0] < 128).count();
            assert!(ink > 2000, "crop {} has {ink} ink pixels", c.id);
        }
        assert!(!tmp.path().join("synthetic-page.jpg").exists());
    }
}
