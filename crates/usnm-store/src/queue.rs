//! Work queues: Azure Storage Queues over REST with Entra bearer tokens, or
//! a local directory for tests and trials. Shared by the on-prem OCR worker
//! and the cloud jobs that fill and drain its queues (12 §12.4): Entra
//! only, no SAS tokens, no redirects.
//!
//! The OCR exchange has three queues: `ocr-priority` (pages someone was shown), `ocr-bulk` (the
//! damage-ranked backlog) and `ocr-deadletter` (messages that failed too
//! often or can't be done). A received message is hidden for a visibility
//! timeout fixed when it is taken. The nodes can take, peek and delete
//! messages but not update them (ADR-0015), so the hide isn't renewed; the
//! worker deletes the message when the page is written, and a page not
//! written reappears when the timeout runs out.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use crate::credential::Credential;
use anyhow::{bail, Context};
use async_trait::async_trait;
use reqwest::{Method, StatusCode, Url};

pub const PRIORITY: &str = "ocr-priority";
pub const BULK: &str = "ocr-bulk";
pub const DEADLETTER: &str = "ocr-deadletter";

/// Queue service REST API version (bearer tokens need 2017-11-09 or later;
/// the same version the blob client uses).
pub const API_VERSION: &str = "2023-11-03";

/// Queue endpoint host suffixes (public, US Government and China clouds).
pub const QUEUE_HOST_SUFFIXES: [&str; 3] = [
    ".queue.core.windows.net",
    ".queue.core.usgovcloudapi.net",
    ".queue.core.chinacloudapi.cn",
];

/// A message as received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Received {
    pub queue: String,
    pub id: String,
    pub pop_receipt: String,
    pub dequeue_count: u32,
    pub text: String,
}

/// The message is gone or another receiver holds it: its pop receipt no
/// longer matches.
#[derive(Debug, thiserror::Error)]
#[error("lease on message `{0}` lost")]
pub struct LeaseLost(pub String);

#[async_trait]
pub trait Queue: Send + Sync + std::fmt::Debug {
    /// Take one message, hiding it for `visibility`.
    async fn receive(&self, queue: &str, visibility: Duration) -> anyhow::Result<Option<Received>>;
    /// Delete a held message. Fails with [`LeaseLost`] if it isn't held.
    async fn delete(&self, msg: &Received) -> anyhow::Result<()>;
    /// Add a message that never expires.
    async fn send(&self, queue: &str, text: &str) -> anyhow::Result<()>;
    /// About how many messages `queue` holds, hidden ones included (Get
    /// Queue Metadata's `x-ms-approximate-messages-count`).
    async fn approximate_count(&self, queue: &str) -> anyhow::Result<u64>;
    /// The texts of up to `n` (1 to 32) visible messages at the front of
    /// `queue`, without taking or hiding them (Peek Messages).
    async fn peek(&self, queue: &str, n: u32) -> anyhow::Result<Vec<String>> {
        let _ = (queue, n);
        bail!("this queue can't peek")
    }
}

/// Open a queue service: an `https://{account}.queue.core.windows.net` URL
/// uses Azure Storage Queues with the ambient Entra credential; anything
/// else is a local directory.
pub fn open(location: &str) -> anyhow::Result<Arc<dyn Queue>> {
    if location.starts_with("https://") || location.starts_with("http://") {
        Ok(Arc::new(AzureQueue::new(
            location,
            crate::credential::from_env(),
        )?))
    } else {
        Ok(Arc::new(LocalQueue::new(location)))
    }
}

// ------------------------------------------------------------------ Azure

pub struct AzureQueue {
    /// `https://{account}.queue.core.windows.net[/path]`, no trailing `/`.
    base: String,
    credential: Arc<Credential>,
    http: reqwest::Client,
}

impl std::fmt::Debug for AzureQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AzureQueue")
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

