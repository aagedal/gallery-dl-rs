//! Experimental native Newgrounds art extraction.
//!
//! Mirrors the art parts of gallery_dl/extractor/newgrounds.py. Art posts
//! (`www.newgrounds.com/art/view/USER/SLUG`) are scraped from page HTML: the
//! main image from `<div class="image">` (falling back to `og:image`), extra
//! images from the `imageData` JSON and `art-images` block of multi-image
//! posts, the title from `og:title` and the date from
//! `itemprop="datePublished"`. User art listings (`USER.newgrounds.com/art`)
//! page through `art/page/N` with `X-Requested-With: XMLHttpRequest` (capped
//! at 200 pages) and fetch every listed post. Audio, movies and games are
//! not supported, and there is no login, so adult-rated posts are skipped by
//! the site. Media must be HTTPS on `*.ngfiles.com`.
use crate::{
    Result, error,
    event::{ExtractionEvent, group},
    extract::{Media, MediaJob, PostMetadata},
    file_metadata::FileMetadata,
    imagehosts::{
        attribute, between, between_all, clean_text, meta_property, tag_around, unescape,
    },
    network::NetworkOptions,
    transport::{RateLimits, Redirects, retry_after},
};
use futures_util::{
    StreamExt,
    stream::{self, BoxStream},
};
use reqwest::{Client, StatusCode};
use serde_json::Value;
use std::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
    time::{Duration, SystemTime},
};
use url::Url;

const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:128.0) Gecko/20100101 Firefox/128.0";
const MAX_RESPONSE: usize = 16 * 1024 * 1024;
const MAX_PAGES: u64 = 200;
/// Subdomains that are not user pages.
const RESERVED_HOSTS: [&str; 6] = ["www", "art", "uploads", "img", "api", "css"];

/// Media IDs are `POSTID_INDEX`.
pub fn valid_id(id: &str) -> bool {
    id.split_once('_').is_some_and(|(post, index)| {
        [post, index]
            .iter()
            .all(|p| (1..=20).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_digit()))
    })
}

fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Post { user: String, slug: String },
    User { user: String },
}

fn plain(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
}

pub fn target(url: &Url) -> Result<Target> {
    let unsupported = || {
        error(
            "unsupported Newgrounds URL; expected https://www.newgrounds.com/art/view/USER/SLUG or https://USER.newgrounds.com/art",
        )
    };
    if !plain(url) {
        return Err(unsupported());
    }
    let parts: Vec<&str> = url
        .path_segments()
        .map(|s| s.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    match url.host_str().ok_or_else(unsupported)? {
        "www.newgrounds.com" | "newgrounds.com" => match parts[..] {
            ["art", "view", user, slug] if valid_name(user) && valid_name(slug) => {
                Ok(Target::Post {
                    user: user.to_ascii_lowercase(),
                    slug: slug.into(),
                })
            }
            _ => Err(unsupported()),
        },
        host => {
            let user = host
                .strip_suffix(".newgrounds.com")
                .filter(|u| valid_name(u) && !RESERVED_HOSTS.contains(u))
                .ok_or_else(unsupported)?;
            match parts[..] {
                ["art"] => Ok(Target::User {
                    user: user.to_ascii_lowercase(),
                }),
                _ => Err(unsupported()),
            }
        }
    }
}

pub fn is_newgrounds_url(url: &Url) -> bool {
    target(url).is_ok()
}

/// HTTPS media on `*.ngfiles.com`.
pub fn media_url(raw: &str) -> Option<Url> {
    let raw = unescape(raw.trim());
    let raw = match raw.strip_prefix("//") {
        Some(rest) => format!("https://{rest}"),
        None => raw,
    };
    let url = Url::parse(&raw).ok()?;
    let label = url.host_str()?.strip_suffix(".ngfiles.com")?;
    (url.scheme() == "https"
        && plain(&url)
        && !label.is_empty()
        && label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.')))
    .then_some(url)
}

fn itemprop<'a>(html: &'a str, name: &str) -> Option<&'a str> {
    for quote in ['"', '\''] {
        let needle = format!("itemprop={quote}{name}{quote}");
        if let Some(at) = html.find(&needle) {
            return tag_around(html, at).and_then(|tag| attribute(tag, "content"));
        }
    }
    None
}

