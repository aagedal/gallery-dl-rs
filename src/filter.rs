//! Native metadata expressions and cooperative selection over extracted files.
use crate::{Result, error, extract::Media};
use futures_util::{StreamExt, stream::BoxStream};
use std::{collections::BTreeMap, sync::Arc};
#[path = "filter_eval.rs"]
mod eval;
#[path = "filter_parser.rs"]
mod parser;
pub(crate) use eval::float_repr;

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Value {
    None,
    Bool(bool),
    Int(i128),
    Float(f64),
    String(Arc<str>),
    List(Arc<[Value]>),
    Tuple(Arc<[Value]>),
    Map(Arc<OrderedMap>),
}
/// Literal dictionaries preserve insertion order while looking up keys exactly.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct OrderedMap {
    values: BTreeMap<String, Value>,
    order: Vec<String>,
}
impl OrderedMap {
    pub(super) fn insert(&mut self, key: String, value: Value) {
        if !self.values.contains_key(&key) {
            self.order.push(key.clone());
        }
        self.values.insert(key, value);
    }
    pub(super) fn iter(&self) -> impl Iterator<Item = (&String, &Value)> {
        self.keys().zip(self.values())
    }
    pub(super) fn keys(&self) -> impl Iterator<Item = &String> {
        self.order.iter()
    }
    pub(super) fn values(&self) -> impl Iterator<Item = &Value> {
        self.order.iter().map(|key| &self.values[key])
    }
}
impl std::ops::Deref for OrderedMap {
    type Target = BTreeMap<String, Value>;
    fn deref(&self) -> &Self::Target {
        &self.values
    }
}
impl From<BTreeMap<String, Value>> for OrderedMap {
    fn from(values: BTreeMap<String, Value>) -> Self {
        let order = values.keys().cloned().collect();
        Self { values, order }
    }
}
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Expr {
    Literal(Value),
    Name(String),
    List(Vec<Expr>),
    Tuple(Vec<Expr>),
    Map(Vec<(String, Expr)>),
    Index(Box<Expr>, Box<Expr>),
    Unary(UnaryOp, Box<Expr>),
    Binary(Box<Expr>, BinaryOp, Box<Expr>),
    Compare(Box<Expr>, Vec<(CompareOp, Expr)>),
    Call(String, Vec<Expr>),
    Method(Box<Expr>, String, Vec<Expr>),
    Conditional {
        test: Box<Expr>,
        yes: Box<Expr>,
        no: Box<Expr>,
    },
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum UnaryOp {
    Not,
    Neg,
    Pos,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum BinaryOp {
    And,
    Or,
    Add,
    Sub,
    Mul,
    Div,
    FloorDiv,
    Mod,
    Pow,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum CompareOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    In,
    NotIn,
    Is,
    IsNot,
}

#[derive(Clone, PartialEq)]
pub struct FilterExpression {
    expressions: Arc<[Expr]>,
}
impl std::fmt::Debug for FilterExpression {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FilterExpression { expression: [redacted] }")
    }
}
impl FilterExpression {
    pub fn parse(source: &str) -> Result<Self> {
        let expression = parser::parse(source)?;
        validate_tree(&expression, 0, &mut 4096)?;
        Ok(Self {
            expressions: Arc::from([expression]),
        })
    }
    pub fn parse_all(sources: &[String]) -> Result<Self> {
        if sources.len() > 32 || sources.iter().map(String::len).sum::<usize>() > 65536 {
            return Err(error("file filter exceeds supported limits"));
        }
        let mut expressions = Vec::new();
        for source in sources {
            expressions.extend(Self::parse(source)?.expressions.iter().cloned());
        }
        Ok(Self {
            expressions: expressions.into(),
        })
    }
    pub fn matches(&self, media: &Media) -> Result<bool> {
        if self.expressions.is_empty() {
            return Ok(true);
        }
        let context = context(media);
        for expression in self.expressions.iter() {
            if !eval::truthy(&eval::evaluate(expression, &context)?) {
                return Ok(false);
            }
        }
        Ok(true)
    }
    /// Evaluate post metadata before any file-specific fields are available.
    pub fn matches_post(&self, post: &crate::event::Post) -> Result<bool> {
        if self.expressions.is_empty() {
            return Ok(true);
        }
        let mut context = context_parts(&post.category, post.metadata.as_ref(), None);
        if let Some(count) = post.count {
            context.insert("count".into(), Value::Int(count as i128));
        }
        for expression in self.expressions.iter() {
            if !eval::truthy(&eval::evaluate(expression, &context)?) {
                return Ok(false);
            }
        }
        Ok(true)
    }
    /// Evaluate the allowlisted public metadata of a queued child before
    /// opening that child's extractor or validating its identity.
    pub fn matches_child(&self, child: &crate::event::ChildMetadata) -> Result<bool> {
        if self.expressions.is_empty() {
            return Ok(true);
        }
        let mut context = BTreeMap::from([
            ("category".into(), text(child.category())),
            ("subcategory".into(), text(child.subcategory())),
        ]);
        fn child_value(value: &serde_json::Value, allow_map: bool) -> Option<Value> {
            Some(match value {
                serde_json::Value::Null => Value::None,
                serde_json::Value::Bool(value) => Value::Bool(*value),
                serde_json::Value::String(value) => text(value.as_str()),
                serde_json::Value::Number(value) => {
                    if let Some(value) = value.as_i128() {
                        Value::Int(value)
                    } else {
                        Value::Float(value.as_f64()?)
                    }
                }
                // ChildMetadata has already allowlisted the public server and
                // channel keys. Recursion remains bounded to that one level.
                serde_json::Value::Object(values) if allow_map => map(values
                    .iter()
                    .filter_map(|(key, value)| {
                        child_value(value, false).map(|value| (key.clone(), value))
                    })
                    .collect()),
                _ => return None,
            })
        }
        for (key, value) in child.fields() {
            if let Some(value) = child_value(value, true) {
                context.insert(key.clone(), value);
            }
        }
        let metadata = map(context.clone());
        context.insert("child".into(), metadata.clone());
        context.insert("metadata".into(), metadata);
        for expression in self.expressions.iter() {
            if !eval::truthy(&eval::evaluate(expression, &context)?) {
                return Ok(false);
            }
        }
        Ok(true)
    }
    /// Like upstream's default expression mode, missing keys and evaluation
    /// errors reject the current file. Extraction errors still pass through.
    pub fn apply<'a>(&self, input: BoxStream<'a, Result<Media>>) -> BoxStream<'a, Result<Media>> {
        if self.expressions.is_empty() {
            return input;
        }
        let filter = self.clone();
        futures_util::stream::unfold((input, filter), |(mut input, filter)| async move {
            let mut skipped = 0u8;
            while let Some(item) = input.next().await {
                match item {
                    Err(error) => return Some((Err(error), (input, filter))),
                    Ok(media) if filter.matches(&media).unwrap_or(false) => {
                        return Some((Ok(media), (input, filter)));
                    }
                    Ok(_) => {
                        skipped = skipped.wrapping_add(1);
                        if skipped == 0 {
                            tokio::task::yield_now().await;
                        }
                    }
                }
            }
            None
        })
        .boxed()
    }
}
fn validate_tree(expr: &Expr, depth: usize, budget: &mut usize) -> Result<()> {
    if depth > 64 || *budget == 0 {
        return Err(error("file filter exceeds supported limits"));
    }
    *budget -= 1;
    let mut next = |expr: &Expr| validate_tree(expr, depth + 1, budget);
    match expr {
        Expr::Literal(_) | Expr::Name(_) => {}
        Expr::List(values) | Expr::Tuple(values) | Expr::Call(_, values) => {
            for value in values {
                next(value)?;
            }
        }
        Expr::Map(values) => {
            for (_, value) in values {
                next(value)?;
            }
        }
        Expr::Index(left, right) | Expr::Binary(left, _, right) => {
            next(left)?;
            next(right)?;
        }
        Expr::Unary(_, value) => next(value)?,
        Expr::Compare(left, comparisons) => {
            next(left)?;
            for (_, value) in comparisons {
                next(value)?;
            }
        }
        Expr::Method(value, _, args) => {
            next(value)?;
            for value in args {
                next(value)?;
            }
        }
        Expr::Conditional { test, yes, no } => {
            next(test)?;
            next(yes)?;
            next(no)?;
        }
    }
    Ok(())
}
fn text(value: impl Into<Arc<str>>) -> Value {
    Value::String(value.into())
}
fn map(values: BTreeMap<String, Value>) -> Value {
    Value::Map(Arc::new(values.into()))
}
fn identifier(value: &str) -> Value {
    value
        .parse::<i128>()
        .ok()
        .filter(|number| number.to_string() == value)
        .map(Value::Int)
        .unwrap_or_else(|| text(value))
}
pub(crate) fn context(media: &Media) -> BTreeMap<String, Value> {
    context_parts(&media.category, media.metadata.as_ref(), Some(media))
}
fn context_parts(
    category: &str,
    post: Option<&crate::extract::PostMetadata>,
    media: Option<&Media>,
) -> BTreeMap<String, Value> {
    let mut values = BTreeMap::from([("category".into(), text(category))]);
    if let Some(media) = media {
        let (stem, extension) = media
            .filename
            .rsplit_once('.')
            .unwrap_or((&media.filename, ""));
        values.extend(BTreeMap::from([
            ("filename".into(), text(stem)),
            ("name".into(), text(stem)),
            ("extension".into(), text(extension)),
            ("url".into(), text(media.url.as_str())),
            ("_url".into(), text(media.url.as_str())),
        ]));
    }
    for (name, extensions) in [
        (
            "exts_image",
            &[
                "jpg", "jpeg", "png", "gif", "bmp", "svg", "psd", "ico", "webp", "avif", "heic",
                "heif",
            ][..],
        ),
        (
            "exts_video",
            &[
                "mp4", "m4v", "mov", "webm", "mkv", "ogv", "flv", "avi", "wmv",
            ][..],
        ),
        (
            "exts_archive",
            &["zip", "rar", "7z", "tar", "gz", "bz2", "lzma", "xz"][..],
        ),
    ] {
        values.insert(
            name.into(),
            Value::List(extensions.iter().map(|value| text(*value)).collect()),
        );
    }
    if let Some(post) = post {
        let mut metadata = BTreeMap::from([
            ("post_id".into(), identifier(&post.post_id)),
            ("media_id".into(), identifier(&post.media_id)),
            ("shortcode".into(), text(post.shortcode.as_str())),
            ("index".into(), Value::Int(post.index as i128)),
            (
                "username".into(),
                post.username
                    .as_ref()
                    .map_or(Value::None, |value| text(value.clone())),
            ),
            (
                "caption".into(),
                post.caption
                    .as_ref()
                    .map_or(Value::None, |value| text(value.clone())),
            ),
            (
                "timestamp".into(),
                post.timestamp
                    .map_or(Value::None, |value| Value::Int(value.into())),
            ),
        ]);
        if let Some(rating) = &post.rating {
            metadata.insert("rating".into(), text(rating.clone()));
        }
        if let Some(ranking) = &post.ranking {
            metadata.insert(
                "ranking".into(),
                map(BTreeMap::from([
                    ("mode".into(), text(ranking.mode.as_str())),
                    ("date".into(), text(ranking.date.as_str())),
                    ("rank".into(), Value::Int(ranking.rank.into())),
                    ("type".into(), text(ranking.kind.as_str())),
                ])),
            );
        }
        if let Some(search) = &post.search {
            metadata.insert(
                "search".into(),
                map(BTreeMap::from([
                    ("word".into(), text(search.word.as_str())),
                    ("sort".into(), text(search.sort.as_str())),
                    ("target".into(), text(search.target.as_str())),
                    (
                        "date_start".into(),
                        search
                            .date_start
                            .as_ref()
                            .map_or(Value::None, |s| text(s.as_str())),
                    ),
                    (
                        "date_end".into(),
                        search
                            .date_end
                            .as_ref()
                            .map_or(Value::None, |s| text(s.as_str())),
                    ),
                ])),
            );
        }
        if let Some(series) = &post.series {
            metadata.insert(
                "series".into(),
                map(BTreeMap::from([
                    ("id".into(), identifier(&series.id)),
                    (
                        "title".into(),
                        series
                            .title
                            .as_ref()
                            .map_or(Value::None, |s| text(s.clone())),
                    ),
                    (
                        "caption".into(),
                        series
                            .caption
                            .as_ref()
                            .map_or(Value::None, |s| text(s.clone())),
                    ),
                    ("total".into(), Value::Int(series.total.into())),
                ])),
            );
        }
        if let Some(owner) = &post.user_follow {
            metadata.insert(
                "user_follow".into(),
                map(BTreeMap::from([
                    ("id".into(), identifier(&owner.id)),
                    (
                        "name".into(),
                        owner.name.as_ref().map_or(Value::None, |s| text(s.clone())),
                    ),
                    (
                        "account".into(),
                        owner
                            .account
                            .as_ref()
                            .map_or(Value::None, |s| text(s.clone())),
                    ),
                ])),
            );
        }
        if let Some(index) = post.num_series {
            metadata.insert("num_series".into(), Value::Int(index.into()));
        }
        if let Some(score) = post.score {
            metadata.insert("score".into(), Value::Int(score.into()));
        }
        if let Some(discord) = &post.discord {
            let mut fields = BTreeMap::from([
                ("server_id".into(), text(discord.server_id.as_str())),
                ("channel_id".into(), text(discord.channel_id.as_str())),
            ]);
            for (key, value) in [
                ("server", &discord.server),
                ("channel", &discord.channel),
                ("channel_topic", &discord.channel_topic),
            ] {
                if let Some(value) = value {
                    fields.insert(key.into(), text(value.clone()));
                }
            }
            if let Some(value) = discord.channel_nsfw {
                fields.insert("channel_nsfw".into(), Value::Bool(value));
            }
            if let Some(value) = discord.channel_type {
                fields.insert("channel_type".into(), Value::Int(value.into()));
            }
            if let Some(value) = &discord.parent_id {
                fields.insert("parent_id".into(), text(value.as_str()));
            }
            metadata.extend(fields.clone());
            metadata.insert("discord".into(), map(fields));
        }
        if let Some(asset) = &post.profile_asset {
            metadata.insert(
                "profile_asset".into(),
                map(BTreeMap::from([
                    ("kind".into(), text(asset.kind.as_str())),
                    ("owner_id".into(), text(asset.owner_id.as_str())),
                    ("version".into(), text(asset.version.as_str())),
                ])),
            );
        }
        if let Some(creator) = &post.creator {
            let mut fields = BTreeMap::from([
                ("id".into(), text(creator.id.as_str())),
                ("service".into(), text(creator.service.as_str())),
            ]);
            for (key, value) in [("name", &creator.name), ("public_id", &creator.public_id)] {
                if let Some(value) = value {
                    fields.insert(key.into(), text(value.clone()));
                }
            }
            for (key, value) in [("indexed", creator.indexed), ("updated", creator.updated)] {
                if let Some(value) = value {
                    fields.insert(key.into(), Value::Int(value.into()));
                }
            }
            for (key, value) in [
                ("post_count", creator.post_count),
                ("dm_count", creator.dm_count),
                ("share_count", creator.share_count),
                ("chat_count", creator.chat_count),
            ] {
                if let Some(value) = value {
                    fields.insert(key.into(), Value::Int(value.into()));
                }
            }
            if let Some(value) = &creator.relation_id {
                fields.insert("relation_id".into(), text(value.as_str()));
            }
            if let Some(value) = creator.has_chats {
                fields.insert("has_chats".into(), Value::Bool(value));
            }
            metadata.insert("creator_id".into(), text(creator.id.as_str()));
            metadata.insert(
                "creator_name".into(),
                creator
                    .name
                    .as_ref()
                    .map_or(Value::None, |name| text(name.clone())),
            );
            metadata.insert("creator".into(), map(fields.clone()));
            metadata.insert("user_profile".into(), map(fields));
        }
        if let Some(revision) = &post.revision {
            let mut fields = BTreeMap::new();
            for (key, value) in [
                ("id", revision.id.map(|value| value as i128)),
                ("index", revision.index.map(|value| value as i128)),
                ("count", revision.count.map(|value| value as i128)),
            ] {
                if let Some(value) = value {
                    fields.insert(key.into(), Value::Int(value));
                    metadata.insert(format!("revision_{key}"), Value::Int(value));
                }
            }
            if let Some(hash) = &revision.hash {
                fields.insert("hash".into(), text(hash.clone()));
                metadata.insert("revision_hash".into(), text(hash.clone()));
            }
            metadata.insert("revision".into(), map(fields));
        }
        if let Some(frames) = &post.frames {
            let frames = Value::List(
                frames
                    .iter()
                    .map(|frame| {
                        map(BTreeMap::from([
                            ("file".into(), text(frame.file.as_str())),
                            ("delay".into(), Value::Int(frame.delay.into())),
                        ]))
                    })
                    .collect(),
            );
            metadata.insert("frames".into(), frames);
        }
        for (key, value) in [
            ("retweet_id", &post.retweet_id),
            ("quoted_by_id", &post.quoted_by_id),
            ("reposted_by_id", &post.reposted_by_id),
        ] {
            if let Some(value) = value {
                metadata.insert(key.into(), identifier(value));
            }
        }
        if media.is_none() {
            metadata.remove("media_id");
            metadata.remove("index");
        }
        values.extend(metadata.clone());
        if media.is_some() {
            values.insert("num".into(), Value::Int(post.index as i128));
        }
        if category == "twitter" {
            values.insert("tweet_id".into(), identifier(&post.post_id));
            values.insert(
                "retweet_id".into(),
                post.retweet_id
                    .as_ref()
                    .map_or(Value::Int(0), |value| identifier(value)),
            );
            values.insert(
                "quote_id".into(),
                post.quoted_by_id
                    .as_ref()
                    .map_or(Value::Int(0), |value| identifier(value)),
            );
        }
        if let Some(instance) = &post.instance {
            if matches!(category, "kemono" | "coomer") {
                values.insert("service".into(), text(instance.as_str()));
                if let Some(creator) = &post.creator {
                    values.insert("user".into(), text(creator.id.as_str()));
                } else if let Some(user) = &post.username {
                    values.insert("user".into(), text(user.clone()));
                }
            } else {
                values.insert("instance".into(), text(instance.as_str()));
            }
        }
        if matches!(
            category,
            "facebook"
                | "mastodon"
                | "mastodon.social"
                | "pawoo"
                | "baraag"
                | "kemono"
                | "coomer"
                | "pixiv"
                | "pixiv-novel"
                | "deviantart"
                | "flickr"
                | "wallhaven"
                | "artstation"
                | "pinterest"
                | "newgrounds"
                | "furaffinity"
                | "weasyl"
                | "vsco"
        ) || crate::booru::Site::from_category(category).is_some()
        {
            values.insert("id".into(), identifier(&post.post_id));
        }
        if matches!(
            category,
            "imgur"
                | "bluesky"
                | "reddit"
                | "tumblr"
                | "pixeldrain"
                | "catbox"
                | "imgbox"
                | "postimg"
        ) {
            // Upstream `id` is the image ID for files and the post ID otherwise.
            let id = if media.is_some() {
                &post.media_id
            } else {
                &post.post_id
            };
            values.insert("id".into(), text(id.as_str()));
        }
        values.insert(
            "date".into(),
            post.timestamp
                .map_or(Value::None, |value| Value::Int(value.into())),
        );
        values.insert(
            "content".into(),
            post.caption
                .as_ref()
                .map_or(Value::None, |value| text(value.clone())),
        );
        if let Some(file) = &post.file
            && media.is_some()
        {
            let mut fields = BTreeMap::new();
            for (key, value) in [
                ("width", file.width),
                ("height", file.height),
                ("width_original", file.width_original),
                ("height_original", file.height_original),
                ("bitrate", file.bitrate),
                ("filesize", file.filesize),
            ] {
                if let Some(value) = value {
                    fields.insert(key.into(), Value::Int(value.into()));
                }
            }
            if let Some(value) = file.duration.filter(|value| value.is_finite()) {
                fields.insert("duration".into(), Value::Float(value));
            }
            for (key, value) in [
                ("media_type", &file.media_type),
                ("description", &file.description),
                ("source_username", &file.source_username),
            ] {
                if let Some(value) = value {
                    fields.insert(key.into(), text(value.clone()));
                }
            }
            fields.insert(
                "sensitive_flags".into(),
                Value::List(
                    file.sensitive_flags
                        .iter()
                        .map(|value| text(value.clone()))
                        .collect(),
                ),
            );
            if let Some(value) = &file.source_id {
                fields.insert("source_id".into(), identifier(value));
            }
            values.extend(fields.clone());
            if file.source_id.is_some() || file.source_username.is_some() {
                values.insert(
                    "source_user".into(),
                    map(BTreeMap::from([
                        (
                            "id".into(),
                            file.source_id
                                .as_ref()
                                .map_or(Value::None, |value| identifier(value)),
                        ),
                        (
                            "name".into(),
                            file.source_username
                                .as_ref()
                                .map_or(Value::None, |value| text(value.clone())),
                        ),
                    ])),
                );
            }
            for (alias, key) in [
                ("image_width", "width"),
                ("image_height", "height"),
                ("type", "media_type"),
            ] {
                if let Some(value) = fields.get(key) {
                    values.insert(alias.into(), value.clone());
                }
            }
            metadata.insert("file".into(), map(fields));
        }
        // Instagram's description is its post caption; X descriptions are file alt text.
        if category == "instagram" {
            values.insert(
                "description".into(),
                post.caption
                    .as_ref()
                    .map_or(Value::None, |value| text(value.clone())),
            );
            values.insert("post_shortcode".into(), text(post.shortcode.as_str()));
        }
        if let Some(community) = &post.community {
            let fields = BTreeMap::from([
                ("id".into(), identifier(&community.id)),
                (
                    "name".into(),
                    community
                        .name
                        .as_ref()
                        .map_or(Value::None, |value| text(value.clone())),
                ),
                (
                    "description".into(),
                    community
                        .description
                        .as_ref()
                        .map_or(Value::None, |value| text(value.clone())),
                ),
                (
                    "nsfw".into(),
                    community.nsfw.map_or(Value::None, Value::Bool),
                ),
                (
                    "member_count".into(),
                    community
                        .member_count
                        .map_or(Value::None, |value| Value::Int(value.into())),
                ),
            ]);
            let mut fields = fields;
            for (key, value) in [
                ("role", &community.role),
                ("admin", &community.admin),
                ("creator", &community.creator),
                ("banner", &community.banner),
            ] {
                fields.insert(
                    key.into(),
                    value
                        .as_ref()
                        .map_or(Value::None, |value| text(value.clone())),
                );
            }
            fields.insert(
                "date".into(),
                community
                    .date
                    .map_or(Value::None, |value| Value::Int(value.into())),
            );
            fields.insert(
                "rules".into(),
                Value::List(
                    community
                        .rules
                        .iter()
                        .map(|value| text(value.clone()))
                        .collect(),
                ),
            );
            metadata.insert("community".into(), map(fields.clone()));
            values.insert("community".into(), map(fields));
        }
        if let Some(article) = &post.article {
            let fields = BTreeMap::from([
                (
                    "id".into(),
                    article
                        .id
                        .as_ref()
                        .map_or(Value::None, |value| identifier(value)),
                ),
                (
                    "title".into(),
                    article
                        .title
                        .as_ref()
                        .map_or(Value::None, |value| text(value.clone())),
                ),
                (
                    "date".into(),
                    article
                        .date
                        .map_or(Value::None, |value| Value::Int(value.into())),
                ),
            ]);
            let mut fields = fields;
            fields.insert(
                "date_updated".into(),
                article
                    .date_updated
                    .map_or(Value::None, |value| Value::Int(value.into())),
            );
            fields.insert(
                "html".into(),
                article
                    .html
                    .as_ref()
                    .map_or(Value::None, |value| text(value.clone())),
            );
            metadata.insert("article".into(), map(fields.clone()));
            values.insert("article".into(), map(fields));
        }
        values.insert("metadata".into(), map(metadata));
    }
    values
}
