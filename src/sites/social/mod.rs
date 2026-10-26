//! Large social networks: short-video, microblog and chat platforms.
use crate::sites::Site;

mod arcalive;
mod bilibili;
mod blogger;
mod discord;
mod joyreactor;
mod kuaishou;
mod linkedin;
mod nitter;
mod pholder;
mod pillowfort;
mod plurk;
mod postmill;
mod reactor;
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
    &plurk::SITE,
    &pillowfort::SITE,
    &pholder::SITE,
    &postmill::RADDLE,
    &postmill::POSTMILL,
    &arcalive::SITE,
    &blogger::BLOGSPOT,
    &blogger::BLOGGER,
    &scrolller::SITE,
    &reactor::REACTOR,
    &reactor::PORNREACTOR,
    &reactor::THATPERVERT,
    &joyreactor::SITE,
    &nitter::NITTER_NET,
    &nitter::NITTER_SPACE,
    &nitter::NITTER_TIEKOETTER,
    &nitter::XCANCEL,
    &nitter::LIGHTBRD,
    &nitter::NITTER,
];

pub(super) static SHORTLINK_COOKIE_DOMAINS: &[&str] = shortlink::COOKIE_DOMAINS;
