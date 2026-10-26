//! ImgBB images, albums and user profiles, mirroring
//! gallery_dl/extractor/imgbb.py. Logs in with `username`/`password` unless
//! `PHPSESSID`/`LID` session cookies are loaded.
use crate::error;
use crate::sites::gdl::{
    Scan, auth_info, captures, date, extr, extract_iter, group, int_of, nameext_into, parse_int,
    parse_iso, parse_query, re, rpartition, unescape, unquote,
};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://ibb.co";
const FILENAME: &str = "{title} ({id}).{extension}";
const DIRECTORY: &[&str] = &[
    "{category}",
    "{user[name]:?//}{user[id]:? (/)/}",
    "{album[title]} ({album[id]})",
];

static ALBUM: Route = Route::new("album", FILENAME, DIRECTORY, "{user[id]} {id}");
static IMAGE: Route = Route::new("image", FILENAME, DIRECTORY, "{user[id]} {id}");
static USER: Route = Route::new(
    "user",
    FILENAME,
    &["{category}", "{user[name]} ({user[id]})"],
    "{user[id]} {id}",
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
    Album(String, String),
    Image(String),
    User(String, String),
}

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(
        re!(r"^https?://ibb\.co/album/([^/?#]+)/?(?:\?([^#]+))?"),
        url,
    ) {
        return Some(Target::Album(group(&c, 1).into(), group(&c, 2).into()));
    }
    if let Some(c) = captures(re!(r"^https?://ibb\.co/([^/?#]+)"), url) {
        return Some(Target::Image(group(&c, 1).into()));
    }
    let c = captures(re!(r"^https?://([\w-]+)\.imgbb\.com/?(?:\?([^#]+))?"), url)?;
    Some(Target::User(group(&c, 1).into(), group(&c, 2).into()))
}

/// `ImgbbExtractor.login`.
async fn login(http: &Http) -> crate::Result<()> {
    let root = http.api("https://imgbb.com/")?;
    if http.cookie(&root, "PHPSESSID").is_some() && http.cookie(&root, "LID").is_some() {
        return Ok(());
    }
    let Some((username, password)) = auth_info(http) else {
        return Ok(());
    };
    let url = http.api("https://imgbb.com/login")?;
    let page = http.text(&url).await?;
    let token = extr(&page, "name=\"auth_token\" value=\"", "\"").to_owned();
    let form = [
        ("auth_token", token.as_str()),
        ("login-subject", username.as_str()),
        ("password", password.as_str()),
    ];
    // A successful login redirects; a failed one renders the form again.
    match http
        .post_form_text(&url, &form, &[("Referer", "https://imgbb.com/login")])
        .await
    {
        Err(e) if e.to_string().contains("redirected a POST") => Ok(()),
        _ => Err(error("ImgBB login failed; check the username and password")),
    }
}

fn user_of(page: &str) -> Value {
    let resource = format!("{}}}", Scan::new(page).extr("CHV.obj.resource=", "};"));
    serde_json::from_str::<Value>(&resource)
        .ok()
        .and_then(|r| r.get("user").cloned())
        .unwrap_or(Value::Null)
}

