//! Iwara videos, images, playlists, profiles, favorites, searches and tag
//! listings, mirroring gallery_dl/extractor/iwara.py through
//! `apiq.iwara.tv`. Options: `format`, `include`, `username`/`password`.
use crate::error;
use crate::sites::gdl::{auth_info, captures, date, group, parse_iso, parse_query, re, str_of};
use crate::sites::{Events, Http, Item, Route, Site};
use futures_util::StreamExt;
use serde_json::{Map, Value, json};
use sha1::Digest;
use url::Url;

const API: &str = "https://apiq.iwara.tv";
const FILENAME: &str = "{date} {id} {title[:200]} {filename}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{user[name]}"];
const ARCHIVE: &str = "{type} {user[name]} {id} {file_id}";

macro_rules! routes {
    ($($name:ident = $sub:literal),* $(,)?) => {
        $(static $name: Route = Route::new($sub, FILENAME, DIRECTORY, ARCHIVE);)*
        const ROUTES: &[&Route] = &[$(&$name),*];
    };
}
routes! {
    USER = "user", USER_IMAGES = "user-images", USER_VIDEOS = "user-videos",
    USER_PLAYLISTS = "user-playlists", FOLLOWING = "following", FOLLOWERS = "followers",
    IMAGE = "image", VIDEO = "video", PLAYLIST = "playlist", FAVORITE = "favorite",
    SEARCH = "search", TAG = "tag",
}

pub static SITE: Site = Site {
    category: "iwara",
    name: "Iwara",
    matches: |url| target(url).is_some(),
    extract,
    routes: ROUTES,
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    User(String),
    UserImages(String, String),
    UserVideos(String, String),
    UserPlaylists(String, String),
    Following(String),
    Followers(String),
    Image(String),
    Video(String),
    Playlist(String),
    Favorite(Option<String>),
    Search(String),
    Tag(String, String),
}

/// The site root (`iwara.tv` or `iwara.ai`) and the matched extractor.
fn target(url: &Url) -> Option<(String, Target)> {
    let c = captures(re!(r"^https?://(?:www\.)?iwara\.(tv|ai)(/.*)?$"), url)?;
    let tld = group(&c, 1).to_owned();
    let rest = group(&c, 2);
    let opt =
        |c: &regex::Captures<'_>, i| c.get(i).map_or(String::new(), |m| m.as_str().to_owned());
    let target = if let Some(c) = re!(r"^/profile/([^/?#]+)/?$").captures(rest) {
        Target::User(c[1].to_owned())
    } else if let Some(c) = re!(r"^/profile/([^/?#]+)/images(?:\?([^#]+))?").captures(rest) {
        Target::UserImages(c[1].to_owned(), opt(&c, 2))
    } else if let Some(c) = re!(r"^/profile/([^/?#]+)/videos(?:\?([^#]+))?").captures(rest) {
        Target::UserVideos(c[1].to_owned(), opt(&c, 2))
    } else if let Some(c) = re!(r"^/profile/([^/?#]+)/playlists(?:\?([^#]+))?").captures(rest) {
        Target::UserPlaylists(c[1].to_owned(), opt(&c, 2))
    } else if let Some(c) = re!(r"^/profile/([^/?#]+)/following").captures(rest) {
        Target::Following(c[1].to_owned())
    } else if let Some(c) = re!(r"^/profile/([^/?#]+)/followers").captures(rest) {
        Target::Followers(c[1].to_owned())
    } else if let Some(c) = re!(r"^/image/([^/?#]+)").captures(rest) {
        Target::Image(c[1].to_owned())
    } else if let Some(c) = re!(r"^/video/([^/?#]+)").captures(rest) {
        Target::Video(c[1].to_owned())
    } else if let Some(c) = re!(r"^/playlist/([^/?#]+)").captures(rest) {
        Target::Playlist(c[1].to_owned())
    } else if let Some(c) = re!(r"^/favorites(?:/(image|video)s)?").captures(rest) {
        Target::Favorite(c.get(1).map(|m| m.as_str().to_owned()))
    } else if let Some(c) = re!(r"^/search\?([^#]+)").captures(rest) {
        Target::Search(c[1].to_owned())
    } else {
        let c = re!(r"^/(image|video)s(?:\?([^#]+))?").captures(rest)?;
        Target::Tag(c[1].to_owned(), opt(&c, 2))
    };
    Some((format!("https://www.iwara.{tld}"), target))
}

