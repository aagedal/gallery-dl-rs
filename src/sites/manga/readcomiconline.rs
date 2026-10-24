//! ReadComicOnline issues, comics and tag listings, mirroring
//! gallery_dl/extractor/readcomiconline.py. Image URLs are obfuscated in
//! the reader script; [`baeu`] ports upstream's decoder of `rguard.min.js`.
//!
//! A redirect to the `/AreYouHuman` CAPTCHA page ends extraction with
//! upstream's message; its `captcha: "wait"` prompt is not supported. Not
//! ported: the upstream 3-6 s `request_interval`. Upstream requests tag
//! pages as `https://readcomiconline.li//Artist/…`; this port drops the
//! duplicate slash.
use super::{Gallery, keywords, text};
use crate::sites::art::{extr, extract_iter, segment};
use crate::sites::boorus::{parse_query, unquote};
use crate::sites::{Events, Http, Route, Site, unescape};
use crate::{Result, error};
use base64::Engine;
use futures_util::{StreamExt, stream};
use serde_json::{Map, Value};
use url::Url;

pub static SITE: Site = Site {
    category: "readcomiconline",
    name: "ReadComicOnline",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&ISSUE, &COMIC, &ARTIST, &GENRE, &PUBLISHER, &WRITER],
    api_key: false,
    cookies: Some("readcomiconline.li"),
};

const ROOT: &str = "https://readcomiconline.li";
const BLOGSPOT: &str = "https://2.bp.blogspot.com";
/// Listing pages fetched for one tag.
const MAX_PAGES: u32 = 1000;

/// `ReadcomiconlineBase` formats, shared by every class.
const FILENAME: &str = "{comic}_{issue:>03}_{page:>03}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{comic}", "{issue:>03}"];
const ARCHIVE: &str = "{issue_id}_{page}";

static ISSUE: Route = Route::new("issue", FILENAME, DIRECTORY, ARCHIVE);
static COMIC: Route = Route::new("comic", FILENAME, DIRECTORY, ARCHIVE);
static ARTIST: Route = Route::new("artist", FILENAME, DIRECTORY, ARCHIVE);
static GENRE: Route = Route::new("genre", FILENAME, DIRECTORY, ARCHIVE);
static PUBLISHER: Route = Route::new("publisher", FILENAME, DIRECTORY, ARCHIVE);
static WRITER: Route = Route::new("writer", FILENAME, DIRECTORY, ARCHIVE);

#[derive(Debug, PartialEq)]
enum Target {
    /// `/Comic/NAME/ISSUE` and the raw query.
    Issue { path: String, query: String },
    /// `/Comic/NAME[/]`, raw.
    Comic(String),
    /// `ReadcomiconlineTagExtractor`: its subcategory, `/KIND/NAME[/SUB]`
    /// and the raw name.
    Tag {
        subcategory: &'static str,
        path: String,
        name: String,
    },
}

/// `(?i)BASE_PATTERN(/Comic/[^/?#]+/[^/?#]+\?)([^#]+)`, then
/// `(/Comic/[^/?#]+/?)$`, then
/// `(/(Artist|Genre|Publisher|Writer|)/([^/?#]+)(?:/[^/?#]+)?)`.
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) {
        return None;
    }
    let host = url.host_str()?;
    let host = host.strip_prefix("www.").unwrap_or(host);
    if !matches!(host, "readcomiconline.li" | "readcomiconline.to") {
        return None;
    }
    let raw: Vec<&str> = url.path().split('/').skip(1).collect();
    let query = url.query().filter(|q| !q.is_empty());
    let end = url.query().is_none() && url.fragment().is_none();
    match raw.as_slice() {
        [kind, name, issue] if kind.eq_ignore_ascii_case("comic") && segment(name) => {
            if segment(issue) {
                let query = query?;
                Some(Target::Issue {
                    path: format!("/{kind}/{name}/{issue}"),
                    query: query.to_owned(),
                })
            } else if issue.is_empty() && end {
                Some(Target::Comic(format!("/{kind}/{name}/")))
            } else {
                None
            }
        }
        [kind, name] if kind.eq_ignore_ascii_case("comic") && segment(name) && end => {
            Some(Target::Comic(format!("/{kind}/{name}")))
        }
        [kind, name, rest @ ..] if segment(name) => {
            let subcategory = [&ARTIST, &GENRE, &PUBLISHER, &WRITER]
                .iter()
                .find(|route| route.subcategory.eq_ignore_ascii_case(kind))?
                .subcategory;
            let sub = rest
                .first()
                .filter(|s| segment(s))
                .map(|s| format!("/{s}"))
                .unwrap_or_default();
            Some(Target::Tag {
                subcategory,
                path: format!("/{kind}/{name}{sub}"),
                name: (*name).to_owned(),
            })
        }
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Issue { path, query }) => {
            crate::sites::once(async move { issue_events(http, &path, &query).await })
        }
        Some(Target::Comic(path)) => crate::sites::once(async move {
            let page = page(http, &format!("{ROOT}{path}")).await?;
            let mut children = issues(&page);
            // `MangaExtractor.reverse = True`.
            if !http.option_bool("chapter-reverse", false) {
                children.reverse();
            }
            Ok(super::queue_urls(http, &SITE, &COMIC, children))
        }),
        Some(Target::Tag {
            subcategory,
            path,
            name,
        }) => tag_events(http, subcategory, path, name),
        None => crate::sites::fail(error("unsupported ReadComicOnline URL")),
    }
}

