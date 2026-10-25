//! Explicit extractor post boundaries, kept separate from file identities.
use crate::{
    Result,
    extract::{Media, PostMetadata},
};
use futures_util::{StreamExt, stream::BoxStream};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

/// Cooperative cancellation shared by one parent/child extraction scope.
/// This handle contains no URL, credentials, or producer-owned metadata.
#[derive(Clone, Default)]
pub struct ChildControl(Arc<AtomicBool>);

impl ChildControl {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    pub fn same_scope(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl std::fmt::Debug for ChildControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ChildControl { scope: [redacted] }")
    }
}

/// Bounded public child-selection metadata, never the raw queue payload.
/// IDs intentionally retain their original scalar representation: a filter
/// may reject an invalid identity before the producer validates or uses it.
#[derive(Clone)]
pub struct ChildMetadata {
    category: String,
    subcategory: String,
    fields: serde_json::Map<String, serde_json::Value>,
    keyword_updates: Option<Arc<[KeywordUpdate]>>,
    target_category: Option<String>,
    source_archive: Option<Arc<crate::source_archive::ArchiveDescriptor>>,
    booru_artist: Option<Arc<BooruArtistChild>>,
}

#[derive(Clone)]
struct BooruArtistChild {
    url: Arc<str>,
    queue: KeywordObject,
    inheritance: ArtistInheritance,
}

