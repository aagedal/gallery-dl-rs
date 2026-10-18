//! HTTP representation validation for persistent range downloads (RFC 9110,
//! sections 8.8, 13.1.5, 14.4). Never combine bytes from different resources.
use crate::{
    Result, error,
    resume::{Checkpoint, Validator},
};
use reqwest::{
    Response,
    header::{self, HeaderMap, HeaderName},
};
use sha2::{Digest, Sha256};
use std::time::Duration;

pub(crate) enum Plan {
    Full(Option<Checkpoint>),
    Append { end: u64, total: u64 },
    Complete,
    Restart,
}

pub(crate) fn if_range(checkpoint: &Checkpoint) -> Option<&str> {
    match &checkpoint.validator {
        Validator::Etag(value) if strong_etag(value) => Some(value),
        Validator::Modified(value) if httpdate::parse_http_date(value).is_ok() => Some(value),
        _ => None,
    }
}

pub(crate) fn plan(
    response: &Response,
    offset: u64,
    checkpoint: Option<&Checkpoint>,
) -> Result<Plan> {
    let headers = response.headers();
    let status = response.status().as_u16();
    let target = target_hash(response);
    if status == 200 {
        if headers.contains_key(header::CONTENT_RANGE) {
            return Err(error("unexpected Content-Range in full media response"));
        }
        identity_encoding(headers)?;
        let checkpoint = validator(headers).map(|validator| Checkpoint {
            validator,
            target,
            total: response.content_length(),
        });
        return Ok(Plan::Full(checkpoint));
    }
    let checkpoint = checkpoint.ok_or_else(|| error("unsolicited partial media response"))?;
    if checkpoint.target != target || !matches_validator(&checkpoint.validator, headers) {
        return Ok(Plan::Restart);
    }
    let range = single(headers, header::CONTENT_RANGE)
        .ok_or_else(|| error("range response requires one valid Content-Range"))?;
    if status == 416 {
        let total = range.strip_prefix("bytes */").and_then(decimal);
        return Ok(
            if total == Some(offset) && checkpoint.total.is_none_or(|n| Some(n) == total) {
                Plan::Complete
            } else {
                Plan::Restart
            },
        );
    }
    if status != 206 {
        return Err(error("unexpected HTTP status for media range"));
    }
    identity_encoding(headers)?;
    let (start, end, total) =
        parse_range(range).ok_or_else(|| error("invalid media Content-Range"))?;
    if start != offset || checkpoint.total.is_some_and(|n| n != total) {
        return Err(error(
            "media Content-Range does not match saved partial file",
        ));
    }
    if response
        .content_length()
        .is_some_and(|n| n != end - start + 1)
    {
        return Err(error("media range length does not match Content-Range"));
    }
    Ok(Plan::Append { end, total })
}

fn target_hash(response: &Response) -> String {
    let mut url = response.url().clone();
    url.set_fragment(None);
    format!("{:x}", Sha256::digest(url.as_str().as_bytes()))
}

fn identity_encoding(headers: &HeaderMap) -> Result<()> {
    if headers.contains_key(header::CONTENT_ENCODING)
        && single(headers, header::CONTENT_ENCODING)
            .is_none_or(|value| !value.eq_ignore_ascii_case("identity"))
    {
        return Err(error("resumable media requires identity Content-Encoding"));
    }
    Ok(())
}

fn single(headers: &HeaderMap, name: HeaderName) -> Option<&str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?.to_str().ok()?;
    if values.next().is_some() {
        None
    } else {
        Some(value)
    }
}

fn validator(headers: &HeaderMap) -> Option<Validator> {
    // A weak ETag cannot be used with If-Range, including via a date fallback.
    if headers.contains_key(header::ETAG) {
        return single(headers, header::ETAG)
            .filter(|v| strong_etag(v))
            .map(|v| Validator::Etag(v.into()));
    }
    let modified = single(headers, header::LAST_MODIFIED)?;
    let timestamp = httpdate::parse_http_date(modified).ok()?;
    let date = httpdate::parse_http_date(single(headers, header::DATE)?).ok()?;
    // A conservative clock-skew allowance for treating modification time as
    // strong. RFC 9110 permits a smaller gap when clocks are known to agree.
    if date.duration_since(timestamp).ok()? < Duration::from_secs(60) {
        return None;
    }
    Some(Validator::Modified(modified.into()))
}

fn matches_validator(validator: &Validator, headers: &HeaderMap) -> bool {
    match validator {
        Validator::Etag(value) => {
            strong_etag(value) && single(headers, header::ETAG) == Some(value.as_str())
        }
        Validator::Modified(value) => {
            let expected = httpdate::parse_http_date(value).ok();
            expected.is_some()
                && single(headers, header::LAST_MODIFIED)
                    .and_then(|v| httpdate::parse_http_date(v).ok())
                    == expected
                && !headers.contains_key(header::ETAG)
        }
    }
}

fn strong_etag(value: &str) -> bool {
    value.len() >= 2
        && value.len() <= 4096
        && value.starts_with('"')
        && value.ends_with('"')
        && value.as_bytes()[1..value.len() - 1]
            .iter()
            .all(|b| *b == 0x21 || (0x23..=0x7e).contains(b))
}

fn decimal(value: &str) -> Option<u64> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

fn parse_range(value: &str) -> Option<(u64, u64, u64)> {
    let (span, total) = value.strip_prefix("bytes ")?.split_once('/')?;
    let (start, end) = span.split_once('-')?;
    let (start, end, total) = (decimal(start)?, decimal(end)?, decimal(total)?);
    if start <= end && end < total {
        Some((start, end, total))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_strong_etags_and_range_arithmetic() {
        for value in ["", "W/\"abc\"", "abc", "\"a\"b\"", "\"a\nb\""] {
            assert!(!strong_etag(value));
        }
        assert!(strong_etag("\"\""));
        assert!(strong_etag("\"version-1\""));
        assert_eq!(parse_range("bytes 3-5/6"), Some((3, 5, 6)));
        for value in [
            "bytes 3-2/6",
            "bytes 3-6/6",
            "bytes 3-5/*",
            "items 3-5/6",
            "bytes +3-5/6",
            "bytes 0-18446744073709551615/18446744073709551616",
        ] {
            assert_eq!(parse_range(value), None);
        }
    }
}
