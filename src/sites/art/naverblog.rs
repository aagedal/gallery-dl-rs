//! Naver Blog, mirroring gallery_dl/extractor/naverblog.py.
//!
//! The `videos` option is honored. Upstream's EUC-KR fallback for lazily
//! loaded image URLs that are not valid UTF-8 is not implemented.
use super::{Scan, extr, extract_iter, parse_int};
use crate::sites::{Events, Http, Item, Route, Site, unescape};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

pub static SITE: Site = Site {
    category: "naver-blog",
    name: "Naver Blog",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&POST, &BLOG],
    api_key: false,
    cookies: None,
};

static POST: Route = Route::new(
    "post",
    "{num:>03}.{extension}",
    &[
        "{category}",
        "{blog[user]} {blog[id]}",
        "{post[date]:%Y-%m-%d} {post[title]}",
    ],
    "{blog[id]}_{post[num]}_{num}",
);
static BLOG: Route = Route::new("blog", "{filename}.{extension}", &["{category}"], "");

const ROOT: &str = "https://blog.naver.com";
const REFERER: &str = "https://blog.naver.com/";

#[derive(Debug, PartialEq)]
enum Target {
    Post { blog: String, post: String },
    Blog(String),
}

/// Python `\w` restricted to ASCII.
fn word(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) || url.host_str() != Some("blog.naver.com") {
        return None;
    }
    let path = url.path();
    let query = url.query().unwrap_or_default();
    // `$` anchors: no query or fragment for the path forms.
    let bare = url.query().is_none() && url.fragment().is_none();
    if matches!(path, "/PostView.naver" | "/PostView.nhn") {
        let rest = query.strip_prefix("blogId=")?;
        let blog_len = rest
            .bytes()
            .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_')
            .count();
        let (blog, rest) = rest.split_at(blog_len);
        let rest = rest.strip_prefix("&logNo=")?;
        let post_len = rest.bytes().take_while(u8::is_ascii_digit).count();
        return (word(blog) && post_len > 0).then(|| Target::Post {
            blog: blog.into(),
            post: rest[..post_len].into(),
        });
    }
    if matches!(path, "/PostList.naver" | "/PostList.nhn") {
        let blog = query
            .split('&')
            .find_map(|pair| pair.strip_prefix("blogId="))?;
        return (!blog.is_empty()).then(|| Target::Blog(crate::sites::percent_decode(blog)));
    }
    if !bare {
        return None;
    }
    let trimmed = path.strip_prefix('/')?;
    let trimmed = trimmed.strip_suffix('/').unwrap_or(trimmed);
    match trimmed.split('/').collect::<Vec<_>>().as_slice() {
        [blog, post] if word(blog) && super::digits(post) => Some(Target::Post {
            blog: (*blog).into(),
            post: (*post).into(),
        }),
        [blog] if word(blog) => Some(Target::Blog((*blog).into())),
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Post { blog, post }) => {
            crate::sites::once(async move { post_events(http, &blog, &post).await })
        }
        Some(Target::Blog(blog)) => blog_events(http, blog),
        None => crate::sites::fail(error("unsupported Naver Blog URL")),
    }
}

fn nested_date(seconds: Option<i64>) -> Value {
    crate::filter::source_date_json(seconds)
}

/// Midnight UTC of the current day.
fn today() -> i64 {
    time::OffsetDateTime::now_utc()
        .unix_timestamp()
        .div_euclid(86400)
        * 86400
}

/// Upstream `_parse_datetime`: relative dates ("N시간 전") mean today.
fn parse_date(text: &str, today: i64) -> Option<i64> {
    if text.contains('전') {
        return Some(today);
    }
    super::strptime(text, "%Y. %m. %d. %H:%M")
}

pub(crate) fn metadata(
    page: &str,
    blog: &str,
    post: &str,
    today: i64,
) -> (Map<String, Value>, Option<i64>) {
    let mut scan = Scan::new(page);
    let title = unescape(scan.extr("\"og:title\" content=\"", "\""));
    let description =
        unescape(scan.extr("\"og:description\" content=\"", "\"")).replace("&nbsp;", " ");
    let blog_num = parse_int(scan.extr("var blogNo = '", "'"), 0);
    let user = scan.extr("var nickName = '", "'");
    let mut date = scan.extr("se_publishDate pcol2\">", "<");
    if date.is_empty() {
        date = scan.extr("_postAddDate\">", "<");
    }
    let timestamp = parse_date(date, today);
    let mut post_data = Map::new();
    post_data.insert("title".into(), title.into());
    post_data.insert("description".into(), description.into());
    post_data.insert("num".into(), parse_int(post, 0).into());
    post_data.insert("date".into(), nested_date(timestamp));
    let mut blog_data = Map::new();
    blog_data.insert("id".into(), blog.into());
    blog_data.insert("num".into(), blog_num.into());
    blog_data.insert("user".into(), user.into());
    let mut data = Map::new();
    data.insert("post".into(), post_data.into());
    data.insert("blog".into(), blog_data.into());
    (data, timestamp)
}

