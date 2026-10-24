//! Profile routes, mirroring upstream's `FacebookUserExtractor` (which
//! dispatches to photos by default), `FacebookPhotosExtractor`,
//! `FacebookAvatarExtractor` and `FacebookAlbumsExtractor`. Profile pages
//! only resolve a set, album or avatar photo; the photos themselves are read
//! through the existing album and photo traversal.
use crate::{Result, error};
use serde_json::Value;
use url::Url;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Tab {
    Photos,
    Avatar,
    /// Every album, or only those whose title matches (case-insensitively).
    Albums(Option<String>),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ProfileInput {
    pub(crate) profile: String,
    pub(crate) tab: Tab,
}

impl ProfileInput {
    /// The page that names this route's set, album list or avatar.
    pub(crate) fn page(&self) -> Result<Url> {
        let suffix = match self.tab {
            Tab::Photos => "/photos_by",
            Tab::Avatar => "",
            Tab::Albums(_) => "/photos_albums",
        };
        Url::parse(&format!(
            "https://www.facebook.com/{}{suffix}",
            self.profile
        ))
        .map_err(|_| error("invalid Facebook profile URL"))
    }
}

/// First path segments that are Facebook features, not profiles.
const RESERVED: &[&str] = &[
    "media",
    "photo",
    "photo.php",
    "watch",
    "permalink.php",
    "profile.php",
    "people",
    "groups",
    "events",
    "pages",
    "reel",
    "reels",
    "stories",
    "story.php",
    "share",
    "sharer",
    "sharer.php",
    "hashtag",
    "marketplace",
    "gaming",
    "login",
    "login.php",
    "help",
    "settings",
    "home.php",
    "search",
    "notifications",
    "messages",
    "friends",
    "bookmarks",
    "privacy",
    "policies",
    "ads",
    "business",
    "l.php",
];

fn valid_profile(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        && !RESERVED.contains(&name.to_ascii_lowercase().as_str())
}

/// Upstream `USER_PATTERN` plus one of the supported tabs: nothing (the user
/// dispatcher, photos by default), `photos`, `photos_by`, `avatar` or
/// `photos_albums[/NAME]`. `profile.php?id=ID` also accepts the `sk` tab
/// parameter that Facebook itself uses.
pub(crate) fn profile_input(url: &Url) -> Option<ProfileInput> {
    if !matches!(url.scheme(), "http" | "https")
        || !matches!(
            url.host_str(),
            Some("facebook.com" | "www.facebook.com" | "m.facebook.com" | "web.facebook.com")
        )
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return None;
    }
    let segments: Vec<&str> = url.path().split('/').filter(|s| !s.is_empty()).collect();
    let tab = |rest: &[&str]| -> Option<Tab> {
        match rest {
            [] => Some(Tab::Photos),
            ["photos" | "photos_by"] => Some(Tab::Photos),
            ["avatar"] => Some(Tab::Avatar),
            ["photos_albums"] => Some(Tab::Albums(None)),
            ["photos_albums", name] => Some(Tab::Albums(Some(percent_decode(name).to_lowercase()))),
            _ => None,
        }
    };
    let (profile, tab) = match segments.as_slice() {
        ["profile.php"] => {
            let query = |name: &str| {
                url.query_pairs()
                    .find(|(key, _)| key == name)
                    .map(|(_, value)| value.into_owned())
            };
            let tab = match query("sk").as_deref() {
                None | Some("photos" | "photos_by") => Tab::Photos,
                Some("photos_albums") => Tab::Albums(None),
                Some(_) => return None,
            };
            (query("id")?, tab)
        }
        ["people", _, id, rest @ ..] => (id.to_string(), tab(rest)?),
        [name, rest @ ..] => (name.to_string(), tab(rest)?),
        [] => return None,
    };
    valid_profile(&profile).then_some(ProfileInput { profile, tab })
}

fn percent_decode(text: &str) -> String {
    url::form_urlencoded::parse(format!("x={text}").as_bytes())
        .next()
        .map(|(_, value)| value.into_owned())
        .unwrap_or_default()
}

/// The text between `start` and the next `end` after it.
fn extr<'a>(text: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let from = text.find(start)? + start.len();
    let len = text[from..].find(end)?;
    Some(&text[from..from + len])
}

