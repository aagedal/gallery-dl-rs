//! Poipiku users and posts, mirroring gallery_dl/extractor/poipiku.py: a
//! post page names its thumbnail, and the `ShowAppendFileF.jsp` (anonymous)
//! or `ShowIllustDetailF.jsp` (with a `POIPIKU_LK` session cookie) endpoints
//! list the remaining files.
use super::{Scan, extract_iter, parse_int, truthy};
use crate::sites::{Events, Http, Item, Route, Site, unescape};
use crate::{Result, error};
use futures_util::{StreamExt, stream};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://poipiku.com";

pub static SITE: Site = Site {
    category: "poipiku",
    name: "Poipiku",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&USER, &POST],
    api_key: false,
    cookies: Some("poipiku.com"),
};

const FILENAME: &str = "{post_id}_{num}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{user_id} {user_name}"];

static USER: Route = Route::new("user", FILENAME, DIRECTORY, "{post_id}_{num}");
static POST: Route = Route::new("post", FILENAME, DIRECTORY, "{post_id}_{num}");

#[derive(Debug, PartialEq)]
enum Target {
    /// User ID and first page number.
    User(String, i64),
    Post(String, String),
}

fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) || url.host_str() != Some("poipiku.com") {
        return None;
    }
    let path = url.path();
    if path == "/IllustListPcV.jsp" {
        // `IllustListPcV\.jsp\?PG=(\d+)&ID=(\d+)/?(?:$|[?&#])`
        let query = url.query()?.strip_prefix("PG=")?;
        let page = super::leading_digits(query)?;
        let rest = query[page.len()..].strip_prefix("&ID=")?;
        let user = super::leading_digits(rest)?;
        let tail = &rest[user.len()..];
        let tail = tail.strip_prefix('/').unwrap_or(tail);
        return (tail.is_empty() || tail.starts_with(['?', '&']))
            .then(|| Target::User(user.to_owned(), parse_int(page, 0)));
    }
    let rest = path.strip_prefix('/')?;
    let user = super::leading_digits(rest)?;
    let rest = &rest[user.len()..];
    if matches!(rest, "" | "/") {
        return Some(Target::User(user.to_owned(), 0));
    }
    let post = super::leading_digits(rest.strip_prefix('/')?)?;
    Some(Target::Post(user.to_owned(), post.to_owned()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Poipiku URL"));
    };
    if let Ok(root) = http.api(ROOT) {
        http.set_cookie(&root, "LANG=en; Path=/");
        http.set_cookie(&root, "POIPIKU_CONTENTS_VIEW_MODE=1; Path=/");
    }
    match target {
        Target::Post(user, post) => post_events(http, &POST, format!("/{user}/{post}.html")),
        Target::User(user, page) => listing(http, user, page),
    }
}

/// Upstream `PoipikuUserExtractor.posts`: 48 post links per page.
fn listing(http: &Http, user: String, first: i64) -> Events<'_> {
    crate::sites::paginate(Some(first), move |page| {
        let user = user.clone();
        async move {
            let Some(page) = page else {
                return Ok(None);
            };
            let mut url = http.api(&format!("{ROOT}/IllustListPcV.jsp"))?;
            url.query_pairs_mut()
                .append_pair("PG", &page.to_string())
                .append_pair("ID", &user)
                .append_pair("KWD", "");
            let text = http.text(&url).await?;
            let paths: Vec<String> = extract_iter(&text, "class=\"IllustInfo\" href=\"", "\"")
                .into_iter()
                .map(str::to_owned)
                .collect();
            let next = (paths.len() >= 48).then_some(page + 1);
            let events = stream::iter(paths)
                .map(move |path| post_events(http, &USER, path))
                .flatten()
                .boxed();
            Ok(Some((events, next)))
        }
    })
}

/// The AJAX request headers, with the post page as `Referer`.
fn headers(referer: &str) -> [(&str, &str); 7] {
    [
        ("Accept", "application/json, text/javascript, */*; q=0.01"),
        ("X-Requested-With", "XMLHttpRequest"),
        ("Origin", ROOT),
        ("Referer", referer),
        ("Sec-Fetch-Dest", "empty"),
        ("Sec-Fetch-Mode", "cors"),
        ("Sec-Fetch-Site", "same-origin"),
    ]
}

