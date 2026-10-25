pub mod archive;
pub mod artstation;
mod backend;
pub mod bluesky;
pub mod booru;
pub mod browser;
pub mod cache;
pub mod config;
pub mod cookies;
pub mod date;
pub mod deviantart;
pub mod dispatch;
pub mod download;
pub mod event;
pub mod exec;
pub mod extract;
pub mod facebook;
mod facebook_album;
mod facebook_dash;
mod facebook_profile;
mod facebook_video;
pub mod file_metadata;
pub mod filter;
pub mod flickr;
pub mod furaffinity;
pub mod imagehosts;
pub mod imgur;
pub mod input;
pub mod instagram;
pub mod job_selection;
pub mod kemono;
pub mod mastodon;
mod messages;
pub mod naming;
pub mod network;
pub mod newgrounds;
pub mod oauth;
pub mod pinterest;
pub mod pixiv;
pub mod post_selection;
pub mod postprocess;
pub mod profile_asset;
mod python_unicode;
mod range;
pub mod reddit;
mod reddit_rss;
mod resume;
pub mod selection;
pub mod sites;
pub mod size;
pub mod sleep;
mod source_date;
mod source_date_json;
mod transport;
pub mod tumblr;
pub mod twitter;
pub mod vsco;
pub mod wallhaven;
pub mod weasyl;

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;

pub fn error(message: impl Into<String>) -> Error {
    std::io::Error::other(message.into()).into()
}

pub mod source_archive;
