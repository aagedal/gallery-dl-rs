//! 8muses comic albums, mirroring gallery_dl/extractor/8muses.py: the page
//! embeds an obfuscated JSON blob with pictures and sub-albums.
use crate::error;
use crate::sites::gdl::{captures, date, extr, group, int_of, parse_iso, re, unescape};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://comics.8muses.com";

static ALBUM: Route = Route::new(
    "album",
    "{page:>03}.{extension}",
    &["{category}", "{album[path]:I}"],
    "{hash}",
);

pub static SITE: Site = Site {
    category: "8muses",
    name: "8muses",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&ALBUM],
    api_key: false,
    cookies: None,
};

fn target(url: &Url) -> Option<(String, String)> {
    let c = captures(
        re!(r"^https?://(?:comics\.|www\.)?8muses\.com(/comics/album/[^?#]+)(\?[^#]+)?"),
        url,
    )?;
    Some((group(&c, 1).into(), group(&c, 2).into()))
}

/// `_unobfuscate`: ROT47-like shift of printable ASCII, then JSON.
fn unobfuscate(data: &str) -> crate::Result<Value> {
    let decoded: String = unescape(data.trim_matches(['\t', '\n', '\r', ' ', '!']))
        .chars()
        .map(|c| {
            if ('!'..='~').contains(&c) {
                char::from(33 + ((c as u8 + 14) % 94))
            } else {
                c
            }
        })
        .collect();
    serde_json::from_str(&decoded).map_err(|_| error("8muses page has invalid album data"))
}

fn album_info(album: &Value) -> Value {
    let path: Vec<Value> = crate::sites::scalar(&album["path"])
        .split('/')
        .map(Value::from)
        .collect();
    serde_json::json!({
        "id": album["id"], "path": path, "title": album["name"], "private": album["isPrivate"],
        "url": format!("{ROOT}/comics/album/{}", crate::sites::scalar(&album["permalink"])),
        "parent": int_of(album.get("parentId")), "views": int_of(album.get("numberViews")),
        "likes": int_of(album.get("numberLikes")),
        "date": date(parse_iso(crate::sites::gdl::str_of(album, "updatedAt"))),
    })
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some((path, params)) = target(&url) else {
        return crate::sites::fail(error("unsupported 8muses URL"));
    };
    crate::sites::paginate(Some(format!("{ROOT}{path}{params}")), move |state| {
        let path = path.clone();
        let params = params.clone();
        async move {
            let Some(url) = state else { return Ok(None) };
            let page = http.text(&http.api(&url)?).await?;
            let data = unobfuscate(extr(
                &page,
                "id=\"ractive-public\" type=\"text/plain\">",
                "</script>",
            ))?;
            let mut events = Vec::new();
            if let Some(images) = data["pictures"].as_array().filter(|i| !i.is_empty()) {
                let album = album_info(&data["album"]);
                let items = images
                    .iter()
                    .enumerate()
                    .filter_map(|(index, image)| {
                        let hash = crate::sites::scalar(&image["publicUri"]);
                        let url = format!("{ROOT}/image/fl/{hash}");
                        let mut fields = Map::new();
                        fields.insert("url".into(), url.as_str().into());
                        fields.insert("page".into(), (index + 1).into());
                        fields.insert("hash".into(), hash.as_str().into());
                        fields.insert("count".into(), images.len().into());
                        fields.insert("album".into(), album.clone());
                        fields.insert("filename".into(), hash.as_str().into());
                        fields.insert("extension".into(), "jpg".into());
                        Some(
                            Item::new(Url::parse(&url).ok()?, fields)
                                .ids(crate::sites::scalar(&album["id"]), hash),
                        )
                    })
                    .collect();
                events.push(crate::sites::events(crate::sites::post(
                    &SITE, http, &ALBUM, items,
                )));
            }
            let children: Vec<(Url, Map<String, Value>)> = data["albums"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|album| {
                    let permalink = album
                        .get("permalink")
                        .and_then(Value::as_str)
                        .filter(|p| !p.is_empty())?;
                    let url = format!("{ROOT}/comics/album/{permalink}");
                    let mut fields = Map::new();
                    fields.insert("url".into(), url.as_str().into());
                    fields.insert("name".into(), album["name"].clone());
                    fields.insert("private".into(), album["isPrivate"].clone());
                    Some((Url::parse(&url).ok()?, fields))
                })
                .collect();
            events.push(super::queue(http, &SITE, &ALBUM, children));
            let next = (int_of(data.get("page")) < int_of(data.get("pages"))).then(|| {
                let trimmed = path.trim_end_matches('/');
                let (head, _, num) = crate::sites::gdl::rpartition(trimmed, "/");
                let base = if crate::sites::gdl::digits(num) {
                    head
                } else {
                    path.as_str()
                };
                format!("{ROOT}{base}/{}{params}", int_of(data.get("page")) + 1)
            });
            use futures_util::StreamExt;
            Ok(Some((
                futures_util::stream::iter(events).flatten().boxed(),
                next,
            )))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http as client, keywords, serve};

    /// The inverse of `unobfuscate`'s character shift.
    fn obfuscate(text: &str) -> String {
        text.chars()
            .map(|c| {
                if ('!'..='~').contains(&c) {
                    char::from(33 + ((c as u8 - 33 + 47) % 94))
                } else {
                    c
                }
            })
            .collect()
    }

    #[test]
    fn classifies_and_unobfuscates() {
        assert_eq!(
            target(
                &Url::parse("https://comics.8muses.com/comics/album/Various-Authors/JankinGen")
                    .unwrap()
            ),
            Some((
                "/comics/album/Various-Authors/JankinGen".into(),
                String::new()
            ))
        );
        let value = unobfuscate(&format!("!{}", obfuscate(r#"{"a":1}"#))).unwrap();
        assert_eq!(value, serde_json::json!({"a": 1}));
    }

    #[tokio::test]
    async fn extracts_album_pictures() {
        let json = serde_json::json!({"page": 1, "pages": 1, "album": {"id": 7, "path": "Tufos-Comics/Gallery", "name": "Gallery", "isPrivate": false,
            "permalink": "Tufos-Comics/Gallery", "parentId": "3", "numberViews": 10, "numberLikes": 2, "updatedAt": "2020-01-01T00:00:00Z"},
            "pictures": [{"publicUri": "AbC"}], "albums": [{"permalink": "Tufos-Comics/Gallery/Sub", "name": "Sub", "isPrivate": false}]});
        let page = format!(
            r#"<script id="ractive-public" type="text/plain">!{}</script>"#,
            obfuscate(&json.to_string())
        );
        let (root, server) = serve(vec![html(&page)]);
        let http = client("8muses", root);
        let events: Vec<_> = futures_util::StreamExt::collect(extract(
            &http,
            Url::parse("https://comics.8muses.com/comics/album/Tufos-Comics/Gallery").unwrap(),
        ))
        .await;
        let files: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Ok(crate::event::ExtractionEvent::Media(m)) => Some(m),
                _ => None,
            })
            .collect();
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].url.as_str(),
            "https://comics.8muses.com/image/fl/AbC"
        );
        assert_eq!(
            keywords(files[0])["album"]["path"],
            serde_json::json!(["Tufos-Comics", "Gallery"])
        );
        let (file, dir) = crate::sites::default_templates(files[0]).unwrap();
        assert_eq!(file.render(files[0]).unwrap(), "001.jpg");
        assert_eq!(
            dir.render(files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("8muses/Tufos-Comics/Gallery")
        );
        server.join().unwrap();
    }
}
