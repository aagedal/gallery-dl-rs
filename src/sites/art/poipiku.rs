//! Poipiku posts and users, mirroring gallery_dl/extractor/poipiku.py.
use crate::error;
use crate::sites::gdl::{
    Scan, captures, extract_iter, group, nameext_into, parse_int, re, rextr, rpartition, unescape,
};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use std::time::Duration;
use url::Url;

const ROOT: &str = "https://poipiku.com";

static USER: Route = Route::new(
    "user",
    "{post_id}_{num}.{extension}",
    &["{category}", "{user_id} {user_name}"],
    "{post_id}_{num}",
);
static POST: Route = Route::new(
    "post",
    "{post_id}_{num}.{extension}",
    &["{category}", "{user_id} {user_name}"],
    "{post_id}_{num}",
);

pub static SITE: Site = Site {
    category: "poipiku",
    name: "Poipiku",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&USER, &POST],
    api_key: false,
    cookies: Some("poipiku.com"),
};

#[derive(Debug, PartialEq)]
enum Target {
    User(String, String),
    Post(String, String),
}

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(
        re!(r"^https?://poipiku\.com/(?:IllustListPcV\.jsp\?PG=(\d+)&ID=)?(\d+)/?(?:$|[?&#])"),
        url,
    ) {
        return Some(Target::User(group(&c, 1).into(), group(&c, 2).into()));
    }
    captures(re!(r"^https?://poipiku\.com/(\d+)/(\d+)"), url)
        .map(|c| Target::Post(group(&c, 1).into(), group(&c, 2).into()))
}

