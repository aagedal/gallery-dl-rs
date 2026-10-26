//! Futaba Channel (2chan) threads, mirroring gallery_dl/extractor/2chan.py.
use crate::error;
use crate::sites::gdl::{Scan, captures, extr, group, partition, re, rpartition};
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
    name: "Futaba Channel",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&THREAD],
    api_key: false,
    cookies: None,
};

fn target(url: &Url) -> Option<(String, String, String)> {
    let c = captures(
        re!(r"^https?://([\w-]+)\.2chan\.net/([^/?#]+)/res/(\d+)"),
        url,
    )?;
    Some((
        group(&c, 1).into(),
        group(&c, 2).into(),
        group(&c, 3).into(),
    ))
}

/// `parse`: one post's keywords; `filename` only for posts with a file.
fn parse(post: &str) -> Map<String, Value> {
    let mut scan = Scan::new(post);
    let mut data = Map::new();
    let mut field = |scan: &mut Scan<'_>, key: &str, begin: &str, end: &str| {
        let value = scan.find(begin, end).map_or(Value::Null, Value::from);
        data.insert(key.into(), value);
    };
    field(&mut scan, "post", "class=\"csb\">", "<");
    field(&mut scan, "name", "class=\"cnm\">", "<");
    field(&mut scan, "now", "class=\"cnw\">", "<");
    field(&mut scan, "no", "class=\"cno\">No.", "<");
    scan.find("<blockquote", "");
    field(&mut scan, "com", ">", "</blockquote>");
    if let Some(Value::String(name)) = data.get("name") {
        let name = name.trim().to_owned();
        data.insert("name".into(), name.into());
    }
    let path = extr(post, "<a href=\"/", "\"");
    if !path.is_empty() && !path.starts_with("bin/jump") {
        let mut scan = Scan::new(post);
        scan.find("_blank", "");
        let filename = scan.find(">", "<").unwrap_or_default().to_owned();
        let fsize = scan.find("(", " ").map_or(Value::Null, Value::from);
        let (tim, _, extension) = partition(&filename, ".");
        data.insert("tim".into(), tim.into());
        data.insert("extension".into(), extension.into());
        data.insert(
            "time".into(),
            tim.get(..tim.len().saturating_sub(3))
                .unwrap_or_default()
                .into(),
        );
        data.insert("ext".into(), format!(".{extension}").into());
        data.insert("fsize".into(), fsize);
        data.insert("filename".into(), filename.into());
    }
    data
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some((server, board, thread)) = target(&url) else {
        return crate::sites::fail(error("unsupported 2chan URL"));
    };
    crate::sites::once(async move {
        let page = http
            .text(&http.api(&format!(
                "https://{server}.2chan.net/{board}/res/{thread}.htm"
            ))?)
            .await?;
        let (title, _, board_name) = rpartition(extr(&page, "<title>", "</title>"), " - ");
        let mut meta = Map::new();
        meta.insert("server".into(), server.as_str().into());
        meta.insert("title".into(), title.into());
        meta.insert("board".into(), board.as_str().into());
        let board_name: String = board_name
            .chars()
            .take(board_name.chars().count().saturating_sub(4))
            .collect();
        meta.insert("board_name".into(), board_name.into());
        meta.insert("thread".into(), thread.as_str().into());
        let body = extr(
            &page,
            "<div class=\"thre\"",
            "<div style=\"clear:left\"></div>",
        );
        let items = body
            .split("<table border=0>")
            .map(parse)
            .filter(|post| post.contains_key("filename"))
            .filter_map(|mut post| {
                post.extend(meta.clone());
                let filename = crate::sites::scalar(&post["filename"]);
                let url = Url::parse(&format!(
                    "https://{server}.2chan.net/{board}/src/{filename}"
                ))
                .ok()?;
                let no = post.get("no").map(crate::sites::scalar).unwrap_or_default();
                Some(Item::new(url, post).ids(thread.clone(), no))
            })
            .collect::<Vec<_>>();
        let events = items
            .into_iter()
            .flat_map(|item| crate::sites::post(&SITE, http, &THREAD, vec![item]))
            .collect();
        Ok(crate::sites::events(events))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http as client, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        assert_eq!(
            target(&Url::parse("https://dec.2chan.net/70/res/17222.htm").unwrap()),
            Some(("dec".into(), "70".into(), "17222".into()))
        );
        assert_eq!(
            target(&Url::parse("https://2chan.net/70/res/1.htm").unwrap()),
            None
        );
    }

    #[tokio::test]
    async fn extracts_thread_files() {
        let page = r#"<title>Thread - 二次元裏＠ふたば</title><div class="thre">
            <span class="csb">無念</span><span class="cnm"> としあき </span><span class="cnw">24/01/01</span><span class="cno">No.17222</span>
            <a href="/70/src/1704067200123.jpg" target="_blank">1704067200123.jpg</a>-(123456 B)<blockquote>hello</blockquote>
            <table border=0><span class="cno">No.17223</span><blockquote>reply</blockquote>
            <div style="clear:left"></div>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = client("2chan", root);
        let files = media(extract(
            &http,
            Url::parse("https://dec.2chan.net/70/res/17222.htm").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].url.as_str(),
            "https://dec.2chan.net/70/src/1704067200123.jpg"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["name"], "としあき");
        assert_eq!(k["time"], "1704067200");
        assert_eq!(k["board_name"], "二次元裏");
        assert_eq!(k["com"], "hello");
        let (file, _) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "1704067200123.jpg");
        server.join().unwrap();
    }
}
