//! Postmill forums, mirroring gallery_dl/extractor/postmill.py for its
//! built-in instance (Raddle): submissions, short links, the front page,
//! forums, user submissions, tags and searches.
//!
//! Text posts become `text:` documents (link posts too with the
//! `save-link-post-body` option); image posts download their image, and
//! other links are queued when a native extractor or the direct-link
//! fallback supports them.
use crate::sites::art::{
    Scan, date, digits, extr, extract_iter, file_url, leading_digits, nameext_into, parse_int,
    parse_iso, parse_query, url as site_url,
};
use crate::sites::{Events, Http, Item, Route, Site, unescape};
use crate::{Result, error};
use futures_util::{StreamExt, stream};
use serde_json::{Map, Value};
use url::{Position, Url};

const FILENAME: &str = "{id}_{title[:220]}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{instance}", "{forum}"];
const ARCHIVE: &str = "{filename}";
static POST: Route = Route::new("post", FILENAME, DIRECTORY, ARCHIVE);
static SHORTURL: Route = Route::new("shorturl", FILENAME, DIRECTORY, ARCHIVE);
static HOME: Route = Route::new("home", FILENAME, DIRECTORY, ARCHIVE);
static FORUM: Route = Route::new("forum", FILENAME, DIRECTORY, ARCHIVE);
static USERSUBMISSIONS: Route = Route::new("usersubmissions", FILENAME, DIRECTORY, ARCHIVE);
static TAG: Route = Route::new("tag", FILENAME, DIRECTORY, ARCHIVE);
static SEARCH: Route = Route::new("search", FILENAME, DIRECTORY, ARCHIVE);

/// Raddle's clearnet and onion hosts.
const RADDLE: &[&str] = &[
    "raddle.me",
    "c32zjeghcp5tj3kb72pltz56piei66drc63vkhn5yixiyk4cmerrjtid.onion",
];

pub static RADDLE_SITE: Site = Site {
    category: "raddle",
    name: "Raddle",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[
        &POST,
        &SHORTURL,
        &HOME,
        &FORUM,
        &USERSUBMISSIONS,
        &TAG,
        &SEARCH,
    ],
    api_key: false,
    cookies: Some("raddle.me"),
};

const SORTINGS: &[&str] = &[
    "hot",
    "new",
    "active",
    "top",
    "controversial",
    "most_commented",
];

