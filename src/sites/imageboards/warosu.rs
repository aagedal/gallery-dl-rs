//! warosu.org threads, mirroring gallery_dl/extractor/warosu.py by parsing
//! the thread HTML.
use super::{Extr, digits, host_in, parse_int, rest, seg};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://warosu.org";

static THREAD: Route = Route::new(
    "thread",
    "{tim} {filename}.{extension}",
    &["{category}", "{board}", "{thread} - {title}"],
    "{board}_{thread}_{tim}",
);

pub static SITE: Site = Site {
    category: "warosu",
    name: "warosu",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&THREAD],
    api_key: false,
};

fn target(url: &Url) -> Option<(String, String)> {
    if !host_in(url, &["warosu.org", "www.warosu.org"]) {
        return None;
    }
    // `/([^/]+)/thread/(\d+)`
    let (board, tail) = seg(rest(url), &['/'])?;
    let (thread, _) = tail.strip_prefix("/thread/").and_then(digits)?;
    (!board.contains(['?', '#'])).then(|| (board, thread.to_owned()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some((board, thread)) = target(&url) else {
        return crate::sites::fail(error("unsupported warosu URL"));
    };
    crate::sites::once(async move {
        let api = http.api(&format!("{ROOT}/{board}/thread/{thread}"))?;
        let page = http.text(&api).await?;
        let items = parse_page(&page, &board, &thread)?;
        Ok(crate::sites::events(crate::sites::post(
            &SITE, http, &THREAD, items,
        )))
    })
}

fn parse_page(page: &str, board: &str, thread: &str) -> Result<Vec<Item>> {
    let board_name = crate::sites::extr(page, "<title>", "</title>")
        .unwrap_or_default()
        .split(" - ")
        .nth(1)
        .ok_or_else(|| error("warosu page has no board name"))?
        .to_owned();
    let mut title = crate::sites::unescape(
        crate::sites::extr(page, "class=filetitle>", "<").unwrap_or_default(),
    );
    let content = crate::sites::extr(page, "<div class=content", "</form>").unwrap_or_default();
    let posts: Vec<Map<String, Value>> = content.split("<table>").map(parse_post).collect();
    if title.is_empty() {
        let com = posts[0].get("com").and_then(Value::as_str).unwrap_or_default();
        title = super::truncate(&crate::sites::unescape(&crate::sites::remove_html(com)), 50);
    }
    let mut data = Map::new();
    data.insert("board".into(), board.into());
    data.insert("board_name".into(), board_name.into());
    data.insert("thread".into(), thread.into());
    data.insert("title".into(), title.clone().into());

    let mut items = Vec::new();
    for mut post in posts {
        let Some(image) = post.get("image").and_then(Value::as_str).map(str::to_owned) else {
            continue;
        };
        let Some(url) = crate::sites::media_url(&image, &["warosu.org"]) else {
            continue;
        };
        for key in ["w", "h", "no", "time", "tim"] {
            let value = post.get(key).and_then(Value::as_str).map_or(0, parse_int);
            post.insert(key.into(), value.into());
        }
        let time = post.get("time").and_then(Value::as_i64).unwrap_or_default();
        post.insert("now".into(), now(time).into());
        post.extend(data.clone());
        let tim = post.get("tim").map(crate::sites::scalar).unwrap_or_default();
        let name = post.get("name").and_then(Value::as_str).map(str::to_owned);
        items.push(
            Item::new(url, post)
                .ids(thread, tim)
                .user(name)
                .caption(Some(title.clone()))
                .timestamp(Some(time))
                .header("Referer", &format!("{ROOT}/")),
        );
    }
    Ok(items)
}

/// `strftime("%a, %b {day}, %Y %H:%M:%S")` of a UTC timestamp.
fn now(timestamp: i64) -> String {
    let Ok(dt) = time::OffsetDateTime::from_unix_timestamp(timestamp) else {
        return String::new();
    };
    let weekday = &dt.weekday().to_string()[..3];
    let month = &dt.month().to_string()[..3];
    format!(
        "{weekday}, {month} {}, {:04} {:02}:{:02}:{:02}",
        dt.day(),
        dt.year(),
        dt.hour(),
        dt.minute(),
        dt.second()
    )
}

fn parse_post(post: &str) -> Map<String, Value> {
    let mut extr = Extr::new(post);
    let mut data = Map::new();
    data.insert("no".into(), extr.next("id=p", ">").into());
    data.insert(
        "name".into(),
        extr.next("class=postername>", "<").trim().into(),
    );
    data.insert("time".into(), extr.next("class=posttime title=", "000>").into());
    data.insert(
        "com".into(),
        crate::sites::unescape(&crate::sites::remove_html(
            extr.next("<blockquote>", "</blockquote>").trim(),
        ))
        .into(),
    );
    if post.contains("<span class=\"fileinfo") && extract_image(post, &mut data) {
        let image = data.get("image").and_then(Value::as_str).unwrap_or_default();
        let part = super::rpartition(image, "/").1;
        let (tim, extension) = part.split_once('.').unwrap_or((part, ""));
        let (tim, extension) = (tim.to_owned(), extension.to_owned());
        data.insert("ext".into(), format!(".{extension}").into());
        data.insert("tim".into(), tim.into());
        data.insert("extension".into(), extension.into());
    }
    data
}

fn extract_image(post: &str, data: &mut Map<String, Value>) -> bool {
    let mut extr = Extr::new(post);
    extr.next("<span class=\"fileinfo\">", "");
    data.insert("fsize".into(), extr.next("File: ", ", ").into());
    data.insert("w".into(), extr.next("", "x").into());
    data.insert("h".into(), extr.next("", ", ").into());
    let name = extr.next("", "<").trim_end();
    data.insert(
        "filename".into(),
        crate::sites::percent_decode(super::rpartition(name, ".").0).into(),
    );
    extr.next("<br>", "");
    let url = extr.next("<a href=", ">");
    if url.is_empty() {
        return false;
    }
    let image = if url.starts_with('/') {
        format!("{ROOT}{url}")
    } else if !url.contains("warosu.") {
        return false;
    } else {
        url.to_owned()
    };
    data.insert("image".into(), image.into());
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http, keywords, media, response, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://warosu.org/jp/thread/16656025"),
            Some(("jp".into(), "16656025".into()))
        );
        assert_eq!(
            t("https://www.warosu.org/fa/thread/1#p2"),
            Some(("fa".into(), "1".into()))
        );
        for raw in [
            "https://warosu.org/jp/",
            "https://warosu.org/jp/thread/x",
            "https://warosu.org.evil.test/jp/thread/1",
            "https://i.warosu.org/jp/thread/1",
            "https://warosu.org:8080/jp/thread/1",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    const PAGE: &str = r#"<html><head><title>/jp/ - Otaku Culture - Thread</title></head>
<body><div class=content><form>
<div><span class=filetitle></span>
<span class="fileinfo">File: 1.2 MB, 640x480, my%20pic.jpg<br>
<a href=https://i.warosu.org/data/jp/img/0001/23/1500000000123.jpg><img></a>
<span id=p100><span class=postername>Anonymous </span>
<span class=posttime title=1500000000000>x</span>
<blockquote><p>Opening <b>post</b> &amp; more</p></blockquote></div>
<table><tr><td id=p101><span class=postername>Bob</span>
<span class=posttime title=1500000100000>y</span>
<blockquote>reply without file</blockquote></td></tr></table>
<table><tr><td id=p102><span class=postername>Carl</span>
<span class=posttime title=1500000200000>z</span>
<span class="fileinfo">File: 3 KB, 10x20, clip.webm<br>
<a href=/data/jp/img/0001/23/1500000200456.webm>x</a>
<blockquote>reply</blockquote></td></tr></table>
<table><span class="fileinfo">File: 1 KB, 1x1, evil.png<br><a href=https://evil.test/x.png>x</a></table>
</form></body></html>"#;

    #[tokio::test]
    async fn extracts_thread_page() {
        let (origin, server) = serve(vec![response("200 OK", "text/html", PAGE.as_bytes())]);
        let http = http("warosu", origin);
        let files = media(extract(
            &http,
            Url::parse("https://warosu.org/jp/thread/100").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].url.as_str(),
            "https://i.warosu.org/data/jp/img/0001/23/1500000000123.jpg"
        );
        assert_eq!(
            files[1].url.as_str(),
            "https://warosu.org/data/jp/img/0001/23/1500000200456.webm"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["board_name"], "Otaku Culture");
        assert_eq!(k["title"], "Opening post & more");
        assert_eq!(k["filename"], "my pic");
        assert_eq!(k["extension"], "jpg");
        assert_eq!(k["ext"], ".jpg");
        assert_eq!(k["w"], 640);
        assert_eq!(k["h"], 480);
        assert_eq!(k["fsize"], "1.2 MB");
        assert_eq!(k["no"], 100);
        assert_eq!(k["name"], "Anonymous");
        assert_eq!(k["time"], 1500000000);
        assert_eq!(k["now"], "Fri, Jul 14, 2017 02:40:00");
        assert_eq!(k["tim"], 1500000000123u64);
        let k = keywords(&files[1]);
        assert_eq!(k["name"], "Carl");
        assert_eq!(k["com"], "reply");
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "1500000200456 clip.webm");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("warosu/jp/100 - Opening post & more")
        );
        let (_, identity) = crate::sites::archive_key(&files[1]).unwrap();
        assert_eq!(
            identity,
            r#"["{board}_{thread}_{tim}",["jp","100","1500000200456"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /jp/thread/100 "));
    }

    #[test]
    fn now_avoids_zero_padding() {
        assert_eq!(now(1_000_000_000), "Sun, Sep 9, 2001 01:46:40");
    }
}
