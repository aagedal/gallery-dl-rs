//! MangaFire chapters, volumes and manga, mirroring
//! gallery_dl/extractor/mangafire.py: AJAX chapter and image lists signed
//! with the `vrf` token of utils/mangafire_vrf.py.
//!
//! Like upstream, images are downloaded as served: pages the site
//! scrambles are not descrambled.
use super::{Gallery, Memo, text};
use crate::sites::art::{Scan, extr, extract_iter, split_html};
use crate::sites::{Events, Http, Route, Site, remove_html, unescape};
use crate::{Result, error};
use futures_util::StreamExt;
use serde_json::{Map, Value, json};
use url::Url;

pub static SITE: Site = Site {
    category: "mangafire",
    name: "MangaFire",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CHAPTER, &MANGA],
    api_key: false,
    cookies: None,
};

const ROOT: &str = "https://mangafire.to";

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
    /// `/read/SLUG.ID/LANG/(chapter|volume)-N…`, raw.
    Chapter {
        manga_path: String,
        manga_id: String,
        lang: String,
        info: String,
        kind: String,
    },
    /// `/manga/SLUG.ID`, raw.
    Manga { slug: String, id: String },
}

/// Python `\w` on a raw (percent-encoded) path: ASCII word characters.
fn word(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// `[\w-]+`.
fn slug(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

/// A whole segment matching `([\w-]+)\.(\w+)`.
fn manga_path(segment: &str) -> Option<(&str, &str)> {
    let (name, id) = segment.split_once('.')?;
    (slug(name) && word(id)).then_some((name, id))
}

/// `mangafire\.to/read/([\w-]+\.(\w+))/([\w-]+)/((chapter|volume)-\d+(?:\D.*)?)`
/// and `/manga/([\w-]+)\.(\w+)`. The chapter part is the rest of the path;
/// a query or fragment is ignored.
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::art::on_host(url, &["mangafire.to"]) {
        return None;
    }
    let raw: Vec<&str> = url.path().split('/').skip(1).collect();
    match raw.as_slice() {
        ["read", path, lang, rest @ ..] if !rest.is_empty() && slug(lang) => {
            let (_, manga_id) = manga_path(path)?;
            let info = rest.join("/");
            let (kind, number) = info.split_once('-')?;
            // `\d+` is followed by the end or a non-digit by construction.
            crate::sites::art::leading_digits(number)?;
            matches!(kind, "chapter" | "volume").then(|| Target::Chapter {
                manga_path: (*path).to_owned(),
                manga_id: manga_id.to_owned(),
                lang: (*lang).to_owned(),
                kind: kind.to_owned(),
                info: info.clone(),
            })
        }
        ["manga", path, ..] => manga_path(path).map(|(slug, id)| Target::Manga {
            slug: slug.to_owned(),
            id: id.to_owned(),
        }),
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Chapter {
            manga_path,
            manga_id,
            lang,
            info,
            kind,
        }) => chapter_events(
            http,
            Chapter {
                manga_path,
                manga_id,
                lang,
                info,
                kind,
            },
            Memo::default(),
        ),
        Some(Target::Manga { slug, id }) => {
            crate::sites::once(async move { manga_events(http, slug, id).await })
        }
        None => crate::sites::fail(error("unsupported MangaFire URL")),
    }
}

/// An AJAX request signed with `vrf`.
async fn ajax(http: &Http, path: &str, vrf_input: &str) -> Result<Value> {
    let mut url = http.api(&format!("{ROOT}{path}"))?;
    url.query_pairs_mut()
        .append_pair("vrf", &vrf::generate(vrf_input));
    http.json_with(
        &url,
        &http.browser_headers(&[("x-requested-with", "XMLHttpRequest")]),
    )
    .await
}

/// `_manga_info`, memoized by manga path.
async fn manga_info(http: &Http, manga_path: &str, memo: &Memo) -> Result<Value> {
    let key = format!("info:{manga_path}");
    if let Some(manga) = memo.get(&key) {
        return Ok(manga);
    }
    let url = http.api(&format!("{ROOT}/manga/{manga_path}"))?;
    let page = http.text_with(&url, &http.browser_headers(&[])).await?;
    let manga = Value::Object(parse_manga(&page, manga_path));
    memo.insert(key, manga.clone());
    Ok(manga)
}

