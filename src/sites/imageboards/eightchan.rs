//! 8chan threads and boards, mirroring gallery_dl/extractor/8chan.py. 8chan
//! runs LynxChan, so threads share its JSON parsing.
//!
//! Like upstream, requests carry the terms-of-service cookie (its name read
//! from the confirmation page), and files are fetched with the session's
//! captcha cookies. 8chan currently answers unknown clients with a
//! JavaScript proof-of-work page that neither upstream nor this port
//! solves; the cookies of a browser that passed it (`--cookies` or
//! `--cookies-from-browser`) are required then. Upstream's check that
//! rejects the 166-byte captcha placeholder image is not ported.
use super::{component, digits, host_in, rest};
use crate::sites::{Events, Http, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

// Upstream directory: ("{category}", "{boardUri}", "{threadId} {subject[:50]}"),
// rendered from the thread's Directory kwdict; files carry it under
// `_directory`.
static THREAD: Route = Route::new(
    "thread",
    "{postId}{num:?-//} {filename[:200]}.{extension}",
    &[
        "{category}",
        "{boardUri}",
        "{threadId} {_directory[subject][:50]}",
    ],
    "{boardUri}_{postId}_{num}",
);
static BOARD: Route = Route::new("board", "{filename}.{extension}", &["{category}"], "");

pub static SITE: Site = Site {
    category: "8chan",
    name: "8chan",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&THREAD, &BOARD],
    api_key: false,
    cookies: Some("8chan.moe"),
};

/// Upstream's fallback when no TOS cookie name can be determined.
const TOS_FALLBACK: &str = "TOS20250418";

#[derive(Debug, PartialEq)]
enum Target {
    Thread { board: String, thread: String },
    Board { board: String, page: Option<u64> },
}

/// `8chan\.(moe|se|cc)/BOARD/(?:res|last)/ID` and `/BOARD/(?:(\d+)\.html)?$`.
fn target(url: &Url) -> Option<Target> {
    if !host_in(url, &["8chan.moe", "8chan.se", "8chan.cc"]) {
        return None;
    }
    let (board, tail) = component(rest(url))?;
    if let Some((thread, _)) = tail
        .strip_prefix("/res/")
        .or_else(|| tail.strip_prefix("/last/"))
        .and_then(digits)
    {
        return Some(Target::Thread {
            board,
            thread: thread.into(),
        });
    }
    let tail = tail.strip_prefix('/')?;
    if tail.is_empty() {
        return Some(Target::Board { board, page: None });
    }
    let (page, tail) = digits(tail)?;
    (tail == ".html").then(|| Target::Board {
        board,
        page: page.parse().ok(),
    })
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let root = format!("https://{}", url.host_str().unwrap_or_default());
    match target(&url) {
        Some(Target::Thread { board, thread }) => crate::sites::once(async move {
            accept_terms(http, &root).await?;
            thread_events(http, &root, &board, &thread).await
        }),
        Some(Target::Board { board, page }) => crate::sites::once(async move {
            accept_terms(http, &root).await?;
            Ok(board_events(http, root, board, page.unwrap_or(1)))
        }),
        None => crate::sites::fail(error("unsupported 8chan URL")),
    }
}

/// The name of a `tos*` cookie in a `Cookie` header.
fn tos_cookie(header: Option<String>) -> Option<String> {
    header?
        .split("; ")
        .filter_map(|pair| pair.split_once('=').map(|(name, _)| name))
        .find(|name| name.to_ascii_lowercase().starts_with("tos"))
        .map(str::to_owned)
}

/// `cookies_tos_name` and setting that cookie to `1`.
async fn accept_terms(http: &Http, root: &str) -> Result<()> {
    let confirmed = http.api(&format!("{root}/.static/pages/confirmed.html"))?;
    let name = match tos_cookie(http.cookie_header(&confirmed)) {
        Some(name) => name,
        None => {
            let referer = format!("{root}/.static/pages/disclaimer.html");
            // The page only matters for the cookie it sets.
            let _ = http.get(&confirmed, &[("Referer", &referer)]).await;
            tos_cookie(http.cookie_header(&confirmed)).unwrap_or_else(|| TOS_FALLBACK.into())
        }
    };
    if !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return Err(error("8chan set an invalid TOS cookie"));
    }
    http.set_cookie(
        &http.api(&format!("{root}/"))?,
        &format!("{name}=1; Path=/"),
    );
    Ok(())
}

