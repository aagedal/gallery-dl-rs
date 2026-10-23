//! 2ch threads and boards, mirroring gallery_dl/extractor/2ch.py through the
//! board JSON API.
use super::{component, datetime_string, digits, host_in, on_root, rest, rpartition};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

static THREAD: Route = Route::new(
    "thread",
    "{tim}{filename:? //}.{extension}",
    &["{category}", "{board}", "{thread} {title}"],
    "{board}_{thread}_{tim}",
);
static BOARD: Route = Route::new("board", "{filename}.{extension}", &["{category}"], "");

pub static SITE: Site = Site {
    category: "2ch",
    name: "2ch",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&THREAD, &BOARD],
    api_key: false,
};

#[derive(Debug, PartialEq)]
enum Target {
    Thread {
        root: String,
        board: String,
        thread: String,
    },
    Board {
        root: String,
        board: String,
    },
}

/// The instance root; `2ch.hk` is served from `2ch.org`.
fn root_for(host: &str) -> Option<String> {
    let tld = host.strip_prefix("2ch.")?;
    match tld {
        "hk" => Some("https://2ch.org".into()),
        "org" | "su" | "life" => Some(format!("https://2ch.{tld}")),
        _ => None,
    }
}

fn target(url: &Url) -> Option<Target> {
    if !host_in(url, &["2ch.org", "2ch.su", "2ch.life", "2ch.hk"]) {
        return None;
    }
    let root = root_for(url.host_str()?)?;
    let (board, tail) = component(rest(url))?;
    if let Some((thread, _)) = tail.strip_prefix("/res/").and_then(digits) {
        return Some(Target::Thread {
            root,
            board,
            thread: thread.into(),
        });
    }
    matches!(tail, "" | "/").then_some(Target::Board { root, board })
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Thread {
            root,
            board,
            thread,
        }) => crate::sites::once(async move {
            let api = http.api(&format!("{root}/{board}/res/{thread}.json"))?;
            let body = http.json(&api).await?;
            let items = parse_thread(&root, &board, &thread, &body)?;
            Ok(crate::sites::events(crate::sites::post(
                &SITE, http, &THREAD, items,
            )))
        }),
        Some(Target::Board { root, board }) => crate::sites::once(async move {
            let base = format!("{root}/{board}");
            let api = http.api(&format!("{base}/index.json"))?;
            let index = http.json(&api).await?;
            let mut children = children(&base, &index)?;
            let pages = index
                .get("pages")
                .and_then(Value::as_array)
                .ok_or_else(|| error("2ch index response lacks its page list"))?;
            // `util.advance(index["pages"], 1)`
            for number in pages.iter().skip(1) {
                let api = http.api(&format!("{base}/{}.json", crate::sites::scalar(number)))?;
                let page = http.json(&api).await?;
                children.extend(self::children(&base, &page)?);
            }
            Ok(super::queue_all(http, &SITE, &BOARD, children))
        }),
        None => crate::sites::fail(error("unsupported 2ch URL")),
    }
}

/// Thread URLs of an index or page response; every child carries the whole
/// response as its data, like upstream.
fn children(base: &str, page: &Value) -> Result<Vec<(Url, Map<String, Value>)>> {
    let data = page
        .as_object()
        .ok_or_else(|| error("2ch board response is not an object"))?;
    let threads = data
        .get("threads")
        .and_then(Value::as_array)
        .ok_or_else(|| error("2ch board response lacks its thread list"))?;
    Ok(threads
        .iter()
        .filter_map(|thread| {
            let num = thread.get("thread_num").map(crate::sites::scalar)?;
            digits(&num).filter(|(_, tail)| tail.is_empty())?;
            Some((
                Url::parse(&format!("{base}/res/{num}.html")).ok()?,
                data.clone(),
            ))
        })
        .collect())
}