/// `ReadcomiconlineBase.request`: a page, refusing CAPTCHA redirects.
async fn page(http: &Http, url: &str) -> Result<String> {
    let requested = http.api(url)?;
    let (final_url, body) = http.page(&requested, &http.browser_headers(&[])).await?;
    if final_url != requested && final_url.as_str().contains("/AreYouHuman") {
        return Err(error(format!(
            "Redirect to \n{final_url}\nVisit this URL in your browser and solve the CAPTCHA \
             to continue"
        )));
    }
    Ok(body)
}

/// `ReadcomiconlineIssueExtractor`: the reader page in "One page" mode at
/// the configured `quality`.
async fn issue_events<'a>(http: &'a Http, path: &str, query: &str) -> Result<Events<'a>> {
    let mut params = parse_query(query, false);
    match http.option("quality") {
        None | Some(Value::Null) => {
            if !params.iter().any(|(k, _)| k == "quality") {
                params.push(("quality".into(), "hq".into()));
            }
        }
        Some(Value::String(q)) if q == "auto" => {
            if !params.iter().any(|(k, _)| k == "quality") {
                params.push(("quality".into(), "hq".into()));
            }
        }
        Some(quality) => put(&mut params, "quality", crate::sites::scalar(quality)),
    }
    // Force the "One page" reading mode (#7890).
    put(&mut params, "readType", "0".into());
    let issue_id = params
        .iter()
        .find(|(k, _)| k == "id")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    let joined: Vec<String> = params.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let page_url = format!("{ROOT}{path}?{}", joined.join("&"));
    let page = page(http, &page_url).await?;
    let pages = images(&page)?;
    Ok(crate::sites::events(
        Gallery::chapter(&ISSUE, metadata(&page, &issue_id), pages)
            .id(issue_id)
            .header("Referer", page_url)
            .events(&SITE, http),
    ))
}

/// Python `dict[key] = value` on ordered pairs.
fn put(params: &mut Vec<(String, String)>, key: &str, value: String) {
    match params.iter_mut().find(|(k, _)| k == key) {
        Some((_, v)) => *v = value,
        None => params.push((key.to_owned(), value)),
    }
}

/// `ReadcomiconlineIssueExtractor.metadata`.
fn metadata(page: &str, issue_id: &str) -> Map<String, Value> {
    let (comic, pos) = text::extract(page, "   - Read\r\n    ", "\r\n", 0);
    let (info, _) = text::extract(page, "    ", "\r\n", pos);
    let info = info.unwrap_or_default();
    // `(?:Issue )?#(\d+)|(.+)`
    let number = info
        .strip_prefix("Issue ")
        .unwrap_or(info)
        .strip_prefix('#');
    let digits = number.map_or(0, |n| n.bytes().take_while(u8::is_ascii_digit).count());
    let issue = match number {
        Some(number) if digits > 0 => &number[..digits],
        _ => info.split('\n').next().unwrap_or_default(),
    };
    keywords([
        ("comic", comic.unwrap_or_default().into()),
        ("issue", issue.into()),
        ("issue_id", text::parse_int(issue_id).into()),
        ("lang", "en".into()),
        ("language", "English".into()),
    ])
}

