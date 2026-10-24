//! Futaba Channel (2chan) threads, mirroring gallery_dl/extractor/2chan.py
//! by scraping the Shift_JIS thread page.
use super::{component, digits, rest};
use crate::error;
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

static THREAD: Route = Route::new(
    "thread",
    "{tim}.{extension}",
    &["{category}", "{board_name}", "{thread}"],
    "{board}_{thread}_{tim}",
);

pub static SITE: Site = Site {
    category: "2chan",
    name: "2chan",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&THREAD],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
struct Target {
    server: String,
    board: String,
    thread: String,
}

/// `([\w-]+)\.2chan\.net/([^/?#]+)/res/(\d+)`.
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) {
        return None;
    }
    let server = url.host_str()?.strip_suffix(".2chan.net")?;
    if server.is_empty()
        || !server
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return None;
    }
    let (board, tail) = component(rest(url))?;
    let (thread, _) = tail.strip_prefix("/res/").and_then(digits)?;
    Some(Target {
        server: server.into(),
        board,
        thread: thread.into(),
    })
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported 2chan URL"));
    };
    crate::sites::once(async move {
        let Target {
            server,
            board,
            thread,
        } = target;
        let url = http.api(&format!(
            "https://{server}.2chan.net/{board}/res/{thread}.htm"
        ))?;
        // Pages are Shift_JIS, as their `Content-Type` declares.
        let body = http.get(&url, &[]).await?;
        let page = encoding_rs::SHIFT_JIS.decode(&body).0;
        let items = parse_thread(&page, &server, &board, &thread);
        Ok(crate::sites::events(crate::sites::post(
            &SITE, http, &THREAD, items,
        )))
    })
}

/// gallery-dl `text.extract`: the text between `begin` and `end` at or
/// after `pos`, and the position after `end`.
fn extract_at<'t>(text: &'t str, begin: &str, end: &str, pos: usize) -> Option<(&'t str, usize)> {
    let first = pos + text.get(pos..)?.find(begin)? + begin.len();
    let last = first + text[first..].find(end)?;
    Some((&text[first..last], last + end.len()))
}

/// gallery-dl `text.extract_all`: sequential rules; a miss stores `None`
/// and keeps the position.
fn extract_all(text: &str, rules: &[(&str, &str, &str)], data: &mut Map<String, Value>) {
    let mut pos = 0;
    for (key, begin, end) in rules {
        let value = match extract_at(text, begin, end, pos) {
            Some((value, next)) => {
                pos = next;
                Value::from(value)
            }
            None => Value::Null,
        };
        if !key.is_empty() {
            data.insert((*key).into(), value);
        }
    }
}

/// `metadata`: the thread title and board name from `<title>`.
pub(crate) fn metadata(page: &str, server: &str, board: &str, thread: &str) -> Map<String, Value> {
    let title = crate::sites::extr(page, "<title>", "</title>").unwrap_or_default();
    let (title, board_name) = super::rpartition(title, " - ");
    let count = board_name.chars().count().saturating_sub(4);
    let mut data = Map::new();
    data.insert("server".into(), server.into());
    data.insert("title".into(), title.into());
    data.insert("board".into(), board.into());
    data.insert(
        "board_name".into(),
        super::truncate(board_name, count).into(),
    );
    data.insert("thread".into(), thread.into());
    data
}

/// `parse`: one post's fields, and its file's when it has one.
pub(crate) fn parse_post(post: &str) -> Map<String, Value> {
    let mut data = Map::new();
    extract_all(
        post,
        &[
            ("post", "class=\"csb\">", "<"),
            ("name", "class=\"cnm\">", "<"),
            ("now", "class=\"cnw\">", "<"),
            ("no", "class=\"cno\">No.", "<"),
            ("", "<blockquote", ""),
            ("com", ">", "</blockquote>"),
        ],
        &mut data,
    );
    if let Some(Value::String(name)) = data.get_mut("name") {
        *name = name.trim().to_owned();
    }
    let path = crate::sites::extr(post, "<a href=\"/", "\"").unwrap_or_default();
    if !path.is_empty() && !path.starts_with("bin/jump") {
        extract_all(
            post,
            &[
                ("", "_blank", ""),
                ("filename", ">", "<"),
                ("fsize", "(", " "),
            ],
            &mut data,
        );
        if let Some(filename) = data.get("filename").and_then(Value::as_str) {
            let (tim, extension) = filename.split_once('.').unwrap_or((filename, ""));
            let time: String = {
                let count = tim.chars().count().saturating_sub(3);
                super::truncate(tim, count)
            };
            let (tim, extension) = (tim.to_owned(), extension.to_owned());
            data.insert("tim".into(), tim.into());
            data.insert("time".into(), time.into());
            data.insert("ext".into(), format!(".{extension}").into());
            data.insert("extension".into(), extension.into());
        }
    }
    data
}

