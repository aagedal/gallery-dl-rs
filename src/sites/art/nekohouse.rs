//! Nekohouse creator posts, mirroring gallery_dl/extractor/nekohouse.py: a
//! post page lists its files and attachments; creator pages list 50 posts
//! per `o` offset, each queued as a post.
use super::{Scan, extr, extract_iter, raw_segments, segment};
use crate::error;
use crate::sites::{Events, Http, Item, Route, Site, unescape};
use futures_util::{StreamExt, stream};
use serde_json::Map;
use url::Url;

const ROOT: &str = "https://nekohouse.su";
static POST: Route = Route::new(
    "post",
    "{num:>02} {id|filename}.{extension}",
    &[
        "{category}",
        "{service}",
        "{username} ({user_id})",
        "{post_id} {date} {title[b:230]}",
    ],
    "{service}_{user_id}_{post_id}_{hash}",
);
static USER: Route = Route::new("user", "{filename}.{extension}", &["{category}"], "");

pub static SITE: Site = Site {
    category: "nekohouse",
    name: "Nekohouse",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&POST, &USER],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Post(String, String, String),
    User(String, String),
}

/// `nekohouse\.su/([^/?#]+)/user/([^/?#]+)` followed by `/post/([^/?#]+)`,
/// or by nothing but an optional slash, query and fragment.
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) || url.host_str()? != "nekohouse.su" {
        return None;
    }
    let path = url.path();
    match raw_segments(url).as_slice() {
        [service, "user", user, "post", post, ..]
            if path.starts_with(&format!("/{service}/user/{user}/post/{post}")) =>
        {
            Some(Target::Post(
                (*service).into(),
                (*user).into(),
                (*post).into(),
            ))
        }
        [service, "user", user]
            if segment(service)
                && (path == format!("/{service}/user/{user}")
                    || path == format!("/{service}/user/{user}/")) =>
        {
            Some(Target::User((*service).into(), (*user).into()))
        }
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Post(service, user, post)) => crate::sites::once(async move {
            let page_url = http.api(&format!("{ROOT}/{service}/user/{user}/post/{post}"))?;
            let page = http.text(&page_url).await?;
            let items = parse_post(&page, &service, &user, &post);
            Ok(crate::sites::events(crate::sites::post(
                &SITE, http, &POST, items,
            )))
        }),
        Some(Target::User(service, user)) => creator(http, service, user),
        None => crate::sites::fail(error("unsupported Nekohouse URL")),
    }
}

/// Upstream `NekohouseUserExtractor`: queue each `<article>`'s post until a
/// page lists fewer than 50.
fn creator(http: &Http, service: String, user: String) -> Events<'_> {
    crate::sites::paginate(Some(0u64), move |offset| {
        let base = format!("{ROOT}/{service}/user/{user}");
        async move {
            let Some(offset) = offset else {
                return Ok(None);
            };
            let mut url = http.api(&base)?;
            url.query_pairs_mut().append_pair("o", &offset.to_string());
            let page = http.text(&url).await?;
            let articles = extract_iter(&page, "<article", "</article>");
            let next = (articles.len() >= 50).then_some(offset + 50);
            let children: Vec<Url> = articles
                .into_iter()
                .filter_map(|post| {
                    Url::parse(&format!("{ROOT}{}", extr(post, "<a href=\"", "\""))).ok()
                })
                .filter(|child| matches!(target(child), Some(Target::Post(..))))
                .collect();
            let events = stream::iter(children)
                .map(move |child| {
                    crate::sites::queue(http, "nekohouse", "user", child, &Map::new())
                })
                .flatten()
                .boxed();
            Ok(Some((events, next)))
        }
    })
}

/// Upstream `_extract_files`: `(id, url, name, type)` for the files, then
/// the attachments.
fn files(page: &str) -> Vec<(String, String, Option<String>, &'static str)> {
    let mut files = Vec::new();
    let mut scan = Scan::new(extr(page, "class=\"scrape__files\"", "<footer"));
    loop {
        let id = scan.extr("<a href=\"/post/", "\"");
        if id.is_empty() {
            break;
        }
        let url = format!("{ROOT}{}", scan.extr("href=\"", "\""));
        files.push((id.to_owned(), url, None, "file"));
    }
    let mut scan = Scan::new(extr(page, "class=\"scrape__attachments\"", "</ul>"));
    loop {
        let href = scan.extr("href=\"", "\"");
        if href.is_empty() {
            break;
        }
        let name = unescape(scan.extr("download=\"", "\""));
        files.push((
            String::new(),
            format!("{ROOT}{href}"),
            Some(name),
            "attachment",
        ));
    }
    files
}

