//! Piczel galleries, mirroring gallery_dl/extractor/piczel.py through the
//! public `/api` JSON endpoints.
use super::{digits, on_host, parse_iso, raw_segments, segment, str_of};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://piczel.tv";
/// `piczel.tv/api` was retired for this host, with pages in the path.
const API: &str = "https://api.piczel.tv";
const FILENAME: &str = "{category}_{id}_{title}_{num:>02}.{extension}";
static USER: Route = Route::new(
    "user",
    FILENAME,
    &["{category}", "{user[username]}"],
    "{id}_{num}",
);
static FOLDER: Route = Route::new(
    "folder",
    FILENAME,
    &["{category}", "{user[username]}", "{folder[name]}"],
    "f{folder[id]}_{id}_{num}",
);
static IMAGE: Route = Route::new(
    "image",
    FILENAME,
    &["{category}", "{user[username]}"],
    "{id}_{num}",
);

pub static SITE: Site = Site {
    category: "piczel",
    name: "Piczel",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&USER, &FOLDER, &IMAGE],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    User(String),
    Folder(String),
    Image(String),
}

/// Leading ASCII digits of a segment, as upstream `(\d+)` without anchoring.
fn leading_digits(text: &str) -> Option<&str> {
    let len = text.bytes().take_while(u8::is_ascii_digit).count();
    digits(&text[..len]).then(|| &text[..len])
}

fn target(url: &Url) -> Option<Target> {
    if !on_host(url, &["piczel.tv"]) {
        return None;
    }
    let path = url.path();
    let rest = path.strip_prefix("/gallery/")?;
    // `/gallery/([^/?#]+)/?$`: nothing may follow, not even a query.
    let user = rest.strip_suffix('/').unwrap_or(rest);
    if segment(user) && url.query().is_none() && url.fragment().is_none() {
        return Some(Target::User(user.into()));
    }
    let parts = raw_segments(url);
    match parts.as_slice() {
        ["gallery", first, second, ..] if *first != "image" => {
            leading_digits(second).map(|id| Target::Folder(id.into()))
        }
        ["gallery", "image", id, ..] => leading_digits(id).map(|id| Target::Image(id.into())),
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::User(user)) => {
            pagination(http, &USER, format!("{API}/users/{user}/gallery/{{page}}"))
        }
        Some(Target::Folder(id)) => {
            pagination(http, &FOLDER, format!("{API}/gallery/folder/{id}/page/{{page}}"))
        }
        Some(Target::Image(id)) => crate::sites::once(async move {
            let url = http.api(&format!("{API}/gallery/{id}"))?;
            let post = http.json(&url).await?;
            Ok(crate::sites::events(post_events(http, &IMAGE, post)?))
        }),
        None => crate::sites::fail(error("unsupported Piczel URL")),
    }
}

fn pagination<'a>(http: &'a Http, route: &'static Route, endpoint: String) -> Events<'a> {
    crate::sites::paginate(Some(1i64), move |page| {
        let endpoint = endpoint.clone();
        async move {
            let Some(page) = page else {
                return Ok(None);
            };
            let url = http.api(&endpoint.replace("{page}", &page.to_string()))?;
            let body = http.json(&url).await?;
            let mut events = Vec::new();
            for post in body
                .get("data")
                .and_then(Value::as_array)
                .ok_or_else(|| error("Piczel response contains no data"))?
            {
                events.extend(post_events(http, route, post.clone())?);
            }
            // `meta.next_page` is falsy at the end; guard against loops.
            let next = body
                .pointer("/meta/next_page")
                .and_then(Value::as_i64)
                .filter(|next| *next > page);
            Ok(Some((crate::sites::events(events), next)))
        }
    })
}

fn post_events(
    http: &Http,
    route: &'static Route,
    post: Value,
) -> Result<Vec<crate::event::ExtractionEvent>> {
    let items = parse_post(post)?;
    Ok(crate::sites::post(&SITE, http, route, items))
}

