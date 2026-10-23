//! Behance galleries, user profiles and collections, mirroring
//! gallery_dl/extractor/behance.py: gallery pages embed their project state
//! as JSON, while profiles and moodboards page through the GraphQL API.
use crate::sites::art::{ext_from_url, extr, str_of, truthy};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value, json};
use url::Url;

pub static SITE: Site = Site {
    category: "behance",
    name: "Behance",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&GALLERY, &USER, &COLLECTION],
    api_key: false,
    cookies: None,
};

static GALLERY: Route = Route::new(
    "gallery",
    "{category}_{id}_{num:>02}.{extension}",
    // Upstream `{owners:J, }`; the local parser needs the `/` terminator
    // that Python's `partition` treats as optional. Renders identically.
    &["{category}", "{owners:J, /}", "{id} {name}"],
    "{id}_{num}",
);
static USER: Route = Route::new("user", "{filename}.{extension}", &["{category}"], "");
static COLLECTION: Route = Route::new("collection", "{filename}.{extension}", &["{category}"], "");

const ROOT: &str = "https://www.behance.net";
/// Upstream's fallback `bcp` cookie, also sent as `X-BCP`.
const BCP: &str = "4c34489d-914c-46cd-b44c-dfd0e661136d";
const GALLERY_COOKIES: &str = "gk_suid=14118261; gki=feature_3_in_1_checkout_test:false,hire_browse_get_quote_cta_ab_test:false,feature_hire_dashboard_services_ab_test:false,feature_show_details_jobs_row_ab_test:false,feature_ai_freelance_project_create_flow:false,; ilo0=true; originalReferrer=";
const PROFILE_QUERY: &str = include_str!("behance/GetProfileProjects.graphql");
const MOODBOARD_QUERY: &str = include_str!("behance/GetMoodboardItemsAndRecommendations.graphql");

#[derive(Debug, PartialEq)]
enum Target {
    Gallery(String),
    User(String),
    Collection(String),
}

/// The leading digits of a path segment, as `(\d+)` without an anchor.
fn leading_digits(segment: &str) -> Option<String> {
    let end = segment
        .bytes()
        .position(|b| !b.is_ascii_digit())
        .unwrap_or(segment.len());
    (1..=20).contains(&end).then(|| segment[..end].to_owned())
}

fn target(url: &Url) -> Option<Target> {
    if !crate::sites::art::on_host(url, &["behance.net"]) {
        return None;
    }
    let parts = crate::sites::art::raw_segments(url);
    match parts.as_slice() {
        ["gallery", id, ..] => leading_digits(id).map(Target::Gallery),
        ["collection", id, ..] => leading_digits(id).map(Target::Collection),
        // `([^/?#]+)/?$`: one segment, optional slash, no query or fragment.
        [user]
            if url.query().is_none()
                && url.fragment().is_none()
                && matches!(url.path().strip_prefix('/'), Some(p) if p == *user || p == format!("{user}/")) =>
        {
            Some(Target::User(crate::sites::percent_decode(user)))
        }
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Gallery(id)) => crate::sites::once(async move { gallery(http, &id).await }),
        Some(Target::User(user)) => listing(
            http,
            &USER,
            "GetProfileProjects",
            json!({"username": user, "after": "MAo="}),
        ),
        Some(Target::Collection(id)) => {
            let Ok(id) = id.parse::<u64>() else {
                return crate::sites::fail(error("invalid Behance collection ID"));
            };
            listing(
                http,
                &COLLECTION,
                "GetMoodboardItemsAndRecommendations",
                json!({
                    "afterItem": "MAo=",
                    "firstItem": 40,
                    "id": id,
                    "shouldGetItems": true,
                    "shouldGetMoodboardFields": false,
                    "shouldGetRecommendations": false,
                }),
            )
        }
        None => crate::sites::fail(error("unsupported Behance URL")),
    }
}