impl ChildMetadata {
    pub fn from_raw(
        category: &str,
        subcategory: &str,
        data: &serde_json::Map<String, serde_json::Value>,
    ) -> Self {
        fn label(value: &str) -> String {
            if value.len() <= 64
                && value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
            {
                value.to_owned()
            } else {
                String::new()
            }
        }
        fn scalar(value: &serde_json::Value) -> Option<serde_json::Value> {
            match value {
                serde_json::Value::String(text) => {
                    // Public names/topics and invalid IDs must retain their
                    // exact meaning for filters, including URLs and newlines.
                    (text.len() <= 64 * 1024 && !text.contains('\0')).then(|| value.clone())
                }
                serde_json::Value::Number(number) => {
                    (number.as_str().len() <= 128).then(|| value.clone())
                }
                serde_json::Value::Bool(_) | serde_json::Value::Null => Some(value.clone()),
                serde_json::Value::Array(_) | serde_json::Value::Object(_) => None,
            }
        }
        let mut fields = serde_json::Map::new();
        for key in [
            "id",
            "name",
            "account",
            "service",
            "user",
            "indexed",
            "updated",
            "favorited",
            "faved_seq",
            "server_id",
            "channel_id",
            "server",
            "channel",
            "channel_topic",
            "channel_nsfw",
            "channel_type",
            "parent_id",
            "post_count",
            "did",
            "handle",
            "displayName",
            "description",
        ] {
            let Some(value) = data.get(key) else { continue };
            let value = if matches!(key, "server" | "channel") {
                if let Some(object) = value.as_object() {
                    // Only the public one-level queue shape is retained. Do
                    // not descend into arbitrary API response objects.
                    let keys: &[&str] = if key == "server" {
                        &["id", "name", "indexed", "updated"]
                    } else {
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
                        ]
                    };
                    let public: serde_json::Map<_, _> = keys
                        .iter()
                        .filter_map(|key| {
                            scalar(object.get(*key)?).map(|value| ((*key).to_owned(), value))
                        })
                        .collect();
                    (!public.is_empty()).then_some(serde_json::Value::Object(public))
                } else {
                    scalar(value)
                }
            } else {
                scalar(value)
            };
            if let Some(value) = value {
                fields.insert(key.into(), value);
            }
        }
        Self {
            booru_artist: None,
            keyword_updates: None,
            source_archive: None,
            target_category: None,
            category: label(category),
            subcategory: label(subcategory),
            fields,
        }
    }

    /// Public, bounded source novel queue fields, including uploaded-image changes.
    pub fn from_pixiv_novel(
        subcategory: &str,
        data: &serde_json::Map<String, serde_json::Value>,
    ) -> Self {
        let fields = crate::pixiv::NovelMetadata::queue_fields(data).unwrap_or_default();
        let subcategory = if subcategory.len() <= 64
            && subcategory
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        {
            subcategory
        } else {
            ""
        };
        Self {
            booru_artist: None,
            keyword_updates: None,
            source_archive: None,
            target_category: Some("pixiv".into()),
            category: "pixiv-novel".into(),
            subcategory: subcategory.to_owned(),
            fields,
        }
    }

    /// Preserve bounded complete artist API metadata for a forced tag queue.
    pub fn from_booru_artist(
        category: &str,
        subcategory: &str,
        raw: &serde_json::Map<String, serde_json::Value>,
        url: &url::Url,
    ) -> Result<Self> {
        if !crate::booru::is_danbooru_family(category)
            || !matches!(subcategory, "artist" | "artist-search")
            || url.as_str().len() > 16 * 1024
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(crate::error("invalid booru artist child metadata"));
        }
        fn scan(
            value: &serde_json::Value,
            depth: usize,
            nodes: &mut usize,
            bytes: &mut usize,
        ) -> Result<()> {
            *nodes += 1;
            if depth > 64 || *nodes > 262_144 {
                return Err(crate::error("booru artist metadata exceeds limits"));
            }
            *bytes = bytes.saturating_add(match value {
                serde_json::Value::String(s) => s.len().saturating_mul(6),
                serde_json::Value::Number(n) => n.as_str().len(),
                _ => 8,
            });
            match value {
                serde_json::Value::Array(values) => {
                    for value in values {
                        scan(value, depth + 1, nodes, bytes)?;
                    }
                }
                serde_json::Value::Object(values) => {
                    for (key, value) in values {
                        *bytes = bytes.saturating_add(key.len().saturating_mul(6));
                        scan(value, depth + 1, nodes, bytes)?;
                    }
                }
                _ => {}
            }
            if *bytes > 32 * 1024 * 1024 {
                return Err(crate::error("booru artist metadata exceeds limits"));
            }
            Ok(())
        }
        let mut nodes: usize = 0;
        let mut bytes: usize = 0;
        for (key, value) in raw {
            bytes = bytes.saturating_add(key.len().saturating_mul(6));
            scan(value, 0, &mut nodes, &mut bytes)?;
        }
        let mut fields = raw.clone();
        fields.insert("category".into(), category.into());
        fields.insert("subcategory".into(), subcategory.into());
        fields.insert("_url".into(), url.as_str().into());
        fields.insert(
            "_extractor".into(),
            if crate::booru::category_dialect(category) == Some(crate::booru::Dialect::Danbooru) {
                "DanbooruTagExtractor"
            } else {
                "E621TagExtractor"
            }
            .into(),
        );
        let queue = KeywordObject::new(fields.clone());
        let parent = ArtistInheritance::from(queue.shallow_copy());
        Ok(Self {
            category: category.into(),
            subcategory: subcategory.into(),
            fields,
            keyword_updates: None,
            target_category: Some(category.into()),
            source_archive: Some(Arc::new(crate::source_archive::ArchiveDescriptor {
                category: category.into(),
                format: "t_{search_tags}_{id}".into(),
            })),
            booru_artist: Some(Arc::new(BooruArtistChild {
                url: Arc::from(url.as_str()),
                queue,
                inheritance: parent,
            })),
        })
    }
    pub fn booru_artist_url(&self) -> Option<&str> {
        self.booru_artist.as_ref().map(|artist| artist.url.as_ref())
    }
    pub fn booru_artist_queue(&self) -> Option<&KeywordObject> {
        self.booru_artist.as_ref().map(|artist| &artist.queue)
    }
    pub fn booru_artist_parent(&self) -> Option<&ArtistInheritance> {
        self.booru_artist.as_ref().map(|artist| &artist.inheritance)
    }

    pub fn with_booru_artist_inheritance(mut self, value: &serde_json::Value) -> Result<Self> {
        let Some(artist) = self.booru_artist.as_mut() else {
            return Err(crate::error(
                "artist inheritance requires artist provenance",
            ));
        };
        Arc::make_mut(artist).inheritance.mode = match value {
            serde_json::Value::String(key) if !key.is_empty() => {
                if key.len() > 4096 {
                    return Err(crate::error("artist inheritance key exceeds limits"));
                }
                ArtistInheritanceMode::Nested(Arc::from(key.as_str()))
            }
            value if artist_truthy(value) => ArtistInheritanceMode::Flat,
            _ => ArtistInheritanceMode::Disabled,
        };
        Ok(self)
    }
    /// Explicit target extractor format; producer queue metadata may be ambiguous.
    pub fn with_source_archive(
        mut self,
        descriptor: crate::source_archive::ArchiveDescriptor,
    ) -> Self {
        self.source_archive = Some(Arc::new(descriptor));
        self
    }
    pub fn source_archive(&self) -> Option<&crate::source_archive::ArchiveDescriptor> {
        self.source_archive.as_deref()
    }

    pub fn with_keyword_updates(mut self, updates: Vec<KeywordUpdate>) -> Self {
        self.keyword_updates = Some(updates.into());
        self
    }
    pub fn keyword_updates(&self) -> &[KeywordUpdate] {
        self.keyword_updates.as_deref().unwrap_or_default()
    }
    /// Child extractor configuration category; queue keywords retain the parent category.
    pub fn target_category(&self) -> &str {
        self.target_category.as_deref().unwrap_or(&self.category)
    }

    pub fn category(&self) -> &str {
        &self.category
    }

    pub fn subcategory(&self) -> &str {
        &self.subcategory
    }

    pub fn fields(&self) -> &serde_json::Map<String, serde_json::Value> {
        &self.fields
    }
}