struct Api<'a> {
    http: &'a Http,
    root: String,
    auth: Option<String>,
}

/// Access tokens by username, valid for the process (upstream: 1 hour).
fn token_cache() -> &'static std::sync::Mutex<std::collections::HashMap<String, String>> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, String>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

impl<'a> Api<'a> {
    async fn new(http: &'a Http, root: &str) -> crate::Result<Self> {
        let mut api = Self {
            http,
            root: root.to_owned(),
            auth: None,
        };
        if let Some((username, password)) = auth_info(http) {
            let cached = token_cache()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&username)
                .cloned();
            let token = match cached {
                Some(token) => token,
                None => {
                    let token = api.login(&username, &password).await?;
                    token_cache()
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(username, token.clone());
                    token
                }
            };
            api.auth = Some(token);
        }
        Ok(api)
    }

    fn headers(&self) -> Vec<(&'static str, String)> {
        let mut headers = vec![
            ("Referer", format!("{}/", self.root)),
            ("Content-Type", "application/json".into()),
            ("Origin", self.root.clone()),
            ("X-Site", self.root[8..].to_owned()),
        ];
        if let Some(auth) = &self.auth {
            headers.push(("Authorization", auth.clone()));
        }
        headers
    }

    async fn login(&self, username: &str, password: &str) -> crate::Result<String> {
        let headers = self.headers();
        let refs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let data = self
            .http
            .post_json(
                &self.http.api(&format!("{API}/user/login"))?,
                &json!({"email": username, "password": password}),
                &refs,
            )
            .await?;
        let refresh = data
            .get("token")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| error(format!("Iwara login failed: {}", str_of(&data, "message"))))?;
        let bearer = format!("Bearer {refresh}");
        let mut refs = refs;
        refs.push(("Authorization", &bearer));
        let data = self
            .http
            .post_json(
                &self.http.api(&format!("{API}/user/token"))?,
                &json!({}),
                &refs,
            )
            .await?;
        let access = data
            .get("accessToken")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| error(format!("Iwara login failed: {}", str_of(&data, "message"))))?;
        Ok(format!("Bearer {access}"))
    }

    async fn call(&self, endpoint: &str, params: &[(String, String)]) -> crate::Result<Value> {
        let mut url = self.http.api(&format!("{API}{endpoint}"))?;
        if !params.is_empty() {
            url.query_pairs_mut().extend_pairs(params.iter());
        }
        let headers = self.headers();
        let refs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
        self.http.json_with(&url, &refs).await
    }

    /// `_pagination`: `page`/`limit=50` until a short page.
    async fn pages(
        &self,
        endpoint: &str,
        mut params: Vec<(String, String)>,
    ) -> crate::Result<Vec<Value>> {
        params.retain(|(k, _)| k != "page" && k != "limit");
        let mut results = Vec::new();
        for page in 0.. {
            let mut query = params.clone();
            query.push(("page".into(), page.to_string()));
            query.push(("limit".into(), "50".into()));
            let data = self.call(endpoint, &query).await?;
            let list = data["results"].as_array().cloned().unwrap_or_default();
            let short = list.len() < 50;
            if list.is_empty() {
                break;
            }
            results.extend(list);
            if short {
                break;
            }
        }
        Ok(results)
    }

    /// `source`: the signed video source list.
    async fn source(&self, file_url: &str) -> crate::Result<Vec<Value>> {
        let (base, _, query) = crate::sites::gdl::partition(file_url, "?");
        let expires = crate::sites::gdl::extr(query, "expires=", "&");
        if expires.is_empty() {
            return Ok(Vec::new());
        }
        let file_id = base.rsplit('/').next().unwrap_or_default();
        let key = format!("{file_id}_{expires}_mSvL05GfEmeEmsEYfGCnVpEjYgTJraJN");
        let hash: String = sha1::Sha1::digest(key.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let mut headers = self.headers();
        headers.insert(0, ("X-Version", hash));
        let refs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
        Ok(self
            .http
            .json_with(&self.http.api(file_url)?, &refs)
            .await?
            .as_array()
            .cloned()
            .unwrap_or_default())
    }
}