pub(crate) fn parse_thread(page: &str, server: &str, board: &str, thread: &str) -> Vec<Item> {
    let data = metadata(page, server, board, thread);
    let body = crate::sites::extr(
        page,
        "<div class=\"thre\"",
        "<div style=\"clear:left\"></div>",
    )
    .unwrap_or_default();
    let mut items = Vec::new();
    for post in body.split("<table border=0>") {
        let mut post = parse_post(post);
        let Some(filename) = post.get("filename").and_then(Value::as_str) else {
            continue;
        };
        let tim = post.get("tim").and_then(Value::as_str).unwrap_or_default();
        if !filename
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.')
            || tim.is_empty()
        {
            continue;
        }
        let Ok(url) = Url::parse(&format!(
            "https://{server}.2chan.net/{board}/src/{filename}"
        )) else {
            continue;
        };
        let tim = tim.to_owned();
        post.extend(data.clone());
        let name = post.get("name").and_then(Value::as_str).map(str::to_owned);
        let title = post.get("title").and_then(Value::as_str).map(str::to_owned);
        let time = post
            .get("time")
            .and_then(Value::as_str)
            .and_then(|t| t.parse().ok());
        items.push(
            Item::new(url, post)
                .ids(thread, tim)
                .user(name)
                .caption(title)
                .timestamp(time),
        );
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http, keywords, media, response, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://dec.2chan.net/70/res/14565.htm"),
            Some(Target {
                server: "dec".into(),
                board: "70".into(),
                thread: "14565".into()
            })
        );
        assert!(t("http://may-b.2chan.net/b/res/1.htm#x").is_some());
        for raw in [
            "https://2chan.net/70/res/1.htm",
            "https://a.b.2chan.net/70/res/1.htm",
            "https://dec.2chan.net/70/",
            "https://dec.2chan.net/70/futaba.htm",
            "https://dec.2chan.net.evil.test/70/res/1.htm",
            "https://dec.evil2chan.net/70/res/1.htm",
            "https://dec.2chan.net:8443/70/res/1.htm",
            "https://u@dec.2chan.net/70/res/1.htm",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
        let find = |raw: &str| crate::sites::find(&Url::parse(raw).unwrap()).map(|s| s.category);
        assert_eq!(find("https://dec.2chan.net/70/res/1.htm"), Some("2chan"));
    }

    const PAGE: &str = r#"<html><head><title>スレ題 - 二次元裏＠ふたば</title></head><body>
<div class="thre" data-res="100">画像ファイル名：<a href="/b/src/1700000000123.jpg" target="_blank">1700000000123.jpg</a>-(12345 B)
<span class="csb">無念</span>Name<span class="cnm"> としあき </span><span class="cnw">23/11/15(水)07:13:20</span><span class="cno">No.100</span>
<blockquote>本文<br>二行目</blockquote>
<table border=0><tr><td class=rtd><span class="csb">無念</span><span class="cnm">としあき</span><span class="cnw">23/11/15(水)07:14:00</span><span class="cno">No.101</span><blockquote style="x">返信</blockquote></td></tr></table>
<table border=0><tr><td class=rtd><span class="cno">No.102</span><a href="/bin/jump.php?x">link</a><blockquote>j</blockquote></td></tr></table>
<table border=0><tr><td class=rtd><span class="cno">No.103</span><a href="/b/src/1700000000456.webm" target="_blank">1700000000456.webm</a>-(999 B)<blockquote>v</blockquote></td></tr></table>
<div style="clear:left"></div></body></html>"#;

    #[tokio::test]
    async fn extracts_thread_files() {
        let (body, _, _) = encoding_rs::SHIFT_JIS.encode(PAGE);
        let (root, server) = serve(vec![response(
            "200 OK",
            "text/html; charset=Shift_JIS",
            &body,
        )]);
        let http = http("2chan", root);
        let files = media(extract(
            &http,
            Url::parse("https://img.2chan.net/b/res/100.htm").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].url.as_str(),
            "https://img.2chan.net/b/src/1700000000123.jpg"
        );
        assert_eq!(
            files[1].url.as_str(),
            "https://img.2chan.net/b/src/1700000000456.webm"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["title"], "スレ題");
        assert_eq!(k["board_name"], "二次元裏");
        assert_eq!(k["name"], "としあき");
        assert_eq!(k["post"], "無念");
        assert_eq!(k["now"], "23/11/15(水)07:13:20");
        assert_eq!(k["no"], "100");
        assert_eq!(k["com"], "本文<br>二行目");
        assert_eq!(k["filename"], "1700000000123.jpg");
        assert_eq!(k["fsize"], "12345");
        assert_eq!(k["tim"], "1700000000123");
        assert_eq!(k["time"], "1700000000");
        assert_eq!(k["ext"], ".jpg");
        assert_eq!(k["extension"], "jpg");
        assert_eq!(k["server"], "img");
        let k = keywords(&files[1]);
        assert_eq!(k["no"], "103");
        assert_eq!(k["name"], Value::Null);
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "1700000000456.webm");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("2chan/二次元裏/100")
        );
        let (namespace, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(namespace, "2chan");
        assert_eq!(
            identity,
            r#"["{board}_{thread}_{tim}",["b","100","1700000000123"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /b/res/100.htm "));
    }
}
