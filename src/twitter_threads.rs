//! Child detail traversal state for per-conversation expansion.
use super::*;
use std::collections::{BTreeMap, HashSet, VecDeque};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThreadOptions {
    pub expand: bool,
    pub unique: bool,
}
impl Default for ThreadOptions {
    fn default() -> Self {
        Self {
            expand: false,
            unique: true,
        }
    }
}

pub(super) struct Frame {
    pub(super) id: String,
    pub(super) original: (String, Value),
    pub(super) cursor: Option<String>,
    pub(super) cursors: HashSet<[u8; 32]>,
    pub(super) posts: VecDeque<(String, Value)>,
    pub(super) known: BTreeMap<String, Value>,
    pub(super) pinned: bool,
    pub(super) done: bool,
}
impl Frame {
    pub(super) fn new(id: String, original: (String, Value), pinned: bool) -> Self {
        Self {
            id,
            original,
            cursor: None,
            cursors: HashSet::new(),
            posts: VecDeque::new(),
            known: BTreeMap::new(),
            pinned,
            done: false,
        }
    }
}
