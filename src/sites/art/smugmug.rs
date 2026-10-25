//! SmugMug albums, images and folder paths, mirroring
//! gallery_dl/extractor/smugmug.py through the API v2 with its public
//! `APIKey` (the `api-key` setting replaces it).
//!
//! Upstream's OAuth 1.0 `access-token`/`access-token-secret` signing is not
//! supported, so only public content is reachable; `oauth:smugmug` still
//! obtains (and caches) such tokens for the compatibility backend.
use super::{nameext_into, str_of, truthy};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use futures_util::{StreamExt, stream};
use serde_json::{Map, Value, json};
use std::sync::Arc;
use url::Url;

const API_DOMAIN: &str = "api.smugmug.com";
pub(crate) const API_KEY: &str = "RCVHDGjcbc4Fhzq4qzqLdZmvwmwB6LM2";
/// Upstream's OAuth consumer secret, used by `oauth:smugmug`.
pub(crate) const API_SECRET: &str =
    "jGrdndvJqhTx8XSNs7TFTSSthhZHq92ddMpbpDpkDVNM7TDgnvLFMtfB5Mg5kH73";
const HEADERS: &[(&str, &str)] = &[("Accept", "application/json")];

pub static SITE: Site = Site {
    category: "smugmug",
    name: "SmugMug",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&ALBUM, &IMAGE, &PATH],
    api_key: true,
    cookies: None,
};

const FILENAME: &str =
    "{category}_{User[NickName]:?/_/}{Image[UploadKey]}_{Image[ImageKey]}.{extension}";

static ALBUM: Route = Route::new(
    "album",
    FILENAME,
    &["{category}", "{User[NickName]}", "{Album[Name]}"],
    "a_{Album[AlbumKey]}_{Image[ImageKey]}",
);
static IMAGE: Route = Route::new("image", FILENAME, &["{category}"], "{Image[ImageKey]}");
static PATH: Route = Route::new("path", FILENAME, &["{category}"], "");

#[derive(Debug, PartialEq)]
enum Target {
    Album(String),
    Image(String),
    /// Custom domain or `USER.smugmug.com` user, and the raw URL path.
    Path(Option<String>, Option<String>, String),
}

fn target(url: &Url) -> Option<Target> {
    if url.scheme() == "smugmug" {
        let rest = url.as_str().strip_prefix("smugmug:")?;
        if let Some(id) = rest.strip_prefix("album:") {
            // `smugmug:album:([^:]+)$`, limited to SmugMug's key alphabet.
            let valid = (1..=64).contains(&id.len())
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
            return valid.then(|| Target::Album(id.to_owned()));
        }
        // `smugmug:(?!album:)(?:https?://)?([^/]+)`: an explicit custom domain.
        let rest = rest
            .strip_prefix("https://")
            .or_else(|| rest.strip_prefix("http://"))
            .unwrap_or(rest);
        let inner = Url::parse(&format!("https://{rest}")).ok()?;
        if !crate::sites::plain(&inner) {
            return None;
        }
        let domain = inner.host_str()?.to_owned();
        return location(&inner, Some(domain), None);
    }
    // `([\w-]+)\.smugmug\.com`
    if !crate::sites::plain(url) {
        return None;
    }
    let user = url.host_str()?.strip_suffix(".smugmug.com")?;
    let valid = (1..=63).contains(&user.len())
        && user
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !valid {
        return None;
    }
    location(url, None, Some(user.to_owned()))
}

