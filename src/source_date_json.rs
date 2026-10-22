//! Narrow JSON repair for Python strings containing unpaired surrogate escapes.
//! Only date-bound values are eligible; all remaining validation stays with serde.

#[derive(Clone, Copy)]
enum Scope {
    Root,
    Extractor,
    Category(bool),
    Options,
    Bound,
    Other,
}
impl Scope {
    fn child(self, key: &str) -> Self {
        match self {
            Self::Bound => Self::Bound,
            Self::Root | Self::Extractor | Self::Category(_) | Self::Options
                if matches!(key, "date-before" | "date-after") =>
            {
                Self::Bound
            }
            Self::Root if key == "extractor" => Self::Extractor,
            Self::Extractor if category(key) => Self::Category(matches!(key, "danbooru" | "e621")),
            Self::Category(true) if matches!(key, "artist" | "artist-search" | "tag") => {
                Self::Options
            }
            _ => Self::Other,
        }
    }
}
fn category(key: &str) -> bool {
    matches!(
        key,
        "instagram"
            | "twitter"
            | "facebook"
            | "imgur"
            | "bluesky"
            | "reddit"
            | "danbooru"
            | "e621"
            | "gelbooru"
            | "safebooru"
            | "mastodon"
            | "mastodon.social"
            | "pawoo"
            | "baraag"
            | "kemono"
            | "coomer"
            | "pixeldrain"
            | "catbox"
            | "imgbox"
            | "postimg"
            | "tumblr"
            | "pixiv"
            | "pixiv-novel"
            | "deviantart"
            | "flickr"
            | "wallhaven"
            | "artstation"
            | "pinterest"
            | "newgrounds"
            | "furaffinity"
            | "weasyl"
            | "vsco"
            | "direct"
    ) || crate::sites::is_category(key)
}

