//! Bounded, lazy gallery-dl style file-index selection.
use crate::{Result, error, extract::Media};
use futures_util::{StreamExt, stream::BoxStream};

const MAX_SPEC_BYTES: usize = 4096;
const MAX_GROUPS: usize = 256;

#[derive(Clone, Debug)]
struct Progression {
    start: i128,
    stop: i128,
    step: i128,
}

impl Progression {
    fn contains(&self, index: u64) -> bool {
        let index = i128::from(index);
        index > 0
            && if self.step > 0 {
                index >= self.start && index < self.stop && (index - self.start) % self.step == 0
            } else {
                index <= self.start && index > self.stop && (self.start - index) % -self.step == 0
            }
    }

    fn last_index(&self) -> Option<u64> {
        let last = if self.step > 0 {
            if self.start >= self.stop {
                return None;
            }
            self.start + ((self.stop - 1 - self.start) / self.step) * self.step
        } else {
            if self.start <= self.stop {
                return None;
            }
            self.start
        };
        if last <= 0 { None } else { Some(last as u64) }
    }
}

/// A union of inclusive hyphen ranges and exclusive-stop colon ranges.
/// Positions are one-based; negative colon arguments are mathematical integers,
/// rather than offsets from the end of a collection.
#[derive(Clone, Debug)]
pub struct RangeSelection {
    ranges: Vec<Progression>,
    last: Option<u64>,
}

impl RangeSelection {
    pub fn parse(spec: &str) -> Result<Self> {
        if spec.len() > MAX_SPEC_BYTES || spec.split(',').count() > MAX_GROUPS {
            return Err(error("file range exceeds the supported size limit"));
        }
        fn number(value: &str) -> Result<i128> {
            value
                .trim()
                .parse::<i64>()
                .map(i128::from)
                .map_err(|_| error("invalid integer in file range"))
        }
        fn optional(value: &str, default: i128) -> Result<i128> {
            if value.trim().is_empty() {
                Ok(default)
            } else {
                number(value)
            }
        }
        let mut ranges = Vec::new();
        for group in spec.split(',') {
            // Upstream skips empty comma groups, but not whitespace-only ones.
            if group.is_empty() {
                continue;
            }
            let range = if let Some((start, rest)) = group.split_once(':') {
                let (stop, step) = rest.split_once(':').unwrap_or((rest, ""));
                let step = optional(step, 1)?;
                if step == 0 {
                    return Err(error("file range step must be nonzero"));
                }
                Progression {
                    start: optional(start, 1)?,
                    stop: optional(stop, i128::from(i64::MAX))?,
                    step,
                }
            } else if let Some((start, stop)) = group.split_once('-') {
                Progression {
                    start: optional(start, 1)?,
                    stop: if stop.trim().is_empty() {
                        i128::from(i64::MAX)
                    } else {
                        number(stop)? + 1
                    },
                    step: 1,
                }
            } else {
                let start = number(group)?;
                Progression {
                    start,
                    stop: start + 1,
                    step: 1,
                }
            };
            ranges.push(range);
        }
        let last = ranges.iter().filter_map(Progression::last_index).max();
        Ok(Self { ranges, last })
    }

    pub fn contains(&self, index: u64) -> bool {
        index > 0 && self.ranges.iter().any(|range| range.contains(index))
    }

    /// The last positive selected index, computed without expanding ranges.
    pub fn last_index(&self) -> Option<u64> {
        self.last
    }

