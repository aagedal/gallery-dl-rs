//! Archive of Our Own works, series, tags, searches, user profiles,
//! bookmarks and subscriptions, mirroring gallery_dl/extractor/ao3.py:
//! listings queue works and series, and works download their `formats`.
//!
//! Upstream's `username`/`password` login is not supported; a
//! `remember_user_token` cookie from `--cookies` is sent instead. Upstream's
//! `categorytransfer` (queued works reporting the listing's subcategory) and
//! `request_interval` are not applied.
use super::{Scan, digits, extr, extract_iter, leading_digits, on_host, parse_int, split_html};
use crate::sites::{Events, Http, Item, Route, Site, remove_html, unescape};
use crate::{Result, error};
use futures_util::{StreamExt, stream};
use serde_json::{Map, Value, json};
use url::Url;

const ROOT: &str = "https://archiveofourown.org";
const HOSTS: &[&str] = &[
    "archiveofourown.org",
    "archiveofourown.com",
    "archiveofourown.net",
    "ao3.org",
    "ao3.com",
    "ao3.net",
];

pub static SITE: Site = Site {
    category: "ao3",
    name: "AO3",
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

static WORK: Route = Route::new(
    "work",
    "{id} {title}.{extension}",
    &["{category}", "{author}"],
    "{id}.{extension}",
);
// The listing extractors only queue works, series and users.
const FILENAME: &str = "{filename}.{extension}";
const DIRECTORY: &[&str] = &["{category}"];
static SERIES: Route = Route::new("series", FILENAME, DIRECTORY, "");
static TAG: Route = Route::new("tag", FILENAME, DIRECTORY, "");
static SEARCH: Route = Route::new("search", FILENAME, DIRECTORY, "");
static USER: Route = Route::new("user", FILENAME, DIRECTORY, "");
static USER_WORKS: Route = Route::new("user-works", FILENAME, DIRECTORY, "");
static USER_SERIES: Route = Route::new("user-series", FILENAME, DIRECTORY, "");
static USER_BOOKMARK: Route = Route::new("user-bookmark", FILENAME, DIRECTORY, "");
static SUBSCRIPTIONS: Route = Route::new("subscriptions", FILENAME, DIRECTORY, "");

#[derive(Debug, PartialEq)]
enum Target {
    Work(String),
    /// A listing's subcategory and its path with the query, as upstream's
    /// first group.
    List(&'static str, String),
    /// `USER` or `USER/pseuds/PSEUD`.
    User(String),
}

/// `PREFIX(?:/?\?.+)?`: the prefix, plus the query when only an optional
/// slash separates them.
fn with_query(url: &Url, prefix: &str) -> String {
    let rest = &url.path()[prefix.len()..];
    match url.query() {
        Some(query) if !query.is_empty() && matches!(rest, "" | "/") => {
            format!("{prefix}{rest}?{query}")
        }
        _ => prefix.to_owned(),
    }
}

fn target(url: &Url) -> Option<Target> {
    if !on_host(url, HOSTS) {
        return None;
    }
    let path = url.path();
    let query = url.query().filter(|q| !q.is_empty());
    if let Some(id) = path.strip_prefix("/works/").and_then(leading_digits) {
        return Some(Target::Work(id.to_owned()));
    }
    if let Some(id) = path.strip_prefix("/series/").and_then(leading_digits) {
        return Some(Target::List("series", format!("/series/{id}")));
    }
    if let Some(rest) = path.strip_prefix("/tags/") {
        let tag = rest.split('/').next().filter(|t| !t.is_empty())?;
        let prefix = format!("/tags/{tag}/works");
        return path
            .starts_with(&prefix)
            .then(|| Target::List("tag", with_query(url, &prefix)));
    }
    if matches!(path, "/works/search" | "/works/search/") {
        let query = query?;
        return Some(Target::List("search", format!("{path}?{query}")));
    }
    let rest = path.strip_prefix("/users/")?;
    // `([^/?#]+(?:/pseuds/[^/?#]+)?)(?:/profile)?/?(?:$|\?|#)`
    let profile = |rest: &str| -> Option<String> {
        match rest.split('/').collect::<Vec<_>>().as_slice() {
            [user] if !user.is_empty() => Some((*user).to_owned()),
            [user, "pseuds", pseud] if !user.is_empty() && !pseud.is_empty() => {
                Some(format!("{user}/pseuds/{pseud}"))
            }
            _ => None,
        }
    };
    let trimmed = rest.strip_suffix('/').unwrap_or(rest);
    if let Some(user) =
        profile(trimmed).or_else(|| trimmed.strip_suffix("/profile").and_then(profile))
    {
        return Some(Target::User(user));
    }
    let parts: Vec<&str> = rest.split('/').collect();
    let (user, kind_at) = match parts.as_slice() {
        [user, "pseuds", pseud, kind, ..]
            if !pseud.is_empty() && matches!(*kind, "works" | "series" | "bookmarks") =>
        {
            (*user, 3)
        }
        [user, ..] => (*user, 1),
        [] => return None,
    };
    if user.is_empty() {
        return None;
    }
    let subcategory = match parts.get(kind_at).copied()? {
        "works" => "user-works",
        "series" => "user-series",
        "bookmarks" => "user-bookmark",
        "subscriptions" if kind_at == 1 => "subscriptions",
        _ => return None,
    };
    let prefix = format!("/users/{}", parts[..=kind_at].join("/"));
    Some(Target::List(subcategory, with_query(url, &prefix)))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported AO3 URL"));
    };
    match target {
        Target::Work(id) => crate::sites::once(async move {
            let items = work(http, &id).await?;
            Ok(crate::sites::events(crate::sites::post(
                &SITE, http, &WORK, items,
            )))
        }),
        Target::User(user) => dispatch(http, &user),
        Target::List(subcategory, path) => match subcategory {
            "user-series" => pagination(http, subcategory, path, "<li id=\"series_", |id| {
                digits(id).then(|| (format!("{ROOT}/series/{id}"), Map::new()))
            }),
            "user-bookmark" => pagination(
                http,
                subcategory,
                path,
                "<span class=\"count\"><a href=\"/",
                |item| listed(item.rsplit_once('/').map_or("", |(path, _)| path), item),
            ),
            "subscriptions" => pagination(http, subcategory, path, "<dt>\n<a href=\"/", |item| {
                listed(item, item)
            }),
            _ => pagination(http, subcategory, path, "<li id=\"work_", |id| {
                digits(id).then(|| (format!("{ROOT}/works/{id}"), kind("work")))
            }),
        },
    }
}

/// A queued URL and its keywords.
type Child = (String, Map<String, Value>);

fn kind(name: &str) -> Map<String, Value> {
    let mut data = Map::new();
    data.insert("type".into(), name.into());
    data
}

/// Upstream `items_list`: a bookmarked or subscribed work, series or user.
fn listed(path: &str, item: &str) -> Option<Child> {
    let name = [
        ("works/", "work"),
        ("series/", "series"),
        ("users/", "user"),
    ]
    .into_iter()
    .find_map(|(prefix, name)| item.starts_with(prefix).then_some(name))?;
    Some((format!("{ROOT}/{path}"), kind(name)))
}

/// Upstream `Dispatch` for profiles: queue the `include` extractors.
fn dispatch<'a>(http: &'a Http, user: &str) -> Events<'a> {
    let base = format!("{ROOT}/users/{user}/");
    let children: Vec<Url> = super::include(
        http,
        &["user-works", "user-series", "user-bookmark"],
        &["user-works", "user-series"],
    )
    .into_iter()
    .filter_map(|sub| {
        let page = match sub {
            "user-works" => "works",
            "user-series" => "series",
            _ => "bookmarks",
        };
        Url::parse(&format!("{base}{page}")).ok()
    })
    .collect();
    stream::iter(children)
        .map(move |child| crate::sites::queue(http, "ao3", "user", child, &Map::new()))
        .flatten()
        .boxed()
}

