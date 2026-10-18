//! Small, deliberately non-executable filename templates.
use crate::{Result, error, extract::Media};

const MAX_BYTES: usize = 240;

#[derive(Clone, Debug)]
enum Part {
    Literal(String),
    Field { name: String, width: usize },
}

/// A single-basename template. Fields use `{name}`; only `index` accepts a
/// zero-padded width, such as `{index:03}` or `{index:03d}`. Doubled braces
/// escape literal braces. Rendered names use the downloader's ASCII alphabet.
#[derive(Clone, Debug)]
pub struct FilenameTemplate {
    parts: Vec<Part>,
}

impl FilenameTemplate {
    pub fn parse(template: &str) -> Result<Self> {
        if template.is_empty() || matches!(template, "." | "..") {
            return Err(error("filename template must produce a basename"));
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
                    let (name, format) = field
                        .split_once(':')
                        .map_or((field.as_str(), None), |(name, format)| {
                            (name, Some(format))
                        });
                    if !matches!(
                        name,
                        "filename"
                            | "name"
                            | "extension"
                            | "category"
                            | "post_id"
                            | "media_id"
                            | "shortcode"
                            | "username"
                            | "index"
                            | "timestamp"
                    ) {
                        return Err(error(format!("unknown filename template field: {name}")));
                    }
                    let width = match format {
                        None => 0,
                        Some(format) if name == "index" => {
                            let digits = format.strip_suffix('d').unwrap_or(format);
                            if !digits.starts_with('0')
                                || digits.len() < 2
                                || !digits.bytes().all(|c| c.is_ascii_digit())
                            {
                                return Err(error(
                                    "index format must be a zero-padded width, such as 03",
                                ));
                            }
                            let width = digits
                                .parse::<usize>()
                                .map_err(|_| error("invalid index width"))?;
                            if !(1..=MAX_BYTES).contains(&width) {
                                return Err(error("index width must be between 1 and 240"));
                            }
                            width
                        }
                        Some(_) => return Err(error("only index supports filename formatting")),
                    };
                    parts.push(Part::Field {
                        name: name.into(),
                        width,
                    });
                }
                '}' => return Err(error("unmatched closing brace in filename template")),
                c => literal.push(c),
            }
        }
        if !literal.is_empty() {
            parts.push(Part::Literal(literal));
        }
        Ok(Self { parts })
    }

    pub fn render(&self, media: &Media) -> Result<String> {
        let mut output = String::new();
        for part in &self.parts {
            match part {
                Part::Literal(value) => output.push_str(value),
                Part::Field { name, width } => {
                    let (stem, extension) = media
                        .filename
                        .rsplit_once('.')
                        .filter(|(stem, ext)| !stem.is_empty() && !ext.is_empty())
                        .unwrap_or((&media.filename, ""));
                    let value = match name.as_str() {
                        "filename" => media.filename.clone(),
                        "name" => stem.to_string(),
                        "extension" => extension.to_string(),
                        "category" => media.category.clone(),
                        field => {
                            let metadata = media.metadata.as_ref().ok_or_else(|| {
                                error(format!("filename field {field} requires post metadata"))
                            })?;
                            match field {
                                "post_id" => metadata.post_id.clone(),
                                "media_id" => metadata.media_id.clone(),
                                "shortcode" => metadata.shortcode.clone(),
                                "username" => metadata.username.clone().unwrap_or_default(),
                                "timestamp" => metadata
                                    .timestamp
                                    .map(|v| v.to_string())
                                    .unwrap_or_default(),
                                "index" if metadata.index > 0 => {
                                    format!("{:0width$}", metadata.index, width = *width)
                                }
                                "index" => String::new(),
                                _ => unreachable!("validated at parse time"),
                            }
                        }
                    };
                    if value.is_empty() {
                        return Err(error(format!("filename field {name} is missing")));
                    }
                    // Sanitize each substitution independently so metadata can
                    // never introduce paths, drive prefixes, or control bytes.
                    output.extend(value.chars().map(safe_char));
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
