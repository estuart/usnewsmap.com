//! The Surya 2 test on BOINC volunteers (#516) as a BOINC app, `usnm_surya`.
//!
//! - [`stage`] gets each page's full-size IIIF JPEG (the scan archive first,
//!   then tile.loc.gov one request at a time; see [`crate::scans`]) and
//!   writes one unit directory per page.
//! - [`submit`] hands staged units to stock `create_work`, two results each.
//! - [`work`] runs one unit on a volunteer host, CPU or GPU, and always
//!   writes a result and its diagnostics.
//! - [`validate`] is what stock `script_validator` calls. It keeps a copy of
//!   every result it sees and logs every pair it compares, so the probe's
//!   timings and cross-host agreement survive BOINC's choice of a canonical
//!   result.
//! - [`summary`] reads those copies back into one table.
use anyhow::{bail, ensure, Context};
use std::path::{Path, PathBuf};

pub mod stage;
pub mod submit;
pub mod summary;
pub mod validate;
pub mod work;

/// The unit's request in the BOINC slot. Its presence makes `usnm-boinc work`
/// run a Surya unit.
pub const JOB_FILE: &str = "surya.json";
pub const RESULT_FILE: &str = "result.json";
pub const DIAG_FILE: &str = "diag.txt";
/// The page image's open name in the slot.
pub const PAGE_FILE: &str = "page.jpg";
pub const APP: &str = "usnm_surya";

/// One page to stage: its round 1 group, ISO 639-3 language, doc id
/// (`<lccn>_<date>_ed-<n>_seq-<n>`) and the batches that may hold it
/// (`<name>_ver<NN>`, joined with `+`, tried in order), given as
/// `group:lang:doc_id:batch[+batch...]`. The batches come from the
/// published set's `batches.json` (docs/boinc-surya.md): staging never asks
/// loc.gov's item API where a page is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub doc_id: String,
    pub group: String,
    pub lang: String,
    pub batches: Vec<(String, u16)>,
}
impl std::str::FromStr for Page {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> anyhow::Result<Self> {
        let mut it = s.splitn(4, ':');
        let (Some(group), Some(lang), Some(doc_id), Some(batches)) =
            (it.next(), it.next(), it.next(), it.next())
        else {
            bail!("a page is group:lang:doc_id:batch[+batch...], got {s:?}");
        };
        crate::assess::check_doc_id(doc_id)?;
        usnm_core::ids::PageKey::from_doc_id(doc_id)
            .with_context(|| format!("{doc_id:?} is not <lccn>_<date>_ed-<n>_seq-<n>"))?;
        let batches = batches
            .split('+')
            .map(|b| {
                crate::scans::split_batch(b)
                    .with_context(|| format!("{b:?} is not a batch as <name>_ver<NN>"))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        ensure!(batches.len() <= 8, "at most 8 batches for a page");
        Ok(Self {
            doc_id: doc_id.to_owned(),
            group: group.to_owned(),
            lang: lang.to_owned(),
            batches,
        })
    }
}

/// Where the units live under the staging root.
pub fn units(root: &Path) -> PathBuf {
    root.join("units")
}

/// A hold written when LoC refused or failed a request, as for the
/// assessment: staging will not start again until a person removes it.
pub fn check_no_hold(root: &Path) -> anyhow::Result<()> {
    crate::assess::check_no_hold(root)
}

/// The output files in output template order.
pub fn outputs() -> [&'static str; 5] {
    use usnm_ocr_contract::surya::InputMode;
    [
        InputMode::Tiled.text(),
        InputMode::Whole.text(),
        InputMode::Capped.text(),
        RESULT_FILE,
        DIAG_FILE,
    ]
}

/// Lower-case ASCII letters and hyphens, as a group or language.
pub fn check_word(s: &str, what: &str) -> anyhow::Result<()> {
    ensure!(
        !s.is_empty() && s.len() <= 32 && s.bytes().all(|b| b.is_ascii_lowercase() || b == b'-'),
        "invalid {what} {s:?}"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pages_parse() {
        let p: Page = "serbian:srp:sn85055101_1917-04-03_ed-1_seq-3:dlc_arlington_ver01"
            .parse()
            .unwrap();
        assert_eq!(p.group, "serbian");
        assert_eq!(p.batches, vec![("dlc_arlington".to_owned(), 1)]);
        let p: Page =
            "russian:rus:sn82015743_1931-04-10_ed-1_seq-4:iune_diamond_ver02+iune_garnet_ver01"
                .parse()
                .unwrap();
        assert_eq!(p.batches.len(), 2);
        assert!("serbian:srp".parse::<Page>().is_err());
        // No batch: staging has nowhere to look without the item API.
        assert!("serbian:srp:sn85055101_1917-04-03_ed-1_seq-3"
            .parse::<Page>()
            .is_err());
        assert!("serbian:srp:sn85055101_1917-04-03_ed-1_seq-3:dlc_arlington"
            .parse::<Page>()
            .is_err());
        assert!(
            "serbian:srp:sn85055101_1917-04-03_ed1_seq3:dlc_arlington_ver01"
                .parse::<Page>()
                .is_err()
        );
    }
}
