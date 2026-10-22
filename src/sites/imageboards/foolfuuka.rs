//! FoolFuuka 4chan archives, mirroring gallery_dl/extractor/foolfuuka.py
//! through the `/_/api/chan/` JSON API.
use super::{DIRECTORY, component, digits, host_in, parse_int, rest};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

const FILENAME: &str = "{timestamp_ms} {filename_media}.{extension}";
const ARCHIVE: &str = "{board[shortname]}_{num}_{timestamp}";

// Upstream thread directory: ("{category}", "{board[shortname]}",
// "{thread_num} {title|comment[:50]}"), rendered from the OP; files carry
// the OP's fields under `_directory`.
static THREAD: Route = Route::new(
    "thread",
    FILENAME,
    &[
        "{category}",
        "{board[shortname]}",
        "{thread_num} {_directory[title]|_directory[comment][:50]}",
    ],
    ARCHIVE,
);
static BOARD: Route = Route::new("board", FILENAME, &["{category}"], ARCHIVE);
// Upstream: ("{category}", "search", "{search}").
static SEARCH: Route = Route::new(
    "search",
    FILENAME,
    &["{category}", "search", "{_directory[search]}"],
    ARCHIVE,
);
// Upstream: ("{category}", "{board}", "gallery"), where `board` is the
// Directory kwdict's board name (files carry the API's board object).
static GALLERY: Route = Route::new(
    "gallery",
    FILENAME,
    &["{category}", "{_directory[board]}", "gallery"],
    ARCHIVE,
);

macro_rules! instance {
    ($ident:ident, $category:literal, $name:literal) => {
        pub static $ident: Site = Site {
            category: $category,
            name: $name,
            matches: |url| target($category, url).is_some(),
            extract: |http, url| extract($category, http, url),
            routes: &[&THREAD, &BOARD, &SEARCH, &GALLERY],
            api_key: false,
        };
    };
}
instance!(FOURPLEBS, "4plebs", "4plebs");
instance!(ARCHIVEDMOE, "archivedmoe", "Archived.Moe");
instance!(ARCHIVEOFSINS, "archiveofsins", "Archive of Sins");
instance!(B4K, "b4k", "arch.b4k");
instance!(DESUARCHIVE, "desuarchive", "Desuarchive");
instance!(FIREDEN, "fireden", "Fireden");
instance!(PALANQ, "palanq", "Palanq");
instance!(RBT, "rbt", "RebeccaBlackTech");
instance!(THEBARCHIVE, "thebarchive", "The /b/ Archive");

fn site(category: &str) -> &'static Site {
    match category {
        "4plebs" => &FOURPLEBS,
        "archivedmoe" => &ARCHIVEDMOE,
        "archiveofsins" => &ARCHIVEOFSINS,
        "b4k" => &B4K,
        "desuarchive" => &DESUARCHIVE,
        "fireden" => &FIREDEN,
        "palanq" => &PALANQ,
        "rbt" => &RBT,
        _ => &THEBARCHIVE,
    }
}

fn root(category: &str) -> &'static str {
    match category {
        "4plebs" => "https://archive.4plebs.org",
        "archivedmoe" => "https://archived.moe",
        "archiveofsins" => "https://archiveofsins.com",
        "b4k" => "https://arch.b4k.dev",
        "desuarchive" => "https://desuarchive.org",
        "fireden" => "https://boards.fireden.net",
        "palanq" => "https://archive.palanq.win",
        "rbt" => "https://rbt.asia",
        _ => "https://thebarchive.com",
    }
}

fn hosts(category: &str) -> &'static [&'static str] {
    match category {
        "4plebs" => &["4plebs.org", "archive.4plebs.org"],
        "archivedmoe" => &["archived.moe"],
        "archiveofsins" => &["archiveofsins.com", "www.archiveofsins.com"],
        "b4k" => &["arch.b4k.dev", "arch.b4k.co"],
        "desuarchive" => &["desuarchive.org"],
        "fireden" => &["boards.fireden.net"],
        "palanq" => &["archive.palanq.win"],
        "rbt" => &[
            "rbt.asia",
            "rebeccablacktech.com",
            "archive.rebeccablacktech.com",
        ],
        _ => &["thebarchive.com"],
    }
}

#[derive(Debug, PartialEq)]
enum Target {
    Thread {
        board: String,
        thread: String,
    },
    Board {
        board: String,
        page: Option<String>,
    },
    Search {
        board: String,
        params: Vec<(String, String)>,
    },
    Gallery {
        board: String,
        page: Option<String>,
    },
}

