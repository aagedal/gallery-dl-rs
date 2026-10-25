//! TheHentaiWorld posts and tag listings, mirroring
//! gallery_dl/extractor/thehentaiworld.py: each post page holds one file
//! (or a doujin's page list); tag listings are extracted post by post.
//!
//! A tag listing skips posts that fail to load, as upstream logs and
//! continues.
use crate::sites::art::{Scan, date, extr, extract_iter, nameext_into, on_host, parse_iso};
use crate::sites::manga::text;
use crate::sites::{Events, Http, Item, Route, Site, unescape};
use crate::{Result, error};
use futures_util::StreamExt;
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://thehentaiworld.com";
const FILENAME: &str = "{title} ({id}{num:?-//}).{extension}";
const ARCHIVE: &str = "{id}_{num}";
static TAG: Route = Route::new("tag", FILENAME, &["{category}", "{search_tags}"], ARCHIVE);
static POST: Route = Route::new("post", FILENAME, &["{category}"], ARCHIVE);

/// Upstream `request_interval = (0.5, 1.5)`.
static INTERVAL: super::Interval = super::Interval::new(500);

pub static SITE: Site = Site {
    category: "thehentaiworld",
    name: "TheHentaiWorld",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&TAG, &POST],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Tag(String),
    /// `/videos/SLUG` or `/…hentai-images/SLUG`, raw.
    Post(String),
}

/// `/tag/([^/?#]+)` and `(/(?:video|(?:[\w-]+-)?hentai-image)s/([^/?#]+))`,
/// both unanchored.
fn target(url: &Url) -> Option<Target> {
    if !on_host(url, &["thehentaiworld.com"]) {
        return None;
    }
    let raw: Vec<&str> = url.path().split('/').skip(1).collect();
    match raw.as_slice() {
        ["tag", tag, ..] if !tag.is_empty() => Some(Target::Tag((*tag).to_owned())),
        [kind, slug, ..] if !slug.is_empty() && post_kind(kind) => {
            Some(Target::Post(format!("/{kind}/{slug}")))
        }
        _ => None,
    }
}

/// `video|(?:[\w-]+-)?hentai-image` followed by `s`.
fn post_kind(kind: &str) -> bool {
    if kind == "videos" || kind == "hentai-images" {
        return true;
    }
    kind.strip_suffix("-hentai-images").is_some_and(|prefix| {
        !prefix.is_empty()
            && prefix
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    })
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Post(path)) => crate::sites::once(async move {
            let post = extract_post(http, &format!("{ROOT}{path}/")).await?;
            Ok(crate::sites::events(post_events(http, &POST, post)))
        }),
        Some(Target::Tag(tag)) => tag_posts(http, tag),
        None => crate::sites::fail(error("unsupported TheHentaiWorld URL")),
    }
}

/// `ThehentaiworldExtractor._extract_post`. The post type comes from the
/// character after the root in the post URL, as upstream reads `url[27]`.
async fn extract_post(http: &Http, url: &str) -> Result<Map<String, Value>> {
    INTERVAL.wait().await;
    let page = super::page(http, url, &[]).await?;
    Ok(parse_post(&page, url.as_bytes().get(27).copied()))
}

fn parse_post(page: &str, kind: Option<u8>) -> Map<String, Value> {
    let mut e = Scan::new(page);
    let mut post = Map::new();
    let mut set = |key: &str, value: Value| {
        post.insert(key.to_owned(), value);
    };
    set("num", 0.into());
    set("count", 1.into());
    set("title", unescape(e.extr("<title>", "<").trim()).into());
    set("id", text::parse_int(e.extr(" postid-", " ")).into());
    set("slug", e.extr(" post-", "\"").into());
    let tags = e.extr("id=\"tagsHead\">", "</ul>");
    set("date", date(parse_iso(e.extr("<li>Posted: ", "<"))));
    if kind == Some(b'v') {
        set("type", "video".into());
        set("width", 0.into());
        set("height", 0.into());
        set(
            "votes",
            text::parse_int(e.extr("(<strong>", "</strong>")).into(),
        );
        set("score", text::parse_float(e.extr("<strong>", "<")));
        set("file_url", e.extr("<source src=\"", "\"").into());
    } else {
        let kind = match kind {
            Some(b'g') => "animated",
            Some(b'3') => "3d cgi",
            _ => "image",
        };
        set("type", kind.into());
        set("width", text::parse_int(e.extr("<li>Size: ", " ")).into());
        set("height", text::parse_int(e.extr("x ", "<")).into());
        set("file_url", e.extr("a href=\"", "\"").into());
        set(
            "votes",
            text::parse_int(e.extr("(<strong>", "</strong>")).into(),
        );
        set("score", text::parse_float(e.extr("<strong>", "<")));
        let doujin = e.extr("<a id=\"prev-page\"", "</div></div><");
        if !doujin.is_empty() {
            let urls: Vec<Value> = extract_iter(doujin, "class=\"border\" src=\"", "\"")
                .into_iter()
                .map(|url| full_size(url).into())
                .collect();
            set("file_urls", urls.into());
        }
    }
    // Tags grouped by their link class, in order of first appearance.
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    for item in tags.split("<li><a class=\"").skip(1) {
        let Some((class, rest)) = item.split_once('"') else {
            continue;
        };
        let Some(rest) = rest.strip_prefix(" href=\"") else {
            continue;
        };
        let Some((_, rest)) = rest.split_once("\">") else {
            continue;
        };
        let name = &rest[..rest.find('<').unwrap_or(rest.len())];
        if class.contains('<') || name.is_empty() {
            continue;
        }
        match groups.iter_mut().find(|(key, _)| key == class) {
            Some((_, names)) => names.push(name.to_owned()),
            None => groups.push((class.to_owned(), vec![name.to_owned()])),
        }
    }
    let all: Vec<String> = groups.iter().flat_map(|(_, names)| names.clone()).collect();
    set("tags", all.into());
    for (class, names) in groups {
        let key = match class.as_str() {
            "" => "tags_general".to_owned(),
            class => format!("tags_{class}"),
        };
        set(&key, names.into());
    }
    post
}

