//! Bellazon forum posts, threads and forums, mirroring
//! gallery_dl/extractor/bellazon.py for the Invision Community board at
//! `www.bellazon.com/main`.
//!
//! Each post is a post boundary: the forum's own uploads and attachments
//! are files, and other links are queued when a native extractor or the
//! direct-link fallback supports them. Threads list their newest post first unless `order-posts` says
//! otherwise; quoted posts are skipped unless `quoted` is set.
use crate::sites::art::{
    Scan, date, ext_from_url, extr, extract_iter, leading_digits, nameext_from_name, on_host,
    parse_iso, parse_query, segment, split_html, str_of,
};
use crate::sites::social::xenforo::{jsonld, remove_quotes};
use crate::sites::{Events, Http, Item, Route, Site, unescape};
use crate::{Result, error};
use futures_util::{StreamExt, stream};
use serde_json::{Map, Value, json};
use url::Url;

const ROOT: &str = "https://www.bellazon.com/main";
const FILENAME: &str = "{post[id]}_{num:>02}_{id}_{filename}.{extension}";
const DIRECTORY: &[&str] = &[
    "{category}",
    "{thread[section]}",
    "{thread[title]} ({thread[id]})",
];
const ARCHIVE: &str = "{post[id]}/{id}_{filename}";
/// Pages followed at most in one direction.
const MAX_PAGES: i64 = 10_000;

static POST: Route = Route::new("post", FILENAME, DIRECTORY, ARCHIVE);
static THREAD: Route = Route::new("thread", FILENAME, DIRECTORY, ARCHIVE);
static FORUM: Route = Route::new("forum", FILENAME, DIRECTORY, ARCHIVE);

pub static SITE: Site = Site {
    category: "bellazon",
    name: "Bellazon",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&POST, &THREAD, &FORUM],
    api_key: false,
    cookies: Some("www.bellazon.com"),
};

#[derive(Debug, PartialEq)]
enum Target {
    /// Topic path (with an optional `/page/N`) and comment ID.
    Post(String, String),
    /// Topic path and page.
    Thread(String, Option<String>),
    /// Forum path and page.
    Forum(String, Option<String>),
}

fn target(url: &Url) -> Option<Target> {
    if !on_host(url, &["bellazon.com"]) {
        return None;
    }
    let rest = url.path().strip_prefix("/main/")?;
    let (kind, rest) = rest.split_once('/')?;
    if !matches!(kind, "topic" | "forum") {
        return None;
    }
    // `\d+-[^/?#]+`
    let name = &rest[..rest.find('/').unwrap_or(rest.len())];
    let id = leading_digits(name)?;
    if !name[id.len()..].strip_prefix('-').is_some_and(segment) {
        return None;
    }
    let path = format!("/{kind}/{name}");
    let tail = &rest[name.len()..];
    let page = tail
        .strip_prefix("/page/")
        .and_then(leading_digits)
        .map(str::to_owned);
    if kind == "topic"
        && let Some(comment) = url
            .fragment()
            .and_then(|f| {
                f.strip_prefix("findComment-")
                    .or(f.strip_prefix("comment-"))
            })
            .and_then(leading_digits)
    {
        // `(?:/page/\d+)?/?#`: nothing else may follow the page.
        let page_len = page.as_ref().map_or(0, |p| p.len() + 6);
        if matches!(&tail[page_len..], "" | "/") && url.query().is_none() {
            let path = format!("{path}{}", &tail[..page_len]);
            return Some(Target::Post(path, comment.to_owned()));
        }
    }
    Some(match kind {
        "topic" => Target::Thread(path, page),
        _ => Target::Forum(path, page),
    })
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let quoted = http.option_bool("quoted", false);
    match target(&url) {
        Some(Target::Post(path, id)) => crate::sites::once(async move {
            let page = http.text(&http.api(&format!("{ROOT}{path}"))?).await?;
            let marker = format!("id=\"elComment_{id}");
            let pos = page
                .find(&marker)
                .ok_or_else(|| error("Bellazon post not found"))?;
            let start = (0..=pos.saturating_sub(100))
                .rev()
                .find(|i| page.is_char_boundary(*i))
                .unwrap_or(0);
            let html = extr(&page[start..], "<article ", "</article>");
            let thread = parse_thread(&page)?;
            let post = parse_post(html);
            Ok(post_events(http, &POST, &thread, post, quoted))
        }),
        Some(Target::Thread(path, page)) => thread(http, path, page, quoted),
        Some(Target::Forum(path, page)) => forum(http, path, page),
        None => crate::sites::fail(error("unsupported Bellazon URL")),
    }
}