fn target(category: &str, url: &Url) -> Option<Target> {
    if !host_in(url, hosts(category)) {
        return None;
    }
    let (board, tail) = component(rest(url))?;
    // `/thread/(\d+)`
    if let Some((thread, _)) = tail.strip_prefix("/thread/").and_then(digits) {
        return Some(Target::Thread {
            board,
            thread: thread.into(),
        });
    }
    // `(?:/(?:page/)?(\d*))?$`
    let page = match tail {
        "" => Some(None),
        _ => tail.strip_prefix('/').and_then(|t| {
            let t = t.strip_prefix("page/").unwrap_or(t);
            t.bytes()
                .all(|b| b.is_ascii_digit())
                .then(|| Some(t.to_owned()))
        }),
    };
    if let Some(page) = page {
        return Some(Target::Board { board, page });
    }
    // `/search((?:/[^/?#]+/[^/?#]+)+)`
    if let Some(mut args) = tail.strip_prefix("/search") {
        let mut params: Vec<(String, String)> = Vec::new();
        let mut key = String::new();
        let mut pairs = 0;
        while let Some((name, after)) = component(args) {
            let Some((value, after)) = component(after) else {
                break;
            };
            // Same pairing rule as upstream's alternating split.
            for arg in [name, value] {
                if key.is_empty() {
                    key = arg;
                } else {
                    let value = crate::sites::unescape(&arg);
                    match params.iter_mut().find(|(k, _)| *k == key) {
                        Some(slot) => slot.1 = value,
                        None => params.push((std::mem::take(&mut key), value)),
                    }
                    key.clear();
                }
            }
            pairs += 1;
            args = after;
        }
        if pairs == 0 {
            return None;
        }
        if board != "_" {
            set_param(&mut params, "boards", board.clone());
        }
        return Some(Target::Search { board, params });
    }
    // `/gallery(?:/(\d+))?`
    if let Some(after) = tail.strip_prefix("/gallery") {
        let page = after
            .strip_prefix('/')
            .and_then(digits)
            .map(|(page, _)| page.to_owned());
        return Some(Target::Gallery { board, page });
    }
    None
}

fn set_param(params: &mut Vec<(String, String)>, key: &str, value: String) {
    match params.iter_mut().find(|(k, _)| k == key) {
        Some(slot) => slot.1 = value,
        None => params.push((key.into(), value)),
    }
}

fn extract<'a>(category: &'static str, http: &'a Http, url: Url) -> Events<'a> {
    let Some(target) = target(category, &url) else {
        return crate::sites::fail(error("unsupported FoolFuuka URL"));
    };
    let base = root(category);
    match target {
        Target::Thread { board, thread } => crate::sites::once(async move {
            let mut api = http.api(&format!("{base}/_/api/chan/thread/"))?;
            api.query_pairs_mut()
                .append_pair("board", &board)
                .append_pair("num", &thread);
            let body = http.json(&api).await?;
            let data = body
                .get(&thread)
                .and_then(Value::as_object)
                .ok_or_else(|| error("FoolFuuka thread response lacks the thread"))?;
            let op = data
                .get("op")
                .cloned()
                .ok_or_else(|| error("FoolFuuka thread response lacks the OP"))?;
            let mut posts = vec![op.clone()];
            if let Some(Value::Object(replies)) = data.get("posts") {
                let mut replies: Vec<Value> = replies.values().cloned().collect();
                replies.sort_by_key(|post| post.get("timestamp").and_then(Value::as_i64));
                posts.extend(replies);
            }
            let directory = op
                .as_object()
                .map(|op| super::directory_fields(op, &["title", "comment"]))
                .unwrap_or_default();
            let events = files(category, http, &THREAD, posts, directory).await?;
            Ok(crate::sites::events(events))
        }),
        Target::Board { board, page } => board_events(category, http, board, page),
        Target::Search { params, .. } => search_events(category, http, params),
        Target::Gallery { board, page } => {
            let mut directory = Map::new();
            directory.insert("board".into(), board.clone().into());
            let directory = Value::Object(directory);
            let first = page.as_deref().map_or(1, |p| parse_int(p).max(0) as u64);
            let single = page.is_some();
            crate::sites::paginate(Some(first), move |pnum| {
                let board = board.clone();
                let directory = directory.clone();
                async move {
                    let Some(pnum) = pnum else {
                        return Ok(None);
                    };
                    let api = http.api(&format!(
                        "{base}/_/api/chan/gallery/?board={board}&page={pnum}"
                    ))?;
                    let body = http.json(&api).await?;
                    let posts = match body {
                        Value::Array(posts) if !posts.is_empty() => posts,
                        _ => return Ok(None),
                    };
                    let events = files(category, http, &GALLERY, posts, directory).await?;
                    let next = (!single && pnum < 100_000).then_some(pnum + 1);
                    Ok(Some((crate::sites::events(events), next)))
                }
            })
        }
    }
}

