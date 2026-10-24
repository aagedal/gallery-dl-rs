//! Komikcast chapters and manga, mirroring
//! gallery_dl/extractor/komikcast.py. Every mirror domain is read from
//! `https://komikcast.li`, as upstream does.
use super::{Gallery, set, text};
use crate::sites::art::{extr, extract_iter, segment, split_html};
use crate::sites::{Events, Http, Route, Site, remove_html, unescape};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

pub static SITE: Site = Site {
    category: "komikcast",
    name: "Komikcast",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CHAPTER, &MANGA],
    api_key: false,
    cookies: None,
};

const ROOT: &str = "https://komikcast.li";

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
    /// `/chapter/NAME/`, raw.
    Chapter(String),
    /// `/[komik/]NAME[/]`, raw.
    Manga(String),
}

/// `(?:www\.)?komikcast\d*\.(?:l(?:i|a|ol)|com|cz|site|mo?e)`.
fn komikcast_host(host: &str) -> bool {
    let host = host.strip_prefix("www.").unwrap_or(host);
    let Some(rest) = host.strip_prefix("komikcast") else {
        return false;
    };
    let rest = rest.trim_start_matches(|c: char| c.is_ascii_digit());
    matches!(
        rest,
        ".li" | ".la" | ".lol" | ".com" | ".cz" | ".site" | ".me" | ".moe"
    )
}

/// `BASE_PATTERN(/chapter/[^/?#]+/)` (unanchored) and
/// `BASE_PATTERN(/(?:komik/)?[^/?#]+/?)$`.
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) || !komikcast_host(url.host_str()?) {
        return None;
    }
    let end = url.query().is_none() && url.fragment().is_none();
    let raw: Vec<&str> = url.path().split('/').skip(1).collect();
    match raw.as_slice() {
        ["chapter", name, _, ..] if segment(name) => {
            Some(Target::Chapter(format!("/chapter/{name}/")))
        }
        [name] | [name, ""] | ["komik", name] | ["komik", name, ""] if segment(name) && end => {
            Some(Target::Manga(url.path().to_owned()))
        }
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Chapter(path)) => crate::sites::once(async move {
            let page = page(http, &format!("{ROOT}{path}")).await?;
            let data = parse_chapter_string(extr(&page, "<title>", " - Komikcast<"), Map::new())?;
            Ok(crate::sites::events(
                Gallery::chapter(&CHAPTER, data, images(&page))
                    .id(path)
                    .events(&SITE, http),
            ))
        }),
        Some(Target::Manga(path)) => crate::sites::once(async move {
            let page = page(http, &format!("{ROOT}{path}")).await?;
            let mut children = chapters(&page);
            // `MangaExtractor.reverse = True`.
            if !http.option_bool("chapter-reverse", false) {
                children.reverse();
            }
            Ok(super::queue_urls(http, &SITE, &MANGA, children))
        }),
        None => crate::sites::fail(error("unsupported Komikcast URL")),
    }
}

async fn page(http: &Http, url: &str) -> Result<String> {
    http.text_with(&http.api(url)?, &http.browser_headers(&[]))
        .await
}

/// `KomikcastBase.parse_chapter_string`: the match of
/// `(?:(.*) Chapter )?0*(\d+)([^ ]*)(?: (?:- )?(.+))?` at the start of the
/// unescaped text.
fn parse_chapter_string(info: &str, mut data: Map<String, Value>) -> Result<Map<String, Value>> {
    let info = unescape(info);
    let starts_with_digit = |s: &str| s.starts_with(|c: char| c.is_ascii_digit());
    // The greedy `(.*) Chapter ` takes the last one a number follows.
    let (manga, rest) = info
        .rmatch_indices(" Chapter ")
        .map(|(i, sep)| (&info[..i], &info[i + sep.len()..]))
        .find(|(manga, rest)| !manga.contains('\n') && starts_with_digit(rest))
        .map(|(manga, rest)| (Some(manga), rest))
        .or_else(|| starts_with_digit(&info).then_some((None, info.as_str())))
        .ok_or_else(|| error("Komikcast chapter title has no chapter number"))?;
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    let (number, rest) = rest.split_at(digits);
    // `0*(\d+)` keeps at least one digit.
    let number = match number.trim_start_matches('0') {
        "" => "0",
        number => number,
    };
    let minor_len = rest.find([' ', '\n']).unwrap_or(rest.len());
    let (minor, rest) = rest.split_at(minor_len);
    let title = rest
        .strip_prefix(' ')
        .map(|tail| {
            let line = |s: &'_ str| s.split('\n').next().unwrap_or_default().to_owned();
            match tail.strip_prefix("- ").map(line) {
                Some(title) if !title.is_empty() => title,
                _ => line(tail),
            }
        })
        .unwrap_or_default();
    set(&mut data, "chapter_minor", minor);
    if let Some(manga) = manga.filter(|m| !m.is_empty()) {
        set(&mut data, "manga", text::partition(manga, " Chapter ").0);
    }
    let title = if title.is_empty() || title.to_lowercase().starts_with("bahasa indonesia") {
        String::new()
    } else {
        title.trim().to_owned()
    };
    set(&mut data, "title", title);
    set(&mut data, "chapter", text::parse_int(number));
    set(&mut data, "lang", "id");
    set(&mut data, "language", "Indonesian");
    Ok(data)
}

