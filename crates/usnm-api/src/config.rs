//! Configuration from environment variables (12-factor).

use std::num::NonZeroU32;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendKind {
    /// In-memory engine over local JSONL indexes (development and tests).
    Memory,
    /// Quickwit at the given base URL (production: the localhost sidecar,
    /// or the searcher app's internal address, #267).
    Quickwit(String),
}

/// Per-client token bucket (06 §6.2, 08 §8.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimit {
    pub per_minute: NonZeroU32,
    pub burst: NonZeroU32,
}

/// The default readiness cap's margin over the startup warm-up's budget
/// (`USNM_READY_CAP_SECS` unset): the warm-up stops its last query at the
/// budget, then counts what it warmed, which takes seconds.
pub const READY_CAP_MARGIN_SECS: u64 = 60;

/// The longest warm-up budget or readiness cap that can be set: a day. They
/// are added to `Instant`s, where a value near `u64::MAX` seconds would
/// panic. (The default cap, a day's budget plus the margin, is a little more.)
pub const MAX_WARM_UP_SECS: u64 = 24 * 60 * 60;

/// The shortest `USNM_ABANDON_AFTER_SECS`: the 2 s `Retry-After` of a `202`
/// plus room for a slow network.
pub const MIN_ABANDON_AFTER_SECS: u64 = 5;

