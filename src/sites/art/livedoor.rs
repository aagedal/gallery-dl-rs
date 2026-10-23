//! livedoor Blog, mirroring gallery_dl/extractor/livedoor.py.
use super::{Scan, extr, extract_iter, parse_int};
use crate::sites::{Events, Http, Item, Route, Site, unescape};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

pub static SITE: Site = Site {
    category: "livedoor",
    name: "livedoor Blog",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&BLOG, &POST],
    api_key: false,
};

const FILENAME: &str = "{post[id]}_{post[title]}_{num:>02}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{post[user]}"];
const ARCHIVE: &str = "{post[id]}_{hash}";
static BLOG: Route = Route::new("blog", FILENAME, DIRECTORY, ARCHIVE);
static POST: Route = Route::new("post", FILENAME, DIRECTORY, ARCHIVE);

const ROOT: &str = "http://blog.livedoor.jp";
const HOST: &str = "blog.livedoor.jp";

#[derive(Debug, PartialEq)]
enum Target {
    Blog(String),
    Post { user: String, post: String },
}

fn word(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) || url.host_str() != Some(HOST) {
        return None;
    }
    let path = url.path().strip_prefix('/')?;
    let (user, rest) = path.split_once('/').unwrap_or((path, ""));
    if !word(user) {
        return None;
    }
    if rest.is_empty() {
        return Some(Target::Blog(user.into()));
    }
    let id = rest.strip_prefix("archives/")?;
    let len = id.bytes().take_while(u8::is_ascii_digit).count();
    (len > 0).then(|| Target::Post {
        user: user.into(),
        post: id[..len].into(),
    })
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Blog(user)) => blog(http, user),
        Some(Target::Post { user, post }) => crate::sites::once(async move {
            let url = http.api(&format!("{ROOT}/{user}/archives/{post}.html"))?;
            let page = http
                .text_with(&url, &[("Referer", "http://blog.livedoor.jp/")])
                .await?;
            let mut scan = Scan::new(&page);
            let data = scan.extr("<rdf:RDF", "</rdf:RDF>");
            let body = scan.extr("class=\"article-body-inner\">", "class=\"article-footer\">");
            Ok(crate::sites::events(post_events(
                http,
                &POST,
                load(&user, data, body),
            )))
        }),
        None => crate::sites::fail(error("unsupported livedoor URL")),
    }
}

fn blog(http: &Http, user: String) -> Events<'_> {
    let first = format!("{ROOT}/{user}");
    crate::sites::paginate(Some(first), move |url| {
        let user = user.clone();
        async move {
            let Some(page_url) = url else {
                return Ok(None);
            };
            let url = http.api(&page_url)?;
            let page = http
                .text_with(&url, &[("Referer", "http://blog.livedoor.jp/")])
                .await?;
            let (events, next) = blog_page(http, &user, &page, &page_url)?;
            Ok(Some((crate::sites::events(events), next)))
        }
    })
}

fn blog_page(
    http: &Http,
    user: &str,
    page: &str,
    base: &str,
) -> Result<(Vec<crate::event::ExtractionEvent>, Option<String>)> {
    let mut scan = Scan::new(page);
    let mut events = Vec::new();
    loop {
        let data = scan.extr("<rdf:RDF", "</rdf:RDF>");
        if data.is_empty() {
            break;
        }
        let body = scan.extr("class=\"article-body-inner\">", "class=\"article-footer\">");
        events.extend(post_events(http, &BLOG, load(user, data, body)));
    }
    let next = scan.extr("<a rel=\"next\" href=\"", "\"");
    let next = (!next.is_empty())
        .then(|| Url::parse(base).ok()?.join(next).ok())
        .flatten()
        .filter(|u| crate::sites::plain(u) && u.host_str() == Some(HOST))
        .map(|u| u.to_string());
    Ok((events, next))
}

/// gallery-dl `text.split_html`.
fn split_html(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut rest = text;
    loop {
        let (piece, tail) = match rest.find('<') {
            Some(start) => match rest[start + 1..].find('>') {
                Some(end) if end > 0 => (&rest[..start], Some(&rest[start + end + 2..])),
                _ => (rest, None),
            },
            None => (rest, None),
        };
        if !piece.trim().is_empty() {
            parts.push(unescape(piece).trim().to_owned());
        }
        match tail {
            Some(tail) => rest = tail,
            None => break,
        }
    }
    parts
}

