//! Large social networks: short-video, microblog and chat platforms.
use crate::sites::Site;

mod arcalive;
mod bilibili;
mod blogger;
mod discord;
mod kuaishou;
mod linkedin;
mod naverchzzk;
mod pholder;
mod plurk;
mod scrolller;
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
    &blogger::SITE,
    &plurk::SITE,
    &naverchzzk::SITE,
    &pholder::SITE,
    &scrolller::SITE,
    &arcalive::SITE,
];

pub(super) static SHORTLINK_COOKIE_DOMAINS: &[&str] = shortlink::COOKIE_DOMAINS;
