//! HTML collection discovery and lazy DAPI member requests.
use super::*;

pub(super) struct DapiListing {
    pub value: Arc<Value>,
    pub name: Value,
    pub favorite: bool,
    id: String,
    ids: VecDeque<String>,
    offset: usize,
    done: bool,
}
pub(super) fn capture_favorites(site: Site, url: &Url) -> Option<String> {
    if site != Site::Safebooru || url.path() != "/index.php" {
        return None;
    }
    let rest = url.query()?.strip_prefix("page=favorites&s=view&id=")?;
    let id: String = rest
        .chars()
        .take_while(|&c| crate::python_unicode::decimal_digit(c).is_some())
        .collect();
    (!id.is_empty()).then_some(id)
}
fn scalar_id(id: &str) -> Result<Arc<Value>> {
    let digits = id
        .chars()
        .map(|c| crate::python_unicode::decimal_digit(c).map(|n| char::from(b'0' + n)))
        .collect::<Option<String>>()
        .ok_or_else(|| error("invalid booru collection identifier"))?;
    let digits = digits.trim_start_matches('0');
    serde_json::from_str(if digits.is_empty() { "0" } else { digits })
        .map(Arc::new)
        .map_err(|_| error("invalid booru collection identifier"))
}
pub(super) fn favorites(id: String) -> Result<Box<DapiListing>> {
    Ok(Box::new(DapiListing {
        value: scalar_id(&id)?,
        name: Value::Null,
        favorite: true,
        id,
        ids: VecDeque::new(),
        offset: 0,
        done: false,
    }))
}
fn favorite_ids(text: &str) -> VecDeque<String> {
    let mut ids = VecDeque::new();
    let mut text = text;
    while let Some((before, after)) = text.split_once("id=\"p") {
        if before
            .chars()
            .next_back()
            .is_some_and(crate::python_unicode::whitespace)
        {
            let count = after
                .chars()
                .take_while(|&c| crate::python_unicode::decimal_digit(c).is_some())
                .map(char::len_utf8)
                .sum::<usize>();
            if count != 0 {
                ids.push_back(after[..count].to_owned());
            }
        }
        text = after;
    }
    ids
}
pub(super) fn capture(site: Site, url: &Url) -> Option<String> {
    if site.danbooru_dialect()
        || !matches!(url.path(), "/index.php" | "/")
        || site == Site::Safebooru && url.path() != "/index.php"
    {
        return None;
    }
    let rest = url.query()?.strip_prefix("page=pool&s=show&id=")?;
    let count = rest.bytes().take_while(u8::is_ascii_digit).count();
    (count != 0).then(|| rest[..count].to_owned())
}
fn extract<'a>(text: &'a str, begin: &str, end: &str) -> Option<(&'a str, &'a str)> {
    let (_, rest) = text.split_once(begin)?;
    rest.split_once(end)
}
fn ids(mut text: &str, marker: &str) -> VecDeque<String> {
    let mut ids = VecDeque::new();
    while let Some((id, rest)) = extract(text, marker, "\"") {
        ids.push_back(id.to_owned());
        text = rest;
    }
    ids
}
pub(super) async fn start(client: &Booru, root: &Url, id: String) -> Result<Box<DapiListing>> {
    let body = client
        .call_kind(
            root,
            "index.php",
            &[("page", "pool"), ("s", "show"), ("id", &id)],
            RequestKind::Html,
        )
        .await?;
    let html = String::from_utf8_lossy(&body);
    let (begin, end) = if client.site == Site::Gelbooru {
        ("<h3>Now Viewing: ", "</h3>")
    } else {
        ("<h4>Pool: ", "</h4>")
    };
    let (name, tail) = extract(&html, begin, end)
        .filter(|(name, _)| !name.is_empty())
        .ok_or_else(|| error("booru pool not found"))?;
    Ok(Box::new(DapiListing {
        value: scalar_id(&id)?,
        name: crate::pixiv::pixivision::unescape(name).into(),
        favorite: false,
        id,
        ids: if client.site == Site::Safebooru {
            ids(tail, "class=\"thumb\" id=\"p")
        } else {
            VecDeque::new()
        },
        offset: 0,
        done: client.site == Site::Safebooru,
    }))
}
pub(super) async fn next(client: &Booru, state: &mut State) -> Result<()> {
    let mut listing = state.dapi_listing.take().expect("DAPI collection cursor");
    let result = async {
        loop {
            if let Some(id) = listing.ids.pop_front() {
                let (query, kind) = if client.site == Site::Gelbooru {
                    (
                        vec![
                            ("page", "dapi"),
                            ("q", "index"),
                            ("json", "1"),
                            ("id", id.as_str()),
                            ("s", "post"),
                        ],
                        RequestKind::Api,
                    )
                } else {
                    (
                        vec![
                            ("page", "dapi"),
                            ("s", "post"),
                            ("q", "index"),
                            ("id", id.as_str()),
                        ],
                        RequestKind::Xml,
                    )
                };
                let body = client
                    .call_kind(&state.root, "index.php", &query, kind)
                    .await?;
                return if client.site == Site::Safebooru {
                    xml_posts(&body).map(Some)
                } else {
                    posts(client.site, &body).map(Some)
                };
            }
            if listing.done {
                return Ok(None);
            }
            let offset = listing.offset.to_string();
            let body = client
                .call_kind(
                    &state.root,
                    "index.php",
                    &[
                        (
                            "page",
                            if listing.favorite {
                                "favorites"
                            } else {
                                "pool"
                            },
                        ),
                        ("s", if listing.favorite { "view" } else { "show" }),
                        ("id", &listing.id),
                        ("pid", &offset),
                    ],
                    RequestKind::Html,
                )
                .await?;
            let html = String::from_utf8_lossy(&body);
            listing.ids = if listing.favorite {
                favorite_ids(&html)
            } else {
                ids(&html, "\" id=\"p")
            };
            let per_page = if listing.favorite { 50 } else { 45 };
            listing.done = listing.ids.len() < per_page;
            listing.offset = listing
                .offset
                .checked_add(per_page)
                .ok_or_else(|| error("booru collection offset overflow"))?;
        }
    }
    .await;
    state.dapi_listing = Some(listing);
    match result? {
        Some(raw) => client.emit(state, &raw),
        None => state.done = true,
    }
    Ok(())
}
fn xml_posts(bytes: &[u8]) -> Result<Vec<Value>> {
    let xml = std::str::from_utf8(bytes).map_err(|_| error("invalid booru DAPI XML encoding"))?;
    let document = roxmltree::Document::parse_with_options(
        xml,
        roxmltree::ParsingOptions {
            allow_dtd: false,
            nodes_limit: 262_144,
            ..Default::default()
        },
    )
    .map_err(|_| error("invalid or excessive booru DAPI XML"))?;
    let root = document.root_element();
    if root.tag_name().name() == "error" {
        return Err(error("booru DAPI refused extraction"));
    }
    Ok(root
        .children()
        .filter(roxmltree::Node::is_element)
        .map(|node| {
            Value::Object(
                node.attributes()
                    .map(|attr| (attr.name().to_owned(), attr.value().into()))
                    .collect(),
            )
        })
        .collect())
}
pub(super) struct PreparedPost {
    pub post: Value,
    pub fallback: Option<Value>,
}
pub(super) fn prepare(site: Site, post: &Value) -> Result<Option<PreparedPost>> {
    if !post.is_object() {
        return Err(error("invalid booru DAPI post"));
    }
    let Some(raw) = post
        .get("file_url")
        .and_then(Value::as_str)
        .filter(|url| !url.is_empty())
    else {
        return Ok(None);
    };
    let mut url = raw.to_owned();
    let fallback = if site == Site::Gelbooru && (raw.ends_with(".webm") || raw.ends_with(".mp4")) {
        let Some(md5) = post.get("md5").and_then(Value::as_str) else {
            return Ok(None);
        };
        let Some(preview) = post
            .get("preview_url")
            .and_then(Value::as_str)
            .and_then(|url| Url::parse(url).ok())
        else {
            return Ok(None);
        };
        let prefix: String = md5.chars().take(2).collect();
        let second: String = md5.chars().skip(2).take(2).collect();
        url = format!(
            "{}/images/{prefix}/{second}/{md5}.webm",
            preview.origin().ascii_serialization()
        );
        Some(Value::Array(vec![raw.into()]))
    } else {
        None
    };
    if url.starts_with('/') {
        url = format!(
            "https://{}{}",
            if site == Site::Gelbooru {
                "gelbooru.com"
            } else {
                "safebooru.org"
            },
            url
        );
    }
    let mut post = post.clone();
    post["file_url"] = url.into();
    {
        let tags = post
            .get("tags")
            .and_then(Value::as_str)
            .ok_or_else(|| error("invalid booru DAPI tags"))?;
        let tags = tags
            .trim_matches(crate::python_unicode::whitespace)
            .to_owned();
        if post.get("created_at").is_none() {
            return Err(error("booru DAPI post date is missing"));
        }
        post["tags"] = tags.into();
    }
    Ok(Some(PreparedPost { post, fallback }))
}
