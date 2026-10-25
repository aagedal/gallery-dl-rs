//! vichan imageboards (8kun, smugloli, gurochan), mirroring
//! gallery_dl/extractor/vichan.py through the board JSON API.
use super::{component, digits, instance_rest};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

static THREAD: Route = Route::new(
    "thread",
    "{time}{num:?-//} {filename}.{extension}",
    &["{category}", "{board}", "{thread} {title}"],
    "{board}_{thread}_{tim}",
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
            cookies: None,
        };
    };
}
instance!(EIGHTKUN, "8kun", "8kun");
instance!(SMUGLOLI, "smugloli", "smugloli");
instance!(GUROCHAN, "gurochan", "gurochan");

/// Configured `extractor.vichan.<category>` instances.
pub(crate) static FAMILY: crate::sites::instances::Family = crate::sites::instances::Family {
    basecategory: "vichan",
    builtin: &[&EIGHTKUN, &SMUGLOLI, &GUROCHAN],
    related: &[],
    template: &GUROCHAN,
    matches: |category, url| target(category, url).is_some(),
    extract,
};

fn site(category: &str) -> &'static Site {
    if let Some(instance) = crate::sites::instances::configured(&FAMILY, category) {
        return &instance.site;
    }
    match category {
        "8kun" => &EIGHTKUN,
        "smugloli" => &SMUGLOLI,
        _ => &GUROCHAN,
    }
}

fn hosts(category: &str) -> &'static [&'static str] {
    match category {
        "8kun" => &["8kun.top"],
        "smugloli" => &["smuglo.li", "smugloli.net"],
        _ => &["boards.guro.cx"],
    }
}

/// Upstream instance root; `smugloli` has none and uses the matched origin.
fn root(category: &str, url: &Url) -> String {
    if let Some(instance) = crate::sites::instances::configured(&FAMILY, category) {
        return instance.root.into();
    }
    match category {
        "8kun" => "https://8kun.top".into(),
        "gurochan" => "https://boards.guro.cx".into(),
        _ => format!("{}://{}", url.scheme(), url.host_str().unwrap_or_default()),
    }
}

#[derive(Debug, PartialEq)]
enum Target {
    Thread { board: String, thread: String },
    Board { board: String },
}

fn target(category: &str, url: &Url) -> Option<Target> {
    let (board, tail) = component(instance_rest(&FAMILY, category, url, || hosts(category))?)?;
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
            let items = parse_thread(category, &root, &board, &thread, &body)?;
            Ok(crate::sites::events(crate::sites::post(
                site(category),
                http,
                &THREAD,
                items,
            )))
        }),
        Some(Target::Board { board }) => crate::sites::once(async move {
            let api = http.api(&format!("{root}/{board}/threads.json"))?;
            let body = http.json(&api).await?;
            let children = parse_board(&root, &board, &body)?;
            Ok(super::queue_all(http, site(category), &BOARD, children))
        }),
        None => crate::sites::fail(error("unsupported vichan URL")),
    }
}

fn parse_board(root: &str, board: &str, body: &Value) -> Result<Vec<(Url, Map<String, Value>)>> {
    let pages = body
        .as_array()
        .ok_or_else(|| error("vichan board response is not a page list"))?;
    let mut children = Vec::new();
    for page in pages {
        for thread in page
            .get("threads")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(mut data) = thread.as_object().cloned() else {
                continue;
            };
            let no = data.get("no").map(crate::sites::scalar).unwrap_or_default();
            if digits(&no).is_none_or(|(_, tail)| !tail.is_empty()) {
                continue;
            }
            data.insert(
                "page".into(),
                page.get("page").cloned().unwrap_or(Value::Null),
            );
            let Ok(url) = Url::parse(&format!("{root}/{board}/res/{no}.html")) else {
                continue;
            };
            children.push((url, data));
        }
    }
    Ok(children)
}

fn parse_thread(
    category: &str,
    root: &str,
    board: &str,
    thread: &str,
    body: &Value,
) -> Result<Vec<Item>> {
    let posts = body
        .get("posts")
        .and_then(Value::as_array)
        .filter(|posts| !posts.is_empty())
        .ok_or_else(|| error("vichan thread response contains no posts"))?;
    let op = &posts[0];
    let title = match op.get("sub").and_then(Value::as_str) {
        Some(sub) if !sub.is_empty() => sub.to_owned(),
        _ => crate::sites::remove_html(op.get("com").and_then(Value::as_str).unwrap_or_default()),
    };
    let title = super::truncate(&crate::sites::unescape(&title), 50);
    let mut data = Map::new();
    data.insert("board".into(), board.into());
    data.insert("thread".into(), thread.into());
    data.insert("title".into(), title.into());
    data.insert("num".into(), 0.into());

    let mut items = Vec::new();
    for post in posts {
        let Some(post) = post.as_object() else {
            continue;
        };
        if !post.contains_key("filename") {
            continue;
        }
        let mut post = post.clone();
        post.extend(data.clone());
        items.extend(process(category, root, &mut post));
        let extra = post
            .get("extra_files")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for (num, file) in extra.iter().enumerate() {
            post.insert("num".into(), (num + 1).into());
            if let Some(file) = file.as_object() {
                post.extend(file.clone());
            }
            items.extend(process(category, root, &mut post));
        }
    }
    Ok(items)
}

