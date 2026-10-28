//! *reactor sites (reactor.cc, PornReactor, ThatPervert): tags, searches,
//! users and posts, mirroring gallery_dl/extractor/reactor.py.
use crate::error;
use crate::sites::gdl::{
    date, extr, extract_iter, nameext_into, parse_int, parse_iso, partition, re, rpartition,
    truthy, unescape, ytdl_item,
};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use std::time::Duration;
use url::Url;

const FILENAME: &str = "{post_id}_{num:>02}{title[:100]:?_//}.{extension}";

static TAG: Route = Route::new(
    "tag",
    FILENAME,
    &["{category}", "{search_tags}"],
    "{search_tags}_{post_id}_{num}",
);
static SEARCH: Route = Route::new(
    "search",
    FILENAME,
    &["{category}", "search", "{search_tags}"],
    "s_{search_tags}_{post_id}_{num}",
);
static USER: Route = Route::new(
    "user",
    FILENAME,
    &["{category}", "user", "{user}"],
    "{post_id}_{num}",
);
static POST: Route = Route::new("post", FILENAME, &["{category}"], "{post_id}_{num}");
const ROUTES: &[&Route] = &[&TAG, &SEARCH, &USER, &POST];

macro_rules! instance {
    ($site:ident, $category:literal, $name:literal, $host:literal) => {
        pub static $site: Site = Site {
            category: $category,
            name: $name,
            matches: |url| target($host, url).is_some(),
            extract: |http, url| extract(&$site, $host, http, url),
            routes: ROUTES,
            api_key: false,
            cookies: None,
        };
    };
}
instance!(REACTOR, "reactor", "Reactor", r"(?:[^/.]+\.)?reactor\.cc");
instance!(
    PORNREACTOR,
    "pornreactor",
    "PornReactor",
    r"(?:www\.)?(?:pornreactor\.cc|fapreactor.com)"
);
instance!(
    THATPERVERT,
    "thatpervert",
    "ThatPervert",
    r"thatpervert\.com"
);

#[derive(Debug, PartialEq)]
enum Kind {
    Tag(String),
    Search(String),
    User(String),
    Post,
}

fn compiled(host: &'static str) -> regex::Regex {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<&'static str, regex::Regex>>> = OnceLock::new();
    let mut cache = CACHE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    cache
        .entry(host)
        .or_insert_with(|| {
            regex::Regex::new(&format!(r"^(https?://{host})(/.*)$"))
                .expect("static reactor pattern")
        })
        .clone()
}

/// Root, path and target.
fn target(host: &'static str, url: &Url) -> Option<(String, String, Kind)> {
    let c = compiled(host).captures(url.as_str())?;
    let (root, path) = (c[1].to_owned(), c[2].to_owned());
    let kind = if let Some(c) = re!(r"^/tag/([^/?#]+)(?:/[^/?#]+)?").captures(&path) {
        Kind::Tag(c[1].to_owned())
    } else if let Some(c) = re!(r"^/search(?:/|\?q=)([^/?#]+)").captures(&path) {
        Kind::Search(c[1].to_owned())
    } else if let Some(c) = re!(r"^/user/([^/?#]+)").captures(&path) {
        Kind::User(c[1].to_owned())
    } else if re!(r"^/post/(\d+)").is_match(&path) {
        Kind::Post
    } else {
        return None;
    };
    Some((root, path, kind))
}

/// Upstream `request_interval = (3.0, 6.0)`.
async fn pace() {
    if !cfg!(test) {
        tokio::time::sleep(Duration::from_secs(4)).await;
    }
}

