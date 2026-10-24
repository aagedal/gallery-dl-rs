//! Conservative album parsing. Fixtures describe the accepted protocol, not a
//! captured authenticated Facebook response; unsupported envelopes fail closed.
use crate::{Result, error};
use serde_json::Value;
use url::Url;

pub(crate) struct AlbumInput {
    pub(crate) set: String,
    pub(crate) start: Option<String>,
}

pub(crate) fn album_input(url: &Url) -> Result<AlbumInput> {
    if !matches!(url.scheme(), "http" | "https")
        || !matches!(
            url.host_str(),
            Some("facebook.com" | "www.facebook.com" | "m.facebook.com")
        )
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return Err(error(
            "native Facebook album extraction requires a Facebook URL",
        ));
    }
    let path = url.path().trim_end_matches('/');
    if !matches!(path, "/media/set" | "/photo" | "/photo.php") {
        return Err(error(
            "unsupported native Facebook album URL; use --extractor compatibility",
        ));
    }
    let mut set = None;
    let mut start = None;
    let mut resume = false;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "set" => {
                if set.replace(value.into_owned()).is_some() {
                    return Err(error("Facebook album URL has duplicate set parameters"));
                }
            }
            "fbid" => {
                if start.replace(value.into_owned()).is_some() {
                    return Err(error("Facebook album URL has duplicate fbid parameters"));
                }
            }
            "setextract" => {
                if resume || !value.is_empty() {
                    return Err(error("invalid Facebook setextract parameter"));
                }
                resume = true;
            }
            _ => {}
        }
    }
    let set = set.ok_or_else(|| error("Facebook album URL requires a set parameter"))?;
    validate_set(&set)?;
    if path == "/media/set" {
        if start.is_some() || resume {
            return Err(error(
                "Facebook album root URL cannot specify a starting photo",
            ));
        }
    } else if start.is_some() {
        if !resume {
            return Err(error("Facebook photo album traversal requires setextract"));
        }
    } else if resume {
        return Err(error("Facebook setextract requires a starting photo"));
    }
    if let Some(id) = &start {
        validate_id(id)?;
    }
    Ok(AlbumInput { set, start })
}

fn validate_set(set: &str) -> Result<()> {
    if set.is_empty()
        || set.len() > 256
        || !set
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(error("invalid Facebook album set identifier"));
    }
    Ok(())
}

fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 32
        || !id.bytes().all(|b| b.is_ascii_digit())
        || id.bytes().all(|b| b == b'0')
    {
        return Err(error("invalid Facebook album photo identifier"));
    }
    Ok(())
}

/// Resolve a seed only from a set-bound ordered collection. Membership alone
/// cannot establish the first position in an album.
pub(crate) fn parse_seed(bytes: &[u8], set: &str) -> Result<String> {
    validate_set(set)?;
    let payloads = crate::facebook::embedded_payloads(bytes)?;
    let mut seeds = Vec::new();
    // Album pages now stream the grid as a deferred `mediaset` fragment
    // without its own token; it counts only when the page names this set.
    let mut grid = Vec::new();
    let mut named = false;
    for payload in &payloads {
        visit(payload, &mut |value| {
            if value.get("path") == Some(&serde_json::json!(["mediaset"]))
                && let Some(edges) = value
                    .pointer("/data/grid_media/edges")
                    .and_then(Value::as_array)
                && let Some(node) = edges.first().and_then(|edge| edge.get("node"))
                && let Some(id) = photo_identifier(node)
            {
                grid.push(id.to_owned());
            }
            if bound_set(value, set) {
                named = true;
                for pointer in ["/photos/edges", "/pageItems/edges"] {
                    if let Some(edges) = value.pointer(pointer).and_then(Value::as_array)
                        && let Some(node) = edges.first().and_then(|edge| edge.get("node"))
                        && let Some(id) = photo_identifier(node)
                    {
                        seeds.push(id.to_owned());
                    }
                }
            }
        });
    }
    if seeds.is_empty() && named {
        seeds = grid;
    }
    seeds.sort();
    seeds.dedup();
    match seeds.as_slice() {
        [id] => Ok(id.clone()),
        [] => Err(error(
            "Facebook album has no supported identity-bound first photo; login may be required",
        )),
        _ => Err(error("Facebook album first photo is ambiguous")),
    }
}

