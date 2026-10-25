//! XVIDEOS profile photo galleries, mirroring
//! gallery_dl/extractor/xvideos.py: a gallery's pages of 500 photos are
//! followed through their `Next` links, and a profile queues its galleries
//! from the page's `xv.conf` data.
use crate::error;
use crate::sites::art::{
    Scan, extr, extract_iter, json_loads, leading_digits, on_host, parse_int, segment,
};
use crate::sites::manga::{Gallery, keywords};
use crate::sites::{Events, Http, Route, Site, unescape};
use futures_util::{StreamExt, stream};
use serde_json::{Map, Value, json};
use url::Url;

const ROOT: &str = "https://www.xvideos.com";
/// Photo pages read at most for one gallery.
const MAX_PAGES: usize = 100;

static GALLERY: Route = Route::new(
    "gallery",
    "{category}_{gallery[id]}_{num:>03}.{extension}",
    &[
        "{category}",
        "{user[name]}",
        "{gallery[id]} {gallery[title]}",
    ],
    "{gallery[id]}_{num}",
);
static USER: Route = Route::new("user", "{filename}.{extension}", &["{category}"], "");

pub static SITE: Site = Site {
    category: "xvideos",
    name: "XVIDEOS",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&GALLERY, &USER],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Gallery(String, String),
    User(String),
}

/// `xvideos\.com/(?:profiles|(?:amateur-|model-)?channels)/USER...`.
fn target(url: &Url) -> Option<Target> {
    if !on_host(url, &["xvideos.com"]) {
        return None;
    }
    let path = url.path();
    let parts: Vec<&str> = path[1..].split('/').collect();
    let [kind, user, rest @ ..] = parts.as_slice() else {
        return None;
    };
    if !matches!(
        *kind,
        "profiles" | "channels" | "amateur-channels" | "model-channels"
    ) || !segment(user)
    {
        return None;
    }
    match rest {
        ["photos", id, ..] => {
            leading_digits(id).map(|id| Target::Gallery((*user).to_owned(), id.to_owned()))
        }
        // `/?(?:#.*)?$`: no query may follow.
        [] | [""] if url.query().is_none() => Some(Target::User((*user).to_owned())),
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Gallery(user, id)) => crate::sites::once(async move {
            let page = http
                .text(&http.api(&format!("{ROOT}/profiles/{user}/photos/{id}"))?)
                .await?;
            let data = metadata(&page, &user, &id);
            let pages = images(http, page)
                .await?
                .into_iter()
                .map(|url| (url, None))
                .collect();
            let gallery = Gallery {
                enum_key: "num",
                ..Gallery::chapter(&GALLERY, data, pages).id(id)
            };
            Ok(crate::sites::events(gallery.events(&SITE, http)))
        }),
        Some(Target::User(user)) => crate::sites::once(async move {
            let page = http
                .text(&http.api(&format!("{ROOT}/profiles/{user}"))?)
                .await?;
            let conf = json_loads(extr(&page, "xv.conf=", ";</script>"), "XVIDEOS")?;
            let children: Vec<(Url, Map<String, Value>)> = galleries(&conf)
                .into_iter()
                .filter_map(|gallery| {
                    let id = gallery.get("id")?.as_i64()?;
                    let url = Url::parse(&format!("{ROOT}/profiles/{user}/photos/{id}")).ok()?;
                    Some((url, gallery))
                })
                .collect();
            Ok(stream::iter(children)
                .map(move |(child, data)| {
                    crate::sites::queue(http, SITE.category, USER.subcategory, child, &data)
                })
                .flatten()
                .boxed())
        }),
        None => crate::sites::fail(error("unsupported XVIDEOS URL")),
    }
}

fn metadata(page: &str, user: &str, id: &str) -> Map<String, Value> {
    let mut scan = Scan::new(page);
    let user_id = parse_int(scan.extr("\"id_user\":", ","), 0);
    let display = scan.extr("\"display\":\"", "\"");
    let sex = scan.extr("\"sex\":\"", "\"");
    let title = scan.extr("\"title\":\"", "\"");
    let description = scan
        .extr("<small class=\"mobile-hide\">", "</small>")
        .trim();
    let tags = scan.extr("<em>Tagged:</em>", "<").trim();
    let tags: Vec<String> = if tags.is_empty() {
        Vec::new()
    } else {
        unescape(tags).split(", ").map(str::to_owned).collect()
    };
    keywords([
        (
            "user",
            json!({"id": user_id, "display": display, "sex": sex, "name": user,
                "description": description}),
        ),
        (
            "gallery",
            json!({"id": parse_int(id, 0), "title": unescape(title), "tags": tags}),
        ),
    ])
}

