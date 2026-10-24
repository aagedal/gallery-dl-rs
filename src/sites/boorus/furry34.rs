//! furry34.com, mirroring gallery_dl/extractor/furry34.py through the
//! rule34xyz-style JSON API engine.
use super::rule34xyz::{Api, Target, run};
use super::{parse_query, query_get, unquote};
use crate::sites::{Events, Http, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

const FILENAME: &str = "{category}_{id}.{extension}";
static POST: Route = Route::new("post", FILENAME, &["{category}"], "{id}");
static PLAYLIST: Route = Route::new(
    "playlist",
    FILENAME,
    &["{category}", "{playlist_id}"],
    "p_{playlist_id}_{id}",
);
static TAG: Route = Route::new(
    "tag",
    FILENAME,
    &["{category}", "{search_tags}"],
    "t_{search_tags}_{id}",
);

pub static SITE: Site = Site {
    category: "furry34",
    name: "furry34",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&POST, &PLAYLIST, &TAG],
    api_key: false,
    cookies: None,
};

static API: Api = Api {
    site: &SITE,
    hosts: &["furry34.com", "www.furry34.com"],
    root: "https://furry34.com",
    root_cdn: "https://furry34com.b-cdn.net",
    per_page: 30,
    file_url,
    split_filename: true,
    tag_types: &[
        (None, "general"),
        (Some(1), "general"),
        (Some(2), "copyright"),
        (Some(4), "character"),
        (Some(8), "artist"),
    ],
    // `tags[tag["type"] or 1]`
    falsy_type: Some(1),
    start,
    advance: advance_cursor,
};

fn start(params: &mut Map<String, Value>, per_page: usize) {
    params.insert("sortBy".into(), 0.into());
    params.insert("take".into(), per_page.into());
}

/// `params["cursor"] = data.get("cursor")`; shared with yiffverse.
pub(super) fn advance_cursor(
    params: &mut Map<String, Value>,
    cursor: Option<&Value>,
    _: usize,
) -> Result<()> {
    params.insert("cursor".into(), cursor.cloned().unwrap_or(Value::Null));
    Ok(())
}

/// furry34/yiffverse `_file_url`: the first of `FORMATS` present in
/// `files`. Without one, upstream takes the first `files` key while
/// `extension` keeps the loop's last value, "pic.jpg".
pub(super) fn file_url(api: &Api, _: &Http, post: &mut Map<String, Value>) -> Option<String> {
    const FORMATS: &[(&str, &str)] = &[
        ("100", "mov.mp4"),
        ("101", "mov720.mp4"),
        ("102", "mov480.mp4"),
        ("10", "pic.jpg"),
    ];
    let files = post.get("files")?.as_object()?;
    let (fmt, extension) = match FORMATS.iter().find(|(f, _)| files.contains_key(*f)) {
        Some((fmt, extension)) => ((*fmt).to_owned(), *extension),
        None => (files.keys().next()?.clone(), "pic.jpg"),
    };
    api.media_url(post, fmt, extension)
}

/// The `(tag, query)` groups of a search pattern
/// `/{tag_prefix}([^/?#]+)?(?:/?\?([^#]+))?(?:$|#)`; for an empty prefix
/// the tag group is optional on its own, otherwise together with the prefix.
pub(super) fn tag_groups<'u>(
    url: &'u Url,
    tag_prefix: &str,
) -> Option<(Option<String>, Option<&'u str>)> {
    let rest = url.path().strip_prefix('/')?;
    let (tag, remainder) = match rest.strip_prefix(tag_prefix) {
        Some(after) => {
            let tag = super::until(after, &['/']);
            if tag.is_empty() {
                (None, rest)
            } else {
                (Some(tag), &after[tag.len()..])
            }
        }
        None => (None, rest),
    };
    let query = match url.query() {
        Some("") => return None,
        query => query,
    };
    match remainder {
        "" => {}
        "/" if query.is_some() => {}
        _ => return None,
    }
    Some((tag.map(str::to_owned), query))
}

/// `type` query parameter: video → 1, image → 0.
pub(super) fn search_type(params: &[(String, String)]) -> Option<(&'static str, Value)> {
    match query_get(params, "type") {
        Some("video") => Some(("type", 1.into())),
        Some("image") => Some(("type", 0.into())),
        _ => None,
    }
}

