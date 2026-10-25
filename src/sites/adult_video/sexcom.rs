//! Sex.com pins, related pins, user pins, likes and boards, the account
//! feed and searches, mirroring gallery_dl/extractor/sexcom.py.
//!
//! Pin pages come in three layouts: legacy `/pin/` pages, picture and GIF
//! pages, and video pages whose HLS stream goes to yt-dlp (it needs the
//! site's CloudFront cookies from `--cookies`). The `gifs` option (default
//! on) swaps animated WebP files for GIFs; upstream's `_fallback` URLs are
//! not emitted. Listings fetch each pin's page lazily; one post per pin.
use super::manifest_item;
use crate::extract::ManifestProtocol;
use crate::sites::art::{
    Scan, date, extr, on_host, parse_int, parse_iso, parse_query, segment, split_html, str_of,
};
use crate::sites::manga::text::{partition, partition_last, rextr};
use crate::sites::{Events, Http, Item, Route, Site, unescape};
use crate::{Result, error};
use serde_json::{Map, Value};
use std::collections::VecDeque;
use url::Url;

const ROOT: &str = "https://www.sex.com";
const IMAGES: &str = "https://imagex1.sx.cdn.live";
const FILENAME: &str = "{pin_id}{title:? //}.{extension}";
const ARCHIVE: &str = "{pin_id}";
/// Search result pages read at most.
const MAX_PAGES: i64 = 1000;

static PIN: Route = Route::new("pin", FILENAME, &["{category}"], ARCHIVE);
static RELATED_PIN: Route = Route::new(
    "related-pin",
    FILENAME,
    &["{category}", "related {original_pin[pin_id]}"],
    ARCHIVE,
);
static PINS: Route = Route::new("pins", FILENAME, &["{category}", "{user}"], ARCHIVE);
static LIKES: Route = Route::new(
    "likes",
    FILENAME,
    &["{category}", "{user}", "Likes"],
    ARCHIVE,
);
static BOARD: Route = Route::new(
    "board",
    FILENAME,
    &["{category}", "{user}", "{board}"],
    ARCHIVE,
);
static FEED: Route = Route::new("feed", FILENAME, &["{category}", "feed"], ARCHIVE);
static SEARCH: Route = Route::new(
    "search",
    FILENAME,
    &["{category}", "search", "{search[search]}"],
    ARCHIVE,
);

pub static SITE: Site = Site {
    category: "sexcom",
    name: "Sex.com",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&PIN, &RELATED_PIN, &PINS, &LIKES, &BOARD, &FEED, &SEARCH],
    api_key: false,
    cookies: Some("sex.com"),
};

