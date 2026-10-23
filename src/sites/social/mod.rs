//! Large social networks: short-video, microblog and chat platforms.
use crate::sites::Site;

mod tiktok;

pub(super) static SITES: &[&Site] = &[&tiktok::SITE];
