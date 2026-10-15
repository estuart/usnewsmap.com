//! The campaign's pages: a stratified random sample over the prefetch
//! manifest (language × decade × batch prefix × damage bin, as
//! `assess::sample`), restricted to pages that have an American Stories
//! text when `--with-as` is set, so every page has at least two readers
//! besides ours.
//!
//! The manifest must be one built with `usnm-ingest ocr-prefetch-manifest
//! --min-damage 0` (the default manifest holds only damaged pages). The
//! output is JSON Lines of `assess::SampleRow` (mode `ocr`), in hash
//! order, so any prefix of it is mixed. An explicit `--pages-file` takes
//! those pages from the manifest instead of drawing: they keep their
//! stratum and its size, and a page the manifest lacks is an error.
use crate::assess::{
    sample::{draw, read_strata, weigh},
    SampleRow,
};
use crate::atomic;
use anyhow::{ensure, Context};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};
use usnm_ocr_contract::assess::Mode;

#[derive(Debug, Clone)]
pub struct Options {
    pub manifest: PathBuf,
    /// JSON Lines, one `assess::SampleRow`-shaped row per page.
    pub out: PathBuf,
    pub pages: usize,
    pub min_per_stratum: usize,
    pub seed: String,
    /// JSON object: LCCN to the title's MARC language codes.
    pub title_languages: Option<PathBuf>,
    /// A list of doc ids that have an American Stories text, one per
    /// line; pages not in it are left out.
    pub with_as: Option<PathBuf>,
    /// An explicit list of doc ids, one per line, instead of a draw.
    pub pages_file: Option<PathBuf>,
}

#[derive(Debug, Default, PartialEq)]
pub struct Summary {
    pub manifest_pages: usize,
    pub no_language: usize,
    pub no_model: usize,
    pub without_as: usize,
    pub strata: usize,
    pub drawn: usize,
}

/// One doc id per line; blank lines and `#` comments skipped.
pub fn read_list(path: &Path) -> anyhow::Result<BTreeSet<String>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {path:?}"))?;
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_owned)
        .collect())
}