/// GET a JSON document, naming the proof-of-work page when it answers.
async fn json(http: &Http, url: &Url, headers: &[(&str, &str)]) -> Result<Value> {
    let body = http.get(url, headers).await?;
    serde_json::from_slice(&body).map_err(|_| {
        if String::from_utf8_lossy(&body).contains("POWBlock") {
            error(
                "8chan requires a proof-of-work check; pass the cookies of a browser that opened the site",
            )
        } else {
            error("8chan returned invalid JSON")
        }
    })
}

/// `cookies_prepare`: request the captcha cookies once per session.
/// Failures are ignored, as upstream does.
async fn captcha_cookies(http: &Http, root: &str, referer: &str) {
    let Ok(base) = http.api(&format!("{root}/")) else {
        return;
    };
    if http.cookie(&base, "captchaexpiration").is_some() {
        return;
    }
    let Ok(mut url) = http.api(&format!("{root}/captcha.js")) else {
        return;
    };
    let now = time::OffsetDateTime::now_utc();
    url.query_pairs_mut().append_pair(
        "d",
        &format!("{} GMT+0000 (UTC)", http_date(now, "%a %b %d %Y")),
    );
    let _ = http.get(&url, &[("Referer", referer)]).await;
}

/// The session's `Cookie` header for a file, with `captchaexpiration`
/// moved a month ahead like upstream's `cookies_prepare`. The new value is
/// not a valid cookie-octet string, so it is written into the header.
fn file_cookies(header: &str, now: time::OffsetDateTime) -> String {
    let expires = now + time::Duration::days(30) + time::Duration::seconds(300);
    header
        .split("; ")
        .map(|pair| match pair.split_once('=') {
            Some(("captchaexpiration", _)) => format!(
                "captchaexpiration={} GMT",
                http_date(expires, "%a, %d %b %Y")
            ),
            _ => pair.to_owned(),
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// `strftime` with the given date part followed by ` %H:%M:%S`.
fn http_date(dt: time::OffsetDateTime, date: &str) -> String {
    let weekday = &dt.weekday().to_string()[..3];
    let month = &dt.month().to_string()[..3];
    let date = date
        .replace("%a", weekday)
        .replace("%b", month)
        .replace("%d", &format!("{:02}", dt.day()))
        .replace("%Y", &format!("{:04}", dt.year()));
    format!(
        "{date} {:02}:{:02}:{:02}",
        dt.hour(),
        dt.minute(),
        dt.second()
    )
}

async fn thread_events<'a>(
    http: &'a Http,
    root: &str,
    board: &str,
    thread: &str,
) -> Result<Events<'a>> {
    let page = format!("{root}/{board}/res/{thread}.");
    let referer = format!("{page}html");
    let body = json(
        http,
        &http.api(&format!("{page}json"))?,
        &[("Referer", &referer)],
    )
    .await?;
    captcha_cookies(http, root, &referer).await;
    let mut items = super::lynxchan::parse_thread(root, &body)?;
    let now = time::OffsetDateTime::now_utc();
    for item in &mut items {
        item.headers.insert("Referer".into(), referer.clone());
        if let Some(cookie) = http
            .api(item.url.as_str())
            .ok()
            .and_then(|url| http.cookie_header(&url))
        {
            item.headers
                .insert("Cookie".into(), file_cookies(&cookie, now));
        }
    }
    Ok(crate::sites::events(crate::sites::post(
        &SITE, http, &THREAD, items,
    )))
}

/// Board pages from `start` to `pageCount`, requested as they are reached.
fn board_events(http: &Http, root: String, board: String, start: u64) -> Events<'_> {
    crate::sites::paginate((start, None::<u64>), move |(number, count)| {
        let (root, board) = (root.clone(), board.clone());
        async move {
            if count.is_some_and(|count| number > count) {
                return Ok(None);
            }
            let url = http.api(&format!("{root}/{board}/{number}.json"))?;
            let data = json(http, &url, &[]).await?;
            let count = count.unwrap_or_else(|| {
                data.get("pageCount")
                    .and_then(Value::as_u64)
                    .unwrap_or_default()
            });
            let children = parse_board(&root, &board, &data);
            let events = super::queue_all(http, &SITE, &BOARD, children);
            Ok(Some((events, (number + 1, Some(count)))))
        }
    })
}

