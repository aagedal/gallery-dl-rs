//! WeebDex chapters and manga, mirroring gallery_dl/extractor/weebdex.py:
//! everything comes from the `api.weebdex.org` JSON API, with the manga
//! info memoized per invocation.
//!
//! Upstream's `request_interval` of 0.2 seconds is not applied.
use super::{Gallery, Memo, get, keywords, set, str_of, text};
use crate::sites::art::parse_query;
use crate::sites::{Events, Http, Route, Site};
use crate::{Result, error};
use futures_util::StreamExt;
use serde_json::{Map, Value};
use url::Url;

pub static SITE: Site = Site {
    category: "weebdex",
    name: "WeebDex",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CHAPTER, &MANGA],
    api_key: false,
    cookies: None,
};

const ROOT: &str = "https://weebdex.org";
const API: &str = "https://api.weebdex.org";
/// Chapter list pages a manga may claim.
const MAX_PAGES: i64 = 10_000;

static CHAPTER: Route = Route::new(
    "chapter",
    "{manga}_c{chapter:>03}{chapter_minor:?//}_{page:>03}.{extension}",
    &[
        "{category}",
        "{manga}",
        "{volume:?v/ />02}c{chapter:>03}{chapter_minor:?//}{title:?: //}",
    ],
    "{chapter_id}_{version}_{page}",
);
static MANGA: Route = Route::new("manga", "{filename}.{extension}", &["{category}"], "");

#[derive(Debug, PartialEq)]
enum Target {
    Chapter(String),
    /// Manga ID and the query of a `/title/ID/SLUG?…` URL.
    Manga(String, Option<String>),
}

/// The `\w+` prefix of a raw path segment.
fn word(segment: &str) -> Option<&str> {
    let end = segment
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(segment.len());
    (end > 0).then(|| &segment[..end])
}

/// `weebdex\.org/chapter/(\w+)` and
/// `weebdex\.org/title/(\w+)(?:/[^/?#]+/?\?([^#]+))?`, both unanchored.
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) || url.host_str() != Some("weebdex.org") {
        return None;
    }
    let raw: Vec<&str> = url.path().split('/').skip(1).collect();
    match raw.as_slice() {
        ["chapter", id, ..] => word(id).map(|id| Target::Chapter(id.to_owned())),
        ["title", id, rest @ ..] => {
            let mid = word(id)?;
            // The query counts only right after `ID/SLUG` or `ID/SLUG/`.
            let query = url.query().filter(|q| !q.is_empty());
            let slug = matches!(rest, [slug] | [slug, ""] if !slug.is_empty());
            let qs = (mid.len() == id.len() && slug).then_some(query).flatten();
            Some(Target::Manga(mid.to_owned(), qs.map(str::to_owned)))
        }
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Chapter(id)) => chapter_events(http, id, Memo::default()),
        Some(Target::Manga(id, qs)) => {
            crate::sites::once(async move { manga_events(http, &id, qs.as_deref()).await })
        }
        None => crate::sites::fail(error("unsupported WeebDex URL")),
    }
}

/// `WeebdexBase.headers_api`.
async fn api(http: &Http, url: &Url) -> Result<Value> {
    let referer = format!("{ROOT}/");
    http.json_with(
        url,
        &http.browser_headers(&[
            ("Referer", &referer),
            ("Origin", ROOT),
            ("Sec-Fetch-Dest", "empty"),
            ("Sec-Fetch-Mode", "cors"),
            ("Sec-Fetch-Site", "same-site"),
        ]),
    )
    .await
}

/// The names in a relationship list.
fn names(list: Option<&Value>) -> Vec<Value> {
    list.and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .map(|item| get(item, "name"))
        .collect()
}