/// `BellazonThreadExtractor`: with `order-posts` ascending, `_pagination`
/// (a given page alone, else each page while `rel="next"` names another);
/// otherwise `_pagination_reverse` (the last page, found through the
/// `page/9999` redirect, then each earlier page), newest post first.
fn thread(http: &Http, path: String, page: Option<String>, quoted: bool) -> Events<'_> {
    let order = http.option_str("order-posts").unwrap_or_default();
    let ascending = !order.is_empty() && !order.starts_with(['d', 'r']);
    let base = format!("{ROOT}{path}");
    // Pages as (URL, page number known), in reading order.
    let first = if ascending {
        match &page {
            None => (format!("{base}/"), Some(1)),
            Some(pnum) => (format!("{base}/page/{pnum}/"), None),
        }
    } else {
        (
            format!("{base}/page/{}/", page.as_deref().unwrap_or("9999")),
            None,
        )
    };
    let state = Some((first.0, first.1, 0i64, None::<Value>));
    crate::sites::paginate(state, move |state| {
        let base = base.clone();
        async move {
            let Some((url, pnum, count, thread)) = state else {
                return Ok(None);
            };
            let (location, page) = http.page(&http.api(&url)?, &[]).await?;
            let thread = match thread {
                Some(thread) => thread,
                None => parse_thread(&page)?,
            };
            let mut posts: Vec<&str> = extract_iter(&page, "<article ", "</article>");
            let next = if ascending {
                // Follow only when the first page was not given.
                pnum.filter(|pnum| {
                    page.contains(" rel=\"next\" ")
                        && extr(&page, " rel=\"next\" data-page='", "'") != pnum.to_string()
                        && count < MAX_PAGES
                })
                .map(|pnum| (format!("{base}/page/{}/", pnum + 1), Some(pnum + 1)))
            } else {
                posts.reverse();
                // The page actually served, after redirects.
                let current = match pnum {
                    Some(pnum) => pnum,
                    None => {
                        let parts: Vec<&str> = location.path().rsplitn(4, '/').collect();
                        if parts.get(2) == Some(&"page") {
                            crate::sites::art::parse_int(parts[1], 0)
                        } else {
                            1
                        }
                    }
                };
                match current - 1 {
                    n if n > 1 => Some((format!("{base}/page/{n}/"), Some(n))),
                    1 => Some((format!("{base}/"), Some(1))),
                    _ => None,
                }
            };
            let events: Vec<Events<'_>> = posts
                .into_iter()
                .map(|html| post_events(http, &THREAD, &thread, parse_post(html), quoted))
                .collect();
            let next = next.map(|(url, pnum)| (url, pnum, count + 1, Some(thread)));
            Ok(Some((stream::iter(events).flatten().boxed(), next)))
        }
    })
}

/// `BellazonForumExtractor`: each listed topic queued, page by page.
fn forum(http: &Http, path: String, page: Option<String>) -> Events<'_> {
    let base = format!("{ROOT}{path}");
    let first = match &page {
        None => (format!("{base}/"), Some(1i64)),
        Some(pnum) => (format!("{base}/page/{pnum}/"), None),
    };
    crate::sites::paginate(Some(first), move |state| {
        let base = base.clone();
        async move {
            let Some((url, pnum)) = state else {
                return Ok(None);
            };
            let page = http.text(&http.api(&url)?).await?;
            let children: Vec<Url> = extract_iter(&page, "<li data-ips-hook=\"topicRow\"", "</")
                .into_iter()
                .filter_map(|row| Url::parse(extr(row, "href=\"", "\"")).ok())
                .filter(|url| matches!(target(url), Some(Target::Thread(..))))
                .collect();
            let next = pnum
                .filter(|pnum| {
                    page.contains(" rel=\"next\" ")
                        && extr(&page, " rel=\"next\" data-page='", "'") != pnum.to_string()
                        && *pnum < MAX_PAGES
                })
                .map(|pnum| (format!("{base}/page/{}/", pnum + 1), Some(pnum + 1)));
            let events = stream::iter(children)
                .map(move |child| {
                    crate::sites::queue(http, SITE.category, FORUM.subcategory, child, &Map::new())
                })
                .flatten()
                .boxed();
            Ok(Some((events, next)))
        }
    })
}

