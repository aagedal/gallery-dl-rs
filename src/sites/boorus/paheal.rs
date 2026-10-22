//! rule34.paheal.net tag searches and posts, mirroring
//! gallery_dl/extractor/paheal.py by scraping the Shimmie2 HTML pages.
use super::{
    Extr, below, date_value, extract_iter, float_value, iso_timestamp, leading_digits, parse_bytes,
    parse_float, parse_int, strptime, unquote,
};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use std::collections::VecDeque;
use url::Url;

const ROOT: &str = "https://rule34.paheal.net";
const HOSTS: &[&str] = &["rule34.paheal.net", "rule63.paheal.net", "cosplay.paheal.net"];
const COOKIE: (&str, &str) = ("Cookie", "ui-tnc-agreed=true");
const FILENAME: &str = "{category}_{id}_{md5}.{extension}";

pub static SITE: Site = Site {
    category: "paheal",
    name: "rule34.paheal.net",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&TAG, &POST],
    api_key: false,
};

static TAG: Route = Route::new("tag", FILENAME, &["{category}", "{search_tags}"], "{id}");
static POST: Route = Route::new("post", FILENAME, &["{category}"], "{id}");

#[derive(Debug, PartialEq)]
enum Target {
    /// Raw (still percent-encoded) tag group.
    Tag(String),
    Post(String),
}

fn target(url: &Url) -> Option<Target> {
    let raw = below(url, HOSTS)?;
    if let Some(tail) = raw.strip_prefix("/post/list/") {
        let end = tail.find(['/', '?', '#']).unwrap_or(tail.len());
        return (end > 0).then(|| Target::Tag(tail[..end].to_owned()));
    }
    let tail = raw.strip_prefix("/post/view/")?;
    leading_digits(tail).map(|id| Target::Post(id.to_owned()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Tag(raw)) => tag(http, raw),
        Some(Target::Post(id)) => crate::sites::once(async move {
            let post = match fetch_post(http, &id).await {
                Err(e) if not_found(&e) => return Ok(crate::sites::events(Vec::new())),
                other => other?,
            };
            Ok(crate::sites::events(items(
                http,
                &POST,
                vec![post],
                &Map::new(),
            )))
        }),
        None => crate::sites::fail(error("unsupported paheal URL")),
    }
}

/// HTTP 404, which upstream treats as the end of results.
fn not_found(error: &crate::Error) -> bool {
    error.to_string().ends_with("resource not found")
}

fn page_url(http: &Http, path: &str) -> Result<Url> {
    let mut url = http.api(ROOT)?;
    url.set_path(path);
    Ok(url)
}

async fn fetch_post(http: &Http, id: &str) -> Result<Map<String, Value>> {
    let url = page_url(http, &format!("/post/view/{id}"))?;
    let page = http.text_with(&url, &[COOKIE]).await?;
    parse_post(id, &page)
}

enum Step {
    Page(u64),
    /// Posts still to fetch (`metadata` option), then the next page.
    Posts {
        pids: VecDeque<String>,
        next: Option<u64>,
    },
}

fn tag(http: &Http, raw: String) -> Events<'_> {
    let mut data = Map::new();
    data.insert("search_tags".into(), unquote(&raw).into());
    let metadata = http.option_bool("metadata", false);
    crate::sites::paginate(Step::Page(1), move |mut step| {
        let raw = raw.clone();
        let data = data.clone();
        async move {
            loop {
                match step {
                    Step::Posts { mut pids, next } => {
                        if let Some(pid) = pids.pop_front() {
                            let post = fetch_post(http, &pid).await?;
                            let events = items(http, &TAG, vec![post], &data);
                            return Ok(Some((
                                crate::sites::events(events),
                                Step::Posts { pids, next },
                            )));
                        }
                        match next {
                            Some(pnum) => step = Step::Page(pnum),
                            None => return Ok(None),
                        }
                    }
                    Step::Page(pnum) => {
                        let url = page_url(http, &format!("/post/list/{raw}/{pnum}"))?;
                        let page = match http.text_with(&url, &[COOKIE]).await {
                            Err(e) if not_found(&e) => return Ok(None),
                            other => other?,
                        };
                        if let Some(pid) = redirected_post(&page) {
                            // Upstream follows the redirect to a single
                            // post; Http already followed it.
                            let post = parse_post(&pid, &page)?;
                            let events = items(http, &TAG, vec![post], &data);
                            let done = Step::Posts {
                                pids: VecDeque::new(),
                                next: None,
                            };
                            return Ok(Some((crate::sites::events(events), done)));
                        }
                        let chunks = match page.find("id='image-list'") {
                            Some(pos) => extract_iter(&page[pos..], "<img id='thumb_", "Only</a>"),
                            None => Vec::new(),
                        };
                        let next = (page.contains(">Next<") && pnum < 100_000).then_some(pnum + 1);
                        if metadata {
                            let pids = chunks
                                .iter()
                                .map(|chunk| chunk.split('\'').next().unwrap_or_default().to_owned())
                                .collect();
                            step = Step::Posts { pids, next };
                            continue;
                        }
                        let posts = chunks
                            .into_iter()
                            .map(parse_thumb)
                            .collect::<Result<Vec<_>>>()?;
                        let events = items(http, &TAG, posts, &data);
                        let step = Step::Posts {
                            pids: VecDeque::new(),
                            next,
                        };
                        return Ok(Some((crate::sites::events(events), step)));
                    }
                }
            }
        }
    })
}

