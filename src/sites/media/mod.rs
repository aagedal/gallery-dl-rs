//! Media, social and search sites with public APIs.
//!
//! Besides the site modules this file holds small ports of gallery-dl's
//! `text`/`dt` helpers and the `GalleryExtractor` numbering used by several
//! of them.
use crate::sites::Site;
use serde_json::{Map, Value};
use url::Url;

mod misskey;
mod redgifs;

pub(super) static SITES: &[&Site] = &[
    &redgifs::SITE,
    &misskey::MISSKEY_IO,
    &misskey::MISSKEY_DESIGN,
    &misskey::MISSKEY_ART,
    &misskey::LESBIAN_ENERGY,
    &misskey::SUSHI_SKI,
];

/// gallery-dl `dt.parse_iso` as Unix seconds: Python 3.12
/// `datetime.fromisoformat`, normalized to UTC with microseconds dropped.
/// Naive values are taken as UTC, as upstream treats them.
pub(crate) fn iso_ts(text: &str) -> Option<i64> {
    let text = text.trim_end();
    let b = text.as_bytes();
    let num = |range: std::ops::Range<usize>| -> Option<i64> {
        let part = text.get(range)?;
        (!part.is_empty() && part.bytes().all(|c| c.is_ascii_digit()))
            .then(|| part.parse().ok())
            .flatten()
    };
    let (year, month, day, mut pos) = if b.len() >= 10 && b[4] == b'-' && b[7] == b'-' {
        (num(0..4)?, num(5..7)?, num(8..10)?, 10)
    } else if b.len() >= 8 && b.get(8).is_none_or(|c| !c.is_ascii_digit()) {
        (num(0..4)?, num(4..6)?, num(6..8)?, 8)
    } else {
        return None;
    };
    let month = time::Month::try_from(u8::try_from(month).ok()?).ok()?;
    let date = time::Date::from_calendar_date(i32::try_from(year).ok()?, month, day as u8).ok()?;
    let (mut hour, mut minute, mut second) = (0, 0, 0);
    let mut offset = 0i64;
    if pos < b.len() {
        pos += 1; // any single separator character
        hour = num(pos..pos + 2)?;
        pos += 2;
        if b.get(pos) == Some(&b':') {
            minute = num(pos + 1..pos + 3)?;
            pos += 3;
            if b.get(pos) == Some(&b':') {
                second = num(pos + 1..pos + 3)?;
                pos += 3;
                if matches!(b.get(pos), Some(b'.' | b',')) {
                    let digits = b[pos + 1..]
                        .iter()
                        .take_while(|c| c.is_ascii_digit())
                        .count();
                    if digits == 0 {
                        return None;
                    }
                    pos += 1 + digits;
                }
            }
        } else if b.len() >= pos + 2 && b[pos].is_ascii_digit() {
            minute = num(pos..pos + 2)?;
            pos += 2;
            if b.len() >= pos + 2 && b[pos].is_ascii_digit() {
                second = num(pos..pos + 2)?;
                pos += 2;
            }
        }
        match b.get(pos) {
            None => {}
            Some(b'Z') if pos + 1 == b.len() => {}
            Some(sign @ (b'+' | b'-')) => {
                let rest = text[pos + 1..].replace(':', "");
                if !(rest.len() == 4 || rest.len() == 6)
                    || !rest.bytes().all(|c| c.is_ascii_digit())
                {
                    return None;
                }
                let h: i64 = rest[0..2].parse().ok()?;
                let m: i64 = rest[2..4].parse().ok()?;
                let s: i64 = rest.get(4..6).map_or(Some(0), |s| s.parse().ok())?;
                offset = (h * 3600 + m * 60 + s) * if *sign == b'-' { -1 } else { 1 };
            }
            _ => return None,
        }
    }
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let days =
        i64::from(date.to_julian_day() - time::macros::date!(1970 - 01 - 01).to_julian_day());
    Some(days * 86400 + hour * 3600 + minute * 60 + second - offset)
}

/// The upstream `date` keyword for a Unix timestamp.
pub(crate) fn date(ts: Option<i64>) -> Value {
    crate::filter::source_date_text(ts).into()
}

