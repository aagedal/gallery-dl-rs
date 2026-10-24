//! IMHentai and its sister sites (HentaiEra, HentaiRox, HentaiFox,
//! HentaiEnvy, HentaiZap), mirroring gallery_dl/extractor/imhentai.py:
//! gallery pages list their images as `$.parseJSON('…')` data; tag and
//! search listings queue each gallery page by page.
//!
//! Only upstream's built-in instances are supported; `IMHentai:URL` and
//! configured extra instances are not. Galleries without image data get
//! `.jpg` URLs as upstream does, but without its `_fallback` extensions;
//! listing pages that link to another host end the listing.
use super::{Gallery, keywords, set, text};
use crate::sites::art::{Scan, extr, extract_iter, json_loads, on_host, segment};
use crate::sites::{Events, Http, Route, Site, remove_html, unescape};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

static GALLERY: Route = Route::new(
    "gallery",
    "{category}_{gallery_id}_{num:>03}.{extension}",
    &["{category}", "{gallery_id} {title}"],
    "{gallery_id}_{num}",
);
static TAG: Route = Route::new("tag", "{filename}.{extension}", &["{category}"], "");
static SEARCH: Route = Route::new("search", "{filename}.{extension}", &["{category}"], "");

/// Listing pages one URL may follow.
const MAX_PAGES: usize = 10_000;

macro_rules! instance {
    ($ident:ident, $category:literal, $name:literal, $domain:literal) => {
        pub static $ident: Site = Site {
            category: $category,
            name: $name,
            matches: |url| target($domain, url).is_some(),
            extract: |http, url| extract(&$ident, $domain, http, url),
            routes: &[&GALLERY, &TAG, &SEARCH],
            api_key: false,
            cookies: None,
        };
    };
}
instance!(IMHENTAI, "imhentai", "IMHentai", "imhentai.xxx");
instance!(HENTAIERA, "hentaiera", "HentaiEra", "hentaiera.com");
instance!(HENTAIROX, "hentairox", "HentaiRox", "hentairox.com");
instance!(HENTAIFOX, "hentaifox", "HentaiFox", "hentaifox.com");
instance!(HENTAIENVY, "hentaienvy", "HentaiEnvy", "hentaienvy.com");
instance!(HENTAIZAP, "hentaizap", "HentaiZap", "hentaizap.com");

#[derive(Debug, PartialEq)]
enum Target {
    Gallery(String),
    /// A `tag` or `search` listing and its path (and query), raw.
    Listing(&'static str, String),
}

/// `(?:www\.)?DOMAIN` followed by `/(?:gallery|view)/(\d+)`,
/// `(/(?:artist|category|…|tag)/([^/?#]+))` or
/// `(/(?:advanced-)?search/?\?[^#]+|/[^/?#]+/?)`, all unanchored.
fn target(domain: &str, url: &Url) -> Option<Target> {
    if !on_host(url, &[domain]) {
        return None;
    }
    let path = url.path();
    let raw: Vec<&str> = path.split('/').skip(1).collect();
    if let ["gallery" | "view", id, ..] = raw.as_slice()
        && let Some(id) = crate::sites::art::leading_digits(id)
    {
        return Some(Target::Gallery(id.to_owned()));
    }
    if let [
        kind @ ("artist" | "category" | "character" | "group" | "language" | "parody" | "tag"),
        name,
        ..,
    ] = raw.as_slice()
        && segment(name)
    {
        return Some(Target::Listing("tag", format!("/{kind}/{name}/")));
    }
    let query = url.query().filter(|q| !q.is_empty());
    if let ("/search" | "/search/" | "/advanced-search" | "/advanced-search/", Some(query)) =
        (path, query)
    {
        return Some(Target::Listing("search", format!("{path}?{query}")));
    }
    match raw.as_slice() {
        [first, rest @ ..] if segment(first) => {
            let slash = if rest.is_empty() { "" } else { "/" };
            Some(Target::Listing("search", format!("/{first}{slash}")))
        }
        _ => None,
    }
}

fn extract<'a>(site: &'static Site, domain: &'static str, http: &'a Http, url: Url) -> Events<'a> {
    let root = format!("https://{domain}");
    match target(domain, &url) {
        Some(Target::Gallery(id)) => crate::sites::once(async move {
            let page = page(http, &format!("{root}/gallery/{id}/")).await?;
            let data = metadata(&page, &id);
            let gallery = Gallery {
                enum_key: "num",
                ..Gallery::chapter(&GALLERY, data, images(&page)?).id(id)
            };
            Ok(crate::sites::events(gallery.events(site, http)))
        }),
        Some(Target::Listing(sub, path)) => {
            let route = if sub == "tag" { &TAG } else { &SEARCH };
            listing(site, domain, http, route, format!("{root}{path}"))
        }
        None => crate::sites::fail(error(format!("unsupported {} URL", site.name))),
    }
}