/// The post id when a tag search was redirected to a single `post/view`
/// page: such pages lack the image list but show a main image. The id is
/// read from the page title (`Post 12345: tags`).
fn redirected_post(page: &str) -> Option<String> {
    if page.contains("id='image-list'")
        || !(page.contains("id='main_image'") || page.contains("<source src='"))
    {
        return None;
    }
    let head = &page[..page.find(": ")?];
    let start = head
        .bytes()
        .rposition(|b| !b.is_ascii_digit())
        .map_or(0, |p| p + 1);
    let id = &head[start..];
    (!id.is_empty() && id.len() <= 20).then(|| id.to_owned())
}

fn without_last(text: &str) -> &str {
    let mut chars = text.chars();
    chars.next_back();
    chars.as_str()
}

/// `(dimensions, size, ext)` of a ` // `-separated info string.
fn info(text: &str) -> Result<(&str, &str, &str)> {
    let parts: Vec<&str> = text.split(" // ").collect();
    match parts.as_slice() {
        [dimensions, size, ext] => Ok((dimensions, size, ext)),
        _ => Err(error("unexpected paheal post layout")),
    }
}

fn dimension_fields(post: &mut Map<String, Value>, dimensions: &str, size: &str) {
    let (width, height) = dimensions.split_once('x').unwrap_or((dimensions, ""));
    let (height, duration) = height.split_once(", ").unwrap_or((height, ""));
    post.insert("width".into(), width.into());
    post.insert("height".into(), height.into());
    post.insert(
        "duration".into(),
        float_value(parse_float(without_last(duration))),
    );
    post.insert("size".into(), parse_bytes(without_last(size)).into());
}

/// `text.extract`: the text between `begin` and `end` from `pos`, and the
/// position after `end`.
fn text_extract<'t>(text: &'t str, begin: &str, end: &str, pos: usize) -> Option<(&'t str, usize)> {
    let first = pos + text.get(pos..)?.find(begin)? + begin.len();
    let last = first + text[first..].find(end)?;
    Some((&text[first..last], last + end.len()))
}

/// `PahealTagExtractor._extract_data` for one thumbnail chunk.
fn parse_thumb(chunk: &str) -> Result<Map<String, Value>> {
    let layout = || error("unexpected paheal post list layout");
    let (pid, pos) = text_extract(chunk, "", "'", 0).ok_or_else(layout)?;
    let (data, pos) = text_extract(chunk, "title='", "'", pos).ok_or_else(layout)?;
    let (url, _) = text_extract(chunk, "<a href='", "'", pos).ok_or_else(layout)?;
    let lines: Vec<&str> = data.split('\n').collect();
    let [tags, data, date] = lines.as_slice() else {
        return Err(layout());
    };
    let (dimensions, size, ext) = info(data)?;
    let timestamp = strptime(date, "%B %d, %Y; %H:%M");
    let mut post = Map::new();
    post.insert("id".into(), pid.into());
    post.insert("md5".into(), url[url.rfind('/').map_or(0, |p| p + 1)..].into());
    post.insert("file_url".into(), url.into());
    dimension_fields(&mut post, dimensions, size);
    post.insert("tags".into(), crate::sites::unescape(tags).into());
    post.insert("date".into(), date_value(timestamp));
    post.insert("_ts".into(), timestamp.map_or(Value::Null, Value::from));
    post.insert("filename".into(), format!("{pid} - {tags}").into());
    post.insert("extension".into(), (*ext).into());
    Ok(post)
}

