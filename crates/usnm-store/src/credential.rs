//! Entra ID access tokens for Azure services (Blob Storage, Cosmos DB, Azure
//! Monitor ingestion).
//!
//! In Container Apps the managed identity endpoint (`IDENTITY_ENDPOINT` +
//! `IDENTITY_HEADER`) issues tokens; `AZURE_CLIENT_ID` selects the
//! user-assigned identity (`id-usnm-app`). On an Azure Arc-enabled server
//! (the on-prem OCR nodes, #296) the Connected Machine agent's endpoint
//! (`IDENTITY_ENDPOINT` + `IMDS_ENDPOINT`, no `IDENTITY_HEADER`) issues them
//! after a challenge ([`ArcIdentity`]). Elsewhere (a developer machine) the
//! Azure CLI's signed-in account is used. Tokens are cached until five minutes
//! before they expire, and one refresh at a time is in flight.

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::StoreError;

/// Token audience for Azure Storage.
pub const STORAGE_RESOURCE: &str = "https://storage.azure.com/";

/// Token audience for the Cosmos DB data plane.
pub const COSMOS_RESOURCE: &str = "https://cosmos.azure.com";

/// Token audience for Azure Monitor ingestion (Application Insights with
/// local auth disabled). With the trailing slash, as Microsoft's exporters
/// request it (scope `https://monitor.azure.com//.default`).
pub const MONITOR_RESOURCE: &str = "https://monitor.azure.com/";

const REFRESH_MARGIN: Duration = Duration::from_secs(300);

#[derive(Clone)]
pub struct AccessToken {
    pub token: String,
    pub expires_at: SystemTime,
}

impl fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccessToken")
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

#[async_trait]
pub trait TokenSource: Send + Sync + fmt::Debug {
    async fn fetch(&self) -> Result<AccessToken, StoreError>;
}

/// Caches a [`TokenSource`]'s token and refreshes it before it expires.
#[derive(Debug)]
pub struct Credential {
    source: Box<dyn TokenSource>,
    cached: Mutex<Option<AccessToken>>,
}

impl Credential {
    pub fn new(source: impl TokenSource + 'static) -> Self {
        Self {
            source: Box::new(source),
            cached: Mutex::new(None),
        }
    }

    pub async fn token(&self) -> Result<String, StoreError> {
        let mut cached = self.cached.lock().await;
        if let Some(t) = cached.as_ref() {
            if t.expires_at > SystemTime::now() + REFRESH_MARGIN {
                return Ok(t.token.clone());
            }
        }
        let fresh = self.source.fetch().await?;
        let token = fresh.token.clone();
        *cached = Some(fresh);
        Ok(token)
    }
}

/// The ambient credential for Blob Storage: managed identity when its
/// endpoint is present, otherwise the Azure CLI.
pub fn from_env() -> Arc<Credential> {
    from_env_for(STORAGE_RESOURCE)
}

/// The ambient credential for another resource (e.g. [`COSMOS_RESOURCE`]).
pub fn from_env_for(resource: &'static str) -> Arc<Credential> {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    match (
        var("IDENTITY_ENDPOINT"),
        var("IDENTITY_HEADER"),
        var("IMDS_ENDPOINT"),
    ) {
        (Some(endpoint), Some(header), _) => Arc::new(Credential::new(
            ManagedIdentity::new(endpoint, header, var("AZURE_CLIENT_ID")).with_resource(resource),
        )),
        // The Arc agent sets both endpoints and no header.
        (Some(endpoint), None, Some(_)) => Arc::new(Credential::new(
            ArcIdentity::new(endpoint).with_resource(resource),
        )),
        _ => Arc::new(Credential::new(AzureCli { resource })),
    }
}

/// The App Service / Container Apps managed identity endpoint (API version 2019-08-01).
pub struct ManagedIdentity {
    endpoint: String,
    header: String,
    client_id: Option<String>,
    resource: &'static str,
    http: reqwest::Client,
}

