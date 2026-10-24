//! SubscribeStar user feeds and posts on subscribestar.com and
//! subscribestar.adult, mirroring gallery_dl/extractor/subscribestar.py by
//! scraping post HTML and the infinite-scroll JSON pages.
//!
//! Upstream's `username`/`password` login is not supported; a
//! `_personalization_id` session cookie from `--cookies` is sent instead,
//! also with files on the site's own hosts. Redirects to the subscriber
//! or age verification pages are followed like any same-site redirect
//! rather than reported.
use super::{Scan, extr, extract_iter, parse_int, parse_query};
use crate::sites::{Events, Http, Item, Route, Site, unescape};
use crate::{Result, error};
use futures_util::{StreamExt, stream};
use serde_json::{Map, Value};
use url::Url;

pub static SITE: Site = Site {
    category: "subscribestar",
    name: "SubscribeStar",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&USER, &POST, &USER_ADULT, &POST_ADULT],
    api_key: false,
    // `subscribestar.adult` comes from `extra_cookie_domains`.
    cookies: Some("subscribestar.com"),
};

const FILENAME: &str = "{post_id}_{id}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{author_name}"];

static USER: Route = Route::new("user", FILENAME, DIRECTORY, "{id}");
static POST: Route = Route::new("post", FILENAME, DIRECTORY, "{id}");
// subscribestar.adult URLs append `-adult` to the subcategory.
static USER_ADULT: Route = Route::new("user-adult", FILENAME, DIRECTORY, "{id}");
static POST_ADULT: Route = Route::new("post-adult", FILENAME, DIRECTORY, "{id}");

#[derive(Debug, PartialEq)]
enum Target {
    /// Whether on subscribestar.adult, the user and its query.
    User(bool, String, Option<Vec<(String, String)>>),
    Post(bool, String),
}

fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) {
        return None;
    }
    let host = url.host_str()?;
    let adult = match host.strip_prefix("www.").unwrap_or(host) {
        "subscribestar.com" => false,
        "subscribestar.adult" => true,
        _ => return None,
    };
    let path = url.path().strip_prefix('/')?;
    if let Some(id) = path.strip_prefix("posts/") {
        return Some(Target::Post(adult, super::leading_digits(id)?.to_owned()));
    }
    let user = path.split('/').next().filter(|s| !s.is_empty())?;
    let query = url
        .query()
        .filter(|q| !q.is_empty() && user.len() == path.len())
        .map(parse_query);
    Some(Target::User(adult, user.to_owned(), query))
}

fn root(adult: bool) -> &'static str {
    if adult {
        "https://subscribestar.adult"
    } else {
        "https://www.subscribestar.com"
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported SubscribeStar URL"));
    };
    match target {
        Target::Post(adult, id) => crate::sites::once(async move {
            let route = if adult { &POST_ADULT } else { &POST };
            let page = request(http, &format!("{}/posts/{id}", root(adult))).await?;
            Ok(post_events(
                http,
                route,
                root(adult),
                &page,
                true,
                &Map::new(),
            ))
        }),
        Target::User(adult, user, query) => {
            let route = if adult { &USER_ADULT } else { &USER };
            let mut extra = Map::new();
            let mut url = format!("{}/{user}", root(adult));
            if let Some(query) = query {
                if let Some((_, tag)) = query.iter().find(|(k, _)| k == "tag") {
                    extra.insert("search_tags".into(), tag.clone().into());
                }
                let qs = url::form_urlencoded::Serializer::new(String::new())
                    .extend_pairs(&query)
                    .finish();
                url = format!("{url}?{qs}");
            }
            pagination(http, route, root(adult), url, extra)
        }
    }
}

