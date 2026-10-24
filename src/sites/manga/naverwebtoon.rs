//! Naver Webtoon episodes and comics, mirroring
//! gallery_dl/extractor/naverwebtoon.py.
use super::{Gallery, keywords};
use crate::sites::art::{Scan, extr, extract_iter};
use crate::sites::media::{parse_query, query_get};
use crate::sites::{Events, Http, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

pub static SITE: Site = Site {
    category: "naver-webtoon",
    name: "Naver Webtoon",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&EPISODE, &COMIC],
    api_key: false,
    cookies: None,
};

const ROOT: &str = "https://comic.naver.com";
const REFERER: &str = "https://comic.naver.com/";

static EPISODE: Route = Route::new(
    "episode",
    "{episode:>03}-{num:>02}.{extension}",
    &["{category}", "{comic}"],
    "{title_id}_{episode}_{num}",
);
static COMIC: Route = Route::new("comic", "{filename}.{extension}", &["{category}"], "");

#[derive(Debug, PartialEq)]
enum Target {
    /// `PATH/detail?QUERY`.
    Episode { path: String, query: String },
    /// `PATH/list?QUERY`.
    Comic { query: String },
}

/// `comic\.naver\.com/(webtoon|challenge|bestChallenge)/(detail|list)(?:\.nhn)?\?([^#]+)`.
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) || url.host_str() != Some("comic.naver.com") {
        return None;
    }
    let query = url.query().filter(|q| !q.is_empty())?.to_owned();
    let raw: Vec<&str> = url.path().split('/').skip(1).collect();
    let [path, page] = raw.as_slice() else {
        return None;
    };
    if !matches!(*path, "webtoon" | "challenge" | "bestChallenge") {
        return None;
    }
    let path = (*path).to_owned();
    match *page {
        "detail" | "detail.nhn" => Some(Target::Episode { path, query }),
        "list" | "list.nhn" => Some(Target::Comic { query }),
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Episode { path, query }) => crate::sites::once(async move {
            let url = http.api(&format!("{ROOT}/{path}/detail?{query}"))?;
            let page = http
                .text_with(&url, &http.browser_headers(&[("Referer", REFERER)]))
                .await?;
            Ok(crate::sites::events(episode_events(http, &page, &query)))
        }),
        Some(Target::Comic { query }) => comic_events(http, &query),
        None => crate::sites::fail(error("unsupported Naver Webtoon URL")),
    }
}

/// `NaverWebtoonEpisodeExtractor.metadata`.
fn metadata(page: &str, query: &str) -> Map<String, Value> {
    let params = parse_query(Some(query));
    let param = |name| query_get(&params, name).map_or(Value::Null, Value::from);
    let names = |list: &str, begin: &str| {
        Value::from(
            extract_iter(list, begin, "\"")
                .into_iter()
                .map(|name| name.trim().to_owned())
                .collect::<Vec<_>>(),
        )
    };
    let mut e = Scan::new(page);
    let comic = e.extr("titleName: \"", "\"");
    let tags = names(e.extr("tagList: [", "],"), "\"tagName\":\"");
    let title = e.extr("\"subtitle\":\"", "\"");
    let author = names(e.extr("\"writers\":[", "]"), "\"name\":\"");
    let artist = names(e.extr("\"painters\":[", "]"), "\"name\":\"");
    keywords([
        ("title_id", param("titleId")),
        ("episode", param("no")),
        ("comic", comic.into()),
        ("tags", tags),
        ("title", title.into()),
        ("author", author),
        ("artist", artist),
    ])
}

/// `NaverWebtoonEpisodeExtractor.images`: the viewer's images, without
/// static assets.
fn images(page: &str) -> Vec<String> {
    let view = extr(page, "id=\"comic_view_area\"", "</div>");
    extract_iter(view, "<img src=\"", "\"")
        .into_iter()
        .filter(|url| !url.contains("/static/"))
        .map(str::to_owned)
        .collect()
}