/// The first page of an album's photo grid, in album order, and whether the
/// album continues beyond it.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Grid {
    pub(crate) ids: Vec<String>,
    pub(crate) more: bool,
    /// Continuation of an album grid through its pagination query: the
    /// page's end cursor, the album's node ID and the page's LSD token.
    pub(crate) cursor: Option<String>,
    pub(crate) node: Option<String>,
    pub(crate) lsd: Option<String>,
    pub(crate) query: GridQuery,
}

/// The pagination query that continues a grid.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum GridQuery {
    /// An album's `grid_media`.
    #[default]
    Album,
    /// A profile photo collection's `pageItems` (the `photos_by` tab).
    Collection,
}

impl GridQuery {
    /// Friendly name and persisted ID, observed on 2026-10-23.
    pub(crate) fn operation(self) -> (&'static str, &'static str) {
        match self {
            Self::Album => (
                "ProfileCometLegacyAlbumGridViewPaginationQuery",
                "34407011978913359",
            ),
            Self::Collection => (
                "ProfileCometAppCollectionPhotosRendererPaginationQuery",
                "27028962643386672",
            ),
        }
    }

    fn items(self) -> &'static str {
        match self {
            Self::Album => "/data/node/grid_media",
            Self::Collection => "/data/node/pageItems",
        }
    }
}

/// Photo IDs, `has_next_page` and `end_cursor` of a `grid_media` object.
fn grid_media(media: &Value) -> (Vec<String>, bool, Option<String>) {
    let mut ids: Vec<String> = Vec::new();
    for edge in media
        .get("edges")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        // Collection items wrap their photo in a second `node`.
        let node = edge.get("node");
        if let Some(id) = node
            .and_then(photo_identifier)
            .or_else(|| node.and_then(|n| n.get("node")).and_then(photo_identifier))
            && !ids.iter().any(|seen| seen == id)
        {
            ids.push(id.to_owned());
        }
    }
    let more = media
        .pointer("/page_info/has_next_page")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let cursor = media
        .pointer("/page_info/end_cursor")
        .and_then(Value::as_str)
        .filter(|c| !c.is_empty() && c.len() <= 1024)
        .map(str::to_owned);
    (ids, more, cursor)
}

/// The next grid page from `ProfileCometLegacyAlbumGridViewPaginationQuery`;
/// `None` when the response is not one (for example a rotated query ID).
pub(crate) fn parse_grid_page(bytes: &[u8], query: GridQuery) -> Option<Grid> {
    let data = serde_json::Deserializer::from_slice(bytes)
        .into_iter::<Value>()
        .next()?
        .ok()?;
    let (ids, more, cursor) = grid_media(data.pointer(query.items())?);
    Some(Grid {
        ids,
        more,
        cursor,
        ..Grid::default()
    })
}

/// Anonymous photo pages carry no `nextMediaAfterNodeId`; the album page's
/// deferred `mediaset` grid still lists its first photos. Empty unless the
/// page names `set`.
pub(crate) fn parse_grid(bytes: &[u8], set: &str) -> Grid {
    let Ok(payloads) = crate::facebook::embedded_payloads(bytes) else {
        return Grid::default();
    };
    let mut named = false;
    let mut grid: Option<Grid> = None;
    for payload in &payloads {
        visit(payload, &mut |value| {
            named |= bound_set(value, set);
            if grid.is_none()
                && value.get("path") == Some(&serde_json::json!(["mediaset"]))
                && let Some(media) = value.pointer("/data/grid_media")
            {
                let (ids, more, cursor) = grid_media(media);
                let node = value
                    .pointer("/data/id")
                    .and_then(Value::as_str)
                    .filter(|id| validate_id(id).is_ok())
                    .map(str::to_owned);
                grid = Some(Grid {
                    ids,
                    more,
                    cursor,
                    node,
                    lsd: None,
                    query: GridQuery::Album,
                });
            }
        });
    }
    let Some(mut grid) = grid.filter(|_| named) else {
        return Grid::default();
    };
    grid.lsd = lsd_token(bytes);
    grid
}

