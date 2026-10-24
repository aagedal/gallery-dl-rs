//! XenForo forums, mirroring gallery_dl/extractor/xenforo.py for its
//! built-in instances: posts, threads (newest page first unless
//! `order-posts` says otherwise), forums, profiles and the media gallery.
//!
//! Each post's videos, inline images and attachments are files; external
//! links and media embeds are queued when a native extractor supports them
//! and skipped otherwise. Options: `attachments`, `embeds`, `quoted`,
//! `order-posts` and `metadata`. Upstream's `username`/`password` login is
//! not supported; pass the forum's session cookies with `--cookies`.
use crate::sites::art::{Scan, extr, extract_iter, json_loads, parse_int, str_of};
use crate::sites::media::iso_ts;
use crate::sites::{Events, Http, Item, Route, Site, unescape};
use crate::{Result, error};
use base64::Engine;
use futures_util::{StreamExt, stream};
use serde_json::{Map, Value};
use std::sync::Arc;
use url::Url;

const FILENAME: &str = "{post[id]}_{num:>02}{id:?_//}_{filename}.{extension}";
const DIRECTORY: &[&str] = &[
    "{category}",
    "{thread[section]}",
    "{thread[title]} ({thread[id]})",
];
const ARCHIVE: &str = "{post[id]}/{type[0]}{id}_{filename}";
const MEDIA_FILENAME: &str = "{filename}.{extension}";
static POST: Route = Route::new("post", FILENAME, DIRECTORY, ARCHIVE);
static THREAD: Route = Route::new("thread", FILENAME, DIRECTORY, ARCHIVE);
static FORUM: Route = Route::new("forum", FILENAME, DIRECTORY, ARCHIVE);
static MEDIA_USER: Route = Route::new(
    "media-user",
    MEDIA_FILENAME,
    &["{category}", "Media", "{author_slug}"],
    "{id}",
);
static MEDIA_ALBUM: Route = Route::new(
    "media-album",
    MEDIA_FILENAME,
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
    MEDIA_FILENAME,
    &["{category}", "Media", "Category", "{mcategory}"],
    "{id}",
);
static MEDIA_ITEM: Route = Route::new(
    "media-item",
    MEDIA_FILENAME,
    &["{category}", "Media", "{author_slug|''}"],
    "{id}",
);
static PROFILE: Route = Route::new(
    "profile",
    FILENAME,
    &["{category}", "Profiles", "{profile[name]}"],
    "{id}",
);

/// One built-in instance: its root, accepted hosts and path prefix.
pub struct Instance {
    root: &'static str,
    hosts: &'static [&'static str],
    prefix: &'static str,
}

macro_rules! instance {
    ($ident:ident, $category:literal, $name:literal, $domain:literal, $instance:expr) => {
        pub static $ident: Site = Site {
            category: $category,
            name: $name,
            matches: |url| target(&$instance, url).is_some(),
            extract: |http, url| extract(&$ident, &$instance, http, url),
            routes: &[
                &THREAD,
                &POST,
                &FORUM,
                &MEDIA_USER,
                &MEDIA_ALBUM,
                &MEDIA_CATEGORY,
                &MEDIA_ITEM,
                &PROFILE,
            ],
            api_key: false,
            cookies: Some($domain),
        };
    };
}
instance!(
    SIMPCITY,
    "simpcity",
    "SimpCity",
    "simpcity.cr",
    Instance {
        root: "https://simpcity.cr",
        hosts: &[
            "simpcity.cr",
            "www.simpcity.cr",
            "simpcity.su",
            "www.simpcity.su"
        ],
        prefix: "",
    }
);
instance!(
    NUDOSTARFORUM,
    "nudostarforum",
    "NudoStar Forum",
    "nudostar.com",
    Instance {
        root: "https://nudostar.com/forum",
        hosts: &["nudostar.com", "www.nudostar.com"],
        prefix: "/forum",
    }
);
instance!(
    ATFFORUM,
    "atfforum",
    "AllTheFallen Forum",
    "allthefallen.moe",
    Instance {
        root: "https://allthefallen.moe/forum",
        hosts: &["allthefallen.moe", "www.allthefallen.moe"],
        prefix: "/forum",
    }
);
instance!(
    CELEBFORUM,
    "celebforum",
    "CelebForum",
    "celebforum.to",
    Instance {
        root: "https://celebforum.to",
        hosts: &["celebforum.to", "www.celebforum.to"],
        prefix: "",
    }
);
instance!(
    TITSINTOPS,
    "titsintops",
    "TitsInTops",
    "titsintops.com",
    Instance {
        root: "https://titsintops.com/phpBB2",
        hosts: &["titsintops.com", "www.titsintops.com"],
        prefix: "/phpBB2",
    }
);
instance!(
    SOCIALMEDIAGIRLSFORUM,
    "socialmediagirlsforum",
    "Social Media Girls Forum",
    "forums.socialmediagirls.com",
    Instance {
        root: "https://forums.socialmediagirls.com",
        hosts: &["forums.socialmediagirls.com"],
        prefix: "",
    }
);
instance!(
    BLACKTOWHITE,
    "blacktowhite",
    "BlackToWhite",
    "www.blacktowhite.net",
    Instance {
        root: "https://www.blacktowhite.net",
        hosts: &["blacktowhite.net", "www.blacktowhite.net"],
        prefix: "",
    }
);

#[derive(Debug, PartialEq)]
enum Target {
    /// Path up to the post ID, and the ID.
    Post(String, String),
    /// Thread path and page.
    Thread(String, Option<String>),
    Forum(String, Option<String>),
    /// `/` or `/index.php?`, the user and page.
    MediaUser(String, String, Option<String>),
    /// Album path, album and page.
    MediaAlbum(String, String, Option<String>),
    MediaCategory(String, String, Option<String>),
    /// Media path and `NAME.ID`.
    MediaItem(String, String),
    Profile(String, Option<String>),
}

/// The text up to `/`, `?` or `#`.
fn segment(text: &str) -> &str {
    &text[..text.find(['/', '?', '#']).unwrap_or(text.len())]
}

/// `(?:/page-(\d+))?`.
fn page(text: &str) -> Option<String> {
    let digits = crate::sites::art::leading_digits(text.strip_prefix("/page-")?)?;
    Some(digits.into())
}

/// `(?:[^/?#]+\.)?\d+` at the start of a segment: the greedy prefix ends at
/// the last `.` that is followed by a digit.
fn slug_id(seg: &str) -> Option<&str> {
    let dotted = seg.char_indices().rev().find_map(|(i, c)| {
        (c == '.' && i > 0)
            .then(|| crate::sites::art::leading_digits(&seg[i + 1..]).map(|d| i + 1 + d.len()))
            .flatten()
    });
    match dotted {
        Some(end) => Some(&seg[..end]),
        None => crate::sites::art::leading_digits(seg),
    }
}

