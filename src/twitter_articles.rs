//! Article files and bounded, escaped rendering of explicitly owned article content.
// Native port of gallery-dl's twitter_article.py; Copyright 2026 Mike Fährmann.
// SPDX-License-Identifier: GPL-2.0-or-later
// https://github.com/mikf/gallery-dl/blob/master/gallery_dl/extractor/utils/twitter_article.py
use super::*;
use crate::extract::{MediaJob, inline_text};
use media::SelectedFile;
use serde::Serialize;
use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArticleOptions {
    pub metadata: bool,
    pub cover: bool,
    pub media: bool,
    pub html: bool,
    pub document: bool,
}
impl Default for ArticleOptions {
    fn default() -> Self {
        Self {
            metadata: true,
            cover: true,
            media: true,
            html: false,
            document: false,
        }
    }
}
impl ArticleOptions {
    pub fn disabled() -> Self {
        Self {
            metadata: false,
            cover: false,
            media: false,
            html: false,
            document: false,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ArticleMetadata {
    pub id: Option<String>,
    pub title: Option<Arc<str>>,
    pub date: Option<i64>,
    pub date_updated: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub html: Option<Arc<str>>,
}

pub(super) fn select(
    tweet: &Value,
    options: ArticleOptions,
) -> (Vec<SelectedFile>, Option<ArticleMetadata>) {
    let mut files = Vec::new();
    let mut metadata = None;
    // Match the upstream article boundary: malformed optional article data must
    // not discard ordinary files or article work completed before the failure.
    let _ = select_into(tweet, options, &mut files, &mut metadata);
    (files, metadata)
}

fn select_into(
    tweet: &Value,
    options: ArticleOptions,
    files: &mut Vec<SelectedFile>,
    metadata: &mut Option<ArticleMetadata>,
) -> Result<()> {
    if options == ArticleOptions::disabled() || tweet.get("article").is_none_or(Value::is_null) {
        return Ok(());
    }
    let article = tweet
        .pointer("/article/article_results/result")
        .filter(|value| value.is_object())
        .ok_or_else(|| error("Twitter article result missing or invalid"))?;
    if options.metadata {
        let id = optional_string(article.get("rest_id"), "Twitter article identifier invalid")?;
        if id.as_deref().is_some_and(|id| !valid_id(id)) {
            return Err(error("Twitter article identifier invalid"));
        }
        *metadata = Some(ArticleMetadata {
            id,
            title: optional_string(article.get("title"), "Twitter article title invalid")?
                .map(Arc::from),
            date: optional_timestamp(article.get("metadata"), "first_published_at_secs")?,
            date_updated: optional_timestamp(article.get("lifecycle_state"), "modified_at_secs")?,
            html: None,
        });
    }
    let html = if options.html || options.document {
        Some(Arc::<str>::from(render(article)?))
    } else {
        None
    };
    if options.html {
        metadata
            .get_or_insert(ArticleMetadata {
                id: None,
                title: None,
                date: None,
                date_updated: None,
                html: None,
            })
            .html = html.clone();
    }
    if options.document {
        let document = document(article, html.as_deref().unwrap())?;
        let (url, job) = inline_text(document)?;
        files.push((url, "html".into(), job));
    }
    if options.cover
        && let Some(cover) = article.get("cover_media").filter(|value| !value.is_null())
    {
        files.push(image(cover)?);
    }
    if options.media {
        for media in article
            .get("media_entities")
            .and_then(Value::as_array)
            .ok_or_else(|| error("Twitter article media list missing or invalid"))?
        {
            files.push(image(media)?);
        }
    }
    Ok(())
}

fn optional_string(value: Option<&Value>, message: &'static str) -> Result<Option<String>> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        _ => Err(error(message)),
    }
}
fn optional_timestamp(parent: Option<&Value>, key: &str) -> Result<Option<i64>> {
    let parent = match parent {
        None | Some(Value::Null) => return Ok(None),
        Some(parent) if parent.is_object() => parent,
        _ => return Err(error("Twitter article timestamp fields invalid")),
    };
    match parent.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(value)) => value
            .as_i64()
            .filter(|value| *value >= 0)
            .map(Some)
            .ok_or_else(|| error("Twitter article timestamp invalid")),
        Some(Value::String(value))
            if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            value
                .parse::<i64>()
                .map(Some)
                .map_err(|_| error("Twitter article timestamp invalid"))
        }
        _ => Err(error("Twitter article timestamp invalid")),
    }
}

