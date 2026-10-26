//! Pillowfort posts and users, mirroring gallery_dl/extractor/pillowfort.py.
use crate::error;
use crate::sites::gdl::{
    auth_info, captures, date, extr, group, nameext_into, parse_iso, partition, re, str_of, truthy,
};
use crate::sites::{Events, Http, Item, Route, Site};
use futures_util::StreamExt;
use serde_json::Value;
use url::Url;

const ROOT: &str = "https://www.pillowfort.social";
const BASE: &str = r"^https?://www\.pillowfort\.social";
const FILENAME: &str = "{post_id} {title|original_post[title]:?/ /}{num:>02}.{extension}";

static POST: Route = Route::new("post", FILENAME, &["{category}", "{username}"], "{id}");
static USER: Route = Route::new("user", FILENAME, &["{category}", "{username}"], "{id}");

pub static SITE: Site = Site {
    category: "pillowfort",
    name: "Pillowfort",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&POST, &USER],
    api_key: false,
    cookies: Some("www.pillowfort.social"),
};

#[derive(Debug, PartialEq)]
enum Target {
    Post(String),
    User(String),
}

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(re!(&format!(r"{BASE}/posts/(\d+)")), url) {
        return Some(Target::Post(group(&c, 1).into()));
    }
    let c = captures(re!(&format!(r"{BASE}/([^/?#]+(?:/tagged/[^/?#]+)?)")), url)?;
    // Upstream's `(?!posts/)`.
    (!url.path().starts_with("/posts/")).then(|| Target::User(group(&c, 1).into()))
}

/// `login`: existing session cookies, else the sign-in form.
async fn login(http: &Http) -> crate::Result<()> {
    let base = http.api(ROOT)?;
    if http.cookie(&base, "_Pf_new_session").is_some()
        || http.cookie(&base, "remember_user_token").is_some()
    {
        return Ok(());
    }
    let Some((username, password)) = auth_info(http) else {
        return Ok(());
    };
    let url = http.api(&format!("{ROOT}/users/sign_in"))?;
    let page = http.text(&url).await?;
    let auth = extr(&page, "name=\"authenticity_token\" value=\"", "\"");
    let form = [
        ("utf8", "✓"),
        ("authenticity_token", auth),
        ("user[email]", username.as_str()),
        ("user[password]", password.as_str()),
        ("user[remember_me]", "1"),
    ];
    match http
        .post_form_text(&url, &form, &[("Origin", ROOT), ("Referer", url.as_str())])
        .await
    {
        Err(e) if e.to_string().contains("redirected a POST") => Ok(()),
        Err(e) => Err(e),
        Ok(_) => Err(error(
            "Pillowfort login failed; check the username and password",
        )),
    }
}

