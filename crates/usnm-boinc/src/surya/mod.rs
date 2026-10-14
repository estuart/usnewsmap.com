//! The Surya 2 test on BOINC volunteers (#516) as a BOINC app, `usnm_surya`.
//!
//! - [`stage`] fetches each page's full-size IIIF JPEG from LoC, one request
//!   at a time, and writes one unit directory per page.
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

/// One page to stage: its round 1 group, ISO 639-3 language and doc id
/// (`<lccn>_<date>_ed-<n>_seq-<n>`), given as `group:lang:doc_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub doc_id: String,
    pub group: String,
    pub lang: String,
}
impl std::str::FromStr for Page {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> anyhow::Result<Self> {
        let mut it = s.splitn(3, ':');
        let (Some(group), Some(lang), Some(doc_id)) = (it.next(), it.next(), it.next()) else {
            bail!("a page is group:lang:doc_id, got {s:?}");
        };
        crate::assess::check_doc_id(doc_id)?;
        usnm_core::ids::PageKey::from_doc_id(doc_id)
            .with_context(|| format!("{doc_id:?} is not <lccn>_<date>_ed-<n>_seq-<n>"))?;
        Ok(Self {
            doc_id: doc_id.to_owned(),
            group: group.to_owned(),
            lang: lang.to_owned(),
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
        let p: Page = "serbian:srp:sn85055101_1917-04-03_ed-1_seq-3"
            .parse()
            .unwrap();
        assert_eq!(p.group, "serbian");
        assert!("serbian:srp".parse::<Page>().is_err());
        assert!("serbian:srp:sn85055101_1917-04-03_ed1_seq3"
            .parse::<Page>()
            .is_err());
    }
}
