//! Media, social and search sites with public APIs.
//!
//! Besides the site modules this file holds small ports of gallery-dl's
//! `text`/`dt` helpers and the `GalleryExtractor` numbering used by several
//! of them.
use crate::sites::Site;
use serde_json::{Map, Value};
use url::Url;

mod audiochan;
mod bbc;
mod desktopography;
mod directlink;
mod generic;
mod imagebam;
mod imagechest;
mod imagefap;
mod imageshack;
mod imgbb;
mod khinsider;
mod lexica;
mod listal;
mod misskey;
mod pexels;
mod photovogue;
mod pixnet;
mod recursive;
mod redgifs;
mod shopify;
mod slickpic;
mod soundgasm;
mod telegraph;
mod tenor;
mod tumblrgallery;
mod tungsten;
mod unsplash;
mod wallpapercave;
mod whyp;
mod wikimedia;

pub(super) use directlink::{SITE as DIRECTLINK, matches as is_direct_link};

pub(super) static SITES: &[&Site] = &[
    &directlink::SITE,
    &generic::SITE,
    &recursive::SITE,
    &redgifs::SITE,
    &soundgasm::SITE,
    &misskey::MISSKEY_IO,
    &misskey::MISSKEY_DESIGN,
    &misskey::MISSKEY_ART,
    &misskey::LESBIAN_ENERGY,
    &misskey::SUSHI_SKI,
    &wikimedia::WIKIPEDIA,
    &wikimedia::WIKTIONARY,
    &wikimedia::WIKIQUOTE,
    &wikimedia::WIKIBOOKS,
    &wikimedia::WIKISOURCE,
    &wikimedia::WIKINEWS,
    &wikimedia::WIKIVERSITY,
    &wikimedia::WIKIDATA,
    &wikimedia::WIKIVOYAGE,
    &wikimedia::WIKISPECIES,
    &wikimedia::WIKIMEDIACOMMONS,
    &wikimedia::MEDIAWIKI,
    &wikimedia::MARIOWIKI,
    &wikimedia::BULBAPEDIA,
    &wikimedia::PIDGIWIKI,
    &wikimedia::AZURLANEWIKI,
    &wikimedia::MGEWIKI,
    &unsplash::SITE,
    &pexels::SITE,
    &tenor::SITE,
    &imgbb::SITE,
    &imagechest::SITE,
    &telegraph::SITE,
    &imagebam::SITE,
    &imageshack::SITE,
    &imagefap::SITE,
    &wallpapercave::SITE,
    &desktopography::SITE,
    &lexica::SITE,
    &pixnet::SITE,
    &listal::SITE,
    &khinsider::SITE,
    &audiochan::SITE,
    &whyp::SITE,
    &photovogue::SITE,
    &bbc::SITE,
    &tungsten::SITE,
    &slickpic::SITE,
    &tumblrgallery::SITE,
    &shopify::CHELSEACREW,
    &shopify::FASHIONNOVA,
    &shopify::LOUNGEUNDERWEAR,
    &shopify::MICHAELSCAMERAS,
    &shopify::MODCLOTH,
    &shopify::OHPOLLY,
    &shopify::OMGMIAMISWIMWEAR,
    &shopify::PINUPGIRLCLOTHING,
    &shopify::RAIDLONDON,
    &shopify::UNIQUE_VINTAGE,
    &shopify::WINDSORSTORE,
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

/// An absolute HTTP(S) URL without port or credentials.
pub(crate) fn web_url(raw: &str) -> Option<Url> {
    Url::parse(raw.trim()).ok().filter(crate::sites::plain)
}

/// The text of a string field, or empty.
pub(crate) fn str_of<'v>(value: &'v Value, key: &str) -> &'v str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// Python `\w` for one character.
pub(crate) fn word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Upstream `(?:[^/?#]*-)?(ID)` over one path segment, where `id` accepts
/// the ID characters: the greedy slug ends at the last `-` that is followed
/// by an ID character. Returns the matched text and the ID.
pub(crate) fn slug_id(segment: &str, id: fn(char) -> bool) -> Option<(&str, &str)> {
    let run = |start: usize| {
        let len = segment[start..]
            .find(|c: char| !id(c))
            .unwrap_or(segment.len() - start);
        (len > 0).then(|| (&segment[..start + len], &segment[start..start + len]))
    };
    segment
        .match_indices('-')
        .rev()
        .find_map(|(i, _)| run(i + 1))
        .or_else(|| run(0))
}

/// A raw path segment that stays one segment when formatted into a URL:
/// non-empty and not a (percent-encoded) dot segment.
pub(crate) fn path_segment(text: &str) -> bool {
    let dots = text.to_ascii_lowercase().replace("%2e", ".");
    !text.is_empty() && dots != "." && dots != ".." && !text.contains(['/', '?', '#', '\\'])
}

/// Set a request parameter like a Python dict item: an existing name keeps
/// its position.
pub(crate) fn set_param(params: &mut Vec<(String, String)>, name: &str, value: String) {
    match params.iter_mut().find(|(k, _)| k == name) {
        Some((_, v)) => *v = value,
        None => params.push((name.into(), value)),
    }
}

/// Families with configurable instances (`crate::sites::instances`).
pub(super) static FAMILIES: &[&crate::sites::instances::Family] =
    &[&misskey::FAMILY, &wikimedia::FAMILY, &shopify::FAMILY];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_ids() {
        let digit = |c: char| c.is_ascii_digit();
        assert_eq!(slug_id("a-b-123", digit), Some(("a-b-123", "123")));
        assert_eq!(slug_id("a-1-b", digit), Some(("a-1", "1")));
        assert_eq!(slug_id("12ab", digit), Some(("12", "12")));
        assert_eq!(slug_id("ab", digit), None);
        assert_eq!(slug_id("x-a1b.c", word_char), Some(("x-a1b", "a1b")));
        assert!(path_segment("abc") && !path_segment("%2E.") && !path_segment(""));
    }
}
