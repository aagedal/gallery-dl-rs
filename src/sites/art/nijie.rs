//! Nijie and Horne works, users, favorites, nuita lists, feeds and follows,
//! mirroring gallery_dl/extractor/nijie.py. Both sites need a login.
use crate::error;
use crate::sites::gdl::{
    Scan, auth_info, date, dispatch, extr, extract_iter, nameext_into, parse_int, parse_iso, re,
    rpartition, strptime, unescape,
};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use std::time::Duration;
use url::Url;

const FILENAME: &str = "{image_id}_p{num}.{extension}";
const ARCHIVE: &str = "{image_id}_{num}";

static IMAGE: Route = Route::new("image", FILENAME, &["{category}", "{user_id}"], ARCHIVE);
static USER: Route = Route::new("user", FILENAME, &["{category}", "{user_id}"], ARCHIVE);
static ILLUSTRATION: Route = Route::new(
    "illustration",
    FILENAME,
    &["{category}", "{user_id}"],
    ARCHIVE,
);
static DOUJIN: Route = Route::new("doujin", FILENAME, &["{category}", "{user_id}"], ARCHIVE);
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
static FEED: Route = Route::new("feed", FILENAME, &["{category}", "{user_id}"], ARCHIVE);
static FOLLOWED: Route = Route::new("followed", FILENAME, &["{category}", "{user_id}"], ARCHIVE);
const ROUTES: &[&Route] = &[
    &IMAGE,
    &USER,
    &ILLUSTRATION,
    &DOUJIN,
    &FAVORITE,
    &NUITA,
    &FEED,
    &FOLLOWED,
];

struct Instance {
    category: &'static str,
    root: &'static str,
    domain: &'static str,
}

macro_rules! instance {
    ($site:ident, $inst:ident, $category:literal, $name:literal, $root:literal, $domain:literal) => {
        static $inst: Instance = Instance {
            category: $category,
            root: $root,
            domain: $domain,
        };
        pub static $site: Site = Site {
            category: $category,
            name: $name,
            matches: |url| target(&$inst, url).is_some(),
            extract: |http, url| extract(&$inst, &$site, http, url),
            routes: ROUTES,
            api_key: false,
            cookies: Some($domain),
        };
    };
}
instance!(
    NIJIE,
    NIJIE_I,
    "nijie",
    "Nijie",
    "https://nijie.info",
    "nijie.info"
);
instance!(
    HORNE,
    HORNE_I,
    "horne",
    "Horne",
    "https://horne.red",
    "horne.red"
);

#[derive(Debug, PartialEq)]
enum Target {
    Image(String),
    User(String),
    /// Listing page, user id and route.
    Listing(&'static str, String),
    Feed,
    Followed,
}

fn target(inst: &Instance, url: &Url) -> Option<Target> {
    let raw = url.as_str();
    let rest = raw
        .strip_prefix("https://")
        .or_else(|| raw.strip_prefix("http://"))?;
    let rest = rest
        .strip_prefix("www.")
        .unwrap_or(rest)
        .strip_prefix(inst.domain)?;
    if let Some(c) = re!(r"^/view(?:_popup)?\.php\?id=(\d+)").captures(rest) {
        return Some(Target::Image(c[1].to_owned()));
    }
    if let Some(c) = re!(r"^/members\.php\?id=(\d+)").captures(rest) {
        return Some(Target::User(c[1].to_owned()));
    }
    if let Some(c) =
        re!(r"^/(members_illust|members_dojin|user_like_illust_view|history_nuita)\.php\?id=(\d+)")
            .captures(rest)
    {
        let page = match &c[1] {
            "members_illust" => "members_illust",
            "members_dojin" => "members_dojin",
            "user_like_illust_view" => "user_like_illust_view",
            _ => "history_nuita",
        };
        return Some(Target::Listing(page, c[2].to_owned()));
    }
    if rest.starts_with("/like_user_view.php") {
        return Some(Target::Feed);
    }
    rest.starts_with("/like_my.php").then_some(Target::Followed)
}

fn route_of(page: &str) -> &'static Route {
    match page {
        "members_illust" => &ILLUSTRATION,
        "members_dojin" => &DOUJIN,
        "user_like_illust_view" => &FAVORITE,
        "history_nuita" => &NUITA,
        _ => &FEED,
    }
}

