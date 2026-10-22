//! realbooru.com tag searches, favorites, pools and posts, mirroring
//! gallery_dl/extractor/realbooru.py by scraping the Gelbooru-0.1 HTML pages.
use super::{Extr, below, date_value, extract_iter, leading_digits, parse_int, strptime};
use crate::sites::{Events, Http, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use std::collections::VecDeque;
use url::Url;

const ROOT: &str = "https://realbooru.com";
const FILENAME: &str = "{category}_{id}_{md5}.{extension}";

pub static SITE: Site = Site {
    category: "realbooru",
    name: "Realbooru",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&TAG, &FAVORITE, &POOL, &POST],
    api_key: false,
};

static TAG: Route = Route::new(
    "tag",
    FILENAME,
    &["{category}", "{search_tags}"],
    "t_{search_tags}_{id}",
);
static FAVORITE: Route = Route::new(
    "favorite",
    FILENAME,
    &["{category}", "favorites", "{favorite_id}"],
    "f_{favorite_id}_{id}",
);
static POOL: Route = Route::new(
    "pool",
    FILENAME,
    &["{category}", "pool", "{pool} {pool_name}"],
    "p_{pool}_{id}",
);
static POST: Route = Route::new("post", FILENAME, &["{category}"], "{id}");

#[derive(Debug, PartialEq)]
enum Target {
    /// Raw tag group.
    Tag(String),
    Favorite(String),
    Pool(String),
    Post(String),
}

fn target(url: &Url) -> Option<Target> {
    let raw = below(url, &["realbooru.com"])?;
    if let Some(tags) = raw.strip_prefix("/index.php?page=post&s=list&tags=") {
        return Some(Target::Tag(super::until(tags, &['&', '#']).to_owned()));
    }
    let id = |prefix: &str| raw.strip_prefix(prefix).and_then(leading_digits).map(str::to_owned);
    if let Some(id) = id("/index.php?page=favorites&s=view&id=") {
        return Some(Target::Favorite(id));
    }
    if let Some(id) = id("/index.php?page=pool&s=show&id=") {
        return Some(Target::Pool(id));
    }
    id("/index.php?page=post&s=view&id=").map(Target::Post)
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported realbooru URL"));
    };
    let mut data = Map::new();
    match target {
        Target::Tag(raw) => {
            let tags = super::unquote(&raw.replace('+', " "));
            data.insert("search_tags".into(), tags.clone().into());
            let params = vec![("page", "post".into()), ("s", "list".into()), ("tags", tags)];
            posts(http, &TAG, data, State::list(params, "<a id=\"p", 42))
        }
        Target::Favorite(id) => {
            data.insert("favorite_id".into(), parse_int(&id).into());
            let params = vec![("page", "favorites".into()), ("s", "view".into()), ("id", id)];
            posts(http, &FAVORITE, data, State::list(params, "\" id=\"p", 50))
        }
        Target::Pool(id) => crate::sites::once(async move {
            let mut url = http.api(&format!("{ROOT}/index.php"))?;
            url.query_pairs_mut()
                .append_pair("page", "pool")
                .append_pair("s", "show")
                .append_pair("id", &id);
            let page = http.text(&url).await?;
            let (name, ids) = parse_pool(&page)?;
            data.insert("pool".into(), parse_int(&id).into());
            data.insert("pool_name".into(), crate::sites::unescape(&name).into());
            Ok(posts(
                http,
                &POOL,
                data,
                State {
                    ids: ids.into(),
                    list: None,
                },
            ))
        }),
        Target::Post(id) => posts(
            http,
            &POST,
            data,
            State {
                ids: VecDeque::from([id]),
                list: None,
            },
        ),
    }
}

/// The pool name and its post ids.
fn parse_pool(page: &str) -> Result<(String, Vec<String>)> {
    let begin = "<h4>Pool: ";
    let start = page
        .find(begin)
        .ok_or_else(|| error("unexpected realbooru pool layout"))?
        + begin.len();
    let end = page[start..]
        .find("</h4>")
        .ok_or_else(|| error("unexpected realbooru pool layout"))?
        + start;
    let ids = extract_iter(&page[end + 5..], "class=\"thumb\" id=\"p", "\"")
        .into_iter()
        .map(str::to_owned)
        .collect();
    Ok((page[start..end].to_owned(), ids))
}

