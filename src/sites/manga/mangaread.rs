//! MangaRead chapters and manga, mirroring
//! gallery_dl/extractor/mangaread.py: a Madara (WordPress) reader whose
//! chapter headings carry the manga, chapter number and title.
use super::{Gallery, keywords, set, text};
use crate::sites::art::{Scan, extr, extract_iter, segment, split_html};
use crate::sites::{Events, Http, Route, Site, remove_html, unescape};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

pub static SITE: Site = Site {
    category: "mangaread",
    name: "MangaRead",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CHAPTER, &MANGA],
    api_key: false,
    cookies: None,
};

const ROOT: &str = "https://www.mangaread.org";

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
    /// `/manga/MANGA/CHAPTER`, raw.
    Chapter(String),
    /// `/manga/MANGA`, raw.
    Manga(String),
}

/// `mangaread\.org(/manga/[^/?#]+/[^/?#]+)` (unanchored) and
/// `mangaread\.org(/manga/[^/?#]+)/?$`.
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::art::on_host(url, &["mangaread.org"]) {
        return None;
    }
    let end = url.query().is_none() && url.fragment().is_none();
    let raw: Vec<&str> = url.path().split('/').skip(1).collect();
    match raw.as_slice() {
        ["manga", manga, chapter, ..] if segment(manga) && segment(chapter) => {
            Some(Target::Chapter(format!("/manga/{manga}/{chapter}")))
        }
        ["manga", manga] | ["manga", manga, ""] if segment(manga) && end => {
            Some(Target::Manga(format!("/manga/{manga}")))
        }
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Chapter(path)) => crate::sites::once(async move {
            let page = page(http, &format!("{ROOT}{path}")).await?;
            Ok(crate::sites::events(
                Gallery::chapter(&CHAPTER, metadata(&page)?, images(&page))
                    .id(path)
                    .events(&SITE, http),
            ))
        }),
        Some(Target::Manga(path)) => crate::sites::once(async move {
            let page = page(http, &format!("{ROOT}{path}")).await?;
            let mut children = chapters(&page)?;
            // `MangaExtractor.reverse = True`.
            if !http.option_bool("chapter-reverse", false) {
                children.reverse();
            }
            Ok(super::queue_urls(http, &SITE, &MANGA, children))
        }),
        None => crate::sites::fail(error("unsupported MangaRead URL")),
    }
}

async fn page(http: &Http, url: &str) -> Result<String> {
    http.text_with(&http.api(url)?, &http.browser_headers(&[]))
        .await
}

/// `MangareadBase.parse_chapter_string`: the match of
/// `(?:(.+)\s*-\s*)?[Cc]hapter\s*(\d+)(\.\d+)?(?:\s*-\s*(.+))?` at the
/// start of the unescaped, stripped heading. A `manga` already in `data`
/// is kept.
fn parse_chapter_string(info: &str, data: &mut Map<String, Value>) -> Result<()> {
    let info = unescape(info);
    let info = info.trim();
    // The greedy `(.+)\s*-\s*` takes the last dash that a chapter follows.
    let (manga, rest) = info
        .match_indices('-')
        .rev()
        .filter(|(i, _)| *i > 0 && !info[..*i].contains('\n'))
        .find_map(|(i, _)| chapter_part(info[i + 1..].trim_start()).map(|rest| (&info[..i], rest)))
        .or_else(|| chapter_part(info).map(|rest| ("", rest)))
        .ok_or_else(|| error("MangaRead chapter heading has no chapter number"))?;
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    let (chapter, rest) = rest.split_at(digits);
    let minor_len = rest
        .strip_prefix('.')
        .map(|tail| tail.bytes().take_while(u8::is_ascii_digit).count())
        .filter(|n| *n > 0)
        .map_or(0, |n| n + 1);
    let (minor, rest) = rest.split_at(minor_len);
    let title = rest
        .trim_start()
        .strip_prefix('-')
        .map(|tail| tail.trim_start().split('\n').next().unwrap_or_default())
        .unwrap_or_default();
    if !data.contains_key("manga") {
        set(data, "manga", manga.trim());
    }
    set(data, "chapter", text::parse_int(chapter));
    set(data, "chapter_minor", minor);
    set(data, "title", title);
    set(data, "lang", "en");
    set(data, "language", "English");
    Ok(())
}

