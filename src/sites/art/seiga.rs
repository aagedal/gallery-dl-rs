//! Nico Nico Seiga images and user illustrations, mirroring
//! gallery_dl/extractor/seiga.py: pages give the metadata, and each
//! image's `/image/source/ID` redirect names the original file.
//!
//! A `user_session` cookie for `nicovideo.jp` from `--cookies` is required;
//! upstream's `username`/`password` login is not supported. Original files
//! carry no extension (upstream takes it from the download's Content-Type),
//! so `jpg` is assumed.
use super::{Scan, digits, extract_iter, leading_digits, parse_int, strptime};
use crate::sites::{Events, Http, Item, Route, Site, remove_html};
use crate::{Result, error};
use futures_util::{StreamExt, stream};
use serde_json::{Map, Value, json};
use url::Url;

const ROOT: &str = "https://seiga.nicovideo.jp";
static USER: Route = Route::new(
    "user",
    "{category}_{user[id]}_{image_id}.{extension}",
    &["{category}", "{user[id]}"],
    "{image_id}",
);
static IMAGE: Route = Route::new(
    "image",
    "{category}_{image_id}.{extension}",
    &["{category}"],
    "{image_id}",
);

pub static SITE: Site = Site {
    category: "seiga",
    name: "Nico Nico Seiga",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&USER, &IMAGE],
    api_key: false,
    cookies: Some("nicovideo.jp"),
};

#[derive(Debug, PartialEq)]
enum Target {
    User(String, Option<String>),
    Image(String),
}

fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) {
        return None;
    }
    let host = url.host_str()?;
    let path = url.path();
    let id = |rest: &str| leading_digits(rest).map(str::to_owned);
    match host {
        "nicovideo.jp" | "www.nicovideo.jp" | "seiga.nicovideo.jp" | "sp.seiga.nicovideo.jp" => {
            if let Some(rest) = path.strip_prefix("/user/illust/") {
                let user = id(rest)?;
                // `(?:\?(?:[^&]+&)*sort=([^&#]+))?` right after the ID.
                let sort = (rest.len() == user.len())
                    .then(|| url.query())
                    .flatten()
                    .and_then(|query| {
                        query
                            .split('&')
                            .find_map(|pair| pair.strip_prefix("sort="))
                            .filter(|sort| !sort.is_empty())
                            .map(str::to_owned)
                    });
                return Some(Target::User(user, sort));
            }
            if host == "sp.seiga.nicovideo.jp" {
                if path != "/seiga/" {
                    return None;
                }
                return id(url.fragment()?.strip_prefix("!/im")?).map(Target::Image);
            }
            let rest = path
                .strip_prefix("/seiga/im")
                .or_else(|| path.strip_prefix("/image/source/"))?;
            id(rest).map(Target::Image)
        }
        "lohas.nicoseiga.jp" => {
            let parts: Vec<&str> = path.split('/').collect();
            let rest = match parts.as_slice() {
                ["", "thumb", rest, ..] => rest,
                ["", "priv" | "o", name, number, rest, ..]
                    if !name.is_empty() && digits(number) =>
                {
                    rest
                }
                _ => return None,
            };
            id(rest).map(Target::Image)
        }
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Seiga URL"));
    };
    crate::sites::once(async move {
        let root = http.api(ROOT)?;
        if http.cookie(&root, "user_session").is_none() {
            return Err(error(
                "Seiga requires a 'user_session' cookie for nicovideo.jp (--cookies)",
            ));
        }
        Ok(match target {
            Target::Image(id) => {
                http.set_cookie(&root, "skip_fetish_warning=1; Path=/");
                let page = http
                    .text(&http.api(&format!("{ROOT}/seiga/im{id}"))?)
                    .await?;
                let data = parse_image_page(&page, &id);
                let item = image(http, data).await?;
                crate::sites::events(crate::sites::post(&SITE, http, &IMAGE, vec![item]))
            }
            Target::User(user, sort) => user_images(http, user, sort),
        })
    })
}

