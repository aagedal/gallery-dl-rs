//! *reactor sites (reactor.cc, pornreactor.cc, thatpervert.com), mirroring
//! gallery_dl/extractor/reactor.py: listing pages hold the posts, whose
//! JSON-LD gives the metadata and whose `image` blocks give the files.
//!
//! Requests go to the host of the given URL, like upstream. Upstream names
//! the generic `reactor` instance after the subdomain (`anime.reactor`);
//! here every `*.reactor.cc` host uses the `reactor` category. The `gif`
//! option is honored; upstream's three to six second request interval is
//! not applied.
use crate::error;
use crate::sites::art::{extr, extract_iter, json_loads, nameext_into, parse_int, str_of};
use crate::sites::media::iso_ts;
use crate::sites::{Events, Http, Item, Route, Site, unescape};
use serde_json::{Map, Value};
use url::Url;

const FILENAME: &str = "{post_id}_{num:>02}{title[:100]:?_//}.{extension}";
const ARCHIVE: &str = "{post_id}_{num}";
static TAG: Route = Route::new(
    "tag",
    FILENAME,
    &["{category}", "{search_tags}"],
    "{search_tags}_{post_id}_{num}",
);
static SEARCH: Route = Route::new(
    "search",
    FILENAME,
    &["{category}", "search", "{search_tags}"],
    "s_{search_tags}_{post_id}_{num}",
);
static USER: Route = Route::new("user", FILENAME, &["{category}", "user", "{user}"], ARCHIVE);
static POST: Route = Route::new("post", FILENAME, &["{category}"], ARCHIVE);

macro_rules! instance {
    ($ident:ident, $category:literal, $name:literal, $hosts:expr) => {
        pub static $ident: Site = Site {
            category: $category,
            name: $name,
            matches: |url| target($hosts, url).is_some(),
            extract: |http, url| extract(&$ident, $hosts, http, url),
            routes: &[&TAG, &SEARCH, &USER, &POST],
            api_key: false,
            cookies: None,
        };
    };
}
instance!(REACTOR, "reactor", "Reactor", reactor_host);
instance!(PORNREACTOR, "pornreactor", "PornReactor", pornreactor_host);
instance!(THATPERVERT, "thatpervert", "ThatPervert", thatpervert_host);

/// `(?:[^/.]+\.)?reactor\.cc`.
fn reactor_host(host: &str) -> bool {
    host == "reactor.cc"
        || host
            .strip_suffix(".reactor.cc")
            .is_some_and(|sub| !sub.is_empty() && !sub.contains('.'))
}

/// `(?:www\.)?(?:pornreactor\.cc|fapreactor.com)`.
fn pornreactor_host(host: &str) -> bool {
    let host = host.strip_prefix("www.").unwrap_or(host);
    matches!(host, "pornreactor.cc" | "fapreactor.com")
}

fn thatpervert_host(host: &str) -> bool {
    host == "thatpervert.com"
}

#[derive(Debug, PartialEq)]
enum Target {
    Tag(String),
    Search(String),
    User(String),
    Post,
}

/// The class and its captured value, then the URL's root and the path
/// (with query) upstream requests.
fn target(hosts: fn(&str) -> bool, url: &Url) -> Option<(Target, String, String)> {
    if !crate::sites::plain(url) || !hosts(url.host_str()?) {
        return None;
    }
    let path = url.path();
    let first = |rest: &str| {
        let value = rest.split(['/', '?', '#']).next().unwrap_or_default();
        (!value.is_empty()).then(|| value.to_owned())
    };
    let target = if let Some(rest) = path.strip_prefix("/tag/") {
        Target::Tag(first(rest)?)
    } else if let Some(rest) = path.strip_prefix("/search/") {
        Target::Search(first(rest)?)
    } else if path == "/search" {
        Target::Search(first(url.query()?.strip_prefix("q=")?)?)
    } else if let Some(rest) = path.strip_prefix("/user/") {
        Target::User(first(rest)?)
    } else {
        crate::sites::art::leading_digits(path.strip_prefix("/post/")?)?;
        Target::Post
    };
    let root = format!("{}://{}", url.scheme(), url.host_str()?);
    let mut request = path.to_owned();
    if let Some(query) = url.query() {
        request = format!("{request}?{query}");
    }
    Some((target, root, request))
}

