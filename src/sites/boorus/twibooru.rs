//! Twibooru, mirroring gallery_dl/extractor/twibooru.py through the
//! `/api/v3` API.
use super::philomena::{Target, classify, filter_option};
use super::{date_value, iso_timestamp};
use crate::error;
use crate::sites::{Events, Http, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://twibooru.org";
const PER_PAGE: usize = 50;
const FILENAME: &str = "{id}_{filename}.{extension}";
static POST: Route = Route::new("post", FILENAME, &["{category}"], "{id}");
static SEARCH: Route = Route::new("search", FILENAME, &["{category}", "{search_tags}"], "{id}");
static GALLERY: Route = Route::new(
    "gallery",
    FILENAME,
    &["{category}", "galleries", "{gallery[id]} {gallery[title]}"],
    "{id}",
);

pub static SITE: Site = Site {
    category: "twibooru",
    name: "Twibooru",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&POST, &SEARCH, &GALLERY],
    api_key: true,
};

fn target(url: &Url) -> Option<Target> {
    classify(
        &super::below(url, &["twibooru.org", "www.twibooru.org"])?,
        false,
    )
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Twibooru URL"));
    };
    match target {
        Target::Post(id) => crate::sites::once(async move {
            let url = http.api(&format!("{ROOT}/api/v3/posts/{id}"))?;
            let body = http.json(&url).await?;
            let post = body
                .get("post")
                .ok_or_else(|| error("Twibooru post response lacks a post"))?;
            Ok(crate::sites::events(posts_events(
                http,
                &POST,
                std::slice::from_ref(post),
                &Map::new(),
            )))
        }),
        Target::Search(params) => {
            let mut data = Map::new();
            data.insert(
                "search_tags".into(),
                super::query_get(&params, "q").unwrap_or_default().into(),
            );
            search(http, &SEARCH, params, data)
        }
        Target::Gallery(id) => crate::sites::once(async move {
            let url = http.api(&format!("{ROOT}/api/v3/galleries/{id}"))?;
            let body = http.json(&url).await?;
            let gallery = body
                .get("gallery")
                .cloned()
                .ok_or_else(|| error("Twibooru gallery response lacks a gallery"))?;
            let mut data = Map::new();
            data.insert("gallery".into(), gallery);
            let filter = format!("gallery_id:{id}");
            let params = vec![
                ("sd".into(), "desc".into()),
                ("sf".into(), filter.clone()),
                ("q".into(), filter),
            ];
            Ok(search(http, &GALLERY, params, data))
        }),
    }
}

fn search<'a>(
    http: &'a Http,
    route: &'static Route,
    mut params: Vec<(String, String)>,
    data: Map<String, Value>,
) -> Events<'a> {
    if let Some(key) = http.api_key() {
        params.push(("key".into(), key.into()));
    }
    if let Some(filter) = filter_option(http) {
        params.push(("filter_id".into(), filter));
    } else if http.api_key().is_none() {
        params.push(("filter_id".into(), "2".into()));
    }
    crate::sites::paginate(Some(1u64), move |page| {
        let params = params.clone();
        let data = data.clone();
        async move {
            let Some(page) = page else {
                return Ok(None);
            };
            let mut url = http.api(&format!("{ROOT}/api/v3/search/posts"))?;
            url.query_pairs_mut()
                .extend_pairs(&params)
                .append_pair("page", &page.to_string())
                .append_pair("per_page", &PER_PAGE.to_string());
            let body = http.json(&url).await?;
            let posts = body
                .get("posts")
                .and_then(Value::as_array)
                .ok_or_else(|| error("Twibooru search response lacks posts"))?;
            let events = posts_events(http, route, posts, &data);
            let next = (posts.len() >= PER_PAGE).then_some(page + 1);
            Ok(Some((crate::sites::events(events), next)))
        }
    })
}

