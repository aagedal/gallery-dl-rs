//! MangaTaro chapters and manga, mirroring gallery_dl/extractor/mangataro.py:
//! manga info comes from the series page's JSON-LD (memoized per
//! invocation), chapter images and lists from the `/auth/…` JSON endpoints.
use super::{Gallery, Memo, get, keywords, set, str_of, text};
use crate::sites::art::{extr, json_loads, segment, split_html};
use crate::sites::{Events, Http, Route, Site, remove_html, unescape};
use crate::{Result, error};
use futures_util::StreamExt;
use md5::{Digest, Md5};
use serde_json::{Map, Value};
use url::Url;

pub static SITE: Site = Site {
    category: "mangataro",
    name: "MangaTaro",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CHAPTER, &MANGA],
    api_key: false,
    cookies: None,
};

const ROOT: &str = "https://mangataro.org";
/// Chapters per list request; larger values have no effect.
const LIMIT: i64 = 500;

static CHAPTER: Route = Route::new(
    "chapter",
    "{manga}_c{chapter:>03}{chapter_minor:?//}_{page:>03}.{extension}",
    &[
        "{category}",
        "{manga}",
        "{volume:?v/ />02}c{chapter:>03}{chapter_minor:?//}{title:?: //}",
    ],
    "{manga}_{chapter}{chapter_minor}_{page}",
);
static MANGA: Route = Route::new("manga", "{filename}.{extension}", &["{category}"], "");

#[derive(Debug, PartialEq)]
enum Target {
    /// `/read/SLUG/…ID`, raw, the manga slug and the chapter ID.
    Chapter {
        path: String,
        slug: String,
        id: String,
    },
    /// Manga slug, raw.
    Manga(String),
}

/// `(?:[^/?#]*-)?(\d+)`: the digits after the last `-` followed by one,
/// else at the start; the matched text and the digits.
fn chapter_id(segment: &str) -> Option<(&str, &str)> {
    let start = segment
        .match_indices('-')
        .rev()
        .map(|(i, _)| i + 1)
        .chain([0])
        .find(|&i| segment[i..].starts_with(|c: char| c.is_ascii_digit()))?;
    let len = segment[start..]
        .bytes()
        .take_while(u8::is_ascii_digit)
        .count();
    Some((&segment[..start + len], &segment[start..start + len]))
}

/// `mangataro\.org(/read/([^/?#]+)/(?:[^/?#]*-)?(\d+))` and
/// `mangataro\.org/manga/([^/?#]+)`, both unanchored at the end.
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) || url.host_str() != Some("mangataro.org") {
        return None;
    }
    let raw: Vec<&str> = url.path().split('/').skip(1).collect();
    match raw.as_slice() {
        ["read", slug, chapter, ..] if segment(slug) => {
            let (part, id) = chapter_id(chapter)?;
            Some(Target::Chapter {
                path: format!("/read/{slug}/{part}"),
                slug: (*slug).to_owned(),
                id: id.to_owned(),
            })
        }
        ["manga", slug, ..] if segment(slug) => Some(Target::Manga((*slug).to_owned())),
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Chapter { path, slug, id }) => {
            chapter_events(http, path, slug, id, Memo::default())
        }
        Some(Target::Manga(slug)) => {
            crate::sites::once(async move { manga_events(http, &slug).await })
        }
        None => crate::sites::fail(error("unsupported MangaTaro URL")),
    }
}

async fn page(http: &Http, url: &str) -> Result<String> {
    http.text_with(&http.api(url)?, &http.browser_headers(&[]))
        .await
}

/// `Extractor._extract_jsonld`.
fn jsonld(page: &str) -> Result<Value> {
    let ld = match extr(page, "<script type=\"application/ld+json\">", "</script>") {
        "" => extr(page, "<script type='application/ld+json'>", "</script>"),
        ld => ld,
    };
    json_loads(ld, "MangaTaro")
}

