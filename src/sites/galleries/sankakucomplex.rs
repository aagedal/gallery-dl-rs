//! Sankaku Complex news articles and tag/category/author listings,
//! mirroring gallery_dl/extractor/sankakucomplex.py. Options: `videos`
//! (default on) and `embeds` (iframes, through yt-dlp).
use crate::error;
use crate::extract::MediaJob;
use crate::sites::gdl::{
    Scan, captures, date, extract_iter, group, nameext_into, parse_iso, re, split_html, unescape,
};
use crate::sites::{Events, Http, Item, Route, Site};
use futures_util::StreamExt;
use serde_json::Map;
use url::Url;

const ROOT: &str = "https://news.sankakucomplex.com";

static ARTICLE: Route = Route::new(
    "article",
    "{filename}.{extension}",
    &["{category}", "{date:%Y-%m-%d} {title}"],
    "{date:%Y%m%d}_{filename}",
);
static TAG: Route = Route::new("tag", "{filename}.{extension}", &["{category}"], "");

pub static SITE: Site = Site {
    category: "sankakucomplex",
    name: "Sankaku Complex",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&ARTICLE, &TAG],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Article(String),
    Tag(String),
}

fn target(url: &Url) -> Option<Target> {
    const BASE: &str = r"^https?://(?:news|www)\.sankakucomplex\.com";
    if let Some(c) = captures(re!(&format!(r"{BASE}/(\d\d\d\d/\d\d/\d\d/[^/?#]+)")), url) {
        return Some(Target::Article(group(&c, 1).into()));
    }
    captures(
        re!(&format!(r"{BASE}/((?:tag|category|author)/[^/?#]+)")),
        url,
    )
    .map(|c| Target::Tag(group(&c, 1).into()))
}

