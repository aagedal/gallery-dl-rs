//! 8muses comic albums, mirroring gallery_dl/extractor/8muses.py: each
//! album page embeds its pictures and sub-albums as obfuscated JSON; pages
//! are followed until `page >= pages`, sub-albums are queued.
use crate::sites::art::{date, extr, parse_iso};
use crate::sites::manga::text::{parse_int_value, partition_last};
use crate::sites::{Events, Http, Item, Route, Site, unescape};
use crate::{Result, error};
use serde_json::{Map, Value, json};
use url::Url;

const ROOT: &str = "https://comics.8muses.com";
static ALBUM: Route = Route::new(
    "album",
    "{page:>03}.{extension}",
    &["{category}", "{album[path]:I}"],
    "{hash}",
);

pub static SITE: Site = Site {
    category: "8muses",
    name: "8muses",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&ALBUM],
    api_key: false,
    cookies: None,
};

/// `(?:comics\.|www\.)?8muses\.com(/comics/album/[^?#]+)(\?[^#]+)?`: the
/// raw path and `?query` (or `""`).
fn target(url: &Url) -> Option<(String, String)> {
    if !crate::sites::plain(url)
        || !matches!(
            url.host_str(),
            Some("8muses.com" | "comics.8muses.com" | "www.8muses.com")
        )
    {
        return None;
    }
    let path = url.path();
    if path.len() <= "/comics/album/".len() || !path.starts_with("/comics/album/") {
        return None;
    }
    let query = url
        .query()
        .filter(|q| !q.is_empty())
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    Some((path.to_owned(), query))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some((path, params)) = target(&url) else {
        return crate::sites::fail(error("unsupported 8muses URL"));
    };
    let first = format!("{ROOT}{path}{params}");
    crate::sites::paginate(Some(first), move |state| {
        let path = path.clone();
        let params = params.clone();
        async move {
            let Some(url) = state else { return Ok(None) };
            let page = super::page(http, &url, &[]).await?;
            let data = unobfuscate(extr(
                &page,
                "id=\"ractive-public\" type=\"text/plain\">",
                "</script>",
            ))?;
            let mut events = Vec::new();
            if let Some(images) = data["pictures"].as_array().filter(|i| !i.is_empty()) {
                let album = make_album(&data["album"]);
                events = crate::sites::post(&SITE, http, &ALBUM, files(&album, images));
            }
            let children = sub_albums(&data);
            let page_no = data["page"].as_i64().unwrap_or(0);
            let next = (page_no < data["pages"].as_i64().unwrap_or(0)).then(|| {
                let (head, _, num) = partition_last(path.trim_end_matches('/'), "/");
                let base = match !num.is_empty() && num.chars().all(char::is_numeric) {
                    true => head,
                    false => &path,
                };
                format!("{ROOT}{base}/{}{params}", page_no + 1)
            });
            let stream = futures_util::StreamExt::chain(
                crate::sites::events(events),
                crate::sites::manga::queue_urls(http, &SITE, &ALBUM, children),
            );
            Ok(Some((futures_util::StreamExt::boxed(stream), next)))
        }
    })
}

/// `_unobfuscate`: rotate printable ASCII by 47 within `!`..=`~`.
fn unobfuscate(data: &str) -> Result<Value> {
    let data = unescape(data.trim_matches(['\t', '\n', '\r', ' ', '!']));
    let text: String = data
        .chars()
        .map(|c| match c {
            '!'..='~' => char::from(33 + ((c as u8 + 14) % 94)),
            c => c,
        })
        .collect();
    crate::sites::art::json_loads(&text, "8muses")
}

/// `_make_album`.
fn make_album(album: &Value) -> Value {
    let str_of = |key| crate::sites::art::str_of(album, key);
    json!({
        "id": album.get("id").cloned().unwrap_or(Value::Null),
        "path": str_of("path").split('/').collect::<Vec<_>>(),
        "title": album.get("name").cloned().unwrap_or(Value::Null),
        "private": album.get("isPrivate").cloned().unwrap_or(Value::Null),
        "url": format!("{ROOT}/comics/album/{}", str_of("permalink")),
        "parent": parse_int_value(album.get("parentId")),
        "views": parse_int_value(album.get("numberViews")),
        "likes": parse_int_value(album.get("numberLikes")),
        "date": date(parse_iso(str_of("updatedAt"))),
    })
}

fn files(album: &Value, images: &[Value]) -> Vec<Item> {
    let count = images.len();
    images
        .iter()
        .enumerate()
        .filter_map(|(index, image)| {
            let hash = image["publicUri"].as_str()?;
            let url = Url::parse(&format!("{ROOT}/image/fl/{hash}")).ok()?;
            let mut data = Map::new();
            data.insert("url".into(), url.as_str().into());
            data.insert("page".into(), (index + 1).into());
            data.insert("hash".into(), hash.into());
            data.insert("count".into(), count.into());
            data.insert("album".into(), album.clone());
            data.insert("extension".into(), "jpg".into());
            let id = crate::sites::scalar(&album["id"]);
            Some(Item::new(url, data).ids(id, hash))
        })
        .collect()
}