fn page_text(bytes: &[u8]) -> Result<&str> {
    let page = std::str::from_utf8(bytes).map_err(|_| error("Facebook returned invalid UTF-8"))?;
    if page.contains(r#""props":{"title":"This content isn't available right now""#) {
        return Err(error(
            "Facebook content unavailable to this session; login may be required",
        ));
    }
    Ok(page)
}

/// Upstream `_extract_profile_set_id`: the set of the profile's photos.
pub(crate) fn parse_photos_set(bytes: &[u8]) -> Result<Option<String>> {
    let page = page_text(bytes)?;
    let Some(items) = extr(page, r#""pageItems""#, r#""page_info""#) else {
        return Ok(None);
    };
    let set = extr(items, "set=", "\"")
        .map(|set| set.rsplit_once('&').map_or(set, |(head, _)| head))
        .filter(|set| !set.is_empty())
        .or_else(|| extr(items, r"\/photos\/", r"\/"));
    Ok(set.map(str::to_owned))
}

/// The first photos listed on the `photos_by` page, in order, and whether
/// more follow; anonymous sessions cannot read them from the set page.
pub(crate) fn parse_photos_grid(bytes: &[u8]) -> crate::facebook_album::Grid {
    let mut grid = crate::facebook_album::Grid::default();
    let Ok(page) = std::str::from_utf8(bytes) else {
        return grid;
    };
    let Some(start) = page.find(r#""pageItems""#) else {
        return grid;
    };
    let rest = &page[start..];
    let Some(end) = rest.find(r#""page_info""#) else {
        return grid;
    };
    let marker = r#""node":{"__typename":"Photo","id":""#;
    let mut items = &rest[..end];
    while let Some(pos) = items.find(marker) {
        items = &items[pos + marker.len()..];
        let id = &items[..items.find('"').unwrap_or(0)];
        if !id.is_empty()
            && id.len() <= 32
            && id.bytes().all(|b| b.is_ascii_digit())
            && !grid.ids.iter().any(|seen| seen == id)
        {
            grid.ids.push(id.to_owned());
        }
    }
    let info = &rest[end..];
    grid.more = extr(info, r#""has_next_page":"#, "}") == Some("true");
    grid.cursor = extr(info, r#""end_cursor":""#, "\"")
        .filter(|c| !c.is_empty() && c.len() <= 1024)
        .map(str::to_owned);
    // The collection's own ID follows its `pageItems`.
    grid.node = extr(info, r#"},"id":""#, "\"")
        .filter(|id| {
            !id.is_empty()
                && id.len() <= 512
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'=' | b'_' | b'-'))
        })
        .map(str::to_owned);
    grid.lsd = crate::facebook_album::lsd_token(bytes);
    grid.query = crate::facebook_album::GridQuery::Collection;
    grid
}

/// Post permalinks: `NAME/posts/ID`, `groups/GROUP/posts|permalink/ID`,
/// `permalink.php?story_fbid=ID` and `story.php?story_fbid=ID`.
pub(crate) fn is_post_url(url: &Url) -> bool {
    if !matches!(url.scheme(), "http" | "https")
        || !matches!(
            url.host_str(),
            Some("facebook.com" | "www.facebook.com" | "m.facebook.com" | "web.facebook.com")
        )
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return false;
    }
    let segments: Vec<&str> = url.path().split('/').filter(|s| !s.is_empty()).collect();
    let id =
        |s: &str| !s.is_empty() && s.len() <= 128 && s.bytes().all(|b| b.is_ascii_alphanumeric());
    match segments.as_slice() {
        ["permalink.php" | "story.php"] => {
            url.query_pairs().any(|(k, v)| k == "story_fbid" && id(&v))
        }
        ["groups", group, "posts" | "permalink", post] => {
            (id(group) || valid_profile(group)) && id(post)
        }
        [name, "posts", post] => valid_profile(name) && id(post),
        _ => false,
    }
}

