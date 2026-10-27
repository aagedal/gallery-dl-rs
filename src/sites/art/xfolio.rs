//! xfolio works, portfolios and series, mirroring
//! gallery_dl/extractor/xfolio.py: work pages list their images, while
//! portfolio and series pages queue their works. Upstream's logged-in
//! `fullsize` ZIP download needs the `xfolio_session` cookie and is not
//! implemented; works always download their per-image files.
use crate::error;
use crate::sites::art::{Scan, ext_from_url, extr, extract_iter};
use crate::sites::{Events, Http, Item, Route, Site, unescape};
use serde_json::{Map, Value};
use url::Url;

pub static SITE: Site = Site {
    category: "xfolio",
    name: "xfolio",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&WORK, &USER, &SERIES],
    api_key: false,
    cookies: None,
};

const ROOT: &str = "https://xfolio.jp";
const FILENAME: &str = "{work_id}_{image_id}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{creator_slug}", "{work_id}"];
const ARCHIVE: &str = "{work_id}_{image_id}";
static WORK: Route = Route::new("work", FILENAME, DIRECTORY, ARCHIVE);
static USER: Route = Route::new("user", FILENAME, DIRECTORY, ARCHIVE);
static SERIES: Route = Route::new("series", FILENAME, DIRECTORY, ARCHIVE);

#[derive(Debug, PartialEq)]
enum Target {
    Work { creator: String, work_id: String },
    User(String),
    Series { creator: String, series_id: String },
}

fn leading_digits(text: &str) -> Option<&str> {
    let end = text.bytes().take_while(u8::is_ascii_digit).count();
    (1..=20).contains(&end).then(|| &text[..end])
}

fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) || url.host_str() != Some("xfolio.jp") {
        return None;
    }
    let parts: Vec<&str> = url.path().split('/').skip(1).collect();
    // `xfolio\.jp(?:/[^/?#]+)?/portfolio/`: an optional language segment.
    [1, 0].into_iter().find_map(|skip| {
        if skip == 1 && parts.first().is_none_or(|lang| lang.is_empty()) {
            return None;
        }
        match parts.get(skip..)? {
            ["portfolio", creator, rest @ ..] if !creator.is_empty() => {
                let creator = (*creator).to_owned();
                match rest {
                    [] | [""] | ["works"] | ["works", ""] => Some(Target::User(creator)),
                    ["works", id, ..] => leading_digits(id).map(|id| Target::Work {
                        creator,
                        work_id: id.into(),
                    }),
                    ["series", id, ..] => leading_digits(id).map(|id| Target::Series {
                        creator,
                        series_id: id.into(),
                    }),
                    _ => None,
                }
            }
            _ => None,
        }
    })
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Work { creator, work_id }) => crate::sites::once(async move {
            let url = http.api(&format!("{ROOT}/portfolio/{creator}/works/{work_id}"))?;
            let html = http.text(&url).await?;
            let items = parse_work(&html, &creator, &work_id);
            Ok(crate::sites::events(crate::sites::post(
                &SITE, http, &WORK, items,
            )))
        }),
        Some(Target::User(creator)) => {
            let start = format!("{ROOT}/portfolio/{creator}/works");
            crate::sites::paginate(Some(start), move |next| async move {
                let Some(next) = next else {
                    return Ok(None);
                };
                let html = http.text(&http.api(&next)?).await?;
                let works: Vec<String> = extract_iter(&html, "<div class=\"postItem", "</div>")
                    .into_iter()
                    .map(|item| extr(item, " href=\"", "\"").to_owned())
                    .collect();
                let pager = extr(&html, " class=\"pager__list_next", "</li>");
                let href = extr(pager, " href=\"", "\"");
                let next = if href.is_empty() {
                    None
                } else {
                    let next = Url::parse(ROOT)
                        .and_then(|root| root.join(&unescape(href)))
                        .ok()
                        .filter(|u| crate::sites::plain(u) && u.host_str() == Some("xfolio.jp"));
                    Some(
                        next.ok_or_else(|| error("invalid xfolio page link"))?
                            .to_string(),
                    )
                };
                Ok(Some((queue(http, "user", works), next)))
            })
        }
        Some(Target::Series { creator, series_id }) => crate::sites::once(async move {
            let url = http.api(&format!("{ROOT}/portfolio/{creator}/series/{series_id}"))?;
            let html = http.text(&url).await?;
            let works = extract_iter(&html, "class=\"listWrap--title\">", "</a>")
                .into_iter()
                .map(|item| extr(item, " href=\"", "\"").to_owned())
                .collect();
            Ok(queue(http, "series", works))
        }),
        None => crate::sites::fail(error("unsupported xfolio URL")),
    }
}

