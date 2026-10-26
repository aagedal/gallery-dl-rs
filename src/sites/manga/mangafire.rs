//! MangaFire chapters, volumes and manga, mirroring
//! gallery_dl/extractor/mangafire.py and its `vrf` request signatures.
use super::{Gallery, set};
use crate::error;
use crate::sites::gdl::{
    Scan, captures, extr, extract_iter, group, parse_float, parse_int, partition, quote, re,
    remove_html, rpartition, split_html, unescape,
};
use crate::sites::{Events, Http, Route, Site};
use base64::Engine;
use serde_json::{Map, Value};
use url::Url;

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
static MANGA: Route = super::manga_route();

pub static SITE: Site = Site {
    category: "mangafire",
    name: "MangaFire",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CHAPTER, &MANGA],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Chapter {
        path: String,
        id: String,
        lang: String,
        info: String,
        kind: String,
    },
    Manga(String, String),
}

fn target(url: &Url) -> Option<Target> {
    let chapter = re!(
        r"^https?://(?:www\.)?mangafire\.to/read/([\w-]+\.(\w+))/([\w-]+)/((chapter|volume)-\d+(?:\D.*)?)"
    );
    if let Some(c) = captures(chapter, url) {
        return Some(Target::Chapter {
            path: group(&c, 1).into(),
            id: group(&c, 2).into(),
            lang: group(&c, 3).into(),
            info: group(&c, 4).into(),
            kind: group(&c, 5).into(),
        });
    }
    let manga = re!(r"^https?://(?:www\.)?mangafire\.to/manga/([\w-]+)\.(\w+)");
    captures(manga, url).map(|c| Target::Manga(group(&c, 1).into(), group(&c, 2).into()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported MangaFire URL"));
    };
    crate::sites::once(async move {
        Ok(match target {
            Target::Chapter {
                path,
                id,
                lang,
                info,
                kind,
            } => {
                let chapters = manga_chapters(http, &id, &kind, &lang).await?;
                let anchor = chapters
                    .iter()
                    .find(|(key, _)| *key == info)
                    .map(|(_, anchor)| anchor.clone())
                    .ok_or_else(|| error("MangaFire chapter not found"))?;
                let chapter_id = extr(&anchor, "data-id=\"", "\"").to_owned();
                let mut data = manga_info(
                    &super::fetch(http, &format!("{ROOT}/manga/{path}")).await?,
                    &path,
                );
                data.extend(chapter_info(&anchor));
                let mut url = http.api(&format!("{ROOT}/ajax/read/{kind}/{chapter_id}"))?;
                url.query_pairs_mut()
                    .append_pair("vrf", &vrf(&format!("{kind}@{chapter_id}")));
                let json = http
                    .json_with(&url, &[("x-requested-with", "XMLHttpRequest")])
                    .await?;
                let pages = json["result"]["images"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|image| (crate::sites::scalar(&image[0]), None))
                    .collect();
                crate::sites::events(Gallery::chapter(&CHAPTER, data, pages).events(&SITE, http))
            }
            Target::Manga(slug, id) => {
                let lang = http
                    .option_str("lang")
                    .filter(|l| !l.is_empty())
                    .unwrap_or("en")
                    .to_owned();
                let path = format!("{slug}.{id}");
                let manga = manga_info(
                    &super::fetch(http, &format!("{ROOT}/manga/{path}")).await?,
                    &path,
                );
                let chapters = manga_chapters(http, &id, "chapter", &lang)
                    .await?
                    .into_iter()
                    .filter_map(|(_, anchor)| {
                        let url = Url::parse(&format!("{ROOT}{}", extr(&anchor, "href=\"", "\"")))
                            .ok()?;
                        let mut data = manga.clone();
                        data.extend(chapter_info(&anchor));
                        Some((url, data))
                    })
                    .collect();
                super::queue_chapters(http, &SITE, &MANGA, chapters, true)
            }
        })
    })
}

/// `_manga_info`.
fn manga_info(page: &str, path: &str) -> Map<String, Value> {
    let (slug, _, mid) = rpartition(path, ".");
    let mut scan = Scan::new(page);
    let split = |text: String| -> Vec<String> {
        let parts: Vec<String> = text.split(" , ").map(str::to_owned).collect();
        if parts.len() == 1 && parts[0].is_empty() {
            Vec::new()
        } else {
            parts
        }
    };
    let mut data = Map::new();
    set(
        &mut data,
        "cover",
        extr(scan.extr("class=\"poster\">", "</div>"), "src=\"", "\""),
    );
    set(
        &mut data,
        "status",
        title_case(&scan.extr("<p>", "<").replace('_', " ")),
    );
    set(
        &mut data,
        "manga",
        unescape(scan.extr("itemprop=\"name\">", "<")),
    );
    set(&mut data, "manga_id", mid);
    set(&mut data, "manga_slug", slug);
    let titles: Vec<String> = unescape(scan.extr("<h6>", "<"))
        .split("; ")
        .map(str::to_owned)
        .collect();
    set(&mut data, "manga_titles", titles);
    set(
        &mut data,
        "type",
        remove_html(scan.extr("class=\"min-info\">", "</a>")),
    );
    set(
        &mut data,
        "author",
        split(unescape(&remove_html(
            scan.extr("<span>Author:</span>", "</div>"),
        ))),
    );
    set(
        &mut data,
        "published",
        remove_html(scan.extr("<span>Published:</span>", "</div>")),
    );
    let tags: Vec<String> = split_html(scan.extr("<span>Genres:</span>", "</div>"))
        .into_iter()
        .step_by(2)
        .collect();
    set(&mut data, "tags", tags);
    set(
        &mut data,
        "publisher",
        split(unescape(&remove_html(
            scan.extr("<span>Mangazines:</span>", "</div>"),
        ))),
    );
    set(
        &mut data,
        "score",
        parse_float(&remove_html(scan.extr("class=\"score\">", " / "))),
    );
    set(
        &mut data,
        "description",
        remove_html(scan.extr("id=\"synopsis\">", "<script>")),
    );
    data
}

/// Python `str.title()`.
fn title_case(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut previous_cased = false;
    for c in text.chars() {
        if previous_cased {
            out.extend(c.to_lowercase());
        } else {
            out.extend(c.to_uppercase());
        }
        previous_cased = c.is_alphabetic();
    }
    out
}

/// `_manga_chapters`: chapter anchors by their `chapter-N` path part.
async fn manga_chapters(
    http: &Http,
    id: &str,
    kind: &str,
    lang: &str,
) -> crate::Result<Vec<(String, String)>> {
    let mut url = http.api(&format!("{ROOT}/ajax/read/{id}/{kind}/{lang}"))?;
    url.query_pairs_mut()
        .append_pair("vrf", &vrf(&format!("{id}@{kind}@{lang}")));
    let data = http
        .json_with(&url, &[("x-requested-with", "XMLHttpRequest")])
        .await?;
    let html = crate::sites::gdl::str_of(&data["result"], "html");
    let needle = format!("{id}/{lang}/");
    let mut anchors: Vec<(String, String)> = Vec::new();
    for anchor in extract_iter(html, "<a ", ">") {
        let key = extr(anchor, &needle, "\"").to_owned();
        match anchors.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = anchor.to_owned(),
            None => anchors.push((key, anchor.to_owned())),
        }
    }
    Ok(anchors)
}

