//! Video platforms through yt-dlp, mirroring gallery_dl/extractor/ytdl.py.
//!
//! Upstream only takes `ytdl:`-prefixed URLs unless `extractor.ytdl.enabled`
//! is set; here the hosts of large video and social platforms without a
//! gallery-dl extractor of their own (YouTube, Twitch, Douyin, LinkedIn,
//! Snapchat Spotlight, Xiaohongshu, Bilibili and VK videos)
//! are routed to it directly. `yt-dlp -J --flat-playlist` lists a URL once;
//! each video becomes one post whose file is a video-page job that the
//! downloader hands back to yt-dlp. Playlist entries keep the flat metadata
//! yt-dlp lists instead of upstream's second extraction per entry.
//!
//! Large or transport-only fields (`formats`, `thumbnails`, subtitles,
//! `http_headers`, underscored internals) are dropped from keywords. The
//! executable runs without a shell or user configuration; diagnostics never
//! include its output, which can contain signed URLs.
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error, extract::MediaJob};
use serde_json::{Map, Value};
use std::process::Stdio;
use tokio::io::AsyncReadExt;
use url::Url;

pub static SITE: Site = Site {
    category: "ytdl",
    name: "yt-dlp",
    matches,
    extract,
    routes: ROUTES,
    api_key: false,
    cookies: None,
};

const FILENAME: &str = "{title}-{id}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{subcategory}"];
const ARCHIVE: &str = "{extractor_key} {id}";

macro_rules! routes {
    ($($name:ident = $key:literal),* $(,)?) => {
        $(static $name: Route = Route::new($key, FILENAME, DIRECTORY, ARCHIVE);)*
        static ROUTES: &[&Route] = &[$(&$name),*];
    };
}

// yt-dlp extractor keys, which upstream uses as subcategories.
routes! {
    GENERIC = "Generic",
    YOUTUBE = "Youtube",
    YOUTUBE_TAB = "YoutubeTab",
    YOUTUBE_PLAYLIST = "YoutubePlaylist",
    YOUTUBE_CLIP = "YoutubeClip",
    TWITCH_VOD = "TwitchVod",
    TWITCH_CLIPS = "TwitchClips",
    TWITCH_STREAM = "TwitchStream",
    TWITCH_VIDEOS = "TwitchVideos",
    TWITCH_VIDEOS_CLIPS = "TwitchVideosClips",
    TWITCH_COLLECTION = "TwitchCollection",
    DOUYIN = "Douyin",
    LINKEDIN = "LinkedIn",
    SNAPCHAT = "SnapchatSpotlight",
    TELEGRAM = "TelegramEmbed",
    XIAOHONGSHU = "XiaoHongShu",
    BILIBILI = "BiliBili",
    BILIBILI_SPACE = "BilibiliSpaceVideo",
    VK = "VK",
}

/// Extractor keys outside the static table, leaked once each; bounded so
/// a misbehaving executable cannot grow memory without limit.
static EXTRA: std::sync::Mutex<Vec<&'static Route>> = std::sync::Mutex::new(Vec::new());
const MAX_EXTRA: usize = 64;

fn route(key: &str) -> &'static Route {
    if let Some(route) = ROUTES.iter().find(|r| r.subcategory == key) {
        return route;
    }
    let valid = !key.is_empty()
        && key.len() <= 64
        && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    if !valid {
        return &GENERIC;
    }
    let mut extra = EXTRA.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(route) = extra.iter().find(|r| r.subcategory == key) {
        return route;
    }
    if extra.len() >= MAX_EXTRA {
        return &GENERIC;
    }
    let key: &'static str = Box::leak(key.to_owned().into_boxed_str());
    let route: &'static Route = Box::leak(Box::new(Route::new(key, FILENAME, DIRECTORY, ARCHIVE)));
    extra.push(route);
    route
}