/// Queue work links that resolve to xfolio work pages.
fn queue<'a>(http: &'a Http, subcategory: &'static str, works: Vec<String>) -> Events<'a> {
    let root = Url::parse(ROOT).expect("static root");
    let children: Vec<Url> = works
        .iter()
        .filter_map(|href| root.join(href).ok())
        .filter(|url| matches!(target(url), Some(Target::Work { .. })))
        .collect();
    let data = Map::new();
    futures_util::StreamExt::boxed(futures_util::StreamExt::flatten(
        futures_util::stream::iter(
            children
                .into_iter()
                .map(move |child| crate::sites::queue(http, "xfolio", subcategory, child, &data)),
        ),
    ))
}

/// Upstream `_extract_data`.
fn work_data(html: &str, creator: &str, work_id: &str) -> Map<String, Value> {
    let mut scan = Scan::new(html);
    let title = scan.extr("property=\"og:title\" content=\"", "\"");
    let title = title.rsplit_once(" - ").map_or("", |(t, _)| t);
    let mut data = Map::new();
    data.insert("title".into(), unescape(title).into());
    let description = scan.extr("property=\"og:description\" content=\"", "\"");
    data.insert("description".into(), unescape(description).into());
    data.insert(
        "creator_id".into(),
        scan.extr(" data-creator-id=\"", "\"").into(),
    );
    data.insert(
        "creator_userid".into(),
        scan.extr(" data-creator-user-id=\"", "\"").into(),
    );
    data.insert(
        "creator_name".into(),
        scan.extr(" data-creator-name=\"", "\"").into(),
    );
    let profile = scan.extr(" data-creator-profile=\"", "\"");
    data.insert("creator_profile".into(), unescape(profile).into());
    data.insert("series_id".into(), scan.extr("/series/", "\"").into());
    data.insert("creator_slug".into(), creator.into());
    data.insert("work_id".into(), work_id.into());
    data
}