/// `-220x\d+\.` replaced by `.`: a doujin thumbnail's full-size image.
fn full_size(url: &str) -> String {
    let mut out = String::with_capacity(url.len());
    let mut rest = url;
    while let Some(pos) = rest.find("-220x") {
        let tail = &rest[pos + 5..];
        let digits = tail.bytes().take_while(u8::is_ascii_digit).count();
        if digits > 0 && tail[digits..].starts_with('.') {
            out.push_str(&rest[..pos]);
            rest = &tail[digits..];
        } else {
            out.push_str(&rest[..pos + 5]);
            rest = tail;
        }
    }
    out.push_str(rest);
    out
}

/// `ThehentaiworldExtractor.items` for one post: its doujin pages numbered
/// from one, or its single file as `num` 0.
fn post_events(
    http: &Http,
    route: &'static Route,
    mut post: Map<String, Value>,
) -> Vec<crate::event::ExtractionEvent> {
    let urls: Vec<String> = match post.get("file_urls").and_then(Value::as_array) {
        Some(urls) => {
            let urls: Vec<String> = urls
                .iter()
                .filter_map(|u| u.as_str().map(Into::into))
                .collect();
            post.insert("count".into(), urls.len().into());
            urls
        }
        None => vec![
            post.get("file_url")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        ],
    };
    let numbered = post.contains_key("file_urls");
    let id = crate::sites::scalar(&post["id"]);
    let items = urls
        .into_iter()
        .enumerate()
        .filter_map(|(index, raw)| {
            let url = crate::sites::art::file_url(&raw, ROOT)?;
            let mut data = post.clone();
            let num = if numbered { index + 1 } else { 0 };
            data.insert("num".into(), num.into());
            nameext_into(&raw, &mut data);
            Some(Item::new(url, data).ids(id.clone(), num.to_string()))
        })
        .collect();
    crate::sites::post(&SITE, http, route, items)
}

