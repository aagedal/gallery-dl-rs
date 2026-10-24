//! ImgBB images, albums and user profiles, mirroring
//! gallery_dl/extractor/imgbb.py: pages embed their first images, later ones
//! come from the site's `json` list endpoint.
//!
//! Upstream's username/password login is not ported; private profiles work
//! with `--cookies` for imgbb.com instead.
use super::{date, iso_ts, path_segment, set_param, str_of};
use crate::sites::art::{Scan, extr, extract_iter, json_loads, parse_int};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://ibb.co";
const FILENAME: &str = "{title} ({id}).{extension}";
const DIRECTORY: &[&str] = &[
    "{category}",
    "{user[name]:?//}{user[id]:? (/)/}",
    "{album[title]} ({album[id]})",
];
const ARCHIVE: &str = "{user[id]} {id}";
static ALBUM: Route = Route::new("album", FILENAME, DIRECTORY, ARCHIVE);
static IMAGE: Route = Route::new("image", FILENAME, DIRECTORY, ARCHIVE);
static USER: Route = Route::new(
    "user",
    FILENAME,
    &["{category}", "{user[name]} ({user[id]})"],
    ARCHIVE,
);

pub static SITE: Site = Site {
    category: "imgbb",
    name: "ImgBB",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&ALBUM, &IMAGE, &USER],
    api_key: false,
    cookies: Some("imgbb.com"),
};

#[derive(Debug, PartialEq)]
enum Target {
    Album { id: String, query: Option<String> },
    Image(String),
    User { user: String, query: Option<String> },
}

fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) {
        return None;
    }
    let query = url.query().filter(|q| !q.is_empty()).map(str::to_owned);
    if url.host_str() == Some("ibb.co") {
        let parts: Vec<&str> = url.path()[1..].split('/').collect();
        return match parts.as_slice() {
            // `album/([^/?#]+)/?(?:\?([^#]+))?`
            ["album", id, rest @ ..] if path_segment(id) => Some(Target::Album {
                id: (*id).into(),
                query: query.filter(|_| matches!(rest, [] | [""])),
            }),
            [id, ..] if path_segment(id) => Some(Target::Image((*id).into())),
            _ => None,
        };
    }
    // `([\w-]+)\.imgbb\.com/?(?:\?([^#]+))?`, unanchored after the host.
    let user = crate::sites::art::subdomain(url, "imgbb.com")?;
    Some(Target::User {
        user: user.into(),
        query: query.filter(|_| url.path() == "/"),
    })
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported ImgBB URL"));
    };
    match target {
        Target::Image(id) => crate::sites::once(async move {
            let page = http.text(&http.api(&format!("{ROOT}/{id}"))?).await?;
            let item = image(&page)?;
            Ok(crate::sites::events(crate::sites::post(
                &SITE,
                http,
                &IMAGE,
                item.into_iter().collect(),
            )))
        }),
        Target::Album { id, query } => crate::sites::once(async move {
            let mut params = super::parse_query(query.as_deref());
            let mut url = http.api(&format!("{ROOT}/album/{id}"))?;
            if !params.is_empty() {
                url.query_pairs_mut().extend_pairs(&params);
            }
            let page = http.text(&url).await?;
            let mut scan = Scan::new(&page);
            let mut album = Map::new();
            album.insert(
                "url".into(),
                scan.extr("property=\"og:url\" content=\"", "\"").into(),
            );
            for (key, begin) in [
                ("title", "property=\"og:title\" content=\""),
                ("description", "property=\"og:description\" content=\""),
            ] {
                let text = crate::sites::unescape(scan.extr(begin, "\""));
                album.insert(key.into(), text.into());
            }
            let album_id = scan
                .extr(
                    "data-text=\"album-name\" href=\"https://ibb.co/album/",
                    "\"",
                )
                .to_owned();
            album.insert("id".into(), album_id.clone().into());
            let count = parse_int(scan.extr("data-text=\"image-count\">", "<"), 0);
            album.insert("count".into(), count.into());
            let mut kwdict = Map::new();
            kwdict.insert("album".into(), album.into());
            set_param(&mut params, "pathname", format!("/album/{album_id}"));
            listing(http, &ALBUM, &page, format!("{ROOT}/json"), params, kwdict)
        }),
        Target::User { user, query } => crate::sites::once(async move {
            let base = format!("https://{user}.imgbb.com/");
            let mut params = super::parse_query(query.as_deref());
            let mut url = http.api(&base)?;
            if !params.is_empty() {
                url.query_pairs_mut().extend_pairs(&params);
            }
            // A login redirect to another origin fails here; upstream reports
            // it as requiring authentication.
            let page = http.text(&url).await?;
            set_param(&mut params, "pathname", "/".into());
            listing(
                http,
                &USER,
                &page,
                format!("{base}json"),
                params,
                Map::new(),
            )
        }),
    }
}