fn board_events<'a>(
    category: &'static str,
    http: &'a Http,
    board: String,
    page: Option<String>,
) -> Events<'a> {
    let base = root(category);
    // `if page: return` treats an empty `/page/` group as unset.
    let single = page.as_deref().is_some_and(|p| !p.is_empty());
    let first = page
        .as_deref()
        .map_or(1, |p| if p.is_empty() { 1 } else { parse_int(p) });
    crate::sites::paginate(Some(first), move |pnum| {
        let board = board.clone();
        async move {
            let Some(pnum) = pnum else {
                return Ok(None);
            };
            let api = http.api(&format!(
                "{base}/_/api/chan/index/?board={board}&page={pnum}"
            ))?;
            let body = http.get(&api, &[]).await?;
            let threads = match serde_json::from_slice::<Value>(&body) {
                Ok(Value::Object(threads)) if !threads.is_empty() => threads,
                _ => return Ok(None),
            };
            let mut children = Vec::new();
            for (num, thread) in threads {
                let Value::Object(mut thread) = thread else {
                    continue;
                };
                if digits(&num).is_none_or(|(_, tail)| !tail.is_empty()) {
                    continue;
                }
                let raw = format!("{base}/{board}/thread/{num}");
                let Ok(url) = Url::parse(&raw) else {
                    continue;
                };
                thread.insert("url".into(), raw.into());
                children.push((url, thread));
            }
            let events = super::queue_all(http, site(category), &BOARD, children);
            let next = (!single && pnum < 100_000).then_some(pnum + 1);
            Ok(Some((events, next)))
        }
    })
}

fn search_events<'a>(
    category: &'static str,
    http: &'a Http,
    mut params: Vec<(String, String)>,
) -> Events<'a> {
    let base = root(category);
    let mut directory = Map::new();
    let text = params
        .iter()
        .find(|(k, _)| k == "text")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    directory.insert("search".into(), text.into());
    let directory = Value::Object(directory);
    let page = params
        .iter()
        .find(|(k, _)| k == "page")
        .map_or(1, |(_, v)| match parse_int(v) {
            0 => 1,
            page => page,
        });
    set_param(&mut params, "page", page.to_string());
    if !params.iter().any(|(k, _)| k == "filter") {
        params.push(("filter".into(), "text".into()));
    }
    crate::sites::paginate(Some((page, true)), move |state| {
        let directory = directory.clone();
        let mut params = params.clone();
        async move {
            let Some((page, first)) = state else {
                return Ok(None);
            };
            if !first {
                // Upstream `request_interval = (0.5, 1.5)`.
                #[cfg(not(test))]
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
            set_param(&mut params, "page", page.to_string());
            let mut api = http.api(&format!("{base}/_/api/chan/search/"))?;
            api.query_pairs_mut().extend_pairs(&params);
            let body = http.get(&api, &[]).await?;
            let Ok(data) = serde_json::from_slice::<Value>(&body) else {
                return Ok(None);
            };
            let posts = match &data {
                Value::Object(map) => {
                    if super::truthy(map.get("error")) {
                        return Ok(None);
                    }
                    data.get("0").and_then(|d| d.get("posts"))
                }
                Value::Array(list) => list.first().and_then(|d| d.get("posts")),
                _ => return Ok(None),
            };
            let posts = posts
                .and_then(Value::as_array)
                .cloned()
                .ok_or_else(|| error("FoolFuuka search response lacks posts"))?;
            let count = posts.len();
            let events = files(category, http, &SEARCH, posts, directory).await?;
            let next = (count > 3 && page < 100_000).then_some((page + 1, false));
            Ok(Some((crate::sites::events(events), next)))
        }
    })
}

