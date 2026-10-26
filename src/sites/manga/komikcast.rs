//! Komikcast chapters and manga, mirroring gallery_dl/extractor/komikcast.py.
use super::{Gallery, set};
use crate::error;
use crate::sites::gdl::{
    Scan, captures, extr, extract_iter, group, parse_int, partition, re, remove_html, rpartition,
    split_html, unescape,
};
use crate::sites::{Events, Http, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://komikcast.li";
const BASE: &str = r"^https?://(?:www\.)?komikcast\d*\.(?:l(?:i|a|ol)|com|cz|site|mo?e)";

static CHAPTER: Route = super::chapter_route();
static MANGA: Route = super::manga_route();

pub static SITE: Site = Site {
    category: "komikcast",
    name: "Komikcast",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CHAPTER, &MANGA],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Chapter(String),
    Manga(String),
}

fn target(url: &Url) -> Option<Target> {
    let chapter = re!(&format!(r"{BASE}(/chapter/[^/?#]+/)"));
    if let Some(caps) = captures(chapter, url) {
        return Some(Target::Chapter(group(&caps, 1).into()));
    }
    let manga = re!(&format!(r"{BASE}(/(?:komik/)?[^/?#]+/?)$"));
    captures(manga, url).map(|caps| Target::Manga(group(&caps, 1).into()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Komikcast URL"));
    };
    crate::sites::once(async move {
        Ok(match target {
            Target::Chapter(path) => {
                let page = super::fetch(http, &format!("{ROOT}{path}")).await?;
                let mut data = Map::new();
                parse_chapter_string(extr(&page, "<title>", " - Komikcast<"), &mut data);
                crate::sites::events(
                    Gallery::chapter(&CHAPTER, data, images(&page)).events(&SITE, http),
                )
            }
            Target::Manga(path) => {
                let page = super::fetch(http, &format!("{ROOT}{path}")).await?;
                super::queue_chapters(http, &SITE, &MANGA, chapters(&page), true)
            }
        })
    })
}

/// `KomikcastBase.parse_chapter_string`.
fn parse_chapter_string(text: &str, data: &mut Map<String, Value>) {
    let pattern = re!(r"^(?:(.*) Chapter )?0*(\d+)([^ ]*)(?: (?:- )?(.+))?");
    let text = unescape(text);
    let caps = pattern.captures(&text);
    let field = |i| caps.as_ref().map_or("", |c| group(c, i));
    set(data, "chapter_minor", field(3));
    if !field(1).is_empty() {
        set(data, "manga", partition(field(1), " Chapter ").0);
    }
    let title = field(4);
    if !title.is_empty() && !title.to_lowercase().starts_with("bahasa indonesia") {
        set(data, "title", title.trim());
    } else {
        set(data, "title", "");
    }
    set(data, "chapter", parse_int(field(2)));
    set(data, "lang", "id");
    set(data, "language", "Indonesian");
}

fn images(page: &str) -> Vec<super::Page> {
    let area = extr(page, "<div class=\"main-reading-area", "</div");
    re!(r#"<img[^>]* src=["']([^"']+)"#)
        .captures_iter(area)
        .map(|caps| (unescape(group(&caps, 1)), None))
        .collect()
}

fn chapters(page: &str) -> Vec<(Url, Map<String, Value>)> {
    let mut scan = Scan::new(page);
    let manga = scan.extr("<title>", " - Komikcast<");
    let genres = scan.extr("class=\"komik_info-content-genre\">", "</span>");
    let author = scan.extr(">Author:", "</span>");
    let kind = scan.extr(">Type:", "</span>");
    let mut data = Map::new();
    set(&mut data, "manga", unescape(manga));
    set(&mut data, "genres", split_html(genres));
    set(&mut data, "author", remove_html(author));
    set(&mut data, "type", remove_html(kind));
    extract_iter(page, "<a class=\"chapter-link-item\" href=\"", "</a")
        .into_iter()
        .filter_map(|item| {
            let (url, _, chapter) = rpartition(item, "\">Chapter");
            let (number, sep, minor) = partition(chapter.trim(), ".");
            set(&mut data, "chapter", parse_int(number));
            set(&mut data, "chapter_minor", format!("{sep}{minor}"));
            Some((Url::parse(url).ok()?, data.clone()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::tests::path;
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        for host in [
            "komikcast.li",
            "komikcast02.com",
            "komikcast.lol",
            "komikcast.site",
            "komikcast.me",
            "komikcast.cz",
            "komikcast.la",
            "komikcast.moe",
        ] {
            assert_eq!(
                t(&format!(
                    "https://{host}/chapter/apotheosis-chapter-02-2-bahasa-indonesia/"
                )),
                Some(Target::Chapter(
                    "/chapter/apotheosis-chapter-02-2-bahasa-indonesia/".into()
                )),
                "{host}"
            );
        }
        assert_eq!(
            t("https://komikcast.site/komik/090-eko-to-issho/"),
            Some(Target::Manga("/komik/090-eko-to-issho/".into()))
        );
        assert_eq!(
            t("https://komikcast.me/tonari-no-kashiwagi-san/"),
            Some(Target::Manga("/tonari-no-kashiwagi-san/".into()))
        );
        assert_eq!(t("https://komikcast.org/komik/x"), None);
        assert_eq!(t("https://komikcast.me/a/b/c"), None);
    }

    #[test]
    fn parses_chapter_strings() {
        let mut data = Map::new();
        parse_chapter_string("Apotheosis Chapter 02.2 Bahasa Indonesia", &mut data);
        assert_eq!(data["manga"], "Apotheosis");
        assert_eq!(data["chapter"], 2);
        assert_eq!(data["chapter_minor"], ".2");
        assert_eq!(data["title"], "");
        let mut data = Map::new();
        parse_chapter_string("Soul Land Chapter 300.1 - The Beginning", &mut data);
        assert_eq!(data["title"], "The Beginning");
    }

    #[tokio::test]
    async fn extracts_chapter() {
        let page = r#"<title>Apotheosis Chapter 02.2 Bahasa Indonesia - Komikcast</title>
            <div class="main-reading-area"><img class="x" src="https://sv1.imgkc1.my.id/a/1.jpg?a=1&amp;b=2">
            <img src='https://sv1.imgkc1.my.id/a/2.jpg'></div>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = http("komikcast", root);
        let files = media(extract(
            &http,
            Url::parse("https://komikcast.li/chapter/apotheosis-chapter-02-2-bahasa-indonesia/")
                .unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].url.as_str(),
            "https://sv1.imgkc1.my.id/a/1.jpg?a=1&b=2"
        );
        assert_eq!(keywords(&files[0])["language"], "Indonesian");
        assert_eq!(
            path(&files[1]),
            "komikcast/Apotheosis/c002.2/Apotheosis_c002.2_002.jpg"
        );
        server.join().unwrap();
    }

    #[test]
    fn lists_chapters() {
        let page = r#"<title>090 Eko to Issho - Komikcast</title><span class="komik_info-content-genre"><a>Comedy</a></span>
            <span><b>Author:</b> Someone</span><span><b>Type:</b> Manga</span>
            <a class="chapter-link-item" href="https://komikcast.li/chapter/eko-chapter-2/">Chapter 2.5</a>"#;
        let list = chapters(page);
        assert_eq!(list.len(), 1);
        assert_eq!(
            list[0].0.as_str(),
            "https://komikcast.li/chapter/eko-chapter-2/"
        );
        assert_eq!(list[0].1["chapter_minor"], ".5");
        assert_eq!(list[0].1["author"], "Someone");
        assert_eq!(list[0].1["genres"], serde_json::json!(["Comedy"]));
    }
}