fn parse_board(root: &str, board: &str, data: &Value) -> Vec<(Url, Map<String, Value>)> {
    data.get("threads")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|thread| {
            let data = thread.as_object()?.clone();
            let id = data.get("threadId").map(crate::sites::scalar)?;
            digits(&id).filter(|(_, tail)| tail.is_empty())?;
            let url = Url::parse(&format!("{root}/{board}/res/{id}.html")).ok()?;
            Some((url, data))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, json, keywords, media, response, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let thread = |board: &str, thread: &str| {
            Some(Target::Thread {
                board: board.into(),
                thread: thread.into(),
            })
        };
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://8chan.moe/vhs/res/4505.html"),
            thread("vhs", "4505")
        );
        assert_eq!(
            t("https://8chan.se/vhs/res/4505.html#q1"),
            thread("vhs", "4505")
        );
        assert_eq!(t("https://8chan.cc/a/last/12.html"), thread("a", "12"));
        assert_eq!(
            t("https://8chan.moe/vhs/"),
            Some(Target::Board {
                board: "vhs".into(),
                page: None
            })
        );
        assert_eq!(
            t("https://8chan.moe/vhs/2.html"),
            Some(Target::Board {
                board: "vhs".into(),
                page: Some(2)
            })
        );
        for raw in [
            "https://8chan.moe/",
            "https://8chan.moe/vhs",
            "https://8chan.moe/vhs/?x=1",
            "https://8chan.moe/vhs/catalog.html",
            "https://8chan.moe/vhs/2.html#x",
            "https://www.8chan.moe/vhs/res/1.html",
            "https://8chan.net/vhs/res/1.html",
            "https://8chan.moe.evil.test/vhs/res/1.html",
            "https://8chan.moe:8443/vhs/res/1.html",
            "https://u@8chan.moe/vhs/res/1.html",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
        let find = |raw: &str| crate::sites::find(&Url::parse(raw).unwrap()).map(|s| s.category);
        assert_eq!(find("https://8chan.moe/a/res/1.html"), Some("8chan"));
    }

    fn thread() -> Value {
        json!({"threadId": 100, "boardUri": "a", "subject": "Thread subject", "message": "Op",
        "creation": "2024-01-02T03:04:05.000Z",
        "files": [{"originalName": "first.png", "path": "/.media/aaa.png", "size": 1}],
        "posts": [
            {"postId": 101, "subject": null, "message": "no files", "files": []},
            {"postId": 102, "subject": null, "message": "two",
             "files": [{"originalName": "x.jpg", "path": "/.media/bbb.jpg"},
                       {"originalName": "y.webm", "path": "/.media/ccc.webm"}]},
        ]})
    }

    #[tokio::test]
    async fn extracts_thread_with_session_cookies() {
        let (root, server) = serve(vec![
            response(
                "200 OK\r\nSet-Cookie: TOS20991231=0; Path=/",
                "text/html",
                b"ok",
            ),
            json(thread()),
            response(
                "200 OK\r\nSet-Cookie: captchaid=abc; Path=/\r\nSet-Cookie: captchaexpiration=soon; Path=/",
                "application/javascript",
                b"",
            ),
        ]);
        let http = http("8chan", root);
        let files = media(extract(
            &http,
            Url::parse("https://8chan.moe/a/res/100.html").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].url.as_str(), "https://8chan.moe/.media/aaa.png");
        assert_eq!(files[2].url.as_str(), "https://8chan.moe/.media/ccc.webm");
        assert_eq!(
            files[0].headers["Referer"],
            "https://8chan.moe/a/res/100.html"
        );
        let cookie = &files[1].headers["Cookie"];
        assert!(cookie.contains("TOS20991231=1"), "{cookie}");
        assert!(cookie.contains("captchaid=abc"), "{cookie}");
        assert!(!cookie.contains("captchaexpiration=soon"), "{cookie}");
        let moved = file_cookies(
            "a=1; captchaexpiration=soon",
            time::macros::datetime!(2026-01-31 23:58:00 UTC),
        );
        assert_eq!(
            moved,
            "a=1; captchaexpiration=Tue, 03 Mar 2026 00:03:00 GMT"
        );
        let k = keywords(&files[2]);
        assert_eq!(k["postId"], 102);
        assert_eq!(k["threadId"], 100);
        assert_eq!(k["num"], 1);
        assert_eq!(k["filename"], "y");
        assert_eq!(k["extension"], "webm");
        assert_eq!(k["message"], "two");
        let (file, dir) = crate::sites::default_templates(&files[2]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "100 first.png");
        assert_eq!(file.render(&files[2]).unwrap(), "102-1 y.webm");
        assert_eq!(
            dir.render(&files[2]).unwrap().unwrap(),
            std::path::PathBuf::from("8chan/a/100 Thread subject")
        );
        let (namespace, identity) = crate::sites::archive_key(&files[2]).unwrap();
        assert_eq!(namespace, "8chan");
        assert_eq!(identity, r#"["{boardUri}_{postId}_{num}",["a","102","1"]]"#);
        let requests = server.join().unwrap();
        let first = requests[0].to_ascii_lowercase();
        assert!(first.starts_with("get /.static/pages/confirmed.html "));
        assert!(first.contains("referer: https://8chan.moe/.static/pages/disclaimer.html"));
        let second = requests[1].to_ascii_lowercase();
        assert!(second.starts_with("get /a/res/100.json "));
        assert!(second.contains("cookie: tos20991231=1"));
        assert!(second.contains("referer: https://8chan.moe/a/res/100.html"));
        assert!(requests[2].starts_with("GET /captcha.js?d="));
        assert!(requests[2].contains("+GMT%2B0000+%28UTC%29 "));
    }

    #[tokio::test]
    async fn board_paginates_to_page_count() {
        let (root, server) = serve(vec![
            html("no cookie"),
            json(json!({"pageCount": 3, "threads": [{"threadId": 1}, {"threadId": "x"}]})),
            json(json!({"pageCount": 3, "threads": [{"threadId": 2}]})),
        ]);
        let http = http("8chan", root);
        let mut events = extract(&http, Url::parse("https://8chan.moe/a/2.html").unwrap());
        let mut children = Vec::new();
        while let Some(event) = futures_util::StreamExt::next(&mut events).await {
            if let crate::event::ExtractionEvent::BeginChild(scope) = event.unwrap() {
                children.push(scope.metadata.subcategory().to_owned());
                scope.control.cancel();
            }
        }
        // The thread with an invalid id is skipped.
        assert_eq!(children, vec!["board", "board"]);
        let threads = parse_board(
            "https://8chan.moe",
            "a",
            &json!({"threads": [{"threadId": 7}, {"threadId": "1/x"}]}),
        );
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].0.as_str(), "https://8chan.moe/a/res/7.html");
        assert_eq!(threads[0].1["threadId"], 7);
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /a/2.json "));
        assert!(
            requests[1].contains(&format!("cookie: {TOS_FALLBACK}=1")),
            "{}",
            requests[1]
        );
        assert!(requests[2].starts_with("GET /a/3.json "));
    }

    #[tokio::test]
    async fn proof_of_work_page_is_reported() {
        let (root, server) = serve(vec![html("ok"), html("<title>POWBlock Check…</title>")]);
        let http = http("8chan", root);
        let events = extract(&http, Url::parse("https://8chan.moe/a/res/1.html").unwrap());
        let results: Vec<_> = futures_util::StreamExt::collect(events).await;
        let message = results[0].as_ref().err().unwrap().to_string();
        assert!(message.contains("proof-of-work"), "{message}");
        server.join().unwrap();
    }
}
