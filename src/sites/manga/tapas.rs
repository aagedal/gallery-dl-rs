//! Tapas episodes, series and creators, mirroring
//! gallery_dl/extractor/tapas.py.
//!
//! Not ported: username/password login (`_cpc_` session cookies from
//! `--cookies` are used instead).
use super::{Memo, set, text};
use crate::sites::art::{extr, extract_iter, nameext_into, truthy};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use futures_util::StreamExt;
use serde_json::{Map, Value};
use url::Url;

pub static SITE: Site = Site {
    category: "tapas",
    name: "Tapas",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&EPISODE, &SERIES, &CREATOR],
    api_key: false,
    cookies: Some("tapas.io"),
};

const ROOT: &str = "https://tapas.io";
const REFERER: &str = "https://tapas.io/";
const FILENAME: &str = "{num:>02}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{series[title]}", "{id} {title}"];
const ARCHIVE: &str = "{id}_{num}";

static EPISODE: Route = Route::new("episode", FILENAME, DIRECTORY, ARCHIVE);
static SERIES: Route = Route::new("series", FILENAME, DIRECTORY, ARCHIVE);
static CREATOR: Route = Route::new("creator", FILENAME, DIRECTORY, ARCHIVE);

#[derive(Debug, PartialEq)]
enum Target {
    Episode(String),
    Series(String),
    Creator(String),
}

/// `tapas\.io/episode/(\d+)`, `/series/([^/?#]+)` and
/// `/(?!series|episode)([^/?#]+)`.
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) || url.host_str() != Some("tapas.io") {
        return None;
    }
    let path = url.path();
    let first = path.split('/').nth(1).unwrap_or_default();
    if let Some(rest) = path.strip_prefix("/episode/") {
        let id: String = rest.chars().take_while(char::is_ascii_digit).collect();
        return (!id.is_empty() && id.len() <= 20).then_some(Target::Episode(id));
    }
    if let Some(rest) = path.strip_prefix("/series/") {
        let name = rest.split('/').next().unwrap_or_default();
        return (!name.is_empty()).then(|| Target::Series(name.to_owned()));
    }
    (!first.is_empty() && !first.starts_with("series") && !first.starts_with("episode"))
        .then(|| Target::Creator(first.to_owned()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Episode(id)) => episode_events(http, id, Memo::default()),
        Some(Target::Series(name)) => series_events(http, name),
        Some(Target::Creator(name)) => crate::sites::once(async move {
            login(http)?;
            let page = page(http, &format!("{ROOT}/{name}/series")).await?;
            let list = extr(&page, "<ul class=\"content-list-wrap", "</ul>");
            let children = extract_iter(list, " href=\"", "\"")
                .into_iter()
                .filter_map(|path| Url::parse(&format!("{ROOT}{path}")).ok())
                .map(|url| (url, Map::new()))
                .collect();
            Ok(super::queue_urls(http, &SITE, &CREATOR, children))
        }),
        None => crate::sites::fail(error("unsupported Tapas URL")),
    }
}

/// `TapasExtractor.login` without credentials: the age-check cookies,
/// unless a `_cpc_` session cookie was supplied.
fn login(http: &Http) -> Result<()> {
    let root = http.api(REFERER)?;
    if http.cookie(&root, "_cpc_").is_none() {
        http.set_cookie(&root, "birthDate=1981-02-03; Path=/");
        http.set_cookie(&root, "adjustedBirthDate=1981-02-03; Path=/");
    }
    Ok(())
}

async fn page(http: &Http, url: &str) -> Result<String> {
    http.text_with(
        &http.api(url)?,
        &http.browser_headers(&[("Referer", REFERER)]),
    )
    .await
}

/// `request_api`: the JSON response's `data`.
async fn api(http: &Http, url: Url) -> Result<Value> {
    let mut response = http
        .json_with(
            &url,
            &http.browser_headers(&[
                ("Accept", "application/json, text/javascript, */*;"),
                ("Referer", REFERER),
            ]),
        )
        .await?;
    response
        .get_mut("data")
        .map(Value::take)
        .ok_or_else(|| error("Tapas returned no data"))
}

