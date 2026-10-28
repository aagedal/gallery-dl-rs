//! Lazy, bounded URL input files. File contents never enter error diagnostics.

use std::{collections::VecDeque, path::PathBuf};

use futures_util::{StreamExt, stream::BoxStream};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};

use crate::{Result, error};

const MAX_LINE_BYTES: usize = 64 * 1024;
const READ_BUFFER_BYTES: usize = 8 * 1024;

/// What to do with an input file's lines once their URL succeeded
/// (upstream `-I/--input-file-comment` and `-x/--input-file-delete`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Comment,
    Delete,
}

/// The input-file lines (zero-based) belonging to one URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Origin {
    pub path: PathBuf,
    pub lines: Vec<usize>,
    pub action: Action,
}

/// One input URL with the inline `-KEY=VALUE` options that apply to it:
/// accumulated `-G` globals first, then local options for this URL only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputItem {
    pub url: String,
    pub options: Vec<String>,
    pub origin: Option<Origin>,
}

pub struct Inputs {
    urls: VecDeque<String>,
    files: std::iter::Enumerate<std::vec::IntoIter<(PathBuf, Option<Action>)>>,
    current: Option<InputReader>,
    global: Vec<String>,
}

impl Inputs {
    pub fn new(urls: Vec<String>, files: Vec<PathBuf>) -> Result<Self> {
        Self::with_actions(urls, files.into_iter().map(|path| (path, None)).collect())
    }

    pub fn with_actions(urls: Vec<String>, files: Vec<(PathBuf, Option<Action>)>) -> Result<Self> {
        if files
            .iter()
            .filter(|(path, _)| path.as_os_str() == "-")
            .count()
            > 1
        {
            return Err(error("standard input can only be used once"));
        }
        if files
            .iter()
            .any(|(path, action)| action.is_some() && path.as_os_str() == "-")
        {
            return Err(error("standard input cannot be rewritten"));
        }
        Ok(Self {
            urls: urls.into(),
            files: files.into_iter().enumerate(),
            current: None,
            global: Vec::new(),
        })
    }

    /// Positional URLs precede files; files are opened only when reached.
    pub fn stream(self) -> BoxStream<'static, Result<String>> {
        self.items().map(|item| item.map(|item| item.url)).boxed()
    }

    /// Like [`Self::stream`], with inline options and line origins.
    pub fn items(self) -> BoxStream<'static, Result<InputItem>> {
        futures_util::stream::unfold(self, |mut inputs| async move {
            loop {
                if let Some(url) = inputs.urls.pop_front() {
                    let item = InputItem {
                        url,
                        options: inputs.global.clone(),
                        origin: None,
                    };
                    return Some((Ok(item), inputs));
                }
                if let Some(reader) = inputs.current.as_mut() {
                    let ordinal = reader.ordinal;
                    let line_number = reader.line_number + 1;
                    let event = reader.next_line().await;
                    reader.line_number = line_number;
                    let (result, close) = match event {
                        Line::Bytes(bytes) => (
                            parse_entry(bytes, line_number == 1).map_err(|message| {
                                error(format!(
                                    "input file {ordinal}, line {line_number}: {message}"
                                ))
                            }),
                            false,
                        ),
                        Line::Oversized => (
                            Err(error(format!(
                                "input file {ordinal}, line {line_number}: line exceeds 64 KiB"
                            ))),
                            false,
                        ),
                        Line::ReadFailure => (
                            Err(error(format!(
                                "input file {ordinal}, line {line_number}: unable to read input"
                            ))),
                            true,
                        ),
                        Line::End => (Ok(None), true),
                    };
                    let index = line_number - 1;
                    let output = match result {
                        Ok(Some(Entry::Url(url))) => {
                            let mut options = inputs.global.clone();
                            options.append(&mut reader.local);
                            reader.lines.push(index);
                            let lines = std::mem::take(&mut reader.lines);
                            let origin = reader.action.map(|action| Origin {
                                path: reader.path.clone(),
                                lines,
                                action,
                            });
                            Some(Ok(InputItem {
                                url,
                                options,
                                origin,
                            }))
                        }
                        Ok(Some(Entry::Global(option))) => {
                            inputs.global.push(option);
                            None
                        }
                        Ok(Some(Entry::Local(option))) => {
                            reader.local.push(option);
                            reader.lines.push(index);
                            None
                        }
                        Ok(None) => None,
                        Err(err) => Some(Err(err)),
                    };
                    if close {
                        inputs.current = None;
                    }
                    match output {
                        Some(output) => return Some((output, inputs)),
                        None => continue,
                    }
                }
                let (index, (path, action)) = inputs.files.next()?;
                let ordinal = index + 1;
                let source: Box<dyn AsyncRead + Unpin + Send> = if path.as_os_str() == "-" {
                    Box::new(tokio::io::stdin())
                } else {
                    match tokio::fs::File::open(&path).await {
                        Ok(file) => Box::new(file),
                        Err(_) => {
                            return Some((
                                Err(error(format!("input file {ordinal}: unable to open input"))),
                                inputs,
                            ));
                        }
                    }
                };
                let mut reader = InputReader::new(source, ordinal);
                reader.path = path;
                reader.action = action;
                inputs.current = Some(reader);
            }
        })
        .boxed()
    }
}

