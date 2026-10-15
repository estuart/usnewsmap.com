//! A stand-in for LoC in tests: files by path, redirects, and scripted
//! answers, with every request recorded. It builds batch directories (a
//! bag manifest with real md5s, issue METS, JP2s) the way LoC lays them out.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};

use crate::batch;

/// An answer given once, before the file.
#[derive(Debug, Clone)]
pub enum Scripted {
    Status(u16),
    /// A status with `Retry-After: <secs>`.
    RetryAfter(u16, u32),
    /// A status after a pause.
    Slow(u16, Duration),
    /// A bot challenge page, as Cloudflare sends one: HTML with
    /// `cf-mitigated: challenge`, with this status.
    Challenge(u16),
}

#[derive(Default)]
struct Inner {
    files: HashMap<String, Vec<u8>>,
    redirects: HashMap<String, String>,
    scripted: HashMap<String, VecDeque<Scripted>>,
    /// A bag's manifest lines, by batch path.
    bags: BTreeMap<String, Vec<String>>,
    hits: Vec<(std::time::Instant, String)>,
}

#[derive(Clone)]
pub struct StandIn {
    pub root: String,
    inner: Arc<Mutex<Inner>>,
}

/// A page of an issue: its seq, its frame and the JP2's bytes.
pub struct Page<'a> {
    pub seq: u16,
    pub frame: &'a str,
    pub jp2: &'a [u8],
}

async fn answer(State(s): State<StandIn>, uri: Uri) -> Response {
    let path = uri.path().to_owned();
    let (scripted, redirect, file) = {
        let mut i = s.inner.lock().unwrap();
        i.hits.push((std::time::Instant::now(), path.clone()));
        let scripted = i.scripted.get_mut(&path).and_then(|q| q.pop_front());
        (
            scripted,
            i.redirects.get(&path).cloned(),
            i.files.get(&path).cloned(),
        )
    };
    let status = |c: u16| StatusCode::from_u16(c).unwrap();
    match scripted {
        Some(Scripted::Status(c)) => return status(c).into_response(),
        Some(Scripted::RetryAfter(c, secs)) => {
            return (status(c), [(header::RETRY_AFTER, secs.to_string())]).into_response()
        }
        Some(Scripted::Slow(c, d)) => {
            tokio::time::sleep(d).await;
            return status(c).into_response();
        }
        Some(Scripted::Challenge(c)) => {
            return (
                status(c),
                [
                    (header::CONTENT_TYPE, "text/html; charset=UTF-8"),
                    (header::SERVER, "cloudflare"),
                    (header::HeaderName::from_static("cf-mitigated"), "challenge"),
                ],
                "<!DOCTYPE html><html><head><title>Just a moment...</title></head></html>",
            )
                .into_response()
        }
        None => {}
    }
    if let Some(to) = redirect {
        return (
            StatusCode::PERMANENT_REDIRECT,
            [(header::LOCATION, format!("{}{to}", s.root))],
        )
            .into_response();
    }
    match file {
        Some(b) => b.into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

impl StandIn {
    pub async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let s = StandIn {
            root: format!("http://{}", listener.local_addr().unwrap()),
            inner: Arc::default(),
        };
        let app = axum::Router::new().fallback(answer).with_state(s.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        s
    }

    /// The storage root a client should use (`…/storage-services/service/ndnp/`).
    pub fn storage_root(&self) -> String {
        format!("{}/storage-services/service/ndnp/", self.root)
    }

    pub fn batches_root(&self) -> String {
        format!("{}/data/batches/", self.root)
    }

    pub fn iiif_root(&self) -> String {
        format!("{}/iiif/", self.root)
    }

    pub fn put(&self, path: &str, body: impl Into<Vec<u8>>) {
        self.inner
            .lock()
            .unwrap()
            .files
            .insert(path.to_owned(), body.into());
    }

    pub fn redirect(&self, from: &str, to: &str) {
        self.inner
            .lock()
            .unwrap()
            .redirects
            .insert(from.to_owned(), to.to_owned());
    }

    /// Answer `path` with `s` once, before anything else queued for it.
    pub fn script(&self, path: &str, s: Scripted) {
        self.inner
            .lock()
            .unwrap()
            .scripted
            .entry(path.to_owned())
            .or_default()
            .push_back(s);
    }

    /// Requests whose path ends with `suffix`.
    pub fn hits(&self, suffix: &str) -> usize {
        self.times(suffix).len()
    }

    /// When each request whose path ends with `suffix` came.
    pub fn times(&self, suffix: &str) -> Vec<std::time::Instant> {
        self.inner
            .lock()
            .unwrap()
            .hits
            .iter()
            .filter(|(_, p)| p.ends_with(suffix))
            .map(|(t, _)| *t)
            .collect()
    }

    /// An issue in a bag at `batch_path`: its METS (pages in the order
    /// given), its JP2s and their manifest lines. Returns the METS path.
    pub fn add_issue(
        &self,
        batch_path: &str,
        lccn: &str,
        reel: &str,
        dir: &str,
        pages: &[Page<'_>],
    ) -> String {
        let mut dmd = String::new();
        let mut files = String::new();
        let mut divs = String::new();
        for (i, p) in pages.iter().enumerate() {
            let n = i + 1;
            dmd.push_str(&format!(
                r#"<dmdSec ID="pageModsBib{n}"><mdWrap MDTYPE="MODS"><xmlData><MODS:mods><MODS:part><MODS:extent unit="pages"><MODS:start>{}</MODS:start></MODS:extent></MODS:part></MODS:mods></xmlData></mdWrap></dmdSec>"#,
                p.seq
            ));
            files.push_str(&format!(
                r#"<fileGrp ID="pageFileGrp{n}"><file ID="masterFile{n}" USE="master"><FLocat xlink:href="./{f}.tif"/></file><file ID="serviceFile{n}" USE="service"><FLocat xlink:href="./{f}.jp2"/></file></fileGrp>"#,
                f = p.frame
            ));
            divs.push_str(&format!(
                r#"<div TYPE="np:page" DMDID="pageModsBib{n}"><fptr FILEID="masterFile{n}"/><fptr FILEID="serviceFile{n}"/></div>"#
            ));
        }
        let mets = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><mets xmlns:MODS="http://www.loc.gov/mods/v3" xmlns:xlink="http://www.w3.org/1999/xlink" xmlns="http://www.loc.gov/METS/">{dmd}<fileSec>{files}</fileSec><structMap><div TYPE="np:issue">{divs}</div></structMap></mets>"#
        );
        let issue = format!("data/{lccn}/{reel}/{dir}/");
        let mut lines = vec![format!(
            "{}  {issue}{dir}.xml",
            batch::md5_hex(mets.as_bytes())
        )];
        let mets_path = format!("{batch_path}{issue}{dir}.xml");
        self.put(&mets_path, mets);
        for p in pages {
            lines.push(format!("{}  {issue}{}.jp2", batch::md5_hex(p.jp2), p.frame));
            self.put(
                &format!("{batch_path}{issue}{}.jp2", p.frame),
                p.jp2.to_vec(),
            );
        }
        let manifest = {
            let mut i = self.inner.lock().unwrap();
            let bag = i.bags.entry(batch_path.to_owned()).or_default();
            bag.extend(lines);
            bag.join("\n") + "\n"
        };
        self.put(&format!("{batch_path}manifest-md5.txt"), manifest);
        mets_path
    }
}