/// `[Cc]hapter\s*` followed by a digit: the text from that digit.
fn chapter_part(text: &str) -> Option<&str> {
    let rest = text
        .strip_prefix("Chapter")
        .or_else(|| text.strip_prefix("chapter"))?
        .trim_start();
    rest.starts_with(|c: char| c.is_ascii_digit())
        .then_some(rest)
}

/// `MangareadChapterExtractor.metadata`.
fn metadata(page: &str) -> Result<Map<String, Value>> {
    let tags: Vec<String> = split_html(extr(page, "class=\"wp-manga-tags-list\">", "</div>"))
        .into_iter()
        .step_by(2)
        .collect();
    let mut data = keywords([("tags", tags.into())]);
    let info = extr(page, "<h1 id=\"chapter-heading\">", "</h1>");
    if info.is_empty() {
        return Err(error("MangaRead chapter not found"));
    }
    parse_chapter_string(info, &mut data)?;
    Ok(data)
}

/// `MangareadChapterExtractor.images`.
fn images(page: &str) -> Vec<super::Page> {
    let reader = extr(
        page,
        "<div class=\"reading-content\">",
        "<div class=\"entry-header",
    );
    extract_iter(reader, "<img id=\"image-", ">")
        .into_iter()
        .map(|img| (extr(img, "src=\"", "\"").trim().to_owned(), None))
        .collect()
}

/// `MangareadMangaExtractor.metadata`.
fn manga_metadata(page: &str) -> Map<String, Value> {
    let tags = |html: &str| -> Value {
        extract_iter(html, "\"tag\">", "</a>")
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>()
            .into()
    };
    let description = page
        .find("summary__content")
        .and_then(|pos| text::extract(page, ">", "</div>", pos).0)
        .unwrap_or_default();
    let mut e = Scan::new(extr(
        page,
        "class=\"summary_content\">",
        "class=\"manga-action\"",
    ));
    let rating = text::parse_float(e.extr("total_votes\">", "</span>").trim());
    let alt: Vec<String> = remove_html(e.extr("Alternative\t\t</h5>\n\t</div>", "</div>"))
        .split("; ")
        .map(str::to_owned)
        .collect();
    let author = tags(e.extr("class=\"author-content\">", "</div>"));
    let artist = tags(e.extr("class=\"artist-content\">", "</div>"));
    let genres = tags(e.extr("class=\"genres-content\">", "</div>"));
    let kind = remove_html(e.extr("\tType\t", "\n</div>"));
    let release = text::parse_int(&remove_html(e.extr("\tRelease\t", "\n</div>")));
    let status = remove_html(e.extr("\tStatus\t", "\n</div>"));
    keywords([
        ("manga", extr(page, "<h1>", "</h1>").trim().into()),
        ("description", unescape(&remove_html(description)).into()),
        ("rating", rating),
        ("manga_alt", alt.into()),
        ("author", author),
        ("artist", artist),
        ("genres", genres),
        ("type", kind.into()),
        ("release", release.into()),
        ("status", status.into()),
    ])
}

