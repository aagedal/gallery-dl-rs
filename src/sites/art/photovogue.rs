//! PhotoVogue photographers, mirroring gallery_dl/extractor/photovogue.py.
use crate::error;
use crate::sites::gdl::{captures, date, group, nameext_into, parse_iso, re, str_of, truthy};
use crate::sites::{Events, Http, Item, Route, Site};
use url::Url;

static USER: Route = Route::new(
    "user",
    "{id} {title}.{extension}",
    &["{category}", "{photographer[id]} {photographer[name]}"],
    "{id}",
);

pub static SITE: Site = Site {
    category: "photovogue",
    name: "PhotoVogue",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&USER],
    api_key: false,
    cookies: None,
};

fn target(url: &Url) -> Option<String> {
    captures(
        re!(r"^https?://(?:www\.)?vogue\.com/photovogue/photographers/(\d+)"),
        url,
    )
    .map(|c| group(&c, 1).into())
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(user) = target(&url) else {
        return crate::sites::fail(error("unsupported PhotoVogue URL"));
    };
    crate::sites::paginate(Some(0u64), move |state| {
        let user = user.clone();
        async move {
            let Some(page) = state else { return Ok(None) };
            let mut request = http.api("https://api.vogue.com/production/photos")?;
            request
                .query_pairs_mut()
                .append_pair("count", "50")
                .append_pair("order_by", "DESC")
                .append_pair("page", &page.to_string())
                .append_pair("photographer_id", &user);
            let data = http.json(&request).await?;
            let items: Vec<Item> = data["items"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|photo| {
                    let mut fields = photo.as_object()?.clone();
                    let file = str_of(photo, "gallery_image").to_owned();
                    fields.insert("title".into(), str_of(photo, "title").trim().into());
                    fields.insert("date".into(), date(parse_iso(str_of(photo, "date"))));
                    nameext_into(&file, &mut fields);
                    let id = fields
                        .get("id")
                        .map(crate::sites::scalar)
                        .unwrap_or_default();
                    Some(Item::new(Url::parse(&file).ok()?, fields).ids(id.clone(), id))
                })
                .collect();
            let next = truthy(data.get("has_next")).then_some(page + 1);
            let events = items
                .into_iter()
                .flat_map(|item| crate::sites::post(&SITE, http, &USER, vec![item]))
                .collect();
            Ok(Some((crate::sites::events(events), next)))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http as client, json, media, serve};
    use serde_json::json;

    #[tokio::test]
    async fn pages_photos() {
        let photo = |id: i64| {
            json!({"id": id, "title": " Light ", "date": "2022-01-02T03:04:05Z", "gallery_image": format!("https://img.vogue.com/{id}.jpg"),
            "photographer": {"id": 221252, "name": "Jane"}})
        };
        let (root, server) = serve(vec![
            json(json!({"items": [photo(1)], "has_next": true})),
            json(json!({"items": [photo(2)], "has_next": false})),
        ]);
        let http = client("photovogue", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.vogue.com/photovogue/photographers/221252").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "2 Light.jpg");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("photovogue/221252 Jane")
        );
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with(
            "GET /production/photos?count=50&order_by=DESC&page=1&photographer_id=221252 "
        ));
    }
}
