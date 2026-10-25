//! Blogger blogs, mirroring gallery_dl/extractor/blogger.py through the
//! Blogger API v3: images in each post's HTML at their original size, and
//! with the `videos` option (default on) the blog's embedded videos.
//!
//! Upstream's built-in instance is `blogspot` (`*.blogspot.com`); custom
//! domains are configured as `extractor.blogger.<category>` instances or
//! given as `blogger:URL`. The `api-key` setting replaces upstream's public
//! key.
use crate::sites::art::{extract_iter, json_loads, parse_iso, str_of};
use crate::sites::{Events, Http, Item, Route, Site, remove_html};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

const FILENAME: &str = "{num:>03}.{extension}";
const DIRECTORY: &[&str] = &[
    "blogger",
    "{blog[name]}",
    "{post[date]:%Y-%m-%d} {post[title]}",
];
const ARCHIVE: &str = "{post[id]}_{num}";
static POST: Route = Route::new("post", FILENAME, DIRECTORY, ARCHIVE);
static BLOG: Route = Route::new("blog", FILENAME, DIRECTORY, ARCHIVE);
static SEARCH: Route = Route::new("search", FILENAME, DIRECTORY, ARCHIVE);
static LABEL: Route = Route::new("label", FILENAME, DIRECTORY, ARCHIVE);

pub static SITE: Site = Site {
    category: "blogspot",
    name: "Blogger",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&POST, &BLOG, &SEARCH, &LABEL],
    api_key: true,
    cookies: None,
};

/// Configured `extractor.blogger.<category>` instances (custom domains).
pub(crate) static FAMILY: crate::sites::instances::Family = crate::sites::instances::Family {
    basecategory: "blogger",
    builtin: &[&SITE],
    related: &[],
    template: &SITE,
    matches: |category, url| configured_target(category, url).is_some(),
    extract: |category, http, url| match crate::sites::instances::configured(&FAMILY, category) {
        Some(instance) => match configured_target(category, &url) {
            Some((host, target)) => run(&instance.site, http, host, target),
            None => crate::sites::fail(error("unsupported Blogger URL")),
        },
        None => crate::sites::fail(error("unsupported Blogger URL")),
    },
};

const API: &str = "https://www.googleapis.com/blogger/v3";
const API_KEY: &str = "AIzaSyCN9ax34oMMyM07g_M-5pjeDp_312eITK8";

#[derive(Debug, PartialEq)]
enum Target {
    Post(String),
    Blog,
    Search(String),
    Label(String),
}

/// The blog host (`[\w-]+\.blogspot\.com`) and the matching class.
fn target(url: &Url) -> Option<(String, Target)> {
    if !crate::sites::plain(url) {
        return None;
    }
    let host = url.host_str()?;
    let name = host.strip_suffix(".blogspot.com")?;
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return None;
    }
    let rest = &url[url::Position::BeforePath..url::Position::AfterQuery];
    Some((host.to_owned(), parse_path(rest, url.fragment().is_none())?))
}

/// [`target`] for a configured instance: the blog is upstream's
/// `root.rpartition("/")[2]`.
fn configured_target(category: &str, url: &Url) -> Option<(String, Target)> {
    let instance = crate::sites::instances::configured(&FAMILY, category)?;
    let rest = crate::sites::instances::below(url, instance.spec)?;
    let blog = instance.root.rsplit('/').next().unwrap_or_default();
    Some((
        blog.to_owned(),
        parse_path(&rest, url.fragment().is_none())?,
    ))
}

/// The class matching a raw `/path?query`.
fn parse_path(rest: &str, no_fragment: bool) -> Option<Target> {
    let (path, query) = match rest.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (rest, None),
    };
    let path = if path.is_empty() { "/" } else { path };
    let target = if let Some(post) = post_path(path) {
        Target::Post(post)
    } else if path == "/" && query.is_none() && no_fragment {
        Target::Blog
    } else if matches!(path, "/search" | "/search/") {
        let query = query?.strip_prefix("q=")?;
        let query = query.split('&').next().unwrap_or_default();
        if query.is_empty() {
            return None;
        }
        Target::Search(crate::sites::percent_decode(query))
    } else {
        let label = path
            .strip_prefix("/search/label/")?
            .split('/')
            .next()
            .filter(|label| !label.is_empty())?;
        Target::Label(crate::sites::percent_decode(label))
    };
    Some(target)
}