/// `PahealExtractor._extract_post` for a fetched `post/view` page.
fn parse_post(id: &str, page: &str) -> Result<Map<String, Value>> {
    let mut extr = Extr::new(page);
    let tags = extr.next(": ", "<");
    let mut file_url = extr.next("id='main_image' src='", "'");
    if file_url.is_empty() {
        file_url = extr.next("<source src='", "'");
    }
    let uploader = unquote(extr.next("class='username' href='/user/", "'"));
    let timestamp = iso_timestamp(extr.next("datetime='", "'"));
    let source = crate::sites::unescape(
        crate::sites::extr(extr.next(">Source Link<", "</td>"), "href='", "'").unwrap_or_default(),
    );
    let (dimensions, size, ext) = info(extr.next("Info</th><td>", "<"))?;
    let mut post = Map::new();
    post.insert("id".into(), id.into());
    post.insert("tags".into(), tags.into());
    post.insert("file_url".into(), file_url.into());
    post.insert("uploader".into(), uploader.into());
    post.insert("date".into(), date_value(timestamp));
    post.insert("_ts".into(), timestamp.map_or(Value::Null, Value::from));
    post.insert("source".into(), source.into());
    post.insert(
        "md5".into(),
        super::rpartition(file_url, "/").1.into(),
    );
    dimension_fields(&mut post, dimensions, size);
    post.insert("filename".into(), format!("{id} - {tags}").into());
    post.insert("extension".into(), ext.into());
    Ok(post)
}

