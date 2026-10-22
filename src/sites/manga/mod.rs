//! Manga, comic and webtoon readers.
//!
//! [`Gallery`] mirrors `GalleryExtractor.items` (and so `ChapterExtractor`):
//! chapter metadata and its page list become one post boundary whose files
//! are numbered under `page` (or `num`), honouring a preset `count`.
//! [`queue_chapters`] mirrors `MangaExtractor.items`, queueing each chapter
//! lazily, newest-last by default (`reverse = True`).
use crate::event::ExtractionEvent;
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use futures_util::StreamExt;
use serde_json::{Map, Value};
use url::Url;

mod mangadex;
mod text;

pub(super) static SITES: &[&Site] = &[&mangadex::SITE];

/// `ChapterExtractor.filename_fmt`.
pub(super) const CHAPTER_FILENAME: &str =
    "{manga}_c{chapter:>03}{chapter_minor:?//}_{page:>03}.{extension}";
/// `ChapterExtractor.directory_fmt`.
pub(super) const CHAPTER_DIRECTORY: &[&str] = &[
    "{category}",
    "{manga}",
    "{volume:?v/ />02}c{chapter:>03}{chapter_minor:?//}{title:?: //}",
];
/// `ChapterExtractor.archive_fmt`.
pub(super) const CHAPTER_ARCHIVE: &str = "{manga}_{chapter}{chapter_minor}_{page}";
/// `Extractor.filename_fmt`, inherited by `MangaExtractor`.
pub(super) const FILENAME: &str = "{filename}.{extension}";
/// `Extractor.directory_fmt`, inherited by `MangaExtractor`.
pub(super) const DIRECTORY: &[&str] = &["{category}"];

/// A `ChapterExtractor` subclass with the inherited formats.
pub(super) const fn chapter_route() -> Route {
    Route::new(
        "chapter",
        CHAPTER_FILENAME,
        CHAPTER_DIRECTORY,
        CHAPTER_ARCHIVE,
    )
}

/// A `MangaExtractor` subclass with the inherited formats.
pub(super) const fn manga_route() -> Route {
    Route::new("manga", FILENAME, DIRECTORY, "")
}

/// `util.USERAGENT_FIREFOX`, whose version follows the current date.
pub(super) fn firefox_agent() -> &'static str {
    static AGENT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    AGENT.get_or_init(|| {
        // Proleptic Gregorian ordinal of today, as Python's date.toordinal().
        let days = time::OffsetDateTime::now_utc()
            .unix_timestamp()
            .div_euclid(86_400);
        let version = (days + 719_163 - 735_513) / 28;
        format!(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:{version}.0) \
             Gecko/20100101 Firefox/{version}.0"
        )
    })
}

/// Default upstream session headers: the Firefox user agent unless the user
/// configured one, and `Referer: ROOT/` (`Extractor.referer = True`).
fn session_headers<'h>(
    http: &Http,
    root: &str,
    extra: &[(&'h str, &'h str)],
) -> Vec<(&'h str, String)> {
    let has = |name: &str| extra.iter().any(|(k, _)| k.eq_ignore_ascii_case(name));
    let mut headers = Vec::new();
    if http.network.user_agent().is_none() && !has("User-Agent") {
        headers.push(("User-Agent", firefox_agent().to_owned()));
    }
    if !root.is_empty() && !has("Referer") {
        headers.push(("Referer", format!("{root}/")));
    }
    headers.extend(extra.iter().map(|(k, v)| (*k, (*v).to_owned())));
    headers
}

/// GET a page as text with upstream session headers. The URL is routed
/// through [`Http::api`].
pub(super) async fn get_text(
    http: &Http,
    url: &str,
    root: &str,
    extra: &[(&str, &str)],
) -> Result<String> {
    let url = http.api(url)?;
    let headers = session_headers(http, root, extra);
    let headers: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    http.text_with(&url, &headers).await
}

/// GET JSON with upstream session headers.
pub(super) async fn get_json(
    http: &Http,
    url: &str,
    root: &str,
    extra: &[(&str, &str)],
) -> Result<Value> {
    let url = http.api(url)?;
    let headers = session_headers(http, root, extra);
    let headers: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    http.json_with(&url, &headers).await
}

/// POST JSON with upstream session headers.
pub(super) async fn post_json(
    http: &Http,
    url: &str,
    root: &str,
    body: &Value,
    extra: &[(&str, &str)],
) -> Result<Value> {
    let url = http.api(url)?;
    let headers = session_headers(http, root, extra);
    let headers: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    http.post_json(&url, body, &headers).await
}

/// Append query parameters to an absolute URL string.
pub(super) fn with_query(base: &str, params: &[(&str, &str)]) -> Result<String> {
    let mut url = Url::parse(base).map_err(|_| error("invalid site URL"))?;
    if !params.is_empty() {
        url.query_pairs_mut().extend_pairs(params);
    }
    Ok(url.into())
}

/// Insert a key into a keyword map.
pub(super) fn set(map: &mut Map<String, Value>, key: &str, value: impl Into<Value>) {
    map.insert(key.to_owned(), value.into());
}

