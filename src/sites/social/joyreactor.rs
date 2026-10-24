//! JoyReactor posts, tags, users and searches, mirroring
//! gallery_dl/extractor/joyreactor.py: the HTML pages give the files, and
//! with the `metadata` option the GraphQL API adds post details.
//!
//! Options: `embeds` (default off; queued to yt-dlp), `videos` (default
//! on), `format` and `metadata`. Upstream's three to six second request
//! interval is not applied.
use crate::sites::art::{Scan, extr, extract_iter, parse_int, split_html, strptime};
use crate::sites::media::iso_ts;
use crate::sites::{Events, Http, Item, Route, Site, unescape};
use crate::{Result, error};
use base64::Engine;
use serde_json::{Map, Value, json};
use url::Url;

const FILENAME: &str = "{post_id}_{num:>02}_{file_id}{title:?_//[b:200]}.{extension}";
const ARCHIVE: &str = "{post_id}_{num}";
const TAGS_ARCHIVE: &str = "{search_tags}_{post_id}_{num}";
static POST: Route = Route::new("post", FILENAME, &["{category}"], ARCHIVE);
static TAG: Route = Route::new(
    "tag",
    FILENAME,
    &["{category}", "{search_tags}"],
    TAGS_ARCHIVE,
);
static USER: Route = Route::new("user", FILENAME, &["{category}", "user", "{user}"], ARCHIVE);
static SEARCH: Route = Route::new(
    "search",
    FILENAME,
    &["{category}", "{search_tags}"],
    TAGS_ARCHIVE,
);

pub static SITE: Site = Site {
    category: "joyreactor",
    name: "JoyReactor",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&POST, &TAG, &USER, &SEARCH],
    api_key: false,
    cookies: None,
};

const API: &str = "https://api.joyreactor.com/graphql";
const POST_QUERY: &str = include_str!("joyreactor/IdPostPageQuery.graphql");
const TAG_QUERY: &str = include_str!("joyreactor/TagPageQuery.graphql");
const USER_QUERY: &str = include_str!("joyreactor/UserProfilePageQuery.graphql");
const SEARCH_QUERY: &str = include_str!("joyreactor/SearchPageQuery.graphql");

