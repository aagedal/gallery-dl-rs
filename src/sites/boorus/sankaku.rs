//! Sankaku Channel (sankaku.app) and Idol Complex, mirroring
//! gallery_dl/extractor/sankaku.py and idolcomplex.py through the
//! `sankakuapi.com` / `i.sankakuapi.com` API: tag searches (keyset
//! pagination), pools/books, single posts and book searches.
//!
//! Options: `tags` (`true` for the API's tag categories, `"extended"` for
//! the post page's sidebar) and `notes`. Upstream's `username`/`password`
//! login is not supported, so posts that need an account have no file URL
//! and are skipped; the download-URL `refresh` option is not needed as
//! pages are requested only as files are consumed.
use super::{date_value, parse_query, unquote};
use crate::sites::art::{parse_ts, str_of, truthy, update};
use crate::sites::{Events, Http, Item, Route, Site, unescape};
use crate::{Result, error};
use futures_util::{StreamExt, TryStreamExt, stream};
use serde_json::{Map, Value};
use url::Url;

const FILENAME: &str = "{category}_{id}_{md5}.{extension}";
/// `BooruExtractor.per_page`.
const PER_PAGE: &str = "100";
static TAG: Route = Route::new(
    "tag",
    FILENAME,
    &["{category}", "{search_tags}"],
    "t_{search_tags}_{id}",
);
static POOL: Route = Route::new(
    "pool",
    FILENAME,
    &["{category}", "pool", "{pool[id]} {pool[name_en]}"],
    "p_{pool}_{id}",
);
static POST: Route = Route::new("post", FILENAME, &["{category}"], "{id}");
static BOOKS: Route = Route::new("books", FILENAME, &["{category}"], "");

pub static SANKAKU: Site = Site {
    category: "sankaku",
    name: "Sankaku Channel",
    matches: |url| target(&SANKAKU_API, url).is_some(),
    extract: |http, url| extract(&SANKAKU_API, http, url),
    routes: &[&TAG, &POOL, &POST, &BOOKS],
    api_key: false,
    cookies: None,
};

pub static IDOLCOMPLEX: Site = Site {
    category: "idolcomplex",
    name: "Idol Complex",
    matches: |url| target(&IDOLCOMPLEX_API, url).is_some(),
    extract: |http, url| extract(&IDOLCOMPLEX_API, http, url),
    routes: &[&TAG, &POOL, &POST],
    api_key: false,
    cookies: Some("idolcomplex.com"),
};

/// One frontend: its site, hosts, `Origin` and API root.
struct Frontend {
    site: &'static Site,
    hosts: &'static [&'static str],
    root: &'static str,
    api: &'static str,
}

static SANKAKU_API: Frontend = Frontend {
    site: &SANKAKU,
    hosts: &[
        "chan.sankakucomplex.com",
        "www.sankakucomplex.com",
        "beta.sankakucomplex.com",
        "black.sankakucomplex.com",
        "white.sankakucomplex.com",
        "sankaku.app",
    ],
    root: "https://sankaku.app",
    api: "https://sankakuapi.com",
};

static IDOLCOMPLEX_API: Frontend = Frontend {
    site: &IDOLCOMPLEX,
    hosts: &[
        "idolcomplex.com",
        "www.idolcomplex.com",
        "idol.sankakucomplex.com",
        "www.idol.sankakucomplex.com",
    ],
    root: "https://www.idolcomplex.com",
    api: "https://i.sankakuapi.com",
};

#[derive(Debug, PartialEq)]
enum Target {
    /// The raw query string.
    Tag(String),
    Pool(String),
    Post(String),
    Books(String),
}

/// Python `\w+` on a raw path segment.
fn word(text: &str) -> Option<&str> {
    let len = text
        .bytes()
        .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_')
        .count();
    (len > 0).then(|| &text[..len])
}

