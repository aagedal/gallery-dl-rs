//! XenForo forums (SimpCity, NudoStar, AllTheFallen, CelebForum,
//! TitsInTops, SocialMediaGirls, BlackToWhite, and `xenforo:` URLs),
//! mirroring gallery_dl/extractor/xenforo.py: post attachments and inline
//! images are downloaded, other links and embeds are queued.
//!
//! Options: `embeds`, `quoted`, `attachments`, `order-posts`, `metadata`,
//! and `username`/`password` unless session cookies are loaded.
use crate::error;
use crate::sites::gdl::{
    Scan, auth_info, date, extr, extract_iter, jsonld, nameext_from_name, nameext_into, parse_int,
    parse_iso, parse_query, partition, re, rpartition, slugify, str_of, unescape, unquote,
};
use crate::sites::{Events, Http, Item, Route, Site};
use base64::Engine;
use futures_util::StreamExt;
use serde_json::{Map, Value};
use url::Url;

const FILENAME: &str = "{post[id]}_{num:>02}{id:?_//}_{filename}.{extension}";
const DIRECTORY: &[&str] = &[
    "{category}",
    "{thread[section]}",
    "{thread[title]} ({thread[id]})",
];
const ARCHIVE: &str = "{post[id]}/{type[0]}{id}_{filename}";

static POST: Route = Route::new("post", FILENAME, DIRECTORY, ARCHIVE);
static THREAD: Route = Route::new("thread", FILENAME, DIRECTORY, ARCHIVE);
static FORUM: Route = Route::new("forum", FILENAME, DIRECTORY, ARCHIVE);
static MEDIA_USER: Route = Route::new(
    "media-user",
    "{filename}.{extension}",
    &["{category}", "Media", "{author_slug}"],
    "{id}",
);
static MEDIA_ALBUM: Route = Route::new(
    "media-album",
    "{filename}.{extension}",
    &[
        "{category}",
        "Media",
        "Albums",
        "{album[slug]} ({album[id]})",
    ],
    "{id}",
);
static MEDIA_CATEGORY: Route = Route::new(
    "media-category",
    "{filename}.{extension}",
    &["{category}", "Media", "Category", "{mcategory}"],
    "{id}",
);
static MEDIA_ITEM: Route = Route::new(
    "media-item",
    "{filename}.{extension}",
    &["{category}", "Media", "{author_slug|''}"],
    "{id}",
);
static PROFILE: Route = Route::new(
    "profile",
    FILENAME,
    &["{category}", "Profiles", "{profile[name]}"],
    "{id}",
);
const ROUTES: &[&Route] = &[
    &POST,
    &THREAD,
    &FORUM,
    &MEDIA_USER,
    &MEDIA_ALBUM,
    &MEDIA_CATEGORY,
    &MEDIA_ITEM,
    &PROFILE,
];

struct Instance {
    category: &'static str,
    root: &'static str,
    /// Host (and path prefix) pattern without the scheme.
    pattern: &'static str,
    cookies: &'static [&'static str],
}

