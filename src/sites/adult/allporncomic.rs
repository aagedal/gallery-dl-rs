//! AllPornComic chapters, comics and tag listings, mirroring
//! gallery_dl/extractor/allporncomic.py: a Madara (WordPress) reader whose
//! comic pages list their chapters and whose chapter pages list images (or
//! a video source).
use crate::sites::art::{Scan, date, extr, extract_iter, on_host, parse_iso, split_html, strptime};
use crate::sites::manga::{Gallery, Memo, keywords, set, text};
use crate::sites::{Events, Http, Route, Site, unescape};
use crate::{Result, error};
use futures_util::StreamExt;
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://allporncomic.com";
static CHAPTER: Route = Route::new(
    "chapter",
    "{page:>03}.{extension}",
    &["{category}", "{path[:-1]:I}", "{title}"],
    "{manga_id}_{chapter_id}_{page}",
);
static MANGA: Route = Route::new("manga", "{filename}.{extension}", &["{category}"], "");
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
    /// Comic slug, chapter number (`N` or `N-M`) and title slug, raw.
    Chapter {
        manga: String,
        chapter: Option<String>,
        title: String,
    },
    Manga(String),
    /// The listing path and its `?query` (or `/`).
    Tag(String, String),
}

/// `/porncomic/([^/?#]+)/(\d+(?:-\d+)?)?([^/?#]+)`,
/// `/porncomic/([^/?#]+)` and
/// `(/(?:porncomic-)?(?:genre|series|group|artist|characters)/[^/?#]+
/// (?:/page/\d+)?)(/?\?[^#]+)?`, all unanchored, in class order.
fn target(url: &Url) -> Option<Target> {
    if !on_host(url, &["allporncomic.com"]) {
        return None;
    }
    let raw: Vec<&str> = url.path().split('/').skip(1).collect();
    match raw.as_slice() {
        ["porncomic", manga, segment, ..] if !manga.is_empty() && !segment.is_empty() => {
            let (chapter, title) = split_chapter(segment);
            return Some(Target::Chapter {
                manga: (*manga).to_owned(),
                chapter: chapter.map(str::to_owned),
                title: title.to_owned(),
            });
        }
        ["porncomic", manga, ..] if !manga.is_empty() => {
            return Some(Target::Manga((*manga).to_owned()));
        }
        _ => {}
    }
    let [kind, name, rest @ ..] = raw.as_slice() else {
        return None;
    };
    let kind_ok = matches!(
        kind.strip_prefix("porncomic-").unwrap_or(kind),
        "genre" | "series" | "group" | "artist" | "characters"
    );
    if !kind_ok || name.is_empty() {
        return None;
    }
    let mut path = format!("/{kind}/{name}");
    let mut rest = rest;
    if let ["page", number, tail @ ..] = rest
        && crate::sites::art::digits(number)
    {
        path = format!("{path}/page/{number}");
        rest = tail;
    }
    // The optional `(/?\?[^#]+)` must follow the matched path directly.
    let query = url.query().filter(|q| !q.is_empty());
    let suffix = match (rest, query) {
        ([] | [""], Some(query)) => format!("{}?{query}", if rest.is_empty() { "" } else { "/" }),
        _ => "/".to_owned(),
    };
    Some(Target::Tag(path, suffix))
}

/// `(\d+(?:-\d+)?)?([^/?#]+)` over one segment, with the regex's
/// backtracking: the title keeps at least one character.
fn split_chapter(segment: &str) -> (Option<&str>, &str) {
    let digits = |s: &str| s.bytes().take_while(u8::is_ascii_digit).count();
    let mut len = digits(segment);
    if len > 0
        && let Some(minor) = segment[len..].strip_prefix('-')
        && digits(minor) > 0
    {
        len += 1 + digits(minor);
    }
    if len == segment.len() {
        len -= 1;
        if segment[..len].ends_with('-') {
            len -= 1;
        }
    }
    match len {
        0 => (None, segment),
        len => (Some(&segment[..len]), &segment[len..]),
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Chapter {
            manga,
            chapter,
            title,
        }) => chapter_events(http, manga, chapter, title, Memo::default()),
        Some(Target::Manga(manga)) => crate::sites::once(manga_events(http, manga)),
        Some(Target::Tag(path, query)) => tag_events(http, format!("{ROOT}{path}{query}")),
        None => crate::sites::fail(error("unsupported AllPornComic URL")),
    }
}

