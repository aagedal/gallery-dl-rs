//! AllPornComic chapters, comics and tag listings, mirroring
//! gallery_dl/extractor/allporncomic.py.
use super::{Gallery, set};
use crate::error;
use crate::sites::gdl::{
    Scan, captures, date, extr, extract_iter, group, parse_float, parse_int, parse_iso, partition,
    re, split_html, strptime, unescape,
};
use crate::sites::{Events, Http, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://allporncomic.com";
const BASE: &str = r"^https?://(?:www\.)?allporncomic\.com";

static CHAPTER: Route = Route::new(
    "chapter",
    "{page:>03}.{extension}",
    &["{category}", "{path[:-1]:I}", "{title}"],
    "{manga_id}_{chapter_id}_{page}",
);
static MANGA: Route = super::manga_route();
static TAG: Route = Route::new("tag", "{filename}.{extension}", &["{category}"], "");

pub static SITE: Site = Site {
    category: "allporncomic",
    name: "AllPornComic",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CHAPTER, &MANGA, &TAG],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    /// Path, comic slug, chapter number and title slug.
    Chapter(String, String, Option<String>, String),
    Manga(String),
    /// Listing path and optional query suffix.
    Tag(String, String),
}

fn target(url: &Url) -> Option<Target> {
    let chapter = re!(&format!(
        r"{BASE}(/porncomic/([^/?#]+)/(\d+(?:-\d+)?)?([^/?#]+))"
    ));
    if let Some(c) = captures(chapter, url) {
        return Some(Target::Chapter(
            group(&c, 1).into(),
            group(&c, 2).into(),
            c.get(3).map(|m| m.as_str().to_owned()),
            group(&c, 4).into(),
        ));
    }
    let manga = re!(&format!(r"{BASE}/porncomic/([^/?#]+)"));
    if let Some(c) = captures(manga, url) {
        return Some(Target::Manga(group(&c, 1).into()));
    }
    let tag = re!(&format!(
        r"{BASE}(/(?:porncomic-)?(?:genre|series|group|artist|characters)/[^/?#]+(?:/page/\d+)?)(/?\?[^#]+)?"
    ));
    captures(tag, url).map(|c| Target::Tag(group(&c, 1).into(), group(&c, 2).into()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported AllPornComic URL"));
    };
    match target {
        Target::Chapter(path, slug, chapter, title_slug) => crate::sites::once(async move {
            let page = super::fetch(http, &format!("{ROOT}{path}/")).await?;
            let info = super::fetch(http, &format!("{ROOT}/porncomic/{slug}/")).await?;
            let mut data = manga_info(&info, &slug);
            let (needle, media) = if page.contains("<source src=\"") {
                ("<source src=\"", "video")
            } else {
                (" data-src=\"", "image")
            };
            data.extend(chapter_metadata(
                &page,
                chapter.as_deref(),
                &title_slug,
                media,
            ));
            let pages = extract_iter(&page, needle, "\"")
                .into_iter()
                .map(|url| (url.trim().to_owned(), None))
                .collect();
            Ok(crate::sites::events(
                Gallery::chapter(&CHAPTER, data, pages).events(&SITE, http),
            ))
        }),
        Target::Manga(slug) => crate::sites::once(async move {
            let page = super::fetch(http, &format!("{ROOT}/porncomic/{slug}/")).await?;
            let slug = extr(&page, "/porncomic/", "/");
            let info = manga_info(&page, slug);
            let chapter_date = date(strptime(extr(&page, "<i>", "<"), "%B %d, %Y"));
            let chapters = extract_iter(&page, "<li class=\"wp-manga-chapter", "</li>")
                .into_iter()
                .filter_map(|ch| {
                    let mut data = info.clone();
                    set(&mut data, "date", chapter_date.clone());
                    Some((Url::parse(extr(ch, " href=\"", "\"")).ok()?, data))
                })
                .collect();
            Ok(super::queue_chapters(http, &SITE, &MANGA, chapters, true))
        }),
        Target::Tag(path, query) => {
            let first = format!(
                "{ROOT}{path}{}",
                if query.is_empty() { "/" } else { &query }
            );
            crate::sites::paginate(Some(first), move |url| async move {
                let Some(url) = url else { return Ok(None) };
                let page = super::fetch(http, &url).await?;
                let manga = extract_iter(&page, "id=\"manga-item-", "</div>")
                    .into_iter()
                    .filter_map(|m| Url::parse(extr(m, " href=\"", "\"")).ok())
                    .map(|url| (url, Map::new()))
                    .collect();
                let next = extr(&page, "<link rel=\"next\" href=\"", "\"");
                let next = (!next.is_empty()).then(|| next.to_owned());
                Ok(Some((super::queue_urls(http, &SITE, &TAG, manga), next)))
            })
        }
    }
}

/// `_manga_info`.
fn manga_info(page: &str, slug: &str) -> Map<String, Value> {
    let mut scan = Scan::new(page);
    let lang = scan
        .extr("property=\"og:locale\" content=\"", "\"")
        .to_owned();
    let title = unescape(scan.extr("property=\"og:title\" content=\"", "\""));
    let manga = re!(r"^(.+?)( \([^)]+\))?( \[[^\]]+\])?\s*$")
        .captures(&title)
        .map_or(String::new(), |c| group(&c, 1).to_owned());
    let even = |text: &str| -> Vec<String> { split_html(text).into_iter().step_by(2).collect() };
    let mut data = Map::new();
    set(
        &mut data,
        "description",
        unescape(scan.extr("property=\"og:description\" content=\"", "\"")),
    );
    set(&mut data, "manga", manga);
    set(&mut data, "manga_slug", slug);
    set(
        &mut data,
        "manga_cover",
        scan.extr("property=\"og:image\" content=\"", "\""),
    );
    set(
        &mut data,
        "manga_date",
        date(parse_iso(scan.extr("\"datePublished\":\"", "\""))),
    );
    set(
        &mut data,
        "manga_date_updated",
        date(parse_iso(scan.extr("\"dateModified\":\"", "\""))),
    );
    set(&mut data, "manga_id", parse_int(scan.extr(" postid-", " ")));
    set(
        &mut data,
        "rating",
        parse_float(scan.extr("total_votes\">", "<")),
    );
    set(
        &mut data,
        "votes",
        parse_int(scan.extr("id=\"countrate\">", "<")),
    );
    set(
        &mut data,
        "characters",
        even(scan.extr("class=\"author-content\">", "</div>")),
    );
    set(
        &mut data,
        "parody",
        even(scan.extr("class=\"author-content\">", "</div>")),
    );
    set(
        &mut data,
        "group",
        even(scan.extr("class=\"author-content\">", "</div>")),
    );
    set(
        &mut data,
        "artist",
        even(scan.extr("class=\"artist-content\">", "</div>")),
    );
    set(
        &mut data,
        "tags",
        even(scan.extr("class=\"genres-content\">", "</div>")),
    );
    set(
        &mut data,
        "type",
        scan.extr("class=\"summary-content\">", "<").trim(),
    );
    set(
        &mut data,
        "status",
        scan.extr("class=\"summary-content\">", "<").trim(),
    );
    set(&mut data, "comments", parse_int(scan.extr("<span>", " ")));
    set(
        &mut data,
        "bookmarks",
        parse_int(scan.extr("class=\"action_detail\"><span>", " ")),
    );
    set(&mut data, "lang", partition(&lang, "_").0);
    data
}

fn chapter_metadata(
    page: &str,
    chapter: Option<&str>,
    title_slug: &str,
    media: &str,
) -> Map<String, Value> {
    let (chapter, minor) = match chapter {
        Some(chapter) => {
            let (number, _, minor) = partition(chapter, "-");
            (number, minor)
        }
        None => ("", ""),
    };
    let path = split_html(extr(page, "<ol class=\"breadcrumb", "</ol>"));
    let last = path.last().cloned().unwrap_or_default();
    let title = re!(r"^(?:\s*\d+(?:\.\d+)?\s*\.|\[[^\]]+\])\s").replace(&last, "");
    let title = re!(
        r"(?:\s+-)?(?:\s+[Cc]hapter \d+(?:\s+[Ee]xtras)?)?(?:\s+\([^)]+\))?(?:\s+(?:-\s+)?\[[^\]]+\])?\s*$"
    )
    .replace(&title, "")
    .into_owned();
    let mut data = Map::new();
    set(
        &mut data,
        "path",
        path.get(3..).unwrap_or_default().to_vec(),
    );
    set(&mut data, "media", media);
    set(&mut data, "title", title);
    set(&mut data, "title_slug", title_slug.trim_start_matches('-'));
    set(&mut data, "chapter", parse_int(chapter));
    set(
        &mut data,
        "chapter_id",
        parse_int(extr(page, "manga_chapter_id\" value=\"", "\"")),
    );
    set(
        &mut data,
        "chapter_minor",
        if minor.is_empty() {
            String::new()
        } else {
            format!(".{minor}")
        },
    );
    data
}

#[cfg(test)]
mod tests {
    use super::super::tests::{archive, path};
    use super::*;
    use crate::sites::testing::{children, html, http, keywords, media, serve};

    const INFO: &str = r#"<meta property="og:locale" content="en_US"><meta property="og:title" content="Amerikan Bikini (Kantai Collection) [Kekocha]">
        <meta property="og:description" content="Desc"><meta property="og:image" content="https://c.test/c.jpg">
        "datePublished":"2020-01-01T00:00:00+00:00","dateModified":"2021-01-01T00:00:00+00:00"
        <body class="single postid-123 x"><span class="total_votes">4.2</span><span id="countrate">10</span>
        <div class="author-content"><a>Hibiki</a>, <a>Akatsuki</a></div><div class="author-content"><a>Kantai</a></div>
        <div class="author-content"><a>G</a></div><div class="artist-content"><a>Kekocha</a></div>
        <div class="genres-content"><a>Bikini</a></div><div class="summary-content"> Comic </div>
        <div class="summary-content"> Completed </div><span>5 comments</span><div class="action_detail"><span>7 Users"#;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://allporncomic.com/porncomic/trick-or-treat/0-1-tina-bonus-images/"),
            Some(Target::Chapter(
                "/porncomic/trick-or-treat/0-1-tina-bonus-images".into(),
                "trick-or-treat".into(),
                Some("0-1".into()),
                "-tina-bonus-images".into()
            ))
        );
        assert_eq!(
            t("https://allporncomic.com/porncomic/artworks/crisisbeat-vampire/"),
            Some(Target::Chapter(
                "/porncomic/artworks/crisisbeat-vampire".into(),
                "artworks".into(),
                None,
                "crisisbeat-vampire".into()
            ))
        );
        assert_eq!(
            t("https://allporncomic.com/porncomic/amerikan/"),
            Some(Target::Manga("amerikan".into()))
        );
        assert_eq!(
            t("https://allporncomic.com/characters/princess-zeldab/page/2/?m_orderby=latest"),
            Some(Target::Tag(
                "/characters/princess-zeldab/page/2".into(),
                "/?m_orderby=latest".into()
            ))
        );
        assert_eq!(
            t("https://allporncomic.com/porncomic-genre/bikinia/"),
            Some(Target::Tag(
                "/porncomic-genre/bikinia".into(),
                String::new()
            ))
        );
        assert_eq!(t("https://allporncomic.com/about/"), None);
    }

    #[test]
    fn parses_manga_info() {
        let info = manga_info(INFO, "amerikan");
        assert_eq!(info["manga"], "Amerikan Bikini");
        assert_eq!(info["manga_id"], 123);
        assert_eq!(
            info["characters"],
            serde_json::json!(["Hibiki", "Akatsuki"])
        );
        assert_eq!(info["parody"], serde_json::json!(["Kantai"]));
        assert_eq!(info["status"], "Completed");
        assert_eq!(info["comments"], 5);
        assert_eq!(info["bookmarks"], 7);
        assert_eq!(info["lang"], "en");
    }

    #[tokio::test]
    async fn extracts_chapter() {
        let page = r#"<ol class="breadcrumb"><li><a>Home</a></li><li><a>Comics</a></li><li><a>Kekocha</a></li>
            <li><a>Amerikan Bikini</a></li><li>1. Amerikan Bikini Car Wash - Chapter 1 (Kancolle) [Kekocha]</li></ol>
            <input name="manga_chapter_id" value="456"><img data-src=" https://allporncomic.com/wp-content/a/01.jpg ">
            <img data-src="https://allporncomic.com/wp-content/a/02.jpg">"#;
        let (root, server) = serve(vec![html(page), html(INFO)]);
        let http = http("allporncomic", root);
        let files = media(extract(
            &http,
            Url::parse("https://allporncomic.com/porncomic/amerikan/1-amerikan-bikini-car-wash/")
                .unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[1]);
        assert_eq!(k["title"], "Amerikan Bikini Car Wash");
        assert_eq!(
            k["path"],
            serde_json::json!([
                "Kekocha",
                "Amerikan Bikini",
                "1. Amerikan Bikini Car Wash - Chapter 1 (Kancolle) [Kekocha]"
            ])
        );
        assert_eq!(k["media"], "image");
        assert_eq!(k["chapter"], 1);
        assert_eq!(k["chapter_id"], 456);
        assert_eq!(
            path(&files[1]),
            "allporncomic/Kekocha/Amerikan Bikini/Amerikan Bikini Car Wash/002.jpg"
        );
        assert_eq!(
            archive(&files[0]),
            r#"["{manga_id}_{chapter_id}_{page}",["123","456","1"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /porncomic/amerikan/1-amerikan-bikini-car-wash/ "));
        assert!(requests[1].starts_with("GET /porncomic/amerikan/ "));
    }

    #[tokio::test]
    async fn tag_pages_through_listing() {
        let first = r#"<div id="manga-item-1"><a href="https://allporncomic.com/porncomic/a/">a</a></div>
            <link rel="next" href="https://allporncomic.com/series/k/page/2/">"#;
        let second =
            r#"<div id="manga-item-2"><a href="https://allporncomic.com/porncomic/b/">b</a></div>"#;
        let (root, server) = serve(vec![html(first), html(second)]);
        let http = http("allporncomic", root);
        assert_eq!(
            children(extract(
                &http,
                Url::parse("https://allporncomic.com/series/k/").unwrap()
            ))
            .await
            .len(),
            2
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /series/k/ "));
        assert!(requests[1].starts_with("GET /series/k/page/2/ "));
    }
}