const MAX_OUTPUT: usize = 64 * 1024 * 1024;
const VIDEO_EXTENSIONS: &[&str] = &["mp4", "webm", "mkv", "mov", "flv"];
const AUDIO_EXTENSIONS: &[&str] = &["m4a", "mp3", "ogg", "opus", "flac", "wav", "aac"];
const DROPPED: &[&str] = &[
    "formats",
    "requested_formats",
    "requested_downloads",
    "thumbnails",
    "subtitles",
    "automatic_captions",
    "requested_subtitles",
    "heatmap",
    "http_headers",
    "fragments",
    "entries",
];

fn host_of(url: &Url) -> Option<&str> {
    let host = url.host_str()?;
    Some(host.strip_prefix("www.").unwrap_or(host))
}

fn under(host: &str, domain: &str) -> bool {
    host == domain || host.ends_with(&format!(".{domain}"))
}

/// Whether a URL belongs to a platform routed through yt-dlp. Paths that
/// native extractors handle on shared hosts are left to them.
fn matches(url: &Url) -> bool {
    if !crate::sites::plain(url) {
        return false;
    }
    let Some(host) = host_of(url) else {
        return false;
    };
    let segments = crate::sites::segments(url);
    let first = segments.first().map(String::as_str).unwrap_or_default();
    if under(host, "youtube.com") || under(host, "youtube-nocookie.com") {
        return !segments.is_empty();
    }
    if host == "youtu.be" {
        return segments.len() == 1;
    }
    if under(host, "twitch.tv") {
        return !segments.is_empty();
    }
    if under(host, "douyin.com") {
        return matches!(first, "video" | "note");
    }
    if under(host, "linkedin.com") {
        return matches!(first, "posts" | "feed" | "events");
    }
    if under(host, "snapchat.com") {
        return first == "spotlight";
    }
    if under(host, "xiaohongshu.com") {
        return matches!(first, "explore" | "discovery");
    }
    if under(host, "bilibili.com") {
        return matches!(first, "video" | "bangumi") || host == "space.bilibili.com";
    }
    if host == "b23.tv" {
        return segments.len() == 1;
    }
    if under(host, "vk.com") || under(host, "vkvideo.ru") {
        return first.starts_with("video") || first.starts_with("clip");
    }
    false
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    crate::sites::once(async move {
        let info = run(http, &url).await?;
        let key = info
            .get("extractor_key")
            .and_then(Value::as_str)
            .unwrap_or("Generic")
            .to_owned();
        let route = route(&key);
        let mut entries = Vec::new();
        flatten(info, &mut entries, 0);
        let events: Vec<_> = entries
            .into_iter()
            .filter_map(|entry| item(entry, &url, &key))
            .flat_map(|item| crate::sites::post(&SITE, http, route, vec![item]))
            .collect();
        Ok(crate::sites::events(events))
    })
}

/// Playlist entries in order, recursing into nested playlists.
fn flatten(info: Value, out: &mut Vec<Map<String, Value>>, depth: usize) {
    let Value::Object(mut info) = info else {
        return;
    };
    match info.remove("entries") {
        Some(Value::Array(entries)) if depth < 8 => {
            for entry in entries {
                flatten(entry, out, depth + 1);
            }
        }
        Some(_) => {}
        None => out.push(info),
    }
}