/// `AllporncomicBase._manga_info`.
fn manga_info(page: &str, slug: &str) -> Map<String, Value> {
    let mut e = Scan::new(page);
    let lang = e.extr("property=\"og:locale\" content=\"", "\"");
    let title = unescape(e.extr("property=\"og:title\" content=\"", "\""));
    let description = unescape(e.extr("property=\"og:description\" content=\"", "\""));
    let cover = e.extr("property=\"og:image\" content=\"", "\"");
    let published = parse_iso(e.extr("\"datePublished\":\"", "\""));
    let modified = parse_iso(e.extr("\"dateModified\":\"", "\""));
    let id = text::parse_int(e.extr(" postid-", " "));
    let rating = text::parse_float(e.extr("total_votes\">", "<"));
    let votes = text::parse_int(e.extr("id=\"countrate\">", "<"));
    let mut every_other = |begin: &str| -> Vec<String> {
        split_html(e.extr(begin, "</div>"))
            .into_iter()
            .step_by(2)
            .collect()
    };
    let characters = every_other("class=\"author-content\">");
    let parody = every_other("class=\"author-content\">");
    let group = every_other("class=\"author-content\">");
    let artist = every_other("class=\"artist-content\">");
    let tags = every_other("class=\"genres-content\">");
    keywords([
        ("description", description.into()),
        ("manga", manga_name(&title).into()),
        ("manga_slug", slug.into()),
        ("manga_cover", cover.into()),
        ("manga_date", date(published)),
        ("manga_date_updated", date(modified)),
        ("manga_id", id.into()),
        ("rating", rating),
        ("votes", votes.into()),
        ("characters", characters.into()),
        ("parody", parody.into()),
        ("group", group.into()),
        ("artist", artist.into()),
        ("tags", tags.into()),
        (
            "type",
            e.extr("class=\"summary-content\">", "<").trim().into(),
        ),
        (
            "status",
            e.extr("class=\"summary-content\">", "<").trim().into(),
        ),
        ("comments", text::parse_int(e.extr("<span>", " ")).into()),
        (
            "bookmarks",
            text::parse_int(e.extr("class=\"action_detail\"><span>", " ")).into(),
        ),
        ("lang", text::partition(lang, "_").0.into()),
    ])
}

/// `(.+?)( \([^)]+\))?( \[[^\]]+\])?\s*$` matched against the title: the
/// shortest name followed only by an optional `(…)`, `[…]` and whitespace.
fn manga_name(title: &str) -> String {
    let group = |rest: &str, open: char, close: char| -> Option<usize> {
        let inner = rest.strip_prefix(' ')?.strip_prefix(open)?;
        let end = inner.find(close).filter(|&end| end > 0)?;
        Some(2 + end + 1)
    };
    let tail = |rest: &str| -> bool {
        let mut options = vec![rest];
        if let Some(len) = group(rest, '(', ')') {
            options.push(&rest[len..]);
        }
        options.iter().any(|rest| {
            rest.trim_start().is_empty()
                || group(rest, '[', ']').is_some_and(|len| rest[len..].trim_start().is_empty())
        })
    };
    title
        .char_indices()
        .skip(1)
        .map(|(i, _)| i)
        .chain([title.len()])
        .take_while(|&i| !title[..i].contains('\n'))
        .find(|&i| i > 0 && tail(&title[i..]))
        .map(|i| title[..i].to_owned())
        .unwrap_or_default()
}

