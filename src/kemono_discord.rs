//! Discord server/channel extraction using the site's bounded public API.
use super::{
    Kemono, MAX_PAGES, MAX_POST_FILES, MAX_RESPONSE, Selection, archive_extension, category,
    clean_extension, content_hash, image_extension, safe_file_path, source_file, split_path,
};
use crate::{
    Result, error,
    event::{ChildControl, ChildMetadata, ChildScope, ExtractionEvent, Post, group},
    extract::{Media, MediaJob, PostMetadata},
    file_metadata::FileMetadata,
};
use futures_util::{
    StreamExt,
    stream::{self, BoxStream},
};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    sync::Arc,
};
use url::Url;

const PAGE_SIZE: usize = 150;
const MAX_CHANNELS: usize = 1024;
const MAX_TEXT: usize = 64 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct DiscordMetadata {
    pub server_id: String,
    pub channel_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<Arc<str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<Arc<str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_nsfw: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_type: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_topic: Option<Arc<str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
}

pub fn valid_discord_id(raw: &str) -> bool {
    !raw.is_empty()
        && raw.len() <= 20
        && raw.bytes().all(|byte| byte.is_ascii_digit())
        && raw
            .parse::<u64>()
            .is_ok_and(|id| id > 0 && id.to_string() == raw)
}
fn id(value: &Value) -> Option<String> {
    match value {
        Value::String(id) => Some(id.clone()),
        Value::Number(id) => id.as_u64().map(|id| id.to_string()),
        _ => None,
    }
    .filter(|id| valid_discord_id(id))
}
fn text(value: &Value, key: &str) -> Option<Arc<str>> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| text.len() <= MAX_TEXT && !text.contains('\0'))
        .map(Arc::from)
}
fn invalid() -> crate::Error {
    error("Kemono returned invalid Discord identity or metadata")
}

struct Channel {
    metadata: DiscordMetadata,
    post_count: Option<usize>,
    queue_server: Arc<Value>,
    queue_channel: Value,
    owner: Option<String>,
}
impl Channel {
    fn valid_for(&self, server: &str) -> bool {
        valid_discord_id(&self.metadata.channel_id) && self.owner.as_deref() == Some(server)
    }
}
fn raw_channel_id(value: &Value) -> Option<String> {
    match value {
        Value::String(value) if value.len() <= 128 => Some(value.clone()),
        Value::Number(value) => value.as_u64().map(|value| value.to_string()),
        _ => None,
    }
}
fn queue_fields(value: &Value, fields: &[&str]) -> Value {
    Value::Object(
        fields
            .iter()
            .filter_map(|key| {
                let value = value.get(*key)?;
                let valid = match value {
                    Value::String(text) => text.len() <= MAX_TEXT && !text.contains('\0'),
                    Value::Number(number) => number.to_string().len() <= 128,
                    Value::Bool(_) | Value::Null => true,
                    _ => false,
                };
                valid.then(|| ((*key).into(), value.clone()))
            })
            .collect(),
    )
}
fn server_channels(bytes: &[u8], server: &str) -> Result<VecDeque<Channel>> {
    if bytes.len() > MAX_RESPONSE || !valid_discord_id(server) {
        return Err(invalid());
    }
    let body: Value = serde_json::from_slice(bytes).map_err(|_| invalid())?;
    if body.get("id").and_then(id).as_deref() != Some(server) {
        return Err(invalid());
    }
    let channels = body
        .get("channels")
        .and_then(Value::as_array)
        .ok_or_else(invalid)?;
    if channels.len() > MAX_CHANNELS {
        return Err(error("Kemono Discord channel count exceeds limit"));
    }
    let mut positions = HashMap::new();
    let mut result = VecDeque::new();
    let server_name = text(&body, "name");
    let queue_server = Arc::new(queue_fields(&body, &["id", "name", "indexed", "updated"]));
    for channel in channels {
        let channel_id = channel
            .get("id")
            .and_then(raw_channel_id)
            .unwrap_or_default();
        let owner = channel.get("server_id").and_then(raw_channel_id);
        let post_count = channel
            .get("post_count")
            .and_then(Value::as_u64)
            .and_then(|count| usize::try_from(count).ok())
            .filter(|count| count.checked_add(PAGE_SIZE * MAX_PAGES).is_some());
        let queue_channel = queue_fields(
            channel,
            &[
                "id",
                "server_id",
                "name",
                "parent_channel_id",
                "topic",
                "is_nsfw",
                "type",
                "post_count",
                "position",
                "theme_color",
                "icon_emoji",
            ],
        );
        let canonical = valid_discord_id(&channel_id).then(|| channel_id.clone());
        let descriptor = Channel {
            queue_server: queue_server.clone(),
            queue_channel,
            owner,
            metadata: DiscordMetadata {
                server_id: server.into(),
                channel_id,
                server: server_name.clone(),
                channel: text(channel, "name"),
                channel_nsfw: channel.get("is_nsfw").and_then(Value::as_bool),
                channel_type: channel.get("type").and_then(Value::as_u64),
                channel_topic: text(channel, "topic"),
                parent_id: channel.get("parent_channel_id").and_then(id),
            },
            post_count,
        };
        // Upstream stores channels in an insertion-ordered mapping: a later
        // duplicate replaces its value without moving the original position.
        if let Some(id) = canonical {
            if let Some(position) = positions.get(&id) {
                result[*position] = descriptor;
                continue;
            }
            positions.insert(id, result.len());
        }
        result.push_back(descriptor);
    }
    Ok(result)
}

fn message_id(post: &Value, metadata: &DiscordMetadata) -> Result<String> {
    if !valid_discord_id(&metadata.server_id) || !valid_discord_id(&metadata.channel_id) {
        return Err(invalid());
    }
    for (keys, expected) in [
        (["server", "server_id"], metadata.server_id.as_str()),
        (["channel", "channel_id"], metadata.channel_id.as_str()),
    ] {
        let mut present = false;
        for key in keys {
            if let Some(value) = post.get(key) {
                present = true;
                if id(value).as_deref() != Some(expected) {
                    return Err(invalid());
                }
            }
        }
        if !present {
            return Err(invalid());
        }
    }
    post.get("id").and_then(id).ok_or_else(invalid)
}