/// GET a page, following the site's short "redirected" HTML messages.
async fn request(http: &Http, url: &str) -> Result<String> {
    let mut url = http.api(url)?;
    for _ in 0..5 {
        let page = http.text(&url).await?;
        if page.len() < 250 && page.contains(">redirected<") {
            let href = unescape(extr(&page, "href=\"", "\""));
            let next = Url::parse(&href)
                .ok()
                .filter(target_host)
                .ok_or_else(|| error("SubscribeStar redirected off-site"))?;
            url = http.api(next.as_str())?;
            continue;
        }
        return Ok(page);
    }
    Err(error("SubscribeStar redirected too often"))
}

fn target_host(url: &Url) -> bool {
    super::on_host(url, &["subscribestar.com", "subscribestar.adult"])
}

/// Upstream `_pagination`: the posts of the user page, then of each
/// infinite-scroll JSON page.
fn pagination<'a>(
    http: &'a Http,
    route: &'static Route,
    root: &'static str,
    first: String,
    extra: Map<String, Value>,
) -> Events<'a> {
    crate::sites::paginate(Some((first, true)), move |state| {
        let extra = extra.clone();
        async move {
            let Some((url, first)) = state else {
                return Ok(None);
            };
            let page = if first {
                request(http, &url).await?
            } else {
                let data = http.json(&http.api(&url)?).await?;
                super::str_of(&data, "html").to_owned()
            };
            let posts: Vec<String> = page
                .split("<div class=\"post ")
                .skip(1)
                .map(str::to_owned)
                .collect();
            let Some(last) = posts.last() else {
                return Ok(None);
            };
            let next = extr(
                last,
                "data-role=\"infinite_scroll-next_page\" href=\"",
                "\"",
            );
            let next = (!next.is_empty()).then(|| (format!("{root}{}", unescape(next)), false));
            let events = stream::iter(posts)
                .map(move |html| post_events(http, route, root, &html, false, &extra))
                .flatten()
                .boxed();
            Ok(Some((events, next)))
        }
    })
}

fn post_events<'a>(
    http: &'a Http,
    route: &'static Route,
    root: &str,
    html: &str,
    single: bool,
    extra: &Map<String, Value>,
) -> Events<'a> {
    let items = parse_post(html, root, single, extra)
        .into_iter()
        .map(|item| super::session_cookies(http, item))
        .collect();
    crate::sites::events(crate::sites::post(&SITE, http, route, items))
}

/// The pieces after each `class="NAME"` or `class="NAME ` attribute, as
/// upstream's `text.re(r'class="NAME[" ]').split(...)[1:]`.
fn split_class<'t>(text: &'t str, name: &str) -> Vec<&'t str> {
    let needle = format!("class=\"{name}");
    let mut starts = Vec::new();
    let mut from = 0;
    while let Some(found) = text[from..].find(&needle) {
        let end = from + found + needle.len();
        if matches!(text.as_bytes().get(end), Some(b'"' | b' ')) {
            starts.push((from + found, end + 1));
        }
        from = end;
    }
    starts
        .iter()
        .enumerate()
        .map(|(i, &(_, start))| {
            let stop = starts.get(i + 1).map_or(text.len(), |&(next, _)| next);
            &text[start..stop]
        })
        .collect()
}

/// Upstream `_media_from_post`: gallery items without previews, then
/// attachments and audio uploads.
fn media(html: &str) -> Vec<Map<String, Value>> {
    let mut media = Vec::new();
    let gallery = extr(html, "data-gallery=\"", "\"");
    if !gallery.is_empty()
        && let Ok(Value::Array(items)) = serde_json::from_str::<Value>(&unescape(gallery))
    {
        for item in items {
            if let Value::Object(item) = item
                && !text(&item, "url").contains("/previews")
            {
                media.push(item);
            }
        }
    }
    let uploads = [
        (
            "uploads-docs",
            "doc_preview",
            "doc_preview-title\">",
            "href=\"",
            "attachment",
        ),
        (
            "uploads-audios",
            "audio_preview-data",
            "audio_preview-title\">",
            "src=\"",
            "audio",
        ),
    ];
    for (section, class, title, link, kind) in uploads {
        let section = extr(
            html,
            &format!("class=\"{section}\""),
            "class=\"post-edit_form\"",
        );
        for upload in split_class(section, class) {
            let mut item = Map::new();
            item.insert(
                "id".into(),
                parse_int(extr(upload, "data-upload-id=\"", "\""), 0).into(),
            );
            item.insert("name".into(), unescape(extr(upload, title, "<")).into());
            item.insert("url".into(), unescape(extr(upload, link, "\"")).into());
            item.insert("type".into(), kind.into());
            media.push(item);
        }
    }
    media
}

