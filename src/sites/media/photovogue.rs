//! PhotoVogue photographers, mirroring gallery_dl/extractor/photovogue.py:
//! the `api.vogue.com` photo listing pages through a photographer's work.
use super::{iso_ts, str_of, web_url};
use crate::sites::art::{digits, nameext_into, on_host};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::Value;
use url::Url;

const API: &str = "https://api.vogue.com/production/photos";
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

/// `vogue\.com/photovogue/photographers/(\d+)`.
fn target(url: &Url) -> Option<String> {
    if !on_host(url, &["vogue.com"]) {
        return None;
    }
    let rest = url.path().strip_prefix("/photovogue/photographers/")?;
    let id = crate::sites::art::leading_digits(rest)?;
    digits(id).then(|| id.to_owned())
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(user) = target(&url) else {
        return crate::sites::fail(error("unsupported PhotoVogue URL"));
    };
    crate::sites::paginate(Some(0u64), move |page| {
        let user = user.clone();
        async move {
            let Some(page) = page else {
                return Ok(None);
            };
            let mut url = http.api(API)?;
            url.query_pairs_mut()
                .append_pair("count", "50")
                .append_pair("order_by", "DESC")
                .append_pair("page", &page.to_string())
                .append_pair("photographer_id", &user);
            let data = http.json(&url).await?;
            let next = crate::sites::art::truthy(data.get("has_next")).then_some(page + 1);
            Ok(Some((photos(http, &data)?, next)))
        }
    })
}

/// `items()` for one API page: one post per photo.
fn photos<'a>(http: &'a Http, data: &Value) -> Result<Events<'a>> {
    let Some(items) = data.get("items").and_then(Value::as_array) else {
        return Err(error("PhotoVogue API response has no items"));
    };
    let mut events = Vec::new();
    for photo in items {
        if let Some(item) = parse_photo(photo) {
            events.extend(crate::sites::post(&SITE, http, &USER, vec![item]));
        }
    }
    Ok(crate::sites::events(events))
}

pub(crate) fn parse_photo(photo: &Value) -> Option<Item> {
    let mut photo = photo.as_object()?.clone();
    let raw = str_of(&Value::Object(photo.clone()), "gallery_image").to_owned();
    let url = web_url(&raw)?;
    let title = photo
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_owned();
    photo.insert("title".into(), title.clone().into());
    let ts = iso_ts(str_of(&Value::Object(photo.clone()), "date"));
    photo.insert("date".into(), crate::filter::source_date_json(ts));
    nameext_into(&raw, &mut photo);
    let id = photo
        .get("id")
        .map(crate::sites::scalar)
        .unwrap_or_default();
    let user = photo
        .get("photographer")
        .and_then(|p| p.get("name"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    Some(
        Item::new(url, photo)
            .ids(id.clone(), id)
            .user(user)
            .caption(Some(title))
            .timestamp(ts),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http, json, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.vogue.com/photovogue/photographers/221252"),
            Some("221252".into())
        );
        assert!(t("https://vogue.com/photovogue/photographers/12/x").is_some());
        for raw in [
            "https://www.vogue.com/photovogue/photographers/",
            "https://www.vogue.com/photovogue/photographers/abc",
            "https://evilvogue.com/photovogue/photographers/1",
            "https://vogue.com.evil/photovogue/photographers/1",
            "https://www.vogue.com:8443/photovogue/photographers/1",
            "https://u@www.vogue.com/photovogue/photographers/1",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[tokio::test]
    async fn pages_through_photos() {
        let photo = |id: u64| {
            json!({"id": id, "title": " Title \n", "date": "2020-01-02T03:04:05.000Z",
                "gallery_image": format!("https://img.vogue.com/{id}/photo.jpg"),
                "photographer": {"id": 7, "name": "Ann"}})
        };
        let (root, server) = serve(vec![
            json(json!({"items": [photo(1)], "has_next": true})),
            json(json!({"items": [photo(2)], "has_next": false})),
        ]);
        let http = http("photovogue", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.vogue.com/photovogue/photographers/7").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[1]);
        assert_eq!(k["title"], "Title");
        assert_eq!(k["filename"], "photo");
        assert_eq!(files[1].url.as_str(), "https://img.vogue.com/2/photo.jpg");
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "2 Title.jpg");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("photovogue/7 Ann")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{id}",["1"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with(
            "GET /production/photos?count=50&order_by=DESC&page=0&photographer_id=7 "
        ));
        assert!(requests[1].contains("&page=1&"));
    }
}
