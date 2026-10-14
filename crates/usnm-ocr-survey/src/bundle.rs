//! The volunteer gold bundle (#457): a versioned, checksummed subset of the public ground truth that
//! a BOINC client verifies, completes from the original sources where a licence requires it, cuts
//! locally and scores locally.
//!
//! What ships is decided by each source's licence ([`policy`]):
//! - **References** (BLN600, CC BY-NC-ND): nothing derived from the dataset ships. The manifest names
//!   the original zip (URL, sha256), the image and transcription members in it and their checksums;
//!   the client downloads the zip, checks it and reads both members unchanged.
//! - **Page references** (Overproof dataset 3): the transcription ships (CC BY-NC-SA, with its licence
//!   files), the page image does not. The manifest names the Library of Congress page image (URL,
//!   sha256) and the box; the client downloads it and cuts the box.
//! - **Crops** (NewsEye CC BY 4.0, NZZ, and other sets that allow derivatives): the crop and the
//!   transcription ship, with the dataset's licence files and an ATTRIBUTION.txt that names the
//!   changes made.
//!
//! The builder is reproducible: the same inputs, version and seed give the same bytes. The manifest
//! holds no timestamps, and the tar has sorted entries with fixed metadata.

use crate::{score, text};
use anyhow::{ensure, Context};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

/// The manifest's `format`.
pub const FORMAT: &str = "usnm-ocr-gold-bundle";
/// The manifest's file name inside the bundle.
pub const MANIFEST: &str = "manifest.json";
/// The largest original file a client downloads (the BLN600 zip is about 68 MB).
pub const MAX_REMOTE: u64 = 256 << 20;
/// The largest manifest a client reads.
pub const MAX_MANIFEST: u64 = 16 << 20;

pub fn sha256_hex(b: &[u8]) -> String {
    let d = Sha256::digest(b);
    d.iter().map(|x| format!("{x:02x}")).collect()
}

fn sha256_file(p: &Path) -> anyhow::Result<String> {
    let mut f = std::fs::File::open(p).with_context(|| format!("opening {}", p.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|x| format!("{x:02x}")).collect())
}

/// How a source's crops reach the client.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Ships {
    /// The crops and transcriptions are in the bundle.
    Crops,
    /// Only references: the client downloads the original archive and reads its members unchanged.
    References,
    /// The transcriptions ship; the client downloads each page image and cuts the box.
    PageReferences,
}

