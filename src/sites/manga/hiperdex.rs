//! Hiperdex chapters, manga and artists, mirroring
//! gallery_dl/extractor/hiperdex.py: a Madara (WordPress) reader whose
//! chapter list comes from a POST to the manga's `ajax/chapters/`.
//! Requests go to the mirror the URL names.
use super::{Gallery, Memo, keywords, set, text};
use crate::sites::art::{Scan, extr, extract_iter, segment, split_html};
use crate::sites::http::Body;
use crate::sites::{Events, Http, Route, Site, remove_html, unescape};
use crate::{Result, error};
use futures_util::StreamExt;
use serde_json::{Map, Value};
use url::Url;

pub static SITE: Site = Site {
    category: "hiperdex",
    name: "Hiperdex",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CHAPTER, &MANGA, &ARTIST],
    api_key: false,
    cookies: None,
};

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
static ARTIST: Route = Route::new("artist", "{filename}.{extension}", &["{category}"], "");

#[derive(Debug, PartialEq)]
enum Target {
    /// Root, `/manga[s]/MANGA/CHAPTER`, manga and chapter slugs, raw.
    Chapter {
        root: String,
        path: String,
        manga: String,
        chapter: String,
    },
    /// Root, `/manga[s]/MANGA` and the manga slug, raw.
    Manga {
        root: String,
        path: String,
        manga: String,
    },
    /// Root and `/manga-artist/NAME` or `/manga-author/NAME`, raw.
    Artist { root: String, path: String },
}

/// `(?:www\.)?(?:1st)?hiper(?:dex|toon)\d?\.(?:com|net|info|top)`.
fn hiperdex_host(host: &str) -> bool {
    let host = host.strip_prefix("www.").unwrap_or(host);
    let host = host.strip_prefix("1st").unwrap_or(host);
    let Some(rest) = host.strip_prefix("hiper") else {
        return false;
    };
    let Some(rest) = rest
        .strip_prefix("dex")
        .or_else(|| rest.strip_prefix("toon"))
    else {
        return false;
    };
    let rest = rest
        .strip_prefix(|c: char| c.is_ascii_digit())
        .unwrap_or(rest);
    matches!(rest, ".com" | ".net" | ".info" | ".top")
}

/// `BASE_PATTERN(/mangas?/([^/?#]+)/([^/?#]+))` (unanchored),
/// `BASE_PATTERN(/mangas?/([^/?#]+))/?$` and
/// `BASE_PATTERN(/manga-a(?:rtist|uthor)/(?:[^/?#]+))` (unanchored).
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) || !hiperdex_host(url.host_str()?) {
        return None;
    }
    let root = format!("{}://{}", url.scheme(), url.host_str()?);
    let end = url.query().is_none() && url.fragment().is_none();
    let raw: Vec<&str> = url.path().split('/').skip(1).collect();
    match raw.as_slice() {
        [kind @ ("manga" | "mangas"), manga, chapter, ..] if segment(manga) && segment(chapter) => {
            Some(Target::Chapter {
                path: format!("/{kind}/{manga}/{chapter}"),
                root,
                manga: (*manga).to_owned(),
                chapter: (*chapter).to_owned(),
            })
        }
        [kind @ ("manga" | "mangas"), manga] | [kind @ ("manga" | "mangas"), manga, ""]
            if segment(manga) && end =>
        {
            Some(Target::Manga {
                path: format!("/{kind}/{manga}"),
                root,
                manga: (*manga).to_owned(),
            })
        }
        [kind @ ("manga-artist" | "manga-author"), name, ..] if segment(name) => {
            Some(Target::Artist {
                path: format!("/{kind}/{name}"),
                root,
            })
        }
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Chapter {
            root,
            path,
            manga,
            chapter,
        }) => chapter_events(http, root, path, manga, chapter, Memo::default()),
        Some(Target::Manga { root, path, manga }) => {
            crate::sites::once(async move { manga_events(http, &root, &path, &manga).await })
        }
        Some(Target::Artist { root, path }) => crate::sites::once(async move {
            let page = page(http, &format!("{root}{path}/")).await?;
            // `HiperdexArtistExtractor.reverse = False`.
            let mut children = artist_manga(&page);
            if http.option_bool("chapter-reverse", false) {
                children.reverse();
            }
            Ok(super::queue_urls(http, &SITE, &ARTIST, children))
        }),
        None => crate::sites::fail(error("unsupported Hiperdex URL")),
    }
}

