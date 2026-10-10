//! The on-prem OCR worker (#296): takes pages from Azure Storage Queues,
//! fetches their scans from LoC, reads them with a local OCR engine and
//! writes ALTO and an overlay part to the exchange storage account. It holds
//! no LoC text and makes no keep-or-drop decision; a cloud job checks the
//! parts and the release decides.

pub mod engine;
pub mod loc;
pub mod message;
pub mod queue;
pub mod telemetry;
pub mod worker;

#[cfg(test)]
mod testimg;