fn extract<'a>(
    site: &'static Site,
    hosts: fn(&str) -> bool,
    http: &'a Http,
    url: Url,
) -> Events<'a> {
    let Some((target, root, path)) = target(hosts, &url) else {
        return crate::sites::fail(error("unsupported reactor URL"));
    };
    let gif = http.option_bool("gif", false);
    let mut meta = Map::new();
    let route = match target {
        Target::Post => {
            return crate::sites::once(async move {
                let page = http.text(&http.api(&format!("{root}{path}"))?).await?;
                let start = page.find("class=\"uhead\">").unwrap_or(0);
                let items = parse_post(&page[start..], &meta, gif);
                Ok(crate::sites::events(crate::sites::post(
                    site, http, &POST, items,
                )))
            });
        }
        Target::Tag(tag) => {
            meta.insert(
                "search_tags".into(),
                unescape(&tag).replace('+', " ").into(),
            );
            &TAG
        }
        Target::Search(tag) => {
            meta.insert(
                "search_tags".into(),
                unescape(&tag).replace('+', " ").into(),
            );
            &SEARCH
        }
        Target::User(user) => {
            meta.insert("user".into(), unescape(&user).replace('+', " ").into());
            &USER
        }
    };
    crate::sites::paginate(Some(format!("{root}{path}")), move |url| {
        let (root, meta) = (root.clone(), meta.clone());
        async move {
            let Some(url) = url else {
                return Ok(None);
            };
            let requested = http.api(&url)?;
            let (location, page) = http.page(&requested, &[]).await?;
            // The last page of a tag listing may redirect to the first.
            if location != requested
                && requested.as_str().matches('/').count() == 5
                && location.as_str().matches('/').count() == 4
            {
                return Ok(None);
            }
            let mut events = Vec::new();
            for post in extract_iter(&page, "<div class=\"uhead\">", "<div class=\"ufoot\">") {
                events.extend(crate::sites::post(
                    site,
                    http,
                    route,
                    parse_post(post, &meta, gif),
                ));
            }
            Ok(Some((
                crate::sites::events(events),
                next_page(&page, &root),
            )))
        }
    })
}

/// The link after `class='current'` that precedes `class='next'`.
fn next_page(page: &str, root: &str) -> Option<String> {
    let next = page.find("class='next'")?;
    let current = page[..next].rfind("class='current'")?;
    let href = crate::sites::extr(&page[current..], "href='", "'")?;
    Some(format!("{root}{href}"))
}

/// The post's JSON-LD, retried without control characters and with
/// backslashes escaped.
fn post_json(script: &str) -> Option<Value> {
    json_loads(script, "reactor").ok().or_else(|| {
        let cleaned: String = script
            .chars()
            .filter(|c| (*c as u32) >= 32)
            .collect::<String>()
            .replace('\\', "\\\\");
        json_loads(&cleaned, "reactor").ok()
    })
}