/// One video's post item, or `None` without an HTTP(S) page URL.
fn item(mut info: Map<String, Value>, input: &Url, key: &str) -> Option<Item> {
    let page = ["webpage_url", "original_url", "url"]
        .iter()
        .filter_map(|k| info.get(*k).and_then(Value::as_str))
        .find_map(|raw| crate::extract::http_url(raw).ok())
        .or_else(|| (info.get("_type").is_none()).then(|| input.clone()))?;
    info.retain(|k, _| !k.starts_with('_') && !DROPPED.contains(&k.as_str()));
    let id = info
        .get("id")
        .map(crate::sites::scalar)
        .filter(|id| !id.is_empty() && id != "None")?;
    if !info.contains_key("extractor_key") {
        let key = info_key(&info).unwrap_or_else(|| key.to_owned());
        info.insert("extractor_key".into(), key.into());
    }
    let ext = info
        .get("ext")
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase)
        .filter(|e| {
            VIDEO_EXTENSIONS.contains(&e.as_str()) || AUDIO_EXTENSIONS.contains(&e.as_str())
        })
        .unwrap_or_else(|| "mp4".into());
    info.entry("title").or_insert_with(|| id.clone().into());
    let title = info.get("title").map(crate::sites::scalar);
    let timestamp = crate::sites::media::ts_value(info.get("timestamp"));
    let uploader = ["uploader", "channel", "uploader_id"]
        .iter()
        .find_map(|k| info.get(*k).and_then(Value::as_str))
        .map(str::to_owned);
    info.insert("filename".into(), title.clone().unwrap_or_default().into());
    info.insert("extension".into(), ext.into());
    if timestamp.is_some() {
        info.insert("date".into(), crate::sites::media::date(timestamp));
    }
    Some(
        Item::new(page.clone(), info)
            .ids(id.clone(), id)
            .user(uploader)
            .caption(title)
            .timestamp(timestamp)
            .job(MediaJob::VideoPage { page_url: page }),
    )
}

