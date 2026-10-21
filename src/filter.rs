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
    SourceDate(Option<i64>),
    SourcePrimitive(Arc<Value>),
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
    pub(crate) fn matches_keywords(
        &self,
        keywords: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<bool> {
        let context = keywords
            .iter()
            .map(|(key, value)| (key.clone(), keyword_value(key, value)))
            .collect();
        for expression in self.expressions.iter() {
            if !eval::truthy(&eval::evaluate(expression, &context)?) {
                return Ok(false);
            }
        }
        Ok(true)
    }
    /// Evaluate the metadata available on the producer's Directory event.
    pub fn matches_post(&self, post: &crate::event::Post) -> Result<bool> {
        if self.expressions.is_empty() {
            return Ok(true);
        }
        let mut context = context_parts(&post.category, post.metadata.as_ref(), None);
        if let crate::event::KeywordPolicy::Shared { post_fields, .. } = &post.keywords {
            context.extend(
                post_fields
                    .iter()
                    .map(|(key, value)| (key.clone(), json_value(value))),
            );
        }
        if let Some(count) = post.count {
            context.insert("count".into(), Value::Int(count as i128));
        }
        if let Some(inheritance) = post
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.booru_artist_parent.as_ref())
        {
            context.extend(
                inheritance
                    .keywords()
                    .into_iter()
                    .map(|(key, value)| (key, artist_value(&value))),
            );
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
            if child.category() == "pixiv-novel" || child.booru_artist_url().is_some() {
                context.insert(key.clone(), json_value(value));
                continue;
            }
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
// Structured public metadata is validated before reaching this context.
pub(crate) fn json_value(value: &serde_json::Value) -> Value {
    if let Some(value) = source_primitive_from_json(value) {
        return Value::SourcePrimitive(Arc::new(json_value(value)));
    }
    if let Some(value) = source_date_from_json(value) {
        return Value::SourceDate(value);
    }
    match value {
        serde_json::Value::Null => Value::None,
        serde_json::Value::Bool(value) => Value::Bool(*value),
        serde_json::Value::String(value) => text(value.as_str()),
        serde_json::Value::Number(value) => value
            .as_i128()
            .map(Value::Int)
            .or_else(|| value.as_f64().filter(|n| n.is_finite()).map(Value::Float))
            .unwrap_or(Value::None),
        serde_json::Value::Array(values) => Value::List(values.iter().map(json_value).collect()),
        serde_json::Value::Object(values) => {
            let mut result = OrderedMap::default();
            for (key, value) in values {
                result.insert(key.clone(), json_value(value));
            }
            Value::Map(Arc::new(result))
        }
    }
}
fn artist_value(value: &serde_json::Value) -> Value {
    match json_value(value) {
        value @ (Value::SourcePrimitive(_) | Value::SourceDate(_)) => value,
        value => Value::SourcePrimitive(Arc::new(value)),
    }
}
const SOURCE_PRIMITIVE_KEY: &str = "$gallery_dl_source_primitive";
pub(crate) fn source_primitive_json(value: serde_json::Value) -> serde_json::Value {
    if source_date_from_json(&value).is_some() || source_primitive_from_json(&value).is_some() {
        return value;
    }
    serde_json::Value::Object(serde_json::Map::from_iter([(
        SOURCE_PRIMITIVE_KEY.into(),
        value,
    )]))
}
pub(crate) fn source_primitive_from_json(value: &serde_json::Value) -> Option<&serde_json::Value> {
    let map = value.as_object()?;
    if map.len() == 1 {
        map.get(SOURCE_PRIMITIVE_KEY)
    } else {
        None
    }
}
const SOURCE_DATE_KEY: &str = "$gallery_dl_source_date";
pub(crate) fn source_date_json(value: Option<i64>) -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::from_iter([(
        SOURCE_DATE_KEY.into(),
        value.map_or(serde_json::Value::Null, serde_json::Value::from),
    )]))
}
pub(crate) fn source_date_from_json(value: &serde_json::Value) -> Option<Option<i64>> {
    let map = value.as_object()?;
    if map.len() != 1 {
        return None;
    }
    match map.get(SOURCE_DATE_KEY)? {
        serde_json::Value::Null => Some(None),
        value => value
            .as_i64()
            .filter(|seconds| {
                time::OffsetDateTime::from_unix_timestamp(*seconds)
                    .is_ok_and(|date| (1..=9999).contains(&date.year()))
            })
            .map(Some),
    }
}
/// Producer snapshots are authoritative datetime strings. Runtime hook strings
/// remain strings; typed datetime provenance travels in an internal wrapper.
pub(crate) fn source_date_value(value: &serde_json::Value) -> Value {
    if let serde_json::Value::String(value) = value {
        if value == "[Invalid DateTime]" {
            return Value::SourceDate(None);
        }
        if let Ok(date) = crate::postprocess::template::parse_date(value)
            && (1..=9999).contains(&date.year())
        {
            return Value::SourceDate(Some(date.unix_timestamp()));
        }
    }
    json_value(value)
}
pub(crate) fn keyword_value(_key: &str, value: &serde_json::Value) -> Value {
    json_value(value)
}
pub(crate) fn source_date_text(value: Option<i64>) -> String {
    value
        .and_then(|seconds| time::OffsetDateTime::from_unix_timestamp(seconds).ok())
        .map(|date| {
            format!(
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                date.year(),
                u8::from(date.month()),
                date.day(),
                date.hour(),
                date.minute(),
                date.second()
            )
        })
        .unwrap_or_else(|| "[Invalid DateTime]".into())
}
fn identifier(value: &str) -> Value {
    value
        .parse::<i128>()
        .ok()
        .filter(|number| number.to_string() == value)
        .map(Value::Int)
        .unwrap_or_else(|| text(value))
}
/// Project the Directory's source `date` keyword for the date predicate.
/// Native canonical integer seconds represent a datetime (including epoch zero).
/// Primitive provenance prevents inherited numbers/strings from becoming dates.
/// This follows matches_post's override order without constructing its full context.
pub(crate) fn source_selection_date(post: &crate::event::Post) -> Result<Option<i64>> {
    fn invalid() -> crate::Error {
        error("date predicate requires a datetime keyword")
    }
    fn object(fields: &serde_json::Map<String, serde_json::Value>) -> Result<Option<i64>> {
        if fields.len() == 1 {
            if let Some(value) = fields.get(SOURCE_PRIMITIVE_KEY) {
                return json(value, false);
            }
            if let Some(value) = fields.get(SOURCE_DATE_KEY) {
                if value.is_null() {
                    return Ok(None);
                }
                return value
                    .as_i64()
                    .filter(|seconds| {
                        time::OffsetDateTime::from_unix_timestamp(*seconds)
                            .is_ok_and(|date| (1..=9999).contains(&date.year()))
                    })
                    .map(Some)
                    .ok_or_else(invalid);
            }
        }
        if fields.is_empty() {
            Ok(None)
        } else {
            Err(invalid())
        }
    }
    fn json(value: &serde_json::Value, native_integer: bool) -> Result<Option<i64>> {
        match value {
            serde_json::Value::Null | serde_json::Value::Bool(false) => Ok(None),
            serde_json::Value::String(value) if value.is_empty() => Ok(None),
            serde_json::Value::Array(value) if value.is_empty() => Ok(None),
            serde_json::Value::Object(fields) => object(fields),
            serde_json::Value::Number(value) if native_integer && value.as_i128().is_some() => {
                value.as_i64().map(Some).ok_or_else(invalid)
            }
            serde_json::Value::Number(value) if value.as_f64() == Some(0.0) => Ok(None),
            _ => Err(invalid()),
        }
    }
    if let Some(inheritance) = post
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.booru_artist_parent.as_ref())
    {
        if inheritance.key() == Some("date") {
            return if inheritance.object().is_empty() {
                Ok(None)
            } else {
                Err(invalid())
            };
        }
        if inheritance.is_flat() {
            let inherited = inheritance
                .object()
                .read_keyword("date", |value| match value {
                    Some(crate::event::KeywordValue::Json(value)) => Some(json(value, false)),
                    Some(crate::event::KeywordValue::Object(fields)) => Some(object(fields)),
                    None => None,
                });
            if let Some(value) = inherited {
                return value;
            }
        }
    }
    if let crate::event::KeywordPolicy::Shared { post_fields, .. } = &post.keywords
        && let Some(value) = post_fields.get("date")
    {
        return json(value, true);
    }
    if let Some(popular) = post
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.booru_popular.as_ref())
    {
        return popular
            .get("date")
            .map_or(Ok(None), |value| json(value, false));
    }
    Ok(post
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.timestamp))
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
        if let Some(sketch) = &post.sketch {
            let mut fields = OrderedMap::default();
            for (key, value) in sketch.fields() {
                let value = json_value(value);
                fields.insert(key.clone(), value.clone());
                if !matches!(
                    key.as_str(),
                    "category"
                        | "subcategory"
                        | "post_id"
                        | "media_id"
                        | "index"
                        | "username"
                        | "caption"
                        | "timestamp"
                        | "filename"
                        | "name"
                        | "extension"
                        | "url"
                        | "_url"
                        | "id"
                        | "date"
                ) {
                    metadata.insert(key.clone(), value);
                }
            }
            metadata.insert("sketch".into(), Value::Map(Arc::new(fields)));
            metadata.insert("subcategory".into(), text("sketch"));
            metadata.insert(
                "date".into(),
                post.timestamp
                    .map_or(Value::None, |value| Value::Int(value.into())),
            );
        }
        if let Some(tags) = &post.tags {
            metadata.insert(
                "tags".into(),
                Value::List(tags.values().iter().map(json_value).collect()),
            );
        }
        if let Some(detail) = &post.pixiv_user_detail {
            for (key, value) in detail.fields() {
                metadata.insert(key.clone(), json_value(value));
            }
        }
        for (value, keys) in [
            (post.ajax, ["ajax", "_ajax"]),
            (post.mypixiv, ["mypixiv", "_mypixiv"]),
        ] {
            if let Some(value) = value {
                for key in keys {
                    metadata.insert(key.into(), Value::Bool(value));
                }
            }
        }
        if let Some(detail) = &post.pixiv_novel_metadata {
            for (key, value) in detail.fields() {
                metadata.insert(key.clone(), json_value(value));
            }
        }
        if let Some(caption) = &post.caption_html {
            metadata.insert("caption_html".into(), text(caption.as_ref()));
        }
        if let Some(comments) = &post.comments {
            metadata.insert(
                "comments".into(),
                Value::List(comments.iter().map(json_value).collect()),
            );
        }
        if let Some(tags) = &post.tags_bookmark {
            metadata.insert(
                "tags_bookmark".into(),
                Value::List(tags.iter().map(|tag| text(tag.as_str())).collect()),
            );
        }
        if let Some(original) = post.ugoira_original {
            for key in ["ugoira_original", "_ugoira_original"] {
                metadata.insert(key.into(), Value::Bool(original));
            }
        }
        if let Some(index) = post.ugoira_frame_index
            && media.is_some()
        {
            for key in ["ugoira_frame_index", "_ugoira_frame_index"] {
                metadata.insert(key.into(), Value::Int(index as i128));
            }
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
        if let Some(owner) = &post.user_bookmark {
            metadata.insert(
                "user_bookmark".into(),
                map(BTreeMap::from([
                    ("id".into(), identifier(&owner.id)),
                    (
                        "name".into(),
                        owner
                            .name
                            .as_ref()
                            .map_or(Value::None, |name| text(name.clone())),
                    ),
                    (
                        "account".into(),
                        owner
                            .account
                            .as_ref()
                            .map_or(Value::None, |account| text(account.clone())),
                    ),
                ])),
            );
        }
        if let Some(article) = &post.pixivision {
            let fields = BTreeMap::from([
                ("id".into(), identifier(&article.id)),
                ("title".into(), text(article.title.clone())),
            ]);
            metadata.insert("pixivision_id".into(), identifier(&article.id));
            metadata.insert("pixivision_title".into(), text(article.title.clone()));
            metadata.insert("pixivision".into(), map(fields));
        }
        if let Some(index) = post.num_series {
            metadata.insert("num_series".into(), Value::Int(index.into()));
        }
        if let Some(group) = &post.favgroup {
            metadata.insert("favgroup".into(), json_value(group));
        }
        if let Some(pool) = &post.pool {
            metadata.insert("pool".into(), json_value(pool));
        }
        if let Some(tags) = &post.search_tags {
            metadata.insert("search_tags".into(), text(tags.clone()));
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
            metadata.insert("_ugoira_frame_data".into(), frames.clone());
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
        if let Some(num) = &post.source_num {
            values.insert("num".into(), json_value(num));
        } else if media.is_some()
            && post.booru_popular.is_none()
            && post.booru_artist_parent.is_none()
            && !metadata.contains_key("num")
            && !post
                .source_defaults
                .as_ref()
                .is_some_and(|defaults| matches!(defaults.category, "gelbooru" | "safebooru"))
        {
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
        if post.sketch_archive || post.sketch.is_some() {
            if media.is_some() {
                let id = identifier(&post.media_id);
                values.insert("id".into(), id.clone());
                metadata.insert("id".into(), id);
            } else {
                values.remove("id");
                metadata.remove("id");
            }
        }
        if let Some(fields) = &post.booru_keywords {
            for key in ["filename", "extension", "md5"] {
                values.remove(key);
                metadata.remove(key);
                if let Some(value) = fields.get(key) {
                    let value = json_value(value);
                    values.insert(key.into(), value.clone());
                    metadata.insert(key.into(), value);
                }
            }
            for key in ["id", "tags", "date_favorited"] {
                if let Some(value) = fields.get(key) {
                    let value = if key == "date_favorited" {
                        source_date_value(value)
                    } else {
                        json_value(value)
                    };
                    values.insert(key.into(), value.clone());
                    metadata.insert(key.into(), value);
                }
            }
        }
        if post.booru_artist_parent.is_some() {
            values.insert("subcategory".into(), text("tag"));
            metadata.insert("subcategory".into(), text("tag"));
            values.remove("name");
            metadata.remove("name");
            if let Some(name) = post.booru_keywords.as_ref().and_then(|v| v.get("name")) {
                values.insert("name".into(), json_value(name));
                metadata.insert("name".into(), json_value(name));
            }
        }
        if let Some(fields) = &post.booru_popular {
            values.remove("name");
            if let Some(name) = post
                .booru_keywords
                .as_ref()
                .and_then(|fields| fields.get("name"))
            {
                values.insert("name".into(), json_value(name));
            }
            values.insert("subcategory".into(), text("popular"));
            metadata.insert("subcategory".into(), text("popular"));
            for key in ["date", "scale"] {
                values.remove(key);
                metadata.remove(key);
                if let Some(value) = fields.get(key) {
                    let value = Value::SourcePrimitive(Arc::new(json_value(value)));
                    values.insert(key.into(), value.clone());
                    metadata.insert(key.into(), value);
                }
            }
        }
        if let Some(defaults) = &post.source_defaults {
            values.insert("category".into(), Value::String(defaults.category.into()));
        }
        if let Some(user_id) = &post.booru_user_id {
            let user_id = json_value(user_id);
            values.insert("user_id".into(), user_id.clone());
            metadata.insert("user_id".into(), user_id);
        }
        if let Some(name) = &post.booru_pool_name {
            let name = json_value(name);
            values.insert("pool_name".into(), name.clone());
            metadata.insert("pool_name".into(), name);
        }
        if let Some(owner) = &post.booru_favorite_id {
            let owner = json_value(owner);
            values.insert("favorite_id".into(), owner.clone());
            metadata.insert("favorite_id".into(), owner);
        }
        if let Some(inheritance) = &post.booru_artist_parent {
            for (key, value) in inheritance.keywords() {
                let value = artist_value(&value);
                values.insert(key.clone(), value.clone());
                metadata.insert(key, value);
            }
        }
        if post.booru_artist_parent.is_none() {
            values.insert("metadata".into(), map(metadata));
        } else {
            values.remove("metadata");
            if let Some(value) = post
                .booru_keywords
                .as_ref()
                .and_then(|fields| fields.get("metadata"))
            {
                values.insert("metadata".into(), json_value(value));
            }
        }
        if let Some(value) = post
            .booru_artist_parent
            .as_ref()
            .and_then(|inheritance| inheritance.keywords().remove("metadata"))
        {
            values.insert("metadata".into(), artist_value(&value));
        }
    }
    values
}

#[cfg(test)]
mod source_date_tests {
    use super::*;
    use serde_json::json;

    fn media(value: serde_json::Value) -> Media {
        let mut media = crate::booru::parse_post(
            crate::booru::Site::Gelbooru,
            &json!({"id":42,"file_url":"https://img3.gelbooru.com/images/raw.jpg","md5":"0123456789abcdef0123456789abcdef"}),
        ).unwrap();
        Arc::make_mut(
            media
                .metadata
                .as_mut()
                .unwrap()
                .booru_keywords
                .as_mut()
                .unwrap(),
        )
        .insert("date_favorited".into(), value);
        media
    }
    #[test]
    fn source_dates_preserve_epoch_truthiness_invalid_sentinel_and_live_primitives() {
        for (value, truthy, year, text) in [
            (
                json!("1970-01-01 00:00:00"),
                true,
                "1970",
                "1970-01-01 00:00:00",
            ),
            (
                json!("1969-12-31 23:59:59"),
                true,
                "1969",
                "1969-12-31 23:59:59",
            ),
            (
                json!("[Invalid DateTime]"),
                false,
                "[Invalid DateTime]",
                "[Invalid DateTime]",
            ),
        ] {
            let media = media(value);
            let predicate = FilterExpression::parse("bool(date_favorited)").unwrap();
            assert_eq!(predicate.matches(&media).unwrap(), truthy);
            let keywords = crate::postprocess::context(&media);
            assert_eq!(
                crate::postprocess::public_keywords(&keywords)["date_favorited"],
                text
            );
            assert_eq!(predicate.matches_keywords(&keywords).unwrap(), truthy);
            assert_eq!(
                crate::naming::FilenameTemplate::parse("{date_favorited:%Y}")
                    .unwrap()
                    .render(&media)
                    .unwrap(),
                year
            );
            for template in [
                "{date_favorited:%Y}",
                "{date_favorited:>30}",
                "{date_favorited:NOT_A_DATE_SPEC}",
            ] {
                if !truthy {
                    assert_eq!(
                        crate::postprocess::template::Template::parse(template)
                            .unwrap()
                            .render(&keywords)
                            .unwrap(),
                        "[Invalid DateTime]"
                    );
                }
            }
            assert_eq!(
                crate::postprocess::template::Template::parse("{date_favorited:%Y}")
                    .unwrap()
                    .render(&keywords)
                    .unwrap(),
                year
            );
            assert_eq!(
                crate::postprocess::template::Template::parse("{date_favorited}")
                    .unwrap()
                    .render_value(&keywords)
                    .unwrap(),
                keywords["date_favorited"]
            );
        }
        for value in [
            json!(false),
            json!(0),
            serde_json::Value::Null,
            json!("changed"),
            json!("1970-01-01 00:00:00"),
            json!("[Invalid DateTime]"),
        ] {
            assert_eq!(keyword_value("date_favorited", &value), json_value(&value));
        }
        for value in [json!("1970-01-01 00:00:00"), json!("[Invalid DateTime]")] {
            let keywords = serde_json::Map::from_iter([("date_favorited".into(), value.clone())]);
            assert!(
                FilterExpression::parse("bool(date_favorited)")
                    .unwrap()
                    .matches_keywords(&keywords)
                    .unwrap()
            );
            assert_eq!(
                crate::postprocess::template::Template::parse("{date_favorited}")
                    .unwrap()
                    .render_value(&keywords)
                    .unwrap(),
                value
            );
            assert!(
                crate::postprocess::template::Template::parse("{date_favorited:%Y}")
                    .unwrap()
                    .render(&keywords)
                    .is_err()
            );
        }
        assert!(source_date_from_json(&source_date_json(Some(-62_135_596_801))).is_none());
        assert_eq!(
            source_date_from_json(&source_date_json(Some(-62_135_596_800))),
            Some(Some(-62_135_596_800))
        );
        // Source typing must not spread to unrelated metadata or strings.
        assert!(matches!(
            keyword_value("ordinary", &json!("[Invalid DateTime]")),
            Value::String(_)
        ));
        assert_eq!(
            keyword_value("date_favorited", &serde_json::Value::Null),
            Value::None
        );
    }
}

#[cfg(test)]
mod selection_date_tests {
    use super::*;
    use crate::{
        event::{ChildMetadata, KeywordPolicy, Post},
        extract::PostMetadata,
    };
    use serde_json::json;
    fn post() -> Post {
        Post {
            category: "danbooru".into(),
            metadata: Some(PostMetadata {
                timestamp: Some(0),
                ..Default::default()
            }),
            count: Some(1),
            keywords: Default::default(),
        }
    }
    fn shared_date(post: &mut Post, value: serde_json::Value) {
        let mut policy = KeywordPolicy::shared(&[]);
        if let KeywordPolicy::Shared { post_fields, .. } = &mut policy {
            *post_fields = Arc::new(serde_json::Map::from_iter([("date".into(), value)]));
        }
        post.keywords = policy;
    }
    fn inheritance(
        fields: serde_json::Value,
        mode: serde_json::Value,
    ) -> crate::event::ArtistInheritance {
        ChildMetadata::from_booru_artist(
            "danbooru",
            "artist",
            fields.as_object().unwrap(),
            &"https://danbooru.donmai.us/posts?tags=fixture"
                .parse()
                .unwrap(),
        )
        .unwrap()
        .with_booru_artist_inheritance(&mode)
        .unwrap()
        .booru_artist_parent()
        .unwrap()
        .clone()
    }
    #[test]
    fn source_datetime_epoch_and_falsy_primitive_values_are_distinct() {
        assert_eq!(source_selection_date(&post()).unwrap(), Some(0));
        for (value, expected) in [
            (json!(0), Some(0)),
            (source_date_json(Some(0)), Some(0)),
            (source_date_json(Some(-1)), Some(-1)),
            (source_date_json(None), None),
            (source_primitive_json(json!(0)), None),
            (source_primitive_json(json!(-0.0)), None),
            (json!(null), None),
            (json!(false), None),
            (json!(""), None),
            (json!([]), None),
            (json!({}), None),
        ] {
            let mut post = post();
            shared_date(&mut post, value);
            assert_eq!(source_selection_date(&post).unwrap(), expected);
        }
        for value in [
            json!(true),
            json!("private-unparseable-date"),
            json!("2024-01-01T00:00:00Z"),
            json!([0]),
            json!({"private":"payload"}),
            source_primitive_json(json!(1)),
            json!(1.5),
        ] {
            let mut post = post();
            shared_date(&mut post, value);
            let error = source_selection_date(&post).unwrap_err().to_string();
            assert!(!error.contains("private"));
            assert!(!error.contains("2024"));
            assert!(!error.contains("payload"));
        }
    }
    #[test]
    fn popular_shared_and_artist_overrides_follow_directory_predicate_projection_order() {
        let mut post = post();
        post.metadata.as_mut().unwrap().timestamp = Some(50);
        post.metadata.as_mut().unwrap().booru_popular = Some(Arc::new(
            json!({"date":"2024-01-01"}).as_object().unwrap().clone(),
        ));
        assert!(source_selection_date(&post).is_err());
        shared_date(&mut post, source_date_json(Some(60)));
        assert_eq!(source_selection_date(&post).unwrap(), Some(60));
        post.metadata.as_mut().unwrap().booru_artist_parent =
            Some(inheritance(json!({"id":7,"date":0}), json!(true)));
        assert_eq!(source_selection_date(&post).unwrap(), None);
        post.metadata.as_mut().unwrap().booru_artist_parent =
            Some(inheritance(json!({"id":7,"date":"string"}), json!(false)));
        assert_eq!(source_selection_date(&post).unwrap(), Some(60));
        post.metadata.as_mut().unwrap().booru_artist_parent =
            Some(inheritance(json!({"id":7}), json!(true)));
        assert_eq!(source_selection_date(&post).unwrap(), Some(60));
        post.metadata.as_mut().unwrap().booru_artist_parent =
            Some(inheritance(json!({"id":7}), json!("date")));
        assert!(source_selection_date(&post).is_err());
        post.metadata.as_mut().unwrap().booru_artist_parent = None;
        post.keywords = Default::default();
        post.metadata.as_mut().unwrap().booru_popular = Some(Arc::new(serde_json::Map::new()));
        assert_eq!(source_selection_date(&post).unwrap(), None);
    }
    #[test]
    fn inherited_date_reads_current_shared_map_identity_without_snapshotting_other_fields() {
        let inherited = inheritance(
            json!({"id":7,"date":{"member":1},"unrelated":{"rows":[1,2,3]}}),
            json!(true),
        );
        let date = inherited.object().object_at(&["date".into()]).unwrap();
        let mut post = post();
        post.metadata.as_mut().unwrap().booru_artist_parent = Some(inherited);
        assert!(source_selection_date(&post).is_err());
        date.record(&["member"], &serde_json::Map::new());
        assert_eq!(source_selection_date(&post).unwrap(), None);
        date.record(&["member"], json!({"member":2}).as_object().unwrap());
        assert!(source_selection_date(&post).is_err());
    }
}
