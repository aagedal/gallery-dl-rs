//! Fantia posts, fanclubs and supported plans, mirroring
//! gallery_dl/extractor/fantia.py. Paid content needs a `_session_id`.
use crate::error;
use crate::sites::gdl::{captures, date, extr, extract_iter, group, nameext_into, re, strptime};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value, json};
use std::sync::Mutex;
use url::Url;

const ROOT: &str = "https://fantia.jp";
const FILENAME: &str = "{post_id}_{file_id}.{extension}";

static CREATOR: Route = Route::new(
    "creator",
    FILENAME,
    &["{category}", "{fanclub_id}"],
    "{post_id}_{file_id}",
);
static POST: Route = Route::new(
    "post",
    FILENAME,
    &["{category}", "{fanclub_id}"],
    "{post_id}_{file_id}",
);
static SUPPORTING: Route = Route::new(
    "supporting",
    FILENAME,
    &["{category}", "{fanclub_id}"],
    "{post_id}_{file_id}",
);

pub static SITE: Site = Site {
    category: "fantia",
    name: "Fantia",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CREATOR, &POST, &SUPPORTING],
    api_key: false,
    cookies: Some("fantia.jp"),
};

#[derive(Debug, PartialEq)]
enum Target {
    Creator(String),
    Post(String),
    Supporting(String),
}

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(re!(r"^https?://(?:www\.)?fantia\.jp/fanclubs/(\d+)"), url) {
        return Some(Target::Creator(group(&c, 1).into()));
    }
    if let Some(c) = captures(re!(r"^https?://(?:www\.)?fantia\.jp/posts/(\d+)"), url) {
        return Some(Target::Post(group(&c, 1).into()));
    }
    captures(
        re!(r"^https?://(?:www\.)?fantia\.jp/mypage/users/plans(?:\?type=((?:not_)?free))?"),
        url,
    )
    .map(|c| Target::Supporting(group(&c, 1).into()))
}

/// The session's CSRF token (upstream `self.headers["X-CSRF-Token"]`).
static CSRF: Mutex<String> = Mutex::new(String::new());

fn remember_csrf(page: &str) {
    *CSRF.lock().unwrap_or_else(|e| e.into_inner()) =
        extr(page, "name=\"csrf-token\" content=\"", "\"").to_owned();
}

fn empty_plan() -> Value {
    json!({"id": 0, "price": 0, "limit": 0, "name": "", "description": "", "thumb": format!("{ROOT}/images/fallback/plan/thumb_default.png")})
}