fn image(value: &Value) -> Result<SelectedFile> {
    let source = value
        .pointer("/media_info/original_img_url")
        .and_then(Value::as_str)
        .ok_or_else(|| error("Twitter article image URL missing or invalid"))?;
    let mut url = http_url(source).map_err(|_| error("Twitter article image URL invalid"))?;
    let supported =
        |extension: &str| matches!(extension, "jpg" | "jpeg" | "png" | "webp" | "avif" | "gif");
    let extension = url
        .path()
        .rsplit_once('.')
        .map(|(_, extension)| extension)
        .filter(|extension| supported(extension))
        .map(str::to_owned)
        .or_else(|| {
            url.query_pairs()
                .find(|(key, value)| key == "format" && supported(value))
                .map(|(_, value)| value.into_owned())
        })
        .ok_or_else(|| error("Twitter article image format unsupported"))?;
    let pairs: Vec<_> = url
        .query_pairs()
        .filter(|(key, _)| key != "name")
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    url.set_query(None);
    url.query_pairs_mut()
        .extend_pairs(pairs)
        .append_pair("name", "orig");
    Ok((url, extension, MediaJob::DirectHttp))
}

#[derive(Default)]
struct Html(String);
impl Html {
    fn push(&mut self, text: &str) -> Result<()> {
        if self
            .0
            .len()
            .checked_add(text.len())
            .is_none_or(|size| size > MAX_RESPONSE)
        {
            return Err(error("Twitter article rendered output exceeded 16 MiB"));
        }
        self.0.push_str(text);
        Ok(())
    }
    fn escape(&mut self, text: &str) -> Result<()> {
        for part in text.split_inclusive(['&', '<', '>', '"', '\'']) {
            if let Some(last) = part.chars().last() {
                let replacement = match last {
                    '&' => Some("&amp;"),
                    '<' => Some("&lt;"),
                    '>' => Some("&gt;"),
                    '"' => Some("&quot;"),
                    '\'' => Some("&#39;"),
                    _ => None,
                };
                if let Some(replacement) = replacement {
                    self.push(&part[..part.len() - last.len_utf8()])?;
                    self.push(replacement)?;
                    continue;
                }
            }
            self.push(part)?;
        }
        Ok(())
    }
}
fn escaped(text: &str) -> Result<String> {
    let mut html = Html::default();
    html.escape(text)?;
    Ok(html.0)
}
fn array<'a>(value: Option<&'a Value>, message: &'static str) -> Result<&'a [Value]> {
    match value {
        None | Some(Value::Null) => Ok(&[]),
        Some(Value::Array(values)) => Ok(values),
        _ => Err(error(message)),
    }
}
fn key(value: &Value) -> Result<String> {
    match value {
        Value::String(value) if !value.is_empty() => Ok(value.clone()),
        Value::Number(value) => value
            .as_u64()
            .map(|value| value.to_string())
            .ok_or_else(|| error("Twitter article reference invalid")),
        _ => Err(error("Twitter article reference invalid")),
    }
}

