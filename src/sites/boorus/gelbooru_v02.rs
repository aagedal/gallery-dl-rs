//! Gelbooru Beta 0.2 sites (rule34, tbib, hypnohub, xbooru), mirroring
//! gallery_dl/extractor/gelbooru_v02.py through the XML `dapi` endpoint.
use crate::sites::boorus::{
    Extr, booru_fields, date_value, extract_iter, leading_digits, unquote_plus, until,
};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};
use url::Url;

const PER_PAGE: u64 = 100;
const FILENAME: &str = "{category}_{id}_{md5}.{extension}";
static TAG: Route = Route::new(
    "tag",
    FILENAME,
    &["{category}", "{search_tags}"],
    "t_{search_tags}_{id}",
);
static POOL: Route = Route::new(
    "pool",
    FILENAME,
    &["{category}", "pool", "{pool}"],
    "p_{pool}_{id}",
);
static FAVORITE: Route = Route::new(
    "favorite",
    FILENAME,
    &["{category}", "favorites", "{favorite_id}"],
    "f_{favorite_id}_{id}",
);
static POST: Route = Route::new("post", FILENAME, &["{category}"], "{id}");

macro_rules! instance {
    ($ident:ident, $category:literal, $name:literal) => {
        pub static $ident: Site = Site {
            category: $category,
            name: $name,
            matches: |url| target($category, url).is_some(),
            extract: |http, url| extract($category, http, url),
            routes: &[&TAG, &POOL, &FAVORITE, &POST],
            api_key: true,
        };
    };
}
instance!(RULE34, "rule34", "Rule 34");
instance!(TBIB, "tbib", "The Big ImageBoard");
instance!(HYPNOHUB, "hypnohub", "Hypnohub");
instance!(XBOORU, "xbooru", "Xbooru");

fn site(category: &str) -> &'static Site {
    match category {
        "rule34" => &RULE34,
        "tbib" => &TBIB,
        "hypnohub" => &HYPNOHUB,
        _ => &XBOORU,
    }
}

fn root(category: &str) -> &'static str {
    match category {
        "rule34" => "https://rule34.xxx",
        "tbib" => "https://tbib.org",
        "hypnohub" => "https://hypnohub.net",
        _ => "https://xbooru.com",
    }
}

/// Instance `root-api`, falling back to `root`.
fn root_api(category: &str) -> &'static str {
    match category {
        "rule34" => "https://api.rule34.xxx",
        other => root(other),
    }
}

/// Instance `request-interval`.
fn interval(category: &str) -> Duration {
    match category {
        "rule34" => Duration::from_secs(1),
        _ => Duration::ZERO,
    }
}

fn hosts(category: &str) -> &'static [&'static str] {
    match category {
        "rule34" => &["rule34.xxx", "www.rule34.xxx"],
        "tbib" => &["tbib.org"],
        "hypnohub" => &["hypnohub.net"],
        _ => &["xbooru.com"],
    }
}

#[derive(Debug, PartialEq)]
enum Target {
    /// Raw (still quoted) `tags=` value.
    Tag(String),
    Pool(String),
    Favorite(String),
    Post(String),
}

fn target(category: &str, url: &Url) -> Option<Target> {
    let rest = crate::sites::boorus::below(url, hosts(category))?;
    let id = |prefix: &str| {
        rest.strip_prefix(prefix)
            .and_then(leading_digits)
            .map(str::to_owned)
    };
    if let Some(tags) = rest.strip_prefix("/index.php?page=post&s=list&tags=") {
        return Some(Target::Tag(until(tags, &['&', '#']).to_owned()));
    }
    if let Some(id) = id("/index.php?page=pool&s=show&id=") {
        return Some(Target::Pool(id));
    }
    if let Some(id) = id("/index.php?page=favorites&s=view&id=") {
        return Some(Target::Favorite(id));
    }
    id("/index.php?page=post&s=view&id=").map(Target::Post)
}

/// Per-extraction request state: the instance and its request interval.
struct Ctx<'a> {
    category: &'static str,
    http: &'a Http,
    last: std::sync::Mutex<Option<Instant>>,
}

/// One parsed `dapi` response.
enum Xml {
    Page {
        count: Option<i64>,
        posts: Vec<Map<String, Value>>,
    },
    /// The response was not well-formed XML (`ElementTree.ParseError`).
    Invalid,
}

