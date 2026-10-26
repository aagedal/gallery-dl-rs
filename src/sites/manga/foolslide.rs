//! FoOlSlide readers, mirroring gallery_dl/extractor/foolslide.py.
//!
//! Upstream ships no built-in instances; readers are addressed as
//! `foolslide:https://host/read/...` or `foolslide:https://host/series/...`.
//! Files use the `foolslide` category rather than the reader's host.
use super::set;
use crate::error;
use crate::sites::gdl::{Scan, code_to_language, extr, int_of, parse_int, partition, re, unescape};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

static CHAPTER: Route = Route::new(
    "chapter",
    "{manga}_c{chapter:>03}{chapter_minor:?//}_{page:>03}.{extension}",
    &["{category}", "{manga}", "{chapter_string}"],
    "{id}",
);
static MANGA: Route = super::manga_route();

pub static SITE: Site = Site {
    category: "foolslide",
    name: "FoOlSlide",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CHAPTER, &MANGA],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    /// Reader root and chapter path.
    Chapter(String, String),
    Manga(String, String),
}

fn target(url: &Url) -> Option<Target> {
    let raw = url.as_str();
    let chapter = re!(r"^foolslide:(https?://[^/?#]+)(/read/[^/?#]+/[a-z-]+/\d+/\d+(?:/\d+)?)");
    if let Some(caps) = chapter.captures(raw) {
        return Some(Target::Chapter(caps[1].into(), caps[2].into()));
    }
    let manga = re!(r"^foolslide:(https?://[^/?#]+)(/series/[^/?#]+)");
    manga
        .captures(raw)
        .map(|caps| Target::Manga(caps[1].into(), caps[2].into()))
}

/// Upstream requests every page as `POST adult=true`.
async fn request(http: &Http, url: &str) -> crate::Result<String> {
    http.post_form_text(&http.api(url)?, &[("adult", "true")], &[])
        .await
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported FoOlSlide URL"));
    };
    crate::sites::once(async move {
        Ok(match target {
            Target::Chapter(root, path) => {
                let page_url = format!("{root}{path}");
                let page = request(http, &page_url).await?;
                crate::sites::events(chapter_events(http, &page, &page_url)?)
            }
            Target::Manga(root, path) => {
                let page = request(http, &format!("{root}{path}")).await?;
                super::queue_chapters(http, &SITE, &MANGA, chapters(&page), true)
            }
        })
    })
}

/// `parse_chapter_url`: language, volume and chapter from the reader path.
fn parse_chapter_url(url: &str, data: &mut Map<String, Value>) {
    let info: Vec<&str> = partition(url, "/read/")
        .2
        .trim_end_matches('/')
        .split('/')
        .collect();
    let part = |i: usize| info.get(i).copied().unwrap_or_default();
    let lang = partition(part(1), "-").0;
    set(data, "lang", lang);
    set(
        data,
        "language",
        code_to_language(lang).map_or(Value::Null, Value::from),
    );
    set(data, "volume", parse_int(part(2)));
    set(data, "chapter", parse_int(part(3)));
    set(
        data,
        "chapter_minor",
        if info.len() >= 5 {
            format!(".{}", part(4))
        } else {
            String::new()
        },
    );
    let title = partition(
        crate::sites::gdl::str_of(&Value::Object(data.clone()), "chapter_string"),
        ":",
    )
    .2
    .trim()
    .to_owned();
    set(data, "title", title);
}

fn chapter_events(
    http: &Http,
    page: &str,
    page_url: &str,
) -> crate::Result<Vec<crate::event::ExtractionEvent>> {
    let mut scan = Scan::new(page);
    scan.extr("<h1 class=\"tbtitle dnone\">", "");
    let mut data = Map::new();
    set(
        &mut data,
        "manga",
        unescape(scan.extr("title=\"", "\"")).trim(),
    );
    set(
        &mut data,
        "chapter_string",
        unescape(scan.extr("title=\"", "\"")),
    );
    parse_chapter_url(page_url, &mut data);
    let images: Vec<Value> = serde_json::from_str(extr(page, "var pages = ", ";"))
        .map_err(|_| error("FoOlSlide chapter has no page list"))?;
    let first = images
        .first()
        .ok_or_else(|| error("FoOlSlide chapter has no pages"))?;
    set(&mut data, "count", images.len());
    set(&mut data, "chapter_id", int_of(first.get("chapter_id")));
    let reverse = http.option_bool("page-reverse", false);
    let total = images.len();
    let mut items = Vec::new();
    for (index, image) in images.into_iter().enumerate() {
        let mut image = image.as_object().cloned().unwrap_or_default();
        let url = image.remove("url");
        image.remove("chapter_id");
        image.remove("thumb_url");
        for key in ["height", "id", "size", "width"] {
            let value = int_of(image.get(key));
            set(&mut image, key, value);
        }
        let number = if reverse { total - index } else { index + 1 };
        set(&mut data, "page", number);
        data.extend(image);
        let filename = crate::sites::scalar(data.get("filename").unwrap_or(&Value::Null));
        let (name, ext) = crate::sites::gdl::nameext_from_name(&filename);
        set(&mut data, "filename", name);
        set(&mut data, "extension", ext);
        let Some(url) = url
            .as_ref()
            .and_then(Value::as_str)
            .and_then(|u| Url::parse(u).ok())
        else {
            continue;
        };
        let id = crate::sites::scalar(&data["id"]);
        items.push(Item::new(url, data.clone()).ids(crate::sites::scalar(&data["chapter_id"]), id));
    }
    Ok(crate::sites::post(&SITE, http, &CHAPTER, items))
}