impl AzureQueue {
    /// `location` is the account's queue endpoint. SAS tokens are refused
    /// (Entra only); plain `http` only on loopback, for test servers.
    pub fn new(location: &str, credential: Arc<Credential>) -> anyhow::Result<Self> {
        let url = Url::parse(location).with_context(|| format!("queue URL `{location}`"))?;
        let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
        let queue_host = url.host_str().is_some_and(|h| {
            let h = h.to_ascii_lowercase();
            QUEUE_HOST_SUFFIXES
                .iter()
                .any(|s| h.len() > s.len() && h.ends_with(s))
        });
        match url.scheme() {
            "https" if queue_host || loopback => {}
            "http" if loopback => {}
            _ => bail!("queue URL `{location}` must be https://<account>.queue.core.windows.net"),
        }
        if url.query().is_some() || url.fragment().is_some() {
            bail!("queue URL must not carry a query (SAS tokens are not used)");
        }
        if !url.username().is_empty() || url.password().is_some() {
            bail!("queue URL must not carry credentials");
        }
        let path = url.path().trim_matches('/');
        let base = if path.is_empty() {
            url.origin().ascii_serialization()
        } else {
            format!("{}/{path}", url.origin().ascii_serialization())
        };
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            // Never carry the bearer token anywhere but the configured host.
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            base,
            credential,
            http,
        })
    }

    async fn request(&self, method: Method, path: &str) -> anyhow::Result<reqwest::RequestBuilder> {
        let token = self.credential.token().await?;
        Ok(self
            .http
            .request(method, format!("{}/{path}", self.base))
            .bearer_auth(token)
            .header("x-ms-version", API_VERSION)
            .header("x-ms-date", httpdate::fmt_http_date(SystemTime::now())))
    }
}

fn check_queue_name(queue: &str) -> anyhow::Result<()> {
    // Azure: 3-63 lower-case letters, digits and single hyphens.
    let ok = (3..=63).contains(&queue.len())
        && queue
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !queue.starts_with('-')
        && !queue.ends_with('-')
        && !queue.contains("--");
    if ok {
        Ok(())
    } else {
        bail!("invalid queue name `{queue}`")
    }
}

fn check_id(id: &str) -> anyhow::Result<()> {
    if !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        Ok(())
    } else {
        bail!("invalid message id")
    }
}