/// `/[a-z]{2}(?:[-_][A-Z]{2})?`: an optional language segment.
fn language(segment: &str) -> bool {
    let b = segment.as_bytes();
    match b.len() {
        2 => b.iter().all(u8::is_ascii_lowercase),
        5 => {
            b[..2].iter().all(u8::is_ascii_lowercase)
                && matches!(b[2], b'-' | b'_')
                && b[3..].iter().all(u8::is_ascii_uppercase)
        }
        _ => false,
    }
}

fn target(frontend: &Frontend, url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) || !frontend.hosts.contains(&url.host_str()?) {
        return None;
    }
    let path = url.path();
    let first = path[1..].split('/').next().unwrap_or_default();
    let stripped = language(first).then(|| &path[first.len() + 1..]);
    [stripped, Some(path)]
        .into_iter()
        .flatten()
        .find_map(|path| route(frontend, path, url.query()))
}

/// The extractor for a path below the optional language segment.
fn route(frontend: &Frontend, path: &str, query: Option<&str>) -> Option<Target> {
    let sankaku = std::ptr::eq(frontend, &SANKAKU_API);
    if matches!(path, "" | "/" | "/posts" | "/posts/") {
        return query.map(|q| Target::Tag(q.to_owned()));
    }
    let after = |prefixes: &[&str]| {
        prefixes
            .iter()
            .find_map(|prefix| path.strip_prefix(prefix))
            .and_then(word)
            .map(str::to_owned)
    };
    let pool = if sankaku {
        after(&["/books/", "/pool/show/", "/pools/show/"])
    } else {
        after(&["/pool/show/", "/pools/show/", "/pool/", "/pools/"])
    };
    if let Some(id) = pool {
        return Some(Target::Pool(id));
    }
    if let Some(id) = after(&["/post/show/", "/posts/show/", "/post/", "/posts/"]) {
        return Some(Target::Post(id));
    }
    if sankaku && matches!(path, "/books" | "/books/") {
        return query.map(|q| Target::Books(q.to_owned()));
    }
    None
}

/// The `tags` search parameter: `text.unquote(query["tags"].replace("+", " "))`.
fn search_tags(query: &str) -> String {
    let params = parse_query(query, false);
    let tags = super::query_get(&params, "tags").unwrap_or_default();
    unquote(&tags.replace('+', " "))
}

/// Rewrite `date:DD.MM.YYYY` and `date:YYYY-MM-DD` (not followed by `T`)
/// to `date:YYYY-MM-DDT00:00`, as upstream does for the API (#1790).
pub(crate) fn rewrite_dates(tags: &str) -> String {
    let mut out = String::with_capacity(tags.len());
    let mut rest = tags;
    while let Some(index) = rest.find("date:") {
        out.push_str(&rest[..index + 5]);
        rest = &rest[index + 5..];
        let b = rest.as_bytes();
        let digits = |range: std::ops::Range<usize>| {
            b.get(range)
                .is_some_and(|d| d.iter().all(u8::is_ascii_digit))
        };
        let sep = |i: usize| matches!(b.get(i), Some(b'.' | b'-'));
        let free = b.get(10) != Some(&b'T');
        if digits(0..2) && sep(2) && digits(3..5) && sep(5) && digits(6..10) && free {
            out.push_str(&format!(
                "{}-{}-{}T00:00",
                &rest[6..10],
                &rest[3..5],
                &rest[..2]
            ));
            rest = &rest[10..];
        } else if digits(0..4) && sep(4) && digits(5..7) && sep(7) && digits(8..10) && free {
            out.push_str(&format!(
                "{}-{}-{}T00:00",
                &rest[..4],
                &rest[5..7],
                &rest[8..10]
            ));
            rest = &rest[10..];
        }
    }
    out.push_str(rest);
    out
}