/// The image (`(?:/[^/?#]+)+/i-([^/?#-]+)`) or path
/// (`((?:/[^/?#a-fh-mo-z][^/?#]*)*)/?$`) part of a SmugMug URL.
fn location(url: &Url, domain: Option<String>, user: Option<String>) -> Option<Target> {
    let path = url.path();
    let parts: Vec<&str> = path.split('/').skip(1).collect();
    let leading = parts.iter().take_while(|s| !s.is_empty()).count();
    for k in (1..leading).rev() {
        if let Some(rest) = parts[k].strip_prefix("i-") {
            let id = rest.split('-').next().unwrap_or_default();
            if !id.is_empty() {
                return Some(Target::Image(id.to_owned()));
            }
        }
    }
    if url.query().is_some() || url.fragment().is_some() {
        return None;
    }
    let trimmed = path.strip_suffix('/').unwrap_or(path);
    let allowed = |segment: &&str| {
        segment
            .bytes()
            .next()
            .is_some_and(|b| !matches!(b, b'a'..=b'f' | b'h'..=b'm' | b'o'..=b'z'))
    };
    if !trimmed.is_empty() && !trimmed.split('/').skip(1).all(|s| allowed(&s)) {
        return None;
    }
    Some(Target::Path(domain, user, trimmed.to_owned()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported SmugMug URL"));
    };
    let api = Arc::new(Api {
        http,
        key: http.api_key().unwrap_or(API_KEY).to_owned(),
    });
    match target {
        Target::Album(id) => album(api, id),
        Target::Image(id) => crate::sites::once(async move {
            let mut image = api
                .expansion(&format!("image/{id}"), Some("ImageSizeDetails"), &[])
                .await?;
            let Some(url) = select_format(&mut image, http.option_bool("videos", true)) else {
                return Err(error("SmugMug image has no downloadable size"));
            };
            let mut data = Map::new();
            data.insert("Image".into(), image.into());
            let items = item(url, data).into_iter().collect();
            Ok(crate::sites::events(crate::sites::post(
                &SITE, http, &IMAGE, items,
            )))
        }),
        Target::Path(domain, user, path) => {
            crate::sites::once(async move { path_children(api, domain, user, path).await })
        }
    }
}

/// Upstream `SmugmugAPI`: API v2 requests with `APIKey` and `_verbosity=1`.
struct Api<'a> {
    http: &'a Http,
    key: String,
}

impl<'a> Api<'a> {
    async fn call(&self, endpoint: &str, params: &[(&str, String)], domain: &str) -> Result<Value> {
        let mut url = self
            .http
            .api(&format!("https://{domain}/api/v2/{endpoint}"))?;
        {
            let mut query = url.query_pairs_mut();
            for (key, value) in params {
                query.append_pair(key, value);
            }
            query
                .append_pair("APIKey", &self.key)
                .append_pair("_verbosity", "1");
        }
        let data = self.http.json_with(&url, HEADERS).await.map_err(|e| {
            match crate::sites::http::refused_status(&e) {
                Some(_) => e,
                None if e.to_string().contains("invalid JSON") => {
                    error("SmugMug resource not found")
                }
                None => e,
            }
        })?;
        match data.get("Code").and_then(Value::as_i64) {
            Some(200..400) => Ok(data),
            Some(404) => Err(error("SmugMug resource not found")),
            Some(429) => Err(error("SmugMug rate limit reached")),
            _ => Err(error("SmugMug API request failed")),
        }
    }

    /// Upstream `_expansion`: the first object of an expanded response.
    async fn expansion(
        &self,
        endpoint: &str,
        expands: Option<&str>,
        params: &[(&str, String)],
    ) -> Result<Map<String, Value>> {
        let endpoint = match expands {
            Some(expands) => format!("{endpoint}?_expand={expands}"),
            None => endpoint.to_owned(),
        };
        let data = self.call(&endpoint, params, API_DOMAIN).await?;
        apply_expansions(&data, expands)
            .into_iter()
            .next()
            .and_then(|obj| obj.as_object().cloned())
            .ok_or_else(|| error("SmugMug resource not found"))
    }

    /// Upstream `_pagination`: 100 objects per `start` offset until the
    /// response has no `NextPage`.
    fn pagination<F>(
        self: Arc<Self>,
        endpoint: String,
        expands: Option<&'static str>,
        page: F,
    ) -> Events<'a>
    where
        F: Fn(Vec<Value>) -> Events<'a> + Clone + Send + 'a,
    {
        let endpoint = match expands {
            Some(expands) => format!("{endpoint}?_expand={expands}"),
            None => endpoint,
        };
        crate::sites::paginate(Some(1u64), move |start| {
            let (api, endpoint, page) = (self.clone(), endpoint.clone(), page.clone());
            async move {
                let Some(start) = start else {
                    return Ok(None);
                };
                let params = [("start", start.to_string()), ("count", "100".to_owned())];
                let data = api.call(&endpoint, &params, API_DOMAIN).await?;
                let more = data
                    .get("Response")
                    .and_then(|r| r.get("Pages"))
                    .is_some_and(|p| p.get("NextPage").is_some());
                let objects = apply_expansions(&data, expands);
                Ok(Some((page(objects), more.then_some(start + 100))))
            }
        })
    }
}