struct InputReader {
    source: BufReader<Box<dyn AsyncRead + Unpin + Send>>,
    ordinal: usize,
    line_number: usize,
    path: PathBuf,
    action: Option<Action>,
    /// Local options and their lines, pending the next URL.
    local: Vec<String>,
    lines: Vec<usize>,
}

enum Entry {
    Url(String),
    Global(String),
    Local(String),
}

enum Line {
    Bytes(Vec<u8>),
    Oversized,
    ReadFailure,
    End,
}

impl InputReader {
    fn new(source: Box<dyn AsyncRead + Unpin + Send>, ordinal: usize) -> Self {
        Self {
            source: BufReader::with_capacity(READ_BUFFER_BYTES, source),
            ordinal,
            line_number: 0,
            path: PathBuf::new(),
            action: None,
            local: Vec::new(),
            lines: Vec::new(),
        }
    }

    async fn next_line(&mut self) -> Line {
        let mut bytes = Vec::new();
        let mut oversized = false;
        loop {
            let available = match self.source.fill_buf().await {
                Ok(available) => available,
                Err(_) => return Line::ReadFailure,
            };
            if available.is_empty() {
                return if oversized {
                    Line::Oversized
                } else if bytes.is_empty() {
                    Line::End
                } else {
                    Line::Bytes(bytes)
                };
            }
            let newline = available.iter().position(|byte| *byte == b'\n');
            let length = newline.unwrap_or(available.len());
            if !oversized {
                if bytes.len() + length > MAX_LINE_BYTES {
                    oversized = true;
                    bytes.clear();
                } else {
                    bytes.extend_from_slice(&available[..length]);
                }
            }
            self.source.consume(length + usize::from(newline.is_some()));
            // Regular files and in-memory readers can stay ready indefinitely.
            // Yield even while discarding a very long line or reading comments.
            tokio::task::yield_now().await;
            if newline.is_some() {
                return if oversized {
                    Line::Oversized
                } else {
                    Line::Bytes(bytes)
                };
            }
        }
    }
}

#[cfg(test)]
fn parse_line(bytes: Vec<u8>, first: bool) -> std::result::Result<Option<String>, &'static str> {
    Ok(match parse_entry(bytes, first)? {
        Some(Entry::Url(url)) => Some(url),
        _ => None,
    })
}

/// Upstream input lines: `#` comments, `-KEY=VALUE` options for the next
/// URL, `-G KEY=VALUE` options for all following URLs, or a URL.
fn parse_entry(bytes: Vec<u8>, first: bool) -> std::result::Result<Option<Entry>, &'static str> {
    let line = std::str::from_utf8(&bytes).map_err(|_| "input is not valid UTF-8")?;
    let line = if first {
        line.strip_prefix('\u{feff}').unwrap_or(line)
    } else {
        line
    };
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return Ok(None);
    }
    if let Some(spec) = line.strip_prefix('-') {
        let (global, spec) = match spec.strip_prefix('G') {
            Some(spec) => (true, spec),
            None => (false, spec),
        };
        let (key, value) = spec.split_once('=').ok_or("invalid KEY=VALUE pair")?;
        let (key, value) = (key.trim(), value.trim());
        if key.is_empty() || serde_json::from_str::<serde_json::Value>(value).is_err() {
            return Err("invalid KEY=VALUE pair");
        }
        let option = format!("{key}={value}");
        return Ok(Some(if global {
            Entry::Global(option)
        } else {
            Entry::Local(option)
        }));
    }
    let mut previous_whitespace = false;
    let end = line.char_indices().find_map(|(index, ch)| {
        if ch == '#' && previous_whitespace {
            Some(index)
        } else {
            previous_whitespace = ch.is_whitespace();
            None
        }
    });
    Ok(Some(Entry::Url(
        line[..end.unwrap_or(line.len())].trim_end().to_owned(),
    )))
}