/// `KomikcastChapterExtractor.images`: `<img[^>]* src=["']([^"']+)` in
/// the reading area, unescaped.
fn images(page: &str) -> Vec<super::Page> {
    let area = extr(page, "<div class=\"main-reading-area", "</div");
    let mut pages = Vec::new();
    let mut rest = area;
    while let Some(start) = rest.find("<img") {
        let tag = &rest[start..];
        let tag_end = tag.find('>').unwrap_or(tag.len());
        let found = [" src=\"", " src='"]
            .iter()
            .flat_map(|needle| tag[..tag_end].match_indices(needle))
            .map(|(i, needle)| i + needle.len())
            .filter(|at| tag[*at..].find(['"', '\'']).unwrap_or(tag.len() - at) > 0)
            .max();
        match found {
            Some(at) => {
                let len = tag[at..].find(['"', '\'']).unwrap_or(tag.len() - at);
                pages.push((unescape(&tag[at..at + len]), None));
                rest = &tag[at + len..];
            }
            None => rest = &tag[4..],
        }
    }
    pages
}

/// `KomikcastMangaExtractor.chapters`, in page order.
fn chapters(page: &str) -> Vec<(Url, Map<String, Value>)> {
    let (manga, pos) = text::extract(page, "<title>", " - Komikcast<", 0);
    let (genres, pos) = text::extract(page, "class=\"komik_info-content-genre\">", "</span>", pos);
    let (author, pos) = text::extract(page, ">Author:", "</span>", pos);
    let (kind, _) = text::extract(page, ">Type:", "</span>", pos);
    let data = super::keywords([
        ("manga", unescape(manga.unwrap_or_default()).into()),
        ("genres", split_html(genres.unwrap_or_default()).into()),
        ("author", remove_html(author.unwrap_or_default()).into()),
        ("type", remove_html(kind.unwrap_or_default()).into()),
    ]);
    extract_iter(page, "<a class=\"chapter-link-item\" href=\"", "</a")
        .into_iter()
        .filter_map(|item| {
            let (url, chapter) = item.rsplit_once("\">Chapter")?;
            let (chapter, minor) = text::chapter(chapter.trim());
            let mut data = data.clone();
            set(&mut data, "chapter", chapter);
            set(&mut data, "chapter_minor", minor);
            Some((Url::parse(url).ok()?, data))
        })
        .collect()
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
            t("https://komikcast.li/chapter/apotheosis-chapter-02-2-bahasa-indonesia/"),
            Some(Target::Chapter(
                "/chapter/apotheosis-chapter-02-2-bahasa-indonesia/".into()
            ))
        );
        assert_eq!(
            t("http://www.komikcast02.com/chapter/x/?page=2"),
            Some(Target::Chapter("/chapter/x/".into()))
        );
        assert_eq!(
            t("https://komikcast.lol/komik/090-eko-to-issho/"),
            Some(Target::Manga("/komik/090-eko-to-issho/".into()))
        );
        assert_eq!(
            t("https://komikcast.moe/tonari-no-kashiwagi-san"),
            Some(Target::Manga("/tonari-no-kashiwagi-san".into()))
        );
        for raw in [
            "https://komikcast.li/chapter/x",
            "https://komikcast.li/komik/x/?a=1",
            "https://komikcast.li/a/b/",
            "https://komikcast.org/komik/x/",
            "https://m.komikcast.li/komik/x/",
            "https://komikcast.li.evil.test/komik/x/",
            "https://komikcast.li:8443/komik/x/",
            "https://u:p@komikcast.li/komik/x/",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[test]
    fn parses_chapter_strings() {
        let parse = |info: &str| parse_chapter_string(info, Map::new()).unwrap();
        let d = parse("Apotheosis Chapter 02.2 Bahasa Indonesia");
        assert_eq!(d["manga"], "Apotheosis");
        assert_eq!(d["chapter"], 2);
        assert_eq!(d["chapter_minor"], ".2");
        assert_eq!(d["title"], "");
        let d = parse("Solo Chapter Leveling Chapter 100 - The End &amp; More ");
        assert_eq!(d["manga"], "Solo");
        assert_eq!(d["chapter"], 100);
        assert_eq!(d["title"], "The End & More");
        let d = parse("000 x");
        assert!(!d.contains_key("manga"));
        assert_eq!(d["chapter"], 0);
        assert_eq!(d["title"], "x");
        assert_eq!(d["lang"], "id");
        assert!(parse_chapter_string("Prologue", Map::new()).is_err());
    }

    #[tokio::test]
    async fn extracts_chapter() {
        let page = r#"<title>Apotheosis Chapter 02.2 Bahasa Indonesia - Komikcast</title>
<div class="main-reading-area chapter_body">
<img class="alignnone" src="https://svr4.imgkc4.my.id/wp-content/img/A/Apotheosis/002.2/01.jpg?a=1&amp;b=2" alt="">
<img alt="x" data-x=' src="' src='https://svr4.imgkc4.my.id/wp-content/img/A/Apotheosis/002.2/02.png'>
</div><img src="https://evil.test/no.jpg">"#;
        let (root, requests) = serve(vec![html(page)]);
        let http = http("Komikcast", root);
        let files = media(extract(
            &http,
            Url::parse("https://komikcast.cz/chapter/apotheosis-chapter-02-2-bahasa-indonesia/")
                .unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].url.as_str(),
            "https://svr4.imgkc4.my.id/wp-content/img/A/Apotheosis/002.2/01.jpg?a=1&b=2"
        );
        let k = kw(&files[1]);
        assert_eq!(k["manga"], "Apotheosis");
        assert_eq!(k["chapter"], 2);
        assert_eq!(k["chapter_minor"], ".2");
        assert_eq!(k["page"], 2);
        assert_eq!(k["count"], 2);
        assert_eq!(k["language"], "Indonesian");
        assert_eq!(
            path(&files[1]),
            "komikcast/Apotheosis/c002.2/Apotheosis_c002.2_002.png"
        );
        assert_eq!(
            archive(&files[1]),
            r#"["{manga}_{chapter}{chapter_minor}_{page}",["Apotheosis","2",".2","2"]]"#
        );
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /chapter/apotheosis-chapter-02-2-bahasa-indonesia/ "));
    }

    const MANGA_PAGE: &str = r#"<title>Apotheosis &amp; Co - Komikcast</title>
<span class="komik_info-content-genre"><a>Action</a> <a>Fantasy</a></span>
<span><b>Author:</b> Ranzai Studio</span><span><b>Type:</b> Manhua</span>
<a class="chapter-link-item" href="https://komikcast.li/chapter/apotheosis-chapter-2-5-bahasa-indonesia/">Chapter
 2.5 </a>
<a class="chapter-link-item" href="https://komikcast.li/chapter/apotheosis-chapter-1-bahasa-indonesia/">Chapter 1</a>"#;

    #[test]
    fn parses_manga_page() {
        let chapters = chapters(MANGA_PAGE);
        assert_eq!(chapters.len(), 2);
        let k = &chapters[0].1;
        assert_eq!(k["manga"], "Apotheosis & Co");
        assert_eq!(k["genres"], serde_json::json!(["Action", "Fantasy"]));
        assert_eq!(k["author"], "Ranzai Studio");
        assert_eq!(k["type"], "Manhua");
        assert_eq!(k["chapter"], 2);
        assert_eq!(k["chapter_minor"], ".5");
        assert_eq!(chapters[1].1["chapter"], 1);
        assert_eq!(chapters[1].1["chapter_minor"], "");
    }

    #[tokio::test]
    async fn manga_queues_chapters_oldest_first() {
        let child = "<title>A Chapter 1 - Komikcast<";
        let (root, requests) = serve(vec![html(MANGA_PAGE), html(child), html(child)]);
        let http = http("Komikcast", root);
        assert!(
            media(extract(
                &http,
                Url::parse("https://komikcast.li/komik/apotheosis/").unwrap(),
            ))
            .await
            .is_empty()
        );
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /komik/apotheosis/ "));
        assert!(requests[1].starts_with("GET /chapter/apotheosis-chapter-1-bahasa-indonesia/ "));
        assert!(requests[2].starts_with("GET /chapter/apotheosis-chapter-2-5-bahasa-indonesia/ "));
    }

    #[tokio::test]
    async fn manga_children_are_chapter_jobs() {
        let (root, requests) = serve(vec![html(MANGA_PAGE)]);
        let http = http("Komikcast", root);
        let children = queued(extract(
            &http,
            Url::parse("https://komikcast.li/komik/apotheosis").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].subcategory(), "manga");
        requests.join().unwrap();
    }
}
