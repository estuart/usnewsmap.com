//! The gold bundle: built reproducibly, verified, and materialized for each way a source ships.
use serde_json::json;
use std::io::Write;
use std::path::Path;
use usnm_ocr_survey::bundle::{self, BuildOptions, CropImage, CropText, Input, Ships};

fn write_jsonl(p: &Path, rows: &[serde_json::Value]) {
    let s: String = rows.iter().map(|r| format!("{r}\n")).collect();
    std::fs::write(p, s).unwrap();
}

fn sha(b: &[u8]) -> String {
    bundle::sha256_hex(b)
}

fn jpeg(w: u32, h: u32) -> Vec<u8> {
    let img = image::GrayImage::from_fn(w, h, |x, y| image::Luma([((x * 7 + y * 3) % 251) as u8]));
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Jpeg).unwrap();
    out.into_inner()
}

fn crop_row(cid: &str, group: &str, text: &str) -> serde_json::Value {
    json!({"crop_id": cid, "group": group, "lang": "eng", "transcription": text,
           "transcriber": "x: published ground truth", "reviewer": "not applicable: published ground truth",
           "hint_copy_reviewed": false, "hints": {"texts": {}}, "for_scoring": true, "image": "images/a.jpg"})
}

fn manifest_row(
    cid: &str,
    source: &str,
    page: &str,
    extra: serde_json::Value,
) -> serde_json::Value {
    let mut m = json!({"crop_id": cid, "page_id": page, "lang": "eng", "source_dataset": source,
                       "source_url": "https://example.org/data", "source_license": "CC-BY-4.0 (record)",
                       "source_attribution": format!("{source} by its makers"), "image": "images/a.jpg"});
    m.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    m
}

fn licences(dir: &Path, note: serde_json::Value) {
    std::fs::create_dir_all(dir.join("LICENSES")).unwrap();
    std::fs::write(dir.join("LICENSES/SOURCE.json"), note.to_string()).unwrap();
    std::fs::write(dir.join("LICENSES/LICENSE"), "licence text\n").unwrap();
}

struct Fixture {
    _tmp: tempfile::TempDir,
    root: std::path::PathBuf,
    page: Vec<u8>,
}

