//! Shared redirect validation and per-origin cooldown scheduling.
use crate::{Result, error};
use reqwest::header::{HeaderMap, RETRY_AFTER};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};
use tokio::time::Instant;
use url::Url;

/// Manual redirects keep request credentials under the caller's control.
/// Never include a Location value in diagnostics: it can contain credentials.
pub(crate) struct Redirects {
    visited: Vec<Url>,
}

impl Redirects {
    pub(crate) fn new(url: &Url) -> Self {
        let mut url = url.clone();
        url.set_fragment(None);
        Self { visited: vec![url] }
    }

    pub(crate) fn next(
        &mut self,
        current: &Url,
        status: reqwest::StatusCode,
        headers: &HeaderMap,
    ) -> Result<Option<Url>> {
        if !status.is_redirection() {
            return Ok(None);
        }
        if !matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308) {
            return Err(error("unsupported HTTP redirect status"));
        }
        if self.visited.len() > 5 {
            return Err(error("HTTP redirect limit exceeded (5 hops)"));
        }
        let locations = headers.get_all(reqwest::header::LOCATION);
        let mut locations = locations.iter();
        let location = locations
            .next()
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| error("HTTP redirect has missing or invalid Location"))?;
        if locations.next().is_some() {
            return Err(error("HTTP redirect has multiple Location headers"));
        }
        let mut target = current
            .join(location)
            .map_err(|_| error("HTTP redirect has invalid Location"))?;
        if !matches!(target.scheme(), "http" | "https")
            || !target.username().is_empty()
            || target.password().is_some()
            || target.origin() != self.visited[0].origin()
        {
            return Err(error(
                "HTTP redirect requires the same origin and no URL credentials",
            ));
        }
        target.set_fragment(None);
        if self.visited.contains(&target) {
            return Err(error("HTTP redirect loop detected"));
        }
        self.visited.push(target.clone());
        Ok(Some(target))
    }
}

#[derive(Clone, Default)]
pub(crate) struct RateLimits {
    // None represents a delay too large for the monotonic clock.
    deadlines: Arc<Mutex<HashMap<String, Option<Instant>>>>,
}

impl RateLimits {
    pub(crate) fn defer(&self, url: &Url, delay: Duration) {
        let now = Instant::now();
        let deadline = now.checked_add(delay);
        let mut deadlines = self.deadlines.lock().unwrap();
        deadlines.retain(|_, until| until.is_none_or(|until| until > now));
        deadlines
            .entry(url.origin().ascii_serialization())
            .and_modify(|until| {
                *until = match (*until, deadline) {
                    (Some(old), Some(new)) => Some(old.max(new)),
                    _ => None,
                };
            })
            .or_insert(deadline);
    }

    pub(crate) async fn wait(&self, url: &Url, budget: Duration) -> Result<()> {
        let start = Instant::now();
        loop {
            let until = self
                .deadlines
                .lock()
                .unwrap()
                .get(&url.origin().ascii_serialization())
                .copied();
            let delay = match until {
                None => return Ok(()),
                Some(Some(until)) => until.saturating_duration_since(Instant::now()),
                Some(None) => {
                    return Err(error(
                        "server Retry-After exceeds operation wait limit; retry later",
                    ));
                }
            };
            if delay.is_zero() {
                return Ok(());
            }
            if delay > budget.saturating_sub(start.elapsed()) {
                return Err(error(
                    "server Retry-After exceeds operation wait limit; retry later",
                ));
            }
            // Recheck after sleeping: another response can extend the cooldown.
            tokio::time::sleep(delay).await;
        }
    }
}

