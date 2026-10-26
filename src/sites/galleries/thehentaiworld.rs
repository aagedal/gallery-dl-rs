//! TheHentaiWorld posts and tags, mirroring gallery_dl/extractor/thehentaiworld.py.
use crate::error;
use crate::sites::gdl::{
    Scan, captures, date, extr, extract_iter, group, nameext_into, parse_float, parse_int,
    parse_iso, re, unescape,
};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use std::time::Duration;
use url::Url;

const ROOT: &str = "https://thehentaiworld.com";
const BASE: &str = r"^https?://(?:www\.)?thehentaiworld\.com";
const FILENAME: &str = "{title} ({id}{num:?-//}).{extension}";

static TAG: Route = Route::new(
    "tag",
    FILENAME,
    &["{category}", "{search_tags}"],
    "{id}_{num}",
);
static POST: Route = Route::new("post", FILENAME, &["{category}"], "{id}_{num}");

pub static SITE: Site = Site {
    category: "thehentaiworld",
    name: "TheHentaiWorld",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&TAG, &POST],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Tag(String),
    Post(String),
}

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(re!(&format!(r"{BASE}/tag/([^/?#]+)")), url) {
        return Some(Target::Tag(group(&c, 1).into()));
    }
    captures(
        re!(&format!(
            r"{BASE}(/(?:video|(?:[\w-]+-)?hentai-image)s/([^/?#]+))"
        )),
        url,
    )
    .map(|c| Target::Post(group(&c, 1).into()))
}