/// The distinct photos and the first video a post page shows, in order.
pub(crate) fn parse_post(bytes: &[u8]) -> Result<(Vec<String>, Option<String>)> {
    page_text(bytes)?;
    let payloads = crate::facebook::embedded_payloads(bytes)?;
    let mut photos: Vec<String> = Vec::new();
    let mut video = None;
    for payload in &payloads {
        crate::facebook_album::visit(payload, &mut |value| {
            let kind = value.get("__typename").and_then(Value::as_str);
            let is_media = value.get("__isMedia").and_then(Value::as_str);
            let Some(id) = value.get("id").and_then(Value::as_str).filter(|id| {
                !id.is_empty() && id.len() <= 32 && id.bytes().all(|b| b.is_ascii_digit())
            }) else {
                return;
            };
            if kind == Some("Photo") && is_media == Some("Photo") {
                if !photos.iter().any(|seen| seen == id) {
                    photos.push(id.to_owned());
                }
            } else if kind == Some("Video") && is_media == Some("Video") && video.is_none() {
                video = Some(id.to_owned());
            }
        });
    }
    if photos.is_empty() && video.is_none() {
        return Err(error(
            "Facebook post shows no photos or videos without login; pass --cookies",
        ));
    }
    Ok((photos, video))
}

/// Upstream `_extract_profile_user`: the embedded profile object.
fn parse_user(page: &str) -> Option<Value> {
    let data = extr(page, r#"","user":{""#, r#"},"viewer":{"#)?;
    serde_json::from_str(&format!("{{\"{data}}}")).ok()
}

/// The avatar's photo permalink (`profilePhoto.url`), if the profile has one.
pub(crate) fn parse_avatar(bytes: &[u8]) -> Result<Url> {
    let page = page_text(bytes)?;
    parse_user(page)
        .as_ref()
        .and_then(|user| user.pointer("/profilePhoto/url")?.as_str())
        .and_then(|url| Url::parse(url).ok())
        .filter(|url| crate::facebook::photo_id(url).is_ok())
        .ok_or_else(|| error("Facebook profile has no avatar photo; login may be required"))
}

/// Upstream `FacebookAlbumsExtractor.items`: album URLs in page order,
/// optionally only those titled `name` (lowercase).
pub(crate) fn parse_albums(bytes: &[u8], name: Option<&str>) -> Result<AlbumList> {
    let page = page_text(bytes)?;
    let marker = r#""TimelineAppCollectionAlbumsRenderer","collection":{"id":""#;
    let Some(pos) = page.find(marker) else {
        return Ok(AlbumList::default());
    };
    let collection = extr(&page[pos..], marker, "\"")
        .filter(|id| valid_node(id))
        .map(str::to_owned);
    let items = extr(&page[pos..], r#"},"pageItems":"#, "}}},")
        .ok_or_else(|| error("Facebook album list is incomplete"))?;
    let data: Value = serde_json::from_str(&format!("{items}}}}}"))
        .map_err(|_| error("Facebook album list is invalid"))?;
    let mut list = album_edges(&data, name);
    list.collection = collection;
    list.lsd = crate::facebook_album::lsd_token(bytes);
    Ok(list)
}

/// The next album page of `ProfileCometAppCollectionAlbumsRendererPaginationQuery`.
pub(crate) fn parse_album_page(bytes: &[u8], name: Option<&str>) -> Option<AlbumList> {
    let data = serde_json::Deserializer::from_slice(bytes)
        .into_iter::<Value>()
        .next()?
        .ok()?;
    Some(album_edges(data.pointer("/data/node/pageItems")?, name))
}

/// One page of a profile's albums and how to continue it.
#[derive(Debug, Default)]
pub(crate) struct AlbumList {
    pub(crate) albums: Vec<Url>,
    pub(crate) more: bool,
    pub(crate) cursor: Option<String>,
    pub(crate) collection: Option<String>,
    pub(crate) lsd: Option<String>,
}

fn valid_node(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 512
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'=' | b'_' | b'-'))
}