/// `extract_media_info`.
fn media_info(item: &Value, file: Option<&Value>) -> Map<String, Value> {
    let trimmed = |key: &str| {
        item.get(key)
            .and_then(Value::as_str)
            .map_or(String::new(), |t| t.trim().to_owned())
    };
    let mut info = Map::new();
    info.insert("id".into(), item["id"].clone());
    for (key, from) in [
        ("slug", "slug"),
        ("rating", "rating"),
        ("likes", "numLikes"),
        ("views", "numViews"),
        ("comments", "numComments"),
    ] {
        info.insert(key.into(), item.get(from).cloned().unwrap_or(Value::Null));
    }
    let tags: Vec<Value> = item["tags"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|t| t["id"].clone())
        .collect();
    info.insert("tags".into(), tags.into());
    info.insert("title".into(), trimmed("title").into());
    info.insert("description".into(), trimmed("body").into());
    if let Some(file) = file {
        let (filename, _, extension) = crate::sites::gdl::rpartition(str_of(file, "name"), ".");
        info.insert(
            "file_id".into(),
            file.get("id").cloned().unwrap_or(Value::Null),
        );
        info.insert("filename".into(), filename.into());
        info.insert("extension".into(), extension.into());
        info.insert("date".into(), date(parse_iso(str_of(file, "createdAt"))));
        info.insert(
            "date_updated".into(),
            date(parse_iso(str_of(file, "updatedAt"))),
        );
        for key in ["mime", "size", "width", "height", "duration", "type"] {
            info.insert(key.into(), file.get(key).cloned().unwrap_or(Value::Null));
        }
    }
    info
}

/// `extract_user_info`.
fn user_info(profile: &Value) -> Value {
    let user = &profile["user"];
    json!({
        "id": user.get("id"), "name": user.get("username"),
        "nick": str_of(user, "name").trim(), "status": user.get("status"),
        "role": user.get("role"), "premium": user.get("premium"),
        "date": date(parse_iso(str_of(user, "createdAt"))), "description": profile.get("body"),
    })
}

fn formats(http: &Http) -> Option<Vec<String>> {
    match http.option("format")? {
        Value::String(s) => Some(
            s.replace(' ', "")
                .to_lowercase()
                .split(',')
                .map(str::to_owned)
                .collect(),
        ),
        Value::Array(list) => Some(list.iter().map(crate::sites::scalar).collect()),
        Value::Null => None,
        other => Some(vec![crate::sites::scalar(other)]),
    }
}

/// `extract_video_source`: the best source, or the first requested format.
fn pick_source(http: &Http, mut sources: Vec<Value>) -> Option<Value> {
    match formats(http) {
        Some(wanted) => {
            let named: Vec<(String, Value)> = sources
                .into_iter()
                .filter(|s| crate::sites::gdl::truthy(s.get("src")))
                .filter_map(|s| Some((s.get("name")?.as_str()?.to_lowercase(), s)))
                .collect();
            wanted
                .iter()
                .find_map(|w| named.iter().find(|(n, _)| n == w).map(|(_, s)| s.clone()))
        }
        None => {
            let rank = |s: &Value| match s.get("name").and_then(Value::as_str) {
                _ if !crate::sites::gdl::truthy(s.get("src")) => 0,
                Some("Source") => 99999,
                Some(name) => crate::sites::gdl::parse_int(name),
                None => 0,
            };
            sources.sort_by_key(|s| std::cmp::Reverse(rank(s)));
            sources.into_iter().next()
        }
    }
}

async fn image_items(api: &Api<'_>, images: Vec<Value>, user: Option<&Value>) -> Vec<Item> {
    let mut items = Vec::new();
    for image in images {
        let mut image = image.get("image").cloned().unwrap_or(image);
        if !crate::sites::gdl::truthy(image.get("files")) {
            match api
                .call(
                    &format!("/image/{}", crate::sites::scalar(&image["id"])),
                    &[],
                )
                .await
            {
                Ok(full) => image = full,
                Err(_) => continue,
            }
        }
        let mut group = media_info(&image, image.get("file"));
        group.insert(
            "user".into(),
            user.cloned().unwrap_or_else(|| user_info(&image)),
        );
        group.insert("type".into(), "image".into());
        let files = image["files"].as_array().cloned().unwrap_or_default();
        group.insert("count".into(), files.len().into());
        for (index, file) in files.iter().enumerate() {
            let mut fields = media_info(file, Some(file));
            fields.extend(group.clone());
            fields.insert("num".into(), (index + 1).into());
            let file_id = crate::sites::scalar(&file["id"]);
            let ext = crate::sites::gdl::rpartition(str_of(file, "name"), ".")
                .2
                .to_owned();
            if let Ok(url) = Url::parse(&format!(
                "https://i.iwara.tv/image/original/{file_id}/{file_id}.{ext}"
            )) {
                items.push(Item::new(url, fields).ids(crate::sites::scalar(&image["id"]), file_id));
            }
        }
    }
    items
}