/// Upstream `_extract_files` for a logged-out session.
fn parse_work(html: &str, creator: &str, work_id: &str) -> Vec<Item> {
    let mut work = work_data(html, creator, work_id);
    let mut files = Vec::new();
    // Each wrapper up to the next one: wrappers may nest a `<div>` before
    // their `<img>`, so upstream's `</div>` end loses those images.
    let marker = "class=\"article__wrap_img";
    let wraps: Vec<&str> = html
        .match_indices(marker)
        .map(|(i, _)| &html[i + marker.len()..])
        .filter(|rest| !rest.starts_with("_hide"))
        .map(|rest| &rest[..rest.find(marker).unwrap_or(rest.len())])
        .collect();
    for img in wraps {
        let image_id = extr(img, "/fullscale_image?image_id=", "&");
        if !image_id.is_empty() {
            let url = format!(
                "{ROOT}/user_asset.php?id={image_id}&work_id={work_id}&work_image_id={image_id}&type=work_image"
            );
            let referer = format!("{ROOT}/fullscale_image?image_id={image_id}&work_id={work_id}");
            files.push((
                image_id.to_owned(),
                url,
                "jpg".to_owned(),
                vec![("Referer", referer)],
            ));
            continue;
        }
        let src = extr(img, " src=\"", "\"");
        if src.is_empty() {
            continue;
        }
        // Python `url.split("/", 5)[-2]`; newer `/secure/<n>/` URLs carry
        // the image id as the file name's `<id>_` prefix instead.
        let parts: Vec<&str> = src.splitn(6, '/').collect();
        let mut image_id = parts[parts.len().saturating_sub(2)].to_owned();
        if src.contains("/secure/") {
            let name = src.split('?').next().unwrap_or_default();
            let name = name.rsplit('/').next().unwrap_or_default();
            if let Some((id, _)) = name.split_once('_').filter(|(id, _)| !id.is_empty()) {
                image_id = id.to_owned();
            }
        }
        let url = unescape(src);
        let ext = ext_from_url(&url);
        files.push((
            image_id,
            url,
            ext,
            vec![
                ("Referer", format!("{ROOT}/")),
                ("Sec-Fetch-Dest", "image".into()),
                ("Sec-Fetch-Mode", "no-cors".into()),
                ("Sec-Fetch-Site", "same-site".into()),
            ],
        ));
    }
    work.insert("count".into(), files.len().into());
    let user = work
        .get("creator_name")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let title = work.get("title").and_then(Value::as_str).map(str::to_owned);
    let mut items = Vec::new();
    for (index, (image_id, raw, ext, headers)) in files.into_iter().enumerate() {
        let Some(url) = crate::sites::art::file_url(&raw, ROOT) else {
            continue;
        };
        let mut fields = Map::new();
        fields.insert("image_id".into(), image_id.clone().into());
        fields.insert("url".into(), raw.into());
        fields.insert("extension".into(), ext.into());
        work.insert("num".into(), (index + 1).into());
        crate::sites::art::update(&mut fields, &work);
        let mut item = Item::new(url, fields)
            .ids(work_id, image_id)
            .user(user.clone())
            .caption(title.clone());
        for (name, value) in &headers {
            item = item.header(name, value);
        }
        items.push(item);
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, response, serve};

    fn t(raw: &str) -> Option<Target> {
        target(&Url::parse(raw).unwrap())
    }

    #[test]
    fn classifies_urls() {
        let work = |c: &str, w: &str| {
            Some(Target::Work {
                creator: c.into(),
                work_id: w.into(),
            })
        };
        assert_eq!(
            t("https://xfolio.jp/portfolio/yutakashii/works/23977"),
            work("yutakashii", "23977")
        );
        assert_eq!(
            t("https://xfolio.jp/en/portfolio/a/works/87495"),
            work("a", "87495")
        );
        for raw in [
            "https://xfolio.jp/portfolio/yutakashii",
            "https://xfolio.jp/portfolio/yutakashii/",
            "https://xfolio.jp/portfolio/yutakashii/works?page=3",
            "https://xfolio.jp/zh-CN/portfolio/yutakashii",
        ] {
            assert_eq!(t(raw), Some(Target::User("yutakashii".into())), "{raw}");
        }
        assert_eq!(
            t("https://xfolio.jp/portfolio/donguri/series/1391402"),
            Some(Target::Series {
                creator: "donguri".into(),
                series_id: "1391402".into()
            })
        );
        for raw in [
            "https://xfolio.jp/",
            "https://xfolio.jp/portfolio/",
            "https://xfolio.jp/portfolio/a/other",
            "https://xfolio.jp/a/b/portfolio/c",
            "https://www.xfolio.jp/portfolio/a",
            "https://assets.xfolio.jp/portfolio/a",
            "https://xfolio.jp.evil.com/portfolio/a",
            "https://evilxfolio.jp/portfolio/a",
            "https://xfolio.jp:8080/portfolio/a",
            "https://u:p@xfolio.jp/portfolio/a",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    const WORK_PAGE: &str = r#"<meta property="og:title" content="A &amp; B - Creator - xfolio">
<meta property="og:description" content="Desc &amp; more">
<div data-creator-id="1495" data-creator-user-id="3778" data-creator-name="香椎" data-creator-profile="P &amp; Q">
<a href="/portfolio/yutakashii/series/42">
<div class="article__wrap_img"><a href="/fullscale_image?image_id=113179&work_id=23977"><img src="x"></a></div>
<div class="article__wrap_img"><div class="openIcon"></div><img src="https://assets.xfolio.jp/secure/1359786657/creator/6030/works/87495/305223_r.webp?hash=I&amp;expires=1"></div>
<div class="article__wrap_img">nothing</div>"#;

    #[tokio::test]
    async fn extracts_work() {
        let (root, server) = serve(vec![html(WORK_PAGE)]);
        let http = http("xfolio", root);
        let files = media(extract(
            &http,
            Url::parse("https://xfolio.jp/en/portfolio/yutakashii/works/23977").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[0]);
        assert_eq!(k["title"], "A & B - Creator");
        assert_eq!(k["description"], "Desc & more");
        assert_eq!(k["creator_id"], "1495");
        assert_eq!(k["creator_userid"], "3778");
        assert_eq!(k["creator_profile"], "P & Q");
        assert_eq!(k["series_id"], "42");
        assert_eq!(k["image_id"], "113179");
        assert_eq!(k["extension"], "jpg");
        assert_eq!(k["count"], 2);
        assert_eq!(
            files[0].url.as_str(),
            "https://xfolio.jp/user_asset.php?id=113179&work_id=23977&work_image_id=113179&type=work_image"
        );
        assert_eq!(
            files[0].headers["Referer"],
            "https://xfolio.jp/fullscale_image?image_id=113179&work_id=23977"
        );
        let k = keywords(&files[1]);
        assert_eq!(k["image_id"], "305223");
        assert_eq!(k["extension"], "webp");
        assert_eq!(k["num"], 2);
        assert!(files[1].url.as_str().ends_with("?hash=I&expires=1"));
        assert_eq!(files[1].headers["Sec-Fetch-Site"], "same-site");
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "23977_305223.webp");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("xfolio/yutakashii/23977")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{work_id}_{image_id}",["23977","113179"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /portfolio/yutakashii/works/23977 "));
    }

    #[tokio::test]
    async fn user_pages_queue_works() {
        let first = r#"<div class="postItem"><a href="https://xfolio.jp/portfolio/u/works/1"></a></div>
<li class="pager__list_next"><a href="/portfolio/u/works?page=2&amp;x=1">next</a></li>"#;
        let second = r#"<div class="postItem"><a href="/portfolio/u/works/2"></a></div>"#;
        let (root, server) = serve(vec![
            html(first),
            html(WORK_PAGE),
            html(second),
            html(WORK_PAGE),
        ]);
        let http = http("xfolio", root);
        let files = media(extract(
            &http,
            Url::parse("https://xfolio.jp/portfolio/u").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 4);
        assert_eq!(keywords(&files[2])["work_id"], "2");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /portfolio/u/works "));
        assert!(requests[1].starts_with("GET /portfolio/u/works/1 "));
        assert!(
            requests[2].starts_with("GET /portfolio/u/works?page=2&x=1 "),
            "{}",
            requests[2]
        );
        assert!(requests[3].starts_with("GET /portfolio/u/works/2 "));
    }

    #[tokio::test]
    async fn series_queues_works() {
        let page =
            r#"<div class="listWrap--title"><a href="https://xfolio.jp/portfolio/d/works/7">T</a>"#;
        let (root, server) = serve(vec![html(page), html(WORK_PAGE)]);
        let http = http("xfolio", root);
        let files = media(extract(
            &http,
            Url::parse("https://xfolio.jp/portfolio/d/series/9").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /portfolio/d/series/9 "));
        assert!(requests[1].starts_with("GET /portfolio/d/works/7 "));
    }

    #[tokio::test]
    async fn reports_missing_page() {
        let (root, _server) = serve(vec![response("404 Not Found", "text/html", b"")]);
        let http = http("xfolio", root);
        let events: Vec<_> = futures_util::StreamExt::collect(extract(
            &http,
            Url::parse("https://xfolio.jp/portfolio/a/works/1").unwrap(),
        ))
        .await;
        let message = events[0].as_ref().err().unwrap().to_string();
        assert!(message.contains("not found"), "{message}");
    }
}
