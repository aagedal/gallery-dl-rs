//! WeebDex chapters and manga, mirroring gallery_dl/extractor/weebdex.py
//! through the `api.weebdex.org` JSON API.
use super::{Gallery, set};
use crate::error;
use crate::sites::gdl::{
    captures, date, group, int_of, parse_iso, parse_query, partition, re, str_of,
};
use crate::sites::{Events, Http, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://weebdex.org";
const API: &str = "https://api.weebdex.org";
const HEADERS: &[(&str, &str)] = &[
    ("Referer", "https://weebdex.org/"),
    ("Origin", "https://weebdex.org"),
    ("Sec-Fetch-Dest", "empty"),
    ("Sec-Fetch-Mode", "cors"),
    ("Sec-Fetch-Site", "same-site"),
];

static CHAPTER: Route = Route::new(
    "chapter",
    super::CHAPTER_FILENAME,
    super::CHAPTER_DIRECTORY,
    "{chapter_id}_{version}_{page}",
);
static MANGA: Route = super::manga_route();

pub static SITE: Site = Site {
    category: "weebdex",
    name: "WeebDex",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CHAPTER, &MANGA],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Chapter(String),
    /// Manga ID and the optional query of a slugged title URL.
    Manga(String, String),
}

fn target(url: &Url) -> Option<Target> {
    let chapter = re!(r"^https?://weebdex\.org/chapter/(\w+)");
    if let Some(c) = captures(chapter, url) {
        return Some(Target::Chapter(group(&c, 1).into()));
    }
    let manga = re!(r"^https?://weebdex\.org/title/(\w+)(?:/[^/?#]+/?\?([^#]+))?");
    captures(manga, url).map(|c| Target::Manga(group(&c, 1).into(), group(&c, 2).into()))
}

async fn api(http: &Http, url: &Url) -> crate::Result<Value> {
    http.json_with(url, HEADERS).await
}

/// `_manga_info`.
async fn manga_info(http: &Http, id: &str) -> crate::Result<Map<String, Value>> {
    let manga = api(http, &http.api(&format!("{API}/manga/{id}"))?).await?;
    let rel = &manga["relationships"];
    let names = |key: &str| -> Vec<Value> {
        rel[key]
            .as_array()
            .into_iter()
            .flatten()
            .map(|a| a["name"].clone())
            .collect()
    };
    let tags: Vec<String> = rel["tags"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|t| {
            format!(
                "{}:{}",
                crate::sites::scalar(&t["group"]),
                crate::sites::scalar(&t["name"])
            )
        })
        .collect();
    let mut data = Map::new();
    let field = |key: &str| manga.get(key).cloned().unwrap_or(Value::Null);
    set(&mut data, "manga", field("title"));
    set(&mut data, "manga_id", field("id"));
    set(
        &mut data,
        "manga_date",
        date(parse_iso(str_of(&manga, "created_at"))),
    );
    set(&mut data, "year", field("year"));
    set(&mut data, "status", field("status"));
    set(&mut data, "origin", field("language"));
    set(&mut data, "description", field("description"));
    set(&mut data, "demographic", field("demographic"));
    set(&mut data, "tags", tags);
    set(&mut data, "author", names("authors"));
    set(&mut data, "artist", names("artists"));
    Ok(data)
}

