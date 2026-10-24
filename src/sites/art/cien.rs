//! Ci-en articles, creators, recent articles and followed creators,
//! mirroring gallery_dl/extractor/cien.py by scraping the article pages
//! of ci-en.net and ci-en.dlsite.com.
//!
//! Supporter-only files need the site's session cookies from `--cookies`;
//! files on the site's own hosts are fetched with them. Upstream's
//! `request_interval` of one to two seconds is not applied.
use super::{extr, extract_iter, leading_digits, parse_int, parse_query};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use futures_util::{StreamExt, stream};
use serde_json::{Map, Value};
use url::Url;

pub static SITE: Site = Site {
    category: "cien",
    name: "Ci-en",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&ARTICLE, &CREATOR, &RECENT, &FOLLOWING],
    api_key: false,
    // `ci-en.dlsite.com` comes from `extra_cookie_domains`.
    cookies: Some("ci-en.net"),
};

static ARTICLE: Route = Route::new(
    "article",
    "{num:>02} {filename}.{extension}",
    &["{category}", "{author[name]}", "{post_id} {name}"],
    "{post_id}_{num}",
);
// The listing extractors only queue articles and creators.
static CREATOR: Route = Route::new("creator", "{filename}.{extension}", &["{category}"], "");
static RECENT: Route = Route::new("recent", "{filename}.{extension}", &["{category}"], "");
static FOLLOWING: Route = Route::new("following", "{filename}.{extension}", &["{category}"], "");

type Query = Vec<(String, String)>;

#[derive(Debug, PartialEq)]
enum Target {
    /// Root, creator ID and article ID.
    Article(&'static str, String, String),
    Creator(&'static str, String, Query),
    Recent(&'static str, Query),
    /// Root and whether only followed (not supported) creators are listed.
    Following(&'static str, bool),
}

fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) {
        return None;
    }
    let root = match url.host_str()? {
        "ci-en.net" => "https://ci-en.net",
        "ci-en.dlsite.com" => "https://ci-en.dlsite.com",
        _ => return None,
    };
    let path = url.path();
    let query = url.query().filter(|q| !q.is_empty());
    if let Some(rest) = path.strip_prefix("/creator/") {
        let creator = leading_digits(rest)?;
        let rest = &rest[creator.len()..];
        if let Some(article) = rest.strip_prefix("/article/").and_then(leading_digits) {
            return Some(Target::Article(root, creator.into(), article.into()));
        }
        // `(?:/article(?:\?([^#]+))?)?/?$`
        let query = match (rest, query) {
            ("/article", Some(query)) => parse_query(query),
            ("" | "/" | "/article" | "/article/", None) => Vec::new(),
            _ => return None,
        };
        return url
            .fragment()
            .is_none()
            .then(|| Target::Creator(root, creator.into(), query));
    }
    if let Some(rest) = path.strip_prefix("/mypage/recent") {
        let query = match query {
            Some(query) if rest.is_empty() => parse_query(query),
            _ => Vec::new(),
        };
        return Some(Target::Recent(root, query));
    }
    let rest = path.strip_prefix("/mypage/subscription")?;
    Some(Target::Following(root, rest.starts_with("/following")))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Ci-en URL"));
    };
    if let Ok(dlsite) = http.api("https://ci-en.dlsite.com/") {
        http.set_cookie(&dlsite, "accepted_rating=r18g; Path=/");
    }
    match target {
        Target::Article(root, creator, article) => crate::sites::once(async move {
            let url = format!("{root}/creator/{creator}/article/{article}");
            let page = http.text(&http.api(&url)?).await?;
            let files = files(http, root, &page).await?;
            let items = parse_article(&page, &url, &creator, &article, files)?
                .into_iter()
                .map(|item| super::session_cookies(http, item))
                .collect();
            Ok(crate::sites::events(crate::sites::post(
                &SITE, http, &ARTICLE, items,
            )))
        }),
        Target::Creator(root, creator, mut query) => {
            set(&mut query, "mode", "list");
            articles(
                http,
                &CREATOR,
                root,
                format!("{root}/creator/{creator}/article"),
                query,
            )
        }
        Target::Recent(root, query) => {
            articles(http, &RECENT, root, format!("{root}/mypage/recent"), query)
        }
        Target::Following(root, following) => crate::sites::once(async move {
            let suffix = if following { "/following" } else { "" };
            let url = http.api(&format!("{root}/mypage/subscription{suffix}"))?;
            let page = http.text(&url).await?;
            let children = links(&page, root, " class=\"c-grid-subscriptionInfo", "</figure>");
            Ok(queue(http, &FOLLOWING, children))
        }),
    }
}

/// Python `params[key] = value` on an ordered query.
fn set(query: &mut Query, key: &str, value: &str) {
    match query.iter_mut().find(|(k, _)| k == key) {
        Some((_, v)) => *v = value.to_owned(),
        None => query.push((key.to_owned(), value.to_owned())),
    }
}

/// The `href` of each `begin`…`end` card that names a Ci-en page.
fn links(page: &str, root: &str, begin: &str, end: &str) -> Vec<Url> {
    extract_iter(page, begin, end)
        .into_iter()
        .filter_map(|card| super::file_url(extr(card, " href=\"", "\""), root))
        .filter(|url| target(url).is_some())
        .collect()
}

fn queue<'a>(http: &'a Http, route: &'static Route, children: Vec<Url>) -> Events<'a> {
    let subcategory = route.subcategory;
    stream::iter(children)
        .map(move |url| crate::sites::queue(http, "cien", subcategory, url, &Map::new()))
        .flatten()
        .boxed()
}

/// Upstream `_pagination_articles`: queue each article card, following
/// `rel="next"` page links.
fn articles<'a>(
    http: &'a Http,
    route: &'static Route,
    root: &'static str,
    url: String,
    mut query: Query,
) -> Events<'a> {
    let first = query
        .iter()
        .find(|(k, _)| k == "page")
        .map_or(1, |(_, v)| parse_int(v, 1));
    set(&mut query, "page", &first.to_string());
    crate::sites::paginate(Some(first), move |page| {
        let (url, mut query) = (url.clone(), query.clone());
        async move {
            let Some(page) = page else {
                return Ok(None);
            };
            set(&mut query, "page", &page.to_string());
            let mut url = http.api(&url)?;
            url.query_pairs_mut().extend_pairs(&query);
            let text = http.text(&url).await?;
            let children = links(&text, root, " class=\"c-cardCase-item", "</figure>");
            let next = text.contains(" rel=\"next\"").then_some(page + 1);
            Ok(Some((queue(http, route, children), next)))
        }
    })
}

