//! SlickPic albums and users, mirroring gallery_dl/extractor/slickpic.py:
//! an album page holds the tokens for the `xhr/photo/get/list` request; a
//! user's gallery page links the albums.
use crate::sites::art::{Scan, extract_iter, nameext_into, parse_int, segment, str_of, subdomain};
use crate::sites::{Events, Http, Item, Route, Site, unescape};
use crate::{Result, error};
use futures_util::{StreamExt, stream};
use serde_json::{Map, Value, json};
use url::Url;

static ALBUM: Route = Route::new(
    "album",
    "{num:>03}_{id}{title:?_//}.{extension}",
    &["{category}", "{user[name]}", "{album[id]} {album[title]}"],
    "{id}",
);
static USER: Route = Route::new("user", "{filename}.{extension}", &["{category}"], "");

pub static SITE: Site = Site {
    category: "slickpic",
    name: "SlickPic",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&ALBUM, &USER],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Album(String, String),
    User(String),
}

/// `([\w-]+)\.slickpic\.com` with `/albums/([^/?#]+)`, or the user's root
/// or `/gallery` page.
fn target(url: &Url) -> Option<Target> {
    let user = subdomain(url, "slickpic.com")?.to_owned();
    let path = url.path();
    if let Some(rest) = path.strip_prefix("/albums/") {
        let album = rest.split('/').next().filter(|a| segment(a))?;
        return Some(Target::Album(user, album.into()));
    }
    matches!(path, "/" | "/gallery" | "/gallery/").then_some(Target::User(user))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Album(user, album)) => crate::sites::once(async move {
            let root = format!("https://{user}.slickpic.com");
            let page = http
                .text(&http.api(&format!("{root}/albums/{album}/?wallpaper"))?)
                .await?;
            let meta = metadata(&page);
            let tm = format!(
                "{:.6}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs_f64())
                    .unwrap_or_default()
            );
            let form = [
                ("tm", tm.as_str()),
                ("tk", meta["tk"].as_str()),
                ("shd", meta["shd"].as_str()),
                ("aid", meta["aid"].as_str()),
                ("uid", meta["uid"].as_str()),
                ("col", "0"),
                ("sys", album.as_str()),
                ("vw", "1280"),
                ("vh", "1024"),
                ("skey", ""),
                ("viewer", "false"),
                ("pub", "1"),
                ("sng", "0"),
                ("whq", "1"),
            ];
            let data = http
                .post_form(
                    &http.api(&format!("{root}/xhr/photo/get/list"))?,
                    &form,
                    &[],
                )
                .await?;
            let items = parse_album(&meta, &user, &data)?;
            Ok(crate::sites::events(crate::sites::post(
                &SITE, http, &ALBUM, items,
            )))
        }),
        Some(Target::User(user)) => crate::sites::once(async move {
            let root = format!("https://{user}.slickpic.com");
            let page = http
                .text(&http.api(&format!("{root}/gallery?viewer"))?)
                .await?;
            let base = format!("{root}/albums/");
            let children: Vec<Url> = extract_iter(&page, &format!("href=\"{base}"), "\"")
                .into_iter()
                .filter_map(|album| Url::parse(&format!("{base}{album}")).ok())
                .filter(|url| matches!(target(url), Some(Target::Album(..))))
                .collect();
            Ok(stream::iter(children)
                .map(move |url| crate::sites::queue(http, "slickpic", "user", url, &Map::new()))
                .flatten()
                .boxed())
        }),
        None => crate::sites::fail(error("unsupported SlickPic URL")),
    }
}

/// `metadata()`: the album title, owner and request tokens.
pub(crate) fn metadata(page: &str) -> std::collections::BTreeMap<&'static str, String> {
    let mut scan = Scan::new(page);
    let title = unescape(scan.extr("<title>", "</title>"));
    let (title, user) = title
        .rsplit_once(" by ")
        .map_or((String::new(), title.clone()), |(t, u)| {
            (t.to_owned(), u.to_owned())
        });
    let mut data = std::collections::BTreeMap::new();
    data.insert("title", title);
    data.insert("user", user);
    for (key, begin) in [
        ("tk", "tk = \""),
        ("shd", "shd = \""),
        ("aid", "data-aid=\""),
        ("uid", "data-uid=\""),
    ] {
        data.insert(key, scan.extr(begin, "\"").to_owned());
    }
    data
}

