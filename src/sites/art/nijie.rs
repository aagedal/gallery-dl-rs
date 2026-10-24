//! Nijie and Horne works, user galleries, bookmarks, feeds and followed
//! users, mirroring gallery_dl/extractor/nijie.py by scraping `view.php`
//! pages.
//!
//! Both sites need a session: upstream's `username`/`password` login is
//! not supported, so a `nijie_tok` or `horne_tok` cookie from `--cookies`
//! is required. Upstream's `request_interval` of two to four seconds is
//! not applied.
use super::{digits, extr, extract_iter, leading_digits, on_host, parse_int};
use crate::sites::{Events, Http, Item, Route, Site, unescape};
use crate::{Result, error};
use futures_util::{StreamExt, stream};
use serde_json::{Map, Value};
use url::Url;

const FILENAME: &str = "{image_id}_p{num}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{user_id}"];
const ARCHIVE: &str = "{image_id}_{num}";

// `Dispatch` sets the subcategory; formats come from `NijieExtractor`.
static USER: Route = Route::new("user", FILENAME, DIRECTORY, ARCHIVE);
static ILLUSTRATION: Route = Route::new("illustration", FILENAME, DIRECTORY, ARCHIVE);
static DOUJIN: Route = Route::new("doujin", FILENAME, DIRECTORY, ARCHIVE);
static FAVORITE: Route = Route::new(
    "favorite",
    FILENAME,
    &["{category}", "bookmarks", "{user_id}"],
    "f_{user_id}_{image_id}_{num}",
);
static NUITA: Route = Route::new(
    "nuita",
    FILENAME,
    &["{category}", "nuita", "{user_id}"],
    "n_{user_id}_{image_id}_{num}",
);
static FEED: Route = Route::new("feed", FILENAME, DIRECTORY, ARCHIVE);
static FOLLOWED: Route = Route::new("followed", FILENAME, DIRECTORY, ARCHIVE);
static IMAGE: Route = Route::new("image", FILENAME, DIRECTORY, ARCHIVE);

macro_rules! instance {
    ($ident:ident, $category:literal, $name:literal, $domain:literal) => {
        pub static $ident: Site = Site {
            category: $category,
            name: $name,
            matches: |url| target($category, url).is_some(),
            extract: |http, url| extract($category, http, url),
            routes: &[
                &USER,
                &ILLUSTRATION,
                &DOUJIN,
                &FAVORITE,
                &NUITA,
                &FEED,
                &FOLLOWED,
                &IMAGE,
            ],
            api_key: false,
            cookies: Some($domain),
        };
    };
}
instance!(NIJIE, "nijie", "Nijie", "nijie.info");
instance!(HORNE, "horne", "Horne", "horne.red");

fn site(category: &str) -> &'static Site {
    match category {
        "horne" => &HORNE,
        _ => &NIJIE,
    }
}

fn root(category: &str) -> &'static str {
    match category {
        "horne" => "https://horne.red",
        _ => "https://nijie.info",
    }
}

/// The listing extractors' routes and page names.
static LISTINGS: &[(&Route, &str)] = &[
    (&ILLUSTRATION, "members_illust"),
    (&DOUJIN, "members_dojin"),
    (&FAVORITE, "user_like_illust_view"),
    (&NUITA, "history_nuita"),
    (&FEED, "like_user_view"),
];

