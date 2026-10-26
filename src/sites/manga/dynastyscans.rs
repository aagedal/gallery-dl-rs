//! Dynasty Reader chapters, series, image searches, images and
//! anthologies, mirroring gallery_dl/extractor/dynastyscans.py.
use super::{Gallery, english, set};
use crate::error;
use crate::sites::gdl::{
    Scan, date, extr, extract_iter, nameext_into, parse_int, parse_iso, remove_html, split_html,
    strptime, unescape,
};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://dynasty-scans.com";

static CHAPTER: Route = super::chapter_route();
static MANGA: Route = super::manga_route();
static SEARCH: Route = Route::new(
    "search",
    "{image_id}.{extension}",
    &["{category}", "Images"],
    "i_{image_id}",
);
static IMAGE: Route = Route::new(
    "image",
    "{image_id}.{extension}",
    &["{category}", "Images"],
    "i_{image_id}",
);
static ANTHOLOGY: Route = Route::new(
    "anthology",
    "{image_id}.{extension}",
    &["{category}", "Images"],
    "i_{image_id}",
);

pub static SITE: Site = Site {
    category: "dynastyscans",
    name: "Dynasty Reader",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CHAPTER, &MANGA, &SEARCH, &IMAGE, &ANTHOLOGY],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Chapter(String),
    Manga(String),
    Search(String),
    Image(String),
    Anthology(String),
}

fn target(url: &Url) -> Option<Target> {
    if !crate::sites::gdl::on(url, &["dynasty-scans.com"], true) {
        return None;
    }
    let parts = crate::sites::gdl::path_parts(url);
    match parts.as_slice() {
        ["chapters", name, ..] if !name.is_empty() => Some(Target::Chapter((*name).into())),
        ["series", name, ..] if !name.is_empty() => Some(Target::Manga((*name).into())),
        ["anthologies", name, ..] if !name.is_empty() => Some(Target::Anthology((*name).into())),
        ["images", id, ..] if crate::sites::gdl::digits(id) => Some(Target::Image((*id).into())),
        ["images"] | ["images", ""] => Some(Target::Search(url.query().unwrap_or_default().into())),
        ["images", id, ..] => {
            let len = id.bytes().take_while(u8::is_ascii_digit).count();
            (len > 0).then(|| Target::Image(id[..len].into()))
        }
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Dynasty Reader URL"));
    };
    match target {
        Target::Chapter(name) => crate::sites::once(async move {
            let page = super::fetch(http, &format!("{ROOT}/chapters/{name}")).await?;
            let pages = images(&page)?;
            Ok(crate::sites::events(
                Gallery::chapter(&CHAPTER, chapter_metadata(&page), pages).events(&SITE, http),
            ))
        }),
        Target::Manga(name) => crate::sites::once(async move {
            let page = super::fetch(http, &format!("{ROOT}/series/{name}")).await?;
            let chapters = extract_iter(&page, "<dd>\n<a href=\"", "\"")
                .into_iter()
                .filter_map(|path| Url::parse(&format!("{ROOT}{path}")).ok())
                .map(|url| (url, Map::new()))
                .collect();
            Ok(super::queue_chapters(http, &SITE, &MANGA, chapters, false))
        }),
        Target::Image(id) => crate::sites::once(async move {
            let item = image(http, &id).await?;
            Ok(crate::sites::events(crate::sites::post(
                &SITE,
                http,
                &IMAGE,
                vec![item],
            )))
        }),
        Target::Search(query) => search(http, query),
        Target::Anthology(name) => crate::sites::once(anthology(http, name)),
    }
}

