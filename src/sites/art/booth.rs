//! BOOTH items, shops and category listings, mirroring
//! gallery_dl/extractor/booth.py: item pages supply the original image URLs
//! and the CSRF token for the item's JSON description.
use super::{extr, extract_iter, parse_int, raw_segments, str_of};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://booth.pm";
/// Upstream `_init` sets the `adult=t` cookie for every request.
const COOKIE: (&str, &str) = ("Cookie", "adult=t");
const FILENAME: &str = "{num:>02} {filename}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{shop[name]}", "{id} {name}"];
const ARCHIVE: &str = "{id}_{filename}";
static ITEM: Route = Route::new("item", FILENAME, DIRECTORY, ARCHIVE);
static SHOP: Route = Route::new("shop", FILENAME, DIRECTORY, ARCHIVE);
static CATEGORY: Route = Route::new("category", FILENAME, DIRECTORY, ARCHIVE);

pub static SITE: Site = Site {
    category: "booth",
    name: "BOOTH",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&ITEM, &SHOP, &CATEGORY],
    api_key: false,
};

#[derive(Debug, PartialEq)]
enum Target {
    Item(String),
    /// The shop origin, e.g. `https://SHOP.booth.pm`.
    Shop(String),
    /// Path and query below `https://booth.pm`.
    Category(String),
}

/// `[a-z]{2}(?:-[^/?#]+)?`, the optional language segment.
fn language(segment: &str) -> bool {
    let (code, region) = segment.split_once('-').unwrap_or((segment, "x"));
    code.len() == 2 && code.bytes().all(|b| b.is_ascii_lowercase()) && !region.is_empty()
}

fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) {
        return None;
    }
    let host = url.host_str()?;
    // `[\w-]+\.` admits one label of word characters and hyphens.
    let shop = match host.strip_suffix(".booth.pm") {
        Some(label)
            if (1..=63).contains(&label.len())
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') =>
        {
            Some(label)
        }
        None if host == "booth.pm" => None,
        _ => return None,
    };
    let parts = raw_segments(url);
    let rest = match parts.as_slice() {
        [lang, rest @ ..] if language(lang) => rest,
        rest => rest,
    };
    if let ["items", id, ..] = rest {
        let len = id.bytes().take_while(u8::is_ascii_digit).count();
        if (1..=20).contains(&len) {
            return Some(Target::Item(id[..len].to_owned()));
        }
    }
    match shop {
        // `www.booth.pm` is the main site, not a shop.
        Some("www") => None,
        Some(label) => Some(Target::Shop(format!("{}://{label}.booth.pm", url.scheme()))),
        None => match parts.as_slice() {
            [lang, "browse", _, ..] if language(lang) => {
                let mut path = url.path().to_owned();
                if let Some(query) = url.query() {
                    path.push('?');
                    path.push_str(query);
                }
                path.starts_with(&format!("/{lang}/browse/"))
                    .then_some(Target::Category(path))
            }
            _ => None,
        },
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Item(id)) => crate::sites::once(item(http, id)),
        Some(Target::Shop(root)) => pagination(http, root.clone(), format!("{root}/items"), true),
        Some(Target::Category(path)) => {
            pagination(http, ROOT.into(), format!("{ROOT}{path}"), false)
        }
        None => crate::sites::fail(error("unsupported BOOTH URL")),
    }
}

async fn item(http: &Http, id: String) -> Result<Events<'_>> {
    let page_url = format!("{ROOT}/ja/items/{id}");
    let fallback = http.option_str("strategy") == Some("fallback");
    let page = if fallback {
        None
    } else {
        Some(http.text_with(&http.api(&page_url)?, &[COOKIE]).await?)
    };
    let token = page.as_deref().map_or("", |page| {
        extr(page, "name=\"csrf-token\" content=\"", "\"")
    });
    let mut headers = vec![
        ("Accept", "application/json"),
        ("Content-Type", "application/json"),
        ("Sec-Fetch-Dest", "empty"),
        ("Sec-Fetch-Mode", "cors"),
        ("Sec-Fetch-Site", "same-origin"),
        ("Priority", "u=4"),
        COOKIE,
    ];
    if !token.is_empty() {
        headers.push(("X-CSRF-Token", token));
    }
    let data = http
        .json_with(&http.api(&format!("{page_url}.json"))?, &headers)
        .await?;
    let items = parse_item(data, page.as_deref())?;
    Ok(crate::sites::events(crate::sites::post(
        &SITE, http, &ITEM, items,
    )))
}