/// Upstream `request_interval = (2.0, 4.0)`.
async fn pace() {
    if !cfg!(test) {
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

/// `login`: the `{category}_tok` cookie, else the login form.
async fn login(inst: &Instance, http: &Http) -> crate::Result<()> {
    let base = http.api(inst.root)?;
    if http
        .cookie(&base, &format!("{}_tok", inst.category))
        .is_some()
    {
        return Ok(());
    }
    let Some((username, password)) = auth_info(http) else {
        return Err(error("Username and password required"));
    };
    let url = http.api(&format!("{}/login_int.php", inst.root))?;
    let form = [
        ("email", username.as_str()),
        ("password", password.as_str()),
        ("save", "on"),
    ];
    match http.post_form_or_location(&url, &form, &[]).await? {
        Ok(page) if page.contains("/login.php") => {
            Err(error("Nijie login failed; check the username and password"))
        }
        _ => Ok(()),
    }
}

/// `_extract_data` (and the Horne variant).
fn metadata(inst: &Instance, page: &str) -> Map<String, Value> {
    let mut scan = Scan::new(page);
    let keywords: Vec<String> = unescape(scan.extr("name=\"keywords\" content=\"", "\" />"))
        .split(',')
        .map(str::to_owned)
        .collect();
    let mut data = Map::new();
    data.insert(
        "title".into(),
        keywords.first().map_or("", |k| k.trim()).into(),
    );
    let nine_hours = 9 * 3600;
    if inst.category == "horne" {
        data.insert(
            "description".into(),
            unescape(scan.extr("property=\"og:description\" content=\"", "\"")).into(),
        );
        data.insert(
            "artist_id".into(),
            parse_int(scan.extr("members.php?id=", "\"")).into(),
        );
        let published = rpartition(scan.extr("itemprop='datePublished' content=", "<"), ">")
            .2
            .to_owned();
        data.insert(
            "date".into(),
            date(parse_iso(&published).map(|ts| ts - nine_hours)),
        );
    } else {
        data.insert(
            "description".into(),
            unescape(&scan.extr("\"description\": \"", "\"").replace("&amp;", "&")).into(),
        );
        data.insert(
            "date".into(),
            date(
                strptime(
                    scan.extr("\"datePublished\": \"", "\""),
                    "%a %b %d %H:%M:%S %Y",
                )
                .map(|ts| ts - nine_hours),
            ),
        );
        data.insert(
            "artist_id".into(),
            parse_int(scan.extr("/members.php?id=", "\"")).into(),
        );
    }
    data.insert(
        "artist_name".into(),
        keywords.get(1).cloned().unwrap_or_default().into(),
    );
    let tags: Vec<Value> = keywords
        .get(2..keywords.len().saturating_sub(1))
        .unwrap_or_default()
        .iter()
        .map(|t| Value::from(t.as_str()))
        .collect();
    data.insert("tags".into(), tags.into());
    data
}

/// `_extract_images`: popup images for multi-image works.
async fn images(
    inst: &Instance,
    http: &Http,
    image_id: &str,
    page: &str,
) -> crate::Result<Vec<String>> {
    if page.contains("&#diff_1\" ") {
        pace().await;
        let popup = http
            .text(&http.api(&format!("{}/view_popup.php?id={image_id}", inst.root))?)
            .await?;
        return Ok(extract_iter(&popup, "href=\"javascript:void(0);\"><", ">")
            .into_iter()
            .filter(|media| media.contains(" src=\""))
            .map(|media| extr(media, " src=\"", "\"").to_owned())
            .collect());
    }
    let pos = page.find("id=\"view-center\"").map_or(0, |p| p + 1);
    Ok(vec![
        extr(&page[pos..], "itemprop=\"image\" src=\"", "\"").to_owned(),
    ])
}

/// One work's files; `user` overrides the artist as `user_id`/`user_name`.
async fn work_items(
    inst: &Instance,
    http: &Http,
    image_id: &str,
    user: Option<(i64, String)>,
) -> crate::Result<Vec<Item>> {
    pace().await;
    let Ok((status, page)) = http
        .get_status(
            &http.api(&format!("{}/view.php?id={image_id}", inst.root))?,
            &[],
        )
        .await
    else {
        return Ok(Vec::new());
    };
    if status >= 400 {
        return Ok(Vec::new());
    }
    let mut data = metadata(inst, &page);
    data.insert("image_id".into(), parse_int(image_id).into());
    match user {
        Some((id, name)) => {
            data.insert("user_id".into(), id.into());
            data.insert("user_name".into(), name.into());
        }
        None => {
            data.insert("user_id".into(), data["artist_id"].clone());
            data.insert("user_name".into(), data["artist_name"].clone());
        }
    }
    let urls = images(inst, http, image_id, &page).await?;
    data.insert("count".into(), urls.len().into());
    Ok(urls
        .into_iter()
        .enumerate()
        .filter_map(|(num, path)| {
            let url = format!("https:{path}");
            let mut image = Map::new();
            nameext_into(&path, &mut image);
            image.insert("num".into(), num.into());
            image.insert("url".into(), url.as_str().into());
            image.extend(data.clone());
            if image
                .get("extension")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .is_empty()
            {
                image.insert("extension".into(), "jpg".into());
            }
            Some(Item::new(Url::parse(&url).ok()?, image).ids(image_id, num.to_string()))
        })
        .collect())
}

/// `_pagination`: work ids of a listing page, with the user name.
fn listing<'a>(
    inst: &'static Instance,
    site: &'static Site,
    http: &'a Http,
    page_name: &'static str,
    user_id: String,
) -> Events<'a> {
    crate::sites::paginate(Some((1u64, None::<String>)), move |state| {
        let user_id = user_id.clone();
        async move {
            let Some((number, user_name)) = state else {
                return Ok(None);
            };
            if number == 1 {
                login(inst, http).await?;
            }
            pace().await;
            let mut url = http.api(&format!("{}/{page_name}.php", inst.root))?;
            {
                let mut pairs = url.query_pairs_mut();
                if !user_id.is_empty() {
                    pairs.append_pair("id", &user_id);
                }
                pairs.append_pair("p", &number.to_string());
            }
            let page = http.text(&url).await?;
            let user_name = user_name.unwrap_or_else(|| match page_name {
                "history_nuita" => unescape(extr(&page, "<title>", "さんの抜いた")),
                "like_user_view" => String::new(),
                _ => unescape(extr(&page, "<br />", "<")),
            });
            let ids: Vec<String> = extract_iter(&page, "illust_id=\"", "\"")
                .into_iter()
                .map(str::to_owned)
                .collect();
            let next = page
                .contains("<a rel=\"next\"")
                .then_some((number + 1, Some(user_name.clone())));
            let user = (!user_name.is_empty()).then(|| (parse_int(&user_id), user_name));
            let route = route_of(page_name);
            let events = crate::sites::each(ids, move |id| {
                let user = user.clone();
                async move {
                    Ok(crate::sites::post(
                        site,
                        http,
                        route,
                        work_items(inst, http, &id, user).await?,
                    ))
                }
            });
            Ok(Some((events, next)))
        }
    })
}

