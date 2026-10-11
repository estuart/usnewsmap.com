//! The on-prem OCR pipeline's wire formats (#296, 13 §13.2): the work
//! message a cloud job puts on `ocr-priority` or `ocr-bulk`, and the dead
//! letter a node puts on `ocr-deadletter`. The worker writes and reads them;
//! `usnm-ingest`'s `ocr-sync` and `ocr-enqueue` read and write them from the
//! cloud side, so both sides check them with the same code.

pub mod message;