#[derive(Debug, PartialEq)]
enum Target {
    Post(String),
    /// The listing route, its path and the tag, user or query.
    Listing(&'static str, String, String),
}

/// `joyreactor\.c(om|c)` with `/post/(\d+)` or
/// `(/(tag|user|search)/([^/?#]+)(?:/[^/?#]+)?)`; also returns the
/// `com`/`cc` suffix.
fn target(url: &Url) -> Option<(String, Target)> {
    if !crate::sites::plain(url) {
        return None;
    }
    let tld = match url.host_str()? {
        "joyreactor.com" => "om",
        "joyreactor.cc" => "c",
        _ => return None,
    };
    let path = url.path();
    if let Some(rest) = path.strip_prefix("/post/") {
        let id = crate::sites::art::leading_digits(rest)?;
        return Some((tld.into(), Target::Post(id.into())));
    }
    let parts: Vec<&str> = path.split('/').collect();
    let (kind, name) = match parts.as_slice() {
        ["", "tag", name, ..] if !name.is_empty() => ("tag", *name),
        ["", "user", name, ..] if !name.is_empty() => ("user", *name),
        ["", "search", name, ..] if !name.is_empty() => ("search", *name),
        _ => return None,
    };
    let mut listing = format!("/{kind}/{name}");
    if let Some(extra) = parts.get(3).filter(|p| !p.is_empty()) {
        listing = format!("{listing}/{extra}");
    }
    Some((tld.into(), Target::Listing(kind, listing, name.into())))
}

struct Options {
    embeds: bool,
    videos: bool,
    formats: Option<Vec<String>>,
    metadata: bool,
}

impl Options {
    fn new(http: &Http) -> Self {
        let formats = match http.option("format") {
            Some(Value::String(s)) => Some(s.split(',').map(str::to_owned).collect()),
            Some(Value::Array(list)) => Some(
                list.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect(),
            ),
            _ => None,
        };
        Self {
            embeds: http.option_bool("embeds", false),
            videos: http.option_bool("videos", true),
            formats,
            metadata: http.option_bool("metadata", false),
        }
    }
}

/// `_request_graphql`: the response's `data`.
async fn graphql(http: &Http, root: &str, query: &str, variables: &Value) -> Result<Value> {
    let referer = format!("{root}/");
    let headers = [("Referer", referer.as_str()), ("Origin", root)];
    let body = json!({"variables": variables, "query": query});
    let data = http.post_json(&http.api(API)?, &body, &headers).await?;
    data.get("data")
        .cloned()
        .filter(Value::is_object)
        .ok_or_else(|| error("JoyReactor API response has no data"))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some((tld, target)) = target(&url) else {
        return crate::sites::fail(error("unsupported JoyReactor URL"));
    };
    let root = format!("https://joyreactor.c{tld}");
    let options = std::sync::Arc::new(Options::new(http));
    let dfmt = if tld == "c" {
        "%d.%m.%y, %H:%M"
    } else {
        "%d/%m/%y, %H:%M"
    };
    match target {
        Target::Post(id) => crate::sites::once(async move {
            let page = http.text(&http.api(&format!("{root}/post/{id}"))?).await?;
            let html = extr(&page, "class=\"content\"", "class=\"comment\"").to_owned();
            let data = if options.metadata {
                let node = base64::engine::general_purpose::STANDARD.encode(format!("Post:{id}"));
                let data = graphql(
                    http,
                    &root,
                    POST_QUERY,
                    &json!({"id": node, "isAuthorised": false}),
                )
                .await?;
                data.get("node").cloned()
            } else {
                None
            };
            let items = parse_post(&html, data.as_ref(), &options, &root, dfmt, &Map::new());
            Ok(crate::sites::events(crate::sites::post(
                &SITE, http, &POST, items,
            )))
        }),
        Target::Listing(kind, path, name) => {
            let name = crate::sites::percent_decode(&name);
            let mut meta = Map::new();
            let (route, query, variables) = match kind {
                "tag" => {
                    meta.insert("search_tags".into(), name.clone().into());
                    let variables = json!({"name": name, "page": null, "lineType": "GOOD",
                        "isAuthorised": false, "isHomepage": false});
                    (&TAG, TAG_QUERY, variables)
                }
                "user" => {
                    let variables = json!({"username": name, "page": null, "lineType": "GOOD",
                        "isAuthorised": false, "isHomepage": false});
                    (&USER, USER_QUERY, variables)
                }
                _ => {
                    meta.insert("search_tags".into(), name.clone().into());
                    let variables = json!({"query": name, "page": null, "tagNames": [],
                        "username": null, "isAuthorised": false, "showUnsafe": null,
                        "showNsfw": true, "showOnlyNsfw": false, "minRating": null,
                        "maxRating": null, "sortByDate": false, "sortByRating": false});
                    (&SEARCH, SEARCH_QUERY, variables)
                }
            };
            pagination(
                http,
                Listing {
                    route,
                    query,
                    root,
                    dfmt,
                    meta,
                    options,
                },
                path,
                variables,
            )
        }
    }
}

struct Listing {
    route: &'static Route,
    query: &'static str,
    root: String,
    dfmt: &'static str,
    meta: Map<String, Value>,
    options: std::sync::Arc<Options>,
}

/// `_pagination`: a page's posts split on `class="content"`; it continues
/// only while the next link ends in `/1/rev`, as upstream does.
fn pagination(http: &Http, listing: Listing, path: String, variables: Value) -> Events<'_> {
    let listing = std::sync::Arc::new(listing);
    let first = format!("{}{path}", listing.root);
    crate::sites::paginate(Some((first, true, variables)), move |state| {
        let listing = listing.clone();
        async move {
            let Some((url, first, mut variables)) = state else {
                return Ok(None);
            };
            let html = http.text(&http.api(&url)?).await?;
            let mut posts: Vec<&str> = html.split("class=\"content\"").skip(1).collect();
            if first
                && posts.first().is_some_and(|post| {
                    post.contains("class=\"expand-wrapper relative\"><div class=\"absolute")
                })
            {
                posts.remove(0);
            }
            let pgn = extr(&html, "ant-pagination-next", "</li>");
            let path = extr(pgn, "href=\"", "\"");
            let data_posts: Vec<Option<Value>> = if listing.options.metadata {
                variables["page"] = if path.is_empty() {
                    Value::Null
                } else {
                    (parse_int(path.rsplit('/').next().unwrap_or_default(), 0) + 1).into()
                };
                let data = graphql(http, &listing.root, listing.query, &variables).await?;
                let posts = data
                    .as_object()
                    .and_then(|map| map.values().last())
                    .and_then(|value| value.pointer("/postPager/posts"))
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                // `zip` stops at the shorter list.
                posts.into_iter().map(Some).collect()
            } else {
                posts.iter().map(|_| None).collect()
            };
            let mut events = Vec::new();
            for (html, data) in posts.iter().zip(data_posts) {
                let items = parse_post(
                    html,
                    data.as_ref(),
                    &listing.options,
                    &listing.root,
                    listing.dfmt,
                    &listing.meta,
                );
                events.extend(crate::sites::post(&SITE, http, listing.route, items));
            }
            let next = (!path.is_empty() && path.ends_with("/1/rev"))
                .then(|| (format!("{}{path}", listing.root), false, variables));
            Ok(Some((crate::sites::events(events), next)))
        }
    })
}

