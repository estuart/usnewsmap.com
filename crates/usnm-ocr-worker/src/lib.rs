//! The on-prem OCR worker (#296): takes pages from Azure Storage Queues,
//! fetches their scans from LoC, reads them with a local OCR engine and
//! writes ALTO and an overlay part to the exchange storage account. It holds
//! no LoC text and makes no keep-or-drop decision; a cloud job checks the
//! parts and the release decides.

/// Scans read from the prefetch cache before LoC (#368).
pub mod cache;
pub mod engine;
pub mod loc;
/// Fetching scans ahead of the workers (#368).
pub mod prefetch;
/// The work message and dead letter, from `usnm-ocr-contract` (shared
/// with the cloud jobs that fill and drain the queues).
pub use usnm_ocr_contract::message;
pub mod queue;
pub mod telemetry;
pub mod worker;

pub mod testimg;
