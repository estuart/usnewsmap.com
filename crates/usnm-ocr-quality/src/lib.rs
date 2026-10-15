//! Per-page OCR quality without transcription (#522). Everything here is a
//! pure function of its inputs, single-threaded and deterministic: two hosts
//! that run it on the same page produce the same bytes, which is what the
//! `usnm_quality` BOINC app's validator compares.
//!
//! - [`align`]: character and word alignment with a backtrace, the pairwise
//!   metrics between two readings (CER both ways, WER, word-bag Dice).
//! - [`hat`]: the three-cornered hat, each reader's own error rate from the
//!   pairwise disagreements.
//! - [`consensus`]: the words at least two readers agree on, and each
//!   reader's completeness and precision against them.
//! - [`findability`]: the share of the consensus content words and place
//!   names a reader's text contains.
//! - [`confusion`]: the character substitutions, insertions and deletions
//!   of a reader against the consensus.
//! - [`features`]: scan damage features from the page image, layout
//!   features from its ALTO.
//! - [`crops`]: the seeded, stratified crop selection the vision-language
//!   app reads.
//! - [`report`]: the per-page record, its JSON schema and rounding.

pub mod align;
pub mod confusion;
pub mod consensus;
pub mod crops;
pub mod features;
pub mod findability;
pub mod hat;
pub mod report;