impl ManagedIdentity {
    pub fn new(endpoint: String, header: String, client_id: Option<String>) -> Self {
        Self {
            endpoint,
            header,
            client_id,
            resource: STORAGE_RESOURCE,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()
                .expect("static client config"),
        }
    }

    pub fn with_resource(mut self, resource: &'static str) -> Self {
        self.resource = resource;
        self
    }
}

impl fmt::Debug for ManagedIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ManagedIdentity")
            .field("endpoint", &self.endpoint)
            .field("client_id", &self.client_id)
            .field("resource", &self.resource)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl TokenSource for ManagedIdentity {
    async fn fetch(&self) -> Result<AccessToken, StoreError> {
        let mut query = vec![("api-version", "2019-08-01"), ("resource", self.resource)];
        if let Some(id) = &self.client_id {
            query.push(("client_id", id));
        }
        let resp = self
            .http
            .get(&self.endpoint)
            .query(&query)
            .header("X-IDENTITY-HEADER", &self.header)
            .send()
            .await
            .map_err(|e| StoreError::Credential(format!("managed identity: {e}")))?;
        if !resp.status().is_success() {
            return Err(StoreError::Credential(format!(
                "managed identity returned HTTP {}",
                resp.status().as_u16()
            )));
        }
        let body: Value = resp
            .json()
            .await
            .map_err(|e| StoreError::Credential(format!("managed identity: {e}")))?;
        parse_token(&body, "access_token", "expires_on")
    }
}

/// Where the Arc agent writes its challenge files on Linux.
pub const ARC_TOKEN_DIR: &str = "/var/opt/azcmagent/tokens";

/// The largest challenge file read: the agent's are a few dozen bytes.
const ARC_KEY_MAX_BYTES: u64 = 4096;

/// The managed identity of an Azure Arc-enabled server: the Connected
/// Machine agent's local endpoint (`http://localhost:40342/metadata/identity/oauth2/token`,
/// API version 2020-06-01). The first request is answered 401 with
/// `WWW-Authenticate: Basic realm=<file>`; the file, readable only by root
/// and the `himds` group, holds a one-time secret, sent back as
/// `Authorization: Basic <secret>`. Proving local file access is what
/// authenticates the caller, so the file must be one of the agent's:
/// under [`ARC_TOKEN_DIR`], named `*.key`, at most 4 KiB. The endpoint must be
/// on loopback, so the secret never leaves the machine. See
/// <https://learn.microsoft.com/en-us/azure/azure-arc/servers/managed-identity-authentication>.
pub struct ArcIdentity {
    endpoint: String,
    resource: &'static str,
    token_dir: std::path::PathBuf,
    http: reqwest::Client,
}

