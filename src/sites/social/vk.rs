//! VK, mirroring gallery_dl/extractor/vk.py.
//!
//! Profile, album and wall pages are scraped for metadata; photo lists come
//! from the `al_photos.php` form endpoint, ten photos per page. VK may answer
//! in windows-1251, so bodies that are not valid UTF-8 are decoded as such.
//! The `offset` option sets the starting position in a photo list.
//!
//! Upstream keeps the original `/impf/` or `/impg/` URL as a download
//! fallback; this layer has no fallback URLs, so only the full-size URL is
//! emitted, as upstream tries first.
use crate::sites::art::{Scan, date, extr, nameext_into, parse_int};
use crate::sites::http::Body;
use crate::sites::{Events, Http, Item, Route, Site, plain, scalar, unescape};
use crate::{Result, error};
use serde_json::{Map, Value, json};
use url::Url;

pub static SITE: Site = Site {
    category: "vk",
    name: "VK",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&PHOTOS, &ALBUM, &TAGGED, &WALL_POST],
    api_key: false,
    cookies: Some("vk.com"),
};

const FILENAME: &str = "{id}.{extension}";
const ARCHIVE: &str = "{user[id]}_{id}";
static PHOTOS: Route = Route::new(
    "photos",
    FILENAME,
    &["{category}", "{user[name]|user[id]}"],
    ARCHIVE,
);
static ALBUM: Route = Route::new(
    "album",
    FILENAME,
    &["{category}", "{user[id]}", "{album[id]}"],
    ARCHIVE,
);
static TAGGED: Route = Route::new(
    "tagged",
    FILENAME,
    &["{category}", "{user[id]}", "tags"],
    ARCHIVE,
);
static WALL_POST: Route = Route::new(
    "wall-post",
    "{wall[id]}_{num}.{extension}",
    &["{category}", "{user[id]}", "wall"],
    ARCHIVE,
);

const ROOT: &str = "https://vk.com";
const REFERER: &str = "https://vk.com/";
/// Photo size keys, largest first.
const SIZES: &str = "wzyxrqpo";

#[derive(Debug, PartialEq)]
enum Target {
    /// `/id123`, `/photos-123`, `/albums123` or a screen name.
    Photos {
        id: Option<String>,
        name: Option<String>,
    },
    Album(String, String),
    Tagged(String),
    WallPost(String, String),
}

/// `-?\d+` at the start of `text`: the match and the remainder.
fn signed(text: &str) -> Option<(&str, &str)> {
    let digits = text.strip_prefix('-').unwrap_or(text);
    let len = digits.bytes().take_while(u8::is_ascii_digit).count();
    if len == 0 {
        return None;
    }
    let end = text.len() - digits.len() + len;
    Some((&text[..end], &text[end..]))
}

fn digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

