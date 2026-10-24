//! jschan imageboards (94chan), mirroring gallery_dl/extractor/jschan.py
//! through the board JSON API.
use super::{DIRECTORY, component, digits, host_in, rest};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::Value;
use url::Url;

// Upstream directory: ("{category}", "{board}",
// "{threadId} {subject|nomarkup[:50]}"), rendered from the thread's
// Directory kwdict; files carry it under `_directory`.
static THREAD: Route = Route::new(
    "thread",
    "{postId}{num:?-//} {filename}.{extension}",
    &[
        "{category}",
        "{board}",
        "{threadId} {_directory[subject]|_directory[nomarkup][:50]}",
    ],
    "{board}_{postId}_{num}",
);
static BOARD: Route = Route::new("board", "{filename}.{extension}", &["{category}"], "");

pub static NINETYFOURCHAN: Site = Site {
    category: "94chan",
    name: "94chan",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&THREAD, &BOARD],
    api_key: false,
    cookies: None,
};

const ROOT: &str = "https://94chan.org";

#[derive(Debug, PartialEq)]
enum Target {
    Thread { board: String, thread: String },
    Board { board: String },
}

fn target(url: &Url) -> Option<Target> {
    if !host_in(url, &["94chan.org"]) {
        return None;
    }
    let (board, tail) = component(rest(url))?;
    if let Some((thread, after)) = tail.strip_prefix("/thread/").and_then(digits)
        && after.starts_with(".html")
    {
        return Some(Target::Thread {
            board,
            thread: thread.into(),
        });
    }
    let page = tail
        .strip_prefix('/')
        .and_then(digits)
        .is_some_and(|(_, after)| after.starts_with(".html"));
    (tail.is_empty()
        || tail == "/"
        || tail.starts_with("/index.html")
        || tail.starts_with("/catalog.html")
        || page)
        .then_some(Target::Board { board })
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Thread { board, thread }) => crate::sites::once(async move {
            let api = http.api(&format!("{ROOT}/{board}/thread/{thread}.json"))?;
            let body = http.json(&api).await?;
            let items = parse_thread(&body)?;
            Ok(crate::sites::events(crate::sites::post(
                &NINETYFOURCHAN,
                http,
                &THREAD,
                items,
            )))
        }),
        Some(Target::Board { board }) => crate::sites::once(async move {
            let api = http.api(&format!("{ROOT}/{board}/catalog.json"))?;
            let body = http.json(&api).await?;
            let threads = body
                .as_array()
                .ok_or_else(|| error("jschan catalog is not a thread list"))?;
            let children = threads
                .iter()
                .filter_map(|thread| {
                    let data = thread.as_object()?.clone();
                    let id = data.get("postId").map(crate::sites::scalar)?;
                    digits(&id).filter(|(_, tail)| tail.is_empty())?;
                    let url = Url::parse(&format!("{ROOT}/{board}/thread/{id}.html")).ok()?;
                    Some((url, data))
                })
                .collect();
            Ok(super::queue_all(http, &NINETYFOURCHAN, &BOARD, children))
        }),
        None => crate::sites::fail(error("unsupported jschan URL")),
    }
}

