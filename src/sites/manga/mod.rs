//! Manga, comic and webtoon readers.
//!
//! [`Gallery`] mirrors `GalleryExtractor.items` (and so `ChapterExtractor`):
//! chapter metadata and its page list become one post boundary whose files
//! are numbered under `page` (or `num`), honouring a preset `count`.
//! [`queue_chapters`] mirrors `MangaExtractor.items`, queueing each chapter
//! lazily, newest-last by default (`reverse = True`).
use crate::event::ExtractionEvent;
use crate::sites::{Events, Http, Item, Route, Site};
use futures_util::StreamExt;
use serde_json::{Map, Value};
use url::Url;

mod dandadan;
mod dankefuerslesen;
mod dynastyscans;
mod foolslide;
mod hentai2read;
mod hentaihere;
mod kaliscan;
mod komikcast;
mod madokami;
mod mangadex;
mod mangafox;
mod mangafreak;
mod mangahere;
mod mangaread;
mod mangareader;
mod mangatown;
mod naverwebtoon;
mod rawkuma;
mod senmanga;
mod tcbscans;
mod text;
mod tmohentai;
pub(crate) mod webtoons;

pub(super) static SITES: &[&Site] = &[
    &mangadex::SITE,
    &webtoons::SITE,
    &tcbscans::SITE,
    &rawkuma::SITE,
    &mangatown::SITE,
    &kaliscan::SITE,
    &mangahere::SITE,
    &mangafox::SITE,
    &mangafreak::SITE,
    &dynastyscans::SITE,
    &hentai2read::SITE,
    &mangareader::SITE,
    &senmanga::SITE,
    &tmohentai::SITE,
    &dandadan::SITE,
    &hentaihere::SITE,
    &mangaread::SITE,
    &komikcast::SITE,
    &madokami::SITE,
    &naverwebtoon::SITE,
    &dankefuerslesen::SITE,
    &foolslide::SITE,
];

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

    /// A `GalleryExtractor` whose files are numbered under `num`.
    pub fn numbered(route: &'static Route, data: Map<String, Value>, pages: Vec<Page>) -> Self {
        Self {
            enum_key: "num",
            ..Self::chapter(route, data, pages)
        }
    }

    pub fn uncounted(mut self) -> Self {
        self.uncounted = true;
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

/// Upstream `ChapterExtractor` default formats.
pub(super) const CHAPTER_FILENAME: &str =
    "{manga}_c{chapter:>03}{chapter_minor:?//}_{page:>03}.{extension}";
pub(super) const CHAPTER_DIRECTORY: &[&str] = &[
    "{category}",
    "{manga}",
    "{volume:?v/ />02}c{chapter:>03}{chapter_minor:?//}{title:?: //}",
];
pub(super) const CHAPTER_ARCHIVE: &str = "{manga}_{chapter}{chapter_minor}_{page}";

/// An upstream `ChapterExtractor` subclass with default formats.
pub(super) const fn chapter_route() -> Route {
    Route::new(
        "chapter",
        CHAPTER_FILENAME,
        CHAPTER_DIRECTORY,
        CHAPTER_ARCHIVE,
    )
}

/// Upstream `GalleryExtractor` default formats.
pub(super) const GALLERY_FILENAME: &str = "{category}_{gallery_id}_{num:>03}.{extension}";
pub(super) const GALLERY_ARCHIVE: &str = "{gallery_id}_{num}";

/// An upstream `MangaExtractor` subclass with default formats; it only
/// queues chapters.
pub(super) const fn manga_route() -> Route {
    Route::new("manga", "{filename}.{extension}", &["{category}"], "")
}

/// `MangaExtractor.items`: queue chapters, reversed unless `reverse` is
/// false, with the `chapter-reverse` option flipping the order.
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

/// Fetch a page as text through the site's (test-rewritable) origin.
pub(super) async fn fetch(http: &Http, url: &str) -> crate::Result<String> {
    http.text(&http.api(url)?).await
}

/// The mobile readers of mangahere/fanfox: each `N.html` page shows two
/// images, so pages `1, 3, 5, ...` are fetched until `count` URLs are known.
pub(super) async fn paired_pages(
    http: &Http,
    base: &str,
    mut page: String,
    count: usize,
    headers: &[(&str, &str)],
) -> crate::Result<Vec<Page>> {
    let mut pages = Vec::new();
    let mut number = 1;
    while pages.len() < count {
        let mut scan = crate::sites::gdl::Scan::new(&page);
        for begin in ["<img src=\"", " src=\""] {
            if pages.len() < count {
                let url = crate::sites::gdl::unescape(scan.extr(begin, "\""));
                pages.push((text::ensure_http_scheme(&url), None));
            }
        }
        if pages.len() >= count {
            break;
        }
        number += 2;
        page = http
            .text_with(&http.api(&format!("{base}{number}.html"))?, headers)
            .await?;
    }
    Ok(pages)
}

/// The `(chapter, chapter_minor)` keyword pair of `text.partition(".")`.
pub(super) fn chapter_keys(data: &mut Map<String, Value>, number: &str) {
    let (chapter, minor) = text::chapter(number);
    set(data, "chapter", chapter);
    set(data, "chapter_minor", minor);
}

/// `lang`/`language` keywords for English sites.
pub(super) fn english(data: &mut Map<String, Value>) {
    set(data, "lang", "en");
    set(data, "language", "English");
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

#[cfg(test)]
pub(super) mod tests {

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
}
