//! rule34vault.com, mirroring gallery_dl/extractor/rule34vault.py through
//! the rule34xyz-style JSON API engine.
use super::rule34xyz::{Api, Target, run};
use super::unquote;
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
    category: "rule34vault",
    name: "rule34vault",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&POST, &PLAYLIST, &TAG],
    api_key: false,
    cookies: None,
};

static API: Api = Api {
    site: &SITE,
    // BASE_PATTERN has no optional `www.`.
    hosts: &["rule34vault.com"],
    root: "https://rule34vault.com",
    root_cdn: "https://r34xyz.b-cdn.net",
    per_page: 100,
    file_url,
    split_filename: false,
    tag_types: &[
        (Some(1), "general"),
        (Some(2), "copyright"),
        (Some(4), "character"),
        (Some(8), "artist"),
    ],
    falsy_type: None,
    start,
    advance,
};

fn start(params: &mut Map<String, Value>, per_page: usize) {
    params.insert("CountTotal".into(), false.into());
    params.insert("Skip".into(), 0.into());
    params.insert("take".into(), per_page.into());
}

/// `cursor = data.get("cursor"); Skip += take`.
fn advance(params: &mut Map<String, Value>, cursor: Option<&Value>, per_page: usize) -> Result<()> {
    params.insert("cursor".into(), cursor.cloned().unwrap_or(Value::Null));
    let skip = params.get("Skip").and_then(Value::as_u64).unwrap_or(0);
    let take = params
        .get("take")
        .and_then(Value::as_u64)
        .unwrap_or(per_page as u64);
    params.insert("Skip".into(), (skip + take).into());
    Ok(())
}

/// `{root_cdn}/posts/{id // 1000}/{id}/{id}.{jpg|mp4}` by `type`.
fn file_url(api: &Api, _: &Http, post: &mut Map<String, Value>) -> Option<String> {
    let image = match post.get("type")? {
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::Bool(b) => !b,
        _ => false,
    };
    let extension = if image { "jpg" } else { "mp4" };
    let id = post.get("id")?.as_i64()?;
    let url = format!(
        "{}/posts/{}/{id}/{id}.{extension}",
        api.root_cdn,
        id.div_euclid(1000)
    );
    post.insert("file_url".into(), url.clone().into());
    Some(url)
}

