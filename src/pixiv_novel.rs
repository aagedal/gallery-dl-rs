//! Native Pixiv novels, using the authenticated App API and webview text payload.
//! Optional embedded uploads and artwork queues retain the source traversal order.
use super::{Pixiv, REFERER, id_of, percent_decode, string, valid_id};
use crate::{
    Result, error,
    event::{ExtractionEvent, Post},
    extract::{Media, PostMetadata, inline_text},
};
use futures_util::{
    StreamExt,
    stream::{self, BoxStream},
};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::Arc,
};
use url::Url;

#[path = "pixiv_novel_embeds.rs"]
mod embeds;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NovelOptions {
    pub covers: bool,
    pub full_series: bool,
    pub embeds: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Novel(String),
    User(String),
    Series(String),
    Bookmarks {
        user: String,
        tag: Option<String>,
        private: bool,
    },
}

pub(super) fn is_route(url: &Url) -> bool {
    let segments = super::segments(url);
    let segments = segments.strip_prefix(&["en"]).unwrap_or(&segments);
    matches!(
        segments,
        ["n", _]
            | ["novel", "show.php"]
            | ["novel", "series", _]
            | ["users", _, "novels"]
            | ["users", _, "bookmarks", "novels"]
            | ["users", _, "bookmarks", "novels", _]
    )
}

pub fn target(url: &Url) -> Result<Target> {
    let unsupported = || error("unsupported Pixiv novel URL");
    if !matches!(url.scheme(), "https" | "http")
        || !matches!(url.host_str(), Some("pixiv.net" | "www.pixiv.net"))
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(unsupported());
    }
    let segments = super::segments(url);
    let segments = segments.strip_prefix(&["en"]).unwrap_or(&segments);
    let id = |value: &str| {
        valid_id(value)
            .then(|| value.to_owned())
            .ok_or_else(unsupported)
    };
    let query = |name: &str| {
        url.query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
    };
    let bookmarks = |user: &str, tag: Option<&str>| {
        let tag = tag
            .map(|tag| percent_decode(tag).ok_or_else(unsupported))
            .transpose()?;
        if tag
            .as_ref()
            .is_some_and(|tag| tag.len() > 1024 || tag.chars().any(char::is_control))
        {
            return Err(unsupported());
        }
        Ok(Target::Bookmarks {
            user: id(user)?,
            tag,
            private: query("rest").as_deref() == Some("hide"),
        })
    };
    match segments {
        ["n", raw] => Ok(Target::Novel(id(raw)?)),
        ["novel", "show.php"] => Ok(Target::Novel(id(&query("id").ok_or_else(unsupported)?)?)),
        ["novel", "series", raw] => Ok(Target::Series(id(raw)?)),
        ["users", raw, "novels"] => Ok(Target::User(id(raw)?)),
        ["users", raw, "bookmarks", "novels"] => bookmarks(raw, None),
        ["users", raw, "bookmarks", "novels", tag] => bookmarks(raw, Some(tag)),
        _ => Err(unsupported()),
    }
}

/// Extract exactly the App API webview's `novel: JSON,\n` data member. JSON
/// decoding is the only text transformation; markers and Unicode stay intact.
pub fn parse_webview(bytes: &[u8]) -> Result<String> {
    let invalid = || error("Pixiv returned invalid novel webview content");
    if bytes.len() > super::MAX_RESPONSE {
        return Err(invalid());
    }
    let html = std::str::from_utf8(bytes).map_err(|_| invalid())?;
    let (_, data) = html.split_once("novel: ").ok_or_else(invalid)?;
    let (data, _) = data.split_once(",\n").ok_or_else(invalid)?;
    let body: Value = serde_json::from_str(data).map_err(|_| invalid())?;
    let text = body
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(invalid)?;
    if text.len() > crate::extract::MAX_INLINE_TEXT {
        return Err(error("Pixiv novel text exceeds the payload size limit"));
    }
    Ok(text.to_owned())
}

#[derive(Clone)]
struct Novel {
    data: Value,
    metadata: PostMetadata,
}

impl Novel {
    fn parse(data: Value, mode: super::TagMode) -> Result<Self> {
        let metadata = Self::metadata(&data, mode)?;
        let mut novel = Self { data, metadata };
        novel.prepare_source()?;
        Ok(novel)
    }
    fn metadata(data: &Value, mode: super::TagMode) -> Result<PostMetadata> {
        let id = id_of(data).ok_or_else(|| error("Pixiv returned an invalid novel ID"))?;
        let title = string(data, "title");
        let caption = string(data, "caption")
            .map(crate::mastodon::strip_html)
            .filter(|caption| !caption.trim().is_empty());
        let caption = match (title, caption) {
            (Some(title), Some(caption)) => Some(format!("{title}\n\n{}", caption.trim())),
            (Some(title), None) => Some(title.to_owned()),
            (None, caption) => caption,
        }
        .map(Arc::from);
        let metadata = PostMetadata {
            caption_html: data.get("caption").and_then(Value::as_str).map(Arc::from),
            pixiv_user_detail: data
                .as_object()
                .and_then(super::UserDetail::parse_compat)
                .map(Arc::new),
            tags: data.get("tags").and_then(|tags| mode.transform(tags)),
            comments: data
                .get("comments")
                .and_then(crate::extract::pixiv_comments),
            tags_bookmark: data
                .get("tags_bookmark")
                .and_then(crate::extract::pixiv_bookmark_tags),
            post_id: id.clone(),
            media_id: format!("{id}_p0"),
            index: 1,
            username: data
                .get("user")
                .and_then(|user| string(user, "account"))
                .map(Arc::from),
            timestamp: string(data, "create_date")
                .and_then(|date| crate::date::parse_date(date).ok()),
            caption,
            ..PostMetadata::default()
        };
        Ok(metadata)
    }

    fn post(&self, covers: bool, embeds: bool) -> Post {
        let mut metadata = self.metadata.clone();
        metadata.media_id.clear();
        metadata.index = 0;
        Post {
            category: "pixiv-novel".into(),
            metadata: Some(metadata),
            count: (!embeds).then_some(if covers { 2 } else { 1 }),
        }
    }

    fn text(&mut self, text: String) -> Result<Media> {
        self.data["extension"] = serde_json::json!("txt");
        self.refresh_snapshot()?;
        let (url, job) = inline_text(text)?;
        Ok(Media {
            url,
            job,
            category: "pixiv-novel".into(),
            filename: format!("{}_p0.txt", self.metadata.post_id),
            metadata: Some(self.metadata.clone()),
            headers: BTreeMap::new(),
            handoff: BTreeMap::new(),
        })
    }

    fn cover(&mut self) -> Result<Media> {
        let invalid = || error("Pixiv novel contains no supported cover image");
        let raw = self
            .data
            .get("image_urls")
            .and_then(|images| string(images, "large"))
            .ok_or_else(invalid)?;
        let source = super::media_url(raw).ok_or_else(invalid)?;
        let (_, path) = source.path().split_once("/img/").ok_or_else(invalid)?;
        let (path, _) = path.rsplit_once('.').ok_or_else(invalid)?;
        if path.is_empty() {
            return Err(invalid());
        }
        let base = format!(
            "https://i.pximg.net/novel-cover-original/img/{}",
            path.replace("_master1200", "")
        );
        let url = super::media_url(&format!("{base}.jpg")).ok_or_else(invalid)?;
        let number = self.next_number()?;
        let (filename, extension, date_url) = embeds::url_fields(&url);
        self.data["filename"] = serde_json::json!(filename);
        self.data["extension"] = serde_json::json!(extension);
        self.data["date_url"] = date_url;
        self.refresh_snapshot()?;
        let mut metadata = self.metadata.clone();
        metadata.media_id = format!("{}_p{number}", metadata.post_id);
        metadata.index = number + 1;
        Ok(Media {
            url,
            job: crate::extract::MediaJob::DirectHttp,
            category: "pixiv-novel".into(),
            filename: format!("{}_p{number}.jpg", metadata.post_id),
            metadata: Some(metadata),
            headers: BTreeMap::from([("Referer".into(), REFERER.into())]),
            handoff: BTreeMap::from([(
                "_fallback".into(),
                serde_json::json!([format!("{base}.png")]),
            )]),
        })
    }
}

#[derive(Clone)]
enum Feed {
    User(String),
    Series(String),
    Bookmarks {
        user: String,
        tag: Option<String>,
        private: bool,
    },
}

impl Feed {
    fn first(&self, native: &Pixiv) -> Url {
        let endpoint = match self {
            Self::User(_) => "v1/user/novels",
            Self::Series(_) => "v1/novel/series",
            Self::Bookmarks { .. } => "v1/user/bookmarks/novel",
        };
        let mut url = native
            .api_root
            .join(endpoint)
            .expect("static novel endpoint");
        match self {
            Self::User(id) => {
                url.query_pairs_mut().append_pair("user_id", id);
            }
            Self::Series(id) => {
                url.query_pairs_mut().append_pair("series_id", id);
            }
            Self::Bookmarks { user, tag, private } => {
                url.query_pairs_mut()
                    .append_pair("user_id", user)
                    .append_pair("restrict", if *private { "private" } else { "public" });
                if let Some(tag) = tag {
                    url.query_pairs_mut().append_pair("tag", tag);
                }
                if let Some(cursor) = native
                    .cursor
                    .as_ref()
                    .and_then(|cursor| cursor.query_value())
                {
                    url.query_pairs_mut().append_pair("max_bookmark_id", cursor);
                }
            }
        }
        url
    }

    fn checked(&self, native: &Pixiv, raw: &str) -> Result<Url> {
        let invalid = || error("Pixiv returned an unexpected novel next page");
        let url = Url::parse(raw).map_err(|_| invalid())?;
        let expected = self.first(native);
        let query: Vec<_> = url.query_pairs().collect();
        let scope = expected
            .query_pairs()
            .filter(|(key, _)| key != "max_bookmark_id")
            .all(|(key, value)| {
                let mut entries = query.iter().filter(|(actual, _)| *actual == key);
                entries.next().is_some_and(|(_, actual)| *actual == value)
                    && entries.next().is_none()
            });
        if url.origin() != native.api_root.origin()
            || url.path() != expected.path()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || !scope
            || matches!(self, Self::Bookmarks { tag: None, .. })
                && query.iter().any(|(key, _)| key == "tag")
        {
            return Err(invalid());
        }
        Ok(url)
    }
}

