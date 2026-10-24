//! ComicK covers, chapters and manga, mirroring
//! gallery_dl/extractor/comick.py: manga info comes from the comic page's
//! Next.js data, chapters from `/_next/data/BUILD/…` and the chapter list
//! from api.comick.io.
//!
//! Upstream 1.32.17 calls `self._chapter_info(self, manga, chstr)`, which
//! raises a `TypeError` for every chapter; this port does what the code
//! intends. A 404 for the Next.js data refreshes the build ID once, as
//! upstream does for HTML 404s; the HTTP layer does not expose the response
//! type, so a JSON `notFound` 404 is retried once too before failing.
use super::{Gallery, Memo, get, set, text};
use crate::sites::art::{next_data, parse_query, truthy};
use crate::sites::{Events, Http, Route, Site};
use crate::{Result, error};
use futures_util::StreamExt;
use serde_json::{Map, Value, json};
use url::Url;

pub static SITE: Site = Site {
    category: "comick",
    name: "ComicK",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&COVERS, &CHAPTER, &MANGA],
    api_key: false,
    cookies: None,
};

const ROOT: &str = "https://comick.io";
const API: &str = "https://api.comick.io";
const IMAGES: &str = "https://meo.comick.pictures/";
/// Redirects followed for one chapter before giving up.
const MAX_REDIRECTS: usize = 10;
/// Chapter list pages fetched for one manga.
const MAX_PAGES: usize = 1000;

static COVERS: Route = Route::new(
    "covers",
    "{volume:>02}_{lang}.{extension}",
    &["{category}", "{manga}", "Covers"],
    "c_{id}",
);
static CHAPTER: Route = Route::new(
    "chapter",
    "{manga}_c{chapter:>03}{chapter_minor:?//}_{page:>03}.{extension}",
    &[
        "{category}",
        "{manga}",
        "{volume:?v/ />02}c{chapter:>03}{chapter_minor:?//}{title:?: //}",
    ],
    "{chapter_hid}_{page}",
);
static MANGA: Route = Route::new("manga", "{filename}.{extension}", &["{category}"], "");

#[derive(Debug, PartialEq)]
enum Target {
    Covers(String),
    Chapter {
        slug: String,
        chstr: String,
    },
    /// The slug and the raw query after `/comic/SLUG[/]`.
    Manga {
        slug: String,
        query: String,
    },
}

/// `[\w-]+` on a raw path segment.
fn slug(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

/// The prefix of a segment matching `\w+(?:-(?:chapter|volume)-[^/?#]+)?`.
fn chapter_string(segment: &str) -> Option<&str> {
    let end = segment
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(segment.len());
    if end == 0 {
        return None;
    }
    let rest = &segment[end..];
    let full = ["-chapter-", "-volume-"]
        .iter()
        .any(|kind| rest.len() > kind.len() && rest.starts_with(kind));
    Some(if full { segment } else { &segment[..end] })
}

/// `comick\.io/comic/([\w-]+)/cover`, then
/// `/comic/([\w-]+)/(\w+(?:-(?:chapter|volume)-[^/?#]+)?)`, then
/// `/comic/([\w-]+)/?(?:\?([^#]+))?`, all unanchored at the end.
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::art::on_host(url, &["comick.io"]) {
        return None;
    }
    let raw: Vec<&str> = url.path().split('/').skip(1).collect();
    let ["comic", name, rest @ ..] = raw.as_slice() else {
        return None;
    };
    if !slug(name) {
        return None;
    }
    let slug = (*name).to_owned();
    match rest {
        [second, ..] if second.starts_with("cover") => Some(Target::Covers(slug)),
        [second, ..] if chapter_string(second).is_some() => Some(Target::Chapter {
            slug,
            chstr: chapter_string(second).unwrap_or_default().to_owned(),
        }),
        [] | [""] => Some(Target::Manga {
            slug,
            query: url.query().unwrap_or_default().to_owned(),
        }),
        _ => Some(Target::Manga {
            slug,
            query: String::new(),
        }),
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Covers(slug)) => {
            crate::sites::once(async move { covers_events(http, &slug).await })
        }
        Some(Target::Chapter { slug, chstr }) => chapter_events(http, slug, chstr, Memo::default()),
        Some(Target::Manga { slug, query }) => {
            crate::sites::once(async move { manga_events(http, &slug, &query).await })
        }
        None => crate::sites::fail(error("unsupported ComicK URL")),
    }
}

