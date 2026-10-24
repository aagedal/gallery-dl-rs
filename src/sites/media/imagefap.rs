//! ImageFap galleries, images, folders and user profiles, mirroring
//! gallery_dl/extractor/imagefap.py: gallery pages hold the metadata and
//! the photo viewer's partial pages list the files.
//!
//! Upstream waits 2-4 s between requests; this port keeps a 2 s gap.
//! Human-verification pages end extraction with their message; a browser
//! session from `--cookies` gets past them.
use crate::sites::art::{Scan, extract_iter, nameext_into, parse_int, split_html};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://www.imagefap.com";
const FILENAME: &str = "{category}_{gallery_id}_{num:?/_/>04}{filename}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{gallery_id} {title}"];
const ARCHIVE: &str = "{gallery_id}_{image_id}";
static GALLERY: Route = Route::new("gallery", FILENAME, DIRECTORY, ARCHIVE);
static IMAGE: Route = Route::new("image", FILENAME, DIRECTORY, ARCHIVE);
static FOLDER: Route = Route::new("folder", FILENAME, DIRECTORY, ARCHIVE);
static USER: Route = Route::new("user", FILENAME, DIRECTORY, ARCHIVE);

pub static SITE: Site = Site {
    category: "imagefap",
    name: "ImageFap",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&GALLERY, &IMAGE, &FOLDER, &USER],
    api_key: false,
    cookies: Some("imagefap.com"),
};

#[derive(Debug, PartialEq)]
enum Target {
    Gallery(String),
    Image(String),
    /// `(user)id`, the user or profile name and the folder ID.
    Folder {
        by_id: bool,
        user: Option<String>,
        profile: Option<String>,
        folder: String,
    },
    User {
        name: Option<String>,
        id: Option<String>,
    },
}

/// The leading `\d+` run of `text`.
fn digits(text: &str) -> Option<&str> {
    crate::sites::art::leading_digits(text)
}

/// The upstream patterns over the path, query and fragment, in class order.
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url)
        || !matches!(
            url.host_str(),
            Some("imagefap.com" | "www.imagefap.com" | "beta.imagefap.com")
        )
    {
        return None;
    }
    let rest = &url[url::Position::BeforePath..];
    // `/(?:gallery\.php\?gid=|gallery/|pictures/)(\d+)`
    for prefix in ["/gallery.php?gid=", "/gallery/", "/pictures/"] {
        if let Some(id) = rest.strip_prefix(prefix).and_then(digits) {
            return Some(Target::Gallery(id.into()));
        }
    }
    if let Some(id) = rest.strip_prefix("/photo/").and_then(digits) {
        return Some(Target::Image(id.into()));
    }
    folder(rest).or_else(|| user(rest))
}

/// `/(?:organizer/|(?:usergallery\.php\?user(id)?=([^&#]+)&|profile/([^/?#]+)
/// /galleries\?)folderid=(?!0\b))(\d+|-1)`.
fn folder(rest: &str) -> Option<Target> {
    let id = |text: &str| {
        if text.starts_with("-1") {
            Some("-1".to_owned())
        } else {
            digits(text).map(str::to_owned)
        }
    };
    if let Some(tail) = rest.strip_prefix("/organizer/") {
        return Some(Target::Folder {
            by_id: false,
            user: None,
            profile: None,
            folder: id(tail)?,
        });
    }
    let (by_id, user, profile, tail) =
        if let Some(tail) = rest.strip_prefix("/usergallery.php?user") {
            let (by_id, tail) = match tail.strip_prefix("id") {
                Some(tail) => (true, tail),
                None => (false, tail),
            };
            let tail = tail.strip_prefix('=')?;
            let end = tail.find(['&', '#'])?;
            let tail_rest = tail[end..].strip_prefix('&')?;
            if end == 0 {
                return None;
            }
            (by_id, Some(tail[..end].to_owned()), None, tail_rest)
        } else {
            let tail = rest.strip_prefix("/profile/")?;
            let end = tail.find(['/', '?', '#'])?;
            if end == 0 {
                return None;
            }
            let tail_rest = tail[end..].strip_prefix("/galleries?")?;
            (false, None, Some(tail[..end].to_owned()), tail_rest)
        };
    let value = tail.strip_prefix("folderid=")?;
    // `(?!0\b)`: folder 0 is the user's gallery list.
    if value.starts_with('0') && !value[1..].starts_with(super::word_char) {
        return None;
    }
    Some(Target::Folder {
        by_id,
        user,
        profile,
        folder: id(value)?,
    })
}

