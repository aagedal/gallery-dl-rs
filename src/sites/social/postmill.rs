//! Postmill instances (Raddle and `postmill:https://...`): posts, short
//! URLs and submission listings, mirroring gallery_dl/extractor/postmill.py.
use crate::error;
use crate::sites::gdl::{
    Scan, date, extr, extract_iter, nameext_into, parse_iso, parse_query, re, text_item, truthy,
    unescape,
};
use crate::sites::{Events, Http, Item, Route, Site};
use futures_util::StreamExt;
use serde_json::{Map, Value};
use url::Url;

const FILENAME: &str = "{id}_{title[:220]}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{instance}", "{forum}"];

macro_rules! routes {
    ($($ident:ident $sub:literal),*) => { $(static $ident: Route = Route::new($sub, FILENAME, DIRECTORY, "{filename}");)* };
}
routes!(POST "post", SHORTURL "shorturl", HOME "home", FORUM "forum", USERSUBMISSIONS "usersubmissions", TAG "tag", SEARCH "search");
const ROUTES: &[&Route] = &[
    &POST,
    &SHORTURL,
    &HOME,
    &FORUM,
    &USERSUBMISSIONS,
    &TAG,
    &SEARCH,
];

pub static RADDLE: Site = Site {
    category: "raddle",
    name: "Raddle",
    matches: |url| target(url, false).is_some(),
    extract: |http, url| extract(&RADDLE, false, http, url),
    routes: ROUTES,
    api_key: false,
    cookies: Some("raddle.me"),
};

// `postmill:https://host/...` for instances without a built-in entry.
pub static POSTMILL: Site = Site {
    category: "postmill",
    name: "Postmill",
    matches: |url| target(url, true).is_some(),
    extract: |http, url| extract(&POSTMILL, true, http, url),
    routes: ROUTES,
    api_key: false,
    cookies: None,
};

#[derive(Debug)]
enum Kind {
    Post(String, String),
    Short(String),
    /// Route, base path, sorting path and query.
    Listing(&'static Route, String, String, String),
}

impl PartialEq for Kind {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Post(a, b), Self::Post(c, d)) => a == c && b == d,
            (Self::Short(a), Self::Short(b)) => a == b,
            (Self::Listing(r, a, b, c), Self::Listing(s, d, e, f)) => {
                r.subcategory == s.subcategory && (a, b, c) == (d, e, f)
            }
            _ => false,
        }
    }
}

/// Root and target of a Raddle (or prefixed) URL.
fn target(url: &Url, prefixed: bool) -> Option<(String, Kind)> {
    let raw = url.as_str();
    let (root, rest) = if prefixed {
        let c = re!(r"^postmill:(https?://[^/?#]+)(.*)$").captures(raw)?;
        (c[1].to_owned(), c[2].to_owned())
    } else {
        let c = re!(r"^(https?://(?:raddle\.me|c32zjeghcp5tj3kb72pltz56piei66drc63vkhn5yixiyk4cmerrjtid\.onion))(.*)$").captures(raw)?;
        (c[1].to_owned(), c[2].to_owned())
    };
    const SORTING: &str = r"(/(?:hot|new|active|top|controversial|most_commented))?(?:\?([^#]+))?$";
    let opt = |c: &regex::Captures, i: usize| c.get(i).map_or("", |m| m.as_str()).to_owned();
    let kind = if let Some(c) = re!(r"^/f/(\w+)/(\d+)").captures(&rest) {
        Kind::Post(c[1].to_owned(), c[2].to_owned())
    } else if let Some(c) = re!(r"^(/\d+)$").captures(&rest) {
        Kind::Short(c[1].to_owned())
    } else if let Some(c) =
        re!(&format!(r"^(/(?:featured|subscribed|all)?){SORTING}")).captures(&rest)
    {
        Kind::Listing(&HOME, c[1].to_owned(), opt(&c, 2), opt(&c, 3))
    } else if let Some(c) = re!(&format!(r"^(/f/\w+){SORTING}")).captures(&rest) {
        Kind::Listing(&FORUM, c[1].to_owned(), opt(&c, 2), opt(&c, 3))
    } else if let Some(c) = re!(r"^(/user/\w+/submissions)(?:\?([^#]+))?$").captures(&rest) {
        Kind::Listing(&USERSUBMISSIONS, c[1].to_owned(), String::new(), opt(&c, 2))
    } else if let Some(c) = re!(&format!(r"^(/tag/\w+){SORTING}")).captures(&rest) {
        Kind::Listing(&TAG, c[1].to_owned(), opt(&c, 2), opt(&c, 3))
    } else {
        let c = re!(r"^(/search)\?(q=[^#]+)$").captures(&rest)?;
        Kind::Listing(&SEARCH, c[1].to_owned(), String::new(), c[2].to_owned())
    };
    Some((root, kind))
}

