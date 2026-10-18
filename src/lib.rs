pub mod archive;
pub mod config;
pub mod cookies;
pub mod download;
pub mod extract;
pub mod instagram;
pub mod naming;
mod range;
mod resume;
mod transport;

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;

pub fn error(message: impl Into<String>) -> Error {
    std::io::Error::other(message.into()).into()
}