fn render(article: &Value) -> Result<String> {
    let content = article
        .get("content_state")
        .filter(|value| value.is_object())
        .ok_or_else(|| error("Twitter article content missing or invalid"))?;
    let blocks = content
        .get("blocks")
        .and_then(Value::as_array)
        .ok_or_else(|| error("Twitter article blocks missing or invalid"))?;
    let mut entities = BTreeMap::new();
    for entity in array(
        content.get("entityMap"),
        "Twitter article entity map invalid",
    )? {
        let id = key(entity
            .get("key")
            .ok_or_else(|| error("Twitter article entity key missing"))?)?;
        let value = entity
            .get("value")
            .ok_or_else(|| error("Twitter article entity value missing"))?;
        if entities.insert(id, value).is_some() {
            return Err(error("Twitter article entity keys repeated"));
        }
    }
    let mut media = BTreeMap::new();
    for value in array(
        article.get("media_entities"),
        "Twitter article media list invalid",
    )? {
        let id = key(value
            .get("media_id")
            .ok_or_else(|| error("Twitter article media identifier missing"))?)?;
        if media.insert(id, value).is_some() {
            return Err(error("Twitter article media identifiers repeated"));
        }
    }
    let mut html = Html::default();
    let mut index = 0;
    let mut block_keys = HashSet::new();
    while index < blocks.len() {
        let block = &blocks[index];
        let kind = block
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| error("Twitter article block type missing"))?;
        let block_id = block
            .get("key")
            .and_then(Value::as_str)
            .ok_or_else(|| error("Twitter article block key missing"))?;
        if !block_keys.insert(block_id.to_owned()) {
            return Err(error("Twitter article block keys repeated"));
        }
        html.push("<section class=\"")?;
        html.escape(kind)?;
        html.push("\" id=\"")?;
        html.escape(block_id)?;
        html.push("\">\n")?;
        match kind {
            "unstyled" | "atomic" => {
                for range in array(
                    block.get("entityRanges"),
                    "Twitter article entity ranges invalid",
                )? {
                    let id = key(range
                        .get("key")
                        .ok_or_else(|| error("Twitter article entity reference missing"))?)?;
                    let entity = entities
                        .get(&id)
                        .ok_or_else(|| error("Twitter article entity reference missing"))?;
                    render_entity(&mut html, block, entity, &media)?;
                }
                if kind == "unstyled" {
                    render_text(&mut html, block)?;
                    html.push("\n")?;
                }
            }
            "ordered-list-item" | "unordered-list-item" => {
                let tag = if kind == "ordered-list-item" {
                    "ol"
                } else {
                    "ul"
                };
                html.push(&format!("<{tag}>\n"))?;
                loop {
                    html.push("<li>")?;
                    render_text(&mut html, &blocks[index])?;
                    html.push("</li>\n")?;
                    if blocks
                        .get(index + 1)
                        .and_then(|block| block.get("type"))
                        .and_then(Value::as_str)
                        != Some(kind)
                    {
                        break;
                    }
                    index += 1;
                    let next_id = blocks[index]
                        .get("key")
                        .and_then(Value::as_str)
                        .ok_or_else(|| error("Twitter article block key missing"))?;
                    if !block_keys.insert(next_id.to_owned()) {
                        return Err(error("Twitter article block keys repeated"));
                    }
                }
                html.push(&format!("</{tag}>\n"))?;
            }
            _ => return Err(error("Twitter article block type unsupported")),
        }
        html.push("</section>\n\n")?;
        index += 1;
    }
    Ok(html.0)
}

fn render_entity(
    html: &mut Html,
    block: &Value,
    entity: &Value,
    media: &BTreeMap<String, &Value>,
) -> Result<()> {
    if entity.is_null() {
        return Ok(());
    }
    match entity.get("type").and_then(Value::as_str) {
        Some("MEDIA") => {
            let data = entity
                .get("data")
                .filter(|value| value.is_object())
                .ok_or_else(|| error("Twitter article media entity data invalid"))?;
            for item in array(
                data.get("mediaItems"),
                "Twitter article media references invalid",
            )? {
                let id = key(item
                    .get("mediaId")
                    .ok_or_else(|| error("Twitter article media reference missing"))?)?;
                let source = media
                    .get(&id)
                    .ok_or_else(|| error("Twitter article referenced media missing"))?;
                let (url, _, _) = image(source)?;
                html.push("<img src=\"")?;
                html.escape(url.as_str())?;
                html.push("\" alt=\"\">\n")?;
            }
            if let Some(caption) =
                optional_string(data.get("caption"), "Twitter article caption invalid")?
            {
                html.push("<span class=\"caption\">")?;
                html.escape(&caption)?;
                html.push("</span>\n")?;
            }
        }
        Some("DIVIDER") => html.push("<hr>\n")?,
        Some("TWEMOJI") => {}
        Some("LATEX") => {
            html.push("<math xmlns=\"http://www.w3.org/1998/Math/MathML\"><semantics><annotation encoding=\"application/x-tex\">")?;
            html.escape(
                block
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| error("Twitter article formula text invalid"))?,
            )?;
            html.push("</annotation></semantics></math>\n")?;
        }
        _ => return Err(error("Twitter article entity type unsupported")),
    }
    Ok(())
}

