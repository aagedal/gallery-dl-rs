//! 4chan threads and boards, mirroring gallery_dl/extractor/4chan.py through
//! the read-only JSON API on `a.4cdn.org`.
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

pub static SITE: Site = Site {
    category: "4chan",
    name: "4chan",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&THREAD, &BOARD],
    api_key: false,
};

static THREAD: Route = Route::new(
    "thread",
    "{tim} {filename}.{extension}",
    &["{category}", "{board}", "{thread} {title}"],
    "{board}_{thread}_{tim}",
);
static BOARD: Route = Route::new("board", "{filename}.{extension}", &["{category}"], "");

#[derive(Debug, PartialEq)]
enum Target {
    Thread { board: String, thread: String },
    Board { board: String },
}

fn valid_board(board: &str) -> bool {
    (1..=32).contains(&board.len()) && board.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url)
        || !crate::sites::host_is(url, &["boards.4chan.org", "boards.4channel.org"])
    {
        return None;
    }
    let parts = crate::sites::segments(url);
    match parts.as_slice() {
        [board, kind, thread, ..]
            if kind == "thread"
                && valid_board(board)
                && (1..=20).contains(&thread.len())
                && thread.bytes().all(|b| b.is_ascii_digit()) =>
        {
            Some(Target::Thread {
                board: board.clone(),
                thread: thread.clone(),
            })
        }
        // `/BOARD/` and `/BOARD/PAGE`.
        [board] | [board, _]
            if valid_board(board)
                && parts
                    .get(1)
                    .is_none_or(|page| page.bytes().all(|b| b.is_ascii_digit()))
                && url.query().is_none() =>
        {
            Some(Target::Board {
                board: board.clone(),
            })
        }
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Thread { board, thread }) => {
            crate::sites::once(async move { thread_events(http, &board, &thread).await })
        }
        Some(Target::Board { board }) => {
            crate::sites::once(async move { board_events(http, &board).await })
        }
        None => crate::sites::fail(error("unsupported 4chan URL")),
    }
}

/// Parse a thread response into its files, in post order.
pub(crate) fn parse_thread(body: &Value, board: &str, thread: &str) -> Result<Vec<Item>> {
    let posts = body
        .get("posts")
        .and_then(Value::as_array)
        .filter(|posts| !posts.is_empty())
        .ok_or_else(|| error("4chan thread response contains no posts"))?;
    let op = &posts[0];
    let title = op
        .get("sub")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            crate::sites::remove_html(op.get("com").and_then(Value::as_str).unwrap_or_default())
        });
    let title: String = crate::sites::unescape(&title).chars().take(50).collect();
    let mut items = Vec::new();
    for post in posts {
        let Some(fields) = post.as_object() else {
            continue;
        };
        if !fields.contains_key("filename") {
            continue;
        }
        let tim = fields
            .get("tim")
            .map(crate::sites::scalar)
            .unwrap_or_default();
        let ext = fields
            .get("ext")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if tim.is_empty()
            || !tim.bytes().all(|b| b.is_ascii_digit())
            || !ext.starts_with('.')
            || !ext[1..].bytes().all(|b| b.is_ascii_alphanumeric())
        {
            continue;
        }
        let Ok(url) = Url::parse(&format!("https://i.4cdn.org/{board}/{tim}{ext}")) else {
            continue;
        };
        let mut keywords: Map<String, Value> = fields.clone();
        keywords.insert("board".into(), board.into());
        keywords.insert("thread".into(), thread.into());
        keywords.insert("title".into(), title.clone().into());
        keywords.insert("extension".into(), ext[1..].into());
        keywords.insert(
            "filename".into(),
            crate::sites::unescape(
                fields
                    .get("filename")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            )
            .into(),
        );
        let no = fields
            .get("no")
            .map(crate::sites::scalar)
            .unwrap_or_default();
        items.push(
            Item::new(url, keywords)
                .ids(thread, tim)
                .user(
                    fields
                        .get("name")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                )
                .caption(Some(title.clone()))
                .timestamp(fields.get("time").and_then(Value::as_i64))
                .header(
                    "Referer",
                    &format!("https://boards.4chan.org/{board}/thread/{thread}#p{no}"),
                ),
        );
    }
    Ok(items)
}

async fn thread_events<'a>(http: &'a Http, board: &str, thread: &str) -> Result<Events<'a>> {
    let url = http.api(&format!("https://a.4cdn.org/{board}/thread/{thread}.json"))?;
    let body = http.json(&url).await?;
    let items = parse_thread(&body, board, thread)?;
    Ok(crate::sites::events(crate::sites::post(
        &SITE, http, &THREAD, items,
    )))
}