fn target(url: &Url) -> Option<Target> {
    let rest = API.below(url)?;
    if let Some(target) = API.post(&rest) {
        return Some(target);
    }
    if let Some(target) = API.playlist(&rest, "/playlists/view/", &PLAYLIST) {
        return Some(target);
    }
    // r"/(?!p(?:ost|laylists)/)([^/?#]+)", prefix-anchored only.
    let path = rest.strip_prefix('/')?;
    if path.starts_with("post/") || path.starts_with("playlists/") {
        return None;
    }
    let tag = super::until(path, &['/', '?']);
    if tag.is_empty() {
        return None;
    }
    let tags: Vec<String> = unquote(tag).split("%7C").map(str::to_owned).collect();
    let include = tags.iter().map(|t| t.replace('_', " ")).collect();
    Some(API.search(&TAG, tags.join(" "), include, None))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(target) => run(&API, http, target),
        None => crate::sites::fail(error("unsupported rule34vault URL")),
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
        crate::sites::find(&url).filter(|s| s.category == "rule34vault")?;
        target(&url)
    }

    #[test]
    fn classifies_urls() {
        assert!(
            matches!(t("https://rule34vault.com/post/280517"), Some(Target::Post(id)) if id == "280517")
        );
        assert!(matches!(
            t("https://rule34vault.com/playlists/view/20164"),
            Some(Target::Search { route, .. }) if route.subcategory == "playlist"
        ));
        let Some(Target::Search {
            route,
            params,
            data,
            ..
        }) = t("https://rule34vault.com/a_b%257Cc%7Cd/x?y#z")
        else {
            panic!()
        };
        assert_eq!(route.subcategory, "tag");
        // Only a literal "%7C" after one unquote separates tags.
        assert_eq!(data["search_tags"], "a_b c|d");
        assert_eq!(params["includeTags"], json!(["a b", "c|d"]));
        assert!(matches!(
            t("https://rule34vault.com/post"),
            Some(Target::Search { .. })
        ));
        assert!(matches!(
            t("https://rule34vault.com/postx/1"),
            Some(Target::Search { .. })
        ));
        for bad in [
            "https://rule34vault.com/post/abc",
            "https://rule34vault.com/playlists/x",
            "https://rule34vault.com/",
            "https://rule34vault.com/?tags=x",
            "https://www.rule34vault.com/post/1",
            "https://rule34vault.com.evil.test/post/1",
            "https://rule34vault.com:444/post/1",
            "https://u:p@rule34vault.com/post/1",
        ] {
            assert!(t(bad).is_none(), "{bad}");
        }
    }

    fn post(id: i64, kind: i64) -> Value {
        json!({"id": id, "type": kind, "created": "2023-09-01T11:57:57.317331Z",
            "files": {"10": [true]}, "uploaderId": 20678})
    }

    #[tokio::test]
    async fn post_with_tags_option_fetches_details() {
        let mut full = post(280517, 0).as_object().unwrap().clone();
        full.insert(
            "tags".into(),
            json!([{"type": 2, "value": "demon slayer"}, {"type": 1, "value": "sfw"}]),
        );
        let (root, server) = serve(vec![json(Value::Object(full))]);
        let http = with_options(http("rule34vault", root), json!({"tags": true}));
        let files = media(extract(
            &http,
            Url::parse("https://rule34vault.com/post/280517").unwrap(),
        ))
        .await;
        assert_eq!(
            files[0].url.as_str(),
            "https://r34xyz.b-cdn.net/posts/280/280517/280517.jpg"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["filename"], "280517");
        assert_eq!(k["extension"], "jpg");
        assert_eq!(k["file_url"], files[0].url.as_str());
        assert_eq!(k["date"], "2023-09-01 11:57:57");
        assert_eq!(k["tags"], json!(["demon slayer", "sfw"]));
        assert_eq!(k["tags_copyright"], json!(["demon slayer"]));
        assert_eq!(k["tags_general"], json!(["sfw"]));
        assert!(!k.contains_key("format") && !k.contains_key("files"));
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "rule34vault_280517.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("rule34vault")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{id}",["280517"]]"#);
        assert!(server.join().unwrap()[0].starts_with("GET /api/v2/post/280517 "));
    }

    #[tokio::test]
    async fn paginates_with_skip_and_cursor() {
        let first: Vec<_> = (1..=100).map(|i| post(i, 1)).collect();
        let (root, server) = serve(vec![
            json(json!({"items": first, "cursor": "next"})),
            json(json!({"items": [post(382937, 1)]})),
        ]);
        let http = http("rule34vault", root);
        let files = media(extract(
            &http,
            Url::parse("https://rule34vault.com/sfw").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 101);
        assert_eq!(
            files[100].url.as_str(),
            "https://r34xyz.b-cdn.net/posts/382/382937/382937.mp4"
        );
        let k = keywords(&files[100]);
        assert_eq!(k["search_tags"], "sfw");
        let (file, dir) = crate::sites::default_templates(&files[100]).unwrap();
        assert_eq!(file.render(&files[100]).unwrap(), "rule34vault_382937.mp4");
        assert_eq!(
            dir.render(&files[100]).unwrap().unwrap(),
            std::path::PathBuf::from("rule34vault/sfw")
        );
        let (_, identity) = crate::sites::archive_key(&files[100]).unwrap();
        assert_eq!(identity, r#"["t_{search_tags}_{id}",["sfw","382937"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("POST /api/v2/post/search/root "));
        assert_eq!(
            body(&requests[0]).to_string(),
            r#"{"includeTags":["sfw"],"CountTotal":false,"Skip":0,"take":100}"#
        );
        assert_eq!(
            body(&requests[1]).to_string(),
            r#"{"includeTags":["sfw"],"CountTotal":false,"Skip":100,"take":100,"cursor":"next"}"#
        );
    }

    #[tokio::test]
    async fn unknown_tag_type_fails_like_upstream() {
        let mut full = post(1, 0).as_object().unwrap().clone();
        full.insert("tags".into(), json!([{"type": null, "value": "x"}]));
        let (root, _server) = serve(vec![json(Value::Object(full))]);
        let http = with_options(http("rule34vault", root), json!({"tags": true}));
        assert!(
            fails(extract(
                &http,
                Url::parse("https://rule34vault.com/post/1").unwrap(),
            ))
            .await
        );
    }
}
