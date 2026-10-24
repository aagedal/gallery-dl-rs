//! Video platforms extracted through yt-dlp. Listed last so native sites
//! sharing a host keep their own paths.
use crate::sites::Site;

mod ytdl;

pub(super) static SITES: &[&Site] = &[&ytdl::SITE];
