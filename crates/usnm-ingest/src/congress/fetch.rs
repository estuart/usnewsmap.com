//! What the Congress fetches share (#229): a paced, retrying HTTP client
//! behind a trait the tests replace with recorded responses ([`Fetch`],
//! [`Http`]), the raw store's create-only cache that keeps only responses a
//! check accepts ([`cached`]), and the Cosmos lock a fetch holds while it
//! runs ([`locked`]).

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context};
use tokio::io::AsyncWriteExt;
use usnm_store::ObjectStore;

/// Fetches a URL: its body, or `None` when the server has no such page (404).
#[async_trait::async_trait]
pub trait Fetch: Send + Sync {
    async fn get(&self, url: &str) -> anyhow::Result<Option<Vec<u8>>>;

    /// Write the body to `dest` (created or truncated) instead of holding it:
    /// its length, or `None` on a 404 (`dest` is then empty). The default
    /// reads the body with [`Fetch::get`]; [`Http`] streams it.
    async fn download(&self, url: &str, dest: &Path) -> anyhow::Result<Option<u64>> {
        match self.get(url).await? {
            Some(body) => {
                tokio::fs::write(dest, &body)
                    .await
                    .with_context(|| dest.display().to_string())?;
                Ok(Some(body.len() as u64))
            }
            None => Ok(None),
        }
    }
}

/// A server over HTTPS: one request at a time, `delay` apart, retrying 429,
/// 5xx, transport errors (a body cut short included) and tries that run past
/// their timeout, with a growing wait (`Retry-After` when given).
///
/// Each try of [`Fetch::get`] may take [`GET_TIMEOUT`], each try of
/// [`Fetch::download`] [`DOWNLOAD_TIMEOUT`] ([`Http::with_timeouts`]). With a
/// cutoff ([`Http::with_cutoff`]: the run's deadline and [`REQUEST_GRACE`]),
/// no try starts with less than [`MIN_TRY`] left before it, a try's timeout
/// is cut to what is left, and the request fails rather than wait to retry
/// past it, so a request started near the deadline can't hold the replica
/// past the job's hard timeout.
pub struct Http {
    client: reqwest::Client,
    delay: Duration,
    retries: u32,
    get_timeout: Duration,
    download_timeout: Duration,
    cutoff: Option<tokio::time::Instant>,
    /// Who the server is, in the logs (`UNT`, `GovInfo`).
    server: &'static str,
    last: tokio::sync::Mutex<Option<tokio::time::Instant>>,
}

/// How long one try of a page (a sitemap, MODS, the Globe's OCR) may take.
pub const GET_TIMEOUT: Duration = Duration::from_secs(120);

/// How long one try of a download may take: 30 minutes covers a 500 MB PDF
/// at under 300 KB/s.
pub const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(1800);

/// How long after the run's deadline a request may still run. The jobs stop
/// between documents at 22 hours and are killed at 24, so every request has
/// ended an hour before that, leaving the rest for writing what was read and
/// the unlock.
pub const REQUEST_GRACE: Duration = Duration::from_secs(3600);

/// The least time a try is started with.
pub const MIN_TRY: Duration = Duration::from_secs(5);

/// How long a try starting at `start` may take: `timeout`, cut to what is
/// left before `cutoff`; `None` when less than [`MIN_TRY`] (or `timeout`, if
/// shorter) is left, and the try isn't started.
pub fn try_budget(
    start: tokio::time::Instant,
    cutoff: Option<tokio::time::Instant>,
    timeout: Duration,
) -> Option<Duration> {
    let Some(cutoff) = cutoff else {
        return Some(timeout);
    };
    let left = cutoff.checked_duration_since(start)?;
    (left >= MIN_TRY.min(timeout)).then(|| left.min(timeout))
}

/// One try's outcome.
enum Try<T> {
    Done(T),
    NotFound,
    /// Try again, after at least this long when the server said.
    Again(Option<Duration>),
}