/// Upstream `_load`: the post object plus its body HTML.
fn load(user: &str, data: &str, body: &str) -> (Map<String, Value>, Option<i64>, String) {
    let mut scan = Scan::new(data);
    let tags = extr(body, "class=\"article-tags\">", "</dl>");
    let about = scan.extr("rdf:about=\"", "\"");
    let id = about.rsplit('/').next().unwrap_or_default();
    let id = id.split('.').next().unwrap_or_default();
    let title = unescape(scan.extr("dc:title=\"", "\""));
    let subject = scan.extr("dc:subject=\"", "\"");
    let (first, second) = subject.split_once(',').unwrap_or((subject, ""));
    let description = scan.extr("dc:description=\"", "\"");
    let timestamp = super::parse_iso(scan.extr("dc:date=\"", "\""));
    let mut post = Map::new();
    post.insert("id".into(), parse_int(id, 0).into());
    post.insert("title".into(), title.into());
    post.insert("categories".into(), serde_json::json!([first, second]));
    post.insert("description".into(), description.into());
    post.insert("date".into(), crate::filter::source_date_json(timestamp));
    let tags: Vec<String> = if tags.is_empty() {
        Vec::new()
    } else {
        split_html(tags).into_iter().skip(1).collect()
    };
    post.insert("tags".into(), tags.into());
    post.insert("user".into(), user.into());
    (post, timestamp, body.to_owned())
}

fn post_events(
    http: &Http,
    route: &'static Route,
    (post, timestamp, body): (Map<String, Value>, Option<i64>, String),
) -> Vec<crate::event::ExtractionEvent> {
    crate::sites::post(&SITE, http, route, images(&post, timestamp, &body))
}

