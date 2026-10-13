//! Generate a synthetic cached page for the documented local smoke test.
use chrono::{NaiveDate, Utc};
use clap::Parser;
use std::{path::PathBuf, sync::Arc};
use usnm_core::ids::PageKey;
use usnm_ingest::curated::{CuratedRow, PartWriter};
use usnm_ocr_contract::{
    boinc,
    prefetch::{self, ManifestRow},
};
use usnm_ocr_worker::{
    loc::{ImageSize, ScanFormat},
    prefetch::{scan_path, sidecar_path, Sidecar},
    testimg,
};
use usnm_state::{
    docs::FileDocs,
    state::{Curated, RunBatch, State},
};
#[derive(Parser)]
struct Args {
    #[arg(long)]
    dir: PathBuf,
}
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let root = args.dir;
    std::fs::create_dir_all(&root)?;
    let key = PageKey::new(
        "sn00000001",
        NaiveDate::from_ymd_opt(1900, 1, 1).unwrap(),
        1,
        1,
    )?;
    let row = ManifestRow::new(&key, "test_pilot", 1, Some("eng".into()), "detected", 0.5);
    let bytes = testimg::text_image("HELLO WORLD");
    let scan = scan_path(&root.join("scans"), &row, ImageSize::Full, ScanFormat::Jpeg);
    std::fs::create_dir_all(scan.parent().unwrap())?;
    std::fs::write(&scan, &bytes)?;
    let side = Sidecar {
        doc_id: key.doc_id(),
        batch: row.batch.clone(),
        batch_version: 1,
        loc_url:
            "https://tile.loc.gov/image-services/iiif/service:ndnp:test/full/full/0/default.jpg"
                .into(),
        iiif_base: "https://tile.loc.gov/image-services/iiif/service:ndnp:test".into(),
        resolution: "full".into(),
        fetched_at: Utc::now(),
        bytes: bytes.len() as u64,
        sha256: boinc::sha256(&bytes),
        download_ms: 0,
        user_agent: "synthetic-fixture".into(),
        format: "jpeg".into(),
        md5: None,
    };
    std::fs::write(sidecar_path(&scan), serde_json::to_vec_pretty(&side)?)?;
    std::fs::write(root.join("manifest.parquet"), prefetch::encode(&[row])?)?;
    std::fs::write(root.join("pages.txt"), format!("{key}\n"))?;
    let curated = usnm_store::open(root.join("curated").to_str().unwrap())?;
    let reference = usnm_store::open(root.join("reference").to_str().unwrap())?;
    let mut writer = PartWriter::new();
    writer.push(CuratedRow::from_ocr(
        key,
        "Old text",
        "test_pilot",
        1,
        "loc",
        Utc::now(),
    ))?;
    curated
        .put_new(
            "pages/test_pilot_ver01/part-0.parquet",
            writer.finish_part()?.unwrap(),
            "application/vnd.apache.parquet",
        )
        .await?;
    let batch = RunBatch {
        batch: "test_pilot".into(),
        curated: Curated {
            version: 1,
            parts: vec!["pages/test_pilot_ver01/part-0.parquet".into()],
            counts: String::new(),
            pages: 1,
            ok_pages: 1,
            lccns: vec!["sn00000001".into()],
            source_sha256: String::new(),
            first: String::new(),
            last: String::new(),
        },
    };
    reference
        .put_new(
            "v1/batches.json",
            serde_json::to_vec(&[batch])?,
            "application/json",
        )
        .await?;
    reference
        .put_new(
            "v1/titles.json",
            serde_json::to_vec(&serde_json::json!([{"lccn":"sn00000001","languages":["eng"]}]))?,
            "application/json",
        )
        .await?;
    State::new(Arc::new(FileDocs::open(root.join("state.json"))?))
        .set_current_version("v1")
        .await?;
    println!("{}", root.display());
    Ok(())
}