/// Upstream `_apply_expansions`: the response's `Locator` objects with
/// each requested `Uris` entry replaced by its expanded object.
pub(crate) fn apply_expansions(data: &Value, expands: Option<&str>) -> Vec<Value> {
    fn unwrap(response: &Value) -> Value {
        let locator = str_of(response, "Locator");
        response.get(locator).cloned().unwrap_or(json!([]))
    }
    let objects = match unwrap(data.get("Response").unwrap_or(&Value::Null)) {
        Value::Array(objects) => objects,
        other => vec![other],
    };
    let (Some(expansions), Some(expands)) = (data.get("Expansions"), expands) else {
        return objects;
    };
    objects
        .into_iter()
        .map(|mut obj| {
            if let Some(Value::Object(uris)) = obj.get_mut("Uris") {
                for name in expands.split(',') {
                    if let Some(uri) = uris.get(name).and_then(Value::as_str) {
                        let expanded = unwrap(expansions.get(uri).unwrap_or(&Value::Null));
                        uris.insert(name.into(), expanded);
                    }
                }
            }
            obj
        })
        .collect()
}

/// Upstream `_select_format`: the original image, else the widest image
/// (or video) size; its URL and dimensions are copied onto the image.
pub(crate) fn select_format(image: &mut Map<String, Value>, videos: bool) -> Option<Url> {
    let uris = image.remove("Uris")?;
    let details = uris.get("ImageSizeDetails")?.as_object()?;
    let widest = |prefix: &str| {
        details
            .iter()
            .filter(|(key, value)| key.starts_with(prefix) && value.is_object())
            .fold(None, |best: Option<&Value>, (_, size)| {
                let width = |v: &Value| v.get("Width").and_then(Value::as_f64).unwrap_or(0.0);
                match best {
                    Some(b) if width(b) >= width(size) => Some(b),
                    _ => Some(size),
                }
            })
    };
    let media = if videos && truthy(image.get("IsVideo")) {
        widest("VideoSize")?
    } else if let Some(original) = details.get("ImageSizeOriginal") {
        original
    } else {
        widest("ImageSize")?
    };
    for key in [
        "Url",
        "Width",
        "Height",
        "MD5",
        "Size",
        "Watermarked",
        "Bitrate",
        "Duration",
    ] {
        if let Some(value) = media.get(key) {
            image.insert(key.into(), value.clone());
        }
    }
    crate::sites::media_url(image.get("Url")?.as_str()?, &["smugmug.com"])
}

fn item(url: Url, mut data: Map<String, Value>) -> Option<Item> {
    nameext_into(url.as_str(), &mut data);
    let image = data.get("Image")?;
    let key = str_of(image, "ImageKey").to_owned();
    let album = data
        .get("Album")
        .map(|a| str_of(a, "AlbumKey").to_owned())
        .unwrap_or_else(|| key.clone());
    let user = data.get("User").map(|u| str_of(u, "NickName").to_owned());
    let caption = str_of(image, "Caption").to_owned();
    Some(
        Item::new(url, data)
            .ids(album, key)
            .user(user)
            .caption(Some(caption)),
    )
}

/// Upstream `empty_user`, without its `Uris`.
fn empty_user() -> Value {
    json!({"Uri": "", "ResponseLevel": "Public", "Name": "", "NickName": "",
        "QuickShare": false, "RefTag": "", "ViewPassHint": "", "WebUri": ""})
}