/// `\bparent:(\w+)` replaced by `id_range:\1`, or `None` without one.
pub(crate) fn parent_query(tags: &str) -> Option<String> {
    let mut out = String::new();
    let mut found = false;
    let mut last = 0;
    for (index, _) in tags.match_indices("parent:") {
        let boundary = tags[..index]
            .chars()
            .last()
            .is_none_or(|c| !(c.is_alphanumeric() || c == '_'));
        let value = &tags[index + 7..];
        if boundary
            && value
                .chars()
                .next()
                .is_some_and(|c| c.is_alphanumeric() || c == '_')
        {
            out.push_str(&tags[last..index]);
            out.push_str("id_range:");
            last = index + 7;
            found = true;
        }
    }
    out.push_str(&tags[last..]);
    found.then_some(out)
}

fn extract<'a>(frontend: &'static Frontend, http: &'a Http, url: Url) -> Events<'a> {
    let Some(target) = target(frontend, &url) else {
        return crate::sites::fail(error(format!("unsupported {} URL", frontend.site.name)));
    };
    let api = Api { http, frontend };
    match target {
        Target::Tag(query) => {
            let mut tags = search_tags(&query);
            if tags.contains("date:") {
                tags = rewrite_dates(&tags);
            }
            let mut data = Map::new();
            data.insert("search_tags".into(), tags.clone().into());
            let mut posts = Vec::new();
            if tags.contains("parent:")
                && let Some(parent) = parent_query(&tags)
            {
                posts.push(api.keyset("/v2/posts/keyset", vec![("tags".into(), parent)]));
            }
            posts.push(api.keyset("/v2/posts/keyset", vec![("tags".into(), tags)]));
            let posts = stream::iter(posts).flatten().boxed();
            items(api, &TAG, posts, data)
        }
        Target::Pool(id) => crate::sites::once(async move {
            let mut pool = api.call(&format!("/pools/{id}"), lang()).await?;
            let posts = pool_posts(&mut pool)?;
            let mut data = Map::new();
            data.insert("pool".into(), pool);
            let posts = stream::iter(posts.into_iter().map(Ok)).boxed();
            Ok(items(api, &POOL, posts, data))
        }),
        Target::Post(id) => crate::sites::once(async move {
            let prefix = if id.len() == 32 { "md5:" } else { "id_range:" };
            let params = vec![
                ("lang".into(), "en".into()),
                ("page".into(), "1".into()),
                ("limit".into(), "1".into()),
                ("tags".into(), format!("{prefix}{id}")),
            ];
            let posts = api.call("/v2/posts", params).await?;
            let posts = posts.as_array().cloned().unwrap_or_default();
            Ok(items(
                api,
                &POST,
                stream::iter(posts.into_iter().map(Ok)).boxed(),
                Map::new(),
            ))
        }),
        Target::Books(query) => {
            let params = vec![
                ("tags".into(), search_tags(&query)),
                ("pool_type".into(), "0".into()),
            ];
            api.keyset("/pools/keyset", params)
                .map(move |pool| match pool {
                    Ok(Value::Object(pool)) => {
                        let id = pool.get("id").map(crate::sites::scalar).unwrap_or_default();
                        match Url::parse(&format!("https://sankaku.app/books/{id}")) {
                            Ok(child) if word(&id) == Some(id.as_str()) => {
                                crate::sites::queue(http, "sankaku", "books", child, &pool)
                            }
                            _ => crate::sites::events(Vec::new()),
                        }
                    }
                    Ok(_) => crate::sites::events(Vec::new()),
                    Err(e) => crate::sites::fail(e),
                })
                .flatten()
                .boxed()
        }
    }
}

