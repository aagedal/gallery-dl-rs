//! Pornhub photo albums, GIFs and user listings, mirroring
//! gallery_dl/extractor/pornhub.py. Albums list their photos through the
//! album JSON API, walked in the order of each photo's `next` link; user
//! photo and GIF listings are AJAX pages requested by POST. The
//! `accessAgeDisclaimerPH` cookie is set like upstream.
use crate::sites::art::{
    Scan, date, extract_iter, include, on_host, parse_int, parse_iso, segment, split_html,
    subdomain,
};
use crate::sites::http::Body;
use crate::sites::manga::text::parse_int_value;
use crate::sites::{Events, Http, Item, Route, Site, remove_html, unescape};
use crate::{Result, error};
use futures_util::{StreamExt, stream};
use serde_json::{Map, Value, json};
use url::Url;

const ROOT: &str = "https://www.pornhub.com";
/// Redirects followed for one listing page.
const MAX_REDIRECTS: usize = 5;

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
    /// `users|model|pornstar/NAME`.
    User(String),
    /// User path and `photos[/...]`.
    Photos(String, String),
    /// User path and `gifs[/...]`.
    Gifs(String, String),
}

/// `(?:[\w-]+\.)?pornhub\.com` on a plain URL.
fn on_site(url: &Url) -> bool {
    on_host(url, &["pornhub.com"]) || subdomain(url, "pornhub.com").is_some()
}

fn target(url: &Url) -> Option<Target> {
    if !on_site(url) {
        return None;
    }
    let path = url.path();
    let id = |prefix: &str| {
        crate::sites::art::leading_digits(path.strip_prefix(prefix)?).map(str::to_owned)
    };
    if let Some(id) = id("/album/") {
        return Some(Target::Gallery(id));
    }
    if let Some(id) = id("/gif/") {
        return Some(Target::Gif(id));
    }
    let parts: Vec<&str> = path[1..].split('/').collect();
    let (kind, name) = match parts.as_slice() {
        [kind @ ("users" | "model" | "pornstar"), name, ..] if segment(name) => (kind, name),
        _ => return None,
    };
    let user = format!("{kind}/{name}");
    match &parts[2..] {
        // `/?$`: nothing may follow, not even a query or fragment.
        [] | [""] if url.query().is_none() && url.fragment().is_none() => Some(Target::User(user)),
        [listing @ ("photos" | "gifs"), rest @ ..] => {
            let sub = rest.first().filter(|s| segment(s));
            let listing = match sub {
                Some(sub) => format!("{listing}/{sub}"),
                None => (*listing).to_owned(),
            };
            Some(if listing.starts_with("photos") {
                Target::Photos(user, listing)
            } else {
                Target::Gifs(user, listing)
            })
        }
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Pornhub URL"));
    };
    if let Ok(root) = http.api(ROOT) {
        http.set_cookie(&root, "accessAgeDisclaimerPH=1; Path=/");
    }
    match target {
        Target::Gallery(id) => crate::sites::once(gallery(http, id)),
        Target::Gif(id) => crate::sites::once(gif(http, id)),
        Target::User(user) => {
            let names = include(http, &["photos", "gifs"], &["photos"]);
            let children: Vec<Url> = names
                .into_iter()
                .filter_map(|name| Url::parse(&format!("{ROOT}/{user}/{name}")).ok())
                .collect();
            stream::iter(children)
                .map(move |child| {
                    crate::sites::queue(http, SITE.category, USER.subcategory, child, &Map::new())
                })
                .flatten()
                .boxed()
        }
        Target::Photos(user, path) => listing(http, &PHOTOS, user, path),
        Target::Gifs(user, path) => listing(http, &GIFS, user, path),
    }
}