fn text<'m>(map: &'m Map<String, Value>, key: &str) -> &'m str {
    map.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// Upstream `_parse_datetime`.
fn parse_date(text: &str) -> Option<i64> {
    let text = text.strip_prefix("Updated on ").unwrap_or(text);
    super::strptime(text, "%b %d, %Y %I:%M %p")
}

/// Upstream `_data_from_post` of the feed (`single = false`) or post page.
fn post_data(html: &str, single: bool) -> (Map<String, Value>, Option<i64>) {
    let mut scan = Scan::new(html);
    let mut data = Map::new();
    let int = |data: &mut Map<String, Value>, key: &str, text: &str| {
        data.insert(key.into(), parse_int(text, 0).into());
    };
    int(&mut data, "post_id", scan.extr("data-id=\"", "\""));
    let content_begin = "<div class=\"post-content\" data-role=\"post_content-text\">";
    let content_end = "</div><div class=\"post-uploads for-youtube\"";
    let tags = |text: &str| -> Value {
        extract_iter(text, "?tag=", "\"")
            .into_iter()
            .map(Value::from)
            .collect::<Vec<_>>()
            .into()
    };
    let timestamp;
    if single {
        timestamp = parse_date(scan.extr("<div class=\"section-title_date\">", "<"));
        data.insert("date".into(), super::date(timestamp));
        let content = scan.extr(content_begin, content_end).trim();
        data.insert("content".into(), content.into());
        let tag_html = scan.extr(
            "<div class=\"post_tags for-post\">",
            "<div class=\"post-actions\">",
        );
        data.insert("tags".into(), tags(tag_html));
        let name = unescape(scan.extr("class=\"star_link\" href=\"/", "\""));
        data.insert("author_name".into(), name.into());
        int(&mut data, "author_id", scan.extr("data-user-id=\"", "\""));
        data.insert(
            "author_nick".into(),
            unescape(scan.extr("alt=\"", "\"")).into(),
        );
    } else {
        int(&mut data, "author_id", scan.extr("data-user-id=\"", "\""));
        data.insert(
            "author_name".into(),
            unescape(scan.extr("href=\"/", "\"")).into(),
        );
        data.insert("author_nick".into(), unescape(scan.extr(">", "<")).into());
        let date = scan.extr("class=\"post-date\">", "</");
        timestamp = parse_date(date.rsplit('>').next().unwrap_or_default());
        data.insert("date".into(), super::date(timestamp));
        let content = scan.extr(content_begin, content_end).trim();
        data.insert("content".into(), content.into());
        let tag_html = scan.extr(
            "<div class=\"post_tags for-post\">",
            "<div class=\"post-actions\">",
        );
        data.insert("tags".into(), tags(tag_html));
    }
    (data, timestamp)
}

