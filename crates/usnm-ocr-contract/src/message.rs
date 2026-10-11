//! The work message: one page to OCR.
//!
//! ```json
//! {"v":1,"page":"sn83045462/1890-05-03/ed-1/seq-7","batch":"nbu_gage",
//!  "language":"deu","reason":"bulk","score":0.62,"queued_at":"2026-10-10T12:00:00Z"}
//! ```
//!
//! A message names the page and why it was queued. It never carries search
//! text: unknown fields are refused, so a query can't ride along. On the
//! queue the JSON is base64-encoded (UTF-8 bytes, standard alphabet), the
//! encoding the Azure portal, Storage Explorer and Functions expect;
//! [`decode`] also takes raw JSON, which the v12 SDKs send by default.

use base64::Engine as _;
use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};
use usnm_core::ids::PageKey;

/// The largest message accepted (Azure's limit is 64 KiB; ours are ~250 bytes).
pub const MAX_MESSAGE_BYTES: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Reason {
    /// A page someone was shown in search results (`ocr-priority`).
    User,
    /// The damage-ranked backlog (`ocr-bulk`).
    Bulk,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::User => "user",
            Reason::Bulk => "bulk",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OcrMessage {
    pub v: u32,
    /// `lccn/YYYY-MM-DD/ed-N/seq-N`.
    pub page: String,
    pub batch: String,
    /// ISO 639-3.
    pub language: String,
    pub reason: Reason,
    /// The page's damage score in its language, when the seeder had one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    /// RFC 3339.
    pub queued_at: String,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum MessageError {
    #[error("message is {0} bytes, over {MAX_MESSAGE_BYTES}")]
    TooLarge(usize),
    #[error("message isn't JSON or base64 JSON: {0}")]
    Json(String),
    #[error("unsupported message version {0}")]
    Version(u32),
    #[error("invalid page `{0}`")]
    Page(String),
    #[error("invalid batch `{0}`")]
    Batch(String),
    #[error("invalid language `{0}` (an ISO 639-3 code in usnm_core::lang)")]
    Language(String),
    #[error("invalid score {0}")]
    Score(f64),
    #[error("invalid queued_at `{0}`")]
    QueuedAt(String),
}

impl MessageError {
    /// Every value [`MessageError::kind`] returns.
    pub const KINDS: [&'static str; 8] = [
        "too_large",
        "bad_json",
        "version",
        "page",
        "batch",
        "language",
        "score",
        "queued_at",
    ];

    /// What was wrong, without any of the message's content: safe to keep
    /// in the dead-letter queue and in logs, since a bad message might carry
    /// anything.
    pub fn kind(&self) -> &'static str {
        match self {
            MessageError::TooLarge(_) => "too_large",
            MessageError::Json(_) => "bad_json",
            MessageError::Version(_) => "version",
            MessageError::Page(_) => "page",
            MessageError::Batch(_) => "batch",
            MessageError::Language(_) => "language",
            MessageError::Score(_) => "score",
            MessageError::QueuedAt(_) => "queued_at",
        }
    }
}

impl OcrMessage {
    /// The page, checked: it must be the canonical form of a valid key.
    pub fn key(&self) -> Result<PageKey, MessageError> {
        let key: PageKey = self
            .page
            .parse()
            .map_err(|_| MessageError::Page(self.page.clone()))?;
        if key.to_string() != self.page {
            return Err(MessageError::Page(self.page.clone()));
        }
        Ok(key)
    }

    pub fn queued_at(&self) -> Result<DateTime<FixedOffset>, MessageError> {
        DateTime::parse_from_rfc3339(&self.queued_at)
            .map_err(|_| MessageError::QueuedAt(self.queued_at.clone()))
    }

    pub fn validate(&self) -> Result<PageKey, MessageError> {
        if self.v != 1 {
            return Err(MessageError::Version(self.v));
        }
        let key = self.key()?;
        // The shape of LoC's base batch names: the awardee's code and the
        // batch's name (`nbu_gage`, `curiv_joshuaTree`), with no `_verNN`.
        // ocr-sync checks that it is in the published version.
        if !is_base_batch_name(&self.batch) {
            return Err(MessageError::Batch(self.batch.clone()));
        }
        // Only languages the code map knows (`deu`, not the catalog's `ger`).
        if usnm_core::lang::by_iso3(&self.language).is_none() {
            return Err(MessageError::Language(self.language.clone()));
        }
        if let Some(s) = self.score {
            if !s.is_finite() || !(0.0..=1.0).contains(&s) {
                return Err(MessageError::Score(s));
            }
        }
        self.queued_at()?;
        Ok(key)
    }

    /// The rules messages were checked by before 12 Step 7, for messages
    /// kept in v1 dead letters: a batch of 1 to 128 `[a-z0-9_]` and any
    /// three lower-case letters as the language.
    pub fn validate_v1(&self) -> Result<PageKey, MessageError> {
        if self.v != 1 {
            return Err(MessageError::Version(self.v));
        }
        let key = self.key()?;
        let b = &self.batch;
        if b.is_empty()
            || b.len() > 128
            || !b
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
        {
            return Err(MessageError::Batch(b.clone()));
        }
        let l = &self.language;
        if l.len() != 3 || !l.bytes().all(|c| c.is_ascii_lowercase()) {
            return Err(MessageError::Language(l.clone()));
        }
        if let Some(s) = self.score {
            if !s.is_finite() || !(0.0..=1.0).contains(&s) {
                return Err(MessageError::Score(s));
            }
        }
        self.queued_at()?;
        Ok(key)
    }

    /// The JSON, base64-encoded, as it goes on the queue.
    pub fn encode(&self) -> String {
        let json = serde_json::to_string(self).expect("serializable");
        base64::engine::general_purpose::STANDARD.encode(json)
    }
}

/// Whether `batch` has the shape of a base batch name: the awardee's code
/// (2 to 10 lower-case letters), `_`, and the batch's name (1 to 40 letters
/// or digits), with no curation version. LoC's 3,010 batches of October
/// 2026 all have it.
pub fn is_base_batch_name(batch: &str) -> bool {
    let Some((awardee, name)) = batch.split_once('_') else {
        return false;
    };
    (2..=10).contains(&awardee.len())
        && awardee.bytes().all(|c| c.is_ascii_lowercase())
        && (1..=40).contains(&name.len())
        && name.bytes().all(|c| c.is_ascii_alphanumeric())
        && !is_versioned_batch(batch)
}

/// Whether `batch` ends in a curation version, `_ver` and digits
/// (`nbu_gage_ver01`), as LoC's batch directories do.
pub fn is_versioned_batch(batch: &str) -> bool {
    batch.rsplit_once("_ver").is_some_and(|(base, n)| {
        !base.is_empty() && !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit())
    })
}

