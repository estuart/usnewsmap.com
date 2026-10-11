//! The Quickwit searcher's cache, thread pool and runtime metrics,
//! reported as the API's own (#125, #251, 08 §8.1.2, 08 §8.4).
//!
//! The searcher (a sidecar on localhost, or its own app on the environment's
//! internal ingress, #267) can't be reached from outside, so neither can its
//! Prometheus endpoint. Every `USNM_SEARCHER_METRICS_SECS` (60 s by default;
//! 0 turns it off, and with it the restart warm-up, which these reads detect)
//! the API reads it and reports, for the split footer, fast field,
//! partial request and predicate caches (`cache`: `split_footer`,
//! `fast_field`, `partial_request`, `predicate`):
//!
//! - `api.searcher_cache_bytes` and `api.searcher_cache_items`: what the
//!   cache holds now (gauges). Recorded when they change, and at least every
//!   [`HEARTBEAT`] so a quiet replica still reports them.
//! - `api.searcher_cache_hits`, `api.searcher_cache_misses` and
//!   `api.searcher_cache_evictions`: what happened since the last scrape
//!   (counters), recorded only when not zero. The first scrape reports
//!   everything since the searcher started, which in a fresh replica is the
//!   warm-up. A restarted searcher starts every counter from 0, so if any
//!   counter of any cache or runtime went down (or a cache or runtime
//!   disappeared), every new value is what happened since.
//!
//! For the thread pools and Tokio runtimes, so a cold search waiting on the
//! search pool's cores can be told from one waiting on downloads (#251), it
//! reports:
//!
//! - `api.searcher_pool_tasks`: the tasks the `search` and `small_tasks`
//!   thread pools (`pool`) are running and have queued (`state`: `ongoing`,
//!   `pending`) at the scrape (a gauge). Recorded for both pools at every
//!   scrape, 0 included, also before Quickwit has used a pool: a snapshot of
//!   an idle pool says as much as a busy one.
//! - `api.searcher_runtime_threads`: the worker threads of each Tokio
//!   runtime (`runtime`: `main`; `blocking` and `non_blocking` once Quickwit
//!   starts them), a gauge recorded at every scrape.
//! - `api.searcher_runtime_busy_ms`: how long those threads were busy since
//!   the last scrape, summed over them (a counter, recorded only when not
//!   zero), and `api.searcher_runtime_capacity_ms`: the thread time they had
//!   in the same span, threads times the time since the last scrape (a
//!   counter). Busy over capacity, summed over any span, is the runtime's
//!   busy share, however the reports fall into it. Both need the last
//!   scrape of the same searcher run, so neither is recorded at the first
//!   scrape or right after a restart, when the busy time covers the
//!   searcher's whole life. The main runtime does the searcher's downloads
//!   and opens the splits.
//!
//! Whether the split footer cache holds every footer is read from two
//! things together: the release's footer total (`footer_bytes` on its
//! `index layout` lines) against `split_footer_cache_capacity`, and whether
//! footer misses and evictions keep coming after the warm-up. An eviction on
//! its own proves nothing: Quickwit 0.9.1 also counts replacing an entry as
//! one, and after a publish the cache drops the old version's footers. The
//! first footer eviction a searcher run shows is logged once, without a
//! cause.
//!
//! A scrape that fails (the sidecar starting, restarting or gone) records
//! nothing. The first failure in a row is logged once, and the recovery once.
//!
//! The searcher can run as its own app (#267) rather than a sidecar on
//! localhost; the API reads the same endpoint at `USNM_QUICKWIT_URL` either
//! way. A restart the scrapes find is also handed to [`spawn`]'s
//! `on_restart`, which warms the new searcher's caches (06 §6.5).

use std::collections::BTreeMap;
use std::future::Future;
use std::time::{Duration, Instant};

