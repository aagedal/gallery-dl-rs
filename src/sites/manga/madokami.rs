//! Madokami manga directories, mirroring gallery_dl/extractor/madokami.py:
//! one file per listed chapter archive.
//!
//! The site needs HTTP basic authentication; like upstream's
//! `_get_auth_info`, the `username` and `password` options of the
//! `madokami` category supply it (a `.netrc` is not consulted). The
//! credentials are sent with the directory page and each file request.
use super::{keywords, set, text};
use crate::sites::art::{Scan, split_html};
use crate::sites::boorus::{parse_bytes, per_post};
use crate::sites::{Events, Http, Item, Route, Site, unescape};
use crate::{Result, error};
use base64::Engine;
use serde_json::{Map, Value};
use url::Url;

pub static SITE: Site = Site {
    category: "madokami",
    name: "Madokami",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&MANGA],
    api_key: false,
    cookies: None,
};

const ROOT: &str = "https://manga.madokami.al";
const HOST: &str = "manga.madokami.al";

static MANGA: Route = Route::new(
    "manga",
    "{filename}.{extension}",
    &["{category}", "{manga}"],
    "{chapter_id}",
);

/// Whether a raw path segment is `\w{n}`.
fn word(segment: &str, n: usize) -> bool {
    segment.len() == n
        && segment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// `manga\.madokami\.al/Manga/(\w/\w{2}/\w{4}/.+)`: the raw text after
/// `/Manga/`, query included.
fn target(url: &Url) -> Option<String> {
    if !crate::sites::plain(url) || url.host_str() != Some(HOST) {
        return None;
    }
    let path = url.path().strip_prefix("/Manga/")?;
    let mut parts = path.splitn(4, '/');
    let (a, b, c, rest) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    let query = url.query().map(|q| format!("?{q}")).unwrap_or_default();
    (word(a, 1) && word(b, 2) && word(c, 4) && !(rest.is_empty() && query.is_empty()))
        .then(|| format!("{path}{query}"))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(path) => crate::sites::once(async move { manga_events(http, &path).await }),
        None => crate::sites::fail(error("unsupported Madokami URL")),
    }
}

/// The `Authorization` value for the configured credentials.
fn authorization(http: &Http) -> Result<String> {
    let username = http
        .option_str("username")
        .filter(|u| !u.is_empty())
        .ok_or_else(|| error("Madokami requires a username & password"))?;
    let password = http
        .option("password")
        .map(crate::sites::scalar)
        .unwrap_or_default();
    let token = base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
    Ok(format!("Basic {token}"))
}

async fn manga_events<'a>(http: &'a Http, path: &str) -> Result<Events<'a>> {
    let auth = authorization(http)?;
    let page = http
        .text_with(
            &http.api(&format!("{ROOT}/Manga/{path}"))?,
            &http.browser_headers(&[("Authorization", &auth)]),
        )
        .await?;
    let mut items = Vec::new();
    let (common, mut chapters) = parse(&page);
    if http.option_bool("chapter-reverse", false) {
        chapters.reverse();
    }
    for (mut chapter, timestamp) in chapters {
        let raw = format!(
            "{ROOT}{}",
            chapter
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or_default()
        );
        let Some(url) = Url::parse(&raw)
            .ok()
            .filter(|url| crate::sites::plain(url) && url.host_str() == Some(HOST))
        else {
            continue;
        };
        let (name, ext) = crate::sites::nameext(&url);
        set(&mut chapter, "filename", name);
        set(&mut chapter, "extension", ext);
        let id = chapter
            .get("chapter_id")
            .map(crate::sites::scalar)
            .unwrap_or_default();
        let caption = chapter
            .get("chapter_string")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let mut data = common.clone();
        crate::sites::art::update(&mut data, &chapter);
        items.push(
            Item::new(url, data)
                .ids(id.clone(), id)
                .caption(caption)
                .timestamp(timestamp)
                .header("Authorization", &auth),
        );
    }
    Ok(crate::sites::events(per_post(&SITE, http, &MANGA, items)))
}

/// The directory page's manga-wide keywords and its chapters with their
/// timestamps.
type Chapter = (Map<String, Value>, Option<i64>);