fn post_events<'a>(http: &'a Http, route: &'static Route, path: String) -> Events<'a> {
    crate::sites::once(async move {
        let items = post(http, &path).await?;
        Ok(crate::sites::events(crate::sites::post(
            &SITE, http, route, items,
        )))
    })
}

/// Upstream `items` for one post page.
async fn post(http: &Http, path: &str) -> Result<Vec<Item>> {
    let post_url = if path.starts_with('/') {
        format!("{ROOT}{path}")
    } else {
        path.to_owned()
    };
    let parsed = Url::parse(&post_url).map_err(|_| error("invalid Poipiku post URL"))?;
    if !matches!(target(&parsed), Some(Target::Post(..))) {
        return Err(error("Poipiku post link left poipiku.com"));
    }
    let page = http.text(&http.api(&post_url)?).await?;
    let root = http.api(ROOT)?;
    let logged_in = http.cookie(&root, "POIPIKU_LK").is_some();
    let mut scan = Scan::new(&page);
    let (mut data, thumb) = parse_page(&mut scan, &post_url, logged_in);
    let password = http.option_str("password").unwrap_or_default();
    let user_id = data["user_id"].as_str().unwrap_or_default().to_owned();
    let post_id = data["post_id"].as_str().unwrap_or_default().to_owned();
    let headers = headers(&post_url);
    let append_file = || async {
        let url = http.api(&format!("{ROOT}/f/ShowAppendFileF.jsp"))?;
        let form = [
            ("UID", user_id.as_str()),
            ("IID", post_id.as_str()),
            ("PAS", password),
            ("MD", "0"),
            ("TWF", "-1"),
        ];
        http.post_form(&url, &form, &headers).await
    };
    let files: Vec<String> = if logged_in {
        let url = http.api(&format!("{ROOT}/f/ShowIllustDetailF.jsp"))?;
        let form = [
            ("ID", user_id.as_str()),
            ("TD", post_id.as_str()),
            ("AD", "-1"),
            ("PAS", password),
        ];
        let detail = http.post_form(&url, &form, &headers).await?;
        if truthy(detail.get("error_code")) {
            // Upstream logs the append endpoint's explanation.
            append_file().await?;
            Vec::new()
        } else {
            let html = detail
                .get("html")
                .and_then(Value::as_str)
                .unwrap_or_default();
            extract_iter(html, "src=\"", "\"")
                .into_iter()
                .map(str::to_owned)
                .collect()
        }
    } else {
        let mut files = Vec::new();
        let single = !thumb.is_empty() && scan.extr("ShowAppendFile", "<").is_empty();
        if !thumb.is_empty() {
            files.push(thumb);
        }
        if !single {
            let append = append_file().await?;
            let html = append
                .get("html")
                .and_then(Value::as_str)
                .unwrap_or_default();
            files.extend(
                extract_iter(html, "class=\"IllustItemThumbImg\" src=\"", "\"")
                    .into_iter()
                    .map(str::to_owned),
            );
        }
        files
    };
    let user_name = data["user_name"].as_str().unwrap_or_default().to_owned();
    let caption = data["description"].as_str().unwrap_or_default().to_owned();
    let mut items = Vec::new();
    for (index, raw) in files.iter().enumerate() {
        let Some(url) = super::file_url(raw, ROOT) else {
            continue;
        };
        data.insert("num".into(), (index + 1).into());
        super::nameext_into(raw, &mut data);
        items.push(
            Item::new(url, data.clone())
                .ids(post_id.clone(), (index + 1).to_string())
                .user(Some(user_name.clone()))
                .caption(Some(caption.clone()))
                .header("Referer", &post_url),
        );
    }
    Ok(items)
}

