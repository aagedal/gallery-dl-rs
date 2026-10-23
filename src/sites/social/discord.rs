//! Discord, mirroring gallery_dl/extractor/discord.py through the v10 API.
//!
//! Every request carries the `token` option as its `Authorization` header;
//! the token is never attached to emitted media or included in errors.
//! Channel traversal (messages, threads, forum posts and category children)
//! runs as a lazy work stack so later pages are only requested as consumers
//! drain earlier ones. Channels reached from a server or category are
//! "safe": an HTTP 403 inside one skips the rest of that channel, exactly
//! like upstream's `extract_channel(..., safe=True)`.
use crate::event::ExtractionEvent;
use crate::sites::{Events, Http, Item, Route, Site, http::refused_status};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

pub static SITE: Site = Site {
    category: "discord",
    name: "Discord",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[
        &CHANNEL,
        &MESSAGE,
        &SERVER_ASSETS,
        &SERVER_SEARCH,
        &SERVER,
        &DIRECT_MESSAGES,
        &DIRECT_MESSAGE,
    ],
    api_key: false,
    cookies: None,
};

const FILENAME: &str = "{message_id}_{num:>02}_{filename[:220]}.{extension}";
const DIRECTORY: &[&str] = &[
    "{category}",
    "{server_id}_{server}",
    "{channel_id}_{channel}",
];
/// Upstream `{recipients:J,}`; gallery-dl's `J` takes everything up to an
/// optional `/`, while the template engine requires the terminator.
const DM_DIRECTORY: &[&str] = &[
    "{category}",
    "Direct Messages",
    "{channel_id}_{recipients:J,/}",
];
const ARCHIVE: &str = "{message_id}_{num}";
static CHANNEL: Route = Route::new("channel", FILENAME, DIRECTORY, ARCHIVE);
static MESSAGE: Route = Route::new("message", FILENAME, DIRECTORY, ARCHIVE);
static SERVER_ASSETS: Route = Route::new(
    "server-assets",
    "{name} ({id}).{extension}",
    &["{category}", "{server_id}_{server}", "Assets", "{label}"],
    "asset_{server_id}_{id}",
);
static SERVER_SEARCH: Route = Route::new("server-search", FILENAME, DIRECTORY, ARCHIVE);
static SERVER: Route = Route::new("server", FILENAME, DIRECTORY, ARCHIVE);
static DIRECT_MESSAGES: Route = Route::new("direct-messages", FILENAME, DM_DIRECTORY, ARCHIVE);
static DIRECT_MESSAGE: Route = Route::new("direct-message", FILENAME, DM_DIRECTORY, ARCHIVE);

const API: &str = "https://discord.com/api/v10";
const CDN: &str = "https://cdn.discordapp.com";
const MESSAGES_BATCH: usize = 100;
const THREADS_BATCH: usize = 25;
const SEARCH_BATCH: usize = 25;
/// Upstream message types with content: default, chat input, thread starter.
const MESSAGE_TYPES: [i64; 3] = [0, 19, 21];

/// Upstream `_raise_invalid_token`, without URLs.
const INVALID_TOKEN: &str = "Discord: invalid or missing token.
Please provide a valid token following these instructions:

1) Open Discord in your browser and log in;
2) Open your browser's Developer Tools (F12) and switch to the Network panel;
3) Reload the page and select any request going to the Discord API (/api/...);
4) In the \"Headers\" tab, look for an entry beginning with \"Authorization: \";
5) Right-click the entry and click \"Copy Value\";
6) Paste the token in your configuration file under \"extractor.discord.token\",
or run this command with the -o \"token=[your token]\" argument.";

#[derive(Debug, PartialEq)]
enum Target {
    Channel {
        server: String,
        channel: String,
    },
    Message {
        server: String,
        channel: String,
        message: String,
    },
    Assets {
        server: String,
        kind: Option<String>,
    },
    Search {
        server: String,
        query: String,
    },
    Server(String),
    DirectMessages(String),
    DirectMessage {
        channel: String,
        message: String,
    },
}

fn digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

/// Python `[\w-]+`.
fn word(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
}

/// `(?:https?://)?discord\.com/channels/...` with upstream's `/?$` endings.
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) || url.host_str() != Some("discord.com") {
        return None;
    }
    let rest = url.path().strip_prefix("/channels/")?;
    // `/channels/(\d+)/search/?\?([^#]+)`: a non-empty query is required and
    // anything may follow it.
    if let Some(server) = rest
        .strip_suffix("/search")
        .or_else(|| rest.strip_suffix("/search/"))
        .filter(|server| digits(server))
    {
        return url
            .query()
            .filter(|query| !query.is_empty())
            .map(|query| Target::Search {
                server: server.into(),
                query: query.into(),
            });
    }
    if url.query().is_some() || url.fragment().is_some() {
        return None;
    }
    let trimmed = rest.strip_suffix('/').unwrap_or(rest);
    let parts: Vec<&str> = trimmed.split('/').collect();
    let assets = |kind: &str| matches!(kind, "asset" | "assets" | "files");
    Some(match parts.as_slice() {
        ["@me", channel] if digits(channel) => Target::DirectMessages((*channel).into()),
        ["@me", channel, message] if digits(channel) && digits(message) => Target::DirectMessage {
            channel: (*channel).into(),
            message: (*message).into(),
        },
        [server] if digits(server) => Target::Server((*server).into()),
        [server, kind] if digits(server) && assets(kind) => Target::Assets {
            server: (*server).into(),
            kind: None,
        },
        [server, kind, name] if digits(server) && assets(kind) => {
            let name = crate::sites::percent_decode(name);
            if !word(&name) {
                return None;
            }
            Target::Assets {
                server: (*server).into(),
                kind: Some(name),
            }
        }
        [server, channel] if digits(server) && digits(channel) => Target::Channel {
            server: (*server).into(),
            channel: (*channel).into(),
        },
        [server, parent, "threads", channel]
            if digits(server) && digits(parent) && digits(channel) =>
        {
            Target::Channel {
                server: (*server).into(),
                channel: (*channel).into(),
            }
        }
        [server, channel, message] if digits(server) && digits(channel) && digits(message) => {
            Target::Message {
                server: (*server).into(),
                channel: (*channel).into(),
                message: (*message).into(),
            }
        }
        _ => return None,
    })
}

/// The `embeds` option: embed types whose media are downloaded.
enum Embeds {
    List(Vec<String>),
    /// A string option, matched as Python `type in "..."` does.
    Text(String),
    None,
}

impl Embeds {
    fn from_option(value: Option<&Value>) -> Self {
        match value {
            None => Self::List(vec!["image".into(), "gifv".into(), "video".into()]),
            Some(Value::Array(items)) => Self::List(
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect(),
            ),
            Some(Value::String(text)) => Self::Text(text.clone()),
            Some(_) => Self::None,
        }
    }

    fn enabled(&self, kind: &str) -> bool {
        match self {
            Self::List(items) => items.iter().any(|item| item == kind),
            Self::Text(text) => text.contains(kind),
            Self::None => false,
        }
    }
}

/// Extractor state: upstream's `server_metadata` and
/// `server_channels_metadata`, plus the API credentials.
struct Ctx<'a> {
    http: &'a Http,
    route: &'static Route,
    token: String,
    embeds: Embeds,
    threads: bool,
    server: Map<String, Value>,
    /// Channel metadata by channel ID, in insertion order.
    channels: Map<String, Value>,
}

fn str_of<'v>(value: &'v Value, key: &str) -> Option<&'v str> {
    value.get(key).and_then(Value::as_str)
}

