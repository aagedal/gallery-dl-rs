//! Requested-post selection and conversation buffering for TweetDetail.
use super::*;
use std::collections::{BTreeMap, VecDeque};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TweetEndpoint {
    #[default]
    Auto,
    Detail,
    RestId,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ConversationMode {
    #[default]
    Off,
    All,
    Accessible,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StatusOptions {
    pub endpoint: TweetEndpoint,
    pub conversations: ConversationMode,
}

pub(super) struct Selection {
    pub(super) id: String,
    mode: ConversationMode,
    found: bool,
    prefix: VecDeque<(String, Value)>,
    prefix_bytes: usize,
    pub(super) known: BTreeMap<String, Value>,
}
pub(super) struct Selected {
    pub(super) posts: Vec<(String, Value)>,
    pub(super) focal: Option<Value>,
    pub(super) done: bool,
}
impl Selection {
    pub(super) fn new(id: String, mode: ConversationMode) -> Self {
        Self {
            id,
            mode,
            found: false,
            prefix: VecDeque::new(),
            prefix_bytes: 0,
            known: BTreeMap::new(),
        }
    }
    pub(super) fn select(&mut self, posts: Vec<(String, Value)>, done: bool) -> Result<Selected> {
        self.known.clear();
        for (id, value) in &posts {
            self.known
                .entry(id.clone())
                .or_insert_with(|| value.clone());
        }
        let focal = if !self.found {
            posts
                .iter()
                .find(|(id, _)| id == &self.id)
                .map(|(_, value)| value.clone())
        } else {
            None
        };
        self.found |= focal.is_some();
        if self.mode == ConversationMode::Off {
            if let Some(value) = &focal {
                return Ok(Selected {
                    posts: vec![(self.id.clone(), value.clone())],
                    focal,
                    done: true,
                });
            }
            if done {
                return Err(error(
                    "Twitter requested post unavailable in conversation response",
                ));
            }
            return Ok(Selected {
                posts: Vec::new(),
                focal: None,
                done: false,
            });
        }
        if self.found {
            let mut selected: Vec<_> = self.prefix.drain(..).collect();
            selected.extend(posts);
            self.known.clear();
            for (id, value) in &selected {
                self.known
                    .entry(id.clone())
                    .or_insert_with(|| value.clone());
            }
            self.prefix_bytes = 0;
            return Ok(Selected {
                posts: selected,
                focal,
                done,
            });
        }
        for post in posts {
            let length = serde_json::to_vec(&post.1)
                .map_err(|_| error("Twitter conversation context invalid"))?
                .len();
            self.prefix_bytes = self
                .prefix_bytes
                .checked_add(length)
                .filter(|n| *n <= MAX_RESPONSE)
                .ok_or_else(|| error("Twitter conversation context buffer exceeded 16 MiB"))?;
            self.prefix.push_back(post);
        }
        let selected = if done && self.mode == ConversationMode::All {
            let posts: Vec<_> = self.prefix.drain(..).collect();
            self.known.clear();
            for (id, value) in &posts {
                self.known
                    .entry(id.clone())
                    .or_insert_with(|| value.clone());
            }
            posts
        } else {
            Vec::new()
        };
        if done {
            self.prefix.clear();
            self.prefix_bytes = 0;
        }
        Ok(Selected {
            posts: selected,
            focal: None,
            done,
        })
    }
}

pub(super) fn endpoint_at(base: &Url, id: &str, cursor: Option<&str>) -> Result<Url> {
    if !valid_id(id) {
        return Err(error("invalid Twitter post identifier"));
    }
    let mut endpoint = base.join("iFEr5AcP121Og4wx9Yqo3w/TweetDetail")?;
    let mut variables = serde_json::json!({"focalTweetId":id,"referrer":"profile","with_rux_injections":false,
        "includePromotedContent":false,"withCommunity":true,"withQuickPromoteEligibilityTweetFields":false,
        "withBirdwatchNotes":true,"withVoice":true});
    if let Some(cursor) = cursor {
        variables["cursor"] = cursor.into();
    }
    let mut features = tweet_features();
    features["rweb_video_screen_enabled"] = false.into();
    endpoint
        .query_pairs_mut()
        .append_pair("variables", &variables.to_string())
        .append_pair("features", &features.to_string())
        .append_pair(
            "fieldToggles",
            &serde_json::json!({"withArticleRichContentState":true,"withArticlePlainText":false,
            "withGrokAnalyze":false,"withDisallowedReplyControls":false})
            .to_string(),
        );
    Ok(endpoint)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn post(id: &str) -> (String, Value) {
        (
            id.into(),
            json!({"__typename":"Tweet","rest_id":id,"legacy":{"id_str":id,"user_id_str":"42"}}),
        )
    }

    #[test]
    fn inaccessible_focal_modes_never_substitute_context_for_a_requested_post() {
        for mode in [
            ConversationMode::Off,
            ConversationMode::All,
            ConversationMode::Accessible,
        ] {
            let mut selection = Selection::new("100".into(), mode);
            let first = selection.select(vec![post("99")], false).unwrap();
            assert!(first.posts.is_empty());
            let last = selection.select(vec![post("101")], true);
            match mode {
                ConversationMode::Off => assert!(last.is_err()),
                ConversationMode::All => assert_eq!(
                    last.unwrap()
                        .posts
                        .iter()
                        .map(|(id, _)| id.as_str())
                        .collect::<Vec<_>>(),
                    ["99", "101"]
                ),
                ConversationMode::Accessible => assert!(last.unwrap().posts.is_empty()),
            }
        }
    }

    #[test]
    fn accessible_focal_releases_ordered_prefix_and_keeps_first_duplicate_for_quotes() {
        let mut selection = Selection::new("100".into(), ConversationMode::Accessible);
        let mut first = post("99");
        first.1["legacy"]["full_text"] = "first".into();
        assert!(
            selection
                .select(vec![first.clone()], false)
                .unwrap()
                .posts
                .is_empty()
        );
        let mut later = post("99");
        later.1["legacy"]["full_text"] = "later".into();
        let selected = selection
            .select(vec![later, post("100"), post("101")], false)
            .unwrap();
        assert_eq!(
            selected
                .posts
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            ["99", "99", "100", "101"]
        );
        assert_eq!(selection.known["99"], first.1);
        assert!(selected.focal.is_some());
        assert!(!selected.done);
        assert_eq!(
            selection.select(vec![post("102")], true).unwrap().posts[0].0,
            "102"
        );
    }

    #[test]
    fn cumulative_prefocal_buffer_has_a_private_size_failure() {
        let mut selection = Selection::new("100".into(), ConversationMode::Accessible);
        let mut first = post("98");
        first.1["padding"] = "PRIVATE".repeat(MAX_RESPONSE / 14).into();
        assert!(
            selection
                .select(vec![first.clone()], false)
                .unwrap()
                .posts
                .is_empty()
        );
        let mut second = first.clone();
        second.0 = "99".into();
        second.1["rest_id"] = "99".into();
        let message = selection
            .select(vec![second], false)
            .err()
            .unwrap()
            .to_string();
        assert!(message.contains("16 MiB"));
        assert!(!message.contains("PRIVATE"));
        // Finding the focal releases the prefix instead of retaining another page.
        let mut selection = Selection::new("100".into(), ConversationMode::Accessible);
        selection.select(vec![first], false).unwrap();
        assert_eq!(
            selection
                .select(vec![post("100")], true)
                .unwrap()
                .posts
                .len(),
            2
        );
    }
}
