//! The work message: one page to OCR.
//!
//! ```json
//! {"v":1,"page":"sn83045462/1890-05-03/ed-1/seq-7","batch":"nbu_gage_ver01",
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
    #[error("invalid language `{0}` (ISO 639-3, three lower-case letters)")]
    Language(String),
    #[error("invalid score {0}")]
    Score(f64),
    #[error("invalid queued_at `{0}`")]
    QueuedAt(String),
}

impl MessageError {
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
        // LoC batch names: lower-case letters, digits and `_`.
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
            batch: "nbu_gage_ver01".into(),
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
        let raw = r#"{"v":1,"page":"sn83045462/1890-05-03/ed-1/seq-7","batch":"nbu_gage_ver01","language":"eng","reason":"user","queued_at":"2026-10-10T08:00:00-04:00"}"#;
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
        assert!(matches!(
            bad(|m| m.language = "de".into()),
            MessageError::Language(_)
        ));
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
}