fn episode_events(http: &Http, page: &str, query: &str) -> Vec<crate::event::ExtractionEvent> {
    let data = metadata(page, query);
    let id = format!(
        "{}_{}",
        data.get("title_id")
            .map(crate::sites::scalar)
            .unwrap_or_default(),
        data.get("episode")
            .map(crate::sites::scalar)
            .unwrap_or_default()
    );
    let pages = images(page).into_iter().map(|url| (url, None)).collect();
    let mut gallery = Gallery::chapter(&EPISODE, data, pages)
        .id(id)
        .header("Referer", REFERER);
    gallery.enum_key = "num";
    gallery.events(&SITE, http)
}

/// `NaverWebtoonComicExtractor.items`: the article list API, page by page,
/// queueing each episode under its comic's level path.
fn comic_events<'a>(http: &'a Http, query: &str) -> Events<'a> {
    let params = parse_query(Some(query));
    let title_id = query_get(&params, "titleId").map(str::to_owned);
    let first = match query_get(&params, "page") {
        Some(page) if !page.is_empty() => crate::sites::art::parse_int(page, 1),
        _ => 1,
    };
    let sort = query_get(&params, "sort").unwrap_or("ASC").to_owned();
    crate::sites::paginate(Some(first), move |state| {
        let (title_id, sort) = (title_id.clone(), sort.clone());
        async move {
            let Some(page_no) = state else {
                return Ok(None);
            };
            let data = articles(http, title_id.as_deref(), page_no, &sort).await?;
            let path = crate::sites::scalar(data.get("webtoonLevelCode").unwrap_or(&Value::Null))
                .to_lowercase()
                .replacen("_c", "C", 1);
            let base = format!(
                "{ROOT}/{path}/detail?titleId={}&no=",
                crate::sites::scalar(data.get("titleId").unwrap_or(&Value::Null))
            );
            let children = data
                .get("articleList")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|article| {
                    let no = crate::sites::scalar(article.get("no")?);
                    let url = Url::parse(&format!("{base}{no}")).ok()?;
                    Some((url, article.as_object()?.clone()))
                })
                .collect();
            let events = super::queue_urls(http, &SITE, &COMIC, children);
            let next = data
                .pointer("/pageInfo/nextPage")
                .and_then(Value::as_i64)
                .filter(|next| *next != 0 && *next != page_no);
            Ok(Some((events, next)))
        }
    })
}

