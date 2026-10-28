//! Non-executable filename and directory templates.
use crate::{Result, error, extract::Media};
use std::{collections::BTreeMap, path::PathBuf};
#[path = "naming_format.rs"]
pub(crate) mod format;
use format::{Field, Value};

const MAX_BYTES: usize = 240;
const MAX_TEMPLATE: usize = 8192;

/// Names a template may reference. Signed transport URLs are deliberately
/// unavailable so credentials cannot leak into paths.
const FIELDS: &[&str] = &[
    "_parent",
    "md5",
    "sha1",
    "sha224",
    "sha256",
    "sha384",
    "sha512",
    "sha512_224",
    "sha512_256",
    "sha3_224",
    "sha3_256",
    "sha3_384",
    "sha3_512",
    "blake2b",
    "blake2s",
    "filename",
    "name",
    "extension",
    "category",
    "post_id",
    "media_id",
    "shortcode",
    "username",
    "user",
    "user_id",
    "service",
    "instance",
    "index",
    "num",
    "timestamp",
    "date",
    "date_favorited",
    "scale",
    "caption",
    "caption_html",
    "title",
    "suffix",
    "date_url",
    "ajax",
    "_ajax",
    "mypixiv",
    "_mypixiv",
    "content",
    "description",
    "community",
    "community_id",
    "community_name",
    "article",
    "width",
    "height",
    "width_original",
    "height_original",
    "image_width",
    "image_height",
    "duration",
    "bitrate",
    "media_type",
    "type",
    "sensitive_flags",
    "source_id",
    "source_username",
    "source_user",
    "post_shortcode",
    "tweet_id",
    "retweet_id",
    "quote_id",
    "quoted_by_id",
    "reposted_by_id",
    "id",
    "metadata",
    "profile",
    "profile_publicity",
    "workspace",
    "comments",
    "tags_bookmark",
    "frames",
    "_ugoira_frame_data",
    "ugoira_original",
    "ugoira_frame_index",
    "_ugoira_original",
    "_ugoira_frame_index",
    "tags",
    "sketch",
    "user",
    "text",
    "created_at",
    "subcategory",
    "profile_asset",
    "ranking",
    "search",
    "series",
    "num_series",
    "user_follow",
    "user_bookmark",
    "pixivision",
    "pixivision_id",
    "pixivision_title",
    "rating",
    "score",
    "search_tags",
    "pool",
    "pool_name",
    "favorite_id",
    "favgroup",
    "filesize",
    "revision",
    "revision_id",
    "revision_hash",
    "revision_index",
    "revision_count",
    "creator",
    "creator_id",
    "creator_name",
    "user_profile",
    "discord",
    "server",
    "server_id",
    "channel",
    "channel_id",
    "channel_topic",
    "channel_nsfw",
    "channel_type",
    "parent_id",
];

#[derive(Clone, Debug)]
enum Part {
    Literal(String),
    Field(Field),
}

/// A single-basename template using gallery-dl replacement fields, such as
/// `{date:%Y-%m-%d}_{username!l}_{index:>03}`, `{caption[:40]|post_id}`, or
/// `{community_name:?[/]/}`. Doubled braces escape literal braces. Rendered
/// names use the downloader's ASCII alphabet.
#[derive(Clone, Debug)]
pub struct FilenameTemplate {
    parts: Option<Vec<Part>>,
    /// Upstream-style conditional formats: the first matching filter selects
    /// its template; otherwise `parts`, then the default name, applies.
    conditions: Vec<(crate::filter::FilterExpression, Vec<Part>)>,
}

