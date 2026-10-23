//! Bilibili, mirroring gallery_dl/extractor/bilibili.py.
//!
//! Articles ("opus") are read from the page's `window.__INITIAL_STATE__`,
//! or from the web-dynamic `detail` API when the page is risk-controlled;
//! user and favorite article lists come from the web-dynamic API and are
//! queued as article jobs. The `livephoto` option (default true) also emits
//! live-photo videos. Favorites need the `SESSDATA` cookie.
use crate::sites::art::{extr, nameext_into, truthy};
use crate::sites::{Events, Http, Item, Route, Site, plain, scalar};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

pub static SITE: Site = Site {
    category: "bilibili",
    name: "Bilibili",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&ARTICLE, &USER_ARTICLES, &USER_ARTICLES_FAVORITE],
    api_key: false,
    cookies: Some("bilibili.com"),
};

static ARTICLE: Route = Route::new(
    "article",
    "{id}_{num}{suffix}.{extension}",
    &["{category}", "{username}"],
    "{id}_{num}{suffix}",
);
static USER_ARTICLES: Route = Route::new(
    "user-articles",
    "{filename}.{extension}",
    &["{category}"],
    "",
);
static USER_ARTICLES_FAVORITE: Route = Route::new(
    "user-articles-favorite",
    "{filename}.{extension}",
    &["{category}"],
    "",
);

const ROOT: &str = "https://www.bilibili.com";
const REFERER: &str = "https://www.bilibili.com/";
const API: &str = "https://api.bilibili.com/x/polymer/web-dynamic/v1";

#[derive(Debug, PartialEq)]
enum Target {
    Article(String),
    UserArticles(String),
    Favorites,
}

/// Leading ASCII digits of `text`.
fn leading_digits(text: &str) -> Option<&str> {
    let len = text.bytes().take_while(u8::is_ascii_digit).count();
    (len > 0).then(|| &text[..len])
}

fn target(url: &Url) -> Option<Target> {
    if !plain(url) {
        return None;
    }
    let path = url.path();
    match url.host_str()? {
        "t.bilibili.com" => {
            leading_digits(path.strip_prefix('/')?).map(|id| Target::Article(id.into()))
        }
        "bilibili.com" | "www.bilibili.com" => {
            leading_digits(path.strip_prefix("/opus/")?).map(|id| Target::Article(id.into()))
        }
        "space.bilibili.com" => {
            let rest = path.strip_prefix('/')?;
            let user = leading_digits(rest)?;
            let rest = rest[user.len()..].strip_prefix('/')?;
            if ["article", "upload/opus", "dynamic"]
                .iter()
                .any(|p| rest.starts_with(p))
            {
                return Some(Target::UserArticles(user.into()));
            }
            (rest == "favlist" && url.query().is_some_and(|q| q.starts_with("fid=opus")))
                .then_some(Target::Favorites)
        }
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Article(id)) => crate::sites::once(async move {
            let state = initial_state(http, &id).await?;
            let items = article_items(state, http.option_bool("livephoto", true))?;
            Ok(crate::sites::events(crate::sites::post(
                &SITE, http, &ARTICLE, items,
            )))
        }),
        Some(Target::UserArticles(user)) => {
            feed(http, &USER_ARTICLES, Page::Space { user, offset: None })
        }
        Some(Target::Favorites) => feed(http, &USER_ARTICLES_FAVORITE, Page::Fav(1)),
        None => crate::sites::fail(error("unsupported Bilibili URL")),
    }
}

/// Upstream `BilibiliAPI.article`: the page's `__INITIAL_STATE__` object.
/// Risk-control (captcha) pages and refused page loads fall back to the
/// web-dynamic `detail` API instead of upstream's five-minute retry loop.
async fn initial_state(http: &Http, id: &str) -> Result<Map<String, Value>> {
    let url = http.api(&format!("{ROOT}/opus/{id}"))?;
    let page = match http
        .text_with(&url, &http.browser_headers(&[("Referer", REFERER)]))
        .await
    {
        Ok(page) => page,
        Err(page_error) => return dynamic_state(http, id).await.map_err(|_| page_error),
    };
    let state = extr(&page, "window.__INITIAL_STATE__=", "};");
    if !state.is_empty()
        && let Ok(Value::Object(state)) = serde_json::from_str(&format!("{state}}}"))
    {
        return Ok(state);
    }
    let fallback = dynamic_state(http, id).await;
    if page.contains("window._riskdata_") {
        return fallback.map_err(|_| {
            error(format!(
                "{id}: Bilibili risk control blocked the article page"
            ))
        });
    }
    fallback.map_err(|_| {
        error(format!(
            "{id}: Unable to extract Bilibili INITIAL_STATE data"
        ))
    })
}