impl<'a> Ctx<'a> {
    fn new(category: &'static str, http: &'a Http) -> Arc<Self> {
        Arc::new(Self {
            category,
            http,
            last: std::sync::Mutex::new(None),
        })
    }

    /// Honor the instance `request-interval` between requests.
    async fn wait(&self) {
        let interval = interval(self.category);
        if interval.is_zero() {
            return;
        }
        let delay = {
            let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
            let now = Instant::now();
            let start = last.map_or(now, |t| (t + interval).max(now));
            *last = Some(start);
            start - now
        };
        tokio::time::sleep(delay).await;
    }

    async fn html(&self, url: &Url) -> Result<String> {
        self.wait().await;
        self.http.text(url).await
    }

    /// `_api_request`: `params` followed by `api_key` and `user_id`.
    async fn api(&self, params: &[(&str, String)]) -> Result<Xml> {
        let mut url = self.http.api(&format!(
            "{}/index.php?page=dapi&s=post&q=index",
            root_api(self.category)
        ))?;
        {
            let mut pairs = url.query_pairs_mut();
            for (key, value) in params {
                pairs.append_pair(key, value);
            }
            let key = self
                .http
                .api_key()
                .or_else(|| self.http.option_str("api-key"));
            if let Some(key) = key {
                pairs.append_pair("api_key", key);
            }
            if let Some(user) = self.http.option("user-id").filter(|v| !v.is_null()) {
                pairs.append_pair("user_id", &crate::sites::scalar(user));
            }
        }
        self.wait().await;
        let body = self.http.text(&url).await?;
        parse_xml(self.http.name(), &body)
    }
}

fn parse_xml(name: &str, body: &str) -> Result<Xml> {
    let Ok(doc) = roxmltree::Document::parse(body) else {
        return Ok(Xml::Invalid);
    };
    let root = doc.root_element();
    if root.tag_name().name() == "error" {
        let msg = root.text().unwrap_or_default().trim();
        if msg.to_lowercase().starts_with("missing authentication") {
            return Err(error(format!(
                "{name} API requires 'api-key' & 'user-id' authentication"
            )));
        }
        let msg: String = msg.chars().take(200).collect();
        return Err(error(format!("{name} API error: '{msg}'")));
    }
    let count = root
        .attribute("count")
        .and_then(|c| c.trim().parse::<i64>().ok());
    let posts = root
        .children()
        .filter(roxmltree::Node::is_element)
        .map(|post| {
            post.attributes()
                .map(|a| (a.name().to_owned(), Value::String(a.value().to_owned())))
                .collect()
        })
        .collect();
    Ok(Xml::Page { count, posts })
}

fn extract<'a>(category: &'static str, http: &'a Http, url: Url) -> Events<'a> {
    let Some(target) = target(category, &url) else {
        return crate::sites::fail(error("unsupported Gelbooru v0.2 URL"));
    };
    let ctx = Ctx::new(category, http);
    match target {
        Target::Tag(raw) => {
            let tags = unquote_plus(&raw);
            let mut data = Map::new();
            data.insert("search_tags".into(), tags.clone().into());
            let tags = if tags == "all" { String::new() } else { tags };
            api_pagination(ctx, &TAG, ("tags", tags), data)
        }
        Target::Post(id) => api_pagination(ctx, &POST, ("id", id), Map::new()),
        Target::Favorite(id) => {
            let mut data = Map::new();
            data.insert("favorite_id".into(), crate::sites::boorus::int_value(&id));
            html_pagination(ctx, &FAVORITE, "favorites", "view", id, 50, data)
        }
        Target::Pool(id) => pool(ctx, id),
    }
}

/// Files of one API page, one post boundary per post.
fn posts_events(
    ctx: &Ctx<'_>,
    route: &'static Route,
    posts: Vec<Map<String, Value>>,
    data: &Map<String, Value>,
) -> Events<'static> {
    let items = posts
        .into_iter()
        .filter_map(|post| parse_post(ctx.category, post, data))
        .collect();
    crate::sites::events(crate::sites::boorus::per_post(
        site(ctx.category),
        ctx.http,
        route,
        items,
    ))
}

struct ApiState {
    key: &'static str,
    value: String,
    pid: u64,
    count: u64,
    total: Option<i64>,
    last_id: Option<String>,
    retries: u8,
    done: bool,
    data: Map<String, Value>,
}