fn array(value: Option<&Value>) -> &[Value] {
    value.and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

/// Python truthiness of an optional JSON value.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => *value,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

fn unexpected() -> crate::Error {
    error("Discord returned an unexpected response")
}

impl<'a> Ctx<'a> {
    fn new(http: &'a Http, route: &'static Route) -> Result<Self> {
        let token = http
            .option_str("token")
            .filter(|token| {
                !token.is_empty() && token.len() <= 4096 && !token.chars().any(char::is_control)
            })
            .ok_or_else(|| error(INVALID_TOKEN))?
            .to_owned();
        Ok(Self {
            http,
            route,
            token,
            embeds: Embeds::from_option(http.option("embeds")),
            threads: http.option_bool("threads", true),
            server: Map::new(),
            channels: Map::new(),
        })
    }

    async fn call(&self, endpoint: &str, params: &[(&str, String)]) -> Result<Value> {
        let mut url = self.http.api(&format!("{API}{endpoint}"))?;
        if !params.is_empty() {
            url.query_pairs_mut().extend_pairs(params);
        }
        match self
            .http
            .json_with(&url, &[("Authorization", self.token.as_str())])
            .await
        {
            Err(e) if refused_status(&e) == Some(401) => Err(error(INVALID_TOKEN)),
            other => other,
        }
    }

    async fn get_server(&self, server: &str) -> Result<Value> {
        self.call(&format!("/guilds/{server}"), &[]).await
    }

    async fn get_channel(&self, channel: &str) -> Result<Value> {
        self.call(&format!("/channels/{channel}"), &[]).await
    }

    /// `get_message`: the first message `around` the ID.
    async fn get_message(&self, channel: &str, message: &str) -> Result<Value> {
        let body = self
            .call(
                &format!("/channels/{channel}/messages"),
                &[("limit", "1".into()), ("around", message.into())],
            )
            .await?;
        array(Some(&body))
            .first()
            .cloned()
            .ok_or_else(|| error("Discord message not found"))
    }

    fn parse_channel(&mut self, channel: &Value) -> Map<String, Value> {
        let get = |key: &str, default: Value| channel.get(key).cloned().unwrap_or(default);
        let parent_id = get("parent_id", Value::Null);
        let mut meta = Map::new();
        meta.insert("channel".into(), get("name", "".into()));
        meta.insert("channel_id".into(), get("id", Value::Null));
        meta.insert("channel_type".into(), get("type", Value::Null));
        meta.insert("channel_topic".into(), get("topic", "".into()));
        meta.insert("parent_id".into(), parent_id.clone());
        meta.insert(
            "is_thread".into(),
            channel.get("thread_metadata").is_some().into(),
        );
        if let Some(parent) = parent_id.as_str().and_then(|id| self.channels.get(id)) {
            meta.insert(
                "parent".into(),
                parent.get("channel").cloned().unwrap_or_default(),
            );
            meta.insert(
                "parent_type".into(),
                parent.get("channel_type").cloned().unwrap_or_default(),
            );
        }
        if matches!(channel.get("type").and_then(Value::as_i64), Some(1 | 3)) {
            let recipients = array(channel.get("recipients"));
            let field = |key| {
                recipients
                    .iter()
                    .map(|user| user.get(key).cloned().unwrap_or_default())
                    .collect::<Vec<_>>()
            };
            meta.insert("channel".into(), "DMs".into());
            meta.insert("recipients".into(), field("username").into());
            meta.insert("recipients_id".into(), field("id").into());
        }
        let id = crate::sites::scalar(&meta["channel_id"]);
        self.channels.insert(id, meta.clone().into());
        meta
    }

    fn parse_server(&mut self, server: &Value) -> Map<String, Value> {
        let get = |key: &str| server.get(key).cloned().unwrap_or_default();
        let mut meta = Map::new();
        meta.insert("server".into(), get("name"));
        meta.insert("server_id".into(), get("id"));
        meta.insert("owner_id".into(), get("owner_id"));
        meta.insert(
            "server_files".into(),
            server_assets(server, None)
                .into_iter()
                .map(Value::from)
                .collect(),
        );
        self.server = meta.clone();
        meta
    }

    async fn build_server_and_channels(&mut self, server: &str) -> Result<()> {
        let info = self.get_server(server).await?;
        self.parse_server(&info);
        let body = self
            .call(&format!("/guilds/{server}/channels"), &[])
            .await?;
        let mut channels: Vec<&Value> = array(Some(&body)).iter().collect();
        // Categories first, so children see their parent's metadata.
        channels.sort_by_key(|channel| channel.get("type").and_then(Value::as_i64) != Some(4));
        for channel in channels {
            self.parse_channel(channel);
        }
        Ok(())
    }

    /// `extract_message`: one post per message snapshot pass.
    fn extract_message(&self, message: &Value) -> Vec<ExtractionEvent> {
        let content = |message: &Value| {
            message
                .get("type")
                .and_then(Value::as_i64)
                .is_some_and(|kind| MESSAGE_TYPES.contains(&kind))
        };
        if !content(message) {
            return Vec::new();
        }
        let mut meta = self.server.clone();
        if let Some(Value::Object(channel)) =
            str_of(message, "channel_id").and_then(|id| self.channels.get(id))
        {
            meta.extend(channel.clone());
        }
        let author = message.get("author").cloned().unwrap_or_default();
        let author_id = author.get("id").cloned().unwrap_or_default();
        let author_files: Vec<Value> = [("avatar", "avatars"), ("banner", "banners")]
            .into_iter()
            .filter(|(kind, _)| truthy(author.get(*kind)))
            .map(|(kind, path)| {
                let mut file = Map::new();
                file.insert(
                    "url".into(),
                    format!(
                        "{CDN}/{path}/{}/{}.png?size=4096",
                        crate::sites::scalar(&author_id),
                        crate::sites::scalar(&author[kind])
                    )
                    .into(),
                );
                file.insert("filename".into(), kind.into());
                file.insert("extension".into(), "png".into());
                file.into()
            })
            .collect();
        let text = message_text(message);
        let timestamp = str_of(message, "timestamp").and_then(crate::sites::media::iso_ts);
        let message_id = message.get("id").cloned().unwrap_or_default();
        let username = author.get("username").cloned().unwrap_or_default();
        meta.insert("author".into(), username.clone());
        meta.insert("author_id".into(), author_id);
        meta.insert("author_files".into(), author_files.into());
        meta.insert("message".into(), text.clone().into());
        meta.insert("message_id".into(), message_id.clone());
        meta.insert("date".into(), crate::sites::media::date(timestamp));
        meta.insert("files".into(), Value::Array(Vec::new()));

        let mut snapshots = vec![message];
        snapshots.extend(
            array(message.get("message_snapshots"))
                .iter()
                .filter_map(|snapshot| snapshot.get("message"))
                .filter(|snapshot| content(snapshot)),
        );
        let message_id = crate::sites::scalar(&message_id);
        let username = username.as_str().map(str::to_owned);
        let mut files: Vec<Map<String, Value>> = Vec::new();
        let mut events = Vec::new();
        // Upstream emits the accumulated file list after each snapshot.
        for snapshot in snapshots {
            for attachment in array(snapshot.get("attachments")) {
                files.push(file(attachment.get("url"), "attachment"));
            }
            for embed in array(snapshot.get("embeds")) {
                if !str_of(embed, "type").is_some_and(|kind| self.embeds.enabled(kind)) {
                    continue;
                }
                if let Some(url) = ["video", "image", "thumbnail"]
                    .into_iter()
                    .filter_map(|field| embed.get(field))
                    .find_map(|field| field.get("proxy_url").filter(|url| !url.is_null()))
                {
                    files.push(file(Some(url), "embed"));
                }
            }
            for (index, file) in files.iter_mut().enumerate() {
                let (name, ext) = file
                    .get("url")
                    .and_then(Value::as_str)
                    .and_then(|url| Url::parse(url).ok())
                    .map(|url| crate::sites::nameext(&url))
                    .unwrap_or_default();
                file.insert("filename".into(), name.into());
                file.insert("extension".into(), ext.into());
                file.insert("num".into(), (index + 1).into());
            }
            meta.insert(
                "files".into(),
                files.iter().cloned().map(Value::Object).collect(),
            );
            let items = files
                .iter()
                .filter_map(|file| {
                    let url = file
                        .get("url")
                        .and_then(Value::as_str)
                        .and_then(|url| Url::parse(url).ok())
                        .filter(crate::sites::plain)?;
                    let mut keywords = meta.clone();
                    keywords.extend(file.clone());
                    Some(
                        Item::new(url, keywords)
                            .ids(message_id.clone(), crate::sites::scalar(&file["num"]))
                            .user(username.clone())
                            .caption(Some(text.clone()))
                            .timestamp(timestamp),
                    )
                })
                .collect();
            events.extend(crate::sites::post(&SITE, self.http, self.route, items));
        }
        events
    }
}

fn file(url: Option<&Value>, kind: &str) -> Map<String, Value> {
    let mut file = Map::new();
    file.insert("url".into(), url.cloned().unwrap_or_default());
    file.insert("type".into(), kind.into());
    file
}

/// `extract_message_text`: content, rich embed text and poll text.
fn message_text(message: &Value) -> String {
    fn add<'v>(parts: &mut Vec<&'v str>, value: Option<&'v Value>) {
        if let Some(text) = value.and_then(Value::as_str).filter(|t| !t.is_empty()) {
            parts.push(text);
        }
    }
    let mut parts: Vec<&str> = Vec::new();
    let mut push = |value| add(&mut parts, value);
    push(message.get("content"));
    for embed in array(message.get("embeds")) {
        if str_of(embed, "type") == Some("rich") {
            push(embed.pointer("/author/name"));
            push(embed.get("title"));
            push(embed.get("description"));
            for field in array(embed.get("fields")) {
                push(field.get("name"));
                push(field.get("value"));
            }
            push(embed.pointer("/footer/text"));
        }
    }
    if truthy(message.get("poll")) {
        let poll = &message["poll"];
        push(poll.pointer("/question/text"));
        for answer in array(poll.get("answers")) {
            push(answer.pointer("/poll_media/text"));
        }
    }
    parts.join("\n")
}