/// `AllporncomicBase._manga_info` behind `Extractor.cache`.
async fn cached_info(http: &Http, slug: &str, memo: &Memo) -> Result<Map<String, Value>> {
    if let Some(Value::Object(info)) = memo.get(slug) {
        return Ok(info);
    }
    let page = super::page(http, &format!("{ROOT}/porncomic/{slug}/"), &[]).await?;
    let info = manga_info(&page, slug);
    memo.insert(slug, Value::Object(info.clone()));
    Ok(info)
}

/// `^(?:\s*\d+(?:\.\d+)?\s*\.|\[[^\]]+\])\s` removed from a breadcrumb.
fn strip_number(title: &str) -> &str {
    let prefix = |s: &str| -> Option<usize> {
        if let Some(inner) = s.strip_prefix('[') {
            let end = inner.find(']').filter(|&end| end > 0)?;
            return Some(end + 2);
        }
        let body = s.trim_start();
        let mut pos = s.len() - body.len();
        let digits = body.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        pos += digits;
        if let Some(minor) = s[pos..].strip_prefix('.') {
            let len = minor.bytes().take_while(u8::is_ascii_digit).count();
            if len > 0 {
                pos += 1 + len;
            }
        }
        let rest = s[pos..].trim_start();
        pos = s.len() - rest.len();
        rest.starts_with('.').then_some(pos + 1)
    };
    prefix(title)
        .and_then(|len| {
            let rest = &title[len..];
            let ws = rest.chars().next().filter(|c| c.is_whitespace())?;
            Some(&rest[ws.len_utf8()..])
        })
        .unwrap_or(title)
}

/// `(?:\s+-)?(?:\s+[Cc]hapter \d+(?:\s+[Ee]xtras)?)?(?:\s+\([^)]+\))?
/// (?:\s+(?:-\s+)?\[[^\]]+\])?\s*$` removed from the earliest position
/// where it matches.
fn strip_suffix(title: &str) -> &str {
    fn spaces(s: &str) -> Option<&str> {
        let rest = s.trim_start();
        (rest.len() < s.len()).then_some(rest)
    }
    fn dash(s: &str) -> Option<&str> {
        spaces(s)?.strip_prefix('-')
    }
    fn chapter(s: &str) -> Option<&str> {
        let s = spaces(s)?;
        let s = s
            .strip_prefix("Chapter ")
            .or_else(|| s.strip_prefix("chapter "))?;
        let digits = s.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        let s = &s[digits..];
        Some(
            spaces(s)
                .and_then(|r| {
                    r.strip_prefix("Extras")
                        .or_else(|| r.strip_prefix("extras"))
                })
                .unwrap_or(s),
        )
    }
    fn bracketed(s: &str, open: char, close: char) -> Option<&str> {
        let inner = s.strip_prefix(open)?;
        let end = inner.find(close).filter(|&end| end > 0)?;
        Some(&inner[end + close.len_utf8()..])
    }
    fn paren(s: &str) -> Option<&str> {
        bracketed(spaces(s)?, '(', ')')
    }
    fn square(s: &str) -> Option<&str> {
        let s = spaces(s)?;
        let s = s.strip_prefix('-').and_then(spaces).unwrap_or(s);
        bracketed(s, '[', ']')
    }
    let steps: [fn(&str) -> Option<&str>; 4] = [dash, chapter, paren, square];
    let matches = |start: &str| {
        (0..16u8).any(|mask| {
            let mut rest = Some(start);
            for (bit, step) in steps.iter().enumerate() {
                if mask & (1 << bit) != 0 {
                    rest = rest.and_then(step);
                }
            }
            rest.is_some_and(|rest| rest.trim_start().is_empty())
        })
    };
    let end = title
        .char_indices()
        .map(|(i, _)| i)
        .chain([title.len()])
        .find(|&i| matches(&title[i..]))
        .unwrap_or(title.len());
    &title[..end]
}

