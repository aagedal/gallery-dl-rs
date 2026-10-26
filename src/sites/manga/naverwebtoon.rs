//! Naver Webtoon episodes and comics, mirroring
//! gallery_dl/extractor/naverwebtoon.py.
use super::{Gallery, set};
use crate::error;
use crate::sites::gdl::{
    Scan, captures, extr, extract_iter, group, int_of, parse_query, re, str_of,
};
use crate::sites::{Events, Http, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://comic.naver.com";

static EPISODE: Route = Route::new(
    "episode",
    "{episode:>03}-{num:>02}.{extension}",
    &["{category}", "{comic}"],
    "{title_id}_{episode}_{num}",
);
static COMIC: Route = Route::new("comic", "{filename}.{extension}", &["{category}"], "");

pub static SITE: Site = Site {
    category: "naver-webtoon",
    name: "Naver Webtoon",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&EPISODE, &COMIC],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Episode(String, String),
    Comic(String),
}

fn target(url: &Url) -> Option<Target> {
    let episode = re!(
        r"^https?://comic\.naver\.com/(webtoon|challenge|bestChallenge)/detail(?:\.nhn)?\?([^#]+)"
    );
    if let Some(caps) = captures(episode, url) {
        return Some(Target::Episode(
            group(&caps, 1).into(),
            group(&caps, 2).into(),
        ));
    }
    let comic = re!(
        r"^https?://comic\.naver\.com/(webtoon|challenge|bestChallenge)/list(?:\.nhn)?\?([^#]+)"
    );
    captures(comic, url).map(|caps| Target::Comic(group(&caps, 2).into()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Naver Webtoon URL"));
    };
    match target {
        Target::Episode(path, query) => crate::sites::once(async move {
            let page = super::fetch(http, &format!("{ROOT}/{path}/detail?{query}")).await?;
            let query = parse_query(&query);
            let data = metadata(&page, &query);
            let view = extr(&page, "id=\"comic_view_area\"", "</div>");
            let pages = extract_iter(view, "<img src=\"", "\"")
                .into_iter()
                .filter(|url| !url.contains("/static/"))
                .map(|url| (url.to_owned(), None))
                .collect();
            Ok(crate::sites::events(
                Gallery::numbered(&EPISODE, data, pages).events(&SITE, http),
            ))
        }),
        Target::Comic(query) => {
            let query = parse_query(&query);
            let title_id = query.get("titleId").cloned().unwrap_or(Value::Null);
            let page = match query.get("page") {
                Some(Value::String(p)) => crate::sites::art::parse_int(p, 1),
                _ => 1,
            };
            let sort = query
                .get("sort")
                .map(crate::sites::scalar)
                .unwrap_or_else(|| "ASC".into());
            crate::sites::paginate(Some(page), move |state| {
                let title_id = title_id.clone();
                let sort = sort.clone();
                async move {
                    let Some(page) = state else {
                        return Ok(None);
                    };
                    let mut url = http.api(&format!("{ROOT}/api/article/list"))?;
                    url.query_pairs_mut()
                        .append_pair("titleId", &crate::sites::scalar(&title_id))
                        .append_pair("page", &page.to_string())
                        .append_pair("sort", &sort);
                    let data = http
                        .json_with(&url, &[("Accept", "application/json, text/plain, */*")])
                        .await?;
                    let path = str_of(&data, "webtoonLevelCode")
                        .to_lowercase()
                        .replacen("_c", "C", 1);
                    let base = format!(
                        "{ROOT}/{path}/detail?titleId={}&no=",
                        crate::sites::scalar(&data["titleId"])
                    );
                    let children = data["articleList"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|article| {
                            let url = Url::parse(&format!(
                                "{base}{}",
                                crate::sites::scalar(&article["no"])
                            ))
                            .ok()?;
                            Some((url, article.as_object().cloned().unwrap_or_default()))
                        })
                        .collect();
                    let next = int_of(data["pageInfo"].get("nextPage"));
                    let events = super::queue_urls(http, &SITE, &COMIC, children);
                    Ok(Some((events, (next != 0).then_some(next))))
                }
            })
        }
    }
}

fn metadata(page: &str, query: &Map<String, Value>) -> Map<String, Value> {
    let mut scan = Scan::new(page);
    let names = |text: &str| -> Vec<String> {
        extract_iter(text, "\"name\":\"", "\"")
            .into_iter()
            .map(|a| a.trim().to_owned())
            .collect()
    };
    let mut data = Map::new();
    set(
        &mut data,
        "title_id",
        query.get("titleId").cloned().unwrap_or(Value::Null),
    );
    set(
        &mut data,
        "episode",
        query.get("no").cloned().unwrap_or(Value::Null),
    );
    set(&mut data, "comic", scan.extr("titleName: \"", "\""));
    let tags: Vec<String> = extract_iter(scan.extr("tagList: [", "],"), "\"tagName\":\"", "\"")
        .into_iter()
        .map(|t| t.trim().to_owned())
        .collect();
    set(&mut data, "tags", tags);
    set(&mut data, "title", scan.extr("\"subtitle\":\"", "\""));
    set(&mut data, "author", names(scan.extr("\"writers\":[", "]")));
    set(&mut data, "artist", names(scan.extr("\"painters\":[", "]")));
    data
}

#[cfg(test)]
mod tests {
    use super::super::tests::{archive, path};
    use super::*;
    use crate::sites::testing::{children, html, http, json, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://comic.naver.com/webtoon/detail?titleId=26458&no=1&weekday=tue"),
            Some(Target::Episode(
                "webtoon".into(),
                "titleId=26458&no=1&weekday=tue".into()
            ))
        );
        assert!(matches!(
            t("https://comic.naver.com/bestChallenge/detail.nhn?titleId=771467&no=3"),
            Some(Target::Episode(..))
        ));
        assert_eq!(
            t("https://comic.naver.com/challenge/list?titleId=765124"),
            Some(Target::Comic("titleId=765124".into()))
        );
        assert_eq!(t("https://comic.naver.com/webtoon/list"), None);
    }

    #[tokio::test]
    async fn extracts_episode() {
        let page = r#"titleName: "Comic", tagList: [{"tagName":" a "},{"tagName":"b"}],
            "subtitle":"Ep 1","writers":[{"name":"W "}],"painters":[{"name":"P"}]
            <div id="comic_view_area"><img src="https://image-comic.pstatic.net/static/x.gif">
            <img src="https://image-comic.pstatic.net/webtoon/26458/1/a_1.jpg"><img src="https://image-comic.pstatic.net/webtoon/26458/1/a_2.jpg"></div>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = http("naver-webtoon", root);
        let files = media(extract(
            &http,
            Url::parse("https://comic.naver.com/webtoon/detail?titleId=26458&no=1").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[1]);
        assert_eq!(k["comic"], "Comic");
        assert_eq!(k["tags"], serde_json::json!(["a", "b"]));
        assert_eq!(k["author"], serde_json::json!(["W"]));
        assert_eq!(k["episode"], "1");
        assert_eq!(path(&files[1]), "naver-webtoon/Comic/001-02.jpg");
        assert_eq!(
            archive(&files[0]),
            r#"["{title_id}_{episode}_{num}",["26458","1","1"]]"#
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn comic_pages_through_articles() {
        let (root, server) = serve(vec![
            json(
                serde_json::json!({"webtoonLevelCode": "BEST_CHALLENGE", "titleId": 5,
                "articleList": [{"no": 1}, {"no": 2}], "pageInfo": {"nextPage": 2}}),
            ),
            json(
                serde_json::json!({"webtoonLevelCode": "BEST_CHALLENGE", "titleId": 5,
                "articleList": [{"no": 3}], "pageInfo": {"nextPage": 0}}),
            ),
        ]);
        let http = http("naver-webtoon", root);
        let queued = children(extract(
            &http,
            Url::parse("https://comic.naver.com/bestChallenge/list?titleId=5").unwrap(),
        ))
        .await;
        assert_eq!(queued.len(), 3);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/article/list?titleId=5&page=1&sort=ASC "));
        assert!(requests[1].starts_with("GET /api/article/list?titleId=5&page=2&sort=ASC "));
    }
}
