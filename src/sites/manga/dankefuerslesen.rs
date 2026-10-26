//! Danke fürs Lesen chapters and manga, mirroring
//! gallery_dl/extractor/dankefuerslesen.py through the series API.
use super::{Gallery, set};
use crate::error;
use crate::sites::gdl::{captures, date, group, parse_int, parse_ts, re, rpartition, str_of};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://danke.moe";
const MANGA_PATTERN: &str = r"^https?://(?:www\.)?danke\.moe/read(?:er)?/(?:manga|series)/([\w-]+)";

static CHAPTER: Route = super::chapter_route();
/// The `zip` option moves the chapter directory into the filename.
static CHAPTER_ZIP: Route = Route::new(
    "chapter",
    "{volume:?v/ />02}c{chapter:>03}{chapter_minor:?//}{title:?: //}.{extension}",
    &["{category}", "{manga}"],
    super::CHAPTER_ARCHIVE,
);
static MANGA: Route = super::manga_route();

pub static SITE: Site = Site {
    category: "dankefuerslesen",
    name: "Danke fürs Lesen",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CHAPTER, &MANGA, &CHAPTER_ZIP],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Chapter(String, String),
    Manga(String),
}

fn target(url: &Url) -> Option<Target> {
    let chapter = re!(&format!(r"{MANGA_PATTERN}/([\w-]+)"));
    if let Some(caps) = captures(chapter, url) {
        return Some(Target::Chapter(
            group(&caps, 1).into(),
            group(&caps, 2).into(),
        ));
    }
    captures(re!(MANGA_PATTERN), url).map(|caps| Target::Manga(group(&caps, 1).into()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Danke fürs Lesen URL"));
    };
    crate::sites::once(async move {
        Ok(match target {
            Target::Chapter(slug, ch) => {
                let manga = http
                    .json(&http.api(&format!("{ROOT}/api/series/{slug}/"))?)
                    .await?;
                chapter(http, &manga, &slug, &ch)?
            }
            Target::Manga(slug) => {
                let manga = http
                    .json(&http.api(&format!("{ROOT}/api/series/{slug}/"))?)
                    .await?;
                super::queue_chapters(http, &SITE, &MANGA, chapters(&manga), false)
            }
        })
    })
}

fn chapter<'a>(http: &'a Http, manga: &Value, slug: &str, ch: &str) -> crate::Result<Events<'a>> {
    let (chapter, key, minor) = if ch.contains('-') {
        let (chapter, _, minor) = rpartition(ch, "-");
        (chapter, ch.replace('-', "."), format!(".{minor}"))
    } else {
        (ch, ch.to_owned(), String::new())
    };
    let missing = || error("Danke fürs Lesen chapter not found");
    let entry = manga["chapters"].get(&key).ok_or_else(missing)?;
    let (group_id, files) = entry["groups"]
        .as_object()
        .and_then(|groups| groups.iter().next())
        .ok_or_else(missing)?;
    let mut data = Map::new();
    set(&mut data, "manga", manga["title"].clone());
    set(&mut data, "manga_slug", manga["slug"].clone());
    set(&mut data, "author", manga["author"].clone());
    set(&mut data, "artist", manga["artist"].clone());
    set(&mut data, "description", manga["description"].clone());
    set(&mut data, "title", entry["title"].clone());
    set(
        &mut data,
        "volume",
        crate::sites::gdl::int_of(entry.get("volume")),
    );
    set(&mut data, "chapter", parse_int(chapter));
    set(&mut data, "chapter_minor", minor);
    let groups: Vec<String> = str_of(&manga["groups"], group_id)
        .split(" & ")
        .map(str::to_owned)
        .collect();
    set(&mut data, "group", groups);
    set(&mut data, "group_id", parse_int(group_id));
    set(
        &mut data,
        "date",
        date(entry["release_date"].get(group_id).and_then(parse_ts)),
    );
    set(&mut data, "lang", Value::Null);
    set(&mut data, "language", Value::Null);
    if http.option_bool("zip", false) {
        // GalleryExtractor assets: the chapter archive numbered 0.
        let url = format!("{ROOT}/api/download_chapter/{slug}/{ch}/");
        let mut asset = data;
        set(&mut asset, "count", 0);
        set(&mut asset, "page", 0);
        set(&mut asset, "type", "archive");
        set(&mut asset, "extension", "zip");
        set(&mut asset, "url", url.as_str());
        let (name, _) = crate::sites::nameext(&Url::parse(&url).map_err(|_| missing())?);
        set(&mut asset, "filename", name);
        let item = Item::new(Url::parse(&url).map_err(|_| missing())?, asset);
        return Ok(crate::sites::events(crate::sites::post(
            &SITE,
            http,
            &CHAPTER_ZIP,
            vec![item],
        )));
    }
    let base = format!(
        "{ROOT}/media/manga/{slug}/chapters/{}/{group_id}/",
        crate::sites::scalar(&entry["folder"])
    );
    let pages = files
        .as_array()
        .into_iter()
        .flatten()
        .map(|file| (format!("{base}{}", crate::sites::scalar(file)), None))
        .collect();
    Ok(crate::sites::events(
        Gallery::chapter(&CHAPTER, data, pages).events(&SITE, http),
    ))
}

