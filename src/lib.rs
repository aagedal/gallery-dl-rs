pub mod actions;
pub mod archive;
pub mod artstation;
mod backend;
pub mod bluesky;
pub mod booru;
pub mod browser;
mod browser_chromium;
pub mod config;
pub mod cookies;
pub mod date;
pub mod deviantart;
pub mod dispatch;
pub mod download;
mod durable;
pub mod event;
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
pub mod geo;
pub mod imagehosts;
pub mod imgur;
pub mod input;
pub mod instagram;
pub mod job_selection;
pub mod kemono;
pub mod logfile;
pub mod mastodon;
mod messages;
pub mod naming;
pub mod netrc;
pub mod network;
pub mod newgrounds;
pub mod oauth;
pub mod output;
pub mod path_options;
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
pub mod signature;
pub mod sites;
pub mod size;
pub mod sleep;
mod source_date;
mod source_date_json;
pub mod tag_filter;
mod transport;
pub mod tumblr;
pub mod twitter;
pub mod unsupported;
pub mod vsco;
pub mod wallhaven;
pub mod weasyl;

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;

/// Upstream `StopExtraction`/`TerminateExtraction`/`SystemExit` raised by
/// postprocessors and actions: stop the current input, all inputs, or exit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    Abort,
    Terminate,
    Exit(i32),
}

impl std::fmt::Display for Stop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Abort => f.write_str("extraction stopped"),
            Self::Terminate => f.write_str("extraction terminated"),
            Self::Exit(code) => write!(f, "exit requested ({code})"),
        }
    }
}

impl std::error::Error for Stop {}

impl Stop {
    /// The stop request carried by an error, if any.
    pub fn of(error: &Error) -> Option<Self> {
        error.downcast_ref::<Self>().copied()
    }
}

pub fn error(message: impl Into<String>) -> Error {
    std::io::Error::other(message.into()).into()
}

pub mod source_archive;