/// `/(?:profile(?:\.php\?user=|/)([^/?#]+)(?:/galleries(?:\?folderid=0)?)?
/// |usergallery\.php\?userid=(\d+))(?:$|#)`.
fn user(rest: &str) -> Option<Target> {
    let ends = |tail: &str| tail.is_empty() || tail.starts_with('#');
    if let Some(tail) = rest.strip_prefix("/usergallery.php?userid=") {
        let id = digits(tail)?;
        return ends(&tail[id.len()..]).then(|| Target::User {
            name: None,
            id: Some(id.into()),
        });
    }
    let tail = rest
        .strip_prefix("/profile.php?user=")
        .or_else(|| rest.strip_prefix("/profile/"))?;
    let end = tail.find(['/', '?', '#']).unwrap_or(tail.len());
    let (name, tail) = tail.split_at(end);
    if name.is_empty() {
        return None;
    }
    let tail = match tail.strip_prefix("/galleries") {
        Some(tail) => tail.strip_prefix("?folderid=0").unwrap_or(tail),
        None => tail,
    };
    ends(tail).then(|| Target::User {
        name: Some(name.into()),
        id: None,
    })
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported ImageFap URL"));
    };
    match target {
        Target::Gallery(gid) => gallery(http, gid),
        Target::Image(id) => crate::sites::once(async move {
            let (_, page) = fetch(http, &format!("{ROOT}/photo/{id}/"), &[]).await?;
            let item = image(&page)?;
            Ok(crate::sites::events(crate::sites::post(
                &SITE,
                http,
                &IMAGE,
                vec![item],
            )))
        }),
        Target::Folder {
            by_id,
            user,
            profile,
            folder,
        } => folder_galleries(http, by_id, user.or(profile).unwrap_or_default(), folder),
        Target::User { name, id } => user_folders(http, name, id),
    }
}

/// Upstream `request`: a request after the interval, failing on a redirect
/// to the human-verification page. Returns the final URL and the page.
async fn fetch(http: &Http, url: &str, headers: &[(&str, &str)]) -> Result<(Url, String)> {
    throttle().await;
    let (location, page) = http.page(&http.api(url)?, headers).await?;
    if location.path().ends_with("/human-verification") {
        let message = crate::sites::art::extr(&page, "<div class=\"mt-4", "<");
        let message = message.split_once('>').map_or("", |(_, text)| text);
        let message = message.split_whitespace().collect::<Vec<_>>().join(" ");
        if !message.is_empty() {
            return Err(error(format!("ImageFap: '{message}'")));
        }
    }
    Ok((location, page))
}

/// Keep at least two seconds between ImageFap requests of this process.
async fn throttle() {
    #[cfg(not(test))]
    {
        static LAST: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
        let wait = {
            let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
            let now = std::time::Instant::now();
            let next = last.map_or(now, |last| {
                (last + std::time::Duration::from_secs(2)).max(now)
            });
            *last = Some(next);
            next - now
        };
        tokio::time::sleep(wait).await;
    }
}