/// `PillowfortExtractor.items` for one post: files, and embeds when
/// `external` is set.
fn post_events<'a>(http: &'a Http, route: &'static Route, post: &Value) -> Events<'a> {
    let empty = || futures_util::stream::empty().boxed();
    let Some(mut post) = post.as_object().cloned() else {
        return empty();
    };
    if post.contains_key("original_post") && !truthy(http.option("reblogs")) {
        return empty();
    }
    let inline = http.option("inline").is_none_or(|v| truthy(Some(v)));
    let external = truthy(http.option("external"));
    let mut files = post
        .remove("media")
        .and_then(|m| m.as_array().cloned())
        .unwrap_or_default();
    if inline {
        let content = post
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default();
        for c in
            re!(r#"src="(https://img\d+\.pillowfort\.social/posts/[^"]+)"#).captures_iter(content)
        {
            files.push(serde_json::json!({"url": &c[1]}));
        }
    }
    let created = date(parse_iso(
        post.get("created_at")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    ));
    post.insert("date".into(), created);
    let post_id = post.remove("id").unwrap_or(Value::Null);
    post.insert("post_id".into(), post_id.clone());
    post.insert("count".into(), files.len().into());
    post.insert("num".into(), 0.into());
    let post_id = crate::sites::scalar(&post_id);
    let mut parts: Vec<Events<'a>> = Vec::new();
    let mut num = 0;
    for file in files {
        let url = match str_of(&file, "url") {
            "" => str_of(&file, "b2_lg_url"),
            url => url,
        }
        .to_owned();
        if url.is_empty() {
            continue;
        }
        let embed = truthy(file.get("embed_code"));
        if embed && !external {
            continue;
        }
        if !embed {
            num += 1;
            post.insert("num".into(), num.into());
        }
        if let Some(file) = file.as_object() {
            post.extend(file.clone());
        }
        nameext_into(&url, &mut post);
        let name = crate::sites::scalar(&post["filename"]);
        let (hash, _, filename) = partition(&name, "_");
        post.insert("hash".into(), hash.into());
        post.insert("filename".into(), filename.into());
        if file.get("id").is_none() {
            post.insert("id".into(), hash.into());
        }
        if let Some(created) = file.get("created_at").and_then(Value::as_str) {
            post.insert("date".into(), date(parse_iso(created)));
        }
        let Ok(parsed) = Url::parse(&url) else {
            continue;
        };
        if embed {
            parts.push(crate::sites::galleries::queue(
                http,
                &SITE,
                route,
                vec![(parsed, post.clone())],
            ));
        } else {
            let id = crate::sites::scalar(&post["id"]);
            parts.push(crate::sites::events(crate::sites::post(
                &SITE,
                http,
                route,
                vec![Item::new(parsed, post.clone()).ids(post_id.clone(), id)],
            )));
        }
    }
    futures_util::stream::iter(parts).flatten().boxed()
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Pillowfort URL"));
    };
    match target {
        Target::Post(id) => crate::sites::once(async move {
            login(http).await?;
            let post = http
                .json(&http.api(&format!("{ROOT}/posts/{id}/json/"))?)
                .await?;
            Ok(post_events(http, &POST, &post))
        }),
        Target::User(path) => crate::sites::paginate(Some(1u64), move |state| {
            let path = path.clone();
            async move {
                let Some(page) = state else { return Ok(None) };
                if page == 1 {
                    login(http).await?;
                }
                let mut url = http.api(&format!("{ROOT}/{path}/json/"))?;
                url.query_pairs_mut().append_pair("p", &page.to_string());
                let posts = http.json(&url).await?["posts"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                let next = (posts.len() >= 20).then_some(page + 1);
                let events = posts
                    .iter()
                    .map(|post| post_events(http, &USER, post))
                    .collect::<Vec<_>>();
                Ok(Some((
                    futures_util::stream::iter(events).flatten().boxed(),
                    next,
                )))
            }
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http as client, json, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.pillowfort.social/posts/1557500"),
            Some(Target::Post("1557500".into()))
        );
        assert_eq!(
            t("https://www.pillowfort.social/Pome/tagged/art"),
            Some(Target::User("Pome/tagged/art".into()))
        );
    }

    #[tokio::test]
    async fn post_media_and_inline_images() {
        let post = json!({"id": 9, "title": "Hello", "username": "pome", "created_at": "2020-01-02T03:04:05Z",
            "content": "<img src=\"https://img2.pillowfort.social/posts/abc_inline.png\">",
            "media": [{"id": 5, "url": "https://img3.pillowfort.social/posts/def_photo.jpg", "created_at": "2020-01-03T00:00:00Z"},
                      {"url": "https://youtube.com/watch?v=x", "embed_code": "<iframe>"}]});
        let (root, server) = serve(vec![json(post)]);
        let http = client("pillowfort", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.pillowfort.social/posts/9").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[1]);
        assert_eq!(
            (
                k["hash"].as_str(),
                k["filename"].as_str(),
                k["id"].as_str(),
                k["count"].as_i64()
            ),
            (Some("abc"), Some("inline"), Some("abc"), Some(3))
        );
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "9 Hello 01.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("pillowfort/pome")
        );
        server.join().unwrap();
    }
}