/// The licence decides what ships. Sources that are not listed are refused, so a new dataset gets a
/// deliberate decision here and in docs/ocr-gold-set.md before it can go into a bundle.
pub fn policy(source: &str) -> Option<Ships> {
    match source {
        "bln600" => Some(Ships::References),
        "overproof-ca" => Some(Ships::PageReferences),
        "newseye-bnf" | "newseye-nlf-fi" | "newseye-nlf-sv" | "austrian" | "reichsanzeiger"
        | "nzz" => Some(Ships::Crops),
        _ => None,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub bytes: u64,
    pub path: String,
    pub sha256: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RemoteKind {
    /// An archive whose members are read unchanged.
    Zip,
    /// A page image a crop is cut from.
    PageImage,
}

/// An original file the client downloads from its publisher.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Remote {
    pub bytes: Option<u64>,
    pub id: String,
    pub kind: RemoteKind,
    /// The archive's password where the publisher prints one (BLN600's deters scrapers; it is on the
    /// dataset page and in its readme).
    pub password: Option<String>,
    pub sha256: String,
    pub source: String,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum CropImage {
    /// A file in the bundle.
    Bundled { path: String, sha256: String },
    /// A member of a remote archive, used unchanged.
    ZipMember {
        member: String,
        remote: String,
        sha256: String,
    },
    /// A box `[x0, y0, x1, y1]` cut from a remote page image, converted to greyscale.
    PageRegion {
        #[serde(rename = "box")]
        region: [u32; 4],
        remote: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum CropText {
    /// The transcription is in the manifest.
    Inline { sha256: String, text: String },
    /// A text member of a remote archive; `sha256` is of the transcription as gold.py reads it
    /// (UTF-8, byte-order mark and CRLF removed, ends stripped).
    ZipMember {
        member: String,
        remote: String,
        sha256: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Crop {
    pub crop_id: String,
    pub group: String,
    pub image: CropImage,
    pub lang: String,
    pub source: String,
    pub transcription: CropText,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    pub attribution: Option<String>,
    pub license: String,
    /// The bundle paths of the licence, readme and attribution files that travel with this source.
    pub license_files: Vec<String>,
    pub ships: Ships,
    pub url: String,
}

/// manifest.json. Fields are in alphabetical order, and so are the keys of every map.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub bundle_version: u32,
    pub crops: Vec<Crop>,
    /// Every file in the bundle except manifest.json.
    pub files: Vec<FileEntry>,
    pub format: String,
    pub remotes: Vec<Remote>,
    pub seed: u64,
    pub sources: BTreeMap<String, Source>,
}

impl Manifest {
    pub fn remote(&self, id: &str) -> anyhow::Result<&Remote> {
        self.remotes
            .iter()
            .find(|r| r.id == id)
            .with_context(|| format!("no remote {id:?}"))
    }

    /// Checks the manifest's own consistency: format, unique ids, safe paths, known remotes.
    pub fn check(&self) -> anyhow::Result<()> {
        ensure!(
            self.format == FORMAT,
            "not a gold bundle manifest: format {:?}",
            self.format
        );
        ensure!(self.bundle_version > 0, "bundle_version must be positive");
        let mut ids = BTreeSet::new();
        for c in &self.crops {
            ensure!(ids.insert(&c.crop_id), "duplicate crop {}", c.crop_id);
            ensure!(safe_name(&c.crop_id), "unsafe crop id {:?}", c.crop_id);
            ensure!(
                self.sources.contains_key(&c.source),
                "{}: unknown source",
                c.crop_id
            );
            let r = match &c.image {
                CropImage::Bundled { path, .. } => {
                    ensure!(
                        self.files.iter().any(|f| &f.path == path),
                        "{}: image not in files",
                        c.crop_id
                    );
                    None
                }
                CropImage::ZipMember { remote, .. } => Some((remote, RemoteKind::Zip)),
                CropImage::PageRegion { remote, region } => {
                    ensure!(
                        region[2] > region[0] && region[3] > region[1],
                        "{}: empty box",
                        c.crop_id
                    );
                    Some((remote, RemoteKind::PageImage))
                }
            };
            let t = match &c.transcription {
                CropText::ZipMember { remote, .. } => Some((remote, RemoteKind::Zip)),
                CropText::Inline { .. } => None,
            };
            for (id, kind) in r.into_iter().chain(t) {
                ensure!(
                    self.remote(id)?.kind == kind,
                    "{}: remote {id} is not a {kind:?}",
                    c.crop_id
                );
            }
        }
        for f in &self.files {
            ensure!(safe_path(&f.path), "unsafe path {:?}", f.path);
        }
        for r in &self.remotes {
            ensure!(
                is_sha256_hex(&r.sha256),
                "{}: sha256 is not 64 lowercase hex digits",
                r.id
            );
            ensure!(
                r.url.starts_with("https://"),
                "{}: remote URL must be https",
                r.id
            );
        }
        Ok(())
    }

    /// sha256 of the manifest as written; reported with every score.
    pub fn digest(&self) -> anyhow::Result<String> {
        Ok(sha256_hex(&to_json(self)?))
    }
}

/// 64 lowercase hex digits: a sha256 as the manifest writes it, and safe as a cache file name.
pub fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The largest bundle tar a client unpacks.
pub const MAX_BUNDLE: u64 = 256 << 20;

fn safe_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !s.starts_with('.')
}

fn safe_path(s: &str) -> bool {
    let p = Path::new(s);
    !s.is_empty() && p.components().all(|c| matches!(c, Component::Normal(_)))
}

/// The manifest's bytes: pretty JSON with a final newline.
pub fn to_json(m: &Manifest) -> anyhow::Result<Vec<u8>> {
    let mut v = serde_json::to_vec_pretty(m)?;
    v.push(b'\n');
    Ok(v)
}

// ------------------------------------------------------------------ build

/// One `public-convert` output directory to take crops from.
pub struct Input {
    pub source: String,
    pub dir: PathBuf,
    /// The original archive, for sources that ship references to its members (checked, not copied).
    pub original: Option<PathBuf>,
}

pub struct BuildOptions {
    pub inputs: Vec<Input>,
    pub version: u32,
    pub seed: u64,
    pub per_source: usize,
    pub out: PathBuf,
}

/// What a build wrote.
pub struct Built {
    pub dir: PathBuf,
    pub tar: PathBuf,
    pub tar_sha256: String,
    pub manifest: Manifest,
}

/// Bundle directory name for a version.
pub fn name(version: u32) -> String {
    format!("usnm-ocr-gold-v{version}")
}

fn str_field<'a>(row: &'a Value, key: &str) -> anyhow::Result<&'a str> {
    row.get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("row without {key}: {row}"))
}

/// The Creative Commons deeds a licence string names, so the attribution note links the licence.
fn license_urls(license: &str) -> Vec<&'static str> {
    let l = license.to_ascii_lowercase().replace(['-', ' '], "");
    [
        (
            "ccbyncnd4.0",
            "https://creativecommons.org/licenses/by-nc-nd/4.0/",
        ),
        (
            "ccbyncsa4.0",
            "https://creativecommons.org/licenses/by-nc-sa/4.0/",
        ),
        (
            "ccbync4.0",
            "https://creativecommons.org/licenses/by-nc/4.0/",
        ),
        ("ccby4.0", "https://creativecommons.org/licenses/by/4.0/"),
        (
            "cc01.0",
            "https://creativecommons.org/publicdomain/zero/1.0/",
        ),
        (
            "publicdomainmark1.0",
            "https://creativecommons.org/publicdomain/mark/1.0/",
        ),
    ]
    .into_iter()
    .filter(|(k, _)| l.contains(k))
    .map(|(_, u)| u)
    .collect()
}