/// The web-dynamic `detail` API response shaped like the page state: the
/// author module plus the dynamic's pictures as a top album.
async fn dynamic_state(http: &Http, id: &str) -> Result<Map<String, Value>> {
    let data = call(http, "/detail", &[("id", id.to_owned())]).await?;
    let modules = data
        .pointer("/data/item/modules")
        .and_then(Value::as_object)
        .ok_or_else(|| error("Bilibili dynamic data has no modules"))?;
    let major = modules.get("module_dynamic").and_then(|m| m.get("major"));
    let pics: Vec<Value> =
        if let Some(pics) = major.and_then(|m| m.get("opus")?.get("pics")?.as_array()) {
            pics.clone()
        } else {
            major
                .and_then(|m| m.get("draw")?.get("items")?.as_array())
                .into_iter()
                .flatten()
                .filter_map(Value::as_object)
                .map(|item| {
                    let mut pic = item.clone();
                    if let Some(src) = pic.remove("src") {
                        pic.insert("url".into(), src);
                    }
                    Value::Object(pic)
                })
                .collect()
        };
    let mut list = vec![serde_json::json!({
        "module_top": {"display": {"album": {"pics": pics}}}
    })];
    if let Some(author) = modules.get("module_author") {
        list.push(serde_json::json!({ "module_author": author }));
    }
    let mut state = Map::new();
    state.insert("id".into(), id.into());
    state.insert("detail".into(), serde_json::json!({ "modules": list }));
    Ok(state)
}