/// `SankakuPoolExtractor.metadata`: tag names and numbered posts.
fn pool_posts(pool: &mut Value) -> Result<Vec<Value>> {
    let pool = pool
        .as_object_mut()
        .ok_or_else(|| error("invalid Sankaku pool"))?;
    for key in ["tags", "artist_tags"] {
        let names: Vec<Value> = pool
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|tag| tag.get("name").cloned().unwrap_or(Value::Null))
            .collect();
        pool.insert(key.into(), names.into());
    }
    let mut posts = match pool.remove("posts") {
        Some(Value::Array(posts)) => posts,
        _ => Vec::new(),
    };
    for (index, post) in posts.iter_mut().enumerate() {
        if let Some(post) = post.as_object_mut() {
            post.insert("num".into(), (index + 1).into());
        }
    }
    Ok(posts)
}

/// The `lang` parameter every endpoint takes.
fn lang() -> Vec<(String, String)> {
    vec![("lang".into(), "en".into())]
}

type Posts<'a> = futures_util::stream::BoxStream<'a, Result<Value>>;

/// The Sankaku API of one frontend.
#[derive(Clone, Copy)]
struct Api<'a> {
    http: &'a Http,
    frontend: &'static Frontend,
}

impl<'a> Api<'a> {
    /// Upstream `_call`: an unsuccessful response aborts with its code.
    async fn call(&self, endpoint: &str, params: Vec<(String, String)>) -> Result<Value> {
        let mut url = self.http.api(&format!("{}{endpoint}", self.frontend.api))?;
        url.query_pairs_mut().extend_pairs(&params);
        let data = self
            .http
            .json_with(
                &url,
                &[
                    ("Accept", "application/vnd.sankaku.api+json;v=2"),
                    ("Origin", self.frontend.root),
                ],
            )
            .await?;
        if data.get("success") == Some(&Value::Bool(false)) {
            let code = str_of(&data, "code");
            let code = code
                .rsplit("__")
                .next()
                .unwrap_or_default()
                .replace(['-', '_'], " ");
            return Err(error(format!("{}: '{code}'", self.frontend.site.name)));
        }
        Ok(data)
    }

    /// Upstream `_pagination`: keyset pages following `meta.next`.
    fn keyset(self, endpoint: &'static str, params: Vec<(String, String)>) -> Posts<'a> {
        stream::try_unfold(Some(String::new()), move |next| {
            let mut params = params.clone();
            async move {
                let Some(next) = next else {
                    return Ok(None);
                };
                params.push(("lang".into(), "en".into()));
                params.push(("limit".into(), PER_PAGE.into()));
                if !next.is_empty() {
                    params.push(("next".into(), next));
                }
                let data = self.call(endpoint, params).await?;
                let posts = data
                    .get("data")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let next = data
                    .pointer("/meta/next")
                    .filter(|next| truthy(Some(next)))
                    .map(crate::sites::scalar);
                Ok::<_, crate::Error>(Some((stream::iter(posts.into_iter().map(Ok)), next)))
            }
        })
        .try_flatten()
        .boxed()
    }

    /// Upstream `tags`: a post's tags, 100 per page.
    async fn tags(&self, id: &str) -> Result<Vec<Value>> {
        let mut tags: Vec<Value> = Vec::new();
        for page in 1.. {
            let params = vec![
                ("lang".into(), "en".into()),
                ("page".into(), page.to_string()),
                ("limit".into(), "100".into()),
            ];
            let data = self.call(&format!("/posts/{id}/tags"), params).await?;
            let new = data
                .get("data")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if new.is_empty() {
                break;
            }
            let count = new.len();
            tags.extend(new);
            let total = data.get("total").and_then(Value::as_u64).unwrap_or(0);
            if count < 80 || tags.len() as u64 >= total {
                break;
            }
        }
        Ok(tags)
    }
}

/// `TAG_TYPES`.
fn tag_type(kind: &Value) -> &'static str {
    match kind.as_i64() {
        Some(0) => "general",
        Some(1) => "artist",
        Some(2) => "studio",
        Some(3) => "copyright",
        Some(4) => "character",
        Some(5) => "genre",
        Some(8) => "medium",
        Some(9) => "meta",
        _ => "",
    }
}