#[derive(Debug, PartialEq)]
enum Target {
    /// Pin path.
    Pin(String),
    /// Pin path and ID.
    Related(String, String),
    /// Route, raw user and board.
    Listing(&'static str, String, Option<String>),
    Feed,
    /// Type, raw query and the path's search term.
    Search(String, Option<String>, Option<String>),
}

fn target(url: &Url) -> Option<Target> {
    if !on_host(url, &["sex.com"]) {
        return None;
    }
    let mut full = url.path().to_owned();
    if let Some(query) = url.query() {
        full.push('?');
        full.push_str(query);
    }
    if let Some(fragment) = url.fragment() {
        full.push('#');
        full.push_str(fragment);
    }
    // `(?:/[a-z]{2})?`: tried with the language prefix first, for each
    // extractor class in upstream order.
    let lang = full
        .get(1..3)
        .filter(|p| p.bytes().all(|b| b.is_ascii_lowercase()))
        .map(|_| &full[3..]);
    let classes: [fn(&str, &str) -> Option<Target>; 4] = [pin, user, feed, |rest, _| search(rest)];
    classes.into_iter().find_map(|class| {
        lang.into_iter()
            .chain([full.as_str()])
            .find_map(|rest| class(rest, &full))
    })
}

fn feed(rest: &str, _: &str) -> Option<Target> {
    rest.starts_with("/feed").then_some(Target::Feed)
}

/// Pins, and related pins of legacy `/pin/` URLs ending in `#related`.
fn pin(rest: &str, full: &str) -> Option<Target> {
    let related = full.ends_with("#related");
    // `(/(?:\w\w/(?:pic|gif|video)s|pin)/\d+/?)`
    let pin = rest.strip_prefix('/').and_then(|tail| {
        let after = tail.strip_prefix("pin/").or_else(|| {
            let (lang, kind) = tail.split_at_checked(3)?;
            let word = |c: char| crate::sites::media::word_char(c);
            (lang.chars().take(2).all(word) && lang.chars().count() == 3 && lang.ends_with('/'))
                .then_some(kind)?
                .strip_prefix("pics/")
                .or_else(|| kind.strip_prefix("gifs/"))
                .or_else(|| kind.strip_prefix("videos/"))
        })?;
        let id = crate::sites::art::leading_digits(after)?;
        let slash = if after[id.len()..].starts_with('/') {
            1
        } else {
            0
        };
        let len = rest.len() - after.len() + id.len() + slash;
        Some((
            rest[..len].to_owned(),
            id.to_owned(),
            tail.starts_with("pin/"),
        ))
    });
    match pin? {
        (path, _, _) if !related => Some(Target::Pin(path)),
        (path, id, true) => Some(Target::Related(path, id)),
        _ => None,
    }
}

/// A user's pins, likes or board.
fn user(rest: &str, _: &str) -> Option<Target> {
    let tail = rest.strip_prefix("/user/")?;
    let (user, after) = tail.split_once('/')?;
    if !segment(user) || user.contains('#') {
        return None;
    }
    if after.starts_with("pins/") {
        return Some(Target::Listing(PINS.subcategory, user.to_owned(), None));
    }
    if after.starts_with("likes/") {
        return Some(Target::Listing(LIKES.subcategory, user.to_owned(), None));
    }
    if ["following/", "pins/", "repins/", "likes/"]
        .iter()
        .any(|p| after.starts_with(p))
    {
        return None;
    }
    let board = &after[..after.find(['/', '?', '#']).unwrap_or(after.len())];
    (!board.is_empty())
        .then(|| Target::Listing(BOARD.subcategory, user.to_owned(), Some(board.to_owned())))
}

/// `/(?:(pic|gif|video)s(?:\?(search=[^#]+)$|/([^/?#]*))|search/(pic|gif|video)s)/?(?:\?([^#]+))?`
fn search(rest: &str) -> Option<Target> {
    let tail = rest.strip_prefix('/')?;
    let kind = |text: &str| {
        ["pic", "gif", "video"]
            .into_iter()
            .find(|k| text.starts_with(&format!("{k}s")))
    };
    let trailing = |text: &str| {
        let text = text.strip_prefix('/').unwrap_or(text);
        text.strip_prefix('?')
            .map(|q| &q[..q.find('#').unwrap_or(q.len())])
            .filter(|q| !q.is_empty())
            .map(str::to_owned)
    };
    if let Some(found) = kind(tail) {
        let after = &tail[found.len() + 1..];
        if let Some(query) = after.strip_prefix("?search=")
            && !query.is_empty()
            && !query.contains('#')
        {
            return Some(Target::Search(
                found.into(),
                Some(format!("search={query}")),
                None,
            ));
        }
        if let Some(path) = after.strip_prefix('/') {
            let end = path.find(['/', '?', '#']).unwrap_or(path.len());
            return Some(Target::Search(
                found.into(),
                trailing(&path[end..]),
                Some(path[..end].to_owned()),
            ));
        }
    }
    let after = tail.strip_prefix("search/")?;
    let found = kind(after)?;
    Some(Target::Search(
        found.into(),
        trailing(&after[found.len() + 1..]),
        None,
    ))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let gifs = http.option_bool("gifs", true);
    match target(&url) {
        Some(Target::Pin(path)) => crate::sites::once(async move {
            let item = parse_pin(http, format!("{ROOT}{path}"), gifs).await?;
            Ok(crate::sites::events(crate::sites::post(
                &SITE,
                http,
                &PIN,
                item.into_iter().collect(),
            )))
        }),
        Some(Target::Related(path, id)) => crate::sites::once(async move {
            let original = parse_pin(http, format!("{ROOT}{path}"), gifs)
                .await?
                .map_or(Value::Null, |item| Value::Object(item.keywords));
            let mut data = Map::new();
            data.insert("original_pin".into(), original);
            let url = format!("{ROOT}/pin/related?pinId={id}&limit=24&offset=0");
            Ok(pins(http, &RELATED_PIN, url, data, gifs))
        }),
        Some(Target::Listing(subcategory, user, board)) => {
            let mut data = Map::new();
            data.insert("user".into(), crate::sites::boorus::unquote(&user).into());
            let (route, url) = match (subcategory, &board) {
                ("pins", _) => (&PINS, format!("{ROOT}/user/{user}/pins/")),
                ("likes", _) => (&LIKES, format!("{ROOT}/user/{user}/likes/")),
                (_, board) => {
                    let board = board.clone().unwrap_or_default();
                    data.insert("board".into(), crate::sites::boorus::unquote(&board).into());
                    (&BOARD, format!("{ROOT}/user/{user}/{board}/"))
                }
            };
            pins(http, route, url, data, gifs)
        }
        Some(Target::Feed) => {
            let mut data = Map::new();
            data.insert("feed".into(), true.into());
            pins(http, &FEED, format!("{ROOT}/feed/"), data, gifs)
        }
        Some(Target::Search(kind, query, alt)) => search_results(http, kind, query, alt, gifs),
        None => crate::sites::fail(error("unsupported Sex.com URL")),
    }
}

struct Listing {
    urls: VecDeque<String>,
    next: Option<String>,
}

/// `_pagination` and `items`: pin links page by page, each pin's page
/// parsed when reached. Pins that fail to load are skipped like upstream.
fn pins<'a>(
    http: &'a Http,
    route: &'static Route,
    url: String,
    data: Map<String, Value>,
    gifs: bool,
) -> Events<'a> {
    let state = Listing {
        urls: VecDeque::new(),
        next: Some(url),
    };
    crate::sites::paginate(state, move |mut state| {
        let data = data.clone();
        async move {
            if let Some(url) = state.urls.pop_front() {
                let item = parse_pin(http, url, gifs).await.ok().flatten();
                let items = item
                    .map(|mut item| {
                        crate::sites::art::update(&mut item.keywords, &data);
                        item
                    })
                    .into_iter()
                    .collect();
                let events = crate::sites::post(&SITE, http, route, items);
                return Ok(Some((crate::sites::events(events), state)));
            }
            let Some(url) = state.next.take() else {
                return Ok(None);
            };
            let page = http.text(&http.api(&url)?).await?;
            let mut scan = Scan::new(&page);
            let next = unescape(scan.extr("<link rel=\"next\" href=\"", "\""));
            loop {
                let href = scan.extr("<a class=\"image_wrapper\" href=\"", "\"");
                if href.is_empty() {
                    break;
                }
                state.urls.push_back(format!("{ROOT}{href}"));
            }
            state.next = (!next.is_empty())
                .then(|| Url::parse(ROOT).ok()?.join(&next).ok())
                .flatten()
                .filter(|url| on_host(url, &["sex.com"]))
                .map(String::from);
            Ok(Some((crate::sites::events(Vec::new()), state)))
        }
    })
}