/// The item description merged into each original image. Without a page
/// (`strategy` = `fallback`) the images come from the JSON description.
pub(crate) fn parse_item(data: Value, page: Option<&str>) -> Result<Vec<Item>> {
    let Value::Object(mut item) = data else {
        return Err(error("BOOTH item is not an object"));
    };
    let category = item.remove("category").unwrap_or(Value::Null);
    item.insert("booth_category".into(), category);
    let published = super::parse_iso(
        item.get("published_at")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    item.insert("date".into(), super::date(published));
    let tags: Vec<Value> = item
        .get("tags")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|tag| tag.get("name").cloned())
        .collect();
    item.insert("tags".into(), tags.into());
    let images = item.remove("images");
    let files: Vec<String> = match page {
        Some(page) => extract_iter(page, "data-origin=\"", "\"")
            .into_iter()
            .map(str::to_owned)
            .collect(),
        None => images
            .as_ref()
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|image| str_of(image, "original").replace("_base_resized", ""))
            .collect(),
    };
    let mut shop = match item.remove("shop") {
        Some(Value::Object(shop)) => shop,
        _ => Map::new(),
    };
    let thumbnail = shop
        .get("thumbnail_url")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let shop_id = thumbnail.rsplit('/').nth(2).unwrap_or_default();
    shop.insert("id".into(), parse_int(shop_id, 0).into());
    item.insert("count".into(), files.len().into());
    let uuid = files
        .first()
        .and_then(|url| url.split('/').nth(3))
        .map_or(Value::Null, Value::from);
    shop.insert("uuid".into(), uuid);
    let shop_name = shop.get("name").and_then(Value::as_str).map(str::to_owned);
    item.insert("shop".into(), Value::Object(shop));

    let id = item.get("id").map(crate::sites::scalar).unwrap_or_default();
    let name = item.get("name").and_then(Value::as_str).map(str::to_owned);
    let mut items = Vec::new();
    for (index, raw) in files.into_iter().enumerate() {
        let Some(url) = super::file_url(&raw, ROOT) else {
            continue;
        };
        let mut fields = item.clone();
        fields.insert("url".into(), raw.clone().into());
        fields.insert("num".into(), (index + 1).into());
        super::nameext_into(&raw, &mut fields);
        let media = format!(
            "{id}_{}",
            str_of(&Value::Object(fields.clone()), "filename")
        );
        items.push(
            Item::new(url, fields)
                .ids(id.clone(), media)
                .user(shop_name.clone())
                .caption(name.clone())
                .timestamp(published),
        );
    }
    Ok(items)
}

/// A listing page's entries and its `rel="next"` link.
pub(crate) fn parse_listing(page: &str, json: bool) -> Result<(Vec<Value>, Option<String>)> {
    let entries = if json {
        extract_iter(page, " data-item=\"", "\"")
            .into_iter()
            .map(|item| super::json_loads(&crate::sites::unescape(item), "BOOTH"))
            .collect::<Result<Vec<_>>>()?
    } else {
        extract_iter(page, "item-card__title", "</div>")
            .into_iter()
            .map(|item| crate::sites::unescape(extr(item, "href=\"", "\"")).into())
            .collect()
    };
    let next = extr(page, "rel=\"next\" class=\"nav-item\" href=\"", "\"");
    Ok((
        entries,
        (!next.is_empty()).then(|| crate::sites::unescape(next)),
    ))
}

