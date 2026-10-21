//! Strict UTC date bounds and lazy collection selection.
use crate::{Result, error, extract::Media};
use futures_util::{StreamExt, stream::BoxStream};
use time::{Date, Month, PrimitiveDateTime, Time, UtcOffset};

/// Parse Unix seconds or an ISO calendar date/time. Naive values use UTC.
/// Fractions are rounded down to whole seconds, including before the epoch.
pub fn parse_date(input: &str) -> Result<i64> {
    let invalid = || error("invalid date; expected Unix seconds or an ISO calendar date/time");
    if input.len() > 128 || !input.is_ascii() {
        return Err(invalid());
    }
    let input = input.trim();
    if let Ok(timestamp) = input.parse::<i64>() {
        return Ok(timestamp);
    }
    let digits = |value: &str| -> Result<u8> {
        if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(invalid());
        }
        value.parse().map_err(|_| invalid())
    };
    if input.len() < 10 || &input[4..5] != "-" || &input[7..8] != "-" {
        return Err(invalid());
    }
    let year = &input[..4];
    if !year.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid());
    }
    let date = Date::from_calendar_date(
        year.parse().map_err(|_| invalid())?,
        Month::try_from(digits(&input[5..7])?).map_err(|_| invalid())?,
        digits(&input[8..10])?,
    )
    .map_err(|_| invalid())?;
    if input.len() == 10 {
        return Ok(date.midnight().assume_utc().unix_timestamp());
    }
    if input.len() < 16 || !matches!(input.as_bytes()[10], b'T' | b' ') || &input[13..14] != ":" {
        return Err(invalid());
    }
    let hour = digits(&input[11..13])?;
    let minute = digits(&input[14..16])?;
    let mut rest = &input[16..];
    let mut second = 0;
    if rest.starts_with(':') {
        if rest.len() < 3 {
            return Err(invalid());
        }
        second = digits(&rest[1..3])?;
        rest = &rest[3..];
        if let Some(fraction) = rest.strip_prefix('.') {
            let end = fraction
                .bytes()
                .position(|byte| !byte.is_ascii_digit())
                .unwrap_or(fraction.len());
            if end == 0 {
                return Err(invalid());
            }
            // The whole local second minus an integral UTC offset is also the
            // floor of the fractional instant; no floating point is involved.
            rest = &fraction[end..];
        }
    }
    let offset = match rest {
        "" | "Z" => UtcOffset::UTC,
        offset if offset.starts_with(['+', '-']) => {
            let sign = if offset.starts_with('-') { -1 } else { 1 };
            let (hours, minutes) = match offset.len() {
                5 => (&offset[1..3], &offset[3..5]),
                6 if &offset[3..4] == ":" => (&offset[1..3], &offset[4..6]),
                _ => return Err(invalid()),
            };
            let hours = digits(hours)?;
            let minutes = digits(minutes)?;
            if hours > 23 || minutes > 59 {
                return Err(invalid());
            }
            UtcOffset::from_hms(sign * hours as i8, sign * minutes as i8, 0)
                .map_err(|_| invalid())?
        }
        _ => return Err(invalid()),
    };
    let clock = Time::from_hms(hour, minute, second).map_err(|_| invalid())?;
    Ok(PrimitiveDateTime::new(date, clock)
        .assume_offset(offset)
        .unix_timestamp())
}

/// Strict date bounds with explicit handling for collection order.
#[derive(Clone, Copy, Debug, Default)]
pub struct DateFilter {
    before: Option<i64>,
    after: Option<i64>,
}

impl DateFilter {
    pub fn new(before: Option<i64>, after: Option<i64>) -> Result<Self> {
        if before
            .zip(after)
            .is_some_and(|(before, after)| after >= before)
        {
            return Err(error("date-after must be earlier than date-before"));
        }
        Ok(Self { before, after })
    }