/// `WeebdexBase._manga_info`, memoized by manga ID.
async fn manga_info(http: &Http, mid: &str, memo: &Memo) -> Result<Map<String, Value>> {
    if let Some(Value::Object(info)) = memo.get(mid) {
        return Ok(info);
    }
    let manga = api(http, &http.api(&format!("{API}/manga/{mid}"))?).await?;
    let rel = get(&manga, "relationships");
    let tags: Vec<String> = rel
        .get("tags")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .map(|tag| format!("{}:{}", str_of(tag, "group"), str_of(tag, "name")))
        .collect();
    let info = keywords([
        ("manga", get(&manga, "title")),
        ("manga_id", get(&manga, "id")),
        (
            "manga_date",
            text::date(text::iso(str_of(&manga, "created_at"))),
        ),
        ("year", get(&manga, "year")),
        ("status", get(&manga, "status")),
        ("origin", get(&manga, "language")),
        ("description", get(&manga, "description")),
        ("demographic", get(&manga, "demographic")),
        ("tags", tags.into()),
        ("author", names(rel.get("authors")).into()),
        ("artist", names(rel.get("artists")).into()),
    ]);
    memo.insert(mid, Value::Object(info.clone()));
    Ok(info)
}

/// `chapter, sep, minor = data["chapter"].partition(".")`, with `0` and
/// `""` when the chapter is not a string.
fn chapter_parts(value: Option<&Value>) -> (i64, String) {
    match value {
        Some(Value::String(chapter)) => text::chapter(chapter),
        _ => (0, String::new()),
    }
}

/// `WeebdexChapterExtractor`: the chapter's JSON and its manga's info; with
/// `data-saver` the optimized images.
fn chapter_events(http: &Http, cid: String, memo: Memo) -> Events<'_> {
    crate::sites::once(async move {
        let data = api(http, &http.api(&format!("{API}/chapter/{cid}"))?).await?;
        let rel = get(&data, "relationships");
        let mid = rel
            .pointer("/manga/id")
            .map(crate::sites::scalar)
            .ok_or_else(|| error("WeebDex chapter has no manga"))?;
        let mut meta = manga_info(http, &mid, &memo).await?;
        let (chapter, minor) = chapter_parts(data.get("chapter"));
        let created = text::iso(str_of(&data, "created_at"));
        set(
            &mut meta,
            "title",
            data.get("title").cloned().unwrap_or_else(|| "".into()),
        );
        set(
            &mut meta,
            "version",
            data.get("version").cloned().unwrap_or_else(|| 0.into()),
        );
        set(
            &mut meta,
            "volume",
            text::parse_int_value(data.get("volume")),
        );
        set(&mut meta, "chapter", chapter);
        set(&mut meta, "chapter_minor", minor);
        set(&mut meta, "chapter_id", cid.as_str());
        set(&mut meta, "date", text::date(created));
        set(
            &mut meta,
            "date_updated",
            text::date(text::iso(str_of(&data, "updated_at"))),
        );
        set(&mut meta, "lang", get(&data, "language"));
        set(
            &mut meta,
            "uploader",
            rel.pointer("/uploader/name")
                .cloned()
                .unwrap_or_else(|| "".into()),
        );
        set(&mut meta, "group", names(rel.get("groups")));
        let original = !http.option_bool("data-saver", false);
        let key = if original { "data" } else { "data_optimized" };
        let base = format!(
            "{}/data/{}/",
            crate::sites::scalar(&get(&data, "node")),
            crate::sites::scalar(&get(&data, "id"))
        );
        let pages = data
            .get(key)
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .map(|page| {
                let size = |i: &str| page.pointer(&format!("/dimensions/{i}")).cloned();
                (
                    format!("{base}{}", crate::sites::scalar(&get(page, "name"))),
                    Some(keywords([
                        ("width", size("0").unwrap_or(Value::Null)),
                        ("height", size("1").unwrap_or(Value::Null)),
                        ("original", original.into()),
                    ])),
                )
            })
            .collect();
        Ok(crate::sites::events(
            Gallery::chapter(&CHAPTER, meta, pages)
                .id(cid)
                .timestamp(created)
                .events(&SITE, http),
        ))
    })
}