/// Upstream post metadata and `_extract_thumb`: the page's visible
/// thumbnail, while placeholder images flag warnings and restrictions.
pub(crate) fn parse_page(
    scan: &mut Scan<'_>,
    post_url: &str,
    logged_in: bool,
) -> (Map<String, Value>, String) {
    let parts: Vec<&str> = post_url.rsplitn(3, '/').collect();
    let file = parts.first().copied().unwrap_or_default();
    let mut data = Map::new();
    data.insert("post_category".into(), scan.extr("<title>[", "]").into());
    data.insert("count".into(), parse_int(scan.extr("(", " "), 0).into());
    data.insert(
        "post_id".into(),
        file.split('.').next().unwrap_or_default().into(),
    );
    data.insert(
        "user_id".into(),
        parts.get(1).copied().unwrap_or_default().into(),
    );
    let name = scan.extr("<h2 class=\"UserInfoUserName\">", "</");
    let name = name.rsplit_once('>').map_or(name, |(_, n)| n);
    data.insert("user_name".into(), unescape(name).into());
    data.insert(
        "description".into(),
        unescape(scan.extr("class=\"IllustItemDesc\" >", "</h1>")).into(),
    );
    data.insert("warning".into(), false.into());
    data.insert("password".into(), false.into());
    data.insert("requires".into(), Value::Null);
    data.insert("original".into(), logged_in.into());
    let mut headers = Map::new();
    headers.insert("Referer".into(), post_url.into());
    data.insert("_http_headers".into(), headers.into());

    let mut thumb = String::new();
    loop {
        let img = scan.extr("class=\"IllustItemThumbImg\" src=\"", "\"");
        if img.is_empty() {
            break;
        }
        if img.starts_with("https://cdn.poipiku.com/img/") {
            // `text.rextr(img, "/", ".")`
            let kind = img
                .rsplit_once('/')
                .map(|(_, name)| name.split('.').next().unwrap_or_default())
                .unwrap_or_default();
            match kind {
                "warning" => {
                    data.insert("warning".into(), true.into());
                }
                "publish_pass" => {
                    data.insert("password".into(), true.into());
                }
                "publish_login" => {
                    data.insert("requires".into(), "login".into());
                }
                "publish_follower" => {
                    data.insert("requires".into(), "follow".into());
                }
                "publish_t_rt" => {
                    data.insert("requires".into(), "retweet".into());
                }
                _ => {}
            }
        } else if [
            "https://img.poipiku.com/img/",
            "//img.poipiku.com/img/",
            "/img/",
        ]
        .iter()
        .any(|prefix| img.starts_with(prefix))
        {
            if img.contains("/warning") {
                data.insert("warning".into(), true.into());
            }
        } else {
            thumb = img.to_owned();
        }
    }
    if truthy(data.get("requires"))
        && !truthy(data.get("password"))
        && !scan.extr("PasswordIcon", ">").is_empty()
    {
        data.insert("password".into(), true.into());
    }
    (data, thumb)
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
            t("https://poipiku.com/25049/"),
            Some(Target::User("25049".into(), 0))
        );
        assert_eq!(
            t("https://poipiku.com/25049?x=1"),
            Some(Target::User("25049".into(), 0))
        );
        assert_eq!(
            t("https://poipiku.com/IllustListPcV.jsp?PG=1&ID=25049&KWD="),
            Some(Target::User("25049".into(), 1))
        );
        assert_eq!(
            t("https://poipiku.com/599459/5776587.html"),
            Some(Target::Post("599459".into(), "5776587".into()))
        );
        for raw in [
            "https://poipiku.com/",
            "https://poipiku.com/abc/",
            "https://poipiku.com/123x",
            "https://poipiku.com/IllustListPcV.jsp?ID=1",
            "https://poipiku.com/IllustListPcV.jsp?PG=1&ID=2x",
            "https://www.poipiku.com/1/2.html",
            "https://poipiku.com.evil.com/1/2.html",
            "https://evilpoipiku.com/1/2.html",
            "https://poipiku.com:8443/1/2.html",
            "https://u:p@poipiku.com/1/2.html",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    const PAGE: &str = "<title>[Doodle] Title(3 images)</title>\
        <h2 class=\"UserInfoUserName\"><a href=\"/9/\">Tom &amp; Co</a></h2>\
        <h1 class=\"IllustItemDesc\" >Hello &lt;3</h1>\
        <img class=\"IllustItemThumbImg\" src=\"https://cdn.poipiku.com/img/warning.png\">\
        <img class=\"IllustItemThumbImg\" src=\"https://img-org.poipiku.com/user_img02/9/77_a.png_640.jpg\">\
        <a class=\"BtnBase\" onclick=\"ShowAppendFile(9,77)\">more</a>";

    #[tokio::test]
    async fn anonymous_posts_list_appended_files() {
        let append = json!({"result_num": 2, "html":
            "<img class=\"IllustItemThumbImg\" src=\"https://img-org.poipiku.com/user_img02/9/b.png\">\
             <img class=\"IllustItemThumbImg\" src=\"//img-org.poipiku.com/user_img02/9/c.jpg\">"});
        let (root, server) = serve(vec![html(PAGE), json(append)]);
        let http = http("poipiku", root);
        let files = media(extract(
            &http,
            Url::parse("https://poipiku.com/9/77.html").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 3);
        assert_eq!(
            files[2].url.as_str(),
            "https://img-org.poipiku.com/user_img02/9/c.jpg"
        );
        assert_eq!(files[0].headers["Referer"], "https://poipiku.com/9/77.html");
        let k = keywords(&files[1]);
        assert_eq!(k["post_category"], "Doodle");
        assert_eq!(k["count"], 3);
        assert_eq!(k["user_name"], "Tom & Co");
        assert_eq!(k["description"], "Hello <3");
        assert_eq!(k["warning"], true);
        assert_eq!(k["requires"], Value::Null);
        assert_eq!(k["original"], false);
        assert_eq!(k["num"], 2);
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "77_2.png");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("poipiku/9 Tom & Co")
        );
        let (_, identity) = crate::sites::archive_key(&files[2]).unwrap();
        assert_eq!(identity, r#"["{post_id}_{num}",["77","3"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /9/77.html "));
        assert!(requests[0].contains("LANG=en"));
        assert!(requests[0].contains("POIPIKU_CONTENTS_VIEW_MODE=1"));
        assert!(requests[1].starts_with("POST /f/ShowAppendFileF.jsp "));
        assert!(requests[1].contains("referer: https://poipiku.com/9/77.html"));
        assert!(requests[1].contains("x-requested-with: XMLHttpRequest"));
        assert!(requests[1].ends_with("UID=9&IID=77&PAS=&MD=0&TWF=-1"));
    }

    #[tokio::test]
    async fn user_listing_paginates_by_48() {
        let links: String = (1..=48)
            .map(|n| format!("<a class=\"IllustInfo\" href=\"/9/{n}.html\">"))
            .collect();
        let single = "<title>[Art] A(1 images)</title>\
            <img class=\"IllustItemThumbImg\" src=\"https://img-org.poipiku.com/x/1.png\">";
        let mut replies = vec![html(&links)];
        replies.extend((1..=48).map(|_| html(single)));
        replies.push(html(""));
        let (root, server) = serve(replies);
        let http = http("poipiku", root);
        let files = media(extract(
            &http,
            Url::parse("https://poipiku.com/9/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 48);
        assert_eq!(keywords(&files[47])["post_id"], "48");
        assert_eq!(keywords(&files[0])["subcategory"], "user");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /IllustListPcV.jsp?PG=0&ID=9&KWD= "));
        assert!(requests[1].starts_with("GET /9/1.html "));
        assert!(requests[49].starts_with("GET /IllustListPcV.jsp?PG=1&ID=9&KWD= "));
    }

    #[test]
    fn restricted_posts_flag_their_requirements() {
        let page = "<img class=\"IllustItemThumbImg\" src=\"https://cdn.poipiku.com/img/publish_login.png\">\
            <span class=\"PasswordIcon\">";
        let (data, thumb) = parse_page(&mut Scan::new(page), "https://poipiku.com/1/2.html", true);
        assert_eq!(thumb, "");
        assert_eq!(data["requires"], "login");
        assert_eq!(data["password"], true);
        assert_eq!(data["original"], true);
    }
}
