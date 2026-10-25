//! FoOlSlide readers on configured instances, mirroring
//! gallery_dl/extractor/foolslide.py. Upstream has no built-in instances:
//! `extractor.foolslide.<category> = {"root": ...}` definitions and
//! `foolslide:https://host/...` inputs go through `crate::sites::instances`.
//!
//! Every page is requested like upstream, as a POST with `adult=true` so
//! mature series skip their confirmation page.
use super::{Gallery, Page, text};
use crate::sites::art::json_loads;
use crate::sites::http::Body;
use crate::sites::instances::{Family, Instance, below, configured};
use crate::sites::{Events, Http, Route, Site, unescape};
use crate::{Result, error};
use serde_json::{Map, Value};
use std::sync::LazyLock;
use url::Url;

static CHAPTER: Route = Route::new(
    "chapter",
    "{manga}_c{chapter:>03}{chapter_minor:?//}_{page:>03}.{extension}",
    &["{category}", "{manga}", "{chapter_string}"],
    "{id}",
);
static MANGA: Route = Route::new("manga", "{filename}.{extension}", &["{category}"], "");

/// Routes and flags for configured instances; not part of the registry.
static TEMPLATE: Site = Site {
    category: "foolslide",
    name: "FoOlSlide",
    matches: |_| false,
    extract: |_, _| crate::sites::fail(error("foolslide needs a configured instance")),
    routes: &[&CHAPTER, &MANGA],
    api_key: false,
    cookies: Some("foolslide"),
};

/// Configured `extractor.foolslide.<category>` instances.
pub(crate) static FAMILY: Family = Family {
    basecategory: "foolslide",
    builtin: &[],
    related: &[],
    template: &TEMPLATE,
    matches: |category, url| target(category, url).is_some(),
    extract,
};

