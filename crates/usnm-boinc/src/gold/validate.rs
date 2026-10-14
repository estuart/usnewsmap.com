//! What stock `script_validator` runs for `usnm_gold`, and what `collect` checks again.
//!
//! One result per unit (quorum 1): each result measures the host that ran it, so two hosts are
//! not expected to agree on edits. A result is checked on its own against what the server knows
//! without trusting the host: the unit it names was made here (job id, bundle version, manifest
//! sha256, engine), every crop of the bundle is either scored or skipped exactly once, each
//! scored crop's reference counts equal the ones `prepare` computed from the transcription,
//! the edit counts and times are in range, and the summary adds up. A host that reads the wrong
//! transcriptions, runs a partial bundle, or edits the numbers by hand fails here. It does not
//! prove which engine ran; that is the trusted-volunteer rule in docs/boinc.md.
use super::{prepare, Report, REPORT_VERSION};
use crate::gold::job::{Engine, Job, MAX_RESULT};
use crate::{atomic, read};
use anyhow::{ensure, Context};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};
use usnm_ocr_survey::bundle;

/// A hypothesis's edits beyond its reference length that still count as sane. The output text
/// limit bounds a real engine's output; this only rejects numbers no crop could produce.
pub const MAX_EXTRA_CHAR_EDITS: usize = 200_000;
pub const MAX_EXTRA_WORD_EDITS: usize = 40_000;
/// How long a crop may take beyond its deadline (engine start and stop).
pub const SLACK_MS: u64 = 30_000;

/// `<project>/gold/<manifest sha256>.json`: the validator's view of one bundle.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expectations {
    pub bundle_version: u32,
    pub crops: BTreeMap<String, (String, prepare::Expected)>,
    /// Every unit made for this bundle, with its engine.
    pub jobs: BTreeMap<String, Engine>,
    pub manifest_sha256: String,
}

pub fn expected_dir(project: &Path) -> PathBuf {
    project.join("gold")
}

fn expected_path(dir: &Path, manifest_sha256: &str) -> anyhow::Result<PathBuf> {
    ensure!(
        usnm_ocr_contract::boinc::is_sha256(manifest_sha256),
        "invalid manifest sha256"
    );
    Ok(dir.join(format!("{manifest_sha256}.json")))
}

pub fn load_expected(dir: &Path, manifest_sha256: &str) -> anyhow::Result<Expectations> {
    let p = expected_path(dir, manifest_sha256)?;
    serde_json::from_slice(&read(&p, 16 << 20).with_context(|| format!("{}", p.display()))?)
        .with_context(|| format!("parsing {}", p.display()))
}

/// Records units in the bundle's expectations, before they are submitted.
pub fn add_jobs(dir: &Path, p: &prepare::Prepared, jobs: &[Job]) -> anyhow::Result<()> {
    let path = expected_path(dir, &p.manifest_sha256)?;
    let mut e = if path.exists() {
        let e = load_expected(dir, &p.manifest_sha256)?;
        ensure!(
            e.crops == p.crops && e.bundle_version == p.bundle_version,
            "{} disagrees with prepared.json",
            path.display()
        );
        e
    } else {
        Expectations {
            bundle_version: p.bundle_version,
            crops: p.crops.clone(),
            jobs: BTreeMap::new(),
            manifest_sha256: p.manifest_sha256.clone(),
        }
    };
    for j in jobs {
        e.jobs.insert(j.id.clone(), j.engine.clone());
    }
    atomic(&path, &serde_json::to_vec_pretty(&e)?)
}

pub fn load(file: &Path) -> anyhow::Result<Report> {
    parse_strict(&read(file, MAX_RESULT)?)
}

pub fn lang_ok(lang: &str) -> bool {
    !lang.is_empty() && lang.len() <= 8 && lang.bytes().all(|b| b.is_ascii_lowercase())
}

fn reason_ok(reason: &str) -> bool {
    matches!(reason, "engine-error" | "not-prepared")
        || reason.strip_prefix("no-model:").is_some_and(lang_ok)
}

/// The unit a result file belongs to, from its upload name (`<workunit>_<replica>_<file>`), so a
/// report from one unit can't be validated as another's.
pub fn workunit_job_id(file: &Path) -> anyhow::Result<String> {
    let name = file
        .file_name()
        .and_then(|s| s.to_str())
        .context("result file name")?;
    let id = name
        .strip_prefix("usnm_gold_")
        .and_then(|s| s.get(..64))
        .filter(|s| usnm_ocr_contract::boinc::is_sha256(s))
        .with_context(|| format!("{name} is not a usnm_gold result file"))?;
    Ok(id.to_string())
}

/// Reads a report and refuses any field the schema doesn't have, so nothing but counts and
/// identifiers can travel in it.
pub fn parse_strict(bytes: &[u8]) -> anyhow::Result<Report> {
    no_duplicate_keys(bytes)?;
    let raw: serde_json::Value = serde_json::from_slice(bytes).context("gold-result.json")?;
    let r: Report = serde_json::from_value(raw.clone()).context("gold-result.json")?;
    ensure!(
        serde_json::to_value(&r)? == raw,
        "the report has fields outside the schema"
    );
    Ok(r)
}