/// `WeebdexMangaExtractor.chapters`: every page of the chapter list in API
/// order (`reverse = False`); the URL's query overrides the defaults.
async fn manga_events<'a>(http: &'a Http, mid: &str, qs: Option<&str>) -> Result<Events<'a>> {
    let mut params = parse_query(qs.unwrap_or_default());
    let mut setdefault = |key: &str, value: String| {
        if !params.iter().any(|(k, _)| k == key) {
            params.push((key.to_owned(), value));
        }
    };
    setdefault("limit", "100".into());
    setdefault("order", "asc".into());
    let lang = match http.option("lang") {
        Some(value) => crate::sites::scalar(value),
        None => "en".into(),
    };
    setdefault("tlang", lang);
    let limit = params
        .iter()
        .find(|(k, _)| k == "limit")
        .map_or(100, |(_, v)| text::parse_int(v));
    let memo = Memo::default();
    let manga = manga_info(http, mid, &memo).await?;
    let mut children = Vec::new();
    for _ in 0..MAX_PAGES {
        let mut url = http.api(&format!("{API}/manga/{mid}/chapters"))?;
        url.query_pairs_mut().extend_pairs(&params);
        let data = api(http, &url).await?;
        for ch in data
            .get("data")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            let mut ch = ch.as_object().cloned().unwrap_or_default();
            let (chapter, minor) = chapter_parts(ch.get("chapter"));
            let volume = text::parse_int_value(ch.get("volume"));
            set(&mut ch, "volume", volume);
            set(&mut ch, "chapter", chapter);
            set(&mut ch, "chapter_minor", minor);
            crate::sites::art::update(&mut ch, &manga);
            let id = ch.get("id").map(crate::sites::scalar).unwrap_or_default();
            children.push((id, ch));
        }
        let page = text::parse_int_value(data.get("page"));
        if text::parse_int_value(data.get("total")) <= page * limit {
            break;
        }
        params.retain(|(k, _)| k != "page");
        params.push(("page".into(), (page + 1).to_string()));
    }
    if http.option_bool("chapter-reverse", false) {
        children.reverse();
    }
    let streams: Vec<Events<'a>> = children
        .into_iter()
        .filter_map(|(id, data)| {
            // The queued URL is `{ROOT}/chapter/ID`.
            let url = Url::parse(&format!("{ROOT}/chapter/{id}")).ok()?;
            let Some(Target::Chapter(cid)) = target(&url) else {
                return None;
            };
            let memo = memo.clone();
            Some(super::queue_with(&SITE, &MANGA, &data, move || {
                chapter_events(http, cid, memo)
            }))
        })
        .collect();
    Ok(futures_util::stream::iter(streams).flatten().boxed())
}

