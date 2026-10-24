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

/// First-party `pbs.twimg.com/media/ID?format=EXT` or `/media/ID.EXT` file URLs.
pub fn is_image_url(url: &Url) -> bool {
    image_input(url).is_some()
}

pub(super) fn image_input(url: &Url) -> Option<(String, String)> {
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str() != Some("pbs.twimg.com")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return None;
    }
    let word = |value: &str, dash: bool| {
        !value.is_empty()
            && value.len() <= 64
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || dash && b == b'-')
    };
    let name = url.path().strip_prefix("/media/")?;
    let (id, format) = match name.split_once('.') {
        // Legacy `ID.jpg:large` names keep only the format before the size.
        Some((id, extension)) => (id, extension.split(':').next()?.to_owned()),
        None => {
            let mut formats = url
                .query_pairs()
                .filter(|(key, _)| key == "format")
                .map(|(_, value)| value.into_owned());
            let format = formats.next()?;
            if formats.next().is_some() {
                return None;
            }
            (name, format)
        }
    };
    (word(id, true) && word(&format, false)).then(|| (id.to_owned(), format))
}

/// One file with the configured size chain, as upstream TwitterImageExtractor.
pub(super) fn image_media(id: &str, format: &str, options: &ImageOptions) -> Result<Media> {
    let base = Url::parse(&format!("https://pbs.twimg.com/media/{id}?format={format}"))
        .map_err(|_| error("Twitter image URL invalid"))?;
    let (url, fallback) = options.apply(base);
    Ok(Media {
        url,
        job: crate::extract::MediaJob::DirectHttp,
        category: "twitter".into(),
        filename: format!("{id}.{format}"),
        metadata: None,
        headers: BTreeMap::from([
            ("User-Agent".into(), USER_AGENT.into()),
            ("Referer".into(), "https://x.com/".into()),
        ]),
        handoff: if fallback.is_empty() {
            BTreeMap::new()
        } else {
            BTreeMap::from([("_fallback".into(), serde_json::json!(fallback))])
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_urls_follow_upstream_pattern_and_reject_other_hosts() {
        for (url, id, format) in [
            (
                "https://pbs.twimg.com/media/ABC-d_1?format=jpg&name=orig",
                "ABC-d_1",
                "jpg",
            ),
            ("https://pbs.twimg.com/media/ABC?format=png", "ABC", "png"),
            ("http://pbs.twimg.com/media/ABC.jpg", "ABC", "jpg"),
            ("https://pbs.twimg.com/media/ABC.jpg:large", "ABC", "jpg"),
            (
                "https://pbs.twimg.com/media/ABC?name=small&format=webp",
                "ABC",
                "webp",
            ),
        ] {
            let url = Url::parse(url).unwrap();
            assert!(is_image_url(&url));
            assert_eq!(image_input(&url), Some((id.into(), format.into())));
        }
        for url in [
            "https://pbs.twimg.com/media/ABC",
            "https://pbs.twimg.com/media/ABC?format=jpg&format=png",
            "https://pbs.twimg.com/media/ABC?format=j%2Fg",
            "https://pbs.twimg.com/media/A/B.jpg",
            "https://pbs.twimg.com/media/.jpg",
            "https://pbs.twimg.com/profile_images/1/a.jpg",
            "https://pbs.twimg.com:8443/media/ABC.jpg",
            "https://user@pbs.twimg.com/media/ABC.jpg",
            "https://video.twimg.com/media/ABC.jpg",
            "https://pbs.twimg.com.evil/media/ABC.jpg",
            "ftp://pbs.twimg.com/media/ABC.jpg",
        ] {
            assert!(!is_image_url(&Url::parse(url).unwrap()), "{url}");
        }
    }

    #[test]
    fn image_media_uses_configured_size_chain_as_fallbacks() {
        let media = image_media("ABC", "jpg", &ImageOptions::default()).unwrap();
        assert_eq!(
            media.url.as_str(),
            "https://pbs.twimg.com/media/ABC?format=jpg&name=orig"
        );
        assert_eq!(media.filename, "ABC.jpg");
        assert_eq!(media.category, "twitter");
        assert_eq!(
            media.handoff["_fallback"],
            serde_json::json!([
                "https://pbs.twimg.com/media/ABC?format=jpg&name=4096x4096",
                "https://pbs.twimg.com/media/ABC?format=jpg&name=large",
                "https://pbs.twimg.com/media/ABC?format=jpg&name=medium",
                "https://pbs.twimg.com/media/ABC?format=jpg&name=small",
            ])
        );
        let single = ImageOptions::new(vec!["large".into()]).unwrap();
        let media = image_media("ABC", "png", &single).unwrap();
        assert_eq!(
            media.url.as_str(),
            "https://pbs.twimg.com/media/ABC?format=png&name=large"
        );
        assert!(media.handoff.is_empty());
    }
}
