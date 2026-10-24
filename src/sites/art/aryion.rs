//! Eka's Portal (aryion.com) galleries, favorites, watch lists, tag and
//! text searches and single posts, mirroring gallery_dl/extractor/aryion.py.
//!
//! Each post is a `HEAD` request to `data.php` for its file name, size and
//! date, then its `view` page for the rest; folders found among posts are
//! expanded into queued posts. Upstream's `username`/`password` login is
//! not supported; a `phpbb3_rl7a3_sid` cookie from `--cookies` is used
//! instead and is required for the watch list.
use super::{Scan, extr, extract_iter, parse_int, parse_query, split_html};
use crate::sites::{Events, Http, Item, Route, Site, unescape};
use crate::{Result, error};
use futures_util::{StreamExt, TryStreamExt, stream};
use serde_json::{Map, Value};
use std::collections::{HashSet, VecDeque};
use url::Url;

const ROOT: &str = "https://aryion.com";
const FILENAME: &str = "{id} {title}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{user!l}", "{path:I}"];
static GALLERY: Route = Route::new("gallery", FILENAME, DIRECTORY, "{id}");
static FAVORITE: Route = Route::new(
    "favorite",
    FILENAME,
    &["{category}", "{user!l}", "favorites", "{folder}"],
    "f_{user}_{id}",
);
static WATCH: Route = Route::new("watch", FILENAME, &["{category}", "{user!l}"], "{id}");
static TAG: Route = Route::new(
    "tag",
    FILENAME,
    &["{category}", "tags", "{search_tags}"],
    "t_{search_tags}_{id}",
);
static SEARCH: Route = Route::new(
    "search",
    FILENAME,
    &[
        "{category}",
        "searches",
        "{search[prefix]}{search[q]|search[tags]|search[user]}",
    ],
    "s_{search[prefix]}{search[q]|search[tags]|search[user]}_{id}",
);
static POST: Route = Route::new("post", FILENAME, DIRECTORY, "{id}");

pub static SITE: Site = Site {
    category: "aryion",
    name: "Eka's Portal",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&GALLERY, &FAVORITE, &WATCH, &TAG, &SEARCH, &POST],
    api_key: false,
    cookies: Some("aryion.com"),
};

#[derive(Debug, PartialEq)]
enum Target {
    Gallery(String),
    Favorite(String, Option<String>),
    Watch,
    Tag(String),
    Search(String),
    Post(String),
}

