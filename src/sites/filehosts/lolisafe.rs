//! lolisafe/chibisafe albums, mirroring gallery_dl/extractor/lolisafe.py.
//!
//! Upstream ships no built-in instances, so albums are addressed as
//! `lolisafe:https://host/a/ID`. The `domain` option (`auto` or a host)
//! selects the API root.
use crate::error;
use crate::sites::gdl::{ensure_http_scheme, nameext_into, re, root_of, rpartition, unescape};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

static ALBUM: Route = Route::new(
    "album",
    "{filename}.{extension}",
    &["{category}", "{album_name} ({album_id})"],
    "{album_id}_{id}",
);

pub static SITE: Site = Site {
    category: "lolisafe",
    name: "lolisafe",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&ALBUM],
    api_key: false,
    cookies: None,
};

/// Instance root and album ID.
fn target(url: &Url) -> Option<(String, String)> {
    let c = re!(r"^lolisafe:(https?://[^/?#]+)/a/([^/?#]+)").captures(url.as_str())?;
    Some((c[1].to_owned(), c[2].to_owned()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some((mut root, album_id)) = target(&url) else {
        return crate::sites::fail(error("unsupported lolisafe URL"));
    };
    match http.option_str("domain") {
        Some("auto") => {
            if let Ok(inner) = Url::parse(&root) {
                root = root_of(&inner);
            }
        }
        Some(domain) if !domain.is_empty() => root = ensure_http_scheme(domain),
        _ => {}
    }
    crate::sites::once(async move {
        let album = http
            .json(&http.api(&format!("{root}/api/album/get/{album_id}"))?)
            .await?;
        let mut data = Map::new();
        data.insert("album_id".into(), album_id.as_str().into());
        data.insert(
            "album_name".into(),
            unescape(crate::sites::gdl::str_of(&album, "title")).into(),
        );
        data.insert("count".into(), album["count"].clone());
        let files = album["files"].as_array().cloned().unwrap_or_default();
        let items = files
            .into_iter()
            .enumerate()
            .filter_map(|(index, file)| {
                let mut file = file.as_object().cloned()?;
                let raw = file.get("file").and_then(Value::as_str)?.to_owned();
                let url = Url::parse(&raw).ok()?;
                file.extend(data.clone());
                file.insert("num".into(), (index + 1).into());
                if !file.contains_key("extension") {
                    nameext_into(&raw, &mut file);
                }
                name_fields(&mut file);
                let id = file.get("id").map(crate::sites::scalar).unwrap_or_default();
                Some(Item::new(url, file).ids(album_id.clone(), id))
            })
            .collect();
        Ok(crate::sites::events(crate::sites::post(
            &SITE, http, &ALBUM, items,
        )))
    })
}

/// The `name`/`id`/`filename` normalisation of `LolisafeAlbumExtractor.items`.
fn name_fields(file: &mut Map<String, Value>) {
    let text = |file: &Map<String, Value>, key: &str| {
        file.get(key).map(crate::sites::scalar).unwrap_or_default()
    };
    let filename = text(file, "filename");
    if file.contains_key("name") {
        let name = text(file, "name");
        let stem = rpartition(&name, ".").0;
        let name = if stem.is_empty() {
            name.clone()
        } else {
            stem.to_owned()
        };
        let (_, sep, fid) = rpartition(&filename, "-");
        if sep.is_empty() || fid.chars().count() == 12 {
            file.entry("id").or_insert_with(|| "".into());
            file.insert("filename".into(), name.as_str().into());
        } else {
            file.insert("id".into(), fid.into());
            file.insert("filename".into(), format!("{name}-{fid}").into());
        }
        file.insert("name".into(), name.into());
    } else if file.contains_key("id") {
        let id = text(file, "id");
        file.insert("name".into(), filename.as_str().into());
        if filename != id {
            file.insert("filename".into(), format!("{filename}-{id}").into());
        }
    } else {
        let (name, _, id) = rpartition(&filename, "-");
        file.insert("name".into(), name.into());
        file.insert("id".into(), id.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http as client, json, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("lolisafe:https://xbunkr.com/a/ID"),
            Some(("https://xbunkr.com".into(), "ID".into()))
        );
        assert_eq!(t("https://xbunkr.com/a/ID"), None);
    }

    #[test]
    fn normalises_names() {
        let mut file = json!({"name": "photo.jpg", "filename": "photo-abcdefghijkl"})
            .as_object()
            .unwrap()
            .clone();
        name_fields(&mut file);
        assert_eq!(file["filename"], "photo");
        assert_eq!(file["id"], "");
        let mut file = json!({"name": "photo.jpg", "filename": "photo-XyZ12"})
            .as_object()
            .unwrap()
            .clone();
        name_fields(&mut file);
        assert_eq!(file["filename"], "photo-XyZ12");
        assert_eq!(file["id"], "XyZ12");
        let mut file = json!({"filename": "clip-Abc"}).as_object().unwrap().clone();
        name_fields(&mut file);
        assert_eq!(file["name"], "clip");
        assert_eq!(file["id"], "Abc");
    }

    #[tokio::test]
    async fn extracts_album() {
        let (root, server) = serve(vec![json(json!({"title": "My &amp; Album", "count": 1,
            "files": [{"file": "https://cdn.xbunkr.com/a/photo-XyZ12.jpg", "name": "photo.jpg"}]}))]);
        let http = client("lolisafe", root);
        let files = media(extract(
            &http,
            Url::parse("lolisafe:https://xbunkr.com/a/AbC").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let k = keywords(&files[0]);
        assert_eq!(k["album_name"], "My & Album");
        assert_eq!(k["id"], "XyZ12");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "photo-XyZ12.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("lolisafe/My & Album (AbC)")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/album/get/AbC "));
    }
}