/// `_parse_pin`: the pin page after redirects, by layout, with the
/// date and tags `items()` adds. `None` for pins without media.
async fn parse_pin(http: &Http, mut url: String, gifs: bool) -> Result<Option<Item>> {
    if url.contains("/pin/") {
        if !url.ends_with('/') {
            url.push('/');
        }
    } else if url.ends_with('/') {
        url.pop();
    }
    let requested = http.api(&url)?;
    let (location, page) = http.page(&requested, &[]).await?;
    // `response.url` on the site's own origin.
    let final_url = format!(
        "{ROOT}{}{}",
        location.path(),
        location
            .query()
            .map(|q| format!("?{q}"))
            .unwrap_or_default()
    );
    if location != requested && final_url.len() <= 25 {
        // Redirected to the home page.
        return Ok(None);
    }
    let item = if final_url.contains("/pin/") {
        pin_legacy(&page)
    } else if final_url.contains("/videos/") {
        pin_video(http, &page)
    } else {
        pin_image(&page, gifs)
    };
    Ok(item.map(|mut item| {
        let fields = &mut item.keywords;
        let file = fields
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if let Some(day) = url_date(file) {
            fields.insert("date_url".into(), date(Some(day)));
            if !fields.contains_key("date") {
                fields.insert("date".into(), date(Some(day)));
                item.timestamp = Some(day);
            }
        }
        if let Some(Value::Array(tags)) = fields.get_mut("tags") {
            for tag in tags.iter_mut() {
                if let Some(stripped) = tag.as_str().and_then(|t| t.strip_prefix('@')) {
                    *tag = stripped.into();
                }
            }
        }
        let id = fields
            .get("pin_id")
            .map(crate::sites::scalar)
            .unwrap_or_default();
        let title = fields
            .get("title")
            .and_then(Value::as_str)
            .map(str::to_owned);
        item.ids(id.clone(), id)
            .caption(title)
            .header("Referer", &final_url)
    }))
}