async fn gallery<'a>(http: &'a Http, id: String) -> Result<Events<'a>> {
    let page = http.text(&http.api(&format!("{ROOT}/album/{id}"))?).await?;
    let mut scan = Scan::new(&page);
    let title = scan.extr("<title>", "</title>");
    let token = scan.extr("data-token=\"", "\"").to_owned();
    let score = scan.extr("<div id=\"albumGreenBar\" style=\"width:", "\"");
    let views = scan.extr("<div id=\"viewsPhotAlbumCounter\">", "<");
    let tags = scan.extr("<div id=\"photoTagsBox\"", "<script");
    let first = scan.extr("<a href=\"/photo/", "\"").to_owned();
    let (title, user) = title.rsplit_once(" - ").unwrap_or(("", title));
    // `user[:-14]` drops the " Porn Pics..." suffix.
    let cut = user.char_indices().rev().nth(13).map_or(0, |(i, _)| i);
    let user = unescape(&user[..cut]);
    let data = Map::from_iter([
        ("user".to_owned(), Value::from(user.clone())),
        (
            "gallery".to_owned(),
            json!({
                "id": parse_int(&id, 0),
                "title": unescape(title),
                "score": parse_int(score.split('%').next().unwrap_or_default(), 0),
                "views": parse_int(views.split(' ').next().unwrap_or_default(), 0),
                "tags": split_html(tags).into_iter().skip(2).collect::<Vec<_>>(),
            }),
        ),
    ]);

    let mut url = http.api(&format!("{ROOT}/api/v1/album/{id}/show_album_json"))?;
    url.query_pairs_mut().append_pair("token", &token);
    let json = http.json(&url).await?;
    let photos = json
        .get("photos")
        .and_then(Value::as_object)
        .filter(|photos| !photos.is_empty())
        .ok_or_else(|| error("Pornhub refused access to the album"))?;
    let items = ordered(photos, &first)
        .into_iter()
        .enumerate()
        .filter_map(|(index, img)| {
            let url = crate::sites::media::web_url(crate::sites::art::str_of(img, "img_large"))?;
            let mut fields = Map::new();
            fields.insert("url".into(), url.as_str().into());
            fields.insert(
                "caption".into(),
                img.get("caption").cloned().unwrap_or(Value::Null),
            );
            let photo_id = parse_int_value(img.get("id"));
            fields.insert("id".into(), photo_id.into());
            fields.insert(
                "views".into(),
                parse_int_value(img.get("times_viewed")).into(),
            );
            fields.insert(
                "score".into(),
                parse_int_value(img.get("vote_percent")).into(),
            );
            fields.insert("num".into(), (index + 1).into());
            crate::sites::art::update(&mut fields, &data);
            crate::sites::art::nameext_into(url.as_str(), &mut fields);
            Some(
                Item::new(url, fields)
                    .ids(id.clone(), photo_id.to_string())
                    .user(Some(user.clone())),
            )
        })
        .collect();
    Ok(crate::sites::events(crate::sites::post(
        &SITE, http, &GALLERY, items,
    )))
}

/// The photos from `first` along their `next` links until the walk
/// returns to `first`; on a missing link, upstream's dictionary order.
fn ordered<'p>(photos: &'p Map<String, Value>, first: &str) -> Vec<&'p Value> {
    let mut results = Vec::new();
    let mut key = first.to_owned();
    loop {
        let Some(img) = photos.get(&key) else {
            return photos.values().collect();
        };
        results.push(img);
        key = img
            .get("next")
            .map(crate::sites::scalar)
            .unwrap_or_default();
        if key == first || results.len() > photos.len() {
            return results;
        }
    }
}

async fn gif<'a>(http: &'a Http, id: String) -> Result<Events<'a>> {
    let page = http.text(&http.api(&format!("{ROOT}/gif/{id}"))?).await?;
    let mut scan = Scan::new(&page);
    let tags: Vec<&str> = scan.extr("data-context-tag='", "'").split(',').collect();
    let title = scan.extr("\"name\": \"", "\"");
    let raw = scan.extr("\"contentUrl\": \"", "\"");
    let uploaded = parse_iso(scan.extr("\"uploadDate\": \"", "\""));
    let viewkey = scan.extr("From this video: <a href=\"/view_video.php?viewkey=", "\"");
    let timestamp = scan.extr("lass=\"directLink tstamp\" rel=\"nofollow\">", "<");
    let user = remove_html(scan.extr("Created by:", "</div>"));
    let Some(url) = crate::sites::media::web_url(raw) else {
        return Err(error("Pornhub GIF has no file"));
    };
    let mut fields = Map::new();
    fields.insert("id".into(), id.clone().into());
    fields.insert("tags".into(), tags.into());
    fields.insert("title".into(), title.into());
    fields.insert("url".into(), raw.into());
    fields.insert("date".into(), date(uploaded));
    fields.insert("viewkey".into(), viewkey.into());
    fields.insert("timestamp".into(), timestamp.into());
    fields.insert("user".into(), user.clone().into());
    crate::sites::art::nameext_into(raw, &mut fields);
    let item = Item::new(url, fields)
        .ids(id.clone(), id)
        .user(Some(user))
        .caption(Some(title.to_owned()))
        .timestamp(uploaded);
    Ok(crate::sites::events(crate::sites::post(
        &SITE,
        http,
        &GIF,
        vec![item],
    )))
}