/// The instance URL's remainder after its prefix, with query and fragment,
/// matched in upstream's class order.
fn target(instance: &Instance, url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) || !instance.hosts.contains(&url.host_str()?) {
        return None;
    }
    let path = url.path().strip_prefix(instance.prefix)?;
    if !path.starts_with('/') {
        return None;
    }
    let mut rest = path.to_owned();
    if let Some(query) = url.query() {
        rest = format!("{rest}?{query}");
    }
    if let Some(fragment) = url.fragment() {
        rest = format!("{rest}#{fragment}");
    }
    let (ip, after) = match rest.strip_prefix("/index.php?") {
        Some(after) => ("/index.php?", after),
        None => ("/", &rest[1..]),
    };
    // XenforoPostExtractor
    if let Some(tail) = after.strip_prefix("threads/") {
        let seg = segment(tail);
        if let Some(post) = tail[seg.len()..].strip_prefix('/') {
            let mut post_rest = post;
            if let Some(p) = post_rest.strip_prefix("page-")
                && let Some(d) = crate::sites::art::leading_digits(p)
            {
                post_rest = &p[d.len()..];
            }
            let post_rest = post_rest.strip_prefix('#').unwrap_or(post_rest);
            if !seg.is_empty()
                && let Some(id) = post_rest
                    .strip_prefix("post-")
                    .and_then(crate::sites::art::leading_digits)
            {
                let consumed = rest.len() - post_rest.len() + "post-".len();
                return Some(Target::Post(rest[..consumed].into(), id.into()));
            }
        }
    }
    if let Some(id) = rest
        .strip_prefix("/posts/")
        .and_then(crate::sites::art::leading_digits)
    {
        return Some(Target::Post("/posts/".into(), id.into()));
    }
    // XenforoThreadExtractor
    if let Some(tail) = after.strip_prefix("threads/")
        && let Some(id) = slug_id(segment(tail))
    {
        return Some(Target::Thread(
            format!("{ip}threads/{id}"),
            page(&tail[id.len()..]),
        ));
    }
    // XenforoForumExtractor
    if let Some(tail) = after.strip_prefix("forums/") {
        let seg = segment(tail);
        if !seg.is_empty() {
            return Some(Target::Forum(
                format!("{ip}forums/{seg}"),
                page(&tail[seg.len()..]),
            ));
        }
    }
    // XenforoMediaUserExtractor
    if let Some(tail) = after.strip_prefix("media/users/") {
        let seg = segment(tail);
        if !seg.is_empty() {
            return Some(Target::MediaUser(
                ip.into(),
                seg.into(),
                page(&tail[seg.len()..]),
            ));
        }
    }
    if let Some(tail) = after.strip_prefix("members/") {
        let seg = segment(tail);
        if !seg.is_empty() && tail[seg.len()..].starts_with("/#xfmgMedia") {
            return Some(Target::MediaUser(ip.into(), seg.into(), None));
        }
    }
    // XenforoMediaAlbumExtractor and XenforoMediaCategoryExtractor
    for (kind, album) in [("albums", true), ("categories", false)] {
        if let Some(tail) = after.strip_prefix(&format!("media/{kind}/")) {
            let seg = segment(tail);
            if !seg.is_empty() {
                let path = format!("{ip}media/{kind}/{seg}");
                let page = page(&tail[seg.len()..]);
                return Some(if album {
                    Target::MediaAlbum(path, seg.into(), page)
                } else {
                    Target::MediaCategory(path, seg.into(), page)
                });
            }
        }
    }
    // XenforoMediaItemExtractor: `media/((?:[^/?#]+\.)\d+)`
    if let Some(tail) = after.strip_prefix("media/")
        && let Some(name) = slug_id(segment(tail)).filter(|name| name.contains('.'))
    {
        return Some(Target::MediaItem(format!("{ip}media/{name}"), name.into()));
    }
    // XenforoProfileExtractor
    if let Some(tail) = after.strip_prefix("members/") {
        let seg = segment(tail);
        if !seg.is_empty() {
            return Some(Target::Profile(
                format!("{ip}members/{seg}"),
                page(&tail[seg.len()..]),
            ));
        }
    }
    None
}

/// Per-extraction settings.
struct Ctx<'a> {
    site: &'static Site,
    root: &'static str,
    /// The root's scheme and host.
    base: String,
    http: &'a Http,
    quoted: bool,
    embeds: bool,
    attachments: bool,
    metadata: bool,
    order: Option<String>,
}

fn extract<'a>(
    site: &'static Site,
    instance: &'static Instance,
    http: &'a Http,
    url: Url,
) -> Events<'a> {
    let Some(target) = target(instance, &url) else {
        return crate::sites::fail(error("unsupported XenForo URL"));
    };
    let root = instance.root;
    let base = root[..root[8..].find('/').map_or(root.len(), |i| i + 8)].to_owned();
    let ctx = Arc::new(Ctx {
        site,
        root,
        base,
        http,
        quoted: http.option_bool("quoted", false),
        embeds: http.option_bool("embeds", true),
        attachments: http.option_bool("attachments", true),
        metadata: http.option_bool("metadata", false),
        order: http
            .option_str("order-posts")
            .filter(|o| !o.is_empty())
            .map(str::to_owned),
    });
    let reverse_order = ctx
        .order
        .as_deref()
        .is_some_and(|o| o.starts_with(['d', 'r']));
    match target {
        Target::Post(path, id) => crate::sites::once(async move {
            let page = request_page(&ctx, &format!("{root}{path}{id}/")).await?.1;
            let marker = format!("data-content=\"post-{id}\"");
            let pos = page
                .find(&marker)
                .ok_or_else(|| error("XenForo post not found"))?;
            let mut start = pos.saturating_sub(200);
            while !page.is_char_boundary(start) {
                start -= 1;
            }
            let html = extr(&page[start..], "<article ", "<footer");
            let mut kwdict = Map::new();
            if let Some(thread) = parse_thread(&page) {
                kwdict.insert("thread".into(), thread);
            }
            Ok(post_events(&ctx, &POST, parse_post(html), &kwdict))
        }),
        Target::Thread(path, pnum) => {
            let (pages, reverse) = match ctx.order.as_deref() {
                Some(order) if !order.starts_with(['d', 'r']) => {
                    let params = if order.starts_with('s') {
                        "?order=reaction_score"
                    } else {
                        ""
                    };
                    (pagination(ctx.clone(), path, pnum, params), false)
                }
                Some("reaction") => (
                    pagination(ctx.clone(), path, pnum, "?order=reaction_score"),
                    false,
                ),
                _ => (pagination_reverse(ctx.clone(), path, pnum), true),
            };
            post_pages(ctx, &THREAD, pages, reverse, PageMeta::Thread)
        }
        Target::Profile(path, pnum) => {
            let pages = if reverse_order {
                pagination_reverse(ctx.clone(), path, pnum)
            } else {
                pagination(ctx.clone(), path, pnum, "")
            };
            post_pages(ctx, &PROFILE, pages, reverse_order, PageMeta::Profile)
        }
        Target::Forum(path, pnum) => {
            let pages = pagination(ctx.clone(), path, pnum, "");
            let site = ctx.site;
            pages
                .map(move |page| match page {
                    Ok(page) => {
                        let children: Vec<Url> = forum_threads(&page)
                            .into_iter()
                            .filter_map(|path| {
                                Url::parse(&format!(
                                    "{root}{}",
                                    crate::sites::percent_decode(&path)
                                ))
                                .ok()
                            })
                            .filter(|url| (site.matches)(url))
                            .collect();
                        stream::iter(children)
                            .map(move |url| {
                                crate::sites::queue(http, site.category, "forum", url, &Map::new())
                            })
                            .flatten()
                            .boxed()
                    }
                    Err(e) => crate::sites::fail(e),
                })
                .flatten()
                .boxed()
        }
        Target::MediaUser(ip, user, pnum) => {
            let mut kwdict = Map::new();
            if !ctx.metadata {
                let (slug, id) = user.rsplit_once('.').unwrap_or(("", &user));
                kwdict.insert("author_slug".into(), slug.into());
                kwdict.insert("author_id".into(), id.into());
            }
            let path = format!("{ip}media/users/{user}");
            items_media(ctx, &MEDIA_USER, path, pnum, kwdict, false)
        }
        Target::MediaAlbum(path, _, pnum) => {
            items_media(ctx, &MEDIA_ALBUM, path, pnum, Map::new(), true)
        }
        Target::MediaCategory(path, category, pnum) => {
            let (name, id) = category.rsplit_once('.').unwrap_or(("", &category));
            let mut kwdict = Map::new();
            kwdict.insert("mcategory".into(), name.into());
            kwdict.insert("mcategory_id".into(), id.into());
            items_media(ctx, &MEDIA_CATEGORY, path, pnum, kwdict, false)
        }
        Target::MediaItem(path, name) => crate::sites::once(async move {
            let url = format!("{root}{path}/");
            let (file, media) = if ctx.metadata {
                extract_media_ex(&ctx, &url, &name).await?
            } else {
                extract_media(&url, &name)
            };
            let items = media_item(&ctx, file, media, &Map::new());
            Ok(crate::sites::events(crate::sites::post(
                ctx.site,
                ctx.http,
                &MEDIA_ITEM,
                items.into_iter().collect(),
            )))
        }),
    }
}