impl FilenameTemplate {
    /// Parse `(filter, template)` pairs in order with an optional default.
    pub fn parse_conditional(
        conditions: &[(String, String)],
        default: Option<&str>,
    ) -> Result<Self> {
        Self::parse_conditional_with_keywords(conditions, default, &[])
    }
    pub fn parse_conditional_with_keywords(
        conditions: &[(String, String)],
        default: Option<&str>,
        keywords: &[String],
    ) -> Result<Self> {
        Self::parse_conditional_mode(conditions, default, keywords, false)
    }
    pub fn parse_source_conditional(
        conditions: &[(String, String)],
        default: Option<&str>,
    ) -> Result<Self> {
        Self::parse_conditional_mode(conditions, default, &[], true)
    }
    fn parse_conditional_mode(
        conditions: &[(String, String)],
        default: Option<&str>,
        keywords: &[String],
        source: bool,
    ) -> Result<Self> {
        if conditions.len() > 32 {
            return Err(error("filename template has too many conditions"));
        }
        Ok(Self {
            parts: default
                .map(|value| Self::parse_mode(value, keywords, source))
                .transpose()?
                .and_then(|t| t.parts),
            conditions: conditions
                .iter()
                .map(|(filter, template)| {
                    Ok((
                        crate::filter::FilterExpression::parse(filter)?,
                        Self::parse_mode(template, keywords, source)?
                            .parts
                            .unwrap_or_default(),
                    ))
                })
                .collect::<Result<_>>()?,
        })
    }

    pub fn parse(template: &str) -> Result<Self> {
        Self::parse_with_keywords(template, &[])
    }
    pub fn parse_with_keywords(template: &str, keywords: &[String]) -> Result<Self> {
        Self::parse_mode(template, keywords, false)
    }
    /// Parse fields supplied dynamically by a source extractor.
    pub fn parse_source(template: &str) -> Result<Self> {
        Self::parse_mode(template, &[], true)
    }
    fn parse_mode(template: &str, keywords: &[String], source: bool) -> Result<Self> {
        if keywords.len() > 4096 || keywords.iter().map(String::len).sum::<usize>() > 1024 * 1024 {
            return Err(error("template keyword declarations exceed limits"));
        }
        if template.is_empty() || matches!(template, "." | "..") {
            return Err(error("filename template must produce a basename"));
        }
        if template.len() > MAX_TEMPLATE {
            return Err(error("filename template exceeds 8192 bytes"));
        }
        let mut chars = template.chars().peekable();
        let mut parts = Vec::new();
        let mut literal = String::new();
        while let Some(c) = chars.next() {
            match c {
                '/' | '\\' | ':' => {
                    return Err(error(
                        "filename templates cannot contain paths or drive prefixes",
                    ));
                }
                '{' if chars.peek() == Some(&'{') => {
                    chars.next();
                    literal.push('{');
                }
                '}' if chars.peek() == Some(&'}') => {
                    chars.next();
                    literal.push('}');
                }
                '{' => {
                    if !literal.is_empty() {
                        parts.push(Part::Literal(std::mem::take(&mut literal)));
                    }
                    let mut field = String::new();
                    loop {
                        match chars.next() {
                            Some('}') => break,
                            Some('{') | None => {
                                return Err(error("unclosed filename template field"));
                            }
                            Some(c) => field.push(c),
                        }
                    }
                    if parts.len() >= 256 {
                        return Err(error("filename template has too many fields"));
                    }
                    let known = |name: &str| {
                        source || FIELDS.contains(&name) || keywords.iter().any(|key| key == name)
                    };
                    parts.push(Part::Field(if source {
                        Field::parse_source(&field, known)?
                    } else {
                        Field::parse(&field, known)?
                    }));
                }
                '}' => return Err(error("unmatched closing brace in filename template")),
                c => literal.push(c),
            }
        }
        if !literal.is_empty() {
            parts.push(Part::Literal(literal));
        }
        Ok(Self {
            parts: Some(parts),
            conditions: Vec::new(),
        })
    }

    /// Render, failing if no condition matches and no default exists.
    pub fn render(&self, media: &Media) -> Result<String> {
        self.render_optional(media)?
            .ok_or_else(|| error("no filename template condition matched"))
    }

