//! 4archive.org threads and boards, mirroring gallery_dl/extractor/4archive.py
//! by parsing HTML pages.
use super::{Extr, NO_DATE, component, datetime_string, digits, host_in, parse_int, rest};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://4archive.org";

static THREAD: Route = Route::new(
    "thread",
    "{no} {filename}.{extension}",
    &["{category}", "{board}", "{thread} {title}"],
    "{board}_{thread}_{no}",
);
static BOARD: Route = Route::new("board", "{filename}.{extension}", &["{category}"], "");

pub static SITE: Site = Site {
    category: "4archive",
    name: "4archive",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&THREAD, &BOARD],
    api_key: false,
};

#[derive(Debug, PartialEq)]
enum Target {
    Thread { board: String, thread: String },
    Board { board: String, page: Option<String> },
}

fn target(url: &Url) -> Option<Target> {
    if !host_in(url, &["4archive.org"]) {
        return None;
    }
    let tail = rest(url).strip_prefix("/board")?;
    let (board, tail) = component(tail)?;
    if let Some((thread, _)) = tail.strip_prefix("/thread/").and_then(digits) {
        return Some(Target::Thread {
            board,
            thread: thread.into(),
        });
    }
    // `(?:/(\d+))?/?$`
    let (page, tail) = match tail.strip_prefix('/').and_then(digits) {
        Some((page, tail)) => (Some(page.to_owned()), tail),
        None => (None, tail),
    };
    matches!(tail, "" | "/").then_some(Target::Board { board, page })
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Thread { board, thread }) => crate::sites::once(async move {
            let api = http.api(&format!("{ROOT}/board/{board}/thread/{thread}"))?;
            let page = http.text(&api).await?;
            let events = parse_thread(&page, &board, &thread)?
                .into_iter()
                .flat_map(|item| crate::sites::post(&SITE, http, &THREAD, vec![item]))
                .collect();
            Ok(crate::sites::events(events))
        }),
        Some(Target::Board { board, page }) => {
            let first = page.as_deref().map_or(1, |p| parse_int(p).max(1));
            crate::sites::paginate(Some(first), move |pnum| {
                let board = board.clone();
                async move {
                    let Some(pnum) = pnum else {
                        return Ok(None);
                    };
                    let api = http.api(&format!("{ROOT}/board/{board}/{pnum}"))?;
                    let page = http.text(&api).await?;
                    if !page.contains("class=\"thread\"") {
                        return Ok(None);
                    }
                    let children = super::extract_iter(&page, "class=\"thread\" id=\"t", "\"")
                        .into_iter()
                        .filter_map(|thread| {
                            let url = Url::parse(&format!("{ROOT}/board/{board}/thread/{thread}"));
                            Some((url.ok()?, Map::new()))
                        })
                        .collect();
                    let events = super::queue_all(http, &SITE, &BOARD, children);
                    Ok(Some((events, (pnum < 100_000).then_some(pnum + 1))))
                }
            })
        }
        None => crate::sites::fail(error("unsupported 4archive URL")),
    }
}

/// One item per post with a file; posts without files only form
/// (empty) Directory boundaries upstream and are dropped here.
fn parse_thread(page: &str, board: &str, thread: &str) -> Result<Vec<Item>> {
    let mut title = crate::sites::unescape(
        crate::sites::extr(page, "class=\"subject\">", "</span>").unwrap_or_default(),
    );
    let posts: Vec<Map<String, Value>> = page
        .split("class=\"postContainer")
        .skip(1)
        .map(parse_post)
        .collect();
    let first = posts
        .first()
        .ok_or_else(|| error("4archive thread page contains no posts"))?;
    if title.is_empty() {
        let com = first.get("com").and_then(Value::as_str).unwrap_or_default();
        title = super::truncate(com, 50);
    }
    let mut items = Vec::new();
    for mut post in posts {
        post.insert("board".into(), board.into());
        post.insert("thread".into(), parse_int(thread).into());
        post.insert("title".into(), title.clone().into());
        let date = post.get("date").and_then(Value::as_i64).unwrap_or(NO_DATE);
        post.insert("date".into(), datetime_string(date).into());
        post.insert("time".into(), date.into());
        let Some(url) = post
            .get("url")
            .and_then(Value::as_str)
            .and_then(absolute)
            .filter(crate::sites::plain)
        else {
            continue;
        };
        let filename = post
            .get("filename")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        super::set_nameext(&mut post, &filename);
        post.insert("url".into(), url.as_str().into());
        let no = post.get("no").map(crate::sites::scalar).unwrap_or_default();
        let name = post.get("name").and_then(Value::as_str).map(str::to_owned);
        let com = post.get("com").and_then(Value::as_str).map(str::to_owned);
        items.push(
            Item::new(url, post)
                .ids(thread, no)
                .user(name)
                .caption(com)
                .timestamp(Some(date)),
        );
    }
    Ok(items)
}

/// An absolute, protocol-relative or root-relative file link.
fn absolute(link: &str) -> Option<Url> {
    if link.starts_with('/') || link.starts_with("http") {
        Url::parse(ROOT).ok()?.join(link).ok()
    } else {
        None
    }
}

/// gallery-dl `text.parse_bytes` for values like `1.5 M`.
fn parse_bytes(value: &str) -> i64 {
    let value = value.trim();
    let Some(last) = value.chars().next_back() else {
        return 0;
    };
    let (number, power) = match "bkmgtp".find(last.to_ascii_lowercase()) {
        Some(power) => (&value[..value.len() - last.len_utf8()], power as i32),
        None => (value, 0),
    };
    number
        .trim()
        .parse::<f64>()
        .map_or(0, |n| (n * 1024f64.powi(power)).round_ties_even() as i64)
}