    /// Select files in original order and drop upstream at the final match.
    /// Extraction errors pass through without consuming a file position.
    pub fn apply<'a>(&self, input: BoxStream<'a, Result<Media>>) -> BoxStream<'a, Result<Media>> {
        let selection = self.clone();
        let input = self.last.map(|_| input);
        futures_util::stream::unfold(
            (input, 0u64, selection),
            |(mut input, mut index, selection)| async move {
                let mut skipped = 0u8;
                loop {
                    let item = input.as_mut()?.next().await?;
                    match item {
                        Err(err) => return Some((Err(err), (input, index, selection))),
                        Ok(media) => {
                            index = index.checked_add(1)?;
                            let selected = selection.contains(index);
                            if Some(index) == selection.last {
                                // Drop before publishing the item, even if the consumer pauses.
                                input = None;
                            }
                            if selected {
                                return Some((Ok(media), (input, index, selection)));
                            }
                            skipped += 1;
                            if skipped == 64 {
                                // An always-ready source must still permit cancellation
                                // while scanning a distant selected position.
                                tokio::task::yield_now().await;
                                skipped = 0;
                            }
                        }
                    }
                }
            },
        )
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::MediaJob;
    use std::{
        collections::BTreeMap,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        task::Poll,
    };

    fn selected(spec: &str, end: u64) -> Vec<u64> {
        let selection = RangeSelection::parse(spec).unwrap();
        (0..=end)
            .filter(|&index| selection.contains(index))
            .collect()
    }

    #[test]
    fn upstream_range_syntax() {
        assert_eq!(selected("-2,4,6-8,10-", 12), [1, 2, 4, 6, 7, 8, 10, 11, 12]);
        assert_eq!(selected(" - 3 , 4-  4, 2-6", 8), [1, 2, 3, 4, 5, 6]);
        assert_eq!(selected("1:2,4:8:2", 10), [1, 4, 6]);
        assert_eq!(selected("10:1:-3", 12), [4, 7, 10]);
        assert_eq!(selected("-4:5:2", 8), [2, 4]);
        assert!(selected("0,8-3,3:3,0:-5:-1,,", 10).is_empty());
        assert_eq!(selected("::2", 7), [1, 3, 5, 7]);
        assert!(selected("5::-1", 8).is_empty());
        assert_eq!(
            RangeSelection::parse("7:15:3").unwrap().last_index(),
            Some(13)
        );
        assert_eq!(
            RangeSelection::parse("10:1:-3").unwrap().last_index(),
            Some(10)
        );
        assert_eq!(RangeSelection::parse("0").unwrap().last_index(), None);
        assert_eq!(
            RangeSelection::parse("1-").unwrap().last_index(),
            Some(i64::MAX as u64 - 1)
        );
    }

    #[test]
    fn extremes_do_not_expand_or_overflow() {
        let max = i64::MAX as u64;
        let all = RangeSelection::parse("-9223372036854775808:9223372036854775807").unwrap();
        assert!(all.contains(1));
        assert!(all.contains(max - 1));
        assert!(!all.contains(max));
        assert_eq!(all.last_index(), Some(max - 1));
        let end = RangeSelection::parse("9223372036854775807-9223372036854775807").unwrap();
        assert!(end.contains(max));
        assert!(!end.contains(max + 1));
        assert_eq!(end.last_index(), Some(max));
        assert_eq!(
            selected("5:-9223372036854775808:-9223372036854775808", 8),
            [5]
        );
        assert!(selected("-9223372036854775808:5:9223372036854775807", 8).is_empty());
    }

    #[test]
    fn signed_progressions_match_enumeration() {
        for start in -6i64..=6 {
            for stop in -6i64..=6 {
                for step in -5i64..=5 {
                    if step == 0 {
                        continue;
                    }
                    let selection =
                        RangeSelection::parse(&format!("{start}:{stop}:{step}")).unwrap();
                    let mut expected = Vec::new();
                    let mut index = start;
                    while if step > 0 { index < stop } else { index > stop } {
                        if index > 0 {
                            expected.push(index as u64);
                        }
                        index += step;
                    }
                    expected.sort_unstable();
                    assert_eq!(
                        (0..=8)
                            .filter(|&index| selection.contains(index))
                            .collect::<Vec<_>>(),
                        expected
                    );
                    assert_eq!(selection.last_index(), expected.last().copied());
                }
            }
        }
    }

    #[test]
    fn invalid_and_bounded_specs() {
        for spec in [
            "1:4:0",
            "1:2:3:4",
            "a",
            " ",
            "1-2-3",
            "9223372036854775808",
            "1:-9223372036854775809",
            "1:3:9223372036854775808",
        ] {
            assert!(RangeSelection::parse(spec).is_err(), "{spec}");
        }
        assert!(RangeSelection::parse(&"1".repeat(4097)).is_err());
        assert!(RangeSelection::parse(&vec!["1"; 257].join(",")).is_err());
        assert!(RangeSelection::parse(&vec!["1"; 256].join(",")).is_ok());
    }

    fn media() -> Media {
        Media {
            url: "https://example.com/a.jpg".parse().unwrap(),
            job: MediaJob::DirectHttp,
            category: "direct".into(),
            filename: "a.jpg".into(),
            metadata: None,
            headers: BTreeMap::new(),
            handoff: BTreeMap::new(),
        }
    }

    struct Tracked {
        polls: Arc<AtomicUsize>,
        dropped: Arc<AtomicBool>,
    }
    impl Drop for Tracked {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }
    impl futures_util::Stream for Tracked {
        type Item = Result<Media>;
        fn poll_next(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> Poll<Option<Self::Item>> {
            let poll = self.polls.fetch_add(1, Ordering::SeqCst);
            Poll::Ready(Some(if poll == 1 {
                Err(error("fixture extraction failure"))
            } else {
                Ok(media())
            }))
        }
    }

    #[tokio::test]
    async fn lazy_stream_errors_positions_and_immediate_drop() {
        let polls = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicBool::new(false));
        let source = Tracked {
            polls: polls.clone(),
            dropped: dropped.clone(),
        }
        .boxed();
        let mut output = RangeSelection::parse("2").unwrap().apply(source);
        assert_eq!(polls.load(Ordering::SeqCst), 0);
        assert!(output.next().await.unwrap().is_err());
        assert_eq!(polls.load(Ordering::SeqCst), 2);
        assert!(!dropped.load(Ordering::SeqCst));
        assert!(output.next().await.unwrap().is_ok());
        assert!(dropped.load(Ordering::SeqCst));
        assert_eq!(polls.load(Ordering::SeqCst), 3);
        assert!(output.next().await.is_none());
        assert_eq!(polls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn skipping_always_ready_items_yields_and_can_be_cancelled() {
        let polls = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicBool::new(false));
        let tracked = Tracked {
            polls: polls.clone(),
            dropped: dropped.clone(),
        };
        let source = futures_util::stream::repeat_with(move || {
            let _guard = &tracked;
            tracked.polls.fetch_add(1, Ordering::SeqCst);
            Ok(media())
        })
        .boxed();
        let selection = RangeSelection::parse("9223372036854775807").unwrap();
        let task = tokio::spawn(async move {
            let mut output = selection.apply(source);
            output.next().await
        });
        tokio::task::yield_now().await;
        assert!(polls.load(Ordering::SeqCst) >= 64);
        assert!(!task.is_finished());
        assert!(!dropped.load(Ordering::SeqCst));
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn empty_drops_without_polling_and_apply_resets_index() {
        let polls = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicBool::new(false));
        let source = Tracked {
            polls: polls.clone(),
            dropped: dropped.clone(),
        }
        .boxed();
        let mut output = RangeSelection::parse("0").unwrap().apply(source);
        assert!(dropped.load(Ordering::SeqCst));
        assert!(output.next().await.is_none());
        assert_eq!(polls.load(Ordering::SeqCst), 0);
        let selection = RangeSelection::parse("3,1,1").unwrap();
        for _ in 0..2 {
            let output = selection
                .apply(futures_util::stream::iter((0..5).map(|_| Ok(media()))).boxed())
                .collect::<Vec<_>>()
                .await;
            assert_eq!(output.len(), 2);
        }
    }
}