/// An object keyword map from `(key, value)` pairs.
pub(super) fn keywords<const N: usize>(pairs: [(&str, Value); N]) -> Map<String, Value> {
    pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect()
}

/// A JSON value as text when it is a string, else `""`.
pub(super) fn str_of<'v>(value: &'v Value, key: &str) -> &'v str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// A JSON value at `key`, or null.
pub(super) fn get(value: &Value, key: &str) -> Value {
    value.get(key).cloned().unwrap_or(Value::Null)
}

/// The site HTTP option `page-reverse` (upstream default `false`).
fn page_reverse(http: &Http) -> bool {
    http.option_bool("page-reverse", false)
}

/// One page of a gallery: its URL and optional per-image keywords.
pub(super) type Page = (String, Option<Map<String, Value>>);

/// `GalleryExtractor.items`: one post boundary of numbered pages.
pub(super) struct Gallery {
    pub route: &'static Route,
    pub data: Map<String, Value>,
    pub pages: Vec<Page>,
    /// `GalleryExtractor.enum`: `page` for chapters, `num` for galleries.
    pub enum_key: &'static str,
    /// Post identity for boundaries and diagnostics.
    pub post_id: String,
    pub timestamp: Option<i64>,
    /// Headers required by image requests (e.g. Referer).
    pub headers: Vec<(&'static str, String)>,
    /// Enumerate every page and leave `count` alone, for extractors with a
    /// custom `items()` that does not zip pages with `count`.
    pub uncounted: bool,
}

impl Gallery {
    pub fn chapter(route: &'static Route, data: Map<String, Value>, pages: Vec<Page>) -> Self {
        Self {
            route,
            data,
            pages,
            enum_key: "page",
            post_id: String::new(),
            timestamp: None,
            headers: Vec::new(),
            uncounted: false,
        }
    }

    pub fn uncounted(mut self) -> Self {
        self.uncounted = true;
        self
    }

    pub fn enum_key(mut self, key: &'static str) -> Self {
        self.enum_key = key;
        self
    }

    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.post_id = id.into();
        self
    }

    pub fn timestamp(mut self, timestamp: Option<i64>) -> Self {
        self.timestamp = timestamp;
        self
    }

    pub fn header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }

    /// Image headers matching upstream session defaults for `root`.
    pub fn session(mut self, http: &Http, root: &str) -> Self {
        for (name, value) in session_headers(http, root, &[]) {
            self.headers.push((name, value));
        }
        self
    }

    /// The post boundary and its files. Pages with unusable URLs keep their
    /// number but are skipped.
    pub fn events(self, site: &Site, http: &Http) -> Vec<ExtractionEvent> {
        let mut data = self.data;
        let count = match data.get("count") {
            _ if self.uncounted => self.pages.len(),
            Some(value) => usize::try_from(value.as_i64().unwrap_or(0).max(0)).unwrap_or(0),
            None => {
                set(&mut data, "count", self.pages.len());
                self.pages.len()
            }
        };
        let mut numbered: Vec<(usize, Page)> = self
            .pages
            .into_iter()
            .take(count)
            .enumerate()
            .map(|(i, page)| (i + 1, page))
            .collect();
        if page_reverse(http) {
            // util.enumerate_reversed: numbers count down from `count`.
            let total = numbered.len();
            numbered = numbered
                .into_iter()
                .rev()
                .enumerate()
                .map(|(i, (_, page))| (total - i, page))
                .collect();
        }
        let mut items = Vec::new();
        for (number, (raw, image)) in numbered {
            set(&mut data, self.enum_key, number);
            let url = Url::parse(raw.trim()).ok().filter(crate::sites::plain);
            let needs_name = match image {
                Some(image) => {
                    let has_extension = image.contains_key("extension");
                    data.extend(image);
                    !has_extension
                }
                None => true,
            };
            let Some(url) = url else { continue };
            if needs_name {
                let (name, ext) = crate::sites::nameext(&url);
                set(&mut data, "filename", name);
                set(&mut data, "extension", ext);
            }
            let mut item = Item::new(url, data.clone())
                .ids(self.post_id.clone(), number.to_string())
                .timestamp(self.timestamp);
            for (name, value) in &self.headers {
                item = item.header(name, value);
            }
            items.push(item);
        }
        crate::sites::post(site, http, self.route, items)
    }
}

/// `MangaExtractor.items`: queue chapters, reversed when `reverse` (the
/// upstream default for sites listing newest first) unless the
/// `chapter-reverse` option flips it. URLs another site would handle are
/// dropped.
pub(super) fn queue_chapters<'a>(
    http: &'a Http,
    site: &'static Site,
    route: &'static Route,
    mut chapters: Vec<(Url, Map<String, Value>)>,
    reverse: bool,
) -> Events<'a> {
    if reverse != http.option_bool("chapter-reverse", false) {
        chapters.reverse();
    }
    queue_urls(http, site, route, chapters)
}