impl Http {
    /// `user_agent` says who is asking and why, on every request.
    pub fn new(delay: Duration, user_agent: &str, server: &'static str) -> anyhow::Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .user_agent(user_agent)
                .connect_timeout(Duration::from_secs(30))
                .build()?,
            delay,
            retries: 6,
            get_timeout: GET_TIMEOUT,
            download_timeout: DOWNLOAD_TIMEOUT,
            cutoff: None,
            server,
            last: tokio::sync::Mutex::new(None),
        })
    }

    /// How long one try of a page, and of a download, may take.
    pub fn with_timeouts(mut self, get: Duration, download: Duration) -> Self {
        self.get_timeout = get;
        self.download_timeout = download;
        self
    }

    /// The time every request has ended by (see [`Http`]).
    pub fn with_cutoff(mut self, cutoff: Option<tokio::time::Instant>) -> Self {
        self.cutoff = cutoff;
        self
    }

    /// One request, paced from the end of the last and retried; `body`
    /// reads a successful response (and may fail, which is retried). Each
    /// try, its body included, may take `timeout` (see [`try_budget`]).
    async fn request<T, F, Fut>(
        &self,
        url: &str,
        timeout: Duration,
        mut body: F,
    ) -> anyhow::Result<Option<T>>
    where
        F: FnMut(reqwest::Response) -> Fut + Send,
        Fut: std::future::Future<Output = anyhow::Result<T>> + Send,
    {
        let mut wait = self.delay;
        for attempt in 0..=self.retries {
            // Paced from the end of the last request, across retries too.
            let mut last = self.last.lock().await;
            let now = tokio::time::Instant::now();
            let start = last.map_or(now, |t| (t + wait).max(now));
            let Some(budget) = try_budget(start, self.cutoff, timeout) else {
                bail!(
                    "`{url}`: no time left before the run's cutoff for try {} of {}",
                    attempt + 1,
                    self.retries + 1
                );
            };
            tokio::time::sleep_until(start).await;
            let tried = tokio::time::timeout(budget, self.try_once(url, attempt, &mut body)).await;
            *last = Some(tokio::time::Instant::now());
            let retry_after = match tried {
                Ok(Ok(Try::Done(b))) => return Ok(Some(b)),
                Ok(Ok(Try::NotFound)) => return Ok(None),
                Ok(Ok(Try::Again(after))) => after,
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    tracing::warn!(url, server = self.server, timeout = ?budget, attempt, "the request ran past its timeout");
                    None
                }
            };
            wait = retry_wait(self.delay, attempt, retry_after);
        }
        bail!("`{url}` failed {} times", self.retries + 1)
    }

    /// One try: sent, and its body read when it succeeds.
    async fn try_once<T, F, Fut>(
        &self,
        url: &str,
        attempt: u32,
        body: &mut F,
    ) -> anyhow::Result<Try<T>>
    where
        F: FnMut(reqwest::Response) -> Fut + Send,
        Fut: std::future::Future<Output = anyhow::Result<T>> + Send,
    {
        match self.client.get(url).send().await {
            // The body can fail after the headers (a dropped connection): a
            // try like a failed send.
            Ok(r) if r.status().is_success() => match body(r).await {
                Ok(b) => Ok(Try::Done(b)),
                Err(e) => {
                    tracing::warn!(url, server = self.server, error = %format!("{e:#}"), attempt, "reading the response failed");
                    Ok(Try::Again(None))
                }
            },
            Ok(r) if r.status() == reqwest::StatusCode::NOT_FOUND => Ok(Try::NotFound),
            Ok(r)
                if r.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
                    || r.status().is_server_error() =>
            {
                tracing::warn!(
                    url,
                    server = self.server,
                    status = r.status().as_u16(),
                    attempt,
                    "the server asked us to wait"
                );
                Ok(Try::Again(
                    r.headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| {
                            usnm_core::retry_after::parse(v, std::time::SystemTime::now())
                        }),
                ))
            }
            Ok(r) => bail!("`{url}` answered {}", r.status()),
            Err(e) => {
                tracing::warn!(url, server = self.server, error = %e, attempt, "request failed");
                Ok(Try::Again(None))
            }
        }
    }
}

#[async_trait::async_trait]
impl Fetch for Http {
    async fn get(&self, url: &str) -> anyhow::Result<Option<Vec<u8>>> {
        self.request(url, self.get_timeout, |r| async move {
            Ok(r.bytes().await?.to_vec())
        })
        .await
    }