/// `MangataroBase._manga_info`, memoized by slug.
async fn manga_info(http: &Http, slug: &str, memo: &Memo) -> Result<Map<String, Value>> {
    if let Some(Value::Object(info)) = memo.get(slug) {
        return Ok(info);
    }
    let page = page(http, &format!("{ROOT}/manga/{slug}")).await?;
    let manga = jsonld(&page)?;
    let name = text::partition_last(str_of(&manga, "name"), " | ").0;
    let author: Vec<&str> = manga
        .pointer("/author/name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .split(", ")
        .collect();
    let info = keywords([
        ("manga", text::partition_last(name, " ").0.into()),
        ("manga_id", extr(&page, "data-manga-id=\"", "\"").into()),
        ("manga_url", get(&manga, "url")),
        ("cover", get(&manga, "image")),
        ("author", author.into()),
        ("genre", get(&manga, "genre")),
        ("status", get(&manga, "status")),
        (
            "description",
            unescape(extr(
                &page,
                "id=\"description-content-tab\">",
                "</div></div>",
            ))
            .into(),
        ),
        (
            "tags",
            split_html(extr(&page, ">Genres</h4>", "</div>")).into(),
        ),
        (
            "publisher",
            remove_html(extr(&page, ">Serialization</h4>", "</div>")).into(),
        ),
    ]);
    memo.insert(slug, Value::Object(info.clone()));
    Ok(info)
}

/// `str(round(minor, 5))[1:]`: Python's float repr without the leading 0.
fn minor_text(minor: f64) -> String {
    let rounded = (minor * 1e5).round() / 1e5;
    let repr = match rounded.to_string() {
        repr if repr.contains('.') => repr,
        repr => format!("{repr}.0"),
    };
    repr[1..].to_owned()
}

/// `MangataroChapterExtractor`: the chapter page's JSON-LD and the manga
/// info, then the image list from `/auth/chapter-content`.
fn chapter_events(http: &Http, path: String, slug: String, id: String, memo: Memo) -> Events<'_> {
    crate::sites::once(async move {
        let page = page(http, &format!("{ROOT}{path}")).await?;
        let ld = jsonld(&page)?;
        let comic = ld.pointer("/@graph/0").cloned().unwrap_or(Value::Null);
        let position = match comic.get("position") {
            Some(Value::String(s)) => s.trim().parse().unwrap_or(0.0),
            Some(value) => value.as_f64().unwrap_or(0.0),
            None => 0.0,
        };
        let minor = position - position.trunc();
        let desc: Vec<&str> = str_of(&comic, "description").splitn(4, " - ").collect();
        let mut data = manga_info(http, &slug, &memo).await?;
        set(
            &mut data,
            "title",
            if desc.len() > 3 { desc[1] } else { "" },
        );
        set(&mut data, "chapter", position.trunc() as i64);
        set(
            &mut data,
            "chapter_minor",
            if minor != 0.0 {
                minor_text(minor)
            } else {
                String::new()
            },
        );
        set(&mut data, "chapter_id", text::parse_int(&id));
        set(&mut data, "chapter_url", get(&comic, "url"));
        let published = text::iso(str_of(&comic, "datePublished"));
        set(&mut data, "date", text::date(published));
        set(
            &mut data,
            "date_updated",
            text::date(text::iso(str_of(&comic, "dateModified"))),
        );
        let url = http.api(&format!("{ROOT}/auth/chapter-content?chapter_id={id}"))?;
        let content = http.json_with(&url, &http.browser_headers(&[])).await?;
        let pages = content
            .get("images")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .map(|url| (crate::sites::scalar(url), None))
            .collect();
        Ok(crate::sites::events(
            Gallery::chapter(&CHAPTER, data, pages)
                .id(id)
                .timestamp(published)
                .events(&SITE, http),
        ))
    })
}