#[derive(Clone)]
struct Annotation {
    start: usize,
    end: usize,
    open: String,
    close: &'static str,
    link: bool,
}
fn index(value: Option<&Value>) -> Result<usize> {
    value
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| error("Twitter article text range invalid"))
}
fn render_text(html: &mut Html, block: &Value) -> Result<()> {
    let text = block
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| error("Twitter article block text invalid"))?;
    // Draft.js positions count UTF-16 code units; never split a surrogate pair or UTF-8 character.
    let units = text.encode_utf16().count();
    let mut annotations = Vec::new();
    for range in array(
        block.get("inlineStyleRanges"),
        "Twitter article text styles invalid",
    )? {
        let start = index(range.get("offset"))?;
        let end = start
            .checked_add(index(range.get("length"))?)
            .ok_or_else(|| error("Twitter article text range invalid"))?;
        let (open, close) = match range.get("style").and_then(Value::as_str) {
            Some("BOLD") => ("<b>", "</b>"),
            Some("ITALIC") => ("<i>", "</i>"),
            Some("UNDERLINE") => ("<u>", "</u>"),
            Some("STRIKETHROUGH") => ("<s>", "</s>"),
            Some("CODE") => ("<code>", "</code>"),
            _ => return Err(error("Twitter article text style unsupported")),
        };
        annotations.push(Annotation {
            start,
            end,
            open: open.into(),
            close,
            link: false,
        });
    }
    if let Some(data) = block.get("data").filter(|value| !value.is_null()) {
        if !data.is_object() {
            return Err(error("Twitter article text data invalid"));
        }
        for (field, mention) in [("mentions", true), ("urls", false)] {
            for range in array(data.get(field), "Twitter article text links invalid")? {
                let start = index(range.get("fromIndex"))?;
                let end = index(range.get("toIndex"))?;
                let source = range
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| error("Twitter article text link invalid"))?;
                let target = if mention {
                    let name = source.strip_prefix('@').unwrap_or(source);
                    if name.is_empty()
                        || name.len() > 15
                        || !name
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                    {
                        return Err(error("Twitter article mention invalid"));
                    }
                    format!("https://x.com/@{name}")
                } else {
                    http_url(source)
                        .map_err(|_| error("Twitter article text link URL invalid"))?
                        .to_string()
                };
                annotations.push(Annotation {
                    start,
                    end,
                    open: format!("<a href=\"{}\">", escaped(&target)?),
                    close: "</a>",
                    link: true,
                });
            }
        }
    }
    let mut events: BTreeMap<usize, (Vec<usize>, Vec<usize>)> = BTreeMap::new();
    events.entry(0).or_default();
    events.entry(units).or_default();
    for (id, annotation) in annotations.iter().enumerate() {
        if annotation.start >= annotation.end || annotation.end > units {
            return Err(error("Twitter article text range invalid"));
        }
        events.entry(annotation.start).or_default().0.push(id);
        events.entry(annotation.end).or_default().1.push(id);
    }
    // Index only requested boundaries, rather than allocating one tree node per character.
    let mut positions = BTreeMap::from([(0, 0)]);
    let mut offset = 0;
    for (byte, character) in text.char_indices() {
        offset += character.len_utf16();
        if events.contains_key(&offset) {
            positions.insert(offset, byte + character.len_utf8());
        }
    }
    if events.keys().any(|offset| !positions.contains_key(offset)) {
        return Err(error("Twitter article text range splits a character"));
    }
    let events: Vec<_> = events.into_iter().collect();
    let mut active = BTreeMap::new();
    for pair in events.windows(2) {
        let (offset, (starts, ends)) = &pair[0];
        for id in ends {
            active.remove(id);
        }
        for id in starts {
            if annotations[*id].link && active.values().any(|prior: &&Annotation| prior.link) {
                return Err(error("Twitter article text links overlap"));
            }
            active.insert(*id, &annotations[*id]);
        }
        for annotation in active.values() {
            html.push(&annotation.open)?;
        }
        html.escape(&text[positions[offset]..positions[&pair[1].0]])?;
        for annotation in active.values().rev() {
            html.push(annotation.close)?;
        }
    }
    Ok(())
}