    async fn download(&self, url: &str, dest: &Path) -> anyhow::Result<Option<u64>> {
        let got = self
            .request(url, self.download_timeout, |mut r| async move {
                // Each try starts the file again.
                let mut f = tokio::fs::File::create(dest)
                    .await
                    .with_context(|| dest.display().to_string())?;
                // Read before the body: it counts down as the body is read.
                let expected = r.content_length();
                let mut n = 0u64;
                while let Some(chunk) = r.chunk().await? {
                    f.write_all(&chunk).await?;
                    n += chunk.len() as u64;
                }
                f.flush().await?;
                if let Some(len) = expected {
                    anyhow::ensure!(n == len, "got {n} bytes of {len}");
                }
                Ok(n)
            })
            .await?;
        if got.is_none() {
            tokio::fs::write(dest, b"").await?;
        }
        Ok(got)
    }
}

/// The longest wait before a retry, whatever the server asks.
pub const MAX_RETRY_WAIT: Duration = Duration::from_secs(600);

/// The wait before the try after `attempt`: doubling from the pacing delay, at
/// least what the server asked (`Retry-After`), and at most
/// [`MAX_RETRY_WAIT`], so a long hint can't keep the job asleep past its
/// deadline margin.
pub fn retry_wait(delay: Duration, attempt: u32, retry_after: Option<Duration>) -> Duration {
    let doubled = delay
        .max(Duration::from_secs(1))
        .saturating_mul(2u32.saturating_pow(attempt + 1));
    doubled
        .max(retry_after.unwrap_or_default())
        .min(MAX_RETRY_WAIT)
}

/// A page the server has not (it answered 404), kept so a resumed fetch
/// doesn't ask again.
pub const RAW_MISSING: &[u8] = b"";

/// A response, from the raw store when it holds it, else fetched (and kept
/// there). `None` when the server has no such page. Only a response `valid`
/// accepts is kept, since the raw store is create-only: a bad answer kept
/// there would be read back on every resumed fetch. A kept copy `valid`
/// refuses (one kept before the check) is passed over and fetched again. A
/// 404 is kept too ([`RAW_MISSING`]): a page the Globe's server has not.
pub async fn cached(
    fetch: &dyn Fetch,
    raw: Option<&dyn ObjectStore>,
    path: &str,
    url: &str,
    valid: fn(&[u8]) -> bool,
) -> anyhow::Result<Option<Vec<u8>>> {
    cache(fetch, raw, path, url, valid, true).await
}

/// [`cached`], keeping only a response the server has: a 404 is never kept,
/// and one kept by an earlier run is passed over, so a page missing for now
/// is asked for again by every run (and `--refetch`).
pub async fn cached_found(
    fetch: &dyn Fetch,
    raw: Option<&dyn ObjectStore>,
    path: &str,
    url: &str,
    valid: fn(&[u8]) -> bool,
) -> anyhow::Result<Option<Vec<u8>>> {
    cache(fetch, raw, path, url, valid, false).await
}

async fn cache(
    fetch: &dyn Fetch,
    raw: Option<&dyn ObjectStore>,
    path: &str,
    url: &str,
    valid: fn(&[u8]) -> bool,
    keep_404: bool,
) -> anyhow::Result<Option<Vec<u8>>> {
    if let Some(raw) = raw {
        if let Some(body) = read_kept(raw, path).await? {
            if body == RAW_MISSING {
                if keep_404 {
                    return Ok(None);
                }
                tracing::debug!(path, "a kept 404; asking again");
            } else if valid(&body) {
                return Ok(Some(body));
            } else {
                tracing::warn!(path, "a kept response isn't usable; fetching it again");
            }
        }
    }
    let body = fetch.get(url).await?;
    if let Some(raw) = raw {
        let keep_it = match body.as_deref() {
            Some(b) => valid(b),
            None => keep_404,
        };
        if keep_it {
            keep(raw, path, body.as_deref().unwrap_or(RAW_MISSING)).await?;
        }
    }
    Ok(body)
}

/// A kept object, decompressed.
pub async fn read_kept(raw: &dyn ObjectStore, path: &str) -> anyhow::Result<Option<Vec<u8>>> {
    match raw.get(path).await? {
        Some(z) => Ok(Some(
            zstd::decode_all(&z[..]).with_context(|| path.to_owned())?,
        )),
        None => Ok(None),
    }
}

/// Keep `body` zstd-compressed. Create-only: a copy already there (a racing
/// run) is the same response.
pub async fn keep(raw: &dyn ObjectStore, path: &str, body: &[u8]) -> anyhow::Result<()> {
    let z = zstd::encode_all(body, 9)?;
    raw.put_new(path, z, "application/zstd").await?;
    Ok(())
}

