//! Lazy, bounded URL input files. File contents never enter error diagnostics.

use std::{collections::VecDeque, path::PathBuf};

use futures_util::{StreamExt, stream::BoxStream};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};

use crate::{Result, error};

const MAX_LINE_BYTES: usize = 64 * 1024;
const READ_BUFFER_BYTES: usize = 8 * 1024;

pub struct Inputs {
    urls: VecDeque<String>,
    files: std::iter::Enumerate<std::vec::IntoIter<PathBuf>>,
    current: Option<InputReader>,
}

impl Inputs {
    pub fn new(urls: Vec<String>, files: Vec<PathBuf>) -> Result<Self> {
        if files.iter().filter(|path| path.as_os_str() == "-").count() > 1 {
            return Err(error("standard input can only be used once"));
        }
        Ok(Self {
            urls: urls.into(),
            files: files.into_iter().enumerate(),
            current: None,
        })
    }

    /// Positional URLs precede files; files are opened only when reached.
    pub fn stream(self) -> BoxStream<'static, Result<String>> {
        futures_util::stream::unfold(self, |mut inputs| async move {
            loop {
                if let Some(url) = inputs.urls.pop_front() {
                    return Some((Ok(url), inputs));
                }
                if let Some(reader) = inputs.current.as_mut() {
                    let ordinal = reader.ordinal;
                    let line_number = reader.line_number + 1;
                    let event = reader.next_line().await;
                    reader.line_number = line_number;
                    let result = match event {
                        Line::Bytes(bytes) => {
                            parse_line(bytes, line_number == 1).map_err(|message| {
                                error(format!(
                                    "input file {ordinal}, line {line_number}: {message}"
                                ))
                            })
                        }
                        Line::Oversized => Err(error(format!(
                            "input file {ordinal}, line {line_number}: line exceeds 64 KiB"
                        ))),
                        Line::ReadFailure => {
                            inputs.current = None;
                            Err(error(format!(
                                "input file {ordinal}, line {line_number}: unable to read input"
                            )))
                        }
                        Line::End => {
                            inputs.current = None;
                            continue;
                        }
                    };
                    match result {
                        Ok(Some(url)) => return Some((Ok(url), inputs)),
                        Ok(None) => continue,
                        Err(err) => return Some((Err(err), inputs)),
                    }
                }
                let (index, path) = inputs.files.next()?;
                let ordinal = index + 1;
                let source: Box<dyn AsyncRead + Unpin + Send> = if path.as_os_str() == "-" {
                    Box::new(tokio::io::stdin())
                } else {
                    match tokio::fs::File::open(path).await {
                        Ok(file) => Box::new(file),
                        Err(_) => {
                            return Some((
                                Err(error(format!("input file {ordinal}: unable to open input"))),
                                inputs,
                            ));
                        }
                    }
                };
                inputs.current = Some(InputReader::new(source, ordinal));
            }
        })
        .boxed()
    }
}

struct InputReader {
    source: BufReader<Box<dyn AsyncRead + Unpin + Send>>,
    ordinal: usize,
    line_number: usize,
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

fn parse_line(bytes: Vec<u8>, first: bool) -> std::result::Result<Option<String>, &'static str> {
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
    if line.starts_with('-') {
        return Err("inline configuration directives are unsupported");
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
    Ok(Some(
        line[..end.unwrap_or(line.len())].trim_end().to_owned(),
    ))
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
