//! comick.io chapters, covers and manga, mirroring
//! gallery_dl/extractor/comick.py (Next.js page data plus `api.comick.io`).
use super::{Gallery, set};
use crate::error;
use crate::sites::gdl::{
    captures, date, group, int_of, nextdata, parse_float, parse_int, parse_iso, parse_query_list,
    partition, re, str_of,
};
use crate::sites::{Events, Http, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://comick.io";
const IMAGES: &str = "https://meo.comick.pictures/";
const BASE: &str = r"^https?://(?:www\.)?comick\.io";

static CHAPTER: Route = Route::new(
    "chapter",
    super::CHAPTER_FILENAME,
    super::CHAPTER_DIRECTORY,
    "{chapter_hid}_{page}",
);
static COVERS: Route = Route::new(
    "covers",
    "{volume:>02}_{lang}.{extension}",
    &["{category}", "{manga}", "Covers"],
    "c_{id}",
);
static MANGA: Route = super::manga_route();

pub static SITE: Site = Site {
    category: "comick",
    name: "Comick",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CHAPTER, &COVERS, &MANGA],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Covers(String),
    Chapter(String, String),
    Manga(String, String),
}

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(re!(&format!(r"{BASE}/comic/([\w-]+)/cover")), url) {
        return Some(Target::Covers(group(&c, 1).into()));
    }
    let chapter = re!(&format!(
        r"{BASE}/comic/([\w-]+)/(\w+(?:-(?:chapter|volume)-[^/?#]+)?)"
    ));
    if let Some(c) = captures(chapter, url) {
        return Some(Target::Chapter(group(&c, 1).into(), group(&c, 2).into()));
    }
    let manga = re!(&format!(r"{BASE}/comic/([\w-]+)/?(?:\?([^#]+))?"));
    captures(manga, url).map(|c| Target::Manga(group(&c, 1).into(), group(&c, 2).into()))
}

/// `_manga_info`: public keywords plus the Next.js build ID.
async fn manga_info(http: &Http, slug: &str) -> crate::Result<(Map<String, Value>, String)> {
    let page = super::fetch(http, &format!("{ROOT}/comic/{slug}")).await?;
    let data = nextdata(&page)?;
    Ok((
        parse_manga(&data["props"]["pageProps"]),
        str_of(&data, "buildId").to_owned(),
    ))
}

fn parse_manga(props: &Value) -> Map<String, Value> {
    let comic = &props["comic"];
    let (mut genre, mut theme, mut format) = (Vec::new(), Vec::new(), Value::from(""));
    for item in comic["md_comic_md_genres"].as_array().into_iter().flatten() {
        let item = &item["md_genres"];
        match str_of(item, "group") {
            "Genre" => genre.push(item["name"].clone()),
            "Theme" => theme.push(item["name"].clone()),
            _ => format = item["name"].clone(),
        }
    }
    let titles = |value: &Value, outer: &str, inner: &str| -> Vec<Value> {
        value
            .as_array()
            .into_iter()
            .flatten()
            .map(|c| c[outer][inner].clone())
            .collect()
    };
    let (tags, publisher) = match &comic["mu_comics"] {
        mu if crate::sites::gdl::truthy(Some(mu)) => (
            titles(&mu["mu_comic_categories"], "mu_categories", "title"),
            titles(&mu["mu_comic_publishers"], "mu_publishers", "title"),
        ),
        _ => (Vec::new(), Vec::new()),
    };
    let names = |key: &str| -> Vec<Value> {
        props[key]
            .as_array()
            .into_iter()
            .flatten()
            .map(|a| a["name"].clone())
            .collect()
    };
    let mut data = Map::new();
    set(&mut data, "manga", comic["title"].clone());
    set(&mut data, "manga_id", comic["id"].clone());
    set(&mut data, "manga_hid", comic["hid"].clone());
    set(&mut data, "manga_slug", comic["slug"].clone());
    let md_titles: Vec<Value> = comic["md_titles"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|t| t["title"].clone())
        .collect();
    set(&mut data, "manga_titles", md_titles);
    set(&mut data, "artist", names("artists"));
    set(&mut data, "author", names("authors"));
    set(&mut data, "genre", genre);
    set(&mut data, "theme", theme);
    set(&mut data, "format", format);
    set(&mut data, "tags", tags);
    set(&mut data, "publisher", publisher);
    set(&mut data, "published", int_of(comic.get("year")));
    set(&mut data, "description", comic["desc"].clone());
    set(&mut data, "demographic", props["demographic"].clone());
    set(&mut data, "origin", comic["iso639_1"].clone());
    set(&mut data, "mature", props["matureContent"].clone());
    set(&mut data, "rating", comic["content_rating"].clone());
    set(&mut data, "rank", comic["follow_rank"].clone());
    set(
        &mut data,
        "score",
        parse_float(&crate::sites::scalar(&comic["bayesian_rating"])),
    );
    set(
        &mut data,
        "status",
        if comic["status"] == 2 {
            "Complete"
        } else {
            "Ongoing"
        },
    );
    set(&mut data, "links", comic["links"].clone());
    data
}

