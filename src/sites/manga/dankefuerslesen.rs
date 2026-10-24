//! Danke fürs Lesen chapters and manga, mirroring
//! gallery_dl/extractor/dankefuerslesen.py: everything comes from the
//! `/api/series/SLUG/` JSON, memoized per invocation.
//!
//! With the `zip` option a chapter is one `download_chapter` archive named
//! after the usual chapter directory, as upstream does.
use super::{Gallery, Memo, get, keywords, set, str_of, text};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use futures_util::StreamExt;
use serde_json::{Map, Value};
use url::Url;

pub static SITE: Site = Site {
    category: "dankefuerslesen",
    name: "Danke fürs Lesen",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CHAPTER, &MANGA, &CHAPTER_ZIP],
    api_key: false,
    cookies: None,
};

const ROOT: &str = "https://danke.moe";
const CHAPTER_DIRECTORY: &str = "{volume:?v/ />02}c{chapter:>03}{chapter_minor:?//}{title:?: //}";

static CHAPTER: Route = Route::new(
    "chapter",
    "{manga}_c{chapter:>03}{chapter_minor:?//}_{page:>03}.{extension}",
    &["{category}", "{manga}", CHAPTER_DIRECTORY],
    "{manga}_{chapter}{chapter_minor}_{page}",
);
/// `DankefuerslesenChapterExtractor._init` with `zip`: the last directory
/// level becomes the filename.
static CHAPTER_ZIP: Route = Route::new(
    "chapter",
    "{volume:?v/ />02}c{chapter:>03}{chapter_minor:?//}{title:?: //}.{extension}",
    &["{category}", "{manga}"],
    "{manga}_{chapter}{chapter_minor}_{page}",
);
static MANGA: Route = Route::new("manga", "{filename}.{extension}", &["{category}"], "");

#[derive(Debug, PartialEq)]
enum Target {
    /// Series slug and chapter string.
    Chapter { slug: String, ch: String },
    /// Series slug.
    Manga(String),
}

/// The `[\w-]+` prefix of a raw path segment.
fn word(segment: &str) -> Option<&str> {
    let end = segment
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
        .unwrap_or(segment.len());
    (1..=128).contains(&end).then(|| &segment[..end])
}

/// `danke\.moe/read(?:er)?/(?:manga|series)/([\w-]+)/([\w-]+)` and
/// `danke\.moe/read(?:er)?/(?:manga|series)/([\w-]+)`, both unanchored.
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::art::on_host(url, &["danke.moe"]) {
        return None;
    }
    let raw: Vec<&str> = url.path().split('/').skip(1).collect();
    let ["read" | "reader", "manga" | "series", name, rest @ ..] = raw.as_slice() else {
        return None;
    };
    let slug = word(name)?;
    let chapter = rest
        .first()
        .filter(|_| slug.len() == name.len())
        .and_then(|segment| word(segment));
    Some(match chapter {
        Some(ch) => Target::Chapter {
            slug: slug.to_owned(),
            ch: ch.to_owned(),
        },
        None => Target::Manga(slug.to_owned()),
    })
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Chapter { slug, ch }) => chapter_events(http, slug, ch, Memo::default()),
        Some(Target::Manga(slug)) => {
            crate::sites::once(async move { manga_events(http, &slug).await })
        }
        None => crate::sites::fail(error("unsupported Danke fürs Lesen URL")),
    }
}

/// `DankefuerslesenBase._manga_info`, memoized by slug.
async fn manga_info(http: &Http, slug: &str, memo: &Memo) -> Result<Value> {
    if let Some(info) = memo.get(slug) {
        return Ok(info);
    }
    let info = http
        .json(&http.api(&format!("{ROOT}/api/series/{slug}/"))?)
        .await?;
    memo.insert(slug, info.clone());
    Ok(info)
}

