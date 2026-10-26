//! MangaFreak chapters and manga, mirroring gallery_dl/extractor/mangafreak.py.
use super::{Gallery, english, set};
use crate::error;
use crate::sites::gdl::{extr, extract_iter, parse_int, partition, rpartition, unescape};
use crate::sites::{Events, Http, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://ww2.mangafreak.me";
const IMAGES: &str = "https://images.mangafreak.me/mangas/";

static CHAPTER: Route = super::chapter_route();
static MANGA: Route = super::manga_route();

pub static SITE: Site = Site {
    category: "mangafreak",
    name: "MangaFreak",
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
        slug: String,
        string: String,
        chapter: String,
        minor: String,
    },
    Manga(String),
}

/// `ww[\dw]\.` or no subdomain.
fn host_ok(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let bare = match host.strip_suffix("mangafreak.me") {
        Some("") => true,
        Some(sub) => {
            let b = sub.as_bytes();
            b.len() == 4
                && b.starts_with(b"ww")
                && (b[2].is_ascii_digit() || b[2] == b'w')
                && b[3] == b'.'
        }
        None => false,
    };
    crate::sites::plain(url) && bare
}

fn target(url: &Url) -> Option<Target> {
    if !host_ok(url) {
        return None;
    }
    let first = url.path().strip_prefix('/')?.split('/').next()?;
    if let Some(rest) = first.strip_prefix("Read1_") {
        // `([^/?#]+)_((\d+)([a-z])?)` with a greedy slug.
        let mut cut = rest.len();
        while let Some(i) = rest[..cut].rfind('_') {
            let tail = &rest[i + 1..];
            let digits = tail.bytes().take_while(u8::is_ascii_digit).count();
            if i > 0 && digits > 0 {
                let minor = tail[digits..]
                    .chars()
                    .next()
                    .filter(char::is_ascii_lowercase)
                    .map(String::from)
                    .unwrap_or_default();
                let string = format!("{}{minor}", &tail[..digits]);
                return Some(Target::Chapter {
                    path: format!("/Read1_{}_{string}", &rest[..i]),
                    slug: rest[..i].into(),
                    chapter: tail[..digits].into(),
                    string,
                    minor,
                });
            }
            cut = i;
        }
        return None;
    }
    let parts = crate::sites::gdl::path_parts(url);
    match parts.as_slice() {
        ["Manga", slug, ..] if !slug.is_empty() => Some(Target::Manga((*slug).into())),
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported MangaFreak URL"));
    };
    crate::sites::once(async move {
        Ok(match target {
            Target::Chapter {
                path,
                slug,
                string,
                chapter,
                minor,
            } => {
                let page = super::fetch(http, &format!("{ROOT}{path}")).await?;
                let title = partition(extr(&page, "selected=\"selected\">", "<"), ": ").2;
                let mut data = Map::new();
                set(
                    &mut data,
                    "manga",
                    unescape(extr(&page, "<title>Read ", " Chapter ")),
                );
                set(&mut data, "manga_slug", slug);
                set(&mut data, "title", unescape(title));
                set(&mut data, "chapter", parse_int(&chapter));
                set(&mut data, "chapter_minor", minor);
                set(&mut data, "chapter_string", string);
                english(&mut data);
                let pages = extract_iter(&page, &format!("src=\"{IMAGES}"), "\"")
                    .into_iter()
                    .map(|path| (format!("{IMAGES}{path}"), None))
                    .collect();
                crate::sites::events(Gallery::chapter(&CHAPTER, data, pages).events(&SITE, http))
            }
            Target::Manga(slug) => {
                let page = super::fetch(http, &format!("{ROOT}/Manga/{slug}")).await?;
                super::queue_chapters(http, &SITE, &MANGA, chapters(&page, &slug), true)
            }
        })
    })
}