/// `_extract_series`: the series of an episode page, memoized by ID.
async fn series(http: &Http, html: &str, memo: &Memo) -> Result<Value> {
    let id = text::rextr(html, "data-series-id=\"", "\"");
    if let Some(series) = memo.get(id) {
        return Ok(series);
    }
    if !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return Err(error("Tapas returned an invalid series ID"));
    }
    let series = api(http, http.api(&format!("{ROOT}/series/{id}"))?).await?;
    memo.insert(id, series.clone());
    Ok(series)
}

/// `TapasEpisodeExtractor.items`.
fn episode_events(http: &Http, id: String, memo: Memo) -> Events<'_> {
    crate::sites::once(async move {
        login(http)?;
        let mut data = api(http, http.api(&format!("{ROOT}/episode/{id}"))?).await?;
        let html = super::str_of(&data, "html").to_owned();
        let Some(Value::Object(mut episode)) = data.get_mut("episode").map(Value::take) else {
            return Err(error("Tapas returned no episode"));
        };
        if !truthy(episode.get("free")) && !truthy(episode.get("unlocked")) {
            return Err(error(format!(
                "{id}: Episode '{}' not unlocked",
                episode
                    .get("title")
                    .map(crate::sites::scalar)
                    .unwrap_or_default()
            )));
        }
        set(&mut episode, "series", series(http, &html, &memo).await?);
        let published = episode
            .get("publish_date")
            .and_then(Value::as_str)
            .and_then(text::iso);
        set(&mut episode, "date", text::date(published));
        let items = if truthy(episode.get("book")) {
            book(&html, episode)?
        } else {
            comic(&html, episode)
        };
        let items = items
            .into_iter()
            .map(|item| {
                let num = item.keywords.get("num").map(crate::sites::scalar);
                item.ids(id.clone(), num.unwrap_or_default())
                    .timestamp(published)
                    .header("Referer", REFERER)
            })
            .collect();
        Ok(crate::sites::events(crate::sites::post(
            &SITE, http, &EPISODE, items,
        )))
    })
}

/// A novel chapter: the viewer's HTML as one `text:` document.
fn book(html: &str, mut episode: Map<String, Value>) -> Result<Vec<Item>> {
    let content = extr(html, "<div class=\"viewer\">", "<div class=\"viewer-bottom");
    let (url, job) = crate::extract::inline_text(content.to_owned())?;
    set(&mut episode, "num", 1);
    set(&mut episode, "extension", "html");
    // Upstream leaves `filename` unset for `text:` URLs.
    set(&mut episode, "filename", "");
    Ok(vec![Item::new(url, episode).job(job)])
}

/// A comic: every `data-src` image, numbered from one.
fn comic(html: &str, mut episode: Map<String, Value>) -> Vec<Item> {
    let mut items = Vec::new();
    for (index, raw) in extract_iter(html, "data-src=\"", "\"")
        .into_iter()
        .enumerate()
    {
        let raw = crate::sites::unescape(raw);
        set(&mut episode, "num", index + 1);
        nameext_into(&raw, &mut episode);
        if let Some(url) = crate::sites::media::web_url(&raw) {
            items.push(Item::new(url, episode.clone()));
        }
    }
    items
}

