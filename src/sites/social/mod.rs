//! Large social networks: short-video, microblog and chat platforms.
use crate::sites::Site;

mod weibo;

pub(super) static SITES: &[&Site] = &[&weibo::SITE];
