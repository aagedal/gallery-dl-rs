//! SmugMug albums, images, paths and users over the SmugMug API v2,
//! mirroring gallery_dl/extractor/smugmug.py. Requests use the public API
//! key, or OAuth 1.0 with `access-token` and `access-token-secret`.
use crate::error;
use crate::sites::gdl::{generate_token, nameext_into, oauth1_header, re, rpartition, truthy};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value, json};
use url::Url;

const API_KEY: &str = "RCVHDGjcbc4Fhzq4qzqLdZmvwmwB6LM2";
const API_SECRET: &str = "jGrdndvJqhTx8XSNs7TFTSSthhZHq92ddMpbpDpkDVNM7TDgnvLFMtfB5Mg5kH73";
/// Default OAuth 1.0a consumer key and secret, for `oauth:smugmug`.
pub(crate) const API_CREDENTIALS: (&str, &str) = (API_KEY, API_SECRET);
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

pub static SITE: Site = Site {
    category: "smugmug",
    name: "SmugMug",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&ALBUM, &IMAGE, &PATH],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Album(String),
    /// Custom domain, user and image id.
    Image(String, String, String),
    /// Custom domain, user and path.
    Path(String, String, String),
}

fn target(url: &Url) -> Option<Target> {
    let raw = url.as_str();
    if let Some(c) = re!(r"^smugmug:album:([^:]+)$").captures(raw) {
        return Some(Target::Album(c[1].to_owned()));
    }
    const BASE: &str = r"^(?:smugmug:(?:https?://)?([^/]+)|https?://([\w-]+)\.smugmug\.com)";
    let opt = |c: &regex::Captures, i: usize| c.get(i).map_or("", |m| m.as_str()).to_owned();
    // Upstream's `smugmug:(?!album:)`.
    if raw.starts_with("smugmug:album:") {
        return None;
    }
    if let Some(c) = re!(&format!(r"{BASE}(?:/[^/?#]+)+/i-([^/?#-]+)")).captures(raw) {
        return Some(Target::Image(opt(&c, 1), opt(&c, 2), opt(&c, 3)));
    }
    let c = re!(&format!(r"{BASE}((?:/[^/?#a-fh-mo-z][^/?#]*)*)/?$")).captures(raw)?;
    Some(Target::Path(opt(&c, 1), opt(&c, 2), opt(&c, 3)))
}

/// `SmugmugAPI._call`.
async fn call(
    http: &Http,
    endpoint: &str,
    params: &[(&str, String)],
    domain: &str,
) -> crate::Result<Value> {
    let option = |key: &str| http.option(key).and_then(Value::as_str).map(str::to_owned);
    let api_key = option("api-key").unwrap_or_else(|| API_KEY.into());
    let api_secret = option("api-secret").unwrap_or_else(|| API_SECRET.into());
    let token = option("access-token").filter(|t| t != "cache");
    let token_secret = option("access-token-secret");
    let mut url = http.api(&format!("https://{domain}/api/v2/{endpoint}"))?;
    {
        let mut pairs = url.query_pairs_mut();
        pairs.extend_pairs(params.iter().map(|(k, v)| (*k, v.as_str())));
        if token.is_none() {
            pairs.append_pair("APIKey", &api_key);
        }
        pairs.append_pair("_verbosity", "1");
    }
    let mut headers = vec![("Accept", "application/json".to_owned())];
    if let (Some(token), Some(secret)) = (&token, &token_secret) {
        let nonce: String = generate_token(8)
            .chars()
            .map(|c| char::from(b'a' + (c.to_digit(16).unwrap_or(0) as u8)))
            .collect();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or_default();
        headers.push((
            "Authorization",
            oauth1_header(
                "GET",
                &url,
                (&api_key, &api_secret),
                (token, secret),
                &nonce,
                now,
            ),
        ));
    }
    let header_refs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    // The API redirects to canonical URIs (`image/ID` → `image/ID-0`,
    // user name case) with empty bodies; follow those like `requests`.
    let mut hops = 0;
    let body = loop {
        let (status, body) = http.get_status(&url, &header_refs).await?;
        if !(300..400).contains(&status) || hops == 3 {
            break body;
        }
        match http.location(&url, &header_refs).await? {
            Some(next) if next.host_str() == url.host_str() => url = next,
            _ => break body,
        }
        hops += 1;
    };
    let data: Value =
        serde_json::from_str(&body).map_err(|_| error("SmugMug resource not found"))?;
    match data["Code"].as_i64().unwrap_or_default() {
        200..=399 => Ok(data),
        404 => Err(error("SmugMug resource not found")),
        429 => Err(error("Rate limit reached")),
        _ => Err(error("API request failed")),
    }
}

