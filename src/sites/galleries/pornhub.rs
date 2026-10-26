//! Pornhub albums, GIFs and user listings, mirroring
//! gallery_dl/extractor/pornhub.py. User URLs dispatch to `photos` (default)
//! and `gifs` through the `include` option.
use crate::error;
use crate::sites::gdl::{
    Scan, captures, date, extract_iter, group, int_of, nameext_into, parse_int, parse_iso,
    partition, re, remove_html, rpartition, split_html, unescape,
};
use crate::sites::{Events, Http, Item, Route, Site};
use futures_util::StreamExt;
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://www.pornhub.com";
const BASE: &str = r"^https?://(?:[\w-]+\.)?pornhub\.com";

static GALLERY: Route = Route::new(
    "gallery",
    "{num:>03}_{id}.{extension}",
    &["{category}", "{user}", "{gallery[id]} {gallery[title]}"],
    "{id}",
);
static GIF: Route = Route::new(
    "gif",
    "{id} {title}.{extension}",
    &["{category}", "{user}", "gifs"],
    "{id}",
);
static USER: Route = Route::new("user", "{filename}.{extension}", &["{category}"], "");
static PHOTOS: Route = Route::new("photos", "{filename}.{extension}", &["{category}"], "");
static GIFS: Route = Route::new("gifs", "{filename}.{extension}", &["{category}"], "");

pub static SITE: Site = Site {
    category: "pornhub",
    name: "Pornhub",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&GALLERY, &GIF, &USER, &PHOTOS, &GIFS],
    api_key: false,
    cookies: Some("pornhub.com"),
};

#[derive(Debug, PartialEq)]
enum Target {
    Gallery(String),
    Gif(String),
    User(String),
    Photos(String, String),
    Gifs(String, String),
}

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(re!(&format!(r"{BASE}/album/(\d+)")), url) {
        return Some(Target::Gallery(group(&c, 1).into()));
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}/gif/(\d+)")), url) {
        return Some(Target::Gif(group(&c, 1).into()));
    }
    if let Some(c) = captures(
        re!(&format!(r"{BASE}/((?:users|model|pornstar)/[^/?#]+)/?$")),
        url,
    ) {
        return Some(Target::User(group(&c, 1).into()));
    }
    if let Some(c) = captures(
        re!(&format!(
            r"{BASE}/((?:users|model|pornstar)/[^/?#]+)/(photos(?:/[^/?#]+)?)"
        )),
        url,
    ) {
        return Some(Target::Photos(group(&c, 1).into(), group(&c, 2).into()));
    }
    let c = captures(
        re!(&format!(
            r"{BASE}/((?:users|model|pornstar)/[^/?#]+)/(gifs(?:/[^/?#]+)?)"
        )),
        url,
    )?;
    Some(Target::Gifs(group(&c, 1).into(), group(&c, 2).into()))
}

fn age_cookie(http: &Http) -> crate::Result<()> {
    http.set_cookie(
        &http.api(ROOT)?,
        "accessAgeDisclaimerPH=1; Domain=.pornhub.com; Path=/",
    );
    Ok(())
}

async fn gallery(http: &Http, id: &str) -> crate::Result<Vec<Item>> {
    let page = http.text(&http.api(&format!("{ROOT}/album/{id}"))?).await?;
    let mut scan = Scan::new(&page);
    let title = scan.extr("<title>", "</title>").to_owned();
    let token = scan.extr("data-token=\"", "\"").to_owned();
    let score = scan
        .extr("<div id=\"albumGreenBar\" style=\"width:", "\"")
        .to_owned();
    let views = scan
        .extr("<div id=\"viewsPhotAlbumCounter\">", "<")
        .to_owned();
    let tags = scan.extr("<div id=\"photoTagsBox\"", "<script").to_owned();
    let first = scan.extr("<a href=\"/photo/", "\"").to_owned();
    let (title, _, user) = rpartition(&title, " - ");
    let user: String = user
        .chars()
        .take(user.chars().count().saturating_sub(14))
        .collect();
    let tags: Vec<String> = split_html(&tags).into_iter().skip(2).collect();
    let mut data = Map::new();
    data.insert("user".into(), unescape(&user).into());
    data.insert(
        "gallery".into(),
        serde_json::json!({"id": parse_int(id), "title": unescape(title), "score": parse_int(partition(&score, "%").0),
            "views": parse_int(partition(&views, " ").0), "tags": tags}),
    );
    let mut url = http.api(&format!("{ROOT}/api/v1/album/{id}/show_album_json"))?;
    url.query_pairs_mut().append_pair("token", &token);
    let json = http.json(&url).await?;
    let photos = json
        .get("photos")
        .and_then(Value::as_object)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| error("Pornhub album is not accessible"))?;
    // Follow the `next` chain from the first photo; fall back to API order.
    let mut ordered = Vec::new();
    let mut key = first.clone();
    loop {
        let Some(image) = photos.get(&key) else {
            ordered = photos.values().cloned().collect();
            break;
        };
        ordered.push(image.clone());
        key = crate::sites::scalar(&image["next"]);
        if key == first {
            break;
        }
    }
    Ok(ordered
        .into_iter()
        .enumerate()
        .filter_map(|(index, img)| {
            let raw = crate::sites::scalar(&img["img_large"]);
            let mut image = Map::new();
            image.insert("url".into(), raw.as_str().into());
            image.insert("caption".into(), img["caption"].clone());
            image.insert("id".into(), int_of(img.get("id")).into());
            image.insert("views".into(), int_of(img.get("times_viewed")).into());
            image.insert("score".into(), int_of(img.get("vote_percent")).into());
            image.insert("num".into(), (index + 1).into());
            image.extend(data.clone());
            nameext_into(&raw, &mut image);
            let file_id = crate::sites::scalar(&image["id"]);
            Some(Item::new(Url::parse(&raw).ok()?, image).ids(id, file_id))
        })
        .collect())
}