/// The article's file URLs: full-size images, `<source>` videos, embeds.
fn files(http: &Http, content: &str) -> Vec<String> {
    let mut urls: Vec<String> = Vec::new();
    for url in extract_iter(content, "data-lazy-src=\"", "\"") {
        let full = re!(r"-\d+x\d+\.").replace(url, ".").into_owned();
        if !urls.contains(&full) {
            urls.push(full);
        }
    }
    if http.option_bool("videos", true) {
        urls.extend(
            re!(r#"<source [^>]*src=["']([^"']+)"#)
                .captures_iter(content)
                .map(|c| c[1].to_owned()),
        );
    }
    if http.option_bool("embeds", false) {
        urls.extend(
            re!(r#"<iframe [^>]*src=["']([^"']+)"#)
                .captures_iter(content)
                .map(|c| format!("ytdl:{}", &c[1])),
        );
    }
    urls
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Sankaku Complex URL"));
    };
    match target {
        Target::Article(path) => crate::sites::once(async move {
            let page = http
                .text(&http.api(&format!("{ROOT}/{path}/?pg=X"))?)
                .await?;
            let mut scan = Scan::new(&page);
            let mut data = Map::new();
            data.insert(
                "title".into(),
                unescape(scan.extr("property=\"og:title\" content=\"", "\"")).into(),
            );
            data.insert(
                "description".into(),
                unescape(scan.extr("property=\"og:description\" content=\"", "\"")).into(),
            );
            data.insert(
                "date".into(),
                date(parse_iso(scan.extr(
                    "property=\"article:published_time\" content=\"",
                    "\"",
                ))),
            );
            let content = scan
                .extr("<div class=\"entry-content\">", "</article>")
                .to_owned();
            let tags: Vec<String> = split_html(scan.extr("=\"meta-tags\">", "</div>"))
                .into_iter()
                .step_by(2)
                .collect();
            data.insert("tags".into(), tags.into());
            let urls = files(http, &content);
            data.insert("count".into(), urls.len().into());
            let items = urls
                .into_iter()
                .enumerate()
                .filter_map(|(index, raw)| {
                    let mut file = Map::new();
                    nameext_into(&raw, &mut file);
                    let (url, job) = if let Some(page) = raw.strip_prefix("ytdl:") {
                        let page_url = Url::parse(page).ok()?;
                        (
                            Url::parse(&raw).ok()?,
                            Some(MediaJob::VideoPage { page_url }),
                        )
                    } else if raw.starts_with('/') {
                        (Url::parse(ROOT).ok()?.join(&raw).ok()?, None)
                    } else {
                        (Url::parse(&raw).ok()?, None)
                    };
                    file.insert("url".into(), url.as_str().into());
                    file.insert("num".into(), (index + 1).into());
                    file.extend(data.clone());
                    let mut item = Item::new(url, file).ids(path.clone(), (index + 1).to_string());
                    if let Some(job) = job {
                        item = item.job(job);
                    }
                    Some(item)
                })
                .collect();
            Ok(crate::sites::events(crate::sites::post(
                &SITE, http, &ARTICLE, items,
            )))
        }),
        Target::Tag(path) => crate::sites::paginate(Some(1u64), move |state| {
            let path = path.clone();
            async move {
                let Some(page) = state else { return Ok(None) };
                let Ok(html) = http
                    .text(&http.api(&format!("{ROOT}/{path}/page/{page}/"))?)
                    .await
                else {
                    return Ok(None);
                };
                let mut seen: Vec<String> = Vec::new();
                for url in extract_iter(&html, "data-direct=\"", "\"") {
                    if seen.last().map(String::as_str) != Some(url) {
                        seen.push(url.to_owned());
                    }
                }
                let children: Vec<Url> = seen.iter().filter_map(|u| Url::parse(u).ok()).collect();
                let events = futures_util::stream::iter(children)
                    .map(move |url| {
                        crate::sites::queue(http, SITE.category, TAG.subcategory, url, &Map::new())
                    })
                    .flatten()
                    .boxed();
                Ok(Some((events, Some(page + 1))))
            }
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{children, html, http as client, keywords, media, response, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://news.sankakucomplex.com/2019/05/11/twitter-cosplayers"),
            Some(Target::Article("2019/05/11/twitter-cosplayers".into()))
        );
        assert_eq!(
            t("https://www.sankakucomplex.com/author/rift/page/5/"),
            Some(Target::Tag("author/rift".into()))
        );
        assert_eq!(t("https://www.sankakucomplex.com/?tags=x"), None);
    }

    #[tokio::test]
    async fn extracts_full_size_images_and_videos() {
        let page = r#"<meta property="og:title" content="Cosplay &amp; More"><meta property="og:description" content="D">
            <meta property="article:published_time" content="2019-05-11T10:00:00+00:00">
            <div class="entry-content"><img data-lazy-src="https://www.sankakucomplex.com/wp-content/uploads/2019/05/a-600x800.jpg">
            <img data-lazy-src="https://www.sankakucomplex.com/wp-content/uploads/2019/05/a-600x800.jpg">
            <video><source type="video/mp4" src="https://www.sankakucomplex.com/wp-content/uploads/2019/05/v.mp4"></video></article>
            <div class="meta-tags"><a>cosplay</a>, <a>twitter</a></div>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = client("sankakucomplex", root);
        let files = media(extract(
            &http,
            Url::parse("https://news.sankakucomplex.com/2019/05/11/twitter-cosplayers").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].url.as_str(),
            "https://www.sankakucomplex.com/wp-content/uploads/2019/05/a.jpg"
        );
        let k = keywords(&files[1]);
        assert_eq!(k["tags"], serde_json::json!(["cosplay", "twitter"]));
        let (_, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("sankakucomplex/2019-05-11 Cosplay & More")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /2019/05/11/twitter-cosplayers/?pg=X "));
    }

    #[tokio::test]
    async fn tag_pages_until_error() {
        let (root, server) = serve(vec![
            html(
                r#"<a data-direct="https://news.sankakucomplex.com/2020/01/01/a"></a><a data-direct="https://news.sankakucomplex.com/2020/01/01/a"></a>"#,
            ),
            response("404 Not Found", "text/html", b""),
        ]);
        let http = client("sankakucomplex", root);
        assert_eq!(
            children(extract(
                &http,
                Url::parse("https://www.sankakucomplex.com/tag/cosplay/").unwrap()
            ))
            .await
            .len(),
            1
        );
        server.join().unwrap();
    }
}
