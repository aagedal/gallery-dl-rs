//! Lexica search results, mirroring gallery_dl/extractor/lexica.py: the
//! web client's `infinite-prompts` endpoint pages through images by cursor.
use super::path_segment;
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value, json};
use url::Url;

const ROOT: &str = "https://lexica.art";
const FILES: &str = "https://lexica-serve-encoded-images2.sharif.workers.dev/full_jpg/";
static SEARCH: Route = Route::new(
    "search",
    "{filename}.{extension}",
    &["{category}", "{search_tags}"],
    "{id}",
);

pub static SITE: Site = Site {
    category: "lexica",
    name: "Lexica",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&SEARCH],
    api_key: false,
    cookies: None,
};

/// `lexica\.art/?\?q=([^&#]+)`: the raw query value; upstream accepts no
/// `www.` and no other parameter first.
fn target(url: &Url) -> Option<String> {
    if !crate::sites::plain(url) || url.host_str() != Some("lexica.art") || url.path() != "/" {
        return None;
    }
    let value = url.query()?.strip_prefix("q=")?;
    let value = value.split('&').next().unwrap_or_default();
    (!value.is_empty()).then(|| value.into())
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(query) = target(&url) else {
        return crate::sites::fail(error("unsupported Lexica URL"));
    };
    // `text.unquote(query).replace("+", " ")`.
    let text = crate::sites::percent_decode(&query).replace('+', " ");
    crate::sites::paginate(Some(json!(0)), move |cursor| {
        let query = query.clone();
        let text = text.clone();
        async move {
            let Some(cursor) = cursor else {
                return Ok(None);
            };
            let body = json!({
                "text": text,
                "searchMode": "images",
                "source": "search",
                "cursor": cursor,
                "model": "lexica-aperture-v2",
            });
            let referer = format!("{ROOT}/?q={query}");
            let headers = [
                ("Accept", "application/json, text/plain, */*"),
                ("Referer", referer.as_str()),
            ];
            let data = http
                .post_json(
                    &http.api(&format!("{ROOT}/api/infinite-prompts"))?,
                    &body,
                    &headers,
                )
                .await
                .map_err(|e| error(format!("Lexica API request failed: {e}")))?;
            let next = data
                .get("nextCursor")
                .filter(|c| crate::sites::art::truthy(Some(c)))
                .cloned();
            Ok(Some((images(http, &data, &text)?, next)))
        }
    })
}

/// `posts()` and `items()` for one response: each image with its prompt.
fn images<'a>(http: &'a Http, data: &Value, tags: &str) -> Result<Events<'a>> {
    let mut prompts = Map::new();
    for prompt in data
        .get("prompts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(id) = prompt.get("id") {
            prompts.insert(crate::sites::scalar(id), prompt.clone());
        }
    }
    let Some(images) = data.get("images").and_then(Value::as_array) else {
        return Err(error("Lexica API response has no images"));
    };
    let mut events = Vec::new();
    for image in images {
        let Value::Object(mut image) = image.clone() else {
            continue;
        };
        let prompt = image
            .remove("promptid")
            .and_then(|id| prompts.get(&crate::sites::scalar(&id)).cloned())
            .unwrap_or(Value::Null);
        image.insert("prompt".into(), prompt);
        let id = image
            .get("id")
            .map(crate::sites::scalar)
            .unwrap_or_default();
        if !path_segment(&id) {
            continue;
        }
        image.insert("filename".into(), image["id"].clone());
        image.insert("extension".into(), "jpg".into());
        image.insert("search_tags".into(), tags.into());
        let Ok(url) = Url::parse(&format!("{FILES}{id}")) else {
            continue;
        };
        let caption = image
            .get("prompt")
            .and_then(|p| p.get("prompt"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let item = Item::new(url, image).ids(id.clone(), id).caption(caption);
        events.extend(crate::sites::post(&SITE, http, &SEARCH, vec![item]));
    }
    Ok(crate::sites::events(events))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http, json, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(t("https://lexica.art/?q=tree"), Some("tree".into()));
        assert_eq!(
            t("https://lexica.art?q=red+fox&x=1"),
            Some("red+fox".into())
        );
        for raw in [
            "https://lexica.art/?q=",
            "https://lexica.art/?x=1&q=tree",
            "https://lexica.art/search?q=tree",
            "https://www.lexica.art/?q=tree",
            "https://lexica.art.evil.test/?q=tree",
            "https://lexica.art:8443/?q=tree",
            "https://u:p@lexica.art/?q=tree",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[tokio::test]
    async fn search_pages_by_cursor() {
        let (root, server) = serve(vec![
            json(serde_json::json!({
                "prompts": [{"id": "p1", "prompt": "a red fox"}],
                "images": [{"id": "img-1", "promptid": "p1", "width": 512}],
                "nextCursor": 50})),
            json(
                serde_json::json!({"prompts": [], "images": [{"id": "img-2", "promptid": "x"}],
                "nextCursor": null}),
            ),
        ]);
        let http = http("lexica", root);
        let files = media(extract(
            &http,
            Url::parse("https://lexica.art/?q=red%2Bfox+art").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].url.as_str(),
            "https://lexica-serve-encoded-images2.sharif.workers.dev/full_jpg/img-1"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["prompt"]["prompt"], "a red fox");
        assert!(!k.contains_key("promptid"));
        assert_eq!(k["search_tags"], "red fox art");
        assert_eq!(keywords(&files[1])["prompt"], Value::Null);
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "img-1.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("lexica/red fox art")
        );
        let (_, identity) = crate::sites::archive_key(&files[1]).unwrap();
        assert_eq!(identity, r#"["{id}",["img-2"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("POST /api/infinite-prompts "));
        assert!(requests[0].contains("referer: https://lexica.art/?q=red%2Bfox+art"));
        assert!(requests[0].contains(r#""cursor":0"#));
        assert!(requests[0].contains(r#""text":"red fox art""#));
        assert!(requests[1].contains(r#""cursor":50"#));
    }
}