fn image_urls(html: &str) -> Vec<Url> {
    let mut urls: Vec<Url> = Vec::new();
    let mut push = |raw: &str| {
        if let Some(url) = media_url(raw)
            && !urls.contains(&url)
        {
            urls.push(url);
        }
    };
    let main = between(html, "<div class=\"image\"", "</div>").and_then(|block| {
        between(block, "href=\"", "\"")
            .filter(|h| media_url(h).is_some())
            .or_else(|| between(block, "src=\"", "\""))
    });
    if let Some(main) = main.or_else(|| meta_property(html, "og:image")) {
        push(main);
    }
    if let Some(data) = between(html, "let imageData =", "];")
        && let Ok(Value::Array(images)) =
            serde_json::from_str::<Value>(&format!("{}]", data.trim()))
    {
        for image in images {
            if let Some(raw) = image.get("image").and_then(Value::as_str) {
                push(raw);
            }
        }
    }
    if let Some(block) = between(html, "class=\"art-images\"", "</div>") {
        for raw in between_all(block, "data-smartload-src=\"", "\"") {
            push(raw);
        }
        for raw in between_all(block, " src=\"", "\"") {
            push(raw);
        }
    }
    urls
}

/// Leading numeric ID of `images/NNN/NNNNNNN_user_slug.ext`.
fn image_index(url: &Url) -> Option<String> {
    let name = url.path_segments()?.next_back()?;
    let digits: String = name
        .bytes()
        .take_while(u8::is_ascii_digit)
        .map(char::from)
        .collect();
    (1..=20).contains(&digits.len()).then_some(digits)
}

fn extension(url: &Url) -> String {
    url.path_segments()
        .and_then(|mut s| s.next_back())
        .and_then(|name| name.rsplit_once('.'))
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .filter(|ext| {
            (1..=5).contains(&ext.len()) && ext.bytes().all(|b| b.is_ascii_alphanumeric())
        })
        .map(|ext| if ext == "jpeg" { "jpg".into() } else { ext })
        .unwrap_or_else(|| "jpg".into())
}

/// Parse an art post page into its images.
pub fn parse_post(bytes: &[u8], user: &str) -> Result<Vec<Media>> {
    let html = std::str::from_utf8(bytes).map_err(|_| error("Newgrounds returned invalid HTML"))?;
    let urls = image_urls(html);
    let post_id = urls
        .first()
        .and_then(image_index)
        .ok_or_else(|| error("Newgrounds post contains no images (adult posts need a login)"))?;
    let caption = meta_property(html, "og:title")
        .and_then(clean_text)
        .map(Arc::<str>::from);
    let timestamp = itemprop(html, "datePublished")
        .or_else(|| between(html, "\"datePublished\":\"", "\""))
        .and_then(|d| crate::date::parse_date(d.trim()).ok());
    let username: Arc<str> = Arc::from(user);
    Ok(urls
        .into_iter()
        .enumerate()
        .map(|(n, url)| {
            let index = n + 1;
            Media {
                filename: format!("{post_id}_{index}.{}", extension(&url)),
                job: MediaJob::DirectHttp,
                category: "newgrounds".into(),
                metadata: Some(PostMetadata {
                    file: Some(FileMetadata {
                        media_type: Some(Arc::from("photo")),
                        ..FileMetadata::default()
                    }),
                    post_id: post_id.clone(),
                    media_id: format!("{post_id}_{index}"),
                    username: Some(username.clone()),
                    caption: caption.clone(),
                    timestamp,
                    index,
                    ..PostMetadata::default()
                }),
                url,
                headers: BTreeMap::new(),
                handoff: BTreeMap::new(),
            }
        })
        .collect())
}

/// Parse one `art/page/N` response into (user, slug) posts and `more`.
pub fn parse_listing(bytes: &[u8]) -> Result<(Vec<(String, String)>, bool)> {
    let body: Value =
        serde_json::from_slice(bytes).map_err(|_| error("Newgrounds returned invalid JSON"))?;
    let mut posts = Vec::new();
    if let Some(years) = body.get("items").and_then(Value::as_object) {
        for fragment in years.values().filter_map(Value::as_array).flatten() {
            let Some(href) = fragment.as_str().and_then(|f| between(f, "href=\"", "\"")) else {
                continue;
            };
            let href = match href.strip_prefix("//") {
                Some(rest) => format!("https://{rest}"),
                None => href.to_owned(),
            };
            if let Ok(Target::Post { user, slug }) = Url::parse(&href)
                .map_err(|_| ())
                .and_then(|u| target(&u).map_err(|_| ()))
            {
                let post = (user, slug);
                if !posts.contains(&post) {
                    posts.push(post);
                }
            }
        }
    }
    let more = match body.get("more") {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => !s.trim().is_empty(),
        Some(Value::Null) | None => false,
        Some(_) => true,
    };
    Ok((posts, more))
}

