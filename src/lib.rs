pub mod archive;
mod backend;
pub mod browser;
pub mod config;
pub mod cookies;
pub mod date;
pub mod download;
pub mod extract;
pub mod facebook;
mod facebook_album;
mod facebook_dash;
mod facebook_video;
pub mod file_metadata;
pub mod filter;
pub mod input;
pub mod instagram;
mod messages;
pub mod naming;
pub mod network;
mod range;
mod resume;
pub mod selection;
pub mod size;
mod transport;
pub mod twitter;

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;

pub fn error(message: impl Into<String>) -> Error {
    std::io::Error::other(message.into()).into()
}