/// The text of the first `<tag>…</tag>` in `xml`, with XML's five entities decoded.
fn xml_value(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let rest = xml.split_once(open.as_str())?.1;
    let v = rest.split_once(close.as_str())?.0;
    Some(
        v.replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&amp;", "&"),
    )
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The first message in a Get Messages response, if any.
fn parse_received(queue: &str, xml: &str) -> anyhow::Result<Option<Received>> {
    let Some(msg) = xml_value(xml, "QueueMessage") else {
        return Ok(None);
    };
    let field = |t: &str| xml_value(&msg, t).with_context(|| format!("message has no {t}"));
    let id = field("MessageId")?;
    check_id(&id)?;
    Ok(Some(Received {
        queue: queue.to_owned(),
        id,
        pop_receipt: field("PopReceipt")?,
        dequeue_count: field("DequeueCount")?.trim().parse()?,
        text: field("MessageText")?,
    }))
}

/// Every message's text in a Peek Messages response.
fn parse_peeked(xml: &str) -> Vec<String> {
    xml.split("<QueueMessage>")
        .skip(1)
        .filter_map(|m| xml_value(m, "MessageText"))
        .collect()
}

fn lease_error(status: StatusCode, msg: &Received, op: &str) -> anyhow::Error {
    // 404: the message is gone (deleted, or its pop receipt is stale);
    // 400 with PopReceiptMismatch: another receiver took it.
    if status == StatusCode::NOT_FOUND || status == StatusCode::BAD_REQUEST {
        LeaseLost(msg.id.clone()).into()
    } else {
        anyhow::anyhow!("{op} on `{}` returned HTTP {}", msg.queue, status.as_u16())
    }
}

fn secs(d: Duration) -> String {
    d.as_secs().to_string()
}

#[async_trait]
impl Queue for AzureQueue {
    async fn receive(&self, queue: &str, visibility: Duration) -> anyhow::Result<Option<Received>> {
        check_queue_name(queue)?;
        let resp = self
            .request(Method::GET, &format!("{queue}/messages"))
            .await?
            .query(&[
                ("numofmessages", "1".to_owned()),
                ("visibilitytimeout", secs(visibility)),
            ])
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("get messages `{queue}`: {}", e.without_url()))?;
        if !resp.status().is_success() {
            bail!(
                "get messages `{queue}` returned HTTP {}",
                resp.status().as_u16()
            );
        }
        let xml = resp.text().await?;
        parse_received(queue, &xml)
    }

    async fn delete(&self, msg: &Received) -> anyhow::Result<()> {
        check_queue_name(&msg.queue)?;
        check_id(&msg.id)?;
        let resp = self
            .request(
                Method::DELETE,
                &format!("{}/messages/{}", msg.queue, msg.id),
            )
            .await?
            .query(&[("popreceipt", msg.pop_receipt.as_str())])
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("delete message: {}", e.without_url()))?;
        if resp.status() != StatusCode::NO_CONTENT {
            return Err(lease_error(resp.status(), msg, "delete message"));
        }
        Ok(())
    }

    async fn send(&self, queue: &str, text: &str) -> anyhow::Result<()> {
        check_queue_name(queue)?;
        let body = format!(
            "<QueueMessage><MessageText>{}</MessageText></QueueMessage>",
            xml_escape(text)
        );
        let resp = self
            .request(Method::POST, &format!("{queue}/messages"))
            .await?
            // -1: never expires (API 2017-07-29 and later).
            .query(&[("messagettl", "-1")])
            .header(reqwest::header::CONTENT_TYPE, "application/xml")
            .body(body)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("put message `{queue}`: {}", e.without_url()))?;
        if resp.status() != StatusCode::CREATED {
            bail!(
                "put message `{queue}` returned HTTP {}",
                resp.status().as_u16()
            );
        }
        Ok(())
    }

    async fn approximate_count(&self, queue: &str) -> anyhow::Result<u64> {
        check_queue_name(queue)?;
        let resp = self
            .request(Method::GET, queue)
            .await?
            .query(&[("comp", "metadata")])
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("get queue metadata `{queue}`: {}", e.without_url()))?;
        if !resp.status().is_success() {
            bail!(
                "get queue metadata `{queue}` returned HTTP {}",
                resp.status().as_u16()
            );
        }
        resp.headers()
            .get("x-ms-approximate-messages-count")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse().ok())
            .with_context(|| format!("queue `{queue}`: no x-ms-approximate-messages-count"))
    }

    async fn peek(&self, queue: &str, n: u32) -> anyhow::Result<Vec<String>> {
        check_queue_name(queue)?;
        let resp = self
            .request(Method::GET, &format!("{queue}/messages"))
            .await?
            .query(&[
                ("peekonly", "true".to_owned()),
                ("numofmessages", n.clamp(1, 32).to_string()),
            ])
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("peek messages `{queue}`: {}", e.without_url()))?;
        if !resp.status().is_success() {
            bail!(
                "peek messages `{queue}` returned HTTP {}",
                resp.status().as_u16()
            );
        }
        Ok(parse_peeked(&resp.text().await?))
    }
}

// ------------------------------------------------------------------ local