/// `MangareadMangaExtractor.chapters`, in page order.
fn chapters(page: &str) -> Result<Vec<(Url, Map<String, Value>)>> {
    if page.contains("class=\"error404") {
        return Err(error("MangaRead manga not found"));
    }
    let mut data = manga_metadata(page);
    let mut results = Vec::new();
    for chapter in extract_iter(page, "<li class=\"wp-manga-chapter", "</li>") {
        let (url, pos) = text::extract(chapter, "<a href=\"", "\"", 0);
        let (info, _) = text::extract(chapter, ">", "</a>", pos);
        parse_chapter_string(info.unwrap_or_default(), &mut data)?;
        if let Ok(url) = Url::parse(url.unwrap_or_default()) {
            results.push((url, data.clone()));
        }
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::super::tests::{archive, path};
    use super::*;
    use crate::sites::testing::{html, http, keywords as kw, media, queued, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.mangaread.org/manga/one-piece/chapter-1053-3/"),
            Some(Target::Chapter("/manga/one-piece/chapter-1053-3".into()))
        );
        assert_eq!(
            t("http://mangaread.org/manga/one-piece/chapter-1/p/2?style=list"),
            Some(Target::Chapter("/manga/one-piece/chapter-1".into()))
        );
        assert_eq!(
            t("https://www.mangaread.org/manga/kanan-sama-wa-akumade-choroi"),
            Some(Target::Manga("/manga/kanan-sama-wa-akumade-choroi".into()))
        );
        assert_eq!(
            t("https://www.mangaread.org/manga/one-piece/"),
            Some(Target::Manga("/manga/one-piece".into()))
        );
        for raw in [
            "https://www.mangaread.org/manga/one-piece/?x=1",
            "https://www.mangaread.org/manga/",
            "https://www.mangaread.org/genres/action/",
            "https://m.mangaread.org/manga/one-piece/",
            "https://www.mangaread.org.evil.test/manga/one-piece/",
            "https://www.mangaread.org:8443/manga/one-piece/",
            "https://u:p@www.mangaread.org/manga/one-piece/",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[test]
    fn parses_chapter_strings() {
        let parse = |info: &str| {
            let mut data = Map::new();
            parse_chapter_string(info, &mut data).map(|()| data)
        };
        let d = parse(" One - Piece - Chapter 1053.5 - The Emperor &amp; Co ").unwrap();
        assert_eq!(d["manga"], "One - Piece");
        assert_eq!(d["chapter"], 1053);
        assert_eq!(d["chapter_minor"], ".5");
        assert_eq!(d["title"], "The Emperor & Co");
        let d = parse("chapter 7").unwrap();
        assert_eq!(d["manga"], "");
        assert_eq!(d["chapter"], 7);
        assert_eq!(d["chapter_minor"], "");
        assert_eq!(d["title"], "");
        let mut data = keywords([("manga", "Kept".into())]);
        parse_chapter_string("Other - Chapter 2", &mut data).unwrap();
        assert_eq!(data["manga"], "Kept");
        assert!(parse("Prologue").is_err());
    }

    #[tokio::test]
    async fn extracts_chapter() {
        let page = r#"<div class="wp-manga-tags-list"><a>Action</a>, <a>Pirates</a></div>
<h1 id="chapter-heading">One Piece - Chapter 1053.5 - Emperor</h1>
<div class="reading-content">
<img id="image-0" src="
 https://www.mangaread.org/wp-content/uploads/WP-manga/data/op/1053/01.jpg" class="wp-manga-chapter-img">
<img id="image-1" src="https://www.mangaread.org/wp-content/uploads/WP-manga/data/op/1053/02.png">
</div><div class="entry-header footer"><img id="image-9" src="https://x.test/no.jpg">"#;
        let (root, requests) = serve(vec![html(page)]);
        let http = http("MangaRead", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.mangaread.org/manga/one-piece/chapter-1053-5/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].url.as_str(),
            "https://www.mangaread.org/wp-content/uploads/WP-manga/data/op/1053/01.jpg"
        );
        let k = kw(&files[1]);
        assert_eq!(k["manga"], "One Piece");
        assert_eq!(k["tags"], serde_json::json!(["Action", "Pirates"]));
        assert_eq!(k["chapter"], 1053);
        assert_eq!(k["chapter_minor"], ".5");
        assert_eq!(k["title"], "Emperor");
        assert_eq!(k["count"], 2);
        assert_eq!(k["page"], 2);
        assert_eq!(k["language"], "English");
        // Windows path sanitization replaces the colon in the chapter title.
        let colon = if cfg!(windows) { "_" } else { ":" };
        assert_eq!(
            path(&files[1]),
            format!("mangaread/One Piece/c1053.5{colon} Emperor/One Piece_c1053.5_002.png")
        );
        assert_eq!(
            archive(&files[1]),
            r#"["{manga}_{chapter}{chapter_minor}_{page}",["One Piece","1053",".5","2"]]"#
        );
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /manga/one-piece/chapter-1053-5 "));
    }

    #[tokio::test]
    async fn chapter_without_heading_fails() {
        let (root, requests) = serve(vec![html("<p>gone</p>")]);
        let http = http("MangaRead", root);
        assert!(
            crate::sites::testing::fails(extract(
                &http,
                Url::parse("https://www.mangaread.org/manga/x/chapter-1/").unwrap(),
            ))
            .await
        );
        requests.join().unwrap();
    }

    const MANGA_PAGE: &str = "<h1> One Piece </h1>
<div class=\"summary__content\"><p>Pirates &amp; treasure</p></div>
<div class=\"summary_content\"><span class=\"total_votes\"> 4.7 </span>
Alternative\t\t</h5>\n\t</div><div>ワンピース; OP</div>
<div class=\"author-content\"><a rel=\"tag\">Oda</a></div>
<div class=\"artist-content\"><a rel=\"tag\">Oda</a></div>
<div class=\"genres-content\"><a rel=\"tag\">Action</a>, <a rel=\"tag\">Comedy</a></div>
\tType\t</h5></div><div>Manga\n</div>
\tRelease\t</h5></div><div><a>1997</a>\n</div>
\tStatus\t</h5></div><div>OnGoing\n</div>
<div class=\"manga-action\">
<li class=\"wp-manga-chapter\"><a href=\"https://www.mangaread.org/manga/one-piece/chapter-2/\">
Chapter 2 - Second</a></li>
<li class=\"wp-manga-chapter\"><a href=\"https://www.mangaread.org/manga/one-piece/chapter-1-5/\">
One Piece - Chapter 1.5</a></li>";

    #[test]
    fn parses_manga_page() {
        let chapters = chapters(MANGA_PAGE).unwrap();
        assert_eq!(chapters.len(), 2);
        let k = &chapters[0].1;
        assert_eq!(k["manga"], "One Piece");
        assert_eq!(k["description"], "Pirates & treasure");
        assert_eq!(k["rating"], 4.7);
        assert_eq!(k["manga_alt"], serde_json::json!(["ワンピース", "OP"]));
        assert_eq!(k["author"], serde_json::json!(["Oda"]));
        assert_eq!(k["genres"], serde_json::json!(["Action", "Comedy"]));
        assert_eq!(k["type"], "Manga");
        assert_eq!(k["release"], 1997);
        assert_eq!(k["status"], "OnGoing");
        assert_eq!(k["chapter"], 2);
        assert_eq!(k["title"], "Second");
        let k = &chapters[1].1;
        assert_eq!(k["manga"], "One Piece");
        assert_eq!(k["chapter"], 1);
        assert_eq!(k["chapter_minor"], ".5");
        assert_eq!(k["title"], "");
        assert!(super::chapters(r#"<body class="error404">"#).is_err());
    }

    #[tokio::test]
    async fn manga_queues_chapters_oldest_first() {
        let child = r#"<h1 id="chapter-heading">Chapter 1</h1>"#;
        let (root, requests) = serve(vec![html(MANGA_PAGE), html(child), html(child)]);
        let http = http("MangaRead", root);
        assert!(
            media(extract(
                &http,
                Url::parse("https://www.mangaread.org/manga/one-piece").unwrap(),
            ))
            .await
            .is_empty()
        );
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /manga/one-piece "));
        assert!(requests[1].starts_with("GET /manga/one-piece/chapter-1-5 "));
        assert!(requests[2].starts_with("GET /manga/one-piece/chapter-2 "));
    }

    #[tokio::test]
    async fn manga_children_are_chapter_jobs() {
        let (root, requests) = serve(vec![html(MANGA_PAGE)]);
        let http = http("MangaRead", root);
        let children = queued(extract(
            &http,
            Url::parse("https://www.mangaread.org/manga/one-piece/").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].subcategory(), "manga");
        requests.join().unwrap();
    }
}