/// `/\d\d\d\d/\d\d/[^/?#]+\.html`, raw as upstream sees it.
fn post_path(path: &str) -> Option<String> {
    let rest = path.strip_prefix('/')?;
    let (year, rest) = rest.split_once('/')?;
    let (month, rest) = rest.split_once('/')?;
    if year.len() != 4
        || month.len() != 2
        || !year
            .bytes()
            .chain(month.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let name = rest.split('/').next().unwrap_or_default();
    let end = name.rfind(".html").filter(|end| *end > 0)?;
    Some(format!("/{year}/{month}/{}", &name[..end + 5]))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some((host, target)) = target(&url) else {
        return crate::sites::fail(error("unsupported Blogger URL"));
    };
    run(&SITE, http, host, target)
}

fn run<'a>(site: &'static Site, http: &'a Http, host: String, target: Target) -> Events<'a> {
    crate::sites::once(async move {
        let blog = blog_by_url(http, &host).await?;
        let id = str_of(&Value::Object(blog.clone()), "id").to_owned();
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
            return Err(error("Blogger blog response has no id"));
        }
        let mut meta = Map::new();
        let (route, endpoint, params): (_, _, Vec<(&str, String)>) = match target {
            Target::Post(path) => {
                let post = call(
                    http,
                    &format!("/blogs/{id}/posts/bypath"),
                    &[("path", path)],
                    None,
                )
                .await?;
                return post_events(site, http, &POST, host, blog, meta, post).await;
            }
            Target::Blog => (&BLOG, format!("/blogs/{id}/posts"), Vec::new()),
            Target::Search(query) => {
                meta.insert("query".into(), query.clone().into());
                (
                    &SEARCH,
                    format!("/blogs/{id}/posts/search"),
                    vec![("q", query)],
                )
            }
            Target::Label(label) => {
                meta.insert("label".into(), label.clone().into());
                (
                    &LABEL,
                    format!("/blogs/{id}/posts"),
                    vec![("labels", label)],
                )
            }
        };
        Ok(pagination(
            site, http, route, host, blog, meta, endpoint, params,
        ))
    })
}

/// One API request: the endpoint's parameters, `key`, then `pageToken`.
async fn call(
    http: &Http,
    endpoint: &str,
    params: &[(&str, String)],
    page: Option<&str>,
) -> Result<Value> {
    let mut url = http.api(&format!("{API}{endpoint}"))?;
    let key = http.api_key().filter(|k| !k.is_empty()).unwrap_or(API_KEY);
    {
        let mut query = url.query_pairs_mut();
        for (name, value) in params {
            query.append_pair(name, value);
        }
        query.append_pair("key", key);
        if let Some(page) = page {
            query.append_pair("pageToken", page);
        }
    }
    let body = http.json(&url).await?;
    body.is_object()
        .then_some(body)
        .ok_or_else(|| error("Blogger API response is not an object"))
}

/// `blog_by_url` with upstream's keyword adjustments.
async fn blog_by_url(http: &Http, host: &str) -> Result<Map<String, Value>> {
    let blog = call(
        http,
        "/blogs/byurl",
        &[("url", format!("http://{host}"))],
        None,
    )
    .await?;
    let mut blog = blog.as_object().cloned().unwrap_or_default();
    for key in ["pages", "posts"] {
        let total = blog
            .get(key)
            .and_then(|v| v.get("totalItems"))
            .cloned()
            .unwrap_or(Value::Null);
        blog.insert(key.into(), total);
    }
    let date = parse_iso(str_of(&Value::Object(blog.clone()), "published"));
    blog.insert("date".into(), crate::filter::source_date_json(date));
    blog.remove("selfLink");
    Ok(blog)
}