/// Published archive passwords (gold.py's BLN600_PASSWORD).
fn archive_password(source: &str) -> Option<&'static str> {
    (source == "bln600").then_some("BLN600")
}

/// A manifest.jsonl row and its crops.jsonl row.
type Pair = (Value, Value);

/// Seeded choice spread over pages: pages and the crops within each page in a seeded order, then
/// round robin (each page's first crop, then each page's second, ...) up to `cap`.
fn pick<'a>(
    rows: &'a [Pair],
    seed: u64,
    source: &str,
    cap: usize,
) -> anyhow::Result<Vec<&'a Pair>> {
    let key = |s: &str| sha256_hex(format!("{seed}:{source}:{s}").as_bytes());
    let mut pages: BTreeMap<String, Vec<(String, &Pair)>> = BTreeMap::new();
    for r in rows {
        let page = str_field(&r.0, "page_id")?;
        let crop = str_field(&r.0, "crop_id")?;
        pages.entry(key(page)).or_default().push((key(crop), r));
    }
    let mut pages: Vec<Vec<(String, &Pair)>> = pages.into_values().collect();
    for p in &mut pages {
        p.sort_by(|a, b| a.0.cmp(&b.0));
    }
    let mut out = Vec::new();
    for rank in 0.. {
        let before = out.len();
        for p in &pages {
            if let Some((_, r)) = p.get(rank) {
                if out.len() < cap {
                    out.push(*r);
                }
            }
        }
        if out.len() == before || out.len() >= cap {
            break;
        }
    }
    Ok(out)
}

/// The SOURCE.json gold.py writes next to each download and copies into LICENSES/.
fn source_note(dir: &Path) -> anyhow::Result<Value> {
    let p = dir.join("LICENSES").join("SOURCE.json");
    serde_json::from_slice(&std::fs::read(&p).with_context(|| format!("reading {}", p.display()))?)
        .with_context(|| format!("parsing {}", p.display()))
}

/// The transcription of a BLN600 text member as gold.py's `parse_bln600` reads it.
pub fn read_text_member(bytes: &[u8]) -> anyhow::Result<String> {
    let s = std::str::from_utf8(bytes).context("transcription is not UTF-8")?;
    let s = s.strip_prefix('\u{feff}').unwrap_or(s);
    Ok(text::py_strip(&s.replace("\r\n", "\n")).to_string())
}

fn zip_member(
    zip: &Path,
    password: Option<&str>,
    member: &str,
    max: u64,
) -> anyhow::Result<Vec<u8>> {
    let f = std::fs::File::open(zip).with_context(|| format!("opening {}", zip.display()))?;
    let mut z = zip::ZipArchive::new(f).with_context(|| format!("reading {}", zip.display()))?;
    let e = match password {
        Some(p) => z.by_name_decrypt(member, p.as_bytes()),
        None => z.by_name(member),
    }
    .with_context(|| format!("{member} in {}", zip.display()))?;
    ensure!(
        e.size() <= max,
        "{member}: {} bytes, over the limit",
        e.size()
    );
    let mut v = Vec::with_capacity(usize::try_from(e.size()).unwrap_or(0));
    e.take(max + 1).read_to_end(&mut v)?;
    Ok(v)
}

