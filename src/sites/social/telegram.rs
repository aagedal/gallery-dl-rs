//! Public Telegram channels through their web preview (`t.me/s/CHANNEL`).
//!
//! gallery-dl has no Telegram extractor; formats follow its conventions.
//! Channel listings run newest first, following each page's `prev` link;
//! single posts use the embed view, which shows the whole album. Photos and
//! inline videos are downloaded directly. Videos too large for the preview,
//! documents, voice notes and polls are not exposed by it (nor to yt-dlp)
//! and are skipped. Private channels and
//! channels without a public preview fail with an explicit error.
use crate::sites::{Events, Http, Item, Route, Site};
use crate::error;
use serde_json::{Map, Value, json};
use url::Url;

pub static SITE: Site = Site {
    category: "telegram",
    name: "Telegram",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CHANNEL, &POST],
    api_key: false,
    cookies: None,
};

const FILENAME: &str = "{channel}_{id}_{num}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{channel}"];
const ARCHIVE: &str = "{channel}_{id}_{num}";
static CHANNEL: Route = Route::new("channel", FILENAME, DIRECTORY, ARCHIVE);
static POST: Route = Route::new("post", FILENAME, DIRECTORY, ARCHIVE);

const ROOT: &str = "https://t.me";
/// Media hosts the preview serves files from.
const MEDIA_HOSTS: &[&str] = &["telesco.pe", "telegram-cdn.org", "cdn-telegram.org"];
/// Channel pages walked at most, about 20 posts each.
const MAX_PAGES: usize = 10_000;

#[derive(Debug, PartialEq)]
enum Target {
    Channel {
        channel: String,
        before: Option<u64>,
    },
    Post {
        channel: String,
        id: u64,
    },
}

/// Telegram usernames: 4–32 characters of letters, digits and underscores.
fn username(name: &str) -> bool {
    (4..=32).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Paths of the web app rather than channels.
const RESERVED: &[&str] = &[
    "s",
    "c",
    "joinchat",
    "addstickers",
    "addemoji",
    "share",
    "proxy",
    "socks",
    "login",
    "iv",
    "setlanguage",
    "addtheme",
    "contact",
    "invoice",
    "boost",
    "bg",
    "k",
    "a",
    "z",
];

fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) || !matches!(url.host_str()?, "t.me" | "telegram.me" | "www.t.me")
    {
        return None;
    }
    let segments = crate::sites::segments(url);
    let parts: Vec<&str> = segments.iter().map(String::as_str).collect();
    let (channel, rest) = match parts.as_slice() {
        ["s", channel, rest @ ..] => (*channel, rest),
        [channel, rest @ ..] if !RESERVED.contains(channel) && !channel.starts_with('+') => {
            (*channel, rest)
        }
        _ => return None,
    };
    if !username(channel) {
        return None;
    }
    let channel = channel.to_owned();
    match rest {
        [] => {
            let before = url
                .query_pairs()
                .find(|(k, _)| k == "before")
                .and_then(|(_, v)| v.parse().ok());
            Some(Target::Channel { channel, before })
        }
        [id] => id.parse().ok().map(|id| Target::Post { channel, id }),
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Post { channel, id }) => crate::sites::once(async move {
            let page = http.api(&format!("{ROOT}/{channel}/{id}?embed=1&mode=tme"))?;
            let html = http.text(&page).await?;
            let messages = messages(&html);
            let message = messages
                .into_iter()
                .find(|m| m.channel.eq_ignore_ascii_case(&channel) && m.id == id)
                .ok_or_else(|| error("Telegram post not found or not public"))?;
            Ok(crate::sites::events(post_events(http, &POST, message)))
        }),
        Some(Target::Channel { channel, before }) => {
            let start = match before {
                Some(before) => format!("/s/{channel}?before={before}"),
                None => format!("/s/{channel}"),
            };
            crate::sites::paginate(Some((start, 0usize)), move |state| {
                let channel = channel.clone();
                async move {
                    let Some((path, count)) = state else {
                        return Ok(None);
                    };
                    let html = http.text(&http.api(&format!("{ROOT}{path}"))?).await?;
                    let mut messages = messages(&html);
                    if count == 0 && messages.is_empty() {
                        return Err(error("Telegram channel not found or has no public preview"));
                    }
                    messages.retain(|m| m.channel.eq_ignore_ascii_case(&channel));
                    messages.sort_by_key(|m| std::cmp::Reverse(m.id));
                    let next = prev_link(&html, &channel).filter(|_| count + 1 < MAX_PAGES);
                    let events = messages
                        .into_iter()
                        .flat_map(|m| post_events(http, &CHANNEL, m))
                        .collect();
                    Ok(Some((
                        crate::sites::events(events),
                        next.map(|path| (path, count + 1)),
                    )))
                }
            })
        }
        None => crate::sites::fail(error("unsupported Telegram URL")),
    }
}