fn images(post: &Map<String, Value>, timestamp: Option<i64>, body: &str) -> Vec<Item> {
    let id = post.get("id").map(crate::sites::scalar).unwrap_or_default();
    let title = post.get("title").and_then(Value::as_str).map(str::to_owned);
    let user = post.get("user").and_then(Value::as_str).map(str::to_owned);
    let mut items = Vec::new();
    for (index, img) in extract_iter(body, "<img ", ">").into_iter().enumerate() {
        let src = extr(img, "src=\"", "\"");
        let alt = extr(img, "alt=\"", "\"");
        if src.is_empty() {
            continue;
        }
        let raw = if src.contains("://livedoor.blogimg.jp/") {
            src.replacen("http:", "https:", 1).replace("-s.", ".")
        } else {
            src.to_owned()
        };
        let Some(url) = super::file_url(&raw, ROOT) else {
            continue;
        };
        let full = url.to_string();
        let last = full.rsplit('/').next().unwrap_or_default();
        let (name, ext) = last.rsplit_once('.').unwrap_or(("", last));
        let mut fields = Map::new();
        fields.insert("url".into(), full.clone().into());
        fields.insert("num".into(), (index + 1).into());
        fields.insert("hash".into(), name.into());
        fields.insert(
            "filename".into(),
            if alt.is_empty() { name } else { alt }.into(),
        );
        fields.insert("extension".into(), ext.into());
        fields.insert("post".into(), post.clone().into());
        items.push(
            Item::new(url, fields)
                .ids(id.clone(), name)
                .user(user.clone())
                .caption(title.clone())
                .timestamp(timestamp)
                .header("Referer", "http://blog.livedoor.jp/"),
        );
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, response, serve};
    use serde_json::json;

    fn t(raw: &str) -> Option<Target> {
        target(&Url::parse(raw).unwrap())
    }

    #[test]
    fn classifies_urls() {
        assert_eq!(
            t("http://blog.livedoor.jp/zatsu_ke/"),
            Some(Target::Blog("zatsu_ke".into()))
        );
        assert_eq!(
            t("http://blog.livedoor.jp/zatsu_ke?p=2"),
            Some(Target::Blog("zatsu_ke".into()))
        );
        assert_eq!(
            t("http://blog.livedoor.jp/zatsu_ke/archives/51493859.html"),
            Some(Target::Post {
                user: "zatsu_ke".into(),
                post: "51493859".into()
            })
        );
        for raw in [
            "http://blog.livedoor.jp/",
            "http://blog.livedoor.jp/zatsu_ke/other",
            "http://blog.livedoor.jp/zatsu_ke/archives/x",
            "http://www.blog.livedoor.jp/zatsu_ke/",
            "http://blog.livedoor.jp.evil.com/zatsu_ke/",
            "http://evilblog.livedoor.jp/zatsu_ke/",
            "http://blog.livedoor.jp:8080/zatsu_ke/",
            "http://a:b@blog.livedoor.jp/zatsu_ke/",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn article(id: u64) -> String {
        format!(
            r#"<rdf:RDF><rdf:Description rdf:about="http://blog.livedoor.jp/u/archives/{id}.html"
 dc:title="T &amp; {id}" dc:subject="Cat,Sub" dc:description="d" dc:date="2020-01-02T12:00:00+09:00"/></rdf:RDF>
<div class="article-body-inner"><img src="http://livedoor.blogimg.jp/u/imgs/a/b/abc-s.jpg" alt="nice">
<img alt="x"><img src="/img/rel.png"><dl class="article-tags"><dt>Tags</dt><dd><a>t1</a></dd> <dd>t&amp;2</dd></dl>
<div class="article-footer">"#
        )
    }

    #[tokio::test]
    async fn paginates_blog() {
        let first = format!(
            "{}<a rel=\"next\" href=\"http://blog.livedoor.jp/u/?p=2\">",
            article(1)
        );
        let (root, server) = serve(vec![html(&first), html(&article(2))]);
        let http = http("livedoor", root);
        let files = media(extract(
            &http,
            Url::parse("http://blog.livedoor.jp/u").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 4);
        assert_eq!(
            files[0].url.as_str(),
            "https://livedoor.blogimg.jp/u/imgs/a/b/abc.jpg"
        );
        assert_eq!(files[1].url.as_str(), "http://blog.livedoor.jp/img/rel.png");
        let k = keywords(&files[1]);
        assert_eq!(k["num"], 3);
        assert_eq!(k["hash"], "rel");
        assert_eq!(k["filename"], "rel");
        assert_eq!(k["extension"], "png");
        let k = keywords(&files[0]);
        assert_eq!(k["filename"], "nice");
        assert_eq!(k["post"]["id"], 1);
        assert_eq!(k["post"]["title"], "T & 1");
        assert_eq!(k["post"]["categories"], json!(["Cat", "Sub"]));
        assert_eq!(k["post"]["tags"], json!(["t1", "t&2"]));
        assert_eq!(k["post"]["user"], "u");
        assert!(!k["post"].as_object().unwrap().contains_key("body"));
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "1_T & 1_01.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("livedoor/u")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{post[id]}_{hash}",["1","abc"]]"#);
        assert_eq!(
            files[0].metadata.as_ref().unwrap().timestamp,
            Some(1577934000)
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /u "), "{}", requests[0]);
        assert!(requests[1].starts_with("GET /u/?p=2 "));
    }

    #[tokio::test]
    async fn extracts_post() {
        let (root, server) = serve(vec![html(&article(7))]);
        let http = http("livedoor", root);
        let files = media(extract(
            &http,
            Url::parse("http://blog.livedoor.jp/u/archives/7.html").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(keywords(&files[0])["subcategory"], "post");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /u/archives/7.html "));
    }

    #[tokio::test]
    async fn reports_missing_page() {
        let (root, _server) = serve(vec![response("404 Not Found", "text/html", b"")]);
        let http = http("livedoor", root);
        let events: Vec<_> = futures_util::StreamExt::collect(extract(
            &http,
            Url::parse("http://blog.livedoor.jp/u/archives/7.html").unwrap(),
        ))
        .await;
        let message = events[0].as_ref().err().unwrap().to_string();
        assert!(message.contains("not found"), "{message}");
    }
}