/// The page's `LSD` token, which authorizes logged-out GraphQL requests.
pub(crate) fn lsd_token(bytes: &[u8]) -> Option<String> {
    let page = std::str::from_utf8(bytes).ok()?;
    let start = page.find(r#"["LSD",[],{"token":""#)? + r#"["LSD",[],{"token":""#.len();
    let len = page[start..].find('"')?;
    Some(&page[start..start + len])
        .filter(|token| {
            !token.is_empty()
                && token.len() <= 128
                && token
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        })
        .map(str::to_owned)
}

/// A missing navigation field is an incomplete response, not an empty album.
pub(crate) fn parse_navigation(bytes: &[u8], id: &str, set: &str) -> Result<Option<String>> {
    validate_id(id)?;
    validate_set(set)?;
    let payloads = crate::facebook::embedded_payloads(bytes)?;
    let mut candidates = Vec::new();
    let mut malformed = false;
    for payload in &payloads {
        visit(payload, &mut |value| {
            if photo_identifier(value) != Some(id) || !own_url_matches(value, id, set) {
                return;
            }
            if let Some(next) = value.get("nextMediaAfterNodeId") {
                if next.is_null() {
                    candidates.push(None);
                } else if let Some(next_id) = photo_identifier(next) {
                    candidates.push(Some(next_id.to_owned()));
                } else {
                    malformed = true;
                }
            }
        });
    }
    if malformed {
        return Err(error("Facebook album returned invalid photo navigation"));
    }
    candidates.sort();
    candidates.dedup();
    match candidates.as_slice() {
        [next] => Ok(next.clone()),
        [] => Err(error(
            "Facebook album photo is missing identity-bound navigation; login may be required",
        )),
        _ => Err(error("Facebook album photo navigation is ambiguous")),
    }
}

fn photo_identifier(value: &Value) -> Option<&str> {
    if value.get("__typename").and_then(Value::as_str) != Some("Photo")
        && value.get("__isNode").and_then(Value::as_str) != Some("Photo")
    {
        return None;
    }
    value
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| validate_id(id).is_ok())
}

fn bound_set(value: &Value, set: &str) -> bool {
    let mut present = false;
    for key in ["mediaSetToken", "mediasetToken"] {
        if let Some(token) = value.get(key) {
            present = true;
            if token.as_str() != Some(set) {
                return false;
            }
        }
    }
    present
}

fn own_url_matches(value: &Value, id: &str, set: &str) -> bool {
    let Some(raw) = value.get("url").and_then(Value::as_str) else {
        return false;
    };
    let Ok(url) = Url::parse(raw) else {
        return false;
    };
    if !matches!(url.scheme(), "http" | "https")
        || !matches!(
            url.host_str(),
            Some("facebook.com" | "www.facebook.com" | "m.facebook.com")
        )
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || !matches!(url.path().trim_end_matches('/'), "/photo" | "/photo.php")
    {
        return false;
    }
    let ids: Vec<_> = url
        .query_pairs()
        .filter(|(k, _)| k == "fbid")
        .map(|(_, v)| v.into_owned())
        .collect();
    let sets: Vec<_> = url
        .query_pairs()
        .filter(|(k, _)| k == "set")
        .map(|(_, v)| v.into_owned())
        .collect();
    ids == [id] && sets == [set]
}

