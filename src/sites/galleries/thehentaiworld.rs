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

/// Quote the unquoted attribute values of the site's minified markup
/// (`<a href=https://…>`, since 2026), which upstream's quoted patterns
/// would otherwise skip.
fn requote(page: &str) -> String {
    re!(r"<[A-Za-z][^<>]*>")
        .replace_all(page, |tag: &regex::Captures| {
            re!(r#"(\s[\w-]+)=("[^"]*"|'[^']*'|[^\s"'<>`]+)"#)
                .replace_all(&tag[0], |attr: &regex::Captures| match &attr[2][..1] {
                    "\"" | "'" => attr[0].to_owned(),
                    _ => format!("{}=\"{}\"", &attr[1], &attr[2]),
                })
                .into_owned()
        })
        .into_owned()
}

/// `(votes, score)` from the current `3.86 / 5 · 7 votes` rating line.
fn rating(page: &str) -> Option<(i64, f64)> {
    let c =
        re!(r#"class="thw-rating-meta">\s*([\d.]+)\s*/\s*\d+\s*·\s*(\d+) vote"#).captures(page)?;
    Some((c[2].parse().ok()?, c[1].parse().ok()?))
}

/// `_extract_post`: fields plus the file URLs.
fn parse_post(url: &str, page: &str) -> (Map<String, Value>, Vec<String>) {
    let page = &requote(page);
    let rating = rating(page);
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
        votes_and_score(&mut post, &mut scan, rating);
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
        votes_and_score(&mut post, &mut scan, rating);
        // Doujin pages were thumbnails after `prev-page`; they now sit in
        // `miniThumbContainer`, while `prev-page` links neighbouring posts.
        let mut doujin = scan.extr("<a id=\"prev-page\"", "</div></div><");
        if doujin.is_empty() {
            doujin = extr(page, "id=\"miniThumbContainer\"", "</div></div><");
        }
        if doujin.is_empty() {
            urls.push(file);
        } else {
            let pages: Vec<String> = re!(r#"class="border"[^>]*? src="([^"]+)""#)
                .captures_iter(doujin)
                .map(|c| re!(r"-220x\d+\.").replace(&c[1], ".").into_owned())
                .collect();
            post.insert("count".into(), pages.len().into());
            urls = pages;
            post.insert("_multi".into(), true.into());
        }
    }
    let mut groups: Vec<(String, Vec<Value>)> = Vec::new();
    // General tags now carry a bare `class` attribute.
    for c in re!(r#"<li><a class(?:="([^"]*)")? href="[^"]*">([^<]+)"#).captures_iter(&tags_html) {
        let key = c.get(1).map_or("", |m| m.as_str());
        match groups.iter_mut().find(|(k, _)| k == key) {
            Some((_, list)) => list.push(c[2].into()),
            None => groups.push((key.to_owned(), vec![c[2].into()])),
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

/// Upstream's `(<strong>votes</strong>) <strong>score</strong>`, or the
/// current rating line when the page has one.
fn votes_and_score(post: &mut Map<String, Value>, scan: &mut Scan, rating: Option<(i64, f64)>) {
    let (votes, score) = rating.unwrap_or_else(|| {
        let votes = parse_int(scan.extr("(<strong>", "</strong>"));
        (votes, parse_float(scan.extr("<strong>", "<")))
    });
    post.insert("votes".into(), votes.into());
    post.insert("score".into(), score.into());
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
                let page = requote(&http.text(&http.api(&url)?).await?);
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

    /// The minified markup served since 2026, with unquoted attributes.
    #[test]
    fn parses_minified_pages() {
        let head =
            r#"<title>Title &#8211; Artist</title><body class="single postid-226208 single-format-standard post-ubel">"#;
        let info = r#"<h3 id="tagsHead">Tags</h3><ul id=tags><li><a class=origin href=https://thehentaiworld.com/tag/o>Frieren</a> <span>5</span> </li><li><a class href=https://thehentaiworld.com/tag/b>blush</a> <span>9</span> </li></ul><h3>Info</h3><ul id=info> <li>Posted: 2024-04-16</li><li>Size: 800 x 1422</li><li><a href=https://thehentaiworld.com/wp-content/uploads/2024/04/Ubel.jpg>Full Size</a></li><li><span class=thw-rating-meta>4.64 / 5 · 11 votes</span></li></ul>"#;
        let nav = r#"<div id=doujin-nav><a class=bigBtn href=https://thehentaiworld.com/hentai-images/prev/ id=prev-page>Previous</a></div>"#;
        let thumbs = r#"<div id=miniThumbContainer><div class="current minithumb"> <a href="?pp=1"><img itemprop=thumbnail class=border loading=lazy src=https://thehentaiworld.com/wp-content/uploads/2024/04/Ubel-220x147.jpg width=160></a></div><div class=minithumb> <a href="?pp=2"><img itemprop=thumbnail class=border loading=lazy src=https://thehentaiworld.com/wp-content/uploads/2024/04/Ubel-2-220x147.jpg></a></div></div><div class="ad native">"#;
        let login = r#"<div class=wpd-login> <a href="https://thehentaiworld.com/wp-login.php?redirect_to=x">Login</a></div>"#;
        let related = r#"<div id=related><img class=border src=https://thehentaiworld.com/wp-content/uploads/other-220x147.jpg></div></div></div><"#;

        let single = format!("{head}{info}{nav}{login}{related}");
        let (post, urls) = parse_post("https://thehentaiworld.com/hentai-images/ubel/", &single);
        assert_eq!(
            urls,
            ["https://thehentaiworld.com/wp-content/uploads/2024/04/Ubel.jpg"]
        );
        assert_eq!(
            (post["id"].as_i64(), post["slug"].as_str()),
            (Some(226208), Some("ubel"))
        );
        assert_eq!(
            (post["width"].as_i64(), post["height"].as_i64()),
            (Some(800), Some(1422))
        );
        assert_eq!(
            (post["votes"].as_i64(), post["score"].as_f64()),
            (Some(11), Some(4.64))
        );
        assert_eq!(post["tags_origin"], serde_json::json!(["Frieren"]));
        assert_eq!(post["tags_general"], serde_json::json!(["blush"]));

        let doujin = format!("{head}{info}{nav}{thumbs}{login}{related}");
        let (post, urls) = parse_post("https://thehentaiworld.com/hentai-images/ubel/", &doujin);
        assert_eq!(
            urls,
            [
                "https://thehentaiworld.com/wp-content/uploads/2024/04/Ubel.jpg",
                "https://thehentaiworld.com/wp-content/uploads/2024/04/Ubel-2.jpg",
            ]
        );
        assert_eq!(post["count"], 2);

        let video = format!(
            "{head}<li>Posted: 2025-09-19</li><span class=thw-rating-meta>4.98 / 5 · 42 votes</span><video><source src=https://thehentaiworld.com/wp-content/uploads/v.mp4 type=video/mp4></video>{login}"
        );
        let (post, urls) = parse_post("https://thehentaiworld.com/videos/v/", &video);
        assert_eq!(
            urls,
            ["https://thehentaiworld.com/wp-content/uploads/v.mp4"]
        );
        assert_eq!(
            (post["votes"].as_i64(), post["type"].as_str()),
            (Some(42), Some("video"))
        );
    }

    #[test]
    fn requotes_only_unquoted_tag_attributes() {
        assert_eq!(
            requote(r#"<a class href=https://x/y?a=1 id="q" title='t'>if a=b</a><img src=/i.jpg>"#),
            r#"<a class href="https://x/y?a=1" id="q" title='t'>if a=b</a><img src="/i.jpg">"#
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
