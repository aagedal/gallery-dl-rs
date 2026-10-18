pub mod archive;
mod backend;
pub mod config;
pub mod cookies;
pub mod download;
pub mod extract;
pub mod facebook;
pub mod instagram;
mod messages;
pub mod naming;
mod range;
mod resume;
mod transport;
pub mod twitter;

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;

pub fn error(message: impl Into<String>) -> Error {
    std::io::Error::other(message.into()).into()
}
