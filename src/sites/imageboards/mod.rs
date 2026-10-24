//! Imageboards and their archives.
use crate::sites::{Events, Http, Route, Site};
use serde_json::{Map, Value};
use url::Url;

mod eightchan;
mod foolfuuka;
mod fourarchive;
mod fourchan;
mod fourchanarchives;
mod jschan;
mod lynxchan;
mod twoch;
mod twochan;
mod twochen;
mod vichan;
mod warosu;

pub(super) static SITES: &[&Site] = &[
    &fourchan::SITE,
    &vichan::EIGHTKUN,
    &vichan::SMUGLOLI,
    &vichan::GUROCHAN,
    &lynxchan::BBWCHAN,
    &lynxchan::KOHLCHAN,
    &lynxchan::ENDCHAN,
    &jschan::NINETYFOURCHAN,
    &warosu::SITE,
    &foolfuuka::FOURPLEBS,
    &foolfuuka::ARCHIVEDMOE,
    &foolfuuka::ARCHIVEOFSINS,
    &foolfuuka::B4K,
    &foolfuuka::DESUARCHIVE,
    &foolfuuka::FIREDEN,
    &foolfuuka::PALANQ,
    &foolfuuka::RBT,
    &foolfuuka::THEBARCHIVE,
    &fourchanarchives::SITE,
    &fourarchive::SITE,
    &twoch::SITE,
    &twochen::STURDYCHAN,
    &twochen::SCHAN,
    &eightchan::SITE,
    &twochan::SITE,
];

/// Key holding the upstream `Message.Directory` fields a route's default
/// directory needs when they differ from the per-file keywords (the
/// registry renders directories from file keywords).
pub(super) const DIRECTORY: &str = "_directory";

/// Whether the URL is plain HTTP(S) on exactly one of `hosts`.
pub(super) fn host_in(url: &Url, hosts: &[&str]) -> bool {
    crate::sites::plain(url) && url.host_str().is_some_and(|host| hosts.contains(&host))
}

/// Path, query and fragment, as matched by upstream patterns after the host.
pub(super) fn rest(url: &Url) -> &str {
    &url[url::Position::BeforePath..]
}

/// Match `/` followed by a non-empty run of characters outside `stop`;
/// returns the run (as serialized, i.e. percent-encoded) and the remainder.
pub(super) fn seg<'t>(text: &'t str, stop: &[char]) -> Option<(String, &'t str)> {
    let text = text.strip_prefix('/')?;
    let end = text.find(|c| stop.contains(&c)).unwrap_or(text.len());
    (end > 0).then(|| (text[..end].to_owned(), &text[end..]))
}

/// `[^/?#]+` path component.
pub(super) fn component(text: &str) -> Option<(String, &str)> {
    seg(text, &['/', '?', '#'])
}

/// A non-empty run of ASCII digits (at most 20) and the remainder.
pub(super) fn digits(text: &str) -> Option<(&str, &str)> {
    let end = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    (1..=20).contains(&end).then(|| text.split_at(end))
}

/// Upstream `(?:/index|/catalog|/\d+|/?$)` board suffixes.
pub(super) fn board_suffix(rest: &str) -> bool {
    rest.is_empty()
        || rest == "/"
        || rest.starts_with("/index")
        || rest.starts_with("/catalog")
        || rest
            .strip_prefix('/')
            .is_some_and(|r| r.starts_with(|c: char| c.is_ascii_digit()))
}

/// gallery-dl `text.extract_from`: sequential extraction that only advances
/// on success and yields `""` otherwise.
pub(super) struct Extr<'t> {
    text: &'t str,
    pos: usize,
}

impl<'t> Extr<'t> {
    pub fn new(text: &'t str) -> Self {
        Self { text, pos: 0 }
    }
    pub fn at(text: &'t str, pos: usize) -> Self {
        Self { text, pos }
    }
    pub fn next(&mut self, begin: &str, end: &str) -> &'t str {
        let Some(first) = self.text[self.pos..]
            .find(begin)
            .map(|i| self.pos + i + begin.len())
        else {
            return "";
        };
        let Some(last) = self.text[first..].find(end).map(|i| first + i) else {
            return "";
        };
        self.pos = last + end.len();
        &self.text[first..last]
    }
}

/// gallery-dl `text.extract_iter`.
pub(super) fn extract_iter<'t>(text: &'t str, begin: &str, end: &str) -> Vec<&'t str> {
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(first) = text[pos..].find(begin).map(|i| pos + i + begin.len()) {
        let Some(last) = text[first..].find(end).map(|i| first + i) else {
            break;
        };
        out.push(&text[first..last]);
        pos = last + end.len();
    }
    out
}

