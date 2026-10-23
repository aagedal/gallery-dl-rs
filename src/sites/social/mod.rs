//! Large social networks: short-video, microblog and chat platforms.
use crate::sites::Site;

mod discord;

pub(super) static SITES: &[&Site] = &[&discord::SITE];
