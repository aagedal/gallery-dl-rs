//! Itaku galleries, posts, stars, bookmarks and profiles, mirroring
//! gallery_dl/extractor/itaku.py through the public `itaku.ee/api` JSON API.
use super::{date, file_url, nameext_into, parse_iso, str_of, truthy};
use crate::event::ExtractionEvent;
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use futures_util::{StreamExt, stream};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://itaku.ee";
const API: &str = "https://itaku.ee/api";
const HEADERS: &[(&str, &str)] = &[("Accept", "application/json, text/plain, */*")];

pub static SITE: Site = Site {
    category: "itaku",
    name: "Itaku",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[
        &GALLERY, &POSTS, &STARS, &FOLLOWING, &FOLLOWERS, &BOOKMARKS, &USER, &IMAGE, &POST, &SEARCH,
    ],
    api_key: false,
    cookies: None,
};

const FILENAME: &str = "{id}{title:? //}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{owner_username}"];
const POST_FILENAME: &str = "{file[id]}{file[title]:? //}.{extension}";
const POST_DIRECTORY: &[&str] = &[
    "{category}",
    "{owner_username}",
    "Posts",
    "{id}{title:? //}",
];
const POST_ARCHIVE: &str = "{id}_{file[id]}";

static GALLERY: Route = Route::new("gallery", FILENAME, DIRECTORY, "{id}");
static POSTS: Route = Route::new("posts", POST_FILENAME, POST_DIRECTORY, POST_ARCHIVE);
static STARS: Route = Route::new("stars", FILENAME, DIRECTORY, "{id}");
static FOLLOWING: Route = Route::new("following", FILENAME, DIRECTORY, "{id}");
static FOLLOWERS: Route = Route::new("followers", FILENAME, DIRECTORY, "{id}");
static BOOKMARKS: Route = Route::new("bookmarks", FILENAME, DIRECTORY, "{id}");
// `Dispatch` sets the subcategory; formats come from `ItakuExtractor`.
static USER: Route = Route::new("user", FILENAME, DIRECTORY, "{id}");
static IMAGE: Route = Route::new("image", FILENAME, DIRECTORY, "{id}");
static POST: Route = Route::new("post", POST_FILENAME, POST_DIRECTORY, POST_ARCHIVE);
static SEARCH: Route = Route::new("search", FILENAME, DIRECTORY, "{id}");

#[derive(Debug, PartialEq)]
enum Target {
    Gallery(String, Option<String>),
    Posts(String, Option<String>),
    Stars(String, Option<String>),
    Following(String),
    Followers(String),
    Bookmarks(String, bool, String),
    User(String),
    Image(String),
    Post(String),
    Search(String),
}

/// The leading ASCII digits of `text`, as upstream `(\d+)` without an anchor.
fn leading_digits(text: &str) -> Option<&str> {
    let len = text.bytes().take_while(u8::is_ascii_digit).count();
    (len > 0 && len <= 20).then(|| &text[..len])
}

fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) || url.host_str() != Some("itaku.ee") {
        return None;
    }
    let path = url.path();
    let parts: Vec<&str> = path.split('/').skip(1).collect();
    let optional = |parts: &[&str]| {
        parts
            .first()
            .and_then(|s| leading_digits(s))
            .map(str::to_owned)
    };
    match parts.as_slice() {
        ["profile", user, rest @ ..] if super::segment(user) => {
            let user = (*user).to_owned();
            match rest {
                [] | [""] => Some(Target::User(user)),
                ["gallery", more @ ..] => Some(Target::Gallery(user, optional(more))),
                ["posts", more @ ..] => Some(Target::Posts(user, optional(more))),
                ["stars", more @ ..] => Some(Target::Stars(user, optional(more))),
                ["following", ..] => Some(Target::Following(user)),
                ["followers", ..] => Some(Target::Followers(user)),
                ["bookmarks", kind @ ("image" | "user"), folder, ..] => Some(Target::Bookmarks(
                    user,
                    *kind == "user",
                    leading_digits(folder)?.to_owned(),
                )),
                _ => None,
            }
        }
        ["images", id, ..] => Some(Target::Image(leading_digits(id)?.to_owned())),
        ["posts", id, ..] => Some(Target::Post(leading_digits(id)?.to_owned())),
        ["home", "images"] | ["home", "images", ""] => url
            .query()
            .filter(|q| !q.is_empty())
            .map(|q| Target::Search(q.to_owned())),
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Itaku URL"));
    };
    match target {
        Target::Gallery(user, section) => crate::sites::once(async move {
            let owner = user_id(http, &user).await?;
            let mut params = vec![("owner", Param::One(owner))];
            params.push(("sections", section.map_or(Param::None, Param::One)));
            Ok(images(http, &GALLERY, "", params))
        }),
        Target::Stars(user, section) => crate::sites::once(async move {
            let owner = user_id(http, &user).await?;
            let params = vec![
                ("stars_of", Param::One(owner)),
                ("sections", section.map_or(Param::None, Param::One)),
                ("ordering", Param::One("-like_date".into())),
            ];
            Ok(images(http, &STARS, "/user_starred_imgs", params))
        }),
        Target::Posts(user, folder) => crate::sites::once(async move {
            let owner = user_id(http, &user).await?;
            let params = vec![
                ("owner", Param::One(owner)),
                ("folders", folder.map_or(Param::None, Param::One)),
            ];
            Ok(posts(http, params))
        }),
        Target::Following(user) => crate::sites::once(async move {
            let owner = user_id(http, &user).await?;
            Ok(users(http, &FOLLOWING, ("followed_by", owner)))
        }),
        Target::Followers(user) => crate::sites::once(async move {
            let owner = user_id(http, &user).await?;
            Ok(users(http, &FOLLOWERS, ("followers_of", owner)))
        }),
        Target::Bookmarks(_, true, folder) => users(http, &BOOKMARKS, ("bookmark_folder", folder)),
        Target::Bookmarks(_, false, folder) => images(
            http,
            &BOOKMARKS,
            "",
            vec![("bookmark_folder", Param::One(folder))],
        ),
        Target::User(user) => dispatch(http, &user),
        Target::Image(id) => crate::sites::once(async move {
            let image = image(http, &id).await?;
            Ok(crate::sites::events(image_events(http, &IMAGE, &image)))
        }),
        Target::Post(id) => crate::sites::once(async move {
            let url = http.api(&format!("{API}/posts/{id}/"))?;
            let post = http.json_with(&url, HEADERS).await?;
            Ok(crate::sites::events(post_events(http, &POST, &post)))
        }),
        Target::Search(query) => images(http, &SEARCH, "", search_params(&query)),
    }
}

/// Upstream `Dispatch` for profiles: queue the `include` extractors.
fn dispatch<'a>(http: &'a Http, user: &str) -> Events<'a> {
    let base = format!("{ROOT}/profile/{user}/");
    let all = ["gallery", "posts", "followers", "following", "stars"];
    let include: Vec<String> = match http.option("include") {
        Some(Value::String(s)) if s == "all" => all.iter().map(|s| (*s).to_owned()).collect(),
        Some(Value::String(s)) => s.replace(' ', "").split(',').map(str::to_owned).collect(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        Some(value) if !truthy(Some(value)) => Vec::new(),
        _ => vec!["gallery".into()],
    };
    let children: Vec<Url> = include
        .iter()
        .filter(|name| all.contains(&name.as_str()))
        .filter_map(|name| Url::parse(&format!("{base}{name}")).ok())
        .collect();
    stream::iter(children)
        .map(move |child| crate::sites::queue(http, "itaku", "user", child, &Map::new()))
        .flatten()
        .boxed()
}

#[derive(Clone, Debug)]
enum Param {
    None,
    One(String),
    Many(Vec<String>),
}

