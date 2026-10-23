//! Large social networks: short-video, microblog and chat platforms.
use crate::sites::Site;

mod bilibili;
mod discord;
mod kuaishou;
mod snapchat;
mod telegram;
mod tiktok;
mod vk;
mod weibo;

pub(super) static SITES: &[&Site] = &[
    &weibo::SITE,
    &tiktok::SITE,
    &discord::SITE,
    &vk::SITE,
    &bilibili::SITE,
    &telegram::SITE,
    &kuaishou::SITE,
    &snapchat::SITE,
];