impl std::fmt::Debug for ChildMetadata {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ChildMetadata { fields: [redacted] }")
    }
}

#[derive(Clone, Debug)]
pub struct ChildScope {
    pub control: ChildControl,
    pub metadata: ChildMetadata,
}

impl ChildScope {
    pub fn new(metadata: ChildMetadata) -> Self {
        Self {
            control: ChildControl::default(),
            metadata,
        }
    }
}

/// Ordered writes performed by a producer between Directory/Url/Queue messages.
#[derive(Clone)]
pub enum KeywordUpdate {
    Set {
        name: String,
        value: serde_json::Value,
    },
    Remove(String),
    /// Python-style `del`: absence is a producer error.
    RemoveRequired(String),
    /// Writes after the emitted file and its hooks complete, before the next emission.
    OnResume(Vec<KeywordUpdate>),
    Increment(String),
    Format {
        name: String,
        template: String,
    },
}
impl std::fmt::Debug for KeywordUpdate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("KeywordUpdate { fields: [redacted] }")
    }
}

/// One producer keyword observed after a post's file hooks have drained.
/// Missing and unobserved are distinct so skipped posts retain their raw API
/// cursor, while an explicit hook deletion can be reported by the producer.
#[derive(Clone)]
pub struct KeywordFeedback(Arc<KeywordFeedbackState>);
struct KeywordFeedbackState {
    name: String,
    value: Mutex<Option<Option<serde_json::Value>>>,
}
impl KeywordFeedback {
    pub fn new(name: &str) -> Self {
        Self(Arc::new(KeywordFeedbackState {
            name: name.to_owned(),
            value: Mutex::new(None),
        }))
    }
    /// Consume the latest observation: None is unobserved; Some(None) means
    /// the keyword was absent, and Some(Some(Null)) is an explicit JSON null.
    pub fn take(&self) -> Option<Option<serde_json::Value>> {
        self.0
            .value
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }
    pub(crate) fn capture(&self, keywords: &serde_json::Map<String, serde_json::Value>) {
        *self.0.value.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(keywords.get(&self.0.name).cloned());
    }
}
impl std::fmt::Debug for KeywordFeedback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("KeywordFeedback { fields: [redacted] }")
    }
}