/// Upstream `SmugmugAlbumExtractor`: album and owner, then every image.
fn album(api: Arc<Api<'_>>, id: String) -> Events<'_> {
    crate::sites::once(async move {
        let mut album = api
            .expansion(&format!("album/{id}"), Some("User"), &[])
            .await?;
        let user = match album.get_mut("Uris").and_then(|u| u.get_mut("User")) {
            Some(Value::Object(user)) => {
                let mut user = std::mem::take(user);
                user.remove("Uris");
                Value::Object(user)
            }
            _ => empty_user(),
        };
        album.remove("Uris");
        let mut data = Map::new();
        data.insert("Album".into(), album.into());
        data.insert("User".into(), user);
        let http = api.http;
        let videos = http.option_bool("videos", true);
        Ok(api.pagination(
            format!("album/{id}!images"),
            Some("ImageSizeDetails"),
            move |images| {
                let events = images
                    .into_iter()
                    .filter_map(|image| {
                        let mut image = image.as_object()?.clone();
                        let url = select_format(&mut image, videos)?;
                        let mut fields = data.clone();
                        fields.insert("Image".into(), image.into());
                        item(url, fields)
                    })
                    .flat_map(|item| crate::sites::post(&SITE, http, &ALBUM, vec![item]))
                    .collect();
                crate::sites::events(events)
            },
        ))
    })
}

/// Queue `smugmug:album:ID` with the node or album object.
fn queue_album<'a>(http: &'a Http, id: &str, data: &Value) -> Events<'a> {
    let Ok(url) = Url::parse(&format!("smugmug:album:{id}")) else {
        return crate::sites::events(Vec::new());
    };
    if !matches!(target(&url), Some(Target::Album(_))) {
        return crate::sites::events(Vec::new());
    }
    let data = data.as_object().cloned().unwrap_or_default();
    crate::sites::queue(http, "smugmug", "path", url, &data)
}

/// The album key at the end of a node's `Uris.Album`.
fn node_album(node: &Value) -> String {
    let uri = node
        .get("Uris")
        .and_then(|u| u.get("Album"))
        .map(|a| match a {
            Value::Object(_) => str_of(a, "Uri"),
            _ => a.as_str().unwrap_or_default(),
        })
        .unwrap_or_default();
    uri.rsplit('/').next().unwrap_or_default().to_owned()
}

/// Upstream `album_nodes`: albums below a folder node, depth first.
fn album_nodes<'a>(api: Arc<Api<'a>>, node_id: String) -> Events<'a> {
    let inner = api.clone();
    api.pagination(format!("node/{node_id}!children"), None, move |nodes| {
        let api = inner.clone();
        stream::iter(nodes)
            .map(move |node| match str_of(&node, "Type") {
                "Album" => queue_album(api.http, &node_album(&node), &node),
                "Folder" => album_nodes(api.clone(), str_of(&node, "NodeID").to_owned()),
                _ => crate::sites::events(Vec::new()),
            })
            .flatten()
            .boxed()
    })
}