/// Hold the Cosmos lock `name` while `body` runs, renewing it every five
/// minutes; `lost` is set if a renewal fails, and the body is expected to
/// stop between requests when it is. `name` is an item id in the `ops`
/// container, so it has no `/`.
pub async fn locked<T>(
    state: &crate::state::State,
    name: &'static str,
    owner: &str,
    body: impl std::future::Future<Output = anyhow::Result<T>>,
    lost: Arc<AtomicBool>,
) -> anyhow::Result<T> {
    let ttl = chrono::Duration::minutes(30);
    state.lock(name, owner, ttl).await?;
    let (s, o, flag) = (state.clone(), owner.to_owned(), lost.clone());
    let (stop, mut stopped) = tokio::sync::watch::channel(false);
    let renewer = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(300)) => {}
                _ = stopped.changed() => break,
            }
            if let Err(e) = s.lock(name, &o, ttl).await {
                tracing::error!(lock = name, error = %format!("{e:#}"), "lock renewal failed");
                flag.store(true, Ordering::SeqCst);
                break;
            }
        }
    });
    let result = body.await;
    // Stopped between renewals, not aborted: a renewal under way when the
    // body ends would otherwise land after the unlock and hold the lock.
    let _ = stop.send(true);
    let _ = renewer.await;
    let unlocked = state.unlock(name, owner).await;
    let out = result?;
    unlocked?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// A download streams to the file, retries a 503, and leaves an empty
    /// file on a 404.
    #[tokio::test]
    async fn downloads_stream_to_a_file_and_retry() {
        use axum::http::StatusCode;
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let h = hits.clone();
        let big: &'static [u8] = Box::leak(vec![b'x'; 300_000].into_boxed_slice());
        let app = axum::Router::new().fallback(move |uri: axum::http::Uri| {
            let h = h.clone();
            async move {
                let n = h.fetch_add(1, Ordering::SeqCst);
                match uri.path() {
                    "/missing.pdf" => (StatusCode::NOT_FOUND, &b"no"[..]),
                    _ if n == 0 => (StatusCode::SERVICE_UNAVAILABLE, &b"later"[..]),
                    _ => (StatusCode::OK, big),
                }
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let http = Http::new(Duration::from_millis(10), "usnm-test", "test").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("a.pdf");
        let n = http
            .download(&format!("http://{addr}/a.pdf"), &dest)
            .await
            .unwrap();
        assert_eq!(n, Some(300_000));
        assert_eq!(std::fs::read(&dest).unwrap().len(), 300_000);
        assert_eq!(hits.load(Ordering::SeqCst), 2, "the 503 was retried");
        let n = http
            .download(&format!("http://{addr}/missing.pdf"), &dest)
            .await
            .unwrap();
        assert_eq!(n, None);
        assert!(std::fs::read(&dest).unwrap().is_empty());
    }

    /// A try gets its timeout, cut to what is left before the cutoff; none
    /// starts with less than `MIN_TRY` left, or after the cutoff.
    #[tokio::test(start_paused = true)]
    async fn tries_fit_before_the_cutoff() {
        let now = tokio::time::Instant::now();
        let s = Duration::from_secs;
        assert_eq!(
            try_budget(now, None, DOWNLOAD_TIMEOUT),
            Some(DOWNLOAD_TIMEOUT)
        );
        let cutoff = Some(now + s(3600));
        assert_eq!(try_budget(now, cutoff, GET_TIMEOUT), Some(GET_TIMEOUT));
        assert_eq!(
            try_budget(now, cutoff, DOWNLOAD_TIMEOUT),
            Some(DOWNLOAD_TIMEOUT)
        );
        // Ten minutes left: a download may take ten minutes, not thirty.
        assert_eq!(
            try_budget(now + s(3000), cutoff, DOWNLOAD_TIMEOUT),
            Some(s(600))
        );
        assert_eq!(
            try_budget(now + s(3600) - MIN_TRY, cutoff, GET_TIMEOUT),
            Some(MIN_TRY)
        );
        assert_eq!(try_budget(now + s(3597), cutoff, GET_TIMEOUT), None);
        assert_eq!(try_budget(now + s(3600), cutoff, GET_TIMEOUT), None);
        assert_eq!(try_budget(now + s(4000), cutoff, GET_TIMEOUT), None);
        // A timeout shorter than MIN_TRY needs only itself.
        let short = Duration::from_millis(200);
        assert_eq!(try_budget(now + s(3599), cutoff, short), Some(short));
        // The defaults: pages as the Globe had them, a longer allowance for PDFs.
        let http = Http::new(Duration::from_secs(1), "usnm-test", "test").unwrap();
        assert_eq!((http.get_timeout, http.download_timeout), (s(120), s(1800)));
        assert_eq!(http.cutoff, None);
    }

    /// A server that never answers: each try stops at its timeout, and no
    /// retry starts that the cutoff leaves no time for, so the request fails
    /// by the cutoff rather than after seven tries and their waits.
    #[tokio::test]
    async fn a_hung_server_is_given_up_on_by_the_cutoff() {
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let h = hits.clone();
        let app = axum::Router::new().fallback(move || {
            let h = h.clone();
            async move {
                h.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_secs(60)).await;
                "late"
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let started = tokio::time::Instant::now();
        // Tries of 300 ms; retries wait 2 s, then 4 s: the cutoff at 3 s
        // leaves room for the second try and not the third.
        let http = Http::new(Duration::from_millis(10), "usnm-test", "test")
            .unwrap()
            .with_timeouts(Duration::from_millis(300), Duration::from_millis(300))
            .with_cutoff(Some(started + Duration::from_secs(3)));
        let err = http
            .get(&format!("http://{addr}/mods.xml"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cutoff"), "{err:#}");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        // Past the cutoff, nothing is asked.
        let http = Http::new(Duration::from_millis(10), "usnm-test", "test")
            .unwrap()
            .with_cutoff(Some(tokio::time::Instant::now()));
        let dir = tempfile::tempdir().unwrap();
        assert!(http
            .download(&format!("http://{addr}/a.pdf"), &dir.path().join("a.pdf"))
            .await
            .is_err());
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    /// The Globe's cache keeps a 404; `cached_found` neither keeps one nor
    /// trusts one an earlier run kept.
    #[tokio::test]
    async fn only_cached_keeps_a_404() {
        struct Answers(std::sync::Mutex<Vec<Option<&'static [u8]>>>, AtomicUsizeRef);
        type AtomicUsizeRef = Arc<std::sync::atomic::AtomicUsize>;
        #[async_trait::async_trait]
        impl Fetch for Answers {
            async fn get(&self, _url: &str) -> anyhow::Result<Option<Vec<u8>>> {
                self.1.fetch_add(1, Ordering::SeqCst);
                Ok(self.0.lock().unwrap().remove(0).map(<[u8]>::to_vec))
            }
        }
        let valid = |b: &[u8]| b.starts_with(b"<mods");
        let dir = tempfile::tempdir().unwrap();
        let raw = usnm_store::LocalStore::new(dir.path());
        let asked = AtomicUsizeRef::default();
        let gi = Answers(
            std::sync::Mutex::new(vec![None, Some(b"<mods/>"), None]),
            asked.clone(),
        );
        // A 404, not kept: asked again, and found.
        assert_eq!(
            cached_found(&gi, Some(&raw), "a.zst", "u", valid)
                .await
                .unwrap(),
            None
        );
        assert!(!raw.exists("a.zst").await.unwrap());
        let got = cached_found(&gi, Some(&raw), "a.zst", "u", valid)
            .await
            .unwrap();
        assert_eq!(got.as_deref(), Some(&b"<mods/>"[..]));
        assert_eq!(asked.load(Ordering::SeqCst), 2);
        // The Globe's keeps the 404 and reads it back without asking.
        assert_eq!(
            cached(&gi, Some(&raw), "b.zst", "u", valid).await.unwrap(),
            None
        );
        assert_eq!(
            cached(&gi, Some(&raw), "b.zst", "u", valid).await.unwrap(),
            None
        );
        assert_eq!(asked.load(Ordering::SeqCst), 3);
        // A 404 an earlier run kept is passed over by `cached_found`.
        let again = Answers(std::sync::Mutex::new(vec![Some(b"<mods/>")]), asked.clone());
        let got = cached_found(&again, Some(&raw), "b.zst", "u", valid)
            .await
            .unwrap();
        assert_eq!(got.as_deref(), Some(&b"<mods/>"[..]));
        assert_eq!(asked.load(Ordering::SeqCst), 4);
    }
}
