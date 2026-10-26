//! Sex.com pins, related pins, user pins/likes/boards, feeds and searches,
//! mirroring gallery_dl/extractor/sexcom.py.
use crate::error;
use crate::sites::gdl::{
    Scan, captures, extr, group, hls_item, int_of, nameext_into, parse_int, parse_iso, parse_query,
    re, rextr, rpartition, split_html, truthy, unescape, unquote, ytdl_item,
};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value, json};
use url::Url;

const ROOT: &str = "https://www.sex.com";
const BASE: &str = r"^https?://(?:www\.)?sex\.com(?:/[a-z]{2})?";
const FILENAME: &str = "{pin_id}{title:? //}.{extension}";

static PIN: Route = Route::new("pin", FILENAME, &["{category}"], "{pin_id}");
static RELATED: Route = Route::new(
    "related-pin",
    FILENAME,
    &["{category}", "related {original_pin[pin_id]}"],
    "{pin_id}",
);
static PINS: Route = Route::new("pins", FILENAME, &["{category}", "{user}"], "{pin_id}");
static LIKES: Route = Route::new(
    "likes",
    FILENAME,
    &["{category}", "{user}", "Likes"],
    "{pin_id}",
);
static BOARD: Route = Route::new(
    "board",
    FILENAME,
    &["{category}", "{user}", "{board}"],
    "{pin_id}",
);
static FEED: Route = Route::new("feed", FILENAME, &["{category}", "feed"], "{pin_id}");
static SEARCH: Route = Route::new(
    "search",
    FILENAME,
    &["{category}", "search", "{search[search]}"],
    "{pin_id}",
);

pub static SITE: Site = Site {
    category: "sexcom",
    name: "Sex.com",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&PIN, &RELATED, &PINS, &LIKES, &BOARD, &FEED, &SEARCH],
    api_key: false,
    cookies: Some("sex.com"),
};

#[derive(Debug, PartialEq)]
enum Target {
    Pin(String),
    Related(String, String),
    Pins(String),
    Likes(String),
    Board(String, String),
    Feed,
    /// Type and query string.
    Search(String, String, String),
}