/// `request_page`: the final URL and text; a redirect to `/register` or
/// an HTTP 403 means the content needs a session.
async fn request_page(ctx: &Ctx<'_>, url: &str) -> Result<(Url, String)> {
    let requested = ctx.http.api(url)?;
    match ctx.http.page(&requested, &[]).await {
        Ok((location, _)) if location != requested && location.path().ends_with("/register") => {
            Err(auth_required())
        }
        Ok(result) => Ok(result),
        Err(e) if crate::sites::http::refused_status(&e) == Some(403) => Err(auth_required()),
        Err(e) => Err(e),
    }
}

fn auth_required() -> crate::Error {
    error("XenForo content requires a login; pass the forum's session cookies with --cookies")
}

type Pages<'a> = futures_util::stream::BoxStream<'a, Result<String>>;

/// `_pagination`: a given page alone, else every page while there is a
/// `pageNav-jump--next` link.
fn pagination<'a>(
    ctx: Arc<Ctx<'a>>,
    path: String,
    pnum: Option<String>,
    params: &'static str,
) -> Pages<'a> {
    let base = format!("{}{path}", ctx.root);
    let first = match &pnum {
        None => format!("{base}/{params}"),
        Some(pnum) => format!("{base}/page-{pnum}{params}"),
    };
    let follow = pnum.is_none();
    stream::try_unfold(Some((first, 1u64)), move |state| {
        let (ctx, base) = (ctx.clone(), base.clone());
        async move {
            let Some((url, pnum)) = state else {
                return Ok(None);
            };
            let page = request_page(&ctx, &url).await?.1;
            let next = (follow && page.contains("pageNav-jump--next"))
                .then(|| (format!("{base}/page-{}{params}", pnum + 1), pnum + 1));
            Ok(Some((page, next)))
        }
    })
    .boxed()
}

/// `_pagination_reverse`: the last page (`page-9999` redirects there), then
/// each earlier page down to the first.
fn pagination_reverse<'a>(ctx: Arc<Ctx<'a>>, path: String, pnum: Option<String>) -> Pages<'a> {
    let base = format!("{}{path}", ctx.root);
    let first = format!("{base}/page-{}", pnum.as_deref().unwrap_or("9999"));
    let probe = pnum.is_none();
    stream::try_unfold(Some((first, None::<i64>)), move |state| {
        let (ctx, base) = (ctx.clone(), base.clone());
        async move {
            let Some((url, pnum)) = state else {
                return Ok(None);
            };
            let (pnum, page) = match pnum {
                Some(pnum) => (pnum, request_page(&ctx, &url).await?.1),
                None => {
                    let requested = ctx.http.api(&url)?;
                    let (location, page) = request_page(&ctx, &url).await?;
                    if probe && location == requested {
                        return Err(auth_required());
                    }
                    let path = location.path();
                    let pnum = if path.ends_with('/') {
                        1
                    } else {
                        parse_int(path.rsplit('-').next().unwrap_or_default(), 1)
                    };
                    (pnum, page)
                }
            };
            let next = match pnum - 1 {
                n if n > 1 => Some((format!("{base}/page-{n}"), Some(n))),
                1 => Some((format!("{base}/"), Some(1))),
                _ => None,
            };
            Ok(Some((page, next)))
        }
    })
    .boxed()
}

#[derive(Clone, Copy)]
enum PageMeta {
    Thread,
    Profile,
}

/// Posts of each page (`<article …<footer`), with the thread or profile
/// metadata from the first page that has it.
fn post_pages<'a>(
    ctx: Arc<Ctx<'a>>,
    route: &'static Route,
    pages: Pages<'a>,
    reverse: bool,
    meta: PageMeta,
) -> Events<'a> {
    let kwdict = Arc::new(std::sync::Mutex::new(Map::new()));
    pages
        .map(move |page| match page {
            Ok(page) => {
                let mut kw = kwdict.lock().unwrap_or_else(|e| e.into_inner());
                match meta {
                    PageMeta::Thread if !kw.contains_key("thread") => {
                        if let Some(thread) = parse_thread(&page) {
                            kw.insert("thread".into(), thread);
                        }
                    }
                    PageMeta::Profile if !kw.contains_key("profile") => {
                        if let Some(profile) = parse_profile(&page) {
                            kw.insert("profile".into(), profile);
                        }
                    }
                    _ => {}
                }
                let kw = kw.clone();
                let mut posts = extract_iter(&page, "<article ", "<footer");
                if reverse {
                    posts.reverse();
                }
                let events: Vec<Events<'a>> = posts
                    .into_iter()
                    .map(|html| post_events(&ctx, route, parse_post(html), &kw))
                    .collect();
                stream::iter(events).flatten().boxed()
            }
            Err(e) => crate::sites::fail(e),
        })
        .flatten()
        .boxed()
}

/// `(/(?:index\.php\?)?threads/[^"]+)"[^>]+data-xf-init=`.
fn forum_threads(page: &str) -> Vec<String> {
    let mut threads = Vec::new();
    let mut pos = 0;
    while let Some(i) = page[pos..].find("threads/") {
        let at = pos + i;
        pos = at + 1;
        let head = &page[..at];
        let start = if head.ends_with("/index.php?") {
            at - "/index.php?".len()
        } else if head.ends_with('/') {
            at - 1
        } else {
            continue;
        };
        let Some(end) = page[at..].find('"').map(|e| at + e) else {
            break;
        };
        if end == at + "threads/".len() {
            continue;
        }
        let attrs = &page[end + 1..];
        let tag = &attrs[..attrs.find('>').unwrap_or(attrs.len())];
        if let Some(k) = tag.find("data-xf-init=").filter(|k| *k >= 1) {
            threads.push(page[start..end].to_owned());
            pos = end + 1 + k + "data-xf-init=".len();
        }
    }
    threads
}

/// `_parse_post`.
fn parse_post(html: &str) -> Map<String, Value> {
    let mut scan = Scan::new(html);
    let author = scan.extr("data-author=\"", "\"");
    let id = match scan.extr("data-content=\"post-", "\"") {
        "" => scan.extr("data-content=\"profile-post-", "\""),
        id => id,
    };
    let author_url = match scan.extr("itemprop=\"url\" content=\"", "\"") {
        "" => scan.extr("<a href=\"", "\""),
        url => url,
    };
    let date = iso_ts(scan.extr("datetime=\"", "\""));
    let content = match scan.extr("class=\"message-body", "<div class=\"js-selectToQuote") {
        "" => scan.extr("class=\"message-body", "</article>"),
        content => content,
    };
    let attachments = scan.extr("<section class=\"message-attachments\">", "</section>");
    let content = content
        .find("<div class=\"bbWrapper")
        .map_or(content, |pos| &content[pos..]);
    let (slug, author_id) = slug_and_id(author_url);
    let mut post = Map::new();
    post.insert("author".into(), author.into());
    post.insert("id".into(), id.into());
    post.insert("author_url".into(), author_url.into());
    post.insert("date".into(), crate::filter::source_date_json(date));
    post.insert("content".into(), content.trim().into());
    post.insert("attachments".into(), attachments.into());
    post.insert("author_slug".into(), slug.into());
    post.insert("author_id".into(), author_id.into());
    post
}

/// `url[url.rfind("/", 0, -1)+1:-1].rpartition(".")` as slug and ID.
fn slug_and_id(url: &str) -> (String, String) {
    let Some(inner) = url.get(..url.len().saturating_sub(1)) else {
        return Default::default();
    };
    let last = inner.rfind('/').map_or(inner, |i| &inner[i + 1..]);
    match last.rsplit_once('.') {
        Some((slug, id)) => (slug.into(), id.into()),
        None => (String::new(), last.into()),
    }
}