/// Why a message went to `ocr-deadletter` (dead letter v2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cause {
    /// Received more than 5 times; why the earlier attempts failed is only
    /// in telemetry (`ocr.pages_failed` by `error`, `ocr_page` requests by
    /// message id).
    DequeueCount,
    /// Failed its checks; `kind` says which.
    Invalid,
    /// Can never succeed (loc.gov lists no image, a 4xx).
    Permanent,
    /// Moved off a work queue after its defer budget while the engines
    /// that route its pages were down; `engines` names them (13 §13.7.6).
    /// The engine contract (#307) sends these.
    Unroutable,
}

impl Cause {
    pub fn as_str(self) -> &'static str {
        match self {
            Cause::DequeueCount => "dequeue_count",
            Cause::Invalid => "invalid",
            Cause::Permanent => "permanent",
            Cause::Unroutable => "unroutable",
        }
    }
}

/// The dead letter's current version.
pub const DEAD_LETTER_VERSION: u32 = 2;
/// The longest `error` kept, in characters.
pub const MAX_ERROR_CHARS: usize = 200;
/// The largest dead letter read (Azure's limit is 64 KiB).
pub const MAX_DEAD_LETTER_BYTES: usize = 8192;

/// A message on `ocr-deadletter`. Any node can put anything there, so the
/// cloud job that drains it (`ocr-sync`, 12 Step 8) reads it with
/// [`decode_dead_letter`]: v1 (no cause) or v2, unknown fields refused,
/// every field checked.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeadLetter {
    pub v: u32,
    /// `ocr-priority` or `ocr-bulk`.
    pub from: String,
    pub message_id: String,
    /// The message, re-encoded from its checked fields; null when it failed
    /// its checks, since it might carry anything.
    pub message: Option<OcrMessage>,
    /// v2: why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<Cause>,
    /// v2, `invalid` only: [`MessageError::kind`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// v2, `unroutable` only: the engines that were down.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engines: Option<Vec<String>>,
    /// Free text, at most [`MAX_ERROR_CHARS`] with no control characters.
    pub error: String,
    pub dequeue_count: u32,
    pub node: String,
    /// RFC 3339.
    pub at: String,
}