pub(crate) fn retry_after(headers: &HeaderMap, now: SystemTime) -> Option<Duration> {
    let value = headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
    if !value.is_empty() && value.bytes().all(|c| c.is_ascii_digit()) {
        // An overflowing delta is still a request to wait, never retry early.
        return Some(Duration::from_secs(value.parse().unwrap_or(u64::MAX)));
    }
    let date = httpdate::parse_http_date(value).ok()?;
    Some(date.duration_since(now).unwrap_or(Duration::ZERO))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirects_resolve_relative_urls_and_bound_loops_and_hops() {
        let origin = Url::parse("https://example.com/start").unwrap();
        for status in [301, 302, 303, 307, 308] {
            let mut redirects = Redirects::new(&origin);
            let mut current = origin.clone();
            for hop in 1..=6 {
                let mut headers = HeaderMap::new();
                headers.insert(
                    reqwest::header::LOCATION,
                    format!("next{hop}#fragment").parse().unwrap(),
                );
                let result = redirects.next(
                    &current,
                    reqwest::StatusCode::from_u16(status).unwrap(),
                    &headers,
                );
                if hop == 6 {
                    assert!(result.unwrap_err().to_string().contains("limit"));
                } else {
                    current = result.unwrap().unwrap();
                    assert_eq!(current.as_str(), format!("https://example.com/next{hop}"));
                }
            }
        }
        for location in ["#fragment", "/start", "https://example.com:443/start"] {
            let mut headers = HeaderMap::new();
            headers.insert(reqwest::header::LOCATION, location.parse().unwrap());
            assert!(
                Redirects::new(&origin)
                    .next(&origin, reqwest::StatusCode::FOUND, &headers)
                    .unwrap_err()
                    .to_string()
                    .contains("loop")
            );
        }
    }

    #[test]
    fn rejects_unsafe_redirects_without_disclosing_locations() {
        let origin = Url::parse("https://example.com/start").unwrap();
        for location in [
            "http://example.com/secret",
            "https://example.com:444/secret",
            "https://sub.example.com/secret",
            "//evil.test/secret",
            "https://user:secret@example.com/",
            "file:///secret",
            "http://[secret",
            "",
            "/start#secret",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(reqwest::header::LOCATION, location.parse().unwrap());
            let diagnostic = Redirects::new(&origin)
                .next(&origin, reqwest::StatusCode::FOUND, &headers)
                .unwrap_err()
                .to_string();
            assert!(!diagnostic.contains("secret"));
        }
        let mut headers = HeaderMap::new();
        assert!(
            Redirects::new(&origin)
                .next(&origin, reqwest::StatusCode::FOUND, &headers)
                .is_err()
        );
        headers.append(reqwest::header::LOCATION, "/one".parse().unwrap());
        headers.append(reqwest::header::LOCATION, "/two".parse().unwrap());
        assert!(
            Redirects::new(&origin)
                .next(&origin, reqwest::StatusCode::FOUND, &headers)
                .is_err()
        );
        assert!(
            Redirects::new(&origin)
                .next(
                    &origin,
                    reqwest::StatusCode::NOT_MODIFIED,
                    &HeaderMap::new()
                )
                .is_err()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn shares_cooldowns_by_origin_and_rechecks_extensions() {
        let limits = RateLimits::default();
        let first = Url::parse("https://example.com/a.jpg").unwrap();
        let same = Url::parse("https://example.com:443/b.jpg").unwrap();
        limits.defer(&first, Duration::from_secs(2));
        for other in [
            "http://example.com/a",
            "https://example.com:444/a",
            "https://cdn.example.com/a",
        ] {
            limits
                .wait(&Url::parse(other).unwrap(), Duration::ZERO)
                .await
                .unwrap();
        }
        let shared = limits.clone();
        let wait = tokio::spawn(async move { shared.wait(&same, Duration::from_secs(10)).await });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        limits.defer(&first, Duration::from_secs(4));
        // A shorter subsequent delay must not shorten the existing deadline.
        limits.defer(&first, Duration::from_secs(1));
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert!(!wait.is_finished());
        tokio::time::advance(Duration::from_secs(3)).await;
        wait.await.unwrap().unwrap();
        limits.wait(&first, Duration::ZERO).await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn rejects_excessive_delays_and_bounds_repeated_extensions() {
        let limits = RateLimits::default();
        let url = Url::parse("https://example.com/a").unwrap();
        limits.defer(&url, Duration::from_secs(2));
        assert!(limits.wait(&url, Duration::from_secs(1)).await.is_err());
        let shared = limits.clone();
        let other = url.clone();
        let wait = tokio::spawn(async move { shared.wait(&other, Duration::from_secs(3)).await });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        limits.defer(&url, Duration::from_secs(3));
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(wait.await.unwrap().is_err());
        limits.defer(&url, Duration::from_secs(u64::MAX));
        assert!(limits.wait(&url, Duration::from_secs(10)).await.is_err());
    }

    #[test]
    fn parses_delta_dates_and_invalid_values() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        for (value, expected) in [
            ("37", Some(Duration::from_secs(37))),
            ("0", Some(Duration::ZERO)),
            (
                "Sun, 09 Sep 2001 01:47:17 GMT",
                Some(Duration::from_secs(37)),
            ),
            ("Sun, 09 Sep 2001 01:46:39 GMT", Some(Duration::ZERO)),
            (
                "999999999999999999999999999999",
                Some(Duration::from_secs(u64::MAX)),
            ),
            ("later", None),
            ("-1", None),
            ("+1", None),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(RETRY_AFTER, value.parse().unwrap());
            assert_eq!(retry_after(&headers, now), expected, "{value}");
        }
    }
}
