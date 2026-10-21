//! Bounded, cancellable ugoira ZIP/frame I/O. Call on an owned blocking worker.
//!
//! Limits: 100,000 entries/frames, 512 MiB per file, 2 GiB expanded data,
//! 64 MiB central directory, 32 MiB animation metadata, 4 KiB/64-component names.
//! Extraction requires an empty private directory; no archive path is executed.
use super::{FrameInput, SourceFrame};
use crate::{Result, error, pixiv::UgoiraFrame};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Cursor, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};
use zip::{
    CompressionMethod, DateTime, ZipArchive, ZipWriter, read::ZipReadOptions,
    write::SimpleFileOptions,
};

const MAX_ENTRIES: usize = 100_000;
const MAX_FILE: u64 = 512 * 1024 * 1024;
const MAX_TOTAL: u64 = 2 * 1024 * 1024 * 1024;
const MAX_CENTRAL: u64 = 64 * 1024 * 1024;
const MAX_ARCHIVE: u64 = MAX_TOTAL + MAX_CENTRAL;
const MAX_METADATA: usize = 32 * 1024 * 1024;
const BUFFER: usize = 64 * 1024;

fn cancelled(flag: &AtomicBool) -> Result<()> {
    if flag.load(Ordering::Acquire) {
        Err(error("ugoira processing cancelled"))
    } else {
        Ok(())
    }
}
fn invalid() -> crate::Error {
    error("invalid or unsafe ugoira ZIP")
}
fn safe_name(name: &str, directory: bool) -> Result<PathBuf> {
    if name.is_empty()
        || name.len() > 4096
        || name.starts_with('/')
        || name.contains(['\\', ':'])
        || name.chars().any(char::is_control)
    {
        return Err(invalid());
    }
    let name = if directory {
        name.strip_suffix('/').unwrap_or(name)
    } else {
        name
    };
    let parts: Vec<_> = name.split('/').collect();
    if parts.len() > 64 || parts.iter().any(|part| matches!(*part, "" | "." | "..")) {
        return Err(invalid());
    }
    Ok(parts.iter().collect())
}
fn key(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/").to_lowercase()
}
fn add_name(names: &mut BTreeMap<String, bool>, path: &Path, directory: bool) -> Result<()> {
    let name = key(path);
    if names.contains_key(&name) {
        return Err(invalid());
    }
    for parent in path
        .ancestors()
        .skip(1)
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        if names.get(&key(parent)) == Some(&false) {
            return Err(invalid());
        }
    }
    if !directory
        && names
            .range(format!("{name}/")..)
            .next()
            .is_some_and(|(next, _)| next.starts_with(&format!("{name}/")))
    {
        return Err(invalid());
    }
    names.insert(name, directory);
    Ok(())
}
fn regular(path: &Path) -> Result<fs::Metadata> {
    let metadata =
        fs::symlink_metadata(path).map_err(|_| error("cannot inspect ugoira source file"))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(error("ugoira source must be a regular file"));
    }
    Ok(metadata)
}
fn private_output(path: &Path) -> Result<File> {
    if path.exists() {
        regular(path)?;
    } else if fs::symlink_metadata(path).is_ok() {
        return Err(error("unsafe ugoira output file"));
    }
    OpenOptions::new()
        .write(true)
        .read(true)
        .create(true)
        .truncate(true)
        .open(path)
        .map_err(|_| error("cannot open ugoira output file"))
}
fn copy_bounded(
    reader: &mut impl Read,
    writer: &mut impl Write,
    maximum: u64,
    flag: &AtomicBool,
) -> Result<u64> {
    let mut buffer = [0; BUFFER];
    let mut copied = 0u64;
    loop {
        cancelled(flag)?;
        let count = reader
            .read(&mut buffer)
            .map_err(|_| error("cannot read ugoira data or validate ZIP checksum"))?;
        if count == 0 {
            return Ok(copied);
        }
        copied = copied
            .checked_add(count as u64)
            .filter(|count| *count <= maximum)
            .ok_or_else(|| error("ugoira data exceeds size limit"))?;
        writer
            .write_all(&buffer[..count])
            .map_err(|_| error("cannot write ugoira data"))?;
    }
}
fn copy_exact(
    reader: &mut impl Read,
    writer: &mut impl Write,
    bytes: u64,
    flag: &AtomicBool,
) -> Result<()> {
    if copy_bounded(&mut reader.take(bytes), writer, bytes, flag)? != bytes {
        return Err(invalid());
    }
    Ok(())
}
fn le16(bytes: &[u8], index: usize) -> u16 {
    u16::from_le_bytes(bytes[index..index + 2].try_into().unwrap())
}
fn le32(bytes: &[u8], index: usize) -> u32 {
    u32::from_le_bytes(bytes[index..index + 4].try_into().unwrap())
}
fn le64(bytes: &[u8], index: usize) -> u64 {
    u64::from_le_bytes(bytes[index..index + 8].try_into().unwrap())
}