/// The `YYYY/MM/DD` before a file URL's name, as midnight UTC.
fn url_date(url: &str) -> Option<i64> {
    let parts: Vec<&str> = url.rsplitn(5, '/').collect();
    let [_, day, month, year, _] = parts.as_slice() else {
        return None;
    };
    let number = |text: &str| text.trim().parse::<i64>().ok();
    let (year, month, day) = (number(year)?, number(month)?, number(day)?);
    crate::sites::art::strptime(&format!("{year:04}-{month:02}-{day:02}"), "%Y-%m-%d")
}

fn pin_legacy(page: &str) -> Option<Item> {
    let mut scan = Scan::new(page);
    let mut data = Map::new();
    data.insert(
        "thumbnail".into(),
        scan.extr("itemprop=\"thumbnail\" content=\"", "\"").into(),
    );
    let kind = scan
        .extr("<h1>", "<")
        .trim_end_matches([' ', '-'])
        .trim()
        .to_lowercase();
    data.insert("type".into(), kind.clone().into());
    data.insert(
        "title".into(),
        unescape(scan.extr("itemprop=\"name\">", "<")).into(),
    );
    let repins = extr(
        scan.extr("\"btn-group\"", "</div>"),
        "\"btn btn-primary\">",
        "<",
    );
    data.insert("repins".into(), parse_int(repins, 0).into());
    let likes = extr(
        scan.extr("\"btn-group\"", "</div>"),
        "\"btn btn-default\">",
        "<",
    );
    data.insert("likes".into(), parse_int(likes, 0).into());
    data.insert(
        "pin_id".into(),
        parse_int(scan.extr("data-id=\"", "\""), 0).into(),
    );
    // The file, and whether it is an embedded player for yt-dlp.
    let (file, embed) = if kind == "video" {
        let info = scan.extr("player.updateSrc(", ");");
        if !info.is_empty() {
            let path = match info.find("label: 'HD'") {
                Some(hd) => rextr(&info[..hd], "src: '", "'"),
                None => extr(info, "src: '", "'"),
            };
            crate::sites::art::nameext_into(path, &mut data);
            data.insert("url".into(), path.into());
            (path.to_owned(), false)
        } else {
            let iframe = scan.extr("<iframe", ">");
            let src = match extr(iframe, " src=\"", "\"") {
                "" => extr(iframe, " src='", "'"),
                src => src,
            };
            if src.is_empty() {
                return None;
            }
            data.insert("url".into(), format!("ytdl:{src}").into());
            (src.to_owned(), true)
        }
    } else {
        let raw = unescape(scan.extr(" src=\"", "\""));
        let url = partition(&raw, "?").0.to_owned();
        crate::sites::art::nameext_into(&url, &mut data);
        data.insert("url".into(), url.clone().into());
        (url, false)
    };
    data.insert(
        "uploader".into(),
        scan.extr("itemprop=\"author\">", "<").into(),
    );
    let posted = parse_iso(scan.extr("datetime=\"", "\""));
    data.insert("date".into(), date(posted));
    data.insert(
        "tags".into(),
        split_html(scan.extr("class=\"tags\"> Tags", "</div>")).into(),
    );
    data.insert(
        "comments".into(),
        parse_int(scan.extr("Comments (", ")"), 0).into(),
    );
    let user = data
        .get("uploader")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let item = if embed {
        crate::sites::social::ytdl_item(&file, data)?
    } else {
        Item::new(crate::sites::art::file_url(&file, ROOT)?, data)
    };
    Some(item.timestamp(posted).user(user))
}

