//! LynxChan imageboards (bbw-chan, kohlchan, endchan), mirroring
//! gallery_dl/extractor/lynxchan.py through the board JSON API.
use super::{DIRECTORY, component, digits, host_in, rest};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::Value;
use url::Url;

// Upstream directory: ("{category}", "{boardUri}",
// "{threadId} {subject|message[:50]}"), rendered from the thread's
// Directory kwdict; files carry it under `_directory`.
static THREAD: Route = Route::new(
    "thread",
    "{postId}{num:?-//} {filename}.{extension}",
    &[
        "{category}",
        "{boardUri}",
        "{threadId} {_directory[subject]|_directory[message][:50]}",
    ],
    "{boardUri}_{postId}_{num}",
);
static BOARD: Route = Route::new("board", "{filename}.{extension}", &["{category}"], "");

macro_rules! instance {
    ($ident:ident, $category:literal, $name:literal) => {
        pub static $ident: Site = Site {
            category: $category,
            name: $name,
            matches: |url| target($category, url).is_some(),
            extract: |http, url| extract($category, http, url),
            routes: &[&THREAD, &BOARD],
            api_key: false,
        };
    };
}
instance!(BBWCHAN, "bbw-chan", "BBW-Chan");
instance!(KOHLCHAN, "kohlchan", "Kohlchan");
instance!(ENDCHAN, "endchan", "Endchan");

fn site(category: &str) -> &'static Site {
    match category {
        "bbw-chan" => &BBWCHAN,
        "kohlchan" => &KOHLCHAN,
        _ => &ENDCHAN,
    }
}

fn hosts(category: &str) -> &'static [&'static str] {
    match category {
        "bbw-chan" => &["bbw-chan.link", "bbw-chan.nl"],
        "kohlchan" => &["kohlchan.net"],
        _ => &["endchan.org", "endchan.net", "endchan.gg"],
    }
}

/// Upstream instance root; `endchan` has none and uses the matched origin.
fn root(category: &str, url: &Url) -> String {
    match category {
        "bbw-chan" => "https://bbw-chan.link".into(),
        "kohlchan" => "https://kohlchan.net".into(),
        _ => format!("{}://{}", url.scheme(), url.host_str().unwrap_or_default()),
    }
}

#[derive(Debug, PartialEq)]
enum Target {
    Thread { board: String, thread: String },
    Board { board: String },
}

fn target(category: &str, url: &Url) -> Option<Target> {
    if !host_in(url, hosts(category)) {
        return None;
    }
    let (board, tail) = component(rest(url))?;
    if let Some((thread, _)) = tail.strip_prefix("/res/").and_then(digits) {
        return Some(Target::Thread {
            board,
            thread: thread.into(),
        });
    }
    super::board_suffix(tail).then_some(Target::Board { board })
}

fn extract<'a>(category: &'static str, http: &'a Http, url: Url) -> Events<'a> {
    let root = root(category, &url);
    match target(category, &url) {
        Some(Target::Thread { board, thread }) => crate::sites::once(async move {
            let api = http.api(&format!("{root}/{board}/res/{thread}.json"))?;
            let body = http.json(&api).await?;
            let items = parse_thread(&root, &body)?;
            Ok(crate::sites::events(crate::sites::post(
                site(category),
                http,
                &THREAD,
                items,
            )))
        }),
        Some(Target::Board { board }) => crate::sites::once(async move {
            let api = http.api(&format!("{root}/{board}/catalog.json"))?;
            let body = http.json(&api).await?;
            let threads = body
                .as_array()
                .ok_or_else(|| error("LynxChan catalog is not a thread list"))?;
            let children = threads
                .iter()
                .filter_map(|thread| {
                    let data = thread.as_object()?.clone();
                    let id = data.get("threadId").map(crate::sites::scalar)?;
                    digits(&id).filter(|(_, tail)| tail.is_empty())?;
                    let url = Url::parse(&format!("{root}/{board}/res/{id}.html")).ok()?;
                    Some((url, data))
                })
                .collect();
            Ok(super::queue_all(http, site(category), &BOARD, children))
        }),
        None => crate::sites::fail(error("unsupported LynxChan URL")),
    }
}