/// `_parse_thread`: the topic's JSON-LD and breadcrumb.
fn parse_thread(page: &str) -> Result<Value> {
    let schema = jsonld(page).ok_or_else(|| error("Bellazon topic has no metadata"))?;
    let author = &schema["author"];
    let stats = &schema["interactionStatistic"];
    let url_t = str_of(&schema, "url");
    let url_a = str_of(author, "url");
    let crumbs = split_html(extr(page, "<nav class=\"ipsBreadcrumb", "</nav>"));
    let path: Vec<String> = crumbs
        .get(2..crumbs.len().saturating_sub(1))
        .unwrap_or_default()
        .to_vec();
    let section = path
        .len()
        .checked_sub(2)
        .and_then(|i| path.get(i))
        .cloned()
        .unwrap_or_default();
    let (id, slug) = slug_id(url_t);
    let (author_id, author_slug) = slug_id(url_a);
    Ok(json!({
        "url": url_t,
        "path": path,
        "title": schema["headline"],
        "views": stats[0]["userInteractionCount"],
        "posts": stats[1]["userInteractionCount"],
        "date": date(parse_iso(str_of(&schema, "datePublished"))),
        "date_updated": date(parse_iso(str_of(&schema, "dateModified"))),
        "description": unescape(str_of(&schema, "text")).trim(),
        "section": section,
        "author": author["name"],
        "author_url": url_a,
        "id": id,
        "slug": crate::sites::boorus::unquote(&slug),
        "author_id": author_id,
        "author_slug": author_slug,
    }))
}

/// `url.rsplit("/", 2)[1].partition("-")`: the ID and slug.
fn slug_id(url: &str) -> (String, String) {
    let parts: Vec<&str> = url.rsplitn(3, '/').collect();
    let name = parts.get(1).copied().unwrap_or_default();
    let (id, slug) = name.split_once('-').unwrap_or((name, ""));
    (id.to_owned(), slug.to_owned())
}

/// `_parse_post`: ID, author, date and content without the signature.
fn parse_post(html: &str) -> Map<String, Value> {
    let mut scan = Scan::new(html);
    let id = scan.extr("id=\"elComment_", "\"");
    let author_url = scan.extr(" href='", "'");
    let posted = parse_iso(scan.extr("datetime='", "'"));
    let raw = scan.extr("<!-- Post content -->", "<menu data-ips-hook=");
    let beg = raw.find('>').map_or(0, |i| i + 1);
    let sig = raw.rfind("<div data-role=\"memberSignature\"");
    let window = match sig {
        Some(sig) => &raw[..(sig + 1).min(raw.len())],
        None => raw,
    };
    let content = match window.rfind("\n\t\t</div>") {
        Some(end) if end + 1 > beg => raw[beg..end + 1].trim(),
        Some(_) => "",
        None => "",
    };
    let (author_id, author_slug) = if author_url.is_empty() {
        (String::new(), String::new())
    } else {
        slug_id(author_url)
    };
    let mut post = Map::new();
    post.insert("id".into(), id.into());
    post.insert("author_url".into(), author_url.into());
    post.insert("date".into(), date(posted));
    post.insert("content".into(), content.into());
    post.insert("author_id".into(), author_id.into());
    post.insert("author_slug".into(), author_slug.into());
    post
}

/// One match of upstream's URL pattern: the whole tag text, a link or
/// video source, and an image source.
type Found = (String, String, String);

/// `(?s)<((?:video .*?<source [^>]*?src|a [^>]*?href)="([^"]+).*?</a>|img [^>]*?src="([^"]+)"[^>]*>)`
fn extract_urls(content: &str) -> Vec<Found> {
    let mut found = Vec::new();
    let mut pos = 0;
    while let Some(at) = content[pos..].find('<').map(|i| i + pos) {
        let text = &content[at + 1..];
        match match_video(text)
            .or_else(|| match_link(text))
            .or_else(|| match_img(text))
        {
            Some((len, groups)) => {
                found.push(groups);
                pos = at + 1 + len;
            }
            None => pos = at + 1,
        }
    }
    found
}

