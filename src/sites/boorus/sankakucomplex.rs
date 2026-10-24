//! Sankaku Complex news articles and their tag, category and author
//! listings, mirroring gallery_dl/extractor/sankakucomplex.py.
//!
//! Articles yield their full-size images and `<source>` videos (option
//! `videos`); with `embeds`, iframes are handed to yt-dlp as upstream's
//! `ytdl:` URLs. Listings queue each article until a page is missing.
use super::{date_value, extract_iter, iso_timestamp, split_html};
use crate::sites::{Events, Http, Item, Route, Site, unescape};
use crate::{Result, error};
use futures_util::{StreamExt, stream};
use serde_json::{Map, Value};
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

/// `(?:news|www)\.sankakucomplex\.com/` followed by
/// `(\d\d\d\d/\d\d/\d\d/[^/?#]+)` or `((?:tag|category|author)/[^/?#]+)`.
fn target(url: &Url) -> Option<Target> {
    let rest = super::below(url, &["news.sankakucomplex.com", "www.sankakucomplex.com"])?;
    let segments: Vec<&str> = rest[1..].split(['/', '?']).collect();
    match segments.as_slice() {
        [year, month, day, slug, ..]
            if [(year, 4), (month, 2), (day, 2)]
                .iter()
                .all(|(s, n)| s.len() == *n && s.bytes().all(|b| b.is_ascii_digit()))
                && !slug.is_empty()
                && rest[1..].starts_with(&format!("{year}/{month}/{day}/{slug}")) =>
        {
            Some(Target::Article(format!("{year}/{month}/{day}/{slug}")))
        }
        [kind, name, ..]
            if matches!(*kind, "tag" | "category" | "author")
                && !name.is_empty()
                && rest[1..].starts_with(&format!("{kind}/{name}")) =>
        {
            Some(Target::Tag(format!("{kind}/{name}")))
        }
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Article(path)) => crate::sites::once(async move {
            let page = http
                .text(&http.api(&format!("{ROOT}/{path}/?pg=X"))?)
                .await?;
            let items = parse_article(
                &page,
                http.option_bool("videos", true),
                http.option_bool("embeds", false),
            )?;
            Ok(crate::sites::events(crate::sites::post(
                &SITE, http, &ARTICLE, items,
            )))
        }),
        Some(Target::Tag(path)) => listing(http, path),
        None => crate::sites::fail(error("unsupported Sankaku Complex URL")),
    }
}

/// Upstream `SankakucomplexTagExtractor`: queue each page's `data-direct`
/// articles until a page is unavailable.
fn listing(http: &Http, path: String) -> Events<'_> {
    crate::sites::paginate(Some(1u64), move |page| {
        let path = path.clone();
        async move {
            let Some(page) = page else {
                return Ok(None);
            };
            let url = http.api(&format!("{ROOT}/{path}/page/{page}/"))?;
            let text = match http.text(&url).await {
                Ok(text) => text,
                Err(e)
                    if crate::sites::http::refused_status(&e).is_some()
                        || e.to_string().ends_with("resource not found") =>
                {
                    return Ok(None);
                }
                Err(e) => return Err(e),
            };
            // `util.unique_sequence`: drop consecutive duplicates.
            let mut links = extract_iter(&text, "data-direct=\"", "\"");
            links.dedup();
            let children: Vec<Url> = links
                .into_iter()
                .filter_map(|link| Url::parse(link).ok())
                .filter(|link| matches!(target(link), Some(Target::Article(_))))
                .collect();
            let events = stream::iter(children)
                .map(move |child| {
                    crate::sites::queue(http, "sankakucomplex", "tag", child, &Map::new())
                })
                .flatten()
                .boxed();
            Ok(Some((events, Some(page + 1))))
        }
    })
}