#[derive(Debug, PartialEq)]
enum Target {
    Post(String, String),
    ShortUrl(String),
    /// A listing's route, its base path, sorting path and query.
    Submissions(&'static str, String, String, String),
}

/// Python `\w+` on a raw (ASCII) path segment.
fn word(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// The instance root (`text.root_from_url`) and the extractor's target.
fn target(url: &Url) -> Option<(String, Target)> {
    if !crate::sites::plain(url) || !RADDLE.contains(&url.host_str()?) {
        return None;
    }
    let root = url[..Position::BeforePath].to_owned();
    let path = url.path();
    let query = url.query();
    let segments: Vec<&str> = path.split('/').skip(1).collect();
    // Upstream's `$`-anchored patterns: nothing may follow but the query.
    let anchored = url.fragment().is_none();
    let sorting = |rest: &[&str]| match rest {
        [] => Some(String::new()),
        [sort] if SORTINGS.contains(sort) => Some(format!("/{sort}")),
        _ => None,
    };
    let listing = |route: &'static Route, base: String, rest: &[&str]| {
        let sort = sorting(rest).filter(|_| anchored)?;
        Some(Target::Submissions(
            route.subcategory,
            base,
            sort,
            query.unwrap_or_default().to_owned(),
        ))
    };
    let target = match segments.as_slice() {
        ["f", forum, id, ..] if word(forum) && leading_digits(id).is_some() => {
            let id = leading_digits(id)?;
            Target::Post((*forum).into(), id.into())
        }
        [id] if digits(id) && query.is_none() && anchored => Target::ShortUrl(format!("/{id}")),
        [first, rest @ ..] if matches!(*first, "" | "featured" | "subscribed" | "all") => {
            listing(&HOME, format!("/{first}"), rest)?
        }
        ["f", forum, rest @ ..] if word(forum) => listing(&FORUM, format!("/f/{forum}"), rest)?,
        ["user", user, "submissions"] if word(user) && anchored => Target::Submissions(
            USERSUBMISSIONS.subcategory,
            format!("/user/{user}/submissions"),
            String::new(),
            query.unwrap_or_default().to_owned(),
        ),
        ["tag", tag, rest @ ..] if word(tag) => listing(&TAG, format!("/tag/{tag}"), rest)?,
        ["search"] if anchored && query.is_some_and(|q| q.starts_with("q=") && q.len() > 2) => {
            Target::Submissions(
                SEARCH.subcategory,
                "/search".into(),
                String::new(),
                query?.to_owned(),
            )
        }
        _ => return None,
    };
    Some((root, target))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some((root, kind)) = target(&url) else {
        return crate::sites::fail(error("unsupported Raddle URL"));
    };
    match kind {
        Target::Post(forum, id) => crate::sites::once(async move {
            let post_url = format!("{root}/f/{forum}/{id}");
            let page = http.text(&http.api(&post_url)?).await?;
            let instance = root.split_once("://").map_or("", |(_, host)| host);
            let save_body = http.option_bool("save-link-post-body", false);
            let (items, link) = parse_post(&page, instance, save_body, &root)?;
            let files = crate::sites::events(crate::sites::post(&RADDLE_SITE, http, &POST, items));
            Ok(match link {
                Some((child, data)) => files
                    .chain(crate::sites::queue(http, "raddle", "post", child, &data))
                    .boxed(),
                None => files,
            })
        }),
        Target::ShortUrl(path) => crate::sites::once(async move {
            let short = format!("{root}{path}");
            let request = http.api(&short)?;
            let Some(location) = http.location(&request, &[]).await? else {
                return Ok(crate::sites::events(Vec::new()));
            };
            // Same-host redirects resolve against the instance itself.
            let full = if location.host_str() == request.host_str() {
                site_url(&short)?
                    .join(&location[Position::BeforePath..])
                    .map_err(|_| error("invalid Raddle redirect"))?
            } else {
                location
            };
            Ok(match target(&full) {
                Some(_) => crate::sites::queue(http, "raddle", "shorturl", full, &Map::new()),
                None => crate::sites::events(Vec::new()),
            })
        }),
        Target::Submissions(subcategory, base, sorting, query) => {
            submissions(http, root, subcategory, base, sorting, &query)
        }
    }
}

/// Upstream `PostmillSubmissionsExtractor.items`: queue every listed
/// submission, following `rel="next"` links.
fn submissions<'a>(
    http: &'a Http,
    root: String,
    subcategory: &'static str,
    base: String,
    sorting: String,
    query: &str,
) -> Events<'a> {
    let whitelist: &[&str] = if subcategory == "search" { &["q"] } else { &[] };
    let params: Vec<(String, String)> = parse_query(query)
        .into_iter()
        .filter(|(key, _)| {
            whitelist.contains(&key.as_str())
                || key == "t"
                || (key.starts_with("next[") && key.ends_with(']'))
        })
        .collect();
    let first = format!("{root}{base}{sorting}");
    crate::sites::paginate(Some(first), move |url| {
        let (root, params) = (root.clone(), params.clone());
        async move {
            let Some(url) = url else {
                return Ok(None);
            };
            let mut request = http.api(&url)?;
            if !params.is_empty() {
                request.query_pairs_mut().extend_pairs(&params);
            }
            let (location, page) = http.page(&request, &[]).await?;
            if location.path() == "/login" && location != request {
                return Err(error("Raddle redirected to its login page"));
            }
            let base = site_url(&url)?;
            let children: Vec<Url> =
                extract_iter(&page, "<nav class=\"submission__nav\">", "</nav>")
                    .into_iter()
                    .filter_map(|nav| base.join(&unescape(extr(nav, "<a href=\"", "\""))).ok())
                    .filter(|child| matches!(target(child), Some((_, Target::Post(..)))))
                    .collect();
            let next = unescape(extr(&page, "<link rel=\"next\" href=\"", "\">"));
            let next = (!next.is_empty())
                .then(|| base.join(&next).ok())
                .flatten()
                .filter(|next| next.as_str().starts_with(&root))
                .map(String::from);
            let events = stream::iter(children)
                .map(move |child| {
                    crate::sites::queue(http, "raddle", subcategory, child, &Map::new())
                })
                .flatten()
                .boxed();
            Ok(Some((events, next)))
        }
    })
}

/// Upstream `_search_image_tag`: `<a href="…"\n +class="submission__image-link"`.
fn has_image_link(page: &str) -> bool {
    page.match_indices("class=\"submission__image-link\"")
        .any(|(index, _)| {
            let before = &page[..index];
            let trimmed = before.trim_end_matches(' ');
            let Some(line) = trimmed.strip_suffix('\n') else {
                return false;
            };
            if trimmed.len() == before.len() {
                return false;
            }
            let Some(head) = line.strip_suffix('"') else {
                return false;
            };
            head.rfind("<a href=\"").is_some_and(|start| {
                let href = &head[start + "<a href=\"".len()..];
                !href.is_empty() && !href.contains('"')
            })
        })
}