async fn page(http: &Http, url: &str) -> Result<String> {
    http.text_with(&http.api(url)?, &http.browser_headers(&[]))
        .await
}

/// `HiperdexBase.manga_data`, memoized by slug like `Extractor.cache`.
async fn manga_data(
    http: &Http,
    root: &str,
    manga: &str,
    page: Option<&str>,
    memo: &Memo,
) -> Result<Map<String, Value>> {
    if let Some(Value::Object(data)) = memo.get(manga) {
        return Ok(data);
    }
    let data = match page {
        Some(page) => parse_manga(page),
        None => parse_manga(&self::page(http, &format!("{root}/manga/{manga}/")).await?),
    };
    memo.insert(manga, Value::Object(data.clone()));
    Ok(data)
}

fn parse_manga(page: &str) -> Map<String, Value> {
    let mut e = Scan::new(page);
    let url = unescape(e.extr("property=\"og:url\" content=\"", "\""));
    let manga = unescape(e.extr(" property=\"name\" title=\"", "\""));
    let score = text::parse_float(e.extr("id=\"averagerate\">", "<"));
    let author = remove_html(e.extr("class=\"author-content\">", "</div>"));
    let artist = remove_html(e.extr("class=\"artist-content\">", "</div>"));
    let genre: Vec<String> = split_html(e.extr("class=\"genres-content\">", "</div>"))
        .into_iter()
        .step_by(2)
        .collect();
    let kind = e.extr("class=\"summary-content\">", "<").trim().to_owned();
    let release = text::parse_int(&remove_html(e.extr("class=\"summary-content\">", "</div>")));
    let status = e.extr("class=\"summary-content\">", "<").trim().to_owned();
    let description = remove_html(&unescape(
        e.extr("<div class=\"description-summary\">", "</div>"),
    ));
    keywords([
        ("url", url.into()),
        ("manga", manga.into()),
        ("score", score),
        ("author", author.into()),
        ("artist", artist.into()),
        ("genre", genre.into()),
        ("type", kind.into()),
        ("release", release.into()),
        ("status", status.into()),
        ("description", description.into()),
        ("language", "English".into()),
        ("lang", "en".into()),
    ])
}

/// `HiperdexBase.chapter_data`: manga data plus the chapter number parsed
/// from its slug.
fn chapter_data(mut data: Map<String, Value>, chapter: &str) -> Map<String, Value> {
    let chapter = chapter.strip_prefix("chapter-").unwrap_or(chapter);
    let (number, _, minor) = text::partition(chapter, "-");
    set(&mut data, "chapter", text::parse_int(number));
    let minor = if minor.is_empty() || minor == "end" {
        String::new()
    } else {
        format!(".{minor}")
    };
    set(&mut data, "chapter_minor", minor);
    data
}

/// `HiperdexChapterExtractor`: the reader page and the (lower-cased)
/// manga's data.
fn chapter_events(
    http: &Http,
    root: String,
    path: String,
    manga: String,
    chapter: String,
    memo: Memo,
) -> Events<'_> {
    crate::sites::once(async move {
        let page = page(http, &format!("{root}{path}/")).await?;
        let data = manga_data(http, &root, &manga.to_lowercase(), None, &memo).await?;
        Ok(crate::sites::events(
            Gallery::chapter(&CHAPTER, chapter_data(data, &chapter), images(&page))
                .id(format!("{manga}/{chapter}"))
                .events(&SITE, http),
        ))
    })
}