/// Upstream `request_interval = (0.5, 1.5)`.
async fn pace() {
    if !cfg!(test) {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// `_extract_thumb`: the thumbnail, flagging warnings and restrictions.
fn thumb(post: &mut Map<String, Value>, scan: &mut Scan) -> String {
    let mut thumb = String::new();
    loop {
        let img = scan.extr("class=\"IllustItemThumbImg\" src=\"", "\"");
        if img.is_empty() {
            return thumb;
        }
        if img.starts_with("https://cdn.poipiku.com/img/") {
            match rextr(img, "/", ".", None) {
                "warning" => {
                    post.insert("warning".into(), true.into());
                }
                "publish_pass" => {
                    post.insert("password".into(), true.into());
                }
                "publish_login" => {
                    post.insert("requires".into(), "login".into());
                }
                "publish_follower" => {
                    post.insert("requires".into(), "follow".into());
                }
                "publish_t_rt" => {
                    post.insert("requires".into(), "retweet".into());
                }
                _ => {}
            }
        } else if [
            "https://img.poipiku.com/img/",
            "//img.poipiku.com/img/",
            "/img/",
        ]
        .iter()
        .any(|p| img.starts_with(p))
        {
            if img.contains("/warning") {
                post.insert("warning".into(), true.into());
            }
        } else {
            img.clone_into(&mut thumb);
        }
    }
}

async fn api(
    http: &Http,
    endpoint: &str,
    form: &[(&str, &str)],
    referer: &str,
) -> crate::Result<Value> {
    let headers = [
        ("Accept", "application/json, text/javascript, */*; q=0.01"),
        ("X-Requested-With", "XMLHttpRequest"),
        ("Origin", ROOT),
        ("Referer", referer),
        ("Sec-Fetch-Dest", "empty"),
        ("Sec-Fetch-Mode", "cors"),
        ("Sec-Fetch-Site", "same-origin"),
    ];
    http.post_form(&http.api(&format!("{ROOT}{endpoint}"))?, form, &headers)
        .await
}

async fn post_items(http: &Http, path: &str, logged_in: bool) -> crate::Result<Vec<Item>> {
    let password = http
        .option("password")
        .map(crate::sites::scalar)
        .unwrap_or_default();
    let post_url = if path.starts_with('/') {
        format!("{ROOT}{path}")
    } else {
        path.to_owned()
    };
    pace().await;
    let page = http.text(&http.api(&post_url)?).await?;
    let mut scan = Scan::new(&page);
    let mut parts = post_url.rsplitn(3, '/');
    let post_id = parts
        .next()
        .unwrap_or_default()
        .split('.')
        .next()
        .unwrap_or_default()
        .to_owned();
    let user_id = parts.next().unwrap_or_default().to_owned();
    let mut post = Map::new();
    post.insert("post_category".into(), scan.extr("<title>[", "]").into());
    post.insert("count".into(), parse_int(scan.extr("(", " ")).into());
    post.insert("post_id".into(), post_id.as_str().into());
    post.insert("user_id".into(), user_id.as_str().into());
    post.insert(
        "user_name".into(),
        unescape(rpartition(scan.extr("<h2 class=\"UserInfoUserName\">", "</"), ">").2).into(),
    );
    post.insert(
        "description".into(),
        unescape(scan.extr("class=\"IllustItemDesc\" >", "</h1>")).into(),
    );
    post.insert("warning".into(), false.into());
    post.insert("password".into(), false.into());
    post.insert("requires".into(), Value::Null);
    post.insert("original".into(), logged_in.into());
    let thumb = thumb(&mut post, &mut scan);
    if !post["requires"].is_null()
        && post["password"] == false
        && !scan.extr("PasswordIcon", ">").is_empty()
    {
        post.insert("password".into(), true.into());
    }
    let append = || {
        [
            ("UID", user_id.as_str()),
            ("IID", post_id.as_str()),
            ("PAS", password.as_str()),
            ("MD", "0"),
            ("TWF", "-1"),
        ]
    };
    let files: Vec<String> = if logged_in {
        let data = api(
            http,
            "/f/ShowIllustDetailF.jsp",
            &[
                ("ID", &user_id),
                ("TD", &post_id),
                ("AD", "-1"),
                ("PAS", &password),
            ],
            &post_url,
        )
        .await?;
        if data
            .get("error_code")
            .is_some_and(|c| crate::sites::gdl::truthy(Some(c)))
        {
            api(http, "/f/ShowAppendFileF.jsp", &append(), &post_url).await?;
            Vec::new()
        } else {
            extract_iter(data["html"].as_str().unwrap_or_default(), "src=\"", "\"")
                .into_iter()
                .map(str::to_owned)
                .collect()
        }
    } else if !thumb.is_empty() && scan.extr("ShowAppendFile", "<").is_empty() {
        vec![thumb]
    } else {
        let mut files: Vec<String> = if thumb.is_empty() {
            Vec::new()
        } else {
            vec![thumb]
        };
        let data = api(http, "/f/ShowAppendFileF.jsp", &append(), &post_url).await?;
        files.extend(
            extract_iter(
                data["html"].as_str().unwrap_or_default(),
                "class=\"IllustItemThumbImg\" src=\"",
                "\"",
            )
            .into_iter()
            .map(str::to_owned),
        );
        files
    };
    Ok(files
        .into_iter()
        .enumerate()
        .filter_map(|(index, url)| {
            let url = if url.starts_with("//") {
                format!("https:{url}")
            } else {
                url
            };
            let mut fields = post.clone();
            fields.insert("num".into(), (index + 1).into());
            nameext_into(&url, &mut fields);
            Some(
                Item::new(Url::parse(&url).ok()?, fields)
                    .ids(post_id.clone(), (index + 1).to_string())
                    .header("Referer", &post_url),
            )
        })
        .collect())
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Poipiku URL"));
    };
    // Upstream `_init`: English pages and the full contents view. Login is
    // checked once, before listing responses reset `POIPIKU_LK`.
    let mut logged_in = false;
    if let Ok(base) = http.api(ROOT) {
        for cookie in ["LANG=en", "POIPIKU_CONTENTS_VIEW_MODE=1"] {
            http.set_cookie(&base, &format!("{cookie}; Domain=poipiku.com; Path=/"));
        }
        logged_in = http
            .cookie(&base, "POIPIKU_LK")
            .is_some_and(|v| !v.is_empty());
    }
    match target {
        Target::Post(user_id, post_id) => crate::sites::once(async move {
            let items = post_items(http, &format!("/{user_id}/{post_id}.html"), logged_in).await?;
            Ok(crate::sites::events(crate::sites::post(
                &SITE, http, &POST, items,
            )))
        }),
        Target::User(page, user_id) => {
            crate::sites::paginate(Some(parse_int(&page)), move |state| {
                let user_id = user_id.clone();
                async move {
                    let Some(number) = state else { return Ok(None) };
                    let mut url = http.api(&format!("{ROOT}/IllustListPcV.jsp"))?;
                    url.query_pairs_mut()
                        .append_pair("PG", &number.to_string())
                        .append_pair("ID", &user_id)
                        .append_pair("KWD", "");
                    pace().await;
                    let page = http.text(&url).await?;
                    let paths: Vec<String> =
                        extract_iter(&page, "class=\"IllustInfo\" href=\"", "\"")
                            .into_iter()
                            .map(str::to_owned)
                            .collect();
                    let next = (paths.len() >= 48).then_some(number + 1);
                    let events = crate::sites::each(paths, move |path| async move {
                        Ok(crate::sites::post(
                            &SITE,
                            http,
                            &USER,
                            post_items(http, &path, logged_in).await?,
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
    use crate::sites::testing::{html, http as client, json, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://poipiku.com/25049/"),
            Some(Target::User(String::new(), "25049".into()))
        );
        assert_eq!(
            t("https://poipiku.com/IllustListPcV.jsp?PG=1&ID=25049&KWD="),
            Some(Target::User("1".into(), "25049".into()))
        );
        assert_eq!(
            t("https://poipiku.com/25049/5864576.html"),
            Some(Target::Post("25049".into(), "5864576".into()))
        );
    }

    #[tokio::test]
    async fn anonymous_post_appends_files() {
        let page = r#"<title>[Illust] Title(3 images)</title><h2 class="UserInfoUserName"><a>Artist</a></h2><h1 class="IllustItemDesc" >Desc</h1>
            <img class="IllustItemThumbImg" src="https://img-org.poipiku.com/user_img02/000025049/005864576_a.jpeg"><a onclick="ShowAppendFile(1)"><"#;
        let append = json!({"html": "<img class=\"IllustItemThumbImg\" src=\"https://img-org.poipiku.com/b.png\">", "result_num": 1});
        let (root, server) = serve(vec![html(page), json(append)]);
        let http = client("poipiku", root);
        let files = media(extract(
            &http,
            Url::parse("https://poipiku.com/25049/5864576.html").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[1]);
        assert_eq!(
            (
                k["post_category"].as_str(),
                k["count"].as_i64(),
                k["user_name"].as_str()
            ),
            (Some("Illust"), Some(3), Some("Artist"))
        );
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "5864576_2.png");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("poipiku/25049 Artist")
        );
        let requests = server.join().unwrap();
        assert!(
            requests[1].starts_with("POST /f/ShowAppendFileF.jsp ")
                && requests[1].ends_with("UID=25049&IID=5864576&PAS=&MD=0&TWF=-1")
        );
    }
}