/// A flat entry's own extractor key, from its `ie_key`.
fn info_key(info: &Map<String, Value>) -> Option<String> {
    info.get("ie_key")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// `yt-dlp -J --flat-playlist URL`, parsed.
async fn run(http: &Http, url: &Url) -> Result<Value> {
    let mut command = tokio::process::Command::new(http.ytdl());
    command.args([
        "--ignore-config",
        "--no-warnings",
        "--quiet",
        "--dump-single-json",
        "--flat-playlist",
        "--no-cache-dir",
    ]);
    if let Some(proxy) = http.network.proxy() {
        command.args(["--proxy", proxy]);
    }
    if let Some(agent) = http.network.user_agent() {
        command.args(["--user-agent", agent]);
    }
    // Cookie copies are removed when the command finishes.
    let scratch = tempfile::tempdir()?;
    if let Some(cookies) = http.ytdl_cookies() {
        command.args(cookies.arguments(scratch.path())?);
    }
    command
        .args(["--", url.as_str()])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            error("yt-dlp sites need yt-dlp on PATH or --ytdl-command")
        } else {
            error("could not start yt-dlp")
        }
    })?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let work = async {
        let mut output = Vec::new();
        let mut chunk = [0u8; 64 * 1024];
        loop {
            let read = stdout.read(&mut chunk).await?;
            if read == 0 {
                break;
            }
            if output.len() + read > MAX_OUTPUT {
                return Err(error("yt-dlp output exceeded 64 MiB"));
            }
            output.extend_from_slice(&chunk[..read]);
        }
        let status = child.wait().await?;
        Ok((status, output))
    };
    let (status, output) = tokio::time::timeout(http.timeout(), work)
        .await
        .map_err(|_| error("yt-dlp extraction timed out"))??;
    if !status.success() {
        return Err(error(
            "yt-dlp could not extract this URL; it may be private, removed or unsupported",
        ));
    }
    serde_json::from_slice(&output).map_err(|_| error("yt-dlp returned invalid JSON"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing;
    use futures_util::StreamExt;

    fn url(raw: &str) -> Url {
        Url::parse(raw).unwrap()
    }

    #[test]
    fn matches_platform_video_urls_only() {
        for yes in [
            "https://www.youtube.com/watch?v=abcdefghijk",
            "https://m.youtube.com/shorts/abcdefghijk",
            "https://music.youtube.com/playlist?list=PL1",
            "https://www.youtube.com/@channel/videos",
            "https://youtu.be/abcdefghijk",
            "https://www.twitch.tv/videos/123",
            "https://clips.twitch.tv/SomeClip",
            "https://www.douyin.com/video/123",
            "https://www.linkedin.com/posts/someone_activity-1-abc",
            "https://www.snapchat.com/spotlight/W7_EDlXWTBi",
            "https://www.xiaohongshu.com/explore/abc",
            "https://www.bilibili.com/video/BV1xx411c7mD",
            "https://space.bilibili.com/123",
            "https://b23.tv/abc",
            "https://vk.com/video-1_2",
            "https://vk.com/clip-1_2",
        ] {
            assert!(matches(&url(yes)), "{yes}");
        }
        for no in [
            "https://www.youtube.com/",
            "https://youtu.be/",
            "https://t.me/durov",
            "https://t.me/s/durov",
            "https://www.linkedin.com/in/someone",
            "https://www.snapchat.com/add/someone",
            "https://www.bilibili.com/read/cv123",
            "https://vk.com/album1_2",
            "https://user:pw@www.youtube.com/watch?v=x",
            "https://www.youtube.com:8443/watch?v=x",
            "https://example.com/watch?v=x",
        ] {
            assert!(!matches(&url(no)), "{no}");
        }
    }

    #[test]
    fn site_order_leaves_native_platform_paths_alone() {
        // Registry matching is ordered; yt-dlp must come after native sites.
        let last = crate::sites::sites().last().unwrap();
        assert_eq!(last.category, "ytdl");
    }

    #[test]
    fn items_keep_upstream_keywords_and_drop_transport_fields() {
        let input = url("https://www.youtube.com/watch?v=abc");
        let info = serde_json::json!({
            "id": "abc", "title": "A / B", "ext": "webm", "uploader": "Chan",
            "timestamp": 1700000000, "extractor_key": "Youtube",
            "webpage_url": "https://www.youtube.com/watch?v=abc",
            "formats": [{"url": "https://signed.example/x"}],
            "http_headers": {"Cookie": "secret"}, "_version": {}
        });
        let Value::Object(info) = info else {
            unreachable!()
        };
        let item = item(info, &input, "Youtube").unwrap();
        assert_eq!(item.url, input);
        assert!(matches!(&item.job, MediaJob::VideoPage { page_url } if *page_url == input));
        assert_eq!(item.keywords["extension"], "webm");
        assert_eq!(item.keywords["filename"], "A / B");
        assert_eq!(item.keywords["date"], "2023-11-14 22:13:20");
        for gone in ["formats", "http_headers", "_version"] {
            assert!(!item.keywords.contains_key(gone), "{gone}");
        }
        assert_eq!(item.username.as_deref(), Some("Chan"));
    }

    #[test]
    fn flat_entries_use_their_own_urls_and_keys() {
        let input = url("https://www.youtube.com/@chan/videos");
        let info = serde_json::json!({
            "_type": "playlist", "id": "UC1", "extractor_key": "YoutubeTab",
            "entries": [
                {"_type": "url", "ie_key": "Youtube", "id": "v1", "title": "One",
                 "url": "https://www.youtube.com/watch?v=v1"},
                {"_type": "playlist", "entries": [
                    {"_type": "url", "ie_key": "Youtube", "id": "v2",
                     "url": "https://www.youtube.com/watch?v=v2"}
                ]},
                {"_type": "url", "id": "bad", "url": "javascript:alert(1)"},
                null
            ]
        });
        let mut entries = Vec::new();
        flatten(info, &mut entries, 0);
        let items: Vec<_> = entries
            .into_iter()
            .filter_map(|e| item(e, &input, "YoutubeTab"))
            .collect();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].url.as_str(), "https://www.youtube.com/watch?v=v1");
        assert_eq!(items[0].keywords["extractor_key"], "Youtube");
        assert_eq!(items[0].keywords["extension"], "mp4");
        assert_eq!(items[1].keywords["title"], "v2");
    }

    #[test]
    fn unknown_extractor_keys_get_bounded_routes() {
        assert_eq!(route("Youtube").subcategory, "Youtube");
        assert_eq!(route("SomethingNew").subcategory, "SomethingNew");
        assert!(std::ptr::eq(route("SomethingNew"), route("SomethingNew")));
        assert_eq!(route("bad key/../").subcategory, "Generic");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn forwards_session_cookies() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("yt-dlp");
        let json = serde_json::json!({"extractor_key": "Youtube", "id": "v1",
            "title": "One", "ext": "mp4"});
        // Succeeds only when the browser specification is forwarded.
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\ncase \"$*\" in *'--cookies-from-browser firefox:/p '*) ;; \
                 *) exit 1;; esac\ncat <<'EOF'\n{json}\nEOF\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let http = testing::http("yt-dlp", url("http://127.0.0.1:1/"))
            .with_ytdl(&script)
            .with_ytdl_cookies(Some(crate::cookies::YtdlCookies::Browser(
                "firefox:/p".into(),
            )));
        let media = testing::media(extract(&http, url("https://www.youtube.com/watch?v=v1"))).await;
        assert_eq!(media.len(), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn extracts_through_the_executable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("yt-dlp");
        let json = serde_json::json!({
            "_type": "playlist", "extractor_key": "YoutubeTab", "id": "PL",
            "entries": [
                {"_type": "url", "ie_key": "Youtube", "id": "v1", "title": "One",
                 "url": "https://www.youtube.com/watch?v=v1"},
                {"_type": "url", "ie_key": "Youtube", "id": "v2", "title": "Two",
                 "url": "https://www.youtube.com/watch?v=v2"}
            ]
        });
        std::fs::write(&script, format!("#!/bin/sh\ncat <<'EOF'\n{json}\nEOF\n")).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let root = url("http://127.0.0.1:1/");
        let http = testing::http("yt-dlp", root).with_ytdl(&script);
        let media = testing::media(extract(
            &http,
            url("https://www.youtube.com/playlist?list=PL"),
        ))
        .await;
        assert_eq!(media.len(), 2);
        assert_eq!(media[1].filename, "Two.mp4");
        let keywords = testing::keywords(&media[0]);
        assert_eq!(keywords["category"], "ytdl");
        assert_eq!(keywords["subcategory"], "YoutubeTab");
        assert!(matches!(media[0].job, MediaJob::VideoPage { .. }));
        let (_, archive) = crate::sites::archive_key(&media[0]).unwrap();
        assert!(archive.contains(r#"["Youtube","v1"]"#), "{archive}");

        std::fs::write(&script, "#!/bin/sh\necho secret-url >&2\nexit 1\n").unwrap();
        let events: Vec<_> = extract(&http, url("https://youtu.be/x")).collect().await;
        let error = events
            .into_iter()
            .find_map(|e| e.err())
            .unwrap()
            .to_string();
        assert!(!error.contains("secret"), "{error}");
        let missing = testing::http("yt-dlp", url("http://127.0.0.1:1/"))
            .with_ytdl(std::path::Path::new("/nonexistent/yt-dlp"));
        assert!(testing::fails(extract(&missing, url("https://youtu.be/x"))).await);
    }
}

#[cfg(test)]
mod routing_tests {
    #[test]
    fn platform_urls_reach_the_intended_site() {
        for (raw, category) in [
            ("https://vk.com/video-1_2", "ytdl"),
            ("https://vk.com/clip-1_2", "ytdl"),
            ("https://vk.com/id1", "vk"),
            ("https://vk.com/album1_2", "vk"),
            ("https://www.bilibili.com/video/BV1xx411c7mD", "ytdl"),
            ("https://www.bilibili.com/opus/123", "bilibili"),
            ("https://www.youtube.com/watch?v=abcdefghijk", "ytdl"),
            ("https://www.twitch.tv/videos/1", "ytdl"),
            ("https://www.tiktok.com/@user/video/123", "tiktok"),
            ("https://weibo.com/1234567890", "weibo"),
            ("https://discord.com/channels/1/2", "discord"),
            ("https://t.me/durov/123", "telegram"),
            ("https://www.snapchat.com/@nasa", "snapchat"),
            ("https://www.snapchat.com/spotlight/W7_EDlXWTBi", "ytdl"),
            ("https://www.kuaishou.com/short-video/3xabc", "kuaishou"),
            (
                "https://www.xiaohongshu.com/explore/6411cf99",
                "xiaohongshu",
            ),
        ] {
            let url = url::Url::parse(raw).unwrap();
            let site = crate::sites::find(&url).map(|s| s.category);
            assert_eq!(site, Some(category), "{raw}");
        }
    }
}