/// `DankefuerslesenChapterExtractor`: the chapter's first group's files,
/// or its ZIP archive with `zip`.
fn chapter_events(http: &Http, slug: String, ch: String, memo: Memo) -> Events<'_> {
    crate::sites::once(async move {
        let manga = manga_info(http, &slug, &memo).await?;
        let (chapter, key, minor) = match ch.rsplit_once('-') {
            Some((chapter, minor)) => (chapter, ch.replace('-', "."), format!(".{minor}")),
            None => (ch.as_str(), ch.clone(), String::new()),
        };
        let data = manga
            .pointer(&format!(
                "/chapters/{}",
                key.replace('~', "~0").replace('/', "~1")
            ))
            .ok_or_else(|| error("Danke fürs Lesen chapter not found"))?;
        let (group_id, files) = data
            .get("groups")
            .and_then(Value::as_object)
            .and_then(|groups| groups.iter().next())
            .ok_or_else(|| error("Danke fürs Lesen chapter has no group"))?;
        let timestamp = data
            .pointer(&format!("/release_date/{group_id}"))
            .and_then(crate::sites::art::parse_ts);
        let group: Vec<&str> = manga
            .pointer(&format!("/groups/{group_id}"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .split(" & ")
            .collect();
        let mut meta = keywords([
            ("manga", get(&manga, "title")),
            ("manga_slug", get(&manga, "slug")),
            ("author", get(&manga, "author")),
            ("artist", get(&manga, "artist")),
            ("description", get(&manga, "description")),
            ("title", get(data, "title")),
            ("volume", text::parse_int_value(data.get("volume")).into()),
            ("chapter", text::parse_int(chapter).into()),
            ("chapter_minor", minor.into()),
            ("group", group.into()),
            ("group_id", text::parse_int(group_id).into()),
            ("date", text::date(timestamp)),
            ("lang", Value::Null),
            ("language", Value::Null),
        ]);
        let id = format!("{slug}/{ch}");
        if http.option_bool("zip", false) {
            // `GalleryExtractor.items` with one asset and no images.
            set(&mut meta, "count", 0);
            let url = format!("{ROOT}/api/download_chapter/{slug}/{ch}/");
            let mut asset = keywords([
                ("type", "archive".into()),
                ("extension", "zip".into()),
                ("url", url.clone().into()),
            ]);
            crate::sites::art::update(&mut asset, &meta);
            set(&mut asset, "page", 0);
            let url = Url::parse(&url).map_err(|_| error("invalid Danke fürs Lesen URL"))?;
            let item = Item::new(url, asset).ids(id, "0").timestamp(timestamp);
            return Ok(crate::sites::events(crate::sites::post(
                &SITE,
                http,
                &CHAPTER_ZIP,
                vec![item],
            )));
        }
        let base = format!(
            "{ROOT}/media/manga/{slug}/chapters/{}/{group_id}/",
            str_of(data, "folder")
        );
        let pages = files
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .map(|file| (format!("{base}{}", crate::sites::scalar(file)), None))
            .collect();
        Ok(crate::sites::events(
            Gallery::chapter(&CHAPTER, meta, pages)
                .id(id)
                .timestamp(timestamp)
                .events(&SITE, http),
        ))
    })
}

/// `DankefuerslesenMangaExtractor`: every chapter in API order
/// (`reverse = False`), sharing the memoized series data.
async fn manga_events<'a>(http: &'a Http, slug: &str) -> Result<Events<'a>> {
    let memo = Memo::default();
    let mut manga = match manga_info(http, slug, &memo).await? {
        Value::Object(manga) => manga,
        _ => return Err(error("Danke fürs Lesen returned no series")),
    };
    set(&mut manga, "lang", Value::Null);
    set(&mut manga, "language", Value::Null);
    let chapters = match manga.remove("chapters") {
        Some(Value::Object(chapters)) => chapters,
        _ => Map::new(),
    };
    let series = str_of(&Value::Object(manga.clone()), "slug").to_owned();
    let mut children = Vec::new();
    for (ch, data) in chapters {
        let mut data = match data {
            Value::Object(data) => data,
            _ => Map::new(),
        };
        let ch = match ch.rsplit_once('.') {
            Some((chapter, minor)) => {
                set(&mut data, "chapter", text::parse_int(chapter));
                set(&mut data, "chapter_minor", format!(".{minor}"));
                ch.replace('.', "-")
            }
            None => {
                set(&mut data, "chapter", text::parse_int(&ch));
                set(&mut data, "chapter_minor", "");
                ch
            }
        };
        let mut info = manga.clone();
        crate::sites::art::update(&mut info, &data);
        children.push((ch, info));
    }
    if http.option_bool("chapter-reverse", false) {
        children.reverse();
    }
    let streams: Vec<Events<'a>> = children
        .into_iter()
        .filter_map(|(ch, data)| {
            // The queued URL is `{ROOT}/read/manga/SLUG/CH/1/`.
            let url = Url::parse(&format!("{ROOT}/read/manga/{series}/{ch}/1/")).ok()?;
            let Some(Target::Chapter { slug, ch }) = target(&url) else {
                return None;
            };
            let memo = memo.clone();
            Some(super::queue_with(&SITE, &MANGA, &data, move || {
                chapter_events(http, slug, ch, memo)
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
            t("https://danke.moe/read/manga/alice-in-fairy-tale/7-5/1/"),
            Some(Target::Chapter {
                slug: "alice-in-fairy-tale".into(),
                ch: "7-5".into(),
            })
        );
        assert_eq!(
            t("http://www.danke.moe/reader/series/x/12"),
            Some(Target::Chapter {
                slug: "x".into(),
                ch: "12".into(),
            })
        );
        assert_eq!(
            t("https://danke.moe/read/manga/alice-in-fairy-tale/"),
            Some(Target::Manga("alice-in-fairy-tale".into()))
        );
        assert_eq!(
            t("https://danke.moe/read/manga/abc.def/1/"),
            Some(Target::Manga("abc".into()))
        );
        for raw in [
            "https://danke.moe/read/manga/",
            "https://danke.moe/read/comic/x/",
            "https://cdn.danke.moe/read/manga/x/",
            "https://danke.moe.evil.test/read/manga/x/",
            "https://danke.moe:8443/read/manga/x/",
            "https://u:p@danke.moe/read/manga/x/",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn series() -> Value {
        json!({
            "slug": "alice", "title": "Alice & Co", "author": "A", "artist": "B",
            "description": "D",
            "groups": {"3": "Danke & Friends"},
            "chapters": {
                "1": {"volume": "1", "title": "Start", "folder": "0001_ab",
                       "groups": {"3": ["001.png"]}, "release_date": {"3": 1600000000}},
                "7.5": {"volume": "2", "title": "Middle", "folder": "0075_cd",
                         "groups": {"3": ["01.jpg", "02.jpg"]},
                         "release_date": {"3": 1700000000}}
            }
        })
    }

    #[tokio::test]
    async fn extracts_chapter_from_series_api() {
        let (root, requests) = serve(vec![json(series())]);
        let http = http("Danke fürs Lesen", root);
        let files = media(extract(
            &http,
            Url::parse("https://danke.moe/read/manga/alice/7-5/1/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[1].url.as_str(),
            "https://danke.moe/media/manga/alice/chapters/0075_cd/3/02.jpg"
        );
        let k = kw(&files[1]);
        assert_eq!(k["manga"], "Alice & Co");
        assert_eq!(k["manga_slug"], "alice");
        assert_eq!(k["title"], "Middle");
        assert_eq!(k["volume"], 2);
        assert_eq!(k["chapter"], 7);
        assert_eq!(k["chapter_minor"], ".5");
        assert_eq!(k["group"], json!(["Danke", "Friends"]));
        assert_eq!(k["group_id"], 3);
        assert_eq!(k["date"], "2023-11-14 22:13:20");
        assert_eq!(k["lang"], Value::Null);
        assert_eq!(k["page"], 2);
        assert_eq!(k["count"], 2);
        assert_eq!(
            path(&files[1]),
            "dankefuerslesen/Alice & Co/v02 c007.5: Middle/Alice & Co_c007.5_002.jpg"
        );
        assert_eq!(
            archive(&files[1]),
            r#"["{manga}_{chapter}{chapter_minor}_{page}",["Alice & Co","7",".5","2"]]"#
        );
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /api/series/alice/ "));
    }

    #[tokio::test]
    async fn zip_option_downloads_the_chapter_archive() {
        let (root, requests) = serve(vec![json(series())]);
        let http = http("Danke fürs Lesen", root).with_options(Some(std::sync::Arc::new(
            json!({"zip": true}).as_object().unwrap().clone(),
        )));
        let files = media(extract(
            &http,
            Url::parse("https://danke.moe/read/manga/alice/7-5/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].url.as_str(),
            "https://danke.moe/api/download_chapter/alice/7-5/"
        );
        let k = kw(&files[0]);
        assert_eq!(k["type"], "archive");
        assert_eq!(k["extension"], "zip");
        assert_eq!(k["page"], 0);
        assert_eq!(k["count"], 0);
        assert_eq!(
            path(&files[0]),
            "dankefuerslesen/Alice & Co/v02 c007.5: Middle.zip"
        );
        assert_eq!(
            archive(&files[0]),
            r#"["{manga}_{chapter}{chapter_minor}_{page}",["Alice & Co","7",".5","0"]]"#
        );
        requests.join().unwrap();
    }

    #[tokio::test]
    async fn missing_chapter_fails() {
        let (root, requests) = serve(vec![json(series())]);
        let http = http("Danke fürs Lesen", root);
        assert!(
            crate::sites::testing::fails(extract(
                &http,
                Url::parse("https://danke.moe/read/manga/alice/9/").unwrap(),
            ))
            .await
        );
        requests.join().unwrap();
    }

    #[tokio::test]
    async fn manga_queues_chapters_in_api_order_with_one_request() {
        let (root, requests) = serve(vec![json(series())]);
        let http = http("Danke fürs Lesen", root);
        let files = media(extract(
            &http,
            Url::parse("https://danke.moe/read/manga/alice/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 3);
        assert_eq!(kw(&files[0])["chapter"], 1);
        assert_eq!(kw(&files[1])["chapter"], 7);
        assert_eq!(requests.join().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn manga_children_are_chapter_jobs() {
        let (root, requests) = serve(vec![json(series())]);
        let http = http("Danke fürs Lesen", root);
        let children = queued(extract(
            &http,
            Url::parse("https://danke.moe/read/manga/alice/").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].subcategory(), "manga");
        requests.join().unwrap();
    }
}