/// `_chapter_info`.
fn chapter_info(info: &str) -> Map<String, Value> {
    let href = extr(info, "href=\"", "\"");
    let mut parts = href.rsplitn(3, '/');
    let chapter_info = parts.next().unwrap_or_default();
    let lang = parts.next().unwrap_or_default();
    let title = unescape(extr(info, "title=\"", "\""));
    let mut data = Map::new();
    if chapter_info.starts_with("vol") {
        let volume_id = parse_int(extr(info, "data-id=\"", "\""));
        set(
            &mut data,
            "volume",
            parse_int(extr(info, "data-number=\"", "\"")),
        );
        set(&mut data, "volume_id", volume_id);
        set(&mut data, "chapter", 0);
        set(&mut data, "chapter_minor", "");
        set(&mut data, "chapter_string", chapter_info);
        set(&mut data, "chapter_id", volume_id);
    } else {
        let (chapter, sep, minor) = partition(extr(info, "data-number=\"", "\""), ".");
        set(&mut data, "chapter", parse_int(chapter));
        set(&mut data, "chapter_minor", format!("{sep}{minor}"));
        set(&mut data, "chapter_string", chapter_info);
        set(
            &mut data,
            "chapter_id",
            parse_int(extr(info, "data-id=\"", "\"")),
        );
    }
    set(&mut data, "title", title);
    set(&mut data, "lang", lang);
    data
}

type Step = fn(u8) -> u8;

macro_rules! step {
    (add $n:literal) => {
        |c: u8| c.wrapping_add($n)
    };
    (sub $n:literal) => {
        |c: u8| c.wrapping_sub($n)
    };
    (rotl $n:literal) => {
        |c: u8| c.rotate_left($n)
    };
    (rotr $n:literal) => {
        |c: u8| c.rotate_right($n)
    };
}