fn parse_thread(body: &Value) -> Result<Vec<Item>> {
    let mut thread = body
        .as_object()
        .cloned()
        .ok_or_else(|| error("jschan thread response is not an object"))?;
    let post_id = thread.get("postId").cloned().unwrap_or(Value::Null);
    thread.insert("threadId".into(), post_id.clone());
    let posts = match thread.remove("replies") {
        Some(Value::Array(posts)) => posts,
        _ => Vec::new(),
    };
    let directory = super::directory_fields(&thread, &["subject", "nomarkup"]);
    let mut items = Vec::new();
    for index in 0..=posts.len() {
        let files = if index == 0 {
            thread.remove("files")
        } else {
            let Some(mut post) = posts[index - 1].as_object().cloned() else {
                continue;
            };
            let files = post.remove("files");
            if super::truthy(files.as_ref()) {
                thread.extend(post);
            }
            files
        };
        let Some(Value::Array(files)) = files else {
            continue;
        };
        if files.is_empty() {
            continue;
        }
        thread.insert("count".into(), files.len().into());
        for (num, file) in files.iter().enumerate() {
            let Some(mut file) = file.as_object().cloned() else {
                continue;
            };
            let name = file
                .get("filename")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
            {
                continue;
            }
            let Ok(url) = Url::parse(&format!("{ROOT}/file/{name}")) else {
                continue;
            };
            file.extend(thread.clone());
            file.insert("num".into(), num.into());
            let site_filename = file.get("filename").cloned().unwrap_or(Value::Null);
            file.insert("siteFilename".into(), site_filename);
            let original = file
                .get("originalFilename")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            super::set_nameext(&mut file, &original);
            file.insert(DIRECTORY.into(), directory.clone());
            let id = file
                .get("postId")
                .map(crate::sites::scalar)
                .unwrap_or_default();
            let date = file
                .get("date")
                .and_then(Value::as_str)
                .and_then(crate::source_date::parse_string);
            let user = file.get("name").and_then(Value::as_str).map(str::to_owned);
            let caption = file
                .get("nomarkup")
                .and_then(Value::as_str)
                .map(str::to_owned);
            items.push(
                Item::new(url, file)
                    .ids(crate::sites::scalar(&post_id), format!("{id}_{num}"))
                    .user(user)
                    .caption(caption)
                    .timestamp(date)
                    .header("Referer", &format!("{ROOT}/")),
            );
        }
    }
    Ok(items)
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
            t("https://94chan.org/art/thread/25.html"),
            Some(Target::Thread {
                board: "art".into(),
                thread: "25".into()
            })
        );
        for raw in [
            "https://94chan.org/art/",
            "https://94chan.org/art",
            "https://94chan.org/art/2.html",
            "https://94chan.org/art/catalog.html",
            "https://94chan.org/art/index.html",
        ] {
            assert_eq!(
                t(raw),
                Some(Target::Board {
                    board: "art".into()
                }),
                "{raw}"
            );
        }
        for raw in [
            "https://94chan.org/art/thread/25",
            "https://94chan.org/art/2",
            "https://94chan.org/art/catalog",
            "https://www.94chan.org/art/",
            "https://94chan.org.evil.test/art/",
            "https://94chan.org:444/art/",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn thread() -> Value {
        json!({"postId": 25, "board": "art", "subject": "Gallery", "nomarkup": "op",
        "date": "2023-05-06T07:08:09.000Z", "name": "Anon",
        "files": [{"filename": "abc123.png", "originalFilename": "Orig.PNG", "size": 1}],
        "replies": [
            {"postId": 26, "nomarkup": "reply", "subject": null,
             "files": [{"filename": "def456.jpg", "originalFilename": "r.jpg"},
                       {"filename": "../x", "originalFilename": "bad.jpg"}]},
        ]})
    }

    #[tokio::test]
    async fn extracts_thread_files() {
        let (origin, server) = serve(vec![json(thread())]);
        let http = http("94chan", origin);
        let files = media(extract(
            &http,
            Url::parse("https://94chan.org/art/thread/25.html").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].url.as_str(), "https://94chan.org/file/abc123.png");
        let k = keywords(&files[0]);
        assert_eq!(k["threadId"], 25);
        assert_eq!(k["siteFilename"], "abc123.png");
        assert_eq!(k["filename"], "Orig");
        assert_eq!(k["extension"], "png");
        assert_eq!(k["count"], 1);
        assert_eq!(k["date"], "2023-05-06T07:08:09.000Z");
        let k = keywords(&files[1]);
        assert_eq!(k["postId"], 26);
        assert_eq!(k["count"], 2);
        assert_eq!(k["nomarkup"], "reply");
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "26 r.jpg");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("94chan/art/25 Gallery")
        );
        let (_, identity) = crate::sites::archive_key(&files[1]).unwrap();
        assert_eq!(identity, r#"["{board}_{postId}_{num}",["art","26","0"]]"#);
        assert_eq!(
            files[0].metadata.as_ref().unwrap().timestamp,
            Some(1683356889)
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /art/thread/25.json "));
    }

    #[tokio::test]
    async fn board_queues_catalog_threads() {
        let (origin, server) = serve(vec![json(json!([{"postId": 25}])), json(thread())]);
        let http = http("94chan", origin);
        let files = media(extract(
            &http,
            Url::parse("https://94chan.org/art/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /art/catalog.json "));
        assert!(requests[1].starts_with("GET /art/thread/25.json "));
    }

    #[tokio::test]
    async fn missing_thread_is_an_error() {
        let (origin, server) = serve(vec![crate::sites::testing::response(
            "404 Not Found",
            "text/html",
            b"gone",
        )]);
        let http = http("94chan", origin);
        let events = extract(
            &http,
            Url::parse("https://94chan.org/a/thread/5.html").unwrap(),
        );
        let results: Vec<_> = futures_util::StreamExt::collect(events).await;
        assert_eq!(results.len(), 1);
        assert!(results[0].is_err());
        assert!(server.join().unwrap()[0].starts_with("GET /a/thread/5.json"));
    }
}