#[derive(Debug, PartialEq)]
enum Target {
    User(String),
    /// A [`LISTINGS`] page name and user ID (`0` for the feed, as upstream).
    Listing(&'static str, i64),
    Followed,
    Image(String),
}

fn target(category: &str, url: &Url) -> Option<Target> {
    let host = &root(category)["https://".len()..];
    if !on_host(url, &[host]) {
        return None;
    }
    let path = url.path();
    // `PAGE\.php\?id=(\d+)`
    let id = || {
        url.query()?
            .strip_prefix("id=")
            .and_then(leading_digits)
            .map(str::to_owned)
    };
    match path {
        "/members.php" => return Some(Target::User(id()?)),
        "/view.php" | "/view_popup.php" => return Some(Target::Image(id()?)),
        _ => {}
    }
    if path.starts_with("/like_user_view.php") {
        return Some(Target::Listing("like_user_view", 0));
    }
    for (_, page) in LISTINGS {
        if path.strip_prefix('/').and_then(|p| p.strip_suffix(".php")) == Some(page) {
            return Some(Target::Listing(page, parse_int(&id()?, 0)));
        }
    }
    path.starts_with("/like_my.php").then_some(Target::Followed)
}

fn extract<'a>(category: &'static str, http: &'a Http, url: Url) -> Events<'a> {
    let Some(target) = target(category, &url) else {
        return crate::sites::fail(error(format!("unsupported {} URL", site(category).name)));
    };
    let events = match target {
        Target::User(id) => return dispatch(http, category, &id),
        Target::Listing(page, id) => listing(http, category, page, id),
        Target::Followed => followed(http, category),
        Target::Image(id) => images(http, category, &IMAGE, vec![id], None),
    };
    // Upstream `login`: without a session it asks for credentials.
    let token = format!("{category}_tok");
    let session = http
        .api(root(category))
        .ok()
        .and_then(|root| http.cookie(&root, &token));
    match session {
        Some(_) => events,
        None => crate::sites::fail(error(format!(
            "{} requires a '{token}' session cookie (--cookies)",
            site(category).name
        ))),
    }
}

/// Upstream `Dispatch` for profiles: queue the `include` extractors.
fn dispatch<'a>(http: &'a Http, category: &'static str, id: &str) -> Events<'a> {
    let root = root(category);
    let children: Vec<Url> = super::include(
        http,
        &["illustration", "doujin", "favorite", "nuita"],
        &["illustration", "doujin"],
    )
    .into_iter()
    .filter_map(|sub| LISTINGS.iter().find(|(route, _)| route.subcategory == sub))
    .filter_map(|(_, page)| Url::parse(&format!("{root}/{page}.php?id={id}")).ok())
    .collect();
    stream::iter(children)
        .map(move |child| crate::sites::queue(http, category, "user", child, &Map::new()))
        .flatten()
        .boxed()
}

/// Upstream `_pagination`: the `illust_id`s of each page, while there is a
/// `rel="next"` link. The first page names the listed user.
fn listing<'a>(
    http: &'a Http,
    category: &'static str,
    page_name: &'static str,
    id: i64,
) -> Events<'a> {
    let route = LISTINGS
        .iter()
        .find_map(|(route, page)| (*page == page_name).then_some(*route))
        .unwrap_or(&FEED);
    crate::sites::paginate(Some((1u64, None::<String>)), move |state| async move {
        let Some((page, name)) = state else {
            return Ok(None);
        };
        let mut url = http.api(&format!("{}/{page_name}.php", root(category)))?;
        url.query_pairs_mut()
            .append_pair("id", &id.to_string())
            .append_pair("p", &page.to_string());
        let text = http.text(&url).await?;
        let name = name.unwrap_or_else(|| match route.subcategory {
            "nuita" => unescape(extr(&text, "<title>", "さんの抜いた")),
            "feed" => String::new(),
            _ => unescape(extr(&text, "<br />", "<")),
        });
        let ids: Vec<String> = extract_iter(&text, "illust_id=\"", "\"")
            .into_iter()
            .filter(|id| digits(id))
            .map(str::to_owned)
            .collect();
        // Bookmarks and nuita always belong to the listed user; elsewhere
        // a named user replaces each work's artist.
        let owner = (matches!(route.subcategory, "favorite" | "nuita") || !name.is_empty())
            .then(|| (id, name.clone()));
        let next = text
            .contains("<a rel=\"next\"")
            .then(|| (page + 1, Some(name)));
        Ok(Some((images(http, category, route, ids, owner), next)))
    })
}

/// Upstream `NijieFollowedExtractor`: queue each followed user's profile.
fn followed<'a>(http: &'a Http, category: &'static str) -> Events<'a> {
    let root = root(category);
    crate::sites::paginate(Some(1u64), move |page| async move {
        let Some(page) = page else {
            return Ok(None);
        };
        let mut url = http.api(&format!("{root}/like_my.php"))?;
        url.query_pairs_mut().append_pair("p", &page.to_string());
        let text = http.text(&url).await?;
        let children: Vec<Url> = extract_iter(&text, "\"><a href=\"/members.php?id=", "\"")
            .into_iter()
            .filter(|id| digits(id))
            .filter_map(|id| Url::parse(&format!("{root}/members.php?id={id}")).ok())
            .collect();
        let next = text.contains("<a rel=\"next\"").then_some(page + 1);
        let events = stream::iter(children)
            .map(move |child| crate::sites::queue(http, category, "followed", child, &Map::new()))
            .flatten()
            .boxed();
        Ok(Some((events, next)))
    })
}

