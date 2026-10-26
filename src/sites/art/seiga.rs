//! Niconico Seiga images and users, mirroring gallery_dl/extractor/seiga.py.
//! Seiga needs a `user_session` cookie or a username and password.
use crate::error;
use crate::sites::gdl::{
    Scan, auth_info, captures, date, extr, extract_iter, group, mime_extension, parse_int,
    partition, re, remove_html, strptime, unescape,
};
use crate::sites::{Events, Http, Item, Route, Site};
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
    name: "Niconico Seiga",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&USER, &IMAGE],
    api_key: false,
    cookies: Some("nicovideo.jp"),
};

#[derive(Debug, PartialEq)]
enum Target {
    User(String, String),
    Image(String),
}

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(
        re!(
            r"^https?://(?:www\.|(?:sp\.)?seiga\.)?nicovideo\.jp/user/illust/(\d+)(?:\?(?:[^&]+&)*sort=([^&#]+))?"
        ),
        url,
    ) {
        return Some(Target::User(group(&c, 1).into(), group(&c, 2).into()));
    }
    captures(
        re!(r"^https?://(?:(?:seiga\.|www\.)?nicovideo\.jp/(?:seiga/im|image/source/)|sp\.seiga\.nicovideo\.jp/seiga/#!/im|lohas\.nicoseiga\.jp/(?:thumb|(?:priv|o)/[^/]+/\d+)/)(\d+)"),
        url,
    )
    .map(|c| Target::Image(group(&c, 1).into()))
}

/// `login`: a `user_session` cookie, else the account login form.
async fn login(http: &Http) -> crate::Result<()> {
    let base = http.api("https://nicovideo.jp/")?;
    if http.cookie(&base, "user_session").is_some() {
        return Ok(());
    }
    let Some((username, password)) = auth_info(http) else {
        return Err(error(
            "username & password or 'user_session' cookie required",
        ));
    };
    let root = "https://account.nicovideo.jp";
    let page = http
        .text(&http.api(&format!("{root}/login?site=seiga"))?)
        .await?;
    let action = http.api(&format!(
        "{root}{}",
        unescape(extr(&page, "<form action=\"", "\""))
    ))?;
    let mut target = http
        .post_form_or_location(
            &action,
            &[
                ("mail_tel", username.as_str()),
                ("password", password.as_str()),
            ],
            &[],
        )
        .await?;
    // Follow the login redirects, as `requests` does after the POST.
    for _ in 0..10 {
        let Err(location) = target else { break };
        if location.as_str().contains("message=cant_login") {
            return Err(error(
                "Niconico login failed; check the username and password",
            ));
        }
        if location.as_str().contains("/mfa") {
            return Err(error(
                "Niconico login needs an email confirmation code; use a 'user_session' cookie instead",
            ));
        }
        target = match http.location(&location, &[]).await? {
            Some(next) => Err(next),
            None => Ok(String::new()),
        };
    }
    if http.cookie(&base, "user_session").is_none() {
        return Err(error(
            "Niconico login failed; check the username and password",
        ));
    }
    Ok(())
}

/// `get_image_url`: the private source URL behind `/image/source/`.
async fn image_url(http: &Http, image_id: &str) -> crate::Result<Url> {
    let location = http
        .location(&http.api(&format!("{ROOT}/image/source/{image_id}"))?, &[])
        .await?
        .ok_or_else(|| error("Seiga image not found"))?;
    if location.as_str().contains("nicovideo.jp/login") {
        return Err(error(format!(
            "HTTP redirect to login page ({})",
            partition(location.as_str(), "?").0
        )));
    }
    Url::parse(&location.as_str().replacen("/o/", "/priv/", 1))
        .map_err(|_| error("invalid Seiga image URL"))
}