/// Three sources: shipped crops, a page-region reference and an encrypted-zip reference.
fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    // Crops that ship.
    let a = root.join("newseye");
    std::fs::create_dir_all(a.join("images")).unwrap();
    let mut man = Vec::new();
    let mut rows = Vec::new();
    for i in 0..5 {
        let cid = format!("pub-newseye-bnf-{i:03}");
        let img = jpeg(20 + i, 10);
        std::fs::write(a.join(format!("images/{cid}.jpg")), &img).unwrap();
        man.push(manifest_row(
            &cid,
            "newseye-bnf",
            &format!("p{}", i % 2),
            json!({"image": format!("images/{cid}.jpg"), "crop_sha256": sha(&img)}),
        ));
        let mut r = crop_row(&cid, "pub-newseye-bnf", &format!("texte numéro {i}"));
        r["image"] = json!(format!("images/{cid}.jpg"));
        rows.push(r);
    }
    // One row that is not for scoring is never picked.
    let mut r = crop_row("pub-newseye-bnf-999", "pub-newseye-bnf", "nope");
    r["for_scoring"] = json!(false);
    rows.push(r);
    man.push(manifest_row(
        "pub-newseye-bnf-999",
        "newseye-bnf",
        "p9",
        json!({"crop_sha256": "0"}),
    ));
    write_jsonl(&a.join("manifest.jsonl"), &man);
    write_jsonl(&a.join("crops.jsonl"), &rows);
    licences(&a, json!({"dataset": "newseye-bnf"}));

    // A page reference: the box is cut on the client from the page image.
    let page = jpeg(64, 48);
    let b = root.join("overproof");
    std::fs::create_dir_all(&b).unwrap();
    write_jsonl(
        &b.join("manifest.jsonl"),
        &[manifest_row(
            "pub-overproof-ca-001",
            "overproof-ca",
            "pg",
            json!({"box": [4, 8, 36, 30], "page_image_sha256": sha(&page),
               "page_image_url": "https://tile.loc.gov/x/full/full/0/default.jpg"}),
        )],
    );
    write_jsonl(
        &b.join("crops.jsonl"),
        &[crop_row(
            "pub-overproof-ca-001",
            "pub-overproof-ca",
            "line one\nline two",
        )],
    );
    licences(&b, json!({"dataset": "overproof-ca"}));

    // An archive reference: nothing from the archive ships.
    let tif = b"not really a tiff".to_vec();
    let zpath = root.join("BLN600.zip");
    {
        let mut z = zip::ZipWriter::new(std::fs::File::create(&zpath).unwrap());
        let o = zip::write::SimpleFileOptions::default()
            .with_aes_encryption(zip::AesMode::Aes256, "BLN600");
        z.start_file("Images/123.tif", o).unwrap();
        z.write_all(&tif).unwrap();
        z.start_file("Ground Truth/123.txt", o).unwrap();
        z.write_all("\u{feff}THE COURT.\r\nThe Queen \r\n".as_bytes())
            .unwrap();
        z.finish().unwrap();
    }
    let zbytes = std::fs::read(&zpath).unwrap();
    let c = root.join("bln");
    std::fs::create_dir_all(&c).unwrap();
    let mut m = manifest_row(
        "pub-bln600-001",
        "bln600",
        "bln600-123",
        json!({"crop_sha256": sha(&tif), "source_image": "123.tif", "region": "123", "image": "images/pub-bln600-001.tif"}),
    );
    m["source_license"] = json!("CC BY-NC-ND 4.0");
    write_jsonl(&c.join("manifest.jsonl"), &[m]);
    write_jsonl(
        &c.join("crops.jsonl"),
        &[crop_row(
            "pub-bln600-001",
            "pub-bln600",
            "THE COURT.\nThe Queen",
        )],
    );
    licences(
        &c,
        json!({"dataset": "bln600", "file": "https://ndownloader.figshare.com/files/1", "sha256": sha(&zbytes)}),
    );
    Fixture {
        _tmp: tmp,
        root,
        page,
    }
}

fn opts(f: &Fixture, out: &str) -> BuildOptions {
    BuildOptions {
        inputs: vec![
            Input {
                source: "newseye-bnf".into(),
                dir: f.root.join("newseye"),
                original: None,
            },
            Input {
                source: "overproof-ca".into(),
                dir: f.root.join("overproof"),
                original: None,
            },
            Input {
                source: "bln600".into(),
                dir: f.root.join("bln"),
                original: Some(f.root.join("BLN600.zip")),
            },
        ],
        version: 3,
        seed: 457,
        per_source: 4,
        out: f.root.join(out),
    }
}