/// Upstream `_order`: the `order` option, else newest first.
fn order(http: &Http) -> String {
    match http.option_str("order") {
        Some("a" | "asc" | "r" | "reverse") => "date_added".into(),
        Some(order) if !order.is_empty() && order != "d" && order != "desc" => order.into(),
        _ => "-date_added".into(),
    }
}

fn many(values: &[&str]) -> Param {
    Param::Many(values.iter().map(|s| (*s).to_owned()).collect())
}

/// Build an API URL from upstream's parameter dict; later keys replace
/// earlier ones in place, `None` values and empty lists are omitted.
fn api_url(
    http: &Http,
    endpoint: &str,
    base: Vec<(&str, Param)>,
    extra: Vec<(&str, Param)>,
) -> Result<Url> {
    let mut params: Vec<(&str, Param)> = base;
    for (key, value) in extra {
        match params.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = value,
            None => params.push((key, value)),
        }
    }
    let mut url = http.api(&format!("{API}{endpoint}"))?;
    {
        let mut query = url.query_pairs_mut();
        for (key, value) in &params {
            match value {
                Param::None => {}
                Param::One(v) => {
                    query.append_pair(key, v);
                }
                Param::Many(vs) => {
                    for v in vs {
                        query.append_pair(key, v);
                    }
                }
            }
        }
    }
    if url.query() == Some("") {
        url.set_query(None);
    }
    Ok(url)
}

async fn user_id(http: &Http, user: &str) -> Result<String> {
    if let Some(id) = user.strip_prefix("id:") {
        return id
            .parse::<i64>()
            .map(|id| id.to_string())
            .map_err(|_| error("invalid Itaku user ID"));
    }
    let url = http.api(&format!("{API}/user_profiles/{user}/"))?;
    let profile = http.json_with(&url, HEADERS).await?;
    profile
        .get("owner")
        .filter(|v| v.is_number() || v.is_string())
        .map(crate::sites::scalar)
        .ok_or_else(|| error("Itaku profile response has no owner"))
}

async fn image(http: &Http, id: &str) -> Result<Value> {
    let url = http.api(&format!("{API}/galleries/images/{id}/"))?;
    http.json_with(&url, HEADERS).await
}

/// Upstream `_pagination`: follow `links.next`, which must stay on itaku.ee.
fn pagination<'a, F>(http: &'a Http, first: Url, page: F) -> Events<'a>
where
    F: Fn(&Value) -> Events<'a> + Clone + Send + 'a,
{
    crate::sites::paginate(Some(first), move |url: Option<Url>| {
        let page = page.clone();
        async move {
            let Some(url) = url else {
                return Ok(None);
            };
            let data = http.json_with(&url, HEADERS).await?;
            let next = next_page(http, &data)?;
            Ok(Some((page(&data), next)))
        }
    })
}

fn next_page(http: &Http, data: &Value) -> Result<Option<Url>> {
    match data.get("links").and_then(|l| l.get("next")) {
        Some(Value::String(next)) if !next.is_empty() => {
            let next = Url::parse(next).map_err(|_| error("invalid Itaku next page"))?;
            if !crate::sites::plain(&next) || next.host_str() != Some("itaku.ee") {
                return Err(error("Itaku next page left itaku.ee"));
            }
            Ok(Some(http.api(next.as_str())?))
        }
        _ => Ok(None),
    }
}

fn results(data: &Value) -> Vec<Value> {
    data.get("results")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// `galleries_images`: each listed image is re-fetched for full metadata.
fn images<'a>(
    http: &'a Http,
    route: &'static Route,
    path: &str,
    extra: Vec<(&str, Param)>,
) -> Events<'a> {
    let base = vec![
        ("cursor", Param::None),
        ("date_range", Param::One(String::new())),
        ("maturity_rating", many(&["SFW", "Questionable", "NSFW"])),
        ("ordering", Param::One(order(http))),
        ("page", Param::One("1".into())),
        ("page_size", Param::One("30".into())),
        ("visibility", many(&["PUBLIC", "PROFILE_ONLY"])),
    ];
    let first = match api_url(http, &format!("/galleries/images{path}/"), base, extra) {
        Ok(url) => url,
        Err(e) => return crate::sites::fail(e),
    };
    pagination(http, first, move |data| {
        let ids: Vec<String> = results(data)
            .iter()
            .filter_map(|r| r.get("id"))
            .filter(|id| id.is_number() || id.is_string())
            .map(crate::sites::scalar)
            .collect();
        stream::iter(ids)
            .map(move |id| {
                crate::sites::once(async move {
                    let image = image(http, &id).await?;
                    Ok(crate::sites::events(image_events(http, route, &image)))
                })
            })
            .flatten()
            .boxed()
    })
}

