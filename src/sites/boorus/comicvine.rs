//! Comic Vine image galleries, mirroring gallery_dl/extractor/comicvine.py
//! (a `BooruExtractor` over `/js/image-data.json`).
use crate::error;
use crate::sites::gdl::{captures, date, extr, group, re, strptime, unquote};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://comicvine.gamespot.com";
const PER_PAGE: usize = 1000;

static TAG: Route = Route::new(
    "tag",
    "{filename}.{extension}",
    &["{category}", "{tag}"],
    "{id}",
);

pub static SITE: Site = Site {
    category: "comicvine",
    name: "Comic Vine",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&TAG],
    api_key: false,
    cookies: None,
};

/// Path, object name and object ID.
fn target(url: &Url) -> Option<(String, String, String)> {
    let pattern = re!(r"^https?://comicvine\.gamespot\.com(/([^/?#]+)/(\d+-\d+)/images/.*)");
    let c = captures(pattern, url)?;
    Some((
        group(&c, 1).into(),
        group(&c, 2).into(),
        group(&c, 3).into(),
    ))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some((path, name, object)) = target(&url) else {
        return crate::sites::fail(error("unsupported Comic Vine URL"));
    };
    crate::sites::once(async move {
        let page = http.text(&http.api(&format!("{ROOT}{path}"))?).await?;
        let gallery = extr(&page, "data-gallery-id=\"", "\"").to_owned();
        let mut data = Map::new();
        data.insert("tag".into(), unquote(&name).into());
        Ok(crate::sites::paginate(Some(0usize), move |state| {
            let gallery = gallery.clone();
            let object = object.clone();
            let data = data.clone();
            async move {
                let Some(start) = state else { return Ok(None) };
                let mut url = http.api(&format!("{ROOT}/js/image-data.json"))?;
                url.query_pairs_mut()
                    .append_pair("images", &gallery)
                    .append_pair("start", &start.to_string())
                    .append_pair("count", &PER_PAGE.to_string())
                    .append_pair("object", &object);
                let images = http.json(&url).await?["images"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                let next = (images.len() >= PER_PAGE).then_some(start + PER_PAGE);
                let key = http.option_str("url").unwrap_or("original").to_owned();
                let events = images
                    .into_iter()
                    .filter_map(|post| post_item(post, &key, &data))
                    .flat_map(|item| crate::sites::post(&SITE, http, &TAG, vec![item]))
                    .collect();
                Ok(Some((crate::sites::events(events), next)))
            }
        }))
    })
}

/// `BooruExtractor.items` for one image: URL, name, metadata, `_prepare`.
fn post_item(post: Value, key: &str, data: &Map<String, Value>) -> Option<Item> {
    let mut post = post.as_object().cloned()?;
    let raw = post.get(key).and_then(Value::as_str)?.to_owned();
    let raw = if raw.starts_with('/') {
        format!("{ROOT}{raw}")
    } else {
        raw
    };
    let url = Url::parse(&raw).ok()?;
    crate::sites::boorus::booru_fields(&url, &mut post, data);
    let created = post
        .get("dateCreated")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    post.insert("date".into(), date(strptime(&created, "%a, %b %d %Y")));
    let tags: Vec<Value> = post
        .get("tags")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|tag| tag["name"].clone())
        .filter(|name| crate::sites::gdl::truthy(Some(name)))
        .collect();
    post.insert("tags".into(), tags.into());
    let id = post.get("id").map(crate::sites::scalar).unwrap_or_default();
    Some(Item::new(url, post).ids(id.clone(), id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, json, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t(
                "https://comicvine.gamespot.com/batman/4005-1699/images/?tag=Fan%20Art%20%26%20Cosplay"
            ),
            Some((
                "/batman/4005-1699/images/?tag=Fan%20Art%20%26%20Cosplay".into(),
                "batman".into(),
                "4005-1699".into()
            ))
        );
        assert_eq!(t("https://comicvine.gamespot.com/batman/4005-1699/"), None);
    }

    #[tokio::test]
    async fn pages_through_image_data() {
        let images: Vec<Value> = (0..PER_PAGE)
            .map(|i| json!({"id": i, "original": format!("https://comicvine.gamespot.com/a/uploads/original/1/{i}.jpg"),
                "dateCreated": "Mon, Jan 04 2021", "tags": [{"name": "Fan Art"}, {"name": ""}]}))
            .collect();
        let (root, server) = serve(vec![
            html("<div data-gallery-id=\"77\"></div>"),
            json(json!({"images": images})),
            json(
                json!({"images": [{"id": 9999, "original": "/a/uploads/original/1/last.png", "dateCreated": "", "tags": []}]}),
            ),
        ]);
        let http = http("comicvine", root);
        let files = media(extract(
            &http,
            Url::parse("https://comicvine.gamespot.com/jock/4040-5653/images/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), PER_PAGE + 1);
        let k = keywords(&files[0]);
        assert_eq!(k["tag"], "jock");
        assert_eq!(k["date"], "2021-01-04 00:00:00");
        assert_eq!(k["tags"], json!(["Fan Art"]));
        assert_eq!(
            files[PER_PAGE].url.as_str(),
            "https://comicvine.gamespot.com/a/uploads/original/1/last.png"
        );
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("comicvine/jock")
        );
        assert_eq!(file.render(&files[0]).unwrap(), "0.jpg");
        let requests = server.join().unwrap();
        assert!(
            requests[1].starts_with(
                "GET /js/image-data.json?images=77&start=0&count=1000&object=4040-5653 "
            )
        );
        assert!(requests[2].contains("start=1000"));
    }
}