/// `aryion\.com/g4` followed by one extractor's path, raw as upstream
/// sees it.
fn target(url: &Url) -> Option<Target> {
    if !super::on_host(url, &["aryion.com"]) {
        return None;
    }
    let path = url.path().strip_prefix("/g4/")?;
    let query = url.query().filter(|q| !q.is_empty());
    let name = |text: &str| {
        let name = text.split(['/', '?', '#']).next().unwrap_or_default();
        (!name.is_empty()).then(|| name.to_owned())
    };
    if let Some(rest) = path
        .strip_prefix("gallery/")
        .or_else(|| path.strip_prefix("user/"))
    {
        return name(rest).map(Target::Gallery);
    }
    if path == "latest.php" {
        return name(query?.strip_prefix("name=")?).map(Target::Gallery);
    }
    if let Some(rest) = path.strip_prefix("favorites/") {
        let user = name(rest)?;
        let folder = rest[user.len()..]
            .strip_prefix('/')
            .filter(|f| !f.is_empty())
            .map(str::to_owned);
        return Some(Target::Favorite(user, folder));
    }
    if path.starts_with("messagepage.php") {
        return Some(Target::Watch);
    }
    match path {
        "tags.php" => return query.map(|q| Target::Tag(q.to_owned())),
        "search.php" => return query.map(|q| Target::Search(q.to_owned())),
        _ => {}
    }
    let id = super::leading_digits(path.strip_prefix("view/")?)?;
    Some(Target::Post(id.to_owned()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Eka's Portal URL"));
    };
    let recursive = http.option_bool("recursive", true);
    match target {
        Target::Gallery(user) if recursive => {
            let ids = params_ids(
                http,
                format!("{ROOT}/g4/gallery/{user}"),
                Vec::new(),
                "class='gallery-item' id='",
                "'",
            );
            posts(http, &GALLERY, ids, Some(user), Map::new(), true)
        }
        Target::Gallery(user) => {
            let ids = next_ids(http, format!("{ROOT}/g4/latest.php?name={user}"));
            posts(http, &GALLERY, ids, Some(user), Map::new(), false)
        }
        Target::Favorite(user, folder) => favorites(http, user, folder, recursive),
        Target::Watch => crate::sites::once(async move {
            let root = http.api(ROOT)?;
            if http.cookie(&root, "phpbb3_rl7a3_sid").is_none() {
                return Err(error(
                    "Eka's Portal watched submissions need a 'phpbb3_rl7a3_sid' session cookie (--cookies)",
                ));
            }
            http.set_cookie(&root, "g4p_msgpage_style=plain; Path=/");
            let ids = params_ids(
                http,
                format!("{ROOT}/g4/messagepage.php"),
                Vec::new(),
                "data-item-id=\"",
                "\"",
            );
            Ok(posts(
                http,
                &WATCH,
                ids,
                Some(String::new()),
                Map::new(),
                true,
            ))
        }),
        Target::Tag(query) => {
            let params = parse_query(&query);
            let mut data = Map::new();
            let tag = params.iter().find(|(name, _)| name == "tag");
            data.insert(
                "search_tags".into(),
                tag.map_or(Value::Null, |(_, value)| value.as_str().into()),
            );
            let ids = params_ids(
                http,
                format!("{ROOT}/g4/tags.php"),
                params,
                "class='gallery-item' id='",
                "'",
            );
            posts(http, &TAG, ids, None, data, true)
        }
        Target::Search(query) => {
            let params = parse_query(&query);
            let get = |name: &str| {
                params
                    .iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, v)| v.as_str())
                    .filter(|v| !v.is_empty())
            };
            let prefix = if get("q").is_some() {
                ""
            } else if get("tags").is_some() {
                "t_"
            } else if get("user").is_some() {
                "u_"
            } else {
                ""
            };
            let mut search: Map<String, Value> = params
                .iter()
                .map(|(name, value)| (name.clone(), Value::from(value.as_str())))
                .collect();
            search.insert("prefix".into(), prefix.into());
            let mut data = Map::new();
            data.insert("search".into(), search.into());
            let ids = next_ids(http, format!("{ROOT}/g4/search.php?{query}"));
            // Upstream keeps the query string as `user`.
            posts(http, &SEARCH, ids, Some(query), data, true)
        }
        Target::Post(id) => posts(
            http,
            &POST,
            stream::iter([Ok(PostId::new(id))]).boxed(),
            None,
            Map::new(),
            true,
        ),
    }
}

/// A post ID and the `folder` keyword in effect when it was listed.
struct PostId {
    id: String,
    folder: Option<String>,
}

impl PostId {
    fn new(id: String) -> Self {
        Self { id, folder: None }
    }
}

type Ids<'a> = futures_util::stream::BoxStream<'a, Result<PostId>>;

/// Upstream `items`: each post becomes a file; folders are expanded into
/// queued posts when `recursive`.
fn posts<'a>(
    http: &'a Http,
    route: &'static Route,
    ids: Ids<'a>,
    user: Option<String>,
    data: Map<String, Value>,
    recursive: bool,
) -> Events<'a> {
    ids.map(move |id| {
        let (user, data) = (user.clone(), data.clone());
        crate::sites::once(async move {
            let PostId { id, folder } = id?;
            let mut item = match parse_post(http, &id, user.as_deref()).await? {
                Some(Parsed::File(item)) => *item,
                Some(Parsed::Folder) if recursive => return Ok(folder_posts(http, route, &id)),
                Some(Parsed::Folder) | None => return Ok(crate::sites::events(Vec::new())),
            };
            super::update(&mut item.keywords, &data);
            if let Some(folder) = folder {
                item.keywords.insert("folder".into(), folder.into());
            }
            Ok(crate::sites::events(crate::sites::post(
                &SITE,
                http,
                route,
                vec![item],
            )))
        })
    })
    .flatten()
    .boxed()
}

/// Queue the posts listed by a folder's view page.
fn folder_posts<'a>(http: &'a Http, route: &'static Route, id: &str) -> Events<'a> {
    let ids = params_ids(
        http,
        format!("{ROOT}/g4/view/{id}"),
        Vec::new(),
        "class='gallery-item' id='",
        "'",
    );
    ids.map(move |id| match id {
        Ok(id) => match Url::parse(&format!("{ROOT}/g4/view/{}", id.id)) {
            Ok(child) => crate::sites::queue(http, "aryion", route.subcategory, child, &Map::new()),
            Err(_) => crate::sites::events(Vec::new()),
        },
        Err(e) => crate::sites::fail(e),
    })
    .flatten()
    .boxed()
}