use opentelemetry::metrics::{Counter, Gauge, Meter};
use opentelemetry::KeyValue;
use usnm_search::cache_metrics::{Cache, CacheReport};
use usnm_search::quickwit::SearcherMetrics;
use usnm_search::thread_metrics::{Pool, ThreadReport};

/// The longest a cache gauge goes unrecorded while the scrapes succeed.
pub const HEARTBEAT: Duration = Duration::from_secs(15 * 60);

/// The instruments the scrapes are recorded with.
pub struct Instruments {
    bytes: Gauge<u64>,
    items: Gauge<u64>,
    hits: Counter<u64>,
    misses: Counter<u64>,
    evictions: Counter<u64>,
    pool_tasks: Gauge<u64>,
    runtime_threads: Gauge<u64>,
    runtime_busy: Counter<u64>,
    runtime_capacity: Counter<u64>,
}

impl Instruments {
    pub fn new(meter: &Meter) -> Self {
        Self {
            bytes: meter
                .u64_gauge("api.searcher_cache_bytes")
                .with_unit("By")
                .with_description("Bytes in a searcher cache, by cache")
                .build(),
            items: meter
                .u64_gauge("api.searcher_cache_items")
                .with_description("Items in a searcher cache (split footers: splits), by cache")
                .build(),
            hits: meter
                .u64_counter("api.searcher_cache_hits")
                .with_description("Searcher cache hits, by cache")
                .build(),
            misses: meter
                .u64_counter("api.searcher_cache_misses")
                .with_description("Searcher cache misses, by cache")
                .build(),
            evictions: meter
                .u64_counter("api.searcher_cache_evictions")
                .with_description("Items evicted from a searcher cache, by cache")
                .build(),
            pool_tasks: meter
                .u64_gauge("api.searcher_pool_tasks")
                .with_description("Tasks in a searcher thread pool, by pool and state")
                .build(),
            runtime_threads: meter
                .u64_gauge("api.searcher_runtime_threads")
                .with_description("Worker threads of a searcher Tokio runtime, by runtime")
                .build(),
            runtime_busy: meter
                .u64_counter("api.searcher_runtime_busy_ms")
                .with_unit("ms")
                .with_description(
                    "Time a searcher Tokio runtime's worker threads were busy, summed over them, by runtime",
                )
                .build(),
            runtime_capacity: meter
                .u64_counter("api.searcher_runtime_capacity_ms")
                .with_unit("ms")
                .with_description(
                    "Thread time a searcher Tokio runtime had (worker threads times elapsed time), by runtime",
                )
                .build(),
        }
    }
}

/// What the scrapes have seen so far; [`Scraper::observe`] takes each one.
pub struct Scraper {
    instruments: Instruments,
    /// The last successful scrape, for the counters' increments.
    last: Option<SearcherMetrics>,
    /// When `last` was taken.
    last_at: Option<Instant>,
    /// Per cache: the gauges' last recorded values, and when.
    recorded: BTreeMap<Cache, ((u64, u64), Instant)>,
    /// Scrapes failed in a row.
    failures: u32,
    /// Whether this searcher run's first footer eviction has been logged.
    footer_evictions_logged: bool,
}

impl Scraper {
    pub fn new(instruments: Instruments) -> Self {
        Self {
            instruments,
            last: None,
            last_at: None,
            recorded: BTreeMap::new(),
            failures: 0,
            footer_evictions_logged: false,
        }
    }

    /// Scrapes failed in a row (0 after a success).
    pub fn failures(&self) -> u32 {
        self.failures
    }

