//! SlickPic albums and users, mirroring gallery_dl/extractor/slickpic.py.
use crate::error;
use crate::sites::gdl::{
    Scan, captures, extract_iter, group, nameext_into, parse_int, re, rpartition, unescape,
};
use crate::sites::{Events, Http, Item, Route, Site};
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

fn target(url: &Url) -> Option<Target> {
    const BASE: &str = r"^https?://([\w-]+)\.slickpic\.com";
    if let Some(c) = captures(re!(&format!(r"{BASE}/albums/([^/?#]+)")), url) {
        return Some(Target::Album(group(&c, 1).into(), group(&c, 2).into()));
    }
    captures(re!(&format!(r"{BASE}(?:/gallery)?/?(?:$|[?#])")), url)
        .map(|c| Target::User(group(&c, 1).into()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported SlickPic URL"));
    };
    crate::sites::once(async move {
        match target {
            Target::Album(user, album) => {
                let root = format!("https://{user}.slickpic.com");
                let page = http
                    .text(&http.api(&format!("{root}/albums/{album}/?wallpaper"))?)
                    .await?;
                let mut scan = Scan::new(&page);
                let full = unescape(scan.extr("<title>", "</title>"));
                let (title, _, owner) = rpartition(&full, " by ");
                let (tk, shd) = (
                    scan.extr("tk = \"", "\"").to_owned(),
                    scan.extr("shd = \"", "\"").to_owned(),
                );
                let (aid, uid) = (
                    scan.extr("data-aid=\"", "\"").to_owned(),
                    scan.extr("data-uid=\"", "\"").to_owned(),
                );
                let tm = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs_f64())
                    .unwrap_or_default()
                    .to_string();
                let form = [
                    ("tm", tm.as_str()),
                    ("tk", tk.as_str()),
                    ("shd", shd.as_str()),
                    ("aid", aid.as_str()),
                    ("uid", uid.as_str()),
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
                let list = http
                    .post_form(
                        &http.api(&format!("{root}/xhr/photo/get/list"))?,
                        &form,
                        &[],
                    )
                    .await?["list"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                let mut data = Map::new();
                data.insert(
                    "album".into(),
                    json!({"id": parse_int(&aid), "title": unescape(title)}),
                );
                data.insert(
                    "user".into(),
                    json!({"id": parse_int(&uid), "name": unescape(owner), "nick": user}),
                );
                data.insert("count".into(), list.len().into());
                let items = list
                    .iter()
                    .enumerate()
                    .filter_map(|(index, img)| {
                        let fname = img["fname"].as_str()?;
                        let url = format!("{}/o/{fname}", crate::sites::scalar(&img["url_rsz"]));
                        let mut fields = Map::new();
                        nameext_into(fname, &mut fields);
                        let id = parse_int(&crate::sites::scalar(&img["id"]));
                        for (key, value) in [
                            ("url", Value::from(url.as_str())),
                            ("num", (index + 1).into()),
                            ("id", id.into()),
                            (
                                "width",
                                parse_int(&crate::sites::scalar(&img["width"])).into(),
                            ),
                            (
                                "height",
                                parse_int(&crate::sites::scalar(&img["height"])).into(),
                            ),
                            ("title", img["title"].clone()),
                            ("description", img["descr"].clone()),
                        ] {
                            fields.insert(key.into(), value);
                        }
                        fields.extend(data.clone());
                        Some(
                            Item::new(Url::parse(&url).ok()?, fields)
                                .ids(aid.clone(), id.to_string()),
                        )
                    })
                    .collect();
                Ok(crate::sites::events(crate::sites::post(
                    &SITE, http, &ALBUM, items,
                )))
            }
            Target::User(user) => {
                let root = format!("https://{user}.slickpic.com");
                let page = http
                    .text(&http.api(&format!("{root}/gallery?viewer"))?)
                    .await?;
                let base = format!("{root}/albums/");
                let children = extract_iter(&page, &format!("href=\"{base}"), "\"")
                    .into_iter()
                    .filter_map(|album| {
                        Some((Url::parse(&format!("{base}{album}")).ok()?, Map::new()))
                    })
                    .collect();
                Ok(crate::sites::galleries::queue(http, &SITE, &USER, children))
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http as client, json as body, keywords, media, serve};

    #[tokio::test]
    async fn extracts_album_list() {
        let page = r#"<title>Trees by Jane</title> tk = "T"; shd = "S"; <div data-aid="11" data-uid="22">"#;
        let list = json!({"list": [{"fname": "a.jpg", "url_rsz": "https://cdn/x", "id": "5", "width": "800", "height": "600", "title": "Oak", "descr": ""}]});
        let (root, server) = serve(vec![html(page), body(list)]);
        let http = client("slickpic", root);
        let files = media(extract(
            &http,
            Url::parse("https://jane.slickpic.com/albums/trees/").unwrap(),
        ))
        .await;
        assert_eq!(files[0].url.as_str(), "https://cdn/x/o/a.jpg");
        assert_eq!(keywords(&files[0])["user"]["name"], "Jane");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "001_5_Oak.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("slickpic/Jane/11 Trees")
        );
        let requests = server.join().unwrap();
        assert!(requests[1].contains("&sys=trees&"));
    }
}
