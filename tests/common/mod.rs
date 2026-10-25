//! Shared helpers for tests that run the binary.
use std::process::Command;

/// The binary with its default credential cache moved to a private
/// temporary directory, so tests never read or write the developer's real
/// `cache.sqlite3`.
pub fn binary_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_gallery-dl-rs"));
    let cache =
        std::env::temp_dir().join(format!("gallery-dl-rs-test-cache-{}", std::process::id()));
    command.env("XDG_CACHE_HOME", &cache).env("APPDATA", &cache);
    command
}
