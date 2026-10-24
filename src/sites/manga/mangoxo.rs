//! Mangoxo albums and channels, mirroring gallery_dl/extractor/mangoxo.py by
//! scraping the paged album and channel listings.
//!
//! Upstream's `username`/`password` login is not supported; a `SESSION`
//! cookie from `--cookies` is sent instead. Without it the site shows at
//! most five images per album.
use super::{keywords, set, text};
use crate::sites::art::{Scan, extr, extract_iter, nameext_into, session_cookies, strptime};
use crate::sites::{Events, Http, Item, Route, Site, unescape};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

pub static SITE: Site = Site {
    category: "mangoxo",
    name: "Mangoxo",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&ALBUM, &CHANNEL],
    api_key: false,
    cookies: Some("www.mangoxo.com"),
};

const ROOT: &str = "https://www.mangoxo.com";
/// Listing pages an album or channel may claim.
const MAX_PAGES: i64 = 10_000;

static ALBUM: Route = Route::new(
    "album",
    "{album[id]}_{num:>03}.{extension}",
    &["{category}", "{channel[name]}", "{album[name]}"],
    "{album[id]}_{num}",
);
static CHANNEL: Route = Route::new("channel", "{filename}.{extension}", &["{category}"], "");

#[derive(Debug, PartialEq)]
enum Target {
    Album(String),
    Channel(String),
}

/// The `\w+` prefix of a raw path segment.
fn word(segment: &str) -> Option<&str> {
    let end = segment
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(segment.len());
    (end > 0).then(|| &segment[..end])
}

/// `(?:www\.)?mangoxo\.com/album/(\w+)` and
/// `(?:www\.)?mangoxo\.com/(\w+)/album`, both unanchored at the end.
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::art::on_host(url, &["mangoxo.com"]) {
        return None;
    }
    let raw: Vec<&str> = url.path().split('/').skip(1).collect();
    match raw.as_slice() {
        ["album", id, ..] if word(id).is_some() => word(id).map(|id| Target::Album(id.to_owned())),
        [user, album, ..] if album.starts_with("album") && word(user) == Some(user) => {
            Some(Target::Channel((*user).to_owned()))
        }
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Album(id)) => crate::sites::once(async move { album(http, &id).await }),
        Some(Target::Channel(user)) => channel(http, user),
        None => crate::sites::fail(error("unsupported Mangoxo URL")),
    }
}

async fn page(http: &Http, url: &str) -> Result<String> {
    http.text_with(&http.api(url)?, &http.browser_headers(&[]))
        .await
}

/// `MangoxoExtractor._total_pages`.
fn total_pages(page: &str) -> i64 {
    text::parse_int(text::extract(page, "total :", ",", 0).0.unwrap_or_default())
}

/// `MangoxoAlbumExtractor.metadata`.
fn metadata(page: &str, id: &str) -> Map<String, Value> {
    let mut scan = Scan::new(page);
    let title = scan.extr("<img id=\"cover-img\" alt=\"", "\"");
    let cid = scan.extr("href=\"https://www.mangoxo.com/user/", "\"");
    let cname = scan.extr("<img alt=\"", "\"");
    let cover = scan.extr(" src=\"", "\"");
    let count = scan.extr("id=\"pic-count\">", "<");
    let date = scan.extr("class=\"fa fa-calendar\"></i>", "<");
    let description = scan.extr("<pre>", "</pre>");
    keywords([
        (
            "channel",
            Value::Object(keywords([
                ("id", cid.into()),
                ("name", unescape(cname).into()),
                ("cover", cover.into()),
            ])),
        ),
        (
            "album",
            Value::Object(keywords([
                ("id", id.into()),
                ("name", unescape(title).into()),
                ("date", text::date(strptime(date.trim(), "%Y.%m.%d %H:%M"))),
                ("description", unescape(description).into()),
            ])),
        ),
        ("count", text::parse_int(count).into()),
    ])
}

/// `MangoxoAlbumExtractor.items`: the images of every listing page, each
/// served through `/external/`.
async fn album<'a>(http: &'a Http, id: &str) -> Result<Events<'a>> {
    let url = format!("{ROOT}/album/{id}/");
    let mut current = page(http, &url).await?;
    let data = metadata(&current, id);
    let total = total_pages(&current).min(MAX_PAGES);
    let mut paths = Vec::new();
    let mut number = 1;
    loop {
        paths.extend(
            extract_iter(&current, "class=\"lightgallery-item\" href=\"", "\"")
                .into_iter()
                .map(str::to_owned),
        );
        if number >= total {
            break;
        }
        number += 1;
        current = page(http, &format!("{url}{number}")).await?;
    }
    let items = paths
        .iter()
        .enumerate()
        .filter_map(|(index, path)| {
            let raw = format!("{ROOT}/external/{}", text::partition_last(path, "url=").2);
            let mut file = data.clone();
            set(&mut file, "num", index + 1);
            set(&mut file, "id", text::parse_int(extr(path, "=", "&")));
            nameext_into(&raw, &mut file);
            let url = Url::parse(&raw).ok()?;
            Some(session_cookies(
                http,
                Item::new(url, file).ids(id, (index + 1).to_string()),
            ))
        })
        .collect();
    Ok(crate::sites::events(crate::sites::post(
        &SITE, http, &ALBUM, items,
    )))
}

