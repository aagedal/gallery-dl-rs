//! Registry of experimental native site extractors that share one HTTP layer,
//! gallery-dl keyword dictionaries and default path templates.
//!
//! Each site mirrors the matching module in gallery_dl/extractor. Emitted
//! files carry the upstream keyword names in [`PostMetadata::site_keywords`],
//! so filters, filename/directory templates and postprocessors see the same
//! fields as gallery-dl. Default paths render the upstream `filename_fmt` and
//! `directory_fmt`; archive identities use the upstream `archive_fmt` fields.
pub mod http;

mod adult;
mod adult_video;
mod art;
mod boorus;
mod filehosts;
mod imageboards;
mod manga;
mod media;
mod social;
mod video;

pub use http::Http;

use crate::{
    Result, error,
    event::{ChildMetadata, ExtractionEvent, KeywordPolicy, child_job, group_with_keywords},
    extract::{Media, MediaJob, PostMetadata},
};
use futures_util::{
    Stream, StreamExt, TryStreamExt,
    stream::{self, BoxStream},
};
use serde_json::{Map, Value};
use std::{collections::BTreeMap, sync::Arc};
use url::Url;

pub type Events<'a> = BoxStream<'a, Result<ExtractionEvent>>;

/// One upstream extractor class: its subcategory and default formats.
pub struct Route {
    pub subcategory: &'static str,
    /// Upstream `filename_fmt`.
    pub filename: &'static str,
    /// Upstream `directory_fmt`.
    pub directory: &'static [&'static str],
    /// Upstream `archive_fmt`.
    pub archive: &'static str,
    templates: std::sync::OnceLock<(
        crate::naming::FilenameTemplate,
        crate::naming::DirectoryTemplate,
    )>,
}

impl Route {
    pub const fn new(
        subcategory: &'static str,
        filename: &'static str,
        directory: &'static [&'static str],
        archive: &'static str,
    ) -> Self {
        Self {
            subcategory,
            filename,
            directory,
            archive,
            templates: std::sync::OnceLock::new(),
        }
    }

    /// Parsed default filename and directory templates.
    pub(crate) fn templates(
        &self,
    ) -> &(
        crate::naming::FilenameTemplate,
        crate::naming::DirectoryTemplate,
    ) {
        self.templates.get_or_init(|| {
            (
                crate::naming::FilenameTemplate::parse_source(self.filename)
                    .expect("static site filename template"),
                crate::naming::DirectoryTemplate::parse_source(
                    &self
                        .directory
                        .iter()
                        .map(|s| (*s).to_owned())
                        .collect::<Vec<_>>(),
                )
                .expect("static site directory template"),
            )
        })
    }
}

impl std::fmt::Debug for Route {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Route")
            .field("subcategory", &self.subcategory)
            .finish_non_exhaustive()
    }
}

/// Category used for compatibility runs of URLs outside every native
/// category; files then carry the upstream extractor's own category.
pub const GENERIC_CATEGORY: &str = "gallery-dl";

/// Defaults for generic compatibility output, whose upstream formats are
/// not reported by gallery-dl.
pub static GENERIC: Route = Route::new("", "{filename}.{extension}", &["{category}"], "");

/// A file's upstream keyword dictionary and the class that produced it.
#[derive(Clone, Debug)]
pub struct SiteKeywords {
    pub route: &'static Route,
    pub fields: Map<String, Value>,
}

impl serde::Serialize for SiteKeywords {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        self.fields.serialize(serializer)
    }
}

/// One supported site category.
pub struct Site {
    /// gallery-dl category name, also the configuration scope.
    pub category: &'static str,
    /// Human-readable name used in diagnostics.
    pub name: &'static str,
    /// Whether a URL belongs to one of this site's extractors.
    pub matches: fn(&Url) -> bool,
    /// Stream post boundaries, files and child jobs for a matching URL.
    pub extract: for<'a> fn(&'a Http, Url) -> Events<'a>,
    /// Upstream extractor classes; the first is the default for files
    /// whose class is unknown (compatibility output without a subcategory).
    pub routes: &'static [&'static Route],
    /// Whether the `api-key` setting applies.
    pub api_key: bool,
    /// Domain whose `--cookies`/`--cookies-from-browser` entries are loaded
    /// into the site's HTTP state; `None` for sites that never use cookies.
    pub cookies: Option<&'static str>,
}