/// Upstream `BilibiliArticleExtractor.items`.
fn article_items(mut article: Map<String, Value>, livephoto: bool) -> Result<Vec<Item>> {
    let list = article
        .get("detail")
        .and_then(|d| d.get("modules"))
        .and_then(Value::as_array)
        .ok_or_else(|| error("Bilibili article data has no modules"))?;
    // Flatten the modules list; upstream warns about blocked articles.
    let mut modules = Map::new();
    for module in list {
        if let Some(module) = module.as_object() {
            modules.extend(
                module
                    .iter()
                    .filter(|(key, _)| *key != "module_type")
                    .map(|(k, v)| (k.clone(), v.clone())),
            );
        }
    }

    let author = modules.get("module_author");
    let field = |key: &str| {
        author
            .and_then(|a| a.get(key))
            .cloned()
            .unwrap_or(Value::Null)
    };
    let username = field("name");
    let timestamp = field("pub_ts")
        .as_i64()
        .or_else(|| scalar(&field("pub_ts")).parse().ok());
    article.insert("username".into(), username.clone());
    article.insert("user_id".into(), field("mid"));

    let mut pics: Vec<Value> = Vec::new();
    if let Some(top) = modules
        .get("module_top")
        .and_then(|m| m.get("display")?.get("album")?.get("pics")?.as_array())
    {
        pics.extend(top.iter().cloned());
    }
    if let Some(paragraphs) = modules
        .get("module_content")
        .and_then(|m| m.get("paragraphs")?.as_array())
    {
        for paragraph in paragraphs {
            if let Some(list) = paragraph.get("pic").and_then(|p| p.get("pics")?.as_array()) {
                pics.extend(list.iter().cloned());
            }
        }
    }
    if let Some(detail) = article.get_mut("detail").and_then(Value::as_object_mut) {
        detail.insert("modules".into(), Value::Object(modules));
    }

    article.insert("count".into(), pics.len().into());
    let id = article.get("id").map(scalar).unwrap_or_default();
    let username = username.as_str().map(str::to_owned);
    let mut items = Vec::new();
    for (index, pic) in pics.iter().enumerate() {
        let num = index + 1;
        article.insert("num".into(), num.into());
        article.insert("suffix".into(), "".into());
        if let Some(pic) = pic.as_object() {
            article.extend(pic.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        let mut push = |raw: &str, suffix: &str| {
            let Some(url) = Url::parse(raw).ok().filter(plain) else {
                return;
            };
            let mut fields = article.clone();
            fields.insert("suffix".into(), suffix.into());
            nameext_into(raw, &mut fields);
            items.push(
                Item::new(url, fields)
                    .ids(id.clone(), format!("{num}{suffix}"))
                    .user(username.clone())
                    .timestamp(timestamp)
                    .header("Referer", REFERER),
            );
        };
        if let Some(raw) = pic.get("url").and_then(Value::as_str) {
            push(raw, "");
        }
        if livephoto
            && let Some(raw) = pic
                .get("live_url")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        {
            push(raw, "l");
        }
    }
    Ok(items)
}

enum Page {
    Space {
        user: String,
        offset: Option<String>,
    },
    Fav(u64),
}

/// Upstream `BilibiliAPI._call`.
async fn call(http: &Http, endpoint: &str, params: &[(&str, String)]) -> Result<Value> {
    let mut url = http.api(&format!("{API}{endpoint}"))?;
    url.query_pairs_mut().extend_pairs(params);
    let data = http
        .json_with(&url, &http.browser_headers(&[("Referer", REFERER)]))
        .await?;
    if truthy(data.get("code")) {
        return Err(error("Bilibili API request failed"));
    }
    Ok(data)
}

/// User articles (`opus/feed/space`) or favorites (`opus/feed/fav`), each
/// article queued as an article job.
fn feed<'a>(http: &'a Http, route: &'static Route, start: Page) -> Events<'a> {
    crate::sites::paginate(Some(start), move |state| async move {
        let Some(page) = state else {
            return Ok(None);
        };
        let (data, page) = match page {
            Page::Space { user, offset } => {
                let mut params = vec![("host_mid", user.clone())];
                if let Some(offset) = offset.clone() {
                    params.push(("offset", offset));
                }
                (
                    call(http, "/opus/feed/space", &params).await?,
                    Page::Space { user, offset },
                )
            }
            Page::Fav(number) => {
                let params = [("page", number.to_string()), ("page_size", "20".into())];
                let data = call(http, "/opus/feed/fav", &params).await.map_err(|e| {
                    let cookie = Url::parse(REFERER)
                        .ok()
                        .and_then(|u| http.cookie(&u, "SESSDATA"));
                    if cookie.is_none() {
                        error("Bilibili API request failed; the 'SESSDATA' cookie is required")
                    } else {
                        e
                    }
                })?;
                (data, Page::Fav(number))
            }
        };
        let data = data.get("data").cloned().unwrap_or(Value::Null);
        let items: Vec<Map<String, Value>> = data
            .get("items")
            .and_then(Value::as_array)
            .ok_or_else(|| error("Bilibili API response contains no items"))?
            .iter()
            .filter_map(|item| item.as_object().cloned())
            .collect();
        let has_more = truthy(data.get("has_more"));
        let next = match page {
            // An empty page would repeat the same offset forever.
            Page::Space { .. } if items.is_empty() => None,
            Page::Space { user, offset } => {
                let offset = items
                    .iter()
                    .rev()
                    .find_map(|item| item.get("opus_id").map(scalar))
                    .or(offset);
                Some(Page::Space { user, offset })
            }
            Page::Fav(number) => Some(Page::Fav(number + 1)),
        }
        .filter(|_| has_more);
        if next.is_some() {
            // Upstream `request_interval = (3.0, 6.0)`.
            #[cfg(not(test))]
            tokio::time::sleep(std::time::Duration::from_secs(4)).await;
        }
        let children: Vec<(Url, Map<String, Value>)> = items
            .into_iter()
            .filter_map(|item| {
                let id = item.get("opus_id").map(scalar)?;
                let url = Url::parse(&format!("{ROOT}/opus/{id}")).ok()?;
                Some((url, item))
            })
            .collect();
        let events = futures_util::StreamExt::boxed(futures_util::StreamExt::flatten(
            futures_util::stream::iter(children.into_iter().map(move |(child, data)| {
                crate::sites::queue(http, "bilibili", route.subcategory, child, &data)
            })),
        ));
        Ok(Some((events, next)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, json, keywords, media};
    use serde_json::json;

    fn t(raw: &str) -> Option<Target> {
        target(&Url::parse(raw).unwrap())
    }

    #[test]
    fn classifies_urls() {
        let article = |id: &str| Some(Target::Article(id.into()));
        assert_eq!(
            t("https://www.bilibili.com/opus/988425412565532689"),
            article("988425412565532689")
        );
        assert_eq!(t("http://bilibili.com/opus/12?x=1"), article("12"));
        assert_eq!(
            t("https://t.bilibili.com/9884254125"),
            article("9884254125")
        );
        assert_eq!(t("https://t.bilibili.com/12abc"), article("12"));
        let user = Some(Target::UserArticles("37737161".into()));
        assert_eq!(t("https://space.bilibili.com/37737161/article"), user);
        assert_eq!(t("https://space.bilibili.com/37737161/upload/opus"), user);
        assert_eq!(t("https://space.bilibili.com/37737161/dynamic?x"), user);
        assert_eq!(t("https://space.bilibili.com/37737161/articles"), user);
        assert_eq!(
            t("https://space.bilibili.com/405279/favlist?fid=opus"),
            Some(Target::Favorites)
        );
        for raw in [
            "https://www.bilibili.com/",
            "https://www.bilibili.com/opus/",
            "https://www.bilibili.com/video/BV1",
            "https://m.bilibili.com/opus/1",
            "https://t.bilibili.com/",
            "https://evil.test/opus/1",
            "https://www.bilibili.com:8443/opus/1",
            "https://u:p@t.bilibili.com/1",
            "ftp://t.bilibili.com/1",
            "https://space.bilibili.com/1",
            "https://space.bilibili.com/1/video",
            "https://space.bilibili.com/x/article",
            "https://space.bilibili.com/1/favlist",
            "https://space.bilibili.com/1/favlist?fid=2",
            "https://space.bilibili.com/1/favlist/x?fid=opus",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn article_page(id: &str) -> Vec<u8> {
        let state = json!({"id": id, "detail": {"modules": [
            {"module_type": "MODULE_TYPE_TOP", "module_top": {"display": {"album": {"pics": [
                {"url": "https://i0.hdslb.com/bfs/new_dyn/a.jpg", "width": 10, "height": 20,
                 "live_url": "https://i0.hdslb.com/bfs/live/a.mov"}]}}}},
            {"module_type": "MODULE_TYPE_AUTHOR", "module_author": {"name": "Author", "mid": 7,
                "pub_ts": 1700000000}},
            {"module_type": "MODULE_TYPE_CONTENT", "module_content": {"paragraphs": [
                {"text": {}},
                {"pic": {"pics": [{"url": "https://i0.hdslb.com/bfs/new_dyn/b.png", "width": 3}]}},
                {"pic": {}}]}}]}});
        html(&format!(
            "<script>window.__INITIAL_STATE__={state};(function(){{}})</script>"
        ))
    }

    #[tokio::test]
    async fn extracts_article_with_live_photos() {
        let (root, server) = crate::sites::testing::serve(vec![article_page("123")]);
        let http = http("bilibili", root);
        let files = media(extract(
            &http,
            Url::parse("https://t.bilibili.com/123").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 3);
        assert_eq!(files[1].url.as_str(), "https://i0.hdslb.com/bfs/live/a.mov");
        let k = keywords(&files[0]);
        assert_eq!(k["username"], "Author");
        assert_eq!(k["user_id"], 7);
        assert_eq!(k["count"], 2);
        assert_eq!(k["num"], 1);
        assert_eq!(k["suffix"], "");
        assert_eq!(k["width"], 10);
        assert!(k["detail"]["modules"]["module_author"].is_object());
        assert!(k["detail"]["modules"].get("module_type").is_none());
        let k = keywords(&files[2]);
        assert_eq!((k["num"].clone(), k["width"].clone()), (json!(2), json!(3)));
        // Keys of earlier pictures persist, as with upstream `dict.update`.
        assert_eq!(k["height"], 20);
        let names: Vec<String> = files
            .iter()
            .map(|f| {
                crate::sites::default_templates(f)
                    .unwrap()
                    .0
                    .render(f)
                    .unwrap()
            })
            .collect();
        assert_eq!(names, ["123_1.jpg", "123_1l.mov", "123_2.png"]);
        let (_, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("bilibili/Author")
        );
        let (_, identity) = crate::sites::archive_key(&files[1]).unwrap();
        assert_eq!(identity, r#"["{id}_{num}{suffix}",["123","1","l"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /opus/123 "), "{}", requests[0]);
    }

    #[tokio::test]
    async fn livephoto_option_and_risk_fallback() {
        let detail = json!({"code": 0, "data": {"item": {"modules": {
            "module_author": {"name": "Author", "mid": 7, "pub_ts": 1700000000},
            "module_dynamic": {"major": {"draw": {"items": [
                {"src": "https://i0.hdslb.com/bfs/new_dyn/a.jpg", "width": 1},
                {"src": "https://i0.hdslb.com/bfs/new_dyn/b.png", "width": 2}]}}}}}}});
        let (root, server) = crate::sites::testing::serve(vec![
            html("<script>window._riskdata_={}</script>"),
            json(detail),
        ]);
        let options = json!({"livephoto": false})
            .as_object()
            .cloned()
            .map(std::sync::Arc::new);
        let http = http("bilibili", root).with_options(options);
        let files = media(extract(
            &http,
            Url::parse("https://www.bilibili.com/opus/5").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[1]);
        assert_eq!(
            (k["username"].clone(), k["num"].clone()),
            (json!("Author"), json!(2))
        );
        assert_eq!(
            files[1].url.as_str(),
            "https://i0.hdslb.com/bfs/new_dyn/b.png"
        );
        let requests = server.join().unwrap();
        assert!(requests[1].contains("/detail?id=5 "), "{}", requests[1]);
    }

    #[tokio::test]
    async fn reports_missing_state() {
        let (root, _server) = crate::sites::testing::serve(vec![html("<html>nothing</html>")]);
        let http = http("bilibili", root);
        let events: Vec<_> = futures_util::StreamExt::collect(extract(
            &http,
            Url::parse("https://www.bilibili.com/opus/9").unwrap(),
        ))
        .await;
        let message = events[0].as_ref().err().unwrap().to_string();
        assert!(message.contains("9: Unable to extract"), "{message}");
        assert!(!message.contains("bilibili.com"));
    }

    #[tokio::test]
    async fn paginates_user_articles_into_article_jobs() {
        let (root, server) = crate::sites::testing::serve(vec![
            json(
                json!({"code": 0, "data": {"has_more": true, "items": [{"opus_id": "123", "content": "x"}]}}),
            ),
            article_page("123"),
            json(json!({"code": 0, "data": {"has_more": false, "items": [{"opus_id": "5"}]}})),
            article_page("5"),
        ]);
        let http = http("bilibili", root);
        let files = media(extract(
            &http,
            Url::parse("https://space.bilibili.com/37737161/article").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 6);
        assert_eq!(keywords(&files[0])["subcategory"], "article");
        assert_eq!(keywords(&files[5])["id"], "5");
        let requests = server.join().unwrap();
        assert!(
            requests[0]
                .starts_with("GET /x/polymer/web-dynamic/v1/opus/feed/space?host_mid=37737161 ")
        );
        assert!(requests[1].starts_with("GET /opus/123 "));
        assert!(requests[2].starts_with(
            "GET /x/polymer/web-dynamic/v1/opus/feed/space?host_mid=37737161&offset=123 "
        ));
    }

    #[tokio::test]
    async fn paginates_favorites_and_reports_api_errors() {
        let (root, server) = crate::sites::testing::serve(vec![
            json(json!({"code": 0, "data": {"has_more": true, "items": []}})),
            json(json!({"code": -101, "message": "not logged in"})),
        ]);
        let http = http("bilibili", root);
        let url = Url::parse("https://space.bilibili.com/1/favlist?fid=opus").unwrap();
        let events: Vec<_> = futures_util::StreamExt::collect(extract(&http, url)).await;
        let message = events.last().unwrap().as_ref().err().unwrap().to_string();
        assert!(message.contains("SESSDATA"), "{message}");
        let requests = server.join().unwrap();
        assert!(
            requests[0]
                .starts_with("GET /x/polymer/web-dynamic/v1/opus/feed/fav?page=1&page_size=20 ")
        );
        assert!(requests[1].contains("?page=2&page_size=20 "));
    }

    #[tokio::test]
    async fn favorites_page_forward_with_cookie() {
        let (root, server) = crate::sites::testing::serve(vec![
            json(json!({"code": 0, "data": {"has_more": true, "items": [{"opus_id": 8}]}})),
            article_page("8"),
            json(json!({"code": 0, "data": {"items": []}})),
        ]);
        let http = http("bilibili", root);
        http.set_cookie(
            &Url::parse(REFERER).unwrap(),
            "SESSDATA=secret; Domain=bilibili.com; Path=/",
        );
        let url = Url::parse("https://space.bilibili.com/1/favlist?fid=opus").unwrap();
        let files = media(extract(&http, url)).await;
        assert_eq!(files.len(), 3);
        assert_eq!(keywords(&files[0])["subcategory"], "article");
        let requests = server.join().unwrap();
        assert!(requests[2].contains("/opus/feed/fav?page=2&page_size=20 "));
    }
}