pub struct Newgrounds {
    client: Client,
    network: NetworkOptions,
    root: Url,
    user_root: Option<Url>,
    timeout: Duration,
    request_timeout: Duration,
    retries: u8,
    rate_limits: RateLimits,
}

struct ListingState {
    user: String,
    page: u64,
    done: bool,
    pending: VecDeque<ExtractionEvent>,
}

impl Newgrounds {
    pub fn new(timeout: Duration) -> Result<Self> {
        Ok(Self {
            client: NetworkOptions::default().client(USER_AGENT, timeout)?,
            network: NetworkOptions::default(),
            root: Url::parse("https://www.newgrounds.com/").expect("static URL"),
            user_root: None,
            timeout,
            request_timeout: timeout,
            retries: 0,
            rate_limits: RateLimits::default(),
        })
    }

    /// Apply proxy and user-agent settings to requests and emitted media.
    pub fn with_network(mut self, network: &NetworkOptions) -> Result<Self> {
        self.client = network.client(USER_AGENT, self.timeout)?;
        self.network = network.clone();
        Ok(self)
    }

    /// Additional attempts for HTTP 429/5xx and transport failures.
    pub fn with_retries(mut self, retries: u8) -> Self {
        self.retries = retries;
        self
    }

    /// Bound each HTTP request separately from the per-call deadline.
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// Override the www and user-page origins; intended for local tests.
    pub fn with_roots(mut self, root: Url, user_root: Url) -> Self {
        self.root = root;
        self.user_root = Some(user_root);
        self
    }

    fn user_root(&self, user: &str) -> Url {
        self.user_root.clone().unwrap_or_else(|| {
            Url::parse(&format!("https://{user}.newgrounds.com/")).expect("validated user")
        })
    }

    async fn call(&self, endpoint: Url, ajax: bool) -> Result<Vec<u8>> {
        tokio::time::timeout(self.timeout, self.request(&endpoint, ajax))
            .await
            .map_err(|_| error("native Newgrounds extraction timed out"))?
    }

    async fn post(&self, user: &str, slug: &str) -> Result<VecDeque<ExtractionEvent>> {
        let endpoint = self
            .root
            .join(&format!("art/view/{user}/{slug}"))
            .map_err(|_| error("invalid Newgrounds path"))?;
        let body = self.call(endpoint, false).await?;
        let mut items = parse_post(&body, user)?;
        for item in &mut items {
            self.network.apply_media(item);
        }
        Ok(group(items, "newgrounds"))
    }