const SCHEDULE_C: [Step; 10] = [
    step!(sub 223),
    step!(rotr 4),
    step!(rotr 4),
    step!(add 234),
    step!(rotr 7),
    step!(rotr 2),
    step!(rotr 7),
    step!(sub 223),
    step!(rotr 7),
    step!(rotr 6),
];
const SCHEDULE_Y: [Step; 10] = [
    step!(add 19),
    step!(rotr 7),
    step!(add 19),
    step!(rotr 6),
    step!(add 19),
    step!(rotr 1),
    step!(add 19),
    step!(rotr 6),
    step!(rotr 7),
    step!(rotr 4),
];
const SCHEDULE_B: [Step; 10] = [
    step!(sub 223),
    step!(rotr 1),
    step!(add 19),
    step!(sub 223),
    step!(rotl 2),
    step!(sub 223),
    step!(add 19),
    step!(rotl 1),
    step!(rotl 2),
    step!(rotl 1),
];
const SCHEDULE_J: [Step; 10] = [
    step!(add 19),
    step!(rotl 1),
    step!(rotl 1),
    step!(rotr 1),
    step!(add 234),
    step!(rotl 1),
    step!(sub 223),
    step!(rotl 6),
    step!(rotl 4),
    step!(rotl 1),
];
const SCHEDULE_E: [Step; 10] = [
    step!(rotr 1),
    step!(rotl 1),
    step!(rotl 6),
    step!(rotr 1),
    step!(rotl 2),
    step!(rotr 4),
    step!(rotl 1),
    step!(rotl 1),
    step!(sub 223),
    step!(rotl 2),
];

const ROUNDS: [(&str, &str, &str, &[Step; 10]); 5] = [
    (
        "FgxyJUQDPUGSzwbAq/ToWn4/e8jYzvabE+dLMb1XU1o=",
        "yH6MXnMEcDVWO/9a6P9W92BAh1eRLVFxFlWTHUqQ474=",
        "l9PavRg=",
        &SCHEDULE_C,
    ),
    (
        "CQx3CLwswJAnM1VxOqX+y+f3eUns03ulxv8Z+0gUyik=",
        "RK7y4dZ0azs9Uqz+bbFB46Bx2K9EHg74ndxknY9uknA=",
        "Ml2v7ag1Jg==",
        &SCHEDULE_Y,
    ),
    (
        "fAS+otFLkKsKAJzu3yU+rGOlbbFVq+u+LaS6+s1eCJs=",
        "rqr9HeTQOg8TlFiIGZpJaxcvAaKHwMwrkqojJCpcvoc=",
        "i/Va0UxrbMo=",
        &SCHEDULE_B,
    ),
    (
        "Oy45fQVK9kq9019+VysXVlz1F9S1YwYKgXyzGlZrijo=",
        "/4GPpmZXYpn5RpkP7FC/dt8SXz7W30nUZTe8wb+3xmU=",
        "WFjKAHGEkQM=",
        &SCHEDULE_J,
    ),
    (
        "aoDIdXezm2l3HrcnQdkPJTDT8+W6mcl2/02ewBHfPzg=",
        "wsSGSBXKWA9q1oDJpjtJddVxH+evCfL5SO9HZnUDFU8=",
        "5Rr27rWd",
        &SCHEDULE_E,
    ),
];

fn rc4(key: &[u8], input: &[u8]) -> Vec<u8> {
    let mut s: Vec<u8> = (0..=255).collect();
    let mut j = 0u8;
    for i in 0..256 {
        j = j.wrapping_add(s[i]).wrapping_add(key[i % key.len()]);
        s.swap(i, j as usize);
    }
    let (mut i, mut j) = (0u8, 0u8);
    input
        .iter()
        .map(|c| {
            i = i.wrapping_add(1);
            j = j.wrapping_add(s[i as usize]);
            s.swap(i as usize, j as usize);
            c ^ s[s[i as usize].wrapping_add(s[j as usize]) as usize]
        })
        .collect()
}

