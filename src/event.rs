//! Explicit extractor post boundaries, kept separate from file identities.
use crate::{
    Result,
    extract::{Media, PostMetadata},
};
use futures_util::{StreamExt, stream::BoxStream};
use std::collections::VecDeque;

#[derive(Clone, Debug)]
pub struct Post {
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
            metadata.media_id.clear();
            metadata.index = 0;
        }
        Self {
            category: category.into(),
            metadata,
            count: Some(media.len()),
        }
    }
}
#[derive(Clone, Debug)]
pub enum ExtractionEvent {
    Post(Box<Post>),
    Media(Box<Media>),
    EndPost,
}
pub fn group(media: Vec<Media>, category: &str) -> VecDeque<ExtractionEvent> {
    let mut events = VecDeque::with_capacity(media.len() + 2);
    events.push_back(ExtractionEvent::Post(Box::new(Post::from_media(
        &media, category,
    ))));
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
