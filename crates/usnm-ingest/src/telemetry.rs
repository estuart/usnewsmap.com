//! The pipeline's telemetry (08 §8.1.2): the shared setup in
//! `usnm-telemetry` (JSON logs on stdout; spans and metrics to Application
//! Insights when `APPLICATIONINSIGHTS_CONNECTION_STRING` is set), under the
//! cloud role `usnm-ingest`, and the pipeline's own instruments.

use std::sync::OnceLock;

use opentelemetry::metrics::{Counter, Gauge, Histogram};
pub use usnm_telemetry::{Telemetry, CONNECTION_STRING_VAR};

/// The cloud role the jobs report as.
pub const SERVICE: usnm_telemetry::Service = usnm_telemetry::Service {
    name: "usnm-ingest",
    version: env!("CARGO_PKG_VERSION"),
};

/// Install logging and, if configured, the exporters. Call
/// [`Telemetry::shutdown`] before the process exits: jobs are short-lived,
/// and whatever is still buffered is lost otherwise.
pub fn init() -> Telemetry {
    usnm_telemetry::init(SERVICE)
}

/// The pipeline's instruments. With no meter provider installed (telemetry
/// off) they record nothing.
pub struct Metrics {
    /// Documents the index node accepted.
    pub docs_sent: Counter<u64>,
    pub bytes_sent: Counter<u64>,
    /// Ingest requests retried after pushback, by HTTP `status`.
    pub retries: Counter<u64>,
    /// Batches by `outcome`: ok, failed, throttled, timed_out, or stopped
    /// (released unstarted when the worker reached its max runtime).
    pub curate_batches: Counter<u64>,
    pub curate_pages: Counter<u64>,
    pub curate_duration: Histogram<f64>,
    pub docs_expected: Gauge<u64>,
    pub disk_free: Gauge<u64>,
    /// Node parts `ocr-sync` took, by `outcome` (imported, rejected) and
    /// `reason` (a reject's code).
    pub ocr_sync_parts: Counter<u64>,
    /// The age of the oldest part `ocr-sync` left in `ocr-out`.
    pub ocr_out_oldest_age: Gauge<u64>,
}

/// The instruments, created on first use from the global meter provider
/// ([`init`] installs it first).
pub fn metrics() -> &'static Metrics {
    static METRICS: OnceLock<Metrics> = OnceLock::new();
    METRICS.get_or_init(|| {
        let m = opentelemetry::global::meter("usnm-ingest");
        Metrics {
            docs_sent: m
                .u64_counter("ingest.docs_sent")
                .with_description("Documents the index node accepted")
                .build(),
            bytes_sent: m
                .u64_counter("ingest.bytes_sent")
                .with_unit("By")
                .with_description("Ingest request bytes the index node accepted")
                .build(),
            retries: m
                .u64_counter("ingest.retries")
                .with_description("Ingest requests retried after the node pushed back")
                .build(),
            curate_batches: m
                .u64_counter("curate.batches")
                .with_description("Batches curated, by outcome")
                .build(),
            curate_pages: m
                .u64_counter("curate.pages")
                .with_description("Pages curated")
                .build(),
            curate_duration: m
                .f64_histogram("curate.duration_seconds")
                .with_unit("s")
                .with_description("Time to download, parse, upload and commit one batch")
                .build(),
            docs_expected: m
                .u64_gauge("release.docs_expected")
                .with_description("Documents the release in progress will index")
                .build(),
            disk_free: m
                .u64_gauge("work_disk_free_bytes")
                .with_unit("By")
                .with_description("Free space on the work directory's file system")
                .build(),
            ocr_sync_parts: m
                .u64_counter("ocr_sync.parts")
                .with_description("OCR node parts picked up, by outcome and reject reason")
                .build(),
            ocr_out_oldest_age: m
                .u64_gauge("ocr.out.oldest_age_s")
                .with_unit("s")
                .with_description("Age of the oldest part left in ocr-out after a pickup")
                .build(),
        }
    })
}