    /// Render, or `None` when the default filename policy should apply.
    pub fn render_optional(&self, media: &Media) -> Result<Option<String>> {
        let mut context = None;
        match self.select(media)? {
            Some(parts) => Self::render_parts(parts, media, &mut context, false, false).map(Some),
            None => Ok(None),
        }
    }

    pub(crate) fn render_keywords(
        &self,
        media: &Media,
        keywords: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<Option<String>> {
        self.render_keywords_mode(media, keywords, false)
    }
    fn render_keywords_mode(
        &self,
        media: &Media,
        keywords: &serde_json::Map<String, serde_json::Value>,
        directory_component: bool,
    ) -> Result<Option<String>> {
        let mut context = Some(keyword_context(media, keywords));
        let mut selected = self.parts.as_deref();
        for (filter, parts) in &self.conditions {
            if filter.matches_keywords(keywords)? {
                selected = Some(parts);
                break;
            }
        }
        match selected {
            Some(parts) => {
                Self::render_parts(parts, media, &mut context, true, directory_component).map(Some)
            }
            None => Ok(None),
        }
    }
    fn select(&self, media: &Media) -> Result<Option<&[Part]>> {
        for (filter, parts) in &self.conditions {
            if filter.matches(media)? {
                return Ok(Some(parts));
            }
        }
        Ok(self.parts.as_deref())
    }

    fn render_directory_segments(
        &self,
        media: &Media,
        context: &mut Option<BTreeMap<String, crate::filter::Value>>,
        keywords: Option<&serde_json::Map<String, serde_json::Value>>,
        examined: &mut usize,
    ) -> Result<Vec<String>> {
        let mut parts = self.parts.as_deref();
        if let Some(keywords) = keywords {
            for (filter, candidate) in &self.conditions {
                if filter.matches_keywords(keywords)? {
                    parts = Some(candidate);
                    break;
                }
            }
        } else {
            parts = self.select(media)?;
        }
        let parts = parts.ok_or_else(|| error("no directory template condition matched"))?;
        if source_paths(media) {
            source_directory_segments(
                Self::render_source_parts(parts, media, context, keywords.is_some())?,
                examined,
            )
        } else {
            Ok(vec![Self::render_parts(
                parts,
                media,
                context,
                keywords.is_some(),
                true,
            )?])
        }
    }

    fn lookup_value(
        media: &Media,
        context: &BTreeMap<String, crate::filter::Value>,
        name: &str,
        keywords: bool,
    ) -> Option<Value> {
        if keywords {
            context.get(name).map(|value| {
                if matches!(name, "date" | "date_updated")
                    && let crate::filter::Value::String(value) = value
                    && let Ok(date) = crate::postprocess::template::parse_date(value)
                {
                    Value::Date(date)
                } else {
                    convert(value, name)
                }
            })
        } else {
            lookup(media, context, name)
        }
    }

    fn render_source_parts(
        parts: &[Part],
        media: &Media,
        context: &mut Option<BTreeMap<String, crate::filter::Value>>,
        keywords: bool,
    ) -> Result<Value> {
        let mut output = String::new();
        for part in parts {
            let text = match part {
                Part::Literal(value) => value.clone(),
                Part::Field(field) => {
                    let context = context.get_or_insert_with(|| crate::filter::context(media));
                    let value = unwrap_source_value(field.render_default_value(|name| {
                        Self::lookup_value(media, context, name, keywords)
                    })?);
                    if parts.len() == 1 {
                        return Ok(value);
                    }
                    let Value::Str(value) = value else {
                        return Err(error("source path template field did not return text"));
                    };
                    value
                }
            };
            if text.len() > 65536 - output.len() {
                return Err(error("source path template output exceeds limits"));
            }
            output.push_str(&text);
        }
        Ok(Value::Str(output))
    }

    fn render_parts(
        parts: &[Part],
        media: &Media,
        context: &mut Option<BTreeMap<String, crate::filter::Value>>,
        keywords: bool,
        directory_component: bool,
    ) -> Result<String> {
        if source_paths(media) {
            let Value::Str(value) = Self::render_source_parts(parts, media, context, keywords)?
            else {
                return Err(error("source filename template must return text"));
            };
            return clean_source_segment(value, directory_component);
        }
        let mut output = String::new();
        for part in parts {
            match part {
                Part::Literal(value) => output.push_str(value),
                Part::Field(field) => {
                    let context = context.get_or_insert_with(|| crate::filter::context(media));
                    let lookup = |name: &str| Self::lookup_value(media, context, name, keywords);
                    let source_nameext = media
                        .metadata
                        .as_ref()
                        .is_some_and(|metadata| metadata.booru_keywords.is_some())
                        && field
                            .names()
                            .any(|name| matches!(name, "filename" | "extension" | "md5"));
                    let value = if source_nameext {
                        field.render_content(lookup)?
                    } else {
                        field.render(lookup)?
                    };
                    let Some(value) = value else {
                        let name = field.names().next().unwrap_or("literal");
                        if media.metadata.is_none()
                            && !matches!(name, "filename" | "name" | "extension" | "category")
                        {
                            return Err(error(format!(
                                "filename field {name} requires post metadata"
                            )));
                        }
                        return Err(error(format!("filename field {name} is missing")));
                    };
                    if crate::path_options::PathOptions::get().is_some() {
                        output.push_str(&value);
                    } else {
                        output.extend(value.chars().map(safe_char));
                    }
                    if output.len() > 65536 {
                        return Err(error("filename template output exceeds limits"));
                    }
                }
            }
        }
        let output: String = match crate::path_options::PathOptions::get() {
            Some(options) => options.clean(&output, directory_component),
            None => output.chars().map(safe_char).collect(),
        };
        let mut output = output.trim_matches('.').to_string();
        if output.is_empty() {
            return Err(error("filename template produced an empty basename"));
        }
        if output.len() > MAX_BYTES {
            match output.rsplit_once('.') {
                Some((stem, extension)) => {
                    if extension.len() + 1 >= MAX_BYTES {
                        return Err(error("filename extension exceeds the basename limit"));
                    }
                    output = format!("{}.{}", &stem[..MAX_BYTES - extension.len() - 1], extension);
                }
                None => output.truncate(MAX_BYTES),
            }
        }
        // Check the final stem: truncation can itself create reserved names.
        // Windows reserves these stems even when an extension is present.
        let stem = output
            .split('.')
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase();
        if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || (stem.len() == 4
                && (stem.starts_with("COM") || stem.starts_with("LPT"))
                && matches!(stem.as_bytes()[3], b'1'..=b'9'))
        {
            if output.len() == MAX_BYTES {
                // Make room in the ASCII stem without changing the extension.
                output.remove(stem.len() - 1);
            }
            output.insert(0, '_');
        }
        Ok(output)
    }
}

fn safe_char(c: char) -> char {
    if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
        c
    } else {
        '_'
    }
}

