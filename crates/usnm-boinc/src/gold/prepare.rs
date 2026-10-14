//! Server side, once per bundle: what `submit` needs to make gold units.
//!
//! Verifies the bundle, then fetches each original file it names (the BLN600 zip from its
//! publisher, the page images from loc.gov) into a server cache, one request at a time, at least
//! [`MIN_GAP`] apart, keeping a file only if its sha256 matches. The server never serves these:
//! BOINC's `create_work --remote_file` needs each one's size and md5, so the client can fetch it
//! from the publisher and check it, and the validator needs each crop's reference counts. Both
//! are computed here and written to `prepared.json` with no text in it. The bundle tar itself
//! (shareable files only) is copied to `bundle.tar` under the campaign root and served by the
//! project as a sticky input.
use crate::atomic;
use anyhow::{ensure, Context};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};
use usnm_ocr_survey::{bundle, score};

pub const MIN_GAP: Duration = Duration::from_secs(5);
pub const PREPARED: &str = "prepared.json";
pub const BUNDLE_COPY: &str = "bundle.tar";

/// A unit input as `create_work` and the job see it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Staged {
    pub bytes: u64,
    pub md5: String,
    /// The open name in the slot.
    pub name: String,
    pub sha256: String,
    /// The publisher's URL, for an original file; none for the bundle tar.
    pub url: Option<String>,
}

/// A crop's reference counts: what every host must report for it, whatever its engine reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expected {
    pub ref_chars: usize,
    pub ref_words: usize,
}

/// prepared.json. Counts and checksums only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Prepared {
    pub bundle: Staged,
    pub bundle_version: u32,
    /// Crop id to its source and reference counts.
    pub crops: BTreeMap<String, (String, Expected)>,
    pub manifest_sha256: String,
    pub remotes: Vec<Staged>,
}

pub struct Options {
    pub bundle: PathBuf,
    pub root: PathBuf,
    pub gap: Duration,
}

pub fn md5_sha256(p: &Path) -> anyhow::Result<(u64, String, String)> {
    use md5::Digest as _;
    use std::io::Read as _;
    let mut f = std::fs::File::open(p).with_context(|| format!("opening {}", p.display()))?;
    let (mut m, mut s) = (md5::Md5::new(), sha2::Sha256::new());
    let mut buf = vec![0u8; 1 << 16];
    let mut n = 0u64;
    loop {
        let k = f.read(&mut buf)?;
        if k == 0 {
            break;
        }
        n += k as u64;
        m.update(&buf[..k]);
        s.update(&buf[..k]);
    }
    let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    Ok((n, hex(&m.finalize()), hex(&s.finalize())))
}

pub fn read(root: &Path) -> anyhow::Result<Prepared> {
    let p = root.join(PREPARED);
    serde_json::from_slice(&crate::read(&p, 16 << 20)?)
        .with_context(|| format!("reading {}", p.display()))
}

pub async fn run(opts: &Options) -> anyhow::Result<Prepared> {
    ensure!(opts.gap >= MIN_GAP, "--gap-secs must be at least 5");
    ensure!(opts.bundle.is_file(), "--bundle must be the bundle tar");
    std::fs::create_dir_all(&opts.root)?;
    let copy = opts.root.join(BUNDLE_COPY);
    if copy != opts.bundle {
        let tmp = opts.root.join(format!("{BUNDLE_COPY}.part"));
        std::fs::copy(&opts.bundle, &tmp)?;
        std::fs::rename(&tmp, &copy)?;
    }
    let scratch = tempfile::tempdir()?;
    let (dir, m) = super::open(&copy, scratch.path())?;
    let cache = opts.root.join("cache");
    let staged = bundle::stage(&m, &cache, opts.gap).await?;
    ensure!(
        staged.failed.is_empty(),
        "{} original files could not be fetched: {:?}",
        staged.failed.len(),
        staged.failed
    );
    let mut remotes = Vec::new();
    for r in &m.remotes {
        let p = bundle::cache_path(&cache, r);
        let (bytes, md5, sha) = md5_sha256(&p)?;
        ensure!(sha == r.sha256, "{}: cached file changed", r.id);
        remotes.push(Staged {
            bytes,
            md5,
            name: p
                .file_name()
                .and_then(|s| s.to_str())
                .context("cache name")?
                .to_string(),
            sha256: sha,
            url: Some(r.url.clone()),
        });
    }
    let (ready, skipped) =
        bundle::materialize(&m, &dir, &cache, &scratch.path().join("crops"), None)?;
    ensure!(
        skipped.is_empty(),
        "{} crops could not be prepared, first: {:?}",
        skipped.len(),
        skipped.first()
    );
    let crops = ready
        .iter()
        .map(|r| {
            let c = score::cer_wer(&r.transcription, "");
            (
                r.crop_id.clone(),
                (
                    r.source.clone(),
                    Expected {
                        ref_chars: c.ref_chars,
                        ref_words: c.ref_words,
                    },
                ),
            )
        })
        .collect();
    let (bytes, md5, sha) = md5_sha256(&copy)?;
    let p = Prepared {
        bundle: Staged {
            bytes,
            md5,
            name: crate::gold::job::BUNDLE_FILE.into(),
            sha256: sha,
            url: None,
        },
        bundle_version: m.bundle_version,
        crops,
        manifest_sha256: m.digest()?,
        remotes,
    };
    atomic(&opts.root.join(PREPARED), &serde_json::to_vec_pretty(&p)?)?;
    Ok(p)
}