fn parse_thread(root: &str, board: &str, thread: &str, body: &Value) -> Result<Vec<Item>> {
    let posts = body
        .get("threads")
        .and_then(|threads| threads.get(0))
        .and_then(|thread| thread.get("posts"))
        .and_then(Value::as_array)
        .filter(|posts| !posts.is_empty())
        .ok_or_else(|| error("2ch thread response contains no posts"))?;
    let op = &posts[0];
    let title = match op.get("subject").and_then(Value::as_str) {
        Some(subject) if !subject.is_empty() => subject.to_owned(),
        _ => crate::sites::remove_html(
            op.get("comment")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        ),
    };
    let title = super::truncate(&crate::sites::unescape(&title), 50);
    let mut items = Vec::new();
    for post in posts {
        let Some(mut post) = post.as_object().cloned() else {
            continue;
        };
        let files = post.remove("files");
        let Some(Value::Array(files)) = files.filter(|files| super::truthy(Some(files))) else {
            continue;
        };
        let timestamp = post
            .get("timestamp")
            .and_then(Value::as_i64)
            .ok_or_else(|| error("2ch post lacks its timestamp"))?;
        let post_name = post.remove("name").unwrap_or(Value::Null);
        post.insert("post_name".into(), post_name);
        post.insert("date".into(), datetime_string(timestamp).into());
        for file in files {
            let Some(mut file) = file.as_object().cloned() else {
                continue;
            };
            file.insert("board".into(), board.into());
            file.insert("thread".into(), thread.into());
            file.insert("title".into(), title.clone().into());
            file.extend(post.clone());
            let text = |key: &str| {
                file.get(key)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            let filename = rpartition(&text("fullname"), ".").0.to_owned();
            let stored = text("name");
            let (tim, extension) = rpartition(&stored, ".");
            let (tim, extension) = (tim.to_owned(), extension.to_owned());
            let Some(url) = on_root(root, &text("path")) else {
                continue;
            };
            file.insert("filename".into(), filename.into());
            file.insert("tim".into(), tim.clone().into());
            file.insert("extension".into(), extension.into());
            let user = file
                .get("post_name")
                .and_then(Value::as_str)
                .map(str::to_owned);
            items.push(
                Item::new(url, file)
                    .ids(thread, tim)
                    .user(user)
                    .timestamp(Some(timestamp))
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
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://2ch.hk/a/res/6202876.html"),
            Some(Target::Thread {
                root: "https://2ch.org".into(),
                board: "a".into(),
                thread: "6202876".into()
            })
        );
        assert_eq!(
            t("https://2ch.life/a/"),
            Some(Target::Board {
                root: "https://2ch.life".into(),
                board: "a".into()
            })
        );
        assert_eq!(
            t("https://2ch.su/b"),
            Some(Target::Board {
                root: "https://2ch.su".into(),
                board: "b".into()
            })
        );
        for raw in [
            "https://2ch.net/a/",
            "https://2ch.org/",
            "https://2ch.org/a/res/x.html",
            "https://2ch.org/a/catalog",
            "https://2ch.org/a/?x=1",
            "https://2ch.org.evil.test/a/",
            "https://2ch.org:8443/a/",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn thread() -> Value {
        json!({"threads": [{"posts": [
            {"num": 100, "name": "Anon", "timestamp": 1672628645, "subject": "",
             "comment": "<b>Op</b> &amp; text",
             "files": [{"fullname": "my pic.jpg", "name": "1672628645123.jpg",
                        "path": "/a/src/100/1672628645123.jpg", "size": 5}]},
            {"num": 101, "name": "Bob", "timestamp": 1672628700, "comment": "no files",
             "files": []},
            {"num": 102, "name": "Eve", "timestamp": 1672628800,
             "files": [{"fullname": "x.webm", "name": "1672628800456.webm",
                        "path": "/a/src/100/1672628800456.webm"},
                       {"fullname": "evil", "name": "1.png", "path": "//evil.test/1.png"}]},
        ]}]})
    }

    #[tokio::test]
    async fn extracts_thread_files() {
        let (origin, server) = serve(vec![json(thread())]);
        let http = http("2ch", origin);
        let files = media(extract(
            &http,
            Url::parse("https://2ch.hk/a/res/100.html").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].url.as_str(),
            "https://2ch.org/a/src/100/1672628645123.jpg"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["filename"], "my pic");
        assert_eq!(k["tim"], "1672628645123");
        assert_eq!(k["extension"], "jpg");
        assert_eq!(k["post_name"], "Anon");
        assert_eq!(k["date"], "2023-01-02 03:04:05");
        assert_eq!(k["title"], "Op & text");
        assert_eq!(k["thread"], "100");
        assert!(!k.contains_key("files"));
        // The file's own `name` (the stored file name) survives the merge.
        assert_eq!(k["name"], "1672628645123.jpg");
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "1672628800456 x.webm");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("2ch/a/100 Op & text")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(
            identity,
            r#"["{board}_{thread}_{tim}",["a","100","1672628645123"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /a/res/100.json "));
    }

    #[tokio::test]
    async fn thread_without_posts_is_an_error() {
        let (origin, _server) = serve(vec![json(json!({"threads": [{"posts": []}]}))]);
        let http = http("2ch", origin);
        let events = extract(&http, Url::parse("https://2ch.org/a/res/100.html").unwrap());
        let results: Vec<_> = futures_util::StreamExt::collect(events).await;
        assert!(results[0].is_err());
    }

    #[tokio::test]
    async fn board_follows_index_and_later_pages() {
        let (origin, server) = serve(vec![
            json(json!({"pages": [0, 1], "threads": [{"thread_num": 100}]})),
            json(json!({"threads": [{"thread_num": 200}]})),
            json(thread()),
            json(
                json!({"threads": [{"posts": [{"num": 200, "name": "A", "timestamp": 5,
                "subject": "Two", "files": [{"fullname": "y.png", "name": "9.png",
                "path": "/a/src/200/9.png"}]}]}]}),
            ),
        ]);
        let http = http("2ch", origin);
        let files = media(extract(&http, Url::parse("https://2ch.org/a/").unwrap())).await;
        assert_eq!(files.len(), 3);
        assert_eq!(keywords(&files[2])["title"], "Two");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /a/index.json "));
        assert!(requests[1].starts_with("GET /a/1.json "));
        assert!(requests[2].starts_with("GET /a/res/100.json "));
        assert!(requests[3].starts_with("GET /a/res/200.json "));
    }
}