async fn page(http: &Http, url: &str) -> Result<String> {
    http.text_with(&http.api(url)?, &http.browser_headers(&[]))
        .await
}

/// `ImhentaiGalleryExtractor._split`: sorted link texts without their
/// count badges.
fn split(html: &str) -> Vec<String> {
    let mut results: Vec<String> = extract_iter(html, ">", "</a>")
        .into_iter()
        .map(|tag| {
            let badge = tag.contains("badge'>") || tag.contains("class='badge");
            let tag = remove_html(tag);
            match badge {
                true => text::partition_last(&tag, " ").0.to_owned(),
                false => tag,
            }
        })
        .collect();
    results.sort();
    results
}

/// `ImhentaiGalleryExtractor.metadata`.
fn metadata(page: &str, id: &str) -> Map<String, Value> {
    let mut scan = Scan::new(page);
    let title = scan.extr("<h1>", "<");
    let title_alt = scan.extr("class=\"subtitle\">", "<");
    let end = match scan.extr("<ul class=\"galleries_info", ">") {
        "" => "</ul>",
        _ => "</li>",
    };
    let mut data = keywords([
        ("gallery_id", text::parse_int(id).into()),
        ("title", unescape(title).into()),
        ("title_alt", unescape(title_alt).into()),
        ("parody", split(scan.extr(">Parodies", end)).into()),
        ("character", split(scan.extr(">Characters", end)).into()),
        ("tags", split(scan.extr(">Tags", end)).into()),
        ("artist", split(scan.extr(">Artists", end)).into()),
        ("group", split(scan.extr(">Groups", end)).into()),
    ]);
    let language = split(scan.extr(">Languages", end));
    let lang = language.first().map(|l| text::language_to_code(l));
    set(&mut data, "language", language);
    set(&mut data, "type", scan.extr("href='/category/", "/"));
    if let Some(lang) = lang {
        set(&mut data, "lang", lang);
    }
    data
}

/// `ImhentaiGalleryExtractor.images`: `N.EXT` beside the first `data-src`
/// image, with sizes from the page's JSON.
fn images(page: &str) -> Result<Vec<super::Page>> {
    let base = format!(
        "{}/",
        text::partition_last(extr(page, "data-src=\"", "\""), "/").0
    );
    let data = json_loads(extr(page, "$.parseJSON('", "'"), "IMHentai").ok();
    let Some(data) = data.filter(|data| !data.is_null()) else {
        // Upstream warns about the missing image data.
        let count = text::parse_int(extr(page, "id=\"load_pages\" value=\"", "\""));
        return Ok((1..=count.clamp(0, 10_000))
            .map(|i| (format!("{base}{i}.jpg"), None))
            .collect());
    };
    let len = data.as_object().map_or(0, Map::len);
    (1..=len)
        .map(|i| {
            let info = data
                .get(i.to_string())
                .and_then(Value::as_str)
                .ok_or_else(|| error("IMHentai image data is incomplete"))?;
            let [ext, width, height] = info.split(',').collect::<Vec<_>>()[..] else {
                return Err(error("IMHentai image data is malformed"));
            };
            let ext = match ext {
                "j" => "jpg",
                "p" => "png",
                "g" => "gif",
                "w" => "webp",
                "a" => "avif",
                _ => return Err(error("IMHentai image type is unknown")),
            };
            Ok((
                format!("{base}{i}.{ext}"),
                Some(keywords([
                    ("width", text::parse_int(width).into()),
                    ("height", text::parse_int(height).into()),
                ])),
            ))
        })
        .collect()
}