/// `ImgbbImageExtractor.posts`: the image page's Open Graph data.
fn image(page: &str) -> Result<Option<Item>> {
    let mut scan = Scan::new(page);
    let mut image = Map::new();
    let id = scan
        .extr("property=\"og:url\" content=\"https://ibb.co/", "\"")
        .to_owned();
    image.insert("id".into(), id.clone().into());
    let title = crate::sites::unescape(scan.extr("\"og:title\" content=\"", " hosted at ImgBB\""));
    image.insert("title".into(), title.clone().into());
    let raw = scan.extr("\"og:image\" content=\"", "\"").to_owned();
    image.insert("url".into(), raw.clone().into());
    for key in ["width", "height"] {
        let value = parse_int(
            scan.extr(&format!("\"og:image:{key}\" content=\""), "\""),
            0,
        );
        image.insert(key.into(), value.into());
    }
    let album = scan.extr("Added to <a", "</a>");
    let ts = iso_ts(scan.extr("<span title=\"", "\""));
    image.insert("date".into(), date(ts));
    let user = resource_user(scan.extr("CHV.obj.resource=", "};"))?;
    let album = if album.is_empty() {
        Value::Null
    } else {
        let mut data = Map::new();
        data.insert("id".into(), extr(album, "/album/", "\"").into());
        let title = album.rsplit_once('>').map_or(album, |(_, t)| t);
        data.insert("title".into(), crate::sites::unescape(title).into());
        data.into()
    };
    image.insert("album".into(), album);
    let username = user
        .get("username")
        .and_then(Value::as_str)
        .map(str::to_owned);
    image.insert("user".into(), user);
    let Some(url) = crate::sites::media_url(&raw, &["ibb.co"]) else {
        return Ok(None);
    };
    crate::sites::art::nameext_into(&raw, &mut image);
    Ok(Some(
        Item::new(url, image)
            .ids(id.clone(), id)
            .user(username)
            .caption(Some(title))
            .timestamp(ts),
    ))
}

/// `util.json_loads(resource + "}").get("user")`.
fn resource_user(resource: &str) -> Result<Value> {
    let data = json_loads(&format!("{resource}}}"), "ImgBB")?;
    Ok(data.get("user").cloned().unwrap_or(Value::Null))
}