async fn articles(http: &Http, title_id: Option<&str>, page: i64, sort: &str) -> Result<Value> {
    let mut url = http.api(&format!("{ROOT}/api/article/list"))?;
    {
        let mut query = url.query_pairs_mut();
        if let Some(title_id) = title_id {
            query.append_pair("titleId", title_id);
        }
        query
            .append_pair("page", &page.to_string())
            .append_pair("sort", sort);
    }
    http.json_with(
        &url,
        &http.browser_headers(&[
            ("Accept", "application/json, text/plain, */*"),
            ("Referer", REFERER),
        ]),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::super::tests::{archive, path};
    use super::*;
    use crate::sites::testing::{html, http, json, keywords as kw, media, serve};
    use serde_json::json;

    const EPISODE_URL: &str =
        "https://comic.naver.com/webtoon/detail?titleId=26458&no=1&weekday=tue";

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t(EPISODE_URL),
            Some(Target::Episode {
                path: "webtoon".into(),
                query: "titleId=26458&no=1&weekday=tue".into()
            })
        );
        assert!(matches!(
            t("https://comic.naver.com/bestChallenge/detail.nhn?titleId=771467&no=3"),
            Some(Target::Episode { ref path, .. }) if path == "bestChallenge"
        ));
        assert_eq!(
            t("http://comic.naver.com/challenge/list?titleId=765124"),
            Some(Target::Comic {
                query: "titleId=765124".into()
            })
        );
        for raw in [
            "https://comic.naver.com/webtoon/detail",
            "https://comic.naver.com/webtoon/list?",
            "https://comic.naver.com/other/list?titleId=1",
            "https://comic.naver.com/webtoon/list/x?titleId=1",
            "https://m.comic.naver.com/webtoon/list?titleId=1",
            "https://comic.naver.com.evil.test/webtoon/list?titleId=1",
            "https://comic.naver.com:8443/webtoon/list?titleId=1",
            "https://u:p@comic.naver.com/webtoon/list?titleId=1",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    const PAGE: &str = r#"<script>
titleName: "N의등대-눈의등대",
tagList: [{"tagName":" 스릴러 "},{"tagName":"완결무료"}],
"subtitle":"n의 등대 1화",
"writers":[{"name":"김규삼 "}],
"painters":[],
</script>
<div class="wt_viewer" id="comic_view_area">
<img src="https://ssl.pstatic.net/static/comic/blank.gif" />
<img src="https://image-comic.pstatic.net/webtoon/26458/1/a_IMAG01_1.jpg" alt="" />
<img src="https://image-comic.pstatic.net/webtoon/26458/1/a_IMAG01_2.jpg" alt="" />
</div>
<img src="https://image-comic.pstatic.net/outside.jpg" />"#;

    #[tokio::test]
    async fn extracts_episode() {
        let (root, requests) = serve(vec![html(PAGE)]);
        let http = http("Naver Webtoon", root);
        let files = media(extract(&http, Url::parse(EPISODE_URL).unwrap())).await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[1].url.as_str(),
            "https://image-comic.pstatic.net/webtoon/26458/1/a_IMAG01_2.jpg"
        );
        assert_eq!(files[0].headers["Referer"], "https://comic.naver.com/");
        let k = kw(&files[1]);
        assert_eq!(k["comic"], "N의등대-눈의등대");
        assert_eq!(k["tags"], json!(["스릴러", "완결무료"]));
        assert_eq!(k["title"], "n의 등대 1화");
        assert_eq!(k["author"], json!(["김규삼"]));
        assert_eq!(k["artist"], json!([]));
        assert_eq!(k["title_id"], "26458");
        assert_eq!(k["episode"], "1");
        assert_eq!(k["num"], 2);
        assert_eq!(k["count"], 2);
        assert_eq!(k["filename"], "a_IMAG01_2");
        assert_eq!(k["subcategory"], "episode");
        assert_eq!(path(&files[1]), "naver-webtoon/N의등대-눈의등대/001-02.jpg");
        assert_eq!(
            archive(&files[1]),
            r#"["{title_id}_{episode}_{num}",["26458","1","2"]]"#
        );
        let requests = requests.join().unwrap();
        assert!(
            requests[0].starts_with("GET /webtoon/detail?titleId=26458&no=1&weekday=tue "),
            "{}",
            requests[0]
        );
    }

    #[tokio::test]
    async fn comic_pages_through_article_list() {
        let list = |no: u32, next: u32| {
            json(json!({
                "titleId": 765124,
                "webtoonLevelCode": "BEST_CHALLENGE",
                "articleList": [{"no": no, "subtitle": "s"}],
                "pageInfo": {"nextPage": next},
            }))
        };
        let (root, requests) = serve(vec![list(1, 2), html(PAGE), list(2, 0), html(PAGE)]);
        let http = http("Naver Webtoon", root);
        let files = media(extract(
            &http,
            Url::parse("https://comic.naver.com/webtoon/list?titleId=765124").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 4);
        let requests = requests.join().unwrap();
        assert!(
            requests[0].starts_with("GET /api/article/list?titleId=765124&page=1&sort=ASC "),
            "{}",
            requests[0]
        );
        assert!(requests[0].contains("accept: application/json, text/plain, */*"));
        assert!(requests[1].starts_with("GET /bestChallenge/detail?titleId=765124&no=1 "));
        assert!(requests[2].starts_with("GET /api/article/list?titleId=765124&page=2&sort=ASC "));
        assert!(requests[3].starts_with("GET /bestChallenge/detail?titleId=765124&no=2 "));
    }
}
