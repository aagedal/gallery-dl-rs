//! imgpile posts and users, mirroring gallery_dl/extractor/imgpile.py.
use crate::error;
use crate::sites::gdl::{
    Scan, captures, extr, group, nameext_into, parse_int, re, remove_html, split_html, unescape,
};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://imgpile.com";
const DIRECTORY: &[&str] = &[
    "{category}",
    "{post[author]}",
    "{post[title]} ({post[id_slug]})",
];

static POST: Route = Route::new(
    "post",
    "{filename}.{extension}",
    DIRECTORY,
    "{post[id_slug]}_{id}",
);
static USER: Route = Route::new(
    "user",
    "{filename}.{extension}",
    DIRECTORY,
    "{post[id_slug]}_{id}",
);

pub static SITE: Site = Site {
    category: "imgpile",
    name: "imgpile",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&POST, &USER],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Post(String),
    User(String),
}

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(re!(r"^https?://(?:www\.)?imgpile\.com/p/(\w+)"), url) {
        return Some(Target::Post(group(&c, 1).into()));
    }
    captures(re!(r"^https?://(?:www\.)?imgpile\.com/u/([^/?#]+)"), url)
        .map(|c| Target::User(group(&c, 1).into()))
}

fn post_items(page: &str, slug: &str) -> Vec<Item> {
    if !page.contains("lass=\"post-media")
        && let Some(data) = crate::sites::gdl::turbo_stream(page)
    {
        return loader_items(&data["loaderData"]["routes/post"]["doc"], slug);
    }
    let mut scan = Scan::new(page);
    let mut post = Map::new();
    post.insert("id_slug".into(), slug.into());
    post.insert(
        "title".into(),
        unescape(scan.extr("<title>", " - imgpile<")).into(),
    );
    post.insert(
        "id".into(),
        parse_int(scan.extr("data-post-id=\"", "\"")).into(),
    );
    post.insert("author".into(), scan.extr("/u/", "\"").into());
    post.insert(
        "score".into(),
        parse_int(&remove_html(scan.extr("class=\"post-score\">", "</"))).into(),
    );
    post.insert(
        "views".into(),
        parse_int(&scan.extr("class=\"meta-value\">", "<").replace(',', "")).into(),
    );
    post.insert(
        "tags".into(),
        split_html(scan.extr(" <!-- Tags -->", "<!-- \"")).into(),
    );
    let mut files = Vec::new();
    while let Some(media) = scan
        .find("lass=\"post-media", "</div>")
        .filter(|m| !m.is_empty())
    {
        files.push((
            extr(media, "data-id=\"", "\"").to_owned(),
            parse_int(extr(media, "data-media-id=\"", "\"")),
            format!("http{}", extr(media, "<a href=\"http", "\"")),
        ));
    }
    post.insert("count".into(), files.len().into());
    let count = files.len();
    files
        .into_iter()
        .enumerate()
        .filter_map(|(index, (id_slug, id, raw))| {
            let url = Url::parse(&raw).ok()?;
            let mut data = Map::new();
            data.insert("post".into(), Value::Object(post.clone()));
            data.insert("count".into(), count.into());
            data.insert("num".into(), (index + 1).into());
            data.insert("id_slug".into(), id_slug.into());
            data.insert("id".into(), id.into());
            data.insert("url".into(), raw.as_str().into());
            nameext_into(&raw, &mut data);
            Some(Item::new(url, data).ids(slug, id.to_string()))
        })
        .collect()
}