/// Upstream `_update`: flatten fields, owners and tags, and add aliases.
pub(crate) fn update(data: &mut Map<String, Value>) {
    if let Some(Value::Array(fields)) = data.get("fields")
        && fields.first().is_some_and(Value::is_object)
    {
        let names = fields
            .iter()
            .map(|f| {
                f.get("name")
                    .filter(|v| truthy(Some(v)))
                    .or_else(|| f.get("label"))
                    .cloned()
                    .unwrap_or(Value::Null)
            })
            .collect();
        data.insert("fields".into(), Value::Array(names));
    }
    let owners: Vec<Value> = data
        .get("owners")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|owner| {
            owner
                .get("display_name")
                .filter(|v| truthy(Some(v)))
                .or_else(|| owner.get("displayName"))
                .cloned()
                .unwrap_or(Value::Null)
        })
        .collect();
    let user = owners
        .iter()
        .map(crate::sites::scalar)
        .collect::<Vec<_>>()
        .join(", ");
    data.insert("owners".into(), Value::Array(owners));

    let tags = match data.get("tags") {
        Some(Value::Array(tags)) if tags.first().is_some_and(Value::is_object) => Value::Array(
            tags.iter()
                .map(|tag| tag.get("title").cloned().unwrap_or(Value::Null))
                .collect(),
        ),
        Some(tags) if truthy(Some(tags)) => tags.clone(),
        _ => Value::Array(Vec::new()),
    };
    data.insert("tags".into(), tags);

    let published = ["publishedOn", "conceived_on"]
        .iter()
        .find_map(|key| data.get(*key).filter(|v| truthy(Some(v))))
        .cloned()
        .unwrap_or(Value::from(0));
    data.insert(
        "date".into(),
        crate::sites::art::date(crate::sites::art::parse_ts(&published)),
    );

    if let Some(creator) = data.get_mut("creator").filter(|c| truthy(Some(c)))
        && let Some(creator) = creator.as_object_mut()
    {
        let name = creator
            .get("url")
            .and_then(Value::as_str)
            .map(|url| url.rsplit('/').next().unwrap_or_default().to_owned())
            .unwrap_or_default();
        creator.insert("name".into(), name.into());
    }

    let id = data.get("id").cloned().unwrap_or(Value::Null);
    let name = data.get("name").cloned().unwrap_or(Value::Null);
    data.insert("gallery_id".into(), id);
    data.insert("title".into(), name);
    data.insert("user".into(), user.into());
}

/// Module types to download: the `modules` option or upstream's default.
fn modules(http: &Http) -> Vec<String> {
    let configured: Vec<String> = match http.option("modules") {
        Some(Value::String(list)) => list.split(',').map(str::to_owned).collect(),
        Some(Value::Array(list)) => list.iter().map(crate::sites::scalar).collect(),
        _ => Vec::new(),
    };
    if configured.is_empty() {
        ["image", "video", "mediacollection", "embed"]
            .map(str::to_owned)
            .to_vec()
    } else {
        configured
    }
}

/// Parse the gallery page's `beconfig-store_state` project.
pub(crate) fn parse_gallery_page(page: &str) -> Result<Map<String, Value>> {
    let state = crate::sites::art::json_loads(
        extr(page, "id=\"beconfig-store_state\">", "</script>"),
        "Behance",
    )?;
    let mut data = state
        .pointer("/project/project")
        .and_then(Value::as_object)
        .cloned()
        .ok_or_else(|| error("Behance page contains no project"))?;
    update(&mut data);
    Ok(data)
}

/// The `/size/` component of an image size URL, as `rsplit("/", 2)[1]`.
fn size_key(url: &str) -> &str {
    let mut parts = url.rsplitn(3, '/');
    parts.next();
    parts.next().unwrap_or_default()
}

/// One downloadable module: its URL and module object.
type Entry = (String, Map<String, Value>);

