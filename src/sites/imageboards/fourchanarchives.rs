//! 4chanarchives.com threads and boards, mirroring
//! gallery_dl/extractor/4chanarchives.py by parsing HTML pages.
use super::{Extr, component, digits, host_in, parse_int, rest};
use crate::error;
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://4chanarchives.com";
const NEEDLE: &str = "<span class=\"postNum desktop\">\n                        <span><a href=\"";

static THREAD: Route = Route::new(
    "thread",
    "{no}-{filename}.{extension}",
    &["{category}", "{board}", "{thread} - {title}"],
    "{board}_{thread}_{no}",
);
static BOARD: Route = Route::new("board", "{filename}.{extension}", &["{category}"], "");

pub static SITE: Site = Site {
    category: "4chanarchives",
    name: "4chanarchives",
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
    if !host_in(url, &["4chanarchives.com"]) {
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
            let events = parse_thread(&page, &board, &thread)
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
                    let threads = super::extract_iter(&page, NEEDLE, "\"");
                    if threads.is_empty() {
                        return Ok(None);
                    }
                    let base = Url::parse(ROOT).map_err(|_| error("invalid root"))?;
                    let children = threads
                        .into_iter()
                        .filter_map(|thread| Some((base.join(thread).ok()?, Map::new())))
                        .collect();
                    let events = super::queue_all(http, &SITE, &BOARD, children);
                    Ok(Some((events, (pnum < 100_000).then_some(pnum + 1))))
                }
            })
        }
        None => crate::sites::fail(error("unsupported 4chanarchives URL")),
    }
}

/// One item per post with a file; posts without files only form
/// (empty) Directory boundaries upstream and are dropped here.
fn parse_thread(page: &str, board: &str, thread: &str) -> Vec<Item> {
    let mut title = crate::sites::unescape(
        crate::sites::extr(page, "property=\"og:title\" content=\"", "\"").unwrap_or_default(),
    );
    let posts: Vec<Map<String, Value>> = super::extract_iter(page, "id=\"pc", "</blockquote>")
        .into_iter()
        .map(parse_post)
        .collect();
    if title.is_empty()
        && let Some(op) = posts.first()
    {
        let com = op.get("com").and_then(Value::as_str).unwrap_or_default();
        title = super::truncate(&crate::sites::unescape(&crate::sites::remove_html(com)), 50);
    }
    let mut items = Vec::new();
    for mut post in posts {
        post.insert("board".into(), board.into());
        post.insert("thread".into(), thread.into());
        post.insert("title".into(), title.clone().into());
        let Some(url) = post
            .get("url")
            .and_then(Value::as_str)
            .and_then(|u| Url::parse(u).ok())
            .filter(crate::sites::plain)
        else {
            continue;
        };
        let no = post.get("no").map(crate::sites::scalar).unwrap_or_default();
        let name = post.get("name").and_then(Value::as_str).map(str::to_owned);
        let com = post.get("com").and_then(Value::as_str).map(str::to_owned);
        items.push(Item::new(url, post).ids(thread, no).user(name).caption(com));
    }
    items
}