/// Tag names grouped by category, in order of first appearance.
fn group(tags: impl IntoIterator<Item = (String, String)>) -> Vec<(String, Vec<String>)> {
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    for (kind, name) in tags {
        match groups.iter_mut().find(|(k, _)| *k == kind) {
            Some((_, names)) => names.push(name),
            None => groups.push((kind, vec![name])),
        }
    }
    groups
}

/// `tags_TYPE` lists and `tag_string_TYPE` strings for each category.
fn insert_groups(post: &mut Map<String, Value>, groups: Vec<(String, Vec<String>)>) {
    for (kind, names) in groups {
        let string = names.join(" ");
        post.insert(format!("tags_{kind}"), names.into());
        post.insert(format!("tag_string_{kind}"), string.into());
    }
}

/// Upstream `_tags_extended`: the categories of the post page's sidebar.
pub(crate) fn sidebar_tags(page: &str) -> Vec<(String, Vec<String>)> {
    let sidebar = crate::sites::art::extr(page, "<ul id=\"tag-sidebar\"", "</ul>");
    let mut found = Vec::new();
    let mut rest = sidebar;
    // `tag-type-([^"' ]+).*?\?tags=([^"'&]+)`, within one line.
    while let Some(index) = rest.find("tag-type-") {
        rest = &rest[index + 9..];
        let kind_len = rest.find(['"', '\'', ' ']).unwrap_or(rest.len());
        let kind = &rest[..kind_len];
        let line = &rest[kind_len..];
        let line = &line[..line.find('\n').unwrap_or(line.len())];
        if kind.is_empty() {
            continue;
        }
        let Some(start) = line.find("?tags=") else {
            continue;
        };
        let value = &line[start + 6..];
        let len = value.find(['"', '\'', '&']).unwrap_or(value.len());
        if len == 0 {
            continue;
        }
        found.push((kind.to_owned(), unescape(&unquote(&value[..len]))));
        rest = &rest[kind_len + start + 6 + len..];
    }
    group(found)
}

/// `BooruExtractor.items` over a post stream: each post becomes one file,
/// with tag categories and notes when the options ask for them.
fn items<'a>(
    api: Api<'a>,
    route: &'static Route,
    posts: Posts<'a>,
    data: Map<String, Value>,
) -> Events<'a> {
    let tags = api.http.option("tags").cloned();
    let notes = api.http.option_bool("notes", false);
    posts
        .map(move |post| {
            let (data, tags) = (data.clone(), tags.clone());
            crate::sites::once(async move {
                let Value::Object(mut post) = post? else {
                    return Ok(crate::sites::events(Vec::new()));
                };
                let Some(url) = file_url(api.frontend.root, &post) else {
                    return Ok(crate::sites::events(Vec::new()));
                };
                let id = post.get("id").map(crate::sites::scalar).unwrap_or_default();
                if tags.as_ref().and_then(Value::as_str) == Some("extended") {
                    let page_url = format!("https://chan.sankakucomplex.com/posts/{id}");
                    // Failures only lose the categories, as upstream warns.
                    if let Ok(url) = api.http.api(&page_url)
                        && let Ok(page) = api
                            .http
                            .text_with(&url, &[("Referer", page_url.as_str())])
                            .await
                    {
                        insert_groups(&mut post, sidebar_tags(&page));
                    }
                } else if truthy(tags.as_ref()) {
                    let tags = api.tags(&id).await?;
                    let names = tags.iter().filter_map(|tag| {
                        let name = tag.get("name")?.as_str().filter(|n| !n.is_empty())?;
                        let kind = tag_type(tag.get("type").unwrap_or(&Value::Null));
                        Some((kind.to_owned(), name.to_lowercase().replace(' ', "_")))
                    });
                    insert_groups(&mut post, group(names));
                }
                if notes {
                    let mut list = Vec::new();
                    if truthy(post.get("has_notes")) {
                        let found = api.call(&format!("/posts/{id}/notes"), lang()).await?;
                        for mut note in found.as_array().cloned().unwrap_or_default() {
                            if let Some(note) = note.as_object_mut() {
                                for key in ["created_at", "updated_at"] {
                                    if let Some(s) = note.get(key).and_then(|v| v.get("s")).cloned()
                                    {
                                        note.insert(key.into(), s);
                                    }
                                }
                            }
                            list.push(note);
                        }
                    }
                    post.insert("notes".into(), list.into());
                }
                let item = prepare(url, post, &data);
                Ok(crate::sites::events(super::per_post(
                    api.frontend.site,
                    api.http,
                    route,
                    vec![item],
                )))
            })
        })
        .flatten()
        .boxed()
}

