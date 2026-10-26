//! Xasiat albums and album listings, mirroring gallery_dl/extractor/xasiat.py.
use crate::error;
use crate::sites::gdl::{captures, group, nameext_into, parse_int, re, unescape};
use crate::sites::{Events, Http, Item, Route, Site};
use futures_util::StreamExt;
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://www.xasiat.com";
const BASE: &str = r"^https?://(?:www\.)?xasiat\.com((?:/fr|/ja)?/albums";

static ALBUM: Route = Route::new(
    "album",
    "{filename}.{extension}",
    &["{category}", "{title}"],
    "{album_url}_{num}",
);
static TAG: Route = listing("tag");
static CATEGORY: Route = listing("category");
static MODEL: Route = listing("model");

const fn listing(subcategory: &'static str) -> Route {
    Route::new(
        subcategory,
        "{filename}.{extension}",
        &["{category}", "{title}"],
        "{album_url}_{num}",
    )
}

pub static SITE: Site = Site {
    category: "xasiat",
    name: "Xasiat",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&ALBUM, &TAG, &CATEGORY, &MODEL],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Album(String, String),
    Listing(&'static str, String),
}

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(re!(&format!(r"{BASE}/(\d+)/[^/?#]+)")), url) {
        return Some(Target::Album(group(&c, 1).into(), group(&c, 2).into()));
    }
    for kind in ["tags", "categories", "models"] {
        let pattern = match kind {
            "tags" => re!(&format!(r"{BASE}/tags/[^/?#]+)")),
            "categories" => re!(&format!(r"{BASE}/categories/[^/?#]+)")),
            _ => re!(&format!(r"{BASE}/models/[^/?#]+)")),
        };
        if let Some(c) = captures(pattern, url) {
            return Some(Target::Listing(kind, group(&c, 1).into()));
        }
    }
    None
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Xasiat URL"));
    };
    match target {
        Target::Album(path, id) => crate::sites::once(async move {
            let (page, final_url) = http
                .text_final(&http.api(&format!("{ROOT}{path}/"))?, &[])
                .await?;
            let mut scan = crate::sites::gdl::Scan::new(&page);
            let title = scan.extr("<h1>", "<").to_owned();
            let info = scan.extr("class=\"info-content\"", "</div>").to_owned();
            let images = scan.extr("class=\"images\"", "</div>");
            let urls: Vec<String> = crate::sites::gdl::extract_iter(images, "href=\"", "\"")
                .into_iter()
                .map(str::to_owned)
                .collect();
            let all = |pattern: &regex::Regex| -> Vec<String> {
                pattern
                    .captures_iter(&info)
                    .map(|c| c[1].to_owned())
                    .collect()
            };
            let categories = all(re!(r#"categories/[^"]+">\s*(.+)\s*</a"#));
            let mut data = Map::new();
            data.insert("title".into(), unescape(&title).into());
            data.insert(
                "model".into(),
                all(re!(r#"top_models1"></i>\s*(.+)\s*</span"#)).into(),
            );
            data.insert(
                "tags".into(),
                all(re!(r#"tags/[^"]+">\s*(.+)\s*</a"#)).into(),
            );
            data.insert(
                "album_category".into(),
                categories.first().cloned().unwrap_or_default().into(),
            );
            data.insert(
                "album_url".into(),
                crate::sites::scalar(&Value::from(final_url.as_str())).into(),
            );
            data.insert("album_id".into(), parse_int(&id).into());
            data.insert("count".into(), urls.len().into());
            let items = urls
                .iter()
                .enumerate()
                .filter_map(|(index, raw)| {
                    let mut fields = data.clone();
                    fields.insert("num".into(), (index + 1).into());
                    nameext_into(&raw[..raw.len().saturating_sub(1)], &mut fields);
                    Some(
                        Item::new(Url::parse(raw).ok()?, fields)
                            .ids(id.clone(), (index + 1).to_string()),
                    )
                })
                .collect();
            Ok(crate::sites::events(crate::sites::post(
                &SITE, http, &ALBUM, items,
            )))
        }),
        Target::Listing(kind, path) => {
            let route: &'static Route = match kind {
                "tags" => &TAG,
                "categories" => &CATEGORY,
                _ => &MODEL,
            };
            crate::sites::paginate(Some(1i64), move |state| {
                let path = path.clone();
                async move {
                    let Some(number) = state else { return Ok(None) };
                    let millis = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |d| d.as_millis());
                    let mut url = http.api(&format!("{ROOT}{path}/"))?;
                    url.query_pairs_mut()
                        .append_pair("mode", "async")
                        .append_pair("function", "get_block")
                        .append_pair("block_id", "list_albums_common_albums_list")
                        .append_pair("sort_by", "post_date")
                        .append_pair("from", &number.to_string())
                        .append_pair("_", &millis.to_string());
                    let page = http.text(&url).await?;
                    let albums: Vec<Url> = re!(r#"class="item  ">\s*<a href="([^"]+)"#)
                        .captures_iter(&page)
                        .filter_map(|c| Url::parse(&c[1]).ok())
                        .collect();
                    let next = (!page.contains("<span>Next</span>")).then_some(number + 1);
                    let events = futures_util::stream::iter(albums)
                        .map(move |url| {
                            crate::sites::queue(
                                http,
                                SITE.category,
                                route.subcategory,
                                url,
                                &Map::new(),
                            )
                        })
                        .flatten()
                        .boxed();
                    Ok(Some((events, next)))
                }
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{children, html, http as client, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.xasiat.com/ja/albums/28155/cosplay1813/"),
            Some(Target::Album(
                "/ja/albums/28155/cosplay1813".into(),
                "28155".into()
            ))
        );
        assert_eq!(
            t("https://www.xasiat.com/albums/models/remu-suzumori/"),
            Some(Target::Listing(
                "models",
                "/albums/models/remu-suzumori".into()
            ))
        );
        assert_eq!(t("https://www.xasiat.com/videos/1/x/"), None);
    }

    #[tokio::test]
    async fn extracts_album() {
        let page = r#"<h1>Photobook &amp; More</h1><div class="info-content"><a href="https://www.xasiat.com/albums/categories/gravure/">
            Gravure </a>
            <i class="top_models1"></i> Remu </span>
            <a href="https://www.xasiat.com/albums/tags/japan/"> Japan </a></div>
            <div class="images"><a href="https://www.xasiat.com/get_image/2/abc/main/1.jpg/"></a></div>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = client("xasiat", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.xasiat.com/albums/28156/photobook/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let k = keywords(&files[0]);
        assert_eq!(k["filename"], "1");
        assert_eq!(k["extension"], "jpg");
        assert_eq!(k["album_category"], "Gravure ");
        assert_eq!(k["model"], serde_json::json!(["Remu "]));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn listing_pages_until_last() {
        let (root, server) = serve(vec![
            html(
                r#"<div class="item  "> <a href="https://www.xasiat.com/albums/1/a/">x</a><a>Next</a>"#,
            ),
            html(
                r#"<div class="item  "> <a href="https://www.xasiat.com/albums/2/b/">x</a><span>Next</span>"#,
            ),
        ]);
        let http = client("xasiat", root);
        assert_eq!(
            children(extract(
                &http,
                Url::parse("https://www.xasiat.com/albums/tags/japan/").unwrap()
            ))
            .await
            .len(),
            2
        );
        let requests = server.join().unwrap();
        assert!(requests[1].contains("from=2&"));
    }
}