async fn board_events<'a>(http: &'a Http, board: &str) -> Result<Events<'a>> {
    let url = http.api(&format!("https://a.4cdn.org/{board}/threads.json"))?;
    let body = http.json(&url).await?;
    let pages = body
        .as_array()
        .ok_or_else(|| error("4chan board response is not a page list"))?;
    let mut children = Vec::new();
    for page in pages {
        let number = page.get("page").cloned().unwrap_or(Value::Null);
        for thread in page
            .get("threads")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(no) = thread.get("no").and_then(Value::as_u64) else {
                continue;
            };
            let mut data = thread.as_object().cloned().unwrap_or_default();
            data.insert("page".into(), number.clone());
            data.insert("id".into(), no.into());
            let child = Url::parse(&format!("https://boards.4chan.org/{board}/thread/{no}/"))
                .map_err(|_| error("invalid 4chan thread URL"))?;
            children.push((child, data));
        }
    }
    Ok(futures_util::StreamExt::boxed(
        futures_util::StreamExt::flatten(futures_util::stream::iter(
            children.into_iter().map(move |(child, data)| {
                crate::sites::queue(http, "4chan", "board", child, &data)
            }),
        )),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http, json, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://boards.4channel.org/a/thread/12345/some-title"),
            Some(Target::Thread {
                board: "a".into(),
                thread: "12345".into()
            })
        );
        assert_eq!(
            t("https://boards.4chan.org/wg/"),
            Some(Target::Board { board: "wg".into() })
        );
        assert_eq!(
            t("https://boards.4chan.org/wg/2"),
            Some(Target::Board { board: "wg".into() })
        );
        for raw in [
            "https://boards.4chan.org/",
            "https://boards.4chan.org/a/catalog",
            "https://i.4cdn.org/a/1.jpg",
            "https://boards.4chan.org.evil.test/a/thread/1",
            "https://boards.4chan.org:8443/a/thread/1",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn thread() -> Value {
        json!({"posts": [
            {"no": 100, "time": 1500000000, "name": "Anonymous", "com": "<b>Op</b> &amp; text",
             "filename": "a&amp;b", "ext": ".jpg", "tim": 1500000000123u64, "w": 10, "h": 20,
             "fsize": 1234, "md5": "abc=="},
            {"no": 101, "time": 1500000100, "com": "no file"},
            {"no": 102, "time": 1500000200, "filename": "clip", "ext": ".webm",
             "tim": 1500000200456u64},
        ]})
    }

    #[tokio::test]
    async fn extracts_thread_files_with_upstream_keywords() {
        let (root, server) = serve(vec![json(thread())]);
        let http = http("4chan", root);
        let files = media(extract(
            &http,
            Url::parse("https://boards.4chan.org/a/thread/100").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].url.as_str(),
            "https://i.4cdn.org/a/1500000000123.jpg"
        );
        assert_eq!(files[0].filename, "a&b.jpg");
        let k = keywords(&files[0]);
        assert_eq!(k["title"], "Op & text");
        assert_eq!(k["board"], "a");
        assert_eq!(k["thread"], "100");
        assert_eq!(k["extension"], "jpg");
        assert_eq!(keywords(&files[1])["extension"], "webm");
        let (namespace, identity) = crate::sites::archive_key(&files[1]).unwrap();
        assert_eq!(namespace, "4chan");
        assert_eq!(
            identity,
            r#"["{board}_{thread}_{tim}",["a","100","1500000200456"]]"#
        );
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "1500000000123 a&b.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("4chan/a/100 Op & text")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /a/thread/100.json "));
    }

    #[tokio::test]
    async fn board_queues_threads_lazily() {
        let (root, server) = serve(vec![
            json(json!([{"page": 1, "threads": [{"no": 100}, {"no": 200}]}])),
            json(thread()),
            json(
                json!({"posts": [{"no": 200, "sub": "Second", "filename": "x",
                "ext": ".png", "tim": 9}]}),
            ),
        ]);
        let http = http("4chan", root);
        let files = media(extract(
            &http,
            Url::parse("https://boards.4chan.org/a/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 3);
        assert_eq!(keywords(&files[2])["title"], "Second");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /a/threads.json "));
        assert!(requests[2].starts_with("GET /a/thread/200.json "));
    }
}
