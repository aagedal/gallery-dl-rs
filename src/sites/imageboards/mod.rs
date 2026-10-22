//! Imageboards and their archives.
use crate::sites::Site;

mod fourchan;

pub(super) static SITES: &[&Site] = &[&fourchan::SITE];