fn keyword_context(
    media: &Media,
    keywords: &serde_json::Map<String, serde_json::Value>,
) -> BTreeMap<String, crate::filter::Value> {
    keywords
        .iter()
        .map(|(key, value)| {
            let value = crate::filter::keyword_value(key, value);
            let inherited = media
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.booru_artist_parent.as_ref())
                .is_some_and(|inheritance| inheritance.has_keyword(key));
            (
                key.clone(),
                if inherited {
                    crate::filter::Value::SourcePrimitive(std::sync::Arc::new(value))
                } else {
                    value
                },
            )
        })
        .collect()
}

fn source_paths(media: &Media) -> bool {
    media.metadata.as_ref().is_some_and(|metadata| {
        metadata.source_defaults.is_some() || metadata.site_keywords.is_some()
    })
}
fn unwrap_source_value(mut value: Value) -> Value {
    while let Value::SourcePrimitive(inner) = value {
        value = *inner;
    }
    value
}
fn clean_source_segment(mut output: String, directory_component: bool) -> Result<String> {
    if output.len() > 65536 {
        return Err(error("source path template output exceeds limits"));
    }
    if directory_component {
        output = output.trim().to_owned();
    }
    if let Some(options) = crate::path_options::PathOptions::get() {
        let output = options.clean(&output, directory_component);
        if matches!(output.as_str(), "." | "..") || output.is_empty() && !directory_component {
            return Err(error("source filename must produce a safe basename"));
        }
        return Ok(output);
    }
    let output: String = output
        .chars()
        .filter(|c| !matches!(*c, '\0'..='\u{1f}' | '\u{7f}'))
        .map(|c| {
            if c == '/'
                || cfg!(windows) && matches!(c, '\\' | '|' | '<' | '>' | ':' | '"' | '?' | '*')
            {
                '_'
            } else {
                c
            }
        })
        .collect();
    let output = if cfg!(windows) && directory_component {
        output.trim_end_matches(['.', ' ']).to_owned()
    } else {
        output
    };
    if matches!(output.as_str(), "." | "..") || output.is_empty() && !directory_component {
        return Err(error("source filename must produce a safe basename"));
    }
    Ok(output)
}
fn source_directory_segments(value: Value, examined: &mut usize) -> Result<Vec<String>> {
    let value = unwrap_source_value(value);
    let count = match &value {
        Value::List(values) => values.len(),
        Value::Map(values) => values.len(),
        Value::Missing => 0,
        _ => 1,
    };
    *examined = examined
        .checked_add(count)
        .filter(|count| *count <= 4096)
        .ok_or_else(|| error("source directory template exceeds item limits"))?;
    let values = match value {
        Value::Str(value) => vec![Value::Str(value)],
        Value::List(values) => values,
        Value::Map(values) => values.into_iter().map(|(key, _)| Value::Str(key)).collect(),
        Value::Missing => Vec::new(),
        _ => {
            return Err(error(
                "source directory template must return text or an iterable",
            ));
        }
    };
    values
        .into_iter()
        .map(|value| {
            let Value::Str(value) = unwrap_source_value(value) else {
                return Err(error("source directory segments must be text"));
            };
            clean_source_segment(value, true)
        })
        .collect()
}