/// The `/s/CHANNEL?before=N` path of the next older page.
fn prev_link(html: &str, channel: &str) -> Option<String> {
    let href = crate::sites::extr(html, "<link rel=\"prev\" href=\"", "\"")?;
    let before: u64 = href
        .strip_prefix(&format!("/s/{channel}?before="))
        .or_else(|| {
            // Channel names are case-insensitive; keep the requested form.
            let (path, before) = href.split_once("?before=")?;
            path.strip_prefix("/s/")?
                .eq_ignore_ascii_case(channel)
                .then_some(before)
        })?
        .parse()
        .ok()?;
    Some(format!("/s/{channel}?before={before}"))
}

#[derive(Debug, Default)]
struct Message {
    channel: String,
    id: u64,
    channel_name: String,
    author: Option<String>,
    forwarded_from: Option<String>,
    text: String,
    date: Option<i64>,
    views: Option<String>,
    files: Vec<File>,
}

#[derive(Debug, PartialEq)]
enum File {
    Photo(Url),
    Video(Url),
}

/// Every widget message on a page, in page order.
fn messages(html: &str) -> Vec<Message> {
    let mut out = Vec::new();
    let mut chunks = html
        .split("class=\"tgme_widget_message ")
        .skip(1)
        .peekable();
    while let Some(chunk) = chunks.next() {
        if let Some(message) = message(chunk) {
            out.push(message);
        }
    }
    out
}

fn attr<'h>(tag: &'h str, name: &str) -> Option<&'h str> {
    crate::sites::extr(tag, &format!("{name}=\""), "\"")
}

/// Text of the element following a class marker, with tags removed.
fn class_text(html: &str, class: &str) -> Option<String> {
    let start = html.find(&format!("class=\"{class}"))?;
    let rest = &html[start..];
    let body = &rest[rest.find('>')? + 1..];
    let end = body
        .find("</a>")
        .into_iter()
        .chain(body.find("</div>"))
        .chain(body.find("</span>"))
        .min()?;
    let text = crate::sites::unescape(&crate::sites::remove_html(&body[..end]));
    (!text.is_empty()).then_some(text)
}

fn message(chunk: &str) -> Option<Message> {
    let head = &chunk[..chunk.find('>')?];
    let (channel, id) = attr(head, "data-post")?.split_once('/')?;
    let mut message = Message {
        channel: channel.to_owned(),
        id: id.parse().ok()?,
        ..Message::default()
    };
    message.channel_name = class_text(chunk, "tgme_widget_message_owner_name").unwrap_or_default();
    message.author = class_text(chunk, "tgme_widget_message_from_author");
    message.forwarded_from = class_text(chunk, "tgme_widget_message_forwarded_from_name");
    message.views = class_text(chunk, "tgme_widget_message_views");
    if let Some(start) = chunk.find("class=\"tgme_widget_message_text") {
        let rest = &chunk[start..];
        if let Some(open) = rest.find('>') {
            let body = &rest[open + 1..];
            // Message text has no nested <div>s; its closing tag ends it.
            let end = body.find("</div>").unwrap_or(body.len());
            let html = body[..end].replace("<br/>", "\n").replace("<br>", "\n");
            message.text = crate::sites::unescape(&strip_tags(&html)).trim().to_owned();
        }
    }
    message.date =
        crate::sites::extr(chunk, "<time datetime=\"", "\"").and_then(crate::sites::media::iso_ts);
    message.files = files(chunk);
    Some(message)
}

/// Remove tags but keep line breaks, unlike `remove_html`.
fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut inside = false;
    for c in html.chars() {
        match c {
            '<' => inside = true,
            '>' if inside => inside = false,
            c if !inside => out.push(c),
            _ => {}
        }
    }
    out
}

fn media_url(raw: &str) -> Option<Url> {
    crate::sites::media_url(&crate::sites::unescape(raw), MEDIA_HOSTS)
        .filter(|url| url.scheme() == "https")
}