/// `id="image-\d+"\s+(?:data-)?src="([^"]+)`, stripped.
fn images(page: &str) -> Vec<super::Page> {
    let mut pages = Vec::new();
    for rest in page.split("id=\"image-").skip(1) {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        let Some(rest) = rest[digits..].strip_prefix('"').filter(|_| digits > 0) else {
            continue;
        };
        let trimmed = rest.trim_start();
        if trimmed.len() == rest.len() {
            continue;
        }
        let trimmed = trimmed.strip_prefix("data-").unwrap_or(trimmed);
        let Some(src) = trimmed.strip_prefix("src=\"") else {
            continue;
        };
        let url = &src[..src.find('"').unwrap_or(0)];
        if !url.is_empty() {
            pages.push((url.trim().to_owned(), None));
        }
    }
    pages
}

/// `HiperdexMangaExtractor`: the manga page, then its chapter list from
/// `ajax/chapters/` below the page's `og:url`, queued newest-last unless
/// `chapter-reverse`.
async fn manga_events<'a>(
    http: &'a Http,
    root: &str,
    path: &str,
    manga: &str,
) -> Result<Events<'a>> {
    let page = page(http, &format!("{root}{path}/")).await?;
    let memo = Memo::default();
    let data = manga_data(http, root, manga, Some(&page), &memo).await?;
    let page_url = data
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let page_url = Url::parse(&page_url)
        .ok()
        .filter(|url| crate::sites::plain(url) && url.host_str().is_some_and(hiperdex_host))
        .ok_or_else(|| error("Hiperdex manga page has no canonical URL"))?;
    let referer = format!(
        "https://{}",
        text::quote(page_url.as_str().get(8..).unwrap_or_default())
    );
    let list = http
        .post(
            &http.api(&format!("{page_url}ajax/chapters/"))?,
            Body::Form(&[]),
            &http.browser_headers(&[
                ("Accept", "*/*"),
                ("X-Requested-With", "XMLHttpRequest"),
                ("Origin", root),
                ("Referer", &referer),
            ]),
        )
        .await?;
    let list = String::from_utf8_lossy(&list);
    let lower = manga.to_lowercase();
    let mut chapters = Vec::new();
    for item in extract_iter(&list, "<li class=\"wp-manga-chapter", "</li>") {
        let url = extr(item, "href=\"", "\"");
        let chapter = url
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or_default();
        let base = manga_data(http, root, &lower, None, &memo).await?;
        chapters.push((url.to_owned(), chapter_data(base, chapter)));
    }
    // `MangaExtractor.reverse = True`.
    if !http.option_bool("chapter-reverse", false) {
        chapters.reverse();
    }
    let streams: Vec<Events<'a>> = chapters
        .into_iter()
        .filter_map(|(url, data)| match target(&Url::parse(&url).ok()?)? {
            Target::Chapter {
                root,
                path,
                manga,
                chapter,
            } => {
                let memo = memo.clone();
                Some(super::queue_with(&SITE, &MANGA, &data, move || {
                    chapter_events(http, root, path, manga, chapter, memo)
                }))
            }
            _ => None,
        })
        .collect();
    Ok(futures_util::stream::iter(streams).flatten().boxed())
}