/// `PostmillExtractor.items` for one submission page.
async fn post<'a>(
    site: &'static Site,
    http: &'a Http,
    root: &str,
    url: &str,
) -> crate::Result<Events<'a>> {
    let page = http.text(&http.api(url)?).await?;
    let mut scan = Scan::new(&page);
    let title = unescape(scan.extr("<meta property=\"og:title\" content=\"", "\">"));
    let published = date(parse_iso(scan.extr(
        "<meta property=\"og:article:published_time\" content=\"",
        "\">",
    )));
    let username = scan
        .extr("<meta property=\"og:article:author\" content=\"", "\">")
        .to_owned();
    let canonical = unescape(scan.extr("<link rel=\"canonical\" href=\"", "\">"));
    let link = unescape(scan.extr(
        "<h1 class=\"submission__title unheaderize inline\"><a href=\"",
        "\"",
    ));
    let body = scan
        .extr(
            "<div class=\"submission__body break-text text-flow\">",
            "</div>",
        )
        .to_owned();
    let c = re!(r"/f/([\w\d_]+)/(\d+)/")
        .captures(&canonical)
        .ok_or_else(|| error("Postmill post has no canonical URL"))?;
    let (forum, id) = (c[1].to_owned(), c[2].parse::<i64>().unwrap_or_default());
    let is_text = link.starts_with('/');
    let is_image = re!(r#"<a href="[^"]+"\n +class="submission__image-link""#).is_match(&page);
    let mut data = Map::new();
    data.insert("title".into(), title.into());
    data.insert("date".into(), published);
    data.insert("username".into(), username.into());
    data.insert("forum".into(), forum.into());
    data.insert("id".into(), id.into());
    let flair: Vec<Value> = extract_iter(&page, "<span class=\"flair__label\">", "</span>")
        .into_iter()
        .map(|f| unescape(f).into())
        .collect();
    data.insert("flair".into(), flair.into());
    data.insert(
        "instance".into(),
        root.split_once("://").map_or(root, |(_, host)| host).into(),
    );
    let save_body = truthy(http.option("save-link-post-body"));
    // (is text, payload): text bodies, image files, or queued links.
    let mut entries: Vec<(&str, String)> = Vec::new();
    if is_text || save_body {
        entries.push(("text", body));
    }
    if is_image {
        entries.push(("url", link));
    } else if !is_text {
        entries.push(("queue", link));
    }
    data.insert("count".into(), entries.len().into());
    let mut parts: Vec<Events<'a>> = Vec::new();
    for (index, (kind, payload)) in entries.into_iter().enumerate() {
        data.insert("num".into(), (index + 1).into());
        let id = id.to_string();
        match kind {
            "text" => {
                data.insert("filename".into(), "".into());
                data.insert("extension".into(), "htm".into());
                if let Some(item) = text_item(payload, data.clone()) {
                    parts.push(crate::sites::events(crate::sites::post(
                        site,
                        http,
                        &POST,
                        vec![item.ids(id, (index + 1).to_string())],
                    )));
                }
            }
            "url" => {
                nameext_into(&payload, &mut data);
                if let Ok(file) = Url::parse(&payload) {
                    parts.push(crate::sites::events(crate::sites::post(
                        site,
                        http,
                        &POST,
                        vec![Item::new(file, data.clone()).ids(id, (index + 1).to_string())],
                    )));
                }
            }
            _ => {
                nameext_into(&payload, &mut data);
                if let Ok(link) = Url::parse(&payload) {
                    parts.push(crate::sites::galleries::queue(
                        http,
                        site,
                        &POST,
                        vec![(link, data.clone())],
                    ));
                }
            }
        }
    }
    Ok(futures_util::stream::iter(parts).flatten().boxed())
}