/// Photos and videos in document order.
fn files(chunk: &str) -> Vec<File> {
    let mut files = Vec::new();
    let mut rest = chunk;
    loop {
        let photo = rest.find("tgme_widget_message_photo_wrap");
        let video = rest.find("tgme_widget_message_video_player");
        let round = rest.find("tgme_widget_message_roundvideo_player");
        let Some(pos) = [photo, video, round].into_iter().flatten().min() else {
            break;
        };
        let tail = &rest[pos..];
        if Some(pos) == photo {
            let tag = &tail[..tail.find('>').unwrap_or(tail.len())];
            if let Some(url) =
                crate::sites::extr(tag, "background-image:url('", "')").and_then(media_url)
            {
                files.push(File::Photo(url));
            }
            rest = &tail[tag.len()..];
            continue;
        }
        // A video player ends at the next player or message part.
        let end = [
            "tgme_widget_message_photo_wrap",
            "tgme_widget_message_video_player",
            "tgme_widget_message_roundvideo_player",
        ]
        .iter()
        .filter_map(|marker| tail[1..].find(marker).map(|i| i + 1))
        .min()
        .unwrap_or(tail.len());
        let player = &tail[..end];
        // Players without a source hold videos too large for the preview.
        if let Some(url) = crate::sites::extr(player, "<video src=\"", "\"").and_then(media_url) {
            files.push(File::Video(url));
        }
        rest = &tail[end..];
    }
    files
}

