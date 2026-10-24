//! Chevereto galleries (jpgfish, imglike, putmega), mirroring
//! gallery_dl/extractor/chevereto.py: one site per `BaseExtractor`
//! instance, with album, category and user listings queued as file pages.
use super::{Body, Extr, request};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

const DIRECTORY: &[&str] = &["{category}", "{user}", "{album}"];
static FILE: Route = Route::new("file", "{filename}.{extension}", DIRECTORY, "{id}");
static ALBUM: Route = Route::new("album", "{filename}.{extension}", DIRECTORY, "{id}");
static CATEGORY: Route = Route::new("category", "{filename}.{extension}", DIRECTORY, "{id}");
static USER: Route = Route::new("user", "{filename}.{extension}", DIRECTORY, "{id}");

macro_rules! instance {
    ($ident:ident, $category:literal, $name:literal) => {
        pub static $ident: Site = Site {
            category: $category,
            name: $name,
            matches: |url| target($category, url).is_some(),
            extract: |http, url| extract($category, http, url),
            routes: &[&FILE, &ALBUM, &CATEGORY, &USER],
            api_key: false,
            cookies: None,
        };
    };
}
instance!(JPGFISH, "jpgfish", "JPG Fish");
instance!(IMGLIKE, "imglike", "IMG Like");
instance!(PUTMEGA, "putmega", "PutMega");

fn site(category: &str) -> &'static Site {
    match category {
        "jpgfish" => &JPGFISH,
        "imglike" => &IMGLIKE,
        _ => &PUTMEGA,
    }
}

fn root(category: &str) -> &'static str {
    match category {
        "jpgfish" => "https://jpg7.cr",
        "imglike" => "https://imglike.com",
        _ => "https://putmega.com",
    }
}

/// The instance host patterns, with an optional `www.` prefix.
fn host_matches(category: &str, host: &str) -> bool {
    let host = host.strip_prefix("www.").unwrap_or(host);
    match category {
        // jpe?g\d?\.(?:cr|su|pet|fish(?:ing)?|church)
        "jpgfish" => {
            let Some((name, tld)) = host.split_once('.') else {
                return false;
            };
            let rest = name
                .strip_prefix("jpeg")
                .or_else(|| name.strip_prefix("jpg"));
            rest.is_some_and(|r| r.is_empty() || (r.len() == 1 && r.as_bytes()[0].is_ascii_digit()))
                && matches!(tld, "cr" | "su" | "pet" | "fish" | "fishing" | "church")
        }
        "imglike" => host == "imglike.com",
        _ => host == "putmega.com" || host == "putme.ga",
    }
}

#[derive(Debug, PartialEq)]
enum Target {
    File(String),
    Album(String),
    Category(String),
    User(String),
}

fn target(category: &str, url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) || !host_matches(category, url.host_str()?) {
        return None;
    }
    let parts = crate::sites::segments(url);
    let raw: Vec<&str> = url.path().split('/').skip(1).collect();
    let segment = |i: usize| raw.get(i).copied().filter(|s| !s.is_empty());
    match parts.first().map(String::as_str) {
        Some("img" | "image" | "video" | "i") if segment(1).is_some() => {
            Some(Target::File(format!("/{}/{}", raw[0], raw[1])))
        }
        Some("a" | "album") if segment(1).is_some() => {
            let sub = if raw.get(2) == Some(&"sub") {
                "/sub"
            } else {
                ""
            };
            Some(Target::Album(format!("/{}/{}{sub}", raw[0], raw[1])))
        }
        Some("category") if segment(1).is_some() => {
            Some(Target::Category(format!("/category/{}", raw[1])))
        }
        Some(_) if segment(0).is_some() => {
            let albums = if raw.get(1) == Some(&"albums") {
                "/albums"
            } else {
                ""
            };
            Some(Target::User(format!("/{}{albums}", raw[0])))
        }
        _ => None,
    }
}