/// `_pagination`: images of the page, then the `/json` list endpoint.
async fn images(
    http: &Http,
    page: String,
    url: &str,
    mut params: Vec<(String, String)>,
) -> crate::Result<(Value, Vec<Map<String, Value>>)> {
    let mut scan = Scan::new(&page);
    let seek = scan.extr("data-seek=\"", "\"").to_owned();
    let token = scan.extr("PF.obj.config.auth_token=\"", "\"").to_owned();
    let user = user_of(&page);
    let mut page = page;
    let mut data: Option<Value> = None;
    let mut result = Vec::new();
    loop {
        for object in extract_iter(&page, "data-object='", "'") {
            let Ok(post) = serde_json::from_str::<Value>(&unquote(object)) else {
                continue;
            };
            let mut image = post["image"].as_object().cloned().unwrap_or_default();
            let filename = image.remove("filename").unwrap_or(Value::Null);
            let name = image.remove("name").unwrap_or(Value::Null);
            image.insert("filename".into(), name);
            image.insert("name".into(), filename);
            image.insert("id".into(), post["id_encoded"].clone());
            image.insert("title".into(), post["title"].clone());
            image.insert("width".into(), int_of(post.get("width")).into());
            image.insert("height".into(), int_of(post.get("height")).into());
            image.insert("size".into(), int_of(image.get("size")).into());
            result.push(image);
        }
        let set = |params: &mut Vec<(String, String)>, key: &str, value: String| match params
            .iter_mut()
            .find(|(k, _)| k == key)
        {
            Some(slot) => slot.1 = value,
            None => params.push((key.into(), value)),
        };
        match &data {
            Some(data) => {
                let end = data["seekEnd"].as_str().unwrap_or_default().to_owned();
                let current = params
                    .iter()
                    .find(|(k, _)| k == "seek")
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default();
                if end.is_empty() || current == end {
                    break;
                }
                set(&mut params, "seek", end);
                let page_number = params
                    .iter()
                    .find(|(k, _)| k == "page")
                    .map_or(1, |(_, v)| parse_int(v))
                    + 1;
                set(&mut params, "page", page_number.to_string());
            }
            None if seek.is_empty() || !page.contains("class=\"pagination-next\"") => break,
            None => {
                set(&mut params, "action", "list".into());
                set(&mut params, "page", "2".into());
                set(&mut params, "seek", seek.clone());
                set(&mut params, "auth_token", token.clone());
            }
        }
        let form: Vec<(&str, &str)> = params
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let response = http
            .post_form(
                &http.api(url)?,
                &form,
                &[
                    ("Accept", "application/json, text/javascript, */*; q=0.01"),
                    ("X-Requested-With", "XMLHttpRequest"),
                    ("Origin", ROOT),
                    ("Sec-Fetch-Dest", "empty"),
                    ("Sec-Fetch-Mode", "cors"),
                    ("Sec-Fetch-Site", "same-origin"),
                ],
            )
            .await?;
        page = crate::sites::gdl::str_of(&response, "html").to_owned();
        data = Some(response);
    }
    Ok((user, result))
}

fn items(images: Vec<Map<String, Value>>, extra: &Map<String, Value>) -> Vec<Item> {
    images
        .into_iter()
        .filter_map(|mut image| {
            image.extend(extra.clone());
            let raw = image.get("url").map(crate::sites::scalar)?;
            nameext_into(&raw, &mut image);
            let id = image
                .get("id")
                .map(crate::sites::scalar)
                .unwrap_or_default();
            Some(Item::new(Url::parse(&raw).ok()?, image).ids(id.clone(), id))
        })
        .collect()
}