fn parse_thread(root: &str, body: &Value) -> Result<Vec<Item>> {
    let mut thread = body
        .as_object()
        .cloned()
        .ok_or_else(|| error("LynxChan thread response is not an object"))?;
    let thread_id = thread.get("threadId").cloned().unwrap_or(Value::Null);
    thread.insert("postId".into(), thread_id.clone());
    let posts = match thread.remove("posts") {
        Some(Value::Array(posts)) => posts,
        _ => Vec::new(),
    };
    let directory = super::directory_fields(&thread, &["subject", "message"]);
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
        for (num, file) in files.iter().enumerate() {
            let Some(mut file) = file.as_object().cloned() else {
                continue;
            };
            file.extend(thread.clone());
            file.insert("num".into(), num.into());
            let path = file.get("path").and_then(Value::as_str).unwrap_or_default();
            let Some(url) = super::on_root(root, path) else {
                continue;
            };
            let original = file
                .get("originalName")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            super::set_nameext(&mut file, &original);
            file.insert(DIRECTORY.into(), directory.clone());
            let post_id = file
                .get("postId")
                .map(crate::sites::scalar)
                .unwrap_or_default();
            let created = file
                .get("creation")
                .and_then(Value::as_str)
                .and_then(crate::source_date::parse_string);
            let name = file.get("name").and_then(Value::as_str).map(str::to_owned);
            let caption = file
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned);
            items.push(
                Item::new(url, file)
                    .ids(crate::sites::scalar(&thread_id), format!("{post_id}_{num}"))
                    .user(name)
                    .caption(caption)
                    .timestamp(created)
                    .header("Referer", &format!("{root}/")),
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
        let t = |c, raw: &str| target(c, &Url::parse(raw).unwrap());
        assert_eq!(
            t("endchan", "https://endchan.gg/art/res/12345.html"),
            Some(Target::Thread {
                board: "art".into(),
                thread: "12345".into()
            })
        );
        assert_eq!(
            t("kohlchan", "https://kohlchan.net/int/catalog.html"),
            Some(Target::Board {
                board: "int".into()
            })
        );
        assert_eq!(
            t("bbw-chan", "https://bbw-chan.nl/bbwdraw/"),
            Some(Target::Board {
                board: "bbwdraw".into()
            })
        );
        for (c, raw) in [
            ("endchan", "https://endchan.org/"),
            ("endchan", "https://www.endchan.org/a/res/1.html"),
            ("endchan", "https://endchan.org.evil.test/a/res/1.html"),
            ("endchan", "http://endchan.org:81/a/res/1.html"),
            ("kohlchan", "https://kohlchan.net/a/res/x.html"),
            ("kohlchan", "https://endchan.org/a/"),
        ] {
            assert_eq!(t(c, raw), None, "{raw}");
        }
    }

    fn thread() -> Value {
        json!({"threadId": 10, "boardUri": "art", "subject": "Topic", "message": "op text",
        "name": "Anon", "creation": "2023-01-02T03:04:05.000Z",
        "files": [{"originalName": "My Pic.PNG", "path": "/.media/aaa.png", "size": 3}],
        "posts": [
            {"postId": 11, "message": "reply", "subject": null,
             "creation": "2023-01-02T03:05:00.000Z",
             "files": [{"originalName": "a.jpg", "path": "/.media/bbb.jpg"},
                       {"originalName": "b.gif", "path": "/.media/ccc.gif"}]},
            {"postId": 12, "message": "no files", "files": []},
            {"postId": 13, "files": [{"originalName": "x", "path": "//evil.test/x"}]},
        ]})
    }

    #[tokio::test]
    async fn extracts_thread_files() {
        let (origin, server) = serve(vec![json(thread())]);
        let http = http("endchan", origin);
        let files = media(extract(
            "endchan",
            &http,
            Url::parse("https://endchan.org/art/res/10.html").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].url.as_str(), "https://endchan.org/.media/aaa.png");
        let k = keywords(&files[0]);
        assert_eq!(k["postId"], 10);
        assert_eq!(k["filename"], "My Pic");
        assert_eq!(k["extension"], "png");
        assert_eq!(k["num"], 0);
        assert!(!k.contains_key("posts") && !k.contains_key("files"));
        let k = keywords(&files[2]);
        assert_eq!(k["postId"], 11);
        assert_eq!(k["message"], "reply");
        assert_eq!(k["num"], 1);
        assert_eq!(k["boardUri"], "art");
        assert_eq!(
            files[2].metadata.as_ref().unwrap().timestamp,
            Some(1672628700)
        );
        let (file, dir) = crate::sites::default_templates(&files[2]).unwrap();
        assert_eq!(file.render(&files[2]).unwrap(), "11-1 b.gif");
        assert_eq!(file.render(&files[0]).unwrap(), "10 My Pic.png");
        for media in &files {
            assert_eq!(
                dir.render(media).unwrap().unwrap(),
                std::path::PathBuf::from("endchan/art/10 Topic")
            );
        }
        let (_, identity) = crate::sites::archive_key(&files[1]).unwrap();
        assert_eq!(
            identity,
            r#"["{boardUri}_{postId}_{num}",["art","11","0"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /art/res/10.json "));
    }

    #[tokio::test]
    async fn directory_falls_back_to_message() {
        let mut body = thread();
        body["subject"] = Value::Null;
        body["message"] = "a very long message that goes on and on and on and on and on".into();
        let (origin, _server) = serve(vec![json(body)]);
        let http = http("kohlchan", origin);
        let files = media(extract(
            "kohlchan",
            &http,
            Url::parse("https://kohlchan.net/art/res/10").unwrap(),
        ))
        .await;
        let (_, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from(
                "kohlchan/art/10 a very long message that goes on and on and on and"
            )
        );
    }

    #[tokio::test]
    async fn board_queues_catalog_threads() {
        let (origin, server) = serve(vec![
            json(json!([{"threadId": 10, "subject": "Topic"}])),
            json(thread()),
        ]);
        let http = http("kohlchan", origin);
        let files = media(extract(
            "kohlchan",
            &http,
            Url::parse("https://kohlchan.net/art/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].url.as_str(), "https://kohlchan.net/.media/aaa.png");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /art/catalog.json "));
        assert!(requests[1].starts_with("GET /art/res/10.json "));
    }

    #[tokio::test]
    async fn missing_thread_is_an_error() {
        let (origin, server) = serve(vec![crate::sites::testing::response(
            "404 Not Found",
            "text/html",
            b"gone",
        )]);
        let http = http("endchan", origin);
        let events = extract(
            "endchan",
            &http,
            Url::parse("https://endchan.org/art/res/10.html").unwrap(),
        );
        let results: Vec<_> = futures_util::StreamExt::collect(events).await;
        assert_eq!(results.len(), 1);
        assert!(results[0].is_err());
        assert!(server.join().unwrap()[0].starts_with("GET /art/res/10.json"));
    }
}