fn pin_image(page: &str, gifs: bool) -> Option<Item> {
    let mut scan = Scan::new(page);
    let href = partition(scan.extr(" href=\"", "\""), "?").0.to_owned();
    let (title, _, kind) = partition_last(scan.extr("<title>", " | "), " ");
    let pin_id = partition_last(scan.extr("rel=\"canonical\" href=\"", "\""), "/").2;
    let tags = split_html(scan.extr("</h1>", "</section>"));
    let mut data = Map::new();
    data.insert("url".into(), href.clone().into());
    data.insert("title".into(), unescape(title).into());
    data.insert("pin_id".into(), parse_int(pin_id, 0).into());
    data.insert("tags".into(), tags.into());
    crate::sites::art::nameext_into(&href, &mut data);
    let mut url = href.clone();
    if kind.eq_ignore_ascii_case("pic") {
        data.insert("type".into(), "picture".into());
    } else {
        data.insert("type".into(), "gif".into());
        if gifs && str_of(&Value::Object(data.clone()), "extension") == "webp" {
            data.insert("extension".into(), "gif".into());
            url = format!("{}gif", &href[..href.len() - 4]);
            data.insert("url".into(), url.clone().into());
        }
    }
    Some(Item::new(crate::sites::art::file_url(&url, ROOT)?, data))
}

fn pin_video(http: &Http, page: &str) -> Option<Item> {
    let mut scan = Scan::new(page);
    let title = unescape(scan.extr("<title>", " | Sex.com<"));
    let pin_id = partition_last(scan.extr("rel=\"canonical\" href=\"", "\""), "/").2;
    let tags = split_html(
        partition(
            scan.extr("event_name=\"video_tags_click\"", "<div data-testid="),
            ">",
        )
        .2,
    );
    let source = scan.extr("<source src=\"", "\"");
    let mut data = Map::new();
    data.insert("extension".into(), "mp4".into());
    data.insert("type".into(), "video".into());
    data.insert("title".into(), title.into());
    data.insert("pin_id".into(), parse_int(pin_id, 0).into());
    data.insert("tags".into(), tags.into());
    data.insert("url".into(), format!("ytdl:{source}").into());
    let item = manifest_item(source, ManifestProtocol::Hls, data)?;
    Some(crate::sites::art::session_cookies(http, item))
}

/// `SexcomSearchExtractor`: the portal search API, 40 results a page.
fn search_results(
    http: &Http,
    kind: String,
    query: Option<String>,
    alt: Option<String>,
    gifs: bool,
) -> Events<'_> {
    let mut params: Vec<(String, String)> = query.as_deref().map(parse_query).unwrap_or_default();
    if let Some(index) = params.iter().position(|(k, _)| k == "query") {
        let (_, value) = params.remove(index);
        set(&mut params, "search", value);
    }
    for (key, value) in [
        ("sexual-orientation", "straight"),
        ("order", "likeCount"),
        ("search", alt.as_deref().unwrap_or_default()),
    ] {
        if !params.iter().any(|(k, _)| k == key) {
            params.push((key.into(), value.into()));
        }
    }
    let mut search: Map<String, Value> = params
        .iter()
        .map(|(k, v)| (k.clone(), Value::from(v.clone())))
        .collect();
    search.insert("type".into(), kind.clone().into());
    let first = params
        .iter()
        .find(|(k, _)| k == "page")
        .map_or(1, |(_, v)| parse_int(v, 1));
    let endpoint = format!(
        "{ROOT}/portal/api/{}s/search",
        if kind == "pic" { "picture" } else { &kind }
    );
    crate::sites::paginate(Some(first), move |page| {
        let mut params = params.clone();
        let search = search.clone();
        let endpoint = endpoint.clone();
        let kind = kind.clone();
        async move {
            let Some(page) = page else {
                return Ok(None);
            };
            set(&mut params, "page", page.to_string());
            set(&mut params, "limit", "40".into());
            let mut url = http.api(&endpoint)?;
            url.query_pairs_mut().extend_pairs(&params);
            let data = http.json(&url).await?;
            let items: Vec<Item> = data
                .get("data")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|pin| search_item(pin, &kind, &search, gifs))
                .collect();
            let mut events = Vec::new();
            for item in items {
                events.extend(crate::sites::post(&SITE, http, &SEARCH, vec![item]));
            }
            let pages = data
                .pointer("/paging/numberOfPages")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let next = (page < pages && page < first + MAX_PAGES).then_some(page + 1);
            Ok(Some((crate::sites::events(events), next)))
        }
    })
}