struct Layout {
    start: u64,
    size: u64,
    entries: usize,
    base: u64,
    length: u64,
    comment: Vec<u8>,
}
/// Preflight central-directory bounds before the ZIP crate allocates its index.
fn layout(reader: &mut (impl Read + Seek), flag: &AtomicBool) -> Result<Layout> {
    cancelled(flag)?;
    let length = reader.seek(SeekFrom::End(0)).map_err(|_| invalid())?;
    if !(22..=MAX_ARCHIVE).contains(&length) {
        return Err(invalid());
    }
    let tail_length = length.min(1024 * 1024 + 65_557) as usize;
    reader
        .seek(SeekFrom::Start(length - tail_length as u64))
        .map_err(|_| invalid())?;
    let mut tail = vec![0; tail_length];
    reader.read_exact(&mut tail).map_err(|_| invalid())?;
    let at = (0..=tail.len() - 22)
        .rev()
        .find(|&at| {
            tail[at..at + 4] == [0x50, 0x4b, 0x05, 0x06]
                && at + 22 + le16(&tail, at + 20) as usize == tail.len()
        })
        .ok_or_else(invalid)?;
    if le16(&tail, at + 4) != 0 || le16(&tail, at + 6) != 0 {
        return Err(invalid());
    }
    let mut entries = le16(&tail, at + 10) as u64;
    let mut size = le32(&tail, at + 12) as u64;
    let mut relative = le32(&tail, at + 16) as u64;
    let mut end = length - tail_length as u64 + at as u64;
    if entries == u16::MAX as u64 || size == u32::MAX as u64 || relative == u32::MAX as u64 {
        if at < 20
            || tail[at - 20..at - 16] != [0x50, 0x4b, 0x06, 0x07]
            || le32(&tail, at - 16) != 0
            || le32(&tail, at - 4) != 1
        {
            return Err(invalid());
        }
        let record = (0..=at.saturating_sub(76))
            .rev()
            .find(|&index| {
                tail[index..index + 4] == [0x50, 0x4b, 0x06, 0x06]
                    && le64(&tail, index + 4).checked_add(index as u64 + 12)
                        == Some((at - 20) as u64)
            })
            .ok_or_else(invalid)?;
        if le64(&tail, record + 4) < 44
            || le32(&tail, record + 16) != 0
            || le32(&tail, record + 20) != 0
            || le64(&tail, record + 24) != le64(&tail, record + 32)
        {
            return Err(invalid());
        }
        entries = le64(&tail, record + 32);
        size = le64(&tail, record + 40);
        relative = le64(&tail, record + 48);
        end = length - tail_length as u64 + record as u64;
        let base = end
            .checked_sub(size)
            .and_then(|start| start.checked_sub(relative))
            .ok_or_else(invalid)?;
        if le64(&tail, at - 12).checked_add(base) != Some(end) {
            return Err(invalid());
        }
    } else if le16(&tail, at + 8) as u64 != entries {
        return Err(invalid());
    }
    if entries > MAX_ENTRIES as u64 || size > MAX_CENTRAL {
        return Err(error("ugoira ZIP directory exceeds limits"));
    }
    let start = end.checked_sub(size).ok_or_else(invalid)?;
    let base = start.checked_sub(relative).ok_or_else(invalid)?;
    // Count every actual record so hidden duplicate/unindexed records are rejected.
    reader.seek(SeekFrom::Start(start)).map_err(|_| invalid())?;
    for _ in 0..entries {
        cancelled(flag)?;
        let mut header = [0; 46];
        reader.read_exact(&mut header).map_err(|_| invalid())?;
        if header[..4] != [0x50, 0x4b, 0x01, 0x02]
            || le16(&header, 8) & 1 != 0
            || le16(&header, 34) != 0
            || le16(&header, 28) > 4096
        {
            return Err(invalid());
        }
        let remainder =
            le16(&header, 28) as u64 + le16(&header, 30) as u64 + le16(&header, 32) as u64;
        if reader
            .stream_position()
            .map_err(|_| invalid())?
            .checked_add(remainder)
            .is_none_or(|position| position > end)
        {
            return Err(invalid());
        }
        reader
            .seek(SeekFrom::Current(remainder as i64))
            .map_err(|_| invalid())?;
    }
    if reader.stream_position().map_err(|_| invalid())? != end {
        return Err(invalid());
    }
    Ok(Layout {
        start,
        size,
        entries: entries as usize,
        base,
        length,
        comment: tail[at + 22..].to_vec(),
    })
}

