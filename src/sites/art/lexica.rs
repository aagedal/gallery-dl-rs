//! Lexica search results, mirroring gallery_dl/extractor/lexica.py.
use crate::error;
use crate::sites::gdl::{captures, group, re, truthy, unquote};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value, json};
use url::Url;

const ROOT: &str = "https://lexica.art";
const IMAGES: &str = "https://lexica-serve-encoded-images2.sharif.workers.dev/full_jpg/";

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

fn target(url: &Url) -> Option<String> {
    captures(re!(r"^https?://lexica\.art/?\?q=([^&#]+)"), url).map(|c| group(&c, 1).into())
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(query) = target(&url) else {
        return crate::sites::fail(error("unsupported Lexica URL"));
    };
    let text = unquote(&query).replace('+', " ");
    crate::sites::paginate(Some(Value::from(0)), move |state| {
        let query = query.clone();
        let text = text.clone();
        async move {
            let Some(cursor) = state else { return Ok(None) };
            let referer = format!("{ROOT}/?q={query}");
            let headers = [
                ("Accept", "application/json, text/plain, */*"),
                ("Referer", referer.as_str()),
            ];
            let body = json!({"text": text, "searchMode": "images", "source": "search", "cursor": cursor, "model": "lexica-aperture-v2"});
            let data = http
                .post_json(
                    &http.api(&format!("{ROOT}/api/infinite-prompts"))?,
                    &body,
                    &headers,
                )
                .await?;
            let prompts: Map<String, Value> = data["prompts"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|prompt| (crate::sites::scalar(&prompt["id"]), prompt.clone()))
                .collect();
            let items: Vec<Item> = data["images"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|image| {
                    let mut image = image.as_object()?.clone();
                    let prompt = image.remove("promptid").map(|id| {
                        prompts
                            .get(&crate::sites::scalar(&id))
                            .cloned()
                            .unwrap_or(Value::Null)
                    });
                    image.insert("prompt".into(), prompt.unwrap_or(Value::Null));
                    let id = image.get("id").map(crate::sites::scalar)?;
                    image.insert("filename".into(), id.as_str().into());
                    image.insert("extension".into(), "jpg".into());
                    image.insert("search_tags".into(), text.as_str().into());
                    Some(
                        Item::new(Url::parse(&format!("{IMAGES}{id}")).ok()?, image)
                            .ids(id.clone(), id),
                    )
                })
                .collect();
            let next = data.get("nextCursor").filter(|c| truthy(Some(c))).cloned();
            let events = items
                .into_iter()
                .flat_map(|item| crate::sites::post(&SITE, http, &SEARCH, vec![item]))
                .collect();
            Ok(Some((crate::sites::events(events), next)))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http as client, json as body, keywords, media, serve};

    #[tokio::test]
    async fn pages_by_cursor() {
        let (root, server) = serve(vec![
            body(
                json!({"prompts": [{"id": "p", "prompt": "cat"}], "images": [{"id": "a", "promptid": "p"}], "nextCursor": 50}),
            ),
            body(json!({"prompts": [], "images": [], "nextCursor": null})),
        ]);
        let http = client("lexica", root);
        let files = media(extract(
            &http,
            Url::parse("https://lexica.art/?q=space+cat").unwrap(),
        ))
        .await;
        assert_eq!(
            files[0].url.as_str(),
            "https://lexica-serve-encoded-images2.sharif.workers.dev/full_jpg/a"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["prompt"]["prompt"], "cat");
        assert_eq!(k["search_tags"], "space cat");
        let requests = server.join().unwrap();
        assert!(requests[1].contains("\"cursor\":50"));
    }
}
