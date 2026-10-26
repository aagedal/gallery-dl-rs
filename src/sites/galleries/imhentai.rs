//! IMHentai and its mirrors (HentaiEra, HentaiRox, HentaiFox, HentaiEnvy,
//! HentaiZap): galleries, tags and searches, mirroring
//! gallery_dl/extractor/imhentai.py.
use crate::error;
use crate::sites::gdl::{
    Scan, extr, extract_iter, language_to_code, parse_int, remove_html, rextr, rpartition, unescape,
};
use crate::sites::manga::{GALLERY_ARCHIVE, GALLERY_FILENAME};
use crate::sites::{Events, Http, Route, Site};
use serde_json::{Map, Value};
use url::Url;

static GALLERY: Route = Route::new(
    "gallery",
    GALLERY_FILENAME,
    &["{category}", "{gallery_id} {title}"],
    GALLERY_ARCHIVE,
);
static TAG: Route = Route::new("tag", "{filename}.{extension}", &["{category}"], "");
static SEARCH: Route = Route::new("search", "{filename}.{extension}", &["{category}"], "");

macro_rules! instance {
    ($ident:ident, $category:literal, $name:literal, $root:literal, $host:literal) => {
        pub static $ident: Site = Site {
            category: $category,
            name: $name,
            matches: |url| target($host, url).is_some(),
            extract: |http, url| extract(&$ident, $root, $host, http, url),
            routes: &[&GALLERY, &TAG, &SEARCH],
            api_key: false,
            cookies: None,
        };
    };
}
instance!(
    IMHENTAI,
    "imhentai",
    "IMHentai",
    "https://imhentai.xxx",
    "imhentai.xxx"
);
instance!(
    HENTAIERA,
    "hentaiera",
    "HentaiEra",
    "https://hentaiera.com",
    "hentaiera.com"
);
instance!(
    HENTAIROX,
    "hentairox",
    "HentaiRox",
    "https://hentairox.com",
    "hentairox.com"
);
instance!(
    HENTAIFOX,
    "hentaifox",
    "HentaiFox",
    "https://hentaifox.com",
    "hentaifox.com"
);
instance!(
    HENTAIENVY,
    "hentaienvy",
    "HentaiEnvy",
    "https://hentaienvy.com",
    "hentaienvy.com"
);
instance!(
    HENTAIZAP,
    "hentaizap",
    "HentaiZap",
    "https://hentaizap.com",
    "hentaizap.com"
);

#[derive(Debug, PartialEq)]
enum Target {
    Gallery(String),
    /// Path such as `/tag/NAME`.
    Tag(String),
    /// Path and query.
    Search(String),
}

/// The URL past `https://(www.)HOST`, if it is on `host`.
fn rest<'u>(host: &str, url: &'u Url) -> Option<&'u str> {
    let raw = url.as_str();
    let raw = raw
        .strip_prefix("https://")
        .or_else(|| raw.strip_prefix("http://"))?;
    let raw = raw.strip_prefix("www.").unwrap_or(raw);
    raw.strip_prefix(host).filter(|r| r.starts_with('/'))
}

fn target(host: &str, url: &Url) -> Option<Target> {
    let rest = rest(host, url)?;
    if let Some(c) = crate::sites::gdl::re!(r"^/(?:gallery|view)/(\d+)").captures(rest) {
        return Some(Target::Gallery(c[1].to_owned()));
    }
    if let Some(c) = crate::sites::gdl::re!(
        r"^(/(?:artist|category|character|group|language|parody|tag)/[^/?#]+)"
    )
    .captures(rest)
    {
        return Some(Target::Tag(c[1].to_owned()));
    }
    crate::sites::gdl::re!(r"^(/(?:advanced-)?search/?\?[^#]+|/[^/?#]+/?)")
        .captures(rest)
        .map(|c| Target::Search(c[1].to_owned()))
}

/// `_split`: sorted tag names without their count badges.
fn split(html: &str) -> Vec<String> {
    let mut results: Vec<String> = extract_iter(html, ">", "</a>")
        .into_iter()
        .map(|tag| {
            let badge = tag.contains("badge'>") || tag.contains("class='badge");
            let tag = remove_html(tag);
            if badge {
                rpartition(&tag, " ").0.to_owned()
            } else {
                tag
            }
        })
        .collect();
    results.sort();
    results
}