fn extract<'a>(category: &'static str, http: &'a Http, url: Url) -> Events<'a> {
    let base = root(category);
    match target(category, &url) {
        Some(Target::File(path)) => crate::sites::once(async move {
            let page = http.text(&http.api(&format!("{base}{path}"))?).await?;
            let item = parse_file(&page, &path)?;
            Ok(crate::sites::events(crate::sites::post(
                site(category),
                http,
                &FILE,
                vec![item],
            )))
        }),
        Some(Target::Album(path)) => {
            let url = format!("{base}{path}");
            if path.ends_with("/sub") {
                let albums = pagination(http, category, url);
                crate::sites::pages(futures_util::StreamExt::map(albums, move |page| {
                    page.map(|(urls, _)| {
                        futures_util::StreamExt::boxed(futures_util::StreamExt::flatten(
                            futures_util::stream::iter(
                                urls.into_iter()
                                    .map(move |album| album_images(http, category, album)),
                            ),
                        ))
                    })
                }))
            } else {
                album_images(http, category, url)
            }
        }
        Some(Target::Category(path)) => {
            queue_pages(http, category, "category", format!("{base}{path}"), |_| {
                Some(&FILE)
            })
        }
        Some(Target::User(path)) => {
            queue_pages(http, category, "user", format!("{base}{path}"), |url| {
                Some(if url.contains("/album/") || url.contains("/a/") {
                    &ALBUM
                } else {
                    &FILE
                })
            })
        }
        None => crate::sites::fail(error("unsupported Chevereto URL")),
    }
}

/// Item URLs of one listing page and the URL of the next.
fn parse_listing(category: &str, page: &str) -> (Vec<String>, Option<String>) {
    let base = root(category);
    let urls = super::extract_iter(page, "<div class=\"list-item-image ", "image-container")
        .into_iter()
        .filter_map(|item| crate::sites::extr(item, "<a href=\"", "\""))
        .filter_map(|href| {
            Url::parse(base)
                .ok()?
                .join(&crate::sites::unescape(href))
                .ok()
        })
        .filter(|url| {
            url.host_str().is_some_and(|h| host_matches(category, h)) && crate::sites::plain(url)
        })
        .map(String::from)
        .collect();
    let next = crate::sites::extr(page, "data-pagination=\"next\" href=\"", "\"")
        .filter(|next| !next.is_empty())
        .map(|next| {
            let next = crate::sites::unescape(next).replace('+', " ");
            if next.starts_with('/') {
                format!("{base}{next}")
            } else {
                next
            }
        });
    (urls, next)
}

/// Fetch a listing page, submitting a configured password when the page
/// asks for one. Without a `password` option, protected content is skipped
/// (upstream would prompt interactively).
async fn fetch_page(http: &Http, url: &str, cookie: &mut Option<String>) -> Result<String> {
    let target = Url::parse(url).map_err(|_| error("invalid Chevereto page URL"))?;
    let api = http.api(target.as_str())?;
    let headers: Vec<(&str, &str)> = cookie
        .as_deref()
        .map(|c| vec![("Cookie", c)])
        .unwrap_or_default();
    let page = http.text_with(&api, &headers).await?;
    if page.len() >= 45_000 || !page.contains("assword required") {
        return Ok(page);
    }
    let Some(form) = crate::sites::extr(&page, "<form ", "</form") else {
        return Ok(page);
    };
    let passwords: Vec<String> = match http.option("password") {
        Some(Value::String(s)) => s.split(',').map(str::to_owned).collect(),
        Some(Value::Array(list)) => list
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    };
    let token = crate::sites::unescape(
        crate::sites::extr(form, "name=\"auth_token\" value=\"", "\"").unwrap_or_default(),
    );
    let mut tried = std::collections::BTreeSet::new();
    for password in passwords.iter().filter(|p| !p.is_empty()) {
        if !tried.insert(password.clone()) {
            continue;
        }
        let response = request(
            http,
            reqwest::Method::POST,
            &api,
            &headers,
            Body::Form(&[("auth_token", &token), ("content-password", password)]),
        )
        .await?;
        let text = response.text();
        if crate::sites::extr(&text, "<form ", "</form").is_none() {
            let jar = response.cookie_header();
            if !jar.is_empty() {
                *cookie = Some(jar);
            }
            return Ok(text);
        }
    }
    Ok(page)
}

type Page = (Vec<String>, String);

/// Lazily fetched listing pages: item URLs and the page text.
fn pagination<'a>(
    http: &'a Http,
    category: &'static str,
    start: String,
) -> futures_util::stream::BoxStream<'a, Result<Page>> {
    let state = (Some(start), None::<String>, 0u32);
    futures_util::StreamExt::boxed(futures_util::stream::try_unfold(
        state,
        move |(url, mut cookie, count)| async move {
            let Some(url) = url else {
                return Ok(None);
            };
            if count >= 10_000 {
                return Ok(None);
            }
            let page = fetch_page(http, &url, &mut cookie).await?;
            let (urls, next) = parse_listing(category, &page);
            let next = next.filter(|n| {
                Url::parse(n).is_ok_and(|n| {
                    crate::sites::plain(&n)
                        && n.host_str().is_some_and(|h| host_matches(category, h))
                })
            });
            Ok(Some(((urls, page), (next, cookie, count + 1))))
        },
    ))
}