/// Return repaired bytes only when a date-bound unpaired escape was changed.
/// Replacements occupy exactly six bytes, so no source indices move.
pub(crate) fn normalize(bytes: &[u8]) -> Option<Vec<u8>> {
    if !bytes
        .windows(3)
        .any(|value| matches!(value, b"\\uD" | b"\\ud"))
    {
        return None;
    }
    let mut lexer = Lexer {
        bytes,
        position: 0,
        replacements: Vec::new(),
    };
    lexer.value(Scope::Root, 0)?;
    lexer.whitespace();
    if lexer.position != bytes.len() || lexer.replacements.is_empty() {
        return None;
    }
    let mut repaired = bytes.to_vec();
    for position in lexer.replacements {
        repaired[position..position + 6].copy_from_slice(b"\\uFFFD");
    }
    // Reject malformed strings, numbers, UTF-8 and JSON outside the narrow lexer.
    // In particular an unrelated unpaired escape must still reject the document.
    serde_json::from_slice::<serde_json::Value>(&repaired).ok()?;
    Some(repaired)
}
struct Lexer<'a> {
    bytes: &'a [u8],
    position: usize,
    replacements: Vec<usize>,
}
impl Lexer<'_> {
    fn whitespace(&mut self) {
        while self
            .bytes
            .get(self.position)
            .is_some_and(|c| matches!(c, b' ' | b'\t' | b'\r' | b'\n'))
        {
            self.position += 1;
        }
    }
    fn expect(&mut self, byte: u8) -> Option<()> {
        self.whitespace();
        if self.bytes.get(self.position) != Some(&byte) {
            return None;
        }
        self.position += 1;
        Some(())
    }
    fn unit(&self, position: usize) -> Option<u16> {
        if self.bytes.get(position..position + 2)? != b"\\u" {
            return None;
        }
        let mut value = 0;
        for byte in self.bytes.get(position + 2..position + 6)? {
            value = value * 16
                + match byte {
                    b'0'..=b'9' => (byte - b'0') as u16,
                    b'a'..=b'f' => (byte - b'a' + 10) as u16,
                    b'A'..=b'F' => (byte - b'A' + 10) as u16,
                    _ => return None,
                };
        }
        Some(value)
    }
    fn string(&mut self, bound: bool) -> Option<std::ops::Range<usize>> {
        self.whitespace();
        let start = self.position;
        self.expect(b'"')?;
        loop {
            match *self.bytes.get(self.position)? {
                b'"' => {
                    self.position += 1;
                    return Some(start..self.position);
                }
                b'\\' => {
                    let escape = *self.bytes.get(self.position + 1)?;
                    if escape == b'u' {
                        let unit = self.unit(self.position)?;
                        if (0xD800..=0xDBFF).contains(&unit)
                            && self
                                .unit(self.position + 6)
                                .is_some_and(|next| (0xDC00..=0xDFFF).contains(&next))
                        {
                            self.position += 12;
                            continue;
                        }
                        if (0xD800..=0xDFFF).contains(&unit) && bound {
                            self.replacements.push(self.position);
                        }
                        self.position += 6;
                    } else if matches!(
                        escape,
                        b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't'
                    ) {
                        self.position += 2;
                    } else {
                        return None;
                    }
                }
                0..=31 => return None,
                _ => self.position += 1,
            }
        }
    }
    fn value(&mut self, scope: Scope, depth: usize) -> Option<()> {
        if depth > 128 {
            return None;
        }
        self.whitespace();
        match *self.bytes.get(self.position)? {
            b'{' => {
                self.position += 1;
                self.whitespace();
                if self.bytes.get(self.position) == Some(&b'}') {
                    self.position += 1;
                    return Some(());
                }
                loop {
                    let key_range = self.string(matches!(scope, Scope::Bound))?;
                    let child = if matches!(scope, Scope::Bound) {
                        Scope::Bound
                    } else {
                        let key: String = serde_json::from_slice(&self.bytes[key_range]).ok()?;
                        scope.child(&key)
                    };
                    self.expect(b':')?;
                    self.value(child, depth + 1)?;
                    self.whitespace();
                    match self.bytes.get(self.position)? {
                        b'}' => {
                            self.position += 1;
                            break;
                        }
                        b',' => self.position += 1,
                        _ => return None,
                    }
                }
            }
            b'[' => {
                self.position += 1;
                self.whitespace();
                if self.bytes.get(self.position) == Some(&b']') {
                    self.position += 1;
                    return Some(());
                }
                let child = if matches!(scope, Scope::Bound) {
                    Scope::Bound
                } else {
                    Scope::Other
                };
                loop {
                    self.value(child, depth + 1)?;
                    self.whitespace();
                    match self.bytes.get(self.position)? {
                        b']' => {
                            self.position += 1;
                            break;
                        }
                        b',' => self.position += 1,
                        _ => return None,
                    }
                }
            }
            b'"' => {
                self.string(matches!(scope, Scope::Bound))?;
            }
            _ => {
                let start = self.position;
                while self.bytes.get(self.position).is_some_and(|c| {
                    !matches!(c, b',' | b'}' | b']' | b' ' | b'\n' | b'\r' | b'\t')
                }) {
                    self.position += 1;
                }
                if start == self.position {
                    return None;
                }
            }
        }
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::normalize;
    #[test]
    fn repairs_only_supported_option_scopes_and_decodes_escaped_keys() {
        for document in [
            r#"{"date-before":"2024-01-01\uD80012:00"}"#,
            r#"{"extractor":{"date-after":"\uDC00"}}"#,
            r#"{"extractor":{"pixiv":{"date-before":"\udfff"}}}"#,
            r#"{"extractor":{"e621":{"tag":{"date-after":"\ud800"}}}}"#,
            r#"{"extractor":{"danbooru":{"artist-search":{"date-after":"\ud800"}}}}"#,
            r#"{"date\u002dbefore":{"\uD800":["\uDC00"]}}"#,
        ] {
            let fixed = normalize(document.as_bytes()).expect(document);
            assert_eq!(fixed.len(), document.len());
            assert!(
                String::from_utf8(fixed.clone())
                    .unwrap()
                    .contains("\\uFFFD")
            );
            assert!(serde_json::from_slice::<serde_json::Value>(&fixed).is_ok());
        }
        for document in [
            r#"{"postprocessors":[{"date-before":"\ud800"}]}"#,
            r#"{"extractor":{"unknown":{"date-before":"\ud800"}}}"#,
            r#"{"extractor":{"pixiv":{"tag":{"date-before":"\ud800"}}}}"#,
            r#"{"other":{"date-before":"\ud800"}}"#,
            r#"[{"date-before":"\ud800"}]"#,
        ] {
            assert!(normalize(document.as_bytes()).is_none(), "{document}");
        }
    }
    #[test]
    fn preserves_pairs_literal_backslashes_duplicates_and_other_bytes() {
        assert!(normalize(br#"{"date-before":"\ud800\udc00"}"#).is_none());
        assert!(normalize(br#"{"date-before":"\\ud800"}"#).is_none());
        let input =
            br#"{"date-before":"\uD800\udc00\ud800","date-before":false,"other":"\\ud800"}"#;
        let expected =
            br#"{"date-before":"\uD800\udc00\uFFFD","date-before":false,"other":"\\ud800"}"#;
        assert_eq!(normalize(input).unwrap(), expected);
        assert!(normalize(br#"{"date-before":"\ud800","other":"\ud800"}"#).is_none());
    }
    #[test]
    fn malformed_and_deep_inputs_remain_for_original_schema_validation() {
        for input in [
            r#"{"date-before":"\ud800",}"#,
            r#"{"date-before":"\ud800"} trailing"#,
            r#"{"date-before":"\ud800\x00"}"#,
            r#"{"date-before": ["\ud800",]}"#,
            r#"{"date-before": ["\ud800",01]}"#,
        ] {
            assert!(normalize(input.as_bytes()).is_none(), "{input}");
        }
        let deep = format!(
            r#"{{"date-before":{}"\ud800"{}}}"#,
            "[".repeat(130),
            "]".repeat(130)
        );
        assert!(normalize(deep.as_bytes()).is_none());
    }
}