/// Every registry site, in matching order.
pub fn sites() -> &'static [&'static Site] {
    static ALL: std::sync::OnceLock<Vec<&'static Site>> = std::sync::OnceLock::new();
    ALL.get_or_init(|| {
        [
            imageboards::SITES,
            boorus::SITES,
            media::SITES,
            filehosts::SITES,
            manga::SITES,
            art::SITES,
            social::SITES,
            adult::SITES,
            adult_video::SITES,
            video::SITES,
        ]
        .concat()
    })
}

/// Lazily initialized per-site HTTP state for one CLI invocation.
pub struct Runtime {
    cells: Vec<tokio::sync::OnceCell<Result<Http>>>,
}

impl Default for Runtime {
    fn default() -> Self {
        Self {
            cells: sites()
                .iter()
                .map(|_| tokio::sync::OnceCell::new())
                .collect(),
        }
    }
}

impl Runtime {
    /// Whether the site's HTTP state was already created.
    pub fn initialized(&self, site: &Site) -> bool {
        sites()
            .iter()
            .position(|candidate| candidate.category == site.category)
            .is_some_and(|index| self.cells[index].initialized())
    }

    /// The site's HTTP state, created by `init` on first use. An
    /// initialization failure is retained for later inputs of the site.
    pub async fn get(&self, site: &Site, init: impl FnOnce() -> Result<Http>) -> Result<&Http> {
        let index = sites()
            .iter()
            .position(|candidate| candidate.category == site.category)
            .ok_or_else(|| error("unknown registry site"))?;
        self.cells[index]
            .get_or_init(|| async { init() })
            .await
            .as_ref()
            .map_err(|e| error(e.to_string()))
    }
}

/// The registry site handling a URL, if any.
pub fn find(url: &Url) -> Option<&'static Site> {
    sites().iter().copied().find(|site| (site.matches)(url))
}

/// The registry site for a category name.
pub fn site(category: &str) -> Option<&'static Site> {
    sites()
        .iter()
        .copied()
        .find(|site| site.category == category)
}

/// Categories gallery-dl has no extractor for, which therefore use the
/// native extractor in compatibility mode too.
pub fn native_only(category: &str) -> bool {
    matches!(
        category,
        "ytdl"
            | "telegram"
            | "snapchat"
            | "kuaishou"
            | "xiaohongshu"
            | "threads"
            | "youtube"
            | "shortlink"
            | "linkedin"
    )
}

/// Cookie domains a site needs besides its own: short links are extracted
/// by their target sites with the short-link site's session, and some
/// sites serve the same extractors from a second domain.
pub fn extra_cookie_domains(site: &Site) -> &'static [&'static str] {
    match site.category {
        "shortlink" => social::SHORTLINK_COOKIE_DOMAINS,
        "cien" => &["ci-en.dlsite.com"],
        "subscribestar" => &["subscribestar.adult"],
        "vipergirls" => adult_video::VIPERGIRLS_COOKIE_DOMAINS,
        _ => &[],
    }
}

pub fn is_category(category: &str) -> bool {
    site(category).is_some()
}

pub fn categories() -> impl Iterator<Item = &'static str> {
    sites().iter().map(|site| site.category)
}

/// Extract any registry URL, for child jobs that cross site boundaries.
pub fn extract<'a>(http: &'a Http, url: Url) -> Events<'a> {
    match find(&url) {
        Some(site) => (site.extract)(http, url),
        None => fail(error("unsupported child URL")),
    }
}

pub(crate) fn fail<'a>(error: crate::Error) -> Events<'a> {
    stream::once(async move { Err(error) }).boxed()
}

/// Run one async step that produces a complete event stream.
pub(crate) fn once<'a, F>(future: F) -> Events<'a>
where
    F: std::future::Future<Output = Result<Events<'a>>> + Send + 'a,
{
    stream::once(future)
        .map(|result| result.unwrap_or_else(fail))
        .flatten()
        .boxed()
}

/// Flatten lazily produced event pages; later pages are requested only as
/// consumers drain earlier ones. An error ends the stream.
pub(crate) fn pages<'a, S>(input: S) -> Events<'a>
where
    S: Stream<Item = Result<Events<'a>>> + Send + 'a,
{
    input
        .map(|result| result.unwrap_or_else(fail))
        .flatten()
        .boxed()
}

