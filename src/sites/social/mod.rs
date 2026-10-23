//! Large social networks: short-video, microblog and chat platforms.
use crate::sites::Site;

mod discord;
mod tiktok;
mod weibo;

pub(super) static SITES: &[&Site] = &[&weibo::SITE, &tiktok::SITE, &discord::SITE];