/// A producer-owned nested dictionary shared by distinct post dictionaries.
/// Root assignment detaches a post; nested edits retain this object identity.
#[derive(Clone)]
pub struct KeywordObject(Arc<Mutex<KeywordObjectState>>);
pub(crate) enum KeywordValue<'a> {
    Json(&'a serde_json::Value),
    Object(&'a serde_json::Map<String, serde_json::Value>),
}
struct KeywordObjectState {
    value: serde_json::Map<String, serde_json::Value>,
    children: std::collections::BTreeMap<String, KeywordObject>,
}
impl KeywordObject {
    pub fn new(value: serde_json::Map<String, serde_json::Value>) -> Self {
        Self(Arc::new(Mutex::new(KeywordObjectState {
            value,
            children: Default::default(),
        })))
    }
    pub(crate) fn contains_key(&self, key: &str) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .value
            .contains_key(key)
    }
    fn keys(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .value
            .keys()
            .cloned()
            .collect()
    }
    /// Read a single current root value without cloning unrelated keywords.
    /// Tracked maps retain their live identity rather than a stale JSON snapshot.
    pub(crate) fn read_keyword<R>(
        &self,
        key: &str,
        read: impl FnOnce(Option<KeywordValue<'_>>) -> R,
    ) -> R {
        let state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(child) = state.children.get(key) {
            let child = child.0.lock().unwrap_or_else(|e| e.into_inner());
            read(Some(KeywordValue::Object(&child.value)))
        } else {
            read(state.value.get(key).map(KeywordValue::Json))
        }
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .value
            .is_empty()
    }
    pub fn snapshot(&self) -> serde_json::Map<String, serde_json::Value> {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let updates = state
            .children
            .iter()
            .map(|(key, child)| (key.clone(), serde_json::Value::Object(child.snapshot())))
            .collect::<Vec<_>>();
        state.value.extend(updates);
        state.value.clone()
    }
    fn shallow_state(&self) -> KeywordObjectState {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let children = state
            .value
            .iter()
            .filter_map(|(key, value)| value.as_object().map(|value| (key.clone(), value.clone())))
            .collect::<Vec<_>>();
        for (key, value) in children {
            state
                .children
                .entry(key)
                .or_insert_with(|| Self::new(value));
        }
        KeywordObjectState {
            value: state.value.clone(),
            children: state.children.clone(),
        }
    }
    pub(crate) fn object_at(&self, path: &[String]) -> Option<Self> {
        let Some(key) = path.first() else {
            return Some(self.clone());
        };
        let child = {
            let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
            if !state.children.contains_key(key) {
                let value = state.value.get(key)?.as_object()?.clone();
                state.children.insert(key.clone(), Self::new(value));
            }
            state.children.get(key)?.clone()
        };
        child.object_at(&path[1..])
    }
    fn contains_object(&self, other: &Self, depth: usize) -> bool {
        if Arc::ptr_eq(&self.0, &other.0) || depth >= 128 {
            return true;
        }
        let children = self
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .children
            .values()
            .cloned()
            .collect::<Vec<_>>();
        children
            .iter()
            .any(|child| child.contains_object(other, depth + 1))
    }
    pub(crate) fn alias_at(&self, path: &[&str], alias: &Self) {
        if path.len() == 1 {
            // JSON-backed keyword objects cannot represent cycles safely.
            if alias.contains_object(self, 0) {
                return;
            }
            self.0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .children
                .insert(path[0].to_owned(), alias.clone());
        } else if !path.is_empty()
            && let Some(child) = self.object_at(&[path[0].to_owned()])
        {
            child.alias_at(&path[1..], alias);
        }
    }
    pub fn shallow_copy(&self) -> Self {
        Self(Arc::new(Mutex::new(self.shallow_state())))
    }
    pub(crate) fn reset_shallow_from(&self, source: &Self) {
        let state = source.shallow_state();
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = state;
    }
    pub(crate) fn sync(&self, value: &serde_json::Map<String, serde_json::Value>) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state
            .children
            .retain(|key, _| value.get(key).is_some_and(serde_json::Value::is_object));
        state.value = value.clone();
    }
    pub(crate) fn replace(&self, value: &serde_json::Map<String, serde_json::Value>) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = KeywordObjectState {
            value: value.clone(),
            children: Default::default(),
        };
    }
    pub(crate) fn record(&self, keys: &[&str], value: &serde_json::Map<String, serde_json::Value>) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(key) = keys.first() {
            if keys.len() > 1 {
                if let Some(child) = state.children.get(*key)
                    && let Some(nested) = value.get(*key).and_then(serde_json::Value::as_object)
                {
                    child.record(&keys[1..], nested);
                }
            } else {
                state.children.remove(*key);
            }
        }
        state.value = value.clone();
    }
}
impl std::fmt::Debug for KeywordObject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("KeywordObject { fields: [redacted] }")
    }
}