/// `ATTR="([^"]+)` inside the tag that starts `text`; returns the value's
/// end offset and the value.
fn attribute<'t>(text: &'t str, attr: &str) -> Option<(usize, &'t str)> {
    let tag_end = text.find('>').unwrap_or(text.len());
    let mut from = 0;
    while let Some(at) = text[from..tag_end].find(attr) {
        let start = from + at + attr.len();
        if let Some(len) = text[start..].find('"').filter(|len| *len > 0) {
            return Some((start + len, &text[start..start + len]));
        }
        from = start;
    }
    None
}

fn match_video(text: &str) -> Option<(usize, Found)> {
    text.strip_prefix("video ")?;
    let mut from = 0;
    while let Some(at) = text[from..].find("<source ") {
        let source = from + at;
        if let Some((end, value)) = attribute(&text[source..], "src=\"") {
            let close = text[source + end..].find("</a>")? + source + end + 4;
            return Some((
                close,
                (text[..close].to_owned(), value.to_owned(), String::new()),
            ));
        }
        from = source + 1;
    }
    None
}

fn match_link(text: &str) -> Option<(usize, Found)> {
    text.strip_prefix("a ")?;
    let (end, value) = attribute(text, "href=\"")?;
    let close = text[end..].find("</a>")? + end + 4;
    Some((
        close,
        (text[..close].to_owned(), value.to_owned(), String::new()),
    ))
}

fn match_img(text: &str) -> Option<(usize, Found)> {
    text.strip_prefix("img ")?;
    let (end, value) = attribute(text, "src=\"")?;
    let close = text[end..].find('>')? + end + 1;
    Some((
        close,
        (text[..close].to_owned(), String::new(), value.to_owned()),
    ))
}

/// `BellazonExtractor.items` for one post: its own uploads as files and
/// other links queued.
fn post_events<'a>(
    http: &'a Http,
    route: &'static Route,
    thread: &Value,
    mut post: Map<String, Value>,
    quoted: bool,
) -> Events<'a> {
    let content = str_of(&Value::Object(post.clone()), "content").to_owned();
    let urls = if quoted {
        extract_urls(&content)
    } else {
        extract_urls(&remove_quotes(&content))
    };
    post.insert("count".into(), urls.len().into());
    let post_id = post.get("id").map(crate::sites::scalar).unwrap_or_default();
    let timestamp = post.get("date").and_then(Value::as_str).and_then(parse_iso);
    let mut data = Map::new();
    data.insert("post".into(), Value::Object(post));
    data.insert("thread".into(), thread.clone());
    data.insert("count".into(), urls.len().into());
    let (mut num, mut internal, mut external) = (0, 0, 0);
    let mut items = Vec::new();
    let mut children = Vec::new();
    for (info, link, img) in urls {
        let url = if img.is_empty() {
            unescape(&link)
        } else {
            match extr(&info, "data-full-image=\"", "\"") {
                "" => unescape(&img),
                full => unescape(full),
            }
        };
        if url.starts_with("https://www.bellazon.com/main/")
            || url.starts_with("//www.bellazon.com/main/")
        {
            if [
                "/main/style_emoticons/",
                "/uploads/emoticons/",
                "/public/style_",
                "/profile/",
                "/topic/",
            ]
            .iter()
            .any(|skip| url.contains(skip))
            {
                continue;
            }
            num += 1;
            internal += 1;
            let mut dc = data.clone();
            dc.insert("num".into(), num.into());
            dc.insert("num_internal".into(), internal.into());
            dc.insert("num_external".into(), external.into());
            file_keywords(&info, &url, &mut dc);
            let absolute = match url.strip_prefix("//") {
                Some(rest) => format!("https://{rest}"),
                None => url,
            };
            if let Some(file) = crate::sites::media::web_url(&absolute) {
                items.push(
                    Item::new(file, dc)
                        .ids(post_id.clone(), num.to_string())
                        .timestamp(timestamp),
                );
            }
        } else {
            num += 1;
            external += 1;
            let Some(child) = crate::sites::media::web_url(&url) else {
                continue;
            };
            if !crate::sites::supported(&child) {
                continue;
            }
            let mut dc = data.clone();
            dc.insert("num".into(), num.into());
            dc.insert("num_internal".into(), internal.into());
            dc.insert("num_external".into(), external.into());
            children.push((child, dc));
        }
    }
    let files = crate::sites::events(crate::sites::post(&SITE, http, route, items));
    let queued = stream::iter(children)
        .map(move |(child, data)| {
            crate::sites::queue(http, SITE.category, route.subcategory, child, &data)
        })
        .flatten();
    files.chain(queued).boxed()
}

