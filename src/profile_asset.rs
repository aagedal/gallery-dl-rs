//! Stable public identity for profile images that are not ordinary posts.
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProfileAssetKind {
    Avatar,
    Background,
}

impl ProfileAssetKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Avatar => "avatar",
            Self::Background => "background",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ProfileAssetMetadata {
    pub kind: ProfileAssetKind,
    pub owner_id: String,
    pub version: String,
}

impl ProfileAssetMetadata {
    pub fn valid_version(&self) -> bool {
        (1..=128).contains(&self.version.len())
            && self
                .version
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    }
}