/// A proven artist child job and its immutable parent-metadata policy.
#[derive(Clone, Debug)]
enum ArtistInheritanceMode {
    Disabled,
    Nested(Arc<str>),
    Flat,
}
#[derive(Clone)]
pub struct ArtistInheritance {
    mode: ArtistInheritanceMode,
    data: KeywordObject,
}
impl std::fmt::Debug for ArtistInheritance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ArtistInheritance { fields: [redacted] }")
    }
}
impl From<KeywordObject> for ArtistInheritance {
    fn from(data: KeywordObject) -> Self {
        Self {
            mode: ArtistInheritanceMode::Nested(Arc::from("_parent")),
            data,
        }
    }
}
fn artist_truthy(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => false,
        serde_json::Value::Bool(value) => *value,
        serde_json::Value::Number(value) => value.as_f64().map_or_else(
            || {
                value
                    .as_str()
                    .bytes()
                    .any(|byte| byte.is_ascii_digit() && byte != b'0')
            },
            |value| value != 0.0,
        ),
        serde_json::Value::String(value) => !value.is_empty(),
        serde_json::Value::Array(value) => !value.is_empty(),
        serde_json::Value::Object(value) => !value.is_empty(),
    }
}
impl ArtistInheritance {
    pub fn key(&self) -> Option<&str> {
        match &self.mode {
            ArtistInheritanceMode::Nested(key) => Some(key),
            _ => None,
        }
    }
    pub fn is_flat(&self) -> bool {
        matches!(self.mode, ArtistInheritanceMode::Flat)
    }
    pub fn object(&self) -> &KeywordObject {
        &self.data
    }
    pub fn snapshot(&self) -> serde_json::Map<String, serde_json::Value> {
        self.data.snapshot()
    }
    pub fn has_keyword(&self, name: &str) -> bool {
        match &self.mode {
            ArtistInheritanceMode::Disabled => false,
            ArtistInheritanceMode::Nested(key) => key.as_ref() == name,
            ArtistInheritanceMode::Flat => name != "_extractor" && self.data.contains_key(name),
        }
    }
    pub(crate) fn keyword_names(&self) -> Vec<String> {
        match &self.mode {
            ArtistInheritanceMode::Disabled => vec![],
            ArtistInheritanceMode::Nested(key) => vec![key.to_string()],
            ArtistInheritanceMode::Flat => self
                .data
                .keys()
                .into_iter()
                .filter(|key| key != "_extractor")
                .collect(),
        }
    }
    pub fn keywords(&self) -> serde_json::Map<String, serde_json::Value> {
        match &self.mode {
            ArtistInheritanceMode::Disabled => serde_json::Map::new(),
            ArtistInheritanceMode::Nested(key) => serde_json::Map::from_iter([(
                key.to_string(),
                serde_json::Value::Object(self.snapshot()),
            )]),
            ArtistInheritanceMode::Flat => {
                let mut fields = self.snapshot();
                fields.remove("_extractor");
                fields
            }
        }
    }
    pub(crate) fn keyword_values(&self) -> serde_json::Map<String, serde_json::Value> {
        self.keywords()
            .into_iter()
            .map(|(key, value)| (key, crate::filter::source_primitive_json(value)))
            .collect()
    }
    pub(crate) fn bindings(&self) -> Vec<(String, KeywordObject)> {
        if let Some(key) = self.key() {
            return vec![(key.into(), self.data.clone())];
        }
        if !self.is_flat() {
            return vec![];
        }
        self.keywords()
            .into_iter()
            .filter_map(|(key, value)| {
                value.as_object()?;
                Some((key.clone(), self.data.object_at(&[key])?))
            })
            .collect()
    }
}