fn chapters(page: &str, slug: &str) -> Vec<(Url, Map<String, Value>)> {
    let table = extr(page, "<table>", "</table>");
    let mut data = Map::new();
    set(
        &mut data,
        "manga",
        unescape(extr(page, "<title>", " Manga")),
    );
    set(&mut data, "manga_slug", slug);
    english(&mut data);
    extract_iter(table, "<tr>", "</tr>")
        .into_iter()
        .filter_map(|row| {
            // Upstream looks for `<a href="`; the live table now puts a
            // class attribute first.
            let href = extr(row, " href=\"", "\"");
            if href.is_empty() {
                return None;
            }
            let string = rpartition(href, "_").2;
            // `(\d+)(\w*)`
            let digits = string.bytes().take_while(u8::is_ascii_digit).count();
            if digits == 0 {
                return None;
            }
            let rest = &string[digits..];
            let word = rest
                .find(|c: char| !(c.is_alphanumeric() || c == '_'))
                .unwrap_or(rest.len());
            let title = partition(extr(row, "\">", "<"), " - ").2;
            let mut item = Map::new();
            set(&mut item, "chapter", parse_int(&string[..digits]));
            set(&mut item, "chapter_minor", &rest[..word]);
            set(&mut item, "chapter_string", string);
            set(&mut item, "title", unescape(title));
            item.extend(data.clone());
            Some((Url::parse(&format!("{ROOT}{href}")).ok()?, item))
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
        assert_eq!(
            t("https://ww2.mangafreak.me/Read1_Onepunch_Man_167e"),
            Some(Target::Chapter {
                path: "/Read1_Onepunch_Man_167e".into(),
                slug: "Onepunch_Man".into(),
                string: "167e".into(),
                chapter: "167".into(),
                minor: "e".into(),
            })
        );
        assert!(matches!(
            t("https://mangafreak.me/Read1_Sss_Rank_23c"),
            Some(Target::Chapter { slug, .. }) if slug == "Sss_Rank"
        ));
        assert_eq!(
            t("https://www.mangafreak.me/Manga/Onepunch_Man"),
            Some(Target::Manga("Onepunch_Man".into()))
        );
        assert_eq!(t("https://ww2.mangafreak.me/Read1_Onepunch"), None);
        assert_eq!(t("https://w2.mangafreak.me/Manga/x"), None);
    }

    #[tokio::test]
    async fn extracts_chapter() {
        let page = r#"<title>Read Onepunch-Man Chapter 1 Online</title>
            <option selected="selected">Chapter 1: One &amp; Punch</option>
            <img src="https://images.mangafreak.me/mangas/onepunch_man/onepunch_man_1/onepunch_man_1_1.jpg">
            <img src="https://images.mangafreak.me/mangas/onepunch_man/onepunch_man_1/onepunch_man_1_2.jpg">"#;
        let (root, server) = serve(vec![html(page)]);
        let http = http("mangafreak", root);
        let files = media(extract(
            &http,
            Url::parse("https://ww2.mangafreak.me/Read1_Onepunch_Man_1").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[0]);
        assert_eq!(k["manga"], "Onepunch-Man");
        assert_eq!(k["title"], "One & Punch");
        assert_eq!(k["chapter_string"], "1");
        assert_eq!(
            path(&files[1]),
            "mangafreak/Onepunch-Man/c001: One & Punch/Onepunch-Man_c001_002.jpg"
        );
        server.join().unwrap();
    }

    #[test]
    fn lists_chapters() {
        let page = r#"<title>Onepunch Man Manga Online</title><table>
            <tr><td><a class="chapter-link" href="/Read1_Onepunch_Man_2b">Chapter 2b - Second</a></td></tr>
            <tr><td>none</td></tr></table>"#;
        let list = chapters(page, "Onepunch_Man");
        assert_eq!(list.len(), 1);
        assert_eq!(
            list[0].0.as_str(),
            "https://ww2.mangafreak.me/Read1_Onepunch_Man_2b"
        );
        assert_eq!(list[0].1["chapter_minor"], "b");
        assert_eq!(list[0].1["title"], "Second");
        assert_eq!(list[0].1["manga"], "Onepunch Man");
    }
}
