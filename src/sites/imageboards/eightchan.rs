//! 8chan (8chan.moe/.se/.cc) threads and boards, mirroring
//! gallery_dl/extractor/8chan.py: the TOS cookie is accepted and captcha
//! cookies prepared before files are downloaded.
use crate::error;
use crate::sites::gdl::{captures, group, int_of, nameext_into, re};
use crate::sites::{Events, Http, Item, Route, Site};
use futures_util::StreamExt;
use serde_json::{Map, Value};
use url::Url;

static THREAD: Route = Route::new(
    "thread",
    "{postId}{num:?-//} {filename[:200]}.{extension}",
    &["{category}", "{boardUri}", "{threadId} {subject[:50]}"],
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

#[derive(Debug, PartialEq)]
enum Target {
    Thread(String, String),
    Board(String, Option<String>),
}

/// Root (`https://8chan.TLD`) and the matched extractor.
fn target(url: &Url) -> Option<(String, Target)> {
    if let Some(c) = captures(
        re!(r"^https?://8chan\.(moe|se|cc)/([^/?#]+)/(?:res|last)/(\d+)"),
        url,
    ) {
        return Some((
            format!("https://8chan.{}", &c[1]),
            Target::Thread(group(&c, 2).into(), group(&c, 3).into()),
        ));
    }
    let c = captures(
        re!(r"^https?://8chan\.(moe|se|cc)/([^/?#]+)/(?:(\d+)\.html)?$"),
        url,
    )?;
    Some((
        format!("https://8chan.{}", &c[1]),
        Target::Board(group(&c, 2).into(), c.get(3).map(|m| m.as_str().to_owned())),
    ))
}

/// `cookies_tos_name` then set it to `1`.
async fn accept_tos(http: &Http, root: &str) -> crate::Result<()> {
    let base = http.api(root)?;
    let existing = http
        .cookie_names(&base)
        .into_iter()
        .find(|n| n.to_lowercase().starts_with("tos"));
    let name = match existing {
        Some(name) => name,
        None => {
            let url = http.api(&format!("{root}/.static/pages/confirmed.html"))?;
            let referer = format!("{root}/.static/pages/disclaimer.html");
            let _ = http.location(&url, &[("Referer", &referer)]).await;
            let _ = http.get(&url, &[("Referer", &referer)]).await;
            http.cookie_names(&base)
                .into_iter()
                .find(|n| n.to_lowercase().starts_with("tos"))
                .unwrap_or_else(|| "TOS20250418".into())
        }
    };
    let domain = &root[8..];
    http.set_cookie(&base, &format!("{name}=1; Domain={domain}; Path=/"));
    Ok(())
}

/// `cookies_prepare`: fetch captcha cookies and push their expiration ahead.
async fn prepare_captcha(http: &Http, root: &str) -> crate::Result<()> {
    let now = time::OffsetDateTime::now_utc();
    let stamp = now
        .format(&time::macros::format_description!(
            "[weekday repr:short] [month repr:short] [day] [year] [hour]:[minute]:[second] GMT+0000 (UTC)"
        ))
        .unwrap_or_default();
    let mut url = http.api(&format!("{root}/captcha.js"))?;
    url.query_pairs_mut().append_pair("d", &stamp);
    http.get(&url, &[]).await?;
    let later = now + time::Duration::days(30) + time::Duration::seconds(300);
    let expiration = later
        .format(&time::macros::format_description!(
            "[weekday repr:short], [day] [month repr:short] [year] [hour]:[minute]:[second] GMT"
        ))
        .unwrap_or_default();
    let base = http.api(root)?;
    if http.cookie(&base, "captchaexpiration").is_some() {
        http.set_cookie(
            &base,
            &format!(
                "captchaexpiration={expiration}; Domain={}; Path=/",
                &root[8..]
            ),
        );
    }
    Ok(())
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some((root, target)) = target(&url) else {
        return crate::sites::fail(error("unsupported 8chan URL"));
    };
    crate::sites::once(async move {
        accept_tos(http, &root).await?;
        Ok(match target {
            Target::Thread(board, thread) => {
                let base = format!("{root}/{board}/res/{thread}.");
                let referer = format!("{base}html");
                let mut data = http
                    .json_with(&http.api(&format!("{base}json"))?, &[("Referer", &referer)])
                    .await?;
                let _ = prepare_captcha(http, &root).await;
                let Some(thread) = data.as_object_mut() else {
                    return Err(error("8chan returned no thread data"));
                };
                thread.insert(
                    "postId".into(),
                    thread.get("threadId").cloned().unwrap_or(Value::Null),
                );
                let posts = thread
                    .remove("posts")
                    .and_then(|p| p.as_array().cloned())
                    .unwrap_or_default();
                let mut current = thread.clone();
                let mut items = Vec::new();
                for post in std::iter::once(Value::Object(thread.clone())).chain(posts) {
                    let Some(mut post) = post.as_object().cloned() else {
                        continue;
                    };
                    let files = post
                        .remove("files")
                        .and_then(|f| f.as_array().cloned())
                        .unwrap_or_default();
                    if files.is_empty() {
                        continue;
                    }
                    current.extend(post);
                    current.remove("files");
                    for (num, file) in files.into_iter().enumerate() {
                        let Some(mut fields) = file.as_object().cloned() else {
                            continue;
                        };
                        let path = fields
                            .get("path")
                            .map(crate::sites::scalar)
                            .unwrap_or_default();
                        let original = fields
                            .get("originalName")
                            .map(crate::sites::scalar)
                            .unwrap_or_default();
                        fields.extend(current.clone());
                        fields.insert("num".into(), num.into());
                        nameext_into(&original, &mut fields);
                        let Ok(url) = Url::parse(&format!("{root}{path}")) else {
                            continue;
                        };
                        let post_id = fields
                            .get("postId")
                            .map(crate::sites::scalar)
                            .unwrap_or_default();
                        items.push(
                            Item::new(url, fields)
                                .ids(post_id, num.to_string())
                                .header("Referer", &referer),
                        );
                    }
                }
                crate::sites::events(
                    items
                        .into_iter()
                        .flat_map(|item| crate::sites::post(&SITE, http, &THREAD, vec![item]))
                        .collect(),
                )
            }
            Target::Board(board, page) => {
                let first = page.map_or(1, |p| crate::sites::gdl::parse_int(&p));
                crate::sites::paginate(Some(first), move |state| {
                    let root = root.clone();
                    let board = board.clone();
                    async move {
                        let Some(number) = state else { return Ok(None) };
                        let data = http
                            .json(&http.api(&format!("{root}/{board}/{number}.json"))?)
                            .await?;
                        let children: Vec<(Url, Map<String, Value>)> = data["threads"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|thread| {
                                let url = Url::parse(&format!(
                                    "{root}/{board}/res/{}.html",
                                    crate::sites::scalar(&thread["threadId"])
                                ))
                                .ok()?;
                                Some((url, thread.as_object()?.clone()))
                            })
                            .collect();
                        let next = (number < int_of(data.get("pageCount"))).then_some(number + 1);
                        let events = futures_util::stream::iter(children)
                            .map(move |(url, data)| {
                                crate::sites::queue(
                                    http,
                                    SITE.category,
                                    BOARD.subcategory,
                                    url,
                                    &data,
                                )
                            })
                            .flatten()
                            .boxed();
                        Ok(Some((events, next)))
                    }
                })
            }
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{children, html, http as client, json, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap()).map(|(_, t)| t);
        assert_eq!(
            t("https://8chan.moe/vhs/last/4.html"),
            Some(Target::Thread("vhs".into(), "4".into()))
        );
        assert_eq!(
            t("https://8chan.se/vhs/"),
            Some(Target::Board("vhs".into(), None))
        );
        assert_eq!(
            t("https://8chan.moe/vhs/2.html"),
            Some(Target::Board("vhs".into(), Some("2".into())))
        );
        assert_eq!(
            target(&Url::parse("https://8chan.cc/vhs/res/4.html").unwrap())
                .unwrap()
                .0,
            "https://8chan.cc"
        );
    }

    #[tokio::test]
    async fn extracts_thread_files() {
        let thread = json!({"threadId": 4, "boardUri": "vhs", "subject": "Tapes", "files": [{"path": "/.media/a.png", "originalName": "tape.png"}],
            "posts": [{"postId": 5, "files": [{"path": "/.media/b.jpg", "originalName": "b.jpg"}, {"path": "/.media/c.jpg", "originalName": "c.jpg"}]},
                      {"postId": 6, "files": []}]});
        let (root, server) = serve(vec![
            html("tos"),
            html("tos"),
            json(thread),
            html("captcha"),
        ]);
        let http = client("8chan", root);
        let files = media(extract(
            &http,
            Url::parse("https://8chan.moe/vhs/res/4.html").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].url.as_str(), "https://8chan.moe/.media/a.png");
        let k = keywords(&files[2]);
        assert_eq!(k["postId"], 5);
        assert_eq!(k["num"], 1);
        assert_eq!(
            files[2].headers["Referer"],
            "https://8chan.moe/vhs/res/4.html"
        );
        let (file, dir) = crate::sites::default_templates(&files[2]).unwrap();
        assert_eq!(file.render(&files[2]).unwrap(), "5-1 c.jpg");
        assert_eq!(
            dir.render(&files[2]).unwrap().unwrap(),
            std::path::PathBuf::from("8chan/vhs/4 Tapes")
        );
        let requests = server.join().unwrap();
        assert!(requests[2].starts_with("GET /vhs/res/4.json "));
        assert!(requests[3].starts_with("GET /captcha.js?d="));
    }

    #[tokio::test]
    async fn board_queues_threads() {
        let (root, server) = serve(vec![
            html("tos"),
            html("tos"),
            json(json!({"pageCount": 2, "threads": [{"threadId": 1}]})),
            json(json!({"pageCount": 2, "threads": [{"threadId": 2}]})),
        ]);
        let http = client("8chan", root);
        assert_eq!(
            children(extract(
                &http,
                Url::parse("https://8chan.moe/vhs/").unwrap()
            ))
            .await
            .len(),
            2
        );
        server.join().unwrap();
    }
}