/// gallery-dl `text.slugify`.
fn slugify(value: &str) -> String {
    let kept: String = value
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '_' || c.is_whitespace() || *c == '-')
        .collect();
    let mut out = String::new();
    let mut dash = false;
    for c in kept.chars() {
        if c == '-' || c.is_whitespace() {
            dash = true;
        } else {
            if dash {
                out.push('-');
            }
            dash = false;
            out.push(c);
        }
    }
    if dash {
        out.push('-');
    }
    out.trim_matches(['-', '_']).to_owned()
}

/// `_parse_author`: the author name and profile link.
fn parse_author(author: Option<&Value>, data: &mut Map<String, Value>) {
    let name = author
        .and_then(|a| a.get("name"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    data.insert("author".into(), name.clone().into());
    match author.map(|a| str_of(a, "url")).filter(|u| !u.is_empty()) {
        Some(url) => {
            let (slug, id) = slug_and_id(url);
            data.insert("author_url".into(), url.into());
            data.insert("author_slug".into(), slug.into());
            data.insert("author_id".into(), id.into());
        }
        None => {
            let chars: Vec<char> = name.chars().collect();
            let head: String = chars.iter().take(15).collect();
            let tail: String = chars.iter().skip(15).collect();
            data.insert("author_url".into(), "".into());
            data.insert("author_slug".into(), slugify(&head).into());
            data.insert("author_id".into(), tail.into());
        }
    }
}

/// `Extractor._extract_jsonld`.
pub(crate) fn jsonld(page: &str) -> Option<Value> {
    let data = match extr(page, "<script type=\"application/ld+json\">", "</script>") {
        "" => extr(page, "<script type='application/ld+json'>", "</script>"),
        data => data,
    };
    json_loads(data, "XenForo").ok()
}

/// `userInteractionCount` of the n-th statistic.
fn stat(stats: &Value, index: usize) -> Value {
    stats
        .get(index)
        .and_then(|s| s.get("userInteractionCount"))
        .cloned()
        .unwrap_or(Value::Null)
}

/// `_parse_thread`: the thread's JSON-LD.
fn parse_thread(page: &str) -> Option<Value> {
    let data = jsonld(page)?;
    let main = data.get("mainEntity").unwrap_or(&data);
    let url = match str_of(main, "url") {
        "" => str_of(main, "@id"),
        url => url,
    };
    let id = url
        .get(url.rfind('.').map_or(0, |i| i + 1)..url.len().saturating_sub(1))
        .unwrap_or_default();
    let mut thread = Map::new();
    parse_author(main.get("author"), &mut thread);
    thread.insert("id".into(), id.into());
    thread.insert("url".into(), url.into());
    thread.insert(
        "title".into(),
        main.get("headline").cloned().unwrap_or(Value::Null),
    );
    thread.insert(
        "date".into(),
        crate::filter::source_date_json(iso_ts(str_of(main, "datePublished"))),
    );
    let tags: Vec<Value> = main
        .get("keywords")
        .and_then(Value::as_str)
        .map(|k| k.split(", ").map(Value::from).collect())
        .unwrap_or_default();
    thread.insert("tags".into(), tags.into());
    thread.insert(
        "section".into(),
        main.get("articleSection").cloned().unwrap_or(Value::Null),
    );
    match main.get("interactionStatistic") {
        Some(stats @ Value::Array(_)) => {
            thread.insert("views".into(), stat(stats, 0));
            thread.insert("posts".into(), stat(stats, 1));
        }
        stats => {
            thread.insert("views".into(), (-1).into());
            thread.insert(
                "posts".into(),
                stats
                    .and_then(|s| s.get("userInteractionCount"))
                    .cloned()
                    .unwrap_or(Value::Null),
            );
        }
    }
    Some(Value::Object(thread))
}

/// `_parse_album`.
fn parse_album(page: &str) -> Option<Value> {
    let data = jsonld(page)?;
    let main = data.get("mainEntity")?;
    let url = match str_of(main, "url") {
        "" => str_of(main, "@id"),
        url => url,
    };
    let (slug, id) = slug_and_id(url);
    let mut album = Map::new();
    parse_author(main.get("author"), &mut album);
    album.insert("id".into(), id.into());
    album.insert("url".into(), url.into());
    album.insert("slug".into(), crate::sites::percent_decode(&slug).into());
    album.insert(
        "title".into(),
        main.get("headline").cloned().unwrap_or(Value::Null),
    );
    album.insert(
        "description".into(),
        main.get("description").cloned().unwrap_or(Value::Null),
    );
    album.insert(
        "date".into(),
        crate::filter::source_date_json(iso_ts(str_of(main, "dateCreated"))),
    );
    if let Some(stats @ Value::Array(_)) = main.get("interactionStatistic") {
        for (index, key) in ["count", "likes", "views", "comments"].iter().enumerate() {
            album.insert((*key).into(), stat(stats, index));
        }
    }
    Some(Value::Object(album))
}

/// `_parse_profile`.
fn parse_profile(page: &str) -> Option<Value> {
    let user = jsonld(page)?;
    let main = user.get("mainEntity")?;
    let url = match str_of(&user, "url") {
        "" => str_of(main, "@id"),
        url => url,
    };
    let (slug, id) = slug_and_id(url);
    let mut profile = Map::new();
    profile.insert(
        "id".into(),
        main.get("identifier")
            .filter(|v| crate::sites::art::truthy(Some(v)))
            .cloned()
            .unwrap_or_else(|| id.into()),
    );
    profile.insert("url".into(), url.into());
    profile.insert("slug".into(), crate::sites::percent_decode(&slug).into());
    for (key, name) in [
        ("name", "name"),
        ("avatar", "image"),
        ("description", "description"),
    ] {
        profile.insert(key.into(), main.get(name).cloned().unwrap_or(Value::Null));
    }
    profile.insert(
        "date".into(),
        crate::filter::source_date_json(iso_ts(str_of(&user, "dateCreated"))),
    );
    if let Some(stats @ Value::Array(_)) = main.get("interactionStatistic") {
        profile.insert("follows".into(), stat(stats, 0));
        profile.insert("likes".into(), stat(stats, 1));
    }
    Some(Value::Object(profile))
}

/// `_remove_quotes`: drop `<blockquote>` blocks, nested ones included.
pub(crate) fn remove_quotes(content: &str) -> String {
    let mut content = content.to_owned();
    while let Some(beg) = content.find("<blockquote") {
        let Some(mut end) = content[beg..].find("</blockquote").map(|i| i + beg) else {
            break;
        };
        let nested = content
            .get(beg + 11..end)
            .map_or(0, |inner| inner.matches("<blockquote").count());
        for _ in 0..nested {
            match content
                .get(end + 13..)
                .and_then(|rest| rest.find("</blockquote"))
            {
                Some(i) => end = end + 13 + i,
                None => break,
            }
        }
        let tail = content.get(end + 13..).unwrap_or_default().to_owned();
        content = format!("{}{tail}", &content[..beg]);
    }
    content
}

/// One match of upstream's `extract_urls` pattern:
/// (video, inline link, bb wrapper, external).
type Found = (String, String, String, String);

/// The first `\ssrc="…"` with a value, then the next `</video>`.
fn match_video(text: &str) -> Option<(usize, Found)> {
    let body = text.strip_prefix("<video ")?;
    let mut from = 0;
    let src_end = loop {
        let at = body[from..].find("src=\"")? + from;
        let preceded = body[..at]
            .chars()
            .next_back()
            .is_some_and(char::is_whitespace);
        let value = &body[at + 5..];
        match value.find('"') {
            Some(len) if preceded && len > 0 => break at + 5 + len + 1,
            _ => from = at + 5,
        }
    };
    let close = body[src_end..].find("</video>")? + src_end;
    let found = (
        body[..close].to_owned(),
        String::new(),
        String::new(),
        String::new(),
    );
    Some(("<video ".len() + close + "</video>".len(), found))
}

/// `<a [^>]*?href="([^"]*?/(?:index\.php\?)?attachments/[^"]+".*?)</a>`.
fn match_attachment(text: &str) -> Option<(usize, Found)> {
    text.strip_prefix("<a ")?;
    let tag_end = text.find('>').unwrap_or(text.len());
    let mut from = 0;
    while let Some(at) = text[from..tag_end].find("href=\"") {
        let start = from + at + 6;
        let value = &text[start..];
        let len = value.find('"').unwrap_or(value.len());
        let href = &value[..len];
        let attachment = href
            .match_indices("/attachments/")
            .any(|(i, m)| i + m.len() < len)
            || href
                .match_indices("/index.php?attachments/")
                .any(|(i, m)| i + m.len() < len);
        if attachment && len < value.len() {
            let close = text[start..].find("</a>")? + start;
            let found = (
                String::new(),
                text[start..close].to_owned(),
                String::new(),
                String::new(),
            );
            return Some((close + 4, found));
        }
        from = start;
    }
    None
}

/// `<div class="bb(?:Image|Media)Wrapper[^>]*?data-src="([^"]+".*?) />`.
fn match_wrapper(text: &str) -> Option<(usize, Found)> {
    if !text.starts_with("<div class=\"bbImageWrapper")
        && !text.starts_with("<div class=\"bbMediaWrapper")
    {
        return None;
    }
    let tag_end = text.find('>').unwrap_or(text.len());
    let at = text[..tag_end].find("data-src=\"")? + 10;
    let len = text[at..].find('"')?;
    if len == 0 {
        return None;
    }
    let close = text[at + len..].find(" />")? + at + len;
    let found = (
        String::new(),
        String::new(),
        text[at..close].to_owned(),
        String::new(),
    );
    Some((close + 3, found))
}

/// `(?:<a [^>]*?href="|<iframe [^>]*?src="|onclick="loadMedia\(this, ')`
/// then `([^"']+[^<]*?<)`.
fn match_external(text: &str) -> Option<(usize, Found)> {
    let start = if text.starts_with("<a ") || text.starts_with("<iframe ") {
        let attr = if text.starts_with("<a ") {
            "href=\""
        } else {
            "src=\""
        };
        let tag_end = text.find('>').unwrap_or(text.len());
        text[..tag_end].find(attr)? + attr.len()
    } else if text.starts_with("onclick=\"loadMedia(this, '") {
        "onclick=\"loadMedia(this, '".len()
    } else {
        return None;
    };
    let value = &text[start..];
    let run = value.find(['"', '\'']).unwrap_or(value.len());
    if run == 0 {
        return None;
    }
    let end = match value[run..].find('<') {
        Some(i) => run + i + 1,
        None => value[1..run].rfind('<').map(|i| i + 2)?,
    };
    let found = (
        String::new(),
        String::new(),
        String::new(),
        value[..end].to_owned(),
    );
    Some((start + end, found))
}

/// `extract_urls(content)`: every match, scanning left to right.
fn extract_urls(content: &str) -> Vec<Found> {
    let mut found = Vec::new();
    let mut pos = 0;
    while pos < content.len() {
        let Some(next) = content[pos..].find(['<', 'o']).map(|i| i + pos) else {
            break;
        };
        let text = &content[next..];
        let matched = match_video(text)
            .or_else(|| match_attachment(text))
            .or_else(|| match_wrapper(text))
            .or_else(|| match_external(text));
        match matched {
            Some((len, groups)) => {
                found.push(groups);
                pos = next + len;
            }
            None => pos = next + text.chars().next().map_or(1, char::len_utf8),
        }
    }
    found
}

/// `_extract_attachments`: each `<li>`'s `href` value onwards.
fn extract_attachments(attachments: &str) -> Vec<Found> {
    extract_iter(attachments, "<li", "</li>")
        .into_iter()
        .filter_map(|att| {
            let at = att.char_indices().find_map(|(i, c)| {
                (c.is_whitespace()
                    && (att[i + c.len_utf8()..].starts_with("href=\"")
                        || att[i + c.len_utf8()..].starts_with("href='")))
                .then_some(i + c.len_utf8() + 6)
            })?;
            let value = &att[at..];
            (!value.is_empty()).then(|| {
                (
                    String::new(),
                    value.to_owned(),
                    String::new(),
                    String::new(),
                )
            })
        })
        .collect()
}

/// `_extract_embeds`: s9e media embeds of TikTok, Reddit and Imgur.
fn extract_embeds(content: &str) -> Vec<Found> {
    let mut found = Vec::new();
    for embed in extract_iter(content, "data-s9e-mediaembed-iframe='", "'") {
        let Ok(Value::Array(values)) = json_loads(embed, "XenForo") else {
            continue;
        };
        let mut data = Map::new();
        for pair in values.chunks(2) {
            if let [Value::String(key), value] = pair {
                data.insert(key.clone(), value.clone());
            }
        }
        let src = data.get("src").and_then(Value::as_str).unwrap_or_default();
        if src.is_empty() {
            continue;
        }
        let frag = src.split_once('#').map_or(src, |(_, frag)| frag);
        let url = match data.get("data-s9e-mediaembed").and_then(Value::as_str) {
            Some("tiktok") => format!("https://www.tiktok.com/@/video/{frag}"),
            Some("reddit") => format!("https://embed.reddit.com/r/{frag}"),
            Some("imgur") => format!("https://imgur.com/{frag}"),
            _ => continue,
        };
        found.push((String::new(), String::new(), String::new(), url));
    }
    found
}

/// `binascii.a2b_base64(...).decode()`.
fn b64(text: &str) -> Option<String> {
    let text = text.trim_end_matches('=');
    let bytes = base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(text)
        .ok()?;
    String::from_utf8(bytes).ok()
}

/// The external link of an `ext` match, `None` for skipped ones.
fn external_link(ext: &str) -> Option<String> {
    if ext.starts_with('#') {
        return None;
    }
    if let Some(rest) = ext.strip_prefix("//") {
        let rest = rest.split('\'').next().unwrap_or_default();
        return Some(format!("https://{rest}"));
    }
    if let Some(query) = ext.strip_prefix("/goto/link-confirmation?") {
        let params = crate::sites::art::parse_query(&unescape(query));
        let url = params.iter().find(|(k, _)| k == "url")?;
        return b64(&url.1);
    }
    if ext.starts_with("/redirect/") {
        let to = extr(ext, "?to=", "&");
        return if to.is_empty() {
            Some(unescape(extr(ext, ">", "<").trim()))
        } else {
            b64(to)
        };
    }
    if ext.starts_with('/') {
        return None;
    }
    Some(ext.split('"').next().unwrap_or_default().to_owned())
}

/// The `num`, `num_internal` and `num_external` counters of one post.
#[derive(Default)]
struct Counters {
    num: usize,
    internal: usize,
    external: usize,
}

impl Counters {
    fn bump(&mut self, data: &mut Map<String, Value>, kind: &str) {
        self.num += 1;
        if kind == "external" {
            self.external += 1;
        } else {
            self.internal += 1;
        }
        data.insert("num".into(), self.num.into());
        data.insert("num_internal".into(), self.internal.into());
        data.insert("num_external".into(), self.external.into());
        data.insert("type".into(), kind.into());
    }
}

/// `items()` for one post: its files as a post, then its queued links.
fn post_events<'a>(
    ctx: &Ctx<'a>,
    route: &'static Route,
    mut post: Map<String, Value>,
    kwdict: &Map<String, Value>,
) -> Events<'a> {
    let content = str_of(&Value::Object(post.clone()), "content").to_owned();
    let content = if ctx.quoted {
        content
    } else {
        remove_quotes(&content)
    };
    let mut urls = extract_urls(&content);
    if ctx.embeds && content.contains("data-s9e-mediaembed-iframe=") {
        urls.extend(extract_embeds(&content));
    }
    let attachments = str_of(&Value::Object(post.clone()), "attachments").to_owned();
    if ctx.attachments && !attachments.is_empty() {
        urls.extend(extract_attachments(&attachments));
    }
    post.insert("count".into(), urls.len().into());
    let post_id = post.get("id").map(crate::sites::scalar).unwrap_or_default();
    let author = post
        .get("author")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let timestamp = crate::filter::source_date_from_json(&post["date"]).flatten();
    let mut data = kwdict.clone();
    data.insert("post".into(), Value::Object(post));
    data.insert("count".into(), urls.len().into());
    let mut counters = Counters::default();
    let mut id_last: Option<i64> = None;
    let mut items = Vec::new();
    let mut children = Vec::new();
    for (video, inline, bb, ext) in urls {
        if !ext.is_empty() {
            let Some(link) = external_link(&ext) else {
                continue;
            };
            counters.bump(&mut data, "external");
            if let Ok(url) = Url::parse(&link)
                && crate::sites::plain(&url)
                && crate::sites::find(&url).is_some()
            {
                children.push((url, data.clone()));
            }
        } else if !video.is_empty() {
            counters.bump(&mut data, "video");
            let src = extr(&video, "src=\"", "\"");
            crate::sites::art::nameext_into(src, &mut data);
            let filename = data["filename"].as_str().unwrap_or_default().to_owned();
            let id = parse_int(filename.split('-').next().unwrap_or_default(), 0);
            data.insert("id".into(), id.into());
            let url = if src.starts_with('/') {
                format!("{}{src}", ctx.base)
            } else {
                src.to_owned()
            };
            if let Some(url) = crate::sites::media::web_url(&url) {
                items.push(file_item(
                    url,
                    &data,
                    &post_id,
                    &author,
                    timestamp,
                    counters.num,
                ));
            }
        } else {
            let inline = if bb.is_empty() { inline } else { bb };
            let url = match inline.find('"') {
                Some(i) => &inline[..i],
                None => &inline[..inline.len().saturating_sub(1)],
            };
            let inner = url.get(..url.len().saturating_sub(1)).unwrap_or_default();
            let last = &url[inner.rfind('/').unwrap_or(0)..];
            let last = last.trim_matches('/');
            let (name, id) = last.rsplit_once('.').unwrap_or(("", last));
            let id = parse_int(id, 0);
            data.insert("id".into(), id.into());
            if id != 0 {
                if id_last == Some(id) {
                    id_last = None;
                    continue;
                }
                id_last = Some(id);
            }
            let alt = match extr(&inline, "alt=\"", "\"") {
                "" => extr(&inline, "title=\"", "\""),
                alt => alt,
            };
            if alt.is_empty() {
                let (stem, ext) = name.rsplit_once('-').unwrap_or(("", name));
                data.insert("filename".into(), stem.into());
                data.insert("extension".into(), ext.into());
            } else {
                let (stem, mut ext) = crate::sites::art::nameext_from_name(alt);
                if ext.is_empty() {
                    ext = name.rsplit('-').next().unwrap_or_default().to_owned();
                }
                data.insert("filename".into(), stem.into());
                data.insert("extension".into(), ext.into());
            }
            counters.bump(&mut data, "inline");
            let url = if url.starts_with('/') {
                format!("{}{url}", ctx.base)
            } else {
                url.to_owned()
            };
            if let Some(url) = crate::sites::media::web_url(&url) {
                items.push(file_item(
                    url,
                    &data,
                    &post_id,
                    &author,
                    timestamp,
                    counters.num,
                ));
            }
        }
    }
    let files = crate::sites::events(crate::sites::post(ctx.site, ctx.http, route, items));
    let (http, category, subcategory) = (ctx.http, ctx.site.category, route.subcategory);
    let queued = stream::iter(children)
        .map(move |(url, data)| crate::sites::queue(http, category, subcategory, url, &data))
        .flatten();
    files.chain(queued).boxed()
}