/// `-\d+x\d+\.` replaced by `.`: the original of a resized image.
fn original(url: &str) -> String {
    let mut out = String::with_capacity(url.len());
    let mut rest = url;
    while let Some(index) = rest.find('-') {
        out.push_str(&rest[..index]);
        let tail = &rest[index + 1..];
        let width = tail.bytes().take_while(u8::is_ascii_digit).count();
        let after = &tail[width..];
        let height = after
            .strip_prefix('x')
            .map_or(0, |h| h.bytes().take_while(u8::is_ascii_digit).count());
        if width > 0 && height > 0 && after[1 + height..].starts_with('.') {
            out.push('.');
            rest = &after[2 + height..];
        } else {
            out.push('-');
            rest = tail;
        }
    }
    out.push_str(rest);
    out
}

/// `<TAG [^>]*src=["']([^"']+)` for every such tag.
fn sources(content: &str, tag: &str) -> Vec<String> {
    let open = format!("<{tag} ");
    let mut found = Vec::new();
    let mut rest = content;
    while let Some(index) = rest.find(&open) {
        rest = &rest[index + open.len()..];
        let inside = &rest[..rest.find('>').unwrap_or(rest.len())];
        // The greedy `[^>]*` takes the tag's last `src=` with a value.
        let value = inside.rmatch_indices("src=").find_map(|(at, _)| {
            let value = inside[at + 4..].strip_prefix(['"', '\''])?;
            let len = value.find(['"', '\'']).unwrap_or(value.len());
            (len > 0).then(|| &value[..len])
        });
        if let Some(value) = value {
            found.push(value.to_owned());
        }
    }
    found
}