/// `_pagination`: the images embedded in `page`, then the `json` list
/// endpoint's pages from page two, sought by `seekEnd`.
fn listing<'a>(
    http: &'a Http,
    route: &'static Route,
    page: &str,
    endpoint: String,
    mut params: Vec<(String, String)>,
    mut kwdict: Map<String, Value>,
) -> Result<Events<'a>> {
    let mut scan = Scan::new(page);
    let seek = scan
        .find("data-seek=\"", "\"")
        .unwrap_or_default()
        .to_owned();
    let token = scan.extr("PF.obj.config.auth_token=\"", "\"").to_owned();
    let resource = scan
        .find("CHV.obj.resource=", "};")
        .ok_or_else(|| error("ImgBB page has no resource data"))?;
    kwdict.insert("user".into(), resource_user(resource)?);
    let first = images_events(http, route, page, &kwdict);
    if seek.is_empty() || !page.contains("class=\"pagination-next\"") {
        return Ok(first);
    }
    set_param(&mut params, "action", "list".into());
    set_param(&mut params, "page", "2".into());
    set_param(&mut params, "seek", seek);
    set_param(&mut params, "auth_token", token);
    let rest = crate::sites::paginate((Some(params), 2u64), move |(form, page)| {
        let endpoint = endpoint.clone();
        let kwdict = kwdict.clone();
        async move {
            let Some(mut form) = form else {
                return Ok(None);
            };
            let pairs: Vec<(&str, &str)> =
                form.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            let headers = [
                ("Accept", "application/json, text/javascript, */*; q=0.01"),
                ("X-Requested-With", "XMLHttpRequest"),
                ("Origin", ROOT),
                ("Sec-Fetch-Dest", "empty"),
                ("Sec-Fetch-Mode", "cors"),
                ("Sec-Fetch-Site", "same-origin"),
            ];
            let data = http
                .post_form(&http.api(&endpoint)?, &pairs, &headers)
                .await?;
            let events = images_events(http, route, str_of(&data, "html"), &kwdict);
            let seek_end = match data.get("seekEnd") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Number(n)) => n.to_string(),
                _ => String::new(),
            };
            let current = super::query_get(&form, "seek").unwrap_or_default();
            let next = if seek_end.is_empty() || seek_end == current || page >= 100_000 {
                None
            } else {
                set_param(&mut form, "seek", seek_end);
                set_param(&mut form, "page", (page + 1).to_string());
                Some(form)
            };
            Ok(Some((events, (next, page + 1))))
        }
    });
    Ok(futures_util::StreamExt::boxed(
        futures_util::StreamExt::chain(first, rest),
    ))
}

/// gallery-dl `text.parse_int` for a JSON value.
fn int(value: Option<&Value>) -> Value {
    match value {
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .unwrap_or(0)
            .into(),
        Some(Value::String(s)) => parse_int(s, 0).into(),
        _ => 0.into(),
    }
}