/// gallery-dl `dt.parse_ts` for a JSON number or numeric string.
pub(crate) fn ts_value(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().filter(|f| f.is_finite()).map(|f| f as i64)),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// gallery-dl `text.extract_from`: sequential extraction from a cursor;
/// a miss returns an empty string and keeps the position.
pub(crate) struct Extr<'t> {
    text: &'t str,
    pos: usize,
}

impl<'t> Extr<'t> {
    pub fn new(text: &'t str) -> Self {
        Self { text, pos: 0 }
    }
    pub fn get(&mut self, begin: &str, end: &str) -> &'t str {
        let rest = &self.text[self.pos..];
        let Some(start) = rest.find(begin).map(|i| i + begin.len()) else {
            return "";
        };
        let Some(stop) = rest[start..].find(end).map(|i| i + start) else {
            return "";
        };
        self.pos += stop + end.len();
        &rest[start..stop]
    }
}

/// gallery-dl `text.extract_iter`.
pub(crate) fn extract_iter<'t>(text: &'t str, begin: &str, end: &str) -> Vec<&'t str> {
    let mut out = Vec::new();
    let mut extr = Extr::new(text);
    loop {
        let before = extr.pos;
        let rest = &text[before..];
        let Some(start) = rest.find(begin).map(|i| i + begin.len()) else {
            break;
        };
        if rest[start..].find(end).is_none() {
            break;
        }
        out.push(extr.get(begin, end));
    }
    out
}

/// gallery-dl `text.rextr`: the last `begin` before `pos`, up to `end`.
pub(crate) fn rextr<'t>(text: &'t str, begin: &str, end: &str, pos: Option<usize>) -> &'t str {
    let head = &text[..pos.unwrap_or(text.len()).min(text.len())];
    // Python's rindex(begin, None, pos) needs the whole match before pos.
    let Some(first) = head.rfind(begin).map(|i| i + begin.len()) else {
        return "";
    };
    text[first..]
        .find(end)
        .map_or("", |stop| &text[first..first + stop])
}

/// gallery-dl `text.nameext_from_name`.
pub(crate) fn nameext_name(name: &str) -> (String, String) {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && ext.chars().count() <= 16 => {
            (stem.to_owned(), ext.to_lowercase())
        }
        _ => (name.to_owned(), String::new()),
    }
}

/// gallery-dl `text.nameext_from_url` for a URL string.
pub(crate) fn nameext_str(url: &str) -> (String, String) {
    let path = url.split('?').next().unwrap_or_default();
    let last = path.rsplit('/').next().unwrap_or_default();
    nameext_name(&crate::sites::percent_decode(last))
}

/// Store `filename` and `extension`.
pub(crate) fn set_nameext(map: &mut Map<String, Value>, (name, ext): (String, String)) {
    map.insert("filename".into(), name.into());
    map.insert("extension".into(), ext.into());
}

/// gallery-dl `text.parse_int`.
pub(crate) fn parse_int(text: &str) -> i64 {
    text.trim().parse().unwrap_or(0)
}

/// gallery-dl `text.parse_query`: first value wins; `+` means space.
pub(crate) fn parse_query(qs: Option<&str>) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for pair in qs.unwrap_or_default().split('&') {
        let Some((name, value)) = pair.split_once('=') else {
            continue;
        };
        let name = crate::sites::percent_decode(&name.replace('+', " "));
        if !out.iter().any(|(k, _)| *k == name) {
            out.push((name, crate::sites::percent_decode(&value.replace('+', " "))));
        }
    }
    out
}

pub(crate) fn query_get<'q>(query: &'q [(String, String)], key: &str) -> Option<&'q str> {
    query
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// gallery-dl `text.split_html`: unescaped, stripped text between tags.
pub(crate) fn split_html(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut rest = text;
    loop {
        let mut cut = None;
        let mut from = 0;
        while let Some(start) = rest[from..].find('<').map(|i| i + from) {
            match rest[start + 1..].find('>') {
                Some(end) if end > 0 => {
                    cut = Some((start, start + end + 2));
                    break;
                }
                Some(_) => from = start + 1,
                None => break,
            }
        }
        let (piece, next) = match cut {
            Some((start, stop)) => (&rest[..start], Some(&rest[stop..])),
            None => (rest, None),
        };
        if !piece.trim().is_empty() {
            parts.push(crate::sites::unescape(piece).trim().to_owned());
        }
        match next {
            Some(next) => rest = next,
            None => return parts,
        }
    }
}