/// `_get_post_data` and the per-content file loop of `items`.
async fn post_items(http: &Http, post_id: &str) -> crate::Result<Vec<Item>> {
    let csrf = CSRF.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let headers = [
        ("Accept", "application/json, text/plain, */*"),
        ("X-Requested-With", "XMLHttpRequest"),
        ("X-CSRF-Token", csrf.as_str()),
    ];
    let resp = http
        .json_with(
            &http.api(&format!("{ROOT}/api/v1/posts/{post_id}"))?,
            &headers,
        )
        .await?["post"]
        .clone();
    let fanclub = &resp["fanclub"];
    let mut post = Map::new();
    post.insert("post_id".into(), resp["id"].clone());
    post.insert(
        "post_url".into(),
        format!("{ROOT}/posts/{}", crate::sites::scalar(&resp["id"])).into(),
    );
    post.insert("post_title".into(), resp["title"].clone());
    post.insert("comment".into(), resp["comment"].clone());
    post.insert("rating".into(), resp["rating"].clone());
    post.insert("posted_at".into(), resp["posted_at"].clone());
    post.insert(
        "date".into(),
        date(strptime(
            resp["posted_at"].as_str().unwrap_or_default(),
            "%a, %d %b %Y %H:%M:%S %z",
        )),
    );
    post.insert("fanclub_id".into(), fanclub["id"].clone());
    post.insert("fanclub_user_id".into(), fanclub["user"]["id"].clone());
    post.insert("fanclub_user_name".into(), fanclub["user"]["name"].clone());
    post.insert("fanclub_name".into(), fanclub["name"].clone());
    post.insert(
        "fanclub_url".into(),
        format!("{ROOT}/fanclubs/{}", crate::sites::scalar(&fanclub["id"])).into(),
    );
    let tags: Vec<Value> = resp["tags"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|t| t["name"].clone())
        .collect();
    post.insert("tags".into(), tags.into());
    let mut contents = resp["post_contents"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if let Some(thumb) = resp["thumb"]["original"].as_str() {
        contents.insert(0, json!({"id": "thumb", "title": "thumb", "category": "thumb", "download_uri": thumb, "visible_status": "visible", "plan": null}));
    }
    post.insert("num".into(), 0.into());
    post.insert("content_count".into(), contents.len().into());
    let mut num = 0;
    let mut items = Vec::new();
    for (index, content) in contents.iter().enumerate() {
        post.insert("content_category".into(), content["category"].clone());
        post.insert("content_title".into(), content["title"].clone());
        let filename = content
            .get("filename")
            .and_then(Value::as_str)
            .filter(|f| !f.is_empty())
            .unwrap_or_default()
            .to_owned();
        post.insert("content_filename".into(), filename.as_str().into());
        post.insert("content_id".into(), content["id"].clone());
        post.insert(
            "content_comment".into(),
            content
                .get("comment")
                .filter(|c| !c.is_null())
                .cloned()
                .unwrap_or_else(|| "".into()),
        );
        post.insert("content_num".into(), (index + 1).into());
        post.insert(
            "plan".into(),
            Some(&content["plan"])
                .filter(|p| !p.is_null())
                .cloned()
                .unwrap_or_else(empty_plan),
        );
        let mut files: Vec<(Value, String)> = Vec::new();
        for photo in content["post_content_photos"]
            .as_array()
            .into_iter()
            .flatten()
        {
            files.push((
                photo["id"].clone(),
                crate::sites::scalar(&photo["url"]["original"]),
            ));
        }
        if let Some(uri) = content.get("download_uri").and_then(Value::as_str) {
            let url = if uri.starts_with('/') {
                format!("{ROOT}{uri}")
            } else {
                uri.to_owned()
            };
            files.push((content["id"].clone(), url));
        }
        let mut blog = String::new();
        if content["category"] == "blog"
            && let Some(comment) = content.get("comment").and_then(Value::as_str)
            && let Ok(ops) = serde_json::from_str::<Value>(comment)
        {
            for op in ops["ops"].as_array().into_iter().flatten() {
                match &op["insert"] {
                    Value::String(text) => blog.push_str(text),
                    Value::Object(insert) if insert.contains_key("fantiaImage") => {
                        let image = &insert["fantiaImage"];
                        files.push((
                            image["id"].clone(),
                            format!("{ROOT}{}", crate::sites::scalar(&image["original_url"])),
                        ));
                    }
                    _ => {}
                }
            }
        }
        post.insert("blogpost_text".into(), blog.into());
        for (file_id, url) in files {
            num += 1;
            post.insert("file_id".into(), file_id.clone());
            post.insert("file_url".into(), url.as_str().into());
            post.insert("num".into(), num.into());
            nameext_into(
                if filename.is_empty() { &url } else { &filename },
                &mut post,
            );
            let Ok(file) = Url::parse(&url) else { continue };
            // Fantia-hosted downloads need the logged-in session.
            let cookie = (file.host_str() == Some("fantia.jp"))
                .then(|| http.cookie_header(&file))
                .flatten();
            let mut item =
                Item::new(file, post.clone()).ids(post_id, crate::sites::scalar(&file_id));
            if let Some(cookie) = cookie {
                item = item.header("Cookie", &cookie);
            }
            items.push(item);
        }
    }
    Ok(items)
}

/// One `_pagination` page: ids after `needle`, keeping the CSRF token.
async fn ids_page(
    http: &Http,
    url: &str,
    params: &[(&str, String)],
    needle: &str,
    page: i64,
) -> crate::Result<Vec<String>> {
    let mut request = http.api(url)?;
    request
        .query_pairs_mut()
        .extend_pairs(params.iter().map(|(k, v)| (*k, v.as_str())))
        .append_pair("page", &page.to_string());
    let html = http.text(&request).await?;
    remember_csrf(&html);
    Ok(extract_iter(&html, needle, "\"")
        .into_iter()
        .map(str::to_owned)
        .collect())
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Fantia URL"));
    };
    match target {
        Target::Post(id) => crate::sites::once(async move {
            let page = http.text(&http.api(&format!("{ROOT}/"))?).await?;
            remember_csrf(&page);
            Ok(crate::sites::events(crate::sites::post(
                &SITE,
                http,
                &POST,
                post_items(http, &id).await?,
            )))
        }),
        Target::Creator(id) => crate::sites::paginate(Some(1i64), move |state| {
            let url = format!("{ROOT}/fanclubs/{id}/posts");
            async move {
                let Some(page) = state else { return Ok(None) };
                let ids =
                    ids_page(http, &url, &[], "class=\"link-block\" href=\"/posts/", page).await?;
                if ids.is_empty() {
                    return Ok(None);
                }
                let events = crate::sites::each(ids, move |post_id| async move {
                    Ok(crate::sites::post(
                        &SITE,
                        http,
                        &CREATOR,
                        post_items(http, &post_id).await?,
                    ))
                });
                Ok(Some((events, Some(page + 1))))
            }
        }),
        Target::Supporting(kind) => {
            let kinds: Vec<String> = if kind.is_empty() {
                vec!["not_free".into(), "free".into()]
            } else {
                vec![kind]
            };
            crate::sites::paginate(Some((0usize, 1i64)), move |state| {
                let kinds = kinds.clone();
                async move {
                    let Some((index, page)) = state else {
                        return Ok(None);
                    };
                    let Some(kind) = kinds.get(index) else {
                        return Ok(None);
                    };
                    let ids = ids_page(
                        http,
                        &format!("{ROOT}/mypage/users/plans"),
                        &[("type", kind.clone())],
                        "class=\"user-avatar\" href=\"/fanclubs/",
                        page,
                    )
                    .await?;
                    let next = if ids.is_empty() {
                        Some((index + 1, 1))
                    } else {
                        Some((index, page + 1))
                    };
                    let children = ids
                        .into_iter()
                        .filter_map(|id| {
                            Some((
                                Url::parse(&format!("{ROOT}/fanclubs/{id}")).ok()?,
                                Map::new(),
                            ))
                        })
                        .collect();
                    Ok(Some((
                        crate::sites::galleries::queue(http, &SITE, &SUPPORTING, children),
                        next,
                    )))
                }
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http as client, json as body, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://fantia.jp/fanclubs/6939"),
            Some(Target::Creator("6939".into()))
        );
        assert_eq!(
            t("https://fantia.jp/posts/508363"),
            Some(Target::Post("508363".into()))
        );
        assert_eq!(
            t("https://fantia.jp/mypage/users/plans?type=free"),
            Some(Target::Supporting("free".into()))
        );
    }

    #[tokio::test]
    async fn post_contents_and_blog_images() {
        let post = json!({"post": {"id": 508363, "title": "Post", "comment": "c", "rating": "general", "posted_at": "Sun, 06 Mar 2022 12:00:00 +0900",
            "fanclub": {"id": 6939, "name": "Club", "user": {"id": 1, "name": "Artist"}}, "tags": [{"name": "art"}],
            "thumb": {"original": "https://cc.fantia.jp/uploads/post/file/508363/thumb.jpg"},
            "post_contents": [
                {"id": 1, "title": "Photos", "category": "photo_gallery", "visible_status": "visible", "plan": null,
                 "post_content_photos": [{"id": 11, "url": {"original": "https://cc.fantia.jp/uploads/post_content_photo/file/11/a.png"}}]},
                {"id": 2, "title": "Blog", "category": "blog", "visible_status": "visible", "plan": {"id": 5, "price": 500},
                 "comment": "{\"ops\":[{\"insert\":\"Hello \"},{\"insert\":{\"fantiaImage\":{\"id\":22,\"original_url\":\"/posts/508363/album_image?query=x\"}}}]}"}]}});
        let (root, server) = serve(vec![
            html(r#"<meta name="csrf-token" content="TOKEN">"#),
            body(post),
        ]);
        let http = client("fantia", root);
        let files = media(extract(
            &http,
            Url::parse("https://fantia.jp/posts/508363").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 3);
        assert_eq!(
            files[2].url.as_str(),
            "https://fantia.jp/posts/508363/album_image?query=x"
        );
        let k = keywords(&files[2]);
        assert_eq!(
            (
                k["blogpost_text"].as_str(),
                k["content_num"].as_i64(),
                k["plan"]["price"].as_i64()
            ),
            (Some("Hello "), Some(3), Some(500))
        );
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "508363_11.png");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("fantia/6939")
        );
        let requests = server.join().unwrap();
        assert!(
            requests[1].contains("X-CSRF-Token: TOKEN")
                || requests[1].contains("x-csrf-token: TOKEN")
        );
    }
}
