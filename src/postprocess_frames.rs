//! Bounded readiness tracking for downloaded original ugoira frame groups.
use super::{FrameInput, SourceFrame};
use crate::{Result, error, extract::Media, pixiv::UgoiraFrame};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
const MAX_GROUPS: usize = 256;
const MAX_FRAMES: usize = 100_000;
type Key = (String, PathBuf);
#[derive(Default)]
pub struct FrameCollector {
    state: Mutex<State>,
}
#[derive(Default, Debug)]
struct State {
    groups: BTreeMap<Key, Group>,
    sequence: u64,
}
#[derive(Debug)]
struct Group {
    frames: Arc<[UgoiraFrame]>,
    ready: BTreeMap<usize, SourceFrame>,
    touched: u64,
    ready_bytes: usize,
    frame_bytes: usize,
    final_media: Option<Media>,
}
pub struct Collection {
    pub input: FrameInput,
    pub media: Media,
}

impl std::fmt::Debug for FrameCollector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FrameCollector { state: [redacted] }")
    }
}
impl std::fmt::Debug for Collection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Collection { input: [redacted], media: [redacted] }")
    }
}

fn descriptor(media: &Media, path: &Path) -> Result<(Key, usize, Arc<[UgoiraFrame]>)> {
    let metadata = media
        .metadata
        .as_ref()
        .ok_or_else(|| error("missing ugoira frame metadata"))?;
    let index = metadata
        .ugoira_frame_index
        .ok_or_else(|| error("missing ugoira frame index"))?;
    let frames = metadata
        .frames
        .clone()
        .filter(|f| !f.is_empty() && f.len() <= MAX_FRAMES && index < f.len())
        .ok_or_else(|| error("invalid ugoira frame group"))?;
    if metadata.ugoira_original != Some(true) {
        return Err(error("invalid original ugoira frame"));
    }
    let mut identity = media.clone();
    let post = identity.metadata.as_mut().expect("checked metadata");
    post.ugoira_frame_index = Some(0);
    post.media_id = format!("{}_p0", post.post_id);
    post.index = 1;
    identity.filename = "animation.frame".into();
    let key = identity
        .archive_key()
        .ok_or_else(|| error("invalid ugoira frame archive identity"))?;
    Ok((
        (key, path.parent().unwrap_or(Path::new(".")).to_owned()),
        index,
        frames,
    ))
}
fn regular(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file())
}
fn source(media: &Media, path: &Path, frame: &UgoiraFrame) -> Result<SourceFrame> {
    let extension = Path::new(&media.filename)
        .extension()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty() && s.len() <= 16 && s.bytes().all(|b| b.is_ascii_alphanumeric()))
        .ok_or_else(|| error("invalid original ugoira frame extension"))?;
    Ok(SourceFrame {
        frame: frame.clone(),
        path: path.to_owned(),
        extension: extension.to_owned(),
    })
}
impl FrameCollector {
    pub fn register(&self, media: &Media, path: &Path) -> Result<()> {
        let (key, index, frames) = descriptor(media, path)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| error("ugoira frame collector failed"))?;
        state.sequence = state.sequence.wrapping_add(1);
        let touched = state.sequence;
        let frame_bytes = if let Some(group) = state.groups.get(&key) {
            group.frame_bytes
        } else {
            frames
                .iter()
                .try_fold(0usize, |total, frame| total.checked_add(frame.file.len()))
                .filter(|total| *total <= 32 * 1024 * 1024)
                .ok_or_else(|| error("ugoira frame names exceed the size limit"))?
        };
        if !state.groups.contains_key(&key) {
            while state.groups.len() >= MAX_GROUPS
                || state.groups.values().map(|g| g.frames.len()).sum::<usize>() + frames.len()
                    > MAX_FRAMES
                || state.groups.values().map(|g| g.frame_bytes).sum::<usize>() + frame_bytes
                    > 32 * 1024 * 1024
            {
                let Some(oldest) = state
                    .groups
                    .iter()
                    .min_by_key(|(_, g)| g.touched)
                    .map(|(k, _)| k.clone())
                else {
                    return Err(error("ugoira frame groups exceed the size limit"));
                };
                state.groups.remove(&oldest);
            }
        }
        let previous_bytes = state
            .groups
            .get(&key)
            .and_then(|g| g.ready.get(&index))
            .map_or(0, |frame| frame.path.as_os_str().as_encoded_bytes().len());
        let total_bytes: usize = state.groups.values().map(|g| g.ready_bytes).sum();
        let incoming_bytes = if regular(path) {
            path.as_os_str().as_encoded_bytes().len()
        } else {
            0
        };
        if incoming_bytes > (32 * 1024 * 1024usize).saturating_sub(total_bytes - previous_bytes) {
            return Err(error("ugoira frame paths exceed the size limit"));
        }
        let group = state.groups.entry(key).or_insert_with(|| Group {
            frames: frames.clone(),
            ready: BTreeMap::new(),
            touched,
            ready_bytes: 0,
            frame_bytes,
            final_media: None,
        });
        if group.frames.len() != frames.len() || group.frames[index] != frames[index] {
            return Err(error("inconsistent ugoira frame group"));
        }
        group.touched = touched;
        if let Some(previous) = group.ready.remove(&index) {
            group.ready_bytes -= previous.path.as_os_str().as_encoded_bytes().len();
        }
        if regular(path) {
            let bytes = path.as_os_str().as_encoded_bytes().len();
            if bytes > 32 * 1024 * 1024 - group.ready_bytes {
                return Err(error("ugoira frame paths exceed the size limit"));
            }
            group
                .ready
                .insert(index, source(media, path, &frames[index])?);
            group.ready_bytes += bytes;
        }
        Ok(())
    }
    pub fn complete(
        &self,
        media: &Media,
        path: &Path,
        newly_downloaded: bool,
    ) -> Result<Option<Collection>> {
        self.register(media, path)?;
        let (key, index, frames) = descriptor(media, path)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| error("ugoira frame collector failed"))?;
        let group = state
            .groups
            .get_mut(&key)
            .ok_or_else(|| error("missing ugoira frame group"))?;
        if index + 1 == frames.len() && newly_downloaded {
            group.final_media = Some(media.clone());
        }
        if frames.len() <= 1 {
            state.groups.remove(&key);
            return Ok(None);
        }
        if group.final_media.is_none()
            || group.ready.len() != frames.len()
            || group.ready.values().any(|frame| !regular(&frame.path))
        {
            return Ok(None);
        }
        let mut group = state.groups.remove(&key).expect("checked group");
        let mut output = group.final_media.take().expect("checked final frame");
        let metadata = output.metadata.as_mut().expect("checked metadata");
        metadata.media_id = format!("{}_p0", metadata.post_id);
        metadata.index = 1;
        let extension = &group.ready[&0].extension;
        output.filename = format!("{}_p0.{extension}", metadata.post_id);
        Ok(Some(Collection {
            input: FrameInput::Files(group.ready.into_values().collect()),
            media: output,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::{MediaJob, PostMetadata};
    fn media(index: usize, count: usize) -> Media {
        Media {
            url: url::Url::parse(&format!("https://i.pximg.net/42_ugoira{index}.jpg")).unwrap(),
            job: MediaJob::DirectHttp,
            category: "pixiv".into(),
            filename: format!("42_p{index}.jpg"),
            metadata: Some(PostMetadata {
                post_id: "42".into(),
                media_id: format!("42_p{index}"),
                index: index + 1,
                frames: Some(
                    (0..count)
                        .map(|n| UgoiraFrame {
                            file: format!("{n:06}.jpg"),
                            delay: 100,
                        })
                        .collect::<Vec<_>>()
                        .into(),
                ),
                ugoira_original: Some(true),
                ugoira_frame_index: Some(index),
                ..Default::default()
            }),
            headers: Default::default(),
            handoff: Default::default(),
        }
    }
    #[test]
    fn all_frames_ready_and_new_last_produce_ordered_group_with_source_final_index() {
        let directory = tempfile::tempdir().unwrap();
        let collector = FrameCollector::default();
        for index in [1, 0] {
            let path = directory.path().join(format!("42_p{index}.jpg"));
            std::fs::write(&path, b"frame").unwrap();
            assert!(
                collector
                    .complete(&media(index, 3), &path, true)
                    .unwrap()
                    .is_none()
            );
        }
        let last = directory.path().join("42_p2.jpg");
        std::fs::write(&last, b"last").unwrap();
        let collection = collector
            .complete(&media(2, 3), &last, true)
            .unwrap()
            .unwrap();
        let FrameInput::Files(files) = collection.input else {
            panic!()
        };
        assert_eq!(
            files
                .iter()
                .map(|f| f.frame.file.as_str())
                .collect::<Vec<_>>(),
            ["000000.jpg", "000001.jpg", "000002.jpg"]
        );
        assert_eq!(collection.media.filename, "42_p0.jpg");
        let metadata = collection.media.metadata.unwrap();
        assert_eq!(metadata.media_id, "42_p0");
        assert_eq!(metadata.index, 1);
        assert_eq!(metadata.ugoira_frame_index, Some(2));
        assert!(collector.state.lock().unwrap().groups.is_empty());
    }
    #[test]
    fn final_download_finishing_first_is_claimed_once_when_earlier_frames_finish() {
        let directory = tempfile::tempdir().unwrap();
        let collector = FrameCollector::default();
        let last = directory.path().join("last.jpg");
        std::fs::write(&last, b"frame").unwrap();
        assert!(
            collector
                .complete(&media(2, 3), &last, true)
                .unwrap()
                .is_none()
        );
        let first = directory.path().join("first.jpg");
        std::fs::write(&first, b"frame").unwrap();
        assert!(
            collector
                .complete(&media(0, 3), &first, true)
                .unwrap()
                .is_none()
        );
        let middle = directory.path().join("middle.jpg");
        std::fs::write(&middle, b"frame").unwrap();
        let collection = collector
            .complete(&media(1, 3), &middle, true)
            .unwrap()
            .unwrap();
        assert_eq!(
            collection.media.metadata.unwrap().ugoira_frame_index,
            Some(2)
        );
        assert!(
            collector
                .complete(&media(1, 3), &middle, false)
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn archive_hits_existing_last_single_frames_and_missing_previous_never_convert() {
        let directory = tempfile::tempdir().unwrap();
        for (count, new_last, first_exists) in [(2, false, true), (1, true, true), (2, true, false)]
        {
            let collector = FrameCollector::default();
            let first = directory.path().join("first.jpg");
            if first_exists {
                std::fs::write(&first, b"frame").unwrap();
            } else {
                let _ = std::fs::remove_file(&first);
            }
            collector.register(&media(0, count), &first).unwrap();
            let last = directory.path().join("last.jpg");
            std::fs::write(&last, b"frame").unwrap();
            assert!(
                collector
                    .complete(&media(count - 1, count), &last, new_last)
                    .unwrap()
                    .is_none()
            );
        }
    }
    #[test]
    fn scope_parent_directory_and_deleted_inputs_prevent_cross_group_conversion() {
        let directory = tempfile::tempdir().unwrap();
        let a = directory.path().join("a");
        let b = directory.path().join("b");
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();
        let collector = FrameCollector::default();
        let first = a.join("first.jpg");
        std::fs::write(&first, b"frame").unwrap();
        collector.register(&media(0, 2), &first).unwrap();
        let last = b.join("last.jpg");
        std::fs::write(&last, b"frame").unwrap();
        assert!(
            collector
                .complete(&media(1, 2), &last, true)
                .unwrap()
                .is_none()
        );
        std::fs::remove_file(&first).unwrap();
        let last = a.join("last.jpg");
        std::fs::write(&last, b"frame").unwrap();
        assert!(
            collector
                .complete(&media(1, 2), &last, true)
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn aggregate_frame_budget_evicts_incomplete_groups_and_path_budget_is_global() {
        let directory = tempfile::tempdir().unwrap();
        let collector = FrameCollector::default();
        let many = media(0, 60_000);
        collector
            .register(&many, &directory.path().join("a/frame"))
            .unwrap();
        collector
            .register(&many, &directory.path().join("b/frame"))
            .unwrap();
        assert_eq!(collector.state.lock().unwrap().groups.len(), 1);
        collector
            .state
            .lock()
            .unwrap()
            .groups
            .values_mut()
            .next()
            .unwrap()
            .ready_bytes = 32 * 1024 * 1024;
        let target = directory.path().join("frame.jpg");
        std::fs::write(&target, b"frame").unwrap();
        assert!(collector.register(&media(0, 2), &target).is_err());
    }
    #[test]
    fn debug_redacts_download_paths_and_signed_media_urls() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("PRIVATE_PATH.jpg");
        std::fs::write(&path, b"frame").unwrap();
        let mut last = media(1, 2);
        last.url.set_query(Some("token=PRIVATE_TOKEN"));
        let collector = FrameCollector::default();
        assert!(collector.complete(&last, &path, true).unwrap().is_none());
        let debug = format!("{collector:?}");
        assert!(!debug.contains("PRIVATE"));
        let collection = Collection {
            input: FrameInput::Files(Vec::new()),
            media: last,
        };
        assert!(!format!("{collection:?}").contains("PRIVATE"));
    }
    #[test]
    fn malformed_identity_or_index_fails_and_idle_groups_are_evicted() {
        let directory = tempfile::tempdir().unwrap();
        let collector = FrameCollector::default();
        let mut invalid = media(0, 2);
        invalid.metadata.as_mut().unwrap().ugoira_frame_index = Some(2);
        assert!(
            collector
                .register(&invalid, &directory.path().join("frame"))
                .is_err()
        );
        for index in 0..MAX_GROUPS + 1 {
            collector
                .register(
                    &media(0, 2),
                    &directory.path().join(index.to_string()).join("frame"),
                )
                .unwrap();
        }
        let state = collector.state.lock().unwrap();
        assert_eq!(state.groups.len(), MAX_GROUPS);
        assert!(
            state
                .groups
                .keys()
                .all(|(_, p)| p != &directory.path().join("0"))
        );
    }
}