fn parse_post(html: &str) -> Map<String, Value> {
    let mut extr = Extr::new(html);
    let mut post = Map::new();
    post.insert("no".into(), parse_int(extr.next("", "\"")).into());
    post.insert("name".into(), extr.next("class=\"name\">", "<").into());
    post.insert(
        "time".into(),
        extr.next("class=\"dateTime postNum\" >", "<")
            .trim_end()
            .into(),
    );
    // `html[html.find('<blockquote'):]`, where -1 keeps the last character.
    let quote = match html.find("<blockquote") {
        Some(start) => &html[start..],
        None => html
            .char_indices()
            .last()
            .map_or("", |(index, _)| &html[index..]),
    };
    let com = quote.split_once('>').map_or("", |(_, tail)| tail);
    post.insert("com".into(), crate::sites::unescape(com).into());
    if let Some(start) = html.find(">File: <") {
        let mut extr = Extr::at(html, start);
        let url = extr.next("href=\"", "\"").to_owned();
        let name = extr.next(">", "<");
        post.insert(
            "filename".into(),
            crate::sites::percent_decode(super::rpartition(name, ".").0).into(),
        );
        post.insert("fsize".into(), extr.next("(", ", ").into());
        post.insert("w".into(), parse_int(extr.next("", "x")).into());
        post.insert("h".into(), parse_int(extr.next("", ")")).into());
        post.insert("extension".into(), super::rpartition(&url, ".").1.into());
        post.insert("url".into(), url.into());
    }
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
            t("https://4chanarchives.com/board/c/thread/2707110/x"),
            Some(Target::Thread {
                board: "c".into(),
                thread: "2707110".into()
            })
        );
        assert_eq!(
            t("https://4chanarchives.com/board/c/"),
            Some(Target::Board {
                board: "c".into(),
                page: None
            })
        );
        assert_eq!(
            t("https://4chanarchives.com/board/c/3/"),
            Some(Target::Board {
                board: "c".into(),
                page: Some("3".into())
            })
        );
        for raw in [
            "https://4chanarchives.com/c/thread/1",
            "https://4chanarchives.com/board/c/catalog",
            "https://4chanarchives.com/board/c/?x",
            "https://www.4chanarchives.com/board/c/",
            "https://4chanarchives.com.evil.test/board/c/",
            "https://4chanarchives.com:8080/board/c/",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    const PAGE: &str = r#"<meta property="og:title" content="">
<div id="pc100"><span class="name">Anonymous</span>
<span class="dateTime postNum" >01/02/23(Mon)03:04:05 </span>
<div class="fileText">File: <a href="https://i.4cdn.org/c/1500000000123.jpg">my%20pic.jpg</a> (1.5 MB, 640x480)</div>
<blockquote class="postMessage">First &amp; <b>best</b></blockquote>
<div id="pc101"><span class="name">Bob</span><span class="dateTime postNum" >x</span>
<blockquote class="postMessage">text only</blockquote>"#;

    #[tokio::test]
    async fn extracts_thread_posts() {
        let (origin, server) = serve(vec![html(PAGE)]);
        let http = http("4chanarchives", origin);
        let files = media(extract(
            &http,
            Url::parse("https://4chanarchives.com/board/c/thread/100").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert!(!files[0].headers.contains_key("Referer"));
        let k = keywords(&files[0]);
        assert_eq!(k["no"], 100);
        assert_eq!(k["name"], "Anonymous");
        assert_eq!(k["time"], "01/02/23(Mon)03:04:05");
        assert_eq!(k["filename"], "my pic");
        assert_eq!(k["extension"], "jpg");
        assert_eq!(k["fsize"], "1.5 MB");
        assert_eq!(k["w"], 640);
        assert_eq!(k["h"], 480);
        assert_eq!(k["com"], "First & <b>best</b>");
        assert_eq!(k["title"], "First & best");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "100-my pic.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("4chanarchives/c/100 - First & best")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{board}_{thread}_{no}",["c","100","100"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /board/c/thread/100 "));
    }

    #[tokio::test]
    async fn board_pages_until_no_threads() {
        let listing = format!("{NEEDLE}https://4chanarchives.com/board/c/thread/100\">");
        let (origin, server) = serve(vec![html(&listing), html(PAGE), html("<html></html>")]);
        let http = http("4chanarchives", origin);
        let files = media(extract(
            &http,
            Url::parse("https://4chanarchives.com/board/c/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /board/c/1 "));
        assert!(requests[1].starts_with("GET /board/c/thread/100 "));
        assert!(requests[2].starts_with("GET /board/c/2 "));
    }

    #[tokio::test]
    async fn missing_thread_is_an_error() {
        let (origin, server) = serve(vec![crate::sites::testing::response(
            "404 Not Found",
            "text/html",
            b"gone",
        )]);
        let http = http("4chanarchives", origin);
        let events = extract(
            &http,
            Url::parse("https://4chanarchives.com/board/c/thread/5").unwrap(),
        );
        let results: Vec<_> = futures_util::StreamExt::collect(events).await;
        assert_eq!(results.len(), 1);
        assert!(results[0].is_err());
        assert!(server.join().unwrap()[0].starts_with("GET /board/c/thread/5 "));
    }
}
