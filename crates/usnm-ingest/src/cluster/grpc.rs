//! Quickwit's per-split resource stats for a search (#251), which 0.9.1
//! reports only in its gRPC root search response (`SearchResponse.
//! resource_stats`, quickwit #6416): not in the REST response, not in its
//! metrics, and in its logs only for the top 5% of memory-hungry queries.
//!
//! The bench's probe sends one search to a node's gRPC port (REST + 1) as
//! `quickwit.search.SearchService/RootSearch`, the way the REST handler
//! builds it (`search_request_from_api_request`: the query string as a
//! `user_input` query AST with AND as the default operator), over HTTP/2
//! without TLS, and reads back the hit count and the stats. The few
//! protobuf fields needed are encoded and decoded here by field number
//! (`quickwit-proto/protos/quickwit/search.proto`), so the workspace needs
//! no protobuf toolchain.

use anyhow::{bail, Context};
use serde::Serialize;
use serde_json::{json, Value};

/// The RPC's path.
pub const ROOT_SEARCH: &str = "/quickwit.search.SearchService/RootSearch";

/// `SplitResourceStats`, summed over a search's locally executed splits.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SplitStats {
    pub split_num_docs: u64,
    pub input_memory_bytes: u64,
    pub download_num_bytes: u64,
    pub download_num_requests: u64,
    pub matched_num_docs: u64,
    pub wait_for_search_permit_microsecs: u64,
    /// Downloading and decoding what the split needs into caches (the main
    /// runtime's part of a split search).
    pub warmup_microsecs: u64,
    pub wait_for_cpu_pool_microsecs: u64,
    /// The search itself, on the search pool.
    pub cpu_search_microsecs: u64,
}

/// What a root search's `resource_stats` says, from its leaves' sum.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SearchStats {
    pub num_hits: u64,
    pub partial_result_cache_num_splits: u64,
    pub localexec_num_splits: u64,
    pub leaf_wall_time_microsecs: u64,
    pub search_pool_cpu_threads: u64,
    pub root_wall_time_microsecs: u64,
    pub splits: SplitStats,
}

impl SearchStats {
    /// Field-wise sum, for a pass.
    pub fn add(&mut self, o: &SearchStats) {
        self.num_hits += o.num_hits;
        self.partial_result_cache_num_splits += o.partial_result_cache_num_splits;
        self.localexec_num_splits += o.localexec_num_splits;
        self.leaf_wall_time_microsecs += o.leaf_wall_time_microsecs;
        self.search_pool_cpu_threads = self.search_pool_cpu_threads.max(o.search_pool_cpu_threads);
        self.root_wall_time_microsecs += o.root_wall_time_microsecs;
        let (a, b) = (&mut self.splits, &o.splits);
        a.split_num_docs += b.split_num_docs;
        a.input_memory_bytes += b.input_memory_bytes;
        a.download_num_bytes += b.download_num_bytes;
        a.download_num_requests += b.download_num_requests;
        a.matched_num_docs += b.matched_num_docs;
        a.wait_for_search_permit_microsecs += b.wait_for_search_permit_microsecs;
        a.warmup_microsecs += b.warmup_microsecs;
        a.wait_for_cpu_pool_microsecs += b.wait_for_cpu_pool_microsecs;
        a.cpu_search_microsecs += b.cpu_search_microsecs;
    }
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn put_bytes(out: &mut Vec<u8>, field: u32, bytes: &[u8]) {
    put_varint(out, u64::from(field << 3 | 2));
    put_varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

/// The `SearchRequest` the REST handler would build from a search body
/// (`query`, `max_hits`, `aggs`) on `indexes`.
pub fn search_request(indexes: &[String], body: &Value) -> anyhow::Result<Vec<u8>> {
    let query = body["query"]
        .as_str()
        .context("the search body has no query")?;
    let ast = json!({
        "type": "user_input",
        "user_text": query,
        "default_operator": "And",
        "lenient": false,
    });
    let mut out = Vec::new();
    for i in indexes {
        put_bytes(&mut out, 1, i.as_bytes());
    }
    let max_hits = body["max_hits"].as_u64().unwrap_or(0);
    if max_hits > 0 {
        put_varint(&mut out, 6 << 3);
        put_varint(&mut out, max_hits);
    }
    if let Some(aggs) = body.get("aggs").filter(|a| !a.is_null()) {
        put_bytes(&mut out, 11, serde_json::to_string(aggs)?.as_bytes());
    }
    put_bytes(&mut out, 13, serde_json::to_string(&ast)?.as_bytes());
    Ok(out)
}

/// A gRPC message frame: uncompressed, its length, the message.
pub fn frame(msg: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(msg.len() + 5);
    out.push(0);
    out.extend_from_slice(&(msg.len() as u32).to_be_bytes());
    out.extend_from_slice(msg);
    out
}

/// The message in a response's first frame.
pub fn unframe(body: &[u8]) -> anyhow::Result<&[u8]> {
    if body.len() < 5 {
        bail!("no gRPC message in the response");
    }
    if body[0] != 0 {
        bail!("a compressed gRPC message");
    }
    let len = u32::from_be_bytes([body[1], body[2], body[3], body[4]]) as usize;
    body.get(5..5 + len).context("a gRPC message cut short")
}

/// A protobuf message's fields: (field number, varint value or bytes).
enum Wire<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Other,
}

fn get_varint(b: &[u8], pos: &mut usize) -> anyhow::Result<u64> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *b.get(*pos).context("a varint cut short")?;
        *pos += 1;
        v |= u64::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            return Ok(v);
        }
    }
    bail!("a varint too long")
}