    async fn next_page(&self, state: &mut ListingState) -> Result<()> {
        state.page += 1;
        let endpoint = self
            .user_root(&state.user)
            .join(&format!("art/page/{}", state.page))
            .map_err(|_| error("invalid Newgrounds path"))?;
        let body = self.call(endpoint, true).await?;
        let (posts, more) = parse_listing(&body)?;
        state.done = posts.is_empty() || !more || state.page >= MAX_PAGES;
        for (user, slug) in posts {
            match self.post(&user, &slug).await {
                Ok(events) => state.pending.extend(events),
                // Adult or removed posts do not abort the whole listing.
                Err(e) if e.to_string().contains("contains no images") => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    pub fn stream(&self, url: &Url) -> BoxStream<'_, Result<Media>> {
        crate::event::media_only(self.events(url))
    }

    /// Emit one post boundary per art post; listings paginate lazily.
    pub fn events(&self, url: &Url) -> BoxStream<'_, Result<ExtractionEvent>> {
        let user = match target(url) {
            Ok(Target::User { user }) => user,
            Ok(Target::Post { user, slug }) => {
                return stream::once(async move { self.post(&user, &slug).await })
                    .map(|result| match result {
                        Ok(events) => stream::iter(events.into_iter().map(Ok)).boxed(),
                        Err(e) => stream::once(async move { Err(e) }).boxed(),
                    })
                    .flatten()
                    .boxed();
            }
            Err(e) => return stream::once(async move { Err(e) }).boxed(),
        };
        let state = ListingState {
            user,
            page: 0,
            done: false,
            pending: VecDeque::new(),
        };
        stream::unfold(Some(state), move |state| async move {
            let mut state = state?;
            loop {
                if let Some(event) = state.pending.pop_front() {
                    return Some((Ok(event), Some(state)));
                }
                if state.done {
                    return None;
                }
                if let Err(e) = self.next_page(&mut state).await {
                    return Some((Err(e), None));
                }
            }
        })
        .boxed()
    }

    async fn request(&self, endpoint: &Url, ajax: bool) -> Result<Vec<u8>> {
        for attempt in 0..=self.retries {
            match self.attempt(endpoint, ajax, attempt).await {
                Ok(body) => return Ok(body),
                Err(failure) if failure.retry && attempt < self.retries => {
                    tokio::time::sleep(backoff(attempt)).await;
                }
                Err(failure) => return Err(failure.error),
            }
        }
        unreachable!()
    }

    async fn attempt(
        &self,
        endpoint: &Url,
        ajax: bool,
        attempt: u8,
    ) -> std::result::Result<Vec<u8>, Failure> {
        let mut target = endpoint.clone();
        let mut redirects = Redirects::new(endpoint);
        let mut response = loop {
            self.rate_limits.wait(&target, self.timeout).await?;
            let mut request = self
                .client
                .get(target.clone())
                .timeout(self.request_timeout)
                .header("Accept-Language", "en-US,en;q=0.5")
                .header("Referer", "https://www.newgrounds.com/");
            if ajax {
                request = request
                    .header("X-Requested-With", "XMLHttpRequest")
                    .header("Accept", "application/json, text/javascript, */*; q=0.01");
            }
            let response = request.send().await.map_err(|_| {
                Failure::transient("Newgrounds request failed; check network access")
            })?;
            match redirects.next(response.url(), response.status(), response.headers())? {
                Some(next) => target = next,
                None => break response,
            }
        };
        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
            let delay = retry_after(response.headers(), SystemTime::now());
            self.rate_limits
                .defer(endpoint, delay.unwrap_or_else(|| backoff(attempt)));
            return Err(Failure::transient(
                if status == StatusCode::TOO_MANY_REQUESTS {
                    match delay {
                        Some(delay) => format!(
                            "Newgrounds rate limit; retry after {} seconds",
                            delay.as_secs()
                        ),
                        None => "Newgrounds rate limit; retry later".into(),
                    }
                } else {
                    format!("Newgrounds returned HTTP {status}")
                },
            ));
        }
        if matches!(status.as_u16(), 404 | 410) {
            return Err(error("Newgrounds post or user not found").into());
        }
        if matches!(status.as_u16(), 401 | 403) {
            return Err(error("Newgrounds refused access").into());
        }
        if !status.is_success() {
            return Err(error(format!("Newgrounds returned HTTP {status}")).into());
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Failure::transient("could not read Newgrounds response"))?
        {
            if body.len() + chunk.len() > MAX_RESPONSE {
                return Err(error("Newgrounds response exceeded 16 MiB").into());
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }
}

fn backoff(attempt: u8) -> Duration {
    Duration::from_millis(250 * (1u64 << attempt.min(6)))
}

struct Failure {
    error: crate::Error,
    retry: bool,
}

impl Failure {
    fn transient(message: impl Into<String>) -> Self {
        Self {
            error: error(message),
            retry: true,
        }
    }
}

impl From<crate::Error> for Failure {
    fn from(error: crate::Error) -> Self {
        Self {
            error,
            retry: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread::{self, JoinHandle},
    };

    fn post_html() -> String {
        r#"<html><head>
<meta property="og:title" content="Big &amp; Bold">
<meta property="og:image" content="https://art.ngfiles.com/thumbnails/1/1234567.png">
<meta itemprop="datePublished" content="2024-01-01T00:00:00-00:00">
</head><body>
<div class="image"><a href="https://art.ngfiles.com/images/1/1234567_alice_ship.png?f1"><img src="x"></a></div>
<script>let imageData = [{"image":"https:\/\/art.ngfiles.com\/images\/1\/1234567_alice_ship.png?f1"},{"image":"https:\/\/art.ngfiles.com\/images\/1\/1234567_9_alice_ship.jpeg"},{"image":"https:\/\/evil.example\/a.png"}];</script>
<div class="art-images"><img data-smartload-src="http://art.ngfiles.com/images/1/x.png"></div>
</body></html>"#
            .into()
    }

    #[test]
    fn parses_post_and_listing() {
        let items = parse_post(post_html().as_bytes(), "alice").unwrap();
        let names: Vec<_> = items.iter().map(|m| m.filename.as_str()).collect();
        assert_eq!(names, ["1234567_1.png", "1234567_2.jpg"]);
        let meta = items[1].metadata.as_ref().unwrap();
        assert_eq!(
            (meta.post_id.as_str(), meta.media_id.as_str()),
            ("1234567", "1234567_2")
        );
        assert_eq!(meta.caption.as_deref(), Some("Big & Bold"));
        assert_eq!((meta.timestamp, meta.index), (Some(1704067200), 2));
        assert_eq!(meta.username.as_deref(), Some("alice"));
        assert!(items[0].archive_key().unwrap().starts_with("newgrounds:"));
        assert!(parse_post(b"<html>login required</html>", "alice").is_err());

        let fallback =
            r#"<meta property="og:image" content="https://art.ngfiles.com/images/2/42_bob_x.gif">"#;
        assert_eq!(
            parse_post(fallback.as_bytes(), "bob").unwrap()[0].filename,
            "42_1.gif"
        );

        let listing = json!({"items": {"2024": [
            "<a href=\"https://www.newgrounds.com/art/view/alice/ship\" class=\"item\">",
            "<a href=\"//www.newgrounds.com/art/view/alice/boat\">",
            "<a href=\"https://www.newgrounds.com/portal/view/1\">"]}, "more": "x"});
        let (posts, more) = parse_listing(listing.to_string().as_bytes()).unwrap();
        assert_eq!(
            posts,
            [
                ("alice".into(), "ship".into()),
                ("alice".into(), "boat".into())
            ]
        );
        assert!(more);
    }

    #[test]
    fn classifies_urls_and_hosts() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.newgrounds.com/art/view/Alice/ship").unwrap(),
            Target::Post {
                user: "alice".into(),
                slug: "ship".into()
            }
        );
        assert_eq!(
            t("https://alice.newgrounds.com/art/").unwrap(),
            Target::User {
                user: "alice".into()
            }
        );
        for raw in [
            "https://www.newgrounds.com/",
            "https://www.newgrounds.com/portal/view/1",
            "https://www.newgrounds.com/art",
            "https://alice.newgrounds.com/",
            "https://alice.newgrounds.com/audio",
            "https://www.newgrounds.com/art/view/alice",
            "https://art.ngfiles.com/images/1/1_a.png",
            "https://www.newgrounds.com.evil.example/art/view/a/b",
        ] {
            assert!(t(raw).is_err(), "{raw}");
        }
        assert!(media_url("https://art.ngfiles.com/a.png").is_some());
        assert!(media_url("http://art.ngfiles.com/a.png").is_none());
        assert!(media_url("https://ngfiles.com.evil.example/a.png").is_none());
    }

    fn response(status: &str, body: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\nRetry-After: 0\r\n\r\n",
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(body);
        out
    }

    fn serve(responses: Vec<Vec<u8>>) -> (Url, JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let root = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let task = thread::spawn(move || {
            let mut requests = Vec::new();
            for reply in responses {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    let count = stream.read(&mut buffer).unwrap();
                    assert_ne!(count, 0);
                    request.extend_from_slice(&buffer[..count]);
                }
                requests.push(String::from_utf8(request).unwrap());
                let _ = stream.write_all(&reply);
            }
            requests
        });
        (root, task)
    }