/// `_pagination`: `pid`-numbered API pages; `total`/`search_count` come from
/// the first response's `count`.
fn api_pagination<'a>(
    ctx: Arc<Ctx<'a>>,
    route: &'static Route,
    (key, value): (&'static str, String),
    data: Map<String, Value>,
) -> Events<'a> {
    let state = ApiState {
        key,
        value,
        pid: 0,
        count: 0,
        total: None,
        last_id: None,
        retries: 0,
        done: false,
        data,
    };
    crate::sites::paginate(state, move |mut state| {
        let ctx = ctx.clone();
        async move {
            if state.done {
                return Ok(None);
            }
            loop {
                let params = [
                    (state.key, state.value.clone()),
                    ("pid", state.pid.to_string()),
                    ("limit", PER_PAGE.to_string()),
                ];
                let (count, posts) = match ctx.api(&params).await? {
                    Xml::Page { count, posts } => (count, posts),
                    Xml::Invalid => {
                        let Some(last) = state.last_id.take().filter(|_| state.key == "tags")
                        else {
                            return Err(error(format!("{} returned invalid XML", ctx.http.name())));
                        };
                        let mut tags: Vec<&str> = state
                            .value
                            .split_whitespace()
                            .filter(|t| !t.starts_with("id:<"))
                            .collect();
                        let bound = format!("id:<{last}");
                        tags.push(&bound);
                        state.value = tags.join(" ");
                        state.pid = 0;
                        continue;
                    }
                };
                if state.total.is_none() {
                    state.total = Some(count.unwrap_or(0));
                    if let Some(total) = count {
                        state.data.insert("total".into(), total.into());
                        if state.data.contains_key("search_tags") {
                            state.data.insert("search_count".into(), total.into());
                        }
                    }
                }
                state.last_id = posts
                    .last()
                    .and_then(|p| p.get("id"))
                    .map(crate::sites::scalar);
                let num = posts.len() as u64;
                state.count += num;
                let events = posts_events(&ctx, route, posts, &state.data);
                if num < PER_PAGE {
                    let total = state.total.unwrap_or(0);
                    if total <= 0 || state.count as i64 >= total {
                        state.done = true;
                        return Ok(Some((events, state)));
                    }
                    if num == 0 {
                        // Upstream retries empty pages indefinitely.
                        state.retries += 1;
                        if state.retries > 3 {
                            return Ok(None);
                        }
                        continue;
                    }
                }
                state.retries = 0;
                state.pid += 1;
                return Ok(Some((events, state)));
            }
        }
    })
}

struct HtmlState {
    ids: VecDeque<String>,
    pid: Option<u64>,
}

/// `_pagination_html`: post ids from HTML list pages, then one API request
/// per id.
fn html_pagination<'a>(
    ctx: Arc<Ctx<'a>>,
    route: &'static Route,
    page: &'static str,
    s: &'static str,
    id: String,
    per_page: u64,
    data: Map<String, Value>,
) -> Events<'a> {
    let state = HtmlState {
        ids: VecDeque::new(),
        pid: Some(0),
    };
    crate::sites::paginate(state, move |mut state| {
        let ctx = ctx.clone();
        let id = id.clone();
        let data = data.clone();
        async move {
            if let Some(post_id) = state.ids.pop_front() {
                let events = api_posts(&ctx, route, post_id, &data).await?;
                return Ok(Some((events, state)));
            }
            let Some(pid) = state.pid else {
                return Ok(None);
            };
            let mut url = ctx.http.api(&format!("{}/index.php", root(ctx.category)))?;
            url.query_pairs_mut()
                .append_pair("page", page)
                .append_pair("s", s)
                .append_pair("id", &id)
                .append_pair("pid", &pid.to_string());
            let html = ctx.html(&url).await?;
            let ids = find_ids(&html);
            state.pid = (ids.len() as u64 >= per_page).then_some(pid + per_page);
            state.ids = ids.into();
            Ok(Some((crate::sites::events(Vec::new()), state)))
        }
    })
}

/// All posts the API returns for one post id.
async fn api_posts(
    ctx: &Ctx<'_>,
    route: &'static Route,
    id: String,
    data: &Map<String, Value>,
) -> Result<Events<'static>> {
    match ctx.api(&[("id", id)]).await? {
        Xml::Page { posts, .. } => Ok(posts_events(ctx, route, posts, data)),
        Xml::Invalid => Err(error(format!("{} returned invalid XML", ctx.http.name()))),
    }
}