#[cfg(test)]
mod tests {
    use super::super::tests::{archive, path};
    use super::*;
    use crate::sites::testing::{http, json, keywords as kw, media, queued, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://weebdex.org/chapter/abc123/2"),
            Some(Target::Chapter("abc123".into()))
        );
        assert_eq!(
            t("https://weebdex.org/title/m1/some-slug?tlang=de&order=desc"),
            Some(Target::Manga(
                "m1".into(),
                Some("tlang=de&order=desc".into())
            ))
        );
        assert_eq!(
            t("https://weebdex.org/title/m1/some-slug/?tlang=de"),
            Some(Target::Manga("m1".into(), Some("tlang=de".into())))
        );
        assert_eq!(
            t("https://weebdex.org/title/m1?tlang=de"),
            Some(Target::Manga("m1".into(), None))
        );
        assert_eq!(
            t("https://weebdex.org/title/m1/slug/more?tlang=de"),
            Some(Target::Manga("m1".into(), None))
        );
        for raw in [
            "https://weebdex.org/chapter/",
            "https://weebdex.org/title/-x",
            "https://www.weebdex.org/title/m1",
            "https://weebdex.org.evil.test/title/m1",
            "https://weebdex.org:8443/title/m1",
            "https://u:p@weebdex.org/title/m1",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn manga_json() -> Value {
        json!({"id": "m1", "title": "Kimi to Boku", "created_at": "2024-01-02T03:04:05Z",
            "year": 2020, "status": "ongoing", "language": "ja", "description": "D",
            "demographic": "shounen", "relationships": {
                "tags": [{"group": "genre", "name": "Romance"}],
                "authors": [{"name": "A"}], "artists": [{"name": "B"}]}})
    }

    fn chapter_json() -> Value {
        json!({"id": "c9", "chapter": "12.5", "volume": "2", "title": "Start: Again",
            "version": 3, "language": "en", "node": "https://node.test",
            "created_at": "2025-01-02T03:04:05Z", "updated_at": "2025-01-03T03:04:05Z",
            "data": [{"name": "1.png", "dimensions": [800, 1200]}],
            "data_optimized": [{"name": "1.webp", "dimensions": [400, 600]}],
            "relationships": {"manga": {"id": "m1"}, "uploader": {"name": "up"},
                "groups": [{"name": "G1"}]}})
    }

    #[tokio::test]
    async fn extracts_chapter() {
        let (root, requests) = serve(vec![json(chapter_json()), json(manga_json())]);
        let http = http("WeebDex", root);
        let files = media(extract(
            &http,
            Url::parse("https://weebdex.org/chapter/c9").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].url.as_str(), "https://node.test/data/c9/1.png");
        let k = kw(&files[0]);
        assert_eq!(k["manga"], "Kimi to Boku");
        assert_eq!(k["manga_date"], "2024-01-02 03:04:05");
        assert_eq!(k["origin"], "ja");
        assert_eq!(k["tags"], json!(["genre:Romance"]));
        assert_eq!(k["author"], json!(["A"]));
        assert_eq!(k["artist"], json!(["B"]));
        assert_eq!(k["volume"], 2);
        assert_eq!(k["chapter"], 12);
        assert_eq!(k["chapter_minor"], ".5");
        assert_eq!(k["chapter_id"], "c9");
        assert_eq!(k["version"], 3);
        assert_eq!(k["lang"], "en");
        assert_eq!(k["uploader"], "up");
        assert_eq!(k["group"], json!(["G1"]));
        assert_eq!(k["date"], "2025-01-02 03:04:05");
        assert_eq!(k["width"], 800);
        assert_eq!(k["height"], 1200);
        assert_eq!(k["original"], true);
        let colon = if cfg!(windows) { "_" } else { ":" };
        assert_eq!(
            path(&files[0]),
            format!(
                "weebdex/Kimi to Boku/v02 c012.5{colon} Start{colon} Again/Kimi to Boku_c012.5_001.png"
            )
        );
        assert_eq!(
            archive(&files[0]),
            r#"["{chapter_id}_{version}_{page}",["c9","3","1"]]"#
        );
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /chapter/c9 "));
        assert!(
            requests[0]
                .to_ascii_lowercase()
                .contains("origin: https://weebdex.org\r\n")
        );
        assert!(requests[1].starts_with("GET /manga/m1 "));
    }

    #[tokio::test]
    async fn chapter_data_saver_uses_optimized_images() {
        let (root, requests) = serve(vec![json(chapter_json()), json(manga_json())]);
        let http = http("WeebDex", root).with_options(Some(std::sync::Arc::new(
            json!({"data-saver": true}).as_object().unwrap().clone(),
        )));
        let files = media(extract(
            &http,
            Url::parse("https://weebdex.org/chapter/c9").unwrap(),
        ))
        .await;
        assert_eq!(files[0].url.as_str(), "https://node.test/data/c9/1.webp");
        assert_eq!(kw(&files[0])["original"], false);
        assert_eq!(kw(&files[0])["width"], 400);
        requests.join().unwrap();
    }

    #[tokio::test]
    async fn manga_pages_the_chapter_list() {
        let first = json!({"data": [{"id": "c1", "chapter": "1", "volume": null}],
            "page": 1, "total": 2});
        let second = json!({"data": [{"id": "c2", "chapter": null}], "page": 2, "total": 2});
        let (root, requests) = serve(vec![json(manga_json()), json(first), json(second)]);
        let http = http("WeebDex", root);
        let children = queued(extract(
            &http,
            Url::parse("https://weebdex.org/title/m1/kimi?limit=1&tlang=de").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].subcategory(), "manga");
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /manga/m1 "));
        assert!(requests[1].starts_with("GET /manga/m1/chapters?limit=1&tlang=de&order=asc "));
        assert!(
            requests[2].starts_with("GET /manga/m1/chapters?limit=1&tlang=de&order=asc&page=2 ")
        );
    }

    #[tokio::test]
    async fn manga_children_share_the_manga_info() {
        let list = json!({"data": [{"id": "c9", "chapter": "12.5"}], "page": 1, "total": 1});
        let (root, requests) = serve(vec![json(manga_json()), json(list), json(chapter_json())]);
        let http = http("WeebDex", root);
        let files = media(extract(
            &http,
            Url::parse("https://weebdex.org/title/m1").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(kw(&files[0])["manga"], "Kimi to Boku");
        let requests = requests.join().unwrap();
        assert!(requests[1].starts_with("GET /manga/m1/chapters?limit=100&order=asc&tlang=en "));
        assert!(requests[2].starts_with("GET /chapter/c9 "));
    }
}
