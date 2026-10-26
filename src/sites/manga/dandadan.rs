//! Dandadan chapters and the chapter index, mirroring
//! gallery_dl/extractor/dandadan.py.
use super::{Gallery, set};
use crate::error;
use crate::sites::gdl::{captures, extr, extract_iter, group, parse_int, partition, re};
use crate::sites::{Events, Http, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://dandadan.net";

static CHAPTER: Route = super::chapter_route();
static MANGA: Route = super::manga_route();

pub static SITE: Site = Site {
    category: "dandadan",
    name: "Dandadan",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CHAPTER, &MANGA],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Chapter(String),
    Manga,
}

fn target(url: &Url) -> Option<Target> {
    let chapter = re!(r"^https?://(?:www\.)?dandadan\.net(/manga/dandadan-chapter-([^/?#]+)/?)");
    if let Some(caps) = captures(chapter, url) {
        return Some(Target::Chapter(group(&caps, 1).into()));
    }
    let manga = re!(r"^https?://(?:www\.)?dandadan\.net(/)");
    captures(manga, url).map(|_| Target::Manga)
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Dandadan URL"));
    };
    crate::sites::once(async move {
        Ok(match target {
            Target::Chapter(path) => {
                let page = super::fetch(http, &format!("{ROOT}{path}")).await?;
                crate::sites::events(
                    Gallery::chapter(&CHAPTER, metadata(&page), images(&page)).events(&SITE, http),
                )
            }
            Target::Manga => {
                let page = super::fetch(http, &format!("{ROOT}/")).await?;
                let chapters = extract_iter(&page, "<li id=\"su-post", "</li>")
                    .into_iter()
                    .filter_map(|post| Url::parse(extr(post, "href=\"", "\"")).ok())
                    .map(|url| (url, Map::new()))
                    .collect();
                super::queue_chapters(http, &SITE, &MANGA, chapters, true)
            }
        })
    })
}

fn metadata(page: &str) -> Map<String, Value> {
    let (chapter, sep, minor) = partition(extr(page, "hapter ", " - "), ".");
    let mut data = Map::new();
    set(&mut data, "manga", "Dandadan");
    set(&mut data, "chapter", parse_int(chapter));
    set(&mut data, "chapter_minor", format!("{sep}{minor}"));
    set(&mut data, "lang", "en");
    data
}

fn images(page: &str) -> Vec<super::Page> {
    let figures: Vec<super::Page> = extract_iter(page, "<figure", "</figure>")
        .into_iter()
        .map(|figure| (extr(figure, "src=\"", "\"").to_owned(), None))
        .collect();
    if !figures.is_empty() {
        return figures;
    }
    extract_iter(
        page,
        "<img decoding=\"async\" class=\"aligncenter\" src=\"",
        "\"",
    )
    .into_iter()
    .map(|src| (src.to_owned(), None))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::super::tests::path;
    use super::*;
    use crate::sites::testing::{children, html, http, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://dandadan.net/manga/dandadan-chapter-40-5-3/"),
            Some(Target::Chapter("/manga/dandadan-chapter-40-5-3/".into()))
        );
        assert_eq!(t("https://dandadan.net/"), Some(Target::Manga));
        assert_eq!(t("https://dandadan.org/"), None);
    }

    #[tokio::test]
    async fn extracts_chapter_and_falls_back_to_images() {
        let page = r#"<title>Dandadan, Chapter 40.5 - Read</title>
            <img decoding="async" class="aligncenter" src="https://cdn.dandadan.net/1.jpg">"#;
        let (root, server) = serve(vec![html(page)]);
        let http = http("dandadan", root);
        let files = media(extract(
            &http,
            Url::parse("https://dandadan.net/manga/dandadan-chapter-40-5/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let k = keywords(&files[0]);
        assert_eq!(k["chapter"], 40);
        assert_eq!(k["chapter_minor"], ".5");
        assert_eq!(
            path(&files[0]),
            "dandadan/Dandadan/c040.5/Dandadan_c040.5_001.jpg"
        );
        server.join().unwrap();
        assert_eq!(
            images("<figure><img src=\"https://a/1.png\"></figure>")[0].0,
            "https://a/1.png"
        );
    }

    #[tokio::test]
    async fn index_queues_chapters() {
        let page = r#"<li id="su-post-1"><a href="https://dandadan.net/manga/dandadan-chapter-2/">2</a></li>
            <li id="su-post-2"><a href="https://dandadan.net/manga/dandadan-chapter-1/">1</a></li>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = http("dandadan", root);
        assert_eq!(
            children(extract(&http, Url::parse("https://dandadan.net/").unwrap()))
                .await
                .len(),
            2
        );
        server.join().unwrap();
    }
}
