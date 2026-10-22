//! yiffverse.com, mirroring gallery_dl/extractor/yiffverse.py through the
//! rule34xyz-style JSON API engine (file URLs and cursors as furry34).
use super::furry34::{advance_cursor, file_url, search_type, tag_groups};
use super::rule34xyz::{Api, Target, run};
use super::{parse_query, query_get, unquote};
use crate::error;
use crate::sites::{Events, Http, Route, Site};
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
    category: "yiffverse",
    name: "yiffverse",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&POST, &PLAYLIST, &TAG],
    api_key: false,
};

static API: Api = Api {
    site: &SITE,
    hosts: &["yiffverse.com", "www.yiffverse.com"],
    root: "https://yiffverse.com",
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
        (Some(16), "system"),
        (Some(32), "meta"),
    ],
    falsy_type: None,
    start,
    advance: advance_cursor,
};

fn start(params: &mut Map<String, Value>, per_page: usize) {
    params.insert("sortOrder".into(), 1.into());
    params.insert("status".into(), 2.into());
    params.insert("take".into(), per_page.into());
}

fn target(url: &Url) -> Option<Target> {
    let rest = API.below(url)?;
    if let Some(target) = API.post(&rest) {
        return Some(target);
    }
    if let Some(target) = API.playlist(&rest, "/playlist/", &PLAYLIST) {
        return Some(target);
    }
    let (tag, query) = tag_groups(url, "tag/")?;
    let params = parse_query(query.unwrap_or_default(), false);
    let mut tags = Vec::new();
    if let Some(tag) = tag {
        tags.push(unquote(&tag));
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
        None => crate::sites::fail(error("unsupported yiffverse URL")),
    }
}

#[cfg(test)]
mod tests {
    use super::super::rule34xyz::tests::{body, with_options};
    use super::*;
    use crate::sites::testing::{http, json, keywords, media, serve};
    use serde_json::json;

    fn t(raw: &str) -> Option<Target> {
        let url = Url::parse(raw).unwrap();
        crate::sites::find(&url).filter(|s| s.category == "yiffverse")?;
        target(&url)
    }

    #[test]
    fn classifies_urls() {
        assert!(matches!(t("https://yiffverse.com/post/574342"), Some(Target::Post(id)) if id == "574342"));
        assert!(matches!(
            t("https://www.yiffverse.com/playlist/6842"),
            Some(Target::Search { route, .. }) if route.subcategory == "playlist"
        ));
        let Some(Target::Search {
            route,
            params,
            data,
            ..
        }) = t("https://yiffverse.com/tag/a_b%7Cc/?tags=d&type=video")
        else {
            panic!()
        };
        assert_eq!(route.subcategory, "tag");
        // The path tag is not split on "|".
        assert_eq!(data["search_tags"], "a_b|c d");
        assert_eq!(
            Value::Object(params),
            json!({"includeTags": ["a b|c", "d"], "type": 1})
        );
        assert!(matches!(
            t("https://yiffverse.com/?tags=x"),
            Some(Target::Search { .. })
        ));
        assert!(matches!(t("https://yiffverse.com/"), Some(Target::Search { .. })));
        for bad in [
            "https://yiffverse.com/tree",
            "https://yiffverse.com/tag/",
            "https://yiffverse.com/tag/a/b",
            "https://yiffverse.com/tag/a?",
            "https://yiffverse.com/playlists/view/1",
            "https://yiffverse.com.evil.test/post/1",
            "https://yiffverse.com:81/post/1",
        ] {
            assert!(t(bad).is_none(), "{bad}");
        }
    }

    fn post(id: i64) -> Value {
        json!({"id": id, "created": "2024-12-06T13:55:24.483002Z", "type": 0,
            "files": {"10": [true]}, "uploaderId": 2})
    }

    #[tokio::test]
    async fn post_tags_and_paths() {
        let mut full = post(574342).as_object().unwrap().clone();
        full.insert(
            "tags".into(),
            json!([{"type": 32, "value": "6:5"}, {"type": 8, "value": "imanika"}]),
        );
        let (root, server) = serve(vec![json(Value::Object(full))]);
        let http = with_options(http("yiffverse", root), json!({"tags": true}));
        let files = media(extract(
            &http,
            Url::parse("https://yiffverse.com/post/574342").unwrap(),
        ))
        .await;
        assert_eq!(
            files[0].url.as_str(),
            "https://furry34com.b-cdn.net/posts/574/574342/574342.pic.jpg"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["filename"], "574342");
        assert_eq!(k["format"], "pic");
        assert_eq!(k["tags_meta"], json!(["6:5"]));
        assert_eq!(k["tags_artist"], json!(["imanika"]));
        assert_eq!(k["date"], "2024-12-06 13:55:24");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "yiffverse_574342.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("yiffverse")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{id}",["574342"]]"#);
        assert!(server.join().unwrap()[0].starts_with("GET /api/v2/post/574342 "));
    }

    #[tokio::test]
    async fn paginates_tag_search() {
        let first: Vec<_> = (1..=30).map(post).collect();
        let (root, server) = serve(vec![
            json(json!({"items": first, "cursor": 7})),
            json(json!({"items": []})),
        ]);
        let http = http("yiffverse", root);
        let files = media(extract(
            &http,
            Url::parse("https://yiffverse.com/tag/tree").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 30);
        assert_eq!(keywords(&files[0])["search_tags"], "tree");
        let (_, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("yiffverse/tree")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["t_{search_tags}_{id}",["tree","1"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("POST /api/v2/post/search/root "));
        assert_eq!(
            body(&requests[0]).to_string(),
            r#"{"includeTags":["tree"],"sortOrder":1,"status":2,"take":30}"#
        );
        assert_eq!(
            body(&requests[1]).to_string(),
            r#"{"includeTags":["tree"],"sortOrder":1,"status":2,"take":30,"cursor":7}"#
        );
    }
}
