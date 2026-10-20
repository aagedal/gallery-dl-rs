//! Bounded, cooperatively cancellable creator and favorite discovery.
use super::{Kemono, MAX_RESPONSE, valid_discord_id, valid_id, valid_service};
use crate::{Result, error, event::ExtractionEvent};
use futures_util::{
    StreamExt,
    stream::{self, BoxStream},
};
use serde_json::Value;
use std::{
    cmp::Ordering,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering as AtomicOrder},
    },
};
use url::Url;
const MAX_ITEMS: usize = 500_000;
const MAX_TEXT: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Favorites {
    #[default]
    Artist,
    Post,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Artists,
    Favorites,
}
#[derive(Clone)]
struct Query {
    kind: Kind,
    favorites: Favorites,
    service: Option<String>,
    search: Option<String>,
    sort: String,
    reverse: bool,
}
fn invalid() -> crate::Error {
    error("invalid Kemono collection data or query")
}
pub(super) fn kind(url: &Url) -> Option<Kind> {
    if url.path().starts_with("/artists") {
        Some(Kind::Artists)
    } else if url.path().starts_with("/favorites") || url.path().starts_with("/account/favorites") {
        Some(Kind::Favorites)
    } else {
        None
    }
}
fn query(url: &Url, kind: Kind, default: Favorites) -> Result<Query> {
    let captures = match kind {
        Kind::Artists => url.path() == "/artists",
        Kind::Favorites => matches!(
            url.path(),
            "/favorites" | "/favorites/" | "/account/favorites" | "/account/favorites/"
        ),
    };
    let mut pairs = std::collections::HashMap::new();
    if captures {
        if url.query().is_some_and(|query| query.len() > 8192) {
            return Err(invalid());
        }
        for part in url.query().unwrap_or_default().split('&') {
            if !part.contains('=') {
                continue;
            }
            for (key, value) in url::form_urlencoded::parse(part.as_bytes()) {
                let recognized = match kind {
                    Kind::Artists => matches!(key.as_ref(), "service" | "q" | "sort_by" | "order"),
                    Kind::Favorites => matches!(key.as_ref(), "type" | "sort" | "order"),
                };
                if recognized {
                    pairs
                        .entry(key.into_owned())
                        .or_insert_with(|| value.into_owned());
                }
            }
        }
        if pairs
            .values()
            .any(|value| value.len() > 1024 || value.chars().any(char::is_control))
        {
            return Err(invalid());
        }
    }

    let favorites = match pairs
        .get("type")
        .filter(|value| !value.is_empty())
        .map(String::as_str)
    {
        Some("artist") => Favorites::Artist,
        Some("post") => Favorites::Post,
        Some(_) if kind == Kind::Favorites => return Err(invalid()),
        _ => default,
    };
    let sort = pairs
        .get(if kind == Kind::Artists {
            "sort_by"
        } else {
            "sort"
        })
        .filter(|value| !value.is_empty())
        .cloned()
        .unwrap_or_else(|| {
            if kind == Kind::Artists {
                "favorited"
            } else if favorites == Favorites::Artist {
                "updated"
            } else {
                "faved_seq"
            }
            .into()
        });
    if sort.len() > 128
        || !sort
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(invalid());
    }
    let order = pairs
        .get("order")
        .filter(|value| !value.is_empty())
        .map_or("desc", String::as_str);
    let service = if kind == Kind::Artists {
        pairs
            .get("service")
            .filter(|value| !value.is_empty())
            .map(|value| value.to_lowercase())
    } else {
        None
    };
    if service
        .as_ref()
        .is_some_and(|service| !valid_service(service))
    {
        return Err(invalid());
    }
    let search = if kind == Kind::Artists {
        pairs
            .get("q")
            .filter(|value| !value.is_empty())
            .map(|value| value.to_lowercase())
    } else {
        None
    };
    Ok(Query {
        kind,
        favorites,
        service,
        search,
        sort,
        reverse: if kind == Kind::Artists {
            order != "asc"
        } else {
            order == "desc"
        },
    })
}
pub(super) fn validate(url: &Url, kind: Kind) -> Result<()> {
    query(url, kind, Favorites::Artist).map(|_| ())
}

