//! EroMe albums, users and searches, mirroring gallery_dl/extractor/erome.py
//! (including its waits on "Please wait a few moments" pages). Option:
//! `reposts`.
use crate::error;
use crate::sites::gdl::{
    Scan, captures, date, extr, extract_iter, group, nameext_into, parse_query, parse_ts, re,
    unescape, unquote,
};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://www.erome.com";

static ALBUM: Route = Route::new(
    "album",
    "{album_id} {title} {num:>02}.{extension}",
    &["{category}", "{user}"],
    "{album_id}_{num}",
);
static USER: Route = Route::new("user", "{filename}.{extension}", &["{category}"], "");
static SEARCH: Route = Route::new("search", "{filename}.{extension}", &["{category}"], "");

pub static SITE: Site = Site {
    category: "erome",
    name: "EroMe",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&ALBUM, &USER, &SEARCH],
    api_key: false,
    cookies: Some("erome.com"),
};

#[derive(Debug, PartialEq)]
enum Target {
    Album(String),
    User(String, String),
    Search(String),
}

fn target(url: &Url) -> Option<Target> {
    const BASE: &str = r"^https?://(?:www\.)?erome\.com";
    if let Some(c) = captures(re!(&format!(r"{BASE}/a/(\w+)")), url) {
        return Some(Target::Album(group(&c, 1).into()));
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}/search/?\?(q=[^#]+)")), url) {
        return Some(Target::Search(group(&c, 1).into()));
    }
    let c = captures(re!(&format!(r"{BASE}/([^/?#]+)(?:/?\?([^#]+))?")), url)?;
    (&c[1] != "a" && !(&c[1] == "search" && url.query().is_some()))
        .then(|| Target::User(group(&c, 1).into(), group(&c, 2).into()))
}

/// GET a page, waiting out the "Please wait" interstitial (5 tries).
async fn request(http: &Http, url: &Url) -> crate::Result<String> {
    for _ in 0..5 {
        let page = http.text(url).await?;
        if !page
            .get(..600.min(page.len()))
            .unwrap_or(&page)
            .contains("<title>Please wait a few moments</title>")
        {
            return Ok(page);
        }
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
    Err(error("EroMe kept answering with a waiting page"))
}

/// `_pagination`: album IDs from listing pages, 36 per full page.
fn listing<'a>(
    http: &'a Http,
    route: &'static Route,
    url: String,
    mut params: Vec<(String, String)>,
) -> Events<'a> {
    let first = params
        .iter()
        .position(|(k, _)| k == "page")
        .map_or(1, |i| crate::sites::art::parse_int(&params.remove(i).1, 1));
    crate::sites::paginate(Some(first), move |state| {
        let url = url.clone();
        let params = params.clone();
        async move {
            let Some(number) = state else { return Ok(None) };
            let mut request_url = http.api(&url)?;
            request_url
                .query_pairs_mut()
                .extend_pairs(params.iter())
                .append_pair("page", &number.to_string());
            let page = request(http, &request_url).await?;
            let ids: Vec<String> = re!(r"(?:https?://)?(?:www\.)?erome\.com/a/(\w+)")
                .captures_iter(&page)
                .map(|c| c[1].to_owned())
                .step_by(2)
                .collect();
            let full = ids.len() >= 36;
            let albums = ids
                .into_iter()
                .filter_map(|id| Some((Url::parse(&format!("{ROOT}/a/{id}")).ok()?, Map::new())))
                .collect();
            Ok(Some((
                super::queue(http, &SITE, route, albums),
                full.then_some(number + 1),
            )))
        }
    })
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported EroMe URL"));
    };
    match target {
        Target::Album(id) => crate::sites::once(async move {
            let album_url = format!("{ROOT}/a/{id}");
            let page = request(http, &http.api(&album_url)?)
                .await
                .map_err(|e| error(format!("{id}: Unable to fetch album page ({e})")))?;
            let mut scan = Scan::new(&page);
            let title = scan
                .extr("property=\"og:title\" content=\"", "\"")
                .to_owned();
            let profile = page[scan.pos()..]
                .find("<div class=\"user-profile")
                .map_or(page.len(), |i| scan.pos() + i);
            let mut scan = Scan::at(&page, profile);
            let user = scan.extr("href=\"https://www.erome.com/", "\"").to_owned();
            let tags = scan.find("<p class=\"mt-10\"", "</p>").map(str::to_owned);
            let mut urls = Vec::new();
            let mut posted = None;
            for group in page.split("<div class=\"media-group\"").skip(1) {
                let url = match extr(group, "<source src=\"", "\"") {
                    "" => extr(group, "data-src=\"", "\""),
                    url => url,
                };
                if !url.is_empty() {
                    urls.push(url.to_owned());
                }
                if posted.is_none() {
                    let ts = extr(group, "?v=", "\"");
                    if ts.len() > 1 {
                        posted = parse_ts(&Value::from(ts));
                    }
                }
            }
            let mut data = Map::new();
            data.insert("album_id".into(), id.as_str().into());
            data.insert("title".into(), unescape(&title).into());
            data.insert("user".into(), unquote(&user).into());
            data.insert("count".into(), urls.len().into());
            data.insert("date".into(), date(posted));
            let tags: Vec<String> = tags.as_deref().map_or_else(Vec::new, |t| {
                extract_iter(t, "?q=", "\"")
                    .into_iter()
                    .map(|t| t.replace('+', " "))
                    .collect()
            });
            data.insert("tags".into(), tags.into());
            let items = urls
                .iter()
                .enumerate()
                .filter_map(|(index, raw)| {
                    let mut fields = data.clone();
                    fields.insert("num".into(), (index + 1).into());
                    nameext_into(raw, &mut fields);
                    Some(
                        Item::new(Url::parse(raw).ok()?, fields)
                            .ids(id.clone(), (index + 1).to_string())
                            .header("Referer", &album_url),
                    )
                })
                .collect();
            Ok(crate::sites::events(crate::sites::post(
                &SITE, http, &ALBUM, items,
            )))
        }),
        Target::User(user, query) => {
            let mut params: Vec<(String, String)> = parse_query(&query)
                .into_iter()
                .map(|(k, v)| (k, crate::sites::scalar(&v)))
                .collect();
            if !params.iter().any(|(k, _)| k == "t") && !http.option_bool("reposts", false) {
                params.push(("t".into(), "posts".into()));
            }
            listing(http, &USER, format!("{ROOT}/{user}"), params)
        }
        Target::Search(query) => {
            let params = parse_query(&query)
                .into_iter()
                .map(|(k, v)| (k, crate::sites::scalar(&v)))
                .collect();
            listing(http, &SEARCH, format!("{ROOT}/search"), params)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{children, html, http as client, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.erome.com/a/NQgdlWvk"),
            Some(Target::Album("NQgdlWvk".into()))
        );
        assert_eq!(
            t("https://www.erome.com/yYgWBZw8o8qsMzM?t=reposts"),
            Some(Target::User("yYgWBZw8o8qsMzM".into(), "t=reposts".into()))
        );
        assert_eq!(
            t("https://www.erome.com/search?q=cute"),
            Some(Target::Search("q=cute".into()))
        );
    }

    #[tokio::test]
    async fn extracts_album_media() {
        let page = r#"<meta property="og:title" content="Cute &amp; Nice"><div class="user-profile"><a href="https://www.erome.com/john3884">
            <p class="mt-10"><a href="/search?q=tag+one">#</a></p>
            <div class="media-group"><img data-src="https://s1.erome.com/1/a.jpg?v=1600000000"></div>
            <div class="media-group"><video><source src="https://v1.erome.com/1/b.mp4" type="video/mp4"></video></div>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = client("erome", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.erome.com/a/NQgdlWvk").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(files[1].url.as_str(), "https://v1.erome.com/1/b.mp4");
        assert_eq!(
            files[1].headers["Referer"],
            "https://www.erome.com/a/NQgdlWvk"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["user"], "john3884");
        assert_eq!(k["tags"], serde_json::json!(["tag one"]));
        assert_eq!(k["date"], "2020-09-13 12:26:40");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            file.render(&files[0]).unwrap(),
            "NQgdlWvk Cute & Nice 01.jpg"
        );
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("erome/john3884")
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn user_lists_posts() {
        let (root, server) = serve(vec![html(
            r#"<a href="https://www.erome.com/a/AAA"><a href="https://www.erome.com/a/AAA"><a href="https://www.erome.com/a/BBB"><a href="https://www.erome.com/a/BBB">"#,
        )]);
        let http = client("erome", root);
        assert_eq!(
            children(extract(
                &http,
                Url::parse("https://www.erome.com/john3884").unwrap()
            ))
            .await
            .len(),
            2
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /john3884?t=posts&page=1 "));
    }
}
