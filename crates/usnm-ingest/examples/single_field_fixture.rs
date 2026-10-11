//! The fixture indexes with one searched field for both texts (05 §5.5.7,
//! #283), for scripts/quickwit-fixtures.sh and the parity test:
//!
//! - `single_field_fixture --template`: the index template on stdin, with
//!   the text fields of `TextLayout::Single`, as a release creates the index.
//! - `single_field_fixture --docs`: fixture documents on stdin, with
//!   `text_all` and `text_all_cg` as a release writes them
//!   (`usnm_core::text_layout::index_fields`); `text` and `text_as` stay.

use std::io::{self, BufRead, Read, Write};

use usnm_core::text::Analyzer;
use usnm_core::text_layout::{self, TextLayout};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["--template"] => {
            let mut template = String::new();
            io::stdin().read_to_string(&mut template)?;
            let yaml = usnm_ingest::sink::text_fields(TextLayout::Single, &template)?;
            out.write_all(yaml.as_bytes())?;
        }
        ["--docs"] => {
            for line in io::stdin().lock().lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                let mut doc: serde_json::Value = serde_json::from_str(&line)?;
                let (words, pairs) = text_layout::index_fields(
                    doc["text"].as_str().unwrap_or_default(),
                    doc["text_as"].as_str(),
                    Analyzer::LATEST,
                );
                doc[text_layout::FIELD] = words.into();
                doc[text_layout::PAIRS_FIELD] = pairs.into();
                serde_json::to_writer(&mut out, &doc)?;
                out.write_all(b"\n")?;
            }
        }
        _ => anyhow::bail!("usage: single_field_fixture --template | single_field_fixture --docs"),
    }
    out.flush()?;
    Ok(())
}
