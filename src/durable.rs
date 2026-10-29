//! Durable file flushes that unit tests skip.
//!
//! On macOS `File::sync_all` issues `F_FULLFSYNC`, which flushes the drive
//! cache and serializes across processes. Unit tests write thousands of
//! throwaway files, so the flush dominates their run time without testing
//! anything.

use std::{fs::File, io};

pub(crate) fn sync_all(file: &File) -> io::Result<()> {
    if cfg!(test) { Ok(()) } else { file.sync_all() }
}

pub(crate) fn sync_data(file: &File) -> io::Result<()> {
    if cfg!(test) { Ok(()) } else { file.sync_data() }
}