fn fields(b: &[u8]) -> anyhow::Result<Vec<(u32, Wire<'_>)>> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < b.len() {
        let key = get_varint(b, &mut pos)?;
        let field = (key >> 3) as u32;
        let wire = match key & 7 {
            0 => Wire::Varint(get_varint(b, &mut pos)?),
            1 => {
                pos += 8;
                Wire::Other
            }
            2 => {
                let len = get_varint(b, &mut pos)? as usize;
                let bytes = b.get(pos..pos + len).context("a field cut short")?;
                pos += len;
                Wire::Bytes(bytes)
            }
            5 => {
                pos += 4;
                Wire::Other
            }
            t => bail!("protobuf wire type {t}"),
        };
        out.push((field, wire));
    }
    if pos > b.len() {
        bail!("a field cut short");
    }
    Ok(out)
}

fn varints(b: &[u8]) -> anyhow::Result<std::collections::BTreeMap<u32, u64>> {
    Ok(fields(b)?
        .into_iter()
        .filter_map(|(f, w)| match w {
            Wire::Varint(v) => Some((f, v)),
            _ => None,
        })
        .collect())
}

fn message(b: &[u8], field: u32) -> anyhow::Result<Option<&[u8]>> {
    Ok(fields(b)?
        .into_iter()
        .find_map(|(f, w)| match (f == field, w) {
            (true, Wire::Bytes(m)) => Some(m),
            _ => None,
        }))
}

/// The hit count and the leaves' summed stats of a `SearchResponse`.
pub fn parse_response(msg: &[u8]) -> anyhow::Result<SearchStats> {
    let top = varints(msg)?;
    let mut s = SearchStats {
        num_hits: top.get(&1).copied().unwrap_or(0),
        ..SearchStats::default()
    };
    let Some(root) = message(msg, 10)? else {
        return Ok(s);
    };
    s.root_wall_time_microsecs = varints(root)?.get(&8).copied().unwrap_or(0);
    let Some(leaf) = message(root, 2)? else {
        return Ok(s);
    };
    let l = varints(leaf)?;
    let g = |m: &std::collections::BTreeMap<u32, u64>, f| m.get(&f).copied().unwrap_or(0);
    s.partial_result_cache_num_splits = g(&l, 1);
    s.localexec_num_splits = g(&l, 3);
    s.leaf_wall_time_microsecs = g(&l, 9);
    s.search_pool_cpu_threads = g(&l, 15);
    if let Some(split) = message(leaf, 6)? {
        let v = varints(split)?;
        s.splits = SplitStats {
            split_num_docs: g(&v, 1),
            input_memory_bytes: g(&v, 2),
            download_num_bytes: g(&v, 3),
            download_num_requests: g(&v, 4),
            matched_num_docs: g(&v, 5),
            wait_for_search_permit_microsecs: g(&v, 6),
            warmup_microsecs: g(&v, 7),
            wait_for_cpu_pool_microsecs: g(&v, 8),
            cpu_search_microsecs: g(&v, 9),
        };
    }
    Ok(s)
}

/// An HTTP/2 client without TLS, for gRPC.
pub fn client(timeout: std::time::Duration) -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .http2_prior_knowledge()
        .timeout(timeout)
        .build()?)
}