/// Python `dict[key] = value`: replace in place, else append.
fn set(params: &mut Vec<(String, String)>, key: &str, value: String) {
    match params.iter_mut().find(|(k, _)| k == key) {
        Some((_, old)) => *old = value,
        None => params.push((key.into(), value)),
    }
}

fn search_item(pin: &Value, kind: &str, search: &Map<String, Value>, gifs: bool) -> Option<Item> {
    let Value::Object(mut pin) = pin.clone() else {
        return None;
    };
    let mut path = pin.get("uri").and_then(Value::as_str)?.to_owned();
    let id = pin.remove("id").unwrap_or(Value::Null);
    pin.insert("pin_id".into(), id.clone());
    crate::sites::art::nameext_into(&path, &mut pin);
    let day = url_date(&path);
    if let Some(day) = day {
        pin.insert("date_url".into(), date(Some(day)));
        pin.insert("date".into(), date(Some(day)));
    }
    if kind == "pic" {
        pin.insert("type".into(), "picture".into());
    } else {
        pin.insert("type".into(), "gif".into());
        if gifs && pin.get("extension").and_then(Value::as_str) == Some("webp") {
            pin.insert("extension".into(), "gif".into());
            path = format!("{}gif", &path[..path.len() - 4]);
        }
    }
    let url = format!("{IMAGES}{path}");
    pin.insert("url".into(), url.clone().into());
    pin.insert("search".into(), Value::Object(search.clone()));
    let id = crate::sites::scalar(&id);
    let title = pin.get("title").and_then(Value::as_str).map(str::to_owned);
    Some(
        Item::new(crate::sites::media::web_url(&url)?, pin)
            .ids(id.clone(), id)
            .caption(title)
            .timestamp(day),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::MediaJob;
    use crate::sites::manga::tests::{archive, path};
    use crate::sites::testing::{html, http, json, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.sex.com/pin/12345-title/"),
            Some(Target::Pin("/pin/12345".into()))
        );
        assert_eq!(
            t("https://www.sex.com/en/pics/12345"),
            Some(Target::Pin("/en/pics/12345".into()))
        );
        assert_eq!(
            t("https://www.sex.com/pin/12345#related"),
            Some(Target::Related("/pin/12345".into(), "12345".into()))
        );
        assert_eq!(
            t("https://www.sex.com/user/some%20one/pins/"),
            Some(Target::Listing("pins", "some%20one".into(), None))
        );
        assert_eq!(
            t("https://sex.com/en/user/someone/likes/"),
            Some(Target::Listing("likes", "someone".into(), None))
        );
        assert_eq!(
            t("https://www.sex.com/user/someone/my-board/"),
            Some(Target::Listing(
                "board",
                "someone".into(),
                Some("my-board".into())
            ))
        );
        assert_eq!(t("https://www.sex.com/feed/"), Some(Target::Feed));
        assert_eq!(
            t("https://www.sex.com/search/pics?query=sea&page=2"),
            Some(Target::Search(
                "pic".into(),
                Some("query=sea&page=2".into()),
                None
            ))
        );
        assert_eq!(
            t("https://www.sex.com/gifs?search=wave"),
            Some(Target::Search(
                "gif".into(),
                Some("search=wave".into()),
                None
            ))
        );
        assert_eq!(
            t("https://www.sex.com/videos/beach/?order=new"),
            Some(Target::Search(
                "video".into(),
                Some("order=new".into()),
                Some("beach".into())
            ))
        );
        for raw in [
            "https://www.sex.com/user/someone/following/",
            "https://www.sex.com/",
            "https://www.sex.com.evil.test/pin/1/",
            "https://www.sex.com:8443/pin/1/",
            "https://u:p@www.sex.com/pin/1/",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[tokio::test]
    async fn extracts_gif_pin_as_gif() {
        let page = r#"<a href="https://imagex1.sx.cdn.live/images/pinporn/2024/01/02/12345.webp?width=300">
<title>Ocean Waves GIF | Sex.com</title><link rel="canonical" href="https://www.sex.com/en/gifs/12345">
<h1>x</h1><a>@sea</a><a>waves</a></section>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = http("sexcom", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.sex.com/en/gifs/12345/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].url.as_str(),
            "https://imagex1.sx.cdn.live/images/pinporn/2024/01/02/12345.gif"
        );
        assert_eq!(
            files[0].headers["Referer"],
            "https://www.sex.com/en/gifs/12345"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["title"], "Ocean Waves");
        assert_eq!(k["type"], "gif");
        assert_eq!(k["pin_id"], 12345);
        assert_eq!(k["tags"], json!(["sea", "waves"]));
        assert_eq!(k["date"], "2024-01-02 00:00:00");
        assert_eq!(path(&files[0]), "sexcom/12345 Ocean Waves.gif");
        assert_eq!(archive(&files[0]), r#"["{pin_id}",["12345"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /en/gifs/12345 "));
    }

    #[tokio::test]
    async fn video_pins_hand_their_stream_to_ytdl() {
        let page = r#"<title>Sunset Clip | Sex.com</title><link rel="canonical" href="https://www.sex.com/en/videos/777">
<div event_name="video_tags_click" class="t"><a>sunset</a></div><div data-testid="x">
<video><source src="https://video.sx.test/777/master.m3u8"></video>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = http("sexcom", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.sex.com/en/videos/777").unwrap(),
        ))
        .await;
        assert_eq!(
            files[0].url.as_str(),
            "ytdl:https://video.sx.test/777/master.m3u8"
        );
        assert!(matches!(files[0].job, MediaJob::Manifest { .. }));
        let k = keywords(&files[0]);
        assert_eq!(k["tags"], json!(["sunset"]));
        assert_eq!(k["extension"], "mp4");
        assert_eq!(path(&files[0]), "sexcom/777 Sunset Clip.mp4");
        server.join().unwrap();
    }

    #[tokio::test]
    async fn board_lists_pins_lazily() {
        let board = r#"<link rel="next" href="/user/someone/my-board/?page=2&amp;x=1">
<a class="image_wrapper" href="/pin/1-a/"></a>"#;
        let legacy = r#"<meta itemprop="thumbnail" content="https://t.test/1.jpg"><h1>Picture - </h1>
<span itemprop="name">Pier</span><div class="btn-group"><a class="btn btn-primary">3</a></div>
<div class="btn-group"><a class="btn btn-default">4</a></div><div data-id="1"></div>
<img src="https://cdn.sex.test/images/pinporn/2019/05/06/1.jpg?x=1">
<a itemprop="author">maker</a><time datetime="2019-05-06T07:08:09"></time>
<div class="tags"> Tags <a>pier</a></div>Comments (2)"#;
        let (root, server) = serve(vec![html(board), html(legacy), html("<p>end</p>")]);
        let http = http("sexcom", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.sex.com/user/someone/my-board/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let k = keywords(&files[0]);
        assert_eq!(k["user"], "someone");
        assert_eq!(k["board"], "my-board");
        assert_eq!(k["type"], "picture");
        assert_eq!(k["repins"], 3);
        assert_eq!(k["likes"], 4);
        assert_eq!(k["uploader"], "maker");
        assert_eq!(k["date"], "2019-05-06 07:08:09");
        assert_eq!(k["date_url"], "2019-05-06 00:00:00");
        assert_eq!(k["comments"], 2);
        assert_eq!(path(&files[0]), "sexcom/someone/my-board/1 Pier.jpg");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /user/someone/my-board/ "));
        assert!(requests[1].starts_with("GET /pin/1-a/ "));
        assert!(requests[2].starts_with("GET /user/someone/my-board/?page=2&x=1 "));
    }

    #[tokio::test]
    async fn searches_the_portal_api() {
        let reply = |pages: i64| {
            json(json!({"data": [{"id": 9, "title": "Wave",
                "uri": "/images/pinporn/2023/03/04/9.webp"}],
                "paging": {"numberOfPages": pages}}))
        };
        let (root, server) = serve(vec![reply(2), reply(2)]);
        let http = http("sexcom", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.sex.com/search/gifs?query=wave").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].url.as_str(),
            "https://imagex1.sx.cdn.live/images/pinporn/2023/03/04/9.gif"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["pin_id"], 9);
        assert_eq!(
            k["search"],
            json!({"search": "wave", "sexual-orientation": "straight",
                "order": "likeCount", "type": "gif"})
        );
        assert_eq!(path(&files[0]), "sexcom/search/wave/9 Wave.gif");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with(
            "GET /portal/api/gifs/search?search=wave&sexual-orientation=straight\
&order=likeCount&page=1&limit=40 "
        ));
        assert!(requests[1].contains("&page=2&limit=40 "));
    }
}