/// `chapter.partition(".")` of a possibly missing chapter number.
fn chapter_number(value: &Value) -> (i64, String) {
    match value.as_str() {
        Some(text) => {
            let (chapter, sep, minor) = partition(text, ".");
            (
                crate::sites::gdl::parse_int(chapter),
                format!("{sep}{minor}"),
            )
        }
        None => (0, String::new()),
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported WeebDex URL"));
    };
    crate::sites::once(async move {
        Ok(match target {
            Target::Chapter(cid) => {
                let mut chapter = api(http, &http.api(&format!("{API}/chapter/{cid}"))?).await?;
                let rel = chapter
                    .as_object_mut()
                    .and_then(|c| c.remove("relationships"))
                    .unwrap_or(Value::Null);
                let mut data = manga_info(http, str_of(&rel["manga"], "id")).await?;
                let (number, minor) = chapter_number(&chapter["chapter"]);
                set(
                    &mut data,
                    "title",
                    chapter.get("title").cloned().unwrap_or_else(|| "".into()),
                );
                set(
                    &mut data,
                    "version",
                    chapter.get("version").cloned().unwrap_or_else(|| 0.into()),
                );
                set(&mut data, "volume", int_of(chapter.get("volume")));
                set(&mut data, "chapter", number);
                set(&mut data, "chapter_minor", minor);
                set(&mut data, "chapter_id", cid.as_str());
                set(
                    &mut data,
                    "date",
                    date(parse_iso(str_of(&chapter, "created_at"))),
                );
                set(
                    &mut data,
                    "date_updated",
                    date(parse_iso(str_of(&chapter, "updated_at"))),
                );
                set(
                    &mut data,
                    "lang",
                    chapter.get("language").cloned().unwrap_or(Value::Null),
                );
                set(
                    &mut data,
                    "uploader",
                    rel.get("uploader")
                        .map_or_else(|| "".into(), |u| u["name"].clone()),
                );
                let groups: Vec<Value> = rel["groups"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|g| g["name"].clone())
                    .collect();
                set(&mut data, "group", groups);
                let base = format!(
                    "{}/data/{}/",
                    crate::sites::scalar(&chapter["node"]),
                    crate::sites::scalar(&chapter["id"])
                );
                let saver = http.option_bool("data-saver", false);
                let pages = chapter[if saver { "data_optimized" } else { "data" }]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|page| {
                        let mut image = Map::new();
                        set(&mut image, "width", page["dimensions"][0].clone());
                        set(&mut image, "height", page["dimensions"][1].clone());
                        set(&mut image, "original", !saver);
                        (
                            format!("{base}{}", crate::sites::scalar(&page["name"])),
                            Some(image),
                        )
                    })
                    .collect();
                crate::sites::events(
                    Gallery::chapter(&CHAPTER, data, pages)
                        .id(cid)
                        .events(&SITE, http),
                )
            }
            Target::Manga(mid, query) => {
                let manga = manga_info(http, &mid).await?;
                let mut params: Vec<(String, String)> = parse_query(&query)
                    .into_iter()
                    .map(|(k, v)| (k, crate::sites::scalar(&v)))
                    .collect();
                let has =
                    |params: &[(String, String)], key: &str| params.iter().any(|(k, _)| k == key);
                if !has(&params, "limit") {
                    params.push(("limit".into(), "100".into()));
                }
                if !has(&params, "order") {
                    params.push(("order".into(), "asc".into()));
                }
                if !has(&params, "tlang") {
                    let lang = http.option_str("lang").unwrap_or("en").to_owned();
                    params.push(("tlang".into(), lang));
                }
                let limit = params
                    .iter()
                    .find(|(k, _)| k == "limit")
                    .map_or(100, |(_, v)| crate::sites::gdl::parse_int(v));
                let mut results = Vec::new();
                loop {
                    let mut url = http.api(&format!("{API}/manga/{mid}/chapters"))?;
                    url.query_pairs_mut().extend_pairs(params.iter());
                    let data = api(http, &url).await?;
                    for ch in data["data"].as_array().into_iter().flatten() {
                        let mut ch = ch.as_object().cloned().unwrap_or_default();
                        let (number, minor) =
                            chapter_number(ch.get("chapter").unwrap_or(&Value::Null));
                        let volume = int_of(ch.get("volume"));
                        set(&mut ch, "volume", volume);
                        set(&mut ch, "chapter", number);
                        set(&mut ch, "chapter_minor", minor);
                        ch.extend(manga.clone());
                        let id = ch.get("id").map(crate::sites::scalar).unwrap_or_default();
                        if let Ok(url) = Url::parse(&format!("{ROOT}/chapter/{id}")) {
                            results.push((url, ch));
                        }
                    }
                    let page = int_of(data.get("page"));
                    if int_of(data.get("total")) <= page * limit {
                        break;
                    }
                    let next = (page + 1).to_string();
                    match params.iter_mut().find(|(k, _)| k == "page") {
                        Some((_, v)) => *v = next,
                        None => params.push(("page".into(), next)),
                    }
                }
                super::queue_chapters(http, &SITE, &MANGA, results, false)
            }
        })
    })
}