/// `ImagefapGalleryExtractor.items`: the gallery page's metadata, then the
/// photo viewer's partial pages, one post per page.
fn gallery(http: &Http, gid: String) -> Events<'_> {
    crate::sites::once(async move {
        let (_, page) = fetch(http, &format!("{ROOT}/gallery/{gid}"), &[]).await?;
        let (data, image_id) = metadata(&page, &gid);
        let total = data.get("count").and_then(Value::as_i64).unwrap_or(0);
        let state = Some((0usize, 0i64));
        Ok(crate::sites::paginate(state, move |state| {
            let data = data.clone();
            let gid = gid.clone();
            let image_id = image_id.clone();
            async move {
                let Some((idx, num)) = state else {
                    return Ok(None);
                };
                let viewer = format!("{ROOT}/photo/{image_id}/");
                let referer = format!("{viewer}?pgid=&gid={image_id}&page=0");
                let url = format!("{viewer}?gid={gid}&idx={idx}&partial=true");
                let headers = [
                    ("Content-Type", "application/x-www-form-urlencoded"),
                    ("X-Requested-With", "XMLHttpRequest"),
                    ("Referer", referer.as_str()),
                ];
                let (_, page) = fetch(http, &url, &headers).await?;
                let links = extract_iter(&page, "<a href=\"", "\"");
                let count = links.len();
                let mut items = Vec::new();
                for (index, raw) in links.into_iter().enumerate() {
                    let mut image = data.clone();
                    let num = num + index as i64 + 1;
                    nameext_into(raw, &mut image);
                    image.insert("num".into(), num.into());
                    let filename = crate::sites::scalar(&image["filename"]);
                    image.insert("image_id".into(), parse_int(&filename, 0).into());
                    if let Some(url) = crate::sites::media_url(raw, &["imagefap.com"]) {
                        items.push(gallery_item(url, image));
                    }
                }
                let num = num + count as i64;
                // Upstream: stop on an empty page, or a short one once all
                // files are listed.
                let next =
                    (count != 0 && (count >= 24 || num < total)).then_some((idx + count, num));
                let events = crate::sites::post(&SITE, http, &GALLERY, items);
                Ok(Some((crate::sites::events(events), next)))
            }
        }))
    })
}

/// `get_job_metadata`: the gallery page fields and the first image's ID.
fn metadata(page: &str, gid: &str) -> (Map<String, Value>, String) {
    let mut scan = Scan::new(page);
    let mut data = Map::new();
    data.insert("gallery_id".into(), parse_int(gid, 0).into());
    data.insert(
        "uploader".into(),
        scan.extr("porn picture gallery by ", " to see hottest")
            .into(),
    );
    let title = crate::sites::unescape(scan.extr("<title>", "<"));
    data.insert("title".into(), title.into());
    let description = scan.extr("id=\"gdesc_text\"", "<");
    let description = description.split_once('>').map_or("", |(_, text)| text);
    data.insert(
        "description".into(),
        crate::sites::unescape(description).into(),
    );
    for (key, begin) in [
        ("categories", "id=\"cnt_cats\""),
        ("tags", "id=\"cnt_tags\""),
    ] {
        let parts = split_html(scan.extr(begin, "</div>"));
        let odd: Vec<Value> = parts
            .into_iter()
            .skip(1)
            .step_by(2)
            .map(Value::from)
            .collect();
        data.insert(key.into(), odd.into());
    }
    data.insert(
        "count".into(),
        parse_int(scan.extr(" 1 of ", " pics\""), 0).into(),
    );
    let image_id = scan.extr("id=\"img_ed_", "\"").to_owned();
    (data, image_id)
}

fn gallery_item(url: Url, data: Map<String, Value>) -> Item {
    let gallery = data.get("gallery_id").map(crate::sites::scalar);
    let image = data.get("image_id").map(crate::sites::scalar);
    let uploader = data
        .get("uploader")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let title = data.get("title").and_then(Value::as_str).map(str::to_owned);
    Item::new(url, data)
        .ids(gallery.unwrap_or_default(), image.unwrap_or_default())
        .user(uploader)
        .caption(title)
}