/// An absolute HTTP(S) URL without port or credentials.
pub(crate) fn web_url(raw: &str) -> Option<Url> {
    Url::parse(raw.trim()).ok().filter(crate::sites::plain)
}

/// `GalleryExtractor.items()` numbering: `count` and `num` (from one) with
/// image data merged cumulatively into the shared metadata, as upstream does.
pub(crate) fn gallery(
    mut data: Map<String, Value>,
    images: Vec<(Url, Option<Map<String, Value>>)>,
) -> Vec<(Url, Map<String, Value>)> {
    let images: Vec<_> = match data.get("count") {
        Some(count) => {
            let count = count.as_u64().unwrap_or(0) as usize;
            images.into_iter().take(count).collect()
        }
        None => {
            data.insert("count".into(), images.len().into());
            images
        }
    };
    let mut out = Vec::with_capacity(images.len());
    for (num, (url, image)) in images.into_iter().enumerate() {
        data.insert("num".into(), (num + 1).into());
        match image.filter(|image| !image.is_empty()) {
            Some(image) => {
                let has_ext = image.contains_key("extension");
                data.extend(image);
                if !has_ext {
                    set_nameext(&mut data, crate::sites::nameext(&url));
                }
            }
            None => set_nameext(&mut data, crate::sites::nameext(&url)),
        }
        out.push((url, data.clone()));
    }
    out
}

/// The text of a string field, or empty.
pub(crate) fn str_of<'v>(value: &'v Value, key: &str) -> &'v str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_dates_match_python() {
        assert_eq!(iso_ts("2017-07-14T02:40:00"), Some(1500000000));
        assert_eq!(iso_ts("2017-07-14T02:40:00.123Z"), Some(1500000000));
        assert_eq!(iso_ts("2017-07-14T04:40:00+02:00"), Some(1500000000));
        assert_eq!(iso_ts("2017-07-14T04:40:00+0200"), Some(1500000000));
        assert_eq!(iso_ts("2017-07-14 02:40"), Some(1500000000));
        assert_eq!(iso_ts("2017-07-14"), Some(1499990400));
        for bad in [
            "",
            "yesterday",
            "2017-13-01",
            "2017-07-14T25:00",
            "2017-07-14T02:40:00Zx",
        ] {
            assert_eq!(iso_ts(bad), None, "{bad}");
        }
        assert_eq!(date(None), "[Invalid DateTime]");
    }

    #[test]
    fn text_helpers_match_gallery_dl() {
        let mut e = Extr::new("<a>1</a><b>2</b>");
        assert_eq!(e.get("<b>", "</b>"), "2");
        assert_eq!(e.get("<a>", "</a>"), "");
        assert_eq!(extract_iter("[1][2][3", "[", "]"), ["1", "2"]);
        assert_eq!(
            rextr("href=\"a\" href=\"b\" X", "href=\"", "\"", Some(17)),
            "b"
        );
        assert_eq!(
            nameext_str("https://x/a/b%20c.JPG?x=1"),
            ("b c".into(), "jpg".into())
        );
        assert_eq!(nameext_name(".hidden"), (".hidden".into(), String::new()));
        assert_eq!(
            parse_query(Some("a=1&a=2&b=x+y&c")),
            vec![("a".into(), "1".into()), ("b".into(), "x y".into())]
        );
        assert_eq!(
            split_html("<p>Hello &amp; </p>\n<br><span> world</span>"),
            ["Hello &", "world"]
        );
    }

    #[test]
    fn gallery_numbering_is_cumulative() {
        let mut meta = Map::new();
        meta.insert("title".into(), "T".into());
        let mut first = Map::new();
        first.insert("caption".into(), "c".into());
        let items = gallery(
            meta,
            vec![
                (Url::parse("https://x.test/a.png").unwrap(), Some(first)),
                (Url::parse("https://x.test/b.jpg").unwrap(), None),
            ],
        );
        assert_eq!(items[1].1["num"], 2);
        assert_eq!(items[1].1["count"], 2);
        assert_eq!(items[1].1["caption"], "c");
        assert_eq!(items[1].1["filename"], "b");
    }
}