async fn video_items(
    api: &Api<'_>,
    http: &Http,
    videos: Vec<Value>,
    user: Option<&Value>,
) -> Vec<Item> {
    let mut items = Vec::new();
    for video in videos {
        let mut video = video.get("video").cloned().unwrap_or(video);
        if video.get("fileUrl").is_none() {
            match api
                .call(
                    &format!("/video/{}", crate::sites::scalar(&video["id"])),
                    &[],
                )
                .await
            {
                Ok(full) => video = full,
                Err(_) => continue,
            }
        }
        let Ok(sources) = api.source(str_of(&video, "fileUrl")).await else {
            continue;
        };
        let Some(source) = pick_source(http, sources) else {
            continue;
        };
        let download = str_of(&source["src"], "download");
        if download.is_empty() {
            continue;
        }
        let mut info = media_info(&video, video.get("file"));
        info.insert(
            "format".into(),
            source.get("name").cloned().unwrap_or(Value::Null),
        );
        info.insert("count".into(), 1.into());
        info.insert("num".into(), 1.into());
        info.insert(
            "user".into(),
            user.cloned().unwrap_or_else(|| user_info(&video)),
        );
        if let Ok(url) = Url::parse(&format!("https:{download}")) {
            let id = crate::sites::scalar(&video["id"]);
            items.push(Item::new(url, info).ids(id.clone(), id));
        }
    }
    items
}

fn queue<'a>(
    http: &'a Http,
    route: &'static Route,
    children: Vec<(Url, Map<String, Value>)>,
) -> Events<'a> {
    futures_util::stream::iter(children)
        .map(move |(url, data)| {
            crate::sites::queue(http, SITE.category, route.subcategory, url, &data)
        })
        .flatten()
        .boxed()
}

fn user_children(
    root: &str,
    users: Vec<Value>,
    key: Option<&str>,
) -> Vec<(Url, Map<String, Value>)> {
    users
        .into_iter()
        .filter_map(|user| {
            let user = match key {
                Some(key) => user.get(key)?.clone(),
                None => user,
            };
            let username = user.get("username")?.as_str()?.to_owned();
            let mut data = user.as_object()?.clone();
            data.insert("type".into(), "user".into());
            Some((
                Url::parse(&format!("{root}/profile/{username}")).ok()?,
                data,
            ))
        })
        .collect()
}

fn params(query: &str) -> Vec<(String, String)> {
    parse_query(query)
        .into_iter()
        .map(|(k, v)| (k, crate::sites::scalar(&v)))
        .collect()
}