fn pagination(http: &Http, root: String, first: String, json: bool) -> Events<'_> {
    let subcategory = if json { "shop" } else { "category" };
    crate::sites::paginate(Some((first, 0u32)), move |state| {
        let root = root.clone();
        async move {
            let Some((url, count)) = state else {
                return Ok(None);
            };
            let page = http.text_with(&http.api(&url)?, &[COOKIE]).await?;
            let (entries, next) = parse_listing(&page, json)?;
            let mut children = Vec::new();
            for entry in entries {
                let (raw, data) = match entry {
                    Value::Object(data) => (
                        str_of(&Value::Object(data.clone()), "shop_item_url").to_owned(),
                        data,
                    ),
                    Value::String(raw) => (raw, Map::new()),
                    _ => continue,
                };
                let Some(child) = super::file_url(&raw, &root) else {
                    continue;
                };
                if matches!(target(&child), Some(Target::Item(_))) {
                    children.push((child, data));
                }
            }
            let next = next
                .filter(|_| count < 10_000)
                .map(|next| (format!("{root}{next}"), count + 1));
            let events = futures_util::StreamExt::boxed(futures_util::StreamExt::flatten(
                futures_util::stream::iter(children.into_iter().map(move |(child, data)| {
                    crate::sites::queue(http, "booth", subcategory, child, &data)
                })),
            ));
            Ok(Some((events, next)))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, json, keywords, media, serve};
    use futures_util::StreamExt;
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://booth.pm/ja/items/4693741"),
            Some(Target::Item("4693741".into()))
        );
        assert_eq!(
            t("https://booth.pm/zh-tw/items/1"),
            Some(Target::Item("1".into()))
        );
        assert_eq!(
            t("https://shop-name.booth.pm/items/12"),
            Some(Target::Item("12".into()))
        );
        assert_eq!(
            t("https://booth.pm/items/12"),
            Some(Target::Item("12".into()))
        );
        assert_eq!(
            t("https://shop-name.booth.pm/"),
            Some(Target::Shop("https://shop-name.booth.pm".into()))
        );
        assert_eq!(
            t("https://booth.pm/ja/browse/Music?page=2"),
            Some(Target::Category("/ja/browse/Music?page=2".into()))
        );
        for raw in [
            "https://booth.pm/",
            "https://booth.pm/ja/browse/",
            "https://booth.pm/jap/items/1",
            "https://booth.pm/ja/items/x",
            "https://www.booth.pm/",
            "https://a.b.booth.pm/",
            "https://shop.booth.pm.evil.com/",
            "https://evilbooth.pm/items/1",
            "https://booth.pm:8443/items/1",
            "https://u:p@booth.pm/items/1",
            "https://u:p@shop.booth.pm/",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn item_json() -> Value {
        json!({"id": 12, "name": "Thing", "category": {"name": "Music"},
            "published_at": "2023-04-05T06:07:08.000+09:00",
            "tags": [{"name": "a"}, {"name": "b"}],
            "images": [{"original": "x"}],
            "shop": {"name": "Shop", "thumbnail_url": "https://booth.pximg.net/c/48x48/users/321/icon_image/u.png"}})
    }

    const ITEM_PAGE: &str = r#"<meta name="csrf-token" content="tok"><div data-origin="https://booth.pximg.net/uuid-1/i/12/a_base_resized.jpg"></div><div data-origin="https://booth.pximg.net/uuid-1/i/12/b.png"></div>"#;

    #[tokio::test]
    async fn extracts_item_images() {
        let (root, server) = serve(vec![html(ITEM_PAGE), json(item_json())]);
        let http = http("booth", root);
        let files = media(extract(
            &http,
            Url::parse("https://booth.pm/en/items/12").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[1]);
        assert_eq!(k["booth_category"]["name"], "Music");
        assert_eq!(k["date"], "2023-04-04 21:07:08");
        assert_eq!(k["tags"], json!(["a", "b"]));
        assert_eq!(k["shop"]["id"], 321);
        assert_eq!(k["shop"]["uuid"], "uuid-1");
        assert_eq!(k["count"], 2);
        assert_eq!(k["num"], 2);
        assert!(!k.contains_key("images"));
        assert_eq!(k["category"], "booth");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "01 a_base_resized.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("booth/Shop/12 Thing")
        );
        let (_, identity) = crate::sites::archive_key(&files[1]).unwrap();
        assert_eq!(identity, r#"["{id}_{filename}",["12","b"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /ja/items/12 "));
        assert!(requests[0].to_ascii_lowercase().contains("cookie: adult=t"));
        assert!(requests[1].starts_with("GET /ja/items/12.json "));
        assert!(
            requests[1]
                .to_ascii_lowercase()
                .contains("x-csrf-token: tok")
        );
    }

    #[tokio::test]
    async fn fallback_strategy_uses_json_images() {
        let mut data = item_json();
        data["images"] =
            json!([{"original": "https://booth.pximg.net/uuid-2/i/12/a_base_resized.jpg"}]);
        let (root, server) = serve(vec![json(data)]);
        let options = json!({"strategy": "fallback"}).as_object().cloned();
        let http = http("booth", root).with_options(options.map(std::sync::Arc::new));
        let files = media(extract(
            &http,
            Url::parse("https://booth.pm/ja/items/12").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].url.as_str(),
            "https://booth.pximg.net/uuid-2/i/12/a.jpg"
        );
        assert_eq!(keywords(&files[0])["shop"]["uuid"], "uuid-2");
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("GET /ja/items/12.json "));
    }

    #[tokio::test]
    async fn missing_item_is_an_error() {
        let (root, server) = serve(vec![crate::sites::testing::response(
            "404 Not Found",
            "text/html",
            b"gone",
        )]);
        let http = http("booth", root);
        let events = extract(&http, Url::parse("https://booth.pm/ja/items/9").unwrap());
        let results: Vec<_> = crate::event::media_only(events).collect().await;
        assert!(results.iter().any(Result::is_err));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn shop_queues_items_across_pages() {
        let first = r#"<li data-item="{&quot;shop_item_url&quot;:&quot;https://shop.booth.pm/items/12&quot;,&quot;id&quot;:12}"></li>
            <a rel="next" class="nav-item" href="/items?page=2">"#;
        let second = r#"<li data-item="{&quot;shop_item_url&quot;:&quot;https://evil.test/items/1&quot;}"></li>"#;
        let (root, server) = serve(vec![
            html(first),
            html(ITEM_PAGE),
            json(item_json()),
            html(second),
        ]);
        let http = http("booth", root);
        let files = media(extract(
            &http,
            Url::parse("https://shop.booth.pm/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /items "));
        assert!(requests[1].starts_with("GET /ja/items/12 "));
        assert!(requests[3].starts_with("GET /items?page=2 "));
    }

    #[test]
    fn parses_category_listing() {
        let page = r#"<div class="item-card__title"><a href="https://booth.pm/ja/items/5?a=1&amp;b=2">x</a></div>
            <a rel="next" class="nav-item" href="/ja/browse/Music?page=2&amp;x=1">"#;
        let (entries, next) = parse_listing(page, false).unwrap();
        assert_eq!(entries, vec![json!("https://booth.pm/ja/items/5?a=1&b=2")]);
        assert_eq!(next.as_deref(), Some("/ja/browse/Music?page=2&x=1"));
    }
}