/// Paginate with a state machine: `next` returns the events of one page and
/// the following state, or `None` when done.
pub(crate) fn paginate<'a, T, F, Fut>(state: T, next: F) -> Events<'a>
where
    T: Send + 'a,
    F: FnMut(T) -> Fut + Send + 'a,
    Fut: std::future::Future<Output = Result<Option<(Events<'a>, T)>>> + Send + 'a,
{
    pages(stream::try_unfold(state, next).into_stream())
}

/// Queue a child URL, extracted only once the consumer reaches it.
pub(crate) fn queue<'a>(
    http: &'a Http,
    category: &str,
    subcategory: &str,
    url: Url,
    data: &Map<String, Value>,
) -> Events<'a> {
    child_job(
        ChildMetadata::from_raw(category, subcategory, data),
        move || extract(http, url),
    )
}

/// One file with its upstream keyword dictionary.
pub(crate) struct Item {
    pub url: Url,
    pub keywords: Map<String, Value>,
    pub post_id: String,
    pub media_id: String,
    pub username: Option<String>,
    pub caption: Option<String>,
    pub timestamp: Option<i64>,
    pub headers: BTreeMap<String, String>,
    /// How the file is fetched; direct HTTP unless set.
    pub job: MediaJob,
}

impl Item {
    pub fn new(url: Url, keywords: Map<String, Value>) -> Self {
        Self {
            url,
            keywords,
            post_id: String::new(),
            media_id: String::new(),
            username: None,
            caption: None,
            timestamp: None,
            headers: BTreeMap::new(),
            job: MediaJob::DirectHttp,
        }
    }
    pub fn ids(mut self, post_id: impl Into<String>, media_id: impl Into<String>) -> Self {
        self.post_id = post_id.into();
        self.media_id = media_id.into();
        self
    }
    pub fn user(mut self, username: Option<String>) -> Self {
        self.username = username.filter(|s| !s.is_empty());
        self
    }
    pub fn caption(mut self, caption: Option<String>) -> Self {
        self.caption = caption.filter(|s| !s.is_empty());
        self
    }
    pub fn timestamp(mut self, timestamp: Option<i64>) -> Self {
        self.timestamp = timestamp;
        self
    }
    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.insert(name.into(), value.into());
        self
    }
    pub fn job(mut self, job: MediaJob) -> Self {
        self.job = job;
        self
    }
}

/// Group files into one post boundary, numbering them from one.
pub(crate) fn post(
    site: &Site,
    http: &Http,
    route: &'static Route,
    items: Vec<Item>,
) -> Vec<ExtractionEvent> {
    let media: Vec<Media> = items
        .into_iter()
        .enumerate()
        .map(|(index, item)| media(site, http, route, item, index + 1))
        .collect();
    if media.is_empty() {
        return Vec::new();
    }
    group_with_keywords(media, site.category, KeywordPolicy::default()).into()
}

pub(crate) fn events<'a>(events: Vec<ExtractionEvent>) -> Events<'a> {
    stream::iter(events.into_iter().map(Ok)).boxed()
}

fn media(site: &Site, http: &Http, route: &'static Route, item: Item, index: usize) -> Media {
    let mut keywords = item.keywords;
    if !keywords.contains_key("filename") || !keywords.contains_key("extension") {
        let (name, ext) = nameext(&item.url);
        keywords.entry("filename").or_insert(name.into());
        keywords.entry("extension").or_insert(ext.into());
    }
    keywords.insert("category".into(), site.category.into());
    keywords.insert("subcategory".into(), route.subcategory.into());
    let stem = keywords.get("filename").map(scalar).unwrap_or_default();
    let extension = keywords.get("extension").map(scalar).unwrap_or_default();
    let filename = if extension.is_empty() {
        stem
    } else {
        format!("{stem}.{extension}")
    };
    let mut media = Media {
        url: item.url,
        job: item.job,
        category: site.category.into(),
        filename,
        metadata: Some(PostMetadata {
            post_id: item.post_id,
            media_id: item.media_id,
            username: item.username.map(Arc::from),
            caption: item.caption.map(Arc::from),
            timestamp: item.timestamp,
            index,
            site_keywords: Some(Arc::new(SiteKeywords {
                route,
                fields: keywords,
            })),
            ..PostMetadata::default()
        }),
        headers: item.headers,
        handoff: BTreeMap::new(),
    };
    http.network.apply_media(&mut media);
    media
}

/// A JSON scalar as text, matching Python `str()` for strings and numbers.
pub(crate) fn scalar(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Null => "None".into(),
        other => other.to_string(),
    }
}

