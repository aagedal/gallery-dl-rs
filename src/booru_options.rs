//! Upstream `metadata` (Danbooru/e621) and `tags`/`notes` (Gelbooru and
//! Safebooru) options: extra requests merged into raw API posts before they
//! are emitted, so their fields become ordinary post keywords.
use super::*;
use serde_json::Map;

/// Bound on cached e621 pool lookups.
const MAX_POOL_CACHE: usize = 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Enrichment {
    /// Danbooru `only=` field list, ending in `,id` like upstream.
    pub includes: Option<String>,
    /// e621 `notes`/`pools` metadata.
    pub notes: bool,
    pub pools: bool,
    /// Gelbooru/Safebooru post-page `tags_<type>` and `notes`.
    pub html_tags: bool,
    pub html_notes: bool,
}

impl Enrichment {
    /// Interpret upstream's options with Python truthiness.
    pub(super) fn parse(site: Site, options: &Map<String, Value>) -> Result<Self> {
        let truthy = |key: &str| options.get(key).is_some_and(crate::postprocess::truthy);
        let mut enrichment = Self::default();
        match site {
            Site::Danbooru => {
                if let Some(value) = options
                    .get("metadata")
                    .filter(|v| crate::postprocess::truthy(v))
                {
                    let includes = match value {
                        Value::Array(items) => items
                            .iter()
                            .map(|item| {
                                item.as_str().ok_or_else(|| {
                                    error("extractor.danbooru.metadata entries must be strings")
                                })
                            })
                            .collect::<Result<Vec<_>>>()?
                            .join(","),
                        Value::String(text) => text.clone(),
                        _ => "artist_commentary,children,notes,parent,uploader".into(),
                    };
                    if includes.len() > 4096 || includes.chars().any(char::is_control) {
                        return Err(error("extractor.danbooru.metadata is invalid"));
                    }
                    enrichment.includes = Some(format!("{includes},id"));
                }
            }
            Site::E621 => {
                let includes: Vec<String> = match options.get("metadata") {
                    Some(value) if crate::postprocess::truthy(value) => match value {
                        Value::String(text) => text.split(',').map(str::to_owned).collect(),
                        Value::Array(items) => items
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect(),
                        _ => vec!["notes".into(), "pools".into()],
                    },
                    _ => Vec::new(),
                };
                enrichment.notes = includes.iter().any(|item| item == "notes");
                enrichment.pools = includes.iter().any(|item| item == "pools");
            }
            Site::Gelbooru | Site::Safebooru => {
                enrichment.html_tags = truthy("tags");
                enrichment.html_notes = truthy("notes");
            }
        }
        Ok(enrichment)
    }

    pub(super) fn any(&self) -> bool {
        *self != Self::default()
    }
}

fn post_id(post: &Value) -> Option<String> {
    number(post.get("id")).map(|id| id.to_string())
}

fn json_body(site: Site, bytes: &[u8]) -> Result<Value> {
    serde_json::from_slice(bytes).map_err(|_| invalid(site))
}

/// Bound on ugoira frames per post.
const MAX_FRAMES: usize = 10_000;

/// upstream `_ugoira_frames`: `media_metadata.metadata` frame delays named
/// `{index:>06}.{ext}`, with the extension from the frame MIME type
/// (`jpeg` becomes `jpg`) or else the ZIP file name.
pub(super) fn ugoira_frame_data(body: &Value) -> Option<Value> {
    let data = body.get("media_metadata")?.get("metadata")?;
    let ext = match data.get("Ugoira:FrameMimeType") {
        Some(mime) => mime.as_str()?.rsplit('/').next()?,
        None => data.get("ZIP:ZipFileName")?.as_str()?.rsplit('.').next()?,
    };
    let ext = if ext == "jpeg" { "jpg" } else { ext };
    if ext.len() > 16 || !ext.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return None;
    }
    let delays = data.get("Ugoira:FrameDelays")?.as_array()?;
    if delays.len() > MAX_FRAMES {
        return None;
    }
    Some(Value::Array(
        delays
            .iter()
            .enumerate()
            .map(|(index, delay)| {
                serde_json::json!({"file": format!("{index:06}.{ext}"), "delay": delay})
            })
            .collect(),
    ))
}