/// Ordered component templates relative to the configured base directory.
/// An empty list selects the base directory itself. Substitutions cannot add
/// separators, absolute prefixes, or parent-directory components.
#[derive(Clone, Debug)]
pub struct DirectoryTemplate {
    components: Option<Vec<FilenameTemplate>>,
    conditions: Vec<(crate::filter::FilterExpression, Vec<FilenameTemplate>)>,
}

/// Retained extractor class defaults; these are never part of mutable metadata.
#[derive(Clone, Debug)]
pub struct SourceDefaults {
    pub(crate) category: &'static str,
    pub(crate) subcategory: &'static str,
    pub(crate) archive: &'static str,
    filename: FilenameTemplate,
    directory: DirectoryTemplate,
}
impl SourceDefaults {
    pub(crate) fn new(
        category: &'static str,
        subcategory: &'static str,
        archive: &'static str,
        filename: &str,
        directory: &[&str],
    ) -> Self {
        Self {
            category,
            subcategory,
            archive,
            filename: FilenameTemplate::parse(filename).expect("static source filename"),
            directory: DirectoryTemplate::parse(
                &directory.iter().map(|s| (*s).into()).collect::<Vec<_>>(),
            )
            .expect("static source directory"),
        }
    }
}
pub(crate) fn effective_filename(
    media: &Media,
    explicit: Option<&FilenameTemplate>,
) -> Option<FilenameTemplate> {
    let source = media
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.source_defaults.as_deref());
    match (explicit, source) {
        (Some(template), Some(source)) => {
            let mut template = template.clone();
            if template.parts.is_none() {
                template.parts = source.filename.parts.clone();
            }
            Some(template)
        }
        (Some(template), None) => Some(template.clone()),
        (None, Some(source)) => Some(source.filename.clone()),
        (None, None) => {
            crate::sites::default_templates(media).map(|(filename, _)| filename.clone())
        }
    }
}
pub(crate) fn effective_directory(
    media: &Media,
    explicit: Option<&DirectoryTemplate>,
) -> Option<DirectoryTemplate> {
    let source = media
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.source_defaults.as_deref());
    match (explicit, source) {
        (Some(template), Some(source)) => {
            let mut template = template.clone();
            if template.components.is_none() {
                template.components = source.directory.components.clone();
            }
            Some(template)
        }
        (Some(template), None) => Some(template.clone()),
        (None, Some(source)) => Some(source.directory.clone()),
        (None, None) => {
            crate::sites::default_templates(media).map(|(_, directory)| directory.clone())
        }
    }
}

