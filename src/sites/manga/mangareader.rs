//! MangaReader chapters, volumes and manga, mirroring
//! gallery_dl/extractor/mangareader.py: the manga page lists every chapter
//! and volume per language, and an AJAX fragment lists a reader's images.
//!
//! Like upstream, images are downloaded as served: pages the site marks as
//! shuffled are not descrambled.
use super::{Gallery, Memo, set, text};
use crate::sites::art::{Scan, extr, extract_iter, split_html};
use crate::sites::{Events, Http, Route, Site, remove_html, unescape};
use crate::{Result, error};
use futures_util::StreamExt;
use serde_json::{Map, Value, json};
use url::Url;

pub static SITE: Site = Site {
    category: "mangareader",
    name: "MangaReader",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CHAPTER, &MANGA],
    api_key: false,
    cookies: None,
};

const ROOT: &str = "https://mangareader.to";

static CHAPTER: Route = Route::new(
    "chapter",
    "{manga}{volume:?_v//>02}{chapter:?_c//>03}{chapter_minor:?//}_{page:>03}.{extension}",
    &[
        "{category}",
        "{manga}",
        "{volume:?v/ />02}{chapter:?c//>03}{chapter_minor:?//}{title:?: //}",
    ],
    "{manga_id}_{chapter_id}_{page}",
);
static MANGA: Route = Route::new("manga", "{filename}.{extension}", &["{category}"], "");

#[derive(Debug, PartialEq)]
enum Target {
    /// `/read/PATH/LANG/KIND-CHSTR`, raw.
    Chapter {
        path: String,
        lang: String,
        kind: String,
        chstr: String,
    },
    /// The `NAME-ID` manga path, raw.
    Manga(String),
}

/// A whole segment matching `[\w-]+-\d+`.
fn manga_path(segment: &str) -> bool {
    segment
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        && segment
            .rsplit_once('-')
            .is_some_and(|(name, id)| !name.is_empty() && crate::sites::art::digits(id))
}

/// `mangareader\.to/read/([\w-]+-\d+)/([^/?#]+)/(chapter|volume)-(\d+[^/?#]*)`
/// and `/([\w-]+-\d+)`, both unanchored at the end.
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::art::on_host(url, &["mangareader.to"]) {
        return None;
    }
    let raw: Vec<&str> = url.path().split('/').skip(1).collect();
    match raw.as_slice() {
        ["read", path, lang, reader, ..] if manga_path(path) && !lang.is_empty() => {
            let (kind, chstr) = reader.split_once('-')?;
            (matches!(kind, "chapter" | "volume")
                && chstr.starts_with(|c: char| c.is_ascii_digit()))
            .then(|| Target::Chapter {
                path: (*path).to_owned(),
                lang: (*lang).to_owned(),
                kind: kind.to_owned(),
                chstr: chstr.to_owned(),
            })
        }
        [path, ..] if manga_path(path) => Some(Target::Manga((*path).to_owned())),
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Chapter {
            path,
            lang,
            kind,
            chstr,
        }) => chapter_events(http, path, lang, kind, chstr, Memo::default()),
        Some(Target::Manga(path)) => {
            crate::sites::once(async move { manga_events(http, path).await })
        }
        None => crate::sites::fail(error("unsupported MangaReader URL")),
    }
}

async fn page(http: &Http, url: &Url, headers: &[(&str, &str)]) -> Result<String> {
    http.text_with(url, &http.browser_headers(headers)).await
}

/// `_manga_info`, memoized by manga path.
async fn manga_info(http: &Http, manga_path: &str, memo: &Memo) -> Result<Value> {
    if let Some(manga) = memo.get(manga_path) {
        return Ok(manga);
    }
    let html = page(http, &http.api(&format!("{ROOT}/{manga_path}"))?, &[]).await?;
    let manga = parse_manga(&html, manga_path);
    memo.insert(manga_path, manga.clone());
    Ok(manga)
}