/// Typed frames of `ugoira_frame_data`; `None` for malformed values.
pub(super) fn parse_frames(value: &Value) -> Option<Arc<[crate::pixiv::UgoiraFrame]>> {
    value
        .as_array()?
        .iter()
        .map(|frame| {
            let delay = frame.get("delay")?;
            Some(crate::pixiv::UgoiraFrame {
                file: frame.get("file")?.as_str()?.to_owned(),
                delay: delay
                    .as_u64()
                    .or_else(|| delay.as_f64().filter(|d| *d >= 0.0).map(|d| d as u64))?,
            })
        })
        .collect()
}

/// Merge every field of `extra` into `post` (Python `dict.update`).
fn update(post: &mut Value, extra: Value) {
    if let (Value::Object(post), Value::Object(extra)) = (post, extra) {
        post.extend(extra);
    }
}

/// gelbooru_v02 `_tags`: `tags_<type>` space-joined names from the sidebar.
pub(super) fn html_tags(page: &str) -> Map<String, Value> {
    let container = ["<ul id=\"tag-", "<ul class=\"tag-"]
        .iter()
        .find_map(|start| {
            let at = page.find(start)? + start.len();
            let end = page[at..].find("</ul>")?;
            Some(&page[at..at + end]).filter(|text| !text.is_empty())
        })
        .unwrap_or_default();
    // `(?s)tag-type-([^"' ]+).*?[?;]tags=([^"'&]+)`
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    let mut rest = container;
    while let Some(at) = rest.find("tag-type-") {
        let after = &rest[at + "tag-type-".len()..];
        let kind_len = after.find(['"', '\'', ' ']).unwrap_or(after.len());
        if kind_len == 0 {
            rest = after;
            continue;
        }
        let kind = &after[..kind_len];
        let tail = &after[kind_len..];
        let Some(found) = tail
            .match_indices("tags=")
            .find(|(index, _)| *index > 0 && matches!(tail.as_bytes()[index - 1], b'?' | b';'))
        else {
            break;
        };
        let value = &tail[found.0 + 5..];
        let len = value.find(['"', '\'', '&']).unwrap_or(value.len());
        if len == 0 {
            rest = value;
            continue;
        }
        let name = crate::sites::unescape(&crate::sites::percent_decode(&value[..len]));
        match groups.iter_mut().find(|(key, _)| key == kind) {
            Some((_, names)) => names.push(name),
            None => groups.push((kind.to_owned(), vec![name])),
        }
        rest = &value[len..];
    }
    groups
        .into_iter()
        .map(|(kind, names)| (format!("tags_{kind}"), names.join(" ").into()))
        .collect()
}

/// Python `int()` of a trimmed decimal string.
fn int(text: &str) -> Option<Value> {
    text.trim().parse::<i64>().ok().map(Value::from)
}

/// gelbooru `_notes`: `<section id="notes">` articles with data attributes.
pub(super) fn gelbooru_notes(page: &str) -> Option<Vec<Value>> {
    let section = between(page, "<section id=\"notes\"", "</section>")?;
    let mut notes = Vec::new();
    let mut rest = section;
    while let Some(at) = rest.find("<article") {
        let after = &rest[at + "<article".len()..];
        let Some(end) = after.find("</article>") else {
            break;
        };
        let note = &after[..end];
        rest = &after[end..];
        let attribute = |name: &str| between(note, &format!("data-{name}=\""), "\"");
        let parsed = (|| {
            Some(Value::Object(Map::from_iter([
                ("width".into(), int(attribute("width")?)?),
                ("height".into(), int(attribute("height")?)?),
                ("x".into(), int(attribute("x")?)?),
                ("y".into(), int(attribute("y")?)?),
                ("body".into(), attribute("body")?.into()),
            ])))
        })();
        notes.extend(parsed);
    }
    Some(notes)
}

