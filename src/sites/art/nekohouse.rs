//! Nekohouse posts and users, mirroring gallery_dl/extractor/nekohouse.py.
use crate::error;
use crate::sites::gdl::{
    Scan, captures, date, extr, extract_iter, group, nameext_into, parse_iso, re, rpartition,
    unescape,
};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://nekohouse.su";
const USER_PATTERN: &str = r"^https?://nekohouse\.su/([^/?#]+)/user/([^/?#]+)";

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

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(re!(&format!(r"{USER_PATTERN}/post/([^/?#]+)")), url) {
        return Some(Target::Post(
            group(&c, 1).into(),
            group(&c, 2).into(),
            group(&c, 3).into(),
        ));
    }
    captures(
        re!(&format!(r"{USER_PATTERN}/?(?:\?([^#]+))?(?:$|\?|#)")),
        url,
    )
    .map(|c| Target::User(group(&c, 1).into(), group(&c, 2).into()))
}

/// `_extract_files`: post files, then attachments.
fn files(html: &str) -> Vec<Map<String, Value>> {
    let mut files = Vec::new();
    let mut scan = Scan::new(extr(html, "class=\"scrape__files\"", "<footer"));
    loop {
        let id = scan.extr("<a href=\"/post/", "\"");
        if id.is_empty() {
            break;
        }
        let mut file = Map::new();
        file.insert("id".into(), id.into());
        file.insert(
            "url".into(),
            format!("{ROOT}{}", scan.extr("href=\"", "\"")).into(),
        );
        file.insert("type".into(), "file".into());
        files.push(file);
    }
    let mut scan = Scan::new(extr(html, "class=\"scrape__attachments\"", "</ul>"));
    loop {
        let url = scan.extr("href=\"", "\"");
        if url.is_empty() {
            break;
        }
        let mut file = Map::new();
        file.insert("id".into(), "".into());
        file.insert("url".into(), format!("{ROOT}{url}").into());
        file.insert(
            "name".into(),
            unescape(scan.extr("download=\"", "\"")).into(),
        );
        file.insert("type".into(), "attachment".into());
        files.push(file);
    }
    files
}

fn post_fields(html: &str) -> Map<String, Value> {
    let mut scan = Scan::new(html);
    let mut post = Map::new();
    post.insert(
        "username".into(),
        unescape(
            rpartition(scan.extr("class=\"scrape__user-name", "</"), ">")
                .2
                .trim(),
        )
        .into(),
    );
    post.insert(
        "title".into(),
        unescape(rpartition(scan.extr("class=\"scrape__title", "</"), ">").2).into(),
    );
    let stamp = scan.extr("datetime=\"", "\"");
    post.insert(
        "date".into(),
        date(parse_iso(stamp.get(..19).unwrap_or(stamp))),
    );
    post.insert(
        "content".into(),
        unescape(scan.extr("class=\"scrape__content\">", "</div>").trim()).into(),
    );
    post
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Nekohouse URL"));
    };
    match target {
        Target::Post(service, user_id, post_id) => crate::sites::once(async move {
            let html = http
                .text(&http.api(&format!("{ROOT}/{service}/user/{user_id}/post/{post_id}"))?)
                .await?;
            let files = files(&html);
            let mut post = post_fields(&html);
            post.insert("service".into(), service.as_str().into());
            post.insert("user_id".into(), user_id.as_str().into());
            post.insert("post_id".into(), post_id.as_str().into());
            post.insert("count".into(), files.len().into());
            let items = files
                .into_iter()
                .enumerate()
                .filter_map(|(index, mut file)| {
                    let url = crate::sites::scalar(&file["url"]);
                    nameext_into(&url, &mut file);
                    file.insert("hash".into(), file["filename"].clone());
                    file.extend(post.clone());
                    file.insert("num".into(), (index + 1).into());
                    if let Some(name) = file.remove("name") {
                        nameext_into(&crate::sites::scalar(&name), &mut file);
                    }
                    Some(
                        Item::new(Url::parse(&url).ok()?, file)
                            .ids(post_id.clone(), (index + 1).to_string()),
                    )
                })
                .collect();
            Ok(crate::sites::events(crate::sites::post(
                &SITE, http, &POST, items,
            )))
        }),
        Target::User(service, user_id) => crate::sites::paginate(Some(0u64), move |state| {
            let creator = format!("{ROOT}/{service}/user/{user_id}");
            async move {
                let Some(offset) = state else { return Ok(None) };
                let mut url = http.api(&creator)?;
                url.query_pairs_mut().append_pair("o", &offset.to_string());
                let html = http.text(&url).await?;
                let posts = extract_iter(&html, "<article", "</article>");
                let next = (posts.len() >= 50).then_some(offset + 50);
                let children = posts
                    .into_iter()
                    .filter_map(|post| {
                        Some((
                            Url::parse(&format!("{ROOT}{}", extr(post, "<a href=\"", "\"")))
                                .ok()?,
                            Map::new(),
                        ))
                    })
                    .collect();
                Ok(Some((
                    crate::sites::galleries::queue(http, &SITE, &USER, children),
                    next,
                )))
            }
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http as client, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://nekohouse.su/fanbox/user/123/post/456"),
            Some(Target::Post("fanbox".into(), "123".into(), "456".into()))
        );
        assert_eq!(
            t("https://nekohouse.su/fanbox/user/123?o=50"),
            Some(Target::User("fanbox".into(), "123".into()))
        );
    }

    #[tokio::test]
    async fn extracts_files_and_attachments() {
        let page = r#"<span class="scrape__user-name">  Artist </span><h1 class="scrape__title"><span>Title</span></h1><time datetime="2024-01-02T03:04:05.000">
            <div class="scrape__content"> Text </div><div class="scrape__files"><a href="/post/abc"><a href="/data/aa/bb/hash1.jpg"></a></div>
            <ul class="scrape__attachments"><a href="/data/cc/hash2.zip" download="pack &amp; more.zip"></ul><footer>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = client("nekohouse", root);
        let files = media(extract(
            &http,
            Url::parse("https://nekohouse.su/fanbox/user/1/post/2").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[1]);
        assert_eq!(
            (
                k["hash"].as_str(),
                k["filename"].as_str(),
                k["username"].as_str()
            ),
            (Some("hash2"), Some("pack & more"), Some("Artist"))
        );
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "01 abc.jpg");
        assert_eq!(file.render(&files[1]).unwrap(), "02 pack & more.zip");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("nekohouse/fanbox/Artist (1)/2 2024-01-02 03:04:05 Title")
        );
        server.join().unwrap();
    }
}
