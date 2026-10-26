//! Motherless media, galleries and groups, mirroring
//! gallery_dl/extractor/motherless.py.
use crate::error;
use crate::sites::gdl::{
    Scan, captures, date, extr, extract_iter, group, nameext_into, parse_int, re, remove_html,
    rpartition, strptime, unescape,
};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://motherless.com";
const BASE: &str = r"^https?://motherless\.com";

static MEDIA: Route = Route::new("media", "{id} {title}.{extension}", &["{category}"], "{id}");
static GALLERY: Route = Route::new(
    "gallery",
    "{id} {title}.{extension}",
    &["{category}", "{uploader}", "{gallery_id} {gallery_title}"],
    "{gallery_id}_{id}",
);
static GROUP: Route = Route::new(
    "group",
    "{id} {title}.{extension}",
    &["{category}", "{uploader}", "{group_id} {group_title}"],
    "{group_id}_{id}",
);

pub static SITE: Site = Site {
    category: "motherless",
    name: "Motherless",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&MEDIA, &GALLERY, &GROUP],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Media(String),
    /// Type (`I`, `V`, `G` or none) and id.
    Gallery(String, String),
    Group(String, String),
}

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(
        re!(&format!(
            r"{BASE}/((?:g/[^/?#]+/|G[IV]?[A-Z0-9]+/)?[A-Z0-9]+)"
        )),
        url,
    ) {
        // Upstream's `(?!G)` before the media id.
        if !rpartition(&c[1], "/").2.starts_with('G') {
            return Some(Target::Media(group(&c, 1).into()));
        }
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}/G([IVG])?([A-Z0-9]+)/?$")), url) {
        return Some(Target::Gallery(group(&c, 1).into(), group(&c, 2).into()));
    }
    captures(re!(&format!(r"{BASE}/g([iv]?)/?([a-z0-9_]+)/?$")), url)
        .map(|c| Target::Group(group(&c, 1).into(), group(&c, 2).into()))
}

/// `MotherlessExtractor.request`: error pages are "not found".
async fn page(http: &Http, url: &str) -> crate::Result<String> {
    let page = http.text(&http.api(url)?).await?;
    if page.contains("<div class=\"error-page")
        || page.contains(">The page you're looking for cannot be found.<")
    {
        return Err(error("Motherless page not found"));
    }
    Ok(page)
}

/// `_parse_datetime`: absolute dates or "N h/d ago".
fn parse_datetime(text: &str) -> Value {
    if !text.contains(" ago") {
        return date(strptime(text, "%d  %b  %Y"));
    }
    let value = parse_int(text.get(..text.len().saturating_sub(5)).unwrap_or_default());
    let hours = text.len() >= 5 && text.as_bytes()[text.len() - 5] == b'h';
    let delta = if hours {
        time::Duration::hours(value)
    } else {
        time::Duration::days(value)
    };
    let then = (time::OffsetDateTime::now_utc() - delta).replace_time(time::Time::MIDNIGHT);
    date(Some(then.unix_timestamp()))
}

fn title_of(page: &str) -> String {
    let title = extr(page, "<title>", "<");
    unescape(title.rfind(" | ").map_or(title, |i| &title[..i]))
}

fn gallery_title(gallery_id: &str, page: &str) -> String {
    let title = extr(
        extr(page, "<h1 class=\"content-title\">", "</h1>"),
        "From the gallery:",
        "<",
    );
    if !title.is_empty() {
        return unescape(title.trim());
    }
    if page.contains(&format!(" href=\"/G{gallery_id}\"")) {
        return title_of(page);
    }
    String::new()
}

fn group_title(page: &str) -> String {
    unescape(
        extr(
            extr(page, "<h1 class=\"group-bio-name\">", "</h1>"),
            ">",
            "<",
        )
        .trim(),
    )
}