/// `ImhentaiExtractor._pagination`: the galleries of each page, up to the
/// rankings sidebar, following the last `page-link` until it is `#`.
fn listing<'a>(
    site: &'static Site,
    domain: &'static str,
    http: &'a Http,
    route: &'static Route,
    url: String,
) -> Events<'a> {
    crate::sites::paginate(Some((url, None::<String>, 0)), move |state| async move {
        let Some((url, mut prev, count)) = state else {
            return Ok(None);
        };
        let root = format!("https://{domain}");
        let mut page = page(http, &url).await?;
        if let Some(pos) = page.find("class=\"ranking_list\"") {
            page.truncate(pos);
        }
        let mut children = Vec::new();
        for id in extract_iter(&page, "href=\"/gallery/", "\"") {
            if prev.as_deref() == Some(id) {
                continue;
            }
            if id.is_empty() {
                break;
            }
            if let Ok(url) = Url::parse(&format!("{root}/gallery/{id}")) {
                children.push((url, Map::new()));
            }
            prev = Some(id.to_owned());
        }
        let href = text::rextr(&page, "class='page-link' href='", "'");
        let next = match href {
            "" | "#" => None,
            href if href.starts_with("//") => Some(format!("https:{href}")),
            href if href.starts_with('/') => Some(format!("{root}{href}")),
            href => Some(href.to_owned()),
        };
        let next = next
            .filter(|next| Url::parse(next).is_ok_and(|next| on_host(&next, &[domain])))
            .filter(|_| count + 1 < MAX_PAGES)
            .map(|next| (next, prev, count + 1));
        Ok(Some((super::queue_urls(http, site, route, children), next)))
    })
}

