//! Poringa posts, users and searches, mirroring gallery_dl/extractor/poringa.py.
use crate::error;
use crate::sites::gdl::{captures, extr, extract_iter, group, nameext_into, re, unescape, unquote};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::Map;
use std::time::Duration;
use url::Url;

const ROOT: &str = "http://www.poringa.net";
const BASE: &str = r"^https?://(?:www\.)?poringa\.net";
const FILENAME: &str = "{post_id}_{title}_{num:>03}_{filename}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{user}", "{post_id}"];

static POST: Route = Route::new("post", FILENAME, DIRECTORY, "{post_id}_{num}");
static USER: Route = Route::new("user", FILENAME, DIRECTORY, "{post_id}_{num}");
static SEARCH: Route = Route::new("search", FILENAME, DIRECTORY, "{post_id}_{num}");

pub static SITE: Site = Site {
    category: "poringa",
    name: "Poringa",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&POST, &USER, &SEARCH],
    api_key: false,
    cookies: Some("poringa.net"),
};

#[derive(Debug, PartialEq)]
enum Target {
    Post(String),
    User(String),
    Search(String),
}

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(re!(&format!(r"{BASE}/posts/imagenes/(\d+)")), url) {
        return Some(Target::Post(group(&c, 1).into()));
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}/(\w+)$")), url) {
        return Some(Target::User(group(&c, 1).into()));
    }
    captures(re!(&format!(r"{BASE}/buscar/\?&?q=([^&#]+)")), url)
        .map(|c| Target::Search(group(&c, 1).into()))
}

/// `PoringaExtractor.request`: retry while the "Please wait" page shows.
async fn request(http: &Http, url: &Url) -> crate::Result<(String, Url)> {
    let mut last = None;
    for _ in 0..5 {
        let (page, final_url) = http.text_final(url, &[]).await?;
        if !page
            .get(..600.min(page.len()))
            .unwrap_or(&page)
            .contains("<title>Please wait a few moments</title>")
        {
            return Ok((page, final_url));
        }
        last = Some((page, final_url));
        if !cfg!(test) {
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }
    last.ok_or_else(|| error("Poringa request failed"))
}

/// One post's files; private or failing posts are skipped like upstream.
async fn post_items(http: &Http, post_id: &str) -> Vec<Item> {
    let url = format!("{ROOT}/posts/imagenes/{post_id}");
    let Ok(target_url) = http.api(&url) else {
        return Vec::new();
    };
    let Ok((page, final_url)) = request(http, &target_url).await else {
        return Vec::new();
    };
    if final_url.as_str().contains("/registro-login?") {
        return Vec::new();
    }
    let title_start = page.find("property=\"og:title\" content=\"");
    let title = extr(&page, "property=\"og:title\" content=\"", "\"");
    let user = title_start
        .and_then(|pos| page[pos..].find("<div class=\"main-info").map(|p| pos + p))
        .map(|pos| extr(&page[pos..], "href=\"http://www.poringa.net/", "\""))
        .filter(|u| !u.is_empty())
        .unwrap_or("poringa");
    let mut data = Map::new();
    data.insert("post_id".into(), post_id.into());
    data.insert("title".into(), unescape(title).into());
    data.insert("user".into(), unquote(user).into());
    let main = extr(&page, "property=\"dc:content\" role=\"main\">", "</div>");
    let urls = extract_iter(main, "<img class=\"imagen\" border=\"0\" src=\"", "\"");
    data.insert("count".into(), urls.len().into());
    urls.into_iter()
        .enumerate()
        .filter_map(|(index, file)| {
            let mut fields = data.clone();
            fields.insert("num".into(), (index + 1).into());
            nameext_into(file, &mut fields);
            Some(
                Item::new(Url::parse(file).ok()?, fields)
                    .ids(post_id, (index + 1).to_string())
                    .header("Referer", &url),
            )
        })
        .collect()
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Poringa URL"));
    };
    let (route, query) = match target {
        Target::Post(id) => {
            return crate::sites::once(async move {
                Ok(crate::sites::events(crate::sites::post(
                    &SITE,
                    http,
                    &POST,
                    post_items(http, &id).await,
                )))
            });
        }
        Target::User(user) => (&USER, user),
        Target::Search(query) => (&SEARCH, query),
    };
    crate::sites::paginate(Some(1u64), move |state| {
        let query = query.clone();
        async move {
            let Some(page_num) = state else {
                return Ok(None);
            };
            let mut url = http.api(&format!("{ROOT}/buscar/"))?;
            url.query_pairs_mut()
                .append_pair("q", &query)
                .append_pair("p", &page_num.to_string());
            let (page, _) = request(http, &url).await?;
            let mut ids: Vec<String> = Vec::new();
            for c in re!(r"(?:https?://)?(?:www\.)?poringa\.net/posts/imagenes/(\d+)")
                .captures_iter(&page)
            {
                if !ids.iter().any(|id| id == &c[1]) {
                    ids.push(c[1].to_owned());
                }
            }
            let next = (ids.len() >= 19).then_some(page_num + 1);
            let events = crate::sites::each(ids, move |id| async move {
                Ok(crate::sites::post(
                    &SITE,
                    http,
                    route,
                    post_items(http, &id).await,
                ))
            });
            Ok(Some((events, next)))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http as client, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("http://www.poringa.net/posts/imagenes/3051081/Turrita.html"),
            Some(Target::Post("3051081".into()))
        );
        assert_eq!(
            t("http://www.poringa.net/Faloxx"),
            Some(Target::User("Faloxx".into()))
        );
        assert_eq!(
            t("http://www.poringa.net/buscar/?&q=yuslopez"),
            Some(Target::Search("yuslopez".into()))
        );
    }

    #[tokio::test]
    async fn retries_wait_page_then_reads_post() {
        let page = r#"<meta property="og:title" content="Title &amp; X"><div class="main-info"><a href="http://www.poringa.net/User%20A">
            <div property="dc:content" role="main"><img class="imagen" border="0" src="http://img.poringa.net/a.jpg"></div>"#;
        let (root, server) = serve(vec![
            html("<title>Please wait a few moments</title>"),
            html(page),
        ]);
        let http = client("poringa", root);
        let files = media(extract(
            &http,
            Url::parse("http://www.poringa.net/posts/imagenes/1").unwrap(),
        ))
        .await;
        let k = keywords(&files[0]);
        assert_eq!(
            (k["title"].as_str(), k["user"].as_str()),
            (Some("Title & X"), Some("User A"))
        );
        assert_eq!(
            files[0].headers["Referer"],
            "http://www.poringa.net/posts/imagenes/1"
        );
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "1_Title & X_001_a.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("poringa/User A/1")
        );
        server.join().unwrap();
    }
}