/// `PahealExtractor.items` for a batch of scraped posts.
fn items(
    http: &Http,
    route: &'static Route,
    posts: Vec<Map<String, Value>>,
    data: &Map<String, Value>,
) -> Vec<crate::event::ExtractionEvent> {
    let mut out = Vec::new();
    for mut post in posts {
        let text = |post: &Map<String, Value>, key: &str| {
            post.get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        let timestamp = post.remove("_ts").and_then(|v| v.as_i64());
        for key in ["id", "width", "height"] {
            let value = parse_int(&text(&post, key));
            post.insert(key.into(), value.into());
        }
        let tags = unquote(&text(&post, "tags"));
        post.insert("tags".into(), tags.into());
        for (key, value) in data {
            post.insert(key.clone(), value.clone());
        }
        let Some(url) = super::file_url(ROOT, &text(&post, "file_url")) else {
            continue;
        };
        out.push(super::item(url, post, "uploader", "tags", timestamp));
    }
    super::per_post(&SITE, http, route, out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, response, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://rule34.paheal.net/post/list/TAG/1"),
            Some(Target::Tag("TAG".into()))
        );
        assert_eq!(
            t("https://cosplay.paheal.net/post/list/a%20b"),
            Some(Target::Tag("a%20b".into()))
        );
        assert_eq!(
            t("http://rule63.paheal.net/post/view/12345"),
            Some(Target::Post("12345".into()))
        );
        assert_eq!(t("https://www.rule34.paheal.net/post/view/1"), None);
        assert_eq!(t("https://paheal.net/post/view/1"), None);
        assert_eq!(t("https://rule34.paheal.net.evil.test/post/view/1"), None);
        assert_eq!(t("https://rule34.paheal.net:81/post/view/1"), None);
        assert_eq!(t("https://rule34.paheal.net/post/view/x"), None);
        assert_eq!(t("https://rule34.paheal.net/post/list/"), None);
    }

    fn thumb(id: u32) -> String {
        format!(
            "<a href='/post/view/{id}' class='shm-thumb'><img id='thumb_{id}' \
             title='tag{id} a&amp;b\n640x480, 12.5s // 1.5MB // webm\nJuly 14, 2017; 02:40' \
             alt=''></a><br><a href='https://r34i.paheal-cdn.net/ab/cd/md5{id}'>File Only</a>"
        )
    }

    fn list(ids: &[u32], next: bool) -> String {
        let mut page = String::from("<div id='image-list'>");
        for id in ids {
            page.push_str(&thumb(*id));
        }
        if next {
            page.push_str("<a href='/post/list/x/2'>Next</a>");
        }
        page
    }

    #[tokio::test]
    async fn paginates_tag_search() {
        let (root, server) = serve(vec![
            html(&list(&[1, 2], true)),
            html(&list(&[3], false)),
        ]);
        let http = http("paheal", root);
        let files = media(extract(
            &http,
            Url::parse("https://rule34.paheal.net/post/list/a%20b/1").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 3);
        let k = keywords(&files[0]);
        assert_eq!(k["id"], 1);
        assert_eq!(k["md5"], "md51");
        assert_eq!(k["tags"], "tag1 a&b");
        assert_eq!(k["width"], 640);
        assert_eq!(k["height"], 480);
        assert_eq!(k["duration"], 12.5);
        assert_eq!(k["size"], 1_572_864);
        assert_eq!(k["date"], "2017-07-14 02:40:00");
        assert_eq!(k["filename"], "1 - tag1 a&amp;b");
        assert_eq!(k["extension"], "webm");
        assert_eq!(k["search_tags"], "a b");
        assert_eq!(files[0].metadata.as_ref().unwrap().timestamp, Some(1_500_000_000));
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "paheal_1_md51.webm");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("paheal/a b")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{id}",["1"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /post/list/a%20b/1 "), "{}", requests[0]);
        assert!(requests[0].to_ascii_lowercase().contains("cookie: ui-tnc-agreed=true"));
        assert!(requests[1].starts_with("GET /post/list/a%20b/2 "));
    }

    const POST_PAGE: &str = "<html><title>Post 77: tag1 tag2</title>\
        <img alt='main image' class='shm-main-image' id='main_image' src='https://r34i.paheal-cdn.net/ab/cd/deadbeef'>\
        <a class='username' href='/user/Some%20One'>Some One</a>\
        <time datetime='2017-07-14T02:40:00+00:00'>x</time>\
        <tr><th>Source Link</th><td><a href='https://example.org/?a=1&amp;b=2'>link</a></td></tr>\
        <tr><th>Info</th><td>1920x1080 // 2MB // jpg</td></tr></html>";

    #[tokio::test]
    async fn parses_post_page() {
        let (root, server) = serve(vec![html(POST_PAGE)]);
        let http = http("paheal", root);
        let files = media(extract(
            &http,
            Url::parse("https://rule34.paheal.net/post/view/77").unwrap(),
        ))
        .await;
        let k = keywords(&files[0]);
        assert_eq!(k["id"], 77);
        assert_eq!(k["tags"], "tag1 tag2");
        assert_eq!(k["uploader"], "Some One");
        assert_eq!(k["source"], "https://example.org/?a=1&b=2");
        assert_eq!(k["md5"], "deadbeef");
        assert_eq!(k["width"], 1920);
        assert_eq!(k["height"], 1080);
        assert_eq!(k["duration"], 0.0);
        assert_eq!(k["size"], 2_097_152);
        assert_eq!(k["date"], "2017-07-14 02:40:00");
        assert_eq!(k["filename"], "77 - tag1 tag2");
        assert_eq!(k["extension"], "jpg");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "paheal_77_deadbeef.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("paheal")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /post/view/77 "));
    }

    #[tokio::test]
    async fn missing_post_and_tag_end_quietly() {
        let (root, _server) = serve(vec![
            response("404 Not Found", "text/html", b""),
            response("404 Not Found", "text/html", b""),
        ]);
        let http = http("paheal", root);
        let post = media(extract(
            &http,
            Url::parse("https://rule34.paheal.net/post/view/1").unwrap(),
        ))
        .await;
        assert!(post.is_empty());
        let tag = media(extract(
            &http,
            Url::parse("https://rule34.paheal.net/post/list/x/1").unwrap(),
        ))
        .await;
        assert!(tag.is_empty());
    }

    #[tokio::test]
    async fn redirected_tag_yields_single_post() {
        let (root, _server) = serve(vec![html(POST_PAGE)]);
        let http = http("paheal", root);
        let files = media(extract(
            &http,
            Url::parse("https://rule34.paheal.net/post/list/tag1").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let k = keywords(&files[0]);
        assert_eq!(k["id"], 77);
        assert_eq!(k["search_tags"], "tag1");
    }
}