fn posts<'a>(http: &'a Http, extra: Vec<(&str, Param)>) -> Events<'a> {
    let base = vec![
        ("cursor", Param::None),
        ("date_range", Param::One(String::new())),
        ("maturity_rating", many(&["SFW", "Questionable", "NSFW"])),
        ("ordering", Param::One(order(http))),
        ("page", Param::One("1".into())),
        ("page_size", Param::One("30".into())),
    ];
    let first = match api_url(http, "/posts/", base, extra) {
        Ok(url) => url,
        Err(e) => return crate::sites::fail(e),
    };
    pagination(http, first, move |data| {
        let events: Vec<ExtractionEvent> = results(data)
            .iter()
            .flat_map(|post| post_events(http, &POSTS, post))
            .collect();
        crate::sites::events(events)
    })
}

/// `user_profiles`: queue each profile, which dispatches to its gallery.
fn users<'a>(http: &'a Http, route: &'static Route, extra: (&str, String)) -> Events<'a> {
    let base = vec![
        ("cursor", Param::None),
        ("ordering", Param::One(order(http))),
        ("page", Param::One("1".into())),
        ("page_size", Param::One("50".into())),
        ("sfw_only", Param::One("false".into())),
    ];
    let first = match api_url(
        http,
        "/user_profiles/",
        base,
        vec![(extra.0, Param::One(extra.1))],
    ) {
        Ok(url) => url,
        Err(e) => return crate::sites::fail(e),
    };
    let subcategory = route.subcategory;
    pagination(http, first, move |data| {
        let children: Vec<(Url, Map<String, Value>)> = results(data)
            .into_iter()
            .filter_map(|user| {
                let name = str_of(&user, "owner_username");
                if !super::segment(name) {
                    return None;
                }
                let url = Url::parse(&format!("{ROOT}/profile/{name}")).ok()?;
                Some((url, user.as_object()?.clone()))
            })
            .collect();
        stream::iter(children)
            .map(move |(url, data)| crate::sites::queue(http, "itaku", subcategory, url, &data))
            .flatten()
            .boxed()
    })
}

fn tag_names(value: Option<&Value>) -> Value {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|t| t.get("name").cloned().unwrap_or(Value::Null))
        .collect::<Vec<_>>()
        .into()
}

/// Upstream `items` for one image: tags, sections, date and file URL.
pub(crate) fn parse_image(image: &Value, videos: bool) -> Option<Item> {
    let mut data = image.as_object()?.clone();
    let created = parse_iso(str_of(image, "date_added"));
    data.insert("date".into(), date(created));
    if let Some(Value::Object(categories)) = data.remove("categorized_tags") {
        for (category, tags) in categories {
            data.insert(
                format!("tags_{}", category.to_lowercase()),
                tag_names(Some(&tags)),
            );
        }
    }
    data.insert("tags".into(), tag_names(image.get("tags")));
    let sections: Vec<Value> = image
        .get("sections")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|s| {
            let title = str_of(s, "title");
            match s.get("group") {
                Some(group) if truthy(Some(group)) => {
                    format!("{}/{title}", str_of(group, "title")).into()
                }
                _ => title.into(),
            }
        })
        .collect();
    data.insert("sections".into(), sections.into());
    let raw = match image.get("video") {
        Some(video) if videos && truthy(Some(video)) => str_of(video, "video"),
        _ => str_of(image, "image"),
    };
    let url = file_url(raw, ROOT)?;
    nameext_into(url.as_str(), &mut data);
    let id = image
        .get("id")
        .map(crate::sites::scalar)
        .unwrap_or_default();
    Some(
        Item::new(url, data)
            .ids(id.clone(), id)
            .user(Some(str_of(image, "owner_username").to_owned()))
            .caption(Some(str_of(image, "title").to_owned()))
            .timestamp(created),
    )
}