pub(crate) fn visit(value: &Value, f: &mut impl FnMut(&Value)) {
    f(value);
    match value {
        Value::Object(object) => {
            for child in object.values() {
                visit(child, f);
            }
        }
        Value::Array(array) => {
            for child in array {
                visit(child, f);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn bytes(value: Value) -> Vec<u8> {
        serde_json::to_vec(&value).unwrap()
    }
    fn photo(id: &str, set: &str, next: Value) -> Value {
        json!({"__typename":"Photo","id":id,"url":format!("https://www.facebook.com/photo/?fbid={id}&set={set}"),"nextMediaAfterNodeId":next})
    }
    #[test]
    fn album_inputs_distinguish_root_resume_and_single_photo() {
        for path in ["/media/set/?set=a.123", "/photo/?set=pcb.456"] {
            let parsed =
                album_input(&Url::parse(&format!("https://www.facebook.com{path}")).unwrap())
                    .unwrap();
            assert!(parsed.start.is_none());
        }
        let parsed = album_input(
            &Url::parse("https://m.facebook.com/photo/?fbid=456&set=a.123&setextract").unwrap(),
        )
        .unwrap();
        assert_eq!(parsed.start.as_deref(), Some("456"));
        assert_eq!(parsed.set, "a.123");
        for raw in [
            "https://www.facebook.com/photo/?fbid=456&set=a.123",
            "https://www.facebook.com/media/set/?set=a.123&set=a.124",
            "https://www.facebook.com/media/set/?set=a.%2F123",
            "https://www.facebook.com/photo/?fbid=0&set=a.123&setextract",
            "https://evil.invalid/media/set/?set=a.123",
            "https://user@facebook.com/media/set/?set=a.123",
        ] {
            assert!(album_input(&Url::parse(raw).unwrap()).is_err(), "{raw}");
        }
    }
    #[test]
    fn seed_uses_ordered_bound_collection_not_first_global_photo() {
        assert_eq!(
            parse_seed(
                include_bytes!("../tests/fixtures/facebook/album_seed.json"),
                "a.123"
            )
            .unwrap(),
            "456"
        );
        assert!(
            parse_seed(
                &bytes(
                    json!({"mediaSetToken":"a.123","unrelated":{"__typename":"Photo","id":"999"}})
                ),
                "a.123"
            )
            .is_err()
        );
    }
    #[test]
    fn membership_cannot_establish_first_position_and_conflicting_seeds_fail() {
        let one = photo("456", "a.123", Value::Null);
        assert!(parse_seed(&bytes(one.clone()), "a.123").is_err());
        assert!(
            parse_seed(
                &bytes(json!([one, photo("457", "a.123", Value::Null)])),
                "a.123"
            )
            .is_err()
        );
        let albums = json!([
            {"mediaSetToken":"a.123","photos":{"edges":[{"node":{"__typename":"Photo","id":"456"}}]}},
            {"mediasetToken":"a.123","pageItems":{"edges":[{"node":{"__typename":"Photo","id":"457"}}]}}
        ]);
        assert!(parse_seed(&bytes(albums), "a.123").is_err());
        for alias in [json!("a.124"), Value::Null, json!(123)] {
            let album = json!({"mediaSetToken":"a.123","mediasetToken":alias,"photos":{"edges":[{"node":{"__typename":"Photo","id":"456"}}]}});
            assert!(parse_seed(&bytes(album), "a.123").is_err());
        }
    }

    #[test]
    fn photo_urls_cannot_forge_membership_with_duplicate_or_foreign_parameters() {
        for raw in [
            "https://www.facebook.com/photo/?fbid=456&set=a.123&set=a.124",
            "https://www.facebook.com/photo/?fbid=456&fbid=457&set=a.123",
            "https://facebook.com.evil.invalid/photo/?fbid=456&set=a.123",
            "https://user@facebook.com/photo/?fbid=456&set=a.123",
        ] {
            let mut value = photo("456", "a.123", Value::Null);
            value["url"] = raw.into();
            assert!(
                parse_navigation(&bytes(value), "456", "a.123").is_err(),
                "{raw}"
            );
        }
    }

    #[test]
    fn navigation_requires_photo_and_album_identity() {
        let value: Value = serde_json::from_slice(include_bytes!(
            "../tests/fixtures/facebook/album_photo.json"
        ))
        .unwrap();
        assert_eq!(
            parse_navigation(&bytes(value.clone()), "456", "a.123").unwrap(),
            Some("457".into())
        );
        assert!(parse_navigation(&bytes(value.clone()), "456", "a.124").is_err());
        assert!(parse_navigation(&bytes(value), "999", "a.123").is_err());
        assert_eq!(
            parse_navigation(&bytes(photo("456", "a.123", Value::Null)), "456", "a.123").unwrap(),
            None
        );
        assert!(parse_navigation(&bytes(json!({"photo":{"__typename":"Photo","id":"456","url":"https://www.facebook.com/photo/?fbid=456&set=a.123"},"unrelated":{"nextMediaAfterNodeId":{"__typename":"Photo","id":"457"}}})),"456","a.123").is_err());
    }
    #[test]
    fn ambiguous_or_malformed_navigation_fails_closed() {
        assert!(
            parse_navigation(
                &bytes(json!([
                    photo("456", "a.123", Value::Null),
                    photo("456", "a.123", json!({"__typename":"Photo","id":"457"}))
                ])),
                "456",
                "a.123"
            )
            .is_err()
        );
        for next in [
            json!({"__typename":"Video","id":"457"}),
            json!({"__typename":"Photo","id":"0"}),
            json!("457"),
        ] {
            assert!(parse_navigation(&bytes(photo("456", "a.123", next)), "456", "a.123").is_err());
        }
    }
    #[test]
    fn html_json_scripts_are_supported_but_not_javascript_or_preview_tags() {
        let body = format!(
            "<meta property='og:image' content='https://example.com/x.jpg'><script>window.data={{}}</script><script type='application/json'>{}</script>",
            photo("456", "a.123", Value::Null)
        );
        assert_eq!(
            parse_navigation(body.as_bytes(), "456", "a.123").unwrap(),
            None
        );
        assert!(parse_seed(b"<html>login shell</html>", "a.123").is_err());
    }
}