/// `MangataroMangaExtractor._update_params`: the `_t` token for `ts`, the
/// MD5 of `{ts}mng_ch_{YYYYMMDDHH}` (UTC) cut to 16 hex digits.
fn token(ts: i64) -> String {
    let date =
        time::OffsetDateTime::from_unix_timestamp(ts).unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    let secret = format!(
        "{ts}mng_ch_{:04}{:02}{:02}{:02}",
        date.year(),
        u8::from(date.month()),
        date.day(),
        date.hour()
    );
    Md5::digest(secret.as_bytes())
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `MangataroMangaExtractor.chapters`: every page of `/auth/manga-chapters`
/// (newest first), queued oldest first unless `chapter-reverse`.
async fn manga_events<'a>(http: &'a Http, slug: &str) -> Result<Events<'a>> {
    let memo = Memo::default();
    let manga = manga_info(http, slug, &memo).await?;
    let referer = manga
        .get("manga_url")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let manga_id = manga
        .get("manga_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let mut children = Vec::new();
    let mut offset = 0;
    loop {
        let ts = time::OffsetDateTime::now_utc().unix_timestamp();
        let mut url = http.api(&format!("{ROOT}/auth/manga-chapters"))?;
        url.query_pairs_mut()
            .append_pair("manga_id", &manga_id)
            .append_pair("offset", &offset.to_string())
            .append_pair("limit", &LIMIT.to_string())
            .append_pair("order", "DESC")
            .append_pair("_t", &token(ts))
            .append_pair("_ts", &ts.to_string());
        let data = http
            .json_with(
                &url,
                &http.browser_headers(&[
                    ("Referer", &referer),
                    ("Sec-Fetch-Dest", "empty"),
                    ("Sec-Fetch-Mode", "cors"),
                    ("Sec-Fetch-Site", "same-origin"),
                ]),
            )
            .await?;
        let list = data
            .get("chapters")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        for ch in list {
            let mut ch = ch.as_object().cloned().unwrap_or_default();
            let mut info = manga.clone();
            set(
                &mut info,
                "chapter_id",
                text::parse_int_value(ch.remove("id").as_ref()),
            );
            crate::sites::art::update(&mut info, &ch);
            let (chapter, sep, minor) = text::partition(
                ch.get("chapter")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                ".",
            );
            set(&mut info, "chapter", text::parse_int(chapter));
            set(
                &mut info,
                "chapter_minor",
                if sep.is_empty() {
                    String::new()
                } else {
                    format!(".{minor}")
                },
            );
            children.push((
                ch.get("url")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                info,
            ));
        }
        if !crate::sites::art::truthy(data.get("has_more")) || list.is_empty() {
            break;
        }
        offset += match text::parse_int_value(data.get("limit")) {
            0 => LIMIT,
            limit => limit,
        };
    }
    // `MangaExtractor.reverse = True`.
    if !http.option_bool("chapter-reverse", false) {
        children.reverse();
    }
    let streams: Vec<Events<'a>> = children
        .into_iter()
        .filter_map(|(url, data)| {
            let Some(Target::Chapter { path, slug, id }) = target(&Url::parse(&url).ok()?) else {
                return None;
            };
            let memo = memo.clone();
            Some(super::queue_with(&SITE, &MANGA, &data, move || {
                chapter_events(http, path, slug, id, memo)
            }))
        })
        .collect();
    Ok(futures_util::stream::iter(streams).flatten().boxed())
}

#[cfg(test)]
mod tests {
    use super::super::tests::{archive, path};
    use super::*;
    use crate::sites::testing::{html, http, json, keywords as kw, media, queued, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://mangataro.org/read/kimi/ch123-12345"),
            Some(Target::Chapter {
                path: "/read/kimi/ch123-12345".into(),
                slug: "kimi".into(),
                id: "12345".into(),
            })
        );
        assert_eq!(
            t("http://mangataro.org/read/kimi/ch-7-abc/"),
            Some(Target::Chapter {
                path: "/read/kimi/ch-7".into(),
                slug: "kimi".into(),
                id: "7".into(),
            })
        );
        assert_eq!(
            t("https://mangataro.org/read/kimi/42"),
            Some(Target::Chapter {
                path: "/read/kimi/42".into(),
                slug: "kimi".into(),
                id: "42".into(),
            })
        );
        assert_eq!(
            t("https://mangataro.org/manga/kimi/extra"),
            Some(Target::Manga("kimi".into()))
        );
        for raw in [
            "https://mangataro.org/read/kimi/chapter",
            "https://mangataro.org/manga/",
            "https://www.mangataro.org/manga/kimi",
            "https://mangataro.org.evil.test/manga/kimi",
            "https://mangataro.org:8443/manga/kimi",
            "https://u:p@mangataro.org/manga/kimi",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[test]
    fn computes_list_token() {
        // 2025-01-02 03:04:05 UTC
        let ts = 1735787045;
        let expected: String = Md5::digest(format!("{ts}mng_ch_2025010203").as_bytes())
            .iter()
            .take(8)
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(token(ts), expected);
        assert_eq!(token(ts).len(), 16);
        assert_eq!(minor_text(0.5), ".5");
        assert_eq!(minor_text(0.25000000001), ".25");
    }

    const MANGA_PAGE: &str = r#"<script type="application/ld+json">{"name": "Kimi to Boku Manga | MangaTaro",
"url": "https://mangataro.org/manga/kimi", "image": "https://mangataro.org/cover.jpg",
"author": {"name": "A One, B Two"}, "genre": ["Romance"], "status": "Ongoing"}</script>
<div data-manga-id="555"></div>
<div id="description-content-tab">Love &amp; more</div></div>
<h4>Genres</h4><a>Romance</a><a>Drama</a></div>
<h4>Serialization</h4><a>Weekly <b>Mag</b></a></div>"#;