/// `_extract_media`: one media page.
async fn media(http: &Http, path: &str) -> crate::Result<Map<String, Value>> {
    let page = page(http, &format!("{ROOT}/{path}")).await?;
    let mut scan = Scan::new(&page);
    let (prefix, _, media_id) = rpartition(path, "/");
    let mut data = Map::new();
    data.insert("id".into(), media_id.into());
    let title = scan.extr("<title>", "<").to_owned();
    data.insert(
        "title".into(),
        unescape(title.rfind(" | ").map_or(title.as_str(), |i| &title[..i])).into(),
    );
    data.insert("type".into(), scan.extr("__mediatype = '", "'").into());
    data.insert("group".into(), scan.extr("__group = '", "'").into());
    data.insert("url".into(), scan.extr("__fileurl = '", "'").into());
    let tags: Vec<Value> =
        extract_iter(scan.extr("class=\"media-meta-tags\">", "</div>"), ">#", "<")
            .into_iter()
            .map(|t| unescape(t).into())
            .collect();
    data.insert("tags".into(), tags.into());
    data.insert(
        "views".into(),
        parse_int(&scan.extr("class=\"count\">", " ").replace(',', "")).into(),
    );
    data.insert(
        "favorites".into(),
        parse_int(&scan.extr("class=\"count\">", " ").replace(',', "")).into(),
    );
    data.insert(
        "date".into(),
        parse_datetime(scan.extr("class=\"count\">", "<")),
    );
    data.insert(
        "uploader".into(),
        unescape(scan.extr("class=\"username\">", "<").trim()).into(),
    );
    if let Some(gallery) = prefix.strip_prefix('G') {
        data.insert("gallery_id".into(), gallery.into());
        data.insert("gallery_title".into(), gallery_title(gallery, &page).into());
    } else if let Some(group) = prefix.strip_prefix("g/") {
        data.insert("group_id".into(), group.into());
        data.insert("group_title".into(), group_title(&page).into());
    }
    Ok(data)
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Motherless URL"));
    };
    let (kind, route, prefix, id) = match target {
        Target::Media(path) => {
            return crate::sites::once(async move {
                let mut file = media(http, &path).await?;
                let url = crate::sites::scalar(&file["url"]);
                nameext_into(&url, &mut file);
                let id = crate::sites::scalar(&file["id"]);
                let target = Url::parse(&url).map_err(|_| error("Motherless media has no file"))?;
                Ok(crate::sites::events(crate::sites::post(
                    &SITE,
                    http,
                    &MEDIA,
                    vec![Item::new(target, file).ids(id.clone(), id)],
                )))
            });
        }
        Target::Gallery(kind, id) => ("gallery", &GALLERY, format!("G{kind}"), id),
        Target::Group(kind, id) => ("group", &GROUP, format!("g{kind}/"), id),
    };
    if prefix == "G" || prefix == "g/" {
        let children = if kind == "gallery" {
            vec![format!("{ROOT}/GI{id}"), format!("{ROOT}/GV{id}")]
        } else {
            vec![format!("{ROOT}/gi/{id}"), format!("{ROOT}/gv/{id}")]
        };
        let children = children
            .into_iter()
            .filter_map(|u| Some((Url::parse(&u).ok()?, Map::new())))
            .collect();
        return super::queue(http, &SITE, route, children);
    }
    crate::sites::paginate(
        Some((
            format!("{ROOT}/{prefix}{id}"),
            None::<Map<String, Value>>,
            0usize,
        )),
        move |state| {
            let id = id.clone();
            async move {
                let Some((url, data, num)) = state else {
                    return Ok(None);
                };
                let html = page(http, &url).await?;
                let data = match data {
                    Some(data) => data,
                    None => {
                        let mut scan = Scan::new(&html);
                        let title = if kind == "gallery" {
                            gallery_title(&id, &html)
                        } else {
                            group_title(&html)
                        };
                        let creator = remove_html(
                            scan.extr(&format!("class=\"{kind}-member-username\">"), "</"),
                        );
                        let mut data = Map::new();
                        data.insert(format!("{kind}_id"), id.as_str().into());
                        data.insert(format!("{kind}_title"), title.into());
                        data.insert(format!("{kind}_creator"), creator.as_str().into());
                        data.insert("uploader".into(), creator.into());
                        let count = rpartition(scan.extr("<span class=\"active\">", ")"), "(")
                            .2
                            .replace(',', "");
                        data.insert("count".into(), parse_int(&count).into());
                        data
                    }
                };
                let mut number = num;
                let mut items = Vec::new();
                for thumb in extract_iter(&html, "class=\"thumb-container", "</div>") {
                    let mut scan = Scan::new(thumb);
                    let codename = scan.extr("data-codename=\"", "\"").to_owned();
                    scan.extr("data-mediatype=\"", "\"");
                    let thumbnail = scan.extr("class=\"static\" src=\"", "\"").to_owned();
                    number += 1;
                    let Ok(mut file) = media(http, &codename).await else {
                        continue;
                    };
                    let uploader = file.get("uploader").cloned();
                    file.extend(data.clone());
                    file.insert("num".into(), number.into());
                    file.insert("thumbnail".into(), thumbnail.into());
                    file.insert("uploader".into(), uploader.unwrap_or(Value::Null));
                    if kind == "group" {
                        file.insert("group".into(), data["group_id"].clone());
                    }
                    let url = crate::sites::scalar(&file["url"]);
                    nameext_into(&url, &mut file);
                    let media_id = crate::sites::scalar(&file["id"]);
                    if let Ok(url) = Url::parse(&url) {
                        items.push(Item::new(url, file).ids(id.clone(), media_id));
                    }
                }
                let next = unescape(extr(&html, "<link rel=\"next\" href=\"", "\""));
                let next = (!next.is_empty()).then_some((next, Some(data), number));
                let events = items
                    .into_iter()
                    .flat_map(|item| crate::sites::post(&SITE, http, route, vec![item]))
                    .collect();
                Ok(Some((crate::sites::events(events), next)))
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{
        children, html, http as client, keywords, media as files_of, serve,
    };

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://motherless.com/B0168DB"),
            Some(Target::Media("B0168DB".into()))
        );
        assert_eq!(
            t("https://motherless.com/GI8C2A5D2A/F3F4B5C"),
            Some(Target::Media("GI8C2A5D2A/F3F4B5C".into()))
        );
        assert_eq!(
            t("https://motherless.com/g/bigtitsgallery/A1B2C3"),
            Some(Target::Media("g/bigtitsgallery/A1B2C3".into()))
        );
        assert_eq!(
            t("https://motherless.com/GI8C2A5D2A"),
            Some(Target::Gallery("I".into(), "8C2A5D2A".into()))
        );
        assert_eq!(
            t("https://motherless.com/G8C2A5D2A"),
            Some(Target::Gallery(String::new(), "8C2A5D2A".into()))
        );
        assert_eq!(
            t("https://motherless.com/gv/movie_scenes"),
            Some(Target::Group("v".into(), "movie_scenes".into()))
        );
    }

    #[test]
    fn parses_dates() {
        assert_eq!(parse_datetime("05  Jan  2023"), "2023-01-05 00:00:00");
    }

    #[tokio::test]
    async fn media_page_fields() {
        let page = r#"<title>Nice Clip | MOTHERLESS.COM</title> __mediatype = 'video'; __group = ''; __fileurl = 'https://cdn5-videos.motherlessmedia.com/videos/B0168DB.mp4';
            <div class="media-meta-tags"><a>#tag1</a><a>#tag2</a></div><span class="count">1,234 Views</span><span class="count">5 Favorites</span><span class="count">05  Jan  2023</span>
            <span class="username"> jane </span>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = client("motherless", root);
        let files = files_of(extract(
            &http,
            Url::parse("https://motherless.com/B0168DB").unwrap(),
        ))
        .await;
        let k = keywords(&files[0]);
        assert_eq!(
            (
                k["views"].as_i64(),
                k["favorites"].as_i64(),
                k["uploader"].as_str()
            ),
            (Some(1234), Some(5), Some("jane"))
        );
        assert_eq!(k["tags"], serde_json::json!(["tag1", "tag2"]));
        let (file, _) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "B0168DB Nice Clip.mp4");
        server.join().unwrap();
    }

    #[tokio::test]
    async fn untyped_gallery_queues_images_and_videos() {
        let (root, server) = serve(vec![]);
        let http = client("motherless", root);
        assert_eq!(
            children(extract(
                &http,
                Url::parse("https://motherless.com/G8C2A5D2A").unwrap()
            ))
            .await
            .len(),
            2
        );
        server.join().unwrap();
    }
}