/// Also poll cancellation while the inflater consumes compressed input that
/// produces little or no output (a decompression read can otherwise run long).
struct CancelFile<'a> {
    file: File,
    flag: &'a AtomicBool,
}
impl Read for CancelFile<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.flag.load(Ordering::Acquire) {
            return Err(io::Error::other("ugoira processing cancelled"));
        }
        let length = buffer.len().min(BUFFER);
        self.file.read(&mut buffer[..length])
    }
}
impl Seek for CancelFile<'_> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        if self.flag.load(Ordering::Acquire) {
            return Err(io::Error::other("ugoira processing cancelled"));
        }
        self.file.seek(position)
    }
}
struct CheckedZip<'a> {
    archive: ZipArchive<CancelFile<'a>>,
    layout: Layout,
    names: BTreeMap<String, bool>,
    first_time: DateTime,
    total: u64,
}
fn checked_zip<'a>(
    path: &Path,
    frames: &[UgoiraFrame],
    flag: &'a AtomicBool,
) -> Result<CheckedZip<'a>> {
    if regular(path)?.len() > MAX_ARCHIVE {
        return Err(error("ugoira ZIP exceeds size limit"));
    }
    let mut file = CancelFile {
        file: File::open(path).map_err(|_| error("cannot open ugoira ZIP"))?,
        flag,
    };
    let layout = layout(&mut file, flag)?;
    let mut archive = ZipArchive::new(file).map_err(|_| invalid())?;
    if archive.len() != layout.entries || archive.offset() != layout.base {
        return Err(invalid());
    }
    cancelled(flag)?;
    if archive.has_overlapping_entries().map_err(|_| invalid())? {
        return Err(invalid());
    }
    let mut names = BTreeMap::new();
    let mut total = 0u64;
    let mut first_time = DateTime::default();
    for index in 0..archive.len() {
        cancelled(flag)?;
        let file = archive.by_index_data(index).map_err(|_| invalid())?;
        if index == 0 {
            first_time = file.last_modified().unwrap_or_default();
        }
        let directory = file.is_dir();
        let mode = file.unix_mode().unwrap_or(0) & 0o170000;
        if file.encrypted()
            || !matches!(
                file.compression(),
                CompressionMethod::Stored | CompressionMethod::Deflated
            )
            || !matches!(mode, 0 | 0o100000 | 0o040000)
            || mode == 0o040000 && !directory
            || directory && mode == 0o100000
        {
            return Err(invalid());
        }
        let name = safe_name(&file.name().map_err(|_| invalid())?, directory)?;
        add_name(&mut names, &name, directory)?;
        if file.size() > MAX_FILE || directory && file.size() != 0 {
            return Err(error("ugoira ZIP entry exceeds limits"));
        }
        total = total
            .checked_add(file.size())
            .filter(|total| *total <= MAX_TOTAL)
            .ok_or_else(|| error("ugoira ZIP expanded data exceeds limit"))?;
    }
    validate_frames(frames, flag)?;
    for frame in frames {
        if names.get(&key(&safe_name(&frame.file, false)?)) != Some(&false) {
            return Err(error("ugoira frame is missing from ZIP"));
        }
    }
    Ok(CheckedZip {
        archive,
        layout,
        names,
        first_time,
        total,
    })
}
fn validate_frames(frames: &[UgoiraFrame], flag: &AtomicBool) -> Result<()> {
    if frames.is_empty() || frames.len() > MAX_ENTRIES {
        return Err(error("ugoira frame count exceeds limits"));
    }
    let mut bytes = 0usize;
    for frame in frames {
        cancelled(flag)?;
        safe_name(&frame.file, false)?;
        bytes = bytes.saturating_add(frame.file.len()).saturating_add(32);
        if bytes > MAX_METADATA {
            return Err(error("ugoira frame metadata exceeds size limit"));
        }
    }
    Ok(())
}
fn renamed_files(frames: &[SourceFrame], flag: &AtomicBool) -> Result<Vec<UgoiraFrame>> {
    if frames.is_empty() || frames.len() > MAX_ENTRIES {
        return Err(error("ugoira frame count exceeds limits"));
    }
    let mut output = Vec::with_capacity(frames.len());
    let mut names = BTreeMap::new();
    let mut total = 0u64;
    let mut names_bytes = 0usize;
    for source in frames {
        cancelled(flag)?;
        safe_name(&source.frame.file, false)?;
        if !(1..=16).contains(&source.extension.len())
            || !source
                .extension
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric())
        {
            return Err(error("invalid ugoira frame extension"));
        }
        let name = format!(
            "{}.{}",
            source.frame.file.split('.').next().unwrap(),
            source.extension
        );
        add_name(&mut names, &safe_name(&name, false)?, false)?;
        names_bytes = names_bytes.saturating_add(name.len()).saturating_add(32);
        if names_bytes > MAX_METADATA {
            return Err(error("ugoira frame metadata exceeds size limit"));
        }
        let bytes = regular(&source.path)?.len();
        if bytes > MAX_FILE {
            return Err(error("ugoira frame exceeds size limit"));
        }
        total = total
            .checked_add(bytes)
            .filter(|total| *total <= MAX_TOTAL)
            .ok_or_else(|| error("ugoira frames exceed total size limit"))?;
        output.push(UgoiraFrame {
            file: name,
            delay: source.frame.delay,
        });
    }
    Ok(output)
}

/// Extract all safe ZIP entries, or copy/rename original frames into an empty directory.
pub(crate) fn extract(
    input: &FrameInput,
    directory: &Path,
    flag: &AtomicBool,
) -> Result<Vec<UgoiraFrame>> {
    cancelled(flag)?;
    let metadata =
        fs::symlink_metadata(directory).map_err(|_| error("missing private ugoira directory"))?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || fs::read_dir(directory)?.next().is_some()
    {
        return Err(error(
            "ugoira extraction directory must be empty and private",
        ));
    }
    match input {
        FrameInput::Zip { path, frames } => {
            let mut checked = checked_zip(path, frames, flag)?;
            for index in 0..checked.archive.len() {
                cancelled(flag)?;
                let mut file = checked
                    .archive
                    .by_index_with_options(
                        index,
                        ZipReadOptions::new().enforce_header_consistency(true),
                    )
                    .map_err(|_| invalid())?;
                let target = directory.join(safe_name(
                    &file.name().map_err(|_| invalid())?,
                    file.is_dir(),
                )?);
                if file.is_dir() {
                    fs::create_dir_all(target)?;
                } else {
                    if let Some(parent) = target.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    let mut output = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(target)
                        .map_err(|_| invalid())?;
                    let size = file.size();
                    if copy_bounded(&mut file, &mut output, size, flag)? != size {
                        return Err(invalid());
                    }
                }
            }
            Ok(frames.to_vec())
        }
        FrameInput::Files(files) => {
            let frames = renamed_files(files, flag)?;
            for (source, frame) in files.iter().zip(&frames) {
                cancelled(flag)?;
                let target = directory.join(safe_name(&frame.file, false)?);
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                let mut output = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(target)
                    .map_err(|_| invalid())?;
                let mut input =
                    File::open(&source.path).map_err(|_| error("cannot open ugoira frame"))?;
                let size = regular(&source.path)?.len();
                if copy_bounded(&mut input, &mut output, size, flag)? != size {
                    return Err(error("ugoira frame changed while copying"));
                }
            }
            Ok(frames)
        }
    }
}