/// One submission page: its files, and the link to queue for link posts.
#[allow(clippy::type_complexity)]
pub(crate) fn parse_post(
    page: &str,
    instance: &str,
    save_body: bool,
    root: &str,
) -> Result<(Vec<Item>, Option<(Url, Map<String, Value>)>)> {
    let mut scan = Scan::new(page);
    let title = unescape(scan.extr("<meta property=\"og:title\" content=\"", "\">"));
    let published = scan.extr(
        "<meta property=\"og:article:published_time\" content=\"",
        "\">",
    );
    let timestamp = parse_iso(published);
    let username = scan
        .extr("<meta property=\"og:article:author\" content=\"", "\">")
        .to_owned();
    let canonical = unescape(scan.extr("<link rel=\"canonical\" href=\"", "\">"));
    let url = unescape(scan.extr(
        "<h1 class=\"submission__title unheaderize inline\"><a href=\"",
        "\"",
    ));
    let body = scan.extr(
        "<div class=\"submission__body break-text text-flow\">",
        "</div>",
    );
    // `/f/([\w\d_]+)/(\d+)/`
    let (forum, id) = canonical
        .split("/f/")
        .skip(1)
        .find_map(|rest| {
            let (forum, rest) = rest.split_once('/')?;
            let (id, _) = rest.split_once('/')?;
            (word(forum) && digits(id)).then(|| (forum.to_owned(), id.to_owned()))
        })
        .ok_or_else(|| error("Raddle submission has no canonical URL"))?;
    let is_text = url.starts_with('/');
    let is_image = has_image_link(page);
    let flair: Vec<Value> = extract_iter(page, "<span class=\"flair__label\">", "</span>")
        .into_iter()
        .map(|label| Value::from(unescape(label)))
        .collect();
    let mut data = Map::new();
    data.insert("title".into(), title.clone().into());
    data.insert("date".into(), date(timestamp));
    data.insert("username".into(), username.clone().into());
    data.insert("forum".into(), forum.into());
    data.insert("id".into(), parse_int(&id, 0).into());
    data.insert("flair".into(), flair.into());
    data.insert("instance".into(), instance.into());
    let text = is_text || save_body;
    let link = !is_text && !is_image;
    let count = usize::from(text) + usize::from(!is_text);
    data.insert("count".into(), count.into());
    let mut items = Vec::new();
    let mut num = 0;
    if text {
        num += 1;
        let (text_url, job) = crate::extract::inline_text(body.to_owned())?;
        let mut fields = data.clone();
        fields.insert("num".into(), num.into());
        fields.insert("filename".into(), "".into());
        fields.insert("extension".into(), "htm".into());
        items.push(Item::new(text_url, fields).job(job));
    }
    let mut queued = None;
    if !is_text {
        num += 1;
        let mut fields = data.clone();
        fields.insert("num".into(), num.into());
        nameext_into(&url, &mut fields);
        if let Some(file) = file_url(&url, root) {
            if link {
                queued = crate::sites::supported(&file).then_some((file, fields));
            } else {
                items.push(Item::new(file, fields));
            }
        }
    }
    let items = items
        .into_iter()
        .map(|item| {
            let num = item.keywords.get("num").map(crate::sites::scalar);
            item.ids(id.clone(), num.unwrap_or_default())
                .user(Some(username.clone()))
                .caption(Some(title.clone()))
                .timestamp(timestamp)
        })
        .collect();
    Ok((items, queued))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, queued, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap()).map(|(_, t)| t);
        assert_eq!(
            t("https://raddle.me/f/Anime/150283/title"),
            Some(Target::Post("Anime".into(), "150283".into()))
        );
        assert_eq!(
            t("https://raddle.me/150283"),
            Some(Target::ShortUrl("/150283".into()))
        );
        let listing = |sub: &'static str, base: &str, sort: &str, query: &str| {
            Some(Target::Submissions(
                sub,
                base.into(),
                sort.into(),
                query.into(),
            ))
        };
        assert_eq!(t("https://raddle.me/"), listing("home", "/", "", ""));
        assert_eq!(
            t("https://raddle.me/all/new?t=week"),
            listing("home", "/all", "/new", "t=week")
        );
        assert_eq!(
            t("https://raddle.me/f/Anime/top"),
            listing("forum", "/f/Anime", "/top", "")
        );
        assert_eq!(
            t("https://raddle.me/user/Sam/submissions?next[id]=1"),
            listing("usersubmissions", "/user/Sam/submissions", "", "next[id]=1")
        );
        assert_eq!(
            t("https://raddle.me/tag/Art"),
            listing("tag", "/tag/Art", "", "")
        );
        assert_eq!(
            t("https://raddle.me/search?q=x"),
            listing("search", "/search", "", "q=x")
        );
        let onion = "http://c32zjeghcp5tj3kb72pltz56piei66drc63vkhn5yixiyk4cmerrjtid.onion/f/a/1";
        assert_eq!(
            target(&Url::parse(onion).unwrap()).map(|(root, _)| root),
            Some("http://c32zjeghcp5tj3kb72pltz56piei66drc63vkhn5yixiyk4cmerrjtid.onion".into())
        );
        for raw in [
            "https://raddle.me/hot",
            "https://raddle.me/f/Anime/other",
            "https://raddle.me/123?x",
            "https://raddle.me/search?x=1",
            "https://raddle.me/user/Sam",
            "https://www.raddle.me/f/a/1",
            "https://raddle.me.evil.com/f/a/1",
            "https://raddle.me:8080/f/a/1",
            "https://u:p@raddle.me/f/a/1",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn page(link: &str, image: bool) -> String {
        let anchor = if image {
            format!("<a href=\"{link}\"\n   class=\"submission__image-link\">")
        } else {
            String::new()
        };
        format!(
            r#"<meta property="og:title" content="A &amp; B">
<meta property="og:article:published_time" content="2020-01-02T03:04:05+00:00">
<meta property="og:article:author" content="Sam">
<link rel="canonical" href="https://raddle.me/f/Art/42/a-b">
<h1 class="submission__title unheaderize inline"><a href="{link}">A</a></h1>
{anchor}
<div class="submission__body break-text text-flow"><p>Body</p></div>
<span class="flair__label">Good &amp; nice</span>"#
        )
    }

    #[tokio::test]
    async fn extracts_text_post() {
        let (root, server) = serve(vec![html(&page("/f/Art/42/a-b", false))]);
        let http = http("Raddle", root);
        let files = media(extract(
            &http,
            Url::parse("https://raddle.me/f/Art/42").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let crate::extract::MediaJob::InlineText { data } = &files[0].job else {
            panic!("text post is not inline text");
        };
        assert_eq!(data, "<p>Body</p>");
        let k = keywords(&files[0]);
        assert_eq!(k["title"], "A & B");
        assert_eq!(k["username"], "Sam");
        assert_eq!(k["forum"], "Art");
        assert_eq!(k["id"], 42);
        assert_eq!(k["flair"], serde_json::json!(["Good & nice"]));
        assert_eq!(k["instance"], "raddle.me");
        assert_eq!(k["date"], "2020-01-02 03:04:05");
        assert_eq!(k["extension"], "htm");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "42_A & B.htm");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("raddle/raddle.me/Art")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /f/Art/42 "));
    }

    #[tokio::test]
    async fn image_and_link_posts() {
        let (root, server) = serve(vec![
            html(&page("https://raddle.me/submission_images/abc.png", true)),
            html(&page("https://www.youtube.com/watch?v=x", false)),
        ]);
        let http = http("Raddle", root);
        let files = media(extract(
            &http,
            Url::parse("https://raddle.me/f/Art/42").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].url.as_str(),
            "https://raddle.me/submission_images/abc.png"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["count"], 1);
        assert_eq!(k["filename"], "abc");
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{filename}",["abc"]]"#);
        let children = queued(extract(
            &http,
            Url::parse("https://raddle.me/f/Art/42").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].subcategory(), "post");
        server.join().unwrap();
    }

    #[tokio::test]
    async fn listings_follow_next_links_and_short_urls_redirect() {
        let first = r#"<nav class="submission__nav"><a href="/f/Art/1/x">x</a></nav>
            <nav class="submission__nav"><a href="/f/Art/2/y">y</a></nav>
            <link rel="next" href="https://raddle.me/f/Art/new?next%5Bid%5D=2">"#;
        let second =
            r#"<nav class="submission__nav"><a href="https://raddle.me/f/Art/3/z">z</a></nav>"#;
        let (root, server) = serve(vec![
            html(first),
            html(second),
            b"HTTP/1.1 302 Found\r\nLocation: /f/Art/9/t\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_vec(),
        ]);
        let http = http("Raddle", root);
        let children = queued(extract(
            &http,
            Url::parse("https://raddle.me/f/Art/new?t=all&x=1").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 3);
        assert_eq!(children[0].subcategory(), "forum");
        let children = queued(extract(&http, Url::parse("https://raddle.me/9").unwrap())).await;
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].subcategory(), "shorturl");
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("GET /f/Art/new?t=all "),
            "{}",
            requests[0]
        );
        assert!(
            requests[1].starts_with("GET /f/Art/new?next%5Bid%5D=2&t=all "),
            "{}",
            requests[1]
        );
        assert!(requests[2].starts_with("GET /9 "));
    }
}