/// Public sub-albums, queued with their name and privacy flag.
fn sub_albums(data: &Value) -> Vec<(Url, Map<String, Value>)> {
    let Some(albums) = data["albums"].as_array() else {
        return Vec::new();
    };
    albums
        .iter()
        .filter_map(|album| {
            // Upstream logs "Private album" for entries without a permalink.
            let permalink = album["permalink"].as_str().filter(|p| !p.is_empty())?;
            let url = format!("{ROOT}/comics/album/{permalink}");
            let mut data = Map::new();
            data.insert("url".into(), url.clone().into());
            data.insert("name".into(), album["name"].clone());
            data.insert("private".into(), album["isPrivate"].clone());
            Some((Url::parse(&url).ok()?, data))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::manga::tests::{archive, path};
    use crate::sites::testing::{html, http, keywords, queued, serve};
    use futures_util::StreamExt;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://comics.8muses.com/comics/album/Fakku-Comics/a/b?sort=x"),
            Some(("/comics/album/Fakku-Comics/a/b".into(), "?sort=x".into()))
        );
        assert!(t("https://www.8muses.com/comics/album/X").is_some());
        assert!(t("http://8muses.com/comics/album/X/2").is_some());
        for raw in [
            "https://comics.8muses.com/comics/album/",
            "https://comics.8muses.com/comics/picture/X/1",
            "https://m.8muses.com/comics/album/X",
            "https://comics.8muses.com.evil.test/comics/album/X",
            "https://comics.8muses.com:8443/comics/album/X",
            "https://u:p@comics.8muses.com/comics/album/X",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    /// The site's obfuscation of a JSON document.
    fn obfuscate(data: &Value) -> String {
        let text: String = data
            .to_string()
            .chars()
            .map(|c| match c {
                '!'..='~' => char::from(33 + ((c as u8 - 33 + 47) % 94)),
                c => c,
            })
            .collect();
        format!(
            "<script id=\"ractive-public\" type=\"text/plain\">!{}</script>",
            text.replace('&', "&amp;").replace('<', "&lt;")
        )
    }

    #[test]
    fn unobfuscates_page_data() {
        let data = json!({"a": "<b> & ~!", "n": 1});
        let page = obfuscate(&data);
        assert_eq!(
            unobfuscate(extr(
                &page,
                "id=\"ractive-public\" type=\"text/plain\">",
                "</script>"
            ))
            .unwrap(),
            data
        );
    }

    #[tokio::test]
    async fn extracts_pages_and_queues_sub_albums() {
        let album = json!({"id": 7, "path": "Comics/Series/Part One", "name": "Part One",
            "isPrivate": false, "permalink": "Comics/Series/Part-One", "parentId": "6",
            "numberViews": 120, "numberLikes": "4", "updatedAt": "2024-02-03T04:05:06.000Z"});
        let first = json!({"page": 1, "pages": 2, "album": album,
            "pictures": [{"publicUri": "aaa"}, {"publicUri": "bbb"}],
            "albums": [{"permalink": "Comics/Series/Part-One/Extra", "name": "Extra",
                "isPrivate": false}, {"permalink": "", "name": "Hidden", "isPrivate": true}]});
        let second = json!({"page": 2, "pages": 2, "album": album,
            "pictures": [{"publicUri": "ccc"}]});
        let (root, server) = serve(vec![html(&obfuscate(&first)), html(&obfuscate(&second))]);
        let http = http("8muses", root);
        let url =
            Url::parse("https://comics.8muses.com/comics/album/Comics/Series/Part-One").unwrap();
        let mut files = Vec::new();
        let mut children = Vec::new();
        let mut events = extract(&http, url);
        while let Some(event) = events.next().await {
            match event.unwrap() {
                crate::event::ExtractionEvent::Media(media) => files.push(media),
                crate::event::ExtractionEvent::BeginChild(scope) => {
                    scope.control.cancel();
                    children.push(scope.metadata);
                }
                _ => {}
            }
        }
        assert_eq!(files.len(), 3);
        assert_eq!(
            files[1].url.as_str(),
            "https://comics.8muses.com/image/fl/bbb"
        );
        let k = keywords(&files[1]);
        assert_eq!(k["page"], 2);
        assert_eq!(k["count"], 2);
        assert_eq!(k["hash"], "bbb");
        assert_eq!(k["extension"], "jpg");
        assert_eq!(k["album"]["parent"], 6);
        assert_eq!(k["album"]["views"], 120);
        assert_eq!(k["album"]["likes"], 4);
        assert_eq!(k["album"]["date"], "2024-02-03 04:05:06");
        assert_eq!(
            k["album"]["url"],
            "https://comics.8muses.com/comics/album/Comics/Series/Part-One"
        );
        assert_eq!(path(&files[1]), "8muses/Comics/Series/Part One/002.jpg");
        assert_eq!(archive(&files[2]), r#"["{hash}",["ccc"]]"#);
        assert_eq!(keywords(&files[2])["page"], 1);
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].subcategory(), "album");
        assert_eq!(children[0].fields()["name"], "Extra");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /comics/album/Comics/Series/Part-One "));
        assert!(requests[1].starts_with("GET /comics/album/Comics/Series/Part-One/2 "));
    }

    #[tokio::test]
    async fn numbered_pages_continue_from_their_parent() {
        let data = json!({"page": 2, "pages": 3, "albums": [{"permalink": "A/B", "name": "B",
            "isPrivate": false}]});
        let last = json!({"page": 3, "pages": 3});
        let (root, server) = serve(vec![html(&obfuscate(&data)), html(&obfuscate(&last))]);
        let http = http("8muses", root);
        let url = Url::parse("https://comics.8muses.com/comics/album/A/2?x=1").unwrap();
        assert_eq!(queued(extract(&http, url)).await.len(), 1);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /comics/album/A/2?x=1 "));
        assert!(requests[1].starts_with("GET /comics/album/A/3?x=1 "));
    }
}