/// `(?:<a[^>]*>)?([^<]+)(?:</a>)?(?: ch(\d+)([^:<]*))?(?:: (.+))?`
fn chapter_title(text: &str) -> (String, String, String, String) {
    let mut rest = text;
    let linked = rest.starts_with("<a") && rest.contains('>');
    if linked {
        rest = &rest[rest.find('>').map_or(0, |i| i + 1)..];
    }
    let name_len = rest.find('<').unwrap_or(rest.len());
    let manga = rest[..name_len].to_owned();
    rest = &rest[name_len..];
    let (mut chapter, mut minor, mut title) = (String::new(), String::new(), String::new());
    if manga.is_empty() {
        return (manga, chapter, minor, title);
    }
    if let Some(tail) = rest.strip_prefix("</a>") {
        rest = tail;
        if let Some(tail) = rest.strip_prefix(" ch") {
            let digits = tail.bytes().take_while(u8::is_ascii_digit).count();
            if digits > 0 {
                chapter = tail[..digits].to_owned();
                let after = &tail[digits..];
                let len = after.find([':', '<']).unwrap_or(after.len());
                minor = after[..len].to_owned();
                rest = &after[len..];
            }
        }
        if let Some(tail) = rest.strip_prefix(": ").filter(|t| !t.is_empty()) {
            title = tail.lines().next().unwrap_or_default().to_owned();
        }
    }
    (manga, chapter, minor, title)
}

fn chapter_metadata(page: &str) -> Map<String, Value> {
    let mut scan = Scan::new(page);
    let (manga, chapter, minor, title) =
        chapter_title(scan.extr("<h3 id='chapter-title'><b>", "</b>"));
    let author = scan.extr(" by ", "</a>");
    let group = scan.extr("\"icon-print\"></i> ", "</span>");
    let group = match remove_html(group) {
        text if text.is_empty() => extr(group, " alt=\"", "\"").to_owned(),
        text => text,
    };
    let mut data = Map::new();
    set(&mut data, "manga", unescape(&manga));
    set(&mut data, "chapter", parse_int(&chapter));
    set(&mut data, "chapter_minor", minor);
    set(&mut data, "title", unescape(&title));
    set(&mut data, "author", remove_html(author));
    set(&mut data, "group", group);
    set(
        &mut data,
        "date",
        date(strptime(
            scan.extr("\"icon-calendar\"></i> ", "<"),
            "%b %d, %Y",
        )),
    );
    set(
        &mut data,
        "tags",
        split_html(scan.extr("class='tags'>", "<div id='chapter-actions'")),
    );
    english(&mut data);
    data
}

fn images(page: &str) -> crate::Result<Vec<super::Page>> {
    let pages: Value = serde_json::from_str(extr(page, "var pages = ", ";\n"))
        .map_err(|_| error("Dynasty Reader chapter has no page list"))?;
    Ok(pages
        .as_array()
        .into_iter()
        .flatten()
        .map(|img| {
            (
                format!("{ROOT}{}", crate::sites::gdl::str_of(img, "image")),
                None,
            )
        })
        .collect())
}

/// `_parse_image_page` as one file.
async fn image(http: &Http, id: &str) -> crate::Result<Item> {
    let page = super::fetch(http, &format!("{ROOT}/images/{id}")).await?;
    let mut scan = Scan::new(&page);
    let created = scan.extr("class='create_at'>", "</span>");
    let tags = scan.extr("class='tags'>", "</span>");
    let src = scan.extr("class='btn-group'>", "</div>");
    let path = scan.extr(" src=\"", "\"");
    let source = if src.contains("Source<") {
        extr(src, "href=\"", "\"")
    } else {
        ""
    };
    let url = format!("{ROOT}{path}");
    let mut data = Map::new();
    set(&mut data, "url", url.as_str());
    set(&mut data, "image_id", parse_int(id));
    set(&mut data, "tags", split_html(tags));
    set(&mut data, "date", remove_html(created));
    set(&mut data, "source", unescape(source));
    nameext_into(&url, &mut data);
    let url = Url::parse(&url).map_err(|_| error("invalid Dynasty Reader image URL"))?;
    Ok(Item::new(url, data).ids(id, id))
}