/// `ThehentaiworldTagExtractor`: the posts of each listing page, extracted
/// as the consumer reaches them, until a page has no `class="next"`.
fn tag_posts(http: &Http, tag: String) -> Events<'_> {
    let base = format!("{ROOT}/tag/{tag}/");
    crate::sites::paginate(Some(1usize), move |state| {
        let base = base.clone();
        let tag = tag.clone();
        async move {
            let Some(pnum) = state else { return Ok(None) };
            let url = match pnum {
                1 => base,
                pnum => format!("{base}page/{pnum}/"),
            };
            INTERVAL.wait().await;
            let page = super::page(http, &url, &[]).await?;
            let posts: Vec<String> = extract_iter(
                extr(&page, "id=\"thumbContainer\"", "<script"),
                " href=\"",
                "\"",
            )
            .into_iter()
            .filter(|href| Url::parse(href).is_ok_and(|url| on_host(&url, &["thehentaiworld.com"])))
            .map(str::to_owned)
            .collect();
            let next =
                (page.contains("class=\"next\"") && pnum < super::MAX_PAGES).then_some(pnum + 1);
            let events = futures_util::stream::iter(posts)
                .then(move |url| {
                    let tag = tag.clone();
                    async move {
                        match extract_post(http, &url).await {
                            Ok(mut post) => {
                                post.insert("search_tags".into(), tag.into());
                                crate::sites::events(post_events(http, &TAG, post))
                            }
                            Err(_) => crate::sites::events(Vec::new()),
                        }
                    }
                })
                .flatten()
                .boxed();
            Ok(Some((events, next)))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::manga::tests::{archive, path};
    use crate::sites::testing::{html, http, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        let post = |path: &str| Some(Target::Post(path.into()));
        assert_eq!(
            t("https://thehentaiworld.com/hentai-images/some-post/"),
            post("/hentai-images/some-post")
        );
        assert_eq!(
            t("https://www.thehentaiworld.com/3d-cgi-hentai-images/x/?a"),
            post("/3d-cgi-hentai-images/x")
        );
        assert_eq!(t("https://thehentaiworld.com/videos/v/"), post("/videos/v"));
        assert_eq!(
            t("https://thehentaiworld.com/tag/some_tag/page/2/"),
            Some(Target::Tag("some_tag".into()))
        );
        for raw in [
            "https://thehentaiworld.com/hentai-images/",
            "https://thehentaiworld.com/-hentai-images/x/",
            "https://thehentaiworld.com/video/x/",
            "https://m.thehentaiworld.com/videos/x/",
            "https://thehentaiworld.com.evil.test/videos/x/",
            "https://thehentaiworld.com:8443/videos/x/",
            "https://u:p@thehentaiworld.com/videos/x/",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    const IMAGE: &str = r#"<title> Night &amp; Day </title><body class="single postid-123 single-format post-night-day">
<ul id="tagsHead"><li><a class="character" href="/tag/a/">Hero</a></li><li><a class="" href="/tag/b/">Sky</a></li><li><a class="character" href="/tag/c/">Sidekick</a></li></ul>
<li>Posted: 2024-05-06T07:08:09</li><li>Size: 1200 x 800</li>
<a href="https://thehentaiworld.com/wp-content/uploads/a.jpg">full</a>(<strong>12</strong> votes) <strong>4.5</strong>"#;

    #[test]
    fn parses_tags_and_doujin_pages() {
        let post = parse_post(IMAGE, Some(b'h'));
        assert_eq!(post["tags"], json!(["Hero", "Sidekick", "Sky"]));
        assert_eq!(post["tags_character"], json!(["Hero", "Sidekick"]));
        assert_eq!(post["tags_general"], json!(["Sky"]));
        assert_eq!(post["width"], 1200);
        assert_eq!(post["height"], 800);
        let doujin = format!(
            "{IMAGE}<a id=\"prev-page\"><img class=\"border\" src=\"https://t.test/p1-220x311.jpg\">\
             <img class=\"border\" src=\"https://t.test/p2-220x5.png\"></div></div><"
        );
        let post = parse_post(&doujin, Some(b'd'));
        assert_eq!(
            post["file_urls"],
            json!(["https://t.test/p1.jpg", "https://t.test/p2.png"])
        );
        assert_eq!(full_size("a-220x-220x1.b"), "a-220x.b");
    }

    #[tokio::test]
    async fn extracts_post() {
        let (root, server) = serve(vec![html(IMAGE)]);
        let http = http("thehentaiworld", root);
        let files = media(extract(
            &http,
            Url::parse("https://thehentaiworld.com/hentai-images/night-day/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let k = keywords(&files[0]);
        assert_eq!(k["title"], "Night & Day");
        assert_eq!(k["id"], 123);
        assert_eq!(k["slug"], "night-day");
        assert_eq!(k["type"], "image");
        assert_eq!(k["votes"], 12);
        assert_eq!(k["score"], 4.5);
        assert_eq!(k["date"], "2024-05-06 07:08:09");
        assert_eq!(k["num"], 0);
        assert_eq!(path(&files[0]), "thehentaiworld/Night & Day (123).jpg");
        assert_eq!(archive(&files[0]), r#"["{id}_{num}",["123","0"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /hentai-images/night-day/ "));
    }

    #[tokio::test]
    async fn tag_extracts_each_post() {
        let listing = r#"<div id="thumbContainer"><a href="https://thehentaiworld.com/videos/clip/">v</a>
<a href="https://evil.test/videos/x/">x</a></div><script>"#;
        let video = r#"<title>Clip</title><body class="x postid-9 y post-clip">(<strong>3</strong>)<strong>5</strong>
<source src="https://thehentaiworld.com/wp-content/uploads/clip.mp4">"#;
        let (root, server) = serve(vec![html(listing), html(video)]);
        let http = http("thehentaiworld", root);
        let files = media(extract(
            &http,
            Url::parse("https://thehentaiworld.com/tag/some_tag/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let k = keywords(&files[0]);
        assert_eq!(k["type"], "video");
        assert_eq!(k["search_tags"], "some_tag");
        assert_eq!(k["extension"], "mp4");
        assert_eq!(path(&files[0]), "thehentaiworld/some_tag/Clip (9).mp4");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /tag/some_tag/ "));
        assert!(requests[1].starts_with("GET /videos/clip/ "));
    }
}