fn post_events(
    http: &Http,
    route: &'static Route,
    message: Message,
) -> Vec<crate::event::ExtractionEvent> {
    let count = message.files.len();
    let base: Map<String, Value> = json!({
        "channel": message.channel,
        "channel_name": message.channel_name,
        "id": message.id,
        "post_url": format!("{ROOT}/{}/{}", message.channel, message.id),
        "text": message.text,
        "author": message.author,
        "forwarded_from": message.forwarded_from,
        "views": message.views,
        "date": crate::sites::media::date(message.date),
        "count": count,
    })
    .as_object()
    .cloned()
    .unwrap_or_default();
    let items = message
        .files
        .into_iter()
        .enumerate()
        .map(|(index, file)| {
            let mut keywords = base.clone();
            keywords.insert("num".into(), (index + 1).into());
            let (url, kind) = match file {
                File::Photo(url) => (url, "photo"),
                File::Video(url) => (url, "video"),
            };
            let (name, mut ext) = crate::sites::nameext(&url);
            if ext.is_empty() {
                ext = if kind == "photo" { "jpg" } else { "mp4" }.into();
            }
            keywords.insert("type".into(), kind.into());
            keywords.insert("filename".into(), name.into());
            keywords.insert("extension".into(), ext.into());
            Item::new(url, keywords)
                .ids(
                    message.id.to_string(),
                    format!("{}_{}", message.id, index + 1),
                )
                .user(Some(message.channel.clone()))
                .caption(Some(message.text.clone()))
                .timestamp(message.date)
        })
        .collect();
    crate::sites::post(&SITE, http, route, items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::MediaJob;
    use crate::sites::testing::{self, html, serve};

    fn url(raw: &str) -> Url {
        Url::parse(raw).unwrap()
    }

    const PHOTO: &str = "https://cdn4.telesco.pe/file/abc.jpg";
    const VIDEO: &str = "https://cdn4.telesco.pe/file/v.mp4?token=t";

    fn message_html(channel: &str, id: u64, media: &str) -> String {
        format!(
            r#"<div class="tgme_widget_message_wrap js-widget_message_wrap"><div class="tgme_widget_message text_not_supported_wrap js-widget_message" data-post="{channel}/{id}" data-view="x">
<div class="tgme_widget_message_author accent_color"><a class="tgme_widget_message_owner_name" href="https://t.me/{channel}"><span dir="auto">Chan &amp; Co</span></a></div>
<div class="media_supported_cont">{media}<div class="tgme_widget_message_text js-message_text" dir="auto">Hello<br/><b>world</b> &lt;3</div></div>
<span class="tgme_widget_message_views">1.2K</span><time datetime="2026-07-16T20:45:25+00:00" class="time">20:45</time></div></div>"#
        )
    }

    fn photo(href: &str) -> String {
        format!(
            r#"<a class="tgme_widget_message_photo_wrap 1 2" href="{href}" style="width:800px;background-image:url('{PHOTO}')"><div class="tgme_widget_message_photo"></div></a>"#
        )
    }

    fn video() -> String {
        format!(
            r#"<a class="tgme_widget_message_video_player js-message_video_player" href="https://t.me/chan/2"><i class="tgme_widget_message_video_thumb" style="background-image:url('https://cdn4.telesco.pe/file/thumb.jpg')"></i><video src="{VIDEO}" class="tgme_widget_message_video"></video></a>"#
        )
    }

    fn big_video() -> String {
        r#"<a class="tgme_widget_message_video_player not_supported js-message_video_player" href="https://t.me/chan/3"><i class="tgme_widget_message_video_thumb" style="background-image:url('https://cdn4.telesco.pe/file/thumb.jpg')"></i><div class="message_media_not_supported">Media is too big</div></a>"#.into()
    }

    #[test]
    fn matches_channels_and_posts() {
        let channel = |c: &str, before| {
            Some(Target::Channel {
                channel: c.into(),
                before,
            })
        };
        assert_eq!(target(&url("https://t.me/s/durov")), channel("durov", None));
        assert_eq!(target(&url("https://t.me/durov")), channel("durov", None));
        assert_eq!(
            target(&url("https://t.me/s/durov?before=531")),
            channel("durov", Some(531))
        );
        assert_eq!(
            target(&url("https://t.me/durov/536?single")),
            Some(Target::Post {
                channel: "durov".into(),
                id: 536
            })
        );
        assert_eq!(
            target(&url("https://telegram.me/s/durov/1")),
            Some(Target::Post {
                channel: "durov".into(),
                id: 1
            })
        );
        for no in [
            "https://t.me/joinchat/abc",
            "https://t.me/+abcdef",
            "https://t.me/c/123/4",
            "https://t.me/abc",
            "https://t.me/durov/x",
            "https://t.me/durov/1/2",
            "https://t.me:8443/durov",
            "https://example.com/durov",
        ] {
            assert_eq!(target(&url(no)), None, "{no}");
        }
    }

    #[test]
    fn parses_messages_and_media() {
        let page = format!(
            "{}{}",
            message_html(
                "chan",
                1,
                &format!(
                    "{}{}",
                    photo("https://t.me/chan/1"),
                    photo("https://t.me/chan/2")
                )
            ),
            message_html("chan", 3, &format!("{}{}", video(), big_video())),
        );
        let messages = messages(&page);
        assert_eq!(messages.len(), 2);
        let first = &messages[0];
        assert_eq!(first.channel_name, "Chan & Co");
        assert_eq!(first.text, "Hello\nworld <3");
        assert_eq!(first.views.as_deref(), Some("1.2K"));
        assert_eq!(first.date, Some(1784234725));
        assert_eq!(
            first.files,
            vec![File::Photo(url(PHOTO)), File::Photo(url(PHOTO))]
        );
        assert_eq!(messages[1].files, vec![File::Video(url(VIDEO))]);
    }

    #[test]
    fn rejects_foreign_media_hosts() {
        let page = message_html(
            "chan",
            1,
            r#"<a class="tgme_widget_message_photo_wrap" style="background-image:url('https://evil.example/x.jpg')"></a>"#,
        );
        assert!(messages(&page)[0].files.is_empty());
    }

    #[tokio::test]
    async fn walks_channel_pages_newest_first() {
        let first = format!(
            r#"<link rel="prev" href="/s/chan?before=5">{}{}"#,
            message_html("chan", 5, &photo("https://t.me/chan/5")),
            message_html("chan", 6, &video()),
        );
        let second = message_html("chan", 4, &photo("https://t.me/chan/4"));
        let (root, server) = serve(vec![html(&first), html(&second)]);
        let http = testing::http("Telegram", root);
        let media = testing::media(extract(&http, url("https://t.me/s/chan"))).await;
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /s/chan "));
        assert!(requests[1].starts_with("GET /s/chan?before=5 "));
        let ids: Vec<_> = media
            .iter()
            .map(|m| testing::keywords(m)["id"].clone())
            .collect();
        assert_eq!(ids, [json!(6), json!(5), json!(4)]);
        let keywords = testing::keywords(&media[0]);
        assert_eq!(keywords["subcategory"], "channel");
        assert_eq!(keywords["type"], "video");
        assert_eq!(keywords["extension"], "mp4");
        assert_eq!(keywords["post_url"], "https://t.me/chan/6");
        let (_, archive) = crate::sites::archive_key(&media[1]).unwrap();
        assert!(archive.contains(r#"["chan","5","1"]"#), "{archive}");
    }

    #[tokio::test]
    async fn single_posts_use_the_embed_view() {
        let page = message_html("chan", 3, &format!("{}{}", video(), big_video()));
        let (root, server) = serve(vec![html(&page)]);
        let http = testing::http("Telegram", root);
        let media = testing::media(extract(&http, url("https://t.me/chan/3"))).await;
        assert!(server.join().unwrap()[0].starts_with("GET /chan/3?embed=1&mode=tme "));
        // The oversized video is not exposed and is skipped.
        assert_eq!(media.len(), 1);
        assert!(matches!(media[0].job, MediaJob::DirectHttp));
        assert_eq!(testing::keywords(&media[0])["count"], 1);
        assert_eq!(testing::keywords(&media[0])["subcategory"], "post");
    }

    #[tokio::test]
    async fn missing_previews_and_posts_fail() {
        let (root, _server) = serve(vec![html("<html>no preview</html>"), html("<html></html>")]);
        let http = testing::http("Telegram", root);
        assert!(testing::fails(extract(&http, url("https://t.me/s/private_chan"))).await);
        assert!(testing::fails(extract(&http, url("https://t.me/chan/9"))).await);
    }
}