/// `ReadcomiconlineIssueExtractor.images`: the obfuscated paths assigned to
/// the reader's image variable, with the script's literal replacements,
/// decoded by [`baeu`].
fn images(page: &str) -> Result<Vec<super::Page>> {
    let (root, pos) = text::extract(page, "return baeu(l, '", "'", 0);
    let (_, pos) = text::extract(page, "var pth = '", "", pos);
    let Some(var) = text::extract(page, "var ", "= '", pos).0 else {
        return Ok(Vec::new());
    };
    let replacements = replacements(page);
    let mut pages = Vec::new();
    for part in page.split(var).skip(2) {
        let mut path = extr(part, "= '", "'").to_owned();
        for (needle, replacement) in &replacements {
            path = path.replace(needle, replacement);
        }
        pages.push((baeu(&path, root.unwrap_or_default())?, None));
    }
    Ok(pages)
}

/// `l = l\.replace\(/([^/]+)/g, ["']([^"']*)` matches: needle and
/// replacement.
fn replacements(page: &str) -> Vec<(&str, &str)> {
    let mut found = Vec::new();
    for part in page.split("l = l.replace(/").skip(1) {
        let Some((needle, rest)) = part.split_once('/') else {
            continue;
        };
        let Some(rest) = rest.strip_prefix("g, ") else {
            continue;
        };
        let Some(rest) = rest.strip_prefix(['"', '\'']) else {
            continue;
        };
        if needle.is_empty() {
            continue;
        }
        let end = rest.find(['"', '\'']).unwrap_or(rest.len());
        found.push((needle, &rest[..end]));
    }
    found
}

/// Python `s[start:end]` on characters.
fn slice(s: &str, start: isize, end: Option<isize>) -> String {
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len() as isize;
    let clamp = |i: isize| (if i < 0 { len + i } else { i }).clamp(0, len) as usize;
    let (start, end) = (clamp(start), clamp(end.unwrap_or(len)));
    if start >= end {
        return String::new();
    }
    chars[start..end].iter().collect()
}

/// `baeu` from `rguard.min.js`: an image URL from its obfuscated form.
fn baeu(url: &str, root: &str) -> Result<String> {
    let root = if root.is_empty() { BLOGSPOT } else { root };
    let url = url.replace("pw_.g28x", "b").replace("d2pr.x_27", "h");
    if url.starts_with("https") {
        return Ok(url.replacen(BLOGSPOT, root, 1));
    }
    let (path, sep, query) = text::partition(&url, "?");
    let s0 = path.contains("=s0");
    let path = slice(path, 0, Some(if s0 { -3 } else { -6 }));
    let path = slice(&path, 15, Some(33)) + &slice(&path, 50, None);
    let path = slice(&path, 0, Some(-11)) + &slice(&path, -2, None);
    // `binascii.a2b_base64` skips characters outside the alphabet.
    let encoded: String = path
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '='))
        .collect();
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .ok_or_else(|| error("ReadComicOnline image path is not valid base64"))?;
    let path = slice(&decoded, 0, Some(13)) + &slice(&decoded, 17, None);
    let path = slice(&path, 0, Some(-2)) + if s0 { "=s0" } else { "=s1600" };
    Ok(format!("{root}/{path}{sep}{query}"))
}

/// `ReadcomiconlineComicExtractor.chapters`, in listing order.
fn issues(page: &str) -> Vec<(Url, Map<String, Value>)> {
    let (comic, pos) = text::extract(page, " class=\"barTitle\">", "<", 0);
    let (listing, _) = text::extract(page, " class=\"listing\">", "</table>", pos);
    let comic = comic
        .unwrap_or_default()
        .rsplit_once("information")
        .map_or("", |(head, _)| head)
        .trim();
    let needle = format!(" title=\"Read {comic} ");
    let comic = unescape(comic);
    extract_iter(listing.unwrap_or_default(), " href=\"", " comic online ")
        .into_iter()
        .filter_map(|item| {
            let (url, _, issue) = text::partition(item, &needle);
            let url = url.rsplit_once('"').map_or("", |(head, _)| head);
            let issue = issue.strip_prefix("Issue #").unwrap_or(issue);
            let issue_id = text::parse_int(url.rsplit_once('=').map_or(url, |(_, id)| id));
            Some((
                Url::parse(&format!("{ROOT}{url}")).ok()?,
                keywords([
                    ("comic", comic.clone().into()),
                    ("issue", issue.into()),
                    ("issue_id", issue_id.into()),
                    ("lang", "en".into()),
                    ("language", "English".into()),
                ]),
            ))
        })
        .collect()
}

