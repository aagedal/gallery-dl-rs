//! Xasiat albums and their tag, category and model listings, mirroring
//! gallery_dl/extractor/xasiat.py. Listings load their album blocks
//! asynchronously page by page and queue each album.
use crate::error;
use crate::sites::art::{Scan, extract_iter, leading_digits, on_host, parse_int, segment};
use crate::sites::manga::keywords;
use crate::sites::{Events, Http, Item, Route, Site, unescape};
use futures_util::{StreamExt, stream};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://www.xasiat.com";
const DIRECTORY: &[&str] = &["{category}", "{title}"];
const ARCHIVE: &str = "{album_url}_{num}";

static ALBUM: Route = Route::new("album", "{filename}.{extension}", DIRECTORY, ARCHIVE);
static TAG: Route = Route::new("tag", "{filename}.{extension}", DIRECTORY, ARCHIVE);
static CATEGORY: Route = Route::new("category", "{filename}.{extension}", DIRECTORY, ARCHIVE);
static MODEL: Route = Route::new("model", "{filename}.{extension}", DIRECTORY, ARCHIVE);

pub static SITE: Site = Site {
    category: "xasiat",
    name: "Xasiat",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&ALBUM, &TAG, &CATEGORY, &MODEL],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    /// Album path and ID.
    Album(String, String),
    /// Listing subcategory and path.
    Listing(&'static str, String),
}

/// `xasiat\.com((?:/fr|/ja)?/albums/...)`, unanchored at the end.
fn target(url: &Url) -> Option<Target> {
    if !on_host(url, &["xasiat.com"]) {
        return None;
    }
    let (lang, rest) = ["/fr", "/ja", ""].into_iter().find_map(|lang| {
        let rest = url.path().strip_prefix(lang)?.strip_prefix("/albums/")?;
        Some((lang, rest))
    })?;
    let mut parts = rest.split('/');
    let first = parts.next()?;
    let name = parts.next().filter(|name| segment(name))?;
    let path = format!("{lang}/albums/{first}/{name}");
    if let Some(id) = leading_digits(first).filter(|id| id.len() == first.len()) {
        return Some(Target::Album(path, id.to_owned()));
    }
    let subcategory = match first {
        "tags" => TAG.subcategory,
        "categories" => CATEGORY.subcategory,
        "models" => MODEL.subcategory,
        _ => return None,
    };
    Some(Target::Listing(subcategory, path))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Album(path, id)) => album(http, path, id),
        Some(Target::Listing(subcategory, path)) => listing(http, subcategory, path),
        None => crate::sites::fail(error("unsupported Xasiat URL")),
    }
}

fn album(http: &Http, path: String, id: String) -> Events<'_> {
    crate::sites::once(async move {
        let (location, page) = http
            .page(&http.api(&format!("{ROOT}{path}/"))?, &[])
            .await?;
        let mut scan = Scan::new(&page);
        let title = unescape(scan.extr("<h1>", "<"));
        let info = scan.extr("class=\"info-content\"", "</div>");
        let images = scan.extr("class=\"images\"", "</div>");
        let urls: Vec<&str> = extract_iter(images, "href=\"", "\"");
        // `response.url`, on the site's own origin.
        let album_url = format!(
            "{ROOT}{}{}",
            location.path(),
            location
                .query()
                .map(|q| format!("?{q}"))
                .unwrap_or_default()
        );
        let data = keywords([
            ("title", title.into()),
            (
                "model",
                labels(info, "top_models1\"></i>", false, "</span").into(),
            ),
            ("tags", labels(info, "tags/", true, "</a").into()),
            (
                "album_category",
                labels(info, "categories/", true, "</a")
                    .into_iter()
                    .next()
                    .unwrap_or_default()
                    .into(),
            ),
            ("album_url", album_url.into()),
            ("album_id", parse_int(&id, 0).into()),
            ("count", urls.len().into()),
        ]);
        let items = urls
            .into_iter()
            .enumerate()
            .filter_map(|(index, raw)| {
                let url = crate::sites::media::web_url(raw)?;
                let mut fields = data.clone();
                fields.insert("num".into(), (index + 1).into());
                // `nameext_from_url(url[:-1])`: the links end in `/`.
                let trimmed = &raw[..raw.char_indices().last().map_or(0, |(i, _)| i)];
                crate::sites::art::nameext_into(trimmed, &mut fields);
                Some(Item::new(url, fields).ids(id.clone(), (index + 1).to_string()))
            })
            .collect();
        Ok(crate::sites::events(crate::sites::post(
            &SITE, http, &ALBUM, items,
        )))
    })
}