/// Fetch each work's page as it is reached; missing or hidden works are
/// skipped like upstream's HTTP errors.
fn images<'a>(
    http: &'a Http,
    category: &'static str,
    route: &'static Route,
    ids: Vec<String>,
    owner: Option<(i64, String)>,
) -> Events<'a> {
    stream::iter(ids)
        .map(move |id| {
            let owner = owner.clone();
            crate::sites::once(async move {
                let root = root(category);
                let url = http.api(&format!("{root}/view.php?id={id}"))?;
                let page = match http.text(&url).await {
                    Ok(page) => page,
                    Err(e) if missing(&e) => return Ok(crate::sites::events(Vec::new())),
                    Err(e) => return Err(e),
                };
                let urls = image_urls(http, root, &id, &page).await?;
                let items = parse_work(category, &page, &id, owner, urls);
                Ok(crate::sites::events(crate::sites::post(
                    site(category),
                    http,
                    route,
                    items,
                )))
            })
        })
        .flatten()
        .boxed()
}

fn missing(error: &crate::Error) -> bool {
    crate::sites::http::refused_status(error).is_some()
        || error.to_string().ends_with("resource not found")
}

/// Upstream `_extract_images`: the popup's images for multi-image works,
/// else the page's main image; protocol-relative as on the site.
async fn image_urls(http: &Http, root: &str, id: &str, page: &str) -> Result<Vec<String>> {
    if page.contains("&#diff_1\" ") {
        let url = http.api(&format!("{root}/view_popup.php?id={id}"))?;
        let popup = http.text(&url).await?;
        return Ok(extract_iter(&popup, "href=\"javascript:void(0);\"><", ">")
            .into_iter()
            .filter(|media| media.contains(" src=\""))
            .map(|media| extr(media, " src=\"", "\"").to_owned())
            .collect());
    }
    let pos = page.find("id=\"view-center\"").map_or(0, |i| i + 1);
    let url = extr(&page[pos..], "itemprop=\"image\" src=\"", "\"");
    Ok(if url.is_empty() {
        Vec::new()
    } else {
        vec![url.to_owned()]
    })
}

