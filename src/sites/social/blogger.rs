//! Blogger blogs (blogspot.com and `blogger:https://...`): posts, blogs,
//! searches and labels over the Blogger v3 API, mirroring
//! gallery_dl/extractor/blogger.py.
use crate::error;
use crate::sites::gdl::{
    extr, nameext_into, nested_date, parse_iso, re, remove_html, truthy, unquote,
};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const API: &str = "https://www.googleapis.com/blogger/v3";
const API_KEY: &str = "AIzaSyCN9ax34oMMyM07g_M-5pjeDp_312eITK8";
const FILENAME: &str = "{num:>03}.{extension}";
const DIRECTORY: &[&str] = &[
    "blogger",
    "{blog[name]}",
    "{post[date]:%Y-%m-%d} {post[title]}",
];

static POST: Route = Route::new("post", FILENAME, DIRECTORY, "{post[id]}_{num}");
static BLOG: Route = Route::new("blog", FILENAME, DIRECTORY, "{post[id]}_{num}");
static SEARCH: Route = Route::new("search", FILENAME, DIRECTORY, "{post[id]}_{num}");
static LABEL: Route = Route::new("label", FILENAME, DIRECTORY, "{post[id]}_{num}");
const ROUTES: &[&Route] = &[&POST, &BLOG, &SEARCH, &LABEL];

pub static BLOGSPOT: Site = Site {
    category: "blogspot",
    name: "Blogspot",
    matches: |url| target(url, false).is_some(),
    extract: |http, url| extract(&BLOGSPOT, false, http, url),
    routes: ROUTES,
    api_key: false,
    cookies: None,
};