/// Album URLs of a `pageItems` object, optionally only those titled `name`.
fn album_edges(data: &Value, name: Option<&str>) -> AlbumList {
    let mut albums = Vec::new();
    for edge in data
        .get("edges")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(node) = edge.get("node") else {
            continue;
        };
        let title = node
            .pointer("/title/text")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if name.is_some_and(|name| name != title.to_lowercase()) {
            continue;
        }
        if let Some(url) = node
            .pointer("/node/url")
            .and_then(Value::as_str)
            .and_then(|url| Url::parse(url).ok())
        {
            albums.push(url);
        }
    }
    AlbumList {
        albums,
        more: data
            .pointer("/page_info/has_next_page")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        cursor: data
            .pointer("/page_info/end_cursor")
            .and_then(Value::as_str)
            .filter(|c| !c.is_empty() && c.len() <= 1024)
            .map(str::to_owned),
        ..AlbumList::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(raw: &str) -> Option<ProfileInput> {
        profile_input(&Url::parse(raw).unwrap())
    }

    fn of(profile: &str, tab: Tab) -> Option<ProfileInput> {
        Some(ProfileInput {
            profile: profile.into(),
            tab,
        })
    }

    #[test]
    fn classifies_profile_urls() {
        assert_eq!(
            input("https://www.facebook.com/NASA"),
            of("NASA", Tab::Photos)
        );
        assert_eq!(
            input("https://facebook.com/NASA/?ref=x"),
            of("NASA", Tab::Photos)
        );
        assert_eq!(
            input("https://m.facebook.com/nasa.gov/photos"),
            of("nasa.gov", Tab::Photos)
        );
        assert_eq!(
            input("https://www.facebook.com/NASA/photos_by"),
            of("NASA", Tab::Photos)
        );
        assert_eq!(
            input("https://www.facebook.com/NASA/avatar"),
            of("NASA", Tab::Avatar)
        );
        assert_eq!(
            input("https://www.facebook.com/NASA/photos_albums"),
            of("NASA", Tab::Albums(None))
        );
        assert_eq!(
            input("https://www.facebook.com/NASA/photos_albums/Cover%20Photos"),
            of("NASA", Tab::Albums(Some("cover photos".into())))
        );
        assert_eq!(
            input("https://www.facebook.com/profile.php?id=100044561550831"),
            of("100044561550831", Tab::Photos)
        );
        assert_eq!(
            input("https://www.facebook.com/profile.php?id=1&sk=photos_albums"),
            of("1", Tab::Albums(None))
        );
        assert_eq!(
            input("https://www.facebook.com/people/Some-Name/100012345/photos"),
            of("100012345", Tab::Photos)
        );
        for raw in [
            "https://www.facebook.com/",
            "https://www.facebook.com/media/set/?set=a.1",
            "https://www.facebook.com/photo/?fbid=1",
            "https://www.facebook.com/watch/?v=1",
            "https://www.facebook.com/groups/123",
            "https://www.facebook.com/NASA/posts/123",
            "https://www.facebook.com/NASA/videos/123",
            "https://www.facebook.com/NASA/info",
            "https://www.facebook.com/profile.php",
            "https://www.facebook.com/profile.php?id=1&sk=about",
            "https://www.facebook.com:8443/NASA",
            "https://u:p@www.facebook.com/NASA",
            "https://evil.test/NASA",
            "https://www.facebook.com/bad%20name",
        ] {
            assert_eq!(input(raw), None, "{raw}");
        }
        assert_eq!(
            input("https://www.facebook.com/NASA/photos_albums")
                .unwrap()
                .page()
                .unwrap()
                .as_str(),
            "https://www.facebook.com/NASA/photos_albums"
        );
    }

    #[test]
    fn recognizes_posts_and_their_media() {
        let post = |raw: &str| is_post_url(&Url::parse(raw).unwrap());
        assert!(post("https://www.facebook.com/NASA/posts/pfbid02abc"));
        assert!(post(
            "https://www.facebook.com/100077921513450/posts/1162826406324732/"
        ));
        assert!(post(
            "https://www.facebook.com/groups/256772258373648/permalink/2061916421192547/"
        ));
        assert!(post(
            "https://m.facebook.com/permalink.php?story_fbid=12&id=3"
        ));
        assert!(!post("https://www.facebook.com/NASA/photos"));
        assert!(!post("https://www.facebook.com/groups/1/members"));
        assert!(!post("https://www.facebook.com/permalink.php?id=3"));
        assert!(
            profile_input(&Url::parse("https://www.facebook.com/NASA/posts/1").unwrap()).is_none()
        );

        let page =
            br#"<script>{"a":[{"media":{"__typename":"Photo","__isMedia":"Photo","id":"11"}},
            {"media":{"__typename":"Photo","__isMedia":"Photo","id":"12"}},
            {"media":{"__typename":"Photo","__isMedia":"Photo","id":"11"}},
            {"media":{"__typename":"Video","__isMedia":"Video","id":"99"}}]}</script>"#;
        let (photos, video) = parse_post(page).unwrap();
        assert_eq!(photos, ["11", "12"]);
        assert_eq!(video.as_deref(), Some("99"));
        let empty = parse_post(b"<html></html>").unwrap_err().to_string();
        assert!(empty.contains("--cookies"), "{empty}");
    }

    #[test]
    fn parses_profile_pages() {
        let photos = br#"<script>{"pageItems":{"edges":[{"node":{"url":"https:\/\/www.facebook.com\/photo\/?fbid=1&set=pb.100044561550831.-2207520000&type=3"}}]},"page_info":{}}</script>"#;
        assert_eq!(
            parse_photos_set(photos).unwrap().as_deref(),
            Some("pb.100044561550831.-2207520000")
        );
        let legacy = br#"{"pageItems":{"url":"https:\/\/www.facebook.com\/NASA\/photos\/a.5\/1"},"page_info":{}}"#;
        assert_eq!(parse_photos_set(legacy).unwrap().as_deref(), Some("a.5"));
        assert_eq!(parse_photos_set(b"<html></html>").unwrap(), None);
        let grid = parse_photos_grid(
            br#"{"pageItems":{"edges":[{"node":{"node":{"__typename":"Photo","id":"11"}}},{"node":{"node":{"__typename":"Photo","id":"12"}}},{"node":{"node":{"__typename":"Photo","id":"11"}}}]},"page_info":{"end_cursor":"c","has_next_page":true},"id":"Y29sbA=="}["LSD",[],{"token":"lsd1"}]"#,
        );
        assert_eq!(
            (grid.ids, grid.more),
            (vec!["11".to_owned(), "12".to_owned()], true)
        );
        // Enough to continue through the collection's pagination query.
        assert_eq!(grid.cursor.as_deref(), Some("c"));
        assert_eq!(grid.node.as_deref(), Some("Y29sbA=="));
        assert_eq!(grid.lsd.as_deref(), Some("lsd1"));
        assert_eq!(grid.query, crate::facebook_album::GridQuery::Collection);
        assert_eq!(parse_photos_grid(b"<html></html>").ids.len(), 0);

        let profile = br#"x","user":{"__isProfile":"User","name":"NASA","profilePhoto":{"url":"https:\/\/www.facebook.com\/photo\/?fbid=423033359192046&set=a.416661003162615","id":"423033359192046"},"id":"100044561550831"},"viewer":{"x":1}"#;
        assert_eq!(
            parse_avatar(profile).unwrap().as_str(),
            "https://www.facebook.com/photo/?fbid=423033359192046&set=a.416661003162615"
        );
        let missing = parse_avatar(b"<html></html>").unwrap_err().to_string();
        assert!(missing.contains("no avatar"), "{missing}");
        let blocked = br#"{"props":{"title":"This content isn't available right now"}}"#;
        assert!(
            parse_avatar(blocked)
                .unwrap_err()
                .to_string()
                .contains("unavailable")
        );

        let albums = br#"{"__typename":"TimelineAppCollectionAlbumsRenderer","collection":{"id":"c"},"pageItems":{"edges":[{"node":{"node":{"__typename":"Album","url":"https:\/\/www.facebook.com\/media\/set\/?set=a.1&type=3"},"title":{"text":"Cover Photos"}},"cursor":"x"},{"node":{"node":{"__typename":"Album","url":"https:\/\/www.facebook.com\/media\/set\/?set=a.2&type=3"},"title":{"text":"Other"}},"cursor":"y"}],"page_info":{"end_cursor":"z","has_next_page":true}}},"can_viewer_create_album":false}"#;
        let list = parse_albums(albums, None).unwrap();
        assert_eq!((list.more, list.cursor.as_deref()), (true, Some("z")));
        assert_eq!(list.collection.as_deref(), Some("c"));
        let all = list.albums;
        assert_eq!(all.len(), 2);
        assert_eq!(
            all[1].as_str(),
            "https://www.facebook.com/media/set/?set=a.2&type=3"
        );
        let named = parse_albums(albums, Some("cover photos")).unwrap();
        assert_eq!(named.albums.len(), 1);
        assert!(
            parse_albums(b"<html></html>", None)
                .unwrap()
                .albums
                .is_empty()
        );
        let next = parse_album_page(
            br#"{"data":{"node":{"pageItems":{"edges":[{"node":{"title":{"text":"Later"},"node":{"url":"https:\/\/www.facebook.com\/media\/set\/?set=a.3&type=3"}}}],"page_info":{"end_cursor":"z2","has_next_page":false}}}}}"#,
            None,
        )
        .unwrap();
        assert_eq!((next.albums.len(), next.more), (1, false));
    }
}