/// Builds the bundle directory and its tar under `opts.out`.
pub fn build(opts: &BuildOptions) -> anyhow::Result<Built> {
    ensure!(opts.version > 0, "--version must be positive");
    let dir = opts.out.join(name(opts.version));
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    std::fs::create_dir_all(dir.join("crops"))?;
    let mut crops = Vec::new();
    let mut remotes: BTreeMap<String, Remote> = BTreeMap::new();
    let mut sources = BTreeMap::new();
    for input in &opts.inputs {
        let ships = policy(&input.source).with_context(|| {
            format!(
                "{}: no shipping policy for this source (see bundle::policy)",
                input.source
            )
        })?;
        let manifest = score::read_jsonl(&input.dir.join("manifest.jsonl"))?;
        let rows = score::read_jsonl(&input.dir.join("crops.jsonl"))?;
        let bad = score::invalid_crops(&rows, true);
        let by_id: BTreeMap<&str, &Value> = manifest
            .iter()
            .filter_map(|m| Some((m.get("crop_id")?.as_str()?, m)))
            .collect();
        let mut pairs = Vec::new();
        for r in rows {
            let cid = str_field(&r, "crop_id")?.to_string();
            let m = *by_id
                .get(cid.as_str())
                .with_context(|| format!("{cid}: not in manifest.jsonl"))?;
            ensure!(
                m.get("source_dataset").and_then(Value::as_str) == Some(input.source.as_str()),
                "{cid}: source_dataset is not {}",
                input.source
            );
            if bad.contains(&cid)
                || r.get("for_scoring") != Some(&Value::Bool(true))
                || m.get("image").is_none_or(Value::is_null)
            {
                continue;
            }
            pairs.push((m.clone(), r));
        }
        let note = source_note(&input.dir)?;
        let first = pairs
            .first()
            .map(|p| &p.0)
            .context(format!("{}: no usable crops", input.source))?;
        // Licence files: the dataset's own, unchanged, and an attribution note.
        let lic_dir = format!("licenses/{}", input.source);
        std::fs::create_dir_all(dir.join(&lic_dir))?;
        let mut license_files = Vec::new();
        let mut names: Vec<PathBuf> = std::fs::read_dir(input.dir.join("LICENSES"))?
            .map(|e| e.map(|e| e.path()))
            .collect::<Result<_, _>>()?;
        names.sort();
        for p in names.iter().filter(|p| p.is_file()) {
            let n = p
                .file_name()
                .and_then(|n| n.to_str())
                .context("licence file name")?;
            std::fs::copy(p, dir.join(&lic_dir).join(n))?;
            license_files.push(format!("{lic_dir}/{n}"));
        }
        let license = str_field(first, "source_license")?.to_string();
        let attribution = first
            .get("source_attribution")
            .and_then(Value::as_str)
            .map(str::to_string);
        let url = str_field(first, "source_url")?.to_string();
        let changes = match ships {
            Ships::Crops => "Changes: regions cut from the page images (box in manifest.json), converted to greyscale and re-encoded as JPEG; transcriptions are the dataset's, unchanged.",
            Ships::PageReferences => "Changes: the transcriptions are the dataset's lines for each region, joined with line breaks. No image ships: the client downloads each page image from the Library of Congress and cuts the box itself.",
            Ships::References => "Changes: none. Nothing from this dataset ships except its licence and readme files, unchanged; the client downloads the original archive and reads its images and transcriptions unchanged.",
        };
        ensure!(
            attribution.is_some(),
            "{}: manifest.jsonl has no source_attribution; every source in a bundle needs one",
            input.source
        );
        let urls = license_urls(&license);
        let attr = format!(
            "Source: {}\nURL: {url}\nLicence: {license}\nLicence text: {}\nAttribution: {}\n{changes}\n",
            input.source,
            if urls.is_empty() { "see the licence files".to_string() } else { urls.join(" ") },
            attribution.as_deref().unwrap_or_default()
        );
        std::fs::write(dir.join(&lic_dir).join("ATTRIBUTION.txt"), attr)?;
        license_files.push(format!("{lic_dir}/ATTRIBUTION.txt"));
        license_files.sort();
        sources.insert(
            input.source.clone(),
            Source {
                attribution,
                license,
                license_files,
                ships,
                url,
            },
        );

        let zip_remote = if ships == Ships::References {
            let file = note
                .get("file")
                .and_then(Value::as_str)
                .context("SOURCE.json has no original file URL")?;
            let sha = note
                .get("sha256")
                .and_then(Value::as_str)
                .context("SOURCE.json has no sha256")?;
            let password = archive_password(&input.source).map(str::to_string);
            let original = input
                .original
                .as_deref()
                .with_context(|| format!("{}: --original is required", input.source))?;
            ensure!(
                sha256_file(original)? == sha,
                "{}: the original archive's sha256 differs from SOURCE.json",
                input.source
            );
            let id = format!("{}-archive", input.source);
            remotes.insert(
                id.clone(),
                Remote {
                    bytes: Some(std::fs::metadata(original)?.len()),
                    id: id.clone(),
                    kind: RemoteKind::Zip,
                    password: password.clone(),
                    sha256: sha.to_string(),
                    source: input.source.clone(),
                    url: file.to_string(),
                },
            );
            Some((id, original, password))
        } else {
            None
        };

        for (m, r) in pick(&pairs, opts.seed, &input.source, opts.per_source)? {
            let cid = str_field(m, "crop_id")?.to_string();
            let truth = str_field(r, "transcription")?.to_string();
            let (image, transcription) = match ships {
                Ships::Crops => {
                    let src = input.dir.join(str_field(m, "image")?);
                    let ext = src.extension().and_then(|e| e.to_str()).unwrap_or("jpg");
                    let path = format!("crops/{cid}.{ext}");
                    std::fs::copy(&src, dir.join(&path))?;
                    let sha = sha256_file(&dir.join(&path))?;
                    ensure!(
                        sha == str_field(m, "crop_sha256")?,
                        "{cid}: crop bytes differ from manifest.jsonl"
                    );
                    (
                        CropImage::Bundled { path, sha256: sha },
                        CropText::Inline {
                            sha256: sha256_hex(truth.as_bytes()),
                            text: truth,
                        },
                    )
                }
                Ships::PageReferences => {
                    let url = m.get("page_image_url").and_then(Value::as_str)
                        .with_context(|| format!("{cid}: manifest.jsonl has no page_image_url (re-run gold.py public-convert)"))?;
                    let sha = str_field(m, "page_image_sha256")?;
                    let b = m.get("box").and_then(Value::as_array).context("no box")?;
                    let region: Vec<u32> = b
                        .iter()
                        .map(|x| x.as_u64().and_then(|x| u32::try_from(x).ok()))
                        .collect::<Option<_>>()
                        .context("bad box")?;
                    ensure!(
                        sha.len() == 64
                            && sha
                                .bytes()
                                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                        "{cid}: page_image_sha256 is not 64 lowercase hex digits"
                    );
                    let id = format!("{}-{}", input.source, &sha[..16]);
                    remotes.entry(id.clone()).or_insert_with(|| Remote {
                        bytes: None,
                        id: id.clone(),
                        kind: RemoteKind::PageImage,
                        password: None,
                        sha256: sha.to_string(),
                        source: input.source.clone(),
                        url: url.to_string(),
                    });
                    let region: [u32; 4] = region.try_into().ok().context("box needs 4 numbers")?;
                    (
                        CropImage::PageRegion { region, remote: id },
                        CropText::Inline {
                            sha256: sha256_hex(truth.as_bytes()),
                            text: truth,
                        },
                    )
                }
                Ships::References => {
                    let (id, original, password) = zip_remote.as_ref().context("no archive")?;
                    let password = password.as_deref();
                    let img = format!("Images/{}", str_field(m, "source_image")?);
                    let txt = format!("Ground Truth/{}.txt", str_field(m, "region")?);
                    let img_sha = sha256_hex(&zip_member(original, password, &img, MAX_REMOTE)?);
                    ensure!(
                        img_sha == str_field(m, "crop_sha256")?,
                        "{cid}: {img} differs from manifest.jsonl"
                    );
                    let t = read_text_member(&zip_member(original, password, &txt, 1 << 20)?)?;
                    ensure!(
                        t == truth,
                        "{cid}: {txt} differs from crops.jsonl's transcription"
                    );
                    (
                        CropImage::ZipMember {
                            member: img,
                            remote: id.clone(),
                            sha256: img_sha,
                        },
                        CropText::ZipMember {
                            member: txt,
                            remote: id.clone(),
                            sha256: sha256_hex(t.as_bytes()),
                        },
                    )
                }
            };
            crops.push(Crop {
                crop_id: cid,
                group: str_field(r, "group")?.to_string(),
                image,
                lang: str_field(m, "lang")?.to_string(),
                source: input.source.clone(),
                transcription,
            });
        }
    }
    crops.sort_by(|a, b| a.crop_id.cmp(&b.crop_id));
    let files = list_files(&dir)?;
    let manifest = Manifest {
        bundle_version: opts.version,
        crops,
        files,
        format: FORMAT.into(),
        remotes: remotes.into_values().collect(),
        seed: opts.seed,
        sources,
    };
    manifest.check()?;
    std::fs::write(dir.join(MANIFEST), to_json(&manifest)?)?;
    let tar = opts.out.join(format!("{}.tar", name(opts.version)));
    write_tar(&dir, &tar)?;
    let tar_sha256 = sha256_file(&tar)?;
    std::fs::write(
        opts.out.join(format!("{}.tar.sha256", name(opts.version))),
        format!("{tar_sha256}  {}.tar\n", name(opts.version)),
    )?;
    Ok(Built {
        dir,
        tar,
        tar_sha256,
        manifest,
    })
}