/// One post page's files with upstream keywords.
pub(crate) fn parse_post(page: &str, service: &str, user_id: &str, post_id: &str) -> Vec<Item> {
    let files = files(page);
    let mut scan = Scan::new(page);
    let mut post = Map::new();
    let username = scan.extr("class=\"scrape__user-name", "</");
    let username = unescape(username.rsplit('>').next().unwrap_or_default().trim());
    post.insert("username".into(), username.clone().into());
    let title = scan.extr("class=\"scrape__title", "</");
    let title = unescape(title.rsplit('>').next().unwrap_or_default());
    post.insert("title".into(), title.clone().into());
    let date = scan.extr("datetime=\"", "\"");
    let timestamp = super::parse_iso(date.get(..19).unwrap_or(date));
    post.insert("date".into(), super::date(timestamp));
    post.insert(
        "content".into(),
        unescape(scan.extr("class=\"scrape__content\">", "</div>").trim()).into(),
    );
    post.insert("service".into(), service.into());
    post.insert("user_id".into(), user_id.into());
    post.insert("post_id".into(), post_id.into());
    post.insert("count".into(), files.len().into());
    let mut items = Vec::new();
    for (index, (id, raw, name, kind)) in files.into_iter().enumerate() {
        let mut file = Map::new();
        file.insert("id".into(), id.into());
        file.insert("url".into(), raw.clone().into());
        file.insert("type".into(), kind.into());
        super::nameext_into(&raw, &mut file);
        file.insert("hash".into(), file["filename"].clone());
        post.insert("num".into(), (index + 1).into());
        super::update(&mut file, &post);
        if let Some(name) = name {
            let (stem, ext) = super::nameext_from_name(&name);
            file.insert("filename".into(), stem.into());
            file.insert("extension".into(), ext.into());
        }
        let Some(url) = Url::parse(&raw).ok().filter(crate::sites::plain) else {
            continue;
        };
        let hash = file
            .get("hash")
            .map(crate::sites::scalar)
            .unwrap_or_default();
        items.push(
            Item::new(url, file)
                .ids(post_id, hash)
                .user(Some(username.clone()))
                .caption(Some(title.clone()))
                .timestamp(timestamp),
        );
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, queued, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://nekohouse.su/fanbox/user/123/post/456"),
            Some(Target::Post("fanbox".into(), "123".into(), "456".into()))
        );
        assert_eq!(
            t("https://nekohouse.su/fanbox/user/123/?o=50"),
            Some(Target::User("fanbox".into(), "123".into()))
        );
        assert_eq!(
            t("https://nekohouse.su/fanbox/user/123"),
            Some(Target::User("fanbox".into(), "123".into()))
        );
        for raw in [
            "https://nekohouse.su/fanbox/user/123/other",
            "https://nekohouse.su/fanbox/123",
            "https://www.nekohouse.su/fanbox/user/123",
            "https://nekohouse.su.evil.com/fanbox/user/123",
            "https://nekohouse.su:8080/fanbox/user/123",
            "https://u:p@nekohouse.su/fanbox/user/123",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    const PAGE: &str = r#"<a class="scrape__user-name" href="/fanbox/user/123"><span>Artist &amp; Co </span></a>
        <h1 class="scrape__title"><span>Hello &amp; world</span></h1>
        <time datetime="2020-01-02T03:04:05.123456">x</time>
        <div class="scrape__content"> Text &amp; more </div>
        <div class="scrape__files"><a href="/post/f1"></a><a href="/data/ab/cd/abcdef.png?f=x.png"></a></div>
        <footer></footer>
        <ul class="scrape__attachments"><li><a href="/data/12/34/1234.zip" download="My &amp; File.ZIP"></a></li></ul>"#;

    #[tokio::test]
    async fn extracts_post() {
        let (root, server) = serve(vec![html(PAGE)]);
        let http = http("Nekohouse", root);
        let files = media(extract(
            &http,
            Url::parse("https://nekohouse.su/fanbox/user/123/post/456").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[0]);
        assert_eq!(k["id"], "f1");
        assert_eq!(k["hash"], "abcdef");
        assert_eq!(k["username"], "Artist & Co");
        assert_eq!(k["title"], "Hello & world");
        assert_eq!(k["content"], "Text & more");
        assert_eq!(k["date"], "2020-01-02 03:04:05");
        assert_eq!(k["count"], 2);
        let k = keywords(&files[1]);
        assert_eq!(k["type"], "attachment");
        assert_eq!(k["filename"], "My & File");
        assert_eq!(k["extension"], "zip");
        assert_eq!(k["hash"], "1234");
        assert_eq!(k["num"], 2);
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "01 f1.png");
        assert_eq!(file.render(&files[1]).unwrap(), "02 My & File.zip");
        let colon = if cfg!(windows) { "_" } else { ":" };
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from(format!(
                "nekohouse/fanbox/Artist & Co (123)/456 2020-01-02 03{colon}04{colon}05 Hello & world"
            ))
        );
        let (_, identity) = crate::sites::archive_key(&files[1]).unwrap();
        assert_eq!(
            identity,
            r#"["{service}_{user_id}_{post_id}_{hash}",["fanbox","123","456","1234"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /fanbox/user/123/post/456 "));
    }

    #[tokio::test]
    async fn queues_creator_posts() {
        let full: String = (0..50)
            .map(|i| format!("<article><a href=\"/fanbox/user/1/post/{i}\"></a></article>"))
            .collect();
        let (root, server) = serve(vec![
            html(&full),
            html("<article><a href=\"/fanbox/user/1/post/x\"></a></article>"),
        ]);
        let http = http("Nekohouse", root);
        let children = queued(extract(
            &http,
            Url::parse("https://nekohouse.su/fanbox/user/1").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 51);
        assert_eq!(children[0].subcategory(), "user");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /fanbox/user/1?o=0 "));
        assert!(requests[1].starts_with("GET /fanbox/user/1?o=50 "));
    }
}
