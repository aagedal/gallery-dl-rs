//! SubscribeStar users and posts (subscribestar.com and .adult),
//! mirroring gallery_dl/extractor/subscribestar.py.
use crate::error;
use crate::sites::gdl::{
    Scan, auth_info, captures, date, extr, extract_iter, group, nameext_from_name, nameext_into,
    parse_int, parse_query, re, rextr, rpartition, strptime, unescape,
};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const FILENAME: &str = "{post_id}_{id}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{author_name}"];

static USER: Route = Route::new("user", FILENAME, DIRECTORY, "{id}");
static POST: Route = Route::new("post", FILENAME, DIRECTORY, "{id}");
static USER_ADULT: Route = Route::new("user-adult", FILENAME, DIRECTORY, "{id}");
static POST_ADULT: Route = Route::new("post-adult", FILENAME, DIRECTORY, "{id}");

pub static SITE: Site = Site {
    category: "subscribestar",
    name: "SubscribeStar",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&USER, &POST, &USER_ADULT, &POST_ADULT],
    api_key: false,
    cookies: Some("subscribestar.com"),
};

#[derive(Debug, PartialEq)]
enum Target {
    User(String, String),
    Post(String),
}

/// Whether the URL is on `.adult`, and its target.
fn target(url: &Url) -> Option<(bool, Target)> {
    const BASE: &str = r"^https?://(?:www\.)?subscribestar\.(com|adult)";
    if let Some(c) = captures(re!(&format!(r"{BASE}/posts/(\d+)")), url) {
        return Some((&c[1] == "adult", Target::Post(group(&c, 2).into())));
    }
    let c = captures(re!(&format!(r"{BASE}/([^/?#]+)(?:\?([^#]+))?")), url)?;
    // Upstream's `(?!posts/)`.
    (!url.path().starts_with("/posts/")).then(|| {
        (
            &c[1] == "adult",
            Target::User(group(&c, 2).into(), group(&c, 3).into()),
        )
    })
}

/// `SubscribestarExtractor.request`: verification redirects fail; HTML
/// redirect messages are followed.
async fn request(http: &Http, url: &str) -> crate::Result<String> {
    let mut url = http.api(url)?;
    for _ in 0..5 {
        let (page, final_url) = http.text_final(&url, &[]).await?;
        if final_url != url
            && (final_url.as_str().contains("/verify_subscriber")
                || final_url.as_str().contains("/age_confirmation_warning"))
        {
            return Err(error(format!("HTTP redirect to {final_url}")));
        }
        if page.len() < 250 && page.contains(">redirected<") {
            url = http.api(&unescape(extr(&page, "href=\"", "\"")))?;
            continue;
        }
        return Ok(page);
    }
    Err(error("SubscribeStar redirected too often"))
}

/// `login`: `_personalization_id`, else the two-step login.
async fn login(http: &Http, root: &str, adult: bool) -> crate::Result<()> {
    let base = http.api(root)?;
    if http.cookie(&base, "_personalization_id").is_some() {
        return Ok(());
    }
    let Some((username, password)) = auth_info(http) else {
        return Ok(());
    };
    if adult {
        http.set_cookie(&base, "18_plus_agreement_generic=true; Path=/");
    }
    let page = request(http, &format!("{root}/login")).await?;
    let csrf = unescape(extr(&page, "<meta name=\"csrf-token\" content=\"", "\""));
    let referer = format!("{root}/login");
    let headers = [
        (
            "Accept",
            "*/*;q=0.5, text/javascript, application/javascript, application/ecmascript, application/x-ecmascript",
        ),
        ("Referer", referer.as_str()),
        ("X-CSRF-Token", csrf.as_str()),
        ("X-Requested-With", "XMLHttpRequest"),
    ];
    for (endpoint, field, value) in [
        ("session.json", "email", username.as_str()),
        ("session/password.json", "password", password.as_str()),
    ] {
        let data = http
            .post_form(
                &http.api(&format!("{root}/{endpoint}"))?,
                &[(field, value)],
                &headers,
            )
            .await?;
        if let Some(errors) = data
            .get("errors")
            .and_then(Value::as_object)
            .filter(|e| !e.is_empty())
        {
            let message = errors
                .values()
                .next_back()
                .map(crate::sites::scalar)
                .unwrap_or_default();
            return Err(error(format!("SubscribeStar login failed: {message}")));
        }
    }
    Ok(())
}

/// `_parse_datetime`: short or long month names.
fn parse_date(text: &str) -> Value {
    let text = text.strip_prefix("Updated on ").unwrap_or(text);
    date(strptime(text, "%b %d, %Y %I:%M %p").or_else(|| strptime(text, "%B %d, %Y %I:%M %p")))
}