#[derive(Debug, thiserror::Error, PartialEq)]
#[error("invalid dead letter: {0}")]
pub struct DeadLetterError(pub &'static str);

/// `error` as a dead letter keeps it: control characters as spaces, cut to
/// [`MAX_ERROR_CHARS`].
pub fn clean_error(error: &str) -> String {
    error
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_ERROR_CHARS)
        .collect()
}

fn is_name(s: &str, max: usize, extra: &[u8]) -> bool {
    !s.is_empty()
        && s.len() <= max
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || extra.contains(&b))
}

/// An engine id as 13 §13.8.1 defines it: `[a-z0-9][a-z0-9_.+-]{0,63}`.
pub fn is_engine_id(s: &str) -> bool {
    let b = s.as_bytes();
    (1..=64).contains(&b.len())
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter()
            .all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"_.+-".contains(&c))
}

impl DeadLetter {
    /// A v2 dead letter for `cause`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        from: &str,
        message_id: &str,
        message: Option<&OcrMessage>,
        cause: Cause,
        kind: Option<&str>,
        engines: Option<Vec<String>>,
        error: &str,
        dequeue_count: u32,
        node: &str,
    ) -> Self {
        Self {
            v: DEAD_LETTER_VERSION,
            from: from.to_owned(),
            message_id: message_id.to_owned(),
            message: message.cloned(),
            cause: Some(cause),
            kind: kind.map(str::to_owned),
            engines,
            error: clean_error(error),
            dequeue_count,
            node: node.to_owned(),
            at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        }
    }

    /// Every field in its format, and the cause's fields with their cause.
    pub fn validate(&self) -> Result<(), DeadLetterError> {
        let e = DeadLetterError;
        if !matches!(self.from.as_str(), "ocr-priority" | "ocr-bulk") {
            return Err(e("from"));
        }
        if !is_name(&self.message_id, 64, b"-_") {
            return Err(e("message_id"));
        }
        if let Some(m) = &self.message {
            // A v1 dead letter holds a message checked by v1's looser rules
            // (any batch of `[a-z0-9_]`, any three lower-case letters).
            let ok = if self.v == 1 {
                m.validate_v1().is_ok()
            } else {
                m.validate().is_ok()
            };
            if !ok {
                return Err(e("message"));
            }
        }
        let error_ok = self.error.chars().count() <= MAX_ERROR_CHARS
            && !self.error.chars().any(char::is_control);
        if !error_ok {
            return Err(e("error"));
        }
        if !is_name(&self.node, 32, b"-") {
            return Err(e("node"));
        }
        DateTime::parse_from_rfc3339(&self.at).map_err(|_| e("at"))?;
        match self.v {
            1 if self.cause.is_none() && self.kind.is_none() && self.engines.is_none() => Ok(()),
            1 => Err(e("v1 has no cause")),
            2 => {
                let cause = self.cause.ok_or(e("cause"))?;
                let kind_ok = match (cause, self.kind.as_deref()) {
                    (Cause::Invalid, Some(k)) => MessageError::KINDS.contains(&k),
                    (Cause::Invalid, None) => false,
                    (_, k) => k.is_none(),
                };
                if !kind_ok {
                    return Err(e("kind"));
                }
                if cause == Cause::Invalid && self.message.is_some() {
                    return Err(e("an invalid message is never kept"));
                }
                // A permanent or unroutable dead letter is about a page that
                // passed its checks, so it keeps the message. A dequeue-count
                // one may not: an invalid payload received too often has no
                // safe message to keep.
                if matches!(cause, Cause::Permanent | Cause::Unroutable) && self.message.is_none() {
                    return Err(e("message"));
                }
                let engines_ok = match (cause, &self.engines) {
                    (Cause::Unroutable, Some(ids)) => {
                        (1..=16).contains(&ids.len()) && ids.iter().all(|i| is_engine_id(i))
                    }
                    (Cause::Unroutable, None) => false,
                    (_, ids) => ids.is_none(),
                };
                if !engines_ok {
                    return Err(e("engines"));
                }
                Ok(())
            }
            _ => Err(e("v")),
        }
    }

    /// The JSON, base64-encoded, as it goes on the queue.
    pub fn encode(&self) -> String {
        let json = serde_json::to_string(self).expect("serializable");
        base64::engine::general_purpose::STANDARD.encode(json)
    }
}