fn image_events(http: &Http, route: &'static Route, image: &Value) -> Vec<ExtractionEvent> {
    match parse_image(image, http.option_bool("videos", true)) {
        Some(item) => crate::sites::post(&SITE, http, route, vec![item]),
        None => Vec::new(),
    }
}

/// Upstream `items` for one post: one Directory and its gallery images.
pub(crate) fn parse_post(post: &Value) -> Vec<Item> {
    let Some(fields) = post.as_object() else {
        return Vec::new();
    };
    let mut data = fields.clone();
    let images = match data.remove("gallery_images") {
        Some(Value::Array(images)) => images,
        _ => Vec::new(),
    };
    data.insert("count".into(), images.len().into());
    let created = parse_iso(str_of(post, "date_added"));
    data.insert("date".into(), date(created));
    data.insert("tags".into(), tag_names(post.get("tags")));
    let id = post.get("id").map(crate::sites::scalar).unwrap_or_default();
    let mut items = Vec::new();
    for (num, image) in images.into_iter().enumerate() {
        let Value::Object(mut file) = image else {
            continue;
        };
        let image_date = parse_iso(file.get("date_added").and_then(Value::as_str).unwrap_or(""));
        file.insert("date".into(), date(image_date));
        let raw = file
            .get("image")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let Some(url) = file_url(raw, ROOT) else {
            continue;
        };
        let media_id = file.get("id").map(crate::sites::scalar).unwrap_or_default();
        let mut keywords = data.clone();
        keywords.insert("num".into(), (num + 1).into());
        keywords.insert("file".into(), Value::Object(file));
        nameext_into(url.as_str(), &mut keywords);
        items.push(
            Item::new(url, keywords)
                .ids(id.clone(), media_id)
                .user(Some(str_of(post, "owner_username").to_owned()))
                .caption(Some(str_of(post, "title").to_owned()))
                .timestamp(created),
        );
    }
    items
}

fn post_events(http: &Http, route: &'static Route, post: &Value) -> Vec<ExtractionEvent> {
    crate::sites::post(&SITE, http, route, parse_post(post))
}