/// Upstream `_extract_files`: the `files` option's kinds in order, else
/// images, videos, downloads and galleries.
async fn files(http: &Http, root: &str, page: &str) -> Result<Vec<Map<String, Value>>> {
    let kinds: Vec<String> = match http.option("files") {
        None | Some(Value::Null) => ["image", "video", "download", "gallery"]
            .map(str::to_owned)
            .to_vec(),
        Some(Value::String(s)) => s.split(',').map(str::to_owned).collect(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        Some(_) => Vec::new(),
    };
    let mut files = Vec::new();
    for kind in kinds {
        match kind.trim_end_matches('s') {
            "cover" => files.push(file(&[
                (
                    "url",
                    extr(page, "property=\"og:image\"\n          content=\"", "\"").into(),
                ),
                ("type", "cover".into()),
            ])),
            "image" => files.extend(images(page)),
            "video" => files.extend(videos(page)),
            "download" => files.extend(downloads(page)),
            "gallery" | "gallerie" => files.extend(galleries(http, root, page).await?),
            _ => {}
        }
    }
    Ok(files)
}

fn file(pairs: &[(&str, Value)]) -> Map<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}

fn images(page: &str) -> Vec<Map<String, Value>> {
    extract_iter(page, "class=\"file-player-image\"", "</figure>")
        .into_iter()
        .map(|image| {
            let size = extr(image, " data-size=\"", "\"");
            let (w, h) = size.split_once('x').unwrap_or((size, ""));
            file(&[
                ("url", extr(image, " data-raw=\"", "\"").into()),
                ("width", parse_int(w, 0).into()),
                ("height", parse_int(h, 0).into()),
                ("type", "image".into()),
            ])
        })
        .collect()
}

