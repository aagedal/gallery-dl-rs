//! imgpile posts and users, mirroring gallery_dl/extractor/imgpile.py: post
//! pages list their media blocks, and user listings come from the JSON
//! posts API and queue each post page.
use super::Extr;
use crate::sites::art::{extr as text, split_html};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

pub static SITE: Site = Site {
    category: "imgpile",
    name: "imgpile",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&POST, &USER],
    api_key: false,
    cookies: None,
};

const DIRECTORY: &[&str] = &[
    "{category}",
    "{post[author]}",
    "{post[title]} ({post[id_slug]})",
];
const ARCHIVE: &str = "{post[id_slug]}_{id}";
static POST: Route = Route::new("post", "{filename}.{extension}", DIRECTORY, ARCHIVE);
static USER: Route = Route::new("user", "{filename}.{extension}", DIRECTORY, ARCHIVE);

const ROOT: &str = "https://imgpile.com";

#[derive(Debug, PartialEq)]
enum Target {
    Post(String),
    User(String),
}

/// `/p/(\w+)` and `/u/([^/?#]+)`.
fn target(url: &Url) -> Option<Target> {
    if !super::on_host(url, &["imgpile.com"]) {
        return None;
    }
    let path = url.path();
    if let Some(rest) = path.strip_prefix("/p/") {
        let len = rest
            .bytes()
            .position(|b| !(b.is_ascii_alphanumeric() || b == b'_'))
            .unwrap_or(rest.len());
        return (len > 0).then(|| Target::Post(rest[..len].to_owned()));
    }
    let user = path.strip_prefix("/u/")?.split('/').next()?;
    (!user.is_empty()).then(|| Target::User(user.to_owned()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Post(id)) => crate::sites::once(async move {
            let page = http.text(&http.api(&format!("{ROOT}/p/{id}"))?).await?;
            let items = parse_post(&page, &id)?;
            Ok(crate::sites::events(crate::sites::post(
                &SITE, http, &POST, items,
            )))
        }),
        Some(Target::User(user)) => posts(http, user),
        None => crate::sites::fail(error("unsupported imgpile URL")),
    }
}

/// `ImgpilePostExtractor.items`: post metadata, then each media block.
fn parse_post(page: &str, id: &str) -> Result<Vec<Item>> {
    let mut extr = Extr::new(page);
    let mut post = Map::new();
    post.insert("id_slug".into(), id.into());
    let title = crate::sites::unescape(extr.next("<title>", " - imgpile<"));
    post.insert("title".into(), title.clone().into());
    post.insert(
        "id".into(),
        super::parse_int(extr.next("data-post-id=\"", "\"")).into(),
    );
    let author = extr.next("/u/", "\"").to_owned();
    post.insert("author".into(), author.clone().into());
    let score = crate::sites::remove_html(extr.next("class=\"post-score\">", "</"));
    post.insert("score".into(), super::parse_int(&score).into());
    let views = extr.next("class=\"meta-value\">", "<").replace(',', "");
    post.insert("views".into(), super::parse_int(&views).into());
    let tags = split_html(extr.next(" <!-- Tags -->", "<!-- \""));
    post.insert("tags".into(), tags.into());
    let mut files = Vec::new();
    loop {
        let media = extr.next("lass=\"post-media", "</div>");
        if media.is_empty() {
            break;
        }
        files.push((
            text(media, "data-id=\"", "\"").to_owned(),
            super::parse_int(text(media, "data-media-id=\"", "\"")),
            format!("http{}", text(media, "<a href=\"http", "\"")),
        ));
    }
    post.insert("count".into(), files.len().into());
    let count = files.len();
    let mut items = Vec::new();
    for (index, (slug, media_id, url)) in files.into_iter().enumerate() {
        let mut data = Map::new();
        data.insert("post".into(), Value::Object(post.clone()));
        data.insert("count".into(), count.into());
        data.insert("num".into(), (index + 1).into());
        data.insert("id_slug".into(), slug.into());
        data.insert("id".into(), media_id.into());
        data.insert("url".into(), url.clone().into());
        super::nameext_from_url(&url, &mut data);
        let Some(file) = super::web_url(&url) else {
            return Err(error("imgpile returned an invalid file URL"));
        };
        items.push(
            Item::new(file, data)
                .ids(id, media_id.to_string())
                .user(Some(author.clone()))
                .caption(Some(title.clone())),
        );
    }
    Ok(items)
}