/// One post per `data-object` entry of a listing page.
fn images_events<'a>(
    http: &'a Http,
    route: &'static Route,
    page: &str,
    kwdict: &Map<String, Value>,
) -> Events<'a> {
    let mut events = Vec::new();
    for object in extract_iter(page, "data-object='", "'") {
        let Ok(post) = serde_json::from_str::<Value>(&crate::sites::percent_decode(object)) else {
            continue;
        };
        let Some(Value::Object(mut image)) = post.get("image").cloned() else {
            continue;
        };
        let name = image.remove("name").unwrap_or(Value::Null);
        let filename = image.remove("filename").unwrap_or(Value::Null);
        image.insert("filename".into(), name);
        image.insert("name".into(), filename);
        let id = post.get("id_encoded").cloned().unwrap_or(Value::Null);
        image.insert("id".into(), id);
        image.insert(
            "title".into(),
            post.get("title").cloned().unwrap_or(Value::Null),
        );
        image.insert("width".into(), int(post.get("width")));
        image.insert("height".into(), int(post.get("height")));
        let size = int(image.get("size"));
        image.insert("size".into(), size);
        let raw = str_of(&Value::Object(image.clone()), "url").to_owned();
        let Some(url) = crate::sites::media_url(&raw, &["ibb.co"]) else {
            continue;
        };
        crate::sites::art::nameext_into(&raw, &mut image);
        crate::sites::art::update(&mut image, kwdict);
        let id = image
            .get("id")
            .map(crate::sites::scalar)
            .unwrap_or_default();
        let username = kwdict
            .get("user")
            .and_then(|user| user.get("username"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let title = image
            .get("title")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let item = Item::new(url, image)
            .ids(id.clone(), id)
            .user(username)
            .caption(title);
        events.extend(crate::sites::post(&SITE, http, route, vec![item]));
    }
    crate::sites::events(events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, json, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://ibb.co/album/i5PggF/?sort=date_desc"),
            Some(Target::Album {
                id: "i5PggF".into(),
                query: Some("sort=date_desc".into())
            })
        );
        assert_eq!(
            t("https://ibb.co/album/i5PggF/x?sort=date_desc"),
            Some(Target::Album {
                id: "i5PggF".into(),
                query: None
            })
        );
        assert_eq!(
            t("https://ibb.co/fUqh5b"),
            Some(Target::Image("fUqh5b".into()))
        );
        assert_eq!(
            t("https://folkie.imgbb.com/?sort=views_desc"),
            Some(Target::User {
                user: "folkie".into(),
                query: Some("sort=views_desc".into())
            })
        );
        for raw in [
            "https://ibb.co/",
            "https://www.ibb.co/abc",
            "https://ibb.co.evil.test/abc",
            "https://evilibb.co/abc",
            "https://a.b.imgbb.com/",
            "https://imgbb.com/",
            "https://folkie.imgbb.com.evil.test/",
            "https://ibb.co:8080/abc",
            "https://u:p@ibb.co/abc",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn object(id: &str) -> String {
        let post = json!({"id_encoded": id, "title": format!("Title {id}"),
            "width": "640", "height": 480,
            "image": {"filename": format!("{id}.jpg"), "name": "orig name",
                "extension": "jpg", "size": "1234",
                "url": format!("https://i.ibb.co/{id}/orig-name.jpg")}});
        let encoded: String =
            url::form_urlencoded::byte_serialize(post.to_string().as_bytes()).collect();
        let encoded = encoded.replace('+', "%20");
        format!("<div data-object='{encoded}'></div>")
    }

    #[tokio::test]
    async fn album_paginates_list_endpoint() {
        let page = format!(
            "<meta property=\"og:url\" content=\"https://ibb.co/album/AbC\">\
             <meta property=\"og:title\" content=\"My &amp; Album\">\
             <meta property=\"og:description\" content=\"desc\">\
             <a data-text=\"album-name\" href=\"https://ibb.co/album/AbC\">My Album</a>\
             <span data-text=\"image-count\">3</span>\
             <div data-seek=\"seek1\"></div>\
             <script>PF.obj.config.auth_token=\"tok\";\
             CHV.obj.resource={{\"type\":\"album\",\"user\":{{\"id\":\"u1\",\"name\":\"Folk\",\"username\":\"folkie\"}}}};</script>\
             {}<a class=\"pagination-next\"></a>",
            object("one")
        );
        let (root, server) = serve(vec![
            html(&page),
            json(json!({"html": object("two"), "seekEnd": "seek2"})),
            json(json!({"html": object("three"), "seekEnd": "seek2"})),
        ]);
        let http = http("imgbb", root);
        let files = media(extract(
            &http,
            Url::parse("https://ibb.co/album/AbC?sort=date_desc").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].url.as_str(), "https://i.ibb.co/one/orig-name.jpg");
        let k = keywords(&files[0]);
        assert_eq!(k["id"], "one");
        assert_eq!(k["name"], "one.jpg");
        assert_eq!(k["filename"], "orig-name");
        assert_eq!(k["width"], 640);
        assert_eq!(k["size"], 1234);
        assert_eq!(k["album"]["title"], "My & Album");
        assert_eq!(k["album"]["count"], 3);
        assert_eq!(k["user"]["id"], "u1");
        let (file, dir) = crate::sites::default_templates(&files[2]).unwrap();
        assert_eq!(file.render(&files[2]).unwrap(), "Title three (three).jpg");
        assert_eq!(
            dir.render(&files[2]).unwrap().unwrap(),
            std::path::PathBuf::from("imgbb/Folk (u1)/My & Album (AbC)")
        );
        let (_, identity) = crate::sites::archive_key(&files[1]).unwrap();
        assert_eq!(identity, r#"["{user[id]} {id}",["u1","two"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /album/AbC?sort=date_desc "));
        assert!(requests[1].starts_with("POST /json "), "{}", requests[1]);
        assert!(requests[1].ends_with(
            "sort=date_desc&pathname=%2Falbum%2FAbC&action=list&page=2&seek=seek1&auth_token=tok"
        ));
        assert!(requests[1].contains("x-requested-with: XMLHttpRequest"));
        assert!(requests[2].ends_with("&page=3&seek=seek2&auth_token=tok"));
    }

    #[tokio::test]
    async fn user_profile_without_more_pages() {
        let page = format!(
            "<script>CHV.obj.resource={{\"user\":{{\"id\":\"u2\",\"name\":\"Ann\"}}}};</script>{}",
            object("x")
        );
        let (root, server) = serve(vec![html(&page)]);
        let http = http("imgbb", root);
        let files = media(extract(
            &http,
            Url::parse("https://ann.imgbb.com/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(keywords(&files[0])["subcategory"], "user");
        let (_, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("imgbb/Ann (u2)")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET / "));
    }

    #[tokio::test]
    async fn image_page() {
        let page = "<meta property=\"og:url\" content=\"https://ibb.co/fUqh5b\">\
            <meta property=\"og:title\" content=\"Cat &amp; Dog hosted at ImgBB\">\
            <meta property=\"og:image\" content=\"https://i.ibb.co/x/cat.png\">\
            <meta property=\"og:image:width\" content=\"800\">\
            <meta property=\"og:image:height\" content=\"600\">\
            Added to <a href=\"https://ibb.co/album/AbC\">Pets</a>\
            <span title=\"2019-05-20 10:11:12\">\
            <script>CHV.obj.resource={\"user\":{\"id\":\"u3\",\"name\":\"Bo\",\"username\":\"bo\"}};</script>";
        let (root, server) = serve(vec![html(page)]);
        let http = http("imgbb", root);
        let files = media(extract(&http, Url::parse("https://ibb.co/fUqh5b").unwrap())).await;
        assert_eq!(files.len(), 1);
        let k = keywords(&files[0]);
        assert_eq!(k["title"], "Cat & Dog");
        assert_eq!(k["width"], 800);
        assert_eq!(k["date"], "2019-05-20 10:11:12");
        assert_eq!(k["album"], json!({"id": "AbC", "title": "Pets"}));
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "Cat & Dog (fUqh5b).png");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("imgbb/Bo (u3)/Pets (AbC)")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{user[id]} {id}",["u3","fUqh5b"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /fUqh5b "));
    }

    #[tokio::test]
    async fn anonymous_image_without_album() {
        let page = "<meta property=\"og:url\" content=\"https://ibb.co/abc\">\
            <meta property=\"og:title\" content=\"cat hosted at ImgBB\">\
            <meta property=\"og:image\" content=\"https://i.ibb.co/x/cat.png\">\
            <script>CHV.obj.resource={\"type\":\"image\"};</script>";
        let (root, server) = serve(vec![html(page)]);
        let http = http("imgbb", root);
        let files = media(extract(&http, Url::parse("https://ibb.co/abc").unwrap())).await;
        let k = keywords(&files[0]);
        assert_eq!(k["album"], Value::Null);
        assert_eq!(k["user"], Value::Null);
        let (_, dir) = crate::sites::default_templates(&files[0]).unwrap();
        // Upstream formats the missing album as `None`.
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("imgbb/None (None)")
        );
        // The registry gives files without a `user[id]` no archive identity.
        assert!(crate::sites::archive_key(&files[0]).is_none());
        server.join().unwrap();
    }
}
