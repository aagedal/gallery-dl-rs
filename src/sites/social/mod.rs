//! Large social networks: short-video, microblog and chat platforms.
use crate::sites::Site;

mod bilibili;
mod discord;
mod kuaishou;
mod linkedin;
mod pillowfort;
mod plurk;
mod shortlink;
mod snapchat;
mod telegram;
mod threads;
mod tiktok;
mod vk;
mod weibo;
mod xiaohongshu;
mod youtube_posts;

pub(super) static SITES: &[&Site] = &[
    &weibo::SITE,
    &tiktok::SITE,
    &discord::SITE,
    &vk::SITE,
    &bilibili::SITE,
    &telegram::SITE,
    &kuaishou::SITE,
    &snapchat::SITE,
    &xiaohongshu::SITE,
    &threads::SITE,
    &youtube_posts::SITE,
    &shortlink::SITE,
    &linkedin::SITE,
    &plurk::SITE,
    &pillowfort::SITE,
];

pub(super) static SHORTLINK_COOKIE_DOMAINS: &[&str] = shortlink::COOKIE_DOMAINS;