#[cfg(test)]
mod tests {
    use super::super::tests::{archive, path};
    use super::*;
    use crate::sites::testing::{html, http, keywords as kw, media, queued, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target("imhentai.xxx", &Url::parse(raw).unwrap());
        assert_eq!(
            t("https://imhentai.xxx/gallery/12345/"),
            Some(Target::Gallery("12345".into()))
        );
        assert_eq!(
            t("https://www.imhentai.xxx/view/12345/3/"),
            Some(Target::Gallery("12345".into()))
        );
        assert_eq!(
            t("https://imhentai.xxx/tag/big-breasts/popular/"),
            Some(Target::Listing("tag", "/tag/big-breasts/".into()))
        );
        assert_eq!(
            t("https://imhentai.xxx/search/?key=a+b&page=2"),
            Some(Target::Listing("search", "/search/?key=a+b&page=2".into()))
        );
        assert_eq!(
            t("https://imhentai.xxx/advanced-search?key=x"),
            Some(Target::Listing("search", "/advanced-search?key=x".into()))
        );
        assert_eq!(
            t("https://imhentai.xxx/popular/?x=1"),
            Some(Target::Listing("search", "/popular/".into()))
        );
        assert_eq!(
            t("https://imhentai.xxx/gallery/abc"),
            Some(Target::Listing("search", "/gallery/".into()))
        );
        assert_eq!(
            t("https://imhentai.xxx/random"),
            Some(Target::Listing("search", "/random".into()))
        );
        for raw in [
            "https://imhentai.xxx/",
            "https://m.imhentai.xxx/gallery/1/",
            "https://imhentai.xxx.evil.test/gallery/1/",
            "https://imhentai.xxx:8443/gallery/1/",
            "https://u:p@imhentai.xxx/gallery/1/",
            "https://hentaifox.com/gallery/1/",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
        let url = Url::parse("https://hentaifox.com/gallery/1/").unwrap();
        assert_eq!(
            crate::sites::find(&url).map(|site| site.category),
            Some("hentaifox")
        );
    }

    const GALLERY_PAGE: &str = r#"<h1>Big &amp; Small</h1><p class="subtitle">Alt &amp; Title</p>
<ul class="galleries_info">
<li><span>Parodies:</span><a href='/parody/x/'>Zeta <span class='badge'>12</span></a><a href='/parody/y/'>Alpha <span class='badge'>3</span></a></li>
<li><span>Tags:</span><a href='/tag/z/'>Solo</a></li>
<li><span>Artists:</span></li>
<li><span>Languages:</span><a href='/language/t/'>translated <span class='badge'>1</span></a><a href='/language/e/'>english <span class='badge'>9</span></a></li>
<li><span>Category:</span><a href='/category/manga/'>Manga</a></li>
</ul>
<img data-src="https://m1.imhentai.xxx/001/abc/cover.jpg">
<script>var g_th = $.parseJSON('{"1":"j,1280,1810","2":"w,800,600"}');</script>"#;

    #[tokio::test]
    async fn extracts_gallery() {
        let (root, requests) = serve(vec![html(GALLERY_PAGE)]);
        let http = http("HentaiFox", root);
        let files = media(extract(
            &HENTAIFOX,
            "hentaifox.com",
            &http,
            Url::parse("https://hentaifox.com/gallery/12345/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[1].url.as_str(),
            "https://m1.imhentai.xxx/001/abc/2.webp"
        );
        let k = kw(&files[1]);
        assert_eq!(k["category"], "hentaifox");
        assert_eq!(k["gallery_id"], 12345);
        assert_eq!(k["title"], "Big & Small");
        assert_eq!(k["title_alt"], "Alt & Title");
        assert_eq!(k["parody"], json!(["Alpha", "Zeta"]));
        assert_eq!(k["tags"], json!(["Solo"]));
        assert_eq!(k["artist"], json!([]));
        assert_eq!(k["language"], json!(["english", "translated"]));
        assert_eq!(k["lang"], "en");
        assert_eq!(k["type"], "manga");
        assert_eq!(k["width"], 800);
        assert_eq!(k["height"], 600);
        assert_eq!(k["num"], 2);
        assert_eq!(k["count"], 2);
        assert_eq!(
            path(&files[1]),
            "hentaifox/12345 Big & Small/hentaifox_12345_002.webp"
        );
        assert_eq!(
            archive(&files[1]),
            r#"["{gallery_id}_{num}",["12345","2"]]"#
        );
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /gallery/12345/ "));
    }

    #[tokio::test]
    async fn gallery_without_image_data_guesses_jpg() {
        let page = r#"<h1>X</h1><img data-src="https://m1.test/a/cover.jpg"><input id="load_pages" value="3">"#;
        let (root, requests) = serve(vec![html(page)]);
        let http = http("IMHentai", root);
        let files = media(extract(
            &IMHENTAI,
            "imhentai.xxx",
            &http,
            Url::parse("https://imhentai.xxx/gallery/1/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 3);
        assert_eq!(files[2].url.as_str(), "https://m1.test/a/3.jpg");
        assert!(!kw(&files[0]).contains_key("lang"));
        requests.join().unwrap();
    }

    #[tokio::test]
    async fn listing_follows_page_links() {
        let first = r#"<a href="/gallery/1/">a</a><a href="/gallery/1/">a</a><a href="/gallery/2/">b</a>
<a class='page-link' href='/tag/x/?page=1'>1</a><a class='page-link' href='/tag/x/?page=2'>next</a>
<div class="ranking_list"><a href="/gallery/99/">top</a></div>"#;
        let second = r#"<a href="/gallery/3/">c</a><a class='page-link' href='#'>next</a>"#;
        let (root, requests) = serve(vec![html(first), html(second)]);
        let http = http("IMHentai", root);
        let children = queued(extract(
            &IMHENTAI,
            "imhentai.xxx",
            &http,
            Url::parse("https://imhentai.xxx/tag/x/").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 3);
        assert_eq!(children[0].subcategory(), "tag");
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /tag/x/ "));
        assert!(requests[1].starts_with("GET /tag/x/?page=2 "));
    }

    #[tokio::test]
    async fn listing_stops_at_foreign_page_links() {
        let page = r#"<a href="/gallery/1/">a</a><a class='page-link' href='https://evil.test/?page=2'>n</a>"#;
        let (root, requests) = serve(vec![html(page)]);
        let http = http("IMHentai", root);
        let children = queued(extract(
            &IMHENTAI,
            "imhentai.xxx",
            &http,
            Url::parse("https://imhentai.xxx/search/?key=x").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].subcategory(), "search");
        assert!(requests.join().unwrap()[0].starts_with("GET /search/?key=x "));
    }
}