/// `re.findall(r"\sid=\"p(\d+)", page)`.
fn find_ids(page: &str) -> Vec<String> {
    let mut ids = Vec::new();
    let mut pos = 0;
    while let Some(found) = page[pos..].find("id=\"p") {
        let start = pos + found;
        pos = start + 5;
        let preceded = page[..start]
            .chars()
            .next_back()
            .is_some_and(char::is_whitespace);
        let digits = page[pos..].bytes().take_while(u8::is_ascii_digit).count();
        if preceded && digits > 0 {
            ids.push(page[pos..pos + digits].to_owned());
        }
    }
    ids
}

/// Pool metadata from the pool page, then its posts.
fn pool(ctx: Arc<Ctx<'_>>, id: String) -> Events<'_> {
    crate::sites::once(async move {
        let mut url = ctx.http.api(&format!("{}/index.php", root(ctx.category)))?;
        url.query_pairs_mut()
            .append_pair("page", "pool")
            .append_pair("s", "show")
            .append_pair("id", &id);
        let page = ctx.html(&url).await?;
        let mut extr = Extr::new(&page);
        let name = extr.next("<h4>Pool: ", "</h4>");
        if name.is_empty() {
            return Err(error(format!("{} pool not found", ctx.http.name())));
        }
        let mut data = Map::new();
        data.insert("pool".into(), crate::sites::boorus::int_value(&id));
        data.insert("pool_name".into(), crate::sites::unescape(name).into());
        if ctx.category == "rule34" {
            return Ok(html_pagination(ctx, &POOL, "pool", "show", id, 45, data));
        }
        let ids: VecDeque<String> = extract_iter(&page[extr.pos..], "class=\"thumb\" id=\"p", "\"")
            .into_iter()
            .map(str::to_owned)
            .collect();
        Ok(crate::sites::paginate(ids, move |mut ids| {
            let ctx = ctx.clone();
            let data = data.clone();
            async move {
                let Some(post_id) = ids.pop_front() else {
                    return Ok(None);
                };
                let events = api_posts(&ctx, &POOL, post_id, &data).await?;
                Ok(Some((events, ids)))
            }
        }))
    })
}

/// `text.ext_from_url`.
fn ext_from_url(url: &str) -> String {
    let path = until(url, &['?', '#']);
    let name = path.rsplit('/').next().unwrap_or_default();
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => ext.to_lowercase(),
        _ => String::new(),
    }
}

const EXTS_VIDEO: &[&str] = &[
    "mp4", "m4v", "mov", "webm", "mkv", "ogv", "flv", "avi", "wmv",
];

/// `dt.parse(created_at, "%a %b %d %H:%M:%S %z %Y")` as Unix seconds.
fn parse_created(text: &str) -> Option<i64> {
    let format = time::format_description::parse_borrowed::<2>(
        "[weekday repr:short] [month repr:short] [day padding:none] [hour]:[minute]:[second] \
         [offset_hour sign:mandatory][offset_minute] [year]",
    )
    .ok()?;
    time::OffsetDateTime::parse(text, &format)
        .ok()
        .map(time::OffsetDateTime::unix_timestamp)
}