/// One article page's files with upstream keywords.
pub(crate) fn parse_article(page: &str, videos: bool, embeds: bool) -> Result<Vec<Item>> {
    let mut scan = crate::sites::art::Scan::new(page);
    let title = unescape(scan.extr("property=\"og:title\" content=\"", "\""));
    let description = unescape(scan.extr("property=\"og:description\" content=\"", "\""));
    let timestamp =
        iso_timestamp(scan.extr("property=\"article:published_time\" content=\"", "\""));
    let content = scan.extr("<div class=\"entry-content\">", "</article>");
    let tags: Vec<Value> = split_html(scan.extr("=\"meta-tags\">", "</div>"))
        .into_iter()
        .step_by(2)
        .map(Value::from)
        .collect();
    let mut data = Map::new();
    data.insert("title".into(), title.clone().into());
    data.insert("description".into(), description.into());
    data.insert("date".into(), date_value(timestamp));
    data.insert("tags".into(), tags.into());
    let mut files: Vec<String> = Vec::new();
    for url in extract_iter(content, "data-lazy-src=\"", "\"") {
        let url = original(url);
        if !files.contains(&url) {
            files.push(url);
        }
    }
    if videos {
        files.extend(sources(content, "source"));
    }
    if embeds {
        files.extend(
            sources(content, "iframe")
                .into_iter()
                .map(|url| format!("ytdl:{url}")),
        );
    }
    data.insert("count".into(), files.len().into());
    let mut items = Vec::new();
    for (index, raw) in files.into_iter().enumerate() {
        let mut file = Map::new();
        crate::sites::art::nameext_into(&raw, &mut file);
        let full = if raw.starts_with('/') {
            format!("{ROOT}{raw}")
        } else {
            raw.clone()
        };
        file.insert("url".into(), full.clone().into());
        file.insert("num".into(), (index + 1).into());
        crate::sites::art::update(&mut file, &data);
        let item = match full.strip_prefix("ytdl:") {
            Some(page) => crate::sites::social::ytdl_item(page, file),
            None => Url::parse(&full)
                .ok()
                .filter(crate::sites::plain)
                .map(|url| Item::new(url, file)),
        };
        items.extend(item.map(|item| {
            item.ids(title.clone(), (index + 1).to_string())
                .caption(Some(title.clone()))
                .timestamp(timestamp)
        }));
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, queued, response, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://news.sankakucomplex.com/2019/05/11/twitter-cosplays"),
            Some(Target::Article("2019/05/11/twitter-cosplays".into()))
        );
        assert_eq!(
            t("https://www.sankakucomplex.com/tag/cosplay/page/2/"),
            Some(Target::Tag("tag/cosplay".into()))
        );
        assert_eq!(
            t("https://news.sankakucomplex.com/author/x"),
            Some(Target::Tag("author/x".into()))
        );
        for raw in [
            "https://news.sankakucomplex.com/2019/5/11/x",
            "https://news.sankakucomplex.com/2019/05/11/",
            "https://news.sankakucomplex.com/user/x",
            "https://chan.sankakucomplex.com/tag/x",
            "https://news.sankakucomplex.com.evil.com/tag/x",
            "https://news.sankakucomplex.com:8080/tag/x",
            "https://u:p@news.sankakucomplex.com/tag/x",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
        assert_eq!(
            crate::sites::find(&Url::parse("https://www.sankakucomplex.com/2020/01/02/x").unwrap())
                .map(|s| s.category),
            Some("sankakucomplex")
        );
    }

    #[test]
    fn originals_and_sources() {
        assert_eq!(original("https://x/a-b-640x480.jpg"), "https://x/a-b.jpg");
        assert_eq!(
            original("https://x/a-640x.jpg-1x2.png"),
            "https://x/a-640x.jpg.png"
        );
        assert_eq!(
            sources(
                "<source data-src=\"a\" src='b.mp4' type=x><source src=\"\">",
                "source"
            ),
            ["b.mp4"]
        );
    }

    #[tokio::test]
    async fn extracts_article() {
        let page = r#"<meta property="og:title" content="Cosplay &amp; More">
            <meta property="og:description" content="About">
            <meta property="article:published_time" content="2019-05-11T08:00:00+00:00">
            <div class="entry-content">
            <img data-lazy-src="https://img.sankakucomplex.com/2019/05/a-500x750.jpg">
            <img data-lazy-src="https://img.sankakucomplex.com/2019/05/a.jpg">
            <video><source src="/wp-content/v.mp4" type="video/mp4"></video>
            <iframe width=1 src="https://www.youtube.com/embed/abc"></iframe>
            </article><div class="meta-tags"><a>cosplay</a>, <a>twitter</a></div>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = http("Sankaku Complex", root).with_options(Some(std::sync::Arc::new(
            serde_json::json!({"embeds": true})
                .as_object()
                .unwrap()
                .clone(),
        )));
        let files = media(extract(
            &http,
            Url::parse("https://news.sankakucomplex.com/2019/05/11/cosplay").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 3);
        assert_eq!(
            files[0].url.as_str(),
            "https://img.sankakucomplex.com/2019/05/a.jpg"
        );
        assert_eq!(
            files[1].url.as_str(),
            "https://news.sankakucomplex.com/wp-content/v.mp4"
        );
        assert_eq!(
            files[2].url.as_str(),
            "ytdl:https://www.youtube.com/embed/abc"
        );
        let k = keywords(&files[1]);
        assert_eq!(k["title"], "Cosplay & More");
        assert_eq!(k["tags"], serde_json::json!(["cosplay", "twitter"]));
        assert_eq!(k["count"], 3);
        assert_eq!(k["num"], 2);
        assert_eq!(k["filename"], "v");
        assert_eq!(k["date"], "2019-05-11 08:00:00");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "a.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("sankakucomplex/2019-05-11 Cosplay & More")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(
            identity,
            r#"["{date:%Y%m%d}_{filename}",["2019-05-11 08:00:00","a"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /2019/05/11/cosplay/?pg=X "));
    }

    #[tokio::test]
    async fn queues_tag_pages_until_missing() {
        let page = r#"<a data-direct="https://news.sankakucomplex.com/2020/01/02/a"></a>
            <a data-direct="https://news.sankakucomplex.com/2020/01/02/a"></a>
            <a data-direct="https://news.sankakucomplex.com/2020/01/03/b"></a>"#;
        let (root, server) = serve(vec![
            html(page),
            response("404 Not Found", "text/html", b""),
        ]);
        let http = http("Sankaku Complex", root);
        let children = queued(extract(
            &http,
            Url::parse("https://news.sankakucomplex.com/tag/cosplay/").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].subcategory(), "tag");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /tag/cosplay/page/1/ "));
        assert!(requests[1].starts_with("GET /tag/cosplay/page/2/ "));
    }
}