/// Ownership of public keyword dictionaries emitted around one post.
/// Shared producers explicitly declare the fields they assign again per file.
#[derive(Clone, Debug, Default)]
pub enum KeywordPolicy {
    #[default]
    Independent,
    CopiedAfterPost {
        file_fields: Arc<[String]>,
        removed_fields: Arc<[String]>,
    },
    Shared {
        file_fields: Arc<[String]>,
        post_fields: Arc<serde_json::Map<String, serde_json::Value>>,
        feedback: Option<KeywordFeedback>,
        objects: Arc<[(String, KeywordObject)]>,
    },
}
impl KeywordPolicy {
    pub fn copied_after_post(fields: &[&str]) -> Self {
        Self::CopiedAfterPost {
            removed_fields: Arc::new([]),
            file_fields: fields.iter().map(|field| (*field).to_owned()).collect(),
        }
    }
    /// Fields removed from the post before the producer copies it into a file.
    pub fn without_post_fields(mut self, names: &[&str]) -> Self {
        if let Self::CopiedAfterPost { removed_fields, .. } = &mut self {
            *removed_fields = names.iter().map(|name| (*name).to_owned()).collect();
        }
        self
    }
    pub fn shared(fields: &[&str]) -> Self {
        Self::Shared {
            post_fields: Arc::new(serde_json::Map::new()),
            feedback: None,
            objects: Arc::from([]),
            file_fields: fields.iter().map(|field| (*field).to_owned()).collect(),
        }
    }
    /// Observe one shared root keyword after all selected file work finishes.
    pub fn with_feedback(mut self, value: KeywordFeedback) -> Self {
        if let Self::Shared { feedback, .. } = &mut self {
            *feedback = Some(value);
        }
        self
    }
    /// Bind a root field to a producer-owned nested dictionary. Each post gets
    /// a fresh attachment, while cloned handles retain the nested identity.
    pub fn with_shared_object(mut self, name: &str, value: KeywordObject) -> Self {
        if let Self::Shared { objects, .. } = &mut self {
            let mut bindings = objects.to_vec();
            if let Some((_, object)) = bindings.iter_mut().find(|(key, _)| key == name) {
                *object = value;
            } else {
                bindings.push((name.to_owned(), value));
            }
            *objects = bindings.into();
        }
        self
    }
    /// Preserve fields the producer already supplied before the Directory event.
    pub fn with_post_fields(mut self, media: &Media, names: &[&str]) -> Self {
        if let Self::Shared { post_fields, .. } = &mut self {
            let context = crate::postprocess::context(media);
            *post_fields = Arc::new(
                names
                    .iter()
                    .filter_map(|name| {
                        context
                            .get(*name)
                            .map(|value| ((*name).to_owned(), value.clone()))
                    })
                    .collect(),
            );
        }
        self
    }
}
#[derive(Clone, Debug)]
pub struct Post {
    pub keywords: KeywordPolicy,
    pub category: String,
    pub metadata: Option<PostMetadata>,
    /// Declared file count; this is metadata, not a framing delimiter.
    pub count: Option<usize>,
}
impl Post {
    pub fn from_media(media: &[Media], category: &str) -> Self {
        let mut metadata = media.first().and_then(|media| media.metadata.clone());
        if let Some(metadata) = &mut metadata {
            metadata.file = None;
            metadata.ugoira_frame_index = None;
            metadata.media_id.clear();
            metadata.index = 0;
            metadata.source_num = metadata.source_num.as_ref().map(|_| 0.into());
            metadata.source_suffix = metadata.source_suffix.as_ref().map(|_| "".into());
        }
        Self {
            keywords: KeywordPolicy::default(),
            category: category.into(),
            metadata,
            count: Some(media.len()),
        }
    }
}
#[derive(Clone, Debug)]
pub enum ExtractionEvent {
    BeginChild(ChildScope),
    EndChild(ChildControl),
    Post(Box<Post>),
    Media(Box<Media>),
    EndPost,
}