fn target(url: &Url) -> Option<Target> {
    if url.scheme() != "https"
        || !plain(url)
        || !matches!(url.host_str()?, "vk.com" | "www.vk.com" | "m.vk.com")
    {
        return None;
    }
    let rest = url.path().strip_prefix('/')?;
    // Patterns anchored with `$` accept no query or fragment.
    let exact = url.query().is_none() && url.fragment().is_none();

    // VkPhotosExtractor
    for prefix in ["albums", "photos", "id"] {
        if let Some((id, _)) = rest.strip_prefix(prefix).and_then(signed) {
            return Some(Target::Photos {
                id: Some(id.into()),
                name: None,
            });
        }
    }
    let excluded = ["album", "tag", "wall"]
        .iter()
        .any(|p| rest.strip_prefix(p).and_then(signed).is_some());
    // Video and clip pages are left to the yt-dlp site.
    let video = ["video", "clip"]
        .iter()
        .any(|p| rest.strip_prefix(p).and_then(signed).is_some());
    if !excluded && !video {
        let name = rest.split('/').next().unwrap_or_default();
        return (!name.is_empty()).then(|| Target::Photos {
            id: None,
            name: Some(name.into()),
        });
    }

    // VkAlbumExtractor
    if let Some((user, tail)) = rest.strip_prefix("album").and_then(signed) {
        let album = tail.strip_prefix('_')?;
        return (exact && digits(album)).then(|| Target::Album(user.into(), album.into()));
    }
    // VkTaggedExtractor
    if let Some((user, tail)) = rest.strip_prefix("tag").and_then(signed) {
        return (exact && tail.is_empty()).then(|| Target::Tagged(user.into()));
    }
    // VkWallPostExtractor
    let (user, tail) = rest.strip_prefix("wall").and_then(signed)?;
    let tail = tail.strip_prefix('_')?;
    let len = tail.bytes().take_while(u8::is_ascii_digit).count();
    (len > 0).then(|| Target::WallPost(user.into(), tail[..len].into()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported VK URL"));
    };
    crate::sites::once(async move {
        let (route, data, list) = match target {
            Target::Photos { id, name } => {
                let (data, id) = profile(http, id, name).await?;
                (&PHOTOS, data, format!("photos{id}"))
            }
            Target::Album(user, album) => (
                &ALBUM,
                album_metadata(http, &user, &album).await?,
                format!("album{user}_{album}"),
            ),
            Target::Tagged(user) => (&TAGGED, json!({"user": {"id": user}}), format!("tag{user}")),
            Target::WallPost(user, wall) => (
                &WALL_POST,
                wall_metadata(http, &user, &wall).await?,
                format!("wall{user}_{wall}"),
            ),
        };
        let data = data.as_object().cloned().unwrap_or_default();
        Ok(photo_pages(http, route, data, list))
    })
}

/// Decode a VK response body: UTF-8 when valid, otherwise windows-1251.
fn decode(body: &[u8]) -> String {
    match std::str::from_utf8(body) {
        Ok(text) => text.to_owned(),
        Err(_) => body
            .iter()
            .map(|&b| match b {
                0..=0x7f => b as char,
                _ => char::from_u32(u32::from(CP1251[usize::from(b - 0x80)])).unwrap_or('\u{fffd}'),
            })
            .collect(),
    }
}

/// windows-1251 code points for bytes 0x80..=0xFF (0x98 is undefined).
const CP1251: [u16; 128] = [
    0x0402, 0x0403, 0x201A, 0x0453, 0x201E, 0x2026, 0x2020, 0x2021, 0x20AC, 0x2030, 0x0409, 0x2039,
    0x040A, 0x040C, 0x040B, 0x040F, 0x0452, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014,
    0xFFFD, 0x2122, 0x0459, 0x203A, 0x045A, 0x045C, 0x045B, 0x045F, 0x00A0, 0x040E, 0x045E, 0x0408,
    0x00A4, 0x0490, 0x00A6, 0x00A7, 0x0401, 0x00A9, 0x0404, 0x00AB, 0x00AC, 0x00AD, 0x00AE, 0x0407,
    0x00B0, 0x00B1, 0x0406, 0x0456, 0x0491, 0x00B5, 0x00B6, 0x00B7, 0x0451, 0x2116, 0x0454, 0x00BB,
    0x0458, 0x0405, 0x0455, 0x0457, 0x0410, 0x0411, 0x0412, 0x0413, 0x0414, 0x0415, 0x0416, 0x0417,
    0x0418, 0x0419, 0x041A, 0x041B, 0x041C, 0x041D, 0x041E, 0x041F, 0x0420, 0x0421, 0x0422, 0x0423,
    0x0424, 0x0425, 0x0426, 0x0427, 0x0428, 0x0429, 0x042A, 0x042B, 0x042C, 0x042D, 0x042E, 0x042F,
    0x0430, 0x0431, 0x0432, 0x0433, 0x0434, 0x0435, 0x0436, 0x0437, 0x0438, 0x0439, 0x043A, 0x043B,
    0x043C, 0x043D, 0x043E, 0x043F, 0x0440, 0x0441, 0x0442, 0x0443, 0x0444, 0x0445, 0x0446, 0x0447,
    0x0448, 0x0449, 0x044A, 0x044B, 0x044C, 0x044D, 0x044E, 0x044F,
];

async fn page(http: &Http, path: &str) -> Result<String> {
    let url = http.api(&format!("{ROOT}/{path}"))?;
    Ok(decode(&http.get(&url, &[("Referer", REFERER)]).await?))
}

/// Upstream `VkPhotosExtractor.metadata`: the profile data and the user ID
/// used for the photo list.
async fn profile(http: &Http, id: Option<String>, name: Option<String>) -> Result<(Value, String)> {
    match id {
        Some(id) => {
            let prefix = if id.starts_with('-') { "public" } else { "id" };
            let text = page(http, &format!("{prefix}{}", id.trim_start_matches('-'))).await?;
            Ok((parse_profile(&text), id))
        }
        None => {
            let text = page(http, &name.unwrap_or_default()).await?;
            let data = parse_profile(&text);
            let id = data["user"]["id"].as_str().unwrap_or_default().to_owned();
            Ok((data, id))
        }
    }
}

/// Upstream `_extract_profile`.
fn parse_profile(page: &str) -> Value {
    let mut scan = Scan::new(page);
    let id = scan.extr("property=\"og:url\" content=\"https://vk.com/id", "\"");
    let nick = unescape(scan.extr("<title>", " | VK</title>"));
    let info = unescape(scan.extr(",\"activity\":\"", "\",\"")).replace("\\/", "/");
    let name = scan.extr("href=\"https://m.vk.com/", "\"");
    let (id, group) = if id.is_empty() {
        (scan.extr("data-from-id=\"", "\""), true)
    } else {
        (id, false)
    };
    json!({"user": {"id": id, "nick": nick, "info": info, "name": name, "group": group}})
}

/// Upstream `VkAlbumExtractor.metadata`.
async fn album_metadata(http: &Http, user: &str, album: &str) -> Result<Value> {
    let text = page(http, &format!("album{user}_{album}")).await?;
    Ok(parse_album(&text, user, album))
}

fn parse_album(page: &str, user: &str, album: &str) -> Value {
    let desc = extr(page, "name=\"og:description\" value=\"", "\"");
    let parts: Vec<&str> = desc.rsplitn(3, " - ").collect();
    let [photos, user_name, album_name] = parts[..] else {
        // Upstream logs a warning and continues with the IDs only.
        return json!({"user": {"id": user}, "album": {"id": album}});
    };
    let count: String = {
        let chars: Vec<char> = photos.chars().collect();
        chars[..chars.len().saturating_sub(7)].iter().collect()
    };
    json!({
        "user": {
            "id": user,
            "nick": unescape(user_name),
            "name": unescape(extr(page, "class=\"ui_crumb\" href=\"/", "\"")),
            "group": user.starts_with('-'),
        },
        "album": {
            "id": album,
            "name": unescape(album_name),
            "count": parse_int(&count, 0),
        },
    })
}

/// Upstream `VkWallPostExtractor.metadata`.
async fn wall_metadata(http: &Http, user: &str, wall: &str) -> Result<Value> {
    let text = page(http, &format!("wall{user}_{wall}")).await?;
    Ok(parse_wall(&text, user, wall))
}

fn parse_wall(page: &str, user: &str, wall: &str) -> Value {
    let desc = match extr(page, "data-testid=\"post_description\">", "</div>") {
        "" => extr(page, "name=\"description\" content=\"", "\""),
        desc => desc,
    };
    json!({"user": {"id": user}, "wall": {"id": wall, "description": unescape(desc)}})
}

/// The `offset` option, as `text.parse_int`.
fn start_offset(http: &Http) -> i64 {
    match http.option("offset") {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        Some(Value::String(s)) => parse_int(s, 0),
        _ => 0,
    }
    .max(0)
}

fn number(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Upstream `_pagination` over `al_photos.php`.
fn photo_pages<'a>(
    http: &'a Http,
    route: &'static Route,
    data: Map<String, Value>,
    list: String,
) -> Events<'a> {
    let offset = start_offset(http);
    crate::sites::paginate(Some(offset), move |state| {
        let data = data.clone();
        let list = list.clone();
        async move {
            let Some(offset) = state else {
                return Ok(None);
            };
            // Upstream `request_interval = (0.5, 1.5)`.
            #[cfg(not(test))]
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            let url = http.api(&format!("{ROOT}/al_photos.php"))?;
            let referer = format!("{ROOT}/{list}");
            let offset_text = offset.to_string();
            let form = [
                ("act", "show"),
                ("al", "1"),
                ("direction", "1"),
                ("list", list.as_str()),
                ("offset", offset_text.as_str()),
            ];
            let body = http
                .post(
                    &url,
                    Body::Form(&form),
                    &[
                        ("X-Requested-With", "XMLHttpRequest"),
                        ("Origin", ROOT),
                        ("Referer", &referer),
                    ],
                )
                .await?;
            let response: Value = serde_json::from_str(&decode(&body))
                .map_err(|_| error("VK returned invalid JSON"))?;
            let payload = response
                .get("payload")
                .and_then(|p| p.get(1))
                .and_then(Value::as_array)
                .ok_or_else(|| error("VK returned an unexpected photo list response"))?;
            if payload.len() < 4 {
                return Err(error(
                    "VK refused the photo list; it may need login cookies for vk.com",
                ));
            }
            let total =
                number(payload.get(1)).ok_or_else(|| error("VK photo list has no total count"))?;
            let mut photos = payload[3].as_array().cloned().unwrap_or_default();
            for (i, photo) in photos.iter_mut().enumerate() {
                if let Some(photo) = photo.as_object_mut() {
                    photo.insert("num".into(), (offset + i as i64 + 1).into());
                    photo.insert("count".into(), total.into());
                }
            }
            let offset_next = offset + photos.len() as i64;
            let next = if offset_next >= total || photos.is_empty() {
                // The last chunk repeats the first photos when `total` is
                // not a multiple of ten.
                let keep = photos.len() as i64 + (total - offset_next).min(0);
                photos.truncate(keep.max(0) as usize);
                None
            } else {
                Some(offset_next)
            };
            let items = photos
                .into_iter()
                .filter_map(|photo| photo_item(photo.as_object()?.clone(), &data, &list))
                .collect();
            let events = crate::sites::post(&SITE, http, route, items);
            Ok(Some((crate::sites::events(events), next)))
        }
    })
}

