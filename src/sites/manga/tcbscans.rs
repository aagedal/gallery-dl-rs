//! TCB Scans chapters and manga, mirroring gallery_dl/extractor/tcbscans.py.
//! Requests go to the mirror the URL names.
use super::{Gallery, keywords, text};
use crate::sites::art::{digits, extr, extract_iter, segment};
use crate::sites::{Events, Http, Route, Site, unescape};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

pub static SITE: Site = Site {
    category: "tcbscans",
    name: "TCB Scans",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CHAPTER, &MANGA],
    api_key: false,
    cookies: None,
};

/// `(?:tcb(?:-backup\.bihar-mirchi|scans)|onepiecechapters)\.(?:com|me)`.
const HOSTS: &[&str] = &[
    "tcbscans.com",
    "tcbscans.me",
    "tcb-backup.bihar-mirchi.com",
    "tcb-backup.bihar-mirchi.me",
    "onepiecechapters.com",
    "onepiecechapters.me",
];

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
    /// `text.root_from_url` and `/chapters/ID/NAME`, raw.
    Chapter { root: String, path: String },
    /// `text.root_from_url` and `/mangas/ID/NAME`, raw.
    Manga { root: String, path: String },
}

/// `(/chapters/\d+/[^/?#]+)` and `(/mangas/\d+/[^/?#]+)` on one of
/// [`HOSTS`], unanchored at the end.
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) || !HOSTS.contains(&url.host_str()?) {
        return None;
    }
    let root = format!("{}://{}", url.scheme(), url.host_str()?);
    let raw: Vec<&str> = url.path().split('/').skip(1).collect();
    match raw.as_slice() {
        [kind @ ("chapters" | "mangas"), id, name, ..] if digits(id) && segment(name) => {
            let path = format!("/{kind}/{id}/{name}");
            Some(if *kind == "chapters" {
                Target::Chapter { root, path }
            } else {
                Target::Manga { root, path }
            })
        }
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Chapter { root, path }) => crate::sites::once(async move {
            let page = page(http, &format!("{root}{path}")).await?;
            let id = path.split('/').nth(2).unwrap_or_default().to_owned();
            Ok(crate::sites::events(
                Gallery::chapter(&CHAPTER, metadata(&page), images(&page))
                    .id(id)
                    .events(&SITE, http),
            ))
        }),
        Some(Target::Manga { root, path }) => crate::sites::once(async move {
            let page = page(http, &format!("{root}{path}")).await?;
            let mut children = chapters(&page, &root);
            // `MangaExtractor.reverse = True`.
            if !http.option_bool("chapter-reverse", false) {
                children.reverse();
            }
            Ok(super::queue_urls(http, &SITE, &MANGA, children))
        }),
        None => crate::sites::fail(error("unsupported TCB Scans URL")),
    }
}

async fn page(http: &Http, url: &str) -> Result<String> {
    http.text_with(&http.api(url)?, &http.browser_headers(&[]))
        .await
}

/// `TcbscansChapterExtractor.metadata`.
fn metadata(page: &str) -> Map<String, Value> {
    let heading = extr(page, "font-bold mt-8\">", "</h1>");
    let (manga, chapter) = heading.rsplit_once(" - Chapter ").unwrap_or(("", heading));
    let (chapter, minor) = text::chapter(chapter);
    keywords([
        ("manga", unescape(manga).trim().into()),
        ("chapter", chapter.into()),
        ("chapter_minor", minor.into()),
        ("lang", "en".into()),
        ("language", "English".into()),
    ])
}

/// `TcbscansChapterExtractor.images`.
fn images(page: &str) -> Vec<super::Page> {
    extract_iter(page, "<img class=\"fixed-ratio-content\" src=\"", "\"")
        .into_iter()
        .map(|url| (url.to_owned(), None))
        .collect()
}