/// Upstream `_extract_data` (or `_extract_data_horne`) and `items` for one
/// work page and its image URLs.
pub(crate) fn parse_work(
    category: &str,
    page: &str,
    id: &str,
    owner: Option<(i64, String)>,
    urls: Vec<String>,
) -> Vec<Item> {
    let mut scan = super::Scan::new(page);
    let keywords: Vec<String> = unescape(scan.extr("name=\"keywords\" content=\"", "\" />"))
        .split(',')
        .map(str::to_owned)
        .collect();
    let title = keywords[0].trim().to_owned();
    let artist = keywords.get(1).cloned().unwrap_or_default();
    let tags: Vec<Value> = keywords
        .get(2..keywords.len().saturating_sub(1))
        .unwrap_or_default()
        .iter()
        .map(|tag| Value::from(tag.as_str()))
        .collect();
    let mut data = Map::new();
    data.insert("title".into(), title.clone().into());
    // Pages give Japan time (UTC+9).
    let jst = |timestamp: Option<i64>| timestamp.map(|t| t - 9 * 3600);
    let timestamp;
    if category == "horne" {
        let description = scan.extr("property=\"og:description\" content=\"", "\"");
        data.insert("description".into(), unescape(description).into());
        let artist_id = parse_int(scan.extr("members.php?id=", "\""), 0);
        data.insert("artist_id".into(), artist_id.into());
        data.insert("artist_name".into(), artist.clone().into());
        data.insert("tags".into(), tags.into());
        let date = scan.extr("itemprop='datePublished' content=", "<");
        timestamp = jst(super::parse_iso(
            date.rsplit('>').next().unwrap_or_default(),
        ));
        data.insert("date".into(), super::date(timestamp));
    } else {
        let description = scan.extr("\"description\": \"", "\"").replace("&amp;", "&");
        data.insert("description".into(), unescape(&description).into());
        let date = scan.extr("\"datePublished\": \"", "\"");
        timestamp = jst(super::strptime(date, "%a %b %d %H:%M:%S %Y"));
        data.insert("date".into(), super::date(timestamp));
        let artist_id = parse_int(scan.extr("/members.php?id=", "\""), 0);
        data.insert("artist_id".into(), artist_id.into());
        data.insert("artist_name".into(), artist.clone().into());
        data.insert("tags".into(), tags.into());
    }
    data.insert("image_id".into(), parse_int(id, 0).into());
    let (user_id, user_name) = match owner {
        Some((user_id, name)) => (Value::from(user_id), name),
        None => (data["artist_id"].clone(), artist),
    };
    data.insert("user_id".into(), user_id);
    data.insert("user_name".into(), user_name.clone().into());
    data.insert("count".into(), urls.len().into());
    let mut items = Vec::new();
    for (num, raw) in urls.into_iter().enumerate() {
        let full = format!("https:{raw}");
        let mut image = Map::new();
        image.insert("num".into(), num.into());
        image.insert("url".into(), full.clone().into());
        super::nameext_into(&full, &mut image);
        super::update(&mut image, &data);
        if image["extension"] == "" {
            image.insert("extension".into(), "jpg".into());
        }
        let Some(url) = Url::parse(&full)
            .ok()
            .filter(|u| raw.starts_with("//") && crate::sites::plain(u))
        else {
            continue;
        };
        items.push(
            Item::new(url, image)
                .ids(id, num.to_string())
                .user(Some(user_name.clone()))
                .caption(Some(title.clone()))
                .timestamp(timestamp),
        );
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, queued, response, serve};

    #[test]
    fn classifies_urls() {
        let t = |category: &str, raw: &str| target(category, &Url::parse(raw).unwrap());
        assert_eq!(
            t("nijie", "https://nijie.info/members.php?id=44"),
            Some(Target::User("44".into()))
        );
        assert_eq!(
            t("nijie", "https://www.nijie.info/members_illust.php?id=44"),
            Some(Target::Listing("members_illust", 44))
        );
        assert_eq!(
            t("horne", "https://horne.red/members_dojin.php?id=58000"),
            Some(Target::Listing("members_dojin", 58000))
        );
        assert_eq!(
            t(
                "nijie",
                "https://nijie.info/user_like_illust_view.php?id=44"
            ),
            Some(Target::Listing("user_like_illust_view", 44))
        );
        assert_eq!(
            t("nijie", "https://nijie.info/history_nuita.php?id=728995"),
            Some(Target::Listing("history_nuita", 728995))
        );
        assert_eq!(
            t("nijie", "https://nijie.info/like_user_view.php"),
            Some(Target::Listing("like_user_view", 0))
        );
        assert_eq!(
            t("horne", "https://horne.red/like_my.php"),
            Some(Target::Followed)
        );
        assert_eq!(
            t("nijie", "https://nijie.info/view_popup.php?id=70720"),
            Some(Target::Image("70720".into()))
        );
        assert_eq!(
            t("horne", "https://horne.red/view.php?id=8716"),
            Some(Target::Image("8716".into()))
        );
        for (category, raw) in [
            ("nijie", "https://horne.red/view.php?id=1"),
            ("horne", "https://nijie.info/view.php?id=1"),
            ("nijie", "https://nijie.info/view.php?x=1&id=1"),
            ("nijie", "https://nijie.info/members.php"),
            ("nijie", "https://nijie.info/members_illust.php?id=abc"),
            ("nijie", "https://nijie.info/"),
            ("nijie", "https://nijie.info.evil.com/view.php?id=1"),
            ("nijie", "https://evilnijie.info/view.php?id=1"),
            ("nijie", "https://pic.nijie.info/view.php?id=1"),
            ("nijie", "https://nijie.info:8443/view.php?id=1"),
            ("nijie", "https://u:p@nijie.info/view.php?id=1"),
        ] {
            assert_eq!(t(category, raw), None, "{raw}");
        }
    }

    const VIEW: &str = r#"<meta name="keywords" content="Title &amp; more ,Artist,tag1,tag2,ニジエ" />
<script>{"description": "Desc &amp;amp; text", "datePublished": "Thu Jan 02 12:04:05 2020"}</script>
<a href="/members.php?id=44">Artist</a>
<div id="view-center"><img itemprop="image" src="//pic.nijie.net/01/nijie/20/44/illust/0_0_abc.png" /></div>"#;

    fn session(category: &str, root: &Url) -> Http {
        let http = http(
            if category == "horne" {
                "Horne"
            } else {
                "Nijie"
            },
            root.clone(),
        );
        http.set_cookie(root, &format!("{category}_tok=tok; Path=/"));
        http
    }

    #[tokio::test]
    async fn extracts_single_image() {
        let (root, server) = serve(vec![html(VIEW)]);
        let http = session("nijie", &root);
        let files = media(extract(
            "nijie",
            &http,
            Url::parse("https://nijie.info/view.php?id=70720").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let k = keywords(&files[0]);
        assert_eq!(k["num"], 0);
        assert_eq!(k["title"], "Title & more");
        assert_eq!(k["description"], "Desc & text");
        assert_eq!(k["date"], "2020-01-02 03:04:05");
        assert_eq!(k["artist_id"], 44);
        assert_eq!(k["artist_name"], "Artist");
        assert_eq!(k["tags"], serde_json::json!(["tag1", "tag2"]));
        assert_eq!(k["user_id"], 44);
        assert_eq!(k["user_name"], "Artist");
        assert_eq!(k["image_id"], 70720);
        assert_eq!(k["filename"], "0_0_abc");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "70720_p0.png");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("nijie/44")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{image_id}_{num}",["70720","0"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /view.php?id=70720 "));
        assert!(requests[0].contains("nijie_tok=tok"));
    }

    #[tokio::test]
    async fn bookmarks_use_the_listed_user_and_skip_missing_works() {
        let list = r#"<p>Owner<br />Owner &amp; Co</p><div illust_id="1"></div><div illust_id="2"></div>
            <a rel="next" href="?p=2">"#;
        let multi = VIEW.replace("<div id=\"view-center\">", "<a href=\"#&#diff_1\" >");
        let popup = r#"<a href="javascript:void(0);"><img src="//pic.nijie.net/a.jpg"></a>
            <a href="javascript:void(0);"><img src="//pic.nijie.net/b"></a>
            <a href="javascript:void(0);"><span></span></a>"#;
        let (root, server) = serve(vec![
            html(list),
            html(&multi),
            html(popup),
            response("404 Not Found", "text/html", b""),
            html("<p>Owner<br />Owner</p>"),
        ]);
        let http = session("nijie", &root);
        let files = media(extract(
            "nijie",
            &http,
            Url::parse("https://nijie.info/user_like_illust_view.php?id=9").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[1]);
        assert_eq!(k["subcategory"], "favorite");
        assert_eq!(k["user_id"], 9);
        assert_eq!(k["user_name"], "Owner & Co");
        assert_eq!(k["count"], 2);
        assert_eq!(k["num"], 1);
        assert_eq!(k["extension"], "jpg");
        let (_, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("nijie/bookmarks/9")
        );
        let (_, identity) = crate::sites::archive_key(&files[1]).unwrap();
        assert_eq!(
            identity,
            r#"["f_{user_id}_{image_id}_{num}",["9","1","1"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /user_like_illust_view.php?id=9&p=1 "));
        assert!(requests[1].starts_with("GET /view.php?id=1 "));
        assert!(requests[2].starts_with("GET /view_popup.php?id=1 "));
        assert!(requests[3].starts_with("GET /view.php?id=2 "));
        assert!(requests[4].starts_with("GET /user_like_illust_view.php?id=9&p=2 "));
    }

    #[tokio::test]
    async fn horne_pages_and_missing_sessions() {
        let page = r#"<meta name="keywords" content="Work,Maker,a,b" />
            <meta property="og:description" content="About &amp; this">
            <a href="https://horne.red/members.php?id=58000">Maker</a>
            <span itemprop='datePublished' content=x>2020-01-02 12:04:05</span>
            <img itemprop="image" src="//pic.horne.red/x/y.gif" />"#;
        let (root, server) = serve(vec![html(page)]);
        let http = session("horne", &root);
        let files = media(extract(
            "horne",
            &http,
            Url::parse("https://horne.red/view.php?id=8716").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let k = keywords(&files[0]);
        assert_eq!(k["category"], "horne");
        assert_eq!(k["description"], "About & this");
        assert_eq!(k["artist_id"], 58000);
        assert_eq!(k["tags"], serde_json::json!(["a"]));
        assert_eq!(k["date"], "2020-01-02 03:04:05");
        let position = |key: &str| k.keys().position(|k| k == key);
        assert!(
            position("date") > position("tags"),
            "Horne lists the date last"
        );
        server.join().unwrap();

        let anonymous = crate::sites::testing::http("Nijie", root);
        assert!(
            crate::sites::testing::fails(extract(
                "nijie",
                &anonymous,
                Url::parse("https://nijie.info/view.php?id=1").unwrap(),
            ))
            .await
        );
    }

    #[tokio::test]
    async fn profiles_and_followed_users_are_queued() {
        let http = crate::sites::testing::http("Nijie", Url::parse("http://127.0.0.1:9/").unwrap());
        let children = queued(extract(
            "nijie",
            &http,
            Url::parse("https://nijie.info/members.php?id=44").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].subcategory(), "user");

        let page = r#"<a href="/members.php?id=6"><img></a><p class="name"><a href="/members.php?id=5">x</a>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = session("nijie", &root);
        let children = queued(extract(
            "nijie",
            &http,
            Url::parse("https://nijie.info/like_my.php").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].subcategory(), "followed");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /like_my.php?p=1 "));
    }
}