/// `_parse_post`: the images of one post block with its JSON-LD data.
fn parse_post(gif: bool, post: &str) -> Vec<Map<String, Value>> {
    let (post, _, script) = partition(post, "<script type=\"application/ld+json\">");
    if script.is_empty() {
        return Vec::new();
    }
    let script = script[..script.find("</").unwrap_or(script.len())].trim();
    let data: Value = match serde_json::from_str(script) {
        Ok(data) => data,
        Err(_) => {
            let cleaned: String = script
                .chars()
                .filter(|c| (*c as u32) >= 32)
                .collect::<String>()
                .replace('\\', "\\\\");
            match serde_json::from_str(&cleaned) {
                Ok(data) => data,
                Err(_) => return Vec::new(),
            }
        }
    };
    let published = date(parse_iso(
        data["datePublished"].as_str().unwrap_or_default(),
    ));
    let user = data["author"]["name"].clone();
    let description = unescape(data["description"].as_str().unwrap_or_default());
    let headline = unescape(data["headline"].as_str().unwrap_or_default());
    let (mut title, _, mut tags) = partition(&headline, " / ");
    if tags.is_empty() {
        std::mem::swap(&mut title, &mut tags);
    }
    let mut tags: Vec<String> = tags.split(" :: ").map(str::to_owned).collect();
    tags.sort();
    let post_id = parse_int(
        rpartition(
            data["mainEntityOfPage"]["@id"].as_str().unwrap_or_default(),
            "/",
        )
        .2,
    );
    let mut num = 0;
    let mut out = Vec::new();
    for image in extract_iter(post, "<div class=\"image\">", "</div>") {
        let mut url = extr(image, " src=\"", "\"").to_owned();
        if url.is_empty() {
            continue;
        }
        if url.starts_with("//") {
            url = format!("http:{url}");
        }
        let image_id = parse_int(partition(rpartition(&url, "-").2, ".").0);
        num += 1;
        let video = url.contains("/post/webm/") || url.contains("/post/mp4/");
        if image.starts_with("<iframe ") {
            url = format!("ytdl:{}", unescape(&url));
        } else if !video {
            url = url.replacen("/post/", "/post/full/", 1);
        }
        if gif && (url.contains("/post/webm/") || url.contains("/post/mp4/")) {
            let gif_url = extr(image, "<a href=\"", "\"");
            if gif_url.is_empty() {
                continue;
            }
            url = gif_url.to_owned();
        }
        let mut file = Map::new();
        file.insert("url".into(), url.as_str().into());
        file.insert("post_id".into(), post_id.into());
        file.insert("image_id".into(), image_id.into());
        file.insert(
            "width".into(),
            parse_int(extr(image, " width=\"", "\"")).into(),
        );
        file.insert(
            "height".into(),
            parse_int(extr(image, " height=\"", "\"")).into(),
        );
        file.insert("title".into(), title.into());
        file.insert("description".into(), description.as_str().into());
        file.insert("tags".into(), tags.clone().into());
        file.insert("date".into(), published.clone());
        file.insert("user".into(), user.clone());
        file.insert("num".into(), num.into());
        out.push(file);
    }
    out
}

fn items(files: Vec<Map<String, Value>>, extra: &Map<String, Value>) -> Vec<Item> {
    files
        .into_iter()
        .filter_map(|mut file| {
            file.extend(extra.clone());
            let url = crate::sites::scalar(&file["url"]);
            nameext_into(&url, &mut file);
            let (id, num) = (
                crate::sites::scalar(&file["post_id"]),
                crate::sites::scalar(&file["num"]),
            );
            let item = match url.strip_prefix("ytdl:") {
                Some(page) => ytdl_item(page, file)?,
                None => Item::new(Url::parse(&url).ok()?, file),
            };
            Some(item.ids(id, num))
        })
        .collect()
}

