//! Upstream's Danbooru `random`/`media-asset`, e621 short-link/`frontend`
//! and Gelbooru `redirect` classes.
use super::*;
use crate::event::{ChildMetadata, child_job};
use base64::Engine;

/// Classify the routes this module handles.
pub(super) fn capture(site: Site, url: &Url) -> Option<Target> {
    let host = url.host_str()?.trim_start_matches("www.");
    let path = url.path();
    match site.dialect() {
        Dialect::Danbooru => {
            if path == "/posts/random" {
                // `(?:[^&#]*&)*tags=([^&#]*)`: the greedy prefix picks the last pair.
                let raw = url
                    .query()
                    .unwrap_or_default()
                    .split('&')
                    .filter_map(|pair| pair.strip_prefix("tags="))
                    .next_back()
                    .unwrap_or_default();
                return Some(Target::Random {
                    tags: crate::sites::percent_decode(&raw.replace('+', " ")),
                });
            }
            let rest = path.strip_prefix("/media_assets/")?;
            let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
            post_id(&rest[..digits]).map(|id| Target::MediaAsset { id })
        }
        Dialect::E621 => {
            let frontend = match host {
                "e621.cc" if path == "/" => url.query()?.strip_prefix("tags="),
                "e621.anthro.fr" if path == "/" => url.query()?.strip_prefix("q="),
                _ => None,
            };
            if let Some(raw) = frontend {
                let raw = raw.split('&').next().unwrap_or_default();
                return Some(Target::Frontend { tags: raw.into() });
            }
            // `/p/(\w+)`: a base-32 post ID.
            let short = path.strip_prefix("/p/")?;
            let short = short.split('/').next().unwrap_or_default();
            u64::from_str_radix(short, 32)
                .ok()
                .filter(|id| *id > 0 && short.len() <= 13)
                .map(|id| Target::Post { id })
        }
        Dialect::Gelbooru => {
            if path != "/redirect.php" {
                return None;
            }
            let raw = url.query()?.strip_prefix("s=")?.split('&').next()?;
            Some(Target::Redirect {
                encoded: raw.into(),
            })
        }
        Dialect::Safebooru => None,
    }
}

/// `binascii.a2b_base64` (non-strict: characters outside the alphabet are
/// ignored) followed by `text.ensure_http_scheme`.
fn redirect_url(encoded: &str) -> Result<Url> {
    let invalid = || error("invalid Gelbooru redirect");
    let decoded = crate::sites::percent_decode(encoded);
    let mut alphabet: String = decoded
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/'))
        .collect();
    while !alphabet.len().is_multiple_of(4) {
        alphabet.push('=');
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(alphabet)
        .map_err(|_| invalid())?;
    let text = String::from_utf8(bytes).map_err(|_| invalid())?;
    let text = text.trim();
    let text = if text.starts_with("http://") || text.starts_with("https://") {
        text.to_owned()
    } else {
        format!("https://{}", text.trim_start_matches("//"))
    };
    Url::parse(&text).map_err(|_| invalid())
}

impl Booru {
    /// Queue-only classes: e621 frontends and Gelbooru redirects.
    pub(super) fn queue_events(&self, target: Target) -> BoxStream<'_, Result<ExtractionEvent>> {
        let (category, subcategory, url) = match target {
            Target::Frontend { tags } => (
                "e621",
                "frontend",
                Url::parse(&format!("https://e621.net/posts?tags={tags}"))
                    .map_err(|_| error("invalid e621 frontend tags")),
            ),
            Target::Redirect { encoded } => ("gelbooru", "redirect", redirect_url(&encoded)),
            _ => unreachable!("queue-only booru target"),
        };
        match url {
            Ok(url) => child_job(
                ChildMetadata::from_raw(category, subcategory, &serde_json::Map::new()),
                move || self.events(&url),
            ),
            Err(failure) => stream::once(async move { Err(failure) }).boxed(),
        }
    }

    /// Danbooru `/posts/random.json` (one post, or a list) for the search.
    pub(super) async fn random(&self, root: &Url, tags: &str) -> Result<Vec<Value>> {
        let query: &[(&str, &str)] = if tags.is_empty() {
            &[]
        } else {
            &[("tags", tags)]
        };
        let body = self.call(root, "posts/random.json", query).await?;
        posts(self.site, &body)
    }

    /// Danbooru `/media_assets/ID.json`, shaped like a post: its last
    /// variant is the file and its tag strings are empty.
    pub(super) async fn media_asset(&self, root: &Url, id: u64) -> Result<Vec<Value>> {
        let endpoint = format!("media_assets/{id}.json");
        let body = self.call(root, &endpoint, &[]).await?;
        let mut asset: Value = serde_json::from_slice(&body).map_err(|_| invalid(self.site))?;
        if number(asset.get("id")) != Some(id) {
            return Err(error("Danbooru media asset not found"));
        }
        let url = asset
            .get("variants")
            .and_then(Value::as_array)
            .and_then(|variants| variants.last())
            .and_then(|variant| variant.get("url"))
            .cloned()
            .ok_or_else(|| error("Danbooru media asset has no file"))?;
        asset["file_url"] = url;
        for key in [
            "tag_string",
            "tag_string_artist",
            "tag_string_character",
            "tag_string_copyright",
            "tag_string_general",
            "tag_string_meta",
        ] {
            asset[key] = "".into();
        }
        let mut raw = vec![asset];
        self.enrich(root, &endpoint, true, &mut raw).await?;
        Ok(raw)
    }
}