async fn page(http: &Http, url: &str) -> Result<String> {
    http.text_with(&http.api(url)?, &http.browser_headers(&[]))
        .await
}

/// A list of `item[outer][inner]` values.
fn titles(items: Option<&Value>, outer: &str, inner: &str) -> Vec<Value> {
    items
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|item| item.get(outer).map(|o| get(o, inner)).unwrap_or_default())
        .collect()
}

/// `_manga_info`, memoized by slug; `_build_id` is kept for chapter data
/// requests but is not a keyword.
async fn manga_info(http: &Http, slug: &str, memo: &Memo) -> Result<Value> {
    if let Some(manga) = memo.get(slug) {
        return Ok(manga);
    }
    let manga = fetch_manga(http, slug).await?;
    memo.insert(slug, manga.clone());
    Ok(manga)
}

async fn fetch_manga(http: &Http, slug: &str) -> Result<Value> {
    let page = page(http, &format!("{ROOT}/comic/{slug}")).await?;
    Ok(parse_manga(&next_data(&page, "ComicK")?))
}

/// The values of every `name=` pair of a query, like the list names of
/// `text.parse_query_list`.
fn query_list(query: &str, name: &str) -> Vec<String> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .filter(|(key, _)| crate::sites::percent_decode(&key.replace('+', " ")) == name)
        .map(|(_, value)| crate::sites::percent_decode(&value.replace('+', " ")))
        .collect()
}

/// `_manga_info` on a comic page's Next.js data.
fn parse_manga(data: &Value) -> Value {
    let props = data.pointer("/props/pageProps").unwrap_or(&Value::Null);
    let comic = props.get("comic").unwrap_or(&Value::Null);
    let (mut genre, mut theme, mut format) = (Vec::new(), Vec::new(), Value::from(""));
    for item in comic
        .get("md_comic_md_genres")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let item = item.get("md_genres").unwrap_or(&Value::Null);
        match super::str_of(item, "group") {
            "Genre" => genre.push(get(item, "name")),
            "Theme" => theme.push(get(item, "name")),
            _ => format = get(item, "name"),
        }
    }
    let (tags, publisher) = match comic.get("mu_comics") {
        Some(mu) if truthy(Some(mu)) => (
            titles(mu.get("mu_comic_categories"), "mu_categories", "title"),
            titles(mu.get("mu_comic_publishers"), "mu_publishers", "title"),
        ),
        _ => (Vec::new(), Vec::new()),
    };
    let status = if comic.get("status").and_then(Value::as_i64) == Some(2) {
        "Complete"
    } else {
        "Ongoing"
    };
    json!({
        "manga": get(comic, "title"),
        "manga_id": get(comic, "id"),
        "manga_hid": get(comic, "hid"),
        "manga_slug": get(comic, "slug"),
        "manga_titles": field(comic.get("md_titles"), "title"),
        "artist": field(props.get("artists"), "name"),
        "author": field(props.get("authors"), "name"),
        "genre": genre,
        "theme": theme,
        "format": format,
        "tags": tags,
        "publisher": publisher,
        "published": text::parse_int_value(comic.get("year")),
        "description": get(comic, "desc"),
        "demographic": get(props, "demographic"),
        "origin": get(comic, "iso639_1"),
        "mature": get(props, "matureContent"),
        "rating": get(comic, "content_rating"),
        "rank": get(comic, "follow_rank"),
        "score": comic
            .get("bayesian_rating")
            .map_or(json!(0.0), |rating| text::parse_float(&crate::sites::scalar(rating))),
        "status": status,
        "links": get(comic, "links"),
        "_build_id": get(data, "buildId"),
    })
}