/// `TapasSeriesExtractor.items`: the series ID from its page, then every
/// episode, oldest first.
fn series_events(http: &Http, name: String) -> Events<'_> {
    let memo = Memo::default();
    crate::sites::once(async move {
        login(http)?;
        let page = page(http, &format!("{ROOT}/series/{name}")).await?;
        let (series_id, _, episode_id) = text::partition(
            extr(&page, "content=\"tapastic://series/", "\""),
            "/episodes/",
        );
        if series_id.is_empty() || !series_id.bytes().all(|b| b.is_ascii_digit()) {
            return Err(error("Tapas series ID not found"));
        }
        let endpoint = format!("{ROOT}/series/{series_id}/episodes");
        let episode_id = episode_id.to_owned();
        Ok(crate::sites::paginate(Some(1u32), move |state| {
            let (endpoint, episode_id, memo) = (endpoint.clone(), episode_id.clone(), memo.clone());
            async move {
                let Some(page_no) = state else {
                    return Ok(None);
                };
                let mut url = http.api(&endpoint)?;
                url.query_pairs_mut()
                    .append_pair("eid", &episode_id)
                    .append_pair("page", &page_no.to_string())
                    .append_pair("sort", "OLDEST")
                    .append_pair("last_access", "0")
                    .append_pair("max_limit", "20");
                let data = api(http, url).await?;
                let streams: Vec<Events<'_>> = data
                    .get("episodes")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|episode| {
                        let id = crate::sites::scalar(episode.get("id")?);
                        if !crate::sites::art::digits(&id) {
                            return None;
                        }
                        let memo = memo.clone();
                        Some(super::queue_with(
                            &SITE,
                            &SERIES,
                            episode.as_object()?,
                            move || episode_events(http, id, memo),
                        ))
                    })
                    .collect();
                let more = truthy(data.pointer("/pagination/has_next"));
                Ok(Some((
                    futures_util::stream::iter(streams).flatten().boxed(),
                    (more && page_no < 10_000).then_some(page_no + 1),
                )))
            }
        }))
    })
}