/// `items()` over the photo list.
pub(crate) fn parse_album(
    meta: &std::collections::BTreeMap<&'static str, String>,
    nick: &str,
    data: &Value,
) -> Result<Vec<Item>> {
    let Some(images) = data.get("list").and_then(Value::as_array) else {
        return Err(error("SlickPic response has no photo list"));
    };
    let album = json!({
        "id": parse_int(&meta["aid"], 0),
        "title": unescape(&meta["title"]),
    });
    let user = json!({
        "id": parse_int(&meta["uid"], 0),
        "name": unescape(&meta["user"]),
        "nick": nick,
    });
    let int = |img: &Value, key: &str| match img.get(key) {
        Some(Value::Number(n)) => Value::Number(n.clone()),
        Some(value) => parse_int(&crate::sites::scalar(value), 0).into(),
        None => 0.into(),
    };
    let mut items = Vec::new();
    for (index, img) in images.iter().enumerate() {
        let fname = str_of(img, "fname");
        let Some(url) =
            crate::sites::media::web_url(&format!("{}/o/{fname}", str_of(img, "url_rsz")))
        else {
            continue;
        };
        let mut fields = Map::new();
        nameext_into(fname, &mut fields);
        fields.insert("url".into(), url.as_str().into());
        fields.insert("num".into(), (index + 1).into());
        fields.insert("id".into(), int(img, "id"));
        fields.insert("width".into(), int(img, "width"));
        fields.insert("height".into(), int(img, "height"));
        fields.insert(
            "title".into(),
            img.get("title").cloned().unwrap_or(Value::Null),
        );
        fields.insert(
            "description".into(),
            img.get("descr").cloned().unwrap_or(Value::Null),
        );
        fields.insert("album".into(), album.clone());
        fields.insert("user".into(), user.clone());
        fields.insert("count".into(), images.len().into());
        let id = crate::sites::scalar(&fields["id"]);
        items.push(
            Item::new(url, fields)
                .ids(meta["aid"].clone(), id)
                .user(Some(nick.to_owned()))
                .caption(Some(unescape(&meta["title"]))),
        );
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, json, keywords, media, queued, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://mattcrandall.slickpic.com/albums/LamborghiniMurcielago/"),
            Some(Target::Album(
                "mattcrandall".into(),
                "LamborghiniMurcielago".into()
            ))
        );
        assert_eq!(
            t("https://bob.slickpic.com/gallery/"),
            Some(Target::User("bob".into()))
        );
        assert_eq!(
            t("https://bob.slickpic.com"),
            Some(Target::User("bob".into()))
        );
        for raw in [
            "https://slickpic.com/albums/x",
            "https://a.b.slickpic.com/albums/x",
            "https://bob.slickpic.com.evil/albums/x",
            "https://bob.slickpic.com:81/albums/x",
            "https://u@bob.slickpic.com/albums/x",
            "https://bob.slickpic.com/albums/",
            "https://bob.slickpic.com/other",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[tokio::test]
    async fn extracts_album() {
        let page = r#"<title>Cars &amp; more by Bob B</title>
            tk = "TK"; shd = "SH"; <div data-aid="12" data-uid="34">"#;
        let list = serde_json::json!({"list": [
            {"url_rsz": "https://img.slickpic.com/r/a", "fname": "one.jpg", "id": "5",
             "width": "800", "height": "600", "title": "One", "descr": "d"},
            {"url_rsz": "https://img.slickpic.com/r/b", "fname": "two.png", "id": "6",
             "width": "8", "height": "6", "title": "", "descr": ""}]});
        let (root, server) = serve(vec![html(page), json(list)]);
        let http = http("slickpic", root);
        let files = media(extract(
            &http,
            Url::parse("https://bob.slickpic.com/albums/cars/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[0]);
        assert_eq!(k["album"]["title"], "Cars & more");
        assert_eq!(k["user"]["name"], "Bob B");
        assert_eq!(k["user"]["nick"], "bob");
        assert_eq!(k["id"], 5);
        assert_eq!(k["width"], 800);
        assert_eq!(
            files[0].url.as_str(),
            "https://img.slickpic.com/r/a/o/one.jpg"
        );
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "001_5_One.jpg");
        assert_eq!(file.render(&files[1]).unwrap(), "002_6.png");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("slickpic/Bob B/12 Cars & more")
        );
        let (_, identity) = crate::sites::archive_key(&files[1]).unwrap();
        assert_eq!(identity, r#"["{id}",["6"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /albums/cars/?wallpaper "));
        assert!(requests[1].starts_with("POST /xhr/photo/get/list "));
        assert!(requests[1].contains("&tk=TK&shd=SH&aid=12&uid=34&col=0&sys=cars&"));
    }

    #[tokio::test]
    async fn queues_user_albums() {
        let page = r#"<a href="https://bob.slickpic.com/albums/a/">a</a>
            <a href="https://bob.slickpic.com/albums/b/">b</a>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = http("slickpic", root);
        let children = queued(extract(
            &http,
            Url::parse("https://bob.slickpic.com/").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 2);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /gallery?viewer "));
    }
}