fn search(http: &Http, query: String) -> Events<'_> {
    let base = format!("{ROOT}/images?{}", query.replace("[]", "%5B%5D"));
    crate::sites::paginate(Some(1u64), move |state| {
        let base = base.clone();
        async move {
            let Some(number) = state else {
                return Ok(None);
            };
            let mut url = http.api(&base)?;
            url.query_pairs_mut()
                .append_pair("page", &number.to_string());
            let page = http.text(&url).await?;
            let mut items = Vec::new();
            for id in extract_iter(&page, "\"/images/", "\"") {
                items.push(image(http, id).await?);
            }
            let events = items
                .into_iter()
                .flat_map(|item| crate::sites::post(&SITE, http, &SEARCH, vec![item]))
                .collect();
            let next = page.contains("rel=\"next\"").then_some(number + 1);
            Ok(Some((crate::sites::events(events), next)))
        }
    })
}

fn elements<'a, 'i>(node: roxmltree::Node<'a, 'i>) -> Vec<roxmltree::Node<'a, 'i>> {
    node.children()
        .filter(roxmltree::Node::is_element)
        .collect()
}

async fn anthology(http: &Http, name: String) -> crate::Result<Events<'_>> {
    let url = format!("{ROOT}/anthologies/{name}.atom");
    let xml = super::fetch(http, &url).await?;
    let doc = roxmltree::Document::parse(&xml)
        .map_err(|_| error("invalid Dynasty Reader anthology feed"))?;
    let root = doc.root_element();
    let top = elements(root);
    let text_of = |node: Option<&roxmltree::Node<'_, '_>>| -> String {
        node.and_then(|n| n.text()).unwrap_or_default().to_owned()
    };
    let skip = |text: String, n: usize| text.chars().skip(n).collect::<String>();
    let mut data = Map::new();
    set(&mut data, "anthology", skip(text_of(top.get(3)), 28));
    if http.option_bool("metadata", false) {
        let page = super::fetch(http, &url[..url.len() - 5]).await?;
        let alert = extr(&page, "<div class='alert", "</div>");
        let alert: Vec<String> = if alert.is_empty() {
            Vec::new()
        } else {
            split_html(alert).into_iter().skip(1).collect()
        };
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
    let mut chapters = Vec::new();
    for entry in top.iter().filter(|n| n.tag_name().name() == "entry") {
        let fields = elements(*entry);
        let content = fields
            .get(6)
            .and_then(|c| elements(*c).into_iter().next())
            .map(elements)
            .unwrap_or_default();
        set(&mut data, "author", skip(text_of(content.first()), 8));
        set(&mut data, "scanlator", skip(text_of(content.get(1)), 11));
        let tags: Vec<String> = skip(text_of(content.get(2)), 6)
            .to_lowercase()
            .split(", ")
            .map(str::to_owned)
            .collect();
        set(&mut data, "tags", tags);
        set(&mut data, "title", text_of(fields.get(5)));
        set(&mut data, "date", date(parse_iso(&text_of(fields.get(1)))));
        set(
            &mut data,
            "date_updated",
            date(parse_iso(&text_of(fields.get(2)))),
        );
        if let Ok(url) = Url::parse(&text_of(fields.get(4))) {
            chapters.push((url, data.clone()));
        }
    }
    Ok(super::queue_urls(http, &SITE, &ANTHOLOGY, chapters))
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
            t("http://dynasty-scans.com/chapters/hitoribocchi_no_oo_seikatsu_ch33"),
            Some(Target::Chapter("hitoribocchi_no_oo_seikatsu_ch33".into()))
        );
        assert_eq!(
            t("https://dynasty-scans.com/series/hitoribocchi"),
            Some(Target::Manga("hitoribocchi".into()))
        );
        assert_eq!(
            t("https://dynasty-scans.com/images?with[]=4930&with[]=5211"),
            Some(Target::Search("with[]=4930&with[]=5211".into()))
        );
        assert_eq!(
            t("https://dynasty-scans.com/images"),
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
        assert_eq!(t("https://dynasty-scans.com/tags/x"), None);
    }

    #[test]
    fn parses_chapter_titles() {
        assert_eq!(
            chapter_title("<a href=\"/series/x\">Hitoribocchi</a> ch33.5: Title: Two"),
            (
                "Hitoribocchi".into(),
                "33".into(),
                ".5".into(),
                "Title: Two".into()
            )
        );
        assert_eq!(
            chapter_title("New Game! The Spinoff Special 13"),
            (
                "New Game! The Spinoff Special 13".into(),
                String::new(),
                String::new(),
                String::new()
            )
        );
    }

    #[tokio::test]
    async fn extracts_chapter() {
        let page = "<h3 id='chapter-title'><b><a href=\"/series/h\">Hitoribocchi</a> ch33</b> \
            by <a href=\"/authors/k\">Katsuwo</a> <span>\"icon-print\"></i> <a>Wild</a></span>\
            \"icon-calendar\"></i> Aug 12, 2018<br>\
            <span class='tags'><a>Comedy</a><a>School</a></span><div id='chapter-actions'>\
            var pages = [{\"image\":\"/system/a/001.png\"},{\"image\":\"/system/a/002.png\"}];\n";
        let (root, server) = serve(vec![html(page)]);
        let http = http("dynastyscans", root);
        let files = media(extract(
            &http,
            Url::parse("https://dynasty-scans.com/chapters/h_ch33").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[1]);
        assert_eq!(k["manga"], "Hitoribocchi");
        assert_eq!(k["chapter"], 33);
        assert_eq!(k["author"], "Katsuwo");
        assert_eq!(k["group"], "Wild");
        assert_eq!(k["date"], "2018-08-12 00:00:00");
        assert_eq!(k["tags"], serde_json::json!(["Comedy", "School"]));
        assert_eq!(
            files[0].url.as_str(),
            "https://dynasty-scans.com/system/a/001.png"
        );
        assert_eq!(
            path(&files[1]),
            "dynastyscans/Hitoribocchi/c033/Hitoribocchi_c033_002.png"
        );
        server.join().unwrap();
    }

    const IMAGE_PAGE: &str = "<span class='create_at'>Jan 1, 2020</span>\
        <span class='tags'><a>Yuri</a></span><div class='btn-group'><a href=\"https://x.test/s\">Source</a></div>\
        <img src=\"/system/images_images/1245/original.jpg?1\">";

    #[tokio::test]
    async fn searches_images_with_pages() {
        let (root, server) = serve(vec![
            html("<a href=\"/images/1245\"></a><a rel=\"next\">"),
            html(IMAGE_PAGE),
            html("<a href=\"/images/77\">"),
            html(IMAGE_PAGE),
        ]);
        let http = http("dynastyscans", root);
        let files = media(extract(
            &http,
            Url::parse("https://dynasty-scans.com/images?with[]=1").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[0]);
        assert_eq!(k["image_id"], 1245);
        assert_eq!(k["source"], "https://x.test/s");
        assert_eq!(k["tags"], serde_json::json!(["Yuri"]));
        assert_eq!(k["date"], "Jan 1, 2020");
        assert_eq!(path(&files[0]), "dynastyscans/Images/1245.jpg");
        assert_eq!(archive(&files[1]), r#"["i_{image_id}",["77"]]"#);
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("GET /images?with%5B%5D=1&page=1 "),
            "{}",
            requests[0]
        );
        assert!(requests[2].starts_with("GET /images?with%5B%5D=1&page=2 "));
    }

    #[tokio::test]
    async fn anthology_queues_entries() {
        let feed = r#"<?xml version="1.0"?><feed xmlns="http://www.w3.org/2005/Atom">
            <id>x</id><link/><link/><title>Dynasty Reader &#187; Anthology: Éclair</title><updated/>
            <entry><id>1</id><published>2020-01-02T03:04:05Z</published><updated>2020-01-03T00:00:00Z</updated>
            <link/><url>https://dynasty-scans.com/chapters/eclair_ch01</url><title>Chapter One</title>
            <content><div><p>Author: A</p><p>Scanlator: S</p><p>Tags: Yuri, Drama</p></div></content></entry>
            </feed>"#;
        let (root, server) = serve(vec![html(feed)]);
        let http = http("dynastyscans", root);
        let queued = children(extract(
            &http,
            Url::parse("https://dynasty-scans.com/anthologies/eclair").unwrap(),
        ))
        .await;
        assert_eq!(queued.len(), 1);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /anthologies/eclair.atom "));
    }
}