/// `PornhubExtractor._pagination`: AJAX pages POSTed with `page=N` until
/// one lists no entries; a redirect renames the user and repeats the page.
fn listing<'a>(http: &'a Http, route: &'static Route, user: String, path: String) -> Events<'a> {
    let path = if path.contains('/') {
        path
    } else {
        format!("{path}/public")
    };
    let (marker, child) = if route.subcategory == PHOTOS.subcategory {
        ("id=\"albumphoto", "album")
    } else {
        ("id=\"gif", "gif")
    };
    let url = format!("{ROOT}/{user}/{path}/ajax");
    let referer = url[..url.len() - 5].to_owned();
    crate::sites::paginate(Some((url, 1u64)), move |state| {
        let path = path.clone();
        let referer = referer.clone();
        async move {
            let Some((mut url, page)) = state else {
                return Ok(None);
            };
            let headers = [
                ("Referer", referer.as_str()),
                ("X-Requested-With", "XMLHttpRequest"),
            ];
            let mut redirects = 0;
            let body = loop {
                let mut endpoint = http.api(&url)?;
                endpoint
                    .query_pairs_mut()
                    .append_pair("page", &page.to_string());
                match http
                    .post_or_redirect(&endpoint, Body::Form(&[]), &headers)
                    .await?
                {
                    Ok(body) => break String::from_utf8_lossy(&body).into_owned(),
                    Err(location) if redirects < MAX_REDIRECTS => {
                        redirects += 1;
                        url = format!("{ROOT}{}/{path}/ajax", location.path());
                    }
                    Err(_) => return Err(error("Pornhub redirected too often")),
                }
            };
            let ids = extract_iter(&body, marker, "\"");
            if ids.is_empty() {
                return Ok(None);
            }
            let children: Vec<Url> = ids
                .into_iter()
                .filter_map(|id| Url::parse(&format!("{ROOT}/{child}/{id}")).ok())
                .filter(|url| target(url).is_some())
                .collect();
            let events = stream::iter(children)
                .map(move |child| {
                    crate::sites::queue(http, SITE.category, route.subcategory, child, &Map::new())
                })
                .flatten()
                .boxed();
            Ok(Some((events, Some((url, page + 1)))))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::manga::tests::{archive, path};
    use crate::sites::testing::{
        html, http, json as reply, keywords, media, queued, response, serve,
    };

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.pornhub.com/album/12345"),
            Some(Target::Gallery("12345".into()))
        );
        assert_eq!(
            t("https://de.pornhub.com/gif/678"),
            Some(Target::Gif("678".into()))
        );
        assert_eq!(
            t("https://pornhub.com/model/someone/"),
            Some(Target::User("model/someone".into()))
        );
        assert_eq!(
            t("https://www.pornhub.com/users/someone/photos/favorites"),
            Some(Target::Photos(
                "users/someone".into(),
                "photos/favorites".into()
            ))
        );
        assert_eq!(
            t("https://www.pornhub.com/pornstar/someone/gifs"),
            Some(Target::Gifs("pornstar/someone".into(), "gifs".into()))
        );
        for raw in [
            "https://www.pornhub.com/model/someone?x=1",
            "https://www.pornhub.com/model/someone/videos",
            "https://www.pornhub.com/album/",
            "https://a.b.pornhub.com/album/1",
            "https://www.pornhub.com.evil.test/album/1",
            "https://www.pornhub.com:8443/album/1",
            "https://u:p@www.pornhub.com/album/1",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[tokio::test]
    async fn extracts_album_in_link_order() {
        let page = r#"<title>Beach Day - someone | Pornhub.com</title>
<div data-token="TOK"></div><div id="albumGreenBar" style="width:87%"></div>
<div id="viewsPhotAlbumCounter">1,234 views</div>
<div id="photoTagsBox"><h3>Tags</h3><a>sand</a><a>sea</a><script>
<a href="/photo/20">"#;
        let photos = serde_json::json!({"photos": {
            "10": {"id": "10", "img_large": "https://ei.phncdn.test/a/10.jpg", "caption": "",
                   "times_viewed": "5", "vote_percent": "90", "next": 20},
            "20": {"id": "20", "img_large": "https://ei.phncdn.test/a/20.jpg", "caption": "First",
                   "times_viewed": "7", "vote_percent": "80", "next": 10},
        }});
        let (root, server) = serve(vec![html(page), reply(photos)]);
        let http = http("pornhub", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.pornhub.com/album/12345").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].url.as_str(), "https://ei.phncdn.test/a/20.jpg");
        let k = keywords(&files[0]);
        assert_eq!(k["user"], "someone");
        assert_eq!(k["id"], 20);
        assert_eq!(k["caption"], "First");
        assert_eq!(k["views"], 7);
        assert_eq!(
            k["gallery"],
            serde_json::json!({"id": 12345, "title": "Beach Day", "score": 87,
                "views": 0, "tags": ["sand", "sea"]})
        );
        assert_eq!(
            path(&files[1]),
            "pornhub/someone/12345 Beach Day/002_10.jpg"
        );
        assert_eq!(archive(&files[1]), r#"["{id}",["10"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /album/12345 "));
        assert!(requests[0].contains("cookie: accessAgeDisclaimerPH=1"));
        assert!(requests[1].starts_with("GET /api/v1/album/12345/show_album_json?token=TOK "));
    }

    #[tokio::test]
    async fn extracts_gif() {
        let page = r#"<div data-context-tag='sea,sun'></div>
<script>{"name": "Waves", "contentUrl": "https://dl.phncdn.test/gif/678.gif", "uploadDate": "2020-01-02T03:04:05+00:00"}</script>
From this video: <a href="/view_video.php?viewkey=abc">v</a>
<a class="directLink tstamp" rel="nofollow">0:42</a>
<div>Created by: <a>someone</a></div>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = http("pornhub", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.pornhub.com/gif/678").unwrap(),
        ))
        .await;
        let k = keywords(&files[0]);
        assert_eq!(k["tags"], serde_json::json!(["sea", "sun"]));
        assert_eq!(k["viewkey"], "abc");
        assert_eq!(k["timestamp"], "0:42");
        assert_eq!(k["user"], "someone");
        assert_eq!(k["date"], "2020-01-02 03:04:05");
        assert_eq!(path(&files[0]), "pornhub/someone/gifs/678 Waves.gif");
        assert_eq!(archive(&files[0]), r#"["{id}",["678"]]"#);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn photo_listing_follows_a_renaming_redirect() {
        let moved = response(
            "301 Moved Permanently\r\nLocation: /model/renamed",
            "text/html",
            b"",
        );
        let (root, server) = serve(vec![
            moved,
            html("<li id=\"albumphoto1\"></li><li id=\"albumphoto2\"></li>"),
            html("<p>none</p>"),
        ]);
        let http = http("pornhub", root);
        let children = queued(extract(
            &http,
            Url::parse("https://www.pornhub.com/users/someone/photos").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].subcategory(), "photos");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("POST /users/someone/photos/public/ajax?page=1 "));
        assert!(requests[0].contains("x-requested-with: XMLHttpRequest"));
        assert!(
            requests[0]
                .contains("referer: https://www.pornhub.com/users/someone/photos/public\r\n")
        );
        assert!(requests[1].starts_with("POST /model/renamed/photos/public/ajax?page=1 "));
        assert!(requests[2].starts_with("POST /model/renamed/photos/public/ajax?page=2 "));
    }

    #[tokio::test]
    async fn user_dispatches_to_photos_by_default() {
        let (root, _server) = serve(vec![]);
        let http = http("pornhub", root);
        let children = queued(extract(
            &http,
            Url::parse("https://www.pornhub.com/model/someone").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].subcategory(), "user");
    }
}
