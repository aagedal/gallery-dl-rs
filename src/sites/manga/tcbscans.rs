//! TCB Scans chapters and manga, mirroring gallery_dl/extractor/tcbscans.py.
use super::{Gallery, chapter_keys, english, fetch, set};
use crate::error;
use crate::sites::gdl::{extr, extract_iter, root_of};
use crate::sites::{Events, Http, Route, Site, unescape};
use serde_json::Map;
use url::Url;

static CHAPTER: Route = super::chapter_route();
static MANGA: Route = super::manga_route();

pub static SITE: Site = Site {
    category: "tcbscans",
    name: "TCB Scans",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CHAPTER, &MANGA],
    api_key: false,
    cookies: None,
};

const HOSTS: &[&str] = &[
    "tcb-backup.bihar-mirchi.com",
    "tcb-backup.bihar-mirchi.me",
    "tcbscans.com",
    "tcbscans.me",
    "onepiecechapters.com",
    "onepiecechapters.me",
];

#[derive(Debug, PartialEq)]
enum Target {
    Chapter,
    Manga,
}

/// `/chapters/\d+/[^/?#]+` or `/mangas/\d+/[^/?#]+`.
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) || !HOSTS.contains(&url.host_str()?) {
        return None;
    }
    let mut parts = url.path().split('/').skip(1);
    let kind = match parts.next()? {
        "chapters" => Target::Chapter,
        "mangas" => Target::Manga,
        _ => return None,
    };
    let id = parts.next()?;
    let slug = parts.next()?;
    (!id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) && !slug.is_empty()).then_some(kind)
}

/// The matched path, without anything after the slug.
fn page_url(url: &Url) -> String {
    let path: Vec<&str> = url.path().split('/').take(4).collect();
    format!("{}{}", root_of(url), path.join("/"))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(kind) = target(&url) else {
        return crate::sites::fail(error("unsupported TCB Scans URL"));
    };
    crate::sites::once(async move {
        let page = fetch(http, &page_url(&url)).await?;
        Ok(match kind {
            Target::Chapter => crate::sites::events(
                Gallery::chapter(&CHAPTER, chapter_metadata(&page), images(&page))
                    .events(&SITE, http),
            ),
            Target::Manga => {
                super::queue_chapters(http, &SITE, &MANGA, chapters(&page, &root_of(&url)), true)
            }
        })
    })
}

fn images(page: &str) -> Vec<super::Page> {
    extract_iter(page, "<img class=\"fixed-ratio-content\" src=\"", "\"")
        .into_iter()
        .map(|url| (url.to_owned(), None))
        .collect()
}

fn chapter_metadata(page: &str) -> Map<String, serde_json::Value> {
    let heading = extr(page, "font-bold mt-8\">", "</h1>");
    let (manga, chapter) = match heading.rfind(" - Chapter ") {
        Some(i) => (&heading[..i], &heading[i + " - Chapter ".len()..]),
        None => ("", heading),
    };
    let mut data = Map::new();
    set(&mut data, "manga", unescape(manga).trim());
    chapter_keys(&mut data, chapter);
    english(&mut data);
    data
}

fn chapters(page: &str, root: &str) -> Vec<(Url, Map<String, serde_json::Value>)> {
    let mut data = Map::new();
    set(
        &mut data,
        "manga",
        unescape(extr(page, "class=\"my-3 font-bold text-3xl\">", "</h1>")),
    );
    english(&mut data);
    let list = extr(page, "class=\"col-span-2\"", "class=\"order-1");
    let mut results = Vec::new();
    for chapter in extract_iter(list, "<a", "</a>") {
        let href = extr(chapter, "href=\"", "\"");
        set(
            &mut data,
            "title",
            unescape(extr(chapter, "text-gray-500\">", "</div>")),
        );
        let label = extr(chapter, "font-bold\">", "</div>");
        let number = label.rsplit_once(" Chapter ").map_or("", |(_, n)| n);
        chapter_keys(&mut data, number);
        if let Ok(url) = Url::parse(&format!("{root}{href}")) {
            results.push((url, data.clone()));
        }
    }
    results
}

#[cfg(test)]
mod tests {
    use super::super::tests::{archive, path};
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://tcbscans.me/chapters/4708/chainsaw-man-chapter-108"),
            Some(Target::Chapter)
        );
        assert_eq!(
            t("https://onepiecechapters.com/chapters/4716/one-piece-chapter-1065"),
            Some(Target::Chapter)
        );
        assert_eq!(
            t("https://tcb-backup.bihar-mirchi.com/mangas/5/one-piece"),
            Some(Target::Manga)
        );
        for raw in [
            "https://tcbscans.me/chapters/x/a",
            "https://tcbscans.me/chapters/12/",
            "https://www.tcbscans.me/chapters/1/a",
            "https://tcbscans.org/chapters/1/a",
            "https://tcbscans.me/projects",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[tokio::test]
    async fn extracts_chapter() {
        let page = r#"<h1 class="font-bold mt-8">Chainsaw Man - Chapter 108.5</h1>
            <img class="fixed-ratio-content" src="https://cdn.onepiecechapters.com/file/x/1.png">
            <img class="fixed-ratio-content" src="https://cdn.onepiecechapters.com/file/x/2.png">"#;
        let (root, server) = serve(vec![html(page)]);
        let http = http("tcbscans", root);
        let files = media(extract(
            &http,
            Url::parse("https://tcbscans.me/chapters/4708/chainsaw-man-chapter-108?x").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[1]);
        assert_eq!(k["manga"], "Chainsaw Man");
        assert_eq!(k["chapter"], 108);
        assert_eq!(k["chapter_minor"], ".5");
        assert_eq!(k["count"], 2);
        assert_eq!(k["language"], "English");
        assert_eq!(
            path(&files[1]),
            "tcbscans/Chainsaw Man/c108.5/Chainsaw Man_c108.5_002.png"
        );
        assert_eq!(
            archive(&files[0]),
            r#"["{manga}_{chapter}{chapter_minor}_{page}",["Chainsaw Man","108",".5","1"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /chapters/4708/chainsaw-man-chapter-108 "));
    }

    #[test]
    fn lists_chapters() {
        let page = r#"<h1 class="my-3 font-bold text-3xl">One &amp; Piece</h1>
            <div class="col-span-2">
            <a href="/chapters/2/op-1066"><div class="font-bold">One Piece Chapter 1066</div>
              <div class="text-gray-500">Title B</div></a>
            <a href="/chapters/1/op-1065"><div class="font-bold">One Piece Chapter 1065.5</div>
              <div class="text-gray-500">Title A</div></a>
            </div><div class="order-1">"#;
        let list = chapters(page, "https://tcbscans.me");
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].0.as_str(), "https://tcbscans.me/chapters/2/op-1066");
        assert_eq!(list[0].1["manga"], "One & Piece");
        assert_eq!(list[1].1["chapter"], 1065);
        assert_eq!(list[1].1["chapter_minor"], ".5");
        assert_eq!(list[1].1["title"], "Title A");
    }
}