struct File {
    url: Url,
    path: String,
    name: Option<Arc<str>>,
    media_id: String,
    kind: &'static str,
}
fn inline_files(post: &Value) -> Vec<File> {
    let Some(content) = post.get("content").and_then(Value::as_str) else {
        return Vec::new();
    };
    let mut files = Vec::new();
    for (at, _) in content.match_indices("http") {
        let rest = &content[at..];
        let Some(origin) = [
            "https://cdn.discordapp.com/",
            "http://cdn.discordapp.com/",
            "https://media.discordapp.net/",
            "http://media.discordapp.net/",
        ]
        .into_iter()
        .find(|origin| rest.starts_with(origin)) else {
            continue;
        };
        let raw = rest
            .split(|character: char| {
                !character.is_ascii_alphanumeric() && !"-._~:/?#[]@!$&'()*+,;%=".contains(character)
            })
            .next()
            .unwrap_or_default();
        if raw.len() > 4096 || raw.len() <= origin.len() {
            continue;
        }
        let Ok(mut url) = Url::parse(raw) else {
            continue;
        };
        if url.port().is_some() || !url.username().is_empty() || url.password().is_some() {
            continue;
        }
        let _ = url.set_scheme("https");
        let _ = url.set_host(Some("cdn.discordapp.com"));
        url.set_fragment(None);
        // Signed CDN parameters are transport context, never descriptive names.
        let path = url.path().to_owned();
        files.push(File {
            url,
            path: path.clone(),
            name: Some(Arc::from(path)),
            media_id: String::new(),
            kind: "inline",
        });
        if files.len() >= MAX_POST_FILES {
            break;
        }
    }
    files
}
fn files(post: &Value, host: &str) -> Vec<File> {
    let mut files = Vec::new();
    let mut names = HashMap::new();
    if let Some(attachments) = post.get("attachments").and_then(Value::as_array) {
        for attachment in attachments {
            let Some(file) = source_file(attachment, super::FileKind::Attachments, &mut names)
            else {
                continue;
            };
            if !safe_file_path(&file.path) {
                continue;
            }
            let Some((fallback, _)) = split_path(&file.path) else {
                continue;
            };
            let Ok(url) = Url::parse(&format!("https://{host}/data{}", file.path)) else {
                continue;
            };
            files.push(File {
                media_id: content_hash(&file.path).unwrap_or(fallback).into(),
                url,
                path: file.path,
                name: file.name,
                kind: "attachment",
            });
            if files.len() >= MAX_POST_FILES {
                return files;
            }
        }
    }
    files.extend(
        inline_files(post)
            .into_iter()
            .take(MAX_POST_FILES - files.len()),
    );
    files
}

pub fn parse_discord_post(
    post: &Value,
    host: &str,
    metadata: &DiscordMetadata,
    selection: &Selection,
) -> Result<Vec<Media>> {
    Ok(parse_post(post, host, metadata, selection)?.0)
}
fn parse_post(
    post: &Value,
    host: &str,
    discord: &DiscordMetadata,
    selection: &Selection,
) -> Result<(Vec<Media>, usize, PostMetadata)> {
    let category = category(host).ok_or_else(invalid)?;
    let host = host
        .strip_prefix("www.")
        .or_else(|| host.strip_prefix("beta."))
        .unwrap_or(host);
    let message = message_id(post, discord)?;
    let base = PostMetadata {
        post_id: message.clone(),
        caption: post.get("content").and_then(Value::as_str).map(Arc::from),
        timestamp: post
            .get("published")
            .and_then(Value::as_str)
            .and_then(|date| crate::date::parse_date(date).ok()),
        discord: Some(discord.clone()),
        instance: Some("discord".into()),
        ..PostMetadata::default()
    };
    let files = files(post, host);
    let count = files.len();
    let mut media = Vec::new();
    for (offset, file) in files.into_iter().enumerate() {
        let path_ext = file
            .url
            .path()
            .rsplit('/')
            .next()
            .and_then(|name| name.rsplit_once('.'))
            .and_then(|(_, extension)| clean_extension(extension));
        let name_ext = file
            .name
            .as_deref()
            .and_then(|name| name.rsplit(['/', '\\']).next())
            .and_then(|name| name.rsplit_once('.'))
            .filter(|(stem, _)| !stem.is_empty())
            .and_then(|(_, extension)| clean_extension(extension));
        let original_ext = name_ext
            .or_else(|| path_ext.clone())
            .unwrap_or_else(|| "bin".into());
        if !selection.original && !path_ext.as_deref().is_some_and(image_extension) {
            continue;
        }
        let url = if !selection.original && file.kind == "attachment" {
            Url::parse(&format!("https://img.{host}/thumbnail/data{}", file.path))
                .map_err(|_| invalid())?
        } else {
            file.url
        };
        let extension = if !selection.original && file.kind == "attachment" {
            "jpg".into()
        } else {
            original_ext
        };
        let kind = if path_ext.as_deref().is_some_and(archive_extension)
            || path_ext.as_deref() == Some("bin") && archive_extension(&extension)
        {
            "archive"
        } else {
            file.kind
        };
        let mut metadata = base.clone();
        metadata.index = offset + 1;
        metadata.media_id = file.media_id;
        metadata.file = Some(FileMetadata {
            description: file.name,
            media_type: Some(Arc::from(kind)),
            ..FileMetadata::default()
        });
        media.push(Media {
            url,
            job: MediaJob::DirectHttp,
            category: category.into(),
            filename: format!("{message}_{:02}.{extension}", offset + 1),
            metadata: Some(metadata),
            headers: BTreeMap::from([
                ("Accept-Encoding".into(), "identity".into()),
                (
                    "Referer".into(),
                    format!(
                        "https://{host}/discord/server/{}/{}",
                        discord.server_id, discord.channel_id
                    ),
                ),
            ]),
            handoff: BTreeMap::new(),
        });
    }
    Ok((media, count, base))
}

struct ChannelState {
    channel: Channel,
    offset: usize,
    pages: usize,
    done: bool,
    seen: HashSet<[u8; 32]>,
}
struct State {
    host: String,
    server: String,
    requested: Option<String>,
    initialized: bool,
    channels: VecDeque<Channel>,
    current: Option<ChannelState>,
    raw: VecDeque<Value>,
    pending: VecDeque<ExtractionEvent>,
    emitted: usize,
    control: Option<ChildControl>,
}

fn channel_error(
    mut state: State,
    error: crate::Error,
) -> Option<(Result<ExtractionEvent>, Option<State>)> {
    if state.requested.is_some() {
        return Some((Err(error), None));
    }
    state.raw.clear();
    state.pending.clear();
    state.current = None;
    state.emitted = 0;
    Some((Err(error), Some(state)))
}