// `blogger:https://host/...` for Blogger blogs on custom domains.
pub static BLOGGER: Site = Site {
    category: "blogger",
    name: "Blogger",
    matches: |url| target(url, true).is_some(),
    extract: |http, url| extract(&BLOGGER, true, http, url),
    routes: ROUTES,
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Kind {
    Post(String),
    Blog,
    Search(String),
    Label(String),
}

/// Blog host and target.
fn target(url: &Url, prefixed: bool) -> Option<(String, Kind)> {
    let raw = url.as_str();
    let c = if prefixed {
        re!(r"^blogger:https?://([^/?#]+)(.*)$").captures(raw)?
    } else {
        re!(r"^https?://([\w-]+\.blogspot\.com)(.*)$").captures(raw)?
    };
    let (host, rest) = (c[1].to_owned(), &c[2]);
    let kind = if let Some(c) = re!(r"^(/\d\d\d\d/\d\d/[^/?#]+\.html)").captures(rest) {
        Kind::Post(c[1].to_owned())
    } else if re!(r"^/?$").is_match(rest) {
        Kind::Blog
    } else if let Some(c) = re!(r"^/search/?\?q=([^&#]+)").captures(rest) {
        Kind::Search(unquote(&c[1]))
    } else {
        Kind::Label(unquote(
            &re!(r"^/search/label/([^/?#]+)").captures(rest)?[1],
        ))
    };
    Some((host, kind))
}

/// `blogger.original`: full-size image URLs.
fn original(url: &str) -> String {
    let replaced = re!(r"(/|=)(?:[sw]\d+|w\d+-h\d+)(/|$)").replace_all(url, "${1}s0${2}");
    replaced.replacen("http:", "https:", 1)
}

async fn call(http: &Http, endpoint: &str, params: &[(&str, &str)]) -> crate::Result<Value> {
    let key = http
        .option("api-key")
        .and_then(Value::as_str)
        .unwrap_or(API_KEY)
        .to_owned();
    let mut url = http.api(&format!("{API}{endpoint}"))?;
    url.query_pairs_mut()
        .extend_pairs(params.iter())
        .append_pair("key", &key);
    http.json(&url).await
}

/// `_extract_videos`: the best stream of each embedded Blogger video.
async fn videos(http: &Http, host: &str, post_id: &str) -> crate::Result<Vec<String>> {
    let mut url = http.api(&format!("https://{host}/feeds/posts/default/{post_id}"))?;
    url.query_pairs_mut()
        .append_pair("alt", "json")
        .append_pair("v", "2")
        .append_pair("dynamicviews", "1")
        .append_pair("rewriteforssl", "true");
    let data = http.json(&url).await?;
    let html = data["entry"]["content"]["$t"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let mut out = Vec::new();
    for c in
        re!(r#"src=["'](https?://www\.blogger\.com/video\.g\?token=[^"']+)"#).captures_iter(&html)
    {
        let page = http.text(&http.api(&c[1])?).await?;
        let Ok(config) = serde_json::from_str::<Value>(extr(&page, "var VIDEO_CONFIG =", "\n"))
        else {
            continue;
        };
        let best = config["streams"]
            .as_array()
            .into_iter()
            .flatten()
            .max_by_key(|s| s["format_id"].as_i64().unwrap_or_default());
        if let Some(best) = best {
            out.push(crate::sites::scalar(&best["play_url"]));
        }
    }
    Ok(out)
}

async fn post_items(
    http: &Http,
    host: &str,
    blog: &Map<String, Value>,
    post: &Value,
    extra: &Map<String, Value>,
) -> crate::Result<Vec<Item>> {
    let Some(mut post) = post.as_object().cloned() else {
        return Ok(Vec::new());
    };
    let content = post
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let mut files: Vec<String> = re!(r#"src="(https?://(?:blogger\.googleusercontent\.com/img|lh\d+(?:-\w+)?\.googleusercontent\.com|\d+\.bp\.blogspot\.com)/[^"]+)"#)
        .captures_iter(&content)
        .map(|c| original(&c[1]))
        .collect();
    let with_videos = http.option("videos").is_none_or(|v| truthy(Some(v)));
    if with_videos
        && (content.contains("id=\"BLOG_video-") || content.contains("class=\"BLOG_video_"))
    {
        files.extend(videos(http, host, &crate::sites::scalar(&post["id"])).await?);
    }
    let author = post["author"]["displayName"].clone();
    post.insert("author".into(), author);
    let replies = post["replies"]["totalItems"].clone();
    post.insert("replies".into(), replies);
    post.insert("content".into(), remove_html(&content).into());
    let published = nested_date(parse_iso(
        post.get("published")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    ));
    post.insert("date".into(), published);
    post.remove("selfLink");
    post.remove("blog");
    let post_id = crate::sites::scalar(&post["id"]);
    let mut data = Map::new();
    data.insert("blog".into(), Value::Object(blog.clone()));
    data.insert("post".into(), Value::Object(post));
    data.extend(extra.clone());
    Ok(files
        .into_iter()
        .enumerate()
        .filter_map(|(index, url)| {
            let mut fields = data.clone();
            fields.insert("num".into(), (index + 1).into());
            fields.insert("url".into(), url.as_str().into());
            nameext_into(&url, &mut fields);
            Some(
                Item::new(Url::parse(&url).ok()?, fields)
                    .ids(post_id.clone(), (index + 1).to_string()),
            )
        })
        .collect())
}

fn extract<'a>(site: &'static Site, prefixed: bool, http: &'a Http, url: Url) -> Events<'a> {
    let Some((host, kind)) = target(&url, prefixed) else {
        return crate::sites::fail(error("unsupported Blogger URL"));
    };
    crate::sites::paginate(
        Some((None::<Map<String, Value>>, None::<String>)),
        move |state| {
            let host = host.clone();
            let kind = match &kind {
                Kind::Post(p) => Kind::Post(p.clone()),
                Kind::Blog => Kind::Blog,
                Kind::Search(q) => Kind::Search(q.clone()),
                Kind::Label(l) => Kind::Label(l.clone()),
            };
            async move {
                let Some((blog, token)) = state else {
                    return Ok(None);
                };
                let blog = match blog {
                    Some(blog) => blog,
                    None => {
                        let mut blog =
                            call(http, "/blogs/byurl", &[("url", &format!("http://{host}"))])
                                .await?
                                .as_object()
                                .cloned()
                                .ok_or_else(|| error("Blogger returned no blog"))?;
                        let pages = blog["pages"]["totalItems"].clone();
                        let posts = blog["posts"]["totalItems"].clone();
                        blog.insert("pages".into(), pages);
                        blog.insert("posts".into(), posts);
                        let published = nested_date(parse_iso(
                            blog.get("published")
                                .and_then(Value::as_str)
                                .unwrap_or_default(),
                        ));
                        blog.insert("date".into(), published);
                        blog.remove("selfLink");
                        blog
                    }
                };
                let blog_id = crate::sites::scalar(&blog["id"]);
                let mut extra = Map::new();
                let (route, endpoint, mut params): (&'static Route, String, Vec<(&str, String)>) =
                    match &kind {
                        Kind::Post(path) => {
                            let post = call(
                                http,
                                &format!("/blogs/{blog_id}/posts/bypath"),
                                &[("path", path)],
                            )
                            .await?;
                            let items = post_items(http, &host, &blog, &post, &extra).await?;
                            return Ok(Some((
                                crate::sites::events(crate::sites::post(site, http, &POST, items)),
                                None,
                            )));
                        }
                        Kind::Blog => (&BLOG, format!("/blogs/{blog_id}/posts"), Vec::new()),
                        Kind::Search(query) => {
                            extra.insert("query".into(), query.as_str().into());
                            (
                                &SEARCH,
                                format!("/blogs/{blog_id}/posts/search"),
                                vec![("q", query.clone())],
                            )
                        }
                        Kind::Label(label) => {
                            extra.insert("label".into(), label.as_str().into());
                            (
                                &LABEL,
                                format!("/blogs/{blog_id}/posts"),
                                vec![("labels", label.clone())],
                            )
                        }
                    };
                if let Some(token) = token {
                    params.push(("pageToken", token));
                }
                let pairs: Vec<(&str, &str)> =
                    params.iter().map(|(k, v)| (*k, v.as_str())).collect();
                let data = call(http, &endpoint, &pairs).await?;
                let mut events = Vec::new();
                for post in data["items"].as_array().into_iter().flatten() {
                    events.extend(crate::sites::post(
                        site,
                        http,
                        route,
                        post_items(http, &host, &blog, post, &extra).await?,
                    ));
                }
                let next = data
                    .get("nextPageToken")
                    .and_then(Value::as_str)
                    .map(|t| (Some(blog), Some(t.to_owned())));
                Ok(Some((crate::sites::events(events), next)))
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http as client, json, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls_and_originals() {
        let t = |raw: &str, prefixed| target(&Url::parse(raw).unwrap(), prefixed);
        assert_eq!(
            t(
                "https://julianbphotography.blogspot.com/2010/12/moon-rise.html",
                false
            ),
            Some((
                "julianbphotography.blogspot.com".into(),
                Kind::Post("/2010/12/moon-rise.html".into())
            ))
        );
        assert_eq!(
            t("https://julianbphotography.blogspot.com/", false),
            Some(("julianbphotography.blogspot.com".into(), Kind::Blog))
        );
        assert_eq!(
            t("https://x.blogspot.com/search/label/Cats%20Dogs", false),
            Some(("x.blogspot.com".into(), Kind::Label("Cats Dogs".into())))
        );
        assert_eq!(
            t("blogger:https://www.kefblog.com.ng/search?q=ab", true),
            Some(("www.kefblog.com.ng".into(), Kind::Search("ab".into())))
        );
        assert_eq!(
            original("http://1.bp.blogspot.com/-x/y/s400/a.jpg"),
            "https://1.bp.blogspot.com/-x/y/s0/a.jpg"
        );
        assert_eq!(
            original("https://blogger.googleusercontent.com/img/b/abc=w640-h480"),
            "https://blogger.googleusercontent.com/img/b/abc=s0"
        );
    }

    #[tokio::test]
    async fn post_by_path() {
        let blog = json!({"id": "1", "name": "Blog", "published": "2010-01-01T00:00:00Z", "pages": {"totalItems": 0}, "posts": {"totalItems": 9}, "selfLink": "x"});
        let post = json!({"id": "7", "title": "Moon", "published": "2010-12-02T03:04:05Z", "author": {"displayName": "Julian"}, "replies": {"totalItems": 2},
            "content": "<p>Hi</p><img src=\"https://1.bp.blogspot.com/-a/b/s1600/moon.jpg\">", "blog": {}, "selfLink": "y"});
        let (root, server) = serve(vec![json(blog), json(post)]);
        let http = client("blogspot", root);
        let files = media(extract(
            &BLOGSPOT,
            false,
            &http,
            Url::parse("https://jb.blogspot.com/2010/12/moon.html").unwrap(),
        ))
        .await;
        assert_eq!(
            files[0].url.as_str(),
            "https://1.bp.blogspot.com/-a/b/s0/moon.jpg"
        );
        let k = keywords(&files[0]);
        assert_eq!(
            (
                k["post"]["author"].as_str(),
                k["post"]["content"].as_str(),
                k["blog"]["posts"].as_i64()
            ),
            (Some("Julian"), Some("Hi"), Some(9))
        );
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "001.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("blogger/Blog/2010-12-02 Moon")
        );
        let requests = server.join().unwrap();
        assert!(
            requests[0]
                .starts_with("GET /blogger/v3/blogs/byurl?url=http%3A%2F%2Fjb.blogspot.com&key=")
        );
        assert!(requests[1].starts_with(
            "GET /blogger/v3/blogs/1/posts/bypath?path=%2F2010%2F12%2Fmoon.html&key="
        ));
    }
}