/// Upstream `SmugmugPathExtractor.items`: queue the albums of a node path,
/// or every album of the user.
async fn path_children<'a>(
    api: Arc<Api<'a>>,
    domain: Option<String>,
    user: Option<String>,
    path: String,
) -> Result<Events<'a>> {
    let user = match (user, domain) {
        (Some(user), _) => user,
        (None, Some(domain)) => {
            let data = api.call("!siteuser", &[], &domain).await?;
            let user = data
                .get("Response")
                .and_then(|r| r.get("User"))
                .map(|u| str_of(u, "NickName"))
                .unwrap_or_default();
            if !super::segment(user) {
                return Err(error("SmugMug site has no user"));
            }
            user.to_owned()
        }
        (None, None) => return Err(error("SmugMug URL names no user")),
    };
    let http = api.http;
    if path.is_empty() {
        return Ok(
            api.pagination(format!("user/{user}!albums"), None, move |albums| {
                stream::iter(albums)
                    .map(move |album| queue_album(http, str_of(&album, "AlbumKey"), &album))
                    .flatten()
                    .boxed()
            }),
        );
    }
    let node = match path.strip_prefix("/gallery/n-") {
        Some(id) => api.expansion(&format!("node/{id}"), None, &[]).await?,
        None => {
            let data = api
                .expansion(
                    &format!("user/{user}!urlpathlookup"),
                    Some("Node"),
                    &[("urlpath", path.clone())],
                )
                .await?;
            data.get("Uris")
                .and_then(|u| u.get("Node"))
                .and_then(Value::as_object)
                .cloned()
                .ok_or_else(|| error("SmugMug path has no node"))?
        }
    };
    let node = Value::Object(node);
    Ok(match str_of(&node, "Type") {
        "Album" => queue_album(http, &node_album(&node), &node),
        "Folder" => album_nodes(api, str_of(&node, "NodeID").to_owned()),
        _ => crate::sites::events(Vec::new()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http, json, keywords, media, queued, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("smugmug:album:cr4C7f"),
            Some(Target::Album("cr4C7f".into()))
        );
        assert_eq!(
            t("https://tdm.smugmug.com/Nature/Dove/i-kCsLJT6"),
            Some(Target::Image("kCsLJT6".into()))
        );
        assert_eq!(
            t("https://tdm.smugmug.com/Nature/Dove/i-kCsLJT6-X3?x=1"),
            Some(Target::Image("kCsLJT6".into()))
        );
        assert_eq!(
            t("https://tdm.smugmug.com/Nature/Dove"),
            Some(Target::Path(
                None,
                Some("tdm".into()),
                "/Nature/Dove".into()
            ))
        );
        assert_eq!(
            t("https://tdm.smugmug.com/"),
            Some(Target::Path(None, Some("tdm".into()), String::new()))
        );
        assert_eq!(
            t("https://tdm.smugmug.com/gallery/n-GLCjnD/"),
            Some(Target::Path(
                None,
                Some("tdm".into()),
                "/gallery/n-GLCjnD".into()
            ))
        );
        assert_eq!(
            t("smugmug:https://www.creativedogportraits.com/PortfolioGallery/"),
            Some(Target::Path(
                Some("www.creativedogportraits.com".into()),
                None,
                "/PortfolioGallery".into()
            ))
        );
        assert_eq!(
            t("smugmug:www.sitkapics.com/TREES-and-TRAILS/i-xGdx3XQ"),
            Some(Target::Image("xGdx3XQ".into()))
        );
        for raw in [
            "smugmug:album:a/b",
            "smugmug:album:",
            "https://tdm.smugmug.com/browse",
            "https://tdm.smugmug.com/Nature?x=1",
            "https://tdm.smugmug.com/i-abc",
            "https://smugmug.com/Nature",
            "https://a.b.smugmug.com/Nature",
            "https://tdm.smugmug.com.evil.com/Nature",
            "https://tdm.smugmug.com:8443/Nature",
            "https://u:p@tdm.smugmug.com/Nature",
            "smugmug:https://u:p@example.com/Nature",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn image(key: &str) -> Value {
        json!({"ImageKey": key, "UploadKey": "123", "FileName": "a.jpg", "IsVideo": false,
            "Caption": "Cap",
            "Uris": {"ImageSizeDetails": format!("/api/v2/image/{key}-0!sizedetails")}})
    }

    fn sizes(key: &str) -> Value {
        json!({"Locator": "ImageSizeDetails", "ImageSizeDetails": {
            "ImageUrlTemplate": "x",
            "ImageSizeMedium": {"Url": format!("https://photos.smugmug.com/M/{key}.jpg"), "Width": 600, "Height": 400},
            "ImageSizeX3Large": {"Url": format!("https://photos.smugmug.com/X3/{key}.jpg"), "Width": 1600, "Height": 1067, "MD5": "m"}}})
    }

    #[tokio::test]
    async fn albums_page_through_images() {
        let album = json!({"Code": 200, "Response": {"Locator": "Album", "Album": {
                "AlbumKey": "cr4C7f", "Name": "Birds",
                "Uris": {"User": "/api/v2/user/tdm"}}},
            "Expansions": {"/api/v2/user/tdm": {"Locator": "User", "User": {
                "NickName": "tdm", "Name": "T", "Uris": {"x": 1}}}}});
        let page = |key: &str, next: bool| {
            let mut pages = json!({"Total": 2});
            if next {
                pages["NextPage"] = "/next".into();
            }
            json!({"Code": 200, "Response": {"Locator": "AlbumImage", "AlbumImage": [image(key)],
                "Pages": pages},
                "Expansions": {format!("/api/v2/image/{key}-0!sizedetails"): sizes(key)}})
        };
        let (root, server) = serve(vec![
            json(album),
            json(page("k1", true)),
            json(page("k2", false)),
        ]);
        let http = http("smugmug", root);
        let files = media(extract(&http, Url::parse("smugmug:album:cr4C7f").unwrap())).await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].url.as_str(),
            "https://photos.smugmug.com/X3/k1.jpg"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["Image"]["Width"], 1600);
        assert_eq!(k["Image"]["MD5"], "m");
        assert!(k["Image"].get("Uris").is_none());
        assert!(k["Album"].get("Uris").is_none());
        assert!(k["User"].get("Uris").is_none());
        assert_eq!(k["User"]["NickName"], "tdm");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "smugmug_tdm_123_k1.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("smugmug/tdm/Birds")
        );
        let (_, identity) = crate::sites::archive_key(&files[1]).unwrap();
        assert_eq!(
            identity,
            r#"["a_{Album[AlbumKey]}_{Image[ImageKey]}",["cr4C7f","k2"]]"#
        );
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with(&format!(
                "GET /api/v2/album/cr4C7f?_expand=User&APIKey={API_KEY}&_verbosity=1 "
            )),
            "{}",
            requests[0]
        );
        assert!(requests[0].contains("accept: application/json"));
        assert!(requests[1].starts_with(&format!(
            "GET /api/v2/album/cr4C7f!images?_expand=ImageSizeDetails&start=1&count=100\
             &APIKey={API_KEY}&_verbosity=1 "
        )));
        assert!(requests[2].contains("&start=101&count=100&"));
    }

    #[tokio::test]
    async fn images_use_the_original_size() {
        let mut details = sizes("k9");
        details["ImageSizeDetails"]["ImageSizeOriginal"] =
            json!({"Url": "https://photos.smugmug.com/O/k9.png", "Width": 10, "Size": 5});
        let data = json!({"Code": 200, "Response": {"Locator": "Image", "Image": image("k9")},
            "Expansions": {"/api/v2/image/k9-0!sizedetails": details}});
        let (root, server) = serve(vec![json(data)]);
        let http = http("smugmug", root).with_api_key(Some("KEY"));
        let files = media(extract(
            &http,
            Url::parse("https://tdm.smugmug.com/Nature/i-k9").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(keywords(&files[0])["Image"]["Size"], 5);
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "smugmug_123_k9.png");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("smugmug")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{Image[ImageKey]}",["k9"]]"#);
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with(
                "GET /api/v2/image/k9?_expand=ImageSizeDetails&APIKey=KEY&_verbosity=1 "
            )
        );
    }

    #[tokio::test]
    async fn folder_paths_queue_nested_albums() {
        let lookup = json!({"Code": 200, "Response": {"Locator": "Folder", "Folder": {
                "Uris": {"Node": "/api/v2/node/F1"}}},
            "Expansions": {"/api/v2/node/F1": {"Locator": "Node", "Node": {
                "Type": "Folder", "NodeID": "F1"}}}});
        let children = |nodes: Value| json!({"Code": 200, "Response": {"Locator": "Node", "Node": nodes, "Pages": {}}});
        let (root, server) = serve(vec![
            json(lookup),
            json(children(json!([
                {"Type": "Folder", "NodeID": "F2"},
                {"Type": "Album", "NodeID": "A1", "Name": "One",
                 "Uris": {"Album": "/api/v2/album/AK1"}}]))),
            json(children(json!([
                {"Type": "Album", "NodeID": "A2", "Uris": {"Album": "/api/v2/album/AK2"}},
                {"Type": "Page", "NodeID": "P1"}]))),
        ]);
        let http = http("smugmug", root);
        let queued = queued(extract(
            &http,
            Url::parse("https://tdm.smugmug.com/Nature").unwrap(),
        ))
        .await;
        assert_eq!(queued.len(), 2);
        assert_eq!(queued[0].subcategory(), "path");
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with(&format!(
                "GET /api/v2/user/tdm!urlpathlookup?_expand=Node&urlpath=%2FNature\
                 &APIKey={API_KEY}&_verbosity=1 "
            )),
            "{}",
            requests[0]
        );
        assert!(requests[1].starts_with("GET /api/v2/node/F1!children?start=1&count=100&"));
        assert!(requests[2].starts_with("GET /api/v2/node/F2!children?start=1&count=100&"));
    }

    #[tokio::test]
    async fn custom_domains_resolve_their_user() {
        let (root, server) = serve(vec![
            json(json!({"Code": 200, "Response": {"User": {"NickName": "dogs"}}})),
            json(
                json!({"Code": 200, "Response": {"Locator": "Album", "Album": [
                {"AlbumKey": "X1", "Name": "A"}], "Pages": {}}}),
            ),
        ]);
        let http = http("smugmug", root);
        let queued = queued(extract(
            &http,
            Url::parse("smugmug:https://www.example.com/").unwrap(),
        ))
        .await;
        assert_eq!(queued.len(), 1);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with(&format!(
            "GET /api/v2/!siteuser?APIKey={API_KEY}&_verbosity=1 "
        )));
        assert!(requests[0].contains("host: 127.0.0.1"));
        assert!(requests[1].starts_with("GET /api/v2/user/dogs!albums?start=1&count=100&"));
    }
}