/// gallery-dl `text.nameext_from_url`: percent-decoded basename and
/// lowercase extension of at most 16 characters.
pub(crate) fn nameext(url: &Url) -> (String, String) {
    let raw = url.path().rsplit('/').next().unwrap_or_default();
    let name = percent_decode(raw);
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && ext.chars().count() <= 16 => {
            (stem.to_owned(), ext.to_lowercase())
        }
        _ => (name, String::new()),
    }
}

pub(crate) fn percent_decode(text: &str) -> String {
    url::form_urlencoded::parse(
        format!("x={}", text.replace('+', "%2B").replace('&', "%26")).as_bytes(),
    )
    .next()
    .map(|(_, value)| value.into_owned())
    .unwrap_or_default()
}

/// Python `html.unescape`.
pub(crate) fn unescape(text: &str) -> String {
    crate::pixiv::pixivision::unescape(text)
}

/// gallery-dl `text.remove_html`: replace tags with spaces and collapse
/// whitespace. Entities are left encoded.
pub(crate) fn remove_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        match rest[start + 1..].find('>') {
            Some(end) if end > 0 => {
                out.push_str(&rest[..start]);
                out.push(' ');
                rest = &rest[start + end + 2..];
            }
            _ => {
                out.push_str(&rest[..=start]);
                rest = &rest[start + 1..];
            }
        }
    }
    out.push_str(rest);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// gallery-dl `text.extr`: the text between `begin` and the next `end`.
pub(crate) fn extr<'t>(text: &'t str, begin: &str, end: &str) -> Option<&'t str> {
    let start = text.find(begin)? + begin.len();
    let stop = text[start..].find(end)?;
    Some(&text[start..start + stop])
}

/// Path segments without empty components.
pub(crate) fn segments(url: &Url) -> Vec<String> {
    url.path_segments()
        .map(|s| s.filter(|s| !s.is_empty()).map(percent_decode).collect())
        .unwrap_or_default()
}

/// Whether an URL has an HTTP(S) scheme, default port and no credentials.
pub(crate) fn plain(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
}

/// Host equal to `domain` or one of `prefixes` followed by `.domain`.
pub(crate) fn host_is(url: &Url, domains: &[&str]) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.strip_prefix("www.").unwrap_or(host);
    domains.contains(&host)
}

/// A media URL that must be HTTPS (or HTTP) on an expected host.
pub(crate) fn media_url(raw: &str, hosts: &[&str]) -> Option<Url> {
    let url = Url::parse(raw.trim()).ok()?;
    let host = url.host_str()?;
    (plain(&url)
        && hosts
            .iter()
            .any(|h| host == *h || host.ends_with(&format!(".{h}"))))
    .then_some(url)
}

/// Archive identity for registry media: the category and the values of the
/// upstream `archive_fmt` fields, including `[key]`/`[index]` subscripts.
pub(crate) fn archive_key(media: &Media) -> Option<(&'static str, String)> {
    let site = site(&media.category)?;
    let keywords = media.metadata.as_ref()?.site_keywords.as_ref()?;
    let format = keywords.route.archive;
    let mut fields = Vec::new();
    let mut rest = format;
    while let Some(start) = rest.find('{') {
        let end = rest[start..].find('}')? + start;
        let spec = rest[start + 1..end].split([':', '!']).next()?;
        // `{a|b}` takes the first truthy alternative, else the last value.
        let mut value = None;
        for alternative in spec.split('|') {
            value = field(&keywords.fields, alternative);
            if art::truthy(value.as_ref()) {
                break;
            }
        }
        let value = value.filter(|value| !value.is_null())?;
        fields.push(scalar(&value));
        rest = &rest[end + 1..];
    }
    let identity = serde_json::json!([format, fields]).to_string();
    (identity.len() <= 4096).then_some((site.category, identity))
}

/// One `name[key][index]` archive field.
fn field(fields: &Map<String, Value>, spec: &str) -> Option<Value> {
    let mut parts = spec.split('[');
    let mut value = fields.get(parts.next()?)?.clone();
    for key in parts {
        let key = key.strip_suffix(']')?;
        value = match (&value, key.parse::<usize>()) {
            (Value::Array(items), Ok(i)) => items.get(i)?.clone(),
            (Value::String(text), Ok(i)) => text.chars().nth(i)?.to_string().into(),
            (Value::Object(map), _) => map.get(key)?.clone(),
            _ => return None,
        };
    }
    Some(value)
}