/// The `key` of each object in a list.
fn field(items: Option<&Value>, key: &str) -> Vec<Value> {
    items
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|item| get(item, key))
        .collect()
}

/// The keywords of `_manga_info`, without `_build_id`.
fn public(manga: &Value) -> Map<String, Value> {
    manga
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(key, _)| !key.starts_with('_'))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

/// The HTTP layer's wording for 404/410 responses.
fn not_found(e: &crate::Error) -> bool {
    e.to_string().ends_with("resource not found")
}

/// `ComickCoversExtractor`: the manga info, then the cover page's covers,
/// oldest first.
async fn covers_events<'a>(http: &'a Http, slug: &str) -> Result<Events<'a>> {
    let manga = manga_info(http, slug, &Memo::default()).await?;
    let slug = super::str_of(&manga, "manga_slug").to_owned();
    let page = page(http, &format!("{ROOT}/comic/{slug}/cover")).await?;
    let data = next_data(&page, "ComicK")?;
    let mut covers = data
        .pointer("/props/pageProps/comic/md_covers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    covers.reverse();
    let pages = covers
        .into_iter()
        .map(|cover| {
            let image = super::keywords([
                ("id", get(&cover, "id")),
                ("width", get(&cover, "w")),
                ("height", get(&cover, "h")),
                ("size", get(&cover, "s")),
                ("lang", get(&cover, "locale")),
                ("volume", text::parse_int_value(cover.get("vol")).into()),
            ]);
            let url = format!("{IMAGES}{}", super::str_of(&cover, "b2key"));
            let mut image = image;
            set(&mut image, "cover", cover);
            (url, Some(image))
        })
        .collect();
    let mut gallery = Gallery::chapter(&COVERS, public(&manga), pages)
        .id(crate::sites::scalar(&get(&manga, "manga_hid")));
    // `GalleryExtractor.enum`.
    gallery.enum_key = "num";
    Ok(crate::sites::events(gallery.events(&SITE, http)))
}

/// `_chapter_info`: the chapter's Next.js data.
async fn chapter_info(http: &Http, manga: &Value, chstr: &str) -> Result<Value> {
    let slug = super::str_of(manga, "manga_slug");
    let build = crate::sites::scalar(&get(manga, "_build_id"));
    let mut url = http.api(&format!(
        "{ROOT}/_next/data/{build}/comic/{slug}/{chstr}.json"
    ))?;
    url.query_pairs_mut()
        .append_pair("slug", slug)
        .append_pair("chapter", chstr);
    let data = http.json_with(&url, &http.browser_headers(&[])).await?;
    Ok(get(&data, "pageProps"))
}