/// Every file under `dir` except manifest.json, sorted by path.
fn list_files(dir: &Path) -> anyhow::Result<Vec<FileEntry>> {
    fn walk(root: &Path, d: &Path, out: &mut Vec<FileEntry>) -> anyhow::Result<()> {
        for e in std::fs::read_dir(d)? {
            let p = e?.path();
            if p.is_dir() {
                walk(root, &p, out)?;
            } else {
                let rel = p
                    .strip_prefix(root)?
                    .to_str()
                    .context("non-UTF-8 path")?
                    .replace('\\', "/");
                if rel != MANIFEST {
                    out.push(FileEntry {
                        bytes: std::fs::metadata(&p)?.len(),
                        sha256: sha256_file(&p)?,
                        path: rel,
                    });
                }
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out)?;
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// A tar of the bundle directory with sorted entries and fixed metadata, so it is reproducible.
fn write_tar(dir: &Path, tar: &Path) -> anyhow::Result<()> {
    let top = dir
        .file_name()
        .and_then(|n| n.to_str())
        .context("bundle dir name")?;
    let mut paths: Vec<String> = list_files(dir)?.into_iter().map(|f| f.path).collect();
    paths.push(MANIFEST.into());
    paths.sort();
    let mut b = tar::Builder::new(std::fs::File::create(tar)?);
    for p in paths {
        let data = std::fs::read(dir.join(&p))?;
        let mut h = tar::Header::new_ustar();
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        h.set_mtime(0);
        h.set_uid(0);
        h.set_gid(0);
        h.set_entry_type(tar::EntryType::Regular);
        b.append_data(&mut h, format!("{top}/{p}"), data.as_slice())?;
    }
    b.into_inner()?.sync_all()?;
    Ok(())
}

// ------------------------------------------------------------------ client

/// Unpacks a bundle tar into `dest` (refusing absolute paths, `..` and links) and returns the
/// bundle directory.
pub fn unpack(tar: &Path, dest: &Path) -> anyhow::Result<PathBuf> {
    let mut a = tar::Archive::new(std::fs::File::open(tar)?);
    let mut total: u64 = 0;
    let mut top = None;
    for e in a.entries()? {
        let mut e = e?;
        ensure!(
            e.header().entry_type() == tar::EntryType::Regular,
            "bundle tar may hold only regular files"
        );
        total = total
            .checked_add(e.header().size()?)
            .filter(|t| *t <= MAX_BUNDLE)
            .context("bundle tar unpacks to more than the size limit")?;
        let p = e.path()?.to_path_buf();
        let s = p.to_str().context("non-UTF-8 path")?.to_string();
        ensure!(safe_path(&s), "unsafe path in tar: {s}");
        let first = s.split('/').next().unwrap_or_default().to_string();
        ensure!(
            top.get_or_insert_with(|| first.clone()) == &first,
            "bundle tar has more than one top directory"
        );
        let out = dest.join(&p);
        std::fs::create_dir_all(out.parent().context("path")?)?;
        e.unpack(&out)?;
    }
    Ok(dest.join(top.context("empty bundle tar")?))
}

/// Reads and checks a bundle directory: the manifest, then every file's size and sha256. Files the
/// manifest doesn't list are an error too.
pub fn verify(dir: &Path) -> anyhow::Result<Manifest> {
    let p = dir.join(MANIFEST);
    ensure!(
        std::fs::metadata(&p)?.len() <= MAX_MANIFEST,
        "manifest too large"
    );
    let m: Manifest =
        serde_json::from_slice(&std::fs::read(&p)?).context("parsing manifest.json")?;
    m.check()?;
    let on_disk = list_files(dir)?;
    ensure!(
        on_disk.len() == m.files.len(),
        "the bundle has {} files, the manifest lists {}",
        on_disk.len(),
        m.files.len()
    );
    for (a, b) in on_disk.iter().zip(&m.files) {
        ensure!(
            a == b,
            "{}: size or sha256 differs from the manifest",
            b.path
        );
    }
    for c in &m.crops {
        if let CropImage::Bundled { path, sha256 } = &c.image {
            ensure!(
                m.files
                    .iter()
                    .any(|f| &f.path == path && &f.sha256 == sha256),
                "{}: image checksum",
                c.crop_id
            );
        }
        if let CropText::Inline { sha256, text } = &c.transcription {
            ensure!(
                &sha256_hex(text.as_bytes()) == sha256,
                "{}: transcription checksum",
                c.crop_id
            );
        }
    }
    Ok(m)
}

/// Where a remote is kept in the client's cache.
pub fn cache_path(cache: &Path, r: &Remote) -> PathBuf {
    let ext = match r.kind {
        RemoteKind::Zip => "zip",
        RemoteKind::PageImage => "jpg",
    };
    cache.join(format!("{}.{ext}", r.sha256))
}

/// Whether a remote is in the cache with the right checksum.
pub fn cached(cache: &Path, r: &Remote) -> bool {
    let p = cache_path(cache, r);
    p.is_file() && sha256_file(&p).is_ok_and(|s| s == r.sha256)
}

/// What staging did with each remote.
#[derive(Debug, Default, Serialize)]
pub struct Staged {
    pub already: usize,
    pub fetched: usize,
    pub failed: Vec<(String, String)>,
}

/// Downloads the remotes that are not in the cache yet, one at a time with `gap` between requests,
/// and keeps each only if its sha256 matches. A failure is recorded, not fatal: the crops that need
/// that file are skipped at run time and reported as skipped.
pub async fn stage(m: &Manifest, cache: &Path, gap: std::time::Duration) -> anyhow::Result<Staged> {
    std::fs::create_dir_all(cache)?;
    let client = reqwest::Client::builder()
        .user_agent(concat!(
            "usnm-ocr-survey/",
            env!("CARGO_PKG_VERSION"),
            " (+https://github.com/tgoodyear/usnewsmap.com)"
        ))
        .timeout(std::time::Duration::from_secs(600))
        .build()?;
    let mut out = Staged::default();
    let mut first = true;
    for r in &m.remotes {
        if cached(cache, r) {
            out.already += 1;
            continue;
        }
        if !first {
            tokio::time::sleep(gap).await;
        }
        first = false;
        match fetch(&client, r).await {
            Ok(bytes) => {
                let p = cache_path(cache, r);
                let tmp = p.with_extension("part");
                std::fs::write(&tmp, &bytes)?;
                std::fs::rename(&tmp, &p)?;
                out.fetched += 1;
            }
            Err(e) => out.failed.push((r.id.clone(), format!("{e:#}"))),
        }
    }
    Ok(out)
}

async fn fetch(client: &reqwest::Client, r: &Remote) -> anyhow::Result<Vec<u8>> {
    let mut resp = client.get(&r.url).send().await?.error_for_status()?;
    let mut bytes = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        bytes.extend_from_slice(&chunk);
        ensure!(bytes.len() as u64 <= MAX_REMOTE, "over {MAX_REMOTE} bytes");
    }
    let sha = sha256_hex(&bytes);
    ensure!(
        sha == r.sha256,
        "sha256 {sha} differs from the manifest's {}",
        r.sha256
    );
    Ok(bytes)
}

/// A crop ready to run: an image file on disk and its transcription.
#[derive(Clone, Debug)]
pub struct Ready {
    pub crop_id: String,
    pub source: String,
    pub group: String,
    pub lang: String,
    pub image: PathBuf,
    pub transcription: String,
}

/// A crop that could not be prepared, and why.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Skipped {
    pub crop_id: String,
    pub reason: String,
}

/// Prepares every crop: bundled crops are used in place, archive members are extracted unchanged
/// and checked, and page regions are cut from the cached page images into `work` as PNG.
pub fn materialize(
    m: &Manifest,
    bundle: &Path,
    cache: &Path,
    work: &Path,
    limit: Option<usize>,
) -> anyhow::Result<(Vec<Ready>, Vec<Skipped>)> {
    std::fs::create_dir_all(work)?;
    let limit = limit.unwrap_or(usize::MAX);
    let mut ready = Vec::new();
    let mut skipped = Vec::new();
    // Each remote is hashed once per call, not once per crop that uses it.
    let mut valid: BTreeMap<String, bool> = BTreeMap::new();
    for (n, c) in m.crops.iter().enumerate() {
        if n >= limit {
            skipped.push(Skipped {
                crop_id: c.crop_id.clone(),
                reason: "not run: over --limit".into(),
            });
            continue;
        }
        match prepare(m, c, bundle, cache, work, &mut valid) {
            Ok(r) => ready.push(r),
            Err(e) => skipped.push(Skipped {
                crop_id: c.crop_id.clone(),
                reason: format!("{e:#}"),
            }),
        }
    }
    Ok((ready, skipped))
}

fn remote_file<'a>(
    m: &'a Manifest,
    cache: &Path,
    id: &str,
    valid: &mut BTreeMap<String, bool>,
) -> anyhow::Result<(PathBuf, Option<&'a str>)> {
    let r = m.remote(id)?;
    let ok = *valid
        .entry(id.to_string())
        .or_insert_with(|| cached(cache, r));
    ensure!(
        ok,
        "{id} is not staged (run the stage step) or its checksum differs"
    );
    Ok((cache_path(cache, r), r.password.as_deref()))
}