fn file_item(
    url: Url,
    data: &Map<String, Value>,
    post_id: &str,
    author: &Option<String>,
    timestamp: Option<i64>,
    num: usize,
) -> Item {
    Item::new(url, data.clone())
        .ids(post_id, num.to_string())
        .user(author.clone())
        .timestamp(timestamp)
}

/// `_extract_media`: the `full` download of a media page.
fn extract_media(url: &str, file: &str) -> (String, Map<String, Value>) {
    let (name, id) = file.rsplit_once('.').unwrap_or(("", file));
    let (stem, ext) = name.rsplit_once('-').unwrap_or(("", name));
    let mut media = Map::new();
    media.insert("id".into(), id.into());
    media.insert("filename".into(), stem.into());
    media.insert("extension".into(), ext.into());
    (format!("{url}full"), media)
}

/// `_extract_media_ex`: the media page's JSON-LD (`metadata` option).
async fn extract_media_ex(
    ctx: &Ctx<'_>,
    url: &str,
    file: &str,
) -> Result<(String, Map<String, Value>)> {
    let page = request_page(ctx, url).await?.1;
    let schema = jsonld(&page).ok_or_else(|| error("XenForo media page has no JSON-LD"))?;
    let main = schema
        .get("mainEntity")
        .cloned()
        .ok_or_else(|| error("XenForo media page has no JSON-LD"))?;
    let (stem, ext) = crate::sites::art::nameext_from_name(str_of(&main, "name"));
    let dimension = |key: &str| {
        main.get(key)
            .map(|v| parse_int(str_of(v, "name").split(' ').next().unwrap_or_default(), 0))
            .unwrap_or(0)
    };
    let mut media = Map::new();
    media.insert("filename".into(), stem.into());
    media.insert("extension".into(), ext.into());
    media.insert("schema".into(), schema.clone());
    media.insert(
        "id".into(),
        file.rsplit('.').next().unwrap_or_default().into(),
    );
    media.insert(
        "size".into(),
        main.get("contentSize").cloned().unwrap_or(Value::Null),
    );
    media.insert(
        "description".into(),
        main.get("description").cloned().unwrap_or(Value::Null),
    );
    media.insert(
        "date".into(),
        crate::filter::source_date_json(iso_ts(str_of(&main, "dateCreated"))),
    );
    media.insert("width".into(), dimension("width").into());
    media.insert("height".into(), dimension("height").into());
    parse_author(main.get("author"), &mut media);
    if let Some(ext) = main
        .get("encodingFormat")
        .filter(|v| crate::sites::art::truthy(Some(v)))
    {
        media.insert("extension".into(), ext.clone());
    }
    if let Some(stats @ Value::Array(_)) = main.get("interactionStatistic") {
        for (index, key) in ["views", "likes", "comments"].iter().enumerate() {
            media.insert((*key).into(), stat(stats, index));
        }
    }
    Ok((str_of(&main, "contentUrl").to_owned(), media))
}