    /// Filter a newest-first collection, preserving unknown timestamps and
    /// extraction errors. Drop upstream as soon as the lower bound is reached,
    /// without reading another item or page.
    pub fn apply<'a>(&self, input: BoxStream<'a, Result<Media>>) -> BoxStream<'a, Result<Media>> {
        self.apply_ordered(input, true)
    }

    /// Preserve source order, unknown timestamps, and extraction errors while
    /// skipping out-of-bound items. Older items do not end an unordered feed.
    pub fn apply_unordered<'a>(
        &self,
        input: BoxStream<'a, Result<Media>>,
    ) -> BoxStream<'a, Result<Media>> {
        self.apply_ordered(input, false)
    }

    fn apply_ordered<'a>(
        &self,
        input: BoxStream<'a, Result<Media>>,
        newest_first: bool,
    ) -> BoxStream<'a, Result<Media>> {
        let filter = *self;
        futures_util::stream::unfold(Some(input), move |mut input| async move {
            let mut skipped = 0u8;
            loop {
                let item = input.as_mut()?.next().await?;
                let timestamp = match &item {
                    Ok(media) => media.metadata.as_ref().and_then(|post| post.timestamp),
                    Err(_) => None,
                };
                if let Some(timestamp) = timestamp {
                    let too_old = filter.after.is_some_and(|after| timestamp <= after);
                    if newest_first && too_old {
                        drop(input);
                        return None;
                    }
                    if too_old || filter.before.is_some_and(|before| timestamp >= before) {
                        skipped += 1;
                        if skipped == 64 {
                            tokio::task::yield_now().await;
                            skipped = 0;
                        }
                        continue;
                    }
                }
                return Some((item, input));
            }
        })
        .fuse()
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::{MediaJob, PostMetadata};
    use std::{
        collections::BTreeMap,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
    };

    #[test]
    fn calendar_timezones_and_fraction_floor() {
        for value in [
            "1970-01-01",
            "1970-01-01T00:00",
            "1970-01-01 00:00:00Z",
            "1970-01-01T01:30:00+01:30",
            "1969-12-31T22:30:00-0130",
        ] {
            assert_eq!(parse_date(value).unwrap(), 0, "{value}");
        }
        assert_eq!(parse_date("1969-12-31T23:59:59.999999999999Z").unwrap(), -1);
        assert_eq!(parse_date("1970-01-01T00:00:00.001Z").unwrap(), 0);
        assert_eq!(parse_date("2000-02-29").unwrap(), 951782400);
        assert_eq!(parse_date("2024-02-29T12:34:56+0230").unwrap(), 1709201096);
        for timestamp in [i64::MIN, -1, 0, i64::MAX] {
            assert_eq!(parse_date(&timestamp.to_string()).unwrap(), timestamp);
        }
        assert_eq!(parse_date(" +123 ").unwrap(), 123);
        assert!(parse_date("0000-01-01").is_ok());
        assert!(parse_date("9999-12-31T23:59:59Z").is_ok());
    }

    #[test]
    fn rejects_invalid_or_unsupported_dates_privately() {
        for value in [
            "",
            "9223372036854775808",
            "-9223372036854775809",
            "2023-02-29",
            "1900-02-29",
            "2024-04-31",
            "2024-13-01",
            "2024-00-01",
            "2024-01-00",
            "2024-1-01",
            "2024-01-01T24:00",
            "2024-01-01T12:60",
            "2024-01-01T12:00:60Z",
            "2024-01-01T12:00:00.",
            "2024-01-01T12:00.5",
            "2024-01-01T12:00:00+24:00",
            "2024-01-01T12:00:00-00:60",
            "2024-01-01T12:00:00+010",
            "2024-01-01Z",
            "today",
            "2024-W01-1",
            "2024-01-01T12:00:00秘密",
            "2024-01-01T12:00:00Zsecret",
        ] {
            let message = parse_date(value).unwrap_err().to_string();
            assert!(!message.contains("secret"));
        }
        assert!(parse_date(&"1".repeat(129)).is_err());
        assert!(DateFilter::new(Some(5), Some(5)).is_err());
        assert!(DateFilter::new(Some(5), Some(6)).is_err());
        assert!(DateFilter::new(Some(6), Some(5)).is_ok());
    }

    fn media(timestamp: Option<i64>) -> Media {
        Media {
            url: "https://example.com/image.jpg".parse().unwrap(),
            job: MediaJob::DirectHttp,
            category: "direct".into(),
            filename: "image.jpg".into(),
            metadata: Some(PostMetadata {
                source_num: None,
                booru_keywords: None,
                source_defaults: None,
                source_suffix: None,
                pool: None,
                favgroup: None,
                search_tags: None,
                keyword_updates: None,
                tags: None,
                sketch: None,
                sketch_archive: false,
                user_bookmark: None,
                bookmark_archive: false,
                ajax: None,
                mypixiv: None,
                pixiv_novel_metadata: None,
                caption_html: None,
                pixiv_user_detail: None,
                comments: None,
                tags_bookmark: None,
                ugoira_original: None,
                ugoira_frame_index: None,
                pixivision: None,
                pixivision_archive: false,
                user_follow: None,
                followed_archive: false,
                series: None,
                num_series: None,
                search: None,
                search_archive: false,
                ranking: None,
                ranking_archive: false,
                profile_asset: None,
                file: None,
                frames: None,
                rating: None,
                score: None,
                revision: None,
                discord: None,
                creator: None,
                community: None,
                community_archive: false,
                retweet_id: None,
                quoted_by_id: None,
                reposted_by_id: None,
                article: None,
                instance: None,
                post_id: "1".into(),
                media_id: "1".into(),
                shortcode: String::new(),
                username: None,
                caption: None,
                timestamp,
                index: 1,
            }),
            headers: BTreeMap::new(),
            handoff: BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn strict_bounds_preserve_unknowns_errors_and_order() {
        let mut unknown = media(None);
        unknown.metadata = None;
        let source = futures_util::stream::iter([
            Ok(media(Some(11))),
            Ok(media(Some(10))),
            Ok(media(Some(9))),
            Err(error("fixture failure")),
            Ok(media(None)),
            Ok(unknown),
            Ok(media(Some(6))),
            Ok(media(Some(5))),
            Ok(media(Some(7))),
        ])
        .boxed();
        let output = DateFilter::new(Some(10), Some(5))
            .unwrap()
            .apply(source)
            .collect::<Vec<_>>()
            .await;
        assert_eq!(output.len(), 5);
        assert_eq!(
            output[0]
                .as_ref()
                .unwrap()
                .metadata
                .as_ref()
                .unwrap()
                .timestamp,
            Some(9)
        );
        assert!(output[1].is_err());
        assert_eq!(
            output[4]
                .as_ref()
                .unwrap()
                .metadata
                .as_ref()
                .unwrap()
                .timestamp,
            Some(6)
        );
    }

    struct Guard(Arc<AtomicBool>);
    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn unordered_bounds_skip_old_items_and_preserve_later_items_errors_and_unknowns() {
        let polls = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = Guard(dropped.clone());
        let count = polls.clone();
        let mut unknown = media(None);
        unknown.metadata = None;
        let source = futures_util::stream::iter([
            Ok(media(Some(4))),
            Ok(media(Some(5))),
            Ok(media(Some(6))),
            Err(error("fixture failure")),
            Ok(media(None)),
            Ok(unknown),
            Ok(media(Some(10))),
            Ok(media(Some(11))),
            Ok(media(Some(7))),
            Ok(media(Some(1))),
            Ok(media(Some(9))),
            Ok(media(Some(8))),
        ])
        .inspect(move |_| {
            let _guard = &guard;
            count.fetch_add(1, Ordering::SeqCst);
        })
        .boxed();
        let mut output = DateFilter::new(Some(10), Some(5))
            .unwrap()
            .apply_unordered(source);
        assert_eq!(polls.load(Ordering::SeqCst), 0);
        assert_eq!(
            output
                .next()
                .await
                .unwrap()
                .unwrap()
                .metadata
                .unwrap()
                .timestamp,
            Some(6)
        );
        assert_eq!(polls.load(Ordering::SeqCst), 3);
        assert!(output.next().await.unwrap().is_err());
        assert_eq!(
            output
                .next()
                .await
                .unwrap()
                .unwrap()
                .metadata
                .unwrap()
                .timestamp,
            None
        );
        assert!(output.next().await.unwrap().unwrap().metadata.is_none());
        for expected in [7, 9] {
            assert_eq!(
                output
                    .next()
                    .await
                    .unwrap()
                    .unwrap()
                    .metadata
                    .unwrap()
                    .timestamp,
                Some(expected)
            );
        }
        assert_eq!(polls.load(Ordering::SeqCst), 11);
        assert!(!dropped.load(Ordering::SeqCst));
        drop(output);
        assert!(dropped.load(Ordering::SeqCst));
        assert_eq!(polls.load(Ordering::SeqCst), 11);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unordered_skipping_old_items_yields_and_cancels() {
        let polls = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = Guard(dropped.clone());
        let count = polls.clone();
        let source = futures_util::stream::repeat_with(move || {
            let _guard = &guard;
            count.fetch_add(1, Ordering::SeqCst);
            Ok(media(Some(5)))
        })
        .boxed();
        let task = tokio::spawn(async move {
            DateFilter::new(None, Some(5))
                .unwrap()
                .apply_unordered(source)
                .next()
                .await
        });
        tokio::task::yield_now().await;
        assert!(polls.load(Ordering::SeqCst) >= 64);
        assert!(!task.is_finished());
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn lazy_and_drops_at_after_boundary_without_prefetch() {
        let polls = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = Guard(dropped.clone());
        let count = polls.clone();
        let source = futures_util::stream::repeat_with(move || {
            let _guard = &guard;
            let index = count.fetch_add(1, Ordering::SeqCst);
            Ok(media(Some(10 - index as i64)))
        })
        .boxed();
        let mut output = DateFilter::new(None, Some(9)).unwrap().apply(source);
        assert_eq!(polls.load(Ordering::SeqCst), 0);
        assert!(output.next().await.unwrap().is_ok());
        assert_eq!(polls.load(Ordering::SeqCst), 1);
        assert!(!dropped.load(Ordering::SeqCst));
        assert!(output.next().await.is_none());
        assert!(dropped.load(Ordering::SeqCst));
        assert_eq!(polls.load(Ordering::SeqCst), 2);
        assert!(output.next().await.is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn skipping_yields_and_cancellation_drops_source() {
        let polls = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = Guard(dropped.clone());
        let count = polls.clone();
        let source = futures_util::stream::repeat_with(move || {
            let _guard = &guard;
            count.fetch_add(1, Ordering::SeqCst);
            Ok(media(Some(20)))
        })
        .boxed();
        let task = tokio::spawn(async move {
            DateFilter::new(Some(10), None)
                .unwrap()
                .apply(source)
                .next()
                .await
        });
        tokio::task::yield_now().await;
        assert!(polls.load(Ordering::SeqCst) >= 64);
        assert!(!task.is_finished());
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(dropped.load(Ordering::SeqCst));
    }
}
