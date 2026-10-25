//! Upstream extractor archive *format descriptors*, independent of native keys.
//!
//! A descriptor describes the job's extractor class, not the eventual media's
//! identity. Empty formats are real inherited `Extractor.archive_fmt` values.
use crate::{event::ChildMetadata, extract::Media};
use url::Url;

#[derive(Clone, PartialEq, Eq)]
pub struct ArchiveDescriptor {
    pub category: String,
    pub format: String,
}
impl std::fmt::Debug for ArchiveDescriptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ArchiveDescriptor { category: [redacted], format: [redacted] }")
    }
}
fn descriptor(category: &str, format: &str) -> ArchiveDescriptor {
    ArchiveDescriptor {
        category: category.into(),
        format: format.into(),
    }
}
const PIXIV: &str = "{id}{suffix}.{extension}";
const FAVORITE: &str = "f_{user_bookmark[id]}_{id}{num}.{extension}";
const RANKING: &str = "r_{ranking[mode]}_{ranking[date]}_{id}{num}.{extension}";
const SEARCH: &str = "s_{search[word]}_{id}{num}.{extension}";
const FOLLOWED: &str = "F_{user_follow[id]}_{id}{num}.{extension}";
const PIXIVISION: &str = "V{pixivision_id}_{id}{suffix}.{extension}";
const TWITTER: &str = "{tweet_id}_{retweet_id}_{num}";
const COMMUNITY: &str = "C_{community[id]}_{tweet_id}_{num}";
const KEMONO: &str = "{service}_{user}_{id}_{num}";
const DISCORD: &str = "discord_{server_id}_{id}_{num}";

/// Source queue-only roots cannot use their own empty format for files emitted
/// by a flattened native implementation. Their child job formats are distinct;
/// restoring those boundaries is required before enabling default archives.
pub(crate) fn native_framing_supported(descriptor: &ArchiveDescriptor) -> bool {
    !(descriptor.format.is_empty()
        && matches!(descriptor.category.as_str(), "imgur" | "deviantart"))
}

/// Whether the native producer schema preserves every keyword needed by this
/// source default format. This is independent of an eager/Directory snapshot
/// which may legitimately precede the first file's values. Explicit user
/// archive formats bypass this policy; unrecognized library categories retain
/// their caller-supplied keyword contract.
pub(crate) fn default_keywords_supported(descriptor: &ArchiveDescriptor) -> bool {
    if !native_framing_supported(descriptor) {
        return false;
    }
    let format = descriptor.format.as_str();
    if format.is_empty() {
        return true; // A real inherited source default needs no keyword lookup.
    }
    match descriptor.category.as_str() {
        "pixiv" => matches!(
            format,
            PIXIV
                | FAVORITE
                | RANKING
                | SEARCH
                | FOLLOWED
                | PIXIVISION
                | "S{user[id]}_{id}"
                | "avatar_{user[id]}_{date}"
                | "background_{user[id]}_{date}"
        ),
        "pixiv-novel" => format == PIXIV,
        "artstation" => format == "{asset[id]}",
        "twitter" => matches!(format, TWITTER | COMMUNITY),
        "kemono" | "coomer" => matches!(format, KEMONO | DISCORD),
        "danbooru" if format == "fg_{favgroup[id]}_{id}" => true,
        "e621" | "e926" if format == "f_{user_id}_{id}" => true,
        "danbooru" | "e621" | "e926" if format == "P_{scale[0]}_{date}_{id}" => true,
        "danbooru" | "e621" | "e926" => {
            matches!(
                format,
                "{id}" | "t_{search_tags}_{id}" | "p_{pool[id]}_{id}"
            )
        }
        "gelbooru" | "safebooru" if format == "f_{favorite_id}_{id}" => true,
        "gelbooru" | "safebooru" => {
            matches!(format, "{id}" | "t_{search_tags}_{id}" | "p_{pool}_{id}")
        }
        "flickr" | "wallhaven" | "imgur" | "furaffinity" | "vsco" | "pixeldrain" => {
            format == "{id}"
        }
        "instagram" => format == "{media_id}",
        "facebook" => format == "{id}.{extension}",
        "weasyl" => format == "{submitid}",
        // Source filename is often unrelated to the native logical filename.
        // Album, search, owner, and per-source index fields were not retained.
        "bluesky" | "reddit" | "tumblr" | "deviantart" | "pinterest" | "newgrounds" | "catbox"
        | "imgbox" | "postimg" | "mastodon" | "mastodon.social" | "pawoo" | "baraag" | "direct"
        | "directlink" => false,
        _ => true,
    }
}