pub(super) fn events(
    extractor: &Kemono,
    host: String,
    server: String,
    requested: Option<String>,
) -> BoxStream<'_, Result<ExtractionEvent>> {
    let state = State {
        host,
        server,
        requested,
        initialized: false,
        channels: VecDeque::new(),
        current: None,
        raw: VecDeque::new(),
        pending: VecDeque::new(),
        emitted: 0,
        control: None,
    };
    stream::unfold(Some(state), move |state| async move {
        let mut state = state?;
        loop {
            if state
                .control
                .as_ref()
                .is_some_and(ChildControl::is_cancelled)
            {
                state.pending.clear();
                state.raw.clear();
                state.current = None;
                state.emitted = 0;
            }
            if state.current.is_none()
                && let Some(control) = state.control.take()
            {
                return Some((Ok(ExtractionEvent::EndChild(control)), Some(state)));
            }
            if let Some(event) = state.pending.pop_front() {
                return Some((Ok(event), Some(state)));
            }
            if extractor
                .max_posts
                .is_some_and(|limit| state.emitted >= limit)
            {
                // Whole-server jobs queue independent upstream channel jobs;
                // each channel gets its own raw-post allowance.
                state.raw.clear();
                state.current = None;
                state.emitted = 0;
                continue;
            }
            if let Some(post) = state.raw.pop_front() {
                state.emitted += 1;
                let discord = &state
                    .current
                    .as_ref()
                    .expect("active Discord channel")
                    .channel
                    .metadata;
                let (mut media, count, base) =
                    match parse_post(&post, &state.host, discord, &extractor.selection) {
                        Ok(parsed) => parsed,
                        Err(error) => return channel_error(state, error),
                    };
                for item in &mut media {
                    extractor.network.apply_media(item);
                }
                let category = category(&state.host).expect("validated host");
                state.pending = if media.is_empty() {
                    VecDeque::from([
                        ExtractionEvent::Post(Box::new(Post {
                            category: category.into(),
                            metadata: Some(base),
                            count: Some(count),
                        })),
                        ExtractionEvent::EndPost,
                    ])
                } else {
                    let mut events = group(media, category);
                    if let Some(ExtractionEvent::Post(post)) = events.front_mut() {
                        post.count = Some(count);
                    }
                    events
                };
                continue;
            }
            if state.current.as_ref().is_some_and(|channel| channel.done) {
                state.current = None;
                state.emitted = 0;
                continue;
            }
            if state.current.is_none() {
                if !state.initialized {
                    let result = async {
                        let body = extractor
                            .call(
                                &state.host,
                                &format!("api/v1/discord/server/{}", state.server),
                                &[],
                            )
                            .await?;
                        server_channels(&body, &state.server)
                    }
                    .await;
                    state.channels = match result {
                        Ok(channels) => channels,
                        Err(error) => return Some((Err(error), None)),
                    };
                    if let Some(requested) = &state.requested {
                        state
                            .channels
                            .retain(|channel| &channel.metadata.channel_id == requested);
                        if state.channels.is_empty() {
                            return Some((Err(error("Kemono Discord channel not found")), None));
                        }
                    }
                    state.initialized = true;
                }
                let channel = state.channels.pop_front()?;
                let offset = if extractor.post_order == super::PostOrder::Reverse {
                    channel.post_count.unwrap_or(0) / PAGE_SIZE * PAGE_SIZE
                } else {
                    0
                };
                let scope = if state.requested.is_none() {
                    let mut fields = serde_json::to_value(&channel.metadata)
                        .expect("serializable Discord metadata")
                        .as_object()
                        .unwrap()
                        .clone();
                    if let Some(id) = channel.queue_channel.get("id") {
                        fields.insert("id".into(), id.clone());
                    }
                    fields.insert("server".into(), channel.queue_server.as_ref().clone());
                    fields.insert("channel".into(), channel.queue_channel.clone());
                    if let Some(count) = channel.post_count {
                        fields.insert("post_count".into(), Value::from(count));
                    }
                    Some(ChildScope::new(ChildMetadata::from_raw(
                        category(&state.host).expect("validated host"),
                        "discord-server",
                        &fields,
                    )))
                } else {
                    None
                };
                state.current = Some(ChannelState {
                    channel,
                    offset,
                    pages: 0,
                    done: false,
                    seen: HashSet::new(),
                });
                if let Some(scope) = scope {
                    state.control = Some(scope.control.clone());
                    return Some((Ok(ExtractionEvent::BeginChild(scope)), Some(state)));
                }
            }
            let channel = state.current.as_mut().expect("active Discord channel");
            if !channel.channel.valid_for(&state.server) {
                return channel_error(state, invalid());
            }
            if extractor.post_order == super::PostOrder::Reverse
                && channel.channel.post_count.is_none()
            {
                return channel_error(
                    state,
                    error("Kemono Discord reverse order requires a valid post count"),
                );
            }
            let offset = channel.offset.to_string();
            let path = format!(
                "api/v1/discord/channel/{}",
                channel.channel.metadata.channel_id
            );
            let bytes = match extractor
                .call(&state.host, &path, &[("o", offset.as_str())])
                .await
            {
                Ok(bytes) => bytes,
                Err(error) => return channel_error(state, error),
            };
            let parsed = serde_json::from_slice::<Value>(&bytes)
                .ok()
                .and_then(|body| body.as_array().cloned());
            let Some(mut messages) = parsed else {
                return channel_error(
                    state,
                    error("Kemono Discord response is not a message list"),
                );
            };
            if messages.len() > PAGE_SIZE {
                return channel_error(state, error("Kemono Discord message page exceeds limit"));
            }
            let identities: Vec<_> = messages
                .iter()
                .map(|post| (post.get("id"), post.get("server"), post.get("channel")))
                .collect();
            let fingerprint = Sha256::digest(
                serde_json::to_vec(&identities).expect("serializable message identities"),
            );
            if !messages.is_empty() && !channel.seen.insert(fingerprint.into()) {
                return channel_error(state, error("Kemono Discord listing repeated a page"));
            }
            channel.pages += 1;
            if extractor.post_order == super::PostOrder::Reverse {
                messages.reverse();
                channel.done = channel.offset == 0 || channel.pages >= MAX_PAGES;
                channel.offset = channel.offset.saturating_sub(PAGE_SIZE);
            } else {
                channel.done = messages.len() < PAGE_SIZE || channel.pages >= MAX_PAGES;
                channel.offset += PAGE_SIZE;
            }
            state.raw = messages.into();
        }
    })
    .boxed()
}