/// One root search on `grpc` (`http://ip:port`) and its stats.
pub async fn root_search(
    http: &reqwest::Client,
    grpc: &str,
    indexes: &[String],
    body: &Value,
) -> anyhow::Result<SearchStats> {
    let resp = http
        .post(format!("{}{ROOT_SEARCH}", grpc.trim_end_matches('/')))
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .body(frame(&search_request(indexes, body)?))
        .send()
        .await?;
    // A failure comes back as headers only, with grpc-status and no message.
    let status = resp
        .headers()
        .get("grpc-status")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let body = resp.bytes().await?;
    if body.is_empty() {
        bail!(
            "gRPC root search failed: grpc-status {}",
            status.as_deref().unwrap_or("missing")
        );
    }
    parse_response(unframe(&body)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(parts: &[(u32, Result<u64, Vec<u8>>)]) -> Vec<u8> {
        let mut out = Vec::new();
        for (f, v) in parts {
            match v {
                Ok(n) => {
                    put_varint(&mut out, u64::from(f << 3));
                    put_varint(&mut out, *n);
                }
                Err(b) => put_bytes(&mut out, *f, b),
            }
        }
        out
    }

    #[test]
    fn encodes_the_rest_handler_s_request() {
        let body =
            json!({"query": "text:radio", "max_hits": 0, "aggs": {"n": {"max": {"field": "day"}}}});
        let req = search_request(&["s1ixb".into(), "s1ixc".into()], &body).unwrap();
        let f = fields(&req).unwrap();
        let strings: Vec<(u32, String)> = f
            .iter()
            .filter_map(|(n, w)| match w {
                Wire::Bytes(b) => Some((*n, String::from_utf8(b.to_vec()).unwrap())),
                _ => None,
            })
            .collect();
        assert_eq!(strings[0], (1, "s1ixb".into()));
        assert_eq!(strings[1], (1, "s1ixc".into()));
        assert_eq!(strings[2], (11, r#"{"n":{"max":{"field":"day"}}}"#.into()));
        let ast: Value = serde_json::from_str(&strings[3].1).unwrap();
        assert_eq!(strings[3].0, 13);
        assert_eq!(
            ast,
            json!({"type": "user_input", "user_text": "text:radio", "default_operator": "And", "lenient": false})
        );
        // max_hits 0 is protobuf's default: not sent.
        assert!(!f.iter().any(|(n, _)| *n == 6));
        assert!(search_request(&[], &json!({})).is_err());
    }

    #[test]
    fn frames_round_trip() {
        let f = frame(b"abc");
        assert_eq!(f, [0, 0, 0, 0, 3, b'a', b'b', b'c']);
        assert_eq!(unframe(&f).unwrap(), b"abc");
        assert!(unframe(&[0, 0, 0, 0, 9, 1]).is_err());
        assert!(unframe(&[1, 0, 0, 0, 0]).is_err());
    }

    #[test]
    fn reads_the_leaves_stats() {
        let split = msg(&[
            (1, Ok(30_000)),
            (3, Ok(5_000_000)),
            (7, Ok(2_000_000)),
            (8, Ok(40)),
            (9, Ok(300_000)),
        ]);
        let leaf = msg(&[
            (3, Ok(58)),
            (6, Err(split.clone())),
            (9, Ok(1_500_000)),
            (15, Ok(4)),
            (5, Err(split)),
        ]);
        let root = msg(&[
            (1, Err(leaf.clone())),
            (2, Err(leaf)),
            (3, Ok(1)),
            (8, Ok(1_600_000)),
        ]);
        let resp = msg(&[(1, Ok(1234)), (2, Err(b"hit".to_vec())), (10, Err(root))]);
        let s = parse_response(&resp).unwrap();
        assert_eq!(s.num_hits, 1234);
        assert_eq!((s.localexec_num_splits, s.search_pool_cpu_threads), (58, 4));
        assert_eq!(s.leaf_wall_time_microsecs, 1_500_000);
        assert_eq!(s.root_wall_time_microsecs, 1_600_000);
        assert_eq!(s.splits.warmup_microsecs, 2_000_000);
        assert_eq!(s.splits.cpu_search_microsecs, 300_000);
        assert_eq!(s.splits.download_num_bytes, 5_000_000);
        let mut sum = SearchStats::default();
        sum.add(&s);
        sum.add(&s);
        assert_eq!(sum.splits.warmup_microsecs, 4_000_000);
        assert_eq!(sum.search_pool_cpu_threads, 4);
        // No stats (a pre-0.9 build): the hit count alone.
        assert_eq!(parse_response(&msg(&[(1, Ok(7))])).unwrap().num_hits, 7);
        assert!(parse_response(&[0x0a, 0x05, 1]).is_err());
    }
}