#[test]
fn builds_reproducibly_and_respects_the_licences() {
    let f = fixture();
    let a = bundle::build(&opts(&f, "a")).unwrap();
    let b = bundle::build(&opts(&f, "b")).unwrap();
    assert_eq!(a.tar_sha256, b.tar_sha256, "same inputs, same bytes");
    let m = &a.manifest;
    assert_eq!(m.bundle_version, 3);
    assert_eq!(
        m.crops.len(),
        4 + 1 + 1,
        "per_source caps the shipped source"
    );
    assert!(m.crops.iter().all(|c| c.crop_id != "pub-newseye-bnf-999"));
    assert_eq!(m.sources["bln600"].ships, Ships::References);
    assert_eq!(m.sources["overproof-ca"].ships, Ships::PageReferences);
    assert_eq!(m.sources["newseye-bnf"].ships, Ships::Crops);
    // Nothing derived from the No-Derivatives archive is in the bundle: no file, no text.
    assert!(m
        .files
        .iter()
        .all(|f| !f.path.starts_with("crops/pub-bln600")));
    let bln = m.crops.iter().find(|c| c.source == "bln600").unwrap();
    assert!(matches!(bln.image, CropImage::ZipMember { .. }));
    assert!(matches!(bln.transcription, CropText::ZipMember { .. }));
    let manifest_text = std::fs::read_to_string(a.dir.join("manifest.json")).unwrap();
    assert!(!manifest_text.contains("THE COURT"));
    // Licence and attribution files travel with every source.
    for s in m.sources.values() {
        assert!(s
            .license_files
            .iter()
            .any(|p| p.ends_with("ATTRIBUTION.txt")));
        assert!(s.license_files.iter().any(|p| p.ends_with("LICENSE")));
    }

    // Client side: unpack the tar, verify, then materialize from a cache.
    let unpacked = bundle::unpack(&a.tar, &f.root.join("client")).unwrap();
    let m2 = bundle::verify(&unpacked).unwrap();
    assert_eq!(m2.digest().unwrap(), m.digest().unwrap());
    let cache = f.root.join("cache");
    std::fs::create_dir_all(&cache).unwrap();
    let (ready, skipped) =
        bundle::materialize(&m2, &unpacked, &cache, &f.root.join("work"), None).unwrap();
    assert_eq!(ready.len(), 4);
    assert_eq!(skipped.len(), 2, "remotes not staged yet: {skipped:?}");
    for r in &m2.remotes {
        let bytes = match r.kind {
            bundle::RemoteKind::Zip => std::fs::read(f.root.join("BLN600.zip")).unwrap(),
            bundle::RemoteKind::PageImage => f.page.clone(),
        };
        std::fs::write(bundle::cache_path(&cache, r), bytes).unwrap();
    }
    let (ready, skipped) =
        bundle::materialize(&m2, &unpacked, &cache, &f.root.join("work"), None).unwrap();
    assert!(skipped.is_empty(), "{skipped:?}");
    let op = ready.iter().find(|r| r.source == "overproof-ca").unwrap();
    let cut = image::open(&op.image).unwrap();
    assert_eq!((cut.width(), cut.height()), (32, 22));
    let bln = ready.iter().find(|r| r.source == "bln600").unwrap();
    assert_eq!(bln.transcription, "THE COURT.\nThe Queen");
    assert_eq!(std::fs::read(&bln.image).unwrap(), b"not really a tiff");
}

#[test]
fn verify_rejects_tampering() {
    let f = fixture();
    let a = bundle::build(&opts(&f, "a")).unwrap();
    let crop = a
        .manifest
        .files
        .iter()
        .find(|x| x.path.starts_with("crops/"))
        .unwrap();
    let original = std::fs::read(a.dir.join(&crop.path)).unwrap();
    std::fs::write(a.dir.join(&crop.path), b"changed").unwrap();
    assert!(bundle::verify(&a.dir).is_err());
    std::fs::write(a.dir.join(&crop.path), original).unwrap();
    assert!(bundle::verify(&a.dir).is_ok());
    std::fs::write(a.dir.join("extra.txt"), b"x").unwrap();
    assert!(bundle::verify(&a.dir).is_err());
}

#[test]
fn refuses_sources_without_a_policy_or_attribution() {
    let f = fixture();
    let mut o = opts(&f, "a");
    o.inputs[0].source = "enp-berlin".into();
    assert!(bundle::build(&o).is_err());
    let p = f.root.join("newseye/manifest.jsonl");
    let s = std::fs::read_to_string(&p).unwrap().replace(
        "\"source_attribution\":\"newseye-bnf by its makers\"",
        "\"source_attribution\":null",
    );
    std::fs::write(&p, s).unwrap();
    let err = bundle::build(&opts(&f, "b")).err().unwrap();
    assert!(format!("{err:#}").contains("source_attribution"), "{err:#}");
}