/// Queue one independent child job without starting its extractor until the
/// consumer has seen its selection metadata. Canceling the scope drops active
/// work and closes any visible descendant scopes before closing this child.
pub fn child_job<'a>(
    metadata: ChildMetadata,
    build: impl FnOnce() -> BoxStream<'a, Result<ExtractionEvent>> + Send + 'a,
) -> BoxStream<'a, Result<ExtractionEvent>> {
    struct State<'a, F> {
        metadata: Option<ChildMetadata>,
        artist_parent: Option<ArtistInheritance>,
        control: ChildControl,
        build: Option<F>,
        input: Option<BoxStream<'a, Result<ExtractionEvent>>>,
        descendants: Vec<ChildControl>,
        ended: bool,
    }
    impl<F> Drop for State<'_, F> {
        fn drop(&mut self) {
            if !self.ended {
                self.control.cancel();
            }
            for control in &self.descendants {
                control.cancel();
            }
        }
    }
    let artist_parent = metadata.booru_artist_parent().cloned();
    let state = State {
        artist_parent,
        metadata: Some(metadata),
        control: ChildControl::default(),
        build: Some(build),
        input: None,
        descendants: Vec::new(),
        ended: false,
    };
    futures_util::stream::unfold(state, |mut state| async move {
        if let Some(metadata) = state.metadata.take() {
            let scope = ChildScope {
                control: state.control.clone(),
                metadata,
            };
            return Some((Ok(ExtractionEvent::BeginChild(scope)), state));
        }
        loop {
            if state.ended {
                return None;
            }
            if state.control.is_cancelled() {
                state.input = None;
                state.build = None;
                for control in &state.descendants {
                    control.cancel();
                }
                if let Some(control) = state.descendants.pop() {
                    return Some((Ok(ExtractionEvent::EndChild(control)), state));
                }
                state.ended = true;
                return Some((Ok(ExtractionEvent::EndChild(state.control.clone())), state));
            }
            if let Some(build) = state.build.take() {
                state.input = Some(build());
            }
            let event = state.input.as_mut().expect("active child job").next().await;
            if state.control.is_cancelled() {
                // A cancellation can arrive while the producer was pending.
                // A newly returned scope has not been shown to the consumer.
                if let Some(Ok(ExtractionEvent::BeginChild(scope))) = event {
                    scope.control.cancel();
                }
                continue;
            }
            match event {
                Some(mut event) => {
                    if state.descendants.is_empty()
                        && let Some(parent) = &state.artist_parent
                    {
                        match &mut event {
                            Ok(ExtractionEvent::Post(post)) => {
                                post.metadata
                                    .get_or_insert_with(Default::default)
                                    .booru_artist_parent = Some(parent.clone());
                                if let KeywordPolicy::Shared {
                                    file_fields,
                                    post_fields,
                                    ..
                                } = &mut post.keywords
                                {
                                    let mut fields = file_fields.to_vec();
                                    fields.retain(|key| key != "num" && key != "name");
                                    for key in parent.keywords().keys() {
                                        if !fields.contains(key) {
                                            fields.push(key.clone());
                                        }
                                    }
                                    *file_fields = fields.into();
                                    let mut fields = post_fields.as_ref().clone();
                                    if !post
                                        .metadata
                                        .as_ref()
                                        .and_then(|m| m.booru_keywords.as_ref())
                                        .is_some_and(|v| v.contains_key("name"))
                                    {
                                        fields.remove("name");
                                    }
                                    fields.extend(parent.keyword_values());
                                    *post_fields = Arc::new(fields);
                                }
                                for (key, object) in parent.bindings() {
                                    post.keywords =
                                        post.keywords.clone().with_shared_object(&key, object);
                                }
                            }
                            Ok(ExtractionEvent::Media(media)) => {
                                media
                                    .metadata
                                    .get_or_insert_with(Default::default)
                                    .booru_artist_parent = Some(parent.clone());
                            }
                            _ => {}
                        }
                    }
                    match &event {
                        Ok(ExtractionEvent::BeginChild(scope)) => {
                            state.descendants.push(scope.control.clone());
                        }
                        Ok(ExtractionEvent::EndChild(control))
                            if state
                                .descendants
                                .last()
                                .is_some_and(|current| current.same_scope(control)) =>
                        {
                            state.descendants.pop();
                        }
                        _ => {}
                    }
                    return Some((event, state));
                }
                None => {
                    state.input = None;
                    state.ended = true;
                    return Some((Ok(ExtractionEvent::EndChild(state.control.clone())), state));
                }
            }
        }
    })
    .boxed()
}
/// Dispatcher for child URLs outside the producing extractor (upstream
/// `Message.Queue` to another site). `None` means no extractor accepts it.
pub type Queue<'a> = std::sync::Arc<
    dyn Fn(url::Url) -> Option<BoxStream<'a, Result<ExtractionEvent>>> + Send + Sync + 'a,
>;