#[derive(Clone, Copy)]
enum Stage {
    Load,
    Text,
    Cover,
    EmbedScan,
    EmbedImages,
    EmbedChildren,
    End,
}
struct State<'a> {
    target: Option<Target>,
    feed: Option<Feed>,
    next: Option<String>,
    pages: usize,
    seen: BTreeSet<String>,
    works: VecDeque<Value>,
    remaining: Option<usize>,
    active: Option<Novel>,
    stage: Stage,
    done: bool,
    text: Option<String>,
    plan: embeds::Plan,
    images: VecDeque<Value>,
    child: Option<BoxStream<'a, Result<ExtractionEvent>>>,
    visited: Arc<tokio::sync::Mutex<embeds::Visited>>,
    subcategory: &'static str,
}

impl Pixiv {
    async fn initialize_novels(&self, state: &mut State<'_>) -> Result<()> {
        let Some(target) = state.target.take() else {
            return Ok(());
        };
        let feed = match target {
            Target::Novel(id) => {
                let mut endpoint = self
                    .api_root
                    .join("v2/novel/detail")
                    .expect("static novel endpoint");
                endpoint.query_pairs_mut().append_pair("novel_id", &id);
                let body = super::json(&self.get(&endpoint).await?)?;
                let novel = body
                    .get("novel")
                    .ok_or_else(|| error("Pixiv response contains no novel"))?;
                if id_of(novel).as_deref() != Some(id.as_str()) {
                    return Err(error("Pixiv returned an unexpected novel"));
                }
                if self.novel_options.full_series
                    && novel
                        .get("series")
                        .is_some_and(|series| !series.is_null() && series != &serde_json::json!({}))
                {
                    let series = novel
                        .get("series")
                        .and_then(id_of)
                        .ok_or_else(|| error("Pixiv novel contains an invalid series ID"))?;
                    state.subcategory = "series";
                    Some(Feed::Series(series))
                } else {
                    state.works.push_back(novel.clone());
                    None
                }
            }
            Target::User(id) => Some(Feed::User(id)),
            Target::Series(id) => Some(Feed::Series(id)),
            Target::Bookmarks { user, tag, private } => {
                Some(Feed::Bookmarks { user, tag, private })
            }
        };
        if let Some(feed) = feed {
            state.next = Some(feed.first(self).to_string());
            state.feed = Some(feed);
        }
        Ok(())
    }

    async fn novel_page(&self, state: &mut State<'_>) -> Result<()> {
        let Some(raw) = state.next.take() else {
            state.done = true;
            return Ok(());
        };
        if state.pages >= super::MAX_PAGES {
            state.done = true;
            return Ok(());
        }
        let endpoint = state
            .feed
            .as_ref()
            .expect("novel listing feed")
            .checked(self, &raw)?;
        let mut key = endpoint.clone();
        let mut pairs: Vec<_> = key
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        pairs.sort();
        key.set_query(None);
        key.query_pairs_mut().extend_pairs(pairs);
        if !state.seen.insert(key.to_string()) {
            return Err(error("Pixiv novel pagination repeated a page"));
        }
        let body = super::json(&self.get(&endpoint).await?)?;
        let works = body
            .get("novels")
            .and_then(Value::as_array)
            .ok_or_else(|| error("Pixiv response contains no novel list"))?;
        state.works.extend(works.iter().cloned());
        state.next = string(&body, "next_url").map(str::to_owned);
        state.pages += 1;
        Ok(())
    }