fn metadata(page: &str, gallery_id: &str) -> Map<String, Value> {
    let mut scan = Scan::new(page);
    let title = scan.extr("<h1>", "<").to_owned();
    let title_alt = scan.extr("class=\"subtitle\">", "<").to_owned();
    let end = if scan.extr("<ul class=\"galleries_info", ">").is_empty() {
        "</ul>"
    } else {
        "</li>"
    };
    let mut data = Map::new();
    data.insert("gallery_id".into(), parse_int(gallery_id).into());
    data.insert("title".into(), unescape(&title).into());
    data.insert("title_alt".into(), unescape(&title_alt).into());
    for (key, label) in [
        ("parody", ">Parodies"),
        ("character", ">Characters"),
        ("tags", ">Tags"),
        ("artist", ">Artists"),
        ("group", ">Groups"),
        ("language", ">Languages"),
    ] {
        data.insert(key.into(), split(scan.extr(label, end)).into());
    }
    data.insert("type".into(), scan.extr("href='/category/", "/").into());
    if let Some(first) = data["language"]
        .get(0)
        .and_then(Value::as_str)
        .map(str::to_owned)
    {
        data.insert(
            "lang".into(),
            language_to_code(&first).map_or(Value::Null, Value::from),
        );
    }
    data
}

/// `images`: from the `$.parseJSON` page table, else numbered guesses
/// with fallback extensions.
fn images(page: &str) -> Vec<(String, Option<Map<String, Value>>)> {
    let base = format!("{}/", rpartition(extr(page, "data-src=\"", "\""), "/").0);
    let exts = [
        ("j", "jpg"),
        ("p", "png"),
        ("g", "gif"),
        ("w", "webp"),
        ("a", "avif"),
    ];
    let ext_of = |key: &str| exts.iter().find(|(k, _)| *k == key).map(|(_, e)| *e);
    let table: Option<Map<String, Value>> =
        serde_json::from_str(extr(page, "$.parseJSON('", "'")).ok();
    let Some(table) = table else {
        let count = parse_int(extr(page, "id=\"load_pages\" value=\"", "\""));
        return (1..=count)
            .map(|i| {
                let fallback: Vec<Value> = exts[1..]
                    .iter()
                    .map(|(_, ext)| format!("{base}{i}.{ext}").into())
                    .collect();
                let mut file = Map::new();
                file.insert("_fallback".into(), fallback.into());
                (format!("{base}{i}.jpg"), Some(file))
            })
            .collect();
    };
    (1..=table.len())
        .filter_map(|i| {
            let entry = table.get(&i.to_string())?.as_str()?;
            let mut parts = entry.split(',');
            let ext = ext_of(parts.next()?)?;
            let mut file = Map::new();
            file.insert(
                "width".into(),
                parse_int(parts.next().unwrap_or_default()).into(),
            );
            file.insert(
                "height".into(),
                parse_int(parts.next().unwrap_or_default()).into(),
            );
            Some((format!("{base}{i}.{ext}"), Some(file)))
        })
        .collect()
}

fn extract<'a>(
    site: &'static Site,
    root: &'static str,
    host: &'static str,
    http: &'a Http,
    url: Url,
) -> Events<'a> {
    let Some(target) = target(host, &url) else {
        return crate::sites::fail(error("unsupported IMHentai URL"));
    };
    match target {
        Target::Gallery(id) => crate::sites::once(async move {
            let page = http
                .text(&http.api(&format!("{root}/gallery/{id}/"))?)
                .await?;
            let data = metadata(&page, &id);
            Ok(crate::sites::events(super::numbered(
                site,
                http,
                &GALLERY,
                &data,
                images(&page),
                &id,
            )))
        }),
        Target::Tag(path) => pagination(site, root, http, &TAG, format!("{root}{path}/")),
        Target::Search(path) => pagination(site, root, http, &SEARCH, format!("{root}{path}")),
    }
}