/// Upstream `_pagination`: queue the child of each `needle` item, following
/// `rel="next"` links.
fn pagination<'a>(
    http: &'a Http,
    subcategory: &'static str,
    path: String,
    needle: &'static str,
    child: fn(&str) -> Option<Child>,
) -> Events<'a> {
    crate::sites::paginate(Some(path), move |path| async move {
        let Some(path) = path else {
            return Ok(None);
        };
        let page = http.text(&http.api(&format!("{ROOT}{path}"))?).await?;
        let children: Vec<(Url, Map<String, Value>)> = extract_iter(&page, needle, "\"")
            .into_iter()
            .filter_map(child)
            .filter_map(|(url, data)| Some((Url::parse(&url).ok()?, data)))
            .filter(|(url, _)| target(url).is_some())
            .collect();
        let next = match extr(&page, "<a rel=\"next\" href=\"", "\"") {
            "" => extr(&page, "<li class=\"next\"><a href=\"", "\""),
            next => next,
        };
        let next = unescape(next);
        let next = (next.starts_with('/') && !next.starts_with("//")).then_some(next);
        let events = stream::iter(children)
            .map(move |(url, data)| crate::sites::queue(http, "ao3", subcategory, url, &data))
            .flatten()
            .boxed();
        Ok(Some((events, next)))
    })
}