/// gallery-dl `text.nameext_from_name`.
pub(super) fn nameext_from_name(name: &str) -> (String, String) {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && ext.chars().count() <= 16 => {
            (stem.to_owned(), ext.to_lowercase())
        }
        _ => (name.to_owned(), String::new()),
    }
}

/// gallery-dl `text.nameext_from_url` applied to a plain string.
pub(super) fn nameext_from_url(url: &str) -> (String, String) {
    let path = url.split('?').next().unwrap_or_default();
    let name = path.rsplit('/').next().unwrap_or_default();
    nameext_from_name(&crate::sites::percent_decode(name))
}

/// Store `filename`/`extension` from [`nameext_from_url`].
pub(super) fn set_nameext(fields: &mut Map<String, Value>, url: &str) {
    let (name, ext) = nameext_from_url(url);
    fields.insert("filename".into(), name.into());
    fields.insert("extension".into(), ext.into());
}

/// gallery-dl `text.parse_int` for strings.
pub(super) fn parse_int(text: &str) -> i64 {
    text.trim().parse().unwrap_or(0)
}

/// Python `text[:count]`.
pub(super) fn truncate(text: &str, count: usize) -> String {
    text.chars().take(count).collect()
}

/// Python `str.rpartition(sep)`: `(head, tail)`, with `("", text)` when absent.
pub(super) fn rpartition<'t>(text: &'t str, sep: &str) -> (&'t str, &'t str) {
    text.rsplit_once(sep).unwrap_or(("", text))
}

/// A URL that keeps the origin of `root`.
pub(super) fn on_root(root: &str, path: &str) -> Option<Url> {
    if !path.starts_with('/') || path.starts_with("//") {
        return None;
    }
    let base = Url::parse(root).ok()?;
    let url = Url::parse(&format!("{root}{path}")).ok()?;
    (crate::sites::plain(&url) && url.origin() == base.origin()).then_some(url)
}

/// Queue child URLs lazily, in order.
pub(super) fn queue_all<'a>(
    http: &'a Http,
    site: &'static Site,
    route: &'static Route,
    children: Vec<(Url, Map<String, Value>)>,
) -> Events<'a> {
    futures_util::StreamExt::boxed(futures_util::StreamExt::flatten(
        futures_util::stream::iter(children.into_iter().map(move |(child, data)| {
            crate::sites::queue(http, site.category, route.subcategory, child, &data)
        })),
    ))
}

/// Unix timestamp of Python's `datetime(1, 1, 1)`, gallery-dl's failed-parse
/// date.
pub(super) const NO_DATE: i64 = -62_135_596_800;

/// Python `str()` of a naive UTC `datetime` built from a Unix timestamp.
pub(super) fn datetime_string(timestamp: i64) -> String {
    match time::OffsetDateTime::from_unix_timestamp(timestamp) {
        Ok(dt) => format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            dt.year(),
            u8::from(dt.month()),
            dt.day(),
            dt.hour(),
            dt.minute(),
            dt.second()
        ),
        Err(_) => String::new(),
    }
}

/// Python truthiness for a JSON value.
pub(super) fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// Copy the named fields of an upstream Directory kwdict for [`DIRECTORY`].
pub(super) fn directory_fields(source: &Map<String, Value>, keys: &[&str]) -> Value {
    Value::Object(
        keys.iter()
            .filter_map(|key| {
                source
                    .get(*key)
                    .map(|value| ((*key).to_owned(), value.clone()))
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers_match_gallery_dl() {
        let mut extr = Extr::new("a=1; b=2; c=3");
        assert_eq!(extr.next("b=", ";"), "2");
        assert_eq!(extr.next("a=", ";"), "");
        assert_eq!(extr.next("", "="), " c");
        assert_eq!(extract_iter("<1><2><3", "<", ">"), vec!["1", "2"]);
        assert_eq!(
            nameext_from_url("my%20file.JPG?x=1"),
            ("my file".into(), "jpg".into())
        );
        assert_eq!(nameext_from_name(".hidden"), (".hidden".into(), "".into()));
        assert_eq!(rpartition("a.b.c", "."), ("a.b", "c"));
        assert_eq!(rpartition("abc", "."), ("", "abc"));
        assert_eq!(parse_int(" 12 "), 12);
        assert_eq!(parse_int("x"), 0);
        assert_eq!(component("/a%20b/c"), Some(("a%20b".into(), "/c")));
        assert_eq!(component("//c"), None);
        assert_eq!(digits("123.html"), Some(("123", ".html")));
        assert!(board_suffix("/catalog.html") && board_suffix("/2") && board_suffix(""));
        assert!(!board_suffix("/res/1") && !board_suffix("/?x"));
        assert!(on_root("https://a.test", "/x.png").is_some());
        assert!(on_root("https://a.test", "@b.test/x.png").is_none());
        assert!(on_root("https://a.test", "//b.test/x.png").is_none());
    }
}