fn queue_pages<'a>(
    http: &'a Http,
    category: &'static str,
    subcategory: &'static str,
    start: String,
    route: fn(&str) -> Option<&'static Route>,
) -> Events<'a> {
    crate::sites::pages(futures_util::StreamExt::map(
        pagination(http, category, start),
        move |page| {
            page.map(|(urls, _)| {
                let children = urls
                    .into_iter()
                    .filter(|url| route(url).is_some())
                    .filter_map(|url| Url::parse(&url).ok())
                    .map(|url| (url, Map::new()))
                    .collect();
                super::queue_all(http, category, subcategory, children)
            })
        },
    ))
}

/// `_extract_metadata_album`.
fn album_metadata(page: &str) -> Map<String, Value> {
    let mut data = Map::new();
    let (url, pos) =
        super::extract(page, "property=\"og:url\" content=\"", "\"", 0).unwrap_or(("", 0));
    let (title, pos) =
        super::extract(page, "property=\"og:title\" content=\"", "\"", pos).unwrap_or(("", pos));
    let last = super::rpartition(url, "/").2;
    let (slug, _, id) = super::rpartition(last, ".");
    data.insert("album_slug".into(), slug.into());
    data.insert("album_id".into(), id.into());
    data.insert("album".into(), crate::sites::unescape(title).into());
    let count = super::extract(page, "data-text=\"image-count\">", "<", pos)
        .map(|(c, _)| super::parse_int(c))
        .unwrap_or(0);
    data.insert("count".into(), count.into());
    data
}

/// Queue an album's images, numbered across pages, with album metadata.
fn album_images<'a>(http: &'a Http, category: &'static str, url: String) -> Events<'a> {
    let mut data: Option<Map<String, Value>> = None;
    let mut num = 0u64;
    crate::sites::pages(futures_util::StreamExt::map(
        pagination(http, category, url),
        move |page| {
            page.map(|(urls, text)| {
                let meta = data.get_or_insert_with(|| album_metadata(&text)).clone();
                let children = urls
                    .into_iter()
                    .filter_map(|url| Url::parse(&url).ok())
                    .map(|url| {
                        num += 1;
                        let mut child = meta.clone();
                        child.insert("num".into(), num.into());
                        (url, child)
                    })
                    .collect();
                super::queue_all(http, category, "album", children)
            })
        },
    ))
}