/// `_manga_info` on the manga page.
fn parse_manga(page: &str, manga_path: &str) -> Map<String, Value> {
    let (slug, mid) = manga_path.rsplit_once('.').unwrap_or(("", manga_path));
    let mut e = Scan::new(page);
    let split = |text: String| -> Value {
        let list: Vec<&str> = text.split(" , ").collect();
        // A single empty name is `()`.
        if list == [""] { json!([]) } else { json!(list) }
    };
    let cover = extr(e.extr("class=\"poster\">", "</div>"), "src=\"", "\"");
    let status = text::title(&e.extr("<p>", "<").replace('_', " "));
    let manga = unescape(e.extr("itemprop=\"name\">", "<"));
    let titles = unescape(e.extr("<h6>", "<"));
    let kind = remove_html(e.extr("class=\"min-info\">", "</a>"));
    let author = unescape(&remove_html(e.extr("<span>Author:</span>", "</div>")));
    let published = remove_html(e.extr("<span>Published:</span>", "</div>"));
    let tags: Vec<String> = split_html(e.extr("<span>Genres:</span>", "</div>"))
        .into_iter()
        .step_by(2)
        .collect();
    let publisher = unescape(&remove_html(e.extr("<span>Mangazines:</span>", "</div>")));
    let score = text::parse_float(&remove_html(e.extr("class=\"score\">", " / ")));
    let description = remove_html(e.extr("id=\"synopsis\">", "<script>"));
    super::keywords([
        ("cover", cover.into()),
        ("status", status.into()),
        ("manga", manga.into()),
        ("manga_id", mid.into()),
        ("manga_slug", slug.into()),
        (
            "manga_titles",
            json!(titles.split("; ").collect::<Vec<_>>()),
        ),
        ("type", kind.into()),
        ("author", split(author)),
        ("published", published.into()),
        ("tags", tags.into()),
        ("publisher", split(publisher)),
        ("score", score),
        ("description", description.into()),
    ])
}

/// `_manga_chapters`: reader anchors keyed by the chapter part of their
/// link, memoized by `(manga_id, type, lang)`.
async fn manga_chapters(
    http: &Http,
    manga_id: &str,
    kind: &str,
    lang: &str,
    memo: &Memo,
) -> Result<Map<String, Value>> {
    let key = format!("chapters:{manga_id}/{kind}/{lang}");
    if let Some(Value::Object(chapters)) = memo.get(&key) {
        return Ok(chapters);
    }
    let data = ajax(
        http,
        &format!("/ajax/read/{manga_id}/{kind}/{lang}"),
        &format!("{manga_id}@{kind}@{lang}"),
    )
    .await?;
    let html = data
        .pointer("/result/html")
        .and_then(Value::as_str)
        .ok_or_else(|| error("MangaFire returned no chapter list"))?;
    let needle = format!("{manga_id}/{lang}/");
    let chapters: Map<String, Value> = extract_iter(html, "<a ", ">")
        .into_iter()
        .map(|anchor| (extr(anchor, &needle, "\"").to_owned(), anchor.into()))
        .collect();
    memo.insert(key, Value::Object(chapters.clone()));
    Ok(chapters)
}

/// `_chapter_info` of a reader anchor.
fn chapter_info(anchor: &str) -> Option<Map<String, Value>> {
    let href = extr(anchor, "href=\"", "\"");
    let mut parts = href.rsplitn(3, '/');
    let info = parts.next()?;
    let lang = parts.next()?;
    parts.next()?;
    let number = extr(anchor, "data-number=\"", "\"");
    let id = text::parse_int(extr(anchor, "data-id=\"", "\""));
    let title = unescape(extr(anchor, "title=\"", "\""));
    if info.starts_with("vol") {
        return Some(super::keywords([
            ("volume", text::parse_int(number).into()),
            ("volume_id", id.into()),
            ("chapter", 0.into()),
            ("chapter_minor", "".into()),
            ("chapter_string", info.into()),
            ("chapter_id", id.into()),
            ("title", title.into()),
            ("lang", lang.into()),
        ]));
    }
    let (chapter, minor) = text::chapter(number);
    Some(super::keywords([
        ("chapter", chapter.into()),
        ("chapter_minor", minor.into()),
        ("chapter_string", info.into()),
        ("chapter_id", id.into()),
        ("title", title.into()),
        ("lang", lang.into()),
    ]))
}

/// The groups of a chapter URL.
struct Chapter {
    manga_path: String,
    manga_id: String,
    lang: String,
    info: String,
    kind: String,
}