/// `AllporncomicChapterExtractor`: chapter metadata over the cached comic
/// info, and its images (or video sources).
fn chapter_events(
    http: &Http,
    manga: String,
    chapter: Option<String>,
    title_slug: String,
    memo: Memo,
) -> Events<'_> {
    crate::sites::once(async move {
        let number = chapter.as_deref().unwrap_or_default();
        let url = format!("{ROOT}/porncomic/{manga}/{number}{title_slug}/");
        let page = super::page(http, &url, &[]).await?;
        let (media, needle) = match page.contains("<source src=\"") {
            true => ("video", "<source src=\""),
            false => ("image", " data-src=\""),
        };
        let path = split_html(extr(&page, "<ol class=\"breadcrumb", "</ol>"));
        let title = strip_suffix(strip_number(path.last().map_or("", String::as_str))).to_owned();
        let (major, _, minor) = text::partition(number, "-");
        let mut data = cached_info(http, &manga, &memo).await?;
        set(
            &mut data,
            "path",
            path.get(3..).unwrap_or_default().to_vec(),
        );
        set(&mut data, "media", media);
        set(&mut data, "title", title);
        set(&mut data, "title_slug", title_slug.trim_start_matches('-'));
        set(&mut data, "chapter", text::parse_int(major));
        set(
            &mut data,
            "chapter_id",
            text::parse_int(extr(&page, "manga_chapter_id\" value=\"", "\"")),
        );
        let minor = match minor {
            "" => String::new(),
            minor => format!(".{minor}"),
        };
        set(&mut data, "chapter_minor", minor);
        let pages = extract_iter(&page, needle, "\"")
            .into_iter()
            .map(|url| (url.trim().to_owned(), None))
            .collect();
        Ok(crate::sites::events(
            Gallery::chapter(&CHAPTER, data, pages)
                .id(format!("{manga}/{number}{title_slug}"))
                .events(&SITE, http),
        ))
    })
}

/// `AllporncomicMangaExtractor`: each listed chapter with the comic info and
/// the page's first `<i>` date, queued newest-last unless `chapter-reverse`.
async fn manga_events(http: &Http, manga: String) -> Result<Events<'_>> {
    let page = super::page(http, &format!("{ROOT}/porncomic/{manga}/"), &[]).await?;
    let slug = extr(&page, "/porncomic/", "/");
    let info = manga_info(&page, slug);
    let memo = Memo::default();
    memo.insert(slug, Value::Object(info.clone()));
    let chapter_date = date(strptime(extr(&page, "<i>", "<"), "%B %d, %Y"));
    let mut chapters = Vec::new();
    for item in extract_iter(&page, "<li class=\"wp-manga-chapter", "</li>") {
        let mut data = info.clone();
        set(&mut data, "date", chapter_date.clone());
        chapters.push((extr(item, " href=\"", "\"").to_owned(), data));
    }
    // `MangaExtractor.reverse = True`.
    if !http.option_bool("chapter-reverse", false) {
        chapters.reverse();
    }
    let streams: Vec<Events<'_>> = chapters
        .into_iter()
        .filter_map(|(url, data)| match target(&Url::parse(&url).ok()?)? {
            Target::Chapter {
                manga,
                chapter,
                title,
            } => {
                let memo = memo.clone();
                Some(crate::sites::manga::queue_with(
                    &SITE,
                    &MANGA,
                    &data,
                    move || chapter_events(http, manga, chapter, title, memo),
                ))
            }
            _ => None,
        })
        .collect();
    Ok(futures_util::stream::iter(streams).flatten().boxed())
}

