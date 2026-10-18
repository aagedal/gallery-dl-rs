//! Explicit card bindings; embedded unified media preserve their JSON source order.
use super::*;
use crate::extract::{MediaJob, direct};
use media::SelectedFile;
use serde::{
    Deserialize, Deserializer,
    de::{MapAccess, Visitor},
};
use std::collections::{BTreeMap, HashSet};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CardMode {
    /// Ignore card payloads.
    #[default]
    Skip,
    /// Extract recognized summary and unified-card media.
    Files,
    /// Extract recognized cards, delegating unhandled cards as typed video-page jobs.
    Ytdl,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CardOptions {
    pub mode: CardMode,
    /// Exact card names, vanity domains, or `name:domain` pairs to skip.
    pub blacklist: Vec<String>,
}

pub(super) fn select(
    tweet: &Value,
    output_id: &str,
    media_options: MediaOptions,
    options: &CardOptions,
) -> Result<Vec<SelectedFile>> {
    if options.mode == CardMode::Skip {
        return Ok(Vec::new());
    }
    let Some(card) = tweet.get("card") else {
        return Ok(Vec::new());
    };
    let card = card.get("legacy").unwrap_or(card);
    let name = card
        .get("name")
        .and_then(Value::as_str)
        .and_then(|name| name.rsplit(':').next())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| error("Twitter card name missing or invalid"))?;
    if options.blacklist.iter().any(|entry| entry == name) {
        return Ok(Vec::new());
    }
    let bindings = bindings(card.get("binding_values"))?;
    if !options.blacklist.is_empty()
        && let Some(vanity) = bindings.get("vanity_url")
    {
        let domain = vanity
            .get("string_value")
            .and_then(Value::as_str)
            .ok_or_else(|| error("Twitter card domain binding invalid"))?;
        if options
            .blacklist
            .iter()
            .any(|entry| entry == domain || entry == &format!("{name}:{domain}"))
        {
            return Ok(Vec::new());
        }
    }
    if matches!(name, "summary" | "summary_large_image") {
        for prefix in [
            "photo_image_full_size_",
            "summary_photo_image_",
            "thumbnail_image_",
        ] {
            for size in ["original", "x_large", "large", "small"] {
                let key = format!("{prefix}{size}");
                if let Some(url) = bindings
                    .get(&key)
                    .and_then(|value| value.get("image_value"))
                    .and_then(|image| image.get("url"))
                {
                    let url = url
                        .as_str()
                        .ok_or_else(|| error("Twitter card image URL invalid"))?;
                    return Ok(vec![image_file(url, true)?]);
                }
            }
        }
    } else if name == "unified_card" {
        let encoded = bindings
            .get("unified_card")
            .and_then(|value| value.get("string_value"))
            .and_then(Value::as_str)
            .ok_or_else(|| error("Twitter unified card data missing or invalid"))?;
        if encoded.len() > MAX_RESPONSE {
            return Err(error("Twitter unified card exceeded 16 MiB"));
        }
        let data: Unified = serde_json::from_str(encoded)
            .map_err(|_| error("Twitter unified card data invalid"))?;
        let mut selected =
            media::select_entities(data.media_entities.0.iter(), output_id, media_options)?;
        if let Some(objects) = data.component_objects {
            let ids = data
                .components
                .ok_or_else(|| error("Twitter card component order missing"))?;
            let mut seen = HashSet::new();
            for id in ids {
                if !seen.insert(id.clone()) {
                    return Err(error("Twitter card component identifiers repeated"));
                }
                let data = objects
                    .get(&id)
                    .and_then(|object| object.get("data"))
                    .filter(|data| data.is_object())
                    .ok_or_else(|| error("Twitter card component data missing or invalid"))?;
                let Some(previews) = data
                    .get("conversation_preview")
                    .filter(|value| !value.is_null())
                else {
                    continue;
                };
                let previews = previews
                    .as_array()
                    .ok_or_else(|| error("Twitter card conversation previews invalid"))?;
                for preview in previews {
                    if !preview.is_object() {
                        return Err(error("Twitter card conversation preview invalid"));
                    }
                    let Some(urls) = preview.get("mediaUrls").filter(|value| !value.is_null())
                    else {
                        continue;
                    };
                    for url in urls
                        .as_array()
                        .ok_or_else(|| error("Twitter card preview URLs invalid"))?
                    {
                        let url = url
                            .as_str()
                            .ok_or_else(|| error("Twitter card preview URL invalid"))?;
                        selected.push(component_file(url)?);
                    }
                }
            }
        }
        return Ok(selected);
    }
    if options.mode == CardMode::Ytdl {
        let page_url = Url::parse(&format!("https://x.com/i/web/status/{output_id}"))?;
        return Ok(vec![(
            Url::parse(&format!("ytdl:{page_url}"))?,
            "mp4".into(),
            MediaJob::VideoPage { page_url },
        )]);
    }
    Ok(Vec::new())
}

