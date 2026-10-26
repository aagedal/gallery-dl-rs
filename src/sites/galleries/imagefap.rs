//! ImageFap galleries, images, folders and users, mirroring
//! gallery_dl/extractor/imagefap.py.
use crate::error;
use crate::sites::gdl::{
    Scan, captures, extr, extract_iter, group, jsonld, nameext_into, parse_int, partition, re,
    split_html, str_of, unescape,
};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use std::time::Duration;
use url::Url;

const ROOT: &str = "https://www.imagefap.com";
const BASE: &str = r"^https?://(?:www\.|beta\.)?imagefap\.com";
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
    /// `(id, user, profile, folder_id)`.
    Folder(bool, String, String, String),
    User(String, String),
}

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(
        re!(&format!(
            r"{BASE}/(?:gallery\.php\?gid=|gallery/|pictures/)(\d+)"
        )),
        url,
    ) {
        return Some(Target::Gallery(group(&c, 1).into()));
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}/photo/(\d+)")), url) {
        return Some(Target::Image(group(&c, 1).into()));
    }
    if let Some(c) = captures(
        re!(&format!(
            r"{BASE}/(?:(organizer/)|(?:usergallery\.php\?user(id)?=([^&#]+)&|profile/([^/?#]+)/galleries\?)folderid=)(\d+|-1)"
        )),
        url,
    ) {
        // Upstream's `folderid=(?!0\b)`.
        if c.get(1).is_some() || &c[5] != "0" {
            return Some(Target::Folder(
                c.get(2).is_some(),
                group(&c, 3).into(),
                group(&c, 4).into(),
                group(&c, 5).into(),
            ));
        }
    }
    let c = captures(
        re!(&format!(
            r"{BASE}/(?:profile(?:\.php\?user=|/)([^/?#]+)(?:/galleries(?:\?folderid=0)?)?|usergallery\.php\?userid=(\d+))(?:$|#)"
        )),
        url,
    )?;
    Some(Target::User(group(&c, 1).into(), group(&c, 2).into()))
}

/// `ImagefapExtractor.request`: GET with upstream's `request_interval`
/// and the human-verification redirect check.
async fn request(
    http: &Http,
    url: &Url,
    headers: &[(&str, &str)],
    wait: bool,
) -> crate::Result<(String, Url)> {
    if wait && !cfg!(test) {
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    let (page, final_url) = http.text_final(url, headers).await?;
    if final_url != *url && final_url.as_str().ends_with("/human-verification") {
        let msg = extr(&page, "<div class=\"mt-4", "<");
        let msg = partition(msg, ">")
            .2
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        return Err(error(format!(
            "ImageFap redirected to '{final_url}': '{msg}'"
        )));
    }
    Ok((page, final_url))
}

fn with_query(http: &Http, raw: &str, params: &[(&str, String)]) -> crate::Result<Url> {
    let mut url = http.api(raw)?;
    url.query_pairs_mut()
        .extend_pairs(params.iter().map(|(k, v)| (*k, v.as_str())));
    Ok(url)
}

/// `get_job_metadata`: gallery fields and the first image's id.
fn metadata(page: &str, gid: &str) -> (Map<String, Value>, String) {
    let mut scan = Scan::new(page);
    let mut data = Map::new();
    data.insert("gallery_id".into(), parse_int(gid).into());
    data.insert(
        "uploader".into(),
        scan.extr("porn picture gallery by ", " to see hottest")
            .into(),
    );
    data.insert("title".into(), unescape(scan.extr("<title>", "<")).into());
    data.insert(
        "description".into(),
        unescape(partition(scan.extr("id=\"gdesc_text\"", "<"), ">").2).into(),
    );
    let odd = |html: &str| -> Value {
        split_html(html)
            .into_iter()
            .skip(1)
            .step_by(2)
            .map(Value::from)
            .collect::<Vec<_>>()
            .into()
    };
    data.insert(
        "categories".into(),
        odd(scan.extr("id=\"cnt_cats\"", "</div>")),
    );
    data.insert("tags".into(), odd(scan.extr("id=\"cnt_tags\"", "</div>")));
    data.insert(
        "count".into(),
        parse_int(scan.extr(" 1 of ", " pics\"")).into(),
    );
    let image_id = scan.extr("id=\"img_ed_", "\"").to_owned();
    (data, image_id)
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported ImageFap URL"));
    };
    match target {
        Target::Gallery(gid) => gallery(http, gid),
        Target::Image(id) => crate::sites::once(async move {
            let (page, _) =
                request(http, &http.api(&format!("{ROOT}/photo/{id}/"))?, &[], false).await?;
            let mut scan = Scan::new(&page);
            let file = scan.extr("original=\"", "\"").to_owned();
            let image_id = scan.extr("id=\"imageid_input\" value=\"", "\"").to_owned();
            let gallery_id = scan
                .extr("id=\"galleryid_input\" value=\"", "\"")
                .to_owned();
            let info = jsonld(&page)?;
            let mut data = Map::new();
            data.insert("title".into(), unescape(str_of(&info, "name")).into());
            data.insert(
                "uploader".into(),
                info.get("author").cloned().unwrap_or(Value::Null),
            );
            data.insert(
                "date".into(),
                info.get("datePublished").cloned().unwrap_or(Value::Null),
            );
            data.insert(
                "width".into(),
                parse_int(&crate::sites::scalar(&info["width"])).into(),
            );
            data.insert(
                "height".into(),
                parse_int(&crate::sites::scalar(&info["height"])).into(),
            );
            data.insert("gallery_id".into(), parse_int(&gallery_id).into());
            data.insert("image_id".into(), parse_int(&image_id).into());
            nameext_into(&file, &mut data);
            let file = Url::parse(&file).map_err(|_| error("ImageFap image has no file"))?;
            Ok(crate::sites::events(crate::sites::post(
                &SITE,
                http,
                &IMAGE,
                vec![Item::new(file, data).ids(gallery_id, image_id)],
            )))
        }),
        Target::Folder(by_id, user, profile, folder) => {
            folder_galleries(http, by_id, user, profile, folder)
        }
        Target::User(user, user_id) => user_folders(http, user, user_id),
    }
}

