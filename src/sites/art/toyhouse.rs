//! Toyhouse art galleries and single images, mirroring
//! gallery_dl/extractor/toyhouse.py: gallery pages list 18 items each and
//! image pages carry the same credit and character markup.
use super::{Scan, date, extract_iter, leading_digits, nameext_into};
use super::{split_html, strptime};
use crate::sites::http::Body;
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://toyhou.se";

pub static SITE: Site = Site {
    category: "toyhouse",
    name: "Toyhouse",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&ART, &IMAGE],
    api_key: false,
    cookies: Some("toyhou.se"),
};

const DIRECTORY: &[&str] = &["{category}", "{user|artists!S}"];

static ART: Route = Route::new("art", "{filename}.{extension}", DIRECTORY, "{id}");
static IMAGE: Route = Route::new("image", "{filename}.{extension}", DIRECTORY, "{id}");

#[derive(Debug, PartialEq)]
enum Target {
    Art(String),
    Image(String),
}

fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) {
        return None;
    }
    let parts = super::raw_segments(url);
    if super::on_host(url, &["toyhou.se"]) {
        return match parts.as_slice() {
            ["~images", id, ..] => Some(Target::Image(leading_digits(id)?.to_owned())),
            [user, art, ..]
                if art.starts_with("art") && url.path().starts_with(&format!("/{user}/")) =>
            {
                Some(Target::Art((*user).to_owned()))
            }
            _ => None,
        };
    }
    // `f\d+\.toyhou\.se/file/[^/?#]+/(?:image|watermark)s/(\d+)`
    let shard = url
        .host_str()?
        .strip_suffix(".toyhou.se")?
        .strip_prefix('f')?;
    if !super::digits(shard) {
        return None;
    }
    match parts.as_slice() {
        ["file", _, "images" | "watermarks", id, ..] => {
            Some(Target::Image(leading_digits(id)?.to_owned()))
        }
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Toyhouse URL"));
    };
    match target {
        Target::Art(user) => gallery(http, user),
        Target::Image(id) => crate::sites::once(async move {
            let page = http
                .text(&http.api(&format!("{ROOT}/~images/{id}"))?)
                .await?;
            let post = parse_post(&page, "<img class=\"mw-100\" src=\"");
            let items = post
                .and_then(|post| item(post, &Map::new()))
                .into_iter()
                .collect();
            Ok(crate::sites::events(crate::sites::post(
                &SITE, http, &IMAGE, items,
            )))
        }),
    }
}

/// Upstream `_parse_post`: file URL, upload date, credited artists and
/// tagged characters of one gallery entry or image page.
pub(crate) fn parse_post(post: &str, needle: &str) -> Option<Map<String, Value>> {
    let mut scan = Scan::new(post);
    let url = scan.extr(needle, "\"");
    let created = strptime(
        scan.extr("</h2>\n            <div class=\"mb-1\">", "<"),
        "%d %b %Y, %I:%M:%S %p",
    );
    // Upstream splits on a misspelled marker, so all credits stay together.
    let artists: Vec<Value> = scan
        .extr(
            "<div class=\"artist-credit\">",
            "</div>\n                    </div>",
        )
        .split("<div class=\"ar tist-credit\">")
        .map(|artist| crate::sites::remove_html(artist).into())
        .collect();
    let characters: Vec<Value> = split_html(scan.extr(
        "<div class=\"image-characters",
        "<div class=\"image-comments\">",
    ))
    .into_iter()
    .skip(2)
    .map(Value::from)
    .collect();
    let mut data = Map::new();
    data.insert("url".into(), url.into());
    data.insert("date".into(), date(created));
    data.insert("artists".into(), artists.into());
    data.insert("characters".into(), characters.into());
    Some(data)
}

/// Upstream `items` for one post: `{id}_{hash}` from the file name.
fn item(mut post: Map<String, Value>, metadata: &Map<String, Value>) -> Option<Item> {
    super::update(&mut post, metadata);
    let raw = post
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let url = super::file_url(&raw, ROOT)?;
    nameext_into(&raw, &mut post);
    let filename = post
        .get("filename")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let (id, hash) = filename.split_once('_').unwrap_or((&filename, ""));
    post.insert("id".into(), id.into());
    post.insert("hash".into(), hash.into());
    let user = post
        .get("user")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            post.get("artists")
                .and_then(|a| a.get(0))
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    Some(Item::new(url, post).ids(id, id).user(user))
}

/// Upstream `_accept_content_warning`: post the form's token and user to
/// the warning endpoint. Returns whether the page had such a form.
async fn accept_content_warning(http: &Http, page: &str) -> Result<bool> {
    let start = page.find(" name=\"_token\"").map_or(0, |pos| pos + 1);
    let mut scan = Scan::new(&page[start..]);
    let token = scan.extr(" value=\"", "\"");
    let user = scan.extr(" value=\"", "\"");
    if token.is_empty() || user.is_empty() {
        return Ok(false);
    }
    let url = http.api(&format!("{ROOT}/~account/warnings/accept"))?;
    let form = [("_token", token), ("user", user)];
    // The endpoint answers with a redirect, which the HTTP layer refuses
    // for POST requests after storing the session cookies it sets.
    let _ = http
        .post(&url, Body::Form(&form), &[("Origin", ROOT)])
        .await;
    Ok(true)
}