/// `ImagefapImageExtractor.get_image`: the `original` URL, IDs and JSON-LD.
fn image(page: &str) -> Result<Item> {
    let mut scan = Scan::new(page);
    let raw = scan.extr("original=\"", "\"");
    let image_id = scan.extr("id=\"imageid_input\" value=\"", "\"");
    let gallery_id = scan.extr("id=\"galleryid_input\" value=\"", "\"");
    let Some(info) = jsonld(page) else {
        return Err(error("ImageFap image page has no JSON-LD data"));
    };
    let field = |key: &str| info.get(key).cloned().unwrap_or(Value::Null);
    let int = |key: &str| match info.get(key) {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        Some(Value::String(s)) => parse_int(s, 0),
        _ => 0,
    };
    let mut data = Map::new();
    let name = info.get("name").and_then(Value::as_str).unwrap_or_default();
    data.insert("title".into(), crate::sites::unescape(name).into());
    data.insert("uploader".into(), field("author"));
    data.insert("date".into(), field("datePublished"));
    data.insert("width".into(), int("width").into());
    data.insert("height".into(), int("height").into());
    data.insert("gallery_id".into(), parse_int(gallery_id, 0).into());
    data.insert("image_id".into(), parse_int(image_id, 0).into());
    nameext_into(raw, &mut data);
    let Some(url) = crate::sites::media_url(raw, &["imagefap.com"]) else {
        return Err(error("ImageFap image page has no file URL"));
    };
    Ok(gallery_item(url, data))
}

/// `Extractor._extract_jsonld`.
fn jsonld(page: &str) -> Option<Map<String, Value>> {
    let raw = crate::sites::extr(page, "<script type=\"application/ld+json\">", "</script>")
        .filter(|s| !s.is_empty())
        .or_else(|| crate::sites::extr(page, "<script type='application/ld+json'>", "</script>"))?;
    serde_json::from_str::<Value>(raw)
        .ok()?
        .as_object()
        .cloned()
}

/// `ImagefapFolderExtractor`: queue each gallery of a folder, 20 per page.
fn folder_galleries(http: &Http, by_id: bool, user: String, folder: String) -> Events<'_> {
    let (base, name) = if folder == "-1" {
        let base = if by_id {
            format!("{ROOT}/usergallery.php?userid={user}&folderid=-1")
        } else {
            format!("{ROOT}/profile/{user}/galleries?folderid=-1")
        };
        (base, Some("Uncategorized".to_owned()))
    } else {
        (format!("{ROOT}/organizer/{folder}/"), None)
    };
    crate::sites::paginate(Some((0u32, name)), move |state| {
        let base = base.clone();
        async move {
            let Some((number, name)) = state else {
                return Ok(None);
            };
            let separator = if base.contains('?') { '&' } else { '?' };
            let (_, page) = fetch(http, &format!("{base}{separator}page={number}"), &[]).await?;
            let mut scan = Scan::new(&page);
            let name = match name {
                Some(name) => name,
                None => scan.extr("class'blk_galleries'><b>", "</b>").to_owned(),
            };
            let mut children = Vec::new();
            loop {
                let gid = scan.extr(" id=\"gid-", "\"");
                if gid.is_empty() {
                    break;
                }
                let title = scan.extr("<b>", "<");
                let mut data = Map::new();
                data.insert("gallery_id".into(), gid.into());
                data.insert("title".into(), crate::sites::unescape(title).into());
                data.insert("folder".into(), crate::sites::unescape(&name).into());
                if let Ok(url) = Url::parse(&format!("{ROOT}/gallery/{gid}"))
                    && matches!(target(&url), Some(Target::Gallery(_)))
                {
                    children.push((url, data));
                }
            }
            let next = (children.len() >= 20 && number < 100_000).then(|| (number + 1, Some(name)));
            Ok(Some((queue_all(http, &FOLDER, children), next)))
        }
    })
}