/// [`post_items`] for the React Router site: its `routes/post` loader doc.
fn loader_items(doc: &Value, slug: &str) -> Vec<Item> {
    let tags: Vec<Value> = doc["tags"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|tag| tag["name"].clone())
        .collect();
    let mut post = Map::new();
    post.insert("id_slug".into(), slug.into());
    post.insert("title".into(), doc["title"].clone());
    post.insert("id".into(), doc["id"].clone());
    post.insert("author".into(), doc["user"]["username"].clone());
    post.insert("score".into(), doc["score"].clone());
    post.insert("views".into(), doc["viewCount"].clone());
    post.insert("tags".into(), tags.into());
    let media = doc["media"].as_array().cloned().unwrap_or_default();
    post.insert("count".into(), media.len().into());
    let count = media.len();
    media
        .iter()
        .enumerate()
        .filter_map(|(index, file)| {
            let raw = file["urls"]["original"].as_str()?.to_owned();
            let url = Url::parse(&raw).ok()?;
            let mut data = Map::new();
            data.insert("post".into(), Value::Object(post.clone()));
            data.insert("count".into(), count.into());
            data.insert("num".into(), (index + 1).into());
            data.insert("id_slug".into(), file["slug"].clone());
            data.insert("id".into(), file["id"].clone());
            data.insert("url".into(), raw.as_str().into());
            nameext_into(&raw, &mut data);
            let id = crate::sites::scalar(&file["id"]);
            Some(Item::new(url, data).ids(slug, id))
        })
        .collect()
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported imgpile URL"));
    };
    match target {
        Target::Post(slug) => crate::sites::once(async move {
            let page = http.text(&http.api(&format!("{ROOT}/p/{slug}"))?).await?;
            Ok(crate::sites::events(crate::sites::post(
                &SITE,
                http,
                &POST,
                post_items(&page, &slug),
            )))
        }),
        Target::User(user) => {
            let mut first = match http.api(&format!("{ROOT}/api/v1/posts")) {
                Ok(url) => url,
                Err(e) => return crate::sites::fail(e),
            };
            first
                .query_pairs_mut()
                .append_pair("limit", "100")
                .append_pair("sort", "latest")
                .append_pair("period", "all")
                .append_pair("visibility", "public")
                .append_pair("username", &user);
            crate::sites::paginate(Some(first), move |state| async move {
                let Some(url) = state else { return Ok(None) };
                let data = http
                    .json_with(
                        &url,
                        &[
                            ("Accept", "application/json"),
                            ("Content-Type", "application/json"),
                            ("Sec-Fetch-Dest", "empty"),
                            ("Sec-Fetch-Mode", "cors"),
                            ("Sec-Fetch-Site", "same-origin"),
                        ],
                    )
                    .await?;
                let total = data["meta"]["total"].clone();
                let children = data["data"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|item| {
                        let url = Url::parse(&format!(
                            "{ROOT}/p/{}",
                            crate::sites::scalar(&item["slug"])
                        ))
                        .ok()?;
                        let mut item = item.as_object().cloned().unwrap_or_default();
                        item.insert("total".into(), total.clone());
                        Some((url, item))
                    })
                    .collect::<Vec<_>>();
                let next = data["links"]
                    .get("next")
                    .and_then(Value::as_str)
                    .and_then(|n| http.api(n).ok());
                let events = futures_util::StreamExt::boxed(futures_util::StreamExt::flatten(
                    futures_util::stream::iter(children.into_iter().map(move |(url, item)| {
                        crate::sites::queue(http, SITE.category, USER.subcategory, url, &item)
                    })),
                ));
                Ok(Some((events, next)))
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{children, html, http as client, json, keywords, media, serve};

    #[test]
    fn reads_turbo_stream_loader() {
        // [root, "loaderData", {routes/post}, "routes/post", {doc}, "doc", ...]
        let values = serde_json::json!([
            {"_1": 2}, "loaderData", {"_3": 4}, "routes/post", {"_5": 6}, "doc",
            {"_7": 8, "_9": 10, "_11": 12}, "id", 105411, "title", "Scans", "media", [13],
            {"_14": 15, "_16": 17, "_18": 19}, "id", 3518940, "slug", "BobTUou", "urls", {"_20": 21},
            "original", "https://cdn.imgpile.com/f/BobTUou.jpg"
        ]);
        let page = format!(
            "<script>window.__reactRouterContext.streamController.enqueue({});</script>",
            serde_json::to_string(&format!("{values}\n")).unwrap()
        );
        let items = post_items(&page, "bbjiXrl");
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].url.as_str(),
            "https://cdn.imgpile.com/f/BobTUou.jpg"
        );
        assert_eq!(items[0].keywords["post"]["title"], "Scans");
        assert_eq!(items[0].keywords["filename"], "BobTUou");
    }

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://imgpile.com/p/bbjiXrl"),
            Some(Target::Post("bbjiXrl".into()))
        );
        assert_eq!(
            t("https://imgpile.com/u/zilla_64"),
            Some(Target::User("zilla_64".into()))
        );
        assert_eq!(t("https://imgpile.com/x"), None);
    }

    #[tokio::test]
    async fn extracts_post() {
        let page = r#"<title>Cats &amp; Dogs - imgpile</title><div data-post-id="77"><a href="/u/zilla"></a>
            <span class="post-score"><b>12</b></span><span class="meta-value">1,234</span>
             <!-- Tags --><a>cat</a><a>dog</a><!-- "
            <div class="post-media" data-id="m1" data-media-id="5"><a href="https://cdn.imgpile.com/a.jpg"></a></div>
            <div class="post-media" data-id="m2" data-media-id="6"><a href="https://cdn.imgpile.com/b.mp4"></a></div>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = client("imgpile", root);
        let files = media(extract(
            &http,
            Url::parse("https://imgpile.com/p/bbjiXrl").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[1]);
        assert_eq!(k["post"]["views"], 1234);
        assert_eq!(k["post"]["tags"], serde_json::json!(["cat", "dog"]));
        assert_eq!(k["id"], 6);
        let (_, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("imgpile/zilla/Cats & Dogs (bbjiXrl)")
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn user_follows_next_links() {
        let (root, server) = serve(vec![
            json(
                serde_json::json!({"meta": {"total": 2}, "data": [{"slug": "a"}], "links": {"next": "https://imgpile.com/api/v1/posts?cursor=2"}}),
            ),
            json(
                serde_json::json!({"meta": {"total": 2}, "data": [{"slug": "b"}], "links": {"next": null}}),
            ),
        ]);
        let http = client("imgpile", root);
        assert_eq!(
            children(extract(
                &http,
                Url::parse("https://imgpile.com/u/zilla").unwrap()
            ))
            .await
            .len(),
            2
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with(
            "GET /api/v1/posts?limit=100&sort=latest&period=all&visibility=public&username=zilla "
        ));
        assert!(requests[1].starts_with("GET /api/v1/posts?cursor=2 "));
    }
}
