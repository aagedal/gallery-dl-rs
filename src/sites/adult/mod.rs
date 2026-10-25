//! Adult image and comic galleries.
//!
//! Page requests send a browser User-Agent, as upstream's default one is a
//! desktop Firefox and several of these sites refuse unknown clients.
use crate::Result;
use crate::sites::{Http, Site};
use std::time::Duration;
use url::Url;

mod ahottie;
mod allporncomic;
mod eightmuses;
mod erome;
mod everia;
mod fuskator;
mod girlsreleased;
mod hentaicosplays;
mod luscious;
mod myhentaigallery;
mod nsfwalbum;
mod pictoa;
mod poringa;
mod pornpics;
mod sxypix;
mod thehentaiworld;
mod vanillarock;

pub(super) static SITES: &[&Site] = &[
    &eightmuses::SITE,
    &allporncomic::SITE,
    &luscious::SITE,
    &myhentaigallery::SITE,
    &hentaicosplays::HENTAICOSPLAY,
    &hentaicosplays::HENTAIIMG,
    &hentaicosplays::PORNIMAGE,
    &thehentaiworld::SITE,
    &vanillarock::SITE,
    &poringa::SITE,
    &erome::SITE,
    &everia::SITE,
    &fuskator::SITE,
    &nsfwalbum::SITE,
    &pornpics::SITE,
    &pictoa::SITE,
    &sxypix::SITE,
    &girlsreleased::SITE,
    &ahottie::SITE,
];

/// Listing pages one URL may follow.
const MAX_PAGES: usize = 10_000;

/// One page's text, requested with a browser User-Agent.
async fn page(http: &Http, url: &str, headers: &[(&str, &str)]) -> Result<String> {
    http.text_with(&http.api(url)?, &http.browser_headers(headers))
        .await
}

/// `EromeExtractor.request` (and Poringa's): a page that is retried up to
/// five times, five seconds apart, while the site serves its "Please wait a
/// few moments" check. Response cookies stay in the session like upstream's
/// cookie cache.
async fn checked_page(http: &Http, url: &Url) -> Result<(Url, String)> {
    for attempt in 0..5 {
        let (location, page) = http.page(url, &http.browser_headers(&[])).await?;
        let head = page.get(..600).unwrap_or(&page);
        if !head.contains("<title>Please wait a few moments</title>") {
            return Ok((location, page));
        }
        if attempt < 4 && !cfg!(test) {
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }
    Err(crate::error(format!(
        "{} kept serving its waiting page",
        http.name()
    )))
}

/// Upstream `request_interval`: a minimum gap between one site's requests
/// in this process (not applied in tests).
pub(super) struct Interval {
    gap: Duration,
    last: std::sync::Mutex<Option<std::time::Instant>>,
}

impl Interval {
    pub const fn new(millis: u64) -> Self {
        Self {
            gap: Duration::from_millis(millis),
            last: std::sync::Mutex::new(None),
        }
    }

    pub async fn wait(&self) {
        if cfg!(test) {
            return;
        }
        let wait = {
            let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
            let now = std::time::Instant::now();
            let next = last.map_or(now, |last| (last + self.gap).max(now));
            *last = Some(next);
            next - now
        };
        tokio::time::sleep(wait).await;
    }
}

/// Python `str.capitalize`.
fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first
            .to_uppercase()
            .chain(chars.flat_map(char::to_lowercase))
            .collect(),
        None => String::new(),
    }
}

/// Python dict assignment on ordered query pairs: `value` replaces an
/// existing `name` in place, else it is appended.
fn set_param(params: &mut Vec<(String, String)>, name: &str, value: String) {
    match params.iter_mut().find(|(key, _)| key == name) {
        Some(entry) => entry.1 = value,
        None => params.push((name.to_owned(), value)),
    }
}

/// `url` with `params` as its query string (requests' `params=`).
fn with_query(http: &Http, url: &str, params: &[(String, String)]) -> Result<Url> {
    let mut url = http.api(url)?;
    if !params.is_empty() {
        url.query_pairs_mut().extend_pairs(params);
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(capitalize("gRAVURE idol"), "Gravure idol");
        assert_eq!(capitalize(""), "");
        let mut params = vec![("q".to_owned(), "a".to_owned())];
        set_param(&mut params, "page", "1".into());
        set_param(&mut params, "q", "b".into());
        assert_eq!(
            params,
            [("q".into(), "b".into()), ("page".into(), "1".into())]
        );
    }
}

/// Families with configurable instances (`crate::sites::instances`).
pub(super) static FAMILIES: &[&crate::sites::instances::Family] = &[];
