//! Dynasty Reader chapters, series, anthologies and images, mirroring
//! gallery_dl/extractor/dynastyscans.py.
use super::{Gallery, keywords, set, text};
use crate::sites::art::{Scan, extr, extract_iter, raw_segments, split_html, strptime};
use crate::sites::{Events, Http, Item, Route, Site, remove_html, unescape};
use crate::{Result, error};
use futures_util::StreamExt;
use serde_json::{Map, Value};
use url::Url;

pub static SITE: Site = Site {
    category: "dynastyscans",
    name: "Dynasty Reader",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CHAPTER, &MANGA, &SEARCH, &IMAGE, &ANTHOLOGY],
    api_key: false,
    cookies: None,
};

const ROOT: &str = "https://dynasty-scans.com";
const REFERER: &str = "https://dynasty-scans.com/";
const IMAGES: &[&str] = &["{category}", "Images"];

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
static SEARCH: Route = Route::new("search", "{image_id}.{extension}", IMAGES, "i_{image_id}");
static IMAGE: Route = Route::new("image", "{image_id}.{extension}", IMAGES, "i_{image_id}");
static ANTHOLOGY: Route = Route::new(
    "anthology",
    "{image_id}.{extension}",
    IMAGES,
    "i_{image_id}",
);

#[derive(Debug, PartialEq)]
enum Target {
    /// `/chapters/NAME`, raw.
    Chapter(String),
    /// `/series/NAME`, raw.
    Manga(String),
    /// The raw query of `/images`.
    Search(String),
    Image(String),
    Anthology(String),
}

fn target(url: &Url) -> Option<Target> {
    if !crate::sites::art::on_host(url, &["dynasty-scans.com"]) {
        return None;
    }
    let raw = raw_segments(url);
    let path = url.path();
    match raw.as_slice() {
        ["chapters", name, ..] if path.starts_with(&format!("/chapters/{name}")) => {
            Some(Target::Chapter(format!("/chapters/{name}")))
        }
        ["series", name, ..] if path.starts_with(&format!("/series/{name}")) => {
            Some(Target::Manga(format!("/series/{name}")))
        }
        ["anthologies", name, ..] if path.starts_with(&format!("/anthologies/{name}")) => {
            Some(Target::Anthology((*name).to_owned()))
        }
        ["images", id, ..] if path.starts_with(&format!("/images/{id}")) => {
            let end = id.find(|c: char| !c.is_ascii_digit()).unwrap_or(id.len());
            (1..=20)
                .contains(&end)
                .then(|| Target::Image(id[..end].to_owned()))
        }
        ["images"] if url.fragment().is_none() => {
            Some(Target::Search(url.query().unwrap_or_default().to_owned()))
        }
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Chapter(path)) => crate::sites::once(async move {
            let page = page(http, &format!("{ROOT}{path}")).await?;
            Ok(crate::sites::events(chapter_events(http, &page, &path)?))
        }),
        Some(Target::Manga(path)) => crate::sites::once(async move {
            let page = page(http, &format!("{ROOT}{path}")).await?;
            // `DynastyscansMangaExtractor.reverse = False`.
            let mut children: Vec<_> = extract_iter(&page, "<dd>\n<a href=\"", "\"")
                .into_iter()
                .filter_map(|path| Url::parse(&format!("{ROOT}{path}")).ok())
                .map(|url| (url, Map::new()))
                .collect();
            if http.option_bool("chapter-reverse", false) {
                children.reverse();
            }
            Ok(super::queue_urls(http, &SITE, &MANGA, children))
        }),
        Some(Target::Search(query)) => search_events(http, query),
        Some(Target::Image(id)) => image_events(http, &IMAGE, id),
        Some(Target::Anthology(name)) => {
            crate::sites::once(async move { anthology_events(http, &name).await })
        }
        None => crate::sites::fail(error("unsupported Dynasty Reader URL")),
    }
}

async fn page(http: &Http, url: &str) -> Result<String> {
    http.text_with(
        &http.api(url)?,
        &http.browser_headers(&[("Referer", REFERER)]),
    )
    .await
}

