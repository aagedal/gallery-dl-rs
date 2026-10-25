//! Motherless media, galleries and groups, mirroring
//! gallery_dl/extractor/motherless.py. Gallery and group listings are
//! read page by page, and each thumbnail's media page is fetched for its
//! file and metadata; one post per file.
use crate::sites::art::{Scan, date, extr, extract_iter, parse_int, strptime};
use crate::sites::manga::text::partition_last;
use crate::sites::{Events, Http, Item, Route, Site, remove_html, unescape};
use crate::{Result, error};
use futures_util::{StreamExt, stream};
use serde_json::{Map, Value};
use std::collections::VecDeque;
use url::Url;

const ROOT: &str = "https://motherless.com";
const FILENAME: &str = "{id} {title}.{extension}";

static MEDIA: Route = Route::new("media", FILENAME, &["{category}"], "{id}");
static GALLERY: Route = Route::new(
    "gallery",
    FILENAME,
    &["{category}", "{uploader}", "{gallery_id} {gallery_title}"],
    "{gallery_id}_{id}",
);
static GROUP: Route = Route::new(
    "group",
    FILENAME,
    &["{category}", "{uploader}", "{group_id} {group_title}"],
    "{group_id}_{id}",
);

pub static SITE: Site = Site {
    category: "motherless",
    name: "Motherless",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&MEDIA, &GALLERY, &GROUP],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    /// Media path, with an optional gallery or group prefix.
    Media(String),
    /// Type (`I`, `V`, `G` or none) and gallery ID.
    Gallery(Option<char>, String),
    /// Type (`i`, `v` or none) and group ID.
    Group(Option<char>, String),
}

fn upper(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// The `[A-Z0-9]+` prefix of `text`.
fn upper_prefix(text: &str) -> &str {
    let end = text
        .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit()))
        .unwrap_or(text.len());
    &text[..end]
}

fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) || url.host_str() != Some("motherless.com") {
        return None;
    }
    let path = &url.path()[1..];
    // `$` anchors gallery and group URLs at the very end.
    let bare = url.query().is_none() && url.fragment().is_none();
    // `((?:g/[^/?#]+/|G[IV]?[A-Z0-9]+/)?(?!G)[A-Z0-9]+)`
    let prefix = if let Some(rest) = path.strip_prefix("g/") {
        rest.split_once('/')
            .filter(|(name, _)| !name.is_empty())
            .map(|(name, _)| name.len() + 3)
    } else if let Some(rest) = path.strip_prefix('G') {
        let rest = rest.strip_prefix(['I', 'V']).unwrap_or(rest);
        rest.split_once('/')
            .filter(|(id, _)| upper(id))
            .map(|(id, _)| path.len() - rest.len() + id.len() + 1)
    } else {
        None
    };
    for start in prefix.into_iter().chain([0]) {
        let media = upper_prefix(&path[start..]);
        if !media.is_empty() && !media.starts_with('G') {
            return Some(Target::Media(path[..start + media.len()].to_owned()));
        }
    }
    if !bare {
        return None;
    }
    let path = path.strip_suffix('/').unwrap_or(path);
    if let Some(rest) = path.strip_prefix('G') {
        return match rest.chars().next() {
            Some(kind @ ('I' | 'V' | 'G')) if upper(&rest[1..]) => {
                Some(Target::Gallery(Some(kind), rest[1..].to_owned()))
            }
            _ => upper(rest).then(|| Target::Gallery(None, rest.to_owned())),
        };
    }
    let rest = path.strip_prefix('g')?;
    let group = |id: &str| {
        (!id.is_empty()
            && id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'))
        .then(|| id.to_owned())
    };
    if let Some(kind @ ('i' | 'v')) = rest.chars().next() {
        let id = &rest[1..];
        if let Some(id) = group(id.strip_prefix('/').unwrap_or(id)) {
            return Some(Target::Group(Some(kind), id));
        }
    }
    group(rest.strip_prefix('/').unwrap_or(rest)).map(|id| Target::Group(None, id))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Media(path)) => crate::sites::once(async move {
            let (data, url) = media(http, &path, true).await?;
            let id = path.rsplit('/').next().unwrap_or_default().to_owned();
            let items = url
                .map(|url| Item::new(url, data).ids(id.clone(), id))
                .into_iter()
                .collect();
            Ok(crate::sites::events(crate::sites::post(
                &SITE, http, &MEDIA, items,
            )))
        }),
        Some(Target::Gallery(None, id)) => queue_both(
            http,
            &GALLERY,
            [format!("{ROOT}/GI{id}"), format!("{ROOT}/GV{id}")],
        ),
        Some(Target::Group(None, id)) => queue_both(
            http,
            &GROUP,
            [format!("{ROOT}/gi/{id}"), format!("{ROOT}/gv/{id}")],
        ),
        Some(Target::Gallery(Some(kind), id)) => {
            listing(http, &GALLERY, format!("{ROOT}/G{kind}{id}"), id)
        }
        Some(Target::Group(Some(kind), id)) => {
            listing(http, &GROUP, format!("{ROOT}/g{kind}/{id}"), id)
        }
        None => crate::sites::fail(error("unsupported Motherless URL")),
    }
}