/// `MangoxoChannelExtractor.items`: the albums of each listing page, up to
/// the first page's page total.
fn channel(http: &Http, user: String) -> Events<'_> {
    crate::sites::paginate(Some((1i64, 1i64)), move |state| {
        let base = format!("{ROOT}/{user}/album/");
        async move {
            let Some((number, total)) = state else {
                return Ok(None);
            };
            let page = page(http, &format!("{base}{number}")).await?;
            let total = match number {
                1 => total_pages(&page).min(MAX_PAGES),
                _ => total,
            };
            let children = extract_iter(&page, "<a class=\"link black\" href=\"", "\"")
                .into_iter()
                .filter_map(|album| Url::parse(album).ok())
                .map(|url| (url, Map::new()))
                .collect();
            let next = (number < total).then_some((number + 1, total));
            Ok(Some((
                super::queue_urls(http, &SITE, &CHANNEL, children),
                next,
            )))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::super::tests::{archive, path};
    use super::*;
    use crate::sites::testing::{html, http, keywords as kw, media, queued, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.mangoxo.com/album/KbvEU6Tm/2"),
            Some(Target::Album("KbvEU6Tm".into()))
        );
        assert_eq!(
            t("http://mangoxo.com/album/a_b-c"),
            Some(Target::Album("a_b".into()))
        );
        assert_eq!(
            t("https://www.mangoxo.com/someone/album/3"),
            Some(Target::Channel("someone".into()))
        );
        assert_eq!(
            t("https://www.mangoxo.com/someone/albums"),
            Some(Target::Channel("someone".into()))
        );
        for raw in [
            "https://www.mangoxo.com/album/",
            "https://www.mangoxo.com/some-one/album",
            "https://www.mangoxo.com/someone/photos",
            "https://m.mangoxo.com/album/x",
            "https://www.mangoxo.com.evil.test/album/x",
            "https://www.mangoxo.com:8443/album/x",
            "https://u:p@www.mangoxo.com/album/x",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    const ALBUM_PAGE: &str = r#"<img id="cover-img" alt="Summer &amp; Sea" src="x">
<a href="https://www.mangoxo.com/user/chan123"><img alt="Chan &amp; Nel" src="https://img.test/cover.jpg"></a>
<span id="pic-count">3</span><i class="fa fa-calendar"></i> 2019.03.04 05:06 </span>
<pre>Hi &amp; bye</pre>
<a class="lightgallery-item" href="/external/x?id=11&amp;url=aHR0cA/1.jpg"></a>
<a class="lightgallery-item" href="/external/x?id=12&amp;url=aHR0cA/2.png"></a>
<script>page({total : 2, size: 2})</script>"#;

    #[tokio::test]
    async fn extracts_album_pages() {
        let second =
            r#"<a class="lightgallery-item" href="/external/x?id=13&amp;url=aHR0cA/3.gif"></a>"#;
        let (root, requests) = serve(vec![html(ALBUM_PAGE), html(second)]);
        let http = http("Mangoxo", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.mangoxo.com/album/KbvEU6Tm").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 3);
        assert_eq!(
            files[2].url.as_str(),
            "https://www.mangoxo.com/external/aHR0cA/3.gif"
        );
        let k = kw(&files[1]);
        assert_eq!(k["channel"]["id"], "chan123");
        assert_eq!(k["channel"]["name"], "Chan & Nel");
        assert_eq!(k["channel"]["cover"], "https://img.test/cover.jpg");
        assert_eq!(k["album"]["id"], "KbvEU6Tm");
        assert_eq!(k["album"]["name"], "Summer & Sea");
        assert_eq!(k["album"]["date"], "2019-03-04 05:06:00");
        assert_eq!(k["album"]["description"], "Hi & bye");
        assert_eq!(k["count"], 3);
        assert_eq!(k["num"], 2);
        assert_eq!(k["id"], 12);
        assert_eq!(k["extension"], "png");
        assert_eq!(
            path(&files[1]),
            "mangoxo/Chan & Nel/Summer & Sea/KbvEU6Tm_002.png"
        );
        assert_eq!(
            archive(&files[1]),
            r#"["{album[id]}_{num}",["KbvEU6Tm","2"]]"#
        );
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /album/KbvEU6Tm/ "));
        assert!(requests[1].starts_with("GET /album/KbvEU6Tm/2 "));
    }

    #[tokio::test]
    async fn album_files_carry_the_session_cookie() {
        let (root, requests) = serve(vec![html(ALBUM_PAGE), html("")]);
        let http = http("Mangoxo", root);
        let url = http.api(ROOT).unwrap();
        http.set_cookie(&url, "SESSION=abc; Path=/");
        let files = media(extract(
            &http,
            Url::parse("https://www.mangoxo.com/album/KbvEU6Tm").unwrap(),
        ))
        .await;
        assert_eq!(files[0].headers["Cookie"], "SESSION=abc");
        let requests = requests.join().unwrap();
        assert!(
            requests[0]
                .to_ascii_lowercase()
                .contains("cookie: session=abc\r\n")
        );
    }

    #[tokio::test]
    async fn channel_queues_albums_of_each_page() {
        let first = r#"<a class="link black" href="https://www.mangoxo.com/album/A1">A</a>
<script>page({total : 2,})</script>"#;
        let second = r#"<a class="link black" href="https://www.mangoxo.com/album/A2">B</a>"#;
        let (root, requests) = serve(vec![html(first), html(second)]);
        let http = http("Mangoxo", root);
        let children = queued(extract(
            &http,
            Url::parse("https://www.mangoxo.com/someone/album").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].subcategory(), "channel");
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /someone/album/1 "));
        assert!(requests[1].starts_with("GET /someone/album/2 "));
    }
}