fn post_each<'a>(http: &'a Http, route: &'static Route, items: Vec<Item>) -> Events<'a> {
    let events = items
        .into_iter()
        .flat_map(|item| crate::sites::post(&SITE, http, route, vec![item]))
        .collect();
    crate::sites::events(events)
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported ImgBB URL"));
    };
    crate::sites::once(async move {
        login(http).await?;
        Ok(match target {
            Target::Image(id) => {
                let page = http.text(&http.api(&format!("{ROOT}/{id}"))?).await?;
                let mut scan = Scan::new(&page);
                let mut image = Map::new();
                image.insert(
                    "id".into(),
                    scan.extr("property=\"og:url\" content=\"https://ibb.co/", "\"")
                        .into(),
                );
                image.insert(
                    "title".into(),
                    unescape(scan.extr("\"og:title\" content=\"", " hosted at ImgBB\"")).into(),
                );
                image.insert(
                    "url".into(),
                    scan.extr("\"og:image\" content=\"", "\"").into(),
                );
                image.insert(
                    "width".into(),
                    parse_int(scan.extr("\"og:image:width\" content=\"", "\"")).into(),
                );
                image.insert(
                    "height".into(),
                    parse_int(scan.extr("\"og:image:height\" content=\"", "\"")).into(),
                );
                let album = scan.extr("Added to <a", "</a>").to_owned();
                image.insert(
                    "date".into(),
                    date(parse_iso(scan.extr("<span title=\"", "\""))),
                );
                image.insert("user".into(), user_of(&page[scan.pos()..]));
                let album = if album.is_empty() {
                    Value::Null
                } else {
                    serde_json::json!({"id": extr(&album, "/album/", "\""), "title": unescape(rpartition(&album, ">").2)})
                };
                image.insert("album".into(), album);
                post_each(http, &IMAGE, items(vec![image], &Map::new()))
            }
            Target::Album(id, query) => {
                let params: Vec<(String, String)> = parse_query(&query)
                    .into_iter()
                    .map(|(k, v)| (k, crate::sites::scalar(&v)))
                    .collect();
                let mut url = http.api(&format!("{ROOT}/album/{id}"))?;
                if !params.is_empty() {
                    url.query_pairs_mut().extend_pairs(params.iter());
                }
                let page = http.text(&url).await?;
                let mut scan = Scan::new(&page);
                let mut album = Map::new();
                album.insert(
                    "url".into(),
                    scan.extr("property=\"og:url\" content=\"", "\"").into(),
                );
                album.insert(
                    "title".into(),
                    unescape(scan.extr("property=\"og:title\" content=\"", "\"")).into(),
                );
                album.insert(
                    "description".into(),
                    unescape(scan.extr("property=\"og:description\" content=\"", "\"")).into(),
                );
                let album_id = scan
                    .extr(
                        "data-text=\"album-name\" href=\"https://ibb.co/album/",
                        "\"",
                    )
                    .to_owned();
                album.insert("id".into(), album_id.as_str().into());
                album.insert(
                    "count".into(),
                    parse_int(scan.extr("data-text=\"image-count\">", "<")).into(),
                );
                let mut params = params;
                params.push(("pathname".into(), format!("/album/{album_id}")));
                let (user, list) =
                    images(http, page.clone(), &format!("{ROOT}/json"), params).await?;
                let mut extra = Map::new();
                extra.insert("album".into(), Value::Object(album));
                extra.insert("user".into(), user);
                post_each(http, &ALBUM, items(list, &extra))
            }
            Target::User(user, query) => {
                let base = format!("https://{user}.imgbb.com/");
                let params: Vec<(String, String)> = parse_query(&query)
                    .into_iter()
                    .map(|(k, v)| (k, crate::sites::scalar(&v)))
                    .collect();
                let mut url = http.api(&base)?;
                if !params.is_empty() {
                    url.query_pairs_mut().extend_pairs(params.iter());
                }
                if let Some(location) = http.location(&url, &[]).await? {
                    return Err(error(format!(
                        "ImgBB user profile redirects ({}); it may require authentication",
                        location.path()
                    )));
                }
                let page = http.text(&url).await?;
                let mut params = params;
                params.push(("pathname".into(), "/".into()));
                let (user, list) = images(http, page, &format!("{base}json"), params).await?;
                let mut extra = Map::new();
                extra.insert("user".into(), user);
                post_each(http, &USER, items(list, &extra))
            }
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http as client, json, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://ibb.co/album/cgJrwc?sort=name_asc"),
            Some(Target::Album("cgJrwc".into(), "sort=name_asc".into()))
        );
        assert_eq!(
            t("https://ibb.co/fUqh5b"),
            Some(Target::Image("fUqh5b".into()))
        );
        assert_eq!(
            t("https://folkie.imgbb.com"),
            Some(Target::User("folkie".into(), String::new()))
        );
    }

    fn object(id: &str) -> String {
        let post = json!({"id_encoded": id, "title": "Pic", "width": "800", "height": 600,
            "image": {"url": format!("https://i.ibb.co/x/{id}.jpg"), "name": id, "filename": format!("{id}.jpg"), "size": "1234"}});
        crate::sites::gdl::quote(&post.to_string())
    }

    #[tokio::test]
    async fn pages_album_through_json() {
        let page = format!(
            r#"<meta property="og:url" content="https://ibb.co/album/cg"><meta property="og:title" content="Album">
            <meta property="og:description" content="D"><a data-text="album-name" href="https://ibb.co/album/cg">x</a><span data-text="image-count">2</span>
            <div data-seek="S1"></div>PF.obj.config.auth_token="TOK";CHV.obj.resource={{"user":{{"id":"u1","name":"Folk"}}}};
            <div data-object='{}'></div><a class="pagination-next">"#,
            object("a1")
        );
        let (root, server) = serve(vec![
            html(&page),
            json(
                json!({"html": format!("<div data-object='{}'></div>", object("a2")), "seekEnd": ""}),
            ),
        ]);
        let http = client("imgbb", root);
        let files = media(extract(
            &http,
            Url::parse("https://ibb.co/album/cg").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[1]);
        assert_eq!(k["id"], "a2");
        assert_eq!(k["user"]["name"], "Folk");
        assert_eq!(k["album"]["title"], "Album");
        assert_eq!(k["size"], 1234);
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "Pic (a2).jpg");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("imgbb/Folk (u1)/Album (cg)")
        );
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("POST /json "));
        assert!(
            requests[1]
                .ends_with("pathname=%2Falbum%2Fcg&action=list&page=2&seek=S1&auth_token=TOK")
        );
    }
}