fn frame_json(frames: &[UgoiraFrame], flag: &AtomicBool) -> Result<Vec<u8>> {
    let mut output = vec![b'['];
    for (index, frame) in frames.iter().enumerate() {
        cancelled(flag)?;
        // Python's source JSON encoder uses ensure_ascii=True, including UTF-16 pairs.
        let raw = serde_json::to_string(&frame.file).map_err(|_| invalid())?;
        let mut name = String::new();
        for ch in raw.chars() {
            if ch >= '\u{7f}' {
                let mut buffer = [0; 2];
                for unit in ch.encode_utf16(&mut buffer) {
                    use std::fmt::Write;
                    write!(name, "\\u{unit:04x}").unwrap();
                }
            } else {
                name.push(ch);
            }
        }
        let item = format!(
            "{}{{\"file\":{name},\"delay\":{}}}",
            if index == 0 { "" } else { "," },
            frame.delay
        );
        if output.len().saturating_add(item.len()).saturating_add(1) > MAX_METADATA {
            return Err(error("ugoira animation metadata exceeds size limit"));
        }
        output.extend_from_slice(item.as_bytes());
    }
    output.push(b']');
    Ok(output)
}
fn date(timestamp: i64) -> Result<DateTime> {
    let time = time::OffsetDateTime::from_unix_timestamp(timestamp)
        .map_err(|_| error("invalid ugoira ZIP timestamp"))?;
    DateTime::from_date_and_time(
        time.year()
            .try_into()
            .map_err(|_| error("invalid ugoira ZIP timestamp"))?,
        time.month() as u8,
        time.day(),
        time.hour(),
        time.minute(),
        time.second(),
    )
    .map_err(|_| error("ugoira ZIP timestamp must be within 1980-2107"))
}
fn options(time: DateTime) -> SimpleFileOptions {
    SimpleFileOptions::default()
        .compression_method(CompressionMethod::Stored)
        .last_modified_time(time)
}
fn metadata_collision(
    name: &str,
    frames: &[UgoiraFrame],
    names: &BTreeMap<String, bool>,
) -> Result<()> {
    let path = safe_name(name, false)?;
    let wanted = key(&path);
    if frames
        .iter()
        .any(|frame| key(&PathBuf::from(&frame.file)) == wanted)
    {
        return Err(error("ugoira metadata would overwrite a frame"));
    }
    if names.get(&wanted) == Some(&true) {
        return Err(error("ugoira metadata collides with a directory"));
    }
    for parent in path
        .ancestors()
        .skip(1)
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        if names.get(&key(parent)) == Some(&false) {
            return Err(invalid());
        }
    }
    let prefix = format!("{wanted}/");
    if names
        .range(prefix.clone()..)
        .next()
        .is_some_and(|(name, _)| name.starts_with(&prefix))
    {
        return Err(invalid());
    }
    Ok(())
}
/// Preserve every existing compressed entry byte-for-byte; append a small ZIP
/// entry and a new directory. A separate entry ZIP permits the source's deliberate
/// duplicate metadata name, which ZipWriter's normal append API rejects.
fn append_metadata(
    mut source: impl Read + Seek,
    layout: &Layout,
    output: &mut File,
    name: &str,
    bytes: &[u8],
    time: DateTime,
    flag: &AtomicBool,
) -> Result<()> {
    let mut entry = ZipWriter::new(Cursor::new(Vec::new()));
    entry
        .start_file(name, options(time))
        .map_err(|_| invalid())?;
    copy_bounded(
        &mut Cursor::new(bytes),
        &mut entry,
        MAX_METADATA as u64,
        flag,
    )?;
    let mut entry = entry.finish().map_err(|_| invalid())?;
    let entry_layout = layout_of_entry(&mut entry, flag)?;
    let raw = entry.into_inner();
    source.seek(SeekFrom::Start(0))?;
    copy_exact(&mut source, output, layout.length, flag)?;
    let local_offset = output
        .stream_position()?
        .checked_sub(layout.base)
        .ok_or_else(invalid)?;
    copy_bounded(
        &mut Cursor::new(&raw[..entry_layout.start as usize]),
        output,
        MAX_METADATA as u64 + 8192,
        flag,
    )?;
    let central_start = output.stream_position()?;
    source.seek(SeekFrom::Start(layout.start))?;
    copy_exact(&mut source, output, layout.size, flag)?;
    let mut central = raw
        [entry_layout.start as usize..(entry_layout.start + entry_layout.size) as usize]
        .to_vec();
    central[42..46].copy_from_slice(
        &u32::try_from(local_offset)
            .map_err(|_| invalid())?
            .to_le_bytes(),
    );
    output.write_all(&central)?;
    let central_size = layout.size + central.len() as u64;
    if central_size > MAX_CENTRAL {
        return Err(error("ugoira ZIP directory exceeds limits"));
    }
    let entries = layout.entries as u64 + 1;
    let relative = central_start.checked_sub(layout.base).ok_or_else(invalid)?;
    if entries >= u16::MAX as u64 {
        let zip64 = output
            .stream_position()?
            .checked_sub(layout.base)
            .ok_or_else(invalid)?;
        output.write_all(&0x06064b50u32.to_le_bytes())?;
        output.write_all(&44u64.to_le_bytes())?;
        output.write_all(&45u16.to_le_bytes())?;
        output.write_all(&45u16.to_le_bytes())?;
        output.write_all(&0u32.to_le_bytes())?;
        output.write_all(&0u32.to_le_bytes())?;
        for value in [entries, entries, central_size, relative] {
            output.write_all(&value.to_le_bytes())?;
        }
        output.write_all(&0x07064b50u32.to_le_bytes())?;
        output.write_all(&0u32.to_le_bytes())?;
        output.write_all(&zip64.to_le_bytes())?;
        output.write_all(&1u32.to_le_bytes())?;
    }
    output.write_all(&0x06054b50u32.to_le_bytes())?;
    output.write_all(&0u16.to_le_bytes())?;
    output.write_all(&0u16.to_le_bytes())?;
    for _ in 0..2 {
        output.write_all(&(entries.min(u16::MAX as u64) as u16).to_le_bytes())?;
    }
    output.write_all(
        &u32::try_from(central_size)
            .map_err(|_| invalid())?
            .to_le_bytes(),
    )?;
    output.write_all(
        &u32::try_from(relative)
            .map_err(|_| invalid())?
            .to_le_bytes(),
    )?;
    output.write_all(&(layout.comment.len() as u16).to_le_bytes())?;
    output.write_all(&layout.comment)?;
    Ok(())
}
fn layout_of_entry(entry: &mut Cursor<Vec<u8>>, flag: &AtomicBool) -> Result<Layout> {
    layout(entry, flag)
}