/// One post's files, following upstream's in-place updates of `post`.
pub(crate) fn parse_post(post: Value) -> Result<Vec<Item>> {
    let Value::Object(mut post) = post else {
        return Err(error("Piczel post is not an object"));
    };
    let tags: Vec<Value> = post
        .get("tags")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|tag| tag.get("title"))
        .filter(|title| super::truthy(Some(title)))
        .cloned()
        .collect();
    post.insert("tags".into(), tags.into());
    let created = parse_iso(
        post.get("created_at")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    post.insert("date".into(), super::date(created));
    let images = if super::truthy(post.get("multi")) {
        let images = match post.remove("images") {
            Some(Value::Array(images)) => images,
            _ => Vec::new(),
        };
        post.insert("count".into(), images.len().into());
        images
    } else {
        post.insert("count".into(), 1.into());
        vec![Value::Object(Map::new())]
    };
    let mut items = Vec::new();
    for (num, image) in images.into_iter().enumerate() {
        if let Value::Object(mut image) = image {
            image.remove("id");
            super::update(&mut post, &image);
        }
        post.insert("num".into(), num.into());
        let Some(url) = post
            .get("image")
            .map(|image| str_of(image, "url"))
            .and_then(|raw| super::file_url(raw, ROOT))
        else {
            continue;
        };
        let mut fields = post.clone();
        super::nameext_into(url.as_str(), &mut fields);
        let id = fields
            .get("id")
            .map(crate::sites::scalar)
            .unwrap_or_default();
        let user = fields
            .get("user")
            .map(|user| str_of(user, "username").to_owned());
        let title = fields
            .get("title")
            .and_then(Value::as_str)
            .map(str::to_owned);
        items.push(
            Item::new(url, fields)
                .ids(id.clone(), format!("{id}_{num}"))
                .user(user)
                .caption(title)
                .timestamp(created),
        );
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http, json, keywords, media, serve};
    use futures_util::StreamExt;
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://piczel.tv/gallery/Bikupan"),
            Some(Target::User("Bikupan".into()))
        );
        assert_eq!(
            t("https://www.piczel.tv/gallery/Bikupan/"),
            Some(Target::User("Bikupan".into()))
        );
        assert_eq!(
            t("https://piczel.tv/gallery/Lulena/1114"),
            Some(Target::Folder("1114".into()))
        );
        assert_eq!(
            t("https://piczel.tv/gallery/image/7807"),
            Some(Target::Image("7807".into()))
        );
        for raw in [
            "https://piczel.tv/gallery/Bikupan?x=1",
            "https://piczel.tv/gallery/",
            "https://piczel.tv/gallery/image/abc",
            "https://piczel.tv/gallery/user/abc",
            "https://piczel.tv.evil.com/gallery/Bikupan",
            "https://evilpiczel.tv/gallery/Bikupan",
            "https://a.piczel.tv/gallery/Bikupan",
            "https://piczel.tv:8080/gallery/Bikupan",
            "https://u:p@piczel.tv/gallery/Bikupan",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn multi() -> Value {
        json!({"id": 7, "title": "Two", "multi": true, "created_at": "2020-01-02T03:04:05.000Z",
            "tags": [{"title": "a"}, {"title": ""}], "user": {"username": "me"},
            "folder": {"id": 3, "name": "F"},
            "image": {"url": "https://apollo.piczel.tv/static/x/main.png"},
            "images": [
                {"id": 1, "image": {"url": "https://apollo.piczel.tv/static/x/one.png"}},
                {"id": 2, "image": {"url": "https://apollo.piczel.tv/static/x/two.jpg"}}]})
    }

    #[test]
    fn parses_multi_posts() {
        let items = parse_post(multi()).unwrap();
        assert_eq!(items.len(), 2);
        let k = &items[1].keywords;
        assert_eq!(k["id"], 7);
        assert_eq!(k["num"], 1);
        assert_eq!(k["count"], 2);
        assert_eq!(k["tags"], json!(["a"]));
        assert_eq!(k["date"], "2020-01-02 03:04:05");
        assert_eq!(k["filename"], "two");
        assert!(!k.contains_key("images"));
    }

    #[tokio::test]
    async fn paginates_folders() {
        let single = json!({"id": 9, "title": "One", "multi": false,
            "created_at": "2020-01-02T03:04:05Z", "tags": [], "user": {"username": "me"},
            "folder": {"id": 3, "name": "F"},
            "image": {"url": "https://apollo.piczel.tv/static/x/solo.gif"}});
        let (root, server) = serve(vec![
            json(json!({"data": [multi()], "meta": {"next_page": 2}})),
            json(json!({"data": [single], "meta": {"next_page": null}})),
        ]);
        let http = http("piczel", root);
        let files = media(extract(
            &http,
            Url::parse("https://piczel.tv/gallery/me/3").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 3);
        let k = keywords(&files[2]);
        assert_eq!(k["num"], 0);
        assert_eq!(k["count"], 1);
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "piczel_7_Two_00.png");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("piczel/me/F")
        );
        let (_, identity) = crate::sites::archive_key(&files[1]).unwrap();
        assert_eq!(identity, r#"["f{folder[id]}_{id}_{num}",["3","7","1"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /gallery/folder/3/page/1 "));
        assert!(requests[1].starts_with("GET /gallery/folder/3/page/2 "));
    }

    #[tokio::test]
    async fn requests_single_image() {
        let (root, server) = serve(vec![json(multi())]);
        let http = http("piczel", root);
        let files = media(extract(
            &http,
            Url::parse("https://piczel.tv/gallery/image/7").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let (file, _) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "piczel_7_Two_01.jpg");
        let (_, identity) = crate::sites::archive_key(&files[1]).unwrap();
        assert_eq!(identity, r#"["{id}_{num}",["7","1"]]"#);
        assert!(server.join().unwrap()[0].starts_with("GET /gallery/7 "));
    }

    #[tokio::test]
    async fn response_without_data_is_an_error() {
        let (root, server) = serve(vec![json(json!({"message": "nope"}))]);
        let http = http("piczel", root);
        let events = extract(&http, Url::parse("https://piczel.tv/gallery/me").unwrap());
        let results: Vec<_> = crate::event::media_only(events).collect().await;
        assert!(results.iter().any(Result::is_err));
        server.join().unwrap();
    }
}