    /// Record one scrape taken at `now`; true if it found that the searcher
    /// restarted since the last successful one.
    pub fn observe(&mut self, scraped: Result<SearcherMetrics, String>, now: Instant) -> bool {
        let metrics = match scraped {
            Ok(m) => m,
            Err(error) => {
                self.failures += 1;
                if self.failures == 1 {
                    tracing::warn!(%error, "searcher metrics unavailable; retrying quietly");
                }
                return false;
            }
        };
        if self.failures > 0 {
            tracing::info!(
                failed_scrapes = self.failures,
                "searcher metrics available again"
            );
            self.failures = 0;
        }
        // Counters that went down: the searcher restarted, and all of its
        // caches and runtimes count from 0 again.
        let restart = self.last.as_ref().is_some_and(|l| restarted(l, &metrics));
        if restart {
            tracing::info!("the searcher restarted; its counters start again from 0");
            self.last = None;
            self.footer_evictions_logged = false;
        }
        self.observe_caches(&metrics.caches, now);
        self.observe_threads(&metrics.threads, now);
        self.last = Some(metrics);
        self.last_at = Some(now);
        restart
    }

    /// Record the caches of one scrape.
    fn observe_caches(&mut self, report: &CacheReport, now: Instant) {
        for (&cache, now_stats) in report {
            let attrs = [KeyValue::new("cache", cache.label())];
            let gauges = (now_stats.bytes, now_stats.items);
            let due = self
                .recorded
                .get(&cache)
                .is_none_or(|&(last, at)| last != gauges || now.duration_since(at) >= HEARTBEAT);
            if due {
                self.instruments.bytes.record(gauges.0, &attrs);
                self.instruments.items.record(gauges.1, &attrs);
                self.recorded.insert(cache, (gauges, now));
            }
            let before = self
                .last
                .as_ref()
                .and_then(|l| l.caches.get(&cache))
                .copied()
                .unwrap_or_default();
            let (hits, misses, evictions) = (
                now_stats.hits.saturating_sub(before.hits),
                now_stats.misses.saturating_sub(before.misses),
                now_stats.evictions.saturating_sub(before.evictions),
            );
            for (counter, n) in [
                (&self.instruments.hits, hits),
                (&self.instruments.misses, misses),
                (&self.instruments.evictions, evictions),
            ] {
                if n > 0 {
                    counter.add(n, &attrs);
                }
            }
        }
        if let Some(footers) = report.get(&Cache::SplitFooter) {
            if footers.evictions > 0 && !self.footer_evictions_logged {
                self.footer_evictions_logged = true;
                tracing::info!(
                    bytes = footers.bytes,
                    items = footers.items,
                    evictions = footers.evictions,
                    "split footer cache evictions seen"
                );
            }
        }
    }

    /// Record the thread pools and runtimes of one scrape taken at `now`.
    fn observe_threads(&self, report: &ThreadReport, now: Instant) {
        // Every pool, also one Quickwit hasn't used yet and so doesn't list.
        for pool in Pool::ALL {
            let tasks = report.pools.get(&pool).copied().unwrap_or_default();
            for (state, n) in [("ongoing", tasks.ongoing), ("pending", tasks.pending)] {
                self.instruments.pool_tasks.record(
                    n,
                    &[
                        KeyValue::new("pool", pool.label()),
                        KeyValue::new("state", state),
                    ],
                );
            }
        }
        for (&runtime, now_stats) in &report.runtimes {
            let attrs = [KeyValue::new("runtime", runtime.label())];
            self.instruments
                .runtime_threads
                .record(now_stats.threads, &attrs);
            // The busy time since the last scrape of this searcher run; none
            // at the first, which covers the searcher's whole life.
            let Some((before, at)) = self
                .last
                .as_ref()
                .and_then(|l| l.threads.runtimes.get(&runtime))
                .zip(self.last_at)
            else {
                continue;
            };
            let busy = now_stats.busy_ms.saturating_sub(before.busy_ms);
            if busy > 0 {
                self.instruments.runtime_busy.add(busy, &attrs);
            }
            let elapsed_ms = u64::try_from(now.duration_since(at).as_millis()).unwrap_or(u64::MAX);
            self.instruments
                .runtime_capacity
                .add(now_stats.threads.saturating_mul(elapsed_ms), &attrs);
        }
    }
}