fn media_item(
    ctx: &Ctx<'_>,
    file: String,
    mut media: Map<String, Value>,
    kwdict: &Map<String, Value>,
) -> Option<Item> {
    let url = crate::sites::art::file_url(&file, &ctx.base)?;
    for (key, value) in kwdict {
        media.entry(key.clone()).or_insert_with(|| value.clone());
    }
    let id = media
        .get("id")
        .map(crate::sites::scalar)
        .unwrap_or_default();
    let author = media
        .get("author")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Some(Item::new(url, media).ids(id.clone(), id).user(author))
}

/// `items_media`: every media page of a user, album or category.
fn items_media<'a>(
    ctx: Arc<Ctx<'a>>,
    route: &'static Route,
    path: String,
    pnum: Option<String>,
    kwdict: Map<String, Value>,
    album: bool,
) -> Events<'a> {
    let reverse = ctx
        .order
        .as_deref()
        .is_some_and(|o| o.starts_with(['d', 'r']));
    let pages = if reverse {
        pagination_reverse(ctx.clone(), path, pnum)
    } else {
        pagination(ctx.clone(), path, pnum, "")
    };
    let kwdict = Arc::new(std::sync::Mutex::new(kwdict));
    pages
        .map(move |page| {
            let (ctx, kwdict) = (ctx.clone(), kwdict.clone());
            match page {
                Ok(page) => {
                    if album {
                        let mut kw = kwdict.lock().unwrap_or_else(|e| e.into_inner());
                        if !kw.contains_key("album")
                            && let Some(data) = parse_album(&page)
                        {
                            kw.insert("album".into(), data);
                        }
                    }
                    let kw = kwdict.lock().unwrap_or_else(|e| e.into_inner()).clone();
                    let mut entries: Vec<(String, String)> = page
                        .split("<div class=\"itemList-item js-inlineModContainer")
                        .skip(1)
                        .map(|html| {
                            let mut scan = Scan::new(html);
                            let href = scan.extr("href=\"", "\"").to_owned();
                            let name = scan.extr("alt='", "'").to_owned();
                            (href, name)
                        })
                        .collect();
                    if reverse {
                        entries.reverse();
                    }
                    stream::iter(entries)
                        .then(move |(href, name)| {
                            let (ctx, kw) = (ctx.clone(), kw.clone());
                            async move {
                                let inner = href.get(..href.len().saturating_sub(1)).unwrap_or("");
                                let file = &inner[inner.rfind('/').map_or(0, |i| i + 1)..];
                                let url = format!("{}{href}", ctx.base);
                                let (file_url, mut media) = if ctx.metadata {
                                    extract_media_ex(&ctx, &url, file).await?
                                } else {
                                    extract_media(&url, file)
                                };
                                if file_url.ends_with("/register/full") {
                                    return Ok(crate::sites::events(Vec::new()));
                                }
                                if !ctx.metadata && !name.is_empty() {
                                    let (stem, ext) =
                                        crate::sites::art::nameext_from_name(&unescape(&name));
                                    media.insert("filename".into(), stem.into());
                                    media.insert("extension".into(), ext.into());
                                }
                                let items = media_item(&ctx, file_url, media, &kw);
                                Ok(crate::sites::events(crate::sites::post(
                                    ctx.site,
                                    ctx.http,
                                    route,
                                    items.into_iter().collect(),
                                )))
                            }
                        })
                        .map(|result: Result<Events<'a>>| result.unwrap_or_else(crate::sites::fail))
                        .flatten()
                        .boxed()
                }
                Err(e) => crate::sites::fail(e),
            }
        })
        .flatten()
        .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, queued, response, serve};

    fn simpcity(raw: &str) -> Option<Target> {
        target(
            &Instance {
                root: "https://simpcity.cr",
                hosts: &[
                    "simpcity.cr",
                    "www.simpcity.cr",
                    "simpcity.su",
                    "www.simpcity.su",
                ],
                prefix: "",
            },
            &Url::parse(raw).unwrap(),
        )
    }

    #[test]
    fn classifies_urls() {
        let t = simpcity;
        assert_eq!(
            t("https://simpcity.cr/threads/title.12345/post-54321"),
            Some(Target::Post(
                "/threads/title.12345/post-".into(),
                "54321".into()
            ))
        );
        assert_eq!(
            t("https://simpcity.su/threads/title.12345/page-3#post-54321"),
            Some(Target::Post(
                "/threads/title.12345/page-3#post-".into(),
                "54321".into()
            ))
        );
        assert_eq!(
            t("https://simpcity.cr/posts/54321/"),
            Some(Target::Post("/posts/".into(), "54321".into()))
        );
        assert_eq!(
            t("https://www.simpcity.cr/threads/my.title.12345/page-9"),
            Some(Target::Thread(
                "/threads/my.title.12345".into(),
                Some("9".into())
            ))
        );
        assert_eq!(
            t("https://simpcity.cr/index.php?threads/title.12345/"),
            Some(Target::Thread(
                "/index.php?threads/title.12345".into(),
                None
            ))
        );
        assert_eq!(
            t("https://simpcity.cr/forums/videos.44/page-2"),
            Some(Target::Forum("/forums/videos.44".into(), Some("2".into())))
        );
        assert_eq!(
            t("https://simpcity.cr/media/users/bob.12/"),
            Some(Target::MediaUser("/".into(), "bob.12".into(), None))
        );
        assert_eq!(
            t("https://simpcity.cr/members/bob.12/#xfmgMedia"),
            Some(Target::MediaUser("/".into(), "bob.12".into(), None))
        );
        assert_eq!(
            t("https://simpcity.cr/media/albums/trip.7/page-2"),
            Some(Target::MediaAlbum(
                "/media/albums/trip.7".into(),
                "trip.7".into(),
                Some("2".into())
            ))
        );
        assert_eq!(
            t("https://simpcity.cr/media/categories/pics.3/"),
            Some(Target::MediaCategory(
                "/media/categories/pics.3".into(),
                "pics.3".into(),
                None
            ))
        );
        assert_eq!(
            t("https://simpcity.cr/media/photo-jpg.123/"),
            Some(Target::MediaItem(
                "/media/photo-jpg.123".into(),
                "photo-jpg.123".into()
            ))
        );
        assert_eq!(
            t("https://simpcity.cr/members/bob.12/page-2"),
            Some(Target::Profile("/members/bob.12".into(), Some("2".into())))
        );
        let nudostar = Url::parse("https://nudostar.com/forum/threads/x.1/").unwrap();
        assert!((NUDOSTARFORUM.matches)(&nudostar));
        let outside = Url::parse("https://nudostar.com/threads/x.1/").unwrap();
        assert!(!(NUDOSTARFORUM.matches)(&outside));
        let prefixed = Url::parse("https://nudostar.com/forumx/threads/x.1/").unwrap();
        assert!(!(NUDOSTARFORUM.matches)(&prefixed));
        for raw in [
            "https://simpcity.cr/threads/",
            "https://simpcity.cr/threads/title/",
            "https://simpcity.cr/media/photo/",
            "https://simpcity.cr/whats-new/",
            "https://simpcity.cr.evil/threads/x.1/",
            "https://evil.simpcity.cr/threads/x.1/",
            "https://simpcity.cr:81/threads/x.1/",
            "https://u@simpcity.cr/threads/x.1/",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[test]
    fn removes_quotes_and_finds_urls() {
        assert_eq!(
            remove_quotes("a<blockquote>x<blockquote>y</blockquote>z</blockquote>b"),
            "ab"
        );
        let content = r##"<video loop>
            <source src="/data/video/12/12345-abc.mp4" /></video>
            <a href="/attachments/pic-jpg.77/" target="_blank"><img alt="pic.jpg"></a>
            <div class="bbImageWrapper " data-src="https://simpcity.cr/attachments/x-png.88/" title="Shot.png" />
            <a href="https://example.org/page" target="_blank">link</a>
            <a href="#top">top</a>
            <iframe src="https://example.org/embed" ></iframe>"##;
        let urls = extract_urls(content);
        assert_eq!(urls.len(), 6);
        assert!(urls[0].0.contains("src=\"/data/video/12/12345-abc.mp4\""));
        assert!(urls[1].1.starts_with("/attachments/pic-jpg.77/\""));
        assert!(
            urls[2]
                .2
                .starts_with("https://simpcity.cr/attachments/x-png.88/\"")
        );
        assert_eq!(
            urls[3].3,
            "https://example.org/page\" target=\"_blank\">link<"
        );
        assert_eq!(urls[4].3, "#top\">top<");
        assert_eq!(
            external_link("/goto/link-confirmation?url=aHR0cHM6Ly9leGFtcGxlLm9yZy94&amp;s=1"),
            Some("https://example.org/x".into())
        );
        assert_eq!(
            external_link("/redirect/?to=aHR0cHM6Ly9leGFtcGxlLm9yZy95&amp;x"),
            Some("https://example.org/y".into())
        );
        assert_eq!(
            external_link("//cdn.test/a'b"),
            Some("https://cdn.test/a".into())
        );
        assert_eq!(external_link("/internal"), None);
        assert_eq!(slugify("Hello, World -_x"), "hello-world-_x");
    }

    const THREAD_LD: &str = r#"<script type="application/ld+json">{"@type": "DiscussionForumPosting",
        "mainEntity": {"url": "https://simpcity.cr/threads/title.12345/", "headline": "Title",
        "datePublished": "2024-01-02T03:04:05+00:00", "articleSection": "Pics",
        "keywords": "a, b", "author": {"name": "Op", "url": "https://simpcity.cr/members/op.9/"},
        "interactionStatistic": [{"userInteractionCount": 100}, {"userInteractionCount": 7}]}}</script>"#;

    fn article(id: u64, body: &str) -> String {
        format!(
            r#"<article data-author="Bob" data-content="post-{id}">
            <a href="https://simpcity.cr/members/bob.12/">Bob</a>
            <time datetime="2024-01-03T03:04:05+0000"></time>
            <div class="message-body"><div class="bbWrapper">{body}</div>
            <div class="js-selectToQuote"></div>
            <section class="message-attachments"><ul><li class="file"><a class="file-preview" href="/attachments/doc-pdf.99/">x</a></li></ul></section>
            <footer>"#
        )
    }

    #[tokio::test]
    async fn thread_pages_newest_first() {
        let body = r#"<blockquote>quoted <img src="/q.jpg"></blockquote>
            <video loop><source src="/data/video/1/555-clip.mp4" /></video>
            <div class="bbImageWrapper" data-src="/attachments/shot-png.88/" title="Shot.png" />
            <a href="https://example.org/page">ext</a>"#;
        let page2 = format!("{THREAD_LD}{}{}", article(1, body), article(2, ""));
        let page1 = format!("{THREAD_LD}{}", article(3, ""));
        let redirect = response(
            "302 Found\r\nLocation: /threads/title.12345/page-2",
            "text/html",
            b"",
        );
        let (root, server) = serve(vec![redirect, html(&page2), html(&page1)]);
        let http = http("simpcity", root);
        let files = media((SIMPCITY.extract)(
            &http,
            Url::parse("https://simpcity.cr/threads/title.12345/").unwrap(),
        ))
        .await;
        let ids: Vec<String> = files
            .iter()
            .map(|f| crate::sites::scalar(&keywords(f)["post"]["id"]))
            .collect();
        assert_eq!(ids, ["2", "1", "1", "1", "3"]);
        let video = keywords(&files[1]);
        assert_eq!(video["type"], "video");
        assert_eq!(video["id"], 555);
        assert_eq!(video["num"], 1);
        assert_eq!(video["count"], 4);
        assert_eq!(video["thread"]["title"], "Title");
        assert_eq!(video["thread"]["id"], "12345");
        assert_eq!(video["thread"]["posts"], 7);
        assert_eq!(video["thread"]["author_slug"], "op");
        assert_eq!(video["post"]["author_id"], "12");
        assert_eq!(
            files[1].url.as_str(),
            "https://simpcity.cr/data/video/1/555-clip.mp4"
        );
        let inline = keywords(&files[2]);
        assert_eq!(inline["type"], "inline");
        assert_eq!(inline["filename"], "Shot");
        assert_eq!(inline["extension"], "png");
        assert_eq!(inline["num"], 2);
        assert_eq!(
            files[2].url.as_str(),
            "https://simpcity.cr/attachments/shot-png.88/"
        );
        let attachment = keywords(&files[3]);
        assert_eq!(attachment["filename"], "doc");
        assert_eq!(attachment["extension"], "pdf");
        assert_eq!(attachment["num"], 4);
        assert_eq!(attachment["num_external"], 1);
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "1_01_555_555-clip.mp4");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("simpcity/Pics/Title (12345)")
        );
        let (_, identity) = crate::sites::archive_key(&files[2]).unwrap();
        assert_eq!(
            identity,
            r#"["{post[id]}/{type[0]}{id}_{filename}",["1","i","88","Shot"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /threads/title.12345/page-9999 "));
        assert!(requests[1].starts_with("GET /threads/title.12345/page-2 "));
        assert!(requests[2].starts_with("GET /threads/title.12345/ "));
    }

    #[tokio::test]
    async fn post_queues_supported_links() {
        let body = r#"<a href="https://speakerdeck.com/user/talk">deck</a>
            <a href="https://example.org/page">other</a>"#;
        let page = format!("{THREAD_LD}<div>{}", article(54321, body));
        let (root, server) = serve(vec![html(&page)]);
        let options = serde_json::json!({"attachments": false})
            .as_object()
            .cloned();
        let http = http("simpcity", root).with_options(options.map(std::sync::Arc::new));
        let children = queued((SIMPCITY.extract)(
            &http,
            Url::parse("https://simpcity.cr/threads/title.12345/post-54321").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 1);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /threads/title.12345/post-54321/ "));
    }

    #[tokio::test]
    async fn media_album_pages() {
        let album_ld = r#"<script type="application/ld+json">{"mainEntity": {
            "url": "https://simpcity.cr/media/albums/trip.7/", "headline": "Trip",
            "dateCreated": "2024-01-02T03:04:05+00:00", "author": {"name": "Bob"}}}</script>"#;
        let page = format!(
            r#"{album_ld}<div class="itemList-item js-inlineModContainer"><a href="/media/beach-jpg.31/"><img alt='Beach &amp; sun.jpg'></a>
            <div class="itemList-item js-inlineModContainer"><a href="/media/hill-png.32/"><img></a>"#
        );
        let (root, server) = serve(vec![html(&page)]);
        let http = http("simpcity", root);
        let files = media((SIMPCITY.extract)(
            &http,
            Url::parse("https://simpcity.cr/media/albums/trip.7/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].url.as_str(),
            "https://simpcity.cr/media/beach-jpg.31/full"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["filename"], "Beach & sun");
        assert_eq!(k["id"], "31");
        assert_eq!(k["album"]["slug"], "trip");
        assert_eq!(k["album"]["author_slug"], "bob");
        assert_eq!(keywords(&files[1])["filename"], "hill");
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "hill.png");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("simpcity/Media/Albums/trip (7)")
        );
        let (_, identity) = crate::sites::archive_key(&files[1]).unwrap();
        assert_eq!(identity, r#"["{id}",["32"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /media/albums/trip.7/ "));
    }

    #[tokio::test]
    async fn forum_queues_threads() {
        let page = r#"<a href="/threads/one.1/" class="x" data-xf-init="preview">
            <a href="/index.php?threads/two.2/" data-tp="1" data-xf-init="preview">
            <a href="/threads/three.3/">no init</a>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = http("simpcity", root);
        let children = queued((SIMPCITY.extract)(
            &http,
            Url::parse("https://simpcity.cr/forums/pics.4/page-2").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 2);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /forums/pics.4/page-2 "));
    }

    #[tokio::test]
    async fn refused_pages_need_cookies() {
        let (root, server) = serve(vec![response("403 Forbidden", "text/html", b">Log in<")]);
        let http = http("simpcity", root);
        assert!(
            crate::sites::testing::fails((SIMPCITY.extract)(
                &http,
                Url::parse("https://simpcity.cr/threads/x.1/").unwrap(),
            ))
            .await
        );
        server.join().unwrap();
    }

    #[test]
    fn media_item_needs_no_request() {
        let (url, media) = extract_media("https://simpcity.cr/media/photo-jpg.5/", "photo-jpg.5");
        assert_eq!(url, "https://simpcity.cr/media/photo-jpg.5/full");
        assert_eq!(media["filename"], "photo");
        assert_eq!(media["extension"], "jpg");
        assert_eq!(media["id"], "5");
    }
}