/// A dead letter's queue text, checked.
pub fn decode_dead_letter(text: &str) -> Result<DeadLetter, DeadLetterError> {
    if text.len() > MAX_DEAD_LETTER_BYTES {
        return Err(DeadLetterError("too_large"));
    }
    let trimmed = text.trim();
    let json = if trimmed.starts_with('{') {
        trimmed.as_bytes().to_vec()
    } else {
        base64::engine::general_purpose::STANDARD
            .decode(trimmed)
            .map_err(|_| DeadLetterError("bad_json"))?
    };
    let d: DeadLetter = serde_json::from_slice(&json).map_err(|_| DeadLetterError("bad_json"))?;
    d.validate()?;
    Ok(d)
}

/// A queue message's text as a checked message and its page.
pub fn decode(text: &str) -> Result<(OcrMessage, PageKey), MessageError> {
    if text.len() > MAX_MESSAGE_BYTES {
        return Err(MessageError::TooLarge(text.len()));
    }
    let trimmed = text.trim();
    let json = if trimmed.starts_with('{') {
        trimmed.as_bytes().to_vec()
    } else {
        base64::engine::general_purpose::STANDARD
            .decode(trimmed)
            .map_err(|e| MessageError::Json(e.to_string()))?
    };
    let msg: OcrMessage =
        serde_json::from_slice(&json).map_err(|e| MessageError::Json(e.to_string()))?;
    let key = msg.validate()?;
    Ok((msg, key))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> OcrMessage {
        OcrMessage {
            v: 1,
            page: "sn83045462/1890-05-03/ed-1/seq-7".into(),
            batch: "nbu_gage".into(),
            language: "deu".into(),
            reason: Reason::Bulk,
            score: Some(0.62),
            queued_at: "2026-10-10T12:00:00Z".into(),
        }
    }

    #[test]
    fn round_trips_base64_and_takes_raw_json() {
        let m = sample();
        let (back, key) = decode(&m.encode()).unwrap();
        assert_eq!(back, m);
        assert_eq!(key.doc_id(), "sn83045462_1890-05-03_ed-1_seq-7");
        let raw = r#"{"v":1,"page":"sn83045462/1890-05-03/ed-1/seq-7","batch":"nbu_gage","language":"eng","reason":"user","queued_at":"2026-10-10T08:00:00-04:00"}"#;
        let (m, _) = decode(raw).unwrap();
        assert_eq!(m.reason, Reason::User);
        assert_eq!(m.score, None);
    }

    #[test]
    fn refuses_search_text_and_bad_fields() {
        let with_query = r#"{"v":1,"page":"sn83045462/1890-05-03/ed-1/seq-7","batch":"b","language":"eng","reason":"user","queued_at":"2026-10-10T08:00:00Z","q":"cross of gold"}"#;
        assert!(matches!(decode(with_query), Err(MessageError::Json(_))));
        let bad = |f: fn(&mut OcrMessage)| {
            let mut m = sample();
            f(&mut m);
            m.validate().unwrap_err()
        };
        assert_eq!(bad(|m| m.v = 2), MessageError::Version(2));
        assert!(matches!(
            bad(|m| m.page = "sn83045462/1890-05-03/ed-01/seq-7".into()),
            MessageError::Page(_)
        ));
        assert!(matches!(
            bad(|m| m.page = "SN1/1890-05-03/ed-1/seq-7".into()),
            MessageError::Page(_)
        ));
        assert!(matches!(
            bad(|m| m.batch = "../x".into()),
            MessageError::Batch(_)
        ));
        // The base name only: `_verNN` would be skipped at release.
        assert!(matches!(
            bad(|m| m.batch = "nbu_gage_ver01".into()),
            MessageError::Batch(_)
        ));
        assert!(is_versioned_batch("nn_kant_ver1"));
        assert!(!is_versioned_batch("nn_kant"));
        assert!(!is_versioned_batch("_ver01"));
        assert!(!is_versioned_batch("nn_kant_ver"));
        assert!(!is_versioned_batch("nn_kant_version"));
        assert!(matches!(
            bad(|m| m.language = "de".into()),
            MessageError::Language(_)
        ));
        // The catalog's MARC code, or a code no list knows, is refused at
        // enqueue rather than dead-lettered on every node.
        assert!(matches!(
            bad(|m| m.language = "ger".into()),
            MessageError::Language(_)
        ));
        assert!(matches!(
            bad(|m| m.language = "zzz".into()),
            MessageError::Language(_)
        ));
        // Words joined by underscores aren't a batch name.
        assert!(matches!(
            bad(|m| m.batch = "cross_of_gold".into()),
            MessageError::Batch(_)
        ));
        assert!(matches!(
            bad(|m| m.batch = "batch_nbu_gage".into()),
            MessageError::Batch(_)
        ));
        assert!(is_base_batch_name("curiv_joshuaTree"));
        assert!(is_base_batch_name("dlc_1arp"));
        assert!(!is_base_batch_name("x_gage"));
        assert!(!is_base_batch_name("nbu_"));
        assert!(matches!(
            bad(|m| m.score = Some(f64::NAN)),
            MessageError::Score(_)
        ));
        assert!(matches!(
            bad(|m| m.score = Some(1.5)),
            MessageError::Score(_)
        ));
        assert!(matches!(
            bad(|m| m.queued_at = "yesterday".into()),
            MessageError::QueuedAt(_)
        ));
        let other_reason = with_query
            .replace(r#""reason":"user""#, r#""reason":"search""#)
            .replace(r#","q":"cross of gold""#, "");
        assert!(matches!(decode(&other_reason), Err(MessageError::Json(_))));
        assert!(matches!(
            decode(&"x".repeat(MAX_MESSAGE_BYTES + 1)),
            Err(MessageError::TooLarge(_))
        ));
        assert!(matches!(decode("not base64!"), Err(MessageError::Json(_))));
    }

    fn dead(cause: Cause) -> DeadLetter {
        let m = sample();
        let (kind, engines, message) = match cause {
            Cause::Invalid => (Some("bad_json"), None, None),
            Cause::Unroutable => (None, Some(vec!["tesseract-5.5".to_owned()]), Some(&m)),
            _ => (None, None, Some(&m)),
        };
        DeadLetter::new(
            "ocr-bulk", "m1", message, cause, kind, engines, "x\ny", 6, "node1",
        )
    }

    #[test]
    fn each_dead_letter_cause_has_its_fields() {
        for cause in [
            Cause::DequeueCount,
            Cause::Invalid,
            Cause::Permanent,
            Cause::Unroutable,
        ] {
            let d = dead(cause);
            let back = decode_dead_letter(&d.encode()).unwrap();
            assert_eq!(back, d);
            assert_eq!(back.v, 2);
            assert_eq!(back.cause, Some(cause));
            assert_eq!(back.kind.is_some(), cause == Cause::Invalid);
            assert_eq!(back.engines.is_some(), cause == Cause::Unroutable);
            assert_eq!(back.error, "x y", "control characters are dropped");
            let json: serde_json::Value =
                serde_json::from_str(&serde_json::to_string(&d).unwrap()).unwrap();
            assert_eq!(json["cause"], cause.as_str());
        }
        // A kind on the wrong cause, a missing one, an unknown one.
        let mut d = dead(Cause::Permanent);
        d.kind = Some("bad_json".into());
        assert!(d.validate().is_err());
        let mut d = dead(Cause::Invalid);
        d.kind = None;
        assert!(d.validate().is_err());
        let mut d = dead(Cause::Invalid);
        d.kind = Some("search text".into());
        assert!(d.validate().is_err());
        // An invalid message is never kept.
        let mut d = dead(Cause::Invalid);
        d.message = Some(sample());
        assert!(d.validate().is_err());
        let mut d = dead(Cause::Unroutable);
        d.engines = Some(vec![]);
        assert!(d.validate().is_err());
        // Engine ids by 13 §13.8.1: lower case, `+` allowed.
        let mut d = dead(Cause::Unroutable);
        d.engines = Some(vec!["tesseract-5.5+deu_latf".into()]);
        assert!(d.validate().is_ok());
        for bad in ["Tesseract-5.5", "-x", "a b", "a/b", &"x".repeat(65)] {
            let mut d = dead(Cause::Unroutable);
            d.engines = Some(vec![bad.to_owned()]);
            assert!(d.validate().is_err(), "{bad}");
        }
        // Permanent and unroutable dead letters are about a checked page:
        // they keep its message. A dequeue-count one may have none.
        for cause in [Cause::Permanent, Cause::Unroutable] {
            let mut d = dead(cause);
            d.message = None;
            assert_eq!(d.validate(), Err(DeadLetterError("message")), "{cause:?}");
        }
        let mut d = dead(Cause::DequeueCount);
        d.message = None;
        assert!(d.validate().is_ok());
        let mut d = dead(Cause::Permanent);
        d.error = "e".repeat(MAX_ERROR_CHARS + 1);
        assert!(d.validate().is_err());
        let mut d = dead(Cause::Permanent);
        d.from = "ocr-deadletter".into();
        assert!(d.validate().is_err());
        assert_eq!(
            clean_error(&"é".repeat(300)).chars().count(),
            MAX_ERROR_CHARS
        );
    }

    #[test]
    fn reads_v1_dead_letters_and_refuses_extra_fields() {
        let v1 = r#"{"v":1,"from":"ocr-bulk","message_id":"m1","message":null,"error":"received 6 times","dequeue_count":6,"node":"node1","at":"2026-10-10T12:00:00+00:00"}"#;
        let d = decode_dead_letter(v1).unwrap();
        assert_eq!((d.v, d.cause), (1, None));
        // A v1 dead letter's message was checked by v1's rules.
        let legacy = v1.replace(
            r#""message":null"#,
            r#""message":{"v":1,"page":"sn83045462/1890-05-03/ed-1/seq-7","batch":"b","language":"zzz","reason":"bulk","queued_at":"2026-10-10T12:00:00Z"}"#,
        );
        assert_eq!(
            decode_dead_letter(&legacy).unwrap().message.unwrap().batch,
            "b"
        );
        let v2_without_cause = v1.replace(r#""v":1"#, r#""v":2"#);
        assert!(decode_dead_letter(&v2_without_cause).is_err());
        let extra = v1.replace(r#""v":1,"#, r#""v":1,"q":"cross of gold","#);
        assert_eq!(
            decode_dead_letter(&extra).unwrap_err(),
            DeadLetterError("bad_json")
        );
        assert!(decode_dead_letter(&"x".repeat(MAX_DEAD_LETTER_BYTES + 1)).is_err());
    }
}