fn process(category: &str, root: &str, post: &mut Map<String, Value>) -> Option<Item> {
    let ext = post.get("ext").and_then(Value::as_str)?.to_owned();
    let tim = post.get("tim").map(crate::sites::scalar)?;
    let board = post.get("board").map(crate::sites::scalar)?;
    if tim.is_empty()
        || !tim.bytes().all(|b| b.is_ascii_alphanumeric())
        || !ext.starts_with('.')
        || !ext[1..].bytes().all(|b| b.is_ascii_alphanumeric())
    {
        return None;
    }
    let raw = if category == "8kun" {
        if tim.chars().count() > 16 {
            format!("https://media.128ducks.com/file_store/{tim}{ext}")
        } else {
            format!("https://media.128ducks.com/{board}/src/{tim}{ext}")
        }
    } else {
        format!("{root}/{board}/src/{tim}{ext}")
    };
    let url = Url::parse(&raw)
        .ok()
        .filter(|url| crate::sites::instances::plain_or_root(url, root))?;
    post.insert("url".into(), raw.into());
    post.insert("extension".into(), ext[1..].into());
    let thread = post
        .get("thread")
        .map(crate::sites::scalar)
        .unwrap_or_default();
    let time = post.get("time").and_then(Value::as_i64);
    let name = post.get("name").and_then(Value::as_str).map(str::to_owned);
    let title = post.get("title").and_then(Value::as_str).map(str::to_owned);
    Some(
        Item::new(url, post.clone())
            .ids(thread, tim)
            .user(name)
            .caption(title)
            .timestamp(time)
            .header("Referer", &format!("{root}/")),
    )
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
            t("8kun", "https://8kun.top/a/res/12345.html#q1"),
            Some(Target::Thread {
                board: "a".into(),
                thread: "12345".into()
            })
        );
        assert_eq!(
            t("smugloli", "https://smugloli.net/a/res/9+50.html"),
            Some(Target::Thread {
                board: "a".into(),
                thread: "9".into()
            })
        );
        for raw in [
            "https://boards.guro.cx/g/",
            "https://boards.guro.cx/g",
            "https://boards.guro.cx/g/catalog.html",
            "https://boards.guro.cx/g/index.html",
            "https://boards.guro.cx/g/2.html",
        ] {
            assert_eq!(
                t("gurochan", raw),
                Some(Target::Board { board: "g".into() }),
                "{raw}"
            );
        }
        for (c, raw) in [
            ("8kun", "https://8kun.top/"),
            ("8kun", "https://8kun.top/a/res/"),
            ("8kun", "https://8kun.top/a/?page=2"),
            ("8kun", "https://www.8kun.top/a/res/1.html"),
            ("8kun", "https://8kun.top.evil.test/a/res/1.html"),
            ("8kun", "https://8kun.top:8443/a/res/1.html"),
            ("8kun", "https://u@8kun.top/a/res/1.html"),
            ("8kun", "https://smuglo.li/a/res/1.html"),
            ("smugloli", "ftp://smuglo.li/a/res/1.html"),
        ] {
            assert_eq!(t(c, raw), None, "{raw}");
        }
        let find = |raw: &str| crate::sites::find(&Url::parse(raw).unwrap()).map(|s| s.category);
        assert_eq!(find("https://smuglo.li/a/res/1.html"), Some("smugloli"));
        assert_eq!(find("https://boards.guro.cx/a/"), Some("gurochan"));
    }

    fn thread() -> Value {
        json!({"posts": [
            {"no": 100, "time": 1500000000, "name": "Anon", "com": "<p>Op &amp; text</p>",
             "filename": "first", "ext": ".jpg", "tim": "1500000000123",
             "extra_files": [{"filename": "second", "ext": ".png", "tim": "1500000000124"}]},
            {"no": 101, "time": 1500000100, "com": "no file"},
        ]})
    }

    #[tokio::test]
    async fn extracts_thread_with_extra_files() {
        let (origin, server) = serve(vec![json(thread())]);
        let http = http("gurochan", origin);
        let files = media(extract(
            "gurochan",
            &http,
            Url::parse("https://boards.guro.cx/g/res/100.html").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].url.as_str(),
            "https://boards.guro.cx/g/src/1500000000123.jpg"
        );
        assert_eq!(
            files[1].url.as_str(),
            "https://boards.guro.cx/g/src/1500000000124.png"
        );
        let k = keywords(&files[1]);
        assert_eq!(k["title"], "Op & text");
        assert_eq!(k["num"], 1);
        assert_eq!(k["filename"], "second");
        assert_eq!(k["extension"], "png");
        assert_eq!(k["no"], 100);
        assert_eq!(keywords(&files[0])["num"], 0);
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "1500000000 first.jpg");
        assert_eq!(file.render(&files[1]).unwrap(), "1500000000-1 second.png");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("gurochan/g/100 Op & text")
        );
        let (namespace, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(namespace, "gurochan");
        assert_eq!(
            identity,
            r#"["{board}_{thread}_{tim}",["g","100","1500000000123"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /g/res/100.json "));
    }

    #[test]
    fn eightkun_uses_media_host() {
        let body = json!({"posts": [{"no": 1, "sub": "S", "filename": "f", "ext": ".jpg",
            "tim": "0123456789abcdef0123456789abcdef"},
            {"no": 2, "filename": "g", "ext": ".gif", "tim": "1500000000"}]});
        let items = parse_thread("8kun", "https://8kun.top", "v", "1", &body).unwrap();
        assert_eq!(
            items[0].url.as_str(),
            "https://media.128ducks.com/file_store/0123456789abcdef0123456789abcdef.jpg"
        );
        assert_eq!(
            items[1].url.as_str(),
            "https://media.128ducks.com/v/src/1500000000.gif"
        );
    }

    #[tokio::test]
    async fn configured_instance_extracts_on_its_root() {
        let instance = crate::sites::instances::define(
            "vichan",
            "vichan-configured",
            json!({"root": "http://vichan.test:8080/chan", "pattern": r"(?:www\.)?vichan\.test:8080/chan|vichan-alias\.test"}),
        );
        let url = Url::parse("https://vichan-alias.test/g/res/100.html#q").unwrap();
        let site = crate::sites::find(&url).unwrap();
        assert_eq!(site.category, "vichan-configured");
        assert!(
            crate::sites::find(&Url::parse("http://vichan.test/chan/g/res/1.html").unwrap())
                .is_none_or(|site| site.category != "vichan-configured")
        );
        assert_eq!(
            target(
                "vichan-configured",
                &Url::parse("http://vichan.test:8080/chan/g/").unwrap()
            ),
            Some(Target::Board { board: "g".into() })
        );
        let (origin, server) = serve(vec![json(thread())]);
        let http = http("vichan-configured", origin);
        let files = media((instance.site.extract)(&http, url)).await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].url.as_str(),
            "http://vichan.test:8080/chan/g/src/1500000000123.jpg"
        );
        let k = keywords(&files[1]);
        assert_eq!(k["category"], "vichan-configured");
        assert_eq!(k["subcategory"], "thread");
        assert_eq!(k["title"], "Op & text");
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("GET /chan/g/res/100.json "),
            "{}",
            requests[0]
        );
    }

    #[tokio::test]
    async fn board_queues_threads_lazily() {
        let (origin, server) = serve(vec![
            json(json!([{"page": 0, "threads": [{"no": 100}]}])),
            json(thread()),
        ]);
        let http = http("smugloli", origin);
        let files = media(extract(
            "smugloli",
            &http,
            Url::parse("https://smuglo.li/a/catalog.html").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].url.as_str(),
            "https://smuglo.li/a/src/1500000000123.jpg"
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /a/threads.json "));
        assert!(requests[1].starts_with("GET /a/res/100.json "));
    }

    #[tokio::test]
    async fn missing_thread_is_an_error() {
        let (origin, server) = serve(vec![crate::sites::testing::response(
            "404 Not Found",
            "text/html",
            b"gone",
        )]);
        let http = http("gurochan", origin);
        let events = extract(
            "gurochan",
            &http,
            Url::parse("https://boards.guro.cx/g/res/100.html").unwrap(),
        );
        let results: Vec<_> = futures_util::StreamExt::collect(events).await;
        assert_eq!(results.len(), 1);
        assert!(results[0].is_err());
        assert!(server.join().unwrap()[0].starts_with("GET /g/res/100.json"));
    }
}