fn prepare(
    m: &Manifest,
    c: &Crop,
    bundle: &Path,
    cache: &Path,
    work: &Path,
    valid: &mut BTreeMap<String, bool>,
) -> anyhow::Result<Ready> {
    let transcription = match &c.transcription {
        CropText::Inline { text, .. } => text.clone(),
        CropText::ZipMember {
            member,
            remote,
            sha256,
        } => {
            let (zip, password) = remote_file(m, cache, remote, valid)?;
            let t = read_text_member(&zip_member(&zip, password, member, 1 << 20)?)?;
            ensure!(
                &sha256_hex(t.as_bytes()) == sha256,
                "{member}: transcription checksum differs"
            );
            t
        }
    };
    let image = match &c.image {
        CropImage::Bundled { path, .. } => bundle.join(path),
        CropImage::ZipMember {
            member,
            remote,
            sha256,
        } => {
            let (zip, password) = remote_file(m, cache, remote, valid)?;
            let bytes = zip_member(&zip, password, member, MAX_REMOTE)?;
            ensure!(&sha256_hex(&bytes) == sha256, "{member}: checksum differs");
            let ext = Path::new(member)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("img");
            let p = work.join(format!("{}.{ext}", c.crop_id));
            std::fs::write(&p, bytes)?;
            p
        }
        CropImage::PageRegion { region, remote } => {
            let page = image::open(remote_file(m, cache, remote, valid)?.0)
                .context("decoding the page image")?;
            let [x0, y0, x1, y1] = *region;
            ensure!(
                x1 <= page.width() && y1 <= page.height(),
                "box outside the page image"
            );
            let p = work.join(format!("{}.png", c.crop_id));
            page.crop_imm(x0, y0, x1 - x0, y1 - y0)
                .to_luma8()
                .save(&p)?;
            p
        }
    };
    Ok(Ready {
        crop_id: c.crop_id.clone(),
        source: c.source.clone(),
        group: c.group.clone(),
        lang: c.lang.clone(),
        image,
        transcription,
    })
}

