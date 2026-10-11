//! Pipeline state (05 §5.9.1): the document store behind the ingest
//! pipeline's work queue, locks and index runs, and the typed items in it.
//!
//! The ingest pipeline writes this state. The API reads it, read-only, for
//! the public status page, which is why it lives apart from the pipeline's
//! heavier dependencies.

pub mod cosmos;
pub mod docs;
pub mod ocr_local;
pub mod state;
pub mod summary;

/// The Cosmos DB database that holds every container.
pub const DATABASE: &str = "usnm";
