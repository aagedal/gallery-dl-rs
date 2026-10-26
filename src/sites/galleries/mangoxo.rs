//! Mangoxo albums and channels, mirroring gallery_dl/extractor/mangoxo.py.
use crate::error;
use crate::sites::gdl::{
    Scan, auth_info, captures, date, extr, extract_iter, group, nameext_into, parse_int, re,
    rpartition, strptime, unescape,
};
use crate::sites::{Events, Http, Item, Route, Site};
use md5::{Digest, Md5};
use serde_json::{Map, Value, json};
use url::Url;

const ROOT: &str = "https://www.mangoxo.com";

static ALBUM: Route = Route::new(
    "album",
    "{album[id]}_{num:>03}.{extension}",
    &["{category}", "{channel[name]}", "{album[name]}"],
    "{album[id]}_{num}",
);
static CHANNEL: Route = Route::new("channel", "{filename}.{extension}", &["{category}"], "");

pub static SITE: Site = Site {
    category: "mangoxo",
    name: "Mangoxo",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&ALBUM, &CHANNEL],
    api_key: false,
    cookies: Some("www.mangoxo.com"),
};

#[derive(Debug, PartialEq)]
enum Target {
    Album(String),
    Channel(String),
}

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(re!(r"^https?://(?:www\.)?mangoxo\.com/album/(\w+)"), url) {
        return Some(Target::Album(group(&c, 1).into()));
    }
    captures(re!(r"^https?://(?:www\.)?mangoxo\.com/(\w+)/album"), url)
        .map(|c| Target::Channel(group(&c, 1).into()))
}

