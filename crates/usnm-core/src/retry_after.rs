//! The HTTP `Retry-After` header (RFC 9110 §10.2.3): delta-seconds or an
//! HTTP date, shared by the LoC client and the Congressional Globe fetch.

use std::time::{Duration, SystemTime};

/// How long a `Retry-After` value asks to wait, measured from `now`: whole
/// seconds (`120`) or an HTTP date (`Wed, 21 Oct 2015 07:28:00 GMT`). A
/// date already past is zero. `None` when the value is neither form. The
/// caller applies its own cap.
pub fn parse(value: &str, now: SystemTime) -> Option<Duration> {
    let v = value.trim();
    if let Ok(s) = v.parse::<u64>() {
        return Some(Duration::from_secs(s));
    }
    let at = httpdate::parse_http_date(v).ok()?;
    Some(at.duration_since(now).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> SystemTime {
        // Wed, 21 Oct 2015 07:28:00 GMT
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_445_412_480)
    }

    #[test]
    fn seconds() {
        assert_eq!(parse("120", now()), Some(Duration::from_secs(120)));
        assert_eq!(parse(" 0 ", now()), Some(Duration::ZERO));
    }

    #[test]
    fn a_date_is_the_time_until_it() {
        assert_eq!(
            parse("Wed, 21 Oct 2015 07:30:30 GMT", now()),
            Some(Duration::from_secs(150))
        );
    }

    #[test]
    fn a_past_date_is_zero() {
        assert_eq!(
            parse("Wed, 21 Oct 2015 07:00:00 GMT", now()),
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn anything_else_is_ignored() {
        for v in ["", "soon", "-5", "1.5", "Wed, 99 Oct 2015 07:00:00 GMT"] {
            assert_eq!(parse(v, now()), None, "{v:?}");
        }
    }
}