fn target(url: &Url) -> Option<Target> {
    let related = url.fragment() == Some("related");
    if !related
        && let Some(c) = captures(
            re!(&format!(r"{BASE}(/(?:\w\w/(?:pic|gif|video)s|pin)/\d+/?)")),
            url,
        )
    {
        return Some(Target::Pin(group(&c, 1).into()));
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}(/pin/(\d+)/?).*#related$")), url) {
        return Some(Target::Related(group(&c, 1).into(), group(&c, 2).into()));
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}/user/([^/?#]+)/pins/")), url) {
        return Some(Target::Pins(group(&c, 1).into()));
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}/user/([^/?#]+)/likes/")), url) {
        return Some(Target::Likes(group(&c, 1).into()));
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}/user/([^/?#]+)/([^/?#]+)")), url) {
        // Upstream's `(?!(?:following|pins|repins|likes)/)`.
        let board = group(&c, 2);
        let reserved = ["following", "pins", "repins", "likes"].contains(&board)
            && url.path().contains(&format!("/{board}/"));
        if !reserved {
            return Some(Target::Board(group(&c, 1).into(), board.into()));
        }
    }
    if captures(re!(&format!(r"{BASE}/feed")), url).is_some() {
        return Some(Target::Feed);
    }
    let c = captures(
        re!(&format!(
            r"{BASE}/(?:(pic|gif|video)s(?:\?(search=[^#]+)$|/([^/?#]*))|search/(pic|gif|video)s)/?(?:\?([^#]+))?"
        )),
        url,
    )?;
    let kind = c.get(1).or(c.get(4)).map_or("", |m| m.as_str()).to_owned();
    let query = c.get(2).or(c.get(5)).map_or("", |m| m.as_str()).to_owned();
    Some(Target::Search(kind, query, group(&c, 3).into()))
}

/// `date_url` (and `date`) from `/YYYY/MM/DD/name` file paths.
fn url_date(url: &str, data: &mut Map<String, Value>) {
    let parts: Vec<&str> = url.rsplitn(5, '/').collect();
    if parts.len() == 5
        && let (Ok(y), Ok(m), Ok(d)) = (
            parts[3].parse::<i32>(),
            parts[2].parse::<u8>(),
            parts[1].parse::<u8>(),
        )
        && let Ok(month) = time::Month::try_from(m)
        && let Ok(day) = time::Date::from_calendar_date(y, month, d)
    {
        let value = crate::sites::gdl::date(Some(day.midnight().assume_utc().unix_timestamp()));
        data.insert("date_url".into(), value.clone());
        data.entry("date").or_insert(value);
    }
}

/// `_parse_pin`: one pin page, by its (redirected) kind.
async fn parse_pin(http: &Http, url: &str) -> Option<Map<String, Value>> {
    let url = if url.contains("/pin/") {
        if url.ends_with('/') {
            url.to_owned()
        } else {
            format!("{url}/")
        }
    } else {
        url.trim_end_matches('/').to_owned()
    };
    let request = http.api(&url).ok()?;
    let target = match http.location(&request, &[]).await.ok()? {
        Some(location) => {
            if location.as_str().len() <= 25 {
                return None;
            }
            location
        }
        None => request,
    };
    let (page, final_url) = http.text_final(&target, &[]).await.ok()?;
    let referer = final_url.to_string();
    let mut data = if final_url.path().contains("/pin/") {
        legacy_pin(&page)?
    } else if final_url.path().contains("/videos/") {
        video_pin(&page)
    } else {
        image_pin(http, &page)
    };
    data.insert("_referer".into(), referer.into());
    let file = crate::sites::scalar(&data["url"]);
    url_date(&file, &mut data);
    let tags: Vec<Value> = data
        .get("tags")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(crate::sites::scalar)
        .map(|t| t.strip_prefix('@').map(str::to_owned).unwrap_or(t).into())
        .collect();
    data.insert("tags".into(), tags.into());
    Some(data)
}

fn legacy_pin(page: &str) -> Option<Map<String, Value>> {
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
    data.insert("type".into(), kind.as_str().into());
    data.insert(
        "title".into(),
        unescape(scan.extr("itemprop=\"name\">", "<")).into(),
    );
    data.insert(
        "repins".into(),
        parse_int(extr(
            scan.extr("\"btn-group\"", "</div>"),
            "\"btn btn-primary\">",
            "<",
        ))
        .into(),
    );
    data.insert(
        "likes".into(),
        parse_int(extr(
            scan.extr("\"btn-group\"", "</div>"),
            "\"btn btn-default\">",
            "<",
        ))
        .into(),
    );
    data.insert(
        "pin_id".into(),
        parse_int(scan.extr("data-id=\"", "\"")).into(),
    );
    if kind == "video" {
        let info = scan.extr("player.updateSrc(", ");");
        if !info.is_empty() {
            let path = match info.find("label: 'HD'") {
                Some(pos) => rextr(info, "src: '", "'", Some(pos)),
                None => extr(info, "src: '", "'"),
            };
            nameext_into(path, &mut data);
            data.insert("url".into(), path.into());
        } else {
            let iframe = scan.extr("<iframe", ">");
            let src = match extr(iframe, " src=\"", "\"") {
                "" => extr(iframe, " src='", "'"),
                src => src,
            };
            if src.is_empty() {
                return None;
            }
            data.insert("extension".into(), Value::Null);
            data.insert("url".into(), format!("ytdl:{src}").into());
        }
    } else {
        let url = unescape(scan.extr(" src=\"", "\""));
        let clean = url.split('?').next().unwrap_or_default().to_owned();
        data.insert("url".into(), clean.as_str().into());
        data.insert("_fallback".into(), vec![url].into());
        nameext_into(&clean, &mut data);
    }
    data.insert(
        "uploader".into(),
        scan.extr("itemprop=\"author\">", "<").into(),
    );
    data.insert(
        "date".into(),
        crate::sites::gdl::date(parse_iso(scan.extr("datetime=\"", "\""))),
    );
    data.insert(
        "tags".into(),
        split_html(scan.extr("class=\"tags\"> Tags", "</div>")).into(),
    );
    data.insert(
        "comments".into(),
        parse_int(scan.extr("Comments (", ")")).into(),
    );
    Some(data)
}

fn image_pin(http: &Http, page: &str) -> Map<String, Value> {
    let mut scan = Scan::new(page);
    let href = scan
        .extr(" href=\"", "\"")
        .split('?')
        .next()
        .unwrap_or_default()
        .to_owned();
    let full_title = scan.extr("<title>", " | ").to_owned();
    let (title, _, kind) = rpartition(&full_title, " ");
    let mut data = Map::new();
    data.insert("url".into(), href.as_str().into());
    data.insert("title".into(), unescape(title).into());
    data.insert(
        "pin_id".into(),
        parse_int(rpartition(scan.extr("rel=\"canonical\" href=\"", "\""), "/").2).into(),
    );
    data.insert(
        "tags".into(),
        split_html(scan.extr("</h1>", "</section>")).into(),
    );
    nameext_into(&href, &mut data);
    if kind.eq_ignore_ascii_case("pic") {
        data.insert("type".into(), "picture".into());
    } else {
        data.insert("type".into(), "gif".into());
        if http.option("gifs").is_none_or(|v| truthy(Some(v))) && data["extension"] == "webp" {
            data.insert("extension".into(), "gif".into());
            data.insert("_fallback".into(), vec![href.clone()].into());
            data.insert(
                "url".into(),
                format!("{}gif", &href[..href.len().saturating_sub(4)]).into(),
            );
        }
    }
    data
}

fn video_pin(page: &str) -> Map<String, Value> {
    let mut scan = Scan::new(page);
    let mut data = Map::new();
    data.insert("extension".into(), "mp4".into());
    data.insert("type".into(), "video".into());
    data.insert(
        "title".into(),
        unescape(scan.extr("<title>", " | Sex.com<")).into(),
    );
    data.insert(
        "pin_id".into(),
        parse_int(rpartition(scan.extr("rel=\"canonical\" href=\"", "\""), "/").2).into(),
    );
    let tags = scan.extr("event_name=\"video_tags_click\"", "<div data-testid=");
    data.insert(
        "tags".into(),
        split_html(tags.split_once('>').map_or("", |(_, rest)| rest)).into(),
    );
    data.insert(
        "url".into(),
        format!("ytdl:{}", scan.extr("<source src=\"", "\"")).into(),
    );
    data.insert("_hls".into(), true.into());
    data
}

/// A pin's item: direct files, yt-dlp pages, or HLS streams.
fn pin_item(mut data: Map<String, Value>, extra: &Map<String, Value>) -> Option<Item> {
    data.extend(extra.clone());
    let url = crate::sites::scalar(&data["url"]);
    let referer = data
        .remove("_referer")
        .map(|r| crate::sites::scalar(&r))
        .unwrap_or_default();
    let hls = data.remove("_hls").is_some();
    let id = crate::sites::scalar(&data["pin_id"]);
    let item = match url.strip_prefix("ytdl:") {
        Some(stream) if hls => hls_item(stream, data)?,
        Some(page) => ytdl_item(page, data)?,
        None => Item::new(Url::parse(&url).ok()?, data),
    };
    Some(item.ids(id.clone(), id).header("Referer", &referer))
}

fn pins_events<'a>(
    http: &'a Http,
    route: &'static Route,
    urls: Vec<String>,
    extra: Map<String, Value>,
) -> Events<'a> {
    crate::sites::each(urls, move |url| {
        let extra = extra.clone();
        async move {
            let items = parse_pin(http, &url)
                .await
                .and_then(|pin| pin_item(pin, &extra))
                .into_iter()
                .collect();
            Ok(crate::sites::post(&SITE, http, route, items))
        }
    })
}

