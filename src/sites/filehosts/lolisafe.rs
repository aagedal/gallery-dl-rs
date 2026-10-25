//! lolisafe/chibisafe albums on configured instances, mirroring
//! `LolisafeAlbumExtractor` in gallery_dl/extractor/lolisafe.py through the
//! `/api/album/get/{id}` endpoint. Upstream has no built-in instances; its
//! subclasses (bunkr, cyberdrop, turbo) are dedicated registry sites that
//! share the `lolisafe` settings scope. The `domain` option is not
//! supported.
use super::{nameext_from_url, rpartition};
use crate::sites::instances::{Family, configured, plain_or_root};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

static ALBUM: Route = Route::new(
    "album",
    "{filename}.{extension}",
    &["{category}", "{album_name} ({album_id})"],
    "{album_id}_{id}",
);

/// Routes and flags for configured instances; not part of the registry.
static TEMPLATE: Site = Site {
    category: "lolisafe",
    name: "lolisafe",
    matches: |_| false,
    extract: |_, _| crate::sites::fail(error("lolisafe needs a configured instance")),
    routes: &[&ALBUM],
    api_key: false,
    cookies: None,
};

/// Configured `extractor.lolisafe.<category>` instances.
pub(crate) static FAMILY: Family = Family {
    basecategory: "lolisafe",
    builtin: &[],
    related: &["bunkr", "cyberdrop", "turbo"],
    template: &TEMPLATE,
    matches: |category, url| target(category, url).is_some(),
    extract,
};

/// `BASE_PATTERN + "/a/([^/?#]+)"`: the album ID.
fn target(category: &str, url: &Url) -> Option<String> {
    let instance = configured(&FAMILY, category)?;
    let rest = crate::sites::instances::below(url, instance.spec)?;
    let tail = rest.strip_prefix("/a/")?;
    let end = tail.find(['/', '?', '#']).unwrap_or(tail.len());
    (end > 0).then(|| tail[..end].to_owned())
}

fn extract<'a>(category: &'static str, http: &'a Http, url: Url) -> Events<'a> {
    let (Some(instance), Some(album_id)) = (configured(&FAMILY, category), target(category, &url))
    else {
        return crate::sites::fail(error("unsupported lolisafe URL"));
    };
    crate::sites::once(async move {
        let api = http.api(&format!("{}/api/album/get/{album_id}", instance.root))?;
        let body = http.json(&api).await?;
        let items = parse_album(instance.root, &album_id, &body)?;
        Ok(crate::sites::events(crate::sites::post(
            &instance.site,
            http,
            &ALBUM,
            items,
        )))
    })
}

fn parse_album(root: &str, album_id: &str, body: &Value) -> Result<Vec<Item>> {
    let files = body
        .get("files")
        .and_then(Value::as_array)
        .ok_or_else(|| error("lolisafe album response lacks files"))?;
    let mut data = Map::new();
    data.insert("album_id".into(), album_id.into());
    data.insert(
        "album_name".into(),
        crate::sites::unescape(
            body.get("title")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        )
        .into(),
    );
    data.insert(
        "count".into(),
        body.get("count").cloned().unwrap_or(Value::Null),
    );
    let mut items = Vec::new();
    for (index, file) in files.iter().enumerate() {
        let Some(mut file) = file.as_object().cloned() else {
            continue;
        };
        let raw = file
            .get("file")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let Some(url) = Url::parse(raw.trim())
            .ok()
            .filter(|url| plain_or_root(url, root))
        else {
            continue;
        };
        file.extend(data.clone());
        file.insert("num".into(), (index + 1).into());
        if !file.contains_key("extension") {
            nameext_from_url(&raw, &mut file);
        }
        finish(&mut file);
        let id = file.get("id").map(crate::sites::scalar).unwrap_or_default();
        let name = file.get("name").and_then(Value::as_str).map(str::to_owned);
        items.push(Item::new(url, file).ids(album_id, id).caption(name));
    }
    Ok(items)
}

/// Upstream's `name`/`id`/`filename` normalization.
fn finish(file: &mut Map<String, Value>) {
    let text = |file: &Map<String, Value>, key: &str| {
        file.get(key).map(crate::sites::scalar).unwrap_or_default()
    };
    let filename = text(file, "filename");
    if let Some(name) = file.get("name").map(crate::sites::scalar) {
        let (stem, sep, _) = rpartition(&name, ".");
        let name = if sep.is_empty() || stem.is_empty() {
            name.clone()
        } else {
            stem.to_owned()
        };
        file.insert("name".into(), name.clone().into());
        let (_, sep, fid) = rpartition(&filename, "-");
        if sep.is_empty() || fid.chars().count() == 12 {
            file.entry("id").or_insert_with(|| "".into());
            file.insert("filename".into(), name.into());
        } else {
            file.insert("id".into(), fid.into());
            file.insert("filename".into(), format!("{name}-{fid}").into());
        }
    } else if let Some(id) = file.get("id").cloned() {
        file.insert("name".into(), filename.clone().into());
        if Value::String(filename.clone()) != id {
            let id = crate::sites::scalar(&id);
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
    use crate::sites::testing::{http, json, keywords, media, serve};
    use serde_json::json;

    #[tokio::test]
    async fn configured_instance_extracts_albums() {
        crate::sites::instances::define(
            "lolisafe",
            "lolisafe-configured",
            json!({"root": "https://safe.test", "pattern": r"(?:www\.)?safe\.test"}),
        );
        let body = json!({"title": "My &amp; Album", "count": 3, "files": [
            {"name": "photo.jpg", "file": "https://cdn.safe.test/photo-AbCdEfGh.jpg"},
            {"name": "clip.mp4", "file": "https://cdn.safe.test/clip-0123456789ab.mp4"},
            {"id": 7, "file": "https://safe.test/raw-x.png"},
            {"name": "bad.jpg", "file": "ftp://cdn.safe.test/bad.jpg"},
        ]});
        let (origin, server) = serve(vec![json(body)]);
        let http = http("lolisafe", origin);
        let url = Url::parse("https://www.safe.test/a/AlbumID?x=1").unwrap();
        let site = crate::sites::find(&url).unwrap();
        assert_eq!(site.category, "lolisafe-configured");
        assert_eq!(
            crate::sites::find(&Url::parse("https://safe.test/f/x").unwrap()).map(|s| s.category),
            None
        );
        let files = media((site.extract)(&http, url)).await;
        assert_eq!(files.len(), 3);
        let k = keywords(&files[0]);
        assert_eq!(k["category"], "lolisafe-configured");
        assert_eq!(k["subcategory"], "album");
        assert_eq!(k["album_id"], "AlbumID");
        assert_eq!(k["album_name"], "My & Album");
        assert_eq!(k["count"], 3);
        assert_eq!(k["num"], 1);
        assert_eq!(k["name"], "photo");
        assert_eq!(k["id"], "AbCdEfGh");
        assert_eq!(k["filename"], "photo-AbCdEfGh");
        assert_eq!(k["extension"], "jpg");
        let k = keywords(&files[1]);
        assert_eq!(k["id"], "");
        assert_eq!(k["filename"], "clip");
        let k = keywords(&files[2]);
        assert_eq!(k["name"], "raw-x");
        assert_eq!(k["filename"], "raw-x-7");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "photo-AbCdEfGh.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("lolisafe-configured").join("My & Album (AlbumID)")
        );
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("GET /api/album/get/AlbumID "),
            "{}",
            requests[0]
        );
        assert_eq!(
            crate::sites::instances::basecategory("cyberdrop"),
            Some("lolisafe")
        );
    }
}