/// `ReadcomiconlineTagExtractor.items`: every listed comic, page by page
/// in list view, until a page lacks a "Next" link.
fn tag_events<'a>(
    http: &'a Http,
    subcategory: &'static str,
    path: String,
    name: String,
) -> Events<'a> {
    let data = keywords([("search_tags", unquote(&name).into())]);
    if let Ok(url) = http.api(ROOT) {
        http.set_cookie(&url, "list-view=list");
    }
    crate::sites::paginate(Some(1), move |number: Option<u32>| {
        let path = path.clone();
        let data = data.clone();
        async move {
            let Some(number) = number else {
                return Ok(None);
            };
            let page = page(http, &format!("{ROOT}{path}?page={number}")).await?;
            let children: Vec<Url> = extract_iter(&page, "<a href=\"/Comic/", "\"")
                .into_iter()
                .filter(|href| !href.contains("?id="))
                .filter_map(|href| Url::parse(&format!("{ROOT}/Comic/{href}")).ok())
                .filter(|url| matches!(target(url), Some(Target::Comic(_))))
                .collect();
            let next =
                (page.contains(">&rsaquo; Next <") && number < MAX_PAGES).then_some(number + 1);
            let events = stream::iter(children)
                .map(move |url| crate::sites::queue(http, SITE.category, subcategory, url, &data))
                .flatten()
                .boxed();
            Ok(Some((events, next)))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::super::tests::{archive, path};
    use super::*;
    use crate::sites::testing::{html, http, keywords as kw, media, queued, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://readcomiconline.li/Comic/W-i-t-c-h/Issue-130?id=22289"),
            Some(Target::Issue {
                path: "/Comic/W-i-t-c-h/Issue-130".into(),
                query: "id=22289".into(),
            })
        );
        assert_eq!(
            t("http://www.READCOMICONLINE.to/comic/X/Annual?id=1&quality=lq#x"),
            Some(Target::Issue {
                path: "/comic/X/Annual".into(),
                query: "id=1&quality=lq".into(),
            })
        );
        assert_eq!(
            t("https://readcomiconline.li/Comic/W-i-t-c-h"),
            Some(Target::Comic("/Comic/W-i-t-c-h".into()))
        );
        assert_eq!(
            t("https://readcomiconline.to/Comic/Bazooka-Jules/"),
            Some(Target::Comic("/Comic/Bazooka-Jules/".into()))
        );
        assert_eq!(
            t("https://readcomiconline.li/Writer/Grant-Morrison/MostPopular"),
            Some(Target::Tag {
                subcategory: "writer",
                path: "/Writer/Grant-Morrison/MostPopular".into(),
                name: "Grant-Morrison".into(),
            })
        );
        assert!(matches!(
            t("https://readcomiconline.li/genre/Action"),
            Some(Target::Tag {
                subcategory: "genre",
                ..
            })
        ));
        for raw in [
            "https://readcomiconline.li/Comic/X/Issue-1",
            "https://readcomiconline.li/Comic/X/Issue-1/?id=1",
            "https://readcomiconline.li/Comic/X?page=2",
            "https://readcomiconline.li/Character/X",
            "https://readcomiconline.com/Comic/X",
            "https://m.readcomiconline.li/Comic/X",
            "https://readcomiconline.li.evil.test/Comic/X",
            "https://readcomiconline.li:8443/Comic/X",
            "https://u:p@readcomiconline.li/Comic/X",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    /// The inverse of [`baeu`] for a path, padded like the reader script.
    fn obfuscate(path: &str, s0: bool) -> String {
        let suffix = if s0 { "=s0" } else { "=s1600" };
        let core = path.strip_suffix(suffix).unwrap();
        // `path[0:13] + path[17:]` drops 4 characters; `[0:-2]` drops 2.
        let decoded = format!("{}XXXX{}ZZ", &core[..13], &core[13..]);
        let encoded = base64::engine::general_purpose::STANDARD.encode(decoded);
        // `[0:-11] + [-2:]` drops 9 characters before the last 2.
        let (head, tail) = encoded.split_at(encoded.len() - 2);
        let step2 = format!("{head}#########{tail}");
        // `[15:33] + [50:]` keeps 18 characters, skips 17, keeps the rest.
        let step1 = format!(
            "{}{}{}{}",
            "a".repeat(15),
            &step2[..18],
            "b".repeat(17),
            &step2[18..]
        );
        format!("{step1}{}", if s0 { "=s0" } else { "123456" })
    }

    #[test]
    fn decodes_obfuscated_urls() {
        let raw = "pw_.g28x";
        assert_eq!(
            baeu(
                &format!("https://2.bp.blogspot.com/a{raw}c.jpg"),
                "https://img.test"
            )
            .unwrap(),
            "https://img.test/abc.jpg"
        );
        let target = "AbCdEfGhIjKlMnOpQrStUvWxYz0123456789/pw/s1600/x=s1600";
        let encoded = obfuscate(target, false);
        assert_eq!(
            baeu(&format!("{encoded}?rhlupa=1"), "").unwrap(),
            format!("{BLOGSPOT}/{target}?rhlupa=1")
        );
        let target = "AbCdEfGhIjKlMnOpQrStUvWxYz0123456789/pw/s0/y=s0";
        let encoded = obfuscate(target, true);
        assert_eq!(
            baeu(&encoded, "https://ano1.rconet.biz/pic").unwrap(),
            format!("https://ano1.rconet.biz/pic/{target}")
        );
    }

    #[tokio::test]
    async fn extracts_issue_with_decoded_images() {
        let one = "AbCdEfGhIjKlMnOpQrStUvWxYz0123456789/p1/s1600/01.jpg=s1600";
        let two = "AbCdEfGhIjKlMnOpQrStUvWxYz0123456789/p2/s1600/02.png=s1600";
        let page = format!(
            "<title>\r\n   - Read\r\n    W.I.T.C.H.\r\n    Issue #130\r\n</title>\r\n\
<script>function x(l) {{ l = l.replace(/__x__/g, 'd'); return baeu(l, 'https://ano1.rconet.biz/pic'); }}\r\n\
var pth = ''; var _q9z = ''; _q9z = '{}'; _q9z = '{}';</script>",
            obfuscate(one, false).replace('d', "__x__"),
            obfuscate(two, false).replace('d', "__x__"),
        );
        let (root, requests) = serve(vec![html(&page)]);
        let http = http("ReadComicOnline", root);
        let files = media(extract(
            &http,
            Url::parse(
                "https://readcomiconline.li/Comic/W-i-t-c-h/Issue-130?id=22289&s=&quality=lq",
            )
            .unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[1].url.as_str(),
            format!("https://ano1.rconet.biz/pic/{two}")
        );
        let referer = "https://readcomiconline.li/Comic/W-i-t-c-h/Issue-130?id=22289&s=&quality=lq&readType=0";
        assert_eq!(files[1].headers["Referer"], referer);
        let k = kw(&files[1]);
        assert_eq!(k["comic"], "W.I.T.C.H.");
        assert_eq!(k["issue"], "130");
        assert_eq!(k["issue_id"], 22289);
        assert_eq!(k["page"], 2);
        assert_eq!(k["count"], 2);
        assert_eq!(k["language"], "English");
        // Windows path sanitization strips a component's trailing dot.
        let series = if cfg!(windows) {
            "W.I.T.C.H"
        } else {
            "W.I.T.C.H."
        };
        assert_eq!(
            path(&files[1]),
            format!("readcomiconline/{series}/130/W.I.T.C.H._130_002.png=s1600")
        );
        assert_eq!(archive(&files[1]), r#"["{issue_id}_{page}",["22289","2"]]"#);
        let requests = requests.join().unwrap();
        assert!(
            requests[0]
                .starts_with("GET /Comic/W-i-t-c-h/Issue-130?id=22289&s=&quality=lq&readType=0 ")
        );
    }

    #[tokio::test]
    async fn issue_quality_defaults_to_hq_and_honours_option() {
        let (root, requests) = serve(vec![html(""), html("")]);
        let http1 = http("ReadComicOnline", root.clone());
        media(extract(
            &http1,
            Url::parse("https://readcomiconline.li/Comic/X/Annual?id=7").unwrap(),
        ))
        .await;
        let http2 = http("ReadComicOnline", root).with_options(Some(std::sync::Arc::new(
            serde_json::json!({"quality": "lq"})
                .as_object()
                .unwrap()
                .clone(),
        )));
        media(extract(
            &http2,
            Url::parse("https://readcomiconline.li/Comic/X/Annual?id=7&quality=hq").unwrap(),
        ))
        .await;
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /Comic/X/Annual?id=7&quality=hq&readType=0 "));
        assert!(requests[1].starts_with("GET /Comic/X/Annual?id=7&quality=lq&readType=0 "));
    }

    #[test]
    fn parses_issue_titles() {
        let meta = |info: &str| {
            metadata(&format!("   - Read\r\n    Comic\r\n    {info}\r\n"), "5")["issue"].clone()
        };
        assert_eq!(meta("Issue #7"), "7");
        assert_eq!(meta("#12"), "12");
        assert_eq!(meta("Annual 2"), "Annual 2");
        assert_eq!(meta("Issue #x"), "Issue #x");
    }

    const COMIC_PAGE: &str = r#"<div class="barTitle">W.I.T.C.H. &amp; Co information</div>
<table class="listing">
<tr><td><a href="/Comic/W-i-t-c-h/Issue-2?id=22290" title="Read W.I.T.C.H. &amp; Co Issue #2 comic online free">x</a></td></tr>
<tr><td><a href="/Comic/W-i-t-c-h/Annual?id=30000" title="Read W.I.T.C.H. &amp; Co Annual comic online free">y</a></td></tr>
</table>"#;

    #[test]
    fn parses_issue_list() {
        let issues = issues(COMIC_PAGE);
        assert_eq!(issues.len(), 2);
        assert_eq!(
            issues[0].0.as_str(),
            "https://readcomiconline.li/Comic/W-i-t-c-h/Issue-2?id=22290"
        );
        assert_eq!(issues[0].1["comic"], "W.I.T.C.H. & Co");
        assert_eq!(issues[0].1["issue"], "2");
        assert_eq!(issues[0].1["issue_id"], 22290);
        assert_eq!(issues[1].1["issue"], "Annual");
    }

    #[tokio::test]
    async fn comic_queues_issues_oldest_first() {
        let (root, requests) = serve(vec![html(COMIC_PAGE), html(""), html("")]);
        let http = http("ReadComicOnline", root);
        assert!(
            media(extract(
                &http,
                Url::parse("https://readcomiconline.li/Comic/W-i-t-c-h").unwrap(),
            ))
            .await
            .is_empty()
        );
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /Comic/W-i-t-c-h "));
        assert!(
            requests[1].starts_with("GET /Comic/W-i-t-c-h/Annual?id=30000&quality=hq&readType=0 ")
        );
        assert!(
            requests[2].starts_with("GET /Comic/W-i-t-c-h/Issue-2?id=22290&quality=hq&readType=0 ")
        );
    }

    #[tokio::test]
    async fn tag_pages_queue_comics_in_list_view() {
        let first = r#"<a href="/Comic/Batman">B</a><a href="/Comic/Batman/Issue-1?id=1">i</a>
<a href="/Comic/Superman">S</a><a href="page2">&rsaquo; Next </a>"#;
        let second = r#"<a href="/Comic/Flash">F</a>"#;
        let (root, requests) = serve(vec![html(first), html(second)]);
        let http = http("ReadComicOnline", root);
        let children = queued(extract(
            &http,
            Url::parse("https://readcomiconline.li/Writer/Grant%20Morrison").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 3);
        assert_eq!(children[0].subcategory(), "writer");
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /Writer/Grant%20Morrison?page=1 "));
        assert!(requests[1].starts_with("GET /Writer/Grant%20Morrison?page=2 "));
        assert!(
            requests[0]
                .to_ascii_lowercase()
                .contains("cookie: list-view=list")
        );
    }

    #[tokio::test]
    async fn captcha_redirect_fails() {
        let redirect = crate::sites::testing::response("302 Found", "text/html", b"")
            .into_iter()
            .collect::<Vec<u8>>();
        let redirect = String::from_utf8(redirect)
            .unwrap()
            .replace("\r\n\r\n", "\r\nLocation: /Special/AreYouHuman?x=1\r\n\r\n")
            .into_bytes();
        let (root, requests) = serve(vec![redirect, html("captcha")]);
        let http = http("ReadComicOnline", root);
        assert!(
            crate::sites::testing::fails(extract(
                &http,
                Url::parse("https://readcomiconline.li/Comic/X").unwrap(),
            ))
            .await
        );
        requests.join().unwrap();
    }
}