/// Upstream `_file_url`: posts without a URL (deleted, or needing a
/// login) are skipped; `http:` URLs become `https:`.
fn file_url(root: &str, post: &Map<String, Value>) -> Option<Url> {
    let raw = post.get("file_url")?.as_str().filter(|u| !u.is_empty())?;
    let raw = match raw.as_bytes().get(4) {
        Some(b's') | None => raw.to_owned(),
        Some(_) => format!("https{}", &raw[4..]),
    };
    let raw = if raw.starts_with('/') {
        format!("{root}{raw}")
    } else {
        raw
    };
    Url::parse(&raw).ok().filter(crate::sites::plain)
}

/// `BooruExtractor.items` bookkeeping and `SankakuExtractor._prepare`.
pub(crate) fn prepare(url: Url, mut post: Map<String, Value>, data: &Map<String, Value>) -> Item {
    if !post.contains_key("extension") {
        crate::sites::art::nameext_into(url.as_str(), &mut post);
    }
    update(&mut post, data);
    let created = post
        .get("created_at")
        .and_then(|c| c.get("s"))
        .cloned()
        .unwrap_or(Value::Null);
    let timestamp = parse_ts(&created);
    post.insert("created_at".into(), created);
    post.insert("date".into(), date_value(timestamp));
    let tags = post
        .remove("tag_names")
        .unwrap_or_else(|| Value::Array(Vec::new()));
    let tag_string = tags
        .as_array()
        .into_iter()
        .flatten()
        .map(crate::sites::scalar)
        .collect::<Vec<_>>()
        .join(" ");
    post.insert("tags".into(), tags);
    post.insert("tag_string".into(), tag_string.clone().into());
    let id = post.get("id").map(crate::sites::scalar).unwrap_or_default();
    let user = post
        .get("author")
        .and_then(|a| a.get("name"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    Item::new(url, post)
        .ids(id.clone(), id)
        .user(user)
        .caption(Some(tag_string))
        .timestamp(timestamp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http, json, keywords, media, queued, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let s = |raw: &str| target(&SANKAKU_API, &Url::parse(raw).unwrap());
        let i = |raw: &str| target(&IDOLCOMPLEX_API, &Url::parse(raw).unwrap());
        assert_eq!(
            s("https://sankaku.app/?tags=bonocho"),
            Some(Target::Tag("tags=bonocho".into()))
        );
        assert_eq!(
            s("https://chan.sankakucomplex.com/en/posts?tags=a+b"),
            Some(Target::Tag("tags=a+b".into()))
        );
        assert_eq!(
            s("https://beta.sankakucomplex.com/?"),
            Some(Target::Tag(String::new()))
        );
        assert_eq!(
            s("https://sankaku.app/books/90"),
            Some(Target::Pool("90".into()))
        );
        assert_eq!(
            s("https://sankaku.app/pt-BR/pools/show/90"),
            Some(Target::Pool("90".into()))
        );
        assert_eq!(
            s("https://sankaku.app/posts/y0abGlDOr2o"),
            Some(Target::Post("y0abGlDOr2o".into()))
        );
        assert_eq!(
            s("https://black.sankakucomplex.com/post/show/360451"),
            Some(Target::Post("360451".into()))
        );
        assert_eq!(
            s("https://sankaku.app/books?tags=aiue"),
            Some(Target::Books("tags=aiue".into()))
        );
        assert_eq!(
            i("https://www.idolcomplex.com/en/posts?tags=x"),
            Some(Target::Tag("tags=x".into()))
        );
        assert_eq!(
            i("https://idol.sankakucomplex.com/pools/abc"),
            Some(Target::Pool("abc".into()))
        );
        assert_eq!(
            i("https://www.idolcomplex.com/en/posts/0123abc"),
            Some(Target::Post("0123abc".into()))
        );
        assert_eq!(i("https://www.idolcomplex.com/books?tags=x"), None);
        for raw in [
            "https://sankaku.app/",
            "https://sankaku.app/books/",
            "https://idol.sankakucomplex.com/posts/1",
            "https://sankaku.app.evil.com/posts/1",
            "https://sankaku.app:8080/posts/1",
            "https://u:p@sankaku.app/posts/1",
        ] {
            assert_eq!(s(raw), None, "{raw}");
        }
        let find = |raw: &str| crate::sites::find(&Url::parse(raw).unwrap()).map(|s| s.category);
        assert_eq!(
            find("https://www.idolcomplex.com/posts/1"),
            Some("idolcomplex")
        );
        assert_eq!(find("https://sankaku.app/posts/1"), Some("sankaku"));
    }

    #[test]
    fn rewrites_search_tags() {
        assert_eq!(
            rewrite_dates("a date:01.02.2020 date:2020-02-03 date:2020-02-03T10:00"),
            "a date:2020-02-01T00:00 date:2020-02-03T00:00 date:2020-02-03T10:00"
        );
        assert_eq!(
            parent_query("x parent:123 myparent:1"),
            Some("x id_range:123 myparent:1".into())
        );
        assert_eq!(parent_query("parent: x"), None);
        assert_eq!(search_tags("tags=a%2Bb+c&x=1"), "a b c");
    }

    #[test]
    fn extended_tags() {
        let page = "<ul id=\"tag-sidebar\">\n<li class=\"tag-type-artist\"><a href=\"/?tags=some%20one&x\">x</a></li>\n\
            <li class=\"tag-type-general\"><a href=\"/?tags=a_b\">x</a></li>\n<li class=\"tag-type-artist\"><a href='/?tags=b&amp;c'></a></li></ul>";
        assert_eq!(
            sidebar_tags(page),
            [
                ("artist".into(), vec!["some one".to_owned(), "b".to_owned()]),
                ("general".into(), vec!["a_b".to_owned()])
            ]
        );
    }

    fn post(id: &str, url: &str) -> Value {
        json!({"id": id, "md5": format!("md5{id}"), "file_url": url, "status": "active",
            "author": {"name": "uploader"}, "has_notes": true,
            "created_at": {"s": 1500000000, "n": 0, "json_class": "Time"},
            "tags": [{"id": 1}], "tag_names": ["a_b", "c"]})
    }

    #[tokio::test]
    async fn paginates_tag_search_with_categories_and_notes() {
        let first = json!({"meta": {"next": "abc"}, "data": [
            post("p1", "http://s.sankakucomplex.com/data/aa/bb/file.jpg?e=1"),
            post("p2", "")]});
        let second = json!({"meta": {"next": null}, "data": []});
        let tags = json!({"total": 2, "data": [
            {"name": "Some One", "type": 1}, {"name": "flower", "type": 0}]});
        let notes = json!([{"id": 9, "created_at": {"s": 1}, "updated_at": {"s": 2}}]);
        let (root, server) = serve(vec![json(first), json(tags), json(notes), json(second)]);
        let http = http("Sankaku", root).with_options(Some(std::sync::Arc::new(
            json!({"tags": true, "notes": true})
                .as_object()
                .unwrap()
                .clone(),
        )));
        let files = media(extract(
            &SANKAKU_API,
            &http,
            Url::parse("https://sankaku.app/?tags=a+date:01.02.2020").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].url.as_str(),
            "https://s.sankakucomplex.com/data/aa/bb/file.jpg?e=1"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["search_tags"], "a date:2020-02-01T00:00");
        assert_eq!(k["tags"], json!(["a_b", "c"]));
        assert_eq!(k["tag_string"], "a_b c");
        assert_eq!(k["tags_artist"], json!(["some_one"]));
        assert_eq!(k["tag_string_general"], "flower");
        assert_eq!(
            k["notes"],
            json!([{"id": 9, "created_at": 1, "updated_at": 2}])
        );
        assert_eq!(k["created_at"], 1500000000);
        assert_eq!(k["date"], "2017-07-14 02:40:00");
        assert_eq!(k["filename"], "file");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "sankaku_p1_md5p1.jpg");
        let colon = if cfg!(windows) { "_" } else { ":" };
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from(format!("sankaku/a date{colon}2020-02-01T00{colon}00"))
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(
            identity,
            r#"["t_{search_tags}_{id}",["a date:2020-02-01T00:00","p1"]]"#
        );
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with(
                "GET /v2/posts/keyset?tags=a+date%3A2020-02-01T00%3A00&lang=en&limit=100 "
            ),
            "{}",
            requests[0]
        );
        assert!(requests[0].contains("accept: application/vnd.sankaku.api+json;v=2"));
        assert!(requests[0].contains("origin: https://sankaku.app"));
        assert!(requests[1].starts_with("GET /posts/p1/tags?lang=en&page=1&limit=100 "));
        assert!(requests[2].starts_with("GET /posts/p1/notes?lang=en "));
        assert!(requests[3].contains("&limit=100&next=abc "));
    }

    #[tokio::test]
    async fn idolcomplex_pools() {
        let pool = json!({"id": "pid", "name_en": "My Pool", "tags": [{"name": "t"}],
            "artist_tags": [], "posts": [post("p1", "https://is.sankakucomplex.com/x/y.png")]});
        let (root, server) = serve(vec![json(pool)]);
        let http = http("Idol Complex", root);
        let files = media(extract(
            &IDOLCOMPLEX_API,
            &http,
            Url::parse("https://www.idolcomplex.com/en/pools/pid").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let k = keywords(&files[0]);
        assert_eq!(k["category"], "idolcomplex");
        assert_eq!(k["num"], 1);
        assert_eq!(k["pool"]["tags"], json!(["t"]));
        assert!(k["pool"].get("posts").is_none());
        let (_, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("idolcomplex/pool/pid My Pool")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /pools/pid?lang=en "));
        assert!(requests[0].contains("origin: https://www.idolcomplex.com"));
    }

    #[tokio::test]
    async fn posts_and_books() {
        let (root, server) = serve(vec![
            json(json!([post("p1", "https://s.sankakucomplex.com/a.jpg")])),
            json(json!({"meta": {"next": null}, "data": [{"id": "b1", "name_en": "Book"}]})),
        ]);
        let http = http("Sankaku", root);
        let files = media(extract(
            &SANKAKU_API,
            &http,
            Url::parse("https://sankaku.app/post/show/0123456789abcdef0123456789abcdef").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{id}",["p1"]]"#);
        let children = queued(extract(
            &SANKAKU_API,
            &http,
            Url::parse("https://sankaku.app/books?tags=x").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].subcategory(), "books");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with(
            "GET /v2/posts?lang=en&page=1&limit=1&tags=md5%3A0123456789abcdef0123456789abcdef "
        ));
        assert!(requests[1].starts_with("GET /pools/keyset?tags=x&pool_type=0&lang=en&limit=100 "));
    }
}