enum Parsed {
    File(Box<Item>),
    Folder,
}

fn missing(error: &crate::Error) -> bool {
    crate::sites::http::refused_status(error).is_some()
        || error.to_string().ends_with("resource not found")
}

/// Upstream `_parse_post`: `None` for unavailable posts.
async fn parse_post(http: &Http, id: &str, user: Option<&str>) -> Result<Option<Parsed>> {
    let file_url = format!("{ROOT}/g4/data.php?id={id}");
    let headers = match http.head(&http.api(&file_url)?, &[]).await {
        Ok(headers) => headers,
        Err(e) if missing(&e) => return Ok(None),
        Err(e) => return Err(e),
    };
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    };
    if matches!(
        header("content-type").as_str(),
        "application/x-folder"
            | "application/x-comic-folder"
            | "application/x-comic-folder-nomerge"
    ) {
        return Ok(Some(Parsed::Folder));
    }
    let page = http
        .text(&http.api(&format!("{ROOT}/g4/view/{id}"))?)
        .await?;
    let head = Head {
        disposition: header("content-disposition"),
        length: header("content-length"),
        modified: header("last-modified"),
    };
    Ok(parse_view(id, user, &head, &page).map(|item| Parsed::File(Box::new(item))))
}

/// The `data.php` response headers upstream reads.
pub(crate) struct Head {
    pub disposition: String,
    pub length: String,
    pub modified: String,
}

/// Upstream `_parse_post` keywords from the file headers and view page.
pub(crate) fn parse_view(id: &str, user: Option<&str>, head: &Head, page: &str) -> Option<Item> {
    let name = filename_from_contentdisposition(&head.disposition);
    let (fname, ext) = match name.rsplit_once('.') {
        Some((fname, ext)) if !fname.is_empty() => (fname.to_owned(), ext.to_owned()),
        Some((_, ext)) => (ext.to_owned(), String::new()),
        None => (name.clone(), String::new()),
    };
    let mut modified = head.modified.clone();
    if modified.len() >= 24 && modified.as_bytes()[22] != b':' && modified.is_char_boundary(24) {
        modified = format!("{}:{} GMT", &modified[..22], &modified[22..24]);
    }
    let timestamp = super::strptime(&modified, "%a, %d %b %Y %H:%M:%S GMT");
    let mut scan = Scan::new(page);
    let heading = unescape(scan.extr("<title>g4 :: ", "<"));
    let (title, artist) = heading.rsplit_once(" by ").unwrap_or(("", &heading));
    let user = user.filter(|u| !u.is_empty()).unwrap_or(artist).to_owned();
    let mut data = Map::new();
    data.insert("id".into(), parse_int(id, 0).into());
    let file_url = format!("{ROOT}/g4/data.php?id={id}");
    data.insert("url".into(), file_url.clone().into());
    data.insert("user".into(), user.clone().into());
    data.insert("title".into(), title.into());
    data.insert("artist".into(), artist.into());
    data.insert(
        "description".into(),
        unescape(scan.extr("property=\"og:description\" content=\"", "\"")).into(),
    );
    let crumbs = split_html(scan.extr("cookiecrumb'>", "</span"));
    let path: Vec<Value> = crumbs
        .get(4..crumbs.len().saturating_sub(1))
        .unwrap_or_default()
        .iter()
        .step_by(2)
        .map(|part| Value::from(part.as_str()))
        .collect();
    data.insert("path".into(), path.into());
    data.insert("date".into(), super::date(timestamp));
    data.insert("size".into(), parse_int(&head.length, 0).into());
    let views = scan.extr("Views</b>:", "<").replace(',', "");
    data.insert("views".into(), parse_int(&views, 0).into());
    data.insert(
        "width".into(),
        parse_int(scan.extr("Resolution</b>:", "x"), 0).into(),
    );
    data.insert("height".into(), parse_int(scan.extr("", "<"), 0).into());
    data.insert(
        "comments".into(),
        parse_int(scan.extr("Comments</b>:", "<"), 0).into(),
    );
    data.insert(
        "favorites".into(),
        parse_int(scan.extr("Favorites</b>:", "<"), 0).into(),
    );
    let tags: Vec<Value> = split_html(scan.extr("class='taglist'>", "</span>"))
        .into_iter()
        .map(Value::from)
        .collect();
    data.insert("tags".into(), tags.into());
    data.insert("filename".into(), fname.into());
    data.insert("extension".into(), ext.into());
    let url = Url::parse(&file_url).ok()?;
    Some(
        Item::new(url, data)
            .ids(id, id)
            .user(Some(user))
            .caption(Some(title.to_owned()))
            .timestamp(timestamp),
    )
}

