//! Archive of Our Own works, series, tags, searches, users, bookmarks and
//! subscriptions, mirroring gallery_dl/extractor/ao3.py. Works download in
//! the configured `formats` (PDF by default).
use crate::error;
use crate::sites::gdl::{
    Scan, auth_info, captures, code_to_language, date, dispatch, extr, extract_iter, group,
    nameext_into, parse_int, parse_iso, partition, re, remove_html, rpartition, split_html,
    unescape,
};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value, json};
use std::time::Duration;
use url::Url;

const ROOT: &str = "https://archiveofourown.org";
const BASE: &str = r"^https?://(?:www\.)?a(?:rchiveofourown|o3)\.(?:org|com|net)";

static WORK: Route = Route::new(
    "work",
    "{id} {title}.{extension}",
    &["{category}", "{author}"],
    "{id}.{extension}",
);
macro_rules! listing_routes {
    ($($ident:ident $sub:literal),*) => { $(static $ident: Route = Route::new($sub, "{filename}.{extension}", &["{category}"], "");)* };
}
listing_routes!(SERIES "series", TAG "tag", SEARCH "search", USER "user", USER_WORKS "user-works", USER_SERIES "user-series", USER_BOOKMARK "user-bookmark", SUBSCRIPTIONS "subscriptions");

pub static SITE: Site = Site {
    category: "ao3",
    name: "Archive of Our Own",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[
        &WORK,
        &SERIES,
        &TAG,
        &SEARCH,
        &USER,
        &USER_WORKS,
        &USER_SERIES,
        &USER_BOOKMARK,
        &SUBSCRIPTIONS,
    ],
    api_key: false,
    cookies: Some("archiveofourown.org"),
};

#[derive(Debug, PartialEq)]
enum Target {
    Work(String),
    /// Listing route name and path.
    Works(&'static str, String),
    UserSeries(String),
    Bookmarks(String),
    Subscriptions(String),
    User(String),
}

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(re!(&format!(r"{BASE}/works/(\d+)")), url) {
        return Some(Target::Work(group(&c, 1).into()));
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}(/series/(\d+))")), url) {
        return Some(Target::Works("series", group(&c, 1).into()));
    }
    if let Some(c) = captures(
        re!(&format!(r"{BASE}(/tags/([^/?#]+)/works(?:/?\?.+)?)")),
        url,
    ) {
        return Some(Target::Works("tag", group(&c, 1).into()));
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}(/works/search/?\?.+)")), url) {
        return Some(Target::Works("search", group(&c, 1).into()));
    }
    if let Some(c) = captures(
        re!(&format!(
            r"{BASE}/users/([^/?#]+(?:/pseuds/[^/?#]+)?)(?:/profile)?/?(?:$|\?|#)"
        )),
        url,
    ) {
        return Some(Target::User(group(&c, 1).into()));
    }
    const USER: &str = r"(/users/([^/?#]+)/(?:pseuds/([^/?#]+)/)?";
    if let Some(c) = captures(re!(&format!(r"{BASE}{USER}works(?:/?\?.+)?)")), url) {
        return Some(Target::Works("user-works", group(&c, 1).into()));
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}{USER}series(?:/?\?.+)?)")), url) {
        return Some(Target::UserSeries(group(&c, 1).into()));
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}{USER}bookmarks(?:/?\?.+)?)")), url) {
        return Some(Target::Bookmarks(group(&c, 1).into()));
    }
    captures(
        re!(&format!(
            r"{BASE}(/users/([^/?#]+)/subscriptions(?:/?\?.+)?)"
        )),
        url,
    )
    .map(|c| Target::Subscriptions(group(&c, 1).into()))
}

fn route_of(name: &str) -> &'static Route {
    match name {
        "series" => &SERIES,
        "tag" => &TAG,
        "search" => &SEARCH,
        "user-works" => &USER_WORKS,
        "user-series" => &USER_SERIES,
        "user-bookmark" => &USER_BOOKMARK,
        _ => &SUBSCRIPTIONS,
    }
}