/// Listing pagination (`_pagination`): query parameters, the post-id
/// delimiter, `per_page`, and the next `pid` (`None` once exhausted).
struct List {
    params: Vec<(&'static str, String)>,
    begin: &'static str,
    per_page: i64,
    pid: Option<i64>,
}

/// Post ids still to fetch, then the listing they came from.
struct State {
    ids: VecDeque<String>,
    list: Option<List>,
}

impl State {
    fn list(params: Vec<(&'static str, String)>, begin: &'static str, per_page: i64) -> Self {
        Self {
            ids: VecDeque::new(),
            list: Some(List {
                params,
                begin,
                per_page,
                pid: Some(0),
            }),
        }
    }
}

/// Fetch one post page per step, requesting listing pages as needed.
fn posts<'a>(
    http: &'a Http,
    route: &'static Route,
    data: Map<String, Value>,
    state: State,
) -> Events<'a> {
    crate::sites::paginate(state, move |mut state| {
        let data = data.clone();
        async move {
            loop {
                if let Some(id) = state.ids.pop_front() {
                    let mut url = http.api(&format!("{ROOT}/index.php"))?;
                    url.query_pairs_mut()
                        .append_pair("page", "post")
                        .append_pair("s", "view")
                        .append_pair("id", &id);
                    let page = http.text(&url).await?;
                    let events = match finish(parse_post(&id, &page), &data) {
                        Some(item) => crate::sites::post(&SITE, http, route, vec![item]),
                        None => Vec::new(),
                    };
                    return Ok(Some((crate::sites::events(events), state)));
                }
                let Some(list) = state.list.as_mut() else {
                    return Ok(None);
                };
                let Some(pid) = list.pid else {
                    return Ok(None);
                };
                let mut url = http.api(&format!("{ROOT}/index.php"))?;
                {
                    let mut query = url.query_pairs_mut();
                    for (key, value) in &list.params {
                        query.append_pair(key, value);
                    }
                    query.append_pair("pid", &pid.to_string());
                }
                let page = http.text(&url).await?;
                let ids = extract_iter(&page, list.begin, "\"");
                list.pid = (ids.len() as i64 >= list.per_page && pid < 10_000_000)
                    .then_some(pid + list.per_page);
                state.ids = ids.into_iter().map(str::to_owned).collect();
            }
        }
    })
}

/// `RealbooruExtractor._parse_post` for a fetched post page.
fn parse_post(id: &str, page: &str) -> Map<String, Value> {
    let mut extr = Extr::new(page);
    let rating = extr.next("name=\"rating\" content=\"", "\"");
    extr.next("class=\"container\"", ">");
    let file_url = extr.next("src=\"", "\"");
    if file_url.ends_with(".mp4") {
        // `_fallback`: the second source; not exposed.
        extr.next("src=\"", "\"");
    }
    let mut post = Map::new();
    post.insert("id".into(), id.into());
    let rating = if rating == "adult" {
        "e".to_owned()
    } else {
        rating.chars().next().unwrap_or('?').to_string()
    };
    post.insert("rating".into(), rating.into());
    post.insert("file_url".into(), file_url.into());
    post.insert(
        "created_at".into(),
        extr.next(">Posted at ", " by ").into(),
    );
    post.insert("uploader".into(), extr.next(">", "<").into());
    post.insert("score".into(), extr.next("\">", "<").into());
    let container = extr.next("<br />", "</div>");
    post.insert(
        "title".into(),
        extr.next("id=\"title\" style=\"width: 100%;\" value=\"", "\"")
            .into(),
    );
    post.insert(
        "source".into(),
        extr.next("d=\"source\" style=\"width: 100%;\" value=\"", "\"")
            .into(),
    );
    let mut tags = Vec::new();
    let mut categories: Vec<(String, Vec<String>)> = Vec::new();
    for (kind, name) in find_tags(container) {
        let tag = crate::sites::unescape(&super::unquote(name));
        tags.push(tag.clone());
        match categories.iter_mut().find(|(k, _)| k == kind) {
            Some((_, list)) => list.push(tag),
            None => categories.push((kind.to_owned(), vec![tag])),
        }
    }
    for (kind, list) in categories {
        post.insert(format!("tags_{kind}"), list.join(", ").into());
    }
    tags.sort();
    post.insert("tags".into(), tags.join(", ").into());
    let md5 = super::before(super::rpartition(file_url, "/").1, ".");
    post.insert("md5".into(), md5.into());
    post
}