#[derive(Debug)]
enum Number {
    Integer(String),
    Float(f64),
}
#[derive(Debug)]
enum Key {
    None,
    Number(Number),
    Text(String),
}
fn key(value: &Value) -> Result<Key> {
    Ok(match value {
        Value::Null | Value::Bool(false) => Key::None,
        Value::Bool(true) => Key::Number(Number::Integer("1".into())),
        Value::String(text) if text.is_empty() => Key::None,
        Value::String(text) if text.len() <= MAX_TEXT => Key::Text(text.clone()),
        Value::Array(values) if values.is_empty() => Key::None,
        Value::Object(values) if values.is_empty() => Key::None,
        Value::Number(number) => {
            let text = number.to_string();
            if text.len() > 128 {
                return Err(invalid());
            }
            if text.contains(['.', 'e', 'E']) {
                let value = number
                    .as_f64()
                    .filter(|value| value.is_finite())
                    .ok_or_else(invalid)?;
                if value == 0.0 {
                    Key::None
                } else {
                    Key::Number(Number::Float(value))
                }
            } else if text == "0" || text == "-0" {
                Key::None
            } else {
                Key::Number(Number::Integer(text))
            }
        }
        _ => return Err(invalid()),
    })
}
fn magnitude(left: &str, right: &str) -> Ordering {
    left.len().cmp(&right.len()).then_with(|| left.cmp(right))
}
fn integers(left: &str, right: &str) -> Ordering {
    let negative_left = left.starts_with('-');
    let negative_right = right.starts_with('-');
    match (negative_left, negative_right) {
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        (true, true) => magnitude(&right[1..], &left[1..]),
        _ => magnitude(left, right),
    }
}
fn integer_float(integer: &str, float: f64) -> Ordering {
    let negative = integer.starts_with('-');
    if negative != (float < 0.0) {
        return if negative {
            Ordering::Less
        } else {
            Ordering::Greater
        };
    }
    // A finite float's truncated integral part has an exact decimal spelling;
    // compare that integer text, never round a u64/large integer through f64.
    let whole = format!("{:.0}", float.abs().trunc());
    let order = magnitude(integer.strip_prefix('-').unwrap_or(integer), &whole).then_with(|| {
        if float.fract() != 0.0 {
            Ordering::Less
        } else {
            Ordering::Equal
        }
    });
    if negative { order.reverse() } else { order }
}
fn compare(left: &Key, right: &Key) -> Ordering {
    match (left, right) {
        (Key::None, Key::None) => Ordering::Equal,
        (Key::None, _) => Ordering::Less,
        (_, Key::None) => Ordering::Greater,
        (Key::Text(left), Key::Text(right)) => left.cmp(right),
        (Key::Number(Number::Integer(left)), Key::Number(Number::Integer(right))) => {
            integers(left, right)
        }
        (Key::Number(Number::Float(left)), Key::Number(Number::Float(right))) => {
            left.partial_cmp(right).expect("finite sort number")
        }
        (Key::Number(Number::Integer(left)), Key::Number(Number::Float(right))) => {
            integer_float(left, *right)
        }
        (Key::Number(Number::Float(left)), Key::Number(Number::Integer(right))) => {
            integer_float(right, *left).reverse()
        }
        _ => Ordering::Equal, // Mixed truthy scalar classes are rejected before sorting.
    }
}
fn checkpoint(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(AtomicOrder::Relaxed) {
        Err(error("Kemono collection parsing cancelled"))
    } else {
        Ok(())
    }
}
fn sort_indices(keys: &[Key], reverse: bool, cancel: &AtomicBool) -> Result<Vec<usize>> {
    let mut source: Vec<_> = (0..keys.len()).collect();
    let mut target = vec![0; keys.len()];
    let mut width = 1;
    while width < keys.len() {
        for start in (0..keys.len()).step_by(width * 2) {
            checkpoint(cancel)?;
            let middle = (start + width).min(keys.len());
            let end = (middle + width).min(keys.len());
            let (mut left, mut right) = (start, middle);
            for slot in &mut target[start..end] {
                checkpoint(cancel)?;
                let order = if left < middle && right < end {
                    let (a, b) = (source[left], source[right]);
                    if matches!((&keys[a], &keys[b]), (Key::None, Key::None)) {
                        b.cmp(&a)
                    } else {
                        let order = compare(&keys[a], &keys[b]);
                        if reverse { order.reverse() } else { order }
                    }
                } else {
                    Ordering::Equal
                };
                if left < middle && (right >= end || order != Ordering::Greater) {
                    *slot = source[left];
                    left += 1;
                } else {
                    *slot = source[right];
                    right += 1;
                }
            }
        }
        std::mem::swap(&mut source, &mut target);
        width *= 2;
    }
    checkpoint(cancel)?;
    Ok(source)
}
#[derive(Debug)]
enum Child {
    Invalid,
    User {
        service: String,
        user: String,
    },
    Post {
        service: String,
        user: String,
        post: String,
    },
    Discord {
        server: String,
    },
}
impl Child {
    fn url(self, host: &str) -> Result<Url> {
        let path = match self {
            Self::Invalid => return Err(invalid()),
            Self::User { service, user } => {
                if !valid_service(&service) || !valid_id(&user) {
                    return Err(invalid());
                }
                format!("{service}/user/{user}")
            }
            Self::Post {
                service,
                user,
                post,
            } => {
                if !valid_service(&service) || !valid_id(&user) || !valid_id(&post) {
                    return Err(invalid());
                }
                format!("{service}/user/{user}/post/{post}")
            }
            Self::Discord { server } => {
                if !valid_discord_id(&server) {
                    return Err(invalid());
                }
                format!("discord/server/{server}")
            }
        };
        Url::parse(&format!("https://{host}/{path}")).map_err(|_| invalid())
    }
}
fn raw_id(row: &Value, key: &str) -> Option<String> {
    match row.get(key)? {
        Value::String(value) if value.len() <= 128 => Some(value.clone()),
        Value::Number(value) => value.as_u64().map(|value| value.to_string()),
        _ => None,
    }
}
fn child(row: &Value, query: &Query) -> Child {
    let Some(service) = row
        .get("service")
        .and_then(Value::as_str)
        .filter(|service| service.len() <= 32)
    else {
        return Child::Invalid;
    };
    let Some(id) = raw_id(row, "id") else {
        return Child::Invalid;
    };
    if query.kind == Kind::Favorites && query.favorites == Favorites::Post {
        let Some(user) = raw_id(row, "user") else {
            return Child::Invalid;
        };
        Child::Post {
            service: service.into(),
            user,
            post: id,
        }
    } else if service == "discord" {
        Child::Discord { server: id }
    } else {
        Child::User {
            service: service.into(),
            user: id,
        }
    }
}
struct Prepared {
    children: Vec<Option<Child>>,
    keys: Vec<Key>,
    class: Option<u8>,
}
impl Prepared {
    fn row(&mut self, row: Value, query: &Query) -> Result<()> {
        let service = row.get("service").and_then(Value::as_str);
        if query
            .service
            .as_deref()
            .is_some_and(|filter| Some(filter) != service)
        {
            return Ok(());
        }
        if let Some(search) = &query.search {
            let Some(name) = row
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| name.len() <= MAX_TEXT)
            else {
                return Ok(());
            };
            if !name.to_lowercase().contains(search) {
                return Ok(());
            }
        }
        let child = child(&row, query);
        let key = key(row.get(&query.sort).ok_or_else(invalid)?)?;
        let class = match key {
            Key::None => None,
            Key::Number(_) => Some(1),
            Key::Text(_) => Some(2),
        };
        if let Some(class) = class {
            if self.class.is_some_and(|old| old != class) {
                return Err(invalid());
            }
            self.class = Some(class);
        }
        self.children.push(Some(child));
        self.keys.push(key);
        Ok(())
    }
}
struct Rows<'a> {
    query: &'a Query,
    cancel: &'a AtomicBool,
}
impl<'de> serde::de::Visitor<'de> for Rows<'_> {
    type Value = Prepared;
    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a bounded creator list")
    }
    fn visit_seq<A: serde::de::SeqAccess<'de>>(
        self,
        mut sequence: A,
    ) -> std::result::Result<Self::Value, A::Error> {
        let mut prepared = Prepared {
            children: Vec::new(),
            keys: Vec::new(),
            class: None,
        };
        for _ in 0..MAX_ITEMS {
            checkpoint(self.cancel).map_err(serde::de::Error::custom)?;
            let Some(row) = sequence.next_element::<Value>()? else {
                return Ok(prepared);
            };
            checkpoint(self.cancel).map_err(serde::de::Error::custom)?;
            prepared
                .row(row, self.query)
                .map_err(serde::de::Error::custom)?;
        }
        if sequence.next_element::<serde::de::IgnoredAny>()?.is_some() {
            return Err(serde::de::Error::custom("collection item limit"));
        }
        Ok(prepared)
    }
}
struct CancellableReader<'a, R> {
    inner: R,
    cancel: &'a AtomicBool,
}
impl<R: std::io::Read> std::io::Read for CancellableReader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.cancel.load(AtomicOrder::Relaxed) {
            return Err(std::io::Error::other("collection parsing cancelled"));
        }
        let size = buffer.len().min(16 * 1024);
        self.inner.read(&mut buffer[..size])
    }
}
fn parse_reader<R: std::io::Read>(
    reader: R,
    query: &Query,
    cancel: &AtomicBool,
) -> Result<Vec<Child>> {
    checkpoint(cancel)?;
    let reader = std::io::BufReader::with_capacity(
        16 * 1024,
        CancellableReader {
            inner: reader,
            cancel,
        },
    );
    let mut decoder = serde_json::Deserializer::from_reader(reader);
    let mut prepared =
        serde::de::Deserializer::deserialize_seq(&mut decoder, Rows { query, cancel })
            .map_err(|_| invalid())?;
    decoder.end().map_err(|_| invalid())?;
    checkpoint(cancel)?;
    let order = sort_indices(&prepared.keys, query.reverse, cancel)?;
    let mut sorted = Vec::with_capacity(order.len());
    for index in order {
        checkpoint(cancel)?;
        sorted.push(
            prepared.children[index]
                .take()
                .expect("unique collection index"),
        );
    }
    Ok(sorted)
}
fn parse(bytes: &[u8], query: &Query, cancel: &AtomicBool) -> Result<Vec<Child>> {
    if bytes.len() > MAX_RESPONSE {
        return Err(error("Kemono collection exceeds 32 MiB"));
    }
    parse_reader(std::io::Cursor::new(bytes), query, cancel)
}
struct CancelOnDrop(Arc<AtomicBool>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, AtomicOrder::Relaxed);
    }
}
async fn parse_background(bytes: Vec<u8>, query: Query) -> Result<Vec<Child>> {
    let cancel = Arc::new(AtomicBool::new(false));
    let guard = CancelOnDrop(cancel.clone());
    let result = tokio::task::spawn_blocking(move || parse(&bytes, &query, &cancel))
        .await
        .map_err(|_| error("Kemono collection parser failed"))?;
    drop(guard);
    result
}
struct State<'a> {
    host: String,
    query: Query,
    children: std::collections::VecDeque<Child>,
    initialized: bool,
    current: Option<BoxStream<'a, Result<ExtractionEvent>>>,
}
pub(super) fn events<'a>(
    extractor: &'a Kemono,
    url: &Url,
    kind: Kind,
) -> BoxStream<'a, Result<ExtractionEvent>> {
    let query = match query(url, kind, extractor.favorites) {
        Ok(query) => query,
        Err(error) => return stream::once(async move { Err(error) }).boxed(),
    };
    let state = State {
        host: url.host_str().expect("validated host").into(),
        query,
        children: Default::default(),
        initialized: false,
        current: None,
    };
    stream::unfold(Some(state), move |state| async move {
        let mut state = state?;
        loop {
            if let Some(child) = &mut state.current {
                if let Some(event) = child.next().await {
                    return Some((event, Some(state)));
                }
                state.current = None;
            }
            if !state.initialized {
                let favorites = if state.query.favorites == Favorites::Artist {
                    "artist"
                } else {
                    "post"
                };
                let result = async {
                    let bytes = if state.query.kind == Kind::Artists {
                        extractor.call(&state.host, "api/v1/creators", &[]).await?
                    } else {
                        extractor
                            .call(
                                &state.host,
                                "api/v1/account/favorites",
                                &[("type", favorites)],
                            )
                            .await?
                    };
                    parse_background(bytes, state.query.clone()).await
                }
                .await;
                state.children = match result {
                    Ok(children) => children.into(),
                    Err(error) => return Some((Err(error), None)),
                };
                state.initialized = true;
            }
            let child = state.children.pop_front()?;
            let url = match child.url(&state.host) {
                Ok(url) => url,
                Err(error) => return Some((Err(error), Some(state))),
            };
            state.current = Some(extractor.events(&url));
        }
    })
    .boxed()
}