/// Default filename and directory templates for registry media.
pub(crate) fn default_templates(
    media: &Media,
) -> Option<&'static (
    crate::naming::FilenameTemplate,
    crate::naming::DirectoryTemplate,
)> {
    Some(
        media
            .metadata
            .as_ref()?
            .site_keywords
            .as_ref()?
            .route
            .templates(),
    )
}

/// The route for compatibility output, chosen by its `subcategory` keyword.
pub(crate) fn route_for(site: &Site, fields: &Map<String, Value>) -> &'static Route {
    fields
        .get("subcategory")
        .and_then(Value::as_str)
        .and_then(|sub| site.routes.iter().find(|route| route.subcategory == sub))
        .unwrap_or(&site.routes[0])
}

#[cfg(test)]
pub(crate) mod testing {
    //! Local HTTP fixtures shared by site tests.
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread::{self, JoinHandle},
        time::Duration,
    };

    pub fn response(status: &str, content_type: &str, body: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\nRetry-After: 0\r\n\r\n",
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(body);
        out
    }

    pub fn json(body: Value) -> Vec<u8> {
        response("200 OK", "application/json", body.to_string().as_bytes())
    }

    pub fn html(body: &str) -> Vec<u8> {
        response("200 OK", "text/html", body.as_bytes())
    }

    /// Serve the given replies in order; returns each request's head and body.
    pub fn serve(responses: Vec<Vec<u8>>) -> (Url, JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let root = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let task = thread::spawn(move || {
            let mut requests = Vec::new();
            for reply in responses {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 4096];
                loop {
                    let count = stream.read(&mut buffer).unwrap();
                    assert_ne!(count, 0);
                    request.extend_from_slice(&buffer[..count]);
                    if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                        let length = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if request.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                requests.push(String::from_utf8(request).unwrap());
                let _ = stream.write_all(&reply);
            }
            requests
        });
        (root, task)
    }

    pub fn http(name: &'static str, root: Url) -> Http {
        Http::new(name, Duration::from_secs(5))
            .unwrap()
            .with_network(&crate::network::NetworkOptions::new(Some(String::new()), None).unwrap())
            .unwrap()
            .with_api_root(root)
    }

    /// Collect all media, failing on any error.
    pub async fn media(events: Events<'_>) -> Vec<Media> {
        crate::event::media_only(events)
            .map(|m| m.unwrap())
            .collect()
            .await
    }

    /// Whether extraction ends in an error.
    pub async fn fails(events: Events<'_>) -> bool {
        crate::event::media_only(events)
            .any(|m| async move { m.is_err() })
            .await
    }

    /// Cancel each queued child before it starts; returns their metadata.
    pub async fn queued(mut events: Events<'_>) -> Vec<ChildMetadata> {
        let mut children = Vec::new();
        while let Some(event) = events.next().await {
            if let ExtractionEvent::BeginChild(scope) = event.unwrap() {
                scope.control.cancel();
                children.push(scope.metadata);
            }
        }
        children
    }

    pub fn keywords(media: &Media) -> &Map<String, Value> {
        media
            .metadata
            .as_ref()
            .unwrap()
            .site_keywords
            .as_ref()
            .map(|keywords| &keywords.fields)
            .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_helpers_match_gallery_dl() {
        assert_eq!(
            remove_html("<b>Hello</b>&amp;  <i>world</i>\n x"),
            "Hello &amp; world x"
        );
        assert_eq!(extr("a[b]c", "[", "]"), Some("b"));
        let url = Url::parse("https://x.test/a/My%20File.JPEG?x=1").unwrap();
        assert_eq!(nameext(&url), ("My File".into(), "jpeg".into()));
        let url = Url::parse("https://x.test/a/noext").unwrap();
        assert_eq!(nameext(&url), ("noext".into(), String::new()));
        assert_eq!(scalar(&Value::Bool(true)), "True");
        assert_eq!(percent_decode("a&b=c+d%20e"), "a&b=c+d e");
    }

    #[test]
    fn registry_templates_parse() {
        let mut seen = std::collections::BTreeSet::new();
        for site in sites() {
            assert!(seen.insert(site.category), "{}", site.category);
            assert!(!site.routes.is_empty() && site.category.len() <= 64);
            for route in site.routes {
                route.templates();
            }
        }
    }
}