/// Upstream search: `tags` split into required, `-negative` and `~optional`.
fn search_params(query: &str) -> Vec<(&'static str, Param)> {
    let (mut required, mut negative, mut optional) = (Vec::new(), Vec::new(), Vec::new());
    for pair in query.split('&') {
        let Some((name, value)) = pair.split_once('=') else {
            continue;
        };
        if crate::sites::percent_decode(&name.replace('+', " ")) != "tags" {
            continue;
        }
        let tag = crate::sites::percent_decode(&value.replace('+', " "));
        if let Some(tag) = tag.strip_prefix('-') {
            negative.push(tag.to_owned());
        } else if let Some(tag) = tag.strip_prefix('~') {
            optional.push(tag.to_owned());
        } else if !tag.is_empty() {
            required.push(tag);
        }
    }
    vec![
        ("required_tags", Param::Many(required)),
        ("negative_tags", Param::Many(negative)),
        ("optional_tags", Param::Many(optional)),
    ]
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
            t("https://itaku.ee/profile/alice/gallery"),
            Some(Target::Gallery("alice".into(), None))
        );
        assert_eq!(
            t("https://itaku.ee/profile/alice/gallery/7"),
            Some(Target::Gallery("alice".into(), Some("7".into())))
        );
        assert_eq!(
            t("https://itaku.ee/profile/alice/posts/3"),
            Some(Target::Posts("alice".into(), Some("3".into())))
        );
        assert_eq!(
            t("https://itaku.ee/profile/alice/stars"),
            Some(Target::Stars("alice".into(), None))
        );
        assert_eq!(
            t("https://itaku.ee/profile/alice/following"),
            Some(Target::Following("alice".into()))
        );
        assert_eq!(
            t("https://itaku.ee/profile/alice/followers"),
            Some(Target::Followers("alice".into()))
        );
        assert_eq!(
            t("https://itaku.ee/profile/alice/bookmarks/user/9"),
            Some(Target::Bookmarks("alice".into(), true, "9".into()))
        );
        assert_eq!(
            t("https://itaku.ee/profile/alice"),
            Some(Target::User("alice".into()))
        );
        assert_eq!(
            t("https://itaku.ee/profile/alice/?x=1"),
            Some(Target::User("alice".into()))
        );
        assert_eq!(
            t("https://itaku.ee/images/12345"),
            Some(Target::Image("12345".into()))
        );
        assert_eq!(
            t("https://itaku.ee/posts/12345"),
            Some(Target::Post("12345".into()))
        );
        assert_eq!(
            t("https://itaku.ee/home/images?tags=a&tags=-b"),
            Some(Target::Search("tags=a&tags=-b".into()))
        );
        for raw in [
            "https://itaku.ee/home/images",
            "https://itaku.ee/images/abc",
            "https://itaku.ee/profile/alice/bookmarks/post/1",
            "https://itaku.ee/profile/alice/unknown",
            "https://www.itaku.ee/images/1",
            "https://itaku.ee.evil.com/images/1",
            "https://evilitaku.ee/images/1",
            "https://itaku.ee:8443/images/1",
            "https://u:p@itaku.ee/images/1",
            "ftp://itaku.ee/images/1",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[test]
    fn search_tags_split() {
        let params = search_params("tags=a+b&tags=-c&tags=~d&tags=&maturity_rating=SFW");
        let lists: Vec<_> = params
            .iter()
            .map(|(k, v)| match v {
                Param::Many(v) => (*k, v.clone()),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(lists[0], ("required_tags", vec!["a b".to_owned()]));
        assert_eq!(lists[1], ("negative_tags", vec!["c".to_owned()]));
        assert_eq!(lists[2], ("optional_tags", vec!["d".to_owned()]));
    }

    fn image_json(id: u64) -> Value {
        json!({"id": id, "title": format!("Pic {id}"), "owner_username": "alice",
            "date_added": "2024-01-02T03:04:05.123456Z",
            "image": format!("https://itaku.ee/api/media/gallery_imgs/pic_{id}.png"),
            "video": null,
            "tags": [{"name": "cat"}, {"name": "dog"}],
            "categorized_tags": {"GENERAL": [{"name": "cat"}], "Character": [{"name": "dog"}]},
            "sections": [{"title": "Sketches", "group": null},
                         {"title": "Inks", "group": {"title": "Art"}}]})
    }

    #[test]
    fn parses_images() {
        let item = parse_image(&image_json(5), true).unwrap();
        let k = &item.keywords;
        assert_eq!(k["date"], "2024-01-02 03:04:05");
        assert_eq!(k["tags"], json!(["cat", "dog"]));
        assert_eq!(k["tags_general"], json!(["cat"]));
        assert_eq!(k["tags_character"], json!(["dog"]));
        assert_eq!(k["sections"], json!(["Sketches", "Art/Inks"]));
        assert!(!k.contains_key("categorized_tags"));
        assert_eq!(k["filename"], "pic_5");
        let mut video = image_json(6);
        video["video"] = json!({"video": "https://itaku.ee/api/media/v/clip.mp4"});
        assert_eq!(
            parse_image(&video, true).unwrap().url.as_str(),
            "https://itaku.ee/api/media/v/clip.mp4"
        );
        assert_eq!(
            parse_image(&video, false).unwrap().keywords["extension"],
            "png"
        );
    }

    #[tokio::test]
    async fn gallery_resolves_user_and_paginates() {
        let (root, server) = serve(vec![
            json(json!({"owner": 42})),
            json(json!({"results": [{"id": 1}],
                "links": {"next": "https://itaku.ee/api/galleries/images/?page=2"}})),
            json(image_json(1)),
            json(json!({"results": [{"id": 2}], "links": {"next": null}})),
            json(image_json(2)),
        ]);
        let http = http("itaku", root);
        let files = media(extract(
            &http,
            Url::parse("https://itaku.ee/profile/alice/gallery/7").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "1 Pic 1.png");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("itaku/alice")
        );
        let (_, identity) = crate::sites::archive_key(&files[1]).unwrap();
        assert_eq!(identity, r#"["{id}",["2"]]"#);
        assert_eq!(keywords(&files[0])["subcategory"], "gallery");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/user_profiles/alice/ "));
        assert!(
            requests[1].starts_with(
                "GET /api/galleries/images/?date_range=&maturity_rating=SFW\
                 &maturity_rating=Questionable&maturity_rating=NSFW&ordering=-date_added\
                 &page=1&page_size=30&visibility=PUBLIC&visibility=PROFILE_ONLY\
                 &owner=42&sections=7 "
            ),
            "{}",
            requests[1]
        );
        assert!(requests[1].contains("accept: application/json, text/plain, */*"));
        assert!(requests[2].starts_with("GET /api/galleries/images/1/ "));
        assert!(requests[3].starts_with("GET /api/galleries/images/?page=2 "));
    }

    #[tokio::test]
    async fn post_files_carry_file_metadata() {
        let post = json!({"id": 77, "title": "Set", "owner_username": "alice",
            "date_added": "2024-01-02T03:04:05Z", "tags": [{"name": "x"}],
            "gallery_images": [
                {"id": 10, "title": "One", "date_added": "2024-01-01T00:00:00Z",
                 "image": "https://itaku.ee/api/media/a.jpg"},
                {"id": 11, "title": "", "date_added": "2024-01-01T00:00:01Z",
                 "image": "https://itaku.ee/api/media/b.jpg"}]});
        let (root, server) = serve(vec![json(post)]);
        let http = http("itaku", root);
        let files = media(extract(
            &http,
            Url::parse("https://itaku.ee/posts/77").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[1]);
        assert_eq!(k["count"], 2);
        assert_eq!(k["num"], 2);
        assert_eq!(k["file"]["date"], "2024-01-01 00:00:01");
        assert_eq!(k["tags"], json!(["x"]));
        assert!(!k.contains_key("gallery_images"));
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "10 One.jpg");
        assert_eq!(file.render(&files[1]).unwrap(), "11.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("itaku/alice/Posts/77 Set")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{id}_{file[id]}",["77","10"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/posts/77/ "));
    }

    #[tokio::test]
    async fn followers_queue_profiles_which_dispatch_to_gallery() {
        let (root, server) = serve(vec![
            json(json!({"results": [{"owner_username": "bob", "owner": 8}],
                "links": {}})),
            json(json!({"owner": 8})),
            json(json!({"results": [{"id": 3}], "links": {}})),
            json(image_json(3)),
        ]);
        let http = http("itaku", root);
        let files = media(extract(
            &http,
            Url::parse("https://itaku.ee/profile/id:5/followers").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with(
                "GET /api/user_profiles/?ordering=-date_added&page=1&page_size=50\
                 &sfw_only=false&followers_of=5 "
            ),
            "{}",
            requests[0]
        );
        assert!(requests[1].starts_with("GET /api/user_profiles/bob/ "));
        assert!(requests[2].contains("&owner=8 "));
    }

    #[tokio::test]
    async fn rejects_foreign_next_page() {
        let (root, _server) = serve(vec![json(json!({"results": [],
            "links": {"next": "https://evil.test/api/x"}}))]);
        let http = http("itaku", root);
        let events: Vec<_> = extract(
            &http,
            Url::parse("https://itaku.ee/profile/alice/bookmarks/image/4").unwrap(),
        )
        .collect()
        .await;
        assert!(events.iter().any(|e| e.is_err()));
    }
}