/// `_sign_by_md5`: login fields with their uppercase MD5 signature.
fn sign(username: &str, password: &str, token: &str, timestamp: &str) -> Vec<(String, String)> {
    let mut params: Vec<(String, String)> = [
        ("username", username),
        ("password", password),
        ("token", token),
        ("timestamp", timestamp),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect();
    let mut sorted = params.clone();
    sorted.sort();
    let mut query = sorted
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    query.push_str("&secretKey=340836904");
    let digest = Md5::digest(query.as_bytes());
    params.push((
        "sign".into(),
        digest.iter().map(|b| format!("{b:02X}")).collect(),
    ));
    params
}

/// `login`: the SESSION cookie from the signed login API.
async fn login(http: &Http) -> crate::Result<()> {
    let Some((username, password)) = auth_info(http) else {
        return Ok(());
    };
    let page = http.text(&http.api(&format!("{ROOT}/login"))?).await?;
    let token = extr(&page, "id=\"loginToken\" value=\"", "\"");
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
        .to_string();
    let params = sign(&username, &password, token, &timestamp);
    let form: Vec<(&str, &str)> = params
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let referer = format!("{ROOT}/login");
    let data = http
        .post_form(
            &http.api(&format!("{ROOT}/api/login"))?,
            &form,
            &[
                ("X-Requested-With", "XMLHttpRequest"),
                ("Referer", referer.as_str()),
            ],
        )
        .await?;
    if crate::sites::scalar(&data["result"]) != "1" {
        return Err(error(format!(
            "Mangoxo login failed: {}",
            crate::sites::scalar(&data["msg"])
        )));
    }
    Ok(())
}

fn total_pages(page: &str) -> i64 {
    parse_int(extr(page, "total :", ","))
}

fn metadata(page: &str, album_id: &str) -> Map<String, Value> {
    let mut scan = Scan::new(page);
    let title = scan.extr("<img id=\"cover-img\" alt=\"", "\"").to_owned();
    let cid = scan
        .extr("href=\"https://www.mangoxo.com/user/", "\"")
        .to_owned();
    let cname = scan.extr("<img alt=\"", "\"").to_owned();
    let cover = scan.extr(" src=\"", "\"").to_owned();
    let count = scan.extr("id=\"pic-count\">", "<").to_owned();
    let created = scan
        .extr("class=\"fa fa-calendar\"></i>", "<")
        .trim()
        .to_owned();
    let description = scan.extr("<pre>", "</pre>").to_owned();
    let mut data = Map::new();
    data.insert(
        "channel".into(),
        json!({"id": cid, "name": unescape(&cname), "cover": cover}),
    );
    data.insert("album".into(), json!({"id": album_id, "name": unescape(&title), "date": date(strptime(&created, "%Y.%m.%d %H:%M")), "description": unescape(&description)}));
    data.insert("count".into(), parse_int(&count).into());
    data
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Mangoxo URL"));
    };
    match target {
        Target::Album(album_id) => crate::sites::once(async move {
            login(http).await?;
            let base = format!("{ROOT}/album/{album_id}/");
            let mut page = http.text(&http.api(&base)?).await?;
            let data = metadata(&page, &album_id);
            let total = total_pages(&page);
            let mut paths: Vec<String> = Vec::new();
            let mut number = 1;
            loop {
                paths.extend(
                    extract_iter(&page, "class=\"lightgallery-item\" href=\"", "\"")
                        .into_iter()
                        .map(str::to_owned),
                );
                if number >= total {
                    break;
                }
                number += 1;
                page = http.text(&http.api(&format!("{base}{number}"))?).await?;
            }
            let items = paths
                .iter()
                .enumerate()
                .filter_map(|(index, path)| {
                    let mut fields = data.clone();
                    fields.insert("num".into(), (index + 1).into());
                    fields.insert("id".into(), parse_int(extr(path, "=", "&")).into());
                    let url = format!("{ROOT}/external/{}", rpartition(path, "url=").2);
                    nameext_into(&url, &mut fields);
                    Some(
                        Item::new(Url::parse(&url).ok()?, fields)
                            .ids(album_id.clone(), (index + 1).to_string()),
                    )
                })
                .collect();
            Ok(crate::sites::events(crate::sites::post(
                &SITE, http, &ALBUM, items,
            )))
        }),
        Target::Channel(user) => crate::sites::paginate(Some((1i64, 1i64)), move |state| {
            let user = user.clone();
            async move {
                let Some((number, total)) = state else {
                    return Ok(None);
                };
                if number == 1 {
                    login(http).await?;
                }
                let page = http
                    .text(&http.api(&format!("{ROOT}/{user}/album/{number}"))?)
                    .await?;
                let children = extract_iter(&page, "<a class=\"link black\" href=\"", "\"")
                    .into_iter()
                    .filter_map(|album| Some((Url::parse(album).ok()?, Map::new())))
                    .collect();
                let total = if number == 1 {
                    total_pages(&page)
                } else {
                    total
                };
                let next = (number < total).then_some((number + 1, total));
                Ok(Some((super::queue(http, &SITE, &CHANNEL, children), next)))
            }
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http as client, keywords, media, serve};

    #[test]
    fn signs_like_upstream() {
        let params = sign("user", "pass", "tok", "1700000000");
        // Python reference: hashlib.md5(sorted query + secretKey).upper().
        assert_eq!(params.last().unwrap().1, "D20E10616D45DE7381EF75226C8AA4AB");
        assert_eq!(
            params.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
            ["username", "password", "token", "timestamp", "sign"]
        );
    }

    #[tokio::test]
    async fn album_pages_external_images() {
        let first = r#"<img id="cover-img" alt="Album &amp; Co"><a href="https://www.mangoxo.com/user/ch1"><img alt="Channel" src="https://c/cover.jpg"><span id="pic-count">2</span>
            <i class="fa fa-calendar"></i> 2019.05.06 07:08<pre>Desc</pre> total :2, <a class="lightgallery-item" href="/image?id=11&url=img/a.jpg">"#;
        let second = r#"<a class="lightgallery-item" href="/image?id=12&url=img/b.png">"#;
        let (root, server) = serve(vec![html(first), html(second)]);
        let http = client("mangoxo", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.mangoxo.com/album/lzVOv1Q9").unwrap(),
        ))
        .await;
        assert_eq!(
            files[1].url.as_str(),
            "https://www.mangoxo.com/external/img/b.png"
        );
        let k = keywords(&files[1]);
        assert_eq!(
            (
                k["id"].as_i64(),
                k["album"]["name"].as_str(),
                k["album"]["date"].as_str()
            ),
            (Some(12), Some("Album & Co"), Some("2019-05-06 07:08:00"))
        );
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "lzVOv1Q9_002.png");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("mangoxo/Channel/Album & Co")
        );
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /album/lzVOv1Q9/2 "));
    }
}
