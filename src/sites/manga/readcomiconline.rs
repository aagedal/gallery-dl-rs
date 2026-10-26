//! ReadComicOnline issues, comics and tag listings, mirroring
//! gallery_dl/extractor/readcomiconline.py (including its `baeu` image
//! URL decoding and CAPTCHA redirect detection).
use super::{Gallery, set};
use crate::error;
use crate::sites::gdl::{
    Scan, captures, extr, extract_iter, group, parse_int, parse_query, partition, re, rpartition,
    unescape, unquote,
};
use crate::sites::{Events, Http, Route, Site};
use base64::Engine;
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://readcomiconline.li";
const BASE: &str = r"(?i)^https?://(?:www\.)?readcomiconline\.(?:li|to)";
const FILENAME: &str = "{comic}_{issue:>03}_{page:>03}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{comic}", "{issue:>03}"];
const ARCHIVE: &str = "{issue_id}_{page}";

static ISSUE: Route = Route::new("issue", FILENAME, DIRECTORY, ARCHIVE);
static COMIC: Route = Route::new("comic", FILENAME, DIRECTORY, ARCHIVE);
static ARTIST: Route = Route::new("artist", FILENAME, DIRECTORY, ARCHIVE);
static GENRE: Route = Route::new("genre", FILENAME, DIRECTORY, ARCHIVE);
static PUBLISHER: Route = Route::new("publisher", FILENAME, DIRECTORY, ARCHIVE);
static WRITER: Route = Route::new("writer", FILENAME, DIRECTORY, ARCHIVE);
static TAG: Route = Route::new("", FILENAME, DIRECTORY, ARCHIVE);

pub static SITE: Site = Site {
    category: "readcomiconline",
    name: "ReadComicOnline",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&ISSUE, &COMIC, &ARTIST, &GENRE, &PUBLISHER, &WRITER, &TAG],
    api_key: false,
    cookies: Some("readcomiconline.li"),
};

#[derive(Debug, PartialEq)]
enum Target {
    /// Path (ending in `?`) and query.
    Issue(String, String),
    Comic(String),
    /// Listing path, kind and name.
    Tag(String, String, String),
}

fn target(url: &Url) -> Option<Target> {
    let issue = re!(&format!(r"{BASE}(/Comic/[^/?#]+/[^/?#]+\?)([^#]+)"));
    if let Some(c) = captures(issue, url) {
        return Some(Target::Issue(group(&c, 1).into(), group(&c, 2).into()));
    }
    let comic = re!(&format!(r"{BASE}(/Comic/[^/?#]+/?)$"));
    if let Some(c) = captures(comic, url) {
        return Some(Target::Comic(group(&c, 1).into()));
    }
    let tag = re!(&format!(
        r"{BASE}(/(Artist|Genre|Publisher|Writer|)/([^/?#]+)(?:/[^/?#]+)?)"
    ));
    captures(tag, url).map(|c| {
        Target::Tag(
            group(&c, 1).into(),
            group(&c, 2).into(),
            group(&c, 3).into(),
        )
    })
}

