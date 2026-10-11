//! The fixture indexes with page languages (05 §5.5.8, #289), for
//! scripts/quickwit-fixtures.sh and the parity test:
//!
//! - `page_language_fixture template`: the index template on stdin, with
//!   the `page_language` and `page_language_set` fields, as a release
//!   creates the index.
//! - `page_language_fixture docs`: fixture documents on stdin, with their
//!   page languages (`fixtures/page_language.rs`), `text_cg` and
//!   `text_as_cg` as a release writes them.

use std::io::{self, BufRead, Read, Write};

#[path = "../../../fixtures/page_language.rs"]
mod page_language;

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
        ["template"] => {
            let mut template = String::new();
            io::stdin().read_to_string(&mut template)?;
            let yaml = usnm_ingest::sink::page_language_fields(true, &template)?;
            out.write_all(yaml.as_bytes())?;
        }
        ["docs"] => {
            for line in io::stdin().lock().lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                let mut doc: serde_json::Value = serde_json::from_str(&line)?;
                page_language::add(&mut doc);
                for (from, to) in [("text", "text_cg"), ("text_as", "text_as_cg")] {
                    if let Some(text) = doc[from].as_str() {
                        doc[to] = usnm_core::common_grams::index_text(
                            text,
                            usnm_core::text::Analyzer::LATEST,
                        )
                        .into();
                    }
                }
                serde_json::to_writer(&mut out, &doc)?;
                out.write_all(b"\n")?;
            }
        }
        _ => anyhow::bail!("usage: page_language_fixture template | page_language_fixture docs"),
    }
    out.flush()?;
    Ok(())
}