fn parse(page: &str) -> (Map<String, Value>, Vec<Chapter>) {
    let mut e = Scan::new(page);
    let mut chapters = Vec::new();
    loop {
        let id = e.extr("<tr data-record=\"", "\"");
        if id.is_empty() {
            break;
        }
        let path = unescape(e.extr("href=\"", "\""));
        let chstr = unescape(e.extr(">", "<"));
        let size = parse_bytes(e.extr("<td>", "</td>"));
        let timestamp = text::iso(e.extr("<td>", "</td>").trim());
        let mut chapter = keywords([
            ("chapter_id", text::parse_int(id).into()),
            ("path", path.into()),
            ("chapter_string", chstr.clone().into()),
            ("size", size.into()),
            ("date", text::date(timestamp)),
        ]);
        chapter_numbers(&chstr, &mut chapter);
        chapters.push((chapter, timestamp));
    }
    let manga = unescape(e.extr("itemprop=\"name\">", "<"));
    let year = text::parse_int(e.extr("itemprop=\"datePublished\" content=\"", "-"));
    let author: Vec<String> = split_html(e.extr("<p class=\"staff", "</p>"))
        .into_iter()
        .skip(1)
        .step_by(2)
        .collect();
    let genre = split_html(e.extr("<h3>Genres</h3>", "</div>"));
    let tags = split_html(e.extr("<h3>Tags</h3>", "</div>"));
    let complete = e
        .extr("span class=\"scanstatus\">", "<")
        .eq_ignore_ascii_case("yes");
    let common = keywords([
        ("manga", manga.into()),
        ("year", year.into()),
        ("author", author.into()),
        ("genre", genre.into()),
        ("tags", tags.into()),
        ("complete", complete.into()),
    ]);
    (common, chapters)
}

/// `\s*` from byte `i`.
fn spaces(s: &str, i: usize) -> usize {
    i + s[i..].len() - s[i..].trim_start().len()
}

/// `\d+` from byte `i`: its end, if any digit.
fn digits(s: &str, i: usize) -> Option<usize> {
    let len = s[i..].bytes().take_while(u8::is_ascii_digit).count();
    (len > 0).then_some(i + len)
}

/// Case-insensitive ASCII `prefix` at byte `i`: the byte after it.
fn tag(s: &str, i: usize, prefix: &str) -> Option<usize> {
    let end = i + prefix.len();
    s.get(i..end)
        .filter(|t| t.eq_ignore_ascii_case(prefix))
        .map(|_| end)
}

/// `ch?\.?\s*(\d+)(?:-(\d+))?` at byte `i`: end, chapter and end range.
fn chapter_at(s: &str, i: usize) -> Option<(usize, &str, &str)> {
    let i = tag(s, i, "c")?;
    let i = tag(s, i, "h").unwrap_or(i);
    let i = tag(s, i, ".").unwrap_or(i);
    let start = spaces(s, i);
    let stop = digits(s, start)?;
    let (end, range) = match tag(s, stop, "-").and_then(|i| digits(s, i).map(|j| (i, j))) {
        Some((i, j)) => (j, &s[i..j]),
        None => (stop, ""),
    };
    Some((end, &s[start..stop], range))
}

/// `(?:v(?:ol)?\.?\s*(\d+))(?:\s+ch?\.?\s*(\d+)(?:-(\d+))?)?` at byte
/// `i`: end, volume, chapter and end range.
fn volume_at(s: &str, i: usize) -> Option<(usize, &str, &str, &str)> {
    let i = tag(s, i, "v")?;
    let i = tag(s, i, "ol").unwrap_or(i);
    let i = tag(s, i, ".").unwrap_or(i);
    let start = spaces(s, i);
    let stop = digits(s, start)?;
    let volume = &s[start..stop];
    let gap = spaces(s, stop);
    match chapter_at(s, gap).filter(|_| gap > stop) {
        Some((end, chapter, range)) => Some((end, volume, chapter, range)),
        None => Some((stop, volume, "", "")),
    }
}

