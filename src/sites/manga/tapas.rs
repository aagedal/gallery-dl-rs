//! Tapas episodes, series and creators, mirroring
//! gallery_dl/extractor/tapas.py. Logs in with the `username`/`password`
//! options unless a `_cpc_` session cookie is loaded; otherwise sets the
//! birth-date cookies that unlock mature series.
use super::set;
use crate::error;
use crate::sites::gdl::{
    auth_info, captures, date, extr, extract_iter, group, nameext_into, parse_iso, partition, re,
    rextr, str_of, unescape,
};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://tapas.io";
const DIRECTORY: &[&str] = &["{category}", "{series[title]}", "{id} {title}"];

static EPISODE: Route = Route::new("episode", "{num:>02}.{extension}", DIRECTORY, "{id}_{num}");
static SERIES: Route = Route::new("series", "{num:>02}.{extension}", DIRECTORY, "{id}_{num}");
static CREATOR: Route = Route::new("creator", "{num:>02}.{extension}", DIRECTORY, "{id}_{num}");

pub static SITE: Site = Site {
    category: "tapas",
    name: "Tapas",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&EPISODE, &SERIES, &CREATOR],
    api_key: false,
    cookies: Some("tapas.io"),
};

#[derive(Debug, PartialEq)]
enum Target {
    Episode(String),
    Series(String),
    Creator(String),
}

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(re!(r"^https?://tapas\.io/episode/(\d+)"), url) {
        return Some(Target::Episode(group(&c, 1).into()));
    }
    if let Some(c) = captures(re!(r"^https?://tapas\.io/series/([^/?#]+)"), url) {
        return Some(Target::Series(group(&c, 1).into()));
    }
    let c = captures(re!(r"^https?://tapas\.io/([^/?#]+)"), url)?;
    let name = group(&c, 1);
    (!name.starts_with("series") && !name.starts_with("episode"))
        .then(|| Target::Creator(name.into()))
}

/// `TapasExtractor.login`.
async fn login(http: &Http) -> crate::Result<()> {
    let root = http.api(ROOT)?;
    if http.cookie(&root, "_cpc_").is_some() {
        return Ok(());
    }
    if let Some((username, password)) = auth_info(http) {
        let url = http.api(&format!("{ROOT}/account/authenticate"))?;
        let referer = format!("{ROOT}/account/authenticate");
        // A successful login redirects; the session cookie arrives with it.
        let _ = http
            .post_form_text(
                &url,
                &[
                    ("from", "https://tapas.io/"),
                    ("email", &username),
                    ("password", &password),
                ],
                &[("Referer", &referer)],
            )
            .await;
        if http.cookie(&root, "_cpc_").is_none() {
            return Err(error("Tapas login failed; check the username and password"));
        }
        return Ok(());
    }
    for name in ["birthDate", "adjustedBirthDate"] {
        http.set_cookie(
            &root,
            &format!("{name}=1981-02-03; Domain=.tapas.io; Path=/"),
        );
    }
    Ok(())
}

