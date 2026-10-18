//! Full-resource MP4 representations from Facebook's embedded DASH manifest.
//! Segment addressing is deliberately unsupported: callers download whole files.
use crate::{Result, error, file_metadata::FileMetadata};
use roxmltree::{Document, Node, ParsingOptions};
use url::Url;

const MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_NODES: u32 = 65_536;
const DASH_NS: &str = "urn:mpeg:dash:schema:mpd:2011";

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Video,
    Audio,
}

struct Candidate {
    url: Url,
    rank: (u64, u64, u64),
    file: FileMetadata,
}

pub(crate) type SelectedFile = (Url, &'static str, FileMetadata);

pub(crate) fn parse_manifest(xml: &str) -> Result<Vec<SelectedFile>> {
    if xml.len() > MAX_BYTES {
        return Err(error("Facebook DASH manifest exceeds the size limit"));
    }
    let doc = Document::parse_with_options(
        xml,
        ParsingOptions {
            allow_dtd: false,
            nodes_limit: MAX_NODES,
            ..Default::default()
        },
    )
    .map_err(|_| error("invalid or excessive Facebook DASH XML"))?;
    let root = doc.root_element();
    if !named(root, "MPD")
        || root
            .attribute("type")
            .is_some_and(|value| value != "static")
    {
        return Err(error("unsupported Facebook DASH manifest type"));
    }
    // Check the entire MPD, including inherited protection and segment addressing.
    // An alternative cannot silently turn a partial resource into a complete file.
    if root
        .descendants()
        .any(|node| named(node, "ContentProtection"))
    {
        return Err(error("encrypted Facebook DASH media is unsupported"));
    }
    if root
        .descendants()
        .any(|node| named(node, "SegmentTemplate") || named(node, "SegmentList"))
    {
        return Err(error("segmented Facebook DASH media is unsupported"));
    }
    if root.descendants().any(|node| {
        (named(node, "Initialization") || named(node, "RepresentationIndex"))
            && node
                .attribute("sourceURL")
                .is_some_and(|value| !value.is_empty())
    }) {
        return Err(error(
            "external Facebook DASH initialization is unsupported",
        ));
    }
    let periods: Vec<_> = root
        .children()
        .filter(|node| named(*node, "Period"))
        .collect();
    if periods.len() != 1 {
        return Err(error("Facebook DASH requires one period"));
    }
    let root_base = base_url(root, None)?;
    let period = periods[0];
    let period_base = base_url(period, root_base.as_ref())?;
    let mut videos = Vec::new();
    let mut audios = Vec::new();
    for adaptation in period
        .children()
        .filter(|node| named(*node, "AdaptationSet"))
    {
        let adaptation_base = base_url(adaptation, period_base.as_ref())?;
        let mut count = 0;
        for representation in adaptation
            .children()
            .filter(|node| named(*node, "Representation"))
        {
            count += 1;
            let kind = match media_kind(representation, adaptation)? {
                Some(kind) => kind,
                None if text_representation(representation, adaptation) => continue,
                None => return Err(error("unrecognized Facebook DASH representation type")),
            };
            let url = base_url(representation, adaptation_base.as_ref())?.ok_or_else(|| {
                error("Facebook DASH representation has no absolute resource URL")
            })?;
            validate_url(&url)?;
            let width = number(representation, adaptation, "width")?;
            let height = number(representation, adaptation, "height")?;
            let bandwidth = number(representation, adaptation, "bandwidth")?;
            let rank = match kind {
                Kind::Video => (
                    width
                        .checked_mul(height)
                        .ok_or_else(|| error("invalid Facebook DASH dimensions"))?,
                    height,
                    bandwidth,
                ),
                Kind::Audio => (0, 0, bandwidth),
            };
            let candidate = Candidate {
                url,
                rank,
                file: FileMetadata {
                    width: (kind == Kind::Video && width > 0).then_some(width),
                    height: (kind == Kind::Video && height > 0).then_some(height),
                    bitrate: (bandwidth > 0).then_some(bandwidth),
                    media_type: Some(std::sync::Arc::from(match kind {
                        Kind::Video => "video",
                        Kind::Audio => "audio",
                    })),
                    ..Default::default()
                },
            };
            match kind {
                Kind::Video => videos.push(candidate),
                Kind::Audio => audios.push(candidate),
            }
        }
        if count == 0 {
            return Err(error("Facebook DASH media set has no representations"));
        }
    }
    let video = select(videos)?.ok_or_else(|| error("Facebook DASH manifest has no MP4 video"))?;
    let mut selected = vec![(video.url, "mp4", video.file)];
    if let Some(audio) = select(audios)? {
        selected.push((audio.url, "m4a", audio.file));
    }
    Ok(selected)
}

fn named(node: Node<'_, '_>, local: &str) -> bool {
    node.is_element()
        && node.tag_name().name() == local
        && matches!(node.tag_name().namespace(), None | Some(DASH_NS))
}

fn inherited<'a>(node: Node<'a, 'a>, parent: Node<'a, 'a>, key: &str) -> Option<&'a str> {
    node.attribute(key).or_else(|| parent.attribute(key))
}

fn text_representation(node: Node<'_, '_>, parent: Node<'_, '_>) -> bool {
    // Caption tracks are outside gallery-dl's video/audio output. An override
    // cannot disguise malformed audio/video as a subtitle representation.
    if parent
        .attribute("mimeType")
        .is_some_and(|mime| mime.starts_with("audio/") || mime.starts_with("video/"))
        || matches!(parent.attribute("contentType"), Some("audio" | "video"))
    {
        return false;
    }
    matches!(
        inherited(node, parent, "mimeType"),
        Some("text/vtt" | "application/ttml+xml")
    ) || inherited(node, parent, "contentType") == Some("text")
}

fn media_kind(node: Node<'_, '_>, parent: Node<'_, '_>) -> Result<Option<Kind>> {
    let mime = inherited(node, parent, "mimeType");
    let content = inherited(node, parent, "contentType");
    let kind = match mime {
        Some("video/mp4") => Some(Kind::Video),
        Some("audio/mp4") => Some(Kind::Audio),
        Some(value) if value.starts_with("video/") || value.starts_with("audio/") => {
            return Err(error("unsupported Facebook DASH media format"));
        }
        Some(_) if matches!(content, Some("video" | "audio")) => {
            return Err(error("inconsistent Facebook DASH media format"));
        }
        Some(_) => None,
        None => match content {
            Some("video") => Some(Kind::Video),
            Some("audio") => Some(Kind::Audio),
            _ => None,
        },
    };
    if let Some(kind) = kind {
        if content.is_some_and(|value| {
            value
                != match kind {
                    Kind::Video => "video",
                    Kind::Audio => "audio",
                }
        }) {
            return Err(error("inconsistent Facebook DASH media type"));
        }
        // A declared container is enough: downloading does not require a decoder.
        // Without MIME metadata require a known MP4 codec rather than guessing
        // the container from contentType alone.
        if mime.is_none() {
            let codecs = inherited(node, parent, "codecs")
                .ok_or_else(|| error("Facebook DASH media requires an MP4 MIME type or codec"))?;
            let prefixes: &[&str] = match kind {
                Kind::Video => &["avc1", "avc3", "av01", "hvc1", "hev1"],
                Kind::Audio => &["mp4a", "ac-3", "ec-3", "ac-4", "alac"],
            };
            if !codecs.split(',').all(|codec| {
                let codec = codec.trim();
                prefixes.iter().any(|prefix| {
                    codec == *prefix
                        || codec
                            .strip_prefix(prefix)
                            .is_some_and(|tail| tail.starts_with('.'))
                })
            }) {
                return Err(error("unrecognized Facebook DASH container codec"));
            }
        }
    }
    Ok(kind)
}

fn number(node: Node<'_, '_>, parent: Node<'_, '_>, key: &str) -> Result<u64> {
    inherited(node, parent, key)
        .map(|value| {
            value
                .parse::<u64>()
                .map_err(|_| error("invalid Facebook DASH quality metadata"))
        })
        .transpose()
        .map(|value| value.unwrap_or(0))
}

fn base_url(node: Node<'_, '_>, inherited: Option<&Url>) -> Result<Option<Url>> {
    let mut bases = node.children().filter(|node| named(*node, "BaseURL"));
    let Some(base) = bases.next() else {
        return Ok(inherited.cloned());
    };
    if bases.next().is_some() || base.children().any(|child| child.is_element()) {
        return Err(error("ambiguous Facebook DASH resource URL"));
    }
    let text: String = base.children().filter_map(|child| child.text()).collect();
    let text = text.trim();
    if text.is_empty() {
        return Err(error("empty Facebook DASH resource URL"));
    }
    let url = match Url::parse(text) {
        Ok(url) => Some(url),
        Err(url::ParseError::RelativeUrlWithoutBase) => inherited
            .map(|base| {
                base.join(text)
                    .map_err(|_| error("invalid Facebook DASH resource URL"))
            })
            .transpose()?,
        Err(_) => return Err(error("invalid Facebook DASH resource URL")),
    };
    Ok(url)
}

fn validate_url(url: &Url) -> Result<()> {
    let lower = url.path().to_ascii_lowercase();
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || lower.ends_with(".mpd")
        || lower.ends_with(".m3u8")
    {
        return Err(error("unsupported Facebook DASH resource URL"));
    }
    Ok(())
}

fn select(candidates: Vec<Candidate>) -> Result<Option<Candidate>> {
    let Some(best) = candidates.iter().map(|candidate| candidate.rank).max() else {
        return Ok(None);
    };
    let mut selected = None;
    for candidate in candidates
        .into_iter()
        .filter(|candidate| candidate.rank == best)
    {
        if selected
            .as_ref()
            .is_some_and(|prior: &Candidate| prior.url != candidate.url)
        {
            return Err(error("ambiguous Facebook DASH quality selection"));
        }
        selected = Some(candidate);
    }
    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(body: &str) -> String {
        format!("<MPD type='static'><Period>{body}</Period></MPD>")
    }
    fn video(body: &str) -> String {
        format!("<AdaptationSet mimeType='video/mp4'>{body}</AdaptationSet>")
    }
    fn rep(url: &str, attrs: &str) -> String {
        format!("<Representation {attrs}><BaseURL>{url}</BaseURL></Representation>")
    }

    #[test]
    fn selects_video_quality_then_audio_bandwidth_and_decodes_entities() {
        let xml = manifest(&format!(
            "{}<AdaptationSet mimeType='audio/mp4' codecs='mp4a.40.2'>{}{}</AdaptationSet>",
            video(&format!(
                "{}{}{}",
                rep(
                    "https://cdn.test/low",
                    "width='640' height='360' bandwidth='9999'"
                ),
                rep(
                    "https://cdn.test/high?a=1&amp;b=2",
                    "width='1920' height='1080' bandwidth='1000'"
                ),
                rep(
                    "https://cdn.test/mid",
                    "width='1280' height='720' bandwidth='2000'"
                )
            )),
            rep("https://cdn.test/audio-low", "bandwidth='64'"),
            rep("https://cdn.test/audio", "bandwidth='128'")
        ));
        let result = parse_manifest(&xml).unwrap();
        assert_eq!(
            result.iter().map(|(_, ext, _)| *ext).collect::<Vec<_>>(),
            ["mp4", "m4a"]
        );
        assert_eq!(result[0].0.as_str(), "https://cdn.test/high?a=1&b=2");
        assert_eq!(result[1].0.as_str(), "https://cdn.test/audio");
    }

    #[test]
    fn resolves_namespaced_inheritance_and_whole_segment_base_resources() {
        let xml = "<MPD xmlns='urn:mpeg:dash:schema:mpd:2011'><BaseURL>https://cdn.test/root/</BaseURL><Period><BaseURL>period/</BaseURL><AdaptationSet contentType='video' codecs='avc1.640028'><BaseURL>video/</BaseURL><Representation height='1080'><BaseURL>file.mp4</BaseURL><SegmentBase indexRange='100-200'><Initialization range='0-99'/></SegmentBase></Representation></AdaptationSet></Period></MPD>";
        let result = parse_manifest(xml).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].0.as_str(),
            "https://cdn.test/root/period/video/file.mp4"
        );
        assert!(
            parse_manifest(&xml.replace("range='0-99'", "sourceURL='init.mp4' range='0-99'"))
                .is_err()
        );
        let mixed_text = manifest(&video(&rep("https://cdn.test/<![CDATA[file]]>.mp4", "")));
        assert_eq!(
            parse_manifest(&mixed_text).unwrap()[0].0.as_str(),
            "https://cdn.test/file.mp4"
        );
        assert!(
            parse_manifest(&manifest(&video(
                "<Representation><BaseURL>relative.mp4</BaseURL></Representation>"
            )))
            .is_err()
        );
    }

    #[test]
    fn rejects_ambiguous_winners_but_allows_duplicate_resources() {
        let duplicate = rep("https://cdn.test/a", "bandwidth='1'");
        assert!(parse_manifest(&manifest(&video(&format!("{duplicate}{duplicate}")))).is_ok());
        assert!(
            parse_manifest(&manifest(&video(&format!(
                "{duplicate}{}",
                rep("https://cdn.test/b", "bandwidth='1'")
            ))))
            .is_err()
        );
    }

    #[test]
    fn malformed_audio_cannot_silently_become_video_only() {
        let good_video = video(&rep("https://cdn.test/video", ""));
        for audio in [
            "<AdaptationSet mimeType='audio/mp4'/>",
            "<AdaptationSet mimeType='audio/mp4'><Representation/></AdaptationSet>",
            "<AdaptationSet mimeType='audio/webm'><Representation><BaseURL>https://cdn.test/audio</BaseURL></Representation></AdaptationSet>",
            "<AdaptationSet mimeType='audio/mp4'><Representation mimeType='application/octet-stream'><BaseURL>https://cdn.test/audio</BaseURL></Representation></AdaptationSet>",
            "<AdaptationSet><Representation codecs='mp4a.40.2'><BaseURL>https://cdn.test/audio</BaseURL></Representation></AdaptationSet>",
        ] {
            assert!(parse_manifest(&manifest(&format!("{good_video}{audio}"))).is_err());
        }
    }

    #[test]
    fn subtitles_do_not_hide_or_replace_audio_video() {
        let source = format!(
            "{}<AdaptationSet mimeType='text/vtt'><Representation><BaseURL>https://cdn.test/captions.vtt</BaseURL></Representation></AdaptationSet>",
            video(&rep("https://cdn.test/video.mp4", ""))
        );
        assert_eq!(parse_manifest(&manifest(&source)).unwrap().len(), 1);
        let disguised = format!(
            "{}<AdaptationSet mimeType='audio/mp4'><Representation mimeType='text/vtt'><BaseURL>https://cdn.test/captions.vtt</BaseURL></Representation></AdaptationSet>",
            video(&rep("https://cdn.test/video.mp4", ""))
        );
        assert!(parse_manifest(&manifest(&disguised)).is_err());
    }

    #[test]
    fn mp4_container_does_not_require_a_supported_decoder() {
        for codec in [
            "av01.0.08M.08",
            "hvc1.1.6.L120",
            "hev1.1.6.L120",
            "vp09.00.41.08",
            "future-codec",
        ] {
            let xml = manifest(&video(&rep(
                "https://cdn.test/video",
                &format!("codecs='{codec}'"),
            )));
            assert!(parse_manifest(&xml).is_ok());
        }
        for codec in ["avc3.640028", "av01.0.08M.08", "hvc1.1.6.L120"] {
            let xml = manifest(&format!(
                "<AdaptationSet contentType='video' codecs='{codec}'>{}</AdaptationSet>",
                rep("https://cdn.test/video", "")
            ));
            assert!(parse_manifest(&xml).is_ok());
        }
    }

    #[test]
    fn rejects_encryption_segment_addressing_and_unsupported_layouts() {
        let good = manifest(&video(&rep("https://cdn.test/video", "")));
        for element in ["ContentProtection", "SegmentTemplate", "SegmentList"] {
            let xml = good.replace("<Period>", &format!("<Period><{element}/>"));
            assert!(parse_manifest(&xml).is_err());
        }
        for xml in [
            good.replace("static", "dynamic"),
            good.replace("</MPD>", "<Period/></MPD>"),
            good.replace("<MPD", "<Wrong").replace("</MPD>", "</Wrong>"),
            good.replace("<MPD", "<MPD xmlns='urn:other'"),
            "<MPD>broken".into(),
        ] {
            assert!(parse_manifest(&xml).is_err());
        }
    }

    #[test]
    fn errors_do_not_disclose_signed_urls_or_xml() {
        for url in [
            "https://user:PRIVATE@cdn.test/file",
            "https://cdn.test/file.mpd?PRIVATE",
            "https://cdn.test/file.m3u8?PRIVATE",
            "file:///PRIVATE",
            "https://cdn.test/file#PRIVATE",
        ] {
            let message = parse_manifest(&manifest(&video(&rep(url, ""))))
                .unwrap_err()
                .to_string();
            assert!(!message.contains("PRIVATE"));
            assert!(!message.contains("cdn.test"));
        }
        let xml = "<!DOCTYPE MPD [<!ENTITY secret SYSTEM 'file:///PRIVATE'>]><MPD>&secret;</MPD>";
        assert!(
            !parse_manifest(xml)
                .unwrap_err()
                .to_string()
                .contains("PRIVATE")
        );
    }

    #[test]
    fn bounds_manifest_size_and_node_count() {
        assert!(parse_manifest(&" ".repeat(MAX_BYTES + 1)).is_err());
        let xml = format!(
            "<MPD><Period>{}</Period></MPD>",
            "<x/>".repeat(MAX_NODES as usize)
        );
        assert!(parse_manifest(&xml).is_err());
    }
}
