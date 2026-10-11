//! The fixture indexes laid out by decade (05 §5.5.5, #123), for
//! scripts/quickwit-fixtures.sh and the parity test:
//!
//! - `decade_fixture --template partitioned|tagged`: the index template on
//!   stdin, with the `decade` field and its partitions (a full base) or its
//!   split tags (a delta), as a release creates the index.
//! - `decade_fixture --docs`: fixture documents on stdin, moved over several
//!   decades (`fixtures/decades.rs`), with `decade`, `text_cg` and
//!   `text_as_cg` as a release writes them.

use std::io::{self, BufRead, Read, Write};

use usnm_ingest::sink::Decades;

#[path = "../../../fixtures/decades.rs"]
mod decades;

fn main() -> anyhow::Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    // The positional form (`template LAYOUT`, `docs`) works for one release,
    // with a warning (#277).
    if let Some(first) = args
        .first_mut()
        .filter(|a| *a == "template" || *a == "docs")
    {
        eprintln!("warning: `decade_fixture {first}` is deprecated; use --{first} (#277)");
        *first = format!("--{first}");
    }
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["--template", layout] => {
            let layout = match *layout {
                "partitioned" => Decades::Partitioned,
                "tagged" => Decades::Tagged,
                other => anyhow::bail!("unknown layout `{other}`: partitioned or tagged"),
            };
            let mut template = String::new();
            io::stdin().read_to_string(&mut template)?;
            out.write_all(layout.apply(&template)?.as_bytes())?;
        }
        ["--docs"] => {
            for line in io::stdin().lock().lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                let mut doc: serde_json::Value = serde_json::from_str(&line)?;
                decades::shift(&mut doc);
                if let Some(text) = doc["text"].as_str() {
                    doc["text_cg"] = usnm_core::common_grams::index_text(
                        text,
                        usnm_core::text::Analyzer::LATEST,
                    )
                    .into();
                }
                if let Some(text) = doc["text_as"].as_str() {
                    doc["text_as_cg"] = usnm_core::common_grams::index_text(
                        text,
                        usnm_core::text::Analyzer::LATEST,
                    )
                    .into();
                }
                serde_json::to_writer(&mut out, &doc)?;
                out.write_all(b"\n")?;
            }
        }
        _ => {
            anyhow::bail!(
                "usage: decade_fixture --template partitioned|tagged | decade_fixture --docs"
            )
        }
    }
    out.flush()?;
    Ok(())
}