/// `_apply_expansions`: response objects with their expanded `Uris`.
fn apply_expansions(data: &Value, expands: &str) -> Vec<Value> {
    let unwrap = |response: &Value| -> Value {
        let locator = response["Locator"].as_str().unwrap_or_default();
        response.get(locator).cloned().unwrap_or_else(|| json!([]))
    };
    let objs = unwrap(&data["Response"]);
    let mut objs = match objs {
        Value::Array(list) => list,
        other => vec![other],
    };
    if let Some(expansions) = data.get("Expansions") {
        for obj in &mut objs {
            for name in expands.split(',').filter(|n| !n.is_empty()) {
                let Some(uri) = obj["Uris"]
                    .get(name)
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                else {
                    continue;
                };
                obj["Uris"][name] = unwrap(&expansions[&uri]);
            }
        }
    }
    objs
}

fn extend(endpoint: &str, expands: &str) -> String {
    if expands.is_empty() {
        endpoint.to_owned()
    } else {
        format!("{endpoint}?_expand={expands}")
    }
}

/// `_expansion`: the first object of one expanded call.
async fn expansion(
    http: &Http,
    endpoint: &str,
    expands: &str,
    params: &[(&str, String)],
) -> crate::Result<Value> {
    let data = call(http, &extend(endpoint, expands), params, "api.smugmug.com").await?;
    apply_expansions(&data, expands)
        .into_iter()
        .next()
        .filter(|v| !v.is_null())
        .ok_or_else(|| error("SmugMug resource not found"))
}

/// `_pagination` over 100-item pages.
async fn paginate_all(http: &Http, endpoint: &str, expands: &str) -> crate::Result<Vec<Value>> {
    let endpoint = extend(endpoint, expands);
    let mut out = Vec::new();
    let mut start = 1;
    loop {
        let data = call(
            http,
            &endpoint,
            &[("start", start.to_string()), ("count", "100".into())],
            "api.smugmug.com",
        )
        .await?;
        out.extend(apply_expansions(&data, expands));
        if data["Response"]["Pages"].get("NextPage").is_none() {
            return Ok(out);
        }
        start += 100;
    }
}

/// `_select_format`: the original, else the widest image or video size.
fn select_format(
    http: &Http,
    mut image: Map<String, Value>,
) -> Option<(String, Map<String, Value>)> {
    let details = image
        .get("Uris")?
        .get("ImageSizeDetails")?
        .as_object()?
        .clone();
    let videos = http.option("videos").is_none_or(|v| truthy(Some(v)));
    let media = if videos && truthy(image.get("IsVideo")) {
        None
    } else {
        details.get("ImageSizeOriginal").cloned()
    };
    let media = match media {
        Some(media) => media,
        None => {
            let prefix = if videos && truthy(image.get("IsVideo")) {
                "VideoSize"
            } else {
                "ImageSize"
            };
            details
                .iter()
                .filter(|(k, _)| k.starts_with(prefix))
                .max_by_key(|(_, v)| v["Width"].as_i64().unwrap_or_default())?
                .1
                .clone()
        }
    };
    image.remove("Uris");
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
    Some((image.get("Url")?.as_str()?.to_owned(), image))
}

fn empty_user() -> Value {
    json!({"Uri": "", "ResponseLevel": "Public", "Name": "", "NickName": "", "QuickShare": false, "RefTag": "", "ViewPassHint": "", "WebUri": ""})
}