/// `collect_server_assets`: `None`, `""` or `"general"` selects the icon,
/// banner and splash images; other types list the server's array of that
/// name (emojis, stickers).
fn server_assets(server: &Value, kind: Option<&str>) -> Vec<Map<String, Value>> {
    let server_id = server
        .get("id")
        .map(crate::sites::scalar)
        .unwrap_or_default();
    match kind {
        Some(kind) if !kind.is_empty() && kind != "general" => array(server.get(kind))
            .iter()
            .filter_map(Value::as_object)
            .filter_map(|asset| {
                let id = asset.get("id").map(crate::sites::scalar)?;
                if id.is_empty() || id.contains(['/', '\\', '?', '#']) {
                    return None;
                }
                let name = asset
                    .get("name")
                    .map(crate::sites::scalar)
                    .unwrap_or_else(|| "None".into());
                let mut out = asset.clone();
                out.insert(
                    "url".into(),
                    format!("{CDN}/{kind}/{id}.png?size=4096").into(),
                );
                out.insert("label".into(), kind.into());
                out.insert("filename".into(), format!("{name} ({id})").into());
                out.insert("extension".into(), "png".into());
                Some(out)
            })
            .collect(),
        _ => [
            ("icon", "icons"),
            ("banner", "banners"),
            ("splash", "splashes"),
            ("discovery_splash", "discovery-splashes"),
        ]
        .into_iter()
        .filter(|(kind, _)| truthy(server.get(*kind)))
        .filter_map(|(kind, path)| {
            let asset = crate::sites::scalar(&server[kind]);
            if asset.contains(['/', '\\', '?', '#']) || server_id.contains(['/', '\\', '?', '#']) {
                return None;
            }
            let mut out = Map::new();
            out.insert(
                "url".into(),
                format!("{CDN}/{path}/{server_id}/{asset}.png?size=4096").into(),
            );
            out.insert("id".into(), format!("{server_id}/{asset}").into());
            out.insert("label".into(), "".into());
            out.insert("name".into(), kind.into());
            out.insert("filename".into(), kind.into());
            out.insert("extension".into(), "png".into());
            Some(out)
        })
        .collect(),
    }
}

/// Upstream `text.parse_query_list`, keeping first values except for the
/// listed names, whose values are collected in order.
fn parse_query_list(query: &str, lists: &[&str]) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for pair in query.split('&') {
        let Some((name, value)) = pair.split_once('=') else {
            continue;
        };
        let name = crate::sites::percent_decode(&name.replace('+', " "));
        let value = crate::sites::percent_decode(&value.replace('+', " "));
        match out.iter_mut().find(|(key, _)| *key == name) {
            Some((_, values)) if lists.contains(&name.as_str()) => values.push(value),
            Some(_) => {}
            None => out.push((name, vec![value])),
        }
    }
    out
}

/// Python dict assignment: replace in place or append.
fn set_param(params: &mut Vec<(String, Vec<String>)>, key: &str, values: Vec<String>) {
    match params.iter_mut().find(|(name, _)| name == key) {
        Some((_, old)) => *old = values,
        None => params.push((key.into(), values)),
    }
}

fn pop_param(params: &mut Vec<(String, Vec<String>)>, key: &str) -> Option<Vec<String>> {
    let pos = params.iter().position(|(name, _)| name == key)?;
    Some(params.remove(pos).1)
}

/// The server-search API parameters for a URL query.
fn search_params(query: &str) -> Vec<(String, Vec<String>)> {
    let mut params = parse_query_list(
        query,
        &["from", "in", "has", "mentions", "author_id", "channel_id"],
    );
    set_param(&mut params, "sort_by", vec!["timestamp".into()]);
    set_param(&mut params, "sort_order", vec!["desc".into()]);
    if let Some(from) = pop_param(&mut params, "from") {
        set_param(&mut params, "author_id", from);
    }
    if let Some(channel) = pop_param(&mut params, "in") {
        set_param(&mut params, "channel_id", channel);
    }
    params
}

/// One pending step of the channel traversal.
enum Task {
    Channel {
        id: String,
        safe: bool,
    },
    Messages {
        channel: String,
        before: Option<String>,
    },
    Threads {
        channel: String,
        offset: usize,
    },
    Search {
        server: String,
        params: Vec<(String, Vec<String>)>,
    },
}

/// A task and the safe channels (innermost last) it runs within.
struct Frame {
    task: Task,
    guards: Vec<u64>,
}

struct Walk<'a> {
    ctx: Ctx<'a>,
    /// Pending frames; the next one is last.
    stack: Vec<Frame>,
    next_guard: u64,
}