/// Descriptor for a supported native root route. The URL determines the source
/// extractor even when several native routes share an implementation.
pub fn for_url(url: &Url) -> Option<ArchiveDescriptor> {
    let path = url.path().trim_end_matches('/');
    if let Ok(target) = crate::pixiv::target(url) {
        use crate::pixiv::{ProfileKind, Target};
        let (category, format) = match target {
            Target::Novel(_) => ("pixiv-novel", PIXIV),
            Target::Profile {
                kind: ProfileKind::Avatar,
                ..
            } => ("pixiv", "avatar_{user[id]}_{date}"),
            Target::Profile {
                kind: ProfileKind::Background,
                ..
            } => ("pixiv", "background_{user[id]}_{date}"),
            Target::Bookmarks { .. } | Target::Following { .. } => ("pixiv", FAVORITE),
            Target::Ranking(_) => ("pixiv", RANKING),
            Target::Search(_) => ("pixiv", SEARCH),
            Target::Followed => ("pixiv", FOLLOWED),
            Target::Pixivision(_) => ("pixiv", PIXIVISION),
            Target::Sketch(_) => ("pixiv", "S{user[id]}_{id}"),
            _ => ("pixiv", PIXIV),
        };
        return Some(descriptor(category, format));
    }
    if crate::twitter::post_id(url).is_ok()
        || crate::twitter::is_profile_timeline_url(url)
        || crate::twitter::is_collection_url(url)
        || crate::twitter::is_search_url(url)
    {
        let format = if path.starts_with("/i/communities") {
            COMMUNITY
        } else if path.ends_with("/header_photo") {
            "BG_{user[id]}_{date}"
        } else if path.ends_with("/photo") {
            "AV_{user[id]}_{date}"
        } else {
            TWITTER
        };
        return Some(descriptor("twitter", format));
    }
    if crate::bluesky::target(url).is_ok() {
        return Some(descriptor("bluesky", "{filename}"));
    }
    if crate::reddit::target(url).is_ok() {
        return Some(descriptor("reddit", "{filename}"));
    }
    if crate::mastodon::target(url).is_ok() {
        return Some(descriptor(
            crate::mastodon::instance_category(url.host_str()?)?,
            "{media[id]}",
        ));
    }
    if crate::tumblr::target(url).is_ok() {
        return Some(descriptor("tumblr", "{id}_{num}"));
    }
    if let Ok(target) = crate::kemono::target(url) {
        let category = crate::kemono::category(url.host_str()?)?;
        let format = if matches!(
            target,
            crate::kemono::Target::Discord {
                channel: Some(_),
                ..
            }
        ) {
            DISCORD
        } else {
            KEMONO
        };
        return Some(descriptor(category, format));
    }
    if let Ok(target) = crate::imgur::target(url) {
        let format = match target {
            crate::imgur::Target::Image(_) => "{id}",
            crate::imgur::Target::Album(_) => "{album[id]}_{id}",
            // Galleries and listings are queue-only; their format is the
            // inherited empty default.
            _ => "",
        };
        return Some(descriptor("imgur", format));
    }
    if let Ok(target) = crate::deviantart::target(url) {
        use crate::deviantart::Target;
        let format = match target {
            Target::User(_) => "",
            Target::Folder { .. } => "F_{folder[uuid]}_{index}.{extension}",
            Target::Collection { .. } => "C_{collection[uuid]}_{index}.{extension}",
            Target::Favorite(_) => "f_{_username}_{index}.{extension}",
            Target::Scraps(_) => "s_{_username}_{index}.{extension}",
            Target::Journal(_) => "j_{_username}_{index}.{extension}",
            Target::Status(_) => "S_{_username}_{index}.{extension}",
            Target::Avatar(_) => "a_{_username}_{index}",
            Target::Background(_) => "b_{index}",
            Target::Tag(_) => "T_{search_tags}_{index}.{extension}",
            Target::Search(_) => "Q_{search_tags}_{index}.{extension}",
            Target::Stash(_) => "{index}.{extension}",
            Target::Gallery(_) | Target::GallerySearch { .. } | Target::Deviation { .. } => {
                "g_{_username}_{index}.{extension}"
            }
        };
        return Some(descriptor("deviantart", format));
    }
    if let Ok(target) = crate::flickr::target(url) {
        let format = match target {
            crate::flickr::Target::Photo { .. } => "{id}",
            crate::flickr::Target::Album { .. } => "a_{album[id]}_{id}",
            crate::flickr::Target::User { .. } => "u_{user[nsid]}_{id}",
            crate::flickr::Target::Gallery { .. } => "g_{gallery[id]}_{id}",
            crate::flickr::Target::Group { .. } => "G_{group[nsid]}_{id}",
            crate::flickr::Target::Favorite { .. } => "f_{user[nsid]}_{id}",
            crate::flickr::Target::Search { .. } => "s_{search}_{id}",
        };
        return Some(descriptor("flickr", format));
    }
    if let Ok(target) = crate::wallhaven::target(url) {
        let format = match target {
            crate::wallhaven::Target::Uploads { .. } => "u_{username}_{id}",
            crate::wallhaven::Target::Search { .. } => "s_{search[q]}_{id}",
            _ => "{id}",
        };
        return Some(descriptor("wallhaven", format));
    }
    if let Ok(target) = crate::artstation::target(url) {
        return Some(descriptor("artstation", target.route().archive));
    }
    if let Ok(target) = crate::pinterest::target(url) {
        let format = if matches!(target, crate::pinterest::Target::Board { .. }) {
            "{board[id]}_{id}"
        } else {
            "{id}{media_id|page_id}"
        };
        return Some(descriptor("pinterest", format));
    }
    if crate::newgrounds::target(url).is_ok() {
        return Some(descriptor("newgrounds", "{_type}{_index}"));
    }
    if crate::furaffinity::target(url).is_ok() {
        return Some(descriptor("furaffinity", "{id}"));
    }
    if crate::weasyl::target(url).is_ok() {
        return Some(descriptor("weasyl", "{submitid}"));
    }
    if let Ok(target) = crate::vsco::target(url) {
        let format = match target {
            crate::vsco::Target::Collection { .. } => "c_{user}_{id}",
            crate::vsco::Target::Space { .. } => "s_{user}_{id}",
            _ => "{id}",
        };
        return Some(descriptor("vsco", format));
    }
    if let Ok(target) = crate::imagehosts::target(url) {
        use crate::imagehosts::Target;
        let (category, format) = match target {
            Target::PixeldrainFile(_) | Target::PixeldrainList(..) => ("pixeldrain", "{id}"),
            Target::CatboxAlbum(_) => ("catbox", "{album_id}_{filename}"),
            Target::PixeldrainFolder(_) => ("pixeldrain", "{path}_{num}"),
            Target::CatboxFile(_) => ("catbox", "{filename}"),
            Target::ImgboxImage(_) => ("imgbox", "{image_key}"),
            Target::ImgboxGallery(_) => ("imgbox", "{gallery_key}_{image_key}"),
            Target::PostimgImage(_) | Target::PostimgGallery(_) => ("postimg", "{token}"),
        };
        return Some(descriptor(category, format));
    }
    if crate::instagram::shortcode(url).is_ok() || crate::instagram::profile_username(url).is_ok() {
        return Some(descriptor("instagram", "{media_id}"));
    }
    if crate::facebook::photo_id(url).is_ok() || crate::facebook_video::video_id(url).is_ok() {
        return Some(descriptor("facebook", "{id}.{extension}"));
    }
    for category in ["danbooru", "e621", "gelbooru", "safebooru"] {
        let site = crate::booru::Site::from_category(category)?;
        if let Ok((target, _)) = crate::booru::target(site, url) {
            // The native parser accepts extra host aliases which have no
            // matching installed upstream extractor; do not invent a class.
            if match category {
                "danbooru" => url.host_str() != Some("danbooru.donmai.us"),
                "e621" => !matches!(url.host_str(), Some("e621.net" | "e926.net")),
                "safebooru" => url.host_str() != Some("safebooru.org"),
                _ => false,
            } {
                return None;
            }
            let category =
                if url.host_str().map(|host| host.trim_start_matches("www.")) == Some("e926.net") {
                    "e926"
                } else {
                    category
                };
            let format = match target {
                crate::booru::Target::Post { .. } => "{id}",
                crate::booru::Target::Tags { .. } => {
                    crate::booru::source_search_tags(site, url)?;
                    "t_{search_tags}_{id}"
                }
                crate::booru::Target::Pool { .. }
                    if !matches!(
                        site,
                        crate::booru::Site::Danbooru | crate::booru::Site::E621
                    ) =>
                {
                    "p_{pool}_{id}"
                }
                crate::booru::Target::Pool { .. } => "p_{pool[id]}_{id}",
                crate::booru::Target::Favgroup { .. } => "fg_{favgroup[id]}_{id}",
                crate::booru::Target::Favorites { .. } => "f_{user_id}_{id}",
                crate::booru::Target::DapiFavorites { .. } => "f_{favorite_id}_{id}",
                crate::booru::Target::Popular { .. } => "P_{scale[0]}_{date}_{id}",
                crate::booru::Target::Artist { .. } | crate::booru::Target::ArtistSearch { .. } => {
                    ""
                }
            };
            return Some(descriptor(category, format));
        }
    }
    None
}