/// Requests that fail on redirects to the CAPTCHA page, as upstream's
/// `request()` does with `captcha = "stop"`.
async fn request(http: &Http, url: &Url, headers: &[(&str, &str)]) -> crate::Result<String> {
    let (page, final_url) = http.text_final(url, headers).await?;
    if final_url.path().contains("/AreYouHuman") {
        return Err(error(
            "ReadComicOnline redirected to a CAPTCHA; solve it in your browser to continue",
        ));
    }
    Ok(page)
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported ReadComicOnline URL"));
    };
    match target {
        Target::Issue(path, query) => crate::sites::once(async move {
            let mut params = parse_query(&query);
            match http.option("quality") {
                None | Some(Value::Null) => {
                    params.entry("quality").or_insert_with(|| "hq".into());
                }
                Some(Value::String(q)) if q == "auto" => {
                    params.entry("quality").or_insert_with(|| "hq".into());
                }
                Some(quality) => {
                    set(&mut params, "quality", crate::sites::scalar(quality));
                }
            }
            set(&mut params, "readType", "0");
            let query = params
                .iter()
                .map(|(k, v)| format!("{k}={}", crate::sites::scalar(v)))
                .collect::<Vec<_>>()
                .join("&");
            let page_url = format!("{ROOT}{path}{query}");
            let page = request(http, &http.api(&page_url)?, &[]).await?;
            let issue_id = params
                .get("id")
                .map(crate::sites::scalar)
                .unwrap_or_default();
            let data = metadata(&page, &issue_id);
            let pages = images(&page);
            Ok(crate::sites::events(
                Gallery::chapter(&ISSUE, data, pages)
                    .header("Referer", page_url)
                    .events(&SITE, http),
            ))
        }),
        Target::Comic(path) => crate::sites::once(async move {
            let page = request(http, &http.api(&format!("{ROOT}{path}"))?, &[]).await?;
            Ok(super::queue_chapters(
                http,
                &SITE,
                &COMIC,
                chapters(&page),
                true,
            ))
        }),
        Target::Tag(path, kind, name) => {
            let route: &'static Route = match kind.to_lowercase().as_str() {
                "artist" => &ARTIST,
                "genre" => &GENRE,
                "publisher" => &PUBLISHER,
                "writer" => &WRITER,
                _ => &TAG,
            };
            let mut data = Map::new();
            set(&mut data, "search_tags", unquote(&name));
            crate::sites::paginate(Some(1u64), move |state| {
                let path = path.clone();
                let data = data.clone();
                async move {
                    let Some(number) = state else { return Ok(None) };
                    let root = http.api(ROOT)?;
                    http.set_cookie(&root, "list-view=list; Domain=readcomiconline.li; Path=/");
                    let mut url = http.api(&format!("{ROOT}{path}"))?;
                    url.query_pairs_mut()
                        .append_pair("page", &number.to_string());
                    let page = request(http, &url, &[]).await?;
                    let comics = extract_iter(&page, "<a href=\"/Comic/", "\"")
                        .into_iter()
                        .filter(|href| !href.contains("?id="))
                        .filter_map(|href| Url::parse(&format!("{ROOT}/Comic/{href}")).ok())
                        .map(|url| (url, data.clone()))
                        .collect();
                    let next = page.contains(">&rsaquo; Next <").then_some(number + 1);
                    Ok(Some((super::queue_urls(http, &SITE, route, comics), next)))
                }
            })
        }
    }
}

fn metadata(page: &str, issue_id: &str) -> Map<String, Value> {
    let mut scan = Scan::new(page);
    let comic = scan.extr("   - Read\r\n    ", "\r\n");
    let info = scan.extr("    ", "\r\n");
    let issue = re!(r"^(?:(?:Issue )?#(\d+)|(.+))")
        .captures(info)
        .map(|c| {
            c.get(1)
                .or_else(|| c.get(2))
                .map_or("", |m| m.as_str())
                .to_owned()
        })
        .unwrap_or_default();
    let mut data = Map::new();
    set(&mut data, "comic", comic);
    set(&mut data, "issue", issue);
    set(&mut data, "issue_id", parse_int(issue_id));
    set(&mut data, "lang", "en");
    set(&mut data, "language", "English");
    data
}