pub(crate) fn image_urls(page: &str) -> Vec<String> {
    extract_iter(page, "data-lazy-src=\"", "\"")
        .into_iter()
        .map(|url| {
            let url = url.replacen("://post", "://blog", 1);
            url.split('?').next().unwrap_or_default().to_owned()
        })
        .collect()
}

/// `v2_video` modules: (vid, inkey) pairs.
fn video_modules(page: &str) -> Vec<(String, String)> {
    extract_iter(page, " data-module='", "'")
        .into_iter()
        .filter(|module| module.contains("\"v2_video\""))
        .filter_map(|module| {
            let value: Value = serde_json::from_str(module).ok()?;
            let data = value.get("data")?;
            Some((
                data.get("vid")?.as_str()?.to_owned(),
                data.get("inkey")?.as_str()?.to_owned(),
            ))
        })
        .collect()
}

async fn video(http: &Http, vid: &str, inkey: &str) -> Result<(String, Map<String, Value>)> {
    if !vid
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(error("invalid Naver video id"));
    }
    let mut url = http.api(&format!(
        "https://apis.naver.com/rmcnmv/rmcnmv/vod/play/v2.0/{vid}"
    ))?;
    let nonce = time::OffsetDateTime::now_utc().unix_timestamp().to_string();
    url.query_pairs_mut()
        .append_pair("key", inkey)
        .append_pair("sid", "2")
        .append_pair("nonce", &nonce)
        .append_pair("devt", "html5_pc")
        .append_pair("prv", "N")
        .append_pair("aup", "N")
        .append_pair("stpb", "N")
        .append_pair("cpl", "ko_KR")
        .append_pair("providerEnv", "real")
        .append_pair("adt", "glad")
        .append_pair("lc", "ko_KR");
    let data = http.json_with(&url, &[("Referer", REFERER)]).await?;
    best_video(&data).ok_or_else(|| error("Naver video response contains no streams"))
}

fn best_video(data: &Value) -> Option<(String, Map<String, Value>)> {
    let list = data.get("videos")?.get("list")?.as_array()?;
    // Python `max` keeps the first maximal element.
    let mut best: Option<&Value> = None;
    for video in list {
        let size = video.get("size").and_then(Value::as_f64).unwrap_or(0.0);
        if best.is_none_or(|b| size > b.get("size").and_then(Value::as_f64).unwrap_or(0.0)) {
            best = Some(video);
        }
    }
    let video = best?.as_object()?.clone();
    Some((video.get("source")?.as_str()?.to_owned(), video))
}

async fn post_events<'a>(http: &'a Http, blog: &str, post: &str) -> Result<Events<'a>> {
    let mut url = http.api(&format!("{ROOT}/PostView.nhn"))?;
    url.query_pairs_mut()
        .append_pair("blogId", blog)
        .append_pair("logNo", post);
    let page = http.text_with(&url, &[("Referer", REFERER)]).await?;
    let (mut data, timestamp) = metadata(&page, blog, post, today());
    let mut files: Vec<(String, Option<Map<String, Value>>)> =
        image_urls(&page).into_iter().map(|u| (u, None)).collect();
    let modules = if http.option_bool("videos", true) {
        video_modules(&page)
    } else {
        Vec::new()
    };
    for (vid, inkey) in modules {
        // Upstream logs a warning and continues on video failures.
        if let Ok((url, info)) = video(http, &vid, &inkey).await {
            files.push((url, Some(info)));
        }
    }
    data.insert("count".into(), files.len().into());
    let title = data["post"]["title"].as_str().map(str::to_owned);
    let user = data["blog"]["user"].as_str().map(str::to_owned);
    let mut items = Vec::new();
    for (index, (raw, info)) in files.into_iter().enumerate() {
        // Like upstream, merged video data persists for later files.
        if let Some(info) = &info {
            super::update(&mut data, info);
        }
        data.insert("num".into(), (index + 1).into());
        if info.as_ref().is_none_or(|i| !i.contains_key("extension")) {
            super::nameext_into(&raw, &mut data);
        }
        let Some(url) = super::file_url(&raw, REFERER) else {
            continue;
        };
        items.push(
            Item::new(url, data.clone())
                .ids(post, (index + 1).to_string())
                .user(user.clone())
                .caption(title.clone())
                .timestamp(timestamp)
                .header("Referer", REFERER),
        );
    }
    Ok(crate::sites::events(crate::sites::post(
        &SITE, http, &POST, items,
    )))
}

