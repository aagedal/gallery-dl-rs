//! EVERIA.CLUB posts and listings, mirroring gallery_dl/extractor/everia.py.
use crate::error;
use crate::sites::gdl::{captures, extr, extract_iter, group, nameext_into, re, unescape, unquote};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::Map;
use url::Url;

const ROOT: &str = "https://everia.club";
const BASE: &str = r"^https?://everia\.club";

static POST: Route = Route::new(
    "post",
    "{filename}.{extension}",
    &["{category}", "{title}"],
    "{post_url}_{num}",
);
static TAG: Route = listing("tag");
static CATEGORY: Route = listing("category");
static DATE: Route = listing("date");
static SEARCH: Route = listing("search");

const fn listing(subcategory: &'static str) -> Route {
    Route::new(subcategory, "{filename}.{extension}", &["{category}"], "")
}

pub static SITE: Site = Site {
    category: "everia",
    name: "EVERIA.CLUB",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&POST, &TAG, &CATEGORY, &DATE, &SEARCH],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Post(String),
    /// Listing route, path and optional search query.
    List(&'static str, String, Option<String>),
}

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(
        re!(&format!(r"{BASE}(/\d{{4}}/\d{{2}}/\d{{2}}/[^/?#]+)")),
        url,
    ) {
        return Some(Target::Post(group(&c, 1).into()));
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}(/tag/[^/?#]+)")), url) {
        return Some(Target::List("tag", group(&c, 1).into(), None));
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}(/category/[^/?#]+)")), url) {
        return Some(Target::List("category", group(&c, 1).into(), None));
    }
    if let Some(c) = captures(
        re!(&format!(
            r"{BASE}(/\d{{4}}(?:/\d{{2}})?(?:/\d{{2}})?)(?:/page/\d+)?/?$"
        )),
        url,
    ) {
        return Some(Target::List("date", group(&c, 1).into(), None));
    }
    captures(re!(&format!(r"{BASE}/(?:page/\d+/)?\?s=([^&#]+)")), url)
        .map(|c| Target::List("search", String::new(), Some(group(&c, 1).into())))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported EVERIA.CLUB URL"));
    };
    match target {
        Target::Post(path) => crate::sites::once(async move {
            let url = format!("{ROOT}{path}/");
            let page = http.text(&http.api(&url)?).await?;
            let content = extr(&page, "itemprop=\"text\">", "<h3");
            let mut urls: Vec<String> = re!(r#"img.*?lazy-src="([^"]+)"#)
                .captures_iter(content)
                .map(|c| unquote(&c[1]))
                .collect();
            // The site has since dropped lazy loading; upstream finds nothing.
            if urls.is_empty() {
                urls = re!(r#"<img [^>]*?src="([^"]+)"#)
                    .captures_iter(content)
                    .map(|c| unquote(&c[1]))
                    .collect();
            }
            let mut data = Map::new();
            data.insert(
                "title".into(),
                unescape(extr(&page, "itemprop=\"headline\">", "</h")).into(),
            );
            let tags: Vec<&str> = extract_iter(&page, "rel=\"tag\">", "</a>");
            data.insert("tags".into(), tags.into());
            data.insert("post_url".into(), unquote(&url).into());
            let category = extr(&page, "post-in-category-", " ");
            let mut chars = category.chars();
            let capitalized: String = chars
                .next()
                .map(|c| {
                    c.to_uppercase()
                        .chain(chars.flat_map(char::to_lowercase))
                        .collect()
                })
                .unwrap_or_default();
            data.insert("post_category".into(), capitalized.into());
            data.insert("count".into(), urls.len().into());
            let items = urls
                .iter()
                .enumerate()
                .filter_map(|(index, raw)| {
                    let mut fields = data.clone();
                    fields.insert("num".into(), (index + 1).into());
                    nameext_into(raw, &mut fields);
                    Some(
                        Item::new(Url::parse(raw).ok()?, fields)
                            .ids(path.clone(), (index + 1).to_string()),
                    )
                })
                .collect();
            Ok(crate::sites::events(crate::sites::post(
                &SITE, http, &POST, items,
            )))
        }),
        Target::List(kind, path, search) => {
            let route: &'static Route = match kind {
                "tag" => &TAG,
                "category" => &CATEGORY,
                "date" => &DATE,
                _ => &SEARCH,
            };
            crate::sites::paginate(Some(1u64), move |state| {
                let path = path.clone();
                let search = search.clone();
                async move {
                    let Some(number) = state else { return Ok(None) };
                    let mut url = http.api(&if number == 1 {
                        format!("{ROOT}{path}/")
                    } else {
                        format!("{ROOT}{path}/page/{number}/")
                    })?;
                    if let Some(query) = &search {
                        url.query_pairs_mut().append_pair("s", query);
                    }
                    // Upstream stops at the first redirect (past the last page).
                    if http.location(&url, &[]).await?.is_some() {
                        return Ok(None);
                    }
                    let page = http.text(&url).await?;
                    let posts = re!(r#"thumbnail">\s*<a href="([^"]+)"#)
                        .captures_iter(&page)
                        .filter_map(|c| Some((Url::parse(&c[1]).ok()?, Map::new())))
                        .collect();
                    Ok(Some((
                        super::queue(http, &SITE, route, posts),
                        Some(number + 1),
                    )))
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
        assert!(matches!(
            t("https://everia.club/2024/09/23/mikacho-joapictures-someday/"),
            Some(Target::Post(_))
        ));
        assert_eq!(
            t("https://everia.club/2023/10/05/"),
            Some(Target::List("date", "/2023/10/05".into(), None))
        );
        assert_eq!(
            t("https://everia.club/?s=saika"),
            Some(Target::List("search", String::new(), Some("saika".into())))
        );
        assert_eq!(
            t("https://everia.club/category/japan/"),
            Some(Target::List("category", "/category/japan".into(), None))
        );
    }

    #[tokio::test]
    async fn extracts_post_images() {
        let page = r#"<h1 itemprop="headline">Mikacho &amp; Pictures</h1><article class="post-in-category-korea x">
            <div itemprop="text"><img class="x" data-lazy-src="https://www.everiaclub.com/a/%E9%9B%AF.jpg"></div><h3>
            <a rel="tag">Mikacho</a>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = client("everia", root);
        let files = media(extract(
            &http,
            Url::parse("https://everia.club/2024/09/23/mikacho/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let k = keywords(&files[0]);
        assert_eq!(k["title"], "Mikacho & Pictures");
        assert_eq!(k["post_category"], "Korea");
        assert_eq!(k["tags"], serde_json::json!(["Mikacho"]));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn listing_stops_at_redirect() {
        let redirect = b"HTTP/1.1 302 Found\r\nLocation: https://everia.club/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let (root, server) = serve(vec![
            html("ok"),
            html(r#"<div class="thumbnail"> <a href="https://everia.club/2024/01/01/a/">"#),
            redirect.to_vec(),
        ]);
        let http = client("everia", root);
        assert_eq!(
            children(extract(
                &http,
                Url::parse("https://everia.club/tag/x/").unwrap()
            ))
            .await
            .len(),
            1
        );
        server.join().unwrap();
    }
}