fn queue_both<'a>(http: &'a Http, route: &'static Route, urls: [String; 2]) -> Events<'a> {
    let children: Vec<Url> = urls.iter().filter_map(|u| Url::parse(u).ok()).collect();
    stream::iter(children)
        .map(move |child| {
            crate::sites::queue(http, SITE.category, route.subcategory, child, &Map::new())
        })
        .flatten()
        .boxed()
}

/// One page; the extractor's first request also fails on the site's
/// error page.
async fn fetch(http: &Http, url: &str, first: bool) -> Result<String> {
    let page = http.text(&http.api(url)?).await?;
    if first
        && (page.contains("<div class=\"error-page")
            || page.contains(">The page you're looking for cannot be found.<"))
    {
        return Err(error("Motherless page not found"));
    }
    Ok(page)
}

/// A page `<title>` as `t[:t.rfind(" | ")]`, where a miss drops the last
/// character like Python's `-1` index.
fn title(raw: &str) -> String {
    let end = raw
        .rfind(" | ")
        .unwrap_or_else(|| raw.char_indices().last().map_or(0, |(i, _)| i));
    unescape(&raw[..end])
}

/// `_extract_media`: the media page's keywords and file URL.
async fn media(http: &Http, path: &str, first: bool) -> Result<(Map<String, Value>, Option<Url>)> {
    let page = fetch(http, &format!("{ROOT}/{path}"), first).await?;
    let mut scan = Scan::new(&page);
    let (prefix, _, id) = partition_last(path, "/");
    let mut data = Map::new();
    data.insert("id".into(), id.into());
    data.insert("title".into(), title(scan.extr("<title>", "<")).into());
    data.insert("type".into(), scan.extr("__mediatype = '", "'").into());
    data.insert("group".into(), scan.extr("__group = '", "'").into());
    let url = scan.extr("__fileurl = '", "'").to_owned();
    data.insert("url".into(), url.clone().into());
    let tags: Vec<String> =
        extract_iter(scan.extr("class=\"media-meta-tags\">", "</div>"), ">#", "<")
            .into_iter()
            .map(unescape)
            .collect();
    data.insert("tags".into(), tags.into());
    let mut count = || parse_int(&scan.extr("class=\"count\">", " ").replace(',', ""), 0);
    let views = count();
    let favorites = count();
    data.insert("views".into(), views.into());
    data.insert("favorites".into(), favorites.into());
    let posted = parse_datetime(scan.extr("class=\"count\">", "<"));
    data.insert("date".into(), date(posted));
    data.insert(
        "uploader".into(),
        unescape(scan.extr("class=\"username\">", "<").trim()).into(),
    );
    if let Some(gallery) = prefix.strip_prefix('G') {
        data.insert("gallery_id".into(), gallery.into());
        data.insert("gallery_title".into(), gallery_title(gallery, &page).into());
    } else if prefix.starts_with('g') {
        let group = prefix.get(2..).unwrap_or_default();
        data.insert("group_id".into(), group.into());
        data.insert("group_title".into(), group_title(&page).into());
    }
    crate::sites::art::nameext_into(&url, &mut data);
    Ok((data, crate::sites::media::web_url(&url)))
}

