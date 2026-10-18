//! Incremental decoding of gallery-dl's outer JSON message array.
use crate::{Result, error};
use serde_json::Value;

pub(crate) const MAX_ROW_BYTES: usize = 16 * 1024 * 1024;
const MAX_DEPTH: usize = 128;

#[derive(Default)]
enum State {
    #[default]
    Start,
    FirstRow,
    NextRow,
    Row,
    Separator,
    Done,
}

#[derive(Default)]
pub(crate) struct Decoder {
    state: State,
    row: Vec<u8>,
    delimiters: Vec<u8>,
    in_string: bool,
    escaped: bool,
}

impl Decoder {
    /// Consume through at most one complete row, leaving subsequent bytes to
    /// the caller. Rows are parsed only once, after their closing delimiter.
    pub(crate) fn feed(&mut self, bytes: &[u8]) -> Result<(usize, Option<Value>)> {
        for (index, &byte) in bytes.iter().enumerate() {
            match self.state {
                State::Start => {
                    if whitespace(byte) {
                        continue;
                    }
                    if byte != b'[' {
                        return Err(error("compatibility messages must be a JSON array"));
                    }
                    self.state = State::FirstRow;
                }
                State::FirstRow | State::NextRow => {
                    if whitespace(byte) {
                        continue;
                    }
                    if byte == b']' && matches!(self.state, State::FirstRow) {
                        self.state = State::Done;
                    } else if byte == b'[' {
                        self.row.push(byte);
                        self.delimiters.push(byte);
                        self.state = State::Row;
                    } else {
                        return Err(error("compatibility message must be a JSON array row"));
                    }
                }
                State::Row => {
                    if self.row.len() == MAX_ROW_BYTES {
                        return Err(error("compatibility message exceeds the size limit"));
                    }
                    self.row.push(byte);
                    if self.in_string {
                        if self.escaped {
                            self.escaped = false;
                        } else if byte == b'\\' {
                            self.escaped = true;
                        } else if byte == b'"' {
                            self.in_string = false;
                        }
                        continue;
                    }
                    match byte {
                        b'"' => self.in_string = true,
                        b'[' | b'{' => {
                            if self.delimiters.len() == MAX_DEPTH {
                                return Err(error(
                                    "compatibility message exceeds the nesting limit",
                                ));
                            }
                            self.delimiters.push(byte);
                        }
                        b']' | b'}' => {
                            let expected = if byte == b']' { b'[' } else { b'{' };
                            if self.delimiters.pop() != Some(expected) {
                                return Err(error(
                                    "compatibility message has mismatched delimiters",
                                ));
                            }
                            if self.delimiters.is_empty() {
                                let value = serde_json::from_slice(&self.row).map_err(|_| {
                                    error("compatibility message contains invalid JSON")
                                })?;
                                self.row.clear();
                                self.state = State::Separator;
                                return Ok((index + 1, Some(value)));
                            }
                        }
                        _ => {}
                    }
                }
                State::Separator => match byte {
                    b',' => self.state = State::NextRow,
                    b']' => self.state = State::Done,
                    byte if whitespace(byte) => {}
                    _ => return Err(error("compatibility messages have an invalid separator")),
                },
                State::Done => {
                    if !whitespace(byte) {
                        return Err(error("compatibility messages contain trailing data"));
                    }
                }
            }
        }
        Ok((bytes.len(), None))
    }

    pub(crate) fn finish(&self) -> Result<()> {
        if matches!(self.state, State::Done) {
            Ok(())
        } else {
            Err(error("compatibility messages are incomplete"))
        }
    }
}