fn gallery(http: &Http, gid: String) -> Events<'_> {
    // State: (gallery fields, first image id, idx, num); `None` fields before the first page.
    crate::sites::paginate(
        Some((None::<(Map<String, Value>, String)>, 0usize, 0i64)),
        move |state| {
            let gid = gid.clone();
            async move {
                let Some((meta, idx, num)) = state else {
                    return Ok(None);
                };
                let (data, image_id) = match meta {
                    Some(meta) => meta,
                    None => {
                        let url = http.api(&format!("{ROOT}/gallery/{gid}"))?;
                        let (page, _) = request(http, &url, &[], false).await?;
                        metadata(&page, &gid)
                    }
                };
                let photo = format!("{ROOT}/photo/{image_id}/");
                let url = with_query(
                    http,
                    &photo,
                    &[
                        ("gid", gid.clone()),
                        ("idx", idx.to_string()),
                        ("partial", "true".into()),
                    ],
                )?;
                let referer = format!("{photo}?pgid=&gid={image_id}&page=0");
                let headers = [
                    ("Content-Type", "application/x-www-form-urlencoded"),
                    ("X-Requested-With", "XMLHttpRequest"),
                    ("Referer", referer.as_str()),
                ];
                let (page, _) = request(http, &url, &headers, true).await?;
                let links = extract_iter(&page, "<a href=\"", "\"");
                let cnt = links.len();
                let mut number = num;
                let items: Vec<Item> = links
                    .into_iter()
                    .filter_map(|link| {
                        number += 1;
                        let mut fields = data.clone();
                        nameext_into(link, &mut fields);
                        fields.insert("num".into(), number.into());
                        let image = parse_int(&crate::sites::scalar(&fields["filename"]));
                        fields.insert("image_id".into(), image.into());
                        Some(
                            Item::new(Url::parse(link).ok()?, fields)
                                .ids(gid.clone(), image.to_string()),
                        )
                    })
                    .collect();
                let total = crate::sites::gdl::int_of(data.get("count"));
                let done = cnt == 0 || (cnt < 24 && number >= total);
                let next = (!done).then(|| (Some((data, image_id)), idx + cnt, number));
                let events = items
                    .into_iter()
                    .flat_map(|item| crate::sites::post(&SITE, http, &GALLERY, vec![item]))
                    .collect();
                Ok(Some((crate::sites::events(events), next)))
            }
        },
    )
}

fn folder_galleries(
    http: &Http,
    by_id: bool,
    user: String,
    profile: String,
    folder: String,
) -> Events<'_> {
    let (url, name) = if folder == "-1" {
        let url = if by_id {
            format!("{ROOT}/usergallery.php?userid={user}&folderid=-1")
        } else {
            let who = if user.is_empty() { &profile } else { &user };
            format!("{ROOT}/profile/{who}/galleries?folderid=-1")
        };
        (url, Some("Uncategorized".to_owned()))
    } else {
        (format!("{ROOT}/organizer/{folder}/"), None)
    };
    crate::sites::paginate(Some((0u64, name)), move |state| {
        let url = url.clone();
        async move {
            let Some((number, name)) = state else {
                return Ok(None);
            };
            let (page, _) = request(
                http,
                &with_query(http, &url, &[("page", number.to_string())])?,
                &[],
                number > 0,
            )
            .await?;
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
                let mut data = Map::new();
                data.insert("gallery_id".into(), gid.into());
                data.insert("title".into(), unescape(scan.extr("<b>", "<")).into());
                data.insert("folder".into(), unescape(&name).into());
                if let Ok(link) = Url::parse(&format!("{ROOT}/gallery/{gid}")) {
                    children.push((link, data));
                }
            }
            let next = (children.len() >= 20).then(|| (number + 1, Some(name)));
            Ok(Some((super::queue(http, &SITE, &FOLDER, children), next)))
        }
    })
}