/// `ImagefapUserExtractor`: queue each folder of the profile's gallery list.
fn user_folders(http: &Http, name: Option<String>, id: Option<String>) -> Events<'_> {
    let base = match (&name, &id) {
        (Some(name), _) => format!("{ROOT}/profile/{name}/galleries"),
        (None, id) => format!(
            "{ROOT}/usergallery.php?userid={}",
            id.as_deref().unwrap_or("")
        ),
    };
    crate::sites::paginate(
        Some((base, 0u32, None::<String>)),
        move |state| async move {
            let Some((base, number, user)) = state else {
                return Ok(None);
            };
            let separator = if base.contains('?') { '&' } else { '?' };
            let (location, page) =
                fetch(http, &format!("{base}{separator}page={number}"), &[]).await?;
            // The first response names the user: later pages request its URL
            // without query, as upstream does.
            let (base, user) = match user {
                Some(user) => (base, user),
                None => {
                    let raw = location.as_str();
                    let raw = raw.split('?').next().unwrap_or_default();
                    let user = raw.rsplit('/').nth(1).unwrap_or_default().to_owned();
                    (format!("{ROOT}{}", location.path()), user)
                }
            };
            let list = crate::sites::art::extr(&page, " id=\"tgl_all\" value=\"", "\"");
            let mut folders: Vec<String> = list
                .trim_end_matches('|')
                .split('|')
                .map(str::to_owned)
                .collect();
            if folders.last().is_some_and(|last| last == "-1") {
                let last = folders.pop().unwrap_or_default();
                if number == 0 {
                    folders.insert(0, last);
                }
            } else if folders.first().is_none_or(String::is_empty) {
                return Ok(None);
            }
            let children = folders
                .iter()
                .filter_map(|folder| {
                    let raw = if folder == "-1" {
                        format!("{ROOT}/profile/{user}/galleries?folderid=-1")
                    } else {
                        format!("{ROOT}/organizer/{folder}/")
                    };
                    let url = Url::parse(&raw).ok()?;
                    matches!(target(&url), Some(Target::Folder { .. })).then(|| (url, Map::new()))
                })
                .collect();
            let number = number + 1;
            let more = page.contains(&format!("href=\"?page={number}\">{}</a>", number + 1));
            let next = (more && number < 100_000).then_some((base, number, Some(user)));
            Ok(Some((queue_all(http, &USER, children), next)))
        },
    )
}