/// Refuses JSON with a repeated key in any object: parsing keeps only the last copy, while the
/// uploaded file keeps both.
pub fn no_duplicate_keys(bytes: &[u8]) -> anyhow::Result<()> {
    use serde::de::{Deserializer as _, MapAccess, SeqAccess, Visitor};
    struct V;
    impl<'de> Visitor<'de> for V {
        type Value = ();
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("JSON")
        }
        fn visit_bool<E>(self, _: bool) -> Result<(), E> {
            Ok(())
        }
        fn visit_i64<E>(self, _: i64) -> Result<(), E> {
            Ok(())
        }
        fn visit_u64<E>(self, _: u64) -> Result<(), E> {
            Ok(())
        }
        fn visit_f64<E>(self, _: f64) -> Result<(), E> {
            Ok(())
        }
        fn visit_str<E>(self, _: &str) -> Result<(), E> {
            Ok(())
        }
        fn visit_unit<E>(self) -> Result<(), E> {
            Ok(())
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<(), A::Error> {
            while a.next_element_seed(S)?.is_some() {}
            Ok(())
        }
        fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<(), A::Error> {
            let mut seen = BTreeSet::new();
            while let Some(k) = a.next_key::<String>()? {
                if !seen.insert(k.clone()) {
                    return Err(serde::de::Error::custom(format!("duplicate key {k:?}")));
                }
                a.next_value_seed(S)?;
            }
            Ok(())
        }
    }
    struct S;
    impl<'de> serde::de::DeserializeSeed<'de> for S {
        type Value = ();
        fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
            d.deserialize_any(V)
        }
    }
    let mut d = serde_json::Deserializer::from_slice(bytes);
    d.deserialize_any(V).context("gold-result.json")?;
    d.end().context("gold-result.json")?;
    Ok(())
}

/// Checks one report against the server's expectations.
pub fn check_report(r: &Report, e: &Expectations) -> anyhow::Result<()> {
    ensure!(r.engine.name == "usnm-tesseract", "unknown engine name");
    ensure!(
        r.report_version == REPORT_VERSION,
        "unsupported report version"
    );
    let b = r.boinc.as_ref().context("not a BOINC report")?;
    let engine = e
        .jobs
        .get(&b.job_id)
        .context("the report names a unit this server did not make")?;
    ensure!(
        r.bundle_version == e.bundle_version && r.manifest_sha256 == e.manifest_sha256,
        "bundle version or manifest differs"
    );
    ensure!(
        b.engine_image == engine.image
            && b.engine_base == engine.base
            && r.engine.version == engine.version
            && r.engine.psm == engine.psm
            && r.engine.dpi == u32::from(engine.dpi),
        "engine differs from the unit's"
    );
    ensure!(r.limit.is_none(), "a limited run is partial");
    chrono::DateTime::parse_from_rfc3339(&r.completed_at).context("completed_at")?;
    let mut seen = BTreeSet::new();
    for c in &r.crops {
        let id = &c.result.crop_id;
        ensure!(seen.insert(id.as_str()), "{id} reported twice");
        let (source, want) = e
            .crops
            .get(id)
            .with_context(|| format!("{id}: not in the bundle"))?;
        ensure!(&c.result.source == source, "{id}: source differs");
        let k = &c.result.counts;
        ensure!(
            k.ref_chars == want.ref_chars && k.ref_words == want.ref_words,
            "{id}: reference counts differ from the transcription's"
        );
        ensure!(
            k.char_edits <= k.ref_chars + MAX_EXTRA_CHAR_EDITS
                && k.word_edits <= k.ref_words + MAX_EXTRA_WORD_EDITS,
            "{id}: edit counts out of range"
        );
        ensure!(
            c.elapsed_ms <= u64::from(engine.timeout_secs) * 1000 + SLACK_MS,
            "{id}: time over the deadline"
        );
        ensure!(
            !c.profile.is_empty()
                && c.profile.len() <= 64
                && c.profile
                    .bytes()
                    .all(|x| x.is_ascii_alphanumeric() || b"_+-".contains(&x)),
            "{id}: invalid profile"
        );
    }
    for s in &r.skipped {
        ensure!(
            e.crops.contains_key(&s.crop_id),
            "{}: skipped crop not in the bundle",
            s.crop_id
        );
        ensure!(
            seen.insert(s.crop_id.as_str()),
            "{} reported twice",
            s.crop_id
        );
        ensure!(reason_ok(&s.reason), "{}: unknown skip reason", s.crop_id);
    }
    ensure!(
        seen.len() == e.crops.len(),
        "{} of {} crops reported",
        seen.len(),
        e.crops.len()
    );
    ensure!(!r.crops.is_empty(), "no crop was scored");
    let results: Vec<_> = r.crops.iter().map(|c| c.result.clone()).collect();
    let want = serde_json::to_value(bundle::summarize(&results))?;
    ensure!(
        serde_json::to_value(&r.summary)? == want,
        "the summary does not add up"
    );
    Ok(())
}