/// `_parse_post`: one file per image block, plus `meta`.
pub(crate) fn parse_post(post: &str, meta: &Map<String, Value>, gif: bool) -> Vec<Item> {
    let Some((post, script)) = post.split_once("<script type=\"application/ld+json\">") else {
        return Vec::new();
    };
    let script = script.split("</").next().unwrap_or_default().trim();
    let Some(data) = post_json(script) else {
        return Vec::new();
    };
    let ts = iso_ts(str_of(&data, "datePublished"));
    let user = data
        .pointer("/author/name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let description = unescape(str_of(&data, "description"));
    let headline = unescape(str_of(&data, "headline"));
    let (mut title, mut tags) = match headline.split_once(" / ") {
        Some((title, tags)) => (title.to_owned(), tags.to_owned()),
        None => (headline.clone(), String::new()),
    };
    if tags.is_empty() {
        std::mem::swap(&mut title, &mut tags);
    }
    let mut tags: Vec<String> = tags.split(" :: ").map(str::to_owned).collect();
    tags.sort();
    let id = data
        .pointer("/mainEntityOfPage/@id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let post_id = parse_int(id.rsplit('/').next().unwrap_or_default(), 0);
    let mut items = Vec::new();
    let mut num = 0;
    for image in extract_iter(post, "<div class=\"image\">", "</div>") {
        let mut url = extr(image, " src=\"", "\"").to_owned();
        if url.is_empty() {
            continue;
        }
        if url.starts_with("//") {
            url = format!("http:{url}");
        }
        let width = extr(image, " width=\"", "\"");
        let height = extr(image, " height=\"", "\"");
        let image_id = url.rsplit('-').next().unwrap_or_default();
        let image_id = parse_int(image_id.split('.').next().unwrap_or_default(), 0);
        num += 1;
        let embed = image.starts_with("<iframe ");
        if embed {
            url = unescape(&url);
        } else if !url.contains("/post/webm/") && !url.contains("/post/mp4/") {
            url = url.replace("/post/", "/post/full/");
        }
        if gif && (url.contains("/post/webm/") || url.contains("/post/mp4/")) {
            let gif_url = extr(image, "<a href=\"", "\"");
            if gif_url.is_empty() {
                continue;
            }
            url = gif_url.to_owned();
        }
        let mut fields = Map::new();
        fields.insert(
            "url".into(),
            if embed {
                format!("ytdl:{url}")
            } else {
                url.clone()
            }
            .into(),
        );
        fields.insert("post_id".into(), post_id.into());
        fields.insert("image_id".into(), image_id.into());
        fields.insert("width".into(), parse_int(width, 0).into());
        fields.insert("height".into(), parse_int(height, 0).into());
        fields.insert("title".into(), title.clone().into());
        fields.insert("description".into(), description.clone().into());
        fields.insert("tags".into(), tags.clone().into());
        fields.insert("date".into(), crate::filter::source_date_json(ts));
        fields.insert("user".into(), user.clone().into());
        fields.insert("num".into(), num.into());
        crate::sites::art::update(&mut fields, meta);
        nameext_into(&url, &mut fields);
        let item = if embed {
            super::ytdl_item(&url, fields)
        } else {
            crate::sites::art::file_url(&url, "https://reactor.cc").map(|u| Item::new(u, fields))
        };
        if let Some(item) = item {
            items.push(
                item.ids(post_id.to_string(), num.to_string())
                    .user(Some(user.clone()))
                    .caption(Some(title.clone()))
                    .timestamp(ts),
            );
        }
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |hosts: fn(&str) -> bool, raw: &str| {
            target(hosts, &Url::parse(raw).unwrap()).map(|(t, root, path)| (t, root, path))
        };
        assert_eq!(
            t(reactor_host, "http://anime.reactor.cc/tag/Anime+Art/2"),
            Some((
                Target::Tag("Anime+Art".into()),
                "http://anime.reactor.cc".into(),
                "/tag/Anime+Art/2".into()
            ))
        );
        assert_eq!(
            t(pornreactor_host, "https://fapreactor.com/search?q=cat&x=1").map(|t| t.0),
            Some(Target::Search("cat&x=1".into()))
        );
        assert_eq!(
            t(thatpervert_host, "http://thatpervert.com/user/Bob").map(|t| t.0),
            Some(Target::User("Bob".into()))
        );
        assert_eq!(
            t(reactor_host, "http://reactor.cc/post/123").map(|t| t.0),
            Some(Target::Post)
        );
        for (hosts, raw) in [
            (
                reactor_host as fn(&str) -> bool,
                "http://a.b.reactor.cc/tag/x",
            ),
            (reactor_host, "http://joyreactor.cc/tag/x"),
            (reactor_host, "http://reactor.cc/post/x"),
            (reactor_host, "http://reactor.cc.evil/tag/x"),
            (reactor_host, "http://reactor.cc:81/tag/x"),
            (reactor_host, "http://u@reactor.cc/tag/x"),
            (thatpervert_host, "http://www.thatpervert.com/tag/x"),
        ] {
            assert_eq!(t(hosts, raw), None, "{raw}");
        }
    }

    fn post_html(id: u64) -> String {
        format!(
            r#"<div class="uhead">
            <div class="image"><img src="//img10.reactor.cc/pics/post/Art-Cat-{id}1.jpeg" width="800" height="600"></div>
            <div class="image"><span><a href="//img10.reactor.cc/pics/post/full/a.gif">g</a><video src="http://img10.reactor.cc/pics/post/webm/Cat-{id}2.webm"></span></div>
            <div class="image"><iframe src="https://www.youtube.com/embed/abc?a=1&amp;b=2" width="1"></div>
            <script type="application/ld+json">{{"datePublished": "2020-01-02T03:04:05+03:00",
              "author": {{"name": "Bob"}}, "description": "D &amp; d",
              "headline": "Hello / Cat :: Art", "mainEntityOfPage": {{"@id": "http://reactor.cc/post/{id}"}}}}</script>
            <div class="ufoot">"#
        )
    }

    #[tokio::test]
    async fn pages_through_tag() {
        let first = format!(
            "{}<span class='current'>1</span><a href='/tag/Cat/1'>2</a><a class='next'>",
            post_html(5)
        );
        let (root, server) = serve(vec![html(&first), html(&post_html(6))]);
        let http = http("reactor", root);
        let files = media((REACTOR.extract)(
            &http,
            Url::parse("http://reactor.cc/tag/Cat").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 6);
        assert_eq!(
            files[0].url.as_str(),
            "http://img10.reactor.cc/pics/post/full/Art-Cat-51.jpeg"
        );
        assert_eq!(
            files[1].url.as_str(),
            "http://img10.reactor.cc/pics/post/webm/Cat-52.webm"
        );
        assert_eq!(
            files[2].url.as_str(),
            "ytdl:https://www.youtube.com/embed/abc?a=1&b=2"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["tags"], serde_json::json!(["Art", "Cat"]));
        assert_eq!(k["title"], "Hello");
        assert_eq!(k["description"], "D & d");
        assert_eq!(k["image_id"], 51);
        assert_eq!(k["search_tags"], "Cat");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "5_01_Hello.jpeg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("reactor/Cat")
        );
        let (_, identity) = crate::sites::archive_key(&files[3]).unwrap();
        assert_eq!(
            identity,
            r#"["{search_tags}_{post_id}_{num}",["Cat","6","1"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /tag/Cat "));
        assert!(requests[1].starts_with("GET /tag/Cat/1 "));
    }

    #[tokio::test]
    async fn gif_option_and_single_post() {
        let page = format!("<html>{}", post_html(7));
        let (root, server) = serve(vec![html(&page)]);
        let options = serde_json::json!({"gif": true}).as_object().cloned();
        let http = http("thatpervert", root).with_options(options.map(std::sync::Arc::new));
        let files = media((THATPERVERT.extract)(
            &http,
            Url::parse("http://thatpervert.com/post/7").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 3);
        assert_eq!(
            files[1].url.as_str(),
            "https://img10.reactor.cc/pics/post/full/a.gif"
        );
        assert_eq!(keywords(&files[0])["category"], "thatpervert");
        let (_, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("thatpervert")
        );
        server.join().unwrap();
    }

    #[test]
    fn repairs_json_with_control_characters() {
        let post = "<script type=\"application/ld+json\">{\"headline\": \"a\tb\\x\",\
            \"mainEntityOfPage\": {\"@id\": \"/post/1\"}}</script>";
        let html = format!(
            "<div class=\"image\"><img src=\"http://x.reactor.cc/post/a-1.png\"></div>{post}"
        );
        let items = parse_post(&html, &Map::new(), false);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].keywords["tags"], serde_json::json!(["ab\\x"]));
    }
}