async fn chapter_info(http: &Http, build: &str, slug: &str, chstr: &str) -> crate::Result<Value> {
    let mut url = http.api(&format!(
        "{ROOT}/_next/data/{build}/comic/{slug}/{chstr}.json"
    ))?;
    url.query_pairs_mut()
        .append_pair("slug", slug)
        .append_pair("chapter", chstr);
    Ok(http.json(&url).await?["pageProps"].clone())
}

/// `ch["chap"].partition(".")` of a possibly missing chapter number.
fn chapter_number(value: &Value) -> (i64, String) {
    match value.as_str().filter(|c| !c.is_empty()) {
        Some(chapter) => {
            let (number, sep, minor) = partition(chapter, ".");
            (parse_int(number), format!("{sep}{minor}"))
        }
        None => (0, String::new()),
    }
}

fn first19(value: &Value) -> String {
    crate::sites::scalar(value).chars().take(19).collect()
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Comick URL"));
    };
    crate::sites::once(async move {
        Ok(match target {
            Target::Covers(slug) => {
                let (manga, _) = manga_info(http, &slug).await?;
                let slug = manga
                    .get("manga_slug")
                    .map(crate::sites::scalar)
                    .unwrap_or(slug);
                let page = super::fetch(http, &format!("{ROOT}/comic/{slug}/cover")).await?;
                let data = nextdata(&page)?;
                let mut covers = data["props"]["pageProps"]["comic"]["md_covers"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                covers.reverse();
                let pages = covers
                    .into_iter()
                    .map(|cover| {
                        let mut image = Map::new();
                        set(&mut image, "id", cover["id"].clone());
                        set(&mut image, "width", cover["w"].clone());
                        set(&mut image, "height", cover["h"].clone());
                        set(&mut image, "size", cover["s"].clone());
                        set(&mut image, "lang", cover["locale"].clone());
                        set(&mut image, "volume", int_of(cover.get("vol")));
                        let url = format!("{IMAGES}{}", crate::sites::scalar(&cover["b2key"]));
                        set(&mut image, "cover", cover);
                        crate::sites::art::nameext_into(&url, &mut image);
                        (url, Some(image))
                    })
                    .collect();
                crate::sites::events(Gallery::numbered(&COVERS, manga, pages).events(&SITE, http))
            }
            Target::Chapter(mut slug, mut chstr) => {
                let (mut manga, mut build) = manga_info(http, &slug).await?;
                let mut refreshed = false;
                let props = loop {
                    match chapter_info(http, &build, &slug, &chstr).await {
                        Ok(props) => match props.get("__N_REDIRECT").and_then(Value::as_str) {
                            Some(path) => {
                                let mut parts = path.rsplitn(3, '/');
                                chstr = parts.next().unwrap_or_default().to_owned();
                                slug = parts.next().unwrap_or_default().to_owned();
                            }
                            None => break props,
                        },
                        // A stale Next.js build ID answers 404; refresh it once.
                        Err(_) if !refreshed => {
                            refreshed = true;
                            (manga, build) = manga_info(http, &slug).await?;
                        }
                        Err(e) => return Err(e),
                    }
                };
                let ch = &props["chapter"];
                let (chapter, minor) = chapter_number(&ch["chap"]);
                let mut data = manga;
                set(&mut data, "title", props["chapTitle"].clone());
                set(&mut data, "volume", int_of(ch.get("vol")));
                set(&mut data, "chapter", chapter);
                set(&mut data, "chapter_minor", minor);
                set(&mut data, "chapter_id", ch["id"].clone());
                set(&mut data, "chapter_hid", ch["hid"].clone());
                set(&mut data, "chapter_string", chstr.as_str());
                set(&mut data, "group", ch["group_name"].clone());
                set(
                    &mut data,
                    "date",
                    date(parse_iso(&first19(&ch["created_at"]))),
                );
                set(
                    &mut data,
                    "date_updated",
                    date(parse_iso(&first19(&ch["updated_at"]))),
                );
                set(&mut data, "lang", ch["lang"].clone());
                let images = ch["md_images"].as_array().cloned().unwrap_or_default();
                if images
                    .iter()
                    .all(|img| !crate::sites::gdl::truthy(img.get("b2key")))
                {
                    return Err(error(format!(
                        "{chstr}: Broken Chapter (missing 'b2key' for all pages)"
                    )));
                }
                let pages = images
                    .into_iter()
                    .map(|img| {
                        let mut image = Map::new();
                        set(&mut image, "width", img["w"].clone());
                        set(&mut image, "height", img["h"].clone());
                        set(&mut image, "size", img["s"].clone());
                        set(&mut image, "optimized", img["optimized"].clone());
                        (
                            format!("{IMAGES}{}", crate::sites::scalar(&img["b2key"])),
                            Some(image),
                        )
                    })
                    .collect();
                crate::sites::events(Gallery::chapter(&CHAPTER, data, pages).events(&SITE, http))
            }
            Target::Manga(slug, query) => {
                let (manga, _) = manga_info(http, &slug).await?;
                let chapters = chapter_list(http, &manga, &query).await?;
                super::queue_urls(http, &SITE, &MANGA, chapters)
            }
        })
    })
}