/// `(/read/[^/?#]+/[a-z-]+/\d+/\d+(?:/\d+)?)` after the root.
static CHAPTER_PATH: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"^/read/[^/?#]+/[a-z-]+/\d+/\d+(?:/\d+)?").expect("static pattern")
});
/// `(/series/[^/?#]+)` after the root.
static MANGA_PATH: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^/series/[^/?#]+").expect("static pattern"));

#[derive(Debug, PartialEq)]
enum Target {
    /// The matched `/read/...` path.
    Chapter(String),
    /// The matched `/series/NAME` path.
    Manga(String),
}

fn target(category: &str, url: &Url) -> Option<Target> {
    let instance = configured(&FAMILY, category)?;
    let rest = below(url, instance.spec)?;
    if let Some(found) = CHAPTER_PATH.find(&rest) {
        return Some(Target::Chapter(found.as_str().to_owned()));
    }
    MANGA_PATH
        .find(&rest)
        .map(|found| Target::Manga(found.as_str().to_owned()))
}

fn extract<'a>(category: &'static str, http: &'a Http, url: Url) -> Events<'a> {
    let (Some(instance), Some(target)) = (configured(&FAMILY, category), target(category, &url))
    else {
        return crate::sites::fail(error("unsupported FoOlSlide URL"));
    };
    match target {
        Target::Chapter(path) => crate::sites::once(async move {
            let page_url = format!("{}{path}", instance.root);
            let page = request(http, &page_url).await?;
            chapter_events(instance, http, &page_url, &page)
        }),
        Target::Manga(path) => crate::sites::once(async move {
            let page = request(http, &format!("{}{path}", instance.root)).await?;
            let mut children = chapters(&page);
            if !http.option_bool("chapter-reverse", false) {
                children.reverse();
            }
            Ok(super::queue_urls(http, &instance.site, &MANGA, children))
        }),
    }
}

/// Upstream `FoolslideExtractor.request`: POST `adult=true`, read as UTF-8.
async fn request(http: &Http, url: &str) -> Result<String> {
    let bytes = http
        .post(&http.api(url)?, Body::Form(&[("adult", "true")]), &[])
        .await?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Upstream `parse_chapter_url`: language, volume and chapter numbers from
/// `/read/MANGA/LANG/VOLUME/CHAPTER[/MINOR]`, the title from
/// `chapter_string`.
fn parse_chapter_url(url: &str, data: &mut Map<String, Value>) -> Option<()> {
    let (_, _, tail) = text::partition(url, "/read/");
    let info: Vec<&str> = tail.trim_end_matches('/').split('/').collect();
    let lang = text::partition(info.get(1)?, "-").0.to_owned();
    let chapter = info.get(3)?;
    data.insert(
        "language".into(),
        text::code_to_language(&lang).map_or(Value::Null, Value::from),
    );
    data.insert("lang".into(), lang.into());
    data.insert("volume".into(), text::parse_int(info[2]).into());
    data.insert("chapter".into(), text::parse_int(chapter).into());
    let minor = info.get(4).map(|m| format!(".{m}")).unwrap_or_default();
    data.insert("chapter_minor".into(), minor.into());
    let chapter_string = data
        .get("chapter_string")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let title = text::partition(chapter_string, ":").2.trim().to_owned();
    data.insert("title".into(), title.into());
    Some(())
}

/// `FoolslideChapterExtractor.items`: one post of the `var pages` images.
fn chapter_events<'a>(
    instance: &'static Instance,
    http: &'a Http,
    page_url: &str,
    page: &str,
) -> Result<Events<'a>> {
    let (_, pos) = text::extract(page, "<h1 class=\"tbtitle dnone\">", "", 0);
    let (manga, pos) = text::extract(page, "title=\"", "\"", pos);
    let (chapter_string, _) = text::extract(page, "title=\"", "\"", pos);
    let mut data = Map::new();
    data.insert(
        "manga".into(),
        unescape(manga.unwrap_or_default()).trim().into(),
    );
    data.insert(
        "chapter_string".into(),
        unescape(chapter_string.unwrap_or_default()).into(),
    );
    parse_chapter_url(page_url, &mut data)
        .ok_or_else(|| error("FoOlSlide chapter URL lacks its chapter number"))?;
    let images = match json_loads(
        crate::sites::art::extr(page, "var pages = ", ";"),
        "FoOlSlide",
    )? {
        Value::Array(images) => images,
        _ => return Err(error("FoOlSlide chapter page lists no images")),
    };
    let chapter_id = images
        .first()
        .and_then(|image| image.get("chapter_id"))
        .map(|id| text::parse_int_value(Some(id)))
        .ok_or_else(|| error("FoOlSlide chapter page lists no images"))?;
    data.insert("count".into(), images.len().into());
    data.insert("chapter_id".into(), chapter_id.into());
    let pages: Vec<Page> = images
        .into_iter()
        .filter_map(|image| {
            let mut image = match image {
                Value::Object(image) => image,
                _ => return None,
            };
            let url = image.get("url").and_then(Value::as_str)?.to_owned();
            for key in ["url", "chapter_id", "thumb_url"] {
                image.shift_remove(key);
            }
            for key in ["height", "id", "size", "width"] {
                if let Some(value) = image.get(key) {
                    let number = text::parse_int_value(Some(value));
                    image.insert(key.into(), number.into());
                }
            }
            // `text.nameext_from_url(data["filename"], data)`.
            let filename = image
                .get("filename")
                .map(crate::sites::scalar)
                .unwrap_or_default();
            crate::sites::art::nameext_into(&filename, &mut image);
            Some((url, Some(image)))
        })
        .collect();
    Ok(crate::sites::events(
        Gallery::chapter(&CHAPTER, data, pages)
            .id(chapter_id.to_string())
            .root(instance.root)
            .events(&instance.site, http),
    ))
}