/// gallery-dl `text.filename_from_contentdisposition`.
pub(crate) fn filename_from_contentdisposition(cd: &str) -> String {
    let value = |pos: usize| {
        let rest = &cd[pos..];
        match rest.strip_prefix('"') {
            Some(quoted) => quoted.split('"').next().unwrap_or_default(),
            None => rest.split(';').next().unwrap_or_default(),
        }
    };
    if let Some(pos) = cd.find("filename*=") {
        let mut parts = value(pos + 10).splitn(3, '\'');
        if let (Some(_charset), Some(_), Some(encoded)) = (parts.next(), parts.next(), parts.next())
        {
            return crate::sites::percent_decode(encoded);
        }
    }
    match cd.find("filename=") {
        Some(pos) => value(pos + 9).to_owned(),
        None => String::new(),
    }
}

/// gallery-dl `text.rextr` before `pos`.
fn rextr<'t>(text: &'t str, begin: &str, end: &str, pos: usize) -> &'t str {
    let Some(first) = text[..pos].rfind(begin).map(|i| i + begin.len()) else {
        return "";
    };
    text[first..]
        .find(end)
        .map_or("", |stop| &text[first..first + stop])
}

/// Upstream `_pagination_params`: `p`-numbered pages of `needle` IDs while
/// a page lists 40 or links `Next >>`.
fn params_ids<'a>(
    http: &'a Http,
    url: String,
    mut params: Vec<(String, String)>,
    needle: &'static str,
    quote: &'static str,
) -> Ids<'a> {
    let start = params
        .iter()
        .find(|(name, _)| name == "p")
        .map_or(1, |(_, p)| parse_int(p, 1));
    if !params.iter().any(|(name, _)| name == "p") {
        params.push(("p".into(), String::new()));
    }
    stream::try_unfold(Some(start), move |page| {
        let (url, mut params) = (url.clone(), params.clone());
        async move {
            let Some(page) = page else {
                return Ok(None);
            };
            for (name, value) in &mut params {
                if name == "p" {
                    *value = page.to_string();
                }
            }
            let mut request = http.api(&url)?;
            request.query_pairs_mut().extend_pairs(&params);
            let text = http.text(&request).await?;
            let ids: Vec<Result<PostId>> = extract_iter(&text, needle, quote)
                .into_iter()
                .map(|id| Ok(PostId::new(id.to_owned())))
                .collect();
            let next = (ids.len() >= 40 || text.contains(">Next &gt;&gt;<")).then_some(page + 1);
            Ok::<_, crate::Error>(Some((stream::iter(ids), next)))
        }
    })
    .try_flatten()
    .boxed()
}

/// Upstream `_pagination_next`: follow each page's `Next >>` link.
fn next_ids(http: &Http, url: String) -> Ids<'_> {
    stream::try_unfold(Some(url), move |url| async move {
        let Some(url) = url else {
            return Ok(None);
        };
        let text = http.text(&http.api(&url)?).await?;
        let ids: Vec<Result<PostId>> = extract_iter(&text, "thumb' href='/g4/view/", "'")
            .into_iter()
            .map(|id| Ok(PostId::new(id.to_owned())))
            .collect();
        let next = text
            .find("Next &gt;&gt;")
            .map(|pos| format!("{ROOT}{}", rextr(&text, "href='", "'", pos)));
        Ok::<_, crate::Error>(Some((stream::iter(ids), next)))
    })
    .try_flatten()
    .boxed()
}