fn parse_file(page: &str, path: &str) -> Result<Item> {
    let mut extr = Extr::new(page);
    let kind =
        crate::sites::extr(page, "property=\"og:type\" content=\"", "\"").unwrap_or_default();
    let title = extr.next("property=\"og:title\" content=\"", "\"");
    let mut file = Map::new();
    let album_url;
    let timestamp;
    if kind == "video" {
        file.insert("id".into(), super::rpartition(path, ".").2.into());
        file.insert("type".into(), "video".into());
        // Upstream searches for a second og:title; fall back to the first.
        let second = extr.next("property=\"og:title\" content=\"", "\"");
        let second = if second.is_empty() { title } else { second };
        file.insert("title".into(), crate::sites::unescape(second).into());
        file.insert(
            "thumbnail".into(),
            extr.next("property=\"og:image\" content=\"", "\"").into(),
        );
        file.insert(
            "url".into(),
            extr.next("property=\"og:video\" content=\"", "\"").into(),
        );
        file.insert(
            "width".into(),
            super::parse_int(extr.next("property=\"video:width\" content=\"", "\"")).into(),
        );
        file.insert(
            "height".into(),
            super::parse_int(extr.next("property=\"video:height\" content=\"", "\"")).into(),
        );
        let duration = extr.next("class=\"far fa-clock\"></i>", "—");
        let added = extr.next("Added to <a", "</a>");
        timestamp = super::parse_iso(extr.next("<span title=\"", "\""));
        file.insert("date".into(), super::date_value(timestamp));
        file.insert("user".into(), extr.next("username: \"", "\"").into());
        let (url_part, _, name) = super::rpartition(added, ">");
        album_url = url_part;
        file.insert("album".into(), crate::sites::remove_html(name).into());
        let (min, _, sec) = super::partition(duration, ":");
        file.insert(
            "duration".into(),
            match (min.trim().parse::<i64>(), sec.trim().parse::<i64>()) {
                (Ok(m), Ok(s)) => (m * 60 + s).into(),
                _ => duration.into(),
            },
        );
    } else {
        let mut url = extr
            .next("<meta property=\"og:image\" content=\"", "\"")
            .to_owned();
        if url.is_empty() {
            url = extr.next("url: \"", "\"").to_owned();
        }
        if url.is_empty() || url.ends_with("/loading.svg") {
            let pos = page.find(" download=");
            url = super::rextr(page, "href=\"", "\"", pos).to_owned();
            if !url.starts_with("https://") {
                url = super::decrypt_xor(&url, b"seltilovessimpcity@simpcityhatesscrapers", true)
                    .unwrap_or_default();
            }
        }
        let added = extr.next("Added to <a", "</a>");
        let (url_part, _, name) = super::rpartition(added, ">");
        album_url = url_part;
        let id = super::rpartition(super::rpartition(path, "/").2, ".").2;
        file.insert("id".into(), id.into());
        file.insert("url".into(), url.into());
        file.insert("type".into(), kind.into());
        file.insert("title".into(), crate::sites::unescape(title).into());
        file.insert("album".into(), crate::sites::remove_html(name).into());
        timestamp = super::parse_iso(extr.next("<span title=\"", "\""));
        file.insert("date".into(), super::date_value(timestamp));
        file.insert("user".into(), extr.next("username: \"", "\"").into());
    }
    let (slug, _, album_id) = super::rpartition(super::rextr(album_url, "/", "\"", None), ".");
    file.insert("album_slug".into(), slug.into());
    file.insert("album_id".into(), album_id.into());
    let raw = file
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    super::nameext_from_url(&raw, &mut file);
    let url = super::web_url(&raw).ok_or_else(|| error("Chevereto page has no valid file URL"))?;
    let id = file.get("id").map(crate::sites::scalar).unwrap_or_default();
    let user = file.get("user").and_then(Value::as_str).map(str::to_owned);
    let caption = file.get("title").and_then(Value::as_str).map(str::to_owned);
    Ok(Item::new(url, file)
        .ids(id.clone(), id)
        .user(user)
        .caption(caption)
        .timestamp(timestamp))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |c, raw: &str| target(c, &Url::parse(raw).unwrap());
        assert_eq!(
            t("jpgfish", "https://jpg5.su/img/funnymeme.LecXGS"),
            Some(Target::File("/img/funnymeme.LecXGS".into()))
        );
        assert_eq!(
            t("jpgfish", "https://www.jpeg.pet/album/CD-1.2.3/sub"),
            Some(Target::Album("/album/CD-1.2.3/sub".into()))
        );
        assert_eq!(
            t("imglike", "https://imglike.com/category/Nature"),
            Some(Target::Category("/category/Nature".into()))
        );
        assert_eq!(
            t("putmega", "https://putme.ga/exearco/albums"),
            Some(Target::User("/exearco/albums".into()))
        );
        assert_eq!(
            t("putmega", "https://putmega.com/a/x.ID"),
            Some(Target::Album("/a/x.ID".into()))
        );
        for (c, raw) in [
            ("jpgfish", "https://jpg12.cr/img/x"),
            ("jpgfish", "https://jpg.com/img/x"),
            ("jpgfish", "https://jpg.fish.evil.test/img/x"),
            ("imglike", "https://imglike.com/"),
            ("imglike", "https://imglike.com:81/a/x"),
            ("putmega", "https://imglike.com/a/x"),
        ] {
            assert_eq!(t(c, raw), None, "{raw}");
        }
        assert!(
            crate::sites::find(&Url::parse("https://jpg6.su/img/a.b").unwrap())
                .is_some_and(|s| s.category == "jpgfish")
        );
    }

    const IMAGE: &str = "<meta property=\"og:type\" content=\"article\">\
<meta property=\"og:title\" content=\"Funny &amp; meme\">\
<meta property=\"og:image\" content=\"https://simp6.jpg5.su/images/funny.png\">\
Added to <a href=\"https://jpg7.cr/album/My-Album.AbC\">My Album</a>\
<span title=\"2023-01-02 03:04:05\">x</span> username: \"alice\",";

    #[test]
    fn parses_image_page() {
        let item = parse_file(IMAGE, "/img/funnymeme.LecXGS").unwrap();
        let k = &item.keywords;
        assert_eq!(item.url.as_str(), "https://simp6.jpg5.su/images/funny.png");
        assert_eq!(k["id"], "LecXGS");
        assert_eq!(k["title"], "Funny & meme");
        assert_eq!(k["album"], "My Album");
        assert_eq!(k["album_slug"], "My-Album");
        assert_eq!(k["album_id"], "AbC");
        assert_eq!(k["date"], "2023-01-02 03:04:05");
        assert_eq!(k["user"], "alice");
        assert_eq!(k["filename"], "funny");
        assert_eq!(k["type"], "article");
    }

    #[test]
    fn parses_video_page() {
        let page = "<meta property=\"og:type\" content=\"video\">\
<meta property=\"og:title\" content=\"Clip\">\
<meta property=\"og:image\" content=\"https://x.jpg7.cr/t.jpg\">\
<meta property=\"og:video\" content=\"https://x.jpg7.cr/v.mp4\">\
<meta property=\"video:width\" content=\"640\"><meta property=\"video:height\" content=\"360\">\
<i class=\"far fa-clock\"></i>01:05—";
        let item = parse_file(page, "/video/clip.ZZ").unwrap();
        let k = &item.keywords;
        assert_eq!(k["id"], "ZZ");
        assert_eq!(k["title"], "Clip");
        assert_eq!(k["duration"], 65);
        assert_eq!(k["width"], 640);
        assert_eq!(k["extension"], "mp4");
        assert_eq!(k["album"], "");
    }

    #[test]
    fn decrypts_download_links() {
        use base64::Engine;
        let key = b"seltilovessimpcity@simpcityhatesscrapers";
        let hex: String = b"http://cdn.test/a.jpg"
            .iter()
            .enumerate()
            .map(|(i, b)| format!("{:02x}", b ^ key[i % key.len()]))
            .collect();
        let page = format!(
            "<meta property=\"og:type\" content=\"image\"><a href=\"{}\" download=\"a.jpg\">",
            base64::engine::general_purpose::STANDARD.encode(hex)
        );
        let item = parse_file(&page, "/img/a.Q").unwrap();
        assert_eq!(item.url.as_str(), "http://cdn.test/a.jpg");
    }

    #[tokio::test]
    async fn album_queues_files_across_pages() {
        let listing = |items: &[&str], next: Option<&str>| {
            let mut page = String::from(
                "<meta property=\"og:url\" content=\"https://jpg7.cr/album/Trip.A1\">\
                 <meta property=\"og:title\" content=\"Trip\"><b data-text=\"image-count\">3</b>",
            );
            for item in items {
                page.push_str(&format!(
                    "<div class=\"list-item-image fixed-size\"><a href=\"{item}\">image-container"
                ));
            }
            if let Some(next) = next {
                page.push_str(&format!("<a data-pagination=\"next\" href=\"{next}\">"));
            }
            page
        };
        let (root, server) = serve(vec![
            html(&listing(
                &["https://jpg7.cr/img/a.1", "/img/b.2"],
                Some("/album/Trip.A1/?page=2&amp;seek=x+y"),
            )),
            html(IMAGE),
            html(IMAGE),
            html(&listing(&["https://jpg7.cr/img/c.3"], None)),
            html(IMAGE),
        ]);
        let http = http("JPG Fish", root);
        let files = media(extract(
            "jpgfish",
            &http,
            Url::parse("https://jpg5.su/album/Trip.A1").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 3);
        let k = keywords(&files[0]);
        assert_eq!(k["subcategory"], "file");
        assert_eq!(k["category"], "jpgfish");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "funny.png");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("jpgfish/alice/My Album")
        );
        assert_eq!(
            crate::sites::archive_key(&files[0]).unwrap().1,
            r#"["{id}",["1"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /album/Trip.A1 "));
        assert!(requests[1].starts_with("GET /img/a.1 "));
        assert!(requests[2].starts_with("GET /img/b.2 "));
        assert!(
            requests[3].starts_with("GET /album/Trip.A1/?page=2&seek=x%20y "),
            "{}",
            requests[3]
        );
    }

    #[test]
    fn album_metadata_matches_upstream() {
        let data = album_metadata(
            "<meta property=\"og:url\" content=\"https://jpg7.cr/album/My-Trip.A1\">\
             <meta property=\"og:title\" content=\"My &amp; Trip\"><b data-text=\"image-count\">12</b>",
        );
        assert_eq!(data["album_slug"], "My-Trip");
        assert_eq!(data["album_id"], "A1");
        assert_eq!(data["album"], "My & Trip");
        assert_eq!(data["count"], 12);
    }

    #[test]
    fn page_without_file_url_is_an_error() {
        let err = parse_file("<html>nothing</html>", "/img/x.AbC")
            .err()
            .unwrap();
        assert!(err.to_string().contains("no valid file URL"));
    }
}