/// The deepest offset a search's `cursor` reaches in `/v1/hits` (06 §6.3.4).
pub const MAX_OFFSET: usize = 10_000;

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: String,
    pub backend: BackendKind,
    /// Local directory with the memory backend's `indexes/`; also the default
    /// reference location.
    pub data_dir: PathBuf,
    /// Where `current.json` and reference snapshots live: a Blob container URL
    /// (read with the managed identity) or a local directory.
    pub reference_url: String,
    /// Persistent response cache (`cache/` container), if any.
    pub response_cache_url: Option<String>,
    /// Responses that took at least this long to compute are persisted.
    pub persist_after: Duration,
    pub allowed_origins: Vec<String>,
    /// How long a visitor's request waits for a search before the API
    /// answers `202 Accepted` and the search carries on without it.
    pub search_timeout: Duration,
    /// The limit on one search computation (each backend call and the whole
    /// response), whether or not anyone still waits for it. Past it the
    /// search is cancelled and fails with a timeout, which is not cached.
    pub compute_cap: Duration,
    /// Search computations (`/v1/aggregate`, `/v1/hits`, `/v1/days`) allowed to run at
    /// once, including those still running after their visitor got a `202`.
    /// A new one queues for a slot (see `search_queue`).
    pub compute_concurrency: usize,
    /// Searches allowed to wait for a slot, first come, first served. A
    /// queued search keeps its place while its visitor keeps asking, within
    /// `compute_cap`; one that finds the queue full gets a `503` busy.
    pub search_queue: usize,
    /// A computation nobody has waited on for this long is cancelled (the
    /// visitor changed the search or left). Visitors ask again within
    /// `Retry-After` (2 s) of each `202`.
    pub abandon_after: Duration,
    /// Warm-up (`crate::prewarm`): limit on each query. Well above
    /// `search_timeout`, because a cold search can take longer than a visitor
    /// is allowed to wait.
    pub prewarm_query_timeout: Duration,
    /// Warm-up before a publish swaps a new version in: limit on the whole
    /// run; queries left when it runs out are skipped. The old version serves
    /// meanwhile, so this only delays the swap.
    pub prewarm_budget: Duration,
    /// Warm-up after a start: the same limit. Kept shorter than
    /// `prewarm_budget`, because by default the replica isn't ready until it
    /// finishes (`ready_cap`), and with a short cap it serves visitors while
    /// it warms, on the same search sidecar.
    pub prewarm_startup_budget: Duration,
    /// Warm-up: after the examples, this many of the most frequent searches
    /// in the search log (0 turns it off).
    pub prewarm_top_searches: usize,
    /// Warm-up: how many of the search log's most recent days are counted.
    pub prewarm_log_days: u64,
    /// Warm-up: the first pause before retrying a query the backend failed
    /// (it doubles, up to 10 s). The searcher can still be starting when a
    /// replica warms up.
    pub prewarm_retry_first: Duration,
    /// After a start, `/readyz` reports ready once the warm-up finishes or
    /// this much time passes, whichever is first. By default the startup
    /// warm-up's budget plus `READY_CAP_MARGIN_SECS`, so a rollout keeps the
    /// old revision serving until the new replica is warm, and only a hung
    /// warm-up reaches it. Where no other replica serves meanwhile (an app
    /// that scales to zero), a short cap lets the first visitor in sooner.
    pub ready_cap: Duration,
    pub refresh_interval: Duration,
    pub cache_bytes: u64,
    /// Cube cell budget; above it buckets are coarsened (ADR-0003).
    pub max_cells: usize,
    /// `None` disables rate limiting.
    pub rate_limit: Option<RateLimit>,
    /// Proxies in front of the API that append to `X-Forwarded-For`
    /// (1 = Container Apps ingress; 2 = a proxy such as Front Door + ingress).
    /// 0 ignores the header and uses the peer address.
    pub trusted_proxy_hops: usize,
    /// Requests allowed to query the search backend at once.
    pub backend_concurrency: usize,
    /// The built web app to serve alongside the API (`web/dist`); none
    /// serves the API alone.
    pub site_dir: Option<PathBuf>,
    /// The site's canonical hostname: requests for `www.` + this host are
    /// redirected to `https://` + this host.
    pub site_host: String,
    /// Cosmos DB endpoint of the pipeline state, read-only for `/v1/status`
    /// (the same variable the ingest jobs use).
    pub cosmos_endpoint: Option<String>,
    /// The ingest CLI's local state file, for `/v1/status` in development.
    pub state_file: Option<PathBuf>,
    /// How often `/v1/status` is recomputed at most.
    pub status_refresh: Duration,
    /// The ingest job's schedule (`USNM_INGEST_CRON`, UTC), for the status
    /// page's next scheduled run; `None` when the job is started by hand.
    pub ingest_cron: Option<crate::status::schedule::Cron>,
    /// Where the anonymous search log goes (`searches/` container), if
    /// anywhere (06 §6.8).
    pub search_log_url: Option<String>,
    /// How often the search log appends its batch.
    pub search_log_flush: Duration,
    /// How often the Quickwit searcher's cache, thread pool and runtime
    /// metrics are read and reported (`crate::searcher_metrics`); zero turns
    /// it off. Quickwit backend only.
    pub searcher_metrics_interval: Duration,
    /// Whether searches cover American Stories' text on a version built
    /// with it (`USNM_AMERICAN_STORIES_SEARCH`, 05 §5.5.4). Off, the API
    /// searches LoC's text alone, whatever `current.json` says.
    pub american_stories_search: bool,
    /// Whether `source=congress` searches a version's Congress index set
    /// (`USNM_CONGRESS_SEARCH`, #229). Off, Congress searches answer 422 and
    /// `/v1/meta` lists no Congress sources, whatever `current.json` says.
    pub congress_search: bool,
    /// Which side a page-language `lang=` filter is rendered as
    /// (`USNM_PAGE_LANGUAGE_FILTER`, 06 §6.3.1): `smaller` (the default,
    /// the side with fewer pages) or `match` (always `page_language:IN`).
    /// Both find the same pages; the setting is for measuring one against
    /// the other.
    pub page_language_filter: usnm_search::PageLanguageSide,
    /// Whether `/v1/hits` lists a title's pages without `q`
    /// (`USNM_BROWSE_PAGES`, #274, 06 §6.3.4). Off by default: such a list
    /// has no word to skip splits by, so it is measured before it is on.
    /// `/v1/meta` says so (`browse_pages`), and the site offers it only then.
    pub browse_pages: bool,
    /// The deepest offset a search's `cursor` reaches in `/v1/hits`
    /// ([`MAX_OFFSET`]). Not a setting: tests lower it
    /// to show that a list of a title's pages without `q`, whose cursor is
    /// a key rather than an offset (#274), goes past it.
    pub max_offset: usize,
    /// End-to-end tests only, and only with the memory backend: an aggregate
    /// search whose query contains this term waits this long before it runs,
    /// to stand in for a cold search on the real corpus.
    pub fixture_slow: Option<(String, Duration)>,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// Build from any key lookup (tests pass `|_| None` for the defaults).
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let var = |k: &str| lookup(k).filter(|v| !v.is_empty());
        let num = |k: &str, default: u64| -> Result<u64, String> {
            var(k).map_or(Ok(default), |v| {
                v.parse().map_err(|_| format!("{k} must be a whole number"))
            })
        };
        // A duration in seconds, at most `MAX_WARM_UP_SECS` when set.
        let span = |k: &str, default: Duration| -> Result<Duration, String> {
            match var(k) {
                None => Ok(default),
                Some(_) => match num(k, 0)? {
                    s if s > MAX_WARM_UP_SECS => {
                        Err(format!("{k} must be at most {MAX_WARM_UP_SECS}"))
                    }
                    s => Ok(Duration::from_secs(s)),
                },
            }
        };
        let flag = |k: &str, default: bool| -> Result<bool, String> {
            var(k).map_or(Ok(default), |v| match v.to_ascii_lowercase().as_str() {
                "true" => Ok(true),
                "false" => Ok(false),
                _ => Err(format!("{k} must be true or false")),
            })
        };
        let backend = match var("USNM_BACKEND").as_deref().unwrap_or("memory") {
            "memory" => BackendKind::Memory,
            "quickwit" => BackendKind::Quickwit(
                var("USNM_QUICKWIT_URL").unwrap_or_else(|| "http://127.0.0.1:7280".to_owned()),
            ),
            other => {
                return Err(format!(
                    "USNM_BACKEND must be memory or quickwit, not `{other}`"
                ))
            }
        };
        let data_dir = var("USNM_DATA_DIR").unwrap_or_else(|| "fixtures/data".to_owned());
        let per_minute = num("USNM_RATE_PER_MIN", 120)?;
        let rate_limit = match NonZeroU32::new(u32::try_from(per_minute).unwrap_or(u32::MAX)) {
            None => None,
            Some(per_minute) => Some(RateLimit {
                per_minute,
                burst: NonZeroU32::new(
                    u32::try_from(num("USNM_RATE_BURST", 40)?).unwrap_or(u32::MAX),
                )
                .ok_or("USNM_RATE_BURST must be at least 1")?,
            }),
        };
        let fixture_slow = match var("USNM_FIXTURE_SLOW_TERM") {
            None => None,
            Some(_) if backend != BackendKind::Memory => {
                return Err("USNM_FIXTURE_SLOW_TERM works with the memory backend only".into())
            }
            Some(term) => Some((
                term.to_lowercase(),
                Duration::from_millis(num("USNM_FIXTURE_SLOW_MS", 5000)?),
            )),
        };
        let prewarm_startup_budget =
            span("USNM_PREWARM_STARTUP_BUDGET_SECS", Duration::from_secs(300))?;
        Ok(Self {
            bind: var("USNM_BIND").unwrap_or_else(|| "0.0.0.0:8080".to_owned()),
            backend,
            reference_url: var("USNM_REFERENCE_URL").unwrap_or_else(|| data_dir.clone()),
            data_dir: PathBuf::from(data_dir),
            response_cache_url: var("USNM_RESPONSE_CACHE_URL"),
            persist_after: Duration::from_millis(num("USNM_PERSIST_AFTER_MS", 500)?),
            allowed_origins: var("USNM_ALLOWED_ORIGINS")
                .unwrap_or_else(|| "https://usnewsmap.com".to_owned())
                .split(',')
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect(),
            search_timeout: Duration::from_secs(num("USNM_SEARCH_TIMEOUT_SECS", 10)?),
            compute_cap: Duration::from_secs(num("USNM_COMPUTE_CAP_SECS", 120)?.max(1)),
            compute_concurrency: usize::try_from(num("USNM_COMPUTE_CONCURRENCY", 2)?.max(1))
                .map_err(|e| e.to_string())?,
            search_queue: usize::try_from(num("USNM_SEARCH_QUEUE", 16)?)
                .map_err(|e| e.to_string())?,
            abandon_after: match num("USNM_ABANDON_AFTER_SECS", 15)? {
                // Well above the 2 s Retry-After, or a slow search still being
                // polled could be cancelled between two polls.
                s if s < MIN_ABANDON_AFTER_SECS => {
                    return Err(format!(
                        "USNM_ABANDON_AFTER_SECS must be at least {MIN_ABANDON_AFTER_SECS}"
                    ))
                }
                s => Duration::from_secs(s),
            },
            prewarm_query_timeout: Duration::from_secs(num("USNM_PREWARM_QUERY_SECS", 60)?),
            prewarm_budget: span("USNM_PREWARM_BUDGET_SECS", Duration::from_secs(900))?,
            prewarm_startup_budget,
            prewarm_top_searches: usize::try_from(num("USNM_PREWARM_TOP_SEARCHES", 20)?)
                .map_err(|e| e.to_string())?,
            prewarm_log_days: num("USNM_PREWARM_LOG_DAYS", 28)?,
            prewarm_retry_first: Duration::from_secs(1),
            ready_cap: span(
                "USNM_READY_CAP_SECS",
                prewarm_startup_budget + Duration::from_secs(READY_CAP_MARGIN_SECS),
            )?,
            refresh_interval: Duration::from_secs(num("USNM_REFRESH_SECS", 600)?.max(1)),
            cache_bytes: num("USNM_CACHE_MB", 256)? * 1024 * 1024,
            max_cells: usnm_core::cube::MAX_CELLS,
            rate_limit,
            trusted_proxy_hops: usize::try_from(num("USNM_TRUSTED_PROXY_HOPS", 1)?)
                .map_err(|e| e.to_string())?,
            backend_concurrency: usize::try_from(num("USNM_BACKEND_CONCURRENCY", 8)?.max(1))
                .map_err(|e| e.to_string())?,
            site_dir: var("USNM_SITE_DIR").map(PathBuf::from),
            site_host: var("USNM_SITE_HOST")
                .unwrap_or_else(|| "usnewsmap.com".to_owned())
                .to_ascii_lowercase(),
            cosmos_endpoint: var("USNM_COSMOS_ENDPOINT"),
            state_file: var("USNM_STATE_FILE").map(PathBuf::from),
            status_refresh: Duration::from_secs(num("USNM_STATUS_REFRESH_SECS", 60)?.max(1)),
            ingest_cron: var("USNM_INGEST_CRON")
                .filter(|c| !c.trim().is_empty())
                .map(|c| crate::status::schedule::Cron::parse(&c))
                .transpose()?,
            search_log_url: var("USNM_SEARCH_LOG_URL"),
            search_log_flush: Duration::from_secs(num("USNM_SEARCH_LOG_FLUSH_SECS", 300)?.max(1)),
            searcher_metrics_interval: Duration::from_secs(num("USNM_SEARCHER_METRICS_SECS", 60)?),
            american_stories_search: flag("USNM_AMERICAN_STORIES_SEARCH", true)?,
            congress_search: flag("USNM_CONGRESS_SEARCH", true)?,
            page_language_filter: match var("USNM_PAGE_LANGUAGE_FILTER").as_deref() {
                None | Some("smaller") => usnm_search::PageLanguageSide::Smaller,
                Some("match") => usnm_search::PageLanguageSide::Match,
                Some(other) => {
                    return Err(format!(
                        "USNM_PAGE_LANGUAGE_FILTER must be smaller or match, not `{other}`"
                    ))
                }
            },
            browse_pages: flag("USNM_BROWSE_PAGES", false)?,
            max_offset: MAX_OFFSET,
            fixture_slow,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_overrides() {
        let c = Config::from_lookup(|_| None).unwrap();
        assert_eq!(c.reference_url, "fixtures/data");
        assert_eq!(c.rate_limit.unwrap().per_minute.get(), 120);
        assert_eq!(c.persist_after, Duration::from_millis(500));
        assert_eq!(c.prewarm_query_timeout, Duration::from_secs(60));
        assert_eq!(c.prewarm_budget, Duration::from_secs(900));
        assert_eq!(c.prewarm_startup_budget, Duration::from_secs(300));
        // Past the startup warm-up's budget, so a start is ready when it's warm.
        assert_eq!(c.ready_cap, Duration::from_secs(360));
        assert_eq!(c.site_host, "usnewsmap.com");
        assert!(c.ingest_cron.is_none());
        assert!(c.search_log_url.is_none());
        assert_eq!(c.search_log_flush, Duration::from_secs(300));
        assert_eq!(c.compute_cap, Duration::from_secs(120));
        assert_eq!(c.compute_concurrency, 2);
        assert_eq!(c.search_queue, 16);
        assert_eq!(c.abandon_after, Duration::from_secs(15));
        assert_eq!(c.prewarm_top_searches, 20);
        assert_eq!(c.prewarm_log_days, 28);
        assert!(c.fixture_slow.is_none());
        assert_eq!(c.searcher_metrics_interval, Duration::from_secs(60));
        assert!(c.american_stories_search);
        assert!(!c.browse_pages);
        let browse = |v: &str| {
            let v = v.to_owned();
            Config::from_lookup(move |k| (k == "USNM_BROWSE_PAGES").then(|| v.clone()))
                .map(|c| c.browse_pages)
        };
        assert_eq!(browse("true"), Ok(true));
        assert_eq!(browse("FALSE"), Ok(false));
        assert!(browse("yes").is_err());
        assert_eq!(
            c.page_language_filter,
            usnm_search::PageLanguageSide::Smaller
        );
        let side = |v: &str| {
            let v = v.to_owned();
            Config::from_lookup(move |k| (k == "USNM_PAGE_LANGUAGE_FILTER").then(|| v.clone()))
                .map(|c| c.page_language_filter)
        };
        assert_eq!(side("match"), Ok(usnm_search::PageLanguageSide::Match));
        assert_eq!(side("smaller"), Ok(usnm_search::PageLanguageSide::Smaller));
        assert!(side("complement").is_err());
        let american = |v: &str| {
            let v = v.to_owned();
            Config::from_lookup(move |k| (k == "USNM_AMERICAN_STORIES_SEARCH").then(|| v.clone()))
                .map(|c| c.american_stories_search)
        };
        assert_eq!(american("false"), Ok(false));
        assert_eq!(american("FALSE"), Ok(false));
        assert_eq!(american("true"), Ok(true));
        // Empty, like unset, takes the default.
        assert_eq!(american(""), Ok(true));
        assert!(american("off").is_err());
        assert!(american("0").is_err());
        let off = Config::from_lookup(|k| (k == "USNM_SEARCHER_METRICS_SECS").then(|| "0".into()))
            .unwrap();
        assert_eq!(off.searcher_metrics_interval, Duration::ZERO);
        let c = Config::from_lookup(|k| match k {
            "USNM_RATE_PER_MIN" => Some("0".into()),
            "USNM_REFERENCE_URL" => Some("https://a.blob.core.windows.net/reference".into()),
            _ => None,
        })
        .unwrap();
        assert!(c.rate_limit.is_none());
        assert_eq!(c.status_refresh, Duration::from_secs(60));
        assert!(c.cosmos_endpoint.is_none());
        assert!(c.reference_url.starts_with("https://"));
        assert!(Config::from_lookup(|k| (k == "USNM_RATE_BURST").then(|| "0".into())).is_err());
        let c = Config::from_lookup(|k| (k == "USNM_INGEST_CRON").then(|| "17 3 * * 1".into()))
            .unwrap();
        assert!(c.ingest_cron.is_some());
        assert!(
            Config::from_lookup(|k| (k == "USNM_INGEST_CRON").then(|| "17 3 *".into())).is_err()
        );
        assert!(Config::from_lookup(|k| (k == "USNM_CACHE_MB").then(|| "x".into())).is_err());
        assert!(
            Config::from_lookup(|k| (k == "USNM_ABANDON_AFTER_SECS").then(|| "2".into())).is_err()
        );
        let slow = Config::from_lookup(|k| (k == "USNM_FIXTURE_SLOW_TERM").then(|| "Slow".into()))
            .unwrap();
        assert_eq!(
            slow.fixture_slow,
            Some(("slow".to_owned(), Duration::from_secs(5)))
        );
        // Never with the real search engine.
        assert!(Config::from_lookup(|k| match k {
            "USNM_FIXTURE_SLOW_TERM" => Some("slow".into()),
            "USNM_BACKEND" => Some("quickwit".into()),
            _ => None,
        })
        .is_err());
    }

    #[test]
    fn ready_cap_follows_the_startup_budget_unless_set() {
        let cap = |vars: &[(&str, &str)]| {
            let vars: Vec<(String, String)> = vars
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect();
            Config::from_lookup(move |k| {
                vars.iter()
                    .find(|(name, _)| name == k)
                    .map(|(_, v)| v.clone())
            })
            .map(|c| c.ready_cap)
        };
        let secs = Duration::from_secs;
        assert_eq!(cap(&[]), Ok(secs(300 + READY_CAP_MARGIN_SECS)));
        // A longer or shorter warm-up moves the default with it.
        assert_eq!(
            cap(&[("USNM_PREWARM_STARTUP_BUDGET_SECS", "120")]),
            Ok(secs(180))
        );
        assert_eq!(
            cap(&[("USNM_PREWARM_STARTUP_BUDGET_SECS", "0")]),
            Ok(secs(60))
        );
        // Set, it wins (an app that scales to zero keeps a short cap).
        assert_eq!(cap(&[("USNM_READY_CAP_SECS", "60")]), Ok(secs(60)));
        assert_eq!(
            cap(&[
                ("USNM_READY_CAP_SECS", "60"),
                ("USNM_PREWARM_STARTUP_BUDGET_SECS", "900"),
            ]),
            Ok(secs(60))
        );
        // Empty, like unset, takes the default.
        assert_eq!(cap(&[("USNM_READY_CAP_SECS", "")]), Ok(secs(360)));
        assert!(cap(&[("USNM_READY_CAP_SECS", "six")]).is_err());
        // Values `Instant` arithmetic can't take are refused, not saturated.
        assert!(cap(&[("USNM_READY_CAP_SECS", &u64::MAX.to_string())]).is_err());
        assert!(cap(&[("USNM_PREWARM_STARTUP_BUDGET_SECS", &u64::MAX.to_string())]).is_err());
        let day = MAX_WARM_UP_SECS.to_string();
        assert_eq!(
            cap(&[("USNM_READY_CAP_SECS", &day)]),
            Ok(secs(MAX_WARM_UP_SECS))
        );
        assert!(cap(&[("USNM_READY_CAP_SECS", &(MAX_WARM_UP_SECS + 1).to_string())]).is_err());
        assert!(cap(&[(
            "USNM_PREWARM_BUDGET_SECS",
            &(MAX_WARM_UP_SECS + 1).to_string()
        )])
        .is_err());
        // The largest budget still gets its margin.
        assert_eq!(
            cap(&[("USNM_PREWARM_STARTUP_BUDGET_SECS", &day)]),
            Ok(secs(MAX_WARM_UP_SECS + READY_CAP_MARGIN_SECS))
        );
    }
}