/// Queue child URLs of this site, in order.
pub(super) fn queue_urls<'a>(
    http: &'a Http,
    site: &'static Site,
    route: &'static Route,
    children: Vec<(Url, Map<String, Value>)>,
) -> Events<'a> {
    futures_util::stream::iter(
        children
            .into_iter()
            .filter(|(url, _)| (site.matches)(url))
            .map(move |(url, data)| {
                crate::sites::queue(http, site.category, route.subcategory, url, &data)
            }),
    )
    .flatten()
    .boxed()
}

/// Queue one child whose events come from `build` instead of URL dispatch,
/// mirroring upstream caches that hand queued data to the child extractor.
pub(super) fn queue_with<'a>(
    site: &'static Site,
    route: &'static Route,
    data: &Map<String, Value>,
    build: impl FnOnce() -> Events<'a> + Send + 'a,
) -> Events<'a> {
    crate::event::child_job(
        crate::event::ChildMetadata::from_raw(site.category, route.subcategory, data),
        build,
    )
}

/// A list-valued option: a list of strings, or a string split on `,`
/// (upstream only splits strings containing a comma).
pub(super) fn option_list(http: &Http, key: &str) -> Option<Vec<String>> {
    match http.option(key)? {
        Value::String(s) if s.contains(',') => Some(s.split(',').map(str::to_owned).collect()),
        Value::String(s) => Some(vec![s.clone()]),
        Value::Array(items) => Some(items.iter().map(crate::sites::scalar).collect()),
        Value::Null => None,
        other => Some(vec![crate::sites::scalar(other)]),
    }
}

/// Parse an absolute or root-relative link on `root`.
pub(super) fn link(root: &str, href: &str) -> Option<Url> {
    let href = crate::sites::unescape(href.trim());
    let url = if href.starts_with('/') && !href.starts_with("//") {
        Url::parse(&format!("{root}{href}"))
    } else if let Some(rest) = href.strip_prefix("//") {
        Url::parse(&format!("https://{rest}"))
    } else {
        Url::parse(&href)
    };
    url.ok().filter(crate::sites::plain)
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use serde_json::json;

    /// Render a file's default path as `directory/filename`.
    pub fn path(media: &crate::extract::Media) -> String {
        let (file, dir) = crate::sites::default_templates(media).unwrap();
        let dir = dir.render(media).unwrap().unwrap();
        format!(
            "{}/{}",
            dir.to_string_lossy().replace('\\', "/"),
            file.render(media).unwrap()
        )
    }

    pub fn archive(media: &crate::extract::Media) -> String {
        crate::sites::archive_key(media).unwrap().1
    }

    static TEST_SITE: Site = Site {
        category: "mangatest",
        name: "test",
        matches: |_| false,
        extract: |_, _| crate::sites::fail(error("unused")),
        routes: &[&TEST_CHAPTER],
        api_key: false,
    };
    static TEST_CHAPTER: Route = chapter_route();

    #[test]
    fn gallery_numbers_pages_and_honours_count() {
        let http = crate::sites::testing::http("test", Url::parse("http://127.0.0.1:1/").unwrap());
        let data = keywords([("manga", "M".into()), ("count", 2.into())]);
        let pages = vec![
            ("https://cdn.test/a/01.JPG".into(), None),
            ("not a url".into(), None),
            ("https://cdn.test/a/03.png".into(), None),
        ];
        let events = Gallery::chapter(&TEST_CHAPTER, data, pages)
            .id("c1")
            .header("Referer", "https://site.test/")
            .events(&TEST_SITE, &http);
        let media: Vec<_> = events
            .into_iter()
            .filter_map(|e| match e {
                ExtractionEvent::Media(m) => Some(*m),
                _ => None,
            })
            .collect();
        assert_eq!(media.len(), 1);
        let k = &media[0]
            .metadata
            .as_ref()
            .unwrap()
            .site_keywords
            .as_ref()
            .unwrap()
            .fields;
        assert_eq!(k["page"], 1);
        assert_eq!(k["count"], 2);
        assert_eq!(k["extension"], "jpg");
        assert_eq!(media[0].headers["Referer"], "https://site.test/");
        let data = keywords([("manga", "M".into())]);
        let pages = vec![
            ("https://cdn.test/1.jpg".into(), Some(Map::new())),
            (
                "https://cdn.test/2".into(),
                Some(
                    json!({"extension": "webp", "filename": "two"})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            ),
        ];
        let events = Gallery::chapter(&TEST_CHAPTER, data, pages).events(&TEST_SITE, &http);
        let names: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                ExtractionEvent::Media(m) => Some(m.filename.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(names, ["1.jpg", "two.webp"]);
    }

    #[test]
    fn links_resolve_on_root() {
        assert_eq!(
            link("https://a.test", "/x?a=1&amp;b=2").unwrap().as_str(),
            "https://a.test/x?a=1&b=2"
        );
        assert_eq!(
            link("https://a.test", "//b.test/y").unwrap().as_str(),
            "https://b.test/y"
        );
        assert!(link("https://a.test", "javascript:x").is_none());
    }
}