fn with_rating(mut params: Vec<(String, String)>) -> Vec<(String, String)> {
    if !params.iter().any(|(k, _)| k == "rating") {
        params.push(("rating".into(), "all".into()));
    }
    params
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some((root, target)) = target(&url) else {
        return crate::sites::fail(error("unsupported Iwara URL"));
    };
    crate::sites::once(async move {
        let api = Api::new(http, &root).await?;
        let post = |route: &'static Route, items: Vec<Item>| -> Events<'_> {
            crate::sites::events(
                items
                    .into_iter()
                    .flat_map(|item| crate::sites::post(&SITE, http, route, vec![item]))
                    .collect(),
            )
        };
        let user_params = |user: &str, query: &str| {
            let user = user.to_owned();
            let query = query.to_owned();
            let api = &api;
            async move {
                let profile = api.call(&format!("/profile/{user}"), &[]).await?;
                let mut params = params(&query);
                params.retain(|(k, _)| k != "user");
                params.push(("user".into(), crate::sites::scalar(&profile["user"]["id"])));
                Ok::<_, crate::Error>((user_info(&profile), params))
            }
        };
        Ok(match target {
            Target::User(user) => {
                let base = format!("{root}/profile/{user}/");
                let views = [
                    ("user-images", format!("{base}images")),
                    ("user-videos", format!("{base}videos")),
                    ("user-playlists", format!("{base}playlists")),
                ];
                let children =
                    crate::sites::gdl::dispatch(http, &views, &["user-images", "user-videos"])?
                        .into_iter()
                        .filter_map(|(_, url)| Some((Url::parse(url).ok()?, Map::new())))
                        .collect();
                queue(http, &USER, children)
            }
            Target::UserImages(user, query) => {
                let (info, params) = user_params(&user, &query).await?;
                let images = api.pages("/images", with_rating(params)).await?;
                post(&USER_IMAGES, image_items(&api, images, Some(&info)).await)
            }
            Target::UserVideos(user, query) => {
                let (info, params) = user_params(&user, &query).await?;
                let videos = api.pages("/videos", with_rating(params)).await?;
                post(
                    &USER_VIDEOS,
                    video_items(&api, http, videos, Some(&info)).await,
                )
            }
            Target::UserPlaylists(user, query) => {
                let (_, params) = user_params(&user, &query).await?;
                let children = api
                    .pages("/playlists", params)
                    .await?
                    .into_iter()
                    .filter_map(|playlist| {
                        let mut data = playlist.as_object()?.clone();
                        data.insert("type".into(), "playlist".into());
                        Some((
                            Url::parse(&format!(
                                "{root}/playlist/{}",
                                crate::sites::scalar(&playlist["id"])
                            ))
                            .ok()?,
                            data,
                        ))
                    })
                    .collect();
                queue(http, &USER_PLAYLISTS, children)
            }
            Target::Following(user) | Target::Followers(user) => {
                let following = matches!(target_kind(&url), Some(true));
                let profile = api.call(&format!("/profile/{user}"), &[]).await?;
                let uid = crate::sites::scalar(&profile["user"]["id"]);
                let (endpoint, key, route) = if following {
                    (format!("/user/{uid}/following"), "user", &FOLLOWING)
                } else {
                    (format!("/user/{uid}/followers"), "follower", &FOLLOWERS)
                };
                let users = api.pages(&endpoint, Vec::new()).await?;
                queue(http, route, user_children(&root, users, Some(key)))
            }
            Target::Image(id) => {
                let image = api.call(&format!("/image/{id}"), &[]).await?;
                post(&IMAGE, image_items(&api, vec![image], None).await)
            }
            Target::Video(id) => {
                let video = api.call(&format!("/video/{id}"), &[]).await?;
                post(&VIDEO, video_items(&api, http, vec![video], None).await)
            }
            Target::Playlist(id) => {
                let videos = api.pages(&format!("/playlist/{id}"), Vec::new()).await?;
                post(&PLAYLIST, video_items(&api, http, videos, None).await)
            }
            Target::Favorite(kind) => {
                if api.auth.is_none() {
                    return Err(error(
                        "Iwara favorites require the username and password options",
                    ));
                }
                // Upstream's default here is the misspelled "vidoo".
                let kind = kind.unwrap_or_else(|| "vidoo".into());
                let results = api
                    .pages(&format!("/favorites/{kind}s"), Vec::new())
                    .await?;
                by_type(&api, http, &root, &FAVORITE, &kind, results).await?
            }
            Target::Search(query) => {
                let params = parse_query(&query);
                let mut kind = params
                    .get("type")
                    .map(crate::sites::scalar)
                    .filter(|t| !t.is_empty())
                    .unwrap_or_else(|| "videos".into());
                if !kind.ends_with('s') {
                    kind.push('s');
                }
                let text = params
                    .get("query")
                    .map(crate::sites::scalar)
                    .unwrap_or_default();
                let results = api
                    .pages(
                        "/search",
                        vec![
                            ("type".into(), kind.clone()),
                            ("query".into(), text.clone()),
                        ],
                    )
                    .await?;
                tag_events(
                    by_type(&api, http, &root, &SEARCH, &kind[..kind.len() - 1], results).await?,
                    text,
                )
            }
            Target::Tag(kind, query) => {
                let params = with_rating(params(&query));
                let tags = params
                    .iter()
                    .find(|(k, _)| k == "tags")
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default();
                let results = api.pages(&format!("/{kind}s"), params).await?;
                tag_events(
                    by_type(&api, http, &root, &TAG, &kind, results).await?,
                    tags,
                )
            }
        })
    })
}

/// Whether a following/followers URL is the `following` one.
fn target_kind(url: &Url) -> Option<bool> {
    let path = url.path();
    path.contains("/following")
        .then_some(true)
        .or_else(|| path.contains("/followers").then_some(false))
}