/// `_manga_info` on the manga page: metadata, then `_chapters` and
/// `_volumes` keyed by list language and number.
fn parse_manga(html: &str, manga_path: &str) -> Value {
    let mid = manga_path.rsplit_once('-').map_or("", |(_, mid)| mid);
    let mut e = Scan::new(html);
    let url = e.extr("property=\"og:url\" content=\"", "\"");
    let slug = url.rsplit('/').next().unwrap_or_default();
    let slug = slug.rsplit_once('-').map_or("", |(slug, _)| slug);
    let mut manga = json!({
        "manga_url": url,
        "manga_slug": slug,
        "manga_id": text::parse_int(mid),
        "manga": unescape(e.extr("class=\"manga-name\">", "<")),
        "manga_alt": unescape(e.extr("class=\"manga-name-or\">", "<")),
        "tags": split_html(e.extr("class=\"genres\">", "</div>")),
        "type": remove_html(e.extr(">Type:", "</div>")),
        "status": remove_html(e.extr(">Status:", "</div>")),
        "author": split_html(e.extr(">Authors:", "</div>"))
            .into_iter()
            .step_by(2)
            .collect::<Vec<_>>(),
        "published": remove_html(e.extr(">Published:", "</div>")),
        "score": text::parse_float(&remove_html(e.extr(">Score:", "</div>"))),
        "views": text::parse_int(&remove_html(e.extr(">Views:", "</div>")).replace(',', "")),
    });

    let mut chapters = Map::new();
    for group in extract_iter(
        e.extr("class=\"chapters-list-ul\">", "    </div>"),
        "<ul",
        "</ul>",
    ) {
        let key = extr(group, " id=\"", "-chapters\"");
        let lang = text::partition(key, "-").0;
        let mut current = Map::new();
        for ch in extract_iter(group, "<li ", "</li>") {
            let chap = extr(ch, "data-number=\"", "\"");
            let name = unescape(extr(ch, "class=\"name\">", "<"));
            let (chapter, minor) = text::chapter(chap);
            current.insert(
                chap.to_owned(),
                json!({
                    "title": text::partition(&name, ":").2.trim(),
                    "chapter": chapter,
                    "chapter_minor": minor,
                    "chapter_string": chap,
                    "chapter_url": format!("{ROOT}{}", extr(ch, "href=\"", "\"")),
                    "lang": lang,
                }),
            );
        }
        chapters.insert(key.to_owned(), current.into());
    }

    let mut volumes = Map::new();
    let list = e.extr("class=\"volume-list-ul\">", "</section>");
    for group in list.split("<div class=\"manga_list-wrap").skip(1) {
        let key = extr(group, " id=\"", "-volumes\"");
        let lang = text::partition(key, "-").0;
        let mut current = Map::new();
        for vol in extract_iter(group, "class=\"item\">", "</div>") {
            let voln = extr(vol, "tick-vol\">", "<");
            let voln = voln.rsplit(' ').next().unwrap_or_default();
            current.insert(
                voln.to_owned(),
                json!({
                    "volume": text::parse_int(voln),
                    "volume_cover": extr(vol, " src=\"", "\""),
                    "chapter": 0,
                    "chapter_minor": "",
                    "chapter_string": voln,
                    "chapter_url": format!("{ROOT}{}", extr(vol, "href=\"", "\"")),
                    "lang": lang,
                }),
            );
        }
        volumes.insert(key.to_owned(), current.into());
    }

    let description = unescape(e.extr("class=\"description-modal\">", "</div>"));
    if let Some(map) = manga.as_object_mut() {
        set(map, "_chapters", chapters);
        set(map, "_volumes", volumes);
        set(map, "description", description.trim());
    }
    manga
}

/// The public keywords of `_manga_info` (without `_chapters`/`_volumes`).
fn public(manga: &Value) -> Map<String, Value> {
    manga
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(key, _)| !key.starts_with('_'))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

/// The session's cookies plus upstream's `mr_settings` reader cookie. Its
/// JSON value is not a valid RFC 6265 cookie value, which the cookie jar
/// refuses, so it is sent as an explicit header instead.
fn settings_cookie(http: &Http, url: &Url) -> String {
    let settings = json!({
        "readingMode": "vertical",
        "readingDirection": "rtl",
        "quality": "high",
    });
    match http.cookie_header(url) {
        Some(session) => format!("{session}; mr_settings={settings}"),
        None => format!("mr_settings={settings}"),
    }
}