/// The chapter heading regex `(?:<a[^>]*>)?([^<]+)(?:</a>)?`
/// `(?: ch(\d+)([^:<]*))?(?:: (.+))?`, as `(manga, chapter, minor, title)`.
fn chapter_title(heading: &str) -> Option<(&str, Option<&str>, &str, &str)> {
    let mut rest = heading;
    if rest.starts_with("<a")
        && let Some(end) = rest.find('>')
    {
        rest = &rest[end + 1..];
    }
    let end = rest.find('<').unwrap_or(rest.len());
    if end == 0 {
        return None;
    }
    let manga = &rest[..end];
    rest = rest[end..].strip_prefix("</a>").unwrap_or(&rest[end..]);
    let (mut chapter, mut minor) = (None, "");
    if let Some(after) = rest.strip_prefix(" ch") {
        let digits = after
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(after.len());
        if digits > 0 {
            chapter = Some(&after[..digits]);
            let tail = &after[digits..];
            let end = tail.find([':', '<']).unwrap_or(tail.len());
            minor = &tail[..end];
            rest = &tail[end..];
        }
    }
    let title = rest
        .strip_prefix(": ")
        .map(|title| title.split('\n').next().unwrap_or_default())
        .unwrap_or_default();
    Some((manga, chapter, minor, title))
}

/// `DynastyscansChapterExtractor.metadata` and `images`.
fn chapter_events(
    http: &Http,
    page: &str,
    path: &str,
) -> Result<Vec<crate::event::ExtractionEvent>> {
    let mut e = Scan::new(page);
    let (manga, chapter, minor, title) =
        chapter_title(e.extr("<h3 id='chapter-title'><b>", "</b>"))
            .ok_or_else(|| error("Dynasty Reader chapter has no title"))?;
    let author = e.extr(" by ", "</a>");
    let group = e.extr("\"icon-print\"></i> ", "</span>");
    let group = match remove_html(group) {
        name if name.is_empty() => extr(group, " alt=\"", "\"").to_owned(),
        name => name,
    };
    let released = strptime(e.extr("\"icon-calendar\"></i> ", "<"), "%b %d, %Y");
    let tags = split_html(e.extr("class='tags'>", "<div id='chapter-actions'"));
    let data = keywords([
        ("manga", unescape(manga).into()),
        (
            "chapter",
            text::parse_int(chapter.unwrap_or_default()).into(),
        ),
        ("chapter_minor", minor.into()),
        ("title", unescape(title).into()),
        ("author", remove_html(author).into()),
        ("group", group.into()),
        ("date", text::date(released)),
        ("tags", tags.into()),
        ("lang", "en".into()),
        ("language", "English".into()),
    ]);
    let pages: Value = serde_json::from_str(extr(page, "var pages = ", ";\n"))
        .map_err(|_| error("Dynasty Reader chapter has no page list"))?;
    let pages = pages
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|page| page.get("image")?.as_str())
        .map(|image| (format!("{ROOT}{image}"), None))
        .collect();
    Ok(Gallery::chapter(&CHAPTER, data, pages)
        .id(path.rsplit('/').next().unwrap_or_default())
        .timestamp(released)
        .header("Referer", REFERER)
        .events(&SITE, http))
}

/// `_parse_image_page`: one image and its keywords.
fn image_events<'a>(http: &'a Http, route: &'static Route, id: String) -> Events<'a> {
    crate::sites::once(async move {
        let page = page(http, &format!("{ROOT}/images/{id}")).await?;
        let mut e = Scan::new(&page);
        let date = e.extr("class='create_at'>", "</span>");
        let tags = e.extr("class='tags'>", "</span>");
        let buttons = e.extr("class='btn-group'>", "</div>");
        let src = e.extr(" src=\"", "\"");
        let source = if buttons.contains("Source<") {
            extr(buttons, "href=\"", "\"")
        } else {
            ""
        };
        let raw = format!("{ROOT}{src}");
        let mut data = keywords([
            ("url", raw.clone().into()),
            ("image_id", text::parse_int(&id).into()),
            ("tags", split_html(tags).into()),
            ("date", remove_html(date).into()),
            ("source", unescape(source).into()),
        ]);
        crate::sites::art::nameext_into(&raw, &mut data);
        let url = crate::sites::art::file_url(&raw, ROOT)
            .ok_or_else(|| error("Dynasty Reader image has no file"))?;
        let item = Item::new(url, data)
            .ids(id.clone(), id)
            .header("Referer", REFERER);
        Ok(crate::sites::events(crate::sites::post(
            &SITE,
            http,
            route,
            vec![item],
        )))
    })
}