pub fn group(media: Vec<Media>, category: &str) -> VecDeque<ExtractionEvent> {
    group_with_keywords(media, category, KeywordPolicy::default())
}
pub fn group_with_keywords(
    media: Vec<Media>,
    category: &str,
    keywords: KeywordPolicy,
) -> VecDeque<ExtractionEvent> {
    let mut events = VecDeque::with_capacity(media.len() + 2);
    let mut post = Post::from_media(&media, category);
    post.keywords = keywords;
    events.push_back(ExtractionEvent::Post(Box::new(post)));
    events.extend(
        media
            .into_iter()
            .map(|media| ExtractionEvent::Media(Box::new(media))),
    );
    events.push_back(ExtractionEvent::EndPost);
    events
}
/// Preserve the original file-only API without guessing boundaries from IDs.
pub fn media_only<'a>(
    input: BoxStream<'a, Result<ExtractionEvent>>,
) -> BoxStream<'a, Result<Media>> {
    futures_util::stream::unfold(input, |mut input| async move {
        let mut skipped = 0u8;
        while let Some(event) = input.next().await {
            match event {
                Ok(ExtractionEvent::Media(media)) => return Some((Ok(*media), input)),
                Err(error) => return Some((Err(error), input)),
                _ => {
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

/// Bound active extraction across every event, page, retry, and child lookup.
/// Time between consumer polls is excluded; exhaustion drops the input before
/// yielding the terminal error, releasing active requests and owned children.
pub fn with_extraction_budget<'a, T: Send + 'a>(
    input: BoxStream<'a, Result<T>>,
    budget: std::time::Duration,
) -> BoxStream<'a, Result<T>> {
    futures_util::stream::unfold(Some((input, budget)), |state| async move {
        let (mut input, remaining) = state?;
        if remaining.is_zero() {
            return Some((Err(crate::error("extraction timed out")), None));
        }
        let started = tokio::time::Instant::now();
        let next = tokio::time::timeout(remaining, input.next()).await;
        let elapsed = started.elapsed();
        match next {
            Ok(item) if elapsed <= remaining => {
                item.map(|item| (item, Some((input, remaining.saturating_sub(elapsed)))))
            }
            _ => Some((Err(crate::error("extraction timed out")), None)),
        }
    })
    .boxed()
}

#[cfg(test)]
mod budget_tests {
    use super::*;
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::Duration,
    };

    struct Dropped(Arc<AtomicBool>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn one_budget_spans_items_but_excludes_consumer_pauses() {
        let dropped = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let input =
            futures_util::stream::unfold((0, Dropped(dropped.clone())), move |(n, guard)| {
                let seen = seen.clone();
                async move {
                    seen.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_secs(6)).await;
                    Some((Ok(n), (n + 1, guard)))
                }
            })
            .boxed();
        let mut input = with_extraction_budget(input, Duration::from_secs(11));
        assert_eq!(input.next().await.unwrap().unwrap(), 0);
        tokio::time::sleep(Duration::from_secs(100)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(
            input
                .next()
                .await
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("timed out")
        );
        assert!(dropped.load(Ordering::SeqCst));
        assert!(input.next().await.is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn rejected_events_and_recoverable_errors_do_not_reset_the_budget() {
        let input =
            futures_util::stream::iter([Ok(1), Err(crate::error("recoverable")), Ok(3), Ok(4)])
                .then(|item| async move {
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    item
                })
                .boxed();
        let input = with_extraction_budget(input, Duration::from_secs(10));
        let events = input
            .filter(|item| std::future::ready(!matches!(item, Ok(1))))
            .collect::<Vec<_>>()
            .await;
        assert_eq!(events.len(), 3);
        assert!(
            events[0]
                .as_ref()
                .unwrap_err()
                .to_string()
                .contains("recoverable")
        );
        assert_eq!(*events[1].as_ref().unwrap(), 3);
        assert!(
            events[2]
                .as_ref()
                .unwrap_err()
                .to_string()
                .contains("timed out")
        );
    }

    #[tokio::test]
    async fn zero_budget_and_consumer_drop_do_not_start_later_work() {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let input = futures_util::stream::once(async move {
            seen.fetch_add(1, Ordering::SeqCst);
            Ok(1)
        })
        .boxed();
        let mut input = with_extraction_budget(input, Duration::ZERO);
        assert!(input.next().await.unwrap().is_err());
        assert!(input.next().await.is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let dropped = Arc::new(AtomicBool::new(false));
        let input = futures_util::stream::unfold(Dropped(dropped.clone()), |guard| async {
            Some((Ok(1), guard))
        })
        .boxed();
        let mut input = with_extraction_budget(input, Duration::from_secs(1));
        assert_eq!(input.next().await.unwrap().unwrap(), 1);
        drop(input);
        assert!(dropped.load(Ordering::SeqCst));
    }
}

#[cfg(test)]
mod artist_metadata_tests {
    use super::*;
    #[test]
    fn cloned_artist_metadata_keeps_independent_policy_and_shared_keyword_objects() {
        let metadata = ChildMetadata::from_booru_artist(
            "danbooru",
            "artist",
            serde_json::json!({"id":7,"name":"fixture"})
                .as_object()
                .unwrap(),
            &"https://danbooru.donmai.us/posts?tags=fixture"
                .parse()
                .unwrap(),
        )
        .unwrap();
        let cloned = metadata
            .clone()
            .with_booru_artist_inheritance(&serde_json::json!(true))
            .unwrap();
        assert_eq!(
            metadata.booru_artist_parent().unwrap().key(),
            Some("_parent")
        );
        assert!(cloned.booru_artist_parent().unwrap().is_flat());
        let queue = metadata.booru_artist_queue().unwrap();
        let mut changed = queue.snapshot();
        changed.insert("name".into(), "updated".into());
        queue.record(&["name"], &changed);
        assert_eq!(
            cloned.booru_artist_queue().unwrap().snapshot()["name"],
            "updated"
        );
        assert_eq!(metadata.booru_artist_url(), cloned.booru_artist_url());
    }
}
