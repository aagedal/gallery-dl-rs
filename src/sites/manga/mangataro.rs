//! MangaTaro chapters and manga, mirroring gallery_dl/extractor/mangataro.py.
use super::{Gallery, set};
use crate::error;
use crate::sites::gdl::{
    captures, date, extr, group, int_of, jsonld, parse_int, parse_iso, partition, re, remove_html,
    rpartition, split_html, str_of, unescape,
};
use crate::sites::{Events, Http, Route, Site};
use md5::Digest;
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://mangataro.org";

static CHAPTER: Route = super::chapter_route();
static MANGA: Route = super::manga_route();

pub static SITE: Site = Site {
    category: "mangataro",
    name: "MangaTaro",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CHAPTER, &MANGA],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    /// Path, manga slug and chapter ID.
    Chapter(String, String, String),
    Manga(String),
}

fn target(url: &Url) -> Option<Target> {
    let chapter = re!(r"^https?://mangataro\.org(/read/([^/?#]+)/(?:[^/?#]*-)?(\d+))");
    if let Some(caps) = captures(chapter, url) {
        return Some(Target::Chapter(
            group(&caps, 1).into(),
            group(&caps, 2).into(),
            group(&caps, 3).into(),
        ));
    }
    let manga = re!(r"^https?://mangataro\.org/manga/([^/?#]+)");
    captures(manga, url).map(|caps| Target::Manga(group(&caps, 1).into()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported MangaTaro URL"));
    };
    crate::sites::once(async move {
        Ok(match target {
            Target::Chapter(path, slug, chapter_id) => {
                let page = super::fetch(http, &format!("{ROOT}{path}")).await?;
                let mut data = manga_info(http, &slug).await?;
                data.extend(chapter_metadata(&page, &chapter_id)?);
                let images = http
                    .json(&http.api(&format!(
                        "{ROOT}/auth/chapter-content?chapter_id={chapter_id}"
                    ))?)
                    .await?;
                let pages = images["images"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|url| (crate::sites::scalar(url), None))
                    .collect();
                crate::sites::events(Gallery::chapter(&CHAPTER, data, pages).events(&SITE, http))
            }
            Target::Manga(slug) => {
                let manga = manga_info(http, &slug).await?;
                let chapters = chapter_list(http, &manga).await?;
                super::queue_chapters(http, &SITE, &MANGA, chapters, true)
            }
        })
    })
}

/// `_manga_info`.
async fn manga_info(http: &Http, slug: &str) -> crate::Result<Map<String, Value>> {
    let page = super::fetch(http, &format!("{ROOT}/manga/{slug}")).await?;
    let manga = jsonld(&page)?;
    let name = rpartition(rpartition(str_of(&manga, "name"), " | ").0, " ").0;
    let authors: Vec<String> = str_of(&manga["author"], "name")
        .split(", ")
        .map(str::to_owned)
        .collect();
    let mut data = Map::new();
    set(&mut data, "manga", name);
    set(&mut data, "manga_id", extr(&page, "data-manga-id=\"", "\""));
    set(&mut data, "manga_url", manga["url"].clone());
    set(&mut data, "cover", manga["image"].clone());
    set(&mut data, "author", authors);
    set(&mut data, "genre", manga["genre"].clone());
    set(&mut data, "status", manga["status"].clone());
    set(
        &mut data,
        "description",
        unescape(extr(
            &page,
            "id=\"description-content-tab\">",
            "</div></div>",
        )),
    );
    set(
        &mut data,
        "tags",
        split_html(extr(&page, ">Genres</h4>", "</div>")),
    );
    set(
        &mut data,
        "publisher",
        remove_html(extr(&page, ">Serialization</h4>", "</div>")),
    );
    Ok(data)
}

fn chapter_metadata(page: &str, chapter_id: &str) -> crate::Result<Map<String, Value>> {
    let comic = jsonld(page)?["@graph"][0].clone();
    let position = comic["position"].as_f64().unwrap_or(0.0);
    let minor = position - position.trunc();
    let description = str_of(&comic, "description");
    let parts: Vec<&str> = description.splitn(4, " - ").collect();
    let mut data = Map::new();
    set(
        &mut data,
        "title",
        if parts.len() > 3 { parts[1] } else { "" },
    );
    set(&mut data, "chapter", position.trunc() as i64);
    let minor = if minor == 0.0 {
        String::new()
    } else {
        // str(round(minor, 5))[1:]
        let rounded = (minor * 1e5).round() / 1e5;
        let text = format!("{rounded}");
        text.get(1..).unwrap_or_default().to_owned()
    };
    set(&mut data, "chapter_minor", minor);
    set(&mut data, "chapter_id", parse_int(chapter_id));
    set(&mut data, "chapter_url", comic["url"].clone());
    set(
        &mut data,
        "date",
        date(parse_iso(str_of(&comic, "datePublished"))),
    );
    set(
        &mut data,
        "date_updated",
        date(parse_iso(str_of(&comic, "dateModified"))),
    );
    Ok(data)
}

/// `_update_params`: the time-derived request signature.
fn signature(ts: i64) -> String {
    let when =
        time::OffsetDateTime::from_unix_timestamp(ts).unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    let secret = format!(
        "{ts}mng_ch_{:04}{:02}{:02}{:02}",
        when.year(),
        u8::from(when.month()),
        when.day(),
        when.hour()
    );
    let digest = md5::Md5::digest(secret.as_bytes());
    digest
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()[..16]
        .to_owned()
}

async fn chapter_list(
    http: &Http,
    manga: &Map<String, Value>,
) -> crate::Result<Vec<(Url, Map<String, Value>)>> {
    let referer = manga
        .get("manga_url")
        .map(crate::sites::scalar)
        .unwrap_or_default();
    let manga_id = manga
        .get("manga_id")
        .map(crate::sites::scalar)
        .unwrap_or_default();
    let mut offset = 0i64;
    let mut results = Vec::new();
    loop {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        let mut url = http.api(&format!("{ROOT}/auth/manga-chapters"))?;
        url.query_pairs_mut()
            .append_pair("manga_id", &manga_id)
            .append_pair("offset", &offset.to_string())
            .append_pair("limit", "500")
            .append_pair("order", "DESC")
            .append_pair("_t", &signature(ts))
            .append_pair("_ts", &ts.to_string());
        let data = http
            .json_with(
                &url,
                &[
                    ("Referer", &referer),
                    ("Sec-Fetch-Dest", "empty"),
                    ("Sec-Fetch-Mode", "cors"),
                    ("Sec-Fetch-Site", "same-origin"),
                ],
            )
            .await?;
        for ch in data["chapters"].as_array().into_iter().flatten() {
            let mut ch = ch.as_object().cloned().unwrap_or_default();
            let number = crate::sites::scalar(&ch["chapter"]);
            let (chapter, sep, minor) = partition(&number, ".");
            let mut entry = manga.clone();
            set(&mut entry, "chapter_id", int_of(ch.remove("id").as_ref()));
            let child = ch
                .get("url")
                .and_then(Value::as_str)
                .and_then(|u| Url::parse(u).ok());
            entry.extend(ch);
            set(&mut entry, "chapter", parse_int(chapter));
            set(
                &mut entry,
                "chapter_minor",
                if sep.is_empty() {
                    String::new()
                } else {
                    format!(".{minor}")
                },
            );
            if let Some(url) = child {
                results.push((url, entry));
            }
        }
        if !crate::sites::gdl::truthy(data.get("has_more")) {
            break;
        }
        let limit = int_of(data.get("limit"));
        offset += if limit == 0 { 500 } else { limit };
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::super::tests::path;
    use super::*;
    use crate::sites::testing::{children, html, http, json, keywords, media, serve};
    use serde_json::json;

    const MANGA_PAGE: &str = r#"<script type="application/ld+json">{"name":"Majo to Youhei Manga | Read Online",
        "url":"https://mangataro.org/manga/majo-to-youhei","image":"https://c.test/c.jpg",
        "author":{"name":"Chouhoukiteki, Kanzaki"},"genre":["Fantasy"],"status":"Ongoing"}</script>
        <div data-manga-id="77"></div><div id="description-content-tab">Witch &amp; merc</div></div>
        <h4>Genres</h4><a>Fantasy</a></div><h4>Serialization</h4><a>Pub</a></div>"#;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://mangataro.org/read/majo-to-youhei/ch8-356833"),
            Some(Target::Chapter(
                "/read/majo-to-youhei/ch8-356833".into(),
                "majo-to-youhei".into(),
                "356833".into()
            ))
        );
        assert!(
            matches!(t("https://mangataro.org/read/x/ch12-2-337633"), Some(Target::Chapter(_, _, id)) if id == "337633")
        );
        assert_eq!(
            t("https://mangataro.org/manga/lookism"),
            Some(Target::Manga("lookism".into()))
        );
        assert_eq!(t("https://www.mangataro.org/manga/lookism"), None);
    }

    #[test]
    fn signs_requests_like_upstream() {
        // md5("1700000000mng_ch_2023111422")[:16]
        assert_eq!(signature(1_700_000_000), "86e204ec6bff4315");
    }

    #[tokio::test]
    async fn extracts_chapter() {
        let chapter = r#"<script type="application/ld+json">{"@graph":[{"position":12.2,
            "description":"Majo - The Title - x - y","url":"https://mangataro.org/read/majo/ch12-2-1",
            "datePublished":"2025-01-02T03:04:05+00:00","dateModified":"2025-01-03T00:00:00+00:00"}]}</script>"#;
        let (root, server) = serve(vec![
            html(chapter),
            html(MANGA_PAGE),
            json(json!({"images": ["https://img.test/1.webp", "https://img.test/2.webp"]})),
        ]);
        let http = http("mangataro", root);
        let files = media(extract(
            &http,
            Url::parse("https://mangataro.org/read/majo/ch12-2-1").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[0]);
        assert_eq!(k["manga"], "Majo to Youhei");
        assert_eq!(k["author"], json!(["Chouhoukiteki", "Kanzaki"]));
        assert_eq!(k["title"], "The Title");
        assert_eq!(k["chapter"], 12);
        assert_eq!(k["chapter_minor"], ".2");
        assert_eq!(k["chapter_id"], 1);
        assert_eq!(k["date"], "2025-01-02 03:04:05");
        assert_eq!(k["publisher"], "Pub");
        assert_eq!(
            path(&files[1]),
            crate::sites::testing::os_path(
                "mangataro/Majo to Youhei/c012.2: The Title/Majo to Youhei_c012.2_002.webp"
            )
        );
        let requests = server.join().unwrap();
        assert!(requests[2].starts_with("GET /auth/chapter-content?chapter_id=1 "));
    }

    #[tokio::test]
    async fn manga_pages_chapter_list() {
        let (root, server) = serve(vec![
            html(MANGA_PAGE),
            json(
                json!({"chapters": [{"id": 3, "chapter": "2.5", "url": "https://mangataro.org/read/majo/ch2-5-3"}],
                "has_more": true, "limit": 1}),
            ),
            json(
                json!({"chapters": [{"id": 2, "chapter": "1", "url": "https://mangataro.org/read/majo/ch1-2"}],
                "has_more": false}),
            ),
        ]);
        let http = http("mangataro", root);
        assert_eq!(
            children(extract(
                &http,
                Url::parse("https://mangataro.org/manga/majo").unwrap()
            ))
            .await
            .len(),
            2
        );
        let requests = server.join().unwrap();
        assert!(
            requests[1].starts_with(
                "GET /auth/manga-chapters?manga_id=77&offset=0&limit=500&order=DESC&_t="
            )
        );
        assert!(requests[2].contains("offset=1&"));
        assert!(
            requests[1].contains("referer: https://mangataro.org/manga/majo-to-youhei")
                || requests[1].contains("Referer: https://mangataro.org/manga/majo-to-youhei")
        );
    }
}