/// `DynastyscansSearchExtractor.images`: result pages while one links
/// `rel="next"`, each image page fetched as the consumer reaches it.
fn search_events(http: &Http, query: String) -> Events<'_> {
    let query = query.replace("[]", "%5B%5D");
    crate::sites::paginate(Some(1u32), move |state| {
        let query = query.clone();
        async move {
            let Some(page_no) = state else {
                return Ok(None);
            };
            let sep = if query.is_empty() { "" } else { "&" };
            let page = page(http, &format!("{ROOT}/images?{query}{sep}page={page_no}")).await?;
            let ids: Vec<String> = extract_iter(&page, "\"/images/", "\"")
                .into_iter()
                .filter(|id| crate::sites::art::segment(id))
                .map(str::to_owned)
                .collect();
            let events = futures_util::stream::iter(ids)
                .map(move |id| image_events(http, &SEARCH, id))
                .flatten()
                .boxed();
            let more = page.contains("rel=\"next\"") && page_no < 10_000;
            Ok(Some((events, more.then_some(page_no + 1))))
        }
    })
}

/// `DynastyscansAnthologyExtractor.items`: the anthology's Atom feed, each
/// entry queued as a chapter with the feed's keywords.
async fn anthology_events<'a>(http: &'a Http, name: &str) -> Result<Events<'a>> {
    let feed = page(http, &format!("{ROOT}/anthologies/{name}.atom")).await?;
    let mut data = Map::new();
    if http.option_bool("metadata", false) {
        let page = page(http, &format!("{ROOT}/anthologies/{name}")).await?;
        let alert = extr(&page, "<div class='alert", "</div>");
        let alert: Vec<String> = split_html(alert).into_iter().skip(1).collect();
        set(&mut data, "alert", alert);
        set(
            &mut data,
            "status",
            extr(&page, "<small>&mdash; ", "</small>"),
        );
        set(
            &mut data,
            "description",
            extr(&page, "<div class='description'>", "</div>"),
        );
    }
    let children = anthology_children(&feed, data)?;
    Ok(super::queue_urls(http, &SITE, &ANTHOLOGY, children))
}

/// The feed's entries by element position, as upstream reads them.
fn anthology_children(
    feed: &str,
    mut data: Map<String, Value>,
) -> Result<Vec<(Url, Map<String, Value>)>> {
    let document = roxmltree::Document::parse(feed)
        .map_err(|_| error("Dynasty Reader returned an invalid anthology feed"))?;
    let invalid = || error("Dynasty Reader returned an unexpected anthology feed");
    fn child<'d, 'i>(
        node: roxmltree::Node<'d, 'i>,
        index: usize,
    ) -> Option<roxmltree::Node<'d, 'i>> {
        node.children()
            .filter(roxmltree::Node::is_element)
            .nth(index)
    }
    let feed_title = child(document.root_element(), 3).ok_or_else(invalid)?;
    let anthology: String = feed_title
        .text()
        .unwrap_or_default()
        .chars()
        .skip(28)
        .collect();
    set(&mut data, "anthology", anthology);
    let text_of = |node: Option<roxmltree::Node<'_, '_>>| -> Result<String> {
        Ok(node
            .ok_or_else(invalid)?
            .text()
            .unwrap_or_default()
            .to_owned())
    };
    let from = |text: String, skip: usize| text.chars().skip(skip).collect::<String>();
    let mut children = Vec::new();
    for entry in document
        .root_element()
        .children()
        .filter(|node| node.is_element() && node.tag_name().name() == "entry")
    {
        let content = child(entry, 6).and_then(|content| child(content, 0));
        let line = |index| text_of(content.and_then(|content| child(content, index)));
        set(&mut data, "author", from(line(0)?, 8));
        set(&mut data, "scanlator", from(line(1)?, 11));
        let tags: Vec<String> = from(line(2)?, 6)
            .to_lowercase()
            .split(", ")
            .map(str::to_owned)
            .collect();
        set(&mut data, "tags", tags);
        set(&mut data, "title", text_of(child(entry, 5))?);
        let date = |index| text_of(child(entry, index)).map(|date| text::iso(&date));
        set(&mut data, "date", text::date(date(1)?));
        set(&mut data, "date_updated", text::date(date(2)?));
        if let Ok(url) = Url::parse(text_of(child(entry, 4))?.trim()) {
            children.push((url, data.clone()));
        }
    }
    Ok(children)
}