async fn gif(http: &Http, id: &str) -> crate::Result<Item> {
    let page = http.text(&http.api(&format!("{ROOT}/gif/{id}"))?).await?;
    let mut scan = Scan::new(&page);
    let mut gif = Map::new();
    gif.insert("id".into(), id.into());
    let tags: Vec<String> = scan
        .extr("data-context-tag='", "'")
        .split(',')
        .map(str::to_owned)
        .collect();
    gif.insert("tags".into(), tags.into());
    gif.insert("title".into(), scan.extr("\"name\": \"", "\"").into());
    let raw = scan.extr("\"contentUrl\": \"", "\"").to_owned();
    gif.insert("url".into(), raw.as_str().into());
    gif.insert(
        "date".into(),
        date(parse_iso(scan.extr("\"uploadDate\": \"", "\""))),
    );
    gif.insert(
        "viewkey".into(),
        scan.extr("From this video: <a href=\"/view_video.php?viewkey=", "\"")
            .into(),
    );
    gif.insert(
        "timestamp".into(),
        scan.extr("lass=\"directLink tstamp\" rel=\"nofollow\">", "<")
            .into(),
    );
    gif.insert(
        "user".into(),
        remove_html(scan.extr("Created by:", "</div>")).into(),
    );
    nameext_into(&raw, &mut gif);
    Ok(Item::new(
        Url::parse(&raw).map_err(|_| error("Pornhub GIF has no file URL"))?,
        gif,
    )
    .ids(id, id))
}