/// `get_image_url`: the `Location` of `/image/source/ID`, with `/o/`
/// replaced by `/priv/`.
async fn image(http: &Http, mut data: Map<String, Value>) -> Result<Item> {
    let id = data
        .get("image_id")
        .map(crate::sites::scalar)
        .unwrap_or_default();
    let source = http.api(&format!("{ROOT}/image/source/{id}"))?;
    let location = http
        .location(&source, &[])
        .await?
        .ok_or_else(|| error("Seiga image has no source redirect"))?;
    if location.as_str().contains("nicovideo.jp/login") {
        return Err(error("Seiga redirected to the login page"));
    }
    let url = Url::parse(&location.as_str().replacen("/o/", "/priv/", 1))
        .map_err(|_| error("invalid Seiga image URL"))?;
    let (name, _) = crate::sites::nameext(&url);
    data.insert("filename".into(), name.into());
    data.insert("extension".into(), "jpg".into());
    let user = data
        .get("user")
        .and_then(|u| u.get("name"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let title = data.get("title").and_then(Value::as_str).map(str::to_owned);
    Ok(Item::new(url, data)
        .ids(id.clone(), id)
        .user(user)
        .caption(title))
}

/// `SeigaImageExtractor.get_images`.
pub(crate) fn parse_image_page(page: &str, id: &str) -> Map<String, Value> {
    let mut scan = Scan::new(page);
    let date = scan.extr("<li class=\"date\"><span class=\"created\">", "<");
    let title = scan.extr("<h1 class=\"title\">", "</h1>");
    let description = scan.extr("<p class=\"discription\">", "</p>");
    let mut user = Scan::new(page);
    let user = json!({
        "id": user.extr("<a href=\"/user/illust/", "\""),
        "name": user.extr("<span itemprop=\"title\">", "<"),
    });
    let ts = strptime(&format!("{date}:00+0900"), "%Y年%m月%d日 %H:%M:%S%z");
    let mut data = Map::new();
    data.insert("date".into(), crate::filter::source_date_json(ts));
    data.insert("title".into(), title.into());
    data.insert("description".into(), remove_html(description).into());
    data.insert("user".into(), user);
    data.insert("image_id".into(), parse_int(id, 0).into());
    data
}

/// `SeigaUserExtractor.get_images`: 40 images per page.
fn user_images(http: &Http, user: String, sort: Option<String>) -> Events<'_> {
    crate::sites::paginate(Some((1u64, None)), move |state| {
        let (user, sort) = (user.clone(), sort.clone());
        async move {
            let Some((page_num, meta)) = state else {
                return Ok(None);
            };
            let mut url = http.api(&format!("{ROOT}/user/illust/{user}"))?;
            {
                let mut query = url.query_pairs_mut();
                if let Some(sort) = &sort {
                    query.append_pair("sort", sort);
                }
                query
                    .append_pair("page", &page_num.to_string())
                    .append_pair("target", "illust_all");
            }
            let page = http.text(&url).await?;
            let meta: Map<String, Value> = match meta {
                Some(meta) => meta,
                None => user_metadata(&page, &user)?,
            };
            let entries = parse_user_page(&page);
            let next = (entries.len() >= 40).then(|| (page_num + 1, Some(meta.clone())));
            let events = stream::iter(entries)
                .then(move |entry| {
                    let mut data = meta.clone();
                    crate::sites::art::update(&mut data, &entry);
                    async move {
                        let item = image(http, data).await?;
                        Ok(crate::sites::events(crate::sites::post(
                            &SITE,
                            http,
                            &USER,
                            vec![item],
                        )))
                    }
                })
                .map(|result: Result<Events<'_>>| result.unwrap_or_else(crate::sites::fail))
                .flatten()
                .boxed();
            Ok(Some((events, next)))
        }
    })
}

/// `get_metadata`: the user and their image count.
pub(crate) fn user_metadata(page: &str, user: &str) -> Result<Map<String, Value>> {
    let mut scan = Scan::new(page);
    let name = scan.extr("<img alt=\"", "\"");
    let msg = scan.extr("<li class=\"user_message\">", "</li>");
    scan.extr("<span class=\"target_name\">すべて</span>", "");
    let count = scan.extr("<span class=\"count \">", "</span>");
    if name.is_empty() && page.contains("ユーザー情報が取得出来ませんでした") {
        return Err(error("Seiga user not found"));
    }
    let mut data = Map::new();
    data.insert(
        "user".into(),
        json!({"id": parse_int(user, 0), "name": name, "message": msg.trim()}),
    );
    data.insert("count".into(), parse_int(count, 0).into());
    Ok(data)
}

/// The image entries of one user page.
pub(crate) fn parse_user_page(page: &str) -> Vec<Map<String, Value>> {
    extract_iter(page, "<li class=\"list_item", "</a></li> ")
        .into_iter()
        .filter_map(|info| {
            let mut scan = Scan::new(info);
            let id = scan.extr("/seiga/im", "\"");
            if !digits(id) {
                return None;
            }
            let mut data = Map::new();
            data.insert("image_id".into(), parse_int(id, 0).into());
            data.insert(
                "title".into(),
                scan.extr("<li class=\"title\">", "</li>").into(),
            );
            for key in ["views", "comments", "clips"] {
                data.insert(
                    key.into(),
                    parse_int(scan.extr("</span>", "</li>"), 0).into(),
                );
            }
            Some(data)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, response, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        let image = |id: &str| Some(Target::Image(id.into()));
        assert_eq!(
            t("https://seiga.nicovideo.jp/seiga/im5977527"),
            image("5977527")
        );
        assert_eq!(t("https://www.nicovideo.jp/image/source/123"), image("123"));
        assert_eq!(
            t("https://sp.seiga.nicovideo.jp/seiga/#!/im123"),
            image("123")
        );
        assert_eq!(t("https://lohas.nicoseiga.jp/thumb/123i"), image("123"));
        assert_eq!(
            t("https://lohas.nicoseiga.jp/priv/abc/999/123"),
            image("123")
        );
        assert_eq!(
            t("https://seiga.nicovideo.jp/user/illust/39537793?target=x&sort=image_view"),
            Some(Target::User("39537793".into(), Some("image_view".into())))
        );
        assert_eq!(
            t("https://sp.seiga.nicovideo.jp/user/illust/1"),
            Some(Target::User("1".into(), None))
        );
        for raw in [
            "https://seiga.nicovideo.jp/seiga/imx",
            "https://sp.seiga.nicovideo.jp/seiga/im1",
            "https://lohas.nicoseiga.jp/priv/abc/x/123",
            "https://evil.nicovideo.jp/seiga/im1",
            "https://seiga.nicovideo.jp.evil/seiga/im1",
            "https://seiga.nicovideo.jp:81/seiga/im1",
            "https://u@seiga.nicovideo.jp/seiga/im1",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn session(root: &Url) -> crate::cookies::CookieJar {
        crate::cookies::CookieJar::parse(&format!(
            "{}\tFALSE\t/\tFALSE\t0\tuser_session\tabc\n",
            root.host_str().unwrap()
        ))
        .unwrap()
    }

    fn redirect(location: &str) -> Vec<u8> {
        response(
            &format!("302 Found\r\nLocation: {location}"),
            "text/html",
            b"",
        )
    }

    #[tokio::test]
    async fn extracts_image() {
        let page = r#"<li class="date"><span class="created">2018年02月03日 04:05</span>
            <h1 class="title">Title</h1><p class="discription">Some <b>text</b></p>
            <a href="/user/illust/77">x</a><span itemprop="title">Artist</span>"#;
        let (root, server) = serve(vec![
            html(page),
            redirect("https://lohas.nicoseiga.jp/o/abc/1/123"),
        ]);
        let http = http("seiga", root.clone()).with_cookies(session(&root));
        let files = media(extract(
            &http,
            Url::parse("https://seiga.nicovideo.jp/seiga/im123").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].url.as_str(),
            "https://lohas.nicoseiga.jp/priv/abc/1/123"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["description"], "Some text");
        assert_eq!(k["user"]["name"], "Artist");
        assert_eq!(k["image_id"], 123);
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "seiga_123.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("seiga")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{image_id}",["123"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /seiga/im123 "));
        assert!(requests[0].contains("skip_fetish_warning=1"));
        assert!(requests[1].starts_with("GET /image/source/123 "));
    }

    #[tokio::test]
    async fn extracts_user_images() {
        let page = r#"<img alt="Bob"><li class="user_message"> hi </li>
            <span class="target_name">すべて</span><span class="count ">2</span>
            <li class="list_item"><a href="/seiga/im5"><li class="title">A</li>
            <span></span>10</li><span></span>2</li><span></span>3</li></a></li> "#;
        let (root, server) = serve(vec![
            html(page),
            redirect("https://lohas.nicoseiga.jp/priv/x/1/5"),
        ]);
        let http = http("seiga", root.clone()).with_cookies(session(&root));
        let files = media(extract(
            &http,
            Url::parse("https://seiga.nicovideo.jp/user/illust/9?sort=image_view").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let k = keywords(&files[0]);
        assert_eq!(k["views"], 10);
        assert_eq!(k["clips"], 3);
        assert_eq!(k["count"], 2);
        assert_eq!(k["user"]["message"], "hi");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "seiga_9_5.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("seiga/9")
        );
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("GET /user/illust/9?sort=image_view&page=1&target=illust_all ")
        );
    }

    #[tokio::test]
    async fn requires_session_cookie() {
        let http = http("seiga", Url::parse("http://127.0.0.1:9/").unwrap());
        assert!(
            crate::sites::testing::fails(extract(
                &http,
                Url::parse("https://seiga.nicovideo.jp/seiga/im1").unwrap()
            ))
            .await
        );
    }
}
