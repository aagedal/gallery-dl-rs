//! Search query routing and progress tracking. API snapshots are not stable contracts.
use super::*;
use sha2::{Digest, Sha256};
use std::collections::HashSet;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TimelineStrategy {
    #[default]
    Auto,
    Media,
    Tweets,
    Replies,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SearchPagination {
    #[default]
    MaxId,
    Cursor,
    Date,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SearchProduct {
    #[default]
    Latest,
    Top,
    Media,
    People,
    Lists,
}
impl SearchProduct {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Latest => "Latest",
            Self::Top => "Top",
            Self::Media => "Media",
            Self::People => "People",
            Self::Lists => "Lists",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimelineOptions {
    pub strategy: TimelineStrategy,
    pub search_pagination: SearchPagination,
    pub search_product: SearchProduct,
    pub limit: u8,
    pub search_limit: u8,
    pub search_stop: u8,
}
impl Default for TimelineOptions {
    fn default() -> Self {
        Self {
            strategy: TimelineStrategy::Auto,
            search_pagination: SearchPagination::MaxId,
            search_product: SearchProduct::Latest,
            limit: 50,
            search_limit: 20,
            search_stop: 3,
        }
    }
}

pub fn is_search_url(url: &Url) -> bool {
    matches!(input(url), Ok(Some(_)))
}

pub(super) fn input(url: &Url) -> Result<Option<String>> {
    if !matches!(url.scheme(), "http" | "https")
        || !crate::twitter::twitter_host(url.host_str())
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return Err(error(
            "native Twitter search requires a first-party search or hashtag URL",
        ));
    }
    let path = url.path().strip_suffix('/').unwrap_or(url.path());
    let query = if path == "/search" {
        let queries: Vec<_> = url
            .query_pairs()
            .filter(|(key, _)| key == "q")
            .map(|(_, v)| v.into_owned())
            .collect();
        if queries.len() != 1 {
            return Err(error("Twitter search requires one query parameter"));
        }
        queries.into_iter().next().unwrap()
    } else if let Some(tag) = path
        .strip_prefix("/hashtag/")
        .filter(|tag| !tag.contains('/'))
    {
        // Path '+' is literal, while form query decoding uses it for a space.
        let encoded = format!("q={}", tag.replace('+', "%2B"));
        let tag = url::form_urlencoded::parse(encoded.as_bytes())
            .next()
            .unwrap()
            .1
            .into_owned();
        format!("#{tag}")
    } else {
        return Ok(None);
    };
    validate_query(&query)?;
    if query == "#" {
        return Err(error("Twitter hashtag is empty"));
    }
    Ok(Some(query))
}

pub(super) fn validate_query(query: &str) -> Result<()> {
    if query.trim().is_empty()
        || query.len() > 8192
        || query.chars().any(|c| c.is_control() || c == '\u{fffd}')
    {
        return Err(error(
            "Twitter search query is empty, invalid, or exceeds its limit",
        ));
    }
    Ok(())
}

pub(super) fn query_owner(query: &str) -> Option<timeline::ProfileInput> {
    let mut from = query
        .split_whitespace()
        .map(|s| s.trim_matches(['(', ')']))
        .filter_map(|s| s.strip_prefix("from:"));
    let name = from.next()?;
    if from.next().is_some()
        || name.is_empty()
        || name.len() > 15
        || !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
    {
        return None;
    }
    Some(timeline::ProfileInput::Name(name.into()))
}

pub(super) struct Paging {
    pub(super) query: String,
    pub(super) cursor: Option<String>,
    pub(super) any_posts: bool,
    requests: HashSet<[u8; 32]>,
    empty_pages: u8,
    max_id: Option<u64>,
    last_timestamp: Option<u64>,
}
impl Paging {
    pub(super) fn new(query: String) -> Self {
        Self {
            query,
            cursor: None,
            any_posts: false,
            requests: HashSet::new(),
            empty_pages: 0,
            max_id: None,
            last_timestamp: None,
        }
    }
    pub(super) fn request(&mut self) -> Result<()> {
        validate_query(&self.query)?;
        let mut hash = Sha256::new();
        hash.update(self.query.len().to_le_bytes());
        hash.update(self.query.as_bytes());
        let cursor = self.cursor.as_deref().unwrap_or_default();
        hash.update(cursor.len().to_le_bytes());
        hash.update(cursor.as_bytes());
        // Date mode can intentionally retry the same until-day query when the
        // last full timestamp advances. Distinguish that progress from a loop.
        hash.update(self.last_timestamp.unwrap_or_default().to_le_bytes());
        if !self.requests.insert(hash.finalize().into()) {
            return Err(error("Twitter search pagination repeated a request"));
        }
        Ok(())
    }
    pub(super) fn advance(
        &mut self,
        page: &timeline::Page,
        options: TimelineOptions,
    ) -> Result<bool> {
        let last = page.posts.last().map(|(id, _)| id.as_str());
        self.any_posts |= last.is_some();
        if page.done {
            return Ok(true);
        }
        let cursor = page
            .cursor
            .as_ref()
            .ok_or_else(|| error("Twitter search pagination is incomplete"))?;
        if last.is_none() {
            if page.continue_empty {
                self.empty_pages = 0;
            } else {
                if self.empty_pages >= options.search_stop {
                    return Ok(true);
                }
                self.empty_pages += 1;
            }
            self.cursor = Some(cursor.clone());
            return Ok(false);
        }
        self.empty_pages = 0;
        let id = last
            .unwrap()
            .parse::<u64>()
            .map_err(|_| error("Twitter search post identifier invalid"))?;
        match options.search_pagination {
            SearchPagination::Cursor => self.cursor = Some(cursor.clone()),
            SearchPagination::MaxId => {
                if id <= 1 {
                    return Ok(true);
                }
                let next = id - 1;
                if self.max_id.is_some_and(|prior| next >= prior) {
                    return Err(error("Twitter search pagination did not advance"));
                }
                self.max_id = Some(next);
                self.query = replace_operator(&self.query, "max_id:", &next.to_string());
                self.cursor = None;
            }
            SearchPagination::Date => {
                let millis = (id >> 22) + 1_288_834_974_657;
                let date = time::OffsetDateTime::from_unix_timestamp((millis / 1000) as i64)
                    .map_err(|_| error("Twitter search timestamp invalid"))?;
                let next = format!(
                    "{:04}-{:02}-{:02}",
                    date.year(),
                    date.month() as u8,
                    date.day()
                );
                if self.last_timestamp == Some(millis) {
                    self.cursor = Some(cursor.clone());
                } else {
                    if self.last_timestamp.is_some_and(|prior| millis > prior) {
                        return Err(error("Twitter search date pagination did not advance"));
                    }
                    self.query = replace_operator(&self.query, "until:", &next);
                    self.cursor = None;
                    self.last_timestamp = Some(millis);
                }
            }
        }
        validate_query(&self.query)?;
        Ok(false)
    }
}

// Replace the same bounded numeric/date operator shape used upstream, preserving
// surrounding parentheses and unrelated query text instead of splitting tokens.
fn replace_operator(query: &str, operator: &str, replacement: &str) -> String {
    let mut output = String::new();
    let mut offset = 0;
    let mut found = false;
    for (start, _) in query.match_indices(operator) {
        if start < offset
            || query[..start]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_')
        {
            continue;
        }
        let value_start = start + operator.len();
        let tail = &query[value_start..];
        let len = if operator == "until:" {
            if tail.len() >= 10
                && tail.as_bytes()[..10].iter().enumerate().all(|(i, c)| {
                    if i == 4 || i == 7 {
                        *c == b'-'
                    } else {
                        c.is_ascii_digit()
                    }
                })
            {
                10
            } else {
                0
            }
        } else {
            tail.bytes().take_while(u8::is_ascii_digit).count()
        };
        if len == 0 {
            continue;
        }
        output.push_str(&query[offset..value_start]);
        output.push_str(replacement);
        offset = value_start + len;
        found = true;
    }
    if !found {
        return format!("{query} {operator}{replacement}");
    }
    output.push_str(&query[offset..]);
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn page(ids: &[&str], cursor: Option<&str>) -> timeline::Page {
        let mut entries: Vec<_> = ids.iter().map(|id| json!({"content":{"entryType":"TimelineTimelineItem",
            "itemContent":{"itemType":"TimelineTweet","tweet_results":{"result":{"__typename":"Tweet",
                "rest_id":id,"legacy":{"id_str":id,"user_id_str":"42"}}}}}})).collect();
        if let Some(cursor) = cursor {
            entries.push(json!({"content":{"entryType":"TimelineTimelineCursor","cursorType":"Bottom","value":cursor}}));
        }
        let bytes=serde_json::to_vec(&json!({"data":{"search_by_raw_query":{"search_timeline":{"timeline":{"instructions":[{"type":"TimelineAddEntries","entries":entries}]}}}}})).unwrap();
        timeline::parse_search_posts(&bytes).unwrap()
    }

    #[test]
    fn search_routes_decode_once_and_preserve_literal_hashtag_plus() {
        for (url, expected) in [
            (
                "https://x.com/search?q=cats+%26+dogs%2520&src=typed_query",
                "cats & dogs%20",
            ),
            ("https://twitter.com/search/?src=x&q=%23rust", "#rust"),
            ("https://mobile.x.com/hashtag/rust+lang", "#rust+lang"),
            ("https://x.com/hashtag/%E6%97%A5%E6%9C%AC?src=x", "#日本"),
        ] {
            let url = Url::parse(url).unwrap();
            assert_eq!(input(&url).unwrap().as_deref(), Some(expected));
            assert!(is_search_url(&url));
        }
        assert!(
            input(&Url::parse("https://x.com/fixture/tweets").unwrap())
                .unwrap()
                .is_none()
        );
        for url in [
            "https://x.com/search",
            "https://x.com/search?q=",
            "https://x.com/search?q=+",
            "https://x.com/search?q=a&q=b",
            "https://x.com/search?q=secret%00",
            "https://x.com/search?q=%FF",
            "https://x.com.evil/search?q=secret",
            "https://user:secret@x.com/search?q=x",
            "https://x.com:444/search?q=x",
            "file:///search?q=secret",
        ] {
            let message = input(&Url::parse(url).unwrap()).unwrap_err().to_string();
            assert!(!message.contains("secret"));
        }
        assert!(validate_query(&"x".repeat(8193)).is_err());
        assert!(validate_query(&"x".repeat(8192)).is_ok());
    }

    #[test]
    fn query_owner_requires_one_valid_explicit_from_operator() {
        assert_eq!(
            query_owner("(from:Fixture) filter:links"),
            Some(timeline::ProfileInput::Name("Fixture".into()))
        );
        for query in [
            "fixture",
            "from:one from:two",
            "from:invalid-name",
            "from:",
            "from:abcdefghijklmnop",
        ] {
            assert_eq!(query_owner(query), None);
        }
    }

    #[test]
    fn operator_rewrites_preserve_boundaries_parentheses_and_query_text() {
        assert_eq!(
            replace_operator("cats (max_id:999) max_id:888!", "max_id:", "42"),
            "cats (max_id:42) max_id:42!"
        );
        assert_eq!(
            replace_operator("xmax_id:999 _max_id:888 日本max_id:777", "max_id:", "42"),
            "xmax_id:999 _max_id:888 日本max_id:777 max_id:42"
        );
        assert_eq!(
            replace_operator("(until:2026-01-02) until:bad", "until:", "2025-02-03"),
            "(until:2025-02-03) until:bad"
        );
    }

    #[test]
    fn query_and_cursor_history_detect_loops_and_bounds_before_network() {
        let mut paging = Paging::new("fixture".into());
        paging.request().unwrap();
        assert!(paging.request().is_err());
        paging.cursor = Some("opaque".into());
        paging.request().unwrap();
        assert!(paging.request().is_err());
        let mut paging = Paging::new("fixture".into());
        assert!(
            !paging
                .advance(&page(&["100"], Some("a")), TimelineOptions::default())
                .unwrap()
        );
        assert!(
            paging
                .advance(&page(&["101"], Some("b")), TimelineOptions::default())
                .is_err()
        );
        let mut paging = Paging::new("x".repeat(8192));
        assert!(
            paging
                .advance(&page(&["100"], Some("a")), TimelineOptions::default())
                .is_err()
        );
    }
}
