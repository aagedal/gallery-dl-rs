//! Large social networks: short-video, microblog and chat platforms.
use crate::sites::Site;
use serde_json::{Map, Value};
use url::Url;

mod arcalive;
mod bilibili;
mod blogger;
mod discord;
mod joyreactor;
mod kuaishou;
mod linkedin;
mod naverchzzk;
mod nitter;
mod pholder;
mod plurk;
mod postmill;
mod reactor;
mod scrolller;
mod shortlink;
mod snapchat;
mod telegram;
mod threads;
mod tiktok;
mod urlshortener;
mod vk;
mod weibo;
pub(super) mod xenforo;
mod xiaohongshu;
mod youtube_posts;

pub(super) static SITES: &[&Site] = &[
    &urlshortener::BITLY,
    &urlshortener::TCO,
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
    &reactor::REACTOR,
    &reactor::PORNREACTOR,
    &reactor::THATPERVERT,
    &joyreactor::SITE,
    &nitter::NITTER_NET,
    &nitter::NITTER_SPACE,
    &nitter::NITTER_TIEKOETTER,
    &nitter::XCANCEL,
    &nitter::LIGHTBRD,
    &xenforo::SIMPCITY,
    &xenforo::NUDOSTARFORUM,
    &xenforo::ATFFORUM,
    &xenforo::CELEBFORUM,
    &xenforo::TITSINTOPS,
    &xenforo::SOCIALMEDIAGIRLSFORUM,
    &xenforo::BLACKTOWHITE,
    &postmill::RADDLE_SITE,
];

pub(super) static SHORTLINK_COOKIE_DOMAINS: &[&str] = shortlink::COOKIE_DOMAINS;

/// An upstream `ytdl:` URL: the page is handed to yt-dlp, and the file's
/// URL keeps the `ytdl:` prefix.
pub(crate) fn ytdl_item(page: &str, keywords: Map<String, Value>) -> Option<crate::sites::Item> {
    let page_url = crate::sites::media::web_url(page)?;
    let url = Url::parse(&format!("ytdl:{page_url}")).ok()?;
    Some(
        crate::sites::Item::new(url, keywords)
            .job(crate::extract::MediaJob::VideoPage { page_url }),
    )
}

/// Families with configurable instances (`crate::sites::instances`).
pub(super) static FAMILIES: &[&crate::sites::instances::Family] = &[
    &urlshortener::FAMILY,
    &nitter::FAMILY,
    &postmill::FAMILY,
    &blogger::FAMILY,
    &reactor::FAMILY,
    &xenforo::FAMILY,
];