/// Upstream `get_images` for everything but video modules, which need an
/// extra request; those are returned with an empty URL for [`resolve_video`].
pub(crate) fn images(data: &Map<String, Value>, enabled: &[String]) -> Result<Vec<Entry>> {
    let modules = data.get("modules").and_then(Value::as_array);
    let Some(modules) = modules.filter(|m| !m.is_empty()) else {
        return match data.get("matureAccess").and_then(Value::as_str) {
            Some("logged-out") => Err(error(
                "Behance: mature content galleries require logged-in cookies",
            )),
            Some("restricted-safe") => {
                Err(error("Behance: mature content blocked in account settings"))
            }
            Some(access) if !access.is_empty() && access != "allowed" => {
                Err(error("Behance refused access to this gallery"))
            }
            _ => Ok(Vec::new()),
        };
    };
    let mut results = Vec::new();
    for module in modules {
        let Some(module) = module.as_object() else {
            continue;
        };
        let typename = module
            .get("__typename")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let kind = typename
            .get(..typename.len().saturating_sub(6))
            .unwrap_or_default()
            .to_lowercase();
        if !enabled.contains(&kind) {
            continue;
        }
        match kind.as_str() {
            "image" => {
                let sizes: Vec<&Value> = module
                    .get("imageSizes")
                    .and_then(|s| s.get("allAvailable"))
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .collect();
                let pick = ["source", "max_3840", "fs", "hd", "disp"]
                    .iter()
                    .find_map(|key| {
                        // Later entries with the same key win, as in a dict.
                        sizes
                            .iter()
                            .rev()
                            .find(|size| size_key(str_of(size, "url")) == *key)
                    });
                if let Some(size) = pick {
                    results.push((str_of(size, "url").to_owned(), module.clone()));
                }
            }
            "video" => results.push((String::new(), module.clone())),
            "mediacollection" => {
                for component in module
                    .get("components")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let sizes = component.get("imageSizes").and_then(Value::as_object);
                    if let Some(size) = sizes
                        .into_iter()
                        .flat_map(|s| s.values())
                        .find(|size| truthy(Some(size)))
                    {
                        let mut parts: Vec<&str> = str_of(size, "url").split('/').collect();
                        if parts.len() > 4 {
                            parts[4] = "source";
                            results.push((parts.join("/"), module.clone()));
                        }
                    }
                }
            }
            // `embed` modules and m3u8 videos are ytdl: URLs upstream, and
            // `text` modules are text: URLs; neither is a direct download.
            _ => {}
        }
    }
    Ok(results)
}

/// The non-HLS URL of a video module: the embed page's `<source>`, else the
/// last non-m3u8 rendition. `None` when only HLS (ytdl) URLs exist.
async fn resolve_video(http: &Http, module: &Map<String, Value>) -> Option<String> {
    let embed = module.get("embed").and_then(Value::as_str).unwrap_or("");
    let src = crate::sites::unescape(extr(embed, "src=\"", "\""));
    if !src.is_empty()
        && let Ok(url) = http.api(&src)
        && let Ok(page) = http.text(&url).await
    {
        let source = extr(&page, "<source src=\"", "\"");
        if !source.is_empty() {
            return (ext_from_url(source) != "m3u8").then(|| source.to_owned());
        }
    }
    module
        .get("videoData")?
        .get("renditions")
        .and_then(Value::as_array)?
        .iter()
        .map(|r| str_of(r, "url"))
        .rfind(|url| !url.is_empty() && ext_from_url(url) != "m3u8")
        .map(str::to_owned)
}

/// Build the gallery's files from resolved `(url, module)` entries.
pub(crate) fn gallery_items(data: &Map<String, Value>, entries: Vec<Entry>) -> Vec<Item> {
    let count = entries.len();
    let id = data.get("id").map(crate::sites::scalar).unwrap_or_default();
    let user = data.get("user").and_then(Value::as_str).map(str::to_owned);
    let name = data.get("name").and_then(Value::as_str).map(str::to_owned);
    let timestamp = data
        .get("date")
        .and_then(Value::as_str)
        .and_then(|d| crate::sites::art::strptime(d, "%Y-%m-%d %H:%M:%S"));
    let mut items = Vec::new();
    for (num, (raw, module)) in entries.into_iter().enumerate() {
        let num = num + 1;
        let Some(url) = crate::sites::art::file_url(&raw, ROOT) else {
            continue;
        };
        let mut keywords = data.clone();
        keywords.insert("count".into(), count.into());
        keywords.insert("num".into(), num.into());
        let extension = module
            .get("extension")
            .filter(|v| truthy(Some(v)))
            .cloned()
            .unwrap_or_else(|| ext_from_url(&raw).into());
        keywords.insert("module".into(), Value::Object(module));
        keywords.insert("extension".into(), extension);
        items.push(
            Item::new(url, keywords)
                .ids(id.clone(), num.to_string())
                .user(user.clone())
                .caption(name.clone())
                .timestamp(timestamp),
        );
    }
    items
}