fn chapters(page: &str) -> Vec<(Url, Map<String, Value>)> {
    let mut scan = Scan::new(page);
    let manga = unescape(scan.extr("<h1 class=\"title\">", "</h1>"))
        .trim()
        .to_owned();
    let author = scan.extr("<b>Author</b>: ", "<br").to_owned();
    let artist = scan.extr("<b>Artist</b>: ", "<br").to_owned();
    let mut results = Vec::new();
    while let Some(url) = scan
        .find("<div class=\"title\"><a href=\"", "\"")
        .filter(|u| !u.is_empty())
    {
        let mut data = Map::new();
        set(&mut data, "manga", manga.as_str());
        set(&mut data, "author", author.as_str());
        set(&mut data, "artist", artist.as_str());
        set(&mut data, "chapter_string", scan.extr("title=\"", "\""));
        set(&mut data, "group", scan.extr("title=\"", "\""));
        parse_chapter_url(url, &mut data);
        if let Ok(child) = Url::parse(&format!("foolslide:{url}")) {
            results.push((child, data));
        }
    }
    results
}

#[cfg(test)]
mod tests {
    use super::super::tests::{archive, path};
    use super::*;
    use crate::sites::testing::{children, html, http, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("foolslide:https://read.powermanga.org/read/one_piece/en/0/123/2/page/4"),
            Some(Target::Chapter(
                "https://read.powermanga.org".into(),
                "/read/one_piece/en/0/123/2".into()
            ))
        );
        assert_eq!(
            t("foolslide:https://reader.example/series/manga/"),
            Some(Target::Manga(
                "https://reader.example".into(),
                "/series/manga".into()
            ))
        );
        assert_eq!(t("https://read.powermanga.org/read/x/en/0/1/"), None);
    }

    #[tokio::test]
    async fn extracts_chapter_via_post() {
        let page = r#"<h1 class="tbtitle dnone"><a title="One &amp; Piece">x</a><a title="Chapter 123: Title">
            var pages = [{"id":"7","chapter_id":"55","url":"https://read.powermanga.org/content/a/01.png","thumb_url":"t",
            "filename":"01.png","height":"1200","width":"800","size":"123"},
            {"id":"8","chapter_id":"55","url":"https://read.powermanga.org/content/a/02.jpg","thumb_url":"t",
            "filename":"02.jpg","height":"1","width":"2","size":"3"}];"#;
        let (root, server) = serve(vec![html(page)]);
        let http = http("foolslide", root);
        let files = media(extract(
            &http,
            Url::parse("foolslide:https://read.powermanga.org/read/one_piece/en-us/1/123/2/")
                .unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[1]);
        assert_eq!(k["manga"], "One & Piece");
        assert_eq!(k["chapter_string"], "Chapter 123: Title");
        assert_eq!(k["title"], "Title");
        assert_eq!(k["lang"], "en");
        assert_eq!(k["language"], "English");
        assert_eq!(k["volume"], 1);
        assert_eq!(k["chapter_minor"], ".2");
        assert_eq!(k["chapter_id"], 55);
        assert_eq!(k["height"], 1);
        assert!(k.get("thumb_url").is_none());
        assert_eq!(
            path(&files[1]),
            "foolslide/One & Piece/Chapter 123: Title/One & Piece_c123.2_002.jpg"
        );
        assert_eq!(archive(&files[0]), r#"["{id}",["7"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("POST /read/one_piece/en-us/1/123/2 "));
        assert!(requests[0].ends_with("adult=true"));
    }

    #[tokio::test]
    async fn manga_queues_prefixed_chapters() {
        let page = r#"<h1 class="title"> Manga </h1><b>Author</b>: Au<br><b>Artist</b>: Ar<br>
            <div class="title"><a href="https://r.example/read/m/en/0/2/" title="Chapter 2: B">x</a><a title="Group">
            <div class="title"><a href="https://r.example/read/m/en/0/1/" title="Chapter 1: A">x</a><a title="Group">"#;
        let list = chapters(page);
        assert_eq!(list.len(), 2);
        assert_eq!(
            list[0].0.as_str(),
            "foolslide:https://r.example/read/m/en/0/2/"
        );
        assert_eq!(list[0].1["group"], "Group");
        assert_eq!(list[0].1["title"], "B");
        let (root, server) = serve(vec![html(page)]);
        let http = http("foolslide", root);
        assert_eq!(
            children(extract(
                &http,
                Url::parse("foolslide:https://r.example/series/m/").unwrap()
            ))
            .await
            .len(),
            2
        );
        server.join().unwrap();
    }
}