    #[tokio::test]
    async fn extracts_chapter() {
        let chapter = r#"<script type="application/ld+json">{"@graph": [{"position": 12.5,
"description": "Kimi to Boku - Hello There - Chapter 12.5 - MangaTaro", "url": "https://mangataro.org/read/kimi/ch12-999",
"datePublished": "2025-02-03T04:05:06+00:00", "dateModified": "2025-02-04T04:05:06+00:00"}]}</script>"#;
        let content = json!({"images": ["https://cdn.test/1.webp", "https://cdn.test/2.webp"]});
        let (root, requests) = serve(vec![html(chapter), html(MANGA_PAGE), json(content)]);
        let http = http("MangaTaro", root);
        let files = media(extract(
            &http,
            Url::parse("https://mangataro.org/read/kimi/ch12-999").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = kw(&files[1]);
        assert_eq!(k["manga"], "Kimi to Boku");
        assert_eq!(k["manga_id"], "555");
        assert_eq!(k["author"], json!(["A One", "B Two"]));
        assert_eq!(k["description"], "Love & more");
        assert_eq!(k["tags"], json!(["Romance", "Drama"]));
        assert_eq!(k["publisher"], "Weekly Mag");
        assert_eq!(k["title"], "Hello There");
        assert_eq!(k["chapter"], 12);
        assert_eq!(k["chapter_minor"], ".5");
        assert_eq!(k["chapter_id"], 999);
        assert_eq!(k["date"], "2025-02-03 04:05:06");
        assert_eq!(k["date_updated"], "2025-02-04 04:05:06");
        assert_eq!(k["page"], 2);
        let colon = if cfg!(windows) { "_" } else { ":" };
        assert_eq!(
            path(&files[1]),
            format!(
                "mangataro/Kimi to Boku/c012.5{colon} Hello There/Kimi to Boku_c012.5_002.webp"
            )
        );
        assert_eq!(
            archive(&files[1]),
            r#"["{manga}_{chapter}{chapter_minor}_{page}",["Kimi to Boku","12",".5","2"]]"#
        );
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /read/kimi/ch12-999 "));
        assert!(requests[1].starts_with("GET /manga/kimi "));
        assert!(requests[2].starts_with("GET /auth/chapter-content?chapter_id=999 "));
    }

    #[tokio::test]
    async fn manga_pages_the_chapter_list() {
        let first = json!({"chapters": [
            {"id": 3, "url": "https://mangataro.org/read/kimi/ch3-3", "chapter": "3", "title": "C"},
        ], "has_more": true, "limit": 1});
        let second = json!({"chapters": [
            {"id": "2", "url": "https://mangataro.org/read/kimi/ch2-2", "chapter": "2.5"},
        ], "has_more": false});
        let (root, requests) = serve(vec![html(MANGA_PAGE), json(first), json(second)]);
        let http = http("MangaTaro", root);
        let children = queued(extract(
            &http,
            Url::parse("https://mangataro.org/manga/kimi").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].subcategory(), "manga");
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /manga/kimi "));
        assert!(requests[1].starts_with(
            "GET /auth/manga-chapters?manga_id=555&offset=0&limit=500&order=DESC&_t="
        ));
        assert!(
            requests[1]
                .to_ascii_lowercase()
                .contains("referer: https://mangataro.org/manga/kimi\r\n")
        );
        assert!(requests[2].starts_with("GET /auth/manga-chapters?manga_id=555&offset=1&"));
    }

    #[tokio::test]
    async fn manga_children_share_the_manga_info() {
        let list = json!({"chapters": [
            {"id": 2, "url": "https://mangataro.org/read/kimi/ch2-2", "chapter": "2.5"},
        ]});
        let chapter = r#"<script type="application/ld+json">{"@graph": [{"position": 2.5, "description": "x"}]}</script>"#;
        let content = json!({"images": ["https://cdn.test/1.webp"]});
        let (root, requests) = serve(vec![
            html(MANGA_PAGE),
            json(list),
            html(chapter),
            json(content),
        ]);
        let http = http("MangaTaro", root);
        let files = media(extract(
            &http,
            Url::parse("https://mangataro.org/manga/kimi").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(kw(&files[0])["manga"], "Kimi to Boku");
        assert_eq!(kw(&files[0])["chapter_minor"], ".5");
        let requests = requests.join().unwrap();
        assert!(requests[2].starts_with("GET /read/kimi/ch2-2 "));
        assert!(requests[3].starts_with("GET /auth/chapter-content?chapter_id=2 "));
    }
}