fn chapters(manga: &Value) -> Vec<(Url, Map<String, Value>)> {
    let mut shared = manga.as_object().cloned().unwrap_or_default();
    shared.remove("chapters");
    set(&mut shared, "lang", Value::Null);
    set(&mut shared, "language", Value::Null);
    let base = format!("{ROOT}/read/manga/{}/", str_of(manga, "slug"));
    manga["chapters"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(ch, entry)| {
            let mut entry = entry.as_object().cloned().unwrap_or_default();
            let path = if ch.contains('.') {
                let (chapter, sep, minor) = rpartition(ch, ".");
                set(&mut entry, "chapter", parse_int(chapter));
                set(&mut entry, "chapter_minor", format!("{sep}{minor}"));
                ch.replace('.', "-")
            } else {
                set(&mut entry, "chapter", parse_int(ch));
                set(&mut entry, "chapter_minor", "");
                ch.clone()
            };
            let mut data = shared.clone();
            data.extend(entry);
            Some((Url::parse(&format!("{base}{path}/1/")).ok()?, data))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::tests::path;
    use super::*;
    use crate::sites::testing::{children, http, json, keywords, media, serve};
    use serde_json::json;

    fn series() -> Value {
        json!({"title": "Awana-chan", "slug": "awana", "author": "A", "artist": "B",
            "description": "D", "groups": {"3": "Danke & Friends"},
            "chapters": {
                "2": {"volume": "1", "title": "Two", "folder": "f2", "groups": {"3": ["01.png", "02.png"]},
                      "release_date": {"3": 1600000000}},
                "7.5": {"volume": null, "title": "", "folder": "f7", "groups": {"3": ["a.jpg"]},
                      "release_date": {"3": 1600000001}}}})
    }

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://danke.moe/read/manga/raul-and-the-vampire/7-5/1/"),
            Some(Target::Chapter("raul-and-the-vampire".into(), "7-5".into()))
        );
        assert_eq!(
            t("https://danke.moe/reader/series/awana/2/1/"),
            Some(Target::Chapter("awana".into(), "2".into()))
        );
        assert_eq!(
            t("https://danke.moe/read/series/awana"),
            Some(Target::Manga("awana".into()))
        );
        assert_eq!(
            t("https://danke.moe/read/manga/awana/"),
            Some(Target::Manga("awana".into()))
        );
        assert_eq!(t("https://danke.moe/about"), None);
    }

    #[tokio::test]
    async fn extracts_chapter() {
        let (root, server) = serve(vec![json(series())]);
        let http = http("dankefuerslesen", root);
        let files = media(extract(
            &http,
            Url::parse("https://danke.moe/read/manga/awana/2/1/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[1].url.as_str(),
            "https://danke.moe/media/manga/awana/chapters/f2/3/02.png"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["group"], json!(["Danke", "Friends"]));
        assert_eq!(k["group_id"], 3);
        assert_eq!(k["volume"], 1);
        assert_eq!(k["date"], "2020-09-13 12:26:40");
        assert_eq!(k["lang"], Value::Null);
        assert_eq!(
            path(&files[1]),
            crate::sites::testing::os_path(
                "dankefuerslesen/Awana-chan/v01 c002: Two/Awana-chan_c002_002.png"
            )
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/series/awana/ "));
    }

    #[tokio::test]
    async fn zip_option_downloads_the_archive() {
        let (root, server) = serve(vec![json(series())]);
        let http = http("dankefuerslesen", root).with_options(Some(std::sync::Arc::new(
            json!({"zip": true}).as_object().unwrap().clone(),
        )));
        let files = media(extract(
            &http,
            Url::parse("https://danke.moe/read/manga/awana/7-5/1/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].url.as_str(),
            "https://danke.moe/api/download_chapter/awana/7-5/"
        );
        assert_eq!(path(&files[0]), "dankefuerslesen/Awana-chan/c007.5.zip");
        server.join().unwrap();
    }

    #[tokio::test]
    async fn manga_queues_chapters_in_order() {
        let list = chapters(&series());
        assert_eq!(
            list[1].0.as_str(),
            "https://danke.moe/read/manga/awana/7-5/1/"
        );
        assert_eq!(list[1].1["chapter_minor"], ".5");
        assert!(list[0].1.get("chapters").is_none());
        let (root, server) = serve(vec![json(series())]);
        let http = http("dankefuerslesen", root);
        assert_eq!(
            children(extract(
                &http,
                Url::parse("https://danke.moe/read/manga/awana").unwrap()
            ))
            .await
            .len(),
            2
        );
        server.join().unwrap();
    }
}