impl DirectoryTemplate {
    /// Parse `(filter, components)` pairs in order with an optional default.
    pub fn parse_conditional(
        conditions: &[(String, Vec<String>)],
        default: Option<&[String]>,
    ) -> Result<Self> {
        Self::parse_conditional_with_keywords(conditions, default, &[])
    }
    pub fn parse_conditional_with_keywords(
        conditions: &[(String, Vec<String>)],
        default: Option<&[String]>,
        keywords: &[String],
    ) -> Result<Self> {
        Self::parse_conditional_mode(conditions, default, keywords, false)
    }
    pub fn parse_source_conditional(
        conditions: &[(String, Vec<String>)],
        default: Option<&[String]>,
    ) -> Result<Self> {
        Self::parse_conditional_mode(conditions, default, &[], true)
    }
    fn parse_conditional_mode(
        conditions: &[(String, Vec<String>)],
        default: Option<&[String]>,
        keywords: &[String],
        source: bool,
    ) -> Result<Self> {
        if conditions.len() > 32 {
            return Err(error("directory template has too many conditions"));
        }
        Ok(Self {
            components: default
                .map(|value| Self::parse_mode(value, keywords, source))
                .transpose()?
                .and_then(|t| t.components),
            conditions: conditions
                .iter()
                .map(|(filter, components)| {
                    Ok((
                        crate::filter::FilterExpression::parse(filter)?,
                        Self::parse_mode(components, keywords, source)?
                            .components
                            .unwrap_or_default(),
                    ))
                })
                .collect::<Result<_>>()?,
        })
    }

    pub fn parse(components: &[String]) -> Result<Self> {
        Self::parse_with_keywords(components, &[])
    }
    pub fn parse_with_keywords(components: &[String], keywords: &[String]) -> Result<Self> {
        Self::parse_mode(components, keywords, false)
    }
    /// Parse fields supplied dynamically by a source extractor.
    pub fn parse_source(components: &[String]) -> Result<Self> {
        Self::parse_mode(components, &[], true)
    }
    fn parse_mode(components: &[String], keywords: &[String], source: bool) -> Result<Self> {
        if components.len() > 32
            || components
                .iter()
                .any(|s| s.len() > 8192 || s.chars().any(char::is_control))
        {
            return Err(error("directory template exceeds component limits"));
        }
        Ok(Self {
            components: Some(
                components
                    .iter()
                    .map(|s| FilenameTemplate::parse_mode(s, keywords, source))
                    .collect::<Result<_>>()?,
            ),
            conditions: Vec::new(),
        })
    }