/// `BooruExtractor.items` for one API post, with `_file_url_rule34` and
/// `GelbooruV02Extractor._prepare`.
fn parse_post(
    category: &str,
    mut fields: Map<String, Value>,
    data: &Map<String, Value>,
) -> Option<Item> {
    let mut raw = fields.get("file_url")?.as_str()?.to_owned();
    if category == "rule34" && !EXTS_VIDEO.contains(&ext_from_url(&raw).as_str()) {
        let path = raw.split_once('.').map_or("", |(_, path)| path);
        raw = format!("https://wimg.{path}");
        fields.insert("file_url".into(), raw.clone().into());
    }
    if raw.is_empty() {
        return None;
    }
    if raw.starts_with('/') {
        raw = format!("{}{raw}", root(category));
    }
    let url = Url::parse(&raw).ok().filter(crate::sites::plain)?;
    booru_fields(&url, &mut fields, data);
    let tags = fields
        .get("tags")
        .and_then(Value::as_str)
        .map(|t| t.trim().to_owned());
    if let Some(tags) = &tags {
        fields.insert("tags".into(), tags.clone().into());
    }
    let timestamp = fields
        .get("created_at")
        .and_then(Value::as_str)
        .and_then(parse_created);
    fields.insert("date".into(), date_value(timestamp));
    let id = fields
        .get("id")
        .map(crate::sites::scalar)
        .unwrap_or_default();
    Some(
        Item::new(url, fields)
            .ids(id.clone(), id)
            .caption(tags)
            .timestamp(timestamp),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http, keywords, media, response, serve};

    fn t(category: &str, raw: &str) -> Option<Target> {
        target(category, &Url::parse(raw).unwrap())
    }

    fn xml(body: &str) -> Vec<u8> {
        response("200 OK", "text/xml", body.as_bytes())
    }

    fn page(body: &str) -> Vec<u8> {
        response("200 OK", "text/html", body.as_bytes())
    }

    fn post_xml(id: u64, ext: &str, host: &str) -> String {
        format!(
            r#"<post id="{id}" md5="m{id}" tags=" a b " created_at="Fri Jul 14 04:40:00 +0200 2017" file_url="https://{host}/images/1/f{id}.{ext}" rating="s" width="10"/>"#
        )
    }

    fn posts_xml(count: u64, ids: impl Iterator<Item = u64>) -> String {
        let posts: String = ids.map(|id| post_xml(id, "png", "img.tbib.org")).collect();
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><posts count="{count}" offset="0">{posts}</posts>"#
        )
    }

    #[test]
    fn classifies_urls() {
        assert_eq!(
            t(
                "tbib",
                "https://tbib.org/index.php?page=post&s=list&tags=a+b%3F&pid=40"
            ),
            Some(Target::Tag("a+b%3F".into()))
        );
        assert_eq!(
            t(
                "rule34",
                "https://www.rule34.xxx/index.php?page=post&s=list&tags="
            ),
            Some(Target::Tag(String::new()))
        );
        assert_eq!(
            t(
                "hypnohub",
                "https://hypnohub.net/index.php?page=pool&s=show&id=61"
            ),
            Some(Target::Pool("61".into()))
        );
        assert_eq!(
            t(
                "xbooru",
                "https://xbooru.com/index.php?page=favorites&s=view&id=45"
            ),
            Some(Target::Favorite("45".into()))
        );
        assert_eq!(
            t(
                "xbooru",
                "https://xbooru.com/index.php?page=post&s=view&id=1025649x"
            ),
            Some(Target::Post("1025649".into()))
        );
        // Query order is significant, like the upstream regex.
        assert_eq!(
            t("tbib", "https://tbib.org/index.php?s=view&page=post&id=1"),
            None
        );
        assert_eq!(
            t(
                "tbib",
                "https://www.tbib.org/index.php?page=post&s=view&id=1"
            ),
            None
        );
        assert_eq!(
            t(
                "tbib",
                "https://tbib.org.evil.test/index.php?page=post&s=view&id=1"
            ),
            None
        );
        assert_eq!(
            t(
                "tbib",
                "https://tbib.org:444/index.php?page=post&s=view&id=1"
            ),
            None
        );
        assert_eq!(
            t("tbib", "https://tbib.org/index.php?page=post&s=view&id=x"),
            None
        );
        assert_eq!(t("tbib", "https://tbib.org/post/1"), None);
        // safebooru belongs to another module.
        assert!(
            crate::sites::find(
                &Url::parse("https://safebooru.org/index.php?page=post&s=view&id=1").unwrap()
            )
            .is_none_or(|site| site.category != "tbib")
        );
        assert!(
            crate::sites::find(
                &Url::parse("https://rule34.xxx/index.php?page=post&s=view&id=1").unwrap()
            )
            .is_some_and(|site| site.category == "rule34")
        );
    }

    #[test]
    fn parses_ids_and_urls() {
        assert_eq!(
            find_ids("<span\nid=\"p12\"><a id=\"p34\">x id=\"p\" <b\tid=\"p5x\">"),
            vec!["12", "34", "5"]
        );
        assert_eq!(ext_from_url("https://a.test/x/y.MP4?z"), "mp4");
        assert_eq!(ext_from_url("https://a.test/x/y"), "");
        let mut post = Map::new();
        post.insert(
            "file_url".into(),
            "https://api-cdn.rule34.xxx/images/1/a.jpeg".into(),
        );
        post.insert("id".into(), "1".into());
        let item = parse_post("rule34", post.clone(), &Map::new()).unwrap();
        assert_eq!(item.url.as_str(), "https://wimg.rule34.xxx/images/1/a.jpeg");
        assert_eq!(
            item.keywords["file_url"],
            "https://wimg.rule34.xxx/images/1/a.jpeg"
        );
        assert_eq!(item.keywords["date"], "[Invalid DateTime]");
        post.insert(
            "file_url".into(),
            "https://api-cdn.rule34.xxx/images/1/a.webm".into(),
        );
        let item = parse_post("rule34", post.clone(), &Map::new()).unwrap();
        assert_eq!(
            item.url.as_str(),
            "https://api-cdn.rule34.xxx/images/1/a.webm"
        );
        post.insert("file_url".into(), "".into());
        assert!(parse_post("tbib", post, &Map::new()).is_none());
        assert!(matches!(
            parse_xml("tbib", "<posts count=\"1\"><post id=\"1\""),
            Ok(Xml::Invalid)
        ));
        let err = parse_xml(
            "rule34",
            "<error>Missing authentication. Go to api.rule34.xxx</error>",
        )
        .err()
        .unwrap()
        .to_string();
        assert!(err.contains("'api-key' & 'user-id'"), "{err}");
    }

    #[tokio::test]
    async fn paginates_tag_search_with_totals() {
        let (root, server) = serve(vec![
            xml(&posts_xml(150, 1..=100)),
            xml(&posts_xml(150, 101..=150)),
        ]);
        let http = http("tbib", root).with_api_key(Some("KEY"));
        let files = media(extract(
            "tbib",
            &http,
            Url::parse("https://tbib.org/index.php?page=post&s=list&tags=a+b%21").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 150);
        let k = keywords(&files[0]);
        assert_eq!(k["search_tags"], "a b!");
        assert_eq!(k["total"], 150);
        assert_eq!(k["search_count"], 150);
        assert_eq!(k["id"], "1");
        assert_eq!(k["tags"], "a b");
        assert_eq!(k["width"], "10");
        assert_eq!(k["date"], "2017-07-14 02:40:00");
        assert_eq!(
            files[0].metadata.as_ref().unwrap().timestamp,
            Some(1_500_000_000)
        );
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "tbib_1_m1.png");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("tbib/a b!")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["t_{search_tags}_{id}",["a b!","1"]]"#);
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with(
                "GET /index.php?page=dapi&s=post&q=index&tags=a+b%21&pid=0&limit=100&api_key=KEY "
            ),
            "{}",
            requests[0]
        );
        assert!(requests[1].contains("&pid=1&limit=100"));
    }

    #[tokio::test]
    async fn invalid_xml_restarts_below_last_id() {
        let (root, server) = serve(vec![
            xml(&posts_xml(110, 1..=100)),
            xml("<posts"),
            xml(&posts_xml(110, 101..=110)),
        ]);
        let http = http("xbooru", root);
        let files = media(extract(
            "xbooru",
            &http,
            Url::parse("https://xbooru.com/index.php?page=post&s=list&tags=all").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 110);
        assert_eq!(keywords(&files[0])["search_tags"], "all");
        let requests = server.join().unwrap();
        assert!(requests[0].contains("&tags=&pid=0&"), "{}", requests[0]);
        assert!(requests[1].contains("&tags=&pid=1&"));
        assert!(
            requests[2].contains("&tags=id%3A%3C100&pid=0&"),
            "{}",
            requests[2]
        );
    }

    #[tokio::test]
    async fn post_and_pool() {
        let (root, server) = serve(vec![xml(&posts_xml(1, 7..=7))]);
        let http = http("hypnohub", root);
        let files = media(extract(
            "hypnohub",
            &http,
            Url::parse("https://hypnohub.net/index.php?page=post&s=view&id=7").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let k = keywords(&files[0]);
        assert_eq!(k["total"], 1);
        assert!(!k.contains_key("search_count"));
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "hypnohub_7_m7.png");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("hypnohub")
        );
        assert_eq!(
            crate::sites::archive_key(&files[0]).unwrap().1,
            r#"["{id}",["7"]]"#
        );
        let requests = server.join().unwrap();
        assert!(
            requests[0]
                .starts_with("GET /index.php?page=dapi&s=post&q=index&id=7&pid=0&limit=100 ")
        );

        let pool_page = r#"<h4>Pool: Tom &amp; Jerry</h4>
            <span class="thumb" id="p3"></span><span class="thumb" id="p4"></span>"#;
        let (root, server) = serve(vec![
            page(pool_page),
            xml(&posts_xml(1, 3..=3)),
            xml(&posts_xml(1, 4..=4)),
        ]);
        let http = crate::sites::testing::http("hypnohub", root);
        let files = media(extract(
            "hypnohub",
            &http,
            Url::parse("https://hypnohub.net/index.php?page=pool&s=show&id=61").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[1]);
        assert_eq!(k["pool"], 61);
        assert_eq!(k["pool_name"], "Tom & Jerry");
        assert!(!k.contains_key("total"));
        let (_, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("hypnohub/pool/61")
        );
        assert_eq!(
            crate::sites::archive_key(&files[1]).unwrap().1,
            r#"["p_{pool}_{id}",["61","4"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /index.php?page=pool&s=show&id=61 "));
        assert!(requests[1].starts_with("GET /index.php?page=dapi&s=post&q=index&id=3 "));
        assert!(requests[2].starts_with("GET /index.php?page=dapi&s=post&q=index&id=4 "));
    }

    #[tokio::test]
    async fn favorites_paginate_html() {
        let ids: String = (1..=50)
            .map(|i| format!("<span id=\"p{i}\" class=\"thumb\"></span>\n"))
            .collect();
        let mut replies = vec![page(&ids)];
        replies.extend((1..=50).map(|i| xml(&posts_xml(1, i..=i))));
        replies.push(page("<span id=\"p51\"></span>"));
        replies.push(xml(&posts_xml(1, 51..=51)));
        let (root, server) = serve(replies);
        let http = http("xbooru", root);
        let files = media(extract(
            "xbooru",
            &http,
            Url::parse("https://xbooru.com/index.php?page=favorites&s=view&id=45").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 51);
        let k = keywords(&files[0]);
        assert_eq!(k["favorite_id"], 45);
        let (_, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("xbooru/favorites/45")
        );
        assert_eq!(
            crate::sites::archive_key(&files[0]).unwrap().1,
            r#"["f_{favorite_id}_{id}",["45","1"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /index.php?page=favorites&s=view&id=45&pid=0 "));
        assert!(requests[51].starts_with("GET /index.php?page=favorites&s=view&id=45&pid=50 "));
    }

    #[tokio::test]
    async fn rule34_pool_uses_html_pages_and_api_root() {
        let pool_page = r#"<h4>Pool: P</h4><span class="thumb" id="p8"></span>"#;
        let post = format!(
            r#"<posts count="1">{}</posts>"#,
            post_xml(8, "jpg", "api-cdn.rule34.xxx")
        );
        let (root, server) = serve(vec![page(pool_page), page(pool_page), xml(&post)]);
        let http = http("rule34", root);
        let files = media(extract(
            "rule34",
            &http,
            Url::parse("https://rule34.xxx/index.php?page=pool&s=show&id=5").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].url.as_str(),
            "https://wimg.rule34.xxx/images/1/f8.jpg"
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /index.php?page=pool&s=show&id=5 "));
        assert!(requests[1].starts_with("GET /index.php?page=pool&s=show&id=5&pid=0 "));
        assert!(requests[2].starts_with("GET /index.php?page=dapi&s=post&q=index&id=8 "));
        assert!(requests[2].to_ascii_lowercase().contains("host: 127.0.0.1"));
    }

    #[tokio::test]
    async fn api_errors_and_missing_pools_fail() {
        let (root, server) = serve(vec![xml("<error>Missing authentication</error>")]);
        let http = http("tbib", root);
        let events = extract(
            "tbib",
            &http,
            Url::parse("https://tbib.org/index.php?page=post&s=view&id=1").unwrap(),
        );
        assert!(crate::sites::testing::fails(events).await);
        server.join().unwrap();

        let (root, server) = serve(vec![page("<html>no pool</html>")]);
        let http = crate::sites::testing::http("tbib", root);
        let events = extract(
            "tbib",
            &http,
            Url::parse("https://tbib.org/index.php?page=pool&s=show&id=1").unwrap(),
        );
        assert!(crate::sites::testing::fails(events).await);
        server.join().unwrap();
    }
}