fn posts_events(
    http: &Http,
    route: &'static Route,
    posts: &[Value],
    data: &Map<String, Value>,
) -> Vec<crate::event::ExtractionEvent> {
    let svg = http.option_bool("svg", true);
    let items = posts
        .iter()
        .filter_map(|post| parse_post(post, data, svg))
        .collect();
    super::per_post(&SITE, http, route, items)
}

fn parse_post(post: &Value, data: &Map<String, Value>, svg: bool) -> Option<crate::sites::Item> {
    let mut fields = post.as_object()?.clone();
    let view = fields.get("view_url")?.as_str()?;
    let raw = if svg && fields.get("format").and_then(Value::as_str) == Some("svg") {
        format!("{}.svg", super::rpartition(view, ".").0)
    } else {
        view.to_owned()
    };
    let url = super::file_url("", &raw).filter(|u| u.host_str().is_some())?;
    super::booru_fields(&url, &mut fields, data);
    let created = fields
        .get("created_at")
        .and_then(Value::as_str)
        .and_then(iso_timestamp);
    fields.insert("date".into(), date_value(created));
    if let Some(name) = fields.get("name").and_then(Value::as_str) {
        let filename = match name.rsplit_once('.') {
            Some((head, _)) => head.to_owned(),
            None => name.to_owned(),
        };
        fields.insert("filename".into(), filename.into());
    }
    Some(super::item(url, fields, "uploader", "tags", created))
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
            t("https://twibooru.org/1234"),
            Some(Target::Post("1234".into()))
        );
        assert_eq!(
            t("https://www.twibooru.org/search?q=cute"),
            Some(Target::Search(vec![("q".into(), "cute".into())]))
        );
        assert_eq!(
            t("https://twibooru.org/tags/rating-colon-safe"),
            Some(Target::Search(vec![("q".into(), "rating:safe".into())]))
        );
        assert_eq!(
            t("https://twibooru.org/galleries/9"),
            Some(Target::Gallery("9".into()))
        );
        for raw in [
            "https://twibooru.org/images/1",
            "https://twibooru.org.evil.test/1",
            "https://twibooru.org:444/1",
            "https://cdn.twibooru.org/1",
            "https://twibooru.org/search",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn post(id: u64) -> Value {
        json!({"id": id, "format": "png", "name": "my.picture.png",
            "created_at": "2017-07-14T02:40:00.123Z", "uploader": "u", "tags": ["a"],
            "view_url": format!("https://cdn.twibooru.org/img/2017/7/14/{id}/full.png")})
    }

    #[tokio::test]
    async fn extracts_post_with_name_filename() {
        let (root, server) = serve(vec![json(json!({"post": post(5)}))]);
        let http = http("Twibooru", root);
        let files = media(extract(
            &http,
            Url::parse("https://twibooru.org/5").unwrap(),
        ))
        .await;
        let k = keywords(&files[0]);
        assert_eq!(k["filename"], "my.picture");
        assert_eq!(k["extension"], "png");
        assert_eq!(k["date"], "2017-07-14 02:40:00");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "5_my.picture.png");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("twibooru")
        );
        assert_eq!(
            crate::sites::archive_key(&files[0]).unwrap().1,
            r#"["{id}",["5"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/v3/posts/5 "));
    }

    #[tokio::test]
    async fn gallery_paginates_search() {
        let first: Vec<_> = (1..=50).map(post).collect();
        let (root, server) = serve(vec![
            json(json!({"gallery": {"id": 9, "title": "G"}})),
            json(json!({"posts": first})),
            json(json!({"posts": []})),
        ]);
        let http = http("Twibooru", root);
        let files = media(extract(
            &http,
            Url::parse("https://twibooru.org/galleries/9").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 50);
        let (_, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("twibooru/galleries/9 G")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/v3/galleries/9 "));
        assert!(
            requests[1].starts_with(
                "GET /api/v3/search/posts?sd=desc&sf=gallery_id%3A9&q=gallery_id%3A9&filter_id=2&page=1&per_page=50 "
            ),
            "{}",
            requests[1]
        );
        assert!(requests[2].contains("&page=2&per_page=50 "));
    }
}