/// Descendant album nodes of a folder.
async fn album_nodes(http: &Http, root: &Value) -> crate::Result<Vec<Value>> {
    let mut out = Vec::new();
    let mut stack = vec![crate::sites::scalar(&root["NodeID"])];
    while let Some(node_id) = stack.pop() {
        for node in paginate_all(http, &format!("node/{node_id}!children"), "").await? {
            match node["Type"].as_str() {
                Some("Album") => out.push(node),
                Some("Folder") => stack.push(crate::sites::scalar(&node["NodeID"])),
                _ => {}
            }
        }
    }
    Ok(out)
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported SmugMug URL"));
    };
    crate::sites::once(async move {
        match target {
            Target::Album(album_id) => {
                let mut album = expansion(http, &format!("album/{album_id}"), "User", &[]).await?;
                let mut user = album["Uris"]
                    .get("User")
                    .filter(|u| u.is_object())
                    .cloned()
                    .unwrap_or_else(empty_user);
                if let Some(user) = user.as_object_mut() {
                    user.remove("Uris");
                }
                if let Some(album) = album.as_object_mut() {
                    album.remove("Uris");
                }
                let mut data = Map::new();
                data.insert("Album".into(), album);
                data.insert("User".into(), user);
                let images = paginate_all(
                    http,
                    &format!("album/{album_id}!images"),
                    "ImageSizeDetails",
                )
                .await?;
                let items = images
                    .into_iter()
                    .filter_map(|image| {
                        let (url, image) = select_format(http, image.as_object()?.clone())?;
                        let key = crate::sites::scalar(&image["ImageKey"]);
                        let mut fields = data.clone();
                        fields.insert("Image".into(), Value::Object(image));
                        nameext_into(&url, &mut fields);
                        Some(Item::new(Url::parse(&url).ok()?, fields).ids(album_id.clone(), key))
                    })
                    .collect();
                Ok(crate::sites::events(crate::sites::post(
                    &SITE, http, &ALBUM, items,
                )))
            }
            Target::Image(_, _, image_id) => {
                let image =
                    expansion(http, &format!("image/{image_id}"), "ImageSizeDetails", &[]).await?;
                let (url, image) =
                    select_format(http, image.as_object().cloned().unwrap_or_default())
                        .ok_or_else(|| error("SmugMug image has no file"))?;
                let mut data = Map::new();
                data.insert("Image".into(), Value::Object(image));
                nameext_into(&url, &mut data);
                let file = Url::parse(&url).map_err(|_| error("SmugMug image has no file"))?;
                Ok(crate::sites::events(crate::sites::post(
                    &SITE,
                    http,
                    &IMAGE,
                    vec![Item::new(file, data).ids(image_id.clone(), image_id)],
                )))
            }
            Target::Path(domain, user, path) => {
                let user = if user.is_empty() {
                    let data = call(http, "!siteuser", &[], &domain).await?;
                    crate::sites::scalar(&data["Response"]["User"]["NickName"])
                } else {
                    user
                };
                let mut children = Vec::new();
                if path.is_empty() {
                    for album in paginate_all(http, &format!("user/{user}!albums"), "").await? {
                        let key = crate::sites::scalar(&album["AlbumKey"]);
                        children.extend(
                            Url::parse(&format!("smugmug:album:{key}"))
                                .ok()
                                .zip(album.as_object().cloned()),
                        );
                    }
                } else {
                    let node = match path.strip_prefix("/gallery/n-") {
                        Some(node_id) => {
                            expansion(http, &format!("node/{node_id}"), "", &[]).await?
                        }
                        None => expansion(
                            http,
                            &format!("user/{user}!urlpathlookup"),
                            "Node",
                            &[("urlpath", path.clone())],
                        )
                        .await?["Uris"]["Node"]
                            .clone(),
                    };
                    let nodes = match node["Type"].as_str() {
                        Some("Album") => vec![node],
                        Some("Folder") => album_nodes(http, &node).await?,
                        _ => Vec::new(),
                    };
                    for node in nodes {
                        let album_id =
                            rpartition(node["Uris"]["Album"].as_str().unwrap_or_default(), "/")
                                .2
                                .to_owned();
                        children.extend(
                            Url::parse(&format!("smugmug:album:{album_id}"))
                                .ok()
                                .zip(node.as_object().cloned()),
                        );
                    }
                }
                Ok(crate::sites::galleries::queue(http, &SITE, &PATH, children))
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{children, http as client, json as body, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("smugmug:album:cr4C7f"),
            Some(Target::Album("cr4C7f".into()))
        );
        assert_eq!(
            t("https://tdm.smugmug.com/Nature/Dove/i-kCsLJT6"),
            Some(Target::Image(String::new(), "tdm".into(), "kCsLJT6".into()))
        );
        assert_eq!(
            t("https://tdm.smugmug.com/Nature/Dove"),
            Some(Target::Path(
                String::new(),
                "tdm".into(),
                "/Nature/Dove".into()
            ))
        );
        assert_eq!(
            t("smugmug:https://www.sitkapics.com/"),
            Some(Target::Path(
                "www.sitkapics.com".into(),
                String::new(),
                String::new()
            ))
        );
        assert_eq!(t("https://tdm.smugmug.com/browse"), None);
    }

    #[test]
    fn signs_like_upstream_oauth1() {
        let url = Url::parse("https://api.smugmug.com/api/v2/album/abc!images?_expand=ImageSizeDetails&start=1&count=100&_verbosity=1").unwrap();
        let header = oauth1_header(
            "GET",
            &url,
            ("ck", "cs"),
            ("tk", "ts"),
            "abcdefghijklmnop",
            1_700_000_000,
        );
        // Python reference from gallery_dl.oauth.OAuth1Client.generate_signature.
        assert!(
            header.ends_with("oauth_signature=\"9p9J5twgW1ErFvbcXzBhejGmcsg%3D\""),
            "{header}"
        );
    }

    #[tokio::test]
    async fn follows_api_redirects() {
        let moved = b"HTTP/1.1 301 Moved Permanently\r\nLocation: /api/v2/image/k1-0?_expand=ImageSizeDetails\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec();
        let image = json!({"Code": 200, "Response": {"Locator": "Image", "Image": {"ImageKey": "k1", "UploadKey": "u1", "IsVideo": false,
            "Uris": {"ImageSizeDetails": "/api/v2/image/k1-0!sizedetails"}}},
            "Expansions": {"/api/v2/image/k1-0!sizedetails": {"Locator": "ImageSizeDetails", "ImageSizeDetails": {
                "ImageSizeOriginal": {"Url": "https://photos.smugmug.com/O/a.jpg", "Width": 4000}}}}});
        let (root, server) = serve(vec![moved.clone(), moved, body(image)]);
        let http = client("smugmug", root);
        let files = media(extract(
            &http,
            Url::parse("https://tdm.smugmug.com/Birds/i-k1").unwrap(),
        ))
        .await;
        assert_eq!(files[0].url.as_str(), "https://photos.smugmug.com/O/a.jpg");
        assert!(server.join().unwrap()[2].starts_with("GET /api/v2/image/k1-0?"));
    }

    #[tokio::test]
    async fn album_selects_original_sizes() {
        let album = json!({"Code": 200, "Response": {"Locator": "Album", "Album": {"AlbumKey": "cr4C7f", "Name": "Birds", "Uris": {"User": "/api/v2/user/tdm"}}},
            "Expansions": {"/api/v2/user/tdm": {"Locator": "User", "User": {"NickName": "tdm", "Name": "T", "Uris": {}}}}});
        let images = json!({"Code": 200, "Response": {"Locator": "AlbumImage", "Pages": {}, "AlbumImage": [{"ImageKey": "k1", "UploadKey": "u1", "IsVideo": false,
            "Uris": {"ImageSizeDetails": "/api/v2/image/k1!sizedetails"}}]},
            "Expansions": {"/api/v2/image/k1!sizedetails": {"Locator": "ImageSizeDetails", "ImageSizeDetails": {
                "ImageSizeLarge": {"Url": "https://photos.smugmug.com/L/a.jpg", "Width": 800}, "ImageSizeOriginal": {"Url": "https://photos.smugmug.com/O/a.jpg", "Width": 4000, "MD5": "m"}}}}});
        let (root, server) = serve(vec![body(album), body(images)]);
        let http = client("smugmug", root);
        let files = media(extract(&http, Url::parse("smugmug:album:cr4C7f").unwrap())).await;
        assert_eq!(files[0].url.as_str(), "https://photos.smugmug.com/O/a.jpg");
        let k = keywords(&files[0]);
        assert_eq!(
            (k["Image"]["MD5"].as_str(), k["User"]["NickName"].as_str()),
            (Some("m"), Some("tdm"))
        );
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "smugmug_tdm_u1_k1.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("smugmug/tdm/Birds")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/v2/album/cr4C7f?_expand=User&APIKey="));
    }

    #[tokio::test]
    async fn user_path_queues_albums() {
        let albums = json!({"Code": 200, "Response": {"Locator": "Album", "Pages": {}, "Album": [{"AlbumKey": "a1"}, {"AlbumKey": "a2"}]}});
        let (root, server) = serve(vec![body(albums)]);
        let http = client("smugmug", root);
        assert_eq!(
            children(extract(
                &http,
                Url::parse("https://tdm.smugmug.com/").unwrap()
            ))
            .await
            .len(),
            2
        );
        server.join().unwrap();
    }
}