/// `_media_from_post`: gallery images, attachments and audio.
fn media(html: &str) -> Vec<Map<String, Value>> {
    let mut out = Vec::new();
    let gallery = extr(html, "data-gallery=\"", "\"");
    if !gallery.is_empty()
        && let Ok(Value::Array(items)) = serde_json::from_str::<Value>(&unescape(gallery))
    {
        out.extend(
            items
                .into_iter()
                .filter(|i| !i["url"].as_str().unwrap_or_default().contains("/previews"))
                .filter_map(|i| i.as_object().cloned()),
        );
    }
    let mut add = |section: &str, split: &regex::Regex, name: &str, src: &str, kind: &str| {
        for part in split.split(section).skip(1) {
            let mut item = Map::new();
            item.insert(
                "id".into(),
                parse_int(extr(part, "data-upload-id=\"", "\"")).into(),
            );
            item.insert("name".into(), unescape(extr(part, name, "<")).into());
            item.insert("url".into(), unescape(extr(part, src, "\"")).into());
            item.insert("type".into(), kind.into());
            out.push(item);
        }
    };
    let docs = extr(html, "class=\"uploads-docs\"", "class=\"post-edit_form\"");
    if !docs.is_empty() {
        add(
            docs,
            re!(r#"class="doc_preview[" ]"#),
            "doc_preview-title\">",
            "href=\"",
            "attachment",
        );
    }
    let audios = extr(html, "class=\"uploads-audios\"", "class=\"post-edit_form\"");
    if !audios.is_empty() {
        add(
            audios,
            re!(r#"class="audio_preview-data[" ]"#),
            "audio_preview-title\">",
            "src=\"",
            "audio",
        );
    }
    out
}

/// `_data_from_post` for listing posts or single post pages.
fn post_data(html: &str, single: bool) -> Map<String, Value> {
    let mut scan = Scan::new(html);
    let mut data = Map::new();
    data.insert(
        "post_id".into(),
        parse_int(scan.extr("data-id=\"", "\"")).into(),
    );
    let tags = |scan: &mut Scan| -> Value {
        extract_iter(
            scan.extr(
                "<div class=\"post_tags for-post\">",
                "<div class=\"post-actions\">",
            ),
            "?tag=",
            "\"",
        )
        .into_iter()
        .map(Value::from)
        .collect::<Vec<_>>()
        .into()
    };
    let content = |scan: &mut Scan| -> Value {
        scan.extr(
            "<div class=\"post-content\" data-role=\"post_content-text\">",
            "</div><div class=\"post-uploads for-youtube\"",
        )
        .trim()
        .into()
    };
    if single {
        data.insert(
            "date".into(),
            parse_date(scan.extr("<div class=\"section-title_date\">", "<")),
        );
        data.insert("content".into(), content(&mut scan));
        data.insert("tags".into(), tags(&mut scan));
        data.insert(
            "author_name".into(),
            unescape(scan.extr("class=\"star_link\" href=\"/", "\"")).into(),
        );
        data.insert(
            "author_id".into(),
            parse_int(scan.extr("data-user-id=\"", "\"")).into(),
        );
        data.insert(
            "author_nick".into(),
            unescape(scan.extr("alt=\"", "\"")).into(),
        );
    } else {
        data.insert(
            "author_id".into(),
            parse_int(scan.extr("data-user-id=\"", "\"")).into(),
        );
        data.insert(
            "author_name".into(),
            unescape(scan.extr("href=\"/", "\"")).into(),
        );
        data.insert("author_nick".into(), unescape(scan.extr(">", "<")).into());
        data.insert(
            "date".into(),
            parse_date(rpartition(scan.extr("class=\"post-date\">", "</"), ">").2),
        );
        data.insert("content".into(), content(&mut scan));
        data.insert("tags".into(), tags(&mut scan));
    }
    data
}

/// `SubscribestarExtractor.items` for one post.
fn post_items(root: &str, html: &str, single: bool, extra: &Map<String, Value>) -> Vec<Item> {
    let files = media(html);
    let mut data = post_data(html, single);
    let mut content = crate::sites::scalar(&data["content"]);
    if content.contains("<html><body>") {
        content = extr(&content, "<body>", "</body>").to_owned();
        data.insert("content".into(), content.as_str().into());
    }
    data.insert(
        "title".into(),
        unescape(rextr(&content, "<h1>", "</h1>", None)).into(),
    );
    data.extend(extra.clone());
    let post_id = crate::sites::scalar(&data["post_id"]);
    files
        .into_iter()
        .enumerate()
        .filter_map(|(index, mut item)| {
            item.extend(data.clone());
            item.insert("num".into(), (index + 1).into());
            let url = crate::sites::scalar(&item["url"]);
            let name = ["name", "original_filename"].iter().find_map(|k| {
                item.get(*k)
                    .and_then(Value::as_str)
                    .filter(|n| !n.is_empty())
                    .map(str::to_owned)
            });
            match name {
                Some(name) => {
                    let (stem, ext) = nameext_from_name(&name);
                    item.insert("filename".into(), stem.into());
                    item.insert("extension".into(), ext.into());
                }
                None => nameext_into(&url, &mut item),
            }
            let url = if url.starts_with('/') {
                format!("{root}{url}")
            } else {
                url
            };
            let id = crate::sites::scalar(&item["id"]);
            Some(Item::new(Url::parse(&url).ok()?, item).ids(post_id.clone(), id))
        })
        .collect()
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some((adult, target)) = target(&url) else {
        return crate::sites::fail(error("unsupported SubscribeStar URL"));
    };
    let root = if adult {
        "https://subscribestar.adult"
    } else {
        "https://www.subscribestar.com"
    };
    match target {
        Target::Post(id) => crate::sites::once(async move {
            login(http, root, adult).await?;
            let html = request(http, &format!("{root}/posts/{id}")).await?;
            let route = if adult { &POST_ADULT } else { &POST };
            Ok(crate::sites::events(crate::sites::post(
                &SITE,
                http,
                route,
                post_items(root, &html, true, &Map::new()),
            )))
        }),
        Target::User(user, query) => {
            let route = if adult { &USER_ADULT } else { &USER };
            let mut extra = Map::new();
            let params = parse_query(&query);
            if let Some(tag) = params.get("tag") {
                extra.insert("search_tags".into(), tag.clone());
            }
            let first = if query.is_empty() {
                format!("{root}/{user}")
            } else {
                format!("{root}/{user}?{query}")
            };
            crate::sites::paginate(Some((first, true)), move |state| {
                let extra = extra.clone();
                async move {
                    let Some((url, html_page)) = state else {
                        return Ok(None);
                    };
                    if html_page {
                        login(http, root, adult).await?;
                    }
                    let page = if html_page {
                        request(http, &url).await?
                    } else {
                        http.json(&http.api(&url)?).await?["html"]
                            .as_str()
                            .unwrap_or_default()
                            .to_owned()
                    };
                    let posts: Vec<&str> = page.split("<div class=\"post ").skip(1).collect();
                    if posts.is_empty() {
                        return Ok(None);
                    }
                    let next = extr(
                        posts.last().copied().unwrap_or_default(),
                        "data-role=\"infinite_scroll-next_page\" href=\"",
                        "\"",
                    );
                    let next =
                        (!next.is_empty()).then(|| (format!("{root}{}", unescape(next)), false));
                    let events = posts
                        .iter()
                        .flat_map(|post| {
                            crate::sites::post(
                                &SITE,
                                http,
                                route,
                                post_items(root, post, false, &extra),
                            )
                        })
                        .collect();
                    Ok(Some((crate::sites::events(events), next)))
                }
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http as client, json, keywords, media as files_of, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.subscribestar.com/subscribestar"),
            Some((false, Target::User("subscribestar".into(), String::new())))
        );
        assert_eq!(
            t("https://subscribestar.adult/kanashiipanda?tag=x"),
            Some((true, Target::User("kanashiipanda".into(), "tag=x".into())))
        );
        assert_eq!(
            t("https://www.subscribestar.com/posts/102468"),
            Some((false, Target::Post("102468".into())))
        );
    }

    #[tokio::test]
    async fn user_pages_posts() {
        let post = r#"<div class="post " data-id="11"><a data-user-id="5" href="/subscribestar">SubscribeStar</a><div class="post-date"><a>Mar 10, 2021 01:02 PM</a></div>
            <div class="post-content" data-role="post_content-text"><h1>Title</h1></div><div class="post-uploads for-youtube">
            <div data-gallery="[{&quot;id&quot;:7,&quot;url&quot;:&quot;https://ss-uploads-prod.b-cdn.net/uploads/a.jpg&quot;,&quot;original_filename&quot;:&quot;pic.jpg&quot;}]"></div>
            <a data-role="infinite_scroll-next_page" href="/subscribestar/posts?page=2">"#;
        let (root, server) = serve(vec![html(post), json(json!({"html": ""}))]);
        let http = client("subscribestar", root);
        let files = files_of(extract(
            &http,
            Url::parse("https://www.subscribestar.com/subscribestar").unwrap(),
        ))
        .await;
        let k = keywords(&files[0]);
        assert_eq!(
            (
                k["title"].as_str(),
                k["author_name"].as_str(),
                k["date"].as_str()
            ),
            (
                Some("Title"),
                Some("subscribestar"),
                Some("2021-03-10 13:02:00")
            )
        );
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "11_7.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("subscribestar/subscribestar")
        );
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /subscribestar/posts?page=2 "));
    }
}