/// `AllporncomicTagExtractor`: each listed comic, following `rel="next"`.
fn tag_events(http: &Http, url: String) -> Events<'_> {
    crate::sites::paginate(Some((url, 0)), move |state| async move {
        let Some((url, count)) = state else {
            return Ok(None);
        };
        let page = super::page(http, &url, &[]).await?;
        let children = extract_iter(&page, "id=\"manga-item-", "</div>")
            .into_iter()
            .filter_map(|item| Url::parse(extr(item, " href=\"", "\"")).ok())
            .map(|url| (url, Map::new()))
            .collect();
        let next = Some(extr(&page, "<link rel=\"next\" href=\"", "\""))
            .filter(|next| Url::parse(next).is_ok_and(|next| on_host(&next, &["allporncomic.com"])))
            .filter(|_| count + 1 < super::MAX_PAGES)
            .map(|next| (next.to_owned(), count + 1));
        Ok(Some((
            crate::sites::manga::queue_urls(http, &SITE, &TAG, children),
            next,
        )))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::manga::tests::{archive, path};
    use crate::sites::testing::{html, http, keywords as kw, media, queued, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        let chapter = |manga: &str, chapter: Option<&str>, title: &str| {
            Some(Target::Chapter {
                manga: manga.into(),
                chapter: chapter.map(Into::into),
                title: title.into(),
            })
        };
        assert_eq!(
            t("https://allporncomic.com/porncomic/some-comic/1-part-one/"),
            chapter("some-comic", Some("1"), "-part-one")
        );
        assert_eq!(
            t("https://www.allporncomic.com/porncomic/c/2-3-extra"),
            chapter("c", Some("2-3"), "-extra")
        );
        assert_eq!(
            t("https://allporncomic.com/porncomic/c/issue-one"),
            chapter("c", None, "issue-one")
        );
        assert_eq!(
            t("https://allporncomic.com/porncomic/c/123"),
            chapter("c", Some("12"), "3")
        );
        assert_eq!(
            t("https://allporncomic.com/porncomic/c/"),
            Some(Target::Manga("c".into()))
        );
        assert_eq!(
            t("https://allporncomic.com/porncomic-genre/action/"),
            Some(Target::Tag("/porncomic-genre/action".into(), "/".into()))
        );
        assert_eq!(
            t("https://allporncomic.com/artist/someone/page/2/?m_orderby=views"),
            Some(Target::Tag(
                "/artist/someone/page/2".into(),
                "/?m_orderby=views".into()
            ))
        );
        for raw in [
            "https://allporncomic.com/",
            "https://allporncomic.com/porncomic/",
            "https://allporncomic.com/tags/x/",
            "https://m.allporncomic.com/porncomic/c/",
            "https://allporncomic.com.evil.test/porncomic/c/",
            "https://allporncomic.com:8443/porncomic/c/",
            "https://u:p@allporncomic.com/porncomic/c/",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[test]
    fn titles_lose_numbers_and_suffixes() {
        assert_eq!(manga_name("Some Comic (Series) [Artist]"), "Some Comic");
        assert_eq!(manga_name("Some (Comic) Title"), "Some (Comic) Title");
        assert_eq!(manga_name(""), "");
        assert_eq!(strip_number(" 12.5 . Title"), "Title");
        assert_eq!(strip_number("[Group] Title"), "Title");
        assert_eq!(strip_number("12 Title"), "12 Title");
        assert_eq!(
            strip_suffix("Title - Chapter 3 Extras (Series) - [Artist] "),
            "Title"
        );
        assert_eq!(strip_suffix("Title chapter 2"), "Title");
        assert_eq!(strip_suffix("Title (x) more"), "Title (x) more");
    }

    const MANGA_PAGE: &str = r#"<meta property="og:locale" content="en_US">
<meta property="og:title" content="Some &amp; Comic (Series) [Artist]">
<meta property="og:description" content="A &amp; B">
<meta property="og:image" content="https://allporncomic.com/cover.jpg">
{"datePublished":"2024-01-02T03:04:05+00:00","dateModified":"2024-02-02T03:04:05+00:00"}
<body class="page postid-4321 x">
<span class="total_votes">4.5</span><span id="countrate">12</span>
<div class="author-content"><a>Char A</a>, <a>Char B</a></div>
<div class="author-content"><a>Parody</a></div>
<div class="author-content"></div>
<div class="artist-content"><a>Artist</a></div>
<div class="genres-content"><a>Tag A</a>, <a>Tag B</a></div>
<div class="summary-content"> Comic </div><div class="summary-content"> Completed </div>
<span>3 comments</span><div class="action_detail"><span>7 Users</span></div>
<li class="wp-manga-chapter"><a href="https://allporncomic.com/porncomic/some-comic/2-part-two/">2</a><i>March 4, 2024</i></li>
<li class="wp-manga-chapter"><a href="https://allporncomic.com/porncomic/some-comic/1-part-one/">1</a></li>"#;

    const CHAPTER_PAGE: &str = r#"<ol class="breadcrumb"><li><a>Home</a></li><li><a>Comics</a></li>
<li><a>Series</a></li><li><a>Some Comic</a></li><li>1. Part One - Chapter 1</li></ol>
<input name="manga_chapter_id" value="99">
<img data-src=" https://cdn.allporncomic.com/a/01.jpg ">
<img data-src="https://cdn.allporncomic.com/a/02.png">"#;

    #[tokio::test]
    async fn extracts_chapter() {
        let (root, server) = serve(vec![html(CHAPTER_PAGE), html(MANGA_PAGE)]);
        let http = http("allporncomic", root);
        let files = media(extract(
            &http,
            Url::parse("https://allporncomic.com/porncomic/some-comic/1-part-one/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].url.as_str(),
            "https://cdn.allporncomic.com/a/01.jpg"
        );
        let k = kw(&files[1]);
        assert_eq!(k["manga"], "Some & Comic");
        assert_eq!(k["description"], "A & B");
        assert_eq!(k["manga_id"], 4321);
        assert_eq!(k["manga_date"], "2024-01-02 03:04:05");
        assert_eq!(k["characters"], json!(["Char A", "Char B"]));
        assert_eq!(k["parody"], json!(["Parody"]));
        assert_eq!(k["group"], json!([]));
        assert_eq!(k["tags"], json!(["Tag A", "Tag B"]));
        assert_eq!(k["type"], "Comic");
        assert_eq!(k["status"], "Completed");
        assert_eq!(k["votes"], 12);
        assert_eq!(k["comments"], 3);
        assert_eq!(k["bookmarks"], 7);
        assert_eq!(k["lang"], "en");
        assert_eq!(
            k["path"],
            json!(["Series", "Some Comic", "1. Part One - Chapter 1"])
        );
        assert_eq!(k["title"], "Part One");
        assert_eq!(k["title_slug"], "part-one");
        assert_eq!(k["chapter"], 1);
        assert_eq!(k["chapter_minor"], "");
        assert_eq!(k["chapter_id"], 99);
        assert_eq!(k["media"], "image");
        assert_eq!(k["page"], 2);
        assert_eq!(
            path(&files[1]),
            "allporncomic/Series/Some Comic/Part One/002.png"
        );
        assert_eq!(
            archive(&files[1]),
            r#"["{manga_id}_{chapter_id}_{page}",["4321","99","2"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /porncomic/some-comic/1-part-one/ "));
        assert!(requests[1].starts_with("GET /porncomic/some-comic/ "));
    }

    #[tokio::test]
    async fn manga_queues_chapters_oldest_first() {
        let (root, server) = serve(vec![html(MANGA_PAGE)]);
        let http = http("allporncomic", root);
        let children = queued(extract(
            &http,
            Url::parse("https://allporncomic.com/porncomic/some-comic/").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].subcategory(), "manga");
        assert_eq!(children[1].fields()["description"], "A & B");
        assert!(server.join().unwrap()[0].starts_with("GET /porncomic/some-comic/ "));
    }

    #[tokio::test]
    async fn tag_follows_next_links() {
        let first = r#"<div id="manga-item-1"><a href="https://allporncomic.com/porncomic/a/">a</a></div>
<link rel="next" href="https://allporncomic.com/porncomic-genre/x/page/2/">"#;
        let second =
            r#"<div id="manga-item-2"><a href="https://allporncomic.com/porncomic/b/">b</a></div>"#;
        let (root, server) = serve(vec![html(first), html(second)]);
        let http = http("allporncomic", root);
        let children = queued(extract(
            &http,
            Url::parse("https://allporncomic.com/porncomic-genre/x").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 2);
        assert_eq!(children[1].subcategory(), "tag");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /porncomic-genre/x/ "));
        assert!(requests[1].starts_with("GET /porncomic-genre/x/page/2/ "));
    }
}