fn target(url: &Url) -> Option<Target> {
    let rest = API.below(url)?;
    if let Some(target) = API.post(&rest) {
        return Some(target);
    }
    if let Some(target) = API.playlist(&rest, "/playlists/view/", &PLAYLIST) {
        return Some(target);
    }
    let (tag, query) = tag_groups(url, "")?;
    let params = parse_query(query.unwrap_or_default(), false);
    let mut tags = Vec::new();
    if let Some(tag) = tag {
        tags.extend(unquote(&unquote(&tag)).split('|').map(str::to_owned));
    }
    if let Some(extra) = query_get(&params, "tags") {
        tags.extend(extra.split('|').map(str::to_owned));
    }
    let include = tags.iter().map(|t| t.replace('_', " ")).collect();
    Some(API.search(&TAG, tags.join(" "), include, search_type(&params)))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(target) => run(&API, http, target),
        None => crate::sites::fail(error("unsupported furry34 URL")),
    }
}

#[cfg(test)]
mod tests {
    use super::super::rule34xyz::tests::{body, with_options};
    use super::*;
    use crate::sites::testing::{fails, http, json, keywords, media, serve};
    use serde_json::json;

    fn t(raw: &str) -> Option<Target> {
        let url = Url::parse(raw).unwrap();
        crate::sites::find(&url).filter(|s| s.category == "furry34")?;
        target(&url)
    }

    fn search(raw: &str) -> (Map<String, Value>, Map<String, Value>) {
        match t(raw) {
            Some(Target::Search {
                route,
                params,
                data,
                ..
            }) if route.subcategory == "tag" => (params, data),
            other => panic!("{raw}: {other:?}"),
        }
    }

