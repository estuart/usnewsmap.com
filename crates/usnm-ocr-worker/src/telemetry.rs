//! The worker's telemetry: the shared setup in `usnm-telemetry` (JSON logs
//! on stdout; spans, their warnings and metrics to Application Insights when
//! `APPLICATIONINSIGHTS_CONNECTION_STRING` is set), under the cloud role
//! `usnm-ocr-worker` with the node's name as the role instance. Uploads carry
//! an Entra token for Azure Monitor from the node's Arc identity.
//!
//! Nothing here carries a message body: pages are named by their ids only.

use opentelemetry::metrics::{Counter, Histogram, Meter};
use opentelemetry::KeyValue;
pub use usnm_telemetry::{Telemetry, CONNECTION_STRING_VAR};

pub const SERVICE: usnm_telemetry::Service = usnm_telemetry::Service {
    name: "usnm-ocr-worker",
    version: env!("CARGO_PKG_VERSION"),
};

/// Install logging and, if the connection string is set, the exporters,
/// reporting as `node`. Without it, logs go to stdout only. Call
/// [`Telemetry::shutdown`] before the process exits.
pub fn init(node: Option<&str>) -> Telemetry {
    usnm_telemetry::init_with(
        SERVICE,
        usnm_telemetry::Options {
            instance: node.map(str::to_owned),
            page_views: false,
        },
    )
}

/// The worker's instruments. With no meter provider installed (telemetry
/// off, tests) they record nothing.
#[derive(Debug, Clone)]
pub struct Metrics {
    /// Pages written, by `engine`, `language`, `reason` and `queue`.
    pub pages_done: Counter<u64>,
    /// Pages left to reappear after a temporary failure, by the same and
    /// `error` (fetch, engine, no_model, output, scratch).
    pub pages_failed: Counter<u64>,
    /// Pages given up without output, by `cause`: `budget` (out of time,
    /// with the page's attributes) or `shutdown` (the grace ran out).
    pub pages_abandoned: Counter<u64>,
    /// Messages moved to `ocr-deadletter`, by `queue` and `cause`
    /// (dequeue_count, invalid, permanent).
    pub dead_lettered: Counter<u64>,
    /// Polls that found both queues empty.
    pub empty_polls: Counter<u64>,
    /// loc.gov answers asking us to slow down, by `status` (429, 503),
    /// `server` (api or tile) and whether a `Retry-After` came with it.
    pub loc_slowdowns: Counter<u64>,
    /// Engine time per page, by `engine`, `models` and `language`.
    pub ocr_seconds: Histogram<f64>,
    /// Time to get a page's scan, by `outcome` and `source`: `loc` (the
    /// issue lookup and the image from loc.gov, a cache miss included) or
    /// `cache` (read and checked from the prefetch cache).
    pub image_fetch_seconds: Histogram<f64>,
}

impl Metrics {
    pub fn new(meter: &Meter) -> Self {
        Self {
            pages_done: meter
                .u64_counter("ocr.pages_done")
                .with_description("Pages OCRed and written")
                .build(),
            pages_failed: meter
                .u64_counter("ocr.pages_failed")
                .with_description("Pages left to reappear after a temporary failure")
                .build(),
            pages_abandoned: meter
                .u64_counter("ocr.pages_abandoned")
                .with_description("Pages given up without output")
                .build(),
            dead_lettered: meter
                .u64_counter("ocr.dead_lettered")
                .with_description("Messages moved to ocr-deadletter")
                .build(),
            empty_polls: meter
                .u64_counter("ocr.empty_polls")
                .with_description("Polls that found both queues empty")
                .build(),
            loc_slowdowns: meter
                .u64_counter("ocr.loc_slowdowns")
                .with_description("loc.gov 429/503 answers")
                .build(),
            ocr_seconds: meter
                .f64_histogram("ocr.ocr_seconds")
                .with_unit("s")
                .with_description("Engine time per page")
                .build(),
            image_fetch_seconds: meter
                .f64_histogram("ocr.image_fetch_seconds")
                .with_unit("s")
                .with_description("Time to get a page's scan, from loc.gov or the prefetch cache")
                .build(),
        }
    }

    /// The instruments on the global meter provider (installed by [`init`]
    /// when telemetry is on).
    pub fn global() -> Self {
        Self::new(&opentelemetry::global::meter(SERVICE.name))
    }
}

/// A page's attributes on its counters.
pub fn page_attrs(engine: &str, language: &str, reason: &str, queue: &str) -> Vec<KeyValue> {
    vec![
        KeyValue::new("engine", engine.to_owned()),
        KeyValue::new("language", language.to_owned()),
        KeyValue::new("reason", reason.to_owned()),
        KeyValue::new("queue", queue.to_owned()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With no connection string, the worker runs with stdout logs only,
    /// and its instruments record into nothing.
    #[tokio::test]
    async fn telemetry_is_off_without_a_connection_string() {
        if std::env::var_os(CONNECTION_STRING_VAR).is_some() {
            eprintln!("{CONNECTION_STRING_VAR} is set; skipping");
            return;
        }
        let t = init(Some("nuc1"));
        assert!(!t.is_on());
        let m = Metrics::global();
        m.pages_done
            .add(1, &page_attrs("usnm-tesseract", "deu", "bulk", "ocr-bulk"));
        m.ocr_seconds.record(1.5, &[]);
        tracing::warn!("a warning with telemetry off");
        t.shutdown().await;
    }

    #[test]
    fn reports_as_the_worker() {
        assert_eq!(SERVICE.name, "usnm-ocr-worker");
        assert_eq!(usnm_telemetry::TOKEN_RESOURCE, "https://monitor.azure.com/");
    }
}