/// gelbooru_v02 `_notes`: `note-box` elements of `id="note-container"`.
pub(super) fn v02_notes(page: &str) -> Option<Vec<Value>> {
    let container = between(page, "id=\"note-container\"", "<img ")?;
    let mut notes = Vec::new();
    for note in container.split("class=\"note-box\"").skip(1) {
        // `text.extract_from`: each field continues after the previous one.
        let mut position = 0;
        let mut next = |start: &str, end: &str| {
            let from = position + note[position..].find(start)? + start.len();
            let to = from + note[from..].find(end)?;
            position = to + end.len();
            Some(&note[from..to])
        };
        let parsed = (|| {
            let width = int(next("width:", "p")?)?;
            let height = int(next("height:", "p")?)?;
            let y = int(next("top:", "p")?)?;
            let x = int(next("left:", "p")?)?;
            let id = int(next("id=\"note-body-", "\"")?)?;
            let body = next(">", "</div>")?;
            Some(Value::Object(Map::from_iter([
                ("width".into(), width),
                ("height".into(), height),
                ("y".into(), y),
                ("x".into(), x),
                ("id".into(), id),
                (
                    "body".into(),
                    crate::sites::unescape(&crate::sites::remove_html(body)).into(),
                ),
            ])))
        })();
        notes.extend(parsed);
    }
    Some(notes)
}

fn between<'a>(text: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let rest = &text[text.find(start)? + start.len()..];
    Some(&rest[..rest.find(end)?])
}

impl Booru {
    /// Apply upstream option requests to one page (or single post) of raw
    /// posts. `endpoint` is the listing's API path; `single` marks a post
    /// detail response, which Danbooru enriches with its own `only=` request.
    /// Danbooru ugoira frame data follows, as upstream requests it while
    /// emitting each post after the page's `only=` merge.
    pub(super) async fn enrich(
        &self,
        root: &Url,
        endpoint: &str,
        single: bool,
        raw: &mut [Value],
    ) -> Result<()> {
        self.enrich_options(root, endpoint, single, raw).await?;
        self.ugoira_frames(root, raw).await
    }

    /// Danbooru `ugoira`: attach `/posts/ID.json?only=media_metadata` frame
    /// names and delays to each ugoira ZIP post (upstream `_ugoira_frames`).
    /// Missing or malformed frame metadata ends extraction like upstream.
    pub(super) async fn ugoira_frames(&self, root: &Url, raw: &mut [Value]) -> Result<()> {
        if !self.ugoira {
            return Ok(());
        }
        for post in raw.iter_mut() {
            let zip =
                string(post, "file_url").is_some_and(|url| source_nameext(url).1.as_ref() == "zip");
            let Some(id) = post_id(post).filter(|_| zip) else {
                continue;
            };
            let body = self
                .call(
                    root,
                    &format!("posts/{id}.json"),
                    &[("only", "media_metadata")],
                )
                .await?;
            let frames = ugoira_frame_data(&json_body(self.site, &body)?)
                .ok_or_else(|| error("Danbooru returned invalid ugoira metadata"))?;
            if let Value::Object(post) = post {
                post.insert(UGOIRA_FRAME_DATA.into(), frames);
            }
        }
        Ok(())
    }