/// Whether the searcher restarted between two scrapes: a counter of any
/// cache or runtime went down, or a cache or runtime it had reported is
/// gone.
fn restarted(before: &SearcherMetrics, now: &SearcherMetrics) -> bool {
    let caches = before
        .caches
        .iter()
        .any(|(cache, b)| match now.caches.get(cache) {
            None => true,
            Some(n) => n.hits < b.hits || n.misses < b.misses || n.evictions < b.evictions,
        });
    let runtimes = before.threads.runtimes.iter().any(|(runtime, b)| {
        now.threads
            .runtimes
            .get(runtime)
            .is_none_or(|n| n.busy_ms < b.busy_ms)
    });
    caches || runtimes
}

/// Scrape with `fetch` every `interval` (the first after one interval, once
/// the searcher has had time to start), for as long as the process runs,
/// calling `on_restart` when a scrape finds the searcher restarted.
pub fn spawn<F, Fut, R>(
    interval: Duration,
    meter: &Meter,
    mut fetch: F,
    mut on_restart: R,
) -> tokio::task::JoinHandle<()>
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = Result<SearcherMetrics, String>> + Send,
    R: FnMut() + Send + 'static,
{
    let mut scraper = Scraper::new(Instruments::new(meter));
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await;
        loop {
            tick.tick().await;
            let scraped = fetch().await;
            if scraper.observe(scraped, Instant::now()) {
                on_restart();
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use usnm_search::cache_metrics::CacheStats;
    use usnm_search::thread_metrics::{Runtime, RuntimeStats};

    fn stats(hits: u64, misses: u64, evictions: u64) -> CacheStats {
        CacheStats {
            bytes: 0,
            items: 0,
            hits,
            misses,
            evictions,
        }
    }

    fn report(entries: &[(Cache, CacheStats)]) -> SearcherMetrics {
        SearcherMetrics {
            caches: entries.iter().copied().collect(),
            threads: ThreadReport::default(),
        }
    }

    fn busy(mut metrics: SearcherMetrics, entries: &[(Runtime, u64)]) -> SearcherMetrics {
        metrics.threads.runtimes = entries
            .iter()
            .map(|&(r, busy_ms)| {
                (
                    r,
                    RuntimeStats {
                        busy_ms,
                        threads: 1,
                    },
                )
            })
            .collect();
        metrics
    }

    #[test]
    fn a_restart_is_seen_in_any_cache() {
        let before = report(&[
            (Cache::SplitFooter, stats(5, 3, 0)),
            (Cache::FastField, stats(9, 9, 1)),
        ]);
        assert!(!restarted(
            &before,
            &report(&[
                (Cache::SplitFooter, stats(5, 4, 0)),
                (Cache::FastField, stats(9, 9, 1)),
                (Cache::Predicate, stats(1, 1, 0)),
            ])
        ));
        // Only the fast field cache went down.
        assert!(restarted(
            &before,
            &report(&[
                (Cache::SplitFooter, stats(6, 4, 0)),
                (Cache::FastField, stats(1, 2, 0)),
            ])
        ));
        // A cache it had reported is gone.
        assert!(restarted(
            &before,
            &report(&[(Cache::SplitFooter, stats(6, 4, 0))])
        ));
    }

    #[test]
    fn a_restart_is_seen_in_a_runtime() {
        let caches = [(Cache::SplitFooter, stats(5, 3, 0))];
        let before = busy(report(&caches), &[(Runtime::Main, 60_000)]);
        // More busy time, and a runtime that started since: no restart.
        assert!(!restarted(
            &before,
            &busy(
                report(&caches),
                &[(Runtime::Main, 61_000), (Runtime::Blocking, 5)]
            )
        ));
        // The caches alone don't show it, but the main runtime's busy time
        // went down.
        assert!(restarted(
            &before,
            &busy(report(&caches), &[(Runtime::Main, 900)])
        ));
        // The runtime is gone.
        assert!(restarted(&before, &report(&caches)));
    }
}