/// Comment out or delete the lines of `origins` (upstream `-I`/`-x`),
/// replacing each file atomically once. Line numbers refer to the files as
/// they were read.
pub fn apply<'a>(origins: impl IntoIterator<Item = &'a Origin>) -> Result<()> {
    let mut files: std::collections::BTreeMap<&PathBuf, Vec<&Origin>> = Default::default();
    for origin in origins {
        files.entry(&origin.path).or_default().push(origin);
    }
    for (path, origins) in files {
        let text = std::fs::read_to_string(path)?;
        let mut lines: Vec<String> = text.split_inclusive('\n').map(str::to_owned).collect();
        for origin in origins {
            for index in &origin.lines {
                if let Some(line) = lines.get_mut(*index) {
                    match origin.action {
                        Action::Comment => line.insert_str(0, "# "),
                        Action::Delete => line.clear(),
                    }
                }
            }
        }
        let directory = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(std::path::Path::new("."));
        let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
        std::io::Write::write_all(&mut temporary, lines.concat().as_bytes())?;
        temporary
            .persist(path)
            .map_err(|e| crate::Error::from(e.error))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::TryStreamExt;

    #[test]
    fn trims_bom_comments_and_crlf_without_losing_fragments() {
        for (line, first, expected) in [
            (
                "\u{feff} https://example.test/a#fragment \t# comment\r",
                true,
                Some("https://example.test/a#fragment"),
            ),
            ("\t# comment", false, None),
            (" \r", false, None),
            (
                "https://example.test/a#fragment",
                false,
                Some("https://example.test/a#fragment"),
            ),
            (
                "https://example.test/a\u{2003}# comment",
                false,
                Some("https://example.test/a"),
            ),
            (
                "\u{feff}https://example.test/a",
                false,
                Some("\u{feff}https://example.test/a"),
            ),
        ] {
            assert_eq!(
                parse_line(line.as_bytes().to_vec(), first)
                    .unwrap()
                    .as_deref(),
                expected
            );
        }
        assert!(parse_line(b"--cookies private".to_vec(), false).is_err());
        assert!(parse_line(vec![255], false).is_err());
    }

    #[tokio::test]
    async fn inline_options_and_origins_are_attached_and_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("urls.txt");
        std::fs::write(
            &file,
            "-G base-directory = \"/tmp/\"\n# note\n-filename=\"a.jpg\"\nhttps://a.test/1 # c\n\nhttps://a.test/2\n",
        )
        .unwrap();
        let items: Vec<_> =
            Inputs::with_actions(Vec::new(), vec![(file.clone(), Some(Action::Delete))])
                .unwrap()
                .items()
                .try_collect()
                .await
                .unwrap();
        assert_eq!(items[0].url, "https://a.test/1");
        assert_eq!(
            items[0].options,
            ["base-directory=\"/tmp/\"", "filename=\"a.jpg\""]
        );
        assert_eq!(items[0].origin.as_ref().unwrap().lines, [2, 3]);
        assert_eq!(items[1].options, ["base-directory=\"/tmp/\""]);
        apply(items[0].origin.iter().chain(items[1].origin.iter())).unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "-G base-directory = \"/tmp/\"\n# note\n\n"
        );
        std::fs::write(&file, "https://a.test/1\nhttps://a.test/2\n").unwrap();
        let mut origin = items[0].origin.clone().unwrap();
        origin.action = Action::Comment;
        origin.lines = vec![1];
        apply([&origin]).unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "https://a.test/1\n# https://a.test/2\n"
        );
        assert!(parse_entry(b"-key".to_vec(), false).is_err());
        assert!(parse_entry(b"-key=not json".to_vec(), false).is_err());
    }

    #[test]
    fn duplicate_stdin_is_rejected_without_opening_anything() {
        assert!(Inputs::new(Vec::new(), vec!["-".into(), "-".into()]).is_err());
    }

    #[tokio::test]
    async fn files_are_lazy_and_follow_positionals_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        std::fs::write(&first, "\u{feff}# header\nfile-one\r\n\n").unwrap();
        let mut stream = Inputs::new(vec!["positional".into()], vec![first, second.clone()])
            .unwrap()
            .stream();
        assert_eq!(stream.next().await.unwrap().unwrap(), "positional");
        assert_eq!(stream.next().await.unwrap().unwrap(), "file-one");
        std::fs::write(second, "file-two").unwrap();
        assert_eq!(stream.try_collect::<Vec<_>>().await.unwrap(), ["file-two"]);
    }

    #[tokio::test]
    async fn line_errors_continue_and_do_not_reveal_contents_or_paths() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("secret-path");
        let mut data = b"--cookies secret-value\n".to_vec();
        data.extend_from_slice(&[255, b'\n']);
        data.extend(std::iter::repeat_n(b'x', MAX_LINE_BYTES + 1));
        data.extend_from_slice(b"\nvalid\n");
        std::fs::write(&file, data).unwrap();
        let results = Inputs::new(Vec::new(), vec![dir.path().join("missing-secret"), file])
            .unwrap()
            .stream()
            .collect::<Vec<_>>()
            .await;
        assert_eq!(results.len(), 5);
        for (index, result) in results[..4].iter().enumerate() {
            let message = result.as_ref().unwrap_err().to_string();
            assert!(!message.contains("secret"));
            if index > 0 {
                assert!(message.contains(&format!("input file 2, line {index}")));
            }
        }
        assert_eq!(results[4].as_ref().unwrap(), "valid");
    }

    #[tokio::test]
    async fn exact_limit_and_unterminated_oversized_lines() {
        let data = vec![b'x'; MAX_LINE_BYTES];
        let mut reader = InputReader::new(Box::new(std::io::Cursor::new(data)), 1);
        assert!(
            matches!(reader.next_line().await, Line::Bytes(bytes) if bytes.len() == MAX_LINE_BYTES)
        );
        assert!(matches!(reader.next_line().await, Line::End));
        let data = vec![b'x'; MAX_LINE_BYTES * 4];
        let mut reader = InputReader::new(Box::new(std::io::Cursor::new(data)), 1);
        assert!(matches!(reader.next_line().await, Line::Oversized));
        assert!(matches!(reader.next_line().await, Line::End));
    }

    #[tokio::test]
    async fn empty_files_are_valid() {
        let file = tempfile::NamedTempFile::new().unwrap();
        assert!(
            Inputs::new(Vec::new(), vec![file.path().to_owned()])
                .unwrap()
                .stream()
                .try_collect::<Vec<_>>()
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn read_failure_stops_that_file_but_preserves_later_files() {
        struct FailingReader(std::io::Cursor<Vec<u8>>);
        impl AsyncRead for FailingReader {
            fn poll_read(
                mut self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
                buffer: &mut tokio::io::ReadBuf<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                let position = self.0.position() as usize;
                if position == self.0.get_ref().len() {
                    return std::task::Poll::Ready(Err(std::io::Error::other("secret failure")));
                }
                let length = buffer.remaining().min(self.0.get_ref().len() - position);
                buffer.put_slice(&self.0.get_ref()[position..position + length]);
                self.0.set_position((position + length) as u64);
                std::task::Poll::Ready(Ok(()))
            }
        }
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), "later\n").unwrap();
        let mut inputs = Inputs::new(Vec::new(), vec![file.path().to_owned()]).unwrap();
        inputs.current = Some(InputReader::new(
            Box::new(FailingReader(std::io::Cursor::new(
                b"valid\nsecret partial".to_vec(),
            ))),
            9,
        ));
        let results = inputs.stream().collect::<Vec<_>>().await;
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].as_ref().unwrap(), "valid");
        assert_eq!(
            results[1].as_ref().unwrap_err().to_string(),
            "input file 9, line 2: unable to read input"
        );
        assert_eq!(results[2].as_ref().unwrap(), "later");
    }

    #[tokio::test]
    async fn discarding_large_lines_yields_to_other_tasks() {
        let ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = ran.clone();
        let task = tokio::spawn(async move {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        let mut reader = InputReader::new(
            Box::new(std::io::Cursor::new(vec![b'x'; MAX_LINE_BYTES * 8])),
            1,
        );
        assert!(matches!(reader.next_line().await, Line::Oversized));
        assert!(ran.load(std::sync::atomic::Ordering::SeqCst));
        task.await.unwrap();
    }
}