fn bindings(value: Option<&Value>) -> Result<BTreeMap<String, Value>> {
    match value {
        Some(Value::Object(values)) => {
            Ok(values.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        }
        Some(Value::Array(values)) => {
            let mut bindings = BTreeMap::new();
            for value in values {
                let key = value
                    .get("key")
                    .and_then(Value::as_str)
                    .ok_or_else(|| error("Twitter card binding key invalid"))?;
                let value = value
                    .get("value")
                    .ok_or_else(|| error("Twitter card binding value missing"))?;
                if bindings.insert(key.into(), value.clone()).is_some() {
                    return Err(error("Twitter card binding keys repeated"));
                }
            }
            Ok(bindings)
        }
        _ => Err(error("Twitter card bindings missing or invalid")),
    }
}

fn image_file(source: &str, normalize: bool) -> Result<SelectedFile> {
    let mut url = http_url(source).map_err(|_| error("Twitter card image URL invalid"))?;
    let supported = |ext: &str| matches!(ext, "jpg" | "jpeg" | "png" | "webp" | "avif" | "gif");
    let extension = url
        .path()
        .rsplit_once('.')
        .map(|(_, ext)| ext)
        .filter(|ext| supported(ext))
        .map(str::to_owned)
        .or_else(|| {
            url.query_pairs()
                .find(|(key, value)| key == "format" && supported(value))
                .map(|(_, value)| value.into_owned())
        })
        .ok_or_else(|| error("Twitter card image format unsupported"))?;
    // Card images without an explicit size keep the supplied URL.
    if normalize && url.query_pairs().any(|(key, _)| key == "name") {
        let pairs: Vec<_> = url
            .query_pairs()
            .filter(|(key, _)| key != "name")
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        url.set_query(None);
        url.query_pairs_mut()
            .extend_pairs(pairs)
            .append_pair("name", "orig");
    }
    Ok((url, extension, MediaJob::DirectHttp))
}

fn component_file(source: &str) -> Result<SelectedFile> {
    let url = http_url(source).map_err(|_| error("Twitter card preview URL invalid"))?;
    if let Some(media) = direct(&url) {
        let extension = media.filename.rsplit_once('.').unwrap().1.to_owned();
        return Ok((url, extension, media.job));
    }
    image_file(source, false)
}

#[derive(Deserialize)]
struct Unified {
    #[serde(default)]
    media_entities: OrderedMedia,
    components: Option<Vec<String>>,
    component_objects: Option<BTreeMap<String, Value>>,
}

#[derive(Default)]
struct OrderedMedia(Vec<Value>);
impl<'de> Deserialize<'de> for OrderedMedia {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct Ordered;
        impl<'de> Visitor<'de> for Ordered {
            type Value = OrderedMedia;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("an ordered media object")
            }
            fn visit_map<M: MapAccess<'de>>(
                self,
                mut map: M,
            ) -> std::result::Result<Self::Value, M::Error> {
                let mut seen = HashSet::new();
                let mut values = Vec::new();
                while let Some((key, value)) = map.next_entry::<String, Value>()? {
                    if !seen.insert(key) {
                        return Err(serde::de::Error::custom("repeated card media key"));
                    }
                    values.push(value);
                }
                Ok(OrderedMedia(values))
            }
        }
        deserializer.deserialize_map(Ordered)
    }
}