/// `re.findall(r'<a class="(?:tag-type-)?([^"]+).*?;tags=([^"&]+)')`.
fn find_tags(text: &str) -> Vec<(&str, &str)> {
    let mut out = Vec::new();
    let mut pos = 0;
    'outer: while let Some(offset) = text[pos..].find("<a class=\"") {
        let start = pos + offset;
        let after = start + "<a class=\"".len();
        let mut candidates = vec![after];
        if text[after..].starts_with("tag-type-") {
            candidates.insert(0, after + "tag-type-".len());
        }
        for class_start in candidates {
            let class_end = text[class_start..]
                .find('"')
                .map_or(text.len(), |p| class_start + p);
            if class_end == class_start {
                continue;
            }
            // `.*?` does not cross newlines.
            let line_end = text[class_end..]
                .find('\n')
                .map_or(text.len(), |p| class_end + p);
            let line = &text[class_end..line_end];
            let mut search = 0;
            while let Some(found) = line[search..].find(";tags=") {
                let name_start = class_end + search + found + ";tags=".len();
                let name_end = text[name_start..]
                    .find(['"', '&'])
                    .map_or(text.len(), |p| name_start + p);
                if name_end > name_start {
                    out.push((&text[class_start..class_end], &text[name_start..name_end]));
                    pos = name_end;
                    continue 'outer;
                }
                search += found + 1;
            }
        }
        pos = start + 1;
    }
    out
}