async fn chapter_list(
    http: &Http,
    manga: &Map<String, Value>,
    query: &str,
) -> crate::Result<Vec<(Url, Map<String, Value>)>> {
    let slug = manga
        .get("manga_slug")
        .map(crate::sites::scalar)
        .unwrap_or_default();
    let hid = manga
        .get("manga_hid")
        .map(crate::sites::scalar)
        .unwrap_or_default();
    let query = parse_query_list(query, &["lang"]);
    let lang = match query
        .get("lang")
        .filter(|l| crate::sites::gdl::truthy(Some(l)))
        .or_else(|| http.option("lang"))
    {
        Some(Value::Array(list)) => Some(
            list.iter()
                .map(crate::sites::scalar)
                .collect::<Vec<_>>()
                .join(","),
        ),
        Some(Value::String(lang)) if !lang.is_empty() => Some(lang.clone()),
        _ => None,
    };
    let mut page = query.get("page").map_or(1, |p| {
        crate::sites::art::parse_int(&crate::sites::scalar(p), 1)
    });
    let mut params: Vec<(String, String)> = Vec::new();
    if let Some(lang) = lang {
        params.push(("lang".into(), lang));
    }
    let non_empty = |key: &str| {
        query
            .get(key)
            .map(crate::sites::scalar)
            .filter(|v| !v.is_empty())
    };
    if let Some(order) = non_empty("date-order") {
        params.push(("date-order".into(), order));
    } else if let Some(order) = non_empty("chap-order") {
        params.push(("chap-order".into(), order));
    } else {
        let order = if http.option_bool("chapter-reverse", false) {
            "0"
        } else {
            "1"
        };
        params.push(("chap-order".into(), order.into()));
    }
    let group = query
        .get("group")
        .map(crate::sites::scalar)
        .filter(|g| g != "0");
    let mut results = Vec::new();
    loop {
        let mut url = http.api(&format!("https://api.comick.io/comic/{hid}/chapters"))?;
        url.query_pairs_mut()
            .extend_pairs(params.iter())
            .append_pair("page", &page.to_string());
        let data = http
            .json_with(
                &url,
                &[
                    ("Origin", "https://comick.io"),
                    ("Sec-Fetch-Dest", "empty"),
                    ("Sec-Fetch-Mode", "cors"),
                    ("Sec-Fetch-Site", "same-site"),
                ],
            )
            .await?;
        let limit = int_of(data.get("limit"));
        for ch in data["chapters"].as_array().into_iter().flatten() {
            if let Some(group) = &group {
                let names = ch["group_name"]
                    .as_array()
                    .map(|g| g.iter().map(crate::sites::scalar).collect::<Vec<_>>());
                if !names.is_some_and(|names| names.contains(group)) {
                    continue;
                }
            }
            let mut entry = ch.as_object().cloned().unwrap_or_default();
            entry.extend(manga.clone());
            let hid = crate::sites::scalar(&ch["hid"]);
            let lang = crate::sites::scalar(&ch["lang"]);
            let url = if let Some(chapter) = ch["chap"].as_str().filter(|c| !c.is_empty()) {
                let (number, minor) = chapter_number(&ch["chap"]);
                set(&mut entry, "volume", int_of(ch.get("vol")));
                set(&mut entry, "chapter", number);
                set(&mut entry, "chapter_minor", minor);
                format!("{ROOT}/comic/{slug}/{hid}-chapter-{chapter}-{lang}")
            } else if let Some(volume) = ch["vol"].as_str().filter(|v| !v.is_empty()) {
                set(&mut entry, "volume", parse_int(volume));
                set(&mut entry, "chapter", 0);
                set(&mut entry, "chapter_minor", "");
                format!("{ROOT}/comic/{slug}/{hid}-volume-{volume}-{lang}")
            } else {
                set(&mut entry, "volume", 0);
                set(&mut entry, "chapter", 0);
                set(&mut entry, "chapter_minor", "");
                format!("{ROOT}/comic/{slug}/{hid}")
            };
            if let Ok(url) = Url::parse(&url) {
                results.push((url, entry));
            }
        }
        if int_of(data.get("total")) <= limit * page {
            break;
        }
        page += 1;
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::super::tests::{archive, path};
    use super::*;
    use crate::sites::testing::{children, html, http, json as reply, keywords, media, serve};
    use serde_json::json;

    fn manga_page(build: &str) -> String {
        let data = json!({"buildId": build, "props": {"pageProps": {
            "comic": {"title": "Neko no Oshigoto", "id": 5, "hid": "H5", "slug": "neko-no-oshigoto",
                "md_titles": [{"title": "Cat"}], "year": 2020, "desc": "D", "iso639_1": "ja",
                "content_rating": "safe", "follow_rank": 9, "bayesian_rating": "8.5", "status": 2, "links": {},
                "md_comic_md_genres": [{"md_genres": {"group": "Genre", "name": "Comedy"}},
                    {"md_genres": {"group": "Format", "name": "Web Comic"}}],
                "mu_comics": null, "md_covers": [{"id": 1, "w": 1, "h": 2, "s": 3, "locale": "ja", "vol": "1", "b2key": "a.jpg"},
                    {"id": 2, "w": 1, "h": 2, "s": 3, "locale": "en", "vol": "2", "b2key": "b.png"}]},
            "artists": [{"name": "Art"}], "authors": [{"name": "Auth"}], "demographic": "Seinen", "matureContent": false}}});
        format!(r#"<script id="__NEXT_DATA__" type="application/json">{data}</script>"#)
    }

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://comick.io/comic/neko-no-oshigoto/L7TaJB4n-chapter-10.5-en"),
            Some(Target::Chapter(
                "neko-no-oshigoto".into(),
                "L7TaJB4n-chapter-10.5-en".into()
            ))
        );
        assert_eq!(
            t("https://comick.io/comic/00-the-100/Zqu59ZKD"),
            Some(Target::Chapter("00-the-100".into(), "Zqu59ZKD".into()))
        );
        assert_eq!(
            t("https://comick.io/comic/x/cover"),
            Some(Target::Covers("x".into()))
        );
        assert_eq!(
            t(
                "https://comick.io/comic/kobayashi?lang=pt-br&group=Amadeus+Scans&chap-order=&date-order=1&page=3#chapter-header"
            ),
            Some(Target::Manga(
                "kobayashi".into(),
                "lang=pt-br&group=Amadeus+Scans&chap-order=&date-order=1&page=3".into()
            ))
        );
    }

    #[tokio::test]
    async fn extracts_chapter_after_build_refresh_and_redirect() {
        let (root, server) = serve(vec![
            html(&manga_page("old")),
            crate::sites::testing::response("404 Not Found", "text/html", b"<html>"),
            html(&manga_page("new")),
            reply(
                json!({"pageProps": {"__N_REDIRECT": "/comic/neko-no-oshigoto/L7-chapter-10.5-en"}}),
            ),
            reply(
                json!({"pageProps": {"chapTitle": "T", "chapter": {"chap": "10.5", "vol": "2", "id": 7, "hid": "L7",
                "group_name": ["G"], "created_at": "2021-01-02T03:04:05.000000+00:00", "updated_at": "2021-01-03T00:00:00Z",
                "lang": "en", "md_images": [{"b2key": "1.jpg", "w": 1, "h": 2, "s": 3, "optimized": 1},
                                            {"b2key": "2.jpg", "w": 1, "h": 2, "s": 3, "optimized": 1}]}}}),
            ),
        ]);
        let http = http("comick", root);
        let files = media(extract(
            &http,
            Url::parse("https://comick.io/comic/neko-no-oshigoto/OLD-chapter-10.5-en").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].url.as_str(), "https://meo.comick.pictures/1.jpg");
        let k = keywords(&files[0]);
        assert_eq!(k["format"], "Web Comic");
        assert_eq!(k["genre"], json!(["Comedy"]));
        assert_eq!(k["status"], "Complete");
        assert_eq!(k["score"], 8.5);
        assert_eq!(k["chapter_string"], "L7-chapter-10.5-en");
        assert_eq!(k["date"], "2021-01-02 03:04:05");
        assert_eq!(
            path(&files[1]),
            "comick/Neko no Oshigoto/v02 c010.5: T/Neko no Oshigoto_c010.5_002.jpg"
        );
        assert_eq!(archive(&files[0]), r#"["{chapter_hid}_{page}",["L7","1"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /_next/data/old/comic/neko-no-oshigoto/OLD-chapter-10.5-en.json?slug=neko-no-oshigoto&chapter=OLD-chapter-10.5-en "));
        assert!(
            requests[3]
                .starts_with("GET /_next/data/new/comic/neko-no-oshigoto/OLD-chapter-10.5-en.json")
        );
        assert!(
            requests[4]
                .starts_with("GET /_next/data/new/comic/neko-no-oshigoto/L7-chapter-10.5-en.json")
        );
    }

    #[tokio::test]
    async fn extracts_covers_oldest_first() {
        let (root, server) = serve(vec![html(&manga_page("b")), html(&manga_page("b"))]);
        let http = http("comick", root);
        let files = media(extract(
            &http,
            Url::parse("https://comick.io/comic/neko-no-oshigoto/cover").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(path(&files[0]), "comick/Neko no Oshigoto/Covers/02_en.png");
        assert_eq!(archive(&files[1]), r#"["c_{id}",["1"]]"#);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn manga_filters_groups_and_pages() {
        let (root, server) = serve(vec![
            html(&manga_page("b")),
            reply(
                json!({"limit": 1, "total": 2, "chapters": [{"hid": "a", "chap": "1", "vol": null, "lang": "pt-br", "group_name": ["Amadeus Scans"]}]}),
            ),
            reply(
                json!({"limit": 1, "total": 2, "chapters": [{"hid": "b", "chap": null, "vol": "3", "lang": "pt-br", "group_name": ["Other"]}]}),
            ),
        ]);
        let http = http("comick", root);
        let queued = children(extract(&http, Url::parse("https://comick.io/comic/neko-no-oshigoto?lang=pt-br&group=Amadeus+Scans&chap-order=&date-order=1").unwrap())).await;
        assert_eq!(queued.len(), 1);
        let requests = server.join().unwrap();
        assert!(
            requests[1].starts_with("GET /comic/H5/chapters?lang=pt-br&date-order=1&page=1 "),
            "{}",
            requests[1]
        );
        assert!(requests[2].contains("&page=2 "));
    }
}