impl Walk<'_> {
    fn frame(&mut self, task: Task, parent: &[u64]) -> Frame {
        let mut guards = parent.to_vec();
        if let Task::Channel { safe: true, .. } = task {
            self.next_guard += 1;
            guards.push(self.next_guard);
        }
        Frame { task, guards }
    }

    /// Run one frame; returns its events and follow-up tasks in order.
    async fn step(&mut self, task: &Task) -> Result<(Vec<ExtractionEvent>, Vec<Task>)> {
        let ctx = &mut self.ctx;
        match task {
            Task::Channel { id, safe } => {
                if !ctx.channels.contains_key(id) {
                    let channel = ctx.get_channel(id).await?;
                    ctx.parse_channel(&channel);
                }
                let kind = ctx
                    .channels
                    .get(id.as_str())
                    .and_then(|channel| channel.get("channel_type"))
                    .and_then(Value::as_i64);
                let messages = || Task::Messages {
                    channel: id.clone(),
                    before: None,
                };
                let threads = || Task::Threads {
                    channel: id.clone(),
                    offset: 0,
                };
                // https://discord.com/developers/docs/resources/channel#channel-object-channel-types
                let tasks = match kind {
                    Some(0 | 5) if ctx.threads => vec![messages(), threads()],
                    Some(0 | 5 | 1 | 3 | 10 | 11 | 12) => vec![messages()],
                    Some(15 | 16) => vec![threads()],
                    Some(4) => ctx
                        .channels
                        .values()
                        .filter(|channel| str_of(channel, "parent_id") == Some(id.as_str()))
                        .filter_map(|channel| str_of(channel, "channel_id"))
                        .filter(|child| digits(child))
                        .map(|child| Task::Channel {
                            id: child.into(),
                            safe: true,
                        })
                        .collect(),
                    _ if !safe => {
                        return Err(error("Discord: This channel type is not supported."));
                    }
                    _ => Vec::new(),
                };
                Ok((Vec::new(), tasks))
            }
            Task::Messages { channel, before } => {
                let mut params = vec![("limit", MESSAGES_BATCH.to_string())];
                if let Some(before) = before {
                    params.push(("before", before.clone()));
                }
                let body = ctx
                    .call(&format!("/channels/{channel}/messages"), &params)
                    .await?;
                let messages = body.as_array().ok_or_else(unexpected)?;
                let events = messages
                    .iter()
                    .flat_map(|message| ctx.extract_message(message))
                    .collect();
                let next = messages
                    .last()
                    .and_then(|last| str_of(last, "id"))
                    .filter(|id| messages.len() >= MESSAGES_BATCH && digits(id))
                    .map(|id| Task::Messages {
                        channel: channel.clone(),
                        before: Some(id.into()),
                    });
                Ok((events, next.into_iter().collect()))
            }
            Task::Threads { channel, offset } => {
                let body = ctx
                    .call(
                        &format!("/channels/{channel}/threads/search"),
                        &[
                            ("sort_by", "last_message_time".into()),
                            ("sort_order", "desc".into()),
                            ("limit", THREADS_BATCH.to_string()),
                            ("offset", offset.to_string()),
                        ],
                    )
                    .await?;
                let threads = array(body.get("threads"));
                let mut tasks = Vec::new();
                for thread in threads {
                    let meta = ctx.parse_channel(thread);
                    if let Some(id) = meta.get("channel_id").and_then(Value::as_str)
                        && digits(id)
                    {
                        tasks.push(Task::Messages {
                            channel: id.into(),
                            before: None,
                        });
                    }
                }
                if threads.len() >= THREADS_BATCH {
                    tasks.push(Task::Threads {
                        channel: channel.clone(),
                        offset: offset + threads.len(),
                    });
                }
                Ok((Vec::new(), tasks))
            }
            Task::Search { server, params } => {
                let pairs: Vec<(&str, String)> = params
                    .iter()
                    .flat_map(|(key, values)| values.iter().map(|v| (key.as_str(), v.clone())))
                    .collect();
                let body = ctx
                    .call(&format!("/guilds/{server}/messages/search"), &pairs)
                    .await?;
                let groups = body
                    .get("messages")
                    .and_then(Value::as_array)
                    .ok_or_else(unexpected)?;
                let max_id = groups
                    .iter()
                    .flat_map(|group| array(Some(group)))
                    .filter_map(|message| str_of(message, "id")?.parse::<u64>().ok())
                    .min()
                    .unwrap_or(0);
                let mut events = Vec::new();
                for message in groups.iter().flat_map(|group| array(Some(group))) {
                    if let Some(channel) = str_of(message, "channel_id")
                        && !ctx.channels.contains_key(channel)
                        && digits(channel)
                    {
                        let info = ctx.get_channel(channel).await?;
                        ctx.parse_channel(&info);
                    }
                    events.extend(ctx.extract_message(message));
                }
                let mut tasks = Vec::new();
                if groups.len() >= SEARCH_BATCH {
                    let mut params = params.clone();
                    set_param(&mut params, "max_id", vec![max_id.to_string()]);
                    tasks.push(Task::Search {
                        server: server.clone(),
                        params,
                    });
                }
                Ok((events, tasks))
            }
        }
    }
}