/// A forum upload's `name`, `filename`, `id` and `extension`.
fn file_keywords(info: &str, url: &str, dc: &mut Map<String, Value>) {
    let alt = extr(info, " alt=\"", "\"");
    if alt.is_empty() || (alt.starts_with("post-") && alt.contains("_thumb.")) {
        crate::sites::art::nameext_into(url, dc);
    } else {
        let name = unescape(alt);
        dc.insert(
            "filename".into(),
            name.split('.').next().unwrap_or_default().into(),
        );
        dc.insert("name".into(), name.into());
    }
    let mut id = extr(info, "data-fileid=\"", "\"").to_owned();
    let ext = extr(info, "data-fileext=\"", "\"");
    if !ext.is_empty() {
        dc.insert("extension".into(), ext.into());
    } else if url.contains("/core/interface/file/attachment.php")
        || url.contains("/main/index.php?")
    {
        if id.is_empty() {
            let params = parse_query(url.split_once('?').map_or("", |(_, q)| q));
            let get = |key: &str| {
                params
                    .iter()
                    .find(|(k, v)| k == key && !v.is_empty())
                    .map(|(_, v)| v.clone())
            };
            id = get("id").or_else(|| get("attach_id")).unwrap_or_default();
        }
        let name = extr(info, ">", "<").trim();
        if !name.is_empty() {
            let name = unescape(name);
            let (stem, ext) = nameext_from_name(&name);
            dc.insert("filename".into(), stem.into());
            dc.insert("extension".into(), ext.into());
            dc.insert("name".into(), name.into());
        }
    } else {
        dc.insert("extension".into(), ext_from_url(url).into());
    }
    dc.insert("id".into(), id.into());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::manga::tests::{archive, path};
    use crate::sites::testing::{html, http, keywords, media, queued, response, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.bellazon.com/main/topic/123-some-model/#findComment-456"),
            Some(Target::Post("/topic/123-some-model".into(), "456".into()))
        );
        assert_eq!(
            t("https://bellazon.com/main/topic/123-x/page/4/#comment-789"),
            Some(Target::Post("/topic/123-x/page/4".into(), "789".into()))
        );
        assert_eq!(
            t("https://www.bellazon.com/main/topic/123-x/page/2/"),
            Some(Target::Thread("/topic/123-x".into(), Some("2".into())))
        );
        assert_eq!(
            t("https://www.bellazon.com/main/forum/5-models/"),
            Some(Target::Forum("/forum/5-models".into(), None))
        );
        for raw in [
            "https://www.bellazon.com/main/topic/abc/",
            "https://www.bellazon.com/main/topic/123/",
            "https://www.bellazon.com/topic/123-x/",
            "https://www.bellazon.com.evil.test/main/topic/123-x/",
            "https://www.bellazon.com:8443/main/topic/123-x/",
            "https://u:p@www.bellazon.com/main/topic/123-x/",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn page(articles: &str) -> String {
        format!(
            r#"<script type="application/ld+json">{{"url": "https://www.bellazon.com/main/topic/123-some-model/",
"headline": "Some Model", "datePublished": "2020-01-02T03:04:05Z", "dateModified": "2021-01-02T03:04:05Z",
"text": " About &amp; more ", "author": {{"name": "starter", "url": "https://www.bellazon.com/main/profile/7-starter/"}},
"interactionStatistic": [{{"userInteractionCount": 100}}, {{"userInteractionCount": 5}}]}}</script>
<nav class="ipsBreadcrumb"><ul><li><a>Home</a></li><li><a>Forums</a></li><li><a>Female Fashion Models</a></li><li><a>Models</a></li><li><a>Some Model</a></li></ul></nav>
{articles}"#
        )
    }

    /// An external link no native extractor supports, which is skipped.
    const OTHER: &str = "https://unsupported.test/x";

    fn article(id: u64, external: &str) -> String {
        format!(
            "<article id=\"elComment_{id}\"><a href='https://www.bellazon.com/main/profile/8-poster/'>p</a>\
<time datetime='2020-02-03T04:05:06Z'></time><!-- Post content --><div class=\"c\">\
<p><img alt=\"post-1_thumb.jpg\" src=\"https://www.bellazon.com/main/uploads/monthly/a_thumb.jpg\" data-full-image=\"https://www.bellazon.com/main/uploads/monthly/a.jpg\" data-fileid=\"55\"></p>\
<blockquote><img src=\"https://www.bellazon.com/main/uploads/quoted.jpg\"></blockquote>\
<p><a href=\"{external}\">ext</a></p>\
<img src=\"https://www.bellazon.com/main/uploads/emoticons/smile.png\">\n\t\t</div>\
<div data-role=\"memberSignature\"><img src=\"https://www.bellazon.com/main/uploads/sig.jpg\"></div>\n\t\t</div>\
<menu data-ips-hook=\"x\"></menu></article>"
        )
    }

    #[tokio::test]
    async fn extracts_post_files() {
        let body = page(&format!("{}{}", article(455, OTHER), article(456, OTHER)));
        let (root, server) = serve(vec![html(&body)]);
        let http = http("bellazon", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.bellazon.com/main/topic/123-some-model/#findComment-456")
                .unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].url.as_str(),
            "https://www.bellazon.com/main/uploads/monthly/a.jpg"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["id"], "55");
        assert_eq!(k["filename"], "a");
        assert_eq!(k["extension"], "jpg");
        assert_eq!(k["num"], 1);
        assert_eq!(k["count"], 3);
        assert_eq!(k["post"]["id"], "456");
        assert_eq!(k["post"]["author_id"], "8");
        assert_eq!(k["post"]["date"], "2020-02-03 04:05:06");
        assert!(!k["post"]["content"].as_str().unwrap().contains("sig.jpg"));
        assert_eq!(k["thread"]["id"], "123");
        assert_eq!(k["thread"]["slug"], "some-model");
        assert_eq!(k["thread"]["section"], "Female Fashion Models");
        assert_eq!(k["thread"]["description"], "About & more");
        assert_eq!(k["thread"]["views"], 100);
        assert_eq!(
            path(&files[0]),
            "bellazon/Female Fashion Models/Some Model (123)/456_01_55_a.jpg"
        );
        assert_eq!(
            archive(&files[0]),
            r#"["{post[id]}/{id}_{filename}",["456","55","a"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /main/topic/123-some-model "));
    }

    #[tokio::test]
    async fn thread_reads_pages_newest_first() {
        let last = page(&format!("{}{}", article(3, OTHER), article(4, OTHER)));
        let first = page(&article(1, OTHER));
        let moved = response(
            "302 Found\r\nLocation: /main/topic/123-some-model/page/2/",
            "text/html",
            b"",
        );
        let (root, server) = serve(vec![moved, html(&last), html(&first)]);
        let http = http("bellazon", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.bellazon.com/main/topic/123-some-model/").unwrap(),
        ))
        .await;
        let posts: Vec<_> = files
            .iter()
            .map(|f| keywords(f)["post"]["id"].clone())
            .collect();
        assert_eq!(posts, ["4", "3", "1"]);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /main/topic/123-some-model/page/9999/ "));
        assert!(requests[1].starts_with("GET /main/topic/123-some-model/page/2/ "));
        assert!(requests[2].starts_with("GET /main/topic/123-some-model/ "));
    }

    #[tokio::test]
    async fn queues_external_links() {
        let (root, server) = serve(vec![html(&page(&article(
            9,
            "https://www.imagebam.com/view/MEABC9",
        )))]);
        let http = http("bellazon", root);
        let children = queued(extract(
            &http,
            Url::parse("https://www.bellazon.com/main/topic/123-some-model/#comment-9").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].subcategory(), "post");
        server.join().unwrap();
    }
}