/// `ComickChapterExtractor`: the (memoized) manga info, the chapter's data
/// (following `__N_REDIRECT`s), then its images.
fn chapter_events(http: &Http, slug: String, chstr: String, memo: Memo) -> Events<'_> {
    crate::sites::once(async move {
        let (mut slug, mut chstr) = (slug, chstr);
        let mut manga = manga_info(http, &slug, &memo).await?;
        let mut retried = false;
        let mut redirects = 0;
        let props = loop {
            let props = match chapter_info(http, &manga, &chstr).await {
                Ok(props) => props,
                Err(e) if not_found(&e) && !retried => {
                    // Updating the Next.js build ID.
                    retried = true;
                    manga = fetch_manga(http, &slug).await?;
                    memo.insert(&slug, manga.clone());
                    continue;
                }
                Err(e) if not_found(&e) => return Err(error("ComicK chapter not found")),
                Err(e) => return Err(e),
            };
            if let Some(path) = props.get("__N_REDIRECT").and_then(Value::as_str) {
                redirects += 1;
                let mut parts = path.rsplitn(3, '/');
                let (Some(next_chstr), Some(next_slug), Some(_)) =
                    (parts.next(), parts.next(), parts.next())
                else {
                    return Err(error("ComicK returned an invalid redirect"));
                };
                if redirects > MAX_REDIRECTS {
                    return Err(error("ComicK redirected too often"));
                }
                (slug, chstr) = (next_slug.to_owned(), next_chstr.to_owned());
                continue;
            }
            break props;
        };
        let ch = props.get("chapter").unwrap_or(&Value::Null);
        let (chapter, minor) = match ch.get("chap").and_then(Value::as_str) {
            Some(chap) if !chap.is_empty() => text::chapter(chap),
            _ => (0, String::new()),
        };
        let date = |key: &str| {
            let raw = super::str_of(ch, key);
            text::iso(raw.get(..19).unwrap_or(raw))
        };
        let created = date("created_at");
        let mut data = public(&manga);
        data.extend(super::keywords([
            ("title", get(&props, "chapTitle")),
            ("volume", text::parse_int_value(ch.get("vol")).into()),
            ("chapter", chapter.into()),
            ("chapter_minor", minor.into()),
            ("chapter_id", get(ch, "id")),
            ("chapter_hid", get(ch, "hid")),
            ("chapter_string", chstr.clone().into()),
            ("group", get(ch, "group_name")),
            ("date", text::date(created)),
            ("date_updated", text::date(date("updated_at"))),
            ("lang", get(ch, "lang")),
        ]));
        let images = ch
            .get("md_images")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if !images.is_empty() && images.iter().all(|img| !truthy(img.get("b2key"))) {
            return Err(error(format!(
                "{chstr}: Broken Chapter (missing 'b2key' for all pages)"
            )));
        }
        // Pages without a `b2key` keep their number but are skipped.
        let pages = images
            .iter()
            .map(|img| {
                let url = match super::str_of(img, "b2key") {
                    "" => String::new(),
                    key => format!("{IMAGES}{key}"),
                };
                let image = super::keywords([
                    ("width", get(img, "w")),
                    ("height", get(img, "h")),
                    ("size", get(img, "s")),
                    ("optimized", get(img, "optimized")),
                ]);
                (url, Some(image))
            })
            .collect();
        Ok(crate::sites::events(
            Gallery::chapter(&CHAPTER, data, pages)
                .id(crate::sites::scalar(&get(ch, "hid")))
                .timestamp(created)
                .events(&SITE, http),
        ))
    })
}

/// `ComickMangaExtractor.chapters`: the API's chapter list in the query's
/// or `lang` option's languages, page by page from the query's `page`.
async fn chapter_list(http: &Http, manga: &Value, query: &str) -> Result<Vec<Value>> {
    let pairs = parse_query(query);
    let first = |name: &str| {
        pairs
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    };
    let langs = query_list(query, "lang");
    let lang = if !langs.is_empty() {
        Some(langs.join(","))
    } else if truthy(http.option("lang")) {
        super::option_list(http, "lang").map(|langs| langs.join(","))
    } else {
        None
    };
    let start = crate::sites::art::parse_int(first("page").unwrap_or_default(), 1);
    let order = match (first("date-order"), first("chap-order")) {
        (Some(order), _) if !order.is_empty() => ("date-order", order.to_owned()),
        (_, Some(order)) if !order.is_empty() => ("chap-order", order.to_owned()),
        _ => (
            "chap-order",
            if http.option_bool("chapter-reverse", false) {
                "0"
            } else {
                "1"
            }
            .to_owned(),
        ),
    };
    let group = first("group").filter(|group| *group != "0");
    let hid = crate::sites::scalar(&get(manga, "manga_hid"));
    let headers = http.browser_headers(&[
        ("Origin", "https://comick.io"),
        ("Sec-Fetch-Dest", "empty"),
        ("Sec-Fetch-Mode", "cors"),
        ("Sec-Fetch-Site", "same-site"),
    ]);
    let mut chapters = Vec::new();
    for page in (start..).take(MAX_PAGES) {
        let mut url = http.api(&format!("{API}/comic/{hid}/chapters"))?;
        {
            let mut params = url.query_pairs_mut();
            if let Some(lang) = &lang {
                params.append_pair("lang", lang);
            }
            params.append_pair("page", &page.to_string());
            params.append_pair(order.0, &order.1);
        }
        let data = http.json_with(&url, &headers).await?;
        let limit = data.get("limit").and_then(Value::as_i64).unwrap_or(0);
        let total = data.get("total").and_then(Value::as_i64).unwrap_or(0);
        for ch in data
            .get("chapters")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let wanted = match group {
                None => true,
                Some(group) => ch
                    .get("group_name")
                    .and_then(Value::as_array)
                    .is_some_and(|groups| groups.iter().any(|g| g.as_str() == Some(group))),
            };
            if wanted {
                chapters.push(ch.clone());
            }
        }
        if limit <= 0 || total <= limit.saturating_mul(page) {
            break;
        }
    }
    Ok(chapters)
}