/// `_pagination`: gallery links, following the last page link.
fn pagination<'a>(
    site: &'static Site,
    root: &'static str,
    http: &'a Http,
    route: &'static Route,
    first: String,
) -> Events<'a> {
    crate::sites::paginate(Some(first), move |state| async move {
        let Some(url) = state else { return Ok(None) };
        let page = http.text(&http.api(&url)?).await?;
        let page = page
            .find("class=\"ranking_list\"")
            .map_or(page.as_str(), |pos| &page[..pos]);
        let mut prev = String::new();
        let mut children = Vec::new();
        for gallery_id in extract_iter(page, "href=\"/gallery/", "\"") {
            if gallery_id == prev {
                continue;
            }
            if let Ok(link) = Url::parse(&format!("{root}/gallery/{gallery_id}")) {
                children.push((link, Map::new()));
            }
            gallery_id.clone_into(&mut prev);
        }
        let href = rextr(page, "class='page-link' href='", "'", None);
        let next = match href {
            "" | "#" => None,
            href if href.starts_with("//") => Some(format!("https:{href}")),
            href if href.starts_with('/') => Some(format!("{root}{href}")),
            href => Some(href.to_owned()),
        };
        Ok(Some((super::queue(http, site, route, children), next)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{children, html, http as client, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target("imhentai.xxx", &Url::parse(raw).unwrap());
        assert_eq!(
            t("https://imhentai.xxx/gallery/12345/"),
            Some(Target::Gallery("12345".into()))
        );
        assert_eq!(
            t("https://imhentai.xxx/view/12345/1/"),
            Some(Target::Gallery("12345".into()))
        );
        assert_eq!(
            t("https://imhentai.xxx/tag/big-breasts/"),
            Some(Target::Tag("/tag/big-breasts".into()))
        );
        assert_eq!(
            t("https://imhentai.xxx/search/?key=a"),
            Some(Target::Search("/search/?key=a".into()))
        );
        assert_eq!(
            t("https://imhentai.xxx/popular/"),
            Some(Target::Search("/popular/".into()))
        );
        assert!(t("https://hentaifox.com/gallery/1/").is_none());
        assert!(
            target(
                "hentaifox.com",
                &Url::parse("https://www.hentaifox.com/gallery/1/").unwrap()
            )
            .is_some()
        );
    }

    #[tokio::test]
    async fn gallery_reads_image_table() {
        let page = r#"<h1>Title &amp; More</h1><p class="subtitle">Alt</p><ul class="galleries_info">
            <li>Tags: <a href='/tag/b/'>beta <span class='badge'>12</span></a><a href='/tag/a/'>alpha <span class='badge'>3</span></a></li>
            <li>Languages: <a href='/language/english/'>english <span class='badge'>1</span></a></li>
            <a href='/category/manga/'>Manga</a><img data-src="https://m1.imhentai.xxx/001/abc/cover.jpg">
            <script>var g_th = $.parseJSON('{"1":"j,1280,1810","2":"w,800,600"}');</script>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = client("imhentai", root);
        let files = media(extract(
            &IMHENTAI,
            "https://imhentai.xxx",
            "imhentai.xxx",
            &http,
            Url::parse("https://imhentai.xxx/gallery/7/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[1].url.as_str(),
            "https://m1.imhentai.xxx/001/abc/2.webp"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["tags"], serde_json::json!(["alpha", "beta"]));
        assert_eq!(k["lang"], "en");
        assert_eq!(k["type"], "manga");
        assert_eq!(k["width"], 1280);
        server.join().unwrap();
    }

    #[test]
    fn missing_table_guesses_with_fallbacks() {
        let page = r#"<img data-src="https://m1/x/cover.jpg"><input id="load_pages" value="2">"#;
        let found = images(page);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].0, "https://m1/x/1.jpg");
        assert_eq!(
            found[0].1.as_ref().unwrap()["_fallback"][0],
            "https://m1/x/1.png"
        );
    }

    #[tokio::test]
    async fn search_follows_page_links() {
        let first = r#"<a href="/gallery/1/"><a href="/gallery/1/"><a href="/gallery/2/"><a class='page-link' href='/search/?key=a&page=2'>"#;
        let second = r#"<a href="/gallery/3/"><a class='page-link' href='#'><div class="ranking_list"><a href="/gallery/9/">"#;
        let (root, server) = serve(vec![html(first), html(second)]);
        let http = client("imhentai", root);
        let queued = children(extract(
            &IMHENTAI,
            "https://imhentai.xxx",
            "imhentai.xxx",
            &http,
            Url::parse("https://imhentai.xxx/search/?key=a").unwrap(),
        ))
        .await;
        assert_eq!(queued.len(), 3);
        server.join().unwrap();
    }
}