/// `FoolslideMangaExtractor.chapters`: chapter links with their keywords,
/// in page order.
fn chapters(page: &str) -> Vec<(Url, Map<String, Value>)> {
    let (manga, pos) = text::extract(page, "<h1 class=\"title\">", "</h1>", 0);
    let (author, pos) = text::extract(page, "<b>Author</b>: ", "<br", pos);
    let (artist, mut pos) = text::extract(page, "<b>Artist</b>: ", "<br", pos);
    let manga = unescape(manga.unwrap_or_default()).trim().to_owned();
    let mut results = Vec::new();
    loop {
        let (url, next) = text::extract(page, "<div class=\"title\"><a href=\"", "\"", pos);
        let Some(url) = url.filter(|url| !url.is_empty()) else {
            return results;
        };
        let (chapter_string, next) = text::extract(page, "title=\"", "\"", next);
        let (group, next) = text::extract(page, "title=\"", "\"", next);
        pos = next;
        let mut data = Map::new();
        data.insert("manga".into(), manga.clone().into());
        data.insert("author".into(), author.unwrap_or_default().into());
        data.insert("artist".into(), artist.unwrap_or_default().into());
        data.insert(
            "chapter_string".into(),
            chapter_string.unwrap_or_default().into(),
        );
        data.insert("group".into(), group.unwrap_or_default().into());
        if parse_chapter_url(url, &mut data).is_none() {
            continue;
        }
        if let Ok(url) = Url::parse(url.trim()) {
            results.push((url, data));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{archive, path};
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, queued, serve};
    use serde_json::json;

    fn define() -> &'static Instance {
        crate::sites::instances::define(
            "foolslide",
            "foolslide-configured",
            json!({"root": "https://reader.fool.test/slide/",
                "pattern": r"(?:www\.)?reader\.fool\.test/slide"}),
        )
    }

    #[test]
    fn classifies_configured_urls() {
        define();
        let t = |raw: &str| target("foolslide-configured", &Url::parse(raw).unwrap());
        assert_eq!(
            t("https://reader.fool.test/slide/read/my_manga/en/0/12/"),
            Some(Target::Chapter("/read/my_manga/en/0/12".into()))
        );
        assert_eq!(
            t("https://reader.fool.test/slide/read/my_manga/en-us/1/12/5/page/3"),
            Some(Target::Chapter("/read/my_manga/en-us/1/12/5".into()))
        );
        assert_eq!(
            t("https://reader.fool.test/slide/series/my_manga/"),
            Some(Target::Manga("/series/my_manga".into()))
        );
        for raw in [
            "https://reader.fool.test/slide/read/my_manga/EN/0/12/",
            "https://reader.fool.test/slide/read/my_manga/en/x/12/",
            "https://reader.fool.test/slide/series/",
            "https://reader.fool.test/read/my_manga/en/0/12/",
            "https://other.test/slide/series/x",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
        // Aliases matched by the instance pattern resolve to its root.
        let url = Url::parse("https://www.reader.fool.test/slide/series/x").unwrap();
        let site = crate::sites::find(&url).unwrap();
        assert_eq!(site.category, "foolslide-configured");
        assert_eq!(
            crate::sites::find(&Url::parse("https://reader.fool.test/slide/about").unwrap())
                .map(|site| site.category),
            None
        );
    }

    const CHAPTER_PAGE: &str = r#"<h1 class="tbtitle dnone"><a href="x" title="My &amp; Manga ">My Manga</a>
<a href="y" title="Chapter 12: The &quot;Start&quot;">c</a></h1>
<script>var pages = [{"id":"101","chapter_id":"55","filename":"01.jpg","url":"https://cdn.fool.test/c/01.jpg","thumb_url":"t","width":"800","height":"1200","size":"1234"},
{"id":"102","chapter_id":"55","filename":"02.png","url":"https://cdn.fool.test/c/02.png","thumb_url":"t","width":"800","height":"1200","size":"99"}];</script>"#;

    #[tokio::test]
    async fn configured_instance_extracts_chapters() {
        let instance = define();
        let (root, requests) = serve(vec![html(CHAPTER_PAGE)]);
        let http = http("foolslide", root);
        let url = Url::parse("https://reader.fool.test/slide/read/my_manga/en/0/12/5/").unwrap();
        let site = crate::sites::find(&url).unwrap();
        assert!(std::ptr::eq(site, &instance.site));
        let files = media((site.extract)(&http, url)).await;
        assert_eq!(files.len(), 2);
        assert_eq!(files[1].url.as_str(), "https://cdn.fool.test/c/02.png");
        let k = keywords(&files[0]);
        assert_eq!(k["category"], "foolslide-configured");
        assert_eq!(k["subcategory"], "chapter");
        assert_eq!(k["manga"], "My & Manga");
        assert_eq!(k["chapter_string"], r#"Chapter 12: The "Start""#);
        assert_eq!(k["title"], r#"The "Start""#);
        assert_eq!(k["lang"], "en");
        assert_eq!(k["language"], "English");
        assert_eq!(k["volume"], 0);
        assert_eq!(k["chapter"], 12);
        assert_eq!(k["chapter_minor"], ".5");
        assert_eq!(k["chapter_id"], 55);
        assert_eq!(k["count"], 2);
        assert_eq!(k["page"], 1);
        assert_eq!(k["id"], 101);
        assert_eq!(k["width"], 800);
        assert_eq!(k["size"], 1234);
        assert_eq!(k["filename"], "01");
        assert_eq!(k["extension"], "jpg");
        assert!(k.get("url").is_none());
        assert!(k.get("thumb_url").is_none());
        // Windows path sanitization replaces `:` and `"` in the chapter string.
        let chapter_dir = if cfg!(windows) {
            "Chapter 12_ The _Start_"
        } else {
            r#"Chapter 12: The "Start""#
        };
        assert_eq!(
            path(&files[1]),
            format!("foolslide-configured/My & Manga/{chapter_dir}/My & Manga_c012.5_002.png")
        );
        assert_eq!(archive(&files[1]), r#"["{id}",["102"]]"#);
        let requests = requests.join().unwrap();
        assert!(
            requests[0].starts_with("POST /slide/read/my_manga/en/0/12/5 "),
            "{}",
            requests[0]
        );
        assert!(requests[0].ends_with("adult=true"));
    }

    #[tokio::test]
    async fn configured_instance_queues_manga_chapters() {
        define();
        let page = r#"<h1 class="title"> Series &amp; Co </h1>
<b>Author</b>: Writer<br><b>Artist</b>: Painter<br>
<div class="title"><a href="https://reader.fool.test/slide/read/s/en/1/2/" title="Chapter 2: Two">Chapter 2</a></div><a title="Team">
<div class="title"><a href="https://reader.fool.test/slide/read/s/es/1/1/" title="Chapter 1">Chapter 1</a></div><a title="Team">"#;
        let (root, requests) = serve(vec![html(page)]);
        let http = http("foolslide", root);
        let url = Url::parse("https://reader.fool.test/slide/series/s").unwrap();
        let site = crate::sites::find(&url).unwrap();
        let children = queued((site.extract)(&http, url)).await;
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].category(), "foolslide-configured");
        assert_eq!(children[0].subcategory(), "manga");
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("POST /slide/series/s "));
        let chapters = chapters(page);
        assert_eq!(chapters[0].1["manga"], "Series & Co");
        assert_eq!(chapters[0].1["author"], "Writer");
        assert_eq!(chapters[0].1["artist"], "Painter");
        assert_eq!(chapters[0].1["group"], "Team");
        assert_eq!(chapters[0].1["title"], "Two");
        assert_eq!(chapters[1].1["language"], "Spanish");
        assert_eq!(chapters[1].1["chapter"], 1);
        assert_eq!(chapters[1].1["volume"], 1);
        assert_eq!(chapters[1].1["chapter_minor"], "");
    }

    #[test]
    fn foolslide_is_only_a_family() {
        assert!(crate::sites::site("foolslide").is_none());
        assert!(crate::sites::instances::family("foolslide").is_some());
    }
}