/// `mangafire_vrf.generate`.
fn vrf(input: &str) -> String {
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut data = quote(input).into_bytes();
    for (key, seed, prefix, schedule) in ROUNDS {
        let (key, seed, prefix) = (
            b64.decode(key).unwrap_or_default(),
            b64.decode(seed).unwrap_or_default(),
            b64.decode(prefix).unwrap_or_default(),
        );
        let mixed = rc4(&key, &data);
        let mut out = Vec::with_capacity(mixed.len() + prefix.len());
        for (index, c) in mixed.into_iter().enumerate() {
            if let Some(p) = prefix.get(index) {
                out.push(*p);
            }
            out.push(schedule[index % 10](c ^ seed[index % 32]));
        }
        data = out;
    }
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

#[cfg(test)]
mod tests {
    use super::super::tests::{archive, path};
    use super::*;
    use crate::sites::testing::{children, html, http, json, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn signs_like_upstream() {
        assert_eq!(
            vrf("abc@chapter@en"),
            "5fcaUfZo7rW1-Z3vTEvXO5sJBfP2ueTM2NIVmfvDuGhYNi8cuoldJCpM52qyLdKWuUTngg"
        );
        assert_eq!(
            vrf("12345"),
            "5fcaUfZo7rW1-Z3vTEvXO5sJBfP2N-TM2NIVmfvruGhYMy8cqYlV"
        );
        assert_eq!(
            vrf("chapter@4567"),
            "5fcaUfZo7rW1-Z3vTEvXO5sJBfP2SeTM2NIVmfvGuGhYFi8cbYlf-pdmqmzSBZZ6"
        );
    }

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://mangafire.to/read/onepiecee.dkw/en/chapter-1100"),
            Some(Target::Chapter {
                path: "onepiecee.dkw".into(),
                id: "dkw".into(),
                lang: "en".into(),
                info: "chapter-1100".into(),
                kind: "chapter".into()
            })
        );
        assert!(
            matches!(t("https://mangafire.to/read/x.abc/fr/volume-3"), Some(Target::Chapter { kind, .. }) if kind == "volume")
        );
        assert_eq!(
            t("https://mangafire.to/manga/onepiecee.dkw"),
            Some(Target::Manga("onepiecee".into(), "dkw".into()))
        );
        assert_eq!(t("https://mangafire.to/home"), None);
    }

    const MANGA_PAGE: &str = r#"<div class="poster"><img src="https://static.mfcdn.cc/c.jpg"></div><p>releasing_now</p>
        <h1 itemprop="name">One Piece</h1><h6>ワンピース; OP</h6><span class="min-info"><a>Manga</a>
        <div><span>Author:</span><a>Oda Eiichiro</a></div><div><span>Published:</span>Jul 22, 1997 to ?</div>
        <div><span>Genres:</span><a>Action</a>, <a>Adventure</a></div><div><span>Mangazines:</span></div>
        <span class="score">9.10 / 10</span><div id="synopsis"><p>Gol D. Roger</p><script>"#;

    #[test]
    fn parses_manga_info() {
        let info = manga_info(MANGA_PAGE, "onepiecee.dkw");
        assert_eq!(info["status"], "Releasing Now");
        assert_eq!(info["manga_titles"], json!(["ワンピース", "OP"]));
        assert_eq!(info["author"], json!(["Oda Eiichiro"]));
        assert_eq!(info["publisher"], json!([]));
        assert_eq!(info["tags"], json!(["Action", "Adventure"]));
        assert_eq!(info["score"], 9.1);
        assert_eq!(info["manga_id"], "dkw");
        assert_eq!(info["description"], "Gol D. Roger");
    }

    fn list() -> Value {
        json!({"result": {"html": "<li><a href=\"/read/onepiecee.dkw/en/chapter-2\" data-number=\"2\" data-id=\"222\" title=\"Two\">\
            <li><a href=\"/read/onepiecee.dkw/en/chapter-1.5\" data-number=\"1.5\" data-id=\"115\" title=\"A &amp; B\">"}})
    }

    #[tokio::test]
    async fn extracts_chapter() {
        let (root, server) = serve(vec![
            json(list()),
            html(MANGA_PAGE),
            json(
                json!({"result": {"images": [["https://s.mfcdn.cc/1.jpg", 1, 0], ["https://s.mfcdn.cc/2.jpg", 1, 0]]}}),
            ),
        ]);
        let http = http("mangafire", root);
        let files = media(extract(
            &http,
            Url::parse("https://mangafire.to/read/onepiecee.dkw/en/chapter-1.5").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[0]);
        assert_eq!(k["chapter_id"], 115);
        assert_eq!(k["title"], "A & B");
        assert_eq!(k["lang"], "en");
        assert_eq!(
            path(&files[1]),
            "mangafire/One Piece/c001.5: A & B/One Piece_c001.5_002.jpg"
        );
        assert_eq!(
            archive(&files[0]),
            r#"["{manga_id}_{chapter_id}_{page}",["dkw","115","1"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with(&format!(
            "GET /ajax/read/dkw/chapter/en?vrf={} ",
            vrf("dkw@chapter@en")
        )));
        assert!(requests[2].starts_with(&format!(
            "GET /ajax/read/chapter/115?vrf={} ",
            vrf("chapter@115")
        )));
    }

    #[tokio::test]
    async fn manga_queues_chapters() {
        let (root, server) = serve(vec![html(MANGA_PAGE), json(list())]);
        let http = http("mangafire", root);
        assert_eq!(
            children(extract(
                &http,
                Url::parse("https://mangafire.to/manga/onepiecee.dkw").unwrap()
            ))
            .await
            .len(),
            2
        );
        server.join().unwrap();
    }
}