/// Retained source classes take priority. Otherwise media fallback is
/// conservative: collection ownership cannot be recovered from a file identity.
pub fn for_media(media: &Media) -> Option<ArchiveDescriptor> {
    if let Some(defaults) = media
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.source_defaults.as_ref())
    {
        return Some(descriptor(defaults.category, defaults.archive));
    }
    if let Some(keywords) = media
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.site_keywords.as_ref())
        .filter(|_| {
            crate::sites::is_category(&media.category)
                || matches!(media.category.as_str(), "pixeldrain" | "catbox")
        })
    {
        return Some(descriptor(&media.category, keywords.route.archive));
    }
    let category = media.category.as_str();
    let format = match category {
        "pixiv" => {
            let m = media.metadata.as_ref()?;
            if m.bookmark_archive || m.user_bookmark.is_some() {
                FAVORITE
            } else if m.ranking_archive || m.ranking.is_some() {
                RANKING
            } else if m.search_archive || m.search.is_some() {
                SEARCH
            } else if m.followed_archive || m.user_follow.is_some() {
                FOLLOWED
            } else if m.pixivision_archive || m.pixivision.is_some() {
                PIXIVISION
            } else if m.sketch_archive || m.sketch.is_some() {
                "S{user[id]}_{id}"
            } else if let Some(asset) = &m.profile_asset {
                match asset.kind {
                    crate::profile_asset::ProfileAssetKind::Avatar => "avatar_{user[id]}_{date}",
                    crate::profile_asset::ProfileAssetKind::Background => {
                        "background_{user[id]}_{date}"
                    }
                }
            } else {
                PIXIV
            }
        }
        "twitter" => {
            let m = media.metadata.as_ref()?;
            if m.community_archive || m.community.is_some() {
                COMMUNITY
            } else if let Some(asset) = &m.profile_asset {
                match asset.kind {
                    crate::profile_asset::ProfileAssetKind::Avatar => "AV_{user[id]}_{date}",
                    crate::profile_asset::ProfileAssetKind::Background => "BG_{user[id]}_{date}",
                }
            } else {
                TWITTER
            }
        }
        "kemono" | "coomer" => {
            let m = media.metadata.as_ref()?;
            if m.discord.is_some() { DISCORD } else { KEMONO }
        }
        "pixiv-novel" => PIXIV,
        "bluesky" | "reddit" => "{filename}",
        "mastodon.social" | "pawoo" | "baraag" => "{media[id]}",
        "tumblr" => "{id}_{num}",
        "artstation" => "{asset[id]}",
        "newgrounds" => "{_type}{_index}",
        "furaffinity" | "vsco" | "pixeldrain" => "{id}",
        "weasyl" => "{submitid}",
        "catbox" => "{album_id}_{filename}",
        "postimg" => "{token}",
        "instagram" => "{media_id}",
        "facebook" => "{id}.{extension}",
        _ => return None,
    };
    Some(descriptor(category, format))
}