fn extract<'a>(
    inst: &'static Instance,
    site: &'static Site,
    http: &'a Http,
    url: Url,
) -> Events<'a> {
    let Some(target) = target(inst, &url) else {
        return crate::sites::fail(error("unsupported Nijie URL"));
    };
    match target {
        Target::Image(id) => crate::sites::once(async move {
            login(inst, http).await?;
            Ok(crate::sites::events(crate::sites::post(
                site,
                http,
                &IMAGE,
                work_items(inst, http, &id, None).await?,
            )))
        }),
        Target::Listing(page, user_id) => listing(inst, site, http, page, user_id),
        Target::Feed => listing(inst, site, http, "like_user_view", String::new()),
        Target::User(user_id) => {
            let views: Vec<(&str, String)> = [
                ("illustration", "members_illust"),
                ("doujin", "members_dojin"),
                ("favorite", "user_like_illust_view"),
                ("nuita", "history_nuita"),
            ]
            .into_iter()
            .map(|(view, page)| (view, format!("{}/{page}.php?id={user_id}", inst.root)))
            .collect();
            match dispatch(http, &views, &["illustration", "doujin"]) {
                Ok(selected) => {
                    let children = selected
                        .into_iter()
                        .filter_map(|(_, url)| Some((Url::parse(url).ok()?, Map::new())))
                        .collect();
                    crate::sites::galleries::queue(http, site, &USER, children)
                }
                Err(e) => crate::sites::fail(e),
            }
        }
        Target::Followed => crate::sites::paginate(Some(1u64), move |state| async move {
            let Some(number) = state else { return Ok(None) };
            if number == 1 {
                login(inst, http).await?;
            }
            pace().await;
            let page = http
                .text(&http.api(&format!("{}/like_my.php?p={number}", inst.root))?)
                .await?;
            let children = extract_iter(&page, "\"><a href=\"/members.php?id=", "\"")
                .into_iter()
                .filter_map(|id| {
                    Some((
                        Url::parse(&format!("{}/members.php?id={id}", inst.root)).ok()?,
                        Map::new(),
                    ))
                })
                .collect();
            let next = page.contains("<a rel=\"next\"").then_some(number + 1);
            Ok(Some((
                crate::sites::galleries::queue(http, site, &FOLLOWED, children),
                next,
            )))
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{children, html, http as client, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&NIJIE_I, &Url::parse(raw).unwrap());
        assert_eq!(
            t("https://nijie.info/view.php?id=70720"),
            Some(Target::Image("70720".into()))
        );
        assert_eq!(
            t("https://www.nijie.info/view_popup.php?id=70720"),
            Some(Target::Image("70720".into()))
        );
        assert_eq!(
            t("https://nijie.info/members.php?id=44"),
            Some(Target::User("44".into()))
        );
        assert_eq!(
            t("https://nijie.info/history_nuita.php?id=728995"),
            Some(Target::Listing("history_nuita", "728995".into()))
        );
        assert_eq!(t("https://nijie.info/like_my.php"), Some(Target::Followed));
        assert!(
            target(
                &HORNE_I,
                &Url::parse("https://horne.red/view.php?id=8716").unwrap()
            )
            .is_some()
        );
    }

    #[tokio::test]
    async fn single_work_with_login_cookie() {
        let page = r#"<meta name="keywords" content="Title,Artist,tag1,tag2,ニジエ" />
            "description": "desc &amp;amp; more", "datePublished": "Sat Aug 06 16:20:26 2016", <a href="/members.php?id=44">
            <div id="view-center"><img itemprop="image" src="//pic.nijie.net/07/nijie/16/44/illust/0_0_403fdd541191110c_c25585.jpg">"#;
        let (root, server) = serve(vec![html(page)]);
        let http = client("nijie", root);
        http.set_cookie(
            &http.api("https://nijie.info").unwrap(),
            "nijie_tok=x; Path=/",
        );
        let files = media(extract(
            &NIJIE_I,
            &NIJIE,
            &http,
            Url::parse("https://nijie.info/view.php?id=70720").unwrap(),
        ))
        .await;
        assert_eq!(
            files[0].url.as_str(),
            "https://pic.nijie.net/07/nijie/16/44/illust/0_0_403fdd541191110c_c25585.jpg"
        );
        let k = keywords(&files[0]);
        assert_eq!(
            (
                k["user_name"].as_str(),
                k["artist_id"].as_i64(),
                k["date"].as_str()
            ),
            (Some("Artist"), Some(44), Some("2016-08-06 07:20:26"))
        );
        assert_eq!(k["tags"], serde_json::json!(["tag1", "tag2"]));
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "70720_p0.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("nijie/44")
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn user_dispatches_default_views() {
        let (root, server) = serve(vec![]);
        let http = client("nijie", root);
        assert_eq!(
            children(extract(
                &NIJIE_I,
                &NIJIE,
                &http,
                Url::parse("https://nijie.info/members.php?id=44").unwrap()
            ))
            .await
            .len(),
            2
        );
        server.join().unwrap();
    }
}