/// Run tasks depth-first, one request per step.
fn walk(ctx: Ctx<'_>, tasks: Vec<Task>) -> Events<'_> {
    let mut state = Walk {
        ctx,
        stack: Vec::new(),
        next_guard: 0,
    };
    let frames: Vec<Frame> = tasks
        .into_iter()
        .map(|task| state.frame(task, &[]))
        .collect();
    state.stack.extend(frames.into_iter().rev());
    crate::sites::paginate(state, |mut state| async move {
        let Some(frame) = state.stack.pop() else {
            return Ok(None);
        };
        match state.step(&frame.task).await {
            Ok((events, tasks)) => {
                let frames: Vec<Frame> = tasks
                    .into_iter()
                    .map(|task| state.frame(task, &frame.guards))
                    .collect();
                state.stack.extend(frames.into_iter().rev());
                Ok(Some((crate::sites::events(events), state)))
            }
            // A safe channel ends silently on HTTP 403, with everything it
            // still had pending.
            Err(e) if refused_status(&e) == Some(403) && !frame.guards.is_empty() => {
                let guard = frame.guards[frame.guards.len() - 1];
                while state
                    .stack
                    .last()
                    .is_some_and(|pending| pending.guards.contains(&guard))
                {
                    state.stack.pop();
                }
                Ok(Some((crate::sites::events(Vec::new()), state)))
            }
            Err(e) => Err(e),
        }
    })
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Discord URL"));
    };
    let route = match &target {
        Target::Channel { .. } => &CHANNEL,
        Target::Message { .. } => &MESSAGE,
        Target::Assets { .. } => &SERVER_ASSETS,
        Target::Search { .. } => &SERVER_SEARCH,
        Target::Server(_) => &SERVER,
        Target::DirectMessages(_) => &DIRECT_MESSAGES,
        Target::DirectMessage { .. } => &DIRECT_MESSAGE,
    };
    let mut ctx = match Ctx::new(http, route) {
        Ok(ctx) => ctx,
        Err(e) => return crate::sites::fail(e),
    };
    match target {
        Target::Channel { server, channel } => crate::sites::once(async move {
            ctx.build_server_and_channels(&server).await?;
            Ok(walk(
                ctx,
                vec![Task::Channel {
                    id: channel,
                    safe: false,
                }],
            ))
        }),
        Target::Message {
            server,
            channel,
            message,
        } => crate::sites::once(async move {
            ctx.build_server_and_channels(&server).await?;
            if !ctx.channels.contains_key(&channel) {
                let info = ctx.get_channel(&channel).await?;
                ctx.parse_channel(&info);
            }
            let message = ctx.get_message(&channel, &message).await?;
            Ok(crate::sites::events(ctx.extract_message(&message)))
        }),
        Target::Assets { server, kind } => crate::sites::once(async move {
            let info = ctx.get_server(&server).await?;
            let mut parsed = ctx.parse_server(&info);
            let kinds: Vec<String> = match kind {
                None => vec![String::new(), "emojis".into(), "stickers".into()],
                Some(kind) => kind.split(',').map(str::to_owned).collect(),
            };
            let server_id = parsed
                .get("server_id")
                .map(crate::sites::scalar)
                .unwrap_or_default();
            let mut events = Vec::new();
            for kind in kinds {
                let assets = server_assets(&info, Some(&kind));
                parsed.insert("count".into(), assets.len().into());
                parsed.insert("label".into(), kind.clone().into());
                let items = assets
                    .into_iter()
                    .filter_map(|mut asset| {
                        let url = asset
                            .get("url")
                            .and_then(Value::as_str)
                            .and_then(|url| Url::parse(url).ok())?;
                        let id = asset
                            .get("id")
                            .map(crate::sites::scalar)
                            .unwrap_or_default();
                        asset.extend(parsed.clone());
                        Some(Item::new(url, asset).ids(server_id.clone(), id))
                    })
                    .collect();
                events.extend(crate::sites::post(&SITE, http, &SERVER_ASSETS, items));
            }
            Ok(crate::sites::events(events))
        }),
        Target::Search { server, query } => crate::sites::once(async move {
            let info = ctx.get_server(&server).await?;
            ctx.parse_server(&info);
            let params = search_params(&query);
            Ok(walk(ctx, vec![Task::Search { server, params }]))
        }),
        Target::Server(server) => crate::sites::once(async move {
            ctx.build_server_and_channels(&server).await?;
            let tasks = ctx
                .channels
                .values()
                .filter(|channel| {
                    matches!(
                        channel.get("channel_type").and_then(Value::as_i64),
                        Some(0 | 5 | 15 | 16)
                    )
                })
                .filter_map(|channel| str_of(channel, "channel_id"))
                .filter(|id| digits(id))
                .map(|id| Task::Channel {
                    id: id.into(),
                    safe: true,
                })
                .collect();
            Ok(walk(ctx, tasks))
        }),
        Target::DirectMessages(channel) => walk(
            ctx,
            vec![Task::Channel {
                id: channel,
                safe: false,
            }],
        ),
        Target::DirectMessage { channel, message } => crate::sites::once(async move {
            let info = ctx.get_channel(&channel).await?;
            ctx.parse_channel(&info);
            let message = ctx.get_message(&channel, &message).await?;
            Ok(crate::sites::events(ctx.extract_message(&message)))
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http, json, keywords, media, response, serve};
    use futures_util::StreamExt;
    use serde_json::json;
    use std::sync::Arc;

    const TOKEN: &str = "SECRET-TOKEN-123";

    fn client(root: Url, options: Value) -> Http {
        let mut map = options.as_object().cloned().unwrap_or_default();
        map.entry("token").or_insert(TOKEN.into());
        http("Discord", root).with_options(Some(Arc::new(map)))
    }

    fn u(raw: &str) -> Url {
        Url::parse(raw).unwrap()
    }

    async fn error_of(events: Events<'_>) -> String {
        let mut stream = std::pin::pin!(crate::event::media_only(events));
        while let Some(item) = stream.next().await {
            if let Err(e) = item {
                return e.to_string();
            }
        }
        panic!("extraction succeeded")
    }

    fn forbidden() -> Vec<u8> {
        response("403 Forbidden", "application/json", b"{\"code\":50001}")
    }

    fn guild() -> Value {
        json!({"id": "1", "name": "Guild", "owner_id": "9", "icon": "ic",
            "banner": null, "splash": "sp",
            "emojis": [{"id": "50", "name": "smile", "animated": false}],
            "stickers": [{"id": "60", "name": "wave", "format_type": 1}]})
    }

    fn msg(id: &str, channel: &str, attachments: &[&str]) -> Value {
        json!({"id": id, "type": 0, "channel_id": channel, "content": format!("m{id}"),
            "timestamp": "2024-01-02T03:04:05.678000+00:00",
            "author": {"id": "7", "username": "alice", "avatar": null},
            "attachments": attachments.iter().map(|url| json!({"url": url})).collect::<Vec<_>>(),
            "embeds": []})
    }

    fn cdn(name: &str) -> String {
        format!("https://cdn.discordapp.com/attachments/10/1/{name}")
    }

    fn has_token(request: &str) -> bool {
        request
            .to_ascii_lowercase()
            .contains(&format!("authorization: {}", TOKEN.to_ascii_lowercase()))
    }

    fn request_line(request: &str) -> &str {
        request.lines().next().unwrap()
    }

    #[test]
    fn url_patterns_match_upstream() {
        let t = |raw: &str| target(&u(raw));
        assert_eq!(
            t("https://discord.com/channels/1234567890/9876543210"),
            Some(Target::Channel {
                server: "1234567890".into(),
                channel: "9876543210".into()
            })
        );
        assert_eq!(
            t("http://discord.com/channels/1/2/threads/3/"),
            Some(Target::Channel {
                server: "1".into(),
                channel: "3".into()
            })
        );
        assert_eq!(
            t("https://discord.com/channels/1/2/3"),
            Some(Target::Message {
                server: "1".into(),
                channel: "2".into(),
                message: "3".into()
            })
        );
        assert_eq!(
            t("https://discord.com/channels/1/assets"),
            Some(Target::Assets {
                server: "1".into(),
                kind: None
            })
        );
        assert_eq!(
            t("https://discord.com/channels/1/asset/"),
            Some(Target::Assets {
                server: "1".into(),
                kind: None
            })
        );
        assert_eq!(
            t("https://discord.com/channels/1/files/emoji-s_2"),
            Some(Target::Assets {
                server: "1".into(),
                kind: Some("emoji-s_2".into())
            })
        );
        assert_eq!(
            t("https://discord.com/channels/1/search?content=a#x"),
            Some(Target::Search {
                server: "1".into(),
                query: "content=a".into()
            })
        );
        assert_eq!(
            t("https://discord.com/channels/1/search/?from=2"),
            Some(Target::Search {
                server: "1".into(),
                query: "from=2".into()
            })
        );
        assert_eq!(
            t("https://discord.com/channels/1/"),
            Some(Target::Server("1".into()))
        );
        assert_eq!(
            t("https://discord.com/channels/@me/5"),
            Some(Target::DirectMessages("5".into()))
        );
        assert_eq!(
            t("https://discord.com/channels/@me/5/6/"),
            Some(Target::DirectMessage {
                channel: "5".into(),
                message: "6".into()
            })
        );
        for raw in [
            "https://discord.com/channels/",
            "https://discord.com/channels/x",
            "https://discord.com/channels/1//",
            "https://discord.com/channels/1?x=1",
            "https://discord.com/channels/1#x",
            "https://discord.com/channels/1/2/3/4",
            "https://discord.com/channels/1/2/thread/3",
            "https://discord.com/channels/1/search",
            "https://discord.com/channels/1/search?",
            "https://discord.com/channels/1/assets/a.b",
            "https://discord.com/channels/1/media",
            "https://discord.com/channels/@me",
            "https://discord.com/channels/@me/x",
            "https://discord.com/channels/@me/1/2/3",
            "https://www.discord.com/channels/1",
            "https://discordapp.com/channels/1",
            "https://discord.com.evil.test/channels/1",
            "https://discord.com:8443/channels/1",
            "https://u:p@discord.com/channels/1",
            "ftp://discord.com/channels/1",
            "https://discord.com/app",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
        assert_eq!(
            crate::sites::find(&u("https://discord.com/channels/1/2")).map(|s| s.category),
            Some("discord")
        );
    }

    #[test]
    fn search_query_mirrors_parse_query_list() {
        let params = search_params("content=a+b%21&from=7&from=8&in=10&has=image&has=link&x");
        assert_eq!(
            params,
            vec![
                ("content".into(), vec!["a b!".into()]),
                ("has".into(), vec!["image".into(), "link".into()]),
                ("sort_by".into(), vec!["timestamp".into()]),
                ("sort_order".into(), vec!["desc".into()]),
                ("author_id".into(), vec!["7".into(), "8".into()]),
                ("channel_id".into(), vec!["10".into()]),
            ]
        );
        // An explicit `author_id` keeps its position when `from` replaces it.
        let params = search_params("author_id=1&from=2&sort_by=relevance");
        assert_eq!(params[0], ("author_id".into(), vec!["2".into()]));
        assert_eq!(params[1], ("sort_by".into(), vec!["timestamp".into()]));
    }

    #[tokio::test]
    async fn missing_token_fails_without_requests() {
        let (root, server) = serve(Vec::new());
        let client = http("Discord", root);
        let message = error_of(extract(&client, u("https://discord.com/channels/1/2"))).await;
        assert!(message.contains("invalid or missing token"), "{message}");
        assert!(message.contains("extractor.discord.token"));
        assert!(!message.contains("http"));
        // The fixture server never received a connection.
        assert!(server.join().unwrap().is_empty());
    }

    #[tokio::test]
    async fn rejected_token_is_an_authentication_error() {
        let (root, server) = serve(vec![response(
            "401 Unauthorized",
            "application/json",
            b"{}",
        )]);
        let client = client(root, json!({}));
        let message = error_of(extract(&client, u("https://discord.com/channels/1"))).await;
        assert!(
            message.starts_with("Discord: invalid or missing token."),
            "{message}"
        );
        assert!(!message.contains(TOKEN));
        assert!(!message.contains("127.0.0.1"));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn text_channel_paginates_messages_then_threads() {
        // 100 messages: only the newest is a default message with a file.
        let page: Vec<Value> = (901..=1000)
            .rev()
            .map(|id| {
                let mut message = msg(&id.to_string(), "10", &[]);
                if id == 1000 {
                    message["attachments"] = json!([{"url": cdn("a.png")}]);
                } else {
                    message["type"] = json!(7);
                }
                message
            })
            .collect();
        let (root, server) = serve(vec![
            json(guild()),
            json(json!([
                {"id": "10", "type": 0, "name": "general", "parent_id": "5", "topic": "chat"},
                {"id": "5", "type": 4, "name": "Cat", "parent_id": null}
            ])),
            json(Value::Array(page)),
            json(json!([msg("900", "10", &[&cdn("b.png")])])),
            json(json!({"threads": [
                {"id": "20", "type": 11, "name": "thr", "parent_id": "10", "thread_metadata": {}}
            ]})),
            json(json!([msg("2000", "20", &[&cdn("c.gif")])])),
        ]);
        let client = client(root, json!({}));
        let files = media(extract(&client, u("https://discord.com/channels/1/10"))).await;
        let names: Vec<&str> = files.iter().map(|m| m.filename.as_str()).collect();
        assert_eq!(names, ["a.png", "b.png", "c.gif"]);
        let k = keywords(&files[0]);
        assert_eq!(k["category"], "discord");
        assert_eq!(k["subcategory"], "channel");
        assert_eq!(k["server"], "Guild");
        assert_eq!(k["server_id"], "1");
        assert_eq!(k["owner_id"], "9");
        assert_eq!(k["channel"], "general");
        assert_eq!(k["channel_topic"], "chat");
        assert_eq!(k["channel_type"], 0);
        assert_eq!(k["parent"], "Cat");
        assert_eq!(k["parent_type"], 4);
        assert_eq!(k["is_thread"], false);
        assert_eq!(k["author"], "alice");
        assert_eq!(k["author_id"], "7");
        assert_eq!(k["author_files"], json!([]));
        assert_eq!(k["message"], "m1000");
        assert_eq!(k["message_id"], "1000");
        assert_eq!(k["date"], "2024-01-02 03:04:05");
        assert_eq!(k["num"], 1);
        assert_eq!(k["type"], "attachment");
        assert_eq!(k["filename"], "a");
        assert_eq!(k["extension"], "png");
        assert_eq!(k["url"], cdn("a.png"));
        let thread = keywords(&files[2]);
        assert_eq!(thread["channel"], "thr");
        assert_eq!(thread["channel_id"], "20");
        assert_eq!(thread["is_thread"], true);
        assert_eq!(thread["parent"], "general");
        assert_eq!(thread["parent_type"], 0);

        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "1000_01_a.png");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("discord/1_Guild/10_general")
        );
        let (namespace, identity) = crate::sites::archive_key(&files[1]).unwrap();
        assert_eq!(namespace, "discord");
        assert_eq!(identity, r#"["{message_id}_{num}",["900","1"]]"#);
        assert!(files[0].archive_key().unwrap().starts_with("discord:v1:"));
        assert!(files.iter().all(|m| m.headers.is_empty()));

        let requests = server.join().unwrap();
        let lines: Vec<&str> = requests.iter().map(|r| request_line(r)).collect();
        assert_eq!(
            lines,
            [
                "GET /api/v10/guilds/1 HTTP/1.1",
                "GET /api/v10/guilds/1/channels HTTP/1.1",
                "GET /api/v10/channels/10/messages?limit=100 HTTP/1.1",
                "GET /api/v10/channels/10/messages?limit=100&before=901 HTTP/1.1",
                "GET /api/v10/channels/10/threads/search?sort_by=last_message_time&sort_order=desc&limit=25&offset=0 HTTP/1.1",
                "GET /api/v10/channels/20/messages?limit=100 HTTP/1.1",
            ]
        );
        assert!(requests.iter().all(|r| has_token(r)));
    }

    #[tokio::test]
    async fn threads_option_disables_thread_traversal() {
        let (root, server) = serve(vec![
            json(guild()),
            json(json!([{"id": "10", "type": 5, "name": "news"}])),
            json(json!([msg("3", "10", &[&cdn("a.png")])])),
        ]);
        let client = client(root, json!({"threads": false}));
        let files = media(extract(&client, u("https://discord.com/channels/1/10"))).await;
        assert_eq!(files.len(), 1);
        assert_eq!(server.join().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn forum_paginates_threads_by_offset() {
        let threads: Vec<Value> = (100..125)
            .map(|id| {
                json!({"id": id.to_string(), "type": 11, "name": format!("post{id}"),
                "parent_id": "30", "thread_metadata": {"archived": false}})
            })
            .collect();
        let mut replies = vec![
            json(guild()),
            json(json!([{"id": "30", "type": 15, "name": "forum"}])),
            json(json!({ "threads": threads })),
            json(json!([msg("7", "100", &[&cdn("first.png")])])),
        ];
        replies.extend((101..125).map(|_| json(json!([]))));
        replies.push(json(json!({"threads": [
            {"id": "200", "type": 11, "name": "old", "parent_id": "30", "thread_metadata": {}}
        ]})));
        replies.push(json(json!([msg("8", "200", &[&cdn("last.png")])])));
        let (root, server) = serve(replies);
        let client = client(root, json!({}));
        let files = media(extract(&client, u("https://discord.com/channels/1/30"))).await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[0]);
        assert_eq!(k["channel"], "post100");
        assert_eq!(k["parent"], "forum");
        assert_eq!(k["parent_type"], 15);
        assert_eq!(keywords(&files[1])["channel"], "old");
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 30);
        assert!(request_line(&requests[2]).contains("/channels/30/threads/search?"));
        assert!(request_line(&requests[2]).contains("&offset=0 "));
        assert!(request_line(&requests[3]).starts_with("GET /api/v10/channels/100/messages?"));
        assert!(request_line(&requests[28]).contains("/channels/30/threads/search?"));
        assert!(request_line(&requests[28]).contains("&offset=25 "));
        assert!(request_line(&requests[29]).starts_with("GET /api/v10/channels/200/messages?"));
    }

    #[tokio::test]
    async fn thread_url_fetches_unlisted_thread() {
        let (root, server) = serve(vec![
            json(guild()),
            json(json!([{"id": "30", "type": 15, "name": "forum"}])),
            json(
                json!({"id": "31", "type": 11, "name": "post", "parent_id": "30",
                "thread_metadata": {}}),
            ),
            json(json!([msg("9", "31", &[&cdn("x.jpg")])])),
        ]);
        let client = client(root, json!({}));
        let files = media(extract(
            &client,
            u("https://discord.com/channels/1/30/threads/31"),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let k = keywords(&files[0]);
        assert_eq!(k["channel"], "post");
        assert_eq!(k["parent"], "forum");
        let requests = server.join().unwrap();
        assert_eq!(
            request_line(&requests[2]),
            "GET /api/v10/channels/31 HTTP/1.1"
        );
    }

    #[tokio::test]
    async fn server_skips_forbidden_channels() {
        let (root, server) = serve(vec![
            json(guild()),
            json(json!([
                {"id": "10", "type": 0, "name": "locked"},
                {"id": "11", "type": 2, "name": "voice"},
                {"id": "12", "type": 15, "name": "forum"},
                {"id": "5", "type": 4, "name": "Cat"}
            ])),
            // Channel 10: messages are forbidden; its thread listing is skipped.
            forbidden(),
            json(json!({"threads": [
                {"id": "40", "type": 11, "name": "t", "parent_id": "12", "thread_metadata": {}}
            ]})),
            json(json!([msg("4", "40", &[&cdn("ok.png")])])),
        ]);
        let client = client(root, json!({}));
        let files = media(extract(&client, u("https://discord.com/channels/1"))).await;
        assert_eq!(files.len(), 1);
        assert_eq!(keywords(&files[0])["subcategory"], "server");
        assert_eq!(keywords(&files[0])["channel"], "t");
        let requests = server.join().unwrap();
        let lines: Vec<&str> = requests.iter().map(|r| request_line(r)).collect();
        assert_eq!(
            lines[2],
            "GET /api/v10/channels/10/messages?limit=100 HTTP/1.1"
        );
        assert!(lines[3].starts_with("GET /api/v10/channels/12/threads/search?"));
        assert_eq!(lines.len(), 5);
    }

    #[tokio::test]
    async fn category_channel_traverses_children_safely() {
        let (root, server) = serve(vec![
            json(guild()),
            json(json!([
                {"id": "10", "type": 0, "name": "locked", "parent_id": "5"},
                {"id": "11", "type": 0, "name": "open", "parent_id": "5"},
                {"id": "12", "type": 0, "name": "elsewhere"},
                {"id": "5", "type": 4, "name": "Cat"}
            ])),
            forbidden(),
            json(json!([msg("6", "11", &[&cdn("y.png")])])),
        ]);
        let client = client(root, json!({"threads": 0}));
        let files = media(extract(&client, u("https://discord.com/channels/1/5"))).await;
        assert_eq!(files.len(), 1);
        assert_eq!(keywords(&files[0])["channel"], "open");
        assert_eq!(keywords(&files[0])["parent"], "Cat");
        assert_eq!(server.join().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn forbidden_requested_channel_is_an_error() {
        let (root, server) = serve(vec![
            json(guild()),
            json(json!([{"id": "10", "type": 0, "name": "locked"}])),
            forbidden(),
        ]);
        let client = client(root, json!({}));
        let message = error_of(extract(&client, u("https://discord.com/channels/1/10"))).await;
        assert_eq!(
            message,
            "Discord refused access; the resource may need authentication"
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn unsupported_channel_type_is_an_error() {
        let (root, server) = serve(vec![
            json(guild()),
            json(json!([{"id": "11", "type": 2, "name": "voice"}])),
        ]);
        let client = client(root, json!({}));
        let message = error_of(extract(&client, u("https://discord.com/channels/1/11"))).await;
        assert_eq!(message, "Discord: This channel type is not supported.");
        server.join().unwrap();
    }

    fn rich_message() -> Value {
        json!({
            "id": "77", "type": 0, "channel_id": "10", "content": "hello",
            "timestamp": "2024-01-02T03:04:05+00:00",
            "author": {"id": "7", "username": "alice", "avatar": "av", "banner": "bn"},
            "attachments": [{"url": "https://cdn.discordapp.com/attachments/10/77/photo.JPG?ex=1"}],
            "embeds": [
                {"type": "rich", "title": "T", "description": "D", "author": {"name": "A"},
                 "fields": [{"name": "F", "value": "V"}, {"name": "", "value": "W"}],
                 "footer": {"text": "Foot"},
                 "image": {"proxy_url": "https://media.discordapp.net/x/rich.png"}},
                {"type": "image", "image": {"url": "https://example.test/raw.png"},
                 "thumbnail": {"proxy_url": "https://media.discordapp.net/x/thumb.webp"}},
                {"type": "gifv", "video": {"proxy_url": "https://media.discordapp.net/x/v.mp4"},
                 "thumbnail": {"proxy_url": "https://media.discordapp.net/x/t.png"}},
                {"type": "article", "thumbnail": {"proxy_url": "https://media.discordapp.net/x/art.png"}}
            ],
            "poll": {"question": {"text": "Q?"},
                     "answers": [{"poll_media": {"text": "yes"}}, {"poll_media": {"emoji": {}}}]},
            "message_snapshots": [
                {"message": {"type": 0, "content": "", "embeds": [],
                    "attachments": [{"url": "https://cdn.discordapp.com/attachments/1/2/fwd.png"}]}},
                {"message": {"type": 7, "embeds": [],
                    "attachments": [{"url": "https://cdn.discordapp.com/attachments/1/2/no.png"}]}}
            ]
        })
    }

    #[tokio::test]
    async fn message_collects_attachments_embeds_and_snapshots() {
        let (root, server) = serve(vec![
            json(guild()),
            json(json!([{"id": "10", "type": 0, "name": "general"}])),
            json(json!([rich_message()])),
        ]);
        let client = client(root, json!({}));
        let files = media(extract(&client, u("https://discord.com/channels/1/10/77"))).await;
        // Upstream yields the accumulated list after each snapshot.
        let names: Vec<&str> = files.iter().map(|m| m.filename.as_str()).collect();
        assert_eq!(
            names,
            [
                "photo.jpg",
                "thumb.webp",
                "v.mp4",
                "photo.jpg",
                "thumb.webp",
                "v.mp4",
                "fwd.png"
            ]
        );
        let nums: Vec<i64> = files
            .iter()
            .map(|m| keywords(m)["num"].as_i64().unwrap())
            .collect();
        assert_eq!(nums, [1, 2, 3, 1, 2, 3, 4]);
        let k = keywords(&files[1]);
        assert_eq!(k["subcategory"], "message");
        assert_eq!(k["type"], "embed");
        assert_eq!(k["url"], "https://media.discordapp.net/x/thumb.webp");
        assert_eq!(k["message"], "hello\nA\nT\nD\nF\nV\nW\nFoot\nQ?\nyes");
        assert_eq!(
            k["author_files"],
            json!([
                {"url": "https://cdn.discordapp.com/avatars/7/av.png?size=4096",
                 "filename": "avatar", "extension": "png"},
                {"url": "https://cdn.discordapp.com/banners/7/bn.png?size=4096",
                 "filename": "banner", "extension": "png"}
            ])
        );
        assert_eq!(k["files"].as_array().unwrap().len(), 3);
        assert_eq!(k["files"][0]["filename"], "photo");
        assert_eq!(k["files"][0]["extension"], "jpg");
        assert_eq!(k["files"][0]["num"], 1);
        assert_eq!(keywords(&files[6])["files"].as_array().unwrap().len(), 4);
        assert_eq!(k["server_files"][0]["name"], "icon");
        assert_eq!(k["date"], "2024-01-02 03:04:05");
        let (file, _) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "77_01_photo.jpg");
        let requests = server.join().unwrap();
        assert_eq!(
            request_line(&requests[2]),
            "GET /api/v10/channels/10/messages?limit=1&around=77 HTTP/1.1"
        );
    }

    #[tokio::test]
    async fn direct_messages_use_recipients_and_embed_option() {
        let dm = json!({"id": "5", "type": 3, "recipients": [
            {"id": "7", "username": "alice"}, {"id": "8", "username": "bob"}]});
        let mut message = rich_message();
        message["channel_id"] = json!("5");
        message["message_snapshots"] = json!([]);
        message["attachments"] = json!([]);
        let (root, server) = serve(vec![json(dm.clone()), json(json!([message.clone()]))]);
        let api = client(root, json!({"embeds": ["rich", "article"]}));
        let files = media(extract(&api, u("https://discord.com/channels/@me/5"))).await;
        let names: Vec<&str> = files.iter().map(|m| m.filename.as_str()).collect();
        assert_eq!(names, ["rich.png", "art.png"]);
        let k = keywords(&files[0]);
        assert_eq!(k["subcategory"], "direct-messages");
        assert_eq!(k["channel"], "DMs");
        assert_eq!(k["recipients"], json!(["alice", "bob"]));
        assert_eq!(k["recipients_id"], json!(["7", "8"]));
        assert!(!k.contains_key("server"));
        let (_, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("discord/Direct Messages/5_alice,bob")
        );
        let requests = server.join().unwrap();
        assert_eq!(
            request_line(&requests[0]),
            "GET /api/v10/channels/5 HTTP/1.1"
        );

        let (root, server) = serve(vec![json(dm), json(json!([message]))]);
        let client = client(root, json!({"embeds": []}));
        let files = media(extract(&client, u("https://discord.com/channels/@me/5/77"))).await;
        assert!(files.is_empty());
        let requests = server.join().unwrap();
        assert_eq!(
            request_line(&requests[1]),
            "GET /api/v10/channels/5/messages?limit=1&around=77 HTTP/1.1"
        );
    }

    #[tokio::test]
    async fn missing_message_is_an_error() {
        let (root, server) = serve(vec![
            json(json!({"id": "5", "type": 1, "recipients": []})),
            json(json!([])),
        ]);
        let client = client(root, json!({}));
        let message = error_of(extract(&client, u("https://discord.com/channels/@me/5/6"))).await;
        assert_eq!(message, "Discord message not found");
        server.join().unwrap();
    }

    #[tokio::test]
    async fn server_assets_cover_general_emojis_and_stickers() {
        let (root, server) = serve(vec![json(guild())]);
        let api = client(root, json!({}));
        let files = media(extract(&api, u("https://discord.com/channels/1/assets"))).await;
        let urls: Vec<&str> = files.iter().map(|m| m.url.as_str()).collect();
        assert_eq!(
            urls,
            [
                "https://cdn.discordapp.com/icons/1/ic.png?size=4096",
                "https://cdn.discordapp.com/splashes/1/sp.png?size=4096",
                "https://cdn.discordapp.com/emojis/50.png?size=4096",
                "https://cdn.discordapp.com/stickers/60.png?size=4096",
            ]
        );
        let icon = keywords(&files[0]);
        assert_eq!(icon["subcategory"], "server-assets");
        assert_eq!(icon["id"], "1/ic");
        assert_eq!(icon["name"], "icon");
        assert_eq!(icon["label"], "");
        assert_eq!(icon["count"], 2);
        assert_eq!(icon["server"], "Guild");
        let emoji = keywords(&files[2]);
        assert_eq!(emoji["label"], "emojis");
        assert_eq!(emoji["filename"], "smile (50)");
        assert_eq!(emoji["animated"], false);
        assert_eq!(emoji["count"], 1);
        let (file, dir) = crate::sites::default_templates(&files[2]).unwrap();
        assert_eq!(file.render(&files[2]).unwrap(), "smile (50).png");
        assert_eq!(
            dir.render(&files[2]).unwrap().unwrap(),
            std::path::PathBuf::from("discord/1_Guild/Assets/emojis")
        );
        let (_, identity) = crate::sites::archive_key(&files[3]).unwrap();
        assert_eq!(identity, r#"["asset_{server_id}_{id}",["1","60"]]"#);
        server.join().unwrap();

        let (root, server) = serve(vec![json(guild())]);
        let client = client(root, json!({}));
        let files = media(extract(
            &client,
            u("https://discord.com/channels/1/files/stickers"),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(keywords(&files[0])["label"], "stickers");
        server.join().unwrap();
    }

    #[tokio::test]
    async fn server_search_paginates_with_max_id() {
        let page: Vec<Value> = (476..=500)
            .rev()
            .map(|id| json!([msg(&id.to_string(), "10", &[&cdn(&format!("{id}.png"))])]))
            .collect();
        let (root, server) = serve(vec![
            json(guild()),
            json(json!({"total_results": 26, "messages": page})),
            json(json!({"id": "10", "type": 0, "name": "general"})),
            json(json!({"messages": [[msg("400", "10", &[&cdn("400.png")])]]})),
        ]);
        let client = client(root, json!({}));
        let files = media(extract(
            &client,
            u("https://discord.com/channels/1/search?content=cat&from=7&from=8&in=10&has=image"),
        ))
        .await;
        assert_eq!(files.len(), 26);
        let k = keywords(&files[25]);
        assert_eq!(k["subcategory"], "server-search");
        assert_eq!(k["channel"], "general");
        assert_eq!(k["server"], "Guild");
        assert_eq!(k["message_id"], "400");
        let requests = server.join().unwrap();
        let lines: Vec<&str> = requests.iter().map(|r| request_line(r)).collect();
        let query = "content=cat&has=image&sort_by=timestamp&sort_order=desc&author_id=7&author_id=8&channel_id=10";
        assert_eq!(
            lines[1],
            format!("GET /api/v10/guilds/1/messages/search?{query} HTTP/1.1")
        );
        assert_eq!(lines[2], "GET /api/v10/channels/10 HTTP/1.1");
        assert_eq!(
            lines[3],
            format!("GET /api/v10/guilds/1/messages/search?{query}&max_id=476 HTTP/1.1")
        );
    }

    /// Kemono's Discord service output keeps category "kemono"; only the
    /// gallery-dl `discord` category maps to this registry site.
    #[test]
    fn kemono_discord_output_is_unaffected() {
        let hash = "ab".repeat(32);
        let fields = json!({"category": "kemono", "subcategory": "discord",
            "server_id": "100", "channel_id": "200", "id": "300", "hash": hash,
            "server": "S", "channel": "C", "num": 1});
        let raw = json!([[
            3,
            format!("https://kemono.cr/data/ab/ab/{hash}.png"),
            fields
        ]]);
        let media = crate::extract::parse_messages(&serde_json::to_vec(&raw).unwrap(), "kemono")
            .unwrap()
            .remove(0);
        assert_eq!(media.category, "kemono");
        let metadata = media.metadata.as_ref().unwrap();
        assert!(metadata.discord.is_some());
        assert!(metadata.site_keywords.is_none());
        let key = media.archive_key();
        assert!(
            key.as_ref().is_none_or(|key| key.starts_with("kemono:")),
            "{key:?}"
        );
        assert_eq!(
            crate::extract::social_site(&u("https://kemono.cr/discord/server/100/200")),
            Some("kemono")
        );
        assert_eq!(
            crate::extract::social_site(&u("https://discord.com/channels/1/2")),
            Some("discord")
        );
        assert!(!crate::extract::generic_category("discord"));
        assert!(!crate::sites::is_category("kemono"));

        // Upstream discord output (compatibility runs) uses this site's routes.
        let fields = json!({"category": "discord", "subcategory": "direct-message",
            "message_id": "5", "num": 2, "channel_id": "6", "recipients": ["a"]});
        let raw = json!([[
            3,
            "https://cdn.discordapp.com/attachments/6/5/x.png",
            fields
        ]]);
        let media = crate::extract::parse_messages(&serde_json::to_vec(&raw).unwrap(), "discord")
            .unwrap()
            .remove(0);
        let metadata = media.metadata.as_ref().unwrap();
        assert!(metadata.discord.is_none());
        let keywords = metadata.site_keywords.as_ref().unwrap();
        assert_eq!(keywords.route.subcategory, "direct-message");
        assert!(media.archive_key().unwrap().starts_with("discord:v1:"));
    }
}