fn videos(page: &str) -> Vec<Map<String, Value>> {
    extract_iter(page, "<vue-file-player", "</vue-file-player>")
        .into_iter()
        .map(|video| {
            let path = extr(video, " base-path=\"", "\"");
            let auth = extr(video, " auth-key=\"", "\"");
            let mut file = Map::new();
            super::nameext_into(extr(video, " file-name=\"", "\""), &mut file);
            file.insert("url".into(), format!("{path}video-web.mp4?{auth}").into());
            file.insert("type".into(), "video".into());
            file
        })
        .collect()
}

fn downloads(page: &str) -> Vec<Map<String, Value>> {
    extract_iter(page, "class=\"downloadBlock", "</div>")
        .into_iter()
        .map(|download| {
            // `name.rpartition(" ")[0]` drops the size after the name.
            let name = extr(download, "<p>", "<");
            let name = name.rsplit_once(' ').map_or("", |(name, _)| name);
            let mut file = Map::new();
            super::nameext_into(name, &mut file);
            file.insert("url".into(), extr(download, " href=\"", "\"").into());
            file.insert("type".into(), "download".into());
            file
        })
        .collect()
}

/// Upstream `_extract_files_gallery`: each gallery's image list, then one
/// `imagePath` request per image.
async fn galleries(http: &Http, root: &str, page: &str) -> Result<Vec<Map<String, Value>>> {
    let mut files = Vec::new();
    for gallery in extract_iter(page, "<vue-image-gallery", "</vue-image-gallery>") {
        let params = [
            ("hash", extr(gallery, " hash=\"", "\"")),
            ("gallery_id", extr(gallery, " gallery-id=\"", "\"")),
            ("time", extr(gallery, " time=\"", "\"")),
        ];
        let mut url = http.api(&format!("{root}/api/creator/gallery/images"))?;
        url.query_pairs_mut().extend_pairs(params);
        let data = http.json(&url).await?;
        let list = data
            .get("imgList")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for (index, file_id) in list.into_iter().enumerate() {
            let mut url = http.api(&format!("{root}/api/creator/gallery/imagePath"))?;
            url.query_pairs_mut()
                .extend_pairs(params)
                .append_pair("page", &index.to_string())
                .append_pair("file_id", &crate::sites::scalar(&file_id));
            let path = http.json(&url).await?;
            let mut file: Map<String, Value> = params
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).into()))
                .collect();
            file.insert("page".into(), index.into());
            file.insert("file_id".into(), file_id);
            file.insert(
                "url".into(),
                path.get("path").cloned().unwrap_or(Value::Null),
            );
            files.push(file);
        }
    }
    Ok(files)
}

