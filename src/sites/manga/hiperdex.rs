//! Hiperdex chapters, manga and artists, mirroring
//! gallery_dl/extractor/hiperdex.py.
use super::{Gallery, set};
use crate::error;
use crate::sites::gdl::{
    Scan, captures, extr, extract_iter, group, parse_float, parse_int, partition, quote, re,
    remove_html, split_html, unescape,
};
use crate::sites::{Events, Http, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const BASE: &str = r"^(https?://(?:www\.)?(?:1st)?hiper(?:dex|toon)\d?\.(?:com|net|info|top))";

static CHAPTER: Route = super::chapter_route();
static MANGA: Route = super::manga_route();
static ARTIST: Route = Route::new("artist", "{filename}.{extension}", &["{category}"], "");

pub static SITE: Site = Site {
    category: "hiperdex",
    name: "Hiperdex",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CHAPTER, &MANGA, &ARTIST],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    /// Root, path, manga slug and chapter slug.
    Chapter(String, String, String, String),
    Manga(String, String, String),
    Artist(String, String),
}

fn target(url: &Url) -> Option<Target> {
    let chapter = re!(&format!(r"{BASE}(/mangas?/([^/?#]+)/([^/?#]+))"));
    if let Some(c) = captures(chapter, url) {
        return Some(Target::Chapter(
            group(&c, 1).into(),
            group(&c, 2).into(),
            group(&c, 3).into(),
            group(&c, 4).into(),
        ));
    }
    let manga = re!(&format!(r"{BASE}(/mangas?/([^/?#]+))/?$"));
    if let Some(c) = captures(manga, url) {
        return Some(Target::Manga(
            group(&c, 1).into(),
            group(&c, 2).into(),
            group(&c, 3).into(),
        ));
    }
    let artist = re!(&format!(r"{BASE}(/manga-a(?:rtist|uthor)/(?:[^/?#]+))"));
    captures(artist, url).map(|c| Target::Artist(group(&c, 1).into(), group(&c, 2).into()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Hiperdex URL"));
    };
    crate::sites::once(async move {
        Ok(match target {
            Target::Chapter(root, path, manga, chapter) => {
                let page = super::fetch(http, &format!("{root}{path}/")).await?;
                let info =
                    super::fetch(http, &format!("{root}/manga/{}/", manga.to_lowercase())).await?;
                let data = chapter_data(&manga_data(&info), &chapter);
                let pages = re!(r#"id="image-\d+"\s+(?:data-)?src="([^"]+)"#)
                    .captures_iter(&page)
                    .map(|c| (group(&c, 1).trim().to_owned(), None))
                    .collect();
                crate::sites::events(Gallery::chapter(&CHAPTER, data, pages).events(&SITE, http))
            }
            Target::Manga(root, path, _) => {
                let page = super::fetch(http, &format!("{root}{path}/")).await?;
                let data = manga_data(&page);
                let page_url = crate::sites::scalar(&data["url"]);
                let referer = format!("https://{}", quote(page_url.get(8..).unwrap_or_default()));
                let html = http
                    .post_form_text(
                        &http.api(&format!("{page_url}ajax/chapters/"))?,
                        &[],
                        &[
                            ("Accept", "*/*"),
                            ("X-Requested-With", "XMLHttpRequest"),
                            ("Origin", &root),
                            ("Referer", &referer),
                        ],
                    )
                    .await?;
                let chapters = extract_iter(&html, "<li class=\"wp-manga-chapter", "</li>")
                    .into_iter()
                    .filter_map(|item| {
                        let url = extr(item, "href=\"", "\"");
                        let slug = url
                            .trim_end_matches('/')
                            .rsplit('/')
                            .next()
                            .unwrap_or_default();
                        Some((Url::parse(url).ok()?, chapter_data(&data, slug)))
                    })
                    .collect();
                super::queue_chapters(http, &SITE, &MANGA, chapters, true)
            }
            Target::Artist(root, path) => {
                let page = super::fetch(http, &format!("{root}{path}/")).await?;
                let manga = extract_iter(&page, "id=\"manga-item-", "<img")
                    .into_iter()
                    .filter_map(|info| Url::parse(extr(info, "href=\"", "\"")).ok())
                    .map(|url| (url, Map::new()))
                    .collect();
                super::queue_chapters(http, &SITE, &ARTIST, manga, false)
            }
        })
    })
}

/// `HiperdexBase.manga_data`.
fn manga_data(page: &str) -> Map<String, Value> {
    let mut scan = Scan::new(page);
    let mut data = Map::new();
    set(
        &mut data,
        "url",
        unescape(scan.extr("property=\"og:url\" content=\"", "\"")),
    );
    set(
        &mut data,
        "manga",
        unescape(scan.extr(" property=\"name\" title=\"", "\"")),
    );
    set(
        &mut data,
        "score",
        parse_float(scan.extr("id=\"averagerate\">", "<")),
    );
    set(
        &mut data,
        "author",
        remove_html(scan.extr("class=\"author-content\">", "</div>")),
    );
    set(
        &mut data,
        "artist",
        remove_html(scan.extr("class=\"artist-content\">", "</div>")),
    );
    let genre: Vec<String> = split_html(scan.extr("class=\"genres-content\">", "</div>"))
        .into_iter()
        .step_by(2)
        .collect();
    set(&mut data, "genre", genre);
    set(
        &mut data,
        "type",
        scan.extr("class=\"summary-content\">", "<").trim(),
    );
    set(
        &mut data,
        "release",
        parse_int(&remove_html(
            scan.extr("class=\"summary-content\">", "</div>"),
        )),
    );
    set(
        &mut data,
        "status",
        scan.extr("class=\"summary-content\">", "<").trim(),
    );
    set(
        &mut data,
        "description",
        remove_html(&unescape(
            scan.extr("<div class=\"description-summary\">", "</div>"),
        )),
    );
    set(&mut data, "language", "English");
    set(&mut data, "lang", "en");
    data
}

/// `HiperdexBase.chapter_data`.
fn chapter_data(manga: &Map<String, Value>, chapter: &str) -> Map<String, Value> {
    let chapter = chapter.strip_prefix("chapter-").unwrap_or(chapter);
    let (number, _, minor) = partition(chapter, "-");
    let mut data = manga.clone();
    set(&mut data, "chapter", parse_int(number));
    set(
        &mut data,
        "chapter_minor",
        if !minor.is_empty() && minor != "end" {
            format!(".{minor}")
        } else {
            String::new()
        },
    );
    data
}

#[cfg(test)]
mod tests {
    use super::super::tests::path;
    use super::*;
    use crate::sites::testing::{children, html, http, keywords, media, serve};

    const MANGA_PAGE: &str = r#"<meta property="og:url" content="https://hiperdex.com/manga/domestic-na-kanojo/">
        <a property="name" title="Domestic na Kanojo"></a><span id="averagerate">4.5</span>
        <div class="author-content"><a>Sasuga Kei</a></div><div class="artist-content"><a>Sasuga</a></div>
        <div class="genres-content"><a>Drama</a>, <a>Romance</a></div>
        <div class="summary-content"> Manga </div><div class="summary-content"><a>2014</a></div>
        <div class="summary-content"> Completed </div><div class="description-summary"><p>Natsuo &amp; co</p></div>"#;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        for host in [
            "hipertoon.com",
            "hiperdex.com",
            "1sthiperdex.com",
            "hiperdex2.com",
            "hiperdex.net",
            "hiperdex.info",
            "hiperdex.top",
        ] {
            assert!(
                matches!(
                    t(&format!("https://{host}/manga/domestic-na-kanojo/154-5/")),
                    Some(Target::Chapter(..))
                ),
                "{host}"
            );
        }
        assert_eq!(
            t("https://hiperdex.com/mangas/x/"),
            Some(Target::Manga(
                "https://hiperdex.com".into(),
                "/mangas/x".into(),
                "x".into()
            ))
        );
        assert_eq!(
            t("https://hiperdex.com/manga-author/viagra/"),
            Some(Target::Artist(
                "https://hiperdex.com".into(),
                "/manga-author/viagra".into()
            ))
        );
        assert_eq!(t("https://hiperdex.org/manga/x/"), None);
    }

    #[test]
    fn parses_chapter_slugs() {
        let manga = manga_data(MANGA_PAGE);
        assert_eq!(manga["genre"], serde_json::json!(["Drama", "Romance"]));
        assert_eq!(manga["release"], 2014);
        assert_eq!(manga["status"], "Completed");
        assert_eq!(manga["description"], "Natsuo & co");
        assert_eq!(chapter_data(&manga, "154-5")["chapter_minor"], ".5");
        assert_eq!(chapter_data(&manga, "chapter-276-end")["chapter_minor"], "");
        assert_eq!(chapter_data(&manga, "chapter-276-end")["chapter"], 276);
    }

    #[tokio::test]
    async fn extracts_chapter() {
        let page = r#"<img id="image-0"  src=" https://hiperdex.com/wp-content/a/01.jpg " class="x">
            <img id="image-1" data-src="https://hiperdex.com/wp-content/a/02.jpg">"#;
        let (root, server) = serve(vec![html(page), html(MANGA_PAGE)]);
        let http = http("hiperdex", root);
        let files = media(extract(
            &http,
            Url::parse("https://hiperdex.com/manga/Domestic-na-kanojo/154-5/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(keywords(&files[0])["score"], 4.5);
        assert_eq!(
            path(&files[1]),
            "hiperdex/Domestic na Kanojo/c154.5/Domestic na Kanojo_c154.5_002.jpg"
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /manga/Domestic-na-kanojo/154-5/ "));
        assert!(requests[1].starts_with("GET /manga/domestic-na-kanojo/ "));
    }

    #[tokio::test]
    async fn manga_posts_for_chapter_list() {
        let list = r#"<li class="wp-manga-chapter"><a href="https://hiperdex.com/manga/domestic-na-kanojo/chapter-2/">2</a></li>
            <li class="wp-manga-chapter"><a href="https://hiperdex.com/manga/domestic-na-kanojo/chapter-1/">1</a></li>"#;
        let (root, server) = serve(vec![html(MANGA_PAGE), html(list)]);
        let http = http("hiperdex", root);
        assert_eq!(
            children(extract(
                &http,
                Url::parse("https://hiperdex.com/manga/domestic-na-kanojo/").unwrap()
            ))
            .await
            .len(),
            2
        );
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("POST /manga/domestic-na-kanojo/ajax/chapters/ "));
        assert!(
            requests[1]
                .to_ascii_lowercase()
                .contains("x-requested-with: xmlhttprequest")
        );
    }

    #[tokio::test]
    async fn artist_queues_manga() {
        let page = r#"<div id="manga-item-1"><a href="https://hiperdex.com/manga/a/"><img></div>
            <div id="manga-item-2"><a href="https://hiperdex.com/manga/b/"><img></div>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = http("hiperdex", root);
        assert_eq!(
            children(extract(
                &http,
                Url::parse("https://hiperdex.com/manga-artist/beck-ho-an/").unwrap()
            ))
            .await
            .len(),
            2
        );
        server.join().unwrap();
    }
}