/// Queues in a directory: `<root>/<queue>/ready/<id>` holds a visible
/// message, `<root>/<queue>/leased/<id>.<receipt>.<visible_at_ms>` a held
/// one, named with the moment it becomes visible again. Receiving renames a
/// message from `ready` to `leased` (atomic, so two workers never take the
/// same one); an expired
/// lease is renamed back before each receive. Each file is JSON
/// `{"dequeue_count": n, "text": "..."}`.
#[derive(Debug, Clone)]
pub struct LocalQueue {
    root: PathBuf,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct LocalMessage {
    dequeue_count: u32,
    text: String,
}

/// A random hex token: hashes keyed by `RandomState` (seeded from the OS)
/// over the time and a counter.
pub fn random_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(unix_nanos());
    h.write_u32(std::process::id());
    h.write_u64(SEQ.fetch_add(1, Ordering::Relaxed));
    format!("{:016x}", h.finish())
}

fn unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

fn visible_at_ms(visibility: Duration) -> u128 {
    (unix_nanos() + visibility.as_nanos()) / 1_000_000
}

impl LocalQueue {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn dir(&self, queue: &str, sub: &str) -> anyhow::Result<PathBuf> {
        check_queue_name(queue)?;
        let d = self.root.join(queue).join(sub);
        std::fs::create_dir_all(&d)?;
        Ok(d)
    }

    /// The held message's file, if `msg`'s receipt still holds it.
    fn leased_path(&self, msg: &Received) -> anyhow::Result<PathBuf> {
        check_id(&msg.id)?;
        if msg.pop_receipt.is_empty() || !msg.pop_receipt.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("invalid pop receipt");
        }
        let leased = self.dir(&msg.queue, "leased")?;
        let prefix = format!("{}.{}.", msg.id, msg.pop_receipt);
        for entry in std::fs::read_dir(&leased)? {
            let name = entry?.file_name().to_string_lossy().into_owned();
            if name.starts_with(&prefix) {
                return Ok(leased.join(name));
            }
        }
        Err(LeaseLost(msg.id.clone()).into())
    }

    /// Return expired leases to `ready`.
    fn requeue_expired(&self, queue: &str) -> anyhow::Result<()> {
        let leased = self.dir(queue, "leased")?;
        let ready = self.dir(queue, "ready")?;
        let now = visible_at_ms(Duration::ZERO);
        for entry in std::fs::read_dir(&leased)? {
            let name = entry?.file_name().to_string_lossy().into_owned();
            let mut parts = name.split('.');
            let (Some(id), Some(_), Some(at)) = (parts.next(), parts.next(), parts.next()) else {
                continue;
            };
            if at.parse::<u128>().is_ok_and(|at| at <= now) {
                // Another worker may win the rename; either way it's back.
                let _ = std::fs::rename(leased.join(&name), ready.join(id));
            }
        }
        Ok(())
    }
}

fn lost_if_missing(e: std::io::Error, msg: &Received) -> anyhow::Error {
    if e.kind() == std::io::ErrorKind::NotFound {
        LeaseLost(msg.id.clone()).into()
    } else {
        e.into()
    }
}