#[cfg(test)]
mod tests {
    use super::super::{Endpoint, FileKind, PostOrder, RevisionMode, Target, target};
    use super::*;
    use crate::network::NetworkOptions;
    use serde_json::json;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
        time::Duration,
    };

    const HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    fn metadata() -> DiscordMetadata {
        DiscordMetadata {
            server_id: "100".into(),
            channel_id: "200".into(),
            server: Some(Arc::from("Server")),
            channel: Some(Arc::from("Channel")),
            ..DiscordMetadata::default()
        }
    }
    fn message(id: u64, channel: &str) -> Value {
        json!({"id":id.to_string(),"server":"100","channel":channel,"content":"message body","published":"2024-01-02T03:04:05.123000",
        "attachments":[{"path":format!("/aa/11/{HASH}.png"),"name":"image.png"}]})
    }
    fn server_payload(channels: &[(&str, usize)]) -> Value {
        json!({"id":"100","name":"Server","channels":channels.iter().map(|(id,count)|json!({"id":id,"server_id":"100","name":format!("channel-{id}"),"is_nsfw":false,"type":0,"topic":"line1\nline2","parent_channel_id":null,"post_count":count})).collect::<Vec<_>>()})
    }
    fn response(status: &str, body: &[u8]) -> Vec<u8> {
        let mut response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body);
        response
    }
    fn serve(replies: Vec<Vec<u8>>) -> (Url, thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let root = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let server = thread::spawn(move || {
            let mut requests = Vec::new();
            for reply in replies {
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                let mut connection = loop {
                    match listener.accept() {
                        Ok((connection, _)) => break connection,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(
                                std::time::Instant::now() < deadline,
                                "missing expected Discord request"
                            );
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("{error}"),
                    }
                };
                // Accepted sockets inherit nonblocking mode on macOS.
                connection.set_nonblocking(false).unwrap();
                connection
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut bytes = Vec::new();
                let mut buffer = [0; 1024];
                while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                    let count = connection.read(&mut buffer).unwrap();
                    assert_ne!(count, 0);
                    bytes.extend_from_slice(&buffer[..count]);
                }
                requests.push(String::from_utf8(bytes).unwrap());
                let _ = connection.write_all(&reply);
            }
            requests
        });
        (root, server)
    }
    fn extractor(root: Url) -> Kemono {
        Kemono::new(Duration::from_secs(5))
            .unwrap()
            .with_api_root(root)
            .with_network(&NetworkOptions::new(Some(String::new()), None).unwrap())
            .unwrap()
    }
    fn ids(events: &[Result<ExtractionEvent>]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event.as_ref().unwrap() {
                ExtractionEvent::Post(post) => {
                    Some(post.metadata.as_ref().unwrap().post_id.clone())
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn routes_cover_channel_paths_server_and_hash_aliases_privately() {
        for suffix in [
            "/200",
            "/channel/200",
            "#200",
            "#channel/200",
            "#/channel/200",
        ] {
            assert_eq!(
                target(
                    &Url::parse(&format!("https://kemono.cr/discord/server/100{suffix}")).unwrap()
                )
                .unwrap(),
                Target::Discord {
                    host: "kemono.cr".into(),
                    server: "100".into(),
                    channel: Some("200".into())
                }
            );
        }
        assert_eq!(
            target(&Url::parse("https://coomer.st/discord/server/100/").unwrap()).unwrap(),
            Target::Discord {
                host: "coomer.st".into(),
                server: "100".into(),
                channel: None
            }
        );
        for path in [
            "/discord/server/0/200",
            "/discord/server/100/0",
            "/discord/server/100/private-channel",
            "/discord/server/100#private-channel",
            "/discord/server/100/18446744073709551616",
            "/discord/channel/200",
        ] {
            let error = target(&Url::parse(&format!("https://kemono.cr{path}")).unwrap())
                .unwrap_err()
                .to_string();
            assert!(!error.contains("private"));
        }
    }

    #[test]
    fn queued_channels_share_bounded_server_metadata_without_cloning_per_channel() {
        let mut body = server_payload(&[("200", 1), ("201", 2), ("202", 3)]);
        body["name"] = json!("Shared server name".repeat(128));
        body["indexed"] = json!("2024-01-01");
        body["updated"] = json!(123);
        let channels = server_channels(body.to_string().as_bytes(), "100").unwrap();
        assert_eq!(channels.len(), 3);
        for channel in &channels {
            assert!(Arc::ptr_eq(
                &channels[0].queue_server,
                &channel.queue_server
            ));
            assert!(Arc::ptr_eq(
                channels[0].metadata.server.as_ref().unwrap(),
                channel.metadata.server.as_ref().unwrap()
            ));
            assert_eq!(channel.queue_server["name"], body["name"]);
            assert_eq!(channel.queue_server["indexed"], "2024-01-01");
            assert!(channel.queue_server.get("channels").is_none());
        }
        assert_eq!(channels[0].queue_channel["id"], "200");
        assert_eq!(channels[1].queue_channel["id"], "201");
        assert_eq!(channels[2].queue_channel["post_count"], 3);
    }

    #[test]
    fn channel_descriptors_defer_invalid_identity_and_replace_duplicate_ids_in_first_slot() {
        let mut payload = server_payload(&[("200", 1), ("201", 2), ("200", 3), ("202", 4)]);
        payload["channels"][0]["name"] = json!("old");
        payload["channels"][2]["name"] = json!("last");
        payload["channels"][3]["server_id"] = json!("private/invalid");
        payload["channels"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id":"private/invalid","server_id":"100"}));
        payload["channels"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id":{"private":"value"},"server_id":"100"}));
        let channels = server_channels(payload.to_string().as_bytes(), "100").unwrap();
        assert_eq!(channels.len(), 5);
        assert_eq!(channels[0].metadata.channel_id, "200");
        assert_eq!(channels[0].metadata.channel.as_deref(), Some("last"));
        assert_eq!(channels[0].post_count, Some(3));
        assert_eq!(channels[1].metadata.channel_id, "201");
        assert!(channels[0].valid_for("100"));
        assert!(channels[1].valid_for("100"));
        assert!(!channels[2].valid_for("100"));
        assert!(!channels[3].valid_for("100"));
        assert!(!channels[4].valid_for("100"));
        assert!(channels[4].queue_channel.get("id").is_none());
    }

    #[test]
    fn capture_server_and_messages_keep_bound_server_channel_identity() {
        let bytes = include_bytes!("../tests/fixtures/kemono/discord-server.json");
        let channels = server_channels(bytes, "100").unwrap();
        assert_eq!(channels.len(), 3);
        assert_eq!(channels[0].metadata.channel_id, "200");
        assert_eq!(channels[0].metadata.channel_nsfw, Some(false));
        assert_eq!(channels[0].post_count, Some(3));
        let messages: Value = serde_json::from_slice(include_bytes!(
            "../tests/fixtures/kemono/discord-channel.json"
        ))
        .unwrap();
        let mut total = 0;
        for post in messages.as_array().unwrap() {
            let (files, _, base) = parse_post(
                post,
                "kemono.cr",
                &channels[0].metadata,
                &Selection {
                    original: true,
                    ..Selection::default()
                },
            )
            .unwrap();
            total += files.len();
            assert!(base.creator.is_none());
            assert_eq!(base.instance.as_deref(), Some("discord"));
            assert_eq!(base.discord.as_ref().unwrap().server_id, "100");
        }
        assert_eq!(total, 2);
        let mut wrong: Value = serde_json::from_slice(bytes).unwrap();
        wrong["channels"][0]["server_id"] = json!("private-other");
        let wrong = server_channels(wrong.to_string().as_bytes(), "100").unwrap();
        assert!(!wrong[0].valid_for("100"));
        assert!(wrong[1].valid_for("100"));
        assert!(server_channels(bytes, "101").is_err());
        let mut post = message(400, "200");
        post["channel"] = json!("private-other");
        assert!(
            !parse_discord_post(&post, "kemono.cr", &metadata(), &Selection::default())
                .unwrap_err()
                .to_string()
                .contains("private")
        );
        post = message(400, "200");
        post["server_id"] = json!("101");
        assert!(
            parse_discord_post(&post, "kemono.cr", &metadata(), &Selection::default()).is_err()
        );
    }

    #[test]
    fn original_and_thumbnail_modes_preserve_duplicates_and_ignore_creator_file_controls() {
        let mut post = message(400, "200");
        post["attachments"] = json!([
            {"path":format!("/aa/11/{HASH}.mp4"),"name":"video.mp4"},
            {"path":format!("/aa/11/{HASH}.png"),"name":"first.png"},
            {"path":format!("/aa/11/{HASH}.png"),"name":"duplicate.png"},
            {"path":format!("/aa/11/{HASH}.bin"),"name":"archive.zip"}]);
        post["content"] = json!(
            "inline https://media.discordapp.net/attachments/200/500/photo.gif?ex=abc and https://evil.example/secret.jpg"
        );
        let selection = Selection {
            files: Some(vec![]),
            ..Selection::default()
        };
        let (media, count, _) = parse_post(&post, "kemono.cr", &metadata(), &selection).unwrap();
        assert_eq!(count, 5);
        assert_eq!(
            media
                .iter()
                .map(|media| media.metadata.as_ref().unwrap().index)
                .collect::<Vec<_>>(),
            [2, 3, 5]
        );
        assert_eq!(
            media
                .iter()
                .map(|media| media.filename.as_str())
                .collect::<Vec<_>>(),
            ["400_02.jpg", "400_03.jpg", "400_05.gif"]
        );
        assert!(
            media[0]
                .url
                .as_str()
                .starts_with("https://img.kemono.cr/thumbnail/data/")
        );
        assert_eq!(
            media[2].url.as_str(),
            "https://cdn.discordapp.com/attachments/200/500/photo.gif?ex=abc"
        );
        assert_eq!(
            media[2]
                .metadata
                .as_ref()
                .unwrap()
                .file
                .as_ref()
                .unwrap()
                .description
                .as_deref(),
            Some("/attachments/200/500/photo.gif")
        );
        assert!(
            !serde_json::to_string(&media[2].metadata.as_ref().unwrap().file)
                .unwrap()
                .contains("ex=abc")
        );
        assert!(media[2].metadata.as_ref().unwrap().media_id.is_empty());
        assert!(media[2].archive_key().is_some());
        let originals = parse_discord_post(
            &post,
            "kemono.cr",
            &metadata(),
            &Selection {
                original: true,
                files: Some(vec![FileKind::File]),
                ..Selection::default()
            },
        )
        .unwrap();
        assert_eq!(originals.len(), 5);
        assert_eq!(originals[3].filename, "400_04.zip");
        assert_eq!(
            originals[3]
                .metadata
                .as_ref()
                .unwrap()
                .file
                .as_ref()
                .unwrap()
                .media_type
                .as_deref(),
            Some("archive")
        );
        assert_ne!(originals[1].archive_key(), originals[2].archive_key());
    }

    #[tokio::test]
    async fn explicit_channel_fetches_server_once_and_never_creator_profiles() {
        let (root, server) = serve(vec![
            response(
                "200 OK",
                server_payload(&[("200", 2), ("201", 0)])
                    .to_string()
                    .as_bytes(),
            ),
            response(
                "200 OK",
                json!([message(400, "200"), message(401, "200")])
                    .to_string()
                    .as_bytes(),
            ),
        ]);
        let events: Vec<_> = extractor(root)
            .with_revisions(RevisionMode::All)
            .with_expand(true)
            .events(
                &Url::parse("https://kemono.cr/discord/server/100/channel/200#human-name").unwrap(),
            )
            .collect()
            .await;
        assert_eq!(ids(&events), ["400", "401"]);
        assert!(!events.iter().any(|event| matches!(
            event,
            Ok(ExtractionEvent::BeginChild(_) | ExtractionEvent::EndChild(_))
        )));
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].starts_with("GET /api/v1/discord/server/100 "));
        assert!(requests[1].starts_with("GET /api/v1/discord/channel/200?o=0 "));
        assert!(
            !requests
                .iter()
                .any(|request| request.contains("profile") || request.contains("revisions"))
        );
    }

    #[tokio::test]
    async fn forward_pagination_uses_150_and_keeps_empty_message_boundaries() {
        let page:Vec<_>=(1..=150).map(|index|if index==1 {json!({"id":"1","server":"100","channel":"200","content":"text only","attachments":[]})} else {message(index,"200")}).collect();
        let (root, server) = serve(vec![
            response(
                "200 OK",
                server_payload(&[("200", 151)]).to_string().as_bytes(),
            ),
            response("200 OK", json!(page).to_string().as_bytes()),
            response(
                "200 OK",
                json!([message(151, "200")]).to_string().as_bytes(),
            ),
        ]);
        let events: Vec<_> = extractor(root)
            .events(&Url::parse("https://kemono.cr/discord/server/100/200").unwrap())
            .collect()
            .await;
        assert_eq!(ids(&events).len(), 151);
        let ExtractionEvent::Post(first) = events[0].as_ref().unwrap() else {
            panic!("expected post")
        };
        assert_eq!(first.count, Some(0));
        assert_eq!(
            first.metadata.as_ref().unwrap().caption.as_deref(),
            Some("text only")
        );
        let requests = server.join().unwrap();
        assert!(requests[2].starts_with("GET /api/v1/discord/channel/200?o=150 "));
    }

    #[tokio::test]
    async fn reverse_pagination_starts_from_count_and_reverses_each_page() {
        let earlier: Vec<_> = (1..=150).map(|id| message(id, "200")).collect();
        let (root, server) = serve(vec![
            response(
                "200 OK",
                server_payload(&[("200", 151)]).to_string().as_bytes(),
            ),
            response(
                "200 OK",
                json!([message(151, "200")]).to_string().as_bytes(),
            ),
            response("200 OK", json!(earlier).to_string().as_bytes()),
        ]);
        let events: Vec<_> = extractor(root)
            .with_post_order(PostOrder::Reverse)
            .events(&Url::parse("https://kemono.cr/discord/server/100/200").unwrap())
            .collect()
            .await;
        let ids = ids(&events);
        assert_eq!(ids.len(), 151);
        assert_eq!(&ids[..3], ["151", "150", "149"]);
        assert_eq!(ids.last().unwrap(), "1");
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /api/v1/discord/channel/200?o=150 "));
        assert!(requests[2].starts_with("GET /api/v1/discord/channel/200?o=0 "));
    }

    #[tokio::test]
    async fn reverse_exact_multiple_skips_empty_tail_probe_and_recovers_previous_page() {
        let earlier: Vec<_> = (1..=150).map(|id| message(id, "200")).collect();
        let (root, server) = serve(vec![
            response(
                "200 OK",
                server_payload(&[("200", 150)]).to_string().as_bytes(),
            ),
            response("200 OK", b"[]"),
            response("200 OK", json!(earlier).to_string().as_bytes()),
        ]);
        let events: Vec<_> = extractor(root)
            .with_post_order(PostOrder::Reverse)
            .events(&Url::parse("https://kemono.cr/discord/server/100/200").unwrap())
            .collect()
            .await;
        assert_eq!(ids(&events).len(), 150);
        assert_eq!(ids(&events)[0], "150");
        assert_eq!(server.join().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn whole_server_limits_raw_posts_per_channel_and_drop_avoids_later_channels() {
        let mut first = message(400, "200");
        first["attachments"] = json!([]);
        let (root, server) = serve(vec![
            response(
                "200 OK",
                server_payload(&[("200", 2), ("201", 2)])
                    .to_string()
                    .as_bytes(),
            ),
            response(
                "200 OK",
                json!([first, message(401, "200")]).to_string().as_bytes(),
            ),
            response(
                "200 OK",
                json!([message(500, "201"), message(501, "201")])
                    .to_string()
                    .as_bytes(),
            ),
        ]);
        let events: Vec<_> = extractor(root)
            .with_max_posts(Some(1))
            .events(&Url::parse("https://kemono.cr/discord/server/100").unwrap())
            .collect()
            .await;
        assert_eq!(ids(&events), ["400", "500"]);
        assert_eq!(server.join().unwrap().len(), 3);
        let (root, server) = serve(vec![
            response(
                "200 OK",
                server_payload(&[("200", 2), ("201", 2)])
                    .to_string()
                    .as_bytes(),
            ),
            response(
                "200 OK",
                json!([message(400, "200"), message(401, "200")])
                    .to_string()
                    .as_bytes(),
            ),
        ]);
        let client = extractor(root);
        let input = Url::parse("https://kemono.cr/discord/server/100").unwrap();
        let mut events = client.events(&input);
        assert!(matches!(
            events.next().await.unwrap().unwrap(),
            ExtractionEvent::BeginChild(_)
        ));
        assert!(matches!(
            events.next().await.unwrap().unwrap(),
            ExtractionEvent::Post(_)
        ));
        drop(events);
        assert_eq!(server.join().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn whole_server_cancelled_channels_skip_fetches_and_pending_media_then_resume_siblings() {
        let full: Vec<_> = (1..=150).map(|id| message(id, "201")).collect();
        let mut payload = server_payload(&[("200", 1), ("201", 151), ("202", 1)]);
        payload["indexed"] = json!(123);
        payload["updated"] = json!("2026-10-21");
        payload["url"] = json!("https://private.example/token");
        payload["channels"][0]["cookie"] = json!("private");
        payload["channels"][0]["position"] = json!("03");
        payload["channels"][0]["theme_color"] = json!(42);
        payload["channels"][0]["icon_emoji"] = json!("猫");
        let (root, server) = serve(vec![
            response("200 OK", payload.to_string().as_bytes()),
            response("200 OK", json!(full).to_string().as_bytes()),
            response(
                "200 OK",
                json!([message(500, "202")]).to_string().as_bytes(),
            ),
        ]);
        let client = extractor(root);
        let input = Url::parse("https://kemono.cr/discord/server/100").unwrap();
        let mut events = client.events(&input);
        let ExtractionEvent::BeginChild(skipped) = events.next().await.unwrap().unwrap() else {
            panic!("missing first channel")
        };
        assert_eq!(skipped.metadata.category(), "kemono");
        assert_eq!(skipped.metadata.subcategory(), "discord-server");
        assert_eq!(skipped.metadata.fields()["id"], "200");
        assert_eq!(skipped.metadata.fields()["server_id"], "100");
        assert_eq!(skipped.metadata.fields()["channel_id"], "200");
        assert_eq!(skipped.metadata.fields()["server"]["id"], "100");
        assert_eq!(skipped.metadata.fields()["server"]["name"], "Server");
        assert_eq!(skipped.metadata.fields()["channel"]["id"], "200");
        assert_eq!(
            skipped.metadata.fields()["channel"]["topic"],
            "line1\nline2"
        );
        assert_eq!(skipped.metadata.fields()["channel"]["post_count"], 1);
        assert!(
            skipped.metadata.fields()["server"]
                .get("channels")
                .is_none()
        );
        assert_eq!(skipped.metadata.fields()["server"]["indexed"], 123);
        assert_eq!(skipped.metadata.fields()["server"]["updated"], "2026-10-21");
        assert_eq!(skipped.metadata.fields()["channel"]["position"], "03");
        assert_eq!(skipped.metadata.fields()["channel"]["theme_color"], 42);
        assert_eq!(skipped.metadata.fields()["channel"]["icon_emoji"], "猫");
        assert!(skipped.metadata.fields()["server"].get("url").is_none());
        assert!(skipped.metadata.fields()["channel"].get("cookie").is_none());
        skipped.control.cancel();
        let ExtractionEvent::EndChild(end) = events.next().await.unwrap().unwrap() else {
            panic!("missing skipped end")
        };
        assert!(end.same_scope(&skipped.control));
        let ExtractionEvent::BeginChild(first) = events.next().await.unwrap().unwrap() else {
            panic!("missing second channel")
        };
        assert_eq!(first.metadata.fields()["id"], "201");
        assert!(matches!(
            events.next().await.unwrap().unwrap(),
            ExtractionEvent::Post(_)
        ));
        first.control.cancel();
        let ExtractionEvent::EndChild(end) = events.next().await.unwrap().unwrap() else {
            panic!("pending post survived cancellation")
        };
        assert!(end.same_scope(&first.control));
        let rest: Vec<_> = events.collect().await;
        assert_eq!(rest.len(), 5);
        let ExtractionEvent::BeginChild(last) = rest[0].as_ref().unwrap() else {
            panic!("missing last channel")
        };
        let ExtractionEvent::EndChild(end) = rest[4].as_ref().unwrap() else {
            panic!("missing last end")
        };
        assert!(end.same_scope(&last.control));
        assert_eq!(ids(&rest), ["500"]);
        assert!(matches!(rest[2], Ok(ExtractionEvent::Media(_))));
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[1].contains("/channel/201?o=0"));
        assert!(requests[2].contains("/channel/202?o=0"));
        assert!(
            !requests
                .iter()
                .any(|request| request.contains("o=150") || request.contains("/channel/200"))
        );
    }

    #[tokio::test]
    async fn channel_filters_skip_bad_identity_before_validation_and_http() {
        use crate::{filter::FilterExpression, job_selection::JobSelection};
        for bad in [
            json!({"id":"private/unsafe","server_id":"100","name":"bad"}),
            json!({"id":"200","server_id":"private/owner","name":"bad"}),
        ] {
            let mut payload = server_payload(&[("201", 1)]);
            payload["channels"].as_array_mut().unwrap().insert(0, bad);
            let (root, server) = serve(vec![
                response("200 OK", payload.to_string().as_bytes()),
                response(
                    "200 OK",
                    json!([message(500, "201")]).to_string().as_bytes(),
                ),
            ]);
            let client = extractor(root);
            let input = Url::parse("https://kemono.cr/discord/server/100").unwrap();
            let selected: Vec<_> = JobSelection {
                child_filter: Some(FilterExpression::parse("channel['id'] == '201'").unwrap()),
                ..JobSelection::default()
            }
            .apply(client.events(&input))
            .collect()
            .await;
            assert_eq!(selected.len(), 1);
            assert_eq!(
                selected[0]
                    .as_ref()
                    .unwrap()
                    .metadata
                    .as_ref()
                    .unwrap()
                    .post_id,
                "500"
            );
            let requests = server.join().unwrap();
            assert_eq!(requests.len(), 2);
            assert!(requests[1].contains("/channel/201?o=0"));
            assert!(!requests.iter().any(|request| request.contains("private")));
        }
    }

    #[tokio::test]
    async fn first_channel_range_stops_before_later_invalid_identity() {
        use crate::{job_selection::JobSelection, selection::RangeSelection};
        let mut payload = server_payload(&[("200", 1)]);
        payload["channels"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id":"private/unsafe","server_id":"100"}));
        let (root, server) = serve(vec![
            response("200 OK", payload.to_string().as_bytes()),
            response(
                "200 OK",
                json!([message(400, "200")]).to_string().as_bytes(),
            ),
        ]);
        let client = extractor(root);
        let input = Url::parse("https://kemono.cr/discord/server/100").unwrap();
        let selected: Vec<_> = JobSelection {
            child_range: Some(RangeSelection::parse("1").unwrap()),
            ..JobSelection::default()
        }
        .apply(client.events(&input))
        .collect()
        .await;
        assert_eq!(selected.len(), 1);
        assert_eq!(
            selected[0]
                .as_ref()
                .unwrap()
                .metadata
                .as_ref()
                .unwrap()
                .post_id,
            "400"
        );
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].contains("/channel/200?o=0"));
    }

    #[tokio::test]
    async fn selected_bad_channel_is_private_resumable_and_explicit_channel_ignores_unrelated_bad_rows()
     {
        let mut payload = server_payload(&[("200", 1), ("201", 1)]);
        payload["channels"][0]["server_id"] = json!("private/owner");
        let (root, server) = serve(vec![
            response("200 OK", payload.to_string().as_bytes()),
            response(
                "200 OK",
                json!([message(500, "201")]).to_string().as_bytes(),
            ),
        ]);
        let events: Vec<_> = extractor(root)
            .events(&Url::parse("https://kemono.cr/discord/server/100").unwrap())
            .collect()
            .await;
        assert_eq!(events.len(), 8);
        let ExtractionEvent::BeginChild(first) = events[0].as_ref().unwrap() else {
            panic!("missing invalid child scope")
        };
        assert_eq!(
            first.metadata.fields()["channel"]["server_id"],
            "private/owner"
        );
        assert_eq!(
            events[1].as_ref().unwrap_err().to_string(),
            "Kemono returned invalid Discord identity or metadata"
        );
        let ExtractionEvent::EndChild(end) = events[2].as_ref().unwrap() else {
            panic!("missing invalid child end")
        };
        assert!(first.control.same_scope(end));
        assert_eq!(
            ids(&events
                .iter()
                .skip(3)
                .map(|event| Ok(event.as_ref().unwrap().clone()))
                .collect::<Vec<_>>()),
            ["500"]
        );
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].contains("/channel/201?o=0"));
        for requested in ["200", "201"] {
            let mut replies = vec![response("200 OK", payload.to_string().as_bytes())];
            if requested == "201" {
                replies.push(response(
                    "200 OK",
                    json!([message(500, "201")]).to_string().as_bytes(),
                ));
            }
            let (root, server) = serve(replies);
            let events: Vec<_> = extractor(root)
                .events(
                    &Url::parse(&format!("https://kemono.cr/discord/server/100/{requested}"))
                        .unwrap(),
                )
                .collect()
                .await;
            assert!(!events.iter().any(|event| matches!(
                event,
                Ok(ExtractionEvent::BeginChild(_) | ExtractionEvent::EndChild(_))
            )));
            if requested == "200" {
                assert_eq!(events.len(), 1);
                assert_eq!(
                    events[0].as_ref().unwrap_err().to_string(),
                    "Kemono returned invalid Discord identity or metadata"
                );
                assert_eq!(server.join().unwrap().len(), 1);
            } else {
                assert_eq!(ids(&events), ["500"]);
                assert_eq!(server.join().unwrap().len(), 2);
            }
        }
    }

    #[tokio::test]
    async fn missing_channels_invalid_message_scope_and_repeated_pages_fail_privately() {
        let (root, server) = serve(vec![response(
            "200 OK",
            server_payload(&[("201", 0)]).to_string().as_bytes(),
        )]);
        let events: Vec<_> = extractor(root)
            .events(&Url::parse("https://kemono.cr/discord/server/100/200").unwrap())
            .collect()
            .await;
        assert_eq!(
            events[0].as_ref().unwrap_err().to_string(),
            "Kemono Discord channel not found"
        );
        server.join().unwrap();
        let mut bad = message(400, "200");
        bad["server"] = json!("private-other");
        let (root, server) = serve(vec![
            response(
                "200 OK",
                server_payload(&[("200", 1)]).to_string().as_bytes(),
            ),
            response("200 OK", json!([bad]).to_string().as_bytes()),
        ]);
        let events: Vec<_> = extractor(root)
            .events(&Url::parse("https://kemono.cr/discord/server/100/200").unwrap())
            .collect()
            .await;
        assert_eq!(events.len(), 1);
        assert!(
            !events[0]
                .as_ref()
                .unwrap_err()
                .to_string()
                .contains("private")
        );
        server.join().unwrap();
        let full: Vec<_> = (1..=150).map(|id| message(id, "200")).collect();
        let (root, server) = serve(vec![
            response(
                "200 OK",
                server_payload(&[("200", 300)]).to_string().as_bytes(),
            ),
            response("200 OK", json!(full).to_string().as_bytes()),
            response("200 OK", json!(full).to_string().as_bytes()),
        ]);
        let events: Vec<_> = extractor(root)
            .events(&Url::parse("https://kemono.cr/discord/server/100/200").unwrap())
            .collect()
            .await;
        assert_eq!(
            events.last().unwrap().as_ref().unwrap_err().to_string(),
            "Kemono Discord listing repeated a page"
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn whole_server_child_errors_are_resumable_and_skip_failed_channel_once() {
        for reply in [
            response("404 Not Found", b"{}"),
            response("200 OK", b"{}"),
            response(
                "200 OK",
                json!([message(400, "999")]).to_string().as_bytes(),
            ),
        ] {
            let (root, server) = serve(vec![
                response(
                    "200 OK",
                    server_payload(&[("200", 1), ("201", 1)])
                        .to_string()
                        .as_bytes(),
                ),
                reply,
                response(
                    "200 OK",
                    json!([message(500, "201")]).to_string().as_bytes(),
                ),
            ]);
            let events: Vec<_> = extractor(root)
                .events(&Url::parse("https://kemono.cr/discord/server/100").unwrap())
                .collect()
                .await;
            assert_eq!(events.len(), 8, "{events:?}");
            let ExtractionEvent::BeginChild(first) = events[0].as_ref().unwrap() else {
                panic!("missing first child")
            };
            assert!(events[1].is_err());
            let ExtractionEvent::EndChild(first_end) = events[2].as_ref().unwrap() else {
                panic!("missing failed child end")
            };
            assert!(first.control.same_scope(first_end));
            let ExtractionEvent::BeginChild(second) = events[3].as_ref().unwrap() else {
                panic!("missing second child")
            };
            let ExtractionEvent::EndChild(second_end) = events[7].as_ref().unwrap() else {
                panic!("missing successful child end")
            };
            assert!(second.control.same_scope(second_end));
            let ExtractionEvent::Post(post) = events[4].as_ref().unwrap() else {
                panic!("expected later channel post")
            };
            assert_eq!(post.metadata.as_ref().unwrap().post_id, "500");
            let requests = server.join().unwrap();
            assert_eq!(requests.len(), 3);
            assert!(requests[2].starts_with("GET /api/v1/discord/channel/201?o=0 "));
        }
    }

    #[tokio::test]
    async fn nonimage_only_messages_keep_source_count_when_thumbnail_mode_emits_no_files() {
        let mut post = message(400, "200");
        post["attachments"][0]["path"] = json!(format!("/aa/11/{HASH}.mp4"));
        let (root, server) = serve(vec![
            response(
                "200 OK",
                server_payload(&[("200", 1)]).to_string().as_bytes(),
            ),
            response("200 OK", json!([post]).to_string().as_bytes()),
        ]);
        let events: Vec<_> = extractor(root)
            .events(&Url::parse("https://kemono.cr/discord/server/100/200").unwrap())
            .collect()
            .await;
        assert_eq!(events.len(), 2);
        let ExtractionEvent::Post(post) = events[0].as_ref().unwrap() else {
            panic!("expected empty post")
        };
        assert_eq!(post.count, Some(1));
        assert!(post.metadata.as_ref().unwrap().file.is_none());
        assert_eq!(server.join().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn expanded_endpoint_applies_only_to_creator_listings_and_expand_remains_additive() {
        let post = json!({"id":"7","service":"patreon","user":"42","file":{},"attachments":[]});
        for (url, endpoint, expand, expanded) in [
            (
                "https://kemono.cr/patreon/user/42",
                Endpoint::Expanded,
                false,
                true,
            ),
            ("https://kemono.cr/posts", Endpoint::Expanded, false, false),
            ("https://kemono.cr/posts", Endpoint::Posts, true, true),
        ] {
            let body = if url.ends_with("/posts") {
                json!({"posts":[post.clone()]})
            } else {
                json!([post.clone()])
            };
            let mut replies = vec![response("200 OK", body.to_string().as_bytes())];
            if expanded {
                replies.push(response(
                    "200 OK",
                    json!({"post":post}).to_string().as_bytes(),
                ));
            }
            let (root, server) = serve(replies);
            let events: Vec<_> = extractor(root)
                .with_metadata(false)
                .with_endpoint(endpoint)
                .with_expand(expand)
                .events(&Url::parse(url).unwrap())
                .collect()
                .await;
            assert_eq!(events.len(), 2);
            assert!(events.iter().all(Result::is_ok));
            assert_eq!(server.join().unwrap().len(), if expanded { 2 } else { 1 });
        }
    }
}
