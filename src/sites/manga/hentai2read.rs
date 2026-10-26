//! Hentai2Read chapters and manga, mirroring
//! gallery_dl/extractor/hentai2read.py.
use super::{Gallery, english, set};
use crate::error;
use crate::sites::gdl::{Scan, captures, group, parse_int, partition, re, unescape};
use crate::sites::{Events, Http, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://hentai2read.com";

static CHAPTER: Route = Route::new(
    "chapter",
    super::CHAPTER_FILENAME,
    super::CHAPTER_DIRECTORY,
    "{chapter_id}_{page}",
);
static MANGA: Route = super::manga_route();

pub static SITE: Site = Site {
    category: "hentai2read",
    name: "Hentai2Read",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CHAPTER, &MANGA],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    /// Path and chapter string.
    Chapter(String, String),
    Manga(String),
}

fn target(url: &Url) -> Option<Target> {
    let chapter = re!(r"^https?://(?:www\.)?hentai2read\.com(/[^/?#]+/([^/?#]+))");
    if let Some(caps) = captures(chapter, url) {
        return Some(Target::Chapter(
            group(&caps, 1).into(),
            group(&caps, 2).into(),
        ));
    }
    let manga = re!(r"^https?://(?:www\.)?hentai2read\.com(/[^/?#]+)/?$");
    captures(manga, url).map(|caps| Target::Manga(group(&caps, 1).into()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Hentai2Read URL"));
    };
    crate::sites::once(async move {
        Ok(match target {
            Target::Chapter(path, chapter) => {
                let page = super::fetch(http, &format!("{ROOT}{path}")).await?;
                let data = chapter_metadata(&page, &chapter);
                let images: Vec<String> =
                    serde_json::from_str(Scan::new(&page).extr("'images' : ", ",\n"))
                        .map_err(|_| error("Hentai2Read chapter has no image list"))?;
                let pages = images
                    .into_iter()
                    .map(|part| (format!("https://hentaicdn.com/hentai{part}"), None))
                    .collect();
                crate::sites::events(Gallery::chapter(&CHAPTER, data, pages).events(&SITE, http))
            }
            Target::Manga(path) => {
                let page = super::fetch(http, &format!("{ROOT}{path}")).await?;
                super::queue_chapters(http, &SITE, &MANGA, chapters(&page), true)
            }
        })
    })
}

fn chapter_metadata(page: &str, chapter: &str) -> Map<String, Value> {
    let mut scan = Scan::new(page);
    let title = scan.extr("<title>", "</title>");
    let manga_id = scan.extr("data-mid=\"", "\"");
    let chapter_id = scan.extr("data-cid=\"", "\"");
    let (number, sep, minor) = partition(chapter, ".");
    let pattern = re!(r"^Reading (.+) \(([^)]+)\) Hentai(?: by (.*))? - ([^:]+): (.+) . Page 1 ");
    let (manga, kind, author, title) = match pattern.captures(title) {
        Some(caps) => (
            group(&caps, 1),
            group(&caps, 2),
            group(&caps, 3),
            group(&caps, 5),
        ),
        None => ("", "", "", ""),
    };
    let mut data = Map::new();
    set(&mut data, "manga", manga);
    set(&mut data, "manga_id", parse_int(manga_id));
    set(&mut data, "chapter", parse_int(number));
    set(&mut data, "chapter_minor", format!("{sep}{minor}"));
    set(&mut data, "chapter_id", parse_int(chapter_id));
    set(&mut data, "type", kind);
    set(&mut data, "author", author);
    set(&mut data, "title", title);
    english(&mut data);
    data
}

fn chapters(page: &str) -> Vec<(Url, Map<String, Value>)> {
    let start = page
        .find("itemscope itemtype=\"http://schema.org/Book")
        .map_or(0, |i| i + 1);
    let mut scan = Scan::at(page, start);
    let manga = scan.extr("<span itemprop=\"name\">", "</span>").to_owned();
    let kind = scan
        .extr("<small class=\"text-danger\">[", "]</small>")
        .to_owned();
    let manga_id = parse_int(Scan::at(page, scan.pos()).extr("data-mid=\"", "\""));
    let mut results = Vec::new();
    while let Some(chapter_id) = scan.find(" data-cid=\"", "\"").filter(|id| !id.is_empty()) {
        scan.extr(" href=\"", "\"");
        let url = scan.extr(" href=\"", "\"");
        let label = unescape(scan.extr(">", "<"));
        let (chapter, _, title) = partition(label.trim(), " - ");
        let (number, sep, minor) = partition(chapter, ".");
        let mut data = Map::new();
        set(&mut data, "manga", manga.as_str());
        set(&mut data, "manga_id", manga_id);
        set(&mut data, "chapter", parse_int(number));
        set(&mut data, "chapter_minor", format!("{sep}{minor}"));
        set(&mut data, "chapter_id", parse_int(chapter_id));
        set(&mut data, "type", kind.as_str());
        set(&mut data, "title", title);
        english(&mut data);
        if let Ok(url) = Url::parse(url) {
            results.push((url, data));
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
            t("https://hentai2read.com/amazon_elixir/1/"),
            Some(Target::Chapter("/amazon_elixir/1".into(), "1".into()))
        );
        assert_eq!(
            t("https://hentai2read.com/popuni_kei_joshi_panic/2.5/"),
            Some(Target::Chapter(
                "/popuni_kei_joshi_panic/2.5".into(),
                "2.5".into()
            ))
        );
        assert_eq!(
            t("https://www.hentai2read.com/amazon_elixir/"),
            Some(Target::Manga("/amazon_elixir".into()))
        );
        assert_eq!(t("https://hentai2read.com/amazon_elixir?x"), None);
    }

    #[tokio::test]
    async fn extracts_chapter() {
        let page = "<title>Reading Amazon Elixir (Original) Hentai by Kuroiwa - 1: Story . Page 1 Online</title>\
            <div data-mid=\"1\" data-cid=\"413\"></div>\
            'images' : [\"/1/1/01.jpg\",\"/1/1/02.png\"],\n";
        let (root, server) = serve(vec![html(page)]);
        let http = http("hentai2read", root);
        let files = media(extract(
            &http,
            Url::parse("https://hentai2read.com/amazon_elixir/1/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[1].url.as_str(),
            "https://hentaicdn.com/hentai/1/1/02.png"
        );
        let k = keywords(&files[1]);
        assert_eq!(k["manga"], "Amazon Elixir");
        assert_eq!(k["type"], "Original");
        assert_eq!(k["author"], "Kuroiwa");
        assert_eq!(k["title"], "Story");
        assert_eq!(k["chapter_id"], 413);
        assert_eq!(archive(&files[0]), r#"["{chapter_id}_{page}",["413","1"]]"#);
        assert_eq!(
            path(&files[1]),
            "hentai2read/Amazon Elixir/c001: Story/Amazon Elixir_c001_002.png"
        );
        server.join().unwrap();
    }

    #[test]
    fn lists_chapters() {
        let page = r##"<div itemscope itemtype="http://schema.org/Book"><span itemprop="name">Amazon Elixir</span>
            <small class="text-danger">[Original]</small><a data-mid="1"></a>
            <li><div data-cid="414"><a href="#bookmark"></a><a href="https://hentai2read.com/amazon_elixir/2/">
            2.5 - Part Two</a></div></li>"##;
        let list = chapters(page);
        assert_eq!(list.len(), 1);
        assert_eq!(
            list[0].0.as_str(),
            "https://hentai2read.com/amazon_elixir/2/"
        );
        let d = &list[0].1;
        assert_eq!(d["manga"], "Amazon Elixir");
        assert_eq!(d["type"], "Original");
        assert_eq!(d["manga_id"], 1);
        assert_eq!(d["chapter"], 2);
        assert_eq!(d["chapter_minor"], ".5");
        assert_eq!(d["title"], "Part Two");
    }
}
