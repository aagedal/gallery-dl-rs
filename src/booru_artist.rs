//! Artist discovery produces independent, lazy tag-search jobs.
use super::*;
use crate::event::{ChildMetadata, child_job};

pub(super) fn capture(site: Site, url: &Url) -> Option<Target> {
    if !site.danbooru_dialect() {
        return None;
    }
    if let Some(rest) = url.path().strip_prefix("/artists/") {
        let count = rest.bytes().take_while(u8::is_ascii_digit).count();
        if count != 0 {
            return Some(Target::Artist {
                id: rest[..count].into(),
            });
        }
    }
    if matches!(url.path(), "/artists" | "/artists/") {
        let query = url.query().filter(|query| !query.is_empty())?;
        let mut seen = std::collections::BTreeSet::new();
        return Some(Target::ArtistSearch {
            params: query
                .split('&')
                .filter(|pair| pair.contains('='))
                .filter_map(|pair| url::form_urlencoded::parse(pair.as_bytes()).next())
                .filter_map(|(key, value)| {
                    seen.insert(key.clone().into_owned())
                        .then(|| (key.into_owned(), value.into_owned()))
                })
                .collect(),
        });
    }
    None
}

// Keep Python's decimal integer spelling without limiting pages to machine ints.
fn page(value: Option<&str>) -> String {
    fn parse(value: &str) -> Option<String> {
        let mut chars = value.trim_matches(char::is_whitespace).chars().peekable();
        let negative = match chars.peek() {
            Some('+') => {
                chars.next();
                false
            }
            Some('-') => {
                chars.next();
                true
            }
            _ => false,
        };
        let mut digits = String::new();
        let mut previous = false;
        for c in chars {
            if c == '_' {
                if !previous {
                    return None;
                }
                previous = false;
            } else {
                digits.push(char::from(b'0' + crate::python_unicode::decimal_digit(c)?));
                previous = true;
            }
        }
        if !previous || digits.len() > 4300 {
            return None;
        }
        let digits = digits.trim_start_matches('0');
        Some(if digits.is_empty() {
            "0".into()
        } else if negative {
            format!("-{digits}")
        } else {
            digits.into()
        })
    }
    value.and_then(parse).unwrap_or_else(|| "1".into())
}
fn increment(value: &mut String) {
    let negative = value.starts_with('-');
    let mut digits = value.trim_start_matches('-').as_bytes().to_vec();
    for digit in digits.iter_mut().rev() {
        if negative {
            if *digit != b'0' {
                *digit -= 1;
                break;
            }
            *digit = b'9';
        } else {
            if *digit != b'9' {
                *digit += 1;
                break;
            }
            *digit = b'0';
        }
    }
    if !negative && digits.iter().all(|digit| *digit == b'0') {
        digits.insert(0, b'1');
    }
    let digits = String::from_utf8(digits).expect("decimal page");
    let digits = digits.trim_start_matches('0');
    *value = if digits.is_empty() {
        "0".into()
    } else if negative {
        format!("-{digits}")
    } else {
        digits.into()
    };
}
fn quote(name: &str) -> String {
    let mut output = String::new();
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
            output.push(char::from(byte));
        } else {
            use std::fmt::Write;
            write!(output, "%{byte:02X}").expect("string output");
        }
    }
    output
}
struct Artists {
    detail: Option<String>,
    params: Vec<(String, String)>,
    pending: VecDeque<Value>,
    done: bool,
    started: bool,
}
impl Artists {
    async fn next(&mut self, booru: &Booru, root: &Url) -> Result<Option<Value>> {
        loop {
            if let Some(value) = self.pending.pop_front() {
                return Ok(Some(value));
            }
            if self.done {
                return Ok(None);
            }
            if let Some(id) = self.detail.take() {
                self.done = true;
                let bytes = booru.call(root, &format!("artists/{id}.json"), &[]).await?;
                return serde_json::from_slice(&bytes)
                    .map(Some)
                    .map_err(|_| error("invalid booru artist response"));
            }
            if self.started {
                let page = &mut self
                    .params
                    .iter_mut()
                    .find(|(key, _)| key == "page")
                    .expect("artist page")
                    .1;
                increment(page);
            }
            self.started = true;
            let query: Vec<_> = self
                .params
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            let bytes = booru.call(root, "artists.json", &query).await?;
            let response: Value = serde_json::from_slice(&bytes)
                .map_err(|_| error("invalid booru artist response"))?;
            let rows = match response {
                Value::Array(rows) => rows,
                Value::Object(rows) if rows.is_empty() => Vec::new(),
                Value::String(rows) if rows.is_empty() => Vec::new(),
                _ => return Err(error("invalid booru artist search response")),
            };
            self.done = rows.len() < 20;
            self.pending = rows.into();
        }
    }
}
pub(super) fn events(
    booru: &Booru,
    target: Target,
    public_root: Url,
) -> BoxStream<'_, Result<ExtractionEvent>> {
    let (detail, mut params, subcategory) = match target {
        Target::Artist { id } => (Some(id), Vec::new(), "artist"),
        Target::ArtistSearch { params } => (None, params, "artist-search"),
        _ => unreachable!(),
    };
    if detail.is_none() {
        let initial = page(
            params
                .iter()
                .find(|(key, _)| key == "page")
                .map(|(_, v)| v.as_str()),
        );
        if let Some((_, value)) = params.iter_mut().find(|(key, _)| key == "page") {
            *value = initial;
        } else {
            params.push(("page".into(), initial));
        }
    }
    let root = booru
        .api_root
        .clone()
        .unwrap_or_else(|| public_root.clone());
    let category = booru.site.category();
    let state = Artists {
        detail,
        params,
        pending: VecDeque::new(),
        done: false,
        started: false,
    };
    stream::unfold(Some(state), move |state| {
        let root = root.clone();
        let public_root = public_root.clone();
        async move {
            let mut state = state?;
            let row = match state.next(booru, &root).await {
                Ok(Some(row)) => row,
                Ok(None) => return None,
                Err(failure) => return Some((Err(failure), None)),
            };
            let child = (|| {
                let fields = row
                    .as_object()
                    .ok_or_else(|| error("invalid booru artist"))?;
                let name = fields
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| error("invalid booru artist name"))?;
                let url = public_root
                    .join(&format!("posts?tags={}", quote(name)))
                    .map_err(|_| error("invalid booru artist URL"))?;
                let mut metadata =
                    ChildMetadata::from_booru_artist(category, subcategory, fields, &url)?;
                if let Some(value) = &booru.artist_parent_metadata {
                    metadata = metadata.with_booru_artist_inheritance(value)?;
                }
                Ok(child_job(metadata, move || booru.events(&url)))
            })();
            match child {
                Ok(child) => Some((Ok(child), Some(state))),
                Err(failure) => Some((Err(failure), None)),
            }
        }
    })
    .map(|result| match result {
        Ok(child) => child,
        Err(failure) => stream::once(async move { Err(failure) }).boxed(),
    })
    .flatten()
    .boxed()
}
