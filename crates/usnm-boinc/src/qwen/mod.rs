//! The vision-language readings (#522) as a BOINC app, `usnm_qwen`: one
//! page per unit, a stratified sample of crops cut from LoC's ALTO, each
//! transcribed by Qwen3-VL on llama.cpp's server. The model is a reader,
//! never a judge: the unit returns its text per crop and nothing else.
//!
//! - [`stage`] cuts the crops of each staged `usnm_quality` page (the same
//!   seed, so both apps agree on them) and writes one unit per page.
//! - [`submit`] hands staged units to stock `create_work`, two results each.
//! - [`work`] runs one unit on a volunteer host, GPU or CPU, and always
//!   writes the readings, a result and the diagnostics.
//! - [`validate`] is what stock `script_validator` calls: a copy of every
//!   result, and per-crop word-bag Dice between two hosts.
//! - [`summary`] reads those copies back into one table.
use anyhow::ensure;
use std::path::{Path, PathBuf};
use usnm_ocr_contract::qwen::Model;

pub mod stage;
pub mod submit;
pub mod summary;
pub mod validate;
pub mod work;

/// The unit's request in the BOINC slot. Its presence makes `usnm-boinc work`
/// run a Qwen unit.
pub const JOB_FILE: &str = "qwen.json";
pub const PAGE_FILE: &str = "page.jpg";
pub const READINGS_FILE: &str = "readings.json";
pub const RESULT_FILE: &str = "result.json";
pub const DIAG_FILE: &str = "diag.txt";
pub const APP: &str = "usnm_qwen";

/// The image records its llama.cpp base and weights lock digest; a unit
/// carries the same values and the host refuses one staged for another.
pub const ENV_LLAMA_IMAGE: &str = "USNM_QWEN_LLAMA_IMAGE";
pub const ENV_WEIGHTS: &str = "USNM_QWEN_WEIGHTS";
/// Where the image keeps the models (`ocr/boinc/qwen/weights.lock`).
pub const MODEL_DIR: &str = "/opt/qwen/models";

/// GPU memory, in MB, free on the host's GPU for the 8B model to be chosen.
pub const GPU_MB_FOR_8B: u32 = 7000;

/// The models in the image, largest first (`ocr/boinc/qwen/weights.lock`).
pub fn default_models() -> Vec<Model> {
    vec![
        Model {
            file: "Qwen3VL-8B-Instruct-Q4_K_M.gguf".into(),
            min_gpu_mb: GPU_MB_FOR_8B,
            mmproj: "mmproj-Qwen3VL-8B-Instruct-Q8_0.gguf".into(),
            name: "qwen3-vl-8b-q4km".into(),
        },
        Model {
            file: "Qwen3-VL-2B-Instruct-Q8_0.gguf".into(),
            min_gpu_mb: 0,
            mmproj: "mmproj-Qwen3-VL-2B-Instruct-Q8_0.gguf".into(),
            name: "qwen3-vl-2b-q8".into(),
        },
    ]
}

/// The model for a host: on a GPU with `free_mb` free, the first whose
/// `min_gpu_mb` fits; on CPU, the last (the smallest).
pub fn choose_model(models: &[Model], gpu_free_mb: Option<u64>) -> Option<&Model> {
    match gpu_free_mb {
        Some(free) => models
            .iter()
            .find(|m| u64::from(m.min_gpu_mb) <= free)
            .or(models.last()),
        None => models.last(),
    }
}

/// Where the units live under the staging root.
pub fn units(root: &Path) -> PathBuf {
    root.join("units")
}

pub fn check_no_hold(root: &Path) -> anyhow::Result<()> {
    crate::assess::check_no_hold(root)
}

/// The output files in output template order.
pub fn outputs() -> [&'static str; 3] {
    [READINGS_FILE, RESULT_FILE, DIAG_FILE]
}

/// Checks a digest argument.
pub fn check_digest(s: &str, what: &str) -> anyhow::Result<()> {
    ensure!(
        s.strip_prefix("sha256:")
            .is_some_and(usnm_ocr_contract::boinc::is_sha256),
        "{what} must be a full sha256 digest"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_choice() {
        let m = default_models();
        assert_eq!(
            choose_model(&m, Some(8000)).unwrap().name,
            "qwen3-vl-8b-q4km"
        );
        assert_eq!(choose_model(&m, Some(4000)).unwrap().name, "qwen3-vl-2b-q8");
        assert_eq!(choose_model(&m, None).unwrap().name, "qwen3-vl-2b-q8");
        assert!(choose_model(&[], None).is_none());
    }
}