#[cfg(test)]
mod tests {
    use super::super::tests::{archive, path};
    use super::*;
    use crate::sites::testing::{children, http, json, keywords, media, serve};
    use serde_json::json;

    fn manga() -> Value {
        json!({"id": "m1", "title": "Yotsuba&!", "created_at": "2020-01-01T00:00:00Z", "year": 2003,
            "status": "ongoing", "language": "ja", "description": "D", "demographic": "seinen",
            "relationships": {"tags": [{"group": "genre", "name": "Comedy"}],
                "authors": [{"name": "Azuma"}], "artists": [{"name": "Azuma"}]}})
    }

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://weebdex.org/chapter/f6c0awnrba"),
            Some(Target::Chapter("f6c0awnrba".into()))
        );
        assert_eq!(
            t("https://weebdex.org/title/f29vfu3dd4/yotsuba-to"),
            Some(Target::Manga("f29vfu3dd4".into(), String::new()))
        );
        assert_eq!(
            t("https://weebdex.org/title/3o0icxno26/ani-datta-mono?group=j0fsj3oem3&order=desc"),
            Some(Target::Manga(
                "3o0icxno26".into(),
                "group=j0fsj3oem3&order=desc".into()
            ))
        );
        assert_eq!(t("https://www.weebdex.org/chapter/x"), None);
    }

    #[tokio::test]
    async fn extracts_chapter() {
        let chapter = json!({"id": "c1", "node": "https://s1.weebdex.net", "chapter": "12.5", "volume": "3",
            "title": "T", "version": 2, "language": "en", "created_at": "2024-02-03T04:05:06Z",
            "data": [{"name": "1.png", "dimensions": [800, 1200]}, {"name": "2.png", "dimensions": [1, 2]}],
            "data_optimized": [],
            "relationships": {"manga": {"id": "m1"}, "uploader": {"name": "U"}, "groups": [{"name": "G"}]}});
        let (root, server) = serve(vec![json(chapter), json(manga())]);
        let http = http("weebdex", root);
        let files = media(extract(
            &http,
            Url::parse("https://weebdex.org/chapter/c1").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].url.as_str(),
            "https://s1.weebdex.net/data/c1/1.png"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["tags"], json!(["genre:Comedy"]));
        assert_eq!(k["group"], json!(["G"]));
        assert_eq!(k["width"], 800);
        assert_eq!(k["original"], true);
        assert_eq!(k["uploader"], "U");
        assert_eq!(
            path(&files[1]),
            "weebdex/Yotsuba&!/v03 c012.5: T/Yotsuba&!_c012.5_002.png"
        );
        assert_eq!(
            archive(&files[1]),
            r#"["{chapter_id}_{version}_{page}",["c1","2","2"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /chapter/c1 "));
        assert!(requests[1].starts_with("GET /manga/m1 "));
        assert!(
            requests[0]
                .to_ascii_lowercase()
                .contains("origin: https://weebdex.org")
        );
    }

    #[tokio::test]
    async fn manga_pages_chapters() {
        let (root, server) = serve(vec![
            json(manga()),
            json(json!({"data": [{"id": "a", "chapter": "1"}], "page": 1, "total": 2})),
            json(json!({"data": [{"id": "b", "chapter": null}], "page": 2, "total": 2})),
        ]);
        let http = http("weebdex", root);
        let queued = children(extract(
            &http,
            Url::parse("https://weebdex.org/title/m1/slug/?limit=1&group=g").unwrap(),
        ))
        .await;
        assert_eq!(queued.len(), 2);
        let requests = server.join().unwrap();
        assert!(
            requests[1].starts_with("GET /manga/m1/chapters?limit=1&group=g&order=asc&tlang=en "),
            "{}",
            requests[1]
        );
        assert!(requests[2].contains("&page=2 "));
    }
}