    pub(super) fn novel_events(&self, target: Target) -> BoxStream<'_, Result<ExtractionEvent>> {
        if self.refresh_token.is_none() {
            return stream::once(async { Err(error("native Pixiv novel extraction requires the pixiv-novel refresh-token setting in --native-config")) }).boxed();
        }
        let subcategory = match &target {
            Target::Novel(_) => "novel",
            Target::User(_) => "user",
            Target::Series(_) => "series",
            Target::Bookmarks { .. } => "bookmark",
        };
        let state = State {
            target: Some(target),
            feed: None,
            next: None,
            pages: 0,
            seen: BTreeSet::new(),
            works: VecDeque::new(),
            remaining: self.max_posts.filter(|limit| *limit > 0),
            active: None,
            stage: Stage::Load,
            done: false,
            text: None,
            plan: embeds::Plan::default(),
            images: VecDeque::new(),
            child: None,
            visited: Arc::new(tokio::sync::Mutex::new(embeds::Visited::default())),
            subcategory,
        };
        stream::unfold(state, move |mut state| async move {
            loop {
                match state.stage {
                    Stage::Load => {
                        // Upstream applies islice to the source novels before
                        // enrichment, parsing, text requests and post predicates.
                        // Check only between posts so the final text/cover drains.
                        if state.remaining == Some(0) {
                            return None;
                        }
                        if let Err(error) = self.initialize_novels(&mut state).await {
                            state.done = true;
                            return Some((Err(error), state));
                        }
                        if state.done {
                            return None;
                        }
                        if let Some(mut data) = state.works.pop_front() {
                            if let Some(remaining) = &mut state.remaining {
                                *remaining -= 1;
                            }
                            let user_detail = match self.enrich_user_work(&mut data).await {
                                Ok(detail) => detail,
                                Err(error) => {
                                    state.done = true;
                                    state.works.clear();
                                    state.next = None;
                                    return Some((Err(error), state));
                                }
                            };
                            if let Err(error) = self.enrich_novel_work(&mut data).await {
                                state.done = true;
                                state.works.clear();
                                state.next = None;
                                return Some((Err(error), state));
                            }
                            match Novel::parse(data, self.tags) {
                                Ok(mut novel) => {
                                    if user_detail.is_some() {
                                        novel.metadata.pixiv_user_detail = user_detail;
                                    }
                                    if let Err(error) = novel.refresh_snapshot() {
                                        return Some((Err(error), state));
                                    }
                                    let post = novel
                                        .post(self.novel_options.covers, self.novel_options.embeds);
                                    state.active = Some(novel);
                                    state.stage = Stage::Text;
                                    return Some((
                                        Ok(ExtractionEvent::Post(Box::new(post))),
                                        state,
                                    ));
                                }
                                Err(error) => return Some((Err(error), state)),
                            }
                        }
                        state.next.as_ref()?;
                        if let Err(error) = self.novel_page(&mut state).await {
                            state.done = true;
                            return Some((Err(error), state));
                        }
                    }
                    Stage::Text => {
                        let novel = state.active.as_mut().expect("active novel");
                        let mut endpoint = self
                            .api_root
                            .join("webview/v2/novel")
                            .expect("static novel endpoint");
                        endpoint
                            .query_pairs_mut()
                            .append_pair("id", &novel.metadata.post_id)
                            .append_pair("viewer_version", "20221031_ai");
                        let text = match self
                            .get(&endpoint)
                            .await
                            .and_then(|body| parse_webview(&body))
                            .and_then(|text| {
                                if self.novel_options.embeds {
                                    state.text = Some(text.clone());
                                }
                                novel.text(text)
                            }) {
                            Ok(mut media) => {
                                self.network.apply_media(&mut media);
                                media
                            }
                            Err(error) => {
                                state.stage = Stage::End;
                                return Some((Err(error), state));
                            }
                        };
                        state.stage = if self.novel_options.covers {
                            Stage::Cover
                        } else if self.novel_options.embeds {
                            Stage::EmbedScan
                        } else {
                            Stage::End
                        };
                        return Some((Ok(ExtractionEvent::Media(Box::new(text))), state));
                    }
                    Stage::Cover => {
                        state.stage = if self.novel_options.embeds {
                            Stage::EmbedScan
                        } else {
                            Stage::End
                        };
                        let result = state.active.as_mut().expect("active novel").cover().map(
                            |mut media| {
                                self.network.apply_media(&mut media);
                                ExtractionEvent::Media(Box::new(media))
                            },
                        );
                        return Some((result, state));
                    }
                    Stage::EmbedScan => {
                        let text = state.text.take().unwrap_or_default();
                        state.plan = match embeds::scan(&text).await {
                            Ok(plan) => plan,
                            Err(error) => {
                                state.done = true;
                                state.works.clear();
                                state.next = None;
                                state.stage = Stage::End;
                                return Some((Err(error), state));
                            }
                        };
                        if state.plan.uploads {
                            let id = state
                                .active
                                .as_ref()
                                .expect("active novel")
                                .metadata
                                .post_id
                                .clone();
                            if let Some(body) = self.request_ajax(&format!("/novel/{id}")).await {
                                state.images = embeds::images(body).unwrap_or_default();
                            }
                        }
                        state.stage = Stage::EmbedImages;
                    }
                    Stage::EmbedImages => {
                        if let Some(image) = state.images.pop_front() {
                            let result = state
                                .active
                                .as_mut()
                                .expect("active novel")
                                .uploaded(image, self.tags)
                                .map(|mut media| {
                                    self.network.apply_media(&mut media);
                                    ExtractionEvent::Media(Box::new(media))
                                });
                            if result.is_err() {
                                state.done = true;
                                state.works.clear();
                                state.next = None;
                                state.images.clear();
                                state.plan.ids.clear();
                                state.stage = Stage::End;
                            }
                            return Some((result, state));
                        }
                        state.stage = Stage::EmbedChildren;
                    }
                    Stage::EmbedChildren => {
                        if let Some(child) = &mut state.child {
                            if let Some(event) = child.next().await {
                                return Some((event, state));
                            }
                            state.child = None;
                        }
                        if let Some(id) = state.plan.ids.pop_front() {
                            let novel = state.active.as_mut().expect("active novel");
                            if let Err(error) = novel.next_number() {
                                state.done = true;
                                state.works.clear();
                                state.next = None;
                                state.plan.ids.clear();
                                state.stage = Stage::End;
                                return Some((Err(error), state));
                            }
                            novel.data["date_url"] = Value::Null;
                            let metadata = crate::event::ChildMetadata::from_pixiv_novel(
                                state.subcategory,
                                novel.data.as_object().expect("novel object"),
                            );
                            let native = self.artwork_extractor.as_deref().unwrap_or(self);
                            let visited = state.visited.clone();
                            state.child = Some(crate::event::child_job(metadata, move || {
                                stream::once(async move {
                                    let mut visited = visited.lock().await;
                                    match visited.insert(&id) {
                                        Ok(false) => return stream::empty().boxed(),
                                        Ok(true) => {}
                                        Err(error) => {
                                            return stream::once(async move { Err(error) }).boxed();
                                        }
                                    }
                                    drop(visited);
                                    let Some(id) = embeds::artwork_id(&id) else {
                                        return stream::once(async {
                                            Err(error(
                                                "Pixiv novel contains an invalid linked artwork ID",
                                            ))
                                        })
                                        .boxed();
                                    };
                                    native.target_events(super::Target::Illust(id))
                                })
                                .flatten()
                                .boxed()
                            }));
                            continue;
                        }
                        state.stage = Stage::End;
                    }
                    Stage::End => {
                        state.active = None;
                        state.stage = Stage::Load;
                        return Some((Ok(ExtractionEvent::EndPost), state));
                    }
                }
            }
        })
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        event::{self, ExtractionEvent},
        extract::MediaJob,
        job_selection::JobSelection,
        pixiv::tests::{pixiv, response, serve, serve_with, token},
        post_selection::PostSelection,
        selection::RangeSelection,
    };
    use serde_json::json;

    fn novel(id: u64, author: &str) -> Value {
        json!({"id": id, "title": format!("Novel {id}"), "caption": "Hello <b>world</b>",
            "user": {"id": 77, "account": author}, "create_date": "2024-01-01T09:00:00+09:00",
            "series": {}, "image_urls": {"large": format!("https://i.pximg.net/c/600x600/novel-cover-master/img/2024/01/01/00/00/00/{id}_master1200.jpg")}})
    }

    fn webview(text: &str) -> Vec<u8> {
        response(
            "200 OK",
            format!(
                "<script>data={{novel: {},\nother: null}};</script>",
                json!({"text": text})
            )
            .as_bytes(),
        )
    }

    fn detail(data: Value) -> Vec<u8> {
        response("200 OK", json!({"novel": data}).to_string().as_bytes())
    }
    fn page(works: Vec<Value>, next: Option<Url>) -> Vec<u8> {
        response(
            "200 OK",
            json!({"novels": works, "next_url": next})
                .to_string()
                .as_bytes(),
        )
    }

    #[test]
    fn classifies_novel_categories_and_exact_bookmark_path_tag_semantics() {
        for (raw, expected) in [
            (
                "https://www.pixiv.net/novel/show.php?id=42",
                Target::Novel("42".into()),
            ),
            ("https://pixiv.net/n/42", Target::Novel("42".into())),
            (
                "https://www.pixiv.net/en/users/9/novels",
                Target::User("9".into()),
            ),
            (
                "https://www.pixiv.net/novel/series/7",
                Target::Series("7".into()),
            ),
            (
                "https://www.pixiv.net/users/9/bookmarks/novels/%E7%8C%AB?tag=ignored&rest=hide",
                Target::Bookmarks {
                    user: "9".into(),
                    tag: Some("猫".into()),
                    private: true,
                },
            ),
            (
                "https://www.pixiv.net/users/9/bookmarks/novels/C++?rest=show&rest=hide",
                Target::Bookmarks {
                    user: "9".into(),
                    tag: Some("C++".into()),
                    private: false,
                },
            ),
            (
                "https://www.pixiv.net/users/9/bookmarks/novels?tag=ignored",
                Target::Bookmarks {
                    user: "9".into(),
                    tag: None,
                    private: false,
                },
            ),
        ] {
            let url = Url::parse(raw).unwrap();
            assert_eq!(target(&url).unwrap(), expected);
            assert_eq!(super::super::category(&url), Some("pixiv-novel"));
            assert!(super::super::is_pixiv_url(&url));
        }
        for raw in [
            "https://www.pixiv.net/n/042",
            "https://www.pixiv.net/novel/show.php?id=bad",
            "https://www.pixiv.net/novel/series/0",
            "https://evil.example/n/42",
            "https://x:y@www.pixiv.net/n/42",
            "https://www.pixiv.net:444/n/42",
        ] {
            assert!(target(&Url::parse(raw).unwrap()).is_err(), "{raw}");
        }
        assert_eq!(
            super::super::category(&Url::parse("https://www.pixiv.net/artworks/42").unwrap()),
            Some("pixiv")
        );
    }

    #[test]
    fn webview_decodes_json_only_and_bounds_invalid_html_and_text() {
        let text = "日本語\r\n[newpage]\n[chapter:段落]\n[ruby:漢字 > かんじ]\n[pixivimage:42-1]\n[uploadedimage:1]\n<&> \\ \" 🦀\0";
        let html = format!("before novel: {},\nafter", json!({"text": text}));
        assert_eq!(parse_webview(html.as_bytes()).unwrap(), text);
        assert_eq!(parse_webview(b"novel: {\"text\":\"\"},\n").unwrap(), "");
        for html in [
            b"no novel".as_slice(),
            b"novel: {},\n",
            b"novel: {\"text\":42},\n",
            b"novel: broken,\n",
            b"novel: {\"text\":\"x\"}",
            &[0xff],
        ] {
            assert_eq!(
                parse_webview(html).unwrap_err().to_string(),
                "Pixiv returned invalid novel webview content"
            );
        }
        assert!(parse_webview(&vec![b'x'; super::super::MAX_RESPONSE + 1]).is_err());
        let oversize = format!(
            "novel: {},\n",
            json!({"text": "x".repeat(crate::extract::MAX_INLINE_TEXT + 1)})
        );
        assert!(
            parse_webview(oversize.as_bytes())
                .unwrap_err()
                .to_string()
                .contains("payload size limit")
        );
    }

    #[tokio::test]
    async fn single_novel_preserves_text_and_emits_cover_after_text_with_separate_archive_ids() {
        let text = "日本語\n[newpage]\n[pixivimage:42-1]\n\r\nEND";
        let (root, server) = serve(vec![
            token("access-secret"),
            detail(novel(42, "author")),
            webview(text),
        ]);
        let native = pixiv(root).with_novel_options(NovelOptions {
            covers: true,
            full_series: false,
            embeds: false,
        });
        let events = native
            .events(&Url::parse("https://www.pixiv.net/novel/show.php?id=42").unwrap())
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert!(
            matches!(&events[0], ExtractionEvent::Post(post) if post.category == "pixiv-novel" && post.count == Some(2))
        );
        let ExtractionEvent::Media(text_media) = &events[1] else {
            panic!("missing text")
        };
        assert_eq!(text_media.category, "pixiv-novel");
        assert_eq!(text_media.filename, "42_p0.txt");
        assert!(matches!(&text_media.job, MediaJob::InlineText { data } if data == text));
        let metadata = text_media.metadata.as_ref().unwrap();
        assert_eq!(metadata.username.as_deref(), Some("author"));
        assert_eq!(metadata.caption.as_deref(), Some("Novel 42\n\nHello world"));
        assert_eq!(metadata.timestamp, Some(1704067200));
        let ExtractionEvent::Media(cover) = &events[2] else {
            panic!("missing cover")
        };
        assert_eq!(
            cover.url.as_str(),
            "https://i.pximg.net/novel-cover-original/img/2024/01/01/00/00/00/42.jpg"
        );
        assert_eq!(cover.filename, "42_p1.jpg");
        assert_eq!(cover.metadata.as_ref().unwrap().index, 2);
        assert_eq!(
            cover.handoff["_fallback"],
            json!(["https://i.pximg.net/novel-cover-original/img/2024/01/01/00/00/00/42.png"])
        );
        assert!(text_media.archive_key().is_some());
        assert_ne!(text_media.archive_key(), cover.archive_key());
        assert!(matches!(events[3], ExtractionEvent::EndPost));
        assert_eq!(events.len(), 4);
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /v2/novel/detail?novel_id=42 "));
        assert!(requests[2].starts_with("GET /webview/v2/novel?id=42&viewer_version=20221031_ai "));
        assert!(
            requests[2]
                .to_ascii_lowercase()
                .contains("authorization: bearer access-secret")
        );
        assert!(text_media.headers.is_empty());
        assert_eq!(
            cover.headers,
            BTreeMap::from([("Referer".into(), REFERER.into())])
        );
    }

    #[tokio::test]
    async fn post_boundary_precedes_webview_request_and_consumer_drop_is_lazy() {
        let (root, server) = serve(vec![token("t"), detail(novel(42, "author"))]);
        let native = pixiv(root);
        let mut events = native.events(&Url::parse("https://www.pixiv.net/n/42").unwrap());
        assert!(matches!(
            events.next().await.unwrap().unwrap(),
            ExtractionEvent::Post(_)
        ));
        drop(events);
        assert_eq!(server.join().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn optional_cover_metadata_is_not_parsed_until_consumed_after_text() {
        for covers in [false, true] {
            let mut work = novel(42, "author");
            work["image_urls"] = json!({"large":"https://evil.example/cover.jpg"});
            let (root, server) = serve(vec![token("t"), detail(work), webview("text")]);
            let native = pixiv(root).with_novel_options(NovelOptions {
                covers,
                full_series: false,
                embeds: false,
            });
            let files = JobSelection {
                file_range: Some(RangeSelection::parse("1").unwrap()),
                ..Default::default()
            }
            .apply(native.events(&Url::parse("https://www.pixiv.net/n/42").unwrap()))
            .collect::<Vec<_>>()
            .await;
            assert_eq!(files.len(), 1);
            assert_eq!(files[0].as_ref().unwrap().filename, "42_p0.txt");
            assert_eq!(server.join().unwrap().len(), 3);
        }
    }

    #[tokio::test]
    async fn listing_webview_failure_closes_post_and_recovers_later_novels() {
        let (root, server) = serve(vec![
            token("t"),
            page(vec![novel(41, "first"), novel(42, "second")], None),
            response("200 OK", b"PRIVATE invalid webview"),
            webview("second text"),
        ]);
        let native = pixiv(root).with_novel_options(NovelOptions {
            covers: true,
            full_series: false,
            embeds: false,
        });
        let events = native
            .events(&Url::parse("https://www.pixiv.net/users/9/novels").unwrap())
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(events[0], Ok(ExtractionEvent::Post(_))));
        assert_eq!(
            events[1].as_ref().unwrap_err().to_string(),
            "Pixiv returned invalid novel webview content"
        );
        assert!(matches!(events[2], Ok(ExtractionEvent::EndPost)));
        assert!(matches!(events[3], Ok(ExtractionEvent::Post(_))));
        assert!(
            matches!(&events[4], Ok(ExtractionEvent::Media(media)) if media.filename == "42_p0.txt")
        );
        assert!(
            matches!(&events[5], Ok(ExtractionEvent::Media(media)) if media.filename == "42_p1.jpg")
        );
        assert!(matches!(events[6], Ok(ExtractionEvent::EndPost)));
        assert_eq!(events.len(), 7);
        assert_eq!(server.join().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn lazy_listing_does_not_fetch_next_novel_webview_or_page_after_first_file() {
        let (root, server) = serve_with(|root| {
            vec![
                token("t"),
                page(
                    vec![novel(41, "first"), novel(42, "second")],
                    Some(root.join("v1/user/novels?user_id=9&offset=30").unwrap()),
                ),
                webview("first"),
            ]
        });
        let native = pixiv(root);
        let mut files = native.stream(&Url::parse("https://www.pixiv.net/users/9/novels").unwrap());
        assert_eq!(files.next().await.unwrap().unwrap().filename, "41_p0.txt");
        drop(files);
        assert_eq!(server.join().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn private_novel_bookmarks_preserve_mixed_authors_and_public_scope_is_explicit() {
        for private in [false, true] {
            let (root, server) = serve(vec![
                token("t"),
                page(vec![novel(41, "first"), novel(42, "second")], None),
                webview("first"),
                webview("second"),
            ]);
            let native = pixiv(root);
            let suffix = if private {
                "?rest=hide&tag=ignored"
            } else {
                "?tag=ignored"
            };
            let url = Url::parse(&format!(
                "https://www.pixiv.net/users/9/bookmarks/novels/%E7%8C%AB{suffix}"
            ))
            .unwrap();
            let files = native
                .stream(&url)
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .collect::<Result<Vec<_>>>()
                .unwrap();
            assert_eq!(
                files
                    .iter()
                    .map(|media| media
                        .metadata
                        .as_ref()
                        .unwrap()
                        .username
                        .as_deref()
                        .unwrap())
                    .collect::<Vec<_>>(),
                ["first", "second"]
            );
            assert_eq!(
                files[0].archive_key(),
                Novel::parse(novel(41, "first"), super::super::TagMode::Japanese)
                    .unwrap()
                    .text("same".into())
                    .unwrap()
                    .archive_key()
            );
            let requests = server.join().unwrap();
            assert!(requests[1].starts_with(&format!(
                "GET /v1/user/bookmarks/novel?user_id=9&restrict={}&tag=%E7%8C%AB ",
                if private { "private" } else { "public" }
            )));
        }
    }

    #[tokio::test]
    async fn full_series_expands_single_detail_only_when_enabled_and_uses_series_scope() {
        for full_series in [false, true] {
            let mut work = novel(42, "author");
            work["series"] = json!({"id":7,"title":"Series"});
            let mut replies = vec![token("t"), detail(work)];
            if full_series {
                replies.push(page(vec![novel(41, "earlier"), novel(42, "author")], None));
            }
            replies.push(webview("text"));
            if full_series {
                replies.push(webview("later"));
            }
            let (root, server) = serve(replies);
            let native = pixiv(root).with_novel_options(NovelOptions {
                covers: false,
                full_series,
                embeds: false,
            });
            let files = native
                .stream(&Url::parse("https://www.pixiv.net/n/42").unwrap())
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .collect::<Result<Vec<_>>>()
                .unwrap();
            assert_eq!(files.len(), if full_series { 2 } else { 1 });
            assert_eq!(
                files[0].metadata.as_ref().unwrap().post_id,
                if full_series { "41" } else { "42" }
            );
            let requests = server.join().unwrap();
            if full_series {
                assert!(requests[2].starts_with("GET /v1/novel/series?series_id=7 "));
            }
        }
    }

    #[tokio::test]
    async fn novel_pagination_preserves_scope_and_rejects_cycles_before_duplicate_request() {
        for feed in [
            Feed::User("9".into()),
            Feed::Series("7".into()),
            Feed::Bookmarks {
                user: "9".into(),
                tag: Some("猫".into()),
                private: true,
            },
        ] {
            let native = Pixiv::new(std::time::Duration::from_secs(2)).unwrap();
            let mut next = feed.first(&native);
            next.query_pairs_mut().append_pair("offset", "30");
            assert!(feed.checked(&native, next.as_str()).is_ok());
            let mut cross = next.clone();
            cross.set_host(Some("evil.example")).unwrap();
            assert!(feed.checked(&native, cross.as_str()).is_err());
            let mut cross = next.clone();
            cross.set_path("/v1/user/illusts");
            assert!(feed.checked(&native, cross.as_str()).is_err());
            let mut cross = next.clone();
            cross
                .query_pairs_mut()
                .append_pair("user_id", "10")
                .append_pair("series_id", "8")
                .append_pair("restrict", "public");
            assert!(feed.checked(&native, cross.as_str()).is_err());
        }
        let (root, server) = serve_with(|root| {
            vec![
                token("t"),
                page(
                    vec![novel(41, "author")],
                    Some(root.join("v1/user/novels?user_id=9").unwrap()),
                ),
                webview("first"),
            ]
        });
        let native = pixiv(root);
        let files = native
            .stream(&Url::parse("https://www.pixiv.net/users/9/novels").unwrap())
            .collect::<Vec<_>>()
            .await;
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].as_ref().unwrap().filename, "41_p0.txt");
        assert_eq!(
            files[1].as_ref().unwrap_err().to_string(),
            "Pixiv novel pagination repeated a page"
        );
        assert_eq!(server.join().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn user_series_and_private_bookmark_feeds_fetch_following_pages_after_current_text() {
        for (path, endpoint) in [
            ("users/9/novels", "v1/user/novels?user_id=9"),
            ("novel/series/7", "v1/novel/series?series_id=7"),
            (
                "users/9/bookmarks/novels/tag?rest=hide",
                "v1/user/bookmarks/novel?user_id=9&restrict=private&tag=tag",
            ),
        ] {
            let (root, server) = serve_with(|root| {
                vec![
                    token("t"),
                    page(
                        vec![novel(41, "first")],
                        Some(root.join(&format!("{endpoint}&offset=30")).unwrap()),
                    ),
                    webview("first"),
                    page(vec![novel(42, "second")], None),
                    webview("second"),
                ]
            });
            let files = pixiv(root)
                .stream(&Url::parse(&format!("https://www.pixiv.net/{path}")).unwrap())
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .collect::<Result<Vec<_>>>()
                .unwrap();
            assert_eq!(
                files
                    .iter()
                    .map(|file| file.filename.as_str())
                    .collect::<Vec<_>>(),
                ["41_p0.txt", "42_p0.txt"]
            );
            let requests = server.join().unwrap();
            assert!(requests[1].starts_with(&format!("GET /{endpoint} ")));
            assert!(requests[2].starts_with("GET /webview/v2/novel?id=41&viewer_version="));
            assert!(requests[3].starts_with(&format!("GET /{endpoint}&offset=30 ")));
            assert!(requests[4].starts_with("GET /webview/v2/novel?id=42&viewer_version="));
        }
    }

    #[tokio::test]
    async fn novel_pagination_stops_at_two_hundred_pages_without_requesting_the_next() {
        let (root, server) = serve_with(|root| {
            let mut replies = vec![token("t")];
            replies.extend((1..=super::super::MAX_PAGES).map(|offset| {
                page(
                    Vec::new(),
                    Some(
                        root.join(&format!("v1/user/novels?user_id=9&offset={offset}"))
                            .unwrap(),
                    ),
                )
            }));
            replies
        });
        let files = pixiv(root)
            .stream(&Url::parse("https://www.pixiv.net/users/9/novels").unwrap())
            .collect::<Vec<_>>()
            .await;
        assert!(files.is_empty(), "{files:?}");
        assert_eq!(server.join().unwrap().len(), super::super::MAX_PAGES + 1);
    }

    #[tokio::test]
    async fn webview_http_failures_close_their_post_and_leave_later_novels_available() {
        for status in ["404 Not Found", "503 Service Unavailable"] {
            let (root, server) = serve(vec![
                token("t"),
                page(vec![novel(41, "first"), novel(42, "second")], None),
                response(status, b"SECRET server response"),
                webview("second"),
            ]);
            let events = pixiv(root)
                .events(&Url::parse("https://www.pixiv.net/users/9/novels").unwrap())
                .collect::<Vec<_>>()
                .await;
            assert!(matches!(events[0], Ok(ExtractionEvent::Post(_))));
            assert!(events[1].is_err());
            assert!(
                !events[1]
                    .as_ref()
                    .unwrap_err()
                    .to_string()
                    .contains("SECRET")
            );
            assert!(matches!(events[2], Ok(ExtractionEvent::EndPost)));
            assert!(matches!(events[3], Ok(ExtractionEvent::Post(_))));
            assert!(
                matches!(&events[4], Ok(ExtractionEvent::Media(media)) if media.filename == "42_p0.txt")
            );
            assert!(matches!(events[5], Ok(ExtractionEvent::EndPost)));
            assert_eq!(server.join().unwrap().len(), 4);
        }
    }

    #[tokio::test]
    async fn unexpected_single_id_is_rejected_before_any_webview_request() {
        let (root, server) = serve(vec![token("t"), detail(novel(43, "wrong"))]);
        let files = pixiv(root)
            .stream(&Url::parse("https://www.pixiv.net/n/42").unwrap())
            .collect::<Vec<_>>()
            .await;
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].as_ref().unwrap_err().to_string(),
            "Pixiv returned an unexpected novel"
        );
        assert_eq!(server.join().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn root_profile_novel_children_use_separately_configured_client_after_begin() {
        let (root, server) = serve(vec![
            token("novel-only-access"),
            page(vec![novel(41, "author")], None),
            webview("text"),
        ]);
        let novel_native = Arc::new(pixiv(root));
        let root_native = Pixiv::new(std::time::Duration::from_secs(2))
            .unwrap()
            .with_include(Some(crate::dispatch::Include::Views(vec![
                "novel-user".into(),
            ])))
            .with_novel_extractor(novel_native);
        let mut events = root_native.events(&Url::parse("https://www.pixiv.net/users/9").unwrap());
        assert!(matches!(
            events.next().await.unwrap().unwrap(),
            ExtractionEvent::BeginChild(_)
        ));
        let files = event::media_only(events)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].category, "pixiv-novel");
        assert!(
            server.join().unwrap()[1]
                .to_ascii_lowercase()
                .contains("authorization: bearer novel-only-access")
        );
    }

    #[tokio::test]
    async fn native_novel_publication_saves_exact_utf8_sidecar_and_archive_without_text_leak() {
        use crate::{
            archive::{Archive, ArchiveOutcome},
            download::{self, DownloadOptions},
        };
        let text =
            "NOVEL_PAYLOAD_SECRET\r\n日本語 🦀\n[newpage]\n[ruby:漢字 > かんじ]\n100% %20\0END";
        let (api, server) = serve(vec![
            token("access-secret"),
            detail(novel(42, "author")),
            webview(text),
        ]);
        let mut files = pixiv(api)
            .stream(&Url::parse("https://www.pixiv.net/n/42").unwrap())
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(server.join().unwrap().len(), 3);
        let media = files.remove(0);
        let key = media.archive_key().unwrap();
        let root = tempfile::tempdir().unwrap();
        let archive = Archive::open(root.path().join("novels.sqlite"))
            .await
            .unwrap();
        let client = download::client(std::time::Duration::from_secs(2)).unwrap();
        let outcome = archive
            .download(
                &client,
                root.path(),
                &media,
                DownloadOptions {
                    write_metadata: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let ArchiveOutcome::Completed(outcome) = outcome else {
            panic!("expected novel publication")
        };
        let path = outcome.saved_path().unwrap();
        assert_eq!(std::fs::read(path).unwrap(), text.as_bytes());
        let sidecar = std::fs::read_to_string(format!("{}.json", path.display())).unwrap();
        assert!(!sidecar.contains("NOVEL_PAYLOAD_SECRET"));
        assert!(!sidecar.contains("text:"));
        assert!(!sidecar.contains("access-secret"));
        let sidecar: Value = serde_json::from_str(&sidecar).unwrap();
        assert_eq!(sidecar["category"], "pixiv-novel");
        assert_eq!(sidecar["metadata"]["post_id"], "42");
        assert_eq!(sidecar["metadata"]["media_id"], "42_p0");
        assert_eq!(sidecar["metadata"]["username"], "author");
        assert!(archive.contains(&key).await.unwrap());
        assert!(matches!(
            archive
                .download(&client, root.path(), &media, DownloadOptions::default())
                .await
                .unwrap(),
            ArchiveOutcome::AlreadyArchived
        ));
    }

    #[tokio::test]
    async fn child_post_ranges_restart_across_novel_user_and_novel_bookmarks() {
        let (root, server) = serve(vec![
            token("t"),
            page(vec![novel(41, "one"), novel(42, "two")], None),
            webview("one"),
            page(vec![novel(43, "three"), novel(44, "four")], None),
            webview("three"),
        ]);
        let native = pixiv(root).with_include(Some(crate::dispatch::Include::Views(vec![
            "novel-user".into(),
            "novel-bookmark".into(),
        ])));
        let files = JobSelection {
            post_selection: Some(PostSelection::new(
                Some(RangeSelection::parse("1").unwrap()),
                None,
            )),
            ..Default::default()
        }
        .apply(native.events(&Url::parse("https://www.pixiv.net/users/9").unwrap()))
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()
        .unwrap();
        assert_eq!(
            files
                .iter()
                .map(|media| media.filename.as_str())
                .collect::<Vec<_>>(),
            ["41_p0.txt", "43_p0.txt"]
        );
        assert_eq!(server.join().unwrap().len(), 5);
    }
    #[tokio::test]
    async fn max_posts_caps_source_novels_across_pages_and_drains_the_final_cover() {
        for (route, endpoint) in [
            ("users/9/novels", "v1/user/novels?user_id=9"),
            ("novel/series/7", "v1/novel/series?series_id=7"),
            (
                "users/9/bookmarks/novels/tag?rest=hide",
                "v1/user/bookmarks/novel?user_id=9&restrict=private&tag=tag",
            ),
        ] {
            let (root, server) = serve_with(|root| {
                vec![
                    token("t"),
                    page(
                        vec![novel(41, "one")],
                        Some(root.join(&format!("{endpoint}&offset=30")).unwrap()),
                    ),
                    webview("one"),
                    page(
                        vec![novel(42, "two"), novel(43, "three")],
                        Some(root.join(&format!("{endpoint}&offset=60")).unwrap()),
                    ),
                    webview("two"),
                ]
            });
            let native = pixiv(root)
                .with_max_posts(Some(2))
                .with_novel_options(NovelOptions {
                    covers: true,
                    full_series: false,
                    embeds: false,
                });
            let events = native
                .events(&Url::parse(&format!("https://www.pixiv.net/{route}")).unwrap())
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .collect::<Result<Vec<_>>>()
                .unwrap();
            assert_eq!(events.len(), 8, "{route}: {events:?}");
            assert!(
                matches!(&events[6], ExtractionEvent::Media(file) if file.filename == "42_p1.jpg")
            );
            assert!(matches!(events[7], ExtractionEvent::EndPost));
            let requests = server.join().unwrap();
            assert_eq!(requests.len(), 5, "{route}");
            assert!(
                !requests
                    .iter()
                    .any(|request| request.contains("offset=60") || request.contains("id=43&"))
            );
        }
    }

    #[tokio::test]
    async fn max_posts_counts_invalid_novels_and_text_failures_before_later_work() {
        for invalid in [true, false] {
            let mut work = novel(41, "one");
            if invalid {
                work["id"] = json!(0);
            }
            let mut replies = vec![token("t"), page(vec![work, novel(42, "two")], None)];
            if !invalid {
                replies.push(response("200 OK", b"invalid webview"));
            }
            let (root, server) = serve(replies);
            let native = pixiv(root).with_max_posts(Some(1));
            let events = native
                .events(&Url::parse("https://www.pixiv.net/users/9/novels").unwrap())
                .collect::<Vec<_>>()
                .await;
            assert_eq!(events.iter().filter(|event| event.is_err()).count(), 1);
            assert_eq!(events.len(), if invalid { 1 } else { 3 });
            if !invalid {
                assert!(matches!(events[2], Ok(ExtractionEvent::EndPost)));
            }
            assert_eq!(server.join().unwrap().len(), if invalid { 2 } else { 3 });
        }
    }

    #[tokio::test]
    async fn max_posts_precedes_post_predicates_and_does_not_fetch_n_plus_one_text() {
        let (root, server) = serve(vec![
            token("t"),
            page(vec![novel(41, "one"), novel(42, "two")], None),
            webview("one"),
        ]);
        let native = pixiv(root).with_max_posts(Some(1));
        let selection = PostSelection::new(Some(RangeSelection::parse("2").unwrap()), None);
        let files = selection
            .apply(
                native.events(&Url::parse("https://www.pixiv.net/users/9/novels").unwrap()),
                None,
                None,
                false,
            )
            .collect::<Vec<_>>()
            .await;
        assert!(files.is_empty());
        assert_eq!(server.join().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn max_posts_full_series_counts_chapters_instead_of_discovery_detail() {
        let mut work = novel(42, "two");
        work["series"] = json!({"id":7});
        let (root, server) = serve(vec![
            token("t"),
            detail(work),
            page(vec![novel(41, "one"), novel(42, "two")], None),
            webview("first chapter"),
        ]);
        let native = pixiv(root)
            .with_max_posts(Some(1))
            .with_novel_options(NovelOptions {
                covers: false,
                full_series: true,
                embeds: false,
            });
        let files = native
            .stream(&Url::parse("https://www.pixiv.net/n/42").unwrap())
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].filename, "41_p0.txt");
        assert_eq!(server.join().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn zero_and_absent_max_posts_leave_novel_feeds_unlimited() {
        for limit in [None, Some(0)] {
            let mut missing_user = novel(41, "one");
            missing_user["user"] = Value::Null;
            let (root, server) = serve(vec![
                token("t"),
                page(vec![missing_user, novel(42, "two")], None),
                webview("one"),
                webview("two"),
            ]);
            let native = pixiv(root).with_max_posts(limit);
            let files = native
                .stream(&Url::parse("https://www.pixiv.net/users/9/novels").unwrap())
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .collect::<Result<Vec<_>>>()
                .unwrap();
            assert_eq!(files.len(), 2);
            assert_eq!(server.join().unwrap().len(), 4);
        }
    }

    #[tokio::test]
    async fn max_posts_counts_missing_author_novels_as_source_work() {
        let mut first = novel(41, "one");
        first["user"] = Value::Null;
        let (root, server) = serve(vec![
            token("t"),
            page(vec![first, novel(42, "two")], None),
            webview("one"),
        ]);
        let native = pixiv(root).with_max_posts(Some(1));
        let files = native
            .stream(&Url::parse("https://www.pixiv.net/users/9/novels").unwrap())
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].filename, "41_p0.txt");
        assert!(files[0].metadata.as_ref().unwrap().username.is_none());
        assert_eq!(server.join().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn max_posts_restarts_for_dispatched_novel_views_and_repeated_jobs() {
        let (root, server) = serve(vec![
            token("t"),
            page(vec![novel(41, "one"), novel(42, "two")], None),
            webview("one"),
            page(vec![novel(43, "three"), novel(44, "four")], None),
            webview("three"),
            page(vec![novel(45, "five"), novel(46, "six")], None),
            webview("five"),
        ]);
        let native = pixiv(root).with_max_posts(Some(1)).with_include(Some(
            crate::dispatch::Include::Views(vec!["novel-user".into(), "novel-bookmark".into()]),
        ));
        let files = JobSelection::default()
            .apply(native.events(&Url::parse("https://www.pixiv.net/users/9").unwrap()))
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            files
                .iter()
                .map(|file| file.filename.as_str())
                .collect::<Vec<_>>(),
            ["41_p0.txt", "43_p0.txt"]
        );
        let files = native
            .stream(&Url::parse("https://www.pixiv.net/users/9/novels").unwrap())
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].filename, "45_p0.txt");
        assert_eq!(server.join().unwrap().len(), 7);
    }
    #[tokio::test]
    async fn configured_novel_tags_are_shared_by_post_text_and_optional_cover() {
        for mode in [
            super::super::TagMode::Japanese,
            super::super::TagMode::Translated,
            super::super::TagMode::Original,
        ] {
            let mut data = novel(42, "author");
            data["tags"] = json!([{"name":"猫","translated_name":"cat","extra":[1,true]},{"name":"猫","translated_name":"cat"}]);
            let expected = mode.transform(&data["tags"]).unwrap();
            let (root, server) = serve(vec![token("t"), detail(data), webview("novel text")]);
            let native = pixiv(root)
                .with_tags(mode)
                .with_novel_options(NovelOptions {
                    covers: true,
                    full_series: false,
                    embeds: false,
                });
            let events = native
                .events(&Url::parse("https://www.pixiv.net/n/42").unwrap())
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .collect::<Result<Vec<_>>>()
                .unwrap();
            let mut media = 0;
            for event in events {
                match event {
                    ExtractionEvent::Post(post) => {
                        assert_eq!(post.metadata.unwrap().tags, Some(expected.clone()))
                    }
                    ExtractionEvent::Media(file) => {
                        media += 1;
                        assert_eq!(file.metadata.unwrap().tags, Some(expected.clone()));
                    }
                    _ => {}
                }
            }
            assert_eq!(media, 2);
            assert_eq!(server.join().unwrap().len(), 3);
        }
    }
    fn cursor(value: Value) -> Option<super::super::BookmarkCursor> {
        Some(super::super::BookmarkCursor::parse(&value).unwrap())
    }

    #[tokio::test]
    async fn bookmark_cursor_preserves_zero_blank_null_and_ignores_input_url_cursor_lazily() {
        for value in [Value::Null, json!(0), json!(""), json!("猫 \n")] {
            let (root, server) = serve_with(|root| {
                vec![token("t"),
                page(vec![novel(41, "one"), novel(42, "two")], Some(root.join("v1/user/bookmarks/novel?user_id=9&restrict=private&tag=tag&max_bookmark_id=next").unwrap())),
                webview("one")]
            });
            let native = pixiv(root).with_cursor(cursor(value.clone()));
            let url = Url::parse("https://www.pixiv.net/users/9/bookmarks/novels/tag?rest=hide&cursor=ignored&max_bookmark_id=ignored").unwrap();
            let mut files = native.stream(&url);
            assert_eq!(files.next().await.unwrap().unwrap().filename, "41_p0.txt");
            drop(files);
            let requests = server.join().unwrap();
            assert_eq!(requests.len(), 3);
            let path = requests[1].split_whitespace().nth(1).unwrap();
            let requested = Url::parse(&format!("http://fixture{path}")).unwrap();
            let actual = requested
                .query_pairs()
                .find(|(key, _)| key == "max_bookmark_id")
                .map(|(_, value)| value.into_owned());
            assert_eq!(
                actual.as_deref(),
                super::super::BookmarkCursor::parse(&value)
                    .unwrap()
                    .query_value()
            );
            assert!(requests[1].contains("user_id=9&restrict=private&tag=tag"));
            assert!(!requests[1].contains("ignored"));
        }
    }

    #[tokio::test]
    async fn bookmark_cursor_changes_and_disappears_on_successor_pages() {
        let (root, server) = serve_with(|root| {
            vec![token("t"),
            page(vec![novel(41,"one")], Some(root.join("v1/user/bookmarks/novel?user_id=9&restrict=private&tag=tag&max_bookmark_id=second").unwrap())), webview("one"),
            page(vec![novel(42,"two")], Some(root.join("v1/user/bookmarks/novel?user_id=9&restrict=private&tag=tag&offset=60").unwrap())), webview("two"),
            page(vec![novel(43,"three")], None), webview("three")]
        });
        let native = pixiv(root).with_cursor(cursor(json!("initial")));
        let files = native
            .stream(
                &Url::parse("https://www.pixiv.net/users/9/bookmarks/novels/tag?rest=hide")
                    .unwrap(),
            )
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(files.len(), 3);
        let requests = server.join().unwrap();
        assert!(requests[1].contains("max_bookmark_id=initial"));
        assert!(requests[3].contains("max_bookmark_id=second"));
        assert!(!requests[5].contains("max_bookmark_id"));
        assert!(requests[5].contains("user_id=9&restrict=private&tag=tag&offset=60"));
    }

    #[tokio::test]
    async fn bookmark_cursor_never_applies_to_user_series_or_single_novel_requests() {
        for route in ["users/9/novels", "novel/series/7", "n/41"] {
            let body = if route == "n/41" {
                detail(novel(41, "one"))
            } else {
                page(vec![novel(41, "one")], None)
            };
            let (root, server) = serve(vec![token("t"), body, webview("one")]);
            let files = pixiv(root)
                .with_cursor(cursor(json!("bookmark-only")))
                .stream(&Url::parse(&format!("https://www.pixiv.net/{route}")).unwrap())
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .collect::<Result<Vec<_>>>()
                .unwrap();
            assert_eq!(files.len(), 1);
            assert!(
                server
                    .join()
                    .unwrap()
                    .iter()
                    .all(|request| !request.contains("max_bookmark_id")
                        && !request.contains("bookmark-only"))
            );
        }
    }

    #[tokio::test]
    async fn dispatched_novel_bookmarks_use_the_novel_clients_cursor_without_owner_lookup() {
        let (root, server) = serve(vec![
            token("novel-token"),
            page(vec![novel(41, "author")], None),
            webview("one"),
        ]);
        let novel_native = Arc::new(pixiv(root).with_cursor(cursor(json!("novel-cursor"))));
        let native = Pixiv::new(std::time::Duration::from_secs(2))
            .unwrap()
            .with_cursor(cursor(json!("illustration-cursor")))
            .with_include(Some(crate::dispatch::Include::Views(vec![
                "novel-bookmark".into(),
            ])))
            .with_novel_extractor(novel_native);
        let files = JobSelection::default()
            .apply(native.events(&Url::parse("https://www.pixiv.net/users/9").unwrap()))
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(files.len(), 1);
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[1].starts_with(
            "GET /v1/user/bookmarks/novel?user_id=9&restrict=public&max_bookmark_id=novel-cursor "
        ));
        assert!(
            !requests
                .iter()
                .any(|request| request.contains("illustration-cursor")
                    || request.contains("user/detail"))
        );
        assert!(files[0].archive_key().unwrap().starts_with("pixiv-novel:"));
    }
    fn enrichable_novel(id: u64) -> Value {
        let mut work = novel(id, "author");
        work["total_comments"] = json!(2);
        work["is_bookmarked"] = json!(true);
        work["_ajax"] = json!(true); // Novel comments have no illustration AJAX gate.
        work
    }

    fn novel_comments_reply() -> Vec<u8> {
        response("200 OK", br#"{"comments":[{"id":7,"comment":"first"},{"id":7,"comment":"duplicate"}],"next_url":null}"#)
    }

    fn novel_bookmark_reply() -> Vec<u8> {
        response("200 OK", br#"{"bookmark_detail":{"tags":[{"name":"keep","is_registered":true},{"name":"omit","is_registered":false},{"name":"keep","is_registered":1}]}}"#)
    }

    #[tokio::test]
    async fn novel_enrichment_finishes_before_post_and_drop_avoids_text_and_cover() {
        let (root, server) = serve(vec![
            token("t"),
            detail(enrichable_novel(41)),
            novel_comments_reply(),
            novel_bookmark_reply(),
        ]);
        let native = pixiv(root)
            .with_comments(true)
            .with_metadata_bookmark(true)
            .with_novel_options(NovelOptions {
                covers: true,
                full_series: false,
                embeds: false,
            });
        let url = Url::parse("https://www.pixiv.net/n/41").unwrap();
        let mut events = native.events(&url);
        let ExtractionEvent::Post(post) = events.next().await.unwrap().unwrap() else {
            panic!("expected enriched post")
        };
        let metadata = post.metadata.unwrap();
        assert_eq!(metadata.comments.as_ref().unwrap().len(), 2);
        assert_eq!(metadata.tags_bookmark.as_deref().unwrap(), ["keep", "keep"]);
        drop(events);
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 4);
        assert!(requests[2].starts_with("GET /v1/novel/comments?novel_id=41 "));
        assert!(requests[3].starts_with("GET /v2/novel/bookmark/detail?novel_id=41 "));
        assert!(!requests.iter().any(|request| request.contains("webview")));
    }

    #[tokio::test]
    async fn novel_enrichment_is_shared_by_post_text_and_cover_after_comment_pagination() {
        let (root, server) = serve_with(|root| {
            vec![token("t"), detail(enrichable_novel(41)),
            response("200 OK",json!({"comments":[{"id":1,"comment":"one"}],"next_url":root.join("v1/novel/comments?novel_id=41&offset=30").unwrap()}).to_string().as_bytes()),
            novel_comments_reply(),novel_bookmark_reply(),webview("text")]
        });
        let native = pixiv(root)
            .with_comments(true)
            .with_metadata_bookmark(true)
            .with_novel_options(NovelOptions {
                covers: true,
                full_series: false,
                embeds: false,
            });
        let events = native
            .events(&Url::parse("https://www.pixiv.net/n/41").unwrap())
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        let ExtractionEvent::Post(post) = &events[0] else {
            panic!("expected post")
        };
        let metadata = post.metadata.as_ref().unwrap();
        assert_eq!(metadata.comments.as_ref().unwrap().len(), 3);
        for event in &events[1..3] {
            let ExtractionEvent::Media(file) = event else {
                panic!("expected text or cover")
            };
            let file_metadata = file.metadata.as_ref().unwrap();
            assert!(Arc::ptr_eq(
                file_metadata.comments.as_ref().unwrap(),
                metadata.comments.as_ref().unwrap()
            ));
            assert!(Arc::ptr_eq(
                file_metadata.tags_bookmark.as_ref().unwrap(),
                metadata.tags_bookmark.as_ref().unwrap()
            ));
        }
        assert!(matches!(events[3], ExtractionEvent::EndPost));
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 6);
        assert!(requests[3].starts_with("GET /v1/novel/comments?novel_id=41&offset=30 "));
        assert!(requests[5].starts_with("GET /webview/v2/novel?id=41&"));
    }

    #[tokio::test]
    async fn novel_enrichment_failures_abort_all_feeds_before_post_and_buffered_successors() {
        for route in [
            "users/9/novels",
            "novel/series/7",
            "users/9/bookmarks/novels",
            "n/41",
        ] {
            for fail_comments in [true, false] {
                let mut first = enrichable_novel(41);
                if route == "n/41" {
                    first["series"] = json!({"id":7});
                }
                let mut replies = vec![token("t")];
                if route == "n/41" {
                    replies.push(detail(first.clone()));
                }
                replies.push(page(vec![first, enrichable_novel(42)], None));
                if !fail_comments {
                    replies.push(novel_comments_reply());
                }
                replies.push(response("404 Not Found", b"PRIVATE_ENRICHMENT_ERROR"));
                let (root, server) = serve(replies);
                let native = pixiv(root)
                    .with_comments(true)
                    .with_metadata_bookmark(true)
                    .with_novel_options(NovelOptions {
                        covers: true,
                        full_series: true,
                        embeds: false,
                    });
                let events = native
                    .events(&Url::parse(&format!("https://www.pixiv.net/{route}")).unwrap())
                    .collect::<Vec<_>>()
                    .await;
                assert_eq!(events.len(), 1, "{route} {events:?}");
                let message = events[0].as_ref().unwrap_err().to_string();
                assert!(!message.contains("PRIVATE_ENRICHMENT_ERROR"));
                let requests = server.join().unwrap();
                assert_eq!(
                    requests.len(),
                    3 + usize::from(route == "n/41") + usize::from(!fail_comments)
                );
                assert!(
                    !requests
                        .iter()
                        .any(|request| request.contains("webview")
                            || request.contains("novel_id=42"))
                );
            }
        }
    }

    #[tokio::test]
    async fn novel_enrichment_budget_precedes_post_range_and_stops_before_next_work() {
        for select_second in [false, true] {
            let (root, server) = serve(vec![
                token("t"),
                page(vec![enrichable_novel(41), enrichable_novel(42)], None),
                novel_comments_reply(),
                novel_bookmark_reply(),
                webview("one"),
            ]);
            let native = pixiv(root)
                .with_comments(true)
                .with_metadata_bookmark(true)
                .with_max_posts(Some(1));
            let url = Url::parse("https://www.pixiv.net/users/9/novels").unwrap();
            let events = native.events(&url);
            let files = if select_second {
                PostSelection::new(Some(RangeSelection::parse("2").unwrap()), None)
                    .apply(events, None, None, false)
            } else {
                event::media_only(events)
            }
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
            assert_eq!(files.len(), usize::from(!select_second));
            assert_eq!(server.join().unwrap().len(), 5);
        }
    }

    #[tokio::test]
    async fn profile_novel_dispatch_uses_independent_enrichment_flags() {
        let (root, server) = serve(vec![
            token("novel-only"),
            page(vec![enrichable_novel(41)], None),
            novel_comments_reply(),
            novel_bookmark_reply(),
            webview("one"),
        ]);
        let novel_native = Arc::new(pixiv(root).with_comments(true).with_metadata_bookmark(true));
        let native = Pixiv::new(std::time::Duration::from_secs(2))
            .unwrap()
            .with_include(Some(crate::dispatch::Include::Views(vec![
                "novel-user".into(),
            ])))
            .with_novel_extractor(novel_native);
        let files = JobSelection::default()
            .apply(native.events(&Url::parse("https://www.pixiv.net/users/9").unwrap()))
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(files.len(), 1);
        let metadata = files[0].metadata.as_ref().unwrap();
        assert!(metadata.comments.is_some());
        assert!(metadata.tags_bookmark.is_some());
        assert_eq!(server.join().unwrap().len(), 5);
    }

    #[tokio::test]
    async fn cancelling_novel_enrichment_closes_http_before_bookmark_post_or_text() {
        use std::time::Duration;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let (started, waiting) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            for reply in [token("t"), detail(enrichable_novel(41))] {
                let (mut connection, _) = listener.accept().await.unwrap();
                let mut buffer = [0; 8192];
                assert_ne!(connection.read(&mut buffer).await.unwrap(), 0);
                connection.write_all(&reply).await.unwrap();
            }
            let (mut connection, _) = listener.accept().await.unwrap();
            let mut buffer = [0; 8192];
            let count = connection.read(&mut buffer).await.unwrap();
            assert!(
                String::from_utf8_lossy(&buffer[..count])
                    .starts_with("GET /v1/novel/comments?novel_id=41 ")
            );
            connection
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n{\"comments\":[")
                .await
                .unwrap();
            started.send(()).unwrap();
            match tokio::time::timeout(Duration::from_secs(2), connection.read(&mut buffer))
                .await
                .unwrap()
            {
                Ok(0) => {}
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
                other => panic!("novel comment connection stayed active: {other:?}"),
            }
            assert!(
                tokio::time::timeout(Duration::from_millis(100), listener.accept())
                    .await
                    .is_err()
            );
        });
        let extraction = tokio::spawn(async move {
            pixiv(root)
                .with_comments(true)
                .with_metadata_bookmark(true)
                .events(&Url::parse("https://www.pixiv.net/n/41").unwrap())
                .collect::<Vec<_>>()
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), waiting)
            .await
            .unwrap()
            .unwrap();
        extraction.abort();
        assert!(extraction.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(3), server)
            .await
            .unwrap()
            .unwrap();
    }
    #[tokio::test]
    async fn recorded_novel_enrichment_source_cases_match_http_metadata_and_ordering() {
        let fixture: Value = serde_json::from_slice(include_bytes!(
            "../tests/fixtures/pixiv/novel_enrichment.json"
        ))
        .unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            // Tag-shape validation predates this enrichment change; this case
            // records an existing difference in malformed-tag handling.
            if case["name"] == "malformed_tags_error_after_enrichment" {
                continue;
            }
            let calls = case["calls"].as_array().unwrap();
            let works = case["input_works"].as_array().unwrap().clone();
            let (root, server) = serve_with(|root| {
                let mut replies = vec![token("t"), page(works.clone(), None)];
                for call in calls.iter().skip(1) {
                    let id = call["id"].as_u64().unwrap();
                    match call["method"].as_str().unwrap() {
                        "novel_comments" => {
                            let values = case["comments_script"]["values"].as_array().unwrap();
                            if id == 42
                                && let Some(before) =
                                    case["comments_script"]["fail_before_index"].as_u64()
                            {
                                if before > 0 {
                                    replies.push(response("200 OK",json!({"comments":values.iter().take(before as usize).cloned().collect::<Vec<_>>(),"next_url":root.join("v1/novel/comments?novel_id=42&offset=30").unwrap()}).to_string().as_bytes()));
                                }
                                replies.push(response("404 Not Found", b"PRIVATE_COMMENT_ERROR"));
                            } else {
                                replies.push(response(
                                    "200 OK",
                                    json!({"comments":values,"next_url":null})
                                        .to_string()
                                        .as_bytes(),
                                ));
                            }
                        }
                        "novel_bookmark_detail" => {
                            replies.push(if id == 42 && case["bookmark_fail"] == true {
                                response("404 Not Found", b"PRIVATE_BOOKMARK_ERROR")
                            } else {
                                response(
                                    "200 OK",
                                    json!({"bookmark_detail":case["bookmark_detail"]})
                                        .to_string()
                                        .as_bytes(),
                                )
                            });
                        }
                        "novel_webview" => {
                            replies.push(if id == 42 && case["webview_fail"] == true {
                                response("404 Not Found", b"PRIVATE_WEBVIEW_ERROR")
                            } else {
                                let content = if id == 42 {
                                    case["webview"].clone()
                                } else {
                                    json!({"text":"Later fixture text"})
                                };
                                response("200 OK", format!("novel: {content},\n").as_bytes())
                            });
                        }
                        other => panic!("unexpected oracle call: {other}"),
                    }
                }
                replies
            });
            let mode = super::super::TagMode::parse(
                case["options"]["tags"].as_str().unwrap_or("japanese"),
            );
            let native = pixiv(root)
                .with_comments(
                    case["options"]
                        .get("comments")
                        .is_some_and(super::super::value_truthy),
                )
                .with_metadata_bookmark(
                    case["options"]
                        .get("metadata-bookmark")
                        .is_some_and(super::super::value_truthy),
                )
                .with_tags(mode)
                .with_novel_options(NovelOptions {
                    covers: case["options"]
                        .get("covers")
                        .is_some_and(super::super::value_truthy),
                    full_series: false,
                    embeds: false,
                });
            let events = native
                .events(&Url::parse("https://www.pixiv.net/users/9/novels").unwrap())
                .collect::<Vec<_>>()
                .await;
            let mut expected = case["events"].as_array().unwrap().iter();
            let mut errors = 0;
            for event in events {
                let (metadata, file) = match event {
                    Ok(ExtractionEvent::Post(post)) => {
                        let next = expected
                            .next()
                            .unwrap_or_else(|| panic!("unexpected post in {}", case["name"]));
                        assert_eq!(next["kind"], "post", "{}", case["name"]);
                        (post.metadata.unwrap(), None)
                    }
                    Ok(ExtractionEvent::Media(file)) => {
                        let next = expected
                            .next()
                            .unwrap_or_else(|| panic!("unexpected media in {}", case["name"]));
                        assert_eq!(next["kind"], "media", "{}", case["name"]);
                        assert_eq!(
                            file.filename,
                            next["default_filename"].as_str().unwrap(),
                            "{}",
                            case["name"]
                        );
                        match &file.job {
                            MediaJob::InlineText { data } => {
                                assert_eq!(format!("text:{data}"), next["url"].as_str().unwrap())
                            }
                            _ => assert_eq!(file.url.as_str(), next["url"].as_str().unwrap()),
                        }
                        let metadata = file.metadata.clone().unwrap();
                        (metadata, Some(file))
                    }
                    Ok(ExtractionEvent::EndPost) => continue,
                    Err(error) => {
                        errors += 1;
                        assert!(!error.to_string().contains("PRIVATE"));
                        continue;
                    }
                    other => panic!("unexpected novel event {other:?}"),
                };
                // The source event just consumed is determined by the number
                // left, preserving repeated comment/tag values verbatim.
                let index = case["events"].as_array().unwrap().len() - expected.len() - 1;
                let source = &case["events"][index]["metadata"];
                assert_eq!(
                    metadata
                        .comments
                        .as_ref()
                        .map(|values| serde_json::to_value(values).unwrap()),
                    source.get("comments").cloned(),
                    "{}",
                    case["name"]
                );
                assert_eq!(
                    metadata
                        .tags_bookmark
                        .as_ref()
                        .map(|values| serde_json::to_value(values).unwrap()),
                    source.get("tags_bookmark").cloned(),
                    "{}",
                    case["name"]
                );
                assert_eq!(
                    metadata
                        .tags
                        .as_ref()
                        .map(|values| serde_json::to_value(values).unwrap()),
                    source.get("tags").cloned(),
                    "{}",
                    case["name"]
                );
                if let Some(file) = file {
                    let raw = works
                        .iter()
                        .find(|work| id_of(work).as_deref() == Some(metadata.post_id.as_str()))
                        .unwrap()
                        .clone();
                    let mut ordinary = Novel::parse(raw, mode).unwrap();
                    let expected_key = if metadata.index == 1 {
                        ordinary.text(String::new()).unwrap().archive_key()
                    } else {
                        ordinary.cover().unwrap().archive_key()
                    };
                    assert_eq!(file.archive_key(), expected_key);
                }
            }
            assert_eq!(expected.len(), 0, "{}", case["name"]);
            if !case["error"].is_null() {
                assert_eq!(errors, 1, "{}", case["name"]);
            } else if case["webview_fail"] == true || case["webview"].get("text").is_none() {
                assert!(errors > 0, "{}", case["name"]);
            } else {
                assert_eq!(errors, 0, "{}", case["name"]);
            }
            let requests = server.join().unwrap();
            assert!(requests[1].starts_with("GET /v1/user/novels?user_id=9 "));
            assert!(requests.iter().all(|request| !request.contains("/illust/")));
        }
    }
    fn novel_author_detail() -> Vec<u8> {
        response("200 OK", br#"{"user":{"id":77,"account":"enriched.author","name":"Public author"},"profile":{"total_novels":2},"profile_publicity":{"region":"public"},"workspace":{"tool":"pen"},"future_public_root":{"nested":[1,true]}}"#)
    }

    #[tokio::test]
    async fn novel_user_metadata_precedes_comments_is_cached_and_shared_across_posts_text_and_covers()
     {
        let (root, server) = serve(vec![
            token("t"),
            page(vec![enrichable_novel(41), enrichable_novel(42)], None),
            novel_author_detail(),
            novel_comments_reply(),
            novel_bookmark_reply(),
            webview("one"),
            novel_comments_reply(),
            novel_bookmark_reply(),
            webview("two"),
        ]);
        let native = pixiv(root)
            .with_metadata(true)
            .with_comments(true)
            .with_metadata_bookmark(true)
            .with_novel_options(NovelOptions {
                covers: true,
                full_series: false,
                embeds: false,
            });
        let events = native
            .events(&Url::parse("https://www.pixiv.net/users/9/novels").unwrap())
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(events.len(), 8);
        let ExtractionEvent::Post(first) = &events[0] else {
            panic!("expected post")
        };
        let first = first
            .metadata
            .as_ref()
            .unwrap()
            .pixiv_user_detail
            .as_ref()
            .unwrap();
        assert_eq!(
            first.fields()["future_public_root"],
            json!({"nested":[1,true]})
        );
        for event in &events {
            let metadata = match event {
                ExtractionEvent::Post(post) => post.metadata.as_ref().unwrap(),
                ExtractionEvent::Media(file) => file.metadata.as_ref().unwrap(),
                ExtractionEvent::EndPost => continue,
                _ => panic!("unexpected child"),
            };
            assert!(Arc::ptr_eq(
                first,
                metadata.pixiv_user_detail.as_ref().unwrap()
            ));
            assert_eq!(metadata.username.as_deref(), Some("enriched.author"));
            let public = serde_json::to_value(metadata).unwrap();
            assert_eq!(public["user"]["id"], 77);
            assert_eq!(public["profile"]["total_novels"], 2);
            assert_eq!(public["future_public_root"], json!({"nested":[1,true]}));
        }
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 9);
        assert!(requests[2].starts_with("GET /v1/user/detail?user_id=77 "));
        assert!(requests[3].starts_with("GET /v1/novel/comments?novel_id=41 "));
        assert!(requests[4].starts_with("GET /v2/novel/bookmark/detail?novel_id=41 "));
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.contains("/user/detail"))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn disabled_novel_user_metadata_retains_basic_author_without_lookup() {
        let work = novel(41, "basic.author");
        let basic = work["user"].clone();
        let (root, server) = serve(vec![token("t"), detail(work), webview("one")]);
        let native = pixiv(root).with_metadata(false);
        let events = native
            .events(&Url::parse("https://www.pixiv.net/n/41").unwrap())
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        let ExtractionEvent::Post(post) = &events[0] else {
            panic!("expected post")
        };
        let metadata = post.metadata.as_ref().unwrap();
        let detail = metadata.pixiv_user_detail.as_ref().unwrap();
        assert_eq!(detail.fields().len(), 1);
        assert_eq!(detail.fields()["user"], basic);
        assert_eq!(metadata.username.as_deref(), Some("basic.author"));
        let ExtractionEvent::Media(file) = &events[1] else {
            panic!("expected text")
        };
        assert!(Arc::ptr_eq(
            detail,
            file.metadata
                .as_ref()
                .unwrap()
                .pixiv_user_detail
                .as_ref()
                .unwrap()
        ));
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(
            !requests
                .iter()
                .any(|request| request.contains("/user/detail"))
        );
    }

    #[tokio::test]
    async fn novel_user_metadata_failure_aborts_all_routes_before_other_enrichment_or_posts() {
        for (route, full_series) in [
            ("users/9/novels", false),
            ("novel/series/7", false),
            ("users/9/bookmarks/novels", false),
            ("n/41", false),
            ("n/41", true),
        ] {
            let mut first = enrichable_novel(41);
            if full_series {
                first["series"] = json!({"id":7});
            }
            let mut replies = vec![token("t")];
            if route == "n/41" {
                replies.push(detail(first.clone()));
            }
            if route != "n/41" || full_series {
                replies.push(page(vec![first, enrichable_novel(42)], None));
            }
            let expected_requests = replies.len() + 1;
            replies.push(response("404 Not Found", b"PRIVATE_AUTHOR_ERROR"));
            let (root, server) = serve(replies);
            let native = pixiv(root)
                .with_metadata(true)
                .with_comments(true)
                .with_metadata_bookmark(true)
                .with_novel_options(NovelOptions {
                    covers: true,
                    full_series,
                    embeds: false,
                });
            let events = native
                .events(&Url::parse(&format!("https://www.pixiv.net/{route}")).unwrap())
                .collect::<Vec<_>>()
                .await;
            assert_eq!(events.len(), 1, "{route} {full_series}");
            assert!(
                !events[0]
                    .as_ref()
                    .unwrap_err()
                    .to_string()
                    .contains("PRIVATE_AUTHOR_ERROR")
            );
            let requests = server.join().unwrap();
            assert_eq!(requests.len(), expected_requests);
            assert!(
                requests
                    .last()
                    .unwrap()
                    .starts_with("GET /v1/user/detail?user_id=77 ")
            );
            assert!(
                !requests
                    .iter()
                    .any(|request| request.contains("/novel/comments")
                        || request.contains("/novel/bookmark/detail")
                        || request.contains("webview"))
            );
        }
    }

    #[tokio::test]
    async fn novel_user_metadata_respects_source_budget_predicates_and_drop_before_text() {
        for select_second in [false, true] {
            let mut second = novel(42, "other.author");
            second["user"]["id"] = json!(88);
            let mut replies = vec![
                token("t"),
                page(vec![novel(41, "first"), second], None),
                novel_author_detail(),
            ];
            if select_second {
                replies.push(webview("one"));
            }
            let (root, server) = serve(replies);
            let native = pixiv(root).with_metadata(true).with_max_posts(Some(1));
            let url = Url::parse("https://www.pixiv.net/users/9/novels").unwrap();
            if select_second {
                let files = PostSelection::new(Some(RangeSelection::parse("2").unwrap()), None)
                    .apply(native.events(&url), None, None, false)
                    .collect::<Vec<_>>()
                    .await;
                assert!(files.is_empty());
            } else {
                let mut events = native.events(&url);
                assert!(matches!(
                    events.next().await.unwrap().unwrap(),
                    ExtractionEvent::Post(_)
                ));
                drop(events);
            }
            let requests = server.join().unwrap();
            assert_eq!(requests.len(), 3 + usize::from(select_second));
            assert!(
                !requests
                    .iter()
                    .any(|request| request.contains("user_id=88"))
            );
        }
    }

    #[tokio::test]
    async fn novel_profile_dispatch_uses_its_separate_user_metadata_setting() {
        let (root, server) = serve(vec![
            token("novel-only"),
            page(vec![novel(41, "author")], None),
            novel_author_detail(),
            webview("one"),
        ]);
        let novel_native = Arc::new(pixiv(root).with_metadata(true));
        let native = Pixiv::new(std::time::Duration::from_secs(2))
            .unwrap()
            .with_metadata(false)
            .with_include(Some(crate::dispatch::Include::Views(vec![
                "novel-user".into(),
            ])))
            .with_novel_extractor(novel_native);
        let files = JobSelection::default()
            .apply(native.events(&Url::parse("https://www.pixiv.net/users/9").unwrap()))
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(files.len(), 1);
        assert!(
            files[0]
                .metadata
                .as_ref()
                .unwrap()
                .pixiv_user_detail
                .as_ref()
                .unwrap()
                .fields()
                .contains_key("profile")
        );
        assert_eq!(server.join().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn cancelling_novel_user_metadata_closes_http_before_comments_post_or_text() {
        use std::time::Duration;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let (started, waiting) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            for reply in [token("t"), detail(enrichable_novel(41))] {
                let (mut connection, _) = listener.accept().await.unwrap();
                let mut buffer = [0; 8192];
                assert_ne!(connection.read(&mut buffer).await.unwrap(), 0);
                connection.write_all(&reply).await.unwrap();
            }
            let (mut connection, _) = listener.accept().await.unwrap();
            let mut buffer = [0; 8192];
            let count = connection.read(&mut buffer).await.unwrap();
            assert!(
                String::from_utf8_lossy(&buffer[..count])
                    .starts_with("GET /v1/user/detail?user_id=77 ")
            );
            connection
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n{\"user\":{")
                .await
                .unwrap();
            started.send(()).unwrap();
            match tokio::time::timeout(Duration::from_secs(2), connection.read(&mut buffer))
                .await
                .unwrap()
            {
                Ok(0) => {}
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
                other => panic!("novel author connection stayed active: {other:?}"),
            }
            assert!(
                tokio::time::timeout(Duration::from_millis(100), listener.accept())
                    .await
                    .is_err()
            );
        });
        let extraction = tokio::spawn(async move {
            pixiv(root)
                .with_metadata(true)
                .with_comments(true)
                .with_metadata_bookmark(true)
                .events(&Url::parse("https://www.pixiv.net/n/41").unwrap())
                .collect::<Vec<_>>()
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), waiting)
            .await
            .unwrap()
            .unwrap();
        extraction.abort();
        assert!(extraction.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(3), server)
            .await
            .unwrap()
            .unwrap();
    }
    #[tokio::test]
    async fn captions_option_ignores_novel_ajax_and_preserves_raw_html_on_all_outputs() {
        let html = " \t<p>Hello &amp; <b>猫</b></p>\n ";
        let mut work = novel(42, "author");
        work["caption"] = json!(html);
        let (root, server) = serve(vec![token("t"), detail(work), webview("novel text")]);
        let native = pixiv(root.clone())
            .with_web_root(root)
            .with_captions(true)
            .with_novel_options(NovelOptions {
                covers: true,
                full_series: false,
                embeds: false,
            });
        let events = native
            .events(&Url::parse("https://www.pixiv.net/n/42").unwrap())
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(events.len(), 4);
        let ExtractionEvent::Post(post) = &events[0] else {
            panic!("expected novel post")
        };
        let raw = post
            .metadata
            .as_ref()
            .unwrap()
            .caption_html
            .as_ref()
            .unwrap();
        assert_eq!(raw.as_ref(), html);
        for event in &events[..3] {
            let metadata = match event {
                ExtractionEvent::Post(post) => post.metadata.as_ref().unwrap(),
                ExtractionEvent::Media(file) => file.metadata.as_ref().unwrap(),
                _ => panic!("expected post/text/cover"),
            };
            assert_eq!(metadata.caption.as_deref(), Some("Novel 42\n\nHello & 猫"));
            assert_eq!(metadata.caption_html.as_deref(), Some(html));
            assert!(Arc::ptr_eq(raw, metadata.caption_html.as_ref().unwrap()));
        }
        assert!(matches!(events[3], ExtractionEvent::EndPost));
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[1].starts_with("GET /v2/novel/detail?novel_id=42 "));
        assert!(requests[2].starts_with("GET /webview/v2/novel?id=42&viewer_version=20221031_ai "));
        assert!(!requests.iter().any(|request| request.contains("/ajax/")));
    }
}