/// Turn API posts into one post boundary of files, resolving remote media.
async fn files(
    category: &'static str,
    http: &Http,
    route: &'static Route,
    posts: Vec<Value>,
    directory: Value,
) -> Result<Vec<crate::event::ExtractionEvent>> {
    let base = root(category);
    let mut items = Vec::new();
    for post in posts {
        let Value::Object(mut post) = post else {
            continue;
        };
        let Some(media) = post.get("media").filter(|m| super::truthy(Some(m))).cloned() else {
            continue;
        };
        let board = post
            .get("board")
            .and_then(|b| b.get("shortname"))
            .map(crate::sites::scalar)
            .unwrap_or_default();
        let mut url = media
            .get("media_link")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if url.is_empty() && media.get("remote_media_link").is_some() {
            url = remote(category, http, &board, &media).await?;
        }
        if url.starts_with('/') {
            url = format!("{base}{url}");
        }
        let Some(url) = Url::parse(&url).ok().filter(crate::sites::plain) else {
            continue;
        };
        let text = |key: &str| {
            media
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        let name = text("media");
        let (filename, extension) = super::rpartition(&name, ".");
        post.insert("filename".into(), filename.into());
        post.insert("extension".into(), extension.into());
        let media_filename = text("media_filename");
        post.insert(
            "filename_media".into(),
            super::rpartition(&media_filename, ".").0.into(),
        );
        let orig = text("media_orig");
        post.insert(
            "timestamp_ms".into(),
            parse_int(super::rpartition(&orig, ".").0).into(),
        );
        post.insert(DIRECTORY.into(), directory.clone());
        let thread = post
            .get("thread_num")
            .map(crate::sites::scalar)
            .unwrap_or_default();
        let num = post.get("num").map(crate::sites::scalar).unwrap_or_default();
        let user = post.get("name").and_then(Value::as_str).map(str::to_owned);
        let caption = post
            .get("comment")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let timestamp = post.get("timestamp").and_then(Value::as_i64);
        let mut item = Item::new(url, post)
            .ids(thread, num)
            .user(user)
            .caption(caption)
            .timestamp(timestamp);
        if category != "archivedmoe" {
            item = item.header("Referer", &format!("{base}/"));
        }
        items.push(item);
    }
    Ok(crate::sites::post(site(category), http, route, items))
}

/// Upstream `remote()`: resolve a `remote_media_link` redirect page.
async fn remote(category: &str, http: &Http, board: &str, media: &Value) -> Result<String> {
    let link = media
        .get("remote_media_link")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if category == "b4k" {
        return Ok(link.to_owned());
    }
    if matches!(board, "wsg" | "gif") {
        let orig = media
            .get("media_orig")
            .map(crate::sites::scalar)
            .unwrap_or_default();
        return Ok(format!("https://i.4cdn.org/{board}/{orig}"));
    }
    // Only follow redirect pages on the archive itself.
    let base = Url::parse(root(category)).map_err(|_| error("invalid FoolFuuka root"))?;
    let Some(page_url) = Url::parse(link)
        .ok()
        .filter(|u| crate::sites::plain(u) && u.host_str() == base.host_str())
    else {
        return Ok(String::new());
    };
    let page = http.text(&http.api(page_url.as_str())?).await?;
    Ok(redirect_target(
        category,
        board,
        crate::sites::extr(&page, "http-equiv=\"Refresh\" content=\"0; url=", "\"")
            .unwrap_or_default(),
    ))
}

fn redirect_target(category: &str, board: &str, url: &str) -> String {
    if url.starts_with("https://thebarchive.com/") {
        // '.webm' -> '.web' (#5116)
        return url.strip_suffix('m').filter(|_| url.ends_with(".webm")).unwrap_or(url).into();
    }
    if category != "archivedmoe" {
        return url.into();
    }
    let (path, filename) = super::rpartition(url, "/");
    let domain = match board {
        "3" | "biz" | "ck" | "diy" | "fa" | "ic" | "jp" | "lit" | "sci" => Some("warosu.org"),
        "tg" => Some("archive.4plebs.org"),
        _ => None,
    };
    if let Some(domain) = domain {
        return format!("https://{domain}/{board}/full_image/{filename}");
    }
    if ["b4k.", "desuarchive.", "palanq."]
        .iter()
        .any(|archive| path.contains(archive))
    {
        let (name, ext) = super::rpartition(filename, ".");
        if name.chars().count() > 13 {
            return format!("{path}/{}.{ext}", super::truncate(name, 13));
        }
    }
    url.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, json, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |c, raw: &str| target(c, &Url::parse(raw).unwrap());
        assert_eq!(
            t("4plebs", "https://archive.4plebs.org/tg/thread/54059290/"),
            Some(Target::Thread {
                board: "tg".into(),
                thread: "54059290".into()
            })
        );
        assert_eq!(
            t("archivedmoe", "https://archived.moe/a/"),
            Some(Target::Board {
                board: "a".into(),
                page: Some(String::new())
            })
        );
        assert_eq!(
            t("rbt", "https://archive.rebeccablacktech.com/g/page/3"),
            Some(Target::Board {
                board: "g".into(),
                page: Some("3".into())
            })
        );
        assert_eq!(
            t("desuarchive", "https://desuarchive.org/a"),
            Some(Target::Board {
                board: "a".into(),
                page: None
            })
        );
        assert_eq!(
            t("archiveofsins", "https://www.archiveofsins.com/_/search/text/yuri/page/2/"),
            Some(Target::Search {
                board: "_".into(),
                params: vec![("text".into(), "yuri".into()), ("page".into(), "2".into())]
            })
        );
        assert_eq!(
            t("palanq", "https://archive.palanq.win/a/search/subject/x/"),
            Some(Target::Search {
                board: "a".into(),
                params: vec![("subject".into(), "x".into()), ("boards".into(), "a".into())]
            })
        );
        assert_eq!(
            t("fireden", "https://boards.fireden.net/sci/gallery/5"),
            Some(Target::Gallery {
                board: "sci".into(),
                page: Some("5".into())
            })
        );
        assert_eq!(
            t("b4k", "https://arch.b4k.co/v/gallery"),
            Some(Target::Gallery {
                board: "v".into(),
                page: None
            })
        );
        for (c, raw) in [
            ("4plebs", "https://4plebs.org/"),
            ("4plebs", "https://www.4plebs.org/a/thread/1"),
            ("4plebs", "https://archive.4plebs.org.evil.test/a/thread/1"),
            ("4plebs", "https://archive.4plebs.org:8000/a/thread/1"),
            ("archivedmoe", "https://archived.moe/a/?x=1"),
            ("archivedmoe", "https://archived.moe/a/search/"),
            ("archivedmoe", "https://archived.moe/a/search/text/"),
            ("archivedmoe", "https://archived.moe/a/foo"),
            ("archivedmoe", "https://www.archived.moe/a/thread/1"),
            ("thebarchive", "https://archived.moe/b/thread/1"),
        ] {
            assert_eq!(t(c, raw), None, "{raw}");
        }
        let find = |raw: &str| crate::sites::find(&Url::parse(raw).unwrap()).map(|s| s.category);
        assert_eq!(find("https://rbt.asia/g/thread/1"), Some("rbt"));
        assert_eq!(find("https://thebarchive.com/b/thread/1"), Some("thebarchive"));
    }

    fn post(num: u64, timestamp: i64, media: Value) -> Value {
        json!({"num": num, "thread_num": 100, "timestamp": timestamp,
            "board": {"name": "Anime", "shortname": "a"}, "title": null,
            "comment": format!("comment {num}"), "name": "Anonymous", "media": media})
    }

    fn file(orig: &str) -> Value {
        json!({"media": format!("{orig}"), "media_filename": "orig name.jpg",
            "media_orig": orig, "media_link": format!("https://img.test/a/{orig}"),
            "remote_media_link": null})
    }

    #[tokio::test]
    async fn extracts_thread_in_timestamp_order() {
        let mut op = post(100, 1500000000, file("1500000000123.jpg"));
        op["title"] = "Thread title".into();
        let body = json!({"100": {"op": op, "posts": {
            "102": post(102, 1500000200, file("1500000200456.png")),
            "101": post(101, 1500000100, json!({"media": "1500000100789.gif",
                "media_filename": "r.gif", "media_orig": "1500000100789.gif",
                "media_link": null,
                "remote_media_link": "https://archive.4plebs.org/a/redirect/1500000100789.gif"})),
            "103": post(103, 1500000300, Value::Null),
        }}});
        let redirect = r#"<meta http-equiv="Refresh" content="0; url=https://i.4cdn.org/a/1500000100789.gif">"#;
        let (origin, server) = serve(vec![json(body), html(redirect)]);
        let http = http("4plebs", origin);
        let files = media(extract(
            "4plebs",
            &http,
            Url::parse("https://archive.4plebs.org/a/thread/100/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 3);
        assert_eq!(files[1].url.as_str(), "https://i.4cdn.org/a/1500000100789.gif");
        assert_eq!(files[2].url.as_str(), "https://img.test/a/1500000200456.png");
        let k = keywords(&files[0]);
        assert_eq!(k["filename"], "1500000000123");
        assert_eq!(k["extension"], "jpg");
        assert_eq!(k["filename_media"], "orig name");
        assert_eq!(k["timestamp_ms"], 1500000000123u64);
        let (file, dir) = crate::sites::default_templates(&files[2]).unwrap();
        assert_eq!(file.render(&files[2]).unwrap(), "1500000200456 orig name.png");
        assert_eq!(
            dir.render(&files[2]).unwrap().unwrap(),
            std::path::PathBuf::from("4plebs/a/100 Thread title")
        );
        let (_, identity) = crate::sites::archive_key(&files[2]).unwrap();
        assert_eq!(
            identity,
            r#"["{board[shortname]}_{num}_{timestamp}",["a","102","1500000200"]]"#
        );
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("GET /_/api/chan/thread/?board=a&num=100 "),
            "{}",
            requests[0]
        );
        assert!(requests[1].starts_with("GET /a/redirect/1500000100789.gif "));
    }

    #[test]
    fn archivedmoe_fixes_redirects() {
        assert_eq!(
            redirect_target("archivedmoe", "jp", "https://i.4cdn.org/jp/123.jpg"),
            "https://warosu.org/jp/full_image/123.jpg"
        );
        assert_eq!(
            redirect_target(
                "archivedmoe",
                "a",
                "https://desuarchive.org/a/full_image/1500000000123456.jpg"
            ),
            "https://desuarchive.org/a/full_image/1500000000123.jpg"
        );
        assert_eq!(
            redirect_target("fireden", "a", "https://thebarchive.com/b/x.webm"),
            "https://thebarchive.com/b/x.web"
        );
        assert_eq!(
            redirect_target("fireden", "jp", "https://i.4cdn.org/jp/1.jpg"),
            "https://i.4cdn.org/jp/1.jpg"
        );
    }

    #[tokio::test]
    async fn board_paginates_until_empty() {
        let (origin, server) = serve(vec![
            json(json!({"100": {"op": {}}, "200": {"op": {}}})),
            json(json!({"100": {"op": post(100, 1, file("1.jpg"))}})),
            json(json!({"200": {"op": post(200, 2, file("2.jpg"))}})),
            json(json!({})),
        ]);
        let http = http("desuarchive", origin);
        let files = media(extract(
            "desuarchive",
            &http,
            Url::parse("https://desuarchive.org/a/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /_/api/chan/index/?board=a&page=1 "));
        assert!(requests[1].starts_with("GET /_/api/chan/thread/?board=a&num=100 "));
        assert!(requests[3].starts_with("GET /_/api/chan/index/?board=a&page=2 "));
    }

    #[tokio::test]
    async fn search_paginates_and_names_directory() {
        let page: Vec<_> = (1..=4).map(|n| post(n, n as i64, file(&format!("{n}.jpg")))).collect();
        let (origin, server) = serve(vec![
            json(json!({"0": {"posts": page}})),
            json(json!([{"posts": [post(9, 9, file("9.jpg"))]}])),
        ]);
        let http = http("archiveofsins", origin);
        let files = media(extract(
            "archiveofsins",
            &http,
            Url::parse("https://archiveofsins.com/h/search/text/a%20b/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 5);
        let (_, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("archiveofsins/search/a%20b")
        );
        let requests = server.join().unwrap();
        assert!(
            requests[0]
                .starts_with("GET /_/api/chan/search/?text=a%2520b&boards=h&page=1&filter=text "),
            "{}",
            requests[0]
        );
        assert!(requests[1].contains("&page=2&"));
    }

    #[tokio::test]
    async fn gallery_uses_board_directory() {
        let (origin, server) = serve(vec![
            json(json!([post(1, 1, file("1.jpg"))])),
            json(json!([])),
        ]);
        let http = http("fireden", origin);
        let files = media(extract(
            "fireden",
            &http,
            Url::parse("https://boards.fireden.net/sci/gallery").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(keywords(&files[0])["board"]["shortname"], "a");
        assert!(!files[0].headers.is_empty());
        let (_, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("fireden/sci/gallery")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /_/api/chan/gallery/?board=sci&page=1 "));
        assert!(requests[1].starts_with("GET /_/api/chan/gallery/?board=sci&page=2 "));
    }
}
