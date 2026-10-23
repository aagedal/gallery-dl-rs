//! Picarto channel galleries, mirroring gallery_dl/extractor/picarto.py
//! through the `ptvintern.picarto.tv` gallery API.
use super::{parse_iso, raw_segments, str_of};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

static GALLERY: Route = Route::new(
    "gallery",
    "{id} {title}.{extension}",
    &["{category}", "{channel[name]}"],
    "{id}",
);

pub static SITE: Site = Site {
    category: "picarto",
    name: "Picarto",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&GALLERY],
    api_key: false,
};

/// `picarto\.tv/([^/?#]+)/gallery`, unanchored after `gallery`.
fn target(url: &Url) -> Option<String> {
    if !crate::sites::plain(url) || url.host_str() != Some("picarto.tv") {
        return None;
    }
    match raw_segments(url).as_slice() {
        [user, gallery, ..] if gallery.starts_with("gallery") => Some((*user).to_owned()),
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(user) = target(&url) else {
        return crate::sites::fail(error("unsupported Picarto URL"));
    };
    let user = crate::sites::percent_decode(&user);
    crate::sites::paginate(Some(1u64), move |page| {
        let user = user.clone();
        async move {
            let Some(page) = page else {
                return Ok(None);
            };
            let mut url = http.api("https://ptvintern.picarto.tv/api/channel-gallery")?;
            url.query_pairs_mut()
                .append_pair("first", "30")
                .append_pair("page", &page.to_string())
                .append_pair("filter_params[album_id]", "")
                .append_pair("filter_params[channel_name]", &user)
                .append_pair("filter_params[q]", "")
                .append_pair("filter_params[visibility]", "")
                .append_pair("order_by[field]", "published_at")
                .append_pair("order_by[order]", "DESC");
            let posts = http.json(&url).await?;
            let Some(posts) = posts.as_array().filter(|posts| !posts.is_empty()) else {
                return Ok(None);
            };
            let mut events = Vec::new();
            for post in posts {
                let items = parse_post(post.clone())?;
                events.extend(crate::sites::post(&SITE, http, &GALLERY, items));
            }
            let next = (page < 10_000).then_some(page + 1);
            Ok(Some((crate::sites::events(events), next)))
        }
    })
}

fn image_url(post: &Map<String, Value>) -> Option<Url> {
    let name = str_of(post.get("default_image")?, "name");
    if name.is_empty() {
        return None;
    }
    super::file_url(
        &format!("https://images.picarto.tv/gallery/{name}"),
        "https://picarto.tv",
    )
}

/// The default image and every variation of one gallery post.
pub(crate) fn parse_post(post: Value) -> Result<Vec<Item>> {
    let Value::Object(mut post) = post else {
        return Err(error("Picarto post is not an object"));
    };
    let created = parse_iso(
        post.get("created_at")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    post.insert("date".into(), super::date(created));
    let variations = match post.remove("variations") {
        Some(Value::Array(variations)) => variations,
        _ => Vec::new(),
    };
    if !super::truthy(post.get("default_image")) {
        return Ok(Vec::new());
    }
    let mut items = Vec::new();
    let mut push = |post: &Map<String, Value>| {
        let Some(url) = image_url(post) else {
            return;
        };
        let mut fields = post.clone();
        super::nameext_into(url.as_str(), &mut fields);
        let id = fields
            .get("id")
            .map(crate::sites::scalar)
            .unwrap_or_default();
        let media = fields
            .get("default_image")
            .and_then(|image| image.get("id"))
            .map_or_else(|| url.path().to_owned(), crate::sites::scalar);
        let user = fields
            .get("channel")
            .map(|channel| str_of(channel, "name").to_owned());
        let title = fields
            .get("title")
            .and_then(Value::as_str)
            .map(str::to_owned);
        items.push(
            Item::new(url, fields)
                .ids(id, media)
                .user(user)
                .caption(title)
                .timestamp(created),
        );
    };
    push(&post);
    for variation in variations {
        if let Value::Object(variation) = variation {
            super::update(&mut post, &variation);
        }
        push(&post);
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
            t("https://picarto.tv/fnook/gallery/default/"),
            Some("fnook".into())
        );
        assert_eq!(t("https://picarto.tv/fnook/gallery"), Some("fnook".into()));
        for raw in [
            "https://picarto.tv/fnook",
            "https://picarto.tv/fnook/videos",
            "https://www.picarto.tv/fnook/gallery",
            "https://picarto.tv.evil.com/fnook/gallery",
            "https://evilpicarto.tv/fnook/gallery",
            "https://picarto.tv:8080/fnook/gallery",
            "https://u:p@picarto.tv/fnook/gallery",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn post() -> Value {
        json!({"id": 5, "title": "Pic", "created_at": "2021-05-06T07:08:09.000000Z",
            "channel": {"name": "fnook"},
            "default_image": {"id": 11, "name": "a/main.png"},
            "variations": [{"default_image": {"id": 12, "name": "a/var.jpg"}}]})
    }

    #[tokio::test]
    async fn paginates_until_empty() {
        let (root, server) = serve(vec![
            json(json!([post(), {"id": 6, "default_image": null}])),
            json(json!([])),
        ]);
        let http = http("picarto", root);
        let files = media(extract(
            &http,
            Url::parse("https://picarto.tv/fnook/gallery/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[1].url.as_str(),
            "https://images.picarto.tv/gallery/a/var.jpg"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["date"], "2021-05-06 07:08:09");
        assert_eq!(k["filename"], "main");
        assert!(!k.contains_key("variations"));
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "5 Pic.jpg");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("picarto/fnook")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{id}",["5"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with(
            "GET /api/channel-gallery?first=30&page=1&filter_params%5Balbum_id%5D=&filter_params%5Bchannel_name%5D=fnook&"
        ), "{}", requests[0]);
        assert!(requests[1].starts_with("GET /api/channel-gallery?first=30&page=2&"));
    }

    #[tokio::test]
    async fn api_errors_end_the_stream() {
        let (root, server) = serve(vec![crate::sites::testing::response(
            "404 Not Found",
            "application/json",
            b"{}",
        )]);
        let http = http("picarto", root);
        let events = extract(
            &http,
            Url::parse("https://picarto.tv/nobody/gallery").unwrap(),
        );
        let results: Vec<_> = crate::event::media_only(events).collect().await;
        assert!(results.iter().any(Result::is_err));
        server.join().unwrap();
    }
}