/// `XasiatExtractor._pagination`: album blocks by post date until a page
/// shows the disabled `Next` marker.
fn listing<'a>(http: &'a Http, subcategory: &'static str, path: String) -> Events<'a> {
    crate::sites::paginate(Some(1), move |pnum| {
        let path = path.clone();
        async move {
            let Some(pnum) = pnum else {
                return Ok(None);
            };
            let mut url = http.api(&format!("{ROOT}{path}/"))?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis());
            url.query_pairs_mut()
                .append_pair("mode", "async")
                .append_pair("function", "get_block")
                .append_pair("block_id", "list_albums_common_albums_list")
                .append_pair("sort_by", "post_date")
                .append_pair("from", &pnum.to_string())
                .append_pair("_", &now.to_string());
            let page = http.text(&url).await?;
            let data = Map::<String, Value>::new();
            let children: Vec<Url> = posts(&page)
                .into_iter()
                .filter_map(|raw| Url::parse(&raw).ok())
                .filter(|url| matches!(target(url), Some(Target::Album(..))))
                .collect();
            let events = stream::iter(children)
                .map(move |child| {
                    crate::sites::queue(http, SITE.category, subcategory, child, &data)
                })
                .flatten()
                .boxed();
            let next = (!page.contains("<span>Next</span>")).then_some(pnum + 1);
            Ok(Some((events, next)))
        }
    })
}

/// `class="item  ">\s*<a href="([^"]+)`.
fn posts(page: &str) -> Vec<String> {
    let marker = "class=\"item  \">";
    let mut out = Vec::new();
    let mut rest = page;
    while let Some(found) = rest.find(marker) {
        rest = &rest[found + marker.len()..];
        let Some(tail) = rest.trim_start().strip_prefix("<a href=\"") else {
            continue;
        };
        match tail.find('"') {
            Some(end) if end > 0 => out.push(tail[..end].to_owned()),
            _ => {}
        }
    }
    out
}