/// Build an archive from original files, or copy the input ZIP unchanged and
/// append source-compatible stored animation metadata. `mtime` is UTC.
pub(crate) fn archive(
    input: &FrameInput,
    output: &Path,
    metadata_name: Option<&str>,
    mtime: Option<i64>,
    flag: &AtomicBool,
) -> Result<()> {
    cancelled(flag)?;
    match input {
        FrameInput::Zip { path, frames } => {
            if fs::canonicalize(path).ok() == fs::canonicalize(output).ok() && output.exists() {
                return Err(error("ugoira output must differ from source"));
            }
            let mut checked = checked_zip(path, frames, flag)?;
            for index in 0..checked.archive.len() {
                cancelled(flag)?;
                let mut file = checked
                    .archive
                    .by_index_with_options(
                        index,
                        ZipReadOptions::new().enforce_header_consistency(true),
                    )
                    .map_err(|_| invalid())?;
                let size = file.size();
                if copy_bounded(&mut file, &mut io::sink(), size, flag)? != size {
                    return Err(invalid());
                }
            }
            if let Some(name) = metadata_name
                && checked.names.contains_key(&key(&safe_name(name, false)?))
                && !checked
                    .archive
                    .file_names()
                    .any(|old| old.is_ok_and(|old| old == name))
            {
                return Err(error(
                    "ugoira metadata name collides with an existing entry",
                ));
            }
            let metadata = metadata_name
                .map(|name| {
                    metadata_collision(name, frames, &checked.names)?;
                    frame_json(frames, flag)
                })
                .transpose()?;
            if let Some(bytes) = &metadata
                && (checked.layout.entries >= MAX_ENTRIES
                    || checked.total.saturating_add(bytes.len() as u64) > MAX_TOTAL)
            {
                return Err(error("ugoira archive metadata exceeds total limits"));
            }
            let mut output = private_output(output)?;
            if let Some(bytes) = metadata {
                let time = if mtime.is_some() {
                    checked.first_time
                } else {
                    DateTime::default()
                };
                append_metadata(
                    checked.archive.into_inner(),
                    &checked.layout,
                    &mut output,
                    metadata_name.unwrap(),
                    &bytes,
                    time,
                    flag,
                )?;
            } else {
                let mut source = checked.archive.into_inner();
                source.seek(SeekFrom::Start(0))?;
                copy_exact(&mut source, &mut output, checked.layout.length, flag)?;
            }
            output.sync_all()?;
        }
        FrameInput::Files(files) => {
            let frames = renamed_files(files, flag)?;
            let mut names = BTreeMap::new();
            for frame in &frames {
                add_name(&mut names, &safe_name(&frame.file, false)?, false)?;
            }
            if let Some(name) = metadata_name {
                metadata_collision(name, &frames, &names)?;
            }
            for source in files {
                if output.exists()
                    && fs::canonicalize(&source.path).ok() == fs::canonicalize(output).ok()
                {
                    return Err(error("ugoira output must differ from source"));
                }
            }
            if metadata_name.is_some() {
                let bytes = frame_json(&frames, flag)?;
                let total = files.iter().try_fold(bytes.len() as u64, |total, source| {
                    regular(&source.path).map(|metadata| total.saturating_add(metadata.len()))
                })?;
                if files.len() >= MAX_ENTRIES || total > MAX_TOTAL {
                    return Err(error("ugoira archive metadata exceeds total limits"));
                }
            }
            let mut writer = ZipWriter::new(private_output(output)?);
            for (source, frame) in files.iter().zip(&frames) {
                cancelled(flag)?;
                let metadata = regular(&source.path)?;
                let time = match mtime {
                    Some(timestamp) => date(timestamp)?,
                    None => {
                        date(time::OffsetDateTime::from(metadata.modified()?).unix_timestamp())?
                    }
                };
                let options = options(time);
                #[cfg(unix)]
                let options = {
                    use std::os::unix::fs::PermissionsExt;
                    options.unix_permissions(metadata.permissions().mode() & 0o7777)
                };
                writer
                    .start_file(&frame.file, options)
                    .map_err(|_| invalid())?;
                let mut file =
                    File::open(&source.path).map_err(|_| error("cannot open ugoira frame"))?;
                if copy_bounded(&mut file, &mut writer, metadata.len(), flag)? != metadata.len() {
                    return Err(error("ugoira frame changed while archiving"));
                }
            }
            if let Some(name) = metadata_name {
                writer
                    .start_file(
                        name,
                        options(mtime.map(date).transpose()?.unwrap_or_default()),
                    )
                    .map_err(|_| invalid())?;
                copy_bounded(
                    &mut Cursor::new(frame_json(&frames, flag)?),
                    &mut writer,
                    MAX_METADATA as u64,
                    flag,
                )?;
            }
            writer.finish().map_err(|_| invalid())?.sync_all()?;
        }
    }
    cancelled(flag)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn fixture_zip(path: &Path, entries: &[(&str, &[u8])], compression: CompressionMethod) {
        let mut writer = ZipWriter::new(File::create(path).unwrap());
        for (name, data) in entries {
            writer
                .start_file(
                    *name,
                    options(DateTime::from_date_and_time(2024, 1, 2, 3, 4, 6).unwrap())
                        .compression_method(compression),
                )
                .unwrap();
            writer.write_all(data).unwrap();
        }
        writer.finish().unwrap();
    }
    fn input(path: &Path, names: &[&str]) -> FrameInput {
        FrameInput::Zip {
            path: path.to_owned(),
            frames: names
                .iter()
                .enumerate()
                .map(|(i, name)| UgoiraFrame {
                    file: (*name).into(),
                    delay: i as u64 + 1,
                })
                .collect::<Vec<_>>()
                .into(),
        }
    }
    fn from_hex(value: &str) -> Vec<u8> {
        (0..value.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&value[at..at + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn source_archive_oracle_matches_entry_order_bytes_compression_and_times() {
        let fixture: serde_json::Value = serde_json::from_slice(include_bytes!(
            "../tests/fixtures/pixiv/ugoira_postprocessor.json"
        ))
        .unwrap();
        for case in fixture["archive_cases"].as_array().unwrap() {
            let temp = tempfile::tempdir().unwrap();
            let source = temp.path().join("source.zip");
            let output = temp.path().join("output.zip");
            let frames: Vec<UgoiraFrame> = serde_json::from_value(case["frames"].clone()).unwrap();
            let frame_input = if case["zip_source"] == true {
                fixture_zip(
                    &source,
                    &[
                        ("000000.jpg", b"fixture 000000.jpg"),
                        ("000001.jpg", b"fixture 000001.jpg"),
                    ],
                    CompressionMethod::Stored,
                );
                FrameInput::Zip {
                    path: source.clone(),
                    frames: frames.clone().into(),
                }
            } else {
                let files = frames
                    .iter()
                    .map(|frame| {
                        let path = temp.path().join(&frame.file);
                        fs::write(&path, format!("fixture {}", frame.file)).unwrap();
                        File::options()
                            .write(true)
                            .open(&path)
                            .unwrap()
                            .set_times(
                                fs::FileTimes::new()
                                    .set_modified(UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
                            )
                            .unwrap();
                        SourceFrame {
                            frame: frame.clone(),
                            path,
                            extension: "jpg".into(),
                        }
                    })
                    .collect();
                FrameInput::Files(files)
            };
            let metadata = match case["options"].get("metadata") {
                Some(serde_json::Value::Bool(false)) => None,
                Some(serde_json::Value::String(name)) => Some(name.as_str()),
                _ => Some("animation.json"),
            };
            let mtime = (case["options"].get("mtime") != Some(&serde_json::Value::Bool(false)))
                .then_some(1_704_164_646);
            archive(
                &frame_input,
                &output,
                metadata,
                mtime,
                &AtomicBool::new(false),
            )
            .unwrap();
            let mut zip = ZipArchive::new(File::open(&output).unwrap()).unwrap();
            let expected = case["entries"].as_array().unwrap();
            assert_eq!(zip.len(), expected.len(), "{}", case["name"]);
            for (index, expected) in expected.iter().enumerate() {
                let mut entry = zip.by_index(index).unwrap();
                assert_eq!(
                    entry.name().unwrap(),
                    expected["name"].as_str().unwrap(),
                    "{}",
                    case["name"]
                );
                assert_eq!(entry.compression(), CompressionMethod::Stored);
                let time = entry.last_modified().unwrap();
                assert_eq!(
                    vec![
                        time.year() as u64,
                        time.month() as u64,
                        time.day() as u64,
                        time.hour() as u64,
                        time.minute() as u64,
                        time.second() as u64
                    ],
                    expected["date_time"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|v| v.as_u64().unwrap())
                        .collect::<Vec<_>>()
                );
                let mut bytes = Vec::new();
                entry.read_to_end(&mut bytes).unwrap();
                assert_eq!(
                    bytes,
                    from_hex(expected["data_hex"].as_str().unwrap()),
                    "{}",
                    case["name"]
                );
            }
        }
    }

    #[test]
    fn extraction_and_archive_preserve_deflate_extra_entries_and_raw_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.zip");
        fixture_zip(
            &source,
            &[
                ("frame.png", b"compressed image data"),
                ("extra.json", b"preserved extra"),
            ],
            CompressionMethod::Deflated,
        );
        let original = fs::read(&source).unwrap();
        let input = input(&source, &["frame.png"]);
        let directory = tempfile::tempdir().unwrap();
        let frames = extract(&input, directory.path(), &AtomicBool::new(false)).unwrap();
        assert_eq!(frames[0].file, "frame.png");
        assert_eq!(
            fs::read(directory.path().join("extra.json")).unwrap(),
            b"preserved extra"
        );
        let output = temp.path().join("output.zip");
        archive(
            &input,
            &output,
            Some("animation.json"),
            Some(0),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(fs::read(&output).unwrap().starts_with(&original));
        let mut result = ZipArchive::new(File::open(&output).unwrap()).unwrap();
        assert_eq!(
            result.by_index_data(0).unwrap().compression(),
            CompressionMethod::Deflated
        );
        let mut metadata = String::new();
        result
            .by_name("animation.json")
            .unwrap()
            .read_to_string(&mut metadata)
            .unwrap();
        assert_eq!(metadata, "[{\"file\":\"frame.png\",\"delay\":1}]");
        let copied = temp.path().join("copied.zip");
        archive(&input, &copied, None, None, &AtomicBool::new(false)).unwrap();
        assert_eq!(fs::read(copied).unwrap(), original);
    }

    #[test]
    fn archive_appends_deliberate_duplicate_metadata_but_never_duplicates_frames() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.zip");
        fixture_zip(
            &source,
            &[("frame.jpg", b"frame"), ("animation.json", b"old metadata")],
            CompressionMethod::Stored,
        );
        let input = input(&source, &["frame.jpg"]);
        let output = temp.path().join("output.zip");
        archive(
            &input,
            &output,
            Some("animation.json"),
            None,
            &AtomicBool::new(false),
        )
        .unwrap();
        let layout = layout(&mut File::open(&output).unwrap(), &AtomicBool::new(false)).unwrap();
        assert_eq!(layout.entries, 3);
        let mut result = ZipArchive::new(File::open(&output).unwrap()).unwrap();
        let mut metadata = String::new();
        result
            .by_name("animation.json")
            .unwrap()
            .read_to_string(&mut metadata)
            .unwrap();
        assert!(metadata.contains("frame.jpg"));
        assert!(
            archive(
                &input,
                &output,
                Some("frame.jpg"),
                None,
                &AtomicBool::new(false)
            )
            .is_err()
        );
        assert!(
            archive(
                &input,
                &output,
                Some("../outside"),
                None,
                &AtomicBool::new(false)
            )
            .is_err()
        );
        assert!(
            archive(
                &input,
                &output,
                Some("ANIMATION.JSON"),
                None,
                &AtomicBool::new(false)
            )
            .is_err()
        );
    }

    #[test]
    fn file_frames_use_first_dot_stem_and_actual_extension_without_losing_order_or_delay() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("download1");
        let second = temp.path().join("download2");
        fs::write(&first, b"png1").unwrap();
        fs::write(&second, b"png2").unwrap();
        let files = FrameInput::Files(vec![
            SourceFrame {
                frame: UgoiraFrame {
                    file: "000.first.jpeg".into(),
                    delay: 0,
                },
                path: first,
                extension: "png".into(),
            },
            SourceFrame {
                frame: UgoiraFrame {
                    file: "001.jpeg".into(),
                    delay: 999,
                },
                path: second,
                extension: "png".into(),
            },
        ]);
        let output = tempfile::tempdir().unwrap();
        let extracted = extract(&files, output.path(), &AtomicBool::new(false)).unwrap();
        assert_eq!(
            extracted
                .iter()
                .map(|f| (f.file.as_str(), f.delay))
                .collect::<Vec<_>>(),
            [("000.png", 0), ("001.png", 999)]
        );
        assert_eq!(fs::read(output.path().join("001.png")).unwrap(), b"png2");
        let archive_path = temp.path().join("frames.zip");
        archive(
            &files,
            &archive_path,
            Some("animation.json"),
            Some(1_704_164_646),
            &AtomicBool::new(false),
        )
        .unwrap();
        let mut zip = ZipArchive::new(File::open(archive_path).unwrap()).unwrap();
        let mut json = String::new();
        zip.by_name("animation.json")
            .unwrap()
            .read_to_string(&mut json)
            .unwrap();
        assert_eq!(
            json,
            "[{\"file\":\"000.png\",\"delay\":0},{\"file\":\"001.png\",\"delay\":999}]"
        );
        let unicode = vec![UgoiraFrame {
            file: "猫😀.jpg".into(),
            delay: 5,
        }];
        assert_eq!(
            frame_json(&unicode, &AtomicBool::new(false)).unwrap(),
            br#"[{"file":"\u732b\ud83d\ude00.jpg","delay":5}]"#
        );
    }

    #[test]
    fn extraction_rejects_traversal_duplicates_case_collisions_and_file_directory_conflicts() {
        for entries in [
            vec![("../escape", b"bad".as_slice())],
            vec![("/absolute", b"bad")],
            vec![("dir\\evil", b"bad")],
            vec![("a.png", b"a"), ("A.png", b"b")],
            vec![("a", b"a"), ("a/b.png", b"b")],
        ] {
            let temp = tempfile::tempdir().unwrap();
            let source = temp.path().join("source.zip");
            fixture_zip(&source, &entries, CompressionMethod::Stored);
            let destination = tempfile::tempdir().unwrap();
            let value = input(&source, &[entries[0].0]);
            assert!(extract(&value, destination.path(), &AtomicBool::new(false)).is_err());
            assert!(fs::read_dir(destination.path()).unwrap().next().is_none());
        }
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.zip");
        fixture_zip(
            &source,
            &[("a.png", b"a"), ("b.png", b"b")],
            CompressionMethod::Stored,
        );
        let mut bytes = fs::read(&source).unwrap();
        for at in 0..bytes.len() - 5 {
            if &bytes[at..at + 5] == b"b.png" {
                bytes[at] = b'a';
            }
        }
        fs::write(&source, bytes).unwrap();
        assert!(
            extract(
                &input(&source, &["a.png"]),
                tempfile::tempdir().unwrap().path(),
                &AtomicBool::new(false)
            )
            .is_err()
        );
    }

    #[test]
    fn crc_errors_encryption_nonregular_entries_and_size_limits_fail_without_publication() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.zip");
        fixture_zip(
            &source,
            &[("frame.png", b"stored-payload")],
            CompressionMethod::Stored,
        );
        let original = fs::read(&source).unwrap();
        for mode in 0..4 {
            let mut bytes = original.clone();
            let central = bytes
                .windows(4)
                .position(|v| v == [0x50, 0x4b, 0x01, 0x02])
                .unwrap();
            match mode {
                0 => {
                    let at = bytes
                        .windows(14)
                        .position(|v| v == b"stored-payload")
                        .unwrap();
                    bytes[at] ^= 1;
                }
                1 => {
                    bytes[central + 8] |= 1;
                    bytes[6] |= 1;
                }
                2 => {
                    bytes[central + 5] = 3;
                    bytes[central + 38..central + 42]
                        .copy_from_slice(&(0o120777u32 << 16).to_le_bytes());
                }
                _ => {
                    bytes[central + 24..central + 28]
                        .copy_from_slice(&((MAX_FILE + 1) as u32).to_le_bytes());
                }
            }
            fs::write(&source, bytes).unwrap();
            let output = temp.path().join(format!("failed{mode}.zip"));
            assert!(
                archive(
                    &input(&source, &["frame.png"]),
                    &output,
                    None,
                    None,
                    &AtomicBool::new(false)
                )
                .is_err()
            );
            assert!(!output.exists());
        }
        let large = temp.path().join("large");
        File::create(&large).unwrap().set_len(MAX_FILE + 1).unwrap();
        let files = FrameInput::Files(vec![SourceFrame {
            frame: UgoiraFrame {
                file: "a.png".into(),
                delay: 1,
            },
            path: large,
            extension: "png".into(),
        }]);
        assert!(
            extract(
                &files,
                tempfile::tempdir().unwrap().path(),
                &AtomicBool::new(false)
            )
            .is_err()
        );
    }

    #[test]
    fn zip64_entries_and_directory_records_are_read_and_rearchived() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("zip64.zip");
        let mut writer = ZipWriter::new(File::create(&source).unwrap());
        writer
            .start_file("frame.png", options(DateTime::default()).large_file(true))
            .unwrap();
        writer.write_all(b"frame").unwrap();
        writer.finish().unwrap();
        let mut bytes = fs::read(&source).unwrap();
        let end = bytes.len() - 22;
        let central = le32(&bytes, end + 16) as u64;
        let size = le32(&bytes, end + 12) as u64;
        let mut records = Vec::new();
        records.extend_from_slice(&0x06064b50u32.to_le_bytes());
        records.extend_from_slice(&44u64.to_le_bytes());
        records.extend_from_slice(&45u16.to_le_bytes());
        records.extend_from_slice(&45u16.to_le_bytes());
        records.extend_from_slice(&[0; 8]);
        for value in [1u64, 1, size, central] {
            records.extend_from_slice(&value.to_le_bytes());
        }
        records.extend_from_slice(&0x07064b50u32.to_le_bytes());
        records.extend_from_slice(&[0; 4]);
        records.extend_from_slice(&(end as u64).to_le_bytes());
        records.extend_from_slice(&1u32.to_le_bytes());
        bytes[end + 8..end + 12].fill(0xff);
        bytes[end + 12..end + 20].fill(0xff);
        bytes.splice(end..end, records);
        fs::write(&source, bytes).unwrap();
        let value = input(&source, &["frame.png"]);
        let destination = tempfile::tempdir().unwrap();
        extract(&value, destination.path(), &AtomicBool::new(false)).unwrap();
        assert_eq!(
            fs::read(destination.path().join("frame.png")).unwrap(),
            b"frame"
        );
        let output = temp.path().join("new.zip");
        archive(
            &value,
            &output,
            Some("animation.json"),
            Some(0),
            &AtomicBool::new(false),
        )
        .unwrap();
        let mut zip = ZipArchive::new(File::open(output).unwrap()).unwrap();
        assert_eq!(zip.len(), 2);
        let mut frame = String::new();
        zip.by_name("frame.png")
            .unwrap()
            .read_to_string(&mut frame)
            .unwrap();
        assert_eq!(frame, "frame");
    }

    #[test]
    fn cancellation_is_checked_before_output_and_between_copy_chunks() {
        let flag = AtomicBool::new(true);
        let output = tempfile::NamedTempFile::new().unwrap();
        fs::write(output.path(), b"untouched").unwrap();
        assert!(
            archive(
                &FrameInput::Files(Vec::new()),
                output.path(),
                None,
                None,
                &flag
            )
            .is_err()
        );
        assert_eq!(fs::read(output.path()).unwrap(), b"untouched");
        struct Cancelling<'a> {
            flag: &'a AtomicBool,
            bytes: Cursor<Vec<u8>>,
        }
        impl Read for Cancelling<'_> {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                let count = self.bytes.read(buffer)?;
                self.flag.store(true, Ordering::Release);
                Ok(count)
            }
        }
        let flag = AtomicBool::new(false);
        let mut reader = Cancelling {
            flag: &flag,
            bytes: Cursor::new(vec![1; BUFFER * 2]),
        };
        let mut output = Vec::new();
        assert!(copy_bounded(&mut reader, &mut output, MAX_FILE, &flag).is_err());
        assert_eq!(output.len(), BUFFER);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_sources_and_zip_links_are_rejected_without_following_them() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let original = temp.path().join("original");
        fs::write(&original, b"private").unwrap();
        let link = temp.path().join("link");
        symlink(&original, &link).unwrap();
        let files = FrameInput::Files(vec![SourceFrame {
            frame: UgoiraFrame {
                file: "a.png".into(),
                delay: 1,
            },
            path: link,
            extension: "png".into(),
        }]);
        assert!(
            extract(
                &files,
                tempfile::tempdir().unwrap().path(),
                &AtomicBool::new(false)
            )
            .is_err()
        );
        let source = temp.path().join("links.zip");
        let mut writer = ZipWriter::new(File::create(&source).unwrap());
        writer
            .add_symlink("a.png", "../original", options(DateTime::default()))
            .unwrap();
        writer.finish().unwrap();
        assert!(
            extract(
                &input(&source, &["a.png"]),
                tempfile::tempdir().unwrap().path(),
                &AtomicBool::new(false)
            )
            .is_err()
        );
    }
}