/// `_pagination` over `nextPageToken`, one API page at a time.
fn pagination<'a>(
    site: &'static Site,
    http: &'a Http,
    route: &'static Route,
    host: String,
    blog: Map<String, Value>,
    meta: Map<String, Value>,
    endpoint: String,
    params: Vec<(&'static str, String)>,
) -> Events<'a> {
    crate::sites::paginate(Some(None::<String>), move |state| {
        let (host, blog, meta) = (host.clone(), blog.clone(), meta.clone());
        let endpoint = endpoint.clone();
        let params = params.clone();
        async move {
            let Some(token) = state else {
                return Ok(None);
            };
            let data = call(http, &endpoint, &params, token.as_deref()).await?;
            let posts = match data.get("items") {
                Some(Value::Array(items)) => items.clone(),
                _ => Vec::new(),
            };
            let next = data
                .get("nextPageToken")
                .map(|token| Some(crate::sites::scalar(token)));
            let events = crate::sites::pages(futures_util::StreamExt::then(
                futures_util::stream::iter(posts),
                move |post| {
                    let (host, blog, meta) = (host.clone(), blog.clone(), meta.clone());
                    async move { post_events(site, http, route, host, blog, meta, post).await }
                },
            ));
            Ok(Some((events, next)))
        }
    })
}

/// `original`: rewrite size components (`/s1600/`, `=w400-h300`) to `s0`
/// and use HTTPS.
pub(crate) fn original(url: &str) -> String {
    fn size(token: &str) -> bool {
        let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        if let Some(rest) = token.strip_prefix('w')
            && let Some((w, h)) = rest.split_once("-h")
            && digits(w)
            && digits(h)
        {
            return true;
        }
        token.strip_prefix(['s', 'w']).is_some_and(digits)
    }
    let mut out = String::with_capacity(url.len());
    let mut rest = url;
    while let Some(index) = rest.find(['/', '=']) {
        out.push_str(&rest[..=index]);
        rest = &rest[index + 1..];
        let end = rest.find('/').unwrap_or(rest.len());
        if size(&rest[..end]) {
            out.push_str("s0");
            rest = &rest[end..];
        }
    }
    out.push_str(rest);
    out.replacen("http:", "https:", 1)
}

/// Image URLs in a post's HTML.
pub(crate) fn images(content: &str) -> Vec<String> {
    extract_iter(content, "src=\"", "\"")
        .into_iter()
        .filter(|src| {
            let Some(rest) = src
                .strip_prefix("https://")
                .or_else(|| src.strip_prefix("http://"))
            else {
                return false;
            };
            let Some((host, path)) = rest.split_once('/') else {
                return false;
            };
            if host == "blogger.googleusercontent.com" {
                return path.strip_prefix("img/").is_some_and(|p| !p.is_empty());
            }
            let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
            // `lh\d+(?:-\w+)?\.googleusercontent\.com` or `\d+\.bp\.blogspot\.com`.
            let known = if let Some(label) = host.strip_suffix(".googleusercontent.com") {
                let (lh, suffix) = match label.split_once('-') {
                    Some((lh, suffix)) => (lh, Some(suffix)),
                    None => (label, None),
                };
                lh.strip_prefix("lh").is_some_and(digits)
                    && suffix.is_none_or(|s| {
                        !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                    })
            } else {
                host.strip_suffix(".bp.blogspot.com").is_some_and(digits)
            };
            known && !path.is_empty()
        })
        .map(original)
        .collect()
}

/// `www.blogger.com/video.g?token=` player URLs in a post's feed HTML.
pub(crate) fn video_players(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (index, _) in html.match_indices("src=") {
        let Some(rest) = html[index + 4..]
            .strip_prefix('"')
            .or_else(|| html[index + 4..].strip_prefix('\''))
        else {
            continue;
        };
        let url = &rest[..rest.find(['"', '\'']).unwrap_or(rest.len())];
        let token = url
            .strip_prefix("https://www.blogger.com/video.g?token=")
            .or_else(|| url.strip_prefix("http://www.blogger.com/video.g?token="));
        if token.is_some_and(|t| !t.is_empty()) {
            out.push(url.to_owned());
        }
    }
    out
}