/// Python `re.findall` of `PREFIX[^"]+">\s*(.+)\s*CLOSE` (`quoted`) or
/// `PREFIX\s*(.+)\s*CLOSE`: after the prefix and any whitespace, the rest
/// of the line up to its last `CLOSE`.
fn labels(text: &str, prefix: &str, quoted: bool, close: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(found) = text[pos..].find(prefix) {
        let start = pos + found + prefix.len();
        pos += found + 1;
        let mut rest = &text[start..];
        if quoted {
            match rest.find('"') {
                Some(quote) if quote > 0 && rest[quote + 1..].starts_with('>') => {
                    rest = &rest[quote + 2..];
                }
                _ => continue,
            }
        }
        let rest = rest.trim_start();
        let line = &rest[..rest.find('\n').unwrap_or(rest.len())];
        // Greedy `(.+)`: up to the line's end when `CLOSE` follows after
        // whitespace, else up to the line's last `CLOSE`.
        let after = rest[line.len()..].trim_start();
        let (end, next) = if after.starts_with(close) {
            (line.len(), text.len() - after.len() + close.len())
        } else {
            match line.rfind(close) {
                Some(end) => (end, text.len() - rest.len() + end + close.len()),
                None => continue,
            }
        };
        if end > 0 {
            out.push(line[..end].to_owned());
            pos = next;
        }
    }
    out
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
        assert_eq!(
            t("https://www.xasiat.com/albums/12345/some-title/"),
            Some(Target::Album(
                "/albums/12345/some-title".into(),
                "12345".into()
            ))
        );
        assert_eq!(
            t("https://xasiat.com/ja/albums/7/t"),
            Some(Target::Album("/ja/albums/7/t".into(), "7".into()))
        );
        assert_eq!(
            t("https://www.xasiat.com/fr/albums/tags/sea/"),
            Some(Target::Listing("tag", "/fr/albums/tags/sea".into()))
        );
        assert_eq!(
            t("https://www.xasiat.com/albums/categories/c/"),
            Some(Target::Listing("category", "/albums/categories/c".into()))
        );
        assert_eq!(
            t("https://www.xasiat.com/albums/models/m"),
            Some(Target::Listing("model", "/albums/models/m".into()))
        );
        for raw in [
            "https://www.xasiat.com/albums/12345/",
            "https://www.xasiat.com/de/albums/1/t/",
            "https://www.xasiat.com/albums/other/x/",
            "https://www.xasiat.com.evil.test/albums/1/t/",
            "https://www.xasiat.com:8080/albums/1/t/",
            "https://u:p@www.xasiat.com/albums/1/t/",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[test]
    fn finds_labels() {
        let info = "<a href=\"/tags/a/\">\n  First tag  </a> <a>\n<a href=\"/tags/b/\">Second\n </a>\n<a href=\"/tags/\">x</a>";
        assert_eq!(
            labels(info, "tags/", true, "</a"),
            ["First tag  ", "Second"]
        );
        assert_eq!(
            labels(
                "<i class=\"top_models1\"></i> Ann </span>",
                "top_models1\"></i>",
                false,
                "</span"
            ),
            ["Ann "]
        );
    }

    #[tokio::test]
    async fn extracts_album() {
        let page = "<h1>Sea &amp; Sand</h1><div class=\"info-content\">
<i class=\"top_models1\"></i> Ann</span>
<a href=\"https://www.xasiat.com/albums/categories/travel/\">\n Travel\n</a>
<a href=\"https://www.xasiat.com/albums/tags/beach/\"> Beach</a>
</div><div class=\"images\"><a href=\"https://www.xasiat.com/get_image/2/abc/1.jpg/\"></a>
<a href=\"https://www.xasiat.com/get_image/2/abc/2.jpg/\"></a></div>";
        let (root, server) = serve(vec![html(page)]);
        let http = http("xasiat", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.xasiat.com/albums/12345/sea-sand/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = kw(&files[1]);
        assert_eq!(k["title"], "Sea & Sand");
        assert_eq!(k["model"], json!(["Ann"]));
        assert_eq!(k["tags"], json!(["Beach"]));
        assert_eq!(k["album_category"], "Travel");
        assert_eq!(
            k["album_url"],
            "https://www.xasiat.com/albums/12345/sea-sand/"
        );
        assert_eq!(k["album_id"], 12345);
        assert_eq!(k["count"], 2);
        assert_eq!(k["num"], 2);
        assert_eq!(path(&files[1]), "xasiat/Sea & Sand/2.jpg");
        assert_eq!(
            archive(&files[1]),
            r#"["{album_url}_{num}",["https://www.xasiat.com/albums/12345/sea-sand/","2"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /albums/12345/sea-sand/ "));
    }

    #[tokio::test]
    async fn queues_listing_albums() {
        let first = "<div class=\"item  \">\n <a href=\"https://www.xasiat.com/albums/1/a/\"></a>\
<div class=\"item  \"><a href=\"https://elsewhere.test/albums/2/b/\"></a>";
        let last = "<div class=\"item  \"><a href=\"https://www.xasiat.com/albums/3/c/\"></a><span>Next</span>";
        let (root, server) = serve(vec![html(first), html(last)]);
        let http = http("xasiat", root);
        let children = queued(extract(
            &http,
            Url::parse("https://www.xasiat.com/albums/tags/sea/").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].subcategory(), "tag");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with(
            "GET /albums/tags/sea/?mode=async&function=get_block\
&block_id=list_albums_common_albums_list&sort_by=post_date&from=1&_="
        ));
        assert!(requests[1].contains("&from=2&_="));
    }
}
