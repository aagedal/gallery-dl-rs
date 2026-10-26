//! Ci-en articles, creators, recent posts and subscriptions, mirroring
//! gallery_dl/extractor/cien.py.
use crate::error;
use crate::sites::gdl::{
    captures, date, extr, extract_iter, group, jsonld, nameext_into, parse_int, parse_iso,
    parse_query, partition, re, root_of, rpartition,
};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use std::time::Duration;
use url::Url;

const BASE: &str = r"^https?://ci-en\.(?:net|dlsite\.com)";

static ARTICLE: Route = Route::new(
    "article",
    "{num:>02} {filename}.{extension}",
    &["{category}", "{author[name]}", "{post_id} {name}"],
    "{post_id}_{num}",
);
static CREATOR: Route = Route::new("creator", "{filename}.{extension}", &["{category}"], "");
static RECENT: Route = Route::new("recent", "{filename}.{extension}", &["{category}"], "");
static FOLLOWING: Route = Route::new("following", "{filename}.{extension}", &["{category}"], "");

pub static SITE: Site = Site {
    category: "cien",
    name: "Ci-en",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&ARTICLE, &CREATOR, &RECENT, &FOLLOWING],
    api_key: false,
    cookies: Some("ci-en.net"),
};

#[derive(Debug, PartialEq)]
enum Target {
    Article(String, String),
    Creator(String, String),
    Recent(String),
    Following(String),
}

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(re!(&format!(r"{BASE}/creator/(\d+)/article/(\d+)")), url) {
        return Some(Target::Article(group(&c, 1).into(), group(&c, 2).into()));
    }
    if let Some(c) = captures(
        re!(&format!(
            r"{BASE}/creator/(\d+)(?:/article(?:\?([^#]+))?)?/?$"
        )),
        url,
    ) {
        return Some(Target::Creator(group(&c, 1).into(), group(&c, 2).into()));
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}/mypage/recent(?:\?([^#]+))?")), url) {
        return Some(Target::Recent(group(&c, 1).into()));
    }
    captures(
        re!(&format!(r"{BASE}/mypage/subscription(/following)?")),
        url,
    )
    .map(|c| Target::Following(group(&c, 1).into()))
}

/// Upstream `request_interval = (1.0, 2.0)`.
async fn pace() {
    if !cfg!(test) {
        tokio::time::sleep(Duration::from_millis(1500)).await;
    }
}

async fn text(http: &Http, url: &Url) -> crate::Result<String> {
    pace().await;
    // Upstream `_init` accepts R-18G content on the dlsite mirror.
    let mirror = Url::parse("https://ci-en.dlsite.com/").map_err(|_| error("invalid Ci-en URL"))?;
    if http.cookie(&mirror, "accepted_rating").is_none() {
        http.set_cookie(
            &mirror,
            "accepted_rating=r18g; Domain=ci-en.dlsite.com; Path=/",
        );
    }
    http.text(url).await
}

fn file(url: String, kind: &str) -> Map<String, Value> {
    let mut file = Map::new();
    file.insert("url".into(), url.into());
    file.insert("type".into(), kind.into());
    file
}

/// `_extract_files` for the configured (or default) file types.
async fn files(http: &Http, root: &str, page: &str) -> crate::Result<Vec<Map<String, Value>>> {
    let kinds: Vec<String> = match http.option("files") {
        None | Some(Value::Null) => ["image", "video", "download", "gallery"]
            .map(str::to_owned)
            .to_vec(),
        Some(Value::String(s)) => s.split(',').map(str::to_owned).collect(),
        Some(Value::Array(list)) => list.iter().map(crate::sites::scalar).collect(),
        Some(other) => vec![crate::sites::scalar(other)],
    };
    let mut files = Vec::new();
    for kind in kinds {
        match kind.trim_end_matches('s') {
            "cover" => files.push(file(
                extr(page, "property=\"og:image\"\n          content=\"", "\"").into(),
                "cover",
            )),
            "image" => {
                for image in extract_iter(page, "class=\"file-player-image\"", "</figure>") {
                    let (w, _, h) = partition(extr(image, " data-size=\"", "\""), "x");
                    let mut entry = file(extr(image, " data-raw=\"", "\"").into(), "image");
                    entry.insert("width".into(), parse_int(w).into());
                    entry.insert("height".into(), parse_int(h).into());
                    files.push(entry);
                }
            }
            "video" => {
                for video in extract_iter(page, "<vue-file-player", "</vue-file-player>") {
                    let path = extr(video, " base-path=\"", "\"");
                    let auth = extr(video, " auth-key=\"", "\"");
                    let mut entry = Map::new();
                    nameext_into(extr(video, " file-name=\"", "\""), &mut entry);
                    entry.insert("url".into(), format!("{path}video-web.mp4?{auth}").into());
                    entry.insert("type".into(), "video".into());
                    files.push(entry);
                }
            }
            "download" => {
                for download in extract_iter(page, "class=\"downloadBlock", "</div>") {
                    let mut entry = Map::new();
                    nameext_into(rpartition(extr(download, "<p>", "<"), " ").0, &mut entry);
                    entry.insert("url".into(), extr(download, " href=\"", "\"").into());
                    entry.insert("type".into(), "download".into());
                    files.push(entry);
                }
            }
            "gallery" | "gallerie" => {
                for gallery in extract_iter(page, "<vue-image-gallery", "</vue-image-gallery>") {
                    let params = [
                        ("hash", extr(gallery, " hash=\"", "\"")),
                        ("gallery_id", extr(gallery, " gallery-id=\"", "\"")),
                        ("time", extr(gallery, " time=\"", "\"")),
                    ];
                    let mut url = http.api(&format!("{root}/api/creator/gallery/images"))?;
                    url.query_pairs_mut().extend_pairs(params.iter());
                    pace().await;
                    let data = http.json(&url).await?;
                    for (index, file_id) in
                        data["imgList"].as_array().into_iter().flatten().enumerate()
                    {
                        let file_id = crate::sites::scalar(file_id);
                        let mut url = http.api(&format!("{root}/api/creator/gallery/imagePath"))?;
                        url.query_pairs_mut()
                            .extend_pairs(params.iter())
                            .append_pair("page", &index.to_string())
                            .append_pair("file_id", &file_id);
                        pace().await;
                        let path = http.json(&url).await?["path"].clone();
                        let mut entry = Map::new();
                        for (key, value) in params {
                            entry.insert(key.into(), value.into());
                        }
                        entry.insert("page".into(), index.into());
                        entry.insert("file_id".into(), file_id.into());
                        entry.insert("url".into(), path);
                        files.push(entry);
                    }
                }
            }
            other => return Err(error(format!("unknown Ci-en file type '{other}'"))),
        }
    }
    Ok(files)
}

fn articles<'a>(
    http: &'a Http,
    route: &'static Route,
    url: String,
    query: String,
    list_mode: bool,
) -> Events<'a> {
    let mut params = parse_query(&query);
    let first = crate::sites::gdl::int_of(params.get("page")).max(1);
    params.remove("page");
    if list_mode {
        params.insert("mode".into(), "list".into());
    }
    crate::sites::paginate(Some(first), move |state| {
        let url = url.clone();
        let params = params.clone();
        async move {
            let Some(number) = state else { return Ok(None) };
            let mut request = http.api(&url)?;
            {
                let mut pairs = request.query_pairs_mut();
                for (key, value) in &params {
                    pairs.append_pair(key, &crate::sites::scalar(value));
                }
                pairs.append_pair("page", &number.to_string());
            }
            let page = text(http, &request).await?;
            let children = extract_iter(&page, " class=\"c-cardCase-item", "</figure>")
                .into_iter()
                .filter_map(|card| {
                    Some((Url::parse(extr(card, " href=\"", "\"")).ok()?, Map::new()))
                })
                .collect();
            let next = page.contains(" rel=\"next\"").then_some(number + 1);
            Ok(Some((
                crate::sites::galleries::queue(http, &SITE, route, children),
                next,
            )))
        }
    })
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Ci-en URL"));
    };
    let root = root_of(&url);
    match target {
        Target::Article(author_id, post_id) => crate::sites::once(async move {
            let post_url = format!("{root}/creator/{author_id}/article/{post_id}");
            let page = text(http, &http.api(&post_url)?).await?;
            let files = files(http, &root, &page).await?;
            let ld = jsonld(&page)?;
            let mut post = match ld {
                Value::Array(list) => list.into_iter().next(),
                other => Some(other),
            }
            .and_then(|v| v.as_object().cloned())
            .ok_or_else(|| error("Ci-en article has no JSON-LD data"))?;
            post.insert("post_url".into(), post_url.as_str().into());
            post.insert("post_id".into(), parse_int(&post_id).into());
            post.insert("count".into(), files.len().into());
            let published = date(parse_iso(
                post.get("datePublished")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            ));
            post.insert("date".into(), published);
            if let Some(author) = post.get_mut("author").and_then(Value::as_object_mut) {
                author.insert("id".into(), parse_int(&author_id).into());
                post.remove("publisher");
                post.remove("sameAs");
            }
            let items = files
                .into_iter()
                .enumerate()
                .filter_map(|(index, file)| {
                    let url = crate::sites::scalar(&file["url"]);
                    let named = file.contains_key("extension");
                    post.insert("num".into(), (index + 1).into());
                    post.extend(file);
                    if !named {
                        nameext_into(&url, &mut post);
                    }
                    Some(
                        Item::new(Url::parse(&url).ok()?, post.clone())
                            .ids(post_id.clone(), (index + 1).to_string()),
                    )
                })
                .collect();
            Ok(crate::sites::events(crate::sites::post(
                &SITE, http, &ARTICLE, items,
            )))
        }),
        Target::Creator(id, query) => articles(
            http,
            &CREATOR,
            format!("{root}/creator/{id}/article"),
            query,
            true,
        ),
        Target::Recent(query) => {
            articles(http, &RECENT, format!("{root}/mypage/recent"), query, false)
        }
        Target::Following(path) => crate::sites::once(async move {
            let page = text(
                http,
                &http.api(&format!("{root}/mypage/subscription{path}"))?,
            )
            .await?;
            let children = extract_iter(&page, "class=\"c-grid-subscriptionInfo", "</figure>")
                .into_iter()
                .filter_map(|s| Some((Url::parse(extr(s, " href=\"", "\"")).ok()?, Map::new())))
                .collect();
            Ok(crate::sites::galleries::queue(
                http, &SITE, &FOLLOWING, children,
            ))
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{children, html, http as client, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://ci-en.net/creator/7491/article/1194568"),
            Some(Target::Article("7491".into(), "1194568".into()))
        );
        assert_eq!(
            t("https://ci-en.dlsite.com/creator/25509/article?page=2"),
            Some(Target::Creator("25509".into(), "page=2".into()))
        );
        assert_eq!(
            t("https://ci-en.net/mypage/subscription/following"),
            Some(Target::Following("/following".into()))
        );
    }

    #[tokio::test]
    async fn article_files_and_jsonld() {
        let page = r#"<script type="application/ld+json">[{"name": "Post", "datePublished": "2024-01-02T03:04:05+09:00", "author": {"name": "Artist"}, "publisher": {}, "sameAs": []}]</script>
            <figure class="file-player-image" data-size="800x600" data-raw="https://media.ci-en.jp/private/attachment/a.jpg?px-time=1"></figure>
            <vue-file-player base-path="https://media.ci-en.jp/video/x/" file-name="clip.mov" auth-key="k=1"></vue-file-player>
            <div class="downloadBlock"><p>pack.zip 12MB</p><a href="https://media.ci-en.jp/dl/pack.zip?x">dl</a></div>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = client("cien", root);
        let files = media(extract(
            &http,
            Url::parse("https://ci-en.net/creator/7/article/9").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 3);
        assert_eq!(
            files[1].url.as_str(),
            "https://media.ci-en.jp/video/x/video-web.mp4?k=1"
        );
        let k = keywords(&files[2]);
        assert_eq!(
            (
                k["filename"].as_str(),
                k["extension"].as_str(),
                k["author"]["id"].as_i64()
            ),
            (Some("pack"), Some("zip"), Some(7))
        );
        assert!(!k.contains_key("publisher"));
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "02 clip.mov");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("cien/Artist/9 Post")
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn creator_lists_articles() {
        let page = r#"<div class="c-cardCase-item"><a href="https://ci-en.net/creator/7/article/1">x</a></figure>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = client("cien", root);
        assert_eq!(
            children(extract(
                &http,
                Url::parse("https://ci-en.net/creator/7").unwrap()
            ))
            .await
            .len(),
            1
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /creator/7/article?mode=list&page=1 "));
    }
}
