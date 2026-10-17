//! Shared, per-origin cooldowns for direct media downloads.
use crate::{Result, error};
use reqwest::header::{HeaderMap, RETRY_AFTER};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};
use tokio::time::Instant;
use url::Url;

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
                        "server Retry-After exceeds download wait limit; retry later",
                    ));
                }
            };
            if delay.is_zero() {
                return Ok(());
            }
            if delay > budget.saturating_sub(start.elapsed()) {
                return Err(error(
                    "server Retry-After exceeds download wait limit; retry later",
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
