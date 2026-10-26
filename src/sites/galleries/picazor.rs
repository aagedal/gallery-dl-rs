//! Picazor users, mirroring gallery_dl/extractor/picazor.py.
use crate::error;
use crate::sites::gdl::{captures, group, nameext_into, re};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::Value;
use url::Url;

const ROOT: &str = "https://picazor.com";

static USER: Route = Route::new(
    "user",
    "{id}_{num:>03}.{extension}",
    &["{category}", "{user}"],
    "{id}_{num}",
);

pub static SITE: Site = Site {
    category: "picazor",
    name: "Picazor",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&USER],
    api_key: false,
    cookies: None,
};

fn target(url: &Url) -> Option<String> {
    captures(
        re!(r"^https?://(?:www\.)?picazor\.com/[a-z]{2}/([^/?#]+)"),
        url,
    )
    .map(|c| group(&c, 1).into())
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(user) = target(&url) else {
        return crate::sites::fail(error("unsupported Picazor URL"));
    };
    // State: page number and the `count` taken from the first file.
    crate::sites::paginate(Some((1u64, None::<Value>)), move |state| {
        let user = user.clone();
        async move {
            let Some((page, count)) = state else {
                return Ok(None);
            };
            let mut request = http.api(&format!("{ROOT}/api/files/{user}/sfiles"))?;
            request
                .query_pairs_mut()
                .append_pair("page", &page.to_string());
            let referer = format!("{ROOT}/en/{user}");
            let data = http.json_with(&request, &[("Referer", &referer)]).await?;
            let files = data.as_array().cloned().unwrap_or_default();
            if files.is_empty() {
                return Ok(None);
            }
            let mut count = count;
            let items: Vec<Item> = files
                .into_iter()
                .filter_map(|item| {
                    let mut item = item.as_object()?.clone();
                    let path = item
                        .get("path")
                        .and_then(Value::as_str)
                        .filter(|p| !p.is_empty())?
                        .to_owned();
                    let count = count
                        .get_or_insert_with(|| item.get("order").cloned().unwrap_or(Value::Null))
                        .clone();
                    item.remove("blurDataURL");
                    item.insert(
                        "num".into(),
                        item.get("order").cloned().unwrap_or(Value::Null),
                    );
                    item.insert("user".into(), user.as_str().into());
                    item.insert("count".into(), count);
                    let file = format!("{ROOT}{path}");
                    nameext_into(&file, &mut item);
                    let id = item.get("id").map(crate::sites::scalar).unwrap_or_default();
                    let num = crate::sites::scalar(&item["num"]);
                    Some(Item::new(Url::parse(&file).ok()?, item).ids(id, num))
                })
                .collect();
            let events = items
                .into_iter()
                .flat_map(|item| crate::sites::post(&SITE, http, &USER, vec![item]))
                .collect();
            Ok(Some((
                crate::sites::events(events),
                Some((page + 1, count)),
            )))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http as client, json, keywords, media, serve};
    use serde_json::json;

    #[tokio::test]
    async fn pages_until_empty() {
        let (root, server) = serve(vec![
            json(
                json!([{"id": 9, "order": 12, "path": "/uploads/a.jpg", "blurDataURL": "x"}, {"id": 9, "order": 11, "path": null}]),
            ),
            json(json!([{"id": 9, "order": 10, "path": "/uploads/b.jpg"}])),
            json(json!([])),
        ]);
        let http = client("picazor", root);
        let files = media(extract(
            &http,
            Url::parse("https://picazor.com/en/jane").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[1]);
        assert_eq!(k["count"], 12);
        assert!(!k.contains_key("blurDataURL"));
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "9_010.jpg");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("picazor/jane")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/files/jane/sfiles?page=1 "));
    }
}