#[cfg(test)]
mod tests {
    use super::super::tests::{archive, path};
    use super::*;
    use crate::sites::testing::{html, http, keywords as kw, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("http://dynasty-scans.com/chapters/hitoribocchi_no_oo_seikatsu_ch33"),
            Some(Target::Chapter(
                "/chapters/hitoribocchi_no_oo_seikatsu_ch33".into()
            ))
        );
        assert_eq!(
            t("https://www.dynasty-scans.com/series/aria"),
            Some(Target::Manga("/series/aria".into()))
        );
        assert_eq!(
            t("https://dynasty-scans.com/images?with[]=4930&with[]=5211"),
            Some(Target::Search("with[]=4930&with[]=5211".into()))
        );
        assert_eq!(
            t("https://dynasty-scans.com/images/"),
            Some(Target::Search(String::new()))
        );
        assert_eq!(
            t("https://dynasty-scans.com/images/1245"),
            Some(Target::Image("1245".into()))
        );
        assert_eq!(
            t("https://dynasty-scans.com/anthologies/%C3%A9clair"),
            Some(Target::Anthology("%C3%A9clair".into()))
        );
        for raw in [
            "https://dynasty-scans.com/chapters/",
            "https://dynasty-scans.com/images/x",
            "https://dynasty-scans.com/images#top",
            "https://dynasty-scans.com/authors/x",
            "https://dynasty-scans.com.evil.test/series/a",
            "https://evildynasty-scans.com/series/a",
            "https://cdn.dynasty-scans.com/series/a",
            "https://dynasty-scans.com:8443/series/a",
            "https://u:p@dynasty-scans.com/series/a",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[test]
    fn parses_chapter_headings() {
        assert_eq!(
            chapter_title(r#"<a href="/series/x">Hitoribocchi</a> ch33.5: A &amp; B"#),
            Some(("Hitoribocchi", Some("33"), ".5", "A &amp; B"))
        );
        assert_eq!(
            chapter_title("Oneshot ch1: x"),
            Some(("Oneshot ch1: x", None, "", ""))
        );
        assert_eq!(
            chapter_title(r#"<a href="/x">M</a> chapter"#),
            Some(("M", None, "", ""))
        );
        assert_eq!(chapter_title("<b>x"), None);
    }

    const CHAPTER_PAGE: &str = r#"<h3 id='chapter-title'><b><a href="/series/hitoribocchi">Hitoribocchi no OO Seikatsu</a> ch33: Friends &amp; Rivals</b> by <a href="/authors/katsuwo">Katsuwo</a> <small></small></h3>
<span class='scanlators'><i class="icon-print"></i> <a href="/scanlators/doki">Doki Fansubs</a></span>
<span class='released'><i class="icon-calendar"></i> May 30, 2017</span>
<span class='tags'><i class="icon-tags"></i> <a href="/tags/a" class="label">4-koma</a> <a href="/tags/b" class="label">Comedy</a></span>
<div id='chapter-actions'>
<script>
    var pages = [{"image":"/system/releases/000/018/122/Page%2001.webp","name":"01"},{"image":"/system/releases/000/018/122/Page%2002.webp","name":"02"}];
</script>"#;

    #[tokio::test]
    async fn extracts_chapter() {
        let (root, requests) = serve(vec![html(CHAPTER_PAGE)]);
        let http = http("Dynasty Reader", root);
        let files = media(extract(
            &http,
            Url::parse("https://dynasty-scans.com/chapters/hitoribocchi_ch33").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[1].url.as_str(),
            "https://dynasty-scans.com/system/releases/000/018/122/Page%2002.webp"
        );
        assert_eq!(files[1].headers["Referer"], "https://dynasty-scans.com/");
        let k = kw(&files[1]);
        assert_eq!(k["manga"], "Hitoribocchi no OO Seikatsu");
        assert_eq!(k["chapter"], 33);
        assert_eq!(k["chapter_minor"], "");
        assert_eq!(k["title"], "Friends & Rivals");
        assert_eq!(k["author"], "Katsuwo");
        assert_eq!(k["group"], "Doki Fansubs");
        assert_eq!(k["date"], "2017-05-30 00:00:00");
        assert_eq!(k["tags"], json!(["4-koma", "Comedy"]));
        assert_eq!(k["language"], "English");
        assert_eq!(k["filename"], "Page 02");
        assert_eq!(k["page"], 2);
        // Windows path sanitization replaces the colon in the chapter title.
        let colon = if cfg!(windows) { "_" } else { ":" };
        assert_eq!(
            path(&files[1]),
            format!(
                "dynastyscans/Hitoribocchi no OO Seikatsu/c033{colon} Friends & Rivals/\
                 Hitoribocchi no OO Seikatsu_c033_002.webp"
            )
        );
        assert_eq!(
            archive(&files[1]),
            r#"["{manga}_{chapter}{chapter_minor}_{page}",["Hitoribocchi no OO Seikatsu","33","","2"]]"#
        );
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /chapters/hitoribocchi_ch33 "));
    }

    #[tokio::test]
    async fn manga_queues_chapters_in_page_order() {
        let series = "<dl><dd>\n<a href=\"/chapters/a_ch1\">1</a></dd>\
                      <dd>\n<a href=\"/chapters/a_ch2\">2</a></dd></dl>";
        let (root, requests) = serve(vec![html(series), html(CHAPTER_PAGE), html(CHAPTER_PAGE)]);
        let http = http("Dynasty Reader", root);
        let files = media(extract(
            &http,
            Url::parse("https://dynasty-scans.com/series/a").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 4);
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /series/a "));
        assert!(requests[1].starts_with("GET /chapters/a_ch1 "));
        assert!(requests[2].starts_with("GET /chapters/a_ch2 "));
    }

    const IMAGE_PAGE: &str = r#"<span class='create_at'><i class="icon-calendar"></i> Apr  8, 2014</span>
<span class='tags'><i class="icon-tags"></i> <a href="/tags/yuri/images" class="label">Yuri</a> <a class="label">Author: Tachi</a></span>
<div class='btn-group'><a href="https://example.org/a?b=1&amp;c=2" class="btn">Source</a></div>
<div class='image'><img alt="Water" src="/system/images_images/000/001/245/full/water.webp" /></div>"#;

    #[tokio::test]
    async fn search_pages_through_results() {
        let (root, requests) = serve(vec![
            html(r#"<a href="/images/1245" class="thumbnail"><a rel="next" href="?page=2">"#),
            html(IMAGE_PAGE),
            html(r#"<a href="/images/7" class="thumbnail">"#),
            html(IMAGE_PAGE),
        ]);
        let http = http("Dynasty Reader", root);
        let files = media(extract(
            &http,
            Url::parse("https://dynasty-scans.com/images?with[]=4930").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = kw(&files[0]);
        assert_eq!(k["image_id"], 1245);
        assert_eq!(k["date"], "Apr 8, 2014");
        assert_eq!(k["tags"], json!(["Yuri", "Author: Tachi"]));
        assert_eq!(k["source"], "https://example.org/a?b=1&c=2");
        assert_eq!(
            k["url"],
            "https://dynasty-scans.com/system/images_images/000/001/245/full/water.webp"
        );
        assert_eq!(k["subcategory"], "search");
        assert_eq!(path(&files[0]), "dynastyscans/Images/1245.webp");
        assert_eq!(archive(&files[1]), r#"["i_{image_id}",["7"]]"#);
        let requests = requests.join().unwrap();
        assert!(
            requests[0].starts_with("GET /images?with%5B%5D=4930&page=1 "),
            "{}",
            requests[0]
        );
        assert!(requests[1].starts_with("GET /images/1245 "));
        assert!(requests[2].starts_with("GET /images?with%5B%5D=4930&page=2 "));
        assert!(requests[3].starts_with("GET /images/7 "));
    }

    #[tokio::test]
    async fn extracts_single_image() {
        let (root, requests) = serve(vec![html(IMAGE_PAGE)]);
        let http = http("Dynasty Reader", root);
        let files = media(extract(
            &http,
            Url::parse("https://dynasty-scans.com/images/1245").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(kw(&files[0])["subcategory"], "image");
        assert_eq!(kw(&files[0])["filename"], "water");
        requests.join().unwrap();
    }

    #[tokio::test]
    async fn anthology_queues_feed_entries() {
        let feed = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xml:lang="en-US" xmlns="http://www.w3.org/2005/Atom">
  <id>tag:dynasty-scans.com,2005:/anthologies/1st_bite</id>
  <link rel="alternate" type="text/html" href="https://dynasty-scans.com"/>
  <link rel="self" type="application/atom+xml" href="https://dynasty-scans.com/anthologies/1st_bite.atom"/>
  <title>Dynasty Reader - Anthology: 1st Bite</title>
  <updated>2026-10-24T18:56:59Z</updated>
  <entry>
    <id>tag:dynasty-scans.com,2005:Chapter/15538</id>
    <published>2016-04-06T19:42:17-04:00</published>
    <updated>2023-09-25T13:42:37-04:00</updated>
    <link rel="alternate" type="text/html" href="https://dynasty-scans.com/chapters/path_2"/>
    <url>https://dynasty-scans.com/chapters/path_2</url>
    <title>Path #2 - First Love</title>
    <content type="xhtml">
      <div xmlns="http://www.w3.org/1999/xhtml">
        <p>Author: Rebecca</p>
        <p>Scanlator: Army</p>
        <p>Tags: Akagi x Kaga, Yuri</p>
      </div>
    </content>
  </entry>
</feed>"#;
        let page = "<div class='alert alert-info'><b>Licensed</b> Removed.</div>\
                    <small>&mdash; Licensed</small><div class='description'><p>D</p></div>";
        let (root, requests) = serve(vec![
            crate::sites::testing::response("200 OK", "application/atom+xml", feed.as_bytes()),
            html(page),
            html(CHAPTER_PAGE),
        ]);
        let http = http("Dynasty Reader", root).with_options(Some(std::sync::Arc::new(
            json!({"metadata": true}).as_object().unwrap().clone(),
        )));
        let files = media(extract(
            &http,
            Url::parse("https://dynasty-scans.com/anthologies/1st_bite").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(kw(&files[0])["subcategory"], "chapter");
        let children = anthology_children(feed, Map::new()).unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(
            children[0].0.as_str(),
            "https://dynasty-scans.com/chapters/path_2"
        );
        assert_eq!(
            Value::Object(children[0].1.clone()),
            json!({
                "anthology": "1st Bite",
                "author": "Rebecca",
                "scanlator": "Army",
                "tags": ["akagi x kaga", "yuri"],
                "title": "Path #2 - First Love",
                "date": "2016-04-06 23:42:17",
                "date_updated": "2023-09-25 17:42:37",
            })
        );
        assert!(anthology_children("<feed/>", Map::new()).is_err());
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /anthologies/1st_bite.atom "));
        assert!(requests[1].starts_with("GET /anthologies/1st_bite "));
        assert!(requests[2].starts_with("GET /chapters/path_2 "));
    }
}