fn parse_post(html: &str) -> Map<String, Value> {
    let mut extr = Extr::new(html);
    let mut post = Map::new();
    post.insert(
        "name".into(),
        extr.next("class=\"name\">", "</span>").into(),
    );
    let date = match extr.next("class=\"dateTime\">", "<") {
        "" => extr.next("class=\"dateTime postNum\" >", "<"),
        date => date,
    };
    let date = crate::source_date::parse_string(date.trim()).unwrap_or(NO_DATE);
    post.insert("date".into(), date.into());
    post.insert("no".into(), parse_int(extr.next(">Post No.", "<")).into());
    if html.contains("class=\"file\"") {
        extr.next("class=\"fileText\"", ">File: <a");
        post.insert("url".into(), extr.next("href=\"", "\"").into());
        post.insert("filename".into(), extr.next("alt=\"Image: ", "\"").into());
        let size = extr.next(" (", ", ");
        let size = &size[..size.len() - size.chars().next_back().map_or(0, char::len_utf8)];
        post.insert("size".into(), parse_bytes(size).into());
        post.insert("width".into(), parse_int(extr.next("", "x")).into());
        post.insert("height".into(), parse_int(extr.next("", "px")).into());
    }
    extr.next("<blockquote ", "");
    let com = extr.next(">", "</blockquote>");
    post.insert(
        "com".into(),
        crate::sites::unescape(&crate::sites::remove_html(com)).into(),
    );
    post
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://4archive.org/board/a/thread/12345/"),
            Some(Target::Thread {
                board: "a".into(),
                thread: "12345".into()
            })
        );
        assert_eq!(
            t("https://4archive.org/board/a/"),
            Some(Target::Board {
                board: "a".into(),
                page: None
            })
        );
        assert_eq!(
            t("https://4archive.org/board/a/7"),
            Some(Target::Board {
                board: "a".into(),
                page: Some("7".into())
            })
        );
        for raw in [
            "https://4archive.org/a/thread/1",
            "https://4archive.org/board/a/thread/x",
            "https://4archive.org/board/a/?page=2",
            "https://www.4archive.org/board/a/",
            "https://4archive.org.evil.test/board/a/",
            "https://4archive.org:8080/board/a/",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[test]
    fn parses_sizes_like_gallery_dl() {
        assert_eq!(parse_bytes("1.5 M"), 1_572_864);
        assert_eq!(parse_bytes("500"), 500);
        assert_eq!(parse_bytes("2k"), 2048);
        assert_eq!(parse_bytes(""), 0);
        assert_eq!(parse_bytes("x"), 0);
    }

    const PAGE: &str = r#"<span class="subject"></span>
<div class="postContainer op"><span class="name">Anon</span>
<span class="dateTime">2023-01-02T03:04:05</span> <a>Post No.100<</a>
<div class="file"><div class="fileText" id="fT1">File: <a href="https://i.4cdn.org/a/150.jpg">x</a>
<img alt="Image: my%20pic.JPG" src="t.jpg"> (1.5 MB, 640x480px)</div></div>
<blockquote class="postMessage">First &amp; <b>best</b></blockquote></div>
<div class="postContainer reply"><span class="name">Bob</span>
<span class="dateTime postNum" >2023-01-02T03:05:00</span> <a>Post No.101<</a>
<blockquote class="postMessage">text only</blockquote></div>"#;

    #[tokio::test]
    async fn extracts_thread_posts() {
        let (origin, server) = serve(vec![html(PAGE)]);
        let http = http("4archive", origin);
        let files = media(extract(
            &http,
            Url::parse("https://4archive.org/board/a/thread/100/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].url.as_str(), "https://i.4cdn.org/a/150.jpg");
        assert!(!files[0].headers.contains_key("Referer"));
        let k = keywords(&files[0]);
        assert_eq!(k["no"], 100);
        assert_eq!(k["thread"], 100);
        assert_eq!(k["name"], "Anon");
        assert_eq!(k["filename"], "my pic");
        assert_eq!(k["extension"], "jpg");
        assert_eq!(k["size"], 1_572_864);
        assert_eq!(k["width"], 640);
        assert_eq!(k["height"], 480);
        assert_eq!(k["time"], 1_672_628_645);
        assert_eq!(k["date"], "2023-01-02 03:04:05");
        assert_eq!(k["com"], "First & best");
        assert_eq!(k["title"], "First & best");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "100 my pic.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("4archive/a/100 First & best")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{board}_{thread}_{no}",["a","100","100"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /board/a/thread/100 "));
    }

    #[tokio::test]
    async fn empty_thread_page_is_an_error() {
        let (origin, _server) = serve(vec![html("<html>gone</html>")]);
        let http = http("4archive", origin);
        let events = extract(
            &http,
            Url::parse("https://4archive.org/board/a/thread/9").unwrap(),
        );
        let results: Vec<_> = futures_util::StreamExt::collect(events).await;
        assert!(results[0].is_err());
    }

    #[tokio::test]
    async fn board_pages_until_no_threads() {
        let listing = r#"<div class="thread" id="t100"></div><div class="thread" id="t200">"#;
        let (origin, server) = serve(vec![
            html(listing),
            html(PAGE),
            html(PAGE),
            html("<html></html>"),
        ]);
        let http = http("4archive", origin);
        let files = media(extract(
            &http,
            Url::parse("https://4archive.org/board/a/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /board/a/1 "));
        assert!(requests[1].starts_with("GET /board/a/thread/100 "));
        assert!(requests[2].starts_with("GET /board/a/thread/200 "));
        assert!(requests[3].starts_with("GET /board/a/2 "));
    }
}