/// Upstream `_pagination` over `/USER/art?page=N`; an empty first page
/// may be a content warning, accepted once before retrying.
fn gallery(http: &Http, user: String) -> Events<'_> {
    let mut metadata = Map::new();
    metadata.insert("user".into(), user.clone().into());
    crate::sites::paginate(Some((1u64, false)), move |state| {
        let (user, metadata) = (user.clone(), metadata.clone());
        async move {
            let Some((page, accepted)) = state else {
                return Ok(None);
            };
            let mut url = http.api(&format!("{ROOT}/{user}/art"))?;
            url.query_pairs_mut().append_pair("page", &page.to_string());
            let text = http.text(&url).await?;
            let posts = extract_iter(&text, "<li class=\"gallery-item\">", "</li>");
            if posts.is_empty() && page == 1 {
                if !accepted && accept_content_warning(http, &text).await? {
                    return Ok(Some((crate::sites::events(Vec::new()), Some((1, true)))));
                }
                return Ok(None);
            }
            let next = (posts.len() >= 18).then_some((page + 1, accepted));
            let events = posts
                .iter()
                .filter_map(|post| parse_post(post, "<a href=\""))
                .filter_map(|post| item(post, &metadata))
                .flat_map(|item| crate::sites::post(&SITE, http, &ART, vec![item]))
                .collect();
            Ok(Some((crate::sites::events(events), next)))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.toyhou.se/d-floe/art"),
            Some(Target::Art("d-floe".into()))
        );
        assert_eq!(
            t("https://toyhou.se/~images/40587320"),
            Some(Target::Image("40587320".into()))
        );
        assert_eq!(
            t(
                "https://f2.toyhou.se/file/f2-toyhou-se/watermarks/36817425_bqhGcwcnU.png?1625561467"
            ),
            Some(Target::Image("36817425".into()))
        );
        assert_eq!(
            t("https://f2.toyhou.se/file/f2-toyhou-se/images/36817425_bq.png"),
            Some(Target::Image("36817425".into()))
        );
        for raw in [
            "https://toyhou.se/d-floe",
            "https://toyhou.se/d-floe/characters",
            "https://toyhou.se/~images/x",
            "https://fx.toyhou.se/file/a/images/1",
            "https://f2.toyhou.se/file/a/other/1",
            "https://toyhou.se.evil.com/u/art",
            "https://eviltoyhou.se/u/art",
            "https://a.toyhou.se/u/art",
            "https://toyhou.se:8443/u/art",
            "https://u:p@toyhou.se/u/art",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn entry(id: u64) -> String {
        format!(
            "<li class=\"gallery-item\"><a href=\"https://f2.toyhou.se/file/f2-toyhou-se/images/{id}_HaSh.png?1\">\
             <h2>x</h2>\n            <div class=\"mb-1\">02 Jan 2020, 03:04:05 pm</div>\
             <div class=\"artist-credit\"><a href=\"/a\">Alice</a></div>\n                    </div>\
             <div class=\"image-characters\"><b>Chars</b><a>Foo</a><a>Bar</a>\
             <div class=\"image-comments\"></li>"
        )
    }

    #[tokio::test]
    async fn gallery_accepts_the_content_warning_and_paginates() {
        let warning = "<form><input type=\"hidden\" name=\"_token\" value=\"TOK\">\
            <input name=\"user\" value=\"d-floe\"></form>";
        let first: String = (1..=18).map(entry).collect();
        let (root, server) = serve(vec![
            html(warning),
            html(""),
            html(&first),
            html(&entry(19)),
        ]);
        let http = http("toyhouse", root);
        let files = media(extract(
            &http,
            Url::parse("https://toyhou.se/d-floe/art").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 19);
        let k = keywords(&files[0]);
        assert_eq!(k["id"], "1");
        assert_eq!(k["hash"], "HaSh");
        assert_eq!(k["user"], "d-floe");
        assert_eq!(k["date"], "2020-01-02 15:04:05");
        assert_eq!(k["artists"], serde_json::json!(["Alice"]));
        assert_eq!(k["characters"], serde_json::json!(["Foo", "Bar"]));
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "1_HaSh.png");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("toyhouse/d-floe")
        );
        let (_, identity) = crate::sites::archive_key(&files[18]).unwrap();
        assert_eq!(identity, r#"["{id}",["19"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /d-floe/art?page=1 "));
        assert!(requests[1].starts_with("POST /~account/warnings/accept "));
        assert!(requests[1].ends_with("_token=TOK&user=d-floe"));
        assert!(requests[2].starts_with("GET /d-floe/art?page=1 "));
        assert!(requests[3].starts_with("GET /d-floe/art?page=2 "));
    }

    #[tokio::test]
    async fn image_pages_credit_their_artists() {
        let page = "<img class=\"mw-100\" src=\"https://f2.toyhou.se/file/f2-toyhou-se/images/7_abc.jpg?2\">\
            <h2>x</h2>\n            <div class=\"mb-1\">1 Feb 2021, 10:00:00 am</div>\
            <div class=\"artist-credit\"><a>Bob</a> <span>(owner)</span></div>\n                    </div>";
        let (root, server) = serve(vec![html(page)]);
        let http = http("toyhouse", root);
        let files = media(extract(
            &http,
            Url::parse("https://f2.toyhou.se/file/f2-toyhou-se/watermarks/7_abc.jpg").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let k = keywords(&files[0]);
        assert_eq!(k["artists"], serde_json::json!(["Bob (owner)"]));
        assert_eq!(k["characters"], serde_json::json!([]));
        assert!(!k.contains_key("user"));
        let (_, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("toyhouse/Bob (owner)")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /~images/7 "));
    }
}