/// `MangareaderChapterExtractor`: the reader page's ID, the memoized manga
/// info's entry for this chapter or volume, then the AJAX image list.
fn chapter_events(
    http: &Http,
    path: String,
    lang: String,
    kind: String,
    chstr: String,
    memo: Memo,
) -> Events<'_> {
    crate::sites::once(async move {
        let reader = http.api(&format!("{ROOT}/read/{path}/{lang}/{kind}-{chstr}"))?;
        let cookie = settings_cookie(http, &reader);
        let html = page(http, &reader, &[("Cookie", &cookie)]).await?;
        let cid = extr(&html, "data-reading-id=\"", "\"").to_owned();

        let manga = manga_info(http, &path, &memo).await?;
        let info = manga
            .get(format!("_{kind}s"))
            .and_then(|lists| lists.get(&lang))
            .and_then(|list| list.get(&chstr))
            .and_then(Value::as_object)
            .ok_or_else(|| error(format!("MangaReader {kind} not found")))?;
        let mut data = public(&manga);
        data.extend(info.clone());
        set(&mut data, "chapter_id", text::parse_int(&cid));

        let key = if kind == "chapter" { "chap" } else { "vol" };
        let mut url = http.api(&format!("{ROOT}/ajax/image/list/{key}/{cid}"))?;
        url.query_pairs_mut()
            .append_pair("mode", "vertical,")
            .append_pair("quality", "high,")
            .append_pair("hozPageSize", "1,");
        let cookie = settings_cookie(http, &url);
        let headers = [
            ("Cookie", cookie.as_str()),
            ("X-Requested-With", "XMLHttpRequest"),
            ("Sec-Fetch-Dest", "empty"),
            ("Sec-Fetch-Mode", "cors"),
            ("Sec-Fetch-Site", "same-origin"),
        ];
        let list = http
            .json_with(&url, &http.browser_headers(&headers))
            .await?;
        let pages = extract_iter(super::str_of(&list, "html"), "data-url=\"", "\"")
            .into_iter()
            .map(|url| (url.to_owned(), None))
            .collect();
        Ok(crate::sites::events(
            Gallery::chapter(&CHAPTER, data, pages)
                .id(cid)
                .events(&SITE, http),
        ))
    })
}

/// `MangareaderMangaExtractor.chapters`: the chapters of the `lang` option
/// (default `en`), queued with the manga keywords.
async fn manga_events(http: &Http, path: String) -> Result<Events<'_>> {
    let memo = Memo::default();
    let manga = manga_info(http, &path, &memo).await?;
    let lang = http
        .option("lang")
        .filter(|lang| crate::sites::art::truthy(Some(lang)))
        .map_or_else(|| "en".to_owned(), crate::sites::scalar);
    let chapters = manga
        .pointer("/_chapters")
        .and_then(|lists| lists.get(&lang))
        .and_then(Value::as_object)
        .ok_or_else(|| error(format!("MangaReader has no '{lang}' chapters")))?;
    let mut chapters: Vec<&Value> = chapters.values().collect();
    // `MangaExtractor.reverse = True`.
    if !http.option_bool("chapter-reverse", false) {
        chapters.reverse();
    }
    let mut streams: Vec<Events<'_>> = Vec::new();
    for info in chapters {
        let Ok(url) = Url::parse(super::str_of(info, "chapter_url")) else {
            continue;
        };
        let Some(Target::Chapter {
            path,
            lang,
            kind,
            chstr,
        }) = target(&url)
        else {
            continue;
        };
        let mut data = public(&manga);
        data.extend(info.as_object().cloned().unwrap_or_default());
        let memo = memo.clone();
        streams.push(super::queue_with(&SITE, &MANGA, &data, move || {
            chapter_events(http, path, lang, kind, chstr, memo)
        }));
    }
    Ok(futures_util::stream::iter(streams).flatten().boxed())
}

