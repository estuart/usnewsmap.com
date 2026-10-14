//! The on-prem OCR pipeline's wire formats (#296, 13 §13.2): the work
//! message a cloud job puts on `ocr-priority` or `ocr-bulk`, the dead
//! letter a node puts on `ocr-deadletter`, the list of pages whose scans
//! are fetched ahead of the workers (`prefetch`, #368), and the list of
//! those already fetched that a node relays to the seeder (`fetched`). The
//! worker writes and reads them; `usnm-ingest`'s `ocr-sync`, `ocr-enqueue` and
//! `ocr-prefetch-manifest` read and write them from the cloud side, so both
//! sides check them with the same code. `status` is what the LAN dashboard
//! reads from a worker and a Proxmox host (12 §12.2.5). `engine` is the
//! engine contract between the worker and an engine adapter (ADR-0016,
//! 13 §13.4), and `runs` the runs record each written page gets (13 §13.8.3).

pub mod engine;
pub mod fetched;
pub mod message;
pub mod prefetch;
pub mod runs;
pub mod status;

/// File-based jobs for the opt-in stock BOINC pilot.
pub mod boinc;

/// File-based jobs for the JP2 against IIIF JPEG assessment (#456).
pub mod assess;

/// File-based jobs for the Surya 2 test on BOINC volunteers (#516).
pub mod surya;