/// `_pagination`: POSTed listing pages, following profile redirects.
fn listing<'a>(
    http: &'a Http,
    route: &'static Route,
    user: String,
    path: String,
    needle: &'static str,
    kind: &'static str,
) -> Events<'a> {
    let path = if path.contains('/') {
        path
    } else {
        format!("{path}/public")
    };
    let first = format!("{ROOT}/{user}/{path}/ajax");
    crate::sites::paginate(Some((first, 1i64)), move |state| {
        let path = path.clone();
        async move {
            let Some((mut url, page)) = state else {
                return Ok(None);
            };
            let html = loop {
                let mut request = http.api(&url)?;
                request
                    .query_pairs_mut()
                    .append_pair("page", &page.to_string());
                let referer = url[..url.len().saturating_sub(5)].to_owned();
                match http
                    .post_or_location(
                        &request,
                        &[
                            ("Referer", &referer),
                            ("X-Requested-With", "XMLHttpRequest"),
                        ],
                    )
                    .await?
                {
                    Ok(html) => break html,
                    Err(location) => url = format!("{ROOT}{}/{path}/ajax", location.path()),
                }
            };
            let ids: Vec<String> = extract_iter(&html, needle, "\"")
                .into_iter()
                .map(str::to_owned)
                .collect();
            if ids.is_empty() {
                return Ok(None);
            }
            let children: Vec<Url> = ids
                .iter()
                .filter_map(|id| Url::parse(&format!("{ROOT}/{kind}/{id}")).ok())
                .collect();
            let events = futures_util::stream::iter(children)
                .map(move |url| {
                    crate::sites::queue(http, SITE.category, route.subcategory, url, &Map::new())
                })
                .flatten()
                .boxed();
            Ok(Some((events, Some((url, page + 1)))))
        }
    })
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Pornhub URL"));
    };
    if let Err(e) = age_cookie(http) {
        return crate::sites::fail(e);
    }
    match target {
        Target::Gallery(id) => crate::sites::once(async move {
            let items = gallery(http, &id).await?;
            Ok(crate::sites::events(crate::sites::post(
                &SITE, http, &GALLERY, items,
            )))
        }),
        Target::Gif(id) => crate::sites::once(async move {
            Ok(crate::sites::events(crate::sites::post(
                &SITE,
                http,
                &GIF,
                vec![gif(http, &id).await?],
            )))
        }),
        Target::Photos(user, path) => {
            listing(http, &PHOTOS, user, path, "id=\"albumphoto", "album")
        }
        Target::Gifs(user, path) => listing(http, &GIFS, user, path, "id=\"gif", "gif"),
        Target::User(user) => crate::sites::once(async move {
            let views = [
                ("photos", format!("{ROOT}/{user}/photos")),
                ("gifs", format!("{ROOT}/{user}/gifs")),
            ];
            let selected: Vec<Url> = crate::sites::gdl::dispatch(http, &views, &["photos"])?
                .into_iter()
                .filter_map(|(_, url)| Url::parse(url).ok())
                .collect();
            Ok(futures_util::stream::iter(selected)
                .map(move |url| {
                    crate::sites::queue(http, SITE.category, USER.subcategory, url, &Map::new())
                })
                .flatten()
                .boxed())
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{children, html, http as client, json, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.pornhub.com/album/19289801"),
            Some(Target::Gallery("19289801".into()))
        );
        assert_eq!(
            t("https://www.pornhub.com/gif/43726891"),
            Some(Target::Gif("43726891".into()))
        );
        assert_eq!(
            t("https://www.pornhub.com/pornstar/danika-mori"),
            Some(Target::User("pornstar/danika-mori".into()))
        );
        assert_eq!(
            t("https://www.pornhub.com/users/flyings0l0/photos/private"),
            Some(Target::Photos(
                "users/flyings0l0".into(),
                "photos/private".into()
            ))
        );
        assert_eq!(
            t("https://www.pornhub.com/model/bossgirl/gifs/video"),
            Some(Target::Gifs("model/bossgirl".into(), "gifs/video".into()))
        );
    }

    #[tokio::test]
    async fn extracts_album_in_chain_order() {
        let page = r#"<title>Holiday &amp; Sun - xyz | Pornhub.com</title><div data-token="TOK"></div>
            <div id="albumGreenBar" style="width:87%"></div><div id="viewsPhotAlbumCounter">1,234 views</div>
            <div id="photoTagsBox"><b>Tags</b><a>x</a><a>beach</a><script><a href="/photo/2">"#;
        let (root, server) = serve(vec![
            html(page),
            json(
                json!({"photos": {"1": {"id": "1", "next": 2, "img_large": "https://ei.phncdn.com/1.jpg", "caption": "", "times_viewed": "5", "vote_percent": "90"},
                                   "2": {"id": "2", "next": 1, "img_large": "https://ei.phncdn.com/2.jpg", "caption": "c", "times_viewed": "6", "vote_percent": "80"}}}),
            ),
        ]);
        let http = client("pornhub", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.pornhub.com/album/19289801").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].url.as_str(), "https://ei.phncdn.com/2.jpg");
        let k = keywords(&files[0]);
        assert_eq!(k["user"], "xyz");
        assert_eq!(k["gallery"]["score"], 87);
        assert_eq!(k["gallery"]["tags"], json!(["x", "beach"]));
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "002_1.jpg");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("pornhub/xyz/19289801 Holiday & Sun")
        );
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /api/v1/album/19289801/show_album_json?token=TOK "));
    }

    #[tokio::test]
    async fn photos_page_and_user_dispatch() {
        let (root, server) = serve(vec![
            html(r#"<li id="albumphoto123"></li><li id="albumphoto456"></li>"#),
            html("<ul></ul>"),
        ]);
        let http = client("pornhub", root);
        assert_eq!(
            children(extract(
                &http,
                Url::parse("https://www.pornhub.com/model/bossgirl/photos").unwrap()
            ))
            .await
            .len(),
            2
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("POST /model/bossgirl/photos/public/ajax?page=1 "));
        assert!(requests[1].starts_with("POST /model/bossgirl/photos/public/ajax?page=2 "));

        let (root, _server) = serve(vec![]);
        let http = client("pornhub", root).with_options(Some(std::sync::Arc::new(
            json!({"include": "all"}).as_object().unwrap().clone(),
        )));
        assert_eq!(
            children(extract(
                &http,
                Url::parse("https://www.pornhub.com/model/bossgirl").unwrap()
            ))
            .await
            .len(),
            2
        );
    }
}