#[cfg(test)]
mod tests {
    use super::super::tests::{archive, path};
    use super::*;
    use crate::sites::testing::{html, http, json, keywords as kw, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://tapas.io/episode/2068651"),
            Some(Target::Episode("2068651".into()))
        );
        assert_eq!(
            t("https://tapas.io/series/just-leave-me-be/info"),
            Some(Target::Series("just-leave-me-be".into()))
        );
        assert_eq!(
            t("https://tapas.io/SANG123/series"),
            Some(Target::Creator("SANG123".into()))
        );
        for raw in [
            "https://tapas.io/",
            "https://tapas.io/episode/x",
            "https://tapas.io/series",
            "https://tapas.io/series/",
            "https://tapas.io/episodes",
            "https://www.tapas.io/SANG123",
            "https://m.tapas.io/SANG123",
            "https://tapas.io.evil.test/SANG123",
            "https://tapas.io:8443/SANG123",
            "https://u:p@tapas.io/SANG123",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn episode(book: bool, free: bool) -> Value {
        let html = if book {
            r#"<div data-series-id="199931"><div class="viewer"><p>Hello &amp; bye</p></div><div class="viewer-bottom">"#
        } else {
            r#"<a data-series-id="1"></a><div data-series-id="251031">
<img data-src="https://us-a.tapas.io/pc/aa/one.jpg?__token__=a&amp;b=1">
<img data-src="https://us-a.tapas.io/pc/bb/two.png">"#
        };
        json!({"data": {"html": html, "episode": {
            "id": 2717154, "title": "Episode 3", "free": free, "unlocked": false,
            "book": book, "publish_date": "2023-01-13T19:00:00Z"}}})
    }

    #[tokio::test]
    async fn extracts_comic_episode() {
        let (root, requests) = serve(vec![
            json(episode(false, true)),
            json(json!({"data": {"id": 251031, "title": "The Doctor Is Out"}})),
        ]);
        let http = http("Tapas", root);
        let files = media(extract(
            &http,
            Url::parse("https://tapas.io/episode/2717154").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].url.as_str(),
            "https://us-a.tapas.io/pc/aa/one.jpg?__token__=a&b=1"
        );
        assert_eq!(files[0].headers["Referer"], "https://tapas.io/");
        let k = kw(&files[1]);
        assert_eq!(k["series"]["title"], "The Doctor Is Out");
        assert_eq!(k["date"], "2023-01-13 19:00:00");
        assert_eq!(k["num"], 2);
        assert_eq!(k["filename"], "two");
        assert_eq!(k["extension"], "png");
        assert_eq!(
            path(&files[1]),
            "tapas/The Doctor Is Out/2717154 Episode 3/02.png"
        );
        assert_eq!(archive(&files[1]), r#"["{id}_{num}",["2717154","2"]]"#);
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /episode/2717154 "));
        assert!(requests[0].contains("accept: application/json, text/javascript, */*;"));
        assert!(requests[0].contains("birthDate=1981-02-03"));
        assert!(requests[1].starts_with("GET /series/251031 "));
    }

    #[tokio::test]
    async fn book_episode_is_inline_html() {
        let (root, requests) = serve(vec![
            json(episode(true, true)),
            json(json!({"data": {"id": 199931, "title": "Tomb Raider King"}})),
        ]);
        let http = http("Tapas", root);
        let files = media(extract(
            &http,
            Url::parse("https://tapas.io/episode/2068651").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let crate::extract::MediaJob::InlineText { data } = &files[0].job else {
            panic!("book episode is not inline text");
        };
        assert_eq!(data, "<p>Hello &amp; bye</p></div>");
        assert_eq!(kw(&files[0])["extension"], "html");
        assert_eq!(
            path(&files[0]),
            "tapas/Tomb Raider King/2717154 Episode 3/01.html"
        );
        requests.join().unwrap();
    }

    #[tokio::test]
    async fn locked_episode_is_an_error() {
        let (root, requests) = serve(vec![json(episode(false, false))]);
        let http = http("Tapas", root);
        let events = extract(&http, Url::parse("https://tapas.io/episode/1").unwrap());
        let results: Vec<_> = crate::event::media_only(events).collect().await;
        let error = results[0].as_ref().unwrap_err().to_string();
        assert!(error.contains("not unlocked"), "{error}");
        requests.join().unwrap();
    }

    #[tokio::test]
    async fn series_pages_episodes_and_reuses_series() {
        let list = |id: u64, next: bool| {
            json(json!({"data": {"episodes": [{"id": id, "title": "E"}],
                "pagination": {"has_next": next}}}))
        };
        let (root, requests) = serve(vec![
            html(r#"<meta content="tapastic://series/251031/episodes/77">"#),
            list(1, true),
            json(episode(false, true)),
            json(json!({"data": {"id": 251031, "title": "T"}})),
            list(2, false),
            json(episode(false, true)),
        ]);
        let http = http("Tapas", root);
        let files = media(extract(
            &http,
            Url::parse("https://tapas.io/series/the-doctor-is-out").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 4);
        assert_eq!(kw(&files[0])["subcategory"], "episode");
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /series/the-doctor-is-out "));
        assert!(
            requests[1].starts_with(
                "GET /series/251031/episodes?eid=77&page=1&sort=OLDEST&last_access=0&max_limit=20 "
            ),
            "{}",
            requests[1]
        );
        assert!(requests[2].starts_with("GET /episode/1 "));
        assert!(requests[3].starts_with("GET /series/251031 "));
        assert!(requests[4].contains("&page=2&"));
        assert!(requests[5].starts_with("GET /episode/2 "));
    }

    #[tokio::test]
    async fn creator_queues_series() {
        let (root, requests) = serve(vec![
            html(
                r#"<a href="/elsewhere"></a><ul class="content-list-wrap js-x">
<li><a href="/series/tomb-raider-king"></a></li></ul>"#,
            ),
            html(r#"<meta content="tapastic://series/9">"#),
            json(json!({"data": {"episodes": [], "pagination": {"has_next": false}}})),
        ]);
        let http = http("Tapas", root);
        let files = media(extract(
            &http,
            Url::parse("https://tapas.io/SANG123").unwrap(),
        ))
        .await;
        assert!(files.is_empty());
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /SANG123/series "));
        assert!(requests[1].starts_with("GET /series/tomb-raider-king "));
        assert!(requests[2].starts_with("GET /series/9/episodes?eid=&page=1&"));
    }
}
