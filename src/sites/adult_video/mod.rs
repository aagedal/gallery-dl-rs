//! Adult video sites' image galleries, forums, boorus and stores.
//!
//! Videos that upstream hands to yt-dlp as `ytdl:` URLs become video-page
//! or HLS/DASH manifest jobs for the same downloader.
use crate::extract::{ManifestProtocol, MediaJob};
use crate::sites::{Item, Site};
use serde_json::{Map, Value};
use url::Url;

mod adultempire;
mod eporner;
mod fikfap;
mod girlswithmuscle;
mod motherless;
mod okporn;
mod pornhub;
mod pornstarstube;
mod scatbooru;
mod sexcom;
mod xasiat;
mod xhamster;
mod xvideos;
mod yourlesbians;

pub(super) static SITES: &[&Site] = &[
    &pornhub::SITE,
    &xvideos::SITE,
    &xhamster::SITE,
    &motherless::SITE,
    &sexcom::SITE,
    &okporn::SITE,
    &pornstarstube::SITE,
    &eporner::SITE,
    &yourlesbians::SITE,
    &adultempire::SITE,
    &scatbooru::SITE,
    &xasiat::SITE,
    &fikfap::SITE,
    &girlswithmuscle::SITE,
];

/// An upstream `ytdl:` URL with `_ytdl_manifest`: yt-dlp reads the HLS or
/// DASH manifest, and the file's URL keeps the `ytdl:` prefix.
pub(super) fn manifest_item(
    raw: &str,
    protocol: ManifestProtocol,
    keywords: Map<String, Value>,
) -> Option<Item> {
    let base_url = crate::sites::media::web_url(raw)?;
    let url = Url::parse(&format!("ytdl:{base_url}")).ok()?;
    Some(Item::new(url, keywords).job(MediaJob::Manifest {
        protocol,
        base_url,
        inline_data: None,
    }))
}

/// The `[\w-]+` prefix of `text` (Python `\w` is Unicode-aware).
pub(super) fn slug(text: &str) -> &str {
    let end = text
        .find(|c: char| !(crate::sites::media::word_char(c) || c == '-'))
        .unwrap_or(text.len());
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_items_keep_the_ytdl_prefix() {
        let item = manifest_item(
            "https://cdn.test/v/master.m3u8",
            ManifestProtocol::Hls,
            Map::new(),
        )
        .unwrap();
        assert_eq!(item.url.as_str(), "ytdl:https://cdn.test/v/master.m3u8");
        assert!(matches!(
            item.job,
            MediaJob::Manifest { protocol: ManifestProtocol::Hls, ref base_url, .. }
                if base_url.as_str() == "https://cdn.test/v/master.m3u8"
        ));
        assert!(
            manifest_item("http://u:p@cdn.test/", ManifestProtocol::Dash, Map::new()).is_none()
        );
        assert_eq!(slug("a-b_c/d"), "a-b_c");
    }
}