/// Upstream `request_interval = (0.5, 1.5)`.
async fn pace() {
    if !cfg!(test) {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// `_extract_post`: fields plus the file URLs.
fn parse_post(url: &str, page: &str) -> (Map<String, Value>, Vec<String>) {
    let mut scan = Scan::new(page);
    let mut post = Map::new();
    post.insert("num".into(), 0.into());
    post.insert("count".into(), 1.into());
    post.insert(
        "title".into(),
        unescape(scan.extr("<title>", "<").trim()).into(),
    );
    post.insert("id".into(), parse_int(scan.extr(" postid-", " ")).into());
    post.insert("slug".into(), scan.extr(" post-", "\"").into());
    let tags_html = scan.extr("id=\"tagsHead\">", "</ul>").to_owned();
    post.insert(
        "date".into(),
        date(parse_iso(scan.extr("<li>Posted: ", "<"))),
    );
    let kind = url.chars().nth(27).unwrap_or_default();
    let mut urls = Vec::new();
    if kind == 'v' {
        post.insert("type".into(), "video".into());
        post.insert("width".into(), 0.into());
        post.insert("height".into(), 0.into());
        post.insert(
            "votes".into(),
            parse_int(scan.extr("(<strong>", "</strong>")).into(),
        );
        post.insert(
            "score".into(),
            parse_float(scan.extr("<strong>", "<")).into(),
        );
        urls.push(scan.extr("<source src=\"", "\"").to_owned());
    } else {
        let kind = match kind {
            'g' => "animated",
            '3' => "3d cgi",
            _ => "image",
        };
        post.insert("type".into(), kind.into());
        post.insert(
            "width".into(),
            parse_int(scan.extr("<li>Size: ", " ")).into(),
        );
        post.insert("height".into(), parse_int(scan.extr("x ", "<")).into());
        let file = scan.extr("a href=\"", "\"").to_owned();
        post.insert(
            "votes".into(),
            parse_int(scan.extr("(<strong>", "</strong>")).into(),
        );
        post.insert(
            "score".into(),
            parse_float(scan.extr("<strong>", "<")).into(),
        );
        let doujin = scan.extr("<a id=\"prev-page\"", "</div></div><");
        if doujin.is_empty() {
            urls.push(file);
        } else {
            let pages: Vec<String> = extract_iter(doujin, "class=\"border\" src=\"", "\"")
                .into_iter()
                .map(|u| re!(r"-220x\d+\.").replace(u, ".").into_owned())
                .collect();
            post.insert("count".into(), pages.len().into());
            urls = pages;
            post.insert("_multi".into(), true.into());
        }
    }
    let mut groups: Vec<(String, Vec<Value>)> = Vec::new();
    for c in re!(r#"<li><a class="([^"]*)" href="[^"]*">([^<]+)"#).captures_iter(&tags_html) {
        match groups.iter_mut().find(|(k, _)| k == &c[1]) {
            Some((_, list)) => list.push(c[2].into()),
            None => groups.push((c[1].to_owned(), vec![c[2].into()])),
        }
    }
    let mut tags = Vec::new();
    for (key, values) in groups {
        tags.extend(values.clone());
        post.insert(
            if key.is_empty() {
                "tags_general".into()
            } else {
                format!("tags_{key}")
            },
            values.into(),
        );
    }
    post.insert("tags".into(), tags.into());
    (post, urls)
}

async fn post_items(http: &Http, url: &str) -> Vec<Item> {
    pace().await;
    let Ok(request) = http.api(url) else {
        return Vec::new();
    };
    let Ok(page) = http.text(&request).await else {
        return Vec::new();
    };
    let (post, urls) = parse_post(url, &page);
    let multi = post.contains_key("_multi");
    let id = crate::sites::scalar(&post["id"]);
    urls.into_iter()
        .enumerate()
        .filter_map(|(index, file)| {
            let mut fields = post.clone();
            fields.remove("_multi");
            if multi {
                fields.insert("num".into(), (index + 1).into());
            }
            nameext_into(&file, &mut fields);
            let num = crate::sites::scalar(&fields["num"]);
            Some(Item::new(Url::parse(&file).ok()?, fields).ids(id.clone(), num))
        })
        .collect()
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported TheHentaiWorld URL"));
    };
    match target {
        Target::Post(path) => crate::sites::once(async move {
            let items = post_items(http, &format!("{ROOT}{path}/")).await;
            Ok(crate::sites::events(crate::sites::post(
                &SITE, http, &POST, items,
            )))
        }),
        Target::Tag(tag) => crate::sites::paginate(Some(1u64), move |state| {
            let tag = tag.clone();
            async move {
                let Some(page_num) = state else {
                    return Ok(None);
                };
                let base = format!("{ROOT}/tag/{tag}/");
                let url = if page_num < 2 {
                    base
                } else {
                    format!("{base}page/{page_num}/")
                };
                let page = http.text(&http.api(&url)?).await?;
                let posts: Vec<String> = extract_iter(
                    extr(&page, "id=\"thumbContainer\"", "<script"),
                    " href=\"",
                    "\"",
                )
                .into_iter()
                .map(str::to_owned)
                .collect();
                let next = page.contains("class=\"next\"").then_some(page_num + 1);
                let events = crate::sites::each(posts, move |post| {
                    let tag = tag.clone();
                    async move {
                        let items = post_items(http, &post)
                            .await
                            .into_iter()
                            .map(|mut item| {
                                item.keywords
                                    .insert("search_tags".into(), tag.as_str().into());
                                item
                            })
                            .collect();
                        Ok(crate::sites::post(&SITE, http, &TAG, items))
                    }
                });
                Ok(Some((events, next)))
            }
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http as client, keywords, media, serve};

    #[test]
    fn parses_doujin_and_tags() {
        let page = r#"<title> Title </title><body class="post postid-42 post-slug"><ul id="tagsHead"><li><a class="character" href="/t/a">Alice</a><li><a class="" href="/t/b">blonde</a><li><a class="character" href="/t/c">Bob</a></ul>
            <li>Posted: 2024-01-02</li><li>Size: 800 x 600</li><a href="https://x/full.jpg">(<strong>5</strong>) <strong>4.5</strong>
            <a id="prev-page"><img class="border" src="https://x/p1-220x300.jpg"><img class="border" src="https://x/p2-220x300.png"></div></div><"#;
        let (post, urls) = parse_post("https://thehentaiworld.com/hentai-images/slug/", page);
        assert_eq!(urls, ["https://x/p1.jpg", "https://x/p2.png"]);
        assert_eq!(post["tags_character"], serde_json::json!(["Alice", "Bob"]));
        assert_eq!(post["tags"], serde_json::json!(["Alice", "Bob", "blonde"]));
        assert_eq!(
            (
                post["type"].as_str(),
                post["width"].as_i64(),
                post["score"].as_f64()
            ),
            (Some("image"), Some(800), Some(4.5))
        );
    }

    #[tokio::test]
    async fn single_image_has_no_number() {
        let page = r#"<title>Title</title> postid-42 post-slug"<li>Size: 800 x 600</li><a href="https://x/full.jpg">"#;
        let (root, server) = serve(vec![html(page)]);
        let http = client("thehentaiworld", root);
        let files = media(extract(
            &http,
            Url::parse("https://thehentaiworld.com/gif-animated-hentai-images/slug/").unwrap(),
        ))
        .await;
        assert_eq!(keywords(&files[0])["type"], "animated");
        let (file, _) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "Title (42).jpg");
        server.join().unwrap();
    }
}
