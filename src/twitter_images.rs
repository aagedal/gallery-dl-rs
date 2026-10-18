//! Ordered image size requests, kept separate from durable social file identity.
use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageOptions {
    sizes: Vec<String>,
}
impl Default for ImageOptions {
    fn default() -> Self {
        Self {
            sizes: ["orig", "4096x4096", "large", "medium", "small"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
        }
    }
}
impl ImageOptions {
    pub fn new(sizes: Vec<String>) -> Result<Self> {
        if sizes.is_empty()
            || sizes.len() > 32
            || sizes.iter().any(|size| {
                size.is_empty()
                    || size.len() > 32
                    || !size
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
            })
        {
            return Err(error("Twitter image sizes invalid"));
        }
        Ok(Self { sizes })
    }
    pub fn sizes(&self) -> &[String] {
        &self.sizes
    }

    pub(super) fn primary(&self, mut url: Url) -> Url {
        let pairs: Vec<_> = url
            .query_pairs()
            .filter(|(key, _)| key != "name")
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        url.set_query(None);
        url.query_pairs_mut()
            .extend_pairs(pairs)
            .append_pair("name", &self.sizes[0]);
        url
    }

    pub(super) fn apply(&self, mut url: Url) -> (Url, Vec<Url>) {
        let pairs: Vec<_> = url
            .query_pairs()
            .filter(|(key, _)| key != "name")
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        url.set_query(None);
        url.query_pairs_mut()
            .extend_pairs(&pairs)
            .append_pair("name", &self.sizes[0]);
        let fallback = self.sizes[1..]
            .iter()
            .map(|size| {
                let mut target = url.clone();
                target.set_query(None);
                target
                    .query_pairs_mut()
                    .extend_pairs(&pairs)
                    .append_pair("name", size);
                target
            })
            .collect();
        (url, fallback)
    }
}