fn extract<'a>(site: &'static Site, host: &'static str, http: &'a Http, url: Url) -> Events<'a> {
    let Some((root, path, kind)) = target(host, &url) else {
        return crate::sites::fail(error("unsupported reactor URL"));
    };
    let gif = truthy(http.option("gif"));
    let mut extra = Map::new();
    let route = match &kind {
        Kind::Tag(tag) | Kind::Search(tag) => {
            extra.insert("search_tags".into(), unescape(tag).replace('+', " ").into());
            if matches!(kind, Kind::Tag(_)) {
                &TAG
            } else {
                &SEARCH
            }
        }
        Kind::User(user) => {
            extra.insert("user".into(), unescape(user).replace('+', " ").into());
            &USER
        }
        Kind::Post => {
            return crate::sites::once(async move {
                pace().await;
                let page = http.text(&http.api(&format!("{root}{path}"))?).await?;
                let post = page.find("class=\"uhead\">").map_or("", |pos| &page[pos..]);
                Ok(crate::sites::events(crate::sites::post(
                    site,
                    http,
                    &POST,
                    items(parse_post(gif, post), &Map::new()),
                )))
            });
        }
    };
    crate::sites::paginate(Some(format!("{root}{path}")), move |state| {
        let root = root.clone();
        let extra = extra.clone();
        async move {
            let Some(url) = state else { return Ok(None) };
            pace().await;
            let request = http.api(&url)?;
            let (page, final_url) = http.text_final(&request, &[]).await?;
            // The last page of some listings redirects to the first one.
            if final_url != request
                && url.matches('/').count() == 5
                && final_url.as_str().matches('/').count() == 4
            {
                return Ok(None);
            }
            let events = extract_iter(&page, "<div class=\"uhead\">", "<div class=\"ufoot\">")
                .into_iter()
                .flat_map(|post| {
                    crate::sites::post(site, http, route, items(parse_post(gif, post), &extra))
                })
                .collect();
            let next = page
                .find("class='next'")
                .and_then(|pos| page[..pos].rfind("class='current'"))
                .map(|pos| extr(&page[pos..], "href='", "'"))
                .filter(|href| !href.is_empty())
                .map(|href| format!("{root}{href}"));
            Ok(Some((crate::sites::events(events), next)))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http as client, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |host, raw: &str| target(host, &Url::parse(raw).unwrap()).map(|(r, _, k)| (r, k));
        assert_eq!(
            t(
                r"(?:[^/.]+\.)?reactor\.cc",
                "http://anime.reactor.cc/tag/Anime+Art"
            ),
            Some((
                "http://anime.reactor.cc".into(),
                Kind::Tag("Anime+Art".into())
            ))
        );
        assert_eq!(
            t(
                r"(?:www\.)?(?:pornreactor\.cc|fapreactor.com)",
                "http://fapreactor.com/search?q=cat"
            ),
            Some(("http://fapreactor.com".into(), Kind::Search("cat".into())))
        );
        assert_eq!(
            t(r"thatpervert\.com", "http://thatpervert.com/post/4282"),
            Some(("http://thatpervert.com".into(), Kind::Post))
        );
    }

    #[tokio::test]
    async fn post_reads_ld_json_images() {
        let page = r#"<div class="uhead">x<div class="image"><img src="//img10.reactor.cc/pics/post/Title-art-12345.jpeg" width="800" height="600"></div>
            <div class="image"><iframe src="https://www.youtube.com/embed/abc"></iframe></div>
            <script type="application/ld+json">{"datePublished": "2020-01-02T03:04:05+03:00", "author": {"name": "jane"}, "description": "d",
            "headline": "Title / art :: anime", "mainEntityOfPage": {"@id": "http://reactor.cc/post/4282"}}</script>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = client("reactor", root);
        let files = media(extract(
            &REACTOR,
            r"(?:[^/.]+\.)?reactor\.cc",
            &http,
            Url::parse("http://reactor.cc/post/4282").unwrap(),
        ))
        .await;
        assert_eq!(
            files[0].url.as_str(),
            "http://img10.reactor.cc/pics/post/full/Title-art-12345.jpeg"
        );
        assert_eq!(
            files[1].url.as_str(),
            "ytdl:https://www.youtube.com/embed/abc"
        );
        let k = keywords(&files[0]);
        assert_eq!(
            (
                k["title"].as_str(),
                k["image_id"].as_i64(),
                k["post_id"].as_i64()
            ),
            (Some("Title"), Some(12345), Some(4282))
        );
        assert_eq!(k["tags"], serde_json::json!(["anime", "art"]));
        let (file, _) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "4282_01_Title.jpg");
        server.join().unwrap();
    }
}
