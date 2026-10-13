//! The OCR gold set in Rust (#296, #457): text normalization and CER/WER scoring that give the same
//! numbers as `scripts/ocr-survey/gold.py`, and the versioned, checksummed bundle of public ground
//! truth that BOINC volunteers fetch, verify, materialize and score locally. docs/ocr-gold-set.md
//! has the rules and the source policy.

pub mod bundle;
pub mod distance;
pub mod score;
pub mod text;