/// Upstream `_pagination_folders`: favorites, descending into folders
/// depth-first (when `recursive`). Like upstream, the `folder` keyword is
/// reset to `""` whenever a folder listing ends.
fn favorites(http: &Http, user: String, folder: Option<String>, recursive: bool) -> Events<'_> {
    struct Frame {
        url: String,
        page: Option<i64>,
        pending: VecDeque<(bool, String)>,
    }
    struct State {
        stack: Vec<Frame>,
        folder: String,
        seen: HashSet<String>,
    }
    let mut url = format!("{ROOT}/g4/favorites/{user}");
    let mut current = String::new();
    if let Some(folder) = folder {
        url = format!("{url}/{folder}");
        current = crate::sites::percent_decode(&folder);
    }
    let state = State {
        stack: vec![Frame {
            url,
            page: Some(1),
            pending: VecDeque::new(),
        }],
        folder: current,
        seen: HashSet::new(),
    };
    let ids = stream::try_unfold(state, move |mut state| async move {
        loop {
            let Some(frame) = state.stack.last_mut() else {
                return Ok(None);
            };
            if let Some((is_folder, value)) = frame.pending.pop_front() {
                let url = format!("{}/{value}", frame.url);
                if !is_folder {
                    let id = PostId {
                        id: value,
                        folder: Some(state.folder.clone()),
                    };
                    return Ok(Some((id, state)));
                }
                if recursive && state.seen.insert(value.clone()) {
                    state.folder = crate::sites::percent_decode(&value);
                    state.stack.push(Frame {
                        url,
                        page: Some(1),
                        pending: VecDeque::new(),
                    });
                }
                continue;
            }
            let Some(page) = frame.page else {
                state.stack.pop();
                state.folder = String::new();
                continue;
            };
            let mut request = http.api(&frame.url)?;
            request
                .query_pairs_mut()
                .append_pair("p", &page.to_string());
            let text = http.text(&request).await?;
            let items = extract_iter(&text, "<li class='gallery-item", "</li>");
            frame.page =
                (items.len() >= 40 || text.contains(">Next &gt;&gt;<")).then_some(page + 1);
            for item in items {
                if extr(item, "data-item-type=\"", "\"") == "Folders" {
                    let href = extr(item, "href='", "'");
                    let name = href.rsplit('/').next().unwrap_or_default();
                    frame.pending.push_back((true, name.to_owned()));
                } else {
                    let id = extr(item, "data-item-id='", "'");
                    frame.pending.push_back((false, id.to_owned()));
                }
            }
        }
    })
    .boxed();
    posts(http, &FAVORITE, ids, Some(user), Map::new(), recursive)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, queued, response, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://aryion.com/g4/gallery/jameshoward"),
            Some(Target::Gallery("jameshoward".into()))
        );
        assert_eq!(
            t("https://www.aryion.com/g4/user/x/y"),
            Some(Target::Gallery("x".into()))
        );
        assert_eq!(
            t("https://aryion.com/g4/latest.php?name=jameshoward"),
            Some(Target::Gallery("jameshoward".into()))
        );
        assert_eq!(
            t("https://aryion.com/g4/favorites/jameshoward/a/b"),
            Some(Target::Favorite("jameshoward".into(), Some("a/b".into())))
        );
        assert_eq!(
            t("https://aryion.com/g4/favorites/jameshoward"),
            Some(Target::Favorite("jameshoward".into(), None))
        );
        assert_eq!(
            t("https://aryion.com/g4/messagepage.php"),
            Some(Target::Watch)
        );
        assert_eq!(
            t("https://aryion.com/g4/tags.php?tag=star+wars&p=28"),
            Some(Target::Tag("tag=star+wars&p=28".into()))
        );
        assert_eq!(
            t("https://aryion.com/g4/search.php?q=x&tags=y"),
            Some(Target::Search("q=x&tags=y".into()))
        );
        assert_eq!(
            t("https://aryion.com/g4/view/510079"),
            Some(Target::Post("510079".into()))
        );
        for raw in [
            "https://aryion.com/g4/tags.php",
            "https://aryion.com/g4/view/x",
            "https://aryion.com/gallery/x",
            "https://aryion.com.evil.com/g4/view/1",
            "https://evilaryion.com/g4/view/1",
            "https://aryion.com:8080/g4/view/1",
            "https://u:p@aryion.com/g4/view/1",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[test]
    fn content_disposition() {
        assert_eq!(
            filename_from_contentdisposition("attachment; filename=\"a b.png\""),
            "a b.png"
        );
        assert_eq!(
            filename_from_contentdisposition("inline; filename*=UTF-8''%E2%9C%93.jpg; x"),
            "✓.jpg"
        );
        assert_eq!(filename_from_contentdisposition("inline"), "");
    }

    fn head(content_type: &str, name: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Disposition: inline; filename=\"{name}\"\r\nContent-Length: 1234\r\nLast-Modified: Thu, 02 Jan 2020 03:04:05 GMT\r\nConnection: close\r\n\r\n"
        )
        .into_bytes()
    }

    const VIEW: &str = "<title>g4 :: My &amp; Title by Artist</title>
        <meta property=\"og:description\" content=\"Desc &amp; more\">
        <span class='cookiecrumb'><a>g4</a> &gt; <a>Artist</a> &gt; <a>Folder A</a> &gt; <a>Sub</a> &gt; <a>Here</a></span>
        <b>Views</b>: 1,234<br><b>Resolution</b>: 800x600<br><b>Comments</b>: 5<br><b>Favorites</b>: 6<br>
        <span class='taglist'><a>tag1</a> <a>tag 2</a></span>";

    #[tokio::test]
    async fn extracts_post() {
        let (root, server) = serve(vec![head("image/png", "file.name.png"), html(VIEW)]);
        let http = http("Eka's Portal", root);
        let files = media(extract(
            &http,
            Url::parse("https://aryion.com/g4/view/510079").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let k = keywords(&files[0]);
        assert_eq!(k["id"], 510079);
        assert_eq!(k["user"], "Artist");
        assert_eq!(k["title"], "My & Title");
        assert_eq!(k["description"], "Desc & more");
        assert_eq!(k["path"], serde_json::json!(["Folder A", "Sub"]));
        assert_eq!(k["date"], "2020-01-02 03:04:05");
        assert_eq!(k["size"], 1234);
        assert_eq!(k["views"], 1234);
        assert_eq!(k["width"], 800);
        assert_eq!(k["height"], 600);
        assert_eq!(k["comments"], 5);
        assert_eq!(k["favorites"], 6);
        assert_eq!(k["tags"], serde_json::json!(["tag1", "tag 2"]));
        assert_eq!(k["filename"], "file.name");
        assert_eq!(k["extension"], "png");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "510079 My & Title.png");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            ["aryion", "artist", "Folder A", "Sub"]
                .iter()
                .collect::<std::path::PathBuf>()
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{id}",["510079"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("HEAD /g4/data.php?id=510079 "));
        assert!(requests[1].starts_with("GET /g4/view/510079 "));
    }

    #[tokio::test]
    async fn tag_search_expands_folders_and_skips_missing_posts() {
        let list = "<li class='gallery-item' id='11'></li><li class='gallery-item' id='12'></li>";
        let folder = "<li class='gallery-item' id='21'></li>";
        let (root, server) = serve(vec![
            html(list),
            head("application/x-folder", ""),
            html(folder),
            response("404 Not Found", "text/html", b""),
        ]);
        let http = http("Eka's Portal", root);
        let children = queued(extract(
            &http,
            Url::parse("https://aryion.com/g4/tags.php?tag=a+b&p=2").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].subcategory(), "tag");
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("GET /g4/tags.php?tag=a+b&p=2 "),
            "{}",
            requests[0]
        );
        assert!(requests[1].starts_with("HEAD /g4/data.php?id=11 "));
        assert!(requests[2].starts_with("GET /g4/view/11?p=1 "));
        assert!(requests[3].starts_with("HEAD /g4/data.php?id=12 "));
    }

    #[tokio::test]
    async fn favorites_descend_into_folders() {
        let top = "<li class='gallery-item' data-item-type=\"Folders\"><a href='/g4/favorites/u/My%20Folder'></a></li>\
            <li class='gallery-item' data-item-id='2'></li>";
        let sub = "<li class='gallery-item' data-item-id='1'></li>";
        let (root, server) = serve(vec![
            html(top),
            html(sub),
            head("image/jpeg", "a.jpg"),
            html(VIEW),
            head("image/jpeg", "b.jpg"),
            html(VIEW),
        ]);
        let http = http("Eka's Portal", root);
        let files = media(extract(
            &http,
            Url::parse("https://aryion.com/g4/favorites/User").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(keywords(&files[0])["folder"], "My Folder");
        assert_eq!(keywords(&files[0])["user"], "User");
        // Upstream resets the folder after leaving a subfolder.
        assert_eq!(keywords(&files[1])["folder"], "");
        let (_, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("aryion/user/favorites/My Folder")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["f_{user}_{id}",["User","1"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /g4/favorites/User?p=1 "));
        assert!(requests[1].starts_with("GET /g4/favorites/User/My%20Folder?p=1 "));
        assert!(requests[2].starts_with("HEAD /g4/data.php?id=1 "));
    }

    #[tokio::test]
    async fn watch_list_needs_a_session() {
        let http = http("Eka's Portal", Url::parse("http://127.0.0.1:9/").unwrap());
        assert!(
            crate::sites::testing::fails(extract(
                &http,
                Url::parse("https://aryion.com/g4/messagepage.php").unwrap()
            ))
            .await
        );
    }
}
