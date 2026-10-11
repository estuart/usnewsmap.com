//! Adds `text_cg` (05 §5.5.3) to index documents: JSON lines on stdin, each
//! with a `text`, to stdout with `text_cg` computed as the release does, and
//! `text_as_cg` for those with American Stories' `text_as` (05 §5.5.4).
//! scripts/quickwit-fixtures.sh loads the fixture indexes through it.
//!
//! `add_text_cg [--common-grams-version N]`: the pairs at `common_grams`
//! version `N`, by default the latest (`usnm_core::common_grams::VERSION`).

use std::io::{self, BufRead, Write};

fn main() -> io::Result<()> {
    let usage = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: add_text_cg [--common-grams-version N]",
        )
    };
    let mut args = std::env::args().skip(1).peekable();
    // The positional form (`add_text_cg N`) works for one release, with a
    // warning (#277).
    let mut version = None;
    if let Some(v) = args.next_if(|a| !a.starts_with('-')) {
        eprintln!("warning: `add_text_cg N` is deprecated; use --common-grams-version N (#277)");
        version = Some(v);
    }
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--common-grams-version" => version = Some(args.next().ok_or_else(usage)?),
            _ => return Err(usage()),
        }
    }
    let version = match version {
        Some(v) => v
            .parse()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("{v}: {e}")))?,
        None => usnm_core::common_grams::VERSION,
    };
    let analyzer = usnm_core::common_grams::analyzer(version).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown common_grams version {version}"),
        )
    })?;
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    for line in io::stdin().lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let mut doc: serde_json::Value = serde_json::from_str(&line)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        if let Some(text) = doc["text"].as_str() {
            doc["text_cg"] = usnm_core::common_grams::index_text(text, analyzer).into();
        }
        // American Stories' text (05 §5.5.4) gets its own pairs.
        if let Some(text) = doc["text_as"].as_str() {
            doc["text_as_cg"] = usnm_core::common_grams::index_text(text, analyzer).into();
        }
        serde_json::to_writer(&mut out, &doc)?;
        out.write_all(b"\n")?;
    }
    out.flush()
}