/// Python `re.subn(r"/imp[fg]/", "/", text)`.
fn strip_imp(text: &str) -> (String, usize) {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    let mut count = 0;
    while let Some(pos) = rest.find("/imp") {
        let tail = &rest[pos + 4..];
        if tail.starts_with("f/") || tail.starts_with("g/") {
            out.push_str(&rest[..pos]);
            out.push('/');
            rest = &tail[2..];
            count += 1;
        } else {
            out.push_str(&rest[..pos + 4]);
            rest = tail;
        }
    }
    out.push_str(rest);
    (out, count)
}

/// Upstream `VkExtractor.items` for one photo; `None` when it has no URL.
fn photo_item(
    mut photo: Map<String, Value>,
    data: &Map<String, Value>,
    list: &str,
) -> Option<Item> {
    let size = SIZES
        .chars()
        .map(|c| format!("{c}_"))
        .find(|key| photo.contains_key(key))?;
    let raw = photo.get(&format!("{size}src"))?.as_str()?.to_owned();
    let (stripped, count) = strip_imp(raw.split('?').next().unwrap_or_default());
    let url = if count > 0 { stripped } else { raw };
    photo.insert("url".into(), url.clone().into());

    let (width, height) = match photo.get(&size).and_then(Value::as_array) {
        Some(dims) if dims.len() == 3 => (dims[1].clone(), dims[2].clone()),
        // Photos without width/height entries (#2535).
        _ => (0.into(), 0.into()),
    };
    photo.insert("width".into(), width);
    photo.insert("height".into(), height);

    let id = photo.get("id").map(scalar).unwrap_or_default();
    let id = id.rsplit('_').next().unwrap_or_default().to_owned();
    photo.insert("id".into(), id.clone().into());
    let timestamp = extr(
        photo
            .get("date")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        "data-date=\"",
        "\"",
    )
    .trim()
    .parse::<i64>()
    .ok();
    photo.insert("date".into(), date(timestamp));
    let description = unescape(extr(
        photo
            .get("desc")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        ">",
        "<",
    ));
    photo.insert("description".into(), description.clone().into());
    photo.extend(data.clone());
    nameext_into(&url, &mut photo);

    let url = Url::parse(&url).ok().filter(plain)?;
    let user = &data["user"];
    let username = user
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| user.get("id").and_then(Value::as_str))
        .map(str::to_owned);
    Some(
        Item::new(url, photo)
            .ids(list, id)
            .user(username)
            .caption(Some(description))
            .timestamp(timestamp)
            .header("Referer", REFERER),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{fails, http, json, keywords, media, response, serve};

    fn t(raw: &str) -> Option<Target> {
        target(&Url::parse(raw).unwrap())
    }

    fn photos_id(id: &str) -> Option<Target> {
        Some(Target::Photos {
            id: Some(id.into()),
            name: None,
        })
    }

    #[test]
    fn classifies_urls() {
        assert_eq!(t("https://vk.com/id398982326"), photos_id("398982326"));
        assert_eq!(
            t("https://vk.com/photos-165740836"),
            photos_id("-165740836")
        );
        assert_eq!(
            t("https://m.vk.com/albums398982326"),
            photos_id("398982326")
        );
        assert_eq!(t("https://www.vk.com/id123abc?x=1"), photos_id("123"));
        assert_eq!(t("https://vk.com/photos123_456"), photos_id("123"));
        assert_eq!(
            t("https://vk.com/cosplayinrussia"),
            Some(Target::Photos {
                id: None,
                name: Some("cosplayinrussia".into())
            })
        );
        assert_eq!(
            t("https://vk.com/albumfoo/x"),
            Some(Target::Photos {
                id: None,
                name: Some("albumfoo".into())
            })
        );
        assert_eq!(
            t("https://vk.com/idiot"),
            Some(Target::Photos {
                id: None,
                name: Some("idiot".into())
            })
        );
        assert_eq!(
            t("https://vk.com/album-165740836_281339889"),
            Some(Target::Album("-165740836".into(), "281339889".into()))
        );
        assert_eq!(
            t("https://vk.com/tag304303884"),
            Some(Target::Tagged("304303884".into()))
        );
        assert_eq!(
            t("https://vk.com/wall-164344045_7212"),
            Some(Target::WallPost("-164344045".into(), "7212".into()))
        );
        assert_eq!(
            t("https://m.vk.com/wall1_2x?z=1"),
            Some(Target::WallPost("1".into(), "2".into()))
        );
        for raw in [
            "https://vk.com/",
            "https://vk.com",
            "http://vk.com/id1",
            "https://vk.com:8443/id1",
            "https://u:p@vk.com/id1",
            "https://vk.com.evil.test/id1",
            "https://evilvk.com/id1",
            "https://new.vk.com/id1",
            "https://vk.com/album1_2?x",
            "https://vk.com/album1_2#x",
            "https://vk.com/album1_2x",
            "https://vk.com/album123",
            "https://vk.com/tag1?x",
            "https://vk.com/tag1x",
            "https://vk.com/wall123",
            "https://vk.com/wall1_",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[test]
    fn decodes_windows_1251() {
        assert_eq!(decode("ok Привет".as_bytes()), "ok Привет");
        assert_eq!(
            decode(&[
                0xcf, 0xf0, 0xe8, 0xe2, 0xe5, 0xf2, b' ', 0xa8, 0xb8, 0xb9, 0x98
            ]),
            "Привет Ёё№\u{fffd}"
        );
    }

    #[test]
    fn strips_imp_segments() {
        assert_eq!(
            strip_imp("https://sun9-1.userapi.com/impg/c1/impf/x.jpg"),
            ("https://sun9-1.userapi.com/c1/x.jpg".into(), 2)
        );
        assert_eq!(
            strip_imp("https://x/imp/impx/a.jpg"),
            ("https://x/imp/impx/a.jpg".into(), 0)
        );
    }

    #[test]
    fn parses_profiles() {
        let user = parse_profile(
            r#"<meta property="og:url" content="https://vk.com/id398982326"><title>Dobrov &amp; Co | VK</title>
            {"x":1,"activity":"a\/b &amp; c","y":2} <a href="https://m.vk.com/dobrov">"#,
        );
        assert_eq!(
            user,
            json!({"user": {"id": "398982326", "nick": "Dobrov & Co", "info": "a/b & c",
                "name": "dobrov", "group": false}})
        );
        let group = parse_profile(
            r#"<title>Cosplay | VK</title><a href="https://m.vk.com/cosplay"> data-from-id="-165740836""#,
        );
        assert_eq!(group["user"]["id"], "-165740836");
        assert_eq!(group["user"]["group"], true);
        assert_eq!(group["user"]["info"], "");
    }

    #[test]
    fn parses_albums_and_walls() {
        let page = r#"<meta name="og:description" value="Album - with - dashes - Cosplay &amp; Co - 42 photos">
            <a class="ui_crumb" href="/cosplay">"#;
        assert_eq!(
            parse_album(page, "-165740836", "281339889"),
            json!({"user": {"id": "-165740836", "nick": "Cosplay & Co", "name": "cosplay",
                "group": true}, "album": {"id": "281339889", "name": "Album - with - dashes",
                "count": 42}})
        );
        assert_eq!(
            parse_album("<html>", "1", "2"),
            json!({"user": {"id": "1"}, "album": {"id": "2"}})
        );
        assert_eq!(
            parse_wall(
                r#"<div data-testid="post_description">Hi &amp; bye</div>"#,
                "1",
                "2"
            )["wall"]["description"],
            "Hi & bye"
        );
        assert_eq!(
            parse_wall(r#"<meta name="description" content="meta">"#, "1", "2")["wall"],
            json!({"id": "2", "description": "meta"})
        );
    }

    fn photo(id: &str, src: &str) -> Value {
        json!({"id": id, "x_": ["a", 604, 403], "x_src": src,
            "date": "<span class=\"rel_date\" data-date=\"1530000000\">x</span>",
            "desc": "<div class=\"d\">A &amp; B</div>"})
    }

    fn page_body(total: i64, photos: Vec<Value>) -> Vec<u8> {
        json(json!({"payload": [0, ["list", total, 0, photos]]}))
    }

    #[tokio::test]
    async fn extracts_tagged_photos_across_pages() {
        let first: Vec<Value> = (1..=10)
            .map(|i| {
                photo(
                    &format!("1_{i}"),
                    &format!("https://sun9-1.userapi.com/impg/c/p{i}.jpg?size=1&type=album"),
                )
            })
            .collect();
        // The last page repeats photos 1-8 after the 12 real ones.
        let mut second = vec![
            photo("1_11", "https://sun9-1.userapi.com/c/p11.jpg?size=1"),
            json!({"id": "1_12", "w_": ["a"], "w_src": "https://sun9-1.userapi.com/c/p12.png"}),
        ];
        second.extend((1..=8).map(|i| photo(&format!("1_{i}"), "https://x.test/dup.jpg")));
        let (root, server) = serve(vec![page_body(12, first), page_body(12, second)]);
        let http = http("vk", root);
        let files = media(extract(
            &http,
            Url::parse("https://vk.com/tag304303884").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 12);
        assert_eq!(files[0].url.as_str(), "https://sun9-1.userapi.com/c/p1.jpg");
        assert_eq!(
            files[10].url.as_str(),
            "https://sun9-1.userapi.com/c/p11.jpg?size=1"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["id"], "1");
        assert_eq!(k["num"], 1);
        assert_eq!(k["count"], 12);
        assert_eq!(k["width"], 604);
        assert_eq!(k["height"], 403);
        assert_eq!(k["date"], "2018-06-26 08:00:00");
        assert_eq!(k["description"], "A & B");
        assert_eq!(k["user"], json!({"id": "304303884"}));
        assert_eq!(k["subcategory"], "tagged");
        assert_eq!(k["filename"], "p1");
        let k = keywords(&files[11]);
        assert_eq!(
            (k["num"].clone(), k["width"].clone()),
            (json!(12), json!(0))
        );
        assert_eq!(k["date"], "[Invalid DateTime]");
        assert_eq!(k["extension"], "png");
        let (file, dir) = crate::sites::default_templates(&files[11]).unwrap();
        assert_eq!(file.render(&files[11]).unwrap(), "12.png");
        assert_eq!(
            dir.render(&files[11]).unwrap().unwrap(),
            std::path::PathBuf::from("vk/304303884/tags")
        );
        let (_, identity) = crate::sites::archive_key(&files[11]).unwrap();
        assert_eq!(identity, r#"["{user[id]}_{id}",["304303884","12"]]"#);
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("POST /al_photos.php "),
            "{}",
            requests[0]
        );
        let lower = requests[0].to_ascii_lowercase();
        assert!(lower.contains("x-requested-with: xmlhttprequest"));
        assert!(lower.contains("origin: https://vk.com"));
        assert!(lower.contains("referer: https://vk.com/tag304303884"));
        assert!(requests[0].ends_with("act=show&al=1&direction=1&list=tag304303884&offset=0"));
        assert!(requests[1].ends_with("&offset=10"));
    }

    #[tokio::test]
    async fn extracts_user_photos_by_name_with_offset_option() {
        // A windows-1251 page with a Cyrillic title.
        let mut page =
            b"<meta property=\"og:url\" content=\"https://vk.com/id42\"><title>".to_vec();
        page.extend_from_slice(&[0xcf, 0xf0, 0xe8, 0xe2, 0xe5, 0xf2]);
        page.extend_from_slice(b" | VK</title><a href=\"https://m.vk.com/someone\">");
        let (root, server) = serve(vec![
            response("200 OK", "text/html; charset=windows-1251", &page),
            page_body(
                6,
                vec![
                    photo("42_5", "https://sun9-1.userapi.com/a/5.jpg"),
                    json!({"id": "42_6", "src": "none"}),
                ],
            ),
        ]);
        let options = json!({"offset": "4"})
            .as_object()
            .cloned()
            .map(std::sync::Arc::new);
        let http = http("vk", root).with_options(options);
        let files = media(extract(
            &http,
            Url::parse("https://vk.com/someone").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let k = keywords(&files[0]);
        assert_eq!(k["num"], 5);
        assert_eq!(k["user"]["nick"], "Привет");
        assert_eq!(k["user"]["name"], "someone");
        assert_eq!(k["subcategory"], "photos");
        let (_, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("vk/someone")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /someone "));
        assert!(requests[1].ends_with("list=photos42&offset=4"));
    }

    #[tokio::test]
    async fn extracts_group_photos_by_id() {
        let (root, server) = serve(vec![
            response(
                "200 OK",
                "text/html",
                b"<title>G | VK</title> data-from-id=\"-7\"",
            ),
            page_body(0, vec![]),
        ]);
        let http = http("vk", root);
        let files = media(extract(
            &http,
            Url::parse("https://vk.com/photos-7").unwrap(),
        ))
        .await;
        assert!(files.is_empty());
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /public7 "));
        assert!(requests[1].ends_with("list=photos-7&offset=0"));
    }

    #[tokio::test]
    async fn extracts_album_and_wall_post() {
        let (root, server) = serve(vec![
            response(
                "200 OK",
                "text/html",
                br#"<meta name="og:description" value="Cos - Group - 1 photos"><a class="ui_crumb" href="/grp">"#,
            ),
            page_body(1, vec![photo("-5_9", "https://sun9-1.userapi.com/a/9.jpg")]),
            response(
                "200 OK",
                "text/html",
                br#"<div data-testid="post_description">Post</div>"#,
            ),
            page_body(1, vec![photo("-5_9", "https://sun9-1.userapi.com/a/9.jpg")]),
        ]);
        let http = http("vk", root);
        let files = media(extract(
            &http,
            Url::parse("https://vk.com/album-5_77").unwrap(),
        ))
        .await;
        let k = keywords(&files[0]);
        assert_eq!(k["album"], json!({"id": "77", "name": "Cos", "count": 1}));
        assert_eq!(k["user"]["group"], true);
        let (_, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("vk/-5/77")
        );
        let files = media(extract(
            &http,
            Url::parse("https://vk.com/wall-5_3").unwrap(),
        ))
        .await;
        let k = keywords(&files[0]);
        assert_eq!(k["wall"], json!({"id": "3", "description": "Post"}));
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "3_1.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("vk/-5/wall")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /album-5_77 "));
        assert!(requests[1].ends_with("list=album-5_77&offset=0"));
        assert!(requests[2].starts_with("GET /wall-5_3 "));
        assert!(requests[3].ends_with("list=wall-5_3&offset=0"));
    }

    #[tokio::test]
    async fn reports_errors_without_urls() {
        let (root, _server) = serve(vec![
            json(json!({"payload": [0, ["Access denied <a href=\"https://vk.com/login\">"]]})),
            json(json!({"payload": [0, "x"]})),
            response("200 OK", "application/json", b"<html>"),
        ]);
        let http = http("vk", root);
        for _ in 0..3 {
            let events: Vec<_> = futures_util::StreamExt::collect(extract(
                &http,
                Url::parse("https://vk.com/tag1").unwrap(),
            ))
            .await;
            let message = events[0].as_ref().err().unwrap().to_string();
            assert!(
                message.contains("VK") && !message.contains("vk.com/"),
                "{message}"
            );
        }
        assert!(fails(extract(&http, Url::parse("https://vk.com/").unwrap())).await);
    }
}