async fn gallery<'a>(http: &'a Http, id: &str) -> Result<Events<'a>> {
    let url = http.api(&format!("{ROOT}/gallery/{id}/a"))?;
    let cookie = format!("{GALLERY_COOKIES}; bcp={BCP}");
    let page = http.text_with(&url, &[("Cookie", &cookie)]).await?;
    let data = parse_gallery_page(&page)?;
    let mut entries = Vec::new();
    for (url, module) in images(&data, &modules(http))? {
        if url.is_empty() {
            if let Some(url) = resolve_video(http, &module).await {
                entries.push((url, module));
            }
        } else {
            entries.push((url, module));
        }
    }
    let items = gallery_items(&data, entries);
    Ok(crate::sites::events(crate::sites::post(
        &SITE, http, &GALLERY, items,
    )))
}

async fn graphql(http: &Http, endpoint: &str, variables: &Value) -> Result<Value> {
    let query = match endpoint {
        "GetProfileProjects" => PROFILE_QUERY,
        _ => MOODBOARD_QUERY,
    };
    let url = http.api(&format!("{ROOT}/v3/graphql"))?;
    let cookie = format!("bcp={BCP}");
    let mut body = http
        .post_json(
            &url,
            &json!({"query": query, "variables": variables}),
            &[
                ("Origin", ROOT),
                ("X-BCP", BCP),
                ("X-Requested-With", "XMLHttpRequest"),
                ("Cookie", &cookie),
            ],
        )
        .await?;
    body.get_mut("data")
        .map(Value::take)
        .ok_or_else(|| error("Behance GraphQL response contains no data"))
}