/// Upstream `request_interval = (0.5, 1.5)`.
async fn pace() {
    if !cfg!(test) {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// `login`: `remember_user_token`, else the login form.
async fn login(http: &Http) -> crate::Result<()> {
    let base = http.api(ROOT)?;
    if http.cookie(&base, "remember_user_token").is_some() {
        return Ok(());
    }
    let Some((username, password)) = auth_info(http) else {
        return Ok(());
    };
    let url = http.api(&format!("{ROOT}/users/login"))?;
    let page = http.text(&url).await?;
    let pos = page.find("id=\"loginform\"").unwrap_or(0);
    let token = unescape(extr(
        &page[pos..],
        " name=\"authenticity_token\" value=\"",
        "\"",
    ));
    let form = [
        ("authenticity_token", token.as_str()),
        ("user[login]", username.as_str()),
        ("user[password]", password.as_str()),
        ("user[remember_me]", "1"),
        ("commit", "Log In"),
    ];
    let redirected = http.post_form_or_location(&url, &form, &[]).await?.is_err();
    if !redirected || http.cookie(&base, "remember_user_token").is_none() {
        return Err(error("AO3 login failed; check the username and password"));
    }
    http.set_cookie(&base, "user_credentials=1; Path=/");
    Ok(())
}

/// `_pagination`: ids or paths after `needle`, following next links.
fn pagination<'a, F>(
    http: &'a Http,
    route: &'static Route,
    first: String,
    needle: &'static str,
    children: F,
) -> Events<'a>
where
    F: Fn(&str) -> Option<(Url, Map<String, Value>)> + Send + Sync + Copy + 'a,
{
    crate::sites::paginate(Some(first), move |state| async move {
        let Some(path) = state else { return Ok(None) };
        pace().await;
        let page = http.text(&http.api(&format!("{ROOT}{path}"))?).await?;
        let children: Vec<_> = extract_iter(&page, needle, "\"")
            .into_iter()
            .filter_map(children)
            .collect();
        let next = match extr(&page, "<a rel=\"next\" href=\"", "\"") {
            "" => extr(&page, "<li class=\"next\"><a href=\"", "\""),
            next => next,
        };
        let next = (!next.is_empty()).then(|| unescape(next));
        Ok(Some((
            crate::sites::galleries::queue(http, &SITE, route, children),
            next,
        )))
    })
}

fn kind(kind: &str) -> Map<String, Value> {
    let mut data = Map::new();
    data.insert("type".into(), kind.into());
    data
}

/// Queued works, series and users of a bookmark/subscription listing.
fn list_child(item: &str, part: bool) -> Option<(Url, Map<String, Value>)> {
    let path = if part { rpartition(item, "/").0 } else { item };
    let url = Url::parse(&format!("{ROOT}/{path}")).ok()?;
    let data = if item.starts_with("works/") {
        kind("work")
    } else if item.starts_with("series/") {
        kind("series")
    } else if item.starts_with("users/") {
        kind("user")
    } else {
        return None;
    };
    Some((url, data))
}

fn work_child(id: &str) -> Option<(Url, Map<String, Value>)> {
    Some((
        Url::parse(&format!("{ROOT}/works/{id}")).ok()?,
        kind("work"),
    ))
}

fn series_child(id: &str) -> Option<(Url, Map<String, Value>)> {
    Some((Url::parse(&format!("{ROOT}/series/{id}")).ok()?, Map::new()))
}

/// `Ao3WorkExtractor.items`.
async fn work(http: &Http, work_id: &str) -> crate::Result<Vec<Item>> {
    login(http).await?;
    let base = http.api(ROOT)?;
    http.set_cookie(&base, "view_adult=true; Path=/");
    let (page, final_url) = http
        .text_final(&http.api(&format!("{ROOT}/works/{work_id}"))?, &[])
        .await?;
    if final_url.as_str().ends_with("/users/login?restricted=true") {
        return Err(error("Login required to access member-only works"));
    }
    if page.len() < 20000 && page.contains("<h2 class=\"landmark heading\">Adult Content Warning</")
    {
        return Err(error("Adult Content"));
    }
    let mut scan = Scan::new(&page);
    let mut chapters = Map::new();
    for ch in extract_iter(
        scan.extr(" id=\"chapter_index\"", "</ul>"),
        " value=\"",
        "</option>",
    ) {
        let (cid, _, name) = partition(ch, "\">");
        chapters.insert(cid.into(), unescape(name).into());
    }
    let mut formats: Vec<(String, String)> = Vec::new();
    let mut path = String::new();
    for dl in extract_iter(scan.extr(" class=\"download\"", "</ul>"), " href=\"", "</") {
        let (link, _, kind) = rpartition(dl, "\">");
        path = link.to_owned();
        formats.push((kind.to_lowercase(), link.to_owned()));
    }
    let mut data = Map::new();
    data.insert("id".into(), parse_int(work_id).into());
    for (key, start) in [
        ("rating", "rating"),
        ("warnings", "warning"),
        ("categories", "category"),
        ("fandom", "fandom"),
        ("relationships", "relationship"),
        ("characters", "character"),
        ("tags", "freeform"),
    ] {
        data.insert(
            key.into(),
            split_html(scan.extr(&format!("<dd class=\"{start} tags\">"), "</dd>")).into(),
        );
    }
    let lang = scan.extr("<dd class=\"language\" lang=\"", "\"").to_owned();
    let series = scan.extr("<dd class=\"series\">", "</dd>").to_owned();
    data.insert(
        "date".into(),
        date(parse_iso(scan.extr("<dd class=\"published\">", "<"))),
    );
    data.insert(
        "date_completed".into(),
        date(parse_iso(
            scan.extr(">Completed:</dt><dd class=\"status\">", "<"),
        )),
    );
    data.insert(
        "date_updated".into(),
        date(rpartition(&path, "updated_at=").2.parse().ok()),
    );
    let count = |text: &str| parse_int(&text.replace(',', ""));
    data.insert(
        "words".into(),
        count(scan.extr("<dd class=\"words\">", "<")).into(),
    );
    data.insert("chapters".into(), chapters.into());
    data.insert(
        "comments".into(),
        count(scan.extr("<dd class=\"comments\">", "<")).into(),
    );
    data.insert(
        "likes".into(),
        count(scan.extr("<dd class=\"kudos\">", "<")).into(),
    );
    data.insert(
        "bookmarks".into(),
        count(&remove_html(scan.extr("<dd class=\"bookmarks\">", "</dd>"))).into(),
    );
    data.insert(
        "views".into(),
        count(scan.extr("<dd class=\"hits\">", "<")).into(),
    );
    data.insert(
        "title".into(),
        unescape(remove_html(scan.extr(" class=\"title heading\">", "</h2>")).trim()).into(),
    );
    data.insert(
        "author".into(),
        unescape(&remove_html(
            scan.extr(" class=\"byline heading\">", "</h3>"),
        ))
        .into(),
    );
    data.insert(
        "summary".into(),
        split_html(scan.extr(" class=\"heading\">Summary:</h3>", "</div>")).into(),
    );
    data.insert(
        "language".into(),
        code_to_language(&lang).map_or(Value::Null, Value::from),
    );
    data.insert("lang".into(), lang.into());
    let series = if series.is_empty() {
        Value::Null
    } else {
        let mut scan = Scan::new(&series);
        json!({
            "prev": scan.extr(" class=\"previous\" href=\"/works/", "\""),
            "index": scan.extr(" class=\"position\">Part ", " "),
            "id": scan.extr(" href=\"/series/", "\""),
            "name": unescape(scan.extr(">", "<")),
            "next": scan.extr(" class=\"next\" href=\"/works/", "\""),
        })
    };
    data.insert("series".into(), series);
    let wanted: Vec<String> = match http.option("formats") {
        None | Some(Value::Null) => vec!["pdf".into()],
        Some(Value::String(s)) if s.is_empty() => Vec::new(),
        Some(Value::String(s)) => s
            .to_lowercase()
            .replace(' ', "")
            .split(',')
            .map(str::to_owned)
            .collect(),
        Some(Value::Array(list)) => list.iter().map(crate::sites::scalar).collect(),
        Some(Value::Bool(false)) => Vec::new(),
        Some(other) => vec![crate::sites::scalar(other)],
    };
    let root = Url::parse(ROOT).map_err(|_| error("invalid AO3 root"))?;
    Ok(wanted
        .iter()
        .filter_map(|format| {
            let link = formats.iter().find(|(kind, _)| kind == format)?.1.as_str();
            let url = root.join(link).ok()?;
            let mut fields = data.clone();
            nameext_into(url.as_str(), &mut fields);
            Some(Item::new(url, fields).ids(work_id, format.clone()))
        })
        .collect())
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported AO3 URL"));
    };
    match target {
        Target::Work(id) => crate::sites::once(async move {
            Ok(crate::sites::events(crate::sites::post(
                &SITE,
                http,
                &WORK,
                work(http, &id).await?,
            )))
        }),
        Target::Works(name, path) => {
            let route = route_of(name);
            crate::sites::once(async move {
                login(http).await?;
                Ok(pagination(http, route, path, "<li id=\"work_", work_child))
            })
        }
        Target::UserSeries(path) => crate::sites::once(async move {
            login(http).await?;
            Ok(pagination(
                http,
                &USER_SERIES,
                path,
                "<li id=\"series_",
                series_child,
            ))
        }),
        Target::Bookmarks(path) => crate::sites::once(async move {
            login(http).await?;
            Ok(pagination(
                http,
                &USER_BOOKMARK,
                path,
                "<span class=\"count\"><a href=\"/",
                |item| list_child(item, true),
            ))
        }),
        Target::Subscriptions(path) => crate::sites::once(async move {
            login(http).await?;
            Ok(pagination(
                http,
                &SUBSCRIPTIONS,
                path,
                "<dt>\n<a href=\"/",
                |item| list_child(item, false),
            ))
        }),
        Target::User(user) => {
            let base = format!("{ROOT}/users/{user}/");
            let views = [
                ("user-works", format!("{base}works")),
                ("user-series", format!("{base}series")),
                ("user-bookmark", format!("{base}bookmarks")),
            ];
            match dispatch(http, &views, &["user-works", "user-series"]) {
                Ok(selected) => {
                    let children = selected
                        .into_iter()
                        .filter_map(|(_, url)| Some((Url::parse(url).ok()?, Map::new())))
                        .collect();
                    crate::sites::galleries::queue(http, &SITE, &USER, children)
                }
                Err(e) => crate::sites::fail(e),
            }
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
            t("https://archiveofourown.org/works/47802076"),
            Some(Target::Work("47802076".into()))
        );
        assert_eq!(
            t("https://ao3.org/series/1903030"),
            Some(Target::Works("series", "/series/1903030".into()))
        );
        assert_eq!(
            t("https://archiveofourown.org/works/search?work_search[query]=air"),
            Some(Target::Works(
                "search",
                "/works/search?work_search[query]=air".into()
            ))
        );
        assert_eq!(
            t("https://archiveofourown.org/users/mjolnir/pseuds/x"),
            Some(Target::User("mjolnir/pseuds/x".into()))
        );
        assert_eq!(
            t("https://archiveofourown.org/users/mjolnir/works"),
            Some(Target::Works("user-works", "/users/mjolnir/works".into()))
        );
        assert_eq!(
            t("https://archiveofourown.org/users/mjolnir/bookmarks"),
            Some(Target::Bookmarks("/users/mjolnir/bookmarks".into()))
        );
    }

    #[tokio::test]
    async fn work_downloads_pdf() {
        let page = r#"<ul id="chapter_index"><option value="1">1. Start</option></ul>
            <li class="download"><ul><li><a href="/downloads/47802076/Test.epub?updated_at=1700000000">EPUB</a></li><li><a href="/downloads/47802076/Test.pdf?updated_at=1700000000">PDF</a></li></ul>
            <dd class="rating tags"><a>General</a></dd><dd class="fandom tags"><a>Fan</a></dd><dd class="language" lang="en">English</dd>
            <dd class="published">2023-05-01</dd><dd class="words">1,234</dd><dd class="kudos">5</dd>
            <h2 class="title heading"> Test &amp; More </h2><h3 class="byline heading"><a>author</a></h3>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = client("ao3", root);
        let files = media(extract(
            &http,
            Url::parse("https://archiveofourown.org/works/47802076").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].url.as_str(),
            "https://archiveofourown.org/downloads/47802076/Test.pdf?updated_at=1700000000"
        );
        let k = keywords(&files[0]);
        assert_eq!(
            (
                k["title"].as_str(),
                k["words"].as_i64(),
                k["language"].as_str()
            ),
            (Some("Test & More"), Some(1234), Some("English"))
        );
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "47802076 Test & More.pdf");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("ao3/author")
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn listing_queues_works() {
        let page = r#"<li id="work_1" class="work"></li><li id="work_2" class="work"></li>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = client("ao3", root);
        assert_eq!(
            children(extract(
                &http,
                Url::parse("https://archiveofourown.org/tags/Fluff/works").unwrap()
            ))
            .await
            .len(),
            2
        );
        server.join().unwrap();
    }
}