fn document(article: &Value, content: &str) -> Result<String> {
    let title = article
        .get("title")
        .and_then(Value::as_str)
        .ok_or_else(|| error("Twitter article document title missing or invalid"))?;
    let mut html = Html::default();
    html.push("<!DOCTYPE html>\n<html dir=\"ltr\" lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><style>body{max-width:598px;margin:auto;padding:1em;font-family:Verdana,sans-serif;line-height:1.65}section{margin-bottom:1.5em}img{max-width:100%}.cover{width:100%}.caption{display:block;font-size:.8em;color:#667}li{margin-bottom:.7em}h1{font-size:2em}math{display:block}</style><title>")?;
    html.escape(title)?;
    html.push("</title></head><body>\n")?;
    if let Some(cover) = article.get("cover_media").filter(|value| !value.is_null()) {
        let (url, _, _) = image(cover)?;
        html.push("<img class=\"cover\" src=\"")?;
        html.escape(url.as_str())?;
        html.push("\" alt=\"")?;
        html.escape(title)?;
        html.push("\">\n")?;
    }
    html.push("<h1>")?;
    html.escape(title)?;
    html.push("</h1>\n")?;
    html.push(content)?;
    html.push("</body></html>\n")?;
    Ok(html.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn article(content: Value) -> Value {
        json!({"rest_id":"101","title":"A <title>","metadata":{"first_published_at_secs":"1700000000"},
            "lifecycle_state":{"modified_at_secs":1700000001},"media_entities":[],
            "content_state":content})
    }
    fn source(article: Value) -> Value {
        json!({"article":{"article_results":{"result":article}}})
    }
    fn fragment() -> ArticleOptions {
        ArticleOptions {
            html: true,
            ..ArticleOptions::disabled()
        }
    }

    #[test]
    fn disabled_and_metadata_only_do_not_parse_unrequested_media_or_content() {
        assert!(
            select(
                &json!({"article":"private malformed article"}),
                ArticleOptions::disabled()
            )
            .1
            .is_none()
        );
        let mut value = article(json!("private malformed content"));
        value["cover_media"] =
            json!({"media_info":{"original_img_url":"file:///private/cover.jpg"}});
        value["media_entities"] = json!("private malformed media");
        let (files, metadata) = select(
            &source(value),
            ArticleOptions {
                metadata: true,
                ..ArticleOptions::disabled()
            },
        );
        assert!(files.is_empty());
        let metadata = metadata.unwrap();
        assert_eq!(metadata.id.as_deref(), Some("101"));
        assert_eq!(metadata.title.as_deref(), Some("A <title>"));
        assert_eq!(metadata.date, Some(1700000000));
        assert_eq!(metadata.date_updated, Some(1700000001));
        assert!(metadata.html.is_none());
        assert!(
            serde_json::to_value(metadata)
                .unwrap()
                .get("html")
                .is_none()
        );
    }

    #[test]
    fn text_renderer_escapes_html_and_supports_utf16_styles_links_and_mentions() {
        let text = "😀 <script> @alice https://e.test/?a=1&b=2";
        let position = |needle: &str| text[..text.find(needle).unwrap()].encode_utf16().count();
        let block = json!({"key":"safe\" id","type":"unstyled","text":text,
            "inlineStyleRanges":[{"offset":0,"length":2,"style":"BOLD"}],
            "data":{"mentions":[{"fromIndex":position("@alice"),"toIndex":position("@alice")+6,"text":"alice"}],
                "urls":[{"fromIndex":position("https:"),"toIndex":text.encode_utf16().count(),"text":"https://e.test/?a=1&b=2"}]}});
        let value = article(json!({"blocks":[block],"entityMap":[]}));
        let (_, metadata) = select(&source(value), fragment());
        let html = metadata.unwrap().html.unwrap();
        assert!(html.contains("<b>😀</b>"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(!html.contains("<script>"));
        assert!(html.contains("id=\"safe&quot; id\""));
        assert!(html.contains("href=\"https://x.com/@alice\">@alice</a>"));
        assert!(html.contains("href=\"https://e.test/?a=1&amp;b=2\""));
    }

    #[test]
    fn lists_and_supported_entities_render_in_source_order() {
        let mut value = article(json!({"entityMap":[
            {"key":"0","value":{"type":"MEDIA","data":{"mediaItems":[{"mediaId":"5"}],"caption":"<caption>"}}},
            {"key":"1","value":{"type":"DIVIDER"}},
            {"key":"2","value":{"type":"LATEX"}},
            {"key":"3","value":{"type":"TWEMOJI"}}
        ],"blocks":[
            {"key":"a","type":"ordered-list-item","text":"one"},
            {"key":"b","type":"ordered-list-item","text":"two"},
            {"key":"c","type":"unordered-list-item","text":"three"},
            {"key":"d","type":"atomic","text":"x < y","entityRanges":[{"key":0},{"key":1},{"key":2},{"key":3}]}
        ]}));
        value["media_entities"] = json!([{"media_id":"5","media_info":{"original_img_url":"https://cdn.example/i.jpg?token=signed&name=small"}}]);
        let (_, metadata) = select(&source(value), fragment());
        let html = metadata.unwrap().html.unwrap();
        assert_eq!(html.matches("<ol>").count(), 1);
        assert_eq!(html.matches("<ul>").count(), 1);
        assert!(html.find("<li>one</li>").unwrap() < html.find("<li>two</li>").unwrap());
        assert!(html.contains("token=signed&amp;name=orig"));
        assert!(html.contains("&lt;caption&gt;"));
        assert!(html.contains("<hr>"));
        assert!(html.contains("application/x-tex\">x &lt; y</annotation>"));
    }

    #[test]
    fn document_precedes_cover_and_media_and_keeps_allowlisted_metadata_shared() {
        let mut value = article(
            json!({"blocks":[{"key":"a","type":"unstyled","text":"hello"}],"entityMap":[]}),
        );
        value["cover_media"] = json!({"media_info":{"original_img_url":"https://cdn.example/cover.jpg?token=s&name=small"}});
        value["media_entities"] = json!([{"media_id":"5","media_info":{"original_img_url":"https://cdn.example/image.png"}}]);
        let (files, metadata) = select(
            &source(value),
            ArticleOptions {
                document: true,
                html: true,
                ..Default::default()
            },
        );
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].1, "html");
        match &files[0].2 {
            MediaJob::InlineText { data } => {
                assert!(data.starts_with("<!DOCTYPE html>"));
                assert!(data.contains("<title>A &lt;title&gt;</title>"));
                assert!(data.contains("token=s&amp;name=orig"));
                assert!(!data.contains("<script"));
            }
            _ => panic!("article document must be an inline text job"),
        }
        assert_eq!(files[1].0.path(), "/cover.jpg");
        assert_eq!(files[2].0.path(), "/image.png");
        let metadata = metadata.unwrap();
        let clone = metadata.clone();
        assert!(Arc::ptr_eq(
            metadata.html.as_ref().unwrap(),
            clone.html.as_ref().unwrap()
        ));
        assert!(Arc::ptr_eq(
            metadata.title.as_ref().unwrap(),
            clone.title.as_ref().unwrap()
        ));
    }

    #[test]
    fn malformed_rendering_inputs_fail_privately_without_accepting_unsafe_links_or_split_characters()
     {
        for block in [
            json!({"key":"a","type":"private unsupported block","text":"hello"}),
            json!({"key":"a","type":"unstyled","text":"😀","inlineStyleRanges":[{"offset":1,"length":1,"style":"BOLD"}]}),
            json!({"key":"a","type":"unstyled","text":"link","data":{"urls":[{"fromIndex":0,"toIndex":4,"text":"javascript:private()"}]}}),
            json!({"key":"a","type":"atomic","entityRanges":[{"key":"private missing entity"}]}),
        ] {
            let value = article(json!({"blocks":[block],"entityMap":[]}));
            let error = render(&value).unwrap_err().to_string();
            let (files, metadata) = select(&source(value), fragment());
            assert!(files.is_empty());
            assert!(metadata.is_none());
            assert!(error.contains("article"));
            assert!(!error.contains("private"));
        }
    }

    #[test]
    fn repeated_media_references_cannot_amplify_rendered_output_past_the_bound() {
        let url = format!("https://cdn.example/{}.jpg", "a".repeat(1024 * 1024));
        let references: Vec<_> = (0..20).map(|_| json!({"mediaId":"5"})).collect();
        let mut value = article(
            json!({"blocks":[{"key":"a","type":"atomic","entityRanges":[{"key":0}]}],
            "entityMap":[{"key":"0","value":{"type":"MEDIA","data":{"mediaItems":references}}}]}),
        );
        value["media_entities"] = json!([{"media_id":"5","media_info":{"original_img_url":url}}]);
        let error = render(&value).unwrap_err().to_string();
        let (files, metadata) = select(&source(value), fragment());
        assert!(files.is_empty());
        assert!(metadata.is_none());
        assert!(error.contains("exceeded 16 MiB"));
    }

    #[test]
    fn invalid_optional_article_envelopes_recover_without_files_or_metadata() {
        for value in [
            json!({}),
            json!({"article":null}),
            json!({"article":"private malformed article"}),
            json!({"article":{"article_results":{"result":null}}}),
        ] {
            let (files, metadata) = select(&value, ArticleOptions::default());
            assert!(files.is_empty());
            assert!(metadata.is_none());
        }
    }

    #[test]
    fn recoverable_errors_preserve_completed_metadata_document_cover_and_media_in_order() {
        let mut value = article(
            json!({"blocks":[{"key":"a","type":"unstyled","text":"safe content"}],"entityMap":[]}),
        );
        value["cover_media"] =
            json!({"media_info":{"original_img_url":"https://cdn.example/cover.jpg"}});
        value["media_entities"] = json!([
            {"media_id":"5","media_info":{"original_img_url":"https://cdn.example/first.jpg"}},
            {"media_id":"6","media_info":{"original_img_url":"file:///private/bad.jpg"}},
            {"media_id":"7","media_info":{"original_img_url":"https://cdn.example/after-error.jpg"}}
        ]);
        let (files, metadata) = select(
            &source(value.clone()),
            ArticleOptions {
                document: true,
                html: true,
                ..Default::default()
            },
        );
        assert_eq!(files.len(), 3);
        assert!(matches!(files[0].2, MediaJob::InlineText { .. }));
        assert_eq!(files[1].0.path(), "/cover.jpg");
        assert_eq!(files[2].0.path(), "/first.jpg");
        assert_eq!(
            metadata.as_ref().unwrap().title.as_deref(),
            Some("A <title>")
        );
        assert!(metadata.unwrap().html.unwrap().contains("safe content"));

        value["cover_media"]["media_info"]["original_img_url"] = json!("file:///private/cover.jpg");
        let (files, metadata) = select(&source(value.clone()), ArticleOptions::default());
        assert!(files.is_empty());
        assert_eq!(metadata.unwrap().id.as_deref(), Some("101"));

        value["content_state"] = json!("private invalid content");
        let (files, metadata) = select(
            &source(value),
            ArticleOptions {
                html: true,
                ..Default::default()
            },
        );
        assert!(files.is_empty());
        assert_eq!(metadata.as_ref().unwrap().id.as_deref(), Some("101"));
        assert!(metadata.unwrap().html.is_none());
    }
}