/// `_parse_datetime`: `%d  %b  %Y`, or `N[h|d] ago` as that day's midnight.
fn parse_datetime(text: &str) -> Option<i64> {
    if !text.contains(" ago") {
        return strptime(text, "%d  %b  %Y");
    }
    let chars: Vec<char> = text.chars().collect();
    let cut = chars.len().checked_sub(5)?;
    let value: String = chars[..cut].iter().collect();
    let value = parse_int(&value, 0);
    let delta = if chars[cut] == 'h' {
        value * 3600
    } else {
        value * 86400
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    let then = now - delta;
    Some(then - then.rem_euclid(86400))
}

fn gallery_title(id: &str, page: &str) -> String {
    let heading = extr(
        extr(page, "<h1 class=\"content-title\">", "</h1>"),
        "From the gallery:",
        "<",
    );
    if !heading.is_empty() {
        return unescape(heading.trim());
    }
    if page.contains(&format!(" href=\"/G{id}\"")) {
        return title(extr(page, "<title>", "<"));
    }
    String::new()
}

fn group_title(page: &str) -> String {
    let title = extr(
        extr(page, "<h1 class=\"group-bio-name\">", "</h1>"),
        ">",
        "<",
    );
    unescape(title.trim())
}

/// `_extract_data`: the listing's ID, title, creator and count.
fn listing_data(page: &str, category: &str, id: &str) -> Map<String, Value> {
    let title = if category == "gallery" {
        gallery_title(id, page)
    } else {
        group_title(page)
    };
    let mut scan = Scan::new(page);
    let creator = remove_html(scan.extr(&format!("class=\"{category}-member-username\">"), "</"));
    let count = scan.extr("<span class=\"active\">", ")");
    let count = partition_last(count, "(").2.replace(',', "");
    let mut data = Map::new();
    data.insert(format!("{category}_id"), id.into());
    data.insert(format!("{category}_title"), title.into());
    data.insert(format!("{category}_creator"), creator.clone().into());
    data.insert("uploader".into(), creator.into());
    data.insert("count".into(), parse_int(&count, 0).into());
    data
}

struct State {
    data: Map<String, Value>,
    /// Thumbnail codenames and images not yet fetched.
    thumbs: VecDeque<(String, String)>,
    next: Option<String>,
    num: usize,
}

/// Gallery and group listings: one media page per thumbnail, pages
/// followed through `rel="next"`.
fn listing<'a>(http: &'a Http, route: &'static Route, url: String, id: String) -> Events<'a> {
    let category = route.subcategory;
    crate::sites::once(async move {
        let page = fetch(http, &url, true).await?;
        let data = listing_data(&page, category, &id);
        let state = State {
            data,
            thumbs: thumbs(&page),
            next: next_page(&page),
            num: 0,
        };
        Ok(crate::sites::paginate(state, move |mut state| {
            let id = id.clone();
            async move {
                if let Some((codename, thumbnail)) = state.thumbs.pop_front() {
                    state.num += 1;
                    let (mut file, url) = media(http, &codename, false).await?;
                    let uploader = file.get("uploader").cloned().unwrap_or(Value::Null);
                    crate::sites::art::update(&mut file, &state.data);
                    file.insert("num".into(), state.num.into());
                    file.insert("thumbnail".into(), thumbnail.into());
                    file.insert("uploader".into(), uploader.clone());
                    if category == "group" {
                        let group = file.get("group_id").cloned().unwrap_or(Value::Null);
                        file.insert("group".into(), group);
                    }
                    let items = url
                        .map(|url| {
                            Item::new(url, file)
                                .ids(id.clone(), codename)
                                .user(uploader.as_str().map(str::to_owned))
                        })
                        .into_iter()
                        .collect();
                    let events = crate::sites::post(&SITE, http, route, items);
                    return Ok(Some((crate::sites::events(events), state)));
                }
                let Some(next) = state.next.take() else {
                    return Ok(None);
                };
                let page = fetch(http, &next, false).await?;
                state.thumbs = thumbs(&page);
                state.next = next_page(&page);
                Ok(Some((crate::sites::events(Vec::new()), state)))
            }
        }))
    })
}

/// `_parse_thumb_data`: each thumbnail's codename and static image.
fn thumbs(page: &str) -> VecDeque<(String, String)> {
    extract_iter(page, "class=\"thumb-container", "</div>")
        .into_iter()
        .map(|thumb| {
            let mut scan = Scan::new(thumb);
            let id = scan.extr("data-codename=\"", "\"").to_owned();
            let _type = scan.extr("data-mediatype=\"", "\"");
            let thumbnail = scan.extr("class=\"static\" src=\"", "\"").to_owned();
            (id, thumbnail)
        })
        .collect()
}

/// The next listing page, on the site itself.
fn next_page(page: &str) -> Option<String> {
    let raw = unescape(extr(page, "<link rel=\"next\" href=\"", "\""));
    let url = Url::parse(&raw).ok()?;
    (crate::sites::plain(&url) && url.host_str() == Some("motherless.com")).then_some(raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::manga::tests::{archive, path};
    use crate::sites::testing::{html, http, keywords, media as files, queued, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://motherless.com/ABC123"),
            Some(Target::Media("ABC123".into()))
        );
        assert_eq!(
            t("https://motherless.com/GIABC/DEF?x"),
            Some(Target::Media("GIABC/DEF".into()))
        );
        assert_eq!(
            t("https://motherless.com/g/some_group/A1B2"),
            Some(Target::Media("g/some_group/A1B2".into()))
        );
        assert_eq!(
            t("https://motherless.com/GABC123"),
            Some(Target::Gallery(None, "ABC123".into()))
        );
        assert_eq!(
            t("https://motherless.com/GV12/"),
            Some(Target::Gallery(Some('V'), "12".into()))
        );
        assert_eq!(
            t("https://motherless.com/g/abc_1"),
            Some(Target::Group(None, "abc_1".into()))
        );
        assert_eq!(
            t("https://motherless.com/gi/abc"),
            Some(Target::Group(Some('i'), "abc".into()))
        );
        for raw in [
            "https://motherless.com/GABC123?page=2",
            "https://motherless.com/term/x",
            "https://www.motherless.com/ABC123",
            "https://motherless.com.evil.test/ABC123",
            "https://motherless.com:8080/ABC123",
            "https://u:p@motherless.com/ABC123",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn media_page(id: &str) -> String {
        format!(
            r#"<title>Sunny &amp; Bright | Motherless.com</title>
<script>__mediatype = 'image'; __group = ''; __fileurl = 'https://cdn5-images.motherlessmedia.test/images/{id}.jpg';</script>
<div class="media-meta-tags"><a>#sun</a><a>#sky</a></div>
<span class="count">1,234 Views</span><span class="count">56 Favorites</span><span class="count">12  Jan  2024</span>
<a class="username">uploader1 </a>"#
        )
    }

    #[tokio::test]
    async fn extracts_media() {
        let (root, server) = serve(vec![html(&media_page("ABC123"))]);
        let http = http("motherless", root);
        let files = files(extract(
            &http,
            Url::parse("https://motherless.com/ABC123").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let k = keywords(&files[0]);
        assert_eq!(k["title"], "Sunny & Bright");
        assert_eq!(k["type"], "image");
        assert_eq!(k["tags"], json!(["sun", "sky"]));
        assert_eq!(k["views"], 1234);
        assert_eq!(k["favorites"], 56);
        assert_eq!(k["date"], "2024-01-12 00:00:00");
        assert_eq!(k["uploader"], "uploader1");
        assert_eq!(path(&files[0]), "motherless/ABC123 Sunny & Bright.jpg");
        assert_eq!(archive(&files[0]), r#"["{id}",["ABC123"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /ABC123 "));
    }

    #[tokio::test]
    async fn error_page_is_not_found() {
        let (root, server) = serve(vec![html("<div class=\"error-page\">")]);
        let http = http("motherless", root);
        assert!(
            crate::sites::testing::fails(extract(
                &http,
                Url::parse("https://motherless.com/ABC123").unwrap(),
            ))
            .await
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn extracts_gallery_pages() {
        let first = r#"<h1 class="content-title">Trip</h1><title>Trip | Motherless.com</title>
<a href="/GX1">x</a><div class="gallery-member-username"><a>maker</a></div>
<span class="active">Images (1,002)</span>
<div class="thumb-container" data-codename="AAA" data-mediatype="image"><img class="static" src="https://thumbs.test/thumb/AAA.jpg"></div>
<link rel="next" href="https://motherless.com/GIX1?page=2&amp;x=1">"#;
        let second = r#"<div class="thumb-container" data-codename="BBB" data-mediatype="image"><img class="static" src="https://thumbs.test/thumb/BBB.jpg"></div>"#;
        let (root, server) = serve(vec![
            html(first),
            html(&media_page("AAA")),
            html(second),
            html(&media_page("BBB")),
        ]);
        let http = http("motherless", root);
        let files = files(extract(
            &http,
            Url::parse("https://motherless.com/GIX1").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[1]);
        assert_eq!(k["gallery_id"], "X1");
        assert_eq!(k["gallery_title"], "Trip");
        assert_eq!(k["gallery_creator"], "maker");
        assert_eq!(k["count"], 1002);
        assert_eq!(k["num"], 2);
        assert_eq!(k["uploader"], "uploader1");
        assert_eq!(k["thumbnail"], "https://thumbs.test/thumb/BBB.jpg");
        assert_eq!(
            path(&files[1]),
            "motherless/uploader1/X1 Trip/BBB Sunny & Bright.jpg"
        );
        assert_eq!(archive(&files[1]), r#"["{gallery_id}_{id}",["X1","BBB"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /GIX1 "));
        assert!(requests[1].starts_with("GET /AAA "));
        assert!(requests[2].starts_with("GET /GIX1?page=2&x=1 "));
    }

    #[tokio::test]
    async fn untyped_group_queues_images_and_videos() {
        let (root, _server) = serve(vec![]);
        let http = http("motherless", root);
        let children = queued(extract(
            &http,
            Url::parse("https://motherless.com/g/abc").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].subcategory(), "group");
    }
}