    fn newgrounds(root: Url) -> Newgrounds {
        Newgrounds::new(Duration::from_secs(5))
            .unwrap()
            .with_network(&NetworkOptions::new(Some(String::new()), None).unwrap())
            .unwrap()
            .with_roots(root.clone(), root)
    }

    #[tokio::test]
    async fn fetches_post_with_retry() {
        let (root, server) = serve(vec![
            response("503 Service Unavailable", b""),
            response("200 OK", post_html().as_bytes()),
        ]);
        let media: Vec<_> = newgrounds(root)
            .with_retries(1)
            .stream(&Url::parse("https://www.newgrounds.com/art/view/alice/ship").unwrap())
            .collect()
            .await;
        assert_eq!(media.len(), 2);
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /art/view/alice/ship "));
    }

    #[tokio::test]
    async fn paginates_user_art() {
        let page = |slug: &str, more: bool| {
            json!({"items": {"2024": [format!("<a href=\"https://www.newgrounds.com/art/view/alice/{slug}\">")]}, "more": more})
                .to_string()
        };
        let (root, server) = serve(vec![
            response("200 OK", page("ship", true).as_bytes()),
            response("200 OK", post_html().as_bytes()),
            response("200 OK", page("adult", false).as_bytes()),
            response("200 OK", b"<html>login</html>"),
        ]);
        let media: Vec<_> = newgrounds(root)
            .stream(&Url::parse("https://alice.newgrounds.com/art").unwrap())
            .collect()
            .await;
        let media: Vec<_> = media.into_iter().map(|m| m.unwrap()).collect();
        assert_eq!(media.len(), 2);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /art/page/1 "));
        assert!(
            requests[0]
                .to_ascii_lowercase()
                .contains("x-requested-with: xmlhttprequest")
        );
        assert!(requests[2].starts_with("GET /art/page/2 "));
        assert!(requests[3].starts_with("GET /art/view/alice/adult "));
    }
}