fn extract<'a>(site: &'static Site, prefixed: bool, http: &'a Http, url: Url) -> Events<'a> {
    let Some((root, kind)) = target(&url, prefixed) else {
        return crate::sites::fail(error("unsupported Postmill URL"));
    };
    let child = move |link: &str| -> Option<Url> {
        let link = if prefixed {
            format!("postmill:{link}")
        } else {
            link.to_owned()
        };
        Url::parse(&link).ok()
    };
    match kind {
        Kind::Post(forum, id) => crate::sites::once(async move {
            post(site, http, &root, &format!("{root}/f/{forum}/{id}")).await
        }),
        Kind::Short(path) => crate::sites::once(async move {
            let short = http.api(&format!("{root}{path}"))?;
            let location = http
                .location(&short, &[])
                .await?
                .ok_or_else(|| error("Postmill short URL did not redirect"))?;
            let children = child(location.as_str())
                .map(|u| (u, Map::new()))
                .into_iter()
                .collect();
            Ok(crate::sites::galleries::queue(
                http, site, &SHORTURL, children,
            ))
        }),
        Kind::Listing(route, base, sorting, query) => {
            let whitelist: &[&str] = if route.subcategory == "search" {
                &["q"]
            } else {
                &[]
            };
            let query: Vec<(String, String)> = parse_query(&query)
                .into_iter()
                .filter(|(key, _)| {
                    whitelist.contains(&key.as_str())
                        || key == "t"
                        || (key.starts_with("next[") && key.ends_with(']'))
                })
                .map(|(k, v)| (k, crate::sites::scalar(&v)))
                .collect();
            let first = format!("{root}{base}{sorting}");
            crate::sites::paginate(Some(first), move |state| {
                let root = root.clone();
                let query = query.clone();
                async move {
                    let Some(page_url) = state else {
                        return Ok(None);
                    };
                    let mut request = http.api(&page_url)?;
                    if !query.is_empty() {
                        request.query_pairs_mut().extend_pairs(query.iter());
                    }
                    let (page, final_url) = http.text_final(&request, &[]).await?;
                    if final_url != request && final_url.as_str() == format!("{root}/login") {
                        return Err(error(format!("HTTP redirect to login page ({final_url})")));
                    }
                    let base = Url::parse(&page_url).map_err(|_| error("invalid Postmill URL"))?;
                    let children = extract_iter(&page, "<nav class=\"submission__nav\">", "</nav>")
                        .into_iter()
                        .filter_map(|nav| {
                            child(
                                base.join(&unescape(extr(nav, "<a href=\"", "\"")))
                                    .ok()?
                                    .as_str(),
                            )
                            .map(|u| (u, Map::new()))
                        })
                        .collect();
                    let next = unescape(extr(&page, "<link rel=\"next\" href=\"", "\">"));
                    Ok(Some((
                        crate::sites::galleries::queue(http, site, route, children),
                        (!next.is_empty()).then_some(next),
                    )))
                }
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{children, html, http as client, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap(), false).map(|(_, k)| k);
        assert_eq!(
            t("https://raddle.me/f/Anime/150698/title"),
            Some(Kind::Post("Anime".into(), "150698".into()))
        );
        assert_eq!(
            t("https://raddle.me/150698"),
            Some(Kind::Short("/150698".into()))
        );
        assert_eq!(
            t("https://raddle.me/f/Anime/new?t=all"),
            Some(Kind::Listing(
                &FORUM,
                "/f/Anime".into(),
                "/new".into(),
                "t=all".into()
            ))
        );
        assert_eq!(
            t("https://raddle.me/"),
            Some(Kind::Listing(
                &HOME,
                "/".into(),
                String::new(),
                String::new()
            ))
        );
        assert_eq!(
            t("https://raddle.me/search?q=tea"),
            Some(Kind::Listing(
                &SEARCH,
                "/search".into(),
                String::new(),
                "q=tea".into()
            ))
        );
        let p = target(
            &Url::parse("postmill:https://postmill.xyz/f/x/1").unwrap(),
            true,
        )
        .unwrap();
        assert_eq!(p.0, "https://postmill.xyz");
    }

    #[tokio::test]
    async fn image_post_saves_file() {
        let page = "<meta property=\"og:title\" content=\"Pic &amp; More\"><meta property=\"og:article:published_time\" content=\"2021-01-02T03:04:05+00:00\">
            <meta property=\"og:article:author\" content=\"jane\"><link rel=\"canonical\" href=\"https://raddle.me/f/Art/42/pic\">
            <h1 class=\"submission__title unheaderize inline\"><a href=\"https://i.example/a.png\">x</a>
            <a href=\"https://i.example/a.png\"\n   class=\"submission__image-link\"><span class=\"flair__label\">OC</span>";
        let (root, server) = serve(vec![html(page)]);
        let http = client("raddle", root);
        let files = media(extract(
            &RADDLE,
            false,
            &http,
            Url::parse("https://raddle.me/f/Art/42").unwrap(),
        ))
        .await;
        assert_eq!(files[0].url.as_str(), "https://i.example/a.png");
        let k = keywords(&files[0]);
        assert_eq!(
            (
                k["instance"].as_str(),
                k["forum"].as_str(),
                k["id"].as_i64()
            ),
            (Some("raddle.me"), Some("Art"), Some(42))
        );
        assert_eq!(k["flair"], serde_json::json!(["OC"]));
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "42_Pic & More.png");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("raddle/raddle.me/Art")
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn text_post_saves_body() {
        let page = "<meta property=\"og:title\" content=\"Text\"><link rel=\"canonical\" href=\"https://raddle.me/f/Talk/7/t\">
            <h1 class=\"submission__title unheaderize inline\"><a href=\"/f/Talk/7/t\">x</a><div class=\"submission__body break-text text-flow\"><p>Hi</p></div>";
        let (root, server) = serve(vec![html(page)]);
        let http = client("raddle", root);
        let files = media(extract(
            &RADDLE,
            false,
            &http,
            Url::parse("https://raddle.me/f/Talk/7").unwrap(),
        ))
        .await;
        assert!(files[0].url.as_str().starts_with("text:"));
        assert_eq!(keywords(&files[0])["extension"], "htm");
        server.join().unwrap();
    }

    #[tokio::test]
    async fn listing_queues_navs_and_follows_next() {
        let first = r#"<nav class="submission__nav"><a href="/f/Art/1/a">x</a></nav><link rel="next" href="https://raddle.me/f/Art?next%5Bid%5D=1">"#;
        let second = r#"<nav class="submission__nav"><a href="/f/Art/2/b">x</a></nav>"#;
        let (root, server) = serve(vec![html(first), html(second)]);
        let http = client("raddle", root);
        assert_eq!(
            children(extract(
                &RADDLE,
                false,
                &http,
                Url::parse("https://raddle.me/f/Art").unwrap()
            ))
            .await
            .len(),
            2
        );
        server.join().unwrap();
    }
}