fn whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n')
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn feed_all(decoder: &mut Decoder, mut bytes: &[u8]) -> Result<Vec<Value>> {
        let mut rows = Vec::new();
        while !bytes.is_empty() {
            let (consumed, row) = decoder.feed(bytes)?;
            assert!(consumed > 0);
            bytes = &bytes[consumed..];
            rows.extend(row);
        }
        Ok(rows)
    }

    #[test]
    fn every_split_preserves_rows_and_utf8() {
        let rows = vec![
            json!([2, {"caption": "雪 🦀 [brackets] {braces} \"quote\" \\ slash"}]),
            json!([3, "https://example.test/a", {"nested": [true, null, {}]}]),
        ];
        let encoded = serde_json::to_vec(&rows).unwrap();
        for split in 0..=encoded.len() {
            let mut decoder = Decoder::default();
            let mut actual = feed_all(&mut decoder, &encoded[..split]).unwrap();
            actual.extend(feed_all(&mut decoder, &encoded[split..]).unwrap());
            decoder.finish().unwrap();
            assert_eq!(actual, rows, "split {split}");
        }
        let mut decoder = Decoder::default();
        let mut actual = Vec::new();
        for byte in &encoded {
            actual.extend(feed_all(&mut decoder, &[*byte]).unwrap());
        }
        decoder.finish().unwrap();
        assert_eq!(actual, rows);
    }

    #[test]
    fn emits_one_row_per_call_without_consuming_next_separator() {
        let mut decoder = Decoder::default();
        let bytes = b" [[2,{}], [3,\"url\",{}]] \n";
        let (consumed, row) = decoder.feed(bytes).unwrap();
        assert_eq!(consumed, 8);
        assert_eq!(row, Some(json!([2, {}])));
        assert!(decoder.finish().is_err());
        let (next, row) = decoder.feed(&bytes[consumed..]).unwrap();
        assert_eq!(row, Some(json!([3, "url", {}])));
        assert!(decoder.finish().is_err());
        let (tail, row) = decoder.feed(&bytes[consumed + next..]).unwrap();
        assert_eq!(tail, bytes.len() - consumed - next);
        assert!(row.is_none());
        decoder.finish().unwrap();
    }

    #[test]
    fn empty_input_and_empty_array() {
        let mut decoder = Decoder::default();
        assert_eq!(decoder.feed(b"").unwrap(), (0, None));
        assert!(decoder.finish().is_err());
        assert!(feed_all(&mut decoder, b" \r\n[ \t] \n").unwrap().is_empty());
        decoder.finish().unwrap();
        assert_eq!(decoder.feed(b"").unwrap(), (0, None));
        assert_eq!(decoder.feed(b" \t").unwrap(), (2, None));
    }

    #[test]
    fn unicode_escape_sequences_and_escaped_string_endings() {
        let bytes = br#"[["\u96ea \ud83e\udd80 [\"{\"] \\"] ,["next"]]"#;
        let expected: Vec<Value> = serde_json::from_slice(bytes).unwrap();
        for split in 0..=bytes.len() {
            let mut decoder = Decoder::default();
            let mut actual = feed_all(&mut decoder, &bytes[..split]).unwrap();
            actual.extend(feed_all(&mut decoder, &bytes[split..]).unwrap());
            decoder.finish().unwrap();
            assert_eq!(actual, expected, "split {split}");
        }
    }

    #[test]
    fn rejects_malformed_framing_and_rows() {
        for bytes in [
            &b"{}"[..],
            b"[{}]",
            b"[1]",
            b"[[1],]",
            b"[,[]]",
            b"[[] []]",
            b"[[]:[]]",
            b"[] []",
            b"[]x",
            b"[[]]]",
            b"[[{]]]",
            b"[[}]]",
            b"[[1,]]",
            b"[[\"bad\\x\"]]",
            b"[[\"\xff\"]]",
            b"[\x0b]",
        ] {
            let mut decoder = Decoder::default();
            assert!(feed_all(&mut decoder, bytes).is_err(), "{bytes:?}");
        }
    }

    #[test]
    fn rejects_truncated_documents() {
        for bytes in [
            &b""[..],
            b" ",
            b"[",
            b"[[",
            b"[[1",
            b"[[1]",
            b"[[1],",
            b"[[\"text",
            b"[[\"escape\\",
            b"[[{}]",
        ] {
            let mut decoder = Decoder::default();
            feed_all(&mut decoder, bytes).unwrap();
            assert!(decoder.finish().is_err(), "{bytes:?}");
        }
    }

    #[test]
    fn limits_nesting_before_serde_parsing() {
        // serde_json's own recursion limit can also reject a deeply nested row;
        // the scanner must bound unfinished input without waiting for serde.
        let mut decoder = Decoder::default();
        let prefix = vec![b'['; MAX_DEPTH + 1]; // outer array plus row depth 128
        decoder.feed(&prefix).unwrap();
        assert!(decoder.feed(b"[").is_err());
        let mut decoder = Decoder::default();
        decoder.feed(b"[[").unwrap();
        decoder.feed(&[b'{'; MAX_DEPTH - 1]).unwrap();
        assert!(decoder.feed(b"{").is_err());
    }

    #[test]
    fn row_size_limit_includes_delimiters() {
        let mut decoder = Decoder::default();
        decoder.feed(b"[[\"").unwrap();
        decoder.feed(&vec![b'x'; MAX_ROW_BYTES - 4]).unwrap();
        let (_, row) = decoder.feed(b"\"]").unwrap();
        assert_eq!(row.unwrap()[0].as_str().unwrap().len(), MAX_ROW_BYTES - 4);
        decoder.feed(b"]").unwrap();
        decoder.finish().unwrap();

        let mut decoder = Decoder::default();
        decoder.feed(b"[[\"").unwrap();
        decoder.feed(&vec![b'x'; MAX_ROW_BYTES - 2]).unwrap();
        assert!(decoder.feed(b"\"").is_err());
    }

    #[test]
    fn errors_do_not_disclose_payloads() {
        let mut decoder = Decoder::default();
        let failure = feed_all(&mut decoder, b"[[\"private-token\",invalid]]").unwrap_err();
        assert!(!failure.to_string().contains("private-token"));
    }
}