impl ArcIdentity {
    pub fn new(endpoint: String) -> Self {
        Self {
            endpoint,
            resource: STORAGE_RESOURCE,
            token_dir: ARC_TOKEN_DIR.into(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                // The endpoint is local: never through a proxy, never redirected.
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("static client config"),
        }
    }

    pub fn with_resource(mut self, resource: &'static str) -> Self {
        self.resource = resource;
        self
    }

    /// Accept challenge files from another directory (tests).
    pub fn with_token_dir(mut self, dir: impl Into<std::path::PathBuf>) -> Self {
        self.token_dir = dir.into();
        self
    }

    fn check_endpoint(&self) -> Result<(), StoreError> {
        let url = reqwest::Url::parse(&self.endpoint)
            .map_err(|e| StoreError::Credential(format!("Arc identity endpoint: {e}")))?;
        let loopback = url.host_str().is_some_and(|h| {
            h.eq_ignore_ascii_case("localhost")
                || h.trim_start_matches('[')
                    .trim_end_matches(']')
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        });
        if url.scheme() == "http" && loopback {
            Ok(())
        } else {
            Err(StoreError::Credential(
                "Arc identity endpoint must be http on loopback".into(),
            ))
        }
    }

    /// The secret in the challenge file `realm` names, after checking it is
    /// one of the agent's files.
    fn read_challenge(&self, realm: &str) -> Result<String, StoreError> {
        let bad = |why: &str| StoreError::Credential(format!("Arc identity challenge: {why}"));
        let path = std::path::Path::new(realm);
        if path.extension().and_then(|e| e.to_str()) != Some("key") {
            return Err(bad("the file isn't a .key file"));
        }
        let dir = self
            .token_dir
            .canonicalize()
            .map_err(|_| bad("the agent's token directory is missing"))?;
        let file = path
            .canonicalize()
            .map_err(|_| bad("the file can't be read"))?;
        if file.parent() != Some(dir.as_path()) {
            return Err(bad("the file isn't in the agent's token directory"));
        }
        let meta = std::fs::metadata(&file).map_err(|_| bad("the file can't be read"))?;
        if !meta.is_file() || meta.len() > ARC_KEY_MAX_BYTES {
            return Err(bad("the file is too large"));
        }
        let secret = std::fs::read_to_string(&file).map_err(|_| bad("the file can't be read"))?;
        Ok(secret.trim().to_owned())
    }

    fn request(&self) -> reqwest::RequestBuilder {
        self.http
            .get(&self.endpoint)
            .query(&[("api-version", "2020-06-01"), ("resource", self.resource)])
            .header("Metadata", "true")
    }
}

impl fmt::Debug for ArcIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ArcIdentity")
            .field("endpoint", &self.endpoint)
            .field("resource", &self.resource)
            .finish_non_exhaustive()
    }
}

/// The file a `WWW-Authenticate: Basic realm=<file>` challenge names.
fn basic_realm(header: &str) -> Option<&str> {
    let rest = header.trim().strip_prefix("Basic")?.trim_start();
    let realm = rest.strip_prefix("realm=")?.trim();
    let realm = realm.trim_matches('"');
    (!realm.is_empty()).then_some(realm)
}

#[async_trait]
impl TokenSource for ArcIdentity {
    async fn fetch(&self) -> Result<AccessToken, StoreError> {
        self.check_endpoint()?;
        let err = |e: reqwest::Error| {
            StoreError::Credential(format!("Arc identity: {}", e.without_url()))
        };
        let first = self.request().send().await.map_err(err)?;
        if first.status() != reqwest::StatusCode::UNAUTHORIZED {
            return Err(StoreError::Credential(format!(
                "Arc identity returned HTTP {} without a challenge",
                first.status().as_u16()
            )));
        }
        let realm = first
            .headers()
            .get(reqwest::header::WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok())
            .and_then(basic_realm)
            .ok_or_else(|| {
                StoreError::Credential("Arc identity: no Basic realm in the challenge".into())
            })?
            .to_owned();
        let secret = self.read_challenge(&realm)?;
        let resp = self
            .request()
            .header(reqwest::header::AUTHORIZATION, format!("Basic {secret}"))
            .send()
            .await
            .map_err(err)?;
        if !resp.status().is_success() {
            return Err(StoreError::Credential(format!(
                "Arc identity returned HTTP {}",
                resp.status().as_u16()
            )));
        }
        let body: Value = resp.json().await.map_err(err)?;
        parse_token(&body, "access_token", "expires_on")
    }
}

/// `az account get-access-token` for local development.
#[derive(Debug)]
pub struct AzureCli {
    pub resource: &'static str,
}

#[async_trait]
impl TokenSource for AzureCli {
    async fn fetch(&self) -> Result<AccessToken, StoreError> {
        let out = tokio::process::Command::new("az")
            .args([
                "account",
                "get-access-token",
                "--resource",
                self.resource,
                "--output",
                "json",
            ])
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| {
                StoreError::Credential(format!(
                    "no managed identity endpoint and the Azure CLI is unavailable: {e}"
                ))
            })?;
        if !out.status.success() {
            return Err(StoreError::Credential(
                "`az account get-access-token` failed; run `az login`".into(),
            ));
        }
        let body: Value = serde_json::from_slice(&out.stdout)
            .map_err(|e| StoreError::Credential(format!("Azure CLI output: {e}")))?;
        parse_token(&body, "accessToken", "expires_on")
    }
}