struct Cursor {
    log_no: String,
    sort_date: String,
}

fn blog_events(http: &Http, blog: String) -> Events<'_> {
    crate::sites::paginate(None::<Option<Cursor>>, move |state| {
        let blog = blog.clone();
        async move {
            let cursor = match state {
                Some(Some(cursor)) => cursor,
                Some(None) => return Ok(None),
                None => {
                    let mut url = http.api(&format!("{ROOT}/PostList.nhn"))?;
                    url.query_pairs_mut().append_pair("blogId", &blog);
                    let page = http.text_with(&url, &[("Referer", REFERER)]).await?;
                    let first = extr(&page, "gnFirstLogNo = \"", "\"");
                    Cursor {
                        log_no: if first.is_empty() { "0" } else { first }.to_owned(),
                        sort_date: String::new(),
                    }
                }
            };
            let mut url = http.api(&format!("{ROOT}/PostViewBottomTitleListAsync.nhn"))?;
            url.query_pairs_mut()
                .append_pair("blogId", &blog)
                .append_pair("logNo", &cursor.log_no)
                .append_pair("viewDate", "")
                .append_pair("categoryNo", "")
                .append_pair("parentCategoryNo", "")
                .append_pair("showNextPage", "true")
                .append_pair("showPreviousPage", "false")
                .append_pair("sortDateInMilli", &cursor.sort_date)
                .append_pair("isThumbnailViewType", "false")
                .append_pair("countPerPage", "");
            let data = http.json_with(&url, &[("Referer", REFERER)]).await?;
            let (children, next) = parse_list(&blog, &data)?;
            let events = futures_util::StreamExt::boxed(futures_util::StreamExt::flatten(
                futures_util::stream::iter(children.into_iter().map(move |(child, post)| {
                    crate::sites::queue(http, "naver-blog", "blog", child, &post)
                })),
            ));
            Ok(Some((events, Some(next))))
        }
    })
}

type Children = Vec<(Url, Map<String, Value>)>;