fn user_folders(http: &Http, user: String, user_id: String) -> Events<'_> {
    let start = if user.is_empty() {
        format!("{ROOT}/usergallery.php?userid={user_id}")
    } else {
        format!("{ROOT}/profile/{user}/galleries")
    };
    crate::sites::paginate(
        Some((start, 0u64, None::<String>)),
        move |state| async move {
            let Some((url, number, name)) = state else {
                return Ok(None);
            };
            let (page, final_url) = request(
                http,
                &with_query(http, &url, &[("page", number.to_string())])?,
                &[],
                number > 0,
            )
            .await?;
            let (url, name) = match name {
                Some(name) => (url, name),
                None => {
                    let base = partition(final_url.as_str(), "?").0.to_owned();
                    let name = base.rsplit('/').nth(1).unwrap_or_default().to_owned();
                    (base, name)
                }
            };
            let mut folders: Vec<String> = extr(&page, " id=\"tgl_all\" value=\"", "\"")
                .trim_end_matches('|')
                .split('|')
                .map(str::to_owned)
                .collect();
            if folders.last().is_some_and(|f| f == "-1") {
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
                    let link = if folder == "-1" {
                        format!("{ROOT}/profile/{name}/galleries?folderid=-1")
                    } else {
                        format!("{ROOT}/organizer/{folder}/")
                    };
                    Some((Url::parse(&link).ok()?, Map::new()))
                })
                .collect();
            let next = number + 1;
            let next = page
                .contains(&format!("href=\"?page={next}\">{}</a>", next + 1))
                .then_some((url, next, Some(name)));
            Ok(Some((super::queue(http, &SITE, &USER, children), next)))
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{children, html, http as client, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.imagefap.com/gallery.php?gid=7102714"),
            Some(Target::Gallery("7102714".into()))
        );
        assert_eq!(
            t("https://beta.imagefap.com/pictures/7102714"),
            Some(Target::Gallery("7102714".into()))
        );
        assert_eq!(
            t("https://www.imagefap.com/photo/1962981893/"),
            Some(Target::Image("1962981893".into()))
        );
        assert_eq!(
            t("https://www.imagefap.com/organizer/409758"),
            Some(Target::Folder(
                false,
                String::new(),
                String::new(),
                "409758".into()
            ))
        );
        assert_eq!(
            t("https://www.imagefap.com/usergallery.php?userid=1981976&folderid=-1"),
            Some(Target::Folder(
                true,
                "1981976".into(),
                String::new(),
                "-1".into()
            ))
        );
        assert_eq!(
            t("https://www.imagefap.com/profile/BdRachel/galleries?folderid=-1"),
            Some(Target::Folder(
                false,
                String::new(),
                "BdRachel".into(),
                "-1".into()
            ))
        );
        assert_eq!(
            t("https://www.imagefap.com/profile/BdRachel/galleries?folderid=0"),
            Some(Target::User("BdRachel".into(), String::new()))
        );
        assert_eq!(
            t("https://www.imagefap.com/usergallery.php?userid=1862791"),
            Some(Target::User(String::new(), "1862791".into()))
        );
        assert_eq!(
            t("https://www.imagefap.com/profile.php?user=BdRachel"),
            Some(Target::User("BdRachel".into(), String::new()))
        );
    }

    #[tokio::test]
    async fn gallery_pages_partial_listing() {
        let page = r#"porn picture gallery by Uploader to see hottest <title>Some &amp; Title</title><div id="cnt_cats"><a>Cat</a></div>
            <div id="cnt_tags"><a>Tag</a></div><span title=" 1 of 2 pics"></span><a id="img_ed_555">"#;
        let partial = r#"<a href="https://cdn.imagefap.com/images/full/1/2/111.jpg"></a><a href="https://cdn.imagefap.com/images/full/1/2/222.jpg"></a>"#;
        let (root, server) = serve(vec![html(page), html(partial)]);
        let http = client("imagefap", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.imagefap.com/gallery/7").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[1]);
        assert_eq!(k["title"], "Some & Title");
        assert_eq!(k["image_id"], 222);
        assert_eq!(k["categories"], serde_json::json!(["Cat"]));
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "imagefap_7_0002_222.jpg");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("imagefap/7 Some & Title")
        );
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /photo/555/?gid=7&idx=0&partial=true "));
    }

    #[tokio::test]
    async fn folder_queues_galleries() {
        let page = r#"<td class'blk_galleries'><b>Folder</b> <tr id="gid-11"><b>One</b> <tr id="gid-12"><b>Two</b>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = client("imagefap", root);
        let queued = children(extract(
            &http,
            Url::parse("https://www.imagefap.com/organizer/409758").unwrap(),
        ))
        .await;
        assert_eq!(queued.len(), 2);
        server.join().unwrap();
    }
}