fn images(page: &str) -> Vec<super::Page> {
    let mut scan = Scan::new(page);
    let root = scan.extr("return baeu(l, '", "'").to_owned();
    scan.extr("var pth = '", "");
    let var = scan.extr("var ", "= '").to_owned();
    if var.is_empty() {
        return Vec::new();
    }
    let replacements: Vec<(String, String)> = re!(r#"l = l\.replace\(/([^/]+)/g, ["']([^"']*)"#)
        .captures_iter(page)
        .map(|c| (group(&c, 1).to_owned(), group(&c, 2).to_owned()))
        .collect();
    page.split(var.as_str())
        .skip(2)
        .map(|part| {
            let mut path = extr(part, "= '", "'").to_owned();
            for (needle, replacement) in &replacements {
                path = path.replace(needle.as_str(), replacement);
            }
            (baeu(&path, &root), None)
        })
        .collect()
}

/// Python `binascii.a2b_base64` (non-strict: other characters are skipped).
fn a2b_base64(text: &str) -> Vec<u8> {
    let mut clean: String = text
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/'))
        .collect();
    while !clean.len().is_multiple_of(4) {
        clean.push('=');
    }
    base64::engine::general_purpose::STANDARD
        .decode(clean)
        .unwrap_or_default()
}

/// `baeu`: decode an obfuscated image path (`rguard.min.js`).
fn baeu(url: &str, root: &str) -> String {
    const BLOGSPOT: &str = "https://2.bp.blogspot.com";
    let root = if root.is_empty() { BLOGSPOT } else { root };
    let url = url.replace("pw_.g28x", "b").replace("d2pr.x_27", "h");
    if url.starts_with("https") {
        return url.replacen(BLOGSPOT, root, 1);
    }
    let (path, sep, query) = partition(&url, "?");
    let s0 = path.contains("=s0");
    let chars: Vec<char> = path.chars().collect();
    let slice = |v: &[char], a: usize, b: usize| -> String {
        v[a.min(v.len())..b.min(v.len()).max(a.min(v.len()))]
            .iter()
            .collect()
    };
    let cut = chars.len().saturating_sub(if s0 { 3 } else { 6 });
    let path: Vec<char> = chars[..cut].to_vec();
    let path: Vec<char> = (slice(&path, 15, 33) + &slice(&path, 50, path.len()))
        .chars()
        .collect();
    let len = path.len();
    let path = slice(&path, 0, len.saturating_sub(11)) + &slice(&path, len.saturating_sub(2), len);
    let decoded: Vec<char> = String::from_utf8_lossy(&a2b_base64(&path))
        .chars()
        .collect();
    let decoded = slice(&decoded, 0, 13) + &slice(&decoded, 17, decoded.len());
    let mut decoded: Vec<char> = decoded.chars().collect();
    decoded.truncate(decoded.len().saturating_sub(2));
    let decoded: String = decoded.into_iter().collect();
    format!(
        "{root}/{decoded}{}{sep}{query}",
        if s0 { "=s0" } else { "=s1600" }
    )
}

fn chapters(page: &str) -> Vec<(Url, Map<String, Value>)> {
    let mut scan = Scan::new(page);
    let comic = scan.extr(" class=\"barTitle\">", "<");
    let listing = scan.extr(" class=\"listing\">", "</table>");
    let comic = rpartition(comic, "information").0.trim().to_owned();
    let needle = format!(" title=\"Read {comic} ");
    let comic = unescape(&comic);
    extract_iter(listing, " href=\"", " comic online ")
        .into_iter()
        .filter_map(|item| {
            let (url, _, issue) = partition(item, &needle);
            let url = rpartition(url, "\"").0;
            let issue = issue.strip_prefix("Issue #").unwrap_or(issue);
            let mut data = Map::new();
            set(&mut data, "comic", comic.as_str());
            set(&mut data, "issue", issue);
            set(&mut data, "issue_id", parse_int(rpartition(url, "=").2));
            set(&mut data, "lang", "en");
            set(&mut data, "language", "English");
            Some((Url::parse(&format!("{ROOT}{url}")).ok()?, data))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::tests::{archive, path};
    use super::*;
    use crate::sites::testing::{children, html, http, keywords, media, serve};

    const ENCODED: &str = "aaaaaaaaaaaaaaaQUJDREVGR0hJSktMTXbbbbbbbbbbbbbbbbbB3L0FiQ2RFZkdoLUlqS2xNbk9fcFFyU3RVdld4WXowMTIzNDU2Nzg5eHQQQQQQQQQg==s0?rhlupa=1&x=2";

    #[test]
    fn decodes_like_upstream() {
        assert_eq!(
            baeu(ENCODED, "https://2.bp.blogspot.com"),
            "https://2.bp.blogspot.com/ABCDEFGHIJKLMbCdEfGh-IjKlMnO_pQrStUvWxYz0123456789=s0?rhlupa=1&x=2"
        );
        assert_eq!(
            baeu(&ENCODED.replace("=s0", "xxxxxx"), ""),
            "https://2.bp.blogspot.com/ABCDEFGHIJKLMbCdEfGh-IjKlMnO_pQrStUvWxYz0123456789=s1600?rhlupa=1&x=2"
        );
        assert_eq!(
            baeu("https://2.bp.blogspot.com/abc=s0", "https://img.test"),
            "https://img.test/abc=s0"
        );
    }

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://readcomiconline.li/Comic/W-i-t-c-h/Issue-130?id=22289"),
            Some(Target::Issue(
                "/Comic/W-i-t-c-h/Issue-130?".into(),
                "id=22289".into()
            ))
        );
        assert_eq!(
            t("https://readcomiconline.to/Comic/Bazooka-Jules"),
            Some(Target::Comic("/Comic/Bazooka-Jules".into()))
        );
        assert_eq!(
            t("https://readcomiconline.li/Artist/Simone-Buonfantino/LatestUpdate"),
            Some(Target::Tag(
                "/Artist/Simone-Buonfantino/LatestUpdate".into(),
                "Artist".into(),
                "Simone-Buonfantino".into()
            ))
        );
        assert_eq!(
            t("https://READCOMICONLINE.LI/Comic/x"),
            Some(Target::Comic("/Comic/x".into()))
        );
        assert_eq!(t("https://readcomiconline.li/Comic/x/y"), None);
    }

    #[tokio::test]
    async fn extracts_issue() {
        let page = format!(
            "<title>\r\n   - Read\r\n    W.i.t.c.h.\r\n    Issue #130\r\n</title>\
             <script>function f(l) {{ l = l.replace(/__x__/g, 'a'); return baeu(l, 'https://2.bp.blogspot.com'); }}\
             var pth = ''; var qw = ''; qw = '{ENCODED}'; qw = '{}';</script>",
            ENCODED.replacen("QUJD", "__x__UJD", 1)
        );
        let (root, server) = serve(vec![html(&page)]);
        let http = http("readcomiconline", root);
        let files = media(extract(
            &http,
            Url::parse("https://readcomiconline.li/Comic/W-i-t-c-h/Issue-130?id=22289&readType=1")
                .unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert!(
            files[0]
                .url
                .as_str()
                .starts_with("https://2.bp.blogspot.com/ABCDEFGHIJKLM")
        );
        assert_eq!(
            files[0].headers["Referer"],
            "https://readcomiconline.li/Comic/W-i-t-c-h/Issue-130?id=22289&readType=0&quality=hq"
        );
        let k = keywords(&files[1]);
        assert_eq!(k["comic"], "W.i.t.c.h.");
        assert_eq!(k["issue"], "130");
        assert_eq!(k["issue_id"], 22289);
        assert_eq!(
            path(&files[1]),
            "readcomiconline/W.i.t.c.h./130/W.i.t.c.h._130_002."
        );
        assert_eq!(archive(&files[0]), r#"["{issue_id}_{page}",["22289","1"]]"#);
        let requests = server.join().unwrap();
        assert!(
            requests[0]
                .starts_with("GET /Comic/W-i-t-c-h/Issue-130?id=22289&readType=0&quality=hq ")
        );
    }

    #[tokio::test]
    async fn comic_lists_issues() {
        let page = r#"<div class="barTitle">W.i.t.c.h. information</div><table class="listing">
            <a href="/Comic/W-i-t-c-h/Issue-2?id=2" title="Read W.i.t.c.h. Issue #2 comic online free">
            <a href="/Comic/W-i-t-c-h/Annual?id=9" title="Read W.i.t.c.h. Annual comic online free"></table>"#;
        let list = chapters(page);
        assert_eq!(list.len(), 2);
        assert_eq!(
            list[0].0.as_str(),
            "https://readcomiconline.li/Comic/W-i-t-c-h/Issue-2?id=2"
        );
        assert_eq!(list[0].1["issue"], "2");
        assert_eq!(list[1].1["issue"], "Annual");
        assert_eq!(list[1].1["issue_id"], 9);
        let (root, server) = serve(vec![html(page)]);
        let http = http("readcomiconline", root);
        assert_eq!(
            children(extract(
                &http,
                Url::parse("https://readcomiconline.li/Comic/W-i-t-c-h").unwrap()
            ))
            .await
            .len(),
            2
        );
        server.join().unwrap();
    }
}
