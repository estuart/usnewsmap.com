//! The on-prem OCR worker (#296): takes pages from Azure Storage Queues,
//! fetches their scans from LoC, reads them with a local OCR engine and
//! writes ALTO and an overlay part to the exchange storage account. It holds
//! no LoC text and makes no keep-or-drop decision; a cloud job checks the
//! parts and the release decides.

pub mod engine;
pub mod loc;
/// The work message and dead letter, from `usnm-ocr-contract` (shared
/// with the cloud jobs that fill and drain the queues).
pub use usnm_ocr_contract::message;
pub mod queue;
pub mod telemetry;
pub mod worker;

pub mod testimg;