#[cfg(test)]
mod tests {
    use super::super::{Selection, Target, target};
    use super::*;
    use crate::network::NetworkOptions;
    use serde_json::json;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
        time::Duration,
    };
    fn child_ids(children: &[Child]) -> Vec<&str> {
        children
            .iter()
            .map(|child| match child {
                Child::User { user, .. } => user.as_str(),
                Child::Post { post, .. } => post.as_str(),
                Child::Discord { server } => server.as_str(),
                Child::Invalid => "invalid",
            })
            .collect()
    }
    fn parse_url(bytes: &[u8], url: &str, default: Favorites) -> Result<Vec<Child>> {
        let url = Url::parse(url).unwrap();
        let query = query(&url, kind(&url).unwrap(), default)?;
        parse(bytes, &query, &AtomicBool::new(false))
    }

    #[test]
    fn sort_matches_python_oracles_for_sentinel_unicode_and_exact_wide_numbers() {
        let cases: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/kemono/collection-sort.json"
        ))
        .unwrap();
        for case in cases.as_array().unwrap() {
            let mut rows = case["rows"].clone();
            for row in rows.as_array_mut().unwrap() {
                row["service"] = json!("patreon");
            }
            for (order, expected) in [("asc", "ascending"), ("desc", "descending")] {
                let children = parse_url(
                    rows.to_string().as_bytes(),
                    &format!("https://kemono.cr/artists?sort_by=key&order={order}"),
                    Favorites::Artist,
                )
                .unwrap();
                assert_eq!(
                    json!(child_ids(&children)),
                    case[expected],
                    "{} {order}",
                    case["case"]
                );
            }
        }
    }

    #[test]
    fn filters_use_unicode_lowercase_and_identity_errors_wait_until_consumption() {
        let bytes = include_bytes!("../tests/fixtures/kemono/creators.json");
        let children = parse_url(
            bytes,
            "https://kemono.cr/artists?service=FANBOX&q=FiXtUrE+%E7%8C%AB",
            Favorites::Artist,
        )
        .unwrap();
        assert_eq!(child_ids(&children), ["43"]);
        let rows = json!([{"id":"42","service":"patreon","name":"keep","favorited":2},
            {"id":"private/invalid","service":"patreon","name":"later","favorited":1},
            {"id":"private","service":"PRIVATE/INVALID","name":"ignored","favorited":3}]);
        let children = parse_url(
            rows.to_string().as_bytes(),
            "https://kemono.cr/artists?service=patreon",
            Favorites::Artist,
        )
        .unwrap();
        assert_eq!(child_ids(&children), ["42", "private/invalid"]);
        let mut children = children.into_iter();
        assert!(children.next().unwrap().url("kemono.cr").is_ok());
        assert_eq!(
            children
                .next()
                .unwrap()
                .url("kemono.cr")
                .unwrap_err()
                .to_string(),
            "invalid Kemono collection data or query"
        );
    }

    #[test]
    fn captured_queries_use_first_values_and_prefix_suffix_capture_rules() {
        let url=Url::parse("https://kemono.cr/artists?q&service&sort_by&order&q=FiXtUrE&q=bad%00&service=FANBOX&service=private/invalid&sort_by=updated&sort_by=bad%00&order=desc&order=asc").unwrap();
        let parsed = query(&url, Kind::Artists, Favorites::Artist).unwrap();
        assert_eq!(parsed.search.as_deref(), Some("fixture"));
        assert_eq!(parsed.service.as_deref(), Some("fanbox"));
        assert_eq!(parsed.sort, "updated");
        assert!(parsed.reverse);
        let url=Url::parse("https://kemono.cr/favorites?type&sort&order&type=post&type=invalid&sort=faved_seq&sort=bad%00&order=asc&order=desc").unwrap();
        let parsed = query(&url, Kind::Favorites, Favorites::Artist).unwrap();
        assert_eq!(parsed.favorites, Favorites::Post);
        assert_eq!(parsed.sort, "faved_seq");
        assert!(!parsed.reverse);
        for (url, kind) in [
            ("https://kemono.cr/artists?q=bad%00&q=valid", Kind::Artists),
            (
                "https://kemono.cr/favorites?type=invalid&type=post",
                Kind::Favorites,
            ),
        ] {
            assert!(query(&Url::parse(url).unwrap(), kind, Favorites::Artist).is_err());
        }
        for path in [
            "/favorites/artists?type=post",
            "/account/favorites/artists?type=post",
        ] {
            let url = Url::parse(&format!("https://kemono.cr{path}")).unwrap();
            let parsed = query(&url, Kind::Favorites, Favorites::Artist).unwrap();
            assert_eq!(parsed.favorites, Favorites::Artist);
            assert!(matches!(target(&url).unwrap(), Target::Favorites { .. }));
        }
        let parsed = query(
            &Url::parse("https://kemono.cr/artists/?service=fanbox&order=asc").unwrap(),
            Kind::Artists,
            Favorites::Artist,
        )
        .unwrap();
        assert!(parsed.service.is_none());
        assert!(parsed.reverse);
        let artists = query(
            &Url::parse("https://kemono.cr/artists?order=DESC").unwrap(),
            Kind::Artists,
            Favorites::Artist,
        )
        .unwrap();
        assert!(artists.reverse);
        let favorites = query(
            &Url::parse("https://kemono.cr/favorites?order=DESC").unwrap(),
            Kind::Favorites,
            Favorites::Artist,
        )
        .unwrap();
        assert!(!favorites.reverse);
    }

    #[test]
    fn bounded_streaming_visitor_caps_rows_without_retaining_full_array() {
        assert_eq!(MAX_ITEMS, 500_000);
        let input = format!(
            "[{}]",
            std::iter::repeat_n("{}", MAX_ITEMS + 1)
                .collect::<Vec<_>>()
                .join(",")
        );
        let url = Url::parse("https://kemono.cr/artists?service=patreon").unwrap();
        let query = query(&url, Kind::Artists, Favorites::Artist).unwrap();
        assert!(parse(input.as_bytes(), &query, &AtomicBool::new(false)).is_err());
        let short = format!(
            "[{}]",
            std::iter::repeat_n("{}", 110_001)
                .collect::<Vec<_>>()
                .join(",")
        );
        assert!(
            parse(short.as_bytes(), &query, &AtomicBool::new(false))
                .unwrap()
                .is_empty()
        );
        assert!(parse(b"[] trailing", &query, &AtomicBool::new(false)).is_err());
    }

    #[test]
    fn in_flight_json_read_cancellation_interrupts_large_single_rows() {
        struct BlockFirst {
            inner: std::io::Cursor<Vec<u8>>,
            started: Option<std::sync::mpsc::Sender<()>>,
            resume: std::sync::mpsc::Receiver<()>,
        }
        impl Read for BlockFirst {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                if let Some(started) = self.started.take() {
                    started.send(()).unwrap();
                    self.resume.recv_timeout(Duration::from_secs(3)).unwrap();
                }
                self.inner.read(buffer)
            }
        }
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let reader = BlockFirst {
            inner: std::io::Cursor::new(
                format!(
                    r#"[{{"service":"patreon","id":"42","favorited":1,"unknown":"{}"}}]"#,
                    "x".repeat(100_000)
                )
                .into_bytes(),
            ),
            started: Some(started_tx),
            resume: resume_rx,
        };
        let flag = cancel.clone();
        let worker = thread::spawn(move || {
            let query = query(
                &Url::parse("https://kemono.cr/artists").unwrap(),
                Kind::Artists,
                Favorites::Artist,
            )
            .unwrap();
            parse_reader(reader, &query, &flag)
        });
        started_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        cancel.store(true, AtomicOrder::Relaxed);
        resume_tx.send(()).unwrap();
        assert!(worker.join().unwrap().is_err());
    }

    fn response(status: &str, body: &[u8]) -> Vec<u8> {
        let mut bytes = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        bytes.extend_from_slice(body);
        bytes
    }
    fn request(stream: &mut std::net::TcpStream) -> String {
        // Accepted sockets inherit nonblocking mode on macOS.
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0; 1024];
        while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            let n = stream.read(&mut buffer).unwrap();
            assert_ne!(n, 0);
            bytes.extend_from_slice(&buffer[..n]);
        }
        String::from_utf8(bytes).unwrap()
    }
    fn serve(replies: Vec<Vec<u8>>) -> (Url, thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let root = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let task = thread::spawn(move || {
            let mut requests = Vec::new();
            for reply in replies {
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(std::time::Instant::now() < deadline);
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("{error}"),
                    }
                };
                requests.push(request(&mut stream));
                let _ = stream.write_all(&reply);
            }
            requests
        });
        (root, task)
    }
    fn extractor(root: Url) -> Kemono {
        Kemono::new(Duration::from_secs(5))
            .unwrap()
            .with_api_root(root)
            .with_metadata(false)
            .with_selection(Selection {
                original: true,
                ..Selection::default()
            })
            .with_network(&NetworkOptions::new(Some(String::new()), None).unwrap())
            .unwrap()
    }
    fn post(id: &str, user: &str) -> Value {
        json!({"id":id,"service":"patreon","user":user,"title":"title","file":{},"attachments":[]})
    }

    #[tokio::test]
    async fn artist_discovery_filters_sorts_and_reuses_creator_event_pipeline() {
        let rows = json!([{"id":"42","service":"patreon","name":"name","favorited":1},{"id":"43","service":"patreon","name":"name","favorited":2}]);
        let (root, server) = serve(vec![
            response("200 OK", rows.to_string().as_bytes()),
            response("200 OK", json!([post("7", "43")]).to_string().as_bytes()),
            response("200 OK", json!([post("8", "42")]).to_string().as_bytes()),
        ]);
        let events: Vec<_> = extractor(root)
            .events(&Url::parse("https://kemono.cr/artists?service=PATREON").unwrap())
            .collect()
            .await;
        assert_eq!(events.len(), 4);
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[0].starts_with("GET /api/v1/creators "));
        assert!(requests[1].starts_with("GET /api/v1/patreon/user/43/posts?o=0 "));
        assert!(requests[2].starts_with("GET /api/v1/patreon/user/42/posts?o=0 "));
    }

    #[tokio::test]
    async fn favorite_post_type_overrides_config_and_queues_sorted_single_posts() {
        let rows = json!([{"id":"7","service":"patreon","user":"42","faved_seq":1},{"id":"8","service":"patreon","user":"42","faved_seq":2}]);
        let (root, server) = serve(vec![
            response("200 OK", rows.to_string().as_bytes()),
            response("200 OK", post("8", "42").to_string().as_bytes()),
            response("200 OK", post("7", "42").to_string().as_bytes()),
        ]);
        let events: Vec<_> = extractor(root)
            .with_favorites(Favorites::Artist)
            .events(&Url::parse("https://kemono.cr/account/favorites?type=post").unwrap())
            .collect()
            .await;
        assert_eq!(events.len(), 4);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/v1/account/favorites?type=post "));
        assert!(requests[1].starts_with("GET /api/v1/patreon/user/42/post/8 "));
        assert!(requests[2].starts_with("GET /api/v1/patreon/user/42/post/7 "));
    }

    #[tokio::test]
    async fn favorite_artist_query_overrides_post_config_and_recurses_discord_server() {
        let rows = json!([{"id":"42","service":"patreon","updated":1},{"id":"100","service":"discord","updated":2}]);
        let server_body = json!({"id":"100","name":"server","channels":[{"id":"200","server_id":"100","post_count":1}]});
        let messages =
            json!([{"id":"400","server":"100","channel":"200","content":"","attachments":[]}]);
        let (root, server) = serve(vec![
            response("200 OK", rows.to_string().as_bytes()),
            response("200 OK", server_body.to_string().as_bytes()),
            response("200 OK", messages.to_string().as_bytes()),
            response("200 OK", json!([post("7", "42")]).to_string().as_bytes()),
        ]);
        let events: Vec<_> = extractor(root)
            .with_favorites(Favorites::Post)
            .events(&Url::parse("https://kemono.cr/favorites?type=artist").unwrap())
            .collect()
            .await;
        assert_eq!(events.len(), 4);
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 4);
        assert!(requests[0].starts_with("GET /api/v1/account/favorites?type=artist "));
        assert!(requests[1].starts_with("GET /api/v1/discord/server/100 "));
        assert!(requests[2].starts_with("GET /api/v1/discord/channel/200?o=0 "));
        assert!(requests[3].starts_with("GET /api/v1/patreon/user/42/posts?o=0 "));
    }

    #[tokio::test]
    async fn raw_post_limit_applies_independently_to_each_queued_creator() {
        let rows = json!([{"id":"42","service":"patreon","favorited":1},{"id":"43","service":"patreon","favorited":1}]);
        let (root, server) = serve(vec![
            response("200 OK", rows.to_string().as_bytes()),
            response(
                "200 OK",
                json!([post("7", "42"), post("8", "42")])
                    .to_string()
                    .as_bytes(),
            ),
            response(
                "200 OK",
                json!([post("9", "43"), post("10", "43")])
                    .to_string()
                    .as_bytes(),
            ),
        ]);
        let events: Vec<_> = extractor(root)
            .with_max_posts(Some(1))
            .events(&Url::parse("https://kemono.cr/artists").unwrap())
            .collect()
            .await;
        let ids: Vec<_> = events
            .iter()
            .filter_map(|event| match event.as_ref().unwrap() {
                ExtractionEvent::Post(post) => {
                    Some(post.metadata.as_ref().unwrap().post_id.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(ids, ["7", "9"]);
        assert_eq!(server.join().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn discovery_child_errors_resume_and_invalid_later_identity_waits_for_poll() {
        let rows = json!([{"id":"42","service":"patreon","favorited":3},{"id":"private/invalid","service":"patreon","favorited":2},{"id":"43","service":"patreon","favorited":1}]);
        let (root, server) = serve(vec![
            response("200 OK", rows.to_string().as_bytes()),
            response("404 Not Found", b"{}"),
            response("200 OK", json!([post("7", "43")]).to_string().as_bytes()),
        ]);
        let events: Vec<_> = extractor(root)
            .events(&Url::parse("https://kemono.cr/artists").unwrap())
            .collect()
            .await;
        assert_eq!(events.len(), 4);
        assert!(events[0].is_err());
        assert!(events[1].is_err());
        assert!(
            !events[1]
                .as_ref()
                .unwrap_err()
                .to_string()
                .contains("private")
        );
        assert!(matches!(events[2], Ok(ExtractionEvent::Post(_))));
        assert_eq!(server.join().unwrap().len(), 3);
        let (root, server) = serve(vec![
            response("200 OK", rows.to_string().as_bytes()),
            response("200 OK", json!([post("7", "42")]).to_string().as_bytes()),
        ]);
        let client = extractor(root);
        let input = Url::parse("https://kemono.cr/artists").unwrap();
        let mut events = client.events(&input);
        assert!(matches!(
            events.next().await.unwrap().unwrap(),
            ExtractionEvent::Post(_)
        ));
        drop(events);
        assert_eq!(server.join().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn dropping_nested_child_request_closes_socket_and_prevents_later_creator() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let root = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let server = thread::spawn(move || {
            let (mut first, _) = listener.accept().unwrap();
            let a = request(&mut first);
            let rows = json!([{"id":"42","service":"patreon","favorited":2},{"id":"43","service":"patreon","favorited":1}]);
            first
                .write_all(&response("200 OK", rows.to_string().as_bytes()))
                .unwrap();
            let (mut child, _) = listener.accept().unwrap();
            let b = request(&mut child);
            tx.send(()).unwrap();
            let mut byte = [0];
            match child.read(&mut byte) {
                Ok(0) => {}
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
                other => panic!("nested request not cancelled: {other:?}"),
            };
            vec![a, b]
        });
        let client = Arc::new(extractor(root));
        let task = tokio::spawn({
            let client = client.clone();
            async move {
                let input = Url::parse("https://kemono.cr/artists").unwrap();
                let mut events = client.events(&input);
                events.next().await
            }
        });
        tokio::task::spawn_blocking(move || rx.recv_timeout(Duration::from_secs(3)).unwrap())
            .await
            .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].contains("/patreon/user/42/posts"));
    }
}