/// `TcbscansMangaExtractor.chapters`, in page order.
fn chapters(page: &str, root: &str) -> Vec<(Url, Map<String, Value>)> {
    let manga = unescape(extr(page, "class=\"my-3 font-bold text-3xl\">", "</h1>"));
    let list = extr(page, "class=\"col-span-2\"", "class=\"order-1");
    extract_iter(list, "<a", "</a>")
        .into_iter()
        .filter_map(|item| {
            let url = Url::parse(&format!("{root}{}", extr(item, "href=\"", "\""))).ok()?;
            let title = unescape(extr(item, "text-gray-500\">", "</div>"));
            let label = extr(item, "font-bold\">", "</div>");
            let number = label.rsplit_once(" Chapter ").map_or(label, |(_, n)| n);
            let (chapter, minor) = text::chapter(number);
            Some((
                url,
                keywords([
                    ("manga", manga.clone().into()),
                    ("lang", "en".into()),
                    ("language", "English".into()),
                    ("title", title.into()),
                    ("chapter", chapter.into()),
                    ("chapter_minor", minor.into()),
                ]),
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::tests::{archive, path};
    use super::*;
    use crate::sites::testing::{html, http, keywords as kw, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://tcbscans.me/chapters/4708/chainsaw-man-chapter-108"),
            Some(Target::Chapter {
                root: "https://tcbscans.me".into(),
                path: "/chapters/4708/chainsaw-man-chapter-108".into()
            })
        );
        assert_eq!(
            t("http://onepiecechapters.com/chapters/4716/one-piece-chapter-1065/x"),
            Some(Target::Chapter {
                root: "http://onepiecechapters.com".into(),
                path: "/chapters/4716/one-piece-chapter-1065".into()
            })
        );
        assert_eq!(
            t("https://tcb-backup.bihar-mirchi.com/mangas/5/one-piece"),
            Some(Target::Manga {
                root: "https://tcb-backup.bihar-mirchi.com".into(),
                path: "/mangas/5/one-piece".into()
            })
        );
        for raw in [
            "https://tcbscans.me/chapters/x/name",
            "https://tcbscans.me/chapters/4708/",
            "https://tcbscans.me/mangas/5",
            "https://www.tcbscans.me/mangas/5/one-piece",
            "https://tcbscans.net/mangas/5/one-piece",
            "https://tcbscans.me.evil.test/mangas/5/one-piece",
            "https://tcbscans.me:8443/mangas/5/one-piece",
            "https://u:p@tcbscans.me/mangas/5/one-piece",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[tokio::test]
    async fn extracts_chapter_images() {
        let page = r#"<h1 class="text-lg md:text-2xl font-bold mt-8">One Piece &amp; Co - Chapter 1065.5</h1>
<img class="fixed-ratio-content" src="https://cdn.onepiecechapters.com/file/CDN-M-A-N/op_1065_01.png">
<img class="fixed-ratio-content" src="https://cdn.onepiecechapters.com/file/CDN-M-A-N/op_1065_02.jpg">"#;
        let (root, requests) = serve(vec![html(page)]);
        let http = http("TCB Scans", root);
        let files = media(extract(
            &http,
            Url::parse("https://tcbscans.me/chapters/4716/one-piece-chapter-1065").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[1].url.as_str(),
            "https://cdn.onepiecechapters.com/file/CDN-M-A-N/op_1065_02.jpg"
        );
        let k = kw(&files[1]);
        assert_eq!(k["manga"], "One Piece & Co");
        assert_eq!(k["chapter"], 1065);
        assert_eq!(k["chapter_minor"], ".5");
        assert_eq!(k["count"], 2);
        assert_eq!(k["page"], 2);
        assert_eq!(k["filename"], "op_1065_02");
        assert_eq!(k["language"], "English");
        assert_eq!(
            path(&files[1]),
            "tcbscans/One Piece & Co/c1065.5/One Piece & Co_c1065.5_002.jpg"
        );
        assert_eq!(
            archive(&files[1]),
            r#"["{manga}_{chapter}{chapter_minor}_{page}",["One Piece & Co","1065",".5","2"]]"#
        );
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /chapters/4716/one-piece-chapter-1065 "));
    }

    const MANGA_PAGE: &str = r#"<h1 class="my-3 font-bold text-3xl">One Piece</h1>
<div class="col-span-2">
<a href="/chapters/7001/one-piece-chapter-1101" class="block">
<div class="text-lg font-bold">One Piece Chapter 1101</div>
<div class="text-gray-500">Bonney&#39;s Journey</div></a>
<a href="/chapters/7000/one-piece-chapter-1100.5" class="block">
<div class="text-lg font-bold">One Piece Chapter 1100.5</div>
<div class="text-gray-500"></div></a>
</div><div class="order-1">"#;

    #[test]
    fn parses_chapter_list() {
        let chapters = chapters(MANGA_PAGE, "https://tcbscans.me");
        assert_eq!(chapters.len(), 2);
        assert_eq!(
            chapters[0].0.as_str(),
            "https://tcbscans.me/chapters/7001/one-piece-chapter-1101"
        );
        let k = &chapters[0].1;
        assert_eq!(k["manga"], "One Piece");
        assert_eq!(k["title"], "Bonney's Journey");
        assert_eq!(k["chapter"], 1101);
        assert_eq!(k["chapter_minor"], "");
        assert_eq!(chapters[1].1["chapter"], 1100);
        assert_eq!(chapters[1].1["chapter_minor"], ".5");
    }

    #[tokio::test]
    async fn manga_queues_chapters_oldest_first_on_its_mirror() {
        let (root, requests) = serve(vec![html(MANGA_PAGE), html(""), html("")]);
        let http = http("TCB Scans", root);
        let files = media(extract(
            &http,
            Url::parse("https://onepiecechapters.com/mangas/5/one-piece").unwrap(),
        ))
        .await;
        assert!(files.is_empty());
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /mangas/5/one-piece "));
        assert!(requests[1].starts_with("GET /chapters/7000/one-piece-chapter-1100.5 "));
        assert!(requests[2].starts_with("GET /chapters/7001/one-piece-chapter-1101 "));
    }
}