/// One crop's score, as reported by a volunteer: counts only, no text.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CropResult {
    pub crop_id: String,
    pub source: String,
    #[serde(flatten)]
    pub counts: score::Counts,
}

/// Per source and overall, CER and WER over the crops that ran.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SourceSummary {
    pub cer: Option<f64>,
    pub crops: usize,
    pub ref_chars: usize,
    pub source: String,
    pub wer: Option<f64>,
}

pub fn summarize(results: &[CropResult]) -> Vec<SourceSummary> {
    let sources: BTreeSet<&str> = results.iter().map(|r| r.source.as_str()).collect();
    std::iter::once(None)
        .chain(sources.into_iter().map(Some))
        .filter_map(|s| {
            let rs: Vec<_> = results
                .iter()
                .filter(|r| s.is_none_or(|s| r.source == s))
                .collect();
            if rs.is_empty() {
                return None;
            }
            let c: usize = rs.iter().map(|r| r.counts.ref_chars).sum();
            let w: usize = rs.iter().map(|r| r.counts.ref_words).sum();
            let ce: usize = rs.iter().map(|r| r.counts.char_edits).sum();
            let we: usize = rs.iter().map(|r| r.counts.word_edits).sum();
            Some(SourceSummary {
                cer: (c > 0).then(|| score::round4(ce as f64 / c as f64)),
                crops: rs.len(),
                ref_chars: c,
                source: s.unwrap_or("all").to_string(),
                wer: (w > 0).then(|| score::round4(we as f64 / w as f64)),
            })
        })
        .collect()
}

/// A lock file for the repository: what a build produced, without any dataset text.
pub fn lock(b: &Built) -> anyhow::Result<Value> {
    let m = &b.manifest;
    let mut per_source: BTreeMap<&str, usize> = BTreeMap::new();
    for c in &m.crops {
        *per_source.entry(c.source.as_str()).or_default() += 1;
    }
    Ok(serde_json::json!({
        "bundle_version": m.bundle_version,
        "crops": m.crops.iter().map(|c| &c.crop_id).collect::<Vec<_>>(),
        "crops_per_source": per_source,
        "files": m.files,
        "format": m.format,
        "manifest_sha256": m.digest()?,
        "remotes": m.remotes,
        "seed": m.seed,
        "tar_bytes": std::fs::metadata(&b.tar)?.len(),
        "tar_sha256": b.tar_sha256,
    }))
}

#[cfg(test)]
mod sha_tests {
    #[test]
    fn remote_checksums_must_be_full_lowercase_hex() {
        assert!(super::is_sha256_hex(&"a".repeat(64)));
        for s in [
            "a".repeat(63),
            "a".repeat(65),
            "A".repeat(64),
            "z".repeat(64),
        ] {
            assert!(!super::is_sha256_hex(&s), "{s}");
        }
    }
}