/// `ComickMangaExtractor.items`: each listed chapter queued with the manga
/// info; the chapters share it.
async fn manga_events<'a>(http: &'a Http, slug: &str, query: &str) -> Result<Events<'a>> {
    let memo = Memo::default();
    let manga = manga_info(http, slug, &memo).await?;
    let slug = super::str_of(&manga, "manga_slug").to_owned();
    memo.insert(&slug, manga.clone());
    let mut streams: Vec<Events<'a>> = Vec::new();
    for ch in chapter_list(http, &manga, query).await? {
        let mut data = ch.as_object().cloned().unwrap_or_default();
        data.extend(public(&manga));
        let hid = crate::sites::scalar(&get(&ch, "hid"));
        let lang = crate::sites::scalar(&get(&ch, "lang"));
        let chap = super::str_of(&ch, "chap");
        let vol = super::str_of(&ch, "vol");
        let path = if !chap.is_empty() {
            let (chapter, minor) = text::chapter(chap);
            set(&mut data, "volume", text::parse_int_value(ch.get("vol")));
            set(&mut data, "chapter", chapter);
            set(&mut data, "chapter_minor", minor);
            format!("{hid}-chapter-{chap}-{lang}")
        } else if !vol.is_empty() {
            set(&mut data, "volume", text::parse_int(vol));
            set(&mut data, "chapter", 0);
            set(&mut data, "chapter_minor", "");
            format!("{hid}-volume-{vol}-{lang}")
        } else {
            set(&mut data, "volume", 0);
            set(&mut data, "chapter", 0);
            set(&mut data, "chapter_minor", "");
            hid
        };
        let Ok(url) = Url::parse(&format!("{ROOT}/comic/{slug}/{path}")) else {
            continue;
        };
        let Some(Target::Chapter { slug, chstr }) = target(&url) else {
            continue;
        };
        let memo = memo.clone();
        streams.push(super::queue_with(&SITE, &MANGA, &data, move || {
            chapter_events(http, slug, chstr, memo)
        }));
    }
    Ok(futures_util::stream::iter(streams).flatten().boxed())
}