/// `_extract_files`: images at full size, videos per `format`, and
/// (with `embeds`) foreign iframes as yt-dlp pages.
fn extract_files(html: &str, options: &Options) -> Vec<Map<String, Value>> {
    let mut files = Vec::new();
    let mut last = "";
    for url in extract_iter(html, "src=\"", "\"") {
        if url.contains("/avatar/") || last == url {
            continue;
        }
        last = url;
        let mut file = Map::new();
        if !url.contains(".joyreactor.c") {
            if !options.embeds {
                continue;
            }
            file.insert("file_url".into(), format!("ytdl:{url}").into());
            file.insert("type".into(), "embed".into());
            files.push(file);
        } else if url.contains("/webm/") {
            if !options.videos {
                continue;
            }
            match &options.formats {
                None => {
                    file.insert("file_url".into(), url.into());
                    file.insert("type".into(), "video".into());
                    file.insert("format".into(), "webm".into());
                    files.push(file);
                }
                Some(formats) => {
                    let (lhs, rhs) = url.rsplit_once("/webm/").unwrap_or(("", url));
                    let stem = rhs
                        .find('.')
                        .map_or(&rhs[..rhs.len().saturating_sub(1)], |i| &rhs[..i]);
                    for fmt in formats {
                        let mut file = Map::new();
                        file.insert(
                            "file_url".into(),
                            format!("{lhs}/{fmt}/{stem}.{fmt}").into(),
                        );
                        file.insert("type".into(), "video".into());
                        file.insert("format".into(), fmt.clone().into());
                        files.push(file);
                    }
                }
            }
        } else {
            let (lhs, rhs) = url.rsplit_once('/').unwrap_or(("", url));
            file.insert("file_url".into(), format!("{lhs}/full/{rhs}").into());
            file.insert("type".into(), "image".into());
            files.push(file);
        }
    }
    files
}