    async fn enrich_options(
        &self,
        root: &Url,
        endpoint: &str,
        single: bool,
        raw: &mut [Value],
    ) -> Result<()> {
        if !self.enrichment.any() || raw.is_empty() {
            return Ok(());
        }
        match self.site {
            Site::Danbooru => {
                let Some(includes) = &self.enrichment.includes else {
                    return Ok(());
                };
                if single {
                    for post in raw.iter_mut() {
                        let body = self.call(root, endpoint, &[("only", includes)]).await?;
                        update(post, json_body(self.site, &body)?);
                    }
                    return Ok(());
                }
                let ids: Vec<String> = raw.iter().filter_map(post_id).collect();
                let limit = raw.len().to_string();
                let tags = format!("id:{}", ids.join(","));
                let body = self
                    .call(
                        root,
                        endpoint,
                        &[("only", includes), ("limit", &limit), ("tags", &tags)],
                    )
                    .await?;
                let rows = match json_body(self.site, &body)? {
                    Value::Array(rows) => rows,
                    Value::Object(mut body) => match body.remove("posts") {
                        Some(Value::Array(rows)) => rows,
                        _ => return Err(invalid(self.site)),
                    },
                    _ => return Err(invalid(self.site)),
                };
                let mut by_id: HashMap<String, Value> = rows
                    .into_iter()
                    .filter_map(|row| Some((post_id(&row)?, row)))
                    .collect();
                for post in raw.iter_mut() {
                    if let Some(extra) = post_id(post).and_then(|id| by_id.remove(&id)) {
                        update(post, extra);
                    }
                }
            }
            Site::E621 => {
                for post in raw.iter_mut() {
                    let Some(id) = post_id(post) else {
                        continue;
                    };
                    if self.enrichment.notes
                        && post
                            .get("has_notes")
                            .is_some_and(crate::postprocess::truthy)
                    {
                        let body = self
                            .call(root, "notes.json", &[("search[post_id]", &id)])
                            .await?;
                        post["notes"] = json_body(self.site, &body)?;
                    }
                    if self.enrichment.pools
                        && let Some(pools) =
                            post.get("pools").filter(|v| crate::postprocess::truthy(v))
                    {
                        let ids = match pools {
                            Value::Array(ids) => ids
                                .iter()
                                .map(|id| match id {
                                    Value::String(text) => text.clone(),
                                    other => other.to_string(),
                                })
                                .collect::<Vec<_>>()
                                .join(","),
                            _ => continue,
                        };
                        post["pools"] = self.e621_pools(root, ids).await?;
                    }
                }
            }
            Site::Gelbooru | Site::Safebooru => {
                for post in raw.iter_mut() {
                    // Upstream fetches the page only for posts with a file URL.
                    let has_file = post
                        .get("file_url")
                        .and_then(Value::as_str)
                        .is_some_and(|url| !url.is_empty());
                    let Some(id) = post_id(post).filter(|_| has_file) else {
                        continue;
                    };
                    let body = self
                        .call_kind(
                            root,
                            "index.php",
                            &[("page", "post"), ("s", "view"), ("id", &id)],
                            RequestKind::Html,
                        )
                        .await?;
                    let page = String::from_utf8_lossy(&body);
                    let Value::Object(fields) = post else {
                        continue;
                    };
                    if self.enrichment.html_tags {
                        fields.extend(html_tags(&page));
                    }
                    if self.enrichment.html_notes {
                        let notes = if self.site == Site::Gelbooru {
                            gelbooru_notes(&page)
                        } else {
                            v02_notes(&page)
                        };
                        if let Some(notes) = notes {
                            fields.insert("notes".into(), Value::Array(notes));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// e621 `_get_pools`, cached by the joined pool IDs.
    async fn e621_pools(&self, root: &Url, ids: String) -> Result<Value> {
        if let Some(pools) = self.pool_cache.lock().await.get(&ids) {
            return Ok(pools.clone());
        }
        let body = self
            .call(root, "pools.json", &[("search[id]", &ids)])
            .await?;
        let mut pools = json_body(self.site, &body)?;
        if let Value::Array(items) = &mut pools {
            for pool in items {
                if let Some(name) = pool.get("name").and_then(Value::as_str) {
                    let name = name.replace('_', " ");
                    pool["name"] = name.into();
                }
            }
        }
        let mut cache = self.pool_cache.lock().await;
        if cache.len() >= MAX_POOL_CACHE {
            cache.clear();
        }
        cache.insert(ids, pools.clone());
        Ok(pools)
    }
}