macro_rules! instance {
    ($site:ident, $inst:ident, $category:literal, $name:literal, $root:literal, $pattern:literal, $cookies:expr, $domain:literal) => {
        static $inst: Instance = Instance {
            category: $category,
            root: $root,
            pattern: $pattern,
            cookies: $cookies,
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
    SIMPCITY,
    SIMPCITY_I,
    "simpcity",
    "SimpCity",
    "https://simpcity.cr",
    r"(?:www\.)?simpcity\.(?:cr|su)",
    &["ogaddgmetaprof_user"],
    "simpcity.cr"
);
instance!(
    NUDOSTARFORUM,
    NUDOSTARFORUM_I,
    "nudostarforum",
    "NudoStar Forum",
    "https://nudostar.com/forum",
    r"(?:www\.)?nudostar\.com/forum",
    &["xf_user"],
    "nudostar.com"
);
instance!(
    ATFFORUM,
    ATFFORUM_I,
    "atfforum",
    "AllTheFallen Forum",
    "https://allthefallen.moe/forum",
    r"(?:www\.)?allthefallen\.moe/forum",
    &["xf_user"],
    "allthefallen.moe"
);
instance!(
    CELEBFORUM,
    CELEBFORUM_I,
    "celebforum",
    "CelebForum",
    "https://celebforum.to",
    r"(?:www\.)?celebforum\.to",
    &["xf_user"],
    "celebforum.to"
);
instance!(
    TITSINTOPS,
    TITSINTOPS_I,
    "titsintops",
    "TitsInTops",
    "https://titsintops.com/phpBB2",
    r"(?:www\.)?titsintops\.com/phpBB2",
    &["xf_user"],
    "titsintops.com"
);
instance!(
    SOCIALMEDIAGIRLSFORUM,
    SOCIALMEDIAGIRLSFORUM_I,
    "socialmediagirlsforum",
    "SocialMediaGirls Forum",
    "https://forums.socialmediagirls.com",
    r"forums\.socialmediagirls\.com",
    &["xf_user"],
    "forums.socialmediagirls.com"
);
instance!(
    BLACKTOWHITE,
    BLACKTOWHITE_I,
    "blacktowhite",
    "BlackToWhite",
    "https://www.blacktowhite.net",
    r"(?:www\.)?blacktowhite\.net",
    &["xf_user"],
    "blacktowhite.net"
);
// `xenforo:https://host/...` for forums without a built-in instance.
instance!(
    XENFORO,
    XENFORO_I,
    "xenforo",
    "XenForo",
    "",
    "",
    &["xf_user"],
    ""
);

#[derive(Debug, PartialEq)]
enum Kind {
    Post {
        path: String,
        id: String,
    },
    Thread {
        path: String,
        page: Option<String>,
    },
    Forum {
        path: String,
        page: Option<String>,
    },
    MediaUser {
        prefix: String,
        user: String,
        page: Option<String>,
    },
    MediaAlbum {
        path: String,
        page: Option<String>,
    },
    MediaCategory {
        path: String,
        name: String,
        page: Option<String>,
    },
    MediaItem {
        path: String,
        file: String,
    },
    Profile {
        path: String,
        page: Option<String>,
    },
}

/// The instance root and the matched extractor.
fn target(inst: &Instance, url: &Url) -> Option<(String, Kind)> {
    let raw = url.as_str();
    let (root, rest) = if inst.category == "xenforo" {
        let c = re!(r"^xenforo:(https?://[^/?#]+)(.*)$").captures(raw)?;
        (c[1].to_owned(), c[2].to_owned())
    } else {
        if !crate::sites::plain(url) {
            return None;
        }
        let pattern = compiled(inst.pattern);
        let c = pattern.captures(raw)?;
        (inst.root.to_owned(), raw[c.get(0)?.end()..].to_owned())
    };
    Some((root, kind(&rest)?))
}

fn compiled(pattern: &'static str) -> regex::Regex {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<&'static str, regex::Regex>>> = OnceLock::new();
    let mut cache = CACHE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    cache
        .entry(pattern)
        .or_insert_with(|| {
            regex::Regex::new(&format!(r"^https?://{pattern}")).expect("static xenforo pattern")
        })
        .clone()
}

/// Upstream class patterns, in module order, below the instance root.
fn kind(rest: &str) -> Option<Kind> {
    let opt = |c: &regex::Captures<'_>, i: usize| c.get(i).map(|m| m.as_str().to_owned());
    if let Some(c) = re!(r"^(/(?:index\.php\?)?threads/[^/?#]+/(?:page-\d+)?#?post-|/posts/)(\d+)")
        .captures(rest)
    {
        return Some(Kind::Post {
            path: c[1].to_owned(),
            id: c[2].to_owned(),
        });
    }
    if let Some(c) =
        re!(r"^(/(?:index\.php\?)?threads/(?:[^/?#]+\.)?\d+)(?:/page-(\d+))?").captures(rest)
    {
        return Some(Kind::Thread {
            path: c[1].to_owned(),
            page: opt(&c, 2),
        });
    }
    if let Some(c) =
        re!(r"^(/(?:index\.php\?)?forums/(?:[^/?#]+\.)?[^/?#]+)(?:/page-(\d+))?").captures(rest)
    {
        return Some(Kind::Forum {
            path: c[1].to_owned(),
            page: opt(&c, 2),
        });
    }
    if let Some(c) = re!(
        r"^(/(?:index\.php\?)?)me(?:dia/users/([^/?#]+)(?:/page-(\d+))?|mbers/([^/?#]+)/#xfmgMedia)"
    )
    .captures(rest)
    {
        let (user, page) = match c.get(2) {
            Some(user) => (user.as_str().to_owned(), opt(&c, 3)),
            None => (c[4].to_owned(), None),
        };
        return Some(Kind::MediaUser {
            prefix: c[1].to_owned(),
            user,
            page,
        });
    }
    if let Some(c) =
        re!(r"^(/(?:index\.php\?)?media/albums/([^/?#]+))(?:/page-(\d+))?").captures(rest)
    {
        return Some(Kind::MediaAlbum {
            path: c[1].to_owned(),
            page: opt(&c, 3),
        });
    }
    if let Some(c) =
        re!(r"^(/(?:index\.php\?)?media/categories/([^/?#]+))(?:/page-(\d+))?").captures(rest)
    {
        return Some(Kind::MediaCategory {
            path: c[1].to_owned(),
            name: c[2].to_owned(),
            page: opt(&c, 3),
        });
    }
    if let Some(c) = re!(r"^(/(?:index\.php\?)?media/((?:[^/?#]+\.)\d+))").captures(rest) {
        return Some(Kind::MediaItem {
            path: c[1].to_owned(),
            file: c[2].to_owned(),
        });
    }
    let c = re!(r"^(/(?:index\.php\?)?members/[^/?#]+)(?:/page-(\d+))?").captures(rest)?;
    Some(Kind::Profile {
        path: c[1].to_owned(),
        page: opt(&c, 2),
    })
}

fn auth_required() -> crate::Error {
    error("this XenForo resource requires authentication (username & password or cookies)")
}

/// `request_page`: a page, failing on redirects to the registration form.
async fn request_page(http: &Http, url: &str) -> crate::Result<(String, Url)> {
    match http.text_final(&http.api(url)?, &[]).await {
        Ok((_, final_url)) if final_url.path().ends_with("/register") => Err(auth_required()),
        Ok(page) => Ok(page),
        Err(e) if crate::sites::http::refused_status(&e) == Some(403) => Err(auth_required()),
        Err(e) => Err(e),
    }
}

/// `XenforoExtractor.login`.
async fn login(http: &Http, inst: &Instance, root: &str) -> crate::Result<()> {
    let base = http.api(root)?;
    if inst
        .cookies
        .iter()
        .any(|name| http.cookie(&base, name).is_some())
    {
        return Ok(());
    }
    let Some((username, password)) = auth_info(http) else {
        return Ok(());
    };
    let url = http.api(&format!("{root}/login/login"))?;
    let page = http.text(&url).await?;
    let token = extr(&page, "name=\"_xfToken\" value=\"", "\"").to_owned();
    let form = [
        ("_xfToken", token.as_str()),
        ("login", username.as_str()),
        ("password", password.as_str()),
        ("remember", "1"),
        ("_xfRedirect", ""),
    ];
    match http.post_form_text(&url, &form, &[]).await {
        Err(e) if e.to_string().contains("redirected a POST") => Ok(()),
        Ok(page) => Err(error(match extract_error(&page) {
            Some(message) => format!("XenForo login failed: \"{message}\""),
            None => "XenForo login failed".into(),
        })),
        Err(e) => Err(e),
    }
}

fn extract_error(html: &str) -> Option<String> {
    let message = match extr(html, "blockMessage--error", "</") {
        "" => extr(html, "\"blockMessage\"", "</div>"),
        message => message,
    };
    (!message.is_empty())
        .then(|| unescape(message[message.find('>').map_or(0, |i| i + 1)..].trim()))
}

/// `_pagination`: pages from `pnum` (or from the start, following
/// `pageNav-jump--next`).
async fn pages_forward(
    http: &Http,
    base: &str,
    page: Option<&str>,
    params: &str,
) -> crate::Result<Vec<String>> {
    let (mut url, mut number) = match page {
        None => (format!("{base}/{params}"), Some(1i64)),
        Some(n) => (format!("{base}/page-{n}{params}"), None),
    };
    let mut pages = Vec::new();
    loop {
        let (page, _) = request_page(http, &url).await?;
        let more = page.contains("pageNav-jump--next");
        pages.push(page);
        match number {
            Some(n) if more => {
                number = Some(n + 1);
                url = format!("{base}/page-{}{params}", n + 1);
            }
            _ => return Ok(pages),
        }
    }
}

/// `_pagination_reverse`: from the last (or given) page back to the first.
async fn pages_reverse(http: &Http, base: &str, page: Option<&str>) -> crate::Result<Vec<String>> {
    let (first, final_url) =
        request_page(http, &format!("{base}/page-{}", page.unwrap_or("9999"))).await?;
    let final_url = final_url.as_str();
    let mut number = if final_url.ends_with('/') {
        1
    } else {
        crate::sites::art::parse_int(&final_url[final_url.rfind('-').map_or(0, |i| i + 1)..], 1)
    };
    let mut pages = vec![first];
    loop {
        number -= 1;
        let url = match number {
            n if n > 1 => format!("{base}/page-{n}"),
            1 => format!("{base}/"),
            _ => return Ok(pages),
        };
        pages.push(request_page(http, &url).await?.0);
    }
}

fn remove_quotes(content: &str) -> String {
    let mut content = content.to_owned();
    while let Some(begin) = content.find("<blockquote") {
        let Some(mut end) = content[begin..].find("</blockquote").map(|i| begin + i) else {
            break;
        };
        for _ in 0..content[begin + 11..end].matches("<blockquote").count() {
            match content[end + 13..].find("</blockquote") {
                Some(i) => end = end + 13 + i,
                None => break,
            }
        }
        content = format!(
            "{}{}",
            &content[..begin],
            content.get(end + 13..).unwrap_or_default()
        );
    }
    content
}

/// `slug.id` from the last path segment of a profile or thread URL.
fn slug_id(url: &str) -> (String, String) {
    let trimmed = &url[..url.len().saturating_sub(1)];
    let segment = &url[trimmed.rfind('/').map_or(0, |i| i + 1)..url.len().saturating_sub(1)];
    let (slug, _, id) = rpartition(segment, ".");
    (slug.to_owned(), id.to_owned())
}

fn parse_post(html: &str) -> Map<String, Value> {
    let mut scan = Scan::new(html);
    let author = scan.extr("data-author=\"", "\"").to_owned();
    let id = match scan.extr("data-content=\"post-", "\"") {
        "" => scan.extr("data-content=\"profile-post-", "\""),
        id => id,
    }
    .to_owned();
    let author_url = match scan.extr("itemprop=\"url\" content=\"", "\"") {
        "" => scan.extr("<a href=\"", "\""),
        url => url,
    }
    .to_owned();
    let posted = date(parse_iso(scan.extr("datetime=\"", "\"")));
    let content = match scan.extr("class=\"message-body", "<div class=\"js-selectToQuote") {
        "" => scan.extr("class=\"message-body", "</article>"),
        content => content,
    };
    let content = content
        .find("<div class=\"bbWrapper")
        .map_or(content, |i| &content[i..])
        .trim()
        .to_owned();
    let attachments = scan
        .extr("<section class=\"message-attachments\">", "</section>")
        .to_owned();
    let (slug, id_author) = slug_id(&author_url);
    let mut post = Map::new();
    post.insert("author".into(), author.into());
    post.insert("id".into(), id.into());
    post.insert("author_url".into(), author_url.into());
    post.insert("date".into(), posted);
    post.insert("content".into(), content.into());
    post.insert("attachments".into(), attachments.into());
    post.insert("author_slug".into(), slug.into());
    post.insert("author_id".into(), id_author.into());
    post
}

fn parse_author(author: &Value, data: &mut Map<String, Value>) {
    let name = str_of(author, "name").to_owned();
    data.insert("author".into(), name.as_str().into());
    match author
        .get("url")
        .and_then(Value::as_str)
        .filter(|u| !u.is_empty())
    {
        Some(url) => {
            let (slug, id) = slug_id(url);
            data.insert("author_url".into(), url.into());
            data.insert("author_slug".into(), slug.into());
            data.insert("author_id".into(), id.into());
        }
        None => {
            data.insert("author_url".into(), "".into());
            data.insert(
                "author_slug".into(),
                slugify(&name.chars().take(15).collect::<String>()).into(),
            );
            data.insert(
                "author_id".into(),
                name.chars().skip(15).collect::<String>().into(),
            );
        }
    }
}

fn parse_thread(page: &str) -> Map<String, Value> {
    let Ok(data) = jsonld(page) else {
        return Map::new();
    };
    let main = data.get("mainEntity").unwrap_or(&data);
    let url = main
        .get("url")
        .or(main.get("@id"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut thread = Map::new();
    let id = &url[url.rfind('.').map_or(0, |i| i + 1)
        ..url
            .len()
            .saturating_sub(1)
            .max(url.rfind('.').map_or(0, |i| i + 1))];
    thread.insert("id".into(), id.into());
    thread.insert("url".into(), url.into());
    thread.insert("title".into(), main["headline"].clone());
    thread.insert(
        "date".into(),
        date(parse_iso(str_of(main, "datePublished"))),
    );
    let tags: Vec<String> = main
        .get("keywords")
        .and_then(Value::as_str)
        .map(|k| k.split(", ").map(str::to_owned).collect())
        .unwrap_or_default();
    thread.insert("tags".into(), tags.into());
    thread.insert("section".into(), main["articleSection"].clone());
    parse_author(&main["author"], &mut thread);
    match &main["interactionStatistic"] {
        Value::Array(stats) => {
            thread.insert(
                "views".into(),
                stats
                    .first()
                    .map_or(Value::Null, |s| s["userInteractionCount"].clone()),
            );
            thread.insert(
                "posts".into(),
                stats
                    .get(1)
                    .map_or(Value::Null, |s| s["userInteractionCount"].clone()),
            );
        }
        stats => {
            thread.insert("views".into(), (-1).into());
            thread.insert("posts".into(), stats["userInteractionCount"].clone());
        }
    }
    thread
}

fn parse_profile(page: &str) -> Map<String, Value> {
    let Ok(user) = jsonld(page) else {
        return Map::new();
    };
    let main = &user["mainEntity"];
    let url = user
        .get("url")
        .or(main.get("@id"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let (slug, id) = slug_id(url);
    let mut profile = Map::new();
    profile.insert(
        "id".into(),
        main.get("identifier")
            .cloned()
            .filter(|v| !v.is_null())
            .unwrap_or_else(|| id.into()),
    );
    profile.insert("url".into(), url.into());
    profile.insert("slug".into(), unquote(&slug).into());
    for (key, from) in [
        ("name", "name"),
        ("avatar", "image"),
        ("description", "description"),
    ] {
        profile.insert(key.into(), main.get(from).cloned().unwrap_or(Value::Null));
    }
    profile.insert("date".into(), date(parse_iso(str_of(&user, "dateCreated"))));
    if let Value::Array(stats) = &main["interactionStatistic"] {
        profile.insert(
            "follows".into(),
            stats
                .first()
                .map_or(Value::Null, |s| s["userInteractionCount"].clone()),
        );
        profile.insert(
            "likes".into(),
            stats
                .get(1)
                .map_or(Value::Null, |s| s["userInteractionCount"].clone()),
        );
    }
    profile
}

fn a2b_base64(text: &str) -> Option<String> {
    let mut clean: String = text
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/'))
        .collect();
    while !clean.len().is_multiple_of(4) {
        clean.push('=');
    }
    base64::engine::general_purpose::STANDARD
        .decode(clean)
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
}

/// One `extract_urls` match: video, attachment, bb-code image, external link.
type Link = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// `XenforoExtractor.items` for one post: files and queued links.
fn post_events<'a>(
    http: &'a Http,
    site: &'static Site,
    route: &'static Route,
    root: &str,
    extra: &Map<String, Value>,
    mut post: Map<String, Value>,
) -> Events<'a> {
    let base = match root[8.min(root.len())..].find('/') {
        Some(pos) => root[..pos + 8].to_owned(),
        None => root.to_owned(),
    };
    let raw_content = str_of(&Value::Object(post.clone()), "content").to_owned();
    let content = if http.option_bool("quoted", false) {
        raw_content
    } else {
        remove_quotes(&raw_content)
    };
    let pattern = re!(concat!(
        r#"(?s)(?:<video (.*?\ssrc="[^"]+".*?)</video>"#,
        r#"|<a [^>]*?href="([^"]*?/(?:index\.php\?)?attachments/[^"]+".*?)</a>"#,
        r#"|<div class="bb(?:Image|Media)Wrapper[^>]*?data-src="([^"]+".*?) />"#,
        r#"|(?:<a [^>]*?href="|<iframe [^>]*?src="|onclick="loadMedia\(this, ')([^"']+[^<]*?<))"#
    ));
    let group = |c: &regex::Captures<'_>, i| c.get(i).map(|m| m.as_str().to_owned());
    let mut urls: Vec<Link> = pattern
        .captures_iter(&content)
        .map(|c| (group(&c, 1), group(&c, 2), group(&c, 3), group(&c, 4)))
        .collect();
    if http.option_bool("embeds", true) && content.contains("data-s9e-mediaembed-iframe=") {
        for embed in extract_iter(&content, "data-s9e-mediaembed-iframe='", "'") {
            let Ok(Value::Array(values)) = serde_json::from_str::<Value>(embed) else {
                continue;
            };
            let data: Map<String, Value> = values
                .chunks(2)
                .filter_map(|p| Some((p.first()?.as_str()?.to_owned(), p.get(1)?.clone())))
                .collect();
            let src = data.get("src").and_then(Value::as_str).unwrap_or_default();
            if src.is_empty() {
                continue;
            }
            let frag = &src[src.find('#').map_or(0, |i| i + 1)..];
            let url = match data.get("data-s9e-mediaembed").and_then(Value::as_str) {
                Some("tiktok") => format!("https://www.tiktok.com/@/video/{frag}"),
                Some("reddit") => format!("https://embed.reddit.com/r/{frag}"),
                Some("imgur") => format!("https://imgur.com/{frag}"),
                _ => continue,
            };
            urls.push((None, None, None, Some(url)));
        }
    }
    let attachments = str_of(&Value::Object(post.clone()), "attachments").to_owned();
    if http.option_bool("attachments", true) && !attachments.is_empty() {
        let href = re!(r#"(?s)\shref=["'](.+)"#);
        for attachment in extract_iter(&attachments, "<li", "</li>") {
            if let Some(c) = href.captures(attachment) {
                urls.push((None, Some(c[1].to_owned()), None, None));
            }
        }
    }
    post.insert("count".into(), urls.len().into());
    let mut data = extra.clone();
    data.insert("post".into(), Value::Object(post));
    data.insert("count".into(), urls.len().into());
    let (mut num, mut internal, mut external) = (0, 0, 0);
    let mut id_last: Option<i64> = None;
    let mut items = Vec::new();
    let mut queued = Vec::new();
    let counters = |data: &mut Map<String, Value>, num: i64, internal: i64, external: i64| {
        data.insert("num".into(), num.into());
        data.insert("num_internal".into(), internal.into());
        data.insert("num_external".into(), external.into());
    };
    for (video, inl, bb, ext) in urls {
        if let Some(mut ext) = ext {
            if ext.starts_with('#') {
                continue;
            }
            if ext.starts_with('/') {
                if ext.as_bytes().get(1) == Some(&b'/') {
                    if let Some(q) = ext.find('\'') {
                        ext.truncate(q);
                    }
                    ext = format!("https:{ext}");
                } else if let Some(query) = ext.strip_prefix("/goto/link-confirmation?") {
                    let params = parse_query(&unescape(query));
                    match params
                        .get("url")
                        .and_then(Value::as_str)
                        .and_then(a2b_base64)
                    {
                        Some(decoded) => ext = decoded,
                        None => continue,
                    }
                } else if ext.starts_with("/redirect/") {
                    ext = match extr(&ext, "?to=", "&") {
                        "" => unescape(extr(&ext, ">", "<").trim()),
                        to => match a2b_base64(to) {
                            Some(decoded) => decoded,
                            None => continue,
                        },
                    };
                } else {
                    continue;
                }
            } else if let Some(q) = ext.find('"') {
                ext.truncate(q);
            }
            num += 1;
            external += 1;
            let mut dc = data.clone();
            counters(&mut dc, num, internal, external);
            dc.insert("type".into(), "external".into());
            if let Ok(url) = Url::parse(&ext) {
                queued.push((url, dc));
            }
        } else if let Some(video) = video {
            num += 1;
            internal += 1;
            let mut dc = data.clone();
            counters(&mut dc, num, internal, external);
            dc.insert("type".into(), "video".into());
            let url = extr(&video, "src=\"", "\"");
            nameext_into(url, &mut dc);
            let filename = dc
                .get("filename")
                .map(crate::sites::scalar)
                .unwrap_or_default();
            dc.insert("id".into(), parse_int(partition(&filename, "-").0).into());
            let url = if url.starts_with('/') {
                format!("{base}{url}")
            } else {
                url.to_owned()
            };
            if let Ok(file) = Url::parse(&url) {
                items.push(Item::new(file, dc));
            }
        } else if let Some(inline) = bb.or(inl) {
            let url = &inline[..inline.find('"').unwrap_or(inline.len())];
            let tail_start = url[..url.len().saturating_sub(1)].rfind('/').unwrap_or(0);
            let (name, _, id) = rpartition(url[tail_start..].trim_matches('/'), ".");
            let id = parse_int(id);
            if id != 0 {
                if Some(id) == id_last {
                    id_last = None;
                    continue;
                }
                id_last = Some(id);
            }
            let mut dc = data.clone();
            dc.insert("id".into(), id.into());
            let alt = match extr(&inline, "alt=\"", "\"") {
                "" => extr(&inline, "title=\"", "\""),
                alt => alt,
            };
            if alt.is_empty() {
                let (filename, _, extension) = rpartition(name, "-");
                dc.insert("filename".into(), filename.into());
                dc.insert("extension".into(), extension.into());
            } else {
                let (stem, mut extension) = nameext_from_name(alt);
                if extension.is_empty() {
                    extension = rpartition(name, "-").2.to_owned();
                }
                dc.insert("filename".into(), stem.into());
                dc.insert("extension".into(), extension.into());
            }
            num += 1;
            internal += 1;
            counters(&mut dc, num, internal, external);
            dc.insert("type".into(), "inline".into());
            let url = if url.starts_with('/') {
                format!("{base}{url}")
            } else {
                url.to_owned()
            };
            if let Ok(file) = Url::parse(&unescape(&url)) {
                items.push(Item::new(file, dc));
            }
        }
    }
    let files = crate::sites::events(crate::sites::post(site, http, route, items));
    let children = futures_util::stream::iter(queued)
        .map(move |(url, data)| {
            crate::sites::queue(http, site.category, route.subcategory, url, &data)
        })
        .flatten();
    files.chain(children).boxed()
}

/// `_extract_media`/`_extract_media_ex`: a media item's file URL and keywords.
async fn media_item(
    http: &Http,
    url: &str,
    file: &str,
) -> crate::Result<(String, Map<String, Value>)> {
    let mut media = Map::new();
    if !http.option_bool("metadata", false) {
        let (name, _, id) = rpartition(file, ".");
        let (filename, _, extension) = rpartition(name, "-");
        media.insert("id".into(), id.into());
        media.insert("filename".into(), filename.into());
        media.insert("extension".into(), extension.into());
        return Ok((format!("{url}full"), media));
    }
    let (page, _) = request_page(http, url).await?;
    let schema = jsonld(&page)?;
    let main = &schema["mainEntity"];
    let (stem, ext) = nameext_from_name(str_of(main, "name"));
    media.insert("filename".into(), stem.into());
    media.insert("extension".into(), ext.into());
    media.insert("schema".into(), schema.clone());
    media.insert("id".into(), rpartition(file, ".").2.into());
    media.insert(
        "size".into(),
        main.get("contentSize").cloned().unwrap_or(Value::Null),
    );
    media.insert(
        "description".into(),
        main.get("description").cloned().unwrap_or(Value::Null),
    );
    media.insert("date".into(), date(parse_iso(str_of(main, "dateCreated"))));
    let dimension = |key: &str| parse_int(partition(str_of(&main[key], "name"), " ").0);
    media.insert("width".into(), dimension("width").into());
    media.insert("height".into(), dimension("height").into());
    parse_author(&main["author"], &mut media);
    if let Some(ext) = main
        .get("encodingFormat")
        .and_then(Value::as_str)
        .filter(|e| !e.is_empty())
    {
        media.insert("extension".into(), ext.into());
    }
    if let Value::Array(stats) = &main["interactionStatistic"] {
        for (index, key) in ["views", "likes", "comments"].into_iter().enumerate() {
            media.insert(
                key.into(),
                stats
                    .get(index)
                    .map_or(Value::Null, |s| s["userInteractionCount"].clone()),
            );
        }
    }
    Ok((str_of(main, "contentUrl").to_owned(), media))
}

/// `items_media`: media list pages (`itemList-item` entries).
#[allow(clippy::too_many_arguments)]
async fn media_list(
    http: &Http,
    site: &'static Site,
    route: &'static Route,
    root: &str,
    base: &str,
    page: Option<&str>,
    mut extra: Map<String, Value>,
    album: bool,
) -> crate::Result<Vec<crate::event::ExtractionEvent>> {
    let reverse = http
        .option_str("order-posts")
        .and_then(|o| o.chars().next())
        .is_some_and(|c| matches!(c, 'd' | 'r'));
    let pages = if reverse {
        pages_reverse(http, base, page).await?
    } else {
        pages_forward(http, base, page, "").await?
    };
    if album && let Some(first) = pages.first() {
        extra.insert("album".into(), parse_album(first)?.into());
    }
    let origin = match root[8.min(root.len())..].find('/') {
        Some(pos) => root[..pos + 8].to_owned(),
        None => root.to_owned(),
    };
    let metadata = http.option_bool("metadata", false);
    let mut events = Vec::new();
    for page in pages {
        let mut posts: Vec<&str> = page
            .split("<div class=\"itemList-item js-inlineModContainer")
            .skip(1)
            .collect();
        if reverse {
            posts.reverse();
        }
        for html in posts {
            let mut scan = Scan::new(html);
            let href = scan.extr("href=\"", "\"").to_owned();
            let name = scan.find("alt='", "'").map(str::to_owned);
            let file = &href[href[..href.len().saturating_sub(1)]
                .rfind('/')
                .map_or(0, |i| i + 1)..href.len().saturating_sub(1)];
            let (url, mut media) = media_item(http, &format!("{origin}{href}"), file).await?;
            if url.ends_with("/register/full") {
                continue;
            }
            if !metadata && let Some(name) = name.filter(|n| !n.is_empty()) {
                let (stem, ext) = nameext_from_name(&unescape(&name));
                media.insert("filename".into(), stem.into());
                media.insert("extension".into(), ext.into());
            }
            media.extend(extra.clone());
            let id = media
                .get("id")
                .map(crate::sites::scalar)
                .unwrap_or_default();
            if let Ok(file) = Url::parse(&url) {
                events.extend(crate::sites::post(
                    site,
                    http,
                    route,
                    vec![Item::new(file, media).ids(id.clone(), id)],
                ));
            }
        }
    }
    Ok(events)
}

fn parse_album(page: &str) -> crate::Result<Map<String, Value>> {
    let main = jsonld(page)?["mainEntity"].clone();
    let url = main
        .get("url")
        .or(main.get("@id"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let (slug, id) = slug_id(&url);
    let mut album = Map::new();
    album.insert("id".into(), id.into());
    album.insert("url".into(), url.as_str().into());
    album.insert("slug".into(), unquote(&slug).into());
    album.insert("title".into(), main["headline"].clone());
    album.insert(
        "description".into(),
        main.get("description").cloned().unwrap_or(Value::Null),
    );
    album.insert("date".into(), date(parse_iso(str_of(&main, "dateCreated"))));
    parse_author(&main["author"], &mut album);
    if let Value::Array(stats) = &main["interactionStatistic"] {
        for (index, key) in ["count", "likes", "views", "comments"]
            .into_iter()
            .enumerate()
        {
            album.insert(
                key.into(),
                stats
                    .get(index)
                    .map_or(Value::Null, |s| s["userInteractionCount"].clone()),
            );
        }
    }
    Ok(album)
}

fn extract<'a>(
    inst: &'static Instance,
    site: &'static Site,
    http: &'a Http,
    url: Url,
) -> Events<'a> {
    let Some((root, kind)) = target(inst, &url) else {
        return crate::sites::fail(error("unsupported XenForo URL"));
    };
    crate::sites::once(async move {
        login(http, inst, &root).await?;
        let reverse_default = |http: &Http| {
            !http
                .option_str("order-posts")
                .and_then(|o| o.chars().next())
                .is_some_and(|c| !matches!(c, 'd' | 'r'))
        };
        Ok(match kind {
            Kind::Post { path, id } => {
                let (page, _) = request_page(http, &format!("{root}{path}{id}/")).await?;
                let pos = page
                    .find(&format!("data-content=\"post-{id}\""))
                    .ok_or_else(|| error("XenForo post not found"))?;
                let html = Scan::at(&page, pos.saturating_sub(200))
                    .extr("<article ", "<footer")
                    .to_owned();
                let mut extra = Map::new();
                extra.insert("thread".into(), parse_thread(&page).into());
                post_events(http, site, &POST, &root, &extra, parse_post(&html))
            }
            Kind::Thread { path, page } => {
                let base = format!("{root}{path}");
                let order = http
                    .option_str("order-posts")
                    .unwrap_or_default()
                    .to_owned();
                let (pages, reverse) = if !reverse_default(http) {
                    let params = if order.starts_with('s') {
                        "?order=reaction_score"
                    } else {
                        ""
                    };
                    (
                        pages_forward(http, &base, page.as_deref(), params).await?,
                        false,
                    )
                } else if order == "reaction" {
                    (
                        pages_forward(http, &base, page.as_deref(), "?order=reaction_score")
                            .await?,
                        false,
                    )
                } else {
                    (pages_reverse(http, &base, page.as_deref()).await?, true)
                };
                let mut extra = Map::new();
                extra.insert(
                    "thread".into(),
                    pages
                        .first()
                        .map(|p| parse_thread(p))
                        .unwrap_or_default()
                        .into(),
                );
                let mut posts = Vec::new();
                for page in &pages {
                    let mut list: Vec<Map<String, Value>> =
                        extract_iter(page, "<article ", "<footer")
                            .into_iter()
                            .map(parse_post)
                            .collect();
                    if reverse {
                        list.reverse();
                    }
                    posts.extend(list);
                }
                let root = root.clone();
                futures_util::stream::iter(posts)
                    .map(move |post| post_events(http, site, &THREAD, &root, &extra, post))
                    .flatten()
                    .boxed()
            }
            Kind::Forum { path, page } => {
                let pages =
                    pages_forward(http, &format!("{root}{path}"), page.as_deref(), "").await?;
                let threads: Vec<Url> = pages
                    .iter()
                    .flat_map(|page| {
                        re!(r#"(/(?:index\.php\?)?threads/[^"]+)"[^>]+data-xf-init="#)
                            .captures_iter(page)
                            .map(|c| c[1].to_owned())
                            .collect::<Vec<_>>()
                    })
                    .filter_map(|path| Url::parse(&format!("{root}{}", unquote(&path))).ok())
                    .map(|url| {
                        if inst.category == "xenforo" {
                            Url::parse(&format!("xenforo:{url}")).unwrap_or(url)
                        } else {
                            url
                        }
                    })
                    .collect();
                futures_util::stream::iter(threads)
                    .map(move |url| {
                        crate::sites::queue(
                            http,
                            site.category,
                            FORUM.subcategory,
                            url,
                            &Map::new(),
                        )
                    })
                    .flatten()
                    .boxed()
            }
            Kind::MediaUser { prefix, user, page } => {
                let mut extra = Map::new();
                if !http.option_bool("metadata", false) {
                    let (slug, _, id) = rpartition(&user, ".");
                    extra.insert("author_slug".into(), slug.into());
                    extra.insert("author_id".into(), id.into());
                }
                let base = format!("{root}{prefix}media/users/{user}");
                crate::sites::events(
                    media_list(
                        http,
                        site,
                        &MEDIA_USER,
                        &root,
                        &base,
                        page.as_deref(),
                        extra,
                        false,
                    )
                    .await?,
                )
            }
            Kind::MediaAlbum { path, page } => {
                let base = format!("{root}{path}");
                crate::sites::events(
                    media_list(
                        http,
                        site,
                        &MEDIA_ALBUM,
                        &root,
                        &base,
                        page.as_deref(),
                        Map::new(),
                        true,
                    )
                    .await?,
                )
            }
            Kind::MediaCategory { path, name, page } => {
                let (category, _, id) = rpartition(&name, ".");
                let mut extra = Map::new();
                extra.insert("mcategory".into(), category.into());
                extra.insert("mcategory_id".into(), id.into());
                let base = format!("{root}{path}");
                crate::sites::events(
                    media_list(
                        http,
                        site,
                        &MEDIA_CATEGORY,
                        &root,
                        &base,
                        page.as_deref(),
                        extra,
                        false,
                    )
                    .await?,
                )
            }
            Kind::MediaItem { path, file } => {
                let (url, media) = media_item(http, &format!("{root}{path}/"), &file).await?;
                let id = media
                    .get("id")
                    .map(crate::sites::scalar)
                    .unwrap_or_default();
                let file = Url::parse(&url).map_err(|_| error("XenForo media has no file URL"))?;
                crate::sites::events(crate::sites::post(
                    site,
                    http,
                    &MEDIA_ITEM,
                    vec![Item::new(file, media).ids(id.clone(), id)],
                ))
            }
            Kind::Profile { path, page } => {
                let base = format!("{root}{path}");
                let reverse = http
                    .option_str("order-posts")
                    .and_then(|o| o.chars().next())
                    .is_some_and(|c| matches!(c, 'd' | 'r'));
                let pages = if reverse {
                    pages_reverse(http, &base, page.as_deref()).await?
                } else {
                    pages_forward(http, &base, page.as_deref(), "").await?
                };
                let mut extra = Map::new();
                extra.insert(
                    "profile".into(),
                    pages
                        .first()
                        .map(|p| parse_profile(p))
                        .unwrap_or_default()
                        .into(),
                );
                let mut posts = Vec::new();
                for page in &pages {
                    let mut list: Vec<Map<String, Value>> =
                        extract_iter(page, "<article ", "<footer")
                            .into_iter()
                            .map(parse_post)
                            .collect();
                    if reverse {
                        list.reverse();
                    }
                    posts.extend(list);
                }
                let root = root.clone();
                futures_util::stream::iter(posts)
                    .map(move |post| post_events(http, site, &PROFILE, &root, &extra, post))
                    .flatten()
                    .boxed()
            }
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http as client, keywords, serve};

    fn t(site: &Site, raw: &str) -> bool {
        (site.matches)(&Url::parse(raw).unwrap())
    }

    #[test]
    fn classifies_urls() {
        let k =
            |inst: &Instance, raw: &str| target(inst, &Url::parse(raw).unwrap()).map(|(_, k)| k);
        assert_eq!(
            k(
                &SIMPCITY_I,
                "https://simpcity.cr/threads/meme-thread.96106/page-7#post-1034578"
            ),
            Some(Kind::Post {
                path: "/threads/meme-thread.96106/page-7#post-".into(),
                id: "1034578".into()
            })
        );
        assert_eq!(
            k(&SIMPCITY_I, "https://simpcity.su/threads/angel.12948/"),
            Some(Kind::Thread {
                path: "/threads/angel.12948".into(),
                page: None
            })
        );
        assert_eq!(
            k(
                &NUDOSTARFORUM_I,
                "https://nudostar.com/forum/threads/aspen-rae.106714/page-2"
            ),
            Some(Kind::Thread {
                path: "/threads/aspen-rae.106714".into(),
                page: Some("2".into())
            })
        );
        assert_eq!(
            k(
                &ATFFORUM_I,
                "https://www.allthefallen.moe/forum/index.php?threads/final-fantasy-xiv.57090/#post-21765744"
            ),
            Some(Kind::Post {
                path: "/index.php?threads/final-fantasy-xiv.57090/#post-".into(),
                id: "21765744".into()
            })
        );
        assert!(
            matches!(k(&ATFFORUM_I, "https://allthefallen.moe/forum/index.php?members/peters.150992/#xfmgMedia"), Some(Kind::MediaUser { user, .. }) if user == "peters.150992")
        );
        assert!(matches!(
            k(
                &ATFFORUM_I,
                "https://allthefallen.moe/forum/index.php?media/1737485564664-png.224260/"
            ),
            Some(Kind::MediaItem { .. })
        ));
        assert!(matches!(
            k(&CELEBFORUM_I, "https://celebforum.to/media/albums/5404/"),
            Some(Kind::MediaAlbum { .. })
        ));
        assert!(
            matches!(k(&ATFFORUM_I, "https://allthefallen.moe/forum/index.php?members/spammy.195035/page-2"), Some(Kind::Profile { page: Some(p), .. }) if p == "2")
        );
        assert!(matches!(
            k(&CELEBFORUM_I, "https://celebforum.to/forums/pornostars.13/"),
            Some(Kind::Forum { .. })
        ));
        assert!(t(&XENFORO, "xenforo:https://forum.example/threads/x.1/"));
        assert!(!t(&SIMPCITY, "https://example.com/threads/x.1/"));
    }

    #[tokio::test]
    async fn extracts_post_files_and_links() {
        let schema = serde_json::json!({"mainEntity": {"url": "https://simpcity.cr/threads/sophia.10049/", "headline": "Sophia",
            "datePublished": "2021-01-01T00:00:00Z", "articleSection": "Asians", "author": {"name": "Ann", "url": "https://simpcity.cr/members/ann.5/"},
            "interactionStatistic": [{"userInteractionCount": 9}, {"userInteractionCount": 2}]}});
        let page = format!(
            r#"<script type="application/ld+json">{schema}</script>
            <article data-author="Ann" data-content="post-10891"><a itemprop="url" content="https://simpcity.cr/members/ann.5/"></a><time datetime="2021-01-02T00:00:00Z">
            <div class="message-body"><div class="bbWrapper">
            <div class="bbImageWrapper " data-src="https://simpcity.cr/attachments/photo-jpg.123/" title="Photo.jpg"><img src="x" /></div>
            <a href="https://simpcity.cr/attachments/photo-jpg.123/" target="_blank">dup</a>
            <a href="https://imx.to/i/abc" class="link">ext</a>
            <video controls src="/data/video/1/1234-clip.mp4"></video>
            </div><div class="js-selectToQuote"></div><footer>"#
        );
        let (root, server) = serve(vec![html(&page)]);
        let http = client("simpcity", root);
        let events: Vec<_> = (SIMPCITY.extract)(
            &http,
            Url::parse("https://simpcity.cr/threads/sophia.10049/post-10891").unwrap(),
        )
        .collect()
        .await;
        let media: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Ok(crate::event::ExtractionEvent::Media(m)) => Some(m),
                _ => None,
            })
            .collect();
        assert_eq!(media.len(), 2, "{events:?}");
        assert_eq!(
            media[0].url.as_str(),
            "https://simpcity.cr/attachments/photo-jpg.123/"
        );
        let k0 = keywords(media[0]);
        assert_eq!(k0["filename"], "Photo");
        assert_eq!(k0["id"], 123);
        assert_eq!(k0["type"], "inline");
        assert_eq!(k0["thread"]["section"], "Asians");
        assert_eq!(
            media[1].url.as_str(),
            "https://simpcity.cr/data/video/1/1234-clip.mp4"
        );
        assert_eq!(keywords(media[1])["id"], 1234);
        let (file, dir) = crate::sites::default_templates(media[0]).unwrap();
        assert_eq!(file.render(media[0]).unwrap(), "10891_01_123_Photo.jpg");
        assert_eq!(
            dir.render(media[0]).unwrap().unwrap(),
            std::path::PathBuf::from("simpcity/Asians/Sophia (10049)")
        );
        let (_, identity) = crate::sites::archive_key(media[0]).unwrap();
        assert_eq!(
            identity,
            r#"["{post[id]}/{type[0]}{id}_{filename}",["10891","i","123","Photo"]]"#
        );
        let children = events
            .iter()
            .filter(|e| matches!(e, Ok(crate::event::ExtractionEvent::BeginChild(_))))
            .count();
        assert_eq!(children, 1);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /threads/sophia.10049/post-10891/ "));
    }

    #[test]
    fn slugifies_authors() {
        assert_eq!(slugify("Hello, World -- x"), "hello-world-x");
    }
}