/// `BooruExtractor.items` and `_prepare` for one post.
fn finish(mut post: Map<String, Value>, data: &Map<String, Value>) -> Option<crate::sites::Item> {
    let raw = post.get("file_url")?.as_str()?.to_owned();
    let raw = if raw.starts_with('/') {
        format!("{ROOT}{raw}")
    } else {
        raw
    };
    let url = Url::parse(&raw).ok().filter(crate::sites::plain)?;
    super::booru_fields(&url, &mut post, data);
    let created = post.get("created_at").and_then(Value::as_str).unwrap_or_default();
    let timestamp = strptime(created, "%b, %d %Y");
    post.insert("date".into(), date_value(timestamp));
    Some(super::item(url, post, "uploader", "tags", timestamp))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://realbooru.com/index.php?page=post&s=list&tags=a+b%21&pid=42"),
            Some(Target::Tag("a+b%21".into()))
        );
        assert_eq!(
            t("https://realbooru.com/index.php?page=post&s=list&tags="),
            Some(Target::Tag(String::new()))
        );
        assert_eq!(
            t("https://realbooru.com/index.php?page=favorites&s=view&id=12345"),
            Some(Target::Favorite("12345".into()))
        );
        assert_eq!(
            t("http://realbooru.com/index.php?page=pool&s=show&id=9"),
            Some(Target::Pool("9".into()))
        );
        assert_eq!(
            t("https://realbooru.com/index.php?page=post&s=view&id=7"),
            Some(Target::Post("7".into()))
        );
        assert_eq!(t("https://realbooru.com/index.php?s=view&page=post&id=7"), None);
        assert_eq!(t("https://www.realbooru.com/index.php?page=post&s=view&id=7"), None);
        assert_eq!(t("https://realbooru.com.evil.test/index.php?page=post&s=view&id=7"), None);
        assert_eq!(t("https://realbooru.com:444/index.php?page=post&s=view&id=7"), None);
        assert_eq!(t("https://realbooru.com/index.php?page=post&s=view&id=x"), None);
    }

    #[test]
    fn finds_tags_like_the_regex() {
        let html = "<a class=\"tag-type-artist\" href=\"index.php?page=post&amp;s=list&amp;tags=some_artist\">x</a>\
                    <a class=\"model\" href=\"index.php?s=list&amp;tags=a%20b&amp;x\">y</a>\
                    <a class=\"none\" href=\"/\">\n;tags=no</a>";
        assert_eq!(
            find_tags(html),
            vec![("artist", "some_artist"), ("model", "a%20b")]
        );
    }

    fn post_page(id: u32) -> String {
        format!(
            "<meta name=\"rating\" content=\"adult\"><div class=\"container\">\
             <video><source src=\"https://realbooru.com//images/ab/cd/md5{id}.mp4\"><source src=\"https://fallback/x.webm\"></video>\
             <div>Posted at Jul, 14 2017 by <a href=\"/u\">user{id}</a> \
             <span>Score: <span id=\"psc\">5</span></span><br />\
             <a class=\"tag-type-general\" href=\"index.php?page=post&amp;s=list&amp;tags=zeta\">zeta</a>\
             <a class=\"tag-type-general\" href=\"index.php?page=post&amp;s=list&amp;tags=alpha\">alpha</a>\
             <a class=\"copyright\" href=\"index.php?page=post&amp;s=list&amp;tags=c%26amp%3Bd\">c</a></div>\
             <input id=\"title\" style=\"width: 100%;\" value=\"Title\">\
             <input id=\"source\" style=\"width: 100%;\" value=\"https://src\">"
        )
    }

    #[tokio::test]
    async fn paginates_tag_search_per_post() {
        let mut listing = String::new();
        for id in 1..=42 {
            listing.push_str(&format!("<a id=\"p{id}\" href=\"x\">"));
        }
        let mut replies = vec![html(&listing), html(&post_page(1))];
        // Only the first post is drained; later pages are never requested.
        replies.truncate(2);
        let (root, server) = serve(replies);
        let http = http("realbooru", root);
        let events = extract(
            &http,
            Url::parse("https://realbooru.com/index.php?page=post&s=list&tags=a+b").unwrap(),
        );
        let first = crate::event::media_only(events)
            .next()
            .await
            .unwrap()
            .unwrap();
        let k = keywords(&first);
        assert_eq!(k["id"], "1");
        assert_eq!(k["rating"], "e");
        assert_eq!(k["md5"], "md51");
        assert_eq!(k["uploader"], "user1");
        assert_eq!(k["score"], "5");
        assert_eq!(k["tags"], "alpha, c&d, zeta");
        assert_eq!(k["tags_general"], "zeta, alpha");
        assert_eq!(k["tags_copyright"], "c&d");
        assert_eq!(k["title"], "Title");
        assert_eq!(k["source"], "https://src");
        assert_eq!(k["created_at"], "Jul, 14 2017");
        assert_eq!(k["date"], "2017-07-14 00:00:00");
        assert_eq!(k["search_tags"], "a b");
        assert_eq!(k["extension"], "mp4");
        let (file, dir) = crate::sites::default_templates(&first).unwrap();
        assert_eq!(file.render(&first).unwrap(), "realbooru_1_md51.mp4");
        assert_eq!(
            dir.render(&first).unwrap().unwrap(),
            std::path::PathBuf::from("realbooru/a b")
        );
        let (_, identity) = crate::sites::archive_key(&first).unwrap();
        assert_eq!(identity, r#"["t_{search_tags}_{id}",["a b","1"]]"#);
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("GET /index.php?page=post&s=list&tags=a+b&pid=0 "),
            "{}",
            requests[0]
        );
        assert!(requests[1].starts_with("GET /index.php?page=post&s=view&id=1 "));
    }

    #[tokio::test]
    async fn favorites_advance_pid_by_per_page() {
        let mut listing = String::new();
        for id in 1..=50 {
            listing.push_str(&format!("<span class=\"thumb\" id=\"p{id}\">"));
        }
        let mut replies = vec![html(&listing)];
        replies.extend((1..=50).map(|id| html(&post_page(id))));
        replies.push(html("<span class=\"thumb\" id=\"p51\">"));
        replies.push(html(&post_page(51)));
        let (root, server) = serve(replies);
        let http = http("realbooru", root);
        let files = media(extract(
            &http,
            Url::parse("https://realbooru.com/index.php?page=favorites&s=view&id=12").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 51);
        assert_eq!(keywords(&files[0])["favorite_id"], 12);
        let (_, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("realbooru/favorites/12")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /index.php?page=favorites&s=view&id=12&pid=0 "));
        assert!(
            requests[51].starts_with("GET /index.php?page=favorites&s=view&id=12&pid=50 "),
            "{}",
            requests[51]
        );
    }

    #[tokio::test]
    async fn pool_reads_name_and_ids() {
        let pool = "<h4>Pool: My &amp; Pool</h4><span class=\"thumb\" id=\"p3\"></span>";
        let (root, server) = serve(vec![html(pool), html(&post_page(3))]);
        let http = http("realbooru", root);
        let files = media(extract(
            &http,
            Url::parse("https://realbooru.com/index.php?page=pool&s=show&id=9").unwrap(),
        ))
        .await;
        let k = keywords(&files[0]);
        assert_eq!(k["pool"], 9);
        assert_eq!(k["pool_name"], "My & Pool");
        let (_, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("realbooru/pool/9 My & Pool")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["p_{pool}_{id}",["9","3"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /index.php?page=pool&s=show&id=9 "));
        assert!(requests[1].starts_with("GET /index.php?page=post&s=view&id=3 "));
    }
}