/// Upstream `CienArticleExtractor.items`: the page's JSON-LD article with
/// each file's fields merged in turn.
pub(crate) fn parse_article(
    page: &str,
    url: &str,
    creator: &str,
    article: &str,
    files: Vec<Map<String, Value>>,
) -> Result<Vec<Item>> {
    let ld = match extr(page, "<script type=\"application/ld+json\">", "</script>") {
        "" => extr(page, "<script type='application/ld+json'>", "</script>"),
        ld => ld,
    };
    let ld = super::json_loads(ld, "Ci-en")?;
    let mut post = match ld {
        Value::Array(mut list) if !list.is_empty() => list.swap_remove(0),
        other => other,
    }
    .as_object()
    .cloned()
    .ok_or_else(|| error("Ci-en article has no metadata"))?;
    post.insert("post_url".into(), url.into());
    post.insert("post_id".into(), parse_int(article, 0).into());
    post.insert("count".into(), files.len().into());
    let published = post.get("datePublished").and_then(Value::as_str);
    let timestamp = super::parse_iso(published.unwrap_or_default());
    post.insert("date".into(), super::date(timestamp));
    if let Some(Value::Object(author)) = post.get_mut("author") {
        author.insert("id".into(), parse_int(creator, 0).into());
        if post.remove("publisher").is_some() {
            post.remove("sameAs");
        }
    }
    let user = post
        .get("author")
        .and_then(|a| a.get("name"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let title = post.get("name").and_then(Value::as_str).map(str::to_owned);
    let mut items = Vec::new();
    for (index, file) in files.into_iter().enumerate() {
        post.insert("num".into(), (index + 1).into());
        super::update(&mut post, &file);
        let raw = file.get("url").and_then(Value::as_str).unwrap_or_default();
        if !file.contains_key("extension") {
            super::nameext_into(raw, &mut post);
        }
        let Some(url) = super::file_url(raw, url).filter(|_| !raw.is_empty()) else {
            continue;
        };
        items.push(
            Item::new(url, post.clone())
                .ids(article, (index + 1).to_string())
                .user(user.clone())
                .caption(title.clone())
                .timestamp(timestamp),
        );
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, json, keywords, media, queued, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://ci-en.net/creator/7491/article/1194568"),
            Some(Target::Article(
                "https://ci-en.net",
                "7491".into(),
                "1194568".into()
            ))
        );
        assert_eq!(
            t("https://ci-en.dlsite.com/creator/25509/article/1172460"),
            Some(Target::Article(
                "https://ci-en.dlsite.com",
                "25509".into(),
                "1172460".into()
            ))
        );
        for raw in [
            "https://ci-en.net/creator/7491",
            "https://ci-en.net/creator/7491/",
            "https://ci-en.net/creator/7491/article",
            "https://ci-en.net/creator/7491/article/",
        ] {
            assert_eq!(
                t(raw),
                Some(Target::Creator("https://ci-en.net", "7491".into(), vec![])),
                "{raw}"
            );
        }
        assert_eq!(
            t("https://ci-en.net/creator/7491/article?page=3&x=1"),
            Some(Target::Creator(
                "https://ci-en.net",
                "7491".into(),
                vec![("page".into(), "3".into()), ("x".into(), "1".into())]
            ))
        );
        assert_eq!(
            t("https://ci-en.net/mypage/recent?page=2"),
            Some(Target::Recent(
                "https://ci-en.net",
                vec![("page".into(), "2".into())]
            ))
        );
        assert_eq!(
            t("https://ci-en.net/mypage/subscription/following"),
            Some(Target::Following("https://ci-en.net", true))
        );
        assert_eq!(
            t("https://ci-en.net/mypage/subscription"),
            Some(Target::Following("https://ci-en.net", false))
        );
        for raw in [
            "https://ci-en.net/creator/7491?x=1",
            "https://ci-en.net/creator/7491/other",
            "https://ci-en.net/creator/7491#top",
            "https://ci-en.net/creator/abc",
            "https://www.ci-en.net/creator/1/article/2",
            "https://ci-en.net.evil.com/creator/1/article/2",
            "https://evilci-en.net/creator/1/article/2",
            "https://ci-en.net:8443/creator/1/article/2",
            "https://u:p@ci-en.net/creator/1/article/2",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    const ARTICLE_PAGE: &str = r#"<html><head>
<script type="application/ld+json">[{"@type":"Article","name":"Hello &amp; bye","datePublished":"2024-03-04T05:06:07+09:00","author":{"@type":"Person","name":"Maker"},"publisher":{"name":"Ci-en"},"sameAs":["x"]}]</script>
</head><body>
<figure class="file-player-image" data-size="800x600" data-raw="https://media.ci-en.jp/private/attachment/creator/1/a.png?px-time=1"></figure>
<vue-file-player base-path="https://media.ci-en.jp/private/attachment/creator/1/v/" file-name="clip.mp4" auth-key="px-time=2"></vue-file-player>
<div class="downloadBlock"><p>data.zip 12MB</p><a href="https://ci-en.net/dl/9">DL</a></div>
<vue-image-gallery hash="h1" gallery-id="g1" time="t1"></vue-image-gallery>
</body></html>"#;

    #[tokio::test]
    async fn extracts_article_files() {
        let (root, server) = serve(vec![
            html(ARTICLE_PAGE),
            json(json!({"imgList": [101]})),
            json(json!({"path": "https://media.ci-en.jp/gallery/g.jpg"})),
        ]);
        let http = http("cien", root.clone());
        http.set_cookie(&root, "ci_en_session=s; Path=/");
        let files = media(extract(
            &http,
            Url::parse("https://ci-en.net/creator/7491/article/1194568").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 4);
        let k = keywords(&files[0]);
        assert_eq!(k["post_id"], 1194568);
        assert_eq!(
            k["author"],
            json!({"@type": "Person", "name": "Maker", "id": 7491})
        );
        assert!(!k.contains_key("publisher") && !k.contains_key("sameAs"));
        assert_eq!(k["date"], "2024-03-03 20:06:07");
        assert_eq!(k["count"], 4);
        assert_eq!(k["width"], 800);
        assert_eq!(k["filename"], "a");
        let k = keywords(&files[1]);
        assert_eq!(k["filename"], "clip");
        assert_eq!(k["type"], "video");
        assert_eq!(k["width"], 800, "earlier file fields are kept");
        assert_eq!(
            files[1].url.as_str(),
            "https://media.ci-en.jp/private/attachment/creator/1/v/video-web.mp4?px-time=2"
        );
        let k = keywords(&files[2]);
        assert_eq!(k["filename"], "data");
        assert_eq!(k["extension"], "zip");
        let k = keywords(&files[3]);
        assert_eq!(k["gallery_id"], "g1");
        assert_eq!(k["file_id"], 101);
        assert_eq!(k["page"], 0);
        assert_eq!(k["filename"], "g");
        let (file, dir) = crate::sites::default_templates(&files[2]).unwrap();
        assert_eq!(file.render(&files[2]).unwrap(), "03 data.zip");
        assert_eq!(
            dir.render(&files[2]).unwrap().unwrap(),
            std::path::PathBuf::from("cien/Maker/1194568 Hello &amp; bye")
        );
        let (_, identity) = crate::sites::archive_key(&files[3]).unwrap();
        assert_eq!(identity, r#"["{post_id}_{num}",["1194568","4"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /creator/7491/article/1194568 "));
        assert!(
            requests[1]
                .starts_with("GET /api/creator/gallery/images?hash=h1&gallery_id=g1&time=t1 ")
        );
        assert!(requests[2].starts_with(
            "GET /api/creator/gallery/imagePath?hash=h1&gallery_id=g1&time=t1&page=0&file_id=101 "
        ));
    }

    #[tokio::test]
    async fn files_option_selects_kinds() {
        let page = ARTICLE_PAGE.replace(
            "<html><head>",
            "<meta property=\"og:image\"\n          content=\"https://media.ci-en.jp/cover.jpg\">",
        );
        let (root, server) = serve(vec![html(&page)]);
        let http = http("cien", root).with_options(Some(std::sync::Arc::new(
            serde_json::from_value(json!({"files": "covers,downloads"})).unwrap(),
        )));
        let files = media(extract(
            &http,
            Url::parse("https://ci-en.dlsite.com/creator/1/article/2").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(keywords(&files[0])["type"], "cover");
        assert_eq!(keywords(&files[1])["type"], "download");
        let requests = server.join().unwrap();
        assert!(requests[0].contains("accepted_rating=r18g"));
    }

    #[tokio::test]
    async fn creator_pages_queue_articles() {
        let first = r#"<div class="c-cardCase-item"><a href="https://ci-en.net/creator/1/article/5">x</a></figure>
            <div class="c-cardCase-item"><a href="https://evil.test/creator/1/article/6">x</a></figure>
            <a rel="next" href="?page=3">"#;
        let (root, server) = serve(vec![html(first), html("<p>end</p>")]);
        let http = http("cien", root);
        let children = queued(extract(
            &http,
            Url::parse("https://ci-en.net/creator/1/article?page=2").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].subcategory(), "creator");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /creator/1/article?page=2&mode=list "));
        assert!(requests[1].starts_with("GET /creator/1/article?page=3&mode=list "));
    }

    #[tokio::test]
    async fn following_queues_creators() {
        let page = r#"<div class="c-grid-subscriptionInfo"><a href="https://ci-en.net/creator/9">x</a></figure>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = http("cien", root);
        let children = queued(extract(
            &http,
            Url::parse("https://ci-en.net/mypage/subscription/following").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].subcategory(), "following");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /mypage/subscription/following "));
    }
}
