//! Writes the engine contract's JSON Schemas (13 §13.4.8):
//! `usnm-ocr-contract-schema --out-dir ocr/contract/v1`. A test in
//! `usnm-ocr-contract` fails when the files in the repository differ from
//! what this writes.

use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;

#[derive(Parser)]
#[command(about = "Write the engine contract's JSON Schemas")]
struct Args {
    /// Where to write `<name>.schema.json` (`ocr/contract/v1`).
    #[arg(long)]
    out_dir: PathBuf,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    std::fs::create_dir_all(&args.out_dir).with_context(|| args.out_dir.display().to_string())?;
    for (name, schema) in usnm_ocr_contract::engine::schemas() {
        let path = args.out_dir.join(format!("{name}.schema.json"));
        std::fs::write(&path, schema).with_context(|| path.display().to_string())?;
        println!("wrote {}", path.display());
    }
    Ok(())
}