#[cfg(test)]
mod tests {
    use super::super::tests::{archive, path};
    use super::*;
    use crate::sites::testing::{
        fails, html, http, json as json_reply, keywords as kw, media, response, serve,
    };

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        let chapter = |slug: &str, chstr: &str| {
            Some(Target::Chapter {
                slug: slug.into(),
                chstr: chstr.into(),
            })
        };
        assert_eq!(
            t("https://comick.io/comic/00-the-beginning/cover"),
            Some(Target::Covers("00-the-beginning".into()))
        );
        assert_eq!(
            t("https://comick.io/comic/x/k3L9w-chapter-12.5-en"),
            chapter("x", "k3L9w-chapter-12.5-en")
        );
        assert_eq!(
            t("https://www.comick.io/comic/x/k3L9w-volume-3-pt-br/"),
            chapter("x", "k3L9w-volume-3-pt-br")
        );
        assert_eq!(t("https://comick.io/comic/x/k3L9w"), chapter("x", "k3L9w"));
        assert_eq!(
            t("https://comick.io/comic/x/k3L9w-extra"),
            chapter("x", "k3L9w")
        );
        assert_eq!(
            t("https://comick.io/comic/one-piece?lang=en&page=2"),
            Some(Target::Manga {
                slug: "one-piece".into(),
                query: "lang=en&page=2".into()
            })
        );
        assert_eq!(
            t("http://comick.io/comic/one-piece/"),
            Some(Target::Manga {
                slug: "one-piece".into(),
                query: String::new()
            })
        );
        for raw in [
            "https://comick.io/comic/",
            "https://comick.io/comic/a%20b",
            "https://comick.io/home",
            "https://comick.dev/comic/one-piece",
            "https://api.comick.io/comic/one-piece",
            "https://comick.io.evil.test/comic/one-piece",
            "https://comick.io:8443/comic/one-piece",
            "https://u:p@comick.io/comic/one-piece",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn manga_page(build: &str) -> String {
        let data = json!({"buildId": build, "props": {"pageProps": {
            "comic": {
                "title": "One Piece", "id": 123, "hid": "Ab1", "slug": "one-piece",
                "md_titles": [{"title": "ワンピース"}],
                "md_comic_md_genres": [
                    {"md_genres": {"name": "Action", "group": "Genre"}},
                    {"md_genres": {"name": "Pirates", "group": "Theme"}},
                    {"md_genres": {"name": "Long Strip", "group": "Format"}}
                ],
                "mu_comics": {
                    "mu_comic_categories": [{"mu_categories": {"title": "Pirate/s"}}],
                    "mu_comic_publishers": [{"mu_publishers": {"title": "Shueisha"}}]
                },
                "year": 1997, "desc": "Gol D. Roger", "iso639_1": "ja",
                "content_rating": "safe", "follow_rank": 1,
                "bayesian_rating": "8.75", "status": 1, "links": {"mu": "x"},
                "md_covers": [
                    {"id": 2, "w": 10, "h": 20, "s": 30, "locale": "ja", "vol": "2", "b2key": "B2.jpg"},
                    {"id": 1, "w": 11, "h": 21, "s": 31, "locale": "en", "vol": "1", "b2key": "A1.png"}
                ]
            },
            "artists": [{"name": "Oda"}], "authors": [{"name": "Oda"}],
            "demographic": "Shounen", "matureContent": false
        }}});
        format!(r#"<script id="__NEXT_DATA__" type="application/json">{data}</script>"#)
    }

    fn chapter_data() -> Value {
        json!({"pageProps": {"chapTitle": "Romance Dawn", "chapter": {
            "id": 9001, "hid": "k3L9w", "chap": "1.5", "vol": "1", "lang": "en",
            "group_name": ["TCB"], "created_at": "2020-01-02T03:04:05.000Z",
            "updated_at": "2021-01-02T03:04:05Z",
            "md_images": [
                {"b2key": "p1.jpg", "w": 800, "h": 1200, "s": 1000, "optimized": 1},
                {"b2key": "p2.webp", "w": 801, "h": 1201, "s": 1001, "optimized": null}
            ]
        }}})
    }

    #[test]
    fn parses_manga_info() {
        let page = manga_page("BUILD1");
        let manga = parse_manga(&next_data(&page, "ComicK").unwrap());
        assert_eq!(manga["manga"], "One Piece");
        assert_eq!(manga["manga_hid"], "Ab1");
        assert_eq!(manga["manga_titles"], json!(["ワンピース"]));
        assert_eq!(manga["artist"], json!(["Oda"]));
        assert_eq!(manga["genre"], json!(["Action"]));
        assert_eq!(manga["theme"], json!(["Pirates"]));
        assert_eq!(manga["format"], "Long Strip");
        assert_eq!(manga["tags"], json!(["Pirate/s"]));
        assert_eq!(manga["publisher"], json!(["Shueisha"]));
        assert_eq!(manga["published"], 1997);
        assert_eq!(manga["score"], json!(8.75));
        assert_eq!(manga["status"], "Ongoing");
        assert_eq!(manga["mature"], false);
        assert_eq!(manga["_build_id"], "BUILD1");
    }

    #[tokio::test]
    async fn extracts_chapter_images() {
        let (root, requests) = serve(vec![
            html(&manga_page("BUILD1")),
            json_reply(chapter_data()),
        ]);
        let http = http("ComicK", root);
        let files = media(extract(
            &http,
            Url::parse("https://comick.io/comic/one-piece/k3L9w-chapter-1.5-en").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(files[1].url.as_str(), "https://meo.comick.pictures/p2.webp");
        let k = kw(&files[1]);
        assert_eq!(k["manga"], "One Piece");
        assert_eq!(k["title"], "Romance Dawn");
        assert_eq!(k["volume"], 1);
        assert_eq!(k["chapter"], 1);
        assert_eq!(k["chapter_minor"], ".5");
        assert_eq!(k["chapter_id"], 9001);
        assert_eq!(k["chapter_hid"], "k3L9w");
        assert_eq!(k["chapter_string"], "k3L9w-chapter-1.5-en");
        assert_eq!(k["group"], json!(["TCB"]));
        assert_eq!(k["date"], "2020-01-02 03:04:05");
        assert_eq!(k["date_updated"], "2021-01-02 03:04:05");
        assert_eq!(k["lang"], "en");
        assert_eq!(k["width"], 801);
        assert_eq!(k["optimized"], Value::Null);
        assert_eq!(k["page"], 2);
        assert_eq!(k["count"], 2);
        assert!(!k.contains_key("_build_id"));
        assert_eq!(
            path(&files[1]),
            "comick/One Piece/v01 c001.5: Romance Dawn/One Piece_c001.5_002.webp"
        );
        assert_eq!(
            archive(&files[1]),
            r#"["{chapter_hid}_{page}",["k3L9w","2"]]"#
        );
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /comic/one-piece "));
        assert!(requests[1].starts_with(
            "GET /_next/data/BUILD1/comic/one-piece/k3L9w-chapter-1.5-en.json?slug=one-piece&chapter=k3L9w-chapter-1.5-en "
        ));
    }

    #[tokio::test]
    async fn chapter_refreshes_the_build_id_and_follows_redirects() {
        let redirect =
            json!({"pageProps": {"__N_REDIRECT": "/comic/one-piece/k3L9w-chapter-1.5-en"}});
        let (root, requests) = serve(vec![
            html(&manga_page("OLD")),
            response("404 Not Found", "text/html", b"<html></html>"),
            html(&manga_page("NEW")),
            json_reply(redirect),
            json_reply(chapter_data()),
        ]);
        let http = http("ComicK", root);
        let files = media(extract(
            &http,
            Url::parse("https://comick.io/comic/one-piece/k3L9w").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(kw(&files[0])["chapter_string"], "k3L9w-chapter-1.5-en");
        let requests = requests.join().unwrap();
        assert!(requests[1].starts_with("GET /_next/data/OLD/comic/one-piece/k3L9w.json?"));
        assert!(requests[2].starts_with("GET /comic/one-piece "));
        assert!(requests[3].starts_with("GET /_next/data/NEW/comic/one-piece/k3L9w.json?"));
        assert!(
            requests[4]
                .starts_with("GET /_next/data/NEW/comic/one-piece/k3L9w-chapter-1.5-en.json?")
        );
    }

    #[tokio::test]
    async fn missing_chapter_is_an_error() {
        let missing = || response("404 Not Found", "application/json", br#"{"notFound":true}"#);
        let (root, requests) = serve(vec![
            html(&manga_page("B")),
            missing(),
            html(&manga_page("B")),
            missing(),
        ]);
        let http = http("ComicK", root);
        let url = Url::parse("https://comick.io/comic/one-piece/k3L9w").unwrap();
        assert!(fails(extract(&http, url)).await);
        assert_eq!(requests.join().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn broken_chapter_is_an_error() {
        let broken = json!({"pageProps": {"chapter": {"hid": "k", "md_images": [{"w": 1}]}}});
        let (root, requests) = serve(vec![html(&manga_page("B")), json_reply(broken)]);
        let http = http("ComicK", root);
        let url = Url::parse("https://comick.io/comic/one-piece/k3L9w").unwrap();
        assert!(fails(extract(&http, url)).await);
        requests.join().unwrap();
    }

    #[tokio::test]
    async fn extracts_covers_oldest_first() {
        let page = manga_page("B");
        let (root, requests) = serve(vec![html(&page), html(&page)]);
        let http = http("ComicK", root);
        let files = media(extract(
            &http,
            Url::parse("https://comick.io/comic/one-piece/cover").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].url.as_str(), "https://meo.comick.pictures/A1.png");
        let k = kw(&files[0]);
        assert_eq!(k["id"], 1);
        assert_eq!(k["volume"], 1);
        assert_eq!(k["lang"], "en");
        assert_eq!(k["width"], 11);
        assert_eq!(k["cover"]["b2key"], "A1.png");
        assert_eq!(k["num"], 1);
        assert_eq!(k["count"], 2);
        assert_eq!(path(&files[0]), "comick/One Piece/Covers/01_en.png");
        assert_eq!(archive(&files[1]), r#"["c_{id}",["2"]]"#);
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /comic/one-piece "));
        assert!(requests[1].starts_with("GET /comic/one-piece/cover "));
    }

    #[tokio::test]
    async fn manga_lists_chapters_by_query_and_shares_the_manga_info() {
        let list = json!({"limit": 1, "total": 2, "chapters": [
            {"hid": "h2", "chap": "2", "vol": null, "lang": "en", "group_name": ["TCB"]}
        ]});
        let list2 = json!({"limit": 1, "total": 2, "chapters": [
            {"hid": "h3", "chap": null, "vol": "3", "lang": "fr", "group_name": ["Other"]}
        ]});
        let (root, requests) = serve(vec![
            html(&manga_page("B")),
            json_reply(list),
            json_reply(list2),
            json_reply(json!({"pageProps": {"chapter": {"md_images": []}}})),
        ]);
        let http = http("ComicK", root);
        let files = media(extract(
            &http,
            Url::parse("https://comick.io/comic/one-piece?lang=en&lang=fr&group=TCB").unwrap(),
        ))
        .await;
        assert!(files.is_empty());
        let requests = requests.join().unwrap();
        assert!(
            requests[1].starts_with("GET /comic/Ab1/chapters?lang=en%2Cfr&page=1&chap-order=1 ")
        );
        assert!(requests[1].contains("origin: https://comick.io"));
        assert!(
            requests[2].starts_with("GET /comic/Ab1/chapters?lang=en%2Cfr&page=2&chap-order=1 ")
        );
        // Only the TCB chapter is queued, and the manga page is not
        // requested again.
        assert!(requests[3].starts_with("GET /_next/data/B/comic/one-piece/h2-chapter-2-en.json?"));
    }

    #[tokio::test]
    async fn manga_queues_volumes_and_bare_chapters_in_api_order() {
        let list = json!({"limit": 300, "total": 2, "chapters": [
            {"hid": "h3", "chap": null, "vol": "3", "lang": "fr"},
            {"hid": "h4", "chap": "", "vol": null, "lang": "en"}
        ]});
        let empty = || json_reply(json!({"pageProps": {"chapter": {"md_images": []}}}));
        let (root, requests) = serve(vec![
            html(&manga_page("B")),
            json_reply(list),
            empty(),
            empty(),
        ]);
        let http = http("ComicK", root).with_options(Some(std::sync::Arc::new(
            json!({"chapter-reverse": true, "lang": ["de", "en"]})
                .as_object()
                .unwrap()
                .clone(),
        )));
        let files = media(extract(
            &http,
            Url::parse("https://comick.io/comic/one-piece/").unwrap(),
        ))
        .await;
        assert!(files.is_empty());
        let requests = requests.join().unwrap();
        assert!(
            requests[1].starts_with("GET /comic/Ab1/chapters?lang=de%2Cen&page=1&chap-order=0 ")
        );
        assert!(requests[2].starts_with("GET /_next/data/B/comic/one-piece/h3-volume-3-fr.json?"));
        assert!(requests[3].starts_with("GET /_next/data/B/comic/one-piece/h4.json?"));
    }
}