/// script_validator's init script: exit 0 when the result is valid.
pub fn check(file: &Path, dir: &Path) -> anyhow::Result<()> {
    let r = load(file)?;
    let id = workunit_job_id(file)?;
    ensure!(
        r.boinc.as_ref().is_some_and(|b| b.job_id == id),
        "the report belongs to another unit"
    );
    check_report(&r, &load_expected(dir, &r.manifest_sha256)?)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::gold::{submit, BoincInfo, CropRun, EngineInfo};
    use usnm_ocr_survey::score::Counts;

    pub(crate) fn report(e: &Expectations, job_id: &str) -> Report {
        let crops: Vec<CropRun> = e
            .crops
            .iter()
            .map(|(id, (source, x))| CropRun {
                result: bundle::CropResult {
                    counts: Counts {
                        ref_chars: x.ref_chars,
                        char_edits: 3,
                        ref_words: x.ref_words,
                        word_edits: 1,
                    },
                    crop_id: id.clone(),
                    source: source.clone(),
                },
                elapsed_ms: 900,
                profile: "eng".into(),
            })
            .collect();
        let results: Vec<_> = crops.iter().map(|c| c.result.clone()).collect();
        let engine = &e.jobs[job_id];
        Report {
            boinc: Some(BoincInfo {
                engine_base: engine.base.clone(),
                engine_image: engine.image.clone(),
                host_id: Some(7),
                job_id: job_id.into(),
            }),
            bundle_version: e.bundle_version,
            completed_at: "2026-10-13T00:00:00Z".into(),
            summary: bundle::summarize(&results),
            crops,
            engine: EngineInfo {
                dpi: 300,
                name: "usnm-tesseract".into(),
                psm: 3,
                version: engine.version.clone(),
            },
            limit: None,
            manifest_sha256: e.manifest_sha256.clone(),
            report_version: REPORT_VERSION,
            skipped: vec![],
        }
    }

    pub(crate) fn setup(dir: &Path) -> (Expectations, String) {
        let p = submit::tests::prepared();
        let j = submit::job(&p, &submit::tests::engine(), 0).unwrap();
        add_jobs(dir, &p, std::slice::from_ref(&j)).unwrap();
        (load_expected(dir, &p.manifest_sha256).unwrap(), j.id)
    }

    #[test]
    fn checks_counts_coverage_and_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let (e, id) = setup(tmp.path());
        let good = report(&e, &id);
        check_report(&good, &e).unwrap();
        let f = tmp.path().join(format!("usnm_gold_{id}_0_0"));
        std::fs::write(&f, serde_json::to_vec(&good).unwrap()).unwrap();
        check(&f, tmp.path()).unwrap();
        let other = tmp.path().join(format!("usnm_gold_{}_0_0", "f".repeat(64)));
        std::fs::copy(&f, &other).unwrap();
        assert!(
            check(&other, tmp.path()).is_err(),
            "replayed into another unit"
        );
        let mut v = serde_json::to_value(&good).unwrap();
        v["crops"][0]["text"] = "page text".into();
        std::fs::write(&f, serde_json::to_vec(&v).unwrap()).unwrap();
        assert!(check(&f, tmp.path()).is_err(), "extra field");
        let dup = String::from_utf8(serde_json::to_vec(&good).unwrap())
            .unwrap()
            .replacen('{', "{\"completed_at\":\"page text\",", 1);
        std::fs::write(&f, dup).unwrap();
        assert!(check(&f, tmp.path()).is_err(), "duplicate key");

        let mut r = good.clone();
        r.crops[0].result.counts.ref_chars += 1;
        assert!(check_report(&r, &e).is_err(), "wrong transcription");

        let mut r = good.clone();
        let c = r.crops.pop().unwrap();
        r.summary = bundle::summarize(&[r.crops[0].result.clone()]);
        assert!(check_report(&r, &e).is_err(), "a crop missing");
        r.skipped.push(bundle::Skipped {
            crop_id: c.result.crop_id,
            reason: "no-model:deu".into(),
        });
        check_report(&r, &e).unwrap();
        r.skipped[0].reason = "engine: some page text".into();
        assert!(check_report(&r, &e).is_err(), "free text reason");

        let mut r = good.clone();
        r.crops[0].result.counts.char_edits = 0;
        assert!(check_report(&r, &e).is_err(), "summary edited by hand");

        let mut r = good.clone();
        r.boinc.as_mut().unwrap().job_id = "f".repeat(64);
        assert!(check_report(&r, &e).is_err(), "unknown unit");

        let mut r = good.clone();
        r.engine.version = "5.4.0".into();
        assert!(check_report(&r, &e).is_err(), "other engine");

        let mut r = good;
        r.limit = Some(3);
        assert!(check_report(&r, &e).is_err(), "partial run");
    }
}