async fn api(http: &Http, url: &Url) -> crate::Result<Value> {
    let data = http
        .json_with(
            url,
            &[("Accept", "application/json, text/javascript, */*;")],
        )
        .await?;
    Ok(data["data"].clone())
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Tapas URL"));
    };
    match target {
        Target::Episode(id) => crate::sites::once(async move {
            login(http).await?;
            let data = api(http, &http.api(&format!("{ROOT}/episode/{id}"))?).await?;
            let mut episode = data["episode"].as_object().cloned().unwrap_or_default();
            let unlocked = |key: &str| crate::sites::gdl::truthy(episode.get(key));
            if !unlocked("free") && !unlocked("unlocked") {
                return Err(error(format!(
                    "{id}: Episode '{}' not unlocked",
                    episode
                        .get("title")
                        .map(crate::sites::scalar)
                        .unwrap_or_default()
                )));
            }
            let html = str_of(&data, "html");
            let series_id = rextr(html, "data-series-id=\"", "\"", None);
            let series = api(http, &http.api(&format!("{ROOT}/series/{series_id}"))?).await?;
            set(&mut episode, "series", series);
            let published = episode
                .get("publish_date")
                .map(crate::sites::scalar)
                .unwrap_or_default();
            set(&mut episode, "date", date(parse_iso(&published)));
            let mut items = Vec::new();
            if crate::sites::gdl::truthy(episode.get("book")) {
                let content = extr(html, "<div class=\"viewer\">", "<div class=\"viewer-bottom");
                let (url, job) = crate::extract::inline_text(content.to_owned())?;
                let mut data = episode.clone();
                set(&mut data, "num", 1);
                set(&mut data, "filename", "01");
                set(&mut data, "extension", "html");
                items.push(Item::new(url, data).job(job).ids(id.clone(), "1"));
            } else {
                for (index, raw) in extract_iter(html, "data-src=\"", "\"")
                    .into_iter()
                    .enumerate()
                {
                    let raw = unescape(raw);
                    let Ok(url) = Url::parse(&raw) else { continue };
                    let mut data = episode.clone();
                    set(&mut data, "num", index + 1);
                    nameext_into(&raw, &mut data);
                    items.push(Item::new(url, data).ids(id.clone(), (index + 1).to_string()));
                }
            }
            Ok(crate::sites::events(crate::sites::post(
                &SITE, http, &EPISODE, items,
            )))
        }),
        Target::Series(name) => crate::sites::once(async move {
            login(http).await?;
            let page = super::fetch(http, &format!("{ROOT}/series/{name}")).await?;
            let (series_id, _, episode_id) = partition(
                extr(&page, "content=\"tapastic://series/", "\""),
                "/episodes/",
            );
            let (series_id, episode_id) = (series_id.to_owned(), episode_id.to_owned());
            Ok(crate::sites::paginate(Some(1u64), move |state| {
                let series_id = series_id.clone();
                let episode_id = episode_id.clone();
                async move {
                    let Some(page) = state else { return Ok(None) };
                    let mut url = http.api(&format!("{ROOT}/series/{series_id}/episodes"))?;
                    url.query_pairs_mut()
                        .append_pair("eid", &episode_id)
                        .append_pair("page", &page.to_string())
                        .append_pair("sort", "OLDEST")
                        .append_pair("last_access", "0")
                        .append_pair("max_limit", "20");
                    let data = api(http, &url).await?;
                    let children = data["episodes"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|episode| {
                            let url = Url::parse(&format!(
                                "{ROOT}/episode/{}",
                                crate::sites::scalar(&episode["id"])
                            ))
                            .ok()?;
                            Some((url, episode.as_object().cloned().unwrap_or_default()))
                        })
                        .collect();
                    let next = crate::sites::gdl::truthy(data["pagination"].get("has_next"))
                        .then_some(page + 1);
                    Ok(Some((
                        super::queue_urls(http, &SITE, &SERIES, children),
                        next,
                    )))
                }
            }))
        }),
        Target::Creator(name) => crate::sites::once(async move {
            login(http).await?;
            let page = super::fetch(http, &format!("{ROOT}/{name}/series")).await?;
            let list = extr(&page, "<ul class=\"content-list-wrap", "</ul>");
            let series = extract_iter(list, " href=\"", "\"")
                .into_iter()
                .filter_map(|path| Url::parse(&format!("{ROOT}{path}")).ok())
                .map(|url| (url, Map::new()))
                .collect();
            Ok(super::queue_urls(http, &SITE, &CREATOR, series))
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{archive, path};
    use super::*;
    use crate::sites::testing::{children, html, http, json, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://tapas.io/episode/2068651"),
            Some(Target::Episode("2068651".into()))
        );
        assert_eq!(
            t("https://tapas.io/series/yona"),
            Some(Target::Series("yona".into()))
        );
        assert_eq!(
            t("https://tapas.io/SANG123/series"),
            Some(Target::Creator("SANG123".into()))
        );
        assert_eq!(t("https://tapas.io/episodes"), None);
        assert_eq!(t("https://www.tapas.io/episode/1"), None);
    }

    #[tokio::test]
    async fn extracts_comic_episode() {
        let episode = json!({"data": {"episode": {"id": 5, "title": "Ep 1", "free": true, "book": false,
            "publish_date": "2021-03-04T05:06:07Z"},
            "html": "<div data-series-id=\"77\"><img data-src=\"https://us-a.tapas.io/pc/a/1.jpg?x=1&amp;y=2\"><img data-src=\"https://us-a.tapas.io/pc/a/2.png\"></div>"}});
        let series = json!({"data": {"id": 77, "title": "Yona"}});
        let (root, server) = serve(vec![json(episode), json(series)]);
        let http = http("tapas", root);
        let files = media(extract(
            &http,
            Url::parse("https://tapas.io/episode/5").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].url.as_str(),
            "https://us-a.tapas.io/pc/a/1.jpg?x=1&y=2"
        );
        let k = keywords(&files[1]);
        assert_eq!(k["series"]["title"], "Yona");
        assert_eq!(k["date"], "2021-03-04 05:06:07");
        assert_eq!(path(&files[1]), "tapas/Yona/5 Ep 1/02.png");
        assert_eq!(archive(&files[0]), r#"["{id}_{num}",["5","1"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /episode/5 "));
        assert!(requests[1].starts_with("GET /series/77 "));
    }

    #[tokio::test]
    async fn locked_episodes_fail_and_books_are_text() {
        let locked = json!({"data": {"episode": {"id": 6, "title": "Paid", "free": false, "unlocked": false}, "html": ""}});
        let (root, server) = serve(vec![json(locked)]);
        let client = http("tapas", root);
        assert!(
            crate::sites::testing::fails(extract(
                &client,
                Url::parse("https://tapas.io/episode/6").unwrap()
            ))
            .await
        );
        server.join().unwrap();

        let book = json!({"data": {"episode": {"id": 7, "title": "Novel", "free": true, "book": true, "publish_date": ""},
            "html": "<i data-series-id=\"8\"></i><div class=\"viewer\"><p>Once</p><div class=\"viewer-bottom\">"}});
        let (root, server) = serve(vec![json(book), json(json!({"data": {"title": "S"}}))]);
        let http = http("tapas", root);
        let files = media(extract(
            &http,
            Url::parse("https://tapas.io/episode/7").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert!(files[0].url.as_str().starts_with("text:"));
        assert_eq!(path(&files[0]), "tapas/S/7 Novel/01.html");
        server.join().unwrap();
    }

    #[tokio::test]
    async fn series_pages_episodes() {
        let (root, server) = serve(vec![
            html(r#"<meta content="tapastic://series/77/episodes/5">"#),
            json(
                json!({"data": {"episodes": [{"id": 5}, {"id": 6}], "pagination": {"has_next": true}}}),
            ),
            json(json!({"data": {"episodes": [{"id": 7}], "pagination": {"has_next": false}}})),
        ]);
        let http = http("tapas", root);
        assert_eq!(
            children(extract(
                &http,
                Url::parse("https://tapas.io/series/yona").unwrap()
            ))
            .await
            .len(),
            3
        );
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with(
            "GET /series/77/episodes?eid=5&page=1&sort=OLDEST&last_access=0&max_limit=20 "
        ));
        assert!(requests[2].contains("page=2"));
    }
}