    /// Render, or `None` when no condition matches and no default exists,
    /// selecting the normal category directory.
    pub fn render(&self, media: &Media) -> Result<Option<PathBuf>> {
        let mut selected = self.components.as_deref();
        for (filter, components) in &self.conditions {
            if filter.matches(media)? {
                selected = Some(components);
                break;
            }
        }
        let Some(components) = selected else {
            return Ok(None);
        };
        let mut output = PathBuf::new();
        let mut bytes = 0;
        let mut context = None;
        let mut examined = 0;
        for component in components {
            for segment in
                component.render_directory_segments(media, &mut context, None, &mut examined)?
            {
                if segment.is_empty() {
                    continue;
                }
                bytes += segment.len() + 1;
                if bytes > 3072 {
                    return Err(error("rendered directory exceeds 3072 bytes"));
                }
                output.push(segment);
            }
        }
        Ok(Some(output))
    }
    pub(crate) fn render_keywords(
        &self,
        media: &Media,
        keywords: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<Option<PathBuf>> {
        let mut selected = self.components.as_deref();
        for (filter, components) in &self.conditions {
            if filter.matches_keywords(keywords)? {
                selected = Some(components);
                break;
            }
        }
        let Some(components) = selected else {
            return Ok(None);
        };
        let mut output = PathBuf::new();
        let mut bytes = 0;
        let mut context = Some(keyword_context(media, keywords));
        let mut examined = 0;
        for component in components {
            for segment in component.render_directory_segments(
                media,
                &mut context,
                Some(keywords),
                &mut examined,
            )? {
                if segment.is_empty() {
                    continue;
                }
                bytes += segment.len() + 1;
                if bytes > 3072 {
                    return Err(error("rendered directory exceeds 3072 bytes"));
                }
                output.push(segment);
            }
        }
        Ok(Some(output))
    }
}

fn convert(value: &crate::filter::Value, key: &str) -> Value {
    convert_value(value, key, key != "tags" && key != "_parent")
}

fn convert_value(value: &crate::filter::Value, key: &str, dates: bool) -> Value {
    use crate::filter::Value as Source;
    match value {
        Source::None => Value::None,
        Source::SourcePrimitive(value) => {
            Value::SourcePrimitive(Box::new(convert_value(value, key, false)))
        }
        Source::SourceDate(value) => value.map_or(Value::InvalidDate, format::source_timestamp),
        Source::SourceNullDate(value) => time::OffsetDateTime::from_unix_timestamp(*value)
            .map_or(Value::InvalidDate, Value::NullDate),
        Source::Bool(value) => Value::Bool(*value),
        // Upstream exposes these as datetimes; native metadata stores seconds.
        Source::Int(value) if dates && matches!(key, "date" | "date_updated") => {
            format::timestamp(*value)
        }
        Source::Int(value) => Value::Int(*value),
        Source::Float(value) => Value::Float(*value),
        Source::String(value) => Value::Str(value.to_string()),
        Source::List(values) | Source::Tuple(values) => Value::List(
            values
                .iter()
                .map(|value| convert_value(value, "", dates))
                .collect(),
        ),
        Source::Map(values) => Value::Map(
            values
                .iter()
                .map(|(key, value)| {
                    (
                        key.clone(),
                        convert_value(value, key, dates && key != "tags" && key != "_parent"),
                    )
                })
                .collect(),
        ),
    }
}

fn lookup(
    media: &Media,
    context: &BTreeMap<String, crate::filter::Value>,
    name: &str,
) -> Option<Value> {
    let (stem, extension) = media
        .filename
        .rsplit_once('.')
        .filter(|(stem, ext)| !stem.is_empty() && !ext.is_empty())
        .unwrap_or((&media.filename, ""));
    let metadata = media.metadata.as_ref();
    if let Some(inheritance) = metadata.and_then(|metadata| metadata.booru_artist_parent.as_ref())
        && inheritance.has_keyword(name)
    {
        return context
            .get(name)
            .map(|value| Value::SourcePrimitive(Box::new(convert_value(value, name, false))));
    }
    if let Some(fields) = metadata.and_then(|metadata| metadata.booru_popular.as_ref())
        && matches!(name, "date" | "scale")
    {
        return fields.get(name).map(|value| {
            Value::SourcePrimitive(Box::new(convert_value(
                &crate::filter::json_value(value),
                name,
                false,
            )))
        });
    }
    if matches!(name, "id" | "tags" | "date_favorited")
        && let Some(value) = metadata
            .and_then(|metadata| metadata.booru_keywords.as_ref())
            .and_then(|fields| fields.get(name))
    {
        return Some(convert(
            &if name == "date_favorited" {
                crate::filter::source_date_value(value)
            } else {
                crate::filter::json_value(value)
            },
            name,
        ));
    }
    if matches!(name, "filename" | "extension" | "md5")
        && let Some(fields) = metadata.and_then(|metadata| metadata.booru_keywords.as_deref())
    {
        return fields
            .get(name)
            .map(|value| convert(&crate::filter::json_value(value), name));
    }
    if metadata.is_some_and(|m| m.booru_artist_parent.is_some() || m.booru_popular.is_some())
        && matches!(name, "name" | "num")
    {
        return context.get(name).map(|value| convert(value, name));
    }
    if metadata
        .and_then(|metadata| metadata.booru_keywords.as_ref())
        .is_some_and(|fields| fields.contains_key(name))
    {
        return context.get(name).map(|value| convert(value, name));
    }
    if metadata
        .and_then(|metadata| metadata.site_keywords.as_ref())
        .is_some_and(|keywords| keywords.fields.contains_key(name))
    {
        return context.get(name).map(|value| convert(value, name));
    }
    Some(match name {
        // Like upstream, `filename` excludes the extension.
        "filename" | "name" => Value::Str(stem.into()),
        "extension" => Value::Str(extension.into()),
        "num" if metadata?.source_num.is_some() => convert(context.get("num")?, "num"),
        "num" if metadata?.pixiv_novel_metadata.is_some() => convert(context.get("num")?, "num"),
        "index" | "num" => match metadata?.index {
            0 => Value::None,
            index => Value::Int(index as i128),
        },
        "community_id" => Value::Str(metadata?.community.as_ref()?.id.clone()),
        "community_name" => Value::Str(metadata?.community.as_ref()?.name.as_deref()?.into()),
        name => {
            let value = context.get(name)?;
            if metadata.is_some_and(|metadata| metadata.sketch.is_some()) {
                // Only the canonical post date is a datetime. Sketch's other
                // public dictionaries retain their original JSON field types.
                if name == "metadata" {
                    if let crate::filter::Value::Map(values) = value {
                        Value::Map(
                            values
                                .iter()
                                .map(|(key, value)| {
                                    (key.clone(), convert_value(value, key, key == "date"))
                                })
                                .collect(),
                        )
                    } else {
                        convert_value(value, name, false)
                    }
                } else {
                    convert_value(value, name, name == "date")
                }
            } else {
                convert(value, name)
            }
        }
    })
}

#[cfg(test)]
#[path = "source_formatter_date_tests.rs"]
mod source_formatter_date_tests;

#[cfg(test)]
#[path = "booru_prepare_cache_tests.rs"]
mod booru_prepare_cache_tests;

#[cfg(test)]
#[path = "booru_path_results_tests.rs"]
mod booru_path_results_tests;

#[cfg(test)]
mod source_segment_limit_tests {
    use super::{Value, source_directory_segments};
    #[test]
    fn directory_iteration_counts_empty_items_and_rejects_nested_values() {
        let mut examined = 0;
        let empty = Value::List(vec![Value::Str(String::new()); 4096]);
        assert!(
            source_directory_segments(empty, &mut examined)
                .unwrap()
                .iter()
                .all(String::is_empty)
        );
        assert!(source_directory_segments(Value::Str(String::new()), &mut examined).is_err());
        assert!(source_directory_segments(Value::List(vec![Value::List(vec![])]), &mut 0).is_err());
        assert!(source_directory_segments(Value::None, &mut 0).is_err());
        assert!(
            source_directory_segments(Value::Missing, &mut 0)
                .unwrap()
                .is_empty()
        );
    }
}

#[cfg(test)]
#[path = "source_formatter_offset_tests.rs"]
mod source_formatter_offset_tests;