/// Adds `search_tags` to every file of `events` (as upstream's kwdict).
fn tag_events(events: Events<'_>, tags: String) -> Events<'_> {
    events
        .map(move |event| match event {
            Ok(crate::event::ExtractionEvent::Media(mut media)) => {
                if let Some(keywords) = media
                    .metadata
                    .as_mut()
                    .and_then(|m| m.site_keywords.as_mut())
                {
                    std::sync::Arc::make_mut(keywords)
                        .fields
                        .insert("search_tags".into(), tags.clone().into());
                }
                Ok(crate::event::ExtractionEvent::Media(media))
            }
            other => other,
        })
        .boxed()
}

async fn by_type<'a>(
    api: &Api<'a>,
    http: &'a Http,
    root: &str,
    route: &'static Route,
    kind: &str,
    results: Vec<Value>,
) -> crate::Result<Events<'a>> {
    let items = match kind {
        "image" => image_items(api, results, None).await,
        "video" => video_items(api, http, results, None).await,
        "user" => return Ok(queue(http, route, user_children(root, results, None))),
        other => return Err(error(format!("Unsupported result type '{other}'"))),
    };
    Ok(crate::sites::events(
        items
            .into_iter()
            .flat_map(|item| crate::sites::post(&SITE, http, route, vec![item]))
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http as client, json as reply, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap()).map(|(_, t)| t);
        assert_eq!(
            t("https://www.iwara.ai/profile/goldenapple"),
            Some(Target::User("goldenapple".into()))
        );
        assert_eq!(
            t("https://www.iwara.tv/profile/u/videos"),
            Some(Target::UserVideos("u".into(), String::new()))
        );
        assert_eq!(
            t("https://www.iwara.tv/profile/tyron82/followers"),
            Some(Target::Followers("tyron82".into()))
        );
        assert_eq!(
            t("https://www.iwara.tv/favorites/images"),
            Some(Target::Favorite(Some("image".into())))
        );
        assert_eq!(
            t("https://www.iwara.tv/search?query=x&type=image"),
            Some(Target::Search("query=x&type=image".into()))
        );
        assert_eq!(
            t("https://www.iwara.tv/videos?tags=aether%2Ccitlali"),
            Some(Target::Tag("video".into(), "tags=aether%2Ccitlali".into()))
        );
        assert_eq!(
            t("https://www.iwara.tv/video/6QvQvzZnELJ9vv/bluearchive-rio"),
            Some(Target::Video("6QvQvzZnELJ9vv".into()))
        );
        assert_eq!(
            target(&Url::parse("https://www.iwara.ai/image/x").unwrap())
                .unwrap()
                .0,
            "https://www.iwara.ai"
        );
    }

    #[tokio::test]
    async fn extracts_video_with_signed_source() {
        let video = json!({"id": "V1", "title": " Rio ", "body": "d", "numLikes": 3, "tags": [{"id": "blue"}],
            "fileUrl": "https://files.iwara.tv/file/F1?expires=1700000000&hash=x",
            "file": {"id": "F1", "name": "rio.mp4", "createdAt": "2024-01-02T03:04:05.000Z", "type": "video"},
            "user": {"id": "U1", "username": "maker", "name": " Maker ", "createdAt": "2020-01-01T00:00:00.000Z"}});
        let sources = json!([{"name": "360", "src": {"download": "//files.iwara.tv/d/360.mp4"}},
                             {"name": "Source", "src": {"download": "//files.iwara.tv/d/source.mp4"}}]);
        let (root, server) = serve(vec![reply(video), reply(sources)]);
        let http = client("iwara", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.iwara.tv/video/V1/rio").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].url.as_str(), "https://files.iwara.tv/d/source.mp4");
        let k = keywords(&files[0]);
        assert_eq!(k["title"], "Rio");
        assert_eq!(k["format"], "Source");
        assert_eq!(k["user"]["nick"], "Maker");
        assert_eq!(k["tags"], json!(["blue"]));
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            file.render(&files[0]).unwrap(),
            crate::sites::testing::os_name("2024-01-02 03:04:05 V1 Rio rio.mp4")
        );
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("iwara/maker")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /video/V1 "));
        let expected: String =
            sha1::Sha1::digest(b"F1_1700000000_mSvL05GfEmeEmsEYfGCnVpEjYgTJraJN")
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
        assert!(
            requests[1]
                .to_ascii_lowercase()
                .contains(&format!("x-version: {expected}"))
        );
        assert!(
            requests[0]
                .to_ascii_lowercase()
                .contains("x-site: www.iwara.tv")
        );
    }
}