/// `_extract_videos`: each player's highest `format_id` stream.
async fn videos(http: &Http, host: &str, post_id: &str) -> Result<Vec<String>> {
    if post_id.is_empty() || !post_id.bytes().all(|b| b.is_ascii_digit()) {
        return Err(error("Blogger post has no id"));
    }
    let mut url = http.api(&format!("https://{host}/feeds/posts/default/{post_id}"))?;
    url.query_pairs_mut()
        .append_pair("alt", "json")
        .append_pair("v", "2")
        .append_pair("dynamicviews", "1")
        .append_pair("rewriteforssl", "true");
    let data = http.json(&url).await?;
    let html = data
        .pointer("/entry/content/$t")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut urls = Vec::new();
    for player in video_players(html) {
        let page = http.text(&http.api(&player)?).await?;
        let config = json_loads(
            crate::sites::art::extr(&page, "var VIDEO_CONFIG =", "\n"),
            "Blogger video",
        )?;
        // Python `max` keeps the first maximal element.
        let mut best: Option<&Value> = None;
        for stream in config
            .get("streams")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let id = stream.get("format_id").and_then(Value::as_f64);
            if best.is_none_or(|b| id > b.get("format_id").and_then(Value::as_f64)) {
                best = Some(stream);
            }
        }
        let best = best.ok_or_else(|| error("Blogger video has no streams"))?;
        urls.push(str_of(best, "play_url").to_owned());
    }
    Ok(urls)
}

async fn post_events<'a>(
    site: &'static Site,
    http: &'a Http,
    route: &'static Route,
    host: String,
    blog: Map<String, Value>,
    meta: Map<String, Value>,
    post: Value,
) -> Result<Events<'a>> {
    let mut post = post
        .as_object()
        .cloned()
        .ok_or_else(|| error("Blogger post is not an object"))?;
    let content = str_of(&Value::Object(post.clone()), "content").to_owned();
    let mut files = images(&content);
    if http.option_bool("videos", true)
        && (content.contains("id=\"BLOG_video-") || content.contains("class=\"BLOG_video_"))
    {
        let id = post.get("id").map(crate::sites::scalar).unwrap_or_default();
        files.extend(videos(http, &host, &id).await?);
    }
    let items = parse_post(&mut post, &content, &blog, &meta, files);
    Ok(crate::sites::events(crate::sites::post(
        site, http, route, items,
    )))
}