/// Upstream `items` for one post's HTML.
pub(crate) fn parse_post(
    html: &str,
    root: &str,
    single: bool,
    extra: &Map<String, Value>,
) -> Vec<Item> {
    let media = media(html);
    let (mut data, timestamp) = post_data(html, single);
    let mut content = text(&data, "content").to_owned();
    if content.contains("<html><body>") {
        content = extr(&content, "<body>", "</body>").to_owned();
        data.insert("content".into(), content.clone().into());
    }
    let title = unescape(crate::sites::manga::text::rextr(&content, "<h1>", "</h1>"));
    data.insert("title".into(), title.clone().into());
    super::update(&mut data, extra);
    let post_id = data
        .get("post_id")
        .map(crate::sites::scalar)
        .unwrap_or_default();
    let author = data
        .get("author_name")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let mut items = Vec::new();
    for (index, mut item) in media.into_iter().enumerate() {
        super::update(&mut item, &data);
        item.insert("num".into(), (index + 1).into());
        let raw = text(&item, "url").to_owned();
        let name = ["name", "original_filename"]
            .iter()
            .find_map(|key| {
                item.get(*key)
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
            })
            .map(str::to_owned);
        match name {
            Some(name) => {
                let (stem, ext) = super::nameext_from_name(&name);
                item.insert("filename".into(), stem.into());
                item.insert("extension".into(), ext.into());
            }
            None => super::nameext_into(&raw, &mut item),
        }
        let Some(url) = super::file_url(&raw, root).filter(|_| !raw.is_empty()) else {
            continue;
        };
        let id = item.get("id").map(crate::sites::scalar).unwrap_or_default();
        items.push(
            Item::new(url, item)
                .ids(post_id.clone(), id)
                .user(author.clone())
                .caption(Some(title.clone()))
                .timestamp(timestamp),
        );
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, json, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.subscribestar.com/subscribestar"),
            Some(Target::User(false, "subscribestar".into(), None))
        );
        assert_eq!(
            t("https://subscribestar.adult/kanashiipanda?tag=art&x=1"),
            Some(Target::User(
                true,
                "kanashiipanda".into(),
                Some(vec![("tag".into(), "art".into()), ("x".into(), "1".into())])
            ))
        );
        assert_eq!(
            t("https://subscribestar.com/user/extra?tag=x"),
            Some(Target::User(false, "user".into(), None))
        );
        assert_eq!(
            t("https://www.subscribestar.com/posts/102468"),
            Some(Target::Post(false, "102468".into()))
        );
        assert_eq!(
            t("https://www.subscribestar.adult/posts/22950"),
            Some(Target::Post(true, "22950".into()))
        );
        for raw in [
            "https://www.subscribestar.com/",
            "https://www.subscribestar.com/posts/abc",
            "https://subscribestar.net/user",
            "https://www.subscribestar.com.evil.com/user",
            "https://evilsubscribestar.com/user",
            "https://a.subscribestar.com/user",
            "https://www.subscribestar.com:8443/user",
            "https://u:p@www.subscribestar.com/user",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    const FEED: &str = r#"<div class="posts">
<div class="post is-shown" data-id="101" data-user-id="5"><a href="/artist">Artist &amp; Co</a>
<div class="post-date"><a href="/posts/101">Jan 2, 2020 03:04 PM</a></div>
<div class="post-content" data-role="post_content-text"><html><body><h1>First &amp; title</h1><p>hi</p></body></html></div><div class="post-uploads for-youtube">
<div data-gallery="[{&quot;id&quot;:7,&quot;url&quot;:&quot;https://ss-uploads.subscribestar.adult/uploads/a.jpg&quot;,&quot;original_filename&quot;:&quot;pic.JPG&quot;},{&quot;id&quot;:8,&quot;url&quot;:&quot;https://x.test/previews/x.jpg&quot;}]"></div>
<div class="uploads-docs"><div class="doc_preview" data-upload-id="9"><a href="/post_uploads?payload=1&amp;x=2"><div class="doc_preview-title">notes.pdf</div></a></div></div><div class="post-edit_form"></div>
<div class="post_tags for-post"><a href="/artist?tag=art">art</a></div><div class="post-actions"></div></div>
<div class="post is-shown" data-id="102" data-user-id="5"><a href="/artist">Artist</a>
<div data-role="infinite_scroll-next_page" href="/artist?page=2&amp;x=1"></div></div>"#;

    #[tokio::test]
    async fn extracts_user_feed_pages() {
        let more = r#"<div class="post " data-id="103" data-user-id="5"><a href="/artist">Artist</a>
            <div data-gallery="[{&quot;id&quot;:10,&quot;url&quot;:&quot;https://ss-uploads.subscribestar.adult/uploads/b.png&quot;}]"></div></div>"#;
        let (root, server) = serve(vec![html(FEED), json(json!({"html": more}))]);
        let http = http("subscribestar", root.clone());
        http.set_cookie(&root, "_personalization_id=p; Path=/");
        let files = media(extract(
            &http,
            Url::parse("https://subscribestar.adult/artist?tag=art").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 3);
        let k = keywords(&files[0]);
        assert_eq!(k["subcategory"], "user-adult");
        assert_eq!(k["id"], 7);
        assert_eq!(k["post_id"], 101);
        assert_eq!(k["author_id"], 5);
        assert_eq!(k["author_name"], "artist");
        assert_eq!(k["author_nick"], "Artist & Co");
        assert_eq!(k["date"], "2020-01-02 15:04:00");
        assert_eq!(k["content"], "<h1>First &amp; title</h1><p>hi</p>");
        assert_eq!(k["title"], "First & title");
        assert_eq!(k["tags"], json!(["art"]));
        assert_eq!(k["search_tags"], "art");
        assert_eq!(k["filename"], "pic");
        assert_eq!(k["extension"], "jpg");
        let k = keywords(&files[1]);
        assert_eq!(k["type"], "attachment");
        assert_eq!(k["num"], 2);
        assert_eq!(
            files[1].url.as_str(),
            "https://subscribestar.adult/post_uploads?payload=1&x=2"
        );
        assert_eq!(
            files[1].headers.get("Cookie").map(String::as_str),
            Some("_personalization_id=p")
        );
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "101_9.pdf");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("subscribestar/artist")
        );
        let (_, identity) = crate::sites::archive_key(&files[2]).unwrap();
        assert_eq!(identity, r#"["{id}",["10"]]"#);
        assert_eq!(keywords(&files[2])["post_id"], 103);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /artist?tag=art "));
        assert!(requests[1].starts_with("GET /artist?page=2&x=1 "));
    }

    #[tokio::test]
    async fn extracts_post_page_after_html_redirect() {
        let redirect = r#"<html><body>You are being <a href="https://www.subscribestar.com/posts/201?r=1">redirected</a>.</body></html>"#;
        let page = r#"<div class="post" data-id="201"><div class="section-title_date">Updated on Feb 3, 2021 10:00 AM</div>
<div class="post-content" data-role="post_content-text"><p>text</p></div><div class="post-uploads for-youtube">
<div class="uploads-audios"><div class="audio_preview-data" data-upload-id="11"><div class="audio_preview-title">song.mp3</div><audio src="https://ss-uploads.subscribestar.com/audio/s.mp3?t=1"></audio></div></div><div class="post-edit_form"></div>
<a class="star_link" href="/artist"></a><span data-user-id="5"><img alt="Artist &amp; Co"></span></div>"#;
        let (root, server) = serve(vec![html(redirect), html(page)]);
        let http = http("subscribestar", root);
        let files = media(extract(
            &http,
            Url::parse("https://subscribestar.com/posts/201").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let k = keywords(&files[0]);
        assert_eq!(k["subcategory"], "post");
        assert_eq!(k["type"], "audio");
        assert_eq!(k["date"], "2021-02-03 10:00:00");
        assert_eq!(k["author_name"], "artist");
        assert_eq!(k["author_id"], 5);
        assert_eq!(k["author_nick"], "Artist & Co");
        assert_eq!(k["title"], "");
        assert_eq!(k["filename"], "song");
        let (file, _) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "201_11.mp3");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /posts/201 "));
        assert!(requests[1].starts_with("GET /posts/201?r=1 "));
    }
}
