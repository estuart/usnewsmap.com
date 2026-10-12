//! The on-prem OCR pipeline's wire formats (#296, 13 §13.2): the work
//! message a cloud job puts on `ocr-priority` or `ocr-bulk`, the dead
//! letter a node puts on `ocr-deadletter`, the list of pages whose scans
//! are fetched ahead of the workers (`prefetch`, #368), and the list of
//! those already fetched that a node relays to the seeder (`fetched`). The
//! worker writes and reads them; `usnm-ingest`'s `ocr-sync`, `ocr-enqueue` and
//! `ocr-prefetch-manifest` read and write them from the cloud side, so both
//! sides check them with the same code.

pub mod fetched;
pub mod message;
pub mod prefetch;