fn queue_all<'a>(
    http: &'a Http,
    route: &'static Route,
    children: Vec<(Url, Map<String, Value>)>,
) -> Events<'a> {
    futures_util::StreamExt::boxed(futures_util::StreamExt::flatten(
        futures_util::stream::iter(children.into_iter().map(move |(child, data)| {
            crate::sites::queue(http, "imagefap", route.subcategory, child, &data)
        })),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, queued, response, serve};
    use futures_util::StreamExt;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        for raw in [
            "https://www.imagefap.com/gallery/7102714",
            "https://www.imagefap.com/pictures/7102714/x",
            "https://beta.imagefap.com/gallery.php?gid=7102714",
            "https://imagefap.com/gallery/7102714abc",
        ] {
            assert_eq!(t(raw), Some(Target::Gallery("7102714".into())), "{raw}");
        }
        assert_eq!(
            t("https://www.imagefap.com/photo/1962981893/?pgid=&gid=7102714"),
            Some(Target::Image("1962981893".into()))
        );
        assert_eq!(
            t("https://www.imagefap.com/organizer/409758"),
            Some(Target::Folder {
                by_id: false,
                user: None,
                profile: None,
                folder: "409758".into()
            })
        );
        assert_eq!(
            t("https://www.imagefap.com/usergallery.php?userid=1981976&folderid=-1"),
            Some(Target::Folder {
                by_id: true,
                user: Some("1981976".into()),
                profile: None,
                folder: "-1".into()
            })
        );
        assert_eq!(
            t("https://www.imagefap.com/profile/BdRachel/galleries?folderid=3"),
            Some(Target::Folder {
                by_id: false,
                user: None,
                profile: Some("BdRachel".into()),
                folder: "3".into()
            })
        );
        // `(?!0\b)` only rejects a whole `0`.
        assert_eq!(
            t("https://www.imagefap.com/profile/Lucy/galleries?folderid=0x"),
            Some(Target::Folder {
                by_id: false,
                user: None,
                profile: Some("Lucy".into()),
                folder: "0".into()
            })
        );
        let user = |name: &str| {
            Some(Target::User {
                name: Some(name.into()),
                id: None,
            })
        };
        assert_eq!(
            t("https://www.imagefap.com/profile/LucyRae/galleries"),
            user("LucyRae")
        );
        assert_eq!(
            t("https://www.imagefap.com/profile/LucyRae/galleries?folderid=0"),
            user("LucyRae")
        );
        assert_eq!(
            t("https://www.imagefap.com/profile.php?user=Lucy"),
            user("Lucy")
        );
        assert_eq!(t("https://beta.imagefap.com/profile/Lucy#x"), user("Lucy"));
        assert_eq!(
            t("https://www.imagefap.com/usergallery.php?userid=1862791"),
            Some(Target::User {
                name: None,
                id: Some("1862791".into())
            })
        );
        for raw in [
            "https://www.imagefap.com/",
            "https://www.imagefap.com/profile/Lucy/other",
            "https://www.imagefap.com/usergallery.php?userid=12x",
            "https://cdn.imagefap.com/gallery/1",
            "https://www.imagefap.com.evil.test/gallery/1",
            "https://evilimagefap.com/gallery/1",
            "https://www.imagefap.com:8080/gallery/1",
            "https://u:p@www.imagefap.com/gallery/1",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[tokio::test]
    async fn gallery_pages_through_the_viewer() {
        let page = "porn picture gallery by Uploader to see hottest <title>Nice &amp; Pics</title>\
            <div id=\"gdesc_text\" class=\"x\">Desc &amp; more</div>\
            <div id=\"cnt_cats\"><a>Cat1</a>, <a>Cat2</a></div>\
            <div id=\"cnt_tags\"><a>tag</a></div>\
            <img title=\"Image 1 of 25 pics\"> <a id=\"img_ed_555\">";
        let first: String = (1..=24)
            .map(|n| {
                format!("<a href=\"https://cdn.imagefap.com/images/full/1/2/{n}00.jpg?end=1\">")
            })
            .collect();
        let second = "<a href=\"https://cdn.imagefap.com/images/full/1/2/2500.png\">";
        let (root, server) = serve(vec![html(page), html(&first), html(second)]);
        let http = http("imagefap", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.imagefap.com/pictures/42/Nice").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 25);
        let k = keywords(&files[24]);
        assert_eq!(k["gallery_id"], 42);
        assert_eq!(k["uploader"], "Uploader");
        assert_eq!(k["title"], "Nice & Pics");
        assert_eq!(k["description"], "Desc & more");
        assert_eq!(k["categories"], serde_json::json!(["Cat1", "Cat2"]));
        assert_eq!(k["tags"], serde_json::json!(["tag"]));
        assert_eq!(k["count"], 25);
        assert_eq!(k["num"], 25);
        assert_eq!(k["image_id"], 2500);
        let (file, dir) = crate::sites::default_templates(&files[24]).unwrap();
        assert_eq!(
            file.render(&files[24]).unwrap(),
            "imagefap_42_0025_2500.png"
        );
        assert_eq!(
            dir.render(&files[24]).unwrap().unwrap(),
            std::path::PathBuf::from("imagefap/42 Nice & Pics")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{gallery_id}_{image_id}",["42","100"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /gallery/42 "));
        assert!(
            requests[1].starts_with("GET /photo/555/?gid=42&idx=0&partial=true "),
            "{}",
            requests[1]
        );
        assert!(
            requests[1]
                .contains("referer: https://www.imagefap.com/photo/555/?pgid=&gid=555&page=0")
        );
        assert!(requests[1].contains("x-requested-with: XMLHttpRequest"));
        assert!(requests[2].starts_with("GET /photo/555/?gid=42&idx=24&partial=true "));
    }

    #[tokio::test]
    async fn image_page_and_human_verification() {
        let page = "<img data-original=\"https://cdn.imagefap.com/images/full/9/8/77.jpg?x\">\
            <input id=\"imageid_input\" value=\"77\"><input id=\"galleryid_input\" value=\"42\">\
            <script type=\"application/ld+json\">{\"name\": \"A &amp; B\", \"author\": \"Up\",\
            \"datePublished\": \"2020-01-02\", \"width\": \"800\", \"height\": 600}</script>";
        let redirect = response(
            "302 Found\r\nLocation: /human-verification",
            "text/html",
            b"",
        );
        let check = html("<div class=\"mt-4 x\">Please   verify\n you</div>");
        let (root, server) = serve(vec![html(page), redirect, check]);
        let http = http("imagefap", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.imagefap.com/photo/77/").unwrap(),
        ))
        .await;
        let k = keywords(&files[0]);
        assert_eq!(k["title"], "A & B");
        assert_eq!(k["uploader"], "Up");
        assert_eq!(k["date"], "2020-01-02");
        assert_eq!((&k["width"], &k["height"]), (&800.into(), &600.into()));
        assert_eq!((&k["gallery_id"], &k["image_id"]), (&42.into(), &77.into()));
        let (file, _) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "imagefap_42_77.jpg");
        let events = extract(
            &http,
            Url::parse("https://www.imagefap.com/photo/78/").unwrap(),
        );
        let errors: Vec<String> = crate::event::media_only(events)
            .filter_map(|m| async move { m.err().map(|e| e.to_string()) })
            .collect()
            .await;
        assert_eq!(errors, ["ImageFap: 'Please verify you'"]);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn folder_and_user_queue_children() {
        let mut folder = String::from("<td class'blk_galleries'><b>My &amp; Folder</b>");
        for n in 1..=20 {
            folder.push_str(&format!("<tr id=\"gid-{n}\"><b>G{n}</b>"));
        }
        let (root, server) = serve(vec![
            html(&folder),
            html("<tr id=\"gid-21\"><b>Last</b>"),
            html("<input id=\"tgl_all\" value=\"5|6|-1|\"> <a href=\"?page=1\">2</a>"),
            html("<input id=\"tgl_all\" value=\"7|-1|\">"),
        ]);
        let http = http("imagefap", root);
        let children = queued(extract(
            &http,
            Url::parse("https://www.imagefap.com/organizer/409758/").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 21);
        let children = queued(extract(
            &http,
            Url::parse("https://www.imagefap.com/profile/Lucy/galleries").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 4);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /organizer/409758/?page=0 "));
        assert!(requests[1].starts_with("GET /organizer/409758/?page=1 "));
        assert!(requests[2].starts_with("GET /profile/Lucy/galleries?page=0 "));
        assert!(requests[3].starts_with("GET /profile/Lucy/galleries?page=1 "));
    }

    #[tokio::test]
    async fn folder_children_carry_gallery_data() {
        let (root, server) = serve(vec![html(
            "<td class'blk_galleries'><b>F &amp; G</b><tr id=\"gid-9\"><b>Nine &amp; Ten</b>",
        )]);
        let http = http("imagefap", root);
        let children = queued(extract(
            &http,
            Url::parse("https://www.imagefap.com/organizer/1/").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].subcategory(), "folder");
        server.join().unwrap();
    }
}
