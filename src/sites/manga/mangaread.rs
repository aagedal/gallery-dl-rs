//! MangaRead chapters and manga, mirroring gallery_dl/extractor/mangaread.py.
use super::{Gallery, english, set};
use crate::error;
use crate::sites::gdl::{
    Scan, captures, extr, extract_iter, group, parse_float, parse_int, re, remove_html, split_html,
    unescape,
};
use crate::sites::{Events, Http, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://www.mangaread.org";

static CHAPTER: Route = super::chapter_route();
static MANGA: Route = super::manga_route();

pub static SITE: Site = Site {
    category: "mangaread",
    name: "MangaRead",
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
    let manga = re!(r"^https?://(?:www\.)?mangaread\.org(/manga/[^/?#]+)/?$");
    if let Some(caps) = captures(manga, url) {
        return Some(Target::Manga(group(&caps, 1).into()));
    }
    let chapter = re!(r"^https?://(?:www\.)?mangaread\.org(/manga/[^/?#]+/[^/?#]+)");
    captures(chapter, url).map(|caps| Target::Chapter(group(&caps, 1).into()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported MangaRead URL"));
    };
    crate::sites::once(async move {
        Ok(match target {
            Target::Chapter(path) => {
                let page = super::fetch(http, &format!("{ROOT}{path}")).await?;
                let tags = extr(&page, "class=\"wp-manga-tags-list\">", "</div>");
                let mut data = Map::new();
                let tags: Vec<String> = split_html(tags).into_iter().step_by(2).collect();
                set(&mut data, "tags", tags);
                let info = extr(&page, "<h1 id=\"chapter-heading\">", "</h1>");
                if info.is_empty() {
                    return Err(error("MangaRead chapter not found"));
                }
                parse_chapter_string(info, &mut data);
                let reading = extr(
                    &page,
                    "<div class=\"reading-content\">",
                    "<div class=\"entry-header",
                );
                let pages = extract_iter(reading, "<img id=\"image-", ">")
                    .into_iter()
                    .map(|img| (extr(img, "src=\"", "\"").trim().to_owned(), None))
                    .collect();
                crate::sites::events(Gallery::chapter(&CHAPTER, data, pages).events(&SITE, http))
            }
            Target::Manga(path) => {
                let page = super::fetch(http, &format!("{ROOT}{path}")).await?;
                if page.contains("class=\"error404") {
                    return Err(error("MangaRead manga not found"));
                }
                let mut data = manga_metadata(&page)?;
                let mut chapters = Vec::new();
                for chapter in extract_iter(&page, "<li class=\"wp-manga-chapter", "</li>") {
                    let mut scan = Scan::new(chapter);
                    let url = scan.extr("<a href=\"", "\"");
                    let info = scan.extr(">", "</a>");
                    parse_chapter_string(info, &mut data);
                    if let Ok(url) = Url::parse(url) {
                        chapters.push((url, data.clone()));
                    }
                }
                super::queue_chapters(http, &SITE, &MANGA, chapters, true)
            }
        })
    })
}

/// `MangareadBase.parse_chapter_string`.
fn parse_chapter_string(info: &str, data: &mut Map<String, Value>) {
    let pattern = re!(r"^(?:(.+)\s*-\s*)?[Cc]hapter\s*(\d+)(\.\d+)?(?:\s*-\s*(.+))?");
    let text = unescape(info);
    let caps = pattern.captures(text.trim());
    let field = |i| caps.as_ref().map_or("", |c| group(c, i));
    if !data.contains_key("manga") {
        set(data, "manga", field(1).trim());
    }
    set(data, "chapter", parse_int(field(2)));
    set(data, "chapter_minor", field(3));
    set(data, "title", field(4));
    english(data);
}

fn tags(text: &str) -> Vec<String> {
    extract_iter(text, "\"tag\">", "</a>")
        .into_iter()
        .map(str::to_owned)
        .collect()
}

fn manga_metadata(page: &str) -> crate::Result<Map<String, Value>> {
    let summary = extr(page, "class=\"summary_content\">", "class=\"manga-action\"");
    let mut scan = Scan::new(summary);
    let index = page
        .find("summary__content")
        .ok_or_else(|| error("MangaRead manga has no summary"))?;
    let mut data = Map::new();
    set(&mut data, "manga", extr(page, "<h1>", "</h1>").trim());
    set(
        &mut data,
        "description",
        unescape(&remove_html(Scan::at(page, index).extr(">", "</div>"))),
    );
    set(
        &mut data,
        "rating",
        parse_float(scan.extr("total_votes\">", "</span>").trim()),
    );
    let alt: Vec<String> = remove_html(scan.extr("Alternative\t\t</h5>\n\t</div>", "</div>"))
        .split("; ")
        .map(str::to_owned)
        .collect();
    set(&mut data, "manga_alt", alt);
    set(
        &mut data,
        "author",
        tags(scan.extr("class=\"author-content\">", "</div>")),
    );
    set(
        &mut data,
        "artist",
        tags(scan.extr("class=\"artist-content\">", "</div>")),
    );
    set(
        &mut data,
        "genres",
        tags(scan.extr("class=\"genres-content\">", "</div>")),
    );
    set(
        &mut data,
        "type",
        remove_html(scan.extr("\tType\t", "\n</div>")),
    );
    set(
        &mut data,
        "release",
        parse_int(&remove_html(scan.extr("\tRelease\t", "\n</div>"))),
    );
    set(
        &mut data,
        "status",
        remove_html(scan.extr("\tStatus\t", "\n</div>")),
    );
    Ok(data)
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
            t("https://www.mangaread.org/manga/one-piece/chapter-1053-3/"),
            Some(Target::Chapter("/manga/one-piece/chapter-1053-3".into()))
        );
        assert_eq!(
            t("https://www.mangaread.org/manga/one-piece"),
            Some(Target::Manga("/manga/one-piece".into()))
        );
        assert_eq!(
            t("https://mangaread.org/manga/one-piece/"),
            Some(Target::Manga("/manga/one-piece".into()))
        );
        assert_eq!(t("https://www.mangaread.org/genres/x"), None);
    }

    #[test]
    fn parses_chapter_strings() {
        let mut data = Map::new();
        parse_chapter_string(" One Piece - Chapter 1053.3 - The New Emperors ", &mut data);
        assert_eq!(data["manga"], "One Piece");
        assert_eq!(data["chapter"], 1053);
        assert_eq!(data["chapter_minor"], ".3");
        assert_eq!(data["title"], "The New Emperors");
        let mut data = Map::new();
        set(&mut data, "manga", "Kept");
        parse_chapter_string("chapter146", &mut data);
        assert_eq!(data["manga"], "Kept");
        assert_eq!(data["chapter"], 146);
    }

    #[tokio::test]
    async fn extracts_chapter() {
        let page = r#"<div class="wp-manga-tags-list"><a>Action</a>, <a>Pirates</a></div>
            <h1 id="chapter-heading">One Piece - Chapter 1</h1>
            <div class="reading-content"><img id="image-0" src=" https://www.mangaread.org/1.jpg " class="x">
            <img id="image-1" src="https://www.mangaread.org/2.jpg"></div><div class="entry-header">"#;
        let (root, server) = serve(vec![html(page)]);
        let http = http("mangaread", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.mangaread.org/manga/one-piece/chapter-1/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[0]);
        assert_eq!(k["tags"], serde_json::json!(["Action", "Pirates"]));
        assert_eq!(
            path(&files[1]),
            "mangaread/One Piece/c001/One Piece_c001_002.jpg"
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn manga_lists_chapters_with_metadata() {
        let page = "<h1> Kanan </h1><div class=\"summary_content\"><span class=\"total_votes\"> 4.5</span>\
            Alternative\t\t</h5>\n\t</div><div>A; B</div><div class=\"author-content\"><a rel=\"tag\">Au</a></div>\
            <div class=\"genres-content\"><a rel=\"tag\">G1</a><a rel=\"tag\">G2</a></div>\
            \tType\t<div>Manga\n</div>\tRelease\t<div>2019\n</div>\tStatus\t<div>OnGoing\n</div>\
            <div class=\"manga-action\"><div class=\"summary__content\"><p>Desc &amp;</p></div>\
            <li class=\"wp-manga-chapter\"><a href=\"https://www.mangaread.org/manga/k/chapter-2/\">Chapter 2</a></li>\
            <li class=\"wp-manga-chapter\"><a href=\"https://www.mangaread.org/manga/k/chapter-1/\">Chapter 1 - Start</a></li>";
        let data = manga_metadata(page).unwrap();
        assert_eq!(data["manga"], "Kanan");
        assert_eq!(data["rating"], 4.5);
        assert_eq!(data["manga_alt"], serde_json::json!(["A", "B"]));
        assert_eq!(data["genres"], serde_json::json!(["G1", "G2"]));
        assert_eq!(data["type"], "Manga");
        assert_eq!(data["release"], 2019);
        assert_eq!(data["status"], "OnGoing");
        assert_eq!(data["description"], "Desc &");
        let (root, server) = serve(vec![html(page)]);
        let http = http("mangaread", root);
        assert_eq!(
            children(extract(
                &http,
                Url::parse("https://www.mangaread.org/manga/k").unwrap()
            ))
            .await
            .len(),
            2
        );
        server.join().unwrap();
    }
}