/// A fixed token (tests and emulators). It never expires.
pub struct StaticToken(pub String);

impl fmt::Debug for StaticToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("StaticToken(..)")
    }
}

#[async_trait]
impl TokenSource for StaticToken {
    async fn fetch(&self) -> Result<AccessToken, StoreError> {
        Ok(AccessToken {
            token: self.0.clone(),
            expires_at: SystemTime::now() + Duration::from_secs(365 * 24 * 3600),
        })
    }
}

/// Read a token and its expiry (Unix seconds, as a number or a string). A
/// missing expiry is treated as ten minutes so the token is refreshed soon.
fn parse_token(body: &Value, token_key: &str, expiry_key: &str) -> Result<AccessToken, StoreError> {
    let token = body[token_key]
        .as_str()
        .filter(|t| !t.is_empty())
        .ok_or_else(|| StoreError::Credential(format!("response has no `{token_key}`")))?;
    let expires = match &body[expiry_key] {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    };
    let expires_at = expires.map_or_else(
        || SystemTime::now() + Duration::from_secs(600),
        |secs| UNIX_EPOCH + Duration::from_secs(secs),
    );
    Ok(AccessToken {
        token: token.to_owned(),
        expires_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn parses_string_and_numeric_expiry() {
        let t = parse_token(
            &serde_json::json!({"access_token": "a", "expires_on": "4102444800"}),
            "access_token",
            "expires_on",
        )
        .unwrap();
        assert_eq!(
            t.expires_at,
            UNIX_EPOCH + Duration::from_secs(4_102_444_800)
        );
        let t = parse_token(
            &serde_json::json!({"accessToken": "b", "expires_on": 4102444800u64}),
            "accessToken",
            "expires_on",
        )
        .unwrap();
        assert_eq!(t.token, "b");
        assert!(parse_token(&serde_json::json!({}), "access_token", "expires_on").is_err());
    }

    #[derive(Debug)]
    struct Counting(Arc<AtomicU32>, u64);

    #[async_trait]
    impl TokenSource for Counting {
        async fn fetch(&self) -> Result<AccessToken, StoreError> {
            let n = self.0.fetch_add(1, Ordering::SeqCst);
            Ok(AccessToken {
                token: format!("t{n}"),
                expires_at: SystemTime::now() + Duration::from_secs(self.1),
            })
        }
    }

    #[tokio::test]
    async fn caches_until_near_expiry() {
        let n = Arc::new(AtomicU32::new(0));
        let long = Credential::new(Counting(n.clone(), 3600));
        assert_eq!(long.token().await.unwrap(), "t0");
        assert_eq!(long.token().await.unwrap(), "t0");
        // Inside the refresh margin: fetched every time.
        let short = Credential::new(Counting(n.clone(), 60));
        assert_eq!(short.token().await.unwrap(), "t1");
        assert_eq!(short.token().await.unwrap(), "t2");
    }

    #[test]
    fn reads_the_realm_of_a_basic_challenge() {
        assert_eq!(
            basic_realm("Basic realm=/var/opt/azcmagent/tokens/a1.key"),
            Some("/var/opt/azcmagent/tokens/a1.key")
        );
        assert_eq!(basic_realm("Basic realm=\"/x/y.key\""), Some("/x/y.key"));
        assert_eq!(basic_realm("Bearer realm=/x.key"), None);
        assert_eq!(basic_realm("Basic realm="), None);
    }

    /// A stand-in for the Arc agent: a challenge naming `key`, then a token
    /// for the secret in it, for `STORAGE_RESOURCE`.
    async fn arc_agent(key: std::path::PathBuf) -> String {
        arc_agent_for(key, STORAGE_RESOURCE).await
    }

    /// [`arc_agent`], checking the token is asked for `resource`.
    async fn arc_agent_for(key: std::path::PathBuf, resource: &'static str) -> String {
        use axum::extract::Query;
        use axum::http::{HeaderMap, StatusCode};
        use axum::response::IntoResponse;
        use std::collections::HashMap;
        let app = axum::Router::new().route(
            "/metadata/identity/oauth2/token",
            axum::routing::get(
                move |headers: HeaderMap, Query(q): Query<HashMap<String, String>>| {
                    let key = key.clone();
                    async move {
                        assert_eq!(headers["metadata"], "true");
                        assert_eq!(q["api-version"], "2020-06-01");
                        assert_eq!(q["resource"], resource);
                        match headers.get("authorization").and_then(|v| v.to_str().ok()) {
                            None => (
                                StatusCode::UNAUTHORIZED,
                                [("www-authenticate", format!("Basic realm={}", key.display()))],
                            )
                                .into_response(),
                            Some("Basic s3cret") => axum::Json(serde_json::json!({
                                "access_token": "arc-token",
                                "expires_on": "4102444800",
                            }))
                            .into_response(),
                            Some(_) => StatusCode::FORBIDDEN.into_response(),
                        }
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}/metadata/identity/oauth2/token")
    }

    #[tokio::test]
    async fn answers_the_arc_agents_challenge() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("abc.key");
        std::fs::write(&key, "s3cret\n").unwrap();
        let endpoint = arc_agent(key).await;
        let cred = Credential::new(ArcIdentity::new(endpoint).with_token_dir(dir.path()));
        assert_eq!(cred.token().await.unwrap(), "arc-token");
    }

    /// The OCR nodes' telemetry: an Arc token for Azure Monitor ingestion.
    #[tokio::test]
    async fn arc_tokens_for_azure_monitor() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("abc.key");
        std::fs::write(&key, "s3cret").unwrap();
        let endpoint = arc_agent_for(key, MONITOR_RESOURCE).await;
        let cred = Credential::new(
            ArcIdentity::new(endpoint)
                .with_token_dir(dir.path())
                .with_resource(MONITOR_RESOURCE),
        );
        assert_eq!(cred.token().await.unwrap(), "arc-token");
    }

    #[tokio::test]
    async fn refuses_challenge_files_that_arent_the_agents() {
        let tokens = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let outside = elsewhere.path().join("abc.key");
        std::fs::write(&outside, "s3cret").unwrap();
        let not_key = tokens.path().join("abc.txt");
        std::fs::write(&not_key, "s3cret").unwrap();
        let big = tokens.path().join("big.key");
        std::fs::write(&big, vec![b'a'; 5000]).unwrap();
        // A path that climbs out of the directory.
        let climbing = tokens
            .path()
            .join("..")
            .join(elsewhere.path().file_name().unwrap())
            .join("abc.key");
        for file in [outside, not_key, big, climbing] {
            let endpoint = arc_agent(file.clone()).await;
            let arc = ArcIdentity::new(endpoint).with_token_dir(tokens.path());
            let err = arc.fetch().await.unwrap_err().to_string();
            assert!(err.contains("Arc identity challenge"), "{file:?}: {err}");
            assert!(!err.contains("s3cret"));
        }
    }

    #[tokio::test]
    async fn the_arc_endpoint_must_be_local() {
        let arc =
            ArcIdentity::new("http://example.com:40342/metadata/identity/oauth2/token".into());
        assert!(arc.fetch().await.is_err());
        let arc = ArcIdentity::new("https://localhost:40342/x".into());
        assert!(arc.fetch().await.is_err());
        assert!(ArcIdentity::new("http://localhost:40342/x".into())
            .check_endpoint()
            .is_ok());
        assert!(ArcIdentity::new("http://[::1]:40342/x".into())
            .check_endpoint()
            .is_ok());
    }
}
