//! Non-executable filename and directory templates.
use crate::{Result, error, extract::Media};
use std::{collections::BTreeMap, path::PathBuf};
#[path = "naming_format.rs"]
mod format;
use format::{Field, Value};

const MAX_BYTES: usize = 240;
const MAX_TEMPLATE: usize = 8192;

/// Names a template may reference. Signed transport URLs are deliberately
/// unavailable so credentials cannot leak into paths.
const FIELDS: &[&str] = &[
    "filename",
    "name",
    "extension",
    "category",
    "post_id",
    "media_id",
    "shortcode",
    "username",
    "user",
    "service",
    "instance",
    "index",
    "num",
    "timestamp",
    "date",
    "caption",
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
    "frames",
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
        if conditions.len() > 32 {
            return Err(error("filename template has too many conditions"));
        }
        Ok(Self {
            parts: default.map(Self::parse).transpose()?.and_then(|t| t.parts),
            conditions: conditions
                .iter()
                .map(|(filter, template)| {
                    Ok((
                        crate::filter::FilterExpression::parse(filter)?,
                        Self::parse(template)?.parts.unwrap_or_default(),
                    ))
                })
                .collect::<Result<_>>()?,
        })
    }

    pub fn parse(template: &str) -> Result<Self> {
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
                    parts.push(Part::Field(Field::parse(&field, |name| {
                        FIELDS.contains(&name)
                    })?));
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
            Some(parts) => Self::render_parts(parts, media, &mut context).map(Some),
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

    fn render_with(
        &self,
        media: &Media,
        context: &mut Option<BTreeMap<String, crate::filter::Value>>,
    ) -> Result<String> {
        let parts = self
            .select(media)?
            .ok_or_else(|| error("no filename template condition matched"))?;
        Self::render_parts(parts, media, context)
    }

    fn render_parts(
        parts: &[Part],
        media: &Media,
        context: &mut Option<BTreeMap<String, crate::filter::Value>>,
    ) -> Result<String> {
        let mut output = String::new();
        for part in parts {
            match part {
                Part::Literal(value) => output.push_str(value),
                Part::Field(field) => {
                    let context = context.get_or_insert_with(|| crate::filter::context(media));
                    let value = field.render(|name| lookup(media, context, name))?;
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
                    // Sanitize each substitution independently so metadata can
                    // never introduce paths, drive prefixes, or control bytes.
                    output.extend(value.chars().map(safe_char));
                    if output.len() > 65536 {
                        return Err(error("filename template output exceeds limits"));
                    }
                }
            }
        }
        let output: String = output.chars().map(safe_char).collect();
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

/// Ordered component templates relative to the configured base directory.
/// An empty list selects the base directory itself. Substitutions cannot add
/// separators, absolute prefixes, or parent-directory components.
#[derive(Clone, Debug)]
pub struct DirectoryTemplate {
    components: Option<Vec<FilenameTemplate>>,
    conditions: Vec<(crate::filter::FilterExpression, Vec<FilenameTemplate>)>,
}

impl DirectoryTemplate {
    /// Parse `(filter, components)` pairs in order with an optional default.
    pub fn parse_conditional(
        conditions: &[(String, Vec<String>)],
        default: Option<&[String]>,
    ) -> Result<Self> {
        if conditions.len() > 32 {
            return Err(error("directory template has too many conditions"));
        }
        Ok(Self {
            components: default
                .map(Self::parse)
                .transpose()?
                .and_then(|t| t.components),
            conditions: conditions
                .iter()
                .map(|(filter, components)| {
                    Ok((
                        crate::filter::FilterExpression::parse(filter)?,
                        Self::parse(components)?.components.unwrap_or_default(),
                    ))
                })
                .collect::<Result<_>>()?,
        })
    }

    pub fn parse(components: &[String]) -> Result<Self> {
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
                    .map(|s| FilenameTemplate::parse(s))
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
        for component in components {
            let component = component.render_with(media, &mut context)?;
            bytes += component.len() + 1;
            if bytes > 3072 {
                return Err(error("rendered directory exceeds 3072 bytes"));
            }
            output.push(component);
        }
        Ok(Some(output))
    }
}

fn convert(value: &crate::filter::Value, key: &str) -> Value {
    convert_value(value, key, key != "tags")
}

fn convert_value(value: &crate::filter::Value, key: &str, dates: bool) -> Value {
    use crate::filter::Value as Source;
    match value {
        Source::None => Value::None,
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
                        convert_value(value, key, dates && key != "tags"),
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
    Some(match name {
        // Unlike upstream, `filename` includes the extension here.
        "filename" => Value::Str(media.filename.clone()),
        "name" => Value::Str(stem.into()),
        "extension" => Value::Str(extension.into()),
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