/// `ImgpileUserExtractor.items`: API pages following `links.next`,
/// queueing each post.
fn posts(http: &Http, user: String) -> Events<'_> {
    let mut first = Url::parse(&format!("{ROOT}/api/v1/posts")).expect("static imgpile URL");
    first
        .query_pairs_mut()
        .append_pair("limit", "100")
        .append_pair("sort", "latest")
        .append_pair("period", "all")
        .append_pair("visibility", "public")
        .append_pair("username", &user);
    let start = (Some(String::from(first)), None::<Value>);
    crate::sites::paginate(start, move |(next, total)| async move {
        let Some(url) = next else {
            return Ok(None);
        };
        let url = http.api(&url)?;
        let headers = [
            ("Accept", "application/json"),
            ("Content-Type", "application/json"),
            ("Sec-Fetch-Dest", "empty"),
            ("Sec-Fetch-Mode", "cors"),
            ("Sec-Fetch-Site", "same-origin"),
        ];
        let data = http.json_with(&url, &headers).await?;
        // `self.kwdict["total"]`, from the first page.
        let total =
            total.unwrap_or_else(|| data.pointer("/meta/total").cloned().unwrap_or(Value::Null));
        let children = data
            .get("data")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|item| {
                let mut item = item.as_object()?.clone();
                let slug = item.get("slug").map(crate::sites::scalar)?;
                let url = Url::parse(&format!("{ROOT}/p/{slug}")).ok()?;
                matches!(target(&url), Some(Target::Post(_))).then_some(())?;
                item.insert("total".into(), total.clone());
                Some((url, item))
            })
            .collect();
        let next = data
            .pointer("/links/next")
            .and_then(Value::as_str)
            .and_then(super::web_url)
            .filter(|next| super::on_host(next, &["imgpile.com"]))
            .map(String::from);
        let events = super::queue_all(http, SITE.category, USER.subcategory, children);
        Ok(Some((events, (next, Some(total)))))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, json, keywords, media, queued, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://imgpile.com/p/AbC_1-x"),
            Some(Target::Post("AbC_1".into()))
        );
        assert_eq!(
            t("https://www.imgpile.com/u/someone/posts"),
            Some(Target::User("someone".into()))
        );
        for raw in [
            "https://imgpile.com/p/",
            "https://imgpile.com/u/",
            "https://cdn.imgpile.com/p/abc",
            "https://imgpile.com.evil.test/p/abc",
            "http://imgpile.com:81/p/abc",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    const PAGE: &str = "<title>Sunset &amp; Sea - imgpile</title>\
        <div data-post-id=\"77\"><a href=\"https://imgpile.com/u/alice\">alice</a>\
        <span class=\"post-score\"><b>12</b></span><span class=\"meta-value\">1,234</span> \
        <!-- Tags --><a>sun</a><a>sea</a><!-- \"end\" -->\
        <div class=\"post-media\" data-id=\"m1\" data-media-id=\"501\"><a href=\"https://cdn.imgpile.com/a/one.jpg\"></a></div>\
        <div class=\"post-media\" data-id=\"m2\" data-media-id=\"502\"><a href=\"https://cdn.imgpile.com/a/two.png\"></a></div>";

    #[tokio::test]
    async fn extracts_post_media() {
        let (root, server) = serve(vec![html(PAGE)]);
        let http = http("imgpile", root);
        let files = media(extract(
            &http,
            Url::parse("https://imgpile.com/p/AbCdEfG").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[1]);
        assert_eq!(k["post"]["title"], "Sunset & Sea");
        assert_eq!(k["post"]["id"], 77);
        assert_eq!(k["post"]["author"], "alice");
        assert_eq!(k["post"]["score"], 12);
        assert_eq!(k["post"]["views"], 1234);
        assert_eq!(k["post"]["tags"], json!(["sun", "sea"]));
        assert_eq!(k["post"]["count"], 2);
        assert_eq!(k["id_slug"], "m2");
        assert_eq!(k["id"], 502);
        assert_eq!(k["num"], 2);
        assert_eq!(k["filename"], "two");
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "two.png");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("imgpile/alice/Sunset & Sea (AbCdEfG)")
        );
        assert_eq!(
            crate::sites::archive_key(&files[1]).unwrap().1,
            r#"["{post[id_slug]}_{id}",["AbCdEfG","502"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /p/AbCdEfG "));
    }

    #[tokio::test]
    async fn user_pages_follow_next_links() {
        let first = json!({"meta": {"total": 2}, "data": [{"slug": "P1", "id": 1}],
            "links": {"next": "https://imgpile.com/api/v1/posts?cursor=abc"}});
        let second = json!({"data": [{"slug": "P2", "id": 2}], "links": {"next": null}});
        let (root, server) = serve(vec![json(first), json(second)]);
        let http = http("imgpile", root);
        let children = queued(extract(
            &http,
            Url::parse("https://imgpile.com/u/alice").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 2);
        assert_eq!(children[1].subcategory(), "user");
        assert_eq!(children[1].fields()["id"], 2);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with(
            "GET /api/v1/posts?limit=100&sort=latest&period=all&visibility=public&username=alice "
        ));
        assert!(requests[0].contains("accept: application/json"));
        assert!(requests[1].starts_with("GET /api/v1/posts?cursor=abc "));
    }
}