/// Queue keywords describe the producer, not necessarily the target. An explicit
/// descriptor takes priority; otherwise only unambiguous native queues qualify.
pub fn for_child(child: &ChildMetadata) -> Option<ArchiveDescriptor> {
    if let Some(value) = child.source_archive() {
        return Some(value.clone());
    }
    if (child.category() == "pixiv-novel" && child.target_category() == "pixiv")
        || (child.category() == "pixiv" && child.subcategory() == "following")
    {
        return Some(descriptor("pixiv", PIXIV));
    }
    // All native queues for these categories target classes sharing one format.
    let format = match child.target_category() {
        "bluesky" | "reddit" => "{filename}",
        "kemono" | "coomer" => match child.subcategory() {
            "discord-server" => DISCORD,
            "artists" | "favorite" => KEMONO,
            _ => return None,
        },
        _ => return None,
    };
    Some(descriptor(child.target_category(), format))
}

/// Add only source keywords whose provenance is retained by the native model.
/// Call before job hooks; never reapply this over a hook-mutated live context.
/// Missing album/folder/search ownership deliberately remains missing rather
/// than manufacturing archive identities.
pub fn project_keywords(media: &Media, context: &mut serde_json::Map<String, serde_json::Value>) {
    use serde_json::{Value, json};
    let Some(metadata) = media.metadata.as_ref() else {
        return;
    };
    match media.category.as_str() {
        "weasyl" if crate::weasyl::valid_id(&metadata.post_id) => {
            // submission_media copies the API's submitid directly to post_id.
            context
                .entry("submitid")
                .or_insert_with(|| metadata.post_id.clone().into());
        }
        "mastodon.social" | "pawoo" | "baraag" if crate::mastodon::valid_id(&metadata.media_id) => {
            // Attachment IDs are copied verbatim. Preview cards deliberately do
            // not qualify: their synthetic native IDs have additional guards.
            context
                .entry("media")
                .or_insert_with(|| json!({"id": metadata.media_id}));
        }
        "artstation" if crate::artstation::valid_id(&metadata.media_id) => {
            context
                .entry("asset")
                .or_insert_with(|| json!({"id": metadata.media_id}));
        }
        "pixiv"
            if metadata.profile_asset.is_none()
                && metadata.sketch.is_none()
                && !metadata.sketch_archive =>
        {
            context
                .entry("id")
                .or_insert_with(|| Value::String(metadata.post_id.clone()));
            let num = metadata.index.saturating_sub(1);
            context
                .entry("num")
                .or_insert_with(|| metadata.source_num.clone().unwrap_or_else(|| num.into()));
            if let Some(suffix) = &metadata.source_suffix {
                context.insert("suffix".into(), suffix.clone());
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    #[test]
    fn booru_tag_descriptors_require_an_actual_source_match() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/source_archive/booru_search.json"
        ))
        .unwrap();
        let mut matched = 0;
        let mut unmatched = 0;
        for case in fixture["cases"].as_array().unwrap() {
            let url = Url::parse(case["url"].as_str().unwrap()).unwrap();
            let actual = for_url(&url);
            if !case["error"].is_null() {
                assert!(actual.is_none(), "{}", case["name"]);
                unmatched += 1;
                continue;
            }
            let actual = actual.unwrap_or_else(|| panic!("{}", case["name"]));
            assert_eq!(actual.category, case["category"], "{}", case["name"]);
            assert_eq!(actual.format, case["archive_format"], "{}", case["name"]);
            assert!(default_keywords_supported(&actual), "{}", case["name"]);
            matched += 1;
        }
        assert_eq!((matched, unmatched), (87, 23));
    }

    #[test]
    fn root_descriptors_match_installed_source_classes() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/source_archive/descriptors.json"
        ))
        .unwrap();
        let mut checked = 0;
        for case in fixture["cases"].as_array().unwrap() {
            let url = Url::parse(case["url"].as_str().unwrap()).unwrap();
            let actual = for_url(&url);
            let expected = &case["descriptor"];
            if expected.is_null() || case["native_supported"] == false {
                assert!(actual.is_none(), "{} has no matched upstream class", url);
            } else {
                let actual = actual.unwrap_or_else(|| panic!("missing native route {}", url));
                assert_eq!(
                    actual.category,
                    expected["category"].as_str().unwrap(),
                    "{}",
                    url
                );
                assert_eq!(
                    actual.format,
                    expected["format"].as_str().unwrap(),
                    "{}",
                    url
                );
                checked += 1;
            }
        }
        assert!(checked >= 100);
    }

    #[test]
    fn empty_formats_and_unknown_routes_are_distinct() {
        let empty = for_url(&Url::parse("https://imgur.com/gallery/abcde").unwrap()).unwrap();
        assert_eq!(empty.format, "");
        assert!(
            for_url(&Url::parse("https://example.invalid/not-a-native-route").unwrap()).is_none()
        );
        assert!(
            for_url(&Url::parse("https://www.flickr.com/photos/example/galleries").unwrap())
                .is_none()
        );
        assert!(for_url(&Url::parse("https://user:PRIVATE@imgur.com/abcde").unwrap()).is_none());
    }

    #[test]
    fn child_annotation_overrides_ambiguous_producer_fields_without_debug_leaks() {
        let queue = ChildMetadata::from_raw("pixiv", "user", &serde_json::Map::new());
        // The same source producer queues avatar, artworks, favorites and novels.
        assert!(for_child(&queue).is_none());
        let annotation = descriptor("PRIVATE-category", "PRIVATE-format");
        let queue = queue.with_source_archive(annotation.clone());
        assert_eq!(for_child(&queue), Some(annotation.clone()));
        assert!(!format!("{annotation:?} {queue:?}").contains("PRIVATE"));
        let novel = ChildMetadata::from_pixiv_novel("novel", json!({"id":42}).as_object().unwrap());
        assert_eq!(for_child(&novel), Some(descriptor("pixiv", PIXIV)));
    }

    #[test]
    fn native_projected_keys_match_source_items_not_native_archive_hashes() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/source_archive/descriptors.json"
        ))
        .unwrap();
        for case in fixture["key_cases"].as_array().unwrap() {
            let url = Url::parse(case["url"].as_str().unwrap()).unwrap();
            let descriptor = for_url(&url).unwrap();
            let media = if descriptor.category == "pixiv" {
                crate::pixiv::parse_illust(&case["input"]).unwrap()
            } else {
                crate::artstation::parse_project(
                    &serde_json::to_vec(&case["input"]).unwrap(),
                    "abc123",
                )
                .unwrap()
            };
            let template =
                crate::postprocess::template::Template::parse(&descriptor.format).unwrap();
            let keys: Vec<_> = media
                .iter()
                .map(|media| {
                    let mut context = crate::postprocess::context(media);
                    project_keywords(media, &mut context);
                    template.render(&context).unwrap()
                })
                .collect();
            assert_eq!(json!(keys), case["keys"], "{}", case["name"]);
            if descriptor.category == "pixiv" {
                let post = crate::event::Post::from_media(&media, "pixiv");
                assert_eq!(
                    post.metadata.as_ref().unwrap().source_suffix,
                    Some(json!(""))
                );
                for file in media {
                    let mut live = json!({"suffix":"post-hook"}).as_object().unwrap().clone();
                    let updates = file
                        .metadata
                        .as_ref()
                        .unwrap()
                        .keyword_updates
                        .as_deref()
                        .unwrap_or_default();
                    crate::postprocess::keywords::apply(&mut live, updates).unwrap();
                    let expected = if case["name"] == "pixiv_single" {
                        "post-hook".to_owned()
                    } else {
                        format!("_p{:02}", file.metadata.as_ref().unwrap().index - 1)
                    };
                    assert_eq!(live["suffix"], expected);
                }
            }
        }
    }

    #[test]
    fn schema_gate_is_independent_of_blank_initial_snapshots() {
        let supported = [
            ("pixiv", PIXIV),
            ("pixiv", FAVORITE),
            ("pixiv", RANKING),
            ("pixiv", SEARCH),
            ("pixiv", FOLLOWED),
            ("pixiv", PIXIVISION),
            ("pixiv", "S{user[id]}_{id}"),
            ("pixiv-novel", PIXIV),
            ("artstation", "{asset[id]}"),
            ("twitter", TWITTER),
            ("twitter", COMMUNITY),
            ("kemono", KEMONO),
            ("coomer", DISCORD),
            ("danbooru", "{id}"),
            ("danbooru", "t_{search_tags}_{id}"),
            ("e926", "{id}"),
            ("danbooru", "p_{pool[id]}_{id}"),
            ("danbooru", "fg_{favgroup[id]}_{id}"),
            ("e621", "p_{pool[id]}_{id}"),
            ("e926", "p_{pool[id]}_{id}"),
            ("flickr", "{id}"),
            ("wallhaven", "{id}"),
            ("instagram", "{media_id}"),
            ("facebook", "{id}.{extension}"),
            ("weasyl", "{submitid}"),
            ("custom-library", ""),
            ("custom-library", "{custom[id]}"),
        ];
        for (category, format) in supported {
            assert!(
                default_keywords_supported(&descriptor(category, format)),
                "{category} {format}"
            );
        }
        for (category, format) in [
            ("imgur", ""),
            ("deviantart", ""),
            ("bluesky", "{filename}"),
            ("reddit", "{filename}"),
            ("tumblr", "{id}_{num}"),
            ("mastodon.social", "{media[id]}"),
            ("newgrounds", "{_type}{_index}"),
            ("flickr", "a_{album[id]}_{id}"),
            ("imgur", "{album[id]}_{id}"),
            ("gelbooru", "p_{pool[id]}_{id}"),
            ("pinterest", "{board[id]}_{id}"),
            ("twitter", "AV_{user[id]}_{date}"),
        ] {
            assert!(
                !default_keywords_supported(&descriptor(category, format)),
                "{category} {format}"
            );
        }
    }

    #[test]
    fn queue_only_empty_defaults_require_source_child_framing() {
        for raw in [
            "https://imgur.com/gallery/abcde",
            "https://www.deviantart.com/example",
        ] {
            let descriptor = for_url(&Url::parse(raw).unwrap()).unwrap();
            assert!(descriptor.format.is_empty()); // Preserve the true root descriptor.
            assert!(!native_framing_supported(&descriptor));
            assert!(!default_keywords_supported(&descriptor));
        }
        // Empty formats are not generally invalid; only proven flattened source
        // dispatch roots are excluded. Caller-owned library extractors may emit
        // files with a valid literal-empty format.
        let custom = descriptor("custom-library", "");
        assert!(native_framing_supported(&custom));
        assert!(default_keywords_supported(&custom));
        assert!(native_framing_supported(&descriptor("imgur", "{id}")));
    }

    #[test]
    fn only_proven_submission_and_attachment_ids_are_projected() {
        let submission = json!({"submitid":123,"media":{"submission":[{"url":"https://cdn.weasyl.com/example.jpg"}]}});
        let media = crate::weasyl::submission_media(&submission).unwrap();
        let mut context = crate::postprocess::context(&media);
        project_keywords(&media, &mut context);
        let template = crate::postprocess::template::Template::parse("{submitid}").unwrap();
        assert_eq!(template.render(&context).unwrap(), "123");
        // Source Weasyl.populate_submission retains submitid without rewriting.
        assert_eq!(context["submitid"], submission["submitid"]);
        let status = json!({"id":"42","account":{"acct":"artist"},"reblog":null,
            "media_attachments":[{"id":"77","type":"image","url":"https://mastodon.social/example.jpg"}]});
        let mut media = crate::mastodon::parse_status(&status, "mastodon.social")
            .pop()
            .unwrap();
        let mut context = crate::postprocess::context(&media);
        project_keywords(&media, &mut context);
        assert_eq!(context["media"]["id"], "77");
        media.metadata.as_mut().unwrap().media_id = "card77".into();
        let mut context = crate::postprocess::context(&media);
        project_keywords(&media, &mut context);
        assert!(!context.contains_key("media"));
        // Newgrounds' output counter is not the source image-prefix identity.
        media.category = "newgrounds".into();
        let mut context = crate::postprocess::context(&media);
        project_keywords(&media, &mut context);
        assert!(!context.contains_key("_type"));
        assert!(!context.contains_key("_index"));
    }

    #[test]
    fn ambiguous_media_does_not_assume_category_default() {
        let direct =
            crate::extract::direct(&Url::parse("https://example.invalid/file.jpg").unwrap())
                .unwrap();
        assert!(for_media(&direct).is_none());
        for category in [
            "imgur",
            "deviantart",
            "flickr",
            "wallhaven",
            "danbooru",
            "e621",
            "pinterest",
            "imgbox",
        ] {
            let mut media = direct.clone();
            media.category = category.into();
            assert!(for_media(&media).is_none(), "{category}");
        }
    }
}