/// The volume and chapter searches over `chapter_string`; a match replaces
/// `chapter_string` with the matched text.
fn chapter_numbers(chstr: &str, chapter: &mut Map<String, Value>) {
    let starts = || chstr.char_indices().map(|(i, _)| i);
    let found = starts()
        .find_map(|i| volume_at(chstr, i).map(|(end, v, c, r)| (i, end, v, c, r)))
        .or_else(|| {
            starts().find_map(|i| chapter_at(chstr, i).map(|(end, c, r)| (i, end, "", c, r)))
        });
    match found {
        Some((start, end, volume, number, range)) => {
            set(chapter, "chapter_string", &chstr[start..end]);
            set(chapter, "volume", text::parse_int(volume));
            set(chapter, "chapter", text::parse_int(number));
            set(chapter, "chapter_end", text::parse_int(range));
        }
        None => {
            set(chapter, "volume", 0);
            set(chapter, "chapter", 0);
            set(chapter, "chapter_end", 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{archive, path};
    use super::*;
    use crate::sites::testing::{html, http, keywords as kw, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://manga.madokami.al/Manga/A/AK/AKEB/Akebi-chan%20no%20Sailor-fuku"),
            Some("A/AK/AKEB/Akebi-chan%20no%20Sailor-fuku".into())
        );
        assert_eq!(
            t("http://manga.madokami.al/Manga/_/__/____/x/y?z=1"),
            Some("_/__/____/x/y?z=1".into())
        );
        for raw in [
            "https://manga.madokami.al/Manga/A/AK/AKEB/",
            "https://manga.madokami.al/Manga/A/AKX/AKEB/x",
            "https://manga.madokami.al/Raws/A/AK/AKEB/x",
            "https://madokami.al/Manga/A/AK/AKEB/x",
            "https://manga.madokami.al.evil.test/Manga/A/AK/AKEB/x",
            "https://manga.madokami.al:8443/Manga/A/AK/AKEB/x",
            "https://u:p@manga.madokami.al/Manga/A/AK/AKEB/x",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[test]
    fn parses_chapter_strings() {
        let parse = |s: &str| {
            let mut m = Map::new();
            chapter_numbers(s, &mut m);
            (
                m.get("chapter_string").cloned(),
                m["volume"].clone(),
                m["chapter"].clone(),
                m["chapter_end"].clone(),
            )
        };
        assert_eq!(
            parse("Akebi v01 c001-006 [Group].zip"),
            (Some("v01 c001-006".into()), 1.into(), 1.into(), 6.into())
        );
        assert_eq!(
            parse("Title Vol. 3 Ch.12.cbz"),
            (Some("Vol. 3 Ch.12".into()), 3.into(), 12.into(), 0.into())
        );
        assert_eq!(
            parse("Title v2.cbz"),
            (Some("v2".into()), 2.into(), 0.into(), 0.into())
        );
        assert_eq!(
            parse("Oneshot c05-07.zip"),
            (Some("c05-07".into()), 0.into(), 5.into(), 7.into())
        );
        assert_eq!(parse("Extras.zip"), (None, 0.into(), 0.into(), 0.into()));
    }

    const PAGE: &str = r#"<table id="index-table">
<tr data-record="123"><td><a href="/Manga/A/AK/AKEB/Akebi/Akebi%20v01%20c001-006.zip">Akebi &amp; v01 c001-006.zip</a></td>
<td>51.2M</td><td> 2019-04-01 12:00 </td></tr>
<tr data-record="124"><td><a href="@evil.test/x.zip">bad</a></td><td>1k</td><td>x</td></tr>
</table>
<span itemprop="name">Akebi-chan no Sailor-fuku</span>
<meta itemprop="datePublished" content="2016-01-01">
<p class="staff"><a>Hiro</a>, <a>Other</a></p>
<h3>Genres</h3><div><a>Comedy</a><a>School</a></div>
<h3>Tags</h3><div><a>Uniforms</a></div>
<span class="scanstatus">Yes</span>"#;

    #[tokio::test]
    async fn extracts_chapters_with_basic_auth() {
        let (root, requests) = serve(vec![html(PAGE)]);
        let http = http("Madokami", root).with_options(Some(std::sync::Arc::new(
            serde_json::json!({"username": "user", "password": "pass"})
                .as_object()
                .unwrap()
                .clone(),
        )));
        let files = media(extract(
            &http,
            Url::parse("https://manga.madokami.al/Manga/A/AK/AKEB/Akebi").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].url.as_str(),
            "https://manga.madokami.al/Manga/A/AK/AKEB/Akebi/Akebi%20v01%20c001-006.zip"
        );
        assert_eq!(files[0].headers["Authorization"], "Basic dXNlcjpwYXNz");
        let k = kw(&files[0]);
        assert_eq!(k["manga"], "Akebi-chan no Sailor-fuku");
        assert_eq!(k["year"], 2016);
        assert_eq!(k["author"], serde_json::json!(["Hiro", "Other"]));
        assert_eq!(k["genre"], serde_json::json!(["Comedy", "School"]));
        assert_eq!(k["tags"], serde_json::json!(["Uniforms"]));
        assert_eq!(k["complete"], true);
        assert_eq!(k["chapter_id"], 123);
        assert_eq!(k["chapter_string"], "v01 c001-006");
        assert_eq!(k["volume"], 1);
        assert_eq!(k["chapter"], 1);
        assert_eq!(k["chapter_end"], 6);
        assert_eq!(k["size"], 53687091);
        assert_eq!(k["date"], "2019-04-01 12:00:00");
        assert_eq!(k["filename"], "Akebi v01 c001-006");
        assert_eq!(k["extension"], "zip");
        assert_eq!(
            path(&files[0]),
            "madokami/Akebi-chan no Sailor-fuku/Akebi v01 c001-006.zip"
        );
        assert_eq!(archive(&files[0]), r#"["{chapter_id}",["123"]]"#);
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /Manga/A/AK/AKEB/Akebi "));
        assert!(
            requests[0]
                .to_ascii_lowercase()
                .contains("authorization: basic dxnlcjpwyxnz\r\n")
        );
    }

    #[tokio::test]
    async fn requires_credentials() {
        let http = http("Madokami", Url::parse("http://127.0.0.1:9/").unwrap());
        assert!(
            crate::sites::testing::fails(extract(
                &http,
                Url::parse("https://manga.madokami.al/Manga/A/AK/AKEB/Akebi").unwrap(),
            ))
            .await
        );
    }
}