/// `items()` for one post's HTML and optional GraphQL data.
fn parse_post(
    html: &str,
    data: Option<&Value>,
    options: &Options,
    root: &str,
    dfmt: &str,
    meta: &Map<String, Value>,
) -> Vec<Item> {
    let files = extract_files(html, options);
    let mut scan = Scan::new(html);
    let mut post = meta.clone();
    post.insert("count".into(), files.len().into());
    post.insert("user".into(), unescape(scan.extr(" alt=\"", "\"")).into());
    post.insert("user_id".into(), scan.extr("/avatar/user/", "\"").into());
    let tags: Vec<Value> = split_html(scan.extr("class=\"post-tags", "</div>"))
        .into_iter()
        .skip(1)
        .map(Value::from)
        .collect();
    post.insert("tags".into(), tags.into());
    post.insert(
        "content".into(),
        scan.extr("class=\"post-content\">", "<div class=\"post-footer\">")
            .into(),
    );
    let date = scan.extr("</span></button>", "</div>");
    let post_id = scan.extr("\"/post/", "\"").to_owned();
    post.insert("post_id".into(), post_id.clone().into());
    let ts = match data {
        None => strptime(date.rsplit('>').next().unwrap_or_default(), dfmt),
        Some(data) => {
            for (key, name) in [
                ("nsfw", "nsfw"),
                ("unsafe", "unsafe"),
                ("banned", "banned"),
                ("comments", "commentsCount"),
                ("rating", "rating"),
            ] {
                post.insert(key.into(), data.get(name).cloned().unwrap_or(Value::Null));
            }
            if let Some(seo) = data.get("seoAttributes").filter(|s| s.is_object()) {
                post.insert(
                    "title".into(),
                    seo.get("title").cloned().unwrap_or(Value::Null),
                );
            }
            iso_ts(
                data.get("createdAt")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            )
        }
    };
    post.insert("date".into(), crate::filter::source_date_json(ts));
    let user = post.get("user").and_then(Value::as_str).map(str::to_owned);
    let referer = format!("{root}/");
    let mut items = Vec::new();
    for (index, file) in files.into_iter().enumerate() {
        let raw = file
            .get("file_url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let mut fields = post.clone();
        fields.insert("num".into(), (index + 1).into());
        let page = raw.strip_prefix("ytdl:");
        crate::sites::art::nameext_into(page.unwrap_or(&raw), &mut fields);
        crate::sites::art::update(&mut fields, &file);
        let filename = fields
            .get("filename")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let file_id = filename.rsplit('-').next().unwrap_or_default().to_owned();
        fields.insert("file_id".into(), file_id.into());
        let item = match page {
            Some(page) => super::ytdl_item(page, fields),
            None => crate::sites::art::file_url(&raw, root).map(|url| Item::new(url, fields)),
        };
        if let Some(item) = item {
            items.push(
                item.ids(post_id.clone(), (index + 1).to_string())
                    .user(user.clone())
                    .timestamp(ts)
                    .header("Referer", &referer),
            );
        }
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, json, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://joyreactor.cc/post/3721876"),
            Some(("c".into(), Target::Post("3721876".into())))
        );
        assert_eq!(
            t("http://joyreactor.com/tag/Cat%20Art/best/x"),
            Some((
                "om".into(),
                Target::Listing("tag", "/tag/Cat%20Art/best".into(), "Cat%20Art".into())
            ))
        );
        assert_eq!(
            t("https://joyreactor.cc/search/cats").map(|t| t.1),
            Some(Target::Listing(
                "search",
                "/search/cats".into(),
                "cats".into()
            ))
        );
        for raw in [
            "https://www.joyreactor.cc/post/1",
            "https://anime.joyreactor.cc/post/1",
            "https://joyreactor.cc/post/x",
            "https://joyreactor.cc/tag/",
            "https://joyreactor.cc.evil/post/1",
            "https://joyreactor.cc:81/post/1",
            "https://u@joyreactor.cc/post/1",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn post_html(id: u64) -> String {
        format!(
            r#"class="content"><img src="https://img10.joyreactor.cc/pics/avatar/user/77" alt="Bob &amp; co">
            <a href="/avatar/user/77">x</a>
            <div class="post-tags"><a>Cat</a><a>Art</a></div>
            <div class="post-content"><img src="https://img10.joyreactor.cc/pics/post/Cat-Art-{id}1.jpeg">
            <img src="https://img10.joyreactor.cc/pics/post/Cat-Art-{id}1.jpeg">
            <video src="https://img10.joyreactor.cc/pics/post/webm/Cat-{id}2.webm"></video>
            <iframe src="https://www.youtube.com/embed/xyz"></iframe>
            <div class="post-footer"><span>x</span></button><span>05.06.24, 07:08</div>
            <a href="/post/{id}">link</a>"#
        )
    }

    #[tokio::test]
    async fn extracts_post() {
        let page = format!("{}class=\"comment\"", post_html(5));
        let (root, server) = serve(vec![html(&page)]);
        let options = serde_json::json!({"embeds": true, "format": "mp4,webm"})
            .as_object()
            .cloned();
        let http = http("joyreactor", root).with_options(options.map(std::sync::Arc::new));
        let files = media(extract(
            &http,
            Url::parse("https://joyreactor.cc/post/5").unwrap(),
        ))
        .await;
        let urls: Vec<&str> = files.iter().map(|f| f.url.as_str()).collect();
        assert_eq!(
            urls,
            [
                "https://img10.joyreactor.cc/pics/post/full/Cat-Art-51.jpeg",
                "https://img10.joyreactor.cc/pics/post/mp4/Cat-52.mp4",
                "https://img10.joyreactor.cc/pics/post/webm/Cat-52.webm",
                "ytdl:https://www.youtube.com/embed/xyz",
            ]
        );
        let k = keywords(&files[0]);
        assert_eq!(k["user"], "Bob & co");
        assert_eq!(k["user_id"], "77");
        assert_eq!(k["tags"], serde_json::json!(["Cat", "Art"]));
        assert_eq!(k["post_id"], "5");
        assert_eq!(k["file_id"], "51");
        assert_eq!(k["count"], 4);
        assert_eq!(keywords(&files[1])["format"], "mp4");
        assert_eq!(files[0].headers["Referer"], "https://joyreactor.cc/");
        assert_eq!(
            files[0].metadata.as_ref().unwrap().timestamp,
            strptime("05.06.24, 07:08", "%d.%m.%y, %H:%M")
        );
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "5_01_51.jpeg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("joyreactor")
        );
        let (_, identity) = crate::sites::archive_key(&files[1]).unwrap();
        assert_eq!(identity, r#"["{post_id}_{num}",["5","2"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /post/5 "));
    }

    #[tokio::test]
    async fn tag_listing_with_metadata() {
        let page = format!(
            "<div>{}{}<li class=\"ant-pagination-next\"><a href=\"/tag/Cat/2\"></a></li>",
            post_html(1),
            post_html(2)
        );
        let data = serde_json::json!({"data": {"tag": {"postPager": {"posts": [
            {"createdAt": "2024-01-02T03:04:05Z", "nsfw": false, "rating": 1.5,
             "commentsCount": 3, "seoAttributes": {"title": "Seo"}},
            {"createdAt": "2024-01-03T03:04:05Z"}]}}}});
        let (root, server) = serve(vec![html(&page), json(data)]);
        let options = serde_json::json!({"metadata": true, "videos": false})
            .as_object()
            .cloned();
        let http = http("joyreactor", root).with_options(options.map(std::sync::Arc::new));
        let files = media(extract(
            &http,
            Url::parse("https://joyreactor.com/tag/Cat").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[0]);
        assert_eq!(k["title"], "Seo");
        assert_eq!(k["comments"], 3);
        assert_eq!(k["search_tags"], "Cat");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "1_01_11_Seo.jpeg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("joyreactor/Cat")
        );
        let (_, identity) = crate::sites::archive_key(&files[1]).unwrap();
        assert_eq!(
            identity,
            r#"["{search_tags}_{post_id}_{num}",["Cat","2","1"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /tag/Cat "));
        assert!(requests[1].starts_with("POST /graphql "));
        assert!(requests[1].contains("origin: https://joyreactor.com"));
        assert!(requests[1].contains(r#""page":3"#));
        assert!(requests[1].contains("query TagPageQuery"));
    }
}