/// `MangafireChapterExtractor`: the anchor for this chapter from the
/// (memoized) chapter list, the manga info, then the image list.
fn chapter_events(http: &Http, chapter: Chapter, memo: Memo) -> Events<'_> {
    crate::sites::once(async move {
        let Chapter {
            manga_path,
            manga_id,
            lang,
            info,
            kind,
        } = chapter;
        let chapters = manga_chapters(http, &manga_id, &kind, &lang, &memo).await?;
        let not_found = || error("MangaFire chapter not found");
        let anchor = chapters
            .get(&info)
            .and_then(Value::as_str)
            .ok_or_else(not_found)?;
        let chapter_id = extr(anchor, "data-id=\"", "\"").to_owned();
        let manga = manga_info(http, &manga_path, &memo).await?;
        let mut data = manga.as_object().cloned().unwrap_or_default();
        data.extend(chapter_info(anchor).ok_or_else(not_found)?);

        let list = ajax(
            http,
            &format!("/ajax/read/{kind}/{chapter_id}"),
            &format!("{kind}@{chapter_id}"),
        )
        .await?;
        let pages = list
            .pointer("/result/images")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|image| Some((image.get(0)?.as_str()?.to_owned(), None)))
            .collect();
        Ok(crate::sites::events(
            Gallery::chapter(&CHAPTER, data, pages)
                .id(chapter_id)
                .events(&SITE, http),
        ))
    })
}

/// `MangafireMangaExtractor.chapters`: the `chapter` list in the `lang`
/// option's language (default `en`), queued with the manga keywords.
async fn manga_events(http: &Http, slug: String, id: String) -> Result<Events<'_>> {
    let lang = http
        .option("lang")
        .filter(|lang| crate::sites::art::truthy(Some(lang)))
        .map_or_else(|| "en".to_owned(), crate::sites::scalar);
    let memo = Memo::default();
    let manga = manga_info(http, &format!("{slug}.{id}"), &memo).await?;
    let chapters = manga_chapters(http, &id, "chapter", &lang, &memo).await?;
    let mut chapters: Vec<&str> = chapters.values().filter_map(Value::as_str).collect();
    // `MangaExtractor.reverse = True`.
    if !http.option_bool("chapter-reverse", false) {
        chapters.reverse();
    }
    let mut streams: Vec<Events<'_>> = Vec::new();
    for anchor in chapters {
        let Ok(url) = Url::parse(&format!("{ROOT}{}", extr(anchor, "href=\"", "\""))) else {
            continue;
        };
        let Some(Target::Chapter {
            manga_path,
            manga_id,
            lang,
            info,
            kind,
        }) = target(&url)
        else {
            continue;
        };
        let mut data = manga.as_object().cloned().unwrap_or_default();
        data.extend(chapter_info(anchor).unwrap_or_default());
        let chapter = Chapter {
            manga_path,
            manga_id,
            lang,
            info,
            kind,
        };
        let memo = memo.clone();
        streams.push(super::queue_with(&SITE, &MANGA, &data, move || {
            chapter_events(http, chapter, memo)
        }));
    }
    Ok(futures_util::stream::iter(streams).flatten().boxed())
}