    #[test]
    fn classifies_urls() {
        assert!(
            matches!(t("https://www.furry34.com/post/541949"), Some(Target::Post(id)) if id == "541949")
        );
        assert!(matches!(
            t("https://furry34.com/playlists/view/8966"),
            Some(Target::Search { route, .. }) if route.subcategory == "playlist"
        ));
        let (params, data) =
            search("https://furry34.com/dariana_%2528quetzaly%2529%257Canimated?type=video");
        assert_eq!(data["search_tags"], "dariana_(quetzaly) animated");
        assert_eq!(
            Value::Object(params),
            json!({"includeTags": ["dariana (quetzaly)", "animated"], "type": 1})
        );
        let (params, data) = search("https://furry34.com/tree/?tags=a_b%7Cc&type=image#x");
        assert_eq!(data["search_tags"], "tree a_b c");
        assert_eq!(params["includeTags"], json!(["tree", "a b", "c"]));
        assert_eq!(params["type"], 0);
        let (params, data) = search("https://furry34.com/");
        assert_eq!(data["search_tags"], "");
        assert_eq!(Value::Object(params), json!({"includeTags": []}));
        // url::Url normalizes the bare origin to "/", which matches too.
        search("https://furry34.com");
        search("https://furry34.com/post");
        for bad in [
            "https://furry34.com/tree/",
            "https://furry34.com/tree?",
            "https://furry34.com/a/b",
            "https://furry34.com/post/abc",
            "https://furry34.com.evil.test/post/1",
            "https://m.furry34.com/post/1",
            "https://furry34.com:8443/post/1",
            "https://u@furry34.com/post/1",
        ] {
            assert!(t(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn file_url_fallback_keeps_last_extension() {
        let plain = http("furry34", Url::parse("http://127.0.0.1:1/").unwrap());
        let mut post = json!({"id": 605309, "files": {"5": [true], "10": [false]}})
            .as_object()
            .unwrap()
            .clone();
        assert_eq!(
            file_url(&API, &plain, &mut post).as_deref(),
            Some("https://furry34.com/posts/605/605309/605309.pic.jpg")
        );
        post.insert("files".into(), json!({"7": [1]}));
        assert_eq!(
            file_url(&API, &plain, &mut post).as_deref(),
            Some("https://furry34com.b-cdn.net/posts/605/605309/605309.pic.jpg")
        );
        assert_eq!(post["format_id"], "7");
        assert_eq!(post["format"], "pic");
        post.insert("files".into(), json!({}));
        assert_eq!(file_url(&API, &plain, &mut post), None);
    }

    fn post(id: i64) -> Value {
        json!({"id": id, "created": "2024-09-20T19:49:47.443232Z", "type": 1,
            "files": {"10": [true], "100": [false]}, "uploaderId": 2})
    }

    #[tokio::test]
    async fn post_with_falsy_tag_types() {
        let mut full = post(541949).as_object().unwrap().clone();
        full.insert(
            "tags".into(),
            json!([{"type": 8, "value": "echodragon"}, {"type": 0, "value": "canid"},
                   {"type": null, "value": "anthro"}]),
        );
        let (root, server) = serve(vec![json(Value::Object(full))]);
        let http = with_options(http("furry34", root), json!({"tags": true}));
        let files = media(extract(
            &http,
            Url::parse("https://furry34.com/post/541949").unwrap(),
        ))
        .await;
        assert_eq!(
            files[0].url.as_str(),
            "https://furry34.com/posts/541/541949/541949.mov.mp4"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["format"], "mov");
        assert_eq!(k["format_id"], "100");
        assert_eq!(k["filename"], "541949");
        assert_eq!(k["extension"], "mp4");
        assert_eq!(k["date"], "2024-09-20 19:49:47");
        assert_eq!(k["tags_general"], json!(["canid", "anthro"]));
        assert_eq!(k["tags_artist"], json!(["echodragon"]));
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "furry34_541949.mp4");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("furry34")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{id}",["541949"]]"#);
        assert!(server.join().unwrap()[0].starts_with("GET /api/v2/post/541949 "));
    }

    #[tokio::test]
    async fn paginates_by_cursor() {
        let first: Vec<_> = (1..=30).map(post).collect();
        let (root, server) = serve(vec![
            json(json!({"items": first, "cursor": "abc"})),
            json(json!({"items": [post(31)], "cursor": null})),
        ]);
        let http = http("furry34", root);
        let files = media(extract(
            &http,
            Url::parse("https://furry34.com/playlists/view/8966").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 31);
        let k = keywords(&files[30]);
        assert_eq!(k["playlist_id"], "8966");
        let (file, dir) = crate::sites::default_templates(&files[30]).unwrap();
        assert_eq!(file.render(&files[30]).unwrap(), "furry34_31.mp4");
        assert_eq!(
            dir.render(&files[30]).unwrap().unwrap(),
            std::path::PathBuf::from("furry34/8966")
        );
        let (_, identity) = crate::sites::archive_key(&files[30]).unwrap();
        assert_eq!(identity, r#"["p_{playlist_id}_{id}",["8966","31"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("POST /api/v2/post/search/playlist/8966 "));
        assert_eq!(body(&requests[0]).to_string(), r#"{"sortBy":0,"take":30}"#);
        assert_eq!(
            body(&requests[1]).to_string(),
            r#"{"sortBy":0,"take":30,"cursor":"abc"}"#
        );
    }

    #[tokio::test]
    async fn tag_search_body() {
        let (root, server) = serve(vec![json(json!({"items": [post(2)]}))]);
        let http = http("furry34", root);
        let files = media(extract(
            &http,
            Url::parse("https://furry34.com/big_tree?type=video").unwrap(),
        ))
        .await;
        let k = keywords(&files[0]);
        assert_eq!(k["search_tags"], "big_tree");
        let (_, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("furry34/big_tree")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["t_{search_tags}_{id}",["big_tree","2"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("POST /api/v2/post/search/root "));
        assert_eq!(
            body(&requests[0]).to_string(),
            r#"{"includeTags":["big tree"],"type":1,"sortBy":0,"take":30}"#
        );
    }

    #[tokio::test]
    async fn missing_post_is_an_error() {
        let (root, server) = serve(vec![crate::sites::testing::response(
            "404 Not Found",
            "application/json",
            b"{}",
        )]);
        let http = http("furry34", root);
        assert!(
            fails(extract(
                &http,
                Url::parse("https://furry34.com/post/9").unwrap()
            ))
            .await
        );
        server.join().unwrap();
    }
}