pub fn run(opts: &Options) -> anyhow::Result<Summary> {
    ensure!(opts.pages > 0, "--pages must be positive");
    ensure!(!opts.seed.is_empty(), "--seed must not be empty");
    let read = read_strata(&opts.manifest, opts.title_languages.as_deref())?;
    let mut strata = read.strata;
    let mut summary = Summary {
        manifest_pages: read.manifest_pages,
        no_language: read.no_language,
        no_model: read.no_model,
        ..Default::default()
    };
    // Stratum sizes stay the corpus's: the American Stories restriction
    // narrows the pool, not the population the weights refer to.
    let sizes: BTreeMap<_, usize> = strata.iter().map(|(k, v)| (k.clone(), v.len())).collect();
    if let Some(list) = &opts.with_as {
        let with_as = read_list(list)?;
        for rows in strata.values_mut() {
            let before = rows.len();
            rows.retain(|(r, _)| with_as.contains(&r.doc_id));
            summary.without_as += before - rows.len();
        }
        strata.retain(|_, v| !v.is_empty());
    }
    summary.strata = strata.len();
    let mut rows: Vec<(String, SampleRow)> = match &opts.pages_file {
        Some(file) => {
            let wanted = read_list(file)?;
            let mut found = BTreeSet::new();
            let mut out = Vec::new();
            for (k, pool) in &strata {
                for (r, lang) in pool {
                    if !wanted.contains(&r.doc_id) {
                        continue;
                    }
                    found.insert(r.doc_id.clone());
                    out.push((
                        crate::assess::sample::order(&opts.seed, Mode::Ocr, &r.doc_id),
                        SampleRow {
                            batch: r.batch.clone(),
                            batch_version: r.batch_version,
                            crash_weight: 0.0,
                            damage: k.3.clone(),
                            damage_rate: r.damage_rate,
                            decade: r.decade(),
                            doc_id: r.doc_id.clone(),
                            lang: lang.clone(),
                            mode: Mode::Ocr,
                            prefix: k.2.clone(),
                            stratum_pages: sizes[k],
                            weight: 0.0,
                            year: chrono::Datelike::year(&r.date),
                        },
                    ));
                }
            }
            let missing: Vec<&String> = wanted.difference(&found).collect();
            ensure!(
                missing.is_empty(),
                "{} listed pages are not in the manifest (or have no language or model): {}",
                missing.len(),
                missing
                    .iter()
                    .take(5)
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            out.sort_by(|a, b| a.0.cmp(&b.0));
            out
        }
        None => draw(
            &strata,
            opts.pages,
            opts.min_per_stratum,
            &opts.seed,
            &BTreeSet::new(),
            Mode::Ocr,
        ),
    };
    for (_, r) in &mut rows {
        r.stratum_pages = sizes[&crate::assess::sample::key_of(r)];
    }
    weigh(&mut rows, &mut []);
    summary.drawn = rows.len();
    let mut text = String::new();
    for (_, r) in &rows {
        text.push_str(&serde_json::to_string(r)?);
        text.push('\n');
    }
    atomic(&opts.out, text.as_bytes())?;
    Ok(summary)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use usnm_ocr_contract::prefetch::ManifestRow;

    /// A manifest of `n` English pages of one title in 1890 with damage
    /// rates spread over the three bins.
    pub(crate) fn manifest(dir: &Path, n: usize) -> PathBuf {
        let rows: Vec<ManifestRow> = (0..n)
            .map(|i| {
                let date = chrono::NaiveDate::from_ymd_opt(1890, 1, 1).unwrap()
                    + chrono::Days::new(i as u64);
                let key = usnm_core::ids::PageKey::new("sn83045462", date, 1, 1).unwrap();
                ManifestRow::new(&key, "dlc_abc_ver01", 1, Some("eng".into()), "detected", {
                    let r = i as f64 / n as f64;
                    r * 0.9
                })
            })
            .collect();
        let bytes = usnm_ocr_contract::prefetch::encode(&rows).unwrap();
        let p = dir.join("manifest.parquet");
        std::fs::write(&p, bytes).unwrap();
        p
    }

    #[test]
    fn draws_a_stratified_sample_and_an_explicit_list() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = manifest(tmp.path(), 30);
        let out = tmp.path().join("sample.jsonl");
        let s = run(&Options {
            manifest: manifest.clone(),
            out: out.clone(),
            pages: 9,
            min_per_stratum: 1,
            seed: "s".into(),
            title_languages: None,
            with_as: None,
            pages_file: None,
        })
        .unwrap();
        assert_eq!(s.manifest_pages, 30);
        assert_eq!(s.strata, 3);
        assert_eq!(s.drawn, 9);
        let rows = crate::assess::read_sample(&out).unwrap();
        assert!(rows.iter().all(|r| r.mode == Mode::Ocr && r.weight > 0.0));
        let bins: BTreeSet<&str> = rows.iter().map(|r| r.damage.as_str()).collect();
        assert_eq!(bins.len(), 3);
        // The same seed draws the same pages.
        let again = tmp.path().join("again.jsonl");
        run(&Options {
            out: again.clone(),
            ..Options {
                manifest: manifest.clone(),
                out: out.clone(),
                pages: 9,
                min_per_stratum: 1,
                seed: "s".into(),
                title_languages: None,
                with_as: None,
                pages_file: None,
            }
        })
        .unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), std::fs::read(&again).unwrap());
        // An American Stories list narrows the pool, not the stratum size.
        let list = tmp.path().join("as.txt");
        std::fs::write(&list, format!("{}\n{}\n", rows[0].doc_id, rows[1].doc_id)).unwrap();
        let s = run(&Options {
            manifest: manifest.clone(),
            out: out.clone(),
            pages: 9,
            min_per_stratum: 1,
            seed: "s".into(),
            title_languages: None,
            with_as: Some(list.clone()),
            pages_file: None,
        })
        .unwrap();
        assert_eq!(s.drawn, 2);
        assert_eq!(s.without_as, 28);
        let narrowed = crate::assess::read_sample(&out).unwrap();
        assert!(narrowed.iter().all(|r| r.stratum_pages >= 9));
        // An explicit list keeps those pages with their strata.
        let s = run(&Options {
            manifest,
            out: out.clone(),
            pages: 1,
            min_per_stratum: 1,
            seed: "s".into(),
            title_languages: None,
            with_as: None,
            pages_file: Some(list),
        })
        .unwrap();
        assert_eq!(s.drawn, 2);
        let listed = crate::assess::read_sample(&out).unwrap();
        assert_eq!(listed.len(), 2);
        assert!(listed.iter().all(|r| r.stratum_pages > 0 && r.weight > 0.0));
    }

    #[test]
    fn a_listed_page_the_manifest_lacks_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = manifest(tmp.path(), 3);
        let list = tmp.path().join("pages.txt");
        std::fs::write(&list, "sn83045462_1999-01-01_ed-1_seq-1\n").unwrap();
        let err = run(&Options {
            manifest,
            out: tmp.path().join("o.jsonl"),
            pages: 1,
            min_per_stratum: 1,
            seed: "s".into(),
            title_languages: None,
            with_as: None,
            pages_file: Some(list),
        })
        .unwrap_err();
        assert!(err.to_string().contains("not in the manifest"));
    }
}