/// utils/mangafire_vrf.py (adapted upstream from FMD2): five rounds of RC4
/// and a keyed byte schedule over the quoted input, URL-safe base64.
mod vrf {
    use base64::Engine;
    use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};

    #[derive(Clone, Copy)]
    enum Op {
        Add(u8),
        Sub(u8),
        Rotl(u32),
        Rotr(u32),
    }
    use Op::{Add, Rotl, Rotr, Sub};

    impl Op {
        fn apply(self, c: u8) -> u8 {
            match self {
                Add(n) => c.wrapping_add(n),
                Sub(n) => c.wrapping_sub(n),
                Rotl(n) => c.rotate_left(n),
                Rotr(n) => c.rotate_right(n),
            }
        }
    }

    /// `(key, seed, prefix, schedule)` per round.
    const ROUNDS: [(&str, &str, &str, [Op; 10]); 5] = [
        (
            "FgxyJUQDPUGSzwbAq/ToWn4/e8jYzvabE+dLMb1XU1o=",
            "yH6MXnMEcDVWO/9a6P9W92BAh1eRLVFxFlWTHUqQ474=",
            "l9PavRg=",
            [
                Sub(223),
                Rotr(4),
                Rotr(4),
                Add(234),
                Rotr(7),
                Rotr(2),
                Rotr(7),
                Sub(223),
                Rotr(7),
                Rotr(6),
            ],
        ),
        (
            "CQx3CLwswJAnM1VxOqX+y+f3eUns03ulxv8Z+0gUyik=",
            "RK7y4dZ0azs9Uqz+bbFB46Bx2K9EHg74ndxknY9uknA=",
            "Ml2v7ag1Jg==",
            [
                Add(19),
                Rotr(7),
                Add(19),
                Rotr(6),
                Add(19),
                Rotr(1),
                Add(19),
                Rotr(6),
                Rotr(7),
                Rotr(4),
            ],
        ),
        (
            "fAS+otFLkKsKAJzu3yU+rGOlbbFVq+u+LaS6+s1eCJs=",
            "rqr9HeTQOg8TlFiIGZpJaxcvAaKHwMwrkqojJCpcvoc=",
            "i/Va0UxrbMo=",
            [
                Sub(223),
                Rotr(1),
                Add(19),
                Sub(223),
                Rotl(2),
                Sub(223),
                Add(19),
                Rotl(1),
                Rotl(2),
                Rotl(1),
            ],
        ),
        (
            "Oy45fQVK9kq9019+VysXVlz1F9S1YwYKgXyzGlZrijo=",
            "/4GPpmZXYpn5RpkP7FC/dt8SXz7W30nUZTe8wb+3xmU=",
            "WFjKAHGEkQM=",
            [
                Add(19),
                Rotl(1),
                Rotl(1),
                Rotr(1),
                Add(234),
                Rotl(1),
                Sub(223),
                Rotl(6),
                Rotl(4),
                Rotl(1),
            ],
        ),
        (
            "aoDIdXezm2l3HrcnQdkPJTDT8+W6mcl2/02ewBHfPzg=",
            "wsSGSBXKWA9q1oDJpjtJddVxH+evCfL5SO9HZnUDFU8=",
            "5Rr27rWd",
            [
                Rotr(1),
                Rotl(1),
                Rotl(6),
                Rotr(1),
                Rotl(2),
                Rotr(4),
                Rotl(1),
                Rotl(1),
                Sub(223),
                Rotl(2),
            ],
        ),
    ];

    /// `generate`.
    pub fn generate(input: &str) -> String {
        let mut data = super::text::quote(input).into_bytes();
        for (key, seed, prefix, schedule) in ROUNDS {
            let decode = |b64: &str| STANDARD.decode(b64).unwrap_or_default();
            data = transform(
                &rc4(&decode(key), &data),
                &decode(seed),
                &decode(prefix),
                &schedule,
            );
        }
        URL_SAFE_NO_PAD.encode(data)
    }

    fn transform(input: &[u8], seed: &[u8], prefix: &[u8], schedule: &[Op; 10]) -> Vec<u8> {
        let mut out = Vec::with_capacity(input.len() + prefix.len());
        for (idx, c) in input.iter().enumerate() {
            if let Some(byte) = prefix.get(idx) {
                out.push(*byte);
            }
            out.push(schedule[idx % 10].apply(c ^ seed[idx % 32]));
        }
        out
    }

    fn rc4(key: &[u8], input: &[u8]) -> Vec<u8> {
        let mut s: Vec<u8> = (0..=255).collect();
        let mut j = 0u8;
        for i in 0..256 {
            j = j.wrapping_add(s[i]).wrapping_add(key[i % key.len()]);
            s.swap(i, usize::from(j));
        }
        let (mut i, mut j) = (0u8, 0u8);
        input
            .iter()
            .map(|c| {
                i = i.wrapping_add(1);
                j = j.wrapping_add(s[usize::from(i)]);
                s.swap(usize::from(i), usize::from(j));
                c ^ s[usize::from(s[usize::from(i)].wrapping_add(s[usize::from(j)]))]
            })
            .collect()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn rc4_matches_the_reference_vector() {
            // RFC 6229-style check: RC4("Key", "Plaintext").
            assert_eq!(
                rc4(b"Key", b"Plaintext"),
                [0xBB, 0xF3, 0x16, 0xE8, 0xD9, 0x40, 0xAF, 0x0A, 0xD3]
            );
        }

        #[test]
        fn generates_url_safe_tokens() {
            let token = generate("dkw@chapter@en");
            assert!(!token.contains(['+', '/', '=']));
            // Each round adds its prefix to the 18 bytes of
            // `dkw%40chapter%40en`.
            let prefixes = 5 + 7 + 8 + 8 + 6;
            assert_eq!(URL_SAFE_NO_PAD.decode(&token).unwrap().len(), 18 + prefixes);
            assert_eq!(token, generate("dkw@chapter@en"));
        }
    }
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
            t("https://mangafire.to/read/one-piecee.dkw/en/chapter-1100.5"),
            Some(Target::Chapter {
                manga_path: "one-piecee.dkw".into(),
                manga_id: "dkw".into(),
                lang: "en".into(),
                info: "chapter-1100.5".into(),
                kind: "chapter".into()
            })
        );
        assert_eq!(
            t("http://www.mangafire.to/read/x_y.ab1/pt-br/volume-3"),
            Some(Target::Chapter {
                manga_path: "x_y.ab1".into(),
                manga_id: "ab1".into(),
                lang: "pt-br".into(),
                info: "volume-3".into(),
                kind: "volume".into()
            })
        );
        assert_eq!(
            t("https://mangafire.to/manga/one-piecee.dkw"),
            Some(Target::Manga {
                slug: "one-piecee".into(),
                id: "dkw".into()
            })
        );
        for raw in [
            "https://mangafire.to/read/one-piecee.dkw/en/page-1",
            "https://mangafire.to/read/one-piecee.dkw/en/chapter-x",
            "https://mangafire.to/read/one-piecee/en/chapter-1",
            "https://mangafire.to/manga/one-piecee",
            "https://mangafire.to/manga/.dkw",
            "https://mangafire.to/home",
            "https://m.mangafire.to/manga/one-piecee.dkw",
            "https://mangafire.to.evil.test/manga/one-piecee.dkw",
            "https://mangafire.to:8443/manga/one-piecee.dkw",
            "https://u:p@mangafire.to/manga/one-piecee.dkw",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    const MANGA_PAGE: &str = r#"<div class="poster"><div><img src="https://static.mfcdn.cc/x.jpg" alt=""></div></div>
<p>on_hiatus</p><h1 itemprop="name">One Piece &amp; Co</h1><h6>ワンピース; One Piece</h6>
<div class="min-info"><a href="/type/manga">Manga</a>
<div><span>Author:</span> <span><a href="/author/oda">Oda Eiichiro</a></span></div>
<div><span>Published:</span> <span>Jul 22, 1997 to ?</span></div>
<div><span>Genres:</span> <span><a href="/genre/action">Action</a>, <a href="/genre/comedy">Comedy</a></span></div>
<div><span>Mangazines:</span> <span></span></div>
<div class="score">9.12 / 10</div>
<div id="synopsis"><p>Gol D. <b>Roger</b></p></div><script>"#;

    fn chapter_list() -> Value {
        json!({"status": 200, "result": {"html": r#"<ul>
<li><a href="/read/one-piecee.dkw/en/chapter-1101" data-number="1101" data-id="4001" title="Vol 109 Chapter 1101: Bonney&#39;s Journey"><span>Chapter 1101</span></a></li>
<li><a href="/read/one-piecee.dkw/en/chapter-1100.5" data-number="1100.5" data-id="4000" title=""><span>Chapter 1100.5</span></a></li>
</ul>"#}})
    }

    #[test]
    fn parses_manga_info() {
        let manga = parse_manga(MANGA_PAGE, "one-piecee.dkw");
        assert_eq!(manga["cover"], "https://static.mfcdn.cc/x.jpg");
        assert_eq!(manga["status"], "On Hiatus");
        assert_eq!(manga["manga"], "One Piece & Co");
        assert_eq!(manga["manga_id"], "dkw");
        assert_eq!(manga["manga_slug"], "one-piecee");
        assert_eq!(manga["manga_titles"], json!(["ワンピース", "One Piece"]));
        assert_eq!(manga["type"], "Manga");
        assert_eq!(manga["author"], json!(["Oda Eiichiro"]));
        assert_eq!(manga["published"], "Jul 22, 1997 to ?");
        assert_eq!(manga["tags"], json!(["Action", "Comedy"]));
        assert_eq!(manga["publisher"], json!([]));
        assert_eq!(manga["score"], json!(9.12));
        assert_eq!(manga["description"], "Gol D. Roger");
    }

    #[test]
    fn parses_volume_anchors() {
        let anchor = r#"a href="/read/one-piecee.dkw/en/volume-105" data-number="105" data-id="77" title="Vol 105""#;
        let info = chapter_info(anchor).unwrap();
        assert_eq!(info["volume"], 105);
        assert_eq!(info["volume_id"], 77);
        assert_eq!(info["chapter_id"], 77);
        assert_eq!(info["chapter"], 0);
        assert_eq!(info["chapter_string"], "volume-105");
        assert_eq!(info["lang"], "en");
        assert!(chapter_info(r#"a href="volume-1""#).is_none());
    }

    #[tokio::test]
    async fn extracts_chapter_images() {
        let images = json!({"status": 200, "result": {"images": [
            ["https://s1.mfcdn.cc/ab/01.jpg", 1, 0],
            ["https://s1.mfcdn.cc/ab/02.jpg", 1, 7]
        ]}});
        let (root, requests) = serve(vec![
            json_reply(chapter_list()),
            html(MANGA_PAGE),
            json_reply(images),
        ]);
        let http = http("MangaFire", root);
        let files = media(extract(
            &http,
            Url::parse("https://mangafire.to/read/one-piecee.dkw/en/chapter-1101").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(files[1].url.as_str(), "https://s1.mfcdn.cc/ab/02.jpg");
        let k = kw(&files[1]);
        assert_eq!(k["manga"], "One Piece & Co");
        assert_eq!(k["manga_id"], "dkw");
        assert_eq!(k["chapter"], 1101);
        assert_eq!(k["chapter_minor"], "");
        assert_eq!(k["chapter_id"], 4001);
        assert_eq!(k["chapter_string"], "chapter-1101");
        assert_eq!(k["title"], "Vol 109 Chapter 1101: Bonney's Journey");
        assert_eq!(k["lang"], "en");
        assert_eq!(k["page"], 2);
        assert_eq!(k["count"], 2);
        // Windows path sanitization replaces the colon in the chapter title.
        let colon = if cfg!(windows) { "_" } else { ":" };
        assert_eq!(
            path(&files[1]),
            format!(
                "mangafire/One Piece & Co/c1101{colon} Vol 109 Chapter 1101{colon} Bonney's Journey/One Piece & Co_c1101_002.jpg"
            )
        );
        assert_eq!(
            archive(&files[1]),
            r#"["{manga_id}_{chapter_id}_{page}",["dkw","4001","2"]]"#
        );
        let requests = requests.join().unwrap();
        let vrf = |input: &str| vrf::generate(input);
        assert!(requests[0].starts_with(&format!(
            "GET /ajax/read/dkw/chapter/en?vrf={} ",
            vrf("dkw@chapter@en")
        )));
        assert!(requests[0].contains("x-requested-with: XMLHttpRequest"));
        assert!(requests[1].starts_with("GET /manga/one-piecee.dkw "));
        assert!(requests[2].starts_with(&format!(
            "GET /ajax/read/chapter/4001?vrf={} ",
            vrf("chapter@4001")
        )));
    }

    #[tokio::test]
    async fn unknown_chapter_is_an_error() {
        let (root, requests) = serve(vec![json_reply(chapter_list())]);
        let http = http("MangaFire", root);
        let events = extract(
            &http,
            Url::parse("https://mangafire.to/read/one-piecee.dkw/en/chapter-9").unwrap(),
        );
        assert!(crate::sites::testing::fails(events).await);
        requests.join().unwrap();
    }

    #[tokio::test]
    async fn manga_queues_chapters_oldest_first_sharing_lookups() {
        let empty = json!({"result": {"images": []}});
        let (root, requests) = serve(vec![
            html(MANGA_PAGE),
            json_reply(chapter_list()),
            json_reply(empty.clone()),
            json_reply(empty),
        ]);
        let http = http("MangaFire", root);
        let files = media(extract(
            &http,
            Url::parse("https://mangafire.to/manga/one-piecee.dkw").unwrap(),
        ))
        .await;
        assert!(files.is_empty());
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /manga/one-piecee.dkw "));
        assert!(requests[1].starts_with("GET /ajax/read/dkw/chapter/en?vrf="));
        assert!(requests[2].starts_with("GET /ajax/read/chapter/4000?vrf="));
        assert!(requests[3].starts_with("GET /ajax/read/chapter/4001?vrf="));
    }
}