/// The post's upstream keyword adjustments and one item per file.
pub(crate) fn parse_post(
    post: &mut Map<String, Value>,
    content: &str,
    blog: &Map<String, Value>,
    meta: &Map<String, Value>,
    files: Vec<String>,
) -> Vec<Item> {
    let author = post
        .get("author")
        .and_then(|a| a.get("displayName"))
        .cloned()
        .unwrap_or(Value::Null);
    post.insert("author".into(), author);
    let replies = post
        .get("replies")
        .and_then(|r| r.get("totalItems"))
        .cloned()
        .unwrap_or(Value::Null);
    post.insert("replies".into(), replies);
    post.insert("content".into(), remove_html(content).into());
    let date = parse_iso(str_of(&Value::Object(post.clone()), "published"));
    post.insert("date".into(), crate::filter::source_date_json(date));
    post.remove("selfLink");
    post.remove("blog");

    let id = post.get("id").map(crate::sites::scalar).unwrap_or_default();
    let author = post
        .get("author")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let title = post.get("title").and_then(Value::as_str).map(str::to_owned);
    let mut data = Map::new();
    data.insert("blog".into(), blog.clone().into());
    data.insert("post".into(), post.clone().into());
    data.extend(meta.clone());
    files
        .into_iter()
        .enumerate()
        .filter_map(|(index, raw)| {
            let url = crate::sites::media::web_url(&raw)?;
            let mut keywords = data.clone();
            keywords.insert("num".into(), (index + 1).into());
            keywords.insert("url".into(), raw.clone().into());
            crate::sites::art::nameext_into(&raw, &mut keywords);
            Some(
                Item::new(url, keywords)
                    .ids(id.clone(), (index + 1).to_string())
                    .user(author.clone())
                    .caption(title.clone())
                    .timestamp(date),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, json, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap()).map(|(_, target)| target);
        assert_eq!(
            t("https://julianbphotography.blogspot.com/2010/12/moon-rise.html"),
            Some(Target::Post("/2010/12/moon-rise.html".into()))
        );
        assert_eq!(
            t("http://my_blog-1.blogspot.com/2010/12/a%20b.html?m=1#c"),
            Some(Target::Post("/2010/12/a%20b.html".into()))
        );
        assert_eq!(
            t("https://b.blogspot.com/2010/12/a.htmlx"),
            Some(Target::Post("/2010/12/a.html".into()))
        );
        assert_eq!(t("https://b.blogspot.com/"), Some(Target::Blog));
        assert_eq!(t("https://b.blogspot.com"), Some(Target::Blog));
        assert_eq!(
            t("https://b.blogspot.com/search?q=400%20s&max-results=2"),
            Some(Target::Search("400 s".into()))
        );
        assert_eq!(
            t("https://b.blogspot.com/search/?q=x"),
            Some(Target::Search("x".into()))
        );
        assert_eq!(
            t("https://b.blogspot.com/search/label/D%26D/x"),
            Some(Target::Label("D&D".into()))
        );
        for raw in [
            "https://blogspot.com/",
            "https://a.b.blogspot.com/",
            "https://b.blogspot.com/?x=1",
            "https://b.blogspot.com/p/about.html",
            "https://b.blogspot.com/2010/1/a.html",
            "https://b.blogspot.com/2010/12/.html",
            "https://b.blogspot.com/search?x=1&q=a",
            "https://b.blogspot.com/search/label/",
            "https://b.blogspot.com.evil.test/",
            "https://b.blogspot.com:8443/",
            "https://u:p@b.blogspot.com/",
            "ftp://b.blogspot.com/",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[test]
    fn rewrites_images_to_original_size() {
        assert_eq!(
            original("http://1.bp.blogspot.com/-x/s1600/img.jpg"),
            "https://1.bp.blogspot.com/-x/s0/img.jpg"
        );
        assert_eq!(
            original("https://blogger.googleusercontent.com/img/b/R29v/w640-h480/a.png"),
            "https://blogger.googleusercontent.com/img/b/R29v/s0/a.png"
        );
        assert_eq!(
            original("https://lh3.googleusercontent.com/abc=w400"),
            "https://lh3.googleusercontent.com/abc=s0"
        );
        assert_eq!(
            original("https://lh3.googleusercontent.com/s1600x/a=s1=w2"),
            "https://lh3.googleusercontent.com/s1600x/a=s1=s0"
        );
        let content = r#"<img src="https://blogger.googleusercontent.com/img/a/s320/x.jpg">
<img src="https://lh5-abc.googleusercontent.com/p/w100-h50/y.png"><img src="https://evil.test/s1/z.jpg">
<img src="https://blogger.googleusercontent.com/imgx/a.jpg"><img src="http://12.bp.blogspot.com/k/s72/z.gif">"#;
        assert_eq!(
            images(content),
            vec![
                "https://blogger.googleusercontent.com/img/a/s0/x.jpg",
                "https://lh5-abc.googleusercontent.com/p/s0/y.png",
                "https://12.bp.blogspot.com/k/s0/z.gif",
            ]
        );
        assert_eq!(
            video_players(
                r#"<iframe src='https://www.blogger.com/video.g?token=AD6v5d'></iframe><img src="https://evil.test/video.g?token=x">"#
            ),
            vec!["https://www.blogger.com/video.g?token=AD6v5d"]
        );
    }

    #[tokio::test]
    async fn configured_instance_uses_its_domain() {
        crate::sites::instances::define(
            "blogger",
            "blogger-configured",
            json!({"root": "https://blog.configured.test", "pattern": r"(?:www\.)?blog\.configured\.test"}),
        );
        let url = Url::parse("https://www.blog.configured.test/search/label/Cats").unwrap();
        let site = crate::sites::find(&url).unwrap();
        assert_eq!(site.category, "blogger-configured");
        assert!(site.api_key);
        assert_eq!(
            configured_target(
                "blogger-configured",
                &Url::parse("https://blog.configured.test/search/label/Cats").unwrap()
            ),
            Some(("blog.configured.test".into(), Target::Label("Cats".into())))
        );
        let (root, server) = serve(vec![json(blog()), json(json!({"items": []}))]);
        let http = http("blogger-configured", root);
        assert!(media((site.extract)(&http, url)).await.is_empty());
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with(
                "GET /blogger/v3/blogs/byurl?url=http%3A%2F%2Fblog.configured.test&key="
            ),
            "{}",
            requests[0]
        );
        assert!(requests[1].starts_with("GET /blogger/v3/blogs/123/posts?labels=Cats&"));
    }

    fn blog() -> Value {
        json!({"id": "123", "name": "Blog Name", "published": "2010-11-21T10:19:42-08:00",
            "selfLink": "x", "pages": {"totalItems": 2}, "posts": {"totalItems": 5}})
    }

    fn post(id: &str, content: &str) -> Value {
        json!({"id": id, "title": "Moon Rise", "published": "2010-12-14T00:12:00-08:00",
            "selfLink": "x", "blog": {"id": "123"}, "content": content,
            "author": {"displayName": "Julian"}, "replies": {"totalItems": "3"}})
    }

    #[tokio::test]
    async fn extracts_post_with_video() {
        let content = r#"<p>Hi</p><img src="https://blogger.googleusercontent.com/img/b/s400/moon.jpg"><div class="BLOG_video_class"></div>"#;
        let (root, server) = serve(vec![
            json(blog()),
            json(post("456", content)),
            json(json!({"entry": {"content": {"$t":
                "<iframe src=\"https://www.blogger.com/video.g?token=TOK\"></iframe>"}}})),
            html(
                "var VIDEO_CONFIG = {\"streams\": [{\"format_id\": 18, \"play_url\": \"https://r.googlevideo.com/low\"}, {\"format_id\": 22, \"play_url\": \"https://r.googlevideo.com/videoplayback?id=1\"}]}\n",
            ),
        ]);
        let http = http("blogspot", root);
        let files = media(extract(
            &http,
            Url::parse("https://julian.blogspot.com/2010/12/moon-rise.html").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].url.as_str(),
            "https://blogger.googleusercontent.com/img/b/s0/moon.jpg"
        );
        assert_eq!(
            files[1].url.as_str(),
            "https://r.googlevideo.com/videoplayback?id=1"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["blog"]["pages"], 2);
        assert_eq!(k["blog"]["posts"], 5);
        assert!(k["blog"].get("selfLink").is_none());
        assert_eq!(k["post"]["author"], "Julian");
        assert_eq!(k["post"]["replies"], "3");
        assert_eq!(k["post"]["content"], "Hi");
        assert!(k["post"].get("blog").is_none());
        assert_eq!(k["num"], 1);
        assert_eq!(k["filename"], "moon");
        assert_eq!(k["subcategory"], "post");
        assert_eq!(keywords(&files[1])["filename"], "videoplayback");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "001.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("blogger/Blog Name/2010-12-14 Moon Rise")
        );
        let (namespace, identity) = crate::sites::archive_key(&files[1]).unwrap();
        assert_eq!(namespace, "blogspot");
        assert_eq!(identity, r#"["{post[id]}_{num}",["456","2"]]"#);
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with(&format!(
                "GET /blogger/v3/blogs/byurl?url=http%3A%2F%2Fjulian.blogspot.com&key={API_KEY} "
            )),
            "{}",
            requests[0]
        );
        assert!(requests[1].starts_with(&format!(
            "GET /blogger/v3/blogs/123/posts/bypath?path=%2F2010%2F12%2Fmoon-rise.html&key={API_KEY} "
        )));
        assert!(requests[2].starts_with(
            "GET /feeds/posts/default/456?alt=json&v=2&dynamicviews=1&rewriteforssl=true "
        ));
        assert!(requests[3].starts_with("GET /video.g?token=TOK "));
    }

    #[tokio::test]
    async fn label_paginates_with_page_tokens() {
        let image = |n: u32| format!(r#"<img src="https://1.bp.blogspot.com/a/s1600/{n}.jpg">"#);
        let (root, server) = serve(vec![
            json(blog()),
            json(json!({"items": [post("1", &image(1))], "nextPageToken": "NEXT"})),
            json(json!({"items": [post("2", &image(2))]})),
        ]);
        let http = http("blogspot", root).with_api_key(Some("MYKEY"));
        let files = media(extract(
            &http,
            Url::parse("https://b.blogspot.com/search/label/Art%20Work").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[1]);
        assert_eq!(k["label"], "Art Work");
        assert_eq!(k["post"]["id"], "2");
        assert_eq!(k["subcategory"], "label");
        let requests = server.join().unwrap();
        assert!(requests[0].contains("&key=MYKEY "));
        assert!(
            requests[1].starts_with("GET /blogger/v3/blogs/123/posts?labels=Art+Work&key=MYKEY ")
        );
        assert!(requests[2].starts_with(
            "GET /blogger/v3/blogs/123/posts?labels=Art+Work&key=MYKEY&pageToken=NEXT "
        ));
    }
}