fn parse_list(blog: &str, data: &Value) -> Result<(Children, Option<Cursor>)> {
    let posts = data
        .get("postList")
        .and_then(Value::as_array)
        .ok_or_else(|| error("Naver Blog post list response is invalid"))?;
    let mut children = Vec::new();
    for post in posts {
        let Some(mut post) = post.as_object().cloned() else {
            continue;
        };
        let Some(log_no) = post.get("logNo").map(crate::sites::scalar) else {
            continue;
        };
        let raw = format!("{ROOT}/PostView.nhn?blogId={blog}&logNo={log_no}");
        post.insert("url".into(), raw.clone().into());
        if let Ok(child) = Url::parse(&raw) {
            children.push((child, post));
        }
    }
    let next = super::truthy(data.get("hasNextPage")).then(|| Cursor {
        log_no: data
            .get("nextIndexLogNo")
            .map(crate::sites::scalar)
            .unwrap_or_default(),
        sort_date: data
            .get("nextIndexSortDate")
            .map(crate::sites::scalar)
            .unwrap_or_default(),
    });
    Ok((children, next))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, json, keywords, media, serve};
    use serde_json::json;

    fn t(raw: &str) -> Option<Target> {
        target(&Url::parse(raw).unwrap())
    }

    #[test]
    fn classifies_urls() {
        let post = Some(Target::Post {
            blog: "rlfqjxm0".into(),
            post: "221430673006".into(),
        });
        assert_eq!(t("https://blog.naver.com/rlfqjxm0/221430673006"), post);
        assert_eq!(t("https://blog.naver.com/rlfqjxm0/221430673006/"), post);
        assert_eq!(
            t("https://blog.naver.com/PostView.naver?blogId=rlfqjxm0&logNo=221430673006"),
            post
        );
        assert_eq!(
            t("https://blog.naver.com/PostView.nhn?blogId=rlfqjxm0&logNo=221430673006&x=1"),
            post
        );
        assert_eq!(
            t("https://blog.naver.com/gukjung"),
            Some(Target::Blog("gukjung".into()))
        );
        assert_eq!(
            t("https://blog.naver.com/PostList.naver?from=x&blogId=gukjung"),
            Some(Target::Blog("gukjung".into()))
        );
        for raw in [
            "https://www.blog.naver.com/gukjung",
            "https://blog.naver.com.evil.com/gukjung",
            "https://evilblog.naver.com/gukjung",
            "https://blog.naver.com:444/gukjung",
            "https://u@blog.naver.com/gukjung",
            "https://blog.naver.com/",
            "https://blog.naver.com/gukjung?x=1",
            "https://blog.naver.com/a/b/c",
            "https://blog.naver.com/a/12x",
            "https://blog.naver.com/PostView.nhn?logNo=1&blogId=a",
            "https://blog.naver.com/PostList.nhn?x=1",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    const PAGE: &str = r#"<meta property="og:title" content="Post &amp; title"/>
<meta property="og:description" content="desc&amp;nbsp;x"/>
var blogNo = '12345'; var nickName = 'nick';
<span class="se_publishDate pcol2">2019. 1. 5. 13:14</span>
<img data-lazy-src="https://postfiles.pstatic.net/a/b/image%201.jpg?type=w80" />
<img data-lazy-src="https://blogfiles.pstatic.net/c/two.PNG" />
<div data-module='{"type":"v2_video","data":{"vid":"ABC123","inkey":"K"}}'></div>"#;

    #[tokio::test]
    async fn extracts_post_with_video() {
        let (root, server) = serve(vec![
            html(PAGE),
            json(json!({"videos": {"list": [
                {"source": "https://v.test/low.mp4", "size": 10},
                {"source": "https://v.test/high.mp4?x", "size": 20, "encodingOption": {"id": "1080"}}
            ]}})),
        ]);
        let http = http("naver-blog", root);
        let files = media(extract(
            &http,
            Url::parse("https://blog.naver.com/user_1/221430673006").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 3);
        assert_eq!(
            files[0].url.as_str(),
            "https://blogfiles.pstatic.net/a/b/image%201.jpg"
        );
        assert_eq!(files[2].url.as_str(), "https://v.test/high.mp4?x");
        assert_eq!(files[0].headers["Referer"], "https://blog.naver.com/");
        let k = keywords(&files[0]);
        assert_eq!(k["post"]["title"], "Post & title");
        assert_eq!(k["post"]["description"], "desc x");
        assert_eq!(k["post"]["num"], 221430673006u64);
        assert_eq!(
            k["blog"],
            json!({"id": "user_1", "num": 12345, "user": "nick"})
        );
        assert_eq!(k["count"], 3);
        assert_eq!(k["filename"], "image 1");
        let k = keywords(&files[2]);
        assert_eq!(k["size"], 20);
        assert_eq!(k["extension"], "mp4");
        assert_eq!(k["num"], 3);
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "002.png");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("naver-blog/nick user_1/2019-01-05 Post & title")
        );
        let (_, identity) = crate::sites::archive_key(&files[1]).unwrap();
        assert_eq!(
            identity,
            r#"["{blog[id]}_{post[num]}_{num}",["user_1","221430673006","2"]]"#
        );
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("GET /PostView.nhn?blogId=user_1&logNo=221430673006 "),
            "{}",
            requests[0]
        );
        assert!(
            requests[1].starts_with("GET /rmcnmv/rmcnmv/vod/play/v2.0/ABC123?key=K&sid=2&nonce=")
        );
    }

    #[test]
    fn relative_dates_mean_today() {
        assert_eq!(parse_date("3시간 전", 86400), Some(86400));
        assert_eq!(parse_date("2019. 01. 05. 13:14", 0), Some(1546694040));
    }

    #[tokio::test]
    async fn blog_queues_posts_and_paginates() {
        let (root, server) = serve(vec![
            html("var gnFirstLogNo = \"999\";"),
            json(json!({"postList": [{"logNo": "999", "title": "a"}],
                "hasNextPage": true, "nextIndexLogNo": "998", "nextIndexSortDate": "2020"})),
            html(PAGE.split("<div").next().unwrap()),
            json(json!({"postList": [], "hasNextPage": false})),
        ]);
        let http = http("naver-blog", root);
        let files = media(extract(
            &http,
            Url::parse("https://blog.naver.com/user_1").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(keywords(&files[0])["subcategory"], "post");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /PostList.nhn?blogId=user_1 "));
        assert!(requests[1].starts_with(
            "GET /PostViewBottomTitleListAsync.nhn?blogId=user_1&logNo=999&viewDate=&categoryNo=&parentCategoryNo=&showNextPage=true&showPreviousPage=false&sortDateInMilli=&"
        ), "{}", requests[1]);
        assert!(requests[2].starts_with("GET /PostView.nhn?blogId=user_1&logNo=999 "));
        assert!(
            requests[3].contains("logNo=998&") && requests[3].contains("sortDateInMilli=2020&")
        );
    }
}