#[cfg(test)]
mod tests {
    use super::super::tests::{archive, path};
    use super::*;
    use crate::sites::testing::{html, http, json as json_reply, keywords as kw, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://mangareader.to/read/one-piece-3/en/chapter-1100.5"),
            Some(Target::Chapter {
                path: "one-piece-3".into(),
                lang: "en".into(),
                kind: "chapter".into(),
                chstr: "1100.5".into()
            })
        );
        assert_eq!(
            t("http://www.mangareader.to/read/one-piece-3/ja/volume-105/x"),
            Some(Target::Chapter {
                path: "one-piece-3".into(),
                lang: "ja".into(),
                kind: "volume".into(),
                chstr: "105".into()
            })
        );
        assert_eq!(
            t("https://mangareader.to/one-piece-3"),
            Some(Target::Manga("one-piece-3".into()))
        );
        assert_eq!(
            t("https://mangareader.to/one-piece-3/?ref=x"),
            Some(Target::Manga("one-piece-3".into()))
        );
        for raw in [
            "https://mangareader.to/read/one-piece-3/en/chapter-x",
            "https://mangareader.to/read/one-piece-3/en/page-1",
            "https://mangareader.to/read/one-piece/en/chapter-1",
            "https://mangareader.to/home",
            "https://mangareader.to/-3",
            "https://mangareader.to/one-piece-3x",
            "https://m.mangareader.to/one-piece-3",
            "https://mangareader.to.evil.test/one-piece-3",
            "https://mangareader.to:8443/one-piece-3",
            "https://u:p@mangareader.to/one-piece-3",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    const MANGA_PAGE: &str = r#"<meta property="og:url" content="https://mangareader.to/one-piece-3">
<h2 class="manga-name">One Piece &amp; Co</h2><div class="manga-name-or">ワンピース</div>
<div class="genres"><a href="/genre/action">Action</a><a href="/genre/adventure">Adventure</a></div>
<div class="item"><span class="item-head">Type:</span> <a href="/type/manga">Manga</a></div>
<div class="item"><span class="item-head">Status:</span> <span class="name">Publishing</span></div>
<div class="item"><span class="item-head">Authors:</span><a href="/author/oda">Oda, Eiichiro</a>, <a>Other</a></div>
<div class="item"><span class="item-head">Published:</span> <span class="name">Jul 22, 1997 to ?</span></div>
<div class="item"><span class="item-head">Score:</span> <span class="name">8.86</span></div>
<div class="item"><span class="item-head">Views:</span> <span class="name">1,234,567</span></div>
<div class="chapters-list-ul">
<ul id="en-chapters" class="ulclear">
<li class="item" data-number="1101"><a href="/read/one-piece-3/en/chapter-1101"><span class="name">Chapter 1101: Bonney&#39;s Journey</span></a></li>
<li class="item" data-number="1100.5"><a href="/read/one-piece-3/en/chapter-1100.5"><span class="name">Chapter 1100.5</span></a></li>
</ul>
<ul id="ja-chapters" class="ulclear">
<li class="item" data-number="1"><a href="/read/one-piece-3/ja/chapter-1"><span class="name">第1話: Romance Dawn</span></a></li>
</ul>
    </div>
<div class="volume-list-ul">
<div class="manga_list-wrap" id="en-volumes">
<div class="item"><a href="/read/one-piece-3/en/volume-105"><img src="https://img.test/v105.jpg"><span class="tick-vol">VOL 105</span></a></div>
</div></section>
<div class="description-modal">  A &amp; B  </div>"#;

    #[test]
    fn parses_manga_info() {
        let manga = parse_manga(MANGA_PAGE, "one-piece-3");
        assert_eq!(manga["manga"], "One Piece & Co");
        assert_eq!(manga["manga_slug"], "one-piece");
        assert_eq!(manga["manga_id"], 3);
        assert_eq!(manga["manga_alt"], "ワンピース");
        assert_eq!(manga["tags"], json!(["Action", "Adventure"]));
        assert_eq!(manga["type"], "Manga");
        assert_eq!(manga["status"], "Publishing");
        assert_eq!(manga["author"], json!(["Oda, Eiichiro", "Other"]));
        assert_eq!(manga["published"], "Jul 22, 1997 to ?");
        assert_eq!(manga["score"], json!(8.86));
        assert_eq!(manga["views"], 1234567);
        assert_eq!(manga["description"], "A & B");
        let ch = &manga["_chapters"]["en"]["1101"];
        assert_eq!(ch["title"], "Bonney's Journey");
        assert_eq!(ch["chapter"], 1101);
        assert_eq!(ch["chapter_minor"], "");
        assert_eq!(
            ch["chapter_url"],
            "https://mangareader.to/read/one-piece-3/en/chapter-1101"
        );
        assert_eq!(manga["_chapters"]["en"]["1100.5"]["chapter_minor"], ".5");
        assert_eq!(manga["_chapters"]["ja"]["1"]["title"], "Romance Dawn");
        let vol = &manga["_volumes"]["en"]["105"];
        assert_eq!(vol["volume"], 105);
        assert_eq!(vol["volume_cover"], "https://img.test/v105.jpg");
        assert_eq!(vol["chapter"], 0);
        assert_eq!(vol["lang"], "en");
    }

    #[tokio::test]
    async fn extracts_chapter_images() {
        let reader = r#"<div id="wrapper" data-reading-id="98765" data-reading-by="chap">"#;
        let list = json!({"status": true, "html": r#"<div class="iv-card" data-url="https://c-1.mreadercdn.com/_v2/0/abc/01.jpg?t=1"></div><div class="iv-card shuffled" data-url="https://c-1.mreadercdn.com/_v2/0/abc/02.jpg?t=1"></div>"#});
        let (root, requests) = serve(vec![html(reader), html(MANGA_PAGE), json_reply(list)]);
        let http = http("MangaReader", root);
        let files = media(extract(
            &http,
            Url::parse("https://mangareader.to/read/one-piece-3/en/chapter-1101").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[1].url.as_str(),
            "https://c-1.mreadercdn.com/_v2/0/abc/02.jpg?t=1"
        );
        let k = kw(&files[1]);
        assert_eq!(k["manga"], "One Piece & Co");
        assert_eq!(k["manga_id"], 3);
        assert_eq!(k["chapter_id"], 98765);
        assert_eq!(k["chapter"], 1101);
        assert_eq!(k["title"], "Bonney's Journey");
        assert_eq!(k["lang"], "en");
        assert_eq!(k["page"], 2);
        assert_eq!(k["count"], 2);
        assert!(!k.contains_key("_chapters"));
        // Windows path sanitization replaces the colon in the chapter title.
        let colon = if cfg!(windows) { "_" } else { ":" };
        assert_eq!(
            path(&files[1]),
            format!(
                "mangareader/One Piece & Co/c1101{colon} Bonney's Journey/One Piece & Co_c1101_002.jpg"
            )
        );
        assert_eq!(
            archive(&files[1]),
            r#"["{manga_id}_{chapter_id}_{page}",["3","98765","2"]]"#
        );
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /read/one-piece-3/en/chapter-1101 "));
        assert!(requests[0].contains(
            r#"mr_settings={"readingMode":"vertical","readingDirection":"rtl","quality":"high"}"#
        ));
        assert!(requests[1].starts_with("GET /one-piece-3 "));
        assert!(requests[2].starts_with(
            "GET /ajax/image/list/chap/98765?mode=vertical%2C&quality=high%2C&hozPageSize=1%2C "
        ));
        assert!(requests[2].contains("x-requested-with: XMLHttpRequest"));
    }

    #[tokio::test]
    async fn extracts_volume_images() {
        let reader = r#"<div id="wrapper" data-reading-id="555">"#;
        let list = json!({"html": r#"<div data-url="https://c-1.mreadercdn.com/v/1.jpg"></div>"#});
        let (root, requests) = serve(vec![html(reader), html(MANGA_PAGE), json_reply(list)]);
        let http = http("MangaReader", root);
        let files = media(extract(
            &http,
            Url::parse("https://mangareader.to/read/one-piece-3/en/volume-105").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let k = kw(&files[0]);
        assert_eq!(k["volume"], 105);
        assert_eq!(k["chapter"], 0);
        assert_eq!(
            path(&files[0]),
            "mangareader/One Piece & Co/v105/One Piece & Co_v105_001.jpg"
        );
        let requests = requests.join().unwrap();
        assert!(requests[2].starts_with("GET /ajax/image/list/vol/555?"));
    }

    #[tokio::test]
    async fn manga_queues_language_chapters_oldest_first() {
        let empty = json!({"html": ""});
        let (root, requests) = serve(vec![
            html(MANGA_PAGE),
            html("<div data-reading-id=\"1\">"),
            json_reply(empty.clone()),
            html("<div data-reading-id=\"2\">"),
            json_reply(empty),
        ]);
        let http = http("MangaReader", root);
        let files = media(extract(
            &http,
            Url::parse("https://mangareader.to/one-piece-3").unwrap(),
        ))
        .await;
        assert!(files.is_empty());
        let requests = requests.join().unwrap();
        // The manga page is fetched once and shared with the chapters.
        assert!(requests[0].starts_with("GET /one-piece-3 "));
        assert!(requests[1].starts_with("GET /read/one-piece-3/en/chapter-1100.5 "));
        assert!(requests[2].starts_with("GET /ajax/image/list/chap/1?"));
        assert!(requests[3].starts_with("GET /read/one-piece-3/en/chapter-1101 "));
    }

    #[tokio::test]
    async fn unknown_chapter_is_an_error() {
        let (root, requests) = serve(vec![html("<div data-reading-id=\"1\">"), html(MANGA_PAGE)]);
        let http = http("MangaReader", root);
        let events = extract(
            &http,
            Url::parse("https://mangareader.to/read/one-piece-3/fr/chapter-1").unwrap(),
        );
        assert!(crate::sites::testing::fails(events).await);
        requests.join().unwrap();
    }
}