/// One image item; the extension comes from the file's Content-Type like
/// upstream's downloader does for `extension = None`.
async fn image_item(http: &Http, fields: Map<String, Value>) -> crate::Result<Item> {
    let image_id = crate::sites::scalar(&fields["image_id"]);
    let url = image_url(http, &image_id).await?;
    let (_, headers) = http.head(&http.api(url.as_str())?, &[]).await?;
    let mut fields = fields;
    let name = url
        .path_segments()
        .and_then(|mut s| s.next_back())
        .unwrap_or_default()
        .to_owned();
    fields.insert("filename".into(), name.into());
    fields.insert(
        "extension".into(),
        mime_extension(headers.get("content-type").and_then(|v| v.to_str().ok())).into(),
    );
    Ok(Item::new(url, fields).ids(image_id.clone(), image_id))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Seiga URL"));
    };
    match target {
        Target::Image(id) => crate::sites::once(async move {
            login(http).await?;
            let seiga = http.api(ROOT)?;
            http.set_cookie(
                &seiga,
                "skip_fetish_warning=1; Domain=seiga.nicovideo.jp; Path=/",
            );
            let page = http
                .text(&http.api(&format!("{ROOT}/seiga/im{id}"))?)
                .await?;
            let mut scan = Scan::new(&page);
            let created = scan
                .extr("<li class=\"date\"><span class=\"created\">", "<")
                .to_owned();
            let title = scan.extr("<h1 class=\"title\">", "</h1>").to_owned();
            let description = scan.extr("<p class=\"discription\">", "</p>").to_owned();
            let mut scan = Scan::new(&page);
            let user_id = scan.extr("<a href=\"/user/illust/", "\"").to_owned();
            let user_name = scan.extr("<span itemprop=\"title\">", "<").to_owned();
            let mut data = Map::new();
            data.insert("title".into(), title.into());
            data.insert("description".into(), remove_html(&description).into());
            data.insert("user".into(), json!({"id": user_id, "name": user_name}));
            data.insert("image_id".into(), parse_int(&id).into());
            data.insert(
                "date".into(),
                date(strptime(
                    &format!("{created}:00+0900"),
                    "%Y年%m月%d日 %H:%M:%S%z",
                )),
            );
            let item = image_item(http, data).await?;
            Ok(crate::sites::events(crate::sites::post(
                &SITE,
                http,
                &IMAGE,
                vec![item],
            )))
        }),
        Target::User(user_id, order) => {
            crate::sites::paginate(Some((1u64, None::<Map<String, Value>>)), move |state| {
                let user_id = user_id.clone();
                let order = order.clone();
                async move {
                    let Some((page_num, meta)) = state else {
                        return Ok(None);
                    };
                    if page_num == 1 {
                        login(http).await?;
                    }
                    let mut url = http.api(&format!("{ROOT}/user/illust/{user_id}"))?;
                    {
                        let mut pairs = url.query_pairs_mut();
                        if !order.is_empty() {
                            pairs.append_pair("sort", &order);
                        }
                        pairs
                            .append_pair("page", &page_num.to_string())
                            .append_pair("target", "illust_all");
                    }
                    let page = http.text(&url).await?;
                    let meta = match meta {
                        Some(meta) => meta,
                        None => {
                            let mut scan = Scan::new(&page);
                            let name = scan.extr("<img alt=\"", "\"").to_owned();
                            let message = scan
                                .extr("<li class=\"user_message\">", "</li>")
                                .trim()
                                .to_owned();
                            scan.extr("<span class=\"target_name\">すべて</span>", "");
                            let count = scan.extr("<span class=\"count \">", "</span>").to_owned();
                            if name.is_empty()
                                && page.contains("ユーザー情報が取得出来ませんでした")
                            {
                                return Err(error("Seiga user not found"));
                            }
                            let mut meta = Map::new();
                            meta.insert("user".into(), json!({"id": parse_int(&user_id), "name": name, "message": message}));
                            meta.insert("count".into(), parse_int(&count).into());
                            meta
                        }
                    };
                    let infos: Vec<Map<String, Value>> =
                        extract_iter(&page, "<li class=\"list_item", "</a></li> ")
                            .into_iter()
                            .map(|info| {
                                let mut scan = Scan::new(info);
                                let mut data = meta.clone();
                                data.insert(
                                    "image_id".into(),
                                    parse_int(scan.extr("/seiga/im", "\"")).into(),
                                );
                                data.insert(
                                    "title".into(),
                                    scan.extr("<li class=\"title\">", "</li>").into(),
                                );
                                for key in ["views", "comments", "clips"] {
                                    data.insert(
                                        key.into(),
                                        parse_int(scan.extr("</span>", "</li>")).into(),
                                    );
                                }
                                data
                            })
                            .collect();
                    let next = (infos.len() >= 40).then_some((page_num + 1, Some(meta)));
                    let events = crate::sites::each(infos, move |data| async move {
                        Ok(crate::sites::post(
                            &SITE,
                            http,
                            &USER,
                            vec![image_item(http, data).await?],
                        ))
                    });
                    Ok(Some((events, next)))
                }
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http as client, keywords, media, response, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://seiga.nicovideo.jp/user/illust/39537793?sort=image_view"),
            Some(Target::User("39537793".into(), "image_view".into()))
        );
        assert_eq!(
            t("https://seiga.nicovideo.jp/seiga/im5977527"),
            Some(Target::Image("5977527".into()))
        );
        assert_eq!(
            t(
                "https://lohas.nicoseiga.jp/priv/759a4ef1c639106ba4d665ee6333832e647d0e4e/1549727594/5977527"
            ),
            Some(Target::Image("5977527".into()))
        );
        assert_eq!(
            t("https://sp.seiga.nicovideo.jp/seiga/#!/im5977527"),
            Some(Target::Image("5977527".into()))
        );
    }

    #[test]
    fn maps_mime_types() {
        assert_eq!(mime_extension(Some("image/png; charset=binary")), "png");
        assert_eq!(mime_extension(None), "jpg");
        assert_eq!(mime_extension(Some("webp")), "webp");
        assert_eq!(mime_extension(Some("text/plain")), "bin");
    }

    #[tokio::test]
    async fn image_resolves_private_source() {
        let page = r#"<li class="date"><span class="created">2016年08月08日 14:20</span><h1 class="title">ひまわり</h1><p class="discription">夏 <br>です</p>
            <a href="/user/illust/39537793"><span itemprop="title">ゆのみ</span>"#;
        let source = response(
            "302 Found\r\nLocation: https://lohas.nicoseiga.jp/o/abc/1549727594/5977527",
            "text/html",
            b"",
        );
        let head = response("200 OK", "image/png", b"");
        let (root, server) = serve(vec![html(page), source, head]);
        let http = client("seiga", root);
        http.set_cookie(
            &http.api("https://nicovideo.jp/").unwrap(),
            "user_session=x; Path=/",
        );
        let files = media(extract(
            &http,
            Url::parse("https://seiga.nicovideo.jp/seiga/im5977527").unwrap(),
        ))
        .await;
        assert_eq!(
            files[0].url.as_str(),
            "https://lohas.nicoseiga.jp/priv/abc/1549727594/5977527"
        );
        let k = keywords(&files[0]);
        assert_eq!(
            (
                k["title"].as_str(),
                k["description"].as_str(),
                k["date"].as_str()
            ),
            (
                Some("ひまわり"),
                Some("夏 です"),
                Some("2016-08-08 05:20:00")
            )
        );
        let (file, _) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "seiga_5977527.png");
        server.join().unwrap();
    }
}