/// Gallery nodes and the next cursor of one GraphQL page.
pub(crate) fn listing_page(route: &Route, data: &Value) -> Result<(Vec<Value>, Option<String>)> {
    let items = match route.subcategory {
        "user" => data.pointer("/user/profileProjects"),
        _ => data.pointer("/moodboard/items"),
    }
    .ok_or_else(|| error("Behance GraphQL response contains no items"))?;
    let nodes = items
        .get("nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|node| match route.subcategory {
            "user" => Some(node.clone()),
            _ => node.get("entity").cloned(),
        })
        .collect();
    let next = items
        .pointer("/pageInfo/hasNextPage")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        .then(|| {
            items
                .pointer("/pageInfo/endCursor")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .flatten();
    Ok((nodes, next))
}

fn listing<'a>(
    http: &'a Http,
    route: &'static Route,
    endpoint: &'static str,
    variables: Value,
) -> Events<'a> {
    let cursor_key = if route.subcategory == "user" {
        "after"
    } else {
        "afterItem"
    };
    crate::sites::paginate(Some(variables), move |variables| async move {
        let Some(mut variables) = variables else {
            return Ok(None);
        };
        let data = graphql(http, endpoint, &variables).await?;
        let (nodes, next) = listing_page(route, &data)?;
        let mut children = Vec::new();
        for node in nodes {
            let Some(mut gallery) = node.as_object().cloned() else {
                continue;
            };
            let Some(child) = gallery
                .get("url")
                .and_then(Value::as_str)
                .and_then(|u| Url::parse(u).ok())
                .filter(|u| matches!(target(u), Some(Target::Gallery(_))))
            else {
                continue;
            };
            update(&mut gallery);
            children.push((child, gallery));
        }
        let page = futures_util::StreamExt::boxed(futures_util::StreamExt::flatten(
            futures_util::stream::iter(children.into_iter().map(move |(child, data)| {
                crate::sites::queue(http, "behance", route.subcategory, child, &data)
            })),
        ));
        let next = next.map(|cursor| {
            variables[cursor_key] = cursor.into();
            variables
        });
        Ok(Some((page, next)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, json, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.behance.net/gallery/88276087/Audi-R8-RWD"),
            Some(Target::Gallery("88276087".into()))
        );
        assert_eq!(
            t("https://behance.net/gallery/123abc"),
            Some(Target::Gallery("123".into()))
        );
        assert_eq!(
            t("https://www.behance.net/alexstrohl"),
            Some(Target::User("alexstrohl".into()))
        );
        assert_eq!(
            t("https://www.behance.net/alexstrohl/"),
            Some(Target::User("alexstrohl".into()))
        );
        assert_eq!(
            t("https://www.behance.net/collection/71340149/inspiration"),
            Some(Target::Collection("71340149".into()))
        );
        for raw in [
            "https://www.behance.net/",
            "https://www.behance.net/user/projects",
            "https://www.behance.net/user?tab=1",
            "https://www.behance.net/gallery/abc",
            "https://www.behance.net.evil.test/gallery/1",
            "https://evilbehance.net/gallery/1",
            "https://cdn.behance.net/gallery/1",
            "https://www.behance.net:8443/gallery/1",
            "https://u:p@www.behance.net/gallery/1",
            "ftp://www.behance.net/gallery/1",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn project() -> Value {
        json!({
            "id": 88276087, "name": "Audi R8", "publishedOn": 1577934245,
            "owners": [{"displayName": "Jane"}, {"display_name": "Joe"}],
            "fields": [{"name": "Photography"}, {"label": "Design"}],
            "tags": [{"title": "car"}, {"title": "audi"}],
            "creator": {"url": "https://www.behance.net/jane"},
            "modules": [
                {"__typename": "ImageModule", "id": 1, "imageSizes": {"allAvailable": [
                    {"url": "https://mir-s3-cdn-cf.behance.net/project_modules/disp/a.jpg"},
                    {"url": "https://mir-s3-cdn-cf.behance.net/project_modules/max_3840/a.jpg"},
                ]}},
                {"__typename": "TextModule", "text": "hello"},
                {"__typename": "MediaCollectionModule", "components": [
                    {"imageSizes": {"size_original": null,
                        "size_disp": {"url": "https://mir-s3-cdn-cf.behance.net/project_modules/disp/b.png"}}},
                ]},
                {"__typename": "EmbedModule", "originalEmbed": "<iframe src=\"https://youtube.com/x\">"},
                {"__typename": "VideoModule", "id": 3, "embed": "",
                 "videoData": {"renditions": [
                     {"url": "https://v.behance.net/v/a.mp4"},
                     {"url": "https://v.behance.net/v/a.m3u8"}]}},
            ],
        })
    }

    fn page(project: &Value) -> String {
        format!(
            "<html><script type=\"application/json\" id=\"beconfig-store_state\">{}</script></html>",
            json!({"project": {"project": project}})
        )
    }

    #[test]
    fn updates_like_upstream() {
        let mut data = project().as_object().unwrap().clone();
        update(&mut data);
        assert_eq!(data["owners"], json!(["Jane", "Joe"]));
        assert_eq!(data["user"], "Jane, Joe");
        assert_eq!(data["fields"], json!(["Photography", "Design"]));
        assert_eq!(data["tags"], json!(["car", "audi"]));
        assert_eq!(data["date"], "2020-01-02 03:04:05");
        assert_eq!(data["creator"]["name"], "jane");
        assert_eq!(data["gallery_id"], 88276087);
        assert_eq!(data["title"], "Audi R8");
        let mut empty = json!({"id": 1, "name": "x", "owners": []})
            .as_object()
            .unwrap()
            .clone();
        update(&mut empty);
        assert_eq!(empty["date"], "1970-01-01 00:00:00");
        assert_eq!(empty["tags"], json!([]));
    }

    #[test]
    fn mature_galleries_need_login() {
        let data = json!({"modules": [], "matureAccess": "logged-out"});
        let err = images(data.as_object().unwrap(), &modules_default()).unwrap_err();
        assert!(err.to_string().contains("logged-in"));
        let data = json!({"modules": []});
        assert!(
            images(data.as_object().unwrap(), &modules_default())
                .unwrap()
                .is_empty()
        );
    }

    fn modules_default() -> Vec<String> {
        ["image", "video", "mediacollection", "embed"]
            .map(str::to_owned)
            .to_vec()
    }

    #[tokio::test]
    async fn extracts_gallery() {
        let (root, server) = serve(vec![html(&page(&project()))]);
        let http = http("Behance", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.behance.net/gallery/88276087/Audi-R8").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 3);
        assert_eq!(
            files[0].url.as_str(),
            "https://mir-s3-cdn-cf.behance.net/project_modules/max_3840/a.jpg"
        );
        assert_eq!(
            files[1].url.as_str(),
            "https://mir-s3-cdn-cf.behance.net/project_modules/source/b.png"
        );
        assert_eq!(files[2].url.as_str(), "https://v.behance.net/v/a.mp4");
        let k = keywords(&files[0]);
        assert_eq!(k["num"], 1);
        assert_eq!(k["count"], 3);
        assert_eq!(k["extension"], "jpg");
        assert_eq!(k["module"]["id"], 1);
        assert_eq!(k["subcategory"], "gallery");
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "behance_88276087_02.png");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("behance/Jane, Joe/88276087 Audi R8")
        );
        let (namespace, identity) = crate::sites::archive_key(&files[1]).unwrap();
        assert_eq!(namespace, "behance");
        assert_eq!(identity, r#"["{id}_{num}",["88276087","2"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /gallery/88276087/a "));
        assert!(requests[0].contains("bcp=4c34489d"));
        assert!(requests[0].contains("ilo0=true"));
    }

    #[tokio::test]
    async fn video_embed_source_is_preferred() {
        let project = json!({"id": 5, "name": "V", "owners": [],
            "modules": [{"__typename": "VideoModule",
                "embed": "<iframe src=\"https://www.behance.net/videos/embed?a=1&amp;b=2\">"}]});
        let (root, server) = serve(vec![
            html(&page(&project)),
            html("<video><source src=\"https://v.behance.net/x/video.mp4\"></video>"),
        ]);
        let http = http("Behance", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.behance.net/gallery/5/V").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].url.as_str(), "https://v.behance.net/x/video.mp4");
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /videos/embed?a=1&b=2 "));
    }

    #[tokio::test]
    async fn user_pages_queue_galleries() {
        let node = |id: u64| {
            json!({"id": id, "name": format!("G{id}"), "owners": [{"displayName": "Jane"}],
                "url": format!("https://www.behance.net/gallery/{id}/G"), "publishedOn": 1})
        };
        let (root, server) = serve(vec![
            json(json!({"data": {"user": {"profileProjects": {
                "pageInfo": {"hasNextPage": true, "endCursor": "Mgo="},
                "nodes": [node(1)]}}}})),
            html(&page(
                &json!({"id": 1, "name": "G1", "owners": [{"displayName": "Jane"}],
                "modules": [{"__typename": "ImageModule", "imageSizes": {"allAvailable": [
                    {"url": "https://mir-s3-cdn-cf.behance.net/project_modules/source/1.jpg"}]}}]}),
            )),
            json(json!({"data": {"user": {"profileProjects": {
                "pageInfo": {"hasNextPage": false, "endCursor": null},
                "nodes": []}}}})),
        ]);
        let http = http("Behance", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.behance.net/jane").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(keywords(&files[0])["name"], "G1");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("POST /v3/graphql "));
        assert!(requests[0].contains("x-bcp: 4c34489d") || requests[0].contains("X-BCP"));
        assert!(requests[0].contains(r#""username":"jane""#));
        assert!(requests[0].contains(r#""after":"MAo=""#));
        assert!(requests[0].contains("query GetProfileProjects"));
        assert!(requests[2].contains(r#""after":"Mgo=""#));
    }

    #[test]
    fn collection_pages_use_entities() {
        let data = json!({"moodboard": {"items": {
            "pageInfo": {"hasNextPage": true, "endCursor": "abc"},
            "nodes": [{"entity": {"id": 7}}, {"other": 1}]}}});
        let (nodes, next) = listing_page(&COLLECTION, &data).unwrap();
        assert_eq!(nodes, vec![json!({"id": 7})]);
        assert_eq!(next.as_deref(), Some("abc"));
    }
}