/// `HiperdexArtistExtractor.chapters`: each listed manga, in page order.
fn artist_manga(page: &str) -> Vec<(Url, Map<String, Value>)> {
    extract_iter(page, "id=\"manga-item-", "<img")
        .into_iter()
        .filter_map(|info| Url::parse(extr(info, "href=\"", "\"")).ok())
        .map(|url| (url, Map::new()))
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
            t("https://1sthiperdex.com/manga/domestic-na-kanojo/154-5/"),
            Some(Target::Chapter {
                root: "https://1sthiperdex.com".into(),
                path: "/manga/domestic-na-kanojo/154-5".into(),
                manga: "domestic-na-kanojo".into(),
                chapter: "154-5".into(),
            })
        );
        assert_eq!(
            t("http://www.hiperdex2.top/mangas/x/chapter-1?a=b"),
            Some(Target::Chapter {
                root: "http://www.hiperdex2.top".into(),
                path: "/mangas/x/chapter-1".into(),
                manga: "x".into(),
                chapter: "chapter-1".into(),
            })
        );
        assert_eq!(
            t("https://hipertoon.com/manga/1st-year-max-level-manager/"),
            Some(Target::Manga {
                root: "https://hipertoon.com".into(),
                path: "/manga/1st-year-max-level-manager".into(),
                manga: "1st-year-max-level-manager".into(),
            })
        );
        assert_eq!(
            t("https://hiperdex.net/manga-artist/beck-ho/page/2"),
            Some(Target::Artist {
                root: "https://hiperdex.net".into(),
                path: "/manga-artist/beck-ho".into(),
            })
        );
        assert!(matches!(
            t("https://hiperdex.info/manga-author/name"),
            Some(Target::Artist { .. })
        ));
        for raw in [
            "https://hiperdex.com/manga/x/?page=2",
            "https://hiperdex.com/manga/",
            "https://hiperdex.com/series/x/",
            "https://hiperdex12.com/manga/x/",
            "https://hiperdex.org/manga/x/",
            "https://m.hiperdex.com/manga/x/",
            "https://hiperdex.com.evil.test/manga/x/",
            "https://hiperdex.com:8443/manga/x/",
            "https://u:p@hiperdex.com/manga/x/",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    const MANGA_PAGE: &str = r#"<meta property="og:url" content="https://hiperdex.com/manga/domestic-na-kanojo/" />
<h1 property="name" title="Domestic na Kanojo &amp; Co">x</h1>
<span id="averagerate">4.5</span>
<div class="author-content"><a href="/a">Sasuga Kei</a></div>
<div class="artist-content"><a href="/b">Sasuga Kei</a></div>
<div class="genres-content"><a>Drama</a>, <a>Romance</a></div>
<div class="summary-content"> Manga </div>
<div class="summary-content"><a>2014</a>
</div>
<div class="summary-content"> Completed </div>
<div class="description-summary"><p>Fujii &amp; Hina</p>
</div>"#;

    #[tokio::test]
    async fn extracts_chapter_with_manga_data() {
        let chapter = r#"<img id="image-0" data-src="
 https://hiperdex.com/wp-content/uploads/WP-manga/data/x/01.jpg" class="a">
<img id="image-1"  src="https://hiperdex.com/wp-content/uploads/WP-manga/data/x/02.webp">
<img id="image-x" src="https://evil.test/skip.jpg">"#;
        let (root, requests) = serve(vec![html(chapter), html(MANGA_PAGE)]);
        let http = http("Hiperdex", root);
        let files = media(extract(
            &http,
            Url::parse("https://hiperdex.com/manga/Domestic-Na-Kanojo/154-5/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].url.as_str(),
            "https://hiperdex.com/wp-content/uploads/WP-manga/data/x/01.jpg"
        );
        let k = kw(&files[1]);
        assert_eq!(k["manga"], "Domestic na Kanojo & Co");
        assert_eq!(k["url"], "https://hiperdex.com/manga/domestic-na-kanojo/");
        assert_eq!(k["score"], 4.5);
        assert_eq!(k["author"], "Sasuga Kei");
        assert_eq!(k["genre"], serde_json::json!(["Drama", "Romance"]));
        assert_eq!(k["type"], "Manga");
        assert_eq!(k["release"], 2014);
        assert_eq!(k["status"], "Completed");
        assert_eq!(k["description"], "Fujii & Hina");
        assert_eq!(k["chapter"], 154);
        assert_eq!(k["chapter_minor"], ".5");
        assert_eq!(k["count"], 2);
        assert_eq!(k["page"], 2);
        assert_eq!(k["lang"], "en");
        assert_eq!(
            path(&files[1]),
            "hiperdex/Domestic na Kanojo & Co/c154.5/Domestic na Kanojo & Co_c154.5_002.webp"
        );
        assert_eq!(
            archive(&files[1]),
            r#"["{manga}_{chapter}{chapter_minor}_{page}",["Domestic na Kanojo & Co","154",".5","2"]]"#
        );
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /manga/Domestic-Na-Kanojo/154-5/ "));
        assert!(requests[1].starts_with("GET /manga/domestic-na-kanojo/ "));
    }

    #[test]
    fn parses_chapter_slugs() {
        let data = |slug: &str| chapter_data(Map::new(), slug);
        assert_eq!(data("chapter-12")["chapter"], 12);
        assert_eq!(data("chapter-12")["chapter_minor"], "");
        assert_eq!(data("12-end")["chapter_minor"], "");
        assert_eq!(data("12-5")["chapter_minor"], ".5");
    }

    const CHAPTER_LIST: &str = r#"<ul>
<li class="wp-manga-chapter"><a href="https://hiperdex.com/manga/domestic-na-kanojo/chapter-2-end/">2</a></li>
<li class="wp-manga-chapter"><a href="https://hiperdex.com/manga/domestic-na-kanojo/chapter-1/">1</a></li>
</ul>"#;

    #[tokio::test]
    async fn manga_posts_for_chapters_and_queues_oldest_first() {
        let (root, requests) = serve(vec![
            html(MANGA_PAGE),
            html(CHAPTER_LIST),
            html(""),
            html(""),
        ]);
        let http = http("Hiperdex", root);
        let files = media(extract(
            &http,
            Url::parse("https://hiperdex.com/manga/domestic-na-kanojo").unwrap(),
        ))
        .await;
        assert!(files.is_empty());
        let requests = requests.join().unwrap();
        assert_eq!(requests.len(), 4);
        assert!(requests[0].starts_with("GET /manga/domestic-na-kanojo/ "));
        let post = requests[1].to_ascii_lowercase();
        assert!(post.starts_with("post /manga/domestic-na-kanojo/ajax/chapters/ "));
        assert!(post.contains("x-requested-with: xmlhttprequest"));
        assert!(post.contains("origin: https://hiperdex.com\r\n"));
        assert!(post.contains("referer: https://hiperdex.com/manga/domestic-na-kanojo/\r\n"));
        // The manga data is memoized; children only fetch their pages.
        assert!(requests[2].starts_with("GET /manga/domestic-na-kanojo/chapter-1/ "));
        assert!(requests[3].starts_with("GET /manga/domestic-na-kanojo/chapter-2-end/ "));
    }

    #[tokio::test]
    async fn manga_honours_chapter_reverse() {
        let (root, requests) = serve(vec![
            html(MANGA_PAGE),
            html(CHAPTER_LIST),
            html(""),
            html(""),
        ]);
        let http = http("Hiperdex", root).with_options(Some(std::sync::Arc::new(
            serde_json::json!({"chapter-reverse": true})
                .as_object()
                .unwrap()
                .clone(),
        )));
        assert!(
            media(extract(
                &http,
                Url::parse("https://hiperdex.com/manga/domestic-na-kanojo/").unwrap(),
            ))
            .await
            .is_empty()
        );
        let requests = requests.join().unwrap();
        assert!(requests[2].starts_with("GET /manga/domestic-na-kanojo/chapter-2-end/ "));
        assert!(requests[3].starts_with("GET /manga/domestic-na-kanojo/chapter-1/ "));
    }

    #[tokio::test]
    async fn manga_children_are_chapter_jobs() {
        let (root, requests) = serve(vec![html(MANGA_PAGE), html(CHAPTER_LIST)]);
        let http = http("Hiperdex", root);
        let children = queued(extract(
            &http,
            Url::parse("https://hiperdex.com/manga/domestic-na-kanojo/").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].subcategory(), "manga");
        requests.join().unwrap();
    }

    #[tokio::test]
    async fn artist_queues_manga_in_page_order() {
        let page = r#"<div id="manga-item-1"><a href="https://hiperdex.com/manga/a/">A</a><img>
<div id="manga-item-2"><a href="https://hiperdex.com/manga/b/">B</a><img>
<div id="manga-item-3"><a href="https://evil.test/manga/c/">C</a><img>"#;
        let (root, requests) = serve(vec![html(page)]);
        let http = http("Hiperdex", root);
        let children = queued(extract(
            &http,
            Url::parse("https://hiperdex.com/manga-artist/beck-ho/").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].subcategory(), "artist");
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /manga-artist/beck-ho/ "));
    }
}