/// Upstream `formats`: PDF unless configured.
fn formats(http: &Http) -> Vec<String> {
    match http.option("formats") {
        None => vec!["pdf".into()],
        Some(Value::String(s)) if !s.is_empty() => s
            .to_lowercase()
            .replace(' ', "")
            .split(',')
            .map(str::to_owned)
            .collect(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        Some(_) => Vec::new(),
    }
}

async fn work(http: &Http, id: &str) -> Result<Vec<Item>> {
    let root = http.api(ROOT)?;
    http.set_cookie(&root, "view_adult=true; Path=/");
    let page = http.text(&http.api(&format!("{ROOT}/works/{id}"))?).await?;
    // Upstream recognizes the redirect to `/users/login?restricted=true`.
    if page.contains("id=\"loginform\"") && !page.contains(" class=\"download\"") {
        return Err(error("AO3 login required to access member-only works"));
    }
    if page.len() < 20000 && page.contains("<h2 class=\"landmark heading\">Adult Content Warning</")
    {
        return Err(error("AO3 work is behind an Adult Content warning"));
    }
    Ok(parse_work(&page, id, &formats(http)))
}

/// Upstream `Ao3WorkExtractor.items`: the work's metadata and one file per
/// available format.
pub(crate) fn parse_work(page: &str, id: &str, formats: &[String]) -> Vec<Item> {
    let mut scan = Scan::new(page);
    let mut chapters = Map::new();
    let index = scan.extr(" id=\"chapter_index\"", "</ul>");
    for chapter in extract_iter(index, " value=\"", "</option>") {
        let (cid, name) = chapter.split_once("\">").unwrap_or((chapter, ""));
        chapters.insert(cid.to_owned(), unescape(name).into());
    }
    let mut fmts: Vec<(String, String)> = Vec::new();
    let mut path = "";
    for link in extract_iter(scan.extr(" class=\"download\"", "</ul>"), " href=\"", "</") {
        let (href, kind) = link.rsplit_once("\">").unwrap_or(("", link));
        path = href;
        fmts.push((kind.to_lowercase(), href.to_owned()));
    }
    let mut data = Map::new();
    let mut tags = |data: &mut Map<String, Value>, key: &str, class: &str| {
        let html = scan.extr(&format!("<dd class=\"{class} tags\">"), "</dd>");
        data.insert(key.into(), json!(split_html(html)));
    };
    data.insert("id".into(), parse_int(id, 0).into());
    tags(&mut data, "rating", "rating");
    tags(&mut data, "warnings", "warning");
    tags(&mut data, "categories", "category");
    tags(&mut data, "fandom", "fandom");
    tags(&mut data, "relationships", "relationship");
    tags(&mut data, "characters", "character");
    tags(&mut data, "tags", "freeform");
    let lang = scan.extr("<dd class=\"language\" lang=\"", "\"").to_owned();
    data.insert("lang".into(), lang.clone().into());
    let series = scan.extr("<dd class=\"series\">", "</dd>").to_owned();
    let published = super::parse_iso(scan.extr("<dd class=\"published\">", "<"));
    data.insert("series".into(), series.clone().into());
    data.insert("date".into(), super::date(published));
    let completed = scan.extr(">Completed:</dt><dd class=\"status\">", "<");
    data.insert(
        "date_completed".into(),
        super::date(super::parse_iso(completed)),
    );
    let updated = path.rsplit_once("updated_at=").map_or(path, |(_, ts)| ts);
    let updated = super::parse_ts(&Value::from(updated));
    data.insert("date_updated".into(), super::date(updated));
    fn count(scan: &mut Scan<'_>, begin: &str, end: &str) -> Value {
        parse_int(&remove_html(scan.extr(begin, end)).replace(',', ""), 0).into()
    }
    data.insert(
        "words".into(),
        count(&mut scan, "<dd class=\"words\">", "<"),
    );
    data.insert("chapters".into(), chapters.into());
    data.insert(
        "comments".into(),
        count(&mut scan, "<dd class=\"comments\">", "<"),
    );
    data.insert(
        "likes".into(),
        count(&mut scan, "<dd class=\"kudos\">", "<"),
    );
    data.insert(
        "bookmarks".into(),
        count(&mut scan, "<dd class=\"bookmarks\">", "</dd>"),
    );
    data.insert("views".into(), count(&mut scan, "<dd class=\"hits\">", "<"));
    let title = unescape(remove_html(scan.extr(" class=\"title heading\">", "</h2>")).trim());
    data.insert("title".into(), title.clone().into());
    let author = unescape(&remove_html(
        scan.extr(" class=\"byline heading\">", "</h3>"),
    ));
    data.insert("author".into(), author.clone().into());
    let summary = scan.extr(" class=\"heading\">Summary:</h3>", "</div>");
    data.insert("summary".into(), json!(split_html(summary)));
    let language = crate::sites::manga::text::code_to_language(&lang);
    data.insert("language".into(), language.map_or(Value::Null, Value::from));
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

    let mut items = Vec::new();
    for format in formats {
        // Missing formats are skipped with a warning upstream.
        let Some((_, href)) = fmts.iter().find(|(kind, _)| kind == format) else {
            continue;
        };
        let Some(url) = super::file_url(href, ROOT) else {
            continue;
        };
        super::nameext_into(url.as_str(), &mut data);
        let extension = data["extension"].as_str().unwrap_or_default().to_owned();
        items.push(
            Item::new(url, data.clone())
                .ids(id, extension)
                .user(Some(author.clone()))
                .caption(Some(title.clone()))
                .timestamp(published),
        );
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, queued, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        let list = |sub: &'static str, path: &str| Some(Target::List(sub, path.into()));
        assert_eq!(
            t("https://archiveofourown.org/works/47802"),
            Some(Target::Work("47802".into()))
        );
        assert_eq!(
            t("https://www.ao3.com/works/47802/chapters/1"),
            Some(Target::Work("47802".into()))
        );
        assert_eq!(
            t("https://archiveofourown.org/series/1903030"),
            list("series", "/series/1903030")
        );
        assert_eq!(
            t("https://archiveofourown.org/tags/Sunshine/works?page=2"),
            list("tag", "/tags/Sunshine/works?page=2")
        );
        assert_eq!(
            t("https://archiveofourown.org/tags/A%20B/works/?x=1"),
            list("tag", "/tags/A%20B/works/?x=1")
        );
        assert_eq!(
            t("https://archiveofourown.org/works/search?work_search%5Bquery%5D=air"),
            list("search", "/works/search?work_search%5Bquery%5D=air")
        );
        for raw in [
            "https://archiveofourown.org/users/mikf",
            "https://archiveofourown.org/users/mikf/",
            "https://archiveofourown.org/users/mikf/profile",
            "https://archiveofourown.org/users/mikf?x=1",
        ] {
            assert_eq!(t(raw), Some(Target::User("mikf".into())), "{raw}");
        }
        assert_eq!(
            t("https://archiveofourown.org/users/mikf/pseuds/other/profile"),
            Some(Target::User("mikf/pseuds/other".into()))
        );
        assert_eq!(
            t("https://archiveofourown.org/users/mikf/works?page=2"),
            list("user-works", "/users/mikf/works?page=2")
        );
        assert_eq!(
            t("https://archiveofourown.org/users/mikf/pseuds/other/series"),
            list("user-series", "/users/mikf/pseuds/other/series")
        );
        assert_eq!(
            t("https://archiveofourown.org/users/mikf/bookmarks/x?y=1"),
            list("user-bookmark", "/users/mikf/bookmarks")
        );
        assert_eq!(
            t("https://archiveofourown.org/users/mikf/subscriptions"),
            list("subscriptions", "/users/mikf/subscriptions")
        );
        for raw in [
            "https://archiveofourown.org/works/search",
            "https://archiveofourown.org/works/abc",
            "https://archiveofourown.org/tags/x",
            "https://archiveofourown.org/users/mikf/other",
            "https://archiveofourown.org/users/mikf/pseuds/x/subscriptions",
            "https://archiveofourown.org.evil.com/works/1",
            "https://evilarchiveofourown.org/works/1",
            "https://download.archiveofourown.org/works/1",
            "https://archiveofourown.org:8443/works/1",
            "https://u:p@archiveofourown.org/works/1",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    const WORK_PAGE: &str = r#"<ul id="chapter_index"><select><option value="101">1. One &amp; only</option><option value="102">2. Two</option></select></ul>
<li class="download"><ul class="expandable secondary">
<li><a href="/downloads/47802/Title%20Here.epub?updated_at=1600000000">EPUB</a></li>
<li><a href="/downloads/47802/Title%20Here.pdf?updated_at=1600000000">PDF</a></li></ul>
<dd class="rating tags"><ul><li><a class="tag">Teen And Up Audiences</a></li></ul></dd>
<dd class="warning tags"><ul><li><a class="tag">No Archive Warnings Apply</a></li></ul></dd>
<dd class="category tags"><ul><li><a class="tag">Gen</a></li></ul></dd>
<dd class="fandom tags"><ul><li><a class="tag">Fandom</a></li></ul></dd>
<dd class="relationship tags"></dd>
<dd class="character tags"><ul><li><a>A</a></li><li><a>B &amp; C</a></li></ul></dd>
<dd class="freeform tags"><ul><li><a>Fluff</a></li></ul></dd>
<dd class="language" lang="en">English</dd>
<dd class="series"><span class="series"><a class="previous" href="/works/1">&larr;</a> <span class="position">Part 2 of <a href="/series/123">My &amp; Series</a></span> <a class="next" href="/works/3">&rarr;</a></span></dd>
<dd class="published">2020-01-02</dd><dt class="status">Completed:</dt><dd class="status">2020-02-03</dd>
<dd class="words">1,234</dd><dd class="comments">5</dd><dd class="kudos">10</dd>
<dd class="bookmarks"><a href="/works/47802/bookmarks">3</a></dd><dd class="hits">1,000</dd>
<h2 class="title heading">
  Title &amp; Here
</h2>
<h3 class="byline heading"><a rel="author" href="/users/auth/pseuds/auth">auth</a></h3>
<div class="summary module"><h3 class="heading">Summary:</h3><blockquote><p>Line one.</p><p>Line two.</p></blockquote></div>"#;

    #[tokio::test]
    async fn extracts_work_formats() {
        let (root, server) = serve(vec![html(WORK_PAGE)]);
        let http = http("ao3", root).with_options(Some(std::sync::Arc::new(
            serde_json::from_value(json!({"formats": "EPUB, mobi,pdf"})).unwrap(),
        )));
        let files = media(extract(
            &http,
            Url::parse("https://ao3.org/works/47802").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[1]);
        assert_eq!(k["id"], 47802);
        assert_eq!(k["rating"], json!(["Teen And Up Audiences"]));
        assert_eq!(k["relationships"], json!([]));
        assert_eq!(k["characters"], json!(["A", "B & C"]));
        assert_eq!(k["lang"], "en");
        assert_eq!(k["language"], "English");
        assert_eq!(
            k["series"],
            json!({"prev": "1", "index": "2", "id": "123", "name": "My & Series", "next": "3"})
        );
        assert_eq!(k["date"], "2020-01-02 00:00:00");
        assert_eq!(k["date_completed"], "2020-02-03 00:00:00");
        assert_eq!(k["date_updated"], "2020-09-13 12:26:40");
        assert_eq!(k["words"], 1234);
        assert_eq!(
            k["chapters"],
            json!({"101": "1. One & only", "102": "2. Two"})
        );
        assert_eq!(k["bookmarks"], 3);
        assert_eq!(k["views"], 1000);
        assert_eq!(k["title"], "Title & Here");
        assert_eq!(k["author"], "auth");
        assert_eq!(k["summary"], json!(["Line one.", "Line two."]));
        assert_eq!(k["filename"], "Title Here");
        assert_eq!(k["extension"], "pdf");
        assert_eq!(keywords(&files[0])["extension"], "epub");
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "47802 Title & Here.pdf");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("ao3/auth")
        );
        let (_, identity) = crate::sites::archive_key(&files[1]).unwrap();
        assert_eq!(identity, r#"["{id}.{extension}",["47802","pdf"]]"#);
        assert_eq!(
            files[1].url.as_str(),
            "https://archiveofourown.org/downloads/47802/Title%20Here.pdf?updated_at=1600000000"
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /works/47802 "));
        assert!(requests[0].contains("view_adult=true"));
    }

    #[tokio::test]
    async fn restricted_and_adult_works_fail() {
        let warning = "<h2 class=\"landmark heading\">Adult Content Warning</h2>";
        let login = "<div id=\"loginform\"><form></form></div>";
        let (root, server) = serve(vec![html(warning), html(login)]);
        let http = http("ao3", root);
        for _ in 0..2 {
            let events = extract(&http, Url::parse("https://ao3.org/works/1").unwrap());
            assert!(crate::sites::testing::fails(events).await);
        }
        server.join().unwrap();
    }

    #[tokio::test]
    async fn tag_listings_queue_works_across_pages() {
        let first = r#"<li id="work_11" class="work"></li><li id="work_x"></li>
            <a rel="next" href="/tags/Sun/works?page=2&amp;x=1">Next</a>"#;
        let second = r#"<li id="work_12" class="work"></li>"#;
        let (root, server) = serve(vec![html(first), html(second)]);
        let http = http("ao3", root);
        let children = queued(extract(
            &http,
            Url::parse("https://archiveofourown.org/tags/Sun/works").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].subcategory(), "tag");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /tags/Sun/works "));
        assert!(requests[1].starts_with("GET /tags/Sun/works?page=2&x=1 "));
    }

    #[tokio::test]
    async fn bookmarks_and_subscriptions_queue_by_type() {
        let bookmarks = r#"<span class="count"><a href="/works/5/bookmarks">2</a></span>
            <span class="count"><a href="/series/6/bookmarks">1</a></span>
            <span class="count"><a href="/collections/7/bookmarks">1</a></span>"#;
        let subscriptions = "<dt>\n<a href=\"/users/someone\">x</a></dt>";
        let (root, server) = serve(vec![html(bookmarks), html(subscriptions)]);
        let http = http("ao3", root);
        let children = queued(extract(
            &http,
            Url::parse("https://archiveofourown.org/users/u/bookmarks").unwrap(),
        ))
        .await;
        // Works and series; other bookmark types are skipped.
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].subcategory(), "user-bookmark");
        let children = queued(extract(
            &http,
            Url::parse("https://archiveofourown.org/users/u/subscriptions").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].subcategory(), "subscriptions");
        server.join().unwrap();

        let children = queued(extract(
            &http,
            Url::parse("https://archiveofourown.org/users/u").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].subcategory(), "user");
    }
}
