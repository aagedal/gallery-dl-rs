//! Large social networks: short-video, microblog and chat platforms.
use crate::sites::Site;

mod bilibili;
mod vk;

pub(super) static SITES: &[&Site] = &[&vk::SITE, &bilibili::SITE];