#[async_trait]
impl Queue for LocalQueue {
    async fn receive(&self, queue: &str, visibility: Duration) -> anyhow::Result<Option<Received>> {
        self.requeue_expired(queue)?;
        let ready = self.dir(queue, "ready")?;
        let leased = self.dir(queue, "leased")?;
        let mut names: Vec<String> = std::fs::read_dir(&ready)?
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| check_id(n).is_ok())
            .collect();
        names.sort();
        for id in names {
            let receipt = random_token();
            let to = leased.join(format!("{id}.{receipt}.{}", visible_at_ms(visibility)));
            if std::fs::rename(ready.join(&id), &to).is_err() {
                continue; // another worker took it
            }
            let mut m: LocalMessage = serde_json::from_slice(&std::fs::read(&to)?)
                .with_context(|| format!("local message `{id}`"))?;
            m.dequeue_count += 1;
            std::fs::write(&to, serde_json::to_vec(&m)?)?;
            return Ok(Some(Received {
                queue: queue.to_owned(),
                id,
                pop_receipt: receipt,
                dequeue_count: m.dequeue_count,
                text: m.text,
            }));
        }
        Ok(None)
    }

    async fn delete(&self, msg: &Received) -> anyhow::Result<()> {
        let path = self.leased_path(msg)?;
        std::fs::remove_file(&path).map_err(|e| lost_if_missing(e, msg))
    }

    async fn send(&self, queue: &str, text: &str) -> anyhow::Result<()> {
        let ready = self.dir(queue, "ready")?;
        // Sorted by time, so the oldest is taken first.
        let id = format!("{:024}-{}", unix_nanos(), random_token());
        let tmp = ready
            .parent()
            .expect("queue dir")
            .join(format!(".{id}.tmp"));
        std::fs::write(
            &tmp,
            serde_json::to_vec(&LocalMessage {
                dequeue_count: 0,
                text: text.to_owned(),
            })?,
        )?;
        std::fs::rename(&tmp, ready.join(id))?;
        Ok(())
    }

    async fn approximate_count(&self, queue: &str) -> anyhow::Result<u64> {
        let mut n = 0u64;
        for sub in ["ready", "leased"] {
            n += std::fs::read_dir(self.dir(queue, sub)?)?
                .filter_map(|e| e.ok())
                .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
                .count() as u64;
        }
        Ok(n)
    }

    async fn peek(&self, queue: &str, n: u32) -> anyhow::Result<Vec<String>> {
        self.requeue_expired(queue)?;
        let ready = self.dir(queue, "ready")?;
        let mut names: Vec<String> = std::fs::read_dir(&ready)?
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| !n.starts_with('.'))
            .collect();
        names.sort();
        let mut out = Vec::new();
        for name in names.into_iter().take(n.clamp(1, 32) as usize) {
            // Taken since it was listed: skip it.
            let Ok(b) = std::fs::read(ready.join(&name)) else {
                continue;
            };
            let m: LocalMessage = serde_json::from_slice(&b)
                .with_context(|| format!("local queue message {name}"))?;
            out.push(m.text);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn peeking_takes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let q = LocalQueue::new(dir.path());
        q.send(DEADLETTER, "first").await.unwrap();
        q.send(DEADLETTER, "second").await.unwrap();
        assert_eq!(q.peek(DEADLETTER, 1).await.unwrap(), ["first"]);
        assert_eq!(q.peek(DEADLETTER, 32).await.unwrap(), ["first", "second"]);
        let m = q
            .receive(DEADLETTER, Duration::from_secs(600))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(m.text, "first");
        // A held message isn't visible to a peek.
        assert_eq!(q.peek(DEADLETTER, 32).await.unwrap(), ["second"]);
        let xml = "<QueueMessagesList><QueueMessage><MessageId>a</MessageId><MessageText>x&amp;y</MessageText></QueueMessage>\
                   <QueueMessage><MessageId>b</MessageId><MessageText>z</MessageText></QueueMessage></QueueMessagesList>";
        assert_eq!(parse_peeked(xml), ["x&y", "z"]);
        assert!(parse_peeked("<QueueMessagesList />").is_empty());
    }

    #[tokio::test]
    async fn local_queue_leases_and_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let q = LocalQueue::new(dir.path());
        assert_eq!(
            q.receive(BULK, Duration::from_secs(600)).await.unwrap(),
            None
        );
        q.send(BULK, "first").await.unwrap();
        q.send(BULK, "second").await.unwrap();
        assert_eq!(q.approximate_count(BULK).await.unwrap(), 2);
        let a = q
            .receive(BULK, Duration::from_secs(600))
            .await
            .unwrap()
            .unwrap();
        assert_eq!((a.text.as_str(), a.dequeue_count), ("first", 1));
        let b = q
            .receive(BULK, Duration::from_secs(600))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(b.text, "second");
        // Both held: nothing to take.
        assert_eq!(
            q.receive(BULK, Duration::from_secs(600)).await.unwrap(),
            None
        );
        // Hidden messages count too, as Azure counts them.
        assert_eq!(q.approximate_count(BULK).await.unwrap(), 2);
        q.delete(&a).await.unwrap();
        assert_eq!(q.approximate_count(BULK).await.unwrap(), 1);
        // Deleted: a second delete has nothing to delete.
        assert!(q
            .delete(&a)
            .await
            .unwrap_err()
            .downcast_ref::<LeaseLost>()
            .is_some());
        let stale = Received {
            pop_receipt: "0123".into(),
            ..b.clone()
        };
        assert!(q.delete(&stale).await.is_err());
        drop(b);
        let b2 = q.receive(BULK, Duration::from_secs(600)).await.unwrap();
        assert_eq!(b2, None, "still hidden");
    }

    #[tokio::test]
    async fn an_expired_local_lease_comes_back() {
        let dir = tempfile::tempdir().unwrap();
        let q = LocalQueue::new(dir.path());
        q.send(PRIORITY, "p").await.unwrap();
        let m = q.receive(PRIORITY, Duration::ZERO).await.unwrap().unwrap();
        let again = q
            .receive(PRIORITY, Duration::from_secs(600))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(again.id, m.id);
        assert_eq!(again.dequeue_count, 2);
        // The crashed worker's receipt is stale.
        assert!(q
            .delete(&m)
            .await
            .unwrap_err()
            .downcast_ref::<LeaseLost>()
            .is_some());
    }

    #[test]
    fn parses_get_messages() {
        let xml = "<?xml version=\"1.0\" encoding=\"utf-8\"?><QueueMessagesList><QueueMessage>\
            <MessageId>5974b586-0df3-4e2d-ad0c-18e3892bfca2</MessageId>\
            <InsertionTime>Fri, 09 Oct 2026 21:04:30 GMT</InsertionTime>\
            <ExpirationTime>Fri, 31 Dec 9999 23:59:59 GMT</ExpirationTime>\
            <PopReceipt>YzQ4Yzg1MDItYTc0Ny00OWNjLTkxYTUtZGM0MDFiZDAwYzEw</PopReceipt>\
            <TimeNextVisible>Fri, 09 Oct 2026 21:14:30 GMT</TimeNextVisible>\
            <DequeueCount>3</DequeueCount><MessageText>eyJ2IjoxfQ==</MessageText>\
            </QueueMessage></QueueMessagesList>";
        let m = parse_received(BULK, xml).unwrap().unwrap();
        assert_eq!(m.id, "5974b586-0df3-4e2d-ad0c-18e3892bfca2");
        assert_eq!(m.dequeue_count, 3);
        assert_eq!(m.text, "eyJ2IjoxfQ==");
        assert_eq!(parse_received(BULK, "<QueueMessagesList />").unwrap(), None);
    }

    #[test]
    fn queue_locations() {
        let cred = || Arc::new(Credential::new(crate::credential::StaticToken("t".into())));
        let ok = AzureQueue::new("https://acct.queue.core.windows.net/", cred()).unwrap();
        assert_eq!(ok.base, "https://acct.queue.core.windows.net");
        assert!(AzureQueue::new("http://127.0.0.1:10001/devstoreaccount1", cred()).is_ok());
        for bad in [
            "http://acct.queue.core.windows.net",
            "https://acct.blob.core.windows.net",
            "https://acct.queue.core.windows.net/?sv=2024",
            "https://example.com",
            "https://acct.queue.core.windows.net.evil.example",
        ] {
            assert!(AzureQueue::new(bad, cred()).is_err(), "{bad}");
        }
        assert!(check_queue_name("ocr-bulk").is_ok());
        assert!(check_queue_name("../x").is_err());
        assert!(check_id("../x").is_err());
    }

    /// A stand-in queue service: checks auth and version, holds one message.
    async fn fake_service() -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        use axum::extract::{Path, Query};
        use axum::http::{HeaderMap, StatusCode as S};
        use std::collections::HashMap;
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let check = |h: &HeaderMap| {
            assert_eq!(h["authorization"], "Bearer t");
            assert_eq!(h["x-ms-version"], API_VERSION);
        };
        let (l1, l2, l4) = (log.clone(), log.clone(), log.clone());
        let l3 = log.clone();
        let app = axum::Router::new()
            .route(
                "/{queue}",
                axum::routing::get(
                    move |h: HeaderMap,
                          Path(q): Path<String>,
                          Query(p): Query<HashMap<String, String>>| {
                        check(&h);
                        l3.lock()
                            .unwrap()
                            .push(format!("metadata {q} {}", p["comp"]));
                        async { ([("x-ms-approximate-messages-count", "42")], "") }
                    },
                ),
            )
            .route(
                "/{queue}/messages",
                axum::routing::get(
                    move |h: HeaderMap,
                          Path(q): Path<String>,
                          Query(p): Query<HashMap<String, String>>| {
                        check(&h);
                        l1.lock()
                            .unwrap()
                            .push(format!("get {q} {}", p["visibilitytimeout"]));
                        async move {
                            if q == "ocr-priority" {
                                return "<QueueMessagesList />".to_owned();
                            }
                            "<QueueMessagesList><QueueMessage><MessageId>m1</MessageId>\
                             <PopReceipt>p/1+=</PopReceipt><DequeueCount>1</DequeueCount>\
                             <MessageText>a&amp;b</MessageText></QueueMessage></QueueMessagesList>"
                                .to_owned()
                        }
                    },
                )
                .post(
                    move |h: HeaderMap,
                          Path(q): Path<String>,
                          Query(p): Query<HashMap<String, String>>,
                          body: String| {
                        check(&h);
                        l2.lock()
                            .unwrap()
                            .push(format!("put {q} {} {body}", p["messagettl"]));
                        async { S::CREATED }
                    },
                ),
            )
            .route(
                "/{queue}/messages/{id}",
                axum::routing::delete(
                    move |h: HeaderMap,
                          Path((q, id)): Path<(String, String)>,
                          Query(p): Query<HashMap<String, String>>| {
                        check(&h);
                        l4.lock()
                            .unwrap()
                            .push(format!("delete {q} {id} {}", p["popreceipt"]));
                        async move {
                            if p["popreceipt"] == "stale" {
                                S::NOT_FOUND
                            } else {
                                S::NO_CONTENT
                            }
                        }
                    },
                ),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), log)
    }

    #[tokio::test]
    async fn azure_queue_rest_calls() {
        let (url, log) = fake_service().await;
        let q = AzureQueue::new(
            &url,
            Arc::new(Credential::new(crate::credential::StaticToken("t".into()))),
        )
        .unwrap();
        assert_eq!(
            q.receive(PRIORITY, Duration::from_secs(600)).await.unwrap(),
            None
        );
        let m = q
            .receive(BULK, Duration::from_secs(600))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (m.id.as_str(), m.pop_receipt.as_str(), m.text.as_str()),
            ("m1", "p/1+=", "a&b")
        );
        let stale = Received {
            pop_receipt: "stale".into(),
            ..m.clone()
        };
        assert!(q
            .delete(&stale)
            .await
            .unwrap_err()
            .downcast_ref::<LeaseLost>()
            .is_some());
        q.delete(&m).await.unwrap();
        q.send(DEADLETTER, "x<y").await.unwrap();
        assert_eq!(q.approximate_count(BULK).await.unwrap(), 42);
        assert_eq!(
            *log.lock().unwrap(),
            [
                "get ocr-priority 600",
                "get ocr-bulk 600",
                "delete ocr-bulk m1 stale",
                "delete ocr-bulk m1 p/1+=",
                "put ocr-deadletter -1 <QueueMessage><MessageText>x&lt;y</MessageText></QueueMessage>",
                "metadata ocr-bulk metadata",
            ]
        );
    }
}