/// `_pagination`: pin links, following `rel="next"`.
fn pagination<'a>(
    http: &'a Http,
    route: &'static Route,
    first: String,
    extra: Map<String, Value>,
) -> Events<'a> {
    crate::sites::paginate(Some(first), move |state| {
        let extra = extra.clone();
        async move {
            let Some(url) = state else { return Ok(None) };
            let page = http.text(&http.api(&url)?).await?;
            let mut scan = Scan::new(&page);
            let next = scan.extr("<link rel=\"next\" href=\"", "\"").to_owned();
            let mut pins = Vec::new();
            loop {
                let href = scan.extr("<a class=\"image_wrapper\" href=\"", "\"");
                if href.is_empty() {
                    break;
                }
                pins.push(format!("{ROOT}{href}"));
            }
            let next = (!next.is_empty())
                .then(|| {
                    Url::parse(ROOT)
                        .and_then(|root| root.join(&unescape(&next)))
                        .map(|u| u.to_string())
                        .ok()
                })
                .flatten();
            Ok(Some((pins_events(http, route, pins, extra), next)))
        }
    })
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Sex.com URL"));
    };
    let user = |name: &str| -> Map<String, Value> {
        let mut extra = Map::new();
        extra.insert("user".into(), unquote(name).into());
        extra
    };
    match target {
        Target::Pin(path) => pins_events(http, &PIN, vec![format!("{ROOT}{path}")], Map::new()),
        Target::Related(path, id) => crate::sites::once(async move {
            let original = parse_pin(http, &format!("{ROOT}{path}"))
                .await
                .map_or(Value::Null, Value::Object);
            let mut extra = Map::new();
            extra.insert("original_pin".into(), original);
            Ok(pagination(
                http,
                &RELATED,
                format!("{ROOT}/pin/related?pinId={id}&limit=24&offset=0"),
                extra,
            ))
        }),
        Target::Pins(name) => pagination(
            http,
            &PINS,
            format!("{ROOT}/user/{name}/pins/"),
            user(&name),
        ),
        Target::Likes(name) => pagination(
            http,
            &LIKES,
            format!("{ROOT}/user/{name}/likes/"),
            user(&name),
        ),
        Target::Board(name, board) => {
            let mut extra = user(&name);
            extra.insert("board".into(), unquote(&board).into());
            pagination(http, &BOARD, format!("{ROOT}/user/{name}/{board}/"), extra)
        }
        Target::Feed => {
            let mut extra = Map::new();
            extra.insert("feed".into(), true.into());
            pagination(http, &FEED, format!("{ROOT}/feed/"), extra)
        }
        Target::Search(kind, query, alt) => {
            let mut params = parse_query(&query);
            if let Some(q) = params.remove("query") {
                params.insert("search".into(), q);
            }
            params
                .entry("sexual-orientation")
                .or_insert("straight".into());
            params.entry("order").or_insert("likeCount".into());
            params.entry("search").or_insert(alt.into());
            let mut search = params.clone();
            search.insert("type".into(), kind.as_str().into());
            let first = int_of(params.get("page")).max(1);
            params.remove("page");
            let gifs = http.option("gifs").is_none_or(|v| truthy(Some(v)));
            let endpoint = format!(
                "{ROOT}/portal/api/{}s/search",
                if kind == "pic" { "picture" } else { &kind }
            );
            crate::sites::paginate(Some(first), move |state| {
                let params = params.clone();
                let search = search.clone();
                let endpoint = endpoint.clone();
                let kind = kind.clone();
                async move {
                    let Some(page) = state else { return Ok(None) };
                    let mut url = http.api(&endpoint)?;
                    {
                        let mut pairs = url.query_pairs_mut();
                        for (key, value) in &params {
                            pairs.append_pair(key, &crate::sites::scalar(value));
                        }
                        pairs
                            .append_pair("page", &page.to_string())
                            .append_pair("limit", "40");
                    }
                    let data = http.json(&url).await?;
                    let cdn = "https://imagex1.sx.cdn.live";
                    let items: Vec<Item> = data["data"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|pin| {
                            let mut pin = pin.as_object()?.clone();
                            let mut path = pin.get("uri")?.as_str()?.to_owned();
                            let id = pin.remove("id").unwrap_or(Value::Null);
                            pin.insert("pin_id".into(), id.clone());
                            nameext_into(&path, &mut pin);
                            pin.remove("date");
                            url_date(&path, &mut pin);
                            if let Some(date) = pin.get("date_url").cloned() {
                                pin.insert("date".into(), date);
                            }
                            if kind == "pic" {
                                pin.insert("type".into(), "picture".into());
                            } else {
                                pin.insert("type".into(), "gif".into());
                                if gifs && pin["extension"] == "webp" {
                                    pin.insert("extension".into(), "gif".into());
                                    pin.insert("_fallback".into(), json!([format!("{cdn}{path}")]));
                                    path = format!("{}gif", &path[..path.len() - 4]);
                                }
                            }
                            let url = format!("{cdn}{path}");
                            pin.insert("url".into(), url.as_str().into());
                            pin.insert("search".into(), Value::Object(search.clone()));
                            let id = crate::sites::scalar(&id);
                            Some(Item::new(Url::parse(&url).ok()?, pin).ids(id.clone(), id))
                        })
                        .collect();
                    let next =
                        (page < int_of(data["paging"].get("numberOfPages"))).then_some(page + 1);
                    let events = items
                        .into_iter()
                        .flat_map(|item| crate::sites::post(&SITE, http, &SEARCH, vec![item]))
                        .collect();
                    Ok(Some((crate::sites::events(events), next)))
                }
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http as client, json as body, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.sex.com/pin/21241874/"),
            Some(Target::Pin("/pin/21241874/".into()))
        );
        assert_eq!(
            t("https://www.sex.com/en/pics/12345"),
            Some(Target::Pin("/en/pics/12345".into()))
        );
        assert_eq!(
            t("https://www.sex.com/pin/21241874/#related"),
            Some(Target::Related("/pin/21241874/".into(), "21241874".into()))
        );
        assert_eq!(
            t("https://www.sex.com/user/sirjuan79/pins/"),
            Some(Target::Pins("sirjuan79".into()))
        );
        assert_eq!(
            t("https://www.sex.com/user/ronin17/exciting-hentai/"),
            Some(Target::Board("ronin17".into(), "exciting-hentai".into()))
        );
        assert_eq!(
            t("https://www.sex.com/search/pics?query=ecchi"),
            Some(Target::Search(
                "pic".into(),
                "query=ecchi".into(),
                String::new()
            ))
        );
        assert_eq!(
            t("https://www.sex.com/gifs/cat"),
            Some(Target::Search("gif".into(), String::new(), "cat".into()))
        );
    }

    #[tokio::test]
    async fn search_converts_webp_gifs() {
        let data = json!({"data": [{"id": 9, "uri": "/2023/05/06/clip.webp", "title": "Clip"}], "paging": {"numberOfPages": 1}});
        let (root, server) = serve(vec![body(data)]);
        let http = client("sexcom", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.sex.com/search/gifs?query=cat").unwrap(),
        ))
        .await;
        assert_eq!(
            files[0].url.as_str(),
            "https://imagex1.sx.cdn.live/2023/05/06/clip.gif"
        );
        assert_eq!(
            files[0].handoff["_fallback"],
            json!(["https://imagex1.sx.cdn.live/2023/05/06/clip.webp"])
        );
        let k = keywords(&files[0]);
        assert_eq!(
            (
                k["date"].as_str(),
                k["search"]["search"].as_str(),
                k["type"].as_str()
            ),
            (Some("2023-05-06 00:00:00"), Some("cat"), Some("gif"))
        );
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "9 Clip.gif");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("sexcom/search/cat")
        );
        server.join().unwrap();
    }
}
