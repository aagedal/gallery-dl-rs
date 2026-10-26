//! MangaReader chapters, volumes and manga, mirroring
//! gallery_dl/extractor/mangareader.py.
use super::{Gallery, set};
use crate::error;
use crate::sites::gdl::{
    Scan, captures, extr, extract_iter, group, parse_float, parse_int, partition, re, remove_html,
    rpartition, split_html, unescape,
};
use crate::sites::{Events, Http, Route, Site};
use serde_json::{Map, Value};
use url::Url;

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
static MANGA: Route = super::manga_route();

pub static SITE: Site = Site {
    category: "mangareader",
    name: "MangaReader",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CHAPTER, &MANGA],
    api_key: false,
    cookies: Some("mangareader.to"),
};

#[derive(Debug, PartialEq)]
enum Target {
    Chapter {
        path: String,
        lang: String,
        kind: String,
        number: String,
    },
    Manga(String),
}

fn target(url: &Url) -> Option<Target> {
    let chapter = re!(
        r"^https?://(?:www\.)?mangareader\.to/read/([\w-]+-\d+)/([^/?#]+)/(chapter|volume)-(\d+[^/?#]*)"
    );
    if let Some(caps) = captures(chapter, url) {
        return Some(Target::Chapter {
            path: group(&caps, 1).into(),
            lang: group(&caps, 2).into(),
            kind: group(&caps, 3).into(),
            number: group(&caps, 4).into(),
        });
    }
    let manga = re!(r"^https?://(?:www\.)?mangareader\.to/([\w-]+-\d+)");
    captures(manga, url).map(|caps| Target::Manga(group(&caps, 1).into()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported MangaReader URL"));
    };
    crate::sites::once(async move {
        Ok(match target {
            Target::Chapter {
                path,
                lang,
                kind,
                number,
            } => {
                let settings = serde_json::json!({
                    "readingMode": "vertical",
                    "readingDirection": "rtl",
                    "quality": "high",
                });
                http.set_cookie(
                    &http.api(ROOT)?,
                    &format!("mr_settings={settings}; Domain=mangareader.to; Path=/"),
                );
                let page =
                    super::fetch(http, &format!("{ROOT}/read/{path}/{lang}/{kind}-{number}"))
                        .await?;
                let cid = extr(&page, "data-reading-id=\"", "\"").to_owned();
                let info = manga_info(&super::fetch(http, &format!("{ROOT}/{path}")).await?, &path);
                let entry = info
                    .lists(&kind)
                    .get(&lang)
                    .and_then(|list| list.get(&number))
                    .cloned()
                    .ok_or_else(|| error("MangaReader chapter is missing from its manga"))?;
                let mut data = info.data.clone();
                data.extend(entry);
                set(&mut data, "chapter_id", parse_int(&cid));
                let key = if kind == "chapter" { "chap" } else { "vol" };
                let mut list = http.api(&format!("{ROOT}/ajax/image/list/{key}/{cid}"))?;
                list.query_pairs_mut()
                    .append_pair("mode", "vertical,")
                    .append_pair("quality", "high,")
                    .append_pair("hozPageSize", "1,");
                let json = http
                    .json_with(
                        &list,
                        &[
                            ("X-Requested-With", "XMLHttpRequest"),
                            ("Sec-Fetch-Dest", "empty"),
                            ("Sec-Fetch-Mode", "cors"),
                            ("Sec-Fetch-Site", "same-origin"),
                        ],
                    )
                    .await?;
                let html = crate::sites::gdl::str_of(&json, "html");
                let pages = extract_iter(html, "data-url=\"", "\"")
                    .into_iter()
                    .map(|url| (url.to_owned(), None))
                    .collect();
                crate::sites::events(Gallery::chapter(&CHAPTER, data, pages).events(&SITE, http))
            }
            Target::Manga(path) => {
                let info = manga_info(&super::fetch(http, &format!("{ROOT}/{path}")).await?, &path);
                let lang = http
                    .option_str("lang")
                    .filter(|l| !l.is_empty())
                    .unwrap_or("en");
                let chapters = info
                    .chapters
                    .get(lang)
                    .ok_or_else(|| error("MangaReader manga has no chapters in this language"))?
                    .values()
                    .filter_map(|entry| {
                        let url = Url::parse(entry.get("chapter_url")?.as_str()?).ok()?;
                        let mut data = info.data.clone();
                        data.extend(entry.clone());
                        Some((url, data))
                    })
                    .collect();
                super::queue_chapters(http, &SITE, &MANGA, chapters, true)
            }
        })
    })
}

type Lists = indexmap_lite::Ordered<indexmap_lite::Ordered<Map<String, Value>>>;

/// Insertion-ordered string maps, as Python dicts.
mod indexmap_lite {
    #[derive(Clone, Default)]
    pub struct Ordered<T>(Vec<(String, T)>);

    impl<T> Ordered<T> {
        pub fn get(&self, key: &str) -> Option<&T> {
            self.0.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v)
        }
        pub fn insert(&mut self, key: String, value: T) {
            match self.0.iter_mut().find(|(k, _)| *k == key) {
                Some(slot) => slot.1 = value,
                None => self.0.push((key, value)),
            }
        }
        pub fn values(&self) -> impl Iterator<Item = &T> {
            self.0.iter().map(|(_, v)| v)
        }
    }
}