/// The photo links, continued through the `Next` page while every page so
/// far was full (500 photos).
async fn images(http: &Http, mut page: String) -> crate::Result<Vec<String>> {
    let marker = "<a class=\"embed-responsive-item\" href=\"";
    let mut results: Vec<String> = extract_iter(&page, marker, "\"")
        .into_iter()
        .map(str::to_owned)
        .collect();
    for _ in 0..MAX_PAGES {
        if results.is_empty() || !results.len().is_multiple_of(500) {
            break;
        }
        // `text.rextr(page, ' href="', '"', page.find(">Next</"))`.
        let pos = page.find(">Next</").unwrap_or(page.len().saturating_sub(1));
        let path = rextr(&page, " href=\"", "\"", pos);
        if path.is_empty() {
            break;
        }
        page = http.text(&http.api(&format!("{ROOT}{path}"))?).await?;
        results.extend(
            extract_iter(&page, marker, "\"")
                .into_iter()
                .map(str::to_owned),
        );
    }
    Ok(results)
}

/// gallery-dl `text.rextr`: the text after the last `begin` before `pos`,
/// up to the next `end`.
fn rextr<'t>(text: &'t str, begin: &str, end: &str, pos: usize) -> &'t str {
    let pos = (0..=pos.min(text.len()))
        .rev()
        .find(|i| text.is_char_boundary(*i))
        .unwrap_or(0);
    let Some(start) = text[..pos].rfind(begin).map(|i| i + begin.len()) else {
        return "";
    };
    text[start..]
        .find(end)
        .map_or("", |stop| &text[start..start + stop])
}

/// The profile's galleries but the `0` (profile pictures) one, by ID.
fn galleries(conf: &Value) -> Vec<Map<String, Value>> {
    let Some(galleries) = conf
        .get("data")
        .and_then(|data| data.get("galleries"))
        .and_then(Value::as_object)
    else {
        return Vec::new();
    };
    let mut out: Vec<Map<String, Value>> = galleries
        .iter()
        .filter(|(id, _)| id.as_str() != "0")
        .map(|(id, gallery)| {
            keywords([
                ("id", parse_int(id, 0).into()),
                (
                    "title",
                    unescape(crate::sites::art::str_of(gallery, "title")).into(),
                ),
                (
                    "count",
                    gallery.get("nb_pics").cloned().unwrap_or(Value::Null),
                ),
            ])
        })
        .collect();
    out.sort_by_key(|gallery| gallery.get("id").and_then(Value::as_i64).unwrap_or(0));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::manga::tests::{archive, path};
    use crate::sites::testing::{html, http, keywords as kw, media, queued, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.xvideos.com/profiles/someone/photos/12345/title"),
            Some(Target::Gallery("someone".into(), "12345".into()))
        );
        assert_eq!(
            t("https://xvideos.com/amateur-channels/someone/photos/7"),
            Some(Target::Gallery("someone".into(), "7".into()))
        );
        assert_eq!(
            t("https://www.xvideos.com/model-channels/someone/#_tabPhotos"),
            Some(Target::User("someone".into()))
        );
        for raw in [
            "https://www.xvideos.com/profiles/someone?x=1",
            "https://www.xvideos.com/profiles/someone/videos",
            "https://www.xvideos.com/users/someone",
            "https://www.xvideos.com.evil.test/profiles/someone",
            "https://www.xvideos.com:8080/profiles/someone",
            "https://u:p@www.xvideos.com/profiles/someone",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
        assert_eq!(
            rextr("a href=\"1\" href=\"2\" >Next</", " href=\"", "\"", 18),
            "2"
        );
    }

    #[tokio::test]
    async fn extracts_gallery() {
        let page = r#"<script>{"id_user":42,"display":"Some One","sex":"Woman","title":"Trip &amp; Tour"}</script>
<small class="mobile-hide"> About me </small><em>Tagged:</em> sea, sun<
<a class="embed-responsive-item" href="https://img.xv.test/p/1.jpg"></a>
<a class="embed-responsive-item" href="https://img.xv.test/p/2.jpg"></a>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = http("xvideos", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.xvideos.com/channels/someone/photos/12345").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = kw(&files[1]);
        assert_eq!(
            k["user"],
            json!({"id": 42, "display": "Some One", "sex": "Woman", "name": "someone",
                "description": "About me"})
        );
        assert_eq!(
            k["gallery"],
            json!({"id": 12345, "title": "Trip & Tour", "tags": ["sea", "sun"]})
        );
        assert_eq!(k["count"], 2);
        assert_eq!(
            path(&files[1]),
            "xvideos/someone/12345 Trip & Tour/xvideos_12345_002.jpg"
        );
        assert_eq!(
            archive(&files[1]),
            r#"["{gallery[id]}_{num}",["12345","2"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /profiles/someone/photos/12345 "));
    }

    #[tokio::test]
    async fn queues_profile_galleries_by_id() {
        let page = r#"<script>xv.conf={"data":{"galleries":{"0":{"title":"Profile","nb_pics":1},
"20":{"title":"B","nb_pics":3},"3":{"title":"A &amp; C","nb_pics":2}}}};</script>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = http("xvideos", root);
        let children = queued(extract(
            &http,
            Url::parse("https://www.xvideos.com/profiles/someone").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].fields()["id"], 3);
        assert_eq!(children[1].fields()["id"], 20);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /profiles/someone "));
    }
}