/// `_manga_info`: public metadata plus the `_chapters`/`_volumes` lists.
struct MangaInfo {
    data: Map<String, Value>,
    chapters: Lists,
    volumes: Lists,
}

impl MangaInfo {
    fn lists(&self, kind: &str) -> &Lists {
        if kind == "chapter" {
            &self.chapters
        } else {
            &self.volumes
        }
    }
}

fn manga_info(html: &str, path: &str) -> MangaInfo {
    let mid = rpartition(path, "-").2;
    let mut scan = Scan::new(html);
    let url = scan.extr("property=\"og:url\" content=\"", "\"");
    let mut data = Map::new();
    set(&mut data, "manga_url", url);
    set(
        &mut data,
        "manga_slug",
        rpartition(rpartition(url, "/").2, "-").0,
    );
    set(&mut data, "manga_id", parse_int(mid));
    set(
        &mut data,
        "manga",
        unescape(scan.extr("class=\"manga-name\">", "<")),
    );
    set(
        &mut data,
        "manga_alt",
        unescape(scan.extr("class=\"manga-name-or\">", "<")),
    );
    set(
        &mut data,
        "tags",
        split_html(scan.extr("class=\"genres\">", "</div>")),
    );
    set(
        &mut data,
        "type",
        remove_html(scan.extr(">Type:", "</div>")),
    );
    set(
        &mut data,
        "status",
        remove_html(scan.extr(">Status:", "</div>")),
    );
    let authors: Vec<String> = split_html(scan.extr(">Authors:", "</div>"))
        .into_iter()
        .step_by(2)
        .collect();
    set(&mut data, "author", authors);
    set(
        &mut data,
        "published",
        remove_html(scan.extr(">Published:", "</div>")),
    );
    set(
        &mut data,
        "score",
        parse_float(&remove_html(scan.extr(">Score:", "</div>"))),
    );
    set(
        &mut data,
        "views",
        parse_int(&remove_html(scan.extr(">Views:", "</div>")).replace(',', "")),
    );

    let mut chapters = Lists::default();
    let list = scan.extr("class=\"chapters-list-ul\">", "    </div>");
    for group in extract_iter(list, "<ul", "</ul>") {
        let id = extr(group, " id=\"", "-chapters\"");
        let lang = partition(id, "-").0;
        let mut current = indexmap_lite::Ordered::default();
        for ch in extract_iter(group, "<li ", "</li>") {
            let path = extr(ch, "href=\"", "\"");
            let chap = extr(ch, "data-number=\"", "\"");
            let name = unescape(extr(ch, "class=\"name\">", "<"));
            let (chapter, sep, minor) = partition(chap, ".");
            let mut entry = Map::new();
            set(&mut entry, "title", partition(&name, ":").2.trim());
            set(&mut entry, "chapter", parse_int(chapter));
            set(&mut entry, "chapter_minor", format!("{sep}{minor}"));
            set(&mut entry, "chapter_string", chap);
            set(&mut entry, "chapter_url", format!("{ROOT}{path}"));
            set(&mut entry, "lang", lang);
            current.insert(chap.to_owned(), entry);
        }
        chapters.insert(id.to_owned(), current);
    }

    let mut volumes = Lists::default();
    let list = scan.extr("class=\"volume-list-ul\">", "</section>");
    for group in list.split("<div class=\"manga_list-wrap").skip(1) {
        let id = extr(group, " id=\"", "-volumes\"");
        let lang = partition(id, "-").0;
        let mut current = indexmap_lite::Ordered::default();
        for vol in extract_iter(group, "class=\"item\">", "</div>") {
            let path = extr(vol, "href=\"", "\"");
            let voln = rpartition(extr(vol, "tick-vol\">", "<"), " ").2;
            let mut entry = Map::new();
            set(&mut entry, "volume", parse_int(voln));
            set(&mut entry, "volume_cover", extr(vol, " src=\"", "\""));
            set(&mut entry, "chapter", 0);
            set(&mut entry, "chapter_minor", "");
            set(&mut entry, "chapter_string", voln);
            set(&mut entry, "chapter_url", format!("{ROOT}{path}"));
            set(&mut entry, "lang", lang);
            current.insert(voln.to_owned(), entry);
        }
        volumes.insert(id.to_owned(), current);
    }

    set(
        &mut data,
        "description",
        unescape(scan.extr("class=\"description-modal\">", "</div>")).trim(),
    );
    MangaInfo {
        data,
        chapters,
        volumes,
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{archive, path};
    use super::*;
    use crate::sites::testing::{children, html, http, json, keywords, media, serve};

    const MANGA_PAGE: &str = r#"<meta property="og:url" content="https://mangareader.to/one-piece-3">
        <h2 class="manga-name">One Piece</h2><div class="manga-name-or">ワンピース</div>
        <div class="genres"><a>Action</a><a>Adventure</a></div>
        <div>Type:<a>Manga</a></div><div>Status:<span>Publishing</span></div>
        <div>Authors:<a>Oda, Eiichiro</a>,<a>Other</a></div><div>Published:<span>Jul 22, 1997</span></div>
        <div>Score:<span>9.1</span></div><div>Views:<span>1,234</span></div>
        <div class="chapters-list-ul"><ul id="en-chapters">
        <li class="item" data-number="2"><a href="/read/one-piece-3/en/chapter-2"><span class="name">Chapter 2: Second</span></a></li>
        <li class="item" data-number="1.5"><a href="/read/one-piece-3/en/chapter-1.5"><span class="name">Chapter 1.5</span></a></li>
        </ul>
        <ul id="ja-chapters"><li class="item" data-number="1"><a href="/read/one-piece-3/ja/chapter-1"><span class="name">x</span></a></li></ul>
            </div>
        <div class="volume-list-ul"><div class="manga_list-wrap" id="en-volumes">
        <div class="item"><a href="/read/one-piece-3/en/volume-1"><img src="https://c.test/v1.jpg"><span class="tick-vol">VOL 1</span></a></div>
        </section><div class="description-modal"> A &amp; B </div>"#;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://mangareader.to/read/one-piece-3/en/chapter-1.5"),
            Some(Target::Chapter {
                path: "one-piece-3".into(),
                lang: "en".into(),
                kind: "chapter".into(),
                number: "1.5".into()
            })
        );
        assert!(matches!(
            t("https://mangareader.to/read/one-piece-3/en/volume-1"),
            Some(Target::Chapter { kind, .. }) if kind == "volume"
        ));
        assert_eq!(
            t("https://mangareader.to/one-piece-3"),
            Some(Target::Manga("one-piece-3".into()))
        );
        assert_eq!(t("https://mangareader.to/home"), None);
    }

    #[test]
    fn parses_manga_info() {
        let info = manga_info(MANGA_PAGE, "one-piece-3");
        assert_eq!(info.data["manga_slug"], "one-piece");
        assert_eq!(info.data["manga_id"], 3);
        assert_eq!(
            info.data["author"],
            serde_json::json!(["Oda, Eiichiro", "Other"])
        );
        assert_eq!(info.data["views"], 1234);
        assert_eq!(info.data["score"], 9.1);
        assert_eq!(info.data["description"], "A & B");
        let en = info.chapters.get("en").unwrap();
        assert_eq!(en.get("2").unwrap()["title"], "Second");
        assert_eq!(en.get("1.5").unwrap()["chapter_minor"], ".5");
        let vol = info.volumes.get("en").unwrap().get("1").unwrap();
        assert_eq!(vol["volume_cover"], "https://c.test/v1.jpg");
    }

    #[tokio::test]
    async fn extracts_chapter() {
        let (root, server) = serve(vec![
            html("<div data-reading-id=\"5555\"></div>"),
            html(MANGA_PAGE),
            json(serde_json::json!({"html":
                "<div data-url=\"https://c-1.mreadercdn.com/a/1.jpg\"></div><div data-url=\"https://c-1.mreadercdn.com/a/2.jpg\"></div>"})),
        ]);
        let http = http("mangareader", root);
        let files = media(extract(
            &http,
            Url::parse("https://mangareader.to/read/one-piece-3/en/chapter-1.5").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[1]);
        assert_eq!(k["chapter"], 1);
        assert_eq!(k["chapter_id"], 5555);
        assert_eq!(k["manga"], "One Piece");
        assert_eq!(
            path(&files[1]),
            "mangareader/One Piece/c001.5/One Piece_c001.5_002.jpg"
        );
        assert_eq!(
            archive(&files[0]),
            r#"["{manga_id}_{chapter_id}_{page}",["3","5555","1"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /read/one-piece-3/en/chapter-1.5 "));
        assert!(requests[2].starts_with(
            "GET /ajax/image/list/chap/5555?mode=vertical%2C&quality=high%2C&hozPageSize=1%2C "
        ));
    }

    #[tokio::test]
    async fn manga_queues_language_chapters() {
        let (root, server) = serve(vec![html(MANGA_PAGE)]);
        let http = http("mangareader", root);
        let queued = children(extract(
            &http,
            Url::parse("https://mangareader.to/one-piece-3").unwrap(),
        ))
        .await;
        assert_eq!(queued.len(), 2);
        server.join().unwrap();
    }
}
